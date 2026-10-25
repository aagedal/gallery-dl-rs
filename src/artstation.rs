//! Experimental native ArtStation extraction.
//!
//! Mirrors gallery_dl/extractor/artstation.py using the site's JSON
//! endpoints: projects (`projects/HASH.json`), user portfolios, albums,
//! likes, collections and collection lists, challenges, searches, artwork
//! listings and followed users. Listings paginate lazily by `total_count`
//! (capped at 200 pages) and fetch each project as it is reached. Files carry
//! upstream's keyword dictionaries and default formats. Images are upgraded
//! to the `8k` size with `4k`/`large`/`medium`/`small` download fallbacks;
//! on-site video clips and Marmoset viewers are read from their embed pages
//! and external embeds are handed to yt-dlp (`ytdl:`) when `external` is
//! enabled. ArtStation sits behind Cloudflare, so requests carry browser-like
//! headers and may still be blocked. Media must be HTTPS on
//! `cdn*.artstation.com`.
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
use reqwest::{Client, Method, StatusCode};
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, SystemTime},
};
use url::Url;

/// Browser-like default; ArtStation's Cloudflare front rejects many tool agents.
const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:128.0) Gecko/20100101 Firefox/128.0";
const BROWSER_HEADERS: [(&str, &str); 4] = [
    ("Accept", "application/json, text/plain, */*"),
    ("Accept-Language", "en-US,en;q=0.5"),
    ("Referer", "https://www.artstation.com/"),
    ("Cache-Control", "max-age=0"),
];
const ROOT: &str = "https://www.artstation.com";
const MAX_RESPONSE: usize = 16 * 1024 * 1024;
const MAX_PAGES: u64 = 200;
const MAX_QUERY_PARAMS: usize = 32;
/// First path segments on www.artstation.com that are not user portfolios.
const RESERVED: [&str; 22] = [
    "artwork",
    "artists",
    "users",
    "projects",
    "search",
    "learning",
    "marketplace",
    "blogs",
    "jobs",
    "prints",
    "contests",
    "challenges",
    "about",
    "channels",
    "studios",
    "schools",
    "community",
    "guides",
    "magazine",
    "api",
    "sign_in",
    "sign_up",
];
/// Subdomains that are not portfolio hosts.
const RESERVED_HOSTS: [&str; 4] = ["www", "magazine", "api", "help"];

const FILENAME: &str = "{category}_{id}_{asset[id]}_{title}.{extension}";
const USER_DIRECTORY: &[&str] = &["{category}", "{userinfo[username]}"];
/// Upstream extractor classes supported natively, for `--list-extractors`.
pub const SUBCATEGORIES: &[&str] = &[
    "album",
    "artwork",
    "challenge",
    "collection",
    "collections",
    "following",
    "image",
    "likes",
    "search",
    "user",
];

pub static USER: Route = Route::new("user", FILENAME, USER_DIRECTORY, "{asset[id]}");
pub static ALBUM: Route = Route::new(
    "album",
    FILENAME,
    &[
        "{category}",
        "{userinfo[username]}",
        "Albums",
        "{album[id]} - {album[title]}",
    ],
    "a_{album[id]}_{asset[id]}",
);
pub static LIKES: Route = Route::new(
    "likes",
    FILENAME,
    &["{category}", "{userinfo[username]}", "Likes"],
    "f_{userinfo[id]}_{asset[id]}",
);
pub static COLLECTION: Route = Route::new(
    "collection",
    FILENAME,
    &[
        "{category}",
        "{user}",
        "{collection[id]} {collection[name]}",
    ],
    "c_{collection[id]}_{asset[id]}",
);
pub static COLLECTIONS: Route = Route::new("collections", FILENAME, USER_DIRECTORY, "{asset[id]}");
pub static CHALLENGE: Route = Route::new(
    "challenge",
    "{submission_id}_{asset_id}_{filename}.{extension}",
    &[
        "{category}",
        "Challenges",
        "{challenge[id]} - {challenge[title]}",
    ],
    "c_{challenge[id]}_{asset_id}",
);
pub static SEARCH: Route = Route::new(
    "search",
    FILENAME,
    &["{category}", "Searches", "{search[query]}"],
    "s_{search[query]}_{asset[id]}",
);
pub static ARTWORK: Route = Route::new(
    "artwork",
    FILENAME,
    &["{category}", "Artworks", "{artwork[sorting]!c}"],
    "A_{asset[id]}",
);
pub static IMAGE: Route = Route::new("image", FILENAME, USER_DIRECTORY, "{asset[id]}");
pub static FOLLOWING: Route = Route::new("following", FILENAME, USER_DIRECTORY, "{asset[id]}");

/// Project hash IDs such as `LQVJr`.
pub fn valid_hash(hash: &str) -> bool {
    (1..=16).contains(&hash.len()) && hash.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// Numeric asset IDs.
pub fn valid_id(id: &str) -> bool {
    (1..=20).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_digit())
}

fn valid_user(user: &str) -> bool {
    (1..=64).contains(&user.len())
        && user
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Project {
        hash: String,
    },
    User {
        user: String,
    },
    Album {
        user: String,
        id: String,
    },
    Likes {
        user: String,
    },
    Collection {
        user: String,
        id: String,
    },
    Collections {
        user: String,
    },
    Challenge {
        id: String,
        sorting: String,
    },
    /// Upstream `text.parse_query` pairs: first value per name, in order.
    Search {
        query: Vec<(String, String)>,
    },
    Artwork {
        query: Vec<(String, String)>,
    },
    Following {
        user: String,
    },
}

impl Target {
    pub fn route(&self) -> &'static Route {
        match self {
            Self::Project { .. } => &IMAGE,
            Self::User { .. } => &USER,
            Self::Album { .. } => &ALBUM,
            Self::Likes { .. } => &LIKES,
            Self::Collection { .. } => &COLLECTION,
            Self::Collections { .. } => &COLLECTIONS,
            Self::Challenge { .. } => &CHALLENGE,
            Self::Search { .. } => &SEARCH,
            Self::Artwork { .. } => &ARTWORK,
            Self::Following { .. } => &FOLLOWING,
        }
    }
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
            "unsupported ArtStation URL; expected https://www.artstation.com/artwork/HASH, https://www.artstation.com/USER[/albums/ID|/likes|/collections[/ID]|/following], https://USER.artstation.com, a challenge, /search?QUERY or /artwork?QUERY",
        )
    };
    if !plain(url) {
        return Err(unsupported());
    }
    let parts: Vec<&str> = url
        .path_segments()
        .map(|s| s.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    let host = url.host_str().ok_or_else(unsupported)?;
    let query = || {
        let raw = url
            .query()
            .filter(|q| !q.is_empty())
            .ok_or_else(unsupported)?;
        let pairs = parse_query(raw);
        (pairs.len() <= MAX_QUERY_PARAMS)
            .then_some(pairs)
            .ok_or_else(unsupported)
    };
    if host == "artstn.co" {
        return match parts[..] {
            ["p", hash] if valid_hash(hash) => Ok(Target::Project { hash: hash.into() }),
            _ => Err(unsupported()),
        };
    }
    let www = matches!(host, "www.artstation.com" | "artstation.com");
    let label = host
        .strip_suffix(".artstation.com")
        .filter(|u| valid_user(u) && !u.starts_with("cdn"));
    let subdomain = label.filter(|u| !RESERVED_HOSTS.contains(u));
    if !www && label.is_none() {
        return Err(unsupported());
    }
    // Classes matching any ArtStation host.
    match parts[..] {
        ["artwork" | "projects" | "search", hash, ..] if valid_hash(hash) => {
            return Ok(Target::Project { hash: hash.into() });
        }
        ["search"] if url.query().is_some() => return Ok(Target::Search { query: query()? }),
        ["artwork"] if url.query().is_some() => return Ok(Target::Artwork { query: query()? }),
        _ => {}
    }
    if !www && subdomain.is_none() {
        return Err(unsupported());
    }
    if let Some(user) = subdomain {
        let user = user.to_owned();
        return match parts[..] {
            [] | ["projects"] | ["albums", "all"] => Ok(Target::User { user }),
            ["albums", id, ..] if valid_id(id) => Ok(Target::Album {
                user,
                id: id.into(),
            }),
            _ => Err(unsupported()),
        };
    }
    match parts[..] {
        [
            "challenges" | "contests",
            _,
            "categories" | "challenges",
            id,
            ..,
        ] if valid_id(id) => {
            let sorting = url
                .query_pairs()
                .find(|(k, _)| k == "sorting")
                .map(|(_, v)| v.into_owned())
                .filter(|s| {
                    (1..=32).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_lowercase())
                })
                .unwrap_or_else(|| "popular".into());
            Ok(Target::Challenge {
                id: id.into(),
                sorting,
            })
        }
        [user, ref rest @ ..] if valid_user(user) && !RESERVED.contains(&user) => {
            let user = user.to_owned();
            match rest {
                [] | ["albums", "all"] => Ok(Target::User { user }),
                ["albums", id, ..] if valid_id(id) => Ok(Target::Album {
                    user,
                    id: (*id).into(),
                }),
                ["likes", ..] => Ok(Target::Likes { user }),
                ["collections"] => Ok(Target::Collections { user }),
                ["collections", id, ..] if valid_id(id) => Ok(Target::Collection {
                    user,
                    id: (*id).into(),
                }),
                ["following", ..] => Ok(Target::Following { user }),
                _ => Err(unsupported()),
            }
        }
        _ => Err(unsupported()),
    }
}

