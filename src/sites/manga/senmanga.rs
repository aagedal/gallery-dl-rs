//! Sen Manga raw chapters, mirroring gallery_dl/extractor/senmanga.py.
use super::{Gallery, set};
use crate::error;
use crate::sites::gdl::{
    captures, ensure_http_scheme, extr, extract_iter, group, partition, re, unescape,
};
use crate::sites::{Events, Http, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://raw.senmanga.com";

static CHAPTER: Route = super::chapter_route();

pub static SITE: Site = Site {
    category: "senmanga",
    name: "Sen Manga",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&CHAPTER],
    api_key: false,
    cookies: Some("raw.senmanga.com"),
};

fn target(url: &Url) -> Option<String> {
    let pattern = re!(r"^https?://raw\.senmanga\.com(/[^/?#]+/[^/?#]+)");
    captures(pattern, url).map(|caps| group(&caps, 1).to_owned())
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(path) = target(&url) else {
        return crate::sites::fail(error("unsupported Sen Manga URL"));
    };
    crate::sites::once(async move {
        let page_url = format!("{ROOT}{path}");
        // Select the "All pages" viewer.
        http.set_cookie(
            &http.api(ROOT)?,
            "viewer=1; Domain=raw.senmanga.com; Path=/",
        );
        let page = http
            .text_with(&http.api(&page_url)?, &[("Referer", &page_url)])
            .await?;
        let pages = extract_iter(&page, "<img class=\"picture\" src=\"", "\"")
            .into_iter()
            .map(|url| (ensure_http_scheme(url), None))
            .collect();
        Ok(crate::sites::events(
            Gallery::chapter(&CHAPTER, metadata(&page), pages)
                .header("Referer", page_url)
                .events(&SITE, http),
        ))
    })
}

fn metadata(page: &str) -> Map<String, Value> {
    let (manga, _, chapter) = partition(extr(page, "<title>", "</title>"), " - Chapter ");
    let mut data = Map::new();
    set(&mut data, "manga", unescape(manga).replace('-', " "));
    set(&mut data, "chapter", partition(chapter, " - Page ").0);
    set(&mut data, "chapter_minor", "");
    set(&mut data, "lang", "ja");
    set(&mut data, "language", "Japanese");
    data
}

#[cfg(test)]
mod tests {
    use super::super::tests::path;
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://raw.senmanga.com/Bokura-wa-Minna-Kawaisou/37A/1").as_deref(),
            Some("/Bokura-wa-Minna-Kawaisou/37A")
        );
        assert_eq!(
            t("https://raw.senmanga.com/amama-cinderella/3").as_deref(),
            Some("/amama-cinderella/3")
        );
        assert_eq!(t("https://raw.senmanga.com/amama-cinderella"), None);
        assert_eq!(t("https://senmanga.com/a/b"), None);
    }

    #[tokio::test]
    async fn extracts_chapter() {
        let page = r#"<title>Bokura-wa-Minna-Kawaisou - Chapter 37A - Page 1</title>
            <img class="picture" src="//raw.senmanga.com/viewer/a/1.jpg"><img class="picture" src="https://raw.senmanga.com/viewer/a/2.jpg">"#;
        let (root, server) = serve(vec![html(page)]);
        let http = http("senmanga", root);
        let files = media(extract(
            &http,
            Url::parse("https://raw.senmanga.com/Bokura-wa-Minna-Kawaisou/37A/1").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].url.as_str(),
            "https://raw.senmanga.com/viewer/a/1.jpg"
        );
        assert_eq!(
            files[0].headers["Referer"],
            "https://raw.senmanga.com/Bokura-wa-Minna-Kawaisou/37A"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["manga"], "Bokura wa Minna Kawaisou");
        assert_eq!(k["chapter"], "37A");
        assert_eq!(
            path(&files[1]),
            "senmanga/Bokura wa Minna Kawaisou/c37A/Bokura wa Minna Kawaisou_c37A_002.jpg"
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /Bokura-wa-Minna-Kawaisou/37A "));
        assert!(
            requests[0]
                .to_ascii_lowercase()
                .contains("referer: https://raw.senmanga.com/bokura")
        );
    }
}
