//! Boosty users, media albums, feeds, posts, subscriptions and direct
//! messages, mirroring gallery_dl/extractor/boosty.py. Paid content needs
//! the `auth` cookie, whose `accessToken` is sent as a bearer token.
use crate::error;
use crate::event::ExtractionEvent;
use crate::sites::gdl::{
    captures, extr, group, int_of, nameext_into, nested_date, parse_query, re, truthy, unquote,
};
use crate::sites::{Events, Http, Item, Route, Site, scalar};
use serde_json::{Map, Value};
use std::sync::Arc;
use std::time::Duration;
use url::Url;

const ROOT: &str = "https://www.boosty.to";
const API: &str = "https://api.boosty.to";
const FILENAME: &str = "{num:>02} {file[id]}.{extension}";
const DIRECTORY: &[&str] = &[
    "{category}",
    "{user[blogUrl]} ({user[id]})",
    "{post[date]:%Y-%m-%d} {post[int_id]}",
];

static USER: Route = Route::new("user", FILENAME, DIRECTORY, "{file[id]}");
static MEDIA: Route = Route::new(
    "media",
    "{post[id]}_{num}.{extension}",
    &["{category}", "{user[blogUrl]} ({user[id]})", "media"],
    "{file[id]}",
);
static FEED: Route = Route::new("feed", FILENAME, DIRECTORY, "{file[id]}");
static POST: Route = Route::new("post", FILENAME, DIRECTORY, "{file[id]}");
static FOLLOWING: Route = Route::new("following", FILENAME, DIRECTORY, "{file[id]}");
static MESSAGES: Route = Route::new(
    "direct-messages",
    FILENAME,
    &[
        "{category}",
        "{user[blogUrl]} ({user[id]})",
        "Direct Messages",
    ],
    "{file[id]}",
);

pub static SITE: Site = Site {
    category: "boosty",
    name: "Boosty",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&USER, &MEDIA, &FEED, &POST, &FOLLOWING, &MESSAGES],
    api_key: false,
    cookies: Some("boosty.to"),
};

#[derive(Debug, PartialEq)]
enum Target {
    User(String, String),
    Media(String, String, String),
    Feed(String),
    Post(String, String),
    Following,
    Messages(String),
}

fn target(url: &Url) -> Option<Target> {
    const BASE: &str = r"^https?://(?:www\.)?boosty\.to";
    if let Some(c) = captures(re!(&format!(r"{BASE}/([^/?#]+)(?:\?([^#]+))?$")), url) {
        return Some(Target::User(group(&c, 1).into(), group(&c, 2).into()));
    }
    if let Some(c) = captures(
        re!(&format!(r"{BASE}/([^/?#]+)/media/([^/?#]+)(?:\?([^#]+))?")),
        url,
    ) {
        return Some(Target::Media(
            group(&c, 1).into(),
            group(&c, 2).into(),
            group(&c, 3).into(),
        ));
    }
    if let Some(c) = captures(re!(&format!(r"{BASE}/(?:\?([^#]+))?(?:$|#)")), url) {
        return Some(Target::Feed(group(&c, 1).into()));
    }
    if let Some(c) = captures(re!(&format!(r"{BASE}/([^/?#]+)/posts/([0-9a-f-]+)")), url) {
        return Some(Target::Post(group(&c, 1).into(), group(&c, 2).into()));
    }
    if captures(re!(&format!(r"{BASE}/app/settings/subscriptions")), url).is_some() {
        return Some(Target::Following);
    }
    captures(re!(&format!(r"{BASE}/app/messages/?\?dialogId=(\d+)")), url)
        .map(|c| Target::Messages(group(&c, 1).into()))
}

