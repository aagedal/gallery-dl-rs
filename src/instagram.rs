//! Experimental native Instagram REST media and collection extraction.
#[path = "instagram_collections.rs"]
mod collections;
#[path = "instagram_stories.rs"]
mod stories;
use crate::{
    Result,
    cookies::CookieJar,
    error,
    event::{self, ExtractionEvent},
    extract::{Media, PostMetadata, http_url},
    network::NetworkOptions,
    transport::{RateLimits, Redirects, retry_after},
};
pub use collections::is_unordered_collection_url;
use futures_util::{
    StreamExt, TryStreamExt,
    stream::{self, BoxStream},
};
use reqwest::{Client, StatusCode, header};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    path::Path,
    sync::Mutex,
    time::{Duration, SystemTime},
};
pub use stories::is_story_url;
use url::Url;

const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
const MAX_RESPONSE: usize = 16 * 1024 * 1024;

pub struct Instagram {
    client: Client,
    network: NetworkOptions,
    cookies: Mutex<CookieJar>,
    timeout: Duration,
    request_timeout: Duration,
    retries: u8,
    rate_limits: RateLimits,
}

impl Instagram {
    pub async fn new(cookies: Option<&Path>, timeout: Duration) -> Result<Self> {
        let cookies = match cookies {
            Some(path) => CookieJar::parse(&tokio::fs::read_to_string(path).await?)?,
            None => CookieJar::parse("")?,
        };
        Self::from_cookie_jar(cookies, timeout)
    }

    /// Build an extractor from an already scoped in-memory cookie jar.
    pub fn from_cookie_jar(cookies: CookieJar, timeout: Duration) -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .user_agent(USER_AGENT)
                .timeout(timeout)
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            network: NetworkOptions::default(),
            cookies: Mutex::new(cookies),
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

    /// Configure additional attempts for HTTP 429/5xx and transport failures.
    /// The extraction timeout covers all attempts and waits. Defaults to zero.
    pub fn with_retries(mut self, retries: u8) -> Self {
        self.retries = retries;
        self
    }

    /// Bound each HTTP request separately from the overall extraction deadline.
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    pub async fn extract(&self, post: &Url) -> Result<Vec<Media>> {
        let code = shortcode(post)?;
        let id = media_id(&code)?;
        let endpoint = http_url(&format!(
            "https://www.instagram.com/api/v1/media/{id}/info/"
        ))?;
        self.extract_endpoint(&endpoint, &code).await
    }

    async fn extract_endpoint(&self, endpoint: &Url, code: &str) -> Result<Vec<Media>> {
        let data = self.request_json(endpoint, code, self.timeout).await?;
        let mut media = parse_value(&data, code)?;
        for item in &mut media {
            self.network.apply_media(item);
        }
        Ok(media)
    }

    async fn request_json(&self, endpoint: &Url, referer: &str, budget: Duration) -> Result<Value> {
        self.request_api(endpoint, referer, budget, None).await
    }

    async fn request_form(
        &self,
        endpoint: &Url,
        form: &[(String, String)],
        budget: Duration,
    ) -> Result<Value> {
        self.request_api(endpoint, "", budget, Some(form)).await
    }

    async fn request_api(
        &self,
        endpoint: &Url,
        referer: &str,
        budget: Duration,
        form: Option<&[(String, String)]>,
    ) -> Result<Value> {
        tokio::time::timeout(budget, async {
            for attempt in 0..=self.retries {
                match self.attempt(endpoint, referer, attempt, form).await {
                    Ok(data) => return Ok(data),
                    Err(failure) if failure.retry && attempt < self.retries => {
                        tokio::time::sleep(backoff(attempt)).await;
                    }
                    Err(failure) => return Err(failure.error),
                }
            }
            unreachable!()
        })
        .await
        .map_err(|_| error("native Instagram extraction timed out"))?
    }