/// Whether a URL is an ArtStation page handled by this extractor.
pub fn is_artstation_url(url: &Url) -> bool {
    target(url).is_ok()
}

/// HTTPS media on `cdn*.artstation.com`.
pub fn media_url(raw: &str) -> Option<Url> {
    let url = Url::parse(raw.trim()).ok()?;
    let label = url.host_str()?.strip_suffix(".artstation.com")?;
    (url.scheme() == "https"
        && plain(&url)
        && label.starts_with("cdn")
        && label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-'))
    .then_some(url)
}

fn text(value: &Value, key: &str) -> Option<String> {
    match value.get(key)? {
        Value::String(s) => Some(s.trim().to_owned()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
    .filter(|s| !s.is_empty())
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

fn iso_date(value: Option<&Value>) -> Value {
    value
        .and_then(Value::as_str)
        .and_then(|d| crate::date::parse_date(d).ok())
        .map_or(Value::Null, |ts| {
            crate::filter::source_date_text(Some(ts)).into()
        })
}

/// Upstream's `8k` image upgrade with its fallbacks, for project images.
pub fn upgrade_image(url: &Url) -> (Url, Vec<String>) {
    let raw = url.as_str();
    if raw.contains("/images/images/")
        && let Some((lhs, rhs)) = raw.split_once("/large/")
        && let Ok(upgraded) = Url::parse(&format!("{lhs}/8k/{rhs}"))
    {
        let fallback = ["4k", "large", "medium", "small"]
            .iter()
            .map(|size| format!("{lhs}/{size}/{rhs}"))
            .collect();
        return (upgraded, fallback);
    }
    (url.clone(), Vec::new())
}

/// One project's cleaned keyword data and its assets, as upstream's
/// `get_project_assets` prepares them.
pub struct Project {
    pub data: Map<String, Value>,
    pub assets: Vec<Value>,
}

/// Parse a `projects/HASH.json` response.
pub fn parse_project_json(bytes: &[u8], hash: &str) -> Result<Project> {
    let project: Value =
        serde_json::from_slice(bytes).map_err(|_| error("ArtStation returned invalid JSON"))?;
    if text(&project, "hash_id").as_deref() != Some(hash) {
        return Err(error("ArtStation project not found"));
    }
    let Value::Object(mut data) = project else {
        return Err(error("ArtStation project not found"));
    };
    let assets = match data.remove("assets") {
        Some(Value::Array(assets)) => assets,
        _ => return Err(error("ArtStation project contains no asset list")),
    };
    let title = data
        .get("title")
        .and_then(Value::as_str)
        .map(crate::sites::unescape)
        .unwrap_or_default();
    data.insert("title".into(), title.into());
    let description = data
        .get("description")
        .and_then(Value::as_str)
        .map(|d| crate::sites::unescape(&crate::sites::remove_html(d)))
        .unwrap_or_default();
    data.insert("description".into(), description.into());
    let date = iso_date(data.get("created_at"));
    data.insert("date".into(), date);
    data.insert("count".into(), assets.len().into());
    Ok(Project { data, assets })
}

/// Parse a `projects/HASH.json` response into its image files with default
/// settings and no shared metadata; embedded players and their preview
/// images are skipped.
pub fn parse_project(bytes: &[u8], hash: &str) -> Result<Vec<Media>> {
    let project = parse_project_json(bytes, hash)?;
    Ok(project
        .assets
        .iter()
        .enumerate()
        .filter(|(_, asset)| !truthy(asset.get("has_embedded_player")))
        .filter_map(|(position, asset)| {
            image_media(&project, asset, position + 1, &Map::new(), &IMAGE, false)
        })
        .collect())
}

/// The keyword dictionary for one asset of a project.
fn asset_fields(
    project: &Project,
    asset: &Value,
    num: usize,
    data: &Map<String, Value>,
) -> Map<String, Value> {
    let mut fields = project.data.clone();
    fields.insert("asset".into(), asset.clone());
    fields.extend(data.clone());
    fields.insert("num".into(), num.into());
    fields
}

/// Build one file of a project asset.
fn project_media(
    project: &Project,
    mut fields: Map<String, Value>,
    url: Url,
    job: MediaJob,
    video: bool,
    route: &'static Route,
    handoff: BTreeMap<String, Value>,
) -> Media {
    let num = fields.get("num").and_then(Value::as_u64).unwrap_or(1) as usize;
    let asset = fields.get("asset").cloned().unwrap_or_default();
    let name_url = match &job {
        MediaJob::VideoPage { page_url } => page_url.clone(),
        _ => url.clone(),
    };
    let (name, mut ext) = crate::sites::nameext(&name_url);
    if let Some(Value::String(forced)) = fields.get("extension") {
        ext = forced.clone();
    }
    fields.insert("filename".into(), name.clone().into());
    fields.insert("extension".into(), ext.clone().into());
    fields.insert("category".into(), "artstation".into());
    fields.insert("subcategory".into(), route.subcategory.into());
    let dimension = |key: &str| asset.get(key).and_then(Value::as_u64).filter(|v| *v > 0);
    Media {
        filename: if ext.is_empty() {
            name
        } else {
            format!("{name}.{ext}")
        },
        job,
        category: "artstation".into(),
        metadata: Some(PostMetadata {
            file: Some(FileMetadata {
                width: dimension("width"),
                height: dimension("height"),
                media_type: Some(Arc::from(if video { "video" } else { "photo" })),
                ..FileMetadata::default()
            }),
            post_id: text(&Value::Object(project.data.clone()), "hash_id").unwrap_or_default(),
            media_id: text(&asset, "id").unwrap_or_default(),
            username: project
                .data
                .get("user")
                .and_then(|u| text(u, "username"))
                .map(Arc::from),
            caption: project
                .data
                .get("title")
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
                .map(Arc::from),
            timestamp: project
                .data
                .get("published_at")
                .and_then(Value::as_str)
                .and_then(|d| crate::date::parse_date(d).ok()),
            index: num,
            site_keywords: Some(Arc::new(SiteKeywords { route, fields })),
            ..PostMetadata::default()
        }),
        url,
        headers: BTreeMap::new(),
        handoff,
    }
}

/// The image file of an asset with `has_image`, upgraded to `8k`.
fn image_media(
    project: &Project,
    asset: &Value,
    num: usize,
    data: &Map<String, Value>,
    route: &'static Route,
    video: bool,
) -> Option<Media> {
    if !truthy(asset.get("has_image")) {
        return None;
    }
    valid_id(&text(asset, "id")?).then_some(())?;
    let original = media_url(&text(asset, "image_url")?)?;
    let (url, fallback) = upgrade_image(&original);
    let mut fields = asset_fields(project, asset, num, data);
    let (name, ext) = crate::sites::nameext(&original);
    fields.insert("filename".into(), name.into());
    fields.insert("extension".into(), ext.into());
    let mut handoff = BTreeMap::new();
    if !fallback.is_empty() {
        handoff.insert("_fallback".into(), json!(fallback));
    }
    let mut item = project_media(
        project,
        fields,
        url,
        MediaJob::DirectHttp,
        video,
        route,
        handoff,
    );
    // Names come from the original `large` URL, as upstream.
    item.filename = format!(
        "{}.{}",
        keyword(&item, "filename"),
        keyword(&item, "extension")
    );
    Some(item)
}

fn keyword(media: &Media, key: &str) -> String {
    media
        .metadata
        .as_ref()
        .and_then(|m| m.site_keywords.as_ref())
        .and_then(|k| k.fields.get(key))
        .map(crate::sites::scalar)
        .unwrap_or_default()
}

/// The `src` of an embedded player.
fn player_src(player: &str) -> Option<&str> {
    crate::sites::extr(player, "src=\"", "\"")
        .or_else(|| crate::sites::extr(player, "src='", "'"))
        .filter(|src| !src.is_empty())
}

/// Parse a `{data, total_count}` listing page.
pub fn parse_listing(bytes: &[u8]) -> Result<(Vec<Value>, u64)> {
    let body: Value =
        serde_json::from_slice(bytes).map_err(|_| error("ArtStation returned invalid JSON"))?;
    let data = body
        .get("data")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| error("ArtStation response contains no list"))?;
    Ok((
        data,
        body.get("total_count").and_then(Value::as_u64).unwrap_or(0),
    ))
}

/// Upstream extractor settings (`extractor.artstation.*`).
#[derive(Clone, Debug)]
pub struct Options {
    pub mviews: bool,
    pub videos: bool,
    pub external: bool,
    pub previews: bool,
    pub max_posts: Option<u64>,
    pub pro_first: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            mviews: true,
            videos: true,
            external: false,
            previews: false,
            max_posts: None,
            pro_first: true,
        }
    }
}

impl Options {
    pub fn parse(map: &Map<String, Value>) -> Result<Self> {
        let boolean = |key: &str, default: bool| -> Result<bool> {
            match map.get(key) {
                None | Some(Value::Null) => Ok(default),
                Some(Value::Bool(value)) => Ok(*value),
                Some(_) => Err(error(format!("ArtStation {key} must be a boolean"))),
            }
        };
        Ok(Self {
            mviews: boolean("mviews", true)?,
            videos: boolean("videos", true)?,
            external: boolean("external", false)?,
            previews: boolean("previews", false)?,
            pro_first: boolean("pro-first", true)?,
            max_posts: match map.get("max-posts") {
                None | Some(Value::Null) => None,
                Some(Value::Number(n)) => n.as_u64().filter(|v| *v > 0),
                Some(_) => return Err(error("ArtStation max-posts must be an integer")),
            },
        })
    }
}

/// HTTP method and body of one request.
#[derive(Clone, Copy)]
enum Body<'a> {
    Get,
    Post(&'a Value, Option<&'a str>),
}

