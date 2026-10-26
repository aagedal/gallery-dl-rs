//! Weibo statuses, user timelines and albums, mirroring
//! gallery_dl/extractor/weibo.py through the weibo.com `ajax` API.
//!
//! Supported options: `retweets` (also `"original"`), `text`, `videos`,
//! `livephoto`, `movies`, `gifs` (also `"video"`), `subalbums` and `include`
//! (user dispatch, with the `subalbums` alias). Without `SUB` session cookies
//! the Sina Visitor System issues guest cookies, which are kept in memory for
//! the invocation. API requests carry a browser User-Agent (Weibo answers
//! unknown clients with HTTP 403). Guest sessions can read statuses, albums
//! and video listings; the `feed` (default user dispatch), `article` and
//! `home` timelines need login cookies, as upstream. Not mirrored: resolving extension-less
//! `wblive-out.api.weibo.com` stream redirects (those files are skipped).
use crate::sites::http::Body;
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{
    Result, error,
    event::ExtractionEvent,
    extract::{ManifestProtocol, MediaJob},
};
use futures_util::{
    StreamExt, TryStreamExt,
    stream::{self, BoxStream},
};
use serde_json::{Map, Value};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use url::Url;

pub static SITE: Site = Site {
    category: "weibo",
    name: "Weibo",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[
        &USER, &HOME, &FEED, &VIDEOS, &NEWVIDEO, &ARTICLE, &ALBUM, &STATUS, &SUBALBUM,
    ],
    api_key: false,
    cookies: Some("weibo.com"),
};

const FILENAME: &str = "{status[id]}_{num:>02}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{user[screen_name]}"];
const ARCHIVE: &str = "{status[id]}_{num}";
static USER: Route = Route::new("user", FILENAME, DIRECTORY, ARCHIVE);
static HOME: Route = Route::new("home", FILENAME, DIRECTORY, ARCHIVE);
static FEED: Route = Route::new("feed", FILENAME, DIRECTORY, ARCHIVE);
static VIDEOS: Route = Route::new("videos", FILENAME, DIRECTORY, ARCHIVE);
static NEWVIDEO: Route = Route::new("newvideo", FILENAME, DIRECTORY, ARCHIVE);
static ARTICLE: Route = Route::new("article", FILENAME, DIRECTORY, ARCHIVE);
static ALBUM: Route = Route::new("album", FILENAME, DIRECTORY, ARCHIVE);
static STATUS: Route = Route::new("status", FILENAME, DIRECTORY, ARCHIVE);
/// `WeiboAlbumExtractor.items()` in subalbum mode swaps in these formats.
static SUBALBUM: Route = Route::new(
    "album",
    "{filename}.{extension}",
    &[
        "{category}",
        "{user[screen_name]}",
        "Album",
        "{subalbum[pic_title]|''}",
    ],
    "{subalbum[pic_title]}_{pid}",
);

const ROOT: &str = "https://weibo.com";
const PASSPORT: &str = "https://passport.weibo.com/visitor";
/// `text` option: statuses whose text ends like this were truncated.
const EXPAND: &str = "class=\"expand\">展开</span>";
const MAX_PAGES: usize = 100_000;
const MAX_RETRIES: usize = 3;
const NEEDS_LOGIN: &str = "Weibo requires login cookies (SUB, SUBP) for this listing; \
                           pass them with --cookies or --cookies-from-browser";

/// `SUB` values issued by the visitor system; such sessions are not logged in.
static VISITOR_SUBS: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// Serialize the visitor flow so concurrent inputs share one guest session.
static VISITOR_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Clone, Debug, PartialEq)]
enum Kind {
    User,
    Home,
    Feed,
    Videos,
    Newvideo,
    Article,
    /// The optional `tabtype=album[:_-]SUBALBUM` group.
    Album(Option<String>),
    Status,
}

#[derive(Clone, Debug, PartialEq)]
struct Target {
    kind: Kind,
    /// `u`, `n`, `p`, `profile`; `detail`, `status` or a user ID for statuses.
    prefix: Option<String>,
    /// The user name/ID, or the status ID.
    user: String,
}

fn host_ok(url: &Url) -> bool {
    crate::sites::plain(url)
        && matches!(
            url.host_str(),
            Some(
                "weibo.com"
                    | "weibo.cn"
                    | "www.weibo.com"
                    | "www.weibo.cn"
                    | "m.weibo.com"
                    | "m.weibo.cn"
            )
        )
}

/// Upstream patterns in class order: user, home, feed, videos, newvideo,
/// article, album and status. `s` is the URL after its host.
fn target(url: &Url) -> Option<Target> {
    if !host_ok(url) {
        return None;
    }
    let mut s = url.path().to_owned();
    if let Some(query) = url.query() {
        s.push('?');
        s.push_str(query);
    }
    if let Some(fragment) = url.fragment() {
        s.push('#');
        s.push_str(fragment);
    }
    user_target(&s).or_else(|| status_target(&s))
}

/// `USER_PATTERN = /(?:(u|n|p(?:rofile)?)/)?([^/?#]+)(?:/home)?` followed by
/// each listing's suffix.
fn user_target(s: &str) -> Option<Target> {
    let path = s.strip_prefix('/')?;
    let mut candidates = Vec::new();
    for prefix in ["u", "n", "p", "profile"] {
        if let Some(rest) = path
            .strip_prefix(prefix)
            .and_then(|rest| rest.strip_prefix('/'))
        {
            candidates.push((Some(prefix), rest));
        }
    }
    candidates.push((None, path));
    for (prefix, rest) in candidates {
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        if end == 0 {
            continue;
        }
        let user = &rest[..end];
        let after = &rest[end..];
        let tails = match after.strip_prefix("/home") {
            Some(tail) => vec![tail, after],
            None => vec![after],
        };
        for tail in tails {
            if let Some(kind) = listing(tail) {
                return Some(Target {
                    kind,
                    prefix: prefix.map(str::to_owned),
                    user: crate::sites::percent_decode(user),
                });
            }
        }
    }
    None
}

fn listing(tail: &str) -> Option<Kind> {
    if tail.is_empty() || tail.starts_with('#') {
        return Some(Kind::User);
    }
    let tab = tail.strip_prefix("?tabtype=")?;
    for (name, kind) in [
        ("home", Kind::Home),
        ("feed", Kind::Feed),
        ("video", Kind::Videos),
        ("newVideo", Kind::Newvideo),
        ("article", Kind::Article),
    ] {
        if tab.starts_with(name) {
            return Some(kind);
        }
    }
    let rest = tab.strip_prefix("album")?;
    let sub = rest
        .strip_prefix([':', '_', '-'])
        .map(|sub| &sub[..sub.find(['&', '#']).unwrap_or(sub.len())])
        .filter(|sub| !sub.is_empty());
    Some(Kind::Album(sub.map(str::to_owned)))
}

/// `/(detail|status|\d+)/(\w+)`.
fn status_target(s: &str) -> Option<Target> {
    let path = s.strip_prefix('/')?;
    let (prefix, rest) = path.split_once('/')?;
    if !(prefix == "detail"
        || prefix == "status"
        || (!prefix.is_empty() && prefix.bytes().all(|b| b.is_ascii_digit())))
    {
        return None;
    }
    let end = rest
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    (end > 0).then(|| Target {
        kind: Kind::Status,
        prefix: Some(prefix.to_owned()),
        user: rest[..end].to_owned(),
    })
}

/// Configuration read in upstream `_init`.
#[derive(Clone, Copy)]
struct Options {
    livephoto: bool,
    retweets: bool,
    original: bool,
    longtext: bool,
    videos: bool,
    movies: bool,
    gifs: bool,
    gifs_video: bool,
}

impl Options {
    fn new(http: &Http) -> Self {
        Self {
            livephoto: http.option_bool("livephoto", true),
            retweets: http.option_bool("retweets", false),
            original: http.option_str("retweets") == Some("original"),
            longtext: http.option_bool("text", false),
            videos: http.option_bool("videos", true),
            movies: http.option_bool("movies", false),
            gifs: http.option_bool("gifs", true),
            gifs_video: http.option_str("gifs") == Some("video"),
        }
    }
}

/// Per-extraction state.
struct Ctx<'a> {
    http: &'a Http,
    prefix: Option<String>,
    user: String,
    opts: Options,
    logged_in: bool,
    last: Mutex<Option<Instant>>,
}

fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// Python `value == 1`, which `True` also satisfies.
fn is_one(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Number(n)) => n.as_f64() == Some(1.0),
        Some(Value::Bool(b)) => *b,
        _ => false,
    }
}

fn str_of<'v>(value: &'v Value, key: &str) -> &'v str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn set_param(params: &mut Vec<(String, String)>, key: &str, value: String) {
    match params.iter_mut().find(|(k, _)| k == key) {
        Some((_, v)) => *v = value,
        None => params.push((key.into(), value)),
    }
}

fn has_param(params: &[(String, String)], key: &str) -> bool {
    params.iter().any(|(k, _)| k == key)
}

fn is_redirect(e: &crate::Error) -> bool {
    e.to_string()
        .contains("HTTP redirect requires the same origin")
}

/// A server message, bounded and without control characters.
fn message(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).take(200).collect()
}

/// A pseudo-random float in [0, 1) like Python `random.random()`.
fn random() -> f64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    f64::from(nanos.wrapping_mul(2_654_435_761) % 1_000_000_000) / 1e9
}

/// `parse_datetime(created_at, "%a %b %d %H:%M:%S %z %Y")`.
fn created(text: &str) -> Option<i64> {
    let (weekday, rest) = text.split_once(' ')?;
    if weekday.len() != 3 || !weekday.bytes().all(|b| b.is_ascii_alphabetic()) {
        return None;
    }
    crate::sites::art::strptime(rest, "%b %d %H:%M:%S %z %Y")
}

