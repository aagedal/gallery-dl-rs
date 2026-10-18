//! Experimental native Kemono/Coomer post and creator extraction.
//!
//! Mirrors gallery_dl/extractor/kemono.py for single posts and creator post
//! listings using the public `/api/v1` JSON API (which requires
//! `Accept: text/css`). `kemono.*` hosts use the `kemono` category and
//! `coomer.*` hosts the `coomer` category. Files are requested from
//! `https://HOST/data{path}`; the root host redirects to an `nN.HOST` CDN
//! subdomain, which the downloader allows for these categories only. The
//! service name is kept in the `instance` metadata field and the archive key.
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
            "unsupported Kemono/Coomer URL; expected /SERVICE/user/USER_ID[/post/POST_ID] on kemono.cr, kemono.su, coomer.st or coomer.su",
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
        [service, "user", user] if valid_service(service) && valid_id(user) => Ok(Target::User {
            host,
            service: (*service).into(),
            user: (*user).into(),
        }),
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
        Value::String(s) => Some(s.trim().to_owned()),
        Value::Number(n) => Some(n.to_string()),
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
    let Some(id) = id_of(post, "id") else {
        return Vec::new();
    };
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
pub fn parse_single(
    bytes: &[u8],
    host: &str,
    service: &str,
    user: &str,
    id: &str,
) -> Result<Vec<Media>> {
    let body = json(bytes)?;
    let post = body.get("post").filter(|p| p.is_object()).unwrap_or(&body);
    if id_of(post, "id").as_deref() != Some(id) {
        return Err(error("Kemono response does not match the requested post"));
    }
    let media = parse_post(post, host, service, user);
    if media.is_empty() {
        return Err(error("Kemono post contains no files"));
    }
    Ok(media)
}

/// One listing page: per-post files and the number of posts returned.
pub struct Page {
    pub posts: Vec<Vec<Media>>,
    pub count: usize,
}

pub fn parse_listing(bytes: &[u8], host: &str, service: &str, user: &str) -> Result<Page> {
    let body = json(bytes)?;
    let entries = body
        .as_array()
        .ok_or_else(|| error("Kemono response is not a post list"))?;
    let posts = entries
        .iter()
        .map(|post| parse_post(post, host, service, user))
        .filter(|media| !media.is_empty())
        .collect();
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
}

struct ListingState {
    host: String,
    service: String,
    user: String,
    offset: usize,
    pages: usize,
    done: bool,
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

    fn finish(&self, mut media: Vec<Media>) -> VecDeque<ExtractionEvent> {
        let category = media
            .first()
            .map_or_else(|| "kemono".to_owned(), |m| m.category.clone());
        for item in &mut media {
            self.network.apply_media(item);
        }
        group(media, &category)
    }

    async fn next_page(&self, state: &mut ListingState) -> Result<()> {
        let path = format!("api/v1/{}/user/{}/posts", state.service, state.user);
        let offset = state.offset.to_string();
        let body = self.call(&state.host, &path, &[("o", &offset)]).await?;
        let page = parse_listing(&body, &state.host, &state.service, &state.user)?;
        state.pages += 1;
        state.offset += page.count;
        state.done = page.count < PAGE_SIZE || state.pages >= MAX_PAGES;
        for media in page.posts {
            let events = self.finish(media);
            state.pending.extend(events);
        }
        Ok(())
    }

    pub fn stream(&self, url: &Url) -> BoxStream<'_, Result<Media>> {
        crate::event::media_only(self.events(url))
    }

    /// Emit one post boundary per post; creator listings paginate lazily.
    pub fn events(&self, url: &Url) -> BoxStream<'_, Result<ExtractionEvent>> {
        let target = match target(url) {
            Ok(target) => target,
            Err(e) => return stream::once(async move { Err(e) }).boxed(),
        };
        match target {
            Target::Post {
                host,
                service,
                user,
                post,
            } => stream::once(async move {
                let path = format!("api/v1/{service}/user/{user}/post/{post}");
                let body = self.call(&host, &path, &[]).await?;
                let media = parse_single(&body, &host, &service, &user, &post)?;
                Ok::<_, crate::Error>(stream::iter(self.finish(media).into_iter().map(Ok)))
            })
            .map(|result| match result {
                Ok(events) => events.boxed(),
                Err(e) => stream::once(async move { Err(e) }).boxed(),
            })
            .flatten()
            .boxed(),
            Target::User {
                host,
                service,
                user,
            } => {
                let state = ListingState {
                    host,
                    service,
                    user,
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
            let other = parse_post(&post("7"), "kemono.cr", "fanbox", "42");
            assert_ne!(Some(key), other[0].archive_key());
        }
    }

    #[test]
    fn rejects_mismatched_and_empty_posts() {
        let body = post("7").to_string();
        assert!(parse_single(body.as_bytes(), "kemono.cr", "patreon", "42", "8").is_err());
        let empty = json!({"id": 5, "attachments": []}).to_string();
        assert!(
            parse_single(empty.as_bytes(), "kemono.cr", "patreon", "42", "5")
                .unwrap_err()
                .to_string()
                .contains("no files")
        );
        assert!(parse_single(b"nope", "kemono.cr", "patreon", "42", "5").is_err());
        let page = parse_listing(
            json!([post("1"), {"id": "2"}]).to_string().as_bytes(),
            "kemono.cr",
            "patreon",
            "42",
        )
        .unwrap();
        assert_eq!((page.posts.len(), page.count), (1, 2));
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
        let full: Vec<Value> = (1..=50).map(|i| post(&format!("{}", 100 + i))).collect();
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
