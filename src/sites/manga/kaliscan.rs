//! KaliScan chapters and manga, mirroring gallery_dl/extractor/kaliscan.py:
//! reader pages list their images in a `chapImages` script variable.
use super::{Gallery, Memo, keywords, set, text};
use crate::sites::art::{Scan, extr, extract_iter, split_html};
use crate::sites::{Events, Http, Route, Site, remove_html, unescape};
use crate::{Result, error};
use futures_util::StreamExt;
use serde_json::{Map, Value};
use url::Url;

pub static SITE: Site = Site {
    category: "kaliscan",
    name: "KaliScan",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&CHAPTER, &MANGA],
    api_key: false,
    cookies: None,
};

const ROOT: &str = "https://kaliscan.me";

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
    /// `/manga/SLUG/chapter-N` and the manga slug.
    Chapter { path: String, slug: String },
    /// The manga slug.
    Manga(String),
}

/// `[\w-]+` on a raw path segment.
fn slug(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

/// `kaliscan\.me(/manga/([\w-]+)/chapter-([\d.]+))` (unanchored) and
/// `kaliscan\.me(/manga/([\w-]+))/?$`.
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) || url.host_str() != Some("kaliscan.me") {
        return None;
    }
    let end = url.query().is_none() && url.fragment().is_none();
    let raw: Vec<&str> = url.path().split('/').skip(1).collect();
    match raw.as_slice() {
        ["manga", name, chapter, ..] if slug(name) && chapter.starts_with("chapter-") => {
            let number = chapter.strip_prefix("chapter-")?;
            let len = number
                .bytes()
                .take_while(|b| b.is_ascii_digit() || *b == b'.')
                .count();
            (len > 0).then(|| Target::Chapter {
                path: format!("/manga/{name}/chapter-{}", &number[..len]),
                slug: (*name).to_owned(),
            })
        }
        ["manga", name] | ["manga", name, ""] if slug(name) && end => {
            Some(Target::Manga((*name).to_owned()))
        }
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Chapter { path, slug }) => chapter_events(http, path, slug, Memo::default()),
        Some(Target::Manga(slug)) => {
            crate::sites::once(async move { manga_events(http, &slug).await })
        }
        None => crate::sites::fail(error("unsupported KaliScan URL")),
    }
}

async fn page(http: &Http, url: &str) -> Result<String> {
    http.text_with(&http.api(url)?, &http.browser_headers(&[]))
        .await
}

/// `KaliscanBase.manga_data`, memoized by slug like `Extractor.cache`.
async fn manga_data(
    http: &Http,
    slug: &str,
    page: Option<&str>,
    memo: &Memo,
) -> Result<Map<String, Value>> {
    if let Some(Value::Object(data)) = memo.get(slug) {
        return Ok(data);
    }
    let data = match page {
        Some(page) => parse_manga(slug, page),
        None => parse_manga(
            slug,
            &self::page(http, &format!("{ROOT}/manga/{slug}")).await?,
        ),
    };
    memo.insert(slug, Value::Object(data.clone()));
    Ok(data)
}

fn parse_manga(slug: &str, page: &str) -> Map<String, Value> {
    let mut e = Scan::new(page);
    let manga_id = text::parse_int(e.extr("bookId =", ";"));
    let title = unescape(e.extr("<h1>", "<"));
    let titles: Vec<String> = match e.extr("<h2>", "<") {
        "" => Vec::new(),
        alt => alt.split(',').map(|t| t.trim().to_owned()).collect(),
    };
    let author = remove_html(e.extr("Authors :</strong>", "</p>"));
    let status = remove_html(e.extr("Status :</strong>", "</p>"));
    let genres: Vec<String> = split_html(e.extr("Genres :</strong>", "</p>"))
        .iter()
        .map(|g| g.trim_matches([' ', ',']).to_owned())
        .collect();
    let description = match e.extr("class=\"content\"", "<div class=\"readmore\"") {
        "" => String::new(),
        descr => remove_html(&descr[descr.find('>').map_or(0, |i| i + 1)..])
            .trim()
            .to_owned(),
    };
    keywords([
        ("manga", title.into()),
        ("manga_id", manga_id.into()),
        ("manga_slug", slug.into()),
        ("manga_titles", titles.into()),
        ("author", author.into()),
        ("status", status.into()),
        ("genres", genres.into()),
        ("description", description.into()),
        ("lang", "en".into()),
        ("language", "English".into()),
    ])
}

