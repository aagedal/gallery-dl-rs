//! Patreon creators, collections, posts and the home feed, mirroring
//! gallery_dl/extractor/patreon.py through the `www.patreon.com/api`
//! JSON:API endpoints and the pages' bootstrap data.
//!
//! Patron-only posts need a `session_id` cookie for `.patreon.com` from
//! `--cookies`. Requests send upstream's Patreon Android User-Agent unless
//! one is configured. HLS videos (`.m3u8`, such as Mux streams) are handed
//! to yt-dlp as upstream's `ytdl:` manifest URLs. Upstream's resume-cursor
//! log message is not shown; the `cursor` option is honored.
//!
//! Current pages need two additions to upstream's bootstrap extraction: the
//! `__NEXT_DATA__` script now carries a `nonce`, and post pages stream the
//! bootstrap envelope as React Server Component chunks.
use super::{date, extr, extract_iter, on_host, parse_iso, parse_query, segment, str_of, truthy};
use crate::extract::{ManifestProtocol, MediaJob};
use crate::sites::{Events, Http, Item, Route, Site, http::refused_status};
use crate::{Result, error};
use futures_util::{StreamExt, stream};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use url::Url;

const ROOT: &str = "https://www.patreon.com";
const SESSION: &str = "session_id";
/// Upstream's `useragent`.
const AGENT: &str = "Patreon/126.9.0.15 (Android; Android 14; Scale/2.10)";
/// The older app User-Agent whose post pages list `.m3u8` manifests.
const POST_AGENT: &str = "Patreon/14.2.1 (Android; Android 11; Scale/2.10)";

pub static SITE: Site = Site {
    category: "patreon",
    name: "Patreon",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&COLLECTION, &CREATOR, &USER, &POST],
    api_key: false,
    cookies: Some("patreon.com"),
};

const FILENAME: &str = "{id}_{title}_{num:>02}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{creator[full_name]}"];
const ARCHIVE: &str = "{id}_{num}";

static COLLECTION: Route = Route::new(
    "collection",
    FILENAME,
    &[
        "{category}",
        "{creator[full_name]}",
        "Collections",
        "{collection[title]} ({collection[id]})",
    ],
    ARCHIVE,
);
static CREATOR: Route = Route::new("creator", FILENAME, DIRECTORY, ARCHIVE);
static USER: Route = Route::new("user", FILENAME, DIRECTORY, ARCHIVE);
static POST: Route = Route::new("post", FILENAME, DIRECTORY, ARCHIVE);

#[derive(Debug, PartialEq)]
enum Target {
    Collection(String),
    /// The creator name (`None` for `profile/creators`) and the web query.
    Creator(Option<String>, Vec<(String, String)>),
    User,
    Post(String),
}

/// First path segments upstream's creator pattern refuses.
const RESERVED: &[&str] = &[
    "home", "create", "login", "signup", "search", "posts", "messages",
];

fn target(url: &Url) -> Option<Target> {
    if !on_host(url, &["patreon.com"]) {
        return None;
    }
    let path = url.path();
    if let Some(id) = path.strip_prefix("/collection/") {
        return Some(Target::Collection(super::leading_digits(id)?.to_owned()));
    }
    if path == "/home" && url.query().is_none() && url.fragment().is_none() {
        return Some(Target::User);
    }
    if let Some(slug) = path.strip_prefix("/posts/") {
        let slug = slug.split('/').next().unwrap_or_default();
        return segment(slug).then(|| Target::Post(slug.to_owned()));
    }
    let rest = &path[1..];
    if RESERVED.contains(&rest.split('/').next().unwrap_or_default()) {
        return None;
    }
    // `(?:profile/creators|(?:cw?/)?([^/?#]+)(?:/posts)?)/?(?:\?([^#]+))?`
    let (creator, tail) = match rest.strip_prefix("profile/creators") {
        Some(tail) => (None, tail),
        None => {
            let rest = ["cw/", "c/"]
                .iter()
                .find_map(|prefix| rest.strip_prefix(prefix).filter(|r| segment_start(r)))
                .unwrap_or(rest);
            let name = rest.split('/').next().unwrap_or_default();
            if name.is_empty() {
                return None;
            }
            (Some(name.to_owned()), &rest[name.len()..])
        }
    };
    let query = match url.query() {
        Some(query) if !query.is_empty() && matches!(tail, "" | "/" | "/posts" | "/posts/") => {
            parse_query(query)
        }
        _ => Vec::new(),
    };
    Some(Target::Creator(creator, query))
}

fn segment_start(text: &str) -> bool {
    !text.is_empty() && !text.starts_with('/')
}

/// File sources in upstream's default `files` order.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Images,
    ImageLarge,
    Attachments,
    Postfile,
    Content,
}

const KINDS: &[(&str, Kind)] = &[
    ("images", Kind::Images),
    ("image_large", Kind::ImageLarge),
    ("attachments", Kind::Attachments),
    ("postfile", Kind::Postfile),
    ("content", Kind::Content),
];

/// Upstream `_build_file_generators`.
fn kinds(option: Option<&Value>) -> Result<Vec<Kind>> {
    let names: Vec<&str> = match option {
        None | Some(Value::Null) => return Ok(KINDS.iter().map(|(_, kind)| *kind).collect()),
        Some(Value::String(s)) => s.split(',').collect(),
        Some(Value::Array(items)) => items.iter().filter_map(Value::as_str).collect(),
        Some(_) => Vec::new(),
    };
    names
        .into_iter()
        .map(|name| {
            KINDS
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, kind)| *kind)
                .ok_or_else(|| error(format!("unknown Patreon 'files' type '{name}'")))
        })
        .collect()
}

/// Per-extraction requests, options and upstream's user cache.
struct Ctx<'a> {
    http: &'a Http,
    logged_in: bool,
    kinds: Vec<Kind>,
    /// Upstream `format-images`: an `image_urls` key preferred over
    /// `download_url`.
    images_fmt: Option<String>,
    users: Mutex<HashMap<String, Option<Value>>>,
}

impl<'a> Ctx<'a> {
    fn new(http: &'a Http) -> Result<Self> {
        let logged_in = http
            .api(&format!("{ROOT}/"))
            .is_ok_and(|url| http.cookie(&url, SESSION).is_some());
        Ok(Self {
            http,
            logged_in,
            kinds: kinds(http.option("files"))?,
            images_fmt: http
                .option("format-images")
                .filter(|v| truthy(Some(v)))
                .map(crate::sites::scalar),
            users: Mutex::default(),
        })
    }

    fn agent(&self) -> &str {
        self.http.network.user_agent().unwrap_or(AGENT)
    }

    fn refused(&self, e: crate::Error) -> crate::Error {
        if refused_status(&e).is_some() && !self.logged_in {
            error("Patreon refused access; pass a 'session_id' cookie for .patreon.com (--cookies)")
        } else {
            e
        }
    }

    /// A JSON document from the site; URLs from responses must stay on it.
    async fn json(&self, raw: &str, api: bool) -> Result<Value> {
        let url = site_url(raw)?;
        let mut headers = vec![("User-Agent", self.agent())];
        if api {
            headers.push(("Content-Type", "application/vnd.api+json"));
        }
        self.http
            .json_with(&self.http.api(url.as_str())?, &headers)
            .await
            .map_err(|e| self.refused(e))
    }

