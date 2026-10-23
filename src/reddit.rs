//! Experimental native Reddit extraction.
//!
//! Mirrors gallery_dl/extractor/reddit.py with the unauthenticated REST API
//! (`https://www.reddit.com/....json?raw_json=1`): the submission,
//! subreddit (any listing such as `/top` or `/search`), home, user (any
//! listing such as `/submitted` or `/comments`), image and share-redirect
//! classes. Submission files are `i.redd.it`/`preview.redd.it` links,
//! galleries, `media_metadata` embeds and `v.redd.it` videos (DASH manifest,
//! page or plain URL per the `videos` setting, with the MP4 rendition as a
//! download fallback). Other links, from the submission, its self text and
//! its comments, are queued as child jobs for the extractor of that site;
//! links to other submissions are followed up to `recursion` levels.
//!
//! Every file carries the upstream keyword dictionary (the complete
//! submission object plus `comment`, `date`, `num`, `filename`, `extension`
//! and, for user listings, `user`), so the default upstream filename and
//! directory formats apply.
use crate::{
    Result, error,
    event::{ChildMetadata, ExtractionEvent, KeywordPolicy, Queue, child_job, group_with_keywords},
    extract::{ManifestProtocol, Media, MediaJob, PostMetadata},
    file_metadata::FileMetadata,
    network::NetworkOptions,
    sites::{Route, SiteKeywords},
    transport::{RateLimits, Redirects, retry_after},
};
use futures_util::{
    StreamExt,
    stream::{self, BoxStream},
};
use reqwest::{Client, StatusCode, header};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};
use url::Url;

/// Reddit asks API clients for a unique, descriptive User-Agent.
const USER_AGENT: &str = concat!(
    "gallery-dl-rs/",
    env!("CARGO_PKG_VERSION"),
    " (native Reddit extractor)"
);
const API_ROOT: &str = "https://www.reddit.com/";
const MAX_RESPONSE: usize = 16 * 1024 * 1024;
const LOGIN_REQUIRED: &str = "Reddit requires a logged-in session for its JSON API from this \
                              network; pass reddit.com cookies with --cookies";
/// Bound listings to 100 pages (Reddit itself stops near 1,000 items).
const MAX_PAGES: usize = 100;
/// Upstream `request_interval`.
const REQUEST_INTERVAL: Duration = Duration::from_millis(600);
/// Bound `morechildren` expansion and unlimited recursion.
const MAX_MORE_CHILDREN: usize = 10_000;
const MAX_RECURSION: i64 = 1_000;
const MAX_QUERY_PARAMS: usize = 32;

/// Upstream `{id}{num:? //>02} {title|link_title:[:220]}.{extension}`; the
/// template engine has no `[:N]` format specifier, so accessor slices are used.
const FILENAME: &str = "{id}{num:? //>02} {title[:220]|link_title[:220]}.{extension}";
pub static SUBMISSION: Route = Route::new(
    "submission",
    FILENAME,
    &["{category}", "{subreddit}"],
    "{filename}",
);
pub static SUBREDDIT: Route = Route::new(
    "subreddit",
    FILENAME,
    &["{category}", "{subreddit}"],
    "{filename}",
);
pub static HOME: Route = Route::new(
    "home",
    FILENAME,
    &["{category}", "{subreddit}"],
    "{filename}",
);
pub static USER: Route = Route::new(
    "user",
    FILENAME,
    &["{category}", "Users", "{user[name]}"],
    "{filename}",
);
pub static IMAGE: Route = Route::new(
    "image",
    "{filename}.{extension}",
    &["{category}"],
    "{filename}",
);

/// A supported Reddit input with validated components.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Submission {
        id: String,
    },
    /// `/r/NAME[/SUB]` listings; `sub` is e.g. `top`, `new` or `search`.
    Subreddit {
        name: String,
        sub: Option<String>,
        params: Vec<(String, String)>,
    },
    /// The front page and its listings.
    Home {
        sub: Option<String>,
        params: Vec<(String, String)>,
    },
    /// `/user/NAME[/SUB]`; `sub` is e.g. `submitted`, `comments` or `saved`.
    User {
        name: String,
        sub: Option<String>,
        params: Vec<(String, String)>,
    },
    /// A file on `i.redd.it`, `preview.redd.it` or `i.reddituploads.com`.
    Image {
        url: Url,
    },
    /// A mobile-app share link `/r|u|user/NAME/s/CODE`.
    Redirect {
        kind: String,
        name: String,
        code: String,
    },
}

/// Submission IDs are lowercase base36.
pub fn valid_id(id: &str) -> bool {
    (1..=16).contains(&id.len()) && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'z'))
}

/// Media IDs (gallery IDs, file stems, v.redd.it IDs) are short alphanumerics.
pub fn valid_media_id(id: &str) -> bool {
    (1..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

fn lower_word(sub: &str) -> bool {
    (1..=32).contains(&sub.len()) && sub.bytes().all(|b| b.is_ascii_lowercase())
}

/// `reddit.com` with an optional `www.` and/or one more `\w+` label.
fn reddit_host(host: &str) -> bool {
    let rest = host.strip_prefix("www.").unwrap_or(host);
    rest == "reddit.com"
        || rest.strip_suffix(".reddit.com").is_some_and(|label| {
            !label.is_empty()
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        })
}

/// Classify a Reddit URL like upstream's extractor patterns.
pub fn target(url: &Url) -> Result<Target> {
    let unsupported = || {
        error(
            "unsupported Reddit URL; expected a submission, subreddit, user, home, share or i.redd.it link",
        )
    };
    let host = url.host_str().ok_or_else(unsupported)?;
    if !matches!(url.scheme(), "http" | "https")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(unsupported());
    }
    let segments: Vec<_> = url
        .path_segments()
        .map(|segments| segments.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    if host == "redd.it" {
        return match segments.as_slice() {
            [id, ..] if valid_id(id) => Ok(Target::Submission { id: (*id).into() }),
            _ => Err(unsupported()),
        };
    }
    if matches!(
        host,
        "i.redd.it" | "preview.redd.it" | "i.reddituploads.com"
    ) {
        return match segments.as_slice() {
            [_] => {
                let mut url = url.clone();
                url.set_fragment(None);
                if host == "preview.redd.it" {
                    // Upstream downloads the original from i.redd.it.
                    url.set_host(Some("i.redd.it")).map_err(|_| unsupported())?;
                    url.set_query(None);
                }
                url.set_scheme("https").map_err(|_| unsupported())?;
                Ok(Target::Image { url })
            }
            _ => Err(unsupported()),
        };
    }
    if !reddit_host(host) {
        return Err(unsupported());
    }
    let params = || -> Result<Vec<(String, String)>> {
        let params: Vec<_> = url
            .query_pairs()
            .filter(|(key, _)| !key.is_empty())
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        if params.len() > MAX_QUERY_PARAMS || params.iter().any(|(k, v)| k.len() + v.len() > 512) {
            return Err(unsupported());
        }
        Ok(params)
    };
    let submission = |id: &str| {
        let id = id.to_ascii_lowercase();
        valid_id(&id)
            .then_some(Target::Submission { id })
            .ok_or_else(unsupported)
    };
    let sub = |sub: Option<&&str>| match sub {
        None => Ok(None),
        Some(sub) if lower_word(sub) => Ok(Some((*sub).to_owned())),
        Some(_) => Err(unsupported()),
    };
    match segments.as_slice() {
        ["r" | "u" | "user", _, "comments", id, ..] | ["comments" | "gallery", id, ..] => {
            submission(id)
        }
        [kind, name, "s", code]
            if matches!(*kind, "r" | "u" | "user")
                && valid_name(name)
                && code.len() == 10
                && code.bytes().all(|b| b.is_ascii_alphanumeric()) =>
        {
            Ok(Target::Redirect {
                kind: if *kind == "r" { "r" } else { "user" }.into(),
                name: (*name).into(),
                code: (*code).into(),
            })
        }
        ["r", name, rest @ ..] if rest.len() <= 1 && valid_name(name) => Ok(Target::Subreddit {
            name: (*name).into(),
            sub: sub(rest.first())?,
            params: params()?,
        }),
        ["u" | "user", name, rest @ ..] if rest.len() <= 1 && valid_name(name) => {
            Ok(Target::User {
                name: (*name).into(),
                sub: sub(rest.first())?,
                params: params()?,
            })
        }
        [] | [_] => Ok(Target::Home {
            sub: sub(segments.first())?,
            params: params()?,
        }),
        _ => Err(unsupported()),
    }
}

/// Upstream `videos` setting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Videos {
    Disabled,
    /// DASH manifest (default), falling back to the MP4 rendition.
    #[default]
    Dash,
    /// The submission page for a video-page downloader.
    Ytdl,
    /// The `v.redd.it` URL for a video-page downloader.
    Url,
}

/// Upstream `extractor.reddit.*` settings.
#[derive(Clone, Debug)]
pub struct Options {
    /// Number of comments to request per submission (0 disables comments).
    pub comments: u64,
    pub morecomments: bool,
    /// Levels of linked submissions to follow; negative is unlimited.
    pub recursion: i64,
    pub videos: Videos,
    /// Accepted for compatibility; preview fallbacks of failed child jobs are not supported.
    pub previews: bool,
    pub embeds: bool,
    /// Defaults to whether comments are enabled.
    pub selftext: Option<bool>,
    /// Defaults to false for upvoted/downvoted/saved listings, true otherwise.
    pub only: Option<bool>,
    pub id_min: Option<u128>,
    pub id_max: Option<u128>,
    pub limit: Option<u64>,
    /// Accepted for compatibility; every file already carries its submission keywords.
    pub parent_directory: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            comments: 0,
            morecomments: false,
            recursion: 0,
            videos: Videos::Dash,
            previews: true,
            embeds: true,
            selftext: None,
            only: None,
            id_min: None,
            id_max: None,
            limit: None,
            parent_directory: false,
        }
    }
}

/// Upstream `util.b36decode`.
fn b36decode(id: &str) -> Option<u128> {
    if id.is_empty() || id.len() > 24 {
        return None;
    }
    id.bytes().try_fold(0u128, |acc, b| {
        let digit = match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'z' => b - b'a' + 10,
            b'A'..=b'Z' => b - b'A' + 10,
            _ => return None,
        };
        acc.checked_mul(36)?.checked_add(u128::from(digit))
    })
}

