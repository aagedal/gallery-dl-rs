//! Scrolller subreddits, Reddit users and posts, mirroring
//! gallery_dl/extractor/scrolller.py through the GraphQL `admin` endpoint:
//! each post's widest media source, or every album entry's.
//!
//! Upstream's `username`/`password` login is not supported, so followed
//! subreddits (`/following`), which need it, fail with an error. The
//! request interval of half a second to 1.5 seconds is not applied.
use crate::sites::media::{parse_query, query_get};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value, json};
use url::Url;

const ROOT: &str = "https://scrolller.com";
const API: &str = "https://api.scrolller.com/admin";
const FILENAME: &str = "{id}{num:?_//>03}{title:? //[:230]}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{subredditTitle}"];
const ARCHIVE: &str = "{id}_{num}";
static SUBREDDIT: Route = Route::new("subreddit", FILENAME, DIRECTORY, ARCHIVE);
static USER: Route = Route::new(
    "user",
    FILENAME,
    &["{category}", "User", "{posted_by}"],
    ARCHIVE,
);
static FOLLOWING: Route = Route::new("following", FILENAME, DIRECTORY, ARCHIVE);
static POST: Route = Route::new("post", FILENAME, DIRECTORY, ARCHIVE);

pub static SITE: Site = Site {
    category: "scrolller",
    name: "Scrolller",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&SUBREDDIT, &USER, &FOLLOWING, &POST],
    api_key: false,
    cookies: None,
};

const SUBREDDIT_QUERY: &str = include_str!("scrolller/SubredditQuery.graphql");
const CHILDREN_QUERY: &str = include_str!("scrolller/SubredditChildrenQuery.graphql");
const USER_QUERY: &str = include_str!("scrolller/UserPostsQuery.graphql");
const POST_QUERY: &str = include_str!("scrolller/SubredditPostQuery.graphql");

#[derive(Debug, PartialEq)]
enum Target {
    Subreddit(String, Option<String>),
    User(String),
    Following,
    Post(String),
}

/// `(/r/[^/?#]+)(?:/?\?([^#]+))?`, `/reddit-user/([^/?#]+)`, `/following`
/// or `/(?!r/|following$)([^/?#]+)`, in upstream's class order.
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::art::on_host(url, &["scrolller.com"]) {
        return None;
    }
    let path = url.path();
    if let Some(rest) = path.strip_prefix("/r/") {
        let (name, tail) = rest.split_once('/').unwrap_or((rest, ""));
        if !name.is_empty() {
            let query = if tail.is_empty() {
                url.query().filter(|q| !q.is_empty()).map(str::to_owned)
            } else {
                None
            };
            return Some(Target::Subreddit(format!("/r/{name}"), query));
        }
    }
    if let Some(rest) = path.strip_prefix("/reddit-user/") {
        let user = rest.split('/').next().unwrap_or_default();
        if !user.is_empty() {
            return Some(Target::User(user.into()));
        }
    }
    if path.starts_with("/following") {
        return Some(Target::Following);
    }
    if path.starts_with("/r/") {
        return None;
    }
    let slug = path.strip_prefix('/')?.split('/').next()?;
    (!slug.is_empty()).then(|| Target::Post(slug.into()))
}

/// `_request_graphql` against the admin endpoint: the response's `data`.
async fn graphql(http: &Http, query: &str, variables: &Value) -> Result<Value> {
    let body = json!({"query": query, "variables": variables, "authorization": null});
    let headers = [
        ("Origin", ROOT),
        ("Sec-Fetch-Dest", "empty"),
        ("Sec-Fetch-Mode", "cors"),
        ("Sec-Fetch-Site", "same-site"),
    ];
    let data = http.post_json(&http.api(API)?, &body, &headers).await?;
    data.get("data")
        .cloned()
        .filter(Value::is_object)
        .ok_or_else(|| error("Scrolller API response has no data"))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Subreddit(path, query)) => crate::sites::once(async move {
            let params = parse_query(query.as_deref());
            let filter = query_get(&params, "filter")
                .map(|f| Value::from(f.to_uppercase().trim_end_matches('S')))
                .unwrap_or(Value::Null);
            let variables = json!({"url": path, "filter": filter, "sortBy": "RANDOM", "limit": 50});
            let data = graphql(http, SUBREDDIT_QUERY, &variables).await?;
            let subreddit = data.get("getSubreddit").cloned().unwrap_or(Value::Null);
            let variables = json!({
                "subredditId": subreddit.get("id").cloned().unwrap_or(Value::Null),
                "iterator": null,
                "filter": filter,
                "sortBy": "RANDOM",
                "limit": 50,
                "nsfw": "ALL",
            });
            let first = subreddit.get("children").cloned();
            Ok(pagination(
                http,
                &SUBREDDIT,
                CHILDREN_QUERY,
                variables,
                first,
            ))
        }),
        Some(Target::User(user)) => crate::sites::once(async move {
            let variables = json!({
                "username": crate::sites::percent_decode(&user),
                "iterator": null,
                "limit": 40,
                "filter": null,
                "sortBy": "RANDOM",
                "nsfw": "ALL",
            });
            Ok(pagination(http, &USER, USER_QUERY, variables, None))
        }),
        Some(Target::Following) => crate::sites::fail(error(
            "Scrolller followed subreddits require a login, which is not supported",
        )),
        Some(Target::Post(slug)) => crate::sites::once(async move {
            let data = graphql(http, POST_QUERY, &json!({"url": format!("/{slug}")})).await?;
            let post = data.get("getPost").cloned().unwrap_or(Value::Null);
            Ok(crate::sites::events(crate::sites::post(
                &SITE,
                http,
                &POST,
                parse_post(&post),
            )))
        }),
        None => crate::sites::fail(error("unsupported Scrolller URL")),
    }
}

