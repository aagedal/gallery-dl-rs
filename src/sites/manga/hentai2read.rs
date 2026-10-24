//! Hentai2Read chapters and manga, mirroring
//! gallery_dl/extractor/hentai2read.py: chapter metadata comes from the
//! reader page's title, images from its embedded `'images'` list.
use super::{Gallery, keywords, text};
use crate::sites::art::{json_loads, on_host, segment};
use crate::sites::{Events, Http, Route, Site, unescape};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

pub static SITE: Site = Site {
    category: "hentai2read",
    name: "Hentai2Read",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&CHAPTER, &MANGA],
    api_key: false,
    cookies: None,
};

const ROOT: &str = "https://hentai2read.com";
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
    /// `/TITLE/CHAPTER`, raw, and the chapter.
    Chapter { path: String, chapter: String },
    /// `/TITLE`, raw.
    Manga(String),
}

/// `hentai2read\.com(/[^/?#]+/([^/?#]+))` (unanchored) and
/// `hentai2read\.com(/[^/?#]+)/?$`.
fn target(url: &Url) -> Option<Target> {
    if !on_host(url, &["hentai2read.com"]) {
        return None;
    }
    let raw: Vec<&str> = url.path().split('/').skip(1).collect();
    match raw.as_slice() {
        [title, chapter, ..] if segment(title) && segment(chapter) => Some(Target::Chapter {
            path: format!("/{title}/{chapter}"),
            chapter: crate::sites::percent_decode(chapter),
        }),
        // `$`: nothing may follow, not even a query.
        [title] | [title, ""]
            if segment(title) && url.query().is_none() && url.fragment().is_none() =>
        {
            Some(Target::Manga(format!("/{title}")))
        }
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Chapter { path, chapter }) => crate::sites::once(async move {
            let page = page(http, &format!("{ROOT}{path}")).await?;
            let data = metadata(&page, &chapter);
            let id = data
                .get("chapter_id")
                .map(crate::sites::scalar)
                .unwrap_or_default();
            Ok(crate::sites::events(
                Gallery::chapter(&CHAPTER, data, images(&page)?)
                    .id(id)
                    .events(&SITE, http),
            ))
        }),
        Some(Target::Manga(path)) => crate::sites::once(async move {
            let page = page(http, &format!("{ROOT}{path}")).await?;
            let mut children = chapters(&page);
            // `MangaExtractor.reverse = True`.
            if !http.option_bool("chapter-reverse", false) {
                children.reverse();
            }
            Ok(super::queue_urls(http, &SITE, &MANGA, children))
        }),
        None => crate::sites::fail(error("unsupported Hentai2Read URL")),
    }
}

async fn page(http: &Http, url: &str) -> Result<String> {
    http.text_with(&http.api(url)?, &http.browser_headers(&[]))
        .await
}

/// ` - ([^:]+): (.+) . Page 1 ` at the start of `txt`: the title.
fn chapter_title(txt: &str) -> Option<&str> {
    let rest = txt.strip_prefix(" - ")?;
    let colon = rest.find(':').filter(|&i| i > 0)?;
    let rest = rest[colon..].strip_prefix(": ")?;
    rest.rmatch_indices(" Page 1 ").find_map(|(at, _)| {
        // `(.+) .`: the title, a space and any one character.
        let (last, _) = rest[..at].char_indices().next_back()?;
        rest[..last]
            .strip_suffix(' ')
            .filter(|title| !title.is_empty())
    })
}

/// `Reading (.+) \(([^)]+)\) Hentai(?: by (.*))? - ([^:]+): (.+) . Page 1 `
/// matched at the start of the first line, with the regex's backtracking:
/// manga, type, author (if the group took part) and title.
fn parse_title(title: &str) -> Option<(&str, &str, Option<&str>, &str)> {
    let line = title.split('\n').next().unwrap_or_default();
    let rest = line.strip_prefix("Reading ")?;
    rest.rmatch_indices(" (").find_map(|(open, _)| {
        if open == 0 {
            return None;
        }
        let after = &rest[open + 2..];
        let close = after.find(')').filter(|&i| i > 0)?;
        let tail = after[close..].strip_prefix(") Hentai")?;
        let manga = &rest[..open];
        let kind = &after[..close];
        let by = tail.strip_prefix(" by ").and_then(|by| {
            by.rmatch_indices(" - ")
                .find_map(|(dash, _)| Some((&by[..dash], chapter_title(&by[dash..])?)))
        });
        match by {
            Some((author, title)) => Some((manga, kind, Some(author), title)),
            None => Some((manga, kind, None, chapter_title(tail)?)),
        }
    })
}

