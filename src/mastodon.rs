//! Experimental native Mastodon status and user media extraction.
//!
//! Mirrors gallery_dl/extractor/mastodon.py for the instances upstream ships
//! (mastodon.social, pawoo.net, baraag.net) using the public REST API,
//! including followed-account listings queued as user child jobs. All
//! instances use their upstream categories; the instance host is kept
//! in the `instance` metadata field and the archive key. Media attachments are
//! served from instance-chosen CDNs, so any HTTPS host is accepted for files;
//! no API headers or cookies are attached to media requests. User-configured
//! and `mastodon:URL` instances ([`crate::base_instances`]) use the same
//! routes against their root, keyed by its `host[:port]`.
use crate::{
    Result, error,
    event::{ExtractionEvent, KeywordPolicy, Post, group_with_keywords},
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
    " (native Mastodon extractor)"
);
/// Hosts recognized by upstream's built-in Mastodon instances.
pub const INSTANCES: [&str; 3] = ["mastodon.social", "pawoo.net", "baraag.net"];
const MAX_RESPONSE: usize = 16 * 1024 * 1024;
const PAGE_LIMIT: &str = "40";
const MAX_PAGES: usize = 100;

/// Upstream category for each supported instance (also the credential
/// scope): the built-in ones, or a configured/prefixed instance by its
/// root's `host[:port]` (upstream `instance`).
pub fn instance_category(host: &str) -> Option<&'static str> {
    match host {
        "mastodon.social" => Some("mastodon.social"),
        "pawoo.net" => Some("pawoo"),
        "baraag.net" => Some("baraag"),
        _ => configured(host).map(|instance| instance.category),
    }
}

/// The configured or prefixed Mastodon instance whose root has this
/// `host[:port]`.
fn configured(netloc: &str) -> Option<&'static crate::base_instances::Instance> {
    crate::base_instances::by_netloc(crate::base_instances::Base::Mastodon, netloc)
}

/// Whether `category` is the output category of a Mastodon instance.
pub fn is_instance_category(category: &str) -> bool {
    matches!(category, "mastodon.social" | "pawoo" | "baraag")
        || crate::base_instances::by_category(category)
            .is_some_and(|instance| instance.base == crate::base_instances::Base::Mastodon)
}

/// The instance (`host[:port]` of its root) serving an http(s) URL: a
/// built-in host, or a configured/prefixed instance's root.
pub fn instance_for_url(url: &Url) -> Option<String> {
    let host = url.host_str()?;
    if url.port().is_none() && INSTANCES.contains(&host) {
        return Some(host.to_owned());
    }
    crate::base_instances::find(url)
        .filter(|instance| instance.base == crate::base_instances::Base::Mastodon)
        .map(|instance| instance.netloc.to_owned())
}

/// A sensitive `Authorization` value for a bearer token.
fn bearer(token: &str) -> Result<header::HeaderValue> {
    if token.is_empty()
        || token.len() > 4096
        || !token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._~+/=".contains(&b))
    {
        return Err(error("invalid Mastodon access token"));
    }
    let mut value = header::HeaderValue::from_str(&format!("Bearer {token}"))
        .map_err(|_| error("invalid Mastodon access token"))?;
    value.set_sensitive(true);
    Ok(value)
}

/// Upstream's registered OAuth application (client ID and secret) for a
/// built-in instance, used by `oauth:mastodon:INSTANCE`. Upstream's shared
/// access tokens are deliberately not embedded.
pub fn application(host: &str) -> Option<(&'static str, &'static str)> {
    if let Some(instance) = configured(host) {
        return instance.client_id.zip(instance.client_secret);
    }
    match host {
        "mastodon.social" => Some((
            "dBSHdpsnOUZgxOnjKSQrWEPakO3ctM7HmsyoOd4FcRo",
            "DdrODTHs_XoeOsNVXnILTMabtdpWrWOAtrmw91wU1zI",
        )),
        "pawoo.net" => Some((
            "978a25f843ec01e53d09be2c290cd75c782bc3b7fdbd7ea4164b9f3c3780c8ff",
            "9208e3d4a7997032cf4f1b0e12e5df388428ef1fadb446dcfeb4f5ed6872d97b",
        )),
        "baraag.net" => Some((
            "czxx2qilLElYHQ_sm-lO8yXuGwOHxLX9RYYaD0-nq1o",
            "haMaFdMBgK_-BIxufakmI2gFgkYjqmgXGEO2tB-R2xY",
        )),
        _ => None,
    }
}

/// Upstream Mastodon status selection defaults.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelectionOptions {
    pub reblogs: bool,
    pub replies: bool,
    pub text_posts: bool,
    pub cards: bool,
}

impl Default for SelectionOptions {
    fn default() -> Self {
        Self {
            reblogs: false,
            replies: true,
            text_posts: false,
            cards: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Status {
        host: String,
        id: String,
    },
    /// `/objects/ID`: an ActivityPub object resolved to a status by redirect.
    Object {
        host: String,
        id: String,
    },
    User {
        host: String,
        acct: String,
    },
    /// Accounts followed by `acct`, each queued as a user child job.
    Following {
        host: String,
        acct: String,
    },
    Collection {
        host: String,
        feed: Collection,
    },
}

impl Target {
    /// The instance (`host[:port]` of its root) the target belongs to.
    pub fn instance(&self) -> &str {
        match self {
            Self::Status { host, .. }
            | Self::Object { host, .. }
            | Self::User { host, .. }
            | Self::Following { host, .. }
            | Self::Collection { host, .. } => host,
        }
    }
}

/// ActivityPub object IDs (e.g. UUIDs) are short URL-safe tokens.
fn valid_object_id(id: &str) -> bool {
    (1..=128).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

/// Mastodon collections use server-provided cursors; bookmark IDs are not status IDs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Collection {
    Bookmarks,
    Favourites,
    List(String),
    Hashtag(String),
}

fn hashtag(raw: &str) -> Option<String> {
    let mut bytes = Vec::new();
    let mut input = raw.bytes();
    while let Some(b) = input.next() {
        bytes.push(if b == b'%' {
            let high = (input.next()? as char).to_digit(16)? as u8;
            let low = (input.next()? as char).to_digit(16)? as u8;
            high * 16 + low
        } else {
            b
        });
        if bytes.len() > 512 {
            return None;
        }
    }
    let tag = String::from_utf8(bytes).ok()?;
    (!tag.is_empty()
        && tag.chars().count() <= 128
        && tag.chars().all(|c| c.is_alphanumeric() || c == '_'))
    .then_some(tag)
}

/// Status and attachment IDs are decimal snowflakes.
pub fn valid_id(id: &str) -> bool {
    (1..=24).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_digit())
}

/// Preview-card identities follow upstream's instance-owned thumbnail IDs.
pub fn valid_media_id(id: &str) -> bool {
    valid_id(id) || id.strip_prefix("card").is_some_and(valid_id)
}

fn card_id(url: &Url) -> String {
    let count = url.path_segments().map_or(0, Iterator::count);
    let mut id = String::new();
    if count >= 5 {
        for segment in url
            .path_segments()
            .into_iter()
            .flatten()
            .skip(3)
            .take(count - 5)
        {
            for byte in segment.bytes() {
                if !byte.is_ascii_digit() {
                    return "card".into();
                }
                if id.is_empty() && byte == b'0' {
                    continue;
                }
                id.push(byte as char);
                if id.len() > 24 {
                    return "card".into();
                }
            }
        }
    }
    format!("card{id}")
}

/// `user` or `user@domain`.
fn valid_acct(acct: &str) -> bool {
    if let Some(id) = acct.strip_prefix("id:") {
        return valid_id(id);
    }
    let (user, domain) = acct.split_once('@').unwrap_or((acct, ""));
    (1..=64).contains(&user.len())
        && user
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
        && domain.len() <= 253
        && domain
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'))
        && !(acct.contains('@') && domain.is_empty())
}

pub fn target(url: &Url) -> Result<Target> {
    let unsupported = || {
        error(
            "unsupported Mastodon URL; expected a status, object, user, following, bookmarks, favourites, list or hashtag link on mastodon.social, pawoo.net or baraag.net",
        )
    };
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(unsupported());
    }
    // Configured instances keep their root's host as `instance`.
    let host = instance_for_url(url).ok_or_else(unsupported)?;
    let segments: Vec<_> = url
        .path_segments()
        .map(|segments| segments.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    let segments = segments.strip_prefix(&["web"]).unwrap_or(&segments);
    let status = |id: &str| {
        valid_id(id)
            .then(|| Target::Status {
                host: host.clone(),
                id: id.into(),
            })
            .ok_or_else(unsupported)
    };
    let user = |name: &str| {
        let acct = name.strip_prefix('@').unwrap_or(name);
        valid_acct(acct)
            .then(|| Target::User {
                host: host.clone(),
                acct: acct.into(),
            })
            .ok_or_else(unsupported)
    };
    let collection = |feed| {
        Ok(Target::Collection {
            host: host.clone(),
            feed,
        })
    };
    let following = |name: &str| match user(name)? {
        Target::User { host, acct } => Ok(Target::Following { host, acct }),
        _ => unreachable!("user targets"),
    };
    match segments {
        ["statuses", id] | ["notice", id] | ["users", _, "statuses" | "notice", id] => status(id),
        ["objects", id] | ["users", _, "objects", id] if valid_object_id(id) => {
            Ok(Target::Object {
                host: host.clone(),
                id: (*id).into(),
            })
        }
        ["users", name] | ["users", name, "media"] => user(name),
        ["users", name, "following"] => following(name),
        [name, "following"] if name.starts_with('@') => following(name),
        [name, id] if name.starts_with('@') && *id != "media" => {
            user(name)?;
            status(id)
        }
        [name] | [name, "media"] if name.starts_with('@') => user(name),
        ["bookmarks"] => collection(Collection::Bookmarks),
        ["favourites"] => collection(Collection::Favourites),
        ["lists", id] if valid_id(id) => collection(Collection::List((*id).into())),
        ["tags", raw] => collection(Collection::Hashtag(hashtag(raw).ok_or_else(unsupported)?)),
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

/// Status and attachment IDs are JSON strings in the Mastodon API.
fn id_of(value: &Value) -> Option<&str> {
    string(value, "id").filter(|id| valid_id(id))
}

/// Simple HTML-to-text: paragraph and line breaks become newlines, tags are
/// dropped and the common entities are decoded.
pub fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start..].find('>') else {
            rest = "";
            break;
        };
        let tag = rest[start + 1..start + end].to_ascii_lowercase();
        if tag.starts_with("br") || tag == "/p" && !rest[start + end + 1..].is_empty() {
            out.push('\n');
        }
        rest = &rest[start + end + 1..];
    }
    out.push_str(rest);
    out.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .trim()
        .to_owned()
}

