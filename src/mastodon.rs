//! Experimental native Mastodon status and user media extraction.
//!
//! Mirrors gallery_dl/extractor/mastodon.py for the instances upstream ships
//! (mastodon.social, pawoo.net, baraag.net) using the public REST API. All
//! instances use their upstream categories; the instance host is kept
//! in the `instance` metadata field and the archive key. Media attachments are
//! served from instance-chosen CDNs, so any HTTPS host is accepted for files;
//! no API headers or cookies are attached to media requests.
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

/// Upstream category for each supported instance (also the credential scope).
pub fn instance_category(host: &str) -> Option<&'static str> {
    match host {
        "mastodon.social" => Some("mastodon.social"),
        "pawoo.net" => Some("pawoo"),
        "baraag.net" => Some("baraag"),
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
    User {
        host: String,
        acct: String,
    },
    Collection {
        host: String,
        feed: Collection,
    },
    /// Accounts a user follows, queued as user jobs.
    Following {
        host: String,
        acct: String,
    },
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
/// Mastodon snowflakes are numeric; Pleroma/Akkoma flake IDs are
/// alphanumeric (upstream accepts any path segment).
pub fn valid_id(id: &str) -> bool {
    (1..=32).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// Preview-card identities follow upstream's instance-owned thumbnail IDs.
pub fn valid_media_id(id: &str) -> bool {
    match id.strip_prefix("card") {
        Some(rest) => (1..=24).contains(&rest.len()) && rest.bytes().all(|b| b.is_ascii_digit()),
        None => valid_id(id),
    }
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

impl Target {
    /// The instance host the target lives on.
    pub fn host(&self) -> &str {
        match self {
            Self::Status { host, .. }
            | Self::User { host, .. }
            | Self::Collection { host, .. }
            | Self::Following { host, .. } => host,
        }
    }
}

/// A page URL on `host`; other instances than the built-in ones keep
/// upstream's `mastodon:` prefix.
fn instance_url(host: &str, path: &str) -> Option<Url> {
    if INSTANCES.contains(&host) {
        Url::parse(&format!("https://{host}{path}")).ok()
    } else {
        Url::parse(&format!("mastodon:https://{host}{path}")).ok()
    }
}

/// Upstream category of a Mastodon target URL: the built-in instance's
/// category, else `mastodon` for `mastodon:https://HOST/...` instances.
pub fn url_category(url: &Url) -> Option<&'static str> {
    let target = target(url).ok()?;
    Some(instance_category(target.host()).unwrap_or("mastodon"))
}

pub fn target(url: &Url) -> Result<Target> {
    // `mastodon:https://HOST/...` selects any other instance.
    if let Some(inner) = crate::sites::custom_domain(url, "mastodon") {
        return target_on(&inner, true);
    }
    target_on(url, false)
}

fn target_on(url: &Url, custom: bool) -> Result<Target> {
    let unsupported = || {
        error(
            "unsupported Mastodon URL; expected a status, user, bookmarks, favourites, list or hashtag link on mastodon.social, pawoo.net or baraag.net",
        )
    };
    let host = url.host_str().ok_or_else(unsupported)?;
    if !matches!(url.scheme(), "http" | "https")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || !(custom || INSTANCES.contains(&host))
    {
        return Err(unsupported());
    }
    let segments: Vec<_> = url
        .path_segments()
        .map(|segments| segments.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    let segments = segments.strip_prefix(&["web"]).unwrap_or(&segments);
    let host = host.to_owned();
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
    let following = |name: &str| match user(name)? {
        Target::User { host, acct } => Ok(Target::Following { host, acct }),
        _ => Err(unsupported()),
    };
    let collection = |feed| {
        Ok(Target::Collection {
            host: host.clone(),
            feed,
        })
    };
    match segments {
        ["statuses", id] | ["notice", id] | ["users", _, "statuses", id] => status(id),
        ["users", name, "following"] => following(name),
        [name, "following"] if name.starts_with('@') => following(name),
        ["users", name] | ["users", name, "media"] => user(name),
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
        })
    }

    /// Explicit bearer credentials are sent only to this extractor's API origin.
    pub fn with_access_token_for(mut self, instance: &str, token: Option<&str>) -> Result<Self> {
        if instance_category(instance).is_none() {
            return Err(error("unsupported Mastodon credential instance"));
        }
        self.access_token = match token.filter(|token| !token.is_empty()) {
            None => None,
            Some(token)
                if token != "cache"
                    && token.len() <= 4096
                    && token
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-._~+/=".contains(&b)) =>
            {
                let mut value = header::HeaderValue::from_str(&format!("Bearer {token}"))
                    .map_err(|_| error("invalid Mastodon access token"))?;
                value.set_sensitive(true);
                Some((instance.into(), value))
            }
            Some(_) => {
                return Err(error(
                    "invalid Mastodon access token; OAuth cache discovery is not implemented",
                ));
            }
        };
        Ok(self)
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
        self.api_root
            .clone()
            .unwrap_or_else(|| Url::parse(&format!("https://{host}/")).expect("known instance"))
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

    /// Emit one post boundary per status; timelines paginate lazily.
    pub fn events(&self, url: &Url) -> BoxStream<'_, Result<ExtractionEvent>> {
        let target = match target(url) {
            Ok(target) => target,
            Err(e) => return stream::once(async move { Err(e) }).boxed(),
        };
        match target {
            Target::Following { host, acct } => self.following(host, acct),
            Target::Status { host, id } => stream::once(async move {
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
            .boxed(),
            target => stream::unfold(Some((target, None)), move |state| async move {
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
            .boxed(),
        }
    }

    /// An account's ID: `id:ID`, else `accounts/lookup`.
    async fn account_id(&self, host: &str, acct: &str) -> Result<String> {
        if let Some(id) = acct.strip_prefix("id:") {
            return Ok(id.to_owned());
        }
        let (body, _) = self
            .call(
                &self.root(host),
                "api/v1/accounts/lookup",
                &[("acct", acct)],
            )
            .await?;
        let account = json(&body)?;
        Ok(id_of(&account)
            .ok_or_else(|| error("Mastodon account lookup returned no account ID"))?
            .to_owned())
    }

    /// Upstream `MastodonFollowingExtractor`: queue each followed account as
    /// a user job on this instance (`@user@domain` for remote accounts).
    fn following(&self, host: String, acct: String) -> BoxStream<'_, Result<ExtractionEvent>> {
        struct Paging {
            host: String,
            path: String,
            max_id: Option<String>,
            seen: HashSet<String>,
            pages: usize,
            done: bool,
        }
        stream::once(async move {
            let id = self.account_id(&host, &acct).await?;
            Ok::<_, crate::Error>(Paging {
                path: format!("api/v1/accounts/{id}/following"),
                host,
                max_id: None,
                seen: HashSet::new(),
                pages: 0,
                done: false,
            })
        })
        .map(move |paging| match paging {
            Err(e) => stream::once(async move { Err(e) }).boxed(),
            Ok(paging) => stream::unfold(paging, move |mut paging| async move {
                if paging.done {
                    return None;
                }
                let mut query = vec![("limit", PAGE_LIMIT)];
                if let Some(max_id) = &paging.max_id {
                    query.push(("max_id", max_id.as_str()));
                }
                let page = self
                    .call(&self.root(&paging.host), &paging.path, &query)
                    .await;
                let (body, link) = match page {
                    Ok(page) => page,
                    Err(e) => {
                        paging.done = true;
                        return Some((stream::once(async move { Err(e) }).boxed(), paging));
                    }
                };
                let accounts = json(&body)
                    .ok()
                    .and_then(|v| v.as_array().cloned())
                    .unwrap_or_default();
                paging.pages += 1;
                let next = link.as_deref().and_then(next_max_id);
                paging.done = next
                    .as_ref()
                    .is_none_or(|cursor| !paging.seen.insert(cursor.clone()))
                    || paging.pages >= MAX_PAGES;
                paging.max_id = next;
                let host = paging.host.clone();
                let children: Vec<_> = accounts
                    .into_iter()
                    .filter_map(|account| {
                        let acct = string(&account, "acct")?.to_owned();
                        valid_acct(&acct).then_some(())?;
                        let url = instance_url(&host, &format!("/@{acct}"))?;
                        let data = account.as_object()?.clone();
                        Some(crate::event::child_job(
                            crate::event::ChildMetadata::from_raw("mastodon", "following", &data),
                            move || self.events(&url),
                        ))
                    })
                    .collect();
                Some((stream::iter(children).flatten().boxed(), paging))
            })
            .flatten()
            .boxed(),
        })
        .flatten()
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
            Target::Status { .. } | Target::Following { .. } => {
                unreachable!("statuses and followings do not have timeline state")
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
    fn following_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap()).unwrap();
        let expected = Target::Following {
            host: "baraag.net".into(),
            acct: "pumpkinnsfw".into(),
        };
        assert_eq!(
            t("https://baraag.net/users/pumpkinnsfw/following"),
            expected
        );
        assert_eq!(t("https://baraag.net/@pumpkinnsfw/following"), expected);
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
        assert_eq!(next_max_id("<https://x/?max_id=a.b>; rel=\"next\""), None);
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
            "https://mastodon.social/@alice/a-b",
            "https://mastodon.social/@ali%2Fce",
            "https://mastodon.social/@alice@",
            "https://mastodon.social:444/@alice",
            "https://mastodon.social.evil.example/@alice",
            "https://files.mastodon.social/@alice",
            "https://example.social/@alice",
        ] {
            assert!(t(raw).is_err(), "{raw}");
        }
        // `mastodon:` selects other instances, including Pleroma flake IDs.
        assert_eq!(
            t("mastodon:https://donotsta.re/@elly/AcoUaA7EH1igiYKmFU").unwrap(),
            status("donotsta.re", "AcoUaA7EH1igiYKmFU")
        );
        assert_eq!(
            t("mastodon:https://example.social/@alice").unwrap(),
            user("example.social", "alice")
        );
        let custom = Url::parse("mastodon:https://example.social/@alice").unwrap();
        assert_eq!(url_category(&custom), Some("mastodon"));
        assert_eq!(
            url_category(&Url::parse("https://pawoo.net/@alice").unwrap()),
            Some("pawoo")
        );
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
            "lists/o-ther",
            "lists/42/extra",
            "tags/a%2Fb",
            "tags/a%26b",
            "tags/%FF",
            "tags/a%3Fb",
            "tags/a+b",
            "@id:in-valid",
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
        for token in ["cache", "private token", "private\r\ntoken", "秘密"] {
            let error = mastodon(Url::parse("http://127.0.0.1:9/").unwrap())
                .with_access_token_for("mastodon.social", Some(token))
                .err()
                .unwrap()
                .to_string();
            assert!(!error.contains("private"));
        }
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
            .stream(&Url::parse("https://pawoo.net/@nobody").unwrap())
            .collect::<Vec<_>>()
            .await
            .remove(0)
            .unwrap_err();
        assert!(error.to_string().contains("not found"));
        assert!(!error.to_string().contains("127.0.0.1"));
        assert_eq!(server.join().unwrap().len(), 1);
    }
}
