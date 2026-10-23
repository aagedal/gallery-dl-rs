//! Experimental native Reddit submission, subreddit and user extraction.
//!
//! Mirrors gallery_dl/extractor/reddit.py using the unauthenticated JSON
//! endpoints (`https://www.reddit.com/....json?raw_json=1`). Files are
//! `i.redd.it` images, gallery items (`gallery_data` order with
//! `media_metadata`), `v.redd.it` fallback MP4s (video only; Reddit serves
//! audio as a separate DASH stream that is not merged), and external links to
//! direct media files. Other external sites are skipped.
use crate::{
    Result, error,
    event::{ExtractionEvent, group},
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
    collections::{BTreeMap, VecDeque},
    sync::Arc,
    time::{Duration, SystemTime},
};
use url::Url;

/// Reddit asks API clients for a unique, descriptive User-Agent.
const USER_AGENT: &str = concat!(
    "gallery-dl-rs/",
    env!("CARGO_PKG_VERSION"),
    " (native Reddit extractor)"
);
const API_ROOT: &str = "https://www.reddit.com/";
const MAX_RESPONSE: usize = 16 * 1024 * 1024;
const PAGE_LIMIT: &str = "100";
/// Bound listings to 100 pages (Reddit itself stops near 1,000 items).
const MAX_PAGES: usize = 100;

/// A supported Reddit input with validated components.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Submission {
        id: String,
    },
    Subreddit {
        name: String,
        sort: &'static str,
        time: Option<&'static str>,
    },
    User {
        name: String,
    },
}

/// Submission IDs are lowercase base36.
pub fn valid_id(id: &str) -> bool {
    (1..=16).contains(&id.len()) && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'z'))
}

