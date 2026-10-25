//! Experimental native Flickr photo, album, gallery, group, favorite, search
//! and photostream extraction.
//!
//! Mirrors gallery_dl/extractor/flickr.py using the public REST API
//! (`format=json&nojsoncallback=1`) with the public site key that upstream
//! reads from `https://www.flickr.com/prints` unless the `api-key` setting
//! overrides it. Files carry upstream's keyword
//! dictionaries and default `filename_fmt`/`directory_fmt` per extractor
//! class. Single photos use `flickr.photos.getInfo` plus
//! `flickr.photos.getSizes`; listings request `url_*` extras and only call
//! `getSizes` when no listed size fits. Videos use `flickr.video.getStreamInfo`
//! unless the `videos` setting is false. Media must be served over HTTPS from
//! `*.staticflickr.com` or, for video streams, `*.flickr.com`. OAuth and
//! private content are not supported.
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
use reqwest::{Client, StatusCode};
use serde_json::{Map, Value};
use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, SystemTime},
};
use url::Url;

const USER_AGENT: &str = concat!(
    "gallery-dl-rs/",
    env!("CARGO_PKG_VERSION"),
    " (native Flickr extractor)"
);
const MAX_RESPONSE: usize = 32 * 1024 * 1024;
const PAGE_SIZE: usize = 500;
const MAX_PAGES: u64 = 200;
const MAX_SEARCH_PARAMS: usize = 32;
const BASE58: &[u8] = b"123456789abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ";
/// `/photos/X` segments that are site pages rather than users.
const RESERVED_USERS: [&str; 5] = ["tags", "upload", "organize", "friends", "search"];
/// Upstream listing extras before `metadata` additions and `url_*` sizes.
const EXTRAS: &str = "description,date_upload,tags,views,media,path_alias,owner_name,";
/// Upstream `metadata: true` additions.
const METADATA_EXTRAS: &str =
    "license,date_taken,original_format,last_update,geo,machine_tags,o_dims";

/// Upstream `FlickrAPI.FORMATS`: size suffix, label and maximum dimension.
const FORMATS: [(&str, &str, Option<u64>); 16] = [
    ("o", "Original", None),
    ("6k", "X-Large 6K", Some(6144)),
    ("5k", "X-Large 5K", Some(5120)),
    ("4k", "X-Large 4K", Some(4096)),
    ("3k", "X-Large 3K", Some(3072)),
    ("k", "Large 2048", Some(2048)),
    ("h", "Large 1600", Some(1600)),
    ("l", "Large", Some(1024)),
    ("c", "Medium 800", Some(800)),
    ("z", "Medium 640", Some(640)),
    ("m", "Medium", Some(500)),
    ("n", "Small 320", Some(320)),
    ("s", "Small", Some(240)),
    ("q", "Large Square", Some(150)),
    ("t", "Thumbnail", Some(100)),
    ("s", "Square", Some(75)),
];

/// Upstream `FlickrAPI.VIDEO_FORMATS` ranks; unknown stream types rank 0.
fn video_rank(kind: &str) -> u8 {
    match kind {
        "orig" => 9,
        "1080p" => 8,
        "720p" => 7,
        "360p" => 6,
        "288p" => 5,
        "700" => 4,
        "300" => 3,
        "100" => 2,
        "appletv" => 1,
        _ => 0,
    }
}

/// Upstream `FlickrAPI.LICENSES`.
fn license_name(id: &str) -> Option<&'static str> {
    Some(match id {
        "0" => "All Rights Reserved",
        "1" => "Attribution-NonCommercial-ShareAlike License",
        "2" => "Attribution-NonCommercial License",
        "3" => "Attribution-NonCommercial-NoDerivs License",
        "4" => "Attribution License",
        "5" => "Attribution-ShareAlike License",
        "6" => "Attribution-NoDerivs License",
        "7" => "No known copyright restrictions",
        "8" => "United States Government Work",
        "9" => "Public Domain Dedication (CC0)",
        "10" => "Public Domain Mark",
        _ => return None,
    })
}

const FILENAME: &str = "{category}_{id}.{extension}";
const USER_DIRECTORY: &[&str] = &["{category}", "{user[username]}"];
pub static IMAGE: Route = Route::new("image", FILENAME, USER_DIRECTORY, "{id}");
pub static ALBUM: Route = Route::new(
    "album",
    FILENAME,
    &[
        "{category}",
        "{user[username]}",
        "Albums",
        "{album[id]} {album[title]}",
    ],
    "a_{album[id]}_{id}",
);
pub static GALLERY: Route = Route::new(
    "gallery",
    FILENAME,
    &[
        "{category}",
        "{user[username]}",
        "Galleries",
        "{gallery[gallery_id]} {gallery[title]}",
    ],
    "g_{gallery[id]}_{id}",
);
pub static GROUP: Route = Route::new(
    "group",
    FILENAME,
    &["{category}", "Groups", "{group[groupname]}"],
    "G_{group[nsid]}_{id}",
);
pub static USER: Route = Route::new("user", FILENAME, USER_DIRECTORY, "u_{user[nsid]}_{id}");
pub static FAVORITE: Route = Route::new(
    "favorite",
    FILENAME,
    &["{category}", "{user[username]}", "Favorites"],
    "f_{user[nsid]}_{id}",
);
pub static SEARCH: Route = Route::new(
    "search",
    FILENAME,
    &["{category}", "Search", "{search[text]}"],
    "s_{search}_{id}",
);

pub fn valid_id(id: &str) -> bool {
    (1..=24).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_digit())
}

/// Path aliases and NSIDs such as `12345678@N00`.
fn valid_user(user: &str) -> bool {
    (1..=64).contains(&user.len())
        && user
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'@' | b'-' | b'_'))
        && !RESERVED_USERS.contains(&user)
}

/// Group path aliases and NSIDs.
fn valid_group(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'@' | b'-' | b'_' | b'.'))
}

/// Decode a `flic.kr/p/` base58 short code into a photo ID.
pub fn decode_base58(code: &str) -> Option<String> {
    if !(1..=12).contains(&code.len()) {
        return None;
    }
    let mut number: u64 = 0;
    for c in code.bytes() {
        let digit = BASE58.iter().position(|&b| b == c)? as u64;
        number = number.checked_mul(58)?.checked_add(digit)?;
    }
    Some(number.to_string())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Photo {
        id: String,
    },
    /// An album, or the user's album list when `id` is absent.
    Album {
        user: String,
        id: Option<String>,
    },
    Gallery {
        user: String,
        id: String,
    },
    Group {
        name: String,
    },
    User {
        user: String,
    },
    Favorite {
        user: String,
    },
    /// Upstream `text.parse_query` pairs: first value per name, in order.
    Search {
        query: Vec<(String, String)>,
    },
}

impl Target {
    pub fn route(&self) -> &'static Route {
        match self {
            Self::Photo { .. } => &IMAGE,
            Self::Album { .. } => &ALBUM,
            Self::Gallery { .. } => &GALLERY,
            Self::Group { .. } => &GROUP,
            Self::User { .. } => &USER,
            Self::Favorite { .. } => &FAVORITE,
            Self::Search { .. } => &SEARCH,
        }
    }
}

fn segments(url: &Url) -> Vec<&str> {
    url.path_segments()
        .map(|s| s.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default()
}

fn plain(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https")
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
}

/// Upstream `text.parse_query`: `+` is a space, names without `=` are
/// skipped and only the first value of each name is kept.
fn parse_query(query: &str) -> Vec<(String, String)> {
    let decode = |text: &str| crate::sites::percent_decode(&text.replace('+', " "));
    let mut pairs: Vec<(String, String)> = Vec::new();
    for part in query.split('&') {
        let Some((name, value)) = part.split_once('=') else {
            continue;
        };
        let name = decode(name);
        if !pairs.iter().any(|(seen, _)| *seen == name) {
            pairs.push((name, decode(value)));
        }
    }
    pairs
}

pub fn target(url: &Url) -> Result<Target> {
    let unsupported = || {
        error(
            "unsupported Flickr URL; expected https://www.flickr.com/photos/USER[/ID|/albums[/ID]|/galleries/ID|/favorites], /groups/NAME, /search/?QUERY or https://flic.kr/p/CODE",
        )
    };
    if !plain(url) {
        return Err(unsupported());
    }
    let parts = segments(url);
    match url.host_str().ok_or_else(unsupported)? {
        "flic.kr" | "www.flic.kr" => match parts[..] {
            ["p", code] => Ok(Target::Photo {
                id: decode_base58(code)
                    .filter(|id| valid_id(id))
                    .ok_or_else(unsupported)?,
            }),
            _ => Err(unsupported()),
        },
        "www.flickr.com" | "flickr.com" | "m.flickr.com" | "secure.flickr.com" => {
            match parts[..] {
                ["groups", name, ..] if valid_group(name) => {
                    return Ok(Target::Group { name: name.into() });
                }
                ["search"] => {
                    let query = parse_query(url.query().unwrap_or_default());
                    if url.query().is_none_or(str::is_empty) || query.len() > MAX_SEARCH_PARAMS {
                        return Err(unsupported());
                    }
                    return Ok(Target::Search { query });
                }
                _ => {}
            }
            let (user, rest) = match parts[..] {
                ["photos", user, ref rest @ ..] if valid_user(user) => (user.to_owned(), rest),
                _ => return Err(unsupported()),
            };
            match rest {
                [] => Ok(Target::User { user }),
                [page] if page.starts_with("page") && valid_id(&page[4..]) => {
                    Ok(Target::User { user })
                }
                [id, ..] if valid_id(id) => Ok(Target::Photo { id: (*id).into() }),
                ["albums" | "sets", id, ..] if valid_id(id) => Ok(Target::Album {
                    user,
                    id: Some((*id).into()),
                }),
                ["albums" | "sets", ..] => Ok(Target::Album { user, id: None }),
                ["galleries", id, ..] if valid_id(id) => Ok(Target::Gallery {
                    user,
                    id: (*id).into(),
                }),
                ["favorites", ..] => Ok(Target::Favorite { user }),
                _ => Err(unsupported()),
            }
        }
        _ => Err(unsupported()),
    }
}

/// Whether a URL is a Flickr page handled by this extractor.
pub fn is_flickr_url(url: &Url) -> bool {
    target(url).is_ok()
}

fn text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.trim().to_owned()),
        Value::Number(n) => Some(n.to_string()),
        Value::Object(map) => map.get("_content").and_then(text),
        _ => None,
    }
    .filter(|s| !s.is_empty())
}