fn cookie_valid(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 2048
        && value
            .bytes()
            .all(|b| b.is_ascii_graphic() && !matches!(b, b';' | b',' | b'"' | b'\\'))
}

/// `_sina_visitor_system`: request guest `SUB`/`SUBP` cookies.
async fn visitor(http: &Http) -> Result<()> {
    let failed = || error("Weibo visitor system did not issue a session");
    let url = http.api(&format!("{PASSPORT}/genvisitor2"))?;
    let referer = format!(
        "{PASSPORT}/visitor?entry=miniblog&a=enter&url={}&domain=weibo.com",
        "https%3A%2F%2Fweibo.com%2F"
    );
    let form = [
        ("cb", "visitor_gray_callback"),
        ("ver", "20250916"),
        ("tid", ""),
        ("from", "weibo"),
        ("webdriver", "false"),
        ("return_url", "https://weibo.com/"),
    ];
    let page = http
        .post(
            &url,
            Body::Form(&form),
            &http.browser_headers(&[("Referer", referer.as_str())]),
        )
        .await?;
    let page = String::from_utf8_lossy(&page);
    let data: Value = crate::sites::extr(&page, "(", ");")
        .and_then(|json| serde_json::from_str(json).ok())
        .ok_or_else(failed)?;
    let data = data.get("data").ok_or_else(failed)?;
    let sub = str_of(data, "sub");
    let subp = str_of(data, "subp");
    if !cookie_valid(sub) || !cookie_valid(subp) {
        return Err(failed());
    }
    let mut cross = http.api(&format!("{PASSPORT}/visitor"))?;
    {
        let mut pairs = cross.query_pairs_mut();
        pairs.append_pair("a", "crossdomain");
        // requests drops a None `tid`.
        match data.get("tid") {
            None | Some(Value::Null) => {}
            Some(tid) => {
                pairs.append_pair("t", &crate::sites::scalar(tid));
            }
        }
        pairs
            .append_pair("sp", subp)
            .append_pair("s", sub)
            .append_pair("from", "weibo")
            .append_pair("_rand", &random().to_string())
            .append_pair("url", "https://weibo.com/");
    }
    let root = http.api(&format!("{ROOT}/"))?;
    let before = http.cookie(&root, "SUB");
    // The crossdomain response sets `.weibo.com` cookies and redirects to
    // weibo.com, which the shared HTTP layer refuses after storing them.
    let _ = http.get(&cross, &http.browser_headers(&[])).await;
    let after = http.cookie(&root, "SUB");
    if after.is_none() || after == before {
        let domain = match root.host() {
            Some(url::Host::Domain(host)) => format!("; Domain={host}"),
            _ => String::new(),
        };
        http.set_cookie(&root, &format!("SUB={sub}; Path=/{domain}"));
        http.set_cookie(&root, &format!("SUBP={subp}; Path=/{domain}"));
    }
    if let Some(sub) = http.cookie(&root, "SUB") {
        let mut subs = VISITOR_SUBS.lock().unwrap_or_else(|e| e.into_inner());
        if subs.len() >= 1024 {
            subs.remove(0);
        }
        subs.push(sub);
    }
    Ok(())
}

impl<'a> Ctx<'a> {
    /// `_init`: options, login state and the visitor session.
    async fn new(http: &'a Http, prefix: Option<String>, user: String) -> Result<Arc<Self>> {
        let root = http.api(&format!("{ROOT}/"))?;
        let logged_in = {
            let _guard = VISITOR_LOCK.lock().await;
            let sub = http.cookie(&root, "SUB");
            let logged_in = sub.as_ref().is_some_and(|sub| {
                http.cookie(&root, "SUBP").is_some()
                    && !VISITOR_SUBS
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .contains(sub)
            });
            if sub.is_none() {
                visitor(http).await?;
            }
            logged_in
        };
        Ok(Arc::new(Self {
            http,
            prefix,
            user,
            opts: Options::new(http),
            logged_in,
            last: Mutex::new(None),
        }))
    }

    /// Upstream `request_interval = (1.0, 2.0)`.
    async fn wait(&self) {
        if cfg!(test) {
            return;
        }
        let delay = {
            let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
            let now = Instant::now();
            let interval = Duration::from_secs_f64(1.0 + random());
            let start = last.map_or(now, |t| (t + interval).max(now));
            *last = Some(start);
            start - now
        };
        tokio::time::sleep(delay).await;
    }

