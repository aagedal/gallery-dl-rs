//! Experimental native booru extraction for Danbooru, e621/e926, Gelbooru
//! and Safebooru.
//!
//! Mirrors gallery_dl/extractor/danbooru.py (Danbooru and e621 dialects) and
//! gelbooru.py / gelbooru_v02.py (Gelbooru and Safebooru) through their public
//! JSON APIs. Single posts, tag searches, Danbooru/e621 pools and
//! Danbooru favorite groups are supported. Posts without a file URL (deleted,
//! banned or login-restricted) are skipped silently, and media URLs are only
//! accepted from each site's own CDN hosts.
use crate::{
    Result, error,
    event::{ExtractionEvent, KeywordFeedback, KeywordObject, KeywordPolicy, group_with_keywords},
    extract::{Media, MediaJob, PostMetadata},
    file_metadata::FileMetadata,
    network::NetworkOptions,
    transport::{RateLimits, Redirects, retry_after},
};
use futures_util::{
    StreamExt,
    stream::{self, BoxStream},
};
use reqwest::{Client, StatusCode, header};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    sync::Arc,
    time::{Duration, SystemTime},
};
use url::Url;

/// e621 rejects requests without a descriptive User-Agent.
const USER_AGENT: &str = concat!(
    "gallery-dl-rs/",
    env!("CARGO_PKG_VERSION"),
    " (native booru extractor)"
);
const MAX_RESPONSE: usize = 16 * 1024 * 1024;
/// Bound DAPI listings to 100 pages (10,000 Gelbooru posts).
const MAX_PAGES: usize = 100;
const MAX_TAGS: usize = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Site {
    Danbooru,
    E621,
    Gelbooru,
    Safebooru,
}

impl Site {
    pub fn from_category(category: &str) -> Option<Self> {
        Some(match category {
            "danbooru" => Self::Danbooru,
            "e621" => Self::E621,
            "gelbooru" => Self::Gelbooru,
            "safebooru" => Self::Safebooru,
            _ => return None,
        })
    }

    pub fn category(self) -> &'static str {
        match self {
            Self::Danbooru => "danbooru",
            Self::E621 => "e621",
            Self::Gelbooru => "gelbooru",
            Self::Safebooru => "safebooru",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Danbooru => "Danbooru",
            Self::E621 => "e621",
            Self::Gelbooru => "Gelbooru",
            Self::Safebooru => "Safebooru",
        }
    }

    fn danbooru_dialect(self) -> bool {
        matches!(self, Self::Danbooru | Self::E621)
    }

    fn page_limit(self) -> usize {
        match self {
            Self::Danbooru => 200,
            Self::E621 => 320,
            Self::Gelbooru | Self::Safebooru => 100,
        }
    }

    /// HTTPS CDN hosts that may serve this site's files.
    fn media_host(self, host: &str) -> bool {
        match self {
            Self::Danbooru => host == "cdn.donmai.us" || host.ends_with(".donmai.us"),
            Self::E621 => host == "static1.e621.net" || host.ends_with(".e621.net"),
            Self::Gelbooru => host.ends_with(".gelbooru.com"),
            Self::Safebooru => host == "safebooru.org" || host.ends_with(".safebooru.org"),
        }
    }
}

/// A supported booru input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Post {
        id: u64,
    },
    Tags {
        tags: String,
    },
    /// Exact decimal source capture, retaining leading zeroes.
    Pool {
        id: String,
    },
    /// Danbooru favorite group with its exact requested ID.
    Favgroup {
        id: String,
    },
}

fn post_id(raw: &str) -> Option<u64> {
    (1..=12)
        .contains(&raw.len())
        .then(|| raw.bytes().all(|b| b.is_ascii_digit()))
        .filter(|ok| *ok)
        .and_then(|_| raw.parse().ok())
        .filter(|id| *id > 0)
}

/// Booru post IDs are positive decimal integers.
pub fn valid_id(raw: &str) -> bool {
    post_id(raw).is_some_and(|id| id.to_string() == raw)
}

fn valid_tags(tags: &str) -> bool {
    tags.len() <= MAX_TAGS
}

/// Bounded, exact source constructor capture for native-supported tag routes.
/// `Some("")` is a matched empty search; `None` is not a source match (or
/// exceeds the native size bound). Host/query aliases accepted by `target`
/// remain supported independently and use its ordinary query fallback.
pub fn source_search_tags(site: Site, url: &Url) -> Option<String> {
    checked_source_search_tags(site, url).ok().flatten()
}

fn source_search_tags_raw(site: Site, url: &Url) -> Option<&str> {
    if !matches!(url.scheme(), "http" | "https")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    let query = url.query()?;
    Some(match site {
        Site::Danbooru
            if url.host_str() == Some("danbooru.donmai.us") && url.path() == "/posts" =>
        {
            // The source's greedy repeated query prefix selects the last pair.
            query
                .split('&')
                .filter_map(|pair| pair.strip_prefix("tags="))
                .next_back()?
        }
        Site::E621
            if matches!(url.host_str(), Some("e621.net" | "e926.net"))
                && matches!(url.path(), "/post" | "/posts") =>
        {
            // Its lazy regex accepts the first literal substring, even notags=.
            query.get(query.find("tags=")? + 5..)?.split('&').next()?
        }
        Site::Gelbooru
            if matches!(url.host_str(), Some("gelbooru.com" | "www.gelbooru.com"))
                && matches!(url.path(), "/" | "/index.php") =>
        {
            query
                .strip_prefix("page=post&s=list&tags=")?
                .split('&')
                .next()?
        }
        Site::Safebooru
            if url.host_str() == Some("safebooru.org") && url.path() == "/index.php" =>
        {
            query
                .strip_prefix("page=post&s=list&tags=")?
                .split('&')
                .next()?
        }
        _ => return None,
    })
}

fn checked_source_search_tags(site: Site, url: &Url) -> Result<Option<String>> {
    let Some(raw) = source_search_tags_raw(site, url) else {
        return Ok(None);
    };
    // A UTF-8 byte can occupy at most three bytes in percent-encoded form.
    if raw.len() > MAX_TAGS * 3 {
        return Err(error("booru search tags exceed the size limit"));
    }
    let encoded = format!("value={raw}");
    let decoded = url::form_urlencoded::parse(encoded.as_bytes())
        .next()
        .expect("fixed query key")
        .1
        .into_owned();
    if !valid_tags(&decoded) {
        return Err(error("booru search tags exceed the size limit"));
    }
    Ok(Some(decoded))
}

/// Classify a booru URL for `site`, returning the target and API origin.
pub fn target(site: Site, url: &Url) -> Result<(Target, Url)> {
    let unsupported = || {
        error(match site {
            Site::Danbooru => {
                "unsupported booru URL; expected a post, tag search, pool or favorite-group link"
            }
            Site::E621 => {
                "unsupported booru URL; expected a post (/posts/ID), tag search (/posts?tags=...) or pool (/pools/ID) link"
            }
            Site::Gelbooru | Site::Safebooru => {
                "unsupported booru URL; expected index.php?page=post&s=view&id=ID or index.php?page=post&s=list&tags=..."
            }
        })
    };
    if !matches!(url.scheme(), "http" | "https")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(unsupported());
    }
    let host = url.host_str().ok_or_else(unsupported)?;
    let host = host.strip_prefix("www.").unwrap_or(host);
    let root = match (site, host) {
        (Site::Danbooru, "danbooru.donmai.us" | "donmai.us") => "https://danbooru.donmai.us/",
        (Site::E621, "e621.net") => "https://e621.net/",
        (Site::E621, "e926.net") => "https://e926.net/",
        (Site::Gelbooru, "gelbooru.com") => "https://gelbooru.com/",
        (Site::Safebooru, "safebooru.org") => "https://safebooru.org/",
        _ => return Err(unsupported()),
    };
    let query = |key: &str| {
        url.query_pairs()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.into_owned())
    };
    let segments: Vec<_> = url
        .path_segments()
        .map(|s| s.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    // A matched invalid capture must fail; falling back could select a
    // different duplicate query and silently change the requested namespace.
    let source_tags = checked_source_search_tags(site, url)?;
    let target = if let Some((kind, id)) = collection_capture(site, url) {
        Some(match kind {
            CollectionKind::Pool => Target::Pool { id: id.to_owned() },
            CollectionKind::Favgroup => Target::Favgroup { id: id.to_owned() },
        })
    } else if site.danbooru_dialect() {
        match segments.as_slice() {
            ["posts", id] => post_id(id).map(|id| Target::Post { id }),
            ["posts"] => source_tags
                .clone()
                .or_else(|| query("tags"))
                .filter(|t| valid_tags(t))
                .map(|tags| Target::Tags { tags }),
            _ => None,
        }
    } else {
        match (
            segments.as_slice(),
            query("page").as_deref(),
            query("s").as_deref(),
        ) {
            (["index.php"] | [], Some("post"), Some("view")) => query("id")
                .and_then(|id| post_id(&id))
                .map(|id| Target::Post { id }),
            (["index.php"] | [], Some("post"), Some("list")) => source_tags
                .clone()
                .or_else(|| query("tags"))
                .filter(|t| valid_tags(t))
                .map(|tags| Target::Tags { tags }),
            _ => None,
        }
    };
    Ok((target.ok_or_else(unsupported)?, Url::parse(root).unwrap()))
}

fn string<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn number(value: Option<&Value>) -> Option<u64> {
    let value = value?;
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
        .filter(|v| *v > 0)
}

pub(crate) fn valid_md5(md5: &str) -> bool {
    md5.len() == 32 && md5.bytes().all(|b| b.is_ascii_hexdigit())
}

