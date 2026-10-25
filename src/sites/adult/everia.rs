//! EVERIA.CLUB posts and tag, category, date and search listings,
//! mirroring gallery_dl/extractor/everia.py: a post page lists its lazily
//! loaded images; listings queue each post page by page until a page
//! redirects.
use super::capitalize;
use crate::error;
use crate::sites::art::{extr, extract_iter, nameext_into};
use crate::sites::filehosts::{Body, request};
use crate::sites::{Events, Http, Item, Route, Site, percent_decode, unescape};
use serde_json::Map;
use url::Url;

const ROOT: &str = "https://everia.club";
static POST: Route = Route::new(
    "post",
    "{filename}.{extension}",
    &["{category}", "{title}"],
    "{post_url}_{num}",
);
static TAG: Route = Route::new("tag", "{filename}.{extension}", &["{category}"], "");
static CATEGORY: Route = Route::new("category", "{filename}.{extension}", &["{category}"], "");
static DATE: Route = Route::new("date", "{filename}.{extension}", &["{category}"], "");
static SEARCH: Route = Route::new("search", "{filename}.{extension}", &["{category}"], "");

pub static SITE: Site = Site {
    category: "everia",
    name: "EVERIA.CLUB",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&POST, &TAG, &CATEGORY, &DATE, &SEARCH],
    api_key: false,
    cookies: None,
};

#[derive(Debug)]
enum Target {
    /// `/YYYY/MM/DD/SLUG`, raw.
    Post(String),
    /// A listing's route and path (`""` for searches) with its `s` query.
    Listing(&'static Route, String, Option<String>),
}

fn digits(text: &str, len: usize) -> bool {
    text.len() == len && text.bytes().all(|b| b.is_ascii_digit())
}

/// `(/\d{4}/\d{2}/\d{2}/[^/?#]+)`, `(/tag/[^/?#]+)`,
/// `(/category/[^/?#]+)` (all unanchored),
/// `(/\d{4}(?:/\d{2})?(?:/\d{2})?)(?:/page/\d+)?/?$` and
/// `/(?:page/\d+/)?\?s=([^&#]+)`, in class order.
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) || url.host_str() != Some("everia.club") {
        return None;
    }
    let path = url.path();
    let raw: Vec<&str> = path.split('/').skip(1).collect();
    match raw.as_slice() {
        [y, m, d, slug, ..] if digits(y, 4) && digits(m, 2) && digits(d, 2) && !slug.is_empty() => {
            return Some(Target::Post(format!("/{y}/{m}/{d}/{slug}")));
        }
        ["tag", name, ..] if !name.is_empty() => {
            return Some(Target::Listing(&TAG, format!("/tag/{name}"), None));
        }
        ["category", name, ..] if !name.is_empty() => {
            return Some(Target::Listing(
                &CATEGORY,
                format!("/category/{name}"),
                None,
            ));
        }
        _ => {}
    }
    if url.query().is_none() && url.fragment().is_none() {
        let mut parts: &[&str] = raw.strip_suffix(&[""]).unwrap_or(&raw);
        if let [head @ .., "page", n] = parts
            && n.bytes().all(|b| b.is_ascii_digit())
            && !n.is_empty()
        {
            parts = head;
        }
        let date = match parts {
            [y] if digits(y, 4) => Some(format!("/{y}")),
            [y, m] if digits(y, 4) && digits(m, 2) => Some(format!("/{y}/{m}")),
            [y, m, d] if digits(y, 4) && digits(m, 2) && digits(d, 2) => {
                Some(format!("/{y}/{m}/{d}"))
            }
            _ => None,
        };
        if let Some(date) = date {
            return Some(Target::Listing(&DATE, date, None));
        }
    }
    let paged = match raw.as_slice() {
        [""] => true,
        ["page", n, ""] => !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()),
        _ => false,
    };
    let query = url.query().filter(|_| paged)?.strip_prefix("s=")?;
    let value = &query[..query.find('&').unwrap_or(query.len())];
    (!value.is_empty()).then(|| Target::Listing(&SEARCH, String::new(), Some(value.to_owned())))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Post(path)) => crate::sites::once(async move {
            let url = format!("{ROOT}{path}/");
            let page = super::page(http, &url, &[]).await?;
            let items = post_items(&page, &url);
            Ok(crate::sites::events(crate::sites::post(
                &SITE, http, &POST, items,
            )))
        }),
        Some(Target::Listing(route, path, search)) => listing(http, route, path, search),
        None => crate::sites::fail(error("unsupported EVERIA.CLUB URL")),
    }
}

