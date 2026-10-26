//! Skeb posts, works, sent requests, users, searches and followed
//! creators, mirroring gallery_dl/extractor/skeb.py.
use crate::error;
use crate::sites::gdl::{
    captures, dispatch, extr, group, int_of, nameext_into, quote, re, truthy, unquote,
};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value, json};
use url::Url;

const ROOT: &str = "https://skeb.jp";
const BASE: &str = r"^https?://skeb\.jp";
const FILENAME: &str = "{post_num}_{file_id}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{creator[screen_name]}"];
const ARCHIVE: &str = "{post_num}_{_file_id}_{content_category}";

static POST: Route = Route::new("post", FILENAME, DIRECTORY, ARCHIVE);
static WORKS: Route = Route::new("works", FILENAME, DIRECTORY, ARCHIVE);
static SENTREQUESTS: Route = Route::new("sentrequests", FILENAME, DIRECTORY, ARCHIVE);
static USER: Route = Route::new("user", FILENAME, DIRECTORY, ARCHIVE);
static SEARCH: Route = Route::new("search", FILENAME, DIRECTORY, ARCHIVE);
static FOLLOWING: Route = Route::new("following", FILENAME, DIRECTORY, ARCHIVE);
static FOLLOWING_USERS: Route = Route::new("following-users", FILENAME, DIRECTORY, ARCHIVE);

pub static SITE: Site = Site {
    category: "skeb",
    name: "Skeb",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[
        &POST,
        &WORKS,
        &SENTREQUESTS,
        &USER,
        &SEARCH,
        &FOLLOWING,
        &FOLLOWING_USERS,
    ],
    api_key: false,
    cookies: Some("skeb.jp"),
};

#[derive(Debug, PartialEq)]
enum Target {
    Post(String, String),
    Works(String),
    SentRequests(String),
    User(String),
    Search(String),
    Following(String),
    FollowingUsers,
}

fn target(url: &Url) -> Option<Target> {
    let user = |c: &regex::Captures| group(c, 1).to_owned();
    if let Some(c) = captures(re!(&format!(r"{BASE}/@([^/?#]+)/works/(\d+)")), url) {
        return Some(Target::Post(user(&c), group(&c, 2).into()));
    }
    if let Some(c) = captures(re!(&format!(r"{BASE}/@([^/?#]+)/works")), url) {
        return Some(Target::Works(user(&c)));
    }
    if let Some(c) = captures(re!(&format!(r"{BASE}/@([^/?#]+)/sent[ _-]?requests")), url) {
        return Some(Target::SentRequests(user(&c)));
    }
    if let Some(c) = captures(re!(&format!(r"{BASE}/@([^/?#]+)/?$")), url) {
        return Some(Target::User(user(&c)));
    }
    if let Some(c) = captures(re!(&format!(r"{BASE}/search\?q=([^&#]+)")), url) {
        return Some(Target::Search(group(&c, 1).into()));
    }
    if let Some(c) = captures(re!(&format!(r"{BASE}/@([^/?#]+)/following_creators")), url) {
        return Some(Target::Following(user(&c)));
    }
    captures(re!(&format!(r"{BASE}/following_users")), url).map(|_| Target::FollowingUsers)
}

/// A Skeb API GET, retrying once a 429 page hands out its `request_key`.
async fn api(http: &Http, url: &Url) -> crate::Result<Value> {
    let headers = [
        ("Accept", "application/json, text/plain, */*"),
        ("sec-fetch-mode", "cors"),
        ("sec-fetch-site", "same-origin"),
        ("Authorization", "Bearer null"),
    ];
    for _ in 0..3 {
        let (status, body) = http.get_status(url, &headers).await?;
        if status == 429 {
            let key = extr(&body, "request_key=", ";");
            let base = http.api(ROOT)?;
            if !key.is_empty() {
                http.set_cookie(&base, &format!("request_key={key}; Path=/"));
            } else if http.cookie(&base, "request_key").is_none() {
                return Err(error("Skeb rate limit; retry later"));
            }
            continue;
        }
        if !(200..300).contains(&status) {
            return Err(error(format!("Skeb returned HTTP {status}")));
        }
        return serde_json::from_str(&body).map_err(|_| error("Skeb returned invalid JSON"));
    }
    Err(error("Skeb rate limit; retry later"))
}