/// `Hentai2readChapterExtractor.metadata`.
fn metadata(page: &str, chapter: &str) -> Map<String, Value> {
    let (title, pos) = text::extract(page, "<title>", "</title>", 0);
    let (manga_id, pos) = text::extract(page, "data-mid=\"", "\"", pos);
    let (chapter_id, _) = text::extract(page, "data-cid=\"", "\"", pos);
    let (number, minor) = text::chapter(chapter);
    // Upstream warns and leaves these empty when the title does not match.
    let (manga, kind, author, title) =
        parse_title(title.unwrap_or_default()).unwrap_or(("", "", Some(""), ""));
    keywords([
        ("manga", manga.into()),
        (
            "manga_id",
            text::parse_int(manga_id.unwrap_or_default()).into(),
        ),
        ("chapter", number.into()),
        ("chapter_minor", minor.into()),
        (
            "chapter_id",
            text::parse_int(chapter_id.unwrap_or_default()).into(),
        ),
        ("type", kind.into()),
        ("author", author.into()),
        ("title", title.into()),
        ("lang", "en".into()),
        ("language", "English".into()),
    ])
}

/// `Hentai2readChapterExtractor.images`.
fn images(page: &str) -> Result<Vec<super::Page>> {
    let (list, _) = text::extract(page, "'images' : ", ",\n", 0);
    let list = json_loads(list.unwrap_or_default(), "Hentai2Read")?;
    Ok(list
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .map(|part| (format!("{CDN}{}", crate::sites::scalar(part)), None))
        .collect())
}