fn media_url(raw: &str) -> Option<Url> {
    let url = Url::parse(raw).ok()?;
    (url.scheme() == "https"
        && url.host_str().is_some()
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none())
    .then_some(url)
}

fn extension(url: &Url, kind: &str) -> String {
    url.path_segments()
        .and_then(|mut segments| segments.next_back())
        .and_then(|name| name.rsplit_once('.'))
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .filter(|ext| {
            (1..=5).contains(&ext.len()) && ext.bytes().all(|b| b.is_ascii_alphanumeric())
        })
        .map(|ext| if ext == "jpeg" { "jpg".into() } else { ext })
        .unwrap_or_else(|| {
            match kind {
                "image" => "jpg",
                "audio" => "mp3",
                _ => "mp4",
            }
            .into()
        })
}

fn selected(status: &Value, options: SelectionOptions) -> bool {
    id_of(status).is_some()
        && (options.reblogs || status.get("reblog").is_none_or(Value::is_null))
        && (options.replies || status.get("in_reply_to_id").is_none_or(Value::is_null))
}

fn post_metadata(status: &Value, instance: &str) -> PostMetadata {
    let username: Option<Arc<str>> = status
        .get("account")
        .and_then(|account| string(account, "acct"))
        .map(Arc::from);
    let caption: Option<Arc<str>> = string(status, "content")
        .map(strip_html)
        .filter(|c| !c.is_empty())
        .map(Arc::from);
    let timestamp = string(status, "created_at").and_then(|d| crate::date::parse_date(d).ok());
    PostMetadata {
        post_id: id_of(status).unwrap_or_default().into(),
        username,
        caption,
        timestamp,
        instance: Some(instance.into()),
        ..PostMetadata::default()
    }
}

/// Parse one status using the default media-only selection.
pub fn parse_status(status: &Value, instance: &str) -> Vec<Media> {
    parse_status_with_options(status, instance, SelectionOptions::default())
}

