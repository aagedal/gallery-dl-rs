//! PornPics galleries and tag, search, listing and category pages,
//! mirroring gallery_dl/extractor/pornpics.py: a gallery page links each
//! image; the other pages queue galleries from their HTML and then from
//! the site's JSON pagination.
use super::Interval;
use crate::sites::art::{Scan, extract_iter, split_html};
use crate::sites::manga::{Gallery, keywords, text};
use crate::sites::{Events, Http, Route, Site, unescape};
use crate::{Result, error};
use futures_util::StreamExt;
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://www.pornpics.com";
static GALLERY: Route = Route::new(
    "gallery",
    "{category}_{gallery_id}_{num:>03}.{extension}",
    &["{category}", "{gallery_id} {title}"],
    "{gallery_id}_{num}",
);
static TAG: Route = Route::new("tag", "{filename}.{extension}", &["{category}"], "");
static SEARCH: Route = Route::new("search", "{filename}.{extension}", &["{category}"], "");
static LISTING: Route = Route::new("listing", "{filename}.{extension}", &["{category}"], "");
static CATEGORY: Route = Route::new("category", "{filename}.{extension}", &["{category}"], "");

/// Upstream `request_interval = (0.5, 1.5)`.
static INTERVAL: Interval = Interval::new(500);

pub static SITE: Site = Site {
    category: "pornpics",
    name: "PornPics",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&GALLERY, &TAG, &SEARCH, &LISTING, &CATEGORY],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    /// `SLUG-ID` (or `ID`) and the ID.
    Gallery(String, String),
    Tag(String),
    /// The raw search term (a `q` value, pornstar or channel name).
    Search(String),
    Listing(String),
    Category(String),
}

/// The path after an optional `/\w\w` language prefix, as each pattern
/// tries it first: with the prefix (if any), then without.
fn paths(path: &str) -> Vec<&str> {
    let bytes = path.as_bytes();
    let word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut out = Vec::new();
    if bytes.len() >= 4 && word(bytes[1]) && word(bytes[2]) && bytes[3] == b'/' {
        out.push(&path[3..]);
    }
    out.push(path);
    out
}

/// `(?:www\.)?pornpics\.com(?:/\w\w)?` followed by
/// `/galleries/((?:[^/?#]+-)?(\d+))`, `/tags/([^/?#]+)` and
/// `/(?:\?q=|pornstars/|channels/)([^/&#]+)` (unanchored),
/// `/(popular|recent|rating|likes|views|comments)/?$` and
/// `/([^/?#]+)/?$`, in class order.
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::art::on_host(url, &["pornpics.com"]) {
        return None;
    }
    let candidates = paths(url.path());
    let bare = url.query().is_none() && url.fragment().is_none();
    let first = |f: &dyn Fn(&str) -> Option<Target>| candidates.iter().find_map(|p| f(p));
    first(&|path| {
        let segment = path.strip_prefix("/galleries/")?;
        let segment = &segment[..segment.find('/').unwrap_or(segment.len())];
        let (slug, id) = gallery_id(segment)?;
        Some(Target::Gallery(slug.to_owned(), id.to_owned()))
    })
    .or_else(|| {
        first(&|path| {
            let tag = path.strip_prefix("/tags/")?.split('/').next()?;
            (!tag.is_empty()).then(|| Target::Tag(tag.to_owned()))
        })
    })
    .or_else(|| {
        first(&|path| {
            let term = match path {
                "/" => url.query()?.strip_prefix("q=")?.split('&').next()?,
                _ => path
                    .strip_prefix("/pornstars/")
                    .or_else(|| path.strip_prefix("/channels/"))?
                    .split('/')
                    .next()?,
            };
            let term = &term[..term.find(['/', '&']).unwrap_or(term.len())];
            (!term.is_empty()).then(|| Target::Search(term.to_owned()))
        })
    })
    .or_else(|| {
        first(&|path| {
            let name = path.strip_prefix('/')?;
            let name = name.strip_suffix('/').unwrap_or(name);
            if !bare || name.is_empty() || name.contains('/') {
                return None;
            }
            match name {
                "popular" | "recent" | "rating" | "likes" | "views" | "comments" => {
                    Some(Target::Listing(name.to_owned()))
                }
                name => Some(Target::Category(name.to_owned())),
            }
        })
    })
}