impl Options {
    pub fn parse(map: &Map<String, Value>) -> Result<Self> {
        let mut options = Self::default();
        let boolean = |key: &str| -> Result<Option<bool>> {
            match map.get(key) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::Bool(value)) => Ok(Some(*value)),
                Some(_) => Err(error(format!("Reddit {key} must be a boolean"))),
            }
        };
        let integer = |key: &str| -> Result<Option<i64>> {
            let invalid = || error(format!("Reddit {key} must be an integer"));
            match map.get(key) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::Number(n)) => n.as_i64().map(Some).ok_or_else(invalid),
                Some(Value::String(s)) => s.trim().parse().map(Some).map_err(|_| invalid()),
                Some(_) => Err(invalid()),
            }
        };
        let id = |key: &str| -> Result<Option<u128>> {
            let invalid = || error(format!("Reddit {key} must be a base36 ID"));
            match map.get(key) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(s)) if s.is_empty() => Ok(None),
                Some(Value::String(s)) => {
                    let raw = s
                        .rsplit('_')
                        .next()
                        .unwrap_or_default()
                        .to_ascii_lowercase();
                    b36decode(&raw).map(Some).ok_or_else(invalid)
                }
                Some(_) => Err(invalid()),
            }
        };
        if let Some(comments) = integer("comments")? {
            options.comments = comments.max(0) as u64;
        }
        if let Some(value) = boolean("morecomments")? {
            options.morecomments = value;
        }
        if let Some(recursion) = integer("recursion")? {
            options.recursion = recursion;
        }
        options.videos = match map.get("videos") {
            None | Some(Value::Null) => Videos::Dash,
            Some(Value::Bool(false)) => Videos::Disabled,
            Some(Value::Bool(true)) => Videos::Url,
            Some(Value::String(s)) if s == "dash" => Videos::Dash,
            Some(Value::String(s)) if s == "ytdl" => Videos::Ytdl,
            Some(_) => {
                return Err(error(
                    "Reddit videos must be a boolean, \"dash\" or \"ytdl\"",
                ));
            }
        };
        if let Some(value) = boolean("previews")? {
            options.previews = value;
        }
        if let Some(value) = boolean("embeds")? {
            options.embeds = value;
        }
        options.selftext = boolean("selftext")?;
        options.only = boolean("only")?;
        if let Some(value) = boolean("parent-directory")? {
            options.parent_directory = value;
        }
        options.id_min = id("id-min")?;
        options.id_max = id("id-max")?;
        // Upstream ignores the legacy "zik0zj" (2**31 - 1) id-max default.
        if options.id_max == Some(2_147_483_647) {
            options.id_max = None;
        }
        if let Some(limit) = integer("limit")? {
            options.limit = (limit > 0).then_some(limit as u64);
        }
        Ok(options)
    }
}

fn string<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Python truthiness of a JSON value.
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

fn timestamp(value: &Value) -> Option<i64> {
    value
        .get("created_utc")
        .and_then(Value::as_f64)
        .filter(|t| t.is_finite() && *t >= 0.0)
        .map(|t| t as i64)
}

fn dimension(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(Value::as_u64).filter(|v| *v > 0)
}

/// Only HTTPS Reddit media hosts are trusted for gallery, embed and video URLs.
fn reddit_media_url(raw: &str) -> Option<Url> {
    let url = Url::parse(raw.trim()).ok()?;
    let host = url.host_str()?;
    (url.scheme() == "https"
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && (host == "redd.it" || host.ends_with(".redd.it") || host.ends_with(".redditmedia.com")))
    .then_some(url)
}

/// Upstream `url.partition("?")[0].replace("/preview.", "/i.", 1)`.
fn original_media(raw: &str) -> Option<Url> {
    let base = raw.split('?').next().unwrap_or_default();
    reddit_media_url(&base.replacen("/preview.", "/i.", 1))
}

/// One file before its keyword dictionary is assembled.
struct File {
    url: Url,
    job: MediaJob,
    /// URL for `filename`/`extension`, when different from `url`.
    name_url: Option<Url>,
    file: FileMetadata,
    handoff: BTreeMap<String, Value>,
}

impl File {
    fn direct(url: Url) -> Self {
        Self {
            url,
            job: MediaJob::DirectHttp,
            name_url: None,
            file: FileMetadata::default(),
            handoff: BTreeMap::new(),
        }
    }
}

fn source_file(src: &Value, mime: Option<&str>) -> FileMetadata {
    FileMetadata {
        width: dimension(src, "x"),
        height: dimension(src, "y"),
        media_type: mime.map(Arc::from),
        ..FileMetadata::default()
    }
}

/// Upstream `_extract_gallery`: items in `gallery_data` order.
fn gallery(media: &Value) -> Vec<File> {
    let Some(meta) = media.get("media_metadata").filter(|m| m.is_object()) else {
        return Vec::new();
    };
    let items = media
        .pointer("/gallery_data/items")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    items
        .iter()
        .filter_map(|item| {
            let data = meta.get(string(item, "media_id")?)?;
            if string(data, "status") != Some("valid") {
                return None;
            }
            let src = data.get("s")?;
            let raw = ["u", "gif", "mp4"]
                .into_iter()
                .find_map(|key| string(src, key))?;
            let mut file = File::direct(original_media(raw)?);
            file.file = source_file(src, string(data, "m"));
            file.file.description = string(item, "caption").map(Arc::from);
            Some(file)
        })
        .collect()
}

/// Upstream `_extract_embed`: every valid `media_metadata` entry.
fn embeds(media: &Value) -> Vec<File> {
    let Some(meta) = media.get("media_metadata").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut files = Vec::new();
    for data in meta.values() {
        if string(data, "status") != Some("valid") {
            continue;
        }
        if let Some(src) = data.get("s").filter(|s| truthy(Some(s))) {
            let Some(raw) = ["u", "gif", "mp4"]
                .into_iter()
                .find_map(|key| string(src, key))
            else {
                continue;
            };
            let url = if raw.contains("//external") {
                reddit_media_url(raw)
            } else {
                original_media(raw)
            };
            if let Some(url) = url {
                let mut file = File::direct(url);
                file.file = source_file(src, string(data, "m"));
                files.push(file);
            }
        } else {
            let (key, protocol, marker) = if string(data, "dashUrl").is_some() {
                ("dashUrl", ManifestProtocol::Dash, "dash")
            } else {
                ("hlsUrl", ManifestProtocol::Hls, "hls")
            };
            let Some(base_url) = string(data, key).and_then(reddit_media_url) else {
                continue;
            };
            let Ok(url) = Url::parse(&format!("ytdl:{base_url}")) else {
                continue;
            };
            let mut handoff = BTreeMap::new();
            handoff.insert("_ytdl_manifest".into(), Value::from(marker));
            files.push(File {
                url,
                job: MediaJob::Manifest {
                    protocol,
                    base_url: base_url.clone(),
                    inline_data: None,
                },
                name_url: Some(base_url),
                file: FileMetadata::default(),
                handoff,
            });
        }
    }
    files
}

/// Upstream `_extract_video*` for a submission with `is_video`.
fn video(media: &Value, mode: Videos) -> Option<File> {
    let reddit_video = ["/secure_media/reddit_video", "/media/reddit_video"]
        .into_iter()
        .find_map(|pointer| media.pointer(pointer));
    let fallback = reddit_video
        .and_then(|v| string(v, "fallback_url"))
        .and_then(reddit_media_url);
    let page = string(media, "url").and_then(|raw| Url::parse(raw).ok());
    let mut handoff = BTreeMap::new();
    if let Some(title) = media.get("title") {
        handoff.insert("_ytdl_extra".into(), serde_json::json!({ "title": title }));
    }
    if let Some(fallback) = &fallback {
        handoff.insert(
            "_fallback".into(),
            Value::Array(vec![Value::String(fallback.to_string())]),
        );
    }
    let page_job = |page_url: Url| -> Option<(Url, MediaJob)> {
        if !matches!(page_url.scheme(), "http" | "https") {
            return None;
        }
        let url = Url::parse(&format!("ytdl:{page_url}")).ok()?;
        Some((url, MediaJob::VideoPage { page_url }))
    };
    let (url, job) = match mode {
        Videos::Disabled => return None,
        Videos::Dash => match reddit_video
            .and_then(|v| string(v, "dash_url"))
            .and_then(reddit_media_url)
        {
            Some(base_url) => {
                handoff.insert("_ytdl_manifest".into(), Value::from("dash"));
                (
                    Url::parse(&format!("ytdl:{base_url}")).ok()?,
                    MediaJob::Manifest {
                        protocol: ManifestProtocol::Dash,
                        base_url,
                        inline_data: None,
                    },
                )
            }
            None => page_job(page.clone()?)?,
        },
        Videos::Ytdl => {
            let permalink = string(media, "permalink").filter(|p| p.starts_with('/'))?;
            page_job(Url::parse(&format!("https://www.reddit.com{permalink}")).ok()?)?
        }
        Videos::Url => page_job(page.clone()?)?,
    };
    Some(File {
        url,
        job,
        name_url: page,
        file: FileMetadata {
            width: reddit_video.and_then(|v| dimension(v, "width")),
            height: reddit_video.and_then(|v| dimension(v, "height")),
            duration: reddit_video
                .and_then(|v| v.get("duration"))
                .and_then(Value::as_f64),
            bitrate: reddit_video
                .and_then(|v| v.get("bitrate_kbps"))
                .and_then(Value::as_u64)
                .map(|k| k * 1000),
            media_type: Some("video/mp4".into()),
            ..FileMetadata::default()
        },
        handoff,
    })
}

/// `href` values in an HTML fragment (upstream `text.extract_iter`).
fn hrefs(html: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(start) = rest.find(" href=\"") {
        rest = &rest[start + 7..];
        let Some(end) = rest.find('"') else { break };
        out.push(&rest[..end]);
        rest = &rest[end + 1..];
    }
    out
}

/// One `(submission, comments)` pair of upstream's `submissions()`.
#[derive(Clone, Debug, Default)]
pub struct Unit {
    pub submission: Option<Value>,
    pub comments: Vec<Value>,
}

/// Extractor class context for keyword dictionaries.
#[derive(Clone)]
pub struct Context {
    pub route: &'static Route,
    pub subcategory: String,
    pub user: Option<Value>,
}

/// Files, child URLs and linked submissions produced by one unit.
#[derive(Default)]
pub struct Output {
    pub media: Vec<Media>,
    pub queue: Vec<(Url, Map<String, Value>)>,
    pub extra: Vec<String>,
    pub visited: Option<String>,
}

fn date(ts: Option<i64>) -> Value {
    crate::filter::source_date_text(ts).into()
}

/// Upstream `RedditExtractor.items` for one unit, without any I/O.
pub fn process(unit: Unit, options: &Options, context: &Context) -> Output {
    let mut out = Output::default();
    let comments_enabled = options.comments > 0;
    let selftext = options.selftext.unwrap_or(comments_enabled);
    let mut files: Vec<(File, Map<String, Value>)> = Vec::new();
    let mut urls: Vec<(String, Map<String, Value>)> = Vec::new();
    let mut base: Option<Map<String, Value>> = None;
    let mut num = 0u64;
    let numbered = |data: &Map<String, Value>, num: u64| {
        let mut data = data.clone();
        data.insert("num".into(), num.into());
        data
    };

    if let Some(submission) = unit.submission.as_ref().and_then(Value::as_object) {
        let sub = Value::Object(submission.clone());
        let mut data = submission.clone();
        let show = data.remove("_media") != Some(Value::Bool(false));
        data.insert("comment".into(), Value::Null);
        data.insert("date".into(), date(timestamp(&sub)));
        data.insert("num".into(), 0.into());
        if show {
            out.visited = string(&sub, "id").map(str::to_owned);
            let media = submission
                .get("crosspost_parent_list")
                .and_then(Value::as_array)
                .and_then(|list| list.last())
                .unwrap_or(&sub);
            let url = string(media, "url").unwrap_or_default();
            if url.starts_with("https://i.redd.it/") || url.starts_with("https://preview.redd.it/")
            {
                if let Ok(url) = Url::parse(url) {
                    files.push((File::direct(url), data.clone()));
                }
            } else if media.get("gallery_data").is_some() {
                for file in gallery(media) {
                    num += 1;
                    files.push((file, numbered(&data, num)));
                }
            } else if options.embeds && media.get("media_metadata").is_some() {
                for file in embeds(media) {
                    num += 1;
                    files.push((file, numbered(&data, num)));
                }
            } else if media.get("is_video") == Some(&Value::Bool(true)) {
                if let Some(file) = video(media, options.videos) {
                    files.push((file, data.clone()));
                }
            } else if !truthy(submission.get("is_self")) {
                urls.push((url.to_owned(), data.clone()));
            }
            if selftext && let Some(html) = string(&sub, "selftext_html") {
                for href in hrefs(html) {
                    urls.push((href.to_owned(), data.clone()));
                }
            }
        }
        data.insert("num".into(), num.into());
        base = Some(data);
    }

    if comments_enabled {
        if base.is_none()
            && let Some(first) = unit.comments.first().and_then(Value::as_object)
        {
            let mut first = first.clone();
            first.entry("num").or_insert(0.into());
            base = Some(first);
        }
        for comment in &unit.comments {
            let has_media = options.embeds && comment.get("media_metadata").is_some();
            let html = string(comment, "body_html").unwrap_or_default();
            let has_href = html.contains(" href=\"");
            if !has_media && !has_href {
                continue;
            }
            let Some(base) = base.as_mut() else { break };
            let mut data = base.clone();
            data.insert("comment".into(), comment.clone());
            data.insert("date".into(), date(timestamp(comment)));
            if has_media {
                for file in embeds(comment) {
                    num += 1;
                    files.push((file, numbered(&data, num)));
                }
                base.insert("num".into(), num.into());
                data.insert("num".into(), num.into());
            }
            for href in hrefs(html) {
                urls.push((href.to_owned(), data.clone()));
            }
        }
    }

    for (raw, data) in urls {
        if raw.is_empty() || raw.starts_with('#') {
            continue;
        }
        let raw = if raw.starts_with('/') {
            format!("https://www.reddit.com{raw}")
        } else {
            raw
        };
        if [
            "https://www.reddit.com/message/compose",
            "https://reddit.com/message/compose",
            "https://preview.redd.it/",
        ]
        .iter()
        .any(|prefix| raw.starts_with(prefix))
        {
            continue;
        }
        let Ok(url) = Url::parse(&crate::sites::unescape(&raw)) else {
            continue;
        };
        if !matches!(url.scheme(), "http" | "https") {
            continue;
        }
        match target(&url) {
            Ok(Target::Submission { id }) => out.extra.push(id),
            Ok(Target::User { .. } | Target::Subreddit { .. } | Target::Home { .. }) => {}
            _ => out.queue.push((url, data)),
        }
    }

    out.media = files
        .into_iter()
        .enumerate()
        .map(|(i, (file, data))| media(file, data, context, i + 1))
        .collect();
    out
}

