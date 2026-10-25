//! Experimental native VSCO extraction.
//!
//! Mirrors gallery_dl/extractor/vsco.py: user profiles (upstream `Dispatch`
//! over avatar, gallery, spaces and collection, `include` defaulting to the
//! gallery), galleries, collections, spaces, a user's space list, avatars,
//! single images and videos. Pages are read from the page-embedded
//! `window.__PRELOADED_STATE__` JSON, which is undocumented and may change
//! without notice; it also carries the anonymous `tkn` bearer token for the
//! `api/3.0/medias/profile`, `api/2.0/collections` and `grpc/spaces` APIs.
//! Listings paginate lazily, capped at 200 pages. Files carry upstream's
//! keyword dictionaries and default formats. HLS videos are handed to the
//! manifest downloader (upstream `ytdl:` with `_ytdl_manifest: hls`); the
//! `videos` setting disables videos. Media must be HTTPS on a `*.vsco.co`
//! host.
use crate::{
    Result, error,
    event::{ChildMetadata, ExtractionEvent, KeywordPolicy, child_job, group_with_keywords},
    extract::{ManifestProtocol, Media, MediaJob, PostMetadata},
    file_metadata::FileMetadata,
    imagehosts::between,
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

const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:128.0) Gecko/20100101 Firefox/128.0";
const MAX_RESPONSE: usize = 16 * 1024 * 1024;
const MAX_PAGES: u64 = 200;
/// First path segments on vsco.co that are not user galleries.
const RESERVED: [&str; 16] = [
    "api",
    "about",
    "search",
    "spaces",
    "user",
    "feed",
    "store",
    "subscribe",
    "join",
    "login",
    "signup",
    "help",
    "static",
    "discover",
    "settings",
    "_",
];

const FILENAME: &str = "{id}.{extension}";
const USER_DIRECTORY: &[&str] = &["{category}", "{user}"];
/// Upstream extractor classes supported natively, for `--list-extractors`.
pub const SUBCATEGORIES: &[&str] = &[
    "avatar",
    "collection",
    "gallery",
    "image",
    "space",
    "spaces",
    "user",
    "video",
];

pub static USER: Route = Route::new("user", FILENAME, USER_DIRECTORY, "{id}");
pub static GALLERY: Route = Route::new("gallery", FILENAME, USER_DIRECTORY, "{id}");
pub static COLLECTION: Route = Route::new(
    "collection",
    FILENAME,
    &["{category}", "{user}", "collection"],
    "c_{user}_{id}",
);
pub static SPACE: Route = Route::new(
    "space",
    FILENAME,
    &["{category}", "space", "{user}"],
    "s_{user}_{id}",
);
pub static SPACES: Route = Route::new("spaces", FILENAME, USER_DIRECTORY, "{id}");
pub static AVATAR: Route = Route::new("avatar", FILENAME, USER_DIRECTORY, "{id}");
pub static IMAGE: Route = Route::new("image", FILENAME, USER_DIRECTORY, "{id}");
pub static VIDEO: Route = Route::new("video", FILENAME, USER_DIRECTORY, "{id}");

/// Upstream `VscoUserExtractor` dispatch order.
const DISPATCH: [&str; 4] = ["avatar", "gallery", "spaces", "collection"];

/// Media IDs: hex object IDs, video UUID-like IDs and profile image IDs.
pub fn valid_id(id: &str) -> bool {
    (8..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn valid_user(user: &str) -> bool {
    (1..=64).contains(&user.len())
        && user
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

fn valid_space(id: &str) -> bool {
    (1..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// Upstream `user`: dispatch to the `include` extractors.
    User {
        user: String,
    },
    Gallery {
        user: String,
    },
    Collection {
        user: String,
    },
    Space {
        id: String,
    },
    Spaces {
        user: String,
    },
    Avatar {
        user: String,
    },
    Media {
        user: String,
        id: String,
    },
    Video {
        user: String,
        id: String,
    },
}

impl Target {
    pub fn route(&self) -> &'static Route {
        match self {
            Self::User { .. } => &USER,
            Self::Gallery { .. } => &GALLERY,
            Self::Collection { .. } => &COLLECTION,
            Self::Space { .. } => &SPACE,
            Self::Spaces { .. } => &SPACES,
            Self::Avatar { .. } => &AVATAR,
            Self::Media { .. } => &IMAGE,
            Self::Video { .. } => &VIDEO,
        }
    }
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
            "unsupported VSCO URL; expected https://vsco.co/USER[/gallery|/collection|/spaces|/avatar|/media/ID|/video/ID] or https://vsco.co/spaces/ID",
        )
    };
    if !plain(url) || !matches!(url.host_str(), Some("vsco.co" | "www.vsco.co")) {
        return Err(unsupported());
    }
    let parts: Vec<&str> = url
        .path_segments()
        .map(|s| s.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    // Upstream lower-cases the user (or space ID) taken from the URL.
    let lower = |s: &str| s.to_ascii_lowercase();
    match parts[..] {
        ["spaces", id, ..] if valid_space(id) => Ok(Target::Space { id: lower(id) }),
        [user, ..] if !valid_user(user) || RESERVED.contains(&user) => Err(unsupported()),
        [user, "media", id] if id.bytes().all(|b| b.is_ascii_hexdigit()) && valid_id(id) => {
            Ok(Target::Media {
                user: lower(user),
                id: id.into(),
            })
        }
        [user, "video", id] if valid_id(id) => Ok(Target::Video {
            user: lower(user),
            id: id.into(),
        }),
        [user] => Ok(Target::User { user: lower(user) }),
        [user, "gallery" | "images", ..] => Ok(Target::Gallery { user: lower(user) }),
        [user, "collection", ..] => Ok(Target::Collection { user: lower(user) }),
        [user, "spaces", ..] => Ok(Target::Spaces { user: lower(user) }),
        [user, "avatar", ..] => Ok(Target::Avatar { user: lower(user) }),
        _ => Err(unsupported()),
    }
}

pub fn is_vsco_url(url: &Url) -> bool {
    target(url).is_ok()
}

/// HTTPS media on a `*.vsco.co` host; scheme-less URLs get `https://`.
pub fn media_url(raw: &str) -> Option<Url> {
    let url = Url::parse(&ensure_http_scheme(raw.trim())).ok()?;
    let host = url.host_str()?;
    (url.scheme() == "https"
        && plain(&url)
        && host.strip_suffix(".vsco.co").is_some_and(|label| {
            !label.is_empty()
                && label != "www"
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.'))
        }))
    .then_some(url)
}

/// gallery-dl `text.ensure_http_scheme`.
fn ensure_http_scheme(url: &str) -> String {
    if url.is_empty() || url.starts_with("https://") || url.starts_with("http://") {
        url.to_owned()
    } else {
        format!("https://{}", url.trim_start_matches(['/', ':']))
    }
}

/// Upstream's image URL from a `responsive_url`: AWS CDN paths map to
/// `image-CDN.vsco.co`, numeric ones to `image.vsco.co`.
pub fn image_url(responsive: &str) -> String {
    let base = responsive.split_once('/').map_or("", |(_, rest)| rest);
    let (cdn, path) = base.split_once('/').unwrap_or((base, ""));
    if cdn.starts_with("aws") {
        format!("https://image-{cdn}.vsco.co/{path}")
    } else if !cdn.is_empty() && cdn.bytes().all(|b| b.is_ascii_digit()) {
        format!("https://image.vsco.co/{base}")
    } else if responsive.starts_with("http") {
        responsive.to_owned()
    } else {
        format!("https://{responsive}")
    }
}

/// Parse `window.__PRELOADED_STATE__` out of a VSCO page.
pub fn preloaded_state(html: &str) -> Result<Value> {
    let raw = between(html, "__PRELOADED_STATE__ = ", "<")
        .ok_or_else(|| error("VSCO page contains no preloaded state"))?;
    let raw = raw.trim().trim_end_matches(';');
    serde_json::from_str(raw)
        .or_else(|_| serde_json::from_str(&raw.replace("\":undefined", "\":null")))
        .map_err(|_| error("VSCO preloaded state is not valid JSON"))
}

fn page_state(html: &[u8]) -> Result<Value> {
    let html = std::str::from_utf8(html).map_err(|_| error("VSCO returned invalid HTML"))?;
    preloaded_state(html)
}

/// The anonymous bearer token of a page state.
fn token(state: &Value) -> Result<String> {
    state
        .pointer("/users/currentUser/tkn")
        .and_then(Value::as_str)
        .filter(|t| {
            (8..=512).contains(&t.len())
                && t.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        })
        .map(str::to_owned)
        .ok_or_else(|| error("VSCO page contains no access token"))
}

/// The user's `site` object of a page state.
fn site<'a>(state: &'a Value, user: &str) -> Result<&'a Value> {
    state
        .pointer("/sites/siteByUsername")
        .and_then(Value::as_object)
        .and_then(|sites| {
            sites.get(user).or_else(|| {
                sites
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case(user))
                    .map(|(_, v)| v)
            })
        })
        .and_then(|s| s.get("site"))
        .ok_or_else(|| error("VSCO user not found"))
}

