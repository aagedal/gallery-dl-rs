//! Experimental native Tumblr extraction.
//!
//! Mirrors gallery_dl/extractor/tumblr.py using the public API v2 with an
//! `api_key` query parameter (upstream's default public key unless the
//! `api-key` setting overrides it) and the legacy (non-NPF) post format, or
//! OAuth 1.0a signed requests once `access-token`/`access-token-secret` are
//! set (or cached by `oauth:tumblr`). Upstream's `tumblr:` prefix selects
//! custom blog domains.
//! Every upstream class is supported: blog posts (`user`), single posts,
//! tagged posts, posts of one day, likes, followed blogs and followers
//! (queued as child jobs) and searches. Files carry upstream's keyword
//! dictionary and default path templates.
//!
//! Photo posts select the largest API rendition and resolve capped originals;
//! video posts yield `video_url`, audio posts a direct `audio_url`, and every
//! post its inline `<img>`/`<source>` media, enlarged where Tumblr supports
//! it. Reblogs are included, as upstream does by default. Media must be served
//! over HTTPS from `*.media.tumblr.com` (audio also from `a.tumblr.com`,
//! videos from `vt`/`vtt`/`ve`/`va.tumblr.com`).
use crate::{
    Result, error,
    event::{ChildMetadata, ExtractionEvent, KeywordPolicy, child_job, group_with_keywords},
    extract::{Media, MediaJob, PostMetadata},
    file_metadata::FileMetadata,
    network::NetworkOptions,
    sites::{Events, Route, SiteKeywords},
    transport::{RateLimits, Redirects, retry_after},
};
use futures_util::{
    StreamExt,
    stream::{self, BoxStream},
};
use reqwest::{Client, StatusCode, header::HeaderMap};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};
use url::Url;

const USER_AGENT: &str = concat!(
    "gallery-dl-rs/",
    env!("CARGO_PKG_VERSION"),
    " (native Tumblr extractor)"
);
/// Upstream gallery-dl's default public API key.
pub const DEFAULT_API_KEY: &str = "O3hU2tMi5e4Qs5t3vezEi6L0qRORJ5y9oUpSGsrWu8iA3UCc3B";
/// Upstream gallery-dl's default OAuth consumer secret (`TumblrAPI.API_SECRET`).
pub const DEFAULT_API_SECRET: &str = "sFdsK3PDdP2QpYMRAoq0oDnw0sFS24XigXmdfnaeNZpJpqAn03";
/// Tumblr's response to OAuth requests for blogs the token's account may not
/// view; upstream retries such requests with `api_key` authentication.
const NO_PERMISSION: &str = "You do not have permission to view this blog";
const MAX_RESPONSE: usize = 32 * 1024 * 1024;
const PAGE_SIZE: usize = 50;
/// Upstream `_pagination_blogs` page size.
const BLOG_PAGE_SIZE: u64 = 20;
const MAX_PAGES: usize = 200;
/// Rate-limit waits per request, independent of the transient retry count.
const MAX_RATE_WAITS: u8 = 3;
/// Upper bound for `fallback-retries`; upstream's negative "forever" is refused.
const MAX_FALLBACK_RETRIES: u64 = 100;
/// `*.tumblr.com` labels that are services rather than blogs.
const RESERVED_HOSTS: [&str; 12] = [
    "www", "api", "media", "assets", "static", "a", "va", "vt", "vtt", "ve", "help", "embed",
];
/// First `www.tumblr.com` path segments that are site pages, not blogs.
const RESERVED_PATHS: [&str; 30] = [
    "dashboard",
    "explore",
    "search",
    "tagged",
    "settings",
    "login",
    "register",
    "likes",
    "following",
    "inbox",
    "new",
    "blog",
    "communities",
    "about",
    "help",
    "privacy",
    "policy",
    "docs",
    "apps",
    "tv",
    "oauth",
    "logout",
    "reblog",
    "edit",
    "customize",
    "safe-mode",
    "live",
    "messaging",
    "activity",
    "post",
];

const FILENAME: &str = "{category}_{blog_name}_{id}_{num:>02}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{blog_name}"];
const ARCHIVE: &str = "{id}_{num}";
pub(crate) static ROUTE_USER: Route = Route::new("user", FILENAME, DIRECTORY, ARCHIVE);
pub(crate) static ROUTE_POST: Route = Route::new("post", FILENAME, DIRECTORY, ARCHIVE);
pub(crate) static ROUTE_TAG: Route = Route::new("tag", FILENAME, DIRECTORY, ARCHIVE);
pub(crate) static ROUTE_DAY: Route = Route::new("day", FILENAME, DIRECTORY, ARCHIVE);
pub(crate) static ROUTE_LIKES: Route = Route::new(
    "likes",
    FILENAME,
    &["{category}", "{blog_name}", "likes"],
    "f_{blog[name]}_{id}_{num}",
);
pub(crate) static ROUTE_FOLLOWING: Route = Route::new("following", FILENAME, DIRECTORY, ARCHIVE);
pub(crate) static ROUTE_FOLLOWERS: Route = Route::new("followers", FILENAME, DIRECTORY, ARCHIVE);
pub(crate) static ROUTE_SEARCH: Route = Route::new("search", FILENAME, DIRECTORY, ARCHIVE);

/// Fields assigned anew for each file of a post.
const FILE_FIELDS: &[&str] = &[
    "num",
    "count",
    "index",
    "media_id",
    "photo",
    "filename",
    "extension",
    "hash",
];

/// Blog names are 1-32 ASCII letters, digits and inner hyphens.
pub fn valid_blog(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-')
}

pub fn valid_id(id: &str) -> bool {
    (1..=24).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_digit())
}

/// Media IDs are the first 16 hex digits of the SHA-256 of the media path.
pub fn valid_media_id(id: &str) -> bool {
    id.len() == 16 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Blog name for a `BLOG.tumblr.com` host; service hosts are not blogs.
fn host_blog(host: &str) -> Option<&str> {
    let blog = host.strip_suffix(".tumblr.com")?;
    (!blog.contains('.') && !RESERVED_HOSTS.contains(&blog) && valid_blog(blog)).then_some(blog)
}

/// Blog and the index of the following path segment for `www.tumblr.com`
/// paths: `/BLOG`, `/blog/BLOG` and `/blog/view/BLOG` (upstream BASE_PATTERN).
fn www_blog<'a>(segments: &[&'a str]) -> Option<(&'a str, usize)> {
    match segments {
        ["blog", "view", blog, ..] if valid_blog(blog) => Some((blog, 3)),
        ["blog", blog, ..] if valid_blog(blog) => Some((blog, 2)),
        [first, ..] if !RESERVED_PATHS.contains(first) && valid_blog(first) => Some((first, 1)),
        _ => None,
    }
}

fn is_www(host: &str) -> bool {
    matches!(host, "www.tumblr.com" | "tumblr.com")
}

/// A custom blog domain accepted after upstream's `tumblr:` prefix.
fn valid_domain(host: &str) -> bool {
    let labels: Vec<_> = host.split('.').collect();
    host.len() <= 253
        && labels.len() >= 2
        && labels.iter().all(|label| {
            (1..=63).contains(&label.len())
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        })
}

/// The API blog identifier: `NAME.tumblr.com` for blog names, else the
/// custom domain itself (upstream `self.blog`).
fn blog_host(blog: &str) -> String {
    if blog.contains('.') {
        blog.to_owned()
    } else {
        format!("{blog}.tumblr.com")
    }
}

/// Whether a URL is a Tumblr blog or search page handled by this extractor,
/// including upstream's `tumblr:` prefix for custom blog domains.
pub fn is_blog_url(url: &Url) -> bool {
    if url.scheme() == "tumblr" {
        return target(url).is_ok();
    }
    match url.host_str() {
        Some(host) if is_www(host) => {
            let all = segments(url);
            www_blog(&all).is_some() || matches!(all.as_slice(), ["search", _, ..])
        }
        Some(host) => host_blog(host).is_some(),
        None => false,
    }
}

fn segments(url: &Url) -> Vec<&str> {
    url.path_segments()
        .map(|s| s.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// `TumblrUserExtractor`.
    Posts { blog: String },
    /// `TumblrPostExtractor`.
    Post { blog: String, id: String },
    /// `TumblrTagExtractor`.
    Tagged { blog: String, tag: String },
    /// `TumblrDayExtractor`: posts from `start` (UTC midnight) for one day.
    Day { blog: String, start: i64 },
    /// `TumblrLikesExtractor`.
    Likes { blog: String },
    /// `TumblrFollowingExtractor`.
    Following { blog: String },
    /// `TumblrFollowersExtractor`.
    Followers { blog: String },
    /// `TumblrSearchExtractor`: `/search/QUERY[/MODE[/POST_TYPE]][?PARAMS]`.
    Search {
        query: String,
        mode: Option<String>,
        post_type: Option<String>,
        params: Vec<(String, String)>,
    },
}

impl Target {
    fn route(&self) -> &'static Route {
        match self {
            Self::Posts { .. } => &ROUTE_USER,
            Self::Post { .. } => &ROUTE_POST,
            Self::Tagged { .. } => &ROUTE_TAG,
            Self::Day { .. } => &ROUTE_DAY,
            Self::Likes { .. } => &ROUTE_LIKES,
            Self::Following { .. } => &ROUTE_FOLLOWING,
            Self::Followers { .. } => &ROUTE_FOLLOWERS,
            Self::Search { .. } => &ROUTE_SEARCH,
        }
    }
}

fn percent_decode(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Bounded single-line text.
fn text_value(text: &str) -> Option<String> {
    let text = text.trim();
    ((1..=256).contains(&text.len()) && !text.chars().any(char::is_control)).then(|| text.into())
}

fn tag_of(raw: &str) -> Option<String> {
    // Upstream: text.unquote(tag.replace("-", " ")).
    text_value(&percent_decode(&raw.replace(['-', '+'], " "))?)
}

/// Search modes and post types are short ASCII words.
fn word(raw: &str) -> Option<String> {
    ((1..=32).contains(&raw.len())
        && raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')))
    .then(|| raw.into())
}

/// UTC midnight of `YYYY/MM/DD`.
fn day_start(year: &str, month: &str, day: &str) -> Option<i64> {
    let digits = |s: &str, len: usize| s.len() == len && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(year, 4) || !digits(month, 2) || !digits(day, 2) {
        return None;
    }
    let month = time::Month::try_from(month.parse::<u8>().ok()?).ok()?;
    let date = time::Date::from_calendar_date(year.parse().ok()?, month, day.parse().ok()?).ok()?;
    Some(date.midnight().assume_utc().unix_timestamp())
}

pub fn target(url: &Url) -> Result<Target> {
    let unsupported = || {
        error(
            "unsupported Tumblr URL; expected https://BLOG.tumblr.com[/post/ID|/tagged/TAG|/day/YYYY/MM/DD|/likes|/following|/followers], https://www.tumblr.com/BLOG[/ID] or https://www.tumblr.com/search/QUERY",
        )
    };
    // Upstream `tumblr:(https?://)?DOMAIN`: any blog domain, usually custom.
    if url.scheme() == "tumblr" {
        let inner = crate::extract::prefixed_target(url).ok_or_else(unsupported)?;
        if inner.scheme() == "tumblr" {
            return Err(unsupported());
        }
        let host = inner
            .host_str()
            .ok_or_else(unsupported)?
            .to_ascii_lowercase();
        if is_www(&host) || host_blog(&host).is_some() {
            return target(&inner);
        }
        if !valid_domain(&host)
            || host.ends_with(".tumblr.com")
            || inner.port().is_some()
            || !inner.username().is_empty()
            || inner.password().is_some()
        {
            return Err(unsupported());
        }
        return blog_target(host, &segments(&inner), unsupported);
    }
    if !matches!(url.scheme(), "http" | "https")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(unsupported());
    }
    let host = url.host_str().ok_or_else(unsupported)?;
    let all = segments(url);
    let www = is_www(host);
    if www && let ["search", query, rest @ ..] = all.as_slice() {
        if rest.len() > 2 {
            return Err(unsupported());
        }
        let query = percent_decode(query)
            .and_then(|q| text_value(&q))
            .ok_or_else(unsupported)?;
        let word_at = |i: usize| match rest.get(i) {
            Some(raw) => word(raw).map(Some).ok_or_else(unsupported),
            None => Ok(None),
        };
        let mode = word_at(0)?;
        let post_type = word_at(1)?;
        let mut params: Vec<(String, String)> = Vec::new();
        for (key, value) in url.query_pairs() {
            if key.len() > 64 || value.len() > 1024 || params.len() >= 32 {
                return Err(unsupported());
            }
            // Upstream text.parse_query keeps the last value of a key.
            params.retain(|(k, _)| *k != key);
            params.push((key.into_owned(), value.into_owned()));
        }
        return Ok(Target::Search {
            query,
            mode,
            post_type,
            params,
        });
    }
    let (blog, rest) = if www {
        let (blog, next) = www_blog(&all).ok_or_else(unsupported)?;
        (blog, &all[next..])
    } else {
        (host_blog(host).ok_or_else(unsupported)?, &all[..])
    };
    blog_target(blog.to_ascii_lowercase(), rest, unsupported)
}

/// Classify the path below a blog root.
fn blog_target(
    blog: String,
    rest: &[&str],
    unsupported: impl Fn() -> crate::Error,
) -> Result<Target> {
    match rest {
        [] | ["posts"] | ["archive"] => Ok(Target::Posts { blog }),
        ["page", page] if page.bytes().all(|b| b.is_ascii_digit()) && !page.is_empty() => {
            Ok(Target::Posts { blog })
        }
        ["post" | "image", id, ..] | [id, ..] if valid_id(id) => Ok(Target::Post {
            blog,
            id: (*id).into(),
        }),
        ["tagged", tag, ..] | ["archive", "tagged", tag, ..] => Ok(Target::Tagged {
            blog,
            tag: tag_of(tag).ok_or_else(unsupported)?,
        }),
        ["day", year, month, day, ..] => Ok(Target::Day {
            blog,
            start: day_start(year, month, day).ok_or_else(unsupported)?,
        }),
        ["likes", ..] => Ok(Target::Likes { blog }),
        ["following"] => Ok(Target::Following { blog }),
        ["followers"] => Ok(Target::Followers { blog }),
        _ => Err(unsupported()),
    }
}

fn string<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn id_of(post: &Value) -> Option<String> {
    match post.get("id_string").or_else(|| post.get("id"))? {
        Value::String(s) => Some(s.trim().to_owned()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
    .filter(|id| valid_id(id))
}

fn https(url: &Url) -> bool {
    url.scheme() == "https"
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
}

/// HTTPS media on `*.media.tumblr.com` (or `a.tumblr.com` for audio).
fn media_url(raw: &str, audio: bool) -> Option<Url> {
    let url = Url::parse(&raw.trim().replace("&amp;", "&")).ok()?;
    let host = url.host_str()?;
    (https(&url) && (host.ends_with(".media.tumblr.com") || audio && host == "a.tumblr.com"))
        .then_some(url)
}

/// Video hosts: `*.media.tumblr.com` and the bare `vt`/`vtt`/`ve`/`va` hosts.
fn video_url(raw: &str) -> Option<Url> {
    let url = Url::parse(&raw.trim().replace("&amp;", "&")).ok()?;
    let host = url.host_str()?;
    (https(&url)
        && (host.ends_with(".media.tumblr.com")
            || matches!(
                host,
                "vt.tumblr.com" | "vtt.tumblr.com" | "ve.tumblr.com" | "va.tumblr.com"
            )))
    .then(|| original_video(url))
}

/// Blog avatars: `*.media.tumblr.com` or Tumblr's default `assets.tumblr.com`.
fn avatar_url(raw: &str) -> Option<Url> {
    let url = Url::parse(raw.trim()).ok()?;
    let host = url.host_str()?;
    (https(&url) && (host.ends_with(".media.tumblr.com") || host == "assets.tumblr.com"))
        .then_some(url)
}

/// Upstream `_original_video`: drop the `_NNN` size suffix of
/// `(vt|vtt|ve)[.media].tumblr.com/tumblr_HASH_NNN.EXT`.
fn original_video(mut url: Url) -> Url {
    let host = url.host_str().unwrap_or_default();
    let label = host
        .strip_suffix(".media.tumblr.com")
        .or_else(|| host.strip_suffix(".tumblr.com"))
        .unwrap_or_default();
    if !matches!(label, "vt" | "vtt" | "ve") {
        return url;
    }
    let path = url.path();
    let Some(name) = path.strip_prefix("/tumblr_") else {
        return url;
    };
    let Some((stem, ext)) = name.split_once('.') else {
        return url;
    };
    let Some((hash, size)) = stem.split_once('_') else {
        return url;
    };
    if hash.is_empty()
        || size.is_empty()
        || !size.bytes().all(|b| b.is_ascii_digit())
        || ext.is_empty()
        || !ext
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    {
        return url;
    }
    url.set_path(&format!("/tumblr_{hash}.{ext}"));
    url
}

pub fn media_id(url: &Url) -> String {
    let digest = Sha256::digest(url.path().as_bytes());
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

fn extension(url: &Url, fallback: &str) -> String {
    url.path_segments()
        .and_then(|mut s| s.next_back())
        .and_then(|name| name.rsplit_once('.'))
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .filter(|ext| {
            (1..=5).contains(&ext.len()) && ext.bytes().all(|b| b.is_ascii_alphanumeric())
        })
        .map(|ext| if ext == "jpeg" { "jpg".into() } else { ext })
        .unwrap_or_else(|| fallback.into())
}

/// Quoted `src` values of `<TAG ...>` elements in an HTML fragment.
fn sources<'h>(html: &'h str, tag: &str) -> Vec<&'h str> {
    let mut out = Vec::new();
    let lower = html.to_ascii_lowercase();
    let open = format!("<{tag}");
    let mut pos = 0;
    while let Some(start) = lower[pos..].find(&open) {
        let start = pos + start;
        let end = lower[start..].find('>').map_or(lower.len(), |e| start + e);
        let element = &lower[start..end];
        if let Some(src) = element.find("src=") {
            let value = start + src + 4;
            if let Some(quote) = html[value..]
                .chars()
                .next()
                .filter(|c| matches!(c, '"' | '\''))
                && let Some(len) = html[value + 1..end].find(quote)
            {
                out.push(&html[value + 1..value + 1 + len]);
            }
        }
        pos = end.max(start + 1);
    }
    out
}

/// `src` values of `<img>` tags in an HTML fragment.
fn inline_images(html: &str) -> Vec<&str> {
    sources(html, "img")
}

fn dimension(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(Value::as_u64).filter(|v| *v > 0)
}

/// Photo URL, size and chosen size object: start at `original_size`,
/// considering larger `alt_sizes`.
fn photo(photo: &Value) -> Option<(Url, Option<u64>, Option<u64>, &Value)> {
    let sized = |size: &'_ Value| {
        let url = media_url(string(size, "url")?, false)?;
        Some((url, dimension(size, "width"), dimension(size, "height")))
    };
    let mut best = photo
        .get("original_size")
        .and_then(|size| Some((sized(size)?, size)));
    for (candidate, size) in photo
        .get("alt_sizes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|size| Some((sized(size)?, size)))
    {
        // Upstream compares either dimension, even for narrower/taller images.
        if best.as_ref().is_none_or(|((_, width, height), _)| {
            candidate.1.unwrap_or(0) > width.unwrap_or(0)
                || candidate.2.unwrap_or(0) > height.unwrap_or(0)
        }) {
            best = Some((candidate, size));
        }
    }
    best.map(|((url, width, height), size)| (url, width, height, size))
}

/// Request original image tokens only for Tumblr's documented resize paths.
fn original_image(url: &Url, width: Option<u64>, height: Option<u64>, inline: bool) -> Option<Url> {
    let path = url.path();
    let size = if inline {
        path.split('/').find(|segment| {
            segment
                .strip_prefix('s')
                .and_then(|size| size.split_once('x'))
                .is_some_and(|(w, h)| {
                    !w.is_empty()
                        && !h.is_empty()
                        && w.bytes().all(|b| b.is_ascii_digit())
                        && h.bytes().all(|b| b.is_ascii_digit())
                })
        })?
    } else if width == Some(2048) || height == Some(3072) {
        "s2048x3072"
    } else {
        return None;
    };
    let from = format!("/{size}/");
    if !path.contains(&from) {
        return None;
    }
    let mut original = url.clone();
    original.set_path(&path.replacen(&from, "/s99999x99999/", 1));
    Some(original)
}

/// Legacy inline images use the largest numbered rendition, `_1280`.
fn inline_image(mut url: Url) -> Url {
    let Some(host) = url
        .host_str()
        .and_then(|host| host.strip_suffix(".media.tumblr.com"))
    else {
        return url;
    };
    if !host.bytes().all(|b| b.is_ascii_digit()) || host.is_empty() {
        return url;
    }
    let path = url.path();
    let Some((stem, ext)) = path.rsplit_once('.') else {
        return url;
    };
    let Some((base, size)) = stem.rsplit_once('_') else {
        return url;
    };
    let name = base.rsplit('/').next().unwrap_or("");
    let hash = name
        .strip_prefix("tumblr_inline_")
        .or_else(|| name.strip_prefix("tumblr_"));
    if hash.is_some_and(|hash| !hash.is_empty() && !hash.contains('_'))
        && !size.is_empty()
        && size.bytes().all(|b| b.is_ascii_digit())
        && !ext.is_empty()
        && ext
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    {
        url.set_path(&format!("{base}_1280.{ext}"));
    }
    url
}

/// Upstream `_prepare_image` hash: the second `_` part, skipping `inline`.
fn image_hash(filename: &str) -> String {
    let parts: Vec<&str> = filename.split('_').collect();
    match parts.get(1) {
        Some(&"inline") => parts.get(2).copied().unwrap_or(filename),
        Some(part) => part,
        None => filename,
    }
    .into()
}

/// Upstream `_prepare` hash: everything after the first `_`.
fn file_hash(filename: &str) -> String {
    filename
        .split_once('_')
        .map(|(_, rest)| rest)
        .unwrap_or_default()
        .into()
}

fn file_name(name: &str, ext: &str) -> String {
    if ext.is_empty() {
        name.into()
    } else {
        format!("{name}.{ext}")
    }
}

/// Which reblogs to include, matching upstream's `reblogs` option.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Reblogs {
    #[default]
    All,
    OriginalOnly,
    SameBlog,
}

/// Inline media scope, matching upstream's boolean and string modes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Inline {
    #[default]
    All,
    Disabled,
    Reblog,
    Original,
}

