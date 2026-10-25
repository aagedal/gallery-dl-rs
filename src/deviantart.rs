//! Experimental native DeviantArt extraction.
//!
//! Mirrors gallery_dl/extractor/deviantart.py. API requests use the OAuth2
//! API with an access token from the client-credentials grant (upstream's
//! public client unless `client-id`/`client-secret` are configured), or a
//! private token from the `refresh-token` grant where upstream switches to
//! one. Tokens are cached for the run, refreshed once on HTTP 401, never
//! included in errors and never sent with media requests. Without a
//! configured `refresh-token` (or with `"cache"`), one stored by
//! `oauth:deviantart` in the credential cache is used; DeviantArt rotates
//! refresh tokens, so each new one is written back to the cache (keyed by the
//! configured token, as upstream does) and keeps working on later runs. Scraps, gallery
//! searches and search results go through the web ("Eclipse") endpoints with
//! a CSRF token from the front page, as upstream does when not logged in.
//!
//! Every file carries upstream's keyword dictionary and its extractor class
//! (`deviation`, `gallery`, `folder`, `scraps`, `favorite`, `collection`,
//! `journal`, `status`, `avatar`, `background`, `tag`, `search`,
//! `gallery-search`, `stash`, `watch`, `watch-posts`, and `user`/`following`
//! dispatching; the watch feeds need a refresh token), so default paths
//! follow upstream's `filename_fmt`/`directory_fmt`. Archive identity is the
//! deviation (or status) UUID, plus the file kind for videos, flash files and
//! journal text; avatars use the user name and icon index.
use crate::{
    Result, error,
    event::{ChildMetadata, ExtractionEvent, child_job, group},
    extract::{Media, MediaJob, PostMetadata},
    file_metadata::FileMetadata,
    network::NetworkOptions,
    sites::{Route, SiteKeywords},
    transport::{RateLimits, retry_after},
};
use futures_util::{
    StreamExt,
    stream::{self, BoxStream},
};
use reqwest::{Client, RequestBuilder, StatusCode};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    future::Future,
    sync::Arc,
    time::{Duration, SystemTime},
};
use url::Url;

/// Upstream gallery-dl's public OAuth client.
pub(crate) const CLIENT_ID: &str = "5388";
pub(crate) const CLIENT_SECRET: &str = "76b08c69cfb27f26d6161f9ab6d061a1";
const USER_AGENT: &str = "gallery-dl-rs";
/// Canonical site origin used in keywords and journal markup.
const ROOT: &str = "https://www.deviantart.com";
const DEFAULT_AVATAR: &str = "https://a.deviantart.net/avatars/default.gif";
const API_VERSION: &str = "20210526";
const MAX_RESPONSE: usize = 16 * 1024 * 1024;
const MAX_PAGES: usize = 200;
const MAX_STASH: usize = 1000;
/// Upstream `_limited_request`: one web page request every two seconds.
const PAGE_INTERVAL: Duration = Duration::from_secs(2);
/// Upstream only tries `/intermediary/` URLs for deviations up to this index.
const INTERMEDIARY_MAX_INDEX: i64 = 790677560;

type Events<'a> = BoxStream<'a, Result<ExtractionEvent>>;
/// Folder list kind (`gallery`/`collections`) and lowercase user name.
type FolderKey = (&'static str, String);

const FILENAME: &str = "{category}_{index}_{title}.{extension}";
const GALLERY_ARCHIVE: &str = "g_{_username}_{index}.{extension}";
const USER_DIR: &[&str] = &["{category}", "{username}"];
static USER: Route = Route::new("user", FILENAME, USER_DIR, "");
static GALLERY: Route = Route::new("gallery", FILENAME, USER_DIR, GALLERY_ARCHIVE);
static DEVIATION: Route = Route::new("deviation", FILENAME, USER_DIR, GALLERY_ARCHIVE);
static GALLERY_SEARCH: Route = Route::new("gallery-search", FILENAME, USER_DIR, GALLERY_ARCHIVE);
static AVATAR: Route = Route::new("avatar", FILENAME, USER_DIR, "a_{_username}_{index}");
static BACKGROUND: Route = Route::new("background", FILENAME, USER_DIR, "b_{index}");
static STASH: Route = Route::new("stash", FILENAME, USER_DIR, "{index}.{extension}");
static FOLDER: Route = Route::new(
    "folder",
    FILENAME,
    &["{category}", "{username}", "{folder[title]}"],
    "F_{folder[uuid]}_{index}.{extension}",
);
/// A subfolder reached from its parent folder, with upstream's `flat` layout.
static SUBFOLDER_FLAT: Route = Route::new(
    "folder",
    FILENAME,
    &["{category}", "{username}", "{folder[parent_folder]}"],
    "F_{folder[parent_uuid]}_{index}.{extension}",
);
static SUBFOLDER: Route = Route::new(
    "folder",
    FILENAME,
    &[
        "{category}",
        "{username}",
        "{folder[parent_folder]}",
        "{folder[title]}",
    ],
    "F_{folder[parent_uuid]}_{index}.{extension}",
);
static SCRAPS: Route = Route::new(
    "scraps",
    FILENAME,
    &["{category}", "{username}", "Scraps"],
    "s_{_username}_{index}.{extension}",
);
static FAVORITE: Route = Route::new(
    "favorite",
    FILENAME,
    &["{category}", "{username}", "Favourites"],
    "f_{_username}_{index}.{extension}",
);
static COLLECTION: Route = Route::new(
    "collection",
    FILENAME,
    &[
        "{category}",
        "{username}",
        "Favourites",
        "{collection[title]}",
    ],
    "C_{collection[uuid]}_{index}.{extension}",
);
static JOURNAL: Route = Route::new(
    "journal",
    FILENAME,
    &["{category}", "{username}", "Journal"],
    "j_{_username}_{index}.{extension}",
);
static STATUS: Route = Route::new(
    "status",
    "{category}_{index}_{title}_{date}.{extension}",
    &["{category}", "{username}", "Status"],
    "S_{_username}_{index}.{extension}",
);
static TAG: Route = Route::new(
    "tag",
    FILENAME,
    &["{category}", "Tags", "{search_tags}"],
    "T_{search_tags}_{index}.{extension}",
);
static SEARCH: Route = Route::new(
    "search",
    FILENAME,
    &["{category}", "Search", "{search_tags}"],
    "Q_{search_tags}_{index}.{extension}",
);
/// Deviations and posts of watched users; upstream sets no `archive_fmt`.
static WATCH: Route = Route::new("watch", FILENAME, USER_DIR, "");
static WATCH_POSTS: Route = Route::new("watch-posts", FILENAME, USER_DIR, "");

/// Upstream extractor classes supported natively, for `--list-extractors`.
/// `following` queues watched users; `user` dispatches profile children.
pub const SUBCATEGORIES: &[&str] = &[
    "avatar",
    "background",
    "collection",
    "deviation",
    "favorite",
    "folder",
    "following",
    "gallery",
    "gallery-search",
    "journal",
    "scraps",
    "search",
    "stash",
    "status",
    "tag",
    "user",
    "watch",
    "watch-posts",
];

/// Upstream extractor class for a subcategory name, for tests and tooling.
pub fn route(subcategory: &str) -> Option<&'static Route> {
    [
        &USER,
        &GALLERY,
        &DEVIATION,
        &GALLERY_SEARCH,
        &AVATAR,
        &BACKGROUND,
        &STASH,
        &FOLDER,
        &SCRAPS,
        &FAVORITE,
        &COLLECTION,
        &JOURNAL,
        &STATUS,
        &TAG,
        &SEARCH,
        &WATCH,
        &WATCH_POSTS,
    ]
    .into_iter()
    .find(|route| route.subcategory == subcategory)
}

