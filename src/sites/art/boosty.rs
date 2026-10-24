//! Boosty blogs, media albums, feeds, posts, subscriptions and direct
//! messages, mirroring gallery_dl/extractor/boosty.py through the
//! `api.boosty.to` JSON API.
//!
//! Paid posts need the `auth` cookie from `--cookies`; its access token is
//! sent as a bearer token to the API and with each post's files.
//! Upstream's alternative video formats (`_fallback`) are not emitted, and
//! `_http_headers` lists only the `Origin` header, never the token.
use super::{leading_digits, on_host, parse_query, segment, str_of, truthy};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use futures_util::{StreamExt, stream};
use serde_json::{Map, Value, json};
use std::sync::Arc;
use url::Url;

const ROOT: &str = "https://www.boosty.to";
const API: &str = "https://api.boosty.to";

pub static SITE: Site = Site {
    category: "boosty",
    name: "Boosty",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&USER, &MEDIA, &FEED, &POST, &FOLLOWING, &DIRECT_MESSAGES],
    api_key: false,
    cookies: Some("boosty.to"),
};

const FILENAME: &str = "{num:>02} {file[id]}.{extension}";
const USER_DIRECTORY: &str = "{user[blogUrl]} ({user[id]})";
const DIRECTORY: &[&str] = &[
    "{category}",
    USER_DIRECTORY,
    "{post[date]:%Y-%m-%d} {post[int_id]}",
];
const ARCHIVE: &str = "{file[id]}";

static USER: Route = Route::new("user", FILENAME, DIRECTORY, ARCHIVE);
static MEDIA: Route = Route::new(
    "media",
    "{post[id]}_{num}.{extension}",
    &["{category}", USER_DIRECTORY, "media"],
    ARCHIVE,
);
static FEED: Route = Route::new("feed", FILENAME, DIRECTORY, ARCHIVE);
static POST: Route = Route::new("post", FILENAME, DIRECTORY, ARCHIVE);
static FOLLOWING: Route = Route::new("following", FILENAME, DIRECTORY, ARCHIVE);
static DIRECT_MESSAGES: Route = Route::new(
    "direct-messages",
    FILENAME,
    &["{category}", USER_DIRECTORY, "Direct Messages"],
    ARCHIVE,
);

/// Video formats from best to worst, upstream's `videos` default.
const VIDEOS: &[&str] = &[
    "ultra_hd", "quad_hd", "full_hd", "high", "medium", "low", "lowest", "tiny",
];

/// API query parameters in order; `None` values are not sent, as with
/// Python requests.
type Params = Vec<(String, Option<String>)>;

#[derive(Debug, PartialEq)]
enum Target {
    /// Blog name and web query.
    User(String, Vec<(String, String)>),
    /// Blog name, media type and web query.
    Media(String, String, Vec<(String, String)>),
    Feed(Vec<(String, String)>),
    Post(String, String),
    Following,
    DirectMessages(String),
}

fn target(url: &Url) -> Option<Target> {
    if !on_host(url, &["boosty.to"]) {
        return None;
    }
    let path = url.path();
    // `(?:\?([^#]+))?`; a bare `?` matches none of the patterns.
    let query = match url.query() {
        Some("") => return None,
        query => query,
    };
    let web = || query.map(parse_query).unwrap_or_default();
    let parts: Vec<&str> = path[1..].split('/').collect();
    match parts.as_slice() {
        [user] if segment(user) && url.fragment().is_none() => {
            return Some(Target::User((*user).to_owned(), web()));
        }
        [""] => return Some(Target::Feed(web())),
        _ => {}
    }
    match parts.as_slice() {
        [user, "media", kind, rest @ ..] if segment(user) && segment(kind) => {
            let web = if rest.is_empty() { web() } else { Vec::new() };
            return Some(Target::Media((*user).into(), (*kind).into(), web));
        }
        [user, "posts", id, ..] if segment(user) => {
            let len = id
                .bytes()
                .take_while(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase() || *b == b'-')
                .count();
            return (len > 0).then(|| Target::Post((*user).into(), id[..len].into()));
        }
        _ => {}
    }
    if path.starts_with("/app/settings/subscriptions") {
        return Some(Target::Following);
    }
    if matches!(path, "/app/messages" | "/app/messages/") {
        let id = query?.strip_prefix("dialogId=").and_then(leading_digits)?;
        return Some(Target::DirectMessages(id.to_owned()));
    }
    None
}