fn field(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(text)
}

/// The `_content` of an API text object, or the value itself.
fn content(value: Option<&Value>) -> Value {
    match value {
        Some(Value::Object(map)) => map.get("_content").cloned().unwrap_or(Value::Null),
        Some(value) => value.clone(),
        None => Value::Null,
    }
}

/// gallery-dl `text.parse_int`: integers from numbers or digit strings, else 0.
fn parse_int(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

fn date(timestamp: Option<i64>) -> Value {
    match timestamp {
        Some(_) => crate::filter::source_date_text(timestamp).into(),
        None => Value::Null,
    }
}

fn timestamp(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// HTTPS media on `*.staticflickr.com`, or `*.flickr.com` for video streams.
pub fn media_url(raw: &str, video: bool) -> Option<Url> {
    let url = Url::parse(raw.trim()).ok()?;
    let host = url.host_str()?;
    (url.scheme() == "https"
        && plain(&url)
        && (host.ends_with(".staticflickr.com")
            || video && (host == "flickr.com" || host.ends_with(".flickr.com"))))
    .then_some(url)
}

/// Upstream `_file_url`: photos get a `_d` (download) suffix before the
/// extension; video streams are used as they are.
pub fn file_url(url: &Url) -> Url {
    if url.as_str().contains("/video/") {
        return url.clone();
    }
    let path = url.path();
    let name_start = path.rfind('/').map_or(0, |i| i + 1);
    match path[name_start..].rfind('.') {
        Some(dot) => {
            let dot = name_start + dot;
            let mut out = url.clone();
            out.set_path(&format!("{}_d{}", &path[..dot], &path[dot..]));
            out
        }
        None => url.clone(),
    }
}

/// The `.flickr.api.site_key` assignment in a Flickr page.
pub fn parse_site_key(page: &str) -> Option<String> {
    crate::sites::extr(page, ".flickr.api.site_key = \"", "\"")
        .filter(|key| {
            (8..=64).contains(&key.len()) && key.bytes().all(|b| b.is_ascii_alphanumeric())
        })
        .map(str::to_owned)
}

/// Parse JSON and map `{"stat":"fail"}` responses without echoing details.
fn check(bytes: &[u8]) -> Result<Value> {
    let body: Value =
        serde_json::from_slice(bytes).map_err(|_| error("Flickr returned invalid JSON"))?;
    if body.get("stat").and_then(Value::as_str) == Some("fail") || body.get("code").is_some() {
        let code = body.get("code").and_then(Value::as_i64).unwrap_or(0);
        return Err(error(match code {
            1 => "Flickr photo, album, gallery, group or user not found".to_owned(),
            2 | 99 => "Flickr refused access to private content".to_owned(),
            98 => "Flickr requires authentication for this content".to_owned(),
            100 => "Flickr rejected the API key; check the api-key setting".to_owned(),
            105 => "Flickr API is temporarily unavailable".to_owned(),
            _ => format!("Flickr API error {code}"),
        }));
    }
    Ok(body)
}

/// Parse a `flickr.urls.lookupUser` response into upstream's user dict.
pub fn parse_user(bytes: &[u8], path_alias: &str) -> Result<Map<String, Value>> {
    let body = check(bytes)?;
    let user = body
        .get("user")
        .ok_or_else(|| error("Flickr user not found"))?;
    let nsid = field(user, "id")
        .filter(|id| valid_user(id))
        .ok_or_else(|| error("Flickr user not found"))?;
    let mut out = Map::new();
    out.insert("nsid".into(), nsid.into());
    out.insert("username".into(), content(user.get("username")));
    out.insert("path_alias".into(), path_alias.into());
    Ok(out)
}

/// Parse a `flickr.urls.lookupGroup` response into upstream's group dict.
pub fn parse_group(bytes: &[u8], path_alias: &str) -> Result<Map<String, Value>> {
    let body = check(bytes)?;
    let group = body
        .get("group")
        .ok_or_else(|| error("Flickr group not found"))?;
    let nsid = field(group, "id")
        .filter(|id| valid_group(id))
        .ok_or_else(|| error("Flickr group not found"))?;
    let mut out = Map::new();
    out.insert("nsid".into(), nsid.into());
    out.insert("path_alias".into(), path_alias.into());
    out.insert("groupname".into(), content(group.get("groupname")));
    Ok(out)
}

/// Upstream `people_getInfo` flattening of `_content` objects.
fn clean_person(mut person: Value) -> Value {
    if let Some(map) = person.as_object_mut() {
        for key in [
            "description",
            "username",
            "realname",
            "location",
            "profileurl",
            "photosurl",
            "mobileurl",
        ] {
            if map.get(key).is_some_and(Value::is_object) {
                let value = content(map.get(key));
                map.insert(key.into(), value);
            }
        }
        if let Some(photos) = map.get_mut("photos").and_then(Value::as_object_mut) {
            for key in ["count", "firstdate", "firstdatetaken"] {
                if photos.get(key).is_some_and(Value::is_object) {
                    let value = content(photos.get(key));
                    photos.insert(key.into(), value);
                }
            }
        }
    }
    person
}

/// Upstream `_clean_info`: flatten `title` and `description`.
fn clean_info(mut info: Map<String, Value>) -> Map<String, Value> {
    for key in ["title", "description"] {
        let value = content(info.get(key));
        info.insert(key.into(), value);
    }
    info
}

/// Pick the upstream size from a `getSizes` response: the last entry after
/// `size-max` truncation, skipping sources outside Flickr's media hosts.
pub fn parse_sizes(bytes: &[u8], max_size: Option<u64>) -> Result<Option<(Url, Value, i64, i64)>> {
    let body = check(bytes)?;
    let sizes = body
        .pointer("/sizes/size")
        .and_then(Value::as_array)
        .ok_or_else(|| error("Flickr response contains no size list"))?;
    let mut sizes: &[Value] = sizes;
    if let Some(max) = max_size {
        let max = max as i64;
        if let Some(cut) = sizes.iter().enumerate().skip(1).position(|(_, size)| {
            parse_int(size.get("width")) > max || parse_int(size.get("height")) > max
        }) {
            sizes = &sizes[..cut + 1];
        }
    }
    Ok(sizes.iter().rev().find_map(|size| {
        let url = media_url(&field(size, "source")?, false)?;
        Some((
            url,
            size.get("label").cloned().unwrap_or(Value::Null),
            parse_int(size.get("width")),
            parse_int(size.get("height")),
        ))
    }))
}

/// Pick upstream's best stream from a `video.getStreamInfo` response.
pub fn parse_stream(bytes: &[u8]) -> Result<Option<(Url, Value)>> {
    let body = check(bytes)?;
    let streams = body
        .pointer("/streams/stream")
        .and_then(Value::as_array)
        .ok_or_else(|| error("Flickr response contains no stream list"))?;
    // Python's `max` keeps the first of equal ranks.
    let best = streams
        .iter()
        .rev()
        .max_by_key(|s| video_rank(s.get("type").and_then(Value::as_str).unwrap_or("")));
    Ok(best.and_then(|stream| {
        let url = media_url(&field(stream, "_content")?, true)?;
        Some((url, stream.get("type").cloned().unwrap_or(Value::Null)))
    }))
}

/// Upstream extractor settings (`extractor.flickr.*`).
#[derive(Clone, Debug)]
pub struct Options {
    pub videos: bool,
    pub exif: bool,
    pub info: bool,
    pub contexts: bool,
    pub profile: bool,
    /// Additional listing extras (`metadata`).
    pub metadata: Option<String>,
    /// Maximum width/height (`size-max`).
    pub max_size: Option<u64>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            videos: true,
            exif: false,
            info: false,
            contexts: false,
            profile: false,
            metadata: None,
            max_size: None,
        }
    }
}