    /// Lazily traverse posts and collections. Only the current response page is
    /// buffered; downstream pauses do not consume the active extraction budget.
    pub fn stream(&self, url: &Url) -> BoxStream<'_, Result<Media>> {
        event::media_only(self.events(url))
    }

    /// Post boundaries are explicit even when a reel contains no available files.
    pub fn events(&self, url: &Url) -> BoxStream<'_, Result<ExtractionEvent>> {
        if shortcode(url).is_ok() {
            let url = url.clone();
            return stream::once(async move { self.extract(&url).await })
                .map_ok(|items| stream::iter(event::group(items, "instagram").into_iter().map(Ok)))
                .try_flatten()
                .boxed();
        }
        if let Ok(input) = stories::input(url) {
            return self.story_events(
                input,
                Url::parse("https://www.instagram.com/api/v1/").unwrap(),
            );
        }
        if let Ok(input) = collections::input(url) {
            return self.collection_events(
                input,
                Url::parse("https://www.instagram.com/api/v1/").unwrap(),
            );
        }
        match profile_username(url) {
            Ok(username) => self.profile_events(
                username,
                Url::parse("https://www.instagram.com/api/v1/").unwrap(),
            ),
            Err(e) => stream::once(async move { Err(e) }).boxed(),
        }
    }

    #[cfg(test)]
    fn profile_stream(&self, username: String, base: Url) -> BoxStream<'_, Result<Media>> {
        event::media_only(self.profile_events(username, base))
    }

    fn profile_events(
        &self,
        username: String,
        base: Url,
    ) -> BoxStream<'_, Result<ExtractionEvent>> {
        let state = ProfileState {
            username,
            base,
            user_id: None,
            cursor: None,
            seen: HashSet::new(),
            pending: VecDeque::new(),
            done: false,
            budget: self.timeout,
        };
        stream::unfold(state, move |mut state| async move {
            loop {
                if let Some(item) = state.pending.pop_front() {
                    return Some((Ok(item), state));
                }
                if state.done {
                    return None;
                }
                let started = tokio::time::Instant::now();
                let result =
                    tokio::time::timeout(state.budget, self.profile_page(&mut state)).await;
                state.budget = state.budget.saturating_sub(started.elapsed());
                let result =
                    result.unwrap_or_else(|_| Err(error("native Instagram extraction timed out")));
                if let Err(e) = result {
                    state.done = true;
                    return Some((Err(e), state));
                }
            }
        })
        .boxed()
    }

    async fn profile_page(&self, state: &mut ProfileState) -> Result<()> {
        if state.user_id.is_none() {
            let mut endpoint = state.base.join("users/web_profile_info/")?;
            endpoint
                .query_pairs_mut()
                .append_pair("username", &state.username);
            let data = self.request_json(&endpoint, "", state.budget).await?;
            validate_response(&data)?;
            let user = data
                .pointer("/data/user")
                .ok_or_else(|| error("Instagram profile unavailable"))?;
            if !user
                .get("username")
                .and_then(Value::as_str)
                .is_some_and(|name| name.eq_ignore_ascii_case(&state.username))
            {
                return Err(error(
                    "Instagram response does not match the requested profile",
                ));
            }
            if user.get("is_private").and_then(Value::as_bool) == Some(true)
                && user.get("followed_by_viewer").and_then(Value::as_bool) != Some(true)
            {
                return Err(error(
                    "Instagram private profile requires an authorized session; refresh --cookies",
                ));
            }
            state.user_id = Some(identifier(user.get("id"))?);
            return Ok(());
        }
        let user_id = state.user_id.as_deref().unwrap();
        let mut endpoint = state.base.join(&format!("feed/user/{user_id}/"))?;
        endpoint.query_pairs_mut().append_pair("count", "30");
        if let Some(cursor) = &state.cursor {
            endpoint.query_pairs_mut().append_pair("max_id", cursor);
        }
        let data = self.request_json(&endpoint, "", state.budget).await?;
        validate_response(&data)?;
        let posts = data
            .get("items")
            .and_then(Value::as_array)
            .ok_or_else(|| error("Instagram profile feed items are missing"))?;
        let more = data
            .get("more_available")
            .and_then(Value::as_bool)
            .ok_or_else(|| error("Instagram profile pagination state is missing"))?;
        let cursor = if more {
            let cursor = data
                .get("next_max_id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty() && s.len() <= 8192)
                .ok_or_else(|| {
                    error("Instagram profile pagination cursor is missing or invalid")
                })?;
            if !state
                .seen
                .insert(<[u8; 32]>::from(Sha256::digest(cursor.as_bytes())))
            {
                return Err(error("Instagram profile pagination cursor repeated"));
            }
            Some(cursor.to_owned())
        } else {
            None
        };
        let mut pending = VecDeque::new();
        for post in posts {
            if identifier(post.pointer("/user/pk"))? != user_id {
                return Err(error(
                    "Instagram feed post does not match the requested profile",
                ));
            }
            let code = post
                .get("code")
                .and_then(Value::as_str)
                .ok_or_else(|| error("Instagram feed post shortcode is missing"))?;
            media_id(code)?;
            let mut media = parse_post(post, code)?;
            for item in &mut media {
                self.network.apply_media(item);
            }
            pending.extend(event::group(media, "instagram"));
        }
        state.pending = pending;
        state.cursor = cursor;
        state.done = !more;
        Ok(())
    }

    async fn attempt(
        &self,
        endpoint: &Url,
        code: &str,
        attempt: u8,
        mut form: Option<&[(String, String)]>,
    ) -> std::result::Result<Value, Failure> {
        let mut target = endpoint.clone();
        let mut redirects = Redirects::new(endpoint);
        let mut response = loop {
            self.rate_limits.wait(&target, self.timeout).await?;
            let mut request = self
                .client
                .request(
                    if form.is_some() {
                        reqwest::Method::POST
                    } else {
                        reqwest::Method::GET
                    },
                    target.clone(),
                )
                .timeout(self.request_timeout)
                .header(header::ACCEPT, "application/json")
                .header(
                    header::REFERER,
                    if code.is_empty() {
                        "https://www.instagram.com/".into()
                    } else {
                        format!("https://www.instagram.com/p/{code}/")
                    },
                )
                .header("X-IG-App-ID", "936619743392459")
                .header("X-Requested-With", "XMLHttpRequest")
                .header("X-IG-WWW-Claim", "0");
            if let Some(form) = form {
                request = request.form(form);
            }
            if let Some(cookies) = self.cookies.lock().unwrap().header(&target) {
                if let Some(token) = cookies
                    .split("; ")
                    .find_map(|s| s.strip_prefix("csrftoken="))
                {
                    request = request.header("X-CSRFToken", token);
                }
                request = request.header(header::COOKIE, cookies);
            }
            let response = request.send().await.map_err(|_| {
                Failure::transient("Instagram request failed; check network access")
            })?;
            self.cookies
                .lock()
                .unwrap()
                .store_response(response.url(), response.headers());
            match redirects.next(response.url(), response.status(), response.headers())? {
                Some(next) => {
                    // API normalization is allowed; login/challenge pages are terminal.
                    if !next.path().starts_with("/api/") {
                        return Err(error("Instagram redirected outside the API; login or a session challenge may be required").into());
                    }
                    // Match HTTP client redirect semantics: 307/308 retain the
                    // method/body; 301/302/303 turn POST into a body-free GET.
                    if matches!(response.status().as_u16(), 301..=303) {
                        form = None;
                    }
                    target = next;
                }
                None => break response,
            }
        };
        let status = response.status();
        if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
            let delay = retry_after(response.headers(), SystemTime::now());
            // Keep final-attempt cooldowns for subsequent posts on this instance.
            self.rate_limits
                .defer(endpoint, delay.unwrap_or_else(|| backoff(attempt)));
            return Err(Failure::transient(
                if status == StatusCode::TOO_MANY_REQUESTS {
                    match delay {
                        Some(delay) => format!(
                            "Instagram rate limit; retry after {} seconds",
                            delay.as_secs()
                        ),
                        None => "Instagram rate limit; retry later".into(),
                    }
                } else {
                    format!("Instagram returned HTTP {status}")
                },
            ));
        }
        if matches!(status.as_u16(), 401 | 403) {
            return Err(error(
                "Instagram authentication required or session expired; refresh --cookies",
            )
            .into());
        }
        if matches!(status.as_u16(), 404 | 410) {
            return Err(error("Instagram post unavailable or inaccessible to this session").into());
        }
        if !status.is_success() {
            return Err(error(format!("Instagram returned HTTP {status}")).into());
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| Failure::transient("could not read Instagram response"))?
        {
            if body.len() + chunk.len() > MAX_RESPONSE {
                return Err(error("Instagram response exceeded 16 MiB").into());
            }
            body.extend_from_slice(&chunk);
        }
        let data: Value = serde_json::from_slice(&body).map_err(|_| {
            error("Instagram returned a web page or invalid JSON; login may be required")
        })?;
        Ok(data)
    }
}

struct ProfileState {
    username: String,
    base: Url,
    user_id: Option<String>,
    cursor: Option<String>,
    seen: HashSet<[u8; 32]>,
    pending: VecDeque<ExtractionEvent>,
    done: bool,
    budget: Duration,
}

