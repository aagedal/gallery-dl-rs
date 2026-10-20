//! Experimental native Kemono/Coomer single-post, creator and global extraction.
//!
//! Mirrors gallery_dl/extractor/kemono.py for single posts, creator listings
//! and global feeds using the public `/api/v1` JSON API (which requires
//! `Accept: text/css`). `kemono.*` hosts use the `kemono` category and
//! `coomer.*` hosts the `coomer` category. Files are requested from
//! `https://HOST/data{path}`; the root host redirects to an `nN.HOST` CDN
//! subdomain, which the downloader allows for these categories only. The
//! service name is kept in the `instance` metadata field and the archive key.
use crate::{
    Result, error,
    event::{ExtractionEvent, Post, group},
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
    " (native Kemono extractor)"
);
const MAX_RESPONSE: usize = 32 * 1024 * 1024;
const PAGE_SIZE: usize = 50;
const MAX_PAGES: usize = 200;
/// Site domains (without optional `www.`/`beta.` prefixes) and categories.
pub const HOSTS: [(&str, &str); 6] = [
    ("kemono.cr", "kemono"),
    ("kemono.su", "kemono"),
    ("kemono.party", "kemono"),
    ("coomer.st", "coomer"),
    ("coomer.su", "coomer"),
    ("coomer.party", "coomer"),
];

/// Category for a supported site host; CDN subdomains (`n1.kemono.cr`) are
/// not site hosts.
pub fn category(host: &str) -> Option<&'static str> {
    let host = host
        .strip_prefix("www.")
        .or_else(|| host.strip_prefix("beta."))
        .unwrap_or(host);
    HOSTS
        .iter()
        .find(|(domain, _)| *domain == host)
        .map(|(_, category)| *category)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Posts {
        host: String,
    },
    Post {
        host: String,
        service: String,
        user: String,
        post: String,
    },
    User {
        host: String,
        service: String,
        user: String,
    },
}

