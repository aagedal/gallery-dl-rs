//! pixivFANBOX creators, tags, posts and the home and supporting feeds,
//! mirroring gallery_dl/extractor/fanbox.py through the `api.fanbox.cc` JSON
//! API, plus upstream's pixiv `fanbox/creator/ID` redirects.
//!
//! Paid and restricted posts need a `FANBOXSESSID` cookie for `.fanbox.cc`
//! from `--cookies`. It is sent to the API with upstream's Origin, Referer
//! and Sec-Fetch headers, and with files on FANBOX hosts. Embedded videos and
//! links are queued to the site router, or handed to yt-dlp with
//! `embeds: "ytdl"`.
//!
//! The API now wraps some bodies upstream reads directly (`post.info` in
//! `post`, creator pages in `pageUrls` and `posts`, plans in `plans`) and
//! names listing covers in `cover`; both shapes are accepted. The guest
//! `FANBOXSESSID` the API assigns to anonymous requests is never sent back,
//! as the API refuses this client's later requests carrying it, and API
//! calls are spaced half a second apart.
use super::{date, extract_iter, leading_digits, on_host, parse_iso, str_of, truthy};
use crate::sites::{Events, Http, Item, Route, Site, http::refused_status};
use crate::{Result, error};
use futures_util::{StreamExt, stream};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use url::Url;

const ROOT: &str = "https://www.fanbox.cc";
const API: &str = "https://api.fanbox.cc";
const SESSION: &str = "FANBOXSESSID";

pub static SITE: Site = Site {
    category: "fanbox",
    name: "pixivFANBOX",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&CREATOR, &TAG, &POST, &HOME, &SUPPORTING, &REDIRECT],
    api_key: false,
    cookies: Some("fanbox.cc"),
};

const FILENAME: &str = "{id}_{num}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{creatorId}"];
const ARCHIVE: &str = "{id}_{num}";

static CREATOR: Route = Route::new("creator", FILENAME, DIRECTORY, ARCHIVE);
static TAG: Route = Route::new("tag", FILENAME, DIRECTORY, ARCHIVE);
static POST: Route = Route::new("post", FILENAME, DIRECTORY, ARCHIVE);
static HOME: Route = Route::new("home", FILENAME, DIRECTORY, ARCHIVE);
static SUPPORTING: Route = Route::new("supporting", FILENAME, DIRECTORY, ARCHIVE);
/// Upstream's redirect class only queues; it keeps the base formats.
static REDIRECT: Route = Route::new("redirect", "{filename}.{extension}", &["{category}"], "");

/// API request headers besides the browser User-Agent (upstream `_init`).
const HEADERS: &[(&str, &str)] = &[
    ("Accept", "application/json, text/plain, */*"),
    ("Origin", ROOT),
    ("Referer", "https://www.fanbox.cc/"),
    ("Sec-Fetch-Dest", "empty"),
    ("Sec-Fetch-Mode", "cors"),
    ("Sec-Fetch-Site", "same-site"),
];

/// `util.EXTS_ARCHIVE`, for the `archives` keyword.
const ARCHIVES: &[&str] = &["zip", "rar", "7z", "tar", "gz", "bz2", "lzma", "xz"];

#[derive(Debug, PartialEq)]
enum Target {
    Creator(String),
    /// Creator and the tag as written in the URL (percent-encoded).
    Tag(String, String),
    Post(String),
    Home,
    Supporting,
    /// A pixiv user ID.
    Redirect(String),
}

/// The length of the `[\w-]+` prefix of `text`.
fn name_len(text: &str) -> usize {
    text.find(|c: char| !(crate::sites::media::word_char(c) || c == '-'))
        .unwrap_or(text.len())
}

fn target(url: &Url) -> Option<Target> {
    if on_host(url, &["pixiv.net"]) {
        let id = url.path().strip_prefix("/fanbox/creator/")?;
        return Some(Target::Redirect(leading_digits(id)?.to_owned()));
    }
    if !crate::sites::plain(url) {
        return None;
    }
    let host = url.host_str()?;
    let path = url.path();
    // Upstream's `/?$` patterns end the URL: no query or fragment.
    let bare = url.query().is_none() && url.fragment().is_none();
    // `(?!www\.)([\w-]+)\.fanbox\.cc` or `(?:www\.)?fanbox\.cc/@([\w-]+)`.
    let (creator, rest) = if matches!(host, "fanbox.cc" | "www.fanbox.cc") {
        match path.strip_prefix("/@") {
            Some(tail) => tail.split_at(name_len(tail)),
            None if path == "/" && bare => return Some(Target::Home),
            None if path.starts_with("/home/supporting") => return Some(Target::Supporting),
            None => return None,
        }
    } else {
        let label = host.strip_suffix(".fanbox.cc")?;
        if label == "www" || name_len(label) != label.len() {
            return None;
        }
        (label, path)
    };
    if creator.is_empty() {
        return None;
    }
    if matches!(rest, "" | "/" | "/posts" | "/posts/") && bare {
        return Some(Target::Creator(creator.to_owned()));
    }
    if let Some(tag) = rest.strip_prefix("/tags/") {
        let tag = tag.split('/').next().unwrap_or_default();
        return (!tag.is_empty()).then(|| Target::Tag(creator.to_owned(), tag.to_owned()));
    }
    let id = leading_digits(rest.strip_prefix("/posts/")?)?;
    Some(Target::Post(id.to_owned()))
}

/// How embedded videos and links are handled (upstream `embeds`).
#[derive(Clone, Copy, PartialEq)]
enum Embeds {
    Off,
    Queue,
    /// Videos become `ytdl:` files; other embeds are still queued.
    Ytdl,
}

/// Per-extraction API access, options and upstream's memory caches.
struct Ctx<'a> {
    http: &'a Http,
    embeds: Embeds,
    meta_user: bool,
    meta_plan: bool,
    meta_comments: bool,
    fee_max: Option<f64>,
    users: Mutex<HashMap<String, Value>>,
    plans: Mutex<HashMap<String, BTreeMap<i64, Value>>>,
}

