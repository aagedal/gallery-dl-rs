//! Weeb Central chapters and series, mirroring
//! gallery_dl/extractor/weebcentral.py.
use super::{Gallery, set};
use crate::error;
use crate::sites::gdl::{
    Scan, captures, date, group, parse_int, parse_iso, partition, re, remove_html, split_html,
    unescape,
};
use crate::sites::{Events, Http, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://weebcentral.com";

static CHAPTER: Route = super::chapter_route();
static MANGA: Route = super::manga_route();

pub static SITE: Site = Site {
    category: "weebcentral",
    name: "Weeb Central",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&CHAPTER, &MANGA],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Chapter(String),
    Manga(String),
}

fn target(url: &Url) -> Option<Target> {
    let chapter = re!(r"^https?://(?:www\.)?weebcentral\.com/chapters/(\w+)");
    if let Some(caps) = captures(chapter, url) {
        return Some(Target::Chapter(group(&caps, 1).into()));
    }
    let manga = re!(r"^https?://(?:www\.)?weebcentral\.com/series/(\w+)");
    captures(manga, url).map(|caps| Target::Manga(group(&caps, 1).into()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Weeb Central URL"));
    };
    crate::sites::once(async move {
        Ok(match target {
            Target::Chapter(id) => {
                let referer = format!("{ROOT}/chapters/{id}");
                let page = super::fetch(http, &referer).await?;
                let mut scan = Scan::new(&page);
                let manga_id = scan.extr("'series_id': '", "'").to_owned();
                let chapter_type = scan.extr("'chapter_type': '", "'").to_owned();
                let (chapter, sep, minor) = partition(scan.extr("'number': '", "'"), ".");
                let mut data = manga_info(http, &manga_id).await?;
                set(&mut data, "chapter", parse_int(chapter));
                set(&mut data, "chapter_id", id.as_str());
                set(&mut data, "chapter_type", chapter_type);
                set(&mut data, "chapter_minor", format!("{sep}{minor}"));
                let mut url = http.api(&format!("{referer}/images"))?;
                url.query_pairs_mut()
                    .append_pair("is_prev", "False")
                    .append_pair("current_page", "1")
                    .append_pair("reading_style", "long_strip");
                let images = http
                    .text_with(
                        &url,
                        &[
                            ("Accept", "*/*"),
                            ("Referer", &referer),
                            ("HX-Request", "true"),
                            ("HX-Current-URL", &referer),
                        ],
                    )
                    .await?;
                crate::sites::events(
                    Gallery::chapter(&CHAPTER, data, pages(&images)).events(&SITE, http),
                )
            }
            Target::Manga(id) => {
                let referer = format!("{ROOT}/series/{id}");
                let list = http
                    .text_with(
                        &http.api(&format!("{referer}/full-chapter-list"))?,
                        &[
                            ("Accept", "*/*"),
                            ("Referer", &referer),
                            ("HX-Request", "true"),
                            ("HX-Target", "chapter-list"),
                            ("HX-Current-URL", &referer),
                        ],
                    )
                    .await?;
                let data = manga_info(http, &id).await?;
                super::queue_chapters(http, &SITE, &MANGA, chapters(&list, &data), true)
            }
        })
    })
}

/// `_manga_info`.
async fn manga_info(http: &Http, manga_id: &str) -> crate::Result<Map<String, Value>> {
    let page = super::fetch(http, &format!("{ROOT}/series/{manga_id}")).await?;
    let mut scan = Scan::new(&page);
    let odd =
        |text: &str| -> Vec<String> { split_html(text).into_iter().skip(1).step_by(2).collect() };
    let mut data = Map::new();
    set(&mut data, "manga_id", manga_id);
    set(&mut data, "lang", "en");
    set(&mut data, "language", "English");
    set(
        &mut data,
        "manga",
        unescape(scan.extr("<title>", " | Weeb Central")),
    );
    set(
        &mut data,
        "author",
        odd(scan.extr("<strong>Author", "</li>")),
    );
    set(&mut data, "tags", odd(scan.extr("<strong>Tag", "</li>")));
    set(
        &mut data,
        "type",
        remove_html(scan.extr("<strong>Type: ", "</li>")),
    );
    set(
        &mut data,
        "status",
        remove_html(scan.extr("<strong>Status: ", "</li>")),
    );
    set(
        &mut data,
        "release",
        remove_html(scan.extr("<strong>Released: ", "</li>")),
    );
    set(
        &mut data,
        "official",
        scan.extr("<strong>Official Translatio", "</li>")
            .contains(">Yes"),
    );
    set(
        &mut data,
        "description",
        unescape(&remove_html(scan.extr("<strong>Description", "</li>"))),
    );
    Ok(data)
}

fn pages(page: &str) -> Vec<super::Page> {
    let mut scan = Scan::new(page);
    let mut results = Vec::new();
    while let Some(src) = scan.find("src=\"", "\"").filter(|s| !s.is_empty()) {
        let mut image = Map::new();
        set(&mut image, "width", parse_int(scan.extr("width=\"", "\"")));
        set(
            &mut image,
            "height",
            parse_int(scan.extr("height=\"", "\"")),
        );
        results.push((src.to_owned(), Some(image)));
    }
    results
}

fn chapters(page: &str, data: &Map<String, Value>) -> Vec<(Url, Map<String, Value>)> {
    let mut scan = Scan::new(page);
    let mut results = Vec::new();
    while let Some(id) = scan.find("/chapters/", "\"").filter(|s| !s.is_empty()) {
        let (kind, _, chapter) = partition(scan.extr("<span class=\"\">", "<"), " ");
        let (number, sep, minor) = partition(chapter, ".");
        let stamp = scan.extr(" datetime=\"", "\"");
        let stamp = &stamp[..stamp.len().saturating_sub(5)];
        let mut entry = Map::new();
        set(&mut entry, "chapter_id", id);
        set(&mut entry, "chapter", parse_int(number));
        set(&mut entry, "chapter_minor", format!("{sep}{minor}"));
        set(&mut entry, "chapter_type", kind);
        set(&mut entry, "date", date(parse_iso(stamp)));
        entry.extend(data.clone());
        if let Ok(url) = Url::parse(&format!("{ROOT}/chapters/{id}")) {
            results.push((url, entry));
        }
    }
    results
}

#[cfg(test)]
mod tests {
    use super::super::tests::path;
    use super::*;
    use crate::sites::testing::{children, html, http, keywords, media, serve};

    const SERIES: &str = "<title>Aria | Weeb Central</title><li><strong>Author(s): </strong><a>Amano</a></li>\
        <li><strong>Tags(s): </strong><a>Drama</a>, <a>Sci-fi</a></li><li><strong>Type: </strong>Manga</li>\
        <li><strong>Status: </strong>Complete</li><li><strong>Released: </strong><span>2002</span></li>\
        <li><strong>Official Translation: </strong><a>Yes</a></li><li><strong>Description</strong><p>A &amp; B</p></li>";

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://weebcentral.com/chapters/01J76XZ4PC3VW91BYFBQJA44C3"),
            Some(Target::Chapter("01J76XZ4PC3VW91BYFBQJA44C3".into()))
        );
        assert_eq!(
            t("https://weebcentral.com/series/01J76XY8G1GK8EJ9VQG92C3DKM/Aria"),
            Some(Target::Manga("01J76XY8G1GK8EJ9VQG92C3DKM".into()))
        );
        assert_eq!(t("https://weebcentral.com/search"), None);
    }

    #[tokio::test]
    async fn extracts_chapter() {
        let chapter = "'series_id': 'S1', 'chapter_type': 'Chapter', 'number': '10.5'";
        let images = r#"<img src="https://scans.lastation.us/a/1.png" width="800" height="1200"><img src="https://scans.lastation.us/a/2.png" width="1" height="2">"#;
        let (root, server) = serve(vec![html(chapter), html(SERIES), html(images)]);
        let http = http("weebcentral", root);
        let files = media(extract(
            &http,
            Url::parse("https://weebcentral.com/chapters/C1").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[0]);
        assert_eq!(k["manga"], "Aria");
        assert_eq!(k["author"], serde_json::json!(["Amano"]));
        assert_eq!(k["tags"], serde_json::json!(["Drama", "Sci-fi"]));
        assert_eq!(k["official"], true);
        assert_eq!(k["description"], "A & B");
        assert_eq!(k["width"], 800);
        assert_eq!(k["chapter_type"], "Chapter");
        assert_eq!(
            path(&files[1]),
            "weebcentral/Aria/c010.5/Aria_c010.5_002.png"
        );
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /series/S1 "));
        assert!(requests[2].starts_with(
            "GET /chapters/C1/images?is_prev=False&current_page=1&reading_style=long_strip "
        ));
    }

    #[tokio::test]
    async fn series_lists_chapters() {
        let list = r#"<a href="https://weebcentral.com/chapters/C2"><span class="">Chapter 2</span><time datetime="2024-01-02T03:04:05.000Z"></time></a>
            <a href="https://weebcentral.com/chapters/C1"><span class="">Episode 1.5</span><time datetime="2024-01-01T00:00:00.000Z"></time></a>"#;
        let (root, server) = serve(vec![html(list), html(SERIES)]);
        let http = http("weebcentral", root);
        assert_eq!(
            children(extract(
                &http,
                Url::parse("https://weebcentral.com/series/S1/Aria").unwrap()
            ))
            .await
            .len(),
            2
        );
        server.join().unwrap();
        let data = Map::new();
        let parsed = chapters(list, &data);
        assert_eq!(parsed[0].1["date"], "2024-01-02 03:04:05");
        assert_eq!(parsed[1].1["chapter_type"], "Episode");
        assert_eq!(parsed[1].1["chapter_minor"], ".5");
    }
}