/// `EveriaPostExtractor.items`.
fn post_items(page: &str, url: &str) -> Vec<Item> {
    let content = extr(page, "itemprop=\"text\">", "<h3");
    let urls = lazy_sources(content);
    let mut data = Map::new();
    data.insert(
        "title".into(),
        unescape(extr(page, "itemprop=\"headline\">", "</h")).into(),
    );
    let tags: Vec<&str> = extract_iter(page, "rel=\"tag\">", "</a>");
    data.insert("tags".into(), tags.into());
    data.insert("post_url".into(), percent_decode(url).into());
    data.insert(
        "post_category".into(),
        capitalize(extr(page, "post-in-category-", " ")).into(),
    );
    data.insert("count".into(), urls.len().into());
    let title = data["title"].as_str().map(str::to_owned);
    urls.into_iter()
        .enumerate()
        .filter_map(|(index, raw)| {
            let raw = percent_decode(raw);
            let file = Url::parse(&raw).ok().filter(crate::sites::plain)?;
            let mut fields = data.clone();
            fields.insert("num".into(), (index + 1).into());
            nameext_into(&raw, &mut fields);
            Some(
                Item::new(file, fields)
                    .ids(url, (index + 1).to_string())
                    .caption(title.clone()),
            )
        })
        .collect()
}

/// `img.*?lazy-src="([^"]+)` findall: `lazy-src` values that follow an
/// `img` on the same line.
fn lazy_sources(content: &str) -> Vec<&str> {
    let mut found = Vec::new();
    let mut pos = 0;
    while let Some(start) = content[pos..].find("img").map(|i| pos + i) {
        let line_end = content[start..]
            .find('\n')
            .map_or(content.len(), |i| start + i);
        let source = content[start..line_end]
            .find("lazy-src=\"")
            .map(|i| start + i + "lazy-src=\"".len());
        match source {
            Some(begin) => {
                let len = content[begin..].find('"').unwrap_or(content.len() - begin);
                if len > 0 {
                    found.push(&content[begin..begin + len]);
                    pos = begin + len;
                } else {
                    pos = start + 3;
                }
            }
            None => pos = start + 3,
        }
    }
    found
}

/// `EveriaExtractor._pagination`: each listed post, queued, until a page
/// answers with a redirect.
fn listing<'a>(
    http: &'a Http,
    route: &'static Route,
    path: String,
    search: Option<String>,
) -> Events<'a> {
    crate::sites::paginate(Some(1usize), move |state| {
        let path = path.clone();
        let search = search.clone();
        async move {
            let Some(pnum) = state else { return Ok(None) };
            let url = match pnum {
                1 => format!("{ROOT}{path}/"),
                pnum => format!("{ROOT}{path}/page/{pnum}/"),
            };
            let mut url = http.api(&url)?;
            if let Some(search) = &search {
                // Upstream passes the URL's raw text as the `s` parameter.
                url.query_pairs_mut().append_pair("s", search);
            }
            let response = request(
                http,
                reqwest::Method::GET,
                &url,
                &http.browser_headers(&[]),
                Body::None,
            )
            .await?;
            if response.status >= 300 {
                return Ok(None);
            }
            let page = response.text();
            let children = post_links(&page)
                .into_iter()
                .filter_map(|link| Url::parse(link).ok())
                .map(|url| (url, Map::new()))
                .collect();
            let next = (pnum < super::MAX_PAGES).then_some(pnum + 1);
            Ok(Some((
                crate::sites::manga::queue_urls(http, &SITE, route, children),
                next,
            )))
        }
    })
}

