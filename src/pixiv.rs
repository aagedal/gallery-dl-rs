//! Experimental native Pixiv illustration and user-works extraction.
//!
//! Mirrors gallery_dl/extractor/pixiv.py using the iOS App API. Requests are
//! authorized with an access token obtained from the `refresh-token` setting
//! through the OAuth refresh-token grant; the token is cached in memory for the
//! run and refreshed once when the API reports `invalid_grant`. Single-page
//! works yield `meta_single_page.original_image_url`, multi-page works
//! `meta_pages[].image_urls.original`. Ugoira works yield the full-resolution
//! frame ZIP and ordered frame delays from the ugoira metadata endpoint. Files
//! are served from `i.pximg.net`, which requires the App API `Referer` header.
use crate::{
    Result,
    dispatch::Include,
    error,
    event::{ChildMetadata, ExtractionEvent, group},
    extract::{Media, MediaJob, PostMetadata},
    file_metadata::FileMetadata,
    network::NetworkOptions,
    transport::{RateLimits, retry_after},
};
use futures_util::{
    StreamExt,
    stream::{self, BoxStream},
};
use reqwest::{Client, RequestBuilder, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, VecDeque},
    sync::Arc,
    time::{Duration, SystemTime},
};
use url::Url;

#[path = "pixiv_novel.rs"]
pub mod novel;
pub use novel::NovelOptions;
#[path = "pixiv_ranking.rs"]
pub mod ranking;
pub use ranking::{RankingMetadata, RankingOptions};
#[path = "pixiv_search.rs"]
pub mod search;
pub use search::{SearchMetadata, SearchOptions};
#[path = "pixiv_following.rs"]
mod following;

/// Upstream gallery-dl's public iOS app constants.
const CLIENT_ID: &str = "MOBrBDS8blbauoSck0ZfDbtuzpyT";
const CLIENT_SECRET: &str = "lsACyCD94FhDUtGTXi3QzcFE2uU1hqtDaKeqrdwj";
const HASH_SECRET: &str = "28c1fdd170a5204386cb1313c7077b34f83e4aaf4aa829ce78c231e05b0bae2c";
const APP_USER_AGENT: &str = "PixivIOSApp/7.13.3 (iOS 14.6; iPhone13,2)";
const APP_OS_VERSION: &str = "14.6";
pub const REFERER: &str = "https://app-api.pixiv.net/";
const MAX_RESPONSE: usize = 32 * 1024 * 1024;
const MAX_PAGES: usize = 200;
const MAX_UGOIRA_FRAMES: usize = 100_000;
const MAX_UGOIRA_DELAY: u64 = u32::MAX as u64;

/// One ZIP frame and its display duration in milliseconds, in animation order.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct UgoiraFrame {
    pub file: String,
    pub delay: u64,
}

pub fn valid_id(id: &str) -> bool {
    (1..=20).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_digit()) && !id.starts_with('0')
}

/// Media IDs are `ILLUST_pN` for the given illustration ID.
pub fn valid_media_id(post_id: &str, media_id: &str) -> bool {
    valid_id(post_id)
        && media_id
            .strip_prefix(post_id)
            .and_then(|rest| rest.strip_prefix("_p"))
            .is_some_and(|page| {
                (1..=4).contains(&page.len())
                    && page.bytes().all(|b| b.is_ascii_digit())
                    && (page == "0" || !page.starts_with('0'))
            })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Illust,
    Manga,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Illust => "illust",
            Self::Manga => "manga",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProfileKind {
    Avatar,
    Background,
}

impl ProfileKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Avatar => "avatar",
            Self::Background => "background",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Novel(novel::Target),
    Ranking(RankingOptions),
    Search(SearchOptions),
    Following {
        id: Option<String>,
        private: bool,
    },
    Illust(String),
    User {
        id: String,
        kinds: Vec<Kind>,
    },
    Bookmarks {
        id: Option<String>,
        tag: Option<String>,
        private: bool,
    },
    Profile {
        id: String,
        kind: ProfileKind,
    },
}

