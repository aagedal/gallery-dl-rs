//! Poringa image posts, user posts and searches, mirroring
//! gallery_dl/extractor/poringa.py: a post page lists its images; users and
//! searches page through `/buscar/` results.
//!
//! Private posts (redirected to the login page) are skipped. Listings skip
//! posts that fail to load, as upstream logs and continues. Session cookies
//! from `--cookies` (or set by the site) are sent with every request.
use super::checked_page;
use crate::sites::art::{extract_iter, nameext_into, on_host};
use crate::sites::filehosts::extract as find_between;
use crate::sites::{Events, Http, Item, Route, Site, percent_decode, unescape};
use crate::{Result, error};
use futures_util::StreamExt;
use serde_json::Map;
use url::Url;

const ROOT: &str = "http://www.poringa.net";
const FILENAME: &str = "{post_id}_{title}_{num:>03}_{filename}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{user}", "{post_id}"];
const ARCHIVE: &str = "{post_id}_{num}";
static POST: Route = Route::new("post", FILENAME, DIRECTORY, ARCHIVE);
static USER: Route = Route::new("user", FILENAME, DIRECTORY, ARCHIVE);
static SEARCH: Route = Route::new("search", FILENAME, DIRECTORY, ARCHIVE);

pub static SITE: Site = Site {
    category: "poringa",
    name: "Poringa",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&POST, &USER, &SEARCH],
    api_key: false,
    cookies: Some("poringa.net"),
};

#[derive(Debug, PartialEq)]
enum Target {
    Post(String),
    User(String),
    /// The raw `q` value.
    Search(String),
}

/// `/posts/imagenes/(\d+)` (unanchored), `/(\w+)$` and
/// `/buscar/\?&?q=([^&#]+)`, in class order.
fn target(url: &Url) -> Option<Target> {
    if !on_host(url, &["poringa.net"]) {
        return None;
    }
    let path = url.path();
    if let Some(id) = path
        .strip_prefix("/posts/imagenes/")
        .and_then(crate::sites::art::leading_digits)
    {
        return Some(Target::Post(id.to_owned()));
    }
    let name = &path[1..];
    if !name.is_empty()
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && url.query().is_none()
        && url.fragment().is_none()
    {
        return Some(Target::User(name.to_owned()));
    }
    let query = url.query().filter(|_| path == "/buscar/")?;
    let query = query
        .strip_prefix('&')
        .unwrap_or(query)
        .strip_prefix("q=")?;
    let value = &query[..query.find('&').unwrap_or(query.len())];
    (!value.is_empty()).then(|| Target::Search(value.to_owned()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Post(id)) => {
            crate::sites::once(
                async move { Ok(crate::sites::events(post(http, &POST, &id).await?)) },
            )
        }
        Some(Target::User(name)) => search(http, &USER, name),
        // Upstream passes the URL's raw text as the `q` parameter.
        Some(Target::Search(query)) => search(http, &SEARCH, query),
        None => crate::sites::fail(error("unsupported Poringa URL")),
    }
}

/// One post as `PoringaExtractor.items` emits it; nothing for private
/// posts.
async fn post(
    http: &Http,
    route: &'static Route,
    id: &str,
) -> Result<Vec<crate::event::ExtractionEvent>> {
    let url = format!("{ROOT}/posts/imagenes/{id}");
    let (location, page) = checked_page(http, &http.api(&url)?).await?;
    if location.as_str().contains("/registro-login?") {
        return Ok(Vec::new());
    }
    let items = parse_post(&page, id, &url);
    Ok(crate::sites::post(&SITE, http, route, items))
}

fn parse_post(page: &str, id: &str, url: &str) -> Vec<Item> {
    let (title, pos) = find_between(page, "property=\"og:title\" content=\"", "\"", 0)
        .map_or(("", 0), |(title, pos)| (title, pos));
    let user = page
        .get(pos..)
        .and_then(|rest| rest.find("<div class=\"main-info"))
        .and_then(|start| find_between(page, "href=\"http://www.poringa.net/", "\"", pos + start))
        .map(|(user, _)| user)
        .filter(|user| !user.is_empty())
        .unwrap_or("poringa");
    let main = crate::sites::art::extr(page, "property=\"dc:content\" role=\"main\">", "</div>");
    let urls = extract_iter(main, "<img class=\"imagen\" border=\"0\" src=\"", "\"");
    let mut data = Map::new();
    data.insert("post_id".into(), id.into());
    data.insert("title".into(), unescape(title).into());
    data.insert("user".into(), percent_decode(user).into());
    data.insert("count".into(), urls.len().into());
    let username = Some(percent_decode(user));
    urls.into_iter()
        .enumerate()
        .filter_map(|(index, raw)| {
            let file = crate::sites::art::file_url(raw, ROOT)?;
            let mut fields = data.clone();
            fields.insert("num".into(), (index + 1).into());
            nameext_into(raw, &mut fields);
            Some(
                Item::new(file, fields)
                    .ids(id, (index + 1).to_string())
                    .user(username.clone())
                    .header("Referer", url),
            )
        })
        .collect()
}