    /// `request()`: a redirect away from weibo.com (to the visitor system)
    /// starts a guest session and retries once.
    async fn request(
        &self,
        url: &Url,
        headers: &[(&str, &str)],
        interval: bool,
    ) -> Result<Vec<u8>> {
        if interval {
            self.wait().await;
        }
        // Weibo answers unknown clients, and API calls without a weibo.com
        // Referer (upstream's session default), with HTTP 403.
        let mut headers = self.http.browser_headers(headers);
        let referer = format!("{ROOT}/");
        if !headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("referer"))
        {
            headers.push(("Referer", referer.as_str()));
        }
        let headers = headers.as_slice();
        match self.http.get_strict(url, headers).await {
            Err(e) if is_redirect(&e) => {
                visitor(self.http).await?;
                self.http.get_strict(url, headers).await.map_err(|e| {
                    if is_redirect(&e) {
                        error("Weibo redirected to its login page")
                    } else {
                        e
                    }
                })
            }
            result => result,
        }
    }

    async fn json(&self, url: &Url, headers: &[(&str, &str)], interval: bool) -> Result<Value> {
        let body = self.request(url, headers, interval).await?;
        serde_json::from_slice(&body).map_err(|_| error("Weibo returned invalid JSON"))
    }

    /// `_status_by_id`.
    async fn status_by_id(&self, id: &str) -> Result<Value> {
        let mut url = self.http.api(&format!("{ROOT}/ajax/statuses/show"))?;
        url.query_pairs_mut()
            .append_pair("id", id)
            .append_pair("isGetLongText", "true");
        self.json(&url, &[], true).await
    }

    /// `_user`: profile info by screen name (`/n/`) or custom name.
    async fn user_info(&self, user: &str) -> Result<Map<String, Value>> {
        let mut url = self.http.api(&format!("{ROOT}/ajax/profile/info"))?;
        let key = if self.prefix.as_deref() == Some("n") {
            "screen_name"
        } else {
            "custom"
        };
        url.query_pairs_mut().append_pair(key, user);
        self.json(&url, &[], false)
            .await?
            .get("data")
            .and_then(|data| data.get("user"))
            .and_then(Value::as_object)
            .cloned()
            .ok_or_else(|| error("Weibo user not found"))
    }

    /// `_user_id`: the last ten digits of a numeric ID, else a lookup.
    async fn user_id(&self) -> Result<String> {
        let user = &self.user;
        if user.len() >= 10 && user.bytes().all(|b| b.is_ascii_digit()) {
            return Ok(user[user.len() - 10..].to_owned());
        }
        let info = self.user_info(user).await?;
        let id = info
            .get("idstr")
            .map(crate::sites::scalar)
            .filter(|id| !id.is_empty() && !id.contains(['/', '?', '#', '&']))
            .ok_or_else(|| error("Weibo user has no ID"))?;
        Ok(id)
    }

    /// One `_pagination` page.
    async fn page(&self, mut p: Pager) -> Result<Option<(Chunk, Pager)>> {
        if p.done || p.pages >= MAX_PAGES {
            return Ok(None);
        }
        p.pages += 1;
        let uid = p
            .params
            .iter()
            .find(|(k, _)| k == "uid")
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        let referer = format!("{ROOT}/u/{uid}");
        let mut retries = 0;
        let data = loop {
            let mut url = self.http.api(&format!("{ROOT}/ajax{}", p.endpoint))?;
            url.query_pairs_mut().extend_pairs(&p.params);
            let mut headers = vec![
                ("Accept", "application/json, text/plain, */*"),
                ("X-Requested-With", "XMLHttpRequest"),
                ("Referer", referer.as_str()),
            ];
            if let Some(token) = &p.xsrf {
                headers.push(("X-XSRF-TOKEN", token.as_str()));
            }
            let data = match self.json(&url, &headers, true).await {
                // Guest sessions get HTTP 403 for `mymblog` timelines
                // (feed, article): upstream needs login cookies there too.
                Err(e) if !self.logged_in && crate::sites::http::refused_status(&e).is_some() => {
                    return Err(error(NEEDS_LOGIN));
                }
                result => result?,
            };
            p.xsrf = self
                .http
                .cookie(&url, "XSRF-TOKEN")
                .filter(|t| cookie_valid(t));
            let ok = match data.get("ok") {
                None | Some(Value::Null) => 0.0,
                Some(Value::Bool(b)) => f64::from(u8::from(*b)),
                Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
                Some(other) => f64::from(u8::from(truthy(Some(other)))),
            };
            if ok == 0.0 {
                if !has_param(&p.params, "since_id") {
                    // Guests get an empty `{}` for `myhot` (home).
                    if !self.logged_in && data.as_object().is_some_and(Map::is_empty) {
                        return Err(error(NEEDS_LOGIN));
                    }
                    let msg = data
                        .get("msg")
                        .filter(|m| truthy(Some(m)))
                        .map(crate::sites::scalar)
                        .unwrap_or_else(|| "unknown error".into());
                    return Err(error(format!("Weibo API error: \"{}\"", message(&msg))));
                }
            } else if ok < 0.0 {
                retries += 1;
                if retries > MAX_RETRIES {
                    return Err(error("Weibo API kept requesting a new session"));
                }
                let root = self.http.api(&format!("{ROOT}/"))?;
                self.request(&root, &[], true).await?;
                continue;
            }
            break data;
        };
        let Some(data) = data.get("data").filter(|d| d.is_object()) else {
            return Ok(None);
        };
        let Some(list) = data.get("list").and_then(Value::as_array).cloned() else {
            return Ok(None);
        };
        let albums = if p.subalbums {
            p.subalbums = false;
            Some(
                data.get("album_list")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default(),
            )
        } else {
            None
        };
        if let Some(cursor) = data.get("next_cursor").filter(|c| truthy(Some(c))) {
            // videos, newvideo
            if cursor.as_i64() == Some(-1) {
                p.done = true;
            } else {
                set_param(&mut p.params, "cursor", crate::sites::scalar(cursor));
            }
        } else if let Some(since) = data.get("since_id") {
            // album
            set_param(&mut p.params, p.since_key, crate::sites::scalar(since));
            if !truthy(Some(since)) {
                p.done = true;
            } else {
                bump_page(&mut p.params);
            }
        } else if has_param(&p.params, "page") {
            // home, article
            if list.is_empty() {
                p.done = true;
            } else {
                bump_page(&mut p.params);
            }
        } else {
            // feed, last album page
            match list
                .last()
                .and_then(|s| s.get("id"))
                .and_then(Value::as_i64)
            {
                Some(id) => set_param(&mut p.params, "since_id", (id - 1).to_string()),
                None => p.done = true,
            }
        }
        Ok(Some((Chunk { albums, list }, p)))
    }

    /// `items()` for one status.
    async fn status_events(
        &self,
        route: &'static Route,
        status: Value,
        extra: &Map<String, Value>,
    ) -> Result<Vec<ExtractionEvent>> {
        let Value::Object(mut status) = status else {
            return Ok(Vec::new());
        };
        let opts = self.opts;
        if status.contains_key("ori_mid") && !opts.retweets {
            return Ok(Vec::new());
        }
        let mut files = Vec::new();
        if let Some(retweeted) = status.get("retweeted_status").cloned() {
            if !opts.retweets {
                return Ok(Vec::new());
            }
            // Videos of the original post are in `status`, its images in
            // `status["retweeted_status"]`.
            extract_status(&opts, &Value::Object(status.clone()), &mut files);
            extract_status(&opts, &retweeted, &mut files);
            if opts.original {
                status = retweeted.as_object().cloned().unwrap_or_default();
            }
        } else {
            extract_status(&opts, &Value::Object(status.clone()), &mut files);
        }
        if opts.longtext
            && truthy(status.get("isLongText"))
            && status
                .get("text")
                .and_then(Value::as_str)
                .is_some_and(|text| text.ends_with(EXPAND))
        {
            let id = status
                .get("id")
                .map(crate::sites::scalar)
                .unwrap_or_default();
            if let Value::Object(full) = self.status_by_id(&id).await? {
                status = full;
            }
        }
        let ts = status
            .get("created_at")
            .and_then(Value::as_str)
            .and_then(created);
        status.insert("date".into(), crate::sites::media::date(ts));
        status.insert("count".into(), files.len().into());
        let status_id = status
            .get("id")
            .map(crate::sites::scalar)
            .unwrap_or_default();
        let screen_name = status
            .get("user")
            .and_then(|u| u.get("screen_name"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let caption = status
            .get("text_raw")
            .or_else(|| status.get("text"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let status_value = Value::Object(status.clone());
        let mut items = Vec::new();
        let mut manifests = Vec::new();
        let mut num = 0usize;
        for file in files {
            let Value::Object(mut file) = file else {
                continue;
            };
            let raw = file
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            if raw.is_empty() {
                continue;
            }
            let raw = match raw.strip_prefix("http:") {
                Some(rest) => format!("https:{rest}"),
                None => raw,
            };
            let Some(url) = crate::sites::media::web_url(&raw) else {
                continue;
            };
            if !file.contains_key("filename") {
                let (name, mut ext) = crate::sites::nameext(&url);
                if ext == "json" {
                    ext = "mp4".into();
                }
                crate::sites::media::set_nameext(&mut file, (name, ext));
            }
            let hls = file.get("extension").and_then(Value::as_str) == Some("m3u8");
            if hls {
                file.insert("_ytdl_manifest".into(), "hls".into());
                file.insert("extension".into(), "mp4".into());
            }
            num += 1;
            // The Directory message's status dictionary, then the file's.
            let mut keywords = status.clone();
            keywords.extend(file);
            keywords.insert("status".into(), status_value.clone());
            keywords.insert("num".into(), num.into());
            for (key, value) in extra {
                keywords.insert(key.clone(), value.clone());
            }
            items.push(
                Item::new(url, keywords)
                    .ids(status_id.clone(), num.to_string())
                    .user(screen_name.clone())
                    .caption(caption.clone())
                    .timestamp(ts)
                    .header("Referer", &format!("{ROOT}/")),
            );
            manifests.push(hls);
        }
        let mut events = crate::sites::post(&SITE, self.http, route, items);
        let mut hls = manifests.into_iter();
        for event in &mut events {
            if let ExtractionEvent::Media(media) = event
                && hls.next() == Some(true)
            {
                manifest(media);
            }
        }
        Ok(events)
    }
}

/// `ytdl:` with `_ytdl_manifest = "hls"`.
fn manifest(media: &mut crate::extract::Media) {
    let base = media.url.clone();
    if let Ok(url) = Url::parse(&format!("ytdl:{base}")) {
        media.url = url;
        media.job = MediaJob::Manifest {
            protocol: ManifestProtocol::Hls,
            base_url: base,
            inline_data: None,
        };
        media
            .handoff
            .insert("_ytdl_manifest".into(), Value::from("hls"));
    }
}

fn bump_page(params: &mut [(String, String)]) {
    if let Some((_, page)) = params.iter_mut().find(|(k, _)| k == "page") {
        *page = (page.parse::<i64>().unwrap_or(0) + 1).to_string();
    }
}

/// `_pagination` state.
struct Pager {
    endpoint: &'static str,
    params: Vec<(String, String)>,
    since_key: &'static str,
    subalbums: bool,
    xsrf: Option<String>,
    done: bool,
    pages: usize,
}

impl Pager {
    fn new(endpoint: &'static str, params: Vec<(&str, String)>) -> Self {
        Self {
            endpoint,
            params: params.into_iter().map(|(k, v)| (k.to_owned(), v)).collect(),
            since_key: "sinceid",
            subalbums: false,
            xsrf: None,
            done: false,
            pages: 0,
        }
    }
}

struct Chunk {
    /// The first page's `album_list` when requested.
    albums: Option<Vec<Value>>,
    list: Vec<Value>,
}

type Entries<'a> = BoxStream<'a, Result<Value>>;

fn chunks<'a>(ctx: Arc<Ctx<'a>>, pager: Pager) -> BoxStream<'a, Result<Chunk>> {
    stream::try_unfold(pager, move |p| {
        let ctx = ctx.clone();
        async move { ctx.page(p).await }
    })
    .boxed()
}

fn flatten<'a>(chunks: BoxStream<'a, Result<Chunk>>) -> Entries<'a> {
    chunks
        .map_ok(|chunk| stream::iter(chunk.list.into_iter().map(Ok)))
        .try_flatten()
        .boxed()
}

fn pagination<'a>(ctx: Arc<Ctx<'a>>, pager: Pager) -> Entries<'a> {
    flatten(chunks(ctx, pager))
}

/// Statuses through `items()`.
fn statuses_events<'a>(
    ctx: Arc<Ctx<'a>>,
    route: &'static Route,
    statuses: Entries<'a>,
    extra: Map<String, Value>,
) -> Events<'a> {
    crate::sites::pages(statuses.then(move |status| {
        let ctx = ctx.clone();
        let extra = extra.clone();
        async move {
            let events = ctx.status_events(route, status?, &extra).await?;
            Ok(crate::sites::events(events))
        }
    }))
}

/// `_extract_status`.
fn extract_status(opts: &Options, status: &Value, files: &mut Vec<Value>) {
    if let Some(mix) = status.get("mix_media_info") {
        for item in mix
            .get("items")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            match item.get("type").and_then(Value::as_str) {
                Some("video") => {
                    if opts.videos {
                        files.push(extract_video(&item["data"]["media_info"]));
                    }
                }
                Some("pic") => files.push(item["data"]["largest"].clone()),
                // Upstream warns about unknown media types.
                _ => {}
            }
        }
        return;
    }
    if let Some(ids) = status
        .get("pic_ids")
        .and_then(Value::as_array)
        .filter(|ids| !ids.is_empty())
    {
        let infos = status.get("pic_infos");
        for id in ids {
            let Some(pic) = id
                .as_str()
                .and_then(|id| infos.and_then(|infos| infos.get(id)))
            else {
                continue;
            };
            let video = || Value::Object(Map::from_iter([("url".into(), pic["video"].clone())]));
            match pic.get("type").and_then(Value::as_str) {
                Some("gif") if opts.gifs => {
                    if opts.gifs_video {
                        files.push(video());
                    } else {
                        files.push(pic["largest"].clone());
                    }
                }
                Some("livephoto") if opts.livephoto => {
                    files.push(pic["largest"].clone());
                    files.push(video());
                }
                _ => files.push(pic["largest"].clone()),
            }
        }
    }
    if let Some(info) = status.get("page_info")
        && let Some(media) = info.get("media_info")
        && opts.videos
        && (info.get("type") != Some(&Value::from("5")) || opts.movies)
    {
        files.push(extract_video(media));
    }
}

