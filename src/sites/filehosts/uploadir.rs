//! uploadir files, mirroring gallery_dl/extractor/uploadir.py: a HEAD
//! request to `/u/ID` either names a directly served file through its
//! `Content-Disposition`, or redirects to a download page whose form is
//! POSTed (with its `authenticity_token` and the upload ID) to get the file.
use super::{Body, Extr, request};
use crate::extract::MediaJob;
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::Map;
use url::Url;

pub static SITE: Site = Site {
    category: "uploadir",
    name: "uploadir",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&FILE],
    api_key: false,
    cookies: None,
};

static FILE: Route = Route::new(
    "file",
    "{filename} ({id}).{extension}",
    &["{category}"],
    "{id}",
);

const ROOT: &str = "https://uploadir.com";

/// `uploadir\.com/(?:user/)?u(?:ploads)?/([^/?#]+)`.
fn target(url: &Url) -> Option<String> {
    if !crate::sites::plain(url) || url.host_str() != Some("uploadir.com") {
        return None;
    }
    let path = url.path().strip_prefix('/')?;
    let path = path.strip_prefix("user/").unwrap_or(path);
    let rest = path
        .strip_prefix("uploads/")
        .or_else(|| path.strip_prefix("u/"))?;
    let id = rest.split('/').next()?;
    (!id.is_empty() && id.len() <= 64).then(|| crate::sites::percent_decode(id))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(id) = target(&url) else {
        return crate::sites::fail(error("unsupported uploadir URL"));
    };
    crate::sites::once(async move {
        let item = file(http, &id).await?;
        Ok(crate::sites::events(crate::sites::post(
            &SITE,
            http,
            &FILE,
            vec![item],
        )))
    })
}

/// `UploadirFileExtractor.items`.
async fn file(http: &Http, id: &str) -> Result<Item> {
    let mut page = Url::parse(ROOT).expect("static root");
    page.path_segments_mut()
        .expect("base URL")
        .extend(["u", id]);
    let response = request(
        http,
        reqwest::Method::HEAD,
        &http.api(page.as_str())?,
        &[],
        Body::None,
    )
    .await?;
    let mut data = Map::new();
    let (url, job) = if (300..400).contains(&response.status) {
        let location = response
            .location
            .as_deref()
            .and_then(|location| page.join(location).ok())
            .filter(crate::sites::plain)
            .ok_or_else(|| error("uploadir redirect has no valid Location"))?;
        let text = http.text(&http.api(location.as_str())?).await?;
        let (name, action, token) = parse_form(&text)?;
        super::nameext_from_url(&name, &mut data);
        let url = Url::parse(&format!("{ROOT}{action}"))
            .ok()
            .filter(|url| super::on_host(url, &["uploadir.com"]))
            .ok_or_else(|| error("uploadir download form has an invalid action"))?;
        let form = vec![
            ("authenticity_token".to_owned(), token),
            ("upload_id".to_owned(), id.to_owned()),
        ];
        (url, MediaJob::FormPost { form })
    } else {
        let disposition = response
            .headers
            .get(reqwest::header::CONTENT_DISPOSITION)
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| error("uploadir file has no Content-Disposition"))?;
        super::nameext_from_url(&disposition_name(disposition), &mut data);
        (page, MediaJob::DirectHttp)
    };
    data.insert("id".into(), id.into());
    Ok(Item::new(url, data).ids(id, id).job(job))
}

/// The download page's `<h2>` file name, form action and token.
fn parse_form(page: &str) -> Result<(String, String, String)> {
    let mut extr = Extr::new(page);
    let name = crate::sites::unescape(extr.next("<h2>", "</h2>").trim());
    let action = extr.next("class=\"form\" action=\"", "\"").to_owned();
    let token = extr
        .next("name=\"authenticity_token\" value=\"", "\"")
        .to_owned();
    if !action.starts_with('/') || action.starts_with("//") {
        return Err(error("uploadir download page has no download form"));
    }
    Ok((name, action, token))
}

