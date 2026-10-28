//! Experimental native DeviantArt deviation, gallery and folder extraction.
//!
//! Mirrors gallery_dl/extractor/deviantart.py using the OAuth2 API. Requests
//! use an access token from the client-credentials grant (upstream's public
//! client unless `client-id`/`client-secret` are configured); the token is
//! cached for the run and refreshed once on HTTP 401. Single deviations are
//! resolved from the deviation page's `DeviantArt://deviation/UUID` app link.
//! Files are `content.src`, or the original from `deviation/download/UUID` for
//! downloadable deviations. Deviations without `content` (literature, journals,
//! statuses) are skipped. Media URLs carry signed `token` queries and are never
//! included in errors.
use crate::{
    Result, error,
    event::{ExtractionEvent, group},
    extract::{Media, MediaJob, PostMetadata},
    file_metadata::FileMetadata,
    network::NetworkOptions,
    transport::{RateLimits, retry_after},
};
use futures_util::{
    StreamExt,
    stream::{self, BoxStream},
};
use reqwest::{Client, RequestBuilder, StatusCode};
use serde_json::Value;
use std::{
    collections::{BTreeMap, VecDeque},
    time::{Duration, SystemTime},
};
use url::Url;

/// Upstream gallery-dl's public OAuth client.
pub(crate) const CLIENT_ID: &str = "5388";
pub(crate) const CLIENT_SECRET: &str = "76b08c69cfb27f26d6161f9ab6d061a1";
const USER_AGENT: &str = "gallery-dl-rs";
const MAX_RESPONSE: usize = 16 * 1024 * 1024;
const MAX_PAGES: usize = 200;
const PAGE_SIZE: &str = "24";

/// Canonical 8-4-4-4-12 hexadecimal UUID.
pub fn valid_uuid(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

fn valid_user(user: &str) -> bool {
    (1..=32).contains(&user.len())
        && user.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !user.starts_with('-')
}

/// Top-level `www.deviantart.com` paths that are not user names.
const RESERVED: &[&str] = &[
    "about",
    "art",
    "browse",
    "core-membership",
    "daily-deviations",
    "developers",
    "deviation",
    "groups",
    "join",
    "notifications",
    "popular",
    "search",
    "settings",
    "shop",
    "status",
    "tag",
    "team",
    "topic",
    "users",
    "watch",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// Deviation page path `USER/art/SLUG-ID`.
    Deviation {
        user: String,
        slug: String,
    },
    Gallery(String),
    Folder {
        user: String,
        id: String,
        name: Option<String>,
    },
    /// A bare deviation ID (`/view/ID`, `fav.me/dB36`, `view.php?id=`).
    DeviationId(String),
    /// `USER/favourites[/all]`, or one collection folder.
    Favorites {
        user: String,
        folder: Option<(String, Option<String>)>,
    },
    Tag(String),
    Avatar(String),
    Banner(String),
    /// `USER/watching`: watched users, queued as user children.
    Following(String),
}

/// Upstream `util.b36decode`.
fn base36(text: &str) -> Option<u64> {
    (!text.is_empty() && text.len() <= 12)
        .then(|| u64::from_str_radix(text, 36).ok())
        .flatten()
}

/// Whether a URL is a DeviantArt page handled by this extractor.
pub fn is_deviantart_url(url: &Url) -> bool {
    target(url).is_ok()
}

pub fn target(url: &Url) -> Result<Target> {
    let unsupported = || {
        error(
            "unsupported DeviantArt URL; expected https://www.deviantart.com/USER[/gallery[/FOLDER/NAME]] or https://www.deviantart.com/USER/art/SLUG-ID",
        )
    };
    let host = url.host_str().ok_or_else(unsupported)?;
    if !matches!(url.scheme(), "http" | "https")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(unsupported());
    }
    let segments: Vec<&str> = url
        .path_segments()
        .map(|s| s.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    // Upstream also accepts the fxdeviantart.com embed mirror.
    let host = host
        .strip_suffix("fxdeviantart.com")
        .map(|prefix| format!("{prefix}deviantart.com"))
        .unwrap_or_else(|| host.to_owned());
    let host = host.as_str();
    if host == "fav.me" {
        return match segments.as_slice() {
            [code] => code
                .strip_prefix('d')
                .and_then(base36)
                .map(|id| Target::DeviationId(id.to_string()))
                .ok_or_else(unsupported),
            _ => Err(unsupported()),
        };
    }
    if matches!(host, "www.deviantart.com" | "deviantart.com") {
        let digits =
            |id: &str| (1..=20).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_digit());
        match segments.as_slice() {
            ["view" | "deviation", id] if digits(id) => {
                return Ok(Target::DeviationId((*id).into()));
            }
            ["view.php" | "view-full.php"] => {
                return url
                    .query_pairs()
                    .find(|(k, _)| k == "id")
                    .map(|(_, v)| v.into_owned())
                    .filter(|id| digits(id))
                    .map(Target::DeviationId)
                    .ok_or_else(unsupported);
            }
            ["tag", tag] => return Ok(Target::Tag(crate::sites::percent_decode(tag))),
            _ => {}
        }
    }
    let (user, rest) = match host {
        "www.deviantart.com" | "deviantart.com" => match segments.split_first() {
            Some((user, rest)) if !RESERVED.contains(&user.to_ascii_lowercase().as_str()) => {
                (user.to_string(), rest)
            }
            _ => return Err(unsupported()),
        },
        _ => match host.strip_suffix(".deviantart.com") {
            // Legacy USER.deviantart.com hosts.
            Some(user) if !user.contains('.') && user != "www" => {
                (user.to_owned(), segments.as_slice())
            }
            _ => return Err(unsupported()),
        },
    };
    if !valid_user(&user) {
        return Err(unsupported());
    }
    match rest {
        ["avatar", ..] => return Ok(Target::Avatar(user)),
        ["banner" | "background", ..] => return Ok(Target::Banner(user)),
        ["watching", ..] => return Ok(Target::Following(user)),
        ["about"] if url.fragment() == Some("watching") => return Ok(Target::Following(user)),
        // `?catpath=/` and `/all` select every favourite, like the bare page.
        ["favourites"] | ["favourites", "all"] => {
            return Ok(Target::Favorites { user, folder: None });
        }
        ["favourites", id, name, ..] => {
            return Ok(Target::Favorites {
                folder: Some((
                    (*id).to_owned(),
                    Some(crate::sites::percent_decode(name).to_ascii_lowercase()),
                )),
                user,
            });
        }
        _ => {}
    }
    match rest {
        [] | ["gallery"] | ["gallery", "all"] => Ok(Target::Gallery(user)),
        ["art", slug] if numeric_id(slug).is_some() => Ok(Target::Deviation {
            user,
            slug: slug.to_string(),
        }),
        ["gallery", id] | ["gallery", id, _]
            if valid_uuid(id) || (id.len() <= 20 && id.bytes().all(|b| b.is_ascii_digit())) =>
        {
            let name = rest.get(2).map(|n| n.to_ascii_lowercase());
            if !valid_uuid(id) && name.is_none() {
                return Err(unsupported());
            }
            Ok(Target::Folder {
                user,
                id: id.to_string(),
                name,
            })
        }
        _ => Err(unsupported()),
    }
}

