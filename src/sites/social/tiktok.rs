//! TikTok, mirroring gallery_dl/extractor/tiktok.py.
//!
//! Post pages are read from their `__UNIVERSAL_DATA_FOR_REHYDRATION__`
//! JSON (with the older `SIGI_STATE` layout as a fallback), solving the
//! site's proof-of-work JavaScript challenge like upstream when a page
//! comes back without it. Profile listings use the same web API endpoints
//! and cursors as upstream (`creator/item_list`, `post/item_list`,
//! `repost/item_list`, `favorite/item_list`, `user/collect/item_list`,
//! `story/item_list`, `story/user_list`, `story/batch/item_list`), then
//! every listed post is read from its own page, as upstream does.
//!
//! Differences from upstream:
//! - Upstream can hand videos (`videos: "ytdl"`), audio (`audio: "ytdl"`)
//!   and profile listings (`ytdl: true`) to yt-dlp. There is no yt-dlp
//!   here: `"ytdl"` behaves like `true`, so the direct video URL (best
//!   `bitrateInfo` quality, else `playAddr`) or music `playUrl` is emitted,
//!   and the `ytdl`/`module` options are ignored.
//! - Media carry `Referer: https://www.tiktok.com/` and the page's
//!   User-Agent. Videos additionally carry the anonymous `tt_chain_token`
//!   cookie the page set, which TikTok's video CDN requires; no other
//!   cookie is ever attached to media.
//! - Upstream's `_fallback` URLs are not emitted; only the best URL is.
//! - Upstream logs per-post failures in profile listings and continues.
//!   There is no warning channel here, so failed posts are skipped and an
//!   error is only reported when no listed post could be extracted.
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error, event::ExtractionEvent};
use base64::Engine as _;
use futures_util::StreamExt as _;
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use url::Url;

pub static SITE: Site = Site {
    category: "tiktok",
    name: "TikTok",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[
        &POST, &VMPOST, &USER, &AVATAR, &POSTS, &REPOSTS, &STORIES, &LIKES, &SAVED, &FOLLOWING,
    ],
    api_key: false,
    cookies: Some("tiktok.com"),
};

const FILENAME: &str = "{id}{num:?_//>02} {title[b:150]}{file_id:? [/]/}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{user}"];
const ARCHIVE: &str = "{id}_{num}_{file_id}";

static POST: Route = Route::new("post", FILENAME, DIRECTORY, ARCHIVE);
static VMPOST: Route = Route::new("vmpost", FILENAME, DIRECTORY, ARCHIVE);
static USER: Route = Route::new("user", FILENAME, DIRECTORY, ARCHIVE);
static AVATAR: Route = Route::new("avatar", FILENAME, DIRECTORY, ARCHIVE);
static POSTS: Route = Route::new("posts", FILENAME, DIRECTORY, ARCHIVE);
static REPOSTS: Route = Route::new("reposts", FILENAME, DIRECTORY, ARCHIVE);
static STORIES: Route = Route::new("stories", FILENAME, DIRECTORY, ARCHIVE);
static LIKES: Route = Route::new("likes", FILENAME, DIRECTORY, ARCHIVE);
static SAVED: Route = Route::new("saved", FILENAME, DIRECTORY, ARCHIVE);
static FOLLOWING: Route = Route::new("following", FILENAME, DIRECTORY, ARCHIVE);

const ROOT: &str = "https://www.tiktok.com";
const REFERER: &str = "https://www.tiktok.com/";
/// Browser User-Agent for page and API requests unless one is configured;
/// TikTok serves no rehydration data to unknown clients.
const BROWSER_UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:128.0) Gecko/20100101 Firefox/128.0";
/// Hosts TikTok serves media from.
const MEDIA_HOSTS: &[&str] = &[
    "tiktok.com",
    "tiktokv.com",
    "tiktokv.us",
    "tiktokv.eu",
    "tiktokcdn.com",
    "tiktokcdn-us.com",
    "tiktokcdn-eu.com",
    "ibytedtos.com",
    "ibyteimg.com",
    "byteoversea.com",
    "muscdn.com",
];
/// Attempts per API page for empty or undecodable responses.
const PAGE_ATTEMPTS: usize = 3;
/// Upper bound on API pages for one listing.
const MAX_PAGES: usize = 10_000;
const PROFILE_PARTS: [&str; 6] = ["avatar", "posts", "reposts", "stories", "likes", "saved"];

#[derive(Debug, PartialEq)]
enum Target {
    /// `user` is `None` for `/share/video/ID`.
    Post {
        user: Option<String>,
        id: String,
    },
    /// The full short link.
    Vmpost(String),
    User(String),
    Avatar(String),
    Posts(String),
    Reposts(String),
    Stories(String),
    Likes(String),
    Saved(String),
    Following,
}

/// Python `\w` plus `.` and `-`.
fn user_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '.' | '-')
}

/// Split `@NAME` (`[\w.-]*`) from the start of `path`.
fn split_user(path: &str) -> Option<(&str, &str)> {
    let rest = path.strip_prefix('@')?;
    let end = rest.find(|c| !user_char(c)).unwrap_or(rest.len());
    Some((&rest[..end], &rest[end..]))
}

/// The digits after `/photo/` or `/video/`.
fn post_id(rest: &str) -> Option<String> {
    let rest = rest
        .strip_prefix("/photo/")
        .or_else(|| rest.strip_prefix("/video/"))?;
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    (end > 0).then(|| rest[..end].to_owned())
}

/// Upstream patterns, tried in upstream class order.
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) {
        return None;
    }
    let host = url.host_str()?;
    let base = matches!(
        host,
        "tiktok.com" | "www.tiktok.com" | "tiktokv.com" | "www.tiktokv.com"
    );
    let path = crate::sites::percent_decode(url.path());
    let path = path.strip_prefix('/')?;

    // TiktokPostExtractor: /(?:@([\w_.-]*)|share)/(?:phot|vide)o/(\d+)
    if base {
        if let Some((user, rest)) = split_user(path)
            && let Some(id) = post_id(rest)
        {
            return Some(Target::Post {
                user: Some(user.to_owned()),
                id,
            });
        }
        if let Some(rest) = path.strip_prefix("share")
            && let Some(id) = post_id(rest)
        {
            return Some(Target::Post { user: None, id });
        }
    }

    // TiktokVmpostExtractor:
    // (?:(?:v[mt]\.)?tiktok\.com|(?:www\.)?tiktok\.com/t)/(?!@)([^/?#]+)
    let short = match host {
        "vm.tiktok.com" | "vt.tiktok.com" | "tiktok.com" => Some(path),
        "www.tiktok.com" => path.strip_prefix("t/"),
        _ => None,
    };
    if let Some(rest) = short
        && !rest.is_empty()
        && !rest.starts_with(['/', '@'])
    {
        return Some(Target::Vmpost(url.as_str().to_owned()));
    }

    if !base {
        return None;
    }
    if path.starts_with("following") {
        // Upstream lists TiktokFollowingExtractor last, but no earlier
        // pattern can match this path on these hosts.
        return Some(Target::Following);
    }
    let (user, rest) = split_user(path)?;
    if user.is_empty() {
        return None;
    }
    let user = user.to_owned();
    // TiktokUserExtractor: USER_PATTERN/?(?:$|\?|#)
    if rest.is_empty() || rest == "/" {
        return Some(Target::User(user));
    }
    let rest = rest.strip_prefix('/')?;
    if rest.starts_with("avatar") {
        Some(Target::Avatar(user))
    } else if rest.starts_with("posts") {
        Some(Target::Posts(user))
    } else if rest.starts_with("reposts") {
        Some(Target::Reposts(user))
    } else if rest.starts_with("stories") {
        Some(Target::Stories(user))
    } else if rest.starts_with("likes") || rest.starts_with("liked") {
        Some(Target::Likes(user))
    } else if rest.starts_with("saved") {
        Some(Target::Saved(user))
    } else {
        None
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported TikTok URL"));
    };
    match target {
        Target::Post { user, id } => crate::sites::once(async move {
            let options = Options::new(http)?;
            let url = format!("{ROOT}/@{}/video/{id}", user.unwrap_or_default());
            Ok(crate::sites::events(
                post_events(http, &POST, &options, &url).await?,
            ))
        }),
        Target::Vmpost(short) => crate::sites::once(async move {
            let url = resolve_short(http, &short).await?;
            Ok(crate::sites::queue(
                http,
                "tiktok",
                "vmpost",
                url,
                &Map::new(),
            ))
        }),
        Target::User(user) => dispatch(http, &user),
        Target::Avatar(user) => crate::sites::once(async move {
            let mut ctx = Context::default();
            Ok(crate::sites::events(avatar(http, &mut ctx, &user).await?))
        }),
        Target::Posts(user) => listing(http, &POSTS, move |options| async move {
            let mut ctx = Context::default();
            user_posts(http, &mut ctx, &options, &user).await
        }),
        Target::Reposts(user) => listing(http, &REPOSTS, move |options| async move {
            let mut ctx = Context::default();
            secuid_listing(http, &mut ctx, &options, &user, &REPOST_LIST).await
        }),
        Target::Likes(user) => listing(http, &LIKES, move |options| async move {
            let mut ctx = Context::default();
            secuid_listing(http, &mut ctx, &options, &user, &FAVORITE_LIST).await
        }),
        Target::Saved(user) => listing(http, &SAVED, move |options| async move {
            let mut ctx = Context::default();
            secuid_listing(http, &mut ctx, &options, &user, &SAVED_LIST).await
        }),
        Target::Stories(user) => listing(http, &STORIES, move |options| async move {
            let mut ctx = Context::default();
            stories(http, &mut ctx, &options, &user).await
        }),
        Target::Following => listing(http, &FOLLOWING, move |options| async move {
            let mut ctx = Context::default();
            following(http, &mut ctx, &options).await
        }),
    }
}

// ---------------------------------------------------------------------------
// Options

#[derive(Clone, Copy, Debug, PartialEq)]
enum Covers {
    Off,
    First,
    All,
}

/// Upstream `subtitles`: `None` filters are disabled; both `None` means
/// every subtitle.
#[derive(Clone, Debug, Default, PartialEq)]
struct Subtitles {
    sources: Option<HashSet<String>>,
    langs: Option<HashSet<String>>,
}

/// One `range(start, stop, step)` of `util.predicate_range_parse`.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Span {
    start: i64,
    stop: i64,
    step: i64,
}

impl Span {
    fn contains(&self, index: i64) -> bool {
        if self.step > 0 {
            index >= self.start && index < self.stop && (index - self.start) % self.step == 0
        } else {
            index <= self.start && index > self.stop && (self.start - index) % -self.step == 0
        }
    }
}

#[derive(Clone, Debug)]
struct Options {
    photo: bool,
    audio: bool,
    video: bool,
    covers: Covers,
    subtitles: Option<Subtitles>,
    range: Vec<Span>,
    user_agent: String,
}

impl Options {
    fn new(http: &Http) -> Result<Self> {
        let covers = match http.option("covers") {
            Some(Value::String(s)) if s == "all" => Covers::All,
            _ if http.option_bool("covers", false) => Covers::First,
            _ => Covers::Off,
        };
        let subtitles = match http.option("subtitles") {
            Some(Value::String(s)) if s == "all" => Some(Subtitles::default()),
            Some(Value::String(s)) if !s.is_empty() => Some(subtitle_filter(s)),
            _ if http.option_bool("subtitles", false) => Some(subtitle_filter("ASR")),
            _ => None,
        };
        let range = match http.option("tiktok-range") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::String(s)) => parse_range(s)?,
            Some(Value::Number(n)) => parse_range(&n.to_string())?,
            Some(Value::Array(groups)) => {
                let mut spans = Vec::new();
                for group in groups {
                    spans.extend(parse_range(&crate::sites::scalar(group))?);
                }
                spans
            }
            Some(Value::Bool(false)) => Vec::new(),
            Some(_) => return Err(error("invalid tiktok-range option")),
        };
        Ok(Self {
            photo: http.option_bool("photos", true),
            audio: http.option_bool("audio", true),
            video: http.option_bool("videos", true),
            covers,
            subtitles,
            range,
            user_agent: user_agent(http),
        })
    }
}

fn subtitle_filter(spec: &str) -> Subtitles {
    let known = ["ASR", "MT", "LC"];
    let filters: HashSet<String> = spec.split(',').map(str::to_owned).collect();
    let sources: HashSet<String> = filters
        .iter()
        .filter(|f| known.contains(&f.as_str()))
        .cloned()
        .collect();
    let langs: HashSet<String> = filters
        .iter()
        .filter(|f| !known.contains(&f.as_str()))
        .cloned()
        .collect();
    Subtitles {
        sources: (!sources.is_empty()).then_some(sources),
        langs: (!langs.is_empty()).then_some(langs),
    }
}