    async fn page(&self, raw: &str, agent: &str) -> Result<String> {
        let url = self.http.api(site_url(raw)?.as_str())?;
        self.http
            .text_with(&url, &[("User-Agent", agent)])
            .await
            .map_err(|e| self.refused(e))
    }

    /// Upstream `_user`, cached by URL: `None` when the request fails.
    async fn user(&self, url: &str) -> Option<Value> {
        if let Some(user) = lock(&self.users).get(url) {
            return user.clone();
        }
        let user = self.json(url, false).await.ok().and_then(|data| {
            let user = data.get("data")?;
            let mut attr = user.get("attributes")?.as_object()?.clone();
            attr.insert("id".into(), user.get("id").cloned().unwrap_or(Value::Null));
            attr.insert("date".into(), iso_date(attr.get("created")));
            Some(Value::Object(attr))
        });
        lock(&self.users).insert(url.to_owned(), user.clone());
        user
    }

    /// Upstream `_filename`: the `Content-Disposition` name of a HEAD
    /// request, `""` on failure.
    async fn filename(&self, raw: &str) -> String {
        let Ok(url) = self.http.api(raw) else {
            return String::new();
        };
        let headers = [("User-Agent", self.agent())];
        match self.http.head(&url, &headers).await {
            Ok(headers) => super::aryion::filename_from_contentdisposition(
                headers
                    .get("content-disposition")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default(),
            ),
            Err(_) => String::new(),
        }
    }

    /// Upstream `request_location(fatal=False)` with the session cookies.
    async fn location(&self, raw: &str) -> Option<String> {
        let url = self.http.api(site_url(raw).ok()?.as_str()).ok()?;
        let cookie = self.http.cookie_header(&url);
        let mut headers = vec![("User-Agent", self.agent())];
        if let Some(cookie) = &cookie {
            headers.push(("Cookie", cookie.as_str()));
        }
        let location = self.http.location(&url, &headers).await.ok()??;
        Some(location.to_string())
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// An HTTP(S) URL on patreon.com, with gallery-dl `ensure_http_scheme`.
fn site_url(raw: &str) -> Result<Url> {
    let raw = if raw.starts_with("https://") || raw.starts_with("http://") {
        raw.to_owned()
    } else {
        format!("https://{}", raw.trim_start_matches(['/', ':']))
    };
    Url::parse(&raw)
        .ok()
        .filter(|url| on_host(url, &["patreon.com"]))
        .ok_or_else(|| error("Patreon returned an invalid page URL"))
}

fn iso_date(value: Option<&Value>) -> Value {
    match value.and_then(Value::as_str).and_then(parse_iso) {
        Some(ts) => date(Some(ts)),
        None => Value::Null,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Patreon URL"));
    };
    crate::sites::once(async move {
        let ctx = Arc::new(Ctx::new(http)?);
        Ok(match target {
            Target::Collection(id) => {
                let mut data = ctx
                    .json(&format!("{ROOT}/api/collection/{id}"), false)
                    .await?;
                let coll = data.get_mut("data").map(Value::take).unwrap_or(Value::Null);
                let mut attr = coll
                    .get("attributes")
                    .and_then(Value::as_object)
                    .cloned()
                    .ok_or_else(|| error("Patreon returned no collection"))?;
                attr.insert("id".into(), coll.get("id").cloned().unwrap_or(Value::Null));
                attr.insert("date".into(), iso_date(attr.get("created_at")));
                let thumbnail = attr
                    .get("thumbnail")
                    .map(|t| str_of(t, "url"))
                    .unwrap_or("");
                let campaign = extr(thumbnail, "/campaign/", "/").to_owned();
                // Patreon answers '400 Bad Request' without the campaign filter.
                let url = build_url(
                    "posts",
                    order(http, true, Some("collection_order")),
                    &format!(
                        "&filter[campaign_id]={campaign}\
                         &filter[contains_exclusive_posts]=true\
                         &filter[is_draft]=false\
                         &filter[collection_id]={id}\
                         &filter[include_drops]=true"
                    ),
                    cursor(http),
                );
                let mut extra = Map::new();
                extra.insert("collection".into(), Value::Object(attr));
                pagination(ctx, &COLLECTION, url, Arc::new(extra))
            }
            Target::Creator(creator, params) => {
                let campaign = campaign_id(&ctx, creator.as_deref(), &params).await?;
                let sort = param(&params, "sort").unwrap_or("-published_at");
                let filters: String = params
                    .iter()
                    .filter(|(key, _)| key.starts_with("filters["))
                    .map(|(key, value)| format!("&filter[{}={}", &key[8..], escape(value)))
                    .collect();
                let url = build_url(
                    "posts",
                    order(http, false, Some(sort)),
                    &format!(
                        "&filter[campaign_id]={campaign}\
                         &filter[contains_exclusive_posts]=true\
                         &filter[is_draft]=false{filters}"
                    ),
                    cursor(http),
                );
                pagination(ctx, &CREATOR, url, Arc::default())
            }
            // Anonymous requests get an empty feed.
            Target::User if !ctx.logged_in => {
                return Err(error(
                    "the Patreon home feed needs a 'session_id' cookie for .patreon.com (--cookies)",
                ));
            }
            Target::User => {
                // `date-max` starts the stream at that date's cursor.
                let cursor = match date_max(http) {
                    Some(ts) => isoformat(ts),
                    None => cursor(http),
                };
                let url = build_url(
                    "stream",
                    order(http, false, None),
                    "&filter[is_following]=true&json-api-use-default-includes=false",
                    cursor,
                );
                pagination(ctx, &USER, url, Arc::default())
            }
            Target::Post(slug) => {
                let agent = if !ctx.logged_in && http.network.user_agent().is_none() {
                    // Enables `.m3u8` manifest downloads.
                    POST_AGENT
                } else {
                    ctx.agent()
                };
                let page = ctx.page(&format!("{ROOT}/posts/{slug}"), agent).await?;
                let bootstrap = bootstrap(&page)?;
                let Some(post) = bootstrap.get("post") else {
                    if str_of(&bootstrap, "campaignDisciplinaryStatus") == "suspended" {
                        return Err(error("Patreon account suspended"));
                    }
                    return Err(error("Patreon page has no post data"));
                };
                let included = transform(post.get("included"));
                let data = post.get("data").cloned().unwrap_or(Value::Null);
                let post = process(&ctx, data, &included).await?;
                post_events(ctx, &POST, post, Map::new(), true).await?
            }
        })
    })
}

fn param<'p>(params: &'p [(String, String)], key: &str) -> Option<&'p str> {
    params
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.as_str())
}

