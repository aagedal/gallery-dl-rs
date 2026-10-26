//! Scrolller subreddits, users, posts and followed subreddits over its
//! GraphQL API, mirroring gallery_dl/extractor/scrolller.py.
use crate::error;
use crate::sites::gdl::{
    auth_info, captures, group, nameext_into, parse_query, re, truthy, unquote,
};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value, json};
use std::sync::Mutex;
use std::time::Duration;
use url::Url;

const ROOT: &str = "https://scrolller.com";
const BASE: &str = r"^https?://(?:www\.)?scrolller\.com";
const FILENAME: &str = "{id}{num:?_//>03}{title:? //[:230]}.{extension}";

static SUBREDDIT: Route = Route::new(
    "subreddit",
    FILENAME,
    &["{category}", "{subredditTitle}"],
    "{id}_{num}",
);
static USER: Route = Route::new(
    "user",
    FILENAME,
    &["{category}", "User", "{posted_by}"],
    "{id}_{num}",
);
static FOLLOWING: Route = Route::new(
    "following",
    FILENAME,
    &["{category}", "{subredditTitle}"],
    "{id}_{num}",
);
static POST: Route = Route::new(
    "post",
    FILENAME,
    &["{category}", "{subredditTitle}"],
    "{id}_{num}",
);

pub static SITE: Site = Site {
    category: "scrolller",
    name: "Scrolller",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&SUBREDDIT, &USER, &FOLLOWING, &POST],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Subreddit(String, String),
    User(String),
    Following,
    Post(String),
}

fn target(url: &Url) -> Option<Target> {
    if let Some(c) = captures(re!(&format!(r"{BASE}(/r/[^/?#]+)(?:/?\?([^#]+))?")), url) {
        return Some(Target::Subreddit(group(&c, 1).into(), group(&c, 2).into()));
    }
    if let Some(c) = captures(
        re!(&format!(r"{BASE}/reddit-user/([^/?#]+)(?:/?\?([^#]+))?")),
        url,
    ) {
        return Some(Target::User(group(&c, 1).into()));
    }
    if captures(re!(&format!(r"{BASE}/following")), url).is_some() {
        return Some(Target::Following);
    }
    let c = captures(re!(&format!(r"{BASE}/([^/?#]+)")), url)?;
    // Upstream's `(?!r/|following$)`.
    (!(url.path().starts_with("/r/") || url.path() == "/following"))
        .then(|| Target::Post(group(&c, 1).into()))
}

/// The session's login token (upstream `self.auth_token`).
static TOKEN: Mutex<Option<String>> = Mutex::new(None);

fn token() -> Value {
    TOKEN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .map_or(Value::Null, Value::from)
}

/// `_request_graphql`: the response's `data`.
async fn graphql(
    http: &Http,
    operation: &str,
    variables: Value,
    admin: bool,
) -> crate::Result<Value> {
    // Upstream `request_interval = (0.5, 1.5)`.
    if !cfg!(test) {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let body = json!({"query": query(operation), "variables": variables, "authorization": token()})
        .to_string();
    let (url, content_type) = if admin {
        ("https://api.scrolller.com/admin", "application/json")
    } else {
        (
            "https://api.scrolller.com/api/v2/graphql",
            "text/plain;charset=UTF-8",
        )
    };
    let headers = [
        ("Origin", ROOT),
        ("Sec-Fetch-Dest", "empty"),
        ("Sec-Fetch-Mode", "cors"),
        ("Sec-Fetch-Site", "same-site"),
    ];
    let bytes = http
        .post(
            &http.api(url)?,
            crate::sites::http::Body::Raw(content_type, body.as_bytes()),
            &headers,
        )
        .await?;
    let data: Value =
        serde_json::from_slice(&bytes).map_err(|_| error("Scrolller returned invalid JSON"))?;
    Ok(data["data"].clone())
}

async fn login(http: &Http) -> crate::Result<()> {
    if TOKEN.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
        return Ok(());
    }
    let Some((username, password)) = auth_info(http) else {
        return Ok(());
    };
    let data = graphql(
        http,
        "LoginQuery",
        json!({"username": username, "password": password}),
        false,
    )
    .await
    .map_err(|e| error(format!("Scrolller login failed ({e})")))?;
    let token = data["login"]["token"]
        .as_str()
        .ok_or_else(|| error("Scrolller login failed"))?
        .to_owned();
    *TOKEN.lock().unwrap_or_else(|e| e.into_inner()) = Some(token);
    Ok(())
}