pub fn profile_username(url: &Url) -> Result<String> {
    let parts: Vec<_> = url.path().trim_matches('/').split('/').collect();
    let username = parts[0];
    if !matches!(url.scheme(), "http" | "https")
        || !matches!(
            url.host_str(),
            Some("instagram.com" | "www.instagram.com" | "m.instagram.com")
        )
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || !(parts.len() == 1 || (parts.len() == 2 && parts[1] == "feed"))
        || username.is_empty()
        || username.len() > 30
        || !username
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.'))
        || matches!(
            username.to_ascii_lowercase().as_str(),
            "p" | "reel" | "reels" | "tv" | "explore" | "accounts" | "stories" | "direct"
        )
    {
        return Err(error(
            "native Instagram supports posts and profile feeds; use --extractor compatibility for other URLs",
        ));
    }
    Ok(username.into())
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

pub fn shortcode(url: &Url) -> Result<String> {
    if !matches!(url.scheme(), "http" | "https")
        || !matches!(
            url.host_str(),
            Some("instagram.com" | "www.instagram.com" | "m.instagram.com")
        )
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return Err(error("native extraction requires an Instagram post URL"));
    }
    let parts: Vec<_> = url.path().trim_end_matches('/').split('/').collect();
    if parts.len() != 3 || !matches!(parts[1], "p" | "reel" | "tv") {
        return Err(error(
            "native Instagram supports /p/, /reel/, and /tv/ single posts; use --extractor compatibility for other URLs",
        ));
    }
    media_id(parts[2])?;
    Ok(parts[2].into())
}

fn media_id(code: &str) -> Result<u64> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let invalid = || error("invalid Instagram post shortcode");
    if code.is_empty() || code.len() > 11 {
        return Err(invalid());
    }
    code.bytes().try_fold(0u64, |id, c| {
        let digit = ALPHABET.iter().position(|&v| v == c).ok_or_else(invalid)? as u64;
        id.checked_mul(64)
            .and_then(|v| v.checked_add(digit))
            .ok_or_else(invalid)
    })
}

/// Parse a complete REST media-info response, failing the whole carousel if any
/// child is missing. A video thumbnail is never substituted for missing video.
pub fn parse_response(bytes: &[u8], code: &str) -> Result<Vec<Media>> {
    let data: Value = serde_json::from_slice(bytes).map_err(|_| {
        error("Instagram returned a web page or invalid JSON; login may be required")
    })?;
    parse_value(&data, code)
}

fn validate_response(data: &Value) -> Result<()> {
    let message = data.get("message").and_then(Value::as_str).unwrap_or("");
    if data.get("challenge").is_some()
        || matches!(
            message,
            "login_required" | "challenge_required" | "checkpoint_required"
        )
    {
        return Err(error(
            "Instagram authentication required or session expired; refresh --cookies and resolve any session challenge",
        ));
    }
    if matches!(
        message,
        "Please wait a few minutes before you try again."
            | "feedback_required"
            | "rate_limit_error"
    ) {
        return Err(error("Instagram rate limit; retry later"));
    }
    if data.get("status").and_then(Value::as_str) != Some("ok") {
        return Err(error(
            "Instagram could not return this post; it may be unavailable or inaccessible",
        ));
    }
    Ok(())
}

fn parse_value(data: &Value, code: &str) -> Result<Vec<Media>> {
    validate_response(data)?;
    let posts = data
        .get("items")
        .and_then(Value::as_array)
        .filter(|items| items.len() == 1)
        .ok_or_else(|| error("Instagram post unavailable or unexpected media-info response"))?;
    let post = &posts[0];
    if post.get("code").and_then(Value::as_str) != Some(code) {
        return Err(error(
            "Instagram response does not match the requested post",
        ));
    }
    parse_post(post, code)
}

fn parse_post(post: &Value, code: &str) -> Result<Vec<Media>> {
    let post_id = identifier(post.get("pk"))?;
    let kind = post.get("media_type").and_then(Value::as_u64);
    let children: Vec<&Value> = if kind == Some(8) {
        let children = post
            .get("carousel_media")
            .and_then(Value::as_array)
            .filter(|items| !items.is_empty())
            .ok_or_else(|| error("Instagram carousel has no media"))?;
        if post
            .get("carousel_media_count")
            .and_then(Value::as_u64)
            .is_some_and(|count| count != children.len() as u64)
        {
            return Err(error("Instagram returned an incomplete carousel"));
        }
        children.iter().collect()
    } else {
        vec![post]
    };
    children
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let kind = item.get("media_type").and_then(Value::as_u64);
            let candidates = match kind {
                Some(1) => item.pointer("/image_versions2/candidates"),
                Some(2) => item.get("video_versions"),
                _ => return Err(error("unsupported Instagram media type")),
            }
            .and_then(Value::as_array)
            .ok_or_else(|| error("Instagram media variants are missing"))?;
            let variant = candidates
                .iter()
                .filter(|v| v.get("url").and_then(Value::as_str).is_some())
                .max_by_key(|v| {
                    u128::from(v.get("width").and_then(Value::as_u64).unwrap_or(0))
                        * u128::from(v.get("height").and_then(Value::as_u64).unwrap_or(0))
                })
                .ok_or_else(|| error("Instagram has no direct media variant"))?;
            let url = http_url(variant["url"].as_str().unwrap())?;
            if url.path().ends_with(".m3u8") || url.path().ends_with(".mpd") {
                return Err(error(
                    "Instagram returned a streaming manifest instead of direct media",
                ));
            }
            let media_id = identifier(item.get("pk"))?;
            let extension = if kind == Some(2) {
                "mp4"
            } else {
                match url.path().rsplit('.').next().unwrap_or("") {
                    "png" => "png",
                    "webp" => "webp",
                    "avif" => "avif",
                    "heic" => "heic",
                    _ => "jpg",
                }
            };
            Ok(Media {
                url,
                job: crate::extract::MediaJob::DirectHttp,
                handoff: BTreeMap::new(),
                category: "instagram".into(),
                filename: format!("{post_id}_{media_id}.{extension}"),
                metadata: Some(PostMetadata {
                    file: Some(file_metadata(item, variant, kind == Some(2))),
                    frames: None,
                    community: None,
                    community_archive: false,
                    retweet_id: None,
                    quoted_by_id: None,
                    reposted_by_id: None,
                    article: None,
                    instance: None,
                    post_id: post_id.clone(),
                    media_id,
                    shortcode: code.into(),
                    username: post
                        .pointer("/user/username")
                        .and_then(Value::as_str)
                        .map(std::sync::Arc::from),
                    caption: post
                        .pointer("/caption/text")
                        .and_then(Value::as_str)
                        .map(std::sync::Arc::from),
                    timestamp: post.get("taken_at").and_then(Value::as_i64),
                    index: index + 1,
                }),
                headers: BTreeMap::from([
                    ("User-Agent".into(), USER_AGENT.into()),
                    ("Referer".into(), "https://www.instagram.com/".into()),
                ]),
            })
        })
        .collect()
}

