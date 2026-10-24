//! Dandadan chapters and the chapter index, mirroring
//! gallery_dl/extractor/dandadan.py.
use super::{Gallery, keywords, text};
use crate::sites::art::{extr, extract_iter, segment};
use crate::sites::{Events, Http, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

pub static SITE: Site = Site {
    category: "dandadan",
    name: "Dandadan",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&CHAPTER, &MANGA],
    api_key: false,
    cookies: None,
};

const ROOT: &str = "https://dandadan.net";

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
static MANGA: Route = Route::new("manga", "{filename}.{extension}", &["{category}"], "");

#[derive(Debug, PartialEq)]
enum Target {
    /// `/manga/dandadan-chapter-N[/]`, raw.
    Chapter(String),
    /// Any other page: upstream's unanchored `(/)` matches the root.
    Manga,
}

/// `dandadan\.net(/manga/dandadan-chapter-([^/?#]+)/?)` and
/// `dandadan\.net(/)`, both unanchored at the end.
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::art::on_host(url, &["dandadan.net"]) {
        return None;
    }
    let raw: Vec<&str> = url.path().split('/').skip(1).collect();
    Some(match raw.as_slice() {
        ["manga", name, rest @ ..]
            if name.strip_prefix("dandadan-chapter-").is_some_and(segment) =>
        {
            let slash = if rest.is_empty() { "" } else { "/" };
            Target::Chapter(format!("/manga/{name}{slash}"))
        }
        _ => Target::Manga,
    })
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Chapter(path)) => crate::sites::once(async move {
            let page = page(http, &format!("{ROOT}{path}")).await?;
            Ok(crate::sites::events(
                Gallery::chapter(&CHAPTER, metadata(&page), images(&page))
                    .id(path)
                    .events(&SITE, http),
            ))
        }),
        Some(Target::Manga) => crate::sites::once(async move {
            let page = page(http, &format!("{ROOT}/")).await?;
            let mut children = chapters(&page);
            // `MangaExtractor.reverse = True`.
            if !http.option_bool("chapter-reverse", false) {
                children.reverse();
            }
            Ok(super::queue_urls(http, &SITE, &MANGA, children))
        }),
        None => crate::sites::fail(error("unsupported Dandadan URL")),
    }
}

async fn page(http: &Http, url: &str) -> Result<String> {
    http.text_with(&http.api(url)?, &http.browser_headers(&[]))
        .await
}

/// `DandadanChapterExtractor.metadata`.
fn metadata(page: &str) -> Map<String, Value> {
    let (chapter, minor) = text::chapter(extr(page, "hapter ", " - "));
    keywords([
        ("manga", "Dandadan".into()),
        ("chapter", chapter.into()),
        ("chapter_minor", minor.into()),
        ("lang", "en".into()),
    ])
}

/// `DandadanChapterExtractor.images`: each `<figure>`'s image, else the
/// centred `<img>` elements.
fn images(page: &str) -> Vec<super::Page> {
    let figures: Vec<super::Page> = extract_iter(page, "<figure", "</figure>")
        .into_iter()
        .map(|figure| (extr(figure, "src=\"", "\"").to_owned(), None))
        .collect();
    if !figures.is_empty() {
        return figures;
    }
    extract_iter(
        page,
        "<img decoding=\"async\" class=\"aligncenter\" src=\"",
        "\"",
    )
    .into_iter()
    .map(|src| (src.to_owned(), None))
    .collect()
}

/// `DandadanMangaExtractor.chapters`, in page order.
fn chapters(page: &str) -> Vec<(Url, Map<String, Value>)> {
    extract_iter(page, "<li id=\"su-post", "</li>")
        .into_iter()
        .filter_map(|post| Url::parse(extr(post, "href=\"", "\"")).ok())
        .map(|url| (url, Map::new()))
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
            t("https://dandadan.net/manga/dandadan-chapter-171/"),
            Some(Target::Chapter("/manga/dandadan-chapter-171/".into()))
        );
        assert_eq!(
            t("https://www.dandadan.net/manga/dandadan-chapter-12-5"),
            Some(Target::Chapter("/manga/dandadan-chapter-12-5".into()))
        );
        assert_eq!(t("https://dandadan.net/"), Some(Target::Manga));
        assert_eq!(t("https://dandadan.net/manga/other/"), Some(Target::Manga));
        assert_eq!(
            t("https://dandadan.net/manga/dandadan-chapter-/"),
            Some(Target::Manga)
        );
        for raw in [
            "https://m.dandadan.net/",
            "https://dandadan.net.evil.test/",
            "https://dandadan.net:8443/",
            "https://u:p@dandadan.net/",
            "ftp://dandadan.net/",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[tokio::test]
    async fn extracts_chapter_figures() {
        let page = r#"<title>Dandadan Chapter 171.5 - Read Online</title>
<figure class="wp-block-image"><img src="https://cdn.dandadan.net/171/01.webp"></figure>
<figure class="wp-block-image"><img src="https://cdn.dandadan.net/171/02.jpg"></figure>"#;
        let (root, requests) = serve(vec![html(page)]);
        let http = http("Dandadan", root);
        let files = media(extract(
            &http,
            Url::parse("https://dandadan.net/manga/dandadan-chapter-171/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = kw(&files[1]);
        assert_eq!(k["manga"], "Dandadan");
        assert_eq!(k["chapter"], 171);
        assert_eq!(k["chapter_minor"], ".5");
        assert_eq!(k["lang"], "en");
        assert!(!k.contains_key("language"));
        assert_eq!(k["page"], 2);
        assert_eq!(
            path(&files[1]),
            "dandadan/Dandadan/c171.5/Dandadan_c171.5_002.jpg"
        );
        assert_eq!(
            archive(&files[1]),
            r#"["{manga}_{chapter}{chapter_minor}_{page}",["Dandadan","171",".5","2"]]"#
        );
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /manga/dandadan-chapter-171/ "));
    }

    #[test]
    fn falls_back_to_centred_images() {
        let page = r#"<img decoding="async" class="aligncenter" src="https://x.test/1.jpg">
<img decoding="async" class="aligncenter" src="https://x.test/2.jpg">"#;
        assert_eq!(images(page).len(), 2);
    }

    #[tokio::test]
    async fn index_queues_chapters_oldest_first() {
        let index = r#"<ul><li id="su-post-2"><a href="https://dandadan.net/manga/dandadan-chapter-2/">2</a></li>
<li id="su-post-1"><a href="https://dandadan.net/manga/dandadan-chapter-1/">1</a></li></ul>"#;
        let (root, requests) = serve(vec![html(index), html(""), html("")]);
        let http = http("Dandadan", root);
        assert!(
            media(extract(
                &http,
                Url::parse("https://dandadan.net/about").unwrap()
            ))
            .await
            .is_empty()
        );
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET / "));
        assert!(requests[1].starts_with("GET /manga/dandadan-chapter-1/ "));
        assert!(requests[2].starts_with("GET /manga/dandadan-chapter-2/ "));
    }
}
