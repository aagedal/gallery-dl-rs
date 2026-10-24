//! MangaFreak chapters and manga, mirroring gallery_dl/extractor/mangafreak.py:
//! a chapter's images are all on its `Read1_…` page.
use super::{Gallery, keywords, text};
use crate::sites::art::{extr, extract_iter};
use crate::sites::{Events, Http, Route, Site, unescape};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

pub static SITE: Site = Site {
    category: "mangafreak",
    name: "MangaFreak",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&CHAPTER, &MANGA],
    api_key: false,
    cookies: None,
};

// ww2 now redirects here, off-origin for the same-origin HTTP layer.
const ROOT: &str = "https://ww3.mangafreak.me";
const IMAGES: &str = "https://images.mangafreak.me/mangas/";

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
    /// Manga slug, chapter string, its number and letter suffix.
    Chapter {
        slug: String,
        string: String,
        chapter: String,
        minor: String,
    },
    /// Manga slug, raw.
    Manga(String),
}

/// `(?:ww[\dw]\.)?mangafreak\.me`.
fn host(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = match host.split_once('.') {
        Some((sub, rest))
            if sub.len() == 3
                && sub.starts_with("ww")
                && sub[2..].starts_with(|c: char| c == 'w' || c.is_ascii_digit()) =>
        {
            rest
        }
        _ => host,
    };
    crate::sites::plain(url) && host == "mangafreak.me"
}

/// `/Read1_([^/?#]+)_((\d+)([a-z])?)`: the slug takes everything up to the
/// last `_` that is followed by a digit.
fn chapter_segment(segment: &str) -> Option<Target> {
    let rest = segment.strip_prefix("Read1_")?;
    let (slug, string) = rest
        .match_indices('_')
        .rev()
        .map(|(i, _)| (&rest[..i], &rest[i + 1..]))
        .find(|(slug, string)| {
            !slug.is_empty() && string.starts_with(|c: char| c.is_ascii_digit())
        })?;
    let digits = string.bytes().take_while(u8::is_ascii_digit).count();
    let minor = string[digits..]
        .chars()
        .next()
        .filter(char::is_ascii_lowercase)
        .map(String::from)
        .unwrap_or_default();
    Some(Target::Chapter {
        slug: slug.to_owned(),
        string: format!("{}{minor}", &string[..digits]),
        chapter: string[..digits].to_owned(),
        minor,
    })
}

/// `mangafreak\.me(/Read1_…)` and `mangafreak\.me(/Manga/([^/?#]+))`, both
/// unanchored at the end.
fn target(url: &Url) -> Option<Target> {
    if !host(url) {
        return None;
    }
    let raw: Vec<&str> = url.path().split('/').skip(1).collect();
    match raw.as_slice() {
        ["Manga", slug, ..] if !slug.is_empty() => Some(Target::Manga((*slug).to_owned())),
        [segment, ..] => chapter_segment(segment),
        [] => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Chapter {
            slug,
            string,
            chapter,
            minor,
        }) => crate::sites::once(async move {
            let page = page(http, &format!("{ROOT}/Read1_{slug}_{string}")).await?;
            let data = metadata(&page, &slug, &string, &chapter, &minor);
            Ok(crate::sites::events(
                Gallery::chapter(&CHAPTER, data, images(&page))
                    .id(format!("{slug}_{string}"))
                    .events(&SITE, http),
            ))
        }),
        Some(Target::Manga(slug)) => crate::sites::once(async move {
            let page = page(http, &format!("{ROOT}/Manga/{slug}")).await?;
            let mut children = chapters(&page, &slug);
            // `MangaExtractor.reverse = True`.
            if !http.option_bool("chapter-reverse", false) {
                children.reverse();
            }
            Ok(super::queue_urls(http, &SITE, &MANGA, children))
        }),
        None => crate::sites::fail(error("unsupported MangaFreak URL")),
    }
}