/// `BoostyAPI.headers`: the bearer token comes from the `auth` cookie.
fn api_headers(http: &Http) -> Vec<(&'static str, String)> {
    let mut headers = vec![
        ("Accept", "application/json, text/plain, */*".to_owned()),
        ("Origin", ROOT.to_owned()),
    ];
    let auth = Url::parse("https://boosty.to/")
        .ok()
        .and_then(|url| http.cookie(&url, "auth"))
        .map(|auth| unquote(&auth));
    if let Some(token) = auth
        .as_deref()
        .map(|auth| extr(auth, "\"accessToken\":\"", "\""))
        .filter(|token| !token.is_empty())
    {
        headers.push(("Authorization", format!("Bearer {token}")));
    }
    headers
}

type Params = Vec<(String, String)>;

fn set(params: &mut Params, name: &str, value: &str) {
    match params.iter_mut().find(|(k, _)| k == name) {
        Some(entry) => entry.1 = value.to_owned(),
        None => params.push((name.to_owned(), value.to_owned())),
    }
}

fn has(params: &Params, name: &str) -> bool {
    params.iter().any(|(k, _)| k == name)
}

/// `_merge_params`: web query names mapped to their API equivalents.
fn merge_params(web: &str, defaults: &[(&str, &str)]) -> Params {
    let mut params: Params = defaults
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    for (name, value) in parse_query(web) {
        let name = match name.as_str() {
            "isOnlyAllowedPosts" => "is_only_allowed",
            "postsTagsIds" => "tags_ids",
            "postsFrom" => "from_ts",
            "postsTo" => "to_ts",
            other => other,
        };
        set(&mut params, name, &scalar(&value));
    }
    params
}

/// `BoostyAPI._call`: redirects mean an invalid token, 429 waits.
async fn call(http: &Http, endpoint: &str, params: &Params) -> crate::Result<Value> {
    let mut url = http.api(&format!("{API}{endpoint}"))?;
    if !params.is_empty() {
        url.query_pairs_mut().extend_pairs(params);
    }
    let headers = api_headers(http);
    let headers: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    loop {
        let (status, body) = http.get_status(&url, &headers).await?;
        match status {
            0..300 => {
                return serde_json::from_str(&body)
                    .map_err(|_| error("Boosty returned invalid JSON"));
            }
            300..400 => return Err(error("Invalid API access token")),
            429 => tokio::time::sleep(Duration::from_secs(600)).await,
            _ => return Err(error(format!("Boosty API request failed ({status})"))),
        }
    }
}

/// `BoostyAPI.user`.
async fn user(http: &Http, name: &str) -> crate::Result<Value> {
    let mut user = call(http, &format!("/v1/blog/{name}"), &Vec::new()).await?;
    let id = user["owner"]["id"].clone();
    if let Some(map) = user.as_object_mut() {
        map.insert("id".into(), id);
    }
    Ok(user)
}

/// Per-run settings shared by every post of one extraction.
struct Context {
    route: &'static Route,
    /// `self._user`: replaces each post's own `user`.
    user: Option<Value>,
    videos: Vec<String>,
    /// Posts need `hasAccess`, and files get the API headers.
    posts: bool,
}

/// The `videos` option: preferred `ok_video` formats, best first.
fn videos(http: &Http) -> Vec<String> {
    const ALL: [&str; 8] = [
        "ultra_hd", "quad_hd", "full_hd", "high", "medium", "low", "lowest", "tiny",
    ];
    match http.option("videos") {
        Some(Value::String(s)) if !s.is_empty() => s.split(',').map(str::to_owned).collect(),
        Some(Value::Array(list)) => list.iter().map(scalar).collect(),
        None => ALL.map(str::to_owned).to_vec(),
        Some(value) if truthy(Some(value)) => ALL.map(str::to_owned).to_vec(),
        Some(_) => Vec::new(),
    }
}

/// `_update_url`: append the signed query and migration flag.
fn update_url(post: &Map<String, Value>, mut block: Map<String, Value>) -> Map<String, Value> {
    let mut url = block.get("url").map(scalar).unwrap_or_default();
    let mut sep = if url.contains('?') { '&' } else { '?' };
    if let Some(signed) = post
        .get("signedQuery")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        url.push(sep);
        url.push_str(signed.get(1..).unwrap_or_default());
        sep = '&';
    }
    if let Some(migrated) = post.get("isMigrated").filter(|m| !m.is_null()) {
        url.push(sep);
        url.push_str("is_migrated=");
        url.push_str(&scalar(migrated).to_lowercase());
    }
    block.insert("url".into(), url.into());
    block
}

