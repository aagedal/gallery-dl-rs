//! KaliScan chapters and manga, mirroring gallery_dl/extractor/kaliscan.py.
use super::{Gallery, chapter_keys, english, fetch, set};
use crate::error;
use crate::sites::gdl::{Scan, extr, extract_iter, parse_int, remove_html, split_html, unescape};
use crate::sites::{Events, Http, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://kaliscan.me";

static CHAPTER: Route = super::chapter_route();
static MANGA: Route = super::manga_route();

pub static SITE: Site = Site {
    category: "kaliscan",
    name: "KaliScan",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&CHAPTER, &MANGA],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Chapter { slug: String, chapter: String },
    Manga(String),
}

/// `/manga/([\w-]+)/chapter-([\d.]+)` or `/manga/([\w-]+)/?$`.
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::gdl::on(url, &["kaliscan.me"], false) {
        return None;
    }
    let parts = crate::sites::gdl::path_parts(url);
    match parts.as_slice() {
        ["manga", slug] | ["manga", slug, ""]
            if crate::sites::gdl::word(slug) && url.query().is_none() =>
        {
            Some(Target::Manga((*slug).into()))
        }
        ["manga", slug, chapter, ..] if crate::sites::gdl::word(slug) => {
            let number = chapter.strip_prefix("chapter-")?;
            let len = number
                .find(|c: char| !(c.is_ascii_digit() || c == '.'))
                .unwrap_or(number.len());
            (len > 0).then(|| Target::Chapter {
                slug: (*slug).into(),
                chapter: number[..len].into(),
            })
        }
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported KaliScan URL"));
    };
    crate::sites::once(async move {
        Ok(match target {
            Target::Chapter { slug, chapter } => {
                let page = fetch(http, &format!("{ROOT}/manga/{slug}/chapter-{chapter}")).await?;
                let manga = fetch(http, &format!("{ROOT}/manga/{slug}")).await?;
                let data = chapter_metadata(&page, manga_data(&manga, &slug));
                crate::sites::events(
                    Gallery::chapter(&CHAPTER, data, images(&page)).events(&SITE, http),
                )
            }
            Target::Manga(slug) => {
                let page = fetch(http, &format!("{ROOT}/manga/{slug}")).await?;
                let data = manga_data(&page, &slug);
                super::queue_chapters(http, &SITE, &MANGA, chapters(&page, &data), true)
            }
        })
    })
}

/// `KaliscanBase.manga_data`.
fn manga_data(page: &str, slug: &str) -> Map<String, Value> {
    let mut scan = Scan::new(page);
    let manga_id = parse_int(scan.extr("bookId =", ";"));
    let title = unescape(scan.extr("<h1>", "<"));
    let alt_titles: Vec<Value> = match scan.extr("<h2>", "<") {
        "" => Vec::new(),
        alt => alt.split(',').map(|t| t.trim().into()).collect(),
    };
    let author = remove_html(scan.extr("Authors :</strong>", "</p>"));
    let status = remove_html(scan.extr("Status :</strong>", "</p>"));
    let genres: Vec<Value> = split_html(scan.extr("Genres :</strong>", "</p>"))
        .iter()
        .map(|g| g.trim_matches([' ', ',']).into())
        .collect();
    let description = match scan.extr("class=\"content\"", "<div class=\"readmore\"") {
        "" => String::new(),
        descr => remove_html(&descr[descr.find('>').map_or(0, |i| i + 1)..])
            .trim()
            .to_owned(),
    };
    let mut data = Map::new();
    set(&mut data, "manga", title);
    set(&mut data, "manga_id", manga_id);
    set(&mut data, "manga_slug", slug);
    set(&mut data, "manga_titles", alt_titles);
    set(&mut data, "author", author);
    set(&mut data, "status", status);
    set(&mut data, "genres", genres);
    set(&mut data, "description", description);
    english(&mut data);
    data
}

fn chapter_metadata(page: &str, mut data: Map<String, Value>) -> Map<String, Value> {
    let mut scan = Scan::new(page);
    let manga_id = parse_int(scan.extr("bookId =", ";"));
    scan.extr("bookSlug =", ";");
    let chapter_id = parse_int(scan.extr("chapterId =", ";"));
    scan.extr("chapterSlug =", ";");
    let number = scan
        .extr("chapterNumber =", ";")
        .trim_matches([' ', '"', '\'']);
    chapter_keys(&mut data, number);
    set(&mut data, "chapter_id", chapter_id);
    if manga_id != 0 && data.get("manga_id").and_then(Value::as_i64) == Some(0) {
        set(&mut data, "manga_id", manga_id);
    }
    data
}

