//! Experimental native Flickr photo, album and photostream extraction.
//!
//! Mirrors gallery_dl/extractor/flickr.py using the public REST API
//! (`format=json&nojsoncallback=1`) with upstream's default API key unless
//! the `api-key` setting overrides it. Single photos use
//! `flickr.photos.getInfo` and `flickr.photos.getSizes`; albums and
//! photostreams list photos with `url_*` extras and only call `getSizes` for
//! videos. Media must be served over HTTPS from `*.staticflickr.com` or, for
//! video streams, `*.flickr.com`.
use crate::sites::{Route, SiteKeywords, gdl};
use crate::{
    Result, error,
    event::{ExtractionEvent, KeywordPolicy, group_with_keywords},
    extract::{Media, MediaJob, PostMetadata},
    file_metadata::FileMetadata,
    network::NetworkOptions,
    transport::{RateLimits, Redirects, retry_after},
};
use futures_util::{
    StreamExt,
    stream::{self, BoxStream},
};
use reqwest::{Client, StatusCode};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::Arc,
    time::{Duration, SystemTime},
};
use url::Url;

const USER_AGENT: &str = concat!(
    "gallery-dl-rs/",
    env!("CARGO_PKG_VERSION"),
    " (native Flickr extractor)"
);
/// Upstream gallery-dl's default public API key.
pub const DEFAULT_API_KEY: &str = "90c368449018a0cb880ea4889cbb8681";
const MAX_RESPONSE: usize = 32 * 1024 * 1024;
const PAGE_SIZE: usize = 500;
const MAX_PAGES: u64 = 200;
/// Upstream `_pagination` extras before `metadata` and `url_*` formats.
const EXTRAS: &str = "description,date_upload,tags,views,media,path_alias,owner_name,";
/// Upstream `metadata: true` extras.
const METADATA_EXTRAS: &str =
    "license,date_taken,original_format,last_update,geo,machine_tags,o_dims";