/// Upstream `_get_campaign_id`.
async fn campaign_id(
    ctx: &Ctx<'_>,
    creator: Option<&str>,
    params: &[(String, String)],
) -> Result<String> {
    let id = if let Some(id) = creator.and_then(|c| c.strip_prefix("id:")) {
        id.to_owned()
    } else if let Some(id) = [param(params, "c"), param(params, "campaign_id")]
        .into_iter()
        .flatten()
        .find(|id| !id.is_empty())
    {
        id.to_owned()
    } else {
        let url = match param(params, "u") {
            Some(user) => format!("{ROOT}/user?u={}", encode(user)),
            None => format!("{ROOT}/{}", creator.unwrap_or("None")),
        };
        let page = ctx.page(&url, ctx.agent()).await?;
        let found = bootstrap(&page)
            .ok()
            .and_then(|data| {
                data.get("campaign")?
                    .get("data")?
                    .get("id")
                    .map(crate::sites::scalar)
            })
            .filter(|id| !id.is_empty());
        // Next.js 13 pages carry it in escaped JSON instead.
        match found {
            Some(id) => id,
            None => match extr(
                &page,
                r#"{\"value\":{\"campaign\":{\"data\":{\"id\":\""#,
                "\\\"",
            ) {
                "" => return Err(error("Failed to extract Patreon campaign ID")),
                id => id.to_owned(),
            },
        }
    };
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(error("invalid Patreon campaign ID"));
    }
    Ok(id)
}

fn encode(text: &str) -> String {
    url::form_urlencoded::byte_serialize(text.as_bytes()).collect()
}

/// Python `html.escape`.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            c => out.push(c),
        }
    }
    out
}

/// The upstream `cursor` option: `true` (the default) starts at the
/// beginning, a string at that cursor.
fn cursor(http: &Http) -> String {
    match http.option("cursor") {
        Some(Value::String(cursor)) => cursor.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

/// The `date-max` option: a Unix timestamp or an ISO 8601 date.
fn date_max(http: &Http) -> Option<i64> {
    match http.option("date-max")? {
        Value::String(text) => parse_iso(text),
        value => value.as_f64().map(|ts| ts as i64),
    }
}

/// Python `datetime.isoformat()` of a naive UTC datetime.
fn isoformat(ts: i64) -> String {
    date(Some(ts))
        .as_str()
        .map(|text| text.replacen(' ', "T", 1))
        .unwrap_or_default()
}

/// Upstream `_order`, with the collection class's own mapping.
fn order(http: &Http, collection: bool, sort: Option<&str>) -> String {
    if let Some(order) = http.option("order-posts").filter(|v| truthy(Some(v))) {
        let order = crate::sites::scalar(order);
        let order = match order.as_str() {
            "a" | "asc" if collection => "collection_order",
            "d" | "desc" | "r" | "reverse" if collection => "-published_at",
            "d" | "desc" => "-published_at",
            "a" | "asc" | "r" | "reverse" => "published_at",
            other => other,
        };
        return format!("&sort={order}");
    }
    sort.map(|sort| format!("&sort={sort}")).unwrap_or_default()
}

/// Upstream `_build_url`.
fn build_url(endpoint: &str, order: String, query: &str, cursor: String) -> String {
    format!(
        "{ROOT}/api/{endpoint}\
         ?include=campaign,access_rules,attachments,attachments_media,\
         audio,images,media,native_video_insights,poll.choices,\
         poll.current_user_responses.user,\
         poll.current_user_responses.choice,\
         poll.current_user_responses.poll,\
         user,user_defined_tags,ti_checks\
         &fields[campaign]=currency,show_audio_post_download_links,\
         avatar_photo_url,avatar_photo_image_urls,earnings_visibility,\
         is_nsfw,is_monthly,name,url\
         &fields[post]=change_visibility_at,comment_count,commenter_count,\
         content,content_json_string,current_user_can_comment,\
         current_user_can_delete,current_user_can_view,\
         current_user_has_liked,embed,image,insights_last_updated_at,\
         is_paid,like_count,meta_image_url,min_cents_pledged_to_view,\
         post_file,post_metadata,published_at,patreon_url,post_type,\
         pledge_url,preview_asset_type,thumbnail,thumbnail_url,\
         teaser_text,title,upgrade_url,url,was_posted_by_campaign_owner,\
         has_ti_violation,moderation_status,\
         post_level_suspension_removal_date,pls_one_liners_by_category,\
         video_preview,view_count\
         &fields[post_tag]=tag_type,value\
         &fields[user]=image_url,full_name,url\
         &fields[access_rule]=access_rule_type,amount_cents\
         &fields[media]=id,image_urls,download_url,metadata,file_name\
         &fields[native_video_insights]=average_view_duration,\
         average_view_pct,has_preview,id,last_updated_at,num_views,\
         preview_views,video_duration\
         &page[cursor]={cursor}{query}{order}\
         &json-api-version=1.0"
    )
}

/// Upstream `_pagination`: JSON:API pages linked by `links.next`; pages
/// without `included` contribute no posts, as upstream.
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
            let page = ctx.json(&url, true).await?;
            let included = Arc::new(transform(page.get("included")));
            let posts = match page.get("included") {
                Some(_) => page
                    .get("data")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default(),
                None => Vec::new(),
            };
            let next = page
                .get("links")
                .and_then(|links| links.get("next"))
                .and_then(Value::as_str)
                .filter(|next| !next.is_empty())
                .map(str::to_owned);
            let events = stream::iter(posts)
                .map(move |post| {
                    let (ctx, extra, included) = (ctx.clone(), extra.clone(), included.clone());
                    crate::sites::once(async move {
                        let post = process(&ctx, post, &included).await?;
                        post_events(ctx, route, post, (*extra).clone(), false).await
                    })
                })
                .flatten()
                .boxed();
            Ok(Some((events, next)))
        }
    })
}

/// `included` objects by type and ID (upstream `_transform`).
type Included = HashMap<String, Map<String, Value>>;

fn transform(included: Option<&Value>) -> Included {
    let mut result: Included = HashMap::new();
    for item in included.and_then(Value::as_array).into_iter().flatten() {
        let kind = str_of(item, "type").to_owned();
        let id = item.get("id").map(crate::sites::scalar).unwrap_or_default();
        let attributes = item.get("attributes").cloned().unwrap_or(Value::Null);
        result.entry(kind).or_default().insert(id, attributes);
    }
    result
}

fn lookup<'i>(included: &'i Included, reference: &Value) -> Option<&'i Value> {
    let id = reference.get("id").map(crate::sites::scalar)?;
    included.get(str_of(reference, "type"))?.get(&id)
}