/// `_sort_key`: the widest, non-optimized source.
fn best(sources: &[Value]) -> Option<Map<String, Value>> {
    sources
        .iter()
        .max_by_key(|s| {
            (
                s["width"].as_i64().unwrap_or_default(),
                !truthy(s.get("isOptimized")),
            )
        })
        .and_then(|s| s.as_object().cloned())
}

/// `ScrolllerExtractor.items` for one post.
fn post_items(post: &Value) -> Vec<Item> {
    let Some(mut post) = post.as_object().cloned() else {
        return Vec::new();
    };
    let album = post
        .remove("albumContent")
        .and_then(|a| a.as_array().cloned())
        .filter(|a| !a.is_empty());
    let files: Vec<Map<String, Value>> = match album {
        None => post
            .get("mediaSources")
            .and_then(Value::as_array)
            .and_then(|s| best(s))
            .map(|mut src| {
                src.insert("num".into(), 0.into());
                vec![src]
            })
            .unwrap_or_default(),
        Some(album) => album
            .iter()
            .enumerate()
            .filter_map(|(index, media)| {
                let mut src = best(media.get("mediaSources")?.as_array()?)?;
                src.insert("num".into(), (index + 1).into());
                Some(src)
            })
            .collect(),
    };
    post.insert("count".into(), files.len().into());
    let id = post.get("id").map(crate::sites::scalar).unwrap_or_default();
    files
        .into_iter()
        .filter_map(|file| {
            let url = crate::sites::scalar(&file["url"]);
            post.extend(file);
            nameext_into(&url, &mut post);
            let num = crate::sites::scalar(&post["num"]);
            Some(Item::new(Url::parse(&url).ok()?, post.clone()).ids(id.clone(), num))
        })
        .collect()
}

/// Descend into the single nested object holding `items` and `iterator`.
fn listing(mut data: Value) -> Value {
    for _ in 0..8 {
        if data.get("items").is_some() || !data.is_object() {
            break;
        }
        data = data
            .as_object()
            .and_then(|m| m.values().next_back().cloned())
            .unwrap_or(Value::Null);
    }
    data
}

/// `_pagination`, starting from already fetched `first` data if it has items.
fn pagination<'a>(
    http: &'a Http,
    route: &'static Route,
    operation: &'static str,
    variables: Value,
    first: Option<Value>,
    queue: bool,
) -> Events<'a> {
    crate::sites::paginate(Some((variables, first)), move |state| async move {
        let Some((mut variables, first)) = state else {
            return Ok(None);
        };
        let data = match first.filter(|d| {
            d.get("items")
                .and_then(Value::as_array)
                .is_some_and(|i| !i.is_empty())
        }) {
            Some(data) => data,
            None => graphql(http, operation, variables.clone(), true).await?,
        };
        let data = listing(data);
        let items = data["items"].as_array().cloned().unwrap_or_default();
        let next = match data.get("iterator").filter(|i| truthy(Some(i))) {
            Some(iterator) => {
                variables["iterator"] = iterator.clone();
                Some((variables, None))
            }
            None => None,
        };
        let events = if queue {
            let children = items
                .iter()
                .filter_map(|sub| {
                    Some((
                        Url::parse(&format!("{ROOT}{}", sub.get("url")?.as_str()?)).ok()?,
                        sub.as_object()?.clone(),
                    ))
                })
                .collect();
            crate::sites::galleries::queue(http, &SITE, route, children)
        } else {
            crate::sites::events(
                items
                    .iter()
                    .flat_map(|post| crate::sites::post(&SITE, http, route, post_items(post)))
                    .collect(),
            )
        };
        Ok(Some((events, next)))
    })
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Scrolller URL"));
    };
    crate::sites::once(async move {
        login(http).await?;
        Ok(match target {
            Target::Post(slug) => {
                let data = graphql(
                    http,
                    "SubredditPostQuery",
                    json!({"url": format!("/{slug}")}),
                    true,
                )
                .await?;
                crate::sites::events(crate::sites::post(
                    &SITE,
                    http,
                    &POST,
                    post_items(&data["getPost"]),
                ))
            }
            Target::Subreddit(path, query) => {
                let filter = parse_query(&query)
                    .get("filter")
                    .map(|f| {
                        Value::from(crate::sites::scalar(f).to_uppercase().trim_end_matches('S'))
                    })
                    .unwrap_or(Value::Null);
                let variables =
                    json!({"url": path, "filter": filter, "sortBy": "RANDOM", "limit": 50});
                let subreddit =
                    graphql(http, "SubredditQuery", variables, true).await?["getSubreddit"].clone();
                let variables = json!({"subredditId": subreddit["id"], "iterator": null, "filter": filter, "sortBy": "RANDOM", "limit": 50, "isNsfw": subreddit["isNsfw"]});
                pagination(
                    http,
                    &SUBREDDIT,
                    "SubredditChildrenQuery",
                    variables,
                    Some(subreddit["children"].clone()),
                    false,
                )
            }
            Target::User(user) => {
                let mut variables = json!({"username": unquote(&user), "iterator": null, "limit": 40, "filter": null, "sortBy": "RANDOM", "isNsfw": true});
                let posts =
                    graphql(http, "UserPostsQuery", variables.clone(), true).await?["getUserPosts"]
                        .clone();
                let first = if posts
                    .get("items")
                    .and_then(Value::as_array)
                    .is_some_and(|i| !i.is_empty())
                {
                    Some(posts)
                } else {
                    variables["isNsfw"] = false.into();
                    None
                };
                pagination(http, &USER, "UserPostsQuery", variables, first, false)
            }
            Target::Following => {
                if TOKEN.lock().unwrap_or_else(|e| e.into_inner()).is_none() {
                    return Err(error("Login required"));
                }
                let variables = json!({"iterator": null, "filter": null, "limit": 10, "isNsfw": false, "sortBy": "RANDOM"});
                pagination(
                    http,
                    &FOLLOWING,
                    "GetFollowingSubreddits",
                    variables,
                    None,
                    true,
                )
            }
        })
    })
}

