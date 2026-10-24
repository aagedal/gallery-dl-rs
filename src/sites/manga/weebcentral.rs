//! Weeb Central chapters and manga, mirroring
//! gallery_dl/extractor/weebcentral.py: htmx fragments list a series'
//! chapters and a chapter's images.
//!
//! Not ported: the upstream 0.5-1.5 s `request_interval`.
use super::{Gallery, Memo, keywords, text};
use crate::sites::art::{Scan, split_html};
use crate::sites::{Events, Http, Route, Site, remove_html, unescape};
use crate::{Result, error};
use futures_util::StreamExt;
use serde_json::{Map, Value};
use url::Url;

pub static SITE: Site = Site {
    category: "weebcentral",
    name: "Weeb Central",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&CHAPTER, &MANGA],
    api_key: false,
    cookies: None,
};

const ROOT: &str = "https://weebcentral.com";
const REFERER: &str = "https://weebcentral.com/";
const FILENAME: &str = "{manga}_c{chapter:>03}{chapter_minor:?//}_{page:>03}.{extension}";
const DIRECTORY: &[&str] = &[
    "{category}",
    "{manga}",
    "{volume:?v/ />02}c{chapter:>03}{chapter_minor:?//}{title:?: //}",
];
const ARCHIVE: &str = "{manga}_{chapter}{chapter_minor}_{page}";

static CHAPTER: Route = Route::new("chapter", FILENAME, DIRECTORY, ARCHIVE);
static MANGA: Route = Route::new("manga", "{filename}.{extension}", &["{category}"], "");

#[derive(Debug, PartialEq)]
enum Target {
    Chapter(String),
    Manga(String),
}

/// A `\w+` prefix of a path segment.
fn word(segment: &str) -> Option<&str> {
    let end = segment
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(segment.len());
    (1..=64).contains(&end).then(|| &segment[..end])
}

/// `weebcentral\.com(/chapters/(\w+))` and `/series/(\w+)`.
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::art::on_host(url, &["weebcentral.com"]) {
        return None;
    }
    let raw: Vec<&str> = url.path().split('/').skip(1).collect();
    match raw.as_slice() {
        ["chapters", id, ..] => word(id).map(|id| Target::Chapter(id.to_owned())),
        ["series", id, ..] => word(id).map(|id| Target::Manga(id.to_owned())),
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Chapter(id)) => chapter_events(http, id, Memo::default()),
        Some(Target::Manga(id)) => crate::sites::once(async move { manga_events(http, id).await }),
        None => crate::sites::fail(error("unsupported Weeb Central URL")),
    }
}

async fn page(http: &Http, url: Url, headers: &[(&str, &str)]) -> Result<String> {
    let mut all = headers.to_vec();
    if !all.iter().any(|(key, _)| *key == "Referer") {
        all.push(("Referer", REFERER));
    }
    http.text_with(&url, &http.browser_headers(&all)).await
}

/// `_manga_info`, memoized by manga ID.
async fn manga_info(http: &Http, id: &str, memo: &Memo) -> Result<Map<String, Value>> {
    if let Some(Value::Object(info)) = memo.get(id) {
        return Ok(info);
    }
    let page = page(http, http.api(&format!("{ROOT}/series/{id}"))?, &[]).await?;
    let info = parse_manga_info(id, &page);
    memo.insert(id, Value::Object(info.clone()));
    Ok(info)
}