impl<'a> Ctx<'a> {
    fn new(http: &'a Http) -> Self {
        let (mut meta_user, mut meta_plan, mut meta_comments) = (false, false, false);
        if let Some(includes) = http.option("metadata").filter(|v| truthy(Some(v))) {
            let names: Vec<&str> = match includes {
                Value::String(s) => s.split(',').collect(),
                Value::Array(items) => items.iter().filter_map(Value::as_str).collect(),
                _ => vec!["user", "plan"],
            };
            meta_user = names.contains(&"user");
            meta_plan = names.contains(&"plan");
            meta_comments = names.contains(&"comments");
        }
        let embeds = match http.option("embeds") {
            None => Embeds::Queue,
            Some(Value::String(s)) if s == "ytdl" => Embeds::Ytdl,
            Some(value) if truthy(Some(value)) => Embeds::Queue,
            Some(_) => Embeds::Off,
        };
        Self {
            http,
            embeds,
            meta_user,
            meta_plan,
            meta_comments: meta_comments || http.option_bool("comments", false),
            fee_max: http.option("fee-max").and_then(Value::as_f64),
            users: Mutex::default(),
            plans: Mutex::default(),
        }
    }

    /// Whether a (non-guest) `FANBOXSESSID` would be sent to the API.
    fn has_session(&self) -> bool {
        self.http.api(&format!("{API}/")).is_ok_and(|url| {
            cookies(self.http, &url)
                .split("; ")
                .any(|pair| pair.starts_with("FANBOXSESSID="))
        })
    }

    /// One API call's `body`. Page URLs from responses must stay on the
    /// API host. Guest sessions the API assigns are not sent back (see
    /// [`cookies`]), so a refusal always names the logged-in cookie.
    async fn call(&self, raw: &str) -> Result<Value> {
        let url = Url::parse(&ensure_http_scheme(raw))
            .ok()
            .filter(|url| on_host(url, &["api.fanbox.cc"]))
            .ok_or_else(|| error("pixivFANBOX returned an invalid page URL"))?;
        let url = self.http.api(url.as_str())?;
        let mut headers = self.http.browser_headers(HEADERS);
        let cookie = cookies(self.http, &url);
        let session = cookie
            .split("; ")
            .any(|pair| pair.starts_with("FANBOXSESSID="));
        // An explicit header replaces the jar's, even when empty.
        headers.push(("Cookie", cookie.as_str()));
        pace().await;
        let (response, body) = match self.http.get_with_headers(&url, &headers).await {
            Ok(response) => response,
            Err(e) if refused_status(&e).is_some() => {
                return Err(error(format!(
                    "pixivFANBOX refused access; this needs a logged-in '{SESSION}' cookie \
                     for .fanbox.cc (--cookies)"
                )));
            }
            Err(e) => return Err(e),
        };
        if !session {
            let mut guests = lock(&GUESTS);
            let guests = guests.get_or_insert_with(HashSet::new);
            for value in response.get_all(reqwest::header::SET_COOKIE) {
                let value = value.to_str().unwrap_or_default();
                if let Some(id) = value.strip_prefix("FANBOXSESSID=")
                    && guests.len() < 1024
                {
                    guests.insert(id.split(';').next().unwrap_or_default().to_owned());
                }
            }
        }
        let mut data: Value = serde_json::from_slice(&body)
            .map_err(|_| error("pixivFANBOX returned invalid JSON"))?;
        match data.get_mut("body") {
            Some(body) => Ok(body.take()),
            None => Err(error("pixivFANBOX returned no body")),
        }
    }

    /// Upstream `_get_user_data`: the creator with its `user` merged in.
    async fn user(&self, creator: &str) -> Result<Value> {
        if let Some(user) = lock(&self.users).get(creator) {
            return Ok(user.clone());
        }
        let mut user = self
            .call(&format!("{API}/creator.get?creatorId={}", encode(creator)))
            .await?;
        if let Some(map) = user.as_object_mut()
            && let Some(Value::Object(inner)) = map.shift_remove("user")
        {
            super::update(map, &inner);
        }
        lock(&self.users).insert(creator.to_owned(), user.clone());
        Ok(user)
    }

    /// Upstream `_get_plan_data`: plans by fee, with a free plan at 0.
    async fn plans(&self, creator: &str) -> Result<BTreeMap<i64, Value>> {
        if let Some(plans) = lock(&self.plans).get(creator) {
            return Ok(plans.clone());
        }
        let data = self
            .call(&format!(
                "{API}/plan.listCreator?creatorId={}",
                encode(creator)
            ))
            .await?;
        let mut plans = BTreeMap::new();
        plans.insert(
            0,
            serde_json::json!({"id": "", "title": "", "fee": 0, "description": "",
                "coverImageUrl": "", "creatorId": creator, "hasAdultContent": null,
                "paymentMethod": null}),
        );
        for mut plan in listed(data, "plans") {
            if let Some(map) = plan.as_object_mut() {
                map.shift_remove("user");
            }
            plans.insert(fee(&plan), plan);
        }
        lock(&self.plans).insert(creator.to_owned(), plans.clone());
        Ok(plans)
    }

    /// Upstream's plan choice: the plan of the post's fee, else the
    /// cheapest more expensive one, else a copy of the free plan with the
    /// post's fee; the choice is remembered for that fee.
    async fn plan(&self, creator: &str, fee: i64) -> Result<Value> {
        let plans = self.plans(creator).await?;
        if let Some(plan) = plans.get(&fee) {
            return Ok(plan.clone());
        }
        let plan = match plans.range(fee..).next() {
            Some((_, plan)) => plan.clone(),
            None => {
                let mut plan = plans[&0].clone();
                plan["fee"] = fee.into();
                plan
            }
        };
        if let Some(plans) = lock(&self.plans).get_mut(creator) {
            plans.insert(fee, plan.clone());
        }
        Ok(plan)
    }

    /// Upstream `_get_comment_data`: every page; failures end the list.
    async fn comments(&self, id: &str) -> Vec<Value> {
        let mut comments = Vec::new();
        let mut url = format!("{API}/post.getComments?limit=10&postId={}", encode(id));
        while let Ok(body) = self.call(&url).await {
            let list = body.get("commentList").cloned().unwrap_or(Value::Null);
            comments.extend(
                list.get("items")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default(),
            );
            match list.get("nextUrl").and_then(Value::as_str) {
                Some(next) if !next.is_empty() => url = next.to_owned(),
                _ => break,
            }
        }
        comments
    }
}

/// `FANBOXSESSID` values the API assigned to requests without a session.
static GUESTS: Mutex<Option<HashSet<String>>> = Mutex::new(None);