/// Media IDs (gallery IDs, file stems, v.redd.it IDs) are short alphanumerics.
pub fn valid_media_id(id: &str) -> bool {
    (1..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn valid_name(name: &str) -> bool {
    (2..=32).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

/// Classify a Reddit URL. Searches, multireddits, comment permalinks for
/// other views and media hosts are rejected explicitly.
pub fn target(url: &Url) -> Result<Target> {
    let unsupported = || {
        error(
            "unsupported Reddit URL; expected a submission (/r/SUB/comments/ID, redd.it/ID), subreddit (/r/SUB[/hot|new|top|rising]) or user (/user/NAME[/submitted]) link",
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
    let segments: Vec<_> = url
        .path_segments()
        .map(|segments| segments.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    if host == "redd.it" {
        return match segments.as_slice() {
            [id] if valid_id(id) => Ok(Target::Submission { id: (*id).into() }),
            _ => Err(unsupported()),
        };
    }
    if !matches!(
        host,
        "reddit.com" | "www.reddit.com" | "old.reddit.com" | "new.reddit.com" | "np.reddit.com"
    ) {
        return Err(unsupported());
    }
    let submission = |id: &str| {
        let id = id.to_ascii_lowercase();
        valid_id(&id)
            .then_some(Target::Submission { id })
            .ok_or_else(unsupported)
    };
    let subreddit = |name: &str, sort: &str| {
        let sort = match sort {
            "hot" => "hot",
            "new" => "new",
            "top" => "top",
            "rising" => "rising",
            "controversial" => "controversial",
            _ => return Err(unsupported()),
        };
        let time = matches!(sort, "top" | "controversial")
            .then(|| {
                url.query_pairs()
                    .find(|(key, _)| key == "t")
                    .and_then(|(_, t)| {
                        ["hour", "day", "week", "month", "year", "all"]
                            .into_iter()
                            .find(|v| *v == t)
                    })
            })
            .flatten();
        valid_name(name)
            .then(|| Target::Subreddit {
                name: name.into(),
                sort,
                time,
            })
            .ok_or_else(unsupported)
    };
    let user = |name: &str| {
        valid_name(name)
            .then(|| Target::User { name: name.into() })
            .ok_or_else(unsupported)
    };
    match segments.as_slice() {
        ["r", _, "comments", id, ..] | ["comments", id, ..] => submission(id),
        ["r", name] => subreddit(name, "hot"),
        ["r", name, sort] => subreddit(name, sort),
        ["user" | "u", name] | ["user" | "u", name, "submitted"] => user(name),
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

fn extension(mime: Option<&str>, fallback: &str) -> String {
    let sub = mime
        .and_then(|mime| mime.split_once('/'))
        .map(|(_, sub)| sub.to_ascii_lowercase())
        .filter(|sub| {
            !sub.is_empty() && sub.len() <= 10 && sub.bytes().all(|b| b.is_ascii_alphanumeric())
        });
    match sub.as_deref() {
        Some("jpeg") => "jpg".into(),
        Some(sub) => sub.into(),
        None => fallback.into(),
    }
}

fn dimension(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(Value::as_u64).filter(|v| *v > 0)
}

/// Only HTTPS Reddit media hosts are trusted for gallery and video URLs.
fn reddit_media_url(raw: &str) -> Option<Url> {
    let url = Url::parse(raw).ok()?;
    let host = url.host_str()?;
    (url.scheme() == "https"
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && (host == "redd.it" || host.ends_with(".redd.it")))
    .then_some(url)
}

fn file_extension(url: &Url) -> Option<(String, String)> {
    let name = url.path_segments()?.next_back()?;
    let (stem, ext) = name.rsplit_once('.')?;
    let ext = ext.to_ascii_lowercase();
    (!ext.is_empty() && ext.len() <= 5 && ext.bytes().all(|b| b.is_ascii_alphanumeric()))
        .then(|| (stem.into(), if ext == "jpeg" { "jpg".into() } else { ext }))
}

struct Item {
    url: Url,
    media_id: String,
    ext: String,
    file: FileMetadata,
}

fn gallery(post: &Value) -> Vec<Item> {
    let metadata = post.get("media_metadata").unwrap_or(&Value::Null);
    let entries = post
        .pointer("/gallery_data/items")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut items = Vec::new();
    for entry in entries {
        let Some(id) = string(entry, "media_id").filter(|id| valid_media_id(id)) else {
            continue;
        };
        let meta = metadata.get(id).unwrap_or(&Value::Null);
        if string(meta, "status") != Some("valid") {
            continue;
        }
        let source = meta.get("s").unwrap_or(&Value::Null);
        let mime = string(meta, "m");
        let (url, ext) = if string(meta, "e") == Some("Image") {
            let ext = extension(mime, "jpg");
            match Url::parse(&format!("https://i.redd.it/{id}.{ext}")) {
                Ok(url) => (url, ext),
                Err(_) => continue,
            }
        } else {
            // Animated items: prefer the MP4 rendition, then GIF, then `u`.
            let Some(url) = ["mp4", "gif", "u"]
                .into_iter()
                .find_map(|key| string(source, key).and_then(reddit_media_url))
            else {
                continue;
            };
            let ext = file_extension(&url)
                .map(|(_, ext)| ext)
                .unwrap_or_else(|| extension(mime, "mp4"));
            (url, ext)
        };
        items.push(Item {
            url,
            media_id: id.into(),
            ext,
            file: FileMetadata {
                width: dimension(source, "x"),
                height: dimension(source, "y"),
                media_type: mime.map(Arc::from),
                description: string(entry, "caption").map(Arc::from),
                ..FileMetadata::default()
            },
        });
    }
    items
}

fn video(post: &Value) -> Option<Item> {
    let video = ["/secure_media/reddit_video", "/media/reddit_video"]
        .into_iter()
        .find_map(|pointer| post.pointer(pointer))?;
    let url = string(video, "fallback_url").and_then(reddit_media_url)?;
    if url.host_str() != Some("v.redd.it") {
        return None;
    }
    let media_id = url
        .path_segments()?
        .next()
        .filter(|id| valid_media_id(id))?
        .to_owned();
    Some(Item {
        url,
        media_id,
        ext: "mp4".into(),
        file: FileMetadata {
            width: dimension(video, "width"),
            height: dimension(video, "height"),
            duration: video.get("duration").and_then(Value::as_f64),
            bitrate: video
                .get("bitrate_kbps")
                .and_then(Value::as_u64)
                .map(|k| k * 1000),
            media_type: Some("video/mp4".into()),
            ..FileMetadata::default()
        },
    })
}

fn linked(post: &Value, id: &str) -> Option<Item> {
    let raw = string(post, "url_overridden_by_dest").or_else(|| string(post, "url"))?;
    let url = Url::parse(raw).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    let host = url.host_str()?;
    let source = post.pointer("/preview/images/0/source");
    let file = FileMetadata {
        width: source.and_then(|s| dimension(s, "width")),
        height: source.and_then(|s| dimension(s, "height")),
        ..FileMetadata::default()
    };
    if host == "i.redd.it" {
        let (stem, ext) = file_extension(&url)?;
        return valid_media_id(&stem).then_some(Item {
            url,
            media_id: stem,
            ext,
            file,
        });
    }
    if host == "v.redd.it" || host.ends_with("reddit.com") {
        return None;
    }
    let direct = crate::extract::direct(&url)?;
    if !matches!(direct.job, MediaJob::DirectHttp) {
        return None;
    }
    let (_, ext) = file_extension(&url)?;
    Some(Item {
        url,
        media_id: id.into(),
        ext,
        file,
    })
}

/// Parse a submission (`t3` data object). Deleted, removed, crossposted and
/// unsupported-link posts yield an empty list.
pub fn parse_post(post: &Value) -> Vec<Media> {
    let Some(id) = string(post, "id").filter(|id| valid_id(id)) else {
        return Vec::new();
    };
    let removed = post
        .get("removed_by_category")
        .is_some_and(|v| !v.is_null())
        || matches!(string(post, "selftext"), Some("[removed]" | "[deleted]"))
        || string(post, "author") == Some("[deleted]") && string(post, "url").is_none();
    if removed || post.get("crosspost_parent").is_some_and(|v| !v.is_null()) {
        return Vec::new();
    }
    let items = if post.get("is_gallery").and_then(Value::as_bool) == Some(true) {
        gallery(post)
    } else if let Some(item) = video(post) {
        vec![item]
    } else {
        linked(post, id).into_iter().collect()
    };
    let username: Option<Arc<str>> = string(post, "author")
        .filter(|a| *a != "[deleted]")
        .map(Arc::from);
    let caption: Option<Arc<str>> = string(post, "title").map(Arc::from);
    let timestamp = post
        .get("created_utc")
        .and_then(Value::as_f64)
        .filter(|t| t.is_finite() && *t >= 0.0)
        .map(|t| t as i64);
    items
        .into_iter()
        .enumerate()
        .map(|(i, item)| {
            let index = i + 1;
            Media {
                url: item.url,
                job: MediaJob::DirectHttp,
                category: "reddit".into(),
                filename: format!("{id}_{index}.{}", item.ext),
                metadata: Some(PostMetadata {
                    file: Some(item.file),
                    post_id: id.into(),
                    media_id: item.media_id,
                    username: username.clone(),
                    caption: caption.clone(),
                    timestamp,
                    index,
                    ..PostMetadata::default()
                }),
                headers: BTreeMap::new(),
                handoff: BTreeMap::new(),
            }
        })
        .collect()
}

fn children(listing: &Value) -> Result<&[Value]> {
    listing
        .pointer("/data/children")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .ok_or_else(|| error("Reddit response is missing its listing"))
}

/// Parse a `/comments/ID.json` response for submission `id`.
pub fn parse_submission(bytes: &[u8], id: &str) -> Result<Vec<Media>> {
    let body: Value =
        serde_json::from_slice(bytes).map_err(|_| error("Reddit returned invalid JSON"))?;
    let listing = body
        .as_array()
        .and_then(|parts| parts.first())
        .ok_or_else(|| error("Reddit response is missing its submission"))?;
    let post = children(listing)?
        .iter()
        .find(|child| string(child, "kind") == Some("t3"))
        .and_then(|child| child.get("data"))
        .ok_or_else(|| error("Reddit submission not found or removed"))?;
    if string(post, "id") != Some(id) {
        return Err(error("Reddit response does not match the requested post"));
    }
    let media = parse_post(post);
    if media.is_empty() {
        return Err(error(
            "Reddit submission contains no supported media (or was removed)",
        ));
    }
    Ok(media)
}

/// One parsed listing page: per-post files and the `after` cursor.
pub struct Page {
    pub posts: Vec<Vec<Media>>,
    pub after: Option<String>,
}

pub fn parse_listing(bytes: &[u8]) -> Result<Page> {
    let body: Value =
        serde_json::from_slice(bytes).map_err(|_| error("Reddit returned invalid JSON"))?;
    let posts = children(&body)?
        .iter()
        .filter(|child| string(child, "kind") == Some("t3"))
        .filter_map(|child| child.get("data"))
        .map(parse_post)
        .filter(|media| !media.is_empty())
        .collect();
    let after = body
        .pointer("/data/after")
        .and_then(Value::as_str)
        .filter(|a| {
            (1..=64).contains(&a.len()) && a.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        })
        .map(str::to_owned);
    Ok(Page { posts, after })
}

pub struct Reddit {
    client: Client,
    network: NetworkOptions,
    api_root: Url,
    timeout: Duration,
    request_timeout: Duration,
    retries: u8,
    rate_limits: RateLimits,
}

struct ListingState {
    path: String,
    time: Option<&'static str>,
    after: Option<String>,
    pages: usize,
    done: bool,
    pending: VecDeque<ExtractionEvent>,
}

impl Reddit {
    pub fn new(timeout: Duration) -> Result<Self> {
        Ok(Self {
            client: NetworkOptions::default().client(USER_AGENT, timeout)?,
            network: NetworkOptions::default(),
            api_root: Url::parse(API_ROOT).unwrap(),
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

    /// Apply upstream extractor settings (`extractor.reddit.*`).
    pub fn with_options(self, _options: Option<&serde_json::Map<String, Value>>) -> Result<Self> {
        Ok(self)
    }

    /// Like `events`, queueing links to other sites through `queue`.
    pub fn events_with_queue<'a>(
        &'a self,
        url: &Url,
        _queue: crate::event::Queue<'a>,
    ) -> BoxStream<'a, Result<ExtractionEvent>> {
        self.events(url)
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

    /// Each API call is bounded by the overall timeout.
    async fn call(&self, path: &str, query: &[(&str, &str)]) -> Result<Vec<u8>> {
        let mut endpoint = self.api_root.join(path).expect("validated Reddit path");
        endpoint
            .query_pairs_mut()
            .append_pair("raw_json", "1")
            .extend_pairs(query);
        tokio::time::timeout(self.timeout, self.request(&endpoint))
            .await
            .map_err(|_| error("native Reddit extraction timed out"))?
    }

    async fn next_page(&self, state: &mut ListingState) -> Result<()> {
        let mut query = vec![("limit", PAGE_LIMIT)];
        if let Some(time) = state.time {
            query.push(("t", time));
        }
        if let Some(after) = &state.after {
            query.push(("after", after));
        }
        let body = self.call(&state.path, &query).await?;
        let page = parse_listing(&body)?;
        state.pages += 1;
        state.done = page.after.is_none() || page.after == state.after || state.pages >= MAX_PAGES;
        state.after = page.after;
        for mut media in page.posts {
            for item in &mut media {
                self.network.apply_media(item);
            }
            state.pending.extend(group(media, "reddit"));
        }
        Ok(())
    }

    pub fn stream(&self, url: &Url) -> BoxStream<'_, Result<Media>> {
        crate::event::media_only(self.events(url))
    }

    /// Emit one post boundary per submission; listings paginate lazily.
    pub fn events(&self, url: &Url) -> BoxStream<'_, Result<ExtractionEvent>> {
        let target = match target(url) {
            Ok(target) => target,
            Err(e) => return stream::once(async move { Err(e) }).boxed(),
        };
        let (path, time) = match target {
            Target::Submission { id } => {
                return stream::once(async move {
                    let body = self.call(&format!("comments/{id}/.json"), &[]).await?;
                    let mut media = parse_submission(&body, &id)?;
                    for item in &mut media {
                        self.network.apply_media(item);
                    }
                    Ok::<_, crate::Error>(stream::iter(group(media, "reddit").into_iter().map(Ok)))
                })
                .map(|result| match result {
                    Ok(events) => events.boxed(),
                    Err(e) => stream::once(async move { Err(e) }).boxed(),
                })
                .flatten()
                .boxed();
            }
            Target::Subreddit { name, sort, time } => (format!("r/{name}/{sort}/.json"), time),
            Target::User { name } => (format!("user/{name}/submitted/.json"), None),
        };
        let state = ListingState {
            path,
            time,
            after: None,
            pages: 0,
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
                .header(header::ACCEPT, "application/json")
                .send()
                .await
                .map_err(|_| Failure::transient("Reddit request failed; check network access"))?;
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
                            format!("Reddit rate limit; retry after {} seconds", delay.as_secs())
                        }
                        None => "Reddit rate limit; retry later".into(),
                    }
                } else {
                    format!("Reddit returned HTTP {status}")
                },
            ));
        }
        if matches!(status.as_u16(), 404 | 410) {
            return Err(error("Reddit submission, subreddit or user not found").into());
        }
        if matches!(status.as_u16(), 401 | 403) {
            return Err(error(
                "Reddit refused access (private, quarantined or banned content, or blocked client)",
            )
            .into());
        }
        if !status.is_success() {
            return Err(error(format!("Reddit returned HTTP {status}")).into());
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| Failure::transient("could not read Reddit response"))?
        {
            if body.len() + chunk.len() > MAX_RESPONSE {
                return Err(error("Reddit response exceeded 16 MiB").into());
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

    fn image_post(id: &str) -> Value {
        json!({"id": id, "author": "alice", "title": "A title", "created_utc": 1704164645.0,
               "url": format!("https://i.redd.it/img{id}.jpg"),
               "preview": {"images": [{"source": {"url": "x", "width": 800, "height": 600}}]}})
    }

    fn submission(post: Value) -> Vec<u8> {
        json!([{"kind": "Listing", "data": {"children": [{"kind": "t3", "data": post}]}},
               {"kind": "Listing", "data": {"children": []}}])
        .to_string()
        .into_bytes()
    }

    fn listing(posts: Vec<Value>, after: Option<&str>) -> Vec<u8> {
        let children: Vec<_> = posts
            .into_iter()
            .map(|p| json!({"kind": "t3", "data": p}))
            .collect();
        json!({"kind": "Listing", "data": {"after": after, "children": children}})
            .to_string()
            .into_bytes()
    }

    #[test]
    fn parses_image_post_metadata() {
        let media = parse_submission(&submission(image_post("abc12")), "abc12").unwrap();
        assert_eq!(media.len(), 1);
        assert_eq!(media[0].url.as_str(), "https://i.redd.it/imgabc12.jpg");
        assert_eq!(media[0].filename, "abc12_1.jpg");
        let meta = media[0].metadata.as_ref().unwrap();
        assert_eq!(meta.post_id, "abc12");
        assert_eq!(meta.media_id, "imgabc12");
        assert_eq!(meta.username.as_deref(), Some("alice"));
        assert_eq!(meta.caption.as_deref(), Some("A title"));
        assert_eq!(meta.timestamp, Some(1704164645));
        assert_eq!(meta.index, 1);
        let file = meta.file.as_ref().unwrap();
        assert_eq!((file.width, file.height), (Some(800), Some(600)));
        assert!(media[0].archive_key().unwrap().starts_with("reddit:v1:"));
    }

    #[test]
    fn parses_gallery_in_order_with_captions() {
        let post = json!({"id": "gal1", "author": "bob", "title": "G", "is_gallery": true,
            "gallery_data": {"items": [
                {"media_id": "bbb", "caption": "second id first"},
                {"media_id": "aaa"},
                {"media_id": "missing"},
                {"media_id": "ccc"}]},
            "media_metadata": {
                "aaa": {"status": "valid", "e": "Image", "m": "image/png",
                        "s": {"u": "https://preview.redd.it/aaa.png?width=1&s=sig", "x": 10, "y": 20}},
                "bbb": {"status": "valid", "e": "Image", "m": "image/jpg", "s": {"x": 1, "y": 2}},
                "ccc": {"status": "valid", "e": "AnimatedImage", "m": "image/gif",
                        "s": {"gif": "https://i.redd.it/ccc.gif", "mp4": "https://evil.example/x.mp4"}}}});
        let media = parse_post(&post);
        let urls: Vec<_> = media.iter().map(|m| m.url.as_str()).collect();
        assert_eq!(
            urls,
            [
                "https://i.redd.it/bbb.jpg",
                "https://i.redd.it/aaa.png",
                "https://i.redd.it/ccc.gif"
            ]
        );
        assert_eq!(media[1].filename, "gal1_2.png");
        let meta = media[0].metadata.as_ref().unwrap();
        assert_eq!(meta.media_id, "bbb");
        assert_eq!(
            meta.file.as_ref().unwrap().description.as_deref(),
            Some("second id first")
        );
        assert_ne!(media[0].archive_key(), media[1].archive_key());
    }

    #[test]
    fn parses_reddit_video_and_external_direct_links() {
        let post = json!({"id": "vid1", "author": "c", "title": "V", "url": "https://v.redd.it/abcVID",
            "secure_media": {"reddit_video": {
                "fallback_url": "https://v.redd.it/abcVID/DASH_720.mp4?source=fallback",
                "width": 1280, "height": 720, "duration": 12}}});
        let media = parse_post(&post);
        assert_eq!(
            media[0].url.as_str(),
            "https://v.redd.it/abcVID/DASH_720.mp4?source=fallback"
        );
        assert_eq!(media[0].filename, "vid1_1.mp4");
        let meta = media[0].metadata.as_ref().unwrap();
        assert_eq!(meta.media_id, "abcVID");
        assert_eq!(meta.file.as_ref().unwrap().duration, Some(12.0));

        let external = json!({"id": "ext1", "title": "E", "url": "https://example.com/a/pic.PNG"});
        let media = parse_post(&external);
        assert_eq!(media[0].url.as_str(), "https://example.com/a/pic.PNG");
        assert_eq!(media[0].filename, "ext1_1.png");
        assert!(media[0].archive_key().is_some());
        let page = json!({"id": "ext2", "url": "https://example.com/article"});
        assert!(parse_post(&page).is_empty());
    }

    #[test]
    fn skips_removed_deleted_crossposts_and_self_posts() {
        let mut removed = image_post("r1");
        removed["removed_by_category"] = json!("moderator");
        let mut cross = image_post("r2");
        cross["crosspost_parent"] = json!("t3_abc");
        let selfpost = json!({"id": "r3", "is_self": true, "selftext": "[deleted]",
                              "url": "https://www.reddit.com/r/x/comments/r3/t/"});
        for post in [removed, cross, selfpost] {
            assert!(parse_post(&post).is_empty());
        }
        assert!(parse_submission(&submission(image_post("abc")), "other").is_err());
        assert!(parse_submission(b"nope", "abc").is_err());
    }

    #[test]
    fn classifies_supported_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        for raw in [
            "https://www.reddit.com/r/pics/comments/abc12/some_title/",
            "https://old.reddit.com/r/pics/comments/ABC12",
            "https://new.reddit.com/comments/abc12",
            "https://reddit.com/comments/abc12/x/",
            "https://redd.it/abc12",
        ] {
            assert_eq!(
                t(raw).unwrap(),
                Target::Submission { id: "abc12".into() },
                "{raw}"
            );
        }
        assert_eq!(
            t("https://www.reddit.com/r/pics/").unwrap(),
            Target::Subreddit {
                name: "pics".into(),
                sort: "hot",
                time: None
            }
        );
        assert_eq!(
            t("https://www.reddit.com/r/pics/top/?t=week").unwrap(),
            Target::Subreddit {
                name: "pics".into(),
                sort: "top",
                time: Some("week")
            }
        );
        for raw in [
            "https://www.reddit.com/user/bob",
            "https://reddit.com/u/bob/submitted/",
        ] {
            assert_eq!(t(raw).unwrap(), Target::User { name: "bob".into() });
        }
        for raw in [
            "https://www.reddit.com/r/pics/search?q=x",
            "https://www.reddit.com/user/bob/comments",
            "https://www.reddit.com/r/pics/comments/a%2Fb",
            "https://reddit.com.evil.test/r/pics",
            "https://www.reddit.com:444/r/pics",
            "https://v.redd.it/abc",
            "https://www.reddit.com/",
        ] {
            assert!(t(raw).is_err(), "{raw}");
        }
    }

    fn response(status: &str, headers: &str, body: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n",
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

    fn reddit(root: Url) -> Reddit {
        Reddit::new(Duration::from_secs(5))
            .unwrap()
            .with_network(&NetworkOptions::new(Some(String::new()), None).unwrap())
            .unwrap()
            .with_api_root(root)
    }

    #[tokio::test]
    async fn fetches_submission_with_retries_and_user_agent() {
        let (root, server) = serve(vec![
            response("429 Too Many Requests", "Retry-After: 0\r\n", b""),
            response("200 OK", "", &submission(image_post("abc12"))),
        ]);
        let events: Vec<_> = reddit(root)
            .with_retries(1)
            .events(&Url::parse("https://redd.it/abc12").unwrap())
            .collect()
            .await;
        assert_eq!(events.len(), 3, "{events:?}");
        assert!(matches!(events[1], Ok(ExtractionEvent::Media(_))));
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /comments/abc12/.json?raw_json=1 "));
        assert!(
            requests[1]
                .to_ascii_lowercase()
                .contains("user-agent: gallery-dl-rs/")
        );
    }

    #[tokio::test]
    async fn paginates_listing_with_after_cursor() {
        let (root, server) = serve(vec![
            response(
                "200 OK",
                "",
                &listing(vec![image_post("a1")], Some("t3_a1")),
            ),
            response("200 OK", "", &listing(vec![image_post("b2")], None)),
        ]);
        let media: Vec<_> = reddit(root)
            .stream(&Url::parse("https://www.reddit.com/r/pics/new").unwrap())
            .collect()
            .await;
        assert_eq!(media.len(), 2);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /r/pics/new/.json?raw_json=1&limit=100 "));
        assert!(requests[1].contains("&after=t3_a1 "));
    }

    #[tokio::test]
    async fn terminal_errors_are_not_retried() {
        let (root, server) = serve(vec![response("404 Not Found", "", b"{}")]);
        let error = reddit(root)
            .with_retries(3)
            .stream(&Url::parse("https://www.reddit.com/user/nobody").unwrap())
            .collect::<Vec<_>>()
            .await
            .remove(0)
            .unwrap_err();
        assert!(error.to_string().contains("not found"));
        assert!(!error.to_string().contains("127.0.0.1"));
        assert_eq!(server.join().unwrap().len(), 1);
    }
}