fn query(operation: &str) -> &'static str {
    match operation {
        "SubredditPostQuery" => SUBREDDIT_POST_QUERY,
        "UserPostsQuery" => USER_POSTS_QUERY,
        "SubredditQuery" => SUBREDDIT_QUERY,
        "SubredditChildrenQuery" => SUBREDDIT_CHILDREN_QUERY,
        "GetFollowingSubreddits" => GET_FOLLOWING_SUBREDDITS,
        "LoginQuery" => LOGIN_QUERY,
        _ => "",
    }
}

const SUBREDDIT_POST_QUERY: &str = r#"query SubredditPostQuery(
    $url: String!
) {
    getPost(
        data: { url: $url }
    ) {
        __typename id url title subredditId subredditTitle subredditUrl
        redditPath isNsfw hasAudio fullLengthSource gfycatSource redgifsSource
        ownerAvatar username displayName favoriteCount isPaid tags
        commentsCount commentsRepliesCount isFavorite
        albumContent { mediaSources { url width height isOptimized } }
        mediaSources { url width height isOptimized }
        blurredMediaSources { url width height isOptimized }
    }
}
"#;

const USER_POSTS_QUERY: &str = r#"query UserPostsQuery(
    $username: String!
    $iterator: String
    $limit: Int!
    $filter: GalleryFilter
    $sortBy: GallerySortBy
    $isNsfw: Boolean
) {
    getUserPosts(
        data: {
            username: $username
            iterator: $iterator
            limit: $limit
            filter: $filter
            sortBy: $sortBy
            isNsfw: $isNsfw
        }
    ) {
        iterator items {
            __typename id url title posted_by reddit_posted_by subredditId
            subredditTitle subredditUrl subredditIsFollowing redditPath isNsfw
            hasAudio fullLengthSource gfycatSource redgifsSource ownerAvatar
            username displayName favoriteCount isPaid tags commentsCount
            commentsRepliesCount duration createdAt isFavorite
            albumContent { mediaSources { url width height isOptimized } }
            mediaSources { url width height isOptimized }
            blurredMediaSources { url width height isOptimized type }
        }
    }
}
"#;