/// The session's cookies for `url` without guest sessions: the API sets a
/// guest `FANBOXSESSID` on anonymous requests and refuses later requests of
/// this client that send it back (HTTP 403), so only a session from
/// `--cookies` is sent.
fn cookies(http: &Http, url: &Url) -> String {
    let guests = lock(&GUESTS);
    http.cookie_header(url)
        .unwrap_or_default()
        .split("; ")
        .filter(|pair| {
            !pair.is_empty()
                && !pair
                    .strip_prefix("FANBOXSESSID=")
                    .is_some_and(|id| guests.as_ref().is_some_and(|g| g.contains(id)))
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// Space API calls at least half a second apart, across inputs and child
/// jobs: the API answers rapid requests of one session with HTTP 403.
async fn pace() {
    static LAST: Mutex<Option<std::time::Instant>> = Mutex::new(None);
    if cfg!(test) {
        return;
    }
    let delay = {
        let mut last = lock(&LAST);
        let now = std::time::Instant::now();
        let start = last.map_or(now, |t| {
            (t + std::time::Duration::from_millis(500)).max(now)
        });
        *last = Some(start);
        start - now
    };
    tokio::time::sleep(delay).await;
}

/// A list in a response body: the body itself, as upstream reads it, or
/// the field the current API wraps it in.
fn listed(body: Value, key: &str) -> Vec<Value> {
    match body {
        Value::Array(items) => items,
        Value::Object(mut map) => match map.shift_remove(key) {
            Some(Value::Array(items)) => items,
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

fn encode(text: &str) -> String {
    url::form_urlencoded::byte_serialize(text.as_bytes()).collect()
}

fn fee(value: &Value) -> i64 {
    value
        .get("fee")
        .and_then(Value::as_f64)
        .map_or(0, |fee| fee as i64)
}

/// gallery-dl `text.ensure_http_scheme`.
fn ensure_http_scheme(url: &str) -> String {
    if url.is_empty() || url.starts_with("https://") || url.starts_with("http://") {
        url.to_owned()
    } else {
        format!("https://{}", url.trim_start_matches(['/', ':']))
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported pixivFANBOX URL"));
    };
    let ctx = Arc::new(Ctx::new(http));
    match target {
        Target::Creator(creator) => crate::sites::once(async move {
            let url = format!("{API}/post.paginateCreator?creatorId={}", encode(&creator));
            let mut urls: Vec<String> = listed(ctx.call(&url).await?, "pageUrls")
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect();
            // Upstream's `offset`: skip whole pages of ten, then posts.
            let offset = http.option("offset").and_then(Value::as_u64).unwrap_or(0);
            urls.drain(..((offset / 10) as usize).min(urls.len()));
            let skip = (offset % 10) as usize;
            Ok(stream::iter(urls.into_iter().enumerate())
                .map(move |(index, url)| {
                    let ctx = ctx.clone();
                    crate::sites::once(async move {
                        let mut items = listed(ctx.call(&url).await?, "posts");
                        if index == 0 {
                            items.drain(..skip.min(items.len()));
                        }
                        Ok(posts(ctx, &CREATOR, items, Arc::default()))
                    })
                })
                .flatten()
                .boxed())
        }),
        Target::Tag(creator, tag) => {
            let mut extra = Map::new();
            extra.insert(
                "search_tags".into(),
                crate::sites::percent_decode(&tag).into(),
            );
            let url = format!(
                "{API}/post.listTagged?creatorId={}&tag={tag}",
                encode(&creator)
            );
            pagination(ctx, &TAG, url, Arc::new(extra))
        }
        Target::Post(id) => crate::sites::once(async move {
            let mut item = Map::new();
            item.insert("id".into(), id.into());
            item.insert("feeRequired".into(), 0.into());
            process(ctx, &POST, Value::Object(item), Map::new(), true).await
        }),
        // The feeds answer anonymous requests with HTTP 400.
        Target::Home | Target::Supporting if !ctx.has_session() => {
            crate::sites::fail(error(format!(
                "pixivFANBOX feeds need a logged-in '{SESSION}' cookie for .fanbox.cc (--cookies)"
            )))
        }
        Target::Home => pagination(
            ctx,
            &HOME,
            format!("{API}/post.listHome?limit=10"),
            Arc::default(),
        ),
        Target::Supporting => pagination(
            ctx,
            &SUPPORTING,
            format!("{API}/post.listSupporting?limit=10"),
            Arc::default(),
        ),
        Target::Redirect(id) => crate::sites::once(async move {
            let url = http.api(&format!("https://www.pixiv.net/fanbox/creator/{id}"))?;
            let location = http
                .location(&url, &http.browser_headers(&[]))
                .await?
                .filter(|location| self::target(location).is_some())
                .ok_or_else(|| error("pixivFANBOX user not found"))?;
            Ok(crate::sites::queue(
                http,
                "fanbox",
                "redirect",
                location,
                &Map::new(),
            ))
        }),
    }
}

/// Upstream `_pagination`: `body.items` pages linked by `nextUrl`.
fn pagination<'a>(
    ctx: Arc<Ctx<'a>>,
    route: &'static Route,
    url: String,
    extra: Arc<Map<String, Value>>,
) -> Events<'a> {
    crate::sites::paginate(Some(url), move |url| {
        let (ctx, extra) = (ctx.clone(), extra.clone());
        async move {
            let Some(url) = url else {
                return Ok(None);
            };
            let body = ctx.call(&url).await?;
            let items = ["items", "posts"]
                .into_iter()
                .find_map(|key| body.get(key).and_then(Value::as_array))
                .cloned()
                .ok_or_else(|| error("pixivFANBOX returned no posts"))?;
            let next = body
                .get("nextUrl")
                .and_then(Value::as_str)
                .filter(|next| !next.is_empty())
                .map(str::to_owned);
            Ok(Some((posts(ctx, route, items, extra), next)))
        }
    })
}

/// Process each listed post as it is reached.
fn posts<'a>(
    ctx: Arc<Ctx<'a>>,
    route: &'static Route,
    items: Vec<Value>,
    extra: Arc<Map<String, Value>>,
) -> Events<'a> {
    stream::iter(items)
        .map(move |item| {
            let (ctx, extra) = (ctx.clone(), extra.clone());
            crate::sites::once(
                async move { process(ctx, route, item, (*extra).clone(), false).await },
            )
        })
        .flatten()
        .boxed()
}

/// Upstream `items` for one post: its full `post.info` unless `fee-max`
/// excludes it, then its files and embeds. Listed posts whose details fail
/// keep the listing's data like upstream; a single post's failure is an
/// error, as is its restriction to supporters.
async fn process<'a>(
    ctx: Arc<Ctx<'a>>,
    route: &'static Route,
    item: Value,
    extra: Map<String, Value>,
    single: bool,
) -> Result<Events<'a>> {
    let Value::Object(mut post) = item else {
        return Err(error("pixivFANBOX returned an invalid post"));
    };
    let id = post.get("id").map(crate::sites::scalar).unwrap_or_default();
    let fee = post
        .get("feeRequired")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    if !ctx.fee_max.is_some_and(|max| max < fee) {
        match ctx
            .call(&format!("{API}/post.info?postId={}", encode(&id)))
            .await
        {
            // The current API wraps the post in `post`.
            Ok(Value::Object(mut info)) => match info.shift_remove("post") {
                Some(Value::Object(inner)) => post = inner,
                _ => post = info,
            },
            Ok(_) if single => return Err(error("pixivFANBOX returned an invalid post")),
            Err(e) if single => return Err(e),
            _ => {}
        }
    }
    // Current listings name the cover in `cover` instead.
    if !post.contains_key("coverImageUrl")
        && let Some(cover) = post
            .get("cover")
            .filter(|c| str_of(c, "type") == "cover_image")
    {
        let url = cover.get("url").cloned().unwrap_or(Value::Null);
        post.insert("coverImageUrl".into(), url);
    }
    let (body, mut post) = extract_post(&ctx, post).await?;
    super::update(&mut post, &extra);
    let restricted = single && body.is_none() && truthy(post.get("isRestricted"));
    let (items, queued) = urls_from_post(&ctx, body.as_ref(), &post).await;
    let http = ctx.http;
    let events = crate::sites::events(crate::sites::post(&SITE, http, route, items));
    let subcategory = route.subcategory;
    let queued = stream::iter(queued)
        .map(move |(url, data)| crate::sites::queue(http, "fanbox", subcategory, url, &data))
        .flatten();
    let tail = restricted.then(|| {
        crate::sites::fail(error(format!(
            "pixivFANBOX post {id} is restricted to supporters of its plan; \
             pass a supporter's 'FANBOXSESSID' cookie for .fanbox.cc (--cookies)"
        )))
    });
    Ok(events
        .chain(queued)
        .chain(stream::iter(tail).flatten())
        .boxed())
}