/// Trailing numeric deviation ID of an `art/SLUG-ID` segment.
fn numeric_id(slug: &str) -> Option<&str> {
    let id = slug.rsplit('-').next()?;
    ((1..=20).contains(&id.len())
        && id.bytes().all(|b| b.is_ascii_digit())
        && slug.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
    .then_some(id)
}

/// Lowercase, non-alphanumeric runs collapsed to `-`, like DeviantArt URLs.
fn slugify(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_owned()
}

/// Find the deviation UUID in a deviation page's app link metadata.
pub fn page_uuid(html: &str) -> Option<String> {
    const MARKERS: &[&str] = &["DeviantArt://deviation/", "\"deviationUuid\":\""];
    MARKERS.iter().find_map(|marker| {
        html.match_indices(marker).find_map(|(at, _)| {
            let id = html.get(at + marker.len()..at + marker.len() + 36)?;
            valid_uuid(id).then(|| id.to_owned())
        })
    })
}

fn string<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn host_allowed(host: &str) -> bool {
    host == "wixmp.com"
        || host.ends_with(".wixmp.com")
        || host == "deviantart.net"
        || host.ends_with(".deviantart.net")
}

/// HTTPS files on `*.wixmp.com` or `*.deviantart.net` only.
fn media_url(raw: &str) -> Option<Url> {
    let url = Url::parse(raw.trim()).ok()?;
    (url.scheme() == "https"
        && url.host_str().is_some_and(host_allowed)
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none())
    .then_some(url)
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

fn dimension(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(Value::as_u64).filter(|v| *v > 0)
}

/// Whether a deviation object should be fetched through `deviation/download`.
pub fn wants_download(deviation: &Value) -> bool {
    deviation.get("is_downloadable").and_then(Value::as_bool) == Some(true)
        && deviation.get("content").is_some()
}

/// Parse one deviation into its file, optionally using a `deviation/download`
/// response for the original. `None` for deviations without image content.
pub fn parse_deviation(deviation: &Value, download: Option<&Value>) -> Option<Media> {
    let uuid = string(deviation, "deviationid").filter(|id| valid_uuid(id))?;
    let content = deviation.get("content")?;
    let preview = string(content, "src").and_then(media_url)?;
    let original = download.and_then(|d| Some((d, string(d, "src").and_then(media_url)?)));
    let (source, url) = original.unwrap_or((content, preview));
    let timestamp = match deviation.get("published_time") {
        Some(Value::String(s)) => s.trim().parse::<i64>().ok(),
        Some(Value::Number(n)) => n.as_i64(),
        _ => None,
    };
    let ext = extension(&url);
    Some(Media {
        filename: format!("{uuid}.{ext}"),
        job: MediaJob::DirectHttp,
        category: "deviantart".into(),
        metadata: Some(PostMetadata {
            file: Some(FileMetadata {
                width: dimension(source, "width"),
                height: dimension(source, "height"),
                ..FileMetadata::default()
            }),
            post_id: uuid.to_owned(),
            media_id: uuid.to_owned(),
            username: deviation
                .get("author")
                .and_then(|a| string(a, "username"))
                .map(Into::into),
            caption: string(deviation, "title").map(Into::into),
            timestamp,
            index: 1,
            ..PostMetadata::default()
        }),
        url,
        headers: BTreeMap::new(),
        handoff: BTreeMap::new(),
    })
}

fn json(bytes: &[u8]) -> Result<Value> {
    serde_json::from_slice(bytes).map_err(|_| error("DeviantArt returned invalid JSON"))
}

/// One listing page: deviation objects and the next offset.
pub struct Page {
    pub results: Vec<Value>,
    pub next: Option<u64>,
}

pub fn parse_page(body: &Value) -> Result<Page> {
    let results = body
        .get("results")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| error("DeviantArt response contains no results"))?;
    let next = (body.get("has_more").and_then(Value::as_bool) == Some(true))
        .then(|| body.get("next_offset").and_then(Value::as_u64))
        .flatten();
    Ok(Page { results, next })
}

pub struct DeviantArt {
    client: Client,
    network: NetworkOptions,
    root: Url,
    client_id: String,
    client_secret: String,
    access_token: tokio::sync::Mutex<Option<String>>,
    timeout: Duration,
    request_timeout: Duration,
    retries: u8,
    rate_limits: RateLimits,
}

struct ListingState {
    user: String,
    /// The API listing: `gallery`, `collections` or `browse/tags`.
    kind: &'static str,
    folder: Option<String>,
    offset: Option<u64>,
    pages: usize,
    pending: VecDeque<ExtractionEvent>,
}

impl DeviantArt {
    pub fn new(timeout: Duration) -> Result<Self> {
        Ok(Self {
            client: NetworkOptions::default().client(USER_AGENT, timeout)?,
            network: NetworkOptions::default(),
            root: Url::parse("https://www.deviantart.com/").expect("static URL"),
            client_id: CLIENT_ID.into(),
            client_secret: CLIENT_SECRET.into(),
            access_token: tokio::sync::Mutex::new(None),
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

    /// Override upstream's public OAuth client.
    pub fn with_credentials(mut self, id: Option<&str>, secret: Option<&str>) -> Self {
        if let Some(id) = id {
            self.client_id = id.into();
        }
        if let Some(secret) = secret {
            self.client_secret = secret.into();
        }
        self
    }

    pub fn with_retries(mut self, retries: u8) -> Self {
        self.retries = retries;
        self
    }

    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// Override the site, OAuth and API origin; intended for local tests.
    pub fn with_root(mut self, root: Url) -> Self {
        self.root = root;
        self
    }

    fn api(&self, path: &str) -> Url {
        let mut url = self
            .root
            .join("api/v1/oauth2/")
            .and_then(|base| base.join(path))
            .expect("static path");
        url.query_pairs_mut().append_pair("mature_content", "true");
        url
    }

    async fn authenticate(&self) -> Result<String> {
        let endpoint = self.root.join("oauth2/token").expect("static path");
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", "client_credentials")
            .append_pair("client_id", &self.client_id)
            .append_pair("client_secret", &self.client_secret)
            .finish();
        let reply = self
            .request(&endpoint, || {
                self.client
                    .post(endpoint.clone())
                    .timeout(self.request_timeout)
                    .header("Content-Type", "application/x-www-form-urlencoded")
                    .body(body.clone())
            })
            .await?;
        match reply {
            Reply::Ok(body) => json(&body)?
                .get("access_token")
                .and_then(Value::as_str)
                .filter(|token| !token.is_empty() && token.bytes().all(|b| b.is_ascii_graphic()))
                .map(str::to_owned)
                .ok_or_else(|| error("DeviantArt OAuth response contains no access token")),
            Reply::Status(status) if status.is_client_error() => Err(error(
                "DeviantArt rejected the OAuth client credentials; check client-id and client-secret",
            )),
            Reply::Status(status) => Err(error(format!("DeviantArt OAuth returned HTTP {status}"))),
        }
    }

    async fn access_token(&self, stale: Option<&str>) -> Result<String> {
        let mut cached = self.access_token.lock().await;
        if let Some(token) = cached.as_deref()
            && Some(token) != stale
        {
            return Ok(token.to_owned());
        }
        let token = self.authenticate().await?;
        *cached = Some(token.clone());
        Ok(token)
    }

    /// Authorized API GET, refreshing the access token once on HTTP 401.
    async fn get(&self, endpoint: &Url) -> Result<Value> {
        tokio::time::timeout(self.timeout, async {
            let mut token = self.access_token(None).await?;
            for refreshed in [false, true] {
                let reply = self
                    .request(endpoint, || {
                        self.client
                            .get(endpoint.clone())
                            .timeout(self.request_timeout)
                            .bearer_auth(&token)
                    })
                    .await?;
                match reply {
                    Reply::Ok(body) => return json(&body),
                    Reply::Status(StatusCode::UNAUTHORIZED) if !refreshed => {
                        token = self.access_token(Some(&token)).await?;
                    }
                    Reply::Status(status) => return Err(status_error(status)),
                }
            }
            Err(error("DeviantArt refused the refreshed access token"))
        })
        .await
        .map_err(|_| error("native DeviantArt extraction timed out"))?
    }

    /// Resolve a deviation page to its UUID.
    async fn page_uuid(&self, user: &str, slug: &str) -> Result<String> {
        let mut page = self.root.clone();
        page.path_segments_mut()
            .expect("base URL")
            .clear()
            .extend([user, "art", slug]);
        let reply = tokio::time::timeout(
            self.timeout,
            self.request(&page, || {
                self.client.get(page.clone()).timeout(self.request_timeout)
            }),
        )
        .await
        .map_err(|_| error("native DeviantArt extraction timed out"))??;
        match reply {
            Reply::Ok(body) => page_uuid(&String::from_utf8_lossy(&body))
                .ok_or_else(|| error("DeviantArt deviation page contains no deviation UUID")),
            Reply::Status(status) => Err(status_error(status)),
        }
    }

    /// Fetch the original for downloadable deviations, falling back to `content`.
    async fn media(&self, deviation: &Value) -> Option<Media> {
        let mut download = None;
        if wants_download(deviation)
            && let Some(uuid) = string(deviation, "deviationid").filter(|id| valid_uuid(id))
        {
            download = self
                .get(&self.api(&format!("deviation/download/{uuid}")))
                .await
                .ok();
        }
        let mut media = parse_deviation(deviation, download.as_ref())?;
        self.network.apply_media(&mut media);
        Some(media)
    }

    async fn folder_uuid(
        &self,
        kind: &str,
        user: &str,
        id: &str,
        name: Option<&str>,
    ) -> Result<String> {
        if valid_uuid(id) {
            return Ok(id.to_owned());
        }
        let wanted = name.map(slugify).unwrap_or_default();
        let mut offset = 0;
        for _ in 0..MAX_PAGES {
            let mut endpoint = self.api(&format!("{kind}/folders"));
            endpoint
                .query_pairs_mut()
                .append_pair("username", user)
                .append_pair("offset", &offset.to_string())
                .append_pair("limit", "50");
            let page = parse_page(&self.get(&endpoint).await?)?;
            for folder in &page.results {
                if let Some(uuid) = string(folder, "folderid").filter(|id| valid_uuid(id))
                    && string(folder, "name").map(slugify).as_deref() == Some(wanted.as_str())
                {
                    return Ok(uuid.to_owned());
                }
            }
            match page.next {
                Some(next) if next > offset => offset = next,
                _ => break,
            }
        }
        Err(error("DeviantArt gallery folder not found"))
    }

    async fn next_page(&self, state: &mut ListingState) -> Result<()> {
        let offset = state.offset.take().unwrap_or(0);
        let path = match (&state.folder, state.kind) {
            (_, "browse/tags") => "browse/tags".to_owned(),
            (Some(folder), kind) => format!("{kind}/{folder}"),
            (None, kind) => format!("{kind}/all"),
        };
        let mut endpoint = self.api(&path);
        {
            let mut query = endpoint.query_pairs_mut();
            if state.kind == "browse/tags" {
                query.append_pair("tag", &state.user);
            } else {
                query.append_pair("username", &state.user);
            }
            query
                .append_pair("offset", &offset.to_string())
                // Collections allow at most 24 results per page.
                .append_pair(
                    "limit",
                    if state.kind == "collections" {
                        "24"
                    } else {
                        PAGE_SIZE
                    },
                );
        }
        let page = parse_page(&self.get(&endpoint).await?)?;
        state.pages += 1;
        if state.pages < MAX_PAGES {
            state.offset = page.next.filter(|next| *next > offset);
        }
        for deviation in &page.results {
            if let Some(media) = self.media(deviation).await {
                state.pending.extend(group(vec![media], "deviantart"));
            }
        }
        Ok(())
    }

    pub fn stream(&self, url: &Url) -> BoxStream<'_, Result<Media>> {
        crate::event::media_only(self.events(url))
    }

    /// `user/profile/NAME`.
    async fn profile(&self, user: &str) -> Result<Value> {
        self.get(&self.api(&format!("user/profile/{user}"))).await
    }

    fn once_events<'a, F>(&'a self, future: F) -> BoxStream<'a, Result<ExtractionEvent>>
    where
        F: std::future::Future<Output = Result<Vec<Media>>> + Send + 'a,
    {
        stream::once(future)
            .map(|result| match result {
                Ok(media) => stream::iter(
                    media
                        .into_iter()
                        .flat_map(|media| group(vec![media], "deviantart"))
                        .map(Ok),
                )
                .boxed(),
                Err(e) => stream::once(async move { Err(e) }).boxed(),
            })
            .flatten()
            .boxed()
    }

    /// Upstream `DeviantartAvatarExtractor`: the big avatar unless it is
    /// DeviantArt's default.
    async fn avatar(&self, user: &str) -> Result<Vec<Media>> {
        let profile = self.profile(user).await?;
        let author = profile.get("user").cloned().unwrap_or(Value::Null);
        let Some(icon) = string(&author, "usericon") else {
            return Ok(Vec::new());
        };
        if icon == "https://a.deviantart.net/avatars/default.gif" {
            return Ok(Vec::new());
        }
        let index = icon.rsplit_once('?').map_or("0", |(_, index)| index);
        let url = icon.replacen("/avatars/", "/avatars-big/", 1);
        let url = Url::parse(&url).map_err(|_| error("DeviantArt returned an invalid avatar"))?;
        let ext = extension(&url);
        let id = format!("a_{}_{index}", user.to_ascii_lowercase());
        let mut media = Media {
            filename: format!("{id}.{ext}"),
            job: MediaJob::DirectHttp,
            category: "deviantart".into(),
            metadata: Some(PostMetadata {
                post_id: id.clone(),
                media_id: id,
                username: string(&author, "username").map(Into::into),
                caption: Some("avatar".into()),
                index: 1,
                ..PostMetadata::default()
            }),
            url,
            headers: BTreeMap::new(),
            handoff: BTreeMap::new(),
        };
        self.network.apply_media(&mut media);
        Ok(vec![media])
    }

    /// Upstream `DeviantartBackgroundExtractor`: the profile's cover deviation.
    async fn banner(&self, user: &str) -> Result<Vec<Media>> {
        let profile = self.profile(user).await?;
        let Some(cover) = profile.pointer("/cover_deviation/cover_deviation") else {
            return Ok(Vec::new());
        };
        Ok(self.media(cover).await.into_iter().collect())
    }

    /// Upstream `DeviantartDeviationExtractor` for bare IDs: the page at
    /// `/view/ID/` names the deviation's UUID.
    async fn deviation_by_id(&self, id: &str) -> Result<Vec<Media>> {
        // `/view/ID/` redirects to the deviation's canonical page; follow
        // redirects that stay on DeviantArt.
        let mut page = self
            .root
            .join(&format!("view/{id}/"))
            .map_err(|_| error("invalid DeviantArt deviation"))?;
        let mut body = None;
        for _ in 0..5 {
            let response = tokio::time::timeout(
                self.timeout,
                self.client
                    .get(page.clone())
                    .timeout(self.request_timeout)
                    .send(),
            )
            .await
            .map_err(|_| error("native DeviantArt extraction timed out"))?
            .map_err(|_| error("DeviantArt request failed; check network access"))?;
            let status = response.status();
            if status.is_redirection() {
                let next = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| page.join(v).ok())
                    .filter(|next| {
                        next.scheme() == "https"
                            && next.host_str().is_some_and(|h| {
                                h == "deviantart.com" || h.ends_with(".deviantart.com")
                            })
                    })
                    .ok_or_else(|| error("DeviantArt responded with an unexpected redirect"))?;
                page = next;
                continue;
            }
            if !status.is_success() {
                return Err(status_error(status));
            }
            body = Some(
                response
                    .bytes()
                    .await
                    .map_err(|_| error("could not read DeviantArt response"))?,
            );
            break;
        }
        let body = body.ok_or_else(|| error("DeviantArt redirected too often"))?;
        let uuid = page_uuid(&String::from_utf8_lossy(&body))
            .ok_or_else(|| error("DeviantArt deviation page contains no deviation UUID"))?;
        let deviation = self.get(&self.api(&format!("deviation/{uuid}"))).await?;
        let media = self.media(&deviation).await.ok_or_else(|| {
            error(
                "DeviantArt deviation has no downloadable image content (literature and journals are not supported)",
            )
        })?;
        Ok(vec![media])
    }

    /// Upstream `DeviantartFollowingExtractor`: queue every watched user.
    fn following(&self, user: String) -> BoxStream<'_, Result<ExtractionEvent>> {
        stream::try_unfold((Some(0u64), 0usize), move |(offset, pages)| {
            let user = user.clone();
            async move {
                let Some(offset) = offset.filter(|_| pages < MAX_PAGES) else {
                    return Ok(None);
                };
                let mut endpoint = self.api(&format!("user/friends/{user}"));
                endpoint
                    .query_pairs_mut()
                    .append_pair("offset", &offset.to_string())
                    .append_pair("limit", "50");
                let page = parse_page(&self.get(&endpoint).await?)?;
                let children: Vec<_> = page
                    .results
                    .iter()
                    .filter_map(|friend| {
                        let name = friend.get("user").and_then(|u| string(u, "username"))?;
                        valid_user(name).then(|| name.to_owned())
                    })
                    .map(|name| {
                        crate::event::child_job(
                            crate::event::ChildMetadata::from_raw(
                                "deviantart",
                                "following",
                                &serde_json::Map::new(),
                            ),
                            move || {
                                let url = Url::parse(&format!("https://www.deviantart.com/{name}"))
                                    .expect("validated user");
                                self.events(&url)
                            },
                        )
                    })
                    .collect();
                let next = page.next.filter(|next| *next > offset);
                Ok::<_, crate::Error>(Some((stream::iter(children).flatten(), (next, pages + 1))))
            }
        })
        .map(|result| match result {
            Ok(events) => events.boxed(),
            Err(e) => stream::once(async move { Err(e) }).boxed(),
        })
        .flatten()
        .boxed()
    }

    /// Emit one post boundary per deviation; galleries paginate lazily.
    pub fn events(&self, url: &Url) -> BoxStream<'_, Result<ExtractionEvent>> {
        let target = match target(url) {
            Ok(target) => target,
            Err(e) => return stream::once(async move { Err(e) }).boxed(),
        };
        let (kind, user, folder) = match target {
            Target::DeviationId(id) => {
                return self.once_events(async move { self.deviation_by_id(&id).await });
            }
            Target::Avatar(user) => {
                return self.once_events(async move { self.avatar(&user).await });
            }
            Target::Banner(user) => {
                return self.once_events(async move { self.banner(&user).await });
            }
            Target::Following(user) => return self.following(user),
            Target::Tag(tag) => ("browse/tags", tag, None),
            Target::Favorites { user, folder } => ("collections", user, folder),
            Target::Gallery(user) => ("gallery", user, None),
            Target::Folder { user, id, name } => ("gallery", user, Some((id, name))),
            Target::Deviation { user, slug } => {
                return stream::once(async move {
                    let uuid = self.page_uuid(&user, &slug).await?;
                    let deviation = self.get(&self.api(&format!("deviation/{uuid}"))).await?;
                    let media = self.media(&deviation).await.ok_or_else(|| {
                        error(
                            "DeviantArt deviation has no downloadable image content (literature and journals are not supported)",
                        )
                    })?;
                    Ok::<_, crate::Error>(stream::iter(
                        group(vec![media], "deviantart").into_iter().map(Ok),
                    ))
                })
                .map(|result| match result {
                    Ok(events) => events.boxed(),
                    Err(e) => stream::once(async move { Err(e) }).boxed(),
                })
                .flatten()
                .boxed();
            }
        };
        let state = ListingState {
            kind,
            user,
            folder: None,
            offset: None,
            pages: 0,
            pending: VecDeque::new(),
        };
        stream::unfold(Some((state, folder, true)), move |state| async move {
            let (mut state, folder, mut first) = state?;
            if let Some((id, name)) = &folder
                && state.folder.is_none()
            {
                match self
                    .folder_uuid(state.kind, &state.user, id, name.as_deref())
                    .await
                {
                    Ok(uuid) => state.folder = Some(uuid),
                    Err(e) => return Some((Err(e), None)),
                }
            }
            loop {
                if let Some(event) = state.pending.pop_front() {
                    return Some((Ok(event), Some((state, folder, first))));
                }
                if !first && state.offset.is_none() {
                    return None;
                }
                first = false;
                if let Err(e) = self.next_page(&mut state).await {
                    return Some((Err(e), None));
                }
            }
        })
        .boxed()
    }

    async fn request(&self, endpoint: &Url, build: impl Fn() -> RequestBuilder) -> Result<Reply> {
        for attempt in 0..=self.retries {
            match self.attempt(endpoint, build(), attempt).await {
                Ok(reply) => return Ok(reply),
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
        request: RequestBuilder,
        attempt: u8,
    ) -> std::result::Result<Reply, Failure> {
        self.rate_limits.wait(endpoint, self.timeout).await?;
        let mut response = request
            .send()
            .await
            .map_err(|_| Failure::transient("DeviantArt request failed; check network access"))?;
        let status = response.status();
        if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
            let delay = retry_after(response.headers(), SystemTime::now());
            self.rate_limits
                .defer(endpoint, delay.unwrap_or_else(|| backoff(attempt)));
            return Err(Failure::transient(
                if status == StatusCode::TOO_MANY_REQUESTS {
                    match delay {
                        Some(delay) => format!(
                            "DeviantArt rate limit; retry after {} seconds",
                            delay.as_secs()
                        ),
                        None => "DeviantArt rate limit; retry later".into(),
                    }
                } else {
                    format!("DeviantArt returned HTTP {status}")
                },
            ));
        }
        if status.is_redirection() {
            return Err(error("DeviantArt responded with an unexpected redirect").into());
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| Failure::transient("could not read DeviantArt response"))?
        {
            if body.len() + chunk.len() > MAX_RESPONSE {
                return Err(error("DeviantArt response exceeded 16 MiB").into());
            }
            body.extend_from_slice(&chunk);
        }
        Ok(if status.is_success() {
            Reply::Ok(body)
        } else {
            Reply::Status(status)
        })
    }
}

