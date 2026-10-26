//! MangaNato family readers (nelomanga, natomanga, manganato, mangakakalot),
//! mirroring gallery_dl/extractor/manganelo.py.
use super::{Gallery, set};
use crate::error;
use crate::sites::gdl::{
    Scan, date, extr, extract_iter, group, parse_int, parse_iso, partition, re, remove_html,
    rpartition, split_html, strptime, unescape,
};
use crate::sites::{Events, Http, Route, Site};
use serde_json::{Map, Value};
use url::Url;

static CHAPTER: Route = super::chapter_route();
static MANGA: Route = super::manga_route();
static BOOKMARK: Route = Route::new("bookmark", "{filename}.{extension}", &["{category}"], "");

macro_rules! instance {
    ($ident:ident, $category:literal, $name:literal, $domain:literal) => {
        pub static $ident: Site = Site {
            category: $category,
            name: $name,
            matches: |url| target($category, url).is_some(),
            extract: |http, url| extract($category, http, url),
            routes: &[&CHAPTER, &MANGA, &BOOKMARK],
            api_key: false,
            cookies: Some($domain),
        };
    };
}
instance!(NELOMANGA, "nelomanga", "NeloManga", "nelomanga.net");
instance!(NATOMANGA, "natomanga", "NatoManga", "natomanga.com");
instance!(MANGANATO, "manganato", "MangaNato", "manganato.gg");
instance!(
    MANGAKAKALOT,
    "mangakakalot",
    "MangaKakalot",
    "mangakakalot.gg"
);

fn site(category: &str) -> &'static Site {
    match category {
        "nelomanga" => &NELOMANGA,
        "natomanga" => &NATOMANGA,
        "manganato" => &MANGANATO,
        _ => &MANGAKAKALOT,
    }
}

fn root(category: &str) -> &'static str {
    match category {
        "nelomanga" => "https://www.nelomanga.net",
        "natomanga" => "https://www.natomanga.com",
        "manganato" => "https://www.manganato.gg",
        _ => "https://www.mangakakalot.gg",
    }
}

#[derive(Debug, PartialEq)]
enum Target {
    Chapter(String),
    Manga(String),
    Bookmark,
}

fn target(category: &str, url: &Url) -> Option<Target> {
    let host = url.host_str()?;
    let domain = &root(category)["https://www.".len()..];
    if !crate::sites::plain(url) || host.strip_prefix("www.").unwrap_or(host) != domain {
        return None;
    }
    let path = url.path();
    let chapter = re!(r"^(/manga/[^/?#]+/chapter-[^/?#]+)");
    if let Some(c) = chapter.captures(path) {
        return Some(Target::Chapter(group(&c, 1).into()));
    }
    let manga = re!(r"^(/manga/[^/?#]+)$");
    if url.query().is_none()
        && url.fragment().is_none()
        && let Some(c) = manga.captures(path)
    {
        return Some(Target::Manga(group(&c, 1).into()));
    }
    path.starts_with("/bookmark").then_some(Target::Bookmark)
}

fn extract<'a>(category: &'static str, http: &'a Http, url: Url) -> Events<'a> {
    let Some(target) = target(category, &url) else {
        return crate::sites::fail(error("unsupported MangaNato URL"));
    };
    let root = root(category);
    let site = site(category);
    match target {
        Target::Chapter(path) => crate::sites::once(async move {
            let page = super::fetch(http, &format!("{root}{path}")).await?;
            let pages = images(&page)?;
            Ok(crate::sites::events(
                Gallery::chapter(&CHAPTER, chapter_metadata(&page), pages).events(site, http),
            ))
        }),
        Target::Manga(path) => crate::sites::once(async move {
            let page = super::fetch(http, &format!("{root}{path}")).await?;
            let (info, slug, base) = manga_info(&page);
            let api = http.api(&format!("{root}/api/manga/{slug}/chapters?limit=-1"))?;
            let data = http.json(&api).await?;
            let chapters = data["data"]["chapters"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|ch| {
                    let slug = crate::sites::gdl::str_of(ch, "chapter_slug");
                    let (chapter, sep, minor) = partition(slug.get(8..).unwrap_or_default(), "-");
                    let mut entry = info.clone();
                    set(&mut entry, "chapter", parse_int(chapter));
                    set(
                        &mut entry,
                        "chapter_minor",
                        if sep.is_empty() {
                            String::new()
                        } else {
                            format!(".{minor}")
                        },
                    );
                    set(
                        &mut entry,
                        "date",
                        date(parse_iso(crate::sites::gdl::str_of(ch, "updated_at"))),
                    );
                    set(&mut entry, "views", ch["view"].clone());
                    Some((Url::parse(&format!("{base}{slug}")).ok()?, entry))
                })
                .collect();
            Ok(super::queue_chapters(http, site, &MANGA, chapters, true))
        }),
        Target::Bookmark => bookmarks(site, root, http),
    }
}