/// Upstream `_extract_post`: the content body and the post's keywords.
async fn extract_post(
    ctx: &Ctx<'_>,
    mut post: Map<String, Value>,
) -> Result<(Option<Map<String, Value>>, Map<String, Value>)> {
    post.insert("archives".into(), Value::Array(Vec::new()));
    let mut body = match post.shift_remove("body") {
        Some(Value::Object(body)) if !body.is_empty() => Some(body),
        _ => None,
    };
    if let Some(body) = body.as_mut() {
        if let Some(html) = body.get("html") {
            post.insert("html".into(), html.clone());
        }
        if post.get("type").and_then(Value::as_str) == Some("article") {
            post.insert("articleBody".into(), Value::Object(body.clone()));
        }
        if let Some(blocks) = body.get("blocks").cloned() {
            let (mut content, mut images, mut files) = (Vec::new(), Vec::new(), Vec::new());
            for block in blocks.as_array().into_iter().flatten() {
                if let Some(text) = block.get("text") {
                    content.push(crate::sites::scalar(text));
                }
                for link in block
                    .get("links")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    content.push(
                        link.get("url")
                            .map(crate::sites::scalar)
                            .unwrap_or_default(),
                    );
                }
                if let Some(image) = block.get("imageId") {
                    images.push(crate::sites::scalar(image));
                }
                if let Some(file) = block.get("fileId") {
                    files.push(crate::sites::scalar(file));
                }
            }
            post.insert("content".into(), content.join("\n").into());
            sort_map(body, "imageMap", &images);
            if let Some(file_map) = sort_map(body, "fileMap", &files) {
                let archives: Vec<Value> = file_map
                    .values()
                    .filter(|file| {
                        ARCHIVES.contains(&str_of(file, "extension").to_lowercase().as_str())
                    })
                    .cloned()
                    .collect();
                post.insert("archives".into(), archives.into());
            }
        }
    }
    let timestamp = post
        .get("publishedDatetime")
        .and_then(Value::as_str)
        .and_then(parse_iso);
    post.insert(
        "date".into(),
        timestamp.map_or(Value::Null, |ts| date(Some(ts))),
    );
    let text = body.as_ref().and_then(|body| body.get("text").cloned());
    post.insert("text".into(), text.unwrap_or(Value::Null));
    post.insert("isCoverImage".into(), false.into());

    let creator = post
        .get("creatorId")
        .filter(|cid| !cid.is_null())
        .map(crate::sites::scalar);
    if let Some(creator) = &creator {
        if ctx.meta_user {
            post.insert("user".into(), ctx.user(creator).await?);
        }
        if ctx.meta_plan {
            let fee = post
                .get("feeRequired")
                .and_then(Value::as_f64)
                .map_or(0, |fee| fee as i64);
            post.insert("plan".into(), ctx.plan(creator, fee).await?);
        }
    }
    if ctx.meta_comments {
        let comments = if truthy(post.get("commentCount")) {
            let id = post.get("id").map(crate::sites::scalar).unwrap_or_default();
            ctx.comments(&id).await
        } else {
            Vec::new()
        };
        post.insert("comments".into(), comments.into());
    }
    Ok((body, post))
}

/// Upstream `_sort_map`: reorder `body[key]` by the IDs' order in the
/// blocks, dropping entries no block names. Returns the map when non-empty.
fn sort_map(
    body: &mut Map<String, Value>,
    key: &str,
    ids: &[String],
) -> Option<Map<String, Value>> {
    let Some(Value::Object(orig)) = body.get(key) else {
        return None;
    };
    if orig.is_empty() {
        return None;
    }
    let mut sorted = Map::new();
    for id in ids {
        if let Some(value) = orig.get(id) {
            sorted.insert(id.clone(), value.clone());
        }
    }
    body.insert(key.into(), Value::Object(sorted.clone()));
    (!sorted.is_empty()).then_some(sorted)
}

