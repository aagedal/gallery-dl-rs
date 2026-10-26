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
}

fn target(url: &Url) -> Option<Target> {
    let chapter = re!(r"^https?://(?:www\.)?hentaihere\.com/m/S(\d+)/([^/?#]+)");
    if let Some(caps) = captures(chapter, url) {
        return Some(Target::Chapter(
            group(&caps, 1).into(),
            group(&caps, 2).into(),
        ));
    }
    let manga = re!(r"^https?://(?:www\.)?hentaihere\.com(/m/S\d+)/?$");
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
                let data = chapter_metadata(&page, &manga_id, &chapter)?;
                let images: Vec<String> =
                    serde_json::from_str(extr(&page, "var rff_imageList = ", ";"))
                        .map_err(|_| error("HentaiHere chapter has no image list"))?;
                let pages = images
                    .into_iter()
                    .map(|part| (format!("https://hentaicdn.com/hentai{part}"), None))
                    .collect();
                crate::sites::events(Gallery::chapter(&CHAPTER, data, pages).events(&SITE, http))
            }
            Target::Manga(path) => {
                let page = super::fetch(http, &format!("{ROOT}{path}")).await?;
                let manga_id = parse_int(
                    path.trim_end_matches('/')
                        .rsplit('/')
                        .next()
                        .unwrap_or_default()
                        .get(1..)
                        .unwrap_or_default(),
                );
                super::queue_chapters(http, &SITE, &MANGA, chapters(&page, manga_id), true)
            }
        })
    })
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
            "hentaihere/Uchi no Jimuin/c001: Chapter One/Uchi no Jimuin_c001_001.jpg"
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