fn valid_extension(ext: &str) -> bool {
    (1..=5).contains(&ext.len()) && ext.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn media_url(site: Site, raw: &str) -> Option<Url> {
    let url = Url::parse(raw).ok()?;
    let host = url.host_str()?;
    (url.scheme() == "https"
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && site.media_host(host))
    .then_some(url)
}

fn url_extension(url: &Url) -> Option<String> {
    let name = url.path_segments()?.next_back()?;
    let (_, ext) = name.rsplit_once('.')?;
    let ext = ext.to_ascii_lowercase();
    valid_extension(&ext).then_some(ext)
}

/// Parse Gelbooru's `Sat Jan 02 03:04:05 -0500 2024` timestamps.
fn gelbooru_date(raw: &str) -> Option<i64> {
    let parts: Vec<_> = raw.split_whitespace().collect();
    let [_, month, day, time, offset, year] = parts.as_slice() else {
        return None;
    };
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .iter()
    .position(|m| m == month)?
        + 1;
    let day: u8 = day.parse().ok()?;
    crate::date::parse_date(&format!("{year}-{month:02}-{day:02}T{time}{offset}")).ok()
}

fn timestamp(site: Site, post: &Value) -> Option<i64> {
    let raw = string(post, "created_at")?;
    if site.danbooru_dialect() {
        crate::date::parse_date(raw).ok()
    } else {
        gelbooru_date(raw).or_else(|| crate::date::parse_date(raw).ok())
    }
}

/// Space-separated tags; e621 groups them by category.
fn tags(site: Site, post: &Value) -> Option<String> {
    let tags = match site {
        Site::E621 => {
            let groups = post.get("tags")?.as_object()?;
            let all: Vec<_> = groups
                .values()
                .filter_map(Value::as_array)
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            all.join(" ")
        }
        Site::Danbooru => string(post, "tag_string")?.to_owned(),
        Site::Gelbooru | Site::Safebooru => string(post, "tags")?
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" "),
    };
    (!tags.is_empty()).then_some(tags)
}

/// Source URL keyword extraction, before URL normalization or native naming.
fn source_nameext(raw: &str) -> (Arc<str>, Arc<str>) {
    let basename = raw
        .split('?')
        .next()
        .unwrap_or_default()
        .rsplit('/')
        .next()
        .unwrap_or_default();
    let raw = basename.as_bytes();
    let mut bytes = Vec::with_capacity(raw.len());
    let mut index = 0;
    while index < raw.len() {
        if raw[index] == b'%'
            && let Some(pair) = raw.get(index + 1..index + 3)
            && pair.iter().all(u8::is_ascii_hexdigit)
            && let Ok(pair) = std::str::from_utf8(pair)
            && let Ok(byte) = u8::from_str_radix(pair, 16)
        {
            bytes.push(byte);
            index += 3;
            continue;
        }
        bytes.push(raw[index]);
        index += 1;
    }
    let filename = String::from_utf8_lossy(&bytes);
    match filename.rsplit_once('.') {
        Some((name, extension)) if !name.is_empty() && extension.chars().count() <= 16 => {
            (Arc::from(name), Arc::from(extension.to_lowercase()))
        }
        _ => (Arc::from(filename.as_ref()), Arc::from("")),
    }
}

/// Source PathFormat.set_filename's default mapping, once before Prepare hooks.
pub(crate) fn prepare_filename_keywords(
    media: &Media,
    keywords: &mut serde_json::Map<String, Value>,
) -> Result<()> {
    if !media
        .metadata
        .as_ref()
        .is_some_and(|metadata| metadata.booru_keywords.is_some())
    {
        return Ok(());
    }
    let extension = keywords
        .get("extension")
        .ok_or_else(|| error("booru extension keyword is missing"))?;
    let mapped = match extension {
        Value::String(value) => match value.as_str() {
            "jpeg" | "jpe" | "jfif" | "jif" | "jfi" => Some("jpg"),
            "html" => Some("htm"),
            _ => None,
        },
        Value::Array(_) | Value::Object(_) => {
            return Err(error("invalid booru extension keyword type"));
        }
        _ => None,
    };
    if let Some(extension) = mapped {
        keywords.insert("extension".into(), extension.into());
    }
    Ok(())
}

fn nameext_keywords(
    site: Site,
    post: &Value,
    fallback_url: &Url,
) -> serde_json::Map<String, Value> {
    let fields = if site == Site::E621 {
        post.get("file").unwrap_or(&Value::Null)
    } else {
        post
    };
    let mut keywords: serde_json::Map<String, Value> =
        if site == Site::E621 || (!site.danbooru_dialect() && post.get("extension").is_some()) {
            [
                (
                    "filename",
                    if site == Site::E621 {
                        "md5"
                    } else {
                        "filename"
                    },
                ),
                (
                    "extension",
                    if site == Site::E621 {
                        "ext"
                    } else {
                        "extension"
                    },
                ),
            ]
            .into_iter()
            .filter_map(|(key, source)| {
                fields
                    .get(source)
                    .map(|value| (key.to_owned(), value.clone()))
            })
            .collect()
        } else {
            let raw = post
                .get("file_url")
                .and_then(Value::as_str)
                .unwrap_or(fallback_url.as_str());
            let (filename, extension) = source_nameext(raw);
            serde_json::Map::from_iter([
                ("filename".into(), filename.as_ref().into()),
                ("extension".into(), extension.as_ref().into()),
            ])
        };
    if let Some(md5) = post.get("md5") {
        keywords.insert("md5".into(), md5.clone());
    }
    keywords
}

/// Parse one post object. Posts without a usable file yield `None`.
pub fn parse_post(site: Site, post: &Value) -> Option<Media> {
    let id = number(post.get("id"))?;
    let (url, md5, ext, width, height, size) = match site {
        Site::Danbooru => (
            media_url(site, string(post, "file_url")?)?,
            string(post, "md5"),
            string(post, "file_ext"),
            number(post.get("image_width")),
            number(post.get("image_height")),
            number(post.get("file_size")),
        ),
        Site::E621 => {
            let file = post.get("file")?;
            (
                media_url(site, string(file, "url")?)?,
                string(file, "md5"),
                string(file, "ext"),
                number(file.get("width")),
                number(file.get("height")),
                number(file.get("size")),
            )
        }
        Site::Gelbooru => (
            media_url(site, string(post, "file_url")?)?,
            string(post, "md5"),
            None,
            number(post.get("width")),
            number(post.get("height")),
            None,
        ),
        Site::Safebooru => {
            let url = if let Some(raw) = post.get("file_url").and_then(Value::as_str) {
                media_url(site, raw)?
            } else {
                let directory = string(post, "directory")?;
                let image = string(post, "image")?;
                let safe = |s: &str, dot: bool| {
                    s.len() <= 128
                        && s.bytes().all(|b| {
                            b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || dot && b == b'.'
                        })
                        && !s.starts_with('.')
                };
                if !safe(directory, false) || !safe(image, true) {
                    return None;
                }
                media_url(
                    site,
                    &format!("https://safebooru.org/images/{directory}/{image}"),
                )?
            };
            (
                url,
                string(post, "hash").or_else(|| string(post, "md5")),
                None,
                number(post.get("width")),
                number(post.get("height")),
                None,
            )
        }
    };
    let md5 = md5.map(str::to_ascii_lowercase).filter(|m| valid_md5(m));
    let ext = ext
        .map(str::to_ascii_lowercase)
        .filter(|e| valid_extension(e))
        .or_else(|| url_extension(&url))
        .unwrap_or_else(|| "bin".into());
    let ext = if ext == "jpeg" { "jpg".into() } else { ext };
    let id = id.to_string();
    let media_id = md5.clone().unwrap_or_else(|| id.clone());
    let category = site.category();
    let username = match site {
        Site::Danbooru | Site::E621 => string(post, "uploader_name"),
        Site::Gelbooru | Site::Safebooru => string(post, "owner"),
    }
    .map(Arc::from);
    let media_type = match ext.as_str() {
        "jpg" | "png" | "gif" | "webp" | "avif" => {
            Some(format!("image/{}", ext.replace("jpg", "jpeg")))
        }
        "mp4" | "webm" => Some(format!("video/{ext}")),
        _ => None,
    };
    let booru_keywords = Some(Arc::new(nameext_keywords(site, post, &url)));
    Some(Media {
        url,
        job: MediaJob::DirectHttp,
        category: category.into(),
        filename: format!("{category}_{id}_{media_id}.{ext}"),
        metadata: Some(PostMetadata {
            booru_keywords,
            file: Some(FileMetadata {
                width,
                height,
                filesize: size,
                media_type: media_type.map(Arc::from),
                ..FileMetadata::default()
            }),
            rating: string(post, "rating").map(Arc::from),
            score: {
                let score = post.get("score").and_then(|score| {
                    if site == Site::E621 {
                        score.get("total")
                    } else {
                        Some(score)
                    }
                });
                score.and_then(|score| score.as_i64().or_else(|| score.as_str()?.parse().ok()))
            },
            post_id: id,
            media_id,
            username,
            caption: tags(site, post).map(Arc::from),
            timestamp: timestamp(site, post),
            index: 1,
            ..PostMetadata::default()
        }),
        headers: BTreeMap::new(),
        handoff: BTreeMap::new(),
    })
}

fn invalid(site: Site) -> crate::Error {
    error(format!("{} returned invalid JSON", site.name()))
}