fn chapter_metadata(page: &str) -> Map<String, Value> {
    let mut scan = Scan::new(page);
    let first19 = |text: &str| text.chars().take(19).collect::<String>();
    let mut data = Map::new();
    set(
        &mut data,
        "date",
        date(parse_iso(&first19(
            scan.extr("\"datePublished\": \"", "\""),
        ))),
    );
    set(
        &mut data,
        "date_updated",
        date(parse_iso(&first19(scan.extr("\"dateModified\": \"", "\"")))),
    );
    set(
        &mut data,
        "manga_id",
        parse_int(scan.extr("comic_id =", ";")),
    );
    set(
        &mut data,
        "chapter_id",
        parse_int(scan.extr("chapter_id =", ";")),
    );
    set(
        &mut data,
        "manga",
        scan.extr("comic_name =", ";").trim_matches(['"', ' ']),
    );
    set(&mut data, "lang", "en");
    set(&mut data, "language", "English");
    let name = scan
        .extr("chapter_name =", ";")
        .trim_matches(['"', ' '])
        .to_owned();
    let (chapter, sep, minor) = partition(rpartition(&name, " ").2, ".");
    set(&mut data, "chapter", parse_int(chapter));
    set(&mut data, "chapter_minor", format!("{sep}{minor}"));
    set(
        &mut data,
        "author",
        scan.extr(". Author:", " already has ").trim(),
    );
    data
}

fn images(page: &str) -> crate::Result<Vec<super::Page>> {
    let mut scan = Scan::new(page);
    let invalid = || error("MangaNato chapter has no image list");
    let cdns: Vec<String> =
        serde_json::from_str(scan.extr("var cdns =", ";")).map_err(|_| invalid())?;
    let images: Vec<String> =
        serde_json::from_str(scan.extr("var chapterImages =", ";")).map_err(|_| invalid())?;
    let mut cdn = cdns.into_iter().next().ok_or_else(invalid)?;
    if !cdn.ends_with('/') {
        cdn.push('/');
    }
    Ok(images
        .into_iter()
        .map(|path| (format!("{cdn}{path}"), None))
        .collect())
}

/// The shared manga keywords, the manga slug and the chapter URL base.
fn manga_info(page: &str) -> (Map<String, Value>, String, String) {
    let mut scan = Scan::new(page);
    let url = scan.extr("property=\"og:url\" content=\"", "\"").to_owned();
    let slug = url[url.rfind('/').map_or(0, |i| i + 1)..].to_owned();
    let mut info = Map::new();
    set(&mut info, "manga", unescape(scan.extr("<h1>", "<")));
    set(&mut info, "manga_url", url.as_str());
    set(&mut info, "manga_slug", slug.as_str());
    set(
        &mut info,
        "author",
        remove_html(scan.extr("<li>Author(s) :", "</li>")),
    );
    set(&mut info, "status", scan.extr("<li>Status :", "<").trim());
    set(
        &mut info,
        "date_updated",
        date(strptime(
            scan.extr("<li>Last updated :", "<").trim(),
            "%b-%d-%Y %I:%M:%S %p",
        )),
    );
    let mut tags: Vec<String> = split_html(scan.extr(">Genres :", "</li>"))
        .into_iter()
        .step_by(2)
        .collect();
    tags.sort();
    set(&mut info, "tags", tags);
    set(&mut info, "lang", "en");
    (info, slug, format!("{url}/"))
}

fn bookmarks<'a>(site: &'static Site, root: &'static str, http: &'a Http) -> Events<'a> {
    crate::sites::paginate(Some((1i64, 0i64)), move |state| async move {
        let Some((number, last)) = state else {
            return Ok(None);
        };
        let mut url = http.api(&format!("{root}/bookmark"))?;
        url.query_pairs_mut()
            .append_pair("page", &number.to_string());
        if number == 1 && http.location(&url, &[]).await?.is_some() {
            return Err(error("MangaNato bookmarks require authenticated cookies"));
        }
        let page = http.text(&url).await?;
        let last = if number == 1 {
            parse_int(extr(&page, ">Last(", ")"))
        } else {
            last
        };
        let children = extract_iter(&page, "class=\"user-bookmark-item ", "</a>")
            .into_iter()
            .filter_map(|b| Url::parse(extr(b, " href=\"", "\"")).ok())
            .map(|url| (url, Map::new()))
            .collect();
        let events = super::queue_urls(http, site, &BOOKMARK, children);
        Ok(Some((
            events,
            (number < last).then_some((number + 1, last)),
        )))
    })
}