impl Options {
    pub fn parse(map: &Map<String, Value>) -> Result<Self> {
        let mut options = Self::default();
        let boolean = |key: &str, default: bool| -> Result<bool> {
            match map.get(key) {
                None | Some(Value::Null) => Ok(default),
                Some(Value::Bool(value)) => Ok(*value),
                Some(_) => Err(error(format!("Flickr {key} must be a boolean"))),
            }
        };
        options.videos = boolean("videos", true)?;
        options.exif = boolean("exif", false)?;
        options.info = boolean("info", false)?;
        options.contexts = boolean("contexts", false)?;
        options.profile = boolean("profile", false)?;
        let extra = |text: &str| {
            text.len() <= 1024
                && text
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b','))
        };
        options.metadata = match map.get("metadata") {
            None | Some(Value::Null) | Some(Value::Bool(false)) => None,
            Some(Value::Bool(true)) => Some(METADATA_EXTRAS.into()),
            Some(Value::String(s)) if s.is_empty() => None,
            Some(Value::String(s)) if extra(s) => Some(s.clone()),
            Some(Value::Array(items)) => {
                let names: Option<Vec<&str>> = items.iter().map(Value::as_str).collect();
                let joined = names
                    .ok_or_else(|| error("Flickr metadata must list extra names"))?
                    .join(",");
                if !extra(&joined) {
                    return Err(error("Flickr metadata must list extra names"));
                }
                (!joined.is_empty()).then_some(joined)
            }
            Some(Value::Number(n)) if n.as_i64() == Some(0) => None,
            Some(_) => {
                return Err(error(
                    "Flickr metadata must be a boolean, a string or a list of extra names",
                ));
            }
        };
        options.max_size = match map.get("size-max") {
            None | Some(Value::Null) => None,
            Some(Value::Number(n)) => n.as_u64().filter(|v| *v > 0),
            // Unmatched names are ignored, as upstream does after warning.
            Some(Value::String(s)) => FORMATS
                .iter()
                .find(|(fmt, name, _)| s == fmt || s == name)
                .and_then(|(.., width)| *width),
            Some(_) => return Err(error("Flickr size-max must be an integer or format name")),
        };
        Ok(options)
    }

    /// Upstream `self.formats`: sizes within `size-max`, at most eight.
    fn formats(&self) -> Vec<(&'static str, &'static str)> {
        FORMATS
            .iter()
            .filter(|(.., width)| match (self.max_size, width) {
                (Some(max), Some(width)) => *width <= max,
                _ => true,
            })
            .take(8)
            .map(|(fmt, name, _)| (*fmt, *name))
            .collect()
    }

    fn extras(&self) -> String {
        let mut extras = EXTRAS.to_owned();
        if let Some(metadata) = &self.metadata {
            extras.push_str(metadata);
            extras.push(',');
        }
        let sizes: Vec<String> = self
            .formats()
            .iter()
            .map(|(fmt, _)| format!("url_{fmt}"))
            .collect();
        extras.push_str(&sizes.join(","));
        extras
    }
}

/// Upstream `_extract_format` field cleanup that precedes metadata calls.
fn clean_listed(photo: &mut Map<String, Value>) {
    let description = match content(photo.get("description")) {
        Value::String(s) => Value::String(s.trim().to_owned()),
        other => other,
    };
    photo.insert("description".into(), description);
    let views = parse_int(photo.get("views"));
    photo.insert("views".into(), views.into());
    let uploaded = date(timestamp(photo.get("dateupload")));
    photo.insert("date".into(), uploaded);
    let tags: Vec<Value> = photo
        .get("tags")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .split_whitespace()
        .map(Value::from)
        .collect();
    photo.insert("tags".into(), tags.into());
}

/// Upstream `photos_getInfo` cleanup for the `info` setting and image pages.
fn clean_photo_info(photo: &mut Map<String, Value>) {
    let title = content(photo.get("title"));
    photo.insert("title".into(), title);
    let comments = parse_int(Some(&content(photo.get("comments"))));
    photo.insert("comments".into(), comments.into());
    let description = content(photo.get("description"));
    photo.insert("description".into(), description);
    let tags: Vec<Value> = photo
        .get("tags")
        .and_then(|tags| tags.get("tag"))
        .and_then(Value::as_array)
        .map(|tags| tags.iter().filter_map(|t| t.get("raw").cloned()).collect())
        .unwrap_or_default();
    photo.insert("tags".into(), tags.into());
    let views = parse_int(photo.get("views"));
    photo.insert("views".into(), views.into());
}

fn id_value(id: &str) -> Value {
    id.parse::<u64>().map(Value::from).unwrap_or(id.into())
}

pub struct Flickr {
    client: Client,
    network: NetworkOptions,
    api_root: Url,
    web_root: Url,
    /// Configured `api-key`; otherwise the site key is fetched once.
    api_key: Option<String>,
    site_key: tokio::sync::OnceCell<String>,
    options: Options,
    timeout: Duration,
    request_timeout: Duration,
    retries: u8,
    rate_limits: RateLimits,
}

/// One resolved photo listing: its API method, parameters and the keyword
/// data shared by every photo (`user`, `album`, `gallery`, `group`, `search`).
struct Listing {
    route: &'static Route,
    method: &'static str,
    params: Vec<(String, String)>,
    key: &'static str,
    data: Map<String, Value>,
    /// Upstream `self.user`, the owner of photos listed without one.
    user: Option<Value>,
}

struct ListingState {
    listing: Arc<Listing>,
    page: u64,
    index: usize,
    done: bool,
}

impl Flickr {
    pub fn new(timeout: Duration) -> Result<Self> {
        Ok(Self {
            client: NetworkOptions::default().client(USER_AGENT, timeout)?,
            network: NetworkOptions::default(),
            api_root: Url::parse("https://api.flickr.com/").expect("static URL"),
            web_root: Url::parse("https://www.flickr.com/").expect("static URL"),
            api_key: None,
            site_key: tokio::sync::OnceCell::new(),
            options: Options::default(),
            timeout,
            request_timeout: timeout,
            retries: 0,
            rate_limits: RateLimits::default(),
        })
    }

    /// Apply proxy and user-agent settings to API requests and emitted media.
    pub fn with_network(mut self, network: &NetworkOptions) -> Result<Self> {
        self.client = network.client(USER_AGENT, self.timeout)?;
        self.network = network.clone();
        Ok(self)
    }

    /// Use a configured API key instead of the public site key.
    pub fn with_api_key(mut self, key: Option<&str>) -> Self {
        if let Some(key) = key {
            self.api_key = Some(key.into());
        }
        self
    }

    /// Apply upstream extractor settings (`extractor.flickr.*`).
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

    /// Override the API and web origins; intended for local tests.
    pub fn with_api_root(mut self, root: Url) -> Self {
        self.api_root = root.clone();
        self.web_root = root;
        self
    }

    /// Upstream `_extract_apikey`: the public site key from `/prints`,
    /// retrieved once per extractor.
    async fn key(&self) -> Result<&str> {
        if let Some(key) = &self.api_key {
            if key.is_empty() {
                return Err(error("Flickr requires an api-key setting"));
            }
            return Ok(key);
        }
        self.site_key
            .get_or_try_init(|| async {
                let page = self.web_root.join("prints").expect("static Flickr path");
                // The page carries the key even though it answers HTTP 404.
                let body = tokio::time::timeout(self.timeout, self.request_with(&page, true))
                    .await
                    .map_err(|_| error("native Flickr extraction timed out"))??;
                parse_site_key(&String::from_utf8_lossy(&body)).ok_or_else(|| {
                    error("could not retrieve Flickr's public API key; set the api-key setting")
                })
            })
            .await
            .map(String::as_str)
    }

    async fn call(&self, method: &str, query: &[(&str, &str)]) -> Result<Vec<u8>> {
        let key = self.key().await?;
        let mut endpoint = self
            .api_root
            .join("services/rest/")
            .expect("static Flickr path");
        endpoint
            .query_pairs_mut()
            .append_pair("method", method)
            .append_pair("api_key", key)
            .append_pair("format", "json")
            .append_pair("nojsoncallback", "1")
            .extend_pairs(query);
        tokio::time::timeout(self.timeout, self.request(&endpoint))
            .await
            .map_err(|_| error("native Flickr extraction timed out"))?
    }

    async fn api(&self, method: &str, query: &[(&str, &str)]) -> Result<Value> {
        check(&self.call(method, query).await?)
    }

    /// An optional metadata call: API failures are ignored, as upstream
    /// logs them as warnings; transport failures still end extraction.
    async fn optional(&self, method: &str, query: &[(&str, &str)]) -> Result<Option<Value>> {
        Ok(check(&self.call(method, query).await?).ok())
    }

    async fn lookup_user(&self, user: &str) -> Result<Map<String, Value>> {
        let url = format!("https://www.flickr.com/photos/{user}");
        let body = self
            .call("flickr.urls.lookupUser", &[("url", &url)])
            .await?;
        let mut user_data = parse_user(&body, user)?;
        if self.options.profile {
            let nsid = user_data["nsid"].as_str().unwrap_or_default().to_owned();
            if let Some(Value::Object(person)) = self.person(&nsid).await? {
                user_data.extend(person);
            }
        }
        Ok(user_data)
    }

    /// Upstream `people_getInfo`: the cleaned `person` object.
    async fn person(&self, nsid: &str) -> Result<Option<Value>> {
        let body = self
            .api("flickr.people.getInfo", &[("user_id", nsid)])
            .await?;
        Ok(body.get("person").cloned().map(clean_person))
    }

    /// Upstream `_extract_metadata`: optional `info`, `exif` and `contexts`
    /// data, and the license name.
    async fn extract_metadata(&self, photo: &mut Map<String, Value>, info: bool) -> Result<()> {
        let id = photo
            .get("id")
            .map(crate::sites::scalar)
            .unwrap_or_default();
        if info
            && self.options.info
            && let Some(Value::Object(data)) = self
                .optional("flickr.photos.getInfo", &[("photo_id", &id)])
                .await?
                .and_then(|body| body.get("photo").cloned())
        {
            photo.extend(data);
            clean_photo_info(photo);
            photo.insert("id".into(), id_value(&id));
        }
        if self.options.exif
            && let Some(Value::Object(data)) = self
                .optional("flickr.photos.getExif", &[("photo_id", &id)])
                .await?
                .and_then(|body| body.get("photo").cloned())
        {
            photo.extend(data);
        }
        if self.options.contexts
            && let Some(Value::Object(mut data)) = self
                .optional("flickr.photos.getAllContexts", &[("photo_id", &id)])
                .await?
        {
            data.remove("stat");
            photo.extend(data);
        }
        if let Some(license) = photo.get("license").map(crate::sites::scalar) {
            photo.insert("license_name".into(), license_name(&license).into());
        }
        Ok(())
    }