fn parse_manga_info(id: &str, page: &str) -> Map<String, Value> {
    let odd = |html: &str| -> Value {
        split_html(html)
            .into_iter()
            .skip(1)
            .step_by(2)
            .collect::<Vec<_>>()
            .into()
    };
    let mut e = Scan::new(page);
    let manga = unescape(e.extr("<title>", " | Weeb Central"));
    let author = odd(e.extr("<strong>Author", "</li>"));
    let tags = odd(e.extr("<strong>Tag", "</li>"));
    let kind = remove_html(e.extr("<strong>Type: ", "</li>"));
    let status = remove_html(e.extr("<strong>Status: ", "</li>"));
    let release = remove_html(e.extr("<strong>Released: ", "</li>"));
    let official = e
        .extr("<strong>Official Translatio", "</li>")
        .contains(">Yes");
    let description = unescape(&remove_html(e.extr("<strong>Description", "</li>")));
    keywords([
        ("manga_id", id.into()),
        ("lang", "en".into()),
        ("language", "English".into()),
        ("manga", manga.into()),
        ("author", author),
        ("tags", tags),
        ("type", kind.into()),
        ("status", status.into()),
        ("release", release.into()),
        ("official", official.into()),
        ("description", description.into()),
    ])
}

/// `WeebcentralChapterExtractor`: the chapter page, its manga's info, then
/// the long-strip image fragment.
fn chapter_events(http: &Http, id: String, memo: Memo) -> Events<'_> {
    crate::sites::once(async move {
        let referer = format!("{ROOT}/chapters/{id}");
        let chapter = page(http, http.api(&referer)?, &[]).await?;
        let mut e = Scan::new(&chapter);
        let manga_id = e.extr("'series_id': '", "'").to_owned();
        let chapter_type = e.extr("'chapter_type': '", "'");
        let (number, minor) = text::chapter(e.extr("'number': '", "'"));
        if word(&manga_id) != Some(manga_id.as_str()) {
            return Err(error("Weeb Central chapter has no series"));
        }
        let mut data = manga_info(http, &manga_id, &memo).await?;
        data.extend(keywords([
            ("chapter", number.into()),
            ("chapter_id", id.clone().into()),
            ("chapter_type", chapter_type.into()),
            ("chapter_minor", minor.into()),
        ]));
        let mut url = http.api(&format!("{referer}/images"))?;
        url.query_pairs_mut()
            .append_pair("is_prev", "False")
            .append_pair("current_page", "1")
            .append_pair("reading_style", "long_strip");
        let images = page(
            http,
            url,
            &[
                ("Accept", "*/*"),
                ("Referer", &referer),
                ("HX-Request", "true"),
                ("HX-Current-URL", &referer),
            ],
        )
        .await?;
        Ok(crate::sites::events(
            Gallery::chapter(&CHAPTER, data, pages(&images))
                .id(id)
                .header("Referer", REFERER)
                .events(&SITE, http),
        ))
    })
}

/// `WeebcentralChapterExtractor.images`: each `src` with its size.
fn pages(fragment: &str) -> Vec<super::Page> {
    let mut e = Scan::new(fragment);
    let mut pages = Vec::new();
    while let Some(src) = e.find("src=\"", "\"").filter(|src| !src.is_empty()) {
        let width = text::parse_int(e.extr("width=\"", "\""));
        let height = text::parse_int(e.extr("height=\"", "\""));
        let size = keywords([("width", width.into()), ("height", height.into())]);
        pages.push((src.to_owned(), Some(size)));
    }
    pages
}

/// `WeebcentralMangaExtractor.chapters`: the full chapter list fragment,
/// queued newest-last unless `chapter-reverse`.
async fn manga_events<'a>(http: &'a Http, id: String) -> Result<Events<'a>> {
    let referer = format!("{ROOT}/series/{id}");
    let list = page(
        http,
        http.api(&format!("{referer}/full-chapter-list"))?,
        &[
            ("Accept", "*/*"),
            ("Referer", &referer),
            ("HX-Request", "true"),
            ("HX-Target", "chapter-list"),
            ("HX-Current-URL", &referer),
        ],
    )
    .await?;
    let memo = Memo::default();
    let info = manga_info(http, &id, &memo).await?;
    let mut chapters = chapter_list(&list, &info);
    // `MangaExtractor.reverse = True`.
    if !http.option_bool("chapter-reverse", false) {
        chapters.reverse();
    }
    let streams: Vec<Events<'a>> = chapters
        .into_iter()
        .filter(|(chapter_id, _)| word(chapter_id) == Some(chapter_id.as_str()))
        .map(|(chapter_id, data)| {
            let memo = memo.clone();
            super::queue_with(&SITE, &MANGA, &data, move || {
                chapter_events(http, chapter_id, memo)
            })
        })
        .collect();
    Ok(futures_util::stream::iter(streams).flatten().boxed())
}