fn person(value: &Value) -> Value {
    json!({"id": value["id"], "name": value["name"], "screen_name": value["screen_name"], "avatar_url": value["avatar_url"], "header_url": value["header_url"]})
}

/// `_get_post_data` and `_get_files_from_post` for one post.
async fn post_items(
    http: &Http,
    user: &str,
    num: &str,
    extra: &Map<String, Value>,
) -> crate::Result<Vec<Item>> {
    let resp = api(
        http,
        &http.api(&format!("{ROOT}/api/users/{user}/works/{num}"))?,
    )
    .await?;
    let mut post = Map::new();
    for (key, source) in [
        ("post_id", "id"),
        ("body", "body"),
        ("source_body", "source_body"),
        ("translated_body", "translated"),
        ("nsfw", "nsfw"),
        ("anonymous", "anonymous"),
        ("tags", "tag_list"),
        ("genre", "genre"),
        ("thanks", "thanks"),
        ("source_thanks", "source_thanks"),
        ("translated_thanks", "translated_thanks"),
    ] {
        post.insert(key.into(), resp.get(source).cloned().unwrap_or(Value::Null));
    }
    post.insert("post_num".into(), num.into());
    post.insert(
        "post_url".into(),
        format!("{ROOT}{}", crate::sites::scalar(&resp["path"])).into(),
    );
    post.insert("creator".into(), person(&resp["creator"]));
    if !truthy(resp.get("anonymous")) && resp.get("client").is_some() {
        post.insert("client".into(), person(&resp["client"]));
    }
    post.extend(extra.clone());
    let id = crate::sites::scalar(&resp["id"]);
    let mut files: Vec<Map<String, Value>> = Vec::new();
    let file =
        |category: &str, file_id: Value, internal: Value, url: &Value| -> Map<String, Value> {
            let mut file = Map::new();
            file.insert("content_category".into(), category.into());
            file.insert("file_id".into(), file_id);
            file.insert("_file_id".into(), internal);
            file.insert("file_url".into(), url.clone());
            file
        };
    if truthy(http.option("thumbnails")) && resp.get("og_image_url").is_some() {
        files.push(file(
            "thumb",
            "thumb".into(),
            format!("{id}t").into(),
            &resp["og_image_url"],
        ));
    }
    if truthy(http.option("article")) && truthy(resp.get("article_image_url")) {
        files.push(file(
            "article",
            "article".into(),
            format!("{id}a").into(),
            &resp["article_image_url"],
        ));
    }
    for preview in resp["previews"].as_array().into_iter().flatten() {
        let info = &preview["information"];
        let mut entry = file(
            "preview",
            preview["id"].clone(),
            preview["id"].clone(),
            &preview["url"],
        );
        entry.insert(
            "original".into(),
            json!({"width": info["width"], "height": info["height"], "byte_size": info["byte_size"], "duration": info["duration"], "frame_rate": info.get("frame_rate"),
                   "software": info["software"], "extension": info["extension"], "is_movie": info["is_movie"], "transcoder": info["transcoder"]}),
        );
        files.push(entry);
    }
    post.insert("count".into(), files.len().into());
    Ok(files
        .into_iter()
        .enumerate()
        .filter_map(|(index, file)| {
            let url = crate::sites::scalar(&file["file_url"]);
            post.insert("num".into(), (index + 1).into());
            post.extend(file);
            nameext_into(&url, &mut post);
            let file_id = crate::sites::scalar(&post["_file_id"]);
            Some(Item::new(Url::parse(&url).ok()?, post.clone()).ids(id.clone(), file_id))
        })
        .collect())
}

/// `(user, post number)` of a work path like `/@user/works/12`.
fn path_parts(path: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = path.split('/').collect();
    Some((
        parts.get(1)?.get(1..)?.to_owned(),
        parts.get(3)?.to_string(),
    ))
}