/// Remove upstream's `/c/[0-9a-z_]+` thumbnail size segments.
fn full_size(url: &str) -> String {
    let mut out = String::with_capacity(url.len());
    let mut rest = url;
    while let Some(start) = rest.find("/c/") {
        let tail = &rest[start + 3..];
        let len = tail
            .bytes()
            .take_while(|b| b.is_ascii_digit() || b.is_ascii_lowercase() || *b == b'_')
            .count();
        if len == 0 {
            out.push_str(&rest[..start + 3]);
        } else {
            out.push_str(&rest[..start]);
        }
        rest = &tail[len..];
    }
    out.push_str(rest);
    out
}

/// Upstream `_get_urls_from_post`: the cover, HTML links, images, files and
/// embeds, numbered from zero; also the embeds to queue.
async fn urls_from_post(
    ctx: &Ctx<'_>,
    body: Option<&Map<String, Value>>,
    post: &Map<String, Value>,
) -> (Vec<Item>, Vec<(Url, Map<String, Value>)>) {
    let mut files = Files::new(ctx.http, post);
    if let Some(cover) = post.get("coverImageUrl").and_then(Value::as_str)
        && !cover.is_empty()
    {
        let cover = full_size(cover);
        files.push(&cover, |data| {
            data.insert("isCoverImage".into(), true.into());
        });
    }
    let Some(body) = body else {
        return (files.items, Vec::new());
    };
    if let Some(html) = body.get("html").and_then(Value::as_str) {
        let mut urls: Vec<&str> = extract_iter(html, "href=\"", "\"")
            .into_iter()
            .filter(|href| {
                href.contains("fanbox.pixiv.net/images/entry")
                    || href.contains("downloads.fanbox.cc")
            })
            .collect();
        urls.extend(extract_iter(html, "data-src-original=\"", "\""));
        for url in urls {
            files.push(url, |_| {});
        }
    }
    for group in ["images", "imageMap"] {
        for image in values(body.get(group)) {
            let url = str_of(image, "originalUrl");
            files.push(url, |data| {
                if let Some(ext) = image.get("extension") {
                    data.insert("extension".into(), ext.clone());
                }
                for key in ["id", "width", "height"] {
                    let name = if key == "id" { "fileId" } else { key };
                    data.insert(name.into(), image.get(key).cloned().unwrap_or(Value::Null));
                }
            });
        }
    }
    for group in ["files", "fileMap"] {
        for file in values(body.get(group)) {
            let url = str_of(file, "url");
            files.push(url, |data| {
                if let Some(ext) = file.get("extension") {
                    data.insert("extension".into(), ext.clone());
                }
                if let Some(name) = file.get("name") {
                    data.insert("filename".into(), name.clone());
                }
                data.insert(
                    "fileId".into(),
                    file.get("id").cloned().unwrap_or(Value::Null),
                );
            });
        }
    }
    let mut queued = Vec::new();
    if ctx.embeds != Embeds::Off {
        let mut embeds: Vec<&Value> = body.get("video").into_iter().collect();
        embeds.extend(values(body.get("embedMap")));
        for embed in embeds {
            let Some((url, video)) = embed_url(ctx, embed).await else {
                continue;
            };
            let mut data = post.clone();
            data.insert("embed".into(), embed.clone());
            data.insert("embedUrl".into(), url.clone().into());
            super::nameext_into(&url, &mut data);
            data.insert("num".into(), files.num.into());
            files.num += 1;
            if video && ctx.embeds == Embeds::Ytdl {
                let page = url.strip_prefix("ytdl:").unwrap_or(&url);
                if let Some(item) = crate::sites::social::ytdl_item(page, data) {
                    files.items.push(files.ids(item));
                }
            } else if let Ok(url) = Url::parse(&url)
                && crate::sites::plain(&url)
                && crate::sites::supported(&url)
            {
                queued.push((url, data));
            }
        }
    }
    (files.items, queued)
}

/// The values of a list or of a map's entries.
fn values(group: Option<&Value>) -> Vec<&Value> {
    match group {
        Some(Value::Array(items)) => items.iter().collect(),
        Some(Value::Object(map)) => map.values().collect(),
        _ => Vec::new(),
    }
}

/// A post's numbered files.
struct Files<'p> {
    http: &'p Http,
    post: &'p Map<String, Value>,
    num: u64,
    items: Vec<Item>,
}

impl<'p> Files<'p> {
    fn new(http: &'p Http, post: &'p Map<String, Value>) -> Self {
        Self {
            http,
            post,
            num: 0,
            items: Vec::new(),
        }
    }

    /// One file: `fileUrl`, its name and `num`, then `edit`'s keywords.
    /// Invalid URLs still use up their number, as upstream counts them.
    fn push(&mut self, raw: &str, edit: impl FnOnce(&mut Map<String, Value>)) {
        let mut data = self.post.clone();
        data.insert("fileUrl".into(), raw.into());
        super::nameext_into(raw, &mut data);
        edit(&mut data);
        data.insert("num".into(), self.num.into());
        self.num += 1;
        if let Some(url) = crate::sites::media::web_url(raw) {
            // Files share the session like upstream's downloads; only
            // cookies scoped to the file's host apply.
            let cookie = self
                .http
                .api(url.as_str())
                .map(|local| cookies(self.http, &local))
                .unwrap_or_default();
            let mut item = Item::new(url, data).header("Referer", "https://www.fanbox.cc/");
            if !cookie.is_empty() {
                item = item.header("Cookie", &cookie);
            }
            let item = self.ids(item);
            self.items.push(item);
        }
    }