pub fn parse_status_with_options(
    status: &Value,
    instance: &str,
    options: SelectionOptions,
) -> Vec<Media> {
    if !selected(status, options) {
        return Vec::new();
    }
    let id = id_of(status).expect("selected status ID");
    let metadata = post_metadata(status, instance);
    let attachments = status
        .get("media_attachments")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .chain(
            status
                .get("reblog")
                .and_then(|s| s.get("media_attachments"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten(),
        );
    let mut seen = HashSet::new();
    let mut media = Vec::new();
    for attachment in attachments {
        let Some(kind) = string(attachment, "type")
            .filter(|kind| matches!(*kind, "image" | "gifv" | "video" | "audio"))
        else {
            continue;
        };
        let Some(media_id) = id_of(attachment) else {
            continue;
        };
        let Some(url) = ["url", "remote_url"]
            .into_iter()
            .find_map(|key| string(attachment, key).and_then(media_url))
        else {
            continue;
        };
        if !seen.insert(media_id) {
            continue;
        }
        let original = attachment.pointer("/meta/original").unwrap_or(&Value::Null);
        let positive = |key: &str| original.get(key).and_then(Value::as_u64).filter(|v| *v > 0);
        let index = media.len() + 1;
        let ext = extension(&url, kind);
        media.push(Media {
            url,
            job: MediaJob::DirectHttp,
            category: instance_category(instance).unwrap_or("mastodon").into(),
            filename: format!("{id}_{media_id}.{ext}"),
            metadata: Some(PostMetadata {
                file: Some(FileMetadata {
                    width: positive("width"),
                    height: positive("height"),
                    duration: original
                        .get("duration")
                        .and_then(Value::as_f64)
                        .filter(|d| d.is_finite() && *d >= 0.0),
                    media_type: Some(kind.into()),
                    description: string(attachment, "description").map(Arc::from),
                    ..FileMetadata::default()
                }),
                post_id: id.into(),
                media_id: media_id.into(),
                index,
                ..metadata.clone()
            }),
            headers: BTreeMap::new(),
            handoff: BTreeMap::new(),
        });
    }
    if options.cards
        && let Some(card) = status.get("card")
        && let Some(url) = string(card, "image").and_then(media_url)
    {
        let media_id = card_id(&url);
        let ext = extension(&url, "image");
        let positive = |key| card.get(key).and_then(Value::as_u64).filter(|n| *n > 0);
        media.push(Media {
            filename: format!("{id}_{media_id}.{ext}"),
            url,
            job: MediaJob::DirectHttp,
            category: instance_category(instance).unwrap_or("mastodon").into(),
            metadata: Some(PostMetadata {
                media_id,
                index: media.len() + 1,
                file: Some(FileMetadata {
                    width: positive("width"),
                    height: positive("height"),
                    media_type: Some("card".into()),
                    description: string(card, "description")
                        .or_else(|| string(card, "title"))
                        .map(Arc::from),
                    ..FileMetadata::default()
                }),
                ..metadata
            }),
            headers: BTreeMap::new(),
            handoff: BTreeMap::new(),
        });
    }
    media
}

fn json(bytes: &[u8]) -> Result<Value> {
    serde_json::from_slice(bytes).map_err(|_| error("Mastodon returned invalid JSON"))
}

/// Parse a `/api/v1/statuses/ID` response.
pub fn parse_single(bytes: &[u8], id: &str, instance: &str) -> Result<Vec<Media>> {
    let status = json(bytes)?;
    if id_of(&status) != Some(id) {
        return Err(error(
            "Mastodon response does not match the requested status",
        ));
    }
    let media = parse_status(&status, instance);
    if media.is_empty() {
        return Err(error("Mastodon status contains no supported media"));
    }
    Ok(media)
}

/// One timeline page: per-status files and the last status ID seen.
pub struct Page {
    pub statuses: Vec<Vec<Media>>,
    pub last_id: Option<String>,
}

pub fn parse_timeline(bytes: &[u8], instance: &str) -> Result<Page> {
    let body = json(bytes)?;
    let entries = body
        .as_array()
        .ok_or_else(|| error("Mastodon response is not a status list"))?;
    let last_id = entries.last().and_then(id_of).map(str::to_owned);
    let statuses = entries
        .iter()
        .map(|status| parse_status(status, instance))
        .filter(|media| !media.is_empty())
        .collect();
    Ok(Page { statuses, last_id })
}

/// Extract `max_id` from the `rel="next"` entry of a Link header.
pub fn next_max_id(link: &str) -> Option<String> {
    link.split(',').find_map(|part| {
        let (target, params) = part.split_once(';')?;
        if !params
            .split(';')
            .any(|p| matches!(p.trim(), "rel=\"next\"" | "rel=next"))
        {
            return None;
        }
        let url = Url::parse(target.trim().strip_prefix('<')?.strip_suffix('>')?).ok()?;
        url.query_pairs()
            .find(|(key, _)| key == "max_id")
            .map(|(_, id)| id.into_owned())
            .filter(|id| valid_id(id))
    })
}

pub struct Mastodon {
    client: Client,
    network: NetworkOptions,
    api_root: Option<Url>,
    timeout: Duration,
    request_timeout: Duration,
    retries: u8,
    rate_limits: RateLimits,
    selection: SelectionOptions,
    access_token: Option<(String, header::HeaderValue)>,
    /// The instance whose cached access token applies, until looked up.
    cached_token_instance: Option<String>,
    /// A configured instance's own `access-token` (upstream
    /// `config_instance`), used when neither setting nor cache provides one.
    instance_token: Option<String>,
}

struct TimelineState {
    root: Url,
    host: String,
    path: String,
    account_query: bool,
    seen_cursors: HashSet<String>,
    max_id: Option<String>,
    pages: usize,
    done: bool,
    pending: VecDeque<ExtractionEvent>,
}

impl Mastodon {
    pub fn new(timeout: Duration) -> Result<Self> {
        Ok(Self {
            client: NetworkOptions::default().client(USER_AGENT, timeout)?,
            network: NetworkOptions::default(),
            api_root: None,
            timeout,
            request_timeout: timeout,
            retries: 0,
            rate_limits: RateLimits::default(),
            selection: SelectionOptions::default(),
            access_token: None,
            cached_token_instance: None,
            instance_token: None,
        })
    }

    /// Explicit bearer credentials are sent only to this extractor's API origin.
    pub fn with_access_token_for(mut self, instance: &str, token: Option<&str>) -> Result<Self> {
        if instance_category(instance).is_none() {
            return Err(error("unsupported Mastodon credential instance"));
        }
        // Unset or "cache" defers to the credential cache (`with_token_cache`).
        self.cached_token_instance = token
            .is_none_or(|token| token == "cache")
            .then(|| instance.to_owned());
        self.access_token = match token.filter(|token| !token.is_empty() && *token != "cache") {
            None => None,
            Some(token) => Some((instance.into(), bearer(token)?)),
        };
        self.instance_token = None;
        if self.access_token.is_none()
            && let Some(token) = configured(instance).and_then(|known| known.access_token)
        {
            bearer(token)?;
            self.instance_token = Some(instance.into());
        }
        Ok(self)
    }

    /// Upstream `_access_token_cache`: without a configured `access-token`
    /// (or with `"cache"`), use the one `oauth:mastodon:INSTANCE` cached.
    /// Call after `with_access_token_for`; unreadable caches count as empty.
    pub async fn with_token_cache(mut self, cache: Option<&crate::cache::Cache>) -> Self {
        if let Some(cache) = cache
            && let Some(instance) = self.cached_token_instance.take()
        {
            let key = crate::cache::key("extractor.mastodon", "_access_token_cache", &instance);
            if let Ok(Some(token)) = cache.get_str(&key).await
                && let Ok(value) = bearer(&token)
            {
                self.access_token = Some((instance, value));
            }
        }
        self.with_instance_token()
    }

    /// Fall back to the configured instance's own `access-token` when no
    /// setting or cached token applies.
    fn with_instance_token(mut self) -> Self {
        if let Some(instance) = self.instance_token.take()
            && self.access_token.is_none()
            && let Some(token) = configured(&instance).and_then(|known| known.access_token)
            && let Ok(value) = bearer(token)
        {
            self.access_token = Some((instance, value));
        }
        self
    }

    pub fn with_selection(mut self, selection: SelectionOptions) -> Self {
        self.selection = selection;
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

    /// Override the API origin for every instance; intended for local tests.
    pub fn with_api_root(mut self, root: Url) -> Self {
        self.api_root = Some(root);
        self
    }

    fn root(&self, host: &str) -> Url {
        self.api_root.clone().unwrap_or_else(|| {
            configured(host).map_or_else(
                || Url::parse(&format!("https://{host}/")).expect("known instance"),
                |instance| instance.root.clone(),
            )
        })
    }

    async fn call(
        &self,
        root: &Url,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<(Vec<u8>, Option<String>)> {
        let mut endpoint = root.join(path).expect("validated Mastodon path");
        if !query.is_empty() {
            endpoint.query_pairs_mut().extend_pairs(query);
        }
        tokio::time::timeout(self.timeout, self.request(&endpoint))
            .await
            .map_err(|_| error("native Mastodon extraction timed out"))?
    }

    fn finish(&self, mut media: Vec<Media>) -> VecDeque<ExtractionEvent> {
        for item in &mut media {
            self.network.apply_media(item);
        }
        let category = media
            .first()
            .map_or("mastodon", |m| m.category.as_str())
            .to_owned();
        group(media, &category)
    }

    fn status_events(&self, status: &Value, host: &str) -> VecDeque<ExtractionEvent> {
        if !selected(status, self.selection) {
            return VecDeque::new();
        }
        let media = parse_status_with_options(status, host, self.selection);
        if !media.is_empty() {
            return self.finish(media);
        }
        if !self.selection.text_posts {
            return VecDeque::new();
        }
        VecDeque::from([
            ExtractionEvent::Post(Box::new(Post {
                keywords: Default::default(),
                category: instance_category(host).unwrap_or("mastodon").into(),
                metadata: Some(post_metadata(status, host)),
                count: Some(0),
            })),
            ExtractionEvent::EndPost,
        ])
    }

    async fn next_page(&self, state: &mut TimelineState) -> Result<()> {
        let mut query = if state.account_query {
            vec![
                (
                    "only_media",
                    if self.selection.reblogs || self.selection.text_posts || self.selection.cards {
                        "false"
                    } else {
                        "true"
                    },
                ),
                (
                    "exclude_reblogs",
                    if self.selection.reblogs {
                        "false"
                    } else {
                        "true"
                    },
                ),
                (
                    "exclude_replies",
                    if self.selection.replies {
                        "false"
                    } else {
                        "true"
                    },
                ),
                ("limit", PAGE_LIMIT),
            ]
        } else {
            vec![("limit", PAGE_LIMIT)]
        };
        if let Some(max_id) = &state.max_id {
            query.push(("max_id", max_id));
        }
        let (body, link) = self.call(&state.root, &state.path, &query).await?;
        let body = json(&body)?;
        let entries = body
            .as_array()
            .ok_or_else(|| error("Mastodon response is not a status list"))?;
        let last_id = entries.last().and_then(id_of).map(str::to_owned);
        state.pages += 1;
        let next = link
            .as_deref()
            .and_then(next_max_id)
            .or_else(|| state.account_query.then_some(last_id).flatten());
        state.done = next
            .as_ref()
            .is_none_or(|cursor| !state.seen_cursors.insert(cursor.clone()))
            || state.pages >= MAX_PAGES;
        state.max_id = next;
        for status in entries {
            state
                .pending
                .extend(self.status_events(status, &state.host));
        }
        Ok(())
    }

    pub fn stream(&self, url: &Url) -> BoxStream<'_, Result<Media>> {
        crate::event::media_only(self.events(url))
    }

    /// Emit one post boundary per status; timelines paginate lazily. Followed
    /// accounts are not queued; see [`Mastodon::events_with_queue`].
    pub fn events(&self, url: &Url) -> BoxStream<'_, Result<ExtractionEvent>> {
        self.events_with_queue(url, Arc::new(|_| None))
    }

    /// Like [`Mastodon::events`], routing followed accounts' user URLs through
    /// `queue` as `following` child jobs (upstream `MastodonFollowingExtractor`).
    pub fn events_with_queue<'a>(
        &'a self,
        url: &Url,
        queue: crate::event::Queue<'a>,
    ) -> BoxStream<'a, Result<ExtractionEvent>> {
        let target = match target(url) {
            Ok(target) => target,
            Err(e) => return stream::once(async move { Err(e) }).boxed(),
        };
        match target {
            Target::Following { host, acct } => self.following_events(host, acct, queue),
            Target::Object { host, id } => stream::once(async move {
                let id = self.resolve_object(&host, &id).await?;
                Ok::<_, crate::Error>(self.status_stream(host, id))
            })
            .map(|result| match result {
                Ok(events) => events,
                Err(e) => stream::once(async move { Err(e) }).boxed(),
            })
            .flatten()
            .boxed(),
            Target::Status { host, id } => self.status_stream(host, id),
            target => self.timeline_events(target),
        }
    }

    fn status_stream(&self, host: String, id: String) -> BoxStream<'_, Result<ExtractionEvent>> {
        stream::once(async move {
            let root = self.root(&host);
            let (body, _) = self
                .call(&root, &format!("api/v1/statuses/{id}"), &[])
                .await?;
            let status = json(&body)?;
            if id_of(&status) != Some(id.as_str()) {
                return Err(error(
                    "Mastodon response does not match the requested status",
                ));
            }
            let events = self.status_events(&status, &host);
            if events.is_empty() && selected(&status, self.selection) {
                return Err(error("Mastodon status contains no supported media"));
            }
            Ok::<_, crate::Error>(stream::iter(events.into_iter().map(Ok)))
        })
        .map(|result| match result {
            Ok(events) => events.boxed(),
            Err(e) => stream::once(async move { Err(e) }).boxed(),
        })
        .flatten()
        .boxed()
    }

    fn timeline_events(&self, target: Target) -> BoxStream<'_, Result<ExtractionEvent>> {
        stream::unfold(Some((target, None)), move |state| async move {
            let (target, state) = state?;
            let mut state: TimelineState = match state {
                Some(state) => state,
                None => match self.timeline(&target).await {
                    Ok(state) => state,
                    Err(e) => return Some((Err(e), None)),
                },
            };
            loop {
                if let Some(event) = state.pending.pop_front() {
                    return Some((Ok(event), Some((target, Some(state)))));
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

    /// Upstream `account_id_by_username`: `id:` accounts skip the lookup, and
    /// a failed lookup falls back to an exact `acct` match in account search.
    async fn account_id(&self, host: &str, acct: &str) -> Result<String> {
        if let Some(id) = acct.strip_prefix("id:") {
            return Ok(id.to_owned());
        }
        let root = self.root(host);
        let lookup = async {
            let (body, _) = self
                .call(&root, "api/v1/accounts/lookup", &[("acct", acct)])
                .await?;
            id_of(&json(&body)?)
                .map(str::to_owned)
                .ok_or_else(|| error("Mastodon account lookup returned no account ID"))
        };
        if let Ok(id) = lookup.await {
            return Ok(id);
        }
        let handle = if acct.contains('@') {
            format!("@{acct}")
        } else {
            format!("@{acct}@{host}")
        };
        let (body, _) = self
            .call(
                &root,
                "api/v1/accounts/search",
                &[("q", &handle), ("limit", "1")],
            )
            .await?;
        json(&body)?
            .as_array()
            .into_iter()
            .flatten()
            .find(|account| string(account, "acct") == Some(acct))
            .and_then(id_of)
            .map(str::to_owned)
            .ok_or_else(|| error("Mastodon account not found"))
    }

    /// Resolve an `/objects/ID` URL to its status ID from the redirect target.
    async fn resolve_object(&self, host: &str, id: &str) -> Result<String> {
        let url = self
            .root(host)
            .join(&format!("objects/{id}"))
            .expect("validated object ID");
        let resolve = async {
            self.rate_limits.wait(&url, self.timeout).await?;
            let response = self
                .client
                .get(url.clone())
                .timeout(self.request_timeout)
                .send()
                .await
                .map_err(|_| error("Mastodon request failed; check network access"))?;
            if matches!(response.status().as_u16(), 404 | 410) {
                return Err(error("Mastodon object not found"));
            }
            response
                .headers()
                .get(header::LOCATION)
                .filter(|_| response.status().is_redirection())
                .and_then(|value| value.to_str().ok())
                .and_then(|value| url.join(value).ok())
                .and_then(|location| {
                    location
                        .path_segments()?
                        .rfind(|segment| !segment.is_empty())
                        .map(str::to_owned)
                })
                .filter(|id| valid_id(id))
                .ok_or_else(|| error("Mastodon object did not redirect to a status"))
        };
        tokio::time::timeout(self.timeout, resolve)
            .await
            .map_err(|_| error("native Mastodon extraction timed out"))?
    }

    /// Lazily page `/api/v1/accounts/ID/following`, queueing each followed
    /// account's user URL that names a supported instance.
    fn following_events<'a>(
        &'a self,
        host: String,
        acct: String,
        queue: crate::event::Queue<'a>,
    ) -> BoxStream<'a, Result<ExtractionEvent>> {
        struct State {
            host: String,
            acct: String,
            path: Option<String>,
            max_id: Option<String>,
            seen_cursors: HashSet<String>,
            pages: usize,
            done: bool,
            accounts: VecDeque<Value>,
        }
        let category = instance_category(&host).unwrap_or("mastodon");
        let state = State {
            host,
            acct,
            path: None,
            max_id: None,
            seen_cursors: HashSet::new(),
            pages: 0,
            done: false,
            accounts: VecDeque::new(),
        };
        stream::unfold(Some(state), move |state| async move {
            let mut state = state?;
            loop {
                if let Some(account) = state.accounts.pop_front() {
                    return Some((Ok(account), Some(state)));
                }
                if state.done {
                    return None;
                }
                let page = async {
                    let path = match &state.path {
                        Some(path) => path.clone(),
                        None => {
                            let id = self.account_id(&state.host, &state.acct).await?;
                            let path = format!("api/v1/accounts/{id}/following");
                            state.path = Some(path.clone());
                            path
                        }
                    };
                    let mut query = vec![("limit", PAGE_LIMIT)];
                    if let Some(max_id) = &state.max_id {
                        query.push(("max_id", max_id));
                    }
                    let (body, link) = self.call(&self.root(&state.host), &path, &query).await?;
                    let body = json(&body)?;
                    let accounts = body
                        .as_array()
                        .ok_or_else(|| error("Mastodon response is not an account list"))?;
                    state.pages += 1;
                    let next = link.as_deref().and_then(next_max_id);
                    state.done = next
                        .as_ref()
                        .is_none_or(|cursor| !state.seen_cursors.insert(cursor.clone()))
                        || state.pages >= MAX_PAGES;
                    state.max_id = next;
                    state.accounts.extend(accounts.iter().cloned());
                    Ok::<_, crate::Error>(())
                };
                if let Err(e) = page.await {
                    return Some((Err(e), None));
                }
            }
        })
        .flat_map(move |result| match result {
            Err(e) => stream::once(async move { Err(e) }).boxed(),
            Ok(account) => {
                // Upstream queues `account["url"]` for its user extractor;
                // accounts on other instances are unsupported there too.
                let child = string(&account, "url")
                    .and_then(|raw| Url::parse(raw).ok())
                    .filter(|url| matches!(target(url), Ok(Target::User { .. })))
                    .and_then(|url| queue(url));
                match child {
                    Some(events) => {
                        let mut fields = serde_json::Map::new();
                        for key in ["id", "username", "acct", "display_name", "url"] {
                            if let Some(value) = string(&account, key) {
                                fields.insert(key.into(), value.into());
                            }
                        }
                        crate::event::child_job(
                            crate::event::ChildMetadata::from_raw(category, "following", &fields),
                            move || events,
                        )
                    }
                    None => stream::empty().boxed(),
                }
            }
        })
        .boxed()
    }

    async fn timeline(&self, target: &Target) -> Result<TimelineState> {
        let (host, path, account_query) = match target {
            Target::User { host, acct } => {
                let id = self.account_id(host, acct).await?;
                (host, format!("api/v1/accounts/{id}/statuses"), true)
            }
            Target::Collection { host, feed } => {
                let path = match feed {
                    Collection::Bookmarks => "api/v1/bookmarks".into(),
                    Collection::Favourites => "api/v1/favourites".into(),
                    Collection::List(id) => format!("api/v1/timelines/list/{id}"),
                    Collection::Hashtag(tag) => format!("api/v1/timelines/tag/{tag}"),
                };
                (host, path, false)
            }
            Target::Status { .. } | Target::Object { .. } | Target::Following { .. } => {
                unreachable!("statuses and following lists do not have timeline state")
            }
        };
        Ok(TimelineState {
            root: self.root(host),
            host: host.clone(),
            path,
            account_query,
            seen_cursors: HashSet::new(),
            max_id: None,
            pages: 0,
            done: false,
            pending: VecDeque::new(),
        })
    }

    fn authorization(&self, target: &Url) -> Option<&header::HeaderValue> {
        let (instance, token) = self.access_token.as_ref()?;
        (target.path().starts_with("/api/") && target.origin() == self.root(instance).origin())
            .then_some(token)
    }

    async fn request(&self, endpoint: &Url) -> Result<(Vec<u8>, Option<String>)> {
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

    async fn attempt(
        &self,
        endpoint: &Url,
        attempt: u8,
    ) -> std::result::Result<(Vec<u8>, Option<String>), Failure> {
        let mut target = endpoint.clone();
        let mut redirects = Redirects::new(endpoint);
        let mut response = loop {
            self.rate_limits.wait(&target, self.timeout).await?;
            let mut request = self
                .client
                .get(target.clone())
                .timeout(self.request_timeout)
                .header(header::ACCEPT, "application/json");
            if let Some(token) = self.authorization(&target) {
                request = request.header(header::AUTHORIZATION, token.clone());
            }
            let response = request
                .send()
                .await
                .map_err(|_| Failure::transient("Mastodon request failed; check network access"))?;
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
                            "Mastodon rate limit; retry after {} seconds",
                            delay.as_secs()
                        ),
                        None => "Mastodon rate limit; retry later".into(),
                    }
                } else {
                    format!("Mastodon returned HTTP {status}")
                },
            ));
        }
        if matches!(status.as_u16(), 404 | 410) {
            return Err(error("Mastodon status or account not found").into());
        }
        if matches!(status.as_u16(), 401 | 403) {
            return Err(error(
                "Mastodon refused access; configure a valid mastodon access-token for private or authorization-required content",
            )
            .into());
        }
        if !status.is_success() {
            return Err(error(format!("Mastodon returned HTTP {status}")).into());
        }
        let links = response
            .headers()
            .get_all(header::LINK)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect::<Vec<_>>();
        let link = (!links.is_empty()).then(|| links.join(","));
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| Failure::transient("could not read Mastodon response"))?
        {
            if body.len() + chunk.len() > MAX_RESPONSE {
                return Err(error("Mastodon response exceeded 16 MiB").into());
            }
            body.extend_from_slice(&chunk);
        }
        Ok((body, link))
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

