//! Experimental native Wallhaven wallpaper, search, upload and collection
//! extraction.
//!
//! Mirrors gallery_dl/extractor/wallhaven.py using the public JSON API
//! (`/api/v1`). Single wallpapers use `w/ID`; searches, user uploads
//! (`q=@USER`) and collections page through `meta.last_page`, capped at 200
//! pages. An optional `api-key` is sent as the `X-API-Key` header and is
//! required for NSFW content. Media must be HTTPS on `w.wallhaven.cc`.
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
    " (native Wallhaven extractor)"
);
const MAX_RESPONSE: usize = 16 * 1024 * 1024;
const MAX_PAGES: u64 = 200;

/// Wallpaper IDs such as `94x38z`.
pub fn valid_id(id: &str) -> bool {
    (1..=16).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn valid_user(user: &str) -> bool {
    (1..=64).contains(&user.len())
        && user
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Wallpaper {
        id: String,
    },
    /// Search query pairs (without `page`).
    Search {
        query: Vec<(String, String)>,
    },
    Collection {
        user: String,
        id: String,
    },
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
            "unsupported Wallhaven URL; expected https://wallhaven.cc/w/ID, /search?..., /user/USER/uploads or /user/USER/favorites/ID",
        )
    };
    if !plain(url) {
        return Err(unsupported());
    }
    let parts: Vec<&str> = url
        .path_segments()
        .map(|s| s.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    match url.host_str().ok_or_else(unsupported)? {
        "whvn.cc" | "www.whvn.cc" => match parts[..] {
            [id] if valid_id(id) => Ok(Target::Wallpaper { id: id.into() }),
            _ => Err(unsupported()),
        },
        "wallhaven.cc" | "www.wallhaven.cc" => match parts[..] {
            ["w", id] if valid_id(id) => Ok(Target::Wallpaper { id: id.into() }),
            ["search"] => Ok(Target::Search {
                query: url
                    .query_pairs()
                    .filter(|(k, _)| k != "page" && k != "apikey")
                    .map(|(k, v)| (k.into_owned(), v.into_owned()))
                    .collect(),
            }),
            ["user", user, "uploads"] if valid_user(user) => Ok(Target::Search {
                query: vec![("q".into(), format!("@{user}"))],
            }),
            ["user", user, "favorites", id]
                if valid_user(user) && id.bytes().all(|b| b.is_ascii_digit()) && id.len() <= 20 =>
            {
                Ok(Target::Collection {
                    user: user.into(),
                    id: id.into(),
                })
            }
            _ => Err(unsupported()),
        },
        _ => Err(unsupported()),
    }
}

/// Whether a URL is a Wallhaven page handled by this extractor.
pub fn is_wallhaven_url(url: &Url) -> bool {
    target(url).is_ok()
}

/// HTTPS full-size images on `w.wallhaven.cc`.
pub fn media_url(raw: &str) -> Option<Url> {
    let url = Url::parse(raw.trim()).ok()?;
    (url.scheme() == "https" && plain(&url) && url.host_str() == Some("w.wallhaven.cc"))
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

fn extension(url: &Url, file_type: Option<&str>) -> String {
    url.path_segments()
        .and_then(|mut s| s.next_back())
        .and_then(|name| name.rsplit_once('.'))
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .filter(|ext| {
            (1..=5).contains(&ext.len()) && ext.bytes().all(|b| b.is_ascii_alphanumeric())
        })
        .or_else(|| match file_type {
            Some("image/png") => Some("png".into()),
            _ => None,
        })
        .map(|ext| if ext == "jpeg" { "jpg".into() } else { ext })
        .unwrap_or_else(|| "jpg".into())
}

/// Build the media item for one wallpaper object (from `w/ID` or a listing).
pub fn wallpaper(data: &Value, index: usize) -> Option<Media> {
    let id = text(data, "id").filter(|id| valid_id(id))?;
    let url = media_url(&text(data, "path")?)?;
    let file_type = text(data, "file_type");
    let ext = extension(&url, file_type.as_deref());
    let (mut width, mut height) = (number(data, "dimension_x"), number(data, "dimension_y"));
    if let (None, None, Some((w, h))) = (
        width,
        height,
        text(data, "resolution").and_then(|r| {
            let (w, h) = r.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?))
        }),
    ) {
        (width, height) = (Some(w), Some(h));
    }
    let tags: Vec<String> = data
        .get("tags")
        .and_then(Value::as_array)
        .map(|tags| tags.iter().filter_map(|t| text(t, "name")).collect())
        .unwrap_or_default();
    Some(Media {
        filename: format!("wallhaven-{id}.{ext}"),
        job: MediaJob::DirectHttp,
        category: "wallhaven".into(),
        metadata: Some(PostMetadata {
            file: Some(FileMetadata {
                width,
                height,
                media_type: Some(Arc::from("photo")),
                ..FileMetadata::default()
            }),
            post_id: id.clone(),
            media_id: id,
            username: data
                .get("uploader")
                .and_then(|u| text(u, "username"))
                .map(Arc::from),
            caption: (!tags.is_empty()).then(|| Arc::from(tags.join(" "))),
            timestamp: text(data, "created_at").and_then(|d| crate::date::parse_date(&d).ok()),
            index,
            ..PostMetadata::default()
        }),
        url,
        headers: BTreeMap::new(),
        handoff: BTreeMap::new(),
    })
}

