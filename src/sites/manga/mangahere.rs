//! MangaHere chapters and manga, mirroring gallery_dl/extractor/mangahere.py.
use super::{Gallery, english, set};
use crate::error;
use crate::sites::gdl::{Scan, parse_int, partition, rpartition, unescape};
use crate::sites::{Events, Http, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://www.mangahere.cc";
const ROOT_MOBILE: &str = "https://m.mangahere.cc";
const HOSTS: &[&str] = &[
    "mangahere.cc",
    "mangahere.co",
    "www.mangahere.cc",
    "www.mangahere.co",
    "m.mangahere.cc",
    "m.mangahere.co",
];

static CHAPTER: Route = super::chapter_route();
static MANGA: Route = super::manga_route();

pub static SITE: Site = Site {
    category: "mangahere",
    name: "MangaHere",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&CHAPTER, &MANGA],
    api_key: false,
    cookies: Some("mangahere.cc"),
};

#[derive(Debug, PartialEq)]
enum Target {
    /// `title[/vNN]/cXXX`, volume and chapter.
    Chapter {
        part: String,
        volume: String,
        chapter: String,
    },
    /// `/manga/title[/]`.
    Manga(String),
}

fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) || !HOSTS.contains(&url.host_str()?) {
        return None;
    }
    let path = url.path().strip_prefix("/manga/")?;
    let parts: Vec<&str> = path.split('/').collect();
    let chapter = |seg: &str| {
        seg.strip_prefix('c')
            .filter(|c| !c.is_empty())
            .map(str::to_owned)
    };
    let volume = |seg: &str| {
        let digits = seg.strip_prefix('v')?;
        crate::sites::gdl::digits(digits).then(|| digits.trim_start_matches('0').to_owned())
    };
    match parts.as_slice() {
        [title, vol, ch, ..]
            if !title.is_empty() && volume(vol).is_some() && chapter(ch).is_some() =>
        {
            Some(Target::Chapter {
                part: format!("{title}/{vol}/{ch}"),
                volume: volume(vol)?,
                chapter: chapter(ch)?,
            })
        }
        [title, ch, ..] if !title.is_empty() && chapter(ch).is_some() => Some(Target::Chapter {
            part: format!("{title}/{ch}"),
            volume: String::new(),
            chapter: chapter(ch)?,
        }),
        [title] | [title, ""] if !title.is_empty() && url.query().is_none() => {
            Some(Target::Manga(format!("/manga/{path}")))
        }
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported MangaHere URL"));
    };
    crate::sites::once(async move {
        Ok(match target {
            Target::Chapter {
                part,
                volume,
                chapter,
            } => {
                let referer = format!("{ROOT_MOBILE}/");
                let headers = [("Referer", referer.as_str())];
                let base = format!("{ROOT_MOBILE}/manga/{part}/");
                let page = http
                    .text_with(&http.api(&format!("{base}1.html"))?, &headers)
                    .await?;
                let index = page
                    .find("</select>")
                    .ok_or_else(|| error("MangaHere chapter has no page selector"))?;
                let title_page = http
                    .text_with(&http.api(&format!("{ROOT}/manga/{part}/"))?, &headers)
                    .await?;
                let data = chapter_metadata(&page, index, &part, &title_page, &volume, &chapter);
                let count = usize::try_from(data["count"].as_i64().unwrap_or(0)).unwrap_or(0);
                let pages = super::paired_pages(http, &base, page, count, &headers).await?;
                crate::sites::events(
                    Gallery::chapter(&CHAPTER, data, pages)
                        .header("Referer", referer.as_str())
                        .events(&SITE, http),
                )
            }
            Target::Manga(path) => {
                http.set_cookie(
                    &http.api(ROOT)?,
                    "isAdult=1; Domain=www.mangahere.cc; Path=/",
                );
                let page = super::fetch(http, &format!("{ROOT}{path}")).await?;
                super::queue_chapters(http, &SITE, &MANGA, chapters(&page), true)
            }
        })
    })
}

