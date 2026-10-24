//! fanfox.net (MangaFox) chapters and manga, mirroring
//! gallery_dl/extractor/mangafox.py: chapters are read from the mobile site
//! two images per page, like MangaHere.
use super::{Gallery, keywords, text};
use crate::sites::art::{Scan, extr, split_html, strptime};
use crate::sites::{Events, Http, Route, Site, remove_html, unescape};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

pub static SITE: Site = Site {
    category: "mangafox",
    name: "MangaFox",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&CHAPTER, &MANGA],
    api_key: false,
    cookies: None,
};

const ROOT: &str = "https://m.fanfox.net";
/// Pages a chapter may claim; each mobile page holds two images.
const MAX_COUNT: i64 = 5000;

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
    /// `/manga/TITLE/[vVOLUME/]cCHAPTERMINOR`, raw, and its groups.
    Chapter {
        base: String,
        cstr: String,
        volume: String,
        chapter: String,
        minor: String,
    },
    /// `/manga/TITLE`, raw.
    Manga(String),
}

/// `(?:www\.|m\.)?(?:fanfox\.net|mangafox\.me)` with
/// `(/manga/[^/?#]+/((?:v([^/?#]+)/)?c(\d+)([^/?#]*)))` (unanchored at the
/// end) or `(/manga/[^/?#]+)/?$`.
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) {
        return None;
    }
    let host = url.host_str()?;
    let host = host
        .strip_prefix("www.")
        .or_else(|| host.strip_prefix("m."))
        .unwrap_or(host);
    if !matches!(host, "fanfox.net" | "mangafox.me") {
        return None;
    }
    let raw: Vec<&str> = url.path().split('/').skip(1).collect();
    let ["manga", title, rest @ ..] = raw.as_slice() else {
        return None;
    };
    if title.is_empty() {
        return None;
    }
    /// `c(\d+)([^/?#]*)`.
    fn chapter(segment: &str) -> Option<(&str, &str)> {
        let digits = crate::sites::art::leading_digits(segment.strip_prefix('c')?)?;
        Some((digits, &segment[1 + digits.len()..]))
    }
    let found = match rest {
        [volume, second, ..] if volume.len() > 1 && volume.starts_with('v') => {
            chapter(second).map(|c| (&volume[1..], format!("{volume}/{second}"), c))
        }
        _ => None,
    }
    .or_else(|| match rest {
        [first, ..] => chapter(first).map(|c| ("", (*first).to_owned(), c)),
        [] => None,
    });
    match found {
        Some((volume, cstr, (chapter, minor))) => Some(Target::Chapter {
            base: format!("/manga/{title}/{cstr}"),
            volume: volume.to_owned(),
            chapter: chapter.to_owned(),
            minor: minor.to_owned(),
            cstr,
        }),
        None => (matches!(rest, [] | [""]) && url.query().is_none() && url.fragment().is_none())
            .then(|| Target::Manga(format!("/manga/{title}"))),
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Chapter {
            base,
            cstr,
            volume,
            chapter,
            minor,
        }) => crate::sites::once(async move {
            chapter_events(http, &base, &cstr, &volume, &chapter, &minor).await
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
        None => crate::sites::fail(error("unsupported MangaFox URL")),
    }
}

async fn page(http: &Http, url: &str) -> Result<String> {
    http.text_with(&http.api(url)?, &http.browser_headers(&[]))
        .await
}

/// `MangafoxChapterExtractor.metadata`.
fn metadata(
    page: &str,
    cstr: &str,
    volume: &str,
    chapter: &str,
    minor: &str,
) -> Map<String, Value> {
    let (manga, pos) = text::extract(page, "<title>", "</title>", 0);
    // `page.find("</select>", pos) - 40`, where a miss (-1) counts from
    // the end of the page.
    let mut start = match page.get(pos..).and_then(|rest| rest.find("</select>")) {
        Some(found) => (pos + found).saturating_sub(40),
        None => page.len().saturating_sub(41),
    };
    while !page.is_char_boundary(start) {
        start -= 1;
    }
    let (count, pos) = text::extract(page, ">", "<", start);
    let (sid, pos) = text::extract(page, "var series_id =", ";", pos);
    let (cid, _) = text::extract(page, "var chapter_id =", ";", pos);
    keywords([
        ("manga", unescape(manga.unwrap_or_default()).into()),
        ("volume", text::parse_int(volume).into()),
        ("chapter", text::parse_int(chapter).into()),
        ("chapter_minor", minor.into()),
        ("chapter_string", cstr.into()),
        ("count", text::parse_int(count.unwrap_or_default()).into()),
        ("sid", text::parse_int(sid.unwrap_or_default()).into()),
        ("cid", text::parse_int(cid.unwrap_or_default()).into()),
    ])
}