/// `_extract_video`: the highest `quality_index` playback, else a replay or
/// stream URL.
fn extract_video(info: &Value) -> Value {
    let empty = || Value::Object(Map::from_iter([("url".into(), Value::from(""))]));
    if info.get("live_status").and_then(Value::as_i64) == Some(1) {
        // Ongoing live streams are skipped.
        return empty();
    }
    let best = info
        .get("playback_list")
        .and_then(Value::as_array)
        .filter(|list| !list.is_empty())
        .and_then(|list| {
            let mut best: Option<(f64, &Value)> = None;
            for media in list {
                let quality = media.get("meta")?.get("quality_index")?.as_f64()?;
                if best.is_none_or(|(q, _)| quality > q) {
                    best = Some((quality, media));
                }
            }
            best
        });
    let mut video = match best {
        Some((_, media)) => media.get("play_info").cloned().unwrap_or_else(empty),
        None => {
            let url = ["replay_hd", "stream_url_hd", "stream_url"]
                .iter()
                .find_map(|key| info.get(*key).filter(|v| truthy(Some(v))).cloned())
                .unwrap_or_else(|| Value::from(""));
            Value::Object(Map::from_iter([("url".into(), url)]))
        }
    };
    let url = str_of(&video, "url");
    if url.contains("//wblive-out.")
        && Url::parse(url).map_or(true, |u| crate::sites::nameext(&u).1.is_empty())
    {
        // Upstream resolves the redirect `Location`; the shared HTTP layer
        // does not expose cross-origin redirects, so the file is skipped.
        if let Some(map) = video.as_object_mut() {
            map.insert("url".into(), "".into());
        }
    }
    video
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(Target { kind, prefix, user }) = target(&url) else {
        return crate::sites::fail(error("unsupported Weibo URL"));
    };
    crate::sites::once(async move {
        let ctx = Ctx::new(http, prefix, user).await?;
        let listing = |route, endpoint, params| {
            statuses_events(
                ctx.clone(),
                route,
                pagination(ctx.clone(), Pager::new(endpoint, params)),
                Map::new(),
            )
        };
        Ok(match kind {
            Kind::User => dispatch(ctx.clone()).await?,
            Kind::Home => {
                let uid = ctx.user_id().await?;
                listing(
                    &HOME,
                    "/profile/myhot",
                    vec![("uid", uid), ("page", "1".into()), ("feature", "2".into())],
                )
            }
            Kind::Feed => {
                let uid = ctx.user_id().await?;
                let mut params = vec![("uid", uid), ("feature", "0".into())];
                if ctx.logged_in {
                    params.push(("page", "1".into()));
                }
                listing(&FEED, "/statuses/mymblog", params)
            }
            Kind::Videos => {
                let uid = ctx.user_id().await?;
                let entries = pagination(
                    ctx.clone(),
                    Pager::new("/profile/getprofilevideolist", vec![("uid", uid)]),
                )
                .try_filter_map(|status| async move { Ok(status.get("video_detail_vo").cloned()) })
                .boxed();
                statuses_events(ctx.clone(), &VIDEOS, entries, Map::new())
            }
            Kind::Newvideo => {
                let uid = ctx.user_id().await?;
                listing(
                    &NEWVIDEO,
                    "/profile/getWaterFallContent",
                    vec![("uid", uid)],
                )
            }
            Kind::Article => {
                let uid = ctx.user_id().await?;
                listing(
                    &ARTICLE,
                    "/statuses/mymblog",
                    vec![("uid", uid), ("page", "1".into()), ("feature", "10".into())],
                )
            }
            Kind::Album(sub) => {
                if sub.is_none() && !http.option_bool("subalbums", false) {
                    album(ctx.clone()).await?
                } else {
                    subalbums(ctx.clone(), sub).await?
                }
            }
            Kind::Status => {
                let status = ctx.status_by_id(&ctx.user).await?;
                if !is_one(status.get("ok")) {
                    return Err(error("Weibo status not found"));
                }
                crate::sites::events(ctx.status_events(&STATUS, status, &Map::new()).await?)
            }
        })
    })
}

/// `WeiboUserExtractor.items()`: `Dispatch._dispatch_extractors` with the
/// default `include` of `feed` and the `subalbums` alias.
async fn dispatch<'a>(ctx: Arc<Ctx<'a>>) -> Result<Events<'a>> {
    const ALL: &[(&str, &str)] = &[
        ("home", "home"),
        ("feed", "feed"),
        ("videos", "video"),
        ("newvideo", "newVideo"),
        ("article", "article"),
        ("album", "album"),
    ];
    let http = ctx.http;
    let uid = ctx.user_id().await?;
    let include = match http.option("include") {
        None => crate::dispatch::Include::Views(vec!["feed".into()]),
        Some(value) => crate::dispatch::Include::parse(value)?,
    };
    let (names, alias) = match include {
        crate::dispatch::Include::All => {
            (ALL.iter().map(|(n, _)| (*n).to_owned()).collect(), false)
        }
        crate::dispatch::Include::Views(views) => (views, true),
    };
    let mut children = Vec::new();
    for name in names {
        let tab = match ALL.iter().find(|(n, _)| *n == name) {
            Some((_, tab)) => *tab,
            None if alias && name == "subalbums" => "album-only",
            // Upstream warns about invalid includes.
            None => continue,
        };
        let url = Url::parse(&format!("{ROOT}/u/{uid}?tabtype={tab}"))
            .map_err(|_| error("invalid Weibo user URL"))?;
        children.push(crate::sites::queue(
            http,
            SITE.category,
            USER.subcategory,
            url,
            &Map::new(),
        ));
    }
    Ok(stream::iter(children).flatten().boxed())
}

/// `WeiboAlbumExtractor.statuses()`: each image wall entry's status once.
async fn album<'a>(ctx: Arc<Ctx<'a>>) -> Result<Events<'a>> {
    let uid = ctx.user_id().await?;
    let seen = Arc::new(Mutex::new(std::collections::HashSet::new()));
    let fetch = ctx.clone();
    let statuses = pagination(
        ctx.clone(),
        Pager::new("/profile/getImageWall", vec![("uid", uid)]),
    )
    .try_filter_map(move |image| {
        let ctx = fetch.clone();
        let seen = seen.clone();
        async move {
            let mid = image
                .get("mid")
                .map(crate::sites::scalar)
                .unwrap_or_default();
            if !seen
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(mid.clone())
            {
                return Ok(None);
            }
            let status = ctx.status_by_id(&mid).await?;
            Ok(is_one(status.get("ok")).then_some(status))
        }
    })
    .boxed();
    Ok(statuses_events(ctx, &ALBUM, statuses, Map::new()))
}

/// `items_subalbum`: image wall and subalbum files by `pid`, or by status.
async fn subalbums<'a>(ctx: Arc<Ctx<'a>>, sub: Option<String>) -> Result<Events<'a>> {
    let user = ctx.user_info(&ctx.user).await?;
    let uid = user
        .get("idstr")
        .map(crate::sites::scalar)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| error("Weibo user has no ID"))?;
    let mut pager = Pager::new(
        "/profile/getImageWall",
        vec![
            ("uid", uid.clone()),
            ("sinceid", "0".into()),
            ("has_album", "true".into()),
        ],
    );
    pager.subalbums = true;
    let mut wall = chunks(ctx.clone(), pager);
    let (list, first) = match wall.next().await.transpose()? {
        Some(chunk) => (chunk.albums.unwrap_or_default(), chunk.list),
        None => (Vec::new(), Vec::new()),
    };
    let main: Entries<'a> = stream::iter(first.into_iter().map(Ok))
        .chain(flatten(wall))
        .boxed();
    let detail = |sub: &Value| -> (Map<String, Value>, Entries<'a>) {
        let mut pager = Pager::new(
            "/profile/getAlbumDetail",
            vec![
                ("uid", uid.clone()),
                (
                    "containerid",
                    crate::sites::percent_decode(str_of(sub, "containerid")),
                ),
            ],
        );
        pager.since_key = "since_id";
        (
            sub.as_object().cloned().unwrap_or_default(),
            pagination(ctx.clone(), pager),
        )
    };
    let results: Vec<(Map<String, Value>, Entries<'a>)> = match sub.as_deref() {
        None | Some("0") => vec![(Map::new(), main)],
        Some("all") => {
            let mut results: Vec<_> = list.iter().map(detail).collect();
            results.push((Map::new(), main));
            results
        }
        Some("only") => list.iter().map(detail).collect(),
        Some(sub) => {
            let found = if !sub.is_empty() && sub.bytes().all(|b| b.is_ascii_digit()) {
                sub.parse::<usize>()
                    .ok()
                    .and_then(|i| i.checked_sub(1))
                    .and_then(|i| list.get(i))
            } else {
                let title = crate::sites::percent_decode(sub);
                list.iter().find(|s| str_of(s, "pic_title") == title)
            };
            vec![detail(
                found.ok_or_else(|| error("Weibo subalbum not found"))?,
            )]
        }
    };
    let user = Value::Object(user);
    Ok(stream::iter(results)
        .map(move |(subalbum, entries)| {
            let mut extra = Map::new();
            extra.insert("user".into(), user.clone());
            extra.insert("subalbum".into(), Value::Object(subalbum));
            subalbum_events(ctx.clone(), entries, extra)
        })
        .flatten()
        .boxed())
}

