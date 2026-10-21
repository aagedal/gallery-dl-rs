//! Experimental native Pinterest pin, board and user-pin extraction.
//!
//! Mirrors gallery_dl/extractor/pinterest.py using the web "resource" API
//! (`resource/NAME/get/?source_url=..&data={"options":..,"context":{}}`) with
//! upstream's XHR header set. Pins use `PinResource`, boards `BoardResource`
//! then `BoardFeedResource`, and user pins `UserPinsResource`; feeds follow
//! `resource_response.bookmark` until `-end-`, capped at 200 pages. `pin.it`
//! short links are resolved through the `url_shortener` redirect. Media are
//! story-pin image/video blocks, else the best MP4 of `videos.video_list`
//! (HLS-only videos are skipped), else `images.orig`, HTTPS on `i.pinimg.com`
//! or `v*.pinimg.com` only. Promoted pins are skipped.
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
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::Arc,
    time::{Duration, SystemTime},
};
use url::Url;

const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:128.0) Gecko/20100101 Firefox/128.0";
/// Upstream's resource API headers. `X-APP-VERSION` is a pinned web build
/// hash copied from upstream and may need updating if Pinterest enforces it.
const API_HEADERS: [(&str, &str); 6] = [
    ("Accept", "application/json, text/javascript, */*, q=0.01"),
    ("X-Requested-With", "XMLHttpRequest"),
    ("X-APP-VERSION", "0c4af40"),
    ("X-Pinterest-AppState", "active"),
    ("Origin", "https://www.pinterest.com"),
    ("Referer", "https://www.pinterest.com/"),
];
const MAX_RESPONSE: usize = 16 * 1024 * 1024;
const MAX_PAGES: u64 = 200;
const PAGE_SIZE: u64 = 25;
/// Registrable Pinterest domains (with optional `www.` or a two-letter
/// country subdomain such as `br.pinterest.com`).
const DOMAINS: [&str; 30] = [
    "pinterest.com",
    "pinterest.co.uk",
    "pinterest.de",
    "pinterest.fr",
    "pinterest.ca",
    "pinterest.com.au",
    "pinterest.jp",
    "pinterest.es",
    "pinterest.it",
    "pinterest.at",
    "pinterest.ch",
    "pinterest.nz",
    "pinterest.ie",
    "pinterest.dk",
    "pinterest.se",
    "pinterest.pt",
    "pinterest.cl",
    "pinterest.com.mx",
    "pinterest.ph",
    "pinterest.co.kr",
    "pinterest.ru",
    "pinterest.nl",
    "pinterest.be",
    "pinterest.no",
    "pinterest.fi",
    "pinterest.pl",
    "pinterest.co.in",
    "pinterest.com.br",
    "pinterest.cz",
    "pinterest.ro",
];
/// First path segments that are site pages rather than users.
const RESERVED: [&str; 20] = [
    "pin",
    "search",
    "ideas",
    "today",
    "explore",
    "settings",
    "business",
    "categories",
    "topics",
    "resource",
    "url_shortener",
    "login",
    "signup",
    "about",
    "_",
    "news_hub",
    "videos",
    "shopping",
    "notifications",
    "password",
];

/// Pin IDs (numeric, or upstream's alphanumeric form).
pub fn valid_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

fn valid_name(name: &str) -> bool {
    (1..=100).contains(&name.len())
        && !name.starts_with('_')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'%'))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Pin { id: String },
    Board { user: String, board: String },
    User { user: String },
    Short { code: String },
}

fn plain(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https")
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
}

fn pinterest_host(host: &str) -> bool {
    DOMAINS.iter().any(|domain| {
        host == *domain
            || host.strip_suffix(domain).is_some_and(|prefix| {
                prefix
                    .strip_suffix('.')
                    .is_some_and(|label| label == "www" || is_country(label))
            })
    })
}

fn is_country(label: &str) -> bool {
    label.len() == 2 && label.bytes().all(|b| b.is_ascii_lowercase())
}

