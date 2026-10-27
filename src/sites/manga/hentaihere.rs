//! HentaiHere chapters and manga, mirroring gallery_dl/extractor/hentaihere.py.
use super::{Gallery, english, set};
use crate::error;
use crate::sites::gdl::{Scan, captures, extr, group, parse_int, partition, re, unescape};
use crate::sites::{Events, Http, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://hentaihere.com";

static CHAPTER: Route = Route::new(
    "chapter",
    super::CHAPTER_FILENAME,
    super::CHAPTER_DIRECTORY,
    "{chapter_id}_{page}",
);
static MANGA: Route = super::manga_route();

pub static SITE: Site = Site {
    category: "hentaihere",
    name: "HentaiHere",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&CHAPTER, &MANGA],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Chapter(String, String),
    Manga(String),
    /// Newer `/reader/MANGA-slug/CHAPTER_ID` chapter pages (path).
    Reader(String, String),
}

fn target(url: &Url) -> Option<Target> {
    let chapter = re!(r"^https?://(?:www\.)?hentaihere\.com/m/S(\d+)/([^/?#]+)");
    if let Some(caps) = captures(chapter, url) {
        return Some(Target::Chapter(
            group(&caps, 1).into(),
            group(&caps, 2).into(),
        ));
    }
    let reader = re!(r"^https?://(?:www\.)?hentaihere\.com(/reader/(\d+)-[^/?#]+/\d+)");
    if let Some(caps) = captures(reader, url) {
        return Some(Target::Reader(
            group(&caps, 1).into(),
            group(&caps, 2).into(),
        ));
    }
    // `/m/S123` now redirects to `/<type>/123-slug`.
    let manga = re!(r"^https?://(?:www\.)?hentaihere\.com(/m/S\d+|/[a-z-]+/\d+-[^/?#]+)/?$");
    captures(manga, url).map(|caps| Target::Manga(group(&caps, 1).into()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported HentaiHere URL"));
    };
    crate::sites::once(async move {
        Ok(match target {
            Target::Chapter(manga_id, chapter) => {
                let page = super::fetch(http, &format!("{ROOT}/m/S{manga_id}/{chapter}/1")).await?;
                let mut data = chapter_metadata(&page, &manga_id, &chapter)?;
                let pages = match serde_json::from_str::<Vec<String>>(extr(
                    &page,
                    "var rff_imageList = ",
                    ";",
                )) {
                    Ok(images) => images
                        .into_iter()
                        .map(|part| (format!("https://hentaicdn.com/hentai{part}"), None))
                        .collect(),
                    // The newer `/reader/` pages embed absolute URLs.
                    Err(_) => reader_images(&page, &mut data)
                        .ok_or_else(|| error("HentaiHere chapter has no image list"))?,
                };
                crate::sites::events(Gallery::chapter(&CHAPTER, data, pages).events(&SITE, http))
            }
            Target::Reader(path, manga_id) => {
                let page = super::fetch(http, &format!("{ROOT}{path}/page-1")).await?;
                let mut data = Map::new();
                set(&mut data, "manga_id", parse_int(&manga_id));
                let pages = reader_images(&page, &mut data)
                    .ok_or_else(|| error("HentaiHere chapter has no image list"))?;
                english(&mut data);
                crate::sites::events(Gallery::chapter(&CHAPTER, data, pages).events(&SITE, http))
            }
            Target::Manga(path) => {
                let page = super::fetch(http, &format!("{ROOT}{path}")).await?;
                let last = path
                    .trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .unwrap_or_default();
                let manga_id = parse_int(last.strip_prefix('S').unwrap_or(last));
                let mut list = chapters(&page, manga_id);
                if list.is_empty() {
                    list = reader_chapters(&page, manga_id);
                }
                super::queue_chapters(http, &SITE, &MANGA, list, true)
            }
        })
    })
}

/// One page URL with optional per-page keywords.
type Page = (String, Option<Map<String, Value>>);

/// Pages and metadata of the newer reader's `data-reader-data` JSON.
fn reader_images(page: &str, data: &mut Map<String, Value>) -> Option<Vec<Page>> {
    let reader: Value = serde_json::from_str(extr(page, "data-reader-data>", "</script>")).ok()?;
    let images: Vec<Page> = reader["images"]
        .as_array()?
        .iter()
        .filter_map(|url| Some((url.as_str()?.to_owned(), None)))
        .collect();
    let book = &reader["book"];
    if data.get("chapter").is_none() {
        let label = book["chapter"].as_str().unwrap_or_default();
        let number = label.strip_prefix("Chapter ").unwrap_or(label);
        let (major, sep, minor) = partition(number, ".");
        set(data, "chapter", parse_int(major));
        set(data, "chapter_minor", format!("{sep}{minor}"));
    }
    if data
        .get("manga")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        set(data, "manga", book["title"].as_str().unwrap_or_default());
    }
    let chapter_url = reader["chapterUrl"].as_str().unwrap_or_default();
    if let Some(id) = chapter_url.rsplit('/').next().filter(|id| !id.is_empty()) {
        set(data, "chapter_id", parse_int(id));
    }
    // "Reading MANGA by AUTHOR Chapter N Online - Page {page} - HentaiHere"
    let title = reader["title"].as_str().unwrap_or_default();
    if let Some(caps) = re!(r"^Reading .+ by (.+?) Chapter ").captures(title) {
        set(data, "author", group(&caps, 1));
    }
    // `<h1 class="sr-only">MANGA - Chapter N: TITLE</h1>`
    let heading = unescape(extr(page, "<h1 class=\"sr-only\">", "</h1>"));
    if let Some((_, title)) = heading.split_once(": ") {
        set(data, "title", title);
    }
    Some(images)
}