fn subalbum_events<'a>(
    ctx: Arc<Ctx<'a>>,
    entries: Entries<'a>,
    extra: Map<String, Value>,
) -> Events<'a> {
    crate::sites::pages(entries.then(move |file| {
        let ctx = ctx.clone();
        let extra = extra.clone();
        async move {
            let Value::Object(mut file) = file? else {
                return Ok(crate::sites::events(Vec::new()));
            };
            if let Some(pid) = file.get("pid").map(crate::sites::scalar) {
                let Ok(url) = Url::parse(&format!("{ROOT}/ajax/common/download?pid={pid}")) else {
                    return Ok(crate::sites::events(Vec::new()));
                };
                file.insert("filename".into(), pid.clone().into());
                file.insert("extension".into(), "jpg".into());
                file.extend(extra.clone());
                let username = extra
                    .get("user")
                    .and_then(|u| u.get("screen_name"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let item = Item::new(url, file)
                    .ids(pid.clone(), pid)
                    .user(username)
                    .header("Referer", &format!("{ROOT}/"));
                return Ok(crate::sites::events(crate::sites::post(
                    &SITE,
                    ctx.http,
                    &SUBALBUM,
                    vec![item],
                )));
            }
            if let Some(mid) = file.get("mid").map(crate::sites::scalar) {
                let status = ctx.status_by_id(&mid).await?;
                if is_one(status.get("ok")) {
                    return Ok(crate::sites::events(
                        ctx.status_events(&SUBALBUM, status, &extra).await?,
                    ));
                }
            }
            Ok(crate::sites::events(Vec::new()))
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, json, keywords, media, response, serve};
    use serde_json::json;
    use std::path::PathBuf;

    fn t(raw: &str) -> Option<Target> {
        target(&Url::parse(raw).unwrap())
    }

    fn kind(raw: &str) -> Option<Kind> {
        t(raw).map(|t| t.kind)
    }

    /// A logged-in session (cookies loaded from `--cookies`).
    fn session(root: Url) -> Http {
        let http = http("Weibo", root);
        let base = http.api("https://weibo.com/").unwrap();
        http.set_cookie(&base, "SUB=login-sub; Path=/");
        http.set_cookie(&base, "SUBP=login-subp; Path=/");
        http
    }

    fn with(http: Http, options: Value) -> Http {
        http.with_options(Some(Arc::new(options.as_object().unwrap().clone())))
    }

    fn line(request: &str) -> &str {
        request.lines().next().unwrap()
    }

    async fn error_text(events: Events<'_>) -> String {
        crate::event::media_only(events)
            .filter_map(|m| async move { m.err().map(|e| e.to_string()) })
            .boxed()
            .next()
            .await
            .expect("an error")
    }

    fn status(id: u64) -> Value {
        json!({
            "ok": 1,
            "id": id,
            "created_at": "Sun Dec 30 21:56:36 +0800 2018",
            "user": {"screen_name": "Name", "idstr": "1758989602"},
            "text_raw": "hello",
            "pic_ids": [format!("p{id}")],
            "pic_infos": {format!("p{id}"): {
                "type": "pic",
                "largest": {"url": format!("https://wx1.sinaimg.cn/large/p{id}.jpg")},
            }},
        })
    }

    fn urls(files: &[crate::extract::Media]) -> Vec<String> {
        files.iter().map(|m| m.url.to_string()).collect()
    }

    #[test]
    fn classifies_urls() {
        let user = |raw| t(raw).map(|t| (t.kind, t.prefix, t.user));
        assert_eq!(
            user("https://weibo.com/1758989602"),
            Some((Kind::User, None, "1758989602".into()))
        );
        assert_eq!(
            user("https://www.weibo.com/n/周于希Sally"),
            Some((Kind::User, Some("n".into()), "周于希Sally".into()))
        );
        assert_eq!(
            user("https://weibo.com/u/1758989602"),
            Some((Kind::User, Some("u".into()), "1758989602".into()))
        );
        assert_eq!(
            user("https://weibo.com/p/1758989602"),
            Some((Kind::User, Some("p".into()), "1758989602".into()))
        );
        assert_eq!(
            user("https://m.weibo.cn/profile/2314621010"),
            Some((Kind::User, Some("profile".into()), "2314621010".into()))
        );
        assert_eq!(
            user("https://m.weibo.cn/p/2304132314621010_-_WEIBO_SECOND_PROFILE_WEIBO"),
            Some((
                Kind::User,
                Some("p".into()),
                "2304132314621010_-_WEIBO_SECOND_PROFILE_WEIBO".into()
            ))
        );
        assert_eq!(
            user("https://www.weibo.com/p/1003062314621010/home"),
            Some((Kind::User, Some("p".into()), "1003062314621010".into()))
        );
        assert_eq!(kind("https://weibo.com/zhouyuxi77#top"), Some(Kind::User));
        assert_eq!(
            user("http://weibo.cn/u"),
            Some((Kind::User, None, "u".into()))
        );
        assert_eq!(
            kind("https://weibo.com/1758989602?tabtype=home"),
            Some(Kind::Home)
        );
        assert_eq!(
            kind("https://weibo.com/u/7500315942/home?tabtype=feed"),
            Some(Kind::Feed)
        );
        assert_eq!(
            user("https://www.weibo.com/n/周于希Sally?tabtype=feed"),
            Some((Kind::Feed, Some("n".into()), "周于希Sally".into()))
        );
        assert_eq!(
            kind("https://weibo.com/1758989602?tabtype=video"),
            Some(Kind::Videos)
        );
        assert_eq!(
            kind("https://weibo.com/1758989602?tabtype=newVideo"),
            Some(Kind::Newvideo)
        );
        assert_eq!(
            kind("https://weibo.com/1758989602?tabtype=article"),
            Some(Kind::Article)
        );
        assert_eq!(
            kind("https://weibo.com/1758989602?tabtype=album"),
            Some(Kind::Album(None))
        );
        assert_eq!(
            kind("https://weibo.com/u/2142058927?tabtype=album-头像"),
            Some(Kind::Album(Some("%E5%A4%B4%E5%83%8F".into())))
        );
        assert_eq!(
            kind("https://weibo.com/u/1?tabtype=album_only&x=1"),
            Some(Kind::Album(Some("only".into())))
        );
        assert_eq!(
            kind("https://weibo.com/u/1?tabtype=album:2#x"),
            Some(Kind::Album(Some("2".into())))
        );
        // Unknown tab types match no listing.
        assert_eq!(kind("https://weibo.com/u/1?tabtype=likes"), None);
        assert_eq!(kind("https://weibo.com/u/1?other=1"), None);

        let status = |raw| t(raw).map(|t| (t.kind, t.prefix, t.user));
        assert_eq!(
            status("https://m.weibo.cn/detail/4323047042991618"),
            Some((
                Kind::Status,
                Some("detail".into()),
                "4323047042991618".into()
            ))
        );
        assert_eq!(
            status("https://m.weibo.cn/status/4268682979207023"),
            Some((
                Kind::Status,
                Some("status".into()),
                "4268682979207023".into()
            ))
        );
        assert_eq!(
            status("https://weibo.com/3314883543/Iy7fj4qVg?x=1"),
            Some((Kind::Status, Some("3314883543".into()), "Iy7fj4qVg".into()))
        );
        assert_eq!(
            status("https://m.weibo.cn/5746766133/4339748116375525"),
            Some((
                Kind::Status,
                Some("5746766133".into()),
                "4339748116375525".into()
            ))
        );
        assert_eq!(
            kind("https://weibo.cn/detail/4600272267522211"),
            Some(Kind::Status)
        );
        // `/home` after a numeric user is the user profile, not a status.
        assert_eq!(kind("https://weibo.com/1758989602/home"), Some(Kind::User));

        for raw in [
            "https://weibo.com/",
            "https://weibo.com/1758989602/",
            "https://weibo.com/u/",
            "https://weibo.com/abc/def",
            "https://weibo.com:8080/1758989602",
            "https://s.weibo.com/1758989602",
            "https://weibo.org/1758989602",
            "https://notweibo.com/1758989602",
            "ftp://weibo.com/1758989602",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
        assert!((SITE.matches)(
            &Url::parse("https://weibo.com/1758989602").unwrap()
        ));
    }

    #[test]
    fn parses_created_at() {
        assert_eq!(
            crate::sites::media::date(created("Sun Dec 30 21:56:36 +0800 2018")),
            json!("2018-12-30 13:56:36")
        );
        assert_eq!(created("Dec 30 21:56:36 +0800 2018"), None);
    }

    #[tokio::test]
    async fn status_files_and_keywords() {
        let mut body = status(4323047042991618);
        body["pic_ids"] = json!(["a", "b", "c"]);
        body["pic_infos"] = json!({
            "a": {"type": "pic", "largest": {"url": "http://wx1.sinaimg.cn/large/a.jpg", "width": 9}},
            "b": {"type": "gif", "largest": {"url": "https://wx1.sinaimg.cn/large/b.gif"},
                  "video": "https://f.video.weibocdn.com/b.mp4"},
            "c": {"type": "livephoto", "largest": {"url": "https://wx1.sinaimg.cn/large/c.jpg"},
                  "video": "https://livephoto.us.sinaimg.cn/c.mov"},
        });
        body["page_info"] = json!({"type": "11", "media_info": {"playback_list": [
            {"meta": {"quality_index": 1}, "play_info": {"url": "https://f.video.weibocdn.com/low.mp4"}},
            {"meta": {"quality_index": 3}, "play_info": {
                "url": "https://f.video.weibocdn.com/o0/high.mp4?label=mp4_1080p", "label": "mp4_1080p"}},
            {"meta": {"quality_index": 2}, "play_info": {"url": "https://f.video.weibocdn.com/mid.mp4"}},
        ]}});
        let (root, server) = serve(vec![json(body)]);
        let http = session(root);
        let files = media(extract(
            &http,
            Url::parse("https://m.weibo.cn/detail/4323047042991618").unwrap(),
        ))
        .await;
        assert_eq!(
            urls(&files),
            [
                "https://wx1.sinaimg.cn/large/a.jpg",
                "https://wx1.sinaimg.cn/large/b.gif",
                "https://wx1.sinaimg.cn/large/c.jpg",
                "https://livephoto.us.sinaimg.cn/c.mov",
                "https://f.video.weibocdn.com/o0/high.mp4?label=mp4_1080p",
            ]
        );
        let k = keywords(&files[0]);
        assert_eq!(k["subcategory"], "status");
        assert_eq!(k["num"], 1);
        assert_eq!(k["width"], 9);
        assert_eq!(k["status"]["count"], 5);
        assert_eq!(k["status"]["date"], "2018-12-30 13:56:36");
        assert_eq!(k["status"]["id"], 4323047042991618u64);
        assert_eq!(k["user"]["screen_name"], "Name");
        assert_eq!(keywords(&files[4])["label"], "mp4_1080p");
        assert_eq!(keywords(&files[4])["num"], 5);
        assert_eq!(files[0].headers["Referer"], "https://weibo.com/");
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "4323047042991618_02.gif");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            PathBuf::from("weibo/Name")
        );
        let archive = crate::sites::archive_key(&files[4]).unwrap();
        assert!(
            archive.1.contains("\"4323047042991618\",\"5\""),
            "{archive:?}"
        );
        let requests = server.join().unwrap();
        assert!(
            line(&requests[0])
                .starts_with("GET /ajax/statuses/show?id=4323047042991618&isGetLongText=true ")
        );
        assert!(requests[0].contains("SUB=login-sub"));
    }

    #[tokio::test]
    async fn media_options() {
        let mut body = status(1);
        body["pic_ids"] = json!(["b", "c"]);
        body["pic_infos"] = json!({
            "b": {"type": "gif", "largest": {"url": "https://wx1.sinaimg.cn/large/b.gif"},
                  "video": "https://f.video.weibocdn.com/b.mp4"},
            "c": {"type": "livephoto", "largest": {"url": "https://wx1.sinaimg.cn/large/c.jpg"},
                  "video": "https://livephoto.us.sinaimg.cn/c.mov"},
        });
        body["page_info"] = json!({"type": "5", "media_info": {"stream_url": "https://f.video.weibocdn.com/movie.mp4"}});
        let (root, _server) = serve(vec![
            json(body.clone()),
            json(body.clone()),
            json(body.clone()),
            json(body),
        ]);
        let url = Url::parse("https://weibo.com/detail/1").unwrap();
        // Defaults: gif image, live photo image and video, no movie.
        let http = session(root.clone());
        assert_eq!(
            urls(&media(extract(&http, url.clone())).await),
            [
                "https://wx1.sinaimg.cn/large/b.gif",
                "https://wx1.sinaimg.cn/large/c.jpg",
                "https://livephoto.us.sinaimg.cn/c.mov",
            ]
        );
        let http = with(
            session(root.clone()),
            json!({"gifs": "video", "livephoto": false}),
        );
        assert_eq!(
            urls(&media(extract(&http, url.clone())).await),
            [
                "https://f.video.weibocdn.com/b.mp4",
                "https://wx1.sinaimg.cn/large/c.jpg"
            ]
        );
        // `movies` enables type-5 videos; `gifs: false` keeps the image.
        let http = with(
            session(root.clone()),
            json!({"movies": true, "gifs": false}),
        );
        assert_eq!(
            urls(&media(extract(&http, url.clone())).await),
            [
                "https://wx1.sinaimg.cn/large/b.gif",
                "https://wx1.sinaimg.cn/large/c.jpg",
                "https://livephoto.us.sinaimg.cn/c.mov",
                "https://f.video.weibocdn.com/movie.mp4",
            ]
        );
        let http = with(session(root), json!({"movies": true, "videos": false}));
        assert_eq!(media(extract(&http, url)).await.len(), 3);
    }

    #[tokio::test]
    async fn mixed_media_manifests_and_skipped_streams() {
        let mut mixed = status(2);
        mixed["mix_media_info"] = json!({"items": [
            {"type": "pic", "data": {"largest": {"url": "https://wx1.sinaimg.cn/large/m.jpg"}}},
            {"type": "video", "data": {"media_info": {
                "playback_list": [], "stream_url_hd": "",
                "stream_url": "https://f.video.weibocdn.com/v.json"}}},
            {"type": "other", "data": {}},
        ]});
        let mut live = status(3);
        live["pic_ids"] = json!([]);
        live["page_info"] = json!({"media_info": {
            "replay_hd": "https://live.video.weibocdn.com/x_index.m3u8?a=1"}});
        let mut skipped = status(4);
        skipped["mix_media_info"] = json!({"items": [
            {"type": "video", "data": {"media_info": {"live_status": 1,
                "stream_url": "https://f.video.weibocdn.com/live.mp4"}}},
            {"type": "video", "data": {"media_info": {
                "stream_url": "https://wblive-out.api.weibo.com/2/wblive/room/play?id=1"}}},
            {"type": "pic", "data": {"largest": {"url": "https://wx1.sinaimg.cn/large/k.jpg"}}},
        ]});
        let (root, _server) = serve(vec![json(mixed), json(live), json(skipped)]);
        let http = session(root);
        let files = media(extract(
            &http,
            Url::parse("https://weibo.com/detail/2").unwrap(),
        ))
        .await;
        assert_eq!(
            urls(&files),
            [
                "https://wx1.sinaimg.cn/large/m.jpg",
                "https://f.video.weibocdn.com/v.json"
            ]
        );
        assert_eq!(keywords(&files[1])["extension"], "mp4");
        assert_eq!(keywords(&files[1])["filename"], "v");

        let files = media(extract(
            &http,
            Url::parse("https://weibo.com/detail/3").unwrap(),
        ))
        .await;
        assert_eq!(
            urls(&files),
            ["ytdl:https://live.video.weibocdn.com/x_index.m3u8?a=1"]
        );
        assert!(matches!(
            &files[0].job,
            MediaJob::Manifest { protocol: ManifestProtocol::Hls, base_url, .. }
                if base_url.as_str() == "https://live.video.weibocdn.com/x_index.m3u8?a=1"
        ));
        assert_eq!(keywords(&files[0])["extension"], "mp4");
        assert_eq!(keywords(&files[0])["_ytdl_manifest"], "hls");
        assert_eq!(files[0].handoff["_ytdl_manifest"], "hls");

        let files = media(extract(
            &http,
            Url::parse("https://weibo.com/detail/4").unwrap(),
        ))
        .await;
        assert_eq!(urls(&files), ["https://wx1.sinaimg.cn/large/k.jpg"]);
        assert_eq!(keywords(&files[0])["num"], 1);
        assert_eq!(keywords(&files[0])["status"]["count"], 3);
    }

    #[tokio::test]
    async fn status_not_found() {
        let (root, _server) = serve(vec![json(json!({"ok": 0, "message": "gone"}))]);
        let http = session(root);
        let text = error_text(extract(
            &http,
            Url::parse("https://m.weibo.cn/status/4268682979207023").unwrap(),
        ))
        .await;
        assert_eq!(text, "Weibo status not found");
    }

    #[tokio::test]
    async fn retweets() {
        let mut body = status(10);
        body["user"]["screen_name"] = json!("Retweeter");
        body["pic_ids"] = json!([]);
        body["page_info"] =
            json!({"media_info": {"stream_url": "https://f.video.weibocdn.com/rt.mp4"}});
        body["retweeted_status"] = status(11);
        body["retweeted_status"]["user"]["screen_name"] = json!("Author");
        let mut quick = status(12);
        quick["ori_mid"] = json!("11");
        let (root, server) = serve(vec![
            json(body.clone()),
            json(quick.clone()),
            json(body.clone()),
            json(quick),
            json(body),
        ]);
        let url = Url::parse("https://weibo.cn/detail/10").unwrap();
        let quick_url = Url::parse("https://weibo.cn/detail/12").unwrap();
        let http = session(root.clone());
        assert!(media(extract(&http, url.clone())).await.is_empty());
        assert!(media(extract(&http, quick_url.clone())).await.is_empty());

        let http = with(session(root.clone()), json!({"retweets": true}));
        let files = media(extract(&http, url.clone())).await;
        assert_eq!(
            urls(&files),
            [
                "https://f.video.weibocdn.com/rt.mp4",
                "https://wx1.sinaimg.cn/large/p11.jpg"
            ]
        );
        assert_eq!(keywords(&files[0])["status"]["id"], 10);
        assert_eq!(keywords(&files[0])["user"]["screen_name"], "Retweeter");
        assert_eq!(media(extract(&http, quick_url)).await.len(), 1);

        let http = with(session(root), json!({"retweets": "original"}));
        let files = media(extract(&http, url)).await;
        assert_eq!(files.len(), 2);
        assert_eq!(keywords(&files[1])["status"]["id"], 11);
        assert_eq!(keywords(&files[1])["user"]["screen_name"], "Author");
        assert_eq!(server.join().unwrap().len(), 5);
    }

    #[tokio::test]
    async fn long_text() {
        let mut short = status(20);
        short["isLongText"] = json!(true);
        short["text"] = json!("abc<span class=\"expand\">展开</span>");
        let mut full = status(20);
        full["text"] = json!("abc def");
        full["longText"] = json!({"content": "abc def"});
        let (root, server) = serve(vec![json(short.clone()), json(full), json(short)]);
        let url = Url::parse("https://weibo.com/detail/20").unwrap();
        let http = with(session(root.clone()), json!({"text": true}));
        let files = media(extract(&http, url.clone())).await;
        assert_eq!(keywords(&files[0])["status"]["text"], "abc def");
        assert_eq!(
            keywords(&files[0])["status"]["longText"]["content"],
            "abc def"
        );
        let http = session(root);
        let files = media(extract(&http, url)).await;
        assert!(
            keywords(&files[0])["status"]["text"]
                .as_str()
                .unwrap()
                .ends_with(EXPAND)
        );
        assert_eq!(server.join().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn visitor_session_and_feed_since_id_pagination() {
        let callback = "window.visitor_gray_callback && visitor_gray_callback(\
            {\"retcode\":20000000,\"msg\":\"succ\",\"data\":{\"sub\":\"_2AkVisitorSub\",\
            \"subp\":\"0033VisitorSubp\",\"tid\":\"tid1\"}});";
        let (root, server) = serve(vec![
            response("200 OK", "application/javascript", callback.as_bytes()),
            html("<html></html>"),
            json(json!({"data": {"user": {"idstr": "2553930725", "screen_name": "Z"}}})),
            json(json!({"ok": 1, "data": {"list": [status(100), status(90)]}})),
            json(json!({"ok": 0})),
        ]);
        let http = http("Weibo", root);
        let files = media(extract(
            &http,
            Url::parse("https://weibo.com/zhouyuxi77?tabtype=feed").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(keywords(&files[0])["subcategory"], "feed");
        let requests = server.join().unwrap();
        assert!(line(&requests[0]).starts_with("POST /visitor/genvisitor2 "));
        assert!(requests[0].contains("referer: https://passport.weibo.com/visitor/visitor?"));
        let form = requests[0].split("\r\n\r\n").nth(1).unwrap();
        assert!(
            form.starts_with("cb=visitor_gray_callback&ver=20250916&tid=&from=weibo"),
            "{form}"
        );
        assert!(line(&requests[1]).starts_with(
            "GET /visitor/visitor?a=crossdomain&t=tid1&sp=0033VisitorSubp&s=_2AkVisitorSub&from=weibo&_rand="
        ));
        assert!(line(&requests[2]).starts_with("GET /ajax/profile/info?custom=zhouyuxi77 "));
        assert!(requests[2].contains("SUB=_2AkVisitorSub; SUBP=0033VisitorSubp"));
        // A visitor session is not logged in: no `page` parameter.
        assert!(
            line(&requests[3]).starts_with("GET /ajax/statuses/mymblog?uid=2553930725&feature=0 ")
        );
        assert!(requests[3].contains("x-requested-with: XMLHttpRequest"));
        assert!(requests[3].contains("referer: https://weibo.com/u/2553930725"));
        assert!(
            line(&requests[4])
                .starts_with("GET /ajax/statuses/mymblog?uid=2553930725&feature=0&since_id=89 ")
        );
    }

    #[tokio::test]
    async fn visitor_cookies_from_crossdomain_response() {
        let callback = "cb({\"data\":{\"sub\":\"_2AkGenSub\",\"subp\":\"GenSubp\"}});";
        let cross = b"HTTP/1.1 200 OK\r\nSet-Cookie: SUB=_2AkCrossSub; Path=/\r\n\
            Set-Cookie: SUBP=CrossSubp; Path=/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let (root, server) = serve(vec![
            response("200 OK", "text/plain", callback.as_bytes()),
            cross.to_vec(),
            json(status(5)),
        ]);
        let http = http("Weibo", root);
        let files = media(extract(
            &http,
            Url::parse("https://weibo.com/detail/5").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let requests = server.join().unwrap();
        // A missing `tid` is omitted like requests' `None` parameters.
        assert!(line(&requests[1]).starts_with("GET /visitor/visitor?a=crossdomain&sp=GenSubp&s="));
        assert!(requests[2].contains("SUB=_2AkCrossSub; SUBP=CrossSubp"));
        // API calls carry a browser User-Agent and a weibo.com Referer.
        assert!(requests[2].contains("referer: https://weibo.com/\r\n"));
        assert!(requests[2].contains("user-agent: Mozilla/5.0"));
    }

    #[tokio::test]
    async fn redirect_to_passport_starts_visitor_session() {
        let redirect = b"HTTP/1.1 302 Found\r\n\
            Location: https://passport.weibo.com/visitor/visitor?entry=miniblog\r\n\
            Content-Length: 0\r\nConnection: close\r\n\r\n";
        let callback =
            "cb({\"data\":{\"sub\":\"_2AkRedirSub\",\"subp\":\"RedirSubp\",\"tid\":\"t\"}});";
        let (root, server) = serve(vec![
            redirect.to_vec(),
            response("200 OK", "text/plain", callback.as_bytes()),
            html(""),
            json(status(6)),
        ]);
        let http = session(root);
        let files = media(extract(
            &http,
            Url::parse("https://weibo.com/detail/6").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let requests = server.join().unwrap();
        assert!(requests[0].contains("SUB=login-sub"));
        assert!(line(&requests[1]).starts_with("POST /visitor/genvisitor2 "));
        assert!(line(&requests[3]).starts_with("GET /ajax/statuses/show?id=6&"));
        assert!(requests[3].contains("SUB=_2AkRedirSub"));
        assert!(!requests[3].contains("login-sub"));
    }

    #[tokio::test]
    async fn repeated_redirect_reports_login_page() {
        let redirect = b"HTTP/1.1 302 Found\r\nLocation: https://login.sina.com.cn/signin\r\n\
            Content-Length: 0\r\nConnection: close\r\n\r\n";
        let callback = "cb({\"data\":{\"sub\":\"_2AkLoopSub\",\"subp\":\"LoopSubp\"}});";
        let (root, _server) = serve(vec![
            redirect.to_vec(),
            response("200 OK", "text/plain", callback.as_bytes()),
            html(""),
            redirect.to_vec(),
        ]);
        let http = session(root);
        let text = error_text(extract(
            &http,
            Url::parse("https://weibo.com/detail/7").unwrap(),
        ))
        .await;
        assert_eq!(text, "Weibo redirected to its login page");
    }

    #[tokio::test]
    async fn visitor_failure_is_an_error() {
        let (root, _server) = serve(vec![response("200 OK", "text/plain", b"nope")]);
        let http = http("Weibo", root);
        let text = error_text(extract(
            &http,
            Url::parse("https://weibo.com/detail/8").unwrap(),
        ))
        .await;
        assert_eq!(text, "Weibo visitor system did not issue a session");
    }

    #[tokio::test]
    async fn logged_in_feed_uses_pages_and_screen_name_lookup() {
        let (root, server) = serve(vec![
            json(json!({"data": {"user": {"idstr": "2553930725"}}})),
            json(json!({"ok": 1, "data": {"list": [status(100)]}})),
            json(json!({"ok": 1, "data": {"list": []}})),
        ]);
        let http = session(root);
        let files = media(extract(
            &http,
            Url::parse("https://www.weibo.com/n/周于希Sally?tabtype=feed").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let requests = server.join().unwrap();
        assert!(
            line(&requests[0]).starts_with(
                "GET /ajax/profile/info?screen_name=%E5%91%A8%E4%BA%8E%E5%B8%8CSally "
            )
        );
        assert!(
            line(&requests[1])
                .starts_with("GET /ajax/statuses/mymblog?uid=2553930725&feature=0&page=1 ")
        );
        assert!(
            line(&requests[2])
                .starts_with("GET /ajax/statuses/mymblog?uid=2553930725&feature=0&page=2 ")
        );
    }

    #[tokio::test]
    async fn home_and_article_retry_negative_ok() {
        let (root, server) = serve(vec![
            json(json!({"ok": -100, "url": "https://weibo.com/login"})),
            html("<html></html>"),
            json(json!({"ok": 1, "data": {"list": [status(1)]}})),
            json(json!({"ok": 1, "data": {"list": []}})),
            json(json!({"ok": 1, "data": {"list": []}})),
        ]);
        let http = session(root);
        let files = media(extract(
            &http,
            Url::parse("https://weibo.com/p/1003061758989602?tabtype=home").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(keywords(&files[0])["subcategory"], "home");
        let files = media(extract(
            &http,
            Url::parse("https://weibo.com/1758989602?tabtype=article").unwrap(),
        ))
        .await;
        assert!(files.is_empty());
        let requests = server.join().unwrap();
        let home = "GET /ajax/profile/myhot?uid=1758989602&page=1&feature=2 ";
        assert!(line(&requests[0]).starts_with(home));
        assert!(line(&requests[1]).starts_with("GET / "));
        assert!(line(&requests[2]).starts_with(home));
        assert!(
            line(&requests[3])
                .starts_with("GET /ajax/profile/myhot?uid=1758989602&page=2&feature=2 ")
        );
        assert!(
            line(&requests[4])
                .starts_with("GET /ajax/statuses/mymblog?uid=1758989602&page=1&feature=10 ")
        );
    }

    #[tokio::test]
    async fn api_error_on_first_page() {
        let (root, _server) = serve(vec![json(json!({"ok": 0, "msg": "这里还没有内容"}))]);
        let http = session(root);
        let text = error_text(extract(
            &http,
            Url::parse("https://weibo.com/1758989602?tabtype=newVideo").unwrap(),
        ))
        .await;
        assert_eq!(text, "Weibo API error: \"这里还没有内容\"");
    }

    #[tokio::test]
    async fn videos_and_newvideo_follow_cursors() {
        let (root, server) = serve(vec![
            json(
                json!({"ok": 1, "data": {"list": [{"video_detail_vo": status(1)}, {"x": 1}],
                "next_cursor": "c2"}}),
            ),
            json(
                json!({"ok": 1, "data": {"list": [{"video_detail_vo": status(2)}],
                "next_cursor": -1}}),
            ),
            json(json!({"ok": 1, "data": {"list": [status(3)], "next_cursor": 0}})),
            json(json!({"ok": 1, "data": {"list": []}})),
        ]);
        let http = session(root);
        let files = media(extract(
            &http,
            Url::parse("https://weibo.com/1758989602?tabtype=video").unwrap(),
        ))
        .await;
        assert_eq!(
            urls(&files),
            [
                "https://wx1.sinaimg.cn/large/p1.jpg",
                "https://wx1.sinaimg.cn/large/p2.jpg"
            ]
        );
        assert_eq!(keywords(&files[0])["subcategory"], "videos");
        // A falsy `next_cursor` falls through to the last-status-ID branch.
        let files = media(extract(
            &http,
            Url::parse("https://weibo.com/1758989602?tabtype=newVideo").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(keywords(&files[0])["subcategory"], "newvideo");
        let requests = server.join().unwrap();
        assert!(
            line(&requests[0]).starts_with("GET /ajax/profile/getprofilevideolist?uid=1758989602 ")
        );
        assert!(
            line(&requests[1])
                .starts_with("GET /ajax/profile/getprofilevideolist?uid=1758989602&cursor=c2 ")
        );
        assert!(
            line(&requests[2]).starts_with("GET /ajax/profile/getWaterFallContent?uid=1758989602 ")
        );
        assert!(
            line(&requests[3])
                .starts_with("GET /ajax/profile/getWaterFallContent?uid=1758989602&since_id=2 ")
        );
    }

    #[tokio::test]
    async fn album_statuses_once_per_mid() {
        let mut gone = status(6);
        gone["ok"] = json!(0);
        let (root, server) = serve(vec![
            json(
                json!({"ok": 1, "data": {"list": [{"mid": "5"}, {"mid": "5"}, {"mid": 6}],
                "since_id": "abc"}}),
            ),
            json(status(5)),
            json(gone),
            json(json!({"ok": 1, "data": {"list": [], "since_id": 0}})),
        ]);
        let http = session(root);
        let files = media(extract(
            &http,
            Url::parse("https://weibo.com/1758989602?tabtype=album").unwrap(),
        ))
        .await;
        assert_eq!(urls(&files), ["https://wx1.sinaimg.cn/large/p5.jpg"]);
        assert_eq!(keywords(&files[0])["subcategory"], "album");
        let (file, _) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "5_01.jpg");
        let requests = server.join().unwrap();
        assert!(line(&requests[0]).starts_with("GET /ajax/profile/getImageWall?uid=1758989602 "));
        assert!(line(&requests[1]).starts_with("GET /ajax/statuses/show?id=5&"));
        assert!(line(&requests[2]).starts_with("GET /ajax/statuses/show?id=6&"));
        assert!(
            line(&requests[3])
                .starts_with("GET /ajax/profile/getImageWall?uid=1758989602&sinceid=abc ")
        );
    }

    fn album_wall(albums: Value) -> Vec<u8> {
        json(
            json!({"ok": 1, "data": {"list": [{"pid": "main1", "type": "pic"}],
            "album_list": albums, "since_id": 0}}),
        )
    }

    fn albums() -> Value {
        json!([
            {"containerid": "231826_-_photo_-_%E5%A4%B4%E5%83%8F", "pic_title": "头像"},
            {"containerid": "231826_-_photo_-_wall", "pic_title": "Wall"},
        ])
    }

    #[tokio::test]
    async fn subalbum_by_title() {
        let (root, server) = serve(vec![
            json(
                json!({"data": {"user": {"idstr": "2142058927", "id": 2142058927,
                "screen_name": "吴磊LEO"}}}),
            ),
            album_wall(albums()),
            json(
                json!({"ok": 1, "data": {"list": [{"pid": "p1", "type": "pic"}, {"mid": "77"}],
                "since_id": 0}}),
            ),
            json(status(77)),
        ]);
        let http = session(root);
        let files = media(extract(
            &http,
            Url::parse("https://weibo.com/u/2142058927?tabtype=album-头像").unwrap(),
        ))
        .await;
        assert_eq!(
            urls(&files),
            [
                "https://weibo.com/ajax/common/download?pid=p1",
                "https://wx1.sinaimg.cn/large/p77.jpg"
            ]
        );
        let k = keywords(&files[0]);
        assert_eq!(k["filename"], "p1");
        assert_eq!(k["extension"], "jpg");
        assert_eq!(k["pid"], "p1");
        assert_eq!(k["type"], "pic");
        assert_eq!(k["subalbum"]["pic_title"], "头像");
        assert_eq!(k["user"]["screen_name"], "吴磊LEO");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "p1.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            PathBuf::from("weibo/吴磊LEO/Album/头像")
        );
        let archive = crate::sites::archive_key(&files[0]).unwrap();
        assert!(archive.1.contains("\"头像\",\"p1\""), "{archive:?}");
        // Statuses inside a subalbum use the subalbum formats and keywords.
        let k = keywords(&files[1]);
        assert_eq!(k["user"]["screen_name"], "吴磊LEO");
        assert_eq!(k["subalbum"]["pic_title"], "头像");
        assert_eq!(k["status"]["id"], 77);
        let (file, _) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "p77.jpg");
        let requests = server.join().unwrap();
        assert!(line(&requests[0]).starts_with("GET /ajax/profile/info?custom=2142058927 "));
        assert!(line(&requests[1]).starts_with(
            "GET /ajax/profile/getImageWall?uid=2142058927&sinceid=0&has_album=true "
        ));
        assert!(line(&requests[2]).starts_with(
            "GET /ajax/profile/getAlbumDetail?uid=2142058927&containerid=231826_-_photo_-_%E5%A4%B4%E5%83%8F "
        ));
    }

    #[tokio::test]
    async fn subalbum_selection_modes() {
        let user = || json(json!({"data": {"user": {"idstr": "1", "screen_name": "U"}}}));
        let detail =
            |pid: &str| json(json!({"ok": 1, "data": {"list": [{"pid": pid}], "since_id": 0}}));
        let (root, server) = serve(vec![
            // index 2
            user(),
            album_wall(albums()),
            detail("w1"),
            // all: subalbums, then the image wall
            user(),
            album_wall(albums()),
            detail("a1"),
            detail("a2"),
            // `subalbums` option without a group: the image wall only
            user(),
            album_wall(albums()),
            // missing index
            user(),
            album_wall(albums()),
        ]);
        let http = session(root.clone());
        let got = |files: Vec<crate::extract::Media>| {
            files
                .iter()
                .map(|m| keywords(m)["filename"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        let files = media(extract(
            &http,
            Url::parse("https://weibo.com/u/1?tabtype=album_2").unwrap(),
        ))
        .await;
        assert_eq!(got(files), ["w1"]);
        let files = media(extract(
            &http,
            Url::parse("https://weibo.com/u/1?tabtype=album-all").unwrap(),
        ))
        .await;
        assert_eq!(got(files), ["a1", "a2", "main1"]);
        let options = with(session(root.clone()), json!({"subalbums": true}));
        let files = media(extract(
            &options,
            Url::parse("https://weibo.com/u/1?tabtype=album").unwrap(),
        ))
        .await;
        assert_eq!(keywords(&files[0])["subalbum"], json!({}));
        let (_, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            PathBuf::from("weibo/U/Album")
        );
        assert_eq!(got(files), ["main1"]);
        let text = error_text(extract(
            &http,
            Url::parse("https://weibo.com/u/1?tabtype=album-3").unwrap(),
        ))
        .await;
        assert_eq!(text, "Weibo subalbum not found");
        assert_eq!(server.join().unwrap().len(), 11);
    }

    #[tokio::test]
    async fn user_dispatch_defaults_to_feed() {
        let (root, server) = serve(vec![
            json(json!({"ok": 1, "data": {"list": [status(1)]}})),
            json(json!({"ok": 1, "data": {"list": []}})),
        ]);
        let http = session(root);
        let files = media(extract(
            &http,
            Url::parse("https://weibo.com/1758989602").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(keywords(&files[0])["subcategory"], "feed");
        let requests = server.join().unwrap();
        assert!(
            line(&requests[0])
                .starts_with("GET /ajax/statuses/mymblog?uid=1758989602&feature=0&page=1 ")
        );
    }

    #[tokio::test]
    async fn user_dispatch_include() {
        let empty = || json(json!({"ok": 1, "data": {"list": []}}));
        let (root, server) = serve(vec![
            json(json!({"data": {"user": {"idstr": "2553930725"}}})),
            // album
            empty(),
            // subalbums -> album-only
            json(json!({"data": {"user": {"idstr": "2553930725"}}})),
            album_wall(json!([])),
            // all
            empty(),
            empty(),
            empty(),
            empty(),
            empty(),
            empty(),
        ]);
        let http = with(
            session(root.clone()),
            json!({"include": "album, subalbums,bogus"}),
        );
        assert!(
            media(extract(
                &http,
                Url::parse("https://weibo.com/zhouyuxi77").unwrap()
            ))
            .await
            .is_empty()
        );
        let http = with(session(root), json!({"include": "all"}));
        assert!(
            media(extract(
                &http,
                Url::parse("https://weibo.com/u/2553930725").unwrap()
            ))
            .await
            .is_empty()
        );
        let requests = server.join().unwrap();
        let lines: Vec<&str> = requests
            .iter()
            .map(|r| line(r).split(' ').nth(1).unwrap())
            .collect();
        assert_eq!(
            lines,
            [
                "/ajax/profile/info?custom=zhouyuxi77",
                "/ajax/profile/getImageWall?uid=2553930725",
                "/ajax/profile/info?custom=2553930725",
                "/ajax/profile/getImageWall?uid=2553930725&sinceid=0&has_album=true",
                "/ajax/profile/myhot?uid=2553930725&page=1&feature=2",
                "/ajax/statuses/mymblog?uid=2553930725&feature=0&page=1",
                "/ajax/profile/getprofilevideolist?uid=2553930725",
                "/ajax/profile/getWaterFallContent?uid=2553930725",
                "/ajax/statuses/mymblog?uid=2553930725&page=1&feature=10",
                "/ajax/profile/getImageWall?uid=2553930725",
            ]
        );
    }

    #[tokio::test]
    async fn invalid_include_is_an_error() {
        let (root, _server) = serve(vec![]);
        let http = with(session(root), json!({"include": 5}));
        assert!(
            crate::sites::testing::fails(extract(
                &http,
                Url::parse("https://weibo.com/1758989602").unwrap()
            ))
            .await
        );
    }
}