/// Assemble one file with its keyword dictionary and legacy metadata.
fn media(file: File, mut fields: Map<String, Value>, context: &Context, index: usize) -> Media {
    let name_url = file.name_url.as_ref().unwrap_or(&file.url);
    let (name, mut ext) = crate::sites::nameext(name_url);
    if ext.is_empty() && !matches!(file.job, MediaJob::DirectHttp) {
        ext = "mp4".into();
    }
    fields.insert("filename".into(), name.clone().into());
    fields.insert("extension".into(), ext.clone().into());
    fields.insert("category".into(), "reddit".into());
    fields.insert("subcategory".into(), context.subcategory.clone().into());
    if let Some(user) = &context.user {
        fields.insert("user".into(), user.clone());
    }
    let value = Value::Object(fields);
    let comment = value.get("comment").filter(|c| c.is_object());
    // Comment-only units carry the comment; its submission is `link_id`.
    let post_id = string(&value, "link_id")
        .and_then(|link| link.strip_prefix("t3_"))
        .or_else(|| string(&value, "id"))
        .filter(|id| valid_id(id))
        .map(str::to_owned)
        .unwrap_or_default();
    let media_id = if valid_media_id(&name) {
        name.clone()
    } else {
        let digest = Sha256::digest(file.url.as_str().as_bytes());
        digest[..8].iter().map(|b| format!("{b:02x}")).collect()
    };
    let author = comment
        .and_then(|c| string(c, "author"))
        .or_else(|| string(&value, "author"))
        .filter(|a| *a != "[deleted]")
        .map(Arc::from);
    let caption = string(&value, "title")
        .or_else(|| string(&value, "link_title"))
        .map(Arc::from);
    let ts = comment.and_then(timestamp).or_else(|| timestamp(&value));
    let Value::Object(fields) = value else {
        unreachable!("constructed as an object")
    };
    let filename = if ext.is_empty() {
        name
    } else {
        format!("{name}.{ext}")
    };
    Media {
        url: file.url,
        job: file.job,
        category: "reddit".into(),
        filename,
        metadata: Some(PostMetadata {
            file: Some(file.file),
            post_id,
            media_id,
            username: author,
            caption,
            timestamp: ts,
            index,
            site_keywords: Some(Arc::new(SiteKeywords {
                route: context.route,
                fields,
            })),
            ..PostMetadata::default()
        }),
        headers: BTreeMap::new(),
        handoff: file.handoff,
    }
}

fn listing_children(listing: &Value) -> Result<&[Value]> {
    listing
        .pointer("/data/children")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .ok_or_else(|| error("Reddit response is missing its listing"))
}

fn more_ids(value: &Value) -> impl Iterator<Item = String> + '_ {
    value
        .pointer("/data/children")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|id| valid_id(id))
        .map(str::to_owned)
}

/// Upstream `RedditAPI._flatten`: breadth-first comments and `more` IDs.
pub fn flatten(listing: &Value) -> (Vec<Value>, Vec<String>) {
    let mut queue: VecDeque<&Value> = listing
        .pointer("/data/children")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .collect();
    let mut comments = Vec::new();
    let mut more = Vec::new();
    while let Some(child) = queue.pop_front() {
        if string(child, "kind") == Some("more") {
            more.extend(more_ids(child));
            continue;
        }
        let Some(data) = child.get("data").filter(|d| d.is_object()) else {
            continue;
        };
        if let Some(replies) = data
            .pointer("/replies/data/children")
            .and_then(Value::as_array)
        {
            queue.extend(replies.iter());
        }
        comments.push(data.clone());
    }
    (comments, more)
}

/// Parse a `/comments/ID/.json` response into its submission and comment listing.
pub fn parse_submission(body: &Value, id: &str) -> Result<(Value, Value)> {
    let parts = body
        .as_array()
        .filter(|parts| !parts.is_empty())
        .ok_or_else(|| error("Reddit response is missing its submission"))?;
    let post = listing_children(&parts[0])?
        .iter()
        .find(|child| string(child, "kind") == Some("t3"))
        .and_then(|child| child.get("data"))
        .ok_or_else(|| error("Reddit submission not found or removed"))?;
    if string(post, "id") != Some(id) {
        return Err(error("Reddit response does not match the requested post"));
    }
    Ok((post.clone(), parts.get(1).cloned().unwrap_or(Value::Null)))
}

/// One parsed listing page: `(kind, data)` children and the `after` cursor.
pub struct Page {
    pub children: Vec<(String, Value)>,
    pub after: Option<String>,
}

pub fn parse_listing(body: &Value) -> Result<Page> {
    let children = listing_children(body)?
        .iter()
        .filter_map(|child| {
            Some((
                string(child, "kind")?.to_owned(),
                child.get("data").filter(|d| d.is_object())?.clone(),
            ))
        })
        .collect();
    let after = body
        .pointer("/data/after")
        .and_then(Value::as_str)
        .filter(|a| {
            (1..=64).contains(&a.len()) && a.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        })
        .map(str::to_owned);
    Ok(Page { children, after })
}

pub struct Reddit {
    client: Client,
    network: NetworkOptions,
    api_root: Url,
    timeout: Duration,
    request_timeout: Duration,
    retries: u8,
    rate_limits: RateLimits,
    options: Options,
    interval: Duration,
    last_request: tokio::sync::Mutex<Option<Instant>>,
    /// Session cookies, sent with API requests like upstream's REST mode.
    cookies: std::sync::Mutex<crate::cookies::CookieJar>,
}

/// Pending work of one extraction.
enum Pending {
    Unit(Unit),
    /// Fetch a submission (with comments); forbidden ones are skipped when set.
    Fetch(String, bool),
}

struct Listing {
    path: String,
    params: Vec<(String, String)>,
    after: Option<String>,
    pages: usize,
}

struct Walk<'a> {
    listing: Option<Listing>,
    /// User listings resolve the user first and apply `only`.
    user: Option<(String, Option<String>)>,
    only: Option<String>,
    pending: VecDeque<Pending>,
    context: Context,
    visited: HashSet<String>,
    extra: Vec<String>,
    depth: i64,
    queue: Queue<'a>,
}

/// Default dispatcher: direct media links only.
fn direct_queue<'a>() -> Queue<'a> {
    Arc::new(|url: Url| {
        crate::extract::direct(&url).map(|media| {
            stream::iter(
                crate::event::group(vec![media], "direct")
                    .into_iter()
                    .map(Ok),
            )
            .boxed()
        })
    })
}

impl Reddit {
    pub fn new(timeout: Duration) -> Result<Self> {
        Ok(Self {
            client: NetworkOptions::default().client(USER_AGENT, timeout)?,
            network: NetworkOptions::default(),
            api_root: Url::parse(API_ROOT).unwrap(),
            timeout,
            request_timeout: timeout,
            retries: 0,
            rate_limits: RateLimits::default(),
            options: Options::default(),
            interval: REQUEST_INTERVAL,
            last_request: tokio::sync::Mutex::new(None),
            cookies: std::sync::Mutex::new(crate::cookies::CookieJar::default()),
        })
    }

    /// Session cookies for API requests; Reddit serves its JSON listings only
    /// to logged-in sessions on some networks.
    pub fn with_cookies(self, cookies: crate::cookies::CookieJar) -> Self {
        *self.cookies.lock().unwrap() = cookies;
        self
    }

    /// Apply proxy and user-agent settings to API requests and emitted media.
    pub fn with_network(mut self, network: &NetworkOptions) -> Result<Self> {
        self.client = network.client(USER_AGENT, self.timeout)?;
        self.network = network.clone();
        Ok(self)
    }

    /// Apply upstream extractor settings (`extractor.reddit.*`).
    pub fn with_options(mut self, options: Option<&Map<String, Value>>) -> Result<Self> {
        if let Some(options) = options {
            self.options = Options::parse(options)?;
        }
        Ok(self)
    }

    /// Additional attempts for HTTP 429/5xx and transport failures.
    pub fn with_retries(mut self, retries: u8) -> Self {
        self.retries = retries;
        self
    }

