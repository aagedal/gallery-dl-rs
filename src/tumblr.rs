//! Experimental native Tumblr blog, post and tag extraction.
//!
//! Mirrors gallery_dl/extractor/tumblr.py using the public API v2 with an
//! `api_key` query parameter (upstream's default public key unless the
//! `api-key` setting overrides it) and the legacy (non-NPF) post format.
//! Photo posts yield `photos[].original_size`, video posts `video_url`, audio
//! posts a direct `audio_url`, and every post its inline `<img>` images.
//! Reblogs are included, as upstream does by default. Media must be served
//! over HTTPS from `*.media.tumblr.com` (audio also from `a.tumblr.com`).
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
use reqwest::{Client, StatusCode};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    sync::Arc,
    time::{Duration, SystemTime},
};
use url::Url;

const USER_AGENT: &str = concat!(
    "gallery-dl-rs/",
    env!("CARGO_PKG_VERSION"),
    " (native Tumblr extractor)"
);
/// Upstream gallery-dl's default public API key.
pub const DEFAULT_API_KEY: &str = "O3hU2tMi5e4Qs5t3vezEi6L0qRORJ5y9oUpSGsrWu8iA3UCc3B";
const MAX_RESPONSE: usize = 32 * 1024 * 1024;
const PAGE_SIZE: usize = 50;
const MAX_PAGES: usize = 200;
/// `*.tumblr.com` labels that are services rather than blogs.
const RESERVED_HOSTS: [&str; 12] = [
    "www", "api", "media", "assets", "static", "a", "va", "vt", "vtt", "ve", "help", "embed",
];
/// First `www.tumblr.com` path segments that are site pages, not blogs.
const RESERVED_PATHS: [&str; 30] = [
    "dashboard",
    "explore",
    "search",
    "tagged",
    "settings",
    "login",
    "register",
    "likes",
    "following",
    "inbox",
    "new",
    "blog",
    "communities",
    "about",
    "help",
    "privacy",
    "policy",
    "docs",
    "apps",
    "tv",
    "oauth",
    "logout",
    "reblog",
    "edit",
    "customize",
    "safe-mode",
    "live",
    "messaging",
    "activity",
    "post",
];

/// Blog names are 1-32 ASCII letters, digits and inner hyphens.
pub fn valid_blog(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-')
}

pub fn valid_id(id: &str) -> bool {
    (1..=24).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_digit())
}

/// Media IDs are the first 16 hex digits of the SHA-256 of the media path.
pub fn valid_media_id(id: &str) -> bool {
    id.len() == 16 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Blog name for a `BLOG.tumblr.com` host; service hosts are not blogs.
fn host_blog(host: &str) -> Option<&str> {
    let blog = host.strip_suffix(".tumblr.com")?;
    (!blog.contains('.') && !RESERVED_HOSTS.contains(&blog) && valid_blog(blog)).then_some(blog)
}

fn www_blog<'a>(segments: &[&'a str]) -> Option<&'a str> {
    let first = *segments.first()?;
    (!RESERVED_PATHS.contains(&first) && valid_blog(first)).then_some(first)
}

/// Whether a URL is a Tumblr blog page handled by this extractor.
pub fn is_blog_url(url: &Url) -> bool {
    match url.host_str() {
        Some("www.tumblr.com" | "tumblr.com") => www_blog(&segments(url)).is_some(),
        Some(host) => host_blog(host).is_some(),
        None => false,
    }
}