/// Metadata follows the selected carousel child and representation, never its
/// parent post or a video's image preview.
fn file_metadata(item: &Value, variant: &Value, video: bool) -> crate::file_metadata::FileMetadata {
    let dimension =
        |value: Option<&Value>| value.and_then(Value::as_u64).filter(|value| *value > 0);
    crate::file_metadata::FileMetadata {
        width: dimension(variant.get("width")),
        height: dimension(variant.get("height")),
        width_original: dimension(item.get("original_width")),
        height_original: dimension(item.get("original_height")),
        duration: if video {
            item.get("video_duration")
                .and_then(Value::as_f64)
                .filter(|value| value.is_finite() && *value >= 0.0)
        } else {
            None
        },
        media_type: Some(std::sync::Arc::from(if video { "video" } else { "image" })),
        description: item
            .get("accessibility_caption")
            .and_then(Value::as_str)
            .map(std::sync::Arc::from),
        ..Default::default()
    }
}

fn identifier(value: Option<&Value>) -> Result<String> {
    let value = match value {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) if n.is_u64() => n.to_string(),
        _ => return Err(error("Instagram media identifier is missing")),
    };
    if value.is_empty() || !value.bytes().all(|c| c.is_ascii_digit()) {
        return Err(error("invalid Instagram media identifier"));
    }
    Ok(value)
}