    /// Bound each HTTP request separately from the per-call deadline.
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// Minimum delay between API requests (upstream: 0.6 seconds).
    pub fn with_request_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }

    /// Override the API origin; intended for local tests.
    pub fn with_api_root(mut self, root: Url) -> Self {
        self.api_root = root;
        self
    }

    /// Each API call is bounded by the overall timeout.
    async fn call(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> std::result::Result<Value, Failure> {
        let mut endpoint = self
            .api_root
            .join(path)
            .map_err(|_| Failure::from(error("invalid Reddit path")))?;
        endpoint
            .query_pairs_mut()
            .extend_pairs(query)
            .append_pair("raw_json", "1");
        let body = match tokio::time::timeout(self.timeout, self.request(&endpoint, false))
            .await
            .map_err(|_| Failure::from(error("native Reddit extraction timed out")))?
        {
            Ok(body) => body,
            Err(failure) if failure.error.to_string() == LOGIN_REQUIRED && !self.has_session() => {
                return self.feed_fallback(path, query, failure).await;
            }
            Err(failure) => return Err(failure),
        };
        let value: Value = serde_json::from_slice(&body)
            .map_err(|_| Failure::from(error("Reddit returned invalid JSON")))?;
        if let Some(code) = value.get("error") {
            let status = code.as_u64().and_then(|c| u16::try_from(c).ok());
            return Err(Failure {
                error: match status {
                    Some(403) => {
                        error("Reddit refused access (private, quarantined or banned content)")
                    }
                    Some(404) => error("Reddit submission, subreddit or user not found"),
                    _ => error("Reddit API request failed"),
                },
                retry: false,
                status,
            });
        }
        Ok(value)
    }

    fn has_session(&self) -> bool {
        let root = Url::parse("https://www.reddit.com/").unwrap();
        self.cookies
            .lock()
            .unwrap()
            .value(&root, "reddit_session")
            .is_some()
    }

    /// When the JSON API is closed to logged-out clients, read the same
    /// listing or submission from its public Atom feed instead.
    async fn feed_fallback(
        &self,
        path: &str,
        query: &[(&str, &str)],
        refused: Failure,
    ) -> std::result::Result<Value, Failure> {
        if let Some(name) = path
            .strip_prefix("user/")
            .and_then(|rest| rest.strip_suffix("/about.json"))
        {
            // Feeds carry no account IDs; a user's feed holds only their posts.
            return Ok(serde_json::json!({"data": {"name": name, "from_feed": true}}));
        }
        if path.starts_with("api/morechildren") {
            return Ok(serde_json::json!({"json": {"data": {"things": []}}}));
        }
        let Some(feed) = crate::reddit_rss::feed_path(path) else {
            return Err(refused);
        };
        let Ok(mut endpoint) = self.api_root.join(&feed) else {
            return Err(refused);
        };
        endpoint.query_pairs_mut().extend_pairs(query);
        let body = match tokio::time::timeout(self.timeout, self.request(&endpoint, false)).await {
            Ok(Ok(body)) => body,
            // Logged-out feeds allow about one request per minute; report
            // that rather than the login wall.
            Ok(Err(failure)) if failure.error.to_string().contains("rate limit") => {
                return Err(failure);
            }
            _ => return Err(refused),
        };
        let xml = String::from_utf8_lossy(&body);
        let value = match path
            .strip_prefix("comments/")
            .and_then(|rest| rest.split('/').next())
        {
            Some(id) => crate::reddit_rss::submission(&xml, id),
            None => crate::reddit_rss::listing(&xml),
        };
        value.ok_or(refused)
    }

    /// Upstream `RedditAPI.submission`, including `morecomments`.
    async fn submission(&self, id: &str) -> std::result::Result<Unit, Failure> {
        let limit = self.options.comments.to_string();
        let body = self
            .call(&format!("comments/{id}/.json"), &[("limit", &limit)])
            .await?;
        let (post, listing) = parse_submission(&body, id)?;
        let mut comments = Vec::new();
        if self.options.comments > 0 {
            let (flat, mut more) = flatten(&listing);
            comments = flat;
            if !self.options.morecomments {
                more.clear();
            }
            let link = format!("t3_{id}");
            let mut index = 0;
            while index < more.len() && index < MAX_MORE_CHILDREN {
                let end = (index + 100).min(more.len());
                let children = more[index..end].join(",");
                index = end;
                let body = self
                    .call(
                        "api/morechildren",
                        &[
                            ("link_id", &link),
                            ("api_type", "json"),
                            ("children", &children),
                        ],
                    )
                    .await?;
                for thing in body
                    .pointer("/json/data/things")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if string(thing, "kind") == Some("more") {
                        more.extend(more_ids(thing));
                    } else if let Some(data) = thing.get("data").filter(|d| d.is_object()) {
                        comments.push(data.clone());
                    }
                }
            }
        }
        Ok(Unit {
            submission: Some(post),
            comments,
        })
    }

    fn in_range(&self, post: &Value) -> bool {
        let id = string(post, "id").and_then(b36decode).unwrap_or(0);
        self.options.id_min.is_none_or(|min| id >= min)
            && self.options.id_max.is_none_or(|max| id <= max)
    }

    /// Upstream `RedditAPI._pagination` for one page.
    async fn next_page(&self, walk: &mut Walk<'_>) -> Result<()> {
        let Some(listing) = walk.listing.as_mut() else {
            return Ok(());
        };
        let limit = self.options.limit.map(|l| l.to_string());
        let mut query: Vec<(&str, &str)> = listing
            .params
            .iter()
            .filter(|(k, _)| {
                !matches!(k.as_str(), "after" | "raw_json") && (limit.is_none() || k != "limit")
            })
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        if let Some(limit) = &limit {
            query.push(("limit", limit));
        }
        if let Some(after) = &listing.after {
            query.push(("after", after));
        }
        let body = self
            .call(&listing.path, &query)
            .await
            .map_err(|f| f.error)?;
        let page = parse_listing(&body)?;
        listing.pages += 1;
        let done =
            page.after.is_none() || page.after == listing.after || listing.pages >= MAX_PAGES;
        listing.after = page.after;
        if done {
            walk.listing = None;
        }
        let comments = self.options.comments > 0;
        for (kind, post) in page.children {
            if !self.in_range(&post) {
                continue;
            }
            let pending = match kind.as_str() {
                "t3" if comments && truthy(post.get("num_comments")) => {
                    match string(&post, "id").filter(|id| valid_id(id)) {
                        Some(id) => Pending::Fetch(id.to_owned(), true),
                        None => continue,
                    }
                }
                "t3" => Pending::Unit(Unit {
                    submission: Some(post),
                    comments: Vec::new(),
                }),
                "t1" if comments => Pending::Unit(Unit {
                    submission: None,
                    comments: vec![post],
                }),
                _ => continue,
            };
            walk.pending.push_back(pending);
        }
        Ok(())
    }

    /// Upstream `RedditUserExtractor._only`.
    fn only(unit: Unit, uid: &str) -> Option<Unit> {
        let mut submission = unit.submission;
        if let Some(Value::Object(post)) = submission.as_mut()
            && post.get("author_fullname").and_then(Value::as_str) != Some(uid)
        {
            post.insert("_media".into(), Value::Bool(false));
        }
        let comments: Vec<_> = unit
            .comments
            .into_iter()
            .filter(|c| string(c, "author_fullname") == Some(uid))
            .collect();
        (submission.is_some() || !comments.is_empty()).then_some(Unit {
            submission,
            comments,
        })
    }

    /// Turn one unit into its post events and child jobs.
    fn unit_events<'a>(
        &'a self,
        unit: Unit,
        walk: &mut Walk<'a>,
    ) -> BoxStream<'a, Result<ExtractionEvent>> {
        let unit = match &walk.only {
            Some(uid) => match Self::only(unit, uid) {
                Some(unit) => unit,
                None => return stream::empty().boxed(),
            },
            None => unit,
        };
        let mut out = process(unit, &self.options, &walk.context);
        if let Some(id) = out.visited.take() {
            walk.visited.insert(id);
        }
        walk.extra.append(&mut out.extra);
        for item in &mut out.media {
            self.network.apply_media(item);
        }
        let mut parts: Vec<BoxStream<'a, Result<ExtractionEvent>>> = Vec::new();
        if !out.media.is_empty() {
            let events = group_with_keywords(out.media, "reddit", KeywordPolicy::default());
            parts.push(stream::iter(events.into_iter().map(Ok)).boxed());
        }
        for (url, data) in out.queue {
            if let Some(child) = (walk.queue)(url) {
                let metadata = ChildMetadata::from_raw("reddit", &walk.context.subcategory, &data);
                parts.push(child_job(metadata, move || child));
            }
        }
        stream::iter(parts).flatten().boxed()
    }

    /// Resolve a user listing's account and `only` filter.
    async fn prepare_user(
        &self,
        walk: &mut Walk<'_>,
        name: String,
        sub: Option<String>,
    ) -> Result<()> {
        let about = self
            .call(&format!("user/{name}/about.json"), &[])
            .await
            .map_err(|f| f.error)?;
        let user = about.get("data").cloned().unwrap_or(Value::Null);
        let resolved = string(&user, "name")
            .filter(|n| valid_name(n))
            .unwrap_or(&name)
            .to_owned();
        if let Some(listing) = walk.listing.as_mut() {
            listing.path = match &sub {
                Some(sub) => format!("user/{resolved}/{sub}/.json"),
                None => format!("user/{resolved}/.json"),
            };
        }
        let only_default = !matches!(sub.as_deref(), Some("upvoted" | "downvoted" | "saved"));
        if self.options.only.unwrap_or(only_default) {
            match string(&user, "id") {
                Some(id) => walk.only = Some(format!("t2_{id}")),
                None if truthy(user.get("from_feed")) => {}
                None if truthy(user.get("is_suspended")) => {
                    return Err(error("Reddit user is suspended"));
                }
                None => return Err(error("Reddit user not found")),
            }
        }
        walk.context.user = Some(user);
        Ok(())
    }

    /// The next part of the output, or `None` when done.
    async fn step<'a>(
        &'a self,
        walk: &mut Walk<'a>,
    ) -> Result<Option<BoxStream<'a, Result<ExtractionEvent>>>> {
        if let Some((name, sub)) = walk.user.take() {
            self.prepare_user(walk, name, sub).await?;
        }
        loop {
            if let Some(pending) = walk.pending.pop_front() {
                let unit = match pending {
                    Pending::Unit(unit) => unit,
                    Pending::Fetch(id, skip_forbidden) => match self.submission(&id).await {
                        Ok(unit) => unit,
                        Err(f) if skip_forbidden && f.status == Some(403) => continue,
                        Err(f) => return Err(f.error),
                    },
                };
                return Ok(Some(self.unit_events(unit, walk)));
            }
            if walk.listing.is_some() {
                self.next_page(walk).await?;
                continue;
            }
            // Upstream recursion into linked submissions.
            if walk.extra.is_empty()
                || walk.depth == self.options.recursion
                || walk.depth >= MAX_RECURSION
            {
                return Ok(None);
            }
            walk.depth += 1;
            let mut seen = HashSet::new();
            for id in std::mem::take(&mut walk.extra) {
                if !walk.visited.contains(&id) && seen.insert(id.clone()) {
                    walk.pending.push_back(Pending::Fetch(id, false));
                }
            }
        }
    }

    pub fn stream(&self, url: &Url) -> BoxStream<'_, Result<Media>> {
        crate::event::media_only(self.events(url))
    }

    /// Emit one post boundary per submission; only direct media links are queued.
    pub fn events(&self, url: &Url) -> BoxStream<'_, Result<ExtractionEvent>> {
        self.events_with_queue(url, direct_queue())
    }

    /// Like `events`, queueing links to other sites through `queue`.
    pub fn events_with_queue<'a>(
        &'a self,
        url: &Url,
        queue: Queue<'a>,
    ) -> BoxStream<'a, Result<ExtractionEvent>> {
        let target = match target(url) {
            Ok(target) => target,
            Err(e) => return stream::once(async move { Err(e) }).boxed(),
        };
        let walk = move |route: &'static Route,
                         subcategory: String,
                         listing: Option<Listing>,
                         pending: VecDeque<Pending>| Walk {
            listing,
            user: None,
            only: None,
            pending,
            context: Context {
                route,
                subcategory,
                user: None,
            },
            visited: HashSet::new(),
            extra: Vec::new(),
            depth: 0,
            queue,
        };
        let subcategory = |base: &str, sub: &Option<String>| match sub {
            Some(sub) => format!("{base}-{sub}"),
            None => base.to_owned(),
        };
        let listing = |path: String, params: Vec<(String, String)>| {
            Some(Listing {
                path,
                params,
                after: None,
                pages: 0,
            })
        };
        let state = match target {
            Target::Image { url } => {
                let (name, ext) = crate::sites::nameext(&url);
                let mut fields = Map::new();
                fields.insert("filename".into(), name.into());
                fields.insert("extension".into(), ext.into());
                let context = Context {
                    route: &IMAGE,
                    subcategory: "image".into(),
                    user: None,
                };
                let mut item = media(File::direct(url), fields, &context, 1);
                self.network.apply_media(&mut item);
                let events = group_with_keywords(vec![item], "reddit", KeywordPolicy::default());
                return stream::iter(events.into_iter().map(Ok)).boxed();
            }
            Target::Redirect { kind, name, code } => {
                return stream::once(async move {
                    let id = self.resolve_share(&kind, &name, &code).await?;
                    Ok::<_, crate::Error>(walk(
                        &SUBMISSION,
                        "submission".into(),
                        None,
                        VecDeque::from([Pending::Fetch(id, false)]),
                    ))
                })
                .map(move |result| match result {
                    Ok(state) => self.drive(state),
                    Err(e) => stream::once(async move { Err(e) }).boxed(),
                })
                .flatten()
                .boxed();
            }
            Target::Submission { id } => walk(
                &SUBMISSION,
                "submission".into(),
                None,
                VecDeque::from([Pending::Fetch(id, false)]),
            ),
            Target::Subreddit {
                name,
                sub,
                mut params,
            } => {
                if sub.as_deref() == Some("search")
                    && !params.iter().any(|(k, _)| k == "restrict_sr")
                {
                    params.push(("restrict_sr".into(), "1".into()));
                }
                let path = match &sub {
                    Some(sub) => format!("r/{name}/{sub}/.json"),
                    None => format!("r/{name}/.json"),
                };
                walk(
                    &SUBREDDIT,
                    subcategory("subreddit", &sub),
                    listing(path, params),
                    VecDeque::new(),
                )
            }
            Target::Home { sub, params } => {
                let path = match &sub {
                    Some(sub) => format!("{sub}/.json"),
                    None => ".json".into(),
                };
                walk(
                    &HOME,
                    subcategory("home", &sub),
                    listing(path, params),
                    VecDeque::new(),
                )
            }
            Target::User { name, sub, params } => {
                let mut state = walk(
                    &USER,
                    subcategory("user", &sub),
                    listing(String::new(), params),
                    VecDeque::new(),
                );
                state.user = Some((name, sub));
                state
            }
        };
        self.drive(state)
    }

    fn drive<'a>(&'a self, walk: Walk<'a>) -> BoxStream<'a, Result<ExtractionEvent>> {
        stream::unfold(Some(walk), move |walk| async move {
            let mut walk = walk?;
            match self.step(&mut walk).await {
                Ok(Some(part)) => Some((part, Some(walk))),
                Ok(None) => None,
                Err(e) => Some((stream::once(async move { Err(e) }).boxed(), None)),
            }
        })
        .flatten()
        .boxed()
    }

    /// Upstream `RedditRedirectExtractor`: the share link's submission ID.
    async fn resolve_share(&self, kind: &str, name: &str, code: &str) -> Result<String> {
        let endpoint = self
            .api_root
            .join(&format!("{kind}/{name}/s/{code}"))
            .map_err(|_| error("invalid Reddit share link"))?;
        let location = tokio::time::timeout(self.timeout, self.request(&endpoint, true))
            .await
            .map_err(|_| error("native Reddit extraction timed out"))?
            .map_err(|f| f.error)?;
        let unresolved = || error("Reddit share link did not resolve to a submission");
        let location = String::from_utf8(location).map_err(|_| unresolved())?;
        let url = Url::parse(API_ROOT)
            .expect("static URL")
            .join(&location)
            .map_err(|_| unresolved())?;
        match target(&url) {
            Ok(Target::Submission { id }) => Ok(id),
            _ => Err(unresolved()),
        }
    }

    /// Space API requests by the request interval.
    async fn pace(&self) {
        if self.interval.is_zero() {
            return;
        }
        let mut last = self.last_request.lock().await;
        if let Some(previous) = *last {
            let elapsed = previous.elapsed();
            if elapsed < self.interval {
                tokio::time::sleep(self.interval - elapsed).await;
            }
        }
        *last = Some(Instant::now());
    }

    /// With `capture`, a redirect's Location is returned instead of followed.
    async fn request(
        &self,
        endpoint: &Url,
        capture: bool,
    ) -> std::result::Result<Vec<u8>, Failure> {
        for attempt in 0..=self.retries {
            match self.attempt(endpoint, attempt, capture).await {
                Ok(body) => return Ok(body),
                Err(failure) if failure.retry && attempt < self.retries => {
                    tokio::time::sleep(backoff(attempt)).await;
                }
                Err(failure) => return Err(failure),
            }
        }
        unreachable!()
    }

    async fn attempt(
        &self,
        endpoint: &Url,
        attempt: u8,
        capture: bool,
    ) -> std::result::Result<Vec<u8>, Failure> {
        let mut target = endpoint.clone();
        let mut redirects = Redirects::new(endpoint);
        let mut response = loop {
            self.rate_limits.wait(&target, self.timeout).await?;
            self.pace().await;
            let mut request = self
                .client
                .get(target.clone())
                .timeout(self.request_timeout)
                .header(header::ACCEPT, "application/json");
            if let Some(cookies) = self.cookies.lock().unwrap().header(&target) {
                request = request.header(header::COOKIE, cookies);
            }
            let response = request
                .send()
                .await
                .map_err(|_| Failure::transient("Reddit request failed; check network access"))?;
            self.cookies
                .lock()
                .unwrap()
                .store_response(response.url(), response.headers());
            // Reddit sends logged-out API clients on some networks to its
            // login page (`reason=lor2`).
            if response.status().is_redirection()
                && response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|location| location.contains("/login"))
            {
                return Err(error(LOGIN_REQUIRED).into());
            }
            if capture && response.status().is_redirection() {
                let location = response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .filter(|v| v.len() <= 4096)
                    .ok_or_else(|| error("Reddit share link did not resolve to a submission"))?;
                return Ok(location.as_bytes().to_vec());
            }
            match redirects.next(response.url(), response.status(), response.headers())? {
                Some(next) => target = next,
                None => break response,
            }
        };
        let status = response.status();
        // Upstream waits for the reset when fewer than two requests remain.
        let header_number = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<f64>().ok())
                .filter(|v| v.is_finite() && *v >= 0.0)
        };
        if header_number("x-ratelimit-remaining").is_some_and(|r| r < 2.0) {
            let reset = header_number("x-ratelimit-reset")
                .unwrap_or(60.0)
                .min(900.0);
            self.rate_limits
                .defer(endpoint, Duration::from_secs_f64(reset));
        }
        if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
            let delay = retry_after(response.headers(), SystemTime::now());
            self.rate_limits
                .defer(endpoint, delay.unwrap_or_else(|| backoff(attempt)));
            return Err(Failure::transient(
                if status == StatusCode::TOO_MANY_REQUESTS {
                    match delay {
                        Some(delay) => {
                            format!("Reddit rate limit; retry after {} seconds", delay.as_secs())
                        }
                        None => "Reddit rate limit; retry later".into(),
                    }
                } else {
                    format!("Reddit returned HTTP {status}")
                },
            ));
        }
        if capture {
            return Err(error("Reddit share link did not resolve to a submission").into());
        }
        let terminal = |message: &str| Failure {
            error: error(message.to_owned()),
            retry: false,
            status: Some(status.as_u16()),
        };
        if matches!(status.as_u16(), 404 | 410) {
            return Err(terminal("Reddit submission, subreddit or user not found"));
        }
        if matches!(status.as_u16(), 401 | 403) {
            let root = Url::parse("https://www.reddit.com/").unwrap();
            if self
                .cookies
                .lock()
                .unwrap()
                .value(&root, "reddit_session")
                .is_none()
                && response
                    .headers()
                    .get(header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|v| v.starts_with("text/html"))
            {
                return Err(terminal(LOGIN_REQUIRED));
            }
            return Err(terminal(
                "Reddit refused access (private, quarantined or banned content, or blocked client)",
            ));
        }
        if !status.is_success() {
            return Err(terminal(&format!("Reddit returned HTTP {status}")));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| Failure::transient("could not read Reddit response"))?
        {
            if body.len() + chunk.len() > MAX_RESPONSE {
                return Err(error("Reddit response exceeded 16 MiB").into());
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }
}

