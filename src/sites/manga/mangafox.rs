//! Fanfox (MangaFox) chapters and manga, mirroring
//! gallery_dl/extractor/mangafox.py.
use super::{Gallery, english, set};
use crate::error;
use crate::sites::gdl::{Scan, date, extr, parse_int, remove_html, split_html, strptime, unescape};
use crate::sites::{Events, Http, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://m.fanfox.net";
const HOSTS: &[&str] = &["fanfox.net", "mangafox.me"];

static CHAPTER: Route = super::chapter_route();
static MANGA: Route = super::manga_route();

pub static SITE: Site = Site {
    category: "mangafox",
    name: "Fanfox",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&CHAPTER, &MANGA],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
struct Chapter {
    /// `/manga/title/[vX/]cN...`
    base: String,
    cstr: String,
    volume: String,
    chapter: String,
    minor: String,
}

#[derive(Debug, PartialEq)]
enum Target {
    Chapter(Chapter),
    Manga(String),
}

fn host_ok(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host
        .strip_prefix("www.")
        .or_else(|| host.strip_prefix("m."))
        .unwrap_or(host);
    crate::sites::plain(url) && HOSTS.contains(&host)
}

/// `(/manga/[^/?#]+/((?:v([^/?#]+)/)?c(\d+)([^/?#]*)))`.
fn chapter_target(url: &Url) -> Option<Chapter> {
    if !host_ok(url) {
        return None;
    }
    let parts = crate::sites::gdl::path_parts(url);
    let ["manga", title, rest @ ..] = parts.as_slice() else {
        return None;
    };
    if title.is_empty() {
        return None;
    }
    let chapter = |seg: &str| {
        let tail = seg.strip_prefix('c')?;
        let len = tail.bytes().take_while(u8::is_ascii_digit).count();
        (len > 0).then(|| (tail[..len].to_owned(), tail[len..].to_owned()))
    };
    let (volume, seg) = match rest {
        [vol, seg, ..] if vol.len() > 1 && vol.starts_with('v') && chapter(seg).is_some() => {
            (vol[1..].to_owned(), *seg)
        }
        [seg, ..] if chapter(seg).is_some() => (String::new(), *seg),
        _ => return None,
    };
    let (number, minor) = chapter(seg)?;
    let cstr = if volume.is_empty() {
        seg.to_owned()
    } else {
        format!("v{volume}/{seg}")
    };
    Some(Chapter {
        base: format!("/manga/{title}/{cstr}"),
        cstr,
        volume,
        chapter: number,
        minor,
    })
}

fn target(url: &Url) -> Option<Target> {
    if let Some(chapter) = chapter_target(url) {
        return Some(Target::Chapter(chapter));
    }
    if !host_ok(url) || url.query().is_some() {
        return None;
    }
    match crate::sites::gdl::path_parts(url).as_slice() {
        ["manga", title] | ["manga", title, ""] if !title.is_empty() => {
            Some(Target::Manga(format!("/manga/{title}")))
        }
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Fanfox URL"));
    };
    crate::sites::once(async move {
        Ok(match target {
            Target::Chapter(chapter) => {
                let base = format!("{ROOT}{}/", chapter.base);
                let page = super::fetch(http, &format!("{base}1.html")).await?;
                let data = chapter_metadata(&page, &chapter);
                let count = usize::try_from(data["count"].as_i64().unwrap_or(0)).unwrap_or(0);
                let pages = super::paired_pages(http, &base, page, count, &[]).await?;
                crate::sites::events(Gallery::chapter(&CHAPTER, data, pages).events(&SITE, http))
            }
            Target::Manga(path) => {
                let page = super::fetch(http, &format!("{ROOT}{path}")).await?;
                super::queue_chapters(http, &SITE, &MANGA, chapters(&page), true)
            }
        })
    })
}

fn chapter_metadata(page: &str, chapter: &Chapter) -> Map<String, Value> {
    let mut scan = Scan::new(page);
    let manga = scan.extr("<title>", "</title>");
    let select = page[scan.pos()..]
        .find("</select>")
        .map_or(-1, |i| (scan.pos() + i) as i64)
        - 40;
    // Python slices from the end for negative positions.
    let start = if select < 0 {
        (page.len() as i64 + select).max(0) as usize
    } else {
        select as usize
    };
    let mut scan = Scan::at(page, start);
    let count = scan.extr(">", "<");
    let sid = scan.extr("var series_id =", ";");
    let cid = scan.extr("var chapter_id =", ";");
    let mut data = Map::new();
    set(&mut data, "manga", unescape(manga));
    set(&mut data, "volume", parse_int(&chapter.volume));
    set(&mut data, "chapter", parse_int(&chapter.chapter));
    set(&mut data, "chapter_minor", chapter.minor.as_str());
    set(&mut data, "chapter_string", chapter.cstr.as_str());
    set(&mut data, "count", parse_int(count));
    set(&mut data, "sid", parse_int(sid));
    set(&mut data, "cid", parse_int(cid));
    data
}

fn chapters(page: &str) -> Vec<(Url, Map<String, Value>)> {
    let mut scan = Scan::new(page);
    let manga = scan.extr("<p class=\"title\">", "</p>");
    let author = scan.extr("<p>Author(s):", "</p>");
    scan.extr("<dd class=\"chlist\">", "");
    let section = extr(page, "<div class=\"manga-genres\">", "</section>");
    let (genres, _, summary) =
        crate::sites::gdl::partition(section, "<div class=\"manga-summary\">");
    let mut data = Map::new();
    set(&mut data, "manga", unescape(manga));
    set(&mut data, "author", remove_html(author));
    set(&mut data, "description", unescape(&remove_html(summary)));
    set(&mut data, "tags", split_html(genres));
    english(&mut data);
    let mut results = Vec::new();
    loop {
        let raw = format!("https://{}", scan.extr("<a href=\"//", "\""));
        let Some(url) = Url::parse(&raw).ok() else {
            break;
        };
        let Some(chapter) = chapter_target(&url) else {
            break;
        };
        let mut item = Map::new();
        set(&mut item, "volume", parse_int(&chapter.volume));
        set(&mut item, "chapter", parse_int(&chapter.chapter));
        set(&mut item, "chapter_minor", chapter.minor);
        set(&mut item, "chapter_string", chapter.cstr);
        set(
            &mut item,
            "date",
            date(strptime(scan.extr("right\">", "</span>"), "%b %d, %Y")),
        );
        item.extend(data.clone());
        results.push((url, item));
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
            t("http://fanfox.net/manga/kidou_keisatsu_patlabor/v05/c006.2/1.html"),
            Some(Target::Chapter(Chapter {
                base: "/manga/kidou_keisatsu_patlabor/v05/c006.2".into(),
                cstr: "v05/c006.2".into(),
                volume: "05".into(),
                chapter: "006".into(),
                minor: ".2".into(),
            }))
        );
        assert!(matches!(
            t("http://fanfox.net/manga/black_clover/vTBD/c295/1.html"),
            Some(Target::Chapter(Chapter { volume, .. })) if volume == "TBD"
        ));
        assert_eq!(
            t("https://m.fanfox.net/manga/sentai_daishikkaku"),
            Some(Target::Manga("/manga/sentai_daishikkaku".into()))
        );
        assert_eq!(
            t("https://mangafox.me/manga/shangri_la_frontier/"),
            Some(Target::Manga("/manga/shangri_la_frontier".into()))
        );
        assert_eq!(t("https://fanfox.net/manga/x/reviews"), None);
        assert_eq!(t("https://fanfox.com/manga/x"), None);
    }

    #[tokio::test]
    async fn extracts_chapter() {
        let first = r#"<title>Kidou &amp; Patlabor</title><select><option>1</option>
            <option data-a="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa">2</option></select>
            var series_id = 5; var chapter_id = 6;
            <img src="//zjcdn.mangafox.me/1.jpg"><img class="x" src="//zjcdn.mangafox.me/2.jpg">"#;
        let (root, server) = serve(vec![html(first)]);
        let http = http("mangafox", root);
        let files = media(extract(
            &http,
            Url::parse("http://fanfox.net/manga/kidou/v05/c006.2/1.html").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[1]);
        assert_eq!(k["manga"], "Kidou & Patlabor");
        assert_eq!(k["volume"], 5);
        assert_eq!(k["chapter"], 6);
        assert_eq!(k["chapter_minor"], ".2");
        assert_eq!(k["chapter_string"], "v05/c006.2");
        assert_eq!(k["sid"], 5);
        assert_eq!(k["cid"], 6);
        assert_eq!(
            path(&files[1]),
            "mangafox/Kidou & Patlabor/v05 c006.2/Kidou & Patlabor_c006.2_002.jpg"
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /manga/kidou/v05/c006.2/1.html "));
    }

    #[test]
    fn lists_chapters() {
        let page = r#"<p class="title">Kanojo</p><p>Author(s): <a>Hiroyuki</a></p>
            <div class="manga-genres"><span>Comedy</span><span>Romance</span>
            <div class="manga-summary">A &amp; B</div></section>
            <dd class="chlist">
            <a href="//m.fanfox.net/manga/kanojo/c002/1.html"><span class="right">Jan 05, 2021</span></a>
            <a href="//m.fanfox.net/manga/kanojo/v01/c001.5/1.html"><span class="right">Dec 25, 2020</span></a>
            <a href="//m.fanfox.net/forum">"#;
        let list = chapters(page);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].1["date"], "2021-01-05 00:00:00");
        assert_eq!(list[0].1["tags"], serde_json::json!(["Comedy", "Romance"]));
        assert_eq!(list[0].1["author"], "Hiroyuki");
        assert_eq!(list[0].1["description"], "A & B");
        assert_eq!(list[1].1["volume"], 1);
        assert_eq!(list[1].1["chapter_minor"], ".5");
    }
}
