//! Experimental native Bluesky post and author-feed extraction.
//!
//! Mirrors gallery_dl/extractor/bluesky.py: posts are read from the public,
//! unauthenticated AppView, and files are downloaded as full-size blobs via
//! `com.atproto.sync.getBlob`, with URLs built from validated DIDs and CIDs only.
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

const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
const API_ROOT: &str = "https://public.api.bsky.app/xrpc/";
const BLOB_ROOT: &str = "https://bsky.social/xrpc/com.atproto.sync.getBlob";
const MAX_RESPONSE: usize = 16 * 1024 * 1024;
const PAGE_LIMIT: &str = "100";
/// Bound author feeds to 1,000 pages (100,000 posts).
const MAX_PAGES: usize = 1000;

/// Which author feed to read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Feed {
    Media,
    Posts,
}

impl Feed {
    fn filter(self) -> &'static str {
        match self {
            Self::Media => "posts_with_media",
            Self::Posts => "posts_no_replies",
        }
    }
}

/// A supported Bluesky input; `actor` is a validated handle or DID.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Post { actor: String, rkey: String },
    Feed { actor: String, feed: Feed },
}

/// `did:plc:` (base32) or `did:web:` (hostname) identifiers.
pub fn valid_did(did: &str) -> bool {
    if let Some(id) = did.strip_prefix("did:plc:") {
        id.len() == 24 && id.bytes().all(|b| matches!(b, b'a'..=b'z' | b'2'..=b'7'))
    } else if let Some(host) = did.strip_prefix("did:web:") {
        valid_handle(host)
    } else {
        false
    }
}

fn valid_handle(handle: &str) -> bool {
    let labels: Vec<_> = handle.split('.').collect();
    handle.len() <= 253
        && labels.len() >= 2
        && labels.iter().all(|label| {
            (1..=63).contains(&label.len())
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        })
}

fn valid_actor(actor: &str) -> bool {
    valid_did(actor) || valid_handle(actor)
}

/// Record keys are TIDs or other short `[A-Za-z0-9._:~-]` strings.
fn valid_rkey(rkey: &str) -> bool {
    (1..=512).contains(&rkey.len())
        && rkey != "."
        && rkey != ".."
        && rkey
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'~' | b'-'))
}

/// Blob CIDs are CIDv1 in lowercase base32 (`b...`).
pub fn valid_cid(cid: &str) -> bool {
    (8..=128).contains(&cid.len())
        && cid.starts_with('b')
        && cid.bytes().all(|b| matches!(b, b'a'..=b'z' | b'2'..=b'7'))
}