/// `PoringaExtractor._pagination` over `/buscar/?q=…&p=N`: the unique post
/// IDs linked from each page, until a page has fewer than 19.
fn search<'a>(http: &'a Http, route: &'static Route, query: String) -> Events<'a> {
    crate::sites::paginate(Some(1usize), move |state| {
        let query = query.clone();
        async move {
            let Some(pnum) = state else { return Ok(None) };
            let mut url = http.api(&format!("{ROOT}/buscar/"))?;
            url.query_pairs_mut()
                .append_pair("q", &query)
                .append_pair("p", &pnum.to_string());
            let (_, page) = checked_page(http, &url).await?;
            let ids = post_ids(&page);
            let next = (ids.len() >= 19 && pnum < super::MAX_PAGES).then_some(pnum + 1);
            let events = futures_util::stream::iter(ids)
                .then(move |id| async move {
                    crate::sites::events(post(http, route, &id).await.unwrap_or_default())
                })
                .flatten()
                .boxed();
            Ok(Some((events, next)))
        }
    })
}

/// `PoringaPostExtractor.pattern.findall`: IDs of `poringa.net` post links,
/// first occurrences only.
fn post_ids(page: &str) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    for rest in page.split("poringa.net/posts/imagenes/").skip(1) {
        if let Some(id) = crate::sites::art::leading_digits(rest)
            && !ids.iter().any(|seen| seen == id)
        {
            ids.push(id.to_owned());
        }
    }
    ids
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::manga::tests::{archive, path};
    use crate::sites::testing::{html, http, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("http://www.poringa.net/posts/imagenes/3051081/Title.html"),
            Some(Target::Post("3051081".into()))
        );
        assert_eq!(
            t("https://poringa.net/Some_User"),
            Some(Target::User("Some_User".into()))
        );
        assert_eq!(
            t("http://www.poringa.net/buscar/?&q=two+words&p=2"),
            Some(Target::Search("two+words".into()))
        );
        for raw in [
            "http://www.poringa.net/",
            "http://www.poringa.net/user/",
            "http://www.poringa.net/user?x",
            "http://www.poringa.net/buscar/?p=1&q=x",
            "http://www.poringa.net/posts/imagenes/x",
            "http://m.poringa.net/user",
            "http://poringa.net.evil.test/user",
            "http://poringa.net:8080/user",
            "http://u:p@poringa.net/user",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    const POST_PAGE: &str = r#"<meta property="og:title" content="Beach &amp; Sun">
<div class="main-info"><a href="http://www.poringa.net/Some%20One">Some One</a></div>
<div property="dc:content" role="main"><img class="imagen" border="0" src="https://img.test/a/1.jpg">
<img class="imagen" border="0" src="https://img.test/a/2.jpg"></div>"#;

    #[tokio::test]
    async fn extracts_post() {
        let (root, server) = serve(vec![html(POST_PAGE)]);
        let http = http("poringa", root);
        let files = media(extract(
            &http,
            Url::parse("http://www.poringa.net/posts/imagenes/123/Beach.html").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[1].headers["Referer"],
            "http://www.poringa.net/posts/imagenes/123"
        );
        let k = keywords(&files[1]);
        assert_eq!(k["title"], "Beach & Sun");
        assert_eq!(k["user"], "Some One");
        assert_eq!(k["post_id"], "123");
        assert_eq!(k["count"], 2);
        assert_eq!(
            path(&files[1]),
            "poringa/Some One/123/123_Beach & Sun_002_2.jpg"
        );
        assert_eq!(archive(&files[1]), r#"["{post_id}_{num}",["123","2"]]"#);
        assert!(server.join().unwrap()[0].starts_with("GET /posts/imagenes/123 "));
    }

    #[tokio::test]
    async fn waits_out_the_check_page_and_searches() {
        let check = "<html><title>Please wait a few moments</title></html>";
        let results = "<a href=\"http://www.poringa.net/posts/imagenes/5/a.html\">\
            <a href=\"http://www.poringa.net/posts/imagenes/5/a.html\">\
            <a href=\"/posts/imagenes/6\">";
        let post = "<meta property=\"og:title\" content=\"T\">\
            <div property=\"dc:content\" role=\"main\"><img class=\"imagen\" border=\"0\" src=\"https://img.test/x.jpg\"></div>";
        let (root, server) = serve(vec![html(check), html(results), html(post)]);
        let http = http("poringa", root);
        let files = media(extract(
            &http,
            Url::parse("http://www.poringa.net/buscar/?q=two+words").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(keywords(&files[0])["user"], "poringa");
        assert_eq!(keywords(&files[0])["subcategory"], "search");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /buscar/?q=two%2Bwords&p=1 "));
        assert!(requests[1].starts_with("GET /buscar/?q=two%2Bwords&p=1 "));
        assert!(requests[2].starts_with("GET /posts/imagenes/5 "));
    }
}