/// Extract the raw post objects from a single-post or listing response.
pub fn posts(site: Site, bytes: &[u8]) -> Result<Vec<Value>> {
    if site == Site::Safebooru && bytes.iter().all(u8::is_ascii_whitespace) {
        // Safebooru answers an empty result with an empty body.
        return Ok(Vec::new());
    }
    let body: Value = serde_json::from_slice(bytes).map_err(|_| invalid(site))?;
    let list = match (site, body) {
        (Site::Danbooru | Site::Safebooru, Value::Array(list)) => list,
        (Site::Danbooru, post @ Value::Object(_)) => vec![post],
        (Site::E621, Value::Object(mut body)) => {
            match (body.remove("posts"), body.remove("post")) {
                (Some(Value::Array(list)), _) => list,
                (_, Some(post @ Value::Object(_))) => vec![post],
                _ => return Err(invalid(site)),
            }
        }
        (Site::Gelbooru, Value::Object(mut body)) => match body.remove("post") {
            Some(Value::Array(list)) => list,
            Some(post @ Value::Object(_)) => vec![post],
            None => Vec::new(),
            _ => return Err(invalid(site)),
        },
        _ => return Err(invalid(site)),
    };
    Ok(list)
}

/// Parse a collection while retaining its original member positions.
fn collection_metadata(
    site: Site,
    bytes: &[u8],
) -> Result<(serde_json::Map<String, Value>, Vec<u64>)> {
    let Value::Object(mut body) =
        serde_json::from_slice::<Value>(bytes).map_err(|_| invalid(site))?
    else {
        return Err(invalid(site));
    };
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(site))?;
    body.insert("name".into(), name.replace('_', " ").into());
    let ids = match body.remove("post_ids") {
        None => Vec::new(),
        Some(Value::Array(ids)) => ids.iter().filter_map(|id| number(Some(id))).collect(),
        Some(_) => return Err(invalid(site)),
    };
    Ok((body, ids))
}

/// Parse a `/pools/ID.json` response into ordered post IDs.
pub fn pool_ids(site: Site, bytes: &[u8]) -> Result<Vec<u64>> {
    let body: Value = serde_json::from_slice(bytes).map_err(|_| invalid(site))?;
    let ids = body
        .get("post_ids")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid(site))?;
    Ok(ids.iter().filter_map(|id| number(Some(id))).collect())
}

/// Danbooru collection order. E621 always restores the pool member order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PoolOrder {
    #[default]
    Pool,
    PoolReverse,
    IdDescending,
    IdAscending,
}

pub struct Booru {
    pool_order: PoolOrder,
    site: Site,
    client: Client,
    network: NetworkOptions,
    api_root: Option<Url>,
    credentials: Option<(String, String)>,
    api_credentials: Option<(String, String)>,
    timeout: Duration,
    request_timeout: Duration,
    retries: u8,
    rate_limits: RateLimits,
    threshold: Option<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TagOrder {
    Before,
    After,
    Numbered,
}
impl TagOrder {
    fn for_tags(tags: &str) -> Self {
        let mut order = Self::Before;
        // Python str.split also recognizes the four ASCII record separators.
        for tag in tags
            .split(|c: char| c.is_whitespace() || matches!(c, '\u{1c}'..='\u{1f}'))
            .filter(|tag| !tag.is_empty())
        {
            if tag.starts_with("order:") {
                order = match tag {
                    "order:id" | "order:id_asc" => Self::After,
                    "order:id_desc" => Self::Before,
                    _ => Self::Numbered,
                };
            } else if ["id:", "md5:", "ordfav:", "ordfavgroup:", "ordpool:"]
                .iter()
                .any(|prefix| tag.starts_with(prefix))
            {
                return Self::Numbered;
            }
        }
        order
    }
}
enum Cursor {
    /// Source tag ordering selects before/after IDs or numbered API pages.
    Tags {
        order: TagOrder,
        page: Option<String>,
    },
    /// Gelbooru `pid` page number.
    Page(usize),
}

struct PendingTagCursor {
    last_id: Option<Value>,
    feedback: KeywordFeedback,
}

#[derive(Clone, Copy)]
enum CollectionKind {
    Pool,
    Favgroup,
}
impl CollectionKind {
    fn keyword(self) -> &'static str {
        match self {
            Self::Pool => "pool",
            Self::Favgroup => "favgroup",
        }
    }
    fn endpoint(self) -> &'static str {
        match self {
            Self::Pool => "pools",
            Self::Favgroup => "favorite_groups",
        }
    }
}
fn collection_capture(site: Site, url: &Url) -> Option<(CollectionKind, &str)> {
    if !site.danbooru_dialect() {
        return None;
    }
    let path = url.path();
    for (prefix, kind) in [
        ("/pools/", CollectionKind::Pool),
        ("/pool/show/", CollectionKind::Pool),
        ("/favorite_groups/", CollectionKind::Favgroup),
        ("/favorite_group/show/", CollectionKind::Favgroup),
    ] {
        if matches!(kind, CollectionKind::Favgroup) && site != Site::Danbooru {
            continue;
        }
        if let Some(rest) = path.strip_prefix(prefix) {
            let end = rest.bytes().take_while(u8::is_ascii_digit).count();
            if end != 0 {
                return Some((kind, &rest[..end]));
            }
        }
    }
    None
}

struct Collection {
    kind: CollectionKind,
    metadata: KeywordObject,
    positions: HashMap<u64, usize>,
}

struct State {
    collection: Option<Collection>,
    root: Url,
    tags: String,
    search_tags: Option<Arc<str>>,
    cursor: Cursor,
    pages: usize,
    done: bool,
    pending: VecDeque<ExtractionEvent>,
    pending_cursor: Option<PendingTagCursor>,
    pending_error: Option<&'static str>,
}

impl Booru {
    pub fn new(site: Site, timeout: Duration) -> Result<Self> {
        Ok(Self {
            pool_order: PoolOrder::Pool,
            site,
            client: NetworkOptions::default().client(USER_AGENT, timeout)?,
            network: NetworkOptions::default(),
            api_root: None,
            credentials: None,
            api_credentials: None,
            timeout,
            request_timeout: timeout,
            retries: 0,
            rate_limits: RateLimits::default(),
            threshold: None,
        })
    }

    /// Apply proxy and user-agent settings to API requests and emitted media.
    pub fn with_network(mut self, network: &NetworkOptions) -> Result<Self> {
        self.client = network.client(USER_AGENT, self.timeout)?;
        self.network = network.clone();
        Ok(self)
    }

    /// Danbooru/e621 use HTTP Basic authentication with a username and API key.
    /// Credentials are API-only and never become media headers or metadata.
    pub fn with_credentials(
        mut self,
        username: Option<&str>,
        password: Option<&str>,
    ) -> Result<Self> {
        if username.is_some_and(|value| {
            value.len() > 256 || value.contains(':') || value.chars().any(char::is_control)
        }) || password
            .is_some_and(|value| value.len() > 4096 || value.chars().any(char::is_control))
        {
            return Err(error("invalid booru username or password"));
        }
        self.credentials = if self.site.danbooru_dialect() {
            username
                .filter(|value| !value.is_empty())
                .map(|username| (username.to_owned(), password.unwrap_or_default().to_owned()))
        } else {
            None
        };
        Ok(self)
    }

    /// Gelbooru uses the configured `user-id` and `api-key` query parameters.
    pub fn with_api_credentials(
        mut self,
        user_id: Option<&str>,
        api_key: Option<&str>,
    ) -> Result<Self> {
        if user_id.is_some_and(|value| {
            !value.is_empty()
                && (value.len() > 20
                    || !value.bytes().all(|byte| byte.is_ascii_digit())
                    || !value.bytes().any(|byte| byte != b'0'))
        }) || api_key.is_some_and(|value| {
            value.len() > 4096 || !value.bytes().all(|byte| byte.is_ascii_graphic())
        }) {
            return Err(error("invalid booru user-id or api-key"));
        }
        self.api_credentials = if self.site == Site::Gelbooru {
            user_id
                .filter(|value| !value.is_empty())
                .zip(api_key.filter(|value| !value.is_empty()))
                .map(|(user, key)| (user.to_owned(), key.to_owned()))
        } else {
            None
        };
        Ok(self)
    }

    /// Additional attempts for HTTP 429/5xx and transport failures.
    pub fn with_retries(mut self, retries: u8) -> Self {
        self.retries = retries;
        self
    }

    /// Select Danbooru pool traversal order; e621 retains member order.
    pub fn with_pool_order(mut self, order: PoolOrder) -> Self {
        self.pool_order = order;
        self
    }

    /// Minimum raw page length that continues Danbooru/e621 searches.
    /// None uses the source default of twenty fewer than the API page size.
    pub fn with_threshold(mut self, threshold: Option<usize>) -> Self {
        self.threshold = threshold.map(|value| value.max(1));
        self
    }

    /// Bound each HTTP request separately from the per-call deadline.
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// Override the API origin; intended for local tests.
    pub fn with_api_root(mut self, root: Url) -> Self {
        self.api_root = Some(root);
        self
    }

    async fn call(&self, root: &Url, path: &str, query: &[(&str, &str)]) -> Result<Vec<u8>> {
        let mut endpoint = root.join(path).expect("validated booru path");
        if !query.is_empty() {
            endpoint.query_pairs_mut().extend_pairs(query);
        }
        tokio::time::timeout(self.timeout, self.request(&endpoint))
            .await
            .map_err(|_| error(format!("native {} extraction timed out", self.site.name())))?
    }

