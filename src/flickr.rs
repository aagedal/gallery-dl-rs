//! Experimental native Flickr photo, album and photostream extraction.
//!
//! Mirrors gallery_dl/extractor/flickr.py using the public REST API
//! (`format=json&nojsoncallback=1`) with upstream's default API key unless
//! the `api-key` setting overrides it. Single photos use
//! `flickr.photos.getInfo` and `flickr.photos.getSizes`; albums and
//! photostreams list photos with `url_*` extras and only call `getSizes` for
//! videos. Media must be served over HTTPS from `*.staticflickr.com` or, for
//! video streams, `*.flickr.com`.
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
const EXTRAS: &str = "url_o,url_k,url_h,url_l,media,date_upload,owner_name,o_dims";
/// Listing size suffixes, largest first.
const LISTING_SIZES: [&str; 4] = ["o", "k", "h", "l"];
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
    Photo { id: String },
    Album { user: String, id: String },
    User { user: String },
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
    let body = check(bytes)?;
    let sizes = body
        .pointer("/sizes/size")
        .and_then(Value::as_array)
        .ok_or_else(|| error("Flickr response contains no size list"))?;
    let candidates = sizes.iter().filter_map(|size| {
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
        Some((
            is_video,
            rank,
            area,
            Size {
                url,
                width,
                height,
                video: is_video,
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
            let size = LISTING_SIZES.iter().find_map(|s| {
                let url = media_url(&field(item, &format!("url_{s}"))?, false)?;
                Some(Size {
                    url,
                    width: number(item, &format!("width_{s}")),
                    height: number(item, &format!("height_{s}")),
                    video: false,
                })
            });
            Some(Entry {
                photo: Photo {
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

/// Build the media item for a photo and its chosen size.
pub fn media(photo: &Photo, size: Size, index: usize) -> Media {
    let ext = extension(&size.url, if size.video { "mp4" } else { "jpg" });
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
            ..PostMetadata::default()
        }),
        url: size.url,
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
}

enum Listing {
    Album(String),
    User(String),
}

struct ListingState {
    listing: Option<Listing>,
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
        })
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
        parse_sizes(&body, video)
    }

    fn finish(&self, mut item: Media) -> VecDeque<ExtractionEvent> {
        self.network.apply_media(&mut item);
        group_with_keywords(
            vec![item],
            "flickr",
            KeywordPolicy::shared(&["num", "index", "media_id", "filename", "name", "extension"]),
        )
    }

    async fn photo(&self, id: &str) -> Result<VecDeque<ExtractionEvent>> {
        let body = self
            .call("flickr.photos.getInfo", &[("photo_id", id)])
            .await?;
        let photo = parse_info(&body, id)?;
        let size = self.sizes(id, photo.video).await?;
        Ok(self.finish(media(&photo, size, 1)))
    }

    async fn next_page(&self, state: &mut ListingState) -> Result<()> {
        let listing = match state.listing.take() {
            Some(listing) => listing,
            None => {
                let url = format!("https://www.flickr.com/photos/{}/", state.user);
                let body = self
                    .call("flickr.urls.lookupUser", &[("url", &url)])
                    .await?;
                let (nsid, username) = parse_user(&body)?;
                if let Some(username) = username {
                    state.user = username;
                }
                Listing::User(nsid)
            }
        };
        let page = (state.page + 1).to_string();
        let per_page = PAGE_SIZE.to_string();
        let mut query = vec![
            ("extras", EXTRAS),
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
        };
        let body = self.call(method, &query).await?;
        let parsed = parse_page(&body, &state.user)?;
        state.page += 1;
        state.done =
            parsed.entries.is_empty() || parsed.page >= parsed.pages || state.page >= MAX_PAGES;
        for entry in parsed.entries {
            let size = match entry.size {
                Some(size) if !entry.photo.video => size,
                // Videos (and photos without listed sizes) need getSizes.
                _ => self.sizes(&entry.photo.id, entry.photo.video).await?,
            };
            state.index += 1;
            let events = self.finish(media(&entry.photo, size, state.index));
            state.pending.extend(events);
        }
        state.listing = Some(listing);
        Ok(())
    }

    pub fn stream(&self, url: &Url) -> BoxStream<'_, Result<Media>> {
        crate::event::media_only(self.events(url))
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
            Target::Album { user, id } => (Some(Listing::Album(id)), user),
            Target::User { user } => (None, user),
        };
        let state = ListingState {
            listing,
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
        for raw in [
            "https://www.flickr.com/",
            "https://www.flickr.com/photos/tags/cat",
            "https://www.flickr.com/photos/alice/albums",
            "https://www.flickr.com/photos/alice/favorites",
            "https://www.flickr.com/groups/x",
            "https://flic.kr/p/0OIl",
            "https://live.staticflickr.com/65535/1_a_o.jpg",
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
        assert_eq!(media.url.as_str(), ORIGINAL);
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
            ok(sizes_body()),
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
        assert!(requests[3].contains("&page=2 "));
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
        assert!(requests[0].contains("method=flickr.photosets.getPhotos&"));
        assert!(requests[0].contains("&photoset_id=5&"));
    }
}