/// A numeric or alphanumeric ID as text.
fn id_text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
    .filter(|id| valid_space(id))
}

/// Site ID and bearer token from a gallery page.
pub fn parse_gallery_page(html: &[u8], user: &str) -> Result<(String, String)> {
    let state = page_state(html)?;
    let id = id_text(site(&state, user)?.get("id"))
        .filter(|id| id.len() <= 20 && id.bytes().all(|b| b.is_ascii_digit()))
        .ok_or_else(|| error("VSCO user not found"))?;
    Ok((id, token(&state)?))
}

/// Upstream `_transform_media`: page-state camelCase media in API form;
/// `None` without a `responsiveUrl`.
pub fn transform_media(media: &Value) -> Option<Map<String, Value>> {
    let mut out = media.as_object()?.clone();
    if !out.contains_key("responsiveUrl") {
        return None;
    }
    let get = |key: &str| out.get(key).cloned().unwrap_or(Value::Null);
    let renamed = [
        ("_id", get("id")),
        ("is_video", get("isVideo")),
        ("grid_name", get("gridName")),
        ("upload_date", get("uploadDate")),
        ("responsive_url", get("responsiveUrl")),
        ("video_url", get("videoUrl")),
        ("image_meta", get("imageMeta")),
    ];
    out.extend(renamed.map(|(k, v)| (k.to_owned(), v)));
    Some(out)
}

/// Upstream `_transform_video` for API objects with a `playback_url`.
fn transform_video(media: &mut Map<String, Value>) {
    let get = |key: &str| media.get(key).cloned().unwrap_or(Value::Null);
    let renamed = [
        ("is_video", Value::Bool(true)),
        ("grid_name", "".into()),
        ("video_url", get("playback_url")),
        ("responsive_url", get("poster_url")),
        ("upload_date", get("created_date")),
    ];
    media.extend(renamed.map(|(k, v)| (k.to_owned(), v)));
}

fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Number(n)) => n.as_f64().is_some_and(|n| n != 0.0),
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(map)) => !map.is_empty(),
        Some(Value::Bool(true)) => true,
    }
}

/// Upstream `VscoExtractor.items` for one media object: `None` for skipped
/// objects (no image URL, disabled videos or untrusted hosts).
pub fn media_item(
    img: Map<String, Value>,
    user: &str,
    route: &'static Route,
    videos: bool,
) -> Option<Media> {
    let mut img = img;
    if img.contains_key("playback_url") {
        transform_video(&mut img);
    } else if !img.contains_key("responsive_url") {
        return None;
    }
    let id = id_text(img.get("_id")).filter(|id| valid_id(id))?;
    let video = truthy(img.get("is_video"));
    let raw = if video {
        if !videos {
            return None;
        }
        ensure_http_scheme(img.get("video_url").and_then(Value::as_str).unwrap_or(""))
    } else {
        image_url(img.get("responsive_url").and_then(Value::as_str)?)
    };
    let url = media_url(&raw)?;
    let upload = img.get("upload_date").and_then(Value::as_f64);
    let timestamp = upload.map(|ms| (ms / 1000.0).floor() as i64);
    let tags: Vec<Value> = img
        .get("tags")
        .and_then(Value::as_array)
        .map(|tags| tags.iter().filter_map(|t| t.get("text").cloned()).collect())
        .unwrap_or_default();
    let meta = match img.get("image_meta") {
        Some(meta) if truthy(Some(meta)) => meta.clone(),
        _ => Value::Object(Map::new()),
    };
    let description = match img.get("description") {
        Some(d) if truthy(Some(d)) => d.clone(),
        _ => "".into(),
    };
    let (name, mut ext) = crate::sites::nameext(&url);
    let mut fields = Map::new();
    fields.insert("id".into(), img.get("_id").cloned().unwrap_or_default());
    fields.insert("user".into(), user.into());
    fields.insert(
        "grid".into(),
        img.get("grid_name").cloned().unwrap_or_default(),
    );
    fields.insert("meta".into(), meta);
    fields.insert("tags".into(), tags.into());
    fields.insert(
        "date".into(),
        match timestamp {
            Some(_) => crate::filter::source_date_text(timestamp).into(),
            None => Value::Null,
        },
    );
    fields.insert("video".into(), video.into());
    fields.insert(
        "width".into(),
        img.get("width").cloned().unwrap_or_default(),
    );
    fields.insert(
        "height".into(),
        img.get("height").cloned().unwrap_or_default(),
    );
    fields.insert("description".into(), description.clone());
    fields.insert("filename".into(), name.clone().into());
    let mut job = MediaJob::DirectHttp;
    if ext == "m3u8" {
        fields.insert("_ytdl_manifest".into(), "hls".into());
        ext = "mp4".into();
        job = MediaJob::Manifest {
            protocol: ManifestProtocol::Hls,
            base_url: url.clone(),
            inline_data: None,
        };
    }
    fields.insert("extension".into(), ext.clone().into());
    fields.insert("category".into(), "vsco".into());
    fields.insert("subcategory".into(), route.subcategory.into());
    let dimension = |key: &str| img.get(key).and_then(Value::as_u64).filter(|v| *v > 0);
    Some(Media {
        filename: if ext.is_empty() {
            name
        } else {
            format!("{name}.{ext}")
        },
        job,
        category: "vsco".into(),
        metadata: Some(PostMetadata {
            file: Some(FileMetadata {
                width: dimension("width"),
                height: dimension("height"),
                media_type: Some(Arc::from(if video { "video" } else { "photo" })),
                ..FileMetadata::default()
            }),
            post_id: id.clone(),
            media_id: id,
            username: Some(Arc::from(user)),
            caption: description
                .as_str()
                .filter(|d| !d.is_empty())
                .map(Arc::from),
            timestamp,
            index: 1,
            site_keywords: Some(Arc::new(SiteKeywords { route, fields })),
            ..PostMetadata::default()
        }),
        url,
        headers: BTreeMap::new(),
        handoff: BTreeMap::new(),
    })
}