/// Upstream `_process`: the post's attributes with its files, campaign,
/// tags, creator and HTML content resolved.
async fn process(ctx: &Ctx<'_>, post: Value, included: &Included) -> Result<Map<String, Value>> {
    let mut attr = post
        .get("attributes")
        .and_then(Value::as_object)
        .cloned()
        .ok_or_else(|| error("Patreon returned an invalid post"))?;
    let id = post.get("id").map(crate::sites::scalar).unwrap_or_default();
    attr.insert("id".into(), super::parse_int(&id, 0).into());
    let relationships = post.get("relationships").cloned().unwrap_or(Value::Null);
    for key in ["images", "attachments", "attachments_media"] {
        let files: Vec<Value> = relationships
            .get(key)
            .and_then(|files| files.get("data"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|file| lookup(included, file).cloned())
            .collect();
        attr.insert(key.into(), files.into());
    }
    attr.insert("date".into(), iso_date(attr.get("published_at")));
    let campaign = relationships
        .get("campaign")
        .and_then(|c| c.get("data"))
        .and_then(|data| {
            let id = data.get("id").map(crate::sites::scalar)?;
            included.get("campaign")?.get(&id).cloned()
        });
    attr.insert("campaign".into(), campaign.unwrap_or(Value::Null));
    let tags: Vec<Value> = relationships
        .get("user_defined_tags")
        .and_then(|tags| tags.get("data"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|tag| str_of(tag, "type") == "post_tag")
        .map(|tag| str_of(tag, "id").replace("user_defined;", "").into())
        .collect();
    attr.insert("tags".into(), tags.into());
    let user = relationships.get("user").cloned().unwrap_or(Value::Null);
    let related = user
        .get("links")
        .and_then(|links| links.get("related"))
        .and_then(Value::as_str);
    let mut creator = match related {
        Some(url) => ctx.user(url).await,
        None => None,
    };
    if !truthy(creator.as_ref()) {
        creator = user.get("data").and_then(|data| {
            let id = data.get("id").map(crate::sites::scalar)?;
            included.get("user")?.get(&id).cloned()
        });
    }
    attr.insert("creator".into(), creator.unwrap_or(Value::Null));
    if !truthy(attr.get("content"))
        && let Some(markup) = attr.shift_remove("content_json_string")
        && truthy(Some(&markup))
        && let Some(html) = markup.as_str().and_then(tiptap)
    {
        attr.insert("content".into(), html.into());
    }
    Ok(attr)
}

/// One file candidate: its kind, object, URL and name source.
type Candidate = (&'static str, Value, String, String);

/// Upstream `items` for one post: its files from each `files` source,
/// skipping repeated MD5 hashes. A post the session may not view has no
/// files; for the single-post class that is an error.
async fn post_events<'a>(
    ctx: Arc<Ctx<'a>>,
    route: &'static Route,
    mut post: Map<String, Value>,
    extra: Map<String, Value>,
    single: bool,
) -> Result<Events<'a>> {
    super::update(&mut post, &extra);
    let id = post.get("id").map(crate::sites::scalar).unwrap_or_default();
    if post
        .get("current_user_can_view")
        .is_some_and(|view| !truthy(Some(view)))
    {
        if !single {
            return Ok(crate::sites::events(Vec::new()));
        }
        return Err(error(if ctx.logged_in {
            format!("Patreon post {id} is not viewable with this session's pledges")
        } else {
            format!(
                "Patreon post {id} is for patrons only; pass a 'session_id' cookie for \
                 .patreon.com (--cookies)"
            )
        }));
    }
    let mut candidates: Vec<Candidate> = Vec::new();
    for kind in &ctx.kinds {
        candidates.extend(match kind {
            Kind::Images => images(&ctx, &post).await,
            Kind::ImageLarge => image_large(&ctx, &post).await,
            Kind::Attachments => attachments(&ctx, &post).await,
            Kind::Postfile => postfile(&ctx, &post).await,
            Kind::Content => content(&ctx, &post).await,
        });
    }
    let timestamp = post
        .get("published_at")
        .and_then(Value::as_str)
        .and_then(parse_iso);
    let user = post
        .get("creator")
        .and_then(|c| c.get("full_name"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let title = post.get("title").and_then(Value::as_str).map(str::to_owned);
    let mut num = 0u64;
    let mut hashes = HashSet::new();
    let mut items = Vec::new();
    for (kind, file, url, name) in candidates {
        let hash = filehash(&url);
        if !hash.is_empty() && !hashes.insert(hash.clone()) {
            continue;
        }
        num += 1;
        let mut data = post.clone();
        data.insert("hash".into(), hash.into());
        data.insert("type".into(), kind.into());
        data.insert("file".into(), file);
        data.insert("num".into(), num.into());
        super::nameext_into(&name, &mut data);
        let item = if super::ext_from_url(&url) == "m3u8" {
            data.insert("extension".into(), "mp4".into());
            crate::sites::media::web_url(&url).and_then(|base_url| {
                let url = Url::parse(&format!("ytdl:{base_url}")).ok()?;
                Some(
                    Item::new(url, data)
                        .job(MediaJob::Manifest {
                            protocol: ManifestProtocol::Hls,
                            base_url,
                            inline_data: None,
                        })
                        .header("Referer", &format!("{ROOT}/")),
                )
            })
        } else {
            crate::sites::media::web_url(&url).map(|url| Item::new(url, data))
        };
        if let Some(item) = item {
            items.push(
                item.ids(id.clone(), num.to_string())
                    .user(user.clone())
                    .caption(title.clone())
                    .timestamp(timestamp),
            );
        }
    }
    Ok(crate::sites::events(crate::sites::post(
        &SITE, ctx.http, route, items,
    )))
}

/// Upstream `_filehash`: the last 32-character path segment.
fn filehash(url: &str) -> String {
    url.split('?')
        .next()
        .unwrap_or_default()
        .rsplit('/')
        .find(|part| part.len() == 32)
        .unwrap_or_default()
        .to_owned()
}

/// A name from the object, else the URL's `Content-Disposition`, else the
/// URL itself.
async fn name_of(ctx: &Ctx<'_>, file: &Value, key: &str, url: &str) -> String {
    match file.get(key).and_then(Value::as_str) {
        Some(name) if !name.is_empty() => name.to_owned(),
        _ => match ctx.filename(url).await {
            name if name.is_empty() => url.to_owned(),
            name => name,
        },
    }
}

async fn postfile(ctx: &Ctx<'_>, post: &Map<String, Value>) -> Vec<Candidate> {
    let Some(file) = post.get("post_file").filter(|f| truthy(Some(f))) else {
        return Vec::new();
    };
    let url = str_of(file, "url").to_owned();
    let name = match file.get("name").and_then(Value::as_str) {
        Some(name) if !name.is_empty() => name.to_owned(),
        _ if url.starts_with("https://stream.mux.com/") => url.clone(),
        _ => name_of(ctx, &Value::Null, "", &url).await,
    };
    vec![("postfile", file.clone(), url, name)]
}

async fn images(ctx: &Ctx<'_>, post: &Map<String, Value>) -> Vec<Candidate> {
    let mut out = Vec::new();
    for image in post
        .get("images")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let formatted = ctx.images_fmt.as_ref().and_then(|fmt| {
            image
                .get("image_urls")
                .and_then(|urls| urls.get(fmt.as_str()))
                .map(|url| url.as_str().unwrap_or_default())
        });
        let url = formatted.unwrap_or_else(|| str_of(image, "download_url"));
        if url.is_empty() {
            continue;
        }
        let name = name_of(ctx, image, "file_name", url).await;
        out.push(("image", image.clone(), url.to_owned(), name));
    }
    out
}

async fn image_large(ctx: &Ctx<'_>, post: &Map<String, Value>) -> Vec<Candidate> {
    let Some(image) = post.get("image").filter(|i| truthy(Some(i))) else {
        return Vec::new();
    };
    let url = str_of(image, "large_url");
    if url.is_empty() {
        return Vec::new();
    }
    let name = name_of(ctx, image, "file_name", url).await;
    vec![("image_large", image.clone(), url.to_owned(), name)]
}

async fn attachments(ctx: &Ctx<'_>, post: &Map<String, Value>) -> Vec<Candidate> {
    let mut out = Vec::new();
    for attachment in post
        .get("attachments")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(url) = ctx.location(str_of(attachment, "url")).await {
            let name = attachment
                .get("name")
                .map(crate::sites::scalar)
                .unwrap_or_default();
            out.push(("attachment", attachment.clone(), url, name));
        }
    }
    for attachment in post
        .get("attachments_media")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let url = str_of(attachment, "download_url");
        if !url.is_empty() {
            let name = attachment
                .get("file_name")
                .map(crate::sites::scalar)
                .unwrap_or_default();
            out.push(("attachment", attachment.clone(), url.to_owned(), name));
        }
    }
    out
}

