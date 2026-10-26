//! MangaTown chapters and manga, mirroring gallery_dl/extractor/mangatown.py.
use super::{Gallery, chapter_keys, english, fetch, set};
use crate::error;
use crate::sites::gdl::{Scan, ensure_http_scheme, extr, extract_iter, parse_int, unescape};
use crate::sites::{Events, Http, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://www.mangatown.com";

static CHAPTER: Route = super::chapter_route();
static MANGA: Route = super::manga_route();

pub static SITE: Site = Site {
    category: "mangatown",
    name: "MangaTown",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&CHAPTER, &MANGA],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    /// Path up to the chapter segment, volume and chapter number.
    Chapter {
        part: String,
        volume: String,
        chapter: String,
    },
    Manga(String),
}

/// `/manga/[^/?#]+(?:/v0*(\d+))?/c(\d+[^/?#]*)` or `/manga/[^/?#]+`.
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::gdl::on(url, &["mangatown.com"], true) {
        return None;
    }
    let parts = crate::sites::gdl::path_parts(url);
    let ["manga", manga, rest @ ..] = parts.as_slice() else {
        return None;
    };
    if manga.is_empty() {
        return None;
    }
    let chapter_of = |seg: &str| {
        let number = seg.strip_prefix('c')?;
        number
            .starts_with(|c: char| c.is_ascii_digit())
            .then(|| number.to_owned())
    };
    let volume_of = |seg: &str| {
        let number = seg.strip_prefix('v')?;
        crate::sites::gdl::digits(number).then(|| number.trim_start_matches('0').to_owned())
    };
    let chapter = match rest {
        [seg, ..] if chapter_of(seg).is_some() => Some((
            String::new(),
            chapter_of(seg)?,
            format!("/manga/{manga}/{seg}"),
        )),
        [vol, seg, ..] if volume_of(vol).is_some() && chapter_of(seg).is_some() => Some((
            volume_of(vol)?,
            chapter_of(seg)?,
            format!("/manga/{manga}/{vol}/{seg}"),
        )),
        _ => None,
    };
    Some(match chapter {
        Some((volume, chapter, part)) => Target::Chapter {
            part,
            volume,
            chapter,
        },
        None => Target::Manga(format!("/manga/{manga}")),
    })
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported MangaTown URL"));
    };
    crate::sites::once(async move {
        Ok(match target {
            Target::Chapter {
                part,
                volume,
                chapter,
            } => {
                let base = format!("{ROOT}{part}/");
                let mut page = fetch(http, &format!("{base}1.html")).await?;
                let data = chapter_metadata(&page, &volume, &chapter);
                let count = usize::try_from(parse_int_value(&data)).unwrap_or(0);
                let mut pages = Vec::new();
                loop {
                    let image = image(&page);
                    if image.is_empty() {
                        break;
                    }
                    pages.push((ensure_http_scheme(image), None));
                    if count > 0 && pages.len() >= count {
                        break;
                    }
                    page = fetch(http, &format!("{base}{}.html", pages.len() + 1)).await?;
                }
                crate::sites::events(Gallery::chapter(&CHAPTER, data, pages).events(&SITE, http))
            }
            Target::Manga(path) => {
                let page = fetch(http, &format!("{ROOT}{path}")).await?;
                super::queue_chapters(http, &SITE, &MANGA, chapters(&page), true)
            }
        })
    })
}

fn parse_int_value(data: &Map<String, Value>) -> i64 {
    data.get("count").and_then(Value::as_i64).unwrap_or(0)
}

fn image(page: &str) -> &str {
    match extr(page, "id=\"image\" src=\"", "\"") {
        "" => extr(page, "<img src=\"", "\""),
        url => url,
    }
}

fn chapter_metadata(page: &str, volume: &str, chapter: &str) -> Map<String, Value> {
    let mut scan = Scan::new(page);
    let manga = scan.extr("property=\"og:title\" content=\"", "\"");
    let count = scan.extr("total_pages = ", ";");
    let manga_id = scan.extr("series_id=", ";");
    let chapter_id = scan.extr("chapter_id=", ";");
    let mut data = Map::new();
    set(&mut data, "manga", unescape(manga));
    set(&mut data, "manga_id", parse_int(manga_id));
    set(&mut data, "volume", parse_int(volume));
    chapter_keys(&mut data, chapter);
    set(&mut data, "chapter_id", parse_int(chapter_id));
    set(&mut data, "count", parse_int(count));
    english(&mut data);
    data
}

