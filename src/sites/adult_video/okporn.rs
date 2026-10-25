//! ok.porn image albums, mirroring gallery_dl/extractor/okporn.py: the
//! album page's lazy-loaded `data-original` images.
use crate::error;
use crate::sites::art::{extr, extract_iter, leading_digits, on_host, parse_int};
use crate::sites::manga::{Gallery, keywords};
use crate::sites::{Events, Http, Route, Site, unescape};
use url::Url;

const ROOT: &str = "https://ok.porn";

static GALLERY: Route = Route::new(
    "gallery",
    "{category}_{gallery_id}_{num:>03}.{extension}",
    &["{category}", "{gallery_id} {title}"],
    "{gallery_id}_{num}",
);

pub static SITE: Site = Site {
    category: "okporn",
    name: "ok.porn",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&GALLERY],
    api_key: false,
    cookies: None,
};

/// `ok\.porn/albums/(\d+)`, unanchored at the end.
fn target(url: &Url) -> Option<String> {
    if !on_host(url, &["ok.porn"]) {
        return None;
    }
    let id = leading_digits(url.path().strip_prefix("/albums/")?)?;
    Some(id.to_owned())
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(id) = target(&url) else {
        return crate::sites::fail(error("unsupported ok.porn URL"));
    };
    crate::sites::once(async move {
        let page = http
            .text(&http.api(&format!("{ROOT}/albums/{id}/"))?)
            .await?;
        let data = keywords([
            ("gallery_id", parse_int(&id, 0).into()),
            (
                "title",
                unescape(extr(&page, "h1 class=title>", "</h1>")).into(),
            ),
            (
                "description",
                unescape(extr(&page, "name=\"description\" content=\"", "\"")).into(),
            ),
            (
                "tags",
                extr(&page, "name=\"keywords\" content=\"", "\"")
                    .split(", ")
                    .collect::<Vec<_>>()
                    .into(),
            ),
        ]);
        let pages = extract_iter(&page, "data-original=\"", "\"")
            .into_iter()
            .map(|url| (url.to_owned(), None))
            .collect();
        let gallery = Gallery {
            enum_key: "num",
            ..Gallery::chapter(&GALLERY, data, pages).id(id)
        };
        Ok(crate::sites::events(gallery.events(&SITE, http)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::manga::tests::{archive, path};
    use crate::sites::testing::{html, http, keywords as kw, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(t("https://ok.porn/albums/12345/"), Some("12345".into()));
        assert_eq!(t("http://www.ok.porn/albums/7/slug/"), Some("7".into()));
        for raw in [
            "https://ok.porn/albums/",
            "https://ok.porn/videos/12345/",
            "https://ok.porn.evil.test/albums/1/",
            "https://evilok.porn/albums/1/",
            "https://ok.porn:8443/albums/1/",
            "https://u:p@ok.porn/albums/1/",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[tokio::test]
    async fn extracts_album() {
        let page = r#"<meta name="description" content="Beach &amp; sun">
<meta name="keywords" content="beach, summer">
<h1 class=title>Summer &amp; Sea</h1>
<img data-original="https://cdn.ok.test/a/1/one.jpg"><img data-original="https://cdn.ok.test/a/1/two.jpg">"#;
        let (root, server) = serve(vec![html(page)]);
        let http = http("ok.porn", root);
        let files = media(extract(
            &http,
            Url::parse("https://ok.porn/albums/12345/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = kw(&files[1]);
        assert_eq!(k["gallery_id"], 12345);
        assert_eq!(k["title"], "Summer & Sea");
        assert_eq!(k["description"], "Beach & sun");
        assert_eq!(k["tags"], json!(["beach", "summer"]));
        assert_eq!(k["count"], 2);
        assert_eq!(k["filename"], "two");
        assert_eq!(
            path(&files[1]),
            "okporn/12345 Summer & Sea/okporn_12345_002.jpg"
        );
        assert_eq!(
            archive(&files[1]),
            r#"["{gallery_id}_{num}",["12345","2"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /albums/12345/ "));
    }
}