async fn content(ctx: &Ctx<'_>, post: &Map<String, Value>) -> Vec<Candidate> {
    let html = post
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut out = Vec::new();
    for img in extract_iter(html, "><figure><img src=\"", ">") {
        // Python `img[:img.find('"')]`, where a miss drops the last character.
        let raw = match img.find('"') {
            Some(end) => &img[..end],
            None => {
                let mut chars = img.chars();
                chars.next_back();
                chars.as_str()
            }
        };
        let url = crate::sites::unescape(raw);
        let mut data = Map::new();
        data.insert("media_id".into(), extr(img, "media_id=\"", "\"").into());
        let name = name_of(ctx, &Value::Null, "", &url).await;
        out.push(("content", Value::Object(data), url, name));
    }
    out
}

/// Upstream `_extract_bootstrap`: the page's bootstrap object from Next.js
/// data or one of the older inline scripts. Current pages add a `nonce` to
/// the `__NEXT_DATA__` script, which upstream's exact match misses, and post
/// pages stream the envelope as React Server Component chunks instead.
fn bootstrap(page: &str) -> Result<Value> {
    let envelope = next_data(page)
        .and_then(|data| {
            data.get("props")?
                .get("pageProps")?
                .get("bootstrapEnvelope")
                .cloned()
        })
        .or_else(|| flight_envelope(page));
    if let Some(env) = envelope {
        match env.get("pageBootstrap") {
            Some(boot) if truthy(Some(boot)) => return Ok(boot.clone()),
            _ => {
                if let Some(boot) = env.get("bootstrap") {
                    return Ok(boot.clone());
                }
            }
        }
    }
    for begin in [
        "window.patreon = {\"bootstrap\":",
        "window.patreon = wrapInProxy({\"bootstrap\":",
    ] {
        let data = extr(page, begin, "},\"apiServer\"");
        if !data.is_empty() {
            return super::json_loads(&format!("{data}}}"), "Patreon");
        }
    }
    let data = extr(page, "window.patreon.bootstrap,", "});");
    if !data.is_empty() {
        return super::json_loads(&format!("{data}}}"), "Patreon");
    }
    let data = extr(page, "window.patreon = {", "};\n");
    if !data.is_empty()
        && let Ok(value) = serde_json::from_str::<Value>(&format!("{{{data}}}"))
        && let Some(boot) = value.get("bootstrap")
    {
        return Ok(boot.clone());
    }
    Err(error("Unable to extract Patreon bootstrap data"))
}

/// The `__NEXT_DATA__` JSON, whatever attributes its script tag carries.
fn next_data(page: &str) -> Option<Value> {
    let start = page.find(" id=\"__NEXT_DATA__\"")?;
    let rest = &page[start..];
    let open = rest.find('>')? + 1;
    let end = rest[open..].find("</script>")? + open;
    serde_json::from_str(&rest[open..end]).ok()
}

/// The `bootstrapEnvelope` object in a page's `self.__next_f.push([1,"…"])`
/// string chunks, decoded and joined.
fn flight_envelope(page: &str) -> Option<Value> {
    let text: String = extract_iter(page, "self.__next_f.push([1,", "])</script>")
        .into_iter()
        .filter_map(|chunk| match serde_json::from_str(chunk) {
            Ok(Value::String(text)) => Some(text),
            _ => None,
        })
        .collect();
    let key = "\"bootstrapEnvelope\":";
    let start = text.find(key)? + key.len();
    serde_json::Deserializer::from_str(&text[start..])
        .into_iter::<Value>()
        .next()?
        .ok()
}

/// gallery_dl/extractor/utils/patreon_tiptap.py `to_html`: TipTap JSON
/// content as HTML; `None` when it cannot be parsed.
fn tiptap(markup: &str) -> Option<String> {
    let data: Value = serde_json::from_str(markup).ok()?;
    let mut html = String::new();
    for block in data.get("content")?.as_array()? {
        tiptap_content(&mut html, block);
    }
    Some(html)
}

fn tiptap_children(html: &mut String, content: &Value) {
    for block in content
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        tiptap_content(html, block);
    }
}

fn tiptap_content(html: &mut String, content: &Value) {
    let attrs = content.get("attrs").filter(|a| truthy(Some(a)));
    let attr = |key: &str| {
        attrs
            .and_then(|a| a.get(key))
            .filter(|v| truthy(Some(v)))
            .map(crate::sites::scalar)
    };
    match str_of(content, "type") {
        "paragraph" => {
            html.push_str("<p>");
            tiptap_children(html, content);
            html.push_str("</p>");
        }
        "text" => tiptap_text(html, content),
        "heading" => {
            let level = attr("level").unwrap_or_else(|| "3".into());
            html.push_str(&format!("<h{level}>"));
            tiptap_children(html, content);
            html.push_str(&format!("</h{level}>"));
        }
        kind @ ("listItem" | "bulletList" | "orderedList" | "blockquote") => {
            let tag = match kind {
                "listItem" => "li",
                "bulletList" => "ul",
                "orderedList" => "ol",
                _ => "blockquote",
            };
            html.push_str(&format!("<{tag}>"));
            tiptap_children(html, content);
            html.push_str(&format!("</{tag}>"));
        }
        "image" => {
            if let Some(src) = attr("src") {
                html.push_str("<div data-image-container=\"true\"");
                if let Some(align) = attr("alignment") {
                    html.push_str(&format!(" data-alignment=\"{}\"", escape(&align)));
                }
                html.push_str(&format!("><figure><img src=\"{}\"", escape(&src)));
                if let Some(id) = attr("media_id") {
                    html.push_str(&format!(" data-media-id=\"{}\"", escape(&id)));
                }
                html.push_str("/></figure></div>");
            }
        }
        "link" => {
            if let Some(href) = attr("href") {
                html.push_str(&format!("<a href=\"{}\">", escape(&href)));
                tiptap_children(html, content);
                html.push_str("</a>");
            }
        }
        "hardBreak" => html.push_str("<br/>"),
        "horizontalRule" => html.push_str("<hr/>"),
        // Upstream logs unsupported types and skips them.
        _ => {}
    }
}