fn segments(url: &Url) -> Vec<&str> {
    url.path_segments()
        .map(|s| s.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default()
}

/// Decode URL tag segments without changing literal plus characters.
fn percent_decode(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = std::str::from_utf8(bytes.get(index + 1..index + 3)?).ok()?;
            decoded.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

/// The native category of a supported Pixiv artwork or novel URL.
pub fn category(url: &Url) -> Option<&'static str> {
    target(url).ok().map(|target| {
        if matches!(target, Target::Novel(_)) {
            "pixiv-novel"
        } else {
            "pixiv"
        }
    })
}

/// Whether a URL is a Pixiv page handled by this extractor.
pub fn is_pixiv_url(url: &Url) -> bool {
    category(url).is_some()
}

pub fn target(url: &Url) -> Result<Target> {
    let unsupported = || {
        error(
            "unsupported Pixiv URL; expected https://www.pixiv.net/artworks/ID or https://www.pixiv.net/users/ID[/illustrations|/manga|/bookmarks/artworks]",
        )
    };
    if !matches!(url.scheme(), "http" | "https")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || !matches!(url.host_str(), Some("www.pixiv.net" | "pixiv.net"))
    {
        return Err(unsupported());
    }
    if url.path() == "/ranking.php" {
        return RankingOptions::parse(url).map(Target::Ranking);
    }
    if search::is_route(url) {
        return SearchOptions::parse(url).map(Target::Search);
    }
    if novel::is_route(url) {
        return novel::target(url).map(Target::Novel);
    }
    let all = segments(url);
    let rest = match all.as_slice() {
        ["en", rest @ ..] => rest,
        rest => rest,
    };
    let id = |id: &str| valid_id(id).then(|| id.to_owned()).ok_or_else(unsupported);
    let user = |raw: &str, kinds: Vec<Kind>| {
        Ok(Target::User {
            id: id(raw)?,
            kinds,
        })
    };
    let query = |name: &str| {
        url.query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
    };
    let bookmarks = |raw_id: Option<&str>, path_tag: Option<&str>| {
        if query("type").as_deref() == Some("user") {
            return Ok(Target::Following {
                id: raw_id.map(id).transpose()?,
                private: query("rest").as_deref() == Some("hide"),
            });
        }
        if query("type").as_deref() == Some("novel") {
            return Err(unsupported());
        }
        let tag = match query("tag") {
            // Upstream parses the query, then unquotes the tag a second time.
            Some(tag) => Some(percent_decode(&tag).ok_or_else(unsupported)?),
            None => path_tag
                .map(|tag| percent_decode(tag).ok_or_else(unsupported))
                .transpose()?,
        };
        if tag
            .as_ref()
            .is_some_and(|tag| tag.len() > 1024 || tag.chars().any(char::is_control))
        {
            return Err(unsupported());
        }
        Ok(Target::Bookmarks {
            id: raw_id.map(id).transpose()?,
            tag,
            private: query("rest").as_deref() == Some("hide"),
        })
    };
    match rest {
        ["artworks", raw] => Ok(Target::Illust(id(raw)?)),
        ["member_illust.php"] => {
            let raw = url
                .query_pairs()
                .find(|(key, _)| key == "illust_id")
                .map(|(_, value)| value.into_owned())
                .ok_or_else(unsupported)?;
            Ok(Target::Illust(id(&raw)?))
        }
        ["users", raw] | ["users", raw, "artworks"] => user(raw, vec![Kind::Illust, Kind::Manga]),
        ["users", raw, "illustrations"] => user(raw, vec![Kind::Illust]),
        ["users", raw, "manga"] => user(raw, vec![Kind::Manga]),
        ["users", raw, "avatar"] => Ok(Target::Profile {
            id: id(raw)?,
            kind: ProfileKind::Avatar,
        }),
        ["users", raw, "background"] => Ok(Target::Profile {
            id: id(raw)?,
            kind: ProfileKind::Background,
        }),
        ["users", raw, "following"] | ["users", raw, "following", _] => Ok(Target::Following {
            id: Some(id(raw)?),
            private: query("rest").as_deref() == Some("hide"),
        }),
        ["users", raw, "bookmarks", "artworks"] => bookmarks(Some(raw), None),
        ["users", raw, "bookmarks", "artworks", tag] => bookmarks(Some(raw), Some(tag)),
        ["bookmark.php"] => bookmarks(query("id").filter(|id| !id.is_empty()).as_deref(), None),
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

fn id_of(value: &Value) -> Option<String> {
    match value.get("id")? {
        Value::Number(n) => Some(n.to_string()),
        Value::String(s) => Some(s.trim().to_owned()),
        _ => None,
    }
    .filter(|id| valid_id(id))
}

/// HTTPS files on `i.pximg.net` only.
fn media_url(raw: &str) -> Option<Url> {
    let url = Url::parse(raw.trim()).ok()?;
    (url.scheme() == "https"
        && url.host_str() == Some("i.pximg.net")
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none())
    .then_some(url)
}

fn extension(url: &Url) -> String {
    url.path_segments()
        .and_then(|mut s| s.next_back())
        .and_then(|name| name.rsplit_once('.'))
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .filter(|ext| {
            (1..=5).contains(&ext.len()) && ext.bytes().all(|b| b.is_ascii_alphanumeric())
        })
        .map(|ext| if ext == "jpeg" { "jpg".into() } else { ext })
        .unwrap_or_else(|| "jpg".into())
}

/// Why an illustration produced no files.
#[derive(Debug, PartialEq, Eq)]
pub enum Skip {
    Ugoira,
    NoMedia,
}

/// Parse one App API illustration object into its page files.
pub fn parse_illust(illust: &Value) -> std::result::Result<Vec<Media>, Skip> {
    let id = id_of(illust).ok_or(Skip::NoMedia)?;
    if string(illust, "type") == Some("ugoira") {
        return Err(Skip::Ugoira);
    }
    let mut urls = Vec::new();
    let pages = illust
        .get("meta_pages")
        .and_then(Value::as_array)
        .filter(|pages| !pages.is_empty());
    match pages {
        Some(pages) => {
            for page in pages {
                let url = page
                    .get("image_urls")
                    .and_then(|urls| string(urls, "original"))
                    .and_then(media_url)
                    .ok_or(Skip::NoMedia)?;
                urls.push(url);
            }
        }
        None => urls.push(
            illust
                .get("meta_single_page")
                .and_then(|page| string(page, "original_image_url"))
                .and_then(media_url)
                .ok_or(Skip::NoMedia)?,
        ),
    }
    Ok(parse_files(illust, &id, urls))
}

fn illustration_metadata(illust: &Value, id: &str) -> PostMetadata {
    let username = illust
        .get("user")
        .and_then(|user| string(user, "account"))
        .map(Arc::<str>::from);
    let title = string(illust, "title");
    let caption = string(illust, "caption")
        .map(crate::mastodon::strip_html)
        .filter(|text| !text.trim().is_empty());
    let caption = match (title, caption) {
        (Some(title), Some(caption)) => Some(format!("{title}\n\n{}", caption.trim())),
        (Some(title), None) => Some(title.to_owned()),
        (None, caption) => caption.map(|c| c.trim().to_owned()),
    }
    .map(Arc::<str>::from);
    let timestamp = string(illust, "create_date").and_then(|d| crate::date::parse_date(d).ok());
    PostMetadata {
        post_id: id.to_owned(),
        username,
        caption,
        timestamp,
        ..PostMetadata::default()
    }
}

fn parse_files(illust: &Value, id: &str, urls: Vec<Url>) -> Vec<Media> {
    let base = illustration_metadata(illust, id);
    let dimension = |key| illust.get(key).and_then(Value::as_u64).filter(|v| *v > 0);
    let headers = BTreeMap::from([("Referer".to_owned(), REFERER.to_owned())]);
    urls.into_iter()
        .enumerate()
        .map(|(page, url)| {
            let ext = extension(&url);
            let (width, height) = if page == 0 {
                (dimension("width"), dimension("height"))
            } else {
                (None, None)
            };
            Media {
                filename: format!("{id}_p{page}.{ext}"),
                job: MediaJob::DirectHttp,
                category: "pixiv".into(),
                metadata: Some(PostMetadata {
                    file: Some(FileMetadata {
                        width,
                        height,
                        ..FileMetadata::default()
                    }),
                    media_id: format!("{id}_p{page}"),
                    index: page + 1,
                    ..base.clone()
                }),
                url,
                headers: headers.clone(),
                handoff: BTreeMap::new(),
            }
        })
        .collect()
}

/// The shared allowlist for native and compatibility ugoira frame metadata.
pub(crate) fn parse_frames(value: &Value) -> Option<Arc<[UgoiraFrame]>> {
    let raw_frames = value
        .as_array()
        .filter(|frames| !frames.is_empty() && frames.len() <= MAX_UGOIRA_FRAMES)?;
    let frames = raw_frames
        .iter()
        .map(|frame| {
            let file = frame.get("file").and_then(Value::as_str).filter(|file| {
                !file.is_empty()
                    && file.len() <= 255
                    && !matches!(*file, "." | "..")
                    && !file
                        .chars()
                        .any(|c| c.is_control() || matches!(c, '/' | '\\'))
            })?;
            let delay = frame
                .get("delay")
                .and_then(Value::as_u64)
                .filter(|delay| *delay <= MAX_UGOIRA_DELAY)?;
            Some(UgoiraFrame {
                file: file.to_owned(),
                delay,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    Some(frames.into())
}

/// Parse App API animation metadata without downloading or unpacking the ZIP.
pub fn parse_ugoira(illust: &Value, bytes: &[u8]) -> Result<Vec<Media>> {
    let invalid = || error("Pixiv returned invalid ugoira metadata");
    let id = id_of(illust).ok_or_else(invalid)?;
    let body = json(bytes)?;
    let data = body.get("ugoira_metadata").ok_or_else(invalid)?;
    let raw = data
        .get("zip_urls")
        .and_then(|urls| string(urls, "medium"))
        .ok_or_else(invalid)?;
    let url = media_url(&raw.replacen("_ugoira600x600", "_ugoira1920x1080", 1))
        .filter(|url| extension(url) == "zip")
        .ok_or_else(invalid)?;
    let frames = data
        .get("frames")
        .and_then(parse_frames)
        .ok_or_else(invalid)?;
    let mut media = parse_files(illust, &id, vec![url]);
    media[0].metadata.as_mut().expect("Pixiv metadata").frames = Some(frames);
    Ok(media)
}

/// Shared native/compatibility archive identity for Pixiv user profile images.
pub(crate) fn profile_asset_identity(
    kind: &str,
    owner: &str,
    timestamp: Option<i64>,
) -> Option<crate::profile_asset::ProfileAssetMetadata> {
    use crate::profile_asset::{ProfileAssetKind, ProfileAssetMetadata};
    if !valid_id(owner) {
        return None;
    }
    let kind = match kind {
        "avatar" => ProfileAssetKind::Avatar,
        "background" => ProfileAssetKind::Background,
        _ => return None,
    };
    Some(ProfileAssetMetadata {
        kind,
        owner_id: owner.to_owned(),
        version: timestamp
            .map(|date| date.to_string())
            .unwrap_or_else(|| "none".into()),
    })
}

/// Profile images use the upstream kind/user/date identity and URL-derived UTC
/// timestamp, independent of signed query parameters or download fallbacks.
pub fn parse_profile(bytes: &[u8], owner: &str, kind: ProfileKind) -> Result<Vec<Media>> {
    let body = json(bytes)?;
    let user = body
        .get("user")
        .ok_or_else(|| error("Pixiv response contains no user"))?;
    if !valid_id(owner) || id_of(user).as_deref() != Some(owner) {
        return Err(error("Pixiv returned an unexpected user"));
    }
    let raw = match kind {
        ProfileKind::Avatar => user
            .get("profile_image_urls")
            .and_then(|urls| string(urls, "medium"))
            .ok_or_else(|| error("Pixiv user contains no avatar"))?
            .replace("_170.", "."),
        ProfileKind::Background => {
            let profile = body
                .get("profile")
                .ok_or_else(|| error("Pixiv response contains no profile"))?;
            let raw = match profile.get("background_image_url") {
                Some(Value::Null) => return Ok(Vec::new()),
                Some(Value::String(raw)) if raw.is_empty() => return Ok(Vec::new()),
                Some(Value::String(raw)) => raw.as_str(),
                _ => return Err(error("Pixiv profile contains no background URL")),
            };
            let mut parts: Vec<_> = raw.split('/').collect();
            if raw.contains("/c/") && parts.len() >= 5 {
                parts.drain(3..5);
            }
            parts.join("/").replace("_master1200.", ".")
        }
    };
    let url = Url::parse(&raw)
        .ok()
        .filter(|url| {
            url.scheme() == "https"
                && url.port().is_none()
                && url.username().is_empty()
                && url.password().is_none()
                && (url.host_str() == Some("i.pximg.net")
                    || url.host_str() == Some("s.pximg.net")
                        && url.path().starts_with("/common/images/"))
        })
        .ok_or_else(|| error("Pixiv returned an unsupported profile image URL"))?;
    let parts: Vec<_> = url.path_segments().into_iter().flatten().collect();
    let timestamp = if parts.len() >= 8 {
        crate::date::parse_date(&format!(
            "{}-{}-{}T{}:{}:{}+09:00",
            parts[2], parts[3], parts[4], parts[5], parts[6], parts[7]
        ))
        .ok()
    } else {
        None
    };
    let version = timestamp
        .map(|date| date.to_string())
        .unwrap_or_else(|| "none".into());
    let date_suffix = timestamp
        .and_then(|timestamp| time::OffsetDateTime::from_unix_timestamp(timestamp).ok())
        .map(|date| {
            format!(
                "_{:04}-{:02}-{:02}",
                date.year(),
                u8::from(date.month()),
                date.day()
            )
        })
        .unwrap_or_default();
    let label = kind.as_str();
    let ext = extension(&url);
    let mut handoff = BTreeMap::new();
    if kind == ProfileKind::Background && url.as_str().ends_with(".jpg") {
        let mut fallback = url.clone();
        let base = fallback
            .path()
            .strip_suffix(".jpg")
            .expect("jpg suffix")
            .to_owned();
        let candidates: Vec<_> = ["png", "gif"]
            .into_iter()
            .map(|ext| {
                fallback.set_path(&format!("{base}.{ext}"));
                fallback.to_string()
            })
            .collect();
        handoff.insert("_fallback".into(), serde_json::json!(candidates));
    }
    Ok(vec![Media {
        url,
        job: MediaJob::DirectHttp,
        category: "pixiv".into(),
        filename: format!("{label}{date_suffix}.{ext}"),
        metadata: Some(PostMetadata {
            post_id: label.into(),
            media_id: format!("{label}_{owner}_{version}"),
            username: string(user, "account").map(Arc::from),
            caption: Some(Arc::from(label)),
            timestamp,
            index: 1,
            profile_asset: profile_asset_identity(label, owner, timestamp),
            ..PostMetadata::default()
        }),
        headers: BTreeMap::from([("Referer".into(), REFERER.into())]),
        handoff,
    }])
}

fn json(bytes: &[u8]) -> Result<Value> {
    serde_json::from_slice(bytes).map_err(|_| error("Pixiv returned invalid JSON"))
}

/// Parse an `illust/detail` response.
pub fn parse_detail(bytes: &[u8]) -> Result<Vec<Media>> {
    let body = json(bytes)?;
    let illust = body
        .get("illust")
        .ok_or_else(|| error("Pixiv response contains no illustration"))?;
    parse_illust(illust).map_err(|skip| match skip {
        Skip::Ugoira => error("Pixiv ugoira illustration requires animation metadata"),
        Skip::NoMedia => error("Pixiv illustration contains no supported media"),
    })
}

/// One user-works page: per-illustration media and the raw `next_url`.
pub struct Page {
    pub works: Vec<Value>,
    pub next: Option<String>,
}

pub fn parse_page(bytes: &[u8]) -> Result<Page> {
    let body = json(bytes)?;
    let entries = body
        .get("illusts")
        .and_then(Value::as_array)
        .ok_or_else(|| error("Pixiv response contains no illustration list"))?;
    Ok(Page {
        // Keep raw works so animation metadata is fetched only as each work
        // is consumed, never eagerly for an entire listing page.
        works: entries.clone(),
        next: string(&body, "next_url").map(str::to_owned),
    })
}

/// Lowercase hex MD5 (RFC 1321), used only for upstream's `X-Client-Hash`.
pub fn md5_hex(input: &[u8]) -> String {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    let k: Vec<u32> = (0..64)
        .map(|i| ((i as f64 + 1.0).sin().abs() * 4294967296.0) as u32)
        .collect();
    let mut state: [u32; 4] = [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476];
    let mut message = input.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&((input.len() as u64).wrapping_mul(8)).to_le_bytes());
    for chunk in message.chunks(64) {
        let m: Vec<u32> = chunk
            .chunks(4)
            .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
            .collect();
        let [mut a, mut b, mut c, mut d] = state;
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let f = f.wrapping_add(a).wrapping_add(k[i]).wrapping_add(m[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f.rotate_left(S[i]));
        }
        for (word, value) in state.iter_mut().zip([a, b, c, d]) {
            *word = word.wrapping_add(value);
        }
    }
    state
        .iter()
        .flat_map(|word| word.to_le_bytes())
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Upstream's `X-Client-Time` format: UTC `%Y-%m-%dT%H:%M:%S+00:00`.
fn client_time(now: SystemTime) -> String {
    let seconds = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let time = time::OffsetDateTime::from_unix_timestamp(seconds)
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}+00:00",
        time.year(),
        u8::from(time.month()),
        time.day(),
        time.hour(),
        time.minute(),
        time.second()
    )
}

pub struct Pixiv {
    client: Client,
    network: NetworkOptions,
    api_root: Url,
    oauth_root: Url,
    refresh_token: Option<String>,
    access_token: tokio::sync::Mutex<Option<String>>,
    authenticated_user: tokio::sync::Mutex<Option<String>>,
    timeout: Duration,
    request_timeout: Duration,
    retries: u8,
    rate_limits: RateLimits,
    ugoira: bool,
    include: Option<Include>,
    novel_options: NovelOptions,
    novel_extractor: Option<Arc<Pixiv>>,
}

#[derive(Clone)]
enum Feed {
    Works(Kind),
    Bookmarks { tag: Option<String>, private: bool },
}

impl Feed {
    fn endpoint(&self) -> &'static str {
        match self {
            Self::Works(_) => "v1/user/illusts",
            Self::Bookmarks { .. } => "v1/user/bookmarks/illust",
        }
    }
}

struct ListingState {
    kinds: VecDeque<Feed>,
    active: Option<Feed>,
    user: Option<String>,
    next: Option<Url>,
    pages: usize,
    pending: VecDeque<ExtractionEvent>,
    works: VecDeque<Value>,
}

impl Pixiv {
    pub fn new(timeout: Duration) -> Result<Self> {
        Ok(Self {
            client: NetworkOptions::default().client(APP_USER_AGENT, timeout)?,
            network: NetworkOptions::default(),
            api_root: Url::parse("https://app-api.pixiv.net/").expect("static URL"),
            oauth_root: Url::parse("https://oauth.secure.pixiv.net/").expect("static URL"),
            refresh_token: None,
            access_token: tokio::sync::Mutex::new(None),
            authenticated_user: tokio::sync::Mutex::new(None),
            timeout,
            request_timeout: timeout,
            retries: 0,
            rate_limits: RateLimits::default(),
            ugoira: true,
            include: None,
            novel_options: NovelOptions::default(),
            novel_extractor: None,
        })
    }

    /// Apply proxy and user-agent settings to API requests and emitted media.
    pub fn with_network(mut self, network: &NetworkOptions) -> Result<Self> {
        self.client = network.client(APP_USER_AGENT, self.timeout)?;
        self.network = network.clone();
        Ok(self)
    }

    pub fn with_refresh_token(mut self, token: Option<&str>) -> Self {
        self.refresh_token = token.map(str::to_owned);
        self
    }

    pub fn with_novel_options(mut self, options: NovelOptions) -> Self {
        self.novel_options = options;
        self
    }

    /// Apply independently configured pixiv-novel credentials/options to queued novel views.
    pub fn with_novel_extractor(mut self, native: Arc<Pixiv>) -> Self {
        self.novel_extractor = Some(native);
        self
    }

    /// Select bare-profile child views; explicit views remain root jobs.
    pub fn with_include(mut self, include: Option<Include>) -> Self {
        self.include = include;
        self
    }

    /// Download animation frame ZIPs (upstream defaults to enabled).
    pub fn with_ugoira(mut self, enabled: bool) -> Self {
        self.ugoira = enabled;
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

    /// Override the App API origin; intended for local tests.
    pub fn with_api_root(mut self, root: Url) -> Self {
        self.api_root = root;
        self
    }

    /// Override the OAuth origin; intended for local tests.
    pub fn with_oauth_root(mut self, root: Url) -> Self {
        self.oauth_root = root;
        self
    }

    fn app_headers(&self, request: RequestBuilder) -> RequestBuilder {
        let request = request
            .header("App-OS", "ios")
            .header("App-OS-Version", APP_OS_VERSION)
            .timeout(self.request_timeout);
        if self.network.user_agent().is_some() {
            request
        } else {
            request.header("User-Agent", APP_USER_AGENT)
        }
    }

    async fn authenticate(&self) -> Result<String> {
        let token = self.refresh_token.as_deref().ok_or_else(missing_token)?;
        let endpoint = self.oauth_root.join("auth/token").expect("static path");
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("client_id", CLIENT_ID)
            .append_pair("client_secret", CLIENT_SECRET)
            .append_pair("grant_type", "refresh_token")
            .append_pair("refresh_token", token)
            .append_pair("get_secure_url", "1")
            .append_pair("include_policy", "true")
            .finish();
        let response = self
            .request(&endpoint, |time, hash| {
                self.app_headers(self.client.post(endpoint.clone()))
                    .header("X-Client-Time", time)
                    .header("X-Client-Hash", hash)
                    .header("Content-Type", "application/x-www-form-urlencoded")
                    .body(body.clone())
            })
            .await?;
        match response {
            Reply::Ok(body) => {
                let data = json(&body)?;
                // Current OAuth replies wrap the login result in `response`;
                // accept the historical top-level token format as well.
                let data = data.get("response").unwrap_or(&data);
                let access = data
                    .get("access_token")
                    .and_then(Value::as_str)
                    .filter(|token| {
                        !token.is_empty() && token.bytes().all(|b| b.is_ascii_graphic())
                    })
                    .ok_or_else(|| error("Pixiv OAuth response contains no access token"))?;
                *self.authenticated_user.lock().await = data.get("user").and_then(id_of);
                Ok(access.to_owned())
            }
            Reply::Status(status, _) if status.is_client_error() => Err(error(
                "Pixiv rejected the refresh-token setting; obtain a new refresh token",
            )),
            Reply::Status(status, _) => Err(error(format!("Pixiv OAuth returned HTTP {status}"))),
        }
    }

    async fn access_token(&self, stale: Option<&str>) -> Result<String> {
        let mut cached = self.access_token.lock().await;
        if let Some(token) = cached.as_deref()
            && Some(token) != stale
        {
            return Ok(token.to_owned());
        }
        let token = self.authenticate().await?;
        *cached = Some(token.clone());
        Ok(token)
    }

    /// Authorized App API GET, refreshing the access token once on `invalid_grant`.
    async fn get(&self, endpoint: &Url) -> Result<Vec<u8>> {
        tokio::time::timeout(self.timeout, async {
            let mut token = self.access_token(None).await?;
            for refreshed in [false, true] {
                let reply = self
                    .request(endpoint, |_, _| {
                        self.app_headers(self.client.get(endpoint.clone()))
                            .header("Referer", REFERER)
                            .bearer_auth(&token)
                    })
                    .await?;
                match reply {
                    Reply::Ok(body) => return Ok(body),
                    Reply::Status(status, body)
                        if matches!(status.as_u16(), 400 | 401)
                            && !refreshed
                            && String::from_utf8_lossy(&body).contains("invalid_grant") =>
                    {
                        token = self.access_token(Some(&token)).await?;
                    }
                    Reply::Status(status, _) => return Err(status_error(status)),
                }
            }
            Err(error("Pixiv refused the refreshed access token"))
        })
        .await
        .map_err(|_| error("native Pixiv extraction timed out"))?
    }

    async fn work(&self, illust: &Value, listing: bool) -> Result<Vec<Media>> {
        match parse_illust(illust) {
            Ok(media) => Ok(media),
            Err(Skip::Ugoira) if !self.ugoira => Ok(Vec::new()),
            Err(Skip::Ugoira) => {
                let id = id_of(illust).ok_or_else(|| error("Pixiv illustration has no ID"))?;
                let mut endpoint = self
                    .api_root
                    .join("v1/ugoira/metadata")
                    .expect("static path");
                endpoint.query_pairs_mut().append_pair("illust_id", &id);
                parse_ugoira(illust, &self.get(&endpoint).await?)
            }
            Err(Skip::NoMedia) if listing => Ok(Vec::new()),
            Err(Skip::NoMedia) => Err(error("Pixiv illustration contains no supported media")),
        }
    }

    fn finish(&self, mut media: Vec<Media>) -> VecDeque<ExtractionEvent> {
        for item in &mut media {
            self.network.apply_media(item);
        }
        group(media, "pixiv")
    }

    fn first_page(&self, user: &str, feed: &Feed) -> Url {
        let mut url = self.api_root.join(feed.endpoint()).expect("static path");
        url.query_pairs_mut().append_pair("user_id", user);
        match feed {
            Feed::Works(kind) => {
                url.query_pairs_mut().append_pair("type", kind.as_str());
            }
            Feed::Bookmarks { tag, private } => {
                url.query_pairs_mut()
                    .append_pair("restrict", if *private { "private" } else { "public" });
                if let Some(tag) = tag {
                    url.query_pairs_mut().append_pair("tag", tag);
                }
            }
        }
        url
    }

    /// Keep pagination on the authenticated API origin and the selected feed.
    fn next_url(&self, raw: &str, feed: &Feed, user: &str) -> Result<Url> {
        let url = Url::parse(raw).map_err(|_| error("Pixiv returned an invalid next page"))?;
        let expected = self.first_page(user, feed);
        let query: Vec<_> = url.query_pairs().collect();
        let valid_scope = expected.query_pairs().all(|(key, value)| {
            let mut values = query.iter().filter(|(name, _)| *name == key);
            values.next().is_some_and(|(_, actual)| *actual == value) && values.next().is_none()
        });
        if url.origin() != self.api_root.origin()
            || url.path() != expected.path()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || !valid_scope
            || matches!(feed, Feed::Bookmarks { tag: None, .. })
                && query.iter().any(|(key, _)| key == "tag")
        {
            return Err(error("Pixiv returned an unexpected next page"));
        }
        Ok(url)
    }

    async fn next_page(&self, state: &mut ListingState) -> Result<()> {
        if state.user.is_none() {
            tokio::time::timeout(self.timeout, self.access_token(None))
                .await
                .map_err(|_| error("native Pixiv extraction timed out"))??;
            state.user = self.authenticated_user.lock().await.clone();
            if state.user.is_none() {
                return Err(error("Pixiv OAuth response contains no current user ID"));
            }
        }
        let user = state.user.as_deref().expect("resolved listing user");
        let endpoint = match state.next.take() {
            Some(url) => url,
            None => {
                let Some(feed) = state.kinds.pop_front() else {
                    return Ok(());
                };
                state.pages = 0;
                let endpoint = self.first_page(user, &feed);
                state.active = Some(feed);
                endpoint
            }
        };
        let page = parse_page(&self.get(&endpoint).await?)?;
        state.pages += 1;
        if state.pages < MAX_PAGES
            && let Some(next) = page.next
        {
            state.next =
                Some(self.next_url(&next, state.active.as_ref().expect("active feed"), user)?);
        }
        state.works.extend(page.works);
        Ok(())
    }

    pub fn stream(&self, url: &Url) -> BoxStream<'_, Result<Media>> {
        crate::event::media_only(self.events(url))
    }

    /// Bare user profiles dispatch the default artworks child; explicit views
    /// iterate their works directly, without a child boundary per illustration.
    pub fn events(&self, url: &Url) -> BoxStream<'_, Result<ExtractionEvent>> {
        let target = match target(url) {
            Ok(target) => target,
            Err(e) => return stream::once(async move { Err(e) }).boxed(),
        };
        let path = segments(url);
        let bare_user = matches!(path.as_slice(), ["users", _] | ["en", "users", _]);
        if bare_user {
            let Target::User { id, .. } = target else {
                unreachable!("validated bare user")
            };
            let views = crate::dispatch::select(
                self.include.as_ref(),
                &["artworks"],
                &[
                    "avatar",
                    "background",
                    "artworks",
                    "favorite",
                    "bookmark",
                    "user",
                    "sketch",
                ],
                &[("novel-bookmark", "bookmark"), ("novel-user", "user")],
            );
            return stream::iter(views)
                .flat_map(move |view| {
                    let id = id.clone();
                    crate::event::child_job(
                        ChildMetadata::from_raw("pixiv", "user", &serde_json::Map::new()),
                        move || self.view_events(id, &view),
                    )
                })
                .boxed();
        }
        self.target_events(target)
    }

    fn view_events(&self, id: String, view: &str) -> BoxStream<'_, Result<ExtractionEvent>> {
        let target = match view {
            "artworks" => Target::User { id, kinds: vec![Kind::Illust, Kind::Manga] },
            "favorite" => Target::Bookmarks { id: Some(id), tag: None, private: false },
            "avatar" => Target::Profile { id, kind: ProfileKind::Avatar },
            "background" => Target::Profile { id, kind: ProfileKind::Background },
            "bookmark" | "user" => {
                let target = if view == "bookmark" { novel::Target::Bookmarks { user: id, tag: None, private: false } }
                    else { novel::Target::User(id) };
                let native = self.novel_extractor.as_deref().unwrap_or(self);
                return native.novel_events(target);
            }
            "sketch" => return stream::once(async {
                Err(error("native Pixiv Sketch extraction is not supported; use --extractor compatibility"))
            }).boxed(),
            _ => return stream::empty().boxed(),
        };
        self.target_events(target)
    }

    fn target_events(&self, target: Target) -> BoxStream<'_, Result<ExtractionEvent>> {
        if let Target::Novel(target) = target {
            return self.novel_events(target);
        }
        if self.refresh_token.is_none() {
            return stream::once(async { Err(missing_token()) }).boxed();
        }
        let (user, kinds) = match target {
            Target::Novel(target) => return self.novel_events(target),
            Target::Following { id, private } => return self.following_events(id, private),
            Target::Ranking(options) => return self.ranking_events(options),
            Target::Search(options) => return self.search_events(options),
            Target::Profile { id, kind } => {
                return stream::once(async move {
                    let mut endpoint = self.api_root.join("v1/user/detail").expect("static");
                    endpoint.query_pairs_mut().append_pair("user_id", &id);
                    let media = parse_profile(&self.get(&endpoint).await?, &id, kind)?;
                    let events = if media.is_empty() {
                        VecDeque::new()
                    } else {
                        self.finish(media)
                    };
                    Ok::<_, crate::Error>(stream::iter(events.into_iter().map(Ok)))
                })
                .map(|result| match result {
                    Ok(events) => events.boxed(),
                    Err(e) => stream::once(async move { Err(e) }).boxed(),
                })
                .flatten()
                .boxed();
            }
            Target::Illust(id) => {
                return stream::once(async move {
                    let mut endpoint = self.api_root.join("v1/illust/detail").expect("static");
                    endpoint.query_pairs_mut().append_pair("illust_id", &id);
                    let body = json(&self.get(&endpoint).await?)?;
                    let illust = body
                        .get("illust")
                        .ok_or_else(|| error("Pixiv response contains no illustration"))?;
                    if id_of(illust).as_deref() != Some(id.as_str()) {
                        return Err(error("Pixiv returned an unexpected illustration"));
                    }
                    let media = self.work(illust, false).await?;
                    Ok::<_, crate::Error>(stream::iter(self.finish(media).into_iter().map(Ok)))
                })
                .map(|result| match result {
                    Ok(events) => events.boxed(),
                    Err(e) => stream::once(async move { Err(e) }).boxed(),
                })
                .flatten()
                .boxed();
            }
            Target::User { id, kinds } => (
                Some(id),
                kinds.into_iter().map(Feed::Works).collect::<Vec<_>>(),
            ),
            Target::Bookmarks { id, tag, private } => (id, vec![Feed::Bookmarks { tag, private }]),
        };
        let state = ListingState {
            kinds: kinds.into(),
            active: None,
            user,
            next: None,
            pages: 0,
            pending: VecDeque::new(),
            works: VecDeque::new(),
        };
        stream::unfold(Some(state), move |state| async move {
            let mut state = state?;
            loop {
                if let Some(event) = state.pending.pop_front() {
                    return Some((Ok(event), Some(state)));
                }
                if let Some(work) = state.works.pop_front() {
                    match self.work(&work, true).await {
                        Ok(media) => state.pending.extend(self.finish(media)),
                        Err(e) => return Some((Err(e), Some(state))),
                    }
                    continue;
                }
                if state.next.is_none() && state.kinds.is_empty() {
                    return None;
                }
                if let Err(e) = self.next_page(&mut state).await {
                    return Some((Err(e), None));
                }
            }
        })
        .boxed()
    }

    /// Send with retries; `build` receives a fresh client time and hash.
    async fn request(
        &self,
        endpoint: &Url,
        build: impl Fn(&str, &str) -> RequestBuilder,
    ) -> Result<Reply> {
        for attempt in 0..=self.retries {
            let time = client_time(SystemTime::now());
            let hash = md5_hex(format!("{time}{HASH_SECRET}").as_bytes());
            match self.attempt(endpoint, build(&time, &hash), attempt).await {
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
            .map_err(|_| Failure::transient("Pixiv request failed; check network access"))?;
        let status = response.status();
        if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
            let delay = retry_after(response.headers(), SystemTime::now());
            self.rate_limits
                .defer(endpoint, delay.unwrap_or_else(|| backoff(attempt)));
            return Err(Failure::transient(
                if status == StatusCode::TOO_MANY_REQUESTS {
                    match delay {
                        Some(delay) => {
                            format!("Pixiv rate limit; retry after {} seconds", delay.as_secs())
                        }
                        None => "Pixiv rate limit; retry later".into(),
                    }
                } else {
                    format!("Pixiv returned HTTP {status}")
                },
            ));
        }
        if status.is_redirection() {
            return Err(error("Pixiv API responded with an unexpected redirect").into());
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| Failure::transient("could not read Pixiv response"))?
        {
            if body.len() + chunk.len() > MAX_RESPONSE {
                return Err(error("Pixiv response exceeded 32 MiB").into());
            }
            body.extend_from_slice(&chunk);
        }
        Ok(if status.is_success() {
            Reply::Ok(body)
        } else {
            Reply::Status(status, body)
        })
    }
}

enum Reply {
    Ok(Vec<u8>),
    Status(StatusCode, Vec<u8>),
}

fn missing_token() -> crate::Error {
    error(
        "native Pixiv extraction requires the pixiv refresh-token setting in --native-config; or use --extractor compatibility",
    )
}

fn status_error(status: StatusCode) -> crate::Error {
    match status.as_u16() {
        404 | 410 => error("Pixiv illustration or user not found"),
        400 | 401 | 403 => error(format!(
            "Pixiv refused access (HTTP {status}); the work may be restricted or the refresh-token invalid"
        )),
        _ => error(format!("Pixiv returned HTTP {status}")),
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

    fn illust(id: u64, pages: usize) -> Value {
        let page = |n: usize| {
            json!({"image_urls": {"original":
                format!("https://i.pximg.net/img-original/img/2024/01/01/00/00/00/{id}_p{n}.png")}})
        };
        json!({"id": id, "title": "Title", "type": "illust",
            "caption": "Hello <b>world</b>", "create_date": "2024-01-01T09:00:00+09:00",
            "width": 1000, "height": 2000, "user": {"id": 5, "account": "alice"},
            "meta_single_page": if pages == 1 { json!({"original_image_url":
                format!("https://i.pximg.net/img-original/img/2024/01/01/00/00/00/{id}_p0.jpg")}) }
                else { json!({}) },
            "meta_pages": (0..if pages == 1 { 0 } else { pages }).map(page).collect::<Vec<_>>()})
    }

    #[test]
    fn parses_single_and_multi_page_works() {
        let single = parse_illust(&illust(42, 1)).unwrap();
        assert_eq!(single.len(), 1);
        assert_eq!(single[0].filename, "42_p0.jpg");
        assert_eq!(single[0].headers["Referer"], REFERER);
        let meta = single[0].metadata.as_ref().unwrap();
        assert_eq!(meta.post_id, "42");
        assert_eq!(meta.media_id, "42_p0");
        assert_eq!(meta.username.as_deref(), Some("alice"));
        assert_eq!(meta.caption.as_deref(), Some("Title\n\nHello world"));
        assert_eq!(meta.timestamp, Some(1704067200));
        assert_eq!(meta.index, 1);
        let file = meta.file.as_ref().unwrap();
        assert_eq!((file.width, file.height), (Some(1000), Some(2000)));
        let multi = parse_illust(&illust(43, 3)).unwrap();
        let names: Vec<_> = multi.iter().map(|m| m.filename.as_str()).collect();
        assert_eq!(names, ["43_p0.png", "43_p1.png", "43_p2.png"]);
        let second = multi[1].metadata.as_ref().unwrap();
        assert_eq!(
            (second.index, second.file.as_ref().unwrap().width),
            (2, None)
        );
        let key = multi[0].archive_key().unwrap();
        assert!(key.starts_with("pixiv:"), "{key}");
        assert_ne!(Some(key), multi[1].archive_key());
        let mut ugoira = illust(44, 1);
        ugoira["type"] = json!("ugoira");
        assert_eq!(parse_illust(&ugoira).unwrap_err(), Skip::Ugoira);
        let mut offsite = illust(45, 1);
        offsite["meta_single_page"]["original_image_url"] = json!("https://evil.example/x.jpg");
        assert_eq!(parse_illust(&offsite).unwrap_err(), Skip::NoMedia);
        let detail = json!({"illust": ugoira}).to_string();
        let message = parse_detail(detail.as_bytes()).unwrap_err().to_string();
        assert!(message.contains("ugoira"), "{message}");
        assert!(valid_media_id("42", "42_p0") && valid_media_id("42", "42_p12"));
        assert!(!valid_media_id("42", "43_p0") && !valid_media_id("42", "42_p01"));
    }

    fn animation(id: u64) -> Value {
        let mut work = illust(id, 1);
        work["type"] = json!("ugoira");
        work
    }

    fn animation_metadata() -> Value {
        json!({"ugoira_metadata": {
            "zip_urls": {"medium": "https://i.pximg.net/img-zip-ugoira/img/2024/01/01/00/00/00/42_ugoira600x600.zip"},
            "frames": [{"file": "000002.jpg", "delay": 120},
                       {"file": "000000.jpg", "delay": 0},
                       {"file": "000001.jpg", "delay": 80}]
        }})
    }

    #[test]
    fn ugoira_uses_full_resolution_zip_and_preserves_frame_order_and_delays() {
        let media =
            parse_ugoira(&animation(42), animation_metadata().to_string().as_bytes()).unwrap();
        assert_eq!(media.len(), 1);
        let item = &media[0];
        assert!(item.url.as_str().ends_with("42_ugoira1920x1080.zip"));
        assert_eq!(item.filename, "42_p0.zip");
        assert_eq!(
            item.headers,
            BTreeMap::from([("Referer".into(), REFERER.into())])
        );
        assert!(matches!(item.job, MediaJob::DirectHttp));
        let metadata = item.metadata.as_ref().unwrap();
        assert_eq!(metadata.media_id, "42_p0");
        assert!(valid_media_id(&metadata.post_id, &metadata.media_id));
        let frames = metadata.frames.as_ref().unwrap();
        assert_eq!(
            frames
                .iter()
                .map(|f| (f.file.as_str(), f.delay))
                .collect::<Vec<_>>(),
            [("000002.jpg", 120), ("000000.jpg", 0), ("000001.jpg", 80)]
        );
        // Inspection and JSON sidecars serialize the same metadata object.
        assert_eq!(
            serde_json::to_value(metadata).unwrap()["frames"],
            animation_metadata()["ugoira_metadata"]["frames"]
        );
        assert_eq!(metadata.username.as_deref(), Some("alice"));
        assert_eq!(metadata.file.as_ref().unwrap().width, Some(1000));
        let mut already_full = animation_metadata();
        already_full["ugoira_metadata"]["zip_urls"]["medium"] =
            json!("https://i.pximg.net/img-zip-ugoira/42_ugoira1920x1080.zip");
        assert!(parse_ugoira(&animation(42), already_full.to_string().as_bytes()).is_ok());
    }

    #[test]
    fn rejects_malformed_or_unsafe_ugoira_metadata() {
        for value in [
            json!(null),
            json!([]),
            json!({}),
            json!({"ugoira_metadata": {}}),
        ] {
            assert!(parse_ugoira(&animation(42), value.to_string().as_bytes()).is_err());
        }
        for raw in [
            "https://evil.example/42.zip",
            "https://i.pximg.net:444/x.zip",
            "http://i.pximg.net/x.zip",
            "https://a:b@i.pximg.net/x.zip",
            "https://i.pximg.net/x.jpg",
        ] {
            let mut data = animation_metadata();
            data["ugoira_metadata"]["zip_urls"]["medium"] = json!(raw);
            assert!(
                parse_ugoira(&animation(42), data.to_string().as_bytes()).is_err(),
                "{raw}"
            );
        }
        for frames in [
            json!([]),
            json!([{"file": "../x.jpg", "delay": 10}]),
            json!([{"file": "a\\b.jpg", "delay": 10}]),
            json!([{"file": "", "delay": 10}]),
            json!([{"file": "..", "delay": 10}]),
            json!([{"file": "a\u{0}.jpg", "delay": 10}]),
            json!([{"file": "a".repeat(256), "delay": 10}]),
            json!([{"file": "x.jpg", "delay": -1}]),
            json!([{"file": "x.jpg", "delay": 1.5}]),
            json!([{"file": "x.jpg", "delay": MAX_UGOIRA_DELAY + 1}]),
            json!([{"file": "x.jpg"}]),
            json!([{"delay": 10}]),
            json!(vec![
                json!({"file": "x.jpg", "delay": 10});
                MAX_UGOIRA_FRAMES + 1
            ]),
        ] {
            let mut data = animation_metadata();
            data["ugoira_metadata"]["frames"] = frames;
            let error = parse_ugoira(&animation(42), data.to_string().as_bytes())
                .unwrap_err()
                .to_string();
            assert_eq!(error, "Pixiv returned invalid ugoira metadata");
        }
    }

    #[test]
    fn classifies_supported_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        let work = Target::Illust("123".into());
        assert_eq!(t("https://www.pixiv.net/artworks/123").unwrap(), work);
        assert_eq!(t("https://www.pixiv.net/en/artworks/123").unwrap(), work);
        assert_eq!(
            t("https://www.pixiv.net/member_illust.php?mode=medium&illust_id=123").unwrap(),
            work
        );
        let user = |kinds| Target::User {
            id: "9".into(),
            kinds,
        };
        assert_eq!(
            t("https://www.pixiv.net/users/9").unwrap(),
            user(vec![Kind::Illust, Kind::Manga])
        );
        assert_eq!(
            t("https://pixiv.net/en/users/9").unwrap(),
            user(vec![Kind::Illust, Kind::Manga])
        );
        assert_eq!(
            t("https://www.pixiv.net/users/9/illustrations").unwrap(),
            user(vec![Kind::Illust])
        );
        assert_eq!(
            t("https://www.pixiv.net/users/9/manga").unwrap(),
            user(vec![Kind::Manga])
        );
        for raw in [
            "https://www.pixiv.net/",
            "https://www.pixiv.net/artworks/abc",
            "https://www.pixiv.net/artworks/0123",
            "https://www.pixiv.net/users/9/bookmarks/novel-unknown",
            "https://www.pixiv.net/member_illust.php?id=9",
            "https://i.pximg.net/img-original/img/1_p0.png",
            "https://www.pixiv.net:444/artworks/1",
            "https://www.pixiv.net.evil.example/artworks/1",
        ] {
            assert!(t(raw).is_err(), "{raw}");
        }
    }

    #[test]
    fn classifies_public_bookmarks_and_upstream_tag_precedence() {
        let bookmarks = |id: Option<&str>, tag: Option<&str>| Target::Bookmarks {
            id: id.map(str::to_owned),
            tag: tag.map(str::to_owned),
            private: false,
        };
        for raw in [
            "https://www.pixiv.net/users/9/bookmarks/artworks",
            "https://pixiv.net/en/users/9/bookmarks/artworks/",
            "https://www.pixiv.net/bookmark.php?id=9",
        ] {
            assert_eq!(
                target(&Url::parse(raw).unwrap()).unwrap(),
                bookmarks(Some("9"), None)
            );
        }
        for raw in [
            "https://www.pixiv.net/bookmark.php",
            "https://www.pixiv.net/bookmark.php?id=",
        ] {
            assert_eq!(
                target(&Url::parse(raw).unwrap()).unwrap(),
                bookmarks(None, None)
            );
        }
        for (suffix, tag) in [
            ("/cat%20ears", "cat ears"),
            ("/C++", "C++"),
            ("/%E7%8C%AB", "猫"),
            ("/old?tag=new+tag", "new tag"),
            ("/old?tag=%25E7%258C%25AB", "猫"),
            ("/old?tag=first&tag=second", "first"),
            ("/old?tag=", ""),
        ] {
            let raw = format!("https://www.pixiv.net/users/9/bookmarks/artworks{suffix}");
            assert_eq!(
                target(&Url::parse(&raw).unwrap()).unwrap(),
                bookmarks(Some("9"), Some(tag))
            );
        }
        for raw in [
            "https://www.pixiv.net/bookmark.php?id=bad&type=user",
            "https://www.pixiv.net/bookmark.php?type=novel",
            "https://www.pixiv.net/users/9/bookmarks/novel-unknown",
            "https://www.pixiv.net/users/9/following/extra/path",
            "https://www.pixiv.net/users/9/bookmarks/artworks/%00",
            "https://www.pixiv.net/users/9/bookmarks/artworks/%FF",
            "https://www.pixiv.net/users/09/bookmarks/artworks",
            "https://u:p@www.pixiv.net/users/9/bookmarks/artworks",
        ] {
            assert!(target(&Url::parse(raw).unwrap()).is_err(), "{raw}");
        }
        let overlong = format!(
            "https://www.pixiv.net/users/9/bookmarks/artworks/{}",
            "a".repeat(1025)
        );
        assert!(target(&Url::parse(&overlong).unwrap()).is_err());
    }

    #[test]
    fn private_bookmark_routes_follow_upstream_first_rest_value_semantics() {
        for raw in [
            "https://www.pixiv.net/users/9/bookmarks/artworks?rest=hide",
            "https://www.pixiv.net/en/users/9/bookmarks/artworks/cat?tag=override&rest=hide&rest=show",
            "https://www.pixiv.net/bookmark.php?id=9&rest=hide",
            "https://www.pixiv.net/bookmark.php?rest=hide",
        ] {
            assert!(
                matches!(
                    target(&Url::parse(raw).unwrap()).unwrap(),
                    Target::Bookmarks { private: true, .. }
                ),
                "{raw}"
            );
        }
        for suffix in [
            "",
            "?rest=show",
            "?rest=show&rest=hide",
            "?rest=unknown&rest=hide",
        ] {
            let raw = format!("https://www.pixiv.net/users/9/bookmarks/artworks{suffix}");
            assert!(matches!(
                target(&Url::parse(&raw).unwrap()).unwrap(),
                Target::Bookmarks { private: false, .. }
            ));
        }
        assert!(matches!(
            target(&Url::parse("https://www.pixiv.net/bookmark.php?rest=hide").unwrap()).unwrap(),
            Target::Bookmarks {
                id: None,
                private: true,
                ..
            }
        ));
    }

    #[test]
    fn public_and_private_bookmark_next_urls_cannot_change_or_duplicate_visibility() {
        let native = Pixiv::new(Duration::from_secs(2)).unwrap();
        for private in [false, true] {
            let feed = Feed::Bookmarks {
                tag: Some("cat".into()),
                private,
            };
            let mut valid = native.first_page("9", &feed);
            valid
                .query_pairs_mut()
                .append_pair("max_bookmark_id", "123");
            assert!(native.next_url(valid.as_str(), &feed, "9").is_ok());
            let other = if private { "public" } else { "private" };
            let changed = format!(
                "https://app-api.pixiv.net/v1/user/bookmarks/illust?user_id=9&restrict={other}&tag=cat&max_bookmark_id=123"
            );
            assert_eq!(
                native
                    .next_url(&changed, &feed, "9")
                    .unwrap_err()
                    .to_string(),
                "Pixiv returned an unexpected next page"
            );
            for restriction in ["public", "private"] {
                let mut duplicate = valid.clone();
                duplicate
                    .query_pairs_mut()
                    .append_pair("restrict", restriction);
                assert!(native.next_url(duplicate.as_str(), &feed, "9").is_err());
            }
        }
    }

    #[test]
    fn bookmark_pagination_cannot_change_origin_endpoint_user_visibility_or_tag() {
        let native = Pixiv::new(Duration::from_secs(2)).unwrap();
        let feed = Feed::Bookmarks {
            tag: Some("猫".into()),
            private: false,
        };
        let mut next = native.first_page("9", &feed);
        next.query_pairs_mut().append_pair("max_bookmark_id", "123");
        assert!(native.next_url(next.as_str(), &feed, "9").is_ok());
        for raw in [
            "https://evil.example/v1/user/bookmarks/illust?user_id=9&restrict=public&tag=%E7%8C%AB",
            "https://app-api.pixiv.net/v1/user/illusts?user_id=9&restrict=public&tag=%E7%8C%AB",
            "https://app-api.pixiv.net/v1/user/bookmarks/novel?user_id=9&restrict=public&tag=%E7%8C%AB",
            "https://app-api.pixiv.net/v1/user/bookmarks/illust?user_id=10&restrict=public&tag=%E7%8C%AB",
            "https://app-api.pixiv.net/v1/user/bookmarks/illust?user_id=9&restrict=private&tag=%E7%8C%AB",
            "https://app-api.pixiv.net/v1/user/bookmarks/illust?user_id=9&restrict=public&tag=dog",
            "https://app-api.pixiv.net/v1/user/bookmarks/illust?user_id=9&restrict=public",
            "https://app-api.pixiv.net/v1/user/bookmarks/illust?user_id=9&user_id=10&restrict=public&tag=%E7%8C%AB",
            "https://u:p@app-api.pixiv.net/v1/user/bookmarks/illust?user_id=9&restrict=public&tag=%E7%8C%AB",
        ] {
            assert_eq!(
                native.next_url(raw, &feed, "9").unwrap_err().to_string(),
                "Pixiv returned an unexpected next page",
                "{raw}"
            );
        }
        assert!(
            native
                .next_url(
                    next.as_str(),
                    &Feed::Bookmarks {
                        tag: None,
                        private: false
                    },
                    "9"
                )
                .is_err()
        );
    }

    fn user_detail() -> Value {
        json!({"user": {"id": 9, "account": "alice", "profile_image_urls": {
            "medium": "https://i.pximg.net/user-profile/img/2024/01/01/00/00/00/9_170.jpg?token=private-avatar"}},
            "profile": {"background_image_url": "https://i.pximg.net/c/1200x600_90_a2/background/img/2024/01/01/00/00/00/99_master1200.jpg?token=private-background"}})
    }

    #[test]
    fn parses_upstream_avatar_and_background_urls_dates_fallbacks_and_archive_versions() {
        use crate::profile_asset::ProfileAssetKind;
        let body = user_detail().to_string();
        let avatar = parse_profile(body.as_bytes(), "9", ProfileKind::Avatar)
            .unwrap()
            .remove(0);
        assert_eq!(
            avatar.url.path(),
            "/user-profile/img/2024/01/01/00/00/00/9.jpg"
        );
        assert_eq!(avatar.filename, "avatar_2023-12-31.jpg");
        let metadata = avatar.metadata.as_ref().unwrap();
        assert_eq!(metadata.post_id, "avatar");
        assert_eq!(metadata.username.as_deref(), Some("alice"));
        assert_eq!(metadata.caption.as_deref(), Some("avatar"));
        assert_eq!(metadata.timestamp, Some(1704034800));
        let asset = metadata.profile_asset.as_ref().unwrap();
        assert_eq!(asset.kind, ProfileAssetKind::Avatar);
        assert_eq!(asset.owner_id, "9");
        assert_eq!(asset.version, "1704034800");
        assert!(avatar.archive_key().is_some());
        assert!(!valid_media_id(&metadata.post_id, &metadata.media_id));
        let mut refreshed = avatar.clone();
        refreshed.url.set_query(Some("token=another-secret"));
        assert_eq!(avatar.archive_key(), refreshed.archive_key());
        let mut changed = avatar.clone();
        changed
            .metadata
            .as_mut()
            .unwrap()
            .profile_asset
            .as_mut()
            .unwrap()
            .version = "1704034801".into();
        assert_ne!(avatar.archive_key(), changed.archive_key());
        let background = parse_profile(body.as_bytes(), "9", ProfileKind::Background)
            .unwrap()
            .remove(0);
        assert_eq!(
            background.url.path(),
            "/background/img/2024/01/01/00/00/00/99.jpg"
        );
        assert_eq!(background.filename, "background_2023-12-31.jpg");
        assert_eq!(
            background
                .metadata
                .as_ref()
                .unwrap()
                .profile_asset
                .as_ref()
                .unwrap()
                .kind,
            ProfileAssetKind::Background
        );
        assert_ne!(avatar.archive_key(), background.archive_key());
        assert!(background.handoff.is_empty());
        // Upstream adds fallbacks only when the complete URL ends in .jpg.
        let mut unsigned = user_detail();
        unsigned["profile"]["background_image_url"] = json!(
            "https://i.pximg.net/c/1200x600_90_a2/background/img/2024/01/01/00/00/00/99_master1200.jpg"
        );
        let fallback = parse_profile(
            unsigned.to_string().as_bytes(),
            "9",
            ProfileKind::Background,
        )
        .unwrap()
        .remove(0);
        assert_eq!(fallback.archive_key(), background.archive_key());
        assert_eq!(
            fallback.handoff["_fallback"],
            json!([
                "https://i.pximg.net/background/img/2024/01/01/00/00/00/99.png",
                "https://i.pximg.net/background/img/2024/01/01/00/00/00/99.gif"
            ])
        );
        for media in [&avatar, &background] {
            assert_eq!(
                media.headers,
                BTreeMap::from([("Referer".into(), REFERER.into())])
            );
            assert_ne!(
                media.archive_key(),
                parse_illust(&illust(9, 1)).unwrap()[0].archive_key()
            );
        }
        for kind in [ProfileKind::Avatar, ProfileKind::Background] {
            assert!(
                parse_profile(body.as_bytes(), "10", kind)
                    .unwrap_err()
                    .to_string()
                    .contains("unexpected user")
            );
        }
    }

    #[test]
    fn default_avatar_and_empty_background_preserve_upstream_absent_date_behavior() {
        let mut detail = user_detail();
        detail["user"]["profile_image_urls"]["medium"] =
            json!("https://s.pximg.net/common/images/no_profile.png");
        detail["profile"]["background_image_url"] = Value::Null;
        let media = parse_profile(detail.to_string().as_bytes(), "9", ProfileKind::Avatar)
            .unwrap()
            .remove(0);
        assert_eq!(media.filename, "avatar.png");
        assert_eq!(media.metadata.as_ref().unwrap().timestamp, None);
        assert_eq!(
            media
                .metadata
                .as_ref()
                .unwrap()
                .profile_asset
                .as_ref()
                .unwrap()
                .version,
            "none"
        );
        assert!(media.archive_key().is_some());
        assert!(
            parse_profile(detail.to_string().as_bytes(), "9", ProfileKind::Background)
                .unwrap()
                .is_empty()
        );
        for raw in [
            "https://evil.example/9.jpg",
            "http://i.pximg.net/9.jpg",
            "https://x:y@i.pximg.net/9.jpg",
            "https://i.pximg.net:444/9.jpg",
            "https://s.pximg.net/private/9.jpg",
        ] {
            detail["user"]["profile_image_urls"]["medium"] = json!(raw);
            let error = parse_profile(detail.to_string().as_bytes(), "9", ProfileKind::Avatar)
                .unwrap_err()
                .to_string();
            assert_eq!(error, "Pixiv returned an unsupported profile image URL");
        }
    }

    #[test]
    fn md5_and_client_time_match_reference_values() {
        assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(
            md5_hex(b"The quick brown fox jumps over the lazy dog"),
            "9e107d9d372bb6826bd81d3542a419d6"
        );
        assert_eq!(md5_hex(&[b'a'; 64]), "014842d480b571495a4a0363793f7367");
        let time = SystemTime::UNIX_EPOCH + Duration::from_secs(1704067200);
        assert_eq!(client_time(time), "2024-01-01T00:00:00+00:00");
    }

    pub(super) fn response(status: &str, body: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\nRetry-After: 0\r\n\r\n",
            body.len()
        )
        .into_bytes();
        out.extend_from_slice(body);
        out
    }

    pub(super) fn serve(responses: Vec<Vec<u8>>) -> (Url, JoinHandle<Vec<String>>) {
        serve_with(|_| responses)
    }

    pub(super) fn serve_with(
        responses: impl FnOnce(&Url) -> Vec<Vec<u8>>,
    ) -> (Url, JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let root = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let responses = responses(&root);
        listener.set_nonblocking(true).unwrap();
        let task = thread::spawn(move || {
            let mut requests = Vec::new();
            for reply in responses {
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                let (mut stream, _) = loop {
                    match listener.accept() {
                        Ok(connection) => break connection,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(
                                std::time::Instant::now() < deadline,
                                "missing Pixiv request"
                            );
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(e) => panic!("Pixiv test accept failed: {e}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 4096];
                loop {
                    if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                        let length = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length: "))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if request.len() >= end + 4 + length {
                            break;
                        }
                    }
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

    pub(super) fn pixiv(root: Url) -> Pixiv {
        Pixiv::new(Duration::from_secs(5))
            .unwrap()
            .with_network(&NetworkOptions::new(Some(String::new()), None).unwrap())
            .unwrap()
            .with_api_root(root.clone())
            .with_oauth_root(root)
            .with_refresh_token(Some("refresh-secret"))
    }

    pub(super) fn token(value: &str) -> Vec<u8> {
        response(
            "200 OK",
            json!({"access_token": value, "expires_in": 3600})
                .to_string()
                .as_bytes(),
        )
    }

    #[tokio::test]
    async fn fetches_detail_with_oauth_and_refreshes_once() {
        let detail = json!({"illust": illust(42, 2)}).to_string();
        let expired =
            json!({"error": {"message": "Error occurred at the OAuth process. invalid_grant"}})
                .to_string();
        let (root, server) = serve(vec![
            token("first"),
            response("400 Bad Request", expired.as_bytes()),
            token("second"),
            response("503 Service Unavailable", b""),
            response("200 OK", detail.as_bytes()),
        ]);
        let events: Vec<_> = pixiv(root)
            .with_retries(1)
            .events(&Url::parse("https://www.pixiv.net/en/artworks/42").unwrap())
            .collect()
            .await;
        assert_eq!(events.len(), 4, "{events:?}");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("POST /auth/token "));
        let lower = requests[0].to_ascii_lowercase();
        assert!(lower.contains("app-os: ios") && lower.contains("x-client-hash: "));
        assert!(lower.contains("user-agent: pixiviosapp/7.13.3"));
        assert!(requests[0].contains(
            "client_id=MOBrBDS8blbauoSck0ZfDbtuzpyT&client_secret=lsACyCD94FhDUtGTXi3QzcFE2uU1hqtDaKeqrdwj&grant_type=refresh_token&refresh_token=refresh-secret&get_secure_url=1&include_policy=true"
        ));
        assert!(requests[1].starts_with("GET /v1/illust/detail?illust_id=42 "));
        assert!(
            requests[1]
                .to_ascii_lowercase()
                .contains("authorization: bearer first")
        );
        assert!(
            requests[1]
                .to_ascii_lowercase()
                .contains("referer: https://app-api.pixiv.net/")
        );
        assert!(
            requests[4]
                .to_ascii_lowercase()
                .contains("authorization: bearer second")
        );
    }

    #[tokio::test]
    async fn paginates_user_works_by_type() {
        let page = |ids: &[u64], next: Option<String>| {
            response(
                "200 OK",
                json!({"illusts": ids.iter().map(|id| illust(*id, 1)).collect::<Vec<_>>(),
                    "next_url": next})
                .to_string()
                .as_bytes(),
            )
        };
        let (root, server) = serve_with(|root| {
            let next = root.join("v1/user/illusts?user_id=9&type=illust&offset=30");
            vec![
                token("t"),
                page(&[1], Some(next.unwrap().to_string())),
                page(&[2], None),
                page(&[3], Some("https://evil.example/v1/user/illusts".into())),
            ]
        });
        let results: Vec<_> = pixiv(root)
            .stream(&Url::parse("https://www.pixiv.net/users/9").unwrap())
            .collect()
            .await;
        assert_eq!(results.len(), 3, "{results:?}");
        let error = results[2].as_ref().unwrap_err().to_string();
        assert!(error.contains("unexpected next page") && !error.contains("evil"));
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /v1/user/illusts?user_id=9&type=illust "));
        assert!(requests[2].starts_with("GET /v1/user/illusts?user_id=9&type=illust&offset=30 "));
        assert!(requests[3].starts_with("GET /v1/user/illusts?user_id=9&type=manga "));
    }

    #[tokio::test]
    async fn fetches_authenticated_ugoira_metadata_without_leaking_tokens_to_zip() {
        let detail = json!({"illust": animation(42)}).to_string();
        let (root, server) = serve(vec![
            token("access-secret"),
            response("200 OK", detail.as_bytes()),
            response("200 OK", animation_metadata().to_string().as_bytes()),
        ]);
        let items = pixiv(root)
            .stream(&Url::parse("https://www.pixiv.net/artworks/42").unwrap())
            .collect::<Vec<_>>()
            .await;
        assert_eq!(items.len(), 1);
        let item = items[0].as_ref().unwrap();
        assert_eq!(item.filename, "42_p0.zip");
        assert_eq!(
            item.metadata
                .as_ref()
                .unwrap()
                .frames
                .as_ref()
                .unwrap()
                .len(),
            3
        );
        assert_eq!(
            item.headers,
            BTreeMap::from([("Referer".into(), REFERER.into())])
        );
        let requests = server.join().unwrap();
        assert!(requests[2].starts_with("GET /v1/ugoira/metadata?illust_id=42 "));
        assert!(
            requests[2]
                .to_ascii_lowercase()
                .contains("authorization: bearer access-secret")
        );
        assert!(!format!("{item:?}").contains("secret"));
    }

    #[tokio::test]
    async fn rejects_unexpected_detail_identity_before_fetching_animation_metadata() {
        for returned_id in [json!(43), json!("43"), json!(null), json!("invalid")] {
            let mut work = animation(42);
            work["id"] = returned_id;
            let detail = json!({"illust": work});
            let (root, server) = serve(vec![
                token("access-secret"),
                response("200 OK", detail.to_string().as_bytes()),
            ]);
            let result = pixiv(root)
                .stream(&Url::parse("https://www.pixiv.net/artworks/42").unwrap())
                .collect::<Vec<_>>()
                .await;
            assert_eq!(result.len(), 1);
            assert_eq!(
                result[0].as_ref().unwrap_err().to_string(),
                "Pixiv returned an unexpected illustration"
            );
            // A metadata request would fail against the closed local listener;
            // the identity-specific error proves it was rejected beforehand.
            assert_eq!(server.join().unwrap().len(), 2);
        }
    }

    #[tokio::test]
    async fn disabling_ugoira_skips_metadata_requests_for_details_and_listings() {
        for listing in [false, true] {
            let body = if listing {
                json!({"illusts": [animation(42), illust(43, 1)], "next_url": null})
            } else {
                json!({"illust": animation(42)})
            };
            let (root, server) = serve(vec![
                token("t"),
                response("200 OK", body.to_string().as_bytes()),
            ]);
            let url = if listing {
                "https://www.pixiv.net/users/9/illustrations"
            } else {
                "https://www.pixiv.net/artworks/42"
            };
            let items = pixiv(root)
                .with_ugoira(false)
                .stream(&Url::parse(url).unwrap())
                .collect::<Vec<_>>()
                .await;
            assert_eq!(items.len(), usize::from(listing), "{items:?}");
            if listing {
                assert_eq!(items[0].as_ref().unwrap().filename, "43_p0.jpg");
            }
            assert_eq!(server.join().unwrap().len(), 2);
        }
    }

    #[tokio::test]
    async fn listings_fetch_each_animation_only_when_consumed() {
        for first_animation in [false, true] {
            let first = if first_animation {
                animation(42)
            } else {
                illust(42, 1)
            };
            let body = json!({"illusts": [first, animation(43)], "next_url": null});
            let mut replies = vec![token("t"), response("200 OK", body.to_string().as_bytes())];
            if first_animation {
                replies.push(response(
                    "200 OK",
                    animation_metadata().to_string().as_bytes(),
                ));
            }
            let (root, server) = serve(replies);
            let native = pixiv(root);
            let mut items =
                native.stream(&Url::parse("https://www.pixiv.net/users/9/illustrations").unwrap());
            let first = items.next().await.unwrap().unwrap();
            assert_eq!(first.metadata.as_ref().unwrap().post_id, "42");
            assert_eq!(
                first.metadata.as_ref().unwrap().frames.is_some(),
                first_animation
            );
            drop(items);
            assert_eq!(
                server.join().unwrap().len(),
                if first_animation { 3 } else { 2 }
            );
        }
    }

    #[tokio::test]
    async fn malformed_animation_does_not_discard_later_listing_works() {
        let body = json!({"illusts": [animation(42), illust(43, 1)], "next_url": null});
        let (root, server) = serve(vec![
            token("t"),
            response("200 OK", body.to_string().as_bytes()),
            response("200 OK", b"{\"ugoira_metadata\":{}}"),
        ]);
        let items = pixiv(root)
            .stream(&Url::parse("https://www.pixiv.net/users/9/illustrations").unwrap())
            .collect::<Vec<_>>()
            .await;
        assert_eq!(items.len(), 2);
        assert!(
            items[0]
                .as_ref()
                .unwrap_err()
                .to_string()
                .contains("ugoira metadata")
        );
        assert_eq!(items[1].as_ref().unwrap().filename, "43_p0.jpg");
        assert_eq!(server.join().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn cancelling_ugoira_metadata_closes_unfinished_response() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let root = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let (started, waiting) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let replies = [
                token("t"),
                response(
                    "200 OK",
                    json!({"illust": animation(42)}).to_string().as_bytes(),
                ),
            ];
            for reply in replies {
                let (mut connection, _) = listener.accept().await.unwrap();
                let mut request = vec![0; 8192];
                let count = connection.read(&mut request).await.unwrap();
                assert_ne!(count, 0, "expected Pixiv API request");
                connection.write_all(&reply).await.unwrap();
            }
            let (mut connection, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 8192];
            let count = connection.read(&mut request).await.unwrap();
            assert!(
                String::from_utf8_lossy(&request[..count])
                    .starts_with("GET /v1/ugoira/metadata?illust_id=42 ")
            );
            connection
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n{")
                .await
                .unwrap();
            started.send(()).unwrap();
            let closed =
                tokio::time::timeout(Duration::from_secs(2), connection.read(&mut request))
                    .await
                    .expect("cancelled metadata connection remained open");
            match closed {
                Ok(0) => {}
                // Cancelling before the incomplete response has been consumed
                // may close TCP with a reset instead of an orderly EOF.
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
                other => panic!("expected cancelled metadata connection to close: {other:?}"),
            }
        });
        let extraction = tokio::spawn(async move {
            pixiv(root)
                .stream(&Url::parse("https://www.pixiv.net/artworks/42").unwrap())
                .collect::<Vec<_>>()
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), waiting)
            .await
            .unwrap()
            .unwrap();
        extraction.abort();
        assert!(extraction.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(3), server)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn paginates_public_bookmarks_and_preserves_original_work_metadata_and_archive_ids() {
        let page = |works: Vec<Value>, next: Option<Url>| {
            response(
                "200 OK",
                json!({"illusts": works, "next_url": next})
                    .to_string()
                    .as_bytes(),
            )
        };
        let (root, server) = serve_with(|root| {
            let next = root.join("v1/user/bookmarks/illust?user_id=9&restrict=public&tag=%E7%8C%AB&max_bookmark_id=100").unwrap();
            vec![
                token("bookmark-access"),
                page(vec![illust(41, 2)], Some(next)),
                page(vec![animation(42)], None),
                response("200 OK", animation_metadata().to_string().as_bytes()),
            ]
        });
        let native = pixiv(root);
        let items = native
            .stream(
                &Url::parse("https://www.pixiv.net/users/9/bookmarks/artworks/%E7%8C%AB").unwrap(),
            )
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            items
                .iter()
                .map(|item| item.filename.as_str())
                .collect::<Vec<_>>(),
            ["41_p0.png", "41_p1.png", "42_p0.zip"]
        );
        let ordinary = parse_illust(&illust(41, 2)).unwrap();
        assert_eq!(items[0].archive_key(), ordinary[0].archive_key());
        assert_eq!(items[1].archive_key(), ordinary[1].archive_key());
        assert_eq!(
            items[0].metadata.as_ref().unwrap().username.as_deref(),
            Some("alice")
        );
        assert_eq!(
            items[2]
                .metadata
                .as_ref()
                .unwrap()
                .frames
                .as_ref()
                .unwrap()
                .len(),
            3
        );
        for item in &items {
            assert_eq!(
                item.headers,
                BTreeMap::from([("Referer".into(), REFERER.into())])
            );
            assert!(!format!("{item:?}").contains("bookmark-access"));
        }
        let requests = server.join().unwrap();
        assert!(
            requests[1].starts_with(
                "GET /v1/user/bookmarks/illust?user_id=9&restrict=public&tag=%E7%8C%AB "
            )
        );
        assert!(requests[2].starts_with("GET /v1/user/bookmarks/illust?user_id=9&restrict=public&tag=%E7%8C%AB&max_bookmark_id=100 "));
        assert!(requests[3].starts_with("GET /v1/ugoira/metadata?illust_id=42 "));
        for request in &requests[1..] {
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer bookmark-access")
            );
        }
    }

    #[tokio::test]
    async fn own_bookmarks_resolve_oauth_user_from_upstream_response_envelope() {
        for nested in [false, true] {
            let login = json!({"access_token": "current-user-access", "user": {"id": "17"}});
            let login = if nested {
                json!({"response": login})
            } else {
                login
            };
            let (root, server) = serve(vec![
                response("200 OK", login.to_string().as_bytes()),
                response(
                    "200 OK",
                    json!({"illusts": [illust(42, 1)], "next_url": null})
                        .to_string()
                        .as_bytes(),
                ),
            ]);
            let items = pixiv(root)
                .stream(&Url::parse("https://www.pixiv.net/bookmark.php?tag=cat+ears").unwrap())
                .collect::<Vec<_>>()
                .await;
            assert_eq!(items.len(), 1);
            assert_eq!(items[0].as_ref().unwrap().filename, "42_p0.jpg");
            let requests = server.join().unwrap();
            assert!(requests[1].starts_with(
                "GET /v1/user/bookmarks/illust?user_id=17&restrict=public&tag=cat+ears "
            ));
            assert!(
                requests[1]
                    .to_ascii_lowercase()
                    .contains("authorization: bearer current-user-access")
            );
        }
    }

    #[tokio::test]
    async fn private_modern_legacy_and_own_bookmarks_paginate_with_original_artist_archive_ids() {
        for (raw, user) in [
            (
                "https://www.pixiv.net/users/9/bookmarks/artworks/cat?rest=hide",
                "9",
            ),
            (
                "https://www.pixiv.net/bookmark.php?id=9&rest=hide&tag=cat",
                "9",
            ),
            ("https://www.pixiv.net/bookmark.php?rest=hide&tag=cat", "17"),
        ] {
            let mut second = illust(42, 1);
            second["user"]["account"] = json!("second-artist");
            let (root, server) = serve_with(|root| {
                vec![
                response("200 OK", json!({"response":{"access_token":"private-access","user":{"id":17}}}).to_string().as_bytes()),
                response("200 OK", json!({"illusts":[illust(41,1)], "next_url":root.join(&format!("v1/user/bookmarks/illust?user_id={user}&restrict=private&tag=cat&max_bookmark_id=100")).unwrap()}).to_string().as_bytes()),
                response("200 OK", json!({"illusts":[second.clone()], "next_url":null}).to_string().as_bytes()),
            ]
            });
            let files = pixiv(root)
                .stream(&Url::parse(raw).unwrap())
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .collect::<Result<Vec<_>>>()
                .unwrap();
            assert_eq!(files.len(), 2);
            for (media, original) in files.iter().zip([illust(41, 1), second]) {
                let original = parse_illust(&original).unwrap().remove(0);
                assert_eq!(media.archive_key(), original.archive_key());
                assert_eq!(
                    media.metadata.as_ref().unwrap().username,
                    original.metadata.as_ref().unwrap().username
                );
                assert_eq!(
                    media.headers,
                    BTreeMap::from([("Referer".into(), REFERER.into())])
                );
                assert!(!format!("{media:?}").contains("private-access"));
            }
            let requests = server.join().unwrap();
            assert_eq!(requests.len(), 3);
            assert!(requests[1].starts_with(&format!(
                "GET /v1/user/bookmarks/illust?user_id={user}&restrict=private&tag=cat "
            )));
            assert!(requests[2].starts_with(&format!("GET /v1/user/bookmarks/illust?user_id={user}&restrict=private&tag=cat&max_bookmark_id=100 ")));
            assert!(
                requests[1]
                    .to_ascii_lowercase()
                    .contains("authorization: bearer private-access")
            );
        }
    }

    #[tokio::test]
    async fn private_bookmarks_keep_later_work_metadata_and_pagination_requests_lazy() {
        for raw in [
            "https://www.pixiv.net/users/9/bookmarks/artworks?rest=hide",
            "https://www.pixiv.net/bookmark.php?id=9&rest=hide",
        ] {
            let (root, server) = serve_with(|root| {
                vec![token("t"), response("200 OK",
                json!({"illusts":[illust(41,1),animation(42)],"next_url":root.join("v1/user/bookmarks/illust?user_id=9&restrict=private&max_bookmark_id=100").unwrap()}).to_string().as_bytes())]
            });
            let native = pixiv(root);
            let mut files = native.stream(&Url::parse(raw).unwrap());
            assert_eq!(files.next().await.unwrap().unwrap().filename, "41_p0.jpg");
            drop(files);
            assert_eq!(server.join().unwrap().len(), 2);
        }
    }

    #[tokio::test]
    async fn private_bookmarks_refuse_public_next_page_before_sending_authenticated_request() {
        let (root, server) = serve_with(|root| {
            vec![token("t"), response("200 OK",
            json!({"illusts":[illust(41,1)], "next_url":root.join("v1/user/bookmarks/illust?user_id=9&restrict=public").unwrap()}).to_string().as_bytes())]
        });
        let files = pixiv(root)
            .stream(
                &Url::parse("https://www.pixiv.net/users/9/bookmarks/artworks?rest=hide").unwrap(),
            )
            .collect::<Vec<_>>()
            .await;
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].as_ref().unwrap_err().to_string(),
            "Pixiv returned an unexpected next page"
        );
        assert_eq!(server.join().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn own_bookmarks_require_valid_oauth_user_without_guessing_or_echoing_login() {
        for user in [json!(null), json!({"id": "bad-id"}), json!({"id": "0"})] {
            let login = json!({"response": {"access_token": "sensitive-access", "user": user}});
            let (root, server) = serve(vec![response("200 OK", login.to_string().as_bytes())]);
            let result = pixiv(root)
                .stream(&Url::parse("https://www.pixiv.net/bookmark.php").unwrap())
                .collect::<Vec<_>>()
                .await;
            assert_eq!(result.len(), 1);
            assert_eq!(
                result[0].as_ref().unwrap_err().to_string(),
                "Pixiv OAuth response contains no current user ID"
            );
            assert_eq!(server.join().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn bookmark_pagination_and_animation_requests_are_lazy() {
        let (root, server) = serve_with(|root| {
            let next = root
                .join("v1/user/bookmarks/illust?user_id=9&restrict=public&max_bookmark_id=100")
                .unwrap();
            vec![
                token("t"),
                response(
                    "200 OK",
                    json!({"illusts": [illust(42, 1), animation(43)],
                "next_url": next})
                    .to_string()
                    .as_bytes(),
                ),
            ]
        });
        let native = pixiv(root);
        let mut items =
            native.stream(&Url::parse("https://www.pixiv.net/bookmark.php?id=9").unwrap());
        assert_eq!(items.next().await.unwrap().unwrap().filename, "42_p0.jpg");
        drop(items);
        assert_eq!(server.join().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn bookmarks_reject_unexpected_pagination_before_sending_authorization() {
        let (root, server) = serve_with(|root| {
            let next = root
                .join("v1/user/bookmarks/illust?user_id=9&restrict=private")
                .unwrap();
            vec![
                token("t"),
                response(
                    "200 OK",
                    json!({"illusts": [illust(42, 1)],
                "next_url": next})
                    .to_string()
                    .as_bytes(),
                ),
            ]
        });
        let result = pixiv(root)
            .stream(&Url::parse("https://www.pixiv.net/users/9/bookmarks/artworks").unwrap())
            .collect::<Vec<_>>()
            .await;
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0].as_ref().unwrap_err().to_string(),
            "Pixiv returned an unexpected next page"
        );
        assert_eq!(server.join().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn bare_user_dispatch_boundary_precedes_token_validation_and_can_cancel_without_http() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let root = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let native = pixiv(root).with_refresh_token(None);
        let mut events = native.events(&Url::parse("https://www.pixiv.net/en/users/9").unwrap());
        let scope = match events.next().await.unwrap().unwrap() {
            ExtractionEvent::BeginChild(scope) => scope,
            event => panic!("expected dispatcher child before token validation, got {event:?}"),
        };
        assert_eq!(scope.metadata.category(), "pixiv");
        assert_eq!(scope.metadata.subcategory(), "user");
        assert!(scope.metadata.fields().is_empty());
        assert!(matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock));
        scope.control.cancel();
        match events.next().await.unwrap().unwrap() {
            ExtractionEvent::EndChild(control) => assert!(control.same_scope(&scope.control)),
            event => panic!("expected cancelled child to close, got {event:?}"),
        }
        assert!(events.next().await.is_none());
        assert!(matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock));
    }

    #[tokio::test]
    async fn rejected_or_zero_range_bare_user_child_never_validates_token_or_authenticates() {
        use crate::{
            filter::FilterExpression, job_selection::JobSelection, selection::RangeSelection,
        };
        for token in [None, Some("unused-refresh")] {
            for range in [false, true] {
                let listener = TcpListener::bind("127.0.0.1:0").unwrap();
                listener.set_nonblocking(true).unwrap();
                let root =
                    Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
                let native = pixiv(root).with_refresh_token(token);
                let selection = if range {
                    JobSelection {
                        child_range: Some(RangeSelection::parse("0").unwrap()),
                        ..Default::default()
                    }
                } else {
                    JobSelection {
                        child_filter: Some(FilterExpression::parse("False").unwrap()),
                        ..Default::default()
                    }
                };
                let selected = selection
                    .apply(native.events(&Url::parse("https://www.pixiv.net/users/9").unwrap()))
                    .collect::<Vec<_>>()
                    .await;
                assert!(selected.is_empty(), "{selected:?}");
                assert!(
                    matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock)
                );
            }
        }
    }

    #[tokio::test]
    async fn selected_and_default_bare_user_dispatch_keep_direct_artwork_archive_ids() {
        use crate::{
            filter::FilterExpression, job_selection::JobSelection, selection::RangeSelection,
        };
        for selected in [false, true] {
            let (root, server) = serve(vec![
                token("t"),
                response(
                    "200 OK",
                    json!({"illusts": [illust(42, 2)], "next_url": null})
                        .to_string()
                        .as_bytes(),
                ),
                response("200 OK", br#"{"illusts":[],"next_url":null}"#),
            ]);
            let native = pixiv(root);
            let url = Url::parse("https://www.pixiv.net/users/9").unwrap();
            let items = if selected {
                JobSelection {
                    child_range: Some(RangeSelection::parse("1").unwrap()),
                    child_filter: Some(
                        FilterExpression::parse("category == 'pixiv' and subcategory == 'user'")
                            .unwrap(),
                    ),
                    ..Default::default()
                }
                .apply(native.events(&url))
                .collect::<Vec<_>>()
                .await
            } else {
                native.stream(&url).collect::<Vec<_>>().await
            };
            let expected = parse_illust(&illust(42, 2)).unwrap();
            assert_eq!(items.len(), expected.len());
            for (actual, expected) in items.into_iter().zip(expected) {
                let actual = actual.unwrap();
                assert_eq!(actual.archive_key(), expected.archive_key());
                assert_eq!(actual.filename, expected.filename);
            }
            let requests = server.join().unwrap();
            assert_eq!(requests.len(), 3);
            assert!(requests[1].starts_with("GET /v1/user/illusts?user_id=9&type=illust "));
            assert!(requests[2].starts_with("GET /v1/user/illusts?user_id=9&type=manga "));
        }
    }

    #[tokio::test]
    async fn explicit_pixiv_views_bypass_dispatch_child_predicates() {
        use crate::{
            filter::FilterExpression, job_selection::JobSelection, selection::RangeSelection,
        };
        for view in ["artworks", "illustrations", "manga", "bookmarks/artworks"] {
            let mut responses = vec![
                token("t"),
                response(
                    "200 OK",
                    json!({"illusts": [illust(42, 1)], "next_url": null})
                        .to_string()
                        .as_bytes(),
                ),
            ];
            if view == "artworks" {
                responses.push(response("200 OK", br#"{"illusts":[],"next_url":null}"#));
            }
            let (root, server) = serve(responses);
            let native = pixiv(root);
            let url = Url::parse(&format!("https://www.pixiv.net/users/9/{view}")).unwrap();
            let items = JobSelection {
                child_filter: Some(FilterExpression::parse("False").unwrap()),
                child_range: Some(RangeSelection::parse("0").unwrap()),
                ..Default::default()
            }
            .apply(native.events(&url))
            .collect::<Vec<_>>()
            .await;
            assert_eq!(items.len(), 1, "{view}: {items:?}");
            assert_eq!(items[0].as_ref().unwrap().filename, "42_p0.jpg");
            assert_eq!(
                server.join().unwrap().len(),
                if view == "artworks" { 3 } else { 2 }
            );
        }
    }

    #[tokio::test]
    async fn bare_profile_has_one_child_for_all_works_and_explicit_view_has_none() {
        for bare in [false, true] {
            let (root, server) = serve(vec![
                token("t"),
                response(
                    "200 OK",
                    json!({"illusts": [illust(42, 1), illust(43, 1)], "next_url": null})
                        .to_string()
                        .as_bytes(),
                ),
                response("200 OK", br#"{"illusts":[],"next_url":null}"#),
            ]);
            let native = pixiv(root);
            let url = Url::parse(if bare {
                "https://www.pixiv.net/users/9"
            } else {
                "https://www.pixiv.net/users/9/artworks"
            })
            .unwrap();
            let events = native
                .events(&url)
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .collect::<Result<Vec<_>>>()
                .unwrap();
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event, ExtractionEvent::BeginChild(_)))
                    .count(),
                usize::from(bare)
            );
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event, ExtractionEvent::EndChild(_)))
                    .count(),
                usize::from(bare)
            );
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event, ExtractionEvent::Post(_)))
                    .count(),
                2
            );
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event, ExtractionEvent::Media(_)))
                    .count(),
                2
            );
            assert_eq!(server.join().unwrap().len(), 3);
        }
    }

    #[tokio::test]
    async fn accepted_bare_profile_validates_token_in_child_and_direct_view_stays_root_error() {
        let native = Pixiv::new(Duration::from_secs(1)).unwrap();
        let mut events = native.events(&Url::parse("https://www.pixiv.net/users/9").unwrap());
        assert!(matches!(
            events.next().await.unwrap().unwrap(),
            ExtractionEvent::BeginChild(_)
        ));
        assert!(
            events
                .next()
                .await
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("refresh-token")
        );
        assert!(matches!(
            events.next().await.unwrap().unwrap(),
            ExtractionEvent::EndChild(_)
        ));
        assert!(events.next().await.is_none());
        let mut direct =
            native.events(&Url::parse("https://www.pixiv.net/users/9/artworks").unwrap());
        assert!(
            direct
                .next()
                .await
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("refresh-token")
        );
        assert!(direct.next().await.is_none());
    }

    fn included(views: &[&str]) -> Include {
        Include::Views(views.iter().map(|view| (*view).to_owned()).collect())
    }

    #[tokio::test]
    async fn include_empty_and_unknown_views_do_not_validate_token_or_open_http() {
        for include in [included(&[]), included(&["unknown", "artworks-invalid"])] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let root = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
            let native = pixiv(root)
                .with_refresh_token(None)
                .with_include(Some(include));
            assert!(
                native
                    .events(&Url::parse("https://www.pixiv.net/users/9").unwrap())
                    .collect::<Vec<_>>()
                    .await
                    .is_empty()
            );
            assert!(
                matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock)
            );
        }
    }

    #[tokio::test]
    async fn include_all_keeps_exact_catalog_order_and_unsupported_errors_scoped() {
        let page = |id| {
            response(
                "200 OK",
                json!({"illusts": [illust(id, 1)], "next_url": null})
                    .to_string()
                    .as_bytes(),
            )
        };
        let profile = response("200 OK", user_detail().to_string().as_bytes());
        let (root, server) = serve(vec![
            token("access-secret"),
            profile.clone(),
            profile,
            page(41),
            response("200 OK", br#"{"illusts":[],"next_url":null}"#),
            page(42),
            response("200 OK", br#"{"novels":[],"next_url":null}"#),
            response("200 OK", br#"{"novels":[],"next_url":null}"#),
        ]);
        let native = pixiv(root).with_include(Some(Include::All));
        let events = native
            .events(&Url::parse("https://www.pixiv.net/users/9").unwrap())
            .collect::<Vec<_>>()
            .await;
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, Ok(ExtractionEvent::BeginChild(_))))
                .count(),
            7
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, Ok(ExtractionEvent::EndChild(_))))
                .count(),
            7
        );
        let results = events
            .iter()
            .filter_map(|event| match event {
                Ok(ExtractionEvent::Media(media)) => {
                    Some(media.metadata.as_ref().unwrap().post_id.clone())
                }
                Err(error) => Some(error.to_string()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(&results[..4], ["avatar", "background", "41", "42"]);
        assert!(results[4].contains("Sketch"));
        assert!(results.iter().all(|value| !value.contains("secret")));
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 8);
        assert!(requests[1].starts_with("GET /v1/user/detail?user_id=9 "));
        assert!(requests[2].starts_with("GET /v1/user/detail?user_id=9 "));
        assert!(requests[3].starts_with("GET /v1/user/illusts?user_id=9&type=illust "));
        assert!(
            requests[5].starts_with("GET /v1/user/bookmarks/illust?user_id=9&restrict=public ")
        );
    }

    #[tokio::test]
    async fn ordered_aliases_duplicates_and_failed_selected_children_leave_later_siblings_available()
     {
        use crate::job_selection::JobSelection;
        let profile = response("200 OK", user_detail().to_string().as_bytes());
        let (root, server) = serve(vec![
            token("t"),
            response("503 Service Unavailable", b""),
            profile.clone(),
            response("503 Service Unavailable", b""),
            profile,
        ]);
        let native = pixiv(root).with_include(Some(included(&[
            "novel-user",
            "avatar",
            "novel-bookmark",
            "avatar",
        ])));
        let results = JobSelection::default()
            .apply(native.events(&Url::parse("https://www.pixiv.net/users/9").unwrap()))
            .collect::<Vec<_>>()
            .await;
        assert_eq!(results.len(), 4, "{results:?}");
        assert!(results[0].as_ref().unwrap_err().to_string().contains("503"));
        assert_eq!(
            results[1].as_ref().unwrap().filename,
            "avatar_2023-12-31.jpg"
        );
        assert!(results[2].as_ref().unwrap_err().to_string().contains("503"));
        assert_eq!(
            results[3].as_ref().unwrap().filename,
            "avatar_2023-12-31.jpg"
        );
        assert_eq!(server.join().unwrap().len(), 5);
    }

    #[tokio::test]
    async fn rejected_unsupported_include_children_never_validate_token_or_authenticate() {
        use crate::{filter::FilterExpression, job_selection::JobSelection};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let root = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let native = pixiv(root)
            .with_refresh_token(None)
            .with_include(Some(included(&["novel-user", "novel-bookmark", "sketch"])));
        let items = JobSelection {
            child_filter: Some(FilterExpression::parse("False").unwrap()),
            ..Default::default()
        }
        .apply(native.events(&Url::parse("https://www.pixiv.net/users/9").unwrap()))
        .collect::<Vec<_>>()
        .await;
        assert!(items.is_empty(), "{items:?}");
        assert!(matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock));
    }

    #[tokio::test]
    async fn selected_child_range_skips_earlier_unsupported_views_and_stops_later_views() {
        use crate::{job_selection::JobSelection, selection::RangeSelection};
        let (root, server) = serve(vec![
            token("t"),
            response("200 OK", user_detail().to_string().as_bytes()),
        ]);
        let native = pixiv(root).with_include(Some(included(&["sketch", "avatar", "novel-user"])));
        let results = JobSelection {
            child_range: Some(RangeSelection::parse("2").unwrap()),
            ..Default::default()
        }
        .apply(native.events(&Url::parse("https://www.pixiv.net/users/9").unwrap()))
        .collect::<Vec<_>>()
        .await;
        assert_eq!(results.len(), 1, "{results:?}");
        assert_eq!(
            results[0].as_ref().unwrap().filename,
            "avatar_2023-12-31.jpg"
        );
        assert_eq!(server.join().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn included_child_post_and_file_ranges_restart_for_each_selected_view() {
        use crate::{
            job_selection::JobSelection, post_selection::PostSelection, selection::RangeSelection,
        };
        for post_range in [false, true] {
            let (root, server) = serve_with(|root| {
                let page = |id, cursor| {
                    response("200 OK", json!({"illusts": [illust(id, 2), illust(id + 1, 1)],
                    "next_url": root.join(&format!("v1/user/bookmarks/illust?user_id=9&restrict=public&max_bookmark_id={cursor}")).unwrap()}).to_string().as_bytes())
                };
                vec![token("t"), page(41, 100), page(43, 200)]
            });
            let native = pixiv(root).with_include(Some(included(&["favorite", "favorite"])));
            let selection = if post_range {
                JobSelection {
                    post_selection: Some(PostSelection::new(
                        Some(RangeSelection::parse("1").unwrap()),
                        None,
                    )),
                    ..Default::default()
                }
            } else {
                JobSelection {
                    file_range: Some(RangeSelection::parse("1").unwrap()),
                    ..Default::default()
                }
            };
            let items = selection
                .apply(native.events(&Url::parse("https://www.pixiv.net/users/9").unwrap()))
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .collect::<Result<Vec<_>>>()
                .unwrap();
            assert_eq!(items.len(), if post_range { 4 } else { 2 });
            assert_eq!(items[0].metadata.as_ref().unwrap().post_id, "41");
            assert_eq!(
                items.last().unwrap().metadata.as_ref().unwrap().post_id,
                "43"
            );
            assert_eq!(server.join().unwrap().len(), 3);
        }
    }

    #[tokio::test]
    async fn explicit_profile_assets_bypass_include_and_child_selection_and_keep_referer_only() {
        use crate::{filter::FilterExpression, job_selection::JobSelection};
        for kind in ["avatar", "background"] {
            let (root, server) = serve(vec![
                token("access-secret"),
                response("200 OK", user_detail().to_string().as_bytes()),
            ]);
            let native = pixiv(root).with_include(Some(included(&[])));
            let url = Url::parse(&format!("https://www.pixiv.net/users/9/{kind}")).unwrap();
            let items = JobSelection {
                child_filter: Some(FilterExpression::parse("False").unwrap()),
                ..Default::default()
            }
            .apply(native.events(&url))
            .collect::<Vec<_>>()
            .await;
            assert_eq!(items.len(), 1);
            let media = items[0].as_ref().unwrap();
            assert_eq!(media.metadata.as_ref().unwrap().post_id, kind);
            assert_eq!(
                media.headers,
                BTreeMap::from([("Referer".into(), REFERER.into())])
            );
            let requests = server.join().unwrap();
            assert!(
                requests[1]
                    .to_ascii_lowercase()
                    .contains("authorization: bearer access-secret")
            );
        }
    }

    #[tokio::test]
    async fn requires_refresh_token_without_network() {
        let error = Pixiv::new(Duration::from_secs(5))
            .unwrap()
            .with_api_root(Url::parse("http://127.0.0.1:9/").unwrap())
            .stream(&Url::parse("https://www.pixiv.net/artworks/1").unwrap())
            .collect::<Vec<_>>()
            .await
            .remove(0)
            .unwrap_err()
            .to_string();
        assert!(error.contains("refresh-token"), "{error}");
    }

    #[tokio::test]
    async fn rejected_refresh_token_is_not_echoed() {
        let (root, server) = serve(vec![response(
            "400 Bad Request",
            b"{\"error\":\"invalid_grant\"}",
        )]);
        let error = pixiv(root)
            .with_retries(3)
            .stream(&Url::parse("https://www.pixiv.net/artworks/1").unwrap())
            .collect::<Vec<_>>()
            .await
            .remove(0)
            .unwrap_err()
            .to_string();
        assert!(error.contains("refresh-token"), "{error}");
        assert!(!error.contains("refresh-secret") && !error.contains("127.0.0.1"));
        assert_eq!(server.join().unwrap().len(), 1);
    }
}
