//! Experimental native Imgur image, album, gallery and listing extraction.
//!
//! Mirrors gallery_dl/extractor/imgur.py: posts are read from Imgur's public
//! `post/v1` API with the anonymous web client ID, and file URLs are rebuilt
//! as `https://i.imgur.com/{id}.{ext}` from validated identifiers only.
//! User submissions, favorites, favorite folders, subreddits, tags and
//! searches page through the `3` API lazily and queue each post as an album
//! or image child job, as upstream.
use crate::{
    Result, error,
    event::{ChildMetadata, ExtractionEvent, child_job, group},
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
use serde_json::{Map, Value};
use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, SystemTime},
};
use url::Url;

const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
const CLIENT_ID: &str = "546c25a59c58ad7";
const API_ROOT: &str = "https://api.imgur.com/";
const MAX_RESPONSE: usize = 16 * 1024 * 1024;
const MAX_PAGES: u64 = 200;

/// Upstream extractor classes supported natively, for `--list-extractors`.
pub const SUBCATEGORIES: &[&str] = &[
    "album",
    "favorite",
    "favorite-folder",
    "gallery",
    "image",
    "search",
    "subreddit",
    "tag",
    "user",
];

/// A supported Imgur input: a post holding a validated 5- or 7-character ID,
/// or one of upstream's queue-only listings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Image(String),
    Album(String),
    Gallery(String),
    /// A user's submissions (upstream `user`).
    User(String),
    /// A user's gallery favorites (upstream `favorite`).
    Favorite(String),
    /// A user's favorites folder (upstream `favorite-folder`).
    FavoriteFolder(String, String),
    Subreddit(String),
    Tag(String),
    /// A gallery search for the decoded query (upstream `search`).
    Search(String),
}

impl Target {
    /// The post ID of image, album and gallery targets; empty for listings.
    pub fn id(&self) -> &str {
        match self {
            Self::Image(id) | Self::Album(id) | Self::Gallery(id) => id,
            _ => "",
        }
    }

    /// Upstream subcategory of the target's extractor class.
    pub fn subcategory(&self) -> &'static str {
        match self {
            Self::Image(_) => "image",
            Self::Album(_) => "album",
            Self::Gallery(_) => "gallery",
            Self::User(_) => "user",
            Self::Favorite(_) => "favorite",
            Self::FavoriteFolder(..) => "favorite-folder",
            Self::Subreddit(_) => "subreddit",
            Self::Tag(_) => "tag",
            Self::Search(_) => "search",
        }
    }
}

/// Account names, subreddits and tags.
fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

