//! MangaHere chapters and manga, mirroring gallery_dl/extractor/mangahere.py:
//! chapters are read from the mobile site two images per page.
use super::{Gallery, keywords, text};
use crate::sites::{Events, Http, Route, Site, unescape};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

pub static SITE: Site = Site {
    category: "mangahere",
    name: "MangaHere",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&CHAPTER, &MANGA],
    api_key: false,
    cookies: None,
};

const ROOT: &str = "https://www.mangahere.cc";
const ROOT_MOBILE: &str = "https://m.mangahere.cc";
/// `_init`: the session Referer is the mobile root.
const REFERER: &str = "https://m.mangahere.cc/";
/// Pages a chapter may claim; each mobile page holds two images.
const MAX_COUNT: i64 = 5000;
const FILENAME: &str = "{manga}_c{chapter:>03}{chapter_minor:?//}_{page:>03}.{extension}";
const DIRECTORY: &[&str] = &[
    "{category}",
    "{manga}",
    "{volume:?v/ />02}c{chapter:>03}{chapter_minor:?//}{title:?: //}",
];

static CHAPTER: Route = Route::new(
    "chapter",
    FILENAME,
    DIRECTORY,
    "{manga}_{chapter}{chapter_minor}_{page}",
);
static MANGA: Route = Route::new("manga", FILENAME, DIRECTORY, "");

#[derive(Debug, PartialEq)]
enum Target {
    /// `part` is `TITLE[/vNN]/cCHAPTER`, raw.
    Chapter {
        part: String,
        volume: String,
        chapter: String,
    },
    /// `/manga/TITLE[/]`, raw.
    Manga(String),
}

/// `(?:www\.|m\.)?mangahere\.c[co]/manga/([^/]+(?:/v0*(\d+))?/c([^/?#]+))`
/// and `(/manga/[^/?#]+/?)(?:#.*)?$`.
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) {
        return None;
    }
    let host = url.host_str()?;
    let host = host
        .strip_prefix("www.")
        .or_else(|| host.strip_prefix("m."))
        .unwrap_or(host);
    if !matches!(host, "mangahere.cc" | "mangahere.co") {
        return None;
    }
    let raw: Vec<&str> = url.path().split('/').skip(1).collect();
    let chapter = |title: &str, volume: &str, segment: &str| {
        let chapter = segment.strip_prefix('c').filter(|c| !c.is_empty())?;
        let part = if volume.is_empty() {
            format!("{title}/{segment}")
        } else {
            format!("{title}/v{volume}/{segment}")
        };
        Some(Target::Chapter {
            part,
            volume: volume.to_owned(),
            chapter: chapter.to_owned(),
        })
    };
    fn volume(segment: &str) -> Option<&str> {
        let digits = segment.strip_prefix('v')?;
        (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())).then_some(digits)
    }
    match raw.as_slice() {
        ["manga", title, second, rest @ ..] if !title.is_empty() => {
            if let (Some(number), [third, ..]) = (volume(second), rest)
                && let Some(target) = chapter(title, number, third)
            {
                return Some(target);
            }
            if let Some(target) = chapter(title, "", second) {
                return Some(target);
            }
            (second.is_empty() && rest.is_empty() && url.query().is_none())
                .then(|| Target::Manga(format!("/manga/{title}/")))
        }
        ["manga", title] if !title.is_empty() && url.query().is_none() => {
            Some(Target::Manga(format!("/manga/{title}")))
        }
        _ => None,
    }
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
            let root = http.api(ROOT)?;
            http.set_cookie(&root, "isAdult=1; Path=/");
            let page = page(http, &format!("{ROOT}{path}")).await?;
            let mut children = chapters(&page);
            // `MangaExtractor.reverse = True`.
            if !http.option_bool("chapter-reverse", false) {
                children.reverse();
            }
            Ok(super::queue_urls(http, &SITE, &MANGA, children))
        }),
        None => crate::sites::fail(error("unsupported MangaHere URL")),
    }
}

async fn page(http: &Http, url: &str) -> Result<String> {
    http.text_with(
        &http.api(url)?,
        &http.browser_headers(&[("Referer", REFERER)]),
    )
    .await
}

/// `text.extract`: the text between `begin` and `end` from `pos`, and the
/// position after `end`; a miss keeps `pos`.
fn extract_at<'t>(txt: &'t str, begin: &str, end: &str, pos: usize) -> (Option<&'t str>, usize) {
    let found = txt.get(pos..).and_then(|rest| {
        let first = pos + rest.find(begin)? + begin.len();
        let last = first + txt[first..].find(end)?;
        Some((&txt[first..last], last + end.len()))
    });
    match found {
        Some((value, next)) => (Some(value), next),
        None => (None, pos),
    }
}