/// The media object of a single media or video page.
fn page_media(state: &Value, id: &str) -> Result<Value> {
    let by_id = state
        .pointer("/medias/byId")
        .and_then(Value::as_object)
        .ok_or_else(|| error("VSCO media not found"))?;
    by_id
        .get(id)
        .or_else(|| by_id.values().next_back())
        .and_then(|entry| entry.get("media"))
        .cloned()
        .ok_or_else(|| error("VSCO media not found"))
}

/// API media objects and the next cursor of a profile page.
pub type ProfilePage = (Vec<Map<String, Value>>, Option<String>);

/// Parse one `medias/profile` page into media objects and the next cursor.
pub fn parse_profile(bytes: &[u8]) -> Result<ProfilePage> {
    let body: Value =
        serde_json::from_slice(bytes).map_err(|_| error("VSCO returned invalid JSON"))?;
    let list = match body.get("media") {
        None | Some(Value::Null) => return Ok((Vec::new(), None)),
        Some(list) => list
            .as_array()
            .ok_or_else(|| error("VSCO response contains no media list"))?,
    };
    let items = list
        .iter()
        .filter_map(|entry| {
            let kind = entry.get("type")?.as_str()?;
            entry.get(kind)?.as_object().cloned()
        })
        .collect();
    let cursor = body
        .get("next_cursor")
        .and_then(Value::as_str)
        .filter(|c| !c.is_empty())
        .map(str::to_owned);
    Ok((items, cursor))
}

