//! Experimental native Weasyl extraction.
//!
//! Mirrors the submission and gallery parts of gallery_dl/extractor/weasyl.py
//! using the public API. Submissions (`weasyl.com/~USER/submissions/ID/...`,
//! `/submission/ID`) use `api/submissions/ID/view`; user galleries (`/~USER`,
//! `/~USER/gallery`) page `api/users/USER/gallery?nextid=N` until `nextid` is
//! null (capped at 200 pages). An optional `api-key` is sent as the
//! `X-Weasyl-API-Key` header. Non-visual submissions without
//! `media.submission` are skipped. Media must be HTTPS on `*.weasyl.com`.
use crate::{
    Result, error,
    event::{ExtractionEvent, group},
    extract::{Media, MediaJob, PostMetadata},
    file_metadata::FileMetadata,
    imagehosts::clean_text,
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
    collections::{BTreeMap, HashSet, VecDeque},
    sync::Arc,
    time::{Duration, SystemTime},
};
use url::Url;

const USER_AGENT: &str = concat!(
    "gallery-dl-rs/",
    env!("CARGO_PKG_VERSION"),
    " (native Weasyl extractor)"
);
const MAX_RESPONSE: usize = 16 * 1024 * 1024;
const MAX_PAGES: u64 = 200;

/// Submission IDs are decimal.
pub fn valid_id(id: &str) -> bool {
    (1..=20).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_digit())
}

fn valid_user(user: &str) -> bool {
    (1..=64).contains(&user.len()) && user.bytes().all(|b| b.is_ascii_alphanumeric())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Submission { id: String },
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
            "unsupported Weasyl URL; expected https://www.weasyl.com/~USER/submissions/ID/..., /submission/ID or /~USER",
        )
    };
    if !plain(url) || !matches!(url.host_str(), Some("www.weasyl.com" | "weasyl.com")) {
        return Err(unsupported());
    }
    let parts: Vec<&str> = url
        .path_segments()
        .map(|s| s.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    let user = |raw: &str| {
        raw.strip_prefix('~')
            .filter(|u| valid_user(u))
            .map(str::to_ascii_lowercase)
    };
    match parts[..] {
        // Upstream also accepts `/view/ID` and `/submissions/USER`.
        ["submission" | "view", id, ..] if valid_id(id) => Ok(Target::Submission { id: id.into() }),
        ["submissions", name] if valid_user(name) => Ok(Target::Gallery {
            user: name.to_ascii_lowercase(),
        }),
        [owner, "submissions", id, ..] if user(owner).is_some() && valid_id(id) => {
            Ok(Target::Submission { id: id.into() })
        }
        [owner] | [owner, "gallery"] => user(owner)
            .map(|user| Target::Gallery { user })
            .ok_or_else(unsupported),
        _ => Err(unsupported()),
    }
}

pub fn is_weasyl_url(url: &Url) -> bool {
    target(url).is_ok()
}