pub fn target(url: &Url) -> Result<Target> {
    let unsupported = || {
        error(
            "unsupported Pinterest URL; expected https://www.pinterest.com/pin/ID/, https://www.pinterest.com/USER/BOARD/, https://www.pinterest.com/USER/pins/ or https://pin.it/CODE",
        )
    };
    if !plain(url) {
        return Err(unsupported());
    }
    let host = url.host_str().ok_or_else(unsupported)?;
    let parts: Vec<&str> = url
        .path_segments()
        .map(|s| s.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    if host == "pin.it" {
        return match parts[..] {
            [code] if valid_id(code) => Ok(Target::Short { code: code.into() }),
            _ => Err(unsupported()),
        };
    }
    if !pinterest_host(host) {
        return Err(unsupported());
    }
    match parts[..] {
        ["pin", id] if valid_id(id) => Ok(Target::Pin { id: id.into() }),
        [user, "pins"] if valid_name(user) && !RESERVED.contains(&user) => {
            Ok(Target::User { user: user.into() })
        }
        [user, board] if valid_name(user) && valid_name(board) && !RESERVED.contains(&user) => {
            Ok(Target::Board {
                user: user.into(),
                board: board.into(),
            })
        }
        _ => Err(unsupported()),
    }
}

/// Whether a URL is a Pinterest page handled by this extractor.
pub fn is_pinterest_url(url: &Url) -> bool {
    target(url).is_ok()
}

/// HTTPS media on `i.pinimg.com` or `v*.pinimg.com`.
pub fn media_url(raw: &str) -> Option<Url> {
    let url = Url::parse(raw.trim()).ok()?;
    let label = url.host_str()?.strip_suffix(".pinimg.com")?;
    let ok = label == "i"
        || label
            .strip_prefix('v')
            .is_some_and(|rest| rest.bytes().all(|b| b.is_ascii_digit()));
    (url.scheme() == "https" && plain(&url) && ok).then_some(url)
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

/// Parse RFC 2822 dates such as `Mon, 01 Jan 2024 00:00:00 +0000`.
pub fn parse_rfc2822(input: &str) -> Option<i64> {
    let mut parts: Vec<&str> = input.split_whitespace().collect();
    if parts.first().is_some_and(|p| p.ends_with(',')) {
        parts.remove(0);
    }
    let [day, month, year, clock, offset] = parts[..] else {
        return None;
    };
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let month = MONTHS.iter().position(|m| *m == month)? + 1;
    let day: u8 = day.parse().ok()?;
    let offset = match offset {
        "GMT" | "UT" | "UTC" | "Z" => "+0000",
        other => other,
    };
    if year.len() != 4 || clock.len() != 8 {
        return None;
    }
    crate::date::parse_date(&format!("{year}-{month:02}-{day:02}T{clock}{offset}")).ok()
}

#[derive(Clone, Copy)]
enum Kind {
    Photo,
    Video,
}

struct Found {
    url: Url,
    kind: Kind,
    width: Option<u64>,
    height: Option<u64>,
}

fn image(images: &Value, key: &str) -> Option<Found> {
    let entry = images.get(key)?;
    Some(Found {
        url: media_url(&text(entry, "url")?)?,
        kind: Kind::Photo,
        width: number(entry, "width"),
        height: number(entry, "height"),
    })
}

/// Largest MP4 in a `video_list`; HLS (`.m3u8`) entries are ignored.
fn best_video(list: &Value) -> Option<Found> {
    list.as_object()?
        .values()
        .filter_map(|entry| {
            let url = media_url(&text(entry, "url")?)?;
            url.path().ends_with(".mp4").then(|| Found {
                url,
                kind: Kind::Video,
                width: number(entry, "width"),
                height: number(entry, "height"),
            })
        })
        .max_by_key(|f| f.width.unwrap_or(0) * f.height.unwrap_or(0))
}

fn story_media(pin: &Value) -> Vec<Found> {
    let Some(pages) = pin
        .pointer("/story_pin_data/pages")
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    pages
        .iter()
        .filter_map(|page| page.get("blocks").and_then(Value::as_array))
        .flatten()
        .filter_map(|block| {
            if let Some(list) = block.pointer("/video/video_list") {
                return best_video(list);
            }
            block
                .pointer("/image/images")
                .and_then(|images| image(images, "originals").or_else(|| image(images, "orig")))
        })
        .collect()
}

/// Convert one pin object into its media; promoted pins yield nothing.
pub fn pin_media(pin: &Value) -> Vec<Media> {
    let Some(id) = text(pin, "id").filter(|id| valid_id(id)) else {
        return Vec::new();
    };
    if pin.get("is_promoted").and_then(Value::as_bool) == Some(true)
        || pin.get("promoter").is_some_and(|p| !p.is_null())
        || text(pin, "type").is_some_and(|t| t != "pin")
    {
        return Vec::new();
    }
    let mut found = story_media(pin);
    if found.is_empty() {
        if let Some(video) = pin.pointer("/videos/video_list").and_then(best_video) {
            found.push(video);
        } else if let Some(img) = pin.get("images").and_then(|i| image(i, "orig")) {
            found.push(img);
        }
    }
    let username = pin
        .get("pinner")
        .and_then(|p| text(p, "username"))
        .map(Arc::<str>::from);
    let caption = ["title", "grid_title", "description"]
        .iter()
        .find_map(|key| text(pin, key))
        .map(Arc::<str>::from);
    let timestamp = text(pin, "created_at").and_then(|d| parse_rfc2822(&d));
    let story = found.len() > 1;
    found
        .into_iter()
        .enumerate()
        .map(|(i, f)| {
            let index = i + 1;
            Media {
                filename: format!("{id}_{index}.{}", extension(&f.url)),
                job: MediaJob::DirectHttp,
                category: "pinterest".into(),
                metadata: Some(PostMetadata {
                    file: Some(FileMetadata {
                        width: f.width,
                        height: f.height,
                        media_type: Some(Arc::from(match f.kind {
                            Kind::Photo => "photo",
                            Kind::Video => "video",
                        })),
                        ..FileMetadata::default()
                    }),
                    post_id: id.clone(),
                    media_id: if story {
                        format!("{id}_{index}")
                    } else {
                        id.clone()
                    },
                    username: username.clone(),
                    caption: caption.clone(),
                    timestamp,
                    index,
                    ..PostMetadata::default()
                }),
                url: f.url,
                headers: BTreeMap::new(),
                handoff: BTreeMap::new(),
            }
        })
        .collect()
}

/// Parse a resource API body into `resource_response.data` and its bookmark.
pub fn parse_resource(bytes: &[u8]) -> Result<(Value, Option<String>)> {
    let mut body: Value =
        serde_json::from_slice(bytes).map_err(|_| error("Pinterest returned invalid JSON"))?;
    let response = body
        .get_mut("resource_response")
        .ok_or_else(|| error("Pinterest response contains no resource_response"))?;
    let bookmark = text(response, "bookmark").filter(|b| b != "-end-" && b.len() <= 4096);
    let data = response.get_mut("data").map(Value::take);
    match data {
        Some(data) if !data.is_null() => Ok((data, bookmark)),
        _ => Err(error("Pinterest pin, board or user not found")),
    }
}

pub struct Pinterest {
    client: Client,
    network: NetworkOptions,
    api_root: Url,
    short_root: Url,
    timeout: Duration,
    request_timeout: Duration,
    retries: u8,
    rate_limits: RateLimits,
}

enum Feed {
    Board { id: String, source: String },
    User { user: String },
}

struct FeedState {
    feed: Option<Feed>,
    board: Option<(String, String)>,
    bookmark: Option<String>,
    page: u64,
    done: bool,
    pending: VecDeque<ExtractionEvent>,
}

impl Pinterest {
    pub fn new(timeout: Duration) -> Result<Self> {
        Ok(Self {
            client: NetworkOptions::default().client(USER_AGENT, timeout)?,
            network: NetworkOptions::default(),
            api_root: Url::parse("https://www.pinterest.com/").expect("static URL"),
            short_root: Url::parse("https://api.pinterest.com/").expect("static URL"),
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

    /// Override the site and short-link origins; intended for local tests.
    pub fn with_api_root(mut self, root: Url) -> Self {
        self.short_root = root.clone();
        self.api_root = root;
        self
    }

    async fn resource(
        &self,
        name: &str,
        source: &str,
        options: Value,
    ) -> Result<(Value, Option<String>)> {
        let mut endpoint = self
            .api_root
            .join(&format!("resource/{name}/get/"))
            .expect("static Pinterest path");
        endpoint
            .query_pairs_mut()
            .append_pair("source_url", source)
            .append_pair(
                "data",
                &json!({"options": options, "context": {}}).to_string(),
            );
        let body = tokio::time::timeout(self.timeout, self.request(&endpoint, false))
            .await
            .map_err(|_| error("native Pinterest extraction timed out"))??;
        parse_resource(&body)
    }

    async fn resolve_short(&self, code: &str) -> Result<String> {
        let endpoint = self
            .short_root
            .join(&format!("url_shortener/{code}/redirect/"))
            .expect("validated Pinterest code");
        let location = tokio::time::timeout(self.timeout, self.request(&endpoint, true))
            .await
            .map_err(|_| error("native Pinterest extraction timed out"))??;
        let unresolved = || error("Pinterest short link did not resolve to a pin");
        let location = String::from_utf8(location).map_err(|_| unresolved())?;
        let url = endpoint.join(&location).map_err(|_| unresolved())?;
        match target(&url) {
            Ok(Target::Pin { id }) => Ok(id),
            _ => Err(unresolved()),
        }
    }

    async fn pin(&self, id: &str) -> Result<VecDeque<ExtractionEvent>> {
        let (data, _) = self
            .resource(
                "PinResource",
                &format!("/pin/{id}/"),
                json!({"id": id, "field_set_key": "detailed"}),
            )
            .await?;
        if text(&data, "id").as_deref() != Some(id) {
            return Err(error("Pinterest pin not found"));
        }
        Ok(self.events_for(&data))
    }

    fn events_for(&self, pin: &Value) -> VecDeque<ExtractionEvent> {
        let mut items = pin_media(pin);
        if items.is_empty() {
            return VecDeque::new();
        }
        for item in &mut items {
            self.network.apply_media(item);
        }
        group(items, "pinterest")
    }

    async fn next_page(&self, state: &mut FeedState) -> Result<()> {
        if let Some((user, board)) = state.board.take() {
            let source = format!("/{user}/{board}/");
            let (data, _) = self
                .resource(
                    "BoardResource",
                    &source,
                    json!({"username": user, "slug": board, "field_set_key": "detailed"}),
                )
                .await?;
            let id = text(&data, "id")
                .filter(|id| valid_id(id))
                .ok_or_else(|| error("Pinterest board not found"))?;
            state.feed = Some(Feed::Board { id, source });
        }
        state.page += 1;
        let bookmarks = state.bookmark.take().map(|b| vec![b]);
        let (name, source, mut options) = match state.feed.as_ref().expect("feed") {
            Feed::Board { id, source } => (
                "BoardFeedResource",
                source.clone(),
                json!({"board_id": id, "board_url": source, "page_size": PAGE_SIZE,
                    "field_set_key": "react_grid_pin", "prepend": false}),
            ),
            Feed::User { user } => (
                "UserPinsResource",
                format!("/{user}/pins/"),
                json!({"username": user, "is_own_profile_pins": false,
                    "field_set_key": "grid_item", "pin_filter": null}),
            ),
        };
        if let Some(bookmarks) = bookmarks {
            options["bookmarks"] = json!(bookmarks);
        }
        let (data, bookmark) = self.resource(name, &source, options).await?;
        let pins = data
            .as_array()
            .ok_or_else(|| error("Pinterest response contains no pin list"))?;
        for pin in pins {
            state.pending.extend(self.events_for(pin));
        }
        state.done = pins.is_empty() || bookmark.is_none() || state.page >= MAX_PAGES;
        state.bookmark = bookmark;
        Ok(())
    }

    pub fn stream(&self, url: &Url) -> BoxStream<'_, Result<Media>> {
        crate::event::media_only(self.events(url))
    }

    /// Emit one post boundary per pin; feeds paginate lazily.
    pub fn events(&self, url: &Url) -> BoxStream<'_, Result<ExtractionEvent>> {
        let (feed, board) = match target(url) {
            Ok(Target::User { user }) => (Some(Feed::User { user }), None),
            Ok(Target::Board { user, board }) => (None, Some((user, board))),
            Ok(single) => {
                return stream::once(async move {
                    let id = match single {
                        Target::Short { code } => self.resolve_short(&code).await?,
                        Target::Pin { id } => id,
                        _ => unreachable!(),
                    };
                    self.pin(&id).await
                })
                .map(|result| match result {
                    Ok(events) => stream::iter(events.into_iter().map(Ok)).boxed(),
                    Err(e) => stream::once(async move { Err(e) }).boxed(),
                })
                .flatten()
                .boxed();
            }
            Err(e) => return stream::once(async move { Err(e) }).boxed(),
        };
        let state = FeedState {
            feed,
            board,
            bookmark: None,
            page: 0,
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

    async fn request(&self, endpoint: &Url, capture: bool) -> Result<Vec<u8>> {
        for attempt in 0..=self.retries {
            match self.attempt(endpoint, attempt, capture).await {
                Ok(body) => return Ok(body),
                Err(failure) if failure.retry && attempt < self.retries => {
                    tokio::time::sleep(backoff(attempt)).await;
                }
                Err(failure) => return Err(failure.error),
            }
        }
        unreachable!()
    }

    /// With `capture`, a redirect's Location is returned instead of followed.
    async fn attempt(
        &self,
        endpoint: &Url,
        attempt: u8,
        capture: bool,
    ) -> std::result::Result<Vec<u8>, Failure> {
        let mut target = endpoint.clone();
        let mut redirects = Redirects::new(endpoint);
        let mut response = loop {
            self.rate_limits.wait(&target, self.timeout).await?;
            let mut request = self
                .client
                .get(target.clone())
                .timeout(self.request_timeout);
            if !capture {
                for (name, value) in API_HEADERS {
                    request = request.header(name, value);
                }
            }
            let response = request.send().await.map_err(|_| {
                Failure::transient("Pinterest request failed; check network access")
            })?;
            if capture && response.status().is_redirection() {
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .filter(|v| v.len() <= 4096)
                    .ok_or_else(|| error("Pinterest short link did not resolve to a pin"))?;
                return Ok(location.as_bytes().to_vec());
            }
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
                            "Pinterest rate limit; retry after {} seconds",
                            delay.as_secs()
                        ),
                        None => "Pinterest rate limit; retry later".into(),
                    }
                } else {
                    format!("Pinterest returned HTTP {status}")
                },
            ));
        }
        if capture {
            return Err(error("Pinterest short link did not resolve to a pin").into());
        }
        if matches!(status.as_u16(), 404 | 410) {
            return Err(error("Pinterest pin, board or user not found").into());
        }
        if matches!(status.as_u16(), 401 | 403) {
            return Err(error("Pinterest refused access (private board or login required)").into());
        }
        if !status.is_success() {
            return Err(error(format!("Pinterest returned HTTP {status}")).into());
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| Failure::transient("could not read Pinterest response"))?
        {
            if body.len() + chunk.len() > MAX_RESPONSE {
                return Err(error("Pinterest response exceeded 16 MiB").into());
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

// PinterestExtractor.items reuses pin and applies each prepared media map.
// Include native file aliases without resetting unrelated post-level fields.
fn group(media: Vec<Media>, category: &str) -> VecDeque<ExtractionEvent> {
    group_with_keywords(
        media,
        category,
        KeywordPolicy::shared(&[
            "num",
            "index",
            "media_id",
            "filename",
            "name",
            "extension",
            "url",
            "_url",
            "_filesize",
            "width",
            "height",
            "duration",
            "size",
            "story_id",
            "page_id",
            "_fallback",
            "_ytdl_manifest",
            "file",
        ]),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread::{self, JoinHandle},
    };

    fn pin(id: &str) -> Value {
        json!({"id": id, "type": "pin", "title": "", "grid_title": "Cat",
            "created_at": "Mon, 01 Jan 2024 00:00:00 +0000",
            "pinner": {"username": "alice"},
            "images": {"orig": {"url": format!("https://i.pinimg.com/originals/aa/{id}.jpg"),
                "width": 800, "height": 600}}})
    }

    #[test]
    fn shared_keyword_policy_declares_source_file_assignments() {
        let events = group(pin_media(&pin("123")), "pinterest");
        let ExtractionEvent::Post(post) = &events[0] else {
            panic!("missing post");
        };
        let KeywordPolicy::Shared { file_fields, .. } = &post.keywords else {
            panic!("expected shared Pinterest keywords");
        };
        for field in [
            "num",
            "index",
            "media_id",
            "page_id",
            "story_id",
            "width",
            "height",
            "filename",
            "extension",
        ] {
            assert!(file_fields.iter().any(|f| f == field));
        }
        assert!(!file_fields.iter().any(|f| f == "description"));
    }

    #[test]
    fn parses_images_videos_and_story_pins() {
        let items = pin_media(&pin("123"));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].filename, "123_1.jpg");
        let meta = items[0].metadata.as_ref().unwrap();
        assert_eq!(
            (meta.post_id.as_str(), meta.media_id.as_str()),
            ("123", "123")
        );
        assert_eq!(meta.username.as_deref(), Some("alice"));
        assert_eq!(meta.caption.as_deref(), Some("Cat"));
        assert_eq!((meta.timestamp, meta.index), (Some(1704067200), 1));
        let file = meta.file.as_ref().unwrap();
        assert_eq!((file.width, file.height), (Some(800), Some(600)));
        assert!(items[0].archive_key().unwrap().starts_with("pinterest:"));

        let mut video = pin("124");
        video["videos"] = json!({"video_list": {
            "V_HLSV4": {"url": "https://v1.pinimg.com/videos/a.m3u8", "width": 1920, "height": 1080},
            "V_720P": {"url": "https://v1.pinimg.com/videos/720.mp4", "width": 720, "height": 1280},
            "V_EXP4": {"url": "https://v1.pinimg.com/videos/360.mp4", "width": 360, "height": 640}}});
        let items = pin_media(&video);
        assert_eq!(items.len(), 1);
        assert!(items[0].url.as_str().ends_with("720.mp4"));
        assert_eq!(items[0].filename, "124_1.mp4");

        let mut story = pin("125");
        story["story_pin_data"] = json!({"pages": [
            {"blocks": [{"image": {"images": {"originals": {"url": "https://i.pinimg.com/originals/s1.png"}}}}]},
            {"blocks": [{"video": {"video_list": {"V_EXP7": {"url": "https://v2.pinimg.com/s2.mp4"}}}},
                {"image": {"images": {"originals": {"url": "http://i.pinimg.com/insecure.jpg"}}}},
                {"text": "hello"}]}]});
        let items = pin_media(&story);
        assert_eq!(items.len(), 2);
        assert_eq!(items[1].filename, "125_2.mp4");
        assert_eq!(items[1].metadata.as_ref().unwrap().media_id, "125_2");
        assert_ne!(items[0].archive_key(), items[1].archive_key());

        let mut promoted = pin("126");
        promoted["is_promoted"] = json!(true);
        assert!(pin_media(&promoted).is_empty());
        let mut hls = pin("127");
        hls["images"] = json!({"orig": {"url": "https://evil.example/a.jpg"}});
        assert!(pin_media(&hls).is_empty());
        assert!(parse_resource(b"<html>").is_err());
        assert!(parse_resource(br#"{"resource_response":{"data":null}}"#).is_err());
        let (_, bookmark) =
            parse_resource(br#"{"resource_response":{"data":[],"bookmark":"-end-"}}"#).unwrap();
        assert_eq!(bookmark, None);
    }

    #[test]
    fn classifies_supported_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        let p = Target::Pin { id: "123".into() };
        assert_eq!(t("https://www.pinterest.com/pin/123/").unwrap(), p);
        assert_eq!(t("https://www.pinterest.co.uk/pin/123").unwrap(), p);
        assert_eq!(t("https://br.pinterest.com/pin/123/").unwrap(), p);
        assert_eq!(t("https://pinterest.com.au/pin/123/").unwrap(), p);
        assert_eq!(
            t("https://www.pinterest.de/alice/cats/").unwrap(),
            Target::Board {
                user: "alice".into(),
                board: "cats".into()
            }
        );
        assert_eq!(
            t("https://www.pinterest.com/alice/pins/").unwrap(),
            Target::User {
                user: "alice".into()
            }
        );
        assert_eq!(
            t("https://pin.it/AbC12").unwrap(),
            Target::Short {
                code: "AbC12".into()
            }
        );
        for raw in [
            "https://www.pinterest.com/",
            "https://www.pinterest.com/alice/",
            "https://www.pinterest.com/search/pins/",
            "https://www.pinterest.com/alice/_saved/",
            "https://i.pinimg.com/originals/aa/a.jpg",
            "https://www.pinterest.com.evil.example/pin/1/",
            "https://evilpinterest.com/pin/1/",
            "https://www.pinterest.com:444/pin/1/",
        ] {
            assert!(t(raw).is_err(), "{raw}");
        }
        assert!(media_url("https://v.pinimg.com/a.mp4").is_some());
        assert!(media_url("https://s.pinimg.com/a.jpg").is_none());
        assert!(media_url("http://i.pinimg.com/a.jpg").is_none());
        assert_eq!(
            parse_rfc2822("Tue, 02 Jan 2024 01:00:00 +0100"),
            Some(1704153600)
        );
        assert_eq!(parse_rfc2822("garbage"), None);
    }

    fn response(status: &str, extra: &str, body: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\nRetry-After: 0\r\n{extra}\r\n",
            body.len()
        )
        .into_bytes();
        out.extend_from_slice(body);
        out
    }

    fn ok(data: Value, bookmark: &str) -> Vec<u8> {
        let body = json!({"resource_response": {"status": "success", "data": data,
            "bookmark": bookmark}});
        response("200 OK", "", body.to_string().as_bytes())
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

    fn pinterest(root: Url) -> Pinterest {
        Pinterest::new(Duration::from_secs(5))
            .unwrap()
            .with_network(&NetworkOptions::new(Some(String::new()), None).unwrap())
            .unwrap()
            .with_api_root(root)
    }

    fn decoded(request: &str) -> String {
        let line = request.lines().next().unwrap();
        let url = Url::parse(&format!("http://x{}", line.split(' ').nth(1).unwrap())).unwrap();
        url.query_pairs()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&")
    }

    #[tokio::test]
    async fn fetches_pin_via_short_link_with_retry_and_headers() {
        let (root, server) = serve(vec![
            response(
                "302 Found",
                "Location: https://www.pinterest.com/pin/123/?utm=x\r\n",
                b"",
            ),
            response("503 Service Unavailable", "", b""),
            ok(pin("123"), "-end-"),
        ]);
        let media: Vec<_> = pinterest(root)
            .with_retries(1)
            .stream(&Url::parse("https://pin.it/AbC").unwrap())
            .collect()
            .await;
        assert_eq!(media.len(), 1);
        assert_eq!(media[0].as_ref().unwrap().filename, "123_1.jpg");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /url_shortener/AbC/redirect/ "));
        assert!(requests[2].starts_with("GET /resource/PinResource/get/?"));
        let query = decoded(&requests[2]);
        assert!(query.contains("source_url=/pin/123/"), "{query}");
        assert!(query.contains(r#""field_set_key":"detailed""#), "{query}");
        let lower = requests[2].to_ascii_lowercase();
        assert!(lower.contains("x-pinterest-appstate: active"));
        assert!(lower.contains("x-requested-with: xmlhttprequest"));
    }

    #[tokio::test]
    async fn paginates_board_with_bookmarks_and_hides_urls_in_errors() {
        let (root, server) = serve(vec![
            ok(json!({"id": "900", "name": "cats"}), ""),
            ok(json!([pin("1"), {"id": "2", "is_promoted": true}]), "BM1"),
            ok(json!([pin("3")]), "-end-"),
        ]);
        let media: Vec<_> = pinterest(root)
            .stream(&Url::parse("https://www.pinterest.com/alice/cats/").unwrap())
            .collect()
            .await;
        let media: Vec<_> = media.into_iter().map(|m| m.unwrap()).collect();
        assert_eq!(media.len(), 2);
        assert_eq!(media[1].filename, "3_1.jpg");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /resource/BoardResource/get/?"));
        assert!(decoded(&requests[0]).contains(r#""slug":"cats""#));
        assert!(requests[1].starts_with("GET /resource/BoardFeedResource/get/?"));
        assert!(!decoded(&requests[1]).contains("bookmarks"));
        let second = decoded(&requests[2]);
        assert!(second.contains(r#""bookmarks":["BM1"]"#), "{second}");
        assert!(second.contains(r#""board_id":"900""#), "{second}");

        let (root, server) = serve(vec![response("404 Not Found", "", b"")]);
        let error = pinterest(root)
            .stream(&Url::parse("https://www.pinterest.com/alice/pins/").unwrap())
            .collect::<Vec<_>>()
            .await
            .remove(0)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("not found") && !error.contains("127.0.0.1"),
            "{error}"
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /resource/UserPinsResource/get/?"));
    }
}