/// Each chapter's ID and keywords, in list order.
fn chapter_list(list: &str, info: &Map<String, Value>) -> Vec<(String, Map<String, Value>)> {
    let mut chapters = Vec::new();
    let mut e = Scan::new(list);
    while let Some(chapter_id) = e.find("/chapters/", "\"").filter(|id| !id.is_empty()) {
        let (kind, _, number) = text::partition(e.extr("<span class=\"\">", "<"), " ");
        let (number, minor) = text::chapter(number);
        let datetime = e.extr(" datetime=\"", "\"");
        let datetime = datetime
            .char_indices()
            .nth_back(4)
            .map_or("", |(end, _)| &datetime[..end]);
        let mut data = keywords([
            ("chapter_id", chapter_id.into()),
            ("chapter", number.into()),
            ("chapter_minor", minor.into()),
            ("chapter_type", kind.into()),
            ("date", text::date(text::iso(datetime))),
        ]);
        data.extend(info.clone());
        chapters.push((chapter_id.to_owned(), data));
    }
    chapters
}

#[cfg(test)]
mod tests {
    use super::super::tests::{archive, path};
    use super::*;
    use crate::sites::testing::{html, http, keywords as kw, media, serve};
    use serde_json::json;

    const CHAPTER_ID: &str = "01J76XZ4PC3VW91BYFBQJA44C3";
    const MANGA_ID: &str = "01J76XY8G1GK8EJ9VQG92C3DKM";

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t(&format!("https://weebcentral.com/chapters/{CHAPTER_ID}")),
            Some(Target::Chapter(CHAPTER_ID.into()))
        );
        assert_eq!(
            t(&format!(
                "https://www.weebcentral.com/series/{MANGA_ID}/Aria"
            )),
            Some(Target::Manga(MANGA_ID.into()))
        );
        for raw in [
            "https://weebcentral.com/series/",
            "https://weebcentral.com/chapters/-x",
            "https://weebcentral.com/search?q=x",
            "https://weebcentral.com.evil.test/series/A",
            "https://cdn.weebcentral.com/series/A",
            "https://weebcentral.com:8443/series/A",
            "https://u:p@weebcentral.com/series/A",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    const SERIES: &str = r#"<title>Aria &amp; Co | Weeb Central</title>
<li><strong>Author(s): </strong> <span><a href="/a">AMANO Kozue</a></span></li>
<li><strong>Tags(s): </strong> <span><a>Adventure</a>,</span> <span><a>Comedy</a></span></li>
<li><strong>Type: </strong> <a>Manga</a></li>
<li><strong>Status: </strong> <a>Complete</a></li>
<li><strong>Released: </strong> <span>2002</span></li>
<li><strong>Official Translation: </strong> <a>Yes</a></li>
<li><strong>Description</strong><p>On the planet Aqua &amp; more.</p></li>"#;

    fn chapter_page() -> String {
        format!(
            "<script>{{'series_id': '{MANGA_ID}', 'chapter_type': 'Navigation', 'number': '67.5'}}</script>"
        )
    }

    const IMAGES: &str = r#"<img src="https://official.lowee.us/manga/Aria/0067.5-001.png" width="1129" height="1600" alt="">
<img src="https://official.lowee.us/manga/Aria/0067.5-002.png" width="2133" height="1511" alt="">"#;

    #[tokio::test]
    async fn extracts_chapter() {
        let (root, requests) = serve(vec![html(&chapter_page()), html(SERIES), html(IMAGES)]);
        let http = http("Weeb Central", root);
        let files = media(extract(
            &http,
            Url::parse(&format!("https://weebcentral.com/chapters/{CHAPTER_ID}")).unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].headers["Referer"], "https://weebcentral.com/");
        let k = kw(&files[1]);
        assert_eq!(k["manga"], "Aria & Co");
        assert_eq!(k["manga_id"], MANGA_ID);
        assert_eq!(k["author"], json!(["AMANO Kozue"]));
        assert_eq!(k["tags"], json!(["Adventure", "Comedy"]));
        assert_eq!(k["type"], "Manga");
        assert_eq!(k["status"], "Complete");
        assert_eq!(k["release"], "2002");
        assert_eq!(k["official"], true);
        assert_eq!(k["description"], "On the planet Aqua & more.");
        assert_eq!(k["chapter"], 67);
        assert_eq!(k["chapter_minor"], ".5");
        assert_eq!(k["chapter_type"], "Navigation");
        assert_eq!(k["chapter_id"], CHAPTER_ID);
        assert_eq!(k["width"], 2133);
        assert_eq!(k["height"], 1511);
        assert_eq!(k["page"], 2);
        assert_eq!(k["count"], 2);
        assert_eq!(k["filename"], "0067.5-002");
        assert_eq!(
            path(&files[1]),
            "weebcentral/Aria & Co/c067.5/Aria & Co_c067.5_002.png"
        );
        assert_eq!(
            archive(&files[1]),
            r#"["{manga}_{chapter}{chapter_minor}_{page}",["Aria & Co","67",".5","2"]]"#
        );
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with(&format!("GET /chapters/{CHAPTER_ID} ")));
        assert!(requests[1].starts_with(&format!("GET /series/{MANGA_ID} ")));
        assert!(requests[2].starts_with(&format!(
            "GET /chapters/{CHAPTER_ID}/images?is_prev=False&current_page=1&reading_style=long_strip "
        )));
        assert!(requests[2].contains("hx-request: true"));
        assert!(requests[2].contains(&format!(
            "hx-current-url: https://weebcentral.com/chapters/{CHAPTER_ID}"
        )));
    }

    const LIST: &str = r#"<a href="https://weebcentral.com/chapters/01JB">