/// `util.predicate_range_parse`: `"-2,4,6-8,10-"` or `"1:2,4:8:2"`.
fn parse_range(spec: &str) -> Result<Vec<Span>> {
    let invalid = || error("invalid tiktok-range option");
    let int = |text: &str| text.trim().parse::<i64>().map_err(|_| invalid());
    let mut spans = Vec::new();
    for group in spec.split(',') {
        if group.is_empty() {
            continue;
        }
        let span = if let Some((start, rest)) = group.split_once(':') {
            let (stop, step) = rest.split_once(':').unwrap_or((rest, ""));
            let step = if step.trim().is_empty() {
                1
            } else {
                int(step)?
            };
            if step == 0 {
                return Err(invalid());
            }
            Span {
                start: if start.trim().is_empty() {
                    1
                } else {
                    int(start)?
                },
                stop: if stop.trim().is_empty() {
                    i64::MAX
                } else {
                    int(stop)?
                },
                step,
            }
        } else if let Some((start, stop)) = group.split_once('-') {
            Span {
                start: if start.trim().is_empty() {
                    1
                } else {
                    int(start)?
                },
                stop: if stop.trim().is_empty() {
                    i64::MAX
                } else {
                    int(stop)?.saturating_add(1)
                },
                step: 1,
            }
        } else {
            let start = int(group)?;
            Span {
                start,
                stop: start.saturating_add(1),
                step: 1,
            }
        };
        spans.push(span);
    }
    Ok(spans)
}

fn user_agent(http: &Http) -> String {
    http.network.user_agent().unwrap_or(BROWSER_UA).to_owned()
}

// ---------------------------------------------------------------------------
// Pages and rehydration data

/// Per-extraction caches (upstream's class-level rehydration caches).
#[derive(Default)]
struct Context {
    user_detail: HashMap<String, Value>,
    app_context: Option<Value>,
}

async fn fetch_page(http: &Http, url: &str) -> Result<String> {
    let ua = user_agent(http);
    let headers = [
        ("User-Agent", ua.as_str()),
        (
            "Accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        ),
        ("Accept-Language", "en-US,en;q=0.5"),
        ("Referer", REFERER),
    ];
    http.text_with(&http.api(url)?, &headers).await
}

/// `__DEFAULT_SCOPE__` of the rehydration JSON, or an equivalent scope
/// built from a legacy `SIGI_STATE` page.
fn parse_scope(html: &str) -> Option<Map<String, Value>> {
    if let Some(raw) = crate::sites::extr(
        html,
        r#"<script id="__UNIVERSAL_DATA_FOR_REHYDRATION__" type="application/json">"#,
        "</script>",
    ) && let Ok(Value::Object(mut data)) = serde_json::from_str::<Value>(raw)
        && let Some(Value::Object(scope)) = data.remove("__DEFAULT_SCOPE__")
    {
        return Some(scope);
    }
    let raw = crate::sites::extr(
        html,
        r#"<script id="SIGI_STATE" type="application/json">"#,
        "</script>",
    )?;
    sigi_scope(&serde_json::from_str(raw).ok()?)
}

fn sigi_scope(state: &Value) -> Option<Map<String, Value>> {
    let mut scope = Map::new();
    let users = state
        .pointer("/UserModule/users")
        .and_then(Value::as_object);
    if let Some(item) = state
        .get("ItemModule")
        .and_then(Value::as_object)
        .and_then(|items| items.values().next())
        .and_then(Value::as_object)
    {
        let mut item = item.clone();
        if let Some(Value::String(name)) = item.get("author") {
            let author = users
                .and_then(|users| users.get(name))
                .cloned()
                .unwrap_or_else(|| json!({"uniqueId": name}));
            item.insert("author".into(), author);
        }
        scope.insert(
            "webapp.video-detail".into(),
            json!({"statusCode": 0, "itemInfo": {"itemStruct": item}}),
        );
    }
    if let Some((name, user)) = users.and_then(|users| users.iter().next()) {
        let stats = state
            .pointer("/UserModule/stats")
            .and_then(|stats| stats.get(name))
            .cloned()
            .unwrap_or(Value::Null);
        scope.insert(
            "webapp.user-detail".into(),
            json!({"statusCode": 0, "userInfo": {"user": user, "stats": stats}}),
        );
    }
    if let Some(app) = state.pointer("/AppContext/appContext") {
        scope.insert("webapp.app-context".into(), app.clone());
    }
    (!scope.is_empty()).then_some(scope)
}

/// `_extract_rehydration_data`: fetch a page, solving the JavaScript
/// challenge once if the data is missing, and retry once.
async fn rehydration(http: &Http, url: &str, has_keys: &[&str]) -> Result<Map<String, Value>> {
    for attempt in 0..2 {
        let html = fetch_page(http, url).await?;
        if let Some(scope) = parse_scope(&html)
            && has_keys.iter().all(|key| scope.contains_key(*key))
        {
            return Ok(scope);
        }
        if attempt == 0 {
            // Without a challenge, the second attempt is a plain retry.
            let _ = solve_challenge(http, &html).await;
        }
    }
    Err(error(
        "could not extract TikTok rehydration data; the page may require cookies",
    ))
}

fn b64decode(text: &str) -> Option<Vec<u8>> {
    let engine = base64::engine::GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        base64::engine::GeneralPurposeConfig::new()
            .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent)
            .with_decode_allow_trailing_bits(true),
    );
    let cleaned: String = text
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/'))
        .collect();
    engine.decode(cleaned).ok()
}

fn b64encode(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Python `json.dumps` with compact separators and `ensure_ascii`.
fn python_json(value: &Value) -> String {
    let mut out = String::new();
    for c in value.to_string().chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            let mut units = [0u16; 2];
            for unit in c.encode_utf16(&mut units) {
                out.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    out
}

fn class_of<'h>(html: &'h str, id: &str) -> &'h str {
    crate::sites::extr(html, &format!("id=\"{id}\""), ">")
        .and_then(|tag| crate::sites::extr(tag, "class=\"", "\""))
        .unwrap_or_default()
}

/// The challenge cookies `(name, value)` for a challenge page.
fn challenge_cookies(html: &str) -> Result<Vec<(String, String)>> {
    let failed = || error("failed to solve the TikTok JavaScript challenge");
    let cs = class_of(html, "cs");
    let mut c: Value = b64decode(cs)
        .and_then(|raw| serde_json::from_slice(&raw).ok())
        .ok_or_else(failed)?;
    let expected = c
        .pointer("/v/c")
        .and_then(Value::as_str)
        .and_then(b64decode)
        .ok_or_else(failed)?;
    let base = c
        .pointer("/v/a")
        .and_then(Value::as_str)
        .and_then(b64decode)
        .ok_or_else(failed)?;
    let idx = find_digest(&base, &expected).ok_or_else(failed)?;
    let wci = class_of(html, "wci");
    let rci = class_of(html, "rci");
    let rs = class_of(html, "rs");
    if !cookie_name(wci) {
        return Err(failed());
    }
    c["d"] = b64encode(idx.to_string().as_bytes()).into();
    let mut cookies = vec![(wci.to_owned(), b64encode(python_json(&c).as_bytes()))];
    if !rs.is_empty() && cookie_name(rci) && !rs.contains([';', '\r', '\n']) {
        cookies.push((rci.to_owned(), rs.to_owned()));
    }
    Ok(cookies)
}

fn find_digest(base: &[u8], expected: &[u8]) -> Option<u32> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(base);
    (0..1_000_000u32).find(|idx| {
        let mut test = hasher.clone();
        test.update(idx.to_string().as_bytes());
        test.finalize().as_slice() == expected
    })
}

fn cookie_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

/// `_solve_challenge`: store the short-lived challenge cookies.
async fn solve_challenge(http: &Http, html: &str) -> Result<()> {
    let html = html.to_owned();
    let cookies = tokio::task::spawn_blocking(move || challenge_cookies(&html))
        .await
        .map_err(|_| error("failed to solve the TikTok JavaScript challenge"))??;
    let origin = http.api(REFERER)?;
    for (name, value) in cookies {
        http.set_cookie(&origin, &format!("{name}={value}; Path=/; Max-Age=5"));
    }
    Ok(())
}

/// `_check_status_code`: the problem with a detail object, if any.
fn status_problem(detail: &Value, kind: &str) -> Option<String> {
    let status = match detail.get("statusCode") {
        None | Some(Value::Null) => return None,
        Some(Value::Number(n)) => n.as_i64().unwrap_or(-1),
        Some(Value::String(s)) if s.is_empty() => return None,
        Some(Value::String(s)) => s.parse().unwrap_or(-1),
        Some(_) => -1,
    };
    match status {
        0 => None,
        10222 => match video_count(detail) {
            Some(count) if count > 0 => None,
            None => Some(format!("login required to access this {kind}")),
            Some(_) => Some(format!(
                "login required to access this {kind}, or this profile has no videos posted"
            )),
        },
        10221 => Some("user account could not be found".into()),
        10204 => Some(format!("requested {kind} not available")),
        10231 => Some("region locked - try downloading with a VPN/proxy connection".into()),
        _ => {
            let message: String = detail
                .get("statusMsg")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .chars()
                .filter(|c| !c.is_control())
                .take(100)
                .collect();
            Some(format!(
                "received unknown error code {status} ('{message}')"
            ))
        }
    }
}

fn video_count(detail: &Value) -> Option<i64> {
    let info = detail.get("userInfo").filter(|v| truthy(v))?;
    let stats = info
        .get("stats")
        .filter(|v| truthy(v))
        .or_else(|| info.get("statsV2"))?;
    match stats.get("videoCount")? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn str_of<'v>(value: &'v Value, key: &str) -> &'v str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// A scalar as text (ids may be numbers or strings).