/// Per-extraction API access and options.
struct Ctx<'a> {
    http: &'a Http,
    bearer: Option<String>,
    /// `None` when videos are disabled.
    videos: Option<Vec<String>>,
    only_allowed: bool,
    only_bought: bool,
}

impl Ctx<'_> {
    /// Upstream `BoostyAPI._call`.
    async fn call(&self, endpoint: &str, params: &Params) -> Result<Value> {
        let mut url = self.http.api(&format!("{API}{endpoint}"))?;
        let pairs: Vec<(&str, &str)> = params
            .iter()
            .filter_map(|(k, v)| Some((k.as_str(), v.as_deref()?)))
            .collect();
        if !pairs.is_empty() {
            url.query_pairs_mut().extend_pairs(pairs);
        }
        let mut headers = vec![
            ("Accept", "application/json, text/plain, */*"),
            ("Origin", ROOT),
        ];
        if let Some(bearer) = &self.bearer {
            headers.push(("Authorization", bearer.as_str()));
        }
        self.http.json_with(&url, &headers).await
    }

    /// Upstream `BoostyAPI.user`: the blog with `id` set to its owner's.
    async fn user(&self, name: &str) -> Result<Value> {
        let mut user = self.call(&format!("/v1/blog/{name}"), &Vec::new()).await?;
        let id = user
            .get("owner")
            .and_then(|o| o.get("id"))
            .cloned()
            .unwrap_or(Value::Null);
        if let Some(user) = user.as_object_mut() {
            user.insert("id".into(), id);
        }
        Ok(user)
    }
}

/// The `accessToken` of the `auth` cookie, which holds URL-encoded JSON.
fn bearer(http: &Http) -> Option<String> {
    let root = http.api("https://boosty.to/").ok()?;
    let auth = crate::sites::percent_decode(&http.cookie(&root, "auth")?);
    let token = super::extr(&auth, "\"accessToken\":\"", "\"");
    (!token.is_empty()).then(|| format!("Bearer {token}"))
}

/// Upstream's `videos` option: format names in preference order.
fn videos(http: &Http) -> Option<Vec<String>> {
    let all = || VIDEOS.iter().map(|s| (*s).to_owned()).collect();
    match http.option("videos") {
        None | Some(Value::Null) => Some(all()),
        Some(Value::String(s)) if !s.is_empty() => Some(s.split(',').map(str::to_owned).collect()),
        Some(Value::Array(items)) if !items.is_empty() => Some(
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
        ),
        Some(value) if truthy(Some(value)) => Some(all()),
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Boosty URL"));
    };
    let ctx = Arc::new(Ctx {
        http,
        bearer: bearer(http),
        videos: videos(http),
        only_allowed: http.option_bool("allowed", true),
        only_bought: http.option_bool("bought", false),
    });
    let metadata = http.option_bool("metadata", false);
    match target {
        Target::User(user, web) => crate::sites::once(async move {
            let owner = if metadata {
                Some(ctx.user(&user).await?)
            } else {
                None
            };
            let params = merge(
                &web,
                &[
                    ("limit", Some("5")),
                    ("offset", None),
                    ("comments_limit", Some("2")),
                    ("reply_limit", Some("1")),
                ],
            );
            let endpoint = format!("/v1/blog/{user}/post/");
            Ok(pagination(ctx, &USER, endpoint, params, Kind::Posts, owner))
        }),
        Target::Media(user, kind, web) => crate::sites::once(async move {
            let owner = ctx.user(&user).await?;
            let kind = kind.trim_end_matches('s');
            let params = merge(
                &web,
                &[
                    ("type", Some(kind)),
                    ("limit", Some("15")),
                    ("limit_by", Some("media")),
                    ("offset", None),
                ],
            );
            let endpoint = format!("/v1/blog/{user}/media_album/");
            Ok(pagination(
                ctx,
                &MEDIA,
                endpoint,
                params,
                Kind::Media,
                Some(owner),
            ))
        }),
        Target::Feed(web) => {
            let mut params = merge(
                &web,
                &[
                    ("limit", Some("5")),
                    ("offset", None),
                    ("comments_limit", Some("2")),
                ],
            );
            if !has(&params, "only_allowed") && ctx.only_allowed {
                set(&mut params, "only_allowed", "true");
            }
            if !has(&params, "only_bought") && ctx.only_bought {
                set(&mut params, "only_bought", "true");
            }
            let endpoint = "/v1/feed/post/".to_owned();
            pagination(ctx, &FEED, endpoint, params, Kind::Feed, None)
        }
        Target::Post(user, id) => crate::sites::once(async move {
            let owner = if metadata {
                Some(ctx.user(&user).await?)
            } else {
                None
            };
            let post = ctx
                .call(&format!("/v1/blog/{user}/post/{id}"), &Vec::new())
                .await?;
            let items = post_items(&ctx, post, owner.as_ref());
            Ok(crate::sites::events(crate::sites::post(
                &SITE, http, &POST, items,
            )))
        }),
        Target::Following => following(ctx),
        Target::DirectMessages(id) => direct_messages(ctx, id),
    }
}