fn parse(bytes: &[u8]) -> Result<Value> {
    let body: Value =
        serde_json::from_slice(bytes).map_err(|_| error("Wallhaven returned invalid JSON"))?;
    if body.get("error").is_some() {
        return Err(error("Wallhaven API returned an error"));
    }
    Ok(body)
}

/// Parse a `w/ID` response.
pub fn parse_wallpaper(bytes: &[u8], id: &str) -> Result<Media> {
    let body = parse(bytes)?;
    let data = body
        .get("data")
        .filter(|d| text(d, "id").as_deref() == Some(id))
        .ok_or_else(|| error("Wallhaven wallpaper not found"))?;
    wallpaper(data, 1).ok_or_else(|| error("Wallhaven wallpaper contains no supported media"))
}

/// One listing page: wallpaper objects and `meta.last_page`.
pub fn parse_page(bytes: &[u8]) -> Result<(Vec<Value>, u64)> {
    let mut body = parse(bytes)?;
    let last = body
        .pointer("/meta/last_page")
        .and_then(Value::as_u64)
        .unwrap_or(1);
    match body.get_mut("data").map(Value::take) {
        Some(Value::Array(items)) => Ok((items, last)),
        _ => Err(error("Wallhaven response contains no wallpaper list")),
    }
}

pub struct Wallhaven {
    client: Client,
    network: NetworkOptions,
    api_root: Url,
    api_key: Option<String>,
    timeout: Duration,
    request_timeout: Duration,
    retries: u8,
    rate_limits: RateLimits,
}

struct ListingState {
    endpoint: Url,
    page: u64,
    index: usize,
    done: bool,
    pending: VecDeque<ExtractionEvent>,
}