/// Native Tumblr post and image selection settings.
#[derive(Clone, Debug)]
pub struct Selection {
    pub reblogs: Reblogs,
    pub posts: Option<HashSet<String>>,
    pub inline: Inline,
    pub original: bool,
}

impl Default for Selection {
    fn default() -> Self {
        Self {
            reblogs: Reblogs::All,
            posts: None,
            inline: Inline::All,
            original: true,
        }
    }
}

impl Selection {
    /// Restrict extraction to the upstream legacy post types. An empty list
    /// selects no posts; unknown names are rejected rather than ignored.
    pub fn with_posts(mut self, types: impl IntoIterator<Item = String>) -> Result<Self> {
        let types: HashSet<_> = types.into_iter().collect();
        for kind in &types {
            if !matches!(
                kind.as_str(),
                "text"
                    | "quote"
                    | "link"
                    | "answer"
                    | "video"
                    | "audio"
                    | "photo"
                    | "chat"
                    | "search"
            ) {
                return Err(error(format!("unsupported Tumblr post type: {kind}")));
            }
        }
        self.posts = Some(types);
        Ok(self)
    }

    /// Upstream sends `type` when exactly one post type is selected.
    fn single_type(&self) -> Option<&str> {
        let types = self.posts.as_ref()?;
        (types.len() == 1).then(|| types.iter().next().map(String::as_str))?
    }

    fn type_selected(&self, post: &Value) -> bool {
        match &self.posts {
            Some(types) => string(post, "type").is_some_and(|kind| types.contains(kind)),
            None => true,
        }
    }

    fn includes(&self, post: &Value, blog: &str) -> bool {
        if !self.type_selected(post) {
            return false;
        }
        if post.get("reblogged_from_id").is_none() {
            return true;
        }
        match self.reblogs {
            Reblogs::All => true,
            Reblogs::OriginalOnly => false,
            Reblogs::SameBlog => {
                let fallback_blog = post
                    .get("blog")
                    .and_then(|blog| string(blog, "name"))
                    .unwrap_or(blog);
                let fallback = format!("{fallback_blog}.tumblr.com");
                let own_uuid = post
                    .get("blog")
                    .and_then(|blog| string(blog, "uuid"))
                    .unwrap_or(&fallback);
                string(post, "reblogged_root_uuid") == Some(own_uuid)
            }
        }
    }
}

/// One file of a post before numbering.
struct Spec {
    url: Url,
    kind: &'static str,
    width: Option<u64>,
    height: Option<u64>,
    inline: bool,
    /// Upstream `_prepare_image` (image hash and gif handling).
    image: bool,
    photo: Option<Map<String, Value>>,
}

/// Per-class context for building keyword dictionaries.
struct Context<'c> {
    route: &'static Route,
    /// Blog of the listing; the fallback for posts without `blog_name`.
    blog: &'c str,
    /// Blog info for posts without their own `blog` object.
    blog_info: Option<&'c Value>,
    tag: Option<&'c str>,
    selection: &'c Selection,
    external: bool,
}

#[derive(Default)]
struct Parsed {
    media: Vec<Media>,
    /// Upstream `external`: the queued link, with its keyword dictionary.
    external: Option<(Url, Map<String, Value>)>,
}

/// Parse one legacy-format post into its deduplicated media files.
pub fn parse_post(post: &Value, blog: &str) -> Vec<Media> {
    parse_post_selected(post, blog, &Selection::default())
}

fn parse_post_selected(post: &Value, blog: &str, selection: &Selection) -> Vec<Media> {
    parse(
        post,
        &Context {
            route: &ROUTE_USER,
            blog,
            blog_info: None,
            tag: None,
            selection,
            external: false,
        },
    )
    .media
}

fn parse(post: &Value, ctx: &Context) -> Parsed {
    if !ctx.selection.includes(post, ctx.blog) {
        return Parsed::default();
    }
    let Some(id) = id_of(post) else {
        return Parsed::default();
    };
    let username: Arc<str> = Arc::from(
        string(post, "blog_name")
            .or_else(|| post.get("blog").and_then(|blog| string(blog, "name")))
            .filter(|name| valid_blog(name))
            .unwrap_or(ctx.blog),
    );
    let caption = string(post, "summary")
        .map(str::to_owned)
        .or_else(|| string(post, "caption").map(crate::mastodon::strip_html))
        .filter(|text| !text.is_empty())
        .map(Arc::<str>::from);
    let timestamp = post.get("timestamp").and_then(Value::as_i64);
    let mut files: Vec<Spec> = Vec::new();
    for item in post
        .get("photos")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some((url, width, height, best)) = photo(item) {
            // Upstream merges the best size into the photo object and drops
            // the size lists.
            let mut object = item.as_object().cloned().unwrap_or_default();
            if let Some(best) = best.as_object() {
                object.extend(best.clone());
            }
            object.remove("original_size");
            object.remove("alt_sizes");
            files.push(Spec {
                url,
                kind: "photo",
                width,
                height,
                inline: false,
                image: true,
                photo: Some(object),
            });
        }
    }
    let plain = |url, kind| Spec {
        url,
        kind,
        width: None,
        height: None,
        inline: false,
        image: false,
        photo: None,
    };
    if let Some(url) = string(post, "audio_url").and_then(|raw| media_url(raw, true)) {
        files.push(plain(url, "audio"));
    }
    if let Some(url) = string(post, "video_url").and_then(video_url) {
        files.push(plain(url, "video"));
    }
    let mut inline_html = Vec::new();
    match ctx.selection.inline {
        Inline::Disabled => {}
        Inline::All => {
            for key in [
                "body",
                "caption",
                "answer",
                "text",
                "description",
                "question",
            ] {
                inline_html.extend(string(post, key));
            }
            if let Some(reblog) = post.get("reblog") {
                inline_html.extend(string(reblog, "comment"));
                inline_html.extend(string(reblog, "tree_html"));
            }
        }
        Inline::Reblog | Inline::Original => {
            inline_html.extend(string(post, "answer"));
            inline_html.extend(string(post, "question"));
            if let Some(reblog) = post.get("reblog") {
                inline_html.extend(if ctx.selection.inline == Inline::Reblog {
                    string(reblog, "comment")
                } else {
                    string(reblog, "tree_html").or_else(|| string(reblog, "comment"))
                });
            }
        }
    }
    // Upstream yields inline images before inline videos.
    for html in &inline_html {
        for src in inline_images(html) {
            if let Some(url) = media_url(src, false) {
                files.push(Spec {
                    inline: true,
                    image: true,
                    ..plain(inline_image(url), "photo")
                });
            }
        }
    }
    for html in &inline_html {
        for src in sources(html, "source") {
            if let Some(url) = video_url(src) {
                files.push(plain(url, "video"));
            }
        }
    }
    let mut seen = HashSet::new();
    files.retain(|file| seen.insert(file.url.path().to_owned()));
    let external = ctx
        .external
        .then(|| string(post, "permalink_url").or_else(|| string(post, "url")))
        .flatten()
        .and_then(|raw| Url::parse(raw).ok())
        .filter(|url| matches!(url.scheme(), "http" | "https"));
    let count = files.len() + usize::from(external.is_some());

    let mut base = post.as_object().cloned().unwrap_or_default();
    base.remove("trail");
    base.remove("photos");
    if !base.contains_key("blog")
        && let Some(info) = ctx.blog_info
    {
        base.insert("blog".into(), info.clone());
    }
    base.insert(
        "reblogged".into(),
        Value::Bool(post.get("reblogged_from_id").is_some()),
    );
    if timestamp.is_some() {
        base.insert(
            "date".into(),
            crate::filter::source_date_text(timestamp).into(),
        );
    }
    if let Some(tag) = ctx.tag {
        base.insert("search_tags".into(), tag.into());
    }
    base.insert("category".into(), "tumblr".into());
    base.insert("subcategory".into(), ctx.route.subcategory.into());
    base.insert("count".into(), count.into());

    let mut media = Vec::with_capacity(files.len());
    for (index, file) in files.into_iter().enumerate() {
        let num = index + 1;
        let mut handoff = BTreeMap::new();
        if ctx.selection.original
            && file.kind == "photo"
            && let Some(original) = original_image(&file.url, file.width, file.height, file.inline)
        {
            handoff.insert("_tumblr_original".into(), Value::String(original.into()));
        }
        let mut fields = base.clone();
        if let Some(photo) = file.photo {
            fields.insert("photo".into(), Value::Object(photo));
        }
        let (name, ext) = crate::sites::nameext(&file.url);
        let hash = if file.image {
            image_hash(&name)
        } else {
            file_hash(&name)
        };
        fields.insert("hash".into(), hash.into());
        fields.insert("num".into(), num.into());
        let filename = file_name(&name, &ext);
        fields.insert("filename".into(), name.into());
        fields.insert("extension".into(), ext.into());
        media.push(Media {
            filename,
            job: MediaJob::DirectHttp,
            category: "tumblr".into(),
            metadata: Some(PostMetadata {
                file: Some(FileMetadata {
                    width: file.width,
                    height: file.height,
                    media_type: Some(Arc::from(file.kind)),
                    ..FileMetadata::default()
                }),
                post_id: id.clone(),
                media_id: media_id(&file.url),
                username: Some(username.clone()),
                caption: caption.clone(),
                timestamp,
                index: num,
                site_keywords: Some(Arc::new(SiteKeywords {
                    route: ctx.route,
                    fields,
                })),
                ..PostMetadata::default()
            }),
            url: file.url,
            headers: BTreeMap::new(),
            handoff,
        });
    }
    let external = external.map(|url| {
        let mut fields = base;
        fields.insert("num".into(), count.into());
        (url, fields)
    });
    Parsed { media, external }
}

/// One API page: per-post media, the number of posts and the reported total.
pub struct Page {
    pub posts: Vec<Vec<Media>>,
    pub count: usize,
    pub total: Option<u64>,
}

pub fn parse_page(bytes: &[u8], blog: &str) -> Result<Page> {
    let body: Value =
        serde_json::from_slice(bytes).map_err(|_| error("Tumblr returned invalid JSON"))?;
    let response = body.get("response").unwrap_or(&body);
    let entries = response
        .get("posts")
        .and_then(Value::as_array)
        .ok_or_else(|| error("Tumblr response contains no post list"))?;
    Ok(Page {
        posts: entries
            .iter()
            .map(|post| parse_post(post, blog))
            .filter(|media| !media.is_empty())
            .collect(),
        count: entries.len(),
        total: response.get("total_posts").and_then(Value::as_u64),
    })
}

/// Parse a `?id=` response, requiring the requested post.
pub fn parse_single(bytes: &[u8], blog: &str, id: &str) -> Result<Vec<Media>> {
    let page = parse_page(bytes, blog)?;
    let media = page
        .posts
        .into_iter()
        .find(|media| media[0].metadata.as_ref().is_some_and(|m| m.post_id == id));
    match media {
        Some(media) => Ok(media),
        None if page.count == 0 => Err(error("Tumblr post not found")),
        None => Err(error("Tumblr post contains no supported media")),
    }
}

/// Upstream `pagination` strategies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pagination {
    /// Follow `_links.next.href`.
    Api,
    /// Page by the last post's timestamp.
    Before,
    /// Page by `offset` until `total_posts`.
    Offset,
}

/// Upstream extractor settings beyond post selection.
#[derive(Clone, Debug)]
struct Options {
    external: bool,
    avatar: bool,
    offset: Option<u64>,
    pagination: Option<Pagination>,
    fallback_delay: Duration,
    fallback_retries: u64,
    ratelimit_wait: bool,
    /// OAuth 1.0a consumer secret (`api-secret`); upstream's default if unset.
    api_secret: Option<String>,
    /// OAuth 1.0a user token (`access-token`) and its secret.
    access_token: Option<String>,
    access_token_secret: Option<String>,
    /// `access-token` is unset or `"cache"`: use the pair `oauth:tumblr`
    /// cached for the API key.
    access_token_cache: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            external: false,
            avatar: false,
            offset: None,
            pagination: None,
            fallback_delay: Duration::from_secs(120),
            fallback_retries: 2,
            ratelimit_wait: false,
            api_secret: None,
            access_token: None,
            access_token_secret: None,
            access_token_cache: true,
        }
    }
}

impl Options {
    fn parse(options: &Map<String, Value>) -> Result<Self> {
        let mut out = Self::default();
        for (key, value) in options {
            let invalid = || error(format!("invalid Tumblr setting: {key}"));
            if value.is_null() {
                continue;
            }
            match key.as_str() {
                "external" => out.external = value.as_bool().ok_or_else(invalid)?,
                "avatar" => out.avatar = value.as_bool().ok_or_else(invalid)?,
                "offset" => {
                    out.offset = Some(match value {
                        Value::Number(n) => n.as_u64().ok_or_else(invalid)?,
                        Value::String(s) if !s.is_empty() && s.len() <= 19 => {
                            s.parse().map_err(|_| invalid())?
                        }
                        _ => return Err(invalid()),
                    })
                }
                "pagination" => {
                    out.pagination = Some(match value.as_str() {
                        Some("api") => Pagination::Api,
                        Some("before") => Pagination::Before,
                        Some("offset") => Pagination::Offset,
                        _ => return Err(invalid()),
                    })
                }
                "fallback-delay" => {
                    let seconds = value
                        .as_f64()
                        .filter(|s| s.is_finite() && *s >= 0.0)
                        .ok_or_else(invalid)?;
                    out.fallback_delay =
                        Duration::try_from_secs_f64(seconds).map_err(|_| invalid())?;
                }
                "fallback-retries" => {
                    out.fallback_retries = value
                        .as_u64()
                        .filter(|n| *n <= MAX_FALLBACK_RETRIES)
                        .ok_or_else(invalid)?;
                }
                "ratelimit" => {
                    out.ratelimit_wait = match value {
                        Value::String(mode) => mode == "wait",
                        Value::Bool(false) => false,
                        _ => return Err(invalid()),
                    }
                }
                "api-secret" | "access-token" | "access-token-secret" => {
                    if key == "access-token" {
                        out.access_token_cache =
                            matches!(value, Value::Null) || value.as_str() == Some("cache");
                    }
                    let value = match value {
                        Value::Bool(false) | Value::Null => None,
                        // Upstream's "cache" reads the pair stored by
                        // `oauth:tumblr` (see `Tumblr::with_token_cache`).
                        Value::String(s) if s.is_empty() || s == "cache" => None,
                        Value::String(s)
                            if s.len() <= 1024 && s.bytes().all(|b| b.is_ascii_graphic()) =>
                        {
                            Some(s.clone())
                        }
                        _ => return Err(invalid()),
                    };
                    match key.as_str() {
                        // An explicitly empty secret disables OAuth, as upstream.
                        "api-secret" => out.api_secret = Some(value.unwrap_or_default()),
                        "access-token" => out.access_token = value,
                        _ => out.access_token_secret = value,
                    }
                }
                _ => return Err(error(format!("unsupported Tumblr setting: {key}"))),
            }
        }
        Ok(out)
    }
}

pub struct Tumblr {
    client: Client,
    network: NetworkOptions,
    api_root: Url,
    api_key: String,
    timeout: Duration,
    request_timeout: Duration,
    retries: u8,
    rate_limits: RateLimits,
    selection: Selection,
    options: Options,
    /// Tumblr's own web-client token, used once the shared default key has
    /// hit its daily limit. Its responses are NPF and are converted.
    web_token: std::sync::Mutex<Option<String>>,
    /// The page that embeds the token; unit tests leave it unset so they
    /// never reach Tumblr.
    web_root: Option<Url>,
    /// Set once a blog refused OAuth access; later calls use `api_key`.
    oauth_disabled: std::sync::atomic::AtomicBool,
}