/// `MangahereChapterExtractor.metadata`, before the title lookup.
fn metadata(page: &str, volume: &str, chapter: &str) -> Result<Map<String, Value>> {
    let pos = page
        .find("</select>")
        .ok_or_else(|| error("MangaHere chapter has no page selector"))?;
    let mut start = pos.saturating_sub(40);
    while !page.is_char_boundary(start) {
        start -= 1;
    }
    let (count, pos) = extract_at(page, ">", "<", start);
    let (manga_id, pos) = extract_at(page, "series_id = ", ";", pos);
    let (chapter_id, pos) = extract_at(page, "chapter_id = ", ";", pos);
    let (manga, _) = extract_at(page, "\"name\":\"", "\"", pos);
    let (number, minor) = text::chapter(chapter);
    Ok(keywords([
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
    ]))
}

/// `_get_title`: the `title` after the second mention of `part`.
fn title(page: &str, part: &str) -> String {
    let after = |pos: usize| page[pos..].find(part).map(|i| pos + i + part.len());
    after(0)
        .and_then(after)
        .and_then(|pos| extract_at(page, " title=\"", "\"", pos).0)
        .unwrap_or_default()
        .to_owned()
}

/// The two images of one mobile page.
fn images(page: &str) -> Result<[String; 2]> {
    let missing = || error("MangaHere page has no image");
    let (first, pos) = extract_at(page, "<img src=\"", "\"", 0);
    let first = first.ok_or_else(missing)?;
    let second = extract_at(page, " src=\"", "\"", pos)
        .0
        .ok_or_else(missing)?;
    Ok([first, second].map(|url| text::ensure_http_scheme(&unescape(url))))
}

/// `MangahereChapterExtractor.items`: mobile pages 1, 3, 5, … are fetched
/// until `count` images are known.
async fn chapter_events<'a>(
    http: &'a Http,
    part: &str,
    volume: &str,
    chapter: &str,
) -> Result<Events<'a>> {
    let base = format!("{ROOT_MOBILE}/manga/{part}/");
    let first = page(http, &format!("{base}1.html")).await?;
    let mut data = metadata(&first, volume, chapter)?;
    let title_page = page(http, &format!("{ROOT}/manga/{part}/")).await?;
    data.insert("title".into(), title(&title_page, part).into());
    let count = data
        .get("count")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .min(MAX_COUNT);
    let mut pages = Vec::new();
    let mut number = 1;
    let mut current = first;
    while (pages.len() as i64) < count {
        if number > 1 {
            current = page(http, &format!("{base}{number}.html")).await?;
        }
        pages.extend(images(&current)?.map(|url| (url, None)));
        number += 2;
    }
    let id = data
        .get("chapter_id")
        .map(crate::sites::scalar)
        .unwrap_or_default();
    Ok(crate::sites::events(
        Gallery::chapter(&CHAPTER, data, pages)
            .id(id)
            .header("Referer", REFERER)
            .events(&SITE, http),
    ))
}

/// `(?:Vol\.0*(\d+) )?Ch\.0*(\d+)(\S*)(?: - (.*))?` at the start of a
/// chapter's label, as `(volume, chapter, minor, title)`.
fn label(info: &str) -> Option<(&str, &str, &str, &str)> {
    fn digits(s: &str) -> Option<(&str, &str)> {
        let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
        (end > 0).then(|| s.split_at(end))
    }
    let (volume, rest) = info
        .strip_prefix("Vol.")
        .and_then(digits)
        .and_then(|(volume, rest)| Some((volume, rest.strip_prefix(' ')?)))
        .unwrap_or(("", info));
    let (chapter, rest) = digits(rest.strip_prefix("Ch.")?)?;
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let (minor, rest) = rest.split_at(end);
    let title = rest
        .strip_prefix(" - ")
        .map(|title| title.split('\n').next().unwrap_or_default())
        .unwrap_or_default();
    Some((volume, chapter, minor, title))
}