    /// Upstream `_extract_video`: `None` when no stream is available.
    async fn extract_video(&self, photo: &mut Map<String, Value>, id: &str) -> Result<Option<Url>> {
        let secret = match photo.get("secret").and_then(text) {
            Some(secret) => secret,
            None => {
                let info = self
                    .api("flickr.photos.getInfo", &[("photo_id", id)])
                    .await?;
                info.pointer("/photo/secret")
                    .and_then(text)
                    .ok_or_else(|| error("Flickr video has no secret"))?
            }
        };
        let body = self
            .call(
                "flickr.video.getStreamInfo",
                &[("photo_id", id), ("secret", &secret)],
            )
            .await?;
        let Ok(Some((url, kind))) = parse_stream(&body) else {
            return Ok(None);
        };
        photo.insert("url".into(), url.as_str().into());
        photo.insert("label".into(), kind);
        photo.insert("width".into(), 0.into());
        photo.insert("height".into(), 0.into());
        Ok(Some(url))
    }

    /// Upstream `_extract_photo`: the last `getSizes` entry.
    async fn extract_photo(&self, photo: &mut Map<String, Value>, id: &str) -> Result<Option<Url>> {
        let body = self
            .call("flickr.photos.getSizes", &[("photo_id", id)])
            .await?;
        let Ok(Some((url, label, width, height))) = parse_sizes(&body, self.options.max_size)
        else {
            return Ok(None);
        };
        photo.insert("url".into(), url.as_str().into());
        photo.insert("label".into(), label);
        photo.insert("width".into(), width.into());
        photo.insert("height".into(), height.into());
        Ok(Some(url))
    }

