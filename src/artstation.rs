//! Experimental native ArtStation project and portfolio extraction.
//!
//! Mirrors gallery_dl/extractor/artstation.py using the site's JSON
//! endpoints: `projects/HASH.json` for single projects and
//! `users/USER/projects.json?page=N` (capped at 200 pages) followed by each
//! project's JSON for portfolios. ArtStation sits behind Cloudflare, so
//! requests carry browser-like headers and may still be blocked. Only image
//! assets are downloaded, as-is (no `4k` rewrite), over HTTPS from
//! `cdn*.artstation.com`; covers, video player embeds and other asset types
//! are skipped.
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
use serde_json::Value;
use std::{
    collections::{BTreeMap, VecDeque},
    sync::Arc,
    time::{Duration, SystemTime},
};
use url::Url;

/// Browser-like default; ArtStation's Cloudflare front rejects many tool agents.
const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:128.0) Gecko/20100101 Firefox/128.0";
const BROWSER_HEADERS: [(&str, &str); 3] = [
    ("Accept", "application/json, text/plain, */*"),
    ("Accept-Language", "en-US,en;q=0.5"),
    ("Referer", "https://www.artstation.com/"),
];
const MAX_RESPONSE: usize = 16 * 1024 * 1024;
const MAX_PAGES: u64 = 200;
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
    /// A `projects.json`-style listing: albums, likes, one collection or the
    /// artwork page, as an API path with its query.
    Listing {
        path: String,
    },
    /// `USER/collections`: queued collection children.
    Collections {
        user: String,
    },
    /// `challenges/NAME/categories/ID` (or `contests/NAME/challenges/ID`).
    Challenge {
        id: String,
        sorting: String,
    },
    /// `USER/following`: followed users, queued as portfolio children.
    Following {
        user: String,
    },
    /// `search?QUERY`: the decoded query parameters, in order.
    Search {
        params: Vec<(String, String)>,
    },
}

fn plain(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https")
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
}

fn digits(id: &str) -> bool {
    !id.is_empty() && id.len() <= 20 && id.bytes().all(|b| b.is_ascii_digit())
}