/// `_extract_files`: files of a post's data blocks, collecting its text
/// `content` and `links` on the way.
fn extract_files(post: &mut Map<String, Value>, videos: &[String]) -> Vec<Map<String, Value>> {
    let mut files = Vec::new();
    let mut content = Vec::new();
    let mut links = Vec::new();
    if let Some(created) = post.get("createdAt").and_then(crate::sites::gdl::parse_ts) {
        post.insert("date".into(), nested_date(Some(created)));
    }
    let blocks = post.remove("data");
    for block in blocks
        .as_ref()
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(block) = block.as_object().cloned() else {
            continue;
        };
        match block
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "text" => {
                if block.get("modificator").and_then(Value::as_str) == Some("BLOCK_END") {
                    continue;
                }
                if let Some(first) = block
                    .get("content")
                    .and_then(Value::as_str)
                    .and_then(|c| serde_json::from_str::<Value>(c).ok())
                    .and_then(|c| c.get(0).cloned())
                {
                    content.push(first);
                }
            }
            "image" | "audio_file" | "file" => files.push(update_url(post, block)),
            "ok_video" => {
                let formats: Vec<String> = videos
                    .iter()
                    .filter_map(|wanted| {
                        block
                            .get("playerUrls")?
                            .as_array()?
                            .iter()
                            .rev()
                            .find(|fmt| fmt["type"] == wanted.as_str() && truthy(fmt.get("url")))
                            .map(|fmt| scalar(&fmt["url"]))
                    })
                    .collect();
                if let Some((first, rest)) = formats.split_first() {
                    let mut block = block;
                    block.insert("url".into(), first.as_str().into());
                    block.insert("_fallback".into(), rest.to_vec().into());
                    files.push(block);
                }
            }
            "link" => {
                let url = block.get("url").cloned().unwrap_or(Value::Null);
                links.push(url.clone());
                content.push(url);
            }
            "smile" => content
                .push(format!(":{}:", block.get("name").map(scalar).unwrap_or_default()).into()),
            _ => {}
        }
    }
    post.insert("content".into(), content.into());
    post.insert("links".into(), links.into());
    files
}

/// One post's (or message's) files, as `items` yields them.
fn post_events(http: &Http, ctx: &Context, post: Value) -> Vec<ExtractionEvent> {
    let Some(mut post) = post.as_object().cloned() else {
        return Vec::new();
    };
    // Upstream warns and skips posts without access.
    if ctx.posts && !truthy(post.get("hasAccess")) {
        return Vec::new();
    }
    let files = extract_files(&mut post, &ctx.videos);
    if let Some(user) = &ctx.user {
        post.insert("user".into(), user.clone());
    }
    let user = post.remove("user").unwrap_or(Value::Null);
    let post_id = post.get("id").map(scalar).unwrap_or_default();
    let mut data = Map::new();
    data.insert("post".into(), Value::Object(post));
    data.insert("user".into(), user);
    data.insert("count".into(), files.len().into());
    let headers = if ctx.posts {
        api_headers(http)
            .into_iter()
            .filter(|(k, _)| *k != "Accept")
            .collect()
    } else {
        Vec::new()
    };
    let items = files
        .into_iter()
        .enumerate()
        .filter_map(|(index, mut file)| {
            let raw = file.get("url").map(scalar).unwrap_or_default();
            let url = Url::parse(&raw).ok()?;
            let file_id = file.get("id").map(scalar).unwrap_or_default();
            let mut keywords = data.clone();
            if let Some(fallback) = file.remove("_fallback") {
                keywords.insert("_fallback".into(), fallback);
            }
            keywords.insert("num".into(), (index + 1).into());
            keywords.insert("file".into(), Value::Object(file));
            nameext_into(&raw, &mut keywords);
            let mut item = Item::new(url, keywords).ids(post_id.as_str(), file_id);
            for (name, value) in &headers {
                item = item.header(name, value);
            }
            Some(item)
        })
        .collect();
    crate::sites::post(&SITE, http, ctx.route, items)
}

