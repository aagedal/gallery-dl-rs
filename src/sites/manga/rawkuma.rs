//! Rawkuma chapters and manga, mirroring gallery_dl/extractor/rawkuma.py:
//! the chapter list comes from an htmx `admin-ajax.php` fragment.
//!
//! Upstream requests chapter pages as `https://rawkuma.net//manga/…/`
//! (the matched path already starts with a slash); this port drops the
//! duplicate slash.
use super::{Gallery, keywords, text};
use crate::sites::art::{extr, extract_iter, segment};
use crate::sites::{Events, Http, Route, Site, unescape};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

pub static SITE: Site = Site {
    category: "rawkuma",
    name: "Rawkuma",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&CHAPTER, &MANGA],
    api_key: false,
    cookies: None,
};

const ROOT: &str = "https://rawkuma.net";

static CHAPTER: Route = Route::new(
    "chapter",
    "{manga}_c{chapter:>03}{chapter_minor:?//}_{page:>03}.{extension}",
    &[
        "{category}",
        "{manga}",
        "{volume:?v/ />02}c{chapter:>03}{chapter_minor:?//}{title:?: //}",
    ],
    "{chapter_id}_{page}",
);
static MANGA: Route = Route::new("manga", "{filename}.{extension}", &["{category}"], "");

#[derive(Debug, PartialEq)]
enum Target {
    /// `/manga/TITLE/chapter-N.ID`, raw, and the chapter ID.
    Chapter { path: String, id: String },
    /// The manga slug, raw.
    Manga(String),
}

/// The length of a `\d+` run at the start of `text`.
fn digit_run(text: &str) -> usize {
    text.bytes().take_while(u8::is_ascii_digit).count()
}

/// `chapter-\d+(?:.\d+)?\.(\d+)` at the start of a segment, with the
/// regex's backtracking: the matched text and the captured ID.
fn chapter_segment(segment: &str) -> Option<(&str, &str)> {
    let rest = segment.strip_prefix("chapter-")?;
    let base = "chapter-".len();
    // `\.(\d+)` at `at`, taking every digit.
    let tail = |at: usize| {
        let after = rest.get(at..)?.strip_prefix('.')?;
        let len = digit_run(after);
        (len > 0).then(|| (&segment[..base + at + 1 + len], &after[..len]))
    };
    for number in (1..=digit_run(rest)).rev() {
        // With the optional `.\d+`: any one character, then digits.
        if let Some(c) = rest[number..].chars().next().filter(|c| *c != '\n') {
            let start = number + c.len_utf8();
            for len in (1..=digit_run(&rest[start..])).rev() {
                if let Some(found) = tail(start + len) {
                    return Some(found);
                }
            }
        }
        if let Some(found) = tail(number) {
            return Some(found);
        }
    }
    None
}

/// `rawkuma\.(?:net|com)(/manga/[^/?#]+/chapter-\d+(?:.\d+)?\.(\d+))` and
/// `rawkuma\.(?:net|com)/manga/([^/?#]+)`, both unanchored at the end.
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) || !matches!(url.host_str()?, "rawkuma.net" | "rawkuma.com") {
        return None;
    }
    let raw: Vec<&str> = url.path().split('/').skip(1).collect();
    let ["manga", title, rest @ ..] = raw.as_slice() else {
        return None;
    };
    if !segment(title) {
        return None;
    }
    if let Some((chapter, id)) = rest.first().and_then(|s| chapter_segment(s)) {
        return Some(Target::Chapter {
            path: format!("/manga/{title}/{chapter}"),
            id: id.to_owned(),
        });
    }
    Some(Target::Manga((*title).to_owned()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Chapter { path, id }) => crate::sites::once(async move {
            let page = page(http, &format!("{ROOT}{path}/"), &[]).await?;
            let data = metadata(&page, &id);
            let timestamp = text::iso(extr(&page, "datetime=\"", "\""));
            Ok(crate::sites::events(
                Gallery::chapter(&CHAPTER, data, images(&page))
                    .id(id)
                    .timestamp(timestamp)
                    .events(&SITE, http),
            ))
        }),
        Some(Target::Manga(slug)) => crate::sites::once(async move {
            let page_url = format!("{ROOT}/manga/{slug}/");
            let page = page(http, &page_url, &[]).await?;
            let manga = unescape(extr(&page, "<title>", " &#8211; "));
            let manga_id = text::parse_int(extr(&page, "manga_id=", "&"));
            let mut url = http.api(&format!("{ROOT}/wp-admin/admin-ajax.php"))?;
            url.query_pairs_mut()
                .append_pair("manga_id", &manga_id.to_string())
                .append_pair("page", "1")
                .append_pair("action", "chapter_list");
            let list = http
                .text_with(
                    &url,
                    &http.browser_headers(&[
                        ("HX-Request", "true"),
                        ("HX-Trigger", "chapter-list"),
                        ("HX-Target", "chapter-list"),
                        ("HX-Current-URL", &page_url),
                        ("Sec-Fetch-Dest", "empty"),
                        ("Sec-Fetch-Mode", "cors"),
                        ("Sec-Fetch-Site", "same-origin"),
                    ]),
                )
                .await?;
            let mut children = chapters(&list, &manga, manga_id);
            // `MangaExtractor.reverse = True`.
            if !http.option_bool("chapter-reverse", false) {
                children.reverse();
            }
            Ok(super::queue_urls(http, &SITE, &MANGA, children))
        }),
        None => crate::sites::fail(error("unsupported Rawkuma URL")),
    }
}