/// `MangahereMangaExtractor.chapters`, in page order.
fn chapters(page: &str) -> Vec<(Url, Map<String, Value>)> {
    let (manga, pos) = extract_at(page, "<meta name=\"og:title\" content=\"", "\"", 0);
    let manga = unescape(manga.unwrap_or_default());
    let (list, _) = extract_at(
        page,
        "id=\"chapterlist\"",
        "class=\"detail-main-list-more\"",
        pos,
    );
    let list = list.unwrap_or_default();
    let mut results = Vec::new();
    let mut pos = 0;
    loop {
        let (href, next) = extract_at(list, " href=\"", "\"", pos);
        let Some(href) = href.filter(|href| !href.is_empty()) else {
            return results;
        };
        let (info, next) = extract_at(list, "class=\"title3\">", "<", next);
        let (date, next) = extract_at(list, "class=\"title2\">", "<", next);
        pos = next;
        let (volume, chapter, minor, title) = match label(info.unwrap_or_default()) {
            Some((volume, chapter, minor, title)) => {
                (volume, chapter, minor.to_owned(), unescape(title))
            }
            None => {
                let trimmed = &href[..href.len() - href.chars().last().map_or(0, char::len_utf8)];
                let tail = trimmed.rsplit_once("/c").map_or(trimmed, |(_, tail)| tail);
                let (chapter, _, minor) = text::partition(tail, ".");
                ("", chapter, format!(".{minor}"), String::new())
            }
        };
        let Ok(url) = Url::parse(ROOT).and_then(|root| root.join(href)) else {
            continue;
        };
        results.push((
            url,
            keywords([
                ("manga", manga.clone().into()),
                ("title", title.into()),
                ("volume", text::parse_int(volume).into()),
                ("chapter", text::parse_int(chapter).into()),
                ("chapter_minor", minor.into()),
                ("date", date.unwrap_or_default().into()),
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
            t("https://www.mangahere.cc/manga/dongguo_xiaojie/c004.2/"),
            Some(Target::Chapter {
                part: "dongguo_xiaojie/c004.2".into(),
                volume: String::new(),
                chapter: "004.2".into()
            })
        );
        assert_eq!(
            t("http://m.mangahere.co/manga/title/v01/c001/1.html"),
            Some(Target::Chapter {
                part: "title/v01/c001".into(),
                volume: "01".into(),
                chapter: "001".into()
            })
        );
        assert_eq!(
            t("https://www.mangahere.cc/manga/aria/#50"),
            Some(Target::Manga("/manga/aria/".into()))
        );
        assert_eq!(
            t("https://mangahere.co/manga/aria"),
            Some(Target::Manga("/manga/aria".into()))
        );
        for raw in [
            "https://www.mangahere.cc/manga/",
            "https://www.mangahere.cc/manga/aria/?x=1",
            "https://www.mangahere.cc/manga/aria/c/",
            "https://www.mangahere.cc/manga/aria/v01/",
            "https://www.mangahere.cc/directory/",
            "https://fanfox.net/manga/aria/c001/1.html",
            "https://www.mangahere.cc.evil.test/manga/aria/",
            "https://x.mangahere.cc/manga/aria/",
            "https://www.mangahere.cc:8443/manga/aria/",
            "https://u:p@www.mangahere.cc/manga/aria/",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn mobile(n: u32) -> String {
        format!(
            r#"<select><option value="/manga/dongguo_xiaojie/c004.2/1.html">1</option><option value="/manga/dongguo_xiaojie/c004.2/3.html">3</option></select>
<script>var series_id = 11926; var chapter_id = 455678;</script>
<script>{{"name":"Dongguo &amp; Xiaojie"}}</script>
<img src="//zjcdn.mangahere.org/store/manga/11926/004.2/compressed/p{n:03}.jpg?token=a&amp;ttl=1" />
<img class="x" src="//zjcdn.mangahere.org/store/manga/11926/004.2/compressed/p{:03}.jpg" />"#,
            n + 1
        )
    }

    #[tokio::test]
    async fn extracts_chapter_pages_in_pairs() {
        let first = mobile(1);
        let title_page = r#"<a href="/manga/dongguo_xiaojie/c004.2/">x</a>
<a href="/manga/dongguo_xiaojie/c004.2/" title="Dongguo Xiaojie 4.2">"#;
        let (root, requests) = serve(vec![html(&first), html(title_page), html(&mobile(3))]);
        let http = http("MangaHere", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.mangahere.cc/manga/dongguo_xiaojie/c004.2/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 3);
        assert_eq!(
            files[0].url.as_str(),
            "https://zjcdn.mangahere.org/store/manga/11926/004.2/compressed/p001.jpg?token=a&ttl=1"
        );
        assert_eq!(files[0].headers["Referer"], "https://m.mangahere.cc/");
        let k = kw(&files[2]);
        assert_eq!(k["manga"], "Dongguo & Xiaojie");
        assert_eq!(k["manga_id"], 11926);
        assert_eq!(k["chapter_id"], 455678);
        assert_eq!(k["chapter"], 4);
        assert_eq!(k["chapter_minor"], ".2");
        assert_eq!(k["volume"], 0);
        assert_eq!(k["count"], 3);
        assert_eq!(k["title"], "Dongguo Xiaojie 4.2");
        assert_eq!(k["page"], 3);
        assert_eq!(k["filename"], "p003");
        assert_eq!(
            path(&files[2]),
            "mangahere/Dongguo & Xiaojie/c004.2: Dongguo Xiaojie 4.2/Dongguo & Xiaojie_c004.2_003.jpg"
        );
        assert_eq!(
            archive(&files[2]),
            r#"["{manga}_{chapter}{chapter_minor}_{page}",["Dongguo & Xiaojie","4",".2","3"]]"#
        );
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /manga/dongguo_xiaojie/c004.2/1.html "));
        assert!(requests[0].contains("referer: https://m.mangahere.cc/"));
        assert!(requests[1].starts_with("GET /manga/dongguo_xiaojie/c004.2/ "));
        assert!(requests[2].starts_with("GET /manga/dongguo_xiaojie/c004.2/3.html "));
    }

    #[test]
    fn parses_chapter_list() {
        let page = r#"<meta name="og:title" content="Aria &amp; Co">
<ul id="chapterlist">
<a href="/manga/aria/v12/c060.5/1.html"><p class="title3">Vol.12 Ch.060.5 - Special &amp; Navigation</p><p class="title2">Jan 15,2022</p></a>
<a href="/manga/aria/c001/1.html"><p class="title3">Ch.001</p><p class="title2">Mar 1,2010</p></a>
<a href="/manga/aria/c002.1/"><p class="title3">Extra</p><p class="title2">Today</p></a>
</ul><div class="detail-main-list-more">"#;
        let chapters = chapters(page);
        assert_eq!(chapters.len(), 3);
        assert_eq!(
            chapters[0].0.as_str(),
            "https://www.mangahere.cc/manga/aria/v12/c060.5/1.html"
        );
        let k = &chapters[0].1;
        assert_eq!(k["manga"], "Aria & Co");
        assert_eq!(k["volume"], 12);
        assert_eq!(k["chapter"], 60);
        assert_eq!(k["chapter_minor"], ".5");
        assert_eq!(k["title"], "Special & Navigation");
        assert_eq!(k["date"], "Jan 15,2022");
        assert_eq!(chapters[1].1["chapter"], 1);
        assert_eq!(chapters[1].1["chapter_minor"], "");
        assert_eq!(chapters[1].1["title"], "");
        assert_eq!(chapters[2].1["chapter"], 2);
        assert_eq!(chapters[2].1["chapter_minor"], ".1");
        assert_eq!(chapters[2].1["volume"], 0);
    }

    #[tokio::test]
    async fn manga_queues_chapters_oldest_first() {
        let page = r#"<meta name="og:title" content="Aria"><div id="chapterlist">
<a href="/manga/aria/c002/1.html"><p class="title3">Ch.002</p><p class="title2">b</p></a>
<a href="/manga/aria/c001/1.html"><p class="title3">Ch.001</p><p class="title2">a</p></a>
</div><div class="detail-main-list-more">"#;
        let empty = "<select><option>0</option></select>";
        let (root, requests) = serve(vec![
            html(page),
            html(empty),
            html(""),
            html(empty),
            html(""),
        ]);
        let http = http("MangaHere", root);
        let files = media(extract(
            &http,
            Url::parse("https://m.mangahere.cc/manga/aria/").unwrap(),
        ))
        .await;
        assert!(files.is_empty());
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /manga/aria/ "));
        assert!(requests[0].contains("isAdult=1"));
        assert!(requests[1].starts_with("GET /manga/aria/c001/1.html "));
        assert!(requests[3].starts_with("GET /manga/aria/c002/1.html "));
    }

    #[tokio::test]
    async fn chapter_without_selector_is_an_error() {
        let (root, requests) = serve(vec![html("<html></html>")]);
        let http = http("MangaHere", root);
        let events = extract(
            &http,
            Url::parse("https://www.mangahere.cc/manga/a/c001/").unwrap(),
        );
        assert!(crate::sites::testing::fails(events).await);
        requests.join().unwrap();
    }
}