/// `_pagination`: the first page may come with the initial query.
fn pagination<'a>(
    http: &'a Http,
    route: &'static Route,
    query: &'static str,
    variables: Value,
    first: Option<Value>,
) -> Events<'a> {
    let first = first.filter(|data| crate::sites::art::truthy(data.get("items")));
    crate::sites::paginate(Some((variables, first)), move |state| async move {
        let Some((mut variables, data)) = state else {
            return Ok(None);
        };
        let mut data = match data {
            Some(data) => data,
            None => graphql(http, query, &variables).await?,
        };
        // `while "items" not in data: data = data.popitem()[1]`
        while data.get("items").is_none() {
            data = match data {
                Value::Object(map) => map
                    .into_iter()
                    .next_back()
                    .map(|(_, value)| value)
                    .ok_or_else(|| error("Scrolller API response has no items"))?,
                _ => return Err(error("Scrolller API response has no items")),
            };
        }
        let mut events = Vec::new();
        for post in data["items"].as_array().into_iter().flatten() {
            events.extend(crate::sites::post(&SITE, http, route, parse_post(post)));
        }
        let next = match data.get("iterator") {
            Some(iterator) if crate::sites::art::truthy(Some(iterator)) => {
                variables["iterator"] = iterator.clone();
                Some((variables, None))
            }
            _ => None,
        };
        Ok(Some((crate::sites::events(events), next)))
    })
}

/// `max(sources, key=_sort_key)`: the widest source, preferring originals;
/// the first wins ties.
fn best(sources: &[Value]) -> Option<&Value> {
    let key = |src: &Value| {
        (
            src.get("width").and_then(Value::as_i64).unwrap_or(0),
            !crate::sites::art::truthy(src.get("isOptimized")),
        )
    };
    let mut best: Option<&Value> = None;
    for src in sources {
        if best.is_none_or(|b| key(src) > key(b)) {
            best = Some(src);
        }
    }
    best
}

