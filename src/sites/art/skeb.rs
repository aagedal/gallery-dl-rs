//! Skeb works, sent requests, searches and followed creators, mirroring
//! gallery_dl/extractor/skeb.py through the `skeb.jp/api` JSON API and the
//! site's Algolia search index.
use super::{leading_digits, nameext_into, str_of, truthy};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use futures_util::{StreamExt, stream};
use serde_json::{Map, Value, json};
use url::Url;

const ROOT: &str = "https://skeb.jp";
const HEADERS: &[(&str, &str)] = &[
    ("Accept", "application/json, text/plain, */*"),
    ("sec-fetch-mode", "cors"),
    ("sec-fetch-site", "same-origin"),
    ("Authorization", "Bearer null"),
];
const SEARCH_URL: &str = "https://hb1jt3kre9-2.algolianet.com/1/indexes/*/queries";
const SEARCH_HEADERS: &[(&str, &str)] = &[
    ("Origin", ROOT),
    ("x-algolia-api-key", "9a4ce7d609e71bf29e977925e4c6740c"),
    ("x-algolia-application-id", "HB1JT3KRE9"),
];
const FILTERS: &str =
    "genre:art OR genre:voice OR genre:novel OR genre:video OR genre:music OR genre:correction";

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

const FILENAME: &str = "{post_num}_{file_id}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{creator[screen_name]}"];
const ARCHIVE: &str = "{post_num}_{_file_id}_{content_category}";

static POST: Route = Route::new("post", FILENAME, DIRECTORY, ARCHIVE);
static WORKS: Route = Route::new("works", FILENAME, DIRECTORY, ARCHIVE);
static SENTREQUESTS: Route = Route::new("sentrequests", FILENAME, DIRECTORY, ARCHIVE);
// `Dispatch` sets the subcategory; formats come from `SkebExtractor`.
static USER: Route = Route::new("user", FILENAME, DIRECTORY, ARCHIVE);
static SEARCH: Route = Route::new("search", FILENAME, DIRECTORY, ARCHIVE);
static FOLLOWING: Route = Route::new("following", FILENAME, DIRECTORY, ARCHIVE);
static FOLLOWING_USERS: Route = Route::new("following-users", FILENAME, DIRECTORY, ARCHIVE);

#[derive(Debug, PartialEq)]
enum Target {
    Post(String, String),
    Works(String),
    SentRequests(String),
    User(String),
    /// The raw `q` value.
    Search(String),
    Following(String),
    FollowingUsers,
}

fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) || url.host_str() != Some("skeb.jp") {
        return None;
    }
    let path = url.path();
    if path.starts_with("/following_users") {
        return Some(Target::FollowingUsers);
    }
    if path == "/search" {
        let query = url.query()?.strip_prefix("q=")?;
        let value = query.split('&').next().unwrap_or_default();
        return (!value.is_empty()).then(|| Target::Search(value.to_owned()));
    }
    let rest = path.strip_prefix("/@")?;
    let end = rest.find('/').unwrap_or(rest.len());
    let (user, rest) = rest.split_at(end);
    if user.is_empty() {
        return None;
    }
    let user = user.to_owned();
    if let Some(num) = rest.strip_prefix("/works/").and_then(leading_digits) {
        return Some(Target::Post(user, num.to_owned()));
    }
    if rest.starts_with("/works") {
        return Some(Target::Works(user));
    }
    if let Some(sent) = rest.strip_prefix("/sent") {
        let sent = sent.strip_prefix(['_', '-']).unwrap_or(sent);
        let sent = sent.strip_prefix("%20").unwrap_or(sent);
        if sent.starts_with("requests") {
            return Some(Target::SentRequests(user));
        }
    }
    if matches!(rest, "" | "/") && url.query().is_none() && url.fragment().is_none() {
        return Some(Target::User(user));
    }
    if rest.starts_with("/following_creators") {
        return Some(Target::Following(user));
    }
    None
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Skeb URL"));
    };
    match target {
        Target::Post(user, num) => crate::sites::once(async move {
            let items = post(http, &user, &num, &Map::new()).await?;
            Ok(crate::sites::events(crate::sites::post(
                &SITE, http, &POST, items,
            )))
        }),
        Target::Works(user) => works(http, &WORKS, &user, "creator"),
        Target::SentRequests(user) => works(http, &SENTREQUESTS, &user, "client"),
        Target::User(user) => dispatch(http, &user),
        Target::Search(query) => search(http, &query),
        Target::Following(user) => users(
            http,
            &FOLLOWING,
            format!("/users/{user}/following_creators"),
            vec![("sort", "date".into())],
        ),
        Target::FollowingUsers => users(
            http,
            &FOLLOWING_USERS,
            "/following_users".into(),
            Vec::new(),
        ),
    }
}