/// `KaliscanChapterExtractor`: the reader page, then its manga's data.
fn chapter_events(http: &Http, path: String, slug: String, memo: Memo) -> Events<'_> {
    crate::sites::once(async move {
        let page = page(http, &format!("{ROOT}{path}")).await?;
        let mut e = Scan::new(&page);
        let manga_id = text::parse_int(e.extr("bookId =", ";"));
        e.extr("bookSlug =", ";");
        let chapter_id = text::parse_int(e.extr("chapterId =", ";"));
        e.extr("chapterSlug =", ";");
        let number = e
            .extr("chapterNumber =", ";")
            .trim_matches([' ', '"', '\'']);
        let (chapter, minor) = text::chapter(number);
        let mut data = manga_data(http, &slug, None, &memo).await?;
        set(&mut data, "chapter", chapter);
        set(&mut data, "chapter_minor", minor);
        set(&mut data, "chapter_id", chapter_id);
        if manga_id != 0 && data.get("manga_id").and_then(Value::as_i64) == Some(0) {
            set(&mut data, "manga_id", manga_id);
        }
        Ok(crate::sites::events(
            Gallery::chapter(&CHAPTER, data, images(&page))
                .id(chapter_id.to_string())
                .events(&SITE, http),
        ))
    })
}

/// `KaliscanChapterExtractor.images`: the comma-separated `chapImages`.
fn images(page: &str) -> Vec<super::Page> {
    extr(page, "var chapImages = \"", "\"")
        .split(',')
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .map(|url| (url.to_owned(), None))
        .collect()
}

/// `KaliscanMangaExtractor`: the manga page's chapter list, queued
/// newest-last unless `chapter-reverse`; children share the manga data.
async fn manga_events<'a>(http: &'a Http, slug: &str) -> Result<Events<'a>> {
    let page = page(http, &format!("{ROOT}/manga/{slug}")).await?;
    let memo = Memo::default();
    let data = manga_data(http, slug, Some(&page), &memo).await?;
    let mut chapters = chapters(&page, &data);
    // `MangaExtractor.reverse = True`.
    if !http.option_bool("chapter-reverse", false) {
        chapters.reverse();
    }
    let streams: Vec<Events<'a>> = chapters
        .into_iter()
        .filter_map(|(url, data)| match target(&Url::parse(&url).ok()?)? {
            Target::Chapter { path, slug } => {
                let memo = memo.clone();
                Some(super::queue_with(&SITE, &MANGA, &data, move || {
                    chapter_events(http, path, slug, memo)
                }))
            }
            Target::Manga(_) => None,
        })
        .collect();
    Ok(futures_util::stream::iter(streams).flatten().boxed())
}