#[cfg(test)]
mod transport_tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread::{self, JoinHandle},
    };

    const PHOTO: &[u8] = include_bytes!("../tests/fixtures/instagram/photo.json");

    #[tokio::test]
    async fn request_timeout_is_separate_from_the_overall_extraction_deadline() {
        let (endpoint, server) = serve_once("200 OK", "", PHOTO, Duration::from_millis(200));
        let native = Instagram::new(None, Duration::from_secs(3))
            .await
            .unwrap()
            .with_request_timeout(Duration::from_millis(50));
        let message = native
            .extract_endpoint(&endpoint, "BA")
            .await
            .unwrap_err()
            .to_string();
        assert!(message.contains("Instagram request failed"), "{message}");
        server.join().unwrap();
    }

    #[tokio::test]
    async fn configured_user_agent_matches_api_and_media_requests() {
        let (endpoint, server) = serve_once("200 OK", "", PHOTO, Duration::ZERO);
        let network = NetworkOptions::new(None, Some("gallery-dl-native-test/1".into())).unwrap();
        let native = Instagram::new(None, Duration::from_secs(3))
            .await
            .unwrap()
            .with_network(&network)
            .unwrap();
        let media = native.extract_endpoint(&endpoint, "BA").await.unwrap();
        assert!(!media.is_empty());
        for item in media {
            assert_eq!(
                item.headers.get("User-Agent").map(String::as_str),
                Some("gallery-dl-native-test/1")
            );
        }
        let request = server.join().unwrap().to_ascii_lowercase();
        assert!(request.contains("user-agent: gallery-dl-native-test/1\r\n"));
    }

    #[tokio::test]
    async fn api_requests_use_configured_http_proxy() {
        let (proxy, server) = serve_once("200 OK", "", PHOTO, Duration::ZERO);
        let network =
            NetworkOptions::new(Some(proxy.origin().ascii_serialization()), None).unwrap();
        let native = Instagram::new(None, Duration::from_secs(3))
            .await
            .unwrap()
            .with_network(&network)
            .unwrap();
        let mut endpoint = proxy;
        endpoint.set_host(Some("native-api.invalid")).unwrap();
        endpoint.set_port(None).unwrap();
        let media = native.extract_endpoint(&endpoint, "BA").await.unwrap();
        assert!(!media.is_empty());
        let request = server.join().unwrap();
        assert!(request.starts_with(&format!("GET {endpoint} HTTP/1.1\r\n")));
    }

    #[tokio::test]
    async fn profile_user_agent_matches_api_and_media_requests() {
        let profile = serde_json::to_vec(&serde_json::json!({
            "status": "ok", "data": {"user": {"id": "42", "username": "fixture_user"}}
        }))
        .unwrap();
        let mut feed: Value = serde_json::from_slice(PHOTO).unwrap();
        feed["items"][0]["user"]["pk"] = Value::String("42".into());
        feed["more_available"] = Value::Bool(false);
        let (endpoint, server) = serve_sequence(vec![
            response("200 OK", "", &profile),
            response("200 OK", "", &serde_json::to_vec(&feed).unwrap()),
        ]);
        let network = NetworkOptions::new(None, Some("gallery-dl-native-test/1".into())).unwrap();
        let native = Instagram::new(None, Duration::from_secs(3))
            .await
            .unwrap()
            .with_network(&network)
            .unwrap();
        let media: Vec<_> = native
            .profile_stream("fixture_user".into(), endpoint)
            .try_collect()
            .await
            .unwrap();
        assert_eq!(media.len(), 1);
        let file = media[0].metadata.as_ref().unwrap().file.as_ref().unwrap();
        assert_eq!((file.width, file.height), (Some(1080), Some(1080)));
        assert_eq!(file.media_type.as_deref(), Some("image"));
        assert_eq!(
            media[0].headers.get("User-Agent").map(String::as_str),
            Some("gallery-dl-native-test/1")
        );
        for (_, _, request) in server.join().unwrap() {
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("user-agent: gallery-dl-native-test/1\r\n")
            );
        }
    }

    #[tokio::test]
    async fn story_requests_rotate_scoped_cookies_and_apply_configured_media_agent() {
        let profile = serde_json::to_vec(
            &serde_json::json!({"status":"ok","data":{"user":{"id":"42","username":"fixture"}}}),
        )
        .unwrap();
        let reel = serde_json::to_vec(&serde_json::json!({"status":"ok","reels_media":[{"id":"42","user":{"pk":"42","username":"fixture"},"items":[{"pk":"101","media_type":1,"image_versions2":{"candidates":[{"url":"https://cdn.example/story.jpg","width":10,"height":10}]}}]}]})).unwrap();
        let (mut endpoint, server) = serve_sequence(vec![
            response(
                "200 OK",
                "Set-Cookie: sessionid=rotated; Path=/\r\nSet-Cookie: csrftoken=rotated-csrf; Path=/\r\n",
                &profile,
            ),
            response("200 OK", "", &reel),
        ]);
        endpoint.set_path("/api/v1/");
        let network = NetworkOptions::new(None, Some("native-story-test/1".into())).unwrap();
        let native = Instagram::from_cookie_jar(CookieJar::parse("127.0.0.1\tFALSE\t/\tFALSE\t0\tsessionid\tinitial\n127.0.0.1\tFALSE\t/\tFALSE\t0\tcsrftoken\tinitial-csrf\n").unwrap(),Duration::from_secs(3)).unwrap().with_network(&network).unwrap();
        let input =
            stories::input(&Url::parse("https://instagram.com/stories/fixture/").unwrap()).unwrap();
        let media: Vec<_> = native
            .story_stream(input, endpoint)
            .try_collect()
            .await
            .unwrap();
        assert_eq!(media.len(), 1);
        let file = media[0].metadata.as_ref().unwrap().file.as_ref().unwrap();
        assert_eq!((file.width, file.height), (Some(10), Some(10)));
        assert_eq!(file.media_type.as_deref(), Some("image"));
        assert_eq!(
            media[0].headers.get("User-Agent").map(String::as_str),
            Some("native-story-test/1")
        );
        assert!(
            !media[0]
                .headers
                .keys()
                .any(|key| key.eq_ignore_ascii_case("cookie"))
        );
        let requests = server.join().unwrap();
        assert!(requests[0].2.contains("sessionid=initial"));
        assert!(requests[1].2.contains("sessionid=rotated"));
        assert!(
            requests[1]
                .2
                .to_ascii_lowercase()
                .contains("x-csrftoken: rotated-csrf\r\n")
        );
    }

    fn serve_once(
        status: &str,
        extra_headers: &str,
        body: &[u8],
        delay: Duration,
    ) -> (Url, JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = Url::parse(&format!(
            "http://{}/api/v1/media/64/info/",
            listener.local_addr().unwrap()
        ))
        .unwrap();
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{extra_headers}\r\n",
            body.len()
        );
        let body = body.to_vec();
        listener.set_nonblocking(true).unwrap();
        let task = thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(std::time::Instant::now() < deadline, "server timed out");
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(e) => panic!("{e}"),
                }
            };
            // Windows can inherit the listener's nonblocking mode.
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let request = read_request(&mut stream);
            thread::sleep(delay);
            // A timed-out client can disconnect before the response is written.
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.write_all(&body);
            request
        });
        (endpoint, task)
    }

    // Raw responses also allow exercising connection loss and truncated bodies.
    type Arrivals = Vec<(std::time::Instant, SystemTime, String)>;

    fn serve_sequence(responses: Vec<Vec<u8>>) -> (Url, JoinHandle<Arrivals>) {
        serve_delayed_sequence(
            responses
                .into_iter()
                .map(|body| (body, Duration::ZERO))
                .collect(),
        )
    }

    fn serve_delayed_sequence(responses: Vec<(Vec<u8>, Duration)>) -> (Url, JoinHandle<Arrivals>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = Url::parse(&format!(
            "http://{}/api/v1/media/64/info/",
            listener.local_addr().unwrap()
        ))
        .unwrap();
        let task = thread::spawn(move || {
            let mut arrivals = Vec::new();
            for (response, delay) in responses {
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(std::time::Instant::now() < deadline, "server timed out");
                            thread::sleep(Duration::from_millis(2));
                        }
                        Err(e) => panic!("{e}"),
                    }
                };
                let arrival = (std::time::Instant::now(), SystemTime::now());
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let request = read_request(&mut stream);
                arrivals.push((arrival.0, arrival.1, request));
                thread::sleep(delay);
                let _ = stream.write_all(&response);
            }
            arrivals
        });
        (endpoint, task)
    }

    fn read_request(stream: &mut std::net::TcpStream) -> String {
        let mut request = Vec::new();
        let mut buffer = [0; 1024];
        loop {
            if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                let headers = std::str::from_utf8(&request[..end]).unwrap();
                let length = headers
                    .lines()
                    .find_map(|line| {
                        line.split_once(':')
                            .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                            .map(|(_, value)| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                assert!(length <= 64 * 1024);
                if request.len() >= end + 4 + length {
                    break;
                }
            }
            let count = stream.read(&mut buffer).unwrap();
            assert_ne!(count, 0, "client closed before completing request");
            request.extend_from_slice(&buffer[..count]);
            assert!(request.len() <= 128 * 1024);
        }
        String::from_utf8(request).unwrap()
    }

    #[tokio::test]
    async fn form_retries_restart_original_post_and_preserve_bodies_through_307_308() {
        let (endpoint, server) = serve_sequence(vec![
            response(
                "307 Temporary Redirect",
                "Location: /api/v1/clips/moved/\r\nSet-Cookie: csrftoken=updated; Path=/\r\n",
                b"",
            ),
            response("503 Service Unavailable", "Retry-After: 0\r\n", b""),
            response(
                "308 Permanent Redirect",
                "Location: /api/v1/clips/final/\r\n",
                b"",
            ),
            response("200 OK", "", br#"{"status":"ok"}"#),
        ]);
        let native = Instagram::from_cookie_jar(
            CookieJar::parse("127.0.0.1\tFALSE\t/\tFALSE\t0\tcsrftoken\toriginal\n").unwrap(),
            Duration::from_secs(3),
        )
        .unwrap()
        .with_retries(1);
        let form = vec![
            ("max_id".to_owned(), "private+cursor&next=1".to_owned()),
            ("target_user_id".into(), "42".into()),
        ];
        native
            .request_form(&endpoint, &form, Duration::from_secs(3))
            .await
            .unwrap();
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 4);
        for (_, _, request) in &requests {
            assert!(request.starts_with("POST "));
            let (headers, body) = request.split_once("\r\n\r\n").unwrap();
            assert!(
                headers
                    .to_ascii_lowercase()
                    .contains("content-type: application/x-www-form-urlencoded")
            );
            let fields: Vec<_> = url::form_urlencoded::parse(body.as_bytes())
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect();
            assert_eq!(fields, form);
        }
        for (_, _, request) in &requests[1..] {
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("x-csrftoken: updated\r\n")
            );
        }
        assert_eq!(requests[0].2.lines().next(), requests[2].2.lines().next());
        assert!(requests[1].2.starts_with("POST /api/v1/clips/moved/ "));
        assert!(requests[3].2.starts_with("POST /api/v1/clips/final/ "));
    }

    #[tokio::test]
    async fn form_301_302_303_redirects_switch_to_get_without_form_headers_or_body() {
        for status in ["301 Moved Permanently", "302 Found", "303 See Other"] {
            let (endpoint, server) = serve_sequence(vec![
                response(status, "Location: /api/v1/clips/final/\r\n", b""),
                response("200 OK", "", br#"{"status":"ok"}"#),
            ]);
            let native = Instagram::new(None, Duration::from_secs(3)).await.unwrap();
            native
                .request_form(
                    &endpoint,
                    &[("max_id".into(), "private-body".into())],
                    Duration::from_secs(3),
                )
                .await
                .unwrap();
            let requests = server.join().unwrap();
            assert!(requests[0].2.starts_with("POST "));
            assert!(requests[1].2.starts_with("GET /api/v1/clips/final/ "));
            let (headers, body) = requests[1].2.split_once("\r\n\r\n").unwrap();
            assert!(body.is_empty());
            assert!(!headers.to_ascii_lowercase().contains("content-type:"));
            assert!(!requests[1].2.contains("private-body"));
        }
    }

    #[tokio::test]
    async fn form_auth_and_unsafe_redirect_failures_do_not_retry_or_disclose_body() {
        for (status, headers) in [
            ("302 Found", "Location: /accounts/login/?private-value\r\n"),
            (
                "307 Temporary Redirect",
                "Location: https://other.invalid/api/private-value\r\n",
            ),
            ("403 Forbidden", ""),
        ] {
            let (endpoint, server) = serve_sequence(vec![response(status, headers, b"")]);
            let native = Instagram::new(None, Duration::from_secs(3))
                .await
                .unwrap()
                .with_retries(2);
            let message = native
                .request_form(
                    &endpoint,
                    &[("max_id".into(), "private-value".into())],
                    Duration::from_secs(3),
                )
                .await
                .unwrap_err()
                .to_string();
            assert!(!message.contains("private-value"), "{message}");
            assert_eq!(server.join().unwrap().len(), 1);
        }
    }

    fn response(status: &str, headers: &str, body: &[u8]) -> Vec<u8> {
        let mut response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(body);
        response
    }

    #[tokio::test]
    async fn rotates_cookies_on_redirect_retry_and_subsequent_posts_without_modifying_file() {
        let (endpoint, server) = serve_sequence(vec![
            response(
                "302 Found",
                concat!(
                    "Location: /api/other/info/\r\n",
                    "Set-Cookie: csrftoken=redirected; Path=/\r\n",
                    "Set-Cookie: sessionid=rotated; HttpOnly; Path=/\r\n",
                    "Set-Cookie: local=path-only; Path=/api/other/\r\n",
                    "Set-Cookie: foreign=excluded; Domain=evil.test; Path=/\r\n",
                ),
                b"",
            ),
            response(
                "503 Service Unavailable",
                "Retry-After: 0\r\nSet-Cookie: csrftoken=retried; Path=/\r\n",
                b"",
            ),
            response(
                "200 OK",
                "Set-Cookie: csrftoken=next-post; Path=/\r\n",
                PHOTO,
            ),
            response("200 OK", "", PHOTO),
        ]);
        let file = tempfile::NamedTempFile::new().unwrap();
        let original = concat!(
            "127.0.0.1\tFALSE\t/\tFALSE\t0\tcsrftoken\toriginal\n",
            "127.0.0.1\tFALSE\t/\tFALSE\t0\tsessionid\tinitial\n",
        );
        std::fs::write(file.path(), original).unwrap();
        let instagram = Instagram::new(Some(file.path()), Duration::from_secs(3))
            .await
            .unwrap()
            .with_retries(1);
        for _ in 0..2 {
            let media = instagram.extract_endpoint(&endpoint, "BA").await.unwrap();
            assert_eq!(media.len(), 1);
            assert!(
                !media[0]
                    .headers
                    .keys()
                    .any(|key| key.eq_ignore_ascii_case("cookie")
                        || key.eq_ignore_ascii_case("x-csrftoken"))
            );
            let json = serde_json::to_string(&media).unwrap();
            for private in ["initial", "rotated", "next-post", "path-only"] {
                assert!(!json.contains(private));
            }
        }
        let arrivals = server.join().unwrap();
        for (index, token) in ["original", "redirected", "retried", "next-post"]
            .iter()
            .enumerate()
        {
            let request = arrivals[index].2.to_ascii_lowercase();
            assert!(request.contains(&format!("x-csrftoken: {token}\r\n")));
            assert!(request.contains(&format!("csrftoken={token}")));
            assert!(request.contains(if index == 0 {
                "sessionid=initial"
            } else {
                "sessionid=rotated"
            }));
            assert_eq!(request.contains("local=path-only"), index == 1);
            assert!(!request.contains("excluded"));
        }
        assert_eq!(std::fs::read_to_string(file.path()).unwrap(), original);
    }

    #[tokio::test]
    async fn applies_cookie_deletions_on_authentication_failure_before_next_post() {
        let (endpoint, server) = serve_sequence(vec![
            response(
                "401 Unauthorized",
                "Set-Cookie: sessionid=; Path=/; Max-Age=0\r\nSet-Cookie: csrftoken=; Path=/; Max-Age=0\r\n",
                b"",
            ),
            response("200 OK", "", PHOTO),
        ]);
        let mut instagram = Instagram::new(None, Duration::from_secs(3))
            .await
            .unwrap()
            .with_retries(1);
        instagram.cookies = CookieJar::parse(concat!(
            "127.0.0.1\tFALSE\t/\tFALSE\t0\tsessionid\tinitial\n",
            "127.0.0.1\tFALSE\t/\tFALSE\t0\tcsrftoken\tinitial\n",
        ))
        .unwrap()
        .into();
        assert!(
            instagram
                .extract_endpoint(&endpoint, "BA")
                .await
                .unwrap_err()
                .to_string()
                .contains("authentication")
        );
        instagram.extract_endpoint(&endpoint, "BA").await.unwrap();
        let arrivals = server.join().unwrap();
        assert!(
            arrivals[0]
                .2
                .to_ascii_lowercase()
                .contains("sessionid=initial")
        );
        let request = arrivals[1].2.to_ascii_lowercase();
        assert!(!request.contains("cookie:"));
        assert!(!request.contains("x-csrftoken:"));
    }

    #[tokio::test]
    async fn redirects_reselect_cookie_and_csrf_scope_and_retry_from_original_url() {
        let (endpoint, server) = serve_sequence(vec![
            response("302 Found", "Location: /api/other/info/\r\n", b""),
            response("503 Service Unavailable", "Retry-After: 0\r\n", b""),
            response(
                "307 Temporary Redirect",
                "Location: /api/other/info/\r\n",
                b"",
            ),
            response("200 OK", "", PHOTO),
        ]);
        let mut instagram = Instagram::new(None, Duration::from_secs(3))
            .await
            .unwrap()
            .with_retries(1);
        instagram.cookies = CookieJar::parse(concat!(
            "127.0.0.1\tFALSE\t/api/v1/\tFALSE\t0\tcsrftoken\toriginal-token\n",
            "127.0.0.1\tFALSE\t/api/other/\tFALSE\t0\tcsrftoken\tredirect-token\n",
            "127.0.0.1\tFALSE\t/\tFALSE\t0\tsessionid\tlocal-session\n",
        ))
        .unwrap()
        .into();
        assert_eq!(
            instagram
                .extract_endpoint(&endpoint, "BA")
                .await
                .unwrap()
                .len(),
            1
        );
        let requests = server.join().unwrap();
        for (index, (_, _, request)) in requests.iter().enumerate() {
            let (path, token, excluded) = if index % 2 == 0 {
                ("/api/v1/media/64/info/", "original-token", "redirect-token")
            } else {
                ("/api/other/info/", "redirect-token", "original-token")
            };
            let request = request.to_ascii_lowercase();
            assert!(request.starts_with(&format!("get {path} http/1.1\r\n")));
            assert!(request.contains(&format!("x-csrftoken: {token}\r\n")));
            assert!(request.contains(&format!(
                "cookie: csrftoken={token}; sessionid=local-session\r\n"
            )));
            assert!(!request.contains(excluded));
        }
    }

    #[tokio::test]
    async fn login_redirects_and_loops_are_terminal_even_with_retries() {
        for (location, diagnostic) in [
            ("/accounts/login/?secret=hidden", "login"),
            ("/challenge/secret", "challenge"),
            ("/api/v1/media/64/info/#secret", "loop"),
        ] {
            let (endpoint, server) = serve_sequence(vec![response(
                "302 Found",
                &format!("Location: {location}\r\n"),
                b"",
            )]);
            let instagram = Instagram::new(None, Duration::from_secs(2))
                .await
                .unwrap()
                .with_retries(2);
            let error = instagram
                .extract_endpoint(&endpoint, "BA")
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains(diagnostic), "{error}");
            assert!(!error.contains("secret"));
            assert_eq!(server.join().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn redirect_chain_uses_one_extraction_deadline() {
        let (endpoint, server) = serve_delayed_sequence(vec![
            (
                response("302 Found", "Location: /api/next/\r\n", b""),
                Duration::from_millis(400),
            ),
            (response("200 OK", "", PHOTO), Duration::from_millis(800)),
        ]);
        let instagram = Instagram::new(None, Duration::from_secs(1)).await.unwrap();
        let error = instagram
            .extract_endpoint(&endpoint, "BA")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("timed out"), "{error}");
        assert_eq!(server.join().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn retries_transient_statuses_and_transport_failures() {
        for transient in [
            response("429 Too Many Requests", "Retry-After: 0\r\n", b""),
            response("503 Service Unavailable", "Retry-After: invalid\r\n", b""),
            Vec::new(), // Connection closed before response headers.
            b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\n{".to_vec(),
        ] {
            let (endpoint, server) = serve_sequence(vec![transient, response("200 OK", "", PHOTO)]);
            let instagram = Instagram::new(None, Duration::from_secs(3))
                .await
                .unwrap()
                .with_retries(1);
            assert_eq!(
                instagram
                    .extract_endpoint(&endpoint, "BA")
                    .await
                    .unwrap()
                    .len(),
                1
            );
            let arrivals = server.join().unwrap();
            assert!(arrivals[1].0.duration_since(arrivals[0].0) >= Duration::from_millis(250));
        }
    }

    #[tokio::test]
    async fn honors_numeric_and_http_date_cooldowns() {
        for use_date in [false, true] {
            let value = if use_date {
                httpdate::fmt_http_date(SystemTime::now() + Duration::from_secs(2))
            } else {
                "1".to_owned()
            };
            let (endpoint, server) = serve_sequence(vec![
                response(
                    "429 Too Many Requests",
                    &format!("Retry-After: {value}\r\n"),
                    b"",
                ),
                response("200 OK", "", PHOTO),
            ]);
            let instagram = Instagram::new(None, Duration::from_secs(4))
                .await
                .unwrap()
                .with_retries(1);
            instagram.extract_endpoint(&endpoint, "BA").await.unwrap();
            let arrivals = server.join().unwrap();
            if use_date {
                assert!(arrivals[1].1 >= httpdate::parse_http_date(&value).unwrap());
            } else {
                assert!(arrivals[1].0.duration_since(arrivals[0].0) >= Duration::from_secs(1));
            }
        }
    }

    #[tokio::test]
    async fn retains_final_attempt_cooldown_for_the_next_post() {
        let (endpoint, server) = serve_sequence(vec![
            response("429 Too Many Requests", "Retry-After: 1\r\n", b""),
            response("200 OK", "", PHOTO),
        ]);
        let instagram = Instagram::new(None, Duration::from_secs(3)).await.unwrap();
        assert!(
            instagram
                .extract_endpoint(&endpoint, "BA")
                .await
                .unwrap_err()
                .to_string()
                .contains("rate limit")
        );
        let mut next = endpoint.clone();
        next.set_path("/api/v1/media/65/info/");
        instagram.extract_endpoint(&next, "BA").await.unwrap();
        let arrivals = server.join().unwrap();
        assert!(arrivals[1].0.duration_since(arrivals[0].0) >= Duration::from_secs(1));
    }

    #[tokio::test]
    async fn bounds_retries_and_does_not_retry_terminal_errors() {
        let (endpoint, server) =
            serve_sequence(vec![response("503 Service Unavailable", "", b""); 2]);
        let instagram = Instagram::new(None, Duration::from_secs(3))
            .await
            .unwrap()
            .with_retries(1);
        assert!(
            instagram
                .extract_endpoint(&endpoint, "BA")
                .await
                .unwrap_err()
                .to_string()
                .contains("503")
        );
        assert_eq!(server.join().unwrap().len(), 2);
        for (status, body, expected) in [
            ("401 Unauthorized", b"".as_slice(), "authentication"),
            ("403 Forbidden", b"".as_slice(), "authentication"),
            ("404 Not Found", b"".as_slice(), "unavailable"),
            ("410 Gone", b"".as_slice(), "unavailable"),
            ("302 Found", b"".as_slice(), "redirect"),
            ("400 Bad Request", b"".as_slice(), "400"),
            ("200 OK", b"<html>login</html>".as_slice(), "invalid JSON"),
            (
                "200 OK",
                include_bytes!("../tests/fixtures/instagram/rate-limit.json").as_slice(),
                "rate limit",
            ),
        ] {
            let (endpoint, server) = serve_sequence(vec![response(status, "", body)]);
            let instagram = Instagram::new(None, Duration::from_secs(3))
                .await
                .unwrap()
                .with_retries(2);
            assert!(
                instagram
                    .extract_endpoint(&endpoint, "BA")
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains(expected)
            );
            assert_eq!(server.join().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn bounds_total_retry_time_and_rejects_excessive_cooldowns() {
        let (endpoint, server) =
            serve_sequence(vec![response("503 Service Unavailable", "", b""); 3]);
        let instagram = Instagram::new(None, Duration::from_millis(1500))
            .await
            .unwrap()
            .with_retries(10);
        let start = std::time::Instant::now();
        let error = instagram
            .extract_endpoint(&endpoint, "BA")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("timed out"), "{error}");
        assert!(start.elapsed() < Duration::from_secs(3));
        assert_eq!(server.join().unwrap().len(), 3);

        let (endpoint, server) = serve_sequence(vec![response(
            "429 Too Many Requests",
            "Retry-After: 999999999999999999999999\r\n",
            b"",
        )]);
        let instagram = Instagram::new(None, Duration::from_secs(1))
            .await
            .unwrap()
            .with_retries(1);
        let error = instagram
            .extract_endpoint(&endpoint, "BA")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("retry later"), "{error}");
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn cancellation_stops_cooldown_wait_without_losing_the_deadline() {
        let (endpoint, server) = serve_sequence(vec![
            response("429 Too Many Requests", "Retry-After: 1\r\n", b""),
            response("200 OK", "", PHOTO),
        ]);
        let instagram = Instagram::new(None, Duration::from_secs(3))
            .await
            .unwrap()
            .with_retries(1);
        assert!(
            tokio::time::timeout(
                Duration::from_millis(400),
                instagram.extract_endpoint(&endpoint, "BA")
            )
            .await
            .is_err()
        );
        // Dropping the previous future must not leave a worker making retries,
        // and a new call must still honor the previously observed cooldown.
        instagram.extract_endpoint(&endpoint, "BA").await.unwrap();
        let arrivals = server.join().unwrap();
        assert_eq!(arrivals.len(), 2);
        assert!(arrivals[1].0.duration_since(arrivals[0].0) >= Duration::from_secs(1));
    }

    #[tokio::test]
    async fn sends_api_headers_and_scoped_cookies_then_extracts_fixture() {
        let (endpoint, server) = serve_once("200 OK", "", PHOTO, Duration::ZERO);
        let mut instagram = Instagram::new(None, Duration::from_secs(2)).await.unwrap();
        instagram.cookies = CookieJar::parse(concat!(
            "127.0.0.1\tFALSE\t/api/\tFALSE\t0\tcsrftoken\tlocal-token\n",
            "127.0.0.1\tFALSE\t/\tFALSE\t0\tsessionid\tlocal-session\n",
            "127.0.0.1\tFALSE\t/elsewhere\tFALSE\t0\twrongpath\texcluded\n",
            "127.0.0.1\tFALSE\t/\tTRUE\t0\tsecure\texcluded\n",
            ".instagram.com\tTRUE\t/\tFALSE\t0\tforeign\texcluded\n",
        ))
        .unwrap()
        .into();

        let media = instagram.extract_endpoint(&endpoint, "BA").await.unwrap();
        assert_eq!(media.len(), 1);
        assert_eq!(media[0].url.as_str(), "https://cdn.example/64.jpg");
        assert!(!media[0].headers.keys().any(
            |key| key.eq_ignore_ascii_case("cookie") || key.eq_ignore_ascii_case("x-csrftoken")
        ));
        let request = server.join().unwrap().to_ascii_lowercase();
        assert!(request.starts_with("get /api/v1/media/64/info/ http/1.1\r\n"));
        for header in [
            "accept: application/json\r\n",
            "referer: https://www.instagram.com/p/ba/\r\n",
            "x-ig-app-id: 936619743392459\r\n",
            "x-requested-with: xmlhttprequest\r\n",
            "x-ig-www-claim: 0\r\n",
            "x-csrftoken: local-token\r\n",
            "cookie: csrftoken=local-token; sessionid=local-session\r\n",
        ] {
            assert!(
                request.contains(header),
                "missing expected header {header:?}"
            );
        }
        assert!(request.contains(&format!(
            "user-agent: {}\r\n",
            USER_AGENT.to_ascii_lowercase()
        )));
        assert!(!request.contains("excluded"));
    }

    #[tokio::test]
    async fn refuses_redirect_without_forwarding_request_or_credentials() {
        let target = TcpListener::bind("127.0.0.1:0").unwrap();
        target.set_nonblocking(true).unwrap();
        let location = format!(
            "Location: http://{}/stolen\r\n",
            target.local_addr().unwrap()
        );
        let (endpoint, server) = serve_once("302 Found", &location, b"", Duration::ZERO);
        let mut instagram = Instagram::new(None, Duration::from_secs(2)).await.unwrap();
        instagram.cookies = CookieJar::parse("127.0.0.1\tFALSE\t/\tFALSE\t0\tsessionid\tsecret\n")
            .unwrap()
            .into();
        let error = instagram
            .extract_endpoint(&endpoint, "BA")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("same origin"));
        assert!(!error.contains("secret"));
        assert!(server.join().unwrap().contains("sessionid=secret"));
        assert_eq!(
            target.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[tokio::test]
    async fn reports_authentication_unavailable_and_rate_limit_statuses() {
        for (status, headers, expected) in [
            (
                "401 Unauthorized",
                "",
                "authentication required or session expired",
            ),
            (
                "403 Forbidden",
                "",
                "authentication required or session expired",
            ),
            ("404 Not Found", "", "post unavailable or inaccessible"),
            (
                "429 Too Many Requests",
                "Retry-After: 37\r\n",
                "rate limit; retry after 37 seconds",
            ),
        ] {
            let (endpoint, server) = serve_once(status, headers, b"", Duration::ZERO);
            let instagram = Instagram::new(None, Duration::from_secs(2)).await.unwrap();
            let error = instagram
                .extract_endpoint(&endpoint, "BA")
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains(expected), "unexpected diagnostic: {error}");
            server.join().unwrap();
        }
    }

    #[tokio::test]
    async fn bounds_slow_response_time() {
        let (endpoint, server) = serve_once("200 OK", "", PHOTO, Duration::from_millis(250));
        let instagram = Instagram::new(None, Duration::from_millis(50))
            .await
            .unwrap();
        let start = std::time::Instant::now();
        let error = instagram
            .extract_endpoint(&endpoint, "BA")
            .await
            .unwrap_err()
            .to_string();
        assert!(start.elapsed() < Duration::from_secs(1));
        // The client and outer deadline can race; either failure is bounded.
        assert!(error.contains("timed out") || error.contains("request failed"));
        server.join().unwrap();
    }
}

#[cfg(test)]
#[path = "instagram_profile_tests.rs"]
mod profile_tests;

#[cfg(test)]
#[path = "instagram_stories_tests.rs"]
mod story_tests;

#[cfg(test)]
#[path = "instagram_collections_tests.rs"]
mod collection_tests;