/// How a listing advances between pages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Strategy {
    Pages(Pagination),
    /// Likes: `before` = the last `liked_timestamp`.
    Liked,
    /// A single `?id=` request.
    Single,
}

/// Lazily paginated post listing of one extractor class.
struct Listing<'a> {
    route: &'static Route,
    /// The listed blog; `None` for searches.
    blog: Option<String>,
    tag: Option<String>,
    /// Requested post of the `post` class.
    single: Option<String>,
    path: String,
    params: Vec<(String, String)>,
    /// Response key holding the posts.
    key: &'static str,
    strategy: Strategy,
    offset: Option<u64>,
    limit: Option<usize>,
    before: Option<i64>,
    date_min: Option<i64>,
    next: Option<Url>,
    pages: usize,
    done: bool,
    selection: Selection,
    /// Blog info for posts without their own `blog` object.
    blog_info: Option<Value>,
    /// Whether to cache the posts response's `blog`, else request `info`.
    page_blog: bool,
    avatar: bool,
    /// Post IDs of the previous page, repeated by `before` pagination.
    previous: HashSet<String>,
    queue: crate::event::Queue<'a>,
}

impl Listing<'_> {
    /// Cache blog info for posts without their own `blog`; upstream then
    /// identifies the listed blog by its host name.
    fn adopt_blog_info(&mut self, mut info: Value) {
        if let (Some(blog), Some(object)) = (&self.blog, info.as_object_mut()) {
            object.insert("uuid".into(), blog_host(blog).into());
        }
        self.blog_info = Some(info);
    }
}

/// One queued item of a page: a post event or a child job.
enum Item<'a> {
    Event(ExtractionEvent),
    Child(Events<'a>),
}

/// Response of one HTTP request.
enum Reply {
    Body(Vec<u8>),
    /// A redirect target, when redirects are not followed.
    Location(Url),
}

impl Tumblr {
    pub fn new(timeout: Duration) -> Result<Self> {
        Ok(Self {
            client: NetworkOptions::default().client(USER_AGENT, timeout)?,
            network: NetworkOptions::default(),
            api_root: Url::parse("https://api.tumblr.com/").expect("static URL"),
            api_key: DEFAULT_API_KEY.into(),
            timeout,
            request_timeout: timeout,
            retries: 0,
            rate_limits: RateLimits::default(),
            selection: Selection::default(),
            options: Options::default(),
            web_token: std::sync::Mutex::new(None),
            web_root: if cfg!(test) {
                None
            } else {
                Url::parse("https://www.tumblr.com/").ok()
            },
            oauth_disabled: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Upstream signs API requests with OAuth 1.0a once `api-key`,
    /// `api-secret` (both defaulting to upstream's application),
    /// `access-token` and `access-token-secret` are all set.
    fn oauth(&self) -> Option<crate::oauth::OAuth1<'_>> {
        if self
            .oauth_disabled
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return None;
        }
        let consumer_secret = match &self.options.api_secret {
            Some(secret) => secret.as_str(),
            None => DEFAULT_API_SECRET,
        };
        Some(crate::oauth::OAuth1 {
            consumer_key: Some(self.api_key.as_str()).filter(|key| !key.is_empty())?,
            consumer_secret: Some(consumer_secret).filter(|secret| !secret.is_empty())?,
            token: Some(self.options.access_token.as_deref()?),
            token_secret: self.options.access_token_secret.as_deref()?,
        })
    }

    #[cfg(test)]
    fn with_web_root(mut self, root: Url) -> Self {
        self.web_root = Some(root);
        self
    }

    fn web_token(&self) -> Option<String> {
        self.web_token.lock().unwrap().clone()
    }

    /// The public `API_TOKEN` the www.tumblr.com web client embeds.
    async fn activate_web_token(&self) -> Result<String> {
        let failed = || error("Tumblr web client token unavailable");
        let root = self.web_root.clone().ok_or_else(failed)?;
        let reply = tokio::time::timeout(self.timeout, self.fetch(&root, Some("text/html"), true))
            .await
            .map_err(|_| error("native Tumblr extraction timed out"))?
            .map_err(|_| failed())?;
        let Reply::Body(body) = reply else {
            return Err(failed());
        };
        let page = String::from_utf8_lossy(&body);
        let token = crate::sites::extr(&page, r#""API_TOKEN":""#, "\"")
            .filter(|token| {
                !token.is_empty()
                    && token.len() <= 256
                    && token.bytes().all(|b| b.is_ascii_alphanumeric())
            })
            .ok_or_else(failed)?
            .to_owned();
        *self.web_token.lock().unwrap() = Some(token.clone());
        Ok(token)
    }

    /// Apply proxy and user-agent settings to API requests and emitted media.
    pub fn with_network(mut self, network: &NetworkOptions) -> Result<Self> {
        self.client = network.client(USER_AGENT, self.timeout)?;
        self.network = network.clone();
        Ok(self)
    }

    /// Replace the default public API key.
    pub fn with_api_key(mut self, key: Option<&str>) -> Self {
        if let Some(key) = key {
            self.api_key = key.into();
        }
        self
    }

    /// Upstream `OAuth1API`: without a configured `access-token` (or with
    /// `"cache"`), use the token pair `oauth:tumblr` cached for the API key,
    /// replacing any configured `access-token-secret`. Call after the API key
    /// and options; unreadable caches count as empty.
    pub async fn with_token_cache(mut self, cache: Option<&crate::cache::Cache>) -> Self {
        let Some(cache) = cache.filter(|_| self.options.access_token_cache) else {
            return self;
        };
        let (token, secret) = crate::oauth::cached_tokens(cache, "tumblr", &self.api_key).await;
        self.options.access_token = token;
        self.options.access_token_secret = secret;
        self
    }

    /// Select post types, reblogs, inline scope and original image resolution.
    pub fn with_selection(mut self, selection: Selection) -> Self {
        self.selection = selection;
        self
    }

    /// Apply upstream extractor settings (`extractor.tumblr.*`): `external`,
    /// `avatar`, `offset`, `pagination`, `fallback-delay`, `fallback-retries`
    /// and `ratelimit`. Invalid values are rejected without echoing them.
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

    /// Override the API origin; intended for local tests.
    pub fn with_api_root(mut self, root: Url) -> Self {
        self.api_root = root;
        self
    }

    fn endpoint(&self, path: &str) -> Url {
        let mut url = self.api_root.join(path).expect("validated Tumblr path");
        url.query_pairs_mut().append_pair("api_key", &self.api_key);
        url
    }

    /// One API call, returning the JSON `response` object.
    async fn api(&self, url: &Url) -> Result<Value> {
        let call = || async {
            tokio::time::timeout(self.timeout, self.fetch(url, None, true))
                .await
                .map_err(|_| error("native Tumblr extraction timed out"))?
        };
        let reply = match call().await {
            // The shared default key is often exhausted; fall back to the
            // web client's token rather than fail (or wait) for a day.
            Err(e)
                if self.api_key == DEFAULT_API_KEY
                    && self.web_token().is_none()
                    && self.oauth().is_none()
                    && e.to_string().starts_with("Tumblr daily API rate limit") =>
            {
                if self.activate_web_token().await.is_err() {
                    return Err(e);
                }
                call().await?
            }
            result => result?,
        };
        let Reply::Body(body) = reply else {
            return Err(error("Tumblr returned an unexpected redirect"));
        };
        let mut body: Value =
            serde_json::from_slice(&body).map_err(|_| error("Tumblr returned invalid JSON"))?;
        let mut response = match body.get_mut("response") {
            Some(response) => response.take(),
            None => body,
        };
        if self.web_token().is_some() {
            for key in ["posts", "liked_posts"] {
                if let Some(posts) = response.get_mut(key).and_then(Value::as_array_mut) {
                    posts.iter_mut().for_each(npf_to_legacy);
                }
            }
        }
        Ok(response)
    }

    /// Blog info (`/v2/blog/BLOG/info`), as upstream `TumblrAPI.info`.
    async fn info(&self, blog: &str) -> Result<Value> {
        let url = self.endpoint(&format!("v2/blog/{}/info", blog_host(blog)));
        let mut response = self.api(&url).await?;
        match response.get_mut("blog") {
            Some(blog) if blog.is_object() => Ok(blog.take()),
            _ => Err(error("Tumblr returned no blog info")),
        }
    }

    /// The 512px avatar media URL. The API answers with a redirect (or an
    /// `avatar_url`), which is resolved here so the API key never becomes
    /// part of a downloadable media URL.
    async fn avatar(&self, blog: &str) -> Result<Url> {
        let url = self.endpoint(&format!("v2/blog/{}/avatar/512", blog_host(blog)));
        let reply = tokio::time::timeout(self.timeout, self.fetch(&url, None, false))
            .await
            .map_err(|_| error("native Tumblr extraction timed out"))??;
        let avatar = match reply {
            Reply::Location(location) => avatar_url(location.as_str()),
            Reply::Body(body) => serde_json::from_slice::<Value>(&body)
                .ok()
                .and_then(|body| {
                    let response = body.get("response").unwrap_or(&body);
                    avatar_url(string(response, "avatar_url")?)
                }),
        };
        // Never emit a media URL that carries the API key.
        avatar
            .filter(|url| self.api_key.is_empty() || !url.as_str().contains(self.api_key.as_str()))
            .ok_or_else(|| error("Tumblr returned no usable avatar URL"))
    }

    async fn finish_event(&self, mut event: ExtractionEvent) -> ExtractionEvent {
        if let ExtractionEvent::Media(item) = &mut event {
            if let Some(Value::String(original)) = item.handoff.remove("_tumblr_original")
                && let Some(original) = media_url(&original, false)
            {
                self.finish_original(item, original).await;
            }
            if item
                .metadata
                .as_ref()
                .and_then(|meta| meta.file.as_ref())
                .and_then(|file| file.media_type.as_deref())
                == Some("photo")
                && extension(&item.url, "") == "gif"
            {
                let mut gifv = item.url.clone();
                gifv.set_path(&format!("{}v", gifv.path()));
                let fallbacks = item
                    .handoff
                    .entry("_fallback".into())
                    .or_insert_with(|| serde_json::json!([]));
                if let Some(fallbacks) = fallbacks.as_array_mut() {
                    fallbacks.insert(0, Value::String(gifv.into()));
                }
                item.headers.insert(
                    "Accept".into(),
                    "image/avif,image/webp,image/apng,image/svg+xml,image/*,*/*;q=0.8".into(),
                );
            }
            self.network.apply_media(item);
        }
        event
    }

    async fn finish_original(&self, item: &mut Media, original: Url) {
        let fallback = item.url.to_string();
        item.url = self.resolve_original(&original).await.unwrap_or_else(|| {
            item.handoff
                .insert("_fallback".into(), serde_json::json!([fallback]));
            original
        });
        // The archive identity stays tied to the API rendition, independent of
        // the rotating CDN token and whether original resolution succeeded.
        // Keywords follow the URL, as upstream prepares the resolved URL.
        let (name, ext) = crate::sites::nameext(&item.url);
        item.filename = file_name(&name, &ext);
        if let Some(keywords) = item
            .metadata
            .as_mut()
            .and_then(|meta| meta.site_keywords.as_mut())
        {
            let fields = &mut Arc::make_mut(keywords).fields;
            if let Some(Value::Object(photo)) = fields.get_mut("photo") {
                photo.insert("url".into(), item.url.as_str().into());
            }
            fields.insert("hash".into(), image_hash(&name).into());
            fields.insert("filename".into(), name.into());
            fields.insert("extension".into(), ext.into());
        }
    }

    /// Tumblr serves an HTML image page at the oversize URL containing a fresh
    /// CDN token. API credentials are confined to API calls, never these
    /// requests. Failed lookups are retried `fallback-retries` times after
    /// `fallback-delay`, within one call deadline.
    async fn resolve_original(&self, original: &Url) -> Option<Url> {
        let start = Instant::now();
        for attempt in 0..=self.options.fallback_retries {
            if attempt > 0 {
                let delay = self.options.fallback_delay;
                if delay >= self.timeout.saturating_sub(start.elapsed()) {
                    return None;
                }
                tokio::time::sleep(delay).await;
            }
            let remaining = self.timeout.saturating_sub(start.elapsed());
            if let Ok(Some(url)) = tokio::time::timeout(remaining, self.token(original)).await {
                return Some(url);
            }
        }
        None
    }

    async fn token(&self, original: &Url) -> Option<Url> {
        let Reply::Body(body) = self
            .fetch(original, Some("text/html,*/*;q=0.8"), true)
            .await
            .ok()?
        else {
            return None;
        };
        let html = std::str::from_utf8(&body).ok()?;
        // Upstream treats an unchanged URL as a failed lookup.
        inline_images(html)
            .into_iter()
            .find_map(|raw| media_url(raw, false))
            .filter(|url| url != original)
    }

    pub fn stream(&self, url: &Url) -> BoxStream<'_, Result<Media>> {
        crate::event::media_only(self.events(url))
    }

    /// Emit one post boundary per post; listings paginate lazily. Links to
    /// other sites (`external`) are skipped.
    pub fn events(&self, url: &Url) -> BoxStream<'_, Result<ExtractionEvent>> {
        self.events_with_queue(url, Arc::new(|_| None))
    }