const SUBREDDIT_QUERY: &str = r#"query SubredditQuery(
    $url: String!
    $iterator: String
    $sortBy: GallerySortBy
    $filter: GalleryFilter
    $limit: Int!
) {
    getSubreddit(
        data: {
            url: $url,
            iterator: $iterator,
            filter: $filter,
            limit: $limit,
            sortBy: $sortBy
        }
    ) {
        __typename id url title secondaryTitle description createdAt isNsfw
        subscribers isComplete itemCount videoCount pictureCount albumCount
        isPaid username tags isFollowing
        banner { url width height isOptimized }
        children {
            iterator items {
                __typename id url title subredditId subredditTitle subredditUrl
                redditPath isNsfw hasAudio fullLengthSource gfycatSource
                redgifsSource ownerAvatar username displayName favoriteCount
                isPaid tags commentsCount commentsRepliesCount isFavorite
                albumContent { mediaSources { url width height isOptimized } }
                mediaSources { url width height isOptimized }
                blurredMediaSources { url width height isOptimized }
            }
        }
    }
}
"#;

const SUBREDDIT_CHILDREN_QUERY: &str = r#"query SubredditChildrenQuery(
    $subredditId: Int!
    $iterator: String
    $filter: GalleryFilter
    $sortBy: GallerySortBy
    $limit: Int!
    $isNsfw: Boolean
) {
    getSubredditChildren(
        data: {
            subredditId: $subredditId,
            iterator: $iterator,
            filter: $filter,
            sortBy: $sortBy,
            limit: $limit,
            isNsfw: $isNsfw
        },
    ) {
        iterator items {
            __typename id url title subredditId subredditTitle subredditUrl
            redditPath isNsfw hasAudio fullLengthSource gfycatSource
            redgifsSource ownerAvatar username displayName favoriteCount isPaid
            tags commentsCount commentsRepliesCount isFavorite
            albumContent { mediaSources { url width height isOptimized } }
            mediaSources { url width height isOptimized }
            blurredMediaSources { url width height isOptimized }
        }
    }
}
"#;

const GET_FOLLOWING_SUBREDDITS: &str = r#"query GetFollowingSubreddits(
    $iterator: String,
    $limit: Int!,
    $filter: GalleryFilter,
    $isNsfw: Boolean,
    $sortBy: GallerySortBy
) {
    getFollowingSubreddits(
        data: {
            isNsfw: $isNsfw
            limit: $limit
            filter: $filter
            iterator: $iterator
            sortBy: $sortBy
        }
    ) {
        iterator items {
            __typename id url title secondaryTitle description createdAt isNsfw
            subscribers isComplete itemCount videoCount pictureCount albumCount
            isFollowing
        }
    }
}
"#;

const LOGIN_QUERY: &str = r#"query LoginQuery(
    $username: String!,
    $password: String!
) {
    login(
        username: $username,
        password: $password
    ) {
        username token expiresAt isAdmin status isPremium
    }
}
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http as client, json as body, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://scrolller.com/r/AmateurPhotography?filter=pictures"),
            Some(Target::Subreddit(
                "/r/AmateurPhotography".into(),
                "filter=pictures".into()
            ))
        );
        assert_eq!(
            t("https://scrolller.com/reddit-user/jane"),
            Some(Target::User("jane".into()))
        );
        assert_eq!(
            t("https://scrolller.com/following"),
            Some(Target::Following)
        );
        assert_eq!(
            t("https://scrolller.com/some-title-a1b2c3d4f5"),
            Some(Target::Post("some-title-a1b2c3d4f5".into()))
        );
    }

    #[tokio::test]
    async fn post_picks_best_sources() {
        let post = json!({"data": {"getPost": {"id": 7, "title": "Nice", "subredditTitle": "pics", "albumContent": [
            {"mediaSources": [{"url": "https://images.scrolller.com/a-small.jpg", "width": 100, "isOptimized": false}, {"url": "https://images.scrolller.com/a-big.webp", "width": 900, "isOptimized": true}, {"url": "https://images.scrolller.com/a-big.jpg", "width": 900, "isOptimized": false}]},
            {"mediaSources": []},
            {"mediaSources": [{"url": "https://images.scrolller.com/c.mp4", "width": 640, "isOptimized": false}]}]}}});
        let (root, server) = serve(vec![body(post)]);
        let http = client("scrolller", root);
        let files = media(extract(
            &http,
            Url::parse("https://scrolller.com/nice-a1b2").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].url.as_str(),
            "https://images.scrolller.com/a-big.jpg"
        );
        assert_eq!(keywords(&files[1])["num"], 3);
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "7_003 Nice.mp4");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("scrolller/pics")
        );
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with("POST /admin ") && requests[0].contains("SubredditPostQuery")
        );
    }
}
