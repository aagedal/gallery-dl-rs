//! Rawkuma chapters and manga, mirroring gallery_dl/extractor/rawkuma.py.
use super::{Gallery, chapter_keys, fetch, set};
use crate::error;
use crate::sites::gdl::{date, extr, extract_iter, parse_int, parse_iso, rpartition, unescape};
use crate::sites::{Events, Http, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://rawkuma.net";

static CHAPTER: Route = Route::new(
    "chapter",
    super::CHAPTER_FILENAME,
    super::CHAPTER_DIRECTORY,
    "{chapter_id}_{page}",
);
static MANGA: Route = super::manga_route();

pub static SITE: Site = Site {
    category: "rawkuma",
    name: "Rawkuma",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&CHAPTER, &MANGA],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    /// Manga slug, chapter slug and chapter ID.
    Chapter(String, String, String),
    Manga(String),
}

/// `chapter-\d+(?:.\d+)?\.(\d+)`: the chapter ID after the last dot.
fn chapter_id(slug: &str) -> Option<&str> {
    let rest = slug.strip_prefix("chapter-")?;
    let (number, id) = rest.rsplit_once('.')?;
    let lead = number
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(number.len());
    let tail = &number[lead..];
    let tail_ok = tail.is_empty()
        || (tail.chars().count() >= 2 && tail.chars().skip(1).all(|c| c.is_ascii_digit()));
    (lead > 0 && tail_ok && crate::sites::gdl::digits(id)).then_some(id)
}

fn target(url: &Url) -> Option<Target> {
    if !crate::sites::gdl::on(url, &["rawkuma.net", "rawkuma.com"], false) {
        return None;
    }
    match crate::sites::gdl::path_parts(url).as_slice() {
        ["manga", manga, chapter, ..] if !manga.is_empty() => match chapter_id(chapter) {
            Some(id) => Some(Target::Chapter(
                (*manga).into(),
                (*chapter).into(),
                id.into(),
            )),
            None => Some(Target::Manga((*manga).into())),
        },
        ["manga", manga, ..] if !manga.is_empty() => Some(Target::Manga((*manga).into())),
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Rawkuma URL"));
    };
    crate::sites::once(async move {
        Ok(match target {
            Target::Chapter(manga, chapter, id) => {
                let page = fetch(http, &format!("{ROOT}/manga/{manga}/{chapter}/")).await?;
                let data = chapter_metadata(&page, &id);
                let pages = extract_iter(&page, "<img src='", "'")
                    .into_iter()
                    .map(|url| (url.to_owned(), None))
                    .collect();
                crate::sites::events(
                    Gallery::chapter(&CHAPTER, data, pages)
                        .id(id)
                        .events(&SITE, http),
                )
            }
            Target::Manga(manga) => {
                let page_url = format!("{ROOT}/manga/{manga}/");
                let page = fetch(http, &page_url).await?;
                let manga = unescape(extr(&page, "<title>", " &#8211; "));
                let manga_id = parse_int(extr(&page, "manga_id=", "&"));
                let mut list = http.api(&format!("{ROOT}/wp-admin/admin-ajax.php"))?;
                list.query_pairs_mut()
                    .append_pair("manga_id", &manga_id.to_string())
                    .append_pair("page", "1")
                    .append_pair("action", "chapter_list");
                let html = http
                    .text_with(
                        &list,
                        &[
                            ("HX-Request", "true"),
                            ("HX-Trigger", "chapter-list"),
                            ("HX-Target", "chapter-list"),
                            ("HX-Current-URL", &page_url),
                            ("Sec-Fetch-Dest", "empty"),
                            ("Sec-Fetch-Mode", "cors"),
                            ("Sec-Fetch-Site", "same-origin"),
                        ],
                    )
                    .await?;
                super::queue_chapters(http, &SITE, &MANGA, chapters(&html, &manga, manga_id), true)
            }
        })
    })
}

fn chapter_metadata(page: &str, id: &str) -> Map<String, Value> {
    let title = extr(page, "<title>", "<");
    let (manga, _, chapter) = rpartition(title, " Chapter ");
    let chapter = crate::sites::gdl::partition(chapter, " &#8211; ").0;
    let mut data = Map::new();
    set(&mut data, "manga", unescape(manga));
    set(
        &mut data,
        "manga_id",
        parse_int(extr(page, "manga_id=", "&")),
    );
    chapter_keys(&mut data, chapter);
    set(&mut data, "chapter_id", parse_int(id));
    set(
        &mut data,
        "date",
        date(parse_iso(extr(page, "datetime=\"", "\""))),
    );
    set(&mut data, "lang", "ja");
    set(&mut data, "language", "Japanese");
    data
}