fn backoff(attempt: u8) -> Duration {
    Duration::from_millis(250 * (1u64 << attempt.min(6)))
}

struct Failure {
    error: crate::Error,
    retry: bool,
    /// HTTP or API status of a terminal failure.
    status: Option<u16>,
}

impl Failure {
    fn transient(message: impl Into<String>) -> Self {
        Self {
            error: error(message),
            retry: true,
            status: None,
        }
    }
}

impl From<crate::Error> for Failure {
    fn from(error: crate::Error) -> Self {
        Self {
            error,
            retry: false,
            status: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::Mutex,
        thread::{self, JoinHandle},
    };

    fn keywords(media: &Media) -> &Map<String, Value> {
        &media
            .metadata
            .as_ref()
            .unwrap()
            .site_keywords
            .as_ref()
            .unwrap()
            .fields
    }

    fn context() -> Context {
        Context {
            route: &SUBMISSION,
            subcategory: "submission".into(),
            user: None,
        }
    }

    fn run(post: Value, options: &Options) -> Output {
        process(
            Unit {
                submission: Some(post),
                comments: Vec::new(),
            },
            options,
            &context(),
        )
    }

    fn image_post(id: &str) -> Value {
        json!({"id": id, "author": "alice", "title": "A title", "created_utc": 1704164645.0,
               "subreddit": "pics", "is_self": false, "is_video": false,
               "url": format!("https://i.redd.it/img{id}.jpg"),
               "preview": {"images": [{"source": {"url": "x", "width": 800, "height": 600}}]}})
    }

    fn submission(post: Value) -> Vec<u8> {
        submission_with(post, json!([]))
    }

    fn submission_with(post: Value, comments: Value) -> Vec<u8> {
        json!([{"kind": "Listing", "data": {"children": [{"kind": "t3", "data": post}]}},
               {"kind": "Listing", "data": {"children": comments}}])
        .to_string()
        .into_bytes()
    }

    fn listing(children: Vec<Value>, after: Option<&str>) -> Vec<u8> {
        let children: Vec<_> = children
            .into_iter()
            .map(|p| {
                let kind = if p.get("body_html").is_some() {
                    "t1"
                } else {
                    "t3"
                };
                json!({"kind": kind, "data": p})
            })
            .collect();
        json!({"kind": "Listing", "data": {"after": after, "children": children}})
            .to_string()
            .into_bytes()
    }

    #[test]
    fn image_posts_carry_upstream_keywords_and_paths() {
        let out = run(image_post("abc12"), &Options::default());
        assert_eq!(out.media.len(), 1);
        assert_eq!(out.visited.as_deref(), Some("abc12"));
        let media = &out.media[0];
        assert_eq!(media.url.as_str(), "https://i.redd.it/imgabc12.jpg");
        assert_eq!(media.filename, "imgabc12.jpg");
        let k = keywords(media);
        assert_eq!(k["id"], "abc12");
        assert_eq!(k["num"], 0);
        assert_eq!(k["comment"], Value::Null);
        assert_eq!(k["date"], "2024-01-02 03:04:05");
        assert_eq!(k["filename"], "imgabc12");
        assert_eq!(k["extension"], "jpg");
        assert_eq!(k["category"], "reddit");
        assert_eq!(k["subcategory"], "submission");
        assert_eq!(k["preview"]["images"][0]["source"]["width"], 800);
        let meta = media.metadata.as_ref().unwrap();
        assert_eq!(meta.post_id, "abc12");
        assert_eq!(meta.media_id, "imgabc12");
        assert_eq!(meta.username.as_deref(), Some("alice"));
        assert_eq!(meta.caption.as_deref(), Some("A title"));
        assert_eq!(meta.timestamp, Some(1704164645));
        assert!(media.archive_key().unwrap().starts_with("reddit:v1:"));
        let (file, dir) = crate::sites::default_templates(media).unwrap();
        assert_eq!(file.render(media).unwrap(), "abc12 A title.jpg");
        assert_eq!(
            dir.render(media).unwrap().unwrap(),
            std::path::PathBuf::from("reddit/pics")
        );
    }

    #[test]
    fn galleries_follow_item_order_and_number_files() {
        let post = json!({"id": "gal1", "author": "bob", "title": "G", "subreddit": "pics",
            "url": "https://www.reddit.com/gallery/gal1", "is_gallery": true,
            "gallery_data": {"items": [
                {"media_id": "bbb", "caption": "second id first"},
                {"media_id": "aaa"},
                {"media_id": "missing"},
                {"media_id": "fail"},
                {"media_id": "ccc"},
                {"media_id": "evil"}]},
            "media_metadata": {
                "aaa": {"status": "valid", "e": "Image", "m": "image/png",
                        "s": {"u": "https://preview.redd.it/aaa.png?width=1&s=sig", "x": 10, "y": 20}},
                "bbb": {"status": "valid", "e": "Image", "m": "image/jpg",
                        "s": {"u": "https://preview.redd.it/bbb.jpg?s=1", "x": 1, "y": 2}},
                "fail": {"status": "failed"},
                "ccc": {"status": "valid", "e": "AnimatedImage", "m": "image/gif",
                        "s": {"gif": "https://i.redd.it/ccc.gif", "mp4": "https://i.redd.it/ccc.mp4"}},
                "evil": {"status": "valid", "e": "Image", "s": {"u": "https://evil.example/x.jpg"}}}});
        let out = run(post, &Options::default());
        let urls: Vec<_> = out.media.iter().map(|m| m.url.as_str()).collect();
        assert_eq!(
            urls,
            [
                "https://i.redd.it/bbb.jpg",
                "https://i.redd.it/aaa.png",
                "https://i.redd.it/ccc.gif"
            ]
        );
        let nums: Vec<_> = out
            .media
            .iter()
            .map(|m| keywords(m)["num"].clone())
            .collect();
        assert_eq!(nums, [json!(1), json!(2), json!(3)]);
        let meta = out.media[0].metadata.as_ref().unwrap();
        assert_eq!(meta.media_id, "bbb");
        let file = meta.file.as_ref().unwrap();
        assert_eq!(file.description.as_deref(), Some("second id first"));
        assert_eq!((file.width, file.height), (Some(1), Some(2)));
        assert_ne!(out.media[0].archive_key(), out.media[1].archive_key());
        let (template, _) = crate::sites::default_templates(&out.media[1]).unwrap();
        assert_eq!(template.render(&out.media[1]).unwrap(), "gal1 02 G.png");
        assert!(out.queue.is_empty());
    }

    #[test]
    fn embeds_and_manifests_from_media_metadata() {
        let post = json!({"id": "emb1", "title": "E", "is_self": true,
            "url": "https://www.reddit.com/r/x/comments/emb1/e/",
            "media_metadata": {
                "img1": {"status": "valid", "e": "Image", "m": "image/webp",
                         "s": {"u": "https://preview.redd.it/img1.webp?s=1", "x": 5, "y": 6}},
                "ext": {"status": "valid", "e": "Image",
                        "s": {"u": "https://external-preview.redd.it/abc.jpg?auto=webp&s=2"}},
                "vid": {"status": "valid", "e": "RedditVideo",
                        "dashUrl": "https://v.redd.it/link/vid/asset/DASHPlaylist.mpd?a=1"},
                "hls": {"status": "valid", "e": "RedditVideo",
                        "hlsUrl": "https://v.redd.it/link/hls/asset/HLSPlaylist.m3u8"},
                "bad": {"status": "unprocessed"}}});
        let out = run(post.clone(), &Options::default());
        assert_eq!(out.media.len(), 4);
        assert_eq!(out.media[0].url.as_str(), "https://i.redd.it/img1.webp");
        assert_eq!(
            out.media[1].url.as_str(),
            "https://external-preview.redd.it/abc.jpg?auto=webp&s=2"
        );
        assert!(matches!(
            &out.media[2].job,
            MediaJob::Manifest { protocol: ManifestProtocol::Dash, base_url, .. }
                if base_url.as_str() == "https://v.redd.it/link/vid/asset/DASHPlaylist.mpd?a=1"
        ));
        assert!(
            out.media[2]
                .url
                .as_str()
                .starts_with("ytdl:https://v.redd.it/")
        );
        assert_eq!(out.media[2].handoff["_ytdl_manifest"], "dash");
        assert_eq!(keywords(&out.media[2])["extension"], "mpd");
        assert!(matches!(
            &out.media[3].job,
            MediaJob::Manifest {
                protocol: ManifestProtocol::Hls,
                ..
            }
        ));
        assert_eq!(keywords(&out.media[3])["num"], 4);
        let disabled = Options {
            embeds: false,
            ..Options::default()
        };
        assert!(run(post, &disabled).media.is_empty());
    }

    fn video_post() -> Value {
        json!({"id": "vid1", "author": "c", "title": "V", "is_video": true, "is_self": false,
            "url": "https://v.redd.it/abcVID", "subreddit": "videos",
            "permalink": "/r/videos/comments/vid1/v/",
            "secure_media": {"reddit_video": {
                "fallback_url": "https://v.redd.it/abcVID/DASH_720.mp4?source=fallback",
                "dash_url": "https://v.redd.it/abcVID/DASHPlaylist.mpd?a=1",
                "width": 1280, "height": 720, "duration": 12}}})
    }

    #[test]
    fn video_modes_follow_the_videos_setting_with_mp4_fallbacks() {
        let out = run(video_post(), &Options::default());
        let video = &out.media[0];
        assert!(matches!(
            &video.job,
            MediaJob::Manifest { protocol: ManifestProtocol::Dash, base_url, .. }
                if base_url.as_str() == "https://v.redd.it/abcVID/DASHPlaylist.mpd?a=1"
        ));
        assert_eq!(video.handoff["_ytdl_manifest"], "dash");
        assert_eq!(video.handoff["_ytdl_extra"], json!({"title": "V"}));
        assert_eq!(
            video.handoff["_fallback"],
            json!(["https://v.redd.it/abcVID/DASH_720.mp4?source=fallback"])
        );
        assert_eq!(video.filename, "abcVID.mp4");
        let k = keywords(video);
        assert_eq!(k["filename"], "abcVID");
        assert_eq!(k["extension"], "mp4");
        let meta = video.metadata.as_ref().unwrap();
        assert_eq!(meta.media_id, "abcVID");
        assert_eq!(meta.file.as_ref().unwrap().duration, Some(12.0));

        let mode = |videos| {
            run(
                video_post(),
                &Options {
                    videos,
                    ..Options::default()
                },
            )
        };
        let ytdl = mode(Videos::Ytdl);
        assert!(matches!(
            &ytdl.media[0].job,
            MediaJob::VideoPage { page_url }
                if page_url.as_str() == "https://www.reddit.com/r/videos/comments/vid1/v/"
        ));
        assert!(ytdl.media[0].handoff.contains_key("_fallback"));
        let plain = mode(Videos::Url);
        assert!(matches!(
            &plain.media[0].job,
            MediaJob::VideoPage { page_url } if page_url.as_str() == "https://v.redd.it/abcVID"
        ));
        let off = mode(Videos::Disabled);
        assert!(off.media.is_empty() && off.queue.is_empty());
    }

    #[test]
    fn crossposts_use_the_parent_media() {
        let mut post = json!({"id": "x1", "title": "Cross", "is_self": false, "is_video": false,
            "url": "/r/other/comments/p1/t/", "subreddit": "here"});
        post["crosspost_parent_list"] = json!([image_post("p1")]);
        let out = run(post, &Options::default());
        assert_eq!(out.media.len(), 1);
        assert_eq!(out.media[0].url.as_str(), "https://i.redd.it/imgp1.jpg");
        let k = keywords(&out.media[0]);
        assert_eq!(k["id"], "x1");
        assert_eq!(k["subreddit"], "here");
    }

    #[test]
    fn links_are_queued_or_followed_like_upstream() {
        let html = [
            "<a href=\"https://www.redgifs.com/watch/x?a=1&amp;b=2\">g</a>",
            "<a href=\"/r/pics/comments/zz9/t/\">sub</a>",
            "<a href=\"https://redd.it/yy8\">short</a>",
            "<a href=\"/u/someone\">user</a>",
            "<a href=\"https://www.reddit.com/r/pics/\">subreddit</a>",
            "<a href=\"#anchor\">anchor</a>",
            "<a href=\"https://www.reddit.com/message/compose?to=x\">msg</a>",
            "<a href=\"https://preview.redd.it/p.jpg?s=1\">preview</a>",
            "<a href=\"javascript:alert(1)\">js</a>",
            "<a href=\"https://i.redd.it/direct.png\">direct</a>",
        ]
        .concat();
        let post = json!({"id": "lnk1", "title": "L", "is_self": false, "is_video": false,
            "url": "https://imgur.com/a/AbCdE", "selftext_html": html});
        let out = run(post.clone(), &Options::default());
        let queued: Vec<_> = out.queue.iter().map(|(u, _)| u.as_str()).collect();
        assert_eq!(queued, ["https://imgur.com/a/AbCdE"]);
        assert!(out.extra.is_empty());
        let selftext = Options {
            selftext: Some(true),
            ..Options::default()
        };
        let out = run(post, &selftext);
        let queued: Vec<_> = out.queue.iter().map(|(u, _)| u.as_str()).collect();
        assert_eq!(
            queued,
            [
                "https://imgur.com/a/AbCdE",
                "https://www.redgifs.com/watch/x?a=1&b=2",
                "https://i.redd.it/direct.png",
            ]
        );
        assert_eq!(out.extra, ["zz9", "yy8"]);
        assert_eq!(out.queue[0].1["id"], "lnk1");
    }

    #[test]
    fn comments_contribute_embeds_and_links() {
        let comments = vec![
            json!({"id": "c1", "author": "dan", "created_utc": 1704164700.0,
                "body_html": "<p>no links</p>"}),
            json!({"id": "c2", "author": "eve", "created_utc": 1704164800.0,
                "body_html": "<a href=\"https://example.com/page\">x</a>",
                "media_metadata": {"cm1": {"status": "valid", "e": "Image",
                    "s": {"u": "https://preview.redd.it/cm1.png?s=1"}}}}),
        ];
        let options = Options {
            comments: 10,
            ..Options::default()
        };
        let out = process(
            Unit {
                submission: Some(image_post("abc12")),
                comments: comments.clone(),
            },
            &options,
            &context(),
        );
        assert_eq!(out.media.len(), 2);
        let k = keywords(&out.media[1]);
        assert_eq!(k["comment"]["id"], "c2");
        assert_eq!(k["num"], 1);
        assert_eq!(k["date"], "2024-01-02 03:06:40");
        assert_eq!(
            out.media[1].metadata.as_ref().unwrap().username.as_deref(),
            Some("eve")
        );
        assert_eq!(out.queue.len(), 1);
        assert_eq!(out.queue[0].1["comment"]["id"], "c2");

        // Comment-only units (user comment listings) use the comment itself.
        let mut lone = comments[1].clone();
        lone["link_id"] = json!("t3_abc12");
        lone["link_title"] = json!("Parent");
        let out = process(
            Unit {
                submission: None,
                comments: vec![lone],
            },
            &options,
            &context(),
        );
        assert_eq!(out.media.len(), 1);
        let meta = out.media[0].metadata.as_ref().unwrap();
        assert_eq!(meta.post_id, "abc12");
        assert_eq!(meta.caption.as_deref(), Some("Parent"));
        let (file, _) = crate::sites::default_templates(&out.media[0]).unwrap();
        assert_eq!(file.render(&out.media[0]).unwrap(), "c2 01 Parent.png");
        // Without the comments setting comment-only units yield nothing.
        let out = process(
            Unit {
                submission: None,
                comments,
            },
            &Options::default(),
            &context(),
        );
        assert!(out.media.is_empty() && out.queue.is_empty());
    }

    #[test]
    fn flattens_comment_trees_breadth_first() {
        let listing = json!({"data": {"children": [
            {"kind": "t1", "data": {"id": "a", "replies": {"data": {"children": [
                {"kind": "t1", "data": {"id": "a1", "replies": ""}},
                {"kind": "more", "data": {"children": ["m1", "m2"]}}]}}}},
            {"kind": "t1", "data": {"id": "b", "replies": ""}},
            {"kind": "more", "data": {"children": ["m3", "BAD!"]}}]}});
        let (comments, more) = flatten(&listing);
        let ids: Vec<_> = comments.iter().map(|c| c["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["a", "b", "a1"]);
        assert_eq!(more, ["m3", "m1", "m2"]);
    }

    #[test]
    fn classifies_supported_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        for raw in [
            "https://www.reddit.com/r/pics/comments/abc12/some_title/",
            "https://old.reddit.com/r/pics/comments/ABC12",
            "https://np.reddit.com/comments/abc12",
            "https://reddit.com/comments/abc12/x/",
            "https://www.reddit.com/gallery/abc12",
            "https://www.reddit.com/user/bob/comments/abc12/t/",
            "https://redd.it/abc12",
        ] {
            assert_eq!(
                t(raw).unwrap(),
                Target::Submission { id: "abc12".into() },
                "{raw}"
            );
        }
        let pairs = |items: &[(&str, &str)]| -> Vec<(String, String)> {
            items
                .iter()
                .map(|(k, v)| ((*k).into(), (*v).into()))
                .collect()
        };
        assert_eq!(
            t("https://www.reddit.com/r/pics/").unwrap(),
            Target::Subreddit {
                name: "pics".into(),
                sub: None,
                params: vec![]
            }
        );
        assert_eq!(
            t("https://www.reddit.com/r/pics/top/?t=week").unwrap(),
            Target::Subreddit {
                name: "pics".into(),
                sub: Some("top".into()),
                params: pairs(&[("t", "week")])
            }
        );
        assert_eq!(
            t("https://reddit.com/r/pics/search?q=cats").unwrap(),
            Target::Subreddit {
                name: "pics".into(),
                sub: Some("search".into()),
                params: pairs(&[("q", "cats")])
            }
        );
        assert_eq!(
            t("https://www.reddit.com/").unwrap(),
            Target::Home {
                sub: None,
                params: vec![]
            }
        );
        assert_eq!(
            t("https://www.reddit.com/best/?after=x").unwrap(),
            Target::Home {
                sub: Some("best".into()),
                params: pairs(&[("after", "x")])
            }
        );
        assert_eq!(
            t("https://www.reddit.com/user/bob").unwrap(),
            Target::User {
                name: "bob".into(),
                sub: None,
                params: vec![]
            }
        );
        assert_eq!(
            t("https://reddit.com/u/bob/submitted/?sort=top").unwrap(),
            Target::User {
                name: "bob".into(),
                sub: Some("submitted".into()),
                params: pairs(&[("sort", "top")])
            }
        );
        assert_eq!(
            t("https://www.reddit.com/r/pics/s/abc456GHIJ").unwrap(),
            Target::Redirect {
                kind: "r".into(),
                name: "pics".into(),
                code: "abc456GHIJ".into()
            }
        );
        assert_eq!(
            t("https://www.reddit.com/u/bob/s/abc456GHIJ").unwrap(),
            Target::Redirect {
                kind: "user".into(),
                name: "bob".into(),
                code: "abc456GHIJ".into()
            }
        );
        let image = |raw: &str| match t(raw).unwrap() {
            Target::Image { url } => url.to_string(),
            other => panic!("{other:?}"),
        };
        assert_eq!(
            image("https://preview.redd.it/abc.png?width=1&s=x"),
            "https://i.redd.it/abc.png"
        );
        assert_eq!(
            image("https://i.reddituploads.com/abc?fit=max&s=x"),
            "https://i.reddituploads.com/abc?fit=max&s=x"
        );
        for raw in [
            "https://www.reddit.com/r/pics/comments/a%2Fb",
            "https://www.reddit.com/r/pics/TOP",
            "https://www.reddit.com/r/pics/s/short",
            "https://reddit.com.evil.test/r/pics",
            "https://evil-reddit.com/r/pics",
            "https://a.b.reddit.com/r/pics",
            "https://www.reddit.com:444/r/pics",
            "https://v.redd.it/abc",
            "https://i.redd.it/a/b.jpg",
        ] {
            assert!(t(raw).is_err(), "{raw}");
        }
    }

    #[test]
    fn options_parse_upstream_types() {
        let parse = |value: Value| Options::parse(value.as_object().unwrap());
        let options = parse(
            json!({"comments": "50", "morecomments": true, "recursion": -1,
            "videos": "ytdl", "previews": false, "embeds": false, "selftext": true, "only": false,
            "id-min": "t3_10", "id-max": "zik0zj", "limit": 25, "parent-directory": true}),
        )
        .unwrap();
        assert_eq!(options.comments, 50);
        assert!(options.morecomments && !options.previews && !options.embeds);
        assert_eq!(options.recursion, -1);
        assert_eq!(options.videos, Videos::Ytdl);
        assert_eq!((options.selftext, options.only), (Some(true), Some(false)));
        assert_eq!((options.id_min, options.id_max), (Some(36), None));
        assert_eq!(options.limit, Some(25));
        assert_eq!(
            parse(json!({"videos": false})).unwrap().videos,
            Videos::Disabled
        );
        assert_eq!(parse(json!({"videos": true})).unwrap().videos, Videos::Url);
        assert_eq!(parse(json!({})).unwrap().videos, Videos::Dash);
        for bad in [
            json!({"videos": "hls"}),
            json!({"comments": "many"}),
            json!({"embeds": 1}),
            json!({"id-min": "not base36!"}),
            json!({"id-max": 5}),
        ] {
            assert!(parse(bad.clone()).is_err(), "{bad}");
        }
    }

    fn response(status: &str, headers: &str, body: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n",
            body.len()
        )
        .into_bytes();
        out.extend_from_slice(body);
        out
    }

    fn ok(body: &[u8]) -> Vec<u8> {
        response("200 OK", "", body)
    }

    fn serve(responses: Vec<Vec<u8>>) -> (Url, JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let root = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let task = thread::spawn(move || {
            let mut requests = Vec::new();
            for reply in responses {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    let count = stream.read(&mut buffer).unwrap();
                    assert_ne!(count, 0);
                    request.extend_from_slice(&buffer[..count]);
                }
                requests.push(String::from_utf8(request).unwrap());
                let _ = stream.write_all(&reply);
            }
            requests
        });
        (root, task)
    }

