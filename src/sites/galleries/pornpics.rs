//! PornPics galleries, tags, searches, listings and categories, mirroring
//! gallery_dl/extractor/pornpics.py.
use crate::error;
use crate::sites::gdl::{
    Scan, captures, extract_iter, group, parse_int, re, rpartition, split_html, unescape,
};
use crate::sites::manga::{GALLERY_ARCHIVE, GALLERY_FILENAME};
use crate::sites::{Events, Http, Route, Site};
use serde_json::{Map, Value};
use std::time::Duration;
use url::Url;

const ROOT: &str = "https://www.pornpics.com";
const BASE: &str = r"^https?://(?:www\.)?pornpics\.com(?:/\w\w)?";
const DIRECTORY: &[&str] = &["{category}", "{gallery_id} {title}"];

static GALLERY: Route = Route::new("gallery", GALLERY_FILENAME, DIRECTORY, GALLERY_ARCHIVE);
static TAG: Route = Route::new("tag", "{filename}.{extension}", &["{category}"], "");
static SEARCH: Route = Route::new("search", "{filename}.{extension}", &["{category}"], "");
static LISTING: Route = Route::new("listing", "{filename}.{extension}", &["{category}"], "");
static CATEGORY: Route = Route::new("category", "{filename}.{extension}", &["{category}"], "");

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
    Gallery(String, String),
    Tag(String),
    Search(String),
    Listing(String),
    Category(String),
}

fn target(url: &Url) -> Option<Target> {
    if let Some(c) = captures(re!(&format!(r"{BASE}/galleries/((?:[^/?#]+-)?(\d+))")), url) {
        return Some(Target::Gallery(group(&c, 1).into(), group(&c, 2).into()));
    }
    if let Some(c) = captures(re!(&format!(r"{BASE}/tags/([^/?#]+)")), url) {
        return Some(Target::Tag(group(&c, 1).into()));
    }
    if let Some(c) = captures(
        re!(&format!(r"{BASE}/(?:\?q=|pornstars/|channels/)([^/&#]+)")),
        url,
    ) {
        return Some(Target::Search(group(&c, 1).into()));
    }
    if let Some(c) = captures(
        re!(&format!(
            r"{BASE}/(popular|recent|rating|likes|views|comments)/?$"
        )),
        url,
    ) {
        return Some(Target::Listing(group(&c, 1).into()));
    }
    captures(re!(&format!(r"{BASE}/([^/?#]+)/?$")), url)
        .map(|c| Target::Category(group(&c, 1).into()))
}