/// `(?:[^/?#]+-)?(\d+)` over one segment: the digits after the last `-`
/// that has text before it and a digit after it, else leading digits.
fn gallery_id(segment: &str) -> Option<(&str, &str)> {
    let digits = crate::sites::art::leading_digits;
    let from_dash = segment
        .match_indices('-')
        .rev()
        .filter(|(i, _)| *i > 0)
        .find_map(|(i, _)| digits(&segment[i + 1..]).map(|id| (i + 1, id)));
    let (start, id) = from_dash.or_else(|| digits(segment).map(|id| (0, id)))?;
    Some((&segment[..start + id.len()], id))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Gallery(slug, id)) => crate::sites::once(gallery(http, slug, id)),
        Some(Target::Tag(tag)) => paginate(http, &TAG, format!("{ROOT}/tags/{tag}/"), None),
        Some(Target::Search(term)) => {
            let params = vec![
                ("q".to_owned(), term.replace('-', " ")),
                ("lang".to_owned(), "en".to_owned()),
            ];
            paginate(
                http,
                &SEARCH,
                format!("{ROOT}/search/srch.php"),
                Some((params, 0)),
            )
        }
        Some(Target::Listing(name)) => crate::sites::once(async move {
            INTERVAL.wait().await;
            let page = super::page(http, &format!("{ROOT}/{name}/"), &[]).await?;
            let children = gallery_links(extract_iter(&page, "class='rel-link' href='", "'"));
            Ok(crate::sites::manga::queue_urls(
                http, &SITE, &LISTING, children,
            ))
        }),
        Some(Target::Category(name)) => paginate(http, &CATEGORY, format!("{ROOT}/{name}/"), None),
        None => crate::sites::fail(error("unsupported PornPics URL")),
    }
}

/// `PornpicsGalleryExtractor`.
async fn gallery(http: &Http, slug: String, id: String) -> Result<Events<'_>> {
    INTERVAL.wait().await;
    let page = super::page(http, &format!("{ROOT}/galleries/{slug}/"), &[]).await?;
    let data = metadata(&page, &id);
    let pages = extract_iter(&page, "class='rel-link' href='", "'")
        .into_iter()
        .map(|url| (url.to_owned(), None))
        .collect();
    Ok(crate::sites::events(
        Gallery {
            enum_key: "num",
            ..Gallery::chapter(&GALLERY, data, pages).id(id)
        }
        .events(&SITE, http),
    ))
}

/// `PornpicsGalleryExtractor.metadata`.
fn metadata(page: &str, id: &str) -> Map<String, Value> {
    let mut e = Scan::new(page);
    let slug = crate::sites::filehosts::rpartition(e.extr("/galleries/", "/"), "-").0;
    keywords([
        ("gallery_id", text::parse_int(id).into()),
        ("slug", slug.into()),
        ("title", unescape(e.extr("<h1>", "<")).into()),
        (
            "channel",
            split_html(e.extr(">Channel:&nbsp;", "</div>")).into(),
        ),
        (
            "models",
            split_html(e.extr(">Models:", "<span class=\"suggest")).into(),
        ),
        (
            "categories",
            split_html(e.extr(">Categories:", "<span class=\"suggest")).into(),
        ),
        ("tags", split_html(e.extr(">Tags List:", " </div>")).into()),
        (
            "views",
            text::parse_int(&e.extr(">Views:", "<").replace(',', "")).into(),
        ),
    ])
}