/// `thumbnail">\s*<a href="([^"]+)` findall.
fn post_links(page: &str) -> Vec<&str> {
    page.split("thumbnail\">")
        .skip(1)
        .filter_map(|rest| {
            let rest = rest.trim_start().strip_prefix("<a href=\"")?;
            let end = rest.find('"').filter(|&end| end > 0)?;
            Some(&rest[..end])
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::manga::tests::{archive, path};
    use crate::sites::testing::{html, http, keywords, media, queued, response, serve};
    use serde_json::json;

    impl PartialEq for Target {
        fn eq(&self, other: &Self) -> bool {
            match (self, other) {
                (Self::Post(a), Self::Post(b)) => a == b,
                (Self::Listing(r, p, q), Self::Listing(s, o, t)) => {
                    std::ptr::eq(*r, *s) && p == o && q == t
                }
                _ => false,
            }
        }
    }

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        let listing = |route: &'static Route, path: &str, s: Option<&str>| {
            Some(Target::Listing(route, path.into(), s.map(Into::into)))
        };
        assert_eq!(
            t("https://everia.club/2024/09/08/some-title/"),
            Some(Target::Post("/2024/09/08/some-title".into()))
        );
        assert_eq!(
            t("https://everia.club/2024/09/08/page/2/"),
            Some(Target::Post("/2024/09/08/page".into()))
        );
        assert_eq!(
            t("https://everia.club/tag/x/y"),
            listing(&TAG, "/tag/x", None)
        );
        assert_eq!(
            t("https://everia.club/category/cosplay/"),
            listing(&CATEGORY, "/category/cosplay", None)
        );
        assert_eq!(
            t("https://everia.club/2024/09"),
            listing(&DATE, "/2024/09", None)
        );
        assert_eq!(
            t("https://everia.club/2024/page/3/"),
            listing(&DATE, "/2024", None)
        );
        assert_eq!(
            t("https://everia.club/?s=two+words&x=1"),
            listing(&SEARCH, "", Some("two+words"))
        );
        assert_eq!(
            t("https://everia.club/page/2/?s=x"),
            listing(&SEARCH, "", Some("x"))
        );
        for raw in [
            "https://everia.club/",
            "https://everia.club/2024/9",
            "https://everia.club/2024/09?x",
            "https://everia.club/?x=1&s=a",
            "https://www.everia.club/tag/x",
            "https://everia.club.evil.test/tag/x",
            "https://everia.club:8443/tag/x",
            "https://u:p@everia.club/tag/x",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[tokio::test]
    async fn extracts_post() {
        let page = "<h1 itemprop=\"headline\">Studio &amp; Light</h1>\
            <article class=\"post post-in-category-photo x\"><div itemprop=\"text\">\
            <img decoding=\"async\" data-lazy-src=\"https://img.test/2024/a%20b.jpg\">\n\
            <img src=\"x.gif\">\n<p>lazy-src=\"https://img.test/no.jpg\"</p>\n\
            <img class=\"x\" data-lazy-src=\"https://img.test/2024/c.webp\"><h3>\
            <a rel=\"tag\">One</a><a rel=\"tag\">Two</a>";
        let (root, server) = serve(vec![html(page)]);
        let http = http("everia", root);
        let files = media(extract(
            &http,
            Url::parse("https://everia.club/2024/09/08/studio-light").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].url.as_str(), "https://img.test/2024/a%20b.jpg");
        let k = keywords(&files[1]);
        assert_eq!(k["title"], "Studio & Light");
        assert_eq!(k["tags"], json!(["One", "Two"]));
        assert_eq!(k["post_category"], "Photo");
        assert_eq!(
            k["post_url"],
            "https://everia.club/2024/09/08/studio-light/"
        );
        assert_eq!(k["count"], 2);
        assert_eq!(keywords(&files[0])["filename"], "a b");
        assert_eq!(path(&files[1]), "everia/Studio & Light/c.webp");
        assert_eq!(
            archive(&files[1]),
            r#"["{post_url}_{num}",["https://everia.club/2024/09/08/studio-light/","2"]]"#
        );
        assert!(server.join().unwrap()[0].starts_with("GET /2024/09/08/studio-light/ "));
    }

    #[tokio::test]
    async fn listing_stops_at_redirect() {
        let first = "<div class=\"thumbnail\">\n  <a href=\"https://everia.club/2024/01/02/a/\">";
        let moved = response("301 Moved Permanently\r\nLocation: /", "text/html", b"");
        let (root, server) = serve(vec![html(first), moved]);
        let http = http("everia", root);
        let children = queued(extract(
            &http,
            Url::parse("https://everia.club/?s=two+words").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].subcategory(), "search");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /?s=two%2Bwords "));
        assert!(requests[1].starts_with("GET /page/2/?s=two%2Bwords "));
    }
}