impl Wallhaven {
    pub fn new(timeout: Duration) -> Result<Self> {
        Ok(Self {
            client: NetworkOptions::default().client(USER_AGENT, timeout)?,
            network: NetworkOptions::default(),
            api_root: Url::parse("https://wallhaven.cc/").expect("static URL"),
            api_key: None,
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

    /// Optional API key, sent as `X-API-Key`; required for NSFW content.
    pub fn with_api_key(mut self, key: Option<&str>) -> Self {
        self.api_key = key.map(Into::into);
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

    fn endpoint(&self, path: &str) -> Url {
        self.api_root.join(path).expect("validated Wallhaven path")
    }

    async fn call(&self, endpoint: &Url) -> Result<Vec<u8>> {
        tokio::time::timeout(self.timeout, self.request(endpoint))
            .await
            .map_err(|_| error("native Wallhaven extraction timed out"))?
    }

    fn finish(&self, mut items: Vec<Media>) -> VecDeque<ExtractionEvent> {
        for item in &mut items {
            self.network.apply_media(item);
        }
        group_with_keywords(
            items,
            "wallhaven",
            KeywordPolicy::shared(&["num", "index", "media_id", "filename", "name", "extension"]),
        )
    }

    async fn single(&self, id: &str) -> Result<VecDeque<ExtractionEvent>> {
        let body = self.call(&self.endpoint(&format!("api/v1/w/{id}"))).await?;
        Ok(self.finish(vec![parse_wallpaper(&body, id)?]))
    }

    async fn next_page(&self, state: &mut ListingState) -> Result<()> {
        let mut endpoint = state.endpoint.clone();
        endpoint
            .query_pairs_mut()
            .append_pair("page", &(state.page + 1).to_string());
        let body = self.call(&endpoint).await?;
        let (items, last) = parse_page(&body)?;
        state.page += 1;
        state.done = items.is_empty() || state.page >= last || state.page >= MAX_PAGES;
        for item in items {
            if let Some(media) = wallpaper(&item, state.index + 1) {
                state.index += 1;
                let events = self.finish(vec![media]);
                state.pending.extend(events);
            }
        }
        Ok(())
    }

    pub fn stream(&self, url: &Url) -> BoxStream<'_, Result<Media>> {
        crate::event::media_only(self.events(url))
    }

    /// Emit one post boundary per wallpaper; listings paginate lazily.
    pub fn events(&self, url: &Url) -> BoxStream<'_, Result<ExtractionEvent>> {
        let target = match target(url) {
            Ok(target) => target,
            Err(e) => return stream::once(async move { Err(e) }).boxed(),
        };
        let endpoint = match target {
            Target::Wallpaper { id } => {
                return stream::once(async move { self.single(&id).await })
                    .map(|result| match result {
                        Ok(events) => stream::iter(events.into_iter().map(Ok)).boxed(),
                        Err(e) => stream::once(async move { Err(e) }).boxed(),
                    })
                    .flatten()
                    .boxed();
            }
            Target::Search { query } => {
                let mut endpoint = self.endpoint("api/v1/search");
                if !query.is_empty() {
                    endpoint.query_pairs_mut().extend_pairs(&query);
                }
                endpoint
            }
            Target::Collection { user, id } => {
                self.endpoint(&format!("api/v1/collections/{user}/{id}"))
            }
        };
        let state = ListingState {
            endpoint,
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
            let mut request = self
                .client
                .get(target.clone())
                .timeout(self.request_timeout);
            if let Some(key) = &self.api_key {
                request = request.header("X-API-Key", key);
            }
            let response = request.send().await.map_err(|_| {
                Failure::transient("Wallhaven request failed; check network access")
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
                            "Wallhaven rate limit; retry after {} seconds",
                            delay.as_secs()
                        ),
                        None => "Wallhaven rate limit; retry later".into(),
                    }
                } else {
                    format!("Wallhaven returned HTTP {status}")
                },
            ));
        }
        if matches!(status.as_u16(), 404 | 410) {
            return Err(error("Wallhaven wallpaper, user or collection not found").into());
        }
        if matches!(status.as_u16(), 401 | 403) {
            return Err(error(
                "Wallhaven refused access; NSFW and private content need a valid api-key setting",
            )
            .into());
        }
        if !status.is_success() {
            return Err(error(format!("Wallhaven returned HTTP {status}")).into());
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| Failure::transient("could not read Wallhaven response"))?
        {
            if body.len() + chunk.len() > MAX_RESPONSE {
                return Err(error("Wallhaven response exceeded 16 MiB").into());
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

    const FULL: &str = "https://w.wallhaven.cc/full/94/wallhaven-94x38z.png";

    fn item(id: &str) -> Value {
        json!({"id": id, "path": format!("https://w.wallhaven.cc/full/{id}/wallhaven-{id}.jpg"),
            "resolution": "1920x1080", "file_type": "image/jpeg",
            "created_at": "2018-10-31 01:23:10"})
    }

    #[test]
    fn parses_wallpaper_and_listing() {
        let body = json!({"data": {"id": "94x38z", "path": FULL, "resolution": "2560x1440",
            "file_type": "image/png", "created_at": "2018-10-31 01:23:10",
            "uploader": {"username": "alice"},
            "tags": [{"name": "nature"}, {"name": "sky"}]}})
        .to_string();
        let media = parse_wallpaper(body.as_bytes(), "94x38z").unwrap();
        assert_eq!(media.url.as_str(), FULL);
        assert_eq!(media.filename, "wallhaven-94x38z.png");
        let meta = media.metadata.as_ref().unwrap();
        assert_eq!(
            (meta.post_id.as_str(), meta.media_id.as_str()),
            ("94x38z", "94x38z")
        );
        assert_eq!(meta.username.as_deref(), Some("alice"));
        assert_eq!(meta.caption.as_deref(), Some("nature sky"));
        assert_eq!(meta.timestamp, Some(1540948990));
        let file = meta.file.as_ref().unwrap();
        assert_eq!((file.width, file.height), (Some(2560), Some(1440)));
        let key = media.archive_key().unwrap();
        assert!(key.starts_with("wallhaven:"), "{key}");
        let source =
            wallhaven(Url::parse("http://127.0.0.1:1/").unwrap()).finish(vec![media.clone()]);
        let ExtractionEvent::Post(post) = &source[0] else {
            panic!("missing source post")
        };
        let KeywordPolicy::Shared { file_fields, .. } = &post.keywords else {
            panic!("wallhaven must reuse Directory keywords")
        };
        assert_eq!(
            file_fields.as_ref(),
            &["num", "index", "media_id", "filename", "name", "extension"]
        );
        assert!(parse_wallpaper(body.as_bytes(), "other").is_err());
        let bad = json!({"data": {"id": "1", "path": "http://w.wallhaven.cc/full/1.jpg"}});
        assert!(parse_wallpaper(bad.to_string().as_bytes(), "1").is_err());
        let evil = json!({"id": "1", "path": "https://w.wallhaven.cc.evil.example/1.jpg"});
        assert!(wallpaper(&evil, 1).is_none());
        let page = json!({"data": [item("a"), item("b")], "meta": {"last_page": 4}});
        let (items, last) = parse_page(page.to_string().as_bytes()).unwrap();
        assert_eq!((items.len(), last), (2, 4));
        assert!(parse_page(br#"{"error":"Nothing here"}"#).is_err());
        assert!(parse_page(b"nope").is_err());
    }

    #[test]
    fn classifies_supported_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        let w = Target::Wallpaper {
            id: "94x38z".into(),
        };
        assert_eq!(t("https://wallhaven.cc/w/94x38z").unwrap(), w);
        assert_eq!(t("https://whvn.cc/94x38z").unwrap(), w);
        assert_eq!(
            t("https://wallhaven.cc/search?q=touhou&categories=111&page=3&apikey=s").unwrap(),
            Target::Search {
                query: vec![
                    ("q".into(), "touhou".into()),
                    ("categories".into(), "111".into())
                ]
            }
        );
        assert_eq!(
            t("https://wallhaven.cc/user/AksumkA/uploads").unwrap(),
            Target::Search {
                query: vec![("q".into(), "@AksumkA".into())]
            }
        );
        assert_eq!(
            t("https://wallhaven.cc/user/AksumkA/favorites/74").unwrap(),
            Target::Collection {
                user: "AksumkA".into(),
                id: "74".into()
            }
        );
        for raw in [
            "https://wallhaven.cc/",
            "https://wallhaven.cc/w/",
            "https://wallhaven.cc/user/a/favorites/x",
            "https://w.wallhaven.cc/full/94/wallhaven-94x38z.jpg",
            "https://wallhaven.cc.evil.example/w/94x38z",
            "https://wallhaven.cc:444/w/94x38z",
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

    fn wallhaven(root: Url) -> Wallhaven {
        Wallhaven::new(Duration::from_secs(5))
            .unwrap()
            .with_network(&NetworkOptions::new(Some(String::new()), None).unwrap())
            .unwrap()
            .with_api_root(root)
    }

    #[tokio::test]
    async fn fetches_wallpaper_with_retry_and_key_header() {
        let (root, server) = serve(vec![
            response("503 Service Unavailable", b""),
            ok(json!({"data": item("abc")})),
        ]);
        let media: Vec<_> = wallhaven(root)
            .with_retries(1)
            .with_api_key(Some("secretkey"))
            .stream(&Url::parse("https://whvn.cc/abc").unwrap())
            .collect()
            .await;
        assert_eq!(media.len(), 1);
        assert_eq!(media[0].as_ref().unwrap().filename, "wallhaven-abc.jpg");
        let requests = server.join().unwrap();
        assert!(
            requests[1].starts_with("GET /api/v1/w/abc "),
            "{}",
            requests[1]
        );
        assert!(
            requests[1]
                .to_ascii_lowercase()
                .contains("x-api-key: secretkey")
        );
    }

    #[tokio::test]
    async fn paginates_uploads_and_hides_key_on_refusal() {
        let (root, server) = serve(vec![
            ok(json!({"data": [item("a"), {"id": "bad"}], "meta": {"last_page": 2}})),
            ok(json!({"data": [item("b")], "meta": {"last_page": 2}})),
        ]);
        let media: Vec<_> = wallhaven(root)
            .stream(&Url::parse("https://wallhaven.cc/user/alice/uploads").unwrap())
            .collect()
            .await;
        let media: Vec<_> = media.into_iter().map(|m| m.unwrap()).collect();
        let indexes: Vec<_> = media
            .iter()
            .map(|m| m.metadata.as_ref().unwrap().index)
            .collect();
        assert_eq!(indexes, [1, 2]);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/v1/search?q=%40alice&page=1 "));
        assert!(!requests[0].to_ascii_lowercase().contains("x-api-key"));
        assert!(requests[1].contains("&page=2 "));

        let (root, server) = serve(vec![response("401 Unauthorized", b"{}")]);
        let error = wallhaven(root)
            .with_api_key(Some("secretkey"))
            .stream(&Url::parse("https://wallhaven.cc/user/alice/favorites/7").unwrap())
            .collect::<Vec<_>>()
            .await
            .remove(0)
            .unwrap_err()
            .to_string();
        assert!(error.contains("api-key"));
        assert!(!error.contains("secretkey") && !error.contains("127.0.0.1"));
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/v1/collections/alice/7?page=1 "));
    }
}