/// A project listing: the endpoint, its base query and shared metadata.
struct Listing {
    route: &'static Route,
    path: String,
    query: Vec<(String, String)>,
    /// Search body for POST pagination.
    search: Option<Value>,
    data: Map<String, Value>,
}

pub struct ArtStation {
    client: Client,
    network: NetworkOptions,
    api_root: Url,
    options: Options,
    timeout: Duration,
    request_timeout: Duration,
    retries: u8,
    rate_limits: RateLimits,
}

impl ArtStation {
    pub fn new(timeout: Duration) -> Result<Self> {
        Ok(Self {
            client: NetworkOptions::default().client(USER_AGENT, timeout)?,
            network: NetworkOptions::default(),
            api_root: Url::parse("https://www.artstation.com/").expect("static URL"),
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

    /// Apply upstream extractor settings (`extractor.artstation.*`).
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

    /// Override the site origin; intended for local tests.
    pub fn with_api_root(mut self, root: Url) -> Self {
        self.api_root = root;
        self
    }

    fn endpoint(&self, path: &str, query: &[(String, String)]) -> Result<Url> {
        let mut endpoint = self
            .api_root
            .join(path)
            .map_err(|_| error("invalid ArtStation path"))?;
        if !query.is_empty() {
            endpoint.query_pairs_mut().extend_pairs(query);
        }
        Ok(endpoint)
    }

    async fn call(
        &self,
        endpoint: &Url,
        body: Body<'_>,
        missing_ok: bool,
    ) -> Result<Option<Vec<u8>>> {
        tokio::time::timeout(self.timeout, self.request(endpoint, body, missing_ok))
            .await
            .map_err(|_| error("native ArtStation extraction timed out"))?
    }

    async fn json(&self, path: &str, query: &[(String, String)]) -> Result<Value> {
        let endpoint = self.endpoint(path, query)?;
        let body = self
            .call(&endpoint, Body::Get, false)
            .await?
            .unwrap_or_default();
        serde_json::from_slice(&body).map_err(|_| error("ArtStation returned invalid JSON"))
    }

    /// Upstream `get_user_info`.
    async fn userinfo(&self, user: &str) -> Result<Value> {
        self.json(
            &format!("users/{}/quick.json", user.to_ascii_lowercase()),
            &[],
        )
        .await
    }

    /// Upstream `get_project_assets`; `None` when the project is missing.
    async fn fetch_project(&self, hash: &str) -> Result<Option<Project>> {
        let endpoint = self.endpoint(&format!("projects/{hash}.json"), &[])?;
        match self.call(&endpoint, Body::Get, true).await? {
            Some(body) => parse_project_json(&body, hash).map(Some),
            None => Ok(None),
        }
    }

    /// Upstream `_extract_embed`: the file of an embedded player, if any.
    async fn embed(
        &self,
        asset: &Value,
        fields: &mut Map<String, Value>,
    ) -> Result<Option<(Url, MediaJob, bool)>> {
        let player = asset
            .get("player_embedded")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let Some(src) = player_src(player) else {
            return Ok(None);
        };
        if let Some(path) = src.strip_prefix(ROOT) {
            // Embed or video clip hosted on ArtStation.
            let image = asset
                .get("image_url")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let kind = crate::sites::extr(image, "/assets/", "/").unwrap_or_default();
            if kind.is_empty() {
                return Ok(None);
            }
            let marmoset = kind == "marmosets";
            if marmoset && !self.options.mviews || !marmoset && !self.options.videos {
                return Ok(None);
            }
            let endpoint = self
                .api_root
                .join(path.trim_start_matches('/'))
                .map_err(|_| error("invalid ArtStation embed URL"))?;
            let Some(page) = self.call(&endpoint, Body::Get, true).await? else {
                return Ok(None);
            };
            let page = String::from_utf8_lossy(&page);
            let found = if marmoset {
                crate::sites::extr(&page, "marmoset.embed(", "\",")
                    .map(|s| s.trim_matches(['"', '\'', ' ']).to_owned())
            } else {
                page.find("id=\"video\"")
                    .and_then(|at| crate::sites::extr(&page[at + 1..], " src=\"", "\""))
                    .map(str::to_owned)
            };
            return Ok(found
                .and_then(|raw| media_url(&raw))
                .map(|url| (url, MediaJob::DirectHttp, !marmoset)));
        }
        if !self.options.external {
            return Ok(None);
        }
        let Some(page_url) = Url::parse(src)
            .ok()
            .filter(|u| matches!(u.scheme(), "http" | "https"))
        else {
            return Ok(None);
        };
        let Ok(url) = Url::parse(&format!("ytdl:{page_url}")) else {
            return Ok(None);
        };
        fields.insert("extension".into(), "mp4".into());
        Ok(Some((url, MediaJob::VideoPage { page_url }, true)))
    }

    /// Upstream `items` for one project: a post per asset with its embed
    /// and image files.
    async fn project_events(
        &self,
        project: Project,
        data: &Map<String, Value>,
        route: &'static Route,
    ) -> Result<Vec<ExtractionEvent>> {
        let count = project.assets.len();
        let mut out = Vec::new();
        for (position, asset) in project.assets.iter().enumerate() {
            let num = position + 1;
            let mut files = Vec::new();
            let mut skip_image = false;
            if truthy(asset.get("has_embedded_player")) {
                let mut fields = asset_fields(&project, asset, num, data);
                if let Some((url, job, video)) = self.embed(asset, &mut fields).await?
                    && text(asset, "id").is_some_and(|id| valid_id(&id))
                {
                    files.push(project_media(
                        &project,
                        fields,
                        url,
                        job,
                        video,
                        route,
                        BTreeMap::new(),
                    ));
                }
                skip_image = !self.options.previews;
            }
            if !skip_image && let Some(item) = image_media(&project, asset, num, data, route, false)
            {
                files.push(item);
            }
            if files.is_empty() {
                continue;
            }
            for item in &mut files {
                self.network.apply_media(item);
            }
            // Upstream emits Directory for each asset, even within one
            // project. Each asset's Url then reuses that Directory dictionary.
            let policy = KeywordPolicy::shared(&[
                "num",
                "index",
                "media_id",
                "filename",
                "name",
                "extension",
            ])
            .with_post_fields(&files[0], &["num", "index"]);
            let mut events = group_with_keywords(files, "artstation", policy);
            if let Some(ExtractionEvent::Post(post)) = events.front_mut() {
                post.count = Some(count);
            }
            out.extend(events);
        }
        Ok(out)
    }

    /// Upstream `ArtstationImageExtractor`.
    async fn image(&self, hash: &str) -> Result<Events<'_>> {
        let project = self
            .fetch_project(hash)
            .await?
            .ok_or_else(|| error("ArtStation project or user not found"))?;
        let user = project
            .data
            .get("user")
            .and_then(|u| text(u, "username"))
            .filter(|u| valid_user(u))
            .ok_or_else(|| error("ArtStation project has no valid user"))?;
        let mut data = Map::new();
        data.insert("userinfo".into(), self.userinfo(&user).await?);
        Ok(crate::sites::events(
            self.project_events(project, &data, &IMAGE).await?,
        ))
    }

    /// Resolve a project listing's metadata and endpoint.
    async fn listing(&self, target: Target) -> Result<Listing> {
        let route = target.route();
        let mut data = Map::new();
        let pair = |k: &str, v: &str| (k.to_owned(), v.to_owned());
        let (path, query, search) = match target {
            Target::User { user } => {
                data.insert("userinfo".into(), self.userinfo(&user).await?);
                (
                    format!("users/{user}/projects.json"),
                    vec![pair("album_id", "all")],
                    None,
                )
            }
            Target::Album { user, id } => {
                let userinfo = self.userinfo(&user).await?;
                let album = userinfo
                    .get("albums_with_community_projects")
                    .and_then(Value::as_array)
                    .and_then(|albums| {
                        albums
                            .iter()
                            .find(|a| text(a, "id").as_deref() == Some(id.as_str()))
                    })
                    .cloned()
                    .ok_or_else(|| error("ArtStation album not found"))?;
                data.insert("userinfo".into(), userinfo);
                data.insert("album".into(), album);
                (
                    format!("users/{user}/projects.json"),
                    vec![pair("album_id", &id)],
                    None,
                )
            }
            Target::Likes { user } => {
                data.insert("userinfo".into(), self.userinfo(&user).await?);
                (format!("users/{user}/likes.json"), Vec::new(), None)
            }
            Target::Collection { user, id } => {
                let collection = self
                    .json(
                        &format!("collections/{id}.json"),
                        &[pair("username", &user)],
                    )
                    .await?;
                data.insert("collection".into(), collection);
                data.insert("user".into(), user.into());
                (
                    format!("collections/{id}/projects.json"),
                    vec![pair("collection_id", &id)],
                    None,
                )
            }
            Target::Artwork { query } => {
                let artwork: Map<String, Value> = query
                    .iter()
                    .map(|(k, v)| (k.clone(), Value::from(v.as_str())))
                    .collect();
                data.insert("artwork".into(), artwork.into());
                let query = query.into_iter().filter(|(k, _)| k != "page").collect();
                ("projects.json".into(), query, None)
            }
            Target::Search { query } => {
                let get = |key: &str| query.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
                let text_query = get("query")
                    .filter(|q| !q.is_empty())
                    .or_else(|| get("q"))
                    .unwrap_or_default();
                let sorting = get("sort_by")
                    .unwrap_or_else(|| "relevance".into())
                    .to_lowercase();
                let tags: Vec<Value> = get("tags")
                    .unwrap_or_default()
                    .split(',')
                    .map(Value::from)
                    .collect();
                data.insert(
                    "search".into(),
                    json!({"query": text_query, "sorting": sorting, "tags": tags}),
                );
                let filters: Vec<Value> = query
                    .iter()
                    .filter(|(k, _)| k.ends_with("_ids") || k == "tags")
                    .map(|(k, v)| {
                        json!({"field": k, "method": "include",
                            "value": v.split(',').collect::<Vec<_>>()})
                    })
                    .collect();
                let body = json!({
                    "query": text_query,
                    "page": null,
                    "per_page": 50,
                    "sorting": sorting,
                    "pro_first": if self.options.pro_first { "1" } else { "0" },
                    "filters": filters,
                    "additional_fields": [],
                });
                ("api/v2/search/projects.json".into(), Vec::new(), Some(body))
            }
            _ => unreachable!("not a project listing"),
        };
        Ok(Listing {
            route,
            path,
            query,
            search,
            data,
        })
    }

    /// Upstream `_init_csrf_token`.
    async fn csrf_token(&self) -> Result<String> {
        let endpoint = self.endpoint("api/v2/csrf_protection/token.json", &[])?;
        let body = self
            .call(&endpoint, Body::Post(&json!({}), None), false)
            .await?
            .unwrap_or_default();
        let value: Value =
            serde_json::from_slice(&body).map_err(|_| error("ArtStation returned invalid JSON"))?;
        value
            .get("public_csrf_token")
            .and_then(Value::as_str)
            .filter(|t| {
                (1..=512).contains(&t.len())
                    && t.bytes()
                        .all(|b| b.is_ascii_graphic() && !matches!(b, b'"' | b'\\'))
            })
            .map(str::to_owned)
            .ok_or_else(|| error("ArtStation returned no CSRF token"))
    }

    /// Upstream `_pagination` page `page`: items and `total_count`.
    async fn page(
        &self,
        path: &str,
        query: &[(String, String)],
        search: Option<(&Value, &str)>,
        page: u64,
    ) -> Result<(Vec<Value>, u64)> {
        let body = match search {
            Some((body, token)) => {
                let mut body = body.clone();
                body["page"] = page.into();
                let endpoint = self.endpoint(path, &[])?;
                self.call(&endpoint, Body::Post(&body, Some(token)), false)
                    .await?
            }
            None => {
                let mut query = query.to_vec();
                query.push(("page".into(), page.to_string()));
                let endpoint = self.endpoint(path, &query)?;
                self.call(&endpoint, Body::Get, false).await?
            }
        };
        parse_listing(&body.unwrap_or_default())
    }

    /// Lazily paginate a project listing, fetching each project as reached.
    fn projects(&self, target: Target) -> Events<'_> {
        crate::sites::once(async move {
            let listing = Arc::new(self.listing(target).await?);
            let token = match listing.search {
                Some(_) => Some(self.csrf_token().await?),
                None => None,
            };
            let max = self.options.max_posts.unwrap_or(u64::MAX);
            Ok(crate::sites::paginate(
                Some((1u64, 0u64, 0u64)),
                move |state| {
                    let listing = listing.clone();
                    let token = token.clone();
                    async move {
                        let Some((page, total, taken)) = state else {
                            return Ok(None);
                        };
                        if taken >= max {
                            return Ok(None);
                        }
                        let search = listing.search.as_ref().zip(token.as_deref());
                        let (items, count) = self
                            .page(&listing.path, &listing.query, search, page)
                            .await?;
                        let total = total + items.len() as u64;
                        let hashes: Vec<String> = items
                            .iter()
                            .filter_map(|p| text(p, "hash_id").filter(|h| valid_hash(h)))
                            .take(usize::try_from(max - taken).unwrap_or(usize::MAX))
                            .collect();
                        let taken = taken + hashes.len() as u64;
                        let more = !items.is_empty() && total < count && page < MAX_PAGES;
                        let events = stream::iter(hashes)
                            .map(move |hash| {
                                let listing = listing.clone();
                                crate::sites::once(async move {
                                    let Some(project) = self.fetch_project(&hash).await? else {
                                        return Ok(crate::sites::events(Vec::new()));
                                    };
                                    Ok(crate::sites::events(
                                        self.project_events(project, &listing.data, listing.route)
                                            .await?,
                                    ))
                                })
                            })
                            .flatten()
                            .boxed();
                        Ok(Some((events, more.then_some((page + 1, total, taken)))))
                    }
                },
            ))
        })
    }

    /// Upstream `ArtstationCollectionsExtractor`: queue each collection.
    async fn collections(&self, user: String) -> Result<Events<'_>> {
        let list = self
            .json("collections.json", &[("username".into(), user.clone())])
            .await?;
        let children: Vec<Events<'_>> = list
            .as_array()
            .map(|list| {
                list.iter()
                    .filter_map(|collection| {
                        let id = text(collection, "id").filter(|id| valid_id(id))?;
                        let url = Url::parse(&format!("{ROOT}/{user}/collections/{id}")).ok()?;
                        Some(child_job(
                            ChildMetadata::from_raw(
                                "artstation",
                                "collections",
                                collection.as_object()?,
                            ),
                            move || self.events(&url),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(stream::iter(children).flatten().boxed())
    }

    /// Upstream `ArtstationFollowingExtractor`: queue each followed user.
    fn following(&self, user: String) -> Events<'_> {
        crate::sites::paginate(Some((1u64, 0u64)), move |state| {
            let user = user.clone();
            async move {
                let Some((page, total)) = state else {
                    return Ok(None);
                };
                let (items, count) = self
                    .page(&format!("users/{user}/following.json"), &[], None, page)
                    .await?;
                let total = total + items.len() as u64;
                let more = !items.is_empty() && total < count && page < MAX_PAGES;
                let children: Vec<Events<'_>> = items
                    .iter()
                    .filter_map(|followed| {
                        let name = text(followed, "username").filter(|u| valid_user(u))?;
                        let url = Url::parse(&format!("{ROOT}/{name}")).ok()?;
                        Some(child_job(
                            ChildMetadata::from_raw(
                                "artstation",
                                "following",
                                followed.as_object()?,
                            ),
                            move || self.events(&url),
                        ))
                    })
                    .collect();
                Ok(Some((
                    stream::iter(children).flatten().boxed(),
                    more.then_some((page + 1, total)),
                )))
            }
        })
    }

    /// Upstream `ArtstationChallengeExtractor`: every image linked from the
    /// submission updates of a challenge.
    async fn challenge(&self, id: String, sorting: String) -> Result<Events<'_>> {
        let base = "api/v2/competition/";
        let challenge = Arc::new(
            self.json(&format!("{base}challenges/{id}.json"), &[])
                .await?,
        );
        let query = vec![
            ("per_page".to_owned(), "50".to_owned()),
            ("challenge_id".to_owned(), id.clone()),
            ("sort_by".to_owned(), sorting),
        ];
        Ok(crate::sites::paginate(Some((1u64, 0u64)), move |state| {
            let (query, challenge, id) = (query.clone(), challenge.clone(), id.clone());
            async move {
                let Some((page, total)) = state else {
                    return Ok(None);
                };
                let (submissions, count) = self
                    .page(&format!("{base}submissions.json"), &query, None, page)
                    .await?;
                let total = total + submissions.len() as u64;
                let more = !submissions.is_empty() && total < count && page < MAX_PAGES;
                let events = stream::iter(
                    submissions
                        .iter()
                        .filter_map(|s| text(s, "id").filter(|i| valid_id(i)))
                        .collect::<Vec<_>>(),
                )
                .map(move |submission| self.submission(submission, challenge.clone(), id.clone()))
                .flatten()
                .boxed();
                Ok(Some((events, more.then_some((page + 1, total)))))
            }
        }))
    }

    /// The update pages of one challenge submission.
    fn submission(&self, submission: String, challenge: Arc<Value>, id: String) -> Events<'_> {
        let path = format!("api/v2/competition/submissions/{submission}/submission_updates.json");
        let query = vec![("per_page".to_owned(), "50".to_owned())];
        crate::sites::paginate(Some((1u64, 0u64)), move |state| {
            let (path, query, challenge, id) =
                (path.clone(), query.clone(), challenge.clone(), id.clone());
            async move {
                let Some((page, total)) = state else {
                    return Ok(None);
                };
                let (updates, count) = self.page(&path, &query, None, page).await?;
                let total = total + updates.len() as u64;
                let more = !updates.is_empty() && total < count && page < MAX_PAGES;
                let mut events = Vec::new();
                for update in updates {
                    let items = challenge_media(&update, &challenge, &id);
                    let mut items: Vec<Media> = items;
                    for item in &mut items {
                        self.network.apply_media(item);
                    }
                    if !items.is_empty() {
                        events.extend(group_with_keywords(
                            items,
                            "artstation",
                            KeywordPolicy::default(),
                        ));
                    }
                }
                Ok(Some((
                    crate::sites::events(events),
                    more.then_some((page + 1, total)),
                )))
            }
        })
    }

    pub fn stream(&self, url: &Url) -> BoxStream<'_, Result<Media>> {
        crate::event::media_only(self.events(url))
    }

    /// Emit one post boundary per asset; listings paginate lazily and queue
    /// collections and followed users as child jobs.
    pub fn events(&self, url: &Url) -> BoxStream<'_, Result<ExtractionEvent>> {
        let target = match target(url) {
            Ok(target) => target,
            Err(e) => return crate::sites::fail(e),
        };
        match target {
            Target::Project { hash } => crate::sites::once(async move { self.image(&hash).await }),
            Target::Collections { user } => crate::sites::once(self.collections(user)),
            Target::Following { user } => self.following(user),
            Target::Challenge { id, sorting } => crate::sites::once(self.challenge(id, sorting)),
            target => self.projects(target),
        }
    }

    async fn request(
        &self,
        endpoint: &Url,
        body: Body<'_>,
        missing_ok: bool,
    ) -> Result<Option<Vec<u8>>> {
        for attempt in 0..=self.retries {
            match self.attempt(endpoint, body, attempt, missing_ok).await {
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
        body: Body<'_>,
        attempt: u8,
        missing_ok: bool,
    ) -> std::result::Result<Option<Vec<u8>>, Failure> {
        let mut target = endpoint.clone();
        let mut redirects = Redirects::new(endpoint);
        let mut response = loop {
            self.rate_limits.wait(&target, self.timeout).await?;
            let method = match body {
                Body::Get => Method::GET,
                Body::Post(..) => Method::POST,
            };
            let mut request = self
                .client
                .request(method, target.clone())
                .timeout(self.request_timeout);
            for (name, value) in BROWSER_HEADERS {
                request = request.header(name, value);
            }
            request = request.header("Origin", ROOT);
            if let Body::Post(json, token) = body {
                request = request
                    .header("Content-Type", "application/json")
                    .body(json.to_string());
                // Only send the CSRF token to the origin it came from.
                if let Some(token) = token.filter(|_| target.origin() == endpoint.origin()) {
                    request = request.header("PUBLIC-CSRF-TOKEN", token);
                }
            }
            let response = request.send().await.map_err(|_| {
                Failure::transient("ArtStation request failed; check network access")
            })?;
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
                        Some(delay) => format!(
                            "ArtStation rate limit; retry after {} seconds",
                            delay.as_secs()
                        ),
                        None => "ArtStation rate limit; retry later".into(),
                    }
                } else {
                    format!("ArtStation returned HTTP {status}")
                },
            ));
        }
        if matches!(status.as_u16(), 404 | 410) {
            if missing_ok {
                return Ok(None);
            }
            return Err(error("ArtStation project or user not found").into());
        }
        if matches!(status.as_u16(), 401 | 403) {
            return Err(
                error("ArtStation refused access (requests may be blocked by Cloudflare)").into(),
            );
        }
        if !status.is_success() {
            return Err(error(format!("ArtStation returned HTTP {status}")).into());
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| Failure::transient("could not read ArtStation response"))?
        {
            if body.len() + chunk.len() > MAX_RESPONSE {
                return Err(error("ArtStation response exceeded 16 MiB").into());
            }
            body.extend_from_slice(&chunk);
        }
        Ok(Some(body))
    }
}

