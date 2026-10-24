//! HentaiHere chapters and manga, mirroring
//! gallery_dl/extractor/hentaihere.py: chapter metadata comes from the
//! first reader page's title, images from its `rff_imageList`.
use super::{Gallery, keywords, text};
use crate::sites::art::{digits, extr, json_loads, on_host, segment};
use crate::sites::{Events, Http, Route, Site, unescape};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

pub static SITE: Site = Site {
    category: "hentaihere",
    name: "HentaiHere",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&CHAPTER, &MANGA],
    api_key: false,
    cookies: None,
};

const ROOT: &str = "https://hentaihere.com";
const CDN: &str = "https://hentaicdn.com/hentai";

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
    /// Manga ID and chapter, raw.
    Chapter { id: String, chapter: String },
    /// Manga ID.
    Manga(String),
}

/// `hentaihere\.com/m/S(\d+)/([^/?#]+)` (unanchored) and
/// `hentaihere\.com(/m/S\d+)/?$`.
fn target(url: &Url) -> Option<Target> {
    if !on_host(url, &["hentaihere.com"]) {
        return None;
    }
    let raw: Vec<&str> = url.path().split('/').skip(1).collect();
    let ["m", series, rest @ ..] = raw.as_slice() else {
        return None;
    };
    let id = series.strip_prefix('S').filter(|id| digits(id))?;
    match rest {
        [chapter, ..] if segment(chapter) => Some(Target::Chapter {
            id: id.to_owned(),
            chapter: (*chapter).to_owned(),
        }),
        // `$`: nothing may follow, not even a query.
        [] | [""] if url.query().is_none() && url.fragment().is_none() => {
            Some(Target::Manga(id.to_owned()))
        }
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Chapter { id, chapter }) => crate::sites::once(async move {
            let page = page(http, &format!("{ROOT}/m/S{id}/{chapter}/1")).await?;
            let chapter = crate::sites::percent_decode(&chapter);
            let data = metadata(&page, &id, &chapter)?;
            let cid = data
                .get("chapter_id")
                .map(crate::sites::scalar)
                .unwrap_or_default();
            Ok(crate::sites::events(
                Gallery::chapter(&CHAPTER, data, images(&page)?)
                    .id(cid)
                    .events(&SITE, http),
            ))
        }),
        Some(Target::Manga(id)) => crate::sites::once(async move {
            let page = page(http, &format!("{ROOT}/m/S{id}")).await?;
            let mut children = chapters(&page, &id);
            // `MangaExtractor.reverse = True`.
            if !http.option_bool("chapter-reverse", false) {
                children.reverse();
            }
            Ok(super::queue_urls(http, &SITE, &MANGA, children))
        }),
        None => crate::sites::fail(error("unsupported HentaiHere URL")),
    }
}

async fn page(http: &Http, url: &str) -> Result<String> {
    http.text_with(&http.api(url)?, &http.browser_headers(&[]))
        .await
}

/// `Page 1 \| (.+) \(([^)]+)\) - Chapter \d+: (.+) by (.+) at ` matched at
/// the start of the first line, with the regex's backtracking: manga,
/// type, title and author.
fn parse_title(title: &str) -> Option<(&str, &str, &str, &str)> {
    let line = title.split('\n').next().unwrap_or_default();
    let rest = line.strip_prefix("Page 1 | ")?;
    rest.rmatch_indices(" (").find_map(|(open, _)| {
        if open == 0 {
            return None;
        }
        let after = &rest[open + 2..];
        let close = after.find(')').filter(|&i| i > 0)?;
        let tail = after[close..].strip_prefix(") - Chapter ")?;
        let len = tail.bytes().take_while(u8::is_ascii_digit).count();
        let tail = tail[len..].strip_prefix(": ").filter(|_| len > 0)?;
        tail.rmatch_indices(" by ").find_map(|(by, _)| {
            let author = &tail[by + 4..];
            let at = author.rfind(" at ").filter(|&at| at > 0 && by > 0)?;
            Some((&rest[..open], &after[..close], &tail[..by], &author[..at]))
        })
    })
}