/// Canonical 8-4-4-4-12 hexadecimal UUID.
pub fn valid_uuid(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

fn valid_user(user: &str) -> bool {
    (1..=32).contains(&user.len())
        && user.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !user.starts_with('-')
}

fn digits(text: &str) -> bool {
    (1..=20).contains(&text.len()) && text.bytes().all(|b| b.is_ascii_digit())
}

/// Top-level `www.deviantart.com` paths that are not user names.
const RESERVED: &[&str] = &[
    "about",
    "art",
    "browse",
    "core-membership",
    "daily-deviations",
    "developers",
    "deviation",
    "groups",
    "join",
    "notifications",
    "popular",
    "search",
    "settings",
    "shop",
    "stash",
    "status",
    "tag",
    "team",
    "topic",
    "users",
    "view",
    "watch",
];

/// The user-profile subcategories upstream's `include` option selects.
const INCLUDE: &[&str] = &[
    "avatar",
    "background",
    "gallery",
    "scraps",
    "journal",
    "status",
    "favorite",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// Bare profile URL; dispatches to the `include` subcategories.
    User(String),
    Gallery(String),
    Folder {
        user: String,
        id: String,
        name: Option<String>,
    },
    Scraps(String),
    GallerySearch {
        user: String,
        query: String,
    },
    Favorite(String),
    Collection {
        user: String,
        id: String,
        name: String,
    },
    Journal(String),
    Status(String),
    Avatar(String),
    Background(String),
    /// `USER/art|journal/SLUG-ID`, or a bare numeric ID without a user.
    Deviation {
        user: Option<String>,
        kind: &'static str,
        id: String,
    },
    Tag(String),
    /// Raw search query string (`q=...&...`).
    Search(String),
    Stash(String),
    /// `USER/about#watching`: the users USER watches, queued as profiles.
    Following(String),
    /// The authenticated account's watch feed (`/watch/deviations`).
    Watch,
    /// Posts (journals) from watched users (`/watch/posts`).
    WatchPosts,
}

/// Whether a URL is a DeviantArt page handled by this extractor.
pub fn is_deviantart_url(url: &Url) -> bool {
    target(url).is_ok()
}

pub fn target(url: &Url) -> Result<Target> {
    let unsupported = || {
        error(
            "unsupported DeviantArt URL; expected a deviation, user profile, gallery, folder, scraps, favourites, collection, posts, avatar, banner, tag, search or stash URL",
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
    let segments: Vec<&str> = url
        .path_segments()
        .map(|s| s.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    let query = url.query().unwrap_or_default();
    match host {
        "fav.me" => match segments.as_slice() {
            [code] if code.len() > 1 && code.starts_with('d') => {
                let id = b36decode(&code[1..]).ok_or_else(unsupported)?;
                Ok(Target::Deviation {
                    user: None,
                    kind: "art",
                    id: id.to_string(),
                })
            }
            _ => Err(unsupported()),
        },
        "sta.sh" => match segments.as_slice() {
            [id] if stash_id(id) => Ok(Target::Stash(id.to_string())),
            _ => Err(unsupported()),
        },
        "www.deviantart.com" | "deviantart.com" | "www.fxdeviantart.com" | "fxdeviantart.com" => {
            let fx = host.contains("fxdeviantart");
            match segments.as_slice() {
                ["tag", tag, ..] if !fx => {
                    let tag = crate::sites::percent_decode(tag);
                    if tag.is_empty() {
                        Err(unsupported())
                    } else {
                        Ok(Target::Tag(tag))
                    }
                }
                ["search"] | ["search", "deviations"] if !fx && !query.is_empty() => {
                    Ok(Target::Search(query.to_owned()))
                }
                ["stash", id, ..] if !fx && stash_id(id) => Ok(Target::Stash(id.to_string())),
                ["watch", "deviations", ..] | ["notifications", "watch", ..] if !fx => {
                    Ok(Target::Watch)
                }
                ["watch", "posts", ..] if !fx => Ok(Target::WatchPosts),
                ["view" | "deviation", id, ..] if digits(id) => Ok(Target::Deviation {
                    user: None,
                    kind: "art",
                    id: id.to_string(),
                }),
                ["view.php" | "view-full.php"] => {
                    let id = url
                        .query_pairs()
                        .find(|(key, _)| key == "id")
                        .map(|(_, id)| id.into_owned())
                        .filter(|id| digits(id))
                        .ok_or_else(unsupported)?;
                    Ok(Target::Deviation {
                        user: None,
                        kind: "art",
                        id,
                    })
                }
                [user, rest @ ..] if !RESERVED.contains(&user.to_ascii_lowercase().as_str()) => {
                    user_target(user, rest, query, url.fragment()).ok_or_else(unsupported)
                }
                _ => Err(unsupported()),
            }
        }
        _ => {
            // Legacy USER.deviantart.com hosts.
            let user = host
                .strip_suffix(".deviantart.com")
                .or_else(|| host.strip_suffix(".fxdeviantart.com"))
                .filter(|user| !user.contains('.') && *user != "www")
                .ok_or_else(unsupported)?;
            user_target(user, &segments, query, url.fragment()).ok_or_else(unsupported)
        }
    }
}

fn stash_id(id: &str) -> bool {
    (1..=32).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// Upstream's per-user URL patterns; the user name is lowercased.
fn user_target(user: &str, rest: &[&str], query: &str, fragment: Option<&str>) -> Option<Target> {
    if !valid_user(user) {
        return None;
    }
    let user = user.to_ascii_lowercase();
    let folder_id = |id: &str| valid_uuid(id) || digits(id);
    Some(match rest {
        [] => Target::User(user),
        ["gallery"] if query.starts_with("q=") => Target::GallerySearch {
            user,
            query: query.to_owned(),
        },
        ["gallery"] | ["gallery", "all" | "recommended-for-you"]
            if query.starts_with("catpath=scraps") =>
        {
            Target::Scraps(user)
        }
        ["gallery"] | ["gallery", "all" | "recommended-for-you"] => Target::Gallery(user),
        ["gallery", "scraps"] => Target::Scraps(user),
        ["gallery", id, name, ..] if folder_id(id) => Target::Folder {
            user,
            id: id.to_string(),
            name: Some(name.to_string()),
        },
        ["gallery", id] if valid_uuid(id) => Target::Folder {
            user,
            id: id.to_string(),
            name: None,
        },
        ["avatar", ..] => Target::Avatar(user),
        // Upstream `/(?:about#)?watching`.
        ["watching", ..] => Target::Following(user),
        ["about"] if fragment.is_some_and(|f| f.starts_with("watching")) => Target::Following(user),
        ["banner" | "background", ..] => Target::Background(user),
        ["favourites"] | ["favourites", "all"] => Target::Favorite(user),
        ["favourites", id, name, ..] if folder_id(id) => Target::Collection {
            user,
            id: id.to_string(),
            name: name.to_string(),
        },
        ["posts"] | ["posts", "journals"] | ["journal"] => Target::Journal(user),
        ["posts", "statuses", ..] => Target::Status(user),
        [kind @ ("art" | "journal"), slug, ..] => Target::Deviation {
            user: Some(user),
            kind: if *kind == "art" { "art" } else { "journal" },
            id: slug_id(slug)?.to_owned(),
        },
        _ => return None,
    })
}

/// Upstream `(?:[^/?#"]+-)?(\d+)`: digits after the last `-` that is
/// followed by one, else a leading run of digits.
fn slug_id(slug: &str) -> Option<&str> {
    fn take(s: &str) -> Option<&str> {
        let end = s
            .bytes()
            .position(|b| !b.is_ascii_digit())
            .unwrap_or(s.len());
        (1..=20).contains(&end).then(|| &s[..end])
    }
    slug.match_indices('-')
        .rev()
        .find_map(|(at, _)| take(&slug[at + 1..]))
        .or_else(|| take(slug))
}

fn b36decode(text: &str) -> Option<u64> {
    if text.is_empty() {
        return Some(0);
    }
    u64::from_str_radix(text, 36).ok()
}

fn b36encode(mut num: u64) -> String {
    const ALPHABET: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    while num > 0 {
        out.push(ALPHABET[(num % 36) as usize]);
        num /= 36;
    }
    out.reverse();
    String::from_utf8(out).expect("ASCII")
}

/// Find the deviation UUID in a deviation page's app link or state.
pub fn page_uuid(html: &str) -> Option<String> {
    const MARKERS: &[&str] = &[
        "\"deviationUuid\\\":\\\"",
        "DeviantArt://deviation/",
        "\"deviationUuid\":\"",
    ];
    MARKERS.iter().find_map(|marker| {
        html.match_indices(marker).find_map(|(at, _)| {
            let id = html.get(at + marker.len()..at + marker.len() + 36)?;
            valid_uuid(id).then(|| id.to_owned())
        })
    })
}

/// Durable archive identity for a native DeviantArt file: the deviation or
/// status UUID (unchanged from earlier releases for primary files), the UUID
/// and file kind for secondary files, or the user and icon index for avatars.
pub fn archive_identity(post: &PostMetadata) -> Option<String> {
    if let Some(index) = post.media_id.strip_prefix("avatar:") {
        return (valid_user(&post.post_id) && digits(index))
            .then(|| json!(["avatar", post.post_id, index]).to_string());
    }
    if !valid_uuid(&post.post_id) {
        return None;
    }
    if post.media_id == post.post_id {
        return Some(json!([post.post_id]).to_string());
    }
    let kind = post
        .media_id
        .strip_prefix(post.post_id.as_str())?
        .strip_prefix(':')?;
    matches!(kind, "video" | "flash" | "journal").then(|| json!([post.post_id, kind]).to_string())
}

fn string<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn host_allowed(host: &str) -> bool {
    host == "wixmp.com"
        || host.ends_with(".wixmp.com")
        || host == "deviantart.net"
        || host.ends_with(".deviantart.net")
}

/// HTTPS files on `*.wixmp.com` or `*.deviantart.net` only.
fn media_url(raw: &str) -> Option<Url> {
    let url = Url::parse(raw.trim()).ok()?;
    (url.scheme() == "https"
        && url.host_str().is_some_and(host_allowed)
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none())
    .then_some(url)
}

/// gallery-dl `text.ext_from_url`.
fn ext_from_url(raw: &str) -> String {
    let path = raw.split(['?', '#']).next().unwrap_or_default();
    match path.rsplit('/').next().unwrap_or_default().rsplit_once('.') {
        Some((name, ext)) if !name.is_empty() => ext.to_lowercase(),
        _ => String::new(),
    }
}

/// gallery-dl `text.parse_int` for JSON strings and numbers.
fn parse_int(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

/// gallery-dl `text.parse_int` for strings.
fn parse_int_text(text: &str, default: i64) -> i64 {
    let text = text.trim();
    let digits = text.strip_prefix(['+', '-']).unwrap_or(text);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return default;
    }
    text.parse().unwrap_or(default)
}

/// Python truthiness of a JSON value.
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

/// gallery-dl `text.extract_iter`.
fn extract_iter(text: &str, begin: &str, end: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(begin) {
        let after = &rest[start + begin.len()..];
        let Some(stop) = after.find(end) else {
            break;
        };
        found.push(after[..stop].to_owned());
        rest = &after[stop + end.len()..];
    }
    found
}

/// gallery-dl `text.parse_query`: first value per name, in order.
fn parse_query(qs: &str) -> Vec<(String, String)> {
    let mut result: Vec<(String, String)> = Vec::new();
    for (name, value) in url::form_urlencoded::parse(qs.as_bytes()) {
        if !result.iter().any(|(n, _)| *n == name) {
            result.push((name.into_owned(), value.into_owned()));
        }
    }
    result
}

/// Python `re.sub(r"\W", "_", text)`.
fn word_chars(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
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

/// Replace the first match of `prefix\d+` (with at least one digit).
fn replace_digits(text: &str, prefix: &str, with: &str) -> String {
    for (at, _) in text.match_indices(prefix) {
        let rest = &text[at + prefix.len()..];
        let count = rest.bytes().take_while(u8::is_ascii_digit).count();
        if count > 0 {
            return format!("{}{with}{}", &text[..at], &rest[count..]);
        }
    }
    text.to_owned()
}

/// Upstream `-fullview\.[a-z0-9]+\?` → `-fullview.png?` (first match).
fn png_quality(text: &str) -> String {
    const NEEDLE: &str = "-fullview.";
    for (at, _) in text.match_indices(NEEDLE) {
        let rest = &text[at + NEEDLE.len()..];
        let count = rest
            .bytes()
            .take_while(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
            .count();
        if count > 0 && rest[count..].starts_with('?') {
            return format!("{}-fullview.png?{}", &text[..at], &rest[count + 1..]);
        }
    }
    text.to_owned()
}

/// Upstream `(/f/[^/]+/[^/]+)/v\d+/.*` → `/intermediary\1` (first match).
fn intermediary(text: &str) -> Option<String> {
    for (at, _) in text.match_indices("/f/") {
        let rest = &text[at + 3..];
        let mut parts = rest.splitn(3, '/');
        let (Some(a), Some(b), Some(tail)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        if a.is_empty() || b.is_empty() || a.contains('\n') || b.contains('\n') {
            continue;
        }
        let Some(version) = tail.strip_prefix('v') else {
            continue;
        };
        let count = version.bytes().take_while(u8::is_ascii_digit).count();
        if count == 0 || !version[count..].starts_with('/') {
            continue;
        }
        let end = version[count..]
            .find('\n')
            .map(|i| at + 3 + a.len() + b.len() + 3 + count + i)
            .unwrap_or(text.len());
        return Some(format!(
            "{}/intermediary/f/{a}/{b}{}",
            &text[..at],
            &text[end..]
        ));
    }
    None
}

/// Upstream `_find_folder` name matching: `(?i)NAME$` with each `-`
/// replaced by `[^a-z0-9]+`, anchored at the start.
fn folder_name_matches(pattern: &str, name: &str) -> bool {
    fn rest_matches(parts: &[String], text: &str) -> bool {
        let Some((first, rest)) = parts.split_first() else {
            return text.is_empty();
        };
        let Some(after) = strip_prefix_ci(text, first) else {
            return false;
        };
        if rest.is_empty() {
            return after.is_empty() || after == "\n";
        }
        for (i, c) in after.char_indices() {
            if c.is_ascii_alphanumeric() {
                break;
            }
            if rest_matches(rest, &after[i + c.len_utf8()..]) {
                return true;
            }
        }
        false
    }
    fn strip_prefix_ci<'t>(text: &'t str, prefix: &str) -> Option<&'t str> {
        let mut chars = text.char_indices();
        for p in prefix.chars() {
            let (_, c) = chars.next()?;
            if !c.to_lowercase().eq(p.to_lowercase()) {
                return None;
            }
        }
        Some(chars.next().map_or("", |(i, _)| &text[i..]))
    }
    let parts: Vec<String> = pattern.split('-').map(str::to_owned).collect();
    rest_matches(&parts, name)
}

/// Upstream `_find_folder`: a folder (or subfolder) by numeric-ID name
/// pattern, else by UUID, with its parent folder name for subfolders.
fn find_folder<'f>(
    folders: &'f [Value],
    name: Option<&str>,
    id: &str,
) -> Option<(&'f Value, Option<&'f str>)> {
    let wanted = |folder: &Value| {
        if digits(id) {
            name.is_some_and(|name| {
                string(folder, "name").is_some_and(|n| folder_name_matches(name, n))
            })
        } else {
            folder.get("folderid").and_then(Value::as_str) == Some(id)
        }
    };
    for folder in folders {
        if wanted(folder) {
            return Some((folder, None));
        }
        if folder.get("has_subfolders").and_then(Value::as_bool) == Some(true) {
            for sub in folder
                .get("subfolders")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if wanted(sub) {
                    return Some((sub, folder.get("name").and_then(Value::as_str)));
                }
            }
        }
    }
    None
}

fn json(bytes: &[u8]) -> Result<Value> {
    serde_json::from_slice(bytes).map_err(|_| error("DeviantArt returned invalid JSON"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Journals {
    Html,
    Text,
    Off,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Original {
    Off,
    On,
    Images,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Group {
    Off,
    On,
    Skip,
}

/// Upstream extractor settings (`extractor.deviantart.*`).
#[derive(Clone, Debug)]
struct Options {
    include: Vec<&'static str>,
    journals: Journals,
    original: Original,
    /// `,q_N` replacement, `-fullview.png?` for `"png"`, or disabled.
    quality: Option<String>,
    intermediary: bool,
    mature: bool,
    /// Extended `metadata` categories; `Some([])` for base metadata.
    metadata: Option<Vec<String>>,
    flat: bool,
    public: bool,
    group: Group,
    subfolders: bool,
    avatar_formats: Option<Vec<String>>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            include: vec!["gallery"],
            journals: Journals::Html,
            original: Original::Off,
            quality: Some(",q_100".into()),
            intermediary: true,
            mature: true,
            metadata: None,
            flat: true,
            public: true,
            group: Group::On,
            subfolders: true,
            avatar_formats: None,
        }
    }
}

fn option_error(key: &str, expected: &str) -> crate::Error {
    error(format!("extractor.deviantart.{key} must be {expected}"))
}

fn flag(options: &Map<String, Value>, key: &str, default: bool) -> Result<bool> {
    match options.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(option_error(key, "a boolean")),
    }
}

/// A comma-separated string or a list of strings.
fn names(value: &Value, key: &str) -> Result<Vec<String>> {
    match value {
        Value::String(s) => Ok(s
            .replace(' ', "")
            .split(',')
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect()),
        Value::Array(items) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| option_error(key, "a string or a list of strings"))
            })
            .collect(),
        _ => Err(option_error(key, "a string or a list of strings")),
    }
}

impl Options {
    fn parse(options: &Map<String, Value>) -> Result<Self> {
        let mut parsed = Self::default();
        if let Some(value) = options.get("include").filter(|v| !v.is_null()) {
            parsed.include = match value {
                Value::String(s) if s == "all" => INCLUDE.to_vec(),
                value => names(value, "include")?
                    .iter()
                    .filter_map(|name| INCLUDE.iter().copied().find(|sub| sub == name))
                    .collect(),
            };
        }
        parsed.journals = match options.get("journals") {
            None | Some(Value::Null) => Journals::Html,
            Some(Value::String(s)) if s == "html" => Journals::Html,
            Some(Value::String(s)) if s == "text" => Journals::Text,
            Some(Value::String(_) | Value::Bool(false)) => Journals::Off,
            Some(_) => return Err(option_error("journals", "\"html\", \"text\" or \"none\"")),
        };
        parsed.original = match options.get("original") {
            None | Some(Value::Null) | Some(Value::Bool(false)) => Original::Off,
            Some(Value::Bool(true)) => Original::On,
            Some(Value::String(s)) if s.to_lowercase().starts_with("image") => Original::Images,
            Some(Value::String(s)) if s.is_empty() => Original::Off,
            Some(Value::String(_)) => Original::On,
            Some(_) => return Err(option_error("original", "a boolean or \"images\"")),
        };
        parsed.quality = match options.get("quality") {
            None => Some(",q_100".into()),
            Some(Value::Null) | Some(Value::Bool(false)) => None,
            Some(Value::Number(n)) if n.as_u64() == Some(0) => None,
            Some(Value::Number(n)) if n.as_u64().is_some() => Some(format!(",q_{n}")),
            Some(Value::String(s)) if s.is_empty() => None,
            Some(Value::String(s)) if s == "png" => Some("-fullview.png?".into()),
            Some(Value::String(s)) if s.len() <= 8 && s.bytes().all(|b| b.is_ascii_digit()) => {
                Some(format!(",q_{s}"))
            }
            Some(_) => return Err(option_error("quality", "an integer or \"png\"")),
        };
        parsed.intermediary = flag(options, "intermediary", true)?;
        parsed.mature = match options.get("mature") {
            Some(Value::String(s)) => s != "false",
            _ => flag(options, "mature", true)?,
        };
        parsed.metadata = match options.get("metadata") {
            None | Some(Value::Null) | Some(Value::Bool(false)) => None,
            Some(Value::Bool(true)) => Some(Vec::new()),
            Some(Value::String(s)) if s.is_empty() => None,
            Some(Value::String(s)) if s == "all" => Some(
                ["submission", "camera", "stats", "collection", "gallery"]
                    .map(String::from)
                    .to_vec(),
            ),
            Some(value) => {
                let list = names(value, "metadata")?;
                if !list.iter().all(|n| {
                    (1..=32).contains(&n.len()) && n.bytes().all(|b| b.is_ascii_alphanumeric())
                }) {
                    return Err(option_error("metadata", "a list of metadata categories"));
                }
                Some(list)
            }
        };
        parsed.flat = flag(options, "flat", true)?;
        parsed.public = flag(options, "public", true)?;
        parsed.group = match options.get("group") {
            Some(Value::String(s)) if s == "skip" => Group::Skip,
            _ => {
                if flag(options, "group", true)? {
                    Group::On
                } else {
                    Group::Off
                }
            }
        };
        match options.get("folder") {
            None | Some(Value::Null) => {}
            Some(Value::Object(folder)) => parsed.subfolders = flag(folder, "subfolders", true)?,
            Some(_) => return Err(option_error("folder", "an object")),
        }
        match options.get("avatar") {
            None | Some(Value::Null) => {}
            Some(Value::Object(avatar)) => {
                if let Some(value) = avatar.get("formats").filter(|v| !v.is_null()) {
                    let formats = names(value, "avatar.formats")?;
                    if !formats.iter().all(|f| {
                        f.len() <= 32
                            && f.bytes()
                                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'))
                    }) {
                        return Err(option_error("avatar.formats", "a list of SIZE.EXT formats"));
                    }
                    parsed.avatar_formats = Some(formats).filter(|f| !f.is_empty());
                }
            }
            Some(_) => return Err(option_error("avatar", "an object")),
        }
        Ok(parsed)
    }

    /// Upstream `DeviantartOAuthAPI.limit`: metadata batch size.
    fn metadata_limit(&self) -> Option<u64> {
        self.metadata
            .as_ref()
            .map(|ext| if ext.is_empty() { 50 } else { 10 })
    }
}

/// The extractor class and context of one listing.
struct Context {
    route: &'static Route,
    /// Upstream `self.user`: the canonical profile name, if any.
    user: Option<String>,
    /// Class-specific keywords (`folder`, `collection`, `search_tags`).
    extra: Map<String, Value>,
}

impl Context {
    fn new(route: &'static Route, user: Option<String>) -> Arc<Self> {
        Arc::new(Self {
            route,
            user,
            extra: Map::new(),
        })
    }

    fn with(route: &'static Route, user: Option<String>, key: &str, value: Value) -> Arc<Self> {
        let mut extra = Map::new();
        extra.insert(key.into(), value);
        Arc::new(Self { route, user, extra })
    }
}

/// One paginated source of deviation objects.
enum Pager {
    OAuth(OAuthPager),
    Eclipse(EclipsePager),
    Search {
        params: Vec<(String, String)>,
        cursor: Option<String>,
        pages: usize,
    },
    Stash {
        queue: VecDeque<(String, Option<Arc<Value>>)>,
        seen: HashSet<String>,
    },
    Items(Vec<Raw>),
    Done,
}

struct OAuthPager {
    path: String,
    params: Vec<(&'static str, String)>,
    limit: u64,
    offset: Option<u64>,
    cursor: Option<String>,
    public: Option<bool>,
    extend: bool,
    /// Keep only posts whose URL contains this (journals, statuses).
    filter: Option<&'static str>,
    /// Upstream `unpack`: each result's `journal` object (watched posts).
    unpack: bool,
    /// Results are `user/friends` entries, queued as profile children.
    friends: bool,
    pages: usize,
}

struct EclipsePager {
    path: &'static str,
    params: Vec<(&'static str, String)>,
    key: &'static str,
    offset: Option<u64>,
    cursor: Option<String>,
    pages: usize,
}

enum Raw {
    Deviation(Value),
    Status(Value),
    /// Eclipse search/scraps result: numeric ID, author and type.
    Eclipse {
        id: String,
        user: String,
        journal: bool,
    },
    /// A folder or collection queued as a child job.
    Child {
        target: Target,
        subcategory: &'static str,
        data: Map<String, Value>,
        parent: Option<String>,
    },
}

pub struct DeviantArt {
    client: Client,
    network: NetworkOptions,
    root: Url,
    client_id: String,
    client_secret: String,
    /// Current refresh token; DeviantArt may rotate it on every refresh.
    refresh_token: std::sync::Mutex<Option<String>>,
    has_refresh: bool,
    /// The credential cache entry receiving rotated refresh tokens.
    token_cache: Option<(crate::cache::Cache, String)>,
    access_token: tokio::sync::Mutex<Option<String>>,
    private_token: tokio::sync::Mutex<Option<String>>,
    options: Options,
    timeout: Duration,
    request_timeout: Duration,
    retries: u8,
    rate_limits: RateLimits,
    page_interval: Duration,
    last_page: tokio::sync::Mutex<Option<tokio::time::Instant>>,
    /// Session cookies set by web pages, sent back to web (Eclipse) requests.
    cookies: std::sync::Mutex<BTreeMap<String, String>>,
    csrf: tokio::sync::Mutex<Option<String>>,
    profiles: tokio::sync::Mutex<HashMap<String, Option<Arc<Value>>>>,
    folders: tokio::sync::Mutex<HashMap<FolderKey, Arc<Vec<Value>>>>,
}

/// OAuth tokens are bounded printable ASCII.
fn valid_token(token: &str) -> bool {
    !token.is_empty() && token.len() <= 4096 && token.bytes().all(|b| b.is_ascii_graphic())
}

fn fail<'a>(e: crate::Error) -> Events<'a> {
    stream::once(async move { Err(e) }).boxed()
}

/// Run async setup, then continue with the stream it produced.
fn deferred<'a>(setup: impl Future<Output = Result<Events<'a>>> + Send + 'a) -> Events<'a> {
    stream::once(setup)
        .map(|result| match result {
            Ok(events) => events,
            Err(e) => fail(e),
        })
        .flatten()
        .boxed()
}

impl DeviantArt {
    pub fn new(timeout: Duration) -> Result<Self> {
        Ok(Self {
            client: NetworkOptions::default().client(USER_AGENT, timeout)?,
            network: NetworkOptions::default(),
            root: Url::parse("https://www.deviantart.com/").expect("static URL"),
            client_id: CLIENT_ID.into(),
            client_secret: CLIENT_SECRET.into(),
            refresh_token: std::sync::Mutex::new(None),
            has_refresh: false,
            token_cache: None,
            access_token: tokio::sync::Mutex::new(None),
            private_token: tokio::sync::Mutex::new(None),
            options: Options::default(),
            timeout,
            request_timeout: timeout,
            retries: 0,
            rate_limits: RateLimits::default(),
            page_interval: PAGE_INTERVAL,
            last_page: tokio::sync::Mutex::new(None),
            cookies: std::sync::Mutex::new(BTreeMap::new()),
            csrf: tokio::sync::Mutex::new(None),
            profiles: tokio::sync::Mutex::new(HashMap::new()),
            folders: tokio::sync::Mutex::new(HashMap::new()),
        })
    }

    /// Apply proxy and user-agent settings to API requests and emitted media.
    pub fn with_network(mut self, network: &NetworkOptions) -> Result<Self> {
        self.client = network.client(USER_AGENT, self.timeout)?;
        self.network = network.clone();
        Ok(self)
    }

    /// Override upstream's public OAuth client.
    pub fn with_credentials(mut self, id: Option<&str>, secret: Option<&str>) -> Self {
        if let Some(id) = id {
            self.client_id = id.into();
        }
        if let Some(secret) = secret {
            self.client_secret = secret.into();
        }
        self
    }

    /// Upstream `refresh-token`: grants a private access token where upstream
    /// uses one. Unset or `"cache"` defers to [`Self::with_token_cache`].
    pub fn with_refresh_token(mut self, token: Option<&str>) -> Self {
        let token = token.filter(|t| !t.is_empty() && *t != "cache");
        self.has_refresh = token.is_some();
        self.refresh_token = std::sync::Mutex::new(token.map(str::to_owned));
        self
    }

    /// Upstream `_refresh_token_cache`: the entry is keyed `#CLIENT_ID`
    /// (filled by `oauth:deviantart`) without a configured token, else by the
    /// configured token. A cached token takes precedence, and every token
    /// the refresh grant returns is stored there. Call after the client and
    /// refresh-token settings; unreadable caches count as empty.
    pub async fn with_token_cache(mut self, cache: Option<&crate::cache::Cache>) -> Self {
        let Some(cache) = cache else {
            return self;
        };
        let configured = self
            .refresh_token
            .lock()
            .expect("refresh token lock")
            .clone();
        let argument = configured.unwrap_or_else(|| format!("#{}", self.client_id));
        let key = crate::cache::key("extractor.deviantart", "_refresh_token_cache", &argument);
        if let Ok(Some(token)) = cache.get_str(&key).await
            && valid_token(&token)
        {
            self.has_refresh = true;
            *self.refresh_token.lock().expect("refresh token lock") = Some(token);
        }
        self.token_cache = Some((cache.clone(), key));
        self
    }

    /// Apply upstream extractor settings (`extractor.deviantart.*`).
    pub fn with_options(mut self, options: Option<&Map<String, Value>>) -> Result<Self> {
        if let Some(options) = options {
            self.options = Options::parse(options)?;
        }
        Ok(self)
    }

    pub fn with_retries(mut self, retries: u8) -> Self {
        self.retries = retries;
        self
    }

    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// Override the site, OAuth and API origin, without the web request
    /// interval; intended for local tests.
    pub fn with_root(mut self, root: Url) -> Self {
        self.root = root;
        self.page_interval = Duration::ZERO;
        self
    }

    fn api_bare(&self, path: &str) -> Url {
        self.root
            .join("api/v1/oauth2/")
            .and_then(|base| base.join(path))
            .expect("static path")
    }

    fn api(&self, path: &str) -> Url {
        let mut url = self.api_bare(path);
        url.query_pairs_mut()
            .append_pair("mature_content", self.mature());
        url
    }

    fn mature(&self) -> &'static str {
        if self.options.mature { "true" } else { "false" }
    }

    /// A same-path URL on the configured origin for a canonical page URL.
    fn site_url(&self, raw: &str) -> Option<Url> {
        let url = Url::parse(raw).ok()?;
        if url.scheme() != "https"
            || !matches!(
                url.host_str(),
                Some("www.deviantart.com" | "deviantart.com")
            )
            || url.port().is_some()
            || !url.username().is_empty()
        {
            return None;
        }
        let mut local = self.root.join(url.path()).ok()?;
        local.set_query(url.query());
        Some(local)
    }

    async fn authenticate(&self, public: bool) -> Result<String> {
        let endpoint = self.root.join("oauth2/token").expect("static path");
        let refresh = if public {
            None
        } else {
            Some(
                self.refresh_token
                    .lock()
                    .expect("refresh token lock")
                    .clone()
                    .ok_or_else(|| error("DeviantArt private access requires a refresh-token"))?,
            )
        };
        let body = {
            let mut form = url::form_urlencoded::Serializer::new(String::new());
            form.append_pair(
                "grant_type",
                if public {
                    "client_credentials"
                } else {
                    "refresh_token"
                },
            )
            .append_pair("client_id", &self.client_id)
            .append_pair("client_secret", &self.client_secret);
            if let Some(token) = &refresh {
                form.append_pair("refresh_token", token);
            }
            form.finish()
        };
        let reply = self
            .request(&endpoint, || {
                self.client
                    .post(endpoint.clone())
                    .timeout(self.request_timeout)
                    .header("Content-Type", "application/x-www-form-urlencoded")
                    .body(body.clone())
            })
            .await?;
        match reply {
            Reply::Ok(body) => {
                let data = json(&body)?;
                let token = |key: &str| {
                    data.get(key)
                        .and_then(Value::as_str)
                        .filter(|t| valid_token(t))
                        .map(str::to_owned)
                };
                if refresh.is_some()
                    && let Some(rotated) = token("refresh_token")
                {
                    // The previous token may already be invalid: persist the
                    // new one so later runs can still refresh.
                    if let Some((cache, key)) = &self.token_cache
                        && cache
                            .set(key, &Value::from(rotated.as_str()), None)
                            .await
                            .is_err()
                    {
                        eprintln!(
                            "could not store the rotated DeviantArt refresh token in the cache"
                        );
                    }
                    *self.refresh_token.lock().expect("refresh token lock") = Some(rotated);
                }
                token("access_token")
                    .ok_or_else(|| error("DeviantArt OAuth response contains no access token"))
            }
            Reply::Status(status) if status.is_client_error() && refresh.is_some() => Err(error(
                "DeviantArt rejected the refresh-token; it may have expired or been rotated",
            )),
            Reply::Status(status) if status.is_client_error() => Err(error(
                "DeviantArt rejected the OAuth client credentials; check client-id and client-secret",
            )),
            Reply::Status(status) => Err(error(format!("DeviantArt OAuth returned HTTP {status}"))),
        }
    }

    async fn token(&self, public: bool, stale: Option<&str>) -> Result<String> {
        let mut cached = if public {
            self.access_token.lock().await
        } else {
            self.private_token.lock().await
        };
        if let Some(token) = cached.as_deref()
            && Some(token) != stale
        {
            return Ok(token.to_owned());
        }
        let token = self.authenticate(public).await?;
        *cached = Some(token.clone());
        Ok(token)
    }

    /// Whether a call uses the public token; upstream's `public` argument,
    /// defaulting to the `public` option, applies only with a refresh token.
    fn is_public(&self, public: Option<bool>) -> bool {
        !self.has_refresh || public.unwrap_or(self.options.public)
    }

    /// Authorized API GET, refreshing the access token once on HTTP 401.
    async fn call(&self, endpoint: &Url, public: Option<bool>) -> Result<Value> {
        let public = self.is_public(public);
        tokio::time::timeout(self.timeout, async {
            let mut token = self.token(public, None).await?;
            for refreshed in [false, true] {
                let reply = self
                    .request(endpoint, || {
                        self.client
                            .get(endpoint.clone())
                            .timeout(self.request_timeout)
                            .header("dA-minor-version", API_VERSION)
                            .bearer_auth(&token)
                    })
                    .await?;
                match reply {
                    Reply::Ok(body) => return json(&body),
                    Reply::Status(StatusCode::UNAUTHORIZED) if !refreshed => {
                        token = self.token(public, Some(&token)).await?;
                    }
                    Reply::Status(status) => return Err(status_error(status)),
                }
            }
            Err(error("DeviantArt refused the refreshed access token"))
        })
        .await
        .map_err(|_| error("native DeviantArt extraction timed out"))?
    }

    /// GET a web page (or Eclipse endpoint) with session cookies, at most
    /// one every two seconds like upstream's `_limited_request`.
    async fn page(&self, url: &Url) -> Result<String> {
        {
            let mut last = self.last_page.lock().await;
            if let Some(at) = *last {
                tokio::time::sleep_until(at + self.page_interval).await;
            }
            *last = Some(tokio::time::Instant::now());
        }
        let cookie = {
            let cookies = self.cookies.lock().expect("cookie lock");
            cookies
                .iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect::<Vec<_>>()
                .join("; ")
        };
        let reply = tokio::time::timeout(
            self.timeout,
            self.request(url, || {
                let request = self.client.get(url.clone()).timeout(self.request_timeout);
                if cookie.is_empty() {
                    request
                } else {
                    request.header("Cookie", &cookie)
                }
            }),
        )
        .await
        .map_err(|_| error("native DeviantArt extraction timed out"))??;
        match reply {
            Reply::Ok(body) => Ok(String::from_utf8_lossy(&body).into_owned()),
            Reply::Status(status) => Err(status_error(status)),
        }
    }

    async fn csrf_token(&self) -> Result<String> {
        let mut cached = self.csrf.lock().await;
        if let Some(token) = cached.as_deref() {
            return Ok(token.to_owned());
        }
        let page = self.page(&self.root).await?;
        let token = crate::sites::extr(&page, "window.__CSRF_TOKEN__ = '", "'")
            .filter(|t| !t.is_empty() && t.len() <= 256)
            .ok_or_else(|| error("DeviantArt front page contains no CSRF token"))?
            .to_owned();
        *cached = Some(token.clone());
        Ok(token)
    }

    /// Upstream `DeviantartEclipseAPI._call`.
    async fn eclipse(&self, path: &str, params: &[(&str, String)]) -> Result<Value> {
        let csrf = self.csrf_token().await?;
        let mut url = self.root.join(path).expect("static path");
        {
            let mut query = url.query_pairs_mut();
            for (key, value) in params {
                query.append_pair(key, value);
            }
            query.append_pair("csrf_token", &csrf);
        }
        let page = self.page(&url).await?;
        Ok(serde_json::from_str(&page).unwrap_or_else(|_| json!({"error": "invalid JSON"})))
    }

    /// Upstream `deviation_extended_fetch` → `deviationUuid` → `deviation`.
    async fn eclipse_deviation(&self, id: &str, user: &str, journal: bool) -> Result<Value> {
        let data = self
            .eclipse(
                "_puppy/dadeviation/init",
                &[
                    ("deviationid", id.to_owned()),
                    ("username", user.to_owned()),
                    ("type", if journal { "journal" } else { "art" }.to_owned()),
                    ("include_session", "false".into()),
                    ("expand", "deviation.related".into()),
                    ("da_minor_version", "20230710".into()),
                ],
            )
            .await?;
        let uuid = data
            .pointer("/deviation/extended/deviationUuid")
            .and_then(Value::as_str)
            .filter(|id| valid_uuid(id))
            .ok_or_else(|| error("DeviantArt deviation details contain no deviation UUID"))?;
        self.deviation(uuid, None).await
    }

    /// Upstream `DeviantartOAuthAPI.deviation`.
    async fn deviation(&self, uuid: &str, public: Option<bool>) -> Result<Value> {
        let endpoint = self.api(&format!("deviation/{uuid}"));
        let mut deviation = self.call(&endpoint, public).await?;
        if deviation.get("is_mature").and_then(Value::as_bool) == Some(true)
            && public.is_none()
            && self.has_refresh
            && self.is_public(None)
        {
            deviation = self.call(&endpoint, Some(false)).await?;
        }
        self.add_metadata(std::slice::from_mut(&mut deviation))
            .await?;
        Ok(deviation)
    }

    /// Upstream `deviation_download`: public first, then private if a
    /// refresh token is available.
    async fn download(&self, uuid: &str, public: Option<bool>) -> Result<Value> {
        let endpoint = self.api(&format!("deviation/download/{uuid}"));
        match self.call(&endpoint, public).await {
            Err(_) if self.has_refresh && self.is_public(public) => {
                self.call(&endpoint, Some(false)).await
            }
            result => result,
        }
    }

    /// Upstream `_user_details`: the cached `user/profile` response, or
    /// `None` when it fails (groups and unknown users).
    async fn profile(&self, name: &str) -> Option<Arc<Value>> {
        let name = name.to_ascii_lowercase();
        if let Some(cached) = self.profiles.lock().await.get(&name) {
            return cached.clone();
        }
        let profile = self
            .call(&self.api(&format!("user/profile/{name}")), None)
            .await
            .ok()
            .filter(|p| p.get("user").is_some_and(Value::is_object))
            .map(Arc::new);
        self.profiles.lock().await.insert(name, profile.clone());
        profile
    }

    /// Upstream `_metadata`: merge `deviation/metadata` into deviations.
    async fn add_metadata(&self, deviations: &mut [Value]) -> Result<()> {
        let (Some(ext), Some(limit)) = (&self.options.metadata, self.options.metadata_limit())
        else {
            return Ok(());
        };
        let private = ext.iter().any(|e| e == "collection" || e == "gallery") && self.has_refresh;
        for chunk in deviations.chunks_mut(limit as usize) {
            let ids: Vec<String> = chunk
                .iter()
                .filter_map(|d| d.get("deviationid").and_then(Value::as_str))
                .map(str::to_owned)
                .collect();
            if ids.len() != chunk.len() {
                continue;
            }
            let mut endpoint = self.api_bare("deviation/metadata");
            {
                let mut query = endpoint.query_pairs_mut();
                for (num, id) in ids.iter().enumerate() {
                    query.append_pair(&format!("deviationids[{num}]"), id);
                }
                query.append_pair("mature_content", self.mature());
                for name in ext {
                    query.append_pair(&format!("ext_{name}"), "1");
                }
            }
            let data = self
                .call(&endpoint, if private { Some(false) } else { None })
                .await?;
            let list = data
                .get("metadata")
                .and_then(Value::as_array)
                .ok_or_else(|| error("DeviantArt metadata response contains no metadata"))?;
            for (deviation, metadata) in chunk.iter_mut().zip(list) {
                if let (Some(deviation), Some(metadata)) =
                    (deviation.as_object_mut(), metadata.as_object())
                {
                    for (key, value) in metadata {
                        deviation.insert(key.clone(), value.clone());
                    }
                    if let Some(tags) = deviation.get("tags").and_then(Value::as_array) {
                        let names: Vec<Value> = tags
                            .iter()
                            .filter_map(|t| t.get("tag_name").cloned())
                            .collect();
                        deviation.insert("tags".into(), names.into());
                    }
                }
            }
        }
        Ok(())
    }

    /// Upstream `gallery_folders`/`collections_folders`, cached per run.
    async fn folder_list(&self, kind: &'static str, user: &str) -> Result<Arc<Vec<Value>>> {
        let key = (kind, user.to_ascii_lowercase());
        if let Some(cached) = self.folders.lock().await.get(&key) {
            return Ok(cached.clone());
        }
        let mut pager = OAuthPager {
            path: format!("{kind}/folders"),
            params: vec![("username", user.to_owned())],
            limit: 50,
            offset: Some(0),
            cursor: None,
            public: None,
            extend: false,
            filter: None,
            unpack: false,
            friends: false,
            pages: 0,
        };
        let mut folders = Vec::new();
        while let Some(results) = self.oauth_page(&mut pager).await? {
            folders.extend(results);
        }
        let folders = Arc::new(folders);
        self.folders.lock().await.insert(key, folders.clone());
        Ok(folders)
    }

    /// One upstream `_pagination` request; `None` once exhausted.
    async fn oauth_page(&self, pager: &mut OAuthPager) -> Result<Option<Vec<Value>>> {
        if pager.pages >= MAX_PAGES {
            return Ok(None);
        }
        loop {
            let mut endpoint = self.api(&pager.path);
            {
                let mut query = endpoint.query_pairs_mut();
                for (key, value) in &pager.params {
                    query.append_pair(key, value);
                }
                if let Some(offset) = pager.offset {
                    query.append_pair("offset", &offset.to_string());
                }
                query.append_pair("limit", &pager.limit.to_string());
                if let Some(cursor) = &pager.cursor {
                    query.append_pair("cursor", cursor);
                }
            }
            let data = self.call(&endpoint, pager.public).await?;
            let mut results = data
                .get("results")
                .and_then(Value::as_array)
                .cloned()
                .ok_or_else(|| error("DeviantArt response contains no results"))?;
            if pager.unpack {
                results = results
                    .into_iter()
                    .filter_map(|item| item.get("journal").cloned())
                    .collect();
            }
            if pager.extend {
                if self.has_refresh
                    && self.is_public(pager.public)
                    && ((results.len() as u64) < pager.limit
                        || results
                            .iter()
                            .any(|r| r.get("is_mature").and_then(Value::as_bool) == Some(true)))
                {
                    // Upstream `_should_switch_tokens`: repeat with the private token.
                    pager.public = Some(false);
                    continue;
                }
                if results
                    .first()
                    .is_some_and(|r| r.get("deviationid").is_some())
                {
                    self.add_metadata(&mut results).await?;
                }
            }
            pager.pages += 1;
            let has_more = data.get("has_more").and_then(Value::as_bool) == Some(true);
            let previous = pager.offset;
            if !has_more || pager.pages >= MAX_PAGES {
                pager.pages = MAX_PAGES;
            } else if let Some(cursor) = data.get("next_cursor") {
                match cursor.as_str().filter(|c| !c.is_empty() && c.len() <= 1024) {
                    Some(cursor) if pager.cursor.as_deref() != Some(cursor) => {
                        pager.offset = None;
                        pager.cursor = Some(cursor.to_owned());
                    }
                    _ => pager.pages = MAX_PAGES,
                }
            } else if let Some(next) = data.get("next_offset").and_then(Value::as_u64) {
                pager.cursor = None;
                pager.offset = Some(next);
            } else if let Some(offset) = previous {
                pager.offset = Some(offset + results.len() as u64);
            } else {
                pager.pages = MAX_PAGES;
            }
            if let (Some(before), Some(after)) = (previous, pager.offset)
                && after <= before
            {
                pager.pages = MAX_PAGES;
            }
            if let Some(filter) = pager.filter {
                results.retain(|r| string(r, "url").is_some_and(|u| u.contains(filter)));
            }
            return Ok(Some(results));
        }
    }

    /// One upstream `DeviantartEclipseAPI._pagination` request.
    async fn eclipse_page(&self, pager: &mut EclipsePager) -> Result<Option<Vec<Value>>> {
        if pager.pages >= MAX_PAGES {
            return Ok(None);
        }
        let mut params = pager.params.clone();
        if let Some(offset) = pager.offset {
            params.push(("offset", offset.to_string()));
        }
        if let Some(cursor) = &pager.cursor {
            params.push(("cursor", cursor.clone()));
        }
        params.push(("limit", "24".into()));
        let data = self.eclipse(pager.path, &params).await?;
        pager.pages += 1;
        let Some(results) = data.get(pager.key).and_then(Value::as_array).cloned() else {
            pager.pages = MAX_PAGES;
            return Ok(Some(Vec::new()));
        };
        let previous = pager.offset;
        if data.get("hasMore").and_then(Value::as_bool) != Some(true) {
            pager.pages = MAX_PAGES;
        } else if let Some(cursor) = data.get("nextCursor") {
            match cursor.as_str().filter(|c| !c.is_empty() && c.len() <= 1024) {
                Some(cursor) if pager.cursor.as_deref() != Some(cursor) => {
                    pager.offset = None;
                    pager.cursor = Some(cursor.to_owned());
                }
                _ => pager.pages = MAX_PAGES,
            }
        } else if let Some(next) = data.get("nextOffset").and_then(Value::as_u64) {
            pager.offset = Some(next);
            pager.cursor = None;
        } else if let Some(offset) = previous {
            pager.offset = Some(offset + results.len() as u64);
        } else {
            pager.pages = MAX_PAGES;
        }
        if let (Some(before), Some(after)) = (previous, pager.offset)
            && after <= before
        {
            pager.pages = MAX_PAGES;
        }
        Ok(Some(results))
    }

    /// Upstream `_search_html`: one logged-out search results page.
    async fn search_page(
        &self,
        params: &[(String, String)],
        cursor: &mut Option<String>,
    ) -> Result<Vec<Raw>> {
        let mut url = self.root.join("search").expect("static path");
        {
            let mut query = url.query_pairs_mut();
            for (key, value) in params {
                if key != "cursor" {
                    query.append_pair(key, value);
                }
            }
            if let Some(cursor) = cursor.as_deref() {
                query.append_pair("cursor", cursor);
            }
        }
        let page = self.page(&url).await?;
        let found = search_links(&page);
        let keep = found.len().saturating_sub(3);
        let items = found
            .into_iter()
            .take(keep)
            .step_by(3)
            .map(|(user, journal, id)| Raw::Eclipse { id, user, journal })
            .collect();
        let next = crate::sites::extr(&page, r#"\"cursor\":\""#, "\\")
            .filter(|c| !c.is_empty() && c.len() <= 1024)
            .map(str::to_owned);
        *cursor = match next {
            Some(next) if cursor.as_deref() != Some(next.as_str()) => Some(next),
            _ => None,
        };
        Ok(items)
    }

    /// One stash page: a single deviation, or the stash items it links.
    async fn stash_page(
        &self,
        id: &str,
        stash_data: Option<Arc<Value>>,
        queue: &mut VecDeque<(String, Option<Arc<Value>>)>,
        seen: &mut HashSet<String>,
    ) -> Result<Vec<Raw>> {
        let url = self.root.join(&format!("stash/{id}")).expect("stash path");
        let page = self.page(&url).await?;
        if id.starts_with('0')
            && let Some(uuid) =
                crate::sites::extr(&page, "//deviation/", "\"").filter(|u| valid_uuid(u))
        {
            let mut deviation = self.deviation(uuid, None).await?;
            let index = crate::sites::extr(&page, "\\\"deviationId\\\":", ",")
                .and_then(|n| n.trim().parse::<i64>().ok())
                .unwrap_or(0);
            if let Some(object) = deviation.as_object_mut() {
                object.insert("_page".into(), page.into());
                object.insert("index".into(), index.into());
                object.insert("stash_id".into(), id.into());
                let folder = stash_data.as_deref().and_then(|d| d.get("folder"));
                match folder {
                    Some(folder) => {
                        object.insert(
                            "stash_name".into(),
                            folder.get("name").cloned().unwrap_or(Value::Null),
                        );
                        object.insert(
                            "stash_folder".into(),
                            folder.get("folderId").cloned().unwrap_or(Value::Null),
                        );
                        let parent = folder
                            .get("parentId")
                            .filter(|p| truthy(p))
                            .cloned()
                            .unwrap_or_else(|| 0.into());
                        object.insert("stash_parent".into(), parent);
                        object.insert(
                            "stash_description".into(),
                            folder
                                .pointer("/richDescription/excerpt")
                                .cloned()
                                .unwrap_or(Value::Null),
                        );
                    }
                    None => {
                        object.insert("stash_name".into(), "".into());
                        object.insert("stash_description".into(), "".into());
                        object.insert("stash_folder".into(), 0.into());
                        object.insert("stash_parent".into(), 0.into());
                    }
                }
            }
            return Ok(vec![Raw::Deviation(deviation)]);
        }
        let data = crate::sites::extr(&page, ",\\\"stash\\\":", ",\\\"@@")
            .and_then(|raw| {
                let raw = if raw.ends_with(":{}") {
                    raw.get(..raw[..raw.len() - 2].rfind('}').map_or(0, |i| i + 1))
                        .unwrap_or_default()
                } else {
                    raw
                };
                serde_json::from_str::<Value>(&unescape_json(raw)).ok()
            })
            .map(Arc::new)
            .or(stash_data);
        for sid in extract_iter(&page, "href=\"https://www.deviantart.com/stash/", "\"") {
            if sid == id || sid.ends_with("#comments") || !stash_id(&sid) {
                continue;
            }
            if seen.len() < MAX_STASH && seen.insert(sid.clone()) {
                queue.push_back((sid, data.clone()));
            }
        }
        Ok(Vec::new())
    }

    /// The next batch of raw items from a pager; `None` once exhausted.
    async fn next_batch(&self, pager: &mut Pager) -> Result<Option<Vec<Raw>>> {
        match pager {
            Pager::Done => Ok(None),
            Pager::Items(items) => {
                let items = std::mem::take(items);
                *pager = Pager::Done;
                Ok(Some(items))
            }
            Pager::OAuth(oauth) => {
                let statuses = oauth.filter == Some("/status-update/");
                let friends = oauth.friends;
                Ok(self.oauth_page(oauth).await?.map(|results| {
                    results
                        .into_iter()
                        .filter_map(|r| {
                            Some(if friends {
                                friend_child(r)?
                            } else if statuses {
                                Raw::Status(r)
                            } else {
                                Raw::Deviation(r)
                            })
                        })
                        .collect()
                }))
            }
            Pager::Eclipse(eclipse) => Ok(self.eclipse_page(eclipse).await?.map(|results| {
                results
                    .iter()
                    .filter_map(|item| {
                        let deviation = item.get("deviation").unwrap_or(item);
                        let id = match deviation.get("deviationId")? {
                            Value::Number(n) => n.as_u64()?.to_string(),
                            Value::String(s) if digits(s) => s.clone(),
                            _ => return None,
                        };
                        let user = deviation.pointer("/author/username")?.as_str()?;
                        valid_user(user).then(|| Raw::Eclipse {
                            id,
                            user: user.to_owned(),
                            journal: deviation.get("isJournal").and_then(Value::as_bool)
                                == Some(true),
                        })
                    })
                    .collect()
            })),
            Pager::Search {
                params,
                cursor,
                pages,
            } => {
                let items = self.search_page(params, cursor).await?;
                *pages += 1;
                if cursor.is_none() || *pages >= MAX_PAGES {
                    *pager = Pager::Items(items);
                    return Ok(Some(Vec::new()));
                }
                Ok(Some(items))
            }
            Pager::Stash { queue, seen } => {
                let Some((id, data)) = queue.pop_front() else {
                    *pager = Pager::Done;
                    return Ok(None);
                };
                Ok(Some(self.stash_page(&id, data, queue, seen).await?))
            }
        }
    }

    /// Lazily page through a source, one deviation (or child job) at a time.
    fn listing(&self, ctx: Arc<Context>, pager: Pager) -> Events<'_> {
        struct State {
            ctx: Arc<Context>,
            pager: Pager,
            raw: VecDeque<Raw>,
        }
        let state = State {
            ctx,
            pager,
            raw: VecDeque::new(),
        };
        stream::unfold(Some(state), move |state| async move {
            let mut state = state?;
            loop {
                if let Some(raw) = state.raw.pop_front() {
                    match raw {
                        Raw::Status(status) => {
                            let mut expanded = Vec::new();
                            process_status(status, &mut expanded, 0);
                            for item in expanded.into_iter().rev() {
                                state.raw.push_front(Raw::Deviation(item));
                            }
                            continue;
                        }
                        Raw::Child {
                            target,
                            subcategory,
                            data,
                            parent,
                        } => {
                            let events = child_job(
                                ChildMetadata::from_raw("deviantart", subcategory, &data),
                                move || match target {
                                    Target::Folder { user, id, name } => deferred(async move {
                                        self.folder(user, id, name, parent).await
                                    }),
                                    other => self.extract(other),
                                },
                            );
                            return Some((events, Some(state)));
                        }
                        Raw::Eclipse { id, user, journal } => {
                            match self.eclipse_deviation(&id, &user, journal).await {
                                Ok(deviation) => state.raw.push_front(Raw::Deviation(deviation)),
                                Err(e) => return Some((fail(e), None)),
                            }
                            continue;
                        }
                        Raw::Deviation(deviation) => {
                            match self.files(&state.ctx, deviation).await {
                                Ok(media) if media.is_empty() => continue,
                                Ok(media) => {
                                    let events = stream::iter(
                                        group(media, "deviantart").into_iter().map(Ok),
                                    )
                                    .boxed();
                                    return Some((events, Some(state)));
                                }
                                Err(e) => return Some((fail(e), None)),
                            }
                        }
                    }
                }
                match self.next_batch(&mut state.pager).await {
                    Ok(Some(items)) => state.raw.extend(items),
                    Ok(None) => return None,
                    Err(e) => return Some((fail(e), None)),
                }
            }
        })
        .flatten()
        .boxed()
    }

    pub fn stream(&self, url: &Url) -> BoxStream<'_, Result<Media>> {
        crate::event::media_only(self.events(url))
    }

    /// Emit one post boundary per deviation; listings paginate lazily and
    /// user profiles dispatch their `include` subcategories as child jobs.
    pub fn events(&self, url: &Url) -> Events<'_> {
        match target(url) {
            Ok(target) => self.extract(target),
            Err(e) => fail(e),
        }
    }

    fn extract(&self, target: Target) -> Events<'_> {
        if let Target::User(user) = target {
            let children: Vec<Events<'_>> = self
                .options
                .include
                .iter()
                .map(|sub| {
                    let user = user.clone();
                    let target = match *sub {
                        "avatar" => Target::Avatar(user),
                        "background" => Target::Background(user),
                        "scraps" => Target::Scraps(user),
                        "journal" => Target::Journal(user),
                        "status" => Target::Status(user),
                        "favorite" => Target::Favorite(user),
                        _ => Target::Gallery(user),
                    };
                    child_job(
                        ChildMetadata::from_raw("deviantart", "user", &Map::new()),
                        move || self.extract(target),
                    )
                })
                .collect();
            return stream::iter(children).flatten().boxed();
        }
        deferred(self.start(target))
    }

    /// Upstream `items()` user resolution: the profile's canonical name, or
    /// `None` to skip a group when `group` is `"skip"`.
    async fn canonical(&self, user: String) -> Option<String> {
        if self.options.group == Group::Off {
            return Some(user);
        }
        match self.profile(&user).await {
            Some(profile) => Some(
                profile
                    .pointer("/user/username")
                    .and_then(Value::as_str)
                    .filter(|name| name.eq_ignore_ascii_case(&user))
                    .map_or(user, str::to_owned),
            ),
            None if self.options.group == Group::Skip => None,
            None => Some(user),
        }
    }

    fn oauth(&self, path: String, params: Vec<(&'static str, String)>, limit: u64) -> OAuthPager {
        // Upstream rounds page sizes down to a multiple of the metadata batch.
        let limit = match self.options.metadata_limit() {
            Some(batch) if limit > batch => limit / batch * batch,
            _ => limit,
        };
        OAuthPager {
            path,
            params,
            limit,
            offset: Some(0),
            cursor: None,
            public: None,
            extend: true,
            filter: None,
            unpack: false,
            friends: false,
            pages: 0,
        }
    }

    async fn start(&self, target: Target) -> Result<Events<'_>> {
        let user = match &target {
            Target::Gallery(u)
            | Target::Folder { user: u, .. }
            | Target::Scraps(u)
            | Target::GallerySearch { user: u, .. }
            | Target::Favorite(u)
            | Target::Collection { user: u, .. }
            | Target::Journal(u)
            | Target::Status(u)
            | Target::Avatar(u)
            | Target::Background(u)
            | Target::Deviation { user: Some(u), .. } => match self.canonical(u.clone()).await {
                Some(user) => Some(user),
                None => return Ok(stream::empty().boxed()),
            },
            _ => None,
        };
        let name = || user.clone().unwrap_or_default();
        Ok(match target {
            Target::User(_) => unreachable!("dispatched in extract"),
            Target::Gallery(_) if !self.options.flat => {
                self.folder_children("gallery", name()).await?
            }
            Target::Gallery(_) => {
                let pager = self.oauth("gallery/all".into(), vec![("username", name())], 24);
                self.listing(Context::new(&GALLERY, user), Pager::OAuth(pager))
            }
            Target::Folder {
                id, name: folder, ..
            } => self.folder(name(), id, folder, None).await?,
            Target::Favorite(_) if !self.options.flat => {
                self.folder_children("collections", name()).await?
            }
            Target::Favorite(_) => {
                let pager = self.oauth("collections/all".into(), vec![("username", name())], 24);
                self.listing(Context::new(&FAVORITE, user), Pager::OAuth(pager))
            }
            Target::Collection {
                id, name: title, ..
            } => {
                let folders = self.folder_list("collections", &name()).await?;
                let (folder, _) = find_folder(&folders, Some(&title), &id)
                    .ok_or_else(|| error("DeviantArt favourites collection not found"))?;
                let uuid = string(folder, "folderid")
                    .filter(|id| valid_uuid(id))
                    .ok_or_else(|| error("DeviantArt favourites collection not found"))?;
                let collection = json!({
                    "title": folder.get("name").cloned().unwrap_or(Value::Null),
                    "uuid": uuid,
                    "index": id,
                    "owner": name(),
                });
                let pager = self.oauth(
                    format!("collections/{uuid}"),
                    vec![("username", name())],
                    24,
                );
                self.listing(
                    Context::with(&COLLECTION, user, "collection", collection),
                    Pager::OAuth(pager),
                )
            }
            Target::Journal(_) | Target::Status(_) => {
                let (route, filter) = if matches!(target, Target::Journal(_)) {
                    (&JOURNAL, "/journal/")
                } else {
                    (&STATUS, "/status-update/")
                };
                let mut pager =
                    self.oauth("user/profile/posts".into(), vec![("username", name())], 50);
                pager.offset = None;
                pager.filter = Some(filter);
                self.listing(Context::new(route, user), Pager::OAuth(pager))
            }
            Target::Scraps(_) => {
                let pager = EclipsePager {
                    path: "_puppy/dashared/gallection/contents",
                    params: vec![
                        ("username", name()),
                        ("type", "gallery".into()),
                        ("scraps_folder", "true".into()),
                    ],
                    key: "results",
                    offset: Some(0),
                    cursor: None,
                    pages: 0,
                };
                self.listing(Context::new(&SCRAPS, user), Pager::Eclipse(pager))
            }
            Target::GallerySearch { query, .. } => {
                let query = parse_query(&query);
                let get = |key: &str| query.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
                let search = get("q").unwrap_or_default();
                let pager = EclipsePager {
                    path: "_puppy/dashared/gallection/search",
                    params: vec![
                        ("username", name()),
                        ("type", "gallery".into()),
                        ("order", get("sort").unwrap_or_else(|| "most-recent".into())),
                        ("q", search.clone()),
                    ],
                    key: "results",
                    offset: Some(0),
                    cursor: None,
                    pages: 0,
                };
                self.listing(
                    Context::with(&GALLERY_SEARCH, user, "search_tags", search.into()),
                    Pager::Eclipse(pager),
                )
            }
            Target::Avatar(_) => {
                let items = self.avatar(&name()).await;
                self.listing(Context::new(&AVATAR, user), Pager::Items(items))
            }
            Target::Background(_) => {
                let items = self
                    .profile(&name())
                    .await
                    .and_then(|p| p.pointer("/cover_deviation/cover_deviation").cloned())
                    .filter(Value::is_object)
                    .map(Raw::Deviation)
                    .into_iter()
                    .collect();
                self.listing(Context::new(&BACKGROUND, user), Pager::Items(items))
            }
            Target::Deviation { kind, id, .. } => {
                let mut page = self.root.clone();
                match &user {
                    Some(user) => {
                        page.path_segments_mut().expect("base URL").clear().extend([
                            user.as_str(),
                            kind,
                            id.as_str(),
                        ]);
                    }
                    None => {
                        page.path_segments_mut().expect("base URL").clear().extend([
                            "view",
                            id.as_str(),
                            "",
                        ]);
                    }
                }
                let page = self.page(&page).await?;
                let uuid = page_uuid(&page)
                    .ok_or_else(|| error("DeviantArt deviation page contains no deviation UUID"))?;
                let mut deviation = self.deviation(&uuid, None).await?;
                if let Some(object) = deviation.as_object_mut() {
                    object.insert("_page".into(), page.into());
                    object.insert("index_file".into(), 0.into());
                    object.insert("num".into(), 1.into());
                    object.insert("count".into(), 1.into());
                }
                let ctx = Context::new(&DEVIATION, user);
                let files = self.files(&ctx, deviation).await?;
                if files.is_empty() {
                    return Err(error(
                        "DeviantArt deviation has no downloadable content (journals are disabled or the deviation is restricted)",
                    ));
                }
                stream::iter(group(files, "deviantart").into_iter().map(Ok)).boxed()
            }
            Target::Tag(tag) => {
                let pager = self.oauth("browse/tags".into(), vec![("tag", tag.clone())], 50);
                self.listing(
                    Context::with(&TAG, None, "search_tags", tag.into()),
                    Pager::OAuth(pager),
                )
            }
            Target::Search(query) => {
                let params = parse_query(&query);
                let search = params
                    .iter()
                    .find(|(k, _)| k == "q")
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default();
                self.listing(
                    Context::with(&SEARCH, None, "search_tags", search.into()),
                    Pager::Search {
                        params,
                        cursor: None,
                        pages: 0,
                    },
                )
            }
            Target::Following(user) => {
                // Upstream `user_friends`: the public token unless `public` is off.
                let mut pager = self.oauth(format!("user/friends/{user}"), Vec::new(), 50);
                pager.friends = true;
                self.listing(Context::new(&USER, None), Pager::OAuth(pager))
            }
            Target::Watch | Target::WatchPosts => {
                if !self.has_refresh {
                    return Err(error(
                        "DeviantArt watch feeds require a refresh-token (configure `refresh-token` or run `oauth:deviantart`)",
                    ));
                }
                let (route, path) = if matches!(target, Target::Watch) {
                    (&WATCH, "browse/deviantsyouwatch")
                } else {
                    (&WATCH_POSTS, "browse/posts/deviantsyouwatch")
                };
                let mut pager = self.oauth(path.into(), Vec::new(), 50);
                pager.public = Some(false);
                pager.unpack = matches!(target, Target::WatchPosts);
                self.listing(Context::new(route, None), Pager::OAuth(pager))
            }
            Target::Stash(id) => {
                let mut seen = HashSet::new();
                seen.insert(id.clone());
                self.listing(
                    Context::new(&STASH, None),
                    Pager::Stash {
                        queue: VecDeque::from([(id, None)]),
                        seen,
                    },
                )
            }
        })
    }

    /// Upstream `flat: false`: queue each gallery folder or collection.
    async fn folder_children(&self, kind: &'static str, user: String) -> Result<Events<'_>> {
        let folders = self.folder_list(kind, &user).await?;
        let subcategory = if kind == "gallery" {
            "gallery"
        } else {
            "favorite"
        };
        let items = folders
            .iter()
            .filter_map(|folder| {
                let id = string(folder, "folderid").filter(|id| valid_uuid(id))?;
                let name = folder
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let target = if kind == "gallery" {
                    Target::Folder {
                        user: user.clone(),
                        id: id.to_owned(),
                        name: Some(name.to_owned()),
                    }
                } else {
                    Target::Collection {
                        user: user.clone(),
                        id: id.to_owned(),
                        name: name.to_owned(),
                    }
                };
                Some(Raw::Child {
                    target,
                    subcategory,
                    data: folder.as_object().cloned().unwrap_or_default(),
                    parent: None,
                })
            })
            .collect();
        Ok(self.listing(Context::new(&GALLERY, Some(user)), Pager::Items(items)))
    }

    /// Upstream `DeviantartFolderExtractor.deviations`; `parent` names the
    /// folder that queued this one as a subfolder.
    async fn folder(
        &self,
        user: String,
        id: String,
        name: Option<String>,
        parent: Option<String>,
    ) -> Result<Events<'_>> {
        let folders = self.folder_list("gallery", &user).await?;
        let (folder, _) = find_folder(&folders, name.as_deref(), &id)
            .ok_or_else(|| error("DeviantArt gallery folder not found"))?;
        let uuid = string(folder, "folderid")
            .filter(|id| valid_uuid(id))
            .ok_or_else(|| error("DeviantArt gallery folder not found"))?
            .to_owned();
        let folder_name = folder
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mut info = json!({
            "title": folder_name,
            "uuid": uuid,
            "index": id,
            "owner": user,
            "parent_uuid": folder.get("parent").cloned().unwrap_or(Value::Null),
        });
        let route = match &parent {
            Some(parent) => {
                info["parent_folder"] = parent.clone().into();
                if self.options.flat {
                    &SUBFOLDER_FLAT
                } else {
                    &SUBFOLDER
                }
            }
            None => &FOLDER,
        };
        let mut children = Vec::new();
        if self.options.subfolders
            && folder.get("has_subfolders").and_then(Value::as_bool) == Some(true)
        {
            for sub in folder
                .get("subfolders")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let Some(sub_id) = string(sub, "folderid").filter(|id| valid_uuid(id)) else {
                    continue;
                };
                let mut data = sub.as_object().cloned().unwrap_or_default();
                data.insert("parent_folder".into(), folder_name.into());
                data.insert("subfolder".into(), true.into());
                children.push(Raw::Child {
                    target: Target::Folder {
                        user: user.clone(),
                        id: sub_id.to_owned(),
                        name: sub.get("name").and_then(Value::as_str).map(str::to_owned),
                    },
                    subcategory: "folder",
                    data,
                    parent: Some(folder_name.to_owned()),
                });
            }
        }
        let mut pager = self.oauth(
            format!("gallery/{uuid}"),
            vec![("username", user.clone())],
            24,
        );
        pager.params.push(("mode", "newest".into()));
        let ctx = Context::with(route, Some(user), "folder", info);
        let children = self.listing(ctx.clone(), Pager::Items(children));
        Ok(children
            .chain(self.listing(ctx, Pager::OAuth(pager)))
            .boxed())
    }

    /// Upstream `DeviantartAvatarExtractor.deviations`.
    async fn avatar(&self, name: &str) -> Vec<Raw> {
        let lower = name.to_ascii_lowercase();
        let Some(profile) = self.profile(&lower).await else {
            return Vec::new();
        };
        let Some(user) = profile.get("user").filter(|u| u.is_object()) else {
            return Vec::new();
        };
        let Some(icon) = string(user, "usericon") else {
            return Vec::new();
        };
        if icon == DEFAULT_AVATAR {
            return Vec::new();
        }
        let index = icon.rsplit_once('?').map_or("0", |(_, index)| index);
        let make = |url: String, fmt: &str| {
            Raw::Deviation(json!({
                "author": user,
                "da_category": "avatar",
                "index": index.trim().parse::<i64>().unwrap_or(0),
                "is_deleted": false,
                "is_downloadable": false,
                "published_time": 0,
                "title": format!("avatar{fmt}"),
                "stats": {"comments": 0},
                "content": {"src": url},
            }))
        };
        let Some(formats) = &self.options.avatar_formats else {
            return vec![make(icon.replacen("/avatars/", "/avatars-big/", 1), "")];
        };
        let mut chars = lower.chars();
        let (Some(first), Some(second)) = (chars.next(), chars.next()) else {
            return Vec::new();
        };
        formats
            .iter()
            .map(|format| {
                let (fmt, ext) = format.rsplit_once('.').unwrap_or(("", format));
                let fmt = if fmt.is_empty() {
                    String::new()
                } else {
                    format!("-{fmt}")
                };
                make(
                    format!(
                        "https://a.deviantart.net/avatars{fmt}/{first}/{second}/{lower}.{ext}?{index}"
                    ),
                    &fmt,
                )
            })
            .collect()
    }

    /// Upstream `items()` for one deviation (or status): its files.
    async fn files(&self, ctx: &Context, deviation: Value) -> Result<Vec<Media>> {
        let Value::Object(mut dev) = deviation else {
            return Ok(Vec::new());
        };
        if dev.get("is_deleted").and_then(Value::as_bool) == Some(true)
            || dev.get("tier_access").and_then(Value::as_str) == Some("locked")
            || dev.contains_key("premium_folder_data")
        {
            return Ok(Vec::new());
        }
        let status = ctx.route.subcategory == "status" && !dev.contains_key("deviationid");
        if status {
            if prepare_status(ctx, &mut dev).is_none() {
                return Ok(Vec::new());
            }
        } else {
            prepare(ctx, &mut dev);
        }
        for (key, value) in &ctx.extra {
            dev.insert(key.clone(), value.clone());
        }
        let post_id = if status {
            dev.get("statusid")
                .and_then(Value::as_str)
                .map(str::to_owned)
        } else {
            dev.get("deviationid")
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        let post_id = match post_id {
            Some(id) if valid_uuid(&id) => id,
            _ if ctx.route.subcategory == "avatar" => String::new(),
            _ => return Ok(Vec::new()),
        };
        let mut files = Vec::new();
        let downloadable = dev.get("is_downloadable").and_then(Value::as_bool) == Some(true);
        if dev.get("content").is_some_and(Value::is_object) {
            let content = self.extract_content(&mut dev, &post_id).await;
            files.extend(self.commit(ctx, &mut dev, &content, &post_id, None));
        } else if self.options.original != Original::Off
            && downloadable
            && let Ok(content) = self.download(&post_id, None).await
        {
            dev.insert("is_original".into(), true.into());
            files.extend(self.commit(ctx, &mut dev, &content, &post_id, None));
        }
        let video = dev
            .get("videos")
            .and_then(Value::as_array)
            .and_then(|videos| {
                videos
                    .iter()
                    .max_by_key(|v| {
                        let quality = string(v, "quality").unwrap_or_default();
                        parse_int_text(&quality[..quality.len().saturating_sub(1)], 0)
                    })
                    .cloned()
            });
        if let Some(video) = video {
            dev.insert("is_original".into(), false.into());
            files.extend(self.commit(ctx, &mut dev, &video, &post_id, Some("video")));
        }
        if let Some(flash) = dev.get("flash").cloned() {
            dev.insert("is_original".into(), true.into());
            files.extend(self.commit(ctx, &mut dev, &flash, &post_id, Some("flash")));
        }
        if self.options.journals != Journals::Off
            && let Some(journal) = self.extract_journal(&mut dev).await?
        {
            dev.insert("is_original".into(), true.into());
            files.extend(self.commit_journal(ctx, &mut dev, &journal, &post_id));
        }
        let mut index = 0;
        for file in &mut files {
            index += 1;
            if let Some(metadata) = &mut file.metadata {
                metadata.index = index;
            }
        }
        Ok(files)
    }

    /// Upstream `_extract_content`: original downloads and URL rewriting.
    async fn extract_content(&self, dev: &mut Map<String, Value>, uuid: &str) -> Value {
        let mut content = dev.get("content").cloned().unwrap_or_else(|| json!({}));
        let downloadable = dev.get("is_downloadable").and_then(Value::as_bool) == Some(true);
        if self.options.original != Original::Off && downloadable {
            let public = (dev.contains_key("premium_folder_data")
                || dev.contains_key("tier_access")
                || dev.get("is_mature").and_then(Value::as_bool) == Some(true))
            .then_some(false);
            // Upstream aborts on refused downloads; keep the preview instead.
            if let Ok(Value::Object(data)) = self.download(uuid, public).await {
                let image = self.options.original == Original::Images;
                let src = data.get("src").and_then(Value::as_str).unwrap_or_default();
                let path = src.split('?').next().unwrap_or_default();
                if !image || is_image(path) {
                    if let Some(content) = content.as_object_mut() {
                        for (key, value) in data {
                            content.insert(key, value);
                        }
                    }
                    dev.insert("is_original".into(), true.into());
                }
            }
            return content;
        }
        let Some(src) = content
            .get("src")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            return content;
        };
        if !src.starts_with("https://images-wixmp-") {
            return content;
        }
        let mut src = src;
        let index = dev.get("index").and_then(Value::as_i64).unwrap_or(0);
        if self.options.intermediary
            && index <= INTERMEDIARY_MAX_INDEX
            && let Some(rewritten) = intermediary(&src)
        {
            dev.insert("is_original".into(), false.into());
            dev.insert("_fallback".into(), json!([src]));
            src = rewritten;
        }
        if let Some(quality) = &self.options.quality {
            src = if quality.starts_with("-fullview") {
                png_quality(&src)
            } else {
                replace_digits(&src, ",q_", quality)
            };
        }
        src = replace_digits(&src, ",blur_", "");
        content["src"] = src.into();
        content
    }

    /// Upstream `commit`: one file with a snapshot of the keywords.
    fn commit(
        &self,
        ctx: &Context,
        dev: &mut Map<String, Value>,
        target: &Value,
        post_id: &str,
        kind: Option<&str>,
    ) -> Option<Media> {
        let src = target.get("src").and_then(Value::as_str)?.to_owned();
        let url = media_url(&src)?;
        let name = target
            .get("filename")
            .and_then(Value::as_str)
            .filter(|n| !n.is_empty())
            .unwrap_or(&src);
        let extension = ext_from_url(name);
        let mut copy = target.as_object().cloned().unwrap_or_default();
        copy.insert(
            "filename".into(),
            dev.get("filename").cloned().unwrap_or(Value::Null),
        );
        copy.insert("extension".into(), extension.clone().into());
        dev.insert("target".into(), Value::Object(copy));
        dev.insert("extension".into(), extension.clone().into());
        if !dev.contains_key("is_original") {
            dev.insert("is_original".into(), (!src.contains("/v1/")).into());
        }
        let mut handoff = BTreeMap::new();
        if let Some(fallback) = dev.get("_fallback").and_then(Value::as_array) {
            let safe: Vec<Value> = fallback
                .iter()
                .filter(|u| u.as_str().and_then(media_url).is_some())
                .cloned()
                .collect();
            if !safe.is_empty() {
                handoff.insert("_fallback".into(), Value::Array(safe));
            }
        }
        let file = FileMetadata {
            width: target
                .get("width")
                .and_then(Value::as_u64)
                .filter(|v| *v > 0),
            height: target
                .get("height")
                .and_then(Value::as_u64)
                .filter(|v| *v > 0),
            ..FileMetadata::default()
        };
        let ext = if extension.is_empty() {
            "jpg".into()
        } else {
            extension
        };
        let mut media = self.media(
            ctx,
            dev,
            url,
            MediaJob::DirectHttp,
            post_id,
            kind,
            &ext,
            file,
        );
        media.handoff = handoff;
        Some(media)
    }

    /// Upstream `_commit_journal_html`/`_commit_journal_text`.
    fn commit_journal(
        &self,
        ctx: &Context,
        dev: &mut Map<String, Value>,
        html: &str,
        post_id: &str,
    ) -> Option<Media> {
        let (text, extension) = match self.options.journals {
            Journals::Html => (journal_html(dev, html), "htm"),
            Journals::Text => (journal_text(dev, html), "txt"),
            Journals::Off => return None,
        };
        dev.insert("extension".into(), extension.into());
        let (url, job) = crate::extract::inline_text(text).ok()?;
        Some(self.media(
            ctx,
            dev,
            url,
            job,
            post_id,
            Some("journal"),
            extension,
            FileMetadata::default(),
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn media(
        &self,
        ctx: &Context,
        dev: &Map<String, Value>,
        url: Url,
        job: MediaJob,
        post_id: &str,
        kind: Option<&str>,
        ext: &str,
        file: FileMetadata,
    ) -> Media {
        let mut fields = dev.clone();
        fields.remove("_page");
        fields.insert("category".into(), "deviantart".into());
        fields.insert("subcategory".into(), ctx.route.subcategory.into());
        let author = dev
            .get("author")
            .and_then(|a| a.get("username"))
            .and_then(Value::as_str)
            .map(Into::into);
        let index = dev.get("index").and_then(Value::as_i64).unwrap_or(0);
        let (post_id, media_id, filename) = if post_id.is_empty() {
            let user = dev
                .get("_username")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            (
                user,
                format!("avatar:{index}"),
                format!("avatar_{index}.{ext}"),
            )
        } else {
            match kind {
                None => (
                    post_id.to_owned(),
                    post_id.to_owned(),
                    format!("{post_id}.{ext}"),
                ),
                Some(kind) => (
                    post_id.to_owned(),
                    format!("{post_id}:{kind}"),
                    format!("{post_id}_{kind}.{ext}"),
                ),
            }
        };
        let mut media = Media {
            filename,
            job,
            category: "deviantart".into(),
            metadata: Some(PostMetadata {
                file: Some(file),
                post_id,
                media_id,
                username: author,
                caption: dev.get("title").and_then(Value::as_str).map(Into::into),
                timestamp: dev.get("published_time").and_then(Value::as_i64),
                index: 1,
                site_keywords: Some(Arc::new(SiteKeywords {
                    route: ctx.route,
                    fields,
                })),
                ..PostMetadata::default()
            }),
            url,
            headers: BTreeMap::new(),
            handoff: BTreeMap::new(),
        };
        self.network.apply_media(&mut media);
        media
    }

    /// Upstream `_extract_journal`: literature/journal HTML, or a status body.
    async fn extract_journal(&self, dev: &mut Map<String, Value>) -> Result<Option<String>> {
        if dev.contains_key("excerpt") {
            let page = match dev.remove("_page") {
                Some(Value::String(page)) => page,
                _ => {
                    let url = dev
                        .get("url")
                        .and_then(Value::as_str)
                        .and_then(|u| self.site_url(u))
                        .ok_or_else(|| error("DeviantArt journal has no deviation page URL"))?;
                    self.page(&url).await?
                }
            };
            if let Some(html) = crate::sites::extr(
                &page,
                "<h2>Literature Text</h2></span><div>",
                "</div></section></div></div>",
            )
            .filter(|h| !h.is_empty())
            {
                return Ok(Some(html.to_owned()));
            }
            let state =
                crate::sites::extr(&page, "window.__INITIAL_STATE__ = JSON.parse(\"", "\");")
                    .and_then(|raw| serde_json::from_str::<Value>(&unescape_json(raw)).ok());
            let content = state
                .as_ref()
                .and_then(|s| s.pointer("/@@entities/deviation"))
                .and_then(Value::as_object)
                .and_then(|d| d.values().next_back())
                .and_then(|d| d.get("textContent"));
            if let Some(markup) = content
                .and_then(|c| c.pointer("/html/markup"))
                .and_then(Value::as_str)
                .filter(|m| !m.is_empty() && !m.starts_with('{'))
            {
                return Ok(Some(markup.to_owned()));
            }
            // Tiptap/draft JSON markup is not converted; use the excerpt.
            let excerpt = content
                .and_then(|c| c.get("excerpt"))
                .or_else(|| dev.get("excerpt"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            return Ok(Some(excerpt.replace('\n', "<br />")));
        }
        Ok(match dev.remove("body") {
            Some(Value::String(body)) => Some(body),
            _ => None,
        })
    }

    async fn request(&self, endpoint: &Url, build: impl Fn() -> RequestBuilder) -> Result<Reply> {
        for attempt in 0..=self.retries {
            match self.attempt(endpoint, build(), attempt).await {
                Ok(reply) => return Ok(reply),
                Err(failure) if failure.retry && attempt < self.retries => {
                    tokio::time::sleep(backoff(attempt)).await;
                }
                Err(failure) => return Err(failure.error),
            }
        }
        unreachable!()
    }

    async fn attempt(
        &self,
        endpoint: &Url,
        request: RequestBuilder,
        attempt: u8,
    ) -> std::result::Result<Reply, Failure> {
        self.rate_limits.wait(endpoint, self.timeout).await?;
        let mut response = request
            .send()
            .await
            .map_err(|_| Failure::transient("DeviantArt request failed; check network access"))?;
        let status = response.status();
        self.keep_cookies(response.headers());
        if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
            let delay = retry_after(response.headers(), SystemTime::now());
            self.rate_limits
                .defer(endpoint, delay.unwrap_or_else(|| backoff(attempt)));
            return Err(Failure::transient(
                if status == StatusCode::TOO_MANY_REQUESTS {
                    match delay {
                        Some(delay) => format!(
                            "DeviantArt rate limit; retry after {} seconds",
                            delay.as_secs()
                        ),
                        None => "DeviantArt rate limit; retry later".into(),
                    }
                } else {
                    format!("DeviantArt returned HTTP {status}")
                },
            ));
        }
        if status.is_redirection() {
            return Err(error("DeviantArt responded with an unexpected redirect").into());
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| Failure::transient("could not read DeviantArt response"))?
        {
            if body.len() + chunk.len() > MAX_RESPONSE {
                return Err(error("DeviantArt response exceeded 16 MiB").into());
            }
            body.extend_from_slice(&chunk);
        }
        Ok(if status.is_success() {
            Reply::Ok(body)
        } else {
            Reply::Status(status)
        })
    }

    /// Remember `Set-Cookie` name/value pairs for later web requests.
    fn keep_cookies(&self, headers: &reqwest::header::HeaderMap) {
        let mut cookies = self.cookies.lock().expect("cookie lock");
        for value in headers.get_all(reqwest::header::SET_COOKIE) {
            let Ok(value) = value.to_str() else {
                continue;
            };
            let pair = value.split(';').next().unwrap_or_default();
            if let Some((name, value)) = pair.split_once('=') {
                let name = name.trim();
                if !name.is_empty()
                    && name.len() <= 128
                    && value.len() <= 4096
                    && cookies.len() < 64
                    && name.bytes().all(|b| b.is_ascii_graphic() && b != b'=')
                    && value.bytes().all(|b| b.is_ascii_graphic() && b != b';')
                {
                    cookies.insert(name.to_owned(), value.to_owned());
                }
            }
        }
    }
}

/// Upstream `DeviantartStatusExtractor.process_status`: shared deviations
/// and statuses first, then the status itself unless deleted.
/// Upstream `DeviantartFollowingExtractor`: one `user/friends` entry queued
/// as its profile URL, with the entry as the child's keywords.
fn friend_child(entry: Value) -> Option<Raw> {
    let name = entry.pointer("/user/username")?.as_str()?;
    if !valid_user(name) {
        return None;
    }
    let target = Target::User(name.to_ascii_lowercase());
    let Value::Object(data) = entry else {
        return None;
    };
    Some(Raw::Child {
        target,
        subcategory: "following",
        data,
        parent: None,
    })
}

fn process_status(status: Value, out: &mut Vec<Value>, depth: usize) {
    if depth < 8 {
        for item in status
            .get("items")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(deviation) = item.get("deviation") {
                out.push(deviation.clone());
            }
            if let Some(inner) = item.get("status") {
                process_status(inner.clone(), out, depth + 1);
            }
        }
    }
    if status.get("is_deleted").and_then(Value::as_bool) != Some(true) {
        out.push(status);
    }
}

/// Upstream `prepare` for deviation objects.
fn prepare(ctx: &Context, dev: &mut Map<String, Value>) {
    if !dev.contains_key("index") {
        let url = dev.get("url").and_then(Value::as_str).unwrap_or_default();
        if url.starts_with("https://www.deviantart.com/stash/") || url.starts_with("https://sta.sh")
        {
            let name = dev
                .get("content")
                .and_then(|c| c.get("src"))
                .and_then(Value::as_str)
                .and_then(|src| src.split('/').nth(5))
                .unwrap_or_default();
            let base36 = name.split('-').next().unwrap_or_default();
            let base36 = base36.get(1..).unwrap_or_default().to_owned();
            let index = b36decode(&base36).unwrap_or(0);
            dev.insert("index_base36".into(), base36.into());
            dev.insert("index".into(), index.into());
        } else if dev.contains_key("url") {
            let tail = url.rsplit_once('-').map_or(url, |(_, tail)| tail);
            dev.insert("index".into(), parse_int_text(tail, 0).into());
        } else {
            dev.insert("index".into(), 0.into());
            dev.insert("index_base36".into(), "0".into());
        }
    }
    if !dev.contains_key("index_base36") {
        let index = dev.get("index").and_then(Value::as_u64).unwrap_or(0);
        dev.insert("index_base36".into(), b36encode(index).into());
    }
    set_username(ctx, dev);
    let published = parse_int(dev.get("published_time"));
    dev.insert("published_time".into(), published.into());
    dev.insert(
        "date".into(),
        crate::filter::source_date_text(Some(published)).into(),
    );
    let title = dev.get("title").and_then(Value::as_str).unwrap_or_default();
    let author = dev
        .get("author")
        .and_then(|a| a.get("username"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let filename = format!(
        "{}_by_{}-d{}",
        word_chars(&title.to_lowercase()),
        word_chars(&author.to_lowercase()),
        dev.get("index_base36")
            .and_then(Value::as_str)
            .unwrap_or_default()
    );
    dev.insert("filename".into(), filename.into());
}

fn set_username(ctx: &Context, dev: &mut Map<String, Value>) {
    let username = match &ctx.user {
        Some(user) => user.clone(),
        None => dev
            .get("author")
            .and_then(|a| a.get("username"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    };
    dev.insert("_username".into(), username.to_lowercase().into());
    dev.insert("username".into(), username.into());
}

/// Upstream `DeviantartStatusExtractor.prepare` for status objects.
fn prepare_status(ctx: &Context, dev: &mut Map<String, Value>) -> Option<()> {
    let index = dev
        .get("url")
        .and_then(Value::as_str)
        .map(|url| {
            let mut parts = url.rsplit('/');
            let last = parts.next().unwrap_or_default();
            let part = if last.is_empty() {
                parts.next().unwrap_or_default()
            } else {
                last
            };
            parse_int_text(part, 0)
        })
        .unwrap_or(0);
    dev.insert("index".into(), index.into());
    set_username(ctx, dev);
    let ts = dev.get("ts").and_then(Value::as_str)?;
    let seconds = crate::source_date::parse_string(ts)?;
    dev.insert(
        "date".into(),
        crate::filter::source_date_text(Some(seconds)).into(),
    );
    dev.insert("published_time".into(), seconds.into());
    dev.insert("da_category".into(), "Status".into());
    dev.insert("category_path".into(), "status".into());
    dev.insert("is_downloadable".into(), false.into());
    dev.insert("title".into(), "Status Update".into());
    let comments = dev.remove("comments_count").unwrap_or_else(|| 0.into());
    dev.insert("stats".into(), json!({"comments": comments}));
    Some(())
}

fn is_image(path: &str) -> bool {
    matches!(
        ext_from_url(path).as_str(),
        "jpg"
            | "jpeg"
            | "jpe"
            | "png"
            | "gif"
            | "webp"
            | "bmp"
            | "tif"
            | "tiff"
            | "svg"
            | "avif"
            | "ico"
            | "heic"
            | "heif"
    )
}

/// Upstream `_unescape_json`.
fn unescape_json(text: &str) -> String {
    text.replace("\\\"", "\"")
        .replace("\\'", "'")
        .replace("\\\\", "\\")
}

/// Upstream search link pattern
/// `href="https://www.deviantart.com/([^/?#]+)/(art|journal)/(?:[^"]+-)?(\d+)`.
fn search_links(page: &str) -> Vec<(String, bool, String)> {
    const PREFIX: &str = "href=\"https://www.deviantart.com/";
    let mut found = Vec::new();
    for (at, _) in page.match_indices(PREFIX) {
        let rest = &page[at + PREFIX.len()..];
        let Some(slash) = rest.find(['/', '?', '#']) else {
            continue;
        };
        let user = &rest[..slash];
        if user.is_empty() || !rest[slash..].starts_with('/') {
            continue;
        }
        let rest = &rest[slash + 1..];
        let (journal, rest) = if let Some(rest) = rest.strip_prefix("art/") {
            (false, rest)
        } else if let Some(rest) = rest.strip_prefix("journal/") {
            (true, rest)
        } else {
            continue;
        };
        let slug = &rest[..rest.find('"').unwrap_or(rest.len())];
        if let Some(id) = slug_id(slug).filter(|_| valid_user(user)) {
            found.push((user.to_owned(), journal, id.to_owned()));
        }
    }
    found
}

/// Single-pass `{name}` substitution, like Python `str.format`.
fn fill(template: &str, values: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let tail = &rest[start + 1..];
        match tail
            .find('}')
            .and_then(|end| Some((end, values.iter().find(|(k, _)| *k == &tail[..end])?)))
        {
            Some((end, (_, value))) => {
                out.push_str(value);
                rest = &tail[end + 1..];
            }
            None => {
                out.push('{');
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    out
}

const SHADOW: &str = "
<span class=\"shadow\">
    <img src=\"{src}\" class=\"smshadow\" width=\"{width}\" height=\"{height}\">
</span>
<br><br>
";

const HEADER: &str = "<div usr class=\"gr\">
<div class=\"metadata\">
    <h2><a href=\"{url}\">{title}</a></h2>
    <ul>
        <li class=\"author\">
            by <span class=\"name\"><span class=\"username-with-symbol u\">
            <a class=\"u regular username\" href=\"{userurl}\">{username}</a>\
<span class=\"user-symbol regular\"></span></span></span>,
            <span>{date}</span>
        </li>
    </ul>
</div>
";

const HEADER_CUSTOM: &str = "<div class='boxtop journaltop'>
<h2>
    <img src=\"https://st.deviantart.net/minish/gruzecontrol/icons/journal.gif\
?2\" style=\"vertical-align:middle\" alt=\"\"/>
    <a href=\"{url}\">{title}</a>
</h2>
Journal Entry: <span>{date}</span>
";

const HTML: &str = "<!DOCTYPE html>
<html>
<head>
    <meta charset=\"utf-8\">
    <title>{title}</title>
    <link rel=\"stylesheet\" href=\"https://st.deviantart.net\
/css/deviantart-network_lc.css?3843780832\"/>
    <link rel=\"stylesheet\" href=\"https://st.deviantart.net\
/css/group_secrets_lc.css?3250492874\"/>
    <link rel=\"stylesheet\" href=\"https://st.deviantart.net\
/css/v6core_lc.css?4246581581\"/>
    <link rel=\"stylesheet\" href=\"https://st.deviantart.net\
/css/sidebar_lc.css?1490570941\"/>
    <link rel=\"stylesheet\" href=\"https://st.deviantart.net\
/css/writer_lc.css?3090682151\"/>
    <link rel=\"stylesheet\" href=\"https://st.deviantart.net\
/css/v6loggedin_lc.css?3001430805\"/>
    <style>{css}</style>
    <link rel=\"stylesheet\" href=\"https://st.deviantart.net\
/roses/cssmin/core.css?1488405371919\"/>
    <link rel=\"stylesheet\" href=\"https://st.deviantart.net\
/roses/cssmin/peeky.css?1487067424177\"/>
    <link rel=\"stylesheet\" href=\"https://st.deviantart.net\
/roses/cssmin/desktop.css?1491362542749\"/>
    <link rel=\"stylesheet\" href=\"https://static.parastorage.com/services\
/da-deviation/2bfd1ff7a9d6bf10d27b98dd8504c0399c3f9974a015785114b7dc6b\
/app.min.css\"/>
</head>
<body id=\"deviantART-v7\" class=\"bubble no-apps loggedout w960 deviantart\">
    <div id=\"output\">
    <div class=\"dev-page-container bubbleview\">
    <div class=\"dev-page-view view-mode-normal\">
    <div class=\"dev-view-main-content\">
    <div class=\"dev-view-deviation\">
    {shadow}
    <div class=\"journal-wrapper tt-a\">
    <div class=\"journal-wrapper2\">
    <div class=\"journal {cls} journalcontrol\">
    {html}
    </div>
    </div>
    </div>
    </div>
    </div>
    </div>
    </div>
    </div>
</body>
</html>
";

const HTML_EXTRA: &str = r#"<div id="devskin0"><div class="negate-box-margin" style=""><div usr class="gr-box gr-genericbox"
        ><i usr class="gr1"><i></i></i
        ><i usr class="gr2"><i></i></i
        ><i usr class="gr3"><i></i></i
        ><div usr class="gr-top">
            <i usr class="tri"></i>
            {header}
            </div>
    </div><div usr class="gr-body"><div usr class="gr">
            <div class="grf-indent">
            <div class="text">
                {html}            </div>
        </div>
                </div></div>
        <i usr class="gr3 gb"></i>
        <i usr class="gr2 gb"></i>
        <i usr class="gr1 gb gb1"></i>    </div>
    </div></div>"#;

fn field<'a>(dev: &'a Map<String, Value>, key: &str) -> &'a str {
    dev.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn scalar(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => "None".into(),
        Some(other) => other.to_string(),
    }
}

/// Upstream `_commit_journal_html`.
fn journal_html(dev: &Map<String, Value>, html: &str) -> String {
    let title = escape(field(dev, "title"));
    let url = field(dev, "url");
    let date = field(dev, "date");
    let thumbs = dev
        .get("thumbs")
        .filter(|t| t.as_array().is_some_and(|a| !a.is_empty()))
        .or_else(|| dev.get("files"));
    let shadow = thumbs
        .and_then(Value::as_array)
        .and_then(|t| t.first())
        .map(|thumb| {
            fill(
                SHADOW,
                &[
                    ("src", &scalar(thumb.get("src"))),
                    ("width", &scalar(thumb.get("width"))),
                    ("height", &scalar(thumb.get("height"))),
                ],
            )
        })
        .unwrap_or_default();
    let (css, cls, html) = match html.strip_prefix("<style") {
        Some(_) => {
            let (css, html) = html.split_once("</style>").unwrap_or((html, ""));
            let css = css.split_once('>').map_or("", |(_, css)| css);
            (css, "withskin", html)
        }
        None => ("", "journal-green", html),
    };
    const CUSTOM: &str = "<div class=\"boxtop journaltop\">";
    let custom = html.find(CUSTOM).is_some_and(|at| at + CUSTOM.len() <= 250);
    let (needle, header) = if custom {
        (
            CUSTOM,
            fill(
                HEADER_CUSTOM,
                &[("title", &title), ("url", url), ("date", date)],
            ),
        )
    } else {
        let author = dev
            .get("author")
            .and_then(|a| a.get("username"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let urlname = Some(field(dev, "username"))
            .filter(|n| !n.is_empty())
            .map_or_else(|| author.to_lowercase(), str::to_owned);
        (
            "<div usr class=\"gr\">",
            fill(
                HEADER,
                &[
                    ("title", &title),
                    ("url", url),
                    ("userurl", &format!("{ROOT}/{urlname}/")),
                    ("username", author),
                    ("date", date),
                ],
            ),
        )
    };
    let html = if html.contains(needle) {
        html.replacen(needle, &header, 1)
    } else {
        fill(HTML_EXTRA, &[("header", &header), ("html", html)])
    };
    fill(
        HTML,
        &[
            ("title", &title),
            ("html", &html),
            ("shadow", &shadow),
            ("css", css),
            ("cls", cls),
        ],
    )
}

/// Upstream `_commit_journal_text`.
fn journal_text(dev: &Map<String, Value>, html: &str) -> String {
    let html = match html.strip_prefix("<style") {
        Some(_) => html.split_once("</style>").map_or("", |(_, rest)| rest),
        None => html,
    };
    let (head, tail) = html.rsplit_once("<script").unwrap_or(("", html));
    let body = if head.is_empty() { tail } else { head };
    let content: Vec<String> = body
        .split("<br />")
        .map(|part| crate::sites::unescape(&crate::sites::remove_html(part)))
        .collect();
    let author = dev
        .get("author")
        .and_then(|a| a.get("username"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    format!(
        "{}\nby {author}, {}\n\n{}\n",
        field(dev, "title"),
        field(dev, "date"),
        content.join("\n")
    )
}

enum Reply {
    Ok(Vec<u8>),
    Status(StatusCode),
}

fn status_error(status: StatusCode) -> crate::Error {
    match status.as_u16() {
        404 | 410 => error("DeviantArt deviation, user or folder not found"),
        400 | 401 | 403 => error(format!(
            "DeviantArt refused access (HTTP {status}); the deviation may be restricted"
        )),
        _ => error(format!("DeviantArt returned HTTP {status}")),
    }
}

fn backoff(attempt: u8) -> Duration {
    Duration::from_millis(250 * (1u64 << attempt.min(6)))
}

struct Failure {
    error: crate::Error,
    retry: bool,
}

impl Failure {
    fn transient(message: impl Into<String>) -> Self {
        Self {
            error: error(message),
            retry: true,
        }
    }
}

impl From<crate::Error> for Failure {
    fn from(error: crate::Error) -> Self {
        Self {
            error,
            retry: false,
        }
    }
}

#[cfg(test)]
#[path = "deviantart_tests.rs"]
mod tests;