enum Reply {
    Ok(Vec<u8>),
    Status(StatusCode),
}

fn status_error(status: StatusCode) -> crate::Error {
    match status.as_u16() {
        404 | 410 => error("DeviantArt deviation, user or folder not found"),
        400 | 401 | 403 => error(format!(
            "DeviantArt refused access (HTTP {status}); the deviation may be restricted"
        )),
        _ => error(format!("DeviantArt returned HTTP {status}")),
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

    const UUID: &str = "1A2B3C4D-0000-1111-2222-333344445555";

    fn uuid(n: u8) -> String {
        format!("00000000-0000-0000-0000-0000000000{n:02}")
    }

    fn deviation(id: &str, downloadable: bool) -> Value {
        json!({"deviationid": id, "title": "My Art", "published_time": "1704067200",
            "is_downloadable": downloadable, "author": {"username": "alice"},
            "content": {"src": "https://images-wixmp-abc.wixmp.com/f/x/my_art.jpg?token=secret",
                "width": 800, "height": 600}})
    }

    #[test]
    fn parses_deviations_and_skips_text() {
        let media = parse_deviation(&deviation(UUID, false), None).unwrap();
        assert_eq!(media.filename, format!("{UUID}.jpg"));
        let meta = media.metadata.as_ref().unwrap();
        assert_eq!(
            (meta.post_id.as_str(), meta.media_id.as_str()),
            (UUID, UUID)
        );
        assert_eq!(meta.username.as_deref(), Some("alice"));
        assert_eq!(meta.caption.as_deref(), Some("My Art"));
        assert_eq!((meta.timestamp, meta.index), (Some(1704067200), 1));
        let file = meta.file.as_ref().unwrap();
        assert_eq!((file.width, file.height), (Some(800), Some(600)));
        assert!(media.archive_key().unwrap().starts_with("deviantart:"));
        let download = json!({"src": "https://wixmp-x.wixmp.com/d/orig.png?token=t",
            "width": 4000, "height": 3000});
        let original = parse_deviation(&deviation(UUID, true), Some(&download)).unwrap();
        assert_eq!(original.filename, format!("{UUID}.png"));
        let file = original.metadata.unwrap().file.unwrap();
        assert_eq!(file.width, Some(4000));
        let bad = json!({"src": "https://evil.example/x.png"});
        assert!(parse_deviation(&deviation(UUID, true), Some(&bad)).is_some());
        let mut text = deviation(UUID, false);
        text.as_object_mut().unwrap().remove("content");
        assert!(parse_deviation(&text, None).is_none());
        let mut offsite = deviation(UUID, false);
        offsite["content"]["src"] = json!("http://images-wixmp-abc.wixmp.com/a.jpg");
        assert!(parse_deviation(&offsite, None).is_none());
        assert!(media_url("https://a.deviantart.net/x.gif").is_some());
        assert!(media_url("https://wixmp.com.evil.example/x.gif").is_none());
    }

    #[test]
    fn classifies_upstream_url_variants() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap()).unwrap();
        for raw in [
            "https://www.deviantart.com/view/904858796/",
            "https://www.deviantart.com/deviation/904858796",
            "https://www.deviantart.com/view.php?id=904858796",
            "https://www.fxdeviantart.com/view/904858796",
        ] {
            assert_eq!(t(raw), Target::DeviationId("904858796".into()), "{raw}");
        }
        assert_eq!(
            t("https://fav.me/ddijrpu"),
            Target::DeviationId(u64::from_str_radix("dijrpu", 36).unwrap().to_string())
        );
        for raw in [
            "https://www.deviantart.com/h3813067/favourites/",
            "https://www.deviantart.com/h3813067/favourites/all",
            "https://www.deviantart.com/h3813067/favourites/?catpath=/",
            "https://h3813067.deviantart.com/favourites/",
        ] {
            assert_eq!(
                t(raw),
                Target::Favorites {
                    user: "h3813067".into(),
                    folder: None
                },
                "{raw}"
            );
        }
        assert_eq!(
            t("https://www.deviantart.com/pencilshadings/favourites/70595441/3D-Favorites"),
            Target::Favorites {
                user: "pencilshadings".into(),
                folder: Some(("70595441".into(), Some("3d-favorites".into())))
            }
        );
        assert_eq!(
            t("https://www.deviantart.com/tag/nature"),
            Target::Tag("nature".into())
        );
        assert_eq!(
            t("https://www.deviantart.com/shimoda7/avatar"),
            Target::Avatar("shimoda7".into())
        );
        assert_eq!(
            t("https://www.deviantart.com/gdldev/banner"),
            Target::Banner("gdldev".into())
        );
        assert_eq!(
            t("https://www.deviantart.com/shimoda7/about#watching"),
            Target::Following("shimoda7".into())
        );
    }

    #[test]
    fn classifies_urls_and_finds_page_uuid() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        let art = Target::Deviation {
            user: "alice".into(),
            slug: "My-Art-123".into(),
        };
        assert_eq!(
            t("https://www.deviantart.com/alice/art/My-Art-123").unwrap(),
            art
        );
        assert_eq!(
            t("https://alice.deviantart.com/art/My-Art-123").unwrap(),
            art
        );
        let gallery = Target::Gallery("alice".into());
        for raw in [
            "https://www.deviantart.com/alice",
            "https://deviantart.com/alice/gallery",
            "https://www.deviantart.com/alice/gallery/all",
            "https://alice.deviantart.com/gallery/",
        ] {
            assert_eq!(t(raw).unwrap(), gallery, "{raw}");
        }
        assert_eq!(
            t("https://www.deviantart.com/alice/gallery/7/My-Folder").unwrap(),
            Target::Folder {
                user: "alice".into(),
                id: "7".into(),
                name: Some("my-folder".into())
            }
        );
        for raw in [
            "https://www.deviantart.com/",
            "https://www.deviantart.com/alice/art/no-id",
            "https://www.deviantart.com/alice/gallery/7",
            "https://www.deviantart.com:444/alice",
            "https://a.b.deviantart.com/",
            "https://www.deviantart.com.evil.example/alice",
        ] {
            assert!(t(raw).is_err(), "{raw}");
        }
        let html =
            format!(r#"<meta property="da:appurl" content="DeviantArt://deviation/{UUID}"/>"#);
        assert_eq!(page_uuid(&html).as_deref(), Some(UUID));
        assert_eq!(
            page_uuid(&format!(r#"x"deviationUuid":"{UUID}"y"#)).as_deref(),
            Some(UUID)
        );
        assert_eq!(page_uuid("DeviantArt://deviation/not-a-uuid"), None);
        assert_eq!(slugify("My  Folder!"), "my-folder");
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

    fn ok(value: Value) -> Vec<u8> {
        response("200 OK", value.to_string().as_bytes())
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
                let mut buffer = [0; 4096];
                loop {
                    if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                        let length = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length: "))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if request.len() >= end + 4 + length {
                            break;
                        }
                    }
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

    fn client(root: Url) -> DeviantArt {
        DeviantArt::new(Duration::from_secs(5))
            .unwrap()
            .with_network(&NetworkOptions::new(Some(String::new()), None).unwrap())
            .unwrap()
            .with_root(root)
    }

    fn token(value: &str) -> Vec<u8> {
        ok(json!({"access_token": value, "token_type": "Bearer"}))
    }

    #[tokio::test]
    async fn resolves_deviation_page_downloads_original_and_refreshes_once() {
        let page =
            format!(r#"<meta property="da:appurl" content="DeviantArt://deviation/{UUID}">"#);
        let (root, server) = serve(vec![
            response("200 OK", page.as_bytes()),
            token("first"),
            response("401 Unauthorized", b"{}"),
            token("second"),
            response("503 Service Unavailable", b""),
            ok(deviation(UUID, true)),
            ok(json!({"src": "https://wixmp-x.wixmp.com/d/orig.png?token=t"})),
        ]);
        let media: Vec<_> = client(root)
            .with_retries(1)
            .with_credentials(Some("42"), Some("topsecret"))
            .stream(&Url::parse("https://alice.deviantart.com/art/My-Art-123").unwrap())
            .collect()
            .await;
        assert_eq!(media.len(), 1, "{media:?}");
        assert_eq!(media[0].as_ref().unwrap().filename, format!("{UUID}.png"));
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /alice/art/My-Art-123 "));
        assert!(requests[1].starts_with("POST /oauth2/token "));
        assert!(
            requests[1]
                .contains("grant_type=client_credentials&client_id=42&client_secret=topsecret")
        );
        let detail = format!("GET /api/v1/oauth2/deviation/{UUID}?mature_content=true ");
        assert!(requests[2].starts_with(&detail));
        assert!(
            requests[2]
                .to_ascii_lowercase()
                .contains("authorization: bearer first")
        );
        assert!(requests[5].starts_with(&detail));
        assert!(
            requests[5]
                .to_ascii_lowercase()
                .contains("authorization: bearer second")
        );
        assert!(requests[6].starts_with(&format!(
            "GET /api/v1/oauth2/deviation/download/{UUID}?mature_content=true "
        )));
    }

    #[tokio::test]
    async fn paginates_galleries_and_resolves_folders() {
        let (root, server) = serve(vec![
            token("t"),
            ok(json!({"results": [{"folderid": uuid(9), "name": "Other"},
                {"folderid": uuid(8), "name": "My Folder"}], "has_more": false})),
            ok(json!({"results": [deviation(&uuid(1), false),
                {"deviationid": uuid(2), "title": "journal"}],
                "has_more": true, "next_offset": 24})),
            ok(json!({"results": [deviation(&uuid(3), false)], "has_more": false})),
        ]);
        let results: Vec<_> = client(root)
            .stream(&Url::parse("https://www.deviantart.com/alice/gallery/5/my-folder").unwrap())
            .collect()
            .await;
        assert_eq!(results.len(), 2, "{results:?}");
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with(
            "GET /api/v1/oauth2/gallery/folders?mature_content=true&username=alice&offset=0&limit=50 "
        ));
        let folder = format!(
            "GET /api/v1/oauth2/gallery/{}?mature_content=true&username=alice",
            uuid(8)
        );
        assert!(requests[2].starts_with(&format!("{folder}&offset=0&limit=24 ")));
        assert!(requests[3].starts_with(&format!("{folder}&offset=24&limit=24 ")));
    }

    #[tokio::test]
    async fn rejected_credentials_are_not_echoed() {
        let (root, server) = serve(vec![response("401 Unauthorized", b"{\"error\":\"x\"}")]);
        let error = client(root)
            .with_retries(3)
            .with_credentials(Some("42"), Some("topsecret"))
            .stream(&Url::parse("https://www.deviantart.com/alice").unwrap())
            .collect::<Vec<_>>()
            .await
            .remove(0)
            .unwrap_err()
            .to_string();
        assert!(error.contains("client-secret"), "{error}");
        assert!(!error.contains("topsecret") && !error.contains("127.0.0.1"));
        assert_eq!(server.join().unwrap().len(), 1);
    }
}