/// `filename*=UTF-8''NAME`, else `filename="NAME"` (percent-decoded later
/// by `nameext_from_url`, as upstream does).
fn disposition_name(header: &str) -> String {
    match super::partition(header, "filename*=UTF-8''").2 {
        "" => crate::sites::extr(header, "filename=\"", "\"")
            .unwrap_or_default()
            .to_owned(),
        name => name.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        for (raw, id) in [
            ("https://uploadir.com/u/rd3t46ry", "rd3t46ry"),
            ("https://uploadir.com/uploads/rd3t46ry", "rd3t46ry"),
            ("https://uploadir.com/user/uploads/rd3t46ry", "rd3t46ry"),
            ("https://uploadir.com/user/u/rd3t46ry", "rd3t46ry"),
            (
                "https://uploadir.com/uploads/gxe8ti9v/downloads/new",
                "gxe8ti9v",
            ),
            ("http://uploadir.com/u/abc?x=1#y", "abc"),
        ] {
            assert_eq!(t(raw).as_deref(), Some(id), "{raw}");
        }
        for raw in [
            "https://uploadir.com/u/",
            "https://uploadir.com/x/abc",
            "https://www.uploadir.com/u/abc",
            "https://uploadir.com:444/u/abc",
            "https://evil-uploadir.com/u/abc",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[tokio::test]
    async fn served_files_are_named_by_their_content_disposition() {
        let head = |disposition: &str| {
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Disposition: {disposition}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .into_bytes()
        };
        let (root, server) = serve(vec![
            head("inline; filename=\"Chloe and Rachel 4K jpg.jpg\""),
            head(
                "attachment; filename=\"_.png\"; filename*=UTF-8''_%E5%9C%96%E7%89%87_%F0%9F%96%BC_image_.png",
            ),
        ]);
        let http = http("uploadir", root);
        let files = media(extract(
            &http,
            Url::parse("https://uploadir.com/u/rd3t46ry").unwrap(),
        ))
        .await;
        assert_eq!(files[0].url.as_str(), "https://uploadir.com/u/rd3t46ry");
        assert!(matches!(files[0].job, MediaJob::DirectHttp));
        let k = keywords(&files[0]);
        assert_eq!(k["filename"], "Chloe and Rachel 4K jpg");
        assert_eq!(k["extension"], "jpg");
        assert_eq!(k["id"], "rd3t46ry");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            file.render(&files[0]).unwrap(),
            "Chloe and Rachel 4K jpg (rd3t46ry).jpg"
        );
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("uploadir")
        );
        assert_eq!(
            crate::sites::archive_key(&files[0]).unwrap().1,
            r#"["{id}",["rd3t46ry"]]"#
        );
        let files = media(extract(
            &http,
            Url::parse("https://uploadir.com/user/uploads/fllda6xl").unwrap(),
        ))
        .await;
        let k = keywords(&files[0]);
        assert_eq!(k["filename"], "_圖片_🖼_image_");
        assert_eq!(k["extension"], "png");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("HEAD /u/rd3t46ry "));
        assert!(requests[1].starts_with("HEAD /u/fllda6xl "));
    }

    #[tokio::test]
    async fn redirected_uploads_become_form_post_jobs() {
        let page = "<h2> NYAN-Mods-Pack#1.zip </h2>\
            <form class=\"form\" action=\"/uploads/gxe8ti9v/downloads\" method=\"post\">\
            <input type=\"hidden\" name=\"authenticity_token\" value=\"tok+en==\">";
        let redirect = b"HTTP/1.1 302 Found\r\nLocation: /uploads/gxe8ti9v/downloads/new\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec();
        let (root, server) = serve(vec![redirect, html(page)]);
        let http = http("uploadir", root);
        let files = media(extract(
            &http,
            Url::parse("https://uploadir.com/uploads/gxe8ti9v/downloads/new").unwrap(),
        ))
        .await;
        assert_eq!(
            files[0].url.as_str(),
            "https://uploadir.com/uploads/gxe8ti9v/downloads"
        );
        let MediaJob::FormPost { form } = &files[0].job else {
            panic!("expected a form POST job");
        };
        assert_eq!(
            form,
            &[
                ("authenticity_token".to_owned(), "tok+en==".to_owned()),
                ("upload_id".to_owned(), "gxe8ti9v".to_owned()),
            ]
        );
        let k = keywords(&files[0]);
        assert_eq!(k["filename"], "NYAN-Mods-Pack#1");
        assert_eq!(k["extension"], "zip");
        assert_eq!(k["id"], "gxe8ti9v");
        // The token stays out of public JSON and keywords.
        let public = serde_json::to_string(&files[0]).unwrap();
        assert!(!public.contains("tok+en"));
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("HEAD /u/gxe8ti9v "));
        assert!(requests[1].starts_with("GET /uploads/gxe8ti9v/downloads/new "));
    }

    #[test]
    fn download_pages_need_a_local_form_action() {
        assert!(parse_form("<h2>x</h2>").is_err());
        assert!(parse_form("<h2>x</h2><form class=\"form\" action=\"//evil.test/x\">").is_err());
        assert_eq!(
            disposition_name("attachment; filename=\"a b.zip\""),
            "a b.zip"
        );
    }
}