    /// Like `events`, queueing links to other sites through `queue`.
    pub fn events_with_queue<'a>(
        &'a self,
        url: &Url,
        queue: crate::event::Queue<'a>,
    ) -> BoxStream<'a, Result<ExtractionEvent>> {
        match target(url) {
            Ok(target) => self.target_events(target, queue),
            Err(e) => crate::sites::fail(e),
        }
    }

    fn target_events<'a>(&'a self, target: Target, queue: crate::event::Queue<'a>) -> Events<'a> {
        let route = target.route();
        let mut listing = Listing {
            route,
            blog: None,
            tag: None,
            single: None,
            path: String::new(),
            params: Vec::new(),
            key: "posts",
            strategy: Strategy::Pages(Pagination::Offset),
            offset: None,
            limit: Some(PAGE_SIZE),
            before: None,
            date_min: None,
            next: None,
            pages: 0,
            done: false,
            selection: self.selection.clone(),
            blog_info: None,
            page_blog: true,
            avatar: self.options.avatar,
            previous: HashSet::new(),
            queue,
        };
        let posts_params = |listing: &mut Listing, blog: String| {
            listing.path = format!("v2/blog/{}/posts", blog_host(&blog));
            listing.params = vec![
                ("npf".into(), "false".into()),
                ("reblog_info".into(), "true".into()),
            ];
            listing.blog = Some(blog);
        };
        match target {
            Target::Following { blog } | Target::Followers { blog } => {
                return self.blogs(route, blog, listing.queue);
            }
            Target::Post { blog, id } => {
                posts_params(&mut listing, blog);
                listing.params.push(("id".into(), id.clone()));
                listing.single = Some(id);
                listing.strategy = Strategy::Single;
                listing.limit = None;
                // Upstream single-post extraction ignores the posts filter.
                listing.selection.posts = None;
                return self.listing(listing);
            }
            Target::Posts { blog } => posts_params(&mut listing, blog),
            Target::Tagged { blog, tag } => {
                posts_params(&mut listing, blog);
                listing.params.push(("tag".into(), tag.clone()));
                listing.tag = Some(tag);
            }
            Target::Day { blog, start } => {
                posts_params(&mut listing, blog);
                listing.date_min = Some(start);
                listing.before = Some(start + 86400);
            }
            Target::Likes { blog } => {
                listing.path = format!("v2/blog/{}/likes", blog_host(&blog));
                listing.key = "liked_posts";
                listing.strategy = Strategy::Liked;
                listing.page_blog = false;
                listing.blog = Some(blog);
                return self.listing(listing);
            }
            Target::Search {
                query,
                mode,
                post_type,
                params,
            } => {
                listing.path = "v2/timeline/search".into();
                listing.key = "elements";
                listing.page_blog = false;
                listing.avatar = false;
                let days = params
                    .iter()
                    .find(|(key, _)| key == "t")
                    .map(|(_, days)| days.clone());
                listing.params = params
                    .into_iter()
                    .filter(|(key, _)| {
                        !matches!(
                            key.as_str(),
                            "api_key"
                                | "t"
                                | "limit"
                                | "days"
                                | "query"
                                | "mode"
                                | "reblog_info"
                                | "post_type_filter"
                                | "offset"
                                | "before"
                        )
                    })
                    .collect();
                let mut fixed = Vec::new();
                fixed.push(("limit".into(), PAGE_SIZE.to_string()));
                if let Some(days) = days {
                    fixed.push(("days".into(), days));
                }
                listing.limit = None;
                fixed.push(("query".into(), query));
                if let Some(mode) = mode {
                    fixed.push(("mode".into(), mode));
                }
                fixed.push((
                    "reblog_info".into(),
                    (self.selection.reblogs != Reblogs::OriginalOnly).to_string(),
                ));
                if let Some(post_type) = post_type {
                    fixed.push(("post_type_filter".into(), post_type));
                }
                listing.params.extend(fixed);
                // Upstream: searches have no offset; only "before" applies.
                listing.strategy = Strategy::Pages(match self.options.pagination {
                    Some(Pagination::Before) => Pagination::Before,
                    _ => Pagination::Api,
                });
                return self.listing(listing);
            }
        }
        // Blog post listings (upstream `TumblrAPI.posts`).
        if let Some(kind) = listing.selection.single_type() {
            listing.params.push(("type".into(), kind.into()));
        }
        let pagination = self
            .options
            .pagination
            .unwrap_or(if listing.before.is_some() {
                Pagination::Before
            } else {
                Pagination::Offset
            });
        listing.strategy = Strategy::Pages(pagination);
        listing.offset = match pagination {
            Pagination::Offset => Some(self.options.offset.unwrap_or(0)),
            _ => self.options.offset,
        };
        self.listing(listing)
    }

    fn listing<'a>(&'a self, listing: Listing<'a>) -> Events<'a> {
        crate::sites::paginate(listing, move |listing| self.next_page(listing))
    }

    fn page_url(&self, listing: &mut Listing) -> Url {
        if let Some(next) = listing.next.take() {
            return next;
        }
        let mut url = self.endpoint(&listing.path);
        {
            let mut query = url.query_pairs_mut();
            query.extend_pairs(&listing.params);
            if let Some(offset) = listing.offset {
                query.append_pair("offset", &offset.to_string());
            }
            if let Some(limit) = listing.limit {
                query.append_pair("limit", &limit.to_string());
            }
            if let Some(before) = listing.before {
                query.append_pair("before", &before.to_string());
            }
        }
        url
    }

    /// `_links.next.href` on the API origin, with the API key re-added.
    fn next_link(&self, href: &str) -> Option<Url> {
        if !href.starts_with("/v2/") || href.len() > 4096 {
            return None;
        }
        let link = self.api_root.join(href).ok()?;
        if link.origin() != self.api_root.origin() {
            return None;
        }
        let mut url = link.clone();
        url.query_pairs_mut()
            .clear()
            .extend_pairs(link.query_pairs().filter(|(key, _)| key != "api_key"))
            .append_pair("api_key", &self.api_key);
        Some(url)
    }

    async fn next_page<'a>(
        &'a self,
        mut listing: Listing<'a>,
    ) -> Result<Option<(Events<'a>, Listing<'a>)>> {
        if listing.done {
            return Ok(None);
        }
        let url = self.page_url(&mut listing);
        let mut response = self.api(&url).await?;
        listing.pages += 1;
        let mut data = match response.get_mut("timeline") {
            Some(timeline) => timeline.take(),
            None => response,
        };
        if listing.page_blog
            && listing.blog_info.is_none()
            && let Some(blog) = data.get_mut("blog").filter(|blog| blog.is_object())
        {
            listing.adopt_blog_info(blog.take());
        }
        let entries = match data.get_mut(listing.key) {
            Some(Value::Array(entries)) => std::mem::take(entries),
            _ => return Err(error("Tumblr response contains no post list")),
        };
        let last = entries.last();
        match listing.strategy {
            Strategy::Single => listing.done = true,
            Strategy::Pages(Pagination::Api) => {
                listing.next = data
                    .pointer("/_links/next/href")
                    .and_then(Value::as_str)
                    .and_then(|href| self.next_link(href));
                listing.done = listing.next.is_none();
            }
            Strategy::Pages(Pagination::Before) => {
                match last
                    .and_then(|post| post.get("timestamp"))
                    .and_then(Value::as_i64)
                {
                    Some(timestamp)
                        if listing.before.is_none_or(|before| timestamp + 1 < before) =>
                    {
                        listing.before = Some(timestamp + 1);
                        listing.offset = None;
                    }
                    _ => listing.done = true,
                }
            }
            Strategy::Pages(Pagination::Offset) => {
                // The API may cap `limit` below 50, so advance by the count.
                let offset = listing.offset.unwrap_or(0) + entries.len() as u64;
                listing.offset = Some(offset);
                listing.before = None;
                listing.done = data
                    .get("total_posts")
                    .and_then(Value::as_u64)
                    .is_some_and(|total| offset >= total);
            }
            Strategy::Liked => {
                match last
                    .and_then(|post| post.get("liked_timestamp"))
                    .and_then(Value::as_i64)
                {
                    Some(timestamp) => listing.before = Some(timestamp),
                    None => listing.done = true,
                }
            }
        }
        if entries.is_empty() || listing.pages >= MAX_PAGES {
            listing.done = true;
        }
        let items = self.process(&mut listing, entries).await?;
        let events = stream::iter(items)
            .flat_map(move |item| match item {
                Item::Event(event) => {
                    stream::once(async move { Ok(self.finish_event(event).await) }).boxed()
                }
                Item::Child(events) => events,
            })
            .boxed();
        Ok(Some((events, listing)))
    }

    /// Turn one page of posts into post groups, avatars and external links.
    async fn process<'a>(
        &'a self,
        listing: &mut Listing<'a>,
        entries: Vec<Value>,
    ) -> Result<Vec<Item<'a>>> {
        let mut items = Vec::new();
        let previous = std::mem::take(&mut listing.previous);
        let mut found = false;
        for post in &entries {
            let id = id_of(post);
            if let Some(single) = &listing.single
                && id.as_ref() != Some(single)
            {
                continue;
            }
            found = true;
            if let (Some(min), Some(timestamp)) = (
                listing.date_min,
                post.get("timestamp").and_then(Value::as_i64),
            ) && timestamp < min
            {
                listing.done = true;
                break;
            }
            if let Some(id) = &id {
                if previous.contains(id) {
                    continue;
                }
                listing.previous.insert(id.clone());
            }
            if !listing.selection.type_selected(post) {
                continue;
            }
            let blog = listing.blog.clone().unwrap_or_default();
            if post.get("blog").is_none() && listing.blog_info.is_none() && !blog.is_empty() {
                // Upstream requests (and caches) blog info for such posts.
                // Post listings already returned it with the first page;
                // without it, the blog name is the known part of the info.
                let info = if listing.page_blog {
                    serde_json::json!({"name": blog})
                } else {
                    self.info(&blog).await?
                };
                listing.adopt_blog_info(info);
            }
            if listing.avatar && !blog.is_empty() {
                listing.avatar = false;
                let url = self.avatar(&blog).await?;
                let info = post
                    .get("blog")
                    .or(listing.blog_info.as_ref())
                    .cloned()
                    .unwrap_or(Value::Null);
                items.extend(
                    group_with_keywords(
                        vec![avatar_media(url, post, &blog, info, listing.route)],
                        "tumblr",
                        KeywordPolicy::default(),
                    )
                    .into_iter()
                    .map(Item::Event),
                );
            }
            let parsed = parse(
                post,
                &Context {
                    route: listing.route,
                    blog: &blog,
                    blog_info: listing.blog_info.as_ref(),
                    tag: listing.tag.as_deref(),
                    selection: &listing.selection,
                    external: self.options.external,
                },
            );
            if listing.single.is_some()
                && parsed.media.is_empty()
                && parsed.external.is_none()
                && listing.selection.includes(post, &blog)
                && listing.selection.inline == Inline::All
            {
                return Err(error("Tumblr post contains no supported media"));
            }
            if !parsed.media.is_empty() {
                items.extend(
                    group_with_keywords(
                        parsed.media,
                        "tumblr",
                        KeywordPolicy::copied_after_post(FILE_FIELDS),
                    )
                    .into_iter()
                    .map(Item::Event),
                );
            }
            if let Some((url, fields)) = parsed.external
                && let Some(events) = (listing.queue)(url)
            {
                items.push(Item::Child(child_job(
                    ChildMetadata::from_raw("tumblr", listing.route.subcategory, &fields),
                    move || events,
                )));
            }
        }
        if listing.single.is_some() && !found {
            return Err(error("Tumblr post not found"));
        }
        Ok(items)
    }

    /// Followed blogs or followers, each queued as a `user` child job
    /// (upstream `items_blogs` with `_pagination_blogs`).
    fn blogs<'a>(
        &'a self,
        route: &'static Route,
        blog: String,
        queue: crate::event::Queue<'a>,
    ) -> Events<'a> {
        let which = route.subcategory;
        crate::sites::paginate(Some(0u64), move |offset| {
            let blog = blog.clone();
            let queue = queue.clone();
            async move {
                let Some(offset) = offset else {
                    return Ok(None);
                };
                let mut url = self.endpoint(&format!("v2/blog/{}/{which}", blog_host(&blog)));
                url.query_pairs_mut()
                    .append_pair("limit", &BLOG_PAGE_SIZE.to_string())
                    .append_pair("offset", &offset.to_string());
                let response = self.api(&url).await?;
                let blogs = response
                    .get("blogs")
                    .and_then(Value::as_array)
                    .ok_or_else(|| error("Tumblr response contains no blog list"))?;
                let next = offset + BLOG_PAGE_SIZE;
                let more = !blogs.is_empty()
                    && next / BLOG_PAGE_SIZE < MAX_PAGES as u64
                    && response
                        .get("total_blogs")
                        .and_then(Value::as_u64)
                        .is_some_and(|total| next < total);
                let children: Vec<Events<'a>> = blogs
                    .iter()
                    .filter_map(|entry| {
                        let name = string(entry, "name").filter(|name| valid_blog(name))?;
                        let name = name.to_ascii_lowercase();
                        let fields = entry.as_object().cloned().unwrap_or_default();
                        let queue = queue.clone();
                        Some(child_job(
                            ChildMetadata::from_raw("tumblr", "user", &fields),
                            move || self.target_events(Target::Posts { blog: name }, queue),
                        ))
                    })
                    .collect();
                Ok(Some((
                    stream::iter(children).flatten().boxed(),
                    more.then_some(next),
                )))
            }
        })
    }

    async fn fetch(&self, endpoint: &Url, accept: Option<&str>, follow: bool) -> Result<Reply> {
        let mut attempt = 0;
        let mut waits = 0;
        loop {
            match self.attempt(endpoint, attempt, accept, follow).await {
                Ok(reply) => return Ok(reply),
                Err(failure) if failure.wait && waits < MAX_RATE_WAITS => waits += 1,
                // Upstream retries refused OAuth requests once with `api_key`.
                Err(failure) if failure.key_fallback => {
                    self.oauth_disabled
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                }
                Err(failure) if failure.retry && attempt < self.retries => {
                    tokio::time::sleep(backoff(attempt)).await;
                    attempt += 1;
                }
                Err(failure) => return Err(failure.error),
            }
        }
    }

    /// Upstream's handling of Tumblr's daily and hourly API limits.
    fn rate_limit(&self, endpoint: &Url, headers: &HeaderMap) -> Option<Failure> {
        let header = |name: &str| headers.get(name)?.to_str().ok().map(str::trim);
        let reset = |name: &str| {
            header(name)?
                .parse::<f64>()
                .ok()
                .filter(|s| s.is_finite() && *s >= 0.0)
                .map(|s| Duration::from_secs_f64(s.min(1e9).ceil()))
        };
        if header("x-ratelimit-perday-remaining") == Some("0") {
            let reset = reset("x-ratelimit-perday-reset");
            if !self.options.ratelimit_wait {
                return Some(
                    error(match reset {
                        Some(reset) => format!(
                            "Tumblr daily API rate limit exceeded; resets in {} seconds (set ratelimit to \"wait\" or use your own api-key)",
                            reset.as_secs()
                        ),
                        None => "Tumblr daily API rate limit exceeded".into(),
                    })
                    .into(),
                );
            }
            self.rate_limits
                .defer(endpoint, reset.unwrap_or(Duration::from_secs(60)));
            return Some(Failure::wait("Tumblr daily API rate limit exceeded"));
        }
        let reset = reset("x-ratelimit-perhour-reset")?;
        self.rate_limits.defer(endpoint, reset);
        Some(Failure::wait("Tumblr hourly API rate limit exceeded"))
    }

    async fn attempt(
        &self,
        endpoint: &Url,
        attempt: u8,
        accept: Option<&str>,
        follow: bool,
    ) -> std::result::Result<Reply, Failure> {
        let mut target = endpoint.clone();
        let mut redirects = Redirects::new(endpoint);
        let mut signed;
        let mut response = loop {
            self.rate_limits.wait(&target, self.timeout).await?;
            // With the web token, API calls authenticate by header instead.
            let token = self
                .web_token()
                .filter(|_| target.host_str() == self.api_root.host_str());
            // OAuth signs only API-origin requests, replacing `api_key`.
            let oauth = self
                .oauth()
                .filter(|_| token.is_none() && target.origin() == self.api_root.origin());
            signed = oauth.is_some();
            if token.is_some() || oauth.is_some() {
                let pairs: Vec<(String, String)> = target
                    .query_pairs()
                    .filter(|(key, _)| key != "api_key")
                    .map(|(k, v)| (k.into_owned(), v.into_owned()))
                    .collect();
                target.set_query(None);
                if !pairs.is_empty() {
                    target.query_pairs_mut().extend_pairs(pairs);
                }
            }
            let mut request = self
                .client
                .get(target.clone())
                .timeout(self.request_timeout);
            if let Some(token) = &token {
                request = request
                    .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(reqwest::header::ORIGIN, "https://www.tumblr.com");
            }
            if let Some(oauth) = &oauth {
                let value = oauth
                    .header("GET", &target)
                    .map_err(|_| error("invalid Tumblr OAuth credentials"))?;
                request = request.header(reqwest::header::AUTHORIZATION, value);
            }
            if let Some(accept) = accept {
                request = request.header(reqwest::header::ACCEPT, accept);
            }
            let response = request
                .send()
                .await
                .map_err(|_| Failure::transient("Tumblr request failed; check network access"))?;
            if !follow && response.status().is_redirection() {
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| response.url().join(value).ok())
                    .ok_or_else(|| error("HTTP redirect has missing or invalid Location"))?;
                return Ok(Reply::Location(location));
            }
            match redirects.next(response.url(), response.status(), response.headers())? {
                Some(next) => target = next,
                None => break response,
            }
        };
        let status = response.status();
        if status == StatusCode::TOO_MANY_REQUESTS
            && let Some(failure) = self.rate_limit(endpoint, response.headers())
        {
            return Err(failure);
        }
        if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
            let delay = retry_after(response.headers(), SystemTime::now());
            self.rate_limits
                .defer(endpoint, delay.unwrap_or_else(|| backoff(attempt)));
            return Err(Failure::transient(
                if status == StatusCode::TOO_MANY_REQUESTS {
                    match delay {
                        Some(delay) => {
                            format!("Tumblr rate limit; retry after {} seconds", delay.as_secs())
                        }
                        None => "Tumblr rate limit; retry later".into(),
                    }
                } else {
                    format!("Tumblr returned HTTP {status}")
                },
            ));
        }
        if matches!(status.as_u16(), 403 | 404) {
            // Upstream inspects the JSON error for OAuth fallbacks and
            // dashboard-only blogs; only fixed messages are reported.
            let body = small_body(&mut response).await;
            let detail = |pointer: &str| {
                body.as_ref()
                    .and_then(|body| body.pointer(pointer))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            };
            if status == StatusCode::FORBIDDEN
                && signed
                && detail("/response").as_deref() == Some(NO_PERMISSION)
            {
                return Err(Failure {
                    key_fallback: true,
                    ..error("Tumblr refused OAuth access to this blog").into()
                });
            }
            if status == StatusCode::NOT_FOUND
                && detail("/errors/0/detail").is_some_and(|detail| {
                    detail.contains("only viewable within the Tumblr dashboard")
                })
            {
                return Err(error(if signed {
                    "Tumblr blog is only viewable within the Tumblr dashboard; ensure access-token and access-token-secret belong to the same application as api-key and api-secret"
                } else {
                    "Tumblr blog is only viewable within the Tumblr dashboard; configure access-token and access-token-secret"
                })
                .into());
            }
        }
        if matches!(status.as_u16(), 404 | 410) {
            return Err(error("Tumblr blog or post not found").into());
        }
        if matches!(status.as_u16(), 401 | 403) {
            return Err(error(if signed {
                "Tumblr refused access; check the api-key, api-secret, access-token and access-token-secret settings"
            } else {
                "Tumblr refused access; check the api-key setting"
            })
            .into());
        }
        if !status.is_success() {
            return Err(error(format!("Tumblr returned HTTP {status}")).into());
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| Failure::transient("could not read Tumblr response"))?
        {
            if body.len() + chunk.len() > MAX_RESPONSE {
                return Err(error("Tumblr response exceeded 32 MiB").into());
            }
            body.extend_from_slice(&chunk);
        }
        Ok(Reply::Body(body))
    }
}

/// Fill a Neue Post Format post's legacy fields (`photos`, `video_url`,
/// `audio_url`, `body`, `type`) from its own and its reblog trail's content
/// blocks, in display order. Legacy posts are left unchanged.
fn npf_to_legacy(post: &mut Value) {
    let Some(post) = post.as_object_mut() else {
        return;
    };
    if post.contains_key("photos") || !post.get("content").is_some_and(Value::is_array) {
        return;
    }
    let mut blocks: Vec<Value> = Vec::new();
    for item in post
        .get("trail")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        blocks.extend(
            item.get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .cloned(),
        );
    }
    blocks.extend(post["content"].as_array().into_iter().flatten().cloned());
    let mut photos = Vec::new();
    let (mut video, mut audio) = (None, None);
    let mut body = String::new();
    for block in &blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("image") => {
                let mut media: Vec<Value> = block
                    .get("media")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter(|m| m.get("url").and_then(Value::as_str).is_some())
                    .cloned()
                    .map(|mut m| {
                        // NPF names PNGs `.pnj` and GIFs `.gifv`; both serve
                        // the image itself under its real extension too.
                        let url = m["url"].as_str().unwrap_or_default();
                        let real = if let Some(base) = url.strip_suffix(".pnj") {
                            Some(format!("{base}.png"))
                        } else {
                            url.strip_suffix(".gifv").map(|base| format!("{base}.gif"))
                        };
                        if let Some(real) = real {
                            m["url"] = real.into();
                        }
                        m
                    })
                    .collect();
                let width = |m: &Value| m.get("width").and_then(Value::as_u64).unwrap_or(0);
                let original = media
                    .iter()
                    .position(|m| m.get("has_original_dimensions") == Some(&Value::Bool(true)))
                    .or_else(|| (0..media.len()).max_by_key(|&i| width(&media[i])));
                if let Some(index) = original {
                    let original = media.remove(index);
                    photos.push(serde_json::json!({
                        "original_size": original,
                        "alt_sizes": media,
                    }));
                }
            }
            Some("video") => {
                video = video.or_else(|| block.pointer("/media/url").cloned());
            }
            Some("audio") => {
                audio = audio.or_else(|| {
                    block
                        .pointer("/media/url")
                        .or_else(|| block.get("url"))
                        .cloned()
                });
            }
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    let text = text
                        .replace('&', "&amp;")
                        .replace('<', "&lt;")
                        .replace('>', "&gt;");
                    body.push_str(&format!("<p>{text}</p>"));
                }
            }
            _ => {}
        }
    }
    let kind = match post.get("original_type").and_then(Value::as_str) {
        Some("regular") | None => {
            if !photos.is_empty() {
                "photo"
            } else if video.is_some() {
                "video"
            } else if audio.is_some() {
                "audio"
            } else {
                "text"
            }
        }
        Some("note") => "answer",
        Some(kind) => kind,
    };
    post.insert("type".into(), kind.into());
    if !photos.is_empty() {
        post.insert("photos".into(), photos.into());
    }
    if let Some(video) = video {
        post.insert("video_url".into(), video);
    }
    if let Some(audio) = audio {
        post.insert("audio_url".into(), audio);
    }
    if !body.is_empty() {
        post.insert("body".into(), body.into());
    }
}

/// Upstream `_prepare_avatar`: the avatar as its own one-file post.
fn avatar_media(url: Url, post: &Value, blog: &str, info: Value, route: &'static Route) -> Media {
    let mut fields = post.as_object().cloned().unwrap_or_default();
    let (name, ext) = crate::sites::nameext(&url);
    fields.insert("blog".into(), info);
    fields.insert("reblogged".into(), false.into());
    for key in ["type", "id", "hash"] {
        fields.insert(key.into(), "avatar".into());
    }
    fields.insert("num".into(), 1.into());
    fields.insert("count".into(), 1.into());
    fields.insert("category".into(), "tumblr".into());
    fields.insert("subcategory".into(), route.subcategory.into());
    let filename = file_name(&name, &ext);
    fields.insert("filename".into(), name.into());
    fields.insert("extension".into(), ext.into());
    Media {
        filename,
        job: MediaJob::DirectHttp,
        category: "tumblr".into(),
        metadata: Some(PostMetadata {
            post_id: "avatar".into(),
            media_id: media_id(&url),
            username: Some(Arc::from(blog)),
            index: 1,
            site_keywords: Some(Arc::new(SiteKeywords { route, fields })),
            ..PostMetadata::default()
        }),
        url,
        headers: BTreeMap::new(),
        handoff: BTreeMap::new(),
    }
}

