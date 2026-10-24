//! MangaTown chapters and manga, mirroring gallery_dl/extractor/mangatown.py:
//! a chapter is read one HTML page per image.
use super::{Gallery, keywords, text};
use crate::sites::art::{extr, extract_iter};
use crate::sites::{Events, Http, Route, Site, unescape};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

pub static SITE: Site = Site {
    category: "mangatown",
    name: "MangaTown",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&CHAPTER, &MANGA],
    api_key: false,
    cookies: None,
};

const ROOT: &str = "https://www.mangatown.com";
/// Pages a chapter may claim; each page holds one image.
const MAX_COUNT: i64 = 2000;

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
    /// `part` is `/manga/TITLE[/vNN]/cCHAPTER`, raw.
    Chapter {
        part: String,
        volume: String,
        chapter: String,
    },
    /// `/manga/TITLE`, raw.
    Manga(String),
}

/// `mangatown\.com(/manga/[^/?#]+(?:/v0*(\d+))?/c(\d+[^/?#]*))` and
/// `(/manga/[^/?#]+)`, both unanchored at the end.
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::art::on_host(url, &["mangatown.com"]) {
        return None;
    }
    let raw: Vec<&str> = url.path().split('/').skip(1).collect();
    let ["manga", title, rest @ ..] = raw.as_slice() else {
        return None;
    };
    if title.is_empty() {
        return None;
    }
    let chapter = |segment: &str| {
        let chapter = segment.strip_prefix('c')?;
        chapter
            .starts_with(|c: char| c.is_ascii_digit())
            .then(|| chapter.to_owned())
    };
    let volume = |segment: &str| {
        let digits = segment.strip_prefix('v')?;
        crate::sites::art::digits(digits).then(|| digits.to_owned())
    };
    let found = match rest {
        [first, second, ..] if volume(first).is_some() && chapter(second).is_some() => Some((
            format!("/manga/{title}/{first}/{second}"),
            volume(first).unwrap_or_default(),
            chapter(second).unwrap_or_default(),
        )),
        [first, ..] => chapter(first)
            .map(|chapter| (format!("/manga/{title}/{first}"), String::new(), chapter)),
        [] => None,
    };
    Some(match found {
        Some((part, volume, chapter)) => Target::Chapter {
            part,
            volume,
            chapter,
        },
        None => Target::Manga(format!("/manga/{title}")),
    })
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Chapter {
            part,
            volume,
            chapter,
        }) => {
            crate::sites::once(async move { chapter_events(http, &part, &volume, &chapter).await })
        }
        Some(Target::Manga(path)) => crate::sites::once(async move {
            let page = page(http, &format!("{ROOT}{path}")).await?;
            let mut children = chapters(&page);
            // `MangaExtractor.reverse = True`.
            if !http.option_bool("chapter-reverse", false) {
                children.reverse();
            }
            Ok(super::queue_urls(http, &SITE, &MANGA, children))
        }),
        None => crate::sites::fail(error("unsupported MangaTown URL")),
    }
}

async fn page(http: &Http, url: &str) -> Result<String> {
    http.text_with(&http.api(url)?, &http.browser_headers(&[]))
        .await
}

/// `MangatownChapterExtractor.metadata`.
fn metadata(page: &str, volume: &str, chapter: &str) -> Map<String, Value> {
    let (manga, pos) = text::extract(page, "property=\"og:title\" content=\"", "\"", 0);
    let (count, pos) = text::extract(page, "total_pages = ", ";", pos);
    let (manga_id, pos) = text::extract(page, "series_id=", ";", pos);
    let (chapter_id, _) = text::extract(page, "chapter_id=", ";", pos);
    let (number, minor) = text::chapter(chapter);
    keywords([
        ("manga", unescape(manga.unwrap_or_default()).into()),
        (
            "manga_id",
            text::parse_int(manga_id.unwrap_or_default()).into(),
        ),
        ("volume", text::parse_int(volume).into()),
        ("chapter", number.into()),
        ("chapter_minor", minor.into()),
        (
            "chapter_id",
            text::parse_int(chapter_id.unwrap_or_default()).into(),
        ),
        ("count", text::parse_int(count.unwrap_or_default()).into()),
        ("lang", "en".into()),
        ("language", "English".into()),
    ])
}

/// The image of one reader page, if any.
fn image(page: &str) -> Option<String> {
    let url = match extr(page, "id=\"image\" src=\"", "\"") {
        "" => extr(page, "<img src=\"", "\""),
        url => url,
    };
    (!url.is_empty()).then(|| text::ensure_http_scheme(url))
}