/// `HentaihereChapterExtractor.metadata`.
fn metadata(page: &str, manga_id: &str, chapter: &str) -> Result<Map<String, Value>> {
    let title = extr(page, "<title>", "</title>");
    let (manga, kind, title, author) =
        parse_title(title).ok_or_else(|| error("HentaiHere chapter title not recognized"))?;
    let (number, minor) = text::chapter(chapter);
    Ok(keywords([
        ("manga", manga.into()),
        ("manga_id", text::parse_int(manga_id).into()),
        ("chapter", number.into()),
        ("chapter_minor", minor.into()),
        (
            "chapter_id",
            text::parse_int(extr(page, "report/C", "\"")).into(),
        ),
        ("type", kind.into()),
        ("title", title.into()),
        ("author", author.into()),
        ("lang", "en".into()),
        ("language", "English".into()),
    ]))
}

/// `HentaihereChapterExtractor.images`.
fn images(page: &str) -> Result<Vec<super::Page>> {
    let list = json_loads(extr(page, "var rff_imageList = ", ";"), "HentaiHere")?;
    Ok(list
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .map(|part| (format!("{CDN}{}", crate::sites::scalar(part)), None))
        .collect())
}

/// `HentaihereMangaExtractor.chapters`, in page order.
fn chapters(page: &str, manga_id: &str) -> Vec<(Url, Map<String, Value>)> {
    let pos = page
        .find("itemscope itemtype=\"http://schema.org/Book")
        .map_or(0, |i| i + 1);
    let (manga, pos) = text::extract(page, "<span itemprop=\"name\">", "</span>", pos);
    let (kind, mut pos) = text::extract(
        page,
        "<span class=\"mngType text-danger\">[",
        "]</span>",
        pos,
    );
    let manga_id = text::parse_int(manga_id);
    let mut results = Vec::new();
    loop {
        let (marker, next) = text::extract(page, "<li class=\"sub-chp clearfix\">", "", pos);
        if marker.is_none() {
            return results;
        }
        let (url, next) = text::extract(page, "<a href=\"", "\"", next);
        let (chapter, next) = text::extract(page, "title=\"Tagged: -\">\n", "<", next);
        let (chapter_id, next) = text::extract(page, "/C", "\"", next);
        pos = next;
        let chapter = unescape(chapter.unwrap_or_default());
        let (chapter, _, title) = text::partition(chapter.trim(), " - ");
        let (number, minor) = text::chapter(chapter);
        let Some(url) = url.and_then(|url| Url::parse(url).ok()) else {
            continue;
        };
        results.push((
            url,
            keywords([
                ("manga_id", manga_id.into()),
                ("manga", manga.into()),
                ("chapter", number.into()),
                ("chapter_minor", minor.into()),
                (
                    "chapter_id",
                    text::parse_int(chapter_id.unwrap_or_default()).into(),
                ),
                ("type", kind.into()),
                ("title", title.into()),
                ("lang", "en".into()),
                ("language", "English".into()),
            ]),
        ));
    }
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
            t("https://hentaihere.com/m/S13812/1/1/"),
            Some(Target::Chapter {
                id: "13812".into(),
                chapter: "1".into(),
            })
        );
        assert_eq!(
            t("http://www.hentaihere.com/m/S7/2.5"),
            Some(Target::Chapter {
                id: "7".into(),
                chapter: "2.5".into(),
            })
        );
        assert_eq!(
            t("https://hentaihere.com/m/S13812"),
            Some(Target::Manga("13812".into()))
        );
        assert_eq!(
            t("https://hentaihere.com/m/S13812/"),
            Some(Target::Manga("13812".into()))
        );
        for raw in [
            "https://hentaihere.com/m/S13812?x=1",
            "https://hentaihere.com/m/Sx/1",
            "https://hentaihere.com/m/13812/1",
            "https://hentaihere.com/a/S13812/1",
            "https://m.hentaihere.com/m/S13812",
            "https://hentaihere.com.evil.test/m/S13812",
            "https://hentaihere.com:8443/m/S13812",
            "https://u:p@hentaihere.com/m/S13812",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[test]
    fn parses_reader_titles() {
        assert_eq!(
            parse_title(
                "Page 1 | A (B) (Manga) - Chapter 1: One by Two at Three by X at HentaiHere"
            ),
            Some(("A (B)", "Manga", "One by Two at Three", "X"))
        );
        assert_eq!(
            parse_title("Page 1 | A (Manga) - Chapter x: One by Two at "),
            None
        );
        assert_eq!(parse_title("HentaiHere"), None);
    }

    const READER: &str = r#"<title>Page 1 | Love &amp; Peace (Doujinshi) - Chapter 1: First Time by Artist at HentaiHere</title>
<a href="https://hentaihere.com/report/C24680">report</a>
<script>var rff_imageList = ["/hentai/13812/1/01.jpg","/hentai/13812/1/02.jpg"];</script>"#;

    #[tokio::test]
    async fn extracts_chapter() {
        let (root, requests) = serve(vec![html(READER)]);
        let http = http("HentaiHere", root);
        let files = media(extract(
            &http,
            Url::parse("https://hentaihere.com/m/S13812/1.5/3/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[1].url.as_str(),
            "https://hentaicdn.com/hentai/hentai/13812/1/02.jpg"
        );
        let k = kw(&files[1]);
        assert_eq!(k["manga"], "Love &amp; Peace");
        assert_eq!(k["manga_id"], 13812);
        assert_eq!(k["chapter"], 1);
        assert_eq!(k["chapter_minor"], ".5");
        assert_eq!(k["chapter_id"], 24680);
        assert_eq!(k["type"], "Doujinshi");
        assert_eq!(k["title"], "First Time");
        assert_eq!(k["author"], "Artist");
        assert_eq!(k["page"], 2);
        let colon = if cfg!(windows) { "_" } else { ":" };
        assert_eq!(
            path(&files[1]),
            format!(
                "hentaihere/Love &amp; Peace/c001.5{colon} First Time/Love &amp; Peace_c001.5_002.jpg"
            )
        );
        assert_eq!(
            archive(&files[1]),
            r#"["{chapter_id}_{page}",["24680","2"]]"#
        );
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /m/S13812/1.5/1 "));
    }

    #[tokio::test]
    async fn chapter_with_unknown_title_fails() {
        let (root, requests) = serve(vec![html("<title>x</title>")]);
        let http = http("HentaiHere", root);
        assert!(
            crate::sites::testing::fails(extract(
                &http,
                Url::parse("https://hentaihere.com/m/S1/1/").unwrap(),
            ))
            .await
        );
        requests.join().unwrap();
    }

    const MANGA_PAGE: &str = "<div itemscope itemtype=\"http://schema.org/Book\">
<span itemprop=\"name\">Love &amp; Peace</span><span class=\"mngType text-danger\">[Doujinshi]</span>
<ul><li class=\"sub-chp clearfix\"><a href=\"https://hentaihere.com/m/S13812/2/1/\" title=\"Tagged: -\">
2 - Second &amp; Last</a><a href=\"/report/C902\">r</a></li>
<li class=\"sub-chp clearfix\"><a href=\"https://hentaihere.com/m/S13812/1/1/\" title=\"Tagged: -\">
1</a><a href=\"/report/C901\">r</a></li></ul>";

    #[test]
    fn parses_chapter_list() {
        let list = chapters(MANGA_PAGE, "13812");
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].0.as_str(), "https://hentaihere.com/m/S13812/2/1/");
        let k = &list[0].1;
        assert_eq!(k["manga"], "Love &amp; Peace");
        assert_eq!(k["manga_id"], 13812);
        assert_eq!(k["type"], "Doujinshi");
        assert_eq!(k["chapter"], 2);
        assert_eq!(k["chapter_id"], 902);
        assert_eq!(k["title"], "Second & Last");
        assert_eq!(list[1].1["chapter_id"], 901);
        assert_eq!(list[1].1["title"], "");
    }

    #[tokio::test]
    async fn manga_queues_chapters() {
        let (root, requests) = serve(vec![html(MANGA_PAGE)]);
        let http = http("HentaiHere", root);
        let children = queued(extract(
            &http,
            Url::parse("https://hentaihere.com/m/S13812/").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].subcategory(), "manga");
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /m/S13812 "));
    }
}