fn backoff(attempt: u8) -> Duration {
    Duration::from_millis(250 * (1u64 << attempt.min(6)))
}

/// A bounded JSON error body; oversized or invalid bodies are ignored.
async fn small_body(response: &mut reqwest::Response) -> Option<Value> {
    let mut body = Vec::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        if body.len() + chunk.len() > 64 * 1024 {
            return None;
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).ok()
}

struct Failure {
    error: crate::Error,
    retry: bool,
    /// A rate-limit wait already registered with `RateLimits`.
    wait: bool,
    /// OAuth was refused for this blog; retry with `api_key` authentication.
    key_fallback: bool,
}

impl Failure {
    fn transient(message: impl Into<String>) -> Self {
        Self {
            error: error(message),
            retry: true,
            wait: false,
            key_fallback: false,
        }
    }

    fn wait(message: impl Into<String>) -> Self {
        Self {
            error: error(message),
            retry: true,
            wait: true,
            key_fallback: false,
        }
    }
}

impl From<crate::Error> for Failure {
    fn from(error: crate::Error) -> Self {
        Self {
            error,
            retry: false,
            wait: false,
            key_fallback: false,
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
        thread::{self, JoinHandle},
    };

    const PHOTO: &str = "https://64.media.tumblr.com/abc/def/s2048x3072/one.jpg";

    fn post(id: u64) -> Value {
        json!({"id": id, "id_string": id.to_string(), "blog_name": "alice", "type": "photo",
            "timestamp": 1700000000, "summary": "Hi there",
            "caption": "<p>Hi <b>there</b> <img src=\"https://64.media.tumblr.com/x/inline.png\"></p>",
            "photos": [
                {"original_size": {"url": PHOTO, "width": 2000, "height": 1500}},
                {"original_size": {"url": "http://64.media.tumblr.com/insecure.jpg"},
                 "alt_sizes": [
                    {"url": "https://64.media.tumblr.com/s/small.jpg", "width": 100},
                    {"url": "https://64.media.tumblr.com/s/big.jpg", "width": 500, "height": 400}]},
                {"original_size": {"url": "https://evil.example/x.jpg"}},
                {"original_size": {"url": PHOTO}}],
            "video_url": "https://va.media.tumblr.com/tumblr_v.mp4",
            "audio_url": "https://www.example.com/song.mp3"})
    }

    #[test]
    fn parses_legacy_post_media() {
        let media = parse_post(&post(7), "fallback");
        let urls: Vec<_> = media.iter().map(|m| m.url.as_str()).collect();
        assert_eq!(
            urls,
            [
                PHOTO,
                "https://64.media.tumblr.com/s/big.jpg",
                "https://va.media.tumblr.com/tumblr_v.mp4",
                "https://64.media.tumblr.com/x/inline.png",
            ]
        );
        assert_eq!(media[0].filename, "one.jpg");
        assert_eq!(media[2].filename, "tumblr_v.mp4");
        assert_eq!(media[3].filename, "inline.png");
        let meta = media[0].metadata.as_ref().unwrap();
        assert_eq!(meta.post_id, "7");
        assert_eq!(meta.username.as_deref(), Some("alice"));
        assert_eq!(meta.caption.as_deref(), Some("Hi there"));
        assert_eq!(meta.timestamp, Some(1700000000));
        assert!(valid_media_id(&meta.media_id));
        let file = meta.file.as_ref().unwrap();
        assert_eq!((file.width, file.height), (Some(2000), Some(1500)));
        let key = media[0].archive_key().unwrap();
        assert!(key.starts_with("tumblr:"), "{key}");
        assert_ne!(Some(key.clone()), media[1].archive_key());
        assert_ne!(Some(key), parse_post(&post(8), "alice")[0].archive_key());
        let audio = parse_post(
            &json!({"id": 9, "audio_url": "https://a.tumblr.com/tumblr_a.mp3"}),
            "bob",
        );
        assert_eq!(audio[0].filename, "tumblr_a.mp3");
        assert_eq!(
            audio[0].metadata.as_ref().unwrap().username.as_deref(),
            Some("bob")
        );
    }

    #[test]
    fn parses_pages_and_single_posts() {
        let body = json!({"meta": {"status": 200},
            "response": {"posts": [post(1), {"id": 2, "type": "text"}], "total_posts": 2}})
        .to_string();
        let page = parse_page(body.as_bytes(), "alice").unwrap();
        assert_eq!((page.posts.len(), page.count, page.total), (1, 2, Some(2)));
        assert_eq!(
            parse_single(body.as_bytes(), "alice", "1").unwrap().len(),
            4
        );
        assert!(
            parse_single(body.as_bytes(), "alice", "2")
                .unwrap_err()
                .to_string()
                .contains("no supported media")
        );
        let empty = json!({"response": {"posts": []}}).to_string();
        assert!(parse_single(empty.as_bytes(), "alice", "1").is_err());
        assert!(parse_page(b"{}", "alice").is_err());
        assert!(parse_page(b"nope", "alice").is_err());
    }

    #[test]
    fn original_candidates_respect_photo_caps_and_inline_sizes() {
        let url = Url::parse(PHOTO).unwrap();
        let expected = "https://64.media.tumblr.com/abc/def/s99999x99999/one.jpg";
        assert_eq!(
            original_image(&url, Some(2048), None, false)
                .unwrap()
                .as_str(),
            expected
        );
        assert_eq!(
            original_image(&url, None, Some(3072), false)
                .unwrap()
                .as_str(),
            expected
        );
        assert!(original_image(&url, Some(2000), Some(1500), false).is_none());
        assert!(original_image(&url, None, None, false).is_none());
        let inline =
            Url::parse("https://64.media.tumblr.com/a/b/s500x750/x.png?token=one").unwrap();
        assert_eq!(
            original_image(&inline, None, None, true).unwrap().as_str(),
            "https://64.media.tumblr.com/a/b/s99999x99999/x.png?token=one"
        );
        let malformed = Url::parse("https://64.media.tumblr.com/a/sx750/x.png").unwrap();
        assert!(original_image(&malformed, None, None, true).is_none());
        let mut capped = post(10);
        capped["photos"][0]["original_size"]["width"] = json!(2048);
        let parsed = parse_post(&capped, "alice");
        assert_eq!(parsed[0].handoff["_tumblr_original"], expected);
        assert!(!parsed[1].handoff.contains_key("_tumblr_original"));
    }

    #[test]
    fn selects_larger_photo_alternatives_and_upscales_legacy_inline_images() {
        let parsed = parse_post(
            &json!({"id":1,"photos":[{
            "original_size":{"url":"https://64.media.tumblr.com/tiny.jpg","width":500,"height":400},
            "alt_sizes":[{"url":PHOTO,"width":2048,"height":1536}]
        }],"body":"<img src=\"https://64.media.tumblr.com/abc/tumblr_inline_HASH_500.jpg\">"}),
            "alice",
        );
        assert_eq!(parsed[0].url.as_str(), PHOTO);
        assert!(parsed[0].handoff.contains_key("_tumblr_original"));
        assert_eq!(
            parsed[1].url.as_str(),
            "https://64.media.tumblr.com/abc/tumblr_inline_HASH_1280.jpg"
        );
        for raw in [
            "https://va.media.tumblr.com/tumblr_HASH_500.jpg",
            "https://64.media.tumblr.com/arbitrary_500.jpg",
            "https://64.media.tumblr.com/tumblr_HASH_other_500.jpg",
        ] {
            assert_eq!(inline_image(Url::parse(raw).unwrap()).as_str(), raw);
        }
        let sizes = json!({
            "original_size":{"url":PHOTO,"width":2048,"height":1000},
            "alt_sizes":[{"url":"https://64.media.tumblr.com/tall.jpg","width":1000,"height":2000}]
        });
        let taller = photo(&sizes).unwrap();
        assert_eq!(taller.3["height"], 2000);
        assert_eq!(taller.0.as_str(), "https://64.media.tumblr.com/tall.jpg");
    }

    #[test]
    fn classifies_supported_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        let posts = Target::Posts {
            blog: "alice".into(),
        };
        let single = Target::Post {
            blog: "alice".into(),
            id: "123".into(),
        };
        assert_eq!(t("https://alice.tumblr.com").unwrap(), posts);
        assert_eq!(t("https://alice.tumblr.com/posts").unwrap(), posts);
        assert_eq!(t("https://www.tumblr.com/alice").unwrap(), posts);
        assert_eq!(t("https://alice.tumblr.com/post/123").unwrap(), single);
        assert_eq!(t("https://alice.tumblr.com/post/123/slug").unwrap(), single);
        assert_eq!(t("https://www.tumblr.com/alice/123").unwrap(), single);
        assert_eq!(t("https://www.tumblr.com/alice/123/slug").unwrap(), single);
        assert_eq!(
            t("https://alice.tumblr.com/tagged/cute-cats%21").unwrap(),
            Target::Tagged {
                blog: "alice".into(),
                tag: "cute cats!".into()
            }
        );
        for raw in [
            "https://www.tumblr.com/",
            "https://www.tumblr.com/dashboard",
            "https://www.tumblr.com/tagged/cats",
            "https://64.media.tumblr.com/abc/one.jpg",
            "https://media.tumblr.com/x.jpg",
            "https://api.tumblr.com/v2/blog",
            "https://alice.tumblr.com/post/abc",
            "https://alice.tumblr.com/about",
            "https://alice.tumblr.com:444/",
            "https://tumblr.com.evil.example/alice",
            "https://alice.tumblr.com.evil.example/",
        ] {
            assert!(t(raw).is_err(), "{raw}");
        }
    }

    #[test]
    fn tumblr_prefix_selects_custom_blog_domains() {
        let input = |raw: &str| crate::extract::input_url(raw).unwrap();
        let t = |raw: &str| target(&input(raw));
        let custom = |blog: &str| Target::Posts { blog: blog.into() };
        assert_eq!(
            t("tumblr:https://blog.example.com/").unwrap(),
            custom("blog.example.com")
        );
        assert_eq!(
            t("tumblr:Blog.Example.com/post/123/slug").unwrap(),
            Target::Post {
                blog: "blog.example.com".into(),
                id: "123".into()
            }
        );
        assert_eq!(
            t("tumblr:www.example.org/tagged/a-b").unwrap(),
            Target::Tagged {
                blog: "www.example.org".into(),
                tag: "a b".into()
            }
        );
        assert_eq!(
            t("tumblr:example.org/likes").unwrap(),
            Target::Likes {
                blog: "example.org".into()
            }
        );
        // Tumblr hosts keep their ordinary classification.
        assert_eq!(t("tumblr:alice.tumblr.com").unwrap(), custom("alice"));
        assert_eq!(
            t("tumblr:https://www.tumblr.com/alice/123").unwrap(),
            Target::Post {
                blog: "alice".into(),
                id: "123".into()
            }
        );
        let url = input("tumblr:blog.example.com/123");
        assert!(is_blog_url(&url));
        assert_eq!(crate::extract::social_site(&url), Some("tumblr"));
        for raw in [
            "tumblr:https://blog.example.com:8443/",
            "tumblr:https://localhost/",
            "tumblr:https://media.tumblr.com/",
            "tumblr:https://blog.example.com/about",
        ] {
            assert!(t(raw).is_err(), "{raw}");
            assert_eq!(crate::extract::social_site(&input(raw)), None, "{raw}");
        }
        assert_eq!(blog_host("alice"), "alice.tumblr.com");
        assert_eq!(blog_host("blog.example.com"), "blog.example.com");
    }