/// Upstream `Dispatch` for profiles: queue the `include` extractors.
fn dispatch<'a>(http: &'a Http, user: &str) -> Events<'a> {
    let base = format!("{ROOT}/@{user}/");
    let all = ["works", "sentrequests"];
    let include: Vec<String> = match http.option("include") {
        Some(Value::String(s)) if s == "all" => all.iter().map(|s| (*s).to_owned()).collect(),
        Some(Value::String(s)) => s.replace(' ', "").split(',').map(str::to_owned).collect(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        Some(value) if !truthy(Some(value)) => Vec::new(),
        _ if http.option_bool("sent-requests", false) => {
            vec!["works".into(), "sentrequests".into()]
        }
        _ => vec!["works".into()],
    };
    let children: Vec<Url> = include
        .iter()
        .filter(|name| all.contains(&name.as_str()))
        .filter_map(|name| Url::parse(&format!("{base}{name}")).ok())
        .collect();
    stream::iter(children)
        .map(move |child| crate::sites::queue(http, "skeb", "user", child, &Map::new()))
        .flatten()
        .boxed()
}

/// GET JSON with the API headers. A first HTTP 429 that sets the
/// `request_key` cookie is retried once with it, as upstream `_handle_429`.
async fn get_json(http: &Http, url: &Url) -> Result<Value> {
    let before = http.cookie(url, "request_key");
    match http.json_with(url, HEADERS).await {
        Err(_)
            if http
                .cookie(url, "request_key")
                .is_some_and(|k| Some(&k) != before.as_ref()) =>
        {
            http.json_with(url, HEADERS).await
        }
        result => result,
    }
}

/// Upstream `_get_post_data` and `_get_files_from_post` for one work.
async fn post(http: &Http, user: &str, num: &str, extra: &Map<String, Value>) -> Result<Vec<Item>> {
    let url = http.api(&format!("{ROOT}/api/users/{user}/works/{num}"))?;
    let resp = get_json(http, &url).await?;
    let thumbnails = http.option_bool("thumbnails", false);
    let article = http.option_bool("article", false);
    Ok(parse_post(&resp, num, extra, thumbnails, article))
}

fn person(value: Option<&Value>) -> Value {
    let field = |key: &str| {
        value
            .and_then(|v| v.get(key))
            .cloned()
            .unwrap_or(Value::Null)
    };
    json!({
        "id": field("id"),
        "name": field("name"),
        "screen_name": field("screen_name"),
        "avatar_url": field("avatar_url"),
        "header_url": field("header_url"),
    })
}