fn chapter_metadata(
    page: &str,
    index: usize,
    part: &str,
    title_page: &str,
    volume: &str,
    chapter: &str,
) -> Map<String, Value> {
    let mut scan = Scan::at(page, index.saturating_sub(40));
    let count = scan.extr(">", "<");
    let manga_id = scan.extr("series_id = ", ";");
    let chapter_id = scan.extr("chapter_id = ", ";");
    let manga = scan.extr("\"name\":\"", "\"");
    let (number, dot, minor) = partition(chapter, ".");
    let mut data = Map::new();
    set(&mut data, "manga", unescape(manga));
    set(&mut data, "manga_id", parse_int(manga_id));
    set(&mut data, "title", title(title_page, part));
    set(&mut data, "volume", parse_int(volume));
    set(&mut data, "chapter", parse_int(number));
    set(&mut data, "chapter_minor", format!("{dot}{minor}"));
    set(&mut data, "chapter_id", parse_int(chapter_id));
    set(&mut data, "count", parse_int(count));
    english(&mut data);
    data
}

/// `_get_title`: the `title` attribute after the second mention of `part`.
fn title(page: &str, part: &str) -> String {
    let Some(first) = page.find(part).map(|i| i + part.len()) else {
        return String::new();
    };
    let Some(second) = page[first..].find(part).map(|i| first + i + part.len()) else {
        return String::new();
    };
    Scan::at(page, second).extr(" title=\"", "\"").to_owned()
}

/// `(?:Vol\.0*(\d+) )?Ch\.0*(\d+)(\S*)(?: - (.*))?` at the start of `info`.
fn chapter_info(info: &str) -> Option<(String, String, String, String)> {
    let mut rest = info;
    let mut volume = String::new();
    if let Some(after) = rest.strip_prefix("Vol.") {
        let digits = after.trim_start_matches('0');
        let len = digits.bytes().take_while(u8::is_ascii_digit).count();
        // `0*(\d+)` backtracks so at least one digit remains.
        let (vol, tail) = if len > 0 {
            (&digits[..len], &digits[len..])
        } else if after.starts_with('0') {
            ("0", digits)
        } else {
            ("", after)
        };
        if let (false, Some(tail)) = (vol.is_empty(), tail.strip_prefix(' ')) {
            volume = vol.to_owned();
            rest = tail;
        }
    }
    let after = rest.strip_prefix("Ch.")?;
    let digits = after.trim_start_matches('0');
    let len = digits.bytes().take_while(u8::is_ascii_digit).count();
    let (chapter, tail) = if len > 0 {
        (&digits[..len], &digits[len..])
    } else if after.starts_with('0') {
        ("0", digits)
    } else {
        return None;
    };
    let minor_len = tail.find(char::is_whitespace).unwrap_or(tail.len());
    let minor = &tail[..minor_len];
    let title = tail[minor_len..].strip_prefix(" - ").unwrap_or_default();
    Some((volume, chapter.into(), minor.into(), title.into()))
}

fn chapters(page: &str) -> Vec<(Url, Map<String, Value>)> {
    let mut scan = Scan::new(page);
    let manga = unescape(scan.extr("<meta name=\"og:title\" content=\"", "\""));
    let list =
        Scan::at(page, scan.pos()).extr("id=\"chapterlist\"", "class=\"detail-main-list-more\"");
    let mut scan = Scan::new(list);
    let mut results = Vec::new();
    while let Some(href) = scan.find(" href=\"", "\"").filter(|h| !h.is_empty()) {
        let info = scan.extr("class=\"title3\">", "<");
        let date = scan.extr("class=\"title2\">", "<");
        let (volume, chapter, minor, title) = chapter_info(info).unwrap_or_else(|| {
            let trimmed = &href[..href.len().saturating_sub(1)];
            let (chapter, _, minor) = partition(rpartition(trimmed, "/c").2, ".");
            (
                String::new(),
                chapter.into(),
                format!(".{minor}"),
                String::new(),
            )
        });
        let mut data = Map::new();
        set(&mut data, "manga", manga.as_str());
        set(
            &mut data,
            "title",
            if title.is_empty() {
                String::new()
            } else {
                unescape(&title)
            },
        );
        set(&mut data, "volume", parse_int(&volume));
        set(&mut data, "chapter", parse_int(&chapter));
        set(&mut data, "chapter_minor", minor);
        set(&mut data, "date", date);
        english(&mut data);
        if let Ok(url) = Url::parse(ROOT).and_then(|root| root.join(href)) {
            results.push((url, data));
        }
    }
    results
}