fn valid_id(id: &str) -> bool {
    matches!(id.len(), 5 | 7) && id.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

/// Strip the optional `title-` slug prefix used by album and gallery URLs.
fn slug_id(segment: &str) -> Option<String> {
    let id = segment.rsplit('-').next()?;
    let slug = &segment[..segment.len() - id.len()];
    (valid_id(id)
        && slug
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')))
    .then(|| id.to_owned())
}

/// Classify an Imgur URL. Personal (`/user/me`) and other listings are
/// rejected explicitly rather than guessed.
pub fn target(url: &Url) -> Result<Target> {
    let unsupported = || {
        error(
            "unsupported Imgur URL; expected an image, album (/a/ID), gallery (/gallery/ID), /user/NAME[/favorites[/folder/ID]], /r/SUBREDDIT, /t/TAG or /search?q=QUERY link",
        )
    };
    let host = url.host_str().ok_or_else(unsupported)?;
    if !matches!(url.scheme(), "http" | "https")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || !matches!(
            host,
            "imgur.com"
                | "www.imgur.com"
                | "m.imgur.com"
                | "i.imgur.com"
                | "imgur.io"
                | "www.imgur.io"
                | "i.imgur.io"
        )
    {
        return Err(unsupported());
    }
    let segments: Vec<_> = url
        .path_segments()
        .map(|segments| segments.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    let direct = host.starts_with("i.");
    match segments.as_slice() {
        [file] if direct => {
            let stem = file.split_once('.').map_or(*file, |(stem, _)| stem);
            // Thumbnail suffixes (s, b, t, m, l, h) follow a 7-character ID.
            let id = if stem.len() == 8 && b"sbtmlh".contains(&stem.as_bytes()[7]) {
                &stem[..7]
            } else {
                stem
            };
            valid_id(id)
                .then(|| Target::Image(id.into()))
                .ok_or_else(unsupported)
        }
        _ if direct => Err(unsupported()),
        ["user", "me", ..] => Err(unsupported()),
        ["user", name] | ["user", name, "posts" | "submitted"]
            if valid_name(name) && url.query().is_none() =>
        {
            Ok(Target::User((*name).into()))
        }
        ["user", name, "favorites"] if valid_name(name) && url.query().is_none() => {
            Ok(Target::Favorite((*name).into()))
        }
        ["user", name, "favorites", "folder", folder, ..]
            if valid_name(name)
                && (1..=20).contains(&folder.len())
                && folder.bytes().all(|b| b.is_ascii_digit()) =>
        {
            Ok(Target::FavoriteFolder((*name).into(), (*folder).into()))
        }
        ["r", subreddit] if valid_name(subreddit) && url.query().is_none() => {
            Ok(Target::Subreddit((*subreddit).into()))
        }
        ["t", tag] if valid_name(tag) && url.query().is_none() && url.fragment().is_none() => {
            Ok(Target::Tag((*tag).into()))
        }
        ["search", ..] => {
            // Upstream requires `q` to be the first query parameter.
            let raw = url
                .query()
                .and_then(|q| q.strip_prefix("q="))
                .map(|q| q.split('&').next().unwrap_or_default())
                .filter(|q| !q.is_empty())
                .ok_or_else(unsupported)?;
            let query = crate::sites::percent_decode(&raw.replace('+', " "));
            (query.len() <= 1024)
                .then_some(Target::Search(query))
                .ok_or_else(unsupported)
        }
        ["a", segment] => slug_id(segment).map(Target::Album).ok_or_else(unsupported),
        ["gallery", segment] | ["t", _, segment] => slug_id(segment)
            .map(Target::Gallery)
            .ok_or_else(unsupported),
        ["r", subreddit, id]
            if subreddit
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
                && valid_id(id) =>
        {
            Ok(Target::Image((*id).into()))
        }
        [segment] => {
            let id = segment.split_once('.').map_or(*segment, |(stem, _)| stem);
            valid_id(id)
                .then(|| Target::Image(id.into()))
                .ok_or_else(unsupported)
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

/// Parse a `post/v1/media` or `post/v1/albums` response. `requested_id`
/// must match the response so a mismatched post is never misattributed.
pub fn parse_post(bytes: &[u8], requested_id: &str) -> Result<Vec<Media>> {
    parse_post_with_mp4(bytes, requested_id, true)
}

fn parse_post_with_mp4(bytes: &[u8], requested_id: &str, mp4: bool) -> Result<Vec<Media>> {
    let post: Value =
        serde_json::from_slice(bytes).map_err(|_| error("Imgur returned invalid JSON"))?;
    if post.get("errors").is_some() {
        return Err(error("Imgur post unavailable"));
    }
    if string(&post, "id") != Some(requested_id) {
        return Err(error("Imgur response does not match the requested post"));
    }
    let items = post
        .get("media")
        .and_then(Value::as_array)
        .ok_or_else(|| error("Imgur response is missing media"))?;
    let username: Option<Arc<str>> = post
        .pointer("/account/username")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(Arc::from);
    let post_caption = string(&post, "description").or_else(|| string(&post, "title"));
    let post_timestamp = string(&post, "created_at").and_then(|d| crate::date::parse_date(d).ok());
    let mut media = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let id = string(item, "id")
            .filter(|id| valid_id(id))
            .ok_or_else(|| error("Imgur media has an invalid ID"))?;
        let meta = item.get("metadata").unwrap_or(&Value::Null);
        let mut ext = string(item, "ext")
            .or_else(|| {
                string(item, "mime_type")
                    .and_then(|mime| mime.split_once('/'))
                    .map(|(_, sub)| sub)
            })
            .map(str::to_ascii_lowercase)
            .filter(|ext| {
                !ext.is_empty() && ext.len() <= 10 && ext.bytes().all(|b| b.is_ascii_alphanumeric())
            })
            .ok_or_else(|| error("Imgur media has an invalid extension"))?;
        if ext == "jpeg" {
            ext = "jpg".into();
        } else if mp4
            && ext == "gif"
            && meta.get("is_animated").and_then(Value::as_bool) == Some(true)
        {
            // Upstream's default `mp4` option prefers the MP4 rendition.
            ext = "mp4".into();
        }
        let url = Url::parse(&format!("https://i.imgur.com/{id}.{ext}"))
            .map_err(|_| error("invalid Imgur media URL"))?;
        let description = string(meta, "description").or_else(|| string(meta, "title"));
        let positive = |key: &str| item.get(key).and_then(Value::as_u64).filter(|v| *v > 0);
        media.push(Media {
            url,
            job: MediaJob::DirectHttp,
            category: "imgur".into(),
            filename: format!("{id}.{ext}"),
            metadata: Some(PostMetadata {
                file: Some(FileMetadata {
                    width: positive("width"),
                    height: positive("height"),
                    duration: item
                        .get("metadata")
                        .and_then(|m| m.get("duration"))
                        .and_then(Value::as_f64)
                        .filter(|d| *d > 0.0),
                    media_type: string(item, "type").map(Arc::from),
                    description: description.map(Arc::from),
                    ..FileMetadata::default()
                }),
                post_id: requested_id.into(),
                media_id: id.into(),
                username: username.clone(),
                caption: description.or(post_caption).map(Arc::from),
                timestamp: post_timestamp.or_else(|| {
                    string(item, "created_at").and_then(|d| crate::date::parse_date(d).ok())
                }),
                index: index + 1,
                ..PostMetadata::default()
            }),
            headers: BTreeMap::new(),
            handoff: BTreeMap::new(),
        });
    }
    if media.is_empty() {
        return Err(error("Imgur post contains no media"));
    }
    Ok(media)
}

/// Parse one listing page into its posts; `tag` pages list `data.items`.
pub fn parse_listing(bytes: &[u8], tag: bool) -> Result<Vec<Value>> {
    let body: Value =
        serde_json::from_slice(bytes).map_err(|_| error("Imgur returned invalid JSON"))?;
    let data = match body.get("data") {
        Some(data) => data,
        None if body.is_array() => &body,
        None => return Err(error("Imgur response is missing its listing")),
    };
    let data = if tag {
        data.get("items").unwrap_or(&Value::Null)
    } else {
        data
    };
    match data {
        Value::Array(items) => Ok(items.clone()),
        Value::Null => Ok(Vec::new()),
        _ => Err(error("Imgur response is missing its listing")),
    }
}

/// Upstream `_items_queue`: the album or image URL of one listed post.
pub fn queue_url(item: &Value) -> Option<Url> {
    let id = string(item, "id").filter(|id| valid_id(id))?;
    let path = if item.get("is_album").and_then(Value::as_bool) == Some(true) {
        format!("https://imgur.com/a/{id}")
    } else {
        format!("https://imgur.com/{id}")
    };
    Url::parse(&path).ok()
}

pub struct Imgur {
    client: Client,
    network: NetworkOptions,
    api_root: Url,
    client_id: String,
    timeout: Duration,
    request_timeout: Duration,
    retries: u8,
    mp4: bool,
    rate_limits: RateLimits,
}

impl Imgur {
    pub fn new(timeout: Duration) -> Result<Self> {
        Ok(Self {
            client: NetworkOptions::default().client(USER_AGENT, timeout)?,
            network: NetworkOptions::default(),
            api_root: Url::parse(API_ROOT).unwrap(),
            client_id: CLIENT_ID.into(),
            timeout,
            request_timeout: timeout,
            retries: 0,
            mp4: true,
            rate_limits: RateLimits::default(),
        })
    }

    /// Apply proxy and user-agent settings to API requests and emitted media.
    pub fn with_network(mut self, network: &NetworkOptions) -> Result<Self> {
        self.client = network.client(USER_AGENT, self.timeout)?;
        self.network = network.clone();
        Ok(self)
    }

    /// Apply upstream extractor settings (`extractor.imgur.*`): `client-id`.
    pub fn with_options(mut self, options: Option<&Map<String, Value>>) -> Result<Self> {
        match options.and_then(|options| options.get("client-id")) {
            None | Some(Value::Null) => {}
            Some(Value::String(id))
                if (1..=64).contains(&id.len())
                    && id.bytes().all(|b| b.is_ascii_alphanumeric()) =>
            {
                self.client_id = id.clone();
            }
            Some(_) => return Err(error("Imgur client-id must be an alphanumeric string")),
        }
        Ok(self)
    }

    /// Additional attempts for HTTP 429/5xx and transport failures.
    pub fn with_retries(mut self, retries: u8) -> Self {
        self.retries = retries;
        self
    }

    /// Prefer MP4 renditions of animated GIFs (the upstream default).
    /// Disabling this preserves GIFs; native video files remain unchanged.
    pub fn with_mp4(mut self, mp4: bool) -> Self {
        self.mp4 = mp4;
        self
    }

    /// Bound each HTTP request separately from the overall extraction deadline.
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// Override the API origin; intended for local tests.
    pub fn with_api_root(mut self, root: Url) -> Self {
        self.api_root = root;
        self
    }

    fn endpoint(&self, kind: &str, id: &str) -> Url {
        let mut url = self
            .api_root
            .join(&format!("post/v1/{kind}/{id}"))
            .expect("validated Imgur ID");
        url.query_pairs_mut()
            .append_pair("client_id", &self.client_id)
            .append_pair("include", "media,tags,account");
        url
    }

    /// One listing page: the endpoint and whether it uses the `3` API's
    /// `Client-ID` authorization.
    fn listing_page(&self, target: &Target, page: u64) -> Option<(Url, bool)> {
        let path = match target {
            Target::User(name) => format!("3/account/{name}/submissions/{page}"),
            Target::Favorite(name) => format!("3/account/{name}/gallery_favorites/{page}"),
            Target::Subreddit(name) => format!("3/gallery/r/{name}/{page}"),
            Target::Tag(name) => format!("3/gallery/t/{name}/{page}"),
            Target::Search(_) => format!("3/gallery/search/{page}"),
            Target::FavoriteFolder(name, folder) => {
                let mut url = self
                    .api_root
                    .join(&format!("3/account/{name}/folders/{folder}/favorites"))
                    .ok()?;
                url.query_pairs_mut()
                    .append_pair("client_id", &self.client_id)
                    .append_pair("page", &page.to_string())
                    .append_pair("sort", "newest");
                return Some((url, false));
            }
            _ => return None,
        };
        let mut url = self.api_root.join(&path).ok()?;
        if let Target::Search(query) = target {
            url.query_pairs_mut().append_pair("q", query);
        }
        Some((url, true))
    }

    /// Upstream `_items_queue` over a lazily paginated listing.
    fn listing(&self, target: Target) -> BoxStream<'_, Result<ExtractionEvent>> {
        let subcategory = target.subcategory();
        let tag = matches!(target, Target::Tag(_));
        crate::sites::paginate(Some(0u64), move |page| {
            let target = target.clone();
            async move {
                let Some(page) = page else {
                    return Ok(None);
                };
                let (endpoint, authorized) = self
                    .listing_page(&target, page)
                    .ok_or_else(|| error("invalid Imgur listing"))?;
                let body =
                    tokio::time::timeout(self.timeout, self.request_with(&endpoint, authorized))
                        .await
                        .map_err(|_| error("native Imgur extraction timed out"))??;
                let items = parse_listing(&body, tag)?;
                let more = !items.is_empty() && page + 1 < MAX_PAGES;
                let children: Vec<BoxStream<'_, Result<ExtractionEvent>>> = items
                    .iter()
                    .filter_map(|item| {
                        let url = queue_url(item)?;
                        let fields = item.as_object().cloned().unwrap_or_default();
                        Some(child_job(
                            ChildMetadata::from_raw("imgur", subcategory, &fields),
                            move || self.events(&url),
                        ))
                    })
                    .collect();
                Ok(Some((
                    stream::iter(children).flatten().boxed(),
                    more.then_some(page + 1),
                )))
            }
        })
    }

    pub async fn extract(&self, url: &Url) -> Result<Vec<Media>> {
        let target = target(url)?;
        tokio::time::timeout(self.timeout, self.extract_target(&target))
            .await
            .map_err(|_| error("native Imgur extraction timed out"))?
    }

    async fn extract_target(&self, target: &Target) -> Result<Vec<Media>> {
        let id = target.id();
        let body = match target {
            Target::User(_)
            | Target::Favorite(_)
            | Target::FavoriteFolder(..)
            | Target::Subreddit(_)
            | Target::Tag(_)
            | Target::Search(_) => {
                return Err(error("Imgur listings queue posts as child jobs"));
            }
            Target::Image(_) => self.request(&self.endpoint("media", id)).await?,
            Target::Album(_) => self.request(&self.endpoint("albums", id)).await?,
            // Gallery posts may be albums or single images.
            Target::Gallery(_) => match self.request(&self.endpoint("albums", id)).await {
                Ok(body) => body,
                Err(Failure {
                    not_found: true, ..
                }) => self.request(&self.endpoint("media", id)).await?,
                Err(failure) => return Err(failure.error),
            },
        };
        let mut media = parse_post_with_mp4(&body, id, self.mp4)?;
        for item in &mut media {
            self.network.apply_media(item);
        }
        Ok(media)
    }

    pub fn stream(&self, url: &Url) -> BoxStream<'_, Result<Media>> {
        crate::event::media_only(self.events(url))
    }

    /// Emit one post boundary per image, album or gallery post; listings
    /// queue one child job per post.
    pub fn events(&self, url: &Url) -> BoxStream<'_, Result<ExtractionEvent>> {
        match target(url) {
            Ok(Target::Image(_) | Target::Album(_) | Target::Gallery(_)) => {}
            Ok(listing) => return self.listing(listing),
            Err(e) => return crate::sites::fail(e),
        }
        let url = url.clone();
        stream::once(async move {
            let media = self.extract(&url).await?;
            Ok::<_, crate::Error>(stream::iter(group(media, "imgur").into_iter().map(Ok)))
        })
        .map(|result| match result {
            Ok(events) => events.boxed(),
            Err(e) => stream::once(async move { Err(e) }).boxed(),
        })
        .flatten()
        .boxed()
    }

    async fn request(&self, endpoint: &Url) -> std::result::Result<Vec<u8>, Failure> {
        self.request_with(endpoint, false).await
    }

    /// `authorized` sends the `Client-ID` header of the `3` API.
    async fn request_with(
        &self,
        endpoint: &Url,
        authorized: bool,
    ) -> std::result::Result<Vec<u8>, Failure> {
        for attempt in 0..=self.retries {
            match self.attempt(endpoint, attempt, authorized).await {
                Ok(body) => return Ok(body),
                Err(failure) if failure.retry && attempt < self.retries => {
                    tokio::time::sleep(backoff(attempt)).await;
                }
                Err(failure) => return Err(failure),
            }
        }
        unreachable!()
    }

    async fn attempt(
        &self,
        endpoint: &Url,
        attempt: u8,
        authorized: bool,
    ) -> std::result::Result<Vec<u8>, Failure> {
        let mut target = endpoint.clone();
        let mut redirects = Redirects::new(endpoint);
        let mut response = loop {
            self.rate_limits.wait(&target, self.timeout).await?;
            let mut request = self
                .client
                .get(target.clone())
                .timeout(self.request_timeout)
                .header(header::ACCEPT, "application/json")
                .header(header::REFERER, "https://imgur.com/");
            // Only send the client ID to the origin it was meant for.
            if authorized && target.origin() == endpoint.origin() {
                request = request.header(
                    header::AUTHORIZATION,
                    format!("Client-ID {}", self.client_id),
                );
            }
            let response = request
                .send()
                .await
                .map_err(|_| Failure::transient("Imgur request failed; check network access"))?;
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
                            format!("Imgur rate limit; retry after {} seconds", delay.as_secs())
                        }
                        None => "Imgur rate limit; retry later".into(),
                    }
                } else {
                    format!("Imgur returned HTTP {status}")
                },
            ));
        }
        if matches!(status.as_u16(), 404 | 410) {
            return Err(Failure {
                error: error("Imgur post not found or removed"),
                retry: false,
                not_found: true,
            });
        }
        if matches!(status.as_u16(), 401 | 403) {
            return Err(error("Imgur refused access to this post").into());
        }
        if !status.is_success() {
            return Err(error(format!("Imgur returned HTTP {status}")).into());
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| Failure::transient("could not read Imgur response"))?
        {
            if body.len() + chunk.len() > MAX_RESPONSE {
                return Err(error("Imgur response exceeded 16 MiB").into());
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
    not_found: bool,
}

impl Failure {
    fn transient(message: impl Into<String>) -> Self {
        Self {
            error: error(message),
            retry: true,
            not_found: false,
        }
    }
}

impl From<crate::Error> for Failure {
    fn from(error: crate::Error) -> Self {
        Self {
            error,
            retry: false,
            not_found: false,
        }
    }
}

impl From<Failure> for crate::Error {
    fn from(failure: Failure) -> Self {
        failure.error
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

    fn album() -> Vec<u8> {
        json!({
            "id": "AbCdE12",
            "title": "Album title",
            "description": null,
            "created_at": "2021-03-04T05:06:07Z",
            "account": {"username": "someone"},
            "media": [
                {"id":"Img0001","ext":"jpeg","type":"image","mime_type":"image/jpeg",
                 "width":640,"height":480,"size":1234,"created_at":"2021-03-04T05:06:00Z",
                 "metadata":{"title":"","description":"first","is_animated":false}},
                {"id":"Img0002","ext":"gif","type":"image","mime_type":"image/gif",
                 "width":10,"height":10,"metadata":{"is_animated":true}},
                {"id":"Vid0003","ext":"mp4","type":"video","mime_type":"video/mp4",
                 "metadata":{"duration":3.5}}
            ]
        })
        .to_string()
        .into_bytes()
    }

    #[test]
    fn parses_album_media_and_metadata() {
        let media = parse_post(&album(), "AbCdE12").unwrap();
        assert_eq!(media.len(), 3);
        assert_eq!(media[0].url.as_str(), "https://i.imgur.com/Img0001.jpg");
        assert_eq!(media[0].filename, "Img0001.jpg");
        assert_eq!(media[1].filename, "Img0002.mp4");
        assert_eq!(media[2].url.as_str(), "https://i.imgur.com/Vid0003.mp4");
        let post = media[0].metadata.as_ref().unwrap();
        assert_eq!(post.post_id, "AbCdE12");
        assert_eq!(post.media_id, "Img0001");
        assert_eq!(post.index, 1);
        assert_eq!(post.username.as_deref(), Some("someone"));
        assert_eq!(post.caption.as_deref(), Some("first"));
        assert_eq!(post.timestamp, Some(1614834367));
        let file = post.file.as_ref().unwrap();
        assert_eq!((file.width, file.height), (Some(640), Some(480)));
        let third = media[2].metadata.as_ref().unwrap();
        assert_eq!(third.index, 3);
        assert_eq!(third.caption.as_deref(), Some("Album title"));
        assert_eq!(
            third.file.as_ref().unwrap().media_type.as_deref(),
            Some("video")
        );
        assert!(media[0].archive_key().unwrap().starts_with("imgur:v1:"));
        assert_ne!(media[0].archive_key(), media[1].archive_key());
    }

    #[test]
    fn archive_identity_depends_only_on_media_id() {
        let a = parse_post(&album(), "AbCdE12").unwrap().remove(0);
        let mut single = json!({"id":"Img0001","media":[{"id":"Img0001","ext":"jpeg"}]});
        let b = parse_post(single.to_string().as_bytes(), "Img0001")
            .unwrap()
            .remove(0);
        assert_eq!(a.archive_key(), b.archive_key());
        single["media"][0]["id"] = json!("../x");
        assert!(parse_post(single.to_string().as_bytes(), "Img0001").is_err());
    }

    #[test]
    fn mp4_selection_only_changes_animated_gif_renditions() {
        let response = json!({
            "id": "AbCdE12",
            "media": [
                {"id":"Gif0001","ext":"gif","metadata":{"is_animated":true}},
                {"id":"Gif0002","ext":"gif","metadata":{"is_animated":false}},
                {"id":"Gif0003","ext":"gif"},
                {"id":"Vid0004","ext":"mp4","metadata":{"is_animated":true}},
                {"id":"Img0005","ext":"jpeg","metadata":{"is_animated":false}}
            ]
        })
        .to_string();
        let preferred = parse_post(response.as_bytes(), "AbCdE12").unwrap();
        let original = parse_post_with_mp4(response.as_bytes(), "AbCdE12", false).unwrap();
        assert_eq!(preferred[0].url.as_str(), "https://i.imgur.com/Gif0001.mp4");
        assert_eq!(original[0].url.as_str(), "https://i.imgur.com/Gif0001.gif");
        assert_eq!(original[0].filename, "Gif0001.gif");
        assert_eq!(original[0].archive_key(), preferred[0].archive_key());
        assert_eq!(
            serde_json::to_value(&original[0].metadata).unwrap(),
            serde_json::to_value(&preferred[0].metadata).unwrap()
        );
        for index in 1..5 {
            assert_eq!(preferred[index].url, original[index].url);
            assert_eq!(preferred[index].filename, original[index].filename);
        }
        assert_eq!(preferred[1].filename, "Gif0002.gif");
        assert_eq!(preferred[2].filename, "Gif0003.gif");
        assert_eq!(original[3].filename, "Vid0004.mp4");
        assert_eq!(original[4].filename, "Img0005.jpg");
    }

    #[test]
    fn rejects_mismatched_empty_and_invalid_responses() {
        assert!(parse_post(&album(), "Other12").is_err());
        assert!(parse_post(b"not json", "AbCdE12").is_err());
        assert!(parse_post(br#"{"id":"AbCdE12","media":[]}"#, "AbCdE12").is_err());
        assert!(
            parse_post(
                br#"{"id":"AbCdE12","media":[{"id":"Img0001","ext":"j/g"}]}"#,
                "AbCdE12"
            )
            .is_err()
        );
    }

    #[test]
    fn classifies_supported_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        for (raw, expected) in [
            (
                "https://i.imgur.com/21yMxCS.png",
                Target::Image("21yMxCS".into()),
            ),
            (
                "https://i.imgur.com/21yMxCSh.jpg",
                Target::Image("21yMxCS".into()),
            ),
            ("https://imgur.com/21yMxCS", Target::Image("21yMxCS".into())),
            (
                "https://imgur.com/r/pics/21yMxCS",
                Target::Image("21yMxCS".into()),
            ),
            ("https://imgur.com/a/TcBmP", Target::Album("TcBmP".into())),
            (
                "https://imgur.com/a/my-title-TcBmQ12",
                Target::Album("TcBmQ12".into()),
            ),
            (
                "https://imgur.com/gallery/zf2fIms",
                Target::Gallery("zf2fIms".into()),
            ),
            (
                "https://imgur.com/gallery/some-slug-zf2fIms",
                Target::Gallery("zf2fIms".into()),
            ),
            (
                "https://imgur.com/t/cats/zf2fIms",
                Target::Gallery("zf2fIms".into()),
            ),
            (
                "https://www.imgur.io/a/TcBmP",
                Target::Album("TcBmP".into()),
            ),
        ] {
            assert_eq!(t(raw).unwrap(), expected, "{raw}");
        }
        for (raw, expected) in [
            (
                "https://imgur.com/user/someone",
                Target::User("someone".into()),
            ),
            (
                "https://imgur.com/user/someone/submitted/",
                Target::User("someone".into()),
            ),
            (
                "https://m.imgur.com/user/someone/favorites/",
                Target::Favorite("someone".into()),
            ),
            (
                "https://imgur.com/user/someone/favorites/folder/123/My-Title",
                Target::FavoriteFolder("someone".into(), "123".into()),
            ),
            ("https://imgur.com/r/pics", Target::Subreddit("pics".into())),
            ("https://imgur.com/t/cats", Target::Tag("cats".into())),
            (
                "https://imgur.com/search/score?q=red+fox%21&qs=x",
                Target::Search("red fox!".into()),
            ),
        ] {
            assert_eq!(t(raw).unwrap(), expected, "{raw}");
            assert!(crate::source_archive::for_url(&Url::parse(raw).unwrap()).is_some());
        }
        for raw in [
            "https://imgur.com/user/me",
            "https://imgur.com/user/me/posts/hidden",
            "https://imgur.com/user/someone/favorites/folder/x",
            "https://imgur.com/user/a%2Fb",
            "https://imgur.com/search?x=1&q=x",
            "https://imgur.com/search",
            "https://imgur.com/t/cats?x=1",
            "https://imgur.com/a/toolongid",
            "https://imgur.com/a/ab/cd",
            "https://i.imgur.com/a/TcBmP",
            "https://imgur.com.evil.test/a/TcBmP",
            "https://imgur.com:444/a/TcBmP",
            "https://imgur.com/",
        ] {
            assert!(t(raw).is_err(), "{raw}");
        }
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

    fn imgur(root: Url) -> Imgur {
        Imgur::new(Duration::from_secs(5))
            .unwrap()
            .with_network(&NetworkOptions::new(Some(String::new()), None).unwrap())
            .unwrap()
            .with_api_root(root)
    }

    #[tokio::test]
    async fn gallery_uses_album_endpoint_then_falls_back_to_media() {
        let single = json!({"id":"zf2fIms","media":[{"id":"zf2fIms","ext":"png"}]}).to_string();
        let (root, server) = serve(vec![
            response("404 Not Found", "", b"{}"),
            response(
                "200 OK",
                "Content-Type: application/json\r\n",
                single.as_bytes(),
            ),
        ]);
        let events: Vec<_> = imgur(root)
            .events(&Url::parse("https://imgur.com/gallery/cat-zf2fIms").unwrap())
            .collect()
            .await;
        assert_eq!(events.len(), 3);
        assert!(matches!(events[0], Ok(ExtractionEvent::Post(_))));
        assert!(matches!(events[2], Ok(ExtractionEvent::EndPost)));
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with(
            "GET /post/v1/albums/zf2fIms?client_id=546c25a59c58ad7&include=media%2Ctags%2Caccount "
        ));
        assert!(requests[1].starts_with("GET /post/v1/media/zf2fIms?"));
    }

    #[tokio::test]
    async fn extraction_applies_mp4_choice_to_every_post_target() {
        let body = json!({
            "id":"Img0002",
            "media":[{"id":"Img0002","ext":"gif","metadata":{"is_animated":true}}]
        })
        .to_string();
        for url in [
            "https://imgur.com/a/Img0002",
            "https://imgur.com/Img0002",
            "https://imgur.com/gallery/Img0002",
        ] {
            let (root, server) = serve(vec![response("200 OK", "", body.as_bytes())]);
            let media = imgur(root)
                .with_mp4(false)
                .extract(&Url::parse(url).unwrap())
                .await
                .unwrap();
            assert_eq!(media.len(), 1);
            assert_eq!(media[0].filename, "Img0002.gif");
            assert_eq!(media[0].url.as_str(), "https://i.imgur.com/Img0002.gif");
            assert_eq!(server.join().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn retries_rate_limits_and_server_errors() {
        let (root, server) = serve(vec![
            response("429 Too Many Requests", "Retry-After: 0\r\n", b""),
            response("503 Service Unavailable", "", b""),
            response("200 OK", "", &album()),
        ]);
        let media = imgur(root)
            .with_retries(2)
            .extract(&Url::parse("https://imgur.com/a/AbCdE12").unwrap())
            .await
            .unwrap();
        assert_eq!(media.len(), 3);
        assert_eq!(server.join().unwrap().len(), 3);
    }

    #[test]
    fn parses_listings_and_queue_urls() {
        let page = json!({"data": [{"id": "AbCdE", "is_album": true},
            {"id": "Img0001", "is_album": false}, {"id": "../x"}], "success": true});
        let items = parse_listing(page.to_string().as_bytes(), false).unwrap();
        let urls: Vec<_> = items
            .iter()
            .filter_map(queue_url)
            .map(String::from)
            .collect();
        assert_eq!(
            urls,
            ["https://imgur.com/a/AbCdE", "https://imgur.com/Img0001"]
        );
        let tag = json!({"data": {"items": [{"id": "AbCdE"}]}});
        assert_eq!(
            parse_listing(tag.to_string().as_bytes(), true)
                .unwrap()
                .len(),
            1
        );
        assert!(parse_listing(b"{\"data\": []}", false).unwrap().is_empty());
        assert!(parse_listing(b"[]", false).unwrap().is_empty());
        assert!(parse_listing(b"{\"error\": 1}", false).is_err());
        let options = json!({"client-id": "abc/../"});
        assert!(
            imgur(Url::parse(API_ROOT).unwrap())
                .with_options(options.as_object())
                .is_err()
        );
    }

    async fn queued(native: &Imgur, raw: &str) -> Vec<ChildMetadata> {
        let mut events = native.events(&Url::parse(raw).unwrap());
        let mut children = Vec::new();
        while let Some(event) = events.next().await {
            if let ExtractionEvent::BeginChild(scope) = event.unwrap() {
                scope.control.cancel();
                children.push(scope.metadata);
            }
        }
        children
    }

    #[tokio::test]
    async fn listings_paginate_and_queue_posts_as_child_jobs() {
        let page = |items: Value| {
            response(
                "200 OK",
                "",
                json!({"data": items, "success": true})
                    .to_string()
                    .as_bytes(),
            )
        };
        let (root, server) = serve(vec![
            page(json!([{"id": "AbCdE", "is_album": true}, {"id": "Img0001"}])),
            page(json!([])),
            page(json!([{"id": "Img0002"}])),
            page(json!([])),
            page(json!({"items": [{"id": "Img0003"}]})),
            page(json!({"items": []})),
            page(json!([{"id": "Img0004"}])),
            page(json!([])),
            page(json!([{"id": "Img0005"}])),
            page(json!([])),
            page(json!([{"id": "Img0006", "is_album": true}])),
            page(json!([])),
        ]);
        let native = imgur(root)
            .with_options(json!({"client-id": "myclient1"}).as_object())
            .unwrap();
        assert_eq!(
            queued(&native, "https://imgur.com/user/someone")
                .await
                .len(),
            2
        );
        assert_eq!(queued(&native, "https://imgur.com/r/pics").await.len(), 1);
        assert_eq!(queued(&native, "https://imgur.com/t/cats").await.len(), 1);
        assert_eq!(
            queued(&native, "https://imgur.com/search?q=red+fox")
                .await
                .len(),
            1
        );
        assert_eq!(
            queued(&native, "https://imgur.com/user/someone/favorites")
                .await
                .len(),
            1
        );
        assert_eq!(
            queued(
                &native,
                "https://imgur.com/user/someone/favorites/folder/42/x"
            )
            .await
            .len(),
            1
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /3/account/someone/submissions/0 "));
        assert!(
            requests[0]
                .to_ascii_lowercase()
                .contains("authorization: client-id myclient1"),
            "{}",
            requests[0]
        );
        assert!(requests[1].starts_with("GET /3/account/someone/submissions/1 "));
        assert!(requests[2].starts_with("GET /3/gallery/r/pics/0 "));
        assert!(requests[4].starts_with("GET /3/gallery/t/cats/0 "));
        assert!(requests[6].starts_with("GET /3/gallery/search/0?q=red+fox "));
        assert!(requests[8].starts_with("GET /3/account/someone/gallery_favorites/0 "));
        assert!(requests[10].starts_with(
            "GET /3/account/someone/folders/42/favorites?client_id=myclient1&page=0&sort=newest "
        ));
        assert!(!requests[10].to_ascii_lowercase().contains("authorization"));
        assert!(requests[11].contains("&page=1&"));
    }

    #[tokio::test]
    async fn queued_children_extract_their_posts() {
        let album = json!({"id": "AbCdE", "media": [{"id": "Img0001", "ext": "png"}]});
        let (root, server) = serve(vec![
            response(
                "200 OK",
                "",
                json!({"data": [{"id": "AbCdE", "is_album": true, "title": "T"}]})
                    .to_string()
                    .as_bytes(),
            ),
            response("200 OK", "", album.to_string().as_bytes()),
            response("200 OK", "", b"{\"data\": []}"),
        ]);
        let media: Vec<_> = imgur(root)
            .stream(&Url::parse("https://imgur.com/r/pics").unwrap())
            .collect()
            .await;
        assert_eq!(media.len(), 1);
        assert_eq!(
            media[0].as_ref().unwrap().url.as_str(),
            "https://i.imgur.com/Img0001.png"
        );
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /post/v1/albums/AbCdE?"));
    }

    #[tokio::test]
    async fn terminal_errors_are_not_retried() {
        let (root, server) = serve(vec![response("403 Forbidden", "", b"")]);
        let error = imgur(root)
            .with_retries(3)
            .extract(&Url::parse("https://imgur.com/a/AbCdE12").unwrap())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("refused"));
        assert_eq!(server.join().unwrap().len(), 1);
    }
}