/// How a page's posts are found in its `data`.
#[derive(Clone, Copy)]
enum Shape {
    List,
    /// `_transform_media_posts`.
    Media,
    Key(&'static str),
}

/// `BoostyAPI._pagination`.
fn pagination<'a>(
    http: &'a Http,
    ctx: Context,
    endpoint: String,
    mut params: Params,
    shape: Shape,
) -> Events<'a> {
    if !has(&params, "is_only_allowed") && http.option_bool("allowed", true) {
        set(&mut params, "only_allowed", "true");
        set(&mut params, "is_only_allowed", "true");
    }
    let ctx = Arc::new(ctx);
    crate::sites::paginate(Some(params), move |state| {
        let endpoint = endpoint.clone();
        let ctx = ctx.clone();
        async move {
            let Some(mut params) = state else {
                return Ok(None);
            };
            let mut data = call(http, &endpoint, &params).await?;
            let body = data["data"].take();
            let posts: Vec<Value> = match shape {
                Shape::List => body.as_array().cloned().unwrap_or_default(),
                Shape::Key(key) => body[key].as_array().cloned().unwrap_or_default(),
                Shape::Media => body["mediaPosts"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|obj| {
                        let mut post = obj["post"].clone();
                        if let Some(map) = post.as_object_mut() {
                            map.insert("data".into(), obj["media"].clone());
                        }
                        post
                    })
                    .collect(),
            };
            let events: Vec<ExtractionEvent> = posts
                .into_iter()
                .flat_map(|post| post_events(http, &ctx, post))
                .collect();
            let extra = &data["extra"];
            let next = if truthy(extra.get("isLast")) || !truthy(extra.get("offset")) {
                None
            } else {
                set(&mut params, "offset", &scalar(&extra["offset"]));
                Some(params)
            };
            Ok(Some((crate::sites::events(events), next)))
        }
    })
}

/// `BoostyFollowingExtractor`: queue each subscribed blog.
fn following(http: &Http) -> Events<'_> {
    let params = merge_params("", &[("limit", "30"), ("with_follow", "true")]);
    crate::sites::paginate(Some(params), move |state| async move {
        let Some(mut params) = state else {
            return Ok(None);
        };
        let data = call(http, "/v1/user/subscriptions", &params).await?;
        let children = data["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|user| {
                let url = format!("{ROOT}/{}", scalar(&user["blog"]["blogUrl"]));
                Some((Url::parse(&url).ok()?, user.as_object()?.clone()))
            })
            .collect();
        let offset = int_of(data.get("offset")) + int_of(data.get("limit"));
        let next = (offset <= int_of(data.get("total"))).then(|| {
            set(&mut params, "offset", &offset.to_string());
            params
        });
        Ok(Some((
            crate::sites::galleries::queue(http, &SITE, &FOLLOWING, children),
            next,
        )))
    })
}