fn posts_events<'a>(
    http: &'a Http,
    route: &'static Route,
    posts: Vec<(String, String)>,
    extra: Map<String, Value>,
) -> Events<'a> {
    crate::sites::each(posts, move |(user, num)| {
        let extra = extra.clone();
        async move {
            // Upstream logs a failing post and continues with the next one.
            let items = post_items(http, &user, &num, &extra)
                .await
                .unwrap_or_default();
            Ok(crate::sites::post(&SITE, http, route, items))
        }
    })
}

/// `_pagination`: public works in pages of 30.
fn works<'a>(
    http: &'a Http,
    route: &'static Route,
    user: String,
    role: &'static str,
) -> Events<'a> {
    crate::sites::paginate(Some(0u64), move |state| {
        let user = user.clone();
        async move {
            let Some(offset) = state else { return Ok(None) };
            let mut url = http.api(&format!("{ROOT}/api/users/{user}/works"))?;
            url.query_pairs_mut()
                .append_pair("role", role)
                .append_pair("sort", "date")
                .append_pair("offset", &offset.to_string());
            let posts = api(http, &url)
                .await?
                .as_array()
                .cloned()
                .unwrap_or_default();
            let next = (posts.len() >= 30).then_some(offset + 30);
            let posts = posts
                .iter()
                .filter(|p| !truthy(p.get("private")))
                .filter_map(|p| path_parts(p.get("path")?.as_str()?))
                .collect();
            Ok(Some((posts_events(http, route, posts, Map::new()), next)))
        }
    })
}