    /// Upstream `FlickrImageExtractor.items`.
    async fn image(&self, id: &str) -> Result<Events<'_>> {
        let body = self
            .api("flickr.photos.getInfo", &[("photo_id", id)])
            .await?;
        let Some(Value::Object(mut photo)) = body.get("photo").cloned() else {
            return Err(error("Flickr response contains no photo"));
        };
        if photo.get("id").and_then(text).as_deref() != Some(id) {
            return Err(error("Flickr photo not found"));
        }
        self.extract_metadata(&mut photo, false).await?;
        let video =
            photo.get("media").and_then(Value::as_str) == Some("video") && self.options.videos;
        let url = if video {
            self.extract_video(&mut photo, id).await?
        } else {
            self.extract_photo(&mut photo, id).await?
        }
        .ok_or_else(|| error("Flickr photo contains no supported media"))?;
        let owner = photo.get("owner").cloned().unwrap_or(Value::Null);
        let user = match owner.get("nsid").and_then(Value::as_str) {
            Some(nsid) if self.options.profile && valid_user(nsid) => {
                self.person(nsid).await?.unwrap_or(owner)
            }
            _ => owner,
        };
        photo.insert("user".into(), user);
        clean_photo_info(&mut photo);
        let uploaded = timestamp(photo.get("dateuploaded"));
        photo.insert("date".into(), date(uploaded));
        photo.insert("id".into(), id_value(id));
        if let Some(Value::Object(location)) = photo.get_mut("location") {
            for value in location.values_mut() {
                if value.is_object() {
                    *value = content(Some(value));
                }
            }
        }
        let item = self.media(&IMAGE, photo, &url, video, uploaded, 1);
        Ok(crate::sites::events(self.finish(item)))
    }

    /// Upstream `_extract_format` plus `photo.update(data)`; `None` skips a
    /// photo without supported media, as upstream skips failing photos.
    async fn listed(
        &self,
        listing: &Listing,
        mut photo: Map<String, Value>,
        index: usize,
    ) -> Result<Option<Media>> {
        let Some(id) = photo.get("id").and_then(text).filter(|id| valid_id(id)) else {
            return Ok(None);
        };
        clean_listed(&mut photo);
        self.extract_metadata(&mut photo, true).await?;
        photo.insert("id".into(), id_value(&id));
        if !photo.contains_key("owner") {
            photo.insert("owner".into(), listing.user.clone().unwrap_or(Value::Null));
        } else if !(self.options.info && photo.get("owner").is_some_and(Value::is_object)) {
            let mut owner = Map::new();
            owner.insert(
                "nsid".into(),
                photo.get("owner").cloned().unwrap_or_default(),
            );
            owner.insert(
                "username".into(),
                photo.get("ownername").cloned().unwrap_or_default(),
            );
            owner.insert(
                "path_alias".into(),
                photo.get("pathalias").cloned().unwrap_or_default(),
            );
            photo.insert("owner".into(), owner.into());
        }
        photo.remove("pathalias");
        photo.remove("ownername");
        let video =
            photo.get("media").and_then(Value::as_str) == Some("video") && self.options.videos;
        let url = if video {
            self.extract_video(&mut photo, &id).await?
        } else {
            let max = self.options.max_size.map(|max| max as i64);
            let mut chosen = None;
            for (fmt, name) in self.options.formats() {
                let Some(raw) = photo.get(&format!("url_{fmt}")).and_then(text) else {
                    continue;
                };
                let width = parse_int(photo.get(&format!("width_{fmt}")));
                let height = parse_int(photo.get(&format!("height_{fmt}")));
                photo.insert("width".into(), width.into());
                photo.insert("height".into(), height.into());
                if max.is_some_and(|max| width > max || height > max) {
                    continue;
                }
                let Some(url) = media_url(&raw, false) else {
                    continue;
                };
                photo.insert("url".into(), url.as_str().into());
                photo.insert("label".into(), name.into());
                photo.retain(|key, _| {
                    !(key.starts_with("url_")
                        || key.starts_with("width_")
                        || key.starts_with("height_"))
                });
                chosen = Some(url);
                break;
            }
            match chosen {
                Some(url) => Some(url),
                None => self.extract_photo(&mut photo, &id).await?,
            }
        };
        let Some(url) = url else {
            return Ok(None);
        };
        let uploaded = timestamp(photo.get("dateupload"));
        photo.extend(listing.data.clone());
        Ok(Some(self.media(
            listing.route,
            photo,
            &url,
            video,
            uploaded,
            index,
        )))
    }

    /// Build the emitted file from a photo's keyword dictionary.
    fn media(
        &self,
        route: &'static Route,
        mut fields: Map<String, Value>,
        source: &Url,
        video: bool,
        uploaded: Option<i64>,
        index: usize,
    ) -> Media {
        let url = file_url(source);
        let (name, ext) = crate::sites::nameext(&url);
        let ext = if ext.is_empty() {
            if video { "mp4" } else { "jpg" }.to_owned()
        } else {
            ext
        };
        fields.insert("filename".into(), name.clone().into());
        fields.insert("extension".into(), ext.clone().into());
        fields.insert("category".into(), "flickr".into());
        fields.insert("subcategory".into(), route.subcategory.into());
        let id = fields
            .get("id")
            .map(crate::sites::scalar)
            .unwrap_or_default();
        let username = fields
            .get("user")
            .and_then(|user| field(user, "username"))
            .or_else(|| {
                fields
                    .get("owner")
                    .and_then(|owner| field(owner, "username"))
            });
        let positive = |key: &str| {
            u64::try_from(parse_int(fields.get(key)))
                .ok()
                .filter(|v| *v > 0)
        };
        let file = FileMetadata {
            width: positive("width"),
            height: positive("height"),
            media_type: Some(Arc::from(if video { "video" } else { "photo" })),
            ..FileMetadata::default()
        };
        Media {
            filename: format!("{name}.{ext}"),
            job: MediaJob::DirectHttp,
            category: "flickr".into(),
            metadata: Some(PostMetadata {
                file: Some(file),
                post_id: id.clone(),
                media_id: id,
                username: username.as_deref().map(Arc::from),
                caption: fields.get("title").and_then(text).as_deref().map(Arc::from),
                timestamp: uploaded,
                index,
                site_keywords: Some(Arc::new(SiteKeywords { route, fields })),
                ..PostMetadata::default()
            }),
            url,
            headers: BTreeMap::new(),
            handoff: BTreeMap::new(),
        }
    }

    fn finish(&self, mut item: Media) -> Vec<ExtractionEvent> {
        self.network.apply_media(&mut item);
        group_with_keywords(
            vec![item],
            "flickr",
            KeywordPolicy::shared(&["num", "index", "media_id", "filename", "name", "extension"]),
        )
        .into()
    }

    /// Resolve a listing's owner, metadata and API method.
    async fn listing(&self, target: Target) -> Result<Listing> {
        let route = target.route();
        let mut data = Map::new();
        let mut user = None;
        let mut owner = |data: &mut Map<String, Value>, value: Map<String, Value>| {
            let value = Value::Object(value);
            data.insert("user".into(), value.clone());
            user = Some(value);
        };
        let (method, params, key) = match target {
            Target::Album { user: alias, id } => {
                let id = id.expect("album listing with an ID");
                let resolved = self.lookup_user(&alias).await?;
                let nsid = resolved["nsid"].as_str().unwrap_or_default().to_owned();
                owner(&mut data, resolved);
                // Upstream keeps going with an empty album when its
                // metadata cannot be retrieved.
                let album = self
                    .optional(
                        "flickr.photosets.getInfo",
                        &[("photoset_id", &id), ("user_id", &nsid)],
                    )
                    .await?
                    .and_then(|body| body.get("photoset").and_then(Value::as_object).cloned())
                    .map(clean_info)
                    .unwrap_or_default();
                data.insert("album".into(), album.into());
                (
                    "flickr.photosets.getPhotos",
                    vec![("photoset_id".into(), id)],
                    "photoset",
                )
            }
            Target::Gallery { user: alias, id } => {
                let resolved = self.lookup_user(&alias).await?;
                owner(&mut data, resolved);
                let body = self
                    .api("flickr.galleries.getInfo", &[("gallery_id", &id)])
                    .await?;
                let gallery = body
                    .get("gallery")
                    .and_then(Value::as_object)
                    .cloned()
                    .ok_or_else(|| error("Flickr gallery not found"))?;
                data.insert("gallery".into(), clean_info(gallery).into());
                (
                    "flickr.galleries.getPhotos",
                    vec![("gallery_id".into(), id)],
                    "photos",
                )
            }
            Target::Group { name } => {
                let url = format!("https://www.flickr.com/groups/{name}");
                let body = self
                    .call("flickr.urls.lookupGroup", &[("url", &url)])
                    .await?;
                let group = parse_group(&body, &name)?;
                let nsid = group["nsid"].as_str().unwrap_or_default().to_owned();
                data.insert("group".into(), group.into());
                (
                    "flickr.groups.pools.getPhotos",
                    vec![("group_id".into(), nsid)],
                    "photos",
                )
            }
            Target::User { user: alias } | Target::Favorite { user: alias } => {
                let resolved = self.lookup_user(&alias).await?;
                let nsid = resolved["nsid"].as_str().unwrap_or_default().to_owned();
                owner(&mut data, resolved);
                let method = if route.subcategory == "favorite" {
                    "flickr.favorites.getList"
                } else {
                    "flickr.people.getPhotos"
                };
                (method, vec![("user_id".into(), nsid)], "photos")
            }
            Target::Search { query } => {
                let mut search: Map<String, Value> = query
                    .iter()
                    .map(|(k, v)| (k.clone(), Value::from(v.as_str())))
                    .collect();
                search.entry("text").or_insert_with(|| "".into());
                let params = search
                    .iter()
                    .filter(|(key, _)| {
                        !matches!(
                            key.as_str(),
                            "method"
                                | "api_key"
                                | "format"
                                | "nojsoncallback"
                                | "extras"
                                | "page"
                                | "per_page"
                        )
                    })
                    .map(|(k, v)| (k.clone(), crate::sites::scalar(v)))
                    .collect();
                data.insert("search".into(), search.into());
                ("flickr.photos.search", params, "photos")
            }
            Target::Photo { .. } => unreachable!("single photos are not listings"),
        };
        Ok(Listing {
            route,
            method,
            params,
            key,
            data,
            user,
        })
    }

    /// One page of a photo listing and the next state.
    async fn next_page(
        &self,
        mut state: ListingState,
    ) -> Result<Option<(Events<'_>, ListingState)>> {
        let listing = state.listing.clone();
        let page = (state.page + 1).to_string();
        let per_page = PAGE_SIZE.to_string();
        let extras = self.options.extras();
        let mut query: Vec<(&str, &str)> = listing
            .params
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        query.extend([
            ("extras", extras.as_str()),
            ("per_page", per_page.as_str()),
            ("page", page.as_str()),
        ]);
        let body = self.api(listing.method, &query).await?;
        let list = body
            .get(listing.key)
            .ok_or_else(|| error("Flickr response contains no photo list"))?;
        let photos = list
            .get("photo")
            .and_then(Value::as_array)
            .ok_or_else(|| error("Flickr response contains no photo list"))?;
        state.page += 1;
        let pages = u64::try_from(parse_int(list.get("pages"))).unwrap_or(0);
        let done = photos.is_empty() || state.page >= pages || state.page >= MAX_PAGES;
        let mut events = Vec::new();
        for photo in photos {
            let Some(photo) = photo.as_object().cloned() else {
                continue;
            };
            if let Some(item) = self.listed(&listing, photo, state.index + 1).await? {
                state.index += 1;
                events.extend(self.finish(item));
            }
        }
        state.done = done;
        Ok(Some((crate::sites::events(events), state)))
    }

    /// Upstream `_album_items`: queue each of a user's albums.
    fn album_list(&self, alias: String) -> Events<'_> {
        crate::sites::once(async move {
            let user = Arc::new(self.lookup_user(&alias).await?);
            Ok(crate::sites::paginate(Some(0u64), move |page| {
                let user = user.clone();
                let alias = alias.clone();
                async move {
                    let Some(page) = page else {
                        return Ok(None);
                    };
                    let nsid = user["nsid"].as_str().unwrap_or_default().to_owned();
                    let next = (page + 1).to_string();
                    let body = self
                        .api(
                            "flickr.photosets.getList",
                            &[("user_id", &nsid), ("page", &next)],
                        )
                        .await?;
                    let list = body
                        .get("photosets")
                        .ok_or_else(|| error("Flickr response contains no album list"))?;
                    let albums = list
                        .get("photoset")
                        .and_then(Value::as_array)
                        .ok_or_else(|| error("Flickr response contains no album list"))?;
                    let pages = u64::try_from(parse_int(list.get("pages"))).unwrap_or(0);
                    let more = !albums.is_empty() && page + 1 < pages && page + 1 < MAX_PAGES;
                    let children: Vec<Events<'_>> = albums
                        .iter()
                        .filter_map(|album| {
                            let id = field(album, "id").filter(|id| valid_id(id))?;
                            let mut fields = clean_info(album.as_object()?.clone());
                            fields.insert("user".into(), Value::Object((*user).clone()));
                            let url = Url::parse(&format!(
                                "https://www.flickr.com/photos/{alias}/albums/{id}"
                            ))
                            .ok()?;
                            Some(child_job(
                                ChildMetadata::from_raw("flickr", "album", &fields),
                                move || self.events(&url),
                            ))
                        })
                        .collect();
                    Ok(Some((
                        stream::iter(children).flatten().boxed(),
                        more.then_some(page + 1),
                    )))
                }
            }))
        })
    }

    pub fn stream(&self, url: &Url) -> BoxStream<'_, Result<Media>> {
        crate::event::media_only(self.events(url))
    }

    /// Emit one post boundary per photo; listings paginate lazily and album
    /// lists queue one child job per album.
    pub fn events(&self, url: &Url) -> BoxStream<'_, Result<ExtractionEvent>> {
        let target = match target(url) {
            Ok(target) => target,
            Err(e) => return crate::sites::fail(e),
        };
        match target {
            Target::Photo { id } => crate::sites::once(async move { self.image(&id).await }),
            Target::Album { user, id: None } => self.album_list(user),
            target => crate::sites::once(async move {
                let listing = Arc::new(self.listing(target).await?);
                let state = ListingState {
                    listing,
                    page: 0,
                    index: 0,
                    done: false,
                };
                Ok(crate::sites::paginate(state, move |state| async move {
                    if state.done {
                        return Ok(None);
                    }
                    self.next_page(state).await
                }))
            }),
        }
    }

    async fn request(&self, endpoint: &Url) -> Result<Vec<u8>> {
        self.request_with(endpoint, false).await
    }

    /// `missing_ok` reads the body of 404/410 replies instead of failing.
    async fn request_with(&self, endpoint: &Url, missing_ok: bool) -> Result<Vec<u8>> {
        for attempt in 0..=self.retries {
            match self.attempt(endpoint, attempt, missing_ok).await {
                Ok(body) => return Ok(body),
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
        attempt: u8,
        missing_ok: bool,
    ) -> std::result::Result<Vec<u8>, Failure> {
        let mut target = endpoint.clone();
        let mut redirects = Redirects::new(endpoint);
        let mut response = loop {
            self.rate_limits.wait(&target, self.timeout).await?;
            let response = self
                .client
                .get(target.clone())
                .timeout(self.request_timeout)
                .send()
                .await
                .map_err(|_| Failure::transient("Flickr request failed; check network access"))?;
            match redirects.next(response.url(), response.status(), response.headers())? {
                Some(next) => target = next,
                None => break response,
            }
        };
        let status = response.status();
        if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
            let delay = retry_after(response.headers(), SystemTime::now());
            self.rate_limits
                .defer(endpoint, delay.unwrap_or_else(|| backoff(attempt)));
            return Err(Failure::transient(
                if status == StatusCode::TOO_MANY_REQUESTS {
                    match delay {
                        Some(delay) => {
                            format!("Flickr rate limit; retry after {} seconds", delay.as_secs())
                        }
                        None => "Flickr rate limit; retry later".into(),
                    }
                } else {
                    format!("Flickr returned HTTP {status}")
                },
            ));
        }
        let missing = matches!(status.as_u16(), 404 | 410);
        if missing && !missing_ok {
            return Err(error("Flickr photo, album or user not found").into());
        }
        if matches!(status.as_u16(), 401 | 403) {
            return Err(error("Flickr refused access; check the api-key setting").into());
        }
        if !status.is_success() && !missing {
            return Err(error(format!("Flickr returned HTTP {status}")).into());
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| Failure::transient("could not read Flickr response"))?
        {
            if body.len() + chunk.len() > MAX_RESPONSE {
                return Err(error("Flickr response exceeded 32 MiB").into());
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
mod tests {
    use super::*;
    use serde_json::json;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        path::PathBuf,
        thread::{self, JoinHandle},
    };

    const ORIGINAL: &str = "https://live.staticflickr.com/65535/123_abc_o.png";

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

    fn paths(media: &Media) -> (String, PathBuf) {
        let (file, dir) = crate::sites::default_templates(media).unwrap();
        (
            file.render(media).unwrap(),
            dir.render(media).unwrap().unwrap(),
        )
    }

    fn sizes_body() -> Value {
        json!({"stat": "ok", "sizes": {"size": [
            {"label": "Large", "width": 1024, "height": 768,
             "source": "https://live.staticflickr.com/65535/123_abc_b.jpg", "media": "photo"},
            {"label": "Huge", "width": 9000, "height": 9000,
             "source": "http://live.staticflickr.com/insecure.jpg", "media": "photo"},
            {"label": "Original", "width": "800", "height": "600", "source": ORIGINAL, "media": "photo"},
            {"label": "Evil", "width": 1, "height": 1,
             "source": "https://evil.example/x.jpg", "media": "photo"}]}})
    }

    fn streams_body() -> Value {
        json!({"stat": "ok", "streams": {"stream": [
            {"type": "360p", "_content": "https://live.staticflickr.com/video/1/s/360p.mp4?s=x"},
            {"type": "orig", "_content": "https://live.staticflickr.com/video/1/s/orig.mp4?s=y"},
            {"type": "orig", "_content": "https://live.staticflickr.com/video/1/s/second.mp4"},
            {"type": "1080p", "_content": "https://evil.example/1080p.mp4"}]}})
    }

    #[test]
    fn picks_sizes_streams_and_download_urls() {
        let body = sizes_body().to_string();
        let (url, label, width, height) = parse_sizes(body.as_bytes(), None).unwrap().unwrap();
        assert_eq!(url.as_str(), ORIGINAL);
        assert_eq!((label, width, height), (json!("Original"), 800, 600));
        // `size-max` truncates at the first larger size after the first.
        let (url, ..) = parse_sizes(body.as_bytes(), Some(1024)).unwrap().unwrap();
        assert!(url.as_str().ends_with("_b.jpg"));
        let (url, kind) = parse_stream(streams_body().to_string().as_bytes())
            .unwrap()
            .unwrap();
        assert!(url.as_str().ends_with("/orig.mp4?s=y"), "{url}");
        assert_eq!(kind, "orig");
        assert_eq!(
            file_url(&Url::parse(ORIGINAL).unwrap()).as_str(),
            "https://live.staticflickr.com/65535/123_abc_o_d.png"
        );
        assert_eq!(file_url(&url), url);
        let none = json!({"stat": "ok", "sizes": {"size": [
            {"label": "Original", "source": "https://evil.example/x.jpg"}]}})
        .to_string();
        assert!(parse_sizes(none.as_bytes(), None).unwrap().is_none());
        let fail = |code: i64| {
            let body = json!({"stat": "fail", "code": code, "message": "key secret"}).to_string();
            parse_sizes(body.as_bytes(), None)
                .err()
                .unwrap()
                .to_string()
        };
        assert!(fail(1).contains("not found"));
        assert!(fail(100).contains("api-key"));
        assert!(fail(99).contains("private"));
        assert!(!fail(7).contains("secret"));
        assert!(parse_sizes(b"nope", None).is_err());
        let user = json!({"stat": "ok", "user": {"id": "12@N00", "username": {"_content": "al"}}});
        let user = parse_user(user.to_string().as_bytes(), "alias").unwrap();
        assert_eq!(
            Value::Object(user),
            json!({"nsid": "12@N00", "username": "al", "path_alias": "alias"})
        );
        let group =
            json!({"stat": "ok", "group": {"id": "34@N22", "groupname": {"_content": "Cats"}}});
        let group = parse_group(group.to_string().as_bytes(), "cats").unwrap();
        assert_eq!(
            Value::Object(group),
            json!({"nsid": "34@N22", "path_alias": "cats", "groupname": "Cats"})
        );
    }

    #[test]
    fn parses_options_like_upstream() {
        let defaults = Options::default();
        assert!(defaults.videos && !defaults.exif && !defaults.info && !defaults.contexts);
        assert_eq!(
            defaults.extras(),
            "description,date_upload,tags,views,media,path_alias,owner_name,url_o,url_6k,url_5k,url_4k,url_3k,url_k,url_h,url_l"
        );
        let parse = |value: Value| Options::parse(value.as_object().unwrap());
        let options = parse(json!({"size-max": "Large 1600", "metadata": true,
            "videos": false, "exif": true, "contexts": true, "info": true, "profile": true}))
        .unwrap();
        assert_eq!(options.max_size, Some(1600));
        assert!(!options.videos && options.exif && options.contexts && options.info);
        assert_eq!(
            options.extras(),
            format!("{EXTRAS}{METADATA_EXTRAS},url_o,url_h,url_l,url_c,url_z,url_m,url_n,url_s")
        );
        assert_eq!(
            parse(json!({"size-max": "k"})).unwrap().max_size,
            Some(2048)
        );
        assert_eq!(parse(json!({"size-max": 900})).unwrap().max_size, Some(900));
        assert_eq!(parse(json!({"size-max": "bogus"})).unwrap().max_size, None);
        assert_eq!(
            parse(json!({"metadata": ["license", "geo"]}))
                .unwrap()
                .metadata
                .as_deref(),
            Some("license,geo")
        );
        assert_eq!(
            parse(json!({"metadata": "date_taken"}))
                .unwrap()
                .metadata
                .as_deref(),
            Some("date_taken")
        );
        assert!(parse(json!({"metadata": "a&b=c"})).is_err());
        assert!(parse(json!({"videos": "yes"})).is_err());
        assert!(parse(json!({"size-max": []})).is_err());
    }

    #[test]
    fn classifies_supported_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        let photo = Target::Photo { id: "123".into() };
        assert_eq!(t("https://www.flickr.com/photos/alice/123").unwrap(), photo);
        assert_eq!(
            t("https://flickr.com/photos/1@N00/123/in/album-5/").unwrap(),
            photo
        );
        let album = |id: Option<&str>| Target::Album {
            user: "alice".into(),
            id: id.map(Into::into),
        };
        assert_eq!(
            t("https://www.flickr.com/photos/alice/albums/72157").unwrap(),
            album(Some("72157"))
        );
        assert_eq!(
            t("https://www.flickr.com/photos/alice/sets/72157/").unwrap(),
            album(Some("72157"))
        );
        assert_eq!(
            t("https://www.flickr.com/photos/alice/albums").unwrap(),
            album(None)
        );
        assert_eq!(
            t("https://m.flickr.com/photos/alice/sets/").unwrap(),
            album(None)
        );
        assert_eq!(
            t("https://www.flickr.com/photos/alice/galleries/7215/").unwrap(),
            Target::Gallery {
                user: "alice".into(),
                id: "7215".into()
            }
        );
        assert_eq!(
            t("https://www.flickr.com/photos/alice/favorites").unwrap(),
            Target::Favorite {
                user: "alice".into()
            }
        );
        assert_eq!(
            t("https://secure.flickr.com/groups/cat-pics/pool/").unwrap(),
            Target::Group {
                name: "cat-pics".into()
            }
        );
        assert_eq!(
            t("https://flickr.com/search/?text=red+fox&tags=a%20b&text=x&flag").unwrap(),
            Target::Search {
                query: vec![
                    ("text".into(), "red fox".into()),
                    ("tags".into(), "a b".into())
                ]
            }
        );
        let user = Target::User {
            user: "alice".into(),
        };
        assert_eq!(t("https://www.flickr.com/photos/alice/").unwrap(), user);
        assert_eq!(
            t("https://www.flickr.com/photos/alice/page2").unwrap(),
            user
        );
        // Upstream's example: flic.kr/p/FPVo9U decodes to 26140204724.
        assert_eq!(decode_base58("FPVo9U").as_deref(), Some("26140204724"));
        assert_eq!(
            t("https://flic.kr/p/FPVo9U").unwrap(),
            Target::Photo {
                id: "26140204724".into()
            }
        );
        for raw in [
            "https://www.flickr.com/",
            "https://www.flickr.com/photos/tags/cat",
            "https://www.flickr.com/photos/alice/galleries",
            "https://www.flickr.com/photos/alice/galleries/x",
            "https://www.flickr.com/groups/",
            "https://www.flickr.com/groups/a%20b",
            "https://www.flickr.com/search/",
            "https://www.flickr.com/search/?",
            "https://flic.kr/p/0OIl",
            "https://live.staticflickr.com/65535/1_a_o.jpg",
            "https://www.flickr.com:444/photos/alice",
            "https://www.flickr.com.evil.example/photos/alice",
            "https://evilflickr.com/photos/alice",
        ] {
            assert!(t(raw).is_err(), "{raw}");
        }
        assert!(media_url("https://farm1.staticflickr.com/a.jpg", false).is_some());
        assert!(media_url("https://www.flickr.com/play/x", false).is_none());
        assert!(media_url("https://www.flickr.com/play/x", true).is_some());
        assert!(media_url("https://flickr.com.evil.example/x", true).is_none());
        let descriptor = |raw: &str| {
            crate::source_archive::for_url(&Url::parse(raw).unwrap())
                .unwrap()
                .format
        };
        assert_eq!(
            descriptor("https://www.flickr.com/photos/a/galleries/1"),
            "g_{gallery[id]}_{id}"
        );
        assert_eq!(
            descriptor("https://www.flickr.com/groups/a"),
            "G_{group[nsid]}_{id}"
        );
        assert_eq!(
            descriptor("https://www.flickr.com/photos/a/favorites"),
            "f_{user[nsid]}_{id}"
        );
        assert_eq!(
            descriptor("https://www.flickr.com/search/?text=a"),
            "s_{search}_{id}"
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

    fn ok(body: Value) -> Vec<u8> {
        response("200 OK", body.to_string().as_bytes())
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

    fn public(root: Url) -> Flickr {
        Flickr::new(Duration::from_secs(5))
            .unwrap()
            .with_network(&NetworkOptions::new(Some(String::new()), None).unwrap())
            .unwrap()
            .with_api_root(root)
    }

    fn flickr(root: Url) -> Flickr {
        public(root).with_api_key(Some("k"))
    }

    fn with(root: Url, options: Value) -> Flickr {
        flickr(root)
            .with_options(Some(options.as_object().unwrap()))
            .unwrap()
    }

    async fn collect(native: &Flickr, raw: &str) -> Vec<Media> {
        native
            .stream(&Url::parse(raw).unwrap())
            .map(|m| m.unwrap())
            .collect()
            .await
    }

    fn info_body() -> Value {
        json!({"stat": "ok", "photo": {"id": "123", "secret": "s3", "media": "photo",
            "dateuploaded": "1700000000", "views": "17", "license": "4",
            "owner": {"nsid": "1@N00", "username": "alice", "path_alias": "alice"},
            "title": {"_content": "T"}, "description": {"_content": "D"},
            "comments": {"_content": "3"},
            "tags": {"tag": [{"raw": "Red Fox", "_content": "redfox"}]},
            "location": {"locality": {"_content": "Oslo"}, "latitude": "59.9"}}})
    }

    #[tokio::test]
    async fn fetches_image_with_retries_api_key_and_upstream_keywords() {
        let (root, server) = serve(vec![
            response("503 Service Unavailable", b""),
            ok(info_body()),
            ok(sizes_body()),
        ]);
        let events: Vec<_> = flickr(root)
            .with_retries(1)
            .with_api_key(Some("secret"))
            .events(&Url::parse("https://www.flickr.com/photos/alice/123").unwrap())
            .collect()
            .await;
        assert_eq!(events.len(), 3);
        let ExtractionEvent::Post(post) = events[0].as_ref().unwrap() else {
            panic!("missing source post")
        };
        let KeywordPolicy::Shared { file_fields, .. } = &post.keywords else {
            panic!("Flickr must reuse Directory keywords")
        };
        assert_eq!(
            file_fields.as_ref(),
            &["num", "index", "media_id", "filename", "name", "extension"]
        );
        let ExtractionEvent::Media(media) = events[1].as_ref().unwrap() else {
            panic!("missing media")
        };
        assert_eq!(
            media.url.as_str(),
            "https://live.staticflickr.com/65535/123_abc_o_d.png"
        );
        let k = keywords(media);
        assert_eq!(k["id"], json!(123));
        assert_eq!((&k["title"], &k["description"]), (&json!("T"), &json!("D")));
        assert_eq!((&k["comments"], &k["views"]), (&json!(3), &json!(17)));
        assert_eq!(k["tags"], json!(["Red Fox"]));
        assert_eq!(k["date"], "2023-11-14 22:13:20");
        assert_eq!(k["user"]["username"], "alice");
        assert_eq!(k["location"]["locality"], "Oslo");
        assert_eq!(k["license_name"], "Attribution License");
        assert_eq!(
            (&k["label"], &k["width"]),
            (&json!("Original"), &json!(800))
        );
        assert_eq!(
            (&k["filename"], &k["extension"]),
            (&json!("123_abc_o_d"), &json!("png"))
        );
        assert_eq!(
            (&k["category"], &k["subcategory"]),
            (&json!("flickr"), &json!("image"))
        );
        assert_eq!(
            paths(media),
            (
                "flickr_123.png".into(),
                PathBuf::from("flickr").join("alice")
            )
        );
        let meta = media.metadata.as_ref().unwrap();
        assert_eq!(
            (meta.post_id.as_str(), meta.username.as_deref()),
            ("123", Some("alice"))
        );
        assert_eq!(meta.timestamp, Some(1700000000));
        let key = media.archive_key().unwrap();
        assert!(key.starts_with("flickr:"), "{key}");
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with(
            "GET /services/rest/?method=flickr.photos.getInfo&api_key=secret&format=json&nojsoncallback=1&photo_id=123 "
        ), "{}", requests[1]);
        assert!(requests[2].contains("method=flickr.photos.getSizes"));
    }

    #[tokio::test]
    async fn image_videos_use_stream_info_unless_disabled() {
        let mut info = info_body();
        info["photo"]["media"] = json!("video");
        let (root, server) = serve(vec![ok(info.clone()), ok(streams_body())]);
        let media = collect(&flickr(root), "https://flic.kr/p/38").await;
        assert_eq!(media.len(), 1);
        assert_eq!(
            media[0].url.as_str(),
            "https://live.staticflickr.com/video/1/s/orig.mp4?s=y"
        );
        let k = keywords(&media[0]);
        assert_eq!((&k["label"], &k["width"]), (&json!("orig"), &json!(0)));
        assert_eq!(media[0].filename, "orig.mp4");
        let file = media[0].metadata.as_ref().unwrap().file.as_ref().unwrap();
        assert_eq!(file.media_type.as_deref(), Some("video"));
        let requests = server.join().unwrap();
        assert!(requests[0].contains("&photo_id=123 "), "{}", requests[0]);
        assert!(requests[1].contains("method=flickr.video.getStreamInfo"));
        assert!(requests[1].contains("&photo_id=123&secret=s3 "));
        // `videos: false` downloads the poster image instead.
        let (root, server) = serve(vec![ok(info), ok(sizes_body())]);
        let media = collect(
            &with(root, json!({"videos": false})),
            "https://www.flickr.com/photos/alice/123",
        )
        .await;
        assert!(media[0].url.as_str().ends_with("_o_d.png"));
        assert!(server.join().unwrap()[1].contains("method=flickr.photos.getSizes"));
    }

    fn listed(id: &str) -> Value {
        json!({"id": id, "owner": "1@N00", "ownername": "Alice", "pathalias": "alice",
            "secret": "s", "title": format!("t{id}"), "description": {"_content": " d "},
            "dateupload": "1700000000", "tags": "a b", "views": "5", "media": "photo",
            "url_k": format!("https://live.staticflickr.com/{id}_k.jpg"),
            "width_k": "2048", "height_k": 1536,
            "url_o": format!("https://live.staticflickr.com/{id}_o.jpg"),
            "width_o": 4000, "height_o": 3000})
    }

    fn user_body() -> Vec<u8> {
        ok(json!({"stat": "ok", "user": {"id": "1@N00", "username": {"_content": "alice"}}}))
    }

    #[tokio::test]
    async fn paginates_user_photostream_and_resolves_videos() {
        let (root, server) = serve(vec![
            response(
                "404 Not Found",
                b"<script>root.flickr.api.site_key = \"0123abcdef4567\";</script>",
            ),
            user_body(),
            ok(json!({"stat": "ok", "photos": {"page": 1, "pages": 2,
                "photo": [listed("1"), {"id": "2", "media": "video", "secret": "v",
                    "owner": "1@N00", "ownername": "Alice", "pathalias": "alice",
                    "description": {"_content": ""}, "tags": "", "views": 0, "dateupload": 5}]}})),
            ok(streams_body()),
            ok(json!({"stat": "ok", "photos": {"page": 2, "pages": 2, "photo": [listed("3")]}})),
        ]);
        let native = public(root);
        let media = collect(&native, "https://www.flickr.com/photos/alice").await;
        assert_eq!(media.len(), 3);
        assert_eq!(
            media[0].url.as_str(),
            "https://live.staticflickr.com/1_o_d.jpg"
        );
        assert_eq!(media[1].filename, "orig.mp4");
        let indexes: Vec<_> = media
            .iter()
            .map(|m| m.metadata.as_ref().unwrap().index)
            .collect();
        assert_eq!(indexes, [1, 2, 3]);
        let k = keywords(&media[0]);
        assert_eq!(k["id"], json!(1));
        assert_eq!(k["description"], "d");
        assert_eq!(k["tags"], json!(["a", "b"]));
        assert_eq!(k["views"], json!(5));
        assert_eq!(k["date"], "2023-11-14 22:13:20");
        assert_eq!(
            k["owner"],
            json!({"nsid": "1@N00", "username": "Alice", "path_alias": "alice"})
        );
        assert_eq!(
            k["user"],
            json!({"nsid": "1@N00", "username": "alice", "path_alias": "alice"})
        );
        assert_eq!(
            (&k["label"], &k["width"]),
            (&json!("Original"), &json!(4000))
        );
        assert!(!k.contains_key("url_k") && !k.contains_key("pathalias"));
        assert_eq!(k["subcategory"], "user");
        assert_eq!(
            paths(&media[2]),
            ("flickr_3.jpg".into(), PathBuf::from("flickr").join("alice"))
        );
        let mut requests = server.join().unwrap();
        // The public site key is read once from `/prints`.
        assert!(requests.remove(0).starts_with("GET /prints "));
        assert!(
            requests
                .iter()
                .all(|r| r.contains("&api_key=0123abcdef4567&"))
        );
        assert!(requests[0].contains("method=flickr.urls.lookupUser"));
        assert!(requests[1].contains("method=flickr.people.getPhotos"));
        assert!(requests[1].contains("&user_id=1%40N00&extras=description%2Cdate_upload"));
        assert!(requests[1].contains("url_o%2Curl_6k"));
        assert!(requests[1].contains("&per_page=500&page=1 "));
        assert!(requests[2].contains("&photo_id=2&secret=v "));
        assert!(requests[3].contains("&page=2 "));
        assert_eq!(parse_site_key("site_key = \"x\""), None);
        assert_eq!(
            parse_site_key(".flickr.api.site_key = \"a b c d e f\""),
            None
        );
    }

    #[tokio::test]
    async fn album_carries_album_metadata_and_options() {
        let mut photo = listed("7");
        photo.as_object_mut().unwrap().remove("owner");
        photo["license"] = json!("9");
        let (root, server) = serve(vec![
            user_body(),
            ok(
                json!({"stat": "ok", "photoset": {"id": "5", "title": {"_content": "Trip"},
                "description": {"_content": "Fjords"}, "photos": 1}}),
            ),
            ok(json!({"stat": "ok", "photoset": {"page": 1, "pages": 1, "photo": [photo]}})),
            ok(json!({"stat": "ok", "photo": {"id": "7", "camera": "X100",
                "exif": [{"tag": "Model", "raw": {"_content": "X100"}}]}})),
            ok(json!({"stat": "ok", "set": [{"id": "5", "title": "Trip"}], "pool": []})),
        ]);
        let native = with(
            root,
            json!({"exif": true, "contexts": true, "size-max": 3000, "metadata": ["license"]}),
        );
        let media = collect(&native, "https://www.flickr.com/photos/alice/albums/5").await;
        assert_eq!(media.len(), 1);
        // `size-max` skips the 4000px original for the listed 2048px size.
        assert_eq!(
            media[0].url.as_str(),
            "https://live.staticflickr.com/7_k_d.jpg"
        );
        let k = keywords(&media[0]);
        assert_eq!(
            k["album"],
            json!({"id": "5", "title": "Trip", "description": "Fjords", "photos": 1})
        );
        assert_eq!(k["owner"], k["user"]);
        assert_eq!(k["camera"], "X100");
        assert_eq!(k["set"][0]["id"], "5");
        assert!(!k.contains_key("stat"));
        assert_eq!(k["license_name"], "Public Domain Dedication (CC0)");
        assert_eq!(
            (&k["label"], &k["height"]),
            (&json!("Large 2048"), &json!(1536))
        );
        assert_eq!(k["subcategory"], "album");
        assert_eq!(
            paths(&media[0]),
            (
                "flickr_7.jpg".into(),
                PathBuf::from("flickr")
                    .join("alice")
                    .join("Albums")
                    .join("5 Trip")
            )
        );
        // Archive identity stays the photo ID across classes.
        let mut other = media[0].clone();
        other.metadata.as_mut().unwrap().site_keywords = None;
        assert_eq!(other.archive_key(), media[0].archive_key());
        let requests = server.join().unwrap();
        assert!(requests[1].contains("method=flickr.photosets.getInfo&"));
        assert!(requests[1].contains("&photoset_id=5&user_id=1%40N00 "));
        assert!(requests[2].contains("&photoset_id=5&extras="));
        assert!(requests[2].contains("owner_name%2Clicense%2Curl_o%2Curl_k%2Curl_h"));
        assert!(!requests[2].contains("url_3k"));
        assert!(requests[3].contains("method=flickr.photos.getExif&"));
        assert!(requests[4].contains("method=flickr.photos.getAllContexts&"));
    }

    #[tokio::test]
    async fn info_and_profile_settings_merge_upstream_data() {
        let mut info = info_body();
        info["photo"]["id"] = json!("1");
        let (root, server) = serve(vec![
            user_body(),
            ok(json!({"stat": "ok", "person": {"nsid": "1@N00",
                "realname": {"_content": "Alice A"}, "photos": {"count": {"_content": 9}}}})),
            ok(json!({"stat": "ok", "photos": {"page": 1, "pages": 1, "photo": [listed("1")]}})),
            ok(info),
        ]);
        let native = with(root, json!({"info": true, "profile": true}));
        let media = collect(&native, "https://www.flickr.com/photos/alice").await;
        let k = keywords(&media[0]);
        assert_eq!(k["user"]["realname"], "Alice A");
        assert_eq!(k["user"]["photos"]["count"], 9);
        assert_eq!(k["user"]["path_alias"], "alice");
        assert_eq!((&k["title"], &k["comments"]), (&json!("T"), &json!(3)));
        assert_eq!(k["tags"], json!(["Red Fox"]));
        assert_eq!(k["id"], json!(1));
        // `info` keeps the photo's own owner object.
        assert_eq!(k["owner"]["nsid"], "1@N00");
        assert_eq!(k["owner"]["username"], "alice");
        let requests = server.join().unwrap();
        assert!(requests[1].contains("method=flickr.people.getInfo&"));
        assert!(requests[3].contains("method=flickr.photos.getInfo&"));
        assert!(requests[3].contains("&photo_id=1 "));
    }

    #[tokio::test]
    async fn album_metadata_failures_leave_an_empty_album() {
        let (root, server) = serve(vec![
            user_body(),
            ok(json!({"stat": "fail", "code": 1, "message": "nope"})),
            ok(json!({"stat": "ok", "photoset": {"page": 1, "pages": 1, "photo": [listed("8")]}})),
        ]);
        let media = collect(&flickr(root), "https://www.flickr.com/photos/alice/sets/5").await;
        assert_eq!(keywords(&media[0])["album"], json!({}));
        assert_eq!(server.join().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn gallery_group_favorite_and_search_listings() {
        let page = |photo: Value| {
            ok(json!({"stat": "ok", "photos": {"page": 1, "pages": 1, "photo": [photo]}}))
        };
        let (root, server) = serve(vec![
            user_body(),
            ok(
                json!({"stat": "ok", "gallery": {"id": "9-72", "gallery_id": "72",
                "title": {"_content": "Best"}, "description": {"_content": ""}}}),
            ),
            page(listed("1")),
            ok(json!({"stat": "ok", "group": {"id": "34@N22", "groupname": {"_content": "Cats"}}})),
            page(listed("2")),
            user_body(),
            page(listed("3")),
            page(listed("4")),
        ]);
        let native = flickr(root);
        let gallery = collect(&native, "https://www.flickr.com/photos/alice/galleries/72/").await;
        let k = keywords(&gallery[0]);
        assert_eq!(k["gallery"]["title"], "Best");
        assert_eq!(k["subcategory"], "gallery");
        assert_eq!(
            paths(&gallery[0]).1,
            PathBuf::from("flickr")
                .join("alice")
                .join("Galleries")
                .join("72 Best")
        );
        let group = collect(&native, "https://www.flickr.com/groups/cats/").await;
        let k = keywords(&group[0]);
        assert_eq!(
            k["group"],
            json!({"nsid": "34@N22", "path_alias": "cats", "groupname": "Cats"})
        );
        assert!(!k.contains_key("user"));
        assert_eq!(
            paths(&group[0]).1,
            PathBuf::from("flickr").join("Groups").join("Cats")
        );
        let favorites = collect(&native, "https://www.flickr.com/photos/alice/favorites").await;
        assert_eq!(keywords(&favorites[0])["subcategory"], "favorite");
        assert_eq!(
            paths(&favorites[0]).1,
            PathBuf::from("flickr").join("alice").join("Favorites")
        );
        let search = collect(&native, "https://flickr.com/search/?tags=fox&method=x").await;
        let k = keywords(&search[0]);
        assert_eq!(
            k["search"],
            json!({"tags": "fox", "method": "x", "text": ""})
        );
        assert_eq!(paths(&search[0]).1, PathBuf::from("flickr").join("Search"));
        let requests = server.join().unwrap();
        assert!(requests[1].contains("method=flickr.galleries.getInfo&"));
        assert!(requests[1].contains("&gallery_id=72 "));
        assert!(requests[2].contains("method=flickr.galleries.getPhotos&"));
        assert!(requests[3].contains("method=flickr.urls.lookupGroup&"));
        assert!(requests[3].contains("&url=https%3A%2F%2Fwww.flickr.com%2Fgroups%2Fcats "));
        assert!(requests[4].contains("method=flickr.groups.pools.getPhotos&"));
        assert!(requests[4].contains("&group_id=34%40N22&"));
        assert!(requests[6].contains("method=flickr.favorites.getList&"));
        assert!(requests[7].starts_with("GET /services/rest/?method=flickr.photos.search&"));
        assert!(requests[7].contains("&tags=fox&text=&extras="));
        assert_eq!(requests[7].matches("method=").count(), 1);
    }

    #[tokio::test]
    async fn album_list_queues_each_album() {
        let (root, server) = serve(vec![
            user_body(),
            ok(
                json!({"stat": "ok", "photosets": {"page": 1, "pages": 2, "photoset": [
                {"id": "5", "title": {"_content": "Trip"}, "description": {"_content": ""}},
                {"id": "bad", "title": {"_content": "x"}, "description": {"_content": ""}}]}}),
            ),
            ok(
                json!({"stat": "ok", "photosets": {"page": 2, "pages": 2, "photoset": [
                {"id": "6", "title": {"_content": "Home"}, "description": {"_content": ""}}]}}),
            ),
        ]);
        let native = flickr(root);
        let mut events =
            native.events(&Url::parse("https://www.flickr.com/photos/alice/albums").unwrap());
        let mut children = 0;
        while let Some(event) = events.next().await {
            if let ExtractionEvent::BeginChild(scope) = event.unwrap() {
                scope.control.cancel();
                children += 1;
            }
        }
        assert_eq!(children, 2);
        let requests = server.join().unwrap();
        assert!(requests[1].contains("method=flickr.photosets.getList&"));
        assert!(requests[1].contains("&user_id=1%40N00&page=1 "));
        assert!(requests[2].contains("&page=2 "));
    }

    #[tokio::test]
    async fn api_failures_do_not_leak_key() {
        let (root, server) = serve(vec![ok(
            json!({"stat": "fail", "code": 100, "message": "Invalid API Key (secret-key)"}),
        )]);
        let error = flickr(root)
            .with_retries(3)
            .with_api_key(Some("secret-key"))
            .stream(&Url::parse("https://www.flickr.com/photos/alice/albums/5").unwrap())
            .collect::<Vec<_>>()
            .await
            .remove(0)
            .unwrap_err()
            .to_string();
        assert!(error.contains("api-key"));
        assert!(!error.contains("secret-key") && !error.contains("127.0.0.1"));
        let requests = server.join().unwrap();
        assert!(requests[0].contains("method=flickr.urls.lookupUser&"));
    }
}