async fn page(http: &Http, url: &str, headers: &[(&str, &str)]) -> Result<String> {
    http.text_with(&http.api(url)?, &http.browser_headers(headers))
        .await
}

/// `RawkumaChapterExtractor.metadata`.
fn metadata(page: &str, id: &str) -> Map<String, Value> {
    let title = extr(page, "<title>", "<");
    let (manga, chapter) = title.rsplit_once(" Chapter ").unwrap_or(("", title));
    let (chapter, minor) = text::chapter(text::partition(chapter, " &#8211; ").0);
    let date = text::iso(extr(page, "datetime=\"", "\""));
    keywords([
        ("manga", unescape(manga).into()),
        (
            "manga_id",
            text::parse_int(extr(page, "manga_id=", "&")).into(),
        ),
        ("chapter", chapter.into()),
        ("chapter_minor", minor.into()),
        ("chapter_id", text::parse_int(id).into()),
        ("date", text::date(date)),
        ("lang", "ja".into()),
        ("language", "Japanese".into()),
    ])
}

/// `RawkumaChapterExtractor.images`.
fn images(page: &str) -> Vec<super::Page> {
    extract_iter(page, "<img src='", "'")
        .into_iter()
        .map(|url| (url.to_owned(), None))
        .collect()
}

/// `RawkumaMangaExtractor.chapters`, in fragment order. The upstream
/// keyword is spelled `chapter-minor`.
fn chapters(list: &str, manga: &str, manga_id: i64) -> Vec<(Url, Map<String, Value>)> {
    extract_iter(list, "<a href=\"", "\"")
        .into_iter()
        .filter_map(|url| {
            // `url[url.rfind("-")+1:-1]`
            let start = url.rfind('-').map_or(0, |i| i + 1);
            let info = url
                .get(start..url.len().saturating_sub(1).max(start))
                .unwrap_or_default();
            let (chapter, id) = info.rsplit_once('.').unwrap_or(("", info));
            let (chapter, minor) = text::chapter(chapter);
            Some((
                Url::parse(url).ok()?,
                keywords([
                    ("manga", manga.into()),
                    ("manga_id", manga_id.into()),
                    ("chapter", chapter.into()),
                    ("chapter-minor", minor.into()),
                    ("chapter_id", text::parse_int(id).into()),
                ]),
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::tests::{archive, path};
    use super::*;
    use crate::sites::testing::{html, http, keywords as kw, media, queued, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://rawkuma.net/manga/kimi-no-koto/chapter-12.5.123456/"),
            Some(Target::Chapter {
                path: "/manga/kimi-no-koto/chapter-12.5.123456".into(),
                id: "123456".into(),
            })
        );
        assert_eq!(
            t("http://rawkuma.com/manga/x/chapter-7.99"),
            Some(Target::Chapter {
                path: "/manga/x/chapter-7.99".into(),
                id: "99".into(),
            })
        );
        assert_eq!(
            chapter_segment("chapter-1x2.3"),
            Some(("chapter-1x2.3", "3"))
        );
        assert_eq!(chapter_segment("chapter-12"), None);
        assert_eq!(
            t("https://rawkuma.net/manga/kimi-no-koto/"),
            Some(Target::Manga("kimi-no-koto".into()))
        );
        assert_eq!(
            t("https://rawkuma.net/manga/kimi-no-koto/chapter-12/"),
            Some(Target::Manga("kimi-no-koto".into()))
        );
        for raw in [
            "https://rawkuma.net/manga/",
            "https://rawkuma.net/series/x/",
            "https://www.rawkuma.net/manga/x/",
            "https://rawkuma.org/manga/x/",
            "https://rawkuma.net.evil.test/manga/x/",
            "https://rawkuma.net:8443/manga/x/",
            "https://u:p@rawkuma.net/manga/x/",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[tokio::test]
    async fn extracts_chapter() {
        let page = r#"<title>Kimi &amp; Koto Chapter 12.5 &#8211; Rawkuma</title>
<a href="https://rawkuma.net/?manga_id=777&amp;x=1">
<time datetime="2025-01-02T03:04:05+00:00">
<img src='https://rcdn.kyut.dev/a/01.jpg' /><img src='https://rcdn.kyut.dev/a/02.png' />"#;
        let (root, requests) = serve(vec![html(page)]);
        let http = http("Rawkuma", root);
        let files = media(extract(
            &http,
            Url::parse("https://rawkuma.net/manga/kimi-no-koto/chapter-12.5.123456").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = kw(&files[1]);
        assert_eq!(k["manga"], "Kimi & Koto");
        assert_eq!(k["manga_id"], 777);
        assert_eq!(k["chapter"], 12);
        assert_eq!(k["chapter_minor"], ".5");
        assert_eq!(k["chapter_id"], 123456);
        assert_eq!(k["date"], "2025-01-02 03:04:05");
        assert_eq!(k["lang"], "ja");
        assert_eq!(k["page"], 2);
        assert_eq!(
            path(&files[1]),
            "rawkuma/Kimi & Koto/c012.5/Kimi & Koto_c012.5_002.png"
        );
        assert_eq!(
            archive(&files[1]),
            r#"["{chapter_id}_{page}",["123456","2"]]"#
        );
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /manga/kimi-no-koto/chapter-12.5.123456/ "));
    }

    const LIST: &str = r#"<div><a href="https://rawkuma.net/manga/kimi-no-koto/chapter-2.5.1002/">2.5</a>
<a href="https://rawkuma.net/manga/kimi-no-koto/chapter-1.1001/">1</a></div>"#;

    #[test]
    fn parses_chapter_list() {
        let chapters = chapters(LIST, "Kimi", 777);
        assert_eq!(chapters.len(), 2);
        let k = &chapters[0].1;
        assert_eq!(k["chapter"], 2);
        assert_eq!(k["chapter-minor"], ".5");
        assert_eq!(k["chapter_id"], 1002);
        assert_eq!(k["manga_id"], 777);
        assert_eq!(chapters[1].1["chapter_id"], 1001);
        assert_eq!(chapters[1].1["chapter-minor"], "");
    }

    #[tokio::test]
    async fn manga_requests_the_chapter_list_fragment() {
        let manga = r#"<title>Kimi &amp; Koto &#8211; Rawkuma</title>
<link href="https://rawkuma.net/?manga_id=777&amp;p=1">"#;
        let (root, requests) = serve(vec![html(manga), html(LIST), html(""), html("")]);
        let http = http("Rawkuma", root);
        assert!(
            media(extract(
                &http,
                Url::parse("https://rawkuma.net/manga/kimi-no-koto").unwrap(),
            ))
            .await
            .is_empty()
        );
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /manga/kimi-no-koto/ "));
        assert!(
            requests[1].starts_with(
                "GET /wp-admin/admin-ajax.php?manga_id=777&page=1&action=chapter_list "
            )
        );
        let list = requests[1].to_ascii_lowercase();
        assert!(list.contains("hx-current-url: https://rawkuma.net/manga/kimi-no-koto/\r\n"));
        assert!(list.contains("hx-target: chapter-list\r\n"));
        assert!(requests[2].starts_with("GET /manga/kimi-no-koto/chapter-1.1001/ "));
        assert!(requests[3].starts_with("GET /manga/kimi-no-koto/chapter-2.5.1002/ "));
    }

    #[tokio::test]
    async fn manga_children_are_chapter_jobs() {
        let (root, requests) = serve(vec![html("<title>K &#8211; R</title>"), html(LIST)]);
        let http = http("Rawkuma", root);
        let children = queued(extract(
            &http,
            Url::parse("https://rawkuma.net/manga/kimi-no-koto/").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].subcategory(), "manga");
        requests.join().unwrap();
    }
}