fn has(params: &Params, key: &str) -> bool {
    params.iter().any(|(k, _)| k == key)
}

/// Python `params[key] = value` on ordered parameters.
fn set(params: &mut Params, key: &str, value: &str) {
    match params.iter_mut().find(|(k, _)| k == key) {
        Some((_, v)) => *v = Some(value.to_owned()),
        None => params.push((key.to_owned(), Some(value.to_owned()))),
    }
}

/// Upstream `_merge_params`: API defaults overridden by the web query,
/// with web names mapped to their API counterparts.
fn merge(web: &[(String, String)], api: &[(&str, Option<&str>)]) -> Params {
    let mut params: Params = api
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.map(str::to_owned)))
        .collect();
    for (name, value) in web {
        let name = match name.as_str() {
            "isOnlyAllowedPosts" => "is_only_allowed",
            "postsTagsIds" => "tags_ids",
            "postsFrom" => "from_ts",
            "postsTo" => "to_ts",
            name => name,
        };
        set(&mut params, name, value);
    }
    params
}

/// Where a page's posts are.
#[derive(Clone, Copy)]
enum Kind {
    Posts,
    Media,
    Feed,
}

/// Upstream `BoostyAPI._pagination`: follow `extra.offset` until `isLast`.
fn pagination<'a>(
    ctx: Arc<Ctx<'a>>,
    route: &'static Route,
    endpoint: String,
    mut params: Params,
    kind: Kind,
    owner: Option<Value>,
) -> Events<'a> {
    if !has(&params, "is_only_allowed") && ctx.only_allowed {
        set(&mut params, "only_allowed", "true");
        set(&mut params, "is_only_allowed", "true");
    }
    crate::sites::paginate(Some(params), move |params| {
        let (ctx, endpoint, owner) = (ctx.clone(), endpoint.clone(), owner.clone());
        async move {
            let Some(mut params) = params else {
                return Ok(None);
            };
            let data = ctx.call(&endpoint, &params).await?;
            let list = |value: Option<&Value>| value.and_then(Value::as_array).cloned();
            let posts = match kind {
                Kind::Posts => list(data.get("data")),
                Kind::Feed => list(data.get("data").and_then(|d| d.get("posts"))),
                Kind::Media => {
                    list(data.get("data").and_then(|d| d.get("mediaPosts"))).map(|album| {
                        album
                            .into_iter()
                            .map(|entry| {
                                let mut post = entry.get("post").cloned().unwrap_or(Value::Null);
                                if let Some(post) = post.as_object_mut() {
                                    let media = entry.get("media").cloned().unwrap_or(json!([]));
                                    post.insert("data".into(), media);
                                }
                                post
                            })
                            .collect()
                    })
                }
            }
            .ok_or_else(|| error("Boosty returned no posts"))?;
            let extra = data.get("extra").cloned().unwrap_or(Value::Null);
            let offset = extra.get("offset").filter(|o| truthy(Some(o)));
            let next = match offset {
                Some(offset) if !truthy(extra.get("isLast")) => {
                    let offset = crate::sites::scalar(offset);
                    set(&mut params, "offset", &offset);
                    Some(params)
                }
                _ => None,
            };
            let http = ctx.http;
            let items: Vec<Vec<Item>> = posts
                .into_iter()
                .map(|post| post_items(&ctx, post, owner.as_ref()))
                .collect();
            let events = stream::iter(items)
                .map(move |items| {
                    crate::sites::events(crate::sites::post(&SITE, http, route, items))
                })
                .flatten()
                .boxed();
            Ok(Some((events, next)))
        }
    })
}