pub(crate) fn parse_post(
    resp: &Value,
    num: &str,
    extra: &Map<String, Value>,
    thumbnails: bool,
    article: bool,
) -> Vec<Item> {
    let field = |key: &str| resp.get(key).cloned().unwrap_or(Value::Null);
    let mut data = Map::new();
    data.insert("post_id".into(), field("id"));
    data.insert("post_num".into(), num.into());
    data.insert(
        "post_url".into(),
        format!("{ROOT}{}", str_of(resp, "path")).into(),
    );
    for (key, name) in [
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
        data.insert(key.into(), field(name));
    }
    data.insert("creator".into(), person(resp.get("creator")));
    if !truthy(resp.get("anonymous")) && resp.get("client").is_some_and(Value::is_object) {
        data.insert("client".into(), person(resp.get("client")));
    }
    super::update(&mut data, extra);

    let id = resp.get("id").map(crate::sites::scalar).unwrap_or_default();
    let mut files = Vec::new();
    if thumbnails && resp.get("og_image_url").is_some() {
        files.push(json!({"content_category": "thumb", "file_id": "thumb",
            "_file_id": format!("{id}t"), "file_url": field("og_image_url")}));
    }
    if article && truthy(resp.get("article_image_url")) {
        files.push(json!({"content_category": "article", "file_id": "article",
            "_file_id": format!("{id}a"), "file_url": field("article_image_url")}));
    }
    for preview in resp
        .get("previews")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let info = |key: &str| {
            preview
                .get("information")
                .and_then(|i| i.get(key))
                .cloned()
                .unwrap_or(Value::Null)
        };
        let preview_id = preview.get("id").cloned().unwrap_or(Value::Null);
        files.push(json!({"content_category": "preview", "file_id": preview_id,
            "_file_id": preview_id, "file_url": preview.get("url").cloned().unwrap_or(Value::Null),
            "original": {"width": info("width"), "height": info("height"),
                "byte_size": info("byte_size"), "duration": info("duration"),
                "frame_rate": info("frame_rate"), "software": info("software"),
                "extension": info("extension"), "is_movie": info("is_movie"),
                "transcoder": info("transcoder")}}));
    }
    data.insert("count".into(), files.len().into());
    let user = str_of(&data["creator"], "screen_name").to_owned();
    let mut items = Vec::new();
    for (index, file) in files.into_iter().enumerate() {
        let Value::Object(file) = file else {
            continue;
        };
        super::update(&mut data, &file);
        data.insert("num".into(), (index + 1).into());
        let raw = data
            .get("file_url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let Some(url) = super::file_url(&raw, ROOT) else {
            continue;
        };
        nameext_into(url.as_str(), &mut data);
        let media_id = crate::sites::scalar(&data["_file_id"]);
        items.push(
            Item::new(url, data.clone())
                .ids(num, media_id)
                .user(Some(user.clone())),
        );
    }
    items
}

/// Post events for `(user, num)` pairs, each fetched when reached. Failing
/// posts are skipped, as upstream logs them and continues.
fn post_stream<'a>(
    http: &'a Http,
    route: &'static Route,
    posts: Vec<(String, String)>,
    extra: Map<String, Value>,
) -> Events<'a> {
    stream::iter(posts)
        .map(move |(user, num)| {
            let extra = extra.clone();
            crate::sites::once(async move {
                let items = post(http, &user, &num, &extra).await.unwrap_or_default();
                Ok(crate::sites::events(crate::sites::post(
                    &SITE, http, route, items,
                )))
            })
        })
        .flatten()
        .boxed()
}

/// `@USER/works/NUM` of a listed post path.
fn path_parts(path: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = path.split('/').collect();
    let user = parts.get(1)?.get(1..)?;
    let num = parts.get(3)?;
    (super::segment(user) && super::digits(num)).then(|| (user.to_owned(), (*num).to_owned()))
}

