//! Sen Manga raw chapters, mirroring gallery_dl/extractor/senmanga.py: the
//! `viewer=1` cookie selects the all-pages reader, and every request
//! (images included) carries the chapter page as Referer.
use super::{Gallery, keywords, text};
use crate::sites::art::{extr, extract_iter, segment};
use crate::sites::{Events, Http, Route, Site, unescape};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

pub static SITE: Site = Site {
    category: "senmanga",
    name: "Sen Manga",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&CHAPTER],
    api_key: false,
    cookies: Some("raw.senmanga.com"),
};

const ROOT: &str = "https://raw.senmanga.com";

static CHAPTER: Route = Route::new(
    "chapter",
    "{manga}_c{chapter:>03}{chapter_minor:?//}_{page:>03}.{extension}",
    &[
        "{category}",
        "{manga}",
        "{volume:?v/ />02}c{chapter:>03}{chapter_minor:?//}{title:?: //}",
    ],
    "{manga}_{chapter}{chapter_minor}_{page}",
);

/// `raw\.senmanga\.com(/[^/?#]+/[^/?#]+)`, unanchored: the raw path.
fn target(url: &Url) -> Option<String> {
    if !crate::sites::plain(url) || url.host_str() != Some("raw.senmanga.com") {
        return None;
    }
    let raw: Vec<&str> = url.path().split('/').skip(1).collect();
    match raw.as_slice() {
        [manga, chapter, ..] if segment(manga) && segment(chapter) => {
            Some(format!("/{manga}/{chapter}"))
        }
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(path) => crate::sites::once(async move { chapter_events(http, &path).await }),
        None => crate::sites::fail(error("unsupported Sen Manga URL")),
    }
}

async fn chapter_events<'a>(http: &'a Http, path: &str) -> Result<Events<'a>> {
    let page_url = format!("{ROOT}{path}");
    let url = http.api(&page_url)?;
    // Select the "All pages" viewer.
    http.set_cookie(&url, "viewer=1");
    let page = http
        .text_with(&url, &http.browser_headers(&[("Referer", &page_url)]))
        .await?;
    Ok(crate::sites::events(
        Gallery::chapter(&CHAPTER, metadata(&page), images(&page))
            .id(path)
            .header("Referer", page_url)
            .events(&SITE, http),
    ))
}

/// `SenmangaChapterExtractor.metadata`.
fn metadata(page: &str) -> Map<String, Value> {
    let title = extr(page, "<title>", "</title>");
    let (manga, _, chapter) = text::partition(title, " - Chapter ");
    keywords([
        ("manga", unescape(manga).replace('-', " ").into()),
        ("chapter", text::partition(chapter, " - Page ").0.into()),
        ("chapter_minor", "".into()),
        ("lang", "ja".into()),
        ("language", "Japanese".into()),
    ])
}

/// `SenmangaChapterExtractor.images`.
fn images(page: &str) -> Vec<super::Page> {
    extract_iter(page, "<img class=\"picture\" src=\"", "\"")
        .into_iter()
        .map(|url| (text::ensure_http_scheme(url), None))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::tests::{archive, path};
    use super::*;
    use crate::sites::testing::{html, http, keywords as kw, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://raw.senmanga.com/Bokura-wa-Minna-Kawaisou/37A/1"),
            Some("/Bokura-wa-Minna-Kawaisou/37A".into())
        );
        assert_eq!(
            t("http://raw.senmanga.com/Hajime-no-Ippo/1258?x=1"),
            Some("/Hajime-no-Ippo/1258".into())
        );
        for raw in [
            "https://raw.senmanga.com/Hajime-no-Ippo",
            "https://raw.senmanga.com/Hajime-no-Ippo/",
            "https://senmanga.com/Hajime-no-Ippo/1258",
            "https://raw.senmanga.com.evil.test/Hajime-no-Ippo/1258",
            "https://raw.senmanga.com:8443/Hajime-no-Ippo/1258",
            "https://u:p@raw.senmanga.com/Hajime-no-Ippo/1258",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[tokio::test]
    async fn extracts_all_pages_with_referer_and_viewer_cookie() {
        let page = r#"<title>Bokura-wa-Minna-Kawaisou &amp; Co - Chapter 37A - Page 1</title>
<img class="picture" src="//kumacdn.club/image/view/37A/1.jpg">
<img class="picture" src="https://kumacdn.club/image/view/37A/2.png">"#;
        let (root, requests) = serve(vec![html(page)]);
        let http = http("Sen Manga", root);
        let files = media(extract(
            &http,
            Url::parse("https://raw.senmanga.com/Bokura-wa-Minna-Kawaisou/37A/2").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].url.as_str(),
            "https://kumacdn.club/image/view/37A/1.jpg"
        );
        assert_eq!(
            files[1].headers["Referer"],
            "https://raw.senmanga.com/Bokura-wa-Minna-Kawaisou/37A"
        );
        let k = kw(&files[1]);
        assert_eq!(k["manga"], "Bokura wa Minna Kawaisou & Co");
        assert_eq!(k["chapter"], "37A");
        assert_eq!(k["chapter_minor"], "");
        assert_eq!(k["lang"], "ja");
        assert_eq!(k["page"], 2);
        assert_eq!(
            path(&files[1]),
            "senmanga/Bokura wa Minna Kawaisou & Co/c37A/Bokura wa Minna Kawaisou & Co_c37A_002.png"
        );
        assert_eq!(
            archive(&files[1]),
            r#"["{manga}_{chapter}{chapter_minor}_{page}",["Bokura wa Minna Kawaisou & Co","37A","","2"]]"#
        );
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /Bokura-wa-Minna-Kawaisou/37A "));
        let head = requests[0].to_ascii_lowercase();
        assert!(head.contains("cookie: viewer=1\r\n"));
        assert!(
            head.contains("referer: https://raw.senmanga.com/bokura-wa-minna-kawaisou/37a\r\n")
        );
    }
}