fn chapters(html: &str, manga: &str, manga_id: i64) -> Vec<(Url, Map<String, Value>)> {
    extract_iter(html, "<a href=\"", "\"")
        .into_iter()
        .filter_map(|raw| {
            let info = raw.get(raw.rfind('-')? + 1..raw.len().checked_sub(1)?)?;
            let (chapter, _, chapter_id) = rpartition(info, ".");
            let (number, sep, minor) = crate::sites::gdl::partition(chapter, ".");
            let mut data = Map::new();
            set(&mut data, "manga", manga);
            set(&mut data, "manga_id", manga_id);
            set(&mut data, "chapter", parse_int(number));
            set(&mut data, "chapter-minor", format!("{sep}{minor}"));
            set(&mut data, "chapter_id", parse_int(chapter_id));
            Some((Url::parse(raw).ok()?, data))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::tests::{archive, path};
    use super::*;
    use crate::sites::testing::{children, html, http, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://rawkuma.net/manga/saikyou/chapter-3.28214/"),
            Some(Target::Chapter(
                "saikyou".into(),
                "chapter-3.28214".into(),
                "28214".into()
            ))
        );
        assert_eq!(
            t("https://rawkuma.net/manga/makutsu/chapter-3.4.205398/"),
            Some(Target::Chapter(
                "makutsu".into(),
                "chapter-3.4.205398".into(),
                "205398".into()
            ))
        );
        assert_eq!(
            t("https://rawkuma.com/manga/makutsu"),
            Some(Target::Manga("makutsu".into()))
        );
        assert_eq!(t("https://www.rawkuma.net/manga/x"), None);
        assert_eq!(t("https://rawkuma.net/series/x"), None);
    }

    #[tokio::test]
    async fn extracts_chapter() {
        let page = "<title>Some &amp; Manga Chapter 3.4 &#8211; Rawkuma</title>\
            <a href=\"?manga_id=91&x\"></a><time datetime=\"2024-05-01T10:00:00+00:00\">\
            <img src='https://cdn.rawkuma.net/a/01.jpg'><img src='https://cdn.rawkuma.net/a/02.webp'>";
        let (root, server) = serve(vec![html(page)]);
        let http = http("rawkuma", root);
        let files = media(extract(
            &http,
            Url::parse("https://rawkuma.net/manga/some/chapter-3.4.205398/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[1]);
        assert_eq!(k["manga"], "Some & Manga");
        assert_eq!(k["manga_id"], 91);
        assert_eq!(k["chapter"], 3);
        assert_eq!(k["chapter_minor"], ".4");
        assert_eq!(k["chapter_id"], 205398);
        assert_eq!(k["date"], "2024-05-01 10:00:00");
        assert_eq!(k["language"], "Japanese");
        assert_eq!(
            path(&files[1]),
            "rawkuma/Some & Manga/c003.4/Some & Manga_c003.4_002.webp"
        );
        assert_eq!(
            archive(&files[0]),
            r#"["{chapter_id}_{page}",["205398","1"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /manga/some/chapter-3.4.205398/ "));
    }

    #[tokio::test]
    async fn manga_queues_chapter_list() {
        let page = "<title>Some Manga &#8211; Rawkuma</title><a href=\"?manga_id=91&x\">";
        let list = "<a href=\"https://rawkuma.net/manga/some/chapter-2.5.300/\">\
            <a href=\"https://rawkuma.net/manga/some/chapter-1.200/\">";
        let (root, server) = serve(vec![html(page), html(list)]);
        let http = http("rawkuma", root);
        let queued = children(extract(
            &http,
            Url::parse("https://rawkuma.net/manga/some").unwrap(),
        ))
        .await;
        assert_eq!(queued.len(), 2);
        let requests = server.join().unwrap();
        assert!(
            requests[1].starts_with(
                "GET /wp-admin/admin-ajax.php?manga_id=91&page=1&action=chapter_list "
            )
        );
        assert!(
            requests[1]
                .to_ascii_lowercase()
                .contains("hx-trigger: chapter-list")
        );
        let list = chapters(list, "Some Manga", 91);
        // MangaExtractor.reverse: oldest first.
        assert_eq!(list[1].1["chapter"], 1);
        assert_eq!(list[0].1["chapter-minor"], ".5");
        assert_eq!(list[0].1["chapter_id"], 300);
    }
}