    #[tokio::test]
    async fn custom_domains_address_the_api_by_domain() {
        let body = json!({"response": {"blog": {"name": "alice", "uuid": "t:alice"},
            "posts": [post(123)], "total_posts": 1}})
        .to_string();
        let (root, server) = serve(vec![response("200 OK", body.as_bytes())]);
        let media: Vec<_> = tumblr(root)
            .stream(&crate::extract::input_url("tumblr:blog.example.com").unwrap())
            .collect()
            .await;
        assert!(
            !media.is_empty() && media.iter().all(Result::is_ok),
            "{media:?}"
        );
        // Archive identity follows the post's blog name, not the domain.
        let first = media[0].as_ref().unwrap();
        assert_eq!(
            first.metadata.as_ref().unwrap().username.as_deref(),
            Some("alice")
        );
        assert!(first.archive_key().is_some());
        assert_eq!(
            first.archive_key(),
            parse_post(&post(123), "alice")[0].archive_key()
        );
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with("GET /v2/blog/blog.example.com/posts?api_key="),
            "{}",
            requests[0]
        );
    }

    fn response(status: &str, body: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\nRetry-After: 0\r\n\r\n",
            body.len()
        )
        .into_bytes();
        out.extend_from_slice(body);
        out
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

    fn tumblr(root: Url) -> Tumblr {
        Tumblr::new(Duration::from_secs(5))
            .unwrap()
            .with_network(&NetworkOptions::new(Some(String::new()), None).unwrap())
            .unwrap()
            .with_api_root(root)
    }

    #[tokio::test]
    async fn resolves_original_tokens_without_api_credentials_and_preserves_identity() {
        let original = "https://64.media.tumblr.com/new/token/s4096x4096/one.jpg";
        let body = format!("<html><img src=\"{original}\"></html>");
        let (root, server) = serve(vec![response("200 OK", body.as_bytes())]);
        let native = tumblr(root.clone()).with_api_key(Some("private-api-key"));
        let mut media = parse_post(&post(7), "alice").remove(0);
        let identity = media.archive_key();
        native
            .finish_original(&mut media, root.join("s99999x99999/one.jpg").unwrap())
            .await;
        assert_eq!(media.url.as_str(), original);
        assert_eq!(media.archive_key(), identity);
        assert!(!media.handoff.contains_key("_fallback"));
        let requests = server.join().unwrap();
        let request = requests[0].to_ascii_lowercase();
        assert!(request.starts_with("get /s99999x99999/one.jpg "));
        assert!(request.contains("accept: text/html,*/*;q=0.8\r\n"));
        assert!(!request.contains("api_key") && !request.contains("private-api-key"));
        assert!(!request.contains("authorization:") && !request.contains("cookie:"));
    }

    #[tokio::test]
    async fn rejects_unsafe_token_urls_and_keeps_a_download_fallback() {
        for src in [
            "https://evil.example/one.jpg",
            "http://64.media.tumblr.com/one.jpg",
            "https://user:pass@64.media.tumblr.com/one.jpg",
            "https://64.media.tumblr.com:444/one.jpg",
            "https://64.media.tumblr.com.evil.example/one.jpg",
        ] {
            let html = format!("<img src=\"{src}\">");
            let (root, server) = serve(vec![response("200 OK", html.as_bytes())]);
            assert!(
                tumblr(root.clone()).resolve_original(&root).await.is_none(),
                "{src}"
            );
            assert_eq!(server.join().unwrap().len(), 1);
        }
        let (root, server) = serve(vec![response("404 Not Found", b"")]);
        let mut media = parse_post(&post(7), "alice").remove(0);
        let original = root.join("s99999x99999/one.jpg").unwrap();
        tumblr(root)
            .finish_original(&mut media, original.clone())
            .await;
        assert_eq!(media.url, original);
        assert_eq!(media.handoff["_fallback"], json!([PHOTO]));
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn uncapped_photos_skip_token_fetches_and_gifs_offer_gifv_fallback() {
        // No listener is started: finishing these files must not perform I/O.
        let native = tumblr(Url::parse("http://127.0.0.1:9/").unwrap());
        let mut count = 0;
        for event in crate::event::group(parse_post(&post(7), "alice"), "tumblr") {
            native.finish_event(event).await;
            count += 1;
        }
        assert_eq!(count, 6);
        let gif = parse_post(
            &json!({"id":1,"body":"<img src=\"https://64.media.tumblr.com/tumblr_HASH_500.gif?token=yes\">"}),
            "alice",
        );
        let event = native
            .finish_event(ExtractionEvent::Media(Box::new(
                gif.into_iter().next().unwrap(),
            )))
            .await;
        let ExtractionEvent::Media(gif) = &event else {
            panic!("expected media");
        };
        assert_eq!(
            gif.url.as_str(),
            "https://64.media.tumblr.com/tumblr_HASH_1280.gif?token=yes"
        );
        assert_eq!(
            gif.handoff["_fallback"],
            json!(["https://64.media.tumblr.com/tumblr_HASH_1280.gifv?token=yes"])
        );
        assert!(gif.headers["Accept"].contains("image/webp"));
        assert!(!gif.handoff.contains_key("_tumblr_original"));
    }

    #[test]
    fn selection_scopes_reblogs_post_types_and_inline_images() {
        let value = json!({"id":1,"type":"photo","blog":{"uuid":"t:alice"},
            "reblogged_from_id":2,"reblogged_root_uuid":"t:alice",
            "photos":[{"original_size":{"url":PHOTO,"width":2048,"height":3072}}],
            "body":"<img src=\"https://64.media.tumblr.com/body.jpg\">",
            "question":"<img src=\"https://64.media.tumblr.com/question.jpg\">",
            "reblog":{"comment":"<img src=\"https://64.media.tumblr.com/comment.jpg\">",
                      "tree_html":"<img src=\"https://64.media.tumblr.com/tree.jpg\">"}});
        let selection = Selection {
            reblogs: Reblogs::OriginalOnly,
            ..Selection::default()
        };
        assert!(parse_post_selected(&value, "alice", &selection).is_empty());
        let selection = Selection {
            reblogs: Reblogs::SameBlog,
            inline: Inline::Reblog,
            ..Selection::default()
        };
        let media = parse_post_selected(&value, "alice", &selection);
        assert_eq!(
            media.iter().map(|m| m.url.path()).collect::<Vec<_>>(),
            [
                Url::parse(PHOTO).unwrap().path(),
                "/question.jpg",
                "/comment.jpg"
            ]
        );
        let mut other = value.clone();
        other["reblogged_root_uuid"] = json!("t:other");
        assert!(parse_post_selected(&other, "alice", &selection).is_empty());
        let mut fallback = value.clone();
        fallback["blog"] = json!({"name":"renamed"});
        fallback["reblogged_root_uuid"] = json!("renamed.tumblr.com");
        assert!(!parse_post_selected(&fallback, "alice", &selection).is_empty());
        fallback["reblogged_root_uuid"] = json!("alice.tumblr.com");
        assert!(parse_post_selected(&fallback, "alice", &selection).is_empty());
        let selection = Selection {
            inline: Inline::Original,
            ..Selection::default()
        };
        let media = parse_post_selected(&value, "alice", &selection);
        assert!(media.last().unwrap().url.path().ends_with("/tree.jpg"));
        other["reblog"]["tree_html"] = json!("");
        assert!(
            parse_post_selected(&other, "alice", &selection)
                .last()
                .unwrap()
                .url
                .path()
                .ends_with("/comment.jpg")
        );
        let selection = Selection {
            inline: Inline::Disabled,
            original: false,
            ..Selection::default()
        };
        let media = parse_post_selected(&value, "alice", &selection);
        assert_eq!(media.len(), 1);
        assert!(!media[0].handoff.contains_key("_tumblr_original"));
        assert_eq!(
            media[0].metadata.as_ref().unwrap().media_id,
            parse_post(&value, "alice")[0]
                .metadata
                .as_ref()
                .unwrap()
                .media_id
        );
        let selection = Selection::default().with_posts(["video".into()]).unwrap();
        assert!(parse_post_selected(&value, "alice", &selection).is_empty());
        assert!(Selection::default().with_posts(["unknown".into()]).is_err());
        for invalid in [Value::Null, json!(42), json!("unsupported")] {
            let mut malformed = value.clone();
            malformed["type"] = invalid;
            assert!(parse_post_selected(&malformed, "alice", &selection).is_empty());
        }
        assert!(
            parse_post_selected(
                &value,
                "alice",
                &Selection::default().with_posts([]).unwrap()
            )
            .is_empty()
        );
    }

    #[tokio::test]
    async fn filtered_pages_keep_offsets_and_single_posts_ignore_type_filter() {
        let photo = |id| json!({"id":id,"type":"photo","photos":[{"original_size":{"url":PHOTO}}]});
        let video =
            json!({"id":2,"type":"video","video_url":"https://64.media.tumblr.com/video.mp4"});
        let pages = [
            json!({"response":{"posts":[photo(1)],"total_posts":2}}),
            json!({"response":{"posts":[video],"total_posts":2}}),
        ];
        let (root, server) = serve(
            pages
                .iter()
                .map(|p| response("200 OK", p.to_string().as_bytes()))
                .collect(),
        );
        let native =
            tumblr(root).with_selection(Selection::default().with_posts(["video".into()]).unwrap());
        let files: Vec<_> = native
            .stream(&Url::parse("https://alice.tumblr.com/").unwrap())
            .collect()
            .await;
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0]
                .as_ref()
                .unwrap()
                .metadata
                .as_ref()
                .unwrap()
                .post_id,
            "2"
        );
        let requests = server.join().unwrap();
        assert!(requests[1].contains("&offset=1&limit=50 "));
        let (root, server) = serve(vec![response(
            "200 OK",
            json!({"response":{"posts":[photo(1)]}})
                .to_string()
                .as_bytes(),
        )]);
        let native = tumblr(root).with_selection(Selection::default().with_posts([]).unwrap());
        assert_eq!(
            native
                .stream(&Url::parse("https://alice.tumblr.com/post/1").unwrap())
                .collect::<Vec<_>>()
                .await
                .len(),
            1
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn original_disabled_emits_capped_images_without_token_requests() {
        let body = json!({"response":{"posts":[{"id":1,"photos":[{"original_size":{
            "url":"https://64.media.tumblr.com/abc/def/s2048x3072/one.jpg", "width":2048,"height":1536
        }}]}],"total_posts":1}}).to_string();
        // With no proxy and the localhost API fixture, a mistaken token lookup
        // would contact the CDN; the short timeout bounds this regression.
        let (root, server) = serve(vec![response("200 OK", body.as_bytes())]);
        let native = tumblr(root).with_selection(Selection {
            original: false,
            ..Selection::default()
        });
        let mut files = native.stream(&Url::parse("https://alice.tumblr.com/").unwrap());
        let first = tokio::time::timeout(Duration::from_secs(1), files.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        drop(files);
        assert_eq!(first.url.path(), "/abc/def/s2048x3072/one.jpg");
        assert!(!first.handoff.contains_key("_tumblr_original"));
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[test]
    fn npf_posts_gain_legacy_media_fields() {
        let mut post = json!({"id": 5, "original_type": "regular", "content": [
            {"type": "text", "text": "a <b>"},
            {"type": "image", "media": [
                {"url": "https://64.media.tumblr.com/k/s640x960/small.pnj", "width": 640},
                {"url": "https://64.media.tumblr.com/k/s1280x1920/big.pnj", "width": 1080,
                 "has_original_dimensions": true}]},
            {"type": "image", "media": [
                {"url": "https://64.media.tumblr.com/g/s540x810/anim.gifv", "width": 540}]},
            {"type": "video", "media": {"url": "https://va.media.tumblr.com/tumblr_v.mp4"}}],
            "trail": [{"content": [{"type": "image", "media": [
                {"url": "https://64.media.tumblr.com/t/s1280x1920/first.jpg", "width": 9}]}]}]});
        npf_to_legacy(&mut post);
        assert_eq!(post["type"], "photo");
        let photos = post["photos"].as_array().unwrap();
        assert_eq!(photos.len(), 3);
        assert!(
            photos[0]["original_size"]["url"]
                .as_str()
                .unwrap()
                .ends_with("first.jpg")
        );
        assert!(
            photos[1]["original_size"]["url"]
                .as_str()
                .unwrap()
                .ends_with("/big.png")
        );
        assert!(
            photos[1]["alt_sizes"][0]["url"]
                .as_str()
                .unwrap()
                .ends_with("/small.png")
        );
        assert!(
            photos[2]["original_size"]["url"]
                .as_str()
                .unwrap()
                .ends_with("/anim.gif")
        );
        assert_eq!(
            post["video_url"],
            "https://va.media.tumblr.com/tumblr_v.mp4"
        );
        assert_eq!(post["body"], "<p>a &lt;b&gt;</p>");
        // Legacy posts are left alone.
        let mut legacy = json!({"id": 1, "type": "photo", "photos": [], "content": []});
        let before = legacy.clone();
        npf_to_legacy(&mut legacy);
        assert_eq!(legacy, before);
    }

    #[tokio::test]
    async fn exhausted_default_key_falls_back_to_the_web_token() {
        let limited = b"HTTP/1.1 429 Too Many Requests\r\nX-Ratelimit-Perday-Remaining: 0\r\n\
            X-Ratelimit-Perday-Reset: 100\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let npf = json!({"response": {"posts": [{"id": 7, "id_string": "7", "blog_name": "alice",
            "original_type": "photo", "timestamp": 1, "content": [{"type": "image", "media": [
                {"url": "https://64.media.tumblr.com/k/s1280x1920/a.pnj", "width": 10,
                 "has_original_dimensions": true}]}]}]}})
        .to_string();
        let (root, server) = serve(vec![
            limited.to_vec(),
            response("200 OK", br#"<script>{"API_TOKEN":"webToken123"}</script>"#),
            response("200 OK", npf.as_bytes()),
        ]);
        let media: Vec<_> = tumblr(root.clone())
            .with_web_root(root.join("/web/").unwrap())
            .stream(&Url::parse("https://www.tumblr.com/alice/7").unwrap())
            .collect()
            .await;
        let urls: Vec<String> = media
            .iter()
            .map(|m| m.as_ref().unwrap().url.to_string())
            .collect();
        assert_eq!(urls, ["https://64.media.tumblr.com/k/s1280x1920/a.png"]);
        let requests = server.join().unwrap();
        assert!(requests[0].contains(&format!("api_key={DEFAULT_API_KEY}")));
        assert!(requests[1].starts_with("GET /web/ "));
        assert!(!requests[2].contains("api_key"), "{}", requests[2]);
        assert!(
            requests[2]
                .to_ascii_lowercase()
                .contains("authorization: bearer webtoken123\r\n")
        );
    }

    #[tokio::test]
    async fn fetches_post_with_retries_and_api_key() {
        let body = json!({"response": {"posts": [post(123)]}}).to_string();
        let (root, server) = serve(vec![
            response("503 Service Unavailable", b""),
            response("200 OK", body.as_bytes()),
        ]);
        let events: Vec<_> = tumblr(root)
            .with_retries(1)
            .with_api_key(Some("secret"))
            .events(&Url::parse("https://www.tumblr.com/alice/123").unwrap())
            .collect()
            .await;
        assert_eq!(events.len(), 6, "{events:?}");
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with(
            "GET /v2/blog/alice.tumblr.com/posts?api_key=secret&npf=false&reblog_info=true&id=123 "
        ));
    }

    #[tokio::test]
    async fn paginates_tagged_posts_by_offset() {
        let first: Vec<Value> = (1..=20).map(post).collect();
        let pages = [
            json!({"response": {"posts": first, "total_posts": 25}}),
            json!({"response": {"posts": [post(21)], "total_posts": 25}}),
            json!({"response": {"posts": []}}),
        ];
        let (root, server) = serve(
            pages
                .iter()
                .map(|page| response("200 OK", page.to_string().as_bytes()))
                .collect(),
        );
        let media: Vec<_> = tumblr(root)
            .stream(&Url::parse("https://alice.tumblr.com/tagged/a+b").unwrap())
            .collect()
            .await;
        assert_eq!(media.len(), 84);
        let requests = server.join().unwrap();
        let key = format!("api_key={DEFAULT_API_KEY}");
        assert!(requests[0].contains(&key));
        assert!(
            requests[0].contains("&tag=a+b&offset=0&limit=50 "),
            "{}",
            requests[0]
        );
        assert!(requests[1].contains("&offset=20&limit=50 "));
        assert!(requests[2].contains("&offset=21&limit=50 "));
    }

    #[tokio::test]
    async fn dropping_after_first_file_leaves_later_image_tokens_unrequested() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        for (input, skip_reblogs) in [
            ("https://alice.tumblr.com/", false),
            ("https://alice.tumblr.com/post/1", false),
            ("https://alice.tumblr.com/", true),
        ] {
            let photo = |name: &str| {
                json!({"original_size":{
                    "url":format!("https://64.media.tumblr.com/abc/def/s2048x3072/{name}.jpg"),
                    "width":2048,"height":1536
                }})
            };
            let mut body = json!({"response":{"total_posts":2,"posts":[
                {"id":1,"photos":[photo("one"),photo("two")]},
                {"id":2,"photos":[photo("three")]}
            ]}});
            if skip_reblogs {
                body["response"]["posts"].as_array_mut().unwrap().insert(
                    0,
                    json!({
                        "id":0,"reblogged_from_id":99,"photos":[photo("skipped")]
                    }),
                );
                body["response"]["total_posts"] = json!(3);
            }
            let body = body.to_string();
            // A local HTTP proxy records every attempted CDN token request as
            // CONNECT, without contacting Tumblr or needing a TLS fixture.
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let root = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let server_stop = stop.clone();
            let server = thread::spawn(move || {
                let mut requests = Vec::new();
                while !server_stop.load(Ordering::SeqCst) {
                    let (mut socket, _) = match listener.accept() {
                        Ok(connection) => connection,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(1));
                            continue;
                        }
                        Err(error) => panic!("fixture accept: {error}"),
                    };
                    socket.set_nonblocking(false).unwrap();
                    socket
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    let mut request = Vec::new();
                    let mut buffer = [0; 1024];
                    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                        let count = socket.read(&mut buffer).unwrap();
                        assert_ne!(count, 0);
                        request.extend_from_slice(&buffer[..count]);
                    }
                    let request = String::from_utf8(request).unwrap();
                    let reply = if request.starts_with("CONNECT ") {
                        response("502 Bad Gateway", b"")
                    } else {
                        response("200 OK", body.as_bytes())
                    };
                    requests.push(request);
                    socket.write_all(&reply).unwrap();
                }
                requests
            });
            let native = tumblr(root.clone())
                .with_selection(Selection {
                    reblogs: if skip_reblogs {
                        Reblogs::OriginalOnly
                    } else {
                        Reblogs::All
                    },
                    ..Selection::default()
                })
                .with_network(&NetworkOptions::new(Some(root.to_string()), None).unwrap())
                .unwrap();
            let mut files = native.stream(&Url::parse(input).unwrap());
            let first = files.next().await.unwrap().unwrap();
            drop(files);
            stop.store(true, Ordering::SeqCst);
            let requests = server.join().unwrap();
            assert_eq!(requests.len(), 2, "{input}: {requests:?}");
            assert!(requests[0].starts_with("GET "));
            assert!(requests[1].starts_with("CONNECT 64.media.tumblr.com:443 "));
            assert!(first.url.path().ends_with("/s99999x99999/one.jpg"));
            assert_eq!(
                first.handoff["_fallback"],
                json!(["https://64.media.tumblr.com/abc/def/s2048x3072/one.jpg"])
            );
            assert!(!first.handoff.contains_key("_tumblr_original"));
        }
    }

    #[tokio::test]
    async fn terminal_errors_do_not_leak_key() {
        let (root, server) = serve(vec![response("401 Unauthorized", b"{}")]);
        let error = tumblr(root)
            .with_retries(3)
            .with_api_key(Some("secret-key"))
            .stream(&Url::parse("https://alice.tumblr.com/").unwrap())
            .collect::<Vec<_>>()
            .await
            .remove(0)
            .unwrap_err()
            .to_string();
        assert!(error.contains("api-key"));
        assert!(!error.contains("secret-key") && !error.contains("127.0.0.1"));
        assert_eq!(server.join().unwrap().len(), 1);
    }

    /// Upstream class, option and keyword parity.
    mod parity {
        use super::*;

        fn reply(status: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
            let mut out = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
                body.len()
            );
            for (name, value) in headers {
                out.push_str(&format!("{name}: {value}\r\n"));
            }
            out.push_str("\r\n");
            let mut out = out.into_bytes();
            out.extend_from_slice(body);
            out
        }

        fn ok(body: Value) -> Vec<u8> {
            response("200 OK", body.to_string().as_bytes())
        }

        fn options(value: Value) -> Map<String, Value> {
            value.as_object().unwrap().clone()
        }

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

        fn route_of(media: &Media) -> &'static Route {
            media
                .metadata
                .as_ref()
                .unwrap()
                .site_keywords
                .as_ref()
                .unwrap()
                .route
        }

        fn files(events: &[Result<ExtractionEvent>]) -> Vec<&Media> {
            events
                .iter()
                .filter_map(|event| match event {
                    Ok(ExtractionEvent::Media(media)) => Some(&**media),
                    _ => None,
                })
                .collect()
        }

        fn photo_post(id: u64, timestamp: i64) -> Value {
            json!({"id": id, "blog_name": "alice", "type": "photo", "timestamp": timestamp,
                "photos": [{"original_size": {
                    "url": format!("https://64.media.tumblr.com/h/tumblr_p{id}_1280.jpg"),
                    "width": 1280, "height": 720}}]})
        }

        #[test]
        fn classifies_every_upstream_class() {
            let t = |raw: &str| target(&Url::parse(raw).unwrap()).unwrap();
            let blog = || "alice".to_string();
            for raw in [
                "https://www.tumblr.com/blog/alice",
                "https://www.tumblr.com/blog/view/alice",
                "https://tumblr.com/alice/",
                "https://alice.tumblr.com/page/2",
                "https://alice.tumblr.com/archive",
                "https://www.tumblr.com/blog/view/alice/archive",
            ] {
                assert_eq!(t(raw), Target::Posts { blog: blog() }, "{raw}");
            }
            for raw in [
                "https://alice.tumblr.com/image/123",
                "https://alice.tumblr.com/123",
                "https://alice.tumblr.com/post/123/slug/extra",
                "https://www.tumblr.com/blog/view/alice/123",
            ] {
                assert_eq!(
                    t(raw),
                    Target::Post {
                        blog: blog(),
                        id: "123".into()
                    },
                    "{raw}"
                );
            }
            assert_eq!(
                t("https://www.tumblr.com/alice/archive/tagged/a-b"),
                Target::Tagged {
                    blog: blog(),
                    tag: "a b".into()
                }
            );
            assert_eq!(
                t("https://alice.tumblr.com/day/2024/01/02"),
                Target::Day {
                    blog: blog(),
                    start: 1704153600
                }
            );
            assert_eq!(
                t("https://www.tumblr.com/alice/likes"),
                Target::Likes { blog: blog() }
            );
            assert_eq!(
                t("https://alice.tumblr.com/following"),
                Target::Following { blog: blog() }
            );
            assert_eq!(
                t("https://www.tumblr.com/blog/alice/followers"),
                Target::Followers { blog: blog() }
            );
            assert_eq!(
                t("https://www.tumblr.com/search/cute%20cats/recent/photo?t=365&src=typed&t=7"),
                Target::Search {
                    query: "cute cats".into(),
                    mode: Some("recent".into()),
                    post_type: Some("photo".into()),
                    params: vec![("src".into(), "typed".into()), ("t".into(), "7".into())],
                }
            );
            assert_eq!(
                t("https://tumblr.com/search/art"),
                Target::Search {
                    query: "art".into(),
                    mode: None,
                    post_type: None,
                    params: Vec::new(),
                }
            );
            for raw in [
                "https://alice.tumblr.com/day/2024/02/30",
                "https://alice.tumblr.com/day/24/01/02",
                "https://alice.tumblr.com/page/x",
                "https://www.tumblr.com/search",
                "https://www.tumblr.com/search/a/b/c/d",
                "https://www.tumblr.com/search/a/b%20c",
                "https://alice.tumblr.com/search/art",
                "https://www.tumblr.com/blog",
            ] {
                assert!(target(&Url::parse(raw).unwrap()).is_err(), "{raw}");
            }
            let routed = |raw: &str| is_blog_url(&Url::parse(raw).unwrap());
            assert!(routed("https://www.tumblr.com/search/art"));
            assert!(routed("https://www.tumblr.com/blog/view/alice"));
            assert!(routed("https://alice.tumblr.com/likes"));
            for raw in [
                "https://www.tumblr.com/",
                "https://www.tumblr.com/dashboard",
                "https://www.tumblr.com/search",
                "https://64.media.tumblr.com/x.jpg",
                "https://tumblr.com.evil.example/search/art",
            ] {
                assert!(!routed(raw), "{raw}");
            }
        }

        #[test]
        fn keyword_dictionaries_match_upstream() {
            let post = json!({"id": 5, "id_string": "5", "blog_name": "alice", "type": "photo",
                "timestamp": 1700000000, "reblogged_from_id": "3", "tags": ["x"],
                "trail": [{"blog": {"name": "bob"}}],
                "photos": [{"caption": "c", "original_size": {
                    "url": "https://64.media.tumblr.com/h/tumblr_abc123_500.jpg",
                    "width": 500, "height": 281},
                  "alt_sizes": [{"url": "https://64.media.tumblr.com/h/tumblr_abc123_1280.jpg",
                    "width": 1280, "height": 720}]}],
                "video_url": "https://ve.media.tumblr.com/tumblr_vid42_480.mp4",
                "body": "<img src=\"https://64.media.tumblr.com/h/tumblr_inline_xyz_500.png\"> <source src=\"https://vt.tumblr.com/tumblr_inv_720.mp4\" type=\"video/mp4\">"});
            let info = json!({"name": "alice", "uuid": "alice.tumblr.com", "title": "A"});
            let selection = Selection::default();
            let parsed = parse(
                &post,
                &Context {
                    route: &ROUTE_TAG,
                    blog: "alice",
                    blog_info: Some(&info),
                    tag: Some("cats"),
                    selection: &selection,
                    external: false,
                },
            );
            assert!(parsed.external.is_none());
            let media = parsed.media;
            let urls: Vec<_> = media.iter().map(|m| m.url.as_str()).collect();
            assert_eq!(
                urls,
                [
                    "https://64.media.tumblr.com/h/tumblr_abc123_1280.jpg",
                    "https://ve.media.tumblr.com/tumblr_vid42.mp4",
                    "https://64.media.tumblr.com/h/tumblr_inline_xyz_1280.png",
                    "https://vt.tumblr.com/tumblr_inv.mp4",
                ]
            );
            let hashes: Vec<_> = media.iter().map(|m| keywords(m)["hash"].clone()).collect();
            assert_eq!(
                hashes,
                [json!("abc123"), json!("vid42"), json!("xyz"), json!("inv")]
            );
            let k = keywords(&media[0]);
            assert!(!k.contains_key("trail") && !k.contains_key("photos"));
            assert_eq!(
                k["photo"],
                json!({"caption": "c", "width": 1280, "height": 720,
                "url": "https://64.media.tumblr.com/h/tumblr_abc123_1280.jpg"})
            );
            assert_eq!(k["blog"], info);
            assert_eq!(k["reblogged"], true);
            assert_eq!(k["date"], "2023-11-14 22:13:20");
            assert_eq!(k["search_tags"], "cats");
            assert_eq!(k["tags"], json!(["x"]));
            assert_eq!(k["id"], 5);
            assert_eq!(
                (&k["filename"], &k["extension"]),
                (&json!("tumblr_abc123_1280"), &json!("jpg"))
            );
            assert_eq!((&k["num"], &k["count"]), (&json!(1), &json!(4)));
            assert_eq!(
                (&k["category"], &k["subcategory"]),
                (&json!("tumblr"), &json!("tag"))
            );
            assert!(!keywords(&media[1]).contains_key("photo"));
            assert_eq!(keywords(&media[3])["num"], 4);
            assert_eq!(media[0].filename, "tumblr_abc123_1280.jpg");
            assert_eq!(media[3].metadata.as_ref().unwrap().index, 4);
            let (file, dir) = crate::sites::default_templates(&media[1]).unwrap();
            assert_eq!(file.render(&media[1]).unwrap(), "tumblr_alice_5_02.mp4");
            assert_eq!(
                dir.render(&media[1]).unwrap().unwrap(),
                std::path::PathBuf::from("tumblr/alice")
            );
            assert_eq!(route_of(&media[0]).archive, "{id}_{num}");
            // Native archive identity is unchanged.
            assert!(media[0].archive_key().unwrap().starts_with("tumblr:"));
            // Upstream `_prepare_image` and `_prepare` hash fallbacks.
            assert_eq!(image_hash("a_inline"), "a_inline");
            assert_eq!(image_hash("plain"), "plain");
            assert_eq!(image_hash("tumblr_inline_q_1280"), "q");
            assert_eq!(file_hash("plain"), "");
            assert_eq!(file_hash("tumblr_a_b"), "a_b");
            // Posts with their own blog keep it; others without info get none.
            let mut own = post.clone();
            own["blog"] = json!({"name": "alice", "uuid": "t:alice"});
            assert_eq!(
                keywords(&parse_post(&own, "alice")[0])["blog"]["uuid"],
                "t:alice"
            );
            assert!(!keywords(&parse_post(&post, "alice")[0]).contains_key("blog"));
            assert_eq!(
                keywords(&parse_post(&post, "alice")[0])["subcategory"],
                "user"
            );
        }

        #[test]
        fn external_links_are_counted_and_numbered_last() {
            let post = json!({"id": 9, "type": "link", "url": "https://example.com/page",
                "photos": [{"original_size": {"url": "https://64.media.tumblr.com/x.jpg"}}]});
            let selection = Selection::default();
            let context = |external| Context {
                route: &ROUTE_USER,
                blog: "alice",
                blog_info: None,
                tag: None,
                selection: &selection,
                external,
            };
            let parsed = parse(&post, &context(true));
            assert_eq!(keywords(&parsed.media[0])["count"], 2);
            let (url, fields) = parsed.external.unwrap();
            assert_eq!(url.as_str(), "https://example.com/page");
            assert_eq!((&fields["num"], &fields["count"]), (&json!(2), &json!(2)));
            let mut permalink = post.clone();
            permalink["permalink_url"] = json!("https://www.youtube.com/watch?v=x");
            assert_eq!(
                parse(&permalink, &context(true))
                    .external
                    .unwrap()
                    .0
                    .host_str(),
                Some("www.youtube.com")
            );
            permalink["permalink_url"] = json!("javascript:alert(1)");
            permalink["url"] = Value::Null;
            assert!(parse(&permalink, &context(true)).external.is_none());
            let parsed = parse(&post, &context(false));
            assert!(parsed.external.is_none());
            assert_eq!(keywords(&parsed.media[0])["count"], 1);
        }

        #[test]
        fn options_are_typed_and_validated_without_echoing_values() {
            let native = Tumblr::new(Duration::from_secs(5))
                .unwrap()
                .with_options(Some(&options(json!({"external": true, "avatar": true,
                    "offset": "40", "pagination": "before", "fallback-delay": 1.5,
                    "fallback-retries": 0, "ratelimit": "wait"}))))
                .unwrap();
            let o = &native.options;
            assert!(o.external && o.avatar && o.ratelimit_wait);
            assert_eq!(o.offset, Some(40));
            assert_eq!(o.pagination, Some(Pagination::Before));
            assert_eq!(o.fallback_delay, Duration::from_millis(1500));
            assert_eq!(o.fallback_retries, 0);
            let defaults = Tumblr::new(Duration::from_secs(5))
                .unwrap()
                .with_options(Some(&options(
                    json!({"avatar": null, "ratelimit": "abort"}),
                )))
                .unwrap();
            assert!(!defaults.options.avatar && !defaults.options.ratelimit_wait);
            assert_eq!(defaults.options.fallback_retries, 2);
            assert_eq!(defaults.options.fallback_delay, Duration::from_secs(120));
            for (key, value) in [
                ("external", json!("yes")),
                ("avatar", json!(1)),
                ("offset", json!(-1)),
                ("offset", json!("secret-x")),
                ("pagination", json!("cursor")),
                ("fallback-delay", json!(-1)),
                ("fallback-delay", json!("secret-x")),
                ("fallback-retries", json!(-1)),
                ("fallback-retries", json!(1000)),
                ("ratelimit", json!(true)),
                ("unknown", json!(1)),
            ] {
                let error = Tumblr::new(Duration::from_secs(5))
                    .unwrap()
                    .with_options(Some(&options(json!({key: value}))))
                    .err()
                    .unwrap_or_else(|| panic!("{key} accepted"))
                    .to_string();
                assert!(error.contains(key) && !error.contains("secret"), "{error}");
            }
        }

        #[tokio::test]
        async fn day_pages_by_timestamp_and_stops_before_the_day() {
            let start = 1704153600;
            let (root, server) = serve(vec![
                ok(
                    json!({"response": {"blog": {"name": "alice", "uuid": "t:alice"},
                    "posts": [photo_post(1, start + 100), photo_post(2, start + 50)]}}),
                ),
                ok(json!({"response": {"posts": [photo_post(2, start + 50),
                    photo_post(3, start - 10), photo_post(4, start - 20)]}})),
            ]);
            let events: Vec<_> = tumblr(root)
                .events(&Url::parse("https://alice.tumblr.com/day/2024/01/02").unwrap())
                .collect()
                .await;
            let media = files(&events);
            let ids: Vec<_> = media
                .iter()
                .map(|m| m.metadata.as_ref().unwrap().post_id.as_str())
                .collect();
            assert_eq!(ids, ["1", "2"]);
            let k = keywords(media[0]);
            assert_eq!(k["subcategory"], "day");
            // The posts response's blog is used without an info request, with
            // the listed blog's host name as its uuid (upstream).
            assert_eq!(
                k["blog"],
                json!({"name": "alice", "uuid": "alice.tumblr.com"})
            );
            let requests = server.join().unwrap();
            assert_eq!(requests.len(), 2);
            assert!(
                requests[0].contains("&limit=50&before=1704240000 "),
                "{}",
                requests[0]
            );
            assert!(!requests[0].contains("offset="));
            assert!(
                requests[1].contains("&limit=50&before=1704153651 "),
                "{}",
                requests[1]
            );
        }

        #[tokio::test]
        async fn api_pagination_follows_same_origin_links_with_the_key() {
            let (root, server) = serve(vec![
                ok(
                    json!({"response": {"posts": [photo_post(1, 10)], "total_posts": 1,
                    "_links": {"next": {"href":
                        "/v2/blog/alice.tumblr.com/posts?offset=55&api_key=leaked&npf=false"}}}}),
                ),
                ok(json!({"response": {"posts": [photo_post(2, 5)],
                    "_links": {"next": {"href": "//evil.example/v2/x"}}}})),
            ]);
            let native = tumblr(root)
                .with_api_key(Some("secret"))
                .with_selection(Selection::default().with_posts(["photo".into()]).unwrap())
                .with_options(Some(&options(json!({"pagination": "api", "offset": 5}))))
                .unwrap();
            let media: Vec<_> = native
                .stream(&Url::parse("https://alice.tumblr.com/").unwrap())
                .collect()
                .await;
            assert_eq!(media.len(), 2);
            let requests = server.join().unwrap();
            assert!(
                requests[0].starts_with(
                    "GET /v2/blog/alice.tumblr.com/posts?api_key=secret&npf=false&reblog_info=true&type=photo&offset=5&limit=50 "
                ),
                "{}",
                requests[0]
            );
            assert!(
                requests[1].starts_with(
                    "GET /v2/blog/alice.tumblr.com/posts?offset=55&npf=false&api_key=secret "
                ),
                "{}",
                requests[1]
            );
        }

        #[tokio::test]
        async fn offset_option_and_before_strategy_for_blog_posts() {
            let (root, server) = serve(vec![
                ok(json!({"response": {"posts": [photo_post(1, 100)], "total_posts": 9}})),
                ok(json!({"response": {"posts": [], "total_posts": 9}})),
            ]);
            let native = tumblr(root)
                .with_options(Some(&options(json!({"offset": 3}))))
                .unwrap();
            assert_eq!(
                native
                    .stream(&Url::parse("https://alice.tumblr.com/").unwrap())
                    .count()
                    .await,
                1
            );
            let requests = server.join().unwrap();
            assert!(requests[0].contains("&offset=3&limit=50 "));
            assert!(requests[1].contains("&offset=4&limit=50 "));
            let (root, server) = serve(vec![
                ok(json!({"response": {"posts": [photo_post(1, 100)]}})),
                ok(json!({"response": {"posts": []}})),
            ]);
            let native = tumblr(root)
                .with_options(Some(&options(json!({"pagination": "before"}))))
                .unwrap();
            assert_eq!(
                native
                    .stream(&Url::parse("https://alice.tumblr.com/").unwrap())
                    .count()
                    .await,
                1
            );
            let requests = server.join().unwrap();
            assert!(
                requests[0].contains("&reblog_info=true&limit=50 "),
                "{}",
                requests[0]
            );
            assert!(
                requests[1].contains("&limit=50&before=101 "),
                "{}",
                requests[1]
            );
        }

        #[tokio::test]
        async fn likes_page_by_liked_timestamp_and_fetch_blog_info() {
            let mut liked = photo_post(7, 50);
            liked["blog_name"] = json!("bob");
            liked["liked_timestamp"] = json!(500);
            let (root, server) = serve(vec![
                ok(json!({"response": {"liked_posts": [liked], "liked_count": 1}})),
                ok(json!({"response": {"blog": {"name": "alice", "title": "A"}}})),
                ok(json!({"response": {"liked_posts": []}})),
            ]);
            let events: Vec<_> = tumblr(root)
                .events(&Url::parse("https://www.tumblr.com/alice/likes").unwrap())
                .collect()
                .await;
            let media = files(&events);
            assert_eq!(media.len(), 1);
            let k = keywords(media[0]);
            assert_eq!(k["subcategory"], "likes");
            assert_eq!(
                k["blog"],
                json!({"name": "alice", "title": "A", "uuid": "alice.tumblr.com"})
            );
            assert_eq!(
                media[0].metadata.as_ref().unwrap().username.as_deref(),
                Some("bob")
            );
            assert_eq!(route_of(media[0]).archive, "f_{blog[name]}_{id}_{num}");
            let (_, dir) = crate::sites::default_templates(media[0]).unwrap();
            assert_eq!(
                dir.render(media[0]).unwrap().unwrap(),
                std::path::PathBuf::from("tumblr/bob/likes")
            );
            let requests = server.join().unwrap();
            assert!(requests[0].starts_with(&format!(
                "GET /v2/blog/alice.tumblr.com/likes?api_key={DEFAULT_API_KEY}&limit=50 "
            )));
            assert!(requests[1].starts_with("GET /v2/blog/alice.tumblr.com/info?api_key="));
            assert!(
                requests[2].contains("&limit=50&before=500 "),
                "{}",
                requests[2]
            );
        }

        #[tokio::test]
        async fn following_and_followers_queue_user_child_jobs() {
            let (root, server) = serve(vec![
                ok(json!({"response": {"total_blogs": 21, "blogs": [
                    {"name": "bob", "url": "https://bob.tumblr.com/", "updated": 1},
                    {"name": "not valid", "url": "https://evil.example/"}]}})),
                ok(json!({"response": {"posts": [photo_post(1, 10)], "total_posts": 1}})),
                ok(json!({"response": {"total_blogs": 21, "blogs": [{"name": "Carol"}]}})),
                ok(json!({"response": {"posts": [photo_post(2, 10)], "total_posts": 1}})),
            ]);
            let events: Vec<_> = tumblr(root)
                .events(&Url::parse("https://alice.tumblr.com/following").unwrap())
                .collect()
                .await;
            let scopes: Vec<_> = events
                .iter()
                .filter_map(|event| match event {
                    Ok(ExtractionEvent::BeginChild(scope)) => Some(&scope.metadata),
                    _ => None,
                })
                .collect();
            assert_eq!(scopes.len(), 2);
            assert_eq!(
                (scopes[0].category(), scopes[0].subcategory()),
                ("tumblr", "user")
            );
            assert_eq!(scopes[0].fields()["name"], "bob");
            assert_eq!(scopes[0].fields()["updated"], 1);
            assert_eq!(scopes[1].fields()["name"], "Carol");
            assert!(matches!(events[0], Ok(ExtractionEvent::BeginChild(_))));
            assert!(matches!(
                events.last(),
                Some(Ok(ExtractionEvent::EndChild(_)))
            ));
            let media = files(&events);
            assert_eq!(media.len(), 2);
            assert_eq!(keywords(media[0])["subcategory"], "user");
            let requests = server.join().unwrap();
            assert!(requests[0].starts_with("GET /v2/blog/alice.tumblr.com/following?api_key="));
            assert!(requests[0].contains("&limit=20&offset=0 "));
            assert!(requests[1].starts_with("GET /v2/blog/bob.tumblr.com/posts?"));
            assert!(requests[2].starts_with("GET /v2/blog/alice.tumblr.com/following?"));
            assert!(requests[2].contains("&limit=20&offset=20 "));
            assert!(requests[3].starts_with("GET /v2/blog/carol.tumblr.com/posts?"));
            // Followers use their own endpoint and stop at the total.
            let (root, server) = serve(vec![ok(
                json!({"response": {"total_blogs": 0, "blogs": []}}),
            )]);
            let events: Vec<_> = tumblr(root)
                .events(&Url::parse("https://www.tumblr.com/alice/followers").unwrap())
                .collect()
                .await;
            assert!(events.is_empty(), "{events:?}");
            assert!(
                server.join().unwrap()[0].starts_with("GET /v2/blog/alice.tumblr.com/followers?")
            );
        }

        #[tokio::test]
        async fn search_uses_timeline_pagination_and_post_blogs() {
            let mut found = photo_post(3, 10);
            found["blog_name"] = json!("carol");
            found["blog"] = json!({"name": "carol", "uuid": "t:carol"});
            let (root, server) = serve(vec![
                ok(json!({"response": {"timeline": {"elements": [found],
                    "_links": {"next": {"href": "/v2/timeline/search?query=cute+cats&cursor=abc"}}}}})),
                ok(json!({"response": {"timeline": {"elements": []}}})),
            ]);
            let native = tumblr(root)
                .with_selection(Selection {
                    reblogs: Reblogs::OriginalOnly,
                    ..Selection::default()
                })
                .with_options(Some(&options(
                    json!({"avatar": true, "pagination": "offset"}),
                )))
                .unwrap();
            let events: Vec<_> = native
                .events(
                    &Url::parse("https://www.tumblr.com/search/cute%20cats/recent/photo?t=365&src=typed&api_key=x")
                        .unwrap(),
                )
                .collect()
                .await;
            let media = files(&events);
            assert_eq!(media.len(), 1);
            assert_eq!(
                media[0].metadata.as_ref().unwrap().username.as_deref(),
                Some("carol")
            );
            let k = keywords(media[0]);
            assert_eq!(k["subcategory"], "search");
            assert_eq!(k["blog"]["uuid"], "t:carol");
            assert!(!k.contains_key("search_tags"));
            let requests = server.join().unwrap();
            assert!(
                requests[0].starts_with(
                    "GET /v2/timeline/search?api_key=O3hU2tMi5e4Qs5t3vezEi6L0qRORJ5y9oUpSGsrWu8iA3UCc3B&src=typed&limit=50&days=365&query=cute+cats&mode=recent&reblog_info=false&post_type_filter=photo "
                ),
                "{}",
                requests[0]
            );
            assert!(
                requests[1].starts_with(&format!(
                    "GET /v2/timeline/search?query=cute+cats&cursor=abc&api_key={DEFAULT_API_KEY} "
                )),
                "{}",
                requests[1]
            );
        }

        #[tokio::test]
        async fn avatar_is_resolved_and_emitted_before_the_first_post() {
            let (root, server) = serve(vec![
                ok(json!({"response": {"blog": {"name": "alice", "title": "A"},
                    "posts": [{"id": 1, "blog_name": "alice", "type": "text", "timestamp": 5},
                        photo_post(2, 4)],
                    "total_posts": 2}})),
                reply(
                    "302 Found",
                    &[("Location", "https://64.media.tumblr.com/avatar_abc_512.png")],
                    b"",
                ),
            ]);
            let native = tumblr(root)
                .with_api_key(Some("secret"))
                .with_options(Some(&options(json!({"avatar": true}))))
                .unwrap();
            let events: Vec<_> = native
                .events(&Url::parse("https://alice.tumblr.com/").unwrap())
                .collect()
                .await;
            assert!(matches!(events[0], Ok(ExtractionEvent::Post(_))));
            let media = files(&events);
            assert_eq!(media.len(), 2);
            let avatar = media[0];
            assert_eq!(
                avatar.url.as_str(),
                "https://64.media.tumblr.com/avatar_abc_512.png"
            );
            assert!(!avatar.url.as_str().contains("secret"));
            assert_eq!(avatar.filename, "avatar_abc_512.png");
            let k = keywords(avatar);
            for key in ["id", "type", "hash"] {
                assert_eq!(k[key], "avatar", "{key}");
            }
            assert_eq!((&k["num"], &k["count"]), (&json!(1), &json!(1)));
            assert_eq!(k["reblogged"], false);
            assert_eq!(k["blog"]["uuid"], "alice.tumblr.com");
            assert_eq!(k["extension"], "png");
            let (file, _) = crate::sites::default_templates(avatar).unwrap();
            assert_eq!(file.render(avatar).unwrap(), "tumblr_alice_avatar_01.png");
            let requests = server.join().unwrap();
            assert!(
                requests[1].starts_with("GET /v2/blog/alice.tumblr.com/avatar/512?api_key=secret ")
            );
            // An `avatar_url` response body and unsafe hosts.
            for (body, expected) in [
                (
                    json!({"response": {"avatar_url": "https://assets.tumblr.com/images/default_avatar/cube_open_512.png"}}),
                    Some("https://assets.tumblr.com/images/default_avatar/cube_open_512.png"),
                ),
                (
                    json!({"response": {"avatar_url": "https://evil.example/a.png"}}),
                    None,
                ),
                (
                    json!({"response": {"avatar_url": "http://64.media.tumblr.com/a.png"}}),
                    None,
                ),
                (
                    json!({"response": {"avatar_url": format!(
                        "https://64.media.tumblr.com/a.png?api_key={DEFAULT_API_KEY}"
                    )}}),
                    None,
                ),
            ] {
                let (root, server) = serve(vec![ok(body)]);
                let avatar = tumblr(root).avatar("alice").await;
                assert_eq!(avatar.ok().as_ref().map(Url::as_str), expected);
                server.join().unwrap();
            }
        }

        #[tokio::test]
        async fn external_links_are_queued_as_child_jobs() {
            let post = json!({"id": 9, "type": "link", "url": "https://example.com/page",
                "photos": [{"original_size": {"url": "https://64.media.tumblr.com/x.jpg"}}]});
            let other = json!({"id": 10, "type": "link", "url": "https://unsupported.example/"});
            let body = json!({"response": {"posts": [post, other], "total_posts": 2}});
            let (root, server) = serve(vec![ok(body.clone()), ok(body)]);
            let native = tumblr(root)
                .with_options(Some(&options(json!({"external": true}))))
                .unwrap();
            let queued = Arc::new(std::sync::Mutex::new(Vec::new()));
            let seen = queued.clone();
            let queue: crate::event::Queue = Arc::new(move |url: Url| {
                seen.lock().unwrap().push(url.to_string());
                (url.host_str() == Some("example.com")).then(|| stream::empty().boxed())
            });
            let url = Url::parse("https://alice.tumblr.com/").unwrap();
            let events: Vec<_> = native.events_with_queue(&url, queue).collect().await;
            let kinds: Vec<_> = events
                .iter()
                .map(|event| match event.as_ref().unwrap() {
                    ExtractionEvent::Post(_) => "post",
                    ExtractionEvent::Media(_) => "media",
                    ExtractionEvent::EndPost => "end",
                    ExtractionEvent::BeginChild(scope) => {
                        assert_eq!(scope.metadata.subcategory(), "user");
                        assert_eq!(scope.metadata.fields()["id"], 9);
                        "child"
                    }
                    ExtractionEvent::EndChild(_) => "endchild",
                })
                .collect();
            assert_eq!(kinds, ["post", "media", "end", "child", "endchild"]);
            assert_eq!(
                *queued.lock().unwrap(),
                ["https://example.com/page", "https://unsupported.example/"]
            );
            // Without a dispatcher, `events` skips external links.
            let events: Vec<_> = native.events(&url).collect().await;
            assert_eq!(events.len(), 3);
            server.join().unwrap();
        }

        #[tokio::test]
        async fn single_posts_report_missing_posts() {
            let (root, server) =
                serve(vec![ok(json!({"response": {"posts": [photo_post(2, 1)]}}))]);
            let events: Vec<_> = tumblr(root)
                .events(&Url::parse("https://alice.tumblr.com/image/1").unwrap())
                .collect()
                .await;
            assert!(
                events[0]
                    .as_ref()
                    .unwrap_err()
                    .to_string()
                    .contains("not found")
            );
            server.join().unwrap();
        }

        #[tokio::test]
        async fn rate_limits_abort_or_wait_as_configured() {
            let daily = || {
                reply(
                    "429 Too Many Requests",
                    &[
                        ("x-ratelimit-perday-remaining", "0"),
                        ("x-ratelimit-perday-reset", "0"),
                    ],
                    b"{}",
                )
            };
            let page = || ok(json!({"response": {"posts": [], "total_posts": 0}}));
            let url = Url::parse("https://alice.tumblr.com/").unwrap();
            let (root, server) = serve(vec![daily()]);
            let error = tumblr(root)
                .with_retries(3)
                .stream(&url)
                .collect::<Vec<_>>()
                .await
                .remove(0)
                .unwrap_err()
                .to_string();
            assert!(error.contains("daily API rate limit"), "{error}");
            assert_eq!(server.join().unwrap().len(), 1);
            let (root, server) = serve(vec![daily(), page()]);
            let native = tumblr(root)
                .with_options(Some(&options(json!({"ratelimit": "wait"}))))
                .unwrap();
            assert!(native.stream(&url).collect::<Vec<_>>().await.is_empty());
            assert_eq!(server.join().unwrap().len(), 2);
            // Hourly limits always wait, independent of the retry count.
            let hourly = reply(
                "429 Too Many Requests",
                &[("x-ratelimit-perhour-reset", "0")],
                b"{}",
            );
            let (root, server) = serve(vec![hourly, page()]);
            assert!(
                tumblr(root)
                    .stream(&url)
                    .collect::<Vec<_>>()
                    .await
                    .is_empty()
            );
            assert_eq!(server.join().unwrap().len(), 2);
            // Waits are bounded.
            let hourly = || {
                reply(
                    "429 Too Many Requests",
                    &[("x-ratelimit-perhour-reset", "0")],
                    b"{}",
                )
            };
            let (root, server) = serve((0..=MAX_RATE_WAITS).map(|_| hourly()).collect());
            let error = tumblr(root).stream(&url).collect::<Vec<_>>().await;
            assert!(
                error[0]
                    .as_ref()
                    .unwrap_err()
                    .to_string()
                    .contains("hourly")
            );
            assert_eq!(
                server.join().unwrap().len(),
                usize::from(MAX_RATE_WAITS) + 1
            );
            // A wait beyond the call deadline fails instead of sleeping.
            let (root, server) = serve(vec![reply(
                "429 Too Many Requests",
                &[("x-ratelimit-perhour-reset", "3600")],
                b"{}",
            )]);
            let error = tumblr(root).stream(&url).collect::<Vec<_>>().await;
            assert!(
                error[0]
                    .as_ref()
                    .unwrap_err()
                    .to_string()
                    .contains("wait limit")
            );
            assert_eq!(server.join().unwrap().len(), 1);
        }

        #[tokio::test]
        async fn original_lookups_retry_after_fallback_delay_and_update_keywords() {
            let token = "https://64.media.tumblr.com/new/token/s4096x4096/tumblr_big_1280.png";
            let html = format!("<img src=\"{token}\">");
            let (root, server) = serve(vec![
                response("404 Not Found", b""),
                response("200 OK", b"<p>no image</p>"),
                response("200 OK", html.as_bytes()),
            ]);
            let native = tumblr(root.clone())
                .with_options(Some(&options(
                    json!({"fallback-delay": 0, "fallback-retries": 2}),
                )))
                .unwrap();
            let mut capped = post(7);
            capped["photos"][0]["original_size"]["width"] = json!(2048);
            let mut media = parse_post(&capped, "alice").remove(0);
            let identity = media.archive_key();
            native
                .finish_original(&mut media, root.join("s99999x99999/one.jpg").unwrap())
                .await;
            assert_eq!(media.url.as_str(), token);
            assert_eq!(media.archive_key(), identity);
            assert_eq!(media.filename, "tumblr_big_1280.png");
            let k = keywords(&media);
            assert_eq!(k["photo"]["url"], token);
            assert_eq!(
                (&k["filename"], &k["extension"]),
                (&json!("tumblr_big_1280"), &json!("png"))
            );
            assert_eq!(k["hash"], "big");
            assert_eq!(server.join().unwrap().len(), 3);
            // Without retries, one failed lookup keeps the API rendition fallback.
            let (root, server) = serve(vec![response("404 Not Found", b"")]);
            let native = tumblr(root.clone())
                .with_options(Some(&options(
                    json!({"fallback-delay": 0, "fallback-retries": 0}),
                )))
                .unwrap();
            let mut media = parse_post(&capped, "alice").remove(0);
            native
                .finish_original(&mut media, root.join("s99999x99999/one.jpg").unwrap())
                .await;
            assert_eq!(media.handoff["_fallback"], json!([PHOTO]));
            assert_eq!(server.join().unwrap().len(), 1);
        }
    }

    /// Upstream `oauth.OAuth1Client` signing and `TumblrAPI._call` fallbacks.
    mod oauth {
        use super::*;

        fn options(value: Value) -> Map<String, Value> {
            value.as_object().unwrap().clone()
        }

        fn credentials() -> Map<String, Value> {
            json!({"api-secret": "consumer-secret", "access-token": "user-token",
                "access-token-secret": "tsecret-value"})
            .as_object()
            .unwrap()
            .clone()
        }

        fn header_field<'r>(request: &'r str, key: &str) -> &'r str {
            let start = request.find(&format!("{key}=\"")).unwrap() + key.len() + 2;
            let end = request[start..].find('"').unwrap();
            &request[start..start + end]
        }

        fn unquote(value: &str) -> String {
            percent_decode(value).unwrap()
        }

        #[tokio::test]
        async fn cached_token_pairs_sign_requests_unless_tokens_are_configured() {
            let directory = tempfile::tempdir().unwrap();
            let cache = crate::cache::Cache::at(directory.path().join("cache.sqlite3"));
            cache
                .set(
                    "gallery_dl.oauth._token_cache-('tumblr', 'consumer-key')",
                    &json!(["cached-token", "cached-secret"]),
                    None,
                )
                .await
                .unwrap();
            let body = json!({"response": {"posts": [post(123)]}}).to_string();
            let (root, server) = serve(vec![response("200 OK", body.as_bytes())]);
            let native = tumblr(root.clone())
                .with_options(Some(&options(json!({"api-secret": "consumer-secret",
                    "access-token": "cache", "access-token-secret": "ignored"}))))
                .unwrap()
                .with_api_key(Some("consumer-key"))
                .with_token_cache(Some(&cache))
                .await;
            let events: Vec<_> = native
                .events(&Url::parse("https://www.tumblr.com/alice/123").unwrap())
                .collect()
                .await;
            assert!(events.iter().all(Result::is_ok), "{events:?}");
            let request = &server.join().unwrap()[0];
            assert_eq!(header_field(request, "oauth_token"), "cached-token");
            let url = root
                .join("v2/blog/alice.tumblr.com/posts?npf=false&reblog_info=true&id=123")
                .unwrap();
            let params = [
                ("oauth_consumer_key", "consumer-key".to_owned()),
                (
                    "oauth_nonce",
                    header_field(request, "oauth_nonce").to_owned(),
                ),
                ("oauth_signature_method", "HMAC-SHA1".to_owned()),
                (
                    "oauth_timestamp",
                    header_field(request, "oauth_timestamp").to_owned(),
                ),
                ("oauth_version", "1.0".to_owned()),
                ("oauth_token", "cached-token".to_owned()),
            ];
            assert_eq!(
                unquote(header_field(request, "oauth_signature")),
                crate::oauth::signature("GET", &url, &params, "consumer-secret", "cached-secret")
            );

            // Configured tokens win; another API key has no cached pair.
            let configured = tumblr(root.clone())
                .with_options(Some(&credentials()))
                .unwrap()
                .with_api_key(Some("consumer-key"))
                .with_token_cache(Some(&cache))
                .await;
            assert_eq!(
                configured.options.access_token.as_deref(),
                Some("user-token")
            );
            let other = tumblr(root)
                .with_api_key(Some("other-key"))
                .with_token_cache(Some(&cache))
                .await;
            assert!(other.oauth().is_none());
        }

        #[tokio::test]
        async fn configured_tokens_sign_api_requests_instead_of_sending_api_key() {
            let body = json!({"response": {"posts": [post(123)]}}).to_string();
            let (root, server) = serve(vec![response("200 OK", body.as_bytes())]);
            let events: Vec<_> = tumblr(root.clone())
                .with_options(Some(&credentials()))
                .unwrap()
                .with_api_key(Some("consumer-key"))
                .events(&Url::parse("https://www.tumblr.com/alice/123").unwrap())
                .collect()
                .await;
            assert!(events.iter().all(Result::is_ok), "{events:?}");
            let request = &server.join().unwrap()[0];
            let line = request.lines().next().unwrap();
            assert_eq!(
                line,
                "GET /v2/blog/alice.tumblr.com/posts?npf=false&reblog_info=true&id=123 HTTP/1.1"
            );
            assert!(!request.contains("consumer-secret") && !request.contains("tsecret-value"));
            assert!(request.contains("authorization: OAuth oauth_consumer_key=\"consumer-key\","));
            assert_eq!(header_field(request, "oauth_token"), "user-token");
            assert_eq!(header_field(request, "oauth_signature_method"), "HMAC-SHA1");
            assert_eq!(header_field(request, "oauth_version"), "1.0");
            // The signature verifies over the request actually sent.
            let url = root
                .join("v2/blog/alice.tumblr.com/posts?npf=false&reblog_info=true&id=123")
                .unwrap();
            let params = [
                ("oauth_consumer_key", "consumer-key".to_owned()),
                (
                    "oauth_nonce",
                    header_field(request, "oauth_nonce").to_owned(),
                ),
                ("oauth_signature_method", "HMAC-SHA1".to_owned()),
                (
                    "oauth_timestamp",
                    header_field(request, "oauth_timestamp").to_owned(),
                ),
                ("oauth_version", "1.0".to_owned()),
                ("oauth_token", "user-token".to_owned()),
            ];
            assert_eq!(
                unquote(header_field(request, "oauth_signature")),
                crate::oauth::signature("GET", &url, &params, "consumer-secret", "tsecret-value")
            );
        }

        #[tokio::test]
        async fn incomplete_or_disabled_credentials_keep_api_key_authentication() {
            for settings in [
                json!({"access-token": "user-token"}),
                json!({"access-token-secret": "tsecret-value"}),
                json!({"api-secret": "", "access-token": "t", "access-token-secret": "s"}),
                json!({"access-token": "cache", "access-token-secret": "s"}),
                json!({"access-token": false, "access-token-secret": "s"}),
            ] {
                let body = json!({"response": {"posts": [post(123)]}}).to_string();
                let (root, server) = serve(vec![response("200 OK", body.as_bytes())]);
                let _: Vec<_> = tumblr(root)
                    .with_options(Some(&options(settings.clone())))
                    .unwrap()
                    .events(&Url::parse("https://www.tumblr.com/alice/123").unwrap())
                    .collect()
                    .await;
                let request = &server.join().unwrap()[0];
                assert!(
                    request.contains(&format!("api_key={DEFAULT_API_KEY}")),
                    "{settings}"
                );
                assert!(!request.contains("authorization:"), "{settings}");
            }
            // The default application secret signs when only tokens are given.
            let body = json!({"response": {"posts": [post(123)]}}).to_string();
            let (root, server) = serve(vec![response("200 OK", body.as_bytes())]);
            let _: Vec<_> = tumblr(root)
                .with_options(Some(&options(
                    json!({"access-token": "t", "access-token-secret": "s"}),
                )))
                .unwrap()
                .events(&Url::parse("https://www.tumblr.com/alice/123").unwrap())
                .collect()
                .await;
            let request = &server.join().unwrap()[0];
            assert!(!request.contains("api_key="));
            assert_eq!(header_field(request, "oauth_consumer_key"), DEFAULT_API_KEY);
            for bad in [json!(1), json!(true), json!("bad value"), json!(["x"])] {
                let error = Tumblr::new(Duration::from_secs(5))
                    .unwrap()
                    .with_options(Some(&options(json!({"access-token-secret": bad}))))
                    .err()
                    .unwrap()
                    .to_string();
                assert!(error.contains("access-token-secret") && !error.contains("bad value"));
            }
        }

        #[tokio::test]
        async fn refused_oauth_blogs_retry_once_with_api_key() {
            let refused = json!({"meta": {"status": 403, "msg": "Forbidden"},
                "response": NO_PERMISSION})
            .to_string();
            let body = json!({"response": {"posts": [post(123)]}}).to_string();
            let (root, server) = serve(vec![
                response("403 Forbidden", refused.as_bytes()),
                response("200 OK", body.as_bytes()),
                response("200 OK", body.as_bytes()),
            ]);
            let native = tumblr(root)
                .with_options(Some(&credentials()))
                .unwrap()
                .with_api_key(Some("consumer-key"));
            for _ in 0..2 {
                let events: Vec<_> = native
                    .events(&Url::parse("https://www.tumblr.com/alice/123").unwrap())
                    .collect()
                    .await;
                assert!(events.iter().all(Result::is_ok), "{events:?}");
            }
            let requests = server.join().unwrap();
            assert!(requests[0].contains("authorization: OAuth "));
            assert!(!requests[0].contains("api_key="));
            for request in &requests[1..] {
                assert!(request.contains("api_key=consumer-key"), "{request}");
                assert!(!request.contains("authorization:"), "{request}");
            }
        }

        #[tokio::test]
        async fn dashboard_only_blogs_report_fixed_authorization_hints() {
            let dashboard = json!({"meta": {"status": 404}, "errors": [{"title": "Not Found",
                "detail": "This Tumblr is only viewable within the Tumblr dashboard"}]})
            .to_string();
            for oauth in [false, true] {
                let (root, server) = serve(vec![response("404 Not Found", dashboard.as_bytes())]);
                let mut native = tumblr(root).with_retries(2);
                if oauth {
                    native = native.with_options(Some(&credentials())).unwrap();
                }
                let error = native
                    .events(&Url::parse("https://alice.tumblr.com/").unwrap())
                    .collect::<Vec<_>>()
                    .await
                    .remove(0)
                    .unwrap_err()
                    .to_string();
                assert!(error.contains("only viewable within the Tumblr dashboard"));
                assert_eq!(error.contains("same application"), oauth, "{error}");
                assert!(!error.contains("tsecret-value") && !error.contains("user-token"));
                assert_eq!(server.join().unwrap().len(), 1);
            }
            // Other refusals remain terminal and are not retried with api_key.
            let (root, server) = serve(vec![response("403 Forbidden", b"{\"response\":\"x\"}")]);
            let error = tumblr(root)
                .with_options(Some(&credentials()))
                .unwrap()
                .events(&Url::parse("https://alice.tumblr.com/").unwrap())
                .collect::<Vec<_>>()
                .await
                .remove(0)
                .unwrap_err()
                .to_string();
            assert!(error.contains("access-token-secret settings"), "{error}");
            assert_eq!(server.join().unwrap().len(), 1);
        }
    }
}