/// Upstream `BoostyFollowingExtractor`: queue each subscribed blog.
fn following(ctx: Arc<Ctx<'_>>) -> Events<'_> {
    let params: Params = vec![
        ("limit".into(), Some("30".into())),
        ("with_follow".into(), Some("true".into())),
        ("offset".into(), None),
    ];
    crate::sites::paginate(Some(params), move |params| {
        let ctx = ctx.clone();
        async move {
            let Some(mut params) = params else {
                return Ok(None);
            };
            let data = ctx.call("/v1/user/subscriptions", &params).await?;
            let int = |key: &str| data.get(key).and_then(Value::as_i64).unwrap_or(0);
            let offset = int("offset") + int("limit");
            let next = (offset <= int("total")).then(|| {
                set(&mut params, "offset", &offset.to_string());
                params
            });
            let children: Vec<(Url, Map<String, Value>)> = data
                .get("data")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|user| {
                    let blog = str_of(user.get("blog")?, "blogUrl");
                    let url = Url::parse(&format!("{ROOT}/{blog}")).ok()?;
                    segment(blog).then(|| (url, user.as_object().cloned().unwrap_or_default()))
                })
                .collect();
            let http = ctx.http;
            let events = stream::iter(children)
                .map(move |(url, user)| {
                    crate::sites::queue(http, "boosty", "following", url, &user)
                })
                .flatten()
                .boxed();
            Ok(Some((events, next)))
        }
    })
}

