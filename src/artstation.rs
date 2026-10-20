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
    Project { hash: String },
    User { user: String },
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
        "www.artstation.com" | "artstation.com" => match parts[..] {
            ["artwork" | "projects", hash] if valid_hash(hash) => {
                Ok(Target::Project { hash: hash.into() })
            }
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
                [] | ["albums", "all"] => Ok(Target::User { user: user.into() }),
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

/// Parse a `projects/HASH.json` response into its image assets.
pub fn parse_project(bytes: &[u8], hash: &str) -> Result<Vec<Media>> {
    parse_project_data(bytes, hash).map(|(media, _)| media)
}
fn parse_project_data(bytes: &[u8], hash: &str) -> Result<(Vec<Media>, usize)> {
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
        if text(asset, "asset_type").as_deref() != Some("image")
            || asset.get("has_image").and_then(Value::as_bool) == Some(false)
        {
            continue;
        }
        let Some(id) = text(asset, "id").filter(|id| valid_id(id)) else {
            continue;
        };
        let Some(url) = text(asset, "image_url").and_then(|u| media_url(&u)) else {
            continue;
        };
        items.push(Media {
            filename: format!("{hash}_{id}.{}", extension(&url)),
            job: MediaJob::DirectHttp,
            category: "artstation".into(),
            metadata: Some(PostMetadata {
                file: Some(FileMetadata {
                    width: number(asset, "width"),
                    height: number(asset, "height"),
                    media_type: Some(Arc::from("photo")),
                    ..FileMetadata::default()
                }),
                post_id: hash.into(),
                media_id: id,
                username: username.clone(),
                caption: caption.clone(),
                timestamp,
                index: position + 1,
                ..PostMetadata::default()
            }),
            url,
            headers: BTreeMap::new(),
            handoff: BTreeMap::new(),
        });
    }
    Ok((items, assets.len()))
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
}

struct ListingState {
    user: String,
    page: u64,
    seen: u64,
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

    /// Override the site origin; intended for local tests.
    pub fn with_api_root(mut self, root: Url) -> Self {
        self.api_root = root;
        self
    }

    async fn call(&self, path: &str) -> Result<Vec<u8>> {
        let endpoint = self.api_root.join(path).expect("validated ArtStation path");
        tokio::time::timeout(self.timeout, self.request(&endpoint))
            .await
            .map_err(|_| error("native ArtStation extraction timed out"))?
    }

    async fn project(&self, hash: &str) -> Result<VecDeque<ExtractionEvent>> {
        let body = self.call(&format!("projects/{hash}.json")).await?;
        let (mut items, count) = parse_project_data(&body, hash)?;
        for item in &mut items {
            self.network.apply_media(item);
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
        let body = self
            .call(&format!(
                "users/{}/projects.json?page={}",
                state.user, state.page
            ))
            .await?;
        let (hashes, total) = parse_listing(&body)?;
        state.seen += hashes.len() as u64;
        state.done = hashes.is_empty() || state.seen >= total || state.page >= MAX_PAGES;
        for hash in hashes {
            let events = self.project(&hash).await?;
            state.pending.extend(events);
        }
        Ok(())
    }

    pub fn stream(&self, url: &Url) -> BoxStream<'_, Result<Media>> {
        crate::event::media_only(self.events(url))
    }

    /// Emit one post boundary per project; portfolios paginate lazily.
    pub fn events(&self, url: &Url) -> BoxStream<'_, Result<ExtractionEvent>> {
        let user = match target(url) {
            Ok(Target::User { user }) => user,
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
            user,
            page: 0,
            seen: 0,
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
            for (name, value) in BROWSER_HEADERS {
                request = request.header(name, value);
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
        let items = parse_project(body.as_bytes(), "LQVJr").unwrap();
        assert_eq!(items.len(), 2);
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
            "https://www.artstation.com/alice/albums/5",
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
        assert_eq!(events.len(), 6);
        for (position, group) in events.as_chunks::<3>().0.iter().enumerate() {
            let ExtractionEvent::Post(post) = group[0].as_ref().unwrap() else {
                panic!("one source Directory per asset")
            };
            assert_eq!(post.count, Some(6));
            let KeywordPolicy::Shared {
                file_fields,
                post_fields,
            } = &post.keywords
            else {
                panic!("ArtStation asset must reuse Directory keywords")
            };
            assert_eq!(post_fields["num"], [2, 6][position]);
            let filter = crate::filter::FilterExpression::parse("num == 2 and count == 6").unwrap();
            assert_eq!(filter.matches_post(post).unwrap(), position == 0);
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
        assert_eq!(media.len(), 4);
        assert_eq!(media[2].filename, "b_12.png");
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
}