/// Upstream `request_interval = (0.5, 1.5)`.
async fn pace() {
    if !cfg!(test) {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

fn gallery_children(galleries: Vec<Value>) -> Vec<(Url, Map<String, Value>)> {
    galleries
        .into_iter()
        .filter_map(|gallery| {
            let gallery = gallery.as_object()?.clone();
            let url = gallery.get("g_url")?.as_str()?;
            let url = if url.starts_with('/') {
                format!("{ROOT}{url}")
            } else {
                url.to_owned()
            };
            Some((Url::parse(&url).ok()?, gallery))
        })
        .collect()
}

fn href_galleries(page: &str, needle: &str) -> Vec<Value> {
    extract_iter(page, needle, if needle.ends_with('\'') { "'" } else { "\"" })
        .into_iter()
        .map(|href| serde_json::json!({"g_url": if href.starts_with('/') { format!("{ROOT}{href}") } else { href.to_owned() }}))
        .collect()
}

/// `_pagination`: the HTML first page (when `offset` is unset), then JSON
/// pages of `limit // 2` galleries.
fn pagination<'a>(
    http: &'a Http,
    route: &'static Route,
    url: String,
    query: Option<String>,
) -> Events<'a> {
    let start = if query.is_some() {
        (0u64, false)
    } else {
        (20, true)
    };
    crate::sites::paginate(Some(start), move |state| {
        let url = url.clone();
        let query = query.clone();
        async move {
            let Some((offset, html_first)) = state else {
                return Ok(None);
            };
            pace().await;
            if html_first {
                let page = http.text(&http.api(&url)?).await?;
                let children =
                    gallery_children(href_galleries(&page, "class=\"rel-link\" href=\""));
                return Ok(Some((
                    super::queue(http, &SITE, route, children),
                    Some((20, false)),
                )));
            }
            let mut request = http.api(&url)?;
            {
                let mut pairs = request.query_pairs_mut();
                if let Some(query) = &query {
                    pairs.append_pair("q", query).append_pair("lang", "en");
                }
                pairs
                    .append_pair("offset", &offset.to_string())
                    .append_pair("limit", "20");
            }
            let referer = if offset > 0 {
                url.clone()
            } else {
                format!("{ROOT}/")
            };
            let headers = [
                ("Accept", "application/json, text/javascript, */*; q=0.01"),
                ("Referer", referer.as_str()),
                ("X-Requested-With", "XMLHttpRequest"),
            ];
            let galleries = http
                .json_with(&request, &headers)
                .await?
                .as_array()
                .cloned()
                .unwrap_or_default();
            let next = (galleries.len() >= 10).then_some((offset + 10, false));
            Ok(Some((
                super::queue(http, &SITE, route, gallery_children(galleries)),
                next,
            )))
        }
    })
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported PornPics URL"));
    };
    match target {
        Target::Gallery(path, id) => crate::sites::once(async move {
            let page = http
                .text(&http.api(&format!("{ROOT}/galleries/{path}/"))?)
                .await?;
            let mut scan = Scan::new(&page);
            let mut data = Map::new();
            data.insert("gallery_id".into(), parse_int(&id).into());
            data.insert(
                "slug".into(),
                rpartition(scan.extr("/galleries/", "/"), "-").0.into(),
            );
            data.insert("title".into(), unescape(scan.extr("<h1>", "<")).into());
            data.insert(
                "channel".into(),
                split_html(scan.extr(">Channel:&nbsp;", "</div>")).into(),
            );
            data.insert(
                "models".into(),
                split_html(scan.extr(">Models:", "<span class=\"suggest")).into(),
            );
            data.insert(
                "categories".into(),
                split_html(scan.extr(">Categories:", "<span class=\"suggest")).into(),
            );
            data.insert(
                "tags".into(),
                split_html(scan.extr(">Tags List:", " </div>")).into(),
            );
            data.insert(
                "views".into(),
                parse_int(&scan.extr(">Views:", "<").replace(',', "")).into(),
            );
            let images = extract_iter(&page, "class='rel-link' href='", "'")
                .into_iter()
                .map(|u| (u.to_owned(), None))
                .collect();
            Ok(crate::sites::events(super::numbered(
                &SITE, http, &GALLERY, &data, images, &id,
            )))
        }),
        Target::Tag(tag) => pagination(http, &TAG, format!("{ROOT}/tags/{tag}/"), None),
        Target::Category(name) => pagination(http, &CATEGORY, format!("{ROOT}/{name}/"), None),
        Target::Search(query) => pagination(
            http,
            &SEARCH,
            format!("{ROOT}/search/srch.php"),
            Some(query.replace('-', " ")),
        ),
        Target::Listing(name) => crate::sites::once(async move {
            let page = http.text(&http.api(&format!("{ROOT}/{name}/"))?).await?;
            Ok(super::queue(
                http,
                &SITE,
                &LISTING,
                gallery_children(href_galleries(&page, "class='rel-link' href='")),
            ))
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{children, html, http as client, json, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.pornpics.com/galleries/title-62126520/"),
            Some(Target::Gallery("title-62126520".into(), "62126520".into()))
        );
        assert_eq!(
            t("https://www.pornpics.com/de/tags/summer-dress/"),
            Some(Target::Tag("summer-dress".into()))
        );
        assert_eq!(
            t("https://www.pornpics.com/?q=nature+lover"),
            Some(Target::Search("nature+lover".into()))
        );
        assert_eq!(
            t("https://www.pornpics.com/pornstars/jane-doe/"),
            Some(Target::Search("jane-doe".into()))
        );
        assert_eq!(
            t("https://www.pornpics.com/popular/"),
            Some(Target::Listing("popular".into()))
        );
        assert_eq!(
            t("https://www.pornpics.com/ass/"),
            Some(Target::Category("ass".into()))
        );
    }

    #[tokio::test]
    async fn extracts_gallery() {
        let page = r#"<a href="/galleries/some-title-7/"><h1>Some Title</h1><span>Views: 1,234<
            <a class='rel-link' href='https://cdni.pornpics.com/1280/1/2/7/a.jpg'>"#;
        let (root, server) = serve(vec![html(page)]);
        let http = client("pornpics", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.pornpics.com/galleries/some-title-7/").unwrap(),
        ))
        .await;
        let k = keywords(&files[0]);
        assert_eq!(
            (k["slug"].as_str(), k["title"].as_str(), k["views"].as_i64()),
            (Some("some-title"), Some("Some Title"), Some(1234))
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn category_reads_html_then_json() {
        let first = r#"<a class="rel-link" href="/galleries/a-1/">"#;
        let (root, server) = serve(vec![
            html(first),
            json(json!([{"g_url": "https://www.pornpics.com/galleries/b-2/"}])),
        ]);
        let http = client("pornpics", root);
        let queued = children(extract(
            &http,
            Url::parse("https://www.pornpics.com/ass/").unwrap(),
        ))
        .await;
        assert_eq!(queued.len(), 2);
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /ass/?offset=20&limit=20 "));
    }
}
