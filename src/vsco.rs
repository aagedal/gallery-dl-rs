//! Experimental native VSCO media and gallery extraction.
//!
//! Mirrors gallery_dl/extractor/vsco.py. Single media pages and user
//! galleries are read from the page-embedded `window.__PRELOADED_STATE__`
//! JSON, which is undocumented and may change without notice. Galleries take
//! the site ID and the anonymous `tkn` bearer token from that state, then page
//! through `api/3.0/medias/profile` by cursor (capped at 200 pages). Images
//! come from `responsiveUrl` (scheme-less, prefixed with `https://`); videos
//! are kept only when `videoUrl` is a progressive MP4, HLS playlists are
//! skipped. Media must be HTTPS on a `*.vsco.co` host.
use crate::{
    Result, error,
    event::{ExtractionEvent, group},
    extract::{Media, MediaJob, PostMetadata},
    file_metadata::FileMetadata,
    imagehosts::between,
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

const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:128.0) Gecko/20100101 Firefox/128.0";
const MAX_RESPONSE: usize = 16 * 1024 * 1024;
const MAX_PAGES: u64 = 200;
/// First path segments on vsco.co that are not user galleries.
const RESERVED: [&str; 16] = [
    "api",
    "about",
    "search",
    "spaces",
    "user",
    "feed",
    "store",
    "subscribe",
    "join",
    "login",
    "signup",
    "help",
    "static",
    "discover",
    "settings",
    "_",
];

/// Media IDs are 24-digit hex object IDs.
pub fn valid_id(id: &str) -> bool {
    (8..=32).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn valid_user(user: &str) -> bool {
    (1..=64).contains(&user.len())
        && user
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Media { user: String, id: String },
    Gallery { user: String },
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
            "unsupported VSCO URL; expected https://vsco.co/USER, https://vsco.co/USER/gallery or https://vsco.co/USER/media/ID",
        )
    };
    if !plain(url) || !matches!(url.host_str(), Some("vsco.co" | "www.vsco.co")) {
        return Err(unsupported());
    }
    let parts: Vec<&str> = url
        .path_segments()
        .map(|s| s.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    match parts[..] {
        [user, ..] if !valid_user(user) || RESERVED.contains(&user) => Err(unsupported()),
        [user, "media", id] if valid_id(id) => Ok(Target::Media {
            user: user.into(),
            id: id.into(),
        }),
        [user] | [user, "gallery" | "images"] => Ok(Target::Gallery { user: user.into() }),
        _ => Err(unsupported()),
    }
}

pub fn is_vsco_url(url: &Url) -> bool {
    target(url).is_ok()
}

/// HTTPS media on a `*.vsco.co` host; scheme-less URLs get `https://`.
pub fn media_url(raw: &str) -> Option<Url> {
    let raw = raw.trim();
    let raw = match raw.strip_prefix("//") {
        Some(rest) => format!("https://{rest}"),
        None if raw.contains("://") => raw.to_owned(),
        None => format!("https://{raw}"),
    };
    let url = Url::parse(&raw).ok()?;
    let host = url.host_str()?;
    (url.scheme() == "https"
        && plain(&url)
        && host.strip_suffix(".vsco.co").is_some_and(|label| {
            !label.is_empty()
                && label != "www"
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.'))
        }))
    .then_some(url)
}

/// Parse `window.__PRELOADED_STATE__` out of a VSCO page.
pub fn preloaded_state(html: &str) -> Result<Value> {
    let raw = between(html, "__PRELOADED_STATE__ = ", "</script>")
        .ok_or_else(|| error("VSCO page contains no preloaded state"))?;
    let raw = raw.trim().trim_end_matches(';');
    serde_json::from_str(raw)
        .or_else(|_| serde_json::from_str(&raw.replace(":undefined", ":null")))
        .map_err(|_| error("VSCO preloaded state is not valid JSON"))
}

fn field<'a>(value: &'a Value, camel: &str, snake: &str) -> Option<&'a Value> {
    value.get(camel).or_else(|| value.get(snake))
}

