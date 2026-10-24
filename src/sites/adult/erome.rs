//! EroMe albums, user profiles and searches, mirroring
//! gallery_dl/extractor/erome.py: an album page lists its media groups;
//! users and searches page through album links, 36 per page, and queue
//! each album.
//!
//! Session cookies from `--cookies` (or set by the site) are sent with
//! every request. Files are requested with the album page as Referer,
//! which upstream intends but sets to the last media URL.
use super::{checked_page, set_param, with_query};
use crate::sites::art::{date, extr, extract_iter, nameext_into, on_host, parse_query, parse_ts};
use crate::sites::filehosts::extract as find_between;
use crate::sites::{Events, Http, Item, Route, Site, percent_decode, unescape};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://www.erome.com";
const FILENAME: &str = "{album_id} {title} {num:>02}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{user}"];
const ARCHIVE: &str = "{album_id}_{num}";
static ALBUM: Route = Route::new("album", FILENAME, DIRECTORY, ARCHIVE);
static USER: Route = Route::new("user", FILENAME, DIRECTORY, ARCHIVE);
static SEARCH: Route = Route::new("search", FILENAME, DIRECTORY, ARCHIVE);

pub static SITE: Site = Site {
    category: "erome",
    name: "EroMe",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&ALBUM, &USER, &SEARCH],
    api_key: false,
    cookies: Some("erome.com"),
};

#[derive(Debug, PartialEq)]
enum Target {
    Album(String),
    /// The raw user name and query string.
    User(String, Option<String>),
    /// The raw query string, from `q=`.
    Search(String),
}

/// `/a/(\w+)` (unanchored), `/(?!a/|search\?)([^/?#]+)(?:/?\?([^#]+))?` and
/// `/search/?\?(q=[^#]+)`, in class order. Like upstream, `/search/?q=`
/// URLs are user URLs.
fn target(url: &Url) -> Option<Target> {
    if !on_host(url, &["erome.com"]) {
        return None;
    }
    let path = url.path();
    let query = url.query().filter(|q| !q.is_empty());
    if let Some(rest) = path.strip_prefix("/a/") {
        let len = rest
            .bytes()
            .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_')
            .count();
        if len > 0 {
            return Some(Target::Album(rest[..len].to_owned()));
        }
    }
    let raw: Vec<&str> = path.split('/').skip(1).collect();
    let (name, tail) = match raw.as_slice() {
        [name, tail @ ..] if !name.is_empty() => (*name, tail),
        _ => return None,
    };
    let excluded = path.starts_with("/a/") || (path == "/search" && url.query().is_some());
    if !excluded {
        let query = match tail {
            [] | [""] => query.map(str::to_owned),
            _ => None,
        };
        return Some(Target::User(name.to_owned(), query));
    }
    let query = query.filter(|q| q.starts_with("q=") && q.len() > 2)?;
    matches!(path, "/search" | "/search/").then(|| Target::Search(query.to_owned()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Album(id)) => crate::sites::once(album(http, id)),
        Some(Target::User(user, query)) => {
            let mut params = parse_query(query.as_deref().unwrap_or_default());
            if !params.iter().any(|(key, _)| key == "t") && !http.option_bool("reposts", false) {
                params.push(("t".into(), "posts".into()));
            }
            listing(http, &USER, format!("{ROOT}/{user}"), params)
        }
        Some(Target::Search(query)) => {
            listing(http, &SEARCH, format!("{ROOT}/search"), parse_query(&query))
        }
        None => crate::sites::fail(error("unsupported EroMe URL")),
    }
}

/// `EromeAlbumExtractor.items`.
async fn album(http: &Http, id: String) -> Result<Events<'_>> {
    let url = format!("{ROOT}/a/{id}");
    let (_, page) = checked_page(http, &http.api(&url)?).await?;
    let items = parse_album(&page, &id, &url)?;
    Ok(crate::sites::events(crate::sites::post(
        &SITE, http, &ALBUM, items,
    )))
}

fn parse_album(page: &str, id: &str, url: &str) -> Result<Vec<Item>> {
    let (title, pos) =
        find_between(page, "property=\"og:title\" content=\"", "\"", 0).unwrap_or(("", 0));
    let pos = page[pos..]
        .find("<div class=\"user-profile")
        .map(|start| pos + start)
        .ok_or_else(|| error("EroMe album page has no user profile"))?;
    let (user, pos) =
        find_between(page, "href=\"https://www.erome.com/", "\"", pos).unwrap_or(("", pos));
    let tags = find_between(page, "<p class=\"mt-10\"", "</p>", pos).map_or("", |(t, _)| t);
    let mut urls = Vec::new();
    let mut timestamp = None;
    for group in page.split("<div class=\"media-group\"").skip(1) {
        let media = match extr(group, "<source src=\"", "\"") {
            "" => extr(group, "data-src=\"", "\""),
            source => source,
        };
        if !media.is_empty() {
            urls.push(media);
        }
        if timestamp.is_none() {
            let ts = extr(group, "?v=", "\"");
            if ts.len() > 1 {
                timestamp = parse_ts(&Value::from(ts));
            }
        }
    }
    let user = percent_decode(user);
    let mut data = Map::new();
    data.insert("album_id".into(), id.into());
    data.insert("title".into(), unescape(title).into());
    data.insert("user".into(), user.clone().into());
    data.insert("count".into(), urls.len().into());
    data.insert(
        "date".into(),
        timestamp.map_or(Value::Null, |ts| date(Some(ts))),
    );
    let tags: Vec<String> = extract_iter(tags, "?q=", "\"")
        .into_iter()
        .map(|tag| tag.replace('+', " "))
        .collect();
    data.insert("tags".into(), tags.into());
    Ok(urls
        .into_iter()
        .enumerate()
        .filter_map(|(index, raw)| {
            let file = crate::sites::art::file_url(raw, ROOT)?;
            let mut fields = data.clone();
            fields.insert("num".into(), (index + 1).into());
            nameext_into(raw, &mut fields);
            Some(
                Item::new(file, fields)
                    .ids(id, (index + 1).to_string())
                    .user(Some(user.clone()))
                    .timestamp(timestamp)
                    .header("Referer", url),
            )
        })
        .collect())
}