    fn dapi<'q>(extra: &[(&'q str, &'q str)]) -> Vec<(&'q str, &'q str)> {
        let mut query = vec![
            ("page", "dapi"),
            ("s", "post"),
            ("q", "index"),
            ("json", "1"),
        ];
        query.extend_from_slice(extra);
        query
    }

    async fn post(&self, root: &Url, id: u64) -> Result<Vec<Value>> {
        let id = id.to_string();
        let body = if self.site.danbooru_dialect() {
            self.call(root, &format!("posts/{id}.json"), &[]).await?
        } else {
            self.call(root, "index.php", &Self::dapi(&[("id", &id)]))
                .await?
        };
        let posts = posts(self.site, &body)?;
        if !posts
            .iter()
            .any(|p| number(p.get("id")).map(|v| v.to_string()) == Some(id.clone()))
        {
            return Err(error(format!("{} post not found", self.site.name())));
        }
        Ok(posts)
    }

    fn emit(&self, state: &mut State, raw: &[Value]) {
        let collection_snapshot = state
            .collection
            .as_ref()
            .map(|collection| Arc::new(Value::Object(collection.metadata.snapshot())));
        for (position, post) in raw.iter().enumerate() {
            if let Some(collection) = &state.collection
                && !number(post.get("id")).is_some_and(|id| collection.positions.contains_key(&id))
            {
                state.pending_error = Some("invalid booru collection member");
                break;
            }
            if let Some(mut media) = parse_post(self.site, post) {
                if let Some(metadata) = &mut media.metadata {
                    metadata.search_tags = state.search_tags.clone();
                    if let Some(collection) = &state.collection {
                        match collection.kind {
                            CollectionKind::Pool => metadata.pool = collection_snapshot.clone(),
                            CollectionKind::Favgroup => {
                                metadata.favgroup = collection_snapshot.clone()
                            }
                        }
                        metadata.source_num = number(post.get("id"))
                            .and_then(|id| collection.positions.get(&id))
                            .map(|num| (*num).into());
                    }
                }
                self.network.apply_media(&mut media);
                // DAPI search metadata lives in extractor.kwdict and the source
                // Job reapplies it before every message, including each file.
                let fields: &[&str] =
                    if state.search_tags.is_some() && !self.site.danbooru_dialect() {
                        &["num", "index", "media_id", "search_tags"]
                    } else if state.collection.is_some() {
                        &["index", "media_id"]
                    } else {
                        &["num", "index", "media_id"]
                    };
                let post_fields: &[&str] = if state.collection.is_some() {
                    &["filename", "name", "extension", "num"]
                } else {
                    &["filename", "name", "extension"]
                };
                let mut keywords =
                    KeywordPolicy::shared(fields).with_post_fields(&media, post_fields);
                if let Some(collection) = &state.collection {
                    keywords = keywords
                        .with_shared_object(collection.kind.keyword(), collection.metadata.clone());
                }
                if position + 1 == raw.len()
                    && let Some(cursor) = &state.pending_cursor
                {
                    keywords = keywords.with_feedback(cursor.feedback.clone());
                }
                let source_num = media
                    .metadata
                    .as_ref()
                    .and_then(|metadata| metadata.source_num.clone());
                let mut events = group_with_keywords(vec![media], self.site.category(), keywords);
                if state.collection.is_some() {
                    for event in &mut events {
                        if let ExtractionEvent::Post(post) = event
                            && let Some(metadata) = &mut post.metadata
                        {
                            metadata.source_num = source_num.clone();
                        }
                    }
                }
                state.pending.extend(events);
            }
        }
    }

    async fn next_page(&self, state: &mut State) -> Result<()> {
        let limit = self.site.page_limit();
        let limit_text = limit.to_string();
        if !matches!(state.cursor, Cursor::Tags { .. }) {
            state.pages += 1;
            if state.pages >= MAX_PAGES {
                state.done = true;
            }
        }
        match &mut state.cursor {
            Cursor::Tags { order, page } => {
                if let Some(cursor) = state.pending_cursor.take() {
                    *page = Some(match order {
                        TagOrder::Before | TagOrder::After => {
                            let value = cursor
                                .feedback
                                .take()
                                .unwrap_or(cursor.last_id)
                                .ok_or_else(|| error("invalid booru pagination cursor"))?;
                            let context = serde_json::Map::from_iter([("id".into(), value)]);
                            let id = crate::postprocess::template::Template::parse("{id}")?
                                .render(&context)?;
                            format!("{}{id}", if *order == TagOrder::Before { 'b' } else { 'a' })
                        }
                        TagOrder::Numbered => match page.as_deref() {
                            None => "2".into(),
                            Some(value) => value
                                .parse::<usize>()
                                .ok()
                                .and_then(|value| value.checked_add(1))
                                .map(|value| value.to_string())
                                .ok_or_else(|| error("booru pagination page overflow"))?,
                        },
                    });
                }
                let mut query = vec![("tags", state.tags.as_str()), ("limit", &limit_text)];
                if let Some(page) = page.as_ref() {
                    query.push(("page", page));
                }
                let body = self.call(&state.root, "posts.json", &query).await?;
                let mut raw = posts(self.site, &body)?;
                if *order == TagOrder::After && page.is_some() {
                    raw.reverse();
                }
                state.done = raw.len() < self.threshold.unwrap_or(limit - 20);
                if !state.done {
                    // Source chooses the cursor only after the consumer resumes
                    // the final yielded dictionary. Hooks can change its ID.
                    state.pending_cursor = Some(PendingTagCursor {
                        last_id: raw.last().and_then(|post| post.get("id")).cloned(),
                        feedback: KeywordFeedback::new("id"),
                    });
                }
                self.emit(state, &raw);
            }
            Cursor::Page(pid) => {
                let pid_text = pid.to_string();
                *pid += 1;
                let query = Self::dapi(&[
                    ("tags", &state.tags),
                    ("limit", &limit_text),
                    ("pid", &pid_text),
                ]);
                let body = self.call(&state.root, "index.php", &query).await?;
                let raw = posts(self.site, &body)?;
                if raw.len() < limit {
                    state.done = true;
                }
                self.emit(state, &raw);
            }
        }
        Ok(())
    }

    pub fn stream(&self, url: &Url) -> BoxStream<'_, Result<Media>> {
        crate::event::media_only(self.events(url))
    }

    /// Emit one post boundary per booru post; listings paginate lazily.
    pub fn events(&self, url: &Url) -> BoxStream<'_, Result<ExtractionEvent>> {
        let (target, root) = match target(self.site, url) {
            Ok(parsed) => parsed,
            Err(e) => return stream::once(async move { Err(e) }).boxed(),
        };
        let root = self.api_root.clone().unwrap_or(root);
        let (tags, cursor) = match target {
            Target::Post { id } => {
                return stream::once(async move {
                    let raw = self.post(&root, id).await?;
                    let mut state = State {
                        collection: None,
                        root,
                        tags: String::new(),
                        search_tags: None,
                        cursor: Cursor::Page(0),
                        pages: 0,
                        done: true,
                        pending: VecDeque::new(),
                        pending_cursor: None,
                        pending_error: None,
                    };
                    self.emit(&mut state, &raw);
                    if state.pending.is_empty() {
                        return Err(error(format!(
                            "{} post has no downloadable file (deleted, banned or restricted)",
                            self.site.name()
                        )));
                    }
                    Ok::<_, crate::Error>(stream::iter(state.pending.into_iter().map(Ok)))
                })
                .map(|result| match result {
                    Ok(events) => events.boxed(),
                    Err(e) => stream::once(async move { Err(e) }).boxed(),
                })
                .flatten()
                .boxed();
            }
            Target::Tags { tags } if self.site.danbooru_dialect() => {
                let order = TagOrder::for_tags(&tags);
                (tags, Cursor::Tags { order, page: None })
            }
            Target::Tags { tags } => (tags, Cursor::Page(0)),
            collection @ (Target::Pool { .. } | Target::Favgroup { .. }) => {
                let (kind, id) = match collection {
                    Target::Pool { id } => (CollectionKind::Pool, id),
                    Target::Favgroup { id } => (CollectionKind::Favgroup, id),
                    _ => unreachable!(),
                };
                return stream::once(async move {
                    let body = self
                        .call(&root, &format!("{}/{id}.json", kind.endpoint()), &[])
                        .await?;
                    let (metadata, ids) = collection_metadata(self.site, &body)?;
                    let positions = ids
                        .iter()
                        .enumerate()
                        .map(|(num, id)| (*id, num + 1))
                        .collect();
                    let mut state = State {
                        collection: Some(Collection {
                            kind,
                            metadata: KeywordObject::new(metadata),
                            positions,
                        }),
                        root,
                        tags: format!(
                            "{}{}:{id}",
                            if self.site == Site::Danbooru
                                && matches!(
                                    self.pool_order,
                                    PoolOrder::Pool | PoolOrder::PoolReverse
                                )
                            {
                                "ord"
                            } else {
                                ""
                            },
                            kind.keyword()
                        ),
                        search_tags: None,
                        done: false,
                        cursor: Cursor::Tags {
                            order: if self.site == Site::Danbooru
                                && matches!(
                                    self.pool_order,
                                    PoolOrder::Pool
                                        | PoolOrder::PoolReverse
                                        | PoolOrder::IdAscending
                                ) {
                                TagOrder::Numbered
                            } else {
                                TagOrder::Before
                            },
                            page: None,
                        },
                        pages: 0,
                        pending: VecDeque::new(),
                        pending_cursor: None,
                        pending_error: None,
                    };
                    if self.site == Site::E621
                        || matches!(
                            self.pool_order,
                            PoolOrder::PoolReverse | PoolOrder::IdAscending
                        )
                    {
                        // e621 consumes the complete API feed before yielding,
                        // retaining the last response for every repeated ID.
                        let mut by_id = HashMap::new();
                        let mut all = Vec::new();
                        let mut page_number = 1;
                        let mut page: Option<String> = None;
                        loop {
                            let limit = self.site.page_limit().to_string();
                            let mut query =
                                vec![("tags", state.tags.as_str()), ("limit", limit.as_str())];
                            if let Some(page) = &page {
                                query.push(("page", page));
                            }
                            let body = self.call(&state.root, "posts.json", &query).await?;
                            let raw = posts(self.site, &body)?;
                            let done =
                                raw.len() < self.threshold.unwrap_or(self.site.page_limit() - 20);
                            page_number += 1;
                            page = Some(page_number.to_string());
                            for post in raw {
                                if self.site == Site::E621 {
                                    if let Some(id) = number(post.get("id")) {
                                        by_id.insert(id, post);
                                    }
                                } else {
                                    all.push(post);
                                }
                            }
                            if done {
                                break;
                            }
                        }
                        let mut duplicate = false;
                        let ordered: Vec<_> = if self.site == Site::E621 {
                            let mut seen = std::collections::HashSet::new();
                            ids.iter()
                                .filter_map(|id| by_id.get(id).map(|post| (*id, post)))
                                .take_while(|(id, _)| {
                                    if seen.insert(*id) {
                                        true
                                    } else {
                                        duplicate = true;
                                        false
                                    }
                                })
                                .map(|(_, post)| post.clone())
                                .collect()
                        } else {
                            all.reverse();
                            all
                        };
                        if self.site == Site::Danbooru
                            && ordered.iter().any(|post| {
                                !number(post.get("id")).is_some_and(|id| {
                                    state
                                        .collection
                                        .as_ref()
                                        .unwrap()
                                        .positions
                                        .contains_key(&id)
                                })
                            })
                        {
                            return Err(error("invalid booru collection member"));
                        }
                        state.done = true;
                        self.emit(&mut state, &ordered);
                        if duplicate {
                            state.pending_error = Some("invalid repeated e621 pool member");
                        }
                    }
                    Ok::<_, crate::Error>(self.listing(state))
                })
                .map(|result| match result {
                    Ok(events) => events,
                    Err(e) => stream::once(async move { Err(e) }).boxed(),
                })
                .flatten()
                .boxed();
            }
        };
        let search_tags = Some(Arc::from(tags.as_str()));
        let tags = if !self.site.danbooru_dialect() && tags == "all" {
            String::new()
        } else {
            tags
        };
        self.listing(State {
            collection: None,
            root,
            tags,
            search_tags,
            cursor,
            pages: 0,
            done: false,
            pending: VecDeque::new(),
            pending_cursor: None,
            pending_error: None,
        })
    }

    fn listing(&self, state: State) -> BoxStream<'_, Result<ExtractionEvent>> {
        stream::unfold(Some(state), move |state| async move {
            let mut state = state?;
            loop {
                if let Some(event) = state.pending.pop_front() {
                    return Some((Ok(event), Some(state)));
                }
                if let Some(message) = state.pending_error.take() {
                    return Some((Err(error(message)), None));
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
        let name = self.site.name();
        let mut target = endpoint.clone();
        let mut redirects = Redirects::new(endpoint);
        let mut response = loop {
            self.rate_limits.wait(&target, self.timeout).await?;
            // Keep the canonical URL free of secrets. Redirects are restricted
            // to this origin, and credentials are added only to each API request.
            let mut request_url = target.clone();
            if let Some((user_id, api_key)) = &self.api_credentials {
                let query: Vec<_> = request_url
                    .query_pairs()
                    .filter(|(key, _)| !matches!(key.as_ref(), "user_id" | "api_key"))
                    .map(|(key, value)| (key.into_owned(), value.into_owned()))
                    .collect();
                request_url.set_query(None);
                request_url.query_pairs_mut().extend_pairs(query);
                request_url
                    .query_pairs_mut()
                    .extend_pairs([("user_id", user_id.as_str()), ("api_key", api_key.as_str())]);
            }
            let mut request = self
                .client
                .get(request_url)
                .timeout(self.request_timeout)
                .header(header::ACCEPT, "application/json");
            if let Some((username, password)) = &self.credentials {
                request = request.basic_auth(username, Some(password));
            }
            let response = request.send().await.map_err(|_| {
                Failure::transient(format!("{name} request failed; check network access"))
            })?;
            match redirects.next(&target, response.status(), response.headers())? {
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
                            format!("{name} rate limit; retry after {} seconds", delay.as_secs())
                        }
                        None => format!("{name} rate limit; retry later"),
                    }
                } else {
                    format!("{name} returned HTTP {status}")
                },
            ));
        }
        if matches!(status.as_u16(), 404 | 410) {
            return Err(error(format!("{name} post or collection not found")).into());
        }
        if matches!(status.as_u16(), 401 | 403) {
            return Err(error(format!(
                "{name} refused access (login, API key or a descriptive User-Agent may be required)"
            ))
            .into());
        }
        if !status.is_success() {
            return Err(error(format!("{name} returned HTTP {status}")).into());
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| Failure::transient(format!("could not read {name} response")))?
        {
            if body.len() + chunk.len() > MAX_RESPONSE {
                return Err(error(format!("{name} response exceeded 16 MiB")).into());
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
#[path = "booru_pagination_tests.rs"]
mod pagination_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::group;
    use serde_json::json;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread::{self, JoinHandle},
    };

    const MD5: &str = "0123456789abcdef0123456789abcdef";

    pub(super) fn danbooru_post(id: u64) -> Value {
        json!({"id": id, "created_at": "2024-01-02T03:04:05.123-05:00", "md5": MD5,
               "file_ext": "png", "image_width": 800, "image_height": 600, "file_size": 1234,
               "tag_string": "1girl solo", "uploader_name": "alice",
               "file_url": format!("https://cdn.donmai.us/original/01/23/{MD5}.png")})
    }

    pub(super) fn e621_post(id: u64) -> Value {
        json!({"id": id, "created_at": "2024-01-02T08:04:05.000Z",
               "file": {"url": format!("https://static1.e621.net/data/01/23/{MD5}.webm"),
                        "ext": "webm", "md5": MD5, "width": 1920, "height": 1080, "size": 9},
               "tags": {"general": ["a", "b"], "artist": ["c"]}})
    }

    fn fixture_site(family: &str) -> Site {
        Site::from_category(family).unwrap()
    }

    #[test]
    fn source_search_constructor_and_emitted_keys_match_all_recorded_cases() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/source_archive/booru_search.json"
        ))
        .unwrap();
        let mut matched = 0;
        for case in fixture["cases"].as_array().unwrap() {
            let site = fixture_site(case["family"].as_str().unwrap());
            let url = Url::parse(case["url"].as_str().unwrap()).unwrap();
            let capture = source_search_tags(site, &url);
            if case["error"] == "NoExtractor" {
                assert!(capture.is_none(), "{}", case["name"]);
                continue;
            }
            assert!(case["error"].is_null(), "{}", case["name"]);
            let source = case["events"][1]["metadata"]["search_tags"]
                .as_str()
                .unwrap();
            assert_eq!(capture.as_deref(), Some(source), "{}", case["name"]);
            assert_eq!(
                target(site, &url).unwrap().0,
                Target::Tags {
                    tags: source.into()
                },
                "{}",
                case["name"]
            );
            let wire = if !site.danbooru_dialect() && source == "all" {
                ""
            } else {
                source
            };
            assert_eq!(wire, case["calls"][0]["params"]["tags"], "{}", case["name"]);
            // Prefix selection is recorded, not asserted as implemented:
            // ascending/non-ID source pagination remains a separate parity gap.
            let client = Booru::new(site, Duration::from_secs(1)).unwrap();
            let mut state = State {
                collection: None,
                root: Url::parse("https://example.invalid/").unwrap(),
                tags: wire.into(),
                search_tags: Some(Arc::from(source)),
                cursor: Cursor::Page(0),
                pages: 0,
                done: false,
                pending: VecDeque::new(),
                pending_cursor: None,
                pending_error: None,
            };
            client.emit(&mut state, std::slice::from_ref(&case["input_post"]));
            let ExtractionEvent::Post(post) = state.pending.pop_front().unwrap() else {
                panic!()
            };
            assert_eq!(
                post.metadata.as_ref().unwrap().search_tags.as_deref(),
                Some(source)
            );
            let KeywordPolicy::Shared { file_fields, .. } = &post.keywords else {
                panic!()
            };
            assert_eq!(
                file_fields.iter().any(|field| field == "search_tags"),
                !site.danbooru_dialect()
            );
            let ExtractionEvent::Media(media) = state.pending.pop_front().unwrap() else {
                panic!()
            };
            let context = crate::postprocess::context(&media);
            assert_eq!(context["search_tags"], source);
            assert_eq!(context["metadata"]["search_tags"], source);
            if case["name"].as_str().unwrap().ends_with("_plain") {
                let filter = crate::filter::FilterExpression::parse(
                    "search_tags == metadata['search_tags'] and search_tags == 'Cat'",
                )
                .unwrap();
                assert!(filter.matches(&media).unwrap());
                assert!(filter.matches_post(&post).unwrap());
                assert_eq!(
                    crate::naming::FilenameTemplate::parse("{search_tags}")
                        .unwrap()
                        .render(&media)
                        .unwrap(),
                    "Cat"
                );
            }
            let template = crate::postprocess::template::Template::parse(
                case["archive_format"].as_str().unwrap(),
            )
            .unwrap();
            assert_eq!(
                template.render(&context).unwrap(),
                case["events"][1]["archive_key"],
                "{}",
                case["name"]
            );
            assert_eq!(
                media.filename, case["events"][1]["default_filename"],
                "{}",
                case["name"]
            );
            assert_eq!(
                serde_json::to_value(&media.metadata).unwrap()["search_tags"],
                source
            );
            assert!(matches!(
                state.pending.pop_front(),
                Some(ExtractionEvent::EndPost)
            ));
            matched += 1;
        }
        assert_eq!(matched, 87);
    }

    #[test]
    fn search_source_matching_preserves_native_alias_fallback_and_bounds() {
        for (site, raw) in [
            (Site::Danbooru, "https://donmai.us/posts?tags=Cat+Dog"),
            (Site::E621, "https://www.e621.net/posts?tags=Cat+Dog"),
            (
                Site::Gelbooru,
                "https://gelbooru.com/index.php?s=list&page=post&tags=Cat+Dog",
            ),
            (
                Site::Safebooru,
                "https://www.safebooru.org/index.php?page=post&s=list&tags=Cat+Dog",
            ),
        ] {
            let url = Url::parse(raw).unwrap();
            assert!(source_search_tags(site, &url).is_none());
            assert_eq!(
                target(site, &url).unwrap().0,
                Target::Tags {
                    tags: "Cat Dog".into()
                }
            );
        }
        let empty = Url::parse("https://danbooru.donmai.us/posts?tags=").unwrap();
        assert_eq!(
            source_search_tags(Site::Danbooru, &empty),
            Some(String::new())
        );
        assert!(
            source_search_tags(
                Site::Danbooru,
                &Url::parse("https://danbooru.donmai.us/posts").unwrap()
            )
            .is_none()
        );
        let oversized = Url::parse(&format!(
            "https://danbooru.donmai.us/posts?tags={}",
            "a".repeat(MAX_TAGS + 1)
        ))
        .unwrap();
        assert!(source_search_tags(Site::Danbooru, &oversized).is_none());
        assert!(target(Site::Danbooru, &oversized).is_err());
        for (site, raw) in [
            (
                Site::Danbooru,
                format!(
                    "https://danbooru.donmai.us/posts?tags=First&tags={}",
                    "a".repeat(MAX_TAGS + 1)
                ),
            ),
            (
                Site::E621,
                format!(
                    "https://e621.net/posts?notags={}&tags=First",
                    "a".repeat(MAX_TAGS + 1)
                ),
            ),
        ] {
            let url = Url::parse(&raw).unwrap();
            assert!(source_search_tags(site, &url).is_none());
            let failure = target(site, &url).unwrap_err().to_string();
            assert_eq!(failure, "booru search tags exceed the size limit");
        }
    }

    #[tokio::test]
    async fn real_post_and_file_hooks_follow_source_search_keyword_ownership() {
        use crate::postprocess::{
            Postprocessors, Specification,
            jobs::{self, JobConfiguration},
        };
        let fixture: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/source_archive/booru_search.json"
        ))
        .unwrap();
        for case in fixture["ownership_cases"].as_array().unwrap() {
            let site = fixture_site(case["family"].as_str().unwrap());
            let root = tempfile::tempdir().unwrap();
            let source = Url::parse(case["url"].as_str().unwrap()).unwrap();
            let processors = Arc::new(Postprocessors::new([
                json!({"name":"metadata","mode":"modify","event":"post","fields":{"search_tags":"HOOK"}}),
                json!({"name":"metadata","mode":"custom","event":"file","base-directory":root.path(),"directory":[],"filename":"seen.txt","format":"{search_tags}","archive":root.path().join("archive.sqlite")}),
            ].iter().map(|value| Specification::parse(value, root.path()).unwrap()).collect()));
            let configurations = Arc::new(std::collections::BTreeMap::from([(
                site.category().into(),
                JobConfiguration {
                    processors: processors.clone(),
                    root: root.path().into(),
                    filename: None,
                    directory: None,
                    init: false,
                },
            )]));
            let client = Booru::new(site, Duration::from_secs(1)).unwrap();
            let mut state = State {
                collection: None,
                root: Url::parse("https://example.invalid/").unwrap(),
                tags: "Original".into(),
                search_tags: Some(Arc::from("Original")),
                cursor: Cursor::Page(0),
                pages: 0,
                done: false,
                pending: VecDeque::new(),
                pending_cursor: None,
                pending_error: None,
            };
            client.emit(&mut state, std::slice::from_ref(&case["input_post"]));
            // Exercise publication and file hooks without any HTTP. The native
            // producer snapshots/policy remain the ones just emitted above.
            for event in &mut state.pending {
                if let ExtractionEvent::Media(media) = event {
                    (media.url, media.job) =
                        crate::extract::inline_text("fixture bytes".into()).unwrap();
                }
            }
            let input = stream::iter(state.pending.into_iter().map(Ok)).boxed();
            let mut jobs = jobs::apply_with_descriptor(
                input,
                configurations,
                Some(site.category().into()),
                crate::source_archive::for_url(&source),
            );
            let download = crate::download::client(Duration::from_secs(1)).unwrap();
            let mut files = 0;
            while let Some(job) = jobs.next().await {
                let job = job.unwrap();
                assert_eq!(
                    job.scope.as_ref().unwrap().keywords()["search_tags"],
                    case["trace"][1]["search_tags"]
                );
                crate::download::download_with_options(
                    &download,
                    root.path(),
                    &job.media,
                    crate::download::DownloadOptions {
                        postprocessors: Some(&processors),
                        postprocessor_scope: job.scope.as_ref(),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
                files += 1;
            }
            assert_eq!(files, 1);
            let connection =
                rusqlite::Connection::open(root.path().join("archive.sqlite")).unwrap();
            let entries: Vec<String> = connection
                .prepare("SELECT entry FROM archive ORDER BY entry")
                .unwrap()
                .query_map([], |row| row.get(0))
                .unwrap()
                .collect::<std::result::Result<_, _>>()
                .unwrap();
            assert_eq!(
                entries,
                [format!(
                    "{}_MD_{}",
                    site.category(),
                    case["file_archive_key"].as_str().unwrap()
                )]
            );
            assert!(case["shared_reference"].as_bool().unwrap());
            assert_eq!(
                std::fs::read_to_string(root.path().join("seen.txt")).unwrap(),
                case["trace"][2]["search_tags"].as_str().unwrap(),
                "{}",
                case["family"]
            );
        }
    }

    #[test]
    fn parses_danbooru_post() {
        let media = parse_post(Site::Danbooru, &danbooru_post(123)).unwrap();
        assert_eq!(media.filename, format!("danbooru_123_{MD5}.png"));
        let meta = media.metadata.as_ref().unwrap();
        assert_eq!(meta.post_id, "123");
        assert_eq!(meta.media_id, MD5);
        assert_eq!(meta.username.as_deref(), Some("alice"));
        assert_eq!(meta.caption.as_deref(), Some("1girl solo"));
        assert_eq!(meta.timestamp, Some(1704182645));
        assert_eq!(meta.index, 1);
        let file = meta.file.as_ref().unwrap();
        assert_eq!((file.width, file.height), (Some(800), Some(600)));
        assert!(media.archive_key().unwrap().starts_with("danbooru:v1:"));
    }

    #[test]
    fn parses_e621_wrapped_responses() {
        let single = json!({"post": e621_post(5)}).to_string();
        let raw = posts(Site::E621, single.as_bytes()).unwrap();
        let media = parse_post(Site::E621, &raw[0]).unwrap();
        assert_eq!(media.filename, format!("e621_5_{MD5}.webm"));
        let meta = media.metadata.as_ref().unwrap();
        assert_eq!(meta.caption.as_deref(), Some("a b c"));
        assert_eq!(meta.timestamp, Some(1704182645));
        let list = json!({"posts": [e621_post(5), e621_post(4)]}).to_string();
        assert_eq!(posts(Site::E621, list.as_bytes()).unwrap().len(), 2);
    }

    #[test]
    fn parses_gelbooru_and_safebooru_posts() {
        let body = json!({"@attributes": {"count": 1}, "post": [{"id": 7, "md5": MD5,
            "file_url": format!("https://img3.gelbooru.com/images/01/23/{MD5}.jpeg"),
            "width": 10, "height": 20, "tags": " a  b ", "owner": "bob",
            "created_at": "Tue Jan 02 03:04:05 -0500 2024"}]})
        .to_string();
        let raw = posts(Site::Gelbooru, body.as_bytes()).unwrap();
        let media = parse_post(Site::Gelbooru, &raw[0]).unwrap();
        assert_eq!(media.filename, format!("gelbooru_7_{MD5}.jpg"));
        let meta = media.metadata.as_ref().unwrap();
        assert_eq!(meta.caption.as_deref(), Some("a b"));
        assert_eq!(meta.username.as_deref(), Some("bob"));
        assert_eq!(meta.timestamp, Some(1704182645));
        assert!(
            posts(Site::Gelbooru, b"{\"@attributes\":{}}")
                .unwrap()
                .is_empty()
        );

        let body = json!([{"id": 9, "directory": "1234", "image": "abc.png", "hash": MD5,
                           "width": 1, "height": 2, "tags": "x", "owner": "carol"}])
        .to_string();
        let raw = posts(Site::Safebooru, body.as_bytes()).unwrap();
        let media = parse_post(Site::Safebooru, &raw[0]).unwrap();
        assert_eq!(
            media.url.as_str(),
            "https://safebooru.org/images/1234/abc.png"
        );
        assert_eq!(media.filename, format!("safebooru_9_{MD5}.png"));
        assert!(posts(Site::Safebooru, b"").unwrap().is_empty());
        let evil = json!({"id": 9, "directory": "..", "image": "x.png"});
        assert!(parse_post(Site::Safebooru, &evil).is_none());
    }

    #[test]
    fn skips_missing_or_foreign_files() {
        let mut deleted = danbooru_post(1);
        deleted.as_object_mut().unwrap().remove("file_url");
        let mut foreign = danbooru_post(2);
        foreign["file_url"] = json!("https://evil.example/x.png");
        let mut plain = danbooru_post(3);
        plain["file_url"] = json!("http://cdn.donmai.us/x.png");
        for post in [deleted, foreign, plain] {
            assert!(parse_post(Site::Danbooru, &post).is_none());
        }
        let mut e621 = e621_post(4);
        e621["file"]["url"] = Value::Null;
        assert!(parse_post(Site::E621, &e621).is_none());
        let gel = json!({"id": 5, "file_url": "https://gelbooru.com.evil.test/a.jpg"});
        assert!(parse_post(Site::Gelbooru, &gel).is_none());
    }

    #[test]
    fn classifies_supported_urls() {
        let t = |site, raw: &str| target(site, &Url::parse(raw).unwrap()).map(|(t, _)| t);
        assert_eq!(
            t(Site::Danbooru, "https://danbooru.donmai.us/posts/123?q=x").unwrap(),
            Target::Post { id: 123 }
        );
        assert_eq!(
            t(Site::Danbooru, "https://danbooru.donmai.us/posts?tags=a+b").unwrap(),
            Target::Tags { tags: "a b".into() }
        );
        assert_eq!(
            t(Site::E621, "https://e926.net/pools/42").unwrap(),
            Target::Pool { id: "42".into() }
        );
        let (_, root) =
            target(Site::E621, &Url::parse("https://e926.net/posts/1").unwrap()).unwrap();
        assert_eq!(root.as_str(), "https://e926.net/");
        assert_eq!(
            t(
                Site::Gelbooru,
                "https://gelbooru.com/index.php?page=post&s=view&id=9"
            )
            .unwrap(),
            Target::Post { id: 9 }
        );
        assert_eq!(
            t(
                Site::Safebooru,
                "https://safebooru.org/index.php?page=post&s=list&tags=cat"
            )
            .unwrap(),
            Target::Tags { tags: "cat".into() }
        );
        for (site, raw) in [
            (Site::Danbooru, "https://danbooru.donmai.us/posts"),
            (Site::Danbooru, "https://danbooru.donmai.us/posts/abc"),
            (Site::Danbooru, "https://danbooru.donmai.us:444/posts/1"),
            (Site::Danbooru, "https://safebooru.donmai.us/posts/1"),
            (Site::E621, "https://e621.net/users/1"),
            (
                Site::Gelbooru,
                "https://gelbooru.com/index.php?page=pool&s=show&id=1",
            ),
            (
                Site::Gelbooru,
                "https://gelbooru.com/index.php?page=post&s=view&id=0",
            ),
        ] {
            assert!(t(site, raw).is_err(), "{raw}");
        }
    }

    pub(super) fn response(status: &str, headers: &str, body: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n",
            body.len()
        )
        .into_bytes();
        out.extend_from_slice(body);
        out
    }

    pub(super) fn serve(responses: Vec<Vec<u8>>) -> (Url, JoinHandle<Vec<String>>) {
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

    pub(super) fn booru(site: Site, root: Url) -> Booru {
        Booru::new(site, Duration::from_secs(5))
            .unwrap()
            .with_network(&NetworkOptions::new(Some(String::new()), None).unwrap())
            .unwrap()
            .with_api_root(root)
    }

    #[tokio::test]
    async fn fetches_e621_post_with_retries_and_user_agent() {
        let body = json!({"post": e621_post(5)}).to_string();
        let (root, server) = serve(vec![
            response("429 Too Many Requests", "Retry-After: 0\r\n", b""),
            response("200 OK", "", body.as_bytes()),
        ]);
        let events: Vec<_> = booru(Site::E621, root)
            .with_retries(1)
            .events(&Url::parse("https://e621.net/posts/5").unwrap())
            .collect()
            .await;
        assert_eq!(events.len(), 3, "{events:?}");
        let ExtractionEvent::Post(post) = events[0].as_ref().unwrap() else {
            panic!("missing source post")
        };
        let KeywordPolicy::Shared { file_fields, .. } = &post.keywords else {
            panic!("booru must reuse Directory keywords")
        };
        assert_eq!(file_fields.as_ref(), &["num", "index", "media_id"]);
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /posts/5.json "));
        assert!(
            requests[1]
                .to_ascii_lowercase()
                .contains("user-agent: gallery-dl-rs/")
        );
    }

    #[tokio::test]
    async fn paginates_danbooru_tags_with_id_cursor() {
        let page1: Vec<_> = (0..200).map(|i| danbooru_post(1000 - i)).collect();
        let page1 = Value::Array(page1).to_string();
        let (root, server) = serve(vec![
            response("200 OK", "", page1.as_bytes()),
            response("200 OK", "", b"[]"),
        ]);
        let media: Vec<_> = booru(Site::Danbooru, root)
            .stream(&Url::parse("https://danbooru.donmai.us/posts?tags=cat").unwrap())
            .collect()
            .await;
        assert_eq!(media.len(), 200);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /posts.json?tags=cat&limit=200 "));
        assert!(requests[1].contains("&page=b801 "));
    }

    #[tokio::test]
    async fn search_tags_survive_pages_and_requests_stay_lazy_and_isolated() {
        for site in [Site::Danbooru, Site::E621] {
            let raw = if site == Site::Danbooru {
                danbooru_post(42)
            } else {
                e621_post(42)
            };
            let page = if site == Site::Danbooru {
                json!([raw])
            } else {
                json!({"posts":[raw]})
            };
            let empty = if site == Site::Danbooru {
                "[]"
            } else {
                "{\"posts\":[]}"
            };
            let (root, server) = serve(vec![
                response("200 OK", "", page.to_string().as_bytes()),
                response("200 OK", "", empty.as_bytes()),
            ]);
            let client = booru(site, root).with_threshold(Some(1));
            let url = Url::parse(&format!(
                "https://{}/posts?tags=First&tags=Last",
                if site == Site::Danbooru {
                    "danbooru.donmai.us"
                } else {
                    "e621.net"
                }
            ))
            .unwrap();
            let expected = if site == Site::Danbooru {
                "Last"
            } else {
                "First"
            };
            let mut events = client.events(&url);
            let ExtractionEvent::Post(post) = events.next().await.unwrap().unwrap() else {
                panic!()
            };
            assert_eq!(
                post.metadata.unwrap().search_tags.as_deref(),
                Some(expected)
            );
            let ExtractionEvent::Media(media) = events.next().await.unwrap().unwrap() else {
                panic!()
            };
            assert_eq!(
                media.metadata.as_ref().unwrap().search_tags.as_deref(),
                Some(expected)
            );
            assert!(media.headers.values().all(|v| !v.contains("tags=")));
            assert!(matches!(
                events.next().await.unwrap().unwrap(),
                ExtractionEvent::EndPost
            ));
            assert!(events.next().await.is_none());
            let requests = server.join().unwrap();
            for request in requests {
                assert!(request.contains(&format!("tags={expected}")));
            }
        }
        // DAPI 'all' is only a wire sentinel. Its public source context persists.
        let raw =
            json!({"id":42,"directory":"1","image":format!("{MD5}.jpg"),"hash":MD5,"tags":"cat"});
        let (root, server) = serve(vec![response(
            "200 OK",
            "",
            json!([raw]).to_string().as_bytes(),
        )]);
        let client = booru(Site::Safebooru, root);
        let url = Url::parse("https://safebooru.org/index.php?page=post&s=list&tags=all").unwrap();
        let mut events = client.events(&url);
        let ExtractionEvent::Post(post) = events.next().await.unwrap().unwrap() else {
            panic!()
        };
        assert_eq!(post.metadata.unwrap().search_tags.as_deref(), Some("all"));
        drop(events); // No next-page work or selected file is required.
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].contains("&tags=&limit=100&pid=0 "));
    }

    #[tokio::test]
    async fn fetches_pool_in_order_and_gelbooru_pages() {
        let (root, server) = serve(vec![
            response(
                "200 OK",
                "",
                br#"{"id": 42, "name":"test_pool", "post_ids": [3, 1, 2]}"#,
            ),
            response(
                "200 OK",
                "",
                Value::Array(vec![danbooru_post(1), danbooru_post(2), danbooru_post(3)])
                    .to_string()
                    .as_bytes(),
            ),
        ]);
        let ids: Vec<_> = booru(Site::Danbooru, root)
            .stream(&Url::parse("https://danbooru.donmai.us/pools/42").unwrap())
            .map(|m| m.unwrap().metadata.unwrap().post_id)
            .collect()
            .await;
        assert_eq!(ids, ["1", "2", "3"]);
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /posts.json?tags=ordpool%3A42&limit=200 "));

        let (root, server) = serve(vec![response("200 OK", "", b"[]")]);
        let media: Vec<_> = booru(Site::Safebooru, root)
            .stream(
                &Url::parse("https://safebooru.org/index.php?page=post&s=list&tags=cat").unwrap(),
            )
            .collect()
            .await;
        assert!(media.is_empty());
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with(
            "GET /index.php?page=dapi&s=post&q=index&json=1&tags=cat&limit=100&pid=0 "
        ));
    }

    #[test]
    fn invalid_credentials_are_private_and_builders_can_clear_them() {
        let new = |site| Booru::new(site, Duration::from_secs(5)).unwrap();
        for (username, password) in [
            ("secret:name", "key"),
            ("secret\n", "key"),
            ("user", "secret\0"),
        ] {
            let error = new(Site::Danbooru)
                .with_credentials(Some(username), Some(password))
                .err()
                .unwrap();
            assert_eq!(error.to_string(), "invalid booru username or password");
        }
        let long = "s".repeat(4097);
        assert!(
            new(Site::E621)
                .with_credentials(Some("user"), Some(&long))
                .is_err()
        );
        for (user, key) in [
            ("secret-id", "key"),
            ("0", "key"),
            ("123456789012345678901", "key"),
            ("1", "secret key"),
        ] {
            let error = new(Site::Gelbooru)
                .with_api_credentials(Some(user), Some(key))
                .err()
                .unwrap();
            assert_eq!(error.to_string(), "invalid booru user-id or api-key");
        }
        let client = new(Site::Danbooru)
            .with_credentials(Some("user"), Some("key"))
            .unwrap()
            .with_credentials(None, None)
            .unwrap();
        assert!(client.credentials.is_none());
        let client = new(Site::Gelbooru)
            .with_api_credentials(Some("1"), Some("key"))
            .unwrap()
            .with_api_credentials(Some(""), Some(""))
            .unwrap();
        assert!(client.api_credentials.is_none());
        let client = new(Site::Safebooru)
            .with_credentials(Some("user"), Some("key"))
            .unwrap()
            .with_api_credentials(Some("1"), Some("key"))
            .unwrap();
        assert!(client.credentials.is_none());
        assert!(client.api_credentials.is_none());
    }

    #[test]
    fn descriptive_properties_keep_signed_scores_and_archive_identity() {
        let mut post = danbooru_post(123);
        let before = parse_post(Site::Danbooru, &post).unwrap();
        post["rating"] = json!("q");
        post["score"] = json!(-7);
        let media = parse_post(Site::Danbooru, &post).unwrap();
        let file = media.metadata.as_ref().unwrap().file.as_ref().unwrap();
        assert_eq!(file.filesize, Some(1234));
        assert_eq!(
            media.metadata.as_ref().unwrap().rating.as_deref(),
            Some("q")
        );
        assert_eq!(media.metadata.as_ref().unwrap().score, Some(-7));
        assert_eq!(media.archive_key(), before.archive_key());
        assert_eq!(media.filename, before.filename);
        let events = group(vec![media], "danbooru");
        let ExtractionEvent::Post(post) = &events[0] else {
            panic!("missing post boundary")
        };
        let metadata = post.metadata.as_ref().unwrap();
        assert_eq!(metadata.rating.as_deref(), Some("q"));
        assert_eq!(metadata.score, Some(-7));
        assert!(metadata.file.is_none());

        let mut post = e621_post(5);
        post["score"] = json!({"up": 2, "down": -8, "total": -6});
        post["rating"] = json!("e");
        let media = parse_post(Site::E621, &post).unwrap();
        let file = media.metadata.as_ref().unwrap().file.as_ref().unwrap();
        assert_eq!(file.filesize, Some(9));
        assert_eq!(media.metadata.as_ref().unwrap().score, Some(-6));
        assert_eq!(
            media.metadata.as_ref().unwrap().rating.as_deref(),
            Some("e")
        );

        post["score"]["total"] = json!("0");
        post["file"]["size"] = json!("broken");
        let media = parse_post(Site::E621, &post).unwrap();
        let file = media.metadata.as_ref().unwrap().file.as_ref().unwrap();
        assert_eq!(file.filesize, None);
        assert_eq!(media.metadata.as_ref().unwrap().score, Some(0));
    }

    #[tokio::test]
    async fn basic_auth_is_api_only_and_survives_retries() {
        use base64::Engine;
        for site in [Site::Danbooru, Site::E621] {
            let post = if site == Site::Danbooru {
                danbooru_post(5)
            } else {
                json!({"post": e621_post(5)})
            };
            let (root, server) = serve(vec![
                response("503 Service Unavailable", "Retry-After: 0\r\n", b""),
                response("200 OK", "", post.to_string().as_bytes()),
            ]);
            let client = booru(site, root)
                .with_credentials(Some("secret-user"), Some("secret-key"))
                .unwrap()
                .with_retries(1);
            let input = Url::parse(&format!(
                "https://{}/posts/5",
                if site == Site::Danbooru {
                    "danbooru.donmai.us"
                } else {
                    "e621.net"
                }
            ))
            .unwrap();
            let media = client.stream(&input).next().await.unwrap().unwrap();
            let serialized = serde_json::to_string(&media).unwrap();
            assert!(!serialized.contains("secret-"));
            assert!(media.headers.is_empty());
            assert!(media.handoff.is_empty());
            assert!(!media.url.as_str().contains("secret-"));
            let expected = format!(
                "authorization: Basic {}",
                base64::engine::general_purpose::STANDARD.encode("secret-user:secret-key")
            );
            let requests = server.join().unwrap();
            assert_eq!(requests.len(), 2);
            for request in requests {
                assert!(request.lines().any(|line| line == expected));
                assert!(request.starts_with("GET /posts/5.json HTTP/1.1"));
            }
        }
    }

    #[tokio::test]
    async fn basic_auth_is_reattached_on_same_origin_redirects_only_to_api() {
        use base64::Engine;
        for site in [Site::Danbooru, Site::E621] {
            let post = if site == Site::Danbooru {
                danbooru_post(5)
            } else {
                json!({"post": e621_post(5)})
            };
            let (root, server) = serve(vec![
                response(
                    "307 Temporary Redirect",
                    "Location: /redirected/posts/5.json\r\n",
                    b"",
                ),
                response("200 OK", "", post.to_string().as_bytes()),
            ]);
            let client = booru(site, root)
                .with_credentials(Some("redirect-user"), Some("redirect-key"))
                .unwrap();
            let input = Url::parse(if site == Site::Danbooru {
                "https://danbooru.donmai.us/posts/5"
            } else {
                "https://e621.net/posts/5"
            })
            .unwrap();
            let media = client.stream(&input).next().await.unwrap().unwrap();
            assert!(media.headers.is_empty());
            assert!(media.handoff.is_empty());
            assert!(!media.url.as_str().contains("redirect-"));
            let serialized = serde_json::to_string(&media).unwrap();
            assert!(!serialized.contains("redirect-user"));
            assert!(!serialized.contains("redirect-key"));
            let expected = format!(
                "authorization: Basic {}",
                base64::engine::general_purpose::STANDARD.encode("redirect-user:redirect-key")
            );
            let requests = server.join().unwrap();
            assert_eq!(requests.len(), 2);
            assert!(requests[0].starts_with("GET /posts/5.json HTTP/1.1"));
            assert!(requests[1].starts_with("GET /redirected/posts/5.json HTTP/1.1"));
            for request in requests {
                assert!(request.lines().any(|line| line == expected));
                assert_eq!(request.matches("authorization:").count(), 1);
            }
        }
    }

    #[tokio::test]
    async fn gelbooru_query_credentials_are_encoded_and_not_emitted() {
        let post = json!({"post": [{"id": 1, "md5": MD5, "score": "-5", "rating": "safe",
            "file_url": format!("https://img3.gelbooru.com/images/{MD5}.png")} ]});
        let (root, server) = serve(vec![response("200 OK", "", post.to_string().as_bytes())]);
        let client = booru(Site::Gelbooru, root)
            .with_credentials(Some("unused-user"), Some("unused-password"))
            .unwrap()
            .with_api_credentials(Some("456"), Some("secret-key+&"))
            .unwrap();
        let input = Url::parse("https://gelbooru.com/index.php?page=post&s=view&id=1").unwrap();
        let media = client.stream(&input).next().await.unwrap().unwrap();
        let serialized = serde_json::to_string(&media).unwrap();
        assert!(!serialized.contains("secret-key"));
        assert!(!serialized.contains("api_key"));
        assert!(media.headers.is_empty());
        assert_eq!(media.metadata.unwrap().score, Some(-5));
        let requests = server.join().unwrap();
        assert!(requests[0].contains("&user_id=456&api_key=secret-key%2B%26 "));
        assert!(!requests[0].contains("authorization:"));
        assert!(!requests[0].contains("unused-"));
    }

    #[tokio::test]
    async fn credentials_never_cross_api_origin_redirects_or_error_messages() {
        for site in [Site::Danbooru, Site::Gelbooru] {
            let (root, server) = serve(vec![response(
                "302 Found",
                "Location: https://cdn.donmai.us/stolen\r\n",
                b"",
            )]);
            let client = booru(site, root)
                .with_credentials(Some("secret-user"), Some("secret-password"))
                .unwrap()
                .with_api_credentials(Some("456"), Some("secret-key"))
                .unwrap();
            let input = Url::parse(if site == Site::Danbooru {
                "https://danbooru.donmai.us/posts/1"
            } else {
                "https://gelbooru.com/index.php?page=post&s=view&id=1"
            })
            .unwrap();
            let error = client
                .stream(&input)
                .next()
                .await
                .unwrap()
                .unwrap_err()
                .to_string();
            assert!(error.contains("same origin"), "{error}");
            assert!(!error.contains("secret-"));
            assert_eq!(server.join().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn gelbooru_auth_redirects_have_one_credentials_pair_and_stay_lazy() {
        let post = json!({"post": [{"id": 1, "md5": MD5,
            "file_url": format!("https://img3.gelbooru.com/images/{MD5}.png")} ]});
        let (root, server) = serve(vec![
            response(
                "302 Found",
                "Location: /redirected?tags=cat&api_key=stale-key&user_id=1\r\n",
                b"",
            ),
            response("200 OK", "", post.to_string().as_bytes()),
        ]);
        let client = booru(Site::Gelbooru, root)
            .with_api_credentials(Some("456"), Some("secret-key"))
            .unwrap();
        let input = Url::parse("https://gelbooru.com/index.php?page=post&s=list&tags=cat").unwrap();
        let media = client.stream(&input).take(1).collect::<Vec<_>>().await;
        assert_eq!(media.len(), 1);
        assert!(media[0].is_ok());
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            requests[1].starts_with("GET /redirected?tags=cat&user_id=456&api_key=secret-key ")
        );
        for request in requests {
            assert_eq!(request.matches("api_key=").count(), 1);
            assert_eq!(request.matches("user_id=").count(), 1);
        }
    }

    #[tokio::test]
    async fn terminal_errors_are_not_retried() {
        let (root, server) = serve(vec![response("403 Forbidden", "", b"{}")]);
        let error = booru(Site::Gelbooru, root)
            .with_retries(3)
            .stream(&Url::parse("https://gelbooru.com/index.php?page=post&s=view&id=1").unwrap())
            .collect::<Vec<_>>()
            .await
            .remove(0)
            .unwrap_err();
        assert!(error.to_string().contains("refused"));
        assert!(!error.to_string().contains("127.0.0.1"));
        assert_eq!(server.join().unwrap().len(), 1);
    }
}

#[cfg(test)]
#[path = "booru_pool_tests.rs"]
mod pool_tests;

#[cfg(test)]
#[path = "booru_nameext_compat_tests.rs"]
mod nameext_compat_tests;