/// Upstream `BoostyDirectMessagesExtractor`: the dialog's first messages
/// oldest first, then older pages from the first message's ID.
fn direct_messages(ctx: Arc<Ctx<'_>>, id: String) -> Events<'_> {
    crate::sites::once(async move {
        let dialog = ctx.call(&format!("/v1/dialog/{id}"), &Vec::new()).await?;
        let signed = dialog.get("signedQuery").cloned().unwrap_or(Value::Null);
        let mut messages = dialog
            .get("messages")
            .and_then(|m| m.get("data"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let Some(offset) = messages.first().and_then(|m| m.get("id")).cloned() else {
            return Ok(crate::sites::events(Vec::new()));
        };
        let chatmate = str_of(dialog.get("chatmate").unwrap_or(&Value::Null), "url");
        // Upstream ignores any failure to look up the chat partner.
        let user = if segment(chatmate) {
            ctx.user(chatmate).await.unwrap_or(Value::Null)
        } else {
            Value::Null
        };
        messages.reverse();
        let first = message_events(&ctx, messages, &signed, &user);
        let params: Params = vec![
            ("limit".into(), Some("300".into())),
            ("reverse".into(), Some("true".into())),
            ("offset".into(), Some(crate::sites::scalar(&offset))),
        ];
        let rest = crate::sites::paginate(Some(params), move |params| {
            let (ctx, id, signed, user) = (ctx.clone(), id.clone(), signed.clone(), user.clone());
            async move {
                let Some(mut params) = params else {
                    return Ok(None);
                };
                let data = ctx
                    .call(&format!("/v1/dialog/{id}/message/"), &params)
                    .await?;
                let messages = data
                    .get("data")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let extra = data.get("extra");
                let offset = extra
                    .and_then(|e| e.get("offset"))
                    .filter(|o| truthy(Some(o)));
                let next = match offset {
                    Some(offset) if !truthy(extra.and_then(|e| e.get("isLast"))) => {
                        set(&mut params, "offset", &crate::sites::scalar(offset));
                        Some(params)
                    }
                    _ => None,
                };
                Ok(Some((message_events(&ctx, messages, &signed, &user), next)))
            }
        });
        Ok(first.chain(rest).boxed())
    })
}

fn message_events<'a>(
    ctx: &Ctx<'a>,
    messages: Vec<Value>,
    signed: &Value,
    user: &Value,
) -> Events<'a> {
    let http = ctx.http;
    let posts: Vec<Vec<Item>> = messages
        .into_iter()
        .filter_map(|message| {
            let Value::Object(mut message) = message else {
                return None;
            };
            message.insert("signedQuery".into(), signed.clone());
            let files = extract_files(&mut message, ctx.videos.as_deref());
            let mut data = Map::new();
            data.insert("count".into(), files.len().into());
            Some(items(message, user.clone(), data, files, None))
        })
        .collect();
    stream::iter(posts)
        .map(move |items| {
            crate::sites::events(crate::sites::post(&SITE, http, &DIRECT_MESSAGES, items))
        })
        .flatten()
        .boxed()
}

/// Upstream `BoostyExtractor.items` for one post; posts without access
/// are skipped.
fn post_items(ctx: &Ctx<'_>, post: Value, owner: Option<&Value>) -> Vec<Item> {
    let Value::Object(mut post) = post else {
        return Vec::new();
    };
    if !truthy(post.get("hasAccess")) {
        return Vec::new();
    }
    let files = extract_files(&mut post, ctx.videos.as_deref());
    if let Some(owner) = owner.filter(|o| truthy(Some(o))) {
        post.insert("user".into(), owner.clone());
    }
    let user = post.remove("user").unwrap_or(Value::Null);
    let mut data = Map::new();
    data.insert("count".into(), files.len().into());
    data.insert("_http_headers".into(), json!({"Origin": ROOT}));
    items(post, user, data, files, ctx.bearer.as_deref())
}

/// One item per file, with `post`, `user`, `num` and `file` keywords.
fn items(
    post: Map<String, Value>,
    user: Value,
    extra: Map<String, Value>,
    files: Vec<Map<String, Value>>,
    bearer: Option<&str>,
) -> Vec<Item> {
    let timestamp = post.get("createdAt").and_then(super::parse_ts);
    let post_id = post.get("id").map(crate::sites::scalar).unwrap_or_default();
    let title = post.get("title").and_then(Value::as_str).map(str::to_owned);
    let blog = user
        .get("blogUrl")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let headers = extra.contains_key("_http_headers");
    let mut data = Map::new();
    data.insert("post".into(), post.into());
    data.insert("user".into(), user);
    super::update(&mut data, &extra);
    let mut items = Vec::new();
    for (index, file) in files.into_iter().enumerate() {
        let raw = file
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let media_id = file.get("id").map(crate::sites::scalar).unwrap_or_default();
        data.insert("num".into(), (index + 1).into());
        data.insert("file".into(), file.into());
        super::nameext_into(&raw, &mut data);
        let Some(url) = Url::parse(&raw).ok().filter(crate::sites::plain) else {
            continue;
        };
        let mut item = Item::new(url, data.clone())
            .ids(post_id.clone(), media_id)
            .user(blog.clone())
            .caption(title.clone())
            .timestamp(timestamp);
        if headers {
            item = item.header("Origin", ROOT);
            if let Some(bearer) = bearer {
                item = item.header("Authorization", bearer);
            }
        }
        items.push(item);
    }
    items
}

/// Upstream `_extract_files`: files of the post's blocks, with text, links
/// and smileys collected into `content` and `links`.
fn extract_files(
    post: &mut Map<String, Value>,
    videos: Option<&[String]>,
) -> Vec<Map<String, Value>> {
    let mut content = Vec::new();
    let mut links = Vec::new();
    post.insert("content".into(), json!([]));
    post.insert("links".into(), json!([]));
    if let Some(created) = post.get("createdAt") {
        let date = crate::filter::source_date_json(super::parse_ts(created));
        post.insert("date".into(), date);
    }
    let signed = post
        .get("signedQuery")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let migrated = post.get("isMigrated").and_then(Value::as_bool);
    let blocks = match post.remove("data") {
        Some(Value::Array(blocks)) => blocks,
        _ => Vec::new(),
    };
    let mut files = Vec::new();
    for block in blocks {
        let Value::Object(mut block) = block else {
            continue;
        };
        let kind = block
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match kind.to_owned().as_str() {
            "text" => {
                let Some(modificator) = block.get("modificator").and_then(Value::as_str) else {
                    continue;
                };
                if modificator == "BLOCK_END" {
                    continue;
                }
                let text = block
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if let Ok(Value::Array(parts)) = serde_json::from_str::<Value>(text)
                    && let Some(first) = parts.into_iter().next()
                {
                    content.push(first);
                }
            }
            "image" | "audio_file" | "file" => {
                let Some(url) = block.get("url").and_then(Value::as_str) else {
                    continue;
                };
                let url = signed_url(url, &signed, migrated);
                block.insert("url".into(), url.into());
                files.push(block);
            }
            "ok_video" => {
                let Some(videos) = videos else {
                    continue;
                };
                let mut formats = Map::new();
                for format in block
                    .get("playerUrls")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let url = format.get("url").filter(|u| truthy(Some(u)));
                    if let (Some(kind), Some(url)) =
                        (format.get("type").and_then(Value::as_str), url)
                    {
                        formats.insert(kind.to_owned(), url.clone());
                    }
                }
                if let Some(url) = videos.iter().find_map(|name| formats.get(name)) {
                    block.insert("url".into(), url.clone());
                    files.push(block);
                }
            }
            "link" => {
                if let Some(url) = block.get("url") {
                    links.push(url.clone());
                    content.push(url.clone());
                }
            }
            "smile" => {
                if let Some(name) = block.get("name").and_then(Value::as_str) {
                    content.push(format!(":{name}:").into());
                }
            }
            _ => {}
        }
    }
    post.insert("content".into(), content.into());
    post.insert("links".into(), links.into());
    files
}