fn images(page: &str) -> Vec<super::Page> {
    extr(page, "var chapImages = \"", "\"")
        .split(',')
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .map(|url| (url.to_owned(), None))
        .collect()
}

fn chapters(page: &str, data: &Map<String, Value>) -> Vec<(Url, Map<String, Value>)> {
    let list = extr(page, "id=\"chapter-list\">", "</ul>");
    extract_iter(list, "<li", "</li>")
        .into_iter()
        .filter_map(|li| {
            let href = extr(li, "href=\"", "\"");
            if href.is_empty() {
                return None;
            }
            let url = if href.starts_with('/') {
                format!("{ROOT}{href}")
            } else {
                href.to_owned()
            };
            let number = crate::sites::gdl::rpartition(&url, "/chapter-").2;
            let mut chapter = Map::new();
            chapter_keys(&mut chapter, number);
            chapter.extend(data.clone());
            Some((Url::parse(&url).ok()?, chapter))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::tests::path;
    use super::*;
    use crate::sites::testing::{children, html, http, keywords, media, serve};

    const MANGA_PAGE: &str = r#"<script>var bookId = 2142;</script>
        <h1>What&#39;s Wrong</h1><h2>Kim Biseo, Secretary Kim</h2>
        <p><strong>Authors :</strong> <a>Jeong Gyeong-yun</a></p>
        <p><strong>Status :</strong> <span>Completed</span></p>
        <p><strong>Genres :</strong> <a>Comedy</a> , <a>Romance</a></p>
        <div class="content"><p>About it.</p></div><div class="readmore">
        <ul id="chapter-list"><li><a href="/manga/2142-w/chapter-14.5">14.5</a></li>
        <li><a href="/manga/2142-w/chapter-1">1</a></li></ul>"#;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://kaliscan.me/manga/2142-whats-wrong-with-secretary-kim/chapter-14.5"),
            Some(Target::Chapter {
                slug: "2142-whats-wrong-with-secretary-kim".into(),
                chapter: "14.5".into()
            })
        );
        assert_eq!(
            t("https://kaliscan.me/manga/2142-x/"),
            Some(Target::Manga("2142-x".into()))
        );
        assert_eq!(t("https://kaliscan.me/manga/2142-x?page=2"), None);
        assert_eq!(t("https://kaliscan.me/manga/2142-x/reviews"), None);
    }

    #[tokio::test]
    async fn extracts_chapter_with_manga_data() {
        let chapter = r#"var bookId = 2142; var bookSlug = "w"; var chapterId = 77;
            var chapterSlug = "c"; var chapterNumber = "14.5";
            var chapImages = "https://cdn.kaliscan.me/a.jpg, https://cdn.kaliscan.me/b.jpg,""#;
        let (root, server) = serve(vec![html(chapter), html(MANGA_PAGE)]);
        let http = http("kaliscan", root);
        let files = media(extract(
            &http,
            Url::parse("https://kaliscan.me/manga/2142-w/chapter-14.5").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[1]);
        assert_eq!(k["manga"], "What's Wrong");
        assert_eq!(
            k["manga_titles"],
            serde_json::json!(["Kim Biseo", "Secretary Kim"])
        );
        assert_eq!(k["genres"], serde_json::json!(["Comedy", "", "Romance"]));
        assert_eq!(k["author"], "Jeong Gyeong-yun");
        assert_eq!(k["status"], "Completed");
        assert_eq!(k["description"], "About it.");
        assert_eq!(k["chapter"], 14);
        assert_eq!(k["chapter_minor"], ".5");
        assert_eq!(k["chapter_id"], 77);
        assert_eq!(
            path(&files[1]),
            "kaliscan/What's Wrong/c014.5/What's Wrong_c014.5_002.jpg"
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /manga/2142-w/chapter-14.5 "));
        assert!(requests[1].starts_with("GET /manga/2142-w "));
    }

    #[tokio::test]
    async fn manga_queues_chapters() {
        let data = manga_data(MANGA_PAGE, "2142-w");
        let list = chapters(MANGA_PAGE, &data);
        assert_eq!(
            list[0].0.as_str(),
            "https://kaliscan.me/manga/2142-w/chapter-14.5"
        );
        assert_eq!(list[0].1["chapter_minor"], ".5");
        assert_eq!(list[1].1["manga_id"], 2142);
        let (root, server) = serve(vec![html(MANGA_PAGE)]);
        let http = http("kaliscan", root);
        let queued = children(extract(
            &http,
            Url::parse("https://kaliscan.me/manga/2142-w").unwrap(),
        ))
        .await;
        assert_eq!(queued.len(), 2);
        server.join().unwrap();
    }
}