/// `_pagination_users` for followed creators or users.
fn users<'a>(http: &'a Http, route: &'static Route, endpoint: String, sort: bool) -> Events<'a> {
    crate::sites::paginate(Some(0u64), move |state| {
        let endpoint = endpoint.clone();
        async move {
            let Some(offset) = state else { return Ok(None) };
            let mut url = http.api(&format!("{ROOT}/api{endpoint}"))?;
            {
                let mut pairs = url.query_pairs_mut();
                if sort {
                    pairs.append_pair("sort", "date");
                }
                pairs
                    .append_pair("offset", &offset.to_string())
                    .append_pair("limit", "90");
            }
            let data = api(http, &url)
                .await?
                .as_array()
                .cloned()
                .unwrap_or_default();
            let next = (data.len() >= 90).then_some(offset + 90);
            let children = data
                .iter()
                .filter_map(|user| {
                    Some((
                        Url::parse(&format!("{ROOT}/@{}", user.get("screen_name")?.as_str()?))
                            .ok()?,
                        user.as_object()?.clone(),
                    ))
                })
                .collect();
            Ok(Some((
                crate::sites::galleries::queue(http, &SITE, route, children),
                next,
            )))
        }
    })
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Skeb URL"));
    };
    match target {
        Target::Post(user, num) => posts_events(http, &POST, vec![(user, num)], Map::new()),
        Target::Works(user) => works(http, &WORKS, user, "creator"),
        Target::SentRequests(user) => works(http, &SENTREQUESTS, user, "client"),
        Target::User(user) => {
            let base = format!("{ROOT}/@{user}/");
            let views = [
                ("works", format!("{base}works")),
                ("sentrequests", format!("{base}sentrequests")),
            ];
            let default: &[&str] = if truthy(http.option("sent-requests")) {
                &["works", "sentrequests"]
            } else {
                &["works"]
            };
            match dispatch(http, &views, default) {
                Ok(selected) => {
                    let children = selected
                        .into_iter()
                        .filter_map(|(_, url)| Some((Url::parse(url).ok()?, Map::new())))
                        .collect();
                    crate::sites::galleries::queue(http, &SITE, &USER, children)
                }
                Err(e) => crate::sites::fail(e),
            }
        }
        Target::Search(query) => {
            let filters = match http.option("filters") {
                None | Some(Value::Null) => "genre:art OR genre:voice OR genre:novel OR genre:video OR genre:music OR genre:correction".to_owned(),
                Some(Value::String(s)) => s.clone(),
                Some(Value::Array(list)) => list.iter().map(crate::sites::scalar).collect::<Vec<_>>().join(" OR "),
                Some(other) => crate::sites::scalar(other),
            };
            let tags = unquote(&query);
            let mut extra = Map::new();
            extra.insert("search_tags".into(), tags.as_str().into());
            crate::sites::paginate(Some(0i64), move |state| {
                let filters = filters.clone();
                let tags = tags.clone();
                let extra = extra.clone();
                async move {
                    let Some(page) = state else { return Ok(None) };
                    let mut url =
                        http.api("https://hb1jt3kre9-2.algolianet.com/1/indexes/*/queries")?;
                    url.query_pairs_mut().append_pair(
                        "x-algolia-agent",
                        "Algolia for JavaScript (4.13.1); Browser",
                    );
                    let params = format!("hitsPerPage=40&filters={}&page={page}", quote(&filters));
                    let body = json!({"requests": [{"indexName": "Request", "query": tags, "params": params}]});
                    let headers = [
                        ("Origin", ROOT),
                        ("x-algolia-api-key", "9a4ce7d609e71bf29e977925e4c6740c"),
                        ("x-algolia-application-id", "HB1JT3KRE9"),
                    ];
                    let result = http.post_json(&url, &body, &headers).await?["results"][0].clone();
                    let posts = result["hits"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|p| path_parts(p.get("path")?.as_str()?))
                        .collect();
                    let next = (page < int_of(result.get("nbPages"))).then_some(page + 1);
                    Ok(Some((posts_events(http, &SEARCH, posts, extra), next)))
                }
            })
        }
        Target::Following(user) => users(
            http,
            &FOLLOWING,
            format!("/users/{user}/following_creators"),
            true,
        ),
        Target::FollowingUsers => users(http, &FOLLOWING_USERS, "/following_users".into(), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{
        children, http as client, json as body, keywords, media, response, serve,
    };

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://skeb.jp/@kanade_cocotte/works/38"),
            Some(Target::Post("kanade_cocotte".into(), "38".into()))
        );
        assert_eq!(
            t("https://skeb.jp/@kanade_cocotte/sent-requests"),
            Some(Target::SentRequests("kanade_cocotte".into()))
        );
        assert_eq!(
            t("https://skeb.jp/@kanade_cocotte"),
            Some(Target::User("kanade_cocotte".into()))
        );
        assert_eq!(
            t("https://skeb.jp/search?q=bunny%20tree"),
            Some(Target::Search("bunny%20tree".into()))
        );
        assert_eq!(
            t("https://skeb.jp/following_users"),
            Some(Target::FollowingUsers)
        );
    }

    #[tokio::test]
    async fn post_retries_after_request_key() {
        let challenge = response(
            "429 Too Many Requests",
            "text/html",
            b"<script>document.cookie='request_key=KEY; path=/';</script>",
        );
        let post = json!({"id": 5, "path": "/@jane/works/38", "body": "b", "anonymous": false, "tag_list": ["x"], "genre": "art",
            "creator": {"id": 1, "name": "Jane", "screen_name": "jane"}, "client": {"id": 2, "name": "Bob", "screen_name": "bob"},
            "previews": [{"id": 77, "url": "https://si.imgix.net/a/b.png?w=1", "information": {"width": 10, "height": 20, "extension": "png"}}]});
        let (root, server) = serve(vec![challenge, body(post)]);
        let http = client("skeb", root);
        let files = media(extract(
            &http,
            Url::parse("https://skeb.jp/@jane/works/38").unwrap(),
        ))
        .await;
        let k = keywords(&files[0]);
        assert_eq!(
            (
                k["client"]["name"].as_str(),
                k["content_category"].as_str(),
                k["original"]["width"].as_i64()
            ),
            (Some("Bob"), Some("preview"), Some(10))
        );
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "38_77.png");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("skeb/jane")
        );
        let requests = server.join().unwrap();
        assert!(requests[1].contains("request_key=KEY"));
    }

    #[tokio::test]
    async fn user_dispatches_works() {
        let (root, server) = serve(vec![]);
        let http = client("skeb", root);
        assert_eq!(
            children(extract(&http, Url::parse("https://skeb.jp/@jane").unwrap()))
                .await
                .len(),
            1
        );
        server.join().unwrap();
    }
}