/// Upstream `_update_url`: append the post's signed query and migration
/// flag.
fn signed_url(url: &str, signed: &str, migrated: Option<bool>) -> String {
    let mut url = url.to_owned();
    let mut sep = if url.contains('?') { '&' } else { '?' };
    if !signed.is_empty() {
        url.push(sep);
        url.push_str(signed.get(1..).unwrap_or_default());
        sep = '&';
    }
    if let Some(migrated) = migrated {
        url.push(sep);
        url.push_str(if migrated {
            "is_migrated=true"
        } else {
            "is_migrated=false"
        });
    }
    url
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http, json, keywords, media, queued, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        let q = |k: &str, v: &str| vec![(k.to_owned(), v.to_owned())];
        assert_eq!(
            t("https://boosty.to/milshoo"),
            Some(Target::User("milshoo".into(), vec![]))
        );
        assert_eq!(
            t("https://www.boosty.to/milshoo?postsTagsIds=1"),
            Some(Target::User("milshoo".into(), q("postsTagsIds", "1")))
        );
        assert_eq!(
            t("https://boosty.to/milshoo/media/images?offset=x"),
            Some(Target::Media(
                "milshoo".into(),
                "images".into(),
                q("offset", "x")
            ))
        );
        assert_eq!(t("https://boosty.to/"), Some(Target::Feed(vec![])));
        assert_eq!(
            t("https://boosty.to/?only_bought=true#x"),
            Some(Target::Feed(q("only_bought", "true")))
        );
        assert_eq!(
            t("https://boosty.to/milshoo/posts/0c652798-3a9d-4d3d-ba5e-5a5ca7a1a0bc?x=1"),
            Some(Target::Post(
                "milshoo".into(),
                "0c652798-3a9d-4d3d-ba5e-5a5ca7a1a0bc".into()
            ))
        );
        assert_eq!(
            t("https://boosty.to/app/settings/subscriptions"),
            Some(Target::Following)
        );
        assert_eq!(
            t("https://boosty.to/app/messages?dialogId=12345"),
            Some(Target::DirectMessages("12345".into()))
        );
        for raw in [
            "https://boosty.to/milshoo#x",
            "https://boosty.to/milshoo?",
            "https://boosty.to/milshoo/other",
            "https://boosty.to/milshoo/posts/XYZ",
            "https://boosty.to/app/messages?x=1",
            "https://api.boosty.to/milshoo",
            "https://boosty.to.evil.com/milshoo",
            "https://evilboosty.to/milshoo",
            "https://boosty.to:8443/milshoo",
            "https://u:p@boosty.to/milshoo",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn post(id: &str, access: bool) -> Value {
        json!({"id": id, "int_id": 123, "title": "Post", "createdAt": 1577934245,
            "hasAccess": access, "signedQuery": "?expire=1&sign=x", "isMigrated": false,
            "user": {"blogUrl": "blog", "id": 7, "name": "Blogger"},
            "data": [
                {"type": "text", "modificator": "", "content": "[\"Hello\", \"unstyled\", []]"},
                {"type": "text", "modificator": "BLOCK_END", "content": ""},
                {"type": "image", "id": "img-1", "url": "https://images.boosty.to/image/img-1?change_time=1"},
                {"type": "link", "url": "https://example.com/x"},
                {"type": "smile", "name": "cat"},
                {"type": "ok_video", "id": "vid-1", "playerUrls": [
                    {"type": "low", "url": "https://vd.mycdn.me/low"},
                    {"type": "full_hd", "url": "https://vd.mycdn.me/fhd"},
                    {"type": "ultra_hd", "url": ""}]},
                {"type": "file", "id": "file-1", "url": "https://cdn.boosty.to/file/doc.pdf"},
                {"type": "unknown"}]})
    }

    #[tokio::test]
    async fn extracts_blog_posts() {
        let (root, server) = serve(vec![
            json(json!({"data": [post("p1", true), post("p0", false)],
                "extra": {"isLast": false, "offset": "1577934245:1"}})),
            json(json!({"data": [], "extra": {"isLast": true}})),
        ]);
        let http = http("boosty", root.clone());
        http.set_cookie(
            &root,
            "auth=%7B%22accessToken%22%3A%22tok%22%2C%22expiresAt%22%3A1%7D; Path=/",
        );
        let files = media(extract(
            &http,
            Url::parse("https://boosty.to/blog").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 3);
        let k = keywords(&files[0]);
        assert_eq!(k["user"]["blogUrl"], "blog");
        assert!(!k["post"].as_object().unwrap().contains_key("user"));
        assert!(!k["post"].as_object().unwrap().contains_key("data"));
        assert_eq!(
            k["post"]["content"],
            json!(["Hello", "https://example.com/x", ":cat:"])
        );
        assert_eq!(k["post"]["links"], json!(["https://example.com/x"]));
        assert_eq!(k["count"], 3);
        assert_eq!(k["_http_headers"], json!({"Origin": ROOT}));
        assert_eq!(k["file"]["id"], "img-1");
        assert_eq!(
            files[0].url.as_str(),
            "https://images.boosty.to/image/img-1?change_time=1&expire=1&sign=x&is_migrated=false"
        );
        assert_eq!(
            files[0].headers.get("Authorization").map(String::as_str),
            Some("Bearer tok")
        );
        assert_eq!(files[1].url.as_str(), "https://vd.mycdn.me/fhd");
        assert_eq!(
            files[2].url.as_str(),
            "https://cdn.boosty.to/file/doc.pdf?expire=1&sign=x&is_migrated=false"
        );
        let (file, dir) = crate::sites::default_templates(&files[2]).unwrap();
        assert_eq!(file.render(&files[2]).unwrap(), "03 file-1.pdf");
        assert_eq!(
            dir.render(&files[2]).unwrap().unwrap(),
            std::path::PathBuf::from("boosty/blog (7)/2020-01-02 123")
        );
        let (_, identity) = crate::sites::archive_key(&files[2]).unwrap();
        assert_eq!(identity, r#"["{file[id]}",["file-1"]]"#);
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with(
                "GET /v1/blog/blog/post/?limit=5&comments_limit=2&reply_limit=1&only_allowed=true&is_only_allowed=true "
            ),
            "{}",
            requests[0]
        );
        assert!(requests[0].contains("authorization: Bearer tok"));
        assert!(requests[0].contains("origin: https://www.boosty.to"));
        assert!(requests[1].starts_with(
            "GET /v1/blog/blog/post/?limit=5&offset=1577934245%3A1&comments_limit=2&reply_limit=1&only_allowed=true&is_only_allowed=true "
        ));
    }

    #[tokio::test]
    async fn extracts_media_albums() {
        let user = json!({"blogUrl": "blog", "owner": {"id": 7}, "title": "Blog"});
        let album = json!({"data": {"mediaPosts": [{"post": {"id": "p9", "hasAccess": true},
            "media": [{"type": "image", "id": "m1", "url": "https://images.boosty.to/image/m1.png"}]}]},
            "extra": {"isLast": true, "offset": "x"}});
        let (root, server) = serve(vec![json(user), json(album)]);
        let http = http("boosty", root).with_options(Some(Arc::new(
            serde_json::from_value(json!({"allowed": false, "videos": false})).unwrap(),
        )));
        let files = media(extract(
            &http,
            Url::parse("https://boosty.to/blog/media/images").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let k = keywords(&files[0]);
        assert_eq!(k["user"]["id"], 7);
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "p9_1.png");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("boosty/blog (7)/media")
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /v1/blog/blog "));
        assert!(
            requests[1]
                .starts_with("GET /v1/blog/blog/media_album/?type=image&limit=15&limit_by=media ")
        );
    }

    #[tokio::test]
    async fn extracts_direct_messages_oldest_first() {
        let image = |id: &str| json!({"type": "image", "id": id, "url": format!("https://images.boosty.to/image/{id}")});
        let dialog = json!({"signedQuery": "?s=1", "chatmate": {"url": "blog"},
            "messages": {"data": [{"id": 50, "data": [image("i50")]}, {"id": 49, "data": [image("i49")]}]}});
        let user = json!({"blogUrl": "blog", "owner": {"id": 7}});
        let older =
            json!({"data": [{"id": 48, "data": [image("i48")]}], "extra": {"isLast": true}});
        let (root, server) = serve(vec![json(dialog), json(user), json(older)]);
        let http = http("boosty", root);
        let files = media(extract(
            &http,
            Url::parse("https://boosty.to/app/messages?dialogId=5").unwrap(),
        ))
        .await;
        let ids: Vec<&Value> = files.iter().map(|f| &keywords(f)["file"]["id"]).collect();
        assert_eq!(ids, [&json!("i49"), &json!("i50"), &json!("i48")]);
        assert_eq!(
            files[0].url.as_str(),
            "https://images.boosty.to/image/i49?s=1"
        );
        assert!(!files[0].headers.contains_key("Authorization"));
        let (_, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("boosty/blog (7)/Direct Messages")
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /v1/dialog/5 "));
        assert!(requests[1].starts_with("GET /v1/blog/blog "));
        assert!(
            requests[2].starts_with("GET /v1/dialog/5/message/?limit=300&reverse=true&offset=50 ")
        );
    }

    #[tokio::test]
    async fn following_queues_blogs() {
        let (root, server) = serve(vec![json(json!({
            "data": [{"blog": {"blogUrl": "alice"}}, {"blog": {"blogUrl": "../x"}}],
            "offset": 0, "limit": 30, "total": 2}))]);
        let http = http("boosty", root);
        let children = queued(extract(
            &http,
            Url::parse("https://boosty.to/app/settings/subscriptions").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].subcategory(), "following");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /v1/user/subscriptions?limit=30&with_follow=true "));
    }
}