/// HTTPS media on `weasyl.com` or `*.weasyl.com`.
pub fn media_url(raw: &str) -> Option<Url> {
    let url = Url::parse(raw.trim()).ok()?;
    let host = url.host_str()?;
    let allowed = host == "weasyl.com"
        || host.strip_suffix(".weasyl.com").is_some_and(|label| {
            !label.is_empty()
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.'))
        });
    (url.scheme() == "https" && plain(&url) && allowed).then_some(url)
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

fn id_string(value: &Value) -> Option<String> {
    match value {
        Value::Number(n) => n.as_u64().map(|n| n.to_string()),
        Value::String(s) if valid_id(s) => Some(s.clone()),
        _ => None,
    }
}

/// Convert one API submission object; `None` when it has no submission file.
pub fn submission_media(item: &Value) -> Option<Media> {
    let id = id_string(item.get("submitid")?)?;
    let url = item
        .pointer("/media/submission/0/url")
        .and_then(Value::as_str)
        .and_then(media_url)?;
    let sensitive: Arc<[Arc<str>]> = item
        .get("rating")
        .and_then(Value::as_str)
        .filter(|r| *r != "general")
        .map(|r| vec![Arc::<str>::from(r.to_ascii_lowercase())])
        .unwrap_or_default()
        .into();
    Some(Media {
        filename: format!("{id}.{}", extension(&url)),
        job: MediaJob::DirectHttp,
        category: "weasyl".into(),
        metadata: Some(PostMetadata {
            file: Some(FileMetadata {
                sensitive_flags: sensitive,
                ..FileMetadata::default()
            }),
            post_id: id.clone(),
            media_id: id,
            username: item
                .get("owner_login")
                .and_then(Value::as_str)
                .filter(|u| valid_user(u))
                .map(Arc::from),
            caption: item
                .get("title")
                .and_then(Value::as_str)
                .and_then(clean_text)
                .map(Arc::from),
            timestamp: item
                .get("posted_at")
                .and_then(Value::as_str)
                .and_then(|d| crate::date::parse_date(d).ok()),
            index: 1,
            ..PostMetadata::default()
        }),
        url,
        headers: BTreeMap::new(),
        handoff: BTreeMap::new(),
    })
}

fn json(bytes: &[u8]) -> Result<Value> {
    serde_json::from_slice(bytes).map_err(|_| error("Weasyl returned invalid JSON"))
}

/// Submissions and the next cursor from one gallery page.
pub fn parse_gallery(bytes: &[u8]) -> Result<(Vec<Value>, Option<String>)> {
    let mut value = json(bytes)?;
    let next = value.get("nextid").and_then(id_string);
    let items = match value.get_mut("submissions").map(Value::take) {
        Some(Value::Array(items)) => items,
        _ => return Err(error("Weasyl gallery response has no submissions")),
    };
    Ok((items, next))
}

pub struct Weasyl {
    client: Client,
    network: NetworkOptions,
    root: Url,
    api_key: Option<String>,
    timeout: Duration,
    request_timeout: Duration,
    retries: u8,
    rate_limits: RateLimits,
}

struct GalleryState {
    user: String,
    next: Option<String>,
    page: u64,
    done: bool,
    seen: HashSet<String>,
    pending: VecDeque<ExtractionEvent>,
}

impl Weasyl {
    pub fn new(timeout: Duration) -> Result<Self> {
        Ok(Self {
            client: NetworkOptions::default().client(USER_AGENT, timeout)?,
            network: NetworkOptions::default(),
            root: Url::parse("https://www.weasyl.com/").expect("static URL"),
            api_key: None,
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

    /// Send an API key as `X-Weasyl-API-Key`.
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

    /// Override the site origin; intended for local tests.
    pub fn with_root(mut self, root: Url) -> Self {
        self.root = root;
        self
    }

    async fn call(&self, path: &str) -> Result<Vec<u8>> {
        let endpoint = self
            .root
            .join(path)
            .map_err(|_| error("invalid Weasyl path"))?;
        tokio::time::timeout(self.timeout, self.request(&endpoint))
            .await
            .map_err(|_| error("native Weasyl extraction timed out"))?
    }

    fn emit(&self, mut media: Media) -> VecDeque<ExtractionEvent> {
        self.network.apply_media(&mut media);
        group(vec![media], "weasyl")
    }

    async fn submission(&self, id: &str) -> Result<VecDeque<ExtractionEvent>> {
        let body = self.call(&format!("api/submissions/{id}/view")).await?;
        let media = submission_media(&json(&body)?)
            .ok_or_else(|| error("Weasyl submission has no downloadable file"))?;
        Ok(self.emit(media))
    }

    async fn next_page(&self, state: &mut GalleryState) -> Result<()> {
        state.page += 1;
        let mut path = format!("api/users/{}/gallery", state.user);
        if let Some(next) = &state.next {
            path.push_str(&format!("?nextid={next}"));
        }
        let (items, next) = parse_gallery(&self.call(&path).await?)?;
        for item in &items {
            if let Some(media) = submission_media(item)
                && state
                    .seen
                    .insert(media.metadata.as_ref().unwrap().media_id.clone())
            {
                state.pending.extend(self.emit(media));
            }
        }
        state.done = next.is_none() || next == state.next || state.page >= MAX_PAGES;
        state.next = next;
        Ok(())
    }

    pub fn stream(&self, url: &Url) -> BoxStream<'_, Result<Media>> {
        crate::event::media_only(self.events(url))
    }

    /// Emit one post boundary per submission; galleries paginate lazily.
    pub fn events(&self, url: &Url) -> BoxStream<'_, Result<ExtractionEvent>> {
        let user = match target(url) {
            Ok(Target::Gallery { user }) => user,
            Ok(Target::Submission { id }) => {
                return stream::once(async move { self.submission(&id).await })
                    .map(|result| match result {
                        Ok(events) => stream::iter(events.into_iter().map(Ok)).boxed(),
                        Err(e) => stream::once(async move { Err(e) }).boxed(),
                    })
                    .flatten()
                    .boxed();
            }
            Err(e) => return stream::once(async move { Err(e) }).boxed(),
        };
        let state = GalleryState {
            user,
            next: None,
            page: 0,
            done: false,
            seen: HashSet::new(),
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
                .timeout(self.request_timeout)
                .header("Accept", "application/json");
            // Only send the API key to the configured origin.
            if let Some(key) = &self.api_key
                && target.origin() == self.root.origin()
            {
                request = request.header("X-Weasyl-API-Key", key);
            }
            let response = request
                .send()
                .await
                .map_err(|_| Failure::transient("Weasyl request failed; check network access"))?;
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
                            format!("Weasyl rate limit; retry after {} seconds", delay.as_secs())
                        }
                        None => "Weasyl rate limit; retry later".into(),
                    }
                } else {
                    format!("Weasyl returned HTTP {status}")
                },
            ));
        }
        if matches!(status.as_u16(), 404 | 410) {
            return Err(error("Weasyl submission or user not found").into());
        }
        if matches!(status.as_u16(), 401 | 403) {
            return Err(error("Weasyl refused access").into());
        }
        if !status.is_success() {
            return Err(error(format!("Weasyl returned HTTP {status}")).into());
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| Failure::transient("could not read Weasyl response"))?
        {
            if body.len() + chunk.len() > MAX_RESPONSE {
                return Err(error("Weasyl response exceeded 16 MiB").into());
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
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread::{self, JoinHandle},
    };

    fn submission_json(id: u64) -> String {
        format!(
            r#"{{"submitid":{id},"title":"Big &amp; Bold","owner_login":"alice","posted_at":"2023-09-21T13:23:00Z","rating":"mature","media":{{"submission":[{{"url":"https://cdn.weasyl.com/~alice/submissions/{id}/abc/alice-big.PNG"}}]}}}}"#
        )
    }

    #[test]
    fn view_and_submissions_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap()).unwrap();
        assert_eq!(
            t("https://www.weasyl.com/view/2031/a-wesley"),
            Target::Submission { id: "2031".into() }
        );
        assert_eq!(
            t("https://www.weasyl.com/submissions/Tanidareal"),
            Target::Gallery {
                user: "tanidareal".into()
            }
        );
    }

    #[test]
    fn parses_submissions_and_urls() {
        let value: Value = serde_json::from_str(&submission_json(42)).unwrap();
        let item = submission_media(&value).unwrap();
        assert_eq!(item.filename, "42.png");
        let meta = item.metadata.as_ref().unwrap();
        assert_eq!(
            (meta.post_id.as_str(), meta.media_id.as_str()),
            ("42", "42")
        );
        assert_eq!(meta.caption.as_deref(), Some("Big & Bold"));
        assert_eq!(meta.username.as_deref(), Some("alice"));
        assert_eq!(meta.timestamp, Some(1695302580));
        assert_eq!(
            meta.file.as_ref().unwrap().sensitive_flags[0].as_ref(),
            "mature"
        );
        assert!(item.archive_key().unwrap().starts_with("weasyl:"));
        let evil = submission_json(1).replace("cdn.weasyl.com", "cdn.weasyl.com.evil.example");
        assert!(submission_media(&serde_json::from_str(&evil).unwrap()).is_none());
        assert!(submission_media(&serde_json::json!({"submitid":1,"media":{}})).is_none());

        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        let sub = Target::Submission { id: "42".into() };
        assert_eq!(
            t("https://www.weasyl.com/~alice/submissions/42/big-bold").unwrap(),
            sub
        );
        assert_eq!(t("https://weasyl.com/submission/42").unwrap(), sub);
        let gallery = Target::Gallery {
            user: "alice".into(),
        };
        assert_eq!(t("https://www.weasyl.com/~Alice").unwrap(), gallery);
        assert_eq!(t("https://www.weasyl.com/~alice/gallery").unwrap(), gallery);
        for raw in [
            "https://www.weasyl.com/",
            "https://www.weasyl.com/~alice/favorites",
            "https://www.weasyl.com/submission/abc",
            "https://cdn.weasyl.com/~alice/submissions/1/a/b.png",
            "https://www.weasyl.com.evil.example/submission/1",
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

    fn weasyl(root: Url) -> Weasyl {
        Weasyl::new(Duration::from_secs(5))
            .unwrap()
            .with_network(&NetworkOptions::new(Some(String::new()), None).unwrap())
            .unwrap()
            .with_root(root)
    }

    #[tokio::test]
    async fn fetches_submission_with_retry_and_api_key() {
        let (root, server) = serve(vec![
            response("429 Too Many Requests", b""),
            response("200 OK", submission_json(42).as_bytes()),
        ]);
        let media: Vec<_> = weasyl(root)
            .with_api_key(Some("secretkey"))
            .with_retries(1)
            .stream(&Url::parse("https://www.weasyl.com/submission/42").unwrap())
            .collect()
            .await;
        assert_eq!(media.len(), 1);
        assert!(media[0].is_ok());
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /api/submissions/42/view "));
        assert!(
            requests[1]
                .to_ascii_lowercase()
                .contains("x-weasyl-api-key: secretkey"),
            "{}",
            requests[1]
        );
    }

    #[tokio::test]
    async fn paginates_gallery_by_nextid() {
        let page1 = format!(
            r#"{{"submissions":[{},{{"submitid":8,"media":{{}}}}],"nextid":7}}"#,
            submission_json(42)
        );
        let page2 = format!(
            r#"{{"submissions":[{},{}],"nextid":null}}"#,
            submission_json(42),
            submission_json(6)
        );
        let (root, server) = serve(vec![
            response("200 OK", page1.as_bytes()),
            response("200 OK", page2.as_bytes()),
        ]);
        let media: Vec<_> = weasyl(root)
            .stream(&Url::parse("https://www.weasyl.com/~alice/gallery").unwrap())
            .collect()
            .await;
        let ids: Vec<_> = media
            .into_iter()
            .map(|m| m.unwrap().metadata.unwrap().media_id)
            .collect();
        assert_eq!(ids, ["42", "6"]);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/users/alice/gallery "));
        assert!(
            !requests[0]
                .to_ascii_lowercase()
                .contains("x-weasyl-api-key")
        );
        assert!(requests[1].starts_with("GET /api/users/alice/gallery?nextid=7 "));
    }
}