pub fn valid_service(service: &str) -> bool {
    (1..=32).contains(&service.len())
        && service
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// Creator and post IDs are numeric on most services and alphanumeric on a few.
pub fn valid_id(id: &str) -> bool {
    (1..=128).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
        && id != "."
        && id != ".."
}

/// File hashes are the stem of the hash-named `/xx/yy/HASH.ext` path.
pub fn valid_hash(hash: &str) -> bool {
    (1..=128).contains(&hash.len()) && hash.bytes().all(|b| b.is_ascii_alphanumeric())
}

pub fn target(url: &Url) -> Result<Target> {
    let unsupported = || {
        error(
            "unsupported Kemono/Coomer URL; expected /posts or /SERVICE/user/USER_ID[/post/POST_ID] on kemono.cr, kemono.su, coomer.st or coomer.su",
        )
    };
    let host = url.host_str().ok_or_else(unsupported)?;
    if !matches!(url.scheme(), "http" | "https")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || category(host).is_none()
    {
        return Err(unsupported());
    }
    let segments: Vec<_> = url
        .path_segments()
        .map(|segments| segments.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    let host = host.to_owned();
    match segments.as_slice() {
        ["posts"] => {
            listing_query(url)?;
            Ok(Target::Posts { host })
        }
        [service, "user", user] if valid_service(service) && valid_id(user) => {
            listing_query(url)?;
            Ok(Target::User {
                host,
                service: (*service).into(),
                user: (*user).into(),
            })
        }
        [service, "user", user, "post", post]
            if valid_service(service) && valid_id(user) && valid_id(post) =>
        {
            Ok(Target::Post {
                host,
                service: (*service).into(),
                user: (*user).into(),
                post: (*post).into(),
            })
        }
        _ => Err(unsupported()),
    }
}

#[derive(Default)]
struct ListingQuery {
    offset: usize,
    query: Option<String>,
    tag: Option<String>,
}

fn listing_query(url: &Url) -> Result<ListingQuery> {
    let invalid = || error("invalid Kemono listing query");
    if url.query().is_some_and(|query| query.len() > 8192) {
        return Err(invalid());
    }
    let mut parsed = ListingQuery::default();
    let mut seen = HashSet::new();
    for (key, value) in url.query_pairs() {
        if !matches!(key.as_ref(), "o" | "q" | "tag") {
            continue;
        }
        if !seen.insert(key.to_string()) {
            return Err(invalid());
        }
        if key == "o" {
            if value.is_empty()
                || value.len() > 20
                || !value.bytes().all(|byte| byte.is_ascii_digit())
            {
                return Err(invalid());
            }
            let offset: usize = value.parse().map_err(|_| invalid())?;
            parsed.offset = offset - offset % PAGE_SIZE;
            if parsed.offset.checked_add(MAX_PAGES * PAGE_SIZE).is_none() {
                return Err(invalid());
            }
        } else {
            if value.len() > 1024 || value.chars().any(char::is_control) {
                return Err(invalid());
            }
            if key == "q" {
                parsed.query = Some(value.into_owned());
            } else {
                parsed.tag = Some(value.into_owned());
            }
        }
    }
    Ok(parsed)
}

#[derive(Clone)]
struct Identity {
    service: String,
    user: String,
    id: String,
}

fn post_identity(
    post: &Value,
    context: Option<(&str, &str)>,
    expected: Option<&str>,
) -> Result<Identity> {
    let invalid = || error("Kemono response has invalid or contradictory post identity");
    if !post.is_object() {
        return Err(invalid());
    }
    let service = match post.get("service") {
        Some(Value::String(service)) if valid_service(service) => service.clone(),
        None => context
            .map(|(service, _)| service.to_owned())
            .ok_or_else(invalid)?,
        _ => return Err(invalid()),
    };
    let user = match post.get("user") {
        Some(_) => id_of(post, "user").ok_or_else(invalid)?,
        None => context
            .map(|(_, user)| user.to_owned())
            .ok_or_else(invalid)?,
    };
    if !valid_service(&service)
        || !valid_id(&user)
        || context.is_some_and(|(owner_service, owner)| service != owner_service || user != owner)
    {
        return Err(invalid());
    }
    let id = id_of(post, "id").ok_or_else(invalid)?;
    if expected.is_some_and(|expected| id != expected) {
        return Err(invalid());
    }
    Ok(Identity { service, user, id })
}

fn post_metadata(post: &Value, identity: &Identity) -> PostMetadata {
    PostMetadata {
        post_id: identity.id.clone(),
        username: Some(Arc::from(identity.user.as_str())),
        instance: Some(identity.service.clone()),
        caption: string(post, "title").map(Arc::from),
        timestamp: ["published", "added"]
            .into_iter()
            .find_map(|key| string(post, key).and_then(|date| crate::date::parse_date(date).ok())),
        ..PostMetadata::default()
    }
}

fn string<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// IDs may be JSON strings or numbers.
fn id_of(value: &Value, key: &str) -> Option<String> {
    match value.get(key)? {
        Value::String(s) => Some(s.to_owned()),
        Value::Number(n) if n.is_u64() => Some(n.to_string()),
        _ => None,
    }
    .filter(|id| valid_id(id))
}

/// Validate a server file path and return its hash and lowercase extension.
fn split_path(path: &str) -> Option<(&str, Option<String>)> {
    let rest = path.strip_prefix('/')?;
    if rest.is_empty()
        || !rest.split('/').all(|segment| {
            !segment.is_empty()
                && segment != "."
                && segment != ".."
                && segment
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        })
    {
        return None;
    }
    let base = rest.rsplit('/').next()?;
    let (stem, ext) = match base.rsplit_once('.') {
        Some((stem, ext)) => (stem, Some(ext)),
        None => (base, None),
    };
    valid_hash(stem).then(|| (stem, ext.and_then(clean_extension)))
}

fn clean_extension(ext: &str) -> Option<String> {
    let ext = ext.to_ascii_lowercase();
    ((1..=8).contains(&ext.len()) && ext.bytes().all(|b| b.is_ascii_alphanumeric()))
        .then(|| if ext == "jpeg" { "jpg".into() } else { ext })
}

/// Parse one post object into its deduplicated files (`file` first, then
/// `attachments`). Posts without files yield an empty list.
pub fn parse_post(post: &Value, host: &str, service: &str, user: &str) -> Vec<Media> {
    let Some(category) = category(host) else {
        return Vec::new();
    };
    let Ok(identity) = post_identity(post, Some((service, user)), None) else {
        return Vec::new();
    };
    let id = identity.id.clone();
    let caption: Option<Arc<str>> = string(post, "title").map(Arc::from);
    let timestamp = ["published", "added"]
        .into_iter()
        .find_map(|key| string(post, key).and_then(|d| crate::date::parse_date(d).ok()));
    let username: Arc<str> = Arc::from(user);
    let files = post.get("file").into_iter().chain(
        post.get("attachments")
            .and_then(Value::as_array)
            .into_iter()
            .flatten(),
    );
    let mut seen = HashSet::new();
    let mut media = Vec::new();
    for file in files {
        let Some(path) = string(file, "path") else {
            continue;
        };
        let Some((hash, ext)) = split_path(path) else {
            continue;
        };
        if !seen.insert(path.to_owned()) {
            continue;
        }
        let name = string(file, "name");
        let Ok(url) = Url::parse(&format!("https://{host}/data{path}")) else {
            continue;
        };
        let ext = ext
            .or_else(|| {
                name.and_then(|n| n.rsplit_once('.'))
                    .and_then(|(_, ext)| clean_extension(ext))
            })
            .unwrap_or_else(|| "bin".into());
        let index = media.len() + 1;
        media.push(Media {
            url,
            job: MediaJob::DirectHttp,
            category: category.into(),
            filename: format!("{id}_{index:02}.{ext}"),
            metadata: Some(PostMetadata {
                file: Some(FileMetadata {
                    description: name.map(Arc::from),
                    ..FileMetadata::default()
                }),
                post_id: id.clone(),
                media_id: hash.into(),
                username: Some(username.clone()),
                caption: caption.clone(),
                timestamp,
                index,
                instance: Some(service.into()),
                ..PostMetadata::default()
            }),
            headers: BTreeMap::new(),
            handoff: BTreeMap::new(),
        });
    }
    media
}

fn json(bytes: &[u8]) -> Result<Value> {
    serde_json::from_slice(bytes).map_err(|_| error("Kemono returned invalid JSON"))
}

/// Parse a post response: either `{"post": {...}, ...}` or a bare post.
/// Empty posts are valid and still have post boundaries in the event stream.
pub fn parse_single(
    bytes: &[u8],
    host: &str,
    service: &str,
    user: &str,
    id: &str,
) -> Result<Vec<Media>> {
    let post = single_post(bytes, service, user, id)?;
    Ok(parse_post(&post, host, service, user))
}

fn single_post(bytes: &[u8], service: &str, user: &str, id: &str) -> Result<Value> {
    let body = json(bytes)?;
    let post = body.get("post").unwrap_or(&body);
    post_identity(post, Some((service, user)), Some(id))?;
    Ok(post.clone())
}

/// One listing page: per-post files (including empty posts) and raw post count.
pub struct Page {
    pub posts: Vec<Vec<Media>>,
    pub count: usize,
}

fn listing_entries(bytes: &[u8], global: bool) -> Result<Vec<Value>> {
    let body = json(bytes)?;
    let entries = if global {
        body.get("posts").and_then(Value::as_array)
    } else {
        body.as_array()
    }
    .ok_or_else(|| error("Kemono response is not a post list"))?;
    if entries.len() > PAGE_SIZE {
        return Err(error("Kemono post list exceeds page limit"));
    }
    Ok(entries.clone())
}

pub fn parse_listing(bytes: &[u8], host: &str, service: &str, user: &str) -> Result<Page> {
    parse_listing_context(bytes, host, Some((service, user)))
}

pub fn parse_global_listing(bytes: &[u8], host: &str) -> Result<Page> {
    parse_listing_context(bytes, host, None)
}

fn parse_listing_context(bytes: &[u8], host: &str, context: Option<(&str, &str)>) -> Result<Page> {
    let entries = listing_entries(bytes, context.is_none())?;
    let posts = entries
        .iter()
        .map(|post| {
            let identity = post_identity(post, context, None)?;
            Ok(parse_post(post, host, &identity.service, &identity.user))
        })
        .collect::<Result<_>>()?;
    Ok(Page {
        posts,
        count: entries.len(),
    })
}

pub struct Kemono {
    client: Client,
    network: NetworkOptions,
    api_root: Option<Url>,
    timeout: Duration,
    request_timeout: Duration,
    retries: u8,
    rate_limits: RateLimits,
    expand: bool,
    max_posts: Option<usize>,
}

struct ListingState {
    host: String,
    context: Option<(String, String)>,
    query: ListingQuery,
    offset: usize,
    pages: usize,
    emitted: usize,
    done: bool,
    raw: VecDeque<Value>,
    seen_pages: HashSet<[u8; 32]>,
    pending: VecDeque<ExtractionEvent>,
}

impl Kemono {
    pub fn new(timeout: Duration) -> Result<Self> {
        Ok(Self {
            client: NetworkOptions::default().client(USER_AGENT, timeout)?,
            network: NetworkOptions::default(),
            api_root: None,
            timeout,
            request_timeout: timeout,
            retries: 0,
            rate_limits: RateLimits::default(),
            expand: false,
            max_posts: None,
        })
    }

    /// Fetch each listing post's full detail response only when it is consumed.
    pub fn with_expand(mut self, expand: bool) -> Self {
        self.expand = expand;
        self
    }

    /// Limit raw posts, including posts with no files. Zero means unlimited.
    pub fn with_max_posts(mut self, max_posts: Option<usize>) -> Self {
        self.max_posts = max_posts.filter(|limit| *limit > 0);
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

    /// Override the API origin for every host; intended for local tests.
    pub fn with_api_root(mut self, root: Url) -> Self {
        self.api_root = Some(root);
        self
    }

    async fn call(&self, host: &str, path: &str, query: &[(&str, &str)]) -> Result<Vec<u8>> {
        let root = self
            .api_root
            .clone()
            .unwrap_or_else(|| Url::parse(&format!("https://{host}/")).expect("known host"));
        let mut endpoint = root.join(path).expect("validated Kemono path");
        if !query.is_empty() {
            endpoint.query_pairs_mut().extend_pairs(query);
        }
        tokio::time::timeout(self.timeout, self.request(&endpoint))
            .await
            .map_err(|_| error("native Kemono extraction timed out"))?
    }

    fn finish(&self, host: &str, post: &Value, identity: &Identity) -> VecDeque<ExtractionEvent> {
        let mut media = parse_post(post, host, &identity.service, &identity.user);
        let category = category(host).expect("validated host");
        for item in &mut media {
            self.network.apply_media(item);
        }
        if media.is_empty() {
            VecDeque::from([
                ExtractionEvent::Post(Box::new(Post {
                    category: category.into(),
                    metadata: Some(post_metadata(post, identity)),
                    count: Some(0),
                })),
                ExtractionEvent::EndPost,
            ])
        } else {
            group(media, category)
        }
    }

    async fn next_page(&self, state: &mut ListingState) -> Result<()> {
        let path = state.context.as_ref().map_or_else(
            || "api/v1/posts".to_owned(),
            |(service, user)| format!("api/v1/{service}/user/{user}/posts"),
        );
        let offset = state.offset.to_string();
        let mut query = vec![("o", offset.as_str())];
        if let Some(value) = &state.query.query {
            query.push(("q", value.as_str()));
        }
        if let Some(value) = &state.query.tag {
            query.push(("tag", value.as_str()));
        }
        let body = self.call(&state.host, &path, &query).await?;
        let entries = listing_entries(&body, state.context.is_none())?;
        // Identify the ordered source posts, independent of JSON formatting or
        // unrelated metadata. Do not validate entries eagerly: a raw post limit
        // may intentionally stop before a later malformed entry is consumed.
        let identities: Vec<_> = entries
            .iter()
            .map(|post| {
                (
                    post.get("service")
                        .and_then(Value::as_str)
                        .or_else(|| state.context.as_ref().map(|(service, _)| service.as_str())),
                    id_of(post, "user")
                        .or_else(|| state.context.as_ref().map(|(_, user)| user.clone())),
                    id_of(post, "id"),
                )
            })
            .collect();
        let fingerprint =
            Sha256::digest(serde_json::to_vec(&identities).expect("serializable identities"));
        if !entries.is_empty() && !state.seen_pages.insert(fingerprint.into()) {
            return Err(error("Kemono listing repeated a page"));
        }
        state.pages += 1;
        state.offset += PAGE_SIZE;
        state.done = entries.len() < PAGE_SIZE || state.pages >= MAX_PAGES;
        state.raw = entries.into();
        Ok(())
    }

    pub fn stream(&self, url: &Url) -> BoxStream<'_, Result<Media>> {
        crate::event::media_only(self.events(url))
    }

    /// Emit every raw post boundary; page and detail requests are both lazy.
    pub fn events(&self, url: &Url) -> BoxStream<'_, Result<ExtractionEvent>> {
        let target = match target(url) {
            Ok(target) => target,
            Err(e) => return stream::once(async move { Err(e) }).boxed(),
        };
        let (host, context) = match target {
            Target::Post {
                host,
                service,
                user,
                post,
            } => {
                return stream::once(async move {
                    let path = format!("api/v1/{service}/user/{user}/post/{post}");
                    let body = self.call(&host, &path, &[]).await?;
                    let raw = single_post(&body, &service, &user, &post)?;
                    let identity = post_identity(&raw, Some((&service, &user)), Some(&post))?;
                    Ok::<_, crate::Error>(stream::iter(
                        self.finish(&host, &raw, &identity).into_iter().map(Ok),
                    ))
                })
                .map(|result| match result {
                    Ok(events) => events.boxed(),
                    Err(e) => stream::once(async move { Err(e) }).boxed(),
                })
                .flatten()
                .boxed();
            }
            Target::User {
                host,
                service,
                user,
            } => (host, Some((service, user))),
            Target::Posts { host } => (host, None),
        };
        let query = match listing_query(url) {
            Ok(query) => query,
            Err(e) => return stream::once(async move { Err(e) }).boxed(),
        };
        let state = ListingState {
            host,
            context,
            offset: query.offset,
            query,
            pages: 0,
            emitted: 0,
            done: false,
            raw: VecDeque::new(),
            seen_pages: HashSet::new(),
            pending: VecDeque::new(),
        };
        stream::unfold(Some(state), move |state| async move {
            let mut state = state?;
            loop {
                if let Some(event) = state.pending.pop_front() {
                    return Some((Ok(event), Some(state)));
                }
                if self.max_posts.is_some_and(|limit| state.emitted >= limit) {
                    return None;
                }
                if let Some(mut post) = state.raw.pop_front() {
                    state.emitted += 1;
                    let context = state
                        .context
                        .as_ref()
                        .map(|(service, user)| (service.as_str(), user.as_str()));
                    let identity = match post_identity(&post, context, None) {
                        Ok(identity) => identity,
                        Err(e) => return Some((Err(e), None)),
                    };
                    if self.expand {
                        let path = format!(
                            "api/v1/{}/user/{}/post/{}",
                            identity.service, identity.user, identity.id
                        );
                        let result = async {
                            let body = self.call(&state.host, &path, &[]).await?;
                            single_post(&body, &identity.service, &identity.user, &identity.id)
                        }
                        .await;
                        match result {
                            Ok(detail) => post = detail,
                            Err(e) => return Some((Err(e), None)),
                        }
                    }
                    state.pending = self.finish(&state.host, &post, &identity);
                    continue;
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
                // The Kemono API only answers with JSON for this Accept value.
                .header(header::ACCEPT, "text/css")
                .send()
                .await
                .map_err(|_| Failure::transient("Kemono request failed; check network access"))?;
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
                            format!("Kemono rate limit; retry after {} seconds", delay.as_secs())
                        }
                        None => "Kemono rate limit; retry later".into(),
                    }
                } else {
                    format!("Kemono returned HTTP {status}")
                },
            ));
        }
        if matches!(status.as_u16(), 404 | 410) {
            return Err(error("Kemono post or creator not found").into());
        }
        if matches!(status.as_u16(), 401 | 403) {
            return Err(error("Kemono refused access").into());
        }
        if !status.is_success() {
            return Err(error(format!("Kemono returned HTTP {status}")).into());
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| Failure::transient("could not read Kemono response"))?
        {
            if body.len() + chunk.len() > MAX_RESPONSE {
                return Err(error("Kemono response exceeded 32 MiB").into());
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

    const HASH_A: &str = "aa11";
    const HASH_B: &str = "bb22";

    fn post(id: &str) -> Value {
        json!({"id": id, "user": "42", "service": "patreon", "title": "Hello",
            "published": "2024-01-02T03:04:05", "added": "2024-02-02T00:00:00",
            "file": {"name": "cover.JPEG", "path": format!("/aa/11/{HASH_A}.jpeg")},
            "attachments": [
                {"name": "cover.JPEG", "path": format!("/aa/11/{HASH_A}.jpeg")},
                {"name": "bad", "path": "/../etc/passwd"},
                {"name": "clip.mp4", "path": format!("/bb/22/{HASH_B}")}]})
    }

    #[test]
    fn parses_post_and_dedupes_files() {
        for body in [json!({"post": post("7"), "attachments": []}), post("7")] {
            let body = body.to_string();
            let media = parse_single(body.as_bytes(), "kemono.cr", "patreon", "42", "7").unwrap();
            assert_eq!(media.len(), 2);
            assert_eq!(
                media[0].url.as_str(),
                format!("https://kemono.cr/data/aa/11/{HASH_A}.jpeg")
            );
            assert_eq!(media[0].category, "kemono");
            assert_eq!(media[0].filename, "7_01.jpg");
            assert_eq!(media[1].filename, "7_02.mp4");
            let meta = media[0].metadata.as_ref().unwrap();
            assert_eq!(meta.post_id, "7");
            assert_eq!(meta.media_id, HASH_A);
            assert_eq!(meta.username.as_deref(), Some("42"));
            assert_eq!(meta.caption.as_deref(), Some("Hello"));
            assert_eq!(meta.timestamp, Some(1704164645));
            assert_eq!(meta.index, 1);
            assert_eq!(meta.instance.as_deref(), Some("patreon"));
            assert_eq!(
                meta.file.as_ref().unwrap().description.as_deref(),
                Some("cover.JPEG")
            );
            let key = media[0].archive_key().unwrap();
            assert!(key.starts_with("kemono:"), "{key}");
            assert_ne!(Some(key.clone()), media[1].archive_key());
            let coomer = parse_post(&post("7"), "coomer.st", "patreon", "42");
            assert_eq!(coomer[0].category, "coomer");
            assert_ne!(Some(key.clone()), coomer[0].archive_key());
            let mut different_service = post("7");
            different_service["service"] = json!("fanbox");
            let other = parse_post(&different_service, "kemono.cr", "fanbox", "42");
            assert_ne!(Some(key), other[0].archive_key());
        }
    }

    #[test]
    fn rejects_mismatched_posts_and_retains_empty_posts() {
        let body = post("7").to_string();
        assert!(parse_single(body.as_bytes(), "kemono.cr", "patreon", "42", "8").is_err());
        let empty = json!({"id": 5, "attachments": []}).to_string();
        assert!(
            parse_single(empty.as_bytes(), "kemono.cr", "patreon", "42", "5")
                .unwrap()
                .is_empty()
        );
        assert!(parse_single(b"nope", "kemono.cr", "patreon", "42", "5").is_err());
        let page = parse_listing(
            json!([post("1"), {"id": "2"}]).to_string().as_bytes(),
            "kemono.cr",
            "patreon",
            "42",
        )
        .unwrap();
        assert_eq!((page.posts.len(), page.count), (2, 2));
        assert!(parse_listing(b"{}", "kemono.cr", "patreon", "42").is_err());
    }

    #[test]
    fn classifies_supported_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://kemono.su/patreon/user/42/post/7").unwrap(),
            Target::Post {
                host: "kemono.su".into(),
                service: "patreon".into(),
                user: "42".into(),
                post: "7".into(),
            }
        );
        assert_eq!(
            t("https://coomer.st/onlyfans/user/some_name").unwrap(),
            Target::User {
                host: "coomer.st".into(),
                service: "onlyfans".into(),
                user: "some_name".into(),
            }
        );
        assert!(t("https://www.kemono.cr/fanbox/user/1/").is_ok());
        for raw in [
            "https://kemono.cr/",
            "https://kemono.cr/patreon/user/",
            "https://kemono.cr/Patreon/user/1",
            "https://kemono.cr/patreon/user/1/post/",
            "https://kemono.cr/patreon/user/1/post/a%2Fb",
            "https://n1.kemono.cr/patreon/user/1",
            "https://kemono.cr:444/patreon/user/1",
            "https://kemono.cr.evil.example/patreon/user/1",
            "https://notkemono.cr/patreon/user/1",
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

    fn kemono(root: Url) -> Kemono {
        Kemono::new(Duration::from_secs(5))
            .unwrap()
            .with_network(&NetworkOptions::new(Some(String::new()), None).unwrap())
            .unwrap()
            .with_api_root(root)
    }

    #[tokio::test]
    async fn fetches_post_with_retries_and_css_accept() {
        let (root, server) = serve(vec![
            response("503 Service Unavailable", b""),
            response("200 OK", json!({"post": post("7")}).to_string().as_bytes()),
        ]);
        let events: Vec<_> = kemono(root)
            .with_retries(1)
            .events(&Url::parse("https://kemono.cr/patreon/user/42/post/7").unwrap())
            .collect()
            .await;
        assert_eq!(events.len(), 4, "{events:?}");
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /api/v1/patreon/user/42/post/7 "));
        let lower = requests[1].to_ascii_lowercase();
        assert!(lower.contains("accept: text/css"));
        assert!(lower.contains("user-agent: gallery-dl-rs/"));
    }

    #[tokio::test]
    async fn paginates_creator_posts_by_offset() {
        let full: Vec<Value> = (1..=50)
            .map(|i| {
                let mut post = post(&format!("{}", 100 + i));
                post["service"] = json!("onlyfans");
                post["user"] = json!("alice");
                post
            })
            .collect();
        let (root, server) = serve(vec![
            response("200 OK", Value::Array(full).to_string().as_bytes()),
            response("200 OK", b"[]"),
        ]);
        let media: Vec<_> = kemono(root)
            .stream(&Url::parse("https://coomer.su/onlyfans/user/alice").unwrap())
            .collect()
            .await;
        assert_eq!(media.len(), 100);
        assert_eq!(media[0].as_ref().unwrap().category, "coomer");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/v1/onlyfans/user/alice/posts?o=0 "));
        assert!(requests[1].starts_with("GET /api/v1/onlyfans/user/alice/posts?o=50 "));
    }

    #[test]
    fn global_listings_use_each_posts_owner_and_preserve_archive_identity() {
        let first = post("7");
        let mut second = post("7");
        second["service"] = json!("fanbox");
        second["user"] = json!("other_creator");
        let page = parse_global_listing(
            json!({"posts": [first, second]}).to_string().as_bytes(),
            "kemono.cr",
        )
        .unwrap();
        assert_eq!((page.count, page.posts.len()), (2, 2));
        let first = &page.posts[0][0];
        let second = &page.posts[1][0];
        assert_eq!(first.filename, second.filename);
        assert_eq!(
            first.metadata.as_ref().unwrap().instance.as_deref(),
            Some("patreon")
        );
        assert_eq!(
            second.metadata.as_ref().unwrap().instance.as_deref(),
            Some("fanbox")
        );
        assert_eq!(
            second.metadata.as_ref().unwrap().username.as_deref(),
            Some("other_creator")
        );
        assert_ne!(first.archive_key(), second.archive_key());
        assert_eq!(
            first.archive_key(),
            parse_post(&post("7"), "kemono.cr", "patreon", "42")[0].archive_key()
        );
        assert!(parse_global_listing(b"[]", "kemono.cr").is_err());
        assert!(parse_global_listing(br#"{"posts":{}}"#, "kemono.cr").is_err());
    }

    #[test]
    fn listing_and_detail_identity_reject_contradictions_privately() {
        for (field, value) in [
            ("service", json!("secretservice")),
            ("user", json!("secretuser")),
            ("id", json!("secretpost")),
        ] {
            let mut body = post("7");
            body[field] = value;
            let error = parse_single(
                body.to_string().as_bytes(),
                "kemono.cr",
                "patreon",
                "42",
                "7",
            )
            .unwrap_err()
            .to_string();
            assert!(!error.contains("secret"));
            assert!(error.contains("identity"));
        }
        for body in [
            json!({"id": "7", "service": "patreon"}),
            json!({"id": "7", "user": "42"}),
            json!({"id": "7", "user": "../private", "service": "patreon"}),
            json!({"id": "7", "user": "42", "service": null}),
            json!({"id": 7.5, "user": "42", "service": "patreon"}),
        ] {
            assert!(
                parse_global_listing(json!({"posts": [body]}).to_string().as_bytes(), "kemono.cr")
                    .is_err()
            );
        }
        let minimal = json!({"id": "7", "file": {"path": "/aa/11/aa11.jpg"}});
        assert_eq!(
            parse_single(
                minimal.to_string().as_bytes(),
                "kemono.cr",
                "patreon",
                "42",
                "7"
            )
            .unwrap()
            .len(),
            1
        );
        assert!(
            parse_listing(
                json!([{ "id": "7", "user": "other", "service": "patreon" }])
                    .to_string()
                    .as_bytes(),
                "kemono.cr",
                "patreon",
                "42"
            )
            .is_err()
        );
    }

    #[test]
    fn queries_are_bounded_private_and_offsets_align_down() {
        assert_eq!(
            target(&Url::parse("https://kemono.cr/posts").unwrap()).unwrap(),
            Target::Posts {
                host: "kemono.cr".into()
            }
        );
        for path in ["/posts", "/patreon/user/42"] {
            let url = Url::parse(&format!(
                "https://kemono.cr{path}?o=149&q=red%20%26%20blue&tag=%E7%8C%AB"
            ))
            .unwrap();
            let query = listing_query(&url).unwrap();
            assert_eq!(query.offset, 100);
            assert_eq!(query.query.as_deref(), Some("red & blue"));
            assert_eq!(query.tag.as_deref(), Some("猫"));
            for query in [
                "o=-1",
                "o=secret",
                "o=999999999999999999999999",
                "q=secret%00",
                "tag=secret%0A",
                "q=secret&q=again",
                "o=1&o=2",
            ] {
                let error =
                    target(&Url::parse(&format!("https://kemono.cr{path}?{query}")).unwrap())
                        .unwrap_err()
                        .to_string();
                assert!(!error.contains("secret"));
                assert_eq!(error, "invalid Kemono listing query");
            }
        }
        let long = "s".repeat(1025);
        assert!(
            target(&Url::parse(&format!("https://kemono.cr/posts?q={long}")).unwrap()).is_err()
        );
    }

    #[tokio::test]
    async fn global_queries_are_encoded_and_mixed_creators_keep_order() {
        let mut other = post("8");
        other["service"] = json!("fanbox");
        other["user"] = json!("alice");
        let (root, server) = serve(vec![response(
            "200 OK",
            json!({"posts": [post("7"), other]}).to_string().as_bytes(),
        )]);
        let items: Vec<_> = kemono(root)
            .stream(
                &Url::parse("https://kemono.cr/posts?o=149&q=red%20%26%20blue&tag=%E7%8C%AB")
                    .unwrap(),
            )
            .collect()
            .await;
        let ids: Vec<_> = items
            .iter()
            .map(|item| {
                item.as_ref()
                    .unwrap()
                    .metadata
                    .as_ref()
                    .unwrap()
                    .post_id
                    .as_str()
            })
            .collect();
        assert_eq!(ids, ["7", "7", "8", "8"]);
        assert_eq!(
            items[2]
                .as_ref()
                .unwrap()
                .metadata
                .as_ref()
                .unwrap()
                .username
                .as_deref(),
            Some("alice")
        );
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("GET /api/v1/posts?o=100&q=red+%26+blue&tag=%E7%8C%AB "));
    }

    #[tokio::test]
    async fn creator_queries_preserve_filters_on_later_pages() {
        let full: Vec<_> = (0..50)
            .map(|index| post(&(index + 1).to_string()))
            .collect();
        let (root, server) = serve(vec![
            response("200 OK", json!(full).to_string().as_bytes()),
            response("200 OK", b"[]"),
        ]);
        let items: Vec<_> = kemono(root)
            .stream(
                &Url::parse("https://kemono.cr/patreon/user/42?o=99&q=a%26b&tag=a%2Fb").unwrap(),
            )
            .collect()
            .await;
        assert_eq!(items.len(), 100);
        assert!(items.iter().all(Result::is_ok));
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with("GET /api/v1/patreon/user/42/posts?o=50&q=a%26b&tag=a%2Fb ")
        );
        assert!(
            requests[1].starts_with("GET /api/v1/patreon/user/42/posts?o=100&q=a%26b&tag=a%2Fb ")
        );
    }

    #[tokio::test]
    async fn expand_is_lazy_per_post_and_cancels_without_fetching_next_detail() {
        let listing = json!({"posts": [
            {"id": "7", "service": "patreon", "user": "42", "title": "summary"},
            {"id": "8", "service": "fanbox", "user": "alice"}
        ]});
        let mut detail = post("7");
        detail["title"] = json!("expanded");
        let (root, server) = serve(vec![
            response("200 OK", listing.to_string().as_bytes()),
            response("200 OK", json!({"post": detail}).to_string().as_bytes()),
        ]);
        let client = kemono(root).with_expand(true);
        let input = Url::parse("https://kemono.cr/posts").unwrap();
        let mut events = client.events(&input);
        let event = events.next().await.unwrap().unwrap();
        let ExtractionEvent::Post(post) = event else {
            panic!("expected boundary")
        };
        assert_eq!(
            post.metadata.as_ref().unwrap().caption.as_deref(),
            Some("expanded")
        );
        let first = events.next().await.unwrap().unwrap();
        let ExtractionEvent::Media(first) = first else {
            panic!("expected first file")
        };
        assert_eq!(first.filename, "7_01.jpg");
        assert_eq!(
            first.archive_key(),
            parse_post(&detail, "kemono.cr", "patreon", "42")[0].archive_key()
        );
        drop(events);
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].starts_with("GET /api/v1/patreon/user/42/post/7 "));
    }

    #[tokio::test]
    async fn global_expansion_fetches_each_author_in_order_and_honors_raw_cutoff() {
        let mut second = post("8");
        second["service"] = json!("fanbox");
        second["user"] = json!("alice");
        let listing = json!({"posts": [
            {"id": "7", "service": "patreon", "user": "42"},
            {"id": "8", "service": "fanbox", "user": "alice"},
            {"id": "invalid", "service": null}
        ]});
        let (root, server) = serve(vec![
            response("200 OK", listing.to_string().as_bytes()),
            response("200 OK", post("7").to_string().as_bytes()),
            response("200 OK", json!({"post": second}).to_string().as_bytes()),
        ]);
        let items: Vec<_> = kemono(root)
            .with_expand(true)
            .with_max_posts(Some(2))
            .stream(&Url::parse("https://kemono.cr/posts").unwrap())
            .collect()
            .await;
        assert_eq!(items.len(), 4);
        let ids: Vec<_> = items
            .iter()
            .map(|item| {
                item.as_ref()
                    .unwrap()
                    .metadata
                    .as_ref()
                    .unwrap()
                    .post_id
                    .as_str()
            })
            .collect();
        assert_eq!(ids, ["7", "7", "8", "8"]);
        assert_eq!(
            items[2]
                .as_ref()
                .unwrap()
                .metadata
                .as_ref()
                .unwrap()
                .username
                .as_deref(),
            Some("alice")
        );
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[1].starts_with("GET /api/v1/patreon/user/42/post/7 "));
        assert!(requests[2].starts_with("GET /api/v1/fanbox/user/alice/post/8 "));
    }

    #[tokio::test]
    async fn max_posts_counts_empty_posts_and_prevents_later_details_and_pages() {
        let listing: Vec<_> = (1..=50)
            .map(|index| json!({"id": index, "service": "patreon", "user": "42"}))
            .collect();
        let empty = json!({"id": "1", "title": "empty", "published": "2024-01-02T03:04:05"});
        let (root, server) = serve(vec![
            response("200 OK", json!(listing).to_string().as_bytes()),
            response("200 OK", empty.to_string().as_bytes()),
        ]);
        let client = kemono(root).with_expand(true).with_max_posts(Some(1));
        let events: Vec<_> = client
            .events(&Url::parse("https://coomer.st/patreon/user/42").unwrap())
            .collect()
            .await;
        assert_eq!(events.len(), 2);
        let ExtractionEvent::Post(post) = events[0].as_ref().unwrap() else {
            panic!("expected empty boundary")
        };
        assert_eq!(post.category, "coomer");
        assert_eq!(post.count, Some(0));
        let metadata = post.metadata.as_ref().unwrap();
        assert_eq!(metadata.post_id, "1");
        assert_eq!(metadata.username.as_deref(), Some("42"));
        assert_eq!(metadata.instance.as_deref(), Some("patreon"));
        assert_eq!(metadata.caption.as_deref(), Some("empty"));
        assert_eq!(metadata.timestamp, Some(1704164645));
        assert!(matches!(events[1], Ok(ExtractionEvent::EndPost)));
        assert_eq!(server.join().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn empty_single_posts_have_metadata_boundaries_and_zero_limit_is_unlimited() {
        let (root, server) = serve(vec![response("200 OK", br#"{"id":"7","title":"empty"}"#)]);
        let events: Vec<_> = kemono(root)
            .events(&Url::parse("https://coomer.st/onlyfans/user/alice/post/7").unwrap())
            .collect()
            .await;
        assert_eq!(events.len(), 2);
        let ExtractionEvent::Post(empty_post) = events[0].as_ref().unwrap() else {
            panic!("expected post")
        };
        assert_eq!(empty_post.category, "coomer");
        assert_eq!(
            empty_post.metadata.as_ref().unwrap().username.as_deref(),
            Some("alice")
        );
        server.join().unwrap();
        let (root, server) = serve(vec![response(
            "200 OK",
            json!([{ "id": "1" }, post("2")]).to_string().as_bytes(),
        )]);
        let events: Vec<_> = kemono(root)
            .with_max_posts(Some(1))
            .with_max_posts(Some(0))
            .events(&Url::parse("https://kemono.cr/patreon/user/42").unwrap())
            .collect()
            .await;
        assert_eq!(events.len(), 6);
        assert!(events.iter().all(Result::is_ok));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn expansion_rejects_changed_owner_service_or_post_before_files() {
        for (field, value) in [
            ("service", "private-service"),
            ("user", "private-user"),
            ("id", "private-post"),
        ] {
            let mut detail = post("7");
            detail[field] = json!(value);
            let (root, server) = serve(vec![
                response("200 OK", json!([post("7")]).to_string().as_bytes()),
                response("200 OK", detail.to_string().as_bytes()),
            ]);
            let events: Vec<_> = kemono(root)
                .with_expand(true)
                .events(&Url::parse("https://kemono.cr/patreon/user/42").unwrap())
                .collect()
                .await;
            assert_eq!(events.len(), 1);
            let error = events[0].as_ref().unwrap_err().to_string();
            assert!(error.contains("identity"));
            assert!(!error.contains("private"));
            server.join().unwrap();
        }
    }

    #[tokio::test]
    async fn repeated_pages_terminate_with_private_diagnostic() {
        let page: Vec<_> = (1..=50).map(|index| post(&index.to_string())).collect();
        let body = json!(page).to_string();
        // The server may repeat the same posts with different field ordering,
        // formatting and refreshed non-identity metadata.
        let repeated: Vec<_> = (1..=50).map(|index| json!({"title":"refreshed", "service":"patreon", "id":index.to_string(), "user":"42"})).collect();
        let repeated = serde_json::to_vec_pretty(&repeated).unwrap();
        let (root, server) = serve(vec![
            response("200 OK", body.as_bytes()),
            response("200 OK", repeated.as_slice()),
        ]);
        let items: Vec<_> = kemono(root)
            .stream(&Url::parse("https://kemono.cr/patreon/user/42").unwrap())
            .collect()
            .await;
        assert_eq!(items.len(), 101);
        assert!(
            items
                .last()
                .unwrap()
                .as_ref()
                .unwrap_err()
                .to_string()
                .contains("repeated a page")
        );
        assert_eq!(server.join().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn terminal_errors_are_not_retried() {
        let (root, server) = serve(vec![response("404 Not Found", b"{}")]);
        let error = kemono(root)
            .with_retries(3)
            .stream(&Url::parse("https://kemono.su/fanbox/user/1").unwrap())
            .collect::<Vec<_>>()
            .await
            .remove(0)
            .unwrap_err();
        assert!(error.to_string().contains("not found"));
        assert!(!error.to_string().contains("127.0.0.1"));
        assert_eq!(server.join().unwrap().len(), 1);
    }
}