/// `MangatownChapterExtractor.items`: pages 1, 2, 3, … are fetched until
/// `count` images are known or a page has none.
async fn chapter_events<'a>(
    http: &'a Http,
    part: &str,
    volume: &str,
    chapter: &str,
) -> Result<Events<'a>> {
    let base = format!("{ROOT}{part}/");
    let mut current = page(http, &format!("{base}1.html")).await?;
    let data = metadata(&current, volume, chapter);
    let count = data
        .get("count")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .min(MAX_COUNT);
    let mut pages = Vec::new();
    let mut number = 1;
    while (pages.len() as i64) < count {
        if number > 1 {
            current = page(http, &format!("{base}{number}.html")).await?;
        }
        let Some(url) = image(&current) else { break };
        pages.push((url, None));
        number += 1;
    }
    let id = data
        .get("chapter_id")
        .map(crate::sites::scalar)
        .unwrap_or_default();
    Ok(crate::sites::events(
        Gallery::chapter(&CHAPTER, data, pages)
            .id(id)
            .events(&SITE, http),
    ))
}

/// `MangatownMangaExtractor.chapters`, in page order.
fn chapters(page: &str) -> Vec<(Url, Map<String, Value>)> {
    let (manga, pos) = text::extract(page, "<title>", "</title>", 0);
    let manga = text::partition(manga.unwrap_or_default(), " Manga").0;
    let manga = unescape(&manga.replacen("Read ", "", 1));
    let (list, _) = text::extract(page, "class=\"chapter_list\"", "</ul>", pos);
    extract_iter(list.unwrap_or_default(), "<li>", "</li>")
        .into_iter()
        .filter_map(|item| {
            let (path, pos) = text::extract(item, "<a href=\"", "\"", 0);
            let (title, pos) = text::extract(item, "<span>", "<", pos);
            let (date, _) = text::extract(item, "class=\"time\">", "<", pos);
            let url = Url::parse(&format!("{ROOT}{}", path?)).ok()?;
            let (chapter, minor) = text::chapter(extr(path?, "/c", "/"));
            Some((
                url,
                keywords([
                    ("manga", manga.clone().into()),
                    ("chapter", chapter.into()),
                    ("chapter_minor", minor.into()),
                    ("title", title.map(unescape).unwrap_or_default().into()),
                    ("date", date.into()),
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

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.mangatown.com/manga/kimi_ni_todoke/v01/c001/1.html"),
            Some(Target::Chapter {
                part: "/manga/kimi_ni_todoke/v01/c001".into(),
                volume: "01".into(),
                chapter: "001".into()
            })
        );
        assert_eq!(
            t("http://mangatown.com/manga/title/c010.5/"),
            Some(Target::Chapter {
                part: "/manga/title/c010.5".into(),
                volume: String::new(),
                chapter: "010.5".into()
            })
        );
        assert_eq!(
            t("https://www.mangatown.com/manga/kimi_ni_todoke/"),
            Some(Target::Manga("/manga/kimi_ni_todoke".into()))
        );
        assert_eq!(
            t("https://www.mangatown.com/manga/title/v01/"),
            Some(Target::Manga("/manga/title".into()))
        );
        assert_eq!(
            t("https://www.mangatown.com/manga/title/cover"),
            Some(Target::Manga("/manga/title".into()))
        );
        for raw in [
            "https://www.mangatown.com/manga/",
            "https://www.mangatown.com/directory/",
            "https://m.mangatown.com/manga/title/",
            "https://www.mangatown.com.evil.test/manga/title/",
            "https://www.mangatown.com:8443/manga/title/",
            "https://u:p@www.mangatown.com/manga/title/",
            "ftp://www.mangatown.com/manga/title/",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn reader(n: u32) -> String {
        format!(
            r#"<img id="image" src="//zjcdn.mangahere.org/store/manga/123/01-001.0/compressed/p{n:03}.jpg" />"#
        )
    }

    #[tokio::test]
    async fn extracts_chapter_one_page_per_image() {
        let first = format!(
            r#"<meta property="og:title" content="Kimi &amp; Todoke" />
<script>var total_pages = 3; var series_id=1234; var chapter_id=56789;</script>{}"#,
            reader(1)
        );
        let (root, requests) = serve(vec![html(&first), html(&reader(2)), html(&reader(3))]);
        let http = http("MangaTown", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.mangatown.com/manga/kimi_ni_todoke/v01/c001.5/1.html").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 3);
        assert_eq!(
            files[1].url.as_str(),
            "https://zjcdn.mangahere.org/store/manga/123/01-001.0/compressed/p002.jpg"
        );
        let k = kw(&files[2]);
        assert_eq!(k["manga"], "Kimi & Todoke");
        assert_eq!(k["manga_id"], 1234);
        assert_eq!(k["chapter_id"], 56789);
        assert_eq!(k["volume"], 1);
        assert_eq!(k["chapter"], 1);
        assert_eq!(k["chapter_minor"], ".5");
        assert_eq!(k["count"], 3);
        assert_eq!(k["page"], 3);
        assert_eq!(k["lang"], "en");
        assert_eq!(
            path(&files[2]),
            "mangatown/Kimi & Todoke/v01 c001.5/Kimi & Todoke_c001.5_003.jpg"
        );
        assert_eq!(
            archive(&files[2]),
            r#"["{manga}_{chapter}{chapter_minor}_{page}",["Kimi & Todoke","1",".5","3"]]"#
        );
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /manga/kimi_ni_todoke/v01/c001.5/1.html "));
        assert!(requests[1].starts_with("GET /manga/kimi_ni_todoke/v01/c001.5/2.html "));
        assert!(requests[2].starts_with("GET /manga/kimi_ni_todoke/v01/c001.5/3.html "));
    }

    #[tokio::test]
    async fn chapter_stops_at_a_page_without_image() {
        let first = format!(
            r#"<script>var total_pages = 5; var series_id=1; var chapter_id=2;</script>
<img src="https://img.test/a.png">{}"#,
            ""
        );
        let (root, requests) = serve(vec![html(&first), html("<p>end</p>")]);
        let http = http("MangaTown", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.mangatown.com/manga/x/c002/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(kw(&files[0])["count"], 5);
        assert_eq!(kw(&files[0])["manga"], "");
        assert_eq!(requests.join().unwrap().len(), 2);
    }

    const MANGA_PAGE: &str = r#"<title>Read Kimi ni Todoke Manga Online for Free</title>
<ul class="chapter_list">
<li><a href="/manga/kimi_ni_todoke/v31/c123.5/">Kimi ni Todoke 123.5</a><span>Special &amp; More</span><span class="time">Jan 01,2020</span></li>
<li><a href="/manga/kimi_ni_todoke/c001/">Kimi ni Todoke 1</a><span class="time">Today</span></li>
</ul>"#;

    #[test]
    fn parses_chapter_list() {
        let chapters = chapters(MANGA_PAGE);
        assert_eq!(chapters.len(), 2);
        assert_eq!(
            chapters[0].0.as_str(),
            "https://www.mangatown.com/manga/kimi_ni_todoke/v31/c123.5/"
        );
        let k = &chapters[0].1;
        assert_eq!(k["manga"], "Kimi ni Todoke");
        assert_eq!(k["chapter"], 123);
        assert_eq!(k["chapter_minor"], ".5");
        assert_eq!(k["title"], "Special & More");
        assert_eq!(k["date"], "Jan 01,2020");
        assert_eq!(chapters[1].1["title"], "");
        assert_eq!(chapters[1].1["date"], "Today");
        assert_eq!(chapters[1].1["chapter_minor"], "");
    }

    #[tokio::test]
    async fn manga_queues_chapters_oldest_first() {
        let (root, requests) = serve(vec![html(MANGA_PAGE), html(""), html("")]);
        let http = http("MangaTown", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.mangatown.com/manga/kimi_ni_todoke/").unwrap(),
        ))
        .await;
        assert!(files.is_empty());
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /manga/kimi_ni_todoke "));
        assert!(requests[1].starts_with("GET /manga/kimi_ni_todoke/c001/1.html "));
        assert!(requests[2].starts_with("GET /manga/kimi_ni_todoke/v31/c123.5/1.html "));
    }

    #[tokio::test]
    async fn manga_honours_chapter_reverse() {
        let (root, requests) = serve(vec![html(MANGA_PAGE), html(""), html("")]);
        let http = http("MangaTown", root).with_options(Some(std::sync::Arc::new(
            serde_json::json!({"chapter-reverse": true})
                .as_object()
                .unwrap()
                .clone(),
        )));
        assert!(
            media(extract(
                &http,
                Url::parse("https://www.mangatown.com/manga/kimi_ni_todoke").unwrap(),
            ))
            .await
            .is_empty()
        );
        let requests = requests.join().unwrap();
        assert!(requests[1].starts_with("GET /manga/kimi_ni_todoke/v31/c123.5/1.html "));
        assert!(requests[2].starts_with("GET /manga/kimi_ni_todoke/c001/1.html "));
    }

    #[tokio::test]
    async fn manga_children_are_chapter_jobs() {
        let (root, requests) = serve(vec![html(MANGA_PAGE)]);
        let http = http("MangaTown", root);
        let children = queued(extract(
            &http,
            Url::parse("https://www.mangatown.com/manga/kimi_ni_todoke").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 2);
        requests.join().unwrap();
    }
}