/// Upstream `_pagination`: 30 works per `offset` page; private works are
/// skipped.
fn works<'a>(http: &'a Http, route: &'static Route, user: &str, role: &'static str) -> Events<'a> {
    let endpoint = format!("{ROOT}/api/users/{user}/works");
    crate::sites::paginate(Some(0u64), move |offset| {
        let endpoint = endpoint.clone();
        async move {
            let Some(offset) = offset else {
                return Ok(None);
            };
            let mut url = http.api(&endpoint)?;
            url.query_pairs_mut()
                .append_pair("role", role)
                .append_pair("sort", "date")
                .append_pair("offset", &offset.to_string());
            let data = get_json(http, &url).await?;
            let list = data.as_array().cloned().unwrap_or_default();
            let posts = list
                .iter()
                .filter(|post| !truthy(post.get("private")))
                .filter_map(|post| path_parts(str_of(post, "path")))
                .collect();
            let next = (list.len() >= 30).then_some(offset + 30);
            Ok(Some((post_stream(http, route, posts, Map::new()), next)))
        }
    })
}

/// gallery-dl `text.quote` (Python `urllib.parse.quote` with `/` safe).
fn quote(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' | b'/' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Upstream `SkebSearchExtractor`: Algolia `Request` index pages of 40.
fn search<'a>(http: &'a Http, raw: &str) -> Events<'a> {
    let query = crate::sites::percent_decode(raw);
    let filters = match http.option("filters") {
        None | Some(Value::Null) => FILTERS.to_owned(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .map(crate::sites::scalar)
            .collect::<Vec<_>>()
            .join(" OR "),
        Some(other) => crate::sites::scalar(other),
    };
    let prefix = format!("hitsPerPage=40&filters={}&page=", quote(&filters));
    let mut extra = Map::new();
    extra.insert("search_tags".into(), query.clone().into());
    crate::sites::paginate(Some(0u64), move |page| {
        let (query, prefix, extra) = (query.clone(), prefix.clone(), extra.clone());
        async move {
            let Some(page) = page else {
                return Ok(None);
            };
            let mut url = http.api(SEARCH_URL)?;
            url.query_pairs_mut().append_pair(
                "x-algolia-agent",
                "Algolia for JavaScript (4.13.1); Browser",
            );
            let body = json!({"requests": [{"indexName": "Request", "query": query,
                "params": format!("{prefix}{page}")}]});
            let data = http.post_json(&url, &body, SEARCH_HEADERS).await?;
            let result = data
                .get("results")
                .and_then(|r| r.get(0))
                .ok_or_else(|| error("Skeb search returned no results"))?;
            let posts = result
                .get("hits")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|hit| path_parts(str_of(hit, "path")))
                .collect();
            let pages = result.get("nbPages").and_then(Value::as_u64).unwrap_or(0);
            let next = (page < pages).then_some(page + 1);
            Ok(Some((post_stream(http, &SEARCH, posts, extra), next)))
        }
    })
}