fn segments(url: &Url) -> Vec<&str> {
    url.path_segments()
        .map(|s| s.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Posts { blog: String },
    Post { blog: String, id: String },
    Tagged { blog: String, tag: String },
}

fn percent_decode(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn tag_of(raw: &str) -> Option<String> {
    // Upstream: text.unquote(tag.replace("-", " ")).
    let tag = percent_decode(&raw.replace(['-', '+'], " "))?;
    let tag = tag.trim();
    ((1..=256).contains(&tag.len()) && !tag.chars().any(char::is_control)).then(|| tag.into())
}

pub fn target(url: &Url) -> Result<Target> {
    let unsupported = || {
        error(
            "unsupported Tumblr URL; expected https://BLOG.tumblr.com[/post/ID|/tagged/TAG] or https://www.tumblr.com/BLOG[/ID]",
        )
    };
    if !matches!(url.scheme(), "http" | "https")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(unsupported());
    }
    let host = url.host_str().ok_or_else(unsupported)?;
    let all = segments(url);
    let (blog, rest) = if matches!(host, "www.tumblr.com" | "tumblr.com") {
        (www_blog(&all).ok_or_else(unsupported)?, &all[1..])
    } else {
        (host_blog(host).ok_or_else(unsupported)?, &all[..])
    };
    let blog = blog.to_ascii_lowercase();
    match rest {
        [] | ["posts"] => Ok(Target::Posts { blog }),
        ["post", id] | ["post", id, _] if valid_id(id) => Ok(Target::Post {
            blog,
            id: (*id).into(),
        }),
        [id] | [id, _] if valid_id(id) && matches!(host, "www.tumblr.com" | "tumblr.com") => {
            Ok(Target::Post {
                blog,
                id: (*id).into(),
            })
        }
        ["tagged", tag] => Ok(Target::Tagged {
            blog,
            tag: tag_of(tag).ok_or_else(unsupported)?,
        }),
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

fn id_of(post: &Value) -> Option<String> {
    match post.get("id_string").or_else(|| post.get("id"))? {
        Value::String(s) => Some(s.trim().to_owned()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
    .filter(|id| valid_id(id))
}

/// HTTPS media on `*.media.tumblr.com` (or `a.tumblr.com` for audio).
fn media_url(raw: &str, audio: bool) -> Option<Url> {
    let url = Url::parse(&raw.trim().replace("&amp;", "&")).ok()?;
    let host = url.host_str()?;
    (url.scheme() == "https"
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && (host.ends_with(".media.tumblr.com") || audio && host == "a.tumblr.com"))
        .then_some(url)
}

pub fn media_id(url: &Url) -> String {
    let digest = Sha256::digest(url.path().as_bytes());
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
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

/// `src` values of `<img>` tags in an HTML fragment.
fn inline_images(html: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let lower = html.to_ascii_lowercase();
    let mut pos = 0;
    while let Some(start) = lower[pos..].find("<img") {
        let start = pos + start;
        let end = lower[start..].find('>').map_or(lower.len(), |e| start + e);
        let tag = &lower[start..end];
        if let Some(src) = tag.find("src=") {
            let value = start + src + 4;
            if let Some(quote) = html[value..]
                .chars()
                .next()
                .filter(|c| matches!(c, '"' | '\''))
                && let Some(len) = html[value + 1..end].find(quote)
            {
                out.push(&html[value + 1..value + 1 + len]);
            }
        }
        pos = end;
    }
    out
}

fn dimension(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(Value::as_u64).filter(|v| *v > 0)
}

/// Photo URL and size: `original_size`, else the widest valid `alt_sizes`.
fn photo(photo: &Value) -> Option<(Url, Option<u64>, Option<u64>)> {
    let sized = |size: &Value| {
        let url = media_url(string(size, "url")?, false)?;
        Some((url, dimension(size, "width"), dimension(size, "height")))
    };
    photo.get("original_size").and_then(sized).or_else(|| {
        photo
            .get("alt_sizes")?
            .as_array()?
            .iter()
            .filter_map(sized)
            .max_by_key(|(_, width, _)| width.unwrap_or(0))
    })
}

/// URL, media type, width, height and fallback extension.
type File<'a> = (Url, &'a str, Option<u64>, Option<u64>, &'a str);

/// Parse one legacy-format post into its deduplicated media files.
pub fn parse_post(post: &Value, blog: &str) -> Vec<Media> {
    let Some(id) = id_of(post) else {
        return Vec::new();
    };
    let username: Arc<str> = Arc::from(
        string(post, "blog_name")
            .filter(|name| valid_blog(name))
            .unwrap_or(blog),
    );
    let caption = string(post, "summary")
        .map(str::to_owned)
        .or_else(|| string(post, "caption").map(crate::mastodon::strip_html))
        .filter(|text| !text.is_empty())
        .map(Arc::<str>::from);
    let timestamp = post.get("timestamp").and_then(Value::as_i64);
    let mut files: Vec<File> = Vec::new();
    for item in post
        .get("photos")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some((url, width, height)) = photo(item) {
            files.push((url, "photo", width, height, "jpg"));
        }
    }
    if let Some(url) = string(post, "video_url").and_then(|raw| media_url(raw, false)) {
        files.push((url, "video", None, None, "mp4"));
    }
    if let Some(url) = string(post, "audio_url").and_then(|raw| media_url(raw, true)) {
        files.push((url, "audio", None, None, "mp3"));
    }
    for key in ["body", "caption", "answer", "text", "description"] {
        for src in string(post, key).map(inline_images).unwrap_or_default() {
            if let Some(url) = media_url(src, false) {
                files.push((url, "photo", None, None, "jpg"));
            }
        }
    }
    let mut seen = HashSet::new();
    let mut media = Vec::new();
    for (url, kind, width, height, fallback) in files {
        if !seen.insert(url.path().to_owned()) {
            continue;
        }
        let index = media.len() + 1;
        let ext = extension(&url, fallback);
        media.push(Media {
            filename: format!("{id}_{index}.{ext}"),
            job: MediaJob::DirectHttp,
            category: "tumblr".into(),
            metadata: Some(PostMetadata {
                file: Some(FileMetadata {
                    width,
                    height,
                    media_type: Some(Arc::from(kind)),
                    ..FileMetadata::default()
                }),
                post_id: id.clone(),
                media_id: media_id(&url),
                username: Some(username.clone()),
                caption: caption.clone(),
                timestamp,
                index,
                ..PostMetadata::default()
            }),
            url,
            headers: BTreeMap::new(),
            handoff: BTreeMap::new(),
        });
    }
    media
}

/// One API page: per-post media, the number of posts and the reported total.
pub struct Page {
    pub posts: Vec<Vec<Media>>,
    pub count: usize,
    pub total: Option<u64>,
}

pub fn parse_page(bytes: &[u8], blog: &str) -> Result<Page> {
    let body: Value =
        serde_json::from_slice(bytes).map_err(|_| error("Tumblr returned invalid JSON"))?;
    let response = body.get("response").unwrap_or(&body);
    let entries = response
        .get("posts")
        .and_then(Value::as_array)
        .ok_or_else(|| error("Tumblr response contains no post list"))?;
    Ok(Page {
        posts: entries
            .iter()
            .map(|post| parse_post(post, blog))
            .filter(|media| !media.is_empty())
            .collect(),
        count: entries.len(),
        total: response.get("total_posts").and_then(Value::as_u64),
    })
}

/// Parse a `?id=` response, requiring the requested post.
pub fn parse_single(bytes: &[u8], blog: &str, id: &str) -> Result<Vec<Media>> {
    let page = parse_page(bytes, blog)?;
    let media = page
        .posts
        .into_iter()
        .find(|media| media[0].metadata.as_ref().is_some_and(|m| m.post_id == id));
    match media {
        Some(media) => Ok(media),
        None if page.count == 0 => Err(error("Tumblr post not found")),
        None => Err(error("Tumblr post contains no supported media")),
    }
}

pub struct Tumblr {
    client: Client,
    network: NetworkOptions,
    api_root: Url,
    api_key: String,
    timeout: Duration,
    request_timeout: Duration,
    retries: u8,
    rate_limits: RateLimits,
}

struct ListingState {
    blog: String,
    tag: Option<String>,
    offset: usize,
    pages: usize,
    done: bool,
    pending: VecDeque<ExtractionEvent>,
}

impl Tumblr {
    pub fn new(timeout: Duration) -> Result<Self> {
        Ok(Self {
            client: NetworkOptions::default().client(USER_AGENT, timeout)?,
            network: NetworkOptions::default(),
            api_root: Url::parse("https://api.tumblr.com/").expect("static URL"),
            api_key: DEFAULT_API_KEY.into(),
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

    async fn call(&self, blog: &str, query: &[(&str, &str)]) -> Result<Vec<u8>> {
        let mut endpoint = self
            .api_root
            .join(&format!("v2/blog/{blog}.tumblr.com/posts"))
            .expect("validated Tumblr blog");
        endpoint
            .query_pairs_mut()
            .append_pair("api_key", &self.api_key)
            .append_pair("npf", "false")
            .append_pair("reblog_info", "true")
            .extend_pairs(query);
        tokio::time::timeout(self.timeout, self.request(&endpoint))
            .await
            .map_err(|_| error("native Tumblr extraction timed out"))?
    }

    fn finish(&self, mut media: Vec<Media>) -> VecDeque<ExtractionEvent> {
        for item in &mut media {
            self.network.apply_media(item);
        }
        group(media, "tumblr")
    }

    async fn next_page(&self, state: &mut ListingState) -> Result<()> {
        let offset = state.offset.to_string();
        let limit = PAGE_SIZE.to_string();
        let mut query = vec![("offset", offset.as_str()), ("limit", limit.as_str())];
        if let Some(tag) = &state.tag {
            query.insert(0, ("tag", tag.as_str()));
        }
        let body = self.call(&state.blog, &query).await?;
        let page = parse_page(&body, &state.blog)?;
        state.pages += 1;
        state.offset += page.count;
        // The API may cap `limit` below 50, so stop on an empty page or the total.
        state.done = page.count == 0
            || page.total.is_some_and(|total| state.offset as u64 >= total)
            || state.pages >= MAX_PAGES;
        for media in page.posts {
            let events = self.finish(media);
            state.pending.extend(events);
        }
        Ok(())
    }

    pub fn stream(&self, url: &Url) -> BoxStream<'_, Result<Media>> {
        crate::event::media_only(self.events(url))
    }

    /// Emit one post boundary per post; listings paginate lazily.
    pub fn events(&self, url: &Url) -> BoxStream<'_, Result<ExtractionEvent>> {
        let target = match target(url) {
            Ok(target) => target,
            Err(e) => return stream::once(async move { Err(e) }).boxed(),
        };
        let (blog, tag) = match target {
            Target::Post { blog, id } => {
                return stream::once(async move {
                    let body = self.call(&blog, &[("id", &id)]).await?;
                    let media = parse_single(&body, &blog, &id)?;
                    Ok::<_, crate::Error>(stream::iter(self.finish(media).into_iter().map(Ok)))
                })
                .map(|result| match result {
                    Ok(events) => events.boxed(),
                    Err(e) => stream::once(async move { Err(e) }).boxed(),
                })
                .flatten()
                .boxed();
            }
            Target::Posts { blog } => (blog, None),
            Target::Tagged { blog, tag } => (blog, Some(tag)),
        };
        let state = ListingState {
            blog,
            tag,
            offset: 0,
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
                .send()
                .await
                .map_err(|_| Failure::transient("Tumblr request failed; check network access"))?;
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
                            format!("Tumblr rate limit; retry after {} seconds", delay.as_secs())
                        }
                        None => "Tumblr rate limit; retry later".into(),
                    }
                } else {
                    format!("Tumblr returned HTTP {status}")
                },
            ));
        }
        if matches!(status.as_u16(), 404 | 410) {
            return Err(error("Tumblr blog or post not found").into());
        }
        if matches!(status.as_u16(), 401 | 403) {
            return Err(error("Tumblr refused access; check the api-key setting").into());
        }
        if !status.is_success() {
            return Err(error(format!("Tumblr returned HTTP {status}")).into());
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| Failure::transient("could not read Tumblr response"))?
        {
            if body.len() + chunk.len() > MAX_RESPONSE {
                return Err(error("Tumblr response exceeded 32 MiB").into());
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

    const PHOTO: &str = "https://64.media.tumblr.com/abc/def/s2048x3072/one.jpg";

    fn post(id: u64) -> Value {
        json!({"id": id, "id_string": id.to_string(), "blog_name": "alice", "type": "photo",
            "timestamp": 1700000000, "summary": "Hi there",
            "caption": "<p>Hi <b>there</b> <img src=\"https://64.media.tumblr.com/x/inline.png\"></p>",
            "photos": [
                {"original_size": {"url": PHOTO, "width": 2048, "height": 1536}},
                {"original_size": {"url": "http://64.media.tumblr.com/insecure.jpg"},
                 "alt_sizes": [
                    {"url": "https://64.media.tumblr.com/s/small.jpg", "width": 100},
                    {"url": "https://64.media.tumblr.com/s/big.jpg", "width": 500, "height": 400}]},
                {"original_size": {"url": "https://evil.example/x.jpg"}},
                {"original_size": {"url": PHOTO}}],
            "video_url": "https://va.media.tumblr.com/tumblr_v.mp4",
            "audio_url": "https://www.example.com/song.mp3"})
    }

    #[test]
    fn parses_legacy_post_media() {
        let media = parse_post(&post(7), "fallback");
        let urls: Vec<_> = media.iter().map(|m| m.url.as_str()).collect();
        assert_eq!(
            urls,
            [
                PHOTO,
                "https://64.media.tumblr.com/s/big.jpg",
                "https://va.media.tumblr.com/tumblr_v.mp4",
                "https://64.media.tumblr.com/x/inline.png",
            ]
        );
        assert_eq!(media[0].filename, "7_1.jpg");
        assert_eq!(media[2].filename, "7_3.mp4");
        assert_eq!(media[3].filename, "7_4.png");
        let meta = media[0].metadata.as_ref().unwrap();
        assert_eq!(meta.post_id, "7");
        assert_eq!(meta.username.as_deref(), Some("alice"));
        assert_eq!(meta.caption.as_deref(), Some("Hi there"));
        assert_eq!(meta.timestamp, Some(1700000000));
        assert!(valid_media_id(&meta.media_id));
        let file = meta.file.as_ref().unwrap();
        assert_eq!((file.width, file.height), (Some(2048), Some(1536)));
        let key = media[0].archive_key().unwrap();
        assert!(key.starts_with("tumblr:"), "{key}");
        assert_ne!(Some(key.clone()), media[1].archive_key());
        assert_ne!(Some(key), parse_post(&post(8), "alice")[0].archive_key());
        let audio = parse_post(
            &json!({"id": 9, "audio_url": "https://a.tumblr.com/tumblr_a.mp3"}),
            "bob",
        );
        assert_eq!(audio[0].filename, "9_1.mp3");
        assert_eq!(
            audio[0].metadata.as_ref().unwrap().username.as_deref(),
            Some("bob")
        );
    }

    #[test]
    fn parses_pages_and_single_posts() {
        let body = json!({"meta": {"status": 200},
            "response": {"posts": [post(1), {"id": 2, "type": "text"}], "total_posts": 2}})
        .to_string();
        let page = parse_page(body.as_bytes(), "alice").unwrap();
        assert_eq!((page.posts.len(), page.count, page.total), (1, 2, Some(2)));
        assert_eq!(
            parse_single(body.as_bytes(), "alice", "1").unwrap().len(),
            4
        );
        assert!(
            parse_single(body.as_bytes(), "alice", "2")
                .unwrap_err()
                .to_string()
                .contains("no supported media")
        );
        let empty = json!({"response": {"posts": []}}).to_string();
        assert!(parse_single(empty.as_bytes(), "alice", "1").is_err());
        assert!(parse_page(b"{}", "alice").is_err());
        assert!(parse_page(b"nope", "alice").is_err());
    }

    #[test]
    fn classifies_supported_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        let posts = Target::Posts {
            blog: "alice".into(),
        };
        let single = Target::Post {
            blog: "alice".into(),
            id: "123".into(),
        };
        assert_eq!(t("https://alice.tumblr.com").unwrap(), posts);
        assert_eq!(t("https://alice.tumblr.com/posts").unwrap(), posts);
        assert_eq!(t("https://www.tumblr.com/alice").unwrap(), posts);
        assert_eq!(t("https://alice.tumblr.com/post/123").unwrap(), single);
        assert_eq!(t("https://alice.tumblr.com/post/123/slug").unwrap(), single);
        assert_eq!(t("https://www.tumblr.com/alice/123").unwrap(), single);
        assert_eq!(t("https://www.tumblr.com/alice/123/slug").unwrap(), single);
        assert_eq!(
            t("https://alice.tumblr.com/tagged/cute-cats%21").unwrap(),
            Target::Tagged {
                blog: "alice".into(),
                tag: "cute cats!".into()
            }
        );
        for raw in [
            "https://www.tumblr.com/",
            "https://www.tumblr.com/dashboard",
            "https://www.tumblr.com/tagged/cats",
            "https://64.media.tumblr.com/abc/one.jpg",
            "https://media.tumblr.com/x.jpg",
            "https://api.tumblr.com/v2/blog",
            "https://alice.tumblr.com/post/abc",
            "https://alice.tumblr.com/about",
            "https://alice.tumblr.com:444/",
            "https://tumblr.com.evil.example/alice",
            "https://alice.tumblr.com.evil.example/",
        ] {
            assert!(t(raw).is_err(), "{raw}");
        }
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

    fn tumblr(root: Url) -> Tumblr {
        Tumblr::new(Duration::from_secs(5))
            .unwrap()
            .with_network(&NetworkOptions::new(Some(String::new()), None).unwrap())
            .unwrap()
            .with_api_root(root)
    }

    #[tokio::test]
    async fn fetches_post_with_retries_and_api_key() {
        let body = json!({"response": {"posts": [post(123)]}}).to_string();
        let (root, server) = serve(vec![
            response("503 Service Unavailable", b""),
            response("200 OK", body.as_bytes()),
        ]);
        let events: Vec<_> = tumblr(root)
            .with_retries(1)
            .with_api_key(Some("secret"))
            .events(&Url::parse("https://www.tumblr.com/alice/123").unwrap())
            .collect()
            .await;
        assert_eq!(events.len(), 6, "{events:?}");
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with(
            "GET /v2/blog/alice.tumblr.com/posts?api_key=secret&npf=false&reblog_info=true&id=123 "
        ));
    }

    #[tokio::test]
    async fn paginates_tagged_posts_by_offset() {
        let first: Vec<Value> = (1..=20).map(post).collect();
        let pages = [
            json!({"response": {"posts": first, "total_posts": 25}}),
            json!({"response": {"posts": [post(21)], "total_posts": 25}}),
            json!({"response": {"posts": []}}),
        ];
        let (root, server) = serve(
            pages
                .iter()
                .map(|page| response("200 OK", page.to_string().as_bytes()))
                .collect(),
        );
        let media: Vec<_> = tumblr(root)
            .stream(&Url::parse("https://alice.tumblr.com/tagged/a+b").unwrap())
            .collect()
            .await;
        assert_eq!(media.len(), 84);
        let requests = server.join().unwrap();
        let key = format!("api_key={DEFAULT_API_KEY}");
        assert!(requests[0].contains(&key));
        assert!(
            requests[0].contains("&tag=a+b&offset=0&limit=50 "),
            "{}",
            requests[0]
        );
        assert!(requests[1].contains("&offset=20&limit=50 "));
        assert!(requests[2].contains("&offset=21&limit=50 "));
    }

    #[tokio::test]
    async fn terminal_errors_do_not_leak_key() {
        let (root, server) = serve(vec![response("401 Unauthorized", b"{}")]);
        let error = tumblr(root)
            .with_retries(3)
            .with_api_key(Some("secret-key"))
            .stream(&Url::parse("https://alice.tumblr.com/").unwrap())
            .collect::<Vec<_>>()
            .await
            .remove(0)
            .unwrap_err()
            .to_string();
        assert!(error.contains("api-key"));
        assert!(!error.contains("secret-key") && !error.contains("127.0.0.1"));
        assert_eq!(server.join().unwrap().len(), 1);
    }
}
