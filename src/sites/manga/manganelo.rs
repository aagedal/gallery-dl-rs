//! MangaKakalot and its mirrors (NeloManga, NatoManga, MangaNato), mirroring
//! gallery_dl/extractor/manganelo.py: chapter pages embed their image list,
//! a manga's chapters come from `/api/manga/SLUG/chapters`.
//!
//! Configured `extractor.manganelo.<category>` instances and `manganelo:URL`
//! inputs go through `crate::sites::instances`. Bookmarks need the session
//! cookies (`--cookies`).
use super::{Gallery, get, keywords, text};
use crate::sites::art::{Scan, extr, extract_iter, json_loads, on_host, segment, split_html};
use crate::sites::{Events, Http, Route, Site, remove_html, unescape};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

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
static BOOKMARK: Route = Route::new("bookmark", "{filename}.{extension}", &["{category}"], "");

macro_rules! instance {
    ($ident:ident, $category:literal, $name:literal, $domain:literal) => {
        pub static $ident: Site = Site {
            category: $category,
            name: $name,
            matches: |url| target($domain, url).is_some(),
            extract: |http, url| extract(&$ident, $domain, http, url),
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

#[derive(Debug, PartialEq)]
enum Target {
    /// `/manga/SLUG/chapter-…`, raw.
    Chapter(String),
    /// `/manga/SLUG`, raw.
    Manga(String),
    Bookmark,
}

/// Configured `extractor.manganelo.<category>` instances.
pub(crate) static FAMILY: crate::sites::instances::Family = crate::sites::instances::Family {
    basecategory: "manganelo",
    builtin: &[&NELOMANGA, &NATOMANGA, &MANGANATO, &MANGAKAKALOT],
    related: &[],
    template: &MANGAKAKALOT,
    matches: |category, url| target_in(instance(category).1, url).is_some(),
    extract: |category, http, url| {
        let (site, hosts) = instance(category);
        extract_in(site, hosts, http, url)
    },
};

/// Which URLs an instance accepts.
#[derive(Clone, Copy)]
enum Hosts {
    /// `(?:www\.)?DOMAIN`, with the root `https://www.DOMAIN`.
    Domain(&'static str),
    /// A configured instance's root and `host[:port][/path]`.
    Configured(&'static str, &'static str),
}

/// The site and hosts of a built-in or configured instance.
fn instance(category: &str) -> (&'static Site, Hosts) {
    if let Some(configured) = crate::sites::instances::configured(&FAMILY, category) {
        return (
            &configured.site,
            Hosts::Configured(configured.root, configured.spec),
        );
    }
    let site = FAMILY
        .builtin
        .iter()
        .copied()
        .find(|site| site.category == category)
        .unwrap_or(&MANGAKAKALOT);
    (site, Hosts::Domain(site.cookies.unwrap_or_default()))
}

fn target(domain: &'static str, url: &Url) -> Option<Target> {
    target_in(Hosts::Domain(domain), url)
}

/// `(?:www\.)?DOMAIN` followed by `(/manga/[^/?#]+/chapter-[^/?#]+)`
/// (unanchored), `(/manga/[^/?#]+)$` or `/bookmark` (unanchored).
fn target_in(hosts: Hosts, url: &Url) -> Option<Target> {
    let (path, bare) = match hosts {
        Hosts::Domain(domain) => {
            if !on_host(url, &[domain]) {
                return None;
            }
            (
                url.path().to_owned(),
                url.query().is_none() && url.fragment().is_none(),
            )
        }
        Hosts::Configured(_, spec) => {
            let rest = crate::sites::instances::below(url, spec)?;
            let bare = !rest.contains('?') && url.fragment().is_none();
            (rest.split('?').next().unwrap_or_default().to_owned(), bare)
        }
    };
    if path.starts_with("/bookmark") {
        return Some(Target::Bookmark);
    }
    let raw: Vec<&str> = path.split('/').skip(1).collect();
    match raw.as_slice() {
        ["manga", slug, chapter, ..]
            if segment(slug)
                && chapter.len() > "chapter-".len()
                && chapter.starts_with("chapter-") =>
        {
            Some(Target::Chapter(format!("/manga/{slug}/{chapter}")))
        }
        // `$`: nothing may follow, not even a query.
        ["manga", slug] if segment(slug) && bare => Some(Target::Manga(format!("/manga/{slug}"))),
        _ => None,
    }
}

fn extract<'a>(site: &'static Site, domain: &'static str, http: &'a Http, url: Url) -> Events<'a> {
    extract_in(site, Hosts::Domain(domain), http, url)
}

fn extract_in<'a>(site: &'static Site, hosts: Hosts, http: &'a Http, url: Url) -> Events<'a> {
    let root = match hosts {
        Hosts::Domain(domain) => format!("https://www.{domain}"),
        Hosts::Configured(root, _) => root.to_owned(),
    };
    match target_in(hosts, &url) {
        Some(Target::Chapter(path)) => crate::sites::once(async move {
            let page = page(http, &format!("{root}{path}")).await?;
            let data = metadata(&page);
            let id = data
                .get("chapter_id")
                .map(crate::sites::scalar)
                .unwrap_or_default();
            Ok(crate::sites::events(
                Gallery::chapter(&CHAPTER, data, images(&page)?)
                    .id(id)
                    .events(site, http),
            ))
        }),
        Some(Target::Manga(path)) => crate::sites::once(async move {
            let page = page(http, &format!("{root}{path}")).await?;
            let mut children = chapters(http, &root, &page).await?;
            // `MangaExtractor.reverse = True`.
            if !http.option_bool("chapter-reverse", false) {
                children.reverse();
            }
            Ok(super::queue_urls(http, site, &MANGA, children))
        }),
        Some(Target::Bookmark) => bookmarks(site, http, root),
        None => crate::sites::fail(error(format!("unsupported {} URL", site.name))),
    }
}

async fn page(http: &Http, url: &str) -> Result<String> {
    http.text_with(&http.api(url)?, &http.browser_headers(&[]))
        .await
}

/// `parse_datetime_iso(value[:19])`.
fn iso19(value: &str) -> Value {
    let end = value.char_indices().nth(19).map_or(value.len(), |(i, _)| i);
    text::date(text::iso(&value[..end]))
}

/// `ManganeloChapterExtractor.metadata`.
fn metadata(page: &str) -> Map<String, Value> {
    let mut scan = Scan::new(page);
    let mut data = keywords([
        ("date", iso19(scan.extr("\"datePublished\": \"", "\""))),
        (
            "date_updated",
            iso19(scan.extr("\"dateModified\": \"", "\"")),
        ),
        (
            "manga_id",
            text::parse_int(scan.extr("comic_id =", ";")).into(),
        ),
        (
            "chapter_id",
            text::parse_int(scan.extr("chapter_id =", ";")).into(),
        ),
        (
            "manga",
            scan.extr("comic_name =", ";")
                .trim_matches(['"', ' '])
                .into(),
        ),
        ("lang", "en".into()),
        ("language", "English".into()),
    ]);
    let name = scan.extr("chapter_name =", ";").trim_matches(['"', ' ']);
    let last = name.rsplit(' ').next().unwrap_or_default();
    let (chapter, minor) = text::chapter(last);
    super::set(&mut data, "chapter", chapter);
    super::set(&mut data, "chapter_minor", minor);
    super::set(
        &mut data,
        "author",
        scan.extr(". Author:", " already has ").trim(),
    );
    data
}

/// `ManganeloChapterExtractor.images`: the first CDN joined with each path.
fn images(page: &str) -> Result<Vec<super::Page>> {
    let mut scan = Scan::new(page);
    let cdns = json_loads(scan.extr("var cdns =", ";"), "Manganelo")?;
    let images = json_loads(scan.extr("var chapterImages =", ";"), "Manganelo")?;
    let mut cdn = cdns
        .get(0)
        .and_then(Value::as_str)
        .ok_or_else(|| error("Manganelo chapter lists no image CDN"))?
        .to_owned();
    if !cdn.ends_with('/') {
        cdn.push('/');
    }
    Ok(images
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .map(|path| (format!("{cdn}{}", crate::sites::scalar(path)), None))
        .collect())
}

/// `ManganeloMangaExtractor.chapters`: the page's manga info and every
/// chapter from the API, in API order.
async fn chapters(http: &Http, root: &str, page: &str) -> Result<Vec<(Url, Map<String, Value>)>> {
    let mut scan = Scan::new(page);
    let url = scan.extr("property=\"og:url\" content=\"", "\"");
    let slug = url.rsplit('/').next().unwrap_or_default();
    let manga = unescape(scan.extr("<h1>", "<"));
    let author = remove_html(scan.extr("<li>Author(s) :", "</li>"));
    let status = scan.extr("<li>Status :", "<").trim();
    let updated = crate::sites::art::strptime(
        scan.extr("<li>Last updated :", "<").trim(),
        "%b-%d-%Y %I:%M:%S %p",
    );
    let mut tags: Vec<String> = split_html(scan.extr(">Genres :", "</li>"))
        .into_iter()
        .step_by(2)
        .collect();
    tags.sort();
    let info = keywords([
        ("manga", manga.into()),
        ("manga_url", url.into()),
        ("manga_slug", slug.into()),
        ("author", author.into()),
        ("status", status.into()),
        ("date_updated", text::date(updated)),
        ("tags", tags.into()),
        ("lang", "en".into()),
    ]);
    let api = http.api(&format!("{root}/api/manga/{slug}/chapters?limit=-1"))?;
    let data = http.json_with(&api, &http.browser_headers(&[])).await?;
    let list = data
        .pointer("/data/chapters")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    Ok(list
        .iter()
        .filter_map(|ch| {
            let slug = super::str_of(ch, "chapter_slug");
            let (chapter, sep, minor) =
                text::partition(slug.get("chapter-".len()..).unwrap_or_default(), "-");
            let mut data = info.clone();
            super::set(&mut data, "chapter", text::parse_int(chapter));
            super::set(
                &mut data,
                "chapter_minor",
                if sep.is_empty() {
                    minor.to_owned()
                } else {
                    format!(".{minor}")
                },
            );
            super::set(
                &mut data,
                "date",
                text::date(text::iso(super::str_of(ch, "updated_at"))),
            );
            super::set(&mut data, "views", get(ch, "view"));
            Some((Url::parse(&format!("{url}/{slug}")).ok()?, data))
        })
        .collect())
}

/// `ManganeloBookmarkExtractor.items`: every bookmarked manga, page by
/// page up to the `Last(N)` link of the first page.
fn bookmarks<'a>(site: &'static Site, http: &'a Http, root: String) -> Events<'a> {
    crate::sites::paginate(Some((1i64, 0i64)), move |state| {
        let root = root.clone();
        async move {
            let Some((number, last)) = state else {
                return Ok(None);
            };
            let mut url = http.api(&format!("{root}/bookmark"))?;
            url.query_pairs_mut()
                .append_pair("page", &number.to_string());
            let (served, page) = http.page(&url, &http.browser_headers(&[])).await?;
            // Upstream `response.history`: a redirect means a login page.
            if served != url {
                return Err(error(format!(
                    "{} bookmarks require authenticated cookies (--cookies)",
                    site.name
                )));
            }
            let last = match number {
                1 => text::parse_int(extr(&page, ">Last(", ")")),
                _ => last,
            };
            let children = extract_iter(&page, "class=\"user-bookmark-item ", "</a>")
                .into_iter()
                .filter_map(|item| Url::parse(extr(item, " href=\"", "\"")).ok())
                .map(|url| (url, Map::new()))
                .collect();
            let next = (number < last).then_some((number + 1, last));
            Ok(Some((
                super::queue_urls(http, site, &BOOKMARK, children),
                next,
            )))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::super::tests::{archive, path};
    use super::*;
    use crate::sites::testing::{html, http, json, keywords as kw, media, queued, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target("mangakakalot.gg", &Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.mangakakalot.gg/manga/kimi/chapter-12-5?x=1"),
            Some(Target::Chapter("/manga/kimi/chapter-12-5".into()))
        );
        assert_eq!(
            t("https://mangakakalot.gg/manga/kimi"),
            Some(Target::Manga("/manga/kimi".into()))
        );
        assert_eq!(
            t("https://www.mangakakalot.gg/bookmark?page=2"),
            Some(Target::Bookmark)
        );
        for raw in [
            "https://www.mangakakalot.gg/manga/kimi/",
            "https://www.mangakakalot.gg/manga/kimi?page=1",
            "https://www.mangakakalot.gg/manga/kimi/chapter-",
            "https://www.mangakakalot.gg/manga/kimi/extra",
            "https://www.mangakakalot.gg/genre/all",
            "https://m.mangakakalot.gg/manga/kimi",
            "https://www.mangakakalot.gg.evil.test/manga/kimi",
            "https://www.mangakakalot.gg:8443/manga/kimi",
            "https://u:p@www.mangakakalot.gg/manga/kimi",
            "https://www.natomanga.com/manga/kimi",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
        let url = Url::parse("https://www.natomanga.com/manga/kimi").unwrap();
        assert_eq!(
            crate::sites::find(&url).map(|site| site.category),
            Some("natomanga")
        );
        let url = Url::parse("https://nelomanga.net/manga/kimi/chapter-1").unwrap();
        assert_eq!(
            crate::sites::find(&url).map(|site| site.category),
            Some("nelomanga")
        );
    }

    const CHAPTER_PAGE: &str = r#"<script type="application/ld+json">{"datePublished": "2024-05-06T07:08:09+07:00", "dateModified": "2024-06-07T08:09:10+07:00"}</script>
<script>var comic_id = 4321; var chapter_id = 98765; var comic_name = "Kimi to Boku";
var chapter_name = "Vol.2 Chapter 12.5";</script>
<p>Read Kimi. Author: Some One already has 3 chapters.</p>
<script>var cdns = ["https:\/\/img.cdn.test\/a","https:\/\/backup.test\/"];
var chapterImages = ["manga/kimi/1.webp","manga/kimi/2.jpg"];</script>"#;

    #[tokio::test]
    async fn configured_instance_uses_its_root() {
        crate::sites::instances::define(
            "manganelo",
            "manganelo-configured",
            serde_json::json!({"root": "https://www.nelo.test", "pattern": r"(?:www\.)?nelo\.test"}),
        );
        let url = Url::parse("https://nelo.test/manga/kimi/chapter-12-5").unwrap();
        let site = crate::sites::find(&url).unwrap();
        assert_eq!(site.category, "manganelo-configured");
        assert_eq!(site.cookies, Some("www.nelo.test"));
        assert!(
            crate::sites::find(&Url::parse("https://www.nelo.test/manga/kimi?x=1").unwrap())
                .is_none()
        );
        let (root, requests) = serve(vec![html(CHAPTER_PAGE)]);
        let http = http("manganelo-configured", root);
        let files = media((site.extract)(&http, url)).await;
        assert_eq!(files.len(), 2);
        assert_eq!(kw(&files[0])["category"], "manganelo-configured");
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /manga/kimi/chapter-12-5 "));
        assert!(requests[0].contains("host: "));
    }

    #[tokio::test]
    async fn extracts_chapter() {
        let (root, requests) = serve(vec![html(CHAPTER_PAGE)]);
        let http = http("MangaKakalot", root);
        let files = media(extract(
            &MANGAKAKALOT,
            "mangakakalot.gg",
            &http,
            Url::parse("https://www.mangakakalot.gg/manga/kimi/chapter-12-5").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].url.as_str(),
            "https://img.cdn.test/a/manga/kimi/1.webp"
        );
        let k = kw(&files[1]);
        assert_eq!(k["category"], "mangakakalot");
        assert_eq!(k["manga"], "Kimi to Boku");
        assert_eq!(k["manga_id"], 4321);
        assert_eq!(k["chapter_id"], 98765);
        assert_eq!(k["chapter"], 12);
        assert_eq!(k["chapter_minor"], ".5");
        assert_eq!(k["author"], "Some One");
        assert_eq!(k["date"], "2024-05-06 07:08:09");
        assert_eq!(k["date_updated"], "2024-06-07 08:09:10");
        assert_eq!(k["page"], 2);
        assert_eq!(
            path(&files[1]),
            "mangakakalot/Kimi to Boku/c012.5/Kimi to Boku_c012.5_002.jpg"
        );
        assert_eq!(
            archive(&files[1]),
            r#"["{manga}_{chapter}{chapter_minor}_{page}",["Kimi to Boku","12",".5","2"]]"#
        );
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /manga/kimi/chapter-12-5 "));
    }

    const MANGA_PAGE: &str = r#"<meta property="og:url" content="https://www.natomanga.com/manga/kimi-boku" />
<h1>Kimi &amp; Boku</h1>
<li>Author(s) : <a href="/author/x">Some One</a></li>
<li>Status : Ongoing </li>
<li>Last updated : Jan-02-2025 03:04:05 PM</li>
<li class="genres">Genres : <a>Romance</a>, <a>Action</a>, <a>Comedy</a></li>"#;

    #[tokio::test]
    async fn manga_queues_api_chapters_oldest_first() {
        let api = serde_json::json!({"data": {"chapters": [
            {"chapter_slug": "chapter-2-5", "updated_at": "2025-01-02T03:04:05", "view": 77},
            {"chapter_slug": "chapter-1", "updated_at": "2024-12-01T00:00:00", "view": 5},
        ]}});
        let (root, requests) = serve(vec![html(MANGA_PAGE), json(api)]);
        let http = http("NatoManga", root);
        let children = queued(extract(
            &NATOMANGA,
            "natomanga.com",
            &http,
            Url::parse("https://www.natomanga.com/manga/kimi-boku").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 2);
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /manga/kimi-boku "));
        assert!(requests[1].starts_with("GET /api/manga/kimi-boku/chapters?limit=-1 "));
    }

    #[tokio::test]
    async fn manga_chapter_keywords() {
        let api = serde_json::json!({"data": {"chapters": [
            {"chapter_slug": "chapter-2-5", "updated_at": "2025-01-02T03:04:05", "view": 77},
        ]}});
        let (root, requests) = serve(vec![json(api)]);
        let http = http("NatoManga", root);
        let chapters = chapters(&http, "https://www.natomanga.com", MANGA_PAGE)
            .await
            .unwrap();
        requests.join().unwrap();
        assert_eq!(
            chapters[0].0.as_str(),
            "https://www.natomanga.com/manga/kimi-boku/chapter-2-5"
        );
        let k = &chapters[0].1;
        assert_eq!(k["manga"], "Kimi & Boku");
        assert_eq!(k["manga_slug"], "kimi-boku");
        assert_eq!(k["author"], "Some One");
        assert_eq!(k["status"], "Ongoing");
        assert_eq!(k["date_updated"], "2025-01-02 15:04:05");
        assert_eq!(
            k["tags"],
            serde_json::json!(["Action", "Comedy", "Romance"])
        );
        assert_eq!(k["chapter"], 2);
        assert_eq!(k["chapter_minor"], ".5");
        assert_eq!(k["date"], "2025-01-02 03:04:05");
        assert_eq!(k["views"], 77);
        assert_eq!(k["lang"], "en");
    }

    #[tokio::test]
    async fn bookmarks_follow_pages() {
        let first = r#"<a class="user-bookmark-item x" href="https://www.manganato.gg/manga/a">A</a>
<a href="?page=2">Last(2)</a>"#;
        let second =
            r#"<a class="user-bookmark-item y" href="https://www.manganato.gg/manga/b">B</a>"#;
        let (root, requests) = serve(vec![html(first), html(second)]);
        let http = http("MangaNato", root);
        let children = queued(extract(
            &MANGANATO,
            "manganato.gg",
            &http,
            Url::parse("https://www.manganato.gg/bookmark").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].subcategory(), "bookmark");
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /bookmark?page=1 "));
        assert!(requests[1].starts_with("GET /bookmark?page=2 "));
    }
}