/// `Hentai2readMangaExtractor.chapters`, in page order.
fn chapters(page: &str) -> Vec<(Url, Map<String, Value>)> {
    let pos = page
        .find("itemscope itemtype=\"http://schema.org/Book")
        .map_or(0, |i| i + 1);
    let (manga, pos) = text::extract(page, "<span itemprop=\"name\">", "</span>", pos);
    let (kind, pos) = text::extract(page, "<small class=\"text-danger\">[", "]</small>", pos);
    let (manga_id, mut pos) = text::extract(page, "data-mid=\"", "\"", pos);
    let manga_id = text::parse_int(manga_id.unwrap_or_default());
    let mut results = Vec::new();
    loop {
        let (chapter_id, next) = text::extract(page, " data-cid=\"", "\"", pos);
        let Some(chapter_id) = chapter_id.filter(|id| !id.is_empty()) else {
            return results;
        };
        let (_, next) = text::extract(page, " href=\"", "\"", next);
        let (url, next) = text::extract(page, " href=\"", "\"", next);
        let (chapter, next) = text::extract(page, ">", "<", next);
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
                ("manga", manga.into()),
                ("manga_id", manga_id.into()),
                ("chapter", number.into()),
                ("chapter_minor", minor.into()),
                ("chapter_id", text::parse_int(chapter_id).into()),
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
    use crate::sites::testing::{html, http, keywords as kw, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://hentai2read.com/amazon_elixir/1/"),
            Some(Target::Chapter {
                path: "/amazon_elixir/1".into(),
                chapter: "1".into(),
            })
        );
        assert_eq!(
            t("http://www.hentai2read.com/amazon_elixir/2.5/3/?x"),
            Some(Target::Chapter {
                path: "/amazon_elixir/2.5".into(),
                chapter: "2.5".into(),
            })
        );
        assert_eq!(
            t("https://hentai2read.com/amazon_elixir/"),
            Some(Target::Manga("/amazon_elixir".into()))
        );
        assert_eq!(
            t("https://hentai2read.com/amazon_elixir"),
            Some(Target::Manga("/amazon_elixir".into()))
        );
        for raw in [
            "https://hentai2read.com/",
            "https://hentai2read.com/amazon_elixir/?page=2",
            "https://m.hentai2read.com/amazon_elixir/",
            "https://hentai2read.com.evil.test/amazon_elixir/",
            "https://hentai2read.com:8443/amazon_elixir/",
            "https://u:p@hentai2read.com/amazon_elixir/",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[test]
    fn parses_reader_titles() {
        assert_eq!(
            parse_title(
                "Reading Amazon (Elixir) (Doujinshi) Hentai by Some (One) - 1: Part - A: B | Page 1 Online"
            ),
            Some((
                "Amazon (Elixir)",
                "Doujinshi",
                Some("Some (One) - 1: Part"),
                "B"
            ))
        );
        assert_eq!(
            parse_title("Reading Amazon (Manga) Hentai - 2: Online. Page 1 x"),
            None
        );
        assert_eq!(
            parse_title("Reading Amazon Elixir (Manga) Hentai - 2: Second Chapter | Page 1 Online"),
            Some(("Amazon Elixir", "Manga", None, "Second Chapter"))
        );
        assert_eq!(parse_title("Hentai2Read"), None);
    }

    const READER: &str = "<title>Reading Amazon Elixir (Original) Hentai by Shoot - 1: Pilot &amp; Co | Page 1 Online</title>
<div data-mid=\"1234\" data-cid=\"5678\"></div>
<script>var gData = {'images' : [\"/hentai/1/1/a.jpg\",\"/hentai/1/1/b.png\"],\n'x': 1};</script>";

    #[tokio::test]
    async fn extracts_chapter() {
        let (root, requests) = serve(vec![html(READER)]);
        let http = http("Hentai2Read", root);
        let files = media(extract(
            &http,
            Url::parse("https://hentai2read.com/amazon_elixir/1.5/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].url.as_str(),
            "https://hentaicdn.com/hentai/hentai/1/1/a.jpg"
        );
        let k = kw(&files[1]);
        assert_eq!(k["manga"], "Amazon Elixir");
        assert_eq!(k["manga_id"], 1234);
        assert_eq!(k["chapter"], 1);
        assert_eq!(k["chapter_minor"], ".5");
        assert_eq!(k["chapter_id"], 5678);
        assert_eq!(k["type"], "Original");
        assert_eq!(k["author"], "Shoot");
        assert_eq!(k["title"], "Pilot &amp; Co");
        assert_eq!(k["page"], 2);
        let colon = if cfg!(windows) { "_" } else { ":" };
        assert_eq!(
            path(&files[1]),
            format!(
                "hentai2read/Amazon Elixir/c001.5{colon} Pilot &amp; Co/Amazon Elixir_c001.5_002.png"
            )
        );
        assert_eq!(
            archive(&files[1]),
            r#"["{chapter_id}_{page}",["5678","2"]]"#
        );
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /amazon_elixir/1.5 "));
    }

    #[tokio::test]
    async fn chapter_without_title_match_has_empty_metadata() {
        let page = "<title>Other</title><script>{'images' : [\"/x.jpg\"],\n}</script>";
        let (root, requests) = serve(vec![html(page)]);
        let http = http("Hentai2Read", root);
        let files = media(extract(
            &http,
            Url::parse("https://hentai2read.com/x/2/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(kw(&files[0])["manga"], "");
        assert_eq!(kw(&files[0])["author"], "");
        requests.join().unwrap();
    }

    const MANGA_PAGE: &str = r##"<div itemscope itemtype="http://schema.org/Book">
<span itemprop="name">Amazon Elixir</span><small class="text-danger">[Original]</small>
<a data-mid="1234"></a>
<ul><li><div data-cid="9002"><a href="https://hentai2read.com/amazon_elixir/2/x">x</a>
<a href="https://hentai2read.com/amazon_elixir/2/">2 - Second &amp; Last</a></div></li>
<li><div data-cid="9001"><a href="#">x</a><a href="https://hentai2read.com/amazon_elixir/1/">1</a></div></li></ul>"##;

    #[test]
    fn parses_chapter_list() {
        let list = chapters(MANGA_PAGE);
        assert_eq!(list.len(), 2);
        assert_eq!(
            list[0].0.as_str(),
            "https://hentai2read.com/amazon_elixir/2/"
        );
        let k = &list[0].1;
        assert_eq!(k["manga"], "Amazon Elixir");
        assert_eq!(k["manga_id"], 1234);
        assert_eq!(k["type"], "Original");
        assert_eq!(k["chapter"], 2);
        assert_eq!(k["chapter_id"], 9002);
        assert_eq!(k["title"], "Second & Last");
        assert_eq!(list[1].1["chapter_id"], 9001);
        assert_eq!(list[1].1["title"], "");
    }

    #[tokio::test]
    async fn manga_queues_chapters_oldest_first() {
        let reader = |n: u32| html(&format!("<title>x</title>{{'images' : [\"/{n}.jpg\"],\n}}"));
        let (root, requests) = serve(vec![html(MANGA_PAGE), reader(1), reader(2)]);
        let http = http("Hentai2Read", root);
        let files = media(extract(
            &http,
            Url::parse("https://hentai2read.com/amazon_elixir/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /amazon_elixir "));
        assert!(requests[1].starts_with("GET /amazon_elixir/1 "));
        assert!(requests[2].starts_with("GET /amazon_elixir/2 "));
    }
}
