//! uploadir files, mirroring gallery_dl/extractor/uploadir.py: redirected
//! uploads are downloaded by POSTing their download form.
use crate::error;
use crate::sites::gdl::{Scan, captures, group, nameext_into, partition, re, unescape};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, json};
use url::Url;

const ROOT: &str = "https://uploadir.com";

static FILE: Route = Route::new(
    "file",
    "{filename} ({id}).{extension}",
    &["{category}"],
    "{id}",
);

pub static SITE: Site = Site {
    category: "uploadir",
    name: "uploadir",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&FILE],
    api_key: false,
    cookies: Some("uploadir.com"),
};

fn target(url: &Url) -> Option<String> {
    captures(
        re!(r"^https?://uploadir\.com/(?:user/)?u(?:ploads)?/([^/?#]+)"),
        url,
    )
    .map(|c| group(&c, 1).into())
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(file_id) = target(&url) else {
        return crate::sites::fail(error("unsupported uploadir URL"));
    };
    crate::sites::once(async move {
        let request = http.api(&format!("{ROOT}/u/{file_id}"))?;
        let mut data = Map::new();
        let item = match http.location(&request, &[]).await? {
            Some(location) => {
                let page = http.text(&location).await?;
                let mut scan = Scan::new(&page);
                let name = unescape(scan.extr("<h2>", "</h2>").trim());
                let action = http.api(&format!(
                    "{ROOT}{}",
                    scan.extr("class=\"form\" action=\"", "\"")
                ))?;
                let token = scan.extr("name=\"authenticity_token\" value=\"", "\"");
                nameext_into(&name, &mut data);
                data.insert("_http_method".into(), "POST".into());
                data.insert(
                    "_http_data".into(),
                    json!({"authenticity_token": token, "upload_id": file_id}),
                );
                data.insert("id".into(), file_id.as_str().into());
                let cookie = http.cookie_header(&action);
                let item = Item::new(action, data);
                match cookie {
                    Some(cookie) => item.header("Cookie", &cookie),
                    None => item,
                }
            }
            None => {
                let (_, headers) = http.get_with_headers(&request, &[]).await?;
                let disposition = headers
                    .get("content-disposition")
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| error("uploadir sent no Content-Disposition"))?
                    .to_owned();
                let name = match partition(&disposition, "filename*=UTF-8''").2 {
                    "" => crate::sites::gdl::extr(&disposition, "filename=\"", "\"").to_owned(),
                    name => name.to_owned(),
                };
                nameext_into(&name, &mut data);
                data.insert("id".into(), file_id.as_str().into());
                Item::new(request, data)
            }
        };
        Ok(crate::sites::events(crate::sites::post(
            &SITE,
            http,
            &FILE,
            vec![item.ids(file_id.clone(), file_id)],
        )))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http as client, keywords, media, response, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://uploadir.com/u/rd3t46ry"),
            Some("rd3t46ry".into())
        );
        assert_eq!(
            t("https://uploadir.com/uploads/gxe8ti9v/downloads/new"),
            Some("gxe8ti9v".into())
        );
    }

    #[tokio::test]
    async fn redirected_upload_posts_its_form() {
        let redirect = response(
            "302 Found\r\nLocation: /uploads/abc/downloads/new\r\nSet-Cookie: _session=S; Path=/",
            "text/html",
            b"",
        );
        let page = r#"<h2> report &amp; data.pdf </h2><form class="form" action="/uploads/abc/downloads"><input name="authenticity_token" value="TOK">"#;
        let (root, server) = serve(vec![redirect, html(page)]);
        let http = client("uploadir", root);
        let files = media(extract(
            &http,
            Url::parse("https://uploadir.com/u/abc").unwrap(),
        ))
        .await;
        assert_eq!(files[0].handoff["_http_method"], "POST");
        assert_eq!(
            files[0].handoff["_http_data"],
            json!({"authenticity_token": "TOK", "upload_id": "abc"})
        );
        assert_eq!(files[0].headers["Cookie"], "_session=S");
        let (file, _) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "report & data (abc).pdf");
        assert_eq!(keywords(&files[0])["id"], "abc");
        server.join().unwrap();
    }
}