<span class="">Chapter 2</span><time datetime="2024-09-06T21:43:47.380Z"></time></a>
<a href="https://weebcentral.com/chapters/01JA">
<span class="">Navigation 1.5</span><time datetime="2024-09-05T21:43:47.380Z"></time></a>"#;

    #[tokio::test]
    async fn manga_queues_chapters_oldest_first_with_one_series_lookup() {
        let list = LIST;
        let (root, requests) = serve(vec![
            html(list),
            html(SERIES),
            html(&chapter_page()),
            html(IMAGES),
            html(&chapter_page()),
            html(IMAGES),
        ]);
        let http = http("Weeb Central", root);
        let files = media(extract(
            &http,
            Url::parse(&format!("https://weebcentral.com/series/{MANGA_ID}/Aria")).unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 4);
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with(&format!("GET /series/{MANGA_ID}/full-chapter-list ")));
        assert!(requests[0].contains("hx-target: chapter-list"));
        assert!(requests[1].starts_with(&format!("GET /series/{MANGA_ID} ")));
        assert!(requests[2].starts_with("GET /chapters/01JA "));
        assert!(requests[3].starts_with("GET /chapters/01JA/images?"));
        assert!(requests[4].starts_with("GET /chapters/01JB "));
        assert!(requests[5].starts_with("GET /chapters/01JB/images?"));
    }

    #[test]
    fn chapter_list_keywords() {
        let info = parse_manga_info(MANGA_ID, SERIES);
        let chapters = chapter_list(LIST, &info);
        assert_eq!(chapters.len(), 2);
        let (id, data) = &chapters[1];
        assert_eq!(id, "01JA");
        assert_eq!(data["chapter_id"], "01JA");
        assert_eq!(data["chapter"], 1);
        assert_eq!(data["chapter_minor"], ".5");
        assert_eq!(data["chapter_type"], "Navigation");
        assert_eq!(data["date"], "2024-09-05 21:43:47");
        assert_eq!(data["manga"], "Aria & Co");
        assert_eq!(data["official"], true);
    }
}