fn chapters(page: &str) -> Vec<(Url, Map<String, Value>)> {
    let mut scan = Scan::new(page);
    let title = scan.extr("<title>", "</title>");
    let manga = crate::sites::gdl::partition(title, " Manga")
        .0
        .replacen("Read ", "", 1);
    let manga = unescape(&manga);
    let list = scan.extr("class=\"chapter_list\"", "</ul>");
    extract_iter(list, "<li>", "</li>")
        .into_iter()
        .filter_map(|ch| {
            let mut scan = Scan::new(ch);
            let path = scan.extr("<a href=\"", "\"");
            let title = scan.find("<span>", "<");
            let date = scan.extr("class=\"time\">", "<");
            let mut data = Map::new();
            set(&mut data, "manga", manga.as_str());
            chapter_keys(&mut data, extr(path, "/c", "/"));
            set(&mut data, "title", title.map(unescape).unwrap_or_default());
            set(&mut data, "date", date);
            english(&mut data);
            Some((Url::parse(&format!("{ROOT}{path}")).ok()?, data))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::tests::path;
    use super::*;
    use crate::sites::testing::{children, html, http, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.mangatown.com/manga/kimetsu_no_yaiba/c001/1.html"),
            Some(Target::Chapter {
                part: "/manga/kimetsu_no_yaiba/c001".into(),
                volume: String::new(),
                chapter: "001".into()
            })
        );
        assert_eq!(
            t("http://mangatown.com/manga/x/v02/c010.5/"),
            Some(Target::Chapter {
                part: "/manga/x/v02/c010.5".into(),
                volume: "2".into(),
                chapter: "010.5".into()
            })
        );
        assert_eq!(
            t("https://www.mangatown.com/manga/kimetsu_no_yaiba/"),
            Some(Target::Manga("/manga/kimetsu_no_yaiba".into()))
        );
        assert_eq!(t("https://www.mangatown.com/directory/"), None);
    }

    #[tokio::test]
    async fn extracts_chapter_pages() {
        let first = r#"<meta property="og:title" content="Kimetsu no Yaiba">
            var total_pages = 2; var series_id=123; var chapter_id=456;
            <img id="image" src="//zjcdn.mangahere.org/store/1.jpg">"#;
        let second = r#"<img src="https://zjcdn.mangahere.org/store/2.jpg">"#;
        let (root, server) = serve(vec![html(first), html(second)]);
        let http = http("mangatown", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.mangatown.com/manga/kimetsu_no_yaiba/c001/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[0]);
        assert_eq!(k["manga_id"], 123);
        assert_eq!(k["chapter_id"], 456);
        assert_eq!(k["chapter"], 1);
        assert_eq!(k["volume"], 0);
        assert_eq!(
            files[0].url.as_str(),
            "https://zjcdn.mangahere.org/store/1.jpg"
        );
        assert_eq!(
            path(&files[1]),
            "mangatown/Kimetsu no Yaiba/c001/Kimetsu no Yaiba_c001_002.jpg"
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /manga/kimetsu_no_yaiba/c001/1.html "));
        assert!(requests[1].starts_with("GET /manga/kimetsu_no_yaiba/c001/2.html "));
    }

    #[tokio::test]
    async fn manga_lists_chapters() {
        let page = r#"<title>Read Kimetsu no Yaiba Manga Online</title>
            <ul class="chapter_list">
            <li><a href="/manga/k/c002/">x</a><span>Two</span><span class="time">Jan 2</span></li>
            <li><a href="/manga/k/c001.5/">x</a><span class="time">Jan 1</span></li>
            </ul>"#;
        let list = chapters(page);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].1["manga"], "Kimetsu no Yaiba");
        assert_eq!(list[0].1["title"], "Two");
        assert_eq!(list[1].1["chapter_minor"], ".5");
        let (root, server) = serve(vec![html(page)]);
        let http = http("mangatown", root);
        let queued = children(extract(
            &http,
            Url::parse("https://www.mangatown.com/manga/k/").unwrap(),
        ))
        .await;
        assert_eq!(queued.len(), 2);
        server.join().unwrap();
    }
}