const FILENAME: &str = "{category}_{id}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{user[username]}"];
static IMAGE: Route = Route::new("image", FILENAME, DIRECTORY, "{id}");
static ALBUM: Route = Route::new(
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
static GALLERY: Route = Route::new(
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
static GROUP: Route = Route::new(
    "group",
    FILENAME,
    &["{category}", "Groups", "{group[groupname]}"],
    "G_{group[nsid]}_{id}",
);
static USER: Route = Route::new("user", FILENAME, DIRECTORY, "u_{user[nsid]}_{id}");
static FAVORITE: Route = Route::new(
    "favorite",
    FILENAME,
    &["{category}", "{user[username]}", "Favorites"],
    "f_{user[nsid]}_{id}",
);
static SEARCH: Route = Route::new(
    "search",
    FILENAME,
    &["{category}", "Search", "{search[text]}"],
    "s_{search}_{id}",
);
/// Upstream `VIDEO_FORMATS` stream ranks.
fn stream_rank(kind: &str) -> u8 {
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
/// Upstream `LICENSES`.
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

/// Upstream per-photo metadata options.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MetaOptions {
    /// `info`: merge `photos.getInfo` into listed photos.
    pub info: bool,
    /// `exif`: merge `photos.getExif` (`exif`, `camera`).
    pub exif: bool,
    /// `contexts`: merge `photos.getAllContexts` (`set`, `pool`).
    pub contexts: bool,
    /// `profile`: replace `user` with `people.getInfo`.
    pub profile: bool,
    /// `videos`: download video streams rather than their previews.
    pub videos: bool,
    /// `metadata`: additional listing extras.
    pub extras: Option<String>,
}

impl Default for MetaOptions {
    fn default() -> Self {
        Self {
            info: false,
            exif: false,
            contexts: false,
            profile: false,
            videos: true,
            extras: None,
        }
    }
}

impl MetaOptions {
    /// Read the upstream option names from Flickr's site options;
    /// `metadata` and `videos` fall back to inherited typed settings.
    pub fn from_options(
        options: Option<&Map<String, Value>>,
        metadata: Option<bool>,
        videos: Option<bool>,
    ) -> Self {
        let get = |key: &str| options.and_then(|o| o.get(key));
        let flag = |key: &str, default: bool| get(key).map_or(default, crate::postprocess::truthy);
        let extras = match get("metadata") {
            Some(Value::Array(items)) => Some(
                items
                    .iter()
                    .map(|item| {
                        item.as_str()
                            .map_or_else(|| item.to_string(), str::to_owned)
                    })
                    .collect::<Vec<_>>()
                    .join(","),
            ),
            Some(Value::String(extras)) if !extras.is_empty() => Some(extras.clone()),
            Some(value) if crate::postprocess::truthy(value) => Some(METADATA_EXTRAS.into()),
            Some(_) => None,
            None => metadata.unwrap_or(false).then(|| METADATA_EXTRAS.into()),
        };
        Self {
            info: flag("info", false),
            exif: flag("exif", false),
            contexts: flag("contexts", false),
            profile: flag("profile", false),
            videos: flag("videos", videos.unwrap_or(true)),
            extras,
        }
    }
}
/// Upstream `FORMATS`: size suffix, label and maximum dimension, largest
/// first (`None` for originals).
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

/// Upstream `size-max`: the largest allowed image dimension and the (at
/// most eight) listing formats requested as `url_*` extras.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SizeLimit {
    max: Option<u64>,
    formats: Vec<&'static str>,
}

impl Default for SizeLimit {
    fn default() -> Self {
        Self::new(None)
    }
}

impl SizeLimit {
    pub fn new(max: Option<u64>) -> Self {
        let formats = FORMATS
            .iter()
            .filter(|(_, _, width)| max.is_none() || width.is_none_or(|w| Some(w) <= max))
            .take(8)
            .map(|(code, ..)| *code)
            .collect();
        Self { max, formats }
    }

    /// An integer, or a format code or label such as `"l"` or `"Medium"`;
    /// unknown names disable the limit like upstream.
    pub fn from_option(value: Option<&Value>) -> Self {
        let max = match value {
            Some(Value::Number(n)) => n.as_u64(),
            Some(Value::String(name)) => FORMATS
                .iter()
                .find(|(code, label, _)| name == code || name == label)
                .and_then(|(.., width)| *width),
            _ => None,
        };
        Self::new(max.filter(|max| *max > 0))
    }

    fn exceeds(&self, width: Option<u64>, height: Option<u64>) -> bool {
        self.max
            .is_some_and(|max| width.unwrap_or(0) > max || height.unwrap_or(0) > max)
    }

    #[cfg(test)]
    fn extras(&self) -> String {
        self.extras_with(None)
    }

    /// Upstream `_pagination` extras, with `metadata` includes.
    fn extras_with(&self, includes: Option<&str>) -> String {
        let mut extras = EXTRAS.to_owned();
        if let Some(includes) = includes {
            extras.push_str(includes);
            extras.push(',');
        }
        let formats: Vec<_> = self
            .formats
            .iter()
            .map(|code| format!("url_{code}"))
            .collect();
        extras.push_str(&formats.join(","));
        extras
    }
}
const BASE58: &[u8] = b"123456789abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ";
/// `/photos/X` segments that are site pages rather than users.
const RESERVED_USERS: [&str; 5] = ["tags", "upload", "organize", "friends", "search"];

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
    Album {
        user: String,
        id: String,
    },
    User {
        user: String,
    },
    /// A user's albums, queued as album children.
    Albums {
        user: String,
    },
    Favorites {
        user: String,
    },
    Gallery {
        user: String,
        id: String,
    },
    Group {
        name: String,
    },
    /// `photos.search` parameters from the search page's query.
    Search {
        params: Vec<(String, String)>,
    },
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

pub fn target(url: &Url) -> Result<Target> {
    let unsupported = || {
        error(
            "unsupported Flickr URL; expected https://www.flickr.com/photos/USER[/ID|/albums/ID] or https://flic.kr/p/CODE",
        )
    };
    if !plain(url) {
        return Err(unsupported());
    }
    let parts = segments(url);
    // Upstream also reads the photo ID of a static file URL
    // (`HOST.static[.]flickr.com/SERVER[/...]/ID_SECRET_SIZE.EXT`).
    if let Some(host) = url.host_str()
        && let Some(label) = host
            .strip_suffix(".staticflickr.com")
            .or_else(|| host.strip_suffix(".static.flickr.com"))
        && !label.is_empty()
        && label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        && let [servers @ .., file] = parts.as_slice()
        && !servers.is_empty()
        && servers
            .iter()
            .all(|s| s.bytes().all(|b| b.is_ascii_digit()))
    {
        let id: String = file.chars().take_while(char::is_ascii_digit).collect();
        return if valid_id(&id) {
            Ok(Target::Photo { id })
        } else {
            Err(unsupported())
        };
    }
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
            let (user, rest) = match parts[..] {
                ["photos", user, ref rest @ ..] if valid_user(user) => (user.to_owned(), rest),
                ["groups", name, ..] if valid_user(name) => {
                    return Ok(Target::Group {
                        name: (*name).into(),
                    });
                }
                ["search"] => {
                    let mut params: Vec<(String, String)> = url
                        .query_pairs()
                        .map(|(k, v)| (k.into_owned(), v.into_owned()))
                        .filter(|(k, _)| !matches!(k.as_str(), "page" | "per_page" | "extras"))
                        .collect();
                    if params.is_empty() {
                        return Err(unsupported());
                    }
                    if !params.iter().any(|(k, _)| k == "text") {
                        params.push(("text".into(), String::new()));
                    }
                    return Ok(Target::Search { params });
                }
                _ => return Err(unsupported()),
            };
            match rest {
                [] => Ok(Target::User { user }),
                ["albums" | "sets"] => Ok(Target::Albums { user }),
                ["favorites", ..] => Ok(Target::Favorites { user }),
                ["galleries", id, ..] if valid_id(id) => Ok(Target::Gallery {
                    user,
                    id: (*id).into(),
                }),
                [page] if page.starts_with("page") && valid_id(&page[4..]) => {
                    Ok(Target::User { user })
                }
                [id, ..] if valid_id(id) => Ok(Target::Photo { id: (*id).into() }),
                ["albums" | "sets", id, ..] if valid_id(id) => Ok(Target::Album {
                    user,
                    id: (*id).into(),
                }),
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

fn number_of(value: &Value, key: &str) -> Option<u64> {
    number(value, key)
}

fn number(value: &Value, key: &str) -> Option<u64> {
    field(value, key)?.parse().ok().filter(|v| *v > 0)
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

/// A chosen file: URL, width, height and whether it is a video.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Size {
    pub url: Url,
    pub width: Option<u64>,
    pub height: Option<u64>,
    pub video: bool,
    /// Upstream `label`: the size or stream name.
    pub label: String,
}

fn video_rank(label: &str) -> u8 {
    match label {
        "Video Original" => 4,
        "4K MP4" | "2160p" => 3,
        "HD MP4" | "1080p" => 2,
        _ => 1,
    }
}

/// Pick the best size from a `getSizes` response: the best video stream for
/// videos, else "Original" or the largest photo.
pub fn parse_sizes(bytes: &[u8], video: bool) -> Result<Size> {
    parse_sizes_with(bytes, video, &SizeLimit::default())
}

/// [`parse_sizes`] with `size-max`: like upstream `photos_getSizes`, photo
/// sizes from the first one (after the smallest) exceeding the limit are
/// dropped, and the largest remaining one is used.
pub fn parse_sizes_with(bytes: &[u8], video: bool, limit: &SizeLimit) -> Result<Size> {
    let body = check(bytes)?;
    let mut sizes: &[Value] = body
        .pointer("/sizes/size")
        .and_then(Value::as_array)
        .ok_or_else(|| error("Flickr response contains no size list"))?;
    if !video
        && limit.max.is_some()
        && let Some(cut) = sizes
            .iter()
            .enumerate()
            .skip(1)
            .position(|(_, size)| limit.exceeds(number(size, "width"), number(size, "height")))
    {
        sizes = &sizes[..cut + 1];
    }
    let limited = !video && limit.max.is_some();
    let candidates = sizes.iter().enumerate().filter_map(|(position, size)| {
        let is_video = size.get("media").and_then(Value::as_str) == Some("video")
            || field(size, "label").is_some_and(|l| l.contains("MP4") || l.contains("Video"));
        let url = media_url(&field(size, "source")?, is_video)?;
        let label = field(size, "label").unwrap_or_default();
        let width = number(size, "width");
        let height = number(size, "height");
        let area = width.unwrap_or(0) * height.unwrap_or(0);
        let rank = if is_video {
            video_rank(&label)
        } else {
            u8::from(label == "Original")
        };
        // Upstream takes the last remaining size under a limit.
        let area = if limited { position as u64 } else { area };
        Some((
            is_video,
            if limited { 0 } else { rank },
            area,
            Size {
                url,
                width,
                height,
                video: is_video,
                label,
            },
        ))
    });
    let best = candidates
        .filter(|(is_video, ..)| *is_video == video)
        .max_by_key(|(_, rank, area, _)| (*rank, *area));
    best.map(|(.., size)| size)
        .ok_or_else(|| error("Flickr photo contains no supported media"))
}

/// Parse JSON and map `{"stat":"fail"}` responses without echoing details.
fn check(bytes: &[u8]) -> Result<Value> {
    let body: Value =
        serde_json::from_slice(bytes).map_err(|_| error("Flickr returned invalid JSON"))?;
    if body.get("stat").and_then(Value::as_str) == Some("fail") {
        let code = body.get("code").and_then(Value::as_i64).unwrap_or(0);
        return Err(error(match code {
            1 => "Flickr photo, album or user not found".to_owned(),
            100 => "Flickr rejected the API key; check the api-key setting".to_owned(),
            105 => "Flickr API is temporarily unavailable".to_owned(),
            _ => format!("Flickr API error {code}"),
        }));
    }
    Ok(body)
}

/// Per-photo metadata independent of the chosen size.
#[derive(Clone, Debug, Default)]
pub struct Photo {
    /// Upstream keywords (the API photo object); empty for bare photos.
    pub keywords: Map<String, Value>,
    pub route: Option<&'static Route>,
    pub id: String,
    pub username: Option<String>,
    pub title: Option<String>,
    pub timestamp: Option<i64>,
    pub video: bool,
}

/// Parse a `flickr.photos.getInfo` response.
pub fn parse_info(bytes: &[u8], id: &str) -> Result<Photo> {
    let body = check(bytes)?;
    let photo = body
        .get("photo")
        .ok_or_else(|| error("Flickr response contains no photo"))?;
    if field(photo, "id").as_deref() != Some(id) {
        return Err(error("Flickr photo not found"));
    }
    let owner = photo.get("owner").unwrap_or(&Value::Null);
    Ok(Photo {
        keywords: photo.as_object().cloned().unwrap_or_default(),
        route: Some(&IMAGE),
        id: id.into(),
        username: field(owner, "username").or_else(|| field(owner, "path_alias")),
        title: photo.get("title").and_then(text),
        timestamp: photo
            .get("dates")
            .and_then(|d| field(d, "posted"))
            .and_then(|t| t.parse().ok()),
        video: photo.get("media").and_then(Value::as_str) == Some("video"),
    })
}

/// One listing entry: metadata and, for photos, the largest listed size.
pub struct Entry {
    pub photo: Photo,
    pub size: Option<Size>,
}

/// One listing page: entries, the current page and the page count.
pub struct Page {
    pub entries: Vec<Entry>,
    pub page: u64,
    pub pages: u64,
}

/// Parse a `photosets.getPhotos` (`photoset`) or `people.getPhotos`
/// (`photos`) response.
pub fn parse_page(bytes: &[u8], fallback_user: &str) -> Result<Page> {
    parse_page_with(bytes, fallback_user, &SizeLimit::default())
}

/// [`parse_page`] with `size-max`: the first listed format within the limit.
pub fn parse_page_with(bytes: &[u8], fallback_user: &str, limit: &SizeLimit) -> Result<Page> {
    let body = check(bytes)?;
    let list = body
        .get("photoset")
        .or_else(|| body.get("photos"))
        .ok_or_else(|| error("Flickr response contains no photo list"))?;
    let owner = field(list, "ownername").unwrap_or_else(|| fallback_user.into());
    let photos = list
        .get("photo")
        .and_then(Value::as_array)
        .ok_or_else(|| error("Flickr response contains no photo list"))?;
    let entries = photos
        .iter()
        .filter_map(|item| {
            let id = field(item, "id").filter(|id| valid_id(id))?;
            let video = item.get("media").and_then(Value::as_str) == Some("video");
            let size = limit.formats.iter().find_map(|s| {
                let url = media_url(&field(item, &format!("url_{s}"))?, false)?;
                let width = number(item, &format!("width_{s}"));
                let height = number(item, &format!("height_{s}"));
                let label = FORMATS.iter().find(|(code, ..)| code == s)?.1;
                (!limit.exceeds(width, height)).then(|| Size {
                    url,
                    width,
                    height,
                    video: false,
                    label: label.into(),
                })
            });
            Some(Entry {
                photo: Photo {
                    keywords: item.as_object().cloned().unwrap_or_default(),
                    route: None,
                    id,
                    username: field(item, "ownername").or_else(|| Some(owner.clone())),
                    title: field(item, "title"),
                    timestamp: field(item, "dateupload").and_then(|t| t.parse().ok()),
                    video,
                },
                size,
            })
        })
        .collect();
    Ok(Page {
        entries,
        page: number(list, "page").unwrap_or(1),
        pages: number(list, "pages").unwrap_or(1),
    })
}

/// Parse a `flickr.urls.lookupUser` response into the user's NSID.
pub fn parse_user(bytes: &[u8]) -> Result<(String, Option<String>)> {
    let body = check(bytes)?;
    let user = body
        .get("user")
        .ok_or_else(|| error("Flickr user not found"))?;
    let nsid = field(user, "id")
        .filter(|id| valid_user(id))
        .ok_or_else(|| error("Flickr user not found"))?;
    Ok((nsid, user.get("username").and_then(text)))
}

/// A `{"_content": ...}` wrapper's content, or the value itself.
fn content(value: &Value) -> Value {
    value
        .get("_content")
        .cloned()
        .unwrap_or_else(|| value.clone())
}

fn flatten_content(object: &mut Map<String, Value>, key: &str) {
    if let Some(value) = object.get_mut(key)
        && value.is_object()
    {
        *value = value.get("_content").cloned().unwrap_or(Value::Null);
    }
}

/// Upstream `_clean_info` of albums and galleries.
fn clean_title(mut info: Value) -> Value {
    if let Value::Object(object) = &mut info {
        flatten_content(object, "title");
        flatten_content(object, "description");
    }
    info
}

/// Upstream's `photos.getInfo` cleanup (`info` option and single photos).
fn clean_info(photo: &mut Map<String, Value>) {
    flatten_content(photo, "title");
    flatten_content(photo, "description");
    let comments = photo.get("comments").map(content);
    photo.insert("comments".into(), gdl::int_of(comments.as_ref()).into());
    if let Some(Value::Object(tags)) = photo.get("tags") {
        let raw: Vec<Value> = tags
            .get("tag")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|tag| tag.get("raw").cloned())
            .collect();
        photo.insert("tags".into(), raw.into());
    }
    let views = gdl::int_of(photo.get("views"));
    photo.insert("views".into(), views.into());
    let id = gdl::int_of(photo.get("id"));
    photo.insert("id".into(), id.into());
}

fn date_value(timestamp: Option<i64>) -> Value {
    crate::filter::source_date_text(timestamp).into()
}

fn warn(kind: &str, id: &str) {
    eprintln!("[flickr][warning] Unable to retrieve '{kind}' data for {id}");
}

/// Upstream `_file_url`: photos use their `_d` (download) variant.
fn file_url(url: &Url) -> Url {
    let raw = url.as_str();
    if raw.contains("/video/") || url.query().is_some() {
        return url.clone();
    }
    match raw.rsplit_once('.') {
        Some((path, ext)) if !ext.contains('/') => {
            Url::parse(&format!("{path}_d.{ext}")).unwrap_or_else(|_| url.clone())
        }
        _ => url.clone(),
    }
}

/// Build the media item for a photo and its chosen size.
pub fn media(photo: &Photo, size: Size, index: usize) -> Media {
    let ext = extension(&size.url, if size.video { "mp4" } else { "jpg" });
    let url = file_url(&size.url);
    let site_keywords = photo.route.map(|route| {
        let mut fields = photo.keywords.clone();
        fields.insert("url".into(), size.url.as_str().into());
        fields.insert("label".into(), size.label.clone().into());
        fields.insert("width".into(), size.width.unwrap_or(0).into());
        fields.insert("height".into(), size.height.unwrap_or(0).into());
        let (filename, extension) = crate::sites::nameext(&url);
        let extension = crate::path_options::map_extension(&extension).unwrap_or(extension);
        fields.insert("filename".into(), filename.into());
        fields.insert("extension".into(), extension.into());
        Arc::new(SiteKeywords { route, fields })
    });
    Media {
        filename: format!("{}.{ext}", photo.id),
        job: MediaJob::DirectHttp,
        category: "flickr".into(),
        metadata: Some(PostMetadata {
            file: Some(FileMetadata {
                width: size.width,
                height: size.height,
                media_type: Some(Arc::from(if size.video { "video" } else { "photo" })),
                ..FileMetadata::default()
            }),
            post_id: photo.id.clone(),
            media_id: photo.id.clone(),
            username: photo.username.as_deref().map(Arc::from),
            caption: photo.title.as_deref().map(Arc::from),
            timestamp: photo.timestamp,
            index,
            site_keywords,
            ..PostMetadata::default()
        }),
        url,
        headers: BTreeMap::new(),
        handoff: BTreeMap::new(),
    }
}

pub struct Flickr {
    client: Client,
    network: NetworkOptions,
    api_root: Url,
    api_key: String,
    /// Whether `api_key` was configured; otherwise the site's public key
    /// is used (upstream `_extract_apikey`).
    explicit_key: bool,
    public_key: tokio::sync::OnceCell<String>,
    timeout: Duration,
    request_timeout: Duration,
    retries: u8,
    rate_limits: RateLimits,
    size_limit: SizeLimit,
    meta: MetaOptions,
}

enum Listing {
    Album(String),
    User(String),
    Favorites(String),
    Gallery(String),
    Group(String),
    Search(Vec<(String, String)>),
}

/// What a listing needs resolved before its first page.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Lookup {
    UserPhotos,
    UserFavorites,
    Group,
}

struct ListingState {
    listing: Option<Listing>,
    lookup: Lookup,
    route: &'static Route,
    /// Upstream `metadata()`: `user`, `album`, `gallery`, `group` or `search`.
    data: Option<Map<String, Value>>,
    user: String,
    page: u64,
    index: usize,
    done: bool,
    pending: VecDeque<ExtractionEvent>,
}

impl Flickr {
    pub fn new(timeout: Duration) -> Result<Self> {
        Ok(Self {
            client: NetworkOptions::default().client(USER_AGENT, timeout)?,
            network: NetworkOptions::default(),
            api_root: Url::parse("https://api.flickr.com/").expect("static URL"),
            api_key: DEFAULT_API_KEY.into(),
            explicit_key: false,
            public_key: tokio::sync::OnceCell::new(),
            timeout,
            request_timeout: timeout,
            retries: 0,
            rate_limits: RateLimits::default(),
            size_limit: SizeLimit::default(),
            meta: MetaOptions::default(),
        })
    }

    /// Upstream `info`, `exif`, `contexts`, `profile`, `videos` and `metadata`.
    pub fn with_meta(mut self, meta: MetaOptions) -> Self {
        self.meta = meta;
        self
    }

    /// Upstream `size-max`.
    pub fn with_size_limit(mut self, limit: SizeLimit) -> Self {
        self.size_limit = limit;
        self
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
            self.explicit_key = true;
        }
        self
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

    /// The configured key, else the public `site_key` of flickr.com/prints
    /// (fetched once; the built-in key when that fails or for test roots).
    async fn key(&self) -> &str {
        if self.explicit_key || self.api_root.host_str() != Some("api.flickr.com") {
            return &self.api_key;
        }
        self.public_key
            .get_or_init(|| async {
                // The page answers 404 but still carries the key.
                let request = self
                    .client
                    .get("https://www.flickr.com/prints")
                    .timeout(self.request_timeout)
                    .send();
                let page = match tokio::time::timeout(self.timeout, request).await {
                    Ok(Ok(response)) => response.text().await.unwrap_or_default(),
                    _ => return self.api_key.clone(),
                };
                let start = "flickr.api.site_key = \"";
                page.find(start)
                    .map(|i| &page[i + start.len()..])
                    .and_then(|rest| rest.split('"').next())
                    .filter(|key| !key.is_empty() && key.bytes().all(|b| b.is_ascii_hexdigit()))
                    .map_or_else(|| self.api_key.clone(), str::to_owned)
            })
            .await
    }

    async fn call(&self, method: &str, query: &[(&str, &str)]) -> Result<Vec<u8>> {
        let api_key = self.key().await;
        if api_key.is_empty() {
            return Err(error("Flickr requires an api-key setting"));
        }
        let mut endpoint = self
            .api_root
            .join("services/rest/")
            .expect("static Flickr path");
        endpoint
            .query_pairs_mut()
            .append_pair("method", method)
            .append_pair("api_key", api_key)
            .append_pair("format", "json")
            .append_pair("nojsoncallback", "1")
            .extend_pairs(query);
        tokio::time::timeout(self.timeout, self.request(&endpoint))
            .await
            .map_err(|_| error("native Flickr extraction timed out"))?
    }

    async fn sizes(&self, id: &str, video: bool) -> Result<Size> {
        let body = self
            .call("flickr.photos.getSizes", &[("photo_id", id)])
            .await?;
        parse_sizes_with(&body, video, &self.size_limit)
    }

    fn finish(&self, mut item: Media) -> VecDeque<ExtractionEvent> {
        self.network.apply_media(&mut item);
        group_with_keywords(
            vec![item],
            "flickr",
            KeywordPolicy::shared(&["num", "index", "media_id", "filename", "name", "extension"]),
        )
    }

    async fn api(&self, method: &str, query: &[(&str, &str)]) -> Result<Value> {
        check(&self.call(method, query).await?)
    }

    /// Upstream `FlickrImageExtractor.items`.
    async fn photo(&self, id: &str) -> Result<VecDeque<ExtractionEvent>> {
        let body = self
            .call("flickr.photos.getInfo", &[("photo_id", id)])
            .await?;
        let mut photo = parse_info(&body, id)?;
        let keywords = &mut photo.keywords;
        self.extract_metadata(keywords, false).await;
        let size = if photo.video && self.meta.videos {
            self.video(id, keywords.get("secret").and_then(text))
                .await?
        } else {
            self.sizes(id, false).await?
        };
        let owner = keywords.get("owner").cloned().unwrap_or(Value::Null);
        let user = match field(&owner, "nsid") {
            Some(nsid) if self.meta.profile => self.people_info(&nsid).await?,
            _ => owner,
        };
        keywords.insert("user".into(), user);
        clean_info(keywords);
        let uploaded = keywords.get("dateuploaded").and_then(text);
        let uploaded = uploaded.and_then(|t| t.parse().ok());
        keywords.insert("date".into(), date_value(uploaded));
        if let Some(Value::Object(location)) = keywords.get_mut("location") {
            for value in location.values_mut() {
                if let Some(content) = value.get("_content") {
                    *value = content.clone();
                }
            }
        }
        Ok(self.finish(media(&photo, size, 1)))
    }

    /// Upstream `_extract_metadata`: optional `info`, `exif` and `contexts`
    /// API calls; failures only warn.
    async fn extract_metadata(&self, photo: &mut Map<String, Value>, info: bool) {
        let id = photo.get("id").and_then(text).unwrap_or_default();
        let query = [("photo_id", id.as_str())];
        if info && self.meta.info {
            match self.api("flickr.photos.getInfo", &query).await {
                Ok(Value::Object(mut body)) => {
                    if let Some(Value::Object(data)) = body.remove("photo") {
                        photo.extend(data);
                        clean_info(photo);
                    }
                }
                _ => warn("info", &id),
            }
        }
        if self.meta.exif {
            match self.api("flickr.photos.getExif", &query).await {
                Ok(Value::Object(mut body)) => {
                    if let Some(Value::Object(data)) = body.remove("photo") {
                        photo.extend(data);
                    }
                }
                _ => warn("exif", &id),
            }
        }
        if self.meta.contexts {
            match self.api("flickr.photos.getAllContexts", &query).await {
                Ok(Value::Object(mut body)) => {
                    body.remove("stat");
                    photo.extend(body);
                }
                _ => warn("contexts", &id),
            }
        }
        if let Some(license) = photo.get("license").and_then(text) {
            let name = license_name(&license).map_or(Value::Null, Value::from);
            photo.insert("license_name".into(), name);
        }
    }

    /// Upstream `video_getStreamInfo`: the best-ranked stream.
    async fn video(&self, id: &str, secret: Option<String>) -> Result<Size> {
        let secret = match secret.filter(|secret| !secret.is_empty()) {
            Some(secret) => secret,
            None => self
                .api("flickr.photos.getInfo", &[("photo_id", id)])
                .await?
                .pointer("/photo/secret")
                .and_then(text)
                .ok_or_else(|| error("Flickr video contains no secret"))?,
        };
        let body = self
            .api(
                "flickr.video.getStreamInfo",
                &[("photo_id", id), ("secret", secret.as_str())],
            )
            .await?;
        let mut best: Option<(u8, &Value)> = None;
        for stream in body
            .pointer("/streams/stream")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let rank = stream_rank(stream.get("type").and_then(Value::as_str).unwrap_or(""));
            // Python's max() keeps the first of equal elements.
            if best.is_none_or(|(best, _)| rank > best) {
                best = Some((rank, stream));
            }
        }
        let (_, stream) = best.ok_or_else(|| error("Flickr video contains no streams"))?;
        let url = field(stream, "_content")
            .and_then(|url| media_url(&url, true))
            .ok_or_else(|| error("Flickr video contains no supported stream"))?;
        Ok(Size {
            url,
            width: None,
            height: None,
            video: true,
            label: field(stream, "type").unwrap_or_default(),
        })
    }

    /// Upstream `people_getInfo`.
    async fn people_info(&self, nsid: &str) -> Result<Value> {
        let body = self
            .api("flickr.people.getInfo", &[("user_id", nsid)])
            .await?;
        let Some(mut person) = body.get("person").cloned() else {
            return Ok(body);
        };
        if let Value::Object(user) = &mut person {
            for key in [
                "description",
                "username",
                "realname",
                "location",
                "profileurl",
                "photosurl",
                "mobileurl",
            ] {
                flatten_content(user, key);
            }
            if let Some(Value::Object(photos)) = user.get_mut("photos") {
                for key in ["count", "firstdate", "firstdatetaken"] {
                    flatten_content(photos, key);
                }
            }
        }
        Ok(person)
    }

    /// Upstream `urls_lookupUser` (plus `people_getInfo` with `profile`).
    async fn lookup_user(&self, name: &str) -> Result<Map<String, Value>> {
        let url = format!("https://www.flickr.com/photos/{name}");
        let body = self
            .call("flickr.urls.lookupUser", &[("url", &url)])
            .await?;
        let (nsid, username) = parse_user(&body)?;
        let mut user = Map::new();
        user.insert("nsid".into(), nsid.clone().into());
        user.insert("username".into(), username.map_or(Value::Null, Value::from));
        user.insert("path_alias".into(), name.into());
        if self.meta.profile
            && let Value::Object(info) = self.people_info(&nsid).await?
        {
            user.extend(info);
        }
        Ok(user)
    }

    /// Upstream `metadata()` of listing extractors and their first listing.
    async fn listing_metadata(&self, state: &mut ListingState) -> Result<Listing> {
        let mut data = Map::new();
        let listing = match (state.listing.take(), state.lookup) {
            (Some(Listing::Search(params)), _) => {
                let search: Map<String, Value> = params
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone().into()))
                    .collect();
                data.insert("search".into(), search.into());
                Listing::Search(params)
            }
            (None, Lookup::Group) => {
                let url = format!("https://www.flickr.com/groups/{}", state.user);
                let body = self
                    .api("flickr.urls.lookupGroup", &[("url", &url)])
                    .await?;
                let nsid = body
                    .pointer("/group/id")
                    .and_then(text)
                    .filter(|id| valid_user(id))
                    .ok_or_else(|| error("Flickr group not found"))?;
                data.insert(
                    "group".into(),
                    json!({
                        "nsid": nsid,
                        "path_alias": state.user,
                        "groupname": body.pointer("/group/groupname").map(content),
                    }),
                );
                Listing::Group(nsid)
            }
            (listing, lookup) => {
                let user = self.lookup_user(&state.user).await?;
                let nsid = user.get("nsid").and_then(text).unwrap_or_default();
                if let Some(Value::String(name)) = user.get("username") {
                    state.user = name.clone();
                }
                let listing = match listing {
                    Some(Listing::Album(id)) => {
                        let album = self
                            .api(
                                "flickr.photosets.getInfo",
                                &[("photoset_id", id.as_str()), ("user_id", nsid.as_str())],
                            )
                            .await
                            .ok()
                            .and_then(|mut body| body.get_mut("photoset").map(Value::take))
                            .map(clean_title);
                        if album.is_none() {
                            eprintln!("[flickr][warning] {id}: Unable to retrieve album metadata");
                        }
                        data.insert("album".into(), album.unwrap_or_else(|| json!({})));
                        Listing::Album(id)
                    }
                    Some(Listing::Gallery(id)) => {
                        let gallery = self
                            .api("flickr.galleries.getInfo", &[("gallery_id", id.as_str())])
                            .await?
                            .get_mut("gallery")
                            .map(Value::take)
                            .map(clean_title)
                            .unwrap_or_else(|| json!({}));
                        data.insert("gallery".into(), gallery);
                        Listing::Gallery(id)
                    }
                    _ if lookup == Lookup::UserFavorites => Listing::Favorites(nsid),
                    _ => Listing::User(nsid),
                };
                data.insert("user".into(), user.into());
                listing
            }
        };
        state.data = Some(data);
        Ok(listing)
    }

    /// Upstream `_extract_format` plus `photo.update(data)`.
    async fn listed(
        &self,
        entry: Entry,
        data: &Map<String, Value>,
        route: &'static Route,
    ) -> Result<(Photo, Size)> {
        let Entry { mut photo, size } = entry;
        let keywords = &mut photo.keywords;
        let description = keywords.get("description").map(content);
        let description = description.as_ref().and_then(Value::as_str).unwrap_or("");
        keywords.insert("description".into(), description.trim().into());
        let views = gdl::int_of(keywords.get("views"));
        keywords.insert("views".into(), views.into());
        keywords.insert("date".into(), date_value(photo.timestamp));
        let tags: Vec<Value> = keywords
            .get("tags")
            .and_then(Value::as_str)
            .unwrap_or("")
            .split_whitespace()
            .map(Value::from)
            .collect();
        keywords.insert("tags".into(), tags.into());
        self.extract_metadata(keywords, true).await;
        let id = gdl::int_of(keywords.get("id"));
        keywords.insert("id".into(), id.into());
        match keywords.get("owner") {
            None => {
                let user = data.get("user").cloned().unwrap_or(Value::Null);
                keywords.insert("owner".into(), user);
            }
            Some(Value::String(nsid)) if !self.meta.info => {
                let owner = json!({
                    "nsid": nsid,
                    "username": keywords.get("ownername"),
                    "path_alias": keywords.get("pathalias"),
                });
                keywords.insert("owner".into(), owner);
            }
            Some(_) => {}
        }
        keywords.remove("pathalias");
        keywords.remove("ownername");
        if photo.username.is_none() {
            photo.username = keywords
                .get("owner")
                .and_then(|owner| owner.get("username"))
                .and_then(text);
        }
        let size = if photo.video && self.meta.videos {
            self.video(&photo.id, keywords.get("secret").and_then(text))
                .await?
        } else {
            match size {
                Some(size) => {
                    keywords.retain(|key, _| {
                        !["url_", "width_", "height_"]
                            .iter()
                            .any(|prefix| key.starts_with(prefix))
                    });
                    size
                }
                None => self.sizes(&photo.id, false).await?,
            }
        };
        keywords.extend(data.clone());
        photo.route = Some(route);
        Ok((photo, size))
    }

    async fn next_page(&self, state: &mut ListingState) -> Result<()> {
        let listing = match state.listing.take() {
            Some(listing) if state.data.is_some() => listing,
            listing => {
                state.listing = listing;
                self.listing_metadata(state).await?
            }
        };
        let page = (state.page + 1).to_string();
        let per_page = PAGE_SIZE.to_string();
        let extras = self.size_limit.extras_with(self.meta.extras.as_deref());
        let mut query = vec![
            ("extras", extras.as_str()),
            ("per_page", per_page.as_str()),
            ("page", page.as_str()),
        ];
        let method = match &listing {
            Listing::Album(id) => {
                query.insert(0, ("photoset_id", id.as_str()));
                "flickr.photosets.getPhotos"
            }
            Listing::User(nsid) => {
                query.insert(0, ("user_id", nsid.as_str()));
                "flickr.people.getPhotos"
            }
            Listing::Favorites(nsid) => {
                query.insert(0, ("user_id", nsid.as_str()));
                "flickr.favorites.getList"
            }
            Listing::Gallery(id) => {
                query.insert(0, ("gallery_id", id.as_str()));
                "flickr.galleries.getPhotos"
            }
            Listing::Group(nsid) => {
                query.insert(0, ("group_id", nsid.as_str()));
                "flickr.groups.pools.getPhotos"
            }
            Listing::Search(params) => {
                for (key, value) in params {
                    query.push((key.as_str(), value.as_str()));
                }
                "flickr.photos.search"
            }
        };
        let body = self.call(method, &query).await?;
        let parsed = parse_page_with(&body, &state.user, &self.size_limit)?;
        state.page += 1;
        state.done =
            parsed.entries.is_empty() || parsed.page >= parsed.pages || state.page >= MAX_PAGES;
        let data = state.data.clone().unwrap_or_default();
        for entry in parsed.entries {
            let id = entry.photo.id.clone();
            let (photo, size) = match self.listed(entry, &data, state.route).await {
                Ok(listed) => listed,
                Err(e) => {
                    eprintln!("[flickr][warning] Skipping photo {id} ({e})");
                    continue;
                }
            };
            state.index += 1;
            let events = self.finish(media(&photo, size, state.index));
            state.pending.extend(events);
        }
        state.listing = Some(listing);
        Ok(())
    }

    pub fn stream(&self, url: &Url) -> BoxStream<'_, Result<Media>> {
        crate::event::media_only(self.events(url))
    }

    /// Upstream `FlickrAlbumExtractor` without an album ID: queue every
    /// album of the user (`photosets.getList`).
    fn albums(&self, user: String) -> BoxStream<'_, Result<ExtractionEvent>> {
        stream::try_unfold(
            (None::<Map<String, Value>>, 1u64, false),
            move |(data, page, done)| {
                let user = user.clone();
                async move {
                    if done || page > MAX_PAGES {
                        return Ok(None);
                    }
                    let data = match data {
                        Some(data) => data,
                        None => {
                            let mut data = Map::new();
                            data.insert("user".into(), self.lookup_user(&user).await?.into());
                            data
                        }
                    };
                    let nsid = data
                        .get("user")
                        .and_then(|user| user.get("nsid"))
                        .and_then(text)
                        .unwrap_or_default();
                    let number = page.to_string();
                    let body = check(
                        &self
                            .call(
                                "flickr.photosets.getList",
                                &[("user_id", nsid.as_str()), ("page", number.as_str())],
                            )
                            .await?,
                    )?;
                    let sets = body
                        .get("photosets")
                        .ok_or_else(|| error("Flickr response contains no albums"))?;
                    let pages = number_of(sets, "pages").unwrap_or(1);
                    let children: Vec<_> = sets
                        .get("photoset")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default()
                        .into_iter()
                        .filter_map(|album| {
                            let id = field(&album, "id").filter(|id| valid_id(id))?;
                            let url = Url::parse(&format!(
                                "https://www.flickr.com/photos/{user}/albums/{id}"
                            ))
                            .ok()?;
                            let mut album =
                                clean_title(album).as_object().cloned().unwrap_or_default();
                            album.extend(data.clone());
                            Some(crate::event::child_job(
                                crate::event::ChildMetadata::from_raw("flickr", "album", &album)
                                    .with_url(url.as_str()),
                                move || self.events(&url),
                            ))
                        })
                        .collect();
                    Ok::<_, crate::Error>(Some((
                        stream::iter(children).flatten().boxed(),
                        (Some(data), page + 1, page >= pages),
                    )))
                }
            },
        )
        .map(|result| match result {
            Ok(events) => events,
            Err(e) => stream::once(async move { Err(e) }).boxed(),
        })
        .flatten()
        .boxed()
    }

    /// Emit one post boundary per photo; listings paginate lazily.
    pub fn events(&self, url: &Url) -> BoxStream<'_, Result<ExtractionEvent>> {
        let target = match target(url) {
            Ok(target) => target,
            Err(e) => return stream::once(async move { Err(e) }).boxed(),
        };
        let (listing, user) = match target {
            Target::Photo { id } => {
                return stream::once(async move { self.photo(&id).await })
                    .map(|result| match result {
                        Ok(events) => stream::iter(events.into_iter().map(Ok)).boxed(),
                        Err(e) => stream::once(async move { Err(e) }).boxed(),
                    })
                    .flatten()
                    .boxed();
            }
            Target::Album { user, id } => {
                ((Some(Listing::Album(id)), Lookup::UserPhotos, &ALBUM), user)
            }
            Target::User { user } => ((None, Lookup::UserPhotos, &USER), user),
            Target::Favorites { user } => ((None, Lookup::UserFavorites, &FAVORITE), user),
            Target::Gallery { user, id } => (
                (Some(Listing::Gallery(id)), Lookup::UserPhotos, &GALLERY),
                user,
            ),
            Target::Group { name } => ((None, Lookup::Group, &GROUP), name),
            Target::Search { params } => (
                (Some(Listing::Search(params)), Lookup::UserPhotos, &SEARCH),
                "search".into(),
            ),
            Target::Albums { user } => return self.albums(user),
        };
        let (listing, lookup, route) = listing;
        let state = ListingState {
            listing,
            lookup,
            route,
            data: None,
            user,
            page: 0,
            index: 0,
            done: false,
            pending: VecDeque::new(),
        };
        stream::unfold(Some(state), move |state| async move {
            let mut state = state?;
            loop {
                if let Some(event) = state.pending.pop_front() {
                    return Some((Ok(event), Some(state)));
                }
                if state.done {
                    return None;
                }
                if let Err(e) = self.next_page(&mut state).await {
                    return Some((Err(e), None));
                }
            }
        })
        .boxed()
    }

    async fn request(&self, endpoint: &Url) -> Result<Vec<u8>> {
        for attempt in 0..=self.retries {
            match self.attempt(endpoint, attempt).await {
                Ok(body) => return Ok(body),
                Err(failure) if failure.retry && attempt < self.retries => {
                    tokio::time::sleep(backoff(attempt)).await;
                }
                Err(failure) => return Err(failure.error),
            }
        }
        unreachable!()
    }

    async fn attempt(&self, endpoint: &Url, attempt: u8) -> std::result::Result<Vec<u8>, Failure> {
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
        if matches!(status.as_u16(), 404 | 410) {
            return Err(error("Flickr photo, album or user not found").into());
        }
        if matches!(status.as_u16(), 401 | 403) {
            return Err(error("Flickr refused access; check the api-key setting").into());
        }
        if !status.is_success() {
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
        thread::{self, JoinHandle},
    };

    const ORIGINAL: &str = "https://live.staticflickr.com/65535/123_abc_o.png";

    fn sizes_body() -> Value {
        json!({"stat": "ok", "sizes": {"size": [
            {"label": "Large", "width": 1024, "height": 768,
             "source": "https://live.staticflickr.com/65535/123_abc_b.jpg", "media": "photo"},
            {"label": "Original", "width": "800", "height": "600", "source": ORIGINAL, "media": "photo"},
            {"label": "Huge", "width": 9000, "height": 9000,
             "source": "http://live.staticflickr.com/insecure.jpg", "media": "photo"},
            {"label": "Site MP4", "width": 640, "height": 360,
             "source": "https://www.flickr.com/photos/a/123/play/site/x/", "media": "video"},
            {"label": "Video Original", "width": 320, "height": 240,
             "source": "https://www.flickr.com/photos/a/123/play/orig/x/", "media": "video"},
            {"label": "HD MP4", "width": 1920, "height": 1080,
             "source": "https://evil.example/hd.mp4", "media": "video"}]}})
    }

    #[test]
    fn static_file_urls_name_their_photo() {
        for (url, id) in [
            (
                "http://c2.staticflickr.com/2/1475/24531000464_9a7503ae68_b.jpg",
                "24531000464",
            ),
            (
                "https://farm2.static.flickr.com/1035/1188352415_cb139831d0.jpg",
                "1188352415",
            ),
            ("https://live.staticflickr.com/65535/123_abc_o.png", "123"),
        ] {
            let found = target(&Url::parse(url).unwrap()).unwrap();
            assert!(
                matches!(found, Target::Photo { id: ref found } if found == id),
                "{url}"
            );
        }
        assert!(target(&Url::parse("https://live.staticflickr.com/x.jpg").unwrap()).is_err());
    }

    #[test]
    fn picks_original_photo_and_best_video() {
        let body = sizes_body().to_string();
        let photo = parse_sizes(body.as_bytes(), false).unwrap();
        assert_eq!(photo.url.as_str(), ORIGINAL);
        assert_eq!((photo.width, photo.height), (Some(800), Some(600)));
        let video = parse_sizes(body.as_bytes(), true).unwrap();
        assert!(video.video);
        assert!(video.url.path().contains("/play/orig/"));
        let item = media(
            &Photo {
                id: "123".into(),
                ..Photo::default()
            },
            video,
            1,
        );
        assert_eq!(item.filename, "123.mp4");
        let item = media(
            &Photo {
                id: "123".into(),
                username: Some("alice".into()),
                title: Some("Hi".into()),
                timestamp: Some(1700000000),
                video: false,
                ..Photo::default()
            },
            photo,
            1,
        );
        assert_eq!(item.filename, "123.png");
        let meta = item.metadata.as_ref().unwrap();
        assert_eq!(
            (meta.post_id.as_str(), meta.media_id.as_str()),
            ("123", "123")
        );
        assert_eq!(meta.username.as_deref(), Some("alice"));
        assert_eq!(meta.caption.as_deref(), Some("Hi"));
        assert_eq!(meta.timestamp, Some(1700000000));
        assert_eq!(
            meta.file.as_ref().unwrap().media_type.as_deref(),
            Some("photo")
        );
        let key = item.archive_key().unwrap();
        assert!(key.starts_with("flickr:"), "{key}");
        let mut other = item.clone();
        other.metadata.as_mut().unwrap().post_id = "124".into();
        assert_ne!(other.archive_key(), Some(key));
        let none = json!({"stat": "ok", "sizes": {"size": [
            {"label": "Original", "source": "https://evil.example/x.jpg"}]}})
        .to_string();
        assert!(parse_sizes(none.as_bytes(), false).is_err());
    }

    #[test]
    fn size_max_limits_listing_formats_and_sizes() {
        assert_eq!(
            SizeLimit::default().formats,
            ["o", "6k", "5k", "4k", "3k", "k", "h", "l"]
        );
        let limit = SizeLimit::from_option(Some(&json!("Large")));
        assert_eq!(limit, SizeLimit::from_option(Some(&json!(1024))));
        assert_eq!(limit.formats, ["o", "l", "c", "z", "m", "n", "s", "q"]);
        assert!(
            limit
                .extras()
                .ends_with(",url_o,url_l,url_c,url_z,url_m,url_n,url_s,url_q")
        );
        assert_eq!(
            SizeLimit::from_option(Some(&json!("huge"))),
            SizeLimit::default()
        );
        let page = json!({"stat": "ok", "photos": {"page": 1, "pages": 1, "photo": [
            {"id": "1", "media": "photo",
             "url_o": "https://live.staticflickr.com/o.jpg", "width_o": 4000, "height_o": 3000,
             "url_l": "https://live.staticflickr.com/l.jpg", "width_l": 1024, "height_l": 768}]}})
        .to_string();
        let page = parse_page_with(page.as_bytes(), "x", &limit).unwrap();
        let size = page.entries[0].size.as_ref().unwrap();
        assert_eq!(size.url.as_str(), "https://live.staticflickr.com/l.jpg");
        let sizes = json!({"stat": "ok", "sizes": {"size": [
            {"label": "Square", "width": 75, "height": 75,
             "source": "https://live.staticflickr.com/sq.jpg"},
            {"label": "Medium", "width": 500, "height": 375,
             "source": "https://live.staticflickr.com/m.jpg"},
            {"label": "Large 1600", "width": 1600, "height": 1200,
             "source": "https://live.staticflickr.com/h.jpg"},
            {"label": "Original", "width": 4000, "height": 3000,
             "source": "https://live.staticflickr.com/o.jpg"}]}})
        .to_string();
        let size = parse_sizes_with(sizes.as_bytes(), false, &limit).unwrap();
        assert_eq!(size.url.as_str(), "https://live.staticflickr.com/m.jpg");
        let size = parse_sizes(sizes.as_bytes(), false).unwrap();
        assert_eq!(size.url.as_str(), "https://live.staticflickr.com/o.jpg");
    }

    #[test]
    fn parses_info_listing_and_failures() {
        let info = json!({"stat": "ok", "photo": {"id": "123", "media": "video",
            "owner": {"nsid": "1@N00", "username": "alice"},
            "title": {"_content": "Title"}, "dates": {"posted": "1700000000"}}})
        .to_string();
        let photo = parse_info(info.as_bytes(), "123").unwrap();
        assert!(photo.video);
        assert_eq!(photo.username.as_deref(), Some("alice"));
        assert_eq!(photo.title.as_deref(), Some("Title"));
        assert_eq!(photo.timestamp, Some(1700000000));
        assert!(parse_info(info.as_bytes(), "124").is_err());
        let page = json!({"stat": "ok", "photoset": {"page": "1", "pages": 3,
            "ownername": "bob", "photo": [
                {"id": "1", "title": "a", "dateupload": "5", "media": "photo",
                 "url_l": "https://live.staticflickr.com/l.jpg", "width_l": 1024,
                 "url_k": "https://live.staticflickr.com/k.jpg", "width_k": "2048", "height_k": 1536},
                {"id": "2", "media": "video", "ownername": "carol"},
                {"id": "bad"}]}})
        .to_string();
        let page = parse_page(page.as_bytes(), "x").unwrap();
        assert_eq!((page.page, page.pages, page.entries.len()), (1, 3, 2));
        let size = page.entries[0].size.as_ref().unwrap();
        assert_eq!(size.url.as_str(), "https://live.staticflickr.com/k.jpg");
        assert_eq!((size.width, size.height), (Some(2048), Some(1536)));
        assert_eq!(page.entries[0].photo.username.as_deref(), Some("bob"));
        assert_eq!(page.entries[1].photo.username.as_deref(), Some("carol"));
        assert!(page.entries[1].photo.video && page.entries[1].size.is_none());
        let fail = |code: i64| {
            let body = json!({"stat": "fail", "code": code, "message": "key secret"}).to_string();
            parse_page(body.as_bytes(), "x").err().unwrap().to_string()
        };
        assert!(fail(1).contains("not found"));
        assert!(fail(100).contains("api-key"));
        assert!(!fail(7).contains("secret"));
        assert!(parse_page(b"nope", "x").is_err());
        let user = json!({"stat": "ok", "user": {"id": "12@N00", "username": {"_content": "al"}}});
        assert_eq!(
            parse_user(user.to_string().as_bytes()).unwrap(),
            ("12@N00".into(), Some("al".into()))
        );
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
        assert_eq!(
            t("https://www.flickr.com/photos/alice/albums/72157").unwrap(),
            Target::Album {
                user: "alice".into(),
                id: "72157".into()
            }
        );
        assert_eq!(
            t("https://www.flickr.com/photos/alice/sets/72157/").unwrap(),
            Target::Album {
                user: "alice".into(),
                id: "72157".into()
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
        assert_eq!(
            t("https://www.flickr.com/photos/alice/albums").unwrap(),
            Target::Albums {
                user: "alice".into()
            }
        );
        assert_eq!(
            t("https://www.flickr.com/photos/alice/favorites").unwrap(),
            Target::Favorites {
                user: "alice".into()
            }
        );
        assert_eq!(
            t("https://www.flickr.com/groups/x/").unwrap(),
            Target::Group { name: "x".into() }
        );
        assert_eq!(
            t("https://www.flickr.com/photos/flickr/galleries/72157681572514792/").unwrap(),
            Target::Gallery {
                user: "flickr".into(),
                id: "72157681572514792".into()
            }
        );
        assert_eq!(
            t("https://flickr.com/search/?text=tree%20cloud&styles=minimalism&page=2").unwrap(),
            Target::Search {
                params: vec![
                    ("text".into(), "tree cloud".into()),
                    ("styles".into(), "minimalism".into())
                ]
            }
        );
        for raw in [
            "https://www.flickr.com/",
            "https://www.flickr.com/photos/tags/cat",
            "https://www.flickr.com/search/",
            "https://flic.kr/p/0OIl",
            "https://www.flickr.com:444/photos/alice",
            "https://www.flickr.com.evil.example/photos/alice",
        ] {
            assert!(t(raw).is_err(), "{raw}");
        }
        assert!(media_url("https://farm1.staticflickr.com/a.jpg", false).is_some());
        assert!(media_url("https://www.flickr.com/play/x", false).is_none());
        assert!(media_url("https://www.flickr.com/play/x", true).is_some());
        assert!(media_url("https://flickr.com.evil.example/x", true).is_none());
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

    fn flickr(root: Url) -> Flickr {
        Flickr::new(Duration::from_secs(5))
            .unwrap()
            .with_network(&NetworkOptions::new(Some(String::new()), None).unwrap())
            .unwrap()
            .with_api_root(root)
    }

    #[tokio::test]
    async fn fetches_photo_with_retries_and_api_key() {
        let info = json!({"stat": "ok", "photo": {"id": "123", "media": "photo",
            "owner": {"username": "alice"}, "title": {"_content": "T"}}});
        let (root, server) = serve(vec![
            response("503 Service Unavailable", b""),
            ok(info),
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
        // Upstream `_file_url` downloads the `_d` variant.
        assert_eq!(
            media.url.as_str(),
            "https://live.staticflickr.com/65535/123_abc_o_d.png"
        );
        let keywords = &media
            .metadata
            .as_ref()
            .unwrap()
            .site_keywords
            .as_ref()
            .unwrap();
        assert_eq!(keywords.route.subcategory, "image");
        assert_eq!(keywords.fields["title"], "T");
        assert_eq!(keywords.fields["id"], 123);
        assert_eq!(keywords.fields["user"], json!({"username": "alice"}));
        assert_eq!(keywords.fields["label"], "Original");
        assert_eq!(keywords.fields["filename"], "123_abc_o_d");
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with(
            "GET /services/rest/?method=flickr.photos.getInfo&api_key=secret&format=json&nojsoncallback=1&photo_id=123 "
        ), "{}", requests[1]);
        assert!(requests[2].contains("method=flickr.photos.getSizes"));
    }

    #[tokio::test]
    async fn paginates_user_photostream_and_resolves_videos() {
        let photo = |id: &str| {
            json!({"id": id, "media": "photo", "ownername": "Alice",
                "url_o": format!("https://live.staticflickr.com/{id}_o.jpg")})
        };
        let (root, server) = serve(vec![
            ok(json!({"stat": "ok", "user": {"id": "1@N00", "username": {"_content": "alice"}}})),
            ok(json!({"stat": "ok", "photos": {"page": 1, "pages": 2,
                "photo": [photo("1"), {"id": "2", "media": "video"}]}})),
            ok(json!({"stat": "ok", "photo": {"id": "2", "secret": "s3"}})),
            ok(json!({"stat": "ok", "streams": {"stream": [
                {"type": "700", "_content": "https://live.staticflickr.com/video/2/700.mp4"},
                {"type": "orig", "_content": "https://live.staticflickr.com/video/2/orig.mp4"},
                {"type": "orig", "_content": "https://live.staticflickr.com/video/2/other.mp4"}]}})),
            ok(json!({"stat": "ok", "photos": {"page": 2, "pages": 2, "photo": [photo("3")]}})),
        ]);
        let media: Vec<_> = flickr(root)
            .stream(&Url::parse("https://www.flickr.com/photos/alice").unwrap())
            .collect()
            .await;
        let media: Vec<_> = media.into_iter().map(|m| m.unwrap()).collect();
        assert_eq!(media.len(), 3);
        assert_eq!(media[1].filename, "2.mp4");
        let indexes: Vec<_> = media
            .iter()
            .map(|m| m.metadata.as_ref().unwrap().index)
            .collect();
        assert_eq!(indexes, [1, 2, 3]);
        assert_eq!(
            media[1].metadata.as_ref().unwrap().username.as_deref(),
            Some("alice")
        );
        let requests = server.join().unwrap();
        let key = format!("api_key={DEFAULT_API_KEY}");
        assert!(
            requests[0].contains("method=flickr.urls.lookupUser") && requests[0].contains(&key)
        );
        assert!(requests[1].contains("method=flickr.people.getPhotos"));
        assert!(requests[1].contains("&user_id=1%40N00&extras="));
        assert!(requests[1].contains("&per_page=500&page=1 "));
        assert!(requests[2].contains("method=flickr.photos.getInfo"));
        assert!(requests[3].contains("method=flickr.video.getStreamInfo"));
        assert!(requests[3].contains("&secret=s3"));
        assert_eq!(
            media[1].url.as_str(),
            "https://live.staticflickr.com/video/2/orig.mp4"
        );
        assert!(requests[4].contains("&page=2 "));
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
        // Like upstream, album metadata starts with the owner lookup.
        assert!(requests[0].contains("method=flickr.urls.lookupUser&"));
    }

    #[tokio::test]
    async fn album_listing_exposes_upstream_keywords_and_metadata_options() {
        let (root, server) = serve(vec![
            ok(json!({"stat": "ok", "user": {"id": "1@N00", "username": {"_content": "alice"}}})),
            ok(json!({"stat": "ok", "photoset": {"id": "5",
                "title": {"_content": "Trip"}, "description": {"_content": "D"}}})),
            ok(
                json!({"stat": "ok", "photoset": {"page": 1, "pages": 1, "photo": [
                {"id": "7", "secret": "s", "title": "Pic", "license": "4",
                 "description": {"_content": " text \n"}, "dateupload": "1700000000",
                 "tags": "a b", "views": "12", "media": "photo", "pathalias": "al",
                 "ownername": "alice",
                 "url_o": "https://live.staticflickr.com/7_o.jpg", "width_o": "40",
                 "height_o": 30}]}}),
            ),
            ok(json!({"stat": "ok", "photo": {"id": "7", "camera": "Cam",
                "exif": [{"tag": "Make"}]}})),
            ok(json!({"stat": "ok", "set": [{"id": "5"}], "pool": []})),
        ]);
        let meta = MetaOptions::from_options(
            json!({"exif": true, "contexts": 1, "metadata": ["license", "geo"]}).as_object(),
            None,
            None,
        );
        let media: Vec<_> = flickr(root)
            .with_meta(meta)
            .stream(&Url::parse("https://www.flickr.com/photos/al/albums/5").unwrap())
            .collect()
            .await;
        let media = media.into_iter().next().unwrap().unwrap();
        let keywords = media
            .metadata
            .as_ref()
            .unwrap()
            .site_keywords
            .clone()
            .unwrap();
        let fields = &keywords.fields;
        assert_eq!(keywords.route.subcategory, "album");
        assert_eq!(keywords.route.archive, "a_{album[id]}_{id}");
        assert_eq!(fields["id"], 7);
        assert_eq!(fields["description"], "text");
        assert_eq!(fields["tags"], json!(["a", "b"]));
        assert_eq!(fields["views"], 12);
        assert_eq!(fields["date"], "2023-11-14 22:13:20");
        assert_eq!(fields["camera"], "Cam");
        assert_eq!(fields["set"], json!([{"id": "5"}]));
        assert_eq!(fields["license_name"], "Attribution License");
        assert_eq!(
            fields["owner"],
            json!({"nsid": "1@N00", "username": "alice", "path_alias": "al"})
        );
        assert_eq!(fields["album"]["title"], "Trip");
        assert_eq!(fields["user"]["nsid"], "1@N00");
        assert_eq!(
            (fields["width"].clone(), fields["label"].clone()),
            (json!(40), json!("Original"))
        );
        assert!(!fields.contains_key("url_o") && !fields.contains_key("pathalias"));
        let requests = server.join().unwrap();
        assert!(requests[1].contains("method=flickr.photosets.getInfo"));
        assert!(requests[2].contains(
            "extras=description%2Cdate_upload%2Ctags%2Cviews%2Cmedia%2Cpath_alias%2Cowner_name%2Clicense%2Cgeo%2Curl_o"
        ));
        assert!(requests[3].contains("method=flickr.photos.getExif"));
        assert!(requests[4].contains("method=flickr.photos.getAllContexts"));
    }
}