    fn reddit(root: Url) -> Reddit {
        Reddit::new(Duration::from_secs(5))
            .unwrap()
            .with_network(&NetworkOptions::new(Some(String::new()), None).unwrap())
            .unwrap()
            .with_request_interval(Duration::ZERO)
            .with_api_root(root)
    }

    fn with(reddit: Reddit, options: Value) -> Reddit {
        reddit.with_options(options.as_object()).unwrap()
    }

    fn files(events: &[Result<ExtractionEvent>]) -> Vec<&Media> {
        events
            .iter()
            .filter_map(|e| match e {
                Ok(ExtractionEvent::Media(m)) => Some(m.as_ref()),
                _ => None,
            })
            .collect()
    }

    /// A dispatcher recording queued URLs without extracting them.
    fn recording<'a>() -> (Queue<'a>, Arc<Mutex<Vec<String>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = seen.clone();
        let queue: Queue<'a> = Arc::new(move |url: Url| {
            if url.host_str() == Some("unsupported.example") {
                return None;
            }
            record.lock().unwrap().push(url.to_string());
            Some(stream::empty().boxed())
        });
        (queue, seen)
    }

    #[tokio::test]
    async fn fetches_submission_with_retries_and_user_agent() {
        let (root, server) = serve(vec![
            response("429 Too Many Requests", "Retry-After: 0\r\n", b""),
            ok(&submission(image_post("abc12"))),
        ]);
        let events: Vec<_> = reddit(root)
            .with_retries(1)
            .events(&Url::parse("https://redd.it/abc12").unwrap())
            .collect()
            .await;
        assert_eq!(events.len(), 3, "{events:?}");
        assert!(matches!(events[1], Ok(ExtractionEvent::Media(_))));
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /comments/abc12/.json?limit=0&raw_json=1 "));
        assert!(
            requests[1]
                .to_ascii_lowercase()
                .contains("user-agent: gallery-dl-rs/")
        );
    }

    #[tokio::test]
    async fn paginates_listings_with_params_limit_and_id_range() {
        let (root, server) = serve(vec![
            ok(&listing(
                vec![image_post("a1"), image_post("zz")],
                Some("t3_a1"),
            )),
            ok(&listing(vec![image_post("b2")], None)),
        ]);
        let native = with(reddit(root), json!({"limit": 50, "id-max": "b2"}));
        let events: Vec<_> = native
            .events(&Url::parse("https://www.reddit.com/r/pics/top/?t=week&limit=5").unwrap())
            .collect()
            .await;
        let media = files(&events);
        assert_eq!(media.len(), 2);
        assert_eq!(keywords(media[0])["subcategory"], "subreddit-top");
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with("GET /r/pics/top/.json?t=week&limit=50&raw_json=1 "),
            "{}",
            requests[0]
        );
        assert!(requests[1].contains("&after=t3_a1&raw_json=1 "));
    }

    #[tokio::test]
    async fn login_walls_fall_back_to_the_public_feed() {
        let feed = r#"<?xml version="1.0" encoding="UTF-8"?><feed xmlns="http://www.w3.org/2005/Atom"><entry><author><name>/u/alice</name></author><category term="pics"/><content type="html">&lt;a href=&quot;https://i.redd.it/abc.jpeg&quot;&gt;[link]&lt;/a&gt;</content><id>t3_a1</id><link href="https://www.reddit.com/r/pics/comments/a1/x/"/><published>2026-10-20T10:00:00+00:00</published><title>A</title></entry></feed>"#;
        let empty = r#"<?xml version="1.0" encoding="UTF-8"?><feed xmlns="http://www.w3.org/2005/Atom"></feed>"#;
        let wall = || {
            response(
                "302 Found",
                "Location: https://old.reddit.com/login/?reason=lor2\r\n",
                b"",
            )
        };
        let (root, server) = serve(vec![
            wall(),
            response(
                "200 OK",
                "Content-Type: application/atom+xml\r\n",
                feed.as_bytes(),
            ),
            wall(),
            response(
                "200 OK",
                "Content-Type: application/atom+xml\r\n",
                empty.as_bytes(),
            ),
        ]);
        let native = reddit(root);
        let events: Vec<_> = native
            .events(&Url::parse("https://www.reddit.com/r/pics/top/").unwrap())
            .collect()
            .await;
        let files = files(&events);
        assert_eq!(files.len(), 1, "{events:?}");
        assert_eq!(files[0].url.as_str(), "https://i.redd.it/abc.jpeg");
        let requests = server.join().unwrap();
        assert!(
            requests[1].starts_with("GET /r/pics/top/.rss?"),
            "{}",
            requests[1]
        );
        assert!(requests[3].contains("after=t3_a1"), "{}", requests[3]);
    }

    #[tokio::test]
    async fn session_cookies_are_sent_and_login_walls_ask_for_them() {
        let (root, server) = serve(vec![
            ok(&listing(vec![image_post("h1")], None)),
            response(
                "302 Found",
                "Location: https://old.reddit.com/login/?reason=lor2\r\n",
                b"",
            ),
            // The feed fallback is refused as well, so the wall is reported.
            response("404 Not Found", "", b""),
            response(
                "403 Forbidden",
                "Content-Type: text/html\r\n",
                b"<html></html>",
            ),
            response("404 Not Found", "", b""),
        ]);
        let host = root.host_str().unwrap().to_owned();
        let jar = crate::cookies::CookieJar::parse(&format!(
            "{host}\tFALSE\t/\tFALSE\t0\ttoken_v2\tsecret\n"
        ))
        .unwrap();
        let native = reddit(root).with_cookies(jar);
        let events: Vec<_> = native
            .events(&Url::parse("https://www.reddit.com/").unwrap())
            .collect()
            .await;
        assert_eq!(files(&events).len(), 1);
        for _ in 0..2 {
            let events: Vec<_> = native
                .events(&Url::parse("https://www.reddit.com/").unwrap())
                .collect()
                .await;
            let message = events[0].as_ref().err().unwrap().to_string();
            assert!(message.contains("--cookies"), "{message}");
            assert!(!message.contains("lor2"), "{message}");
        }
        let requests = server.join().unwrap();
        assert!(requests[0].contains("token_v2=secret"), "{}", requests[0]);
    }

    #[tokio::test]
    async fn search_restricts_to_subreddit_and_home_uses_front_page() {
        let (root, server) = serve(vec![
            ok(&listing(vec![], None)),
            ok(&listing(vec![image_post("h1")], None)),
        ]);
        let native = reddit(root);
        let none: Vec<_> = native
            .events(&Url::parse("https://www.reddit.com/r/pics/search?q=cat").unwrap())
            .collect()
            .await;
        assert!(none.is_empty());
        let events: Vec<_> = native
            .events(&Url::parse("https://www.reddit.com/").unwrap())
            .collect()
            .await;
        assert_eq!(keywords(files(&events)[0])["subcategory"], "home");
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with("GET /r/pics/search/.json?q=cat&restrict_sr=1&raw_json=1 ")
        );
        assert!(requests[1].starts_with("GET /.json?raw_json=1 "));
    }

    #[tokio::test]
    async fn user_listings_resolve_the_account_and_filter_foreign_posts() {
        let mut own = image_post("o1");
        own["author_fullname"] = json!("t2_u1");
        let mut foreign = image_post("f1");
        foreign["author_fullname"] = json!("t2_other");
        let about = json!({"kind": "t2", "data": {"id": "u1", "name": "Bob"}}).to_string();
        let (root, server) = serve(vec![
            ok(about.as_bytes()),
            ok(&listing(vec![own, foreign], None)),
        ]);
        let events: Vec<_> = reddit(root)
            .events(&Url::parse("https://www.reddit.com/user/bob/submitted/").unwrap())
            .collect()
            .await;
        let media = files(&events);
        assert_eq!(media.len(), 1);
        let k = keywords(media[0]);
        assert_eq!(k["id"], "o1");
        assert_eq!(k["user"]["name"], "Bob");
        assert_eq!(k["subcategory"], "user-submitted");
        let (_, dir) = crate::sites::default_templates(media[0]).unwrap();
        assert_eq!(
            dir.render(media[0]).unwrap().unwrap(),
            std::path::PathBuf::from("reddit/Users/Bob")
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /user/bob/about.json?raw_json=1 "));
        assert!(requests[1].starts_with("GET /user/Bob/submitted/.json?raw_json=1 "));

        // Saved listings keep other authors by default.
        let (root, server) = serve(vec![
            ok(about.as_bytes()),
            ok(&listing(vec![image_post("s1")], None)),
        ]);
        let events: Vec<_> = reddit(root)
            .events(&Url::parse("https://www.reddit.com/user/bob/saved").unwrap())
            .collect()
            .await;
        assert_eq!(files(&events).len(), 1);
        server.join().unwrap();

        // Suspended users fail when `only` applies.
        let suspended = json!({"data": {"is_suspended": true, "name": "gone"}}).to_string();
        let (root, server) = serve(vec![ok(suspended.as_bytes())]);
        let error = reddit(root)
            .events(&Url::parse("https://www.reddit.com/user/gone").unwrap())
            .collect::<Vec<_>>()
            .await
            .remove(0)
            .unwrap_err();
        assert!(error.to_string().contains("suspended"));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn comments_fetch_submissions_and_expand_more_children() {
        let mut post = image_post("abc12");
        post["num_comments"] = json!(3);
        let comments = json!([
            {"kind": "t1", "data": {"id": "c1", "created_utc": 1.0,
                "body_html": "<a href=\"https://example.com/one\">1</a>", "replies": ""}},
            {"kind": "more", "data": {"children": ["c2"]}}]);
        let more = json!({"json": {"data": {"things": [
            {"kind": "t1", "data": {"id": "c2", "created_utc": 2.0,
                "body_html": "<a href=\"https://example.com/two\">2</a>"}}]}}});
        let (root, server) = serve(vec![
            ok(&listing(vec![post.clone()], None)),
            ok(&submission_with(post, comments)),
            ok(more.to_string().as_bytes()),
        ]);
        let native = with(reddit(root), json!({"comments": 5, "morecomments": true}));
        let (queue, seen) = recording();
        let events: Vec<_> = native
            .events_with_queue(
                &Url::parse("https://www.reddit.com/r/pics/new").unwrap(),
                queue,
            )
            .collect()
            .await;
        assert_eq!(files(&events).len(), 1);
        assert_eq!(
            *seen.lock().unwrap(),
            ["https://example.com/one", "https://example.com/two"]
        );
        let children = events
            .iter()
            .filter(|e| matches!(e, Ok(ExtractionEvent::BeginChild(_))))
            .count();
        assert_eq!(children, 2);
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /comments/abc12/.json?limit=5&raw_json=1 "));
        assert!(requests[2].starts_with(
            "GET /api/morechildren?link_id=t3_abc12&api_type=json&children=c2&raw_json=1 "
        ));
    }

    #[tokio::test]
    async fn forbidden_comment_fetches_are_skipped_in_listings() {
        let mut post = image_post("p1");
        post["num_comments"] = json!(1);
        let mut second = image_post("p2");
        second["num_comments"] = json!(0);
        let (root, server) = serve(vec![
            ok(&listing(vec![post, second], None)),
            response("403 Forbidden", "", b"{\"error\": 403}"),
        ]);
        let native = with(reddit(root), json!({"comments": 1}));
        let events: Vec<_> = native
            .events(&Url::parse("https://www.reddit.com/r/pics/").unwrap())
            .collect()
            .await;
        assert!(events.iter().all(Result::is_ok), "{events:?}");
        assert_eq!(keywords(files(&events)[0])["id"], "p2");
        server.join().unwrap();
    }

    #[tokio::test]
    async fn child_jobs_and_recursion() {
        let mut first = image_post("aa1");
        first["selftext_html"] = json!(
            [
                "<a href=\"https://www.reddit.com/r/pics/comments/bb2/t/\">next</a>",
                "<a href=\"https://unsupported.example/x\">skip</a>",
                "<a href=\"https://www.redgifs.com/watch/clip\">clip</a>",
            ]
            .concat()
        );
        let mut second = image_post("bb2");
        second["selftext_html"] =
            json!("<a href=\"https://www.reddit.com/r/pics/comments/cc3/t/\">deeper</a>");
        let (root, server) = serve(vec![
            ok(&submission(first.clone())),
            ok(&submission(second)),
        ]);
        let native = with(reddit(root), json!({"recursion": 1, "selftext": true}));
        let (queue, seen) = recording();
        let events: Vec<_> = native
            .events_with_queue(&Url::parse("https://redd.it/aa1").unwrap(), queue)
            .collect()
            .await;
        let ids: Vec<_> = files(&events)
            .iter()
            .map(|m| keywords(m)["id"].clone())
            .collect();
        assert_eq!(ids, [json!("aa1"), json!("bb2")]);
        assert_eq!(
            *seen.lock().unwrap(),
            ["https://www.redgifs.com/watch/clip"]
        );
        let scope = events
            .iter()
            .find_map(|e| match e {
                Ok(ExtractionEvent::BeginChild(scope)) => Some(scope),
                _ => None,
            })
            .unwrap();
        assert_eq!(scope.metadata.category(), "reddit");
        assert_eq!(scope.metadata.subcategory(), "submission");
        // Only two submissions were fetched; cc3 is beyond the recursion depth.
        assert_eq!(server.join().unwrap().len(), 2);

        // Without recursion, linked submissions are neither fetched nor queued.
        let (root, server) = serve(vec![ok(&submission(first))]);
        let native = with(reddit(root), json!({"selftext": true}));
        let events: Vec<_> = native
            .events(&Url::parse("https://redd.it/aa1").unwrap())
            .collect()
            .await;
        assert_eq!(files(&events).len(), 1);
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn share_links_resolve_to_their_submission() {
        let (root, server) = serve(vec![
            response(
                "301 Moved Permanently",
                "Location: https://www.reddit.com/r/pics/comments/abc12/title/?share_id=x\r\n",
                b"",
            ),
            ok(&submission(image_post("abc12"))),
        ]);
        let events: Vec<_> = reddit(root)
            .events(&Url::parse("https://www.reddit.com/r/pics/s/abc456GHIJ").unwrap())
            .collect()
            .await;
        assert_eq!(keywords(files(&events)[0])["id"], "abc12");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /r/pics/s/abc456GHIJ "));
        assert!(requests[1].starts_with("GET /comments/abc12/.json?"));
    }

    #[tokio::test]
    async fn image_links_need_no_requests() {
        let events: Vec<_> = reddit(Url::parse("http://127.0.0.1:9/").unwrap())
            .events(&Url::parse("https://preview.redd.it/xyz.jpg?width=640&s=a").unwrap())
            .collect()
            .await;
        let media = files(&events);
        assert_eq!(media[0].url.as_str(), "https://i.redd.it/xyz.jpg");
        let k = keywords(media[0]);
        assert_eq!(k["filename"], "xyz");
        assert_eq!(k["subcategory"], "image");
        let (file, dir) = crate::sites::default_templates(media[0]).unwrap();
        assert_eq!(file.render(media[0]).unwrap(), "xyz.jpg");
        assert_eq!(
            dir.render(media[0]).unwrap().unwrap(),
            std::path::PathBuf::from("reddit")
        );
    }

    #[tokio::test]
    async fn terminal_errors_are_not_retried() {
        let (root, server) = serve(vec![response("404 Not Found", "", b"{}")]);
        let error = reddit(root)
            .with_retries(3)
            .stream(&Url::parse("https://www.reddit.com/user/nobody").unwrap())
            .collect::<Vec<_>>()
            .await
            .remove(0)
            .unwrap_err();
        assert!(error.to_string().contains("not found"));
        assert!(!error.to_string().contains("127.0.0.1"));
        assert_eq!(server.join().unwrap().len(), 1);
    }
}