#[cfg(test)]
mod tests {
    use super::super::tests::{archive, path};
    use super::*;
    use crate::sites::testing::{children, html, http, json, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |c: &str, raw: &str| target(c, &Url::parse(raw).unwrap());
        assert_eq!(
            t(
                "mangakakalot",
                "https://www.mangakakalot.gg/manga/aria/chapter-10.5"
            ),
            Some(Target::Chapter("/manga/aria/chapter-10.5".into()))
        );
        assert_eq!(
            t("natomanga", "https://natomanga.com/manga/aria"),
            Some(Target::Manga("/manga/aria".into()))
        );
        assert_eq!(
            t("natomanga", "https://www.natomanga.com/manga/aria/"),
            None
        );
        assert_eq!(
            t("manganato", "https://www.manganato.gg/bookmark"),
            Some(Target::Bookmark)
        );
        assert_eq!(t("nelomanga", "https://www.manganato.gg/manga/aria"), None);
        assert_eq!(
            crate::sites::find(&Url::parse("https://www.nelomanga.net/manga/x").unwrap())
                .unwrap()
                .category,
            "nelomanga"
        );
    }

    #[tokio::test]
    async fn extracts_chapter() {
        let page = r#""datePublished": "2024-05-01T10:00:00+07:00", "dateModified": "2024-05-02T00:00:00+07:00"
            var comic_id = 123; var chapter_id = 456; var comic_name = "Aria"; var chapter_name = "Chapter 10.5";
            . Author: Amano Kozue already has many
            var cdns = ["https://img.example/"]; var chapterImages = ["a/1.jpg","a/2.jpg"];"#;
        let (root, server) = serve(vec![html(page)]);
        let http = http("mangakakalot", root);
        let files = media(extract(
            "mangakakalot",
            &http,
            Url::parse("https://www.mangakakalot.gg/manga/aria/chapter-10.5").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(files[1].url.as_str(), "https://img.example/a/2.jpg");
        let k = keywords(&files[0]);
        assert_eq!(k["date"], "2024-05-01 10:00:00");
        assert_eq!(k["chapter"], 10);
        assert_eq!(k["chapter_minor"], ".5");
        assert_eq!(k["author"], "Amano Kozue");
        assert_eq!(k["category"], "mangakakalot");
        assert_eq!(
            path(&files[1]),
            "mangakakalot/Aria/c010.5/Aria_c010.5_002.jpg"
        );
        assert_eq!(
            archive(&files[0]),
            r#"["{manga}_{chapter}{chapter_minor}_{page}",["Aria","10",".5","1"]]"#
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn manga_reads_chapter_api() {
        let page = r#"<meta property="og:url" content="https://www.natomanga.com/manga/aria"><h1>Aria</h1>
            <li>Author(s) : <a>Amano</a></li><li>Status : Completed</li><li>Last updated : Jan-02-2024 03:04:05 PM</li>
            <li class="genres">>Genres : <a>Sci-fi</a>, <a>Drama</a></li>"#;
        let (root, server) = serve(vec![
            html(page),
            json(serde_json::json!({"data": {"chapters": [
                {"chapter_slug": "chapter-2-5", "updated_at": "2024-01-01T00:00:00Z", "view": 9},
                {"chapter_slug": "chapter-1", "updated_at": "2023-01-01T00:00:00Z", "view": 1}]}})),
        ]);
        let http = http("natomanga", root);
        assert_eq!(
            children(extract(
                "natomanga",
                &http,
                Url::parse("https://www.natomanga.com/manga/aria").unwrap()
            ))
            .await
            .len(),
            2
        );
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /api/manga/aria/chapters?limit=-1 "));
        let (info, slug, base) = manga_info(page);
        assert_eq!(slug, "aria");
        assert_eq!(base, "https://www.natomanga.com/manga/aria/");
        assert_eq!(info["tags"], serde_json::json!(["Drama", "Sci-fi"]));
        assert_eq!(info["date_updated"], "2024-01-02 15:04:05");
    }
}