/// Upstream `_pagination_users` and `items_users`: queue each followed
/// creator's profile with its user object.
fn users<'a>(
    http: &'a Http,
    route: &'static Route,
    endpoint: String,
    params: Vec<(&'static str, String)>,
) -> Events<'a> {
    const LIMIT: u64 = 90;
    let subcategory = route.subcategory;
    crate::sites::paginate(Some(0u64), move |offset| {
        let (endpoint, params) = (endpoint.clone(), params.clone());
        async move {
            let Some(offset) = offset else {
                return Ok(None);
            };
            let mut url = http.api(&format!("{ROOT}/api{endpoint}"))?;
            {
                let mut query = url.query_pairs_mut();
                for (key, value) in &params {
                    query.append_pair(key, value);
                }
                query
                    .append_pair("offset", &offset.to_string())
                    .append_pair("limit", &LIMIT.to_string());
            }
            let data = get_json(http, &url).await?;
            let list = data.as_array().cloned().unwrap_or_default();
            let next = (list.len() as u64 >= LIMIT).then_some(offset + LIMIT);
            let children: Vec<(Url, Map<String, Value>)> = list
                .into_iter()
                .filter_map(|user| {
                    let name = str_of(&user, "screen_name");
                    if !super::segment(name) {
                        return None;
                    }
                    let url = Url::parse(&format!("{ROOT}/@{name}")).ok()?;
                    Some((url, user.as_object()?.clone()))
                })
                .collect();
            let events = stream::iter(children)
                .map(move |(url, data)| crate::sites::queue(http, "skeb", subcategory, url, &data))
                .flatten()
                .boxed();
            Ok(Some((events, next)))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http, json, keywords, media, queued, response, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://skeb.jp/@kanade_cocotte/works/38"),
            Some(Target::Post("kanade_cocotte".into(), "38".into()))
        );
        assert_eq!(
            t("https://skeb.jp/@kanade_cocotte/works"),
            Some(Target::Works("kanade_cocotte".into()))
        );
        for raw in [
            "https://skeb.jp/@u/sentrequests",
            "https://skeb.jp/@u/sent-requests",
            "https://skeb.jp/@u/sent_requests",
        ] {
            assert_eq!(t(raw), Some(Target::SentRequests("u".into())), "{raw}");
        }
        assert_eq!(t("https://skeb.jp/@u"), Some(Target::User("u".into())));
        assert_eq!(t("https://skeb.jp/@u/"), Some(Target::User("u".into())));
        assert_eq!(
            t("https://skeb.jp/search?q=%23tag&x=1"),
            Some(Target::Search("%23tag".into()))
        );
        assert_eq!(
            t("https://skeb.jp/@u/following_creators"),
            Some(Target::Following("u".into()))
        );
        assert_eq!(
            t("https://skeb.jp/following_users"),
            Some(Target::FollowingUsers)
        );
        for raw in [
            "https://skeb.jp/@u?x=1",
            "https://skeb.jp/@/works",
            "https://skeb.jp/@u/other",
            "https://skeb.jp/search?x=1",
            "https://skeb.jp/search?q=",
            "https://www.skeb.jp/@u/works",
            "https://skeb.jp.evil.com/@u/works",
            "https://evilskeb.jp/@u/works",
            "https://skeb.jp:8443/@u/works",
            "https://u:p@skeb.jp/@u/works",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn work(id: u64, num: u64) -> Value {
        json!({"id": id, "path": format!("/@artist/works/{num}"), "body": "Please",
            "source_body": null, "translated": null, "nsfw": false, "anonymous": false,
            "tag_list": ["a"], "genre": "art", "thanks": "Thanks",
            "source_thanks": null, "translated_thanks": null,
            "og_image_url": "https://si.imgix.net/x/og?w=1",
            "creator": {"id": 1, "name": "Artist", "screen_name": "artist",
                "avatar_url": "a", "header_url": "h", "extra": true},
            "client": {"id": 2, "name": "Client", "screen_name": "client",
                "avatar_url": "a", "header_url": "h"},
            "previews": [{"id": 900 + num, "url": format!("https://si.imgix.net/x/uploads/origins/p{num}.png?s=1"),
                "information": {"width": 10, "height": 20, "byte_size": 30,
                    "duration": null, "software": "", "extension": "png",
                    "is_movie": false, "transcoder": "image"}}]})
    }

    #[tokio::test]
    async fn extracts_a_work_after_the_request_key_challenge() {
        let challenge = response("429 Too Many Requests", "text/html", b"loading");
        let challenge = String::from_utf8(challenge).unwrap().replacen(
            "\r\n\r\n",
            "\r\nSet-Cookie: request_key=k1;path=/\r\n\r\n",
            1,
        );
        let (root, server) = serve(vec![challenge.into_bytes(), json(work(55, 7))]);
        let http = http("skeb", root).with_options(Some(std::sync::Arc::new(
            serde_json::from_value(json!({"thumbnails": true})).unwrap(),
        )));
        let files = media(extract(
            &http,
            Url::parse("https://skeb.jp/@artist/works/7").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[1]);
        assert_eq!(k["post_num"], "7");
        assert_eq!(k["post_id"], 55);
        assert_eq!(k["post_url"], "https://skeb.jp/@artist/works/7");
        assert_eq!(k["translated_body"], Value::Null);
        assert_eq!(k["tags"], json!(["a"]));
        assert_eq!(k["client"]["screen_name"], "client");
        assert!(k["creator"].get("extra").is_none());
        assert_eq!(k["count"], 2);
        assert_eq!(k["num"], 2);
        assert_eq!(k["original"]["width"], 10);
        assert_eq!(k["original"]["frame_rate"], Value::Null);
        assert_eq!(keywords(&files[0])["file_id"], "thumb");
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "7_907.png");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("skeb/artist")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(
            identity,
            r#"["{post_num}_{_file_id}_{content_category}",["7","55t","thumb"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/users/artist/works/7 "));
        assert!(requests[0].contains("authorization: Bearer null"));
        assert!(
            requests[1].contains("cookie: request_key=k1"),
            "{}",
            requests[1]
        );
    }

    #[tokio::test]
    async fn works_page_by_offset() {
        let first: Vec<Value> = (1..=30)
            .map(|n| json!({"path": format!("/@artist/works/{n}"), "private": n != 1}))
            .collect();
        let (root, server) = serve(vec![
            json(Value::Array(first)),
            json(work(1, 1)),
            json(json!([{"path": "/@artist/works/31", "private": false}])),
            json(work(31, 31)),
        ]);
        let http = http("skeb", root);
        let files = media(extract(
            &http,
            Url::parse("https://skeb.jp/@artist/sentrequests").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(keywords(&files[1])["subcategory"], "sentrequests");
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with("GET /api/users/artist/works?role=client&sort=date&offset=0 ")
        );
        assert!(requests[1].starts_with("GET /api/users/artist/works/1 "));
        assert!(requests[2].contains("&offset=30 "));
        assert!(requests[3].starts_with("GET /api/users/artist/works/31 "));
    }

    #[tokio::test]
    async fn search_posts_to_algolia() {
        let (root, server) = serve(vec![
            json(json!({"results": [{"hits": [{"path": "/@artist/works/3"}], "nbPages": 0}]})),
            json(work(3, 3)),
        ]);
        let http = http("skeb", root);
        let files = media(extract(
            &http,
            Url::parse("https://skeb.jp/search?q=%E7%8C%AB").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(keywords(&files[0])["search_tags"], "猫");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with(
            "POST /1/indexes/*/queries?x-algolia-agent=Algolia+for+JavaScript+%284.13.1%29%3B+Browser "
        ));
        assert!(requests[0].contains("x-algolia-application-id: HB1JT3KRE9"));
        assert!(
            requests[0].contains(
                r#""params":"hitsPerPage=40&filters=genre%3Aart%20OR%20genre%3Avoice%20OR%20genre%3Anovel%20OR%20genre%3Avideo%20OR%20genre%3Amusic%20OR%20genre%3Acorrection&page=0""#
            ),
            "{}",
            requests[0]
        );
        assert!(requests[0].contains(r#""query":"猫""#));
    }

    #[tokio::test]
    async fn following_queues_creators() {
        let (root, server) = serve(vec![json(json!([
            {"screen_name": "alice", "id": 1}, {"screen_name": "../x", "id": 2}
        ]))]);
        let http = http("skeb", root);
        let children = queued(extract(
            &http,
            Url::parse("https://skeb.jp/@me/following_creators").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].subcategory(), "following");
        assert_eq!(children[0].fields()["id"], 1);
        let requests = server.join().unwrap();
        assert!(
            requests[0]
                .starts_with("GET /api/users/me/following_creators?sort=date&offset=0&limit=90 ")
        );
    }

    #[tokio::test]
    async fn profiles_dispatch_to_works() {
        let http = http("skeb", Url::parse("http://127.0.0.1:9/").unwrap());
        let children = queued(extract(&http, Url::parse("https://skeb.jp/@u").unwrap())).await;
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].subcategory(), "user");
    }
}