fn tiptap_text(html: &mut String, content: &Value) {
    let mut close = Vec::new();
    for mark in content
        .get("marks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match str_of(mark, "type") {
            "link" => {
                let attrs = mark.get("attrs").cloned().unwrap_or(Value::Null);
                html.push_str("<a href=\"");
                html.push_str(&escape(str_of(&attrs, "href")));
                html.push('"');
                for key in ["target", "rel"] {
                    if let Some(value) = attrs.get(key) {
                        html.push_str(&format!(" {key}=\"{}\"", crate::sites::scalar(value)));
                    }
                }
                html.push('>');
                close.push("</a>");
            }
            "bold" => {
                html.push_str("<strong>");
                close.push("</strong>");
            }
            "italic" => {
                html.push_str("<em>");
                close.push("</em>");
            }
            "underline" => {
                html.push_str("<u>");
                close.push("</u>");
            }
            "strike" => {
                html.push_str("<s>");
                close.push("</s>");
            }
            _ => {}
        }
    }
    html.push_str(&escape(str_of(content, "text")));
    for tag in close.into_iter().rev() {
        html.push_str(tag);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http as client, json, keywords, media, response, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        let creator = |name: Option<&str>, query: &[(&str, &str)]| {
            Some(Target::Creator(
                name.map(str::to_owned),
                query
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                    .collect(),
            ))
        };
        assert_eq!(
            t("https://www.patreon.com/koveliana"),
            creator(Some("koveliana"), &[])
        );
        assert_eq!(
            t("https://www.patreon.com/koveliana/posts?filters[month]=2020-3"),
            creator(Some("koveliana"), &[("filters[month]", "2020-3")])
        );
        assert_eq!(
            t("https://www.patreon.com/cw/anythingelse"),
            creator(Some("anythingelse"), &[])
        );
        assert_eq!(
            t("https://patreon.com/c/koveliana/"),
            creator(Some("koveliana"), &[])
        );
        assert_eq!(t("https://www.patreon.com/c"), creator(Some("c"), &[]));
        assert_eq!(
            t("https://www.patreon.com/user/posts/?u=2931440"),
            creator(Some("user"), &[("u", "2931440")])
        );
        assert_eq!(
            t("https://www.patreon.com/profile/creators?c=369707"),
            creator(None, &[("c", "369707")])
        );
        assert_eq!(
            t("https://www.patreon.com/id:369707"),
            creator(Some("id:369707"), &[])
        );
        // A query after other paths is not part of the match.
        assert_eq!(
            t("https://www.patreon.com/x/about?u=1"),
            creator(Some("x"), &[])
        );
        assert_eq!(t("https://www.patreon.com/home"), Some(Target::User));
        assert_eq!(
            t("https://www.patreon.com/posts/precious-metal-23563293"),
            Some(Target::Post("precious-metal-23563293".into()))
        );
        assert_eq!(
            t("https://www.patreon.com/collection/15764"),
            Some(Target::Collection("15764".into()))
        );
        for raw in [
            "https://www.patreon.com/",
            "https://www.patreon.com/create",
            "https://www.patreon.com/login",
            "https://www.patreon.com/search?q=foobar",
            "https://www.patreon.com/messages/?mode=user&tab=chats",
            "https://www.patreon.com/home?x=1",
            "https://www.patreon.com/posts",
            "https://www.patreon.com/posts/",
            "https://www.patreon.com/collection/x",
            "https://patreon.com.evil.com/user",
            "https://evilpatreon.com/user",
            "https://c10.patreon.com/user",
            "https://www.patreon.com:8443/user",
            "https://u:p@www.patreon.com/user",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn post_page() -> String {
        let data = json!({"props": {"pageProps": {"bootstrapEnvelope": {"pageBootstrap": {
        "post": {
            "data": post_data("23563293", true),
            "included": included(),
        }}}}}});
        format!("<script id=\"__NEXT_DATA__\" type=\"application/json\">{data}</script>")
    }

    fn post_data(id: &str, viewable: bool) -> Value {
        json!({"id": id, "type": "post", "attributes": {
            "title": "Precious Metal", "published_at": "2018-12-21T21:56:39.000+00:00",
            "current_user_can_view": viewable, "content": null,
            "content_json_string": "{\"type\":\"doc\",\"content\":[{\"type\":\"paragraph\",\"content\":[{\"type\":\"text\",\"text\":\"+3 \",\"marks\":[{\"type\":\"bold\"}]},{\"type\":\"text\",\"text\":\"<outfits>\"}]},{\"type\":\"image\",\"attrs\":{\"src\":\"https://c10.patreonusercontent.com/4/p/post/1/0123456789abcdef0123456789abcdef/1.png?a=1&b=2\",\"media_id\":5}}]}",
            "post_file": {"name": "outfits.zip", "url": "https://c10.patreonusercontent.com/4/p/post/1/ffffffffffffffffffffffffffffffff/1.zip?t=1"},
            "image": {"large_url": "https://c10.patreonusercontent.com/4/p/post/1/0123456789abcdef0123456789abcdef/2.png"}
        }, "relationships": {
            "campaign": {"data": {"id": "77", "type": "campaign"}},
            "user": {"data": {"id": "9", "type": "user"},
                     "links": {"related": "https://www.patreon.com/api/user/9"}},
            "user_defined_tags": {"data": [
                {"id": "user_defined;Tag A", "type": "post_tag"},
                {"id": "other", "type": "other"}]},
            "images": {"data": [{"id": "m1", "type": "media"}]},
            "attachments_media": {"data": [{"id": "m2", "type": "media"}]}
        }})
    }

    fn included() -> Value {
        json!([
            {"id": "77", "type": "campaign", "attributes": {"name": "Camp"}},
            {"id": "9", "type": "user", "attributes": {"full_name": "Included Name"}},
            {"id": "m1", "type": "media", "attributes": {"id": 1, "file_name": "first.jpg",
                "download_url": "https://c10.patreonusercontent.com/4/p/post/1/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/1.jpg",
                "image_urls": {"thumbnail": "https://c10.patreonusercontent.com/4/p/post/1/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb/t.jpg"}}},
            {"id": "m2", "type": "media", "attributes": {"id": 2, "file_name": "clip.mp4",
                "download_url": "https://stream.mux.com/abc.m3u8?token=x"}}
        ])
    }

    fn user() -> Value {
        json!({"data": {"id": "9", "type": "user", "attributes": {
            "full_name": "Koveliana", "created": "2018-10-17T05:45:19.000+00:00"}}})
    }

    #[tokio::test]
    async fn extracts_post() {
        let disposition = response(
            "200 OK\r\nContent-Disposition: attachment; filename=\"large.png\"",
            "image/png",
            b"",
        );
        let head = response("200 OK", "image/png", b"");
        let (root, server) = serve(vec![html(&post_page()), json(user()), disposition, head]);
        let http = client("Patreon", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.patreon.com/posts/precious-metal-23563293").unwrap(),
        ))
        .await;
        // images, image_large; the content image repeats image_large's hash.
        let kinds: Vec<_> = files.iter().map(|f| keywords(f)["type"].clone()).collect();
        assert_eq!(kinds, ["image", "image_large", "attachment", "postfile"]);
        let k = keywords(&files[0]);
        assert_eq!(k["id"], 23563293);
        assert_eq!(k["num"], 1);
        assert_eq!(k["filename"], "first");
        assert_eq!(k["hash"], "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert_eq!(k["creator"]["full_name"], "Koveliana");
        assert_eq!(k["creator"]["date"], "2018-10-17 05:45:19");
        assert_eq!(k["campaign"]["name"], "Camp");
        assert_eq!(k["tags"], json!(["Tag A"]));
        assert_eq!(k["date"], "2018-12-21 21:56:39");
        assert_eq!(
            k["content"],
            "<p><strong>+3 </strong>&lt;outfits&gt;</p><div data-image-container=\"true\">\
             <figure><img src=\"https://c10.patreonusercontent.com/4/p/post/1/0123456789abcdef0123456789abcdef/1.png?a=1&amp;b=2\" data-media-id=\"5\"/></figure></div>"
        );
        assert_eq!(k.get("content_json_string"), None);
        assert_eq!(keywords(&files[1])["filename"], "large");
        let video = &files[2];
        assert_eq!(
            video.url.as_str(),
            "ytdl:https://stream.mux.com/abc.m3u8?token=x"
        );
        assert_eq!(keywords(video)["extension"], "mp4");
        assert!(matches!(
            video.job,
            MediaJob::Manifest {
                protocol: ManifestProtocol::Hls,
                ..
            }
        ));
        assert_eq!(
            video.headers.get("Referer").map(String::as_str),
            Some("https://www.patreon.com/")
        );
        let k = keywords(&files[3]);
        assert_eq!(
            (k["filename"].clone(), k["num"].clone()),
            (json!("outfits"), json!(4))
        );
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            file.render(&files[0]).unwrap(),
            "23563293_Precious Metal_01.jpg"
        );
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("patreon/Koveliana")
        );
        let (_, identity) = crate::sites::archive_key(&files[3]).unwrap();
        assert_eq!(identity, r#"["{id}_{num}",["23563293","4"]]"#);
        let requests = server.join().unwrap();
        let first = requests[0].to_ascii_lowercase();
        assert!(first.starts_with("get /posts/precious-metal-23563293 "));
        assert!(first.contains("user-agent: patreon/14.2.1"));
        assert!(requests[1].starts_with("GET /api/user/9 "));
        assert!(requests[1].contains(AGENT));
        assert!(
            requests[2].starts_with("HEAD /4/p/post/1/0123456789abcdef0123456789abcdef/2.png ")
        );
        // Upstream names content images before dropping repeated hashes.
        assert!(
            requests[3]
                .starts_with("HEAD /4/p/post/1/0123456789abcdef0123456789abcdef/1.png?a=1&b=2 ")
        );
    }

    #[tokio::test]
    async fn files_and_format_images_options() {
        let (root, server) = serve(vec![html(&post_page()), json(user())]);
        let options = serde_json::from_value(
            json!({"files": "images,postfile", "format-images": "thumbnail"}),
        )
        .unwrap();
        let http = client("Patreon", root.clone()).with_options(Some(Arc::new(options)));
        http.set_cookie(&root, "session_id=sess; Path=/");
        let files = media(extract(
            &http,
            Url::parse("https://www.patreon.com/posts/23563293").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert!(files[0].url.as_str().ends_with("/t.jpg"));
        assert_eq!(keywords(&files[1])["type"], "postfile");
        let requests = server.join().unwrap();
        // Logged in: the regular app User-Agent and the session cookie.
        assert!(requests[0].contains(AGENT));
        assert!(requests[0].contains("cookie: session_id=sess"));
    }

    #[tokio::test]
    async fn unviewable_posts_need_a_session() {
        let data = json!({"props": {"pageProps": {"bootstrapEnvelope": {"bootstrap": {
            "post": {"data": post_data("5", false), "included": included()}}}}}});
        let page =
            format!("<script id=\"__NEXT_DATA__\" type=\"application/json\">{data}</script>");
        let (root, server) = serve(vec![html(&page), json(user())]);
        let http = client("Patreon", root);
        let results: Vec<_> = crate::event::media_only(extract(
            &http,
            Url::parse("https://www.patreon.com/posts/5").unwrap(),
        ))
        .collect()
        .await;
        let message = results[0].as_ref().err().unwrap().to_string();
        assert!(message.contains("session_id"), "{message}");
        server.join().unwrap();
    }

    fn api_page(id: &str, next: Option<&str>) -> Value {
        let mut post = post_data(id, true);
        post["attributes"]["content_json_string"] = Value::Null;
        post["attributes"]["post_file"] = Value::Null;
        post["attributes"]["image"] = Value::Null;
        post["relationships"]["attachments_media"] = json!({"data": []});
        json!({"data": [post], "included": included(), "links": {"next": next}})
    }

    #[tokio::test]
    async fn creator_pages_through_campaign_posts() {
        let creator_page = "<script>window.patreon = {\"bootstrap\": {\"campaign\": \
            {\"data\": {\"id\": \"369707\"}}},\"apiServer\": \"x\"};</script>";
        let next = "https://www.patreon.com/api/posts?page%5Bcursor%5D=abc";
        let (root, server) = serve(vec![
            html(creator_page),
            json(api_page("1", Some(next))),
            json(user()),
            json(api_page("2", None)),
        ]);
        let options = serde_json::from_value(json!({"order-posts": "asc"})).unwrap();
        let http = client("Patreon", root).with_options(Some(Arc::new(options)));
        let files = media(extract(
            &http,
            Url::parse("https://www.patreon.com/koveliana/posts?filters[month]=2020-3&x=<")
                .unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(keywords(&files[1])["id"], 2);
        assert_eq!(keywords(&files[1])["subcategory"], "creator");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /koveliana "));
        let api = &requests[1];
        assert!(api.starts_with("GET /api/posts?include=campaign,"), "{api}");
        assert!(api.contains("&page[cursor]=&filter[campaign_id]=369707"));
        assert!(api.contains("&filter[month]=2020-3&sort=published_at&json-api-version=1.0 "));
        assert!(api.contains("content-type: application/vnd.api+json"));
        assert!(requests[3].starts_with("GET /api/posts?page%5Bcursor%5D=abc "));
    }

    #[tokio::test]
    async fn campaign_ids_from_options_and_next_js() {
        let (root, server) = serve(vec![json(api_page("1", None)), json(user())]);
        let http = client("Patreon", root);
        media(extract(
            &http,
            Url::parse("https://www.patreon.com/id:42").unwrap(),
        ))
        .await;
        let requests = server.join().unwrap();
        assert!(requests[0].contains("filter[campaign_id]=42&"));
        assert!(requests[0].contains("&sort=-published_at&"));

        let page = r#"self.__next_f.push([1,"{\"value\":{\"campaign\":{\"data\":{\"id\":\"555\",\"type\""])"#;
        let (root, server) = serve(vec![html(page), json(api_page("1", None)), json(user())]);
        let http = client("Patreon", root);
        media(extract(
            &http,
            Url::parse("https://www.patreon.com/profile/creators?u=31").unwrap(),
        ))
        .await;
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /user?u=31 "));
        assert!(requests[1].contains("filter[campaign_id]=555&"));
    }

    #[tokio::test]
    async fn collection_and_home_feed() {
        let collection = json!({"data": {"id": "15764", "type": "collection", "attributes": {
            "title": "JoJo", "created_at": "2023-08-31T14:10:41.000+00:00",
            "thumbnail": {"url": "https://c10.patreonusercontent.com/4/patreon-media/p/campaign/3300/abc/x.jpg"}}}});
        let (root, server) = serve(vec![
            json(collection),
            json(api_page("1", None)),
            json(user()),
        ]);
        let http = client("Patreon", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.patreon.com/collection/15764").unwrap(),
        ))
        .await;
        let k = keywords(&files[0]);
        assert_eq!(k["collection"]["title"], "JoJo");
        assert_eq!(k["collection"]["id"], "15764");
        assert_eq!(k["collection"]["date"], "2023-08-31 14:10:41");
        let (_, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("patreon/Koveliana/Collections/JoJo (15764)")
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/collection/15764 "));
        assert!(requests[1].contains("&filter[campaign_id]=3300&"));
        assert!(requests[1].contains(
            "&filter[collection_id]=15764&filter[include_drops]=true&sort=collection_order&"
        ));

        let (root, server) = serve(vec![json(api_page("1", None)), json(user())]);
        let options = serde_json::from_value(json!({"date-max": "2024-01-02T03:04:05Z"})).unwrap();
        let http = client("Patreon", root.clone()).with_options(Some(Arc::new(options)));
        http.set_cookie(&root, "session_id=sess; Path=/");
        let files = media(extract(
            &http,
            Url::parse("https://www.patreon.com/home").unwrap(),
        ))
        .await;
        assert_eq!(keywords(&files[0])["subcategory"], "user");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/stream?include="));
        assert!(requests[0].contains(
            "&page[cursor]=2024-01-02T03:04:05&filter[is_following]=true\
             &json-api-use-default-includes=false&json-api-version=1.0 "
        ));
    }

    #[tokio::test]
    async fn attachments_resolve_their_redirect() {
        let mut page = api_page("1", None);
        page["data"][0]["attributes"]["attachments"] = Value::Null;
        page["data"][0]["relationships"]["attachments"] =
            json!({"data": [{"id": "a1", "type": "attachment"}]});
        page["included"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id": "a1", "type": "attachment", "attributes": {
                "name": "pack.zip", "url": "https://www.patreon.com/file?h=1&i=2"}}));
        let moved = b"HTTP/1.1 302 Found\r\nLocation: https://c10.patreonusercontent.com/4/f/cccccccccccccccccccccccccccccccc/pack.zip\r\n\
            Content-Length: 0\r\nConnection: close\r\n\r\n"
            .to_vec();
        let (root, server) = serve(vec![json(page), json(user()), moved]);
        let options = serde_json::from_value(json!({"files": ["attachments"]})).unwrap();
        let http = client("Patreon", root.clone()).with_options(Some(Arc::new(options)));
        http.set_cookie(&root, "session_id=sess; Path=/");
        let files = media(extract(
            &http,
            Url::parse("https://www.patreon.com/id:1").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let k = keywords(&files[0]);
        assert_eq!(
            (k["filename"].clone(), k["extension"].clone()),
            (json!("pack"), json!("zip"))
        );
        assert_eq!(k["file"]["name"], "pack.zip");
        let requests = server.join().unwrap();
        assert!(requests[2].starts_with("GET /file?h=1&i=2 "));
        assert!(requests[2].contains("cookie: session_id=sess"));
    }

    #[tokio::test]
    async fn refused_listings_name_the_cookie() {
        async fn message(events: Events<'_>) -> String {
            let results: Vec<_> = crate::event::media_only(events).collect().await;
            results[0].as_ref().err().unwrap().to_string()
        }
        let refused = response("401 Unauthorized", "application/json", b"{}");
        let (root, server) = serve(vec![refused]);
        let http = client("Patreon", root);
        let text = message(extract(
            &http,
            Url::parse("https://www.patreon.com/id:1").unwrap(),
        ))
        .await;
        assert!(text.contains("session_id"), "{text}");
        server.join().unwrap();
        // The home feed is not requested without a session.
        let http = client("Patreon", Url::parse("http://127.0.0.1:9/").unwrap());
        let text = message(extract(
            &http,
            Url::parse("https://www.patreon.com/home").unwrap(),
        ))
        .await;
        assert!(text.contains("session_id"), "{text}");
    }

    #[test]
    fn helpers() {
        assert_eq!(
            filehash(
                "https://x/4/p/0123456789abcdef0123456789abcdef/eyJ/1.png?h=ffffffffffffffffffffffffffffffff"
            ),
            "0123456789abcdef0123456789abcdef"
        );
        assert_eq!(filehash("https://x/a/b.png"), "");
        assert_eq!(
            escape("<a href='x'>&\"</a>"),
            "&lt;a href=&#x27;x&#x27;&gt;&amp;&quot;&lt;/a&gt;"
        );
        assert_eq!(isoformat(1704164645), "2024-01-02T03:04:05");
        let markup = json!({"content": [
            {"type": "heading", "attrs": {"level": 2}, "content": [{"type": "text", "text": "H"}]},
            {"type": "bulletList", "content": [{"type": "listItem", "content": [
                {"type": "paragraph"}]}]},
            {"type": "paragraph", "content": [
                {"type": "text", "text": "l", "marks": [
                    {"type": "link", "attrs": {"href": "https://a/?b&c", "target": "_blank"}},
                    {"type": "strike"}]},
                {"type": "hardBreak"}]},
            {"type": "horizontalRule"}, {"type": "unknown"}]});
        assert_eq!(
            tiptap(&markup.to_string()).unwrap(),
            "<h2>H</h2><ul><li><p></p></li></ul>\
             <p><a href=\"https://a/?b&amp;c\" target=\"_blank\"><s>l</s></a><br/></p><hr/>"
        );
        assert_eq!(tiptap("not json"), None);
        let old =
            "<script>window.patreon = wrapInProxy({\"bootstrap\": {\"post\": 1},\"apiServer\"";
        assert_eq!(bootstrap(old).unwrap(), json!({"post": 1}));
        assert!(bootstrap("<html></html>").is_err());
        let nonce = r#"<script id="__NEXT_DATA__" type="application/json" nonce="n">{"props":
            {"pageProps":{"bootstrapEnvelope":{"pageBootstrap":{"campaign":1}}}}}</script>"#;
        assert_eq!(bootstrap(nonce).unwrap(), json!({"campaign": 1}));
        // Server Component chunks split the envelope anywhere.
        let chunk = |text: &str| {
            format!(
                "<script>self.__next_f.push([1,{}])</script>",
                Value::from(text)
            )
        };
        let flight = chunk("60:[\"$\",\"$L81\",null,{\"bootstrapEnvelope\":{\"pageBoot")
            + &chunk("strap\":{\"post\":{\"data\":2}}}}]\n");
        assert_eq!(bootstrap(&flight).unwrap(), json!({"post": {"data": 2}}));
    }
}