/// `BoostyDirectMessagesExtractor`: the first dialog page, then older
/// messages through `_pagination_dialog`.
fn messages(http: &Http, dialog: String) -> Events<'_> {
    crate::sites::once(async move {
        let response = call(http, &format!("/v1/dialog/{dialog}"), &Vec::new()).await?;
        let signed = response.get("signedQuery").cloned().unwrap_or(Value::Null);
        let Some(mut first) = response["messages"]["data"].as_array().cloned() else {
            return Ok(crate::sites::events(Vec::new()));
        };
        let Some(offset) = first.first().map(|m| scalar(&m["id"])) else {
            return Ok(crate::sites::events(Vec::new()));
        };
        let chatmate = scalar(&response["chatmate"]["url"]);
        let owner = user(http, &chatmate).await.unwrap_or(Value::Null);
        let ctx = Arc::new(Context {
            route: &MESSAGES,
            user: Some(owner),
            videos: videos(http),
            posts: false,
        });
        let sign = move |mut message: Value| {
            if let Some(map) = message.as_object_mut() {
                map.insert("signedQuery".into(), signed.clone());
            }
            message
        };
        first.reverse();
        let head: Vec<ExtractionEvent> = first
            .into_iter()
            .map(&sign)
            .flat_map(|message| post_events(http, &ctx, message))
            .collect();
        let endpoint = format!("/v1/dialog/{dialog}/message/");
        let params: Params = vec![
            ("limit".into(), "300".into()),
            ("reverse".into(), "true".into()),
            ("offset".into(), offset),
        ];
        let rest = crate::sites::paginate(Some(params), move |state| {
            let (endpoint, ctx, sign) = (endpoint.clone(), ctx.clone(), sign.clone());
            async move {
                let Some(mut params) = state else {
                    return Ok(None);
                };
                let data = call(http, &endpoint, &params).await?;
                let events: Vec<ExtractionEvent> = data["data"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .map(&sign)
                    .flat_map(|message| post_events(http, &ctx, message))
                    .collect();
                let extra = &data["extra"];
                let next =
                    (!truthy(extra.get("isLast")) && truthy(extra.get("offset"))).then(|| {
                        set(&mut params, "offset", &scalar(&extra["offset"]));
                        params
                    });
                Ok(Some((crate::sites::events(events), next)))
            }
        });
        use futures_util::StreamExt;
        Ok(crate::sites::events(head).chain(rest).boxed())
    })
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Boosty URL"));
    };
    let metadata = http.option_bool("metadata", false);
    let context = move |route, user| Context {
        route,
        user,
        videos: videos(http),
        posts: true,
    };
    match target {
        Target::User(name, query) => crate::sites::once(async move {
            let owner = if metadata {
                Some(user(http, &name).await?)
            } else {
                None
            };
            let params = merge_params(
                &query,
                &[
                    ("limit", "5"),
                    ("comments_limit", "2"),
                    ("reply_limit", "1"),
                ],
            );
            Ok(pagination(
                http,
                context(&USER, owner),
                format!("/v1/blog/{name}/post/"),
                params,
                Shape::List,
            ))
        }),
        Target::Media(name, kind, query) => crate::sites::once(async move {
            let owner = user(http, &name).await?;
            let kind = kind.trim_end_matches('s');
            let params = merge_params(
                &query,
                &[("type", kind), ("limit", "15"), ("limit_by", "media")],
            );
            Ok(pagination(
                http,
                context(&MEDIA, Some(owner)),
                format!("/v1/blog/{name}/media_album/"),
                params,
                Shape::Media,
            ))
        }),
        Target::Feed(query) => {
            let mut params = merge_params(&query, &[("limit", "5"), ("comments_limit", "2")]);
            if !has(&params, "only_allowed") && http.option_bool("allowed", true) {
                set(&mut params, "only_allowed", "true");
            }
            if !has(&params, "only_bought") && http.option_bool("bought", false) {
                set(&mut params, "only_bought", "true");
            }
            pagination(
                http,
                context(&FEED, None),
                "/v1/feed/post/".into(),
                params,
                Shape::Key("posts"),
            )
        }
        Target::Post(name, id) => crate::sites::once(async move {
            let owner = if metadata {
                Some(user(http, &name).await?)
            } else {
                None
            };
            let post = call(http, &format!("/v1/blog/{name}/post/{id}"), &Vec::new()).await?;
            Ok(crate::sites::events(post_events(
                http,
                &context(&POST, owner),
                post,
            )))
        }),
        Target::Following => following(http),
        Target::Messages(dialog) => messages(http, dialog),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{children, http as client, json as body, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://boosty.to/milshoo"),
            Some(Target::User("milshoo".into(), String::new()))
        );
        assert_eq!(
            t("https://boosty.to/milshoo?postsTagsIds=1"),
            Some(Target::User("milshoo".into(), "postsTagsIds=1".into()))
        );
        assert_eq!(
            t("https://boosty.to/milshoo/media/videos"),
            Some(Target::Media(
                "milshoo".into(),
                "videos".into(),
                String::new()
            ))
        );
        assert_eq!(t("https://boosty.to/"), Some(Target::Feed(String::new())));
        assert_eq!(
            t("https://boosty.to/milshoo/posts/0c652798-3b0b-4bd6-8a74-56d6a7e47ca5"),
            Some(Target::Post(
                "milshoo".into(),
                "0c652798-3b0b-4bd6-8a74-56d6a7e47ca5".into()
            ))
        );
        assert_eq!(
            t("https://boosty.to/app/settings/subscriptions"),
            Some(Target::Following)
        );
        assert_eq!(
            t("https://boosty.to/app/messages?dialogId=12345"),
            Some(Target::Messages("12345".into()))
        );
    }

    fn sample_post() -> Value {
        json!({"id": "0c652798", "int_id": 5547, "title": "Post", "hasAccess": true,
        "createdAt": 1_700_000_000, "signedQuery": "?sig=abc", "isMigrated": false,
        "user": {"blogUrl": "milshoo", "id": 1},
        "data": [
            {"type": "text", "modificator": "", "content": "[\"Hello\",\"unstyled\",[]]"},
            {"type": "text", "modificator": "BLOCK_END", "content": ""},
            {"type": "image", "id": "img1", "url": "https://images.boosty.to/image/img1?change_time=1"},
            {"type": "link", "url": "https://example.org/"},
            {"type": "smile", "name": "cat"},
            {"type": "ok_video", "id": "vid1", "playerUrls": [
                {"type": "low", "url": "https://vd.example/low.mp4"},
                {"type": "full_hd", "url": "https://vd.example/hd.mp4"},
                {"type": "ultra_hd", "url": ""}]}
        ]})
    }

    #[tokio::test]
    async fn post_files_videos_and_text() {
        let (root, _server) = serve(vec![body(sample_post())]);
        let http = client("boosty", root);
        let files = media(extract(
            &http,
            Url::parse("https://boosty.to/milshoo/posts/0c652798").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].url.as_str(),
            "https://images.boosty.to/image/img1?change_time=1&sig=abc&is_migrated=false"
        );
        assert_eq!(files[1].url.as_str(), "https://vd.example/hd.mp4");
        assert_eq!(
            files[1].handoff["_fallback"],
            json!(["https://vd.example/low.mp4"])
        );
        let k = keywords(&files[0]);
        assert_eq!(
            k["post"]["content"],
            json!(["Hello", "https://example.org/", ":cat:"])
        );
        assert_eq!(k["user"]["blogUrl"], "milshoo");
        assert_eq!(k["count"], 2);
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "01 img1.");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            crate::sites::testing::os_dir("boosty/milshoo (1)/2023-11-14 5547")
        );
    }

    #[tokio::test]
    async fn user_pagination_skips_inaccessible_posts() {
        let mut locked = sample_post();
        locked["hasAccess"] = json!(false);
        let (root, server) = serve(vec![
            body(json!({"data": [locked], "extra": {"isLast": false, "offset": "abc"}})),
            body(json!({"data": [sample_post()], "extra": {"isLast": true}})),
        ]);
        let http = client("boosty", root);
        let files = media(extract(
            &http,
            Url::parse("https://boosty.to/milshoo").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let requests = server.join().unwrap();
        assert!(requests[0].contains("is_only_allowed=true"));
        assert!(requests[1].contains("offset=abc"));
    }

    #[tokio::test]
    async fn following_queues_blogs() {
        let (root, _server) = serve(vec![body(json!({
            "data": [{"blog": {"blogUrl": "a"}}, {"blog": {"blogUrl": "b"}}],
            "offset": 0, "limit": 30, "total": 2}))]);
        let http = client("boosty", root);
        let queued = children(extract(
            &http,
            Url::parse("https://boosty.to/app/settings/subscriptions").unwrap(),
        ))
        .await;
        assert_eq!(queued.len(), 2);
    }
}