/// `EromeExtractor._pagination`: every other album link of each page,
/// queued, until a page has fewer than 36.
fn listing<'a>(
    http: &'a Http,
    route: &'static Route,
    url: String,
    params: Vec<(String, String)>,
) -> Events<'a> {
    let start = crate::sites::art::parse_int(
        params
            .iter()
            .find(|(key, _)| key == "page")
            .map_or("", |(_, value)| value),
        1,
    );
    crate::sites::paginate(Some((start, 0)), move |state| {
        let url = url.clone();
        let mut params = params.clone();
        async move {
            let Some((pnum, count)) = state else {
                return Ok(None);
            };
            set_param(&mut params, "page", pnum.to_string());
            let (_, page) = checked_page(http, &with_query(http, &url, &params)?).await?;
            let ids = album_ids(&page);
            let children = ids
                .iter()
                .filter_map(|id| Url::parse(&format!("{ROOT}/a/{id}")).ok())
                .map(|url| (url, Map::new()))
                .collect();
            let next =
                (ids.len() >= 36 && count + 1 < super::MAX_PAGES).then_some((pnum + 1, count + 1));
            Ok(Some((
                crate::sites::manga::queue_urls(http, &SITE, route, children),
                next,
            )))
        }
    })
}

/// `EromeAlbumExtractor.pattern.findall(page)[::2]`: album IDs of
/// `erome.com/a/ID` links, every other one.
fn album_ids(page: &str) -> Vec<String> {
    page.split("erome.com/a/")
        .skip(1)
        .filter_map(|rest| {
            let len = rest
                .bytes()
                .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_')
                .count();
            (len > 0).then(|| rest[..len].to_owned())
        })
        .step_by(2)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::manga::tests::{archive, path};
    use crate::sites::testing::{html, http, keywords, media, queued, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.erome.com/a/NQgdlWvk"),
            Some(Target::Album("NQgdlWvk".into()))
        );
        assert_eq!(
            t("https://erome.com/someone/?t=reposts"),
            Some(Target::User("someone".into(), Some("t=reposts".into())))
        );
        assert_eq!(
            t("https://www.erome.com/someone/x?y"),
            Some(Target::User("someone".into(), None))
        );
        assert_eq!(
            t("https://www.erome.com/search?q=two+words&page=2"),
            Some(Target::Search("q=two+words&page=2".into()))
        );
        assert_eq!(
            t("https://www.erome.com/search/?q=x"),
            Some(Target::User("search".into(), Some("q=x".into())))
        );
        for raw in [
            "https://www.erome.com/",
            "https://www.erome.com/a/",
            "https://www.erome.com/search?page=1",
            "https://m.erome.com/user",
            "https://erome.com.evil.test/user",
            "https://erome.com:8443/user",
            "https://u:p@erome.com/user",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[tokio::test]
    async fn extracts_album() {
        let page = r#"<meta property="og:title" content="Lake &amp; Hills">
<div class="user-profile"><a href="https://www.erome.com/Some%20User">Some User</a></div>
<p class="mt-10"><a href="/search?q=outdoor+trip">#outdoor trip</a><a href="/search?q=lake">#lake</a></p>
<div class="media-group"><div class="img" data-src="https://s1.erome.com/1/a/one.jpg?v=1700000000"></div></div>
<div class="media-group"><video><source src="https://v1.erome.com/1/a/two.mp4" type="video/mp4"></video></div>"#;
        let (root, server) = serve(vec![html(page)]);
        let http = http("erome", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.erome.com/a/abc123").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(files[1].url.as_str(), "https://v1.erome.com/1/a/two.mp4");
        assert_eq!(
            files[1].headers["Referer"],
            "https://www.erome.com/a/abc123"
        );
        let k = keywords(&files[1]);
        assert_eq!(k["title"], "Lake & Hills");
        assert_eq!(k["user"], "Some User");
        assert_eq!(k["count"], 2);
        assert_eq!(k["date"], "2023-11-14 22:13:20");
        assert_eq!(k["tags"], json!(["outdoor trip", "lake"]));
        assert_eq!(k["num"], 2);
        assert_eq!(
            path(&files[1]),
            "erome/Some User/abc123 Lake & Hills 02.mp4"
        );
        assert_eq!(archive(&files[1]), r#"["{album_id}_{num}",["abc123","2"]]"#);
        assert!(server.join().unwrap()[0].starts_with("GET /a/abc123 "));
    }

    #[tokio::test]
    async fn user_pages_queue_every_other_album_link() {
        let mut first = String::new();
        for i in 0..36 {
            first.push_str(&format!(
                "<a href=\"https://www.erome.com/a/id{i}\"><img></a><a href=\"https://www.erome.com/a/id{i}\">t</a>"
            ));
        }
        let second = "<a href=\"https://www.erome.com/a/last\">";
        let (root, server) = serve(vec![html(&first), html(second)]);
        let http = http("erome", root);
        let children = queued(extract(
            &http,
            Url::parse("https://www.erome.com/someone?page=3").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 37);
        assert_eq!(children[0].subcategory(), "user");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /someone?page=3&t=posts "));
        assert!(requests[1].starts_with("GET /someone?page=4&t=posts "));
    }
}