fn chapter_metadata(
    page: &str,
    manga_id: &str,
    chapter: &str,
) -> crate::Result<Map<String, Value>> {
    let title = extr(page, "<title>", "</title>");
    let chapter_id = extr(page, "report/C", "\"");
    let (number, sep, minor) = partition(chapter, ".");
    let pattern = re!(r"^Page 1 \| (.+) \(([^)]+)\) - Chapter \d+: (.+) by (.+) at ");
    // Upstream fails on titles it cannot parse; keep the pages instead.
    let caps = pattern.captures(title);
    let field = |i| caps.as_ref().map_or("", |c| group(c, i));
    let mut data = Map::new();
    set(&mut data, "manga", field(1));
    set(&mut data, "manga_id", parse_int(manga_id));
    set(&mut data, "chapter", parse_int(number));
    set(&mut data, "chapter_minor", format!("{sep}{minor}"));
    set(&mut data, "chapter_id", parse_int(chapter_id));
    set(&mut data, "type", field(2));
    set(&mut data, "title", field(3));
    set(&mut data, "author", field(4));
    english(&mut data);
    Ok(data)
}

/// The newer manga page's chapter `<select>`: `<option value="ID"
/// data-read="URL" …>Chapter N - TITLE</option>`.
fn reader_chapters(page: &str, manga_id: i64) -> Vec<(Url, Map<String, Value>)> {
    let manga = unescape(extr(page, "\"@type\":\"Book\",\"name\":\"", "\""));
    let mut results = Vec::new();
    let mut scan = Scan::new(page);
    while let Some(option) = scan.find("<option value=\"", "</option>") {
        let read = extr(option, "data-read=\"", "\"");
        let Ok(url) = Url::parse(read) else { continue };
        let chapter_id = option.split('"').next().unwrap_or_default();
        let label = unescape(option.rsplit('>').next().unwrap_or_default());
        let (chapter, _, title) = partition(label.trim(), " - ");
        let number = chapter.strip_prefix("Chapter ").unwrap_or(chapter);
        let (major, sep, minor) = partition(number, ".");
        let mut data = Map::new();
        set(&mut data, "manga_id", manga_id);
        set(&mut data, "manga", manga.as_str());
        set(&mut data, "chapter", parse_int(major));
        set(&mut data, "chapter_minor", format!("{sep}{minor}"));
        set(&mut data, "chapter_id", parse_int(chapter_id));
        set(&mut data, "title", title);
        english(&mut data);
        results.push((url, data));
    }
    results
}

fn chapters(page: &str, manga_id: i64) -> Vec<(Url, Map<String, Value>)> {
    let start = page
        .find("itemscope itemtype=\"http://schema.org/Book")
        .map_or(0, |i| i + 1);
    let mut scan = Scan::at(page, start);
    let manga = scan.extr("<span itemprop=\"name\">", "</span>").to_owned();
    let kind = scan
        .extr("<span class=\"mngType text-danger\">[", "]</span>")
        .to_owned();
    let mut results = Vec::new();
    while scan.find("<li class=\"sub-chp clearfix\">", "").is_some() {
        let url = scan.extr("<a href=\"", "\"");
        let chapter = scan.extr("title=\"Tagged: -\">\n", "<");
        let chapter_id = scan.extr("/C", "\"");
        let label = unescape(chapter);
        let (chapter, _, title) = partition(label.trim(), " - ");
        let (number, sep, minor) = partition(chapter, ".");
        let mut data = Map::new();
        set(&mut data, "manga_id", manga_id);
        set(&mut data, "manga", manga.as_str());
        set(&mut data, "chapter", parse_int(number));
        set(&mut data, "chapter_minor", format!("{sep}{minor}"));
        set(&mut data, "chapter_id", parse_int(chapter_id));
        set(&mut data, "type", kind.as_str());
        set(&mut data, "title", title);
        english(&mut data);
        if let Ok(url) = Url::parse(url) {
            results.push((url, data));
        }
    }
    results
}