/// Upstream `_id_from_url`: the joined 8th-10th path components of an
/// image URL as an integer.
fn challenge_asset_id(url: &str) -> u64 {
    let joined: String = url.split('/').skip(7).take(3).collect();
    joined.parse().unwrap_or(0)
}

/// The image files linked from one challenge submission update.
pub fn challenge_media(update: &Value, challenge: &Value, challenge_id: &str) -> Vec<Media> {
    let Some(mut base) = update.as_object().cloned() else {
        return Vec::new();
    };
    base.insert("challenge".into(), challenge.clone());
    let body = update.get("body").and_then(Value::as_str).unwrap_or("");
    let mut seen: Vec<&str> = Vec::new();
    let mut rest = body;
    while let Some(start) = rest.find(" href=\"") {
        rest = &rest[start + 7..];
        let Some(end) = rest.find('"') else { break };
        let href = &rest[..end];
        rest = &rest[end..];
        // `util.unique_sequence` drops adjacent repeats only.
        if seen.last() != Some(&href) {
            seen.push(href);
        }
    }
    seen.into_iter()
        .filter_map(|href| {
            let url = media_url(&crate::sites::unescape(href))?;
            let asset_id = challenge_asset_id(url.as_str());
            let (name, ext) = crate::sites::nameext(&url);
            let mut fields = base.clone();
            fields.insert("asset_id".into(), asset_id.into());
            fields.insert("filename".into(), name.clone().into());
            fields.insert("extension".into(), ext.clone().into());
            fields.insert("category".into(), "artstation".into());
            fields.insert("subcategory".into(), "challenge".into());
            Some(Media {
                filename: format!("{name}.{ext}"),
                job: MediaJob::DirectHttp,
                category: "artstation".into(),
                metadata: Some(PostMetadata {
                    file: Some(FileMetadata {
                        media_type: Some(Arc::from("photo")),
                        ..FileMetadata::default()
                    }),
                    post_id: challenge_id.into(),
                    media_id: asset_id.to_string(),
                    username: update
                        .get("user")
                        .and_then(|u| text(u, "username"))
                        .map(Arc::from),
                    timestamp: update
                        .get("created_at")
                        .and_then(Value::as_str)
                        .and_then(|d| crate::date::parse_date(d).ok()),
                    index: 1,
                    site_keywords: Some(Arc::new(SiteKeywords {
                        route: &CHALLENGE,
                        fields,
                    })),
                    ..PostMetadata::default()
                }),
                url,
                headers: BTreeMap::new(),
                handoff: BTreeMap::new(),
            })
        })
        .collect()
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

    const IMAGE_12: &str =
        "https://cdnb.artstation.com/p/assets/images/images/000/012/large/a.png?1700";

    fn project(hash: &str) -> Value {
        json!({"id": 90, "hash_id": hash, "title": "Ship &amp; Sea",
            "description": "<p>Hello</p> <b>world</b>",
            "created_at": "2024-01-02T03:04:05.000-00:00",
            "published_at": "2024-01-01T00:00:00.000-00:00",
            "user": {"username": "alice"}, "assets": [
                {"id": 11, "asset_type": "cover", "has_image": true, "has_embedded_player": false,
                 "image_url": "https://cdna.artstation.com/p/assets/covers/1/large/c.jpg"},
                {"id": 12, "asset_type": "image", "has_image": true, "width": 1920, "height": 1080,
                 "has_embedded_player": false, "image_url": IMAGE_12},
                {"id": 13, "asset_type": "video", "has_image": true, "has_embedded_player": true,
                 "image_url": "https://cdna.artstation.com/p/assets/covers/13/large/v.jpg",
                 "player_embedded": "<iframe src='https://www.youtube.com/embed/xyz'></iframe>"},
                {"id": 14, "asset_type": "image", "has_image": true,
                 "image_url": "http://cdnb.artstation.com/p/assets/images/images/14/large/b.jpg"},
                {"id": 15, "asset_type": "image", "has_image": true,
                 "image_url": "https://cdn.artstation.com.evil.example/b.jpg"},
                {"id": 16, "asset_type": "video_clip", "has_image": true, "has_embedded_player": true,
                 "image_url": "https://cdna.artstation.com/p/assets/video_clips/16/large/p.jpg",
                 "player_embedded": "<iframe src=\"https://www.artstation.com/api/v2/animation/video_clips/16/embed.html\"></iframe>"},
                {"id": 17, "asset_type": "marmoset", "has_image": true, "has_embedded_player": true,
                 "image_url": "https://cdna.artstation.com/p/assets/marmosets/17/large/m.jpg",
                 "player_embedded": "<iframe src=\"https://www.artstation.com/embed/17\"></iframe>"}]})
    }

    fn userinfo() -> Value {
        json!({"id": 7, "username": "alice", "full_name": "Alice",
            "albums_with_community_projects": [{"id": 5, "title": "Trip"}]})
    }

    const VIDEO_PAGE: &str = r#"<html><video id="video"><source src="https://cdna.artstation.com/p/video_clips/16/clip.mp4"></video></html>"#;
    const MARMOSET_PAGE: &str = r#"<script>marmoset.embed("https://cdna.artstation.com/p/marmosets/17/scene.mview", {width: 1});</script>"#;

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

    #[test]
    fn parses_project_assets_like_upstream() {
        let body = project("LQVJr").to_string();
        let items = parse_project(body.as_bytes(), "LQVJr").unwrap();
        let ids: Vec<_> = items
            .iter()
            .map(|m| m.metadata.as_ref().unwrap().media_id.as_str())
            .collect();
        assert_eq!(ids, ["11", "12"]);
        let item = &items[1];
        assert_eq!(
            item.url.as_str(),
            "https://cdnb.artstation.com/p/assets/images/images/000/012/8k/a.png?1700"
        );
        assert_eq!(
            item.handoff["_fallback"],
            json!([
                "https://cdnb.artstation.com/p/assets/images/images/000/012/4k/a.png?1700",
                IMAGE_12,
                "https://cdnb.artstation.com/p/assets/images/images/000/012/medium/a.png?1700",
                "https://cdnb.artstation.com/p/assets/images/images/000/012/small/a.png?1700"
            ])
        );
        // Covers are not upgraded.
        assert!(items[0].url.as_str().ends_with("/covers/1/large/c.jpg"));
        assert!(!items[0].handoff.contains_key("_fallback"));
        assert_eq!(item.filename, "a.png");
        let k = keywords(item);
        assert_eq!(k["title"], "Ship & Sea");
        assert_eq!(k["description"], "Hello world");
        assert_eq!(k["date"], "2024-01-02 03:04:05");
        assert_eq!((&k["num"], &k["count"]), (&json!(2), &json!(7)));
        assert_eq!(k["asset"]["id"], 12);
        assert!(!k.contains_key("assets"));
        assert_eq!(
            (&k["filename"], &k["extension"]),
            (&json!("a"), &json!("png"))
        );
        let meta = item.metadata.as_ref().unwrap();
        assert_eq!(
            (meta.post_id.as_str(), meta.media_id.as_str()),
            ("LQVJr", "12")
        );
        assert_eq!(meta.username.as_deref(), Some("alice"));
        assert_eq!(meta.caption.as_deref(), Some("Ship & Sea"));
        assert_eq!((meta.timestamp, meta.index), (Some(1704067200), 2));
        let file = meta.file.as_ref().unwrap();
        assert_eq!((file.width, file.height), (Some(1920), Some(1080)));
        let key = item.archive_key().unwrap();
        assert!(key.starts_with("artstation:"), "{key}");
        assert_ne!(items[0].archive_key(), Some(key));
        assert!(parse_project(body.as_bytes(), "other").is_err());
        assert!(parse_project(b"<html>", "LQVJr").is_err());
        let listing = json!({"data": [{"hash_id": "a"}], "total_count": 3});
        let (items, total) = parse_listing(listing.to_string().as_bytes()).unwrap();
        assert_eq!((items.len(), total), (1, 3));
        let options = Options::parse(
            json!({"external": true, "previews": true, "max-posts": 2, "pro-first": false})
                .as_object()
                .unwrap(),
        )
        .unwrap();
        assert!(options.external && options.previews && !options.pro_first);
        assert_eq!(options.max_posts, Some(2));
        assert!(Options::parse(json!({"videos": 1}).as_object().unwrap()).is_err());
    }

    #[test]
    fn classifies_supported_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        let user = || "alice".to_owned();
        let p = Target::Project {
            hash: "LQVJr".into(),
        };
        assert_eq!(t("https://www.artstation.com/artwork/LQVJr").unwrap(), p);
        assert_eq!(t("https://alice.artstation.com/projects/LQVJr").unwrap(), p);
        assert_eq!(t("https://artstn.co/p/LQVJr").unwrap(), p);
        let u = Target::User { user: user() };
        assert_eq!(t("https://www.artstation.com/alice").unwrap(), u);
        assert_eq!(t("https://www.artstation.com/alice/albums/all").unwrap(), u);
        assert_eq!(t("https://alice.artstation.com/").unwrap(), u);
        assert_eq!(t("https://alice.artstation.com/projects").unwrap(), u);
        let album = Target::Album {
            user: user(),
            id: "5".into(),
        };
        assert_eq!(
            t("https://www.artstation.com/alice/albums/5").unwrap(),
            album
        );
        assert_eq!(t("https://alice.artstation.com/albums/5").unwrap(), album);
        assert_eq!(
            t("https://www.artstation.com/alice/likes").unwrap(),
            Target::Likes { user: user() }
        );
        assert_eq!(
            t("https://www.artstation.com/alice/collections").unwrap(),
            Target::Collections { user: user() }
        );
        assert_eq!(
            t("https://www.artstation.com/alice/collections/9").unwrap(),
            Target::Collection {
                user: user(),
                id: "9".into()
            }
        );
        assert_eq!(
            t("https://www.artstation.com/alice/following").unwrap(),
            Target::Following { user: user() }
        );
        assert_eq!(
            t("https://www.artstation.com/contests/x/challenges/3?sorting=latest").unwrap(),
            Target::Challenge {
                id: "3".into(),
                sorting: "latest".into()
            }
        );
        assert_eq!(
            t("https://www.artstation.com/challenges/x/categories/4/").unwrap(),
            Target::Challenge {
                id: "4".into(),
                sorting: "popular".into()
            }
        );
        assert_eq!(
            t("https://www.artstation.com/search?query=ships&sort_by=likes&q=x").unwrap(),
            Target::Search {
                query: vec![
                    ("query".into(), "ships".into()),
                    ("sort_by".into(), "likes".into()),
                    ("q".into(), "x".into())
                ]
            }
        );
        assert_eq!(
            t("https://magazine.artstation.com/artwork?sorting=trending").unwrap(),
            Target::Artwork {
                query: vec![("sorting".into(), "trending".into())]
            }
        );
        for raw in [
            "https://www.artstation.com/",
            "https://www.artstation.com/search",
            "https://www.artstation.com/artwork/",
            "https://www.artstation.com/artwork",
            "https://www.artstation.com/alice/albums/x",
            "https://www.artstation.com/alice/collections/x",
            "https://www.artstation.com/challenges/x/categories/y",
            "https://cdna.artstation.com/p/assets/images/1/large/a.jpg",
            "https://www.artstation.com.evil.example/alice",
            "https://evilartstation.com/alice",
            "https://artstn.co.evil.example/p/LQVJr",
            "https://www.artstation.com:444/alice",
        ] {
            assert!(t(raw).is_err(), "{raw}");
        }
        assert!(media_url("https://cdn.artstation.com/a.jpg").is_some());
        assert!(media_url("https://www.artstation.com/a.jpg").is_none());
        assert!(media_url("http://cdna.artstation.com/a.jpg").is_none());
        let descriptor = |raw: &str| {
            crate::source_archive::for_url(&Url::parse(raw).unwrap())
                .unwrap()
                .format
        };
        assert_eq!(
            descriptor("https://www.artstation.com/alice/albums/5"),
            "a_{album[id]}_{asset[id]}"
        );
        assert_eq!(
            descriptor("https://www.artstation.com/artwork/LQVJr"),
            "{asset[id]}"
        );
        assert_eq!(
            descriptor("https://www.artstation.com/contests/x/challenges/3"),
            "c_{challenge[id]}_{asset_id}"
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

    fn html(body: &str) -> Vec<u8> {
        response("200 OK", body.as_bytes())
    }

    /// Serve replies in order; returns each request's head and body.
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
                loop {
                    let count = stream.read(&mut buffer).unwrap();
                    assert_ne!(count, 0);
                    request.extend_from_slice(&buffer[..count]);
                    if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                        let length = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if request.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                requests.push(String::from_utf8(request).unwrap());
                let _ = stream.write_all(&reply);
            }
            requests
        });
        (root, task)
    }

    fn artstation(root: Url) -> ArtStation {
        ArtStation::new(Duration::from_secs(5))
            .unwrap()
            .with_network(&NetworkOptions::new(Some(String::new()), None).unwrap())
            .unwrap()
            .with_api_root(root)
    }

    fn with(root: Url, options: Value) -> ArtStation {
        artstation(root)
            .with_options(Some(options.as_object().unwrap()))
            .unwrap()
    }

    async fn collect(native: &ArtStation, raw: &str) -> Vec<Media> {
        native
            .stream(&Url::parse(raw).unwrap())
            .map(|m| m.unwrap())
            .collect()
            .await
    }

    async fn children(native: &ArtStation, raw: &str) -> usize {
        let mut events = native.events(&Url::parse(raw).unwrap());
        let mut children = 0;
        while let Some(event) = events.next().await {
            if let ExtractionEvent::BeginChild(scope) = event.unwrap() {
                scope.control.cancel();
                children += 1;
            }
        }
        children
    }

    #[tokio::test]
    async fn fetches_project_with_embeds_retry_and_browser_headers() {
        let (root, server) = serve(vec![
            response("503 Service Unavailable", b""),
            ok(project("LQVJr")),
            ok(userinfo()),
            html(VIDEO_PAGE),
            html(MARMOSET_PAGE),
        ]);
        let events: Vec<_> = artstation(root)
            .with_retries(1)
            .events(&Url::parse("https://www.artstation.com/artwork/LQVJr").unwrap())
            .collect()
            .await;
        let events: Vec<_> = events.into_iter().map(Result::unwrap).collect();
        // Four assets with files: cover, image, video clip and Marmoset.
        assert_eq!(events.len(), 12);
        let mut media = Vec::new();
        for (position, group) in events.as_chunks::<3>().0.iter().enumerate() {
            let ExtractionEvent::Post(post) = &group[0] else {
                panic!("one source Directory per asset")
            };
            assert_eq!(post.count, Some(7));
            let KeywordPolicy::Shared {
                file_fields,
                post_fields,
                ..
            } = &post.keywords
            else {
                panic!("ArtStation asset must reuse Directory keywords")
            };
            assert_eq!(post_fields["num"], [1, 2, 6, 7][position]);
            assert_eq!(
                file_fields.as_ref(),
                &["num", "index", "media_id", "filename", "name", "extension"]
            );
            let ExtractionEvent::Media(item) = &group[1] else {
                panic!("missing media")
            };
            media.push(item.as_ref().clone());
            assert!(matches!(group[2], ExtractionEvent::EndPost));
        }
        let filter = crate::filter::FilterExpression::parse("num == 2 and count == 7").unwrap();
        let ExtractionEvent::Post(post) = &events[3] else {
            panic!()
        };
        assert!(filter.matches_post(post).unwrap());
        assert_eq!(
            media[2].url.as_str(),
            "https://cdna.artstation.com/p/video_clips/16/clip.mp4"
        );
        assert_eq!(
            media[3].url.as_str(),
            "https://cdna.artstation.com/p/marmosets/17/scene.mview"
        );
        let k = keywords(&media[1]);
        assert_eq!(k["userinfo"]["full_name"], "Alice");
        assert_eq!(k["subcategory"], "image");
        let expected = |name: &str| (name.to_owned(), PathBuf::from("artstation").join("alice"));
        assert_eq!(
            paths(&media[1]),
            expected("artstation_90_12_Ship & Sea.png")
        );
        assert_eq!(
            paths(&media[2]),
            expected("artstation_90_16_Ship & Sea.mp4")
        );
        assert_eq!(
            paths(&media[3]),
            expected("artstation_90_17_Ship & Sea.mview")
        );
        let file = media[2].metadata.as_ref().unwrap().file.as_ref().unwrap();
        assert_eq!(file.media_type.as_deref(), Some("video"));
        let requests = server.join().unwrap();
        let request = requests[1].to_ascii_lowercase();
        assert!(
            request.starts_with("get /projects/lqvjr.json "),
            "{request}"
        );
        assert!(request.contains("user-agent: mozilla/5.0"));
        assert!(request.contains("referer: https://www.artstation.com/"));
        assert!(requests[2].starts_with("GET /users/alice/quick.json "));
        assert!(requests[3].starts_with("GET /api/v2/animation/video_clips/16/embed.html "));
        assert!(requests[4].starts_with("GET /embed/17 "));
    }

    #[tokio::test]
    async fn embed_options_select_external_previews_and_skip_viewers() {
        let (root, server) = serve(vec![ok(project("LQVJr")), ok(userinfo())]);
        let native = with(
            root,
            json!({"external": true, "previews": true, "videos": false, "mviews": false}),
        );
        let media = collect(&native, "https://www.artstation.com/artwork/LQVJr").await;
        let urls: Vec<_> = media.iter().map(|m| m.url.as_str()).collect();
        assert_eq!(urls.len(), 6, "{urls:?}");
        assert_eq!(urls[2], "ytdl:https://www.youtube.com/embed/xyz");
        let MediaJob::VideoPage { page_url } = &media[2].job else {
            panic!("external embeds go to yt-dlp")
        };
        assert_eq!(page_url.as_str(), "https://www.youtube.com/embed/xyz");
        assert_eq!(keywords(&media[2])["extension"], "mp4");
        assert_eq!(media[2].filename, "xyz.mp4");
        // Previews of the external, clip and viewer assets follow.
        assert!(urls[3].ends_with("/covers/13/large/v.jpg"));
        assert!(urls[4].ends_with("/video_clips/16/large/p.jpg"));
        assert!(urls[5].ends_with("/marmosets/17/large/m.jpg"));
        assert_eq!(server.join().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn paginates_portfolio_lazily_with_max_posts_and_skips_missing() {
        let (root, server) = serve(vec![
            ok(userinfo()),
            ok(json!({"data": [{"hash_id": "a"}, {"hash_id": "gone"}], "total_count": 4})),
            ok(project("a")),
            response("404 Not Found", b""),
            ok(json!({"data": [{"hash_id": "b"}, {"hash_id": "c"}], "total_count": 4})),
            ok(project("b")),
        ]);
        let native = with(
            root,
            json!({"max-posts": 3, "mviews": false, "videos": false}),
        );
        let media = collect(&native, "https://www.artstation.com/alice").await;
        assert_eq!(media.len(), 4);
        assert_eq!(keywords(&media[0])["subcategory"], "user");
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /users/alice/projects.json?album_id=all&page=1 "));
        assert!(
            requests[1]
                .to_ascii_lowercase()
                .contains("origin: https://www.artstation.com")
        );
        assert!(requests[4].starts_with("GET /users/alice/projects.json?album_id=all&page=2 "));
        assert!(requests[5].starts_with("GET /projects/b.json "));

        let (root, server) = serve(vec![response("403 Forbidden", b"cf")]);
        let error = artstation(root)
            .stream(&Url::parse("https://alice.artstation.com/projects/x1").unwrap())
            .collect::<Vec<_>>()
            .await
            .remove(0)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("Cloudflare") && !error.contains("127.0.0.1"),
            "{error}"
        );
        server.join().unwrap();
    }

    fn single(hash: &str) -> Value {
        json!({"id": 1, "hash_id": hash, "title": "T", "description": "",
            "created_at": "2024-01-02T03:04:05Z", "user": {"username": "bob"},
            "assets": [{"id": 21, "has_image": true, "has_embedded_player": false,
                "image_url": "https://cdna.artstation.com/p/assets/images/images/021/large/z.jpg"}]})
    }

    #[tokio::test]
    async fn album_likes_and_collection_listings() {
        let page = |hash: &str| ok(json!({"data": [{"hash_id": hash}], "total_count": 1}));
        let (root, server) = serve(vec![
            ok(userinfo()),
            page("a"),
            ok(single("a")),
            ok(userinfo()),
            page("b"),
            ok(single("b")),
            ok(json!({"id": 9, "name": "Refs", "user_id": 7})),
            page("c"),
            ok(single("c")),
        ]);
        let native = artstation(root);
        let album = collect(&native, "https://alice.artstation.com/albums/5").await;
        let k = keywords(&album[0]);
        assert_eq!(k["album"], json!({"id": 5, "title": "Trip"}));
        assert_eq!(
            paths(&album[0]),
            (
                "artstation_1_21_T.jpg".into(),
                PathBuf::from("artstation")
                    .join("alice")
                    .join("Albums")
                    .join("5 - Trip")
            )
        );
        let likes = collect(&native, "https://www.artstation.com/alice/likes").await;
        assert_eq!(keywords(&likes[0])["subcategory"], "likes");
        assert_eq!(
            paths(&likes[0]).1,
            PathBuf::from("artstation").join("alice").join("Likes")
        );
        let collection = collect(&native, "https://www.artstation.com/alice/collections/9").await;
        let k = keywords(&collection[0]);
        assert_eq!(
            (&k["user"], &k["collection"]["name"]),
            (&json!("alice"), &json!("Refs"))
        );
        assert!(!k.contains_key("userinfo"));
        assert_eq!(
            paths(&collection[0]).1,
            PathBuf::from("artstation").join("alice").join("9 Refs")
        );
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /users/alice/projects.json?album_id=5&page=1 "));
        assert!(requests[4].starts_with("GET /users/alice/likes.json?page=1 "));
        assert!(requests[6].starts_with("GET /collections/9.json?username=alice "));
        assert!(
            requests[7].starts_with("GET /collections/9/projects.json?collection_id=9&page=1 ")
        );

        let (root, server) = serve(vec![ok(userinfo())]);
        let missing = artstation(root)
            .stream(&Url::parse("https://www.artstation.com/alice/albums/6").unwrap())
            .collect::<Vec<_>>()
            .await;
        assert!(
            missing[0]
                .as_ref()
                .unwrap_err()
                .to_string()
                .contains("album")
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn search_posts_with_csrf_token_and_artwork_lists() {
        let (root, server) = serve(vec![
            ok(json!({"public_csrf_token": "tok123"})),
            ok(json!({"data": [{"hash_id": "s"}], "total_count": 2})),
            ok(single("s")),
            ok(json!({"data": [], "total_count": 2})),
            ok(json!({"data": [{"hash_id": "w"}], "total_count": 1})),
            ok(single("w")),
        ]);
        let native = with(root, json!({"pro-first": false}));
        let search = collect(
            &native,
            "https://www.artstation.com/search?query=space+ship&sort_by=Likes&tags=a,b&medium_ids=3",
        )
        .await;
        let k = keywords(&search[0]);
        assert_eq!(
            k["search"],
            json!({"query": "space ship", "sorting": "likes", "tags": ["a", "b"]})
        );
        assert!(!k.contains_key("userinfo"));
        assert_eq!(
            paths(&search[0]).1,
            PathBuf::from("artstation")
                .join("Searches")
                .join("space ship")
        );
        let artwork = collect(
            &native,
            "https://www.artstation.com/artwork?sorting=trending&page=4",
        )
        .await;
        let k = keywords(&artwork[0]);
        assert_eq!(k["artwork"], json!({"sorting": "trending", "page": "4"}));
        assert_eq!(
            paths(&artwork[0]).1,
            PathBuf::from("artstation")
                .join("Artworks")
                .join("Trending")
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("POST /api/v2/csrf_protection/token.json "));
        assert!(requests[1].starts_with("POST /api/v2/search/projects.json "));
        assert!(
            requests[1]
                .to_ascii_lowercase()
                .contains("public-csrf-token: tok123")
        );
        let body: Value =
            serde_json::from_str(requests[1].split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body["page"], 1);
        assert_eq!(body["query"], "space ship");
        assert_eq!(body["pro_first"], "0");
        assert_eq!(
            body["filters"],
            json!([{"field": "tags", "method": "include", "value": ["a", "b"]},
                {"field": "medium_ids", "method": "include", "value": ["3"]}])
        );
        let body: Value =
            serde_json::from_str(requests[3].split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body["page"], 2);
        assert!(requests[4].starts_with("GET /projects.json?sorting=trending&page=1 "));
    }

    #[tokio::test]
    async fn collections_and_following_queue_child_jobs() {
        let (root, server) = serve(vec![
            ok(json!([{"id": 9, "name": "Refs"}, {"id": "x"}])),
            ok(json!({"data": [{"username": "bob"}, {"username": "../x"}], "total_count": 3})),
            ok(json!({"data": [{"username": "carol"}], "total_count": 3})),
        ]);
        let native = artstation(root);
        assert_eq!(
            children(&native, "https://www.artstation.com/alice/collections").await,
            1
        );
        assert_eq!(
            children(&native, "https://www.artstation.com/alice/following").await,
            2
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /collections.json?username=alice "));
        assert!(requests[1].starts_with("GET /users/alice/following.json?page=1 "));
        assert!(requests[2].starts_with("GET /users/alice/following.json?page=2 "));
    }

    #[tokio::test]
    async fn challenge_submission_updates_link_images() {
        let image = "https://cdna.artstation.com/p/a/b/c/001/234/567/large/x.jpg";
        let update = json!({"id": 50, "submission_id": 40, "created_at": "2024-01-02T03:04:05Z",
            "body": format!("<a href=\"{image}\">1</a><a href=\"{image}\">2</a><a href=\"https://evil.example/y.jpg\">3</a>")});
        let (root, server) = serve(vec![
            ok(json!({"id": 3, "title": "Dragons"})),
            ok(json!({"data": [{"id": 40}], "total_count": 1})),
            ok(json!({"data": [update], "total_count": 1})),
        ]);
        let media = collect(
            &artstation(root),
            "https://www.artstation.com/contests/x/challenges/3?sorting=latest",
        )
        .await;
        assert_eq!(media.len(), 1);
        let k = keywords(&media[0]);
        assert_eq!(
            (&k["asset_id"], &k["challenge"]["title"]),
            (&json!(1234567), &json!("Dragons"))
        );
        assert_eq!(
            paths(&media[0]),
            (
                "40_1234567_x.jpg".into(),
                PathBuf::from("artstation")
                    .join("Challenges")
                    .join("3 - Dragons")
            )
        );
        // Challenge images have their own archive identity.
        let key = media[0].archive_key().unwrap();
        let mut plain = media[0].clone();
        plain.metadata.as_mut().unwrap().site_keywords = None;
        assert_ne!(plain.archive_key(), Some(key));
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/v2/competition/challenges/3.json "));
        assert!(requests[1].starts_with(
            "GET /api/v2/competition/submissions.json?per_page=50&challenge_id=3&sort_by=latest&page=1 "
        ));
        assert!(requests[2].starts_with(
            "GET /api/v2/competition/submissions/40/submission_updates.json?per_page=50&page=1 "
        ));
    }
}