/// `_extract_files` and `items()` for one post.
pub(crate) fn parse_post(post: &Value) -> Vec<Item> {
    let Some(mut post) = post.as_object().cloned() else {
        return Vec::new();
    };
    // The API dropped `posted_by`; `reddit_posted_by` names the same user.
    if !post.contains_key("posted_by")
        && let Some(user) = post.get("reddit_posted_by").cloned()
    {
        post.insert("posted_by".into(), user);
    }
    let album = post.remove("albumContent");
    let mut files: Vec<Map<String, Value>> = Vec::new();
    match album
        .as_ref()
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty())
    {
        None => {
            let sources = post.get("mediaSources").and_then(Value::as_array);
            if let Some(Value::Object(src)) = sources.and_then(|s| best(s)) {
                let mut src = src.clone();
                src.insert("num".into(), 0.into());
                files.push(src);
            }
        }
        Some(album) => {
            for (index, media) in album.iter().enumerate() {
                let sources = media.get("mediaSources").and_then(Value::as_array);
                if let Some(Value::Object(src)) = sources.and_then(|s| best(s)) {
                    let mut src = src.clone();
                    src.insert("num".into(), (index + 1).into());
                    files.push(src);
                }
            }
        }
    }
    post.insert("count".into(), files.len().into());
    let id = post.get("id").map(crate::sites::scalar).unwrap_or_default();
    let title = post.get("title").and_then(Value::as_str).map(str::to_owned);
    let user = post
        .get("posted_by")
        .or_else(|| post.get("username"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let mut items = Vec::new();
    for file in files {
        let raw = file
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let Some(url) = crate::sites::media::web_url(&raw) else {
            continue;
        };
        let num = file
            .get("num")
            .map(crate::sites::scalar)
            .unwrap_or_default();
        crate::sites::art::update(&mut post, &file);
        crate::sites::art::nameext_into(&raw, &mut post);
        items.push(
            Item::new(url, post.clone())
                .ids(id.clone(), num)
                .user(user.clone())
                .caption(title.clone()),
        );
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http, json, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://scrolller.com/r/AmateurPhotography?filter=pictures"),
            Some(Target::Subreddit(
                "/r/AmateurPhotography".into(),
                Some("filter=pictures".into())
            ))
        );
        assert_eq!(
            t("https://scrolller.com/reddit-user/bob"),
            Some(Target::User("bob".into()))
        );
        assert_eq!(
            t("https://scrolller.com/following"),
            Some(Target::Following)
        );
        assert_eq!(
            t("https://www.scrolller.com/a-title-slug-a1b2c3"),
            Some(Target::Post("a-title-slug-a1b2c3".into()))
        );
        for raw in [
            "https://scrolller.com/",
            "https://scrolller.com/r/",
            "https://scrolller.com.evil/r/x",
            "https://api.scrolller.com/r/x",
            "https://scrolller.com:81/r/x",
            "https://u@scrolller.com/r/x",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn source(url: &str, width: i64, optimized: bool) -> Value {
        json!({"url": url, "width": width, "height": 1, "isOptimized": optimized})
    }

    #[tokio::test]
    async fn extracts_post_album() {
        let post = json!({"getPost": {"id": 42, "title": "Album", "subredditTitle": "pics",
            "albumContent": [
                {"mediaSources": [source("https://images.scrolller.com/a-small.webp", 10, true),
                                  source("https://images.scrolller.com/a.jpg", 20, false),
                                  source("https://images.scrolller.com/a-opt.webp", 20, true)]},
                {"mediaSources": []},
                {"mediaSources": [source("https://images.scrolller.com/c.png", 5, false)]}]}});
        let (root, server) = serve(vec![json(json!({"data": post}))]);
        let http = http("scrolller", root);
        let files = media(extract(
            &http,
            Url::parse("https://scrolller.com/album-slug-42").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].url.as_str(), "https://images.scrolller.com/a.jpg");
        let k = keywords(&files[1]);
        assert_eq!(k["num"], 3);
        assert_eq!(k["count"], 2);
        assert!(!k.contains_key("albumContent"));
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "42_003 Album.png");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("scrolller/pics")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{id}_{num}",["42","1"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("POST /admin "));
        assert!(requests[0].contains("origin: https://scrolller.com"));
        assert!(requests[0].contains(r#""variables":{"url":"/album-slug-42"}"#));
        assert!(requests[0].contains("query SubredditPostQuery"));
    }

    #[tokio::test]
    async fn pages_through_subreddit() {
        let item = |id: i64| {
            json!({"id": id, "title": "", "subredditTitle": "pics",
                "mediaSources": [source(&format!("https://images.scrolller.com/{id}.jpg"), 1, false)]})
        };
        let first = json!({"data": {"getSubreddit": {"id": 7, "isNsfw": false,
            "children": {"iterator": "it1", "items": [item(1)]}}}});
        let second =
            json!({"data": {"getSubreddit": {"children": {"iterator": null, "items": [item(2)]}}}});
        let (root, server) = serve(vec![json(first), json(second)]);
        let http = http("scrolller", root);
        let files = media(extract(
            &http,
            Url::parse("https://scrolller.com/r/pics?filter=videos").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let (file, _) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "1.jpg");
        let requests = server.join().unwrap();
        assert!(requests[0].contains(r#""filter":"VIDEO""#));
        assert!(requests[0].contains("query SubredditQuery"));
        assert!(requests[1].contains("query SubredditChildrenQuery"));
        assert!(requests[1].contains(r#""iterator":"it1""#));
        assert!(requests[1].contains(r#""subredditId":7"#));
        assert!(requests[1].contains(r#""nsfw":"ALL""#));
        assert!(requests[1].contains("nsfw: $nsfw"));
    }

    #[tokio::test]
    async fn user_posts_take_the_reddit_poster_as_posted_by() {
        let page = json!({"data": {"getUserPosts": {"iterator": null, "items": [
            {"id": 3, "title": "", "reddit_posted_by": "spez", "subredditTitle": "pics",
             "mediaSources": [source("https://images.scrolller.com/3.jpg", 1, false)]}]}}});
        let (root, server) = serve(vec![json(page)]);
        let http = http("scrolller", root);
        let files = media(extract(
            &http,
            Url::parse("https://scrolller.com/reddit-user/spez").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let (_, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("scrolller/User/spez")
        );
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].contains("query UserPostsQuery"));
        assert!(requests[0].contains(r#""nsfw":"ALL""#));
        assert!(!requests[0].contains(" posted_by "));
    }

    #[tokio::test]
    async fn following_needs_login() {
        let http =
            crate::sites::testing::http("scrolller", Url::parse("http://127.0.0.1:9/").unwrap());
        assert!(
            crate::sites::testing::fails(extract(
                &http,
                Url::parse("https://scrolller.com/following").unwrap()
            ))
            .await
        );
    }
}