/// `MangafoxChapterExtractor.items`: mobile pages 1, 3, 5, … are fetched
/// until `count` images are known.
async fn chapter_events<'a>(
    http: &'a Http,
    base: &str,
    cstr: &str,
    volume: &str,
    chapter: &str,
    minor: &str,
) -> Result<Events<'a>> {
    let urlbase = format!("{ROOT}{base}");
    let mut current = page(http, &format!("{urlbase}/1.html")).await?;
    let data = metadata(&current, cstr, volume, chapter, minor);
    let count = data
        .get("count")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .min(MAX_COUNT);
    let mut pages = Vec::new();
    let mut number = 1;
    while (pages.len() as i64) < count {
        if number > 1 {
            current = page(http, &format!("{urlbase}/{number}.html")).await?;
        }
        pages.extend(super::mangahere::images(&current, "MangaFox")?.map(|url| (url, None)));
        number += 2;
    }
    let id = data
        .get("cid")
        .map(crate::sites::scalar)
        .unwrap_or_default();
    Ok(crate::sites::events(
        Gallery::chapter(&CHAPTER, data, pages)
            .id(id)
            .events(&SITE, http),
    ))
}

/// `MangafoxMangaExtractor.chapters`, in page order: chapter links until
/// the first that is not a chapter URL.
fn chapters(page: &str) -> Vec<(Url, Map<String, Value>)> {
    let mut e = Scan::new(page);
    let manga = e.extr("<p class=\"title\">", "</p>");
    let author = e.extr("<p>Author(s):", "</p>");
    e.extr("<dd class=\"chlist\">", "");
    let section = extr(page, "<div class=\"manga-genres\">", "</section>");
    let (genres, _, summary) = text::partition(section, "<div class=\"manga-summary\">");
    let data = keywords([
        ("manga", unescape(manga).into()),
        ("author", remove_html(author).into()),
        ("description", unescape(&remove_html(summary)).into()),
        ("tags", split_html(genres).into()),
        ("lang", "en".into()),
        ("language", "English".into()),
    ]);
    let mut results = Vec::new();
    loop {
        let raw = format!("https://{}", e.extr("<a href=\"//", "\""));
        let Some(url) = Url::parse(&raw).ok() else {
            return results;
        };
        let Some(Target::Chapter {
            cstr,
            volume,
            chapter,
            minor,
            ..
        }) = target(&url)
        else {
            return results;
        };
        let date = strptime(e.extr("right\">", "</span>"), "%b %d, %Y");
        let mut chapter = keywords([
            ("volume", text::parse_int(&volume).into()),
            ("chapter", text::parse_int(&chapter).into()),
            ("chapter_minor", minor.into()),
            ("chapter_string", cstr.into()),
            ("date", text::date(date)),
        ]);
        chapter.extend(data.clone());
        results.push((url, chapter));
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
            t("https://fanfox.net/manga/kidou_keisatsu_patlabor/v05/c006.2/1.html"),
            Some(Target::Chapter {
                base: "/manga/kidou_keisatsu_patlabor/v05/c006.2".into(),
                cstr: "v05/c006.2".into(),
                volume: "05".into(),
                chapter: "006".into(),
                minor: ".2".into()
            })
        );
        assert_eq!(
            t("http://m.mangafox.me/manga/title/c001/"),
            Some(Target::Chapter {
                base: "/manga/title/c001".into(),
                cstr: "c001".into(),
                volume: String::new(),
                chapter: "001".into(),
                minor: String::new()
            })
        );
        assert_eq!(
            t("https://www.fanfox.net/manga/title/vTBD/c010/"),
            Some(Target::Chapter {
                base: "/manga/title/vTBD/c010".into(),
                cstr: "vTBD/c010".into(),
                volume: "TBD".into(),
                chapter: "010".into(),
                minor: String::new()
            })
        );
        assert_eq!(
            t("https://fanfox.net/manga/kanojo_mo_kanojo"),
            Some(Target::Manga("/manga/kanojo_mo_kanojo".into()))
        );
        assert_eq!(
            t("https://m.fanfox.net/manga/kanojo_mo_kanojo/"),
            Some(Target::Manga("/manga/kanojo_mo_kanojo".into()))
        );
        for raw in [
            "https://fanfox.net/manga/kanojo_mo_kanojo/?x=1",
            "https://fanfox.net/manga/kanojo_mo_kanojo/#top",
            "https://fanfox.net/manga/kanojo_mo_kanojo/v01/",
            "https://fanfox.net/manga/",
            "https://fanfox.net/directory/",
            "https://www.mangahere.cc/manga/aria/c001/",
            "https://x.fanfox.net/manga/title/c001/",
            "https://fanfox.net.evil.test/manga/title/c001/",
            "https://fanfox.net:8443/manga/title/c001/",
            "https://u:p@fanfox.net/manga/title/c001/",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn mobile(n: u32) -> String {
        format!(
            r#"<img src="//zjcdn.mangafox.me/store/manga/246/05-006.2/compressed/p{n:03}.jpg?token=a&amp;ttl=1" />
<img class="x" src="//zjcdn.mangafox.me/store/manga/246/05-006.2/compressed/p{:03}.jpg" />"#,
            n + 1
        )
    }

    #[tokio::test]
    async fn extracts_chapter_pages_in_pairs() {
        let first = format!(
            r#"<title>Kidou Keisatsu &amp; Patlabor</title>
<select><option value="/manga/kidou_keisatsu_patlabor/v05/c006.2/1.html">1</option><option value="/manga/kidou_keisatsu_patlabor/v05/c006.2/3.html">3</option></select>
<script>var series_id = 246; var chapter_id = 31130;</script>{}"#,
            mobile(1)
        );
        let (root, requests) = serve(vec![html(&first), html(&mobile(3))]);
        let http = http("MangaFox", root);
        let files = media(extract(
            &http,
            Url::parse("https://fanfox.net/manga/kidou_keisatsu_patlabor/v05/c006.2/1.html")
                .unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 3);
        assert_eq!(
            files[0].url.as_str(),
            "https://zjcdn.mangafox.me/store/manga/246/05-006.2/compressed/p001.jpg?token=a&ttl=1"
        );
        let k = kw(&files[2]);
        assert_eq!(k["manga"], "Kidou Keisatsu & Patlabor");
        assert_eq!(k["sid"], 246);
        assert_eq!(k["cid"], 31130);
        assert_eq!(k["volume"], 5);
        assert_eq!(k["chapter"], 6);
        assert_eq!(k["chapter_minor"], ".2");
        assert_eq!(k["chapter_string"], "v05/c006.2");
        assert_eq!(k["count"], 3);
        assert_eq!(k["page"], 3);
        assert!(!k.contains_key("lang"));
        assert_eq!(
            path(&files[2]),
            "mangafox/Kidou Keisatsu & Patlabor/v05 c006.2/Kidou Keisatsu & Patlabor_c006.2_003.jpg"
        );
        assert_eq!(
            archive(&files[2]),
            r#"["{manga}_{chapter}{chapter_minor}_{page}",["Kidou Keisatsu & Patlabor","6",".2","3"]]"#
        );
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /manga/kidou_keisatsu_patlabor/v05/c006.2/1.html "));
        assert!(requests[1].starts_with("GET /manga/kidou_keisatsu_patlabor/v05/c006.2/3.html "));
    }

    const MANGA_PAGE: &str = r#"<p class="title">Kanojo mo Kanojo</p>
<p>Author(s:<a>x</a></p><p>Author(s):<a href="/x">Hiroyuki</a></p>
<div class="manga-genres"><span>Comedy</span><span>Romance</span>
<div class="manga-summary">Nao &amp; <b>Saki</b>.</div></section>
<dd class="chlist">
<a href="//fanfox.net/manga/kanojo_mo_kanojo/v02/c012.5/1.html"><span class="right">Jan 15, 2022</span></a>
<a href="//fanfox.net/manga/kanojo_mo_kanojo/c001/1.html"><span class="right">Today</span></a>
<a href="//fanfox.net/manga/kanojo_mo_kanojo/">x</a>
<a href="//fanfox.net/manga/kanojo_mo_kanojo/c000/1.html"></a>"#;

    #[test]
    fn parses_chapter_list() {
        let chapters = chapters(MANGA_PAGE);
        assert_eq!(chapters.len(), 2);
        assert_eq!(
            chapters[0].0.as_str(),
            "https://fanfox.net/manga/kanojo_mo_kanojo/v02/c012.5/1.html"
        );
        let k = &chapters[0].1;
        assert_eq!(k["manga"], "Kanojo mo Kanojo");
        assert_eq!(k["author"], "Hiroyuki");
        assert_eq!(k["description"], "Nao & Saki .");
        assert_eq!(k["tags"], serde_json::json!(["Comedy", "Romance"]));
        assert_eq!(k["volume"], 2);
        assert_eq!(k["chapter"], 12);
        assert_eq!(k["chapter_minor"], ".5");
        assert_eq!(k["chapter_string"], "v02/c012.5");
        assert_eq!(k["date"], "2022-01-15 00:00:00");
        assert_eq!(chapters[1].1["date"], "[Invalid DateTime]");
        assert_eq!(chapters[1].1["volume"], 0);
    }

    #[tokio::test]
    async fn manga_queues_chapters_oldest_first() {
        let (root, requests) = serve(vec![html(MANGA_PAGE), html(""), html("")]);
        let http = http("MangaFox", root);
        let files = media(extract(
            &http,
            Url::parse("https://fanfox.net/manga/kanojo_mo_kanojo").unwrap(),
        ))
        .await;
        assert!(files.is_empty());
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /manga/kanojo_mo_kanojo "));
        assert!(requests[1].starts_with("GET /manga/kanojo_mo_kanojo/c001/1.html "));
        assert!(requests[2].starts_with("GET /manga/kanojo_mo_kanojo/v02/c012.5/1.html "));
    }
}