async fn page(http: &Http, url: &str) -> Result<String> {
    http.text_with(&http.api(url)?, &http.browser_headers(&[]))
        .await
}

/// `MangafreakChapterExtractor.metadata`.
fn metadata(
    page: &str,
    slug: &str,
    string: &str,
    chapter: &str,
    minor: &str,
) -> Map<String, Value> {
    let title = text::partition(extr(page, "selected=\"selected\">", "<"), ": ").2;
    keywords([
        (
            "manga",
            unescape(extr(page, "<title>Read ", " Chapter ")).into(),
        ),
        ("manga_slug", slug.into()),
        ("title", unescape(title).into()),
        ("chapter", text::parse_int(chapter).into()),
        ("chapter_minor", minor.into()),
        ("chapter_string", string.into()),
        ("lang", "en".into()),
        ("language", "English".into()),
    ])
}

/// `MangafreakChapterExtractor.images`.
fn images(page: &str) -> Vec<super::Page> {
    extract_iter(page, &format!("src=\"{IMAGES}"), "\"")
        .into_iter()
        .map(|path| (format!("{IMAGES}{path}"), None))
        .collect()
}

/// `MangafreakMangaExtractor.chapters`, in table order.
fn chapters(page: &str, slug: &str) -> Vec<(Url, Map<String, Value>)> {
    let table = extr(page, "<table>", "</table>");
    let manga = unescape(extr(page, "<title>", " Manga"));
    extract_iter(table, "<tr>", "</tr>")
        .into_iter()
        .filter_map(|row| {
            let href = extr(row, "<a href=\"", "\"");
            if href.is_empty() {
                return None;
            }
            let string = href.rsplit('_').next().unwrap_or_default();
            // `(\d+)(\w*)`
            let digits = string.bytes().take_while(u8::is_ascii_digit).count();
            let minor: String = string[digits..]
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            let title = text::partition(extr(row, "\">", "<"), " - ").2;
            Some((
                Url::parse(&format!("{ROOT}{href}")).ok()?,
                keywords([
                    ("chapter", text::parse_int(&string[..digits]).into()),
                    ("chapter_minor", minor.into()),
                    ("chapter_string", string.into()),
                    ("title", unescape(title).into()),
                    ("manga", manga.clone().into()),
                    ("manga_slug", slug.into()),
                    ("lang", "en".into()),
                    ("language", "English".into()),
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

    fn chapter(slug: &str, string: &str, chapter: &str, minor: &str) -> Option<Target> {
        Some(Target::Chapter {
            slug: slug.into(),
            string: string.into(),
            chapter: chapter.into(),
            minor: minor.into(),
        })
    }

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://ww2.mangafreak.me/Read1_Onepunch_Man_1"),
            chapter("Onepunch_Man", "1", "1", "")
        );
        assert_eq!(
            t("http://mangafreak.me/Read1_A_12_103e/2"),
            chapter("A_12", "103e", "103", "e")
        );
        assert_eq!(
            t("https://www.mangafreak.me/Read1_Onepunch_Man_7xyz"),
            chapter("Onepunch_Man", "7x", "7", "x")
        );
        assert_eq!(
            t("https://ww9.mangafreak.me/Manga/Onepunch_Man"),
            Some(Target::Manga("Onepunch_Man".into()))
        );
        for raw in [
            "https://ww2.mangafreak.me/Read1_Onepunch_Man",
            "https://ww2.mangafreak.me/Manga/",
            "https://ww2.mangafreak.me/Genre/Action",
            "https://wwx.mangafreak.me/Manga/x",
            "https://ww22.mangafreak.me/Manga/x",
            "https://ww2.mangafreak.me.evil.test/Manga/x",
            "https://ww2.mangafreak.me:8443/Manga/x",
            "https://u:p@ww2.mangafreak.me/Manga/x",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[tokio::test]
    async fn extracts_chapter() {
        let page = r#"<title>Read Onepunch &amp; Man Chapter 103e Online</title>
<option value="x" selected="selected">Chapter 103e: Fight &amp; Win</option>
<img src="https://images.mangafreak.me/mangas/onepunch_man/onepunch_man_103e/onepunch_man_103e_1.jpg">
<img src="https://images.mangafreak.me/mangas/onepunch_man/onepunch_man_103e/onepunch_man_103e_2.jpg">
<img src="https://ads.test/x.jpg">"#;
        let (root, requests) = serve(vec![html(page)]);
        let http = http("MangaFreak", root);
        let files = media(extract(
            &http,
            Url::parse("https://ww2.mangafreak.me/Read1_Onepunch_Man_103e").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = kw(&files[1]);
        assert_eq!(k["manga"], "Onepunch & Man");
        assert_eq!(k["manga_slug"], "Onepunch_Man");
        assert_eq!(k["title"], "Fight & Win");
        assert_eq!(k["chapter"], 103);
        assert_eq!(k["chapter_minor"], "e");
        assert_eq!(k["chapter_string"], "103e");
        assert_eq!(k["page"], 2);
        assert_eq!(k["count"], 2);
        let colon = if cfg!(windows) { "_" } else { ":" };
        assert_eq!(
            path(&files[1]),
            format!(
                "mangafreak/Onepunch & Man/c103e{colon} Fight & Win/Onepunch & Man_c103e_002.jpg"
            )
        );
        assert_eq!(
            archive(&files[1]),
            r#"["{manga}_{chapter}{chapter_minor}_{page}",["Onepunch & Man","103","e","2"]]"#
        );
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /Read1_Onepunch_Man_103e "));
    }

    const MANGA_PAGE: &str = r#"<title>Onepunch Man Manga</title>
<table><tr><th>Chapter</th></tr>
<tr><td><a href="/Read1_Onepunch_Man_2b">Onepunch Man 2b - Second &amp; Last</a></td></tr>
<tr><td><a href="/Read1_Onepunch_Man_1">Onepunch Man 1</a></td></tr>
</table>"#;

    #[test]
    fn parses_chapter_list() {
        let list = chapters(MANGA_PAGE, "Onepunch_Man");
        assert_eq!(list.len(), 2);
        assert_eq!(
            list[0].0.as_str(),
            "https://ww3.mangafreak.me/Read1_Onepunch_Man_2b"
        );
        let k = &list[0].1;
        assert_eq!(k["manga"], "Onepunch Man");
        assert_eq!(k["chapter"], 2);
        assert_eq!(k["chapter_minor"], "b");
        assert_eq!(k["chapter_string"], "2b");
        assert_eq!(k["title"], "Second & Last");
        assert_eq!(list[1].1["title"], "");
        assert_eq!(list[1].1["chapter_minor"], "");
        assert!(chapters("<title>x Manga</title>", "x").is_empty());
    }

    #[tokio::test]
    async fn manga_queues_chapters_oldest_first() {
        let (root, requests) = serve(vec![html(MANGA_PAGE)]);
        let http = http("MangaFreak", root);
        let children = queued(extract(
            &http,
            Url::parse("https://ww2.mangafreak.me/Manga/Onepunch_Man").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].subcategory(), "manga");
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /Manga/Onepunch_Man "));
    }

    #[tokio::test]
    async fn manga_honours_chapter_reverse() {
        let (root, requests) = serve(vec![html(MANGA_PAGE), html(""), html("")]);
        let http = http("MangaFreak", root).with_options(Some(std::sync::Arc::new(
            serde_json::json!({"chapter-reverse": true})
                .as_object()
                .unwrap()
                .clone(),
        )));
        assert!(
            media(extract(
                &http,
                Url::parse("https://ww2.mangafreak.me/Manga/Onepunch_Man").unwrap(),
            ))
            .await
            .is_empty()
        );
        let requests = requests.join().unwrap();
        assert!(requests[1].starts_with("GET /Read1_Onepunch_Man_2b "));
        assert!(requests[2].starts_with("GET /Read1_Onepunch_Man_1 "));
    }
}