    fn ids(&self, item: Item) -> Item {
        let id = self
            .post
            .get("id")
            .map(crate::sites::scalar)
            .unwrap_or_default();
        let num = item
            .keywords
            .get("num")
            .map(crate::sites::scalar)
            .unwrap_or_default();
        let timestamp = self
            .post
            .get("publishedDatetime")
            .and_then(Value::as_str)
            .and_then(parse_iso);
        let user = self
            .post
            .get("creatorId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let title = self
            .post
            .get("title")
            .and_then(Value::as_str)
            .map(str::to_owned);
        item.ids(id, num)
            .user(user)
            .caption(title)
            .timestamp(timestamp)
    }
}

/// Upstream `_process_embed`: the embed's URL and whether it is a video.
/// Old `fanbox` embeds resolve through pixiv's redirect; unknown services
/// and failed redirects are skipped.
async fn embed_url(ctx: &Ctx<'_>, embed: &Value) -> Option<(String, bool)> {
    let provider = str_of(embed, "serviceProvider");
    let id = [embed.get("videoId"), embed.get("contentId")]
        .into_iter()
        .find(|id| truthy(*id))
        .flatten()
        .map(crate::sites::scalar)
        .unwrap_or_default();
    let prefix = if ctx.embeds == Embeds::Ytdl {
        "ytdl:"
    } else {
        ""
    };
    Some(match provider {
        "soundcloud" => (format!("{prefix}https://soundcloud.com/{id}"), true),
        "youtube" => (format!("{prefix}https://youtube.com/watch?v={id}"), true),
        "vimeo" => (format!("{prefix}https://vimeo.com/{id}"), true),
        "fanbox" => {
            let url = ctx
                .http
                .api(&format!("https://www.pixiv.net/fanbox/{id}"))
                .ok()?;
            let location = ctx
                .http
                .location(&url, &ctx.http.browser_headers(&[]))
                .await
                .ok()??;
            (location.to_string(), false)
        }
        "twitter" => (format!("https://twitter.com/_/status/{id}"), false),
        "google_forms" => (
            format!("https://docs.google.com/forms/d/e/{id}/viewform?usp=sf_link"),
            false,
        ),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http as client, json, keywords, media, queued, response, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        let creator = |name: &str| Some(Target::Creator(name.into()));
        assert_eq!(t("https://xub.fanbox.cc"), creator("xub"));
        assert_eq!(t("https://xub.fanbox.cc/posts/"), creator("xub"));
        assert_eq!(t("https://www.fanbox.cc/@xub"), creator("xub"));
        assert_eq!(
            t("https://fanbox.cc/@my-name_1/posts"),
            creator("my-name_1")
        );
        assert_eq!(
            t("https://mochirong.fanbox.cc/tags/%E3%81%BF%E3%82%8B/x"),
            Some(Target::Tag("mochirong".into(), "%E3%81%BF%E3%82%8B".into()))
        );
        assert_eq!(
            t("https://www.fanbox.cc/@xub/posts/1910054?x=1"),
            Some(Target::Post("1910054".into()))
        );
        assert_eq!(
            t("https://xub.fanbox.cc/posts/1910054"),
            Some(Target::Post("1910054".into()))
        );
        assert_eq!(t("https://fanbox.cc/"), Some(Target::Home));
        assert_eq!(t("https://www.fanbox.cc"), Some(Target::Home));
        assert_eq!(
            t("https://www.fanbox.cc/home/supporting"),
            Some(Target::Supporting)
        );
        assert_eq!(
            t("https://www.pixiv.net/fanbox/creator/52336352"),
            Some(Target::Redirect("52336352".into()))
        );
        for raw in [
            "https://www.fanbox.cc/?x=1",
            "https://xub.fanbox.cc/?x=1",
            "https://xub.fanbox.cc/about",
            "https://www.fanbox.cc/@",
            "https://www.fanbox.cc/@xub.x",
            "https://www.fanbox.cc/user/xub",
            "https://xub.fanbox.cc/posts/abc",
            "https://xub.fanbox.cc/tags/",
            "https://a.b.fanbox.cc/",
            "https://fanbox.cc.evil.com/@xub",
            "https://xub.fanbox.cc.evil.com/",
            "https://evilfanbox.cc/@xub",
            "https://xub.fanbox.cc:8443/",
            "https://u:p@xub.fanbox.cc/",
            "https://www.pixiv.net/fanbox/creator/x",
            "https://pixiv.net.evil.com/fanbox/creator/1",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn article() -> Value {
        json!({"body": {
            "id": "1910054", "title": "Article", "type": "article", "feeRequired": 500,
            "creatorId": "xub", "publishedDatetime": "2021-02-01T12:00:00+09:00",
            "coverImageUrl": "https://pixiv.pximg.net/c/1200x630_90_a2_g5/fanbox/public/images/post/1910054/cover/abc.jpeg",
            "isRestricted": false, "commentCount": 1,
            "body": {
                "blocks": [
                    {"type": "p", "text": "Hello", "links": [{"url": "https://example.test/"}]},
                    {"type": "image", "imageId": "img2"},
                    {"type": "image", "imageId": "img1"},
                    {"type": "file", "fileId": "f1"},
                    {"type": "embed", "embedId": "e1"}
                ],
                "imageMap": {
                    "img1": {"id": "img1", "extension": "png", "width": 10, "height": 20,
                        "originalUrl": "https://downloads.fanbox.cc/images/post/1910054/img1.png"},
                    "img2": {"id": "img2", "extension": "jpeg", "width": 30, "height": 40,
                        "originalUrl": "https://downloads.fanbox.cc/images/post/1910054/img2.jpeg"},
                    "unused": {"id": "unused", "originalUrl": "https://downloads.fanbox.cc/x.png"}
                },
                "fileMap": {"f1": {"id": "f1", "name": "pack", "extension": "zip",
                    "url": "https://downloads.fanbox.cc/files/post/1910054/f1.zip"}},
                "embedMap": {
                    "e1": {"id": "e1", "serviceProvider": "twitter", "contentId": "123"},
                    "e2": {"id": "e2", "serviceProvider": "youtube", "videoId": "abc"},
                    "e3": {"id": "e3", "serviceProvider": "unknown", "contentId": "x"}
                }
            }
        }})
    }

    #[tokio::test]
    async fn extracts_article_post() {
        let (root, server) = serve(vec![json(article())]);
        let options = serde_json::from_value(json!({"embeds": false})).unwrap();
        let http = client("pixivFANBOX", root.clone()).with_options(Some(Arc::new(options)));
        http.set_cookie(&root, "FANBOXSESSID=sess; Path=/");
        let events = extract(
            &http,
            Url::parse("https://xub.fanbox.cc/posts/1910054").unwrap(),
        );
        let files = media(events).await;
        assert_eq!(files.len(), 4);
        let k = keywords(&files[0]);
        assert_eq!(k["isCoverImage"], true);
        assert_eq!(k["num"], 0);
        assert_eq!(
            k["fileUrl"],
            "https://pixiv.pximg.net/fanbox/public/images/post/1910054/cover/abc.jpeg"
        );
        assert_eq!(k["content"], "Hello\nhttps://example.test/");
        assert_eq!(k["date"], "2021-02-01 03:00:00");
        assert_eq!(k["archives"][0]["id"], "f1");
        assert_eq!(k["articleBody"]["imageMap"]["unused"]["id"], "unused");
        assert_eq!(k["text"], Value::Null);
        assert_eq!(k["subcategory"], "post");
        let k = keywords(&files[1]);
        assert_eq!(k["fileId"], "img2");
        assert_eq!(k["isCoverImage"], false);
        assert_eq!(
            (k["width"].clone(), k["num"].clone()),
            (json!(30), json!(1))
        );
        let k = keywords(&files[3]);
        assert_eq!(k["filename"], "pack");
        assert_eq!(k["extension"], "zip");
        assert_eq!(k["num"], 3);
        let (file, dir) = crate::sites::default_templates(&files[2]).unwrap();
        assert_eq!(file.render(&files[2]).unwrap(), "1910054_2.png");
        assert_eq!(
            dir.render(&files[2]).unwrap().unwrap(),
            std::path::PathBuf::from("fanbox/xub")
        );
        let (_, identity) = crate::sites::archive_key(&files[3]).unwrap();
        assert_eq!(identity, r#"["{id}_{num}",["1910054","3"]]"#);
        assert_eq!(
            files[1].headers.get("Cookie").map(String::as_str),
            Some("FANBOXSESSID=sess")
        );
        assert_eq!(
            files[1].headers.get("Referer").map(String::as_str),
            Some("https://www.fanbox.cc/")
        );
        let requests = server.join().unwrap();
        let request = requests[0].to_ascii_lowercase();
        assert!(
            request.starts_with("get /post.info?postid=1910054 "),
            "{request}"
        );
        assert!(request.contains("cookie: fanboxsessid=sess"));
        assert!(request.contains("origin: https://www.fanbox.cc"));
        assert!(request.contains("referer: https://www.fanbox.cc/"));
        assert!(request.contains("sec-fetch-site: same-site"));
        assert!(request.contains("user-agent: mozilla/5.0"));
    }

    #[tokio::test]
    async fn embeds_are_queued_or_handed_to_ytdl() {
        let (root, server) = serve(vec![json(article())]);
        let http = client("pixivFANBOX", root);
        let children = queued(extract(
            &http,
            Url::parse("https://xub.fanbox.cc/posts/1910054").unwrap(),
        ))
        .await;
        // YouTube is queued for the yt-dlp site; without the program's
        // router (as in this test) the Twitter embed has no extractor.
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].subcategory(), "post");
        server.join().unwrap();

        let (root, server) = serve(vec![json(article())]);
        let options = serde_json::from_value(json!({"embeds": "ytdl"})).unwrap();
        let http = client("pixivFANBOX", root).with_options(Some(Arc::new(options)));
        let files = media(extract(
            &http,
            Url::parse("https://xub.fanbox.cc/posts/1910054").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 5);
        let k = keywords(&files[4]);
        assert_eq!(k["embedUrl"], "ytdl:https://youtube.com/watch?v=abc");
        assert_eq!(k["num"], 5);
        assert!(matches!(
            files[4].job,
            crate::extract::MediaJob::VideoPage { ref page_url }
                if page_url.as_str() == "https://youtube.com/watch?v=abc"
        ));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn metadata_and_comments_options() {
        let user = json!({"body": {"creatorId": "xub", "description": "Hi",
            "user": {"userId": "7", "name": "Xub"}}});
        let plans = json!({"body": {"plans": [
            {"id": "p1", "fee": 300, "title": "Small", "user": {"userId": "7"}},
            {"id": "p2", "fee": 1000, "title": "Big", "user": {"userId": "7"}}]}});
        let comments = json!({"body": {"commentList": {
            "items": [{"id": "c1", "body": "nice"}],
            "nextUrl": "https://api.fanbox.cc/post.getComments?limit=10&postId=1910054&page=2"}}});
        let (root, server) = serve(vec![
            json(article()),
            json(user),
            json(plans),
            json(comments),
            response("500 Internal Server Error", "text/plain", b""),
        ]);
        let options =
            serde_json::from_value(json!({"metadata": "user,plan", "comments": true, "embeds": 0}))
                .unwrap();
        let http = client("pixivFANBOX", root).with_options(Some(Arc::new(options)));
        let files = media(extract(
            &http,
            Url::parse("https://www.fanbox.cc/@xub/posts/1910054").unwrap(),
        ))
        .await;
        let k = keywords(&files[0]);
        assert_eq!(k["user"]["name"], "Xub");
        assert_eq!(k["user"]["description"], "Hi");
        assert_eq!(k["user"].get("user"), None);
        // A 500 fee falls to the cheapest more expensive plan.
        assert_eq!(k["plan"]["id"], "p2");
        assert_eq!(k["plan"].get("user"), None);
        assert_eq!(k["comments"], json!([{"id": "c1", "body": "nice"}]));
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /creator.get?creatorId=xub "));
        assert!(requests[2].starts_with("GET /plan.listCreator?creatorId=xub "));
        assert!(requests[3].starts_with("GET /post.getComments?limit=10&postId=1910054 "));
        assert!(requests[4].contains("&page=2 "));
    }

    fn listing(id: &str, fee: u64) -> Value {
        json!({"id": id, "title": "T", "feeRequired": fee, "creatorId": "xub",
            "coverImageUrl": format!("https://pixiv.pximg.net/fanbox/{id}.jpeg")})
    }

    #[tokio::test]
    async fn creator_pages_with_offset_and_fee_max() {
        // The current API's wrapped `pageUrls`, `posts` and `post` bodies.
        let pages = json!({"body": {"pageUrls": [
            "https://api.fanbox.cc/post.listCreator?creatorId=xub&maxPublishedDatetime=a&limit=10",
            "api.fanbox.cc/post.listCreator?creatorId=xub&maxPublishedDatetime=b&limit=10"]}});
        let mut paid = listing("4", 1000);
        let cover = paid
            .as_object_mut()
            .unwrap()
            .shift_remove("coverImageUrl")
            .unwrap();
        paid["cover"] = json!({"type": "cover_image", "url": cover});
        let second = json!({"body": {"posts": [listing("3", 0), paid]}});
        let info = json!({"body": {"post": {"id": "3", "title": "Full", "type": "image",
            "creatorId": "xub", "feeRequired": 0, "body": {"images": [{"id": "i",
                "extension": "png",
                "originalUrl": "https://downloads.fanbox.cc/images/post/3/i.png"}]}}}});
        // The anonymous first request is assigned a guest session.
        let pages = response(
            "200 OK\r\nSet-Cookie: FANBOXSESSID=guest1; Path=/\r\nSet-Cookie: other=1; Path=/",
            "application/json",
            pages.to_string().as_bytes(),
        );
        let (root, server) = serve(vec![pages, json(second), json(info)]);
        let options = serde_json::from_value(json!({"offset": 10, "fee-max": 500})).unwrap();
        let http = client("pixivFANBOX", root).with_options(Some(Arc::new(options)));
        let files = media(extract(
            &http,
            Url::parse("https://xub.fanbox.cc/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(keywords(&files[0])["title"], "Full");
        assert_eq!(keywords(&files[0])["subcategory"], "creator");
        // Above `fee-max`: only the listing's cover, without `post.info`.
        assert_eq!(keywords(&files[1])["id"], "4");
        assert_eq!(keywords(&files[1])["isCoverImage"], true);
        assert_eq!(
            files[1].url.as_str(),
            "https://pixiv.pximg.net/fanbox/4.jpeg"
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /post.paginateCreator?creatorId=xub "));
        assert!(requests[1].contains("maxPublishedDatetime=b"));
        assert!(requests[2].starts_with("GET /post.info?postId=3 "));
        // It is never sent back; other cookies are.
        assert!(
            requests[1].contains("cookie: other=1\r\n"),
            "{}",
            requests[1]
        );
        assert!(!requests[2].contains("guest1"));
        assert_eq!(
            files[0].headers.get("Cookie").map(String::as_str),
            Some("other=1")
        );
    }

    #[tokio::test]
    async fn feeds_follow_next_urls_and_tags_add_search_tags() {
        let first = json!({"body": {"items": [listing("1", 0)],
            "nextUrl": "https://api.fanbox.cc/post.listHome?limit=10&maxId=1"}});
        let last = json!({"body": {"items": [], "nextUrl": null}});
        let refused = response("403 Forbidden", "application/json", b"{}");
        let (root, server) = serve(vec![json(first), refused, json(last)]);
        let http = client("pixivFANBOX", root.clone());
        http.set_cookie(&root, "FANBOXSESSID=user; Path=/");
        let files = media(extract(
            &http,
            Url::parse("https://www.fanbox.cc/").unwrap(),
        ))
        .await;
        // A listed post whose details are refused keeps the listing's data.
        assert_eq!(files.len(), 1);
        assert_eq!(keywords(&files[0])["subcategory"], "home");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /post.listHome?limit=10 "));
        assert!(requests[2].starts_with("GET /post.listHome?limit=10&maxId=1 "));

        let tagged = json!({"body": {"items": [listing("5", 0)], "nextUrl": null}});
        let (root, server) = serve(vec![json(tagged), json(json!({"body": listing("5", 0)}))]);
        let http = client("pixivFANBOX", root);
        let files = media(extract(
            &http,
            Url::parse("https://xub.fanbox.cc/tags/%E3%81%BF").unwrap(),
        ))
        .await;
        assert_eq!(keywords(&files[0])["search_tags"], "み");
        assert_eq!(keywords(&files[0])["subcategory"], "tag");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /post.listTagged?creatorId=xub&tag=%E3%81%BF "));

        let (root, server) = serve(vec![json(json!({"body": {"items": [], "nextUrl": ""}}))]);
        let http = client("pixivFANBOX", root.clone());
        http.set_cookie(&root, "FANBOXSESSID=user; Path=/");
        let files = media(extract(
            &http,
            Url::parse("https://www.fanbox.cc/home/supporting").unwrap(),
        ))
        .await;
        assert!(files.is_empty());
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /post.listSupporting?limit=10 "));
    }

    #[tokio::test]
    async fn sessions_are_reported() {
        async fn message(events: Events<'_>) -> String {
            let results: Vec<_> = crate::event::media_only(events).collect().await;
            results[0].as_ref().err().unwrap().to_string()
        }
        // The feeds need a session and are not requested without one.
        let http = client("pixivFANBOX", Url::parse("http://127.0.0.1:9/").unwrap());
        let feed = Url::parse("https://www.fanbox.cc/home/supporting").unwrap();
        let text = message(extract(&http, feed)).await;
        assert!(text.contains("FANBOXSESSID"), "{text}");

        let refused = response("401 Unauthorized", "application/json", b"{}");
        let (root, server) = serve(vec![refused]);
        let http = client("pixivFANBOX", root);
        let text = message(extract(
            &http,
            Url::parse("https://xub.fanbox.cc/").unwrap(),
        ))
        .await;
        assert!(text.contains("FANBOXSESSID"), "{text}");
        server.join().unwrap();

        let restricted = json!({"body": {"id": "9", "title": "Paid", "type": "image",
            "creatorId": "xub", "isRestricted": true, "body": null,
            "coverImageUrl": "https://pixiv.pximg.net/fanbox/9.jpeg"}});
        let (root, server) = serve(vec![json(restricted)]);
        let http = client("pixivFANBOX", root);
        let results: Vec<_> = crate::event::media_only(extract(
            &http,
            Url::parse("https://xub.fanbox.cc/posts/9").unwrap(),
        ))
        .collect()
        .await;
        assert_eq!(results.len(), 2);
        assert!(results[0].is_ok());
        let message = results[1].as_ref().err().unwrap().to_string();
        assert!(message.contains("restricted") && message.contains("FANBOXSESSID"));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn redirects_queue_the_creator() {
        let moved = b"HTTP/1.1 302 Found\r\nLocation: https://www.fanbox.cc/@xub\r\n\
            Content-Length: 0\r\nConnection: close\r\n\r\n"
            .to_vec();
        let (root, server) = serve(vec![moved]);
        let http = client("pixivFANBOX", root);
        let children = queued(extract(
            &http,
            Url::parse("https://www.pixiv.net/fanbox/creator/52336352").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].subcategory(), "redirect");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /fanbox/creator/52336352 "));
    }

    #[test]
    fn helpers() {
        assert_eq!(
            full_size("https://x/c/1200x630_90_a2_g5/fanbox/c/x/a.jpeg"),
            "https://x/fanbox/a.jpeg"
        );
        assert_eq!(full_size("https://x/c//a"), "https://x/c//a");
        assert_eq!(
            ensure_http_scheme("//api.fanbox.cc/x"),
            "https://api.fanbox.cc/x"
        );
        assert_eq!(ensure_http_scheme("http://a"), "http://a");
    }
}