/// Gallery URLs (relative ones on the root) with their `g_url` keyword.
fn gallery_links(hrefs: Vec<&str>) -> Vec<(Url, Map<String, Value>)> {
    hrefs
        .into_iter()
        .filter_map(|href| {
            let href = match href.starts_with('/') {
                true => format!("{ROOT}{href}"),
                false => href.to_owned(),
            };
            let url = Url::parse(&href).ok()?;
            Some((url, keywords([("g_url", href.into())])))
        })
        .collect()
}

/// `PornpicsExtractor._pagination`: without parameters the first 20
/// galleries come from the page's HTML; then JSON pages of `limit=20` are
/// requested, `offset` advancing by 10, until one has fewer than 10.
fn paginate<'a>(
    http: &'a Http,
    route: &'static Route,
    url: String,
    params: Option<(Vec<(String, String)>, usize)>,
) -> Events<'a> {
    let html = match params {
        Some(_) => None,
        None => Some(url.clone()),
    };
    let (params, offset) = params.unwrap_or_default();
    let offset = if html.is_some() { 20 } else { offset };
    let first: Events<'a> = match html {
        Some(page_url) => crate::sites::once(async move {
            INTERVAL.wait().await;
            let page = super::page(http, &page_url, &[]).await?;
            let children = gallery_links(extract_iter(&page, "class=\"rel-link\" href=\"", "\""));
            Ok(crate::sites::manga::queue_urls(
                http, &SITE, route, children,
            ))
        }),
        None => crate::sites::events(Vec::new()),
    };
    let rest = crate::sites::paginate(Some((offset, 0usize)), move |state| {
        let url = url.clone();
        let params = params.clone();
        async move {
            let Some((offset, count)) = state else {
                return Ok(None);
            };
            let mut params = params;
            params.push(("offset".into(), offset.to_string()));
            params.push(("limit".into(), "20".into()));
            let referer = match offset {
                0 => format!("{ROOT}/"),
                _ => url.clone(),
            };
            let request = super::with_query(http, &url, &params)?;
            INTERVAL.wait().await;
            let galleries = http
                .json_with(
                    &request,
                    &http.browser_headers(&[
                        ("Accept", "application/json, text/javascript, */*; q=0.01"),
                        ("Referer", &referer),
                        ("X-Requested-With", "XMLHttpRequest"),
                    ]),
                )
                .await?;
            let galleries = galleries.as_array().cloned().unwrap_or_default();
            let next = (galleries.len() >= 10 && count + 1 < super::MAX_PAGES)
                .then_some((offset + 10, count + 1));
            let children = galleries
                .into_iter()
                .filter_map(|gallery| {
                    let Value::Object(data) = gallery else {
                        return None;
                    };
                    let url = Url::parse(data.get("g_url")?.as_str()?).ok()?;
                    Some((url, data))
                })
                .collect();
            Ok(Some((
                crate::sites::manga::queue_urls(http, &SITE, route, children),
                next,
            )))
        }
    });
    first.chain(rest).boxed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::manga::tests::{archive, path};
    use crate::sites::testing::{html, http, json, keywords as kw, media, queued, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.pornpics.com/galleries/some-title-12345/"),
            Some(Target::Gallery("some-title-12345".into(), "12345".into()))
        );
        assert_eq!(
            t("https://pornpics.com/de/galleries/a-1-b/"),
            Some(Target::Gallery("a-1".into(), "1".into()))
        );
        assert_eq!(
            t("https://www.pornpics.com/galleries/678/"),
            Some(Target::Gallery("678".into(), "678".into()))
        );
        assert_eq!(
            t("https://www.pornpics.com/tags/red-hair/"),
            Some(Target::Tag("red-hair".into()))
        );
        assert_eq!(
            t("https://www.pornpics.com/?q=two-words&x=1"),
            Some(Target::Search("two-words".into()))
        );
        assert_eq!(
            t("https://www.pornpics.com/fr/pornstars/some-name/"),
            Some(Target::Search("some-name".into()))
        );
        assert_eq!(
            t("https://www.pornpics.com/popular/"),
            Some(Target::Listing("popular".into()))
        );
        assert_eq!(
            t("https://www.pornpics.com/de/outdoor"),
            Some(Target::Category("outdoor".into()))
        );
        assert_eq!(
            t("https://www.pornpics.com/de/"),
            Some(Target::Category("de".into()))
        );
        for raw in [
            "https://www.pornpics.com/",
            "https://www.pornpics.com/galleries/title/",
            "https://www.pornpics.com/outdoor/?page=2",
            "https://www.pornpics.com/a/b/c/",
            "https://m.pornpics.com/outdoor/",
            "https://pornpics.com.evil.test/outdoor/",
            "https://pornpics.com:8443/outdoor/",
            "https://u:p@pornpics.com/outdoor/",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[tokio::test]
    async fn extracts_gallery() {
        let page = "<link href=\"https://www.pornpics.com/galleries/sunny-day-12345/\">\
            <h1>Sunny &amp; Day</h1><div>Channel:&nbsp;<a>Studio</a></div>\
            <div>Models:<a>Model A</a><span class=\"suggest\"></span>\
            <div>Categories:<a>Outdoor</a><span class=\"suggest\"></span>\
            <div>Tags List:<a>Sun</a> </div><div>Views: 1,234<br>\
            <a class='rel-link' href='https://cdn.pornpics.com/pics/1.jpg'></a>\
            <a class='rel-link' href='https://cdn.pornpics.com/pics/2.jpg'></a>";
        let (root, server) = serve(vec![html(page)]);
        let http = http("pornpics", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.pornpics.com/galleries/sunny-day-12345/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = kw(&files[1]);
        assert_eq!(k["gallery_id"], 12345);
        assert_eq!(k["slug"], "sunny-day");
        assert_eq!(k["title"], "Sunny & Day");
        assert_eq!(k["channel"], json!(["Studio"]));
        assert_eq!(k["models"], json!(["Model A"]));
        assert_eq!(k["categories"], json!(["Outdoor"]));
        assert_eq!(k["tags"], json!(["Sun"]));
        assert_eq!(k["views"], 1234);
        assert_eq!(
            path(&files[1]),
            "pornpics/12345 Sunny & Day/pornpics_12345_002.jpg"
        );
        assert_eq!(
            archive(&files[1]),
            r#"["{gallery_id}_{num}",["12345","2"]]"#
        );
        assert!(server.join().unwrap()[0].starts_with("GET /galleries/sunny-day-12345/ "));
    }

    #[tokio::test]
    async fn category_reads_html_then_json() {
        let page = "<a class=\"rel-link\" href=\"/galleries/a-1/\"></a>";
        let full: Vec<Value> = (2..12)
            .map(|i| json!({"g_url": format!("https://www.pornpics.com/galleries/g-{i}/"), "desc": "x"}))
            .collect();
        let (root, server) = serve(vec![html(page), json(Value::Array(full)), json(json!([]))]);
        let http = http("pornpics", root);
        let children = queued(extract(
            &http,
            Url::parse("https://www.pornpics.com/outdoor/").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 11);
        assert_eq!(children[0].subcategory(), "category");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /outdoor/ "));
        assert!(requests[1].starts_with("GET /outdoor/?offset=20&limit=20 "));
        assert!(requests[1].contains("referer: https://www.pornpics.com/outdoor/"));
        assert!(requests[2].starts_with("GET /outdoor/?offset=30&limit=20 "));
    }

    #[tokio::test]
    async fn search_starts_with_json() {
        let (root, server) = serve(vec![json(json!([
            {"g_url": "https://www.pornpics.com/galleries/a-1/"}
        ]))]);
        let http = http("pornpics", root);
        let children = queued(extract(
            &http,
            Url::parse("https://www.pornpics.com/?q=two-words").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 1);
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with("GET /search/srch.php?q=two+words&lang=en&offset=0&limit=20 ")
        );
        assert!(requests[0].contains("referer: https://www.pornpics.com/\r\n"));
    }
}