fn text(value: &Value, camel: &str, snake: &str) -> Option<String> {
    match field(value, camel, snake)? {
        Value::String(s) => Some(s.trim().to_owned()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
    .filter(|s| !s.is_empty())
}

fn number(value: &Value, key: &str) -> Option<u64> {
    value.get(key)?.as_u64().filter(|v| *v > 0)
}

fn extension(url: &Url, video: bool) -> String {
    url.path_segments()
        .and_then(|mut s| s.next_back())
        .and_then(|name| name.rsplit_once('.'))
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .filter(|ext| {
            (1..=5).contains(&ext.len()) && ext.bytes().all(|b| b.is_ascii_alphanumeric())
        })
        .map(|ext| if ext == "jpeg" { "jpg".into() } else { ext })
        .unwrap_or_else(|| if video { "mp4" } else { "jpg" }.into())
}

/// One media object (page state camelCase or API snake_case) as a download.
pub fn media_item(media: &Value, user: &str) -> Option<Media> {
    let id = text(media, "_id", "_id")
        .or_else(|| text(media, "id", "id"))
        .filter(|id| valid_id(id))?;
    let is_video = field(media, "isVideo", "is_video")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let (url, video) = match text(media, "videoUrl", "video_url") {
        Some(video) if is_video => {
            let url = media_url(&video)?;
            // HLS playlists need a manifest downloader; keep plain MP4 only.
            if !url.path().to_ascii_lowercase().ends_with(".mp4") {
                return None;
            }
            (url, true)
        }
        _ if is_video => return None,
        _ => (
            media_url(&text(media, "responsiveUrl", "responsive_url")?)?,
            false,
        ),
    };
    let username =
        text(media, "permaSubdomain", "perma_subdomain").unwrap_or_else(|| user.to_owned());
    let timestamp = field(media, "uploadDate", "upload_date")
        .and_then(Value::as_i64)
        .map(|ms| if ms > 100_000_000_000 { ms / 1000 } else { ms });
    Some(Media {
        filename: format!("{id}.{}", extension(&url, video)),
        job: MediaJob::DirectHttp,
        category: "vsco".into(),
        metadata: Some(PostMetadata {
            file: Some(FileMetadata {
                width: number(media, "width"),
                height: number(media, "height"),
                media_type: Some(Arc::from(if video { "video" } else { "photo" })),
                ..FileMetadata::default()
            }),
            post_id: id.clone(),
            media_id: id,
            username: Some(Arc::from(username)),
            caption: text(media, "description", "description").map(Arc::<str>::from),
            timestamp,
            index: 1,
            ..PostMetadata::default()
        }),
        url,
        headers: BTreeMap::new(),
        handoff: BTreeMap::new(),
    })
}

/// Parse a single media page.
pub fn parse_media_page(html: &[u8], user: &str, id: &str) -> Result<Vec<Media>> {
    let html = std::str::from_utf8(html).map_err(|_| error("VSCO returned invalid HTML"))?;
    let state = preloaded_state(html)?;
    let media = state
        .pointer(&format!("/medias/byId/{id}/media"))
        .ok_or_else(|| error("VSCO media not found"))?;
    Ok(media_item(media, user).into_iter().collect())
}

/// Site ID and bearer token from a gallery page.
pub fn parse_gallery_page(html: &[u8], user: &str) -> Result<(String, String)> {
    let html = std::str::from_utf8(html).map_err(|_| error("VSCO returned invalid HTML"))?;
    let state = preloaded_state(html)?;
    let site = state
        .pointer("/sites/siteByUsername")
        .and_then(Value::as_object)
        .and_then(|sites| {
            sites
                .get(user)
                .or_else(|| {
                    sites
                        .iter()
                        .find(|(name, _)| name.eq_ignore_ascii_case(user))
                        .map(|(_, v)| v)
                })
                .and_then(|s| s.get("site"))
                .and_then(|s| text(s, "id", "id"))
        })
        .filter(|id| id.len() <= 20 && id.bytes().all(|b| b.is_ascii_digit()))
        .ok_or_else(|| error("VSCO user not found"))?;
    let token = state
        .pointer("/users/currentUser/tkn")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| between(html, "\"tkn\":\"", "\"").map(str::to_owned))
        .filter(|t| {
            (8..=512).contains(&t.len())
                && t.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        })
        .ok_or_else(|| error("VSCO page contains no access token"))?;
    Ok((site, token))
}

/// Parse one `medias/profile` page into media and the next cursor.
pub fn parse_profile(bytes: &[u8], user: &str) -> Result<(Vec<Media>, Option<String>)> {
    let body: Value =
        serde_json::from_slice(bytes).map_err(|_| error("VSCO returned invalid JSON"))?;
    let list = body
        .get("media")
        .and_then(Value::as_array)
        .ok_or_else(|| error("VSCO response contains no media list"))?;
    let items = list
        .iter()
        .filter_map(|entry| {
            let kind = entry.get("type").and_then(Value::as_str).unwrap_or("image");
            let media = entry.get(kind).unwrap_or(entry);
            matches!(kind, "image" | "video")
                .then(|| media_item(media, user))
                .flatten()
        })
        .collect();
    let cursor = body
        .get("next_cursor")
        .and_then(Value::as_str)
        .filter(|c| !c.is_empty())
        .map(str::to_owned);
    Ok((items, cursor))
}

pub struct Vsco {
    client: Client,
    network: NetworkOptions,
    root: Url,
    timeout: Duration,
    request_timeout: Duration,
    retries: u8,
    rate_limits: RateLimits,
}

struct GalleryState {
    user: String,
    site: String,
    token: String,
    cursor: Option<String>,
    page: u64,
    done: bool,
    pending: VecDeque<ExtractionEvent>,
}

impl Vsco {
    pub fn new(timeout: Duration) -> Result<Self> {
        Ok(Self {
            client: NetworkOptions::default().client(USER_AGENT, timeout)?,
            network: NetworkOptions::default(),
            root: Url::parse("https://vsco.co/").expect("static URL"),
            timeout,
            request_timeout: timeout,
            retries: 0,
            rate_limits: RateLimits::default(),
        })
    }

    /// Apply proxy and user-agent settings to requests and emitted media.
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
    pub fn with_root(mut self, root: Url) -> Self {
        self.root = root;
        self
    }

    async fn call(&self, path: &str, token: Option<&str>) -> Result<Vec<u8>> {
        let endpoint = self
            .root
            .join(path)
            .map_err(|_| error("invalid VSCO path"))?;
        tokio::time::timeout(self.timeout, self.request(&endpoint, token))
            .await
            .map_err(|_| error("native VSCO extraction timed out"))?
    }

    fn finish(&self, items: Vec<Media>) -> VecDeque<ExtractionEvent> {
        let mut out = VecDeque::new();
        for mut item in items {
            self.network.apply_media(&mut item);
            out.extend(group(vec![item], "vsco"));
        }
        out
    }

    async fn media(&self, user: &str, id: &str) -> Result<VecDeque<ExtractionEvent>> {
        let body = self.call(&format!("{user}/media/{id}"), None).await?;
        Ok(self.finish(parse_media_page(&body, user, id)?))
    }

    async fn next_page(&self, state: &mut GalleryState) -> Result<()> {
        state.page += 1;
        let mut path = format!("api/3.0/medias/profile?site_id={}&limit=14", state.site);
        if let Some(cursor) = &state.cursor {
            let encoded: String = url::form_urlencoded::byte_serialize(cursor.as_bytes()).collect();
            path.push_str("&cursor=");
            path.push_str(&encoded);
        }
        let body = self.call(&path, Some(&state.token)).await?;
        let (items, cursor) = parse_profile(&body, &state.user)?;
        state.done = cursor.is_none() || cursor == state.cursor || state.page >= MAX_PAGES;
        state.cursor = cursor;
        state.pending.extend(self.finish(items));
        Ok(())
    }

    pub fn stream(&self, url: &Url) -> BoxStream<'_, Result<Media>> {
        crate::event::media_only(self.events(url))
    }

    /// Emit one post boundary per media; galleries paginate lazily.
    pub fn events(&self, url: &Url) -> BoxStream<'_, Result<ExtractionEvent>> {
        let user = match target(url) {
            Ok(Target::Gallery { user }) => user,
            Ok(Target::Media { user, id }) => {
                return stream::once(async move { self.media(&user, &id).await })
                    .map(|result| match result {
                        Ok(events) => stream::iter(events.into_iter().map(Ok)).boxed(),
                        Err(e) => stream::once(async move { Err(e) }).boxed(),
                    })
                    .flatten()
                    .boxed();
            }
            Err(e) => return stream::once(async move { Err(e) }).boxed(),
        };
        stream::unfold(Some(None::<GalleryState>), move |state| {
            let user = user.clone();
            async move {
                let mut state = match state? {
                    Some(state) => state,
                    None => {
                        let page = match self.call(&format!("{user}/gallery"), None).await {
                            Ok(page) => page,
                            Err(e) => return Some((Err(e), None)),
                        };
                        let (site, token) = match parse_gallery_page(&page, &user) {
                            Ok(found) => found,
                            Err(e) => return Some((Err(e), None)),
                        };
                        GalleryState {
                            user,
                            site,
                            token,
                            cursor: None,
                            page: 0,
                            done: false,
                            pending: VecDeque::new(),
                        }
                    }
                };
                loop {
                    if let Some(event) = state.pending.pop_front() {
                        return Some((Ok(event), Some(Some(state))));
                    }
                    if state.done {
                        return None;
                    }
                    if let Err(e) = self.next_page(&mut state).await {
                        return Some((Err(e), None));
                    }
                }
            }
        })
        .boxed()
    }

    async fn request(&self, endpoint: &Url, token: Option<&str>) -> Result<Vec<u8>> {
        for attempt in 0..=self.retries {
            match self.attempt(endpoint, token, attempt).await {
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
        token: Option<&str>,
        attempt: u8,
    ) -> std::result::Result<Vec<u8>, Failure> {
        let mut target = endpoint.clone();
        let mut redirects = Redirects::new(endpoint);
        let mut response = loop {
            self.rate_limits.wait(&target, self.timeout).await?;
            let mut request = self
                .client
                .get(target.clone())
                .timeout(self.request_timeout)
                .header("Accept-Language", "en-US,en;q=0.5")
                .header("Referer", "https://vsco.co/");
            // Only send the token to the origin it came from.
            if let Some(token) = token.filter(|_| target.origin() == endpoint.origin()) {
                request = request
                    .header("Authorization", format!("Bearer {token}"))
                    .header("Accept", "application/json")
                    .header("X-Client-Build", "1")
                    .header("X-Client-Platform", "web");
            }
            let response = request
                .send()
                .await
                .map_err(|_| Failure::transient("VSCO request failed; check network access"))?;
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
                            format!("VSCO rate limit; retry after {} seconds", delay.as_secs())
                        }
                        None => "VSCO rate limit; retry later".into(),
                    }
                } else {
                    format!("VSCO returned HTTP {status}")
                },
            ));
        }
        if matches!(status.as_u16(), 404 | 410) {
            return Err(error("VSCO media or user not found").into());
        }
        if matches!(status.as_u16(), 401 | 403) {
            return Err(error("VSCO refused access").into());
        }
        if !status.is_success() {
            return Err(error(format!("VSCO returned HTTP {status}")).into());
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| Failure::transient("could not read VSCO response"))?
        {
            if body.len() + chunk.len() > MAX_RESPONSE {
                return Err(error("VSCO response exceeded 16 MiB").into());
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

    const ID: &str = "5f1b2c3d4e5f6a7b8c9d0e1f";

    fn page(state: Value) -> Vec<u8> {
        format!(
            "<html><script>window.__PRELOADED_STATE__ = {}</script></html>",
            state.to_string().replace(":null", ":undefined")
        )
        .into_bytes()
    }

    fn media_state() -> Value {
        json!({"medias": {"byId": {ID: {"media": {
            "_id": ID, "responsiveUrl": "im.vsco.co/aws-us-west-2/aa/1/a.jpg",
            "width": 800, "height": 600, "uploadDate": 1704067200000u64,
            "description": "Sunset", "permaSubdomain": "alice", "isVideo": false,
            "extra": null}}}}})
    }

    #[test]
    fn parses_media_page_and_profile() {
        let items = parse_media_page(&page(media_state()), "alice", ID).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].url.as_str(),
            "https://im.vsco.co/aws-us-west-2/aa/1/a.jpg"
        );
        assert_eq!(items[0].filename, format!("{ID}.jpg"));
        let meta = items[0].metadata.as_ref().unwrap();
        assert_eq!(meta.media_id, ID);
        assert_eq!(meta.caption.as_deref(), Some("Sunset"));
        assert_eq!((meta.timestamp, meta.index), (Some(1704067200), 1));
        let file = meta.file.as_ref().unwrap();
        assert_eq!((file.width, file.height), (Some(800), Some(600)));
        assert!(items[0].archive_key().unwrap().starts_with("vsco:"));
        assert!(parse_media_page(&page(media_state()), "alice", "0123456789ab").is_err());

        let body = json!({"media": [
            {"type": "image", "image": {"_id": "aaaaaaaaaaaa", "responsive_url": "img.vsco.co/x/b.png"}},
            {"type": "video", "video": {"_id": "bbbbbbbbbbbb", "is_video": true, "video_url": "https://img.vsco.co/v/b.m3u8"}},
            {"type": "video", "video": {"_id": "cccccccccccc", "is_video": true, "video_url": "https://img.vsco.co/v/c.mp4"}},
            {"type": "image", "image": {"_id": "dddddddddddd", "responsive_url": "evil.example/x.jpg"}},
            {"type": "image", "image": {"_id": "eeeeeeeeeeee", "responsive_url": "http://im.vsco.co/x.jpg"}},
            {"type": "journal", "journal": {"_id": "ffffffffffff"}}],
            "next_cursor": "abc"});
        let (items, cursor) = parse_profile(body.to_string().as_bytes(), "alice").unwrap();
        let names: Vec<_> = items.iter().map(|m| m.filename.as_str()).collect();
        assert_eq!(names, ["aaaaaaaaaaaa.png", "cccccccccccc.mp4"]);
        assert_eq!(cursor.as_deref(), Some("abc"));
    }

    #[test]
    fn classifies_urls_and_hosts() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        let g = Target::Gallery {
            user: "alice".into(),
        };
        assert_eq!(t("https://vsco.co/alice").unwrap(), g);
        assert_eq!(t("https://vsco.co/alice/gallery").unwrap(), g);
        assert!(matches!(
            t(&format!("https://vsco.co/alice/media/{ID}")).unwrap(),
            Target::Media { .. }
        ));
        for raw in [
            "https://vsco.co/",
            "https://vsco.co/api/3.0/medias",
            "https://vsco.co/alice/media/",
            "https://im.vsco.co/aws-us-west-2/a.jpg",
            "https://vsco.co.evil.example/alice",
            "https://vsco.co:444/alice",
        ] {
            assert!(t(raw).is_err(), "{raw}");
        }
        assert!(media_url("im.vsco.co/a.jpg").is_some());
        assert!(media_url("//img.vsco.co/a.jpg").is_some());
        assert!(media_url("https://vsco.co/a.jpg").is_none());
        assert!(media_url("https://im.vsco.co.evil.example/a.jpg").is_none());
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

    fn vsco(root: Url) -> Vsco {
        Vsco::new(Duration::from_secs(5))
            .unwrap()
            .with_network(&NetworkOptions::new(Some(String::new()), None).unwrap())
            .unwrap()
            .with_root(root)
    }

    #[tokio::test]
    async fn fetches_media_with_retry() {
        let (root, server) = serve(vec![
            response("503 Service Unavailable", b""),
            response("200 OK", &page(media_state())),
        ]);
        let media: Vec<_> = vsco(root)
            .with_retries(1)
            .stream(&Url::parse(&format!("https://vsco.co/alice/media/{ID}")).unwrap())
            .collect()
            .await;
        assert_eq!(media.len(), 1);
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with(&format!("GET /alice/media/{ID} ")));
        assert!(!requests[1].to_ascii_lowercase().contains("authorization"));
    }

    #[tokio::test]
    async fn paginates_gallery_with_bearer_token() {
        let state = json!({"sites": {"siteByUsername": {"alice": {"site": {"id": 42}}}},
            "users": {"currentUser": {"tkn": "secrettoken123"}}});
        let entry = |id: &str| json!({"type": "image", "image": {"_id": id, "responsive_url": "im.vsco.co/a/x.jpg"}});
        let (root, server) = serve(vec![
            response("200 OK", &page(state)),
            response(
                "200 OK",
                json!({"media": [entry("aaaaaaaaaaaa")], "next_cursor": "c/2"})
                    .to_string()
                    .as_bytes(),
            ),
            response(
                "200 OK",
                json!({"media": [entry("bbbbbbbbbbbb")]})
                    .to_string()
                    .as_bytes(),
            ),
        ]);
        let media: Vec<_> = vsco(root)
            .stream(&Url::parse("https://vsco.co/alice/gallery").unwrap())
            .collect()
            .await;
        let media: Vec<_> = media.into_iter().map(|m| m.unwrap()).collect();
        assert_eq!(media.len(), 2);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /alice/gallery "));
        assert!(requests[1].starts_with("GET /api/3.0/medias/profile?site_id=42&limit=14 "));
        assert!(requests[1].contains("Bearer secrettoken123"));
        assert!(requests[2].contains("&cursor=c%2F2 "), "{}", requests[2]);

        let (root, server) = serve(vec![response("200 OK", b"<html>none</html>")]);
        let error = vsco(root)
            .stream(&Url::parse("https://vsco.co/alice").unwrap())
            .collect::<Vec<_>>()
            .await
            .remove(0)
            .unwrap_err()
            .to_string();
        assert!(error.contains("preloaded state"), "{error}");
        server.join().unwrap();
    }
}