/// Upstream `VscoSpaceExtractor._pagination` for one batch of posts.
fn space_posts(data: &Value) -> Vec<Map<String, Value>> {
    data.get("postsList")
        .and_then(Value::as_array)
        .map(|posts| {
            posts
                .iter()
                .filter_map(|post| {
                    let mut media = transform_media(post.get("image")?)?;
                    let seconds = media.get("upload_date")?.get("sec")?.as_f64()?;
                    media.insert("upload_date".into(), (seconds * 1000.0).into());
                    Some(media)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The next space cursor, or `None` at the end.
fn space_cursor(data: &Value) -> Option<String> {
    let cursor = data.get("cursor")?;
    if truthy(cursor.get("atEnd")) {
        return None;
    }
    id_text(cursor.pointer("/postcursorcontext/postId"))
}

/// Upstream `include` option values for user profiles.
fn include(value: Option<&Value>) -> Result<Vec<&'static str>> {
    let names: Vec<String> = match value {
        None | Some(Value::Null) => vec!["gallery".into()],
        Some(Value::String(s)) if s == "all" => DISPATCH.iter().map(|s| (*s).into()).collect(),
        Some(Value::String(s)) => s.replace(' ', "").split(',').map(str::to_owned).collect(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| error("VSCO include must list extractor names"))
            })
            .collect::<Result<_>>()?,
        Some(_) => return Err(error("VSCO include must be a string or a list")),
    };
    Ok(names
        .iter()
        .filter_map(|name| DISPATCH.iter().copied().find(|sub| sub == name))
        .collect())
}

/// Request decoration: page loads, REST APIs or the `grpc` space APIs.
#[derive(Clone, Copy)]
enum Auth<'a> {
    Page,
    Api { token: &'a str, referer: &'a str },
    Grpc { token: &'a str, referer: &'a str },
}

pub struct Vsco {
    client: Client,
    network: NetworkOptions,
    root: Url,
    image_root: Url,
    videos: bool,
    include: Vec<&'static str>,
    timeout: Duration,
    request_timeout: Duration,
    retries: u8,
    rate_limits: RateLimits,
}

impl Vsco {
    pub fn new(timeout: Duration) -> Result<Self> {
        Ok(Self {
            client: NetworkOptions::default().client(USER_AGENT, timeout)?,
            network: NetworkOptions::default(),
            root: Url::parse("https://vsco.co/").expect("static URL"),
            image_root: Url::parse("https://im.vsco.co/").expect("static URL"),
            videos: true,
            include: vec!["gallery"],
            timeout,
            request_timeout: timeout,
            retries: 0,
            rate_limits: RateLimits::default(),
        })
    }

    /// Apply proxy and user-agent settings to requests and emitted media.
    pub fn with_network(mut self, network: &NetworkOptions) -> Result<Self> {
        self.client = network.client(USER_AGENT, self.timeout)?;
        self.network = network.clone();
        Ok(self)
    }

    /// Apply upstream extractor settings (`extractor.vsco.*`): `videos` and
    /// the profile `include` list.
    pub fn with_options(mut self, options: Option<&Map<String, Value>>) -> Result<Self> {
        let Some(options) = options else {
            return Ok(self);
        };
        self.videos = match options.get("videos") {
            None | Some(Value::Null) => true,
            Some(Value::Bool(value)) => *value,
            Some(_) => return Err(error("VSCO videos must be a boolean")),
        };
        self.include = include(options.get("include"))?;
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

    /// Override the site and avatar origins; intended for local tests.
    pub fn with_root(mut self, root: Url) -> Self {
        self.image_root = root.join("im/").expect("static path");
        self.root = root;
        self
    }

    async fn call(&self, path: &str, auth: Auth<'_>) -> Result<Vec<u8>> {
        let endpoint = self
            .root
            .join(path)
            .map_err(|_| error("invalid VSCO path"))?;
        tokio::time::timeout(self.timeout, self.request(&endpoint, auth))
            .await
            .map_err(|_| error("native VSCO extraction timed out"))?
    }

    async fn json(&self, path: &str, auth: Auth<'_>) -> Result<Value> {
        serde_json::from_slice(&self.call(path, auth).await?)
            .map_err(|_| error("VSCO returned invalid JSON"))
    }

    async fn state(&self, path: &str) -> Result<Value> {
        page_state(&self.call(path, Auth::Page).await?)
    }

    fn finish(&self, items: Vec<Media>) -> Vec<ExtractionEvent> {
        let mut out = Vec::new();
        for mut item in items {
            self.network.apply_media(&mut item);
            out.extend(group_with_keywords(
                vec![item],
                "vsco",
                KeywordPolicy::default(),
            ));
        }
        out
    }

    fn items(
        &self,
        objects: Vec<Map<String, Value>>,
        user: &str,
        route: &'static Route,
    ) -> Events<'_> {
        let media = objects
            .into_iter()
            .filter_map(|img| media_item(img, user, route, self.videos))
            .collect();
        crate::sites::events(self.finish(media))
    }

    /// Upstream `VscoImageExtractor`.
    async fn media(&self, user: &str, id: &str) -> Result<Events<'_>> {
        let state = self.state(&format!("{user}/media/{id}")).await?;
        let media = page_media(&state, id)?;
        let img = transform_media(&media).ok_or_else(|| error("VSCO media not found"))?;
        Ok(self.items(vec![img], user, &IMAGE))
    }

    /// Upstream `VscoVideoExtractor`.
    async fn video(&self, user: &str, id: &str) -> Result<Events<'_>> {
        let state = self.state(&format!("{user}/video/{id}")).await?;
        let media = page_media(&state, id)?;
        let get = |key: &str| media.get(key).cloned().unwrap_or(Value::Null);
        let img: Map<String, Value> = [
            ("_id", get("id")),
            ("is_video", Value::Bool(true)),
            ("grid_name", "".into()),
            ("upload_date", get("createdDate")),
            ("responsive_url", get("posterUrl")),
            ("video_url", get("playbackUrl")),
            ("image_meta", Value::Null),
            ("width", get("width")),
            ("height", get("height")),
            ("description", get("description")),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect();
        Ok(self.items(vec![img], user, &VIDEO))
    }

    /// Upstream `VscoAvatarExtractor`: the profile image's redirect target.
    async fn avatar(&self, user: &str) -> Result<Events<'_>> {
        let page = self.call(&format!("{user}/gallery"), Auth::Page).await?;
        let page = String::from_utf8_lossy(&page);
        let piid = between(&page, "\"profileImageId\":\"", "\"")
            .filter(|id| valid_id(id))
            .ok_or_else(|| error("VSCO user has no profile image"))?
            .to_owned();
        let url = self.image_root.join(&piid).expect("validated ID");
        let location = tokio::time::timeout(self.timeout, self.location(&url))
            .await
            .map_err(|_| error("native VSCO extraction timed out"))??;
        let img: Map<String, Value> = [
            ("_id", Value::from(piid)),
            ("is_video", false.into()),
            ("grid_name", "".into()),
            ("upload_date", 0.into()),
            ("responsive_url", location.into()),
            ("video_url", "".into()),
            ("image_meta", Value::Null),
            ("width", 0.into()),
            ("height", 0.into()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect();
        Ok(self.items(vec![img], user, &AVATAR))
    }

    /// Upstream `VscoGalleryExtractor`: the profile API by cursor.
    async fn gallery(&self, user: String) -> Result<Events<'_>> {
        let page = self.call(&format!("{user}/gallery"), Auth::Page).await?;
        let (site, token) = parse_gallery_page(&page, &user)?;
        let referer = self.root.join(&user).map(String::from).unwrap_or_default();
        Ok(crate::sites::paginate(
            Some((None::<String>, 0u64)),
            move |state| {
                let (user, site, token, referer) =
                    (user.clone(), site.clone(), token.clone(), referer.clone());
                async move {
                    let Some((cursor, page)) = state else {
                        return Ok(None);
                    };
                    let mut path = format!("api/3.0/medias/profile?site_id={site}&limit=14");
                    if let Some(cursor) = &cursor {
                        let encoded: String =
                            url::form_urlencoded::byte_serialize(cursor.as_bytes()).collect();
                        path.push_str("&cursor=");
                        path.push_str(&encoded);
                    }
                    let auth = Auth::Api {
                        token: &token,
                        referer: &referer,
                    };
                    let body = self.call(&path, auth).await?;
                    let (objects, next) = parse_profile(&body)?;
                    let more = !objects.is_empty()
                        && next.is_some()
                        && next != cursor
                        && page + 1 < MAX_PAGES;
                    Ok(Some((
                        self.items(objects, &user, &GALLERY),
                        more.then_some((next, page + 1)),
                    )))
                }
            },
        ))
    }

    /// Upstream `VscoCollectionExtractor`: page-state media, then the
    /// collection API from page 2.
    async fn collection(&self, user: String) -> Result<Events<'_>> {
        let state = self.state(&format!("{user}/collection/1")).await?;
        let token = token(&state)?;
        let cid = id_text(site(&state, &user)?.get("siteCollectionId"))
            .ok_or_else(|| error("VSCO user has no collection"))?;
        let first: Vec<Map<String, Value>> = state
            .pointer(&format!("/collections/byId/{cid}/1/collection"))
            .and_then(Value::as_array)
            .map(|ids| {
                ids.iter()
                    .filter_map(|entry| {
                        let mid = id_text(entry.get("id"))?;
                        transform_media(state.pointer(&format!("/medias/byId/{mid}/media"))?)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let referer = self.root.join(&user).map(String::from).unwrap_or_default();
        let head = self.items(first, &user, &COLLECTION);
        let rest = crate::sites::paginate(Some(2u64), move |page| {
            let (user, cid, token, referer) =
                (user.clone(), cid.clone(), token.clone(), referer.clone());
            async move {
                let Some(page) = page else {
                    return Ok(None);
                };
                let auth = Auth::Api {
                    token: &token,
                    referer: &referer,
                };
                let body = self
                    .json(
                        &format!("api/2.0/collections/{cid}/medias?page={page}&size=20"),
                        auth,
                    )
                    .await?;
                let objects: Vec<Map<String, Value>> = body
                    .get("medias")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|m| m.as_object().cloned())
                            .collect()
                    })
                    .unwrap_or_default();
                let more = !objects.is_empty() && page < MAX_PAGES;
                Ok(Some((
                    self.items(objects, &user, &COLLECTION),
                    more.then_some(page + 1),
                )))
            }
        });
        Ok(head.chain(rest).boxed())
    }

    /// Upstream `VscoSpaceExtractor`: page-state posts, then the space
    /// posts API by post cursor.
    async fn space(&self, sid: String) -> Result<Events<'_>> {
        let mut state = self.state(&format!("spaces/{sid}")).await?;
        let token = token(&state)?;
        let images = state
            .pointer("/entities/postImages")
            .cloned()
            .unwrap_or_default();
        let posts = state.pointer_mut("/entities/posts").map(Value::take);
        let posts = posts.unwrap_or_default();
        let space = state
            .pointer(&format!("/spaces/byId/{sid}"))
            .ok_or_else(|| error("VSCO space not found"))?;
        let resolve = |pid: &Value| -> Option<Value> {
            let mut post = posts.get(id_text(Some(pid))?)?.clone();
            let image = images.get(id_text(post.get("image"))?)?.clone();
            post.as_object_mut()?.insert("image".into(), image);
            Some(post)
        };
        let list: Vec<Value> = space
            .get("postsList")
            .and_then(Value::as_array)
            .map(|ids| ids.iter().filter_map(resolve).collect())
            .unwrap_or_default();
        let mut data = space.clone();
        data["postsList"] = list.into();
        let referer = self
            .root
            .join(&format!("spaces/{sid}"))
            .map(String::from)
            .unwrap_or_default();
        Ok(crate::sites::paginate(Some((data, 0u64)), move |state| {
            let (sid, token, referer) = (sid.clone(), token.clone(), referer.clone());
            async move {
                let Some((data, page)) = state else {
                    return Ok(None);
                };
                let events = self.items(space_posts(&data), &sid, &SPACE);
                let next = match space_cursor(&data) {
                    Some(cursor) if page + 1 < MAX_PAGES => {
                        let auth = Auth::Grpc {
                            token: &token,
                            referer: &referer,
                        };
                        let path = format!("grpc/spaces/{sid}/posts?cursor={cursor}");
                        Some((self.json(&path, auth).await?, page + 1))
                    }
                    _ => None,
                };
                Ok(Some((events, next)))
            }
        }))
    }

    /// Upstream `VscoSpacesExtractor`: queue each of a user's spaces.
    async fn spaces(&self, user: String) -> Result<Events<'_>> {
        let path = format!("{user}/spaces");
        let state = self.state(&path).await?;
        let token = token(&state)?;
        let uid = id_text(site(&state, &user)?.get("userId"))
            .ok_or_else(|| error("VSCO user not found"))?;
        let referer = self.root.join(&path).map(String::from).unwrap_or_default();
        let auth = Auth::Grpc {
            token: &token,
            referer: &referer,
        };
        let data = self.json(&format!("grpc/spaces/user/{uid}"), auth).await?;
        let children: Vec<Events<'_>> = data
            .get("spacesWithRoleList")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(|entry| {
                        let space = entry.get("space")?.as_object()?.clone();
                        let id = id_text(space.get("id"))?;
                        let url = Url::parse(&format!("https://vsco.co/spaces/{id}")).ok()?;
                        Some(child_job(
                            ChildMetadata::from_raw("vsco", "spaces", &space),
                            move || self.events(&url),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(stream::iter(children).flatten().boxed())
    }

    /// Upstream `VscoUserExtractor`: queue the `include` extractors.
    fn dispatch(&self, user: &str) -> Events<'_> {
        let children: Vec<Events<'_>> = self
            .include
            .iter()
            .filter_map(|sub| Url::parse(&format!("https://vsco.co/{user}/{sub}")).ok())
            .map(|url| {
                child_job(
                    ChildMetadata::from_raw("vsco", "user", &Map::new()),
                    move || self.events(&url),
                )
            })
            .collect();
        stream::iter(children).flatten().boxed()
    }

    pub fn stream(&self, url: &Url) -> BoxStream<'_, Result<Media>> {
        crate::event::media_only(self.events(url))
    }

    /// Emit one post boundary per media; listings paginate lazily.
    pub fn events(&self, url: &Url) -> BoxStream<'_, Result<ExtractionEvent>> {
        let target = match target(url) {
            Ok(target) => target,
            Err(e) => return crate::sites::fail(e),
        };
        match target {
            Target::User { user } => self.dispatch(&user),
            Target::Media { user, id } => {
                crate::sites::once(async move { self.media(&user, &id).await })
            }
            Target::Video { user, id } => {
                crate::sites::once(async move { self.video(&user, &id).await })
            }
            Target::Avatar { user } => crate::sites::once(async move { self.avatar(&user).await }),
            Target::Gallery { user } => crate::sites::once(self.gallery(user)),
            Target::Collection { user } => crate::sites::once(self.collection(user)),
            Target::Space { id } => crate::sites::once(self.space(id)),
            Target::Spaces { user } => crate::sites::once(self.spaces(user)),
        }
    }

    /// The `Location` of a redirect, without following it.
    async fn location(&self, url: &Url) -> Result<String> {
        let response = self
            .client
            .get(url.clone())
            .timeout(self.request_timeout)
            .send()
            .await
            .map_err(|_| error("VSCO request failed; check network access"))?;
        if !response.status().is_redirection() {
            return Err(error("VSCO profile image did not redirect"));
        }
        response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok())
            .and_then(|raw| url.join(raw).ok())
            .filter(|target| media_url(target.as_str()).is_some())
            .map(String::from)
            .ok_or_else(|| error("VSCO profile image redirect is not a VSCO image"))
    }

    async fn request(&self, endpoint: &Url, auth: Auth<'_>) -> Result<Vec<u8>> {
        for attempt in 0..=self.retries {
            match self.attempt(endpoint, auth, attempt).await {
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
        auth: Auth<'_>,
        attempt: u8,
    ) -> std::result::Result<Vec<u8>, Failure> {
        let mut target = endpoint.clone();
        let mut redirects = Redirects::new(endpoint);
        let mut response = loop {
            self.rate_limits.wait(&target, self.timeout).await?;
            let mut request = self
                .client
                .get(target.clone())
                .timeout(self.request_timeout)
                .header("Accept-Language", "en-US,en;q=0.5");
            // Only send the token to the origin it came from.
            let same_origin = target.origin() == endpoint.origin();
            request = match auth {
                Auth::Api { token, referer } if same_origin => request
                    .header("Referer", referer)
                    .header("Authorization", format!("Bearer {token}"))
                    .header("Accept", "application/json")
                    .header("X-Client-Build", "1")
                    .header("X-Client-Platform", "web"),
                Auth::Grpc { token, referer } if same_origin => request
                    .header("Referer", referer)
                    .header("Authorization", format!("Bearer {token}"))
                    .header("Accept", "application/json")
                    .header("Content-Type", "application/json"),
                _ => request.header("Referer", "https://vsco.co/"),
            };
            let response = request
                .send()
                .await
                .map_err(|_| Failure::transient("VSCO request failed; check network access"))?;
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
                            format!("VSCO rate limit; retry after {} seconds", delay.as_secs())
                        }
                        None => "VSCO rate limit; retry later".into(),
                    }
                } else {
                    format!("VSCO returned HTTP {status}")
                },
            ));
        }
        if matches!(status.as_u16(), 404 | 410) {
            return Err(error("VSCO media or user not found").into());
        }
        if matches!(status.as_u16(), 401 | 403) {
            return Err(error("VSCO refused access").into());
        }
        if !status.is_success() {
            return Err(error(format!("VSCO returned HTTP {status}")).into());
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| Failure::transient("could not read VSCO response"))?
        {
            if body.len() + chunk.len() > MAX_RESPONSE {
                return Err(error("VSCO response exceeded 16 MiB").into());
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

    const ID: &str = "5f1b2c3d4e5f6a7b8c9d0e1f";

    fn page(state: Value) -> Vec<u8> {
        format!(
            "<html><script>window.__PRELOADED_STATE__ = {}</script></html>",
            state.to_string().replace("\":null", "\":undefined")
        )
        .into_bytes()
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

    fn paths(media: &Media) -> (String, PathBuf) {
        let (file, dir) = crate::sites::default_templates(media).unwrap();
        (
            file.render(media).unwrap(),
            dir.render(media).unwrap().unwrap(),
        )
    }

    fn camel(id: &str) -> Value {
        json!({"id": id, "responsiveUrl": "im.vsco.co/aws-us-west-2/aa/1/a.jpg",
            "width": 800, "height": 600, "uploadDate": 1704067200000u64,
            "description": "Sunset", "isVideo": false, "gridName": "g1",
            "imageMeta": {"aperture": 2.8}, "tags": [{"text": "sea"}], "extra": null})
    }

    fn media_state() -> Value {
        json!({"medias": {"byId": {ID: {"media": camel(ID)}}}})
    }

    #[test]
    fn builds_upstream_urls_and_keywords() {
        assert_eq!(
            image_url("im.vsco.co/aws-us-west-2/aa/1/a.jpg"),
            "https://image-aws-us-west-2.vsco.co/aa/1/a.jpg"
        );
        assert_eq!(
            image_url("im.vsco.co/1/aa/b.jpg"),
            "https://image.vsco.co/1/aa/b.jpg"
        );
        assert_eq!(
            image_url("https://im.vsco.co/aws-us-west-2/x.jpg"),
            "https://im.vsco.co/aws-us-west-2/x.jpg"
        );
        assert_eq!(image_url("img.vsco.co/x.jpg"), "https://img.vsco.co/x.jpg");
        assert_eq!(ensure_http_scheme("//a.vsco.co/x"), "https://a.vsco.co/x");

        let item = media_item(transform_media(&camel(ID)).unwrap(), "alice", &IMAGE, true).unwrap();
        assert_eq!(
            item.url.as_str(),
            "https://image-aws-us-west-2.vsco.co/aa/1/a.jpg"
        );
        let k = keywords(&item);
        assert_eq!(k["id"], ID);
        assert_eq!((&k["user"], &k["grid"]), (&json!("alice"), &json!("g1")));
        assert_eq!(k["meta"], json!({"aperture": 2.8}));
        assert_eq!(k["tags"], json!(["sea"]));
        assert_eq!(k["date"], "2024-01-01 00:00:00");
        assert_eq!((&k["video"], &k["width"]), (&json!(false), &json!(800)));
        assert_eq!(k["description"], "Sunset");
        assert_eq!(
            (&k["filename"], &k["extension"]),
            (&json!("a"), &json!("jpg"))
        );
        assert_eq!(
            paths(&item),
            (format!("{ID}.jpg"), PathBuf::from("vsco").join("alice"))
        );
        let meta = item.metadata.as_ref().unwrap();
        assert_eq!(meta.media_id, ID);
        assert_eq!(meta.caption.as_deref(), Some("Sunset"));
        assert_eq!(meta.timestamp, Some(1704067200));
        assert!(item.archive_key().unwrap().starts_with("vsco:"));

        // API objects: videos with `playback_url` and HLS manifests.
        let video = json!({"_id": "bbbbbbbbbbbb", "playback_url": "img.vsco.co/v/b/playlist.m3u8",
            "poster_url": "im.vsco.co/1/p.jpg", "created_date": 1704067200000u64,
            "width": 1080, "height": 1920});
        let item = media_item(video.as_object().unwrap().clone(), "alice", &GALLERY, true).unwrap();
        assert_eq!(item.url.as_str(), "https://img.vsco.co/v/b/playlist.m3u8");
        let MediaJob::Manifest {
            protocol, base_url, ..
        } = &item.job
        else {
            panic!("HLS video is not a manifest job");
        };
        assert!(matches!(protocol, ManifestProtocol::Hls));
        assert_eq!(base_url, &item.url);
        let k = keywords(&item);
        assert_eq!(
            (&k["extension"], &k["_ytdl_manifest"], &k["video"]),
            (&json!("mp4"), &json!("hls"), &json!(true))
        );
        assert_eq!((&k["grid"], &k["description"]), (&json!(""), &json!("")));
        assert_eq!(item.filename, "playlist.mp4");
        assert!(media_item(video.as_object().unwrap().clone(), "alice", &GALLERY, false).is_none());
        let mp4 = json!({"_id": "cccccccccccc", "is_video": true, "video_url": "//img.vsco.co/c.mp4",
            "responsive_url": "im.vsco.co/1/p.jpg", "upload_date": 0});
        let item = media_item(mp4.as_object().unwrap().clone(), "alice", &GALLERY, true).unwrap();
        assert!(matches!(item.job, MediaJob::DirectHttp));
        assert_eq!(keywords(&item)["date"], "1970-01-01 00:00:00");
        for bad in [
            json!({"_id": "dddddddddddd", "responsive_url": "evil.example/x.jpg"}),
            json!({"_id": "eeeeeeeeeeee", "responsive_url": "http://im.vsco.co/x.jpg"}),
            json!({"_id": "ffffffffffff"}),
            json!({"_id": "x", "responsive_url": "im.vsco.co/1/x.jpg"}),
        ] {
            assert!(
                media_item(bad.as_object().unwrap().clone(), "a", &GALLERY, true).is_none(),
                "{bad}"
            );
        }
        let body = json!({"media": [
            {"type": "image", "image": {"_id": "aaaaaaaaaaaa", "responsive_url": "img.vsco.co/x/b.png"}},
            {"type": "journal", "journal": {"_id": "ffffffffffff"}}],
            "next_cursor": "abc"});
        let (items, cursor) = parse_profile(body.to_string().as_bytes()).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(cursor.as_deref(), Some("abc"));
        assert!(parse_profile(b"{}").unwrap().0.is_empty());
    }

    #[test]
    fn classifies_urls_and_hosts() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        let user = || "alice".to_owned();
        assert_eq!(
            t("https://vsco.co/Alice").unwrap(),
            Target::User { user: user() }
        );
        assert_eq!(
            t("https://vsco.co/alice/gallery").unwrap(),
            Target::Gallery { user: user() }
        );
        assert_eq!(
            t("https://www.vsco.co/alice/images/1").unwrap(),
            Target::Gallery { user: user() }
        );
        assert_eq!(
            t("https://vsco.co/alice/collection/1").unwrap(),
            Target::Collection { user: user() }
        );
        assert_eq!(
            t("https://vsco.co/alice/spaces").unwrap(),
            Target::Spaces { user: user() }
        );
        assert_eq!(
            t("https://vsco.co/alice/avatar").unwrap(),
            Target::Avatar { user: user() }
        );
        assert_eq!(
            t("https://vsco.co/spaces/A1B2c3d4e5f").unwrap(),
            Target::Space {
                id: "a1b2c3d4e5f".into()
            }
        );
        assert_eq!(
            t(&format!("https://vsco.co/alice/media/{ID}")).unwrap(),
            Target::Media {
                user: user(),
                id: ID.into()
            }
        );
        assert_eq!(
            t("https://vsco.co/alice/video/012345678-9abc-def0").unwrap(),
            Target::Video {
                user: user(),
                id: "012345678-9abc-def0".into()
            }
        );
        for raw in [
            "https://vsco.co/",
            "https://vsco.co/api/3.0/medias",
            "https://vsco.co/spaces",
            "https://vsco.co/alice/media/",
            "https://vsco.co/alice/media/xyz-not-hex",
            "https://vsco.co/alice/journal/x",
            "https://im.vsco.co/aws-us-west-2/a.jpg",
            "https://vsco.co.evil.example/alice",
            "https://evilvsco.co/alice",
            "https://vsco.co:444/alice",
        ] {
            assert!(t(raw).is_err(), "{raw}");
        }
        assert!(media_url("im.vsco.co/a.jpg").is_some());
        assert!(media_url("//img.vsco.co/a.jpg").is_some());
        assert!(media_url("https://vsco.co/a.jpg").is_none());
        assert!(media_url("https://im.vsco.co.evil.example/a.jpg").is_none());
        let descriptor = |raw: &str| {
            crate::source_archive::for_url(&Url::parse(raw).unwrap())
                .unwrap()
                .format
        };
        assert_eq!(
            descriptor("https://vsco.co/a/collection/1"),
            "c_{user}_{id}"
        );
        assert_eq!(descriptor("https://vsco.co/spaces/abc"), "s_{user}_{id}");
        assert_eq!(descriptor("https://vsco.co/a/gallery"), "{id}");
        assert_eq!(include(None).unwrap(), ["gallery"]);
        assert_eq!(
            include(Some(&json!("all"))).unwrap(),
            ["avatar", "gallery", "spaces", "collection"]
        );
        assert_eq!(
            include(Some(&json!("collection, bogus,avatar"))).unwrap(),
            ["collection", "avatar"]
        );
        assert!(include(Some(&json!(1))).is_err());
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

    fn ok(body: &[u8]) -> Vec<u8> {
        response("200 OK", body)
    }

    fn json_ok(body: Value) -> Vec<u8> {
        ok(body.to_string().as_bytes())
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

    fn vsco(root: Url) -> Vsco {
        Vsco::new(Duration::from_secs(5))
            .unwrap()
            .with_network(&NetworkOptions::new(Some(String::new()), None).unwrap())
            .unwrap()
            .with_root(root)
    }

    async fn collect(native: &Vsco, raw: &str) -> Vec<Media> {
        native
            .stream(&Url::parse(raw).unwrap())
            .map(|m| m.unwrap())
            .collect()
            .await
    }

    #[tokio::test]
    async fn fetches_media_with_retry() {
        let (root, server) = serve(vec![
            response("503 Service Unavailable", b""),
            ok(&page(media_state())),
        ]);
        let media = collect(
            &vsco(root).with_retries(1),
            &format!("https://vsco.co/Alice/media/{ID}"),
        )
        .await;
        assert_eq!(media.len(), 1);
        assert_eq!(keywords(&media[0])["subcategory"], "image");
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with(&format!("GET /alice/media/{ID} ")));
        assert!(!requests[1].to_ascii_lowercase().contains("authorization"));
    }

    #[tokio::test]
    async fn video_pages_hand_hls_to_the_manifest_downloader() {
        let state = json!({"medias": {"byId": {"v1": {"media": {"id": "0123456789-abcd",
            "createdDate": 1704067200000u64, "posterUrl": "im.vsco.co/1/p.jpg",
            "playbackUrl": "https://img.vsco.co/v/1/playlist.m3u8", "width": 720,
            "height": 1280, "description": "Clip"}}}}});
        let (root, server) = serve(vec![ok(&page(state))]);
        let media = collect(&vsco(root), "https://vsco.co/alice/video/0123456789-abcd").await;
        assert!(matches!(media[0].job, MediaJob::Manifest { .. }));
        let k = keywords(&media[0]);
        assert_eq!(
            (&k["id"], &k["subcategory"]),
            (&json!("0123456789-abcd"), &json!("video"))
        );
        assert_eq!(k["description"], "Clip");
        assert_eq!(paths(&media[0]).0, "0123456789-abcd.mp4");
        assert!(media[0].archive_key().is_some());
        assert!(server.join().unwrap()[0].starts_with("GET /alice/video/0123456789-abcd "));
    }

    #[tokio::test]
    async fn paginates_gallery_with_bearer_token() {
        let state = json!({"sites": {"siteByUsername": {"alice": {"site": {"id": 42}}}},
            "users": {"currentUser": {"tkn": "secrettoken123"}}});
        let entry = |id: &str| json!({"type": "image", "image": {"_id": id, "responsive_url": "im.vsco.co/1/x.jpg"}});
        let (root, server) = serve(vec![
            ok(&page(state)),
            json_ok(json!({"media": [entry("aaaaaaaaaaaa")], "next_cursor": "c/2"})),
            json_ok(json!({"media": [entry("bbbbbbbbbbbb"),
                {"type": "video", "video": {"_id": "cccccccccccc", "playback_url": "img.vsco.co/v.m3u8",
                    "poster_url": "im.vsco.co/1/p.jpg", "created_date": 5000}}]})),
        ]);
        let native = vsco(root)
            .with_options(Some(json!({"videos": false}).as_object().unwrap()))
            .unwrap();
        let media = collect(&native, "https://vsco.co/alice/gallery").await;
        assert_eq!(media.len(), 2);
        assert_eq!(media[0].url.as_str(), "https://image.vsco.co/1/x.jpg");
        assert_eq!(keywords(&media[1])["subcategory"], "gallery");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /alice/gallery "));
        assert!(requests[1].starts_with("GET /api/3.0/medias/profile?site_id=42&limit=14 "));
        assert!(requests[1].contains("Bearer secrettoken123"));
        assert!(
            requests[1]
                .to_ascii_lowercase()
                .contains("x-client-platform: web")
        );
        assert!(requests[1].contains("/alice\r\n"), "{}", requests[1]);
        assert!(requests[2].contains("&cursor=c%2F2 "), "{}", requests[2]);

        let (root, server) = serve(vec![ok(b"<html>none</html>")]);
        let error = vsco(root)
            .stream(&Url::parse("https://vsco.co/alice/gallery").unwrap())
            .collect::<Vec<_>>()
            .await
            .remove(0)
            .unwrap_err()
            .to_string();
        assert!(error.contains("preloaded state"), "{error}");
        server.join().unwrap();
    }

    #[tokio::test]
    async fn collection_reads_page_state_then_api_pages() {
        let state = json!({"sites": {"siteByUsername": {"alice": {"site": {"id": 42,
                "siteCollectionId": "col1"}}}},
            "users": {"currentUser": {"tkn": "secrettoken123"}},
            "collections": {"byId": {"col1": {"1": {"collection": [{"id": ID}]}}}},
            "medias": {"byId": {ID: {"media": camel(ID)}}}});
        let (root, server) = serve(vec![
            ok(&page(state)),
            json_ok(
                json!({"medias": [{"_id": "aaaaaaaaaaaa", "responsive_url": "im.vsco.co/1/y.jpg",
                "upload_date": 1000, "grid_name": "bob"}]}),
            ),
            json_ok(json!({"medias": []})),
        ]);
        let media = collect(&vsco(root), "https://vsco.co/alice/collection/1").await;
        assert_eq!(media.len(), 2);
        assert_eq!(keywords(&media[0])["id"], ID);
        let k = keywords(&media[1]);
        assert_eq!(
            (&k["grid"], &k["subcategory"]),
            (&json!("bob"), &json!("collection"))
        );
        assert_eq!(
            paths(&media[1]),
            (
                "aaaaaaaaaaaa.jpg".into(),
                PathBuf::from("vsco").join("alice").join("collection")
            )
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /alice/collection/1 "));
        assert!(requests[1].starts_with("GET /api/2.0/collections/col1/medias?page=2&size=20 "));
        assert!(requests[1].contains("Bearer secrettoken123"));
        assert!(requests[2].contains("?page=3&size=20 "));
    }

    #[tokio::test]
    async fn space_follows_post_cursors() {
        let post_image = |id: &str| {
            json!({"id": id, "responsiveUrl": "im.vsco.co/1/s.jpg",
            "uploadDate": {"sec": 1704067200}, "isVideo": false, "gridName": "g"})
        };
        let state = json!({"users": {"currentUser": {"tkn": "secrettoken123"}},
            "entities": {"posts": {"p1": {"id": "p1", "image": "i1"}},
                "postImages": {"i1": post_image("aaaaaaaaaaaa")}},
            "spaces": {"byId": {"sp1": {"postsList": ["p1", "missing"],
                "cursor": {"atEnd": false, "postcursorcontext": {"postId": "p1"}}}}}});
        let (root, server) = serve(vec![
            ok(&page(state)),
            json_ok(json!({"postsList": [{"image": post_image("bbbbbbbbbbbb")}],
                "cursor": {"atEnd": true}})),
        ]);
        let media = collect(&vsco(root), "https://vsco.co/spaces/SP1").await;
        assert_eq!(media.len(), 2);
        let k = keywords(&media[1]);
        assert_eq!(
            (&k["user"], &k["subcategory"]),
            (&json!("sp1"), &json!("space"))
        );
        assert_eq!(k["date"], "2024-01-01 00:00:00");
        assert_eq!(
            paths(&media[0]).1,
            PathBuf::from("vsco").join("space").join("sp1")
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /spaces/sp1 "));
        assert!(requests[1].starts_with("GET /grpc/spaces/sp1/posts?cursor=p1 "));
        assert!(
            requests[1]
                .to_ascii_lowercase()
                .contains("content-type: application/json")
        );
    }

    #[tokio::test]
    async fn spaces_and_profiles_queue_child_jobs() {
        let state = json!({"sites": {"siteByUsername": {"alice": {"site": {"id": 42, "userId": 7}}}},
            "users": {"currentUser": {"tkn": "secrettoken123"}}});
        let (root, server) = serve(vec![
            ok(&page(state)),
            json_ok(
                json!({"spacesWithRoleList": [{"space": {"id": "sp1", "name": "One"}},
                {"space": {"id": "bad id"}}]}),
            ),
        ]);
        let native = vsco(root);
        let mut events = native.events(&Url::parse("https://vsco.co/alice/spaces").unwrap());
        let mut children = 0;
        while let Some(event) = events.next().await {
            if let ExtractionEvent::BeginChild(scope) = event.unwrap() {
                scope.control.cancel();
                children += 1;
            }
        }
        assert_eq!(children, 1);
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /grpc/spaces/user/7 "));
        // Profiles dispatch to the `include` extractors without requests.
        let native = vsco(Url::parse("http://127.0.0.1:9/").unwrap())
            .with_options(Some(json!({"include": "all"}).as_object().unwrap()))
            .unwrap();
        let mut events = native.events(&Url::parse("https://vsco.co/alice").unwrap());
        let mut children = 0;
        while let Some(event) = events.next().await {
            if let ExtractionEvent::BeginChild(scope) = event.unwrap() {
                scope.control.cancel();
                children += 1;
            }
        }
        assert_eq!(children, 4);
    }

    #[tokio::test]
    async fn avatar_reads_the_profile_image_redirect() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let root = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let server = thread::spawn(move || {
            let replies = [
                ok(br#"<script>{"profileImageId":"abcdef0123456789"}</script>"#),
                b"HTTP/1.1 302 Found\r\nLocation: https://im.vsco.co/aws-us-west-2/p/avatar.jpg\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
            ];
            let mut requests = Vec::new();
            for reply in replies {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buffer = [0; 2048];
                let count = stream.read(&mut buffer).unwrap();
                requests.push(String::from_utf8_lossy(&buffer[..count]).into_owned());
                let _ = stream.write_all(&reply);
            }
            requests
        });
        let media = collect(&vsco(root), "https://vsco.co/alice/avatar").await;
        assert_eq!(
            media[0].url.as_str(),
            "https://im.vsco.co/aws-us-west-2/p/avatar.jpg"
        );
        let k = keywords(&media[0]);
        assert_eq!(
            (&k["id"], &k["subcategory"]),
            (&json!("abcdef0123456789"), &json!("avatar"))
        );
        assert_eq!(k["date"], "1970-01-01 00:00:00");
        assert_eq!(paths(&media[0]).0, "abcdef0123456789.jpg");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /alice/gallery "));
        assert!(requests[1].starts_with("GET /im/abcdef0123456789 "));
    }
}