// MastodonExtractor.items reuses status, assigns media, and derives its name.
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
            "media",
            "file",
        ]),
    )
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

    fn status(id: &str) -> Value {
        json!({"id": id, "created_at": "2024-01-02T03:04:05.000Z", "reblog": null,
            "content": "<p>Hello &amp; <a href=\"x\">world</a></p><p>line<br>two</p>",
            "account": {"id": "9", "acct": "alice"},
            "media_attachments": [
                {"id": format!("{id}1"), "type": "image",
                 "url": "https://files.mastodon.social/media_attachments/files/1/original/a.jpeg",
                 "description": "alt text",
                 "meta": {"original": {"width": 800, "height": 600}}},
                {"id": format!("{id}2"), "type": "unknown", "url": "https://files.example/x.bin"},
                {"id": format!("{id}3"), "type": "video", "url": "http://insecure.example/v.mp4"},
                {"id": format!("{id}4"), "type": "gifv",
                 "url": "https://cdn.example/media/b.mp4",
                 "meta": {"original": {"width": 10, "height": 20, "duration": 1.5}}}]})
    }

    #[test]
    fn shared_keyword_policy_declares_source_file_assignments() {
        let media = parse_single(
            status("100").to_string().as_bytes(),
            "100",
            "mastodon.social",
        )
        .unwrap();
        let events = group(media, "mastodon.social");
        let ExtractionEvent::Post(post) = &events[0] else {
            panic!("missing post");
        };
        let KeywordPolicy::Shared { file_fields, .. } = &post.keywords else {
            panic!("expected shared Mastodon keywords");
        };
        for field in ["num", "index", "media_id", "media", "filename", "extension"] {
            assert!(file_fields.iter().any(|f| f == field));
        }
        for field in ["width", "height", "description", "caption"] {
            assert!(!file_fields.iter().any(|f| f == field));
        }
    }

    #[test]
    fn parses_status_metadata() {
        let body = status("100").to_string();
        let media = parse_single(body.as_bytes(), "100", "mastodon.social").unwrap();
        assert_eq!(media.len(), 2);
        assert_eq!(media[0].category, "mastodon.social");
        assert_eq!(
            media[0].url.as_str(),
            "https://files.mastodon.social/media_attachments/files/1/original/a.jpeg"
        );
        assert_eq!(media[0].filename, "100_1001.jpg");
        assert_eq!(media[1].filename, "100_1004.mp4");
        let meta = media[0].metadata.as_ref().unwrap();
        assert_eq!(meta.post_id, "100");
        assert_eq!(meta.media_id, "1001");
        assert_eq!(meta.username.as_deref(), Some("alice"));
        assert_eq!(meta.caption.as_deref(), Some("Hello & world\nline\ntwo"));
        assert_eq!(meta.timestamp, Some(1704164645));
        assert_eq!(meta.index, 1);
        assert_eq!(meta.instance.as_deref(), Some("mastodon.social"));
        let file = meta.file.as_ref().unwrap();
        assert_eq!((file.width, file.height), (Some(800), Some(600)));
        assert_eq!(file.description.as_deref(), Some("alt text"));
        assert_eq!(file.media_type.as_deref(), Some("image"));
        let gif = media[1].metadata.as_ref().unwrap();
        assert_eq!(gif.index, 2);
        assert_eq!(gif.file.as_ref().unwrap().duration, Some(1.5));
        assert!(media[0].headers.is_empty());
        let key = media[0].archive_key().unwrap();
        assert!(key.starts_with("mastodon:"), "{key}");
        let mut legacy = media[0].clone();
        legacy.category = "mastodon".into();
        assert_eq!(legacy.archive_key(), Some(key.clone()));
        legacy.category = "pawoo".into();
        assert!(legacy.archive_key().is_none());
        assert_ne!(Some(key.clone()), media[1].archive_key());
        let other = parse_status(&status("100"), "pawoo.net");
        assert_ne!(Some(key), other[0].archive_key());
    }

    #[test]
    fn rejects_mismatched_reblogged_and_empty_statuses() {
        let body = status("100").to_string();
        assert!(parse_single(body.as_bytes(), "101", "mastodon.social").is_err());
        let mut reblog = status("100");
        reblog["reblog"] = status("200");
        assert!(parse_status(&reblog, "mastodon.social").is_empty());
        let empty = json!({"id": "5", "media_attachments": []}).to_string();
        assert!(
            parse_single(empty.as_bytes(), "5", "mastodon.social")
                .unwrap_err()
                .to_string()
                .contains("no supported media")
        );
        assert!(parse_single(b"nope", "5", "mastodon.social").is_err());
    }

    #[test]
    fn parses_timeline_and_link_header() {
        let body = json!([status("30"), {"id": "20", "media_attachments": []}]).to_string();
        let page = parse_timeline(body.as_bytes(), "baraag.net").unwrap();
        assert_eq!(page.statuses.len(), 1);
        assert_eq!(page.last_id.as_deref(), Some("20"));
        let link = "<https://mastodon.social/api/v1/accounts/9/statuses?max_id=20&only_media=true>; rel=\"next\", <https://mastodon.social/api/v1/accounts/9/statuses?min_id=30>; rel=\"prev\"";
        assert_eq!(next_max_id(link).as_deref(), Some("20"));
        assert_eq!(next_max_id("<https://x/?min_id=1>; rel=\"prev\""), None);
        assert_eq!(next_max_id("<https://x/?max_id=a>; rel=\"next\""), None);
    }

    #[test]
    fn classifies_supported_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        let status = |host: &str, id: &str| Target::Status {
            host: host.into(),
            id: id.into(),
        };
        let user = |host: &str, acct: &str| Target::User {
            host: host.into(),
            acct: acct.into(),
        };
        assert_eq!(
            t("https://mastodon.social/@alice/123").unwrap(),
            status("mastodon.social", "123")
        );
        assert_eq!(
            t("https://pawoo.net/web/statuses/456").unwrap(),
            status("pawoo.net", "456")
        );
        assert_eq!(
            t("https://baraag.net/@bob@other.example/789").unwrap(),
            status("baraag.net", "789")
        );
        for raw in [
            "https://mastodon.social/@alice",
            "https://mastodon.social/@alice/media",
            "https://mastodon.social/web/@alice",
        ] {
            assert_eq!(t(raw).unwrap(), user("mastodon.social", "alice"), "{raw}");
        }
        assert_eq!(
            t("https://pawoo.net/@bob@remote.example/media").unwrap(),
            user("pawoo.net", "bob@remote.example")
        );
        for raw in [
            "https://mastodon.social/",
            "https://mastodon.social/alice",
            "https://mastodon.social/@alice/abc",
            "https://mastodon.social/@ali%2Fce",
            "https://mastodon.social/@alice@",
            "https://mastodon.social:444/@alice",
            "https://mastodon.social.evil.example/@alice",
            "https://files.mastodon.social/@alice",
            "https://example.social/@alice",
        ] {
            assert!(t(raw).is_err(), "{raw}");
        }
    }

    #[test]
    fn selects_reblogs_and_replies_without_changing_attachment_identity() {
        let mut outer = status("100");
        outer["reblog"] = status("200");
        assert!(parse_status(&outer, "mastodon.social").is_empty());
        let options = SelectionOptions {
            reblogs: true,
            ..SelectionOptions::default()
        };
        let media = parse_status_with_options(&outer, "mastodon.social", options);
        assert_eq!(media.len(), 4);
        assert_eq!(media[2].filename, "100_2001.jpg");
        let meta = media[2].metadata.as_ref().unwrap();
        assert_eq!(meta.post_id, "100");
        assert_eq!(meta.index, 3);
        assert_eq!(meta.username.as_deref(), Some("alice"));
        assert_eq!(
            media[2].archive_key(),
            parse_status(&status("200"), "mastodon.social")[0].archive_key()
        );
        outer["in_reply_to_id"] = serde_json::json!("50");
        assert!(
            parse_status_with_options(
                &outer,
                "mastodon.social",
                SelectionOptions {
                    replies: false,
                    ..options
                }
            )
            .is_empty()
        );
        assert_eq!(
            parse_status_with_options(&outer, "mastodon.social", options).len(),
            4
        );
    }

    #[tokio::test]
    async fn text_posts_emit_metadata_boundaries_and_preserve_later_media() {
        let mut text = status("32");
        text["media_attachments"] = serde_json::json!([]);
        let mut reply = status("31");
        reply["in_reply_to_id"] = serde_json::json!("30");
        let (root, server) = serve(vec![
            response("200 OK", "", br#"{"id":"9","acct":"alice"}"#),
            response(
                "200 OK",
                "",
                serde_json::json!([text, reply, status("30")])
                    .to_string()
                    .as_bytes(),
            ),
            response("200 OK", "", b"[]"),
        ]);
        let native = mastodon(root).with_selection(SelectionOptions {
            text_posts: true,
            replies: false,
            ..SelectionOptions::default()
        });
        let events = native
            .events(&Url::parse("https://mastodon.social/@alice").unwrap())
            .collect::<Vec<_>>()
            .await;
        assert_eq!(events.len(), 6, "{events:?}");
        let Ok(ExtractionEvent::Post(post)) = &events[0] else {
            panic!("missing text post")
        };
        assert_eq!(post.count, Some(0));
        assert_eq!(post.metadata.as_ref().unwrap().post_id, "32");
        assert_eq!(
            post.metadata.as_ref().unwrap().caption.as_deref(),
            Some("Hello & world\nline\ntwo")
        );
        assert!(matches!(events[1], Ok(ExtractionEvent::EndPost)));
        let Ok(ExtractionEvent::Post(post)) = &events[2] else {
            panic!("missing media post")
        };
        assert_eq!(post.metadata.as_ref().unwrap().post_id, "30");
        let requests = server.join().unwrap();
        assert!(requests[1].contains("only_media=false&exclude_reblogs=true&exclude_replies=true"));
        assert!(requests[2].contains("max_id=30"));
    }

    #[tokio::test]
    async fn selected_reblogs_request_non_media_pages_and_stop_lazily() {
        let mut outer = status("100");
        outer["media_attachments"] = serde_json::json!([]);
        outer["reblog"] = status("200");
        let (root, server) = serve(vec![
            response("200 OK", "", br#"{"id":"9","acct":"alice"}"#),
            response(
                "200 OK",
                "Link: <https://mastodon.social/?max_id=100>; rel=next\r\n",
                serde_json::json!([outer]).to_string().as_bytes(),
            ),
        ]);
        let native = mastodon(root).with_selection(SelectionOptions {
            reblogs: true,
            ..SelectionOptions::default()
        });
        let url = Url::parse("https://mastodon.social/@alice").unwrap();
        let mut events = native.events(&url);
        assert!(matches!(
            events.next().await.unwrap().unwrap(),
            ExtractionEvent::Post(_)
        ));
        drop(events);
        let requests = server.join().unwrap();
        assert!(
            requests[1].contains("only_media=false&exclude_reblogs=false&exclude_replies=false")
        );
    }

    #[tokio::test]
    async fn single_text_posts_and_explicitly_excluded_replies_are_supported() {
        let mut text = status("100");
        text["media_attachments"] = serde_json::json!([]);
        let (root, server) = serve(vec![response("200 OK", "", text.to_string().as_bytes())]);
        let native = mastodon(root).with_selection(SelectionOptions {
            text_posts: true,
            ..SelectionOptions::default()
        });
        let url = Url::parse("https://mastodon.social/@alice/100").unwrap();
        let events = native.events(&url).collect::<Vec<_>>().await;
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], Ok(ExtractionEvent::Post(_))));
        server.join().unwrap();
        text["in_reply_to_id"] = serde_json::json!("99");
        let (root, server) = serve(vec![response("200 OK", "", text.to_string().as_bytes())]);
        let native = mastodon(root).with_selection(SelectionOptions {
            replies: false,
            ..SelectionOptions::default()
        });
        assert!(native.events(&url).collect::<Vec<_>>().await.is_empty());
        server.join().unwrap();
    }

    #[test]
    fn recognizes_collections_numeric_accounts_and_unicode_hashtags() {
        for (path, feed) in [
            ("bookmarks", Collection::Bookmarks),
            ("web/favourites", Collection::Favourites),
            ("lists/42", Collection::List("42".into())),
            ("tags/rust_lang", Collection::Hashtag("rust_lang".into())),
            (
                "tags/%E6%97%A5%E6%9C%AC",
                Collection::Hashtag("日本".into()),
            ),
        ] {
            let url = Url::parse(&format!("https://mastodon.social/{path}")).unwrap();
            assert_eq!(
                target(&url).unwrap(),
                Target::Collection {
                    host: "mastodon.social".into(),
                    feed
                }
            );
        }
        for path in ["@id:42/media", "users/id:42", "web/users/id:42/media"] {
            assert_eq!(
                target(&Url::parse(&format!("https://pawoo.net/{path}")).unwrap()).unwrap(),
                Target::User {
                    host: "pawoo.net".into(),
                    acct: "id:42".into()
                }
            );
        }
        for path in [
            "lists/other",
            "lists/42/extra",
            "tags/a%2Fb",
            "tags/a%26b",
            "tags/%FF",
            "tags/a%3Fb",
            "tags/a+b",
            "@id:invalid",
        ] {
            assert!(
                target(&Url::parse(&format!("https://pawoo.net/{path}")).unwrap()).is_err(),
                "{path}"
            );
        }
    }

    #[tokio::test]
    async fn collection_routes_use_bearer_auth_without_leaking_it_to_media() {
        for (path, endpoint) in [
            ("bookmarks", "bookmarks"),
            ("favourites", "favourites"),
            ("lists/42", "timelines/list/42"),
            ("tags/rust_lang", "timelines/tag/rust_lang"),
        ] {
            let (root, server) = serve(vec![response(
                "200 OK",
                "",
                serde_json::json!([status("100")]).to_string().as_bytes(),
            )]);
            let native = mastodon(root)
                .with_access_token_for("mastodon.social", Some("test-token_123"))
                .unwrap();
            let media = native
                .stream(&Url::parse(&format!("https://mastodon.social/{path}")).unwrap())
                .collect::<Vec<_>>()
                .await;
            assert_eq!(media.len(), 2, "{media:?}");
            for file in media {
                let file = file.unwrap();
                assert!(!file.headers.contains_key("Authorization"));
                assert!(!serde_json::to_string(&file).unwrap().contains("test-token"));
                assert_eq!(
                    file.archive_key(),
                    parse_status(&status("100"), "mastodon.social")
                        .into_iter()
                        .find(|m| m.filename == file.filename)
                        .unwrap()
                        .archive_key()
                );
            }
            let requests = server.join().unwrap();
            assert_eq!(requests.len(), 1);
            assert!(
                requests[0].starts_with(&format!("GET /api/v1/{endpoint}?limit=40 ")),
                "{}",
                requests[0]
            );
            assert!(
                requests[0]
                    .to_ascii_lowercase()
                    .contains("authorization: bearer test-token_123\r\n")
            );
        }
    }

    #[tokio::test]
    async fn bookmarks_follow_bookmark_cursors_and_stop_on_repeated_cursor() {
        let body = |id| serde_json::json!([status(id)]).to_string();
        let (root, server) = serve(vec![
            response(
                "200 OK",
                "Link: <https://mastodon.social/api/v1/bookmarks?max_id=7>; rel=next\r\n",
                body("100").as_bytes(),
            ),
            response(
                "200 OK",
                "Link: <https://mastodon.social/api/v1/bookmarks?max_id=8>; rel=next\r\n",
                body("90").as_bytes(),
            ),
            response(
                "200 OK",
                "Link: <https://mastodon.social/api/v1/bookmarks?max_id=7>; rel=next\r\n",
                body("80").as_bytes(),
            ),
        ]);
        let native = mastodon(root)
            .with_access_token_for("mastodon.social", Some("test-token"))
            .unwrap();
        let media = native
            .stream(&Url::parse("https://mastodon.social/bookmarks").unwrap())
            .collect::<Vec<_>>()
            .await;
        assert_eq!(media.len(), 6, "{media:?}");
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /api/v1/bookmarks?limit=40&max_id=7 "));
        assert!(requests[2].starts_with("GET /api/v1/bookmarks?limit=40&max_id=8 "));
    }

    #[tokio::test]
    async fn numeric_users_skip_lookup_and_collection_cancellation_skips_later_pages() {
        for (path, expected) in [
            ("@id:9/media", "/api/v1/accounts/9/statuses?"),
            ("bookmarks", "/api/v1/bookmarks?"),
        ] {
            let (root, server) = serve(vec![response(
                "200 OK",
                "Link: <https://mastodon.social/?max_id=3>; rel=next\r\n",
                serde_json::json!([status("100")]).to_string().as_bytes(),
            )]);
            let native = mastodon(root);
            let url = Url::parse(&format!("https://mastodon.social/{path}")).unwrap();
            let mut stream = native.events(&url);
            assert!(matches!(
                stream.next().await.unwrap().unwrap(),
                ExtractionEvent::Post(_)
            ));
            drop(stream);
            let requests = server.join().unwrap();
            assert_eq!(requests.len(), 1);
            assert!(requests[0].starts_with(&format!("GET {expected}")));
        }
    }

    #[tokio::test]
    async fn auth_is_removed_from_non_api_redirects_and_failures_are_private() {
        let (root, server) = serve(vec![
            response("302 Found", "Location: /landing\r\n", b""),
            response("200 OK", "", status("100").to_string().as_bytes()),
        ]);
        let native = mastodon(root)
            .with_access_token_for("mastodon.social", Some("private-token"))
            .unwrap();
        assert!(native.access_token.as_ref().unwrap().1.is_sensitive());
        let media = native
            .stream(&Url::parse("https://mastodon.social/@alice/100").unwrap())
            .collect::<Vec<_>>()
            .await;
        assert_eq!(media.len(), 2);
        let requests = server.join().unwrap();
        assert!(
            requests[0]
                .to_ascii_lowercase()
                .contains("authorization: bearer ")
        );
        assert!(!requests[1].to_ascii_lowercase().contains("authorization:"));
        let (root, server) = serve(vec![response(
            "401 Unauthorized",
            "",
            br#"{"error":"private-token"}"#,
        )]);
        let native = mastodon(root)
            .with_access_token_for("mastodon.social", Some("private-token"))
            .unwrap()
            .with_retries(3);
        let error = native
            .stream(&Url::parse("https://mastodon.social/bookmarks").unwrap())
            .next()
            .await
            .unwrap()
            .unwrap_err()
            .to_string();
        assert!(error.contains("access-token"));
        assert!(!error.contains("private-token"));
        assert_eq!(server.join().unwrap().len(), 1);
        for token in ["private token", "private\r\ntoken", "秘密"] {
            let error = mastodon(Url::parse("http://127.0.0.1:9/").unwrap())
                .with_access_token_for("mastodon.social", Some(token))
                .err()
                .unwrap()
                .to_string();
            assert!(!error.contains("private"));
        }
    }

    #[tokio::test]
    async fn unset_or_cache_tokens_come_from_the_credential_cache() {
        let directory = tempfile::tempdir().unwrap();
        let cache = crate::cache::Cache::at(directory.path().join("cache.sqlite3"));
        cache
            .set(
                "gallery_dl.extractor.mastodon._access_token_cache-mastodon.social",
                &serde_json::json!("cached-token"),
                None,
            )
            .await
            .unwrap();
        let api = Url::parse("https://mastodon.social/api/v1/bookmarks").unwrap();
        for setting in [None, Some("cache")] {
            let native = Mastodon::new(Duration::from_secs(1))
                .unwrap()
                .with_access_token_for("mastodon.social", setting)
                .unwrap();
            assert!(native.authorization(&api).is_none());
            let native = native.with_token_cache(Some(&cache)).await;
            let value = native.authorization(&api).unwrap();
            assert!(value.is_sensitive());
            assert_eq!(value.to_str().unwrap(), "Bearer cached-token");
        }
        // Explicit and disabled tokens never consult the cache.
        for (setting, expected) in [(Some("own"), Some("Bearer own")), (Some(""), None)] {
            let native = Mastodon::new(Duration::from_secs(1))
                .unwrap()
                .with_access_token_for("mastodon.social", setting)
                .unwrap()
                .with_token_cache(Some(&cache))
                .await;
            assert_eq!(
                native
                    .authorization(&api)
                    .map(|value| value.to_str().unwrap().to_owned())
                    .as_deref(),
                expected
            );
        }
        // Another instance's cached token is not used.
        let native = Mastodon::new(Duration::from_secs(1))
            .unwrap()
            .with_access_token_for("pawoo.net", None)
            .unwrap()
            .with_token_cache(Some(&cache))
            .await;
        assert!(
            native
                .authorization(&Url::parse("https://pawoo.net/api/v1/bookmarks").unwrap())
                .is_none()
        );
    }

    #[test]
    fn tokens_are_bound_to_an_explicit_instance_origin() {
        let native = Mastodon::new(Duration::from_secs(1))
            .unwrap()
            .with_access_token_for("mastodon.social", Some("test-token"))
            .unwrap();
        assert!(
            native
                .authorization(&Url::parse("https://mastodon.social/api/v1/bookmarks").unwrap())
                .is_some()
        );
        for url in [
            "https://pawoo.net/api/v1/bookmarks",
            "https://baraag.net/api/v1/bookmarks",
            "http://mastodon.social/api/v1/bookmarks",
            "https://mastodon.social:444/api/v1/bookmarks",
            "https://mastodon.social/landing",
        ] {
            assert!(
                native.authorization(&Url::parse(url).unwrap()).is_none(),
                "{url}"
            );
        }
        assert!(
            Mastodon::new(Duration::from_secs(1))
                .unwrap()
                .with_access_token_for("unknown.example", Some("test-token"))
                .is_err()
        );
    }

    #[tokio::test]
    async fn collection_paging_reads_all_link_header_fields() {
        let (root, server) = serve(vec![
            response(
                "200 OK",
                "Link: <https://mastodon.social/api/v1/bookmarks?min_id=100>; rel=prev\r\nLink: <https://mastodon.social/api/v1/bookmarks?max_id=5>; rel=next\r\n",
                serde_json::json!([status("100")]).to_string().as_bytes(),
            ),
            response("200 OK", "", b"[]"),
        ]);
        let media = mastodon(root)
            .stream(&Url::parse("https://mastodon.social/bookmarks").unwrap())
            .collect::<Vec<_>>()
            .await;
        assert_eq!(media.len(), 2);
        assert!(server.join().unwrap()[1].contains("&max_id=5 "));
    }

    #[test]
    fn cards_are_opt_in_and_keep_upstream_thumbnail_archive_identity() {
        let mut post = status("100");
        post["card"] = serde_json::json!({
            "image":"https://files.mastodon.social/cache/preview_cards/images/000/123/456/original/thumb.jpeg",
            "url":"https://article.example/page?private=ignored", "title":"Article", "description":"Preview", "width":400, "height":300
        });
        let options = SelectionOptions {
            cards: true,
            ..SelectionOptions::default()
        };
        assert_eq!(parse_status(&post, "mastodon.social").len(), 2);
        let media = parse_status_with_options(&post, "mastodon.social", options);
        assert_eq!(media.len(), 3);
        let card = &media[2];
        assert_eq!(card.filename, "100_card123456.jpg");
        assert!(card.headers.is_empty());
        let metadata = card.metadata.as_ref().unwrap();
        assert_eq!(metadata.media_id, "card123456");
        assert_eq!(metadata.index, 3);
        let file = metadata.file.as_ref().unwrap();
        assert_eq!(file.width, Some(400));
        assert_eq!(file.height, Some(300));
        assert_eq!(file.description.as_deref(), Some("Preview"));
        assert!(
            !serde_json::to_string(card)
                .unwrap()
                .contains("private=ignored")
        );
        let mut other = post.clone();
        other["id"] = serde_json::json!("200");
        assert_eq!(
            card.archive_key(),
            parse_status_with_options(&other, "mastodon.social", options)[2].archive_key()
        );
        assert_ne!(
            card.archive_key(),
            parse_status_with_options(&post, "pawoo.net", options)[2].archive_key()
        );
        post["card"]["image"] = serde_json::json!("https://cdn.example/thumbnail.jpg");
        let unknown = parse_status_with_options(&post, "mastodon.social", options);
        assert_eq!(unknown.len(), 3);
        assert!(unknown[2].archive_key().is_none());
        for raw in [
            "http://cdn.example/image.jpg",
            "https://user:pass@cdn.example/image.jpg",
            "https://cdn.example:444/image.jpg",
        ] {
            post["card"]["image"] = serde_json::json!(raw);
            assert_eq!(
                parse_status_with_options(&post, "mastodon.social", options).len(),
                2
            );
        }
    }

    #[tokio::test]
    async fn card_only_timelines_disable_media_only_queries() {
        let mut post = status("100");
        post["media_attachments"] = serde_json::json!([]);
        post["card"] = serde_json::json!({"image":"https://files.mastodon.social/cache/preview_cards/images/000/123/456/original/thumb.jpg"});
        let (root, server) = serve(vec![
            response(
                "200 OK",
                "",
                serde_json::json!([post]).to_string().as_bytes(),
            ),
            response("200 OK", "", b"[]"),
        ]);
        let native = mastodon(root).with_selection(SelectionOptions {
            cards: true,
            ..SelectionOptions::default()
        });
        let media = native
            .stream(&Url::parse("https://mastodon.social/@id:9").unwrap())
            .collect::<Vec<_>>()
            .await;
        assert_eq!(media.len(), 1);
        assert_eq!(
            media[0]
                .as_ref()
                .unwrap()
                .metadata
                .as_ref()
                .unwrap()
                .media_id,
            "card123456"
        );
        assert!(server.join().unwrap()[0].contains("only_media=false"));
    }

    #[test]
    fn strips_html() {
        assert_eq!(strip_html("<p>a &lt;b&gt;</p>"), "a <b>");
        assert_eq!(strip_html("plain"), "plain");
        assert_eq!(strip_html("broken <tag"), "broken");
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

    fn mastodon(root: Url) -> Mastodon {
        Mastodon::new(Duration::from_secs(5))
            .unwrap()
            .with_network(&NetworkOptions::new(Some(String::new()), None).unwrap())
            .unwrap()
            .with_api_root(root)
    }

    #[tokio::test]
    async fn fetches_status_with_retries() {
        let (root, server) = serve(vec![
            response("429 Too Many Requests", "Retry-After: 0\r\n", b""),
            response("200 OK", "", status("100").to_string().as_bytes()),
        ]);
        let events: Vec<_> = mastodon(root)
            .with_retries(1)
            .events(&Url::parse("https://mastodon.social/@alice/100").unwrap())
            .collect()
            .await;
        assert_eq!(events.len(), 4, "{events:?}");
        assert!(matches!(events[1], Ok(ExtractionEvent::Media(_))));
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /api/v1/statuses/100 "));
        assert!(
            requests[1]
                .to_ascii_lowercase()
                .contains("user-agent: gallery-dl-rs/")
        );
    }

    #[tokio::test]
    async fn looks_up_account_and_paginates_timeline() {
        let link = "Link: <https://mastodon.social/api/v1/accounts/9/statuses?max_id=30>; rel=\"next\"\r\n";
        let (root, server) = serve(vec![
            response("200 OK", "", br#"{"id": "9", "acct": "alice"}"#),
            response("200 OK", link, json!([status("31")]).to_string().as_bytes()),
            response("200 OK", "", b"[]"),
        ]);
        let media: Vec<_> = mastodon(root)
            .stream(&Url::parse("https://mastodon.social/@alice/media").unwrap())
            .collect()
            .await;
        assert_eq!(media.len(), 2, "{media:?}");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/v1/accounts/lookup?acct=alice "));
        assert!(requests[1].starts_with(
            "GET /api/v1/accounts/9/statuses?only_media=true&exclude_reblogs=true&exclude_replies=false&limit=40 "
        ));
        assert!(requests[2].contains("&max_id=30 "));
    }

    #[tokio::test]
    async fn terminal_errors_are_not_retried() {
        let (root, server) = serve(vec![response("404 Not Found", "", b"{}")]);
        let error = mastodon(root)
            .with_retries(3)
            .stream(&Url::parse("https://pawoo.net/@nobody/123").unwrap())
            .collect::<Vec<_>>()
            .await
            .remove(0)
            .unwrap_err();
        assert!(error.to_string().contains("not found"));
        assert!(!error.to_string().contains("127.0.0.1"));
        assert_eq!(server.join().unwrap().len(), 1);
    }

    /// Upstream following, object and account-search parity.
    /// User-configured (`extractor.mastodon.NAME`) and `mastodon:URL`
    /// instances. The instance registry is process-wide: unique hosts only.
    mod configured {
        use super::*;
        use crate::base_instances::{Base, Definition, register};

        fn define(category: &str, value: Value) {
            register(&[Definition::parse(Base::Mastodon, category, &value)
                .unwrap()
                .unwrap()]);
        }

        #[test]
        fn configured_instances_route_with_their_root_category_and_application() {
            define(
                "cfgdon",
                json!({"root": "https://cfgdon.test",
                       "pattern": r"(?:www\.)?cfgdon\.(?:test|example)",
                       "client-id": "cid", "client-secret": "csecret"}),
            );
            for raw in [
                "https://cfgdon.test/@alice",
                "http://www.cfgdon.example/web/@alice",
            ] {
                let url = Url::parse(raw).unwrap();
                // The instance is the configured root's host, as upstream.
                assert_eq!(
                    target(&url).unwrap(),
                    Target::User {
                        host: "cfgdon.test".into(),
                        acct: "alice".into()
                    }
                );
                assert_eq!(crate::extract::social_site(&url), Some("mastodon"));
                let descriptor = crate::source_archive::for_url(&url).unwrap();
                assert_eq!(descriptor.category, "cfgdon");
                assert_eq!(descriptor.format, "{media[id]}");
            }
            assert_eq!(instance_category("cfgdon.test"), Some("cfgdon"));
            assert!(is_instance_category("cfgdon") && !is_instance_category("cfgdon.test"));
            assert_eq!(application("cfgdon.test"), Some(("cid", "csecret")));
            assert!(target(&Url::parse("https://other-cfgdon.test/@alice").unwrap()).is_err());
            // Output uses the instance category; archive keys its root host.
            let media = parse_status(&status("100"), "cfgdon.test");
            assert_eq!(media[0].category, "cfgdon");
            let builtin = parse_status(&status("100"), "mastodon.social");
            assert!(media[0].archive_key().is_some());
            assert_ne!(media[0].archive_key(), builtin[0].archive_key());
            assert_eq!(
                crate::source_archive::for_media(&media[0]).unwrap().format,
                "{media[id]}"
            );
        }

        #[tokio::test]
        async fn tokens_come_from_the_setting_then_the_cache_then_the_instance() {
            define(
                "tokdon",
                json!({"root": "https://tokdon.test", "access-token": "instance-token"}),
            );
            let directory = tempfile::tempdir().unwrap();
            let cache = crate::cache::Cache::at(directory.path().join("cache.sqlite3"));
            let api = Url::parse("https://tokdon.test/api/v1/bookmarks").unwrap();
            let token = |native: &Mastodon| {
                native
                    .authorization(&api)
                    .map(|value| value.to_str().unwrap().to_owned())
            };
            for setting in [None, Some("cache"), Some("")] {
                let native = Mastodon::new(Duration::from_secs(1))
                    .unwrap()
                    .with_access_token_for("tokdon.test", setting)
                    .unwrap()
                    .with_token_cache(Some(&cache))
                    .await;
                assert_eq!(token(&native).as_deref(), Some("Bearer instance-token"));
            }
            // The token `oauth:mastodon:tokdon.test` cached is per instance.
            let key = crate::cache::key("extractor.mastodon", "_access_token_cache", "tokdon.test");
            cache
                .set(&key, &serde_json::json!("cached-token"), None)
                .await
                .unwrap();
            let native = Mastodon::new(Duration::from_secs(1))
                .unwrap()
                .with_access_token_for("tokdon.test", None)
                .unwrap()
                .with_token_cache(Some(&cache))
                .await;
            assert_eq!(token(&native).as_deref(), Some("Bearer cached-token"));
            let native = Mastodon::new(Duration::from_secs(1))
                .unwrap()
                .with_access_token_for("tokdon.test", Some("own"))
                .unwrap()
                .with_token_cache(Some(&cache))
                .await;
            assert_eq!(token(&native).as_deref(), Some("Bearer own"));
            // Tokens stay with their instance origin.
            for other in [
                "https://mastodon.social/api/v1/bookmarks",
                "http://tokdon.test/api/v1/bookmarks",
                "https://tokdon.test/landing",
            ] {
                assert!(
                    native.authorization(&Url::parse(other).unwrap()).is_none(),
                    "{other}"
                );
            }
            // Another instance never receives this instance's token.
            let native = Mastodon::new(Duration::from_secs(1))
                .unwrap()
                .with_access_token_for("mastodon.social", None)
                .unwrap()
                .with_token_cache(Some(&cache))
                .await;
            assert!(
                native
                    .authorization(&Url::parse("https://mastodon.social/api/v1/x").unwrap())
                    .is_none()
            );
        }

        #[tokio::test]
        async fn configured_instances_request_their_configured_root() {
            let (root, server) = serve(vec![response(
                "200 OK",
                "",
                status("100").to_string().as_bytes(),
            )]);
            // `localhost` keeps this registration away from other tests'
            // 127.0.0.1 servers that may later reuse the port.
            let mut root = root;
            root.set_host(Some("localhost")).unwrap();
            define(
                "rootdon",
                json!({"root": root.as_str(), "access-token": "root-token"}),
            );
            let netloc = format!("localhost:{}", root.port().unwrap());
            let native = Mastodon::new(Duration::from_secs(5))
                .unwrap()
                .with_network(&NetworkOptions::new(Some(String::new()), None).unwrap())
                .unwrap()
                .with_access_token_for(&netloc, None)
                .unwrap()
                .with_token_cache(None)
                .await;
            let media: Vec<_> = native
                .stream(&root.join("@alice/100").unwrap())
                .collect()
                .await;
            assert_eq!(media.len(), 2, "{media:?}");
            let file = media[0].as_ref().unwrap();
            assert_eq!(file.category, "rootdon");
            assert_eq!(
                file.metadata.as_ref().unwrap().instance.as_deref(),
                Some(netloc.as_str())
            );
            let requests = server.join().unwrap();
            assert!(requests[0].starts_with("GET /api/v1/statuses/100 "));
            assert!(
                requests[0]
                    .to_ascii_lowercase()
                    .contains("authorization: bearer root-token\r\n")
            );
        }

        #[test]
        fn prefixed_inputs_use_the_mastodon_family_for_any_host() {
            let input = crate::extract::input_url("mastodon:https://predon.test/@bob/7").unwrap();
            assert_eq!(crate::extract::social_site(&input), Some("mastodon"));
            let url = crate::base_instances::unprefix(input).unwrap();
            assert_eq!(
                target(&url).unwrap(),
                Target::Status {
                    host: "predon.test".into(),
                    id: "7".into()
                }
            );
            assert_eq!(instance_category("predon.test"), Some("predon.test"));
            assert_eq!(application("predon.test"), None);
        }
    }

    mod parity {
        use super::*;
        use std::sync::Mutex;

        fn account(id: &str, acct: &str, url: &str) -> Value {
            serde_json::json!({"id": id, "username": acct.split('@').next().unwrap(),
                "acct": acct, "display_name": "Name", "url": url, "note": "<p>bio</p>"})
        }

        #[test]
        fn classifies_following_and_object_urls() {
            let t = |raw: &str| target(&Url::parse(raw).unwrap());
            for raw in [
                "https://mastodon.social/@alice/following",
                "https://mastodon.social/users/alice/following",
                "https://mastodon.social/web/@alice/following",
            ] {
                assert_eq!(
                    t(raw).unwrap(),
                    Target::Following {
                        host: "mastodon.social".into(),
                        acct: "alice".into()
                    },
                    "{raw}"
                );
            }
            assert_eq!(
                t("https://baraag.net/@bob@remote.example/following").unwrap(),
                Target::Following {
                    host: "baraag.net".into(),
                    acct: "bob@remote.example".into()
                }
            );
            for raw in [
                "https://pawoo.net/objects/9f1c-AB_2",
                "https://pawoo.net/users/alice/objects/9f1c-AB_2",
            ] {
                assert_eq!(
                    t(raw).unwrap(),
                    Target::Object {
                        host: "pawoo.net".into(),
                        id: "9f1c-AB_2".into()
                    },
                    "{raw}"
                );
            }
            assert_eq!(
                t("https://pawoo.net/users/alice/notice/12").unwrap(),
                Target::Status {
                    host: "pawoo.net".into(),
                    id: "12".into()
                }
            );
            for raw in [
                "https://pawoo.net/objects/a.b",
                "https://pawoo.net/objects/a%2Fb",
                "https://pawoo.net/@alice/following/extra",
                "https://pawoo.net/@/following",
            ] {
                assert!(t(raw).is_err(), "{raw}");
            }
        }

        #[tokio::test]
        async fn following_queues_supported_user_urls_lazily_as_child_jobs() {
            let (root, server) = serve(vec![
                response("200 OK", "", br#"{"id":"9","acct":"alice"}"#),
                response(
                    "200 OK",
                    "Link: <https://mastodon.social/api/v1/accounts/9/following?max_id=77>; rel=\"next\"\r\n",
                    serde_json::json!([
                        account("1", "bob", "https://mastodon.social/@bob"),
                        account("2", "carol@remote.example", "https://remote.example/@carol"),
                        account("3", "dave@pawoo.net", "https://pawoo.net/@dave"),
                        {"id": "4", "acct": "broken", "url": "not a url"},
                    ])
                    .to_string()
                    .as_bytes(),
                ),
                response("200 OK", "", b"[]"),
            ]);
            let queued = Arc::new(Mutex::new(Vec::new()));
            let seen = queued.clone();
            let native = mastodon(root);
            let queue: crate::event::Queue<'_> = Arc::new(move |url: Url| {
                seen.lock().unwrap().push(url.to_string());
                Some(stream::empty().boxed())
            });
            let events: Vec<_> = native
                .events_with_queue(
                    &Url::parse("https://mastodon.social/@alice/following").unwrap(),
                    queue,
                )
                .collect()
                .await;
            assert!(events.iter().all(Result::is_ok), "{events:?}");
            let children: Vec<_> = events
                .iter()
                .filter_map(|event| match event {
                    Ok(ExtractionEvent::BeginChild(scope)) => Some(scope),
                    _ => None,
                })
                .collect();
            assert_eq!(children.len(), 2);
            assert_eq!(
                *queued.lock().unwrap(),
                ["https://mastodon.social/@bob", "https://pawoo.net/@dave"]
            );
            let requests = server.join().unwrap();
            assert!(requests[0].starts_with("GET /api/v1/accounts/lookup?acct=alice "));
            assert!(requests[1].starts_with("GET /api/v1/accounts/9/following?limit=40 "));
            assert!(
                requests[2].starts_with("GET /api/v1/accounts/9/following?limit=40&max_id=77 ")
            );
            // Without a queue nothing is followed, but the listing still pages.
            let (root, server) = serve(vec![response(
                "200 OK",
                "",
                serde_json::json!([account("1", "bob", "https://mastodon.social/@bob")])
                    .to_string()
                    .as_bytes(),
            )]);
            let events: Vec<_> = mastodon(root)
                .events(&Url::parse("https://mastodon.social/@id:9/following").unwrap())
                .collect()
                .await;
            assert!(events.is_empty(), "{events:?}");
            assert!(server.join().unwrap()[0].starts_with("GET /api/v1/accounts/9/following?"));
        }

        #[tokio::test]
        async fn failed_lookups_fall_back_to_exact_account_search() {
            let (root, server) = serve(vec![
                response("404 Not Found", "", b"{}"),
                response(
                    "200 OK",
                    "",
                    serde_json::json!([account("5", "alice", "https://mastodon.social/@alice")])
                        .to_string()
                        .as_bytes(),
                ),
                response(
                    "200 OK",
                    "",
                    serde_json::json!([status("100")]).to_string().as_bytes(),
                ),
                response("200 OK", "", b"[]"),
            ]);
            let media: Vec<_> = mastodon(root)
                .stream(&Url::parse("https://mastodon.social/@alice").unwrap())
                .collect()
                .await;
            assert_eq!(media.len(), 2, "{media:?}");
            let requests = server.join().unwrap();
            assert!(
                requests[1].starts_with(
                    "GET /api/v1/accounts/search?q=%40alice%40mastodon.social&limit=1 "
                )
            );
            assert!(requests[2].starts_with("GET /api/v1/accounts/5/statuses?"));
            // Remote accounts search their own handle; inexact matches fail.
            let (root, server) = serve(vec![
                response("404 Not Found", "", b"{}"),
                response(
                    "200 OK",
                    "",
                    serde_json::json!([account("6", "bobby@remote.example", "https://x/@b")])
                        .to_string()
                        .as_bytes(),
                ),
            ]);
            let error = mastodon(root)
                .stream(&Url::parse("https://mastodon.social/@bob@remote.example").unwrap())
                .collect::<Vec<_>>()
                .await
                .remove(0)
                .unwrap_err()
                .to_string();
            assert!(error.contains("account not found"), "{error}");
            let requests = server.join().unwrap();
            assert!(requests[1].contains("q=%40bob%40remote.example&limit=1 "));
        }

        #[tokio::test]
        async fn objects_resolve_their_status_through_the_redirect_location() {
            let (root, server) = serve(vec![
                response(
                    "302 Found",
                    "Location: https://pawoo.net/@alice/100\r\n",
                    b"",
                ),
                response("200 OK", "", status("100").to_string().as_bytes()),
            ]);
            let media: Vec<_> = mastodon(root)
                .with_access_token_for("pawoo.net", Some("test-token"))
                .unwrap()
                .stream(&Url::parse("https://pawoo.net/objects/9f1c-ab").unwrap())
                .collect()
                .await;
            assert_eq!(media.len(), 2, "{media:?}");
            let requests = server.join().unwrap();
            assert!(requests[0].starts_with("GET /objects/9f1c-ab "));
            assert!(!requests[0].to_ascii_lowercase().contains("authorization:"));
            assert!(requests[1].starts_with("GET /api/v1/statuses/100 "));
            for reply in [
                response("200 OK", "", b"{}"),
                response("302 Found", "Location: /about\r\n", b""),
                response("404 Not Found", "", b""),
            ] {
                let (root, server) = serve(vec![reply]);
                let error = mastodon(root)
                    .stream(&Url::parse("https://pawoo.net/objects/x").unwrap())
                    .collect::<Vec<_>>()
                    .await
                    .remove(0)
                    .unwrap_err()
                    .to_string();
                assert!(error.contains("object"), "{error}");
                assert_eq!(server.join().unwrap().len(), 1);
            }
        }
    }
}