pub fn target(url: &Url) -> Result<Target> {
    let unsupported = || {
        error(
            "unsupported ArtStation URL; expected https://www.artstation.com/artwork/HASH, https://USER.artstation.com/projects/HASH or https://www.artstation.com/USER",
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
    match host {
        // Upstream `ArtstationImageExtractor` also accepts `artstn.co/p/HASH`.
        "artstn.co" | "www.artstn.co" => match parts[..] {
            ["p", hash] if valid_hash(hash) => Ok(Target::Project { hash: hash.into() }),
            _ => Err(unsupported()),
        },
        "www.artstation.com" | "artstation.com" => match parts[..] {
            [user, "following"] if valid_user(user) && !RESERVED.contains(&user) => {
                Ok(Target::Following { user: user.into() })
            }
            ["artwork" | "projects", hash] if valid_hash(hash) => {
                Ok(Target::Project { hash: hash.into() })
            }
            [
                "challenges" | "contests",
                _,
                "categories" | "challenges",
                id,
                ..,
            ] if digits(id) => Ok(Target::Challenge {
                id: (*id).into(),
                sorting: url
                    .query_pairs()
                    .find(|(k, _)| k == "sorting")
                    .map(|(_, v)| v.into_owned())
                    .filter(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_lowercase()))
                    .unwrap_or_else(|| "popular".into()),
            }),
            ["search"] if url.query().is_some_and(|q| !q.is_empty()) => Ok(Target::Search {
                params: crate::sites::gdl::parse_query(url.query().unwrap_or_default())
                    .into_iter()
                    .map(|(k, v)| (k, v.as_str().unwrap_or_default().to_owned()))
                    .collect(),
            }),
            ["artwork"] if url.query().is_some() => Ok(Target::Listing {
                path: format!("projects.json?{}", url.query().unwrap_or_default()),
            }),
            [user, "albums", id] if valid_user(user) && digits(id) => Ok(Target::Listing {
                path: format!("users/{user}/projects.json?album_id={id}"),
            }),
            [user, "likes"] if valid_user(user) && !RESERVED.contains(&user) => {
                Ok(Target::Listing {
                    path: format!("users/{user}/likes.json"),
                })
            }
            [user, "collections"] if valid_user(user) && !RESERVED.contains(&user) => {
                Ok(Target::Collections { user: user.into() })
            }
            [user, "collections", id] if valid_user(user) && digits(id) => Ok(Target::Listing {
                path: format!("collections/{id}/projects.json?collection_id={id}"),
            }),
            [user] | [user, "albums", "all"] if valid_user(user) && !RESERVED.contains(&user) => {
                Ok(Target::User { user: user.into() })
            }
            _ => Err(unsupported()),
        },
        _ => {
            let user = host
                .strip_suffix(".artstation.com")
                .filter(|u| valid_user(u) && !RESERVED_HOSTS.contains(u) && !u.starts_with("cdn"))
                .ok_or_else(unsupported)?;
            match parts[..] {
                ["projects" | "artwork", hash] if valid_hash(hash) => {
                    Ok(Target::Project { hash: hash.into() })
                }
                // Upstream's `USER.artstation.com(?:/projects)?/?$`.
                [] | ["projects"] | ["albums", "all"] => Ok(Target::User { user: user.into() }),
                ["albums", id] if digits(id) => Ok(Target::Listing {
                    path: format!("users/{user}/projects.json?album_id={id}"),
                }),
                _ => Err(unsupported()),
            }
        }
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

fn number(value: &Value, key: &str) -> Option<u64> {
    value.get(key)?.as_u64().filter(|v| *v > 0)
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

/// Upstream embed and asset options: `videos`, `mviews`, `external` and
/// `previews`.
#[derive(Clone, Copy, Debug)]
pub struct AssetOptions {
    pub videos: bool,
    pub mviews: bool,
    pub external: bool,
    pub previews: bool,
}

impl Default for AssetOptions {
    fn default() -> Self {
        Self {
            videos: true,
            mviews: true,
            external: false,
            previews: false,
        }
    }
}

/// One project file: ready to download, or an ArtStation-hosted player page
/// that names the file (`marmoset` for `.mview` viewers, else a video clip).
enum Asset {
    Ready(Media),
    Embed {
        media: Media,
        page: Url,
        marmoset: bool,
    },
}

/// Parse a `projects/HASH.json` response into its image assets.
pub fn parse_project(bytes: &[u8], hash: &str) -> Result<Vec<Media>> {
    parse_project_data(bytes, hash, &AssetOptions::default()).map(|(assets, _)| {
        assets
            .into_iter()
            .filter_map(|asset| match asset {
                Asset::Ready(media) => Some(media),
                Asset::Embed { .. } => None,
            })
            .collect()
    })
}

/// Upstream `items`: each asset's embedded player (per the options), then
/// its image unless the player replaced it. ArtStation image URLs are
/// upgraded to `8k` with `4k`/`large`/`medium`/`small` fallbacks.
fn parse_project_data(
    bytes: &[u8],
    hash: &str,
    options: &AssetOptions,
) -> Result<(Vec<Asset>, usize)> {
    let project: Value =
        serde_json::from_slice(bytes).map_err(|_| error("ArtStation returned invalid JSON"))?;
    if text(&project, "hash_id").as_deref() != Some(hash) {
        return Err(error("ArtStation project not found"));
    }
    let username = project
        .get("user")
        .and_then(|u| text(u, "username"))
        .map(Arc::<str>::from);
    let caption = text(&project, "title").map(Arc::<str>::from);
    let timestamp = text(&project, "published_at").and_then(|d| crate::date::parse_date(&d).ok());
    let assets = project
        .get("assets")
        .and_then(Value::as_array)
        .ok_or_else(|| error("ArtStation project contains no asset list"))?;
    let mut items = Vec::new();
    for (position, asset) in assets.iter().enumerate() {
        let Some(id) = text(asset, "id").filter(|id| valid_id(id)) else {
            continue;
        };
        let item = |url: Url, ext: String, video: bool| Media {
            filename: format!("{hash}_{id}.{ext}"),
            job: MediaJob::DirectHttp,
            category: "artstation".into(),
            metadata: Some(PostMetadata {
                file: Some(FileMetadata {
                    width: number(asset, "width"),
                    height: number(asset, "height"),
                    media_type: Some(Arc::from(if video { "video" } else { "photo" })),
                    ..FileMetadata::default()
                }),
                post_id: hash.into(),
                media_id: id.clone(),
                username: username.clone(),
                caption: caption.clone(),
                timestamp,
                index: position + 1,
                ..PostMetadata::default()
            }),
            url,
            headers: BTreeMap::new(),
            handoff: BTreeMap::new(),
        };
        if asset.get("has_embedded_player").and_then(Value::as_bool) == Some(true) {
            let player = text(asset, "player_embedded").unwrap_or_default();
            let src = crate::sites::extr(&player, "src=\"", "\"")
                .filter(|s| !s.is_empty())
                .or_else(|| crate::sites::extr(&player, "src='", "'"))
                .unwrap_or_default();
            let kind = text(asset, "image_url")
                .and_then(|u| crate::sites::extr(&u, "/assets/", "/").map(str::to_owned))
                .unwrap_or_default();
            let hosted = src.starts_with("https://www.artstation.com") && !kind.is_empty();
            if hosted {
                let marmoset = kind == "marmosets";
                if (if marmoset {
                    options.mviews
                } else {
                    options.videos
                }) && let Ok(page) = Url::parse(src)
                {
                    let placeholder = page.clone();
                    items.push(Asset::Embed {
                        media: item(placeholder, String::new(), !marmoset),
                        page,
                        marmoset,
                    });
                }
            } else if !src.is_empty()
                && options.external
                && let Ok(page) = Url::parse(src)
                && matches!(page.scheme(), "http" | "https")
            {
                let mut media = item(page.clone(), "mp4".into(), true);
                media.job = MediaJob::VideoPage { page_url: page };
                items.push(Asset::Ready(media));
            }
            if !options.previews {
                continue;
            }
        }
        let has_image = match asset.get("has_image") {
            Some(value) => value.as_bool() == Some(true),
            None => text(asset, "asset_type").as_deref() == Some("image"),
        };
        if !has_image {
            continue;
        }
        let Some(url) = text(asset, "image_url").and_then(|u| media_url(&u)) else {
            continue;
        };
        let ext = extension(&url);
        let mut media = item(url.clone(), ext, false);
        if url.path().contains("/images/images/")
            && let Some((lhs, rhs)) = url.as_str().split_once("/large/")
            && !rhs.is_empty()
            && let Ok(large) = Url::parse(&format!("{lhs}/8k/{rhs}"))
        {
            media.url = large;
            let fallback: Vec<Value> = ["4k", "large", "medium", "small"]
                .iter()
                .map(|size| Value::String(format!("{lhs}/{size}/{rhs}")))
                .collect();
            media
                .handoff
                .insert("_fallback".into(), Value::Array(fallback));
        }
        items.push(Asset::Ready(media));
    }
    Ok((items, assets.len()))
}

/// The file URL named by an ArtStation player page (upstream
/// `_extract_embed`).
fn embed_url(page: &str, marmoset: bool) -> Option<Url> {
    let raw = if marmoset {
        crate::sites::extr(page, "marmoset.embed(", "\",")?
            .trim_matches(|c| matches!(c, '"' | '\'' | ' '))
            .to_owned()
    } else {
        let from = page.find("id=\"video\"").map_or(0, |at| at + 1);
        crate::sites::extr(&page[from..], " src=\"", "\"")?.to_owned()
    };
    media_url(&raw)
}

/// Parse a `users/USER/projects.json` page into project hashes and
/// `total_count`.
pub fn parse_listing(bytes: &[u8]) -> Result<(Vec<String>, u64)> {
    let body: Value =
        serde_json::from_slice(bytes).map_err(|_| error("ArtStation returned invalid JSON"))?;
    let data = body
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| error("ArtStation response contains no project list"))?;
    let hashes = data
        .iter()
        .filter_map(|p| text(p, "hash_id").filter(|h| valid_hash(h)))
        .collect();
    Ok((
        hashes,
        body.get("total_count").and_then(Value::as_u64).unwrap_or(0),
    ))
}

pub struct ArtStation {
    client: Client,
    network: NetworkOptions,
    api_root: Url,
    timeout: Duration,
    request_timeout: Duration,
    retries: u8,
    rate_limits: RateLimits,
    /// Upstream `pro-first` (search ordering).
    pro_first: bool,
    assets: AssetOptions,
    /// Upstream `max-posts`: at most this many projects per listing.
    max_posts: Option<u64>,
}

/// A JSON POST body and its extra headers.
struct Post {
    body: Vec<u8>,
    headers: Vec<(&'static str, String)>,
}

/// Upstream `ArtstationSearchExtractor.projects`: the search request body;
/// `*_ids` and `tags` parameters become include filters.
fn search_body(params: &[(String, String)], pro_first: bool) -> Value {
    let param = |key: &str| {
        params
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    };
    let query = param("query")
        .filter(|q| !q.is_empty())
        .or_else(|| param("q"))
        .unwrap_or_default();
    let filters: Vec<Value> = params
        .iter()
        .filter(|(key, _)| key.ends_with("_ids") || key == "tags")
        .map(|(key, value)| {
            serde_json::json!({
                "field": key,
                "method": "include",
                "value": value.split(',').collect::<Vec<_>>(),
            })
        })
        .collect();
    serde_json::json!({
        "query": crate::sites::percent_decode(query),
        "page": null,
        "per_page": 50,
        "sorting": param("sort_by").unwrap_or("relevance").to_lowercase(),
        "pro_first": if pro_first { "1" } else { "0" },
        "filters": filters,
        "additional_fields": [],
    })
}

struct ListingState {
    /// API path, possibly with a query.
    path: String,
    /// JSON body of a POST listing (search), with its `page` updated.
    post: Option<Value>,
    /// `PUBLIC-CSRF-TOKEN` for POST listings, fetched before the first page.
    csrf: Option<String>,
    page: u64,
    seen: u64,
    /// Projects queued so far (for `max-posts`).
    projects: u64,
    done: bool,
    pending: VecDeque<ExtractionEvent>,
}

impl ArtStation {
    pub fn new(timeout: Duration) -> Result<Self> {
        Ok(Self {
            client: NetworkOptions::default().client(USER_AGENT, timeout)?,
            network: NetworkOptions::default(),
            api_root: Url::parse("https://www.artstation.com/").expect("static URL"),
            timeout,
            request_timeout: timeout,
            retries: 0,
            rate_limits: RateLimits::default(),
            pro_first: true,
            assets: AssetOptions::default(),
            max_posts: None,
        })
    }

    /// Site options: `pro-first` (default `true`), `videos`, `mviews`,
    /// `external` and `previews`.
    pub fn with_options(mut self, options: Option<&serde_json::Map<String, Value>>) -> Self {
        let flag = |key: &str, default: bool| {
            options
                .and_then(|o| o.get(key))
                .map_or(default, |value| crate::sites::gdl::truthy(Some(value)))
        };
        self.pro_first = flag("pro-first", true);
        self.assets = AssetOptions {
            videos: flag("videos", true),
            mviews: flag("mviews", true),
            external: flag("external", false),
            previews: flag("previews", false),
        };
        self.max_posts = options
            .and_then(|o| o.get("max-posts"))
            .and_then(Value::as_u64)
            .filter(|max| *max > 0);
        self
    }

    /// Apply proxy and user-agent settings to API requests and emitted media.
    pub fn with_network(mut self, network: &NetworkOptions) -> Result<Self> {
        self.client = network.client(USER_AGENT, self.timeout)?;
        self.network = network.clone();
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

    async fn call(&self, path: &str) -> Result<Vec<u8>> {
        self.call_with(path, None).await
    }

    /// A GET, or a JSON POST with extra headers.
    async fn call_with(&self, path: &str, post: Option<&Post>) -> Result<Vec<u8>> {
        let endpoint = self.api_root.join(path).expect("validated ArtStation path");
        tokio::time::timeout(self.timeout, self.request(&endpoint, post))
            .await
            .map_err(|_| error("native ArtStation extraction timed out"))?
    }

    /// Upstream `_init_csrf_token`.
    async fn csrf_token(&self) -> Result<String> {
        let origin = self.origin();
        let post = Post {
            body: b"{}".to_vec(),
            headers: vec![("Accept", "*/*".into()), ("Origin", origin)],
        };
        let body = self
            .call_with("api/v2/csrf_protection/token.json", Some(&post))
            .await?;
        serde_json::from_slice::<Value>(&body)
            .ok()
            .and_then(|v| v.get("public_csrf_token")?.as_str().map(str::to_owned))
            .ok_or_else(|| error("ArtStation returned no CSRF token"))
    }

    fn origin(&self) -> String {
        self.api_root.as_str().trim_end_matches('/').to_owned()
    }

    async fn project(&self, hash: &str) -> Result<VecDeque<ExtractionEvent>> {
        let body = self.call(&format!("projects/{hash}.json")).await?;
        let (assets, count) = parse_project_data(&body, hash, &self.assets)?;
        let mut items = Vec::new();
        for asset in assets {
            let mut item = match asset {
                Asset::Ready(media) => media,
                Asset::Embed {
                    mut media,
                    page,
                    marmoset,
                } => {
                    // Player pages are fetched relative to the API origin.
                    let path = page
                        .as_str()
                        .trim_start_matches("https://www.artstation.com/");
                    let page = String::from_utf8_lossy(&self.call(path).await?).into_owned();
                    let Some(url) = embed_url(&page, marmoset) else {
                        continue;
                    };
                    let (_, ext) = crate::sites::nameext(&url);
                    media
                        .filename
                        .push_str(if ext.is_empty() { "mp4" } else { &ext });
                    media.url = url;
                    media
                }
            };
            self.network.apply_media(&mut item);
            items.push(item);
        }
        // Upstream emits Directory for each asset, even within one project.
        // Each asset's Url then reuses that Directory dictionary.
        Ok(items
            .into_iter()
            .flat_map(|item| {
                let policy = KeywordPolicy::shared(&[
                    "num",
                    "index",
                    "media_id",
                    "filename",
                    "name",
                    "extension",
                ])
                .with_post_fields(&item, &["num", "index"]);
                let mut events = group_with_keywords(vec![item], "artstation", policy);
                if let Some(ExtractionEvent::Post(post)) = events.front_mut() {
                    post.count = Some(count);
                }
                events
            })
            .collect())
    }

    async fn next_page(&self, state: &mut ListingState) -> Result<()> {
        state.page += 1;
        let body = if let Some(json) = state.post.as_mut() {
            if state.csrf.is_none() {
                state.csrf = Some(self.csrf_token().await?);
            }
            json["page"] = state.page.into();
            let post = Post {
                body: json.to_string().into_bytes(),
                headers: vec![
                    ("Origin", self.origin()),
                    ("PUBLIC-CSRF-TOKEN", state.csrf.clone().unwrap_or_default()),
                ],
            };
            self.call_with(&state.path, Some(&post)).await?
        } else {
            let separator = if state.path.contains('?') { '&' } else { '?' };
            self.call(&format!("{}{separator}page={}", state.path, state.page))
                .await?
        };
        let (mut hashes, total) = parse_listing(&body)?;
        let listed = hashes.len() as u64;
        if let Some(max) = self.max_posts {
            hashes.truncate(max.saturating_sub(state.projects) as usize);
        }
        state.projects += hashes.len() as u64;
        state.seen += listed;
        state.done = listed == 0
            || state.seen >= total
            || state.page >= MAX_PAGES
            || self.max_posts.is_some_and(|max| state.projects >= max);
        for hash in hashes {
            let events = self.project(&hash).await?;
            state.pending.extend(events);
        }
        Ok(())
    }

    pub fn stream(&self, url: &Url) -> BoxStream<'_, Result<Media>> {
        crate::event::media_only(self.events(url))
    }

    /// One page of upstream `_pagination` (`data` and `total_count`).
    async fn data_page(&self, path: &str) -> Result<(Vec<Value>, u64)> {
        let value: Value = serde_json::from_slice(&self.call(path).await?)
            .map_err(|_| error("ArtStation returned invalid JSON"))?;
        Ok((
            value
                .get("data")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            value
                .get("total_count")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        ))
    }

    /// All `data` entries of a paginated listing.
    async fn all_pages(&self, path: &str) -> Result<Vec<Value>> {
        let mut all = Vec::new();
        for page in 1..=MAX_PAGES {
            let (data, total) = self.data_page(&format!("{path}&page={page}")).await?;
            let empty = data.is_empty();
            all.extend(data);
            if empty || all.len() as u64 >= total {
                break;
            }
        }
        Ok(all)
    }

    /// Upstream `ArtstationChallengeExtractor`: images linked from each
    /// submission's updates.
    fn challenge(&self, id: String, sorting: String) -> BoxStream<'_, Result<ExtractionEvent>> {
        let base = "api/v2/competition";
        let challenge_id: std::sync::Arc<str> = id.as_str().into();
        stream::once(async move {
            let submissions = self
                .all_pages(&format!(
                    "{base}/submissions.json?per_page=50&challenge_id={id}&sort_by={sorting}"
                ))
                .await?;
            Ok::<_, crate::Error>(submissions)
        })
        .map(move |result| match result {
            Ok(submissions) => {
                let challenge_id = challenge_id.clone();
                stream::iter(submissions)
                    .then(move |submission| {
                        let id = challenge_id.clone();
                        async move {
                            let Some(sid) = submission.get("id").and_then(Value::as_u64) else {
                                return Ok(VecDeque::new());
                            };
                            let updates = self
                                .all_pages(&format!(
                                    "{base}/submissions/{sid}/submission_updates.json?per_page=50"
                                ))
                                .await?;
                            let mut media = Vec::new();
                            for update in &updates {
                                let body = update
                                    .get("body")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default();
                                let mut seen = std::collections::HashSet::new();
                                for href in crate::imagehosts::between_all(body, " href=\"", "\"") {
                                    let Some(url) = media_url(href).filter(|_| seen.insert(href))
                                    else {
                                        continue;
                                    };
                                    // Upstream `_id_from_url`: path parts 7..10 joined.
                                    let parts: Vec<&str> = href.split('/').collect();
                                    let asset =
                                        parts.get(7..10).map(|p| p.concat()).unwrap_or_default();
                                    let name = url
                                        .path_segments()
                                        .and_then(|mut s| s.next_back())
                                        .unwrap_or("file")
                                        .to_owned();
                                    media.push(Media {
                                        filename: format!("{sid}_{asset}_{name}"),
                                        job: MediaJob::DirectHttp,
                                        category: "artstation".into(),
                                        metadata: Some(PostMetadata {
                                            post_id: sid.to_string(),
                                            media_id: format!("c{id}_{asset}"),
                                            index: media.len() + 1,
                                            ..PostMetadata::default()
                                        }),
                                        url,
                                        headers: BTreeMap::new(),
                                        handoff: BTreeMap::new(),
                                    });
                                }
                            }
                            for item in &mut media {
                                self.network.apply_media(item);
                            }
                            Ok::<_, crate::Error>(crate::event::group(media, "artstation"))
                        }
                    })
                    .map(|result| match result {
                        Ok(events) => stream::iter(events.into_iter().map(Ok)).boxed(),
                        Err(e) => stream::once(async move { Err(e) }).boxed(),
                    })
                    .flatten()
                    .boxed()
            }
            Err(e) => stream::once(async move { Err(e) }).boxed(),
        })
        .flatten()
        .boxed()
    }

    /// Upstream `ArtstationFollowingExtractor`: queue followed portfolios.
    fn following(&self, user: String) -> BoxStream<'_, Result<ExtractionEvent>> {
        stream::once(async move {
            self.all_pages(&format!("users/{user}/following.json?per_page=50"))
                .await
        })
        .map(move |result| match result {
            Ok(users) => stream::iter(users)
                .filter_map(move |entry| {
                    let name = entry
                        .get("username")
                        .and_then(Value::as_str)
                        .filter(|name| valid_user(name))
                        .map(str::to_owned);
                    std::future::ready(name.map(|name| {
                        let data = entry.as_object().cloned().unwrap_or_default();
                        crate::event::child_job(
                            crate::event::ChildMetadata::from_raw("artstation", "following", &data),
                            move || {
                                let url = Url::parse(&format!("https://www.artstation.com/{name}"))
                                    .expect("validated user");
                                self.events(&url)
                            },
                        )
                    }))
                })
                .flatten()
                .boxed(),
            Err(e) => stream::once(async move { Err(e) }).boxed(),
        })
        .flatten()
        .boxed()
    }

    /// Upstream `ArtstationCollectionsExtractor`: queue every collection.
    fn collections(&self, user: String) -> BoxStream<'_, Result<ExtractionEvent>> {
        stream::once(async move {
            let body = self
                .call(&format!("collections.json?username={user}"))
                .await?;
            let list: Value = serde_json::from_slice(&body)
                .map_err(|_| error("ArtStation returned invalid JSON"))?;
            Ok::<_, crate::Error>((user, list.as_array().cloned().unwrap_or_default()))
        })
        .map(move |result| match result {
            Ok((user, list)) => stream::iter(list)
                .filter_map(move |collection| {
                    let user = user.clone();
                    std::future::ready(collection.get("id").and_then(Value::as_u64).map(|id| {
                        let data = collection.as_object().cloned().unwrap_or_default();
                        crate::event::child_job(
                            crate::event::ChildMetadata::from_raw(
                                "artstation",
                                "collections",
                                &data,
                            ),
                            move || {
                                let url = Url::parse(&format!(
                                    "https://www.artstation.com/{user}/collections/{id}"
                                ))
                                .expect("valid collection URL");
                                self.events(&url)
                            },
                        )
                    }))
                })
                .flatten()
                .boxed(),
            Err(e) => stream::once(async move { Err(e) }).boxed(),
        })
        .flatten()
        .boxed()
    }

    /// Emit one post boundary per project; portfolios paginate lazily.
    pub fn events(&self, url: &Url) -> BoxStream<'_, Result<ExtractionEvent>> {
        let mut post = None;
        let path = match target(url) {
            Ok(Target::User { user }) => format!("users/{user}/projects.json"),
            Ok(Target::Listing { path }) => path,
            Ok(Target::Search { params }) => {
                post = Some(search_body(&params, self.pro_first));
                "api/v2/search/projects.json".into()
            }
            Ok(Target::Collections { user }) => return self.collections(user),
            Ok(Target::Challenge { id, sorting }) => return self.challenge(id, sorting),
            Ok(Target::Following { user }) => return self.following(user),
            Ok(Target::Project { hash }) => {
                return stream::once(async move { self.project(&hash).await })
                    .map(|result| match result {
                        Ok(events) => stream::iter(events.into_iter().map(Ok)).boxed(),
                        Err(e) => stream::once(async move { Err(e) }).boxed(),
                    })
                    .flatten()
                    .boxed();
            }
            Err(e) => return stream::once(async move { Err(e) }).boxed(),
        };
        let state = ListingState {
            path,
            post,
            csrf: None,
            page: 0,
            seen: 0,
            projects: 0,
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

    async fn request(&self, endpoint: &Url, post: Option<&Post>) -> Result<Vec<u8>> {
        for attempt in 0..=self.retries {
            match self.attempt(endpoint, post, attempt).await {
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
        post: Option<&Post>,
        attempt: u8,
    ) -> std::result::Result<Vec<u8>, Failure> {
        let mut target = endpoint.clone();
        let mut redirects = Redirects::new(endpoint);
        let mut response = loop {
            self.rate_limits.wait(&target, self.timeout).await?;
            let mut request = match post {
                Some(_) => self.client.post(target.clone()),
                None => self.client.get(target.clone()),
            }
            .timeout(self.request_timeout);
            for (name, value) in BROWSER_HEADERS {
                if post.is_some_and(|p| p.headers.iter().any(|(n, _)| *n == name)) {
                    continue;
                }
                request = request.header(name, value);
            }
            if let Some(post) = post {
                for (name, value) in &post.headers {
                    request = request.header(*name, value);
                }
                request = request
                    .header("Content-Type", "application/json")
                    .body(post.body.clone());
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

    fn project(hash: &str) -> Value {
        json!({"hash_id": hash, "title": "Ship", "published_at": "2024-01-01T00:00:00.000-00:00",
            "user": {"username": "alice"}, "assets": [
                {"id": 11, "asset_type": "cover", "has_image": true,
                 "image_url": "https://cdna.artstation.com/p/assets/covers/1/large/c.jpg"},
                {"id": 12, "asset_type": "image", "has_image": true, "width": 1920, "height": 1080,
                 "image_url": "https://cdnb.artstation.com/p/assets/images/12/large/a.png?1700"},
                {"id": 13, "asset_type": "video", "has_image": false,
                 "player_embedded": "<iframe src='https://www.youtube.com/embed/x'></iframe>"},
                {"id": 14, "asset_type": "image", "has_image": true,
                 "image_url": "http://cdnb.artstation.com/p/assets/images/14/large/b.jpg"},
                {"id": 15, "asset_type": "image", "has_image": true,
                 "image_url": "https://cdn.artstation.com.evil.example/b.jpg"},
                {"id": 16, "asset_type": "image", "has_image": true,
                 "image_url": "https://cdna.artstation.com/p/assets/images/16/large/d.jpeg"}]})
    }

    #[test]
    fn parses_project_image_assets() {
        let body = project("LQVJr").to_string();
        let mut items = parse_project(body.as_bytes(), "LQVJr").unwrap();
        // Like upstream, every asset with an image is downloaded (covers too).
        assert_eq!(items.len(), 3);
        assert_eq!(items.remove(0).filename, "LQVJr_11.jpg");
        assert_eq!(items[0].filename, "LQVJr_12.png");
        assert_eq!(items[1].filename, "LQVJr_16.jpg");
        assert!(items[0].url.as_str().ends_with("/large/a.png?1700"));
        let meta = items[0].metadata.as_ref().unwrap();
        assert_eq!(
            (meta.post_id.as_str(), meta.media_id.as_str()),
            ("LQVJr", "12")
        );
        assert_eq!(meta.username.as_deref(), Some("alice"));
        assert_eq!(meta.caption.as_deref(), Some("Ship"));
        assert_eq!((meta.timestamp, meta.index), (Some(1704067200), 2));
        assert_eq!(items[1].metadata.as_ref().unwrap().index, 6);
        let file = meta.file.as_ref().unwrap();
        assert_eq!((file.width, file.height), (Some(1920), Some(1080)));
        let key = items[0].archive_key().unwrap();
        assert!(key.starts_with("artstation:"), "{key}");
        assert_ne!(items[1].archive_key(), Some(key));
        assert!(parse_project(body.as_bytes(), "other").is_err());
        assert!(parse_project(b"<html>", "LQVJr").is_err());
        let listing = json!({"data": [{"hash_id": "a"}, {"hash_id": "b/../c"}], "total_count": 3});
        assert_eq!(
            parse_listing(listing.to_string().as_bytes()).unwrap(),
            (vec!["a".to_owned()], 3)
        );
    }

    #[test]
    fn classifies_supported_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        let p = Target::Project {
            hash: "LQVJr".into(),
        };
        assert_eq!(t("https://www.artstation.com/artwork/LQVJr").unwrap(), p);
        assert_eq!(t("https://alice.artstation.com/projects/LQVJr").unwrap(), p);
        let listing = |path: &str| Target::Listing { path: path.into() };
        assert_eq!(
            t("https://www.artstation.com/huimeiye/albums/770899").unwrap(),
            listing("users/huimeiye/projects.json?album_id=770899")
        );
        assert_eq!(
            t("https://fede-x-rojas.artstation.com/albums/8533110").unwrap(),
            listing("users/fede-x-rojas/projects.json?album_id=8533110")
        );
        assert_eq!(
            t("https://www.artstation.com/mikf/likes").unwrap(),
            listing("users/mikf/likes.json")
        );
        assert_eq!(
            t("https://www.artstation.com/mikf/collections/2647023").unwrap(),
            listing("collections/2647023/projects.json?collection_id=2647023")
        );
        assert_eq!(
            t("https://www.artstation.com/contests/beyond-human/challenges/23?sorting=winners")
                .unwrap(),
            Target::Challenge {
                id: "23".into(),
                sorting: "winners".into()
            }
        );
        assert_eq!(
            t("https://www.artstation.com/challenges/beyond-human/categories/23/submissions")
                .unwrap(),
            Target::Challenge {
                id: "23".into(),
                sorting: "popular".into()
            }
        );
        assert_eq!(t("https://artstn.co/p/LQVJr").unwrap(), p);
        assert_eq!(
            t("https://www.artstation.com/sungchoi/following").unwrap(),
            Target::Following {
                user: "sungchoi".into()
            }
        );
        assert_eq!(
            t("https://www.artstation.com/mikf/collections").unwrap(),
            Target::Collections {
                user: "mikf".into()
            }
        );
        assert_eq!(
            t("https://www.artstation.com/artwork?sorting=latest").unwrap(),
            listing("projects.json?sorting=latest")
        );
        assert_eq!(
            t("https://alice.artstation.com/projects/").unwrap(),
            Target::User {
                user: "alice".into()
            }
        );
        let u = Target::User {
            user: "alice".into(),
        };
        assert_eq!(t("https://www.artstation.com/alice").unwrap(), u);
        assert_eq!(t("https://www.artstation.com/alice/albums/all").unwrap(), u);
        assert_eq!(t("https://alice.artstation.com/").unwrap(), u);
        for raw in [
            "https://www.artstation.com/",
            "https://www.artstation.com/search",
            "https://www.artstation.com/artwork/",
            "https://cdna.artstation.com/p/assets/images/1/large/a.jpg",
            "https://www.artstation.com.evil.example/alice",
            "https://www.artstation.com:444/alice",
        ] {
            assert!(t(raw).is_err(), "{raw}");
        }
        assert!(media_url("https://cdn.artstation.com/a.jpg").is_some());
        assert!(media_url("https://www.artstation.com/a.jpg").is_none());
        assert!(media_url("http://cdna.artstation.com/a.jpg").is_none());
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

    fn artstation(root: Url) -> ArtStation {
        ArtStation::new(Duration::from_secs(5))
            .unwrap()
            .with_network(&NetworkOptions::new(Some(String::new()), None).unwrap())
            .unwrap()
            .with_api_root(root)
    }

    #[tokio::test]
    async fn fetches_project_with_retry_and_browser_headers() {
        let (root, server) = serve(vec![
            response("503 Service Unavailable", b""),
            ok(project("LQVJr")),
        ]);
        let events: Vec<_> = artstation(root)
            .with_retries(1)
            .events(&Url::parse("https://www.artstation.com/artwork/LQVJr").unwrap())
            .collect()
            .await;
        assert_eq!(events.len(), 9);
        for (position, group) in events.as_chunks::<3>().0.iter().enumerate() {
            let ExtractionEvent::Post(post) = group[0].as_ref().unwrap() else {
                panic!("one source Directory per asset")
            };
            assert_eq!(post.count, Some(6));
            let KeywordPolicy::Shared {
                file_fields,
                post_fields,
                ..
            } = &post.keywords
            else {
                panic!("ArtStation asset must reuse Directory keywords")
            };
            assert_eq!(post_fields["num"], [1, 2, 6][position]);
            let filter = crate::filter::FilterExpression::parse("num == 2 and count == 6").unwrap();
            assert_eq!(filter.matches_post(post).unwrap(), position == 1);
            assert_eq!(
                file_fields.as_ref(),
                &["num", "index", "media_id", "filename", "name", "extension"]
            );
            assert!(matches!(group[1], Ok(ExtractionEvent::Media(_))));
            assert!(matches!(group[2], Ok(ExtractionEvent::EndPost)));
        }
        let requests = server.join().unwrap();
        let request = requests[1].to_ascii_lowercase();
        assert!(
            request.starts_with("get /projects/lqvjr.json "),
            "{request}"
        );
        assert!(request.contains("user-agent: mozilla/5.0"));
        assert!(request.contains("referer: https://www.artstation.com/"));
    }

    #[tokio::test]
    async fn paginates_portfolio_and_reports_blocks() {
        let (root, server) = serve(vec![
            ok(json!({"data": [{"hash_id": "a"}], "total_count": 2})),
            ok(project("a")),
            ok(json!({"data": [{"hash_id": "b"}], "total_count": 2})),
            ok(project("b")),
        ]);
        let media: Vec<_> = artstation(root)
            .stream(&Url::parse("https://www.artstation.com/alice").unwrap())
            .collect()
            .await;
        let media: Vec<_> = media.into_iter().map(|m| m.unwrap()).collect();
        assert_eq!(media.len(), 6);
        let limited = artstation(
            serve(vec![
                ok(json!({"data": [{"hash_id": "a"}, {"hash_id": "b"}], "total_count": 9})),
                ok(project("a")),
            ])
            .0,
        )
        .with_options(Some(&serde_json::Map::from_iter([(
            "max-posts".to_owned(),
            json!(1),
        )])))
        .stream(&Url::parse("https://www.artstation.com/alice").unwrap())
        .collect::<Vec<_>>()
        .await;
        assert_eq!(limited.len(), 3);
        assert_eq!(media[4].filename, "b_12.png");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /users/alice/projects.json?page=1 "));
        assert!(requests[2].starts_with("GET /users/alice/projects.json?page=2 "));

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

    #[tokio::test]
    async fn search_posts_queries_with_a_csrf_token() {
        let (root, server) = serve(vec![
            ok(json!({"public_csrf_token": "tok"})),
            ok(json!({"data": [{"hash_id": "a"}], "total_count": 1})),
            ok(project("a")),
        ]);
        let media: Vec<_> = artstation(root)
            .stream(
                &Url::parse(
                    "https://www.artstation.com/search?query=ancient&sort_by=rank&tags=a,b&medium_ids=7",
                )
                .unwrap(),
            )
            .collect()
            .await;
        assert_eq!(media.len(), 3, "{media:?}");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("POST /api/v2/csrf_protection/token.json "));
        assert!(requests[1].starts_with("POST /api/v2/search/projects.json "));
        assert!(
            requests[1]
                .to_ascii_lowercase()
                .contains("public-csrf-token: tok")
        );
        let body: Value =
            serde_json::from_str(requests[1].split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body["query"], "ancient");
        assert_eq!(body["sorting"], "rank");
        assert_eq!(body["page"], 1);
        assert_eq!(body["pro_first"], "1");
        assert_eq!(
            body["filters"],
            json!([{"field": "tags", "method": "include", "value": ["a", "b"]},
                {"field": "medium_ids", "method": "include", "value": ["7"]}])
        );
    }

    #[tokio::test]
    async fn resolves_embeds_and_upgrades_images_to_8k() {
        let image = "https://cdna.artstation.com/p/assets/images/images/008/760/279/large/x.jpg?1";
        let project = json!({"hash_id": "e1", "title": "T", "user": {"username": "u"}, "assets": [
            {"id": 1, "asset_type": "image", "has_image": true, "image_url": image},
            {"id": 2, "asset_type": "video_clip", "has_image": true, "has_embedded_player": true,
             "image_url": "https://cdna.artstation.com/p/assets/video_clips/images/1/large/c.jpg",
             "player_embedded": "<iframe src=\"https://www.artstation.com/api/v2/animation/video_clips/2/player\"></iframe>"},
            {"id": 3, "asset_type": "marmoset", "has_image": true, "has_embedded_player": true,
             "image_url": "https://cdna.artstation.com/p/assets/marmosets/images/3/large/m.jpg",
             "player_embedded": "<iframe src='https://www.artstation.com/embed/3'></iframe>"},
            {"id": 4, "asset_type": "video", "has_image": true, "has_embedded_player": true,
             "image_url": "https://cdna.artstation.com/p/assets/covers/4/large/y.jpg",
             "player_embedded": "<iframe src=\"https://www.youtube.com/embed/JNFfJtwwrU0\"></iframe>"}]});
        let (root, server) = serve(vec![
            ok(project),
            response("200 OK", br#"<video id="video"><source src="https://cdn.artstation.com/p/video_sources/000/819/843/infection-4.mp4" type="video/mp4"></video>"#),
            response("200 OK", br#"marmoset.embed("https://cdna.artstation.com/p/assets/marmosets/attachments/010/915/068/original/Orca.mview?1526922111", {})"#),
        ]);
        let media: Vec<_> = artstation(root)
            .with_options(Some(&serde_json::Map::from_iter([(
                "external".to_owned(),
                json!(true),
            )])))
            .stream(&Url::parse("https://www.artstation.com/artwork/e1").unwrap())
            .collect()
            .await;
        let media: Vec<_> = media.into_iter().map(|m| m.unwrap()).collect();
        let names: Vec<_> = media.iter().map(|m| m.filename.as_str()).collect();
        assert_eq!(names, ["e1_1.jpg", "e1_2.mp4", "e1_3.mview", "e1_4.mp4"]);
        assert_eq!(
            media[0].url.as_str(),
            "https://cdna.artstation.com/p/assets/images/images/008/760/279/8k/x.jpg?1"
        );
        assert_eq!(
            media[0].handoff["_fallback"][0],
            image.replace("/large/", "/4k/")
        );
        assert!(media[1].url.as_str().ends_with("/infection-4.mp4"));
        assert!(media[2].url.as_str().ends_with("/Orca.mview?1526922111"));
        assert!(matches!(&media[3].job, MediaJob::VideoPage { page_url }
            if page_url.as_str() == "https://www.youtube.com/embed/JNFfJtwwrU0"));
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /api/v2/animation/video_clips/2/player "));
        assert!(requests[2].starts_with("GET /embed/3 "));
    }
}