fn text_of(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

/// `_extract_rehydration_data_user`.
async fn user_detail(
    http: &Http,
    ctx: &mut Context,
    profile_url: &str,
    keys: &[&str],
) -> Result<Value> {
    let detail = match ctx.user_detail.get(profile_url) {
        Some(detail) => detail.clone(),
        None => {
            let mut scope = rehydration(
                http,
                profile_url,
                &["webapp.user-detail", "webapp.app-context"],
            )
            .await?;
            let detail = scope.remove("webapp.user-detail").unwrap_or_default();
            ctx.user_detail
                .insert(profile_url.to_owned(), detail.clone());
            ctx.app_context = scope.remove("webapp.app-context");
            detail
        }
    };
    if let Some(problem) = status_problem(&detail, "profile") {
        return Err(error(format!("TikTok profile: {problem}")));
    }
    let mut data = &detail;
    for key in keys {
        data = data.get(*key).ok_or_else(|| {
            error(format!(
                "could not extract TikTok rehydration data ({})",
                keys.join(", ")
            ))
        })?;
    }
    Ok(data.clone())
}

fn sec_uid_like(text: &str) -> bool {
    text.strip_prefix("MS4wLjABAAAA").is_some_and(|rest| {
        (32..=64).contains(&rest.chars().count())
            && rest
                .chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-'))
    })
}

fn digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

/// `_extract_sec_uid`.
async fn sec_uid(http: &Http, ctx: &mut Context, user: &str) -> Result<String> {
    if sec_uid_like(user) {
        return Ok(user.to_owned());
    }
    let profile = format!("{ROOT}/@{user}");
    let id = user_detail(http, ctx, &profile, &["userInfo", "user", "secUid"]).await?;
    id.as_str()
        .filter(|id| sec_uid_like(id))
        .map(str::to_owned)
        .ok_or_else(|| error("unable to extract the TikTok secondary user ID"))
}

/// `_extract_author_id`.
async fn author_id(http: &Http, ctx: &mut Context, user: &str) -> Result<String> {
    if digits(user) {
        return Ok(user.to_owned());
    }
    let profile = format!("{ROOT}/@{user}");
    let id = user_detail(http, ctx, &profile, &["userInfo", "user", "id"]).await?;
    Some(text_of(Some(&id)))
        .filter(|id| digits(id))
        .ok_or_else(|| error("unable to extract the TikTok user ID"))
}

// ---------------------------------------------------------------------------
// Posts

/// `_sanitize_url`.
fn sanitize(url: &str) -> String {
    let url = url.replacen("/photo/", "/video/", 1);
    if url.starts_with("http://") || url.starts_with("https://") {
        url
    } else {
        format!("https://{}", url.trim_start_matches('/'))
    }
}

/// One post page, as upstream `items()` does for each post URL.
async fn post_events(
    http: &Http,
    route: &'static Route,
    options: &Options,
    url: &str,
) -> Result<Vec<ExtractionEvent>> {
    let mut url = sanitize(url);
    let mut scope = rehydration(http, &url, &[]).await?;
    if !scope.contains_key("webapp.video-detail") {
        // Only /video/ links carry the video-detail object.
        let canonical = scope
            .get("seo.abtest")
            .map(|seo| str_of(seo, "canonical"))
            .unwrap_or_default();
        let canonical = sanitize(canonical);
        if !Url::parse(&canonical).is_ok_and(|u| matches!(target(&u), Some(Target::Post { .. }))) {
            return Err(error("TikTok page has no post data"));
        }
        url = canonical;
        scope = rehydration(http, &url, &[]).await?;
    }
    let detail = scope
        .get("webapp.video-detail")
        .ok_or_else(|| error("TikTok page has no post data"))?;
    if let Some(problem) = status_problem(detail, "post") {
        return Err(error(format!("TikTok post: {problem}")));
    }
    let post = detail
        .pointer("/itemInfo/itemStruct")
        .and_then(Value::as_object)
        .cloned()
        .ok_or_else(|| error("TikTok page has no post data"))?;
    let token = match http.api(REFERER) {
        Ok(origin) => http.cookie(&origin, "tt_chain_token"),
        Err(_) => None,
    };
    let items = post_items(post, options, token.as_deref())?;
    Ok(crate::sites::post(&SITE, http, route, items))
}

/// The files of one post (upstream `items()` body).
fn post_items(
    mut post: Map<String, Value>,
    options: &Options,
    token: Option<&str>,
) -> Result<Vec<Item>> {
    let id = text_of(post.get("id"));
    let user = post
        .get("author")
        .filter(|a| truthy(a))
        .map(|a| str_of(a, "uniqueId").to_owned())
        .unwrap_or_default();
    let ts = crate::sites::media::ts_value(post.get("createTime"));
    let desc = post
        .get("desc")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    post.insert("user".into(), user.clone().into());
    post.insert("date".into(), crate::sites::media::date(ts));
    let image_post = post.contains_key("imagePost");
    post.insert(
        "post_type".into(),
        if image_post { "image" } else { "video" }.into(),
    );

    let mut items = Vec::new();
    let push = |post: &Map<String, Value>, url: Url, cookie: bool, items: &mut Vec<Item>| {
        let num = text_of(post.get("num"));
        let file_id = post
            .get("file_id")
            .map(crate::sites::scalar)
            .unwrap_or_default();
        let mut item = Item::new(url, post.clone())
            .ids(id.clone(), format!("{num}_{file_id}"))
            .user(Some(user.clone()))
            .caption(Some(desc.clone()))
            .timestamp(ts)
            .header("Referer", REFERER)
            .header("User-Agent", &options.user_agent);
        if cookie && let Some(token) = token {
            item = item.header("Cookie", &format!("tt_chain_token={token}"));
        }
        items.push(item);
    };

    if image_post {
        if options.photo {
            let title = if desc.is_empty() {
                format!("TikTok photo #{id}")
            } else {
                desc.clone()
            };
            let images = post
                .get("imagePost")
                .and_then(|p| p.get("images"))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for (i, img) in images.into_iter().enumerate() {
                let Some(url) = img
                    .pointer("/imageURL/urlList/0")
                    .and_then(Value::as_str)
                    .and_then(|u| crate::sites::media_url(u, MEDIA_HOSTS))
                else {
                    continue;
                };
                let (name, ext) = crate::sites::nameext(&url);
                let file_id = name.split('~').next().unwrap_or_default().to_owned();
                post.insert("filename".into(), name.into());
                post.insert("extension".into(), ext.into());
                post.insert("type".into(), "image".into());
                post.insert(
                    "width".into(),
                    img.get("imageWidth").cloned().unwrap_or(Value::Null),
                );
                post.insert(
                    "height".into(),
                    img.get("imageHeight").cloned().unwrap_or(Value::Null),
                );
                post.insert("image".into(), img);
                post.insert("title".into(), title.clone().into());
                post.insert("num".into(), (i + 1).into());
                post.insert("file_id".into(), file_id.into());
                push(&post, url, false, &mut items);
            }
        }
        if options.audio
            && let Some(music) = post.get("music").cloned()
            && let Some(url) = music
                .get("playUrl")
                .and_then(Value::as_str)
                .and_then(|u| crate::sites::media_url(u, MEDIA_HOSTS))
        {
            let (name, ext) = crate::sites::nameext(&url);
            post.insert("filename".into(), name.into());
            post.insert(
                "extension".into(),
                if ext.is_empty() { "mp3".into() } else { ext }.into(),
            );
            post.insert("type".into(), "audio".into());
            post.insert("image".into(), Value::Null);
            post.insert("title".into(), fallback_title(&desc, "audio", &id).into());
            post.insert(
                "duration".into(),
                music.get("duration").cloned().unwrap_or(Value::Null),
            );
            post.insert("num".into(), 0.into());
            post.insert(
                "file_id".into(),
                music.get("id").cloned().unwrap_or(Value::Null),
            );
            post.insert("width".into(), 0.into());
            post.insert("height".into(), 0.into());
            push(&post, url, false, &mut items);
        }
    } else if let Some(video) = post.get("video").cloned() {
        if options.video {
            let url = video_urls(&video)
                .into_iter()
                .find_map(|u| crate::sites::media_url(&u, MEDIA_HOSTS))
                .ok_or_else(|| {
                    error("failed to extract TikTok video URLs; you may need cookies")
                })?;
            let (name, ext) = crate::sites::nameext(&url);
            let ext = if ext.is_empty() {
                Some(str_of(&video, "format"))
                    .filter(|f| !f.is_empty())
                    .unwrap_or("mp4")
                    .to_owned()
            } else {
                ext
            };
            post.insert("filename".into(), name.into());
            post.insert("extension".into(), ext.into());
            post.insert("type".into(), "video".into());
            post.insert("image".into(), Value::Null);
            post.insert("title".into(), fallback_title(&desc, "video", &id).into());
            post.insert(
                "duration".into(),
                video.get("duration").cloned().unwrap_or(Value::Null),
            );
            post.insert("num".into(), 0.into());
            post.insert("file_id".into(), "".into());
            post.insert(
                "width".into(),
                video.get("width").cloned().unwrap_or(Value::Null),
            );
            post.insert(
                "height".into(),
                video.get("height").cloned().unwrap_or(Value::Null),
            );
            push(&post, url, true, &mut items);
        }
        if options.covers != Covers::Off {
            for key in ["thumbnail", "cover", "originCover", "dynamicCover"] {
                let Some(raw) = video
                    .get(key)
                    .and_then(Value::as_str)
                    .filter(|u| !u.is_empty())
                else {
                    continue;
                };
                let Some(url) = crate::sites::media_url(raw, MEDIA_HOSTS) else {
                    continue;
                };
                let (name, _) = crate::sites::nameext(&url);
                post.insert("filename".into(), name.into());
                post.insert("extension".into(), "jpg".into());
                post.insert("type".into(), "cover".into());
                post.insert("image".into(), raw.into());
                post.insert(
                    "title".into(),
                    fallback_title(&desc, "video cover", &id).into(),
                );
                post.insert(
                    "duration".into(),
                    video.get("duration").cloned().unwrap_or(Value::Null),
                );
                post.insert("num".into(), 0.into());
                post.insert("file_id".into(), key.into());
                post.insert("width".into(), 0.into());
                post.insert("height".into(), 0.into());
                push(&post, url, false, &mut items);
                if options.covers != Covers::All {
                    break;
                }
            }
        }
        if let Some(filter) = &options.subtitles {
            subtitles(&mut post, &video, filter, &desc, &id, &mut |post, url| {
                push(post, url, false, &mut items)
            });
        }
    }
    Ok(items)
}

fn fallback_title(desc: &str, kind: &str, id: &str) -> String {
    if desc.is_empty() {
        format!("TikTok {kind} #{id}")
    } else {
        desc.to_owned()
    }
}

/// `_extract_video_urls`: `bitrateInfo` by descending resolution, then
/// `playAddr`.
fn video_urls(video: &Value) -> Vec<String> {
    let mut urls = Vec::new();
    if let Some(info) = video.get("bitrateInfo") {
        let list = match info {
            Value::Array(list) => list.clone(),
            other => vec![other.clone()],
        };
        let mut sizes: Vec<(i64, Vec<String>)> = Vec::new();
        for entry in &list {
            let Some(play) = entry.get("PlayAddr") else {
                continue;
            };
            let dim = |key| match play.get(key) {
                Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
                Some(Value::String(s)) => crate::sites::media::parse_int(s),
                _ => 0,
            };
            let size = dim("Width") * dim("Height");
            let list: Vec<String> = play
                .get("UrlList")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect();
            match sizes.iter_mut().find(|(s, _)| *s == size) {
                Some((_, urls)) => urls.extend(list),
                None => sizes.push((size, list)),
            }
        }
        sizes.sort_by_key(|a| std::cmp::Reverse(a.0));
        urls.extend(sizes.into_iter().flat_map(|(_, urls)| urls));
    }
    if let Some(play) = video.get("playAddr").and_then(Value::as_str) {
        urls.push(play.to_owned());
    }
    urls
}

/// `_extract_subtitles`.
fn subtitles(
    post: &mut Map<String, Value>,
    video: &Value,
    filter: &Subtitles,
    desc: &str,
    id: &str,
    push: &mut dyn FnMut(&Map<String, Value>, Url),
) {
    let infos = video
        .get("subtitleInfos")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let field = |sub: &Value, key: &str| sub.get(key).cloned().unwrap_or(Value::Null);
    for sub in infos {
        let source = str_of(&sub, "Source");
        let lang = str_of(&sub, "LanguageCodeName");
        let sources_match = filter.sources.as_ref().is_some_and(|s| s.contains(source));
        let langs_match = filter.langs.as_ref().is_some_and(|l| l.contains(lang));
        if !sources_match && !langs_match && (filter.sources.is_some() || filter.langs.is_some()) {
            continue;
        }
        let Some(url) = sub
            .get("Url")
            .and_then(Value::as_str)
            .filter(|u| !u.is_empty())
            .and_then(|u| crate::sites::media_url(u, MEDIA_HOSTS))
        else {
            continue;
        };
        let py = |key: &str| crate::sites::scalar(&field(&sub, key));
        let (mut name, mut ext) = crate::sites::nameext(&url);
        if name.is_empty() {
            name = format!(
                "{id}_{}_{}_{}",
                py("LanguageCodeName"),
                py("Version"),
                py("Source")
            );
            ext = py("Format").to_lowercase();
            ext = match ext.as_str() {
                "webvtt" => "vtt".into(),
                "creator_caption" => "json".into(),
                _ => ext,
            };
        }
        post.insert("filename".into(), name.into());
        post.insert("extension".into(), ext.into());
        post.insert("type".into(), "subtitle".into());
        post.insert("image".into(), Value::Null);
        post.insert(
            "title".into(),
            fallback_title(desc, "video subtitle", id).into(),
        );
        post.insert(
            "duration".into(),
            video.get("duration").cloned().unwrap_or(Value::Null),
        );
        post.insert("num".into(), 0.into());
        post.insert(
            "file_id".into(),
            format!(
                "{}_{}_{}_{}_{}",
                py("LanguageID"),
                py("LanguageCodeName"),
                py("Source"),
                py("Version"),
                py("Format")
            )
            .into(),
        );
        post.insert("subtitle_lang_id".into(), field(&sub, "LanguageID"));
        post.insert(
            "subtitle_lang_codename".into(),
            field(&sub, "LanguageCodeName"),
        );
        post.insert("subtitle_format".into(), field(&sub, "Format"));
        post.insert("subtitle_version".into(), field(&sub, "Version"));
        post.insert("subtitle_source".into(), field(&sub, "Source"));
        post.insert("width".into(), 0.into());
        post.insert("height".into(), 0.into());
        push(post, url);
    }
    for key in [
        "subtitle_lang_id",
        "subtitle_lang_codename",
        "subtitle_format",
        "subtitle_version",
        "subtitle_source",
    ] {
        post.remove(key);
    }
}

/// Post pages for listed URLs, read lazily one at a time.
fn listing<'a, F, Fut>(http: &'a Http, route: &'static Route, urls: F) -> Events<'a>
where
    F: FnOnce(Arc<Options>) -> Fut + Send + 'a,
    Fut: std::future::Future<Output = Result<Vec<String>>> + Send + 'a,
{
    crate::sites::once(async move {
        let options = Arc::new(Options::new(http)?);
        let urls = urls(options.clone()).await?;
        Ok(post_pages(http, route, options, urls))
    })
}

fn post_pages<'a>(
    http: &'a Http,
    route: &'static Route,
    options: Arc<Options>,
    urls: Vec<String>,
) -> Events<'a> {
    let state = (VecDeque::from(urls), false, None::<crate::Error>);
    crate::sites::paginate(state, move |(mut queue, mut any_ok, mut last)| {
        let options = options.clone();
        async move {
            loop {
                let Some(url) = queue.pop_front() else {
                    return match last {
                        Some(e) if !any_ok => Err(e),
                        _ => Ok(None),
                    };
                };
                match post_events(http, route, &options, &url).await {
                    Ok(events) => {
                        any_ok = true;
                        return Ok(Some((crate::sites::events(events), (queue, any_ok, last))));
                    }
                    Err(e) => last = Some(e),
                }
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Profiles

/// Upstream `Dispatch` for profiles: queue the `include` extractors.
fn dispatch<'a>(http: &'a Http, user: &str) -> Events<'a> {
    let base = format!("{ROOT}/@{user}/");
    let include: Vec<String> = match http.option("include") {
        Some(Value::String(s)) if s == "all" => PROFILE_PARTS.map(str::to_owned).to_vec(),
        Some(Value::String(s)) => s.replace(' ', "").split(',').map(str::to_owned).collect(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        Some(value) if !truthy(value) => Vec::new(),
        _ => vec!["avatar".into(), "posts".into()],
    };
    let children: Vec<Url> = PROFILE_PARTS
        .iter()
        .filter(|part| include.iter().any(|name| name == *part))
        .filter_map(|part| Url::parse(&format!("{base}{part}")).ok())
        .collect();
    futures_util::stream::iter(children)
        .map(move |child| crate::sites::queue(http, "tiktok", "user", child, &Map::new()))
        .flatten()
        .boxed()
}

async fn avatar(http: &Http, ctx: &mut Context, user: &str) -> Result<Vec<ExtractionEvent>> {
    let profile = format!("{ROOT}/@{user}");
    let Value::Object(mut data) = user_detail(http, ctx, &profile, &["userInfo", "user"]).await?
    else {
        return Err(error("TikTok profile data is not an object"));
    };
    let name = data.get("uniqueId").cloned().unwrap_or_else(|| user.into());
    data.insert("user".into(), name.clone());
    let raw = ["avatarLarger", "avatarMedium", "avatarThumb"]
        .iter()
        .find_map(|key| {
            data.get(*key)
                .and_then(Value::as_str)
                .filter(|u| !u.is_empty())
        })
        .ok_or_else(|| error("TikTok profile has no avatar"))?;
    let url = crate::sites::media_url(raw, MEDIA_HOSTS)
        .ok_or_else(|| error("TikTok avatar URL is not on a TikTok host"))?;
    let (filename, ext) = crate::sites::nameext(&url);
    let mut avatar = data.clone();
    let file_id = filename.split('~').next().unwrap_or_default().to_owned();
    avatar.insert("filename".into(), filename.into());
    avatar.insert("extension".into(), ext.into());
    avatar.insert("type".into(), "avatar".into());
    avatar.insert(
        "title".into(),
        format!("@{}", crate::sites::scalar(&name)).into(),
    );
    avatar.insert("id".into(), data.get("id").cloned().unwrap_or(Value::Null));
    avatar.insert("file_id".into(), file_id.clone().into());
    avatar.insert("num".into(), 0.into());
    let id = text_of(data.get("id"));
    let item = Item::new(url, avatar)
        .ids(id, file_id)
        .user(name.as_str().map(str::to_owned))
        .header("Referer", REFERER)
        .header("User-Agent", &user_agent(http));
    Ok(crate::sites::post(&SITE, http, &AVATAR, vec![item]))
}

/// TiktokPostsExtractor `posts()` through the API.
async fn user_posts(
    http: &Http,
    ctx: &mut Context,
    options: &Options,
    user: &str,
) -> Result<Vec<String>> {
    let profile = format!("{ROOT}/@{user}");
    let order = http
        .option_str("order-posts")
        .filter(|order| matches!(*order, "asc" | "reverse" | "popular"))
        .unwrap_or("desc");
    let sec_uid = sec_uid(http, ctx, user).await?;
    if order != "desc" && http.has_cookies() {
        let request_type = match order {
            "popular" => "1",
            _ => "2",
        };
        let params = vec![
            ("secUid", sec_uid.clone()),
            ("post_item_list_request_type", request_type.into()),
            ("count", "15".into()),
            ("needPinnedItemIds", "false".into()),
        ];
        let kind = if request_type == "1" {
            CursorKind::Popular
        } else {
            CursorKind::Forward
        };
        let list = ListRequest {
            endpoint: "post/item_list",
            cursor: kind,
            kind: ListKind::Items,
            ranged: true,
        };
        let (items, ok, _) = execute(http, &list, &options.range, params).await;
        if ok {
            let urls = generate_urls(&items, &profile, options, true);
            if !urls.is_empty() {
                return Ok(urls);
            }
        }
    }
    let params = vec![
        ("secUid", sec_uid),
        ("type", "1".into()),
        ("count", "15".into()),
    ];
    let list = ListRequest {
        endpoint: "creator/item_list",
        cursor: CursorKind::Legacy,
        kind: ListKind::Items,
        ranged: true,
    };
    finish(
        execute(http, &list, &options.range, params).await,
        &profile,
        options,
        true,
    )
}

/// Listings that take a `secUid` and `post_item_list_request_type=0`.
async fn secuid_listing(
    http: &Http,
    ctx: &mut Context,
    options: &Options,
    user: &str,
    list: &ListRequest,
) -> Result<Vec<String>> {
    let profile = format!("{ROOT}/@{user}");
    let params = vec![
        ("secUid", sec_uid(http, ctx, user).await?),
        ("post_item_list_request_type", "0".into()),
        ("needPinnedItemIds", "false".into()),
        ("count", "15".into()),
    ];
    finish(
        execute(http, list, &options.range, params).await,
        &profile,
        options,
        true,
    )
}

async fn stories(
    http: &Http,
    ctx: &mut Context,
    options: &Options,
    user: &str,
) -> Result<Vec<String>> {
    let profile = format!("{ROOT}/@{user}");
    let params = vec![
        ("authorId", author_id(http, ctx, user).await?),
        ("loadBackward", "false".into()),
        ("count", "5".into()),
    ];
    let list = ListRequest {
        endpoint: "story/item_list",
        cursor: CursorKind::Story,
        kind: ListKind::Items,
        ranged: false,
    };
    finish(
        execute(http, &list, &[], params).await,
        &profile,
        options,
        false,
    )
}

/// The stories of every followed account with stories.
async fn following(http: &Http, ctx: &mut Context, options: &Options) -> Result<Vec<String>> {
    let list = ListRequest {
        endpoint: "story/user_list",
        cursor: CursorKind::Item("storyUsers"),
        kind: ListKind::StoryUsers,
        ranged: false,
    };
    let params = vec![("storyFeedScene", "3".into()), ("count", "15".into())];
    let (users, ok, err) = execute(http, &list, &[], params).await;
    if !ok && users.is_empty() {
        return Err(err.unwrap_or_else(|| {
            error("could not extract the TikTok following list; logged-in cookies are required")
        }));
    }
    let mut urls: Vec<String> = Vec::new();
    for batch in users.chunks(10) {
        let mut ids = Vec::new();
        for (id, _) in batch {
            if !is_current_user(http, ctx, id).await? {
                ids.push(id.clone());
            }
        }
        if ids.is_empty() {
            continue;
        }
        let list = ListRequest {
            endpoint: "story/batch/item_list",
            cursor: CursorKind::None,
            kind: ListKind::StoryBatch,
            ranged: false,
        };
        let params = vec![("authorIds", ids.join(",")), ("storyCallScene", "2".into())];
        let (items, _, _) = execute(http, &list, &[], params).await;
        for url in generate_urls(&items, &format!("{ROOT}/@_"), options, false) {
            if !urls.contains(&url) {
                urls.push(url);
            }
        }
    }
    Ok(urls)
}

/// `_is_current_user`, from the `webapp.app-context` of a page.
async fn is_current_user(http: &Http, ctx: &mut Context, id: &str) -> Result<bool> {
    if ctx.app_context.as_ref().is_none_or(|a| !truthy(a)) {
        let mut scope = rehydration(http, REFERER, &["webapp.app-context"]).await?;
        ctx.app_context = scope.remove("webapp.app-context");
    }
    Ok(ctx
        .app_context
        .as_ref()
        .and_then(|a| a.pointer("/user/uid"))
        .is_some_and(|uid| text_of(Some(uid)) == id))
}

fn finish(
    (items, ok, err): (Vec<(String, Value)>, bool, Option<crate::Error>),
    profile: &str,
    options: &Options,
    ranged: bool,
) -> Result<Vec<String>> {
    if !ok
        && items.is_empty()
        && let Some(err) = err
    {
        return Err(err);
    }
    Ok(generate_urls(&items, profile, options, ranged))
}

// ---------------------------------------------------------------------------
// API pagination

#[derive(Clone, Copy, Debug, PartialEq)]
enum CursorKind {
    None,
    /// TiktokForwardTimeCursor. (TiktokBackwardTimeCursor is only used
    /// upstream for `order-posts: desc`, which always takes the legacy
    /// endpoint instead.)
    Forward,
    /// TiktokPopularTimeCursor.
    Popular,
    /// TiktokStoryTimeCursor.
    Story,
    /// TiktokLegacyTimeCursor.
    Legacy,
    /// TiktokItemCursor over this list key.
    Item(&'static str),
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum ListKind {
    /// `itemList` entries keyed by `id`.
    Items,
    /// `batchStoryItemLists[].itemList`.
    StoryBatch,
    /// `storyUsers[].user` as `id` → `uniqueId`.
    StoryUsers,
}

struct ListRequest {
    endpoint: &'static str,
    cursor: CursorKind,
    kind: ListKind,
    /// Whether the `tiktok-range` option applies.
    ranged: bool,
}

static REPOST_LIST: ListRequest = ListRequest {
    endpoint: "repost/item_list",
    cursor: CursorKind::Item("itemList"),
    kind: ListKind::Items,
    ranged: true,
};
static FAVORITE_LIST: ListRequest = ListRequest {
    endpoint: "favorite/item_list",
    cursor: CursorKind::Popular,
    kind: ListKind::Items,
    ranged: true,
};
static SAVED_LIST: ListRequest = ListRequest {
    endpoint: "user/collect/item_list",
    cursor: CursorKind::Popular,
    kind: ListKind::Items,
    ranged: true,
};

const WEEK_MS: i64 = 7 * 86_400_000;

#[derive(Debug)]
struct Cursor {
    kind: CursorKind,
    value: i64,
}

fn int_of(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
        Some(Value::Bool(b)) => i64::from(*b),
        _ => 0,
    }
}

fn last_create_time(data: &Value) -> Option<i64> {
    let item = data.get("itemList")?.as_array()?.last()?;
    let ts = int_of(item.get("createTime"));
    (ts != 0 || item.get("createTime").is_some()).then(|| ts.saturating_mul(1000))
}

impl Cursor {
    fn new(kind: CursorKind) -> Option<Self> {
        let value = match kind {
            CursorKind::None => return None,
            CursorKind::Legacy => {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs() as i64)
                    * 1000
            }
            _ => 0,
        };
        Some(Self { kind, value })
    }

    /// Advance past `data`; `Ok(true)` when the last page was reached.
    fn next_page(&mut self, data: &Value) -> Result<bool> {
        match self.kind {
            CursorKind::None => Ok(true),
            CursorKind::Legacy => {
                let old = self.value;
                self.value = last_create_time(data).unwrap_or(0);
                if self.value == 0 || old == self.value {
                    // The user may not have posted within this week.
                    self.value = old - WEEK_MS;
                }
                let more = data.get("hasMorePrevious").is_some_and(truthy);
                Ok(self.value < 1_472_706_000_000 || !more)
            }
            CursorKind::Item(key) => {
                let count = data.get(key).and_then(Value::as_array).map_or(0, Vec::len);
                self.value += count as i64;
                Ok(match data.get("hasMore") {
                    Some(more) => !truthy(more),
                    None => !data.get("HasMoreAfter").is_some_and(truthy),
                })
            }
            kind => {
                let (reverse, more_key, cursor_key) = match kind {
                    CursorKind::Forward => (false, "hasMore", "cursor"),
                    CursorKind::Story => (false, "HasMoreAfter", "MaxCursor"),
                    _ => (true, "hasMore", "cursor"),
                };
                let has_more = data.get(more_key).is_some_and(truthy);
                let mut new = int_of(data.get(cursor_key));
                if self.value != 0 {
                    // A cursor going the wrong way is replaced.
                    if (reverse && (new > self.value || new == 0))
                        || (!reverse && (new < self.value || new == 0))
                    {
                        new = match (kind, last_create_time(data)) {
                            (CursorKind::Popular, _) => -50_000,
                            (_, Some(ts)) => ts,
                            (_, None) => {
                                if reverse {
                                    -WEEK_MS
                                } else {
                                    WEEK_MS
                                }
                            }
                        };
                    }
                } else if new == 0 && has_more {
                    // Upstream raises for any first page without a cursor
                    // and then keeps the items it already merged; a final
                    // first page is accepted directly instead.
                    return Err(error("could not extract the next TikTok cursor"));
                }
                self.value = new;
                Ok(!has_more)
            }
        }
    }
}

/// A random 64-bit value from std's randomly keyed hasher.
fn random() -> u64 {
    use std::hash::BuildHasher;
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    std::collections::hash_map::RandomState::new().hash_one((
        nanos,
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    ))
}

fn device_id() -> String {
    const LOW: u64 = 7_250_000_000_000_000_000;
    const HIGH: u64 = 7_325_099_899_999_994_577;
    (LOW + random() % (HIGH - LOW + 1)).to_string()
}

fn verify_fp() -> String {
    let mut value = random();
    let mut out = String::from("verify_");
    for _ in 0..7 {
        out.push(char::from_digit((value % 16) as u32, 16).unwrap_or('0'));
        value /= 16;
    }
    out
}

fn api_url(
    http: &Http,
    endpoint: &str,
    device: &str,
    cursor: Option<&Cursor>,
    extra: &[(&str, String)],
) -> Result<Url> {
    let mut url = http.api(&format!("{ROOT}/api/{endpoint}/"))?;
    {
        let mut query = url.query_pairs_mut();
        for (key, value) in [
            ("aid", "1988"),
            ("app_language", "en"),
            ("app_name", "tiktok_web"),
            ("browser_language", "en-US"),
            ("browser_name", "Mozilla"),
            ("browser_online", "true"),
            ("browser_platform", "Win32"),
            ("browser_version", "5.0 (Windows)"),
            ("channel", "tiktok_web"),
            ("cookie_enabled", "true"),
        ] {
            query.append_pair(key, value);
        }
        query.append_pair("device_id", device);
        for (key, value) in [
            ("device_platform", "web_pc"),
            ("focus_state", "true"),
            ("from_page", "user"),
            ("history_len", "2"),
            ("is_fullscreen", "false"),
            ("is_page_visible", "true"),
            ("language", "en"),
            ("os", "windows"),
            ("priority_region", ""),
            ("referer", ""),
            ("region", "US"),
            ("screen_height", "1080"),
            ("screen_width", "1920"),
            ("tz_name", "UTC"),
        ] {
            query.append_pair(key, value);
        }
        query.append_pair("verifyFp", &verify_fp());
        query.append_pair("webcast_language", "en");
        if let Some(cursor) = cursor {
            query.append_pair("cursor", &cursor.value.to_string());
        }
        for (key, value) in extra {
            query.append_pair(key, value);
        }
    }
    Ok(url)
}

/// Items of one response, and whether the list was missing without more
/// pages (upstream `exit_early_due_to_no_items`).
fn extract_items(kind: ListKind, data: &Value) -> (Vec<(String, Value)>, bool) {
    fn item_list(data: &Value, out: &mut Vec<(String, Value)>) -> bool {
        let Some(list) = data.get("itemList").and_then(Value::as_array) else {
            let more = data
                .get("hasMorePrevious")
                .or_else(|| data.get("hasMore"))
                .is_some_and(truthy);
            return !more;
        };
        for item in list {
            let id = text_of(item.get("id"));
            if !id.is_empty() {
                out.push((id, item.clone()));
            }
        }
        false
    }
    let mut out = Vec::new();
    let empty = match kind {
        ListKind::Items => item_list(data, &mut out),
        ListKind::StoryBatch => match data.get("batchStoryItemLists").and_then(Value::as_array) {
            Some(lists) => {
                let mut empty = false;
                for list in lists {
                    empty |= item_list(list, &mut out);
                }
                empty
            }
            None => true,
        },
        ListKind::StoryUsers => match data.get("storyUsers").and_then(Value::as_array) {
            Some(users) => {
                for user in users {
                    let id = text_of(user.pointer("/user/id"));
                    let name = str_of(user.get("user").unwrap_or(&Value::Null), "uniqueId");
                    if !id.is_empty() {
                        out.push((id, name.into()));
                    }
                }
                false
            }
            None => true,
        },
    };
    (out, empty)
}

/// `TiktokPaginationRequest.execute`: all pages, or what was retrieved
/// before a page failed (`ok == false`, with that page's error).
async fn execute(
    http: &Http,
    list: &ListRequest,
    range: &[Span],
    params: Vec<(&str, String)>,
) -> (Vec<(String, Value)>, bool, Option<crate::Error>) {
    let mut items: Vec<(String, Value)> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut device = device_id();
    let mut cursor = Cursor::new(list.cursor);
    let range_stop = (list.ranged && !range.is_empty())
        .then(|| range.iter().map(|s| s.stop).max().unwrap_or(i64::MAX));
    for _ in 0..MAX_PAGES {
        let mut tries = 0;
        loop {
            let page = async {
                let url = api_url(http, list.endpoint, &device, cursor.as_ref(), &params)?;
                let headers = [
                    ("User-Agent", user_agent(http)),
                    ("Accept", "application/json, text/plain, */*".into()),
                    ("Referer", REFERER.into()),
                ];
                let headers: Vec<(&str, &str)> =
                    headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
                let body = http.get(&url, &headers).await?;
                serde_json::from_slice::<Value>(&body)
                    .map_err(|_| error("TikTok returned invalid JSON"))
            }
            .await;
            let result = page.and_then(|data| {
                let (incoming, empty) = extract_items(list.kind, &data);
                let seen: HashSet<&str> = index.keys().map(String::as_str).collect();
                let new: HashSet<&str> = incoming.iter().map(|(id, _)| id.as_str()).collect();
                if !new.is_empty() && new == seen {
                    return Err(error("TikTok API keeps sending the same page"));
                }
                let mut next = cursor.take();
                let last = next.as_mut().map(|c| c.next_page(&data));
                Ok((incoming, empty, next, last))
            });
            match result {
                Ok((incoming, empty, next, last)) => {
                    let last = match last {
                        Some(Err(e)) => {
                            // The cursor is unchanged; retry this page.
                            cursor = next;
                            if tries + 1 >= PAGE_ATTEMPTS {
                                return (items, false, Some(e));
                            }
                            tries += 1;
                            continue;
                        }
                        Some(Ok(last)) => Some(last),
                        None => None,
                    };
                    for (id, item) in incoming {
                        match index.get(&id) {
                            Some(&at) => items[at].1 = item,
                            None => {
                                index.insert(id.clone(), items.len());
                                items.push((id, item));
                            }
                        }
                    }
                    cursor = next;
                    let Some(last) = last else {
                        return (items, true, None);
                    };
                    let enough = range_stop.is_some_and(|stop| items.len() as i64 > stop - 1);
                    if empty || enough || last {
                        return (items, true, None);
                    }
                    break;
                }
                Err(e) => {
                    if e.to_string().contains("same page") {
                        device = device_id();
                    }
                    if tries + 1 >= PAGE_ATTEMPTS {
                        return (items, false, Some(e));
                    }
                    tries += 1;
                }
            }
        }
    }
    (items, true, None)
}

/// `generate_urls`: post URLs passing the range and media-type filters.
fn generate_urls(
    items: &[(String, Value)],
    profile: &str,
    options: &Options,
    ranged: bool,
) -> Vec<String> {
    let mut urls: Vec<String> = Vec::new();
    for (index, (id, item)) in items.iter().enumerate() {
        let index = index as i64 + 1;
        if ranged && !options.range.is_empty() && !options.range.iter().any(|s| s.contains(index)) {
            continue;
        }
        if truthy(item) {
            let image = item.get("imagePost").is_some();
            if (image && !options.photo && !options.audio) || (!image && !options.video) {
                continue;
            }
        }
        if !digits(id) {
            continue;
        }
        let author = item
            .pointer("/author/uniqueId")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty() && name.chars().all(user_char));
        let url = match author {
            Some(name) => format!("{ROOT}/@{name}/video/{id}"),
            None => format!("{profile}/video/{id}"),
        };
        if !urls.contains(&url) {
            urls.push(url);
        }
    }
    urls
}

// ---------------------------------------------------------------------------
// Short links

/// `request_location` with a link-preview User-Agent; the post URL without
/// its query.
async fn resolve_short(http: &Http, short: &str) -> Result<Url> {
    const AGENT: &str = "facebookexternalhit/1.1";
    let client = http
        .network
        .client(AGENT, std::time::Duration::from_secs(60))?;
    let response = client
        .get(http.api(short)?)
        .header("User-Agent", AGENT)
        .send()
        .await
        .map_err(|_| error("TikTok request failed; check network access"))?;
    let status = response.status();
    if matches!(status.as_u16(), 404 | 410) {
        return Err(error("TikTok post not found"));
    }
    if !status.is_redirection() {
        if status.is_success() {
            return Err(error("TikTok post not found"));
        }
        return Err(error(format!("TikTok returned HTTP {status}")));
    }
    let location = response
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let location = Url::parse(short)
        .and_then(|base| base.join(location))
        .map_err(|_| error("TikTok short link has an invalid redirect"))?;
    if location.as_str().len() <= 28 {
        // https://www.tiktok.com/?_r=1
        return Err(error("TikTok post not found"));
    }
    let mut post = location;
    post.set_query(None);
    post.set_fragment(None);
    match target(&post) {
        Some(Target::Post { .. }) => Ok(post),
        _ => Err(error("TikTok short link did not resolve to a post")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, json, keywords, media, response, serve};

    const VIDEO_ID: &str = "7300000000000000001";
    const SEC_UID: &str = "MS4wLjABAAAAabcdefghijklmnopqrstuvwxyz0123456789_-";

    fn t(raw: &str) -> Option<Target> {
        target(&Url::parse(raw).unwrap())
    }

    fn page(scope: Value) -> Vec<u8> {
        html(&format!(
            r#"<html><head><script id="__UNIVERSAL_DATA_FOR_REHYDRATION__" type="application/json">{}</script></head></html>"#,
            json!({"__DEFAULT_SCOPE__": scope})
        ))
    }

    fn with_headers(extra: &str, body: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n",
            body.len()
        )
        .into_bytes();
        out.extend_from_slice(body);
        out
    }

    fn detail(item: Value) -> Value {
        json!({"webapp.video-detail": {"statusCode": 0, "itemInfo": {"itemStruct": item}}})
    }

    fn video_item(id: &str) -> Value {
        json!({
            "id": id,
            "desc": "Hello world",
            "createTime": "1700000000",
            "author": {"uniqueId": "someone", "id": "42"},
            "video": {
                "duration": 12, "width": 576, "height": 1024, "format": "mp4",
                "playAddr": "https://v16-webapp-prime.tiktok.com/video/tos/low/?a=1988",
                "bitrateInfo": [
                    {"PlayAddr": {"Width": 720, "Height": 1280,
                        "UrlList": ["https://v16-webapp-prime.tiktok.com/video/tos/hd/?a=1"]}},
                    {"PlayAddr": {"Width": "1080", "Height": "1920",
                        "UrlList": ["https://v19-webapp-prime.us.tiktok.com/video/tos/fhd/?a=1"]}}
                ],
                "cover": "https://p16-sign.tiktokcdn-us.com/obj/cover1~tplv.jpeg?x=1",
                "originCover": "https://p16-sign.tiktokcdn-us.com/obj/cover2~tplv.jpeg?x=1",
                "subtitleInfos": [
                    {"LanguageID": "2", "LanguageCodeName": "eng-US", "Format": "webvtt",
                     "Version": "1", "Source": "ASR",
                     "Url": "https://v16-webapp.tiktok.com/sub/en/?x=1"},
                    {"LanguageID": "3", "LanguageCodeName": "fra-FR", "Format": "creator_caption",
                     "Version": "4", "Source": "MT",
                     "Url": "https://v16-webapp.tiktok.com/sub/fr/?x=1"}
                ]
            },
            "music": {"id": "999", "playUrl": "https://sf16-ies-music.tiktokcdn.com/obj/song.mp3"}
        })
    }

    fn photo_item(id: &str, desc: &str) -> Value {
        json!({
            "id": id,
            "desc": desc,
            "createTime": 1700000000,
            "author": {"uniqueId": "someone"},
            "imagePost": {"images": [
                {"imageWidth": 1080, "imageHeight": 1440,
                 "imageURL": {"urlList": ["https://p16-sign.tiktokcdn.com/tos/abc~tplv-photomode.jpeg?x=1"]}},
                {"imageWidth": 1080, "imageHeight": 1350,
                 "imageURL": {"urlList": ["https://p16-sign.tiktokcdn.com/tos/def~tplv-photomode.jpeg?x=1"]}}
            ]},
            "music": {"id": 555, "duration": 30,
                      "playUrl": "https://sf16-ies-music.tiktokcdn.com/obj/track"}
        })
    }

    fn profile(user: &str) -> Value {
        json!({
            "webapp.app-context": {"user": {"uid": "1"}},
            "webapp.user-detail": {"statusCode": 0, "userInfo": {
                "user": {"id": "42", "uniqueId": user, "secUid": SEC_UID,
                         "avatarLarger": "https://p16-sign-va.tiktokcdn.com/tos-avt/av1~tplv-crop:1080:1080.jpeg?x=1",
                         "avatarThumb": "https://p16-sign-va.tiktokcdn.com/thumb.jpeg"},
                "stats": {"videoCount": 3}}}
        })
    }

    fn with_options(http: Http, options: Value) -> Http {
        http.with_options(Some(Arc::new(options.as_object().unwrap().clone())))
    }

    fn query(request: &str) -> Vec<(String, String)> {
        let line = request.lines().next().unwrap();
        let target = line.split(' ').nth(1).unwrap();
        Url::parse(&format!("http://x{target}"))
            .unwrap()
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect()
    }

    fn param(request: &str, key: &str) -> Option<String> {
        query(request)
            .into_iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
    }

    fn render(file: &crate::extract::Media) -> (String, std::path::PathBuf, String) {
        let (name, dir) = crate::sites::default_templates(file).unwrap();
        (
            name.render(file).unwrap(),
            dir.render(file).unwrap().unwrap(),
            crate::sites::archive_key(file).unwrap().1,
        )
    }

    async fn errors(events: Events<'_>) -> Vec<String> {
        crate::event::media_only(events)
            .filter_map(|m| async move { m.err().map(|e| e.to_string()) })
            .collect()
            .await
    }

    #[test]
    fn classifies_urls() {
        let post = |user: Option<&str>, id: &str| Target::Post {
            user: user.map(str::to_owned),
            id: id.into(),
        };
        assert_eq!(
            t("https://www.tiktok.com/@some.one_-x/video/7300000000000000001?is_from_webapp=1"),
            Some(post(Some("some.one_-x"), VIDEO_ID))
        );
        assert_eq!(
            t("https://tiktok.com/@user/photo/123"),
            Some(post(Some("user"), "123"))
        );
        assert_eq!(
            t("https://www.tiktokv.com/share/video/123/"),
            Some(post(None, "123"))
        );
        assert_eq!(
            t("https://www.tiktok.com/@/video/123"),
            Some(post(Some(""), "123"))
        );
        assert_eq!(
            t("http://www.tiktok.com/@us%C3%A9r/video/5abc"),
            Some(post(Some("usér"), "5"))
        );
        for raw in [
            "https://vm.tiktok.com/ZMabc123/",
            "https://vt.tiktok.com/ZSabc123",
            "https://www.tiktok.com/t/ZTabc123/",
            // Upstream quirk: the bare domain with any first segment.
            "https://tiktok.com/following",
        ] {
            assert_eq!(t(raw), Some(Target::Vmpost(raw.into())), "{raw}");
        }
        let user = |name: &str| Some(Target::User(name.into()));
        assert_eq!(t("https://www.tiktok.com/@someone"), user("someone"));
        assert_eq!(
            t("https://www.tiktok.com/@someone/?lang=en"),
            user("someone")
        );
        assert_eq!(t("https://tiktokv.com/@someone#x"), user("someone"));
        assert_eq!(
            t("https://www.tiktok.com/@someone/avatar"),
            Some(Target::Avatar("someone".into()))
        );
        assert_eq!(
            t("https://www.tiktok.com/@someone/posts"),
            Some(Target::Posts("someone".into()))
        );
        assert_eq!(
            t("https://www.tiktok.com/@someone/reposts"),
            Some(Target::Reposts("someone".into()))
        );
        assert_eq!(
            t("https://www.tiktok.com/@someone/stories"),
            Some(Target::Stories("someone".into()))
        );
        assert_eq!(
            t("https://www.tiktok.com/@someone/liked"),
            Some(Target::Likes("someone".into()))
        );
        assert_eq!(
            t("https://www.tiktok.com/@someone/likes"),
            Some(Target::Likes("someone".into()))
        );
        assert_eq!(
            t("https://www.tiktok.com/@someone/saved"),
            Some(Target::Saved("someone".into()))
        );
        assert_eq!(
            t("https://www.tiktok.com/following"),
            Some(Target::Following)
        );
        assert_eq!(
            t("https://www.tiktokv.com/following"),
            Some(Target::Following)
        );
        for raw in [
            "https://www.tiktok.com/",
            "https://tiktok.com/",
            "https://vm.tiktok.com/",
            "https://vm.tiktok.com/@user",
            "https://www.tiktok.com/@someone/followers",
            "https://www.tiktok.com/@someone/video/",
            "https://www.tiktok.com/@",
            "https://www.tiktok.com/tag/cats",
            "https://www.tiktok.com/explore",
            "https://m.tiktok.com/@someone",
            "https://www.tiktok.com.evil.test/@someone",
            "https://eviltiktok.com/@someone",
            "https://www.tiktok.com:8443/@someone",
            "https://user@www.tiktok.com/@someone",
            "ftp://www.tiktok.com/@someone",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
        assert!((SITE.matches)(
            &Url::parse("https://vm.tiktok.com/x").unwrap()
        ));
    }

    #[test]
    fn parses_upstream_ranges() {
        let bounds = |spec: &str| {
            parse_range(spec)
                .unwrap()
                .iter()
                .map(|s| (s.start, s.stop))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            bounds("-2,4,6-8,10-"),
            vec![(1, 3), (4, 5), (6, 9), (10, i64::MAX)]
        );
        assert_eq!(bounds(" - 3 , 4-  4, 2-6"), vec![(1, 4), (4, 5), (2, 7)]);
        let spans = parse_range("1:2,4:8:2").unwrap();
        assert!(spans[0].contains(1) && !spans[0].contains(2));
        assert!(spans[1].contains(4) && spans[1].contains(6) && !spans[1].contains(5));
        assert!(!spans[1].contains(8));
        assert!(parse_range("").unwrap().is_empty());
        assert!(parse_range("x").is_err());
        assert!(parse_range("1:5:0").is_err());
    }

    #[test]
    fn parses_options() {
        let (root, _) = serve(vec![]);
        let base = || http("tiktok", root.clone());
        let o = Options::new(&base()).unwrap();
        assert!(o.photo && o.audio && o.video);
        assert_eq!(o.covers, Covers::Off);
        assert_eq!(o.subtitles, None);
        assert!(o.range.is_empty());
        assert_eq!(o.user_agent, BROWSER_UA);

        let o = Options::new(&with_options(
            base(),
            json!({"videos": "ytdl", "audio": false, "photos": 0, "covers": "all",
                   "subtitles": true, "tiktok-range": 5}),
        ))
        .unwrap();
        assert!(o.video && !o.audio && !o.photo);
        assert_eq!(o.covers, Covers::All);
        assert_eq!(o.subtitles, Some(subtitle_filter("ASR")));
        assert_eq!(o.range, parse_range("5").unwrap());

        let o = Options::new(&with_options(
            base(),
            json!({"covers": 1, "subtitles": "all", "tiktok-range": ["1-2", "4"]}),
        ))
        .unwrap();
        assert_eq!(o.covers, Covers::First);
        assert_eq!(o.subtitles, Some(Subtitles::default()));
        assert_eq!(o.range.len(), 2);

        let filter = subtitle_filter("MT,eng-US,LC");
        assert_eq!(
            filter.sources,
            Some(["MT", "LC"].map(str::to_owned).into_iter().collect())
        );
        assert_eq!(
            filter.langs,
            Some(["eng-US"].map(str::to_owned).into_iter().collect())
        );
        assert_eq!(subtitle_filter("deu-DE").sources, None);
        assert!(Options::new(&with_options(base(), json!({"tiktok-range": "a-b"}))).is_err());
    }

    #[test]
    fn reports_status_codes() {
        assert_eq!(status_problem(&json!({}), "post"), None);
        assert_eq!(status_problem(&json!({"statusCode": 0}), "post"), None);
        assert_eq!(
            status_problem(&json!({"statusCode": 10204}), "post").unwrap(),
            "requested post not available"
        );
        assert_eq!(
            status_problem(&json!({"statusCode": 10221}), "profile").unwrap(),
            "user account could not be found"
        );
        assert!(
            status_problem(&json!({"statusCode": 10231}), "post")
                .unwrap()
                .starts_with("region locked")
        );
        assert_eq!(
            status_problem(&json!({"statusCode": 10222}), "post").unwrap(),
            "login required to access this post"
        );
        let private = |count: Value| json!({"statusCode": 10222, "userInfo": {"stats": {}, "statsV2": {"videoCount": count}}});
        assert_eq!(status_problem(&private(json!("7")), "profile"), None);
        assert!(
            status_problem(&private(json!("0")), "profile")
                .unwrap()
                .ends_with("or this profile has no videos posted")
        );
        assert_eq!(
            status_problem(&json!({"statusCode": 1, "statusMsg": "nope\n"}), "post").unwrap(),
            "received unknown error code 1 ('nope')"
        );
    }

    #[test]
    fn orders_video_urls_by_resolution() {
        let urls = video_urls(&video_item("1")["video"]);
        assert_eq!(
            urls,
            vec![
                "https://v19-webapp-prime.us.tiktok.com/video/tos/fhd/?a=1",
                "https://v16-webapp-prime.tiktok.com/video/tos/hd/?a=1",
                "https://v16-webapp-prime.tiktok.com/video/tos/low/?a=1988",
            ]
        );
        let single = json!({"bitrateInfo": {"PlayAddr": {"UrlList": ["https://a.tiktok.com/x"]}}});
        assert_eq!(video_urls(&single), vec!["https://a.tiktok.com/x"]);
        assert!(video_urls(&json!({})).is_empty());
    }

    fn challenge_page(seed: &[u8], idx: u32) -> String {
        use sha2::{Digest, Sha256};
        let mut digest = Sha256::new();
        digest.update(seed);
        digest.update(idx.to_string().as_bytes());
        let c = json!({"v": {"a": b64encode(seed), "c": b64encode(&digest.finalize())}, "s": "é"});
        let cs = b64encode(c.to_string().as_bytes());
        let cs = cs.trim_end_matches('=');
        format!(
            r#"<html><p id="cs" class="{cs}"></p><p id="wci" class="_wafchallengeid"></p><p id="rci" class="waforiginalreid"></p><p id="rs" class="RS123"></p></html>"#
        )
    }

    #[test]
    fn solves_javascript_challenge() {
        let cookies = challenge_cookies(&challenge_page(b"seed-bytes", 4321)).unwrap();
        assert_eq!(cookies[0].0, "_wafchallengeid");
        let raw = String::from_utf8(b64decode(&cookies[0].1).unwrap()).unwrap();
        let bs = '\\';
        assert!(raw.contains(&format!(r#""s":"{bs}u00e9""#)), "{raw}");
        let value: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(b64decode(value["d"].as_str().unwrap()).unwrap(), b"4321");
        assert_eq!(value["s"], "é");
        assert_eq!(cookies[1], ("waforiginalreid".into(), "RS123".into()));
        assert!(challenge_cookies("<html></html>").is_err());
        assert_eq!(
            python_json(&json!({"a": "😀"})),
            format!(r#"{{"a":"{bs}ud83d{bs}ude00"}}"#)
        );
    }

    #[test]
    fn reads_sigi_state_pages() {
        let state = json!({
            "ItemModule": {"5": {"id": "5", "author": "someone", "desc": "d"}},
            "UserModule": {"users": {"someone": {"uniqueId": "someone", "secUid": SEC_UID}},
                           "stats": {"someone": {"videoCount": 2}}},
            "AppContext": {"appContext": {"user": {"uid": "9"}}}
        });
        let html = format!(r#"<script id="SIGI_STATE" type="application/json">{state}</script>"#);
        let scope = parse_scope(&html).unwrap();
        let item = &scope["webapp.video-detail"]["itemInfo"]["itemStruct"];
        assert_eq!(item["author"]["secUid"], SEC_UID);
        assert_eq!(
            scope["webapp.user-detail"]["userInfo"]["stats"]["videoCount"],
            2
        );
        assert_eq!(scope["webapp.app-context"]["user"]["uid"], "9");
        assert!(parse_scope("<html>nothing</html>").is_none());
    }

    #[test]
    fn cursors_follow_upstream_rules() {
        let mut legacy = Cursor::new(CursorKind::Legacy).unwrap();
        assert!(legacy.value > 1_600_000_000_000);
        let page = json!({"itemList": [{"createTime": 1_700_000_000}], "hasMorePrevious": true});
        assert!(!legacy.next_page(&page).unwrap());
        assert_eq!(legacy.value, 1_700_000_000_000);
        // Same cursor again: step back one week.
        assert!(!legacy.next_page(&page).unwrap());
        assert_eq!(legacy.value, 1_700_000_000_000 - WEEK_MS);
        assert!(
            legacy
                .next_page(&json!({"hasMorePrevious": false}))
                .unwrap()
        );
        let mut old = Cursor {
            kind: CursorKind::Legacy,
            value: 1_472_706_000_000,
        };
        assert!(old.next_page(&json!({"hasMorePrevious": true})).unwrap());

        let mut item = Cursor::new(CursorKind::Item("itemList")).unwrap();
        assert!(
            !item
                .next_page(&json!({"itemList": [1, 2], "hasMore": true}))
                .unwrap()
        );
        assert_eq!(item.value, 2);
        let last = json!({"itemList": [3], "HasMoreAfter": true, "hasMore": false});
        assert!(item.next_page(&last).unwrap());
        assert_eq!(item.value, 3);
        let mut users = Cursor::new(CursorKind::Item("storyUsers")).unwrap();
        assert!(
            !users
                .next_page(&json!({"storyUsers": [1], "HasMoreAfter": true}))
                .unwrap()
        );

        let mut popular = Cursor::new(CursorKind::Popular).unwrap();
        assert!(popular.next_page(&json!({"hasMore": true})).is_err());
        assert!(
            !popular
                .next_page(&json!({"hasMore": true, "cursor": "500"}))
                .unwrap()
        );
        assert_eq!(popular.value, 500);
        // Going the wrong way uses the popular fallback.
        assert!(
            !popular
                .next_page(&json!({"hasMore": true, "cursor": 900}))
                .unwrap()
        );
        assert_eq!(popular.value, -50_000);
        assert!(
            popular
                .next_page(&json!({"hasMore": false, "cursor": -60_000}))
                .unwrap()
        );
        assert_eq!(popular.value, -60_000);

        let mut forward = Cursor::new(CursorKind::Forward).unwrap();
        assert!(
            !forward
                .next_page(&json!({"hasMore": true, "cursor": 100}))
                .unwrap()
        );
        let back = json!({"hasMore": true, "cursor": 50, "itemList": [{"createTime": 7}]});
        assert!(!forward.next_page(&back).unwrap());
        assert_eq!(forward.value, 7000);
        assert!(!forward.next_page(&json!({"hasMore": true})).unwrap());
        assert_eq!(forward.value, WEEK_MS);

        let mut story = Cursor::new(CursorKind::Story).unwrap();
        assert!(
            !story
                .next_page(&json!({"HasMoreAfter": true, "MaxCursor": 10}))
                .unwrap()
        );
        assert!(
            story
                .next_page(&json!({"HasMoreAfter": false, "MaxCursor": 20}))
                .unwrap()
        );
        // A single final page without a cursor is accepted.
        let mut first = Cursor::new(CursorKind::Forward).unwrap();
        assert!(first.next_page(&json!({"hasMore": false})).unwrap());
        assert!(Cursor::new(CursorKind::None).is_none());
    }

    #[test]
    fn generates_filtered_urls() {
        let (root, _) = serve(vec![]);
        let items: Vec<(String, Value)> = vec![
            (
                "1".into(),
                json!({"id": "1", "author": {"uniqueId": "a.b"}}),
            ),
            ("2".into(), json!({"id": "2", "imagePost": {}})),
            (
                "3".into(),
                json!({"id": "3", "author": {"uniqueId": "bad/name"}}),
            ),
            ("x".into(), json!({"id": "x"})),
            ("4".into(), Value::Null),
        ];
        let profile = "https://www.tiktok.com/@p";
        let options = Options::new(&http("tiktok", root.clone())).unwrap();
        assert_eq!(
            generate_urls(&items, profile, &options, true),
            vec![
                "https://www.tiktok.com/@a.b/video/1",
                "https://www.tiktok.com/@p/video/2",
                "https://www.tiktok.com/@p/video/3",
                "https://www.tiktok.com/@p/video/4",
            ]
        );
        let options = Options::new(&with_options(
            http("tiktok", root.clone()),
            json!({"photos": false, "audio": false, "tiktok-range": "2-3"}),
        ))
        .unwrap();
        assert_eq!(
            generate_urls(&items, profile, &options, true),
            vec!["https://www.tiktok.com/@p/video/3"]
        );
        // Stories ignore the range.
        assert_eq!(generate_urls(&items, profile, &options, false).len(), 3);
        let options = Options::new(&with_options(
            http("tiktok", root),
            json!({"videos": false}),
        ))
        .unwrap();
        assert_eq!(
            generate_urls(&items, profile, &options, true),
            vec![
                "https://www.tiktok.com/@p/video/2",
                "https://www.tiktok.com/@p/video/4"
            ]
        );
    }

    #[tokio::test]
    async fn video_post_emits_best_quality_video() {
        let body = format!(
            r#"<script id="__UNIVERSAL_DATA_FOR_REHYDRATION__" type="application/json">{}</script>"#,
            json!({"__DEFAULT_SCOPE__": detail(video_item(VIDEO_ID))})
        );
        let (root, server) = serve(vec![with_headers(
            "Set-Cookie: tt_chain_token=CHAIN123; Path=/\r\nSet-Cookie: sessionid=SECRET; Path=/\r\n",
            body.as_bytes(),
        )]);
        let http = http("tiktok", root);
        let files = media(extract(
            &http,
            Url::parse(&format!("https://www.tiktok.com/@someone/photo/{VIDEO_ID}")).unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let file = &files[0];
        assert_eq!(
            file.url.as_str(),
            "https://v19-webapp-prime.us.tiktok.com/video/tos/fhd/?a=1"
        );
        let k = keywords(file);
        assert_eq!(k["type"], "video");
        assert_eq!(k["post_type"], "video");
        assert_eq!(k["user"], "someone");
        assert_eq!(k["title"], "Hello world");
        assert_eq!(k["num"], 0);
        assert_eq!(k["file_id"], "");
        assert_eq!(k["duration"], 12);
        assert_eq!(k["width"], 576);
        assert_eq!(k["extension"], "mp4");
        assert_eq!(k["date"], "2023-11-14 22:13:20");
        assert_eq!(k["subcategory"], "post");
        assert!(k["image"].is_null());
        assert!(!k.contains_key("_fallback"));
        let (name, dir, archive) = render(file);
        assert_eq!(name, format!("{VIDEO_ID} Hello world.mp4"));
        assert_eq!(dir, std::path::PathBuf::from("tiktok/someone"));
        assert_eq!(
            archive,
            format!(r#"["{{id}}_{{num}}_{{file_id}}",["{VIDEO_ID}","0",""]]"#)
        );
        assert_eq!(file.headers["Referer"], "https://www.tiktok.com/");
        assert_eq!(file.headers["Cookie"], "tt_chain_token=CHAIN123");
        assert!(!file.headers.values().any(|v| v.contains("SECRET")));
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with(&format!("GET /@someone/video/{VIDEO_ID} ")),
            "{}",
            requests[0]
        );
        assert!(requests[0].contains("Firefox/128.0"));
    }

    #[tokio::test]
    async fn video_post_with_covers_and_subtitles() {
        let (root, _server) = serve(vec![page(detail(video_item(VIDEO_ID)))]);
        let http = with_options(
            http("tiktok", root),
            json!({"covers": "all", "subtitles": "eng-US,MT"}),
        );
        let files = media(extract(
            &http,
            Url::parse(&format!("https://www.tiktok.com/share/video/{VIDEO_ID}")).unwrap(),
        ))
        .await;
        let types: Vec<_> = files.iter().map(|f| keywords(f)["type"].clone()).collect();
        assert_eq!(
            types,
            ["video", "cover", "cover", "subtitle", "subtitle"].map(Value::from)
        );
        assert!(!files[0].headers.contains_key("Cookie"));
        let cover = keywords(&files[1]);
        assert_eq!(cover["file_id"], "cover");
        assert_eq!(cover["extension"], "jpg");
        assert_eq!(cover["filename"], "cover1~tplv");
        assert_eq!(keywords(&files[2])["file_id"], "originCover");
        let sub = keywords(&files[3]);
        assert_eq!(sub["filename"], format!("{VIDEO_ID}_eng-US_1_ASR"));
        assert_eq!(sub["extension"], "vtt");
        assert_eq!(sub["file_id"], "2_eng-US_ASR_1_webvtt");
        assert_eq!(sub["subtitle_lang_codename"], "eng-US");
        assert_eq!(keywords(&files[4])["extension"], "json");
        let (name, _, _) = render(&files[3]);
        assert_eq!(
            name,
            format!("{VIDEO_ID} Hello world [2_eng-US_ASR_1_webvtt].vtt")
        );

        // Only one cover without "all"; ASR subtitles by default.
        let (root, _server) = serve(vec![page(detail(video_item(VIDEO_ID)))]);
        let http = with_options(
            crate::sites::testing::http("tiktok", root),
            json!({"covers": true, "subtitles": true, "videos": false}),
        );
        let files = media(extract(
            &http,
            Url::parse(&format!("https://www.tiktok.com/@x/video/{VIDEO_ID}")).unwrap(),
        ))
        .await;
        let types: Vec<_> = files.iter().map(|f| keywords(f)["type"].clone()).collect();
        assert_eq!(types, ["cover", "subtitle"].map(Value::from));
        assert!(!keywords(&files[0]).contains_key("subtitle_source"));
    }

    #[tokio::test]
    async fn photo_post_emits_images_and_audio() {
        let long = "Ünïcödé ".repeat(30);
        let (root, _server) = serve(vec![page(detail(photo_item("77", &long)))]);
        let http = http("tiktok", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.tiktok.com/@someone/photo/77").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 3);
        let first = keywords(&files[0]);
        assert_eq!(first["type"], "image");
        assert_eq!(first["post_type"], "image");
        assert_eq!(first["num"], 1);
        assert_eq!(first["file_id"], "abc");
        assert_eq!(first["width"], 1080);
        assert_eq!(first["height"], 1440);
        assert_eq!(first["image"]["imageWidth"], 1080);
        assert_eq!(keywords(&files[1])["num"], 2);
        assert_eq!(keywords(&files[1])["file_id"], "def");
        let audio = keywords(&files[2]);
        assert_eq!(audio["type"], "audio");
        assert_eq!(audio["extension"], "mp3");
        assert_eq!(audio["file_id"], 555);
        assert_eq!(audio["num"], 0);
        assert_eq!(audio["duration"], 30);
        assert!(audio["image"].is_null());
        let (name, _, archive) = render(&files[0]);
        // `{title[b:150]}` keeps whole characters within 150 bytes.
        let mut title = String::new();
        for c in long.chars() {
            if title.len() + c.len_utf8() > 150 {
                break;
            }
            title.push(c);
        }
        assert!(title.len() <= 150 && title.len() > 140);
        assert_eq!(name, format!("77_01 {title} [abc].jpeg"));
        assert_eq!(archive, r#"["{id}_{num}_{file_id}",["77","1","abc"]]"#);

        let (root, _server) = serve(vec![page(detail(photo_item("78", "")))]);
        let http = with_options(
            crate::sites::testing::http("tiktok", root),
            json!({"audio": false}),
        );
        let files = media(extract(
            &http,
            Url::parse("https://www.tiktok.com/@someone/photo/78").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(keywords(&files[0])["title"], "TikTok photo #78");
    }

    #[tokio::test]
    async fn follows_canonical_url_and_solves_challenge() {
        let canonical =
            json!({"seo.abtest": {"canonical": "https://www.tiktok.com/@real/video/5"}});
        let (root, server) = serve(vec![
            page(canonical),
            html(&challenge_page(b"abc", 17)),
            page(detail(video_item("5"))),
        ]);
        let http = http("tiktok", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.tiktok.com/@/video/5").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /@/video/5 "));
        assert!(requests[1].starts_with("GET /@real/video/5 "));
        assert!(!requests[1].contains("_wafchallengeid"));
        assert!(requests[2].starts_with("GET /@real/video/5 "));
        assert!(requests[2].contains("_wafchallengeid="), "{}", requests[2]);
        assert!(requests[2].contains("waforiginalreid=RS123"));
    }

    #[tokio::test]
    async fn post_errors_do_not_leak_urls() {
        let unavailable = json!({"webapp.video-detail": {"statusCode": 10204}});
        let (root, _server) = serve(vec![page(unavailable)]);
        let http = http("tiktok", root);
        let url = Url::parse("https://www.tiktok.com/@a/video/1").unwrap();
        assert_eq!(
            errors(extract(&http, url.clone())).await,
            vec!["TikTok post: requested post not available"]
        );

        let (root, _server) = serve(vec![html("<html></html>"), html("<html></html>")]);
        let http = crate::sites::testing::http("tiktok", root);
        let found = errors(extract(&http, url.clone())).await;
        assert_eq!(found.len(), 1);
        assert!(found[0].contains("rehydration"));

        let mut item = video_item("1");
        item["video"] = json!({"playAddr": "https://evil.test/x.mp4"});
        let (root, _server) = serve(vec![page(detail(item))]);
        let http = crate::sites::testing::http("tiktok", root);
        let found = errors(extract(&http, url)).await;
        assert_eq!(found.len(), 1);
        assert!(!found[0].contains("evil"));
    }

    #[tokio::test]
    async fn avatar_uses_largest_image() {
        let (root, server) = serve(vec![page(profile("someone"))]);
        let http = http("tiktok", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.tiktok.com/@someone/avatar").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let k = keywords(&files[0]);
        assert_eq!(k["type"], "avatar");
        assert_eq!(k["title"], "@someone");
        assert_eq!(k["user"], "someone");
        assert_eq!(k["id"], "42");
        assert_eq!(k["file_id"], "av1");
        assert_eq!(k["num"], 0);
        assert_eq!(k["subcategory"], "avatar");
        let (name, dir, _) = render(&files[0]);
        assert_eq!(name, "42 @someone [av1].jpeg");
        assert_eq!(dir, std::path::PathBuf::from("tiktok/someone"));
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /@someone "));
    }

    #[tokio::test]
    async fn private_profile_fails() {
        let scope = json!({"webapp.app-context": {}, "webapp.user-detail": {"statusCode": 10222,
            "userInfo": {"stats": {"videoCount": 0}}}});
        let (root, _server) = serve(vec![page(scope)]);
        let http = http("tiktok", root);
        let found = errors(extract(
            &http,
            Url::parse("https://www.tiktok.com/@someone/posts").unwrap(),
        ))
        .await;
        assert_eq!(found.len(), 1);
        assert!(found[0].starts_with("TikTok profile: login required"));
    }

    #[tokio::test]
    async fn user_dispatches_avatar_and_posts() {
        let (root, server) = serve(vec![
            page(profile("someone")),
            page(profile("someone")),
            json(
                json!({"itemList": [{"id": "5", "author": {"uniqueId": "someone"}}],
                        "hasMorePrevious": false}),
            ),
            page(detail(video_item("5"))),
        ]);
        let http = http("tiktok", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.tiktok.com/@someone").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(keywords(&files[0])["subcategory"], "avatar");
        assert_eq!(keywords(&files[1])["subcategory"], "posts");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /@someone "));
        assert!(requests[1].starts_with("GET /@someone "));
        assert!(requests[2].starts_with("GET /api/creator/item_list/?"));
    }

    #[tokio::test]
    async fn user_include_option_selects_children() {
        let (root, server) = serve(vec![json(json!({"itemList": [], "hasMore": false}))]);
        let http = with_options(http("tiktok", root), json!({"include": "reposts"}));
        let files = media(extract(
            &http,
            Url::parse(&format!("https://www.tiktok.com/@{SEC_UID}")).unwrap(),
        ))
        .await;
        assert!(files.is_empty());
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/repost/item_list/?"));
        assert_eq!(param(&requests[0], "secUid").as_deref(), Some(SEC_UID));
    }

    #[tokio::test]
    async fn posts_paginate_legacy_endpoint() {
        let (root, server) = serve(vec![
            page(profile("someone")),
            json(json!({"itemList": [
                {"id": "11", "createTime": 1_700_000_000, "author": {"uniqueId": "someone"}},
                {"id": "12", "createTime": 1_690_000_000, "author": {"uniqueId": "other"}}],
                "hasMorePrevious": true})),
            response("200 OK", "application/json", b""),
            json(
                json!({"itemList": [{"id": "13", "createTime": 1_680_000_000,
                "imagePost": {}}], "hasMorePrevious": false}),
            ),
            page(detail(video_item("11"))),
            page(detail(video_item("12"))),
            page(detail(photo_item("13", "x"))),
        ]);
        let http = http("tiktok", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.tiktok.com/@someone/posts").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 5);
        assert_eq!(keywords(&files[0])["subcategory"], "posts");
        let requests = server.join().unwrap();
        let first = &requests[1];
        assert!(
            first.starts_with("GET /api/creator/item_list/?aid=1988&"),
            "{first}"
        );
        assert_eq!(param(first, "secUid").as_deref(), Some(SEC_UID));
        assert_eq!(param(first, "type").as_deref(), Some("1"));
        assert_eq!(param(first, "count").as_deref(), Some("15"));
        assert!(param(first, "verifyFp").unwrap().starts_with("verify_"));
        let device: u64 = param(first, "device_id").unwrap().parse().unwrap();
        assert!((7_250_000_000_000_000_000..=7_325_099_899_999_994_577).contains(&device));
        let cursor: i64 = param(first, "cursor").unwrap().parse().unwrap();
        assert!(cursor > 1_700_000_000_000);
        // The empty response is retried with the same cursor.
        assert_eq!(
            param(&requests[2], "cursor").as_deref(),
            Some("1690000000000")
        );
        assert_eq!(
            param(&requests[3], "cursor").as_deref(),
            Some("1690000000000")
        );
        assert!(requests[4].starts_with("GET /@someone/video/11 "));
        assert!(requests[5].starts_with("GET /@other/video/12 "));
        assert!(requests[6].starts_with("GET /@someone/video/13 "));
    }

    #[tokio::test]
    async fn posts_range_stops_early_and_skips_failed_posts() {
        let (root, server) = serve(vec![
            json(
                json!({"itemList": [{"id": "1"}, {"id": "2"}, {"id": "3"}, {"id": "4"}],
                        "hasMorePrevious": true}),
            ),
            page(json!({"webapp.video-detail": {"statusCode": 10204}})),
            page(detail(video_item("3"))),
        ]);
        let http = with_options(http("tiktok", root), json!({"tiktok-range": "2-3"}));
        let files = media(extract(
            &http,
            Url::parse(&format!("https://www.tiktok.com/@{SEC_UID}/posts")).unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(keywords(&files[0])["id"], "3");
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[1].starts_with(&format!("GET /@{SEC_UID}/video/2 ")));
    }

    #[tokio::test]
    async fn listing_reports_error_when_every_post_fails() {
        let (root, _server) = serve(vec![
            json(json!({"itemList": [{"id": "1"}], "hasMorePrevious": false})),
            page(json!({"webapp.video-detail": {"statusCode": 10231}})),
        ]);
        let http = http("tiktok", root);
        let found = errors(extract(
            &http,
            Url::parse(&format!("https://www.tiktok.com/@{SEC_UID}/posts")).unwrap(),
        ))
        .await;
        assert_eq!(found.len(), 1);
        assert!(found[0].contains("region locked"));
    }

    #[tokio::test]
    async fn ordered_posts_need_cookies_and_fall_back() {
        let (root, server) = serve(vec![
            json(json!({"itemList": [], "hasMore": false, "cursor": "0"})),
            json(json!({"itemList": [{"id": "1"}], "hasMorePrevious": false})),
            page(detail(video_item("1"))),
        ]);
        let http = with_options(http("tiktok", root), json!({"order-posts": "asc"}));
        http.set_cookie(&http.api(REFERER).unwrap(), "sessionid=abc; Path=/");
        let files = media(extract(
            &http,
            Url::parse(&format!("https://www.tiktok.com/@{SEC_UID}/posts")).unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/post/item_list/?"));
        assert_eq!(
            param(&requests[0], "post_item_list_request_type").as_deref(),
            Some("2")
        );
        assert_eq!(param(&requests[0], "cursor").as_deref(), Some("0"));
        assert_eq!(
            param(&requests[0], "needPinnedItemIds").as_deref(),
            Some("false")
        );
        assert!(requests[1].starts_with("GET /api/creator/item_list/?"));

        // Without cookies the order option is ignored.
        let (root, server) = serve(vec![json(json!({"hasMorePrevious": false}))]);
        let http = with_options(
            crate::sites::testing::http("tiktok", root),
            json!({"order-posts": "popular"}),
        );
        let files = media(extract(
            &http,
            Url::parse(&format!("https://www.tiktok.com/@{SEC_UID}/posts")).unwrap(),
        ))
        .await;
        assert!(files.is_empty());
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/creator/item_list/?"));
    }

    #[tokio::test]
    async fn likes_and_saved_use_time_cursor_and_detect_repeats() {
        let (root, server) = serve(vec![
            json(json!({"itemList": [{"id": "1"}], "hasMore": true, "cursor": "900"})),
            json(json!({"itemList": [{"id": "1"}], "hasMore": true, "cursor": "800"})),
            json(json!({"itemList": [{"id": "2"}], "hasMore": false, "cursor": "700"})),
            page(detail(video_item("1"))),
            page(detail(video_item("2"))),
        ]);
        let http = http("tiktok", root);
        let files = media(extract(
            &http,
            Url::parse(&format!("https://www.tiktok.com/@{SEC_UID}/liked")).unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(keywords(&files[0])["subcategory"], "likes");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/favorite/item_list/?"));
        assert_eq!(param(&requests[0], "cursor").as_deref(), Some("0"));
        assert_eq!(param(&requests[1], "cursor").as_deref(), Some("900"));
        // The repeated page regenerates the device ID and is retried.
        assert_eq!(param(&requests[2], "cursor").as_deref(), Some("900"));
        assert_ne!(
            param(&requests[1], "device_id"),
            param(&requests[2], "device_id")
        );

        let (root, server) = serve(vec![json(json!({"hasMore": false}))]);
        let http = crate::sites::testing::http("tiktok", root);
        let files = media(extract(
            &http,
            Url::parse(&format!("https://www.tiktok.com/@{SEC_UID}/saved")).unwrap(),
        ))
        .await;
        assert!(files.is_empty());
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/user/collect/item_list/?"));
    }

    #[tokio::test]
    async fn listing_fails_when_api_never_answers() {
        let empty = || response("200 OK", "application/json", b"");
        let (root, _server) = serve(vec![empty(), empty(), empty()]);
        let http = http("tiktok", root);
        let found = errors(extract(
            &http,
            Url::parse(&format!("https://www.tiktok.com/@{SEC_UID}/reposts")).unwrap(),
        ))
        .await;
        assert_eq!(found, vec!["TikTok returned invalid JSON"]);
    }

    #[tokio::test]
    async fn stories_use_author_id() {
        let (root, server) = serve(vec![
            json(
                json!({"itemList": [{"id": "9", "author": {"uniqueId": "teller"}}],
                        "HasMoreAfter": false, "MaxCursor": 5}),
            ),
            page(detail(video_item("9"))),
        ]);
        let http = http("tiktok", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.tiktok.com/@123456/stories").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/story/item_list/?"));
        assert_eq!(param(&requests[0], "authorId").as_deref(), Some("123456"));
        assert_eq!(
            param(&requests[0], "loadBackward").as_deref(),
            Some("false")
        );
        assert_eq!(param(&requests[0], "count").as_deref(), Some("5"));
        assert!(requests[1].starts_with("GET /@teller/video/9 "));
    }

    #[tokio::test]
    async fn following_batches_story_users() {
        let users: Vec<Value> = (1..=11)
            .map(|i| json!({"user": {"id": i.to_string(), "uniqueId": format!("u{i}")}}))
            .collect();
        let (root, server) = serve(vec![
            json(json!({"storyUsers": users, "hasMore": false})),
            page(json!({"webapp.app-context": {"user": {"uid": "11"}}})),
            json(json!({"batchStoryItemLists": [
                {"itemList": [{"id": "100", "author": {"uniqueId": "u1"}}]},
                {"itemList": [{"id": "101"}]}]})),
            page(detail(video_item("100"))),
            page(detail(video_item("101"))),
        ]);
        let http = http("tiktok", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.tiktok.com/following").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(keywords(&files[0])["subcategory"], "following");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/story/user_list/?"));
        assert_eq!(param(&requests[0], "storyFeedScene").as_deref(), Some("3"));
        assert!(requests[1].starts_with("GET / "));
        assert!(requests[2].starts_with("GET /api/story/batch/item_list/?"));
        assert_eq!(
            param(&requests[2], "authorIds").as_deref(),
            Some("1,2,3,4,5,6,7,8,9,10")
        );
        assert_eq!(param(&requests[2], "storyCallScene").as_deref(), Some("2"));
        assert!(param(&requests[2], "cursor").is_none());
        // The last batch holds only the current user and is skipped.
        assert_eq!(requests.len(), 5);
        assert!(requests[3].starts_with("GET /@u1/video/100 "));
        assert!(requests[4].starts_with("GET /@_/video/101 "));
    }

    #[tokio::test]
    async fn following_without_cookies_is_empty() {
        let (root, server) = serve(vec![json(json!({"statusCode": 0}))]);
        let http = http("tiktok", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.tiktok.com/following").unwrap(),
        ))
        .await;
        assert!(files.is_empty());
        assert_eq!(server.join().unwrap().len(), 1);
    }

    fn redirect(location: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 301 Moved Permanently\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .into_bytes()
    }

    #[tokio::test]
    async fn short_links_queue_the_post() {
        let (root, server) = serve(vec![
            redirect("https://www.tiktok.com/@someone/video/5?_t=abc&_r=1"),
            page(detail(video_item("5"))),
        ]);
        let http = http("tiktok", root);
        let files = media(extract(
            &http,
            Url::parse("https://vm.tiktok.com/ZMabc123/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(keywords(&files[0])["subcategory"], "post");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /ZMabc123/ "));
        assert!(
            requests[0]
                .to_ascii_lowercase()
                .contains("user-agent: facebookexternalhit/1.1")
        );
        assert!(requests[1].starts_with("GET /@someone/video/5 "));
    }

    #[tokio::test]
    async fn short_links_reject_bad_redirects() {
        for reply in [
            redirect("https://www.tiktok.com/?_r=1"),
            redirect("https://evil.test/@someone/video/5"),
            html("<html>no redirect</html>"),
            response("404 Not Found", "text/html", b""),
        ] {
            let (root, _server) = serve(vec![reply]);
            let http = http("tiktok", root);
            let found = errors(extract(
                &http,
                Url::parse("https://vt.tiktok.com/ZSx/").unwrap(),
            ))
            .await;
            assert_eq!(found.len(), 1);
            assert!(!found[0].contains("evil") && !found[0].contains("ZSx"));
        }
    }
}