/// Classify a Bluesky URL. Likes, feeds, lists, searches and other views are
/// rejected explicitly rather than guessed.
pub fn target(url: &Url) -> Result<Target> {
    let unsupported = || {
        error(
            "unsupported Bluesky URL; expected a post (/profile/NAME/post/ID) or profile (/profile/NAME[/media|/posts]) link",
        )
    };
    let host = url.host_str().ok_or_else(unsupported)?;
    if !matches!(url.scheme(), "http" | "https")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || !matches!(host, "bsky.app" | "www.bsky.app" | "main.bsky.dev")
    {
        return Err(unsupported());
    }
    let segments: Vec<_> = url
        .path_segments()
        .map(|segments| segments.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    let actor = |name: &str| {
        let name = name.to_ascii_lowercase();
        let name = name.strip_prefix('@').unwrap_or(&name).to_owned();
        valid_actor(&name).then_some(name).ok_or_else(unsupported)
    };
    match segments.as_slice() {
        ["profile", name, "post", rkey] if valid_rkey(rkey) => Ok(Target::Post {
            actor: actor(name)?,
            rkey: (*rkey).into(),
        }),
        ["profile", name] | ["profile", name, "media"] => Ok(Target::Feed {
            actor: actor(name)?,
            feed: Feed::Media,
        }),
        ["profile", name, "posts"] => Ok(Target::Feed {
            actor: actor(name)?,
            feed: Feed::Posts,
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

/// Media blobs of a post record's embed: images, a video, or either inside
/// `recordWithMedia`. Quoted records are not followed.
fn blobs(embed: &Value) -> Vec<(&Value, &'static str)> {
    match embed.get("$type").and_then(Value::as_str) {
        Some("app.bsky.embed.images") => embed
            .get("images")
            .and_then(Value::as_array)
            .map(|images| images.iter().map(|image| (image, "image")).collect())
            .unwrap_or_default(),
        Some("app.bsky.embed.video") => vec![(embed, "video")],
        Some("app.bsky.embed.recordWithMedia") => embed.get("media").map(blobs).unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// Parse an `app.bsky.feed.defs#postView` into its files. Posts without
/// media yield an empty list.
pub fn parse_post_view(post: &Value) -> Result<Vec<Media>> {
    let uri = string(post, "uri").ok_or_else(|| error("Bluesky post is missing its URI"))?;
    let (did, rkey) = uri
        .strip_prefix("at://")
        .and_then(|rest| rest.split_once("/app.bsky.feed.post/"))
        .filter(|(did, rkey)| valid_did(did) && valid_rkey(rkey))
        .ok_or_else(|| error("Bluesky post has an invalid URI"))?;
    if post.pointer("/author/did").and_then(Value::as_str) != Some(did) {
        return Err(error("Bluesky post author does not match its URI"));
    }
    let username: Option<Arc<str>> = post
        .get("author")
        .and_then(|author| string(author, "handle"))
        .map(Arc::from);
    let record = post.get("record").unwrap_or(&Value::Null);
    let caption: Option<Arc<str>> = string(record, "text").map(Arc::from);
    let timestamp = string(record, "createdAt").and_then(|d| crate::date::parse_date(d).ok());
    let embed = record.get("embed").unwrap_or(&Value::Null);
    let mut media = Vec::new();
    for (item, kind) in blobs(embed) {
        let blob = item.get(kind).unwrap_or(&Value::Null);
        let cid = blob
            .pointer("/ref/$link")
            .and_then(Value::as_str)
            .or_else(|| blob.get("cid").and_then(Value::as_str))
            .filter(|cid| valid_cid(cid))
            .ok_or_else(|| error("Bluesky media has an invalid blob CID"))?;
        let mime = string(blob, "mimeType");
        let ext = extension(mime, if kind == "video" { "mp4" } else { "jpg" });
        let mut url = Url::parse(BLOB_ROOT).unwrap();
        url.query_pairs_mut()
            .append_pair("did", did)
            .append_pair("cid", cid);
        let ratio = |key: &str| {
            item.get("aspectRatio")
                .and_then(|r| r.get(key))
                .and_then(Value::as_u64)
                .filter(|v| *v > 0)
        };
        let index = media.len() + 1;
        media.push(Media {
            url,
            job: MediaJob::DirectHttp,
            category: "bluesky".into(),
            filename: format!("{rkey}_{index}.{ext}"),
            metadata: Some(PostMetadata {
                file: Some(FileMetadata {
                    width: ratio("width"),
                    height: ratio("height"),
                    media_type: mime.map(Arc::from),
                    description: string(item, "alt").map(Arc::from),
                    ..FileMetadata::default()
                }),
                post_id: rkey.into(),
                media_id: cid.into(),
                username: username.clone(),
                caption: caption.clone(),
                timestamp,
                index,
                ..PostMetadata::default()
            }),
            headers: BTreeMap::new(),
            handoff: BTreeMap::new(),
        });
    }
    Ok(media)
}

/// Parse an `app.bsky.feed.getPostThread` response for `did`/`rkey`.
pub fn parse_thread(bytes: &[u8], did: &str, rkey: &str) -> Result<Vec<Media>> {
    let body: Value =
        serde_json::from_slice(bytes).map_err(|_| error("Bluesky returned invalid JSON"))?;
    let thread = body.get("thread").unwrap_or(&Value::Null);
    match thread.get("$type").and_then(Value::as_str) {
        Some("app.bsky.feed.defs#threadViewPost") => {}
        Some("app.bsky.feed.defs#blockedPost") => {
            return Err(error("Bluesky post is blocked"));
        }
        _ => return Err(error("Bluesky post not found or removed")),
    }
    let post = thread.get("post").unwrap_or(&Value::Null);
    if string(post, "uri") != Some(&format!("at://{did}/app.bsky.feed.post/{rkey}")) {
        return Err(error("Bluesky response does not match the requested post"));
    }
    let media = parse_post_view(post)?;
    if media.is_empty() {
        return Err(error("Bluesky post contains no media"));
    }
    Ok(media)
}

/// One parsed `getAuthorFeed` page: per-post files (reposts, pins and
/// media-free posts skipped) and the next cursor.
pub struct Page {
    pub posts: Vec<Vec<Media>>,
    pub cursor: Option<String>,
}

pub fn parse_feed(bytes: &[u8], did: &str) -> Result<Page> {
    let body: Value =
        serde_json::from_slice(bytes).map_err(|_| error("Bluesky returned invalid JSON"))?;
    let items = body
        .get("feed")
        .and_then(Value::as_array)
        .ok_or_else(|| error("Bluesky response is missing its feed"))?;
    let mut posts = Vec::new();
    for item in items {
        // Reposts carry a reason; pinned posts also appear in their place.
        if item.get("reason").is_some_and(|reason| !reason.is_null()) {
            continue;
        }
        let post = item.get("post").unwrap_or(&Value::Null);
        if post.pointer("/author/did").and_then(Value::as_str) != Some(did) {
            continue;
        }
        let media = parse_post_view(post)?;
        if !media.is_empty() {
            posts.push(media);
        }
    }
    let cursor = string(&body, "cursor")
        .filter(|cursor| cursor.len() <= 1024)
        .map(str::to_owned);
    Ok(Page { posts, cursor })
}

pub struct Bluesky {
    client: Client,
    network: NetworkOptions,
    api_root: Url,
    timeout: Duration,
    request_timeout: Duration,
    retries: u8,
    rate_limits: RateLimits,
}

struct FeedState {
    did: Option<String>,
    actor: String,
    feed: Feed,
    cursor: Option<String>,
    pages: usize,
    done: bool,
    pending: VecDeque<ExtractionEvent>,
}

impl Bluesky {
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

    /// Override the AppView origin; intended for local tests.
    pub fn with_api_root(mut self, root: Url) -> Self {
        self.api_root = root;
        self
    }

    fn endpoint(&self, method: &str, query: &[(&str, &str)]) -> Url {
        let mut url = self.api_root.join(method).expect("static XRPC method");
        url.query_pairs_mut().extend_pairs(query);
        url
    }

    /// Each API call is bounded by the overall timeout.
    async fn call(&self, method: &str, query: &[(&str, &str)]) -> Result<Vec<u8>> {
        let endpoint = self.endpoint(method, query);
        tokio::time::timeout(self.timeout, self.request(&endpoint))
            .await
            .map_err(|_| error("native Bluesky extraction timed out"))?
    }

    async fn resolve(&self, actor: &str) -> Result<String> {
        if valid_did(actor) {
            return Ok(actor.into());
        }
        let body = self
            .call("com.atproto.identity.resolveHandle", &[("handle", actor)])
            .await?;
        let body: Value =
            serde_json::from_slice(&body).map_err(|_| error("Bluesky returned invalid JSON"))?;
        string(&body, "did")
            .filter(|did| valid_did(did))
            .map(str::to_owned)
            .ok_or_else(|| error("Bluesky handle did not resolve to a valid DID"))
    }

    async fn post(&self, actor: &str, rkey: &str) -> Result<Vec<Media>> {
        let did = self.resolve(actor).await?;
        let uri = format!("at://{did}/app.bsky.feed.post/{rkey}");
        let body = self
            .call(
                "app.bsky.feed.getPostThread",
                &[("uri", &uri), ("depth", "0"), ("parentHeight", "0")],
            )
            .await?;
        parse_thread(&body, &did, rkey)
    }

    async fn next_page(&self, state: &mut FeedState) -> Result<()> {
        let did = match &state.did {
            Some(did) => did.clone(),
            None => {
                let did = self.resolve(&state.actor).await?;
                state.did = Some(did.clone());
                did
            }
        };
        let mut query = vec![
            ("actor", did.as_str()),
            ("filter", state.feed.filter()),
            ("limit", PAGE_LIMIT),
        ];
        if let Some(cursor) = &state.cursor {
            query.push(("cursor", cursor));
        }
        let body = self.call("app.bsky.feed.getAuthorFeed", &query).await?;
        let page = parse_feed(&body, &did)?;
        state.pages += 1;
        state.done =
            page.cursor.is_none() || page.cursor == state.cursor || state.pages >= MAX_PAGES;
        state.cursor = page.cursor;
        for mut media in page.posts {
            for item in &mut media {
                self.network.apply_media(item);
            }
            state.pending.extend(group(media, "bluesky"));
        }
        Ok(())
    }

    pub fn stream(&self, url: &Url) -> BoxStream<'_, Result<Media>> {
        crate::event::media_only(self.events(url))
    }

    /// Emit one post boundary per post; author feeds are paginated lazily.
    pub fn events(&self, url: &Url) -> BoxStream<'_, Result<ExtractionEvent>> {
        let target = match target(url) {
            Ok(target) => target,
            Err(e) => return stream::once(async move { Err(e) }).boxed(),
        };
        match target {
            Target::Post { actor, rkey } => stream::once(async move {
                let mut media = self.post(&actor, &rkey).await?;
                for item in &mut media {
                    self.network.apply_media(item);
                }
                Ok::<_, crate::Error>(stream::iter(group(media, "bluesky").into_iter().map(Ok)))
            })
            .map(|result| match result {
                Ok(events) => events.boxed(),
                Err(e) => stream::once(async move { Err(e) }).boxed(),
            })
            .flatten()
            .boxed(),
            Target::Feed { actor, feed } => {
                let state = FeedState {
                    did: None,
                    actor,
                    feed,
                    cursor: None,
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
        }
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
                .map_err(|_| Failure::transient("Bluesky request failed; check network access"))?;
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
                            format!(
                                "Bluesky rate limit; retry after {} seconds",
                                delay.as_secs()
                            )
                        }
                        None => "Bluesky rate limit; retry later".into(),
                    }
                } else {
                    format!("Bluesky returned HTTP {status}")
                },
            ));
        }
        if status == StatusCode::BAD_REQUEST {
            // XRPC reports unknown actors, handles and posts as 400 errors.
            return Err(error("Bluesky profile or post not found").into());
        }
        if matches!(status.as_u16(), 404 | 410) {
            return Err(error("Bluesky profile or post not found or removed").into());
        }
        if matches!(status.as_u16(), 401 | 403) {
            return Err(error("Bluesky refused access to this content").into());
        }
        if !status.is_success() {
            return Err(error(format!("Bluesky returned HTTP {status}")).into());
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| Failure::transient("could not read Bluesky response"))?
        {
            if body.len() + chunk.len() > MAX_RESPONSE {
                return Err(error("Bluesky response exceeded 16 MiB").into());
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

    const DID: &str = "did:plc:abcdefghijklmnopqrstuvwx";
    const CID1: &str = "bafkreiaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const CID2: &str = "bafkreibbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const CID3: &str = "bafkreicccccccccccccccccccccccccccccccccccccccccccccccccccc";

    fn image(cid: &str, alt: &str) -> Value {
        json!({"alt": alt, "aspectRatio": {"width": 800, "height": 600},
               "image": {"$type": "blob", "ref": {"$link": cid}, "mimeType": "image/jpeg", "size": 1}})
    }

    fn post(rkey: &str, embed: Value) -> Value {
        json!({
            "uri": format!("at://{DID}/app.bsky.feed.post/{rkey}"),
            "cid": "bafyreipost",
            "author": {"did": DID, "handle": "alice.bsky.social"},
            "record": {"$type": "app.bsky.feed.post", "text": "hello",
                       "createdAt": "2024-01-02T03:04:05.678Z", "embed": embed},
        })
    }

    fn thread(rkey: &str, embed: Value) -> Vec<u8> {
        json!({"thread": {"$type": "app.bsky.feed.defs#threadViewPost", "post": post(rkey, embed)}})
            .to_string()
            .into_bytes()
    }

    #[test]
    fn parses_images_with_metadata_and_blob_urls() {
        let embed = json!({"$type": "app.bsky.embed.images",
                           "images": [image(CID1, "first"), image(CID2, "")]});
        let media = parse_thread(&thread("3kabc", embed), DID, "3kabc").unwrap();
        assert_eq!(media.len(), 2);
        assert_eq!(
            media[0].url.as_str(),
            format!(
                "https://bsky.social/xrpc/com.atproto.sync.getBlob?did=did%3Aplc%3Aabcdefghijklmnopqrstuvwx&cid={CID1}"
            )
        );
        assert_eq!(media[0].filename, "3kabc_1.jpg");
        assert_eq!(media[1].filename, "3kabc_2.jpg");
        let meta = media[0].metadata.as_ref().unwrap();
        assert_eq!(meta.post_id, "3kabc");
        assert_eq!(meta.media_id, CID1);
        assert_eq!(meta.index, 1);
        assert_eq!(meta.username.as_deref(), Some("alice.bsky.social"));
        assert_eq!(meta.caption.as_deref(), Some("hello"));
        assert_eq!(meta.timestamp, Some(1704164645));
        let file = meta.file.as_ref().unwrap();
        assert_eq!((file.width, file.height), (Some(800), Some(600)));
        assert_eq!(file.description.as_deref(), Some("first"));
        assert_eq!(file.media_type.as_deref(), Some("image/jpeg"));
        assert!(
            media[1]
                .metadata
                .as_ref()
                .unwrap()
                .file
                .as_ref()
                .unwrap()
                .description
                .is_none()
        );
        assert!(media[0].archive_key().unwrap().starts_with("bluesky:v1:"));
        assert_ne!(media[0].archive_key(), media[1].archive_key());
    }

    #[test]
    fn parses_video_and_record_with_media() {
        let video = json!({"$type": "app.bsky.embed.video", "alt": "clip",
                           "aspectRatio": {"width": 1280, "height": 720},
                           "video": {"ref": {"$link": CID3}, "mimeType": "video/mp4"}});
        let media = parse_thread(&thread("3kvid", video.clone()), DID, "3kvid").unwrap();
        assert_eq!(media[0].filename, "3kvid_1.mp4");
        let wrapped = json!({"$type": "app.bsky.embed.recordWithMedia",
                             "record": {"record": {"uri": "at://x/y/z"}}, "media": video});
        let quoted = parse_thread(&thread("3kq", wrapped), DID, "3kq").unwrap();
        assert_eq!(quoted.len(), 1);
        assert_eq!(quoted[0].archive_key(), media[0].archive_key());
    }

    #[test]
    fn rejects_invalid_mismatched_and_empty_posts() {
        let embed = json!({"$type": "app.bsky.embed.images", "images": [image(CID1, "")]});
        assert!(parse_thread(&thread("3kabc", embed.clone()), DID, "other").is_err());
        let bad = json!({"$type": "app.bsky.embed.images", "images": [image("../x", "")]});
        assert!(parse_thread(&thread("3kabc", bad), DID, "3kabc").is_err());
        let none = json!({"$type": "app.bsky.embed.external", "external": {}});
        assert!(parse_thread(&thread("3kabc", none), DID, "3kabc").is_err());
        assert!(
            parse_thread(
                br#"{"thread":{"$type":"app.bsky.feed.defs#notFoundPost"}}"#,
                DID,
                "3kabc"
            )
            .is_err()
        );
        assert!(parse_thread(b"nope", DID, "3kabc").is_err());
        let mut forged = post("3kabc", embed);
        forged["author"]["did"] = json!("did:plc:zzzzzzzzzzzzzzzzzzzzzzzz");
        assert!(parse_post_view(&forged).is_err());
    }

    #[test]
    fn feed_skips_reposts_and_media_free_posts() {
        let embed = json!({"$type": "app.bsky.embed.images", "images": [image(CID1, "")]});
        let body = json!({"cursor": "c2", "feed": [
            {"post": post("3ka", embed.clone())},
            {"post": post("3kb", embed.clone()), "reason": {"$type": "app.bsky.feed.defs#reasonRepost"}},
            {"post": post("3kc", json!(null))},
        ]});
        let page = parse_feed(body.to_string().as_bytes(), DID).unwrap();
        assert_eq!(page.posts.len(), 1);
        assert_eq!(page.posts[0][0].metadata.as_ref().unwrap().post_id, "3ka");
        assert_eq!(page.cursor.as_deref(), Some("c2"));
    }

    #[test]
    fn classifies_supported_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://bsky.app/profile/Alice.bsky.social/post/3kabc").unwrap(),
            Target::Post {
                actor: "alice.bsky.social".into(),
                rkey: "3kabc".into()
            }
        );
        assert_eq!(
            t(&format!("https://bsky.app/profile/{DID}")).unwrap(),
            Target::Feed {
                actor: DID.into(),
                feed: Feed::Media
            }
        );
        assert_eq!(
            t("https://bsky.app/profile/alice.bsky.social/media").unwrap(),
            Target::Feed {
                actor: "alice.bsky.social".into(),
                feed: Feed::Media
            }
        );
        assert_eq!(
            t("https://bsky.app/profile/alice.bsky.social/posts").unwrap(),
            Target::Feed {
                actor: "alice.bsky.social".into(),
                feed: Feed::Posts
            }
        );
        for raw in [
            "https://bsky.app/profile/alice.bsky.social/likes",
            "https://bsky.app/profile/alice.bsky.social/feed/x",
            "https://bsky.app/profile/alice/post/3kabc",
            "https://bsky.app/profile/alice.bsky.social/post/a%2Fb",
            "https://bsky.app/search?q=x",
            "https://bsky.app.evil.test/profile/alice.bsky.social",
            "https://bsky.app:444/profile/alice.bsky.social",
            "https://bsky.app/",
        ] {
            assert!(t(raw).is_err(), "{raw}");
        }
        assert!(valid_did("did:web:example.com"));
        assert!(!valid_did("did:plc:short"));
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
        let root = Url::parse(&format!("http://{}/xrpc/", listener.local_addr().unwrap())).unwrap();
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

    fn bluesky(root: Url) -> Bluesky {
        Bluesky::new(Duration::from_secs(5))
            .unwrap()
            .with_network(&NetworkOptions::new(Some(String::new()), None).unwrap())
            .unwrap()
            .with_api_root(root)
    }

    #[tokio::test]
    async fn resolves_handle_then_fetches_post_with_retries() {
        let embed = json!({"$type": "app.bsky.embed.images", "images": [image(CID1, "")]});
        let resolved = json!({"did": DID}).to_string();
        let (root, server) = serve(vec![
            response("200 OK", "", resolved.as_bytes()),
            response("429 Too Many Requests", "Retry-After: 0\r\n", b""),
            response("200 OK", "", &thread("3kabc", embed)),
        ]);
        let events: Vec<_> = bluesky(root)
            .with_retries(1)
            .events(&Url::parse("https://bsky.app/profile/alice.bsky.social/post/3kabc").unwrap())
            .collect()
            .await;
        assert_eq!(events.len(), 3, "{events:?}");
        assert!(matches!(events[1], Ok(ExtractionEvent::Media(_))));
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with(
                "GET /xrpc/com.atproto.identity.resolveHandle?handle=alice.bsky.social "
            )
        );
        assert!(requests[2].starts_with(
            "GET /xrpc/app.bsky.feed.getPostThread?uri=at%3A%2F%2Fdid%3Aplc%3Aabcdefghijklmnopqrstuvwx%2Fapp.bsky.feed.post%2F3kabc&depth=0"
        ));
    }

    #[tokio::test]
    async fn paginates_author_feed_until_cursor_ends() {
        let embed = json!({"$type": "app.bsky.embed.images", "images": [image(CID1, "")]});
        let first = json!({"cursor": "next", "feed": [{"post": post("3ka", embed.clone())}]});
        let second = json!({"feed": [{"post": post("3kb", embed)}]});
        let (root, server) = serve(vec![
            response("200 OK", "", first.to_string().as_bytes()),
            response("200 OK", "", second.to_string().as_bytes()),
        ]);
        let media: Vec<_> = bluesky(root)
            .stream(&Url::parse(&format!("https://bsky.app/profile/{DID}/media")).unwrap())
            .collect()
            .await;
        assert_eq!(media.len(), 2);
        let requests = server.join().unwrap();
        assert!(requests[0].contains("filter=posts_with_media&limit=100 "));
        assert!(requests[1].contains("&cursor=next "));
    }

    #[tokio::test]
    async fn terminal_errors_are_not_retried() {
        let (root, server) = serve(vec![response("400 Bad Request", "", b"{}")]);
        let error = bluesky(root)
            .with_retries(3)
            .stream(&Url::parse("https://bsky.app/profile/nobody.example/post/3kabc").unwrap())
            .collect::<Vec<_>>()
            .await
            .remove(0)
            .unwrap_err();
        assert!(error.to_string().contains("not found"));
        assert!(!error.to_string().contains("127.0.0.1"));
        assert_eq!(server.join().unwrap().len(), 1);
    }
}