#[cfg(test)]
mod tests {
    use super::super::tests::path;
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, serve};

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
            t("http://m.mangahere.co/manga/x/v03/c010/1.html"),
            Some(Target::Chapter {
                part: "x/v03/c010".into(),
                volume: "3".into(),
                chapter: "010".into()
            })
        );
        assert_eq!(
            t("https://www.mangahere.cc/manga/hiyokoi/#50"),
            Some(Target::Manga("/manga/hiyokoi/".into()))
        );
        assert_eq!(t("https://www.mangahere.cc/manga/aria/?x=1"), None);
        assert_eq!(t("https://www.mangahere.com/manga/aria/"), None);
    }

    #[test]
    fn parses_chapter_info() {
        assert_eq!(
            chapter_info("Vol.01 Ch.004.2 - Hello"),
            Some(("1".into(), "4".into(), ".2".into(), "Hello".into()))
        );
        assert_eq!(
            chapter_info("Ch.000"),
            Some((String::new(), "0".into(), String::new(), String::new()))
        );
        assert_eq!(chapter_info("Extra"), None);
    }

    #[tokio::test]
    async fn extracts_paired_pages() {
        let first = r#"<select><option>1</option><option data-a="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa">3</option></select>
            <script>var series_id = 9; var chapter_id = 77; {"name":"Dongguo &amp; Xiaojie"}</script>
            <img src="//l.mangahere.cc/1.jpg?a=1&amp;b=2"><img data-x src="//l.mangahere.cc/2.jpg">"#;
        let title = r#"<a href="/manga/dongguo_xiaojie/c004.2/">x</a>
            <a href="/manga/dongguo_xiaojie/c004.2/" title="The Title">"#;
        let third = r#"<img src="https://l.mangahere.cc/3.jpg">"#;
        let (root, server) = serve(vec![html(first), html(title), html(third)]);
        let http = http("mangahere", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.mangahere.cc/manga/dongguo_xiaojie/c004.2/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 3);
        assert_eq!(
            files[0].url.as_str(),
            "https://l.mangahere.cc/1.jpg?a=1&b=2"
        );
        let k = keywords(&files[2]);
        assert_eq!(k["count"], 3);
        assert_eq!(k["manga"], "Dongguo & Xiaojie");
        assert_eq!(k["title"], "The Title");
        assert_eq!(k["chapter_id"], 77);
        assert_eq!(
            path(&files[2]),
            "mangahere/Dongguo & Xiaojie/c004.2: The Title/Dongguo & Xiaojie_c004.2_003.jpg"
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /manga/dongguo_xiaojie/c004.2/1.html "));
        assert!(
            requests[0]
                .to_ascii_lowercase()
                .contains("referer: https://m.mangahere.cc/")
        );
        assert!(requests[1].starts_with("GET /manga/dongguo_xiaojie/c004.2/ "));
        assert!(requests[2].starts_with("GET /manga/dongguo_xiaojie/c004.2/3.html "));
    }

    #[test]
    fn lists_chapters() {
        let page = r#"<meta name="og:title" content="Aria"><ul id="chapterlist">
            <a href="/manga/aria/v02/c010/1.html"><p class="title3">Vol.02 Ch.010 - End</p><p class="title2">Mar 1,2020</p></a>
            <a href="/manga/aria/c009.5/1.html"><p class="title3">Special</p><p class="title2">Feb 1,2020</p></a>
            </ul><div class="detail-main-list-more">"#;
        let list = chapters(page);
        assert_eq!(list.len(), 2);
        assert_eq!(
            list[0].0.as_str(),
            "https://www.mangahere.cc/manga/aria/v02/c010/1.html"
        );
        assert_eq!(list[0].1["volume"], 2);
        assert_eq!(list[0].1["title"], "End");
        assert_eq!(list[0].1["date"], "Mar 1,2020");
        // Fallback: the URL without its last character, after "/c".
        assert_eq!(list[1].1["chapter"], 9);
        assert_eq!(list[1].1["chapter_minor"], ".5/1.htm");
    }
}