/// `KaliscanMangaExtractor.chapters`, in page order.
fn chapters(page: &str, data: &Map<String, Value>) -> Vec<(String, Map<String, Value>)> {
    let list = extr(page, "id=\"chapter-list\">", "</ul>");
    extract_iter(list, "<li", "</li>")
        .into_iter()
        .filter_map(|li| {
            let url = extr(li, "href=\"", "\"");
            if url.is_empty() {
                return None;
            }
            let url = if url.starts_with('/') {
                format!("{ROOT}{url}")
            } else {
                url.to_owned()
            };
            let number = url
                .rsplit_once("/chapter-")
                .map_or(url.as_str(), |(_, n)| n);
            let (chapter, minor) = text::chapter(number);
            let mut chapter =
                keywords([("chapter", chapter.into()), ("chapter_minor", minor.into())]);
            crate::sites::art::update(&mut chapter, data);
            Some((url, chapter))
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
            t("https://kaliscan.me/manga/13209-sono-bisque/chapter-102.5?x=1"),
            Some(Target::Chapter {
                path: "/manga/13209-sono-bisque/chapter-102.5".into(),
                slug: "13209-sono-bisque".into(),
            })
        );
        assert_eq!(
            t("http://kaliscan.me/manga/x/chapter-7abc"),
            Some(Target::Chapter {
                path: "/manga/x/chapter-7".into(),
                slug: "x".into(),
            })
        );
        assert_eq!(
            t("https://kaliscan.me/manga/13209-sono-bisque/"),
            Some(Target::Manga("13209-sono-bisque".into()))
        );
        for raw in [
            "https://kaliscan.me/manga/x/chapter-",
            "https://kaliscan.me/manga/x/?page=2",
            "https://kaliscan.me/manga/a.b",
            "https://www.kaliscan.me/manga/x",
            "https://kaliscan.me.evil.test/manga/x",
            "https://kaliscan.me:8443/manga/x",
            "https://u:p@kaliscan.me/manga/x",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    const MANGA_PAGE: &str = r#"<script>var bookId = 13209;</script>
<h1>Sono Bisque &amp; Doll</h1><h2>My Dress-Up Darling, Sono Bisque Doll wa Koi wo Suru</h2>
<p><strong>Authors :</strong> <a>Fukuda Shinichi</a></p>
<p><strong>Status :</strong> <span>Completed</span></p>
<p><strong>Genres :</strong> <a>Comedy</a>, <a>Romance</a> ,</p>
<div class="summary"><div class="content" style="x"><p>Gojo and Marin.</p></div><div class="readmore">
<ul class="chapter-list" id="chapter-list">
<li><a href="/manga/13209-sono-bisque/chapter-2.5">2.5</a></li>
<li><a title="none"></a></li>
<li><a href="https://kaliscan.me/manga/13209-sono-bisque/chapter-1">1</a></li>
</ul>"#;

    #[tokio::test]
    async fn extracts_chapter_with_manga_data() {
        let reader = r#"<script>var bookId = 0; var bookSlug = "x";
var chapterId = 4455; var chapterSlug = "chapter-2.5"; var chapterNumber = "2.5";
var chapImages = "https://s1.kaliscan.me/a/01.jpg, https://s1.kaliscan.me/a/02.webp,";</script>"#;
        let (root, requests) = serve(vec![html(reader), html(MANGA_PAGE)]);
        let http = http("KaliScan", root);
        let files = media(extract(
            &http,
            Url::parse("https://kaliscan.me/manga/13209-sono-bisque/chapter-2.5").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(files[1].url.as_str(), "https://s1.kaliscan.me/a/02.webp");
        let k = kw(&files[1]);
        assert_eq!(k["manga"], "Sono Bisque & Doll");
        assert_eq!(k["manga_id"], 13209);
        assert_eq!(k["manga_slug"], "13209-sono-bisque");
        assert_eq!(
            k["manga_titles"],
            serde_json::json!(["My Dress-Up Darling", "Sono Bisque Doll wa Koi wo Suru"])
        );
        assert_eq!(k["author"], "Fukuda Shinichi");
        assert_eq!(k["status"], "Completed");
        assert_eq!(
            k["genres"],
            serde_json::json!(["Comedy", "", "Romance", ""])
        );
        assert_eq!(k["description"], "Gojo and Marin.");
        assert_eq!(k["chapter"], 2);
        assert_eq!(k["chapter_minor"], ".5");
        assert_eq!(k["chapter_id"], 4455);
        assert_eq!(k["count"], 2);
        assert_eq!(k["page"], 2);
        assert_eq!(
            path(&files[1]),
            "kaliscan/Sono Bisque & Doll/c002.5/Sono Bisque & Doll_c002.5_002.webp"
        );
        assert_eq!(
            archive(&files[1]),
            r#"["{manga}_{chapter}{chapter_minor}_{page}",["Sono Bisque & Doll","2",".5","2"]]"#
        );
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /manga/13209-sono-bisque/chapter-2.5 "));
        assert!(requests[1].starts_with("GET /manga/13209-sono-bisque "));
    }

    #[test]
    fn parses_chapter_list() {
        let data = parse_manga("s", MANGA_PAGE);
        let chapters = chapters(MANGA_PAGE, &data);
        assert_eq!(chapters.len(), 2);
        assert_eq!(
            chapters[0].0,
            "https://kaliscan.me/manga/13209-sono-bisque/chapter-2.5"
        );
        assert_eq!(chapters[0].1["chapter"], 2);
        assert_eq!(chapters[0].1["chapter_minor"], ".5");
        assert_eq!(chapters[0].1["manga_id"], 13209);
        assert_eq!(chapters[1].1["chapter"], 1);
        assert_eq!(chapters[1].1["chapter_minor"], "");
    }

    #[tokio::test]
    async fn manga_queues_chapters_oldest_first_sharing_manga_data() {
        let (root, requests) = serve(vec![html(MANGA_PAGE), html(""), html("")]);
        let http = http("KaliScan", root);
        let files = media(extract(
            &http,
            Url::parse("https://kaliscan.me/manga/13209-sono-bisque").unwrap(),
        ))
        .await;
        assert!(files.is_empty());
        let requests = requests.join().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[1].starts_with("GET /manga/13209-sono-bisque/chapter-1 "));
        assert!(requests[2].starts_with("GET /manga/13209-sono-bisque/chapter-2.5 "));
    }

    #[tokio::test]
    async fn manga_children_are_chapter_jobs() {
        let (root, requests) = serve(vec![html(MANGA_PAGE)]);
        let http = http("KaliScan", root);
        let children = queued(extract(
            &http,
            Url::parse("https://kaliscan.me/manga/13209-sono-bisque/").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].subcategory(), "manga");
        requests.join().unwrap();
    }
}