#[cfg(test)]
mod tests {
    use super::super::tests::{archive, path};
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, serve};

    #[test]
    fn reader_layout() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://hentaihere.com/reader/17542-hypnosis-sex-guidance/105688/page-1"),
            Some(Target::Reader(
                "/reader/17542-hypnosis-sex-guidance/105688".into(),
                "17542".into()
            ))
        );
        assert_eq!(
            t("https://hentaihere.com/original/17542-hypnosis-sex-guidance"),
            Some(Target::Manga(
                "/original/17542-hypnosis-sex-guidance".into()
            ))
        );
        let manga = r#"<script type="application/ld+json">[{"@context":"https://schema.org","@type":"Book","name":"Hypnosis &amp; More"}]</script>
            <option value="105689" data-read="https://hentaihere.com/reader/17542-x/105689/page-1" data-added="1 year ago">Chapter 2.5 - Session 2: Reika</option>"#;
        let list = reader_chapters(manga, 17542);
        assert_eq!(list.len(), 1);
        let data = &list[0].1;
        assert_eq!(
            (
                data["manga"].as_str(),
                data["chapter"].as_i64(),
                data["chapter_minor"].as_str(),
                data["title"].as_str()
            ),
            (
                Some("Hypnosis & More"),
                Some(2),
                Some(".5"),
                Some("Session 2: Reika")
            )
        );
        let chapter = r#"<h1 class="sr-only">Hypnosis - Chapter 1: Session 1</h1>
            <script type="application/json" data-reader-data>{"images":["https://hermes.hentai.direct/hentai/17542/1/a.jpg"],
            "chapterUrl":"https://hentaihere.com/reader/17542-x/105688","title":"Reading Hypnosis by Aiue Oka Chapter 1 Online - Page {page} - HentaiHere",
            "book":{"title":"Hypnosis","chapter":"Chapter 1"}}</script>"#;
        let mut data = Map::new();
        let pages = reader_images(chapter, &mut data).unwrap();
        assert_eq!(
            pages[0].0,
            "https://hermes.hentai.direct/hentai/17542/1/a.jpg"
        );
        assert_eq!(
            (
                data["author"].as_str(),
                data["title"].as_str(),
                data["chapter_id"].as_i64()
            ),
            (Some("Aiue Oka"), Some("Session 1"), Some(105688))
        );
    }

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://hentaihere.com/m/S23048/1.5/1/"),
            Some(Target::Chapter("23048".into(), "1.5".into()))
        );
        assert_eq!(
            t("https://hentaihere.com/m/S13812"),
            Some(Target::Manga("/m/S13812".into()))
        );
        assert_eq!(
            t("https://hentaihere.com/m/S13812/"),
            Some(Target::Manga("/m/S13812".into()))
        );
        assert_eq!(t("https://hentaihere.com/m/X1"), None);
    }

    #[tokio::test]
    async fn extracts_chapter() {
        let page = "<title>Page 1 | Uchi no Jimuin (Original) - Chapter 1: Chapter One by Fuuga at HentaiHere</title>\
            <a href=\"/report/C36631\"></a> var rff_imageList = [\"/13812/1/01.jpg\",\"/13812/1/02.jpg\"];";
        let (root, server) = serve(vec![html(page)]);
        let http = http("hentaihere", root);
        let files = media(extract(
            &http,
            Url::parse("https://hentaihere.com/m/S13812/1/1/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[0]);
        assert_eq!(k["manga"], "Uchi no Jimuin");
        assert_eq!(k["type"], "Original");
        assert_eq!(k["title"], "Chapter One");
        assert_eq!(k["author"], "Fuuga");
        assert_eq!(k["chapter_id"], 36631);
        assert_eq!(
            archive(&files[1]),
            r#"["{chapter_id}_{page}",["36631","2"]]"#
        );
        assert_eq!(
            path(&files[0]),
            crate::sites::testing::os_path(
                "hentaihere/Uchi no Jimuin/c001: Chapter One/Uchi no Jimuin_c001_001.jpg"
            )
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /m/S13812/1/1 "));
    }

    #[test]
    fn lists_chapters() {
        let page = "<div itemscope itemtype=\"http://schema.org/Book\"><span itemprop=\"name\">Uchi</span>\
            <span class=\"mngType text-danger\">[Doujinshi]</span>\
            <li class=\"sub-chp clearfix\"><a href=\"https://hentaihere.com/m/S7608/2/1\" title=\"Tagged: -\">\n 2.5 - Two &amp; </a>\
            <a href=\"/report/C99\"></a></li>";
        let list = chapters(page, 7608);
        assert_eq!(list.len(), 1);
        let d = &list[0].1;
        assert_eq!(d["chapter"], 2);
        assert_eq!(d["chapter_minor"], ".5");
        assert_eq!(d["title"], "Two &");
        assert_eq!(d["type"], "Doujinshi");
        assert_eq!(d["chapter_id"], 99);
    }
}
