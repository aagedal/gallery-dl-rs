//! Experimental native Wallhaven wallpaper, search, upload and collection
//! extraction.
//!
//! Mirrors gallery_dl/extractor/wallhaven.py using the public JSON API
//! (`/api/v1`): the `image`, `search`, `uploads`, `collection`,
//! `collections` and `user` classes. Files carry upstream's keyword
//! dictionary (the API object with `url`, `date`, `width`, `height`,
//! `wh_category` and tag names, plus the class's `search`, `username` and
//! `collection_id`) and default `{category}_{id}_{resolution}.{extension}`
//! filenames in the class directories. Listings page through
//! `meta.last_page`, capped at 200 pages; the upstream `metadata` option
//! fetches every listed wallpaper's `w/ID` object (tags, uploader).
//! Collection lists and user profiles queue child jobs (`include` selects a
//! profile's `uploads`/`collections` views). Archive identities remain the
//! wallpaper ID. An optional `api-key` is sent as the `X-API-Key` header and
//! is required for NSFW content. Media must be HTTPS on `w.wallhaven.cc`.
use crate::{
    Result, error,
    event::{ChildMetadata, ExtractionEvent, KeywordPolicy, child_job, group_with_keywords},
    extract::{Media, MediaJob, PostMetadata},
    file_metadata::FileMetadata,
    network::NetworkOptions,
    sites::{Route, SiteKeywords},
    transport::{RateLimits, Redirects, retry_after},
};
use futures_util::{
    StreamExt,
    stream::{self, BoxStream},
};
use reqwest::{Client, StatusCode};
use serde_json::{Map, Value};
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

/// Upstream `WallhavenExtractor.filename_fmt`.
const FILENAME: &str = "{category}_{id}_{resolution}.{extension}";
static IMAGE: Route = Route::new("image", FILENAME, &["{category}"], "{id}");
static SEARCH: Route = Route::new(
    "search",
    FILENAME,
    &["{category}", "{search[tags]}"],
    "s_{search[q]}_{id}",
);
static COLLECTION: Route = Route::new(
    "collection",
    FILENAME,
    &["{category}", "{username}", "{collection_id}"],
    "{id}",
);
static UPLOADS: Route = Route::new(
    "uploads",
    FILENAME,
    &["{category}", "{username}"],
    "u_{username}_{id}",
);

/// Views of upstream's `WallhavenUserExtractor` dispatcher.
const USER_VIEWS: &[&str] = &["uploads", "collections"];

/// Upstream extractor classes supported natively, for `--list-extractors`.
pub const SUBCATEGORIES: &[&str] = &[
    "collection",
    "collections",
    "image",
    "search",
    "uploads",
    "user",
];

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

fn valid_collection(id: &str) -> bool {
    (1..=20).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_digit())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Wallpaper {
        id: String,
    },
    /// Search query pairs as upstream's `text.parse_query` (the first value
    /// of each name wins), without the secret `apikey`.
    Search {
        query: Vec<(String, String)>,
    },
    Uploads {
        user: String,
    },
    Collection {
        user: String,
        id: String,
    },
    Collections {
        user: String,
    },
    User {
        user: String,
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
            "unsupported Wallhaven URL; expected https://wallhaven.cc/w/ID, /search?..., /user/USER, /user/USER/uploads, /user/USER/favorites or /user/USER/favorites/ID",
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
        // Upstream maps full-size and thumbnail file URLs to `image` too.
        "w.wallhaven.cc" => match parts[..] {
            [kind, dir, name]
                if !kind.is_empty()
                    && kind.bytes().all(|b| b.is_ascii_lowercase())
                    && dir.len() == 2
                    && dir.bytes().all(|b| b.is_ascii_alphanumeric()) =>
            {
                let id = name
                    .strip_prefix("wallhaven-")
                    .map(|rest| rest.split('.').next().unwrap_or(rest))
                    .filter(|id| valid_id(id))
                    .ok_or_else(unsupported)?;
                Ok(Target::Wallpaper { id: id.into() })
            }
            _ => Err(unsupported()),
        },
        "wallhaven.cc" | "www.wallhaven.cc" => match parts[..] {
            ["w", id] if valid_id(id) => Ok(Target::Wallpaper { id: id.into() }),
            ["search"] => {
                let mut query: Vec<(String, String)> = Vec::new();
                for (key, value) in url.query_pairs() {
                    if key != "apikey" && !query.iter().any(|(k, _)| *k == key) {
                        query.push((key.into_owned(), value.into_owned()));
                    }
                }
                Ok(Target::Search { query })
            }
            ["user", user] if valid_user(user) && url.query().is_none() => {
                Ok(Target::User { user: user.into() })
            }
            ["user", user, "uploads"] if valid_user(user) => {
                Ok(Target::Uploads { user: user.into() })
            }
            ["user", user, "favorites"] if valid_user(user) && url.query().is_none() => {
                Ok(Target::Collections { user: user.into() })
            }
            ["user", user, "favorites", id] if valid_user(user) && valid_collection(id) => {
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

/// Build the media item for one wallpaper object (from `w/ID` or a listing)
/// with the `image` class keywords.
pub fn wallpaper(data: &Value, index: usize) -> Option<Media> {
    wallpaper_with(data, index, &IMAGE, &Map::new())
}

/// Upstream `_transform`, the class `metadata()` and `nameext_from_url`.
fn wallpaper_with(
    data: &Value,
    index: usize,
    route: &'static Route,
    extra: &Map<String, Value>,
) -> Option<Media> {
    let id = text(data, "id").filter(|id| valid_id(id))?;
    let url = media_url(&text(data, "path")?)?;
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
    let timestamp = text(data, "created_at").and_then(|d| crate::date::parse_date(&d).ok());

    let mut fields = data.as_object().cloned().unwrap_or_default();
    fields.remove("path");
    fields.insert("url".into(), url.as_str().into());
    if let Some(Value::Array(list)) = fields.get_mut("tags") {
        *list = list
            .iter()
            .map(|t| t.get("name").cloned().unwrap_or(Value::Null))
            .collect();
    }
    fields.remove("created_at");
    fields.insert(
        "date".into(),
        match timestamp {
            Some(_) => crate::filter::source_date_text(timestamp).into(),
            None => Value::Null,
        },
    );
    let dimension_x = fields.remove("dimension_x").unwrap_or(Value::Null);
    let dimension_y = fields.remove("dimension_y").unwrap_or(Value::Null);
    fields.insert("width".into(), dimension_x);
    fields.insert("height".into(), dimension_y);
    let wh_category = fields.get("category").cloned().unwrap_or(Value::Null);
    fields.insert("wh_category".into(), wh_category);
    for (key, value) in extra {
        fields.insert(key.clone(), value.clone());
    }
    let (name, ext) = crate::sites::nameext(&url);
    fields.insert("filename".into(), name.clone().into());
    fields.insert("extension".into(), ext.clone().into());
    fields.insert("category".into(), "wallhaven".into());
    fields.insert("subcategory".into(), route.subcategory.into());

    Some(Media {
        filename: if ext.is_empty() {
            name
        } else {
            format!("{name}.{ext}")
        },
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
                .or_else(|| {
                    extra
                        .get("username")
                        .and_then(Value::as_str)
                        .map(Into::into)
                })
                .map(Arc::from),
            caption: (!tags.is_empty()).then(|| Arc::from(tags.join(" "))),
            timestamp,
            index,
            site_keywords: Some(Arc::new(SiteKeywords { route, fields })),
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

/// The `data` object of a `w/ID` response.
fn info_object(bytes: &[u8], id: &str) -> Result<Value> {
    let mut body = parse(bytes)?;
    body.get_mut("data")
        .filter(|d| text(d, "id").as_deref() == Some(id))
        .map(Value::take)
        .ok_or_else(|| error("Wallhaven wallpaper not found"))
}

/// Parse a `w/ID` response.
pub fn parse_wallpaper(bytes: &[u8], id: &str) -> Result<Media> {
    wallpaper(&info_object(bytes, id)?, 1)
        .ok_or_else(|| error("Wallhaven wallpaper contains no supported media"))
}

/// One listing page: wallpaper objects and `meta.last_page`.
pub fn parse_page(bytes: &[u8]) -> Result<(Vec<Value>, u64)> {
    let (items, meta) = parse_listing(bytes)?;
    Ok((items, last_page(meta.as_ref())))
}

fn last_page(meta: Option<&Value>) -> u64 {
    meta.and_then(|meta| meta.get("last_page"))
        .and_then(Value::as_u64)
        .unwrap_or(1)
}

/// One listing page: its `data` array and `meta` object, if any.
fn parse_listing(bytes: &[u8]) -> Result<(Vec<Value>, Option<Value>)> {
    let mut body = parse(bytes)?;
    let meta = body
        .get_mut("meta")
        .map(Value::take)
        .filter(Value::is_object);
    match body.get_mut("data").map(Value::take) {
        Some(Value::Array(items)) => Ok((items, meta)),
        _ => Err(error("Wallhaven response contains no wallpaper list")),
    }
}

/// Upstream's `search[tags]`/`search[tag_id]` from the first page's
/// `meta.query`: a tag object, else the plain query string (or null).
fn search_tags(search: &mut Map<String, Value>, meta: Option<&Value>) {
    let Some(query) = meta.and_then(|meta| meta.get("query")) else {
        return;
    };
    match query {
        Value::Object(tag) => {
            search.insert(
                "tags".into(),
                tag.get("tag").cloned().unwrap_or(Value::Null),
            );
            search.insert(
                "tag_id".into(),
                tag.get("id").cloned().unwrap_or(Value::Null),
            );
        }
        other => {
            search.insert("tags".into(), other.clone());
            search.insert("tag_id".into(), 0.into());
        }
    }
}

pub struct Wallhaven {
    client: Client,
    network: NetworkOptions,
    api_root: Url,
    api_key: Option<String>,
    metadata: bool,
    include: Option<crate::dispatch::Include>,
    timeout: Duration,
    request_timeout: Duration,
    retries: u8,
    rate_limits: RateLimits,
}

struct ListingState {
    endpoint: Url,
    route: &'static Route,
    /// Class `metadata()`; a search's dictionary gains `tags`/`tag_id`.
    extra: Map<String, Value>,
    search: bool,
    page: u64,
    pages: u64,
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
            metadata: false,
            include: None,
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

    /// Apply upstream extractor settings (`extractor.wallhaven.*`):
    /// `metadata` (boolean, default false) fetches the full `w/ID` object of
    /// every listed wallpaper.
    pub fn with_options(mut self, options: Option<&Map<String, Value>>) -> Result<Self> {
        let Some(options) = options else {
            return Ok(self);
        };
        self.metadata = match options.get("metadata") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(value)) => *value,
            Some(_) => return Err(error("extractor.wallhaven.metadata must be true or false")),
        };
        Ok(self)
    }

    /// Profile views queued for `/user/USER` (default `uploads`).
    pub fn with_include(mut self, include: Option<crate::dispatch::Include>) -> Self {
        self.include = include;
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

    async fn info(&self, id: &str) -> Result<Value> {
        let body = self.call(&self.endpoint(&format!("api/v1/w/{id}"))).await?;
        info_object(&body, id)
    }

    async fn single(&self, id: &str) -> Result<VecDeque<ExtractionEvent>> {
        let data = self.info(id).await?;
        let media = wallpaper(&data, 1)
            .ok_or_else(|| error("Wallhaven wallpaper contains no supported media"))?;
        Ok(self.finish(vec![media]))
    }

    async fn next_page(&self, state: &mut ListingState) -> Result<()> {
        let mut endpoint = state.endpoint.clone();
        endpoint
            .query_pairs_mut()
            .append_pair("page", &(state.page + 1).to_string());
        let body = self.call(&endpoint).await?;
        let (items, meta) = parse_listing(&body)?;
        if state.search
            && state.pages == 0
            && let Some(Value::Object(search)) = state.extra.get_mut("search")
        {
            search_tags(search, meta.as_ref());
        }
        state.page += 1;
        state.pages += 1;
        state.done =
            items.is_empty() || state.page >= last_page(meta.as_ref()) || state.pages >= MAX_PAGES;
        for item in items {
            let item = if self.metadata {
                // Upstream's `metadata` option replaces each listed object.
                match text(&item, "id").filter(|id| valid_id(id)) {
                    Some(id) => self.info(&id).await?,
                    None => continue,
                }
            } else {
                item
            };
            if let Some(media) = wallpaper_with(&item, state.index + 1, state.route, &state.extra) {
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
    /// Collection lists and user profiles queue one child job per URL.
    pub fn events(&self, url: &Url) -> BoxStream<'_, Result<ExtractionEvent>> {
        let target = match target(url) {
            Ok(target) => target,
            Err(e) => return stream::once(async move { Err(e) }).boxed(),
        };
        let (endpoint, route, extra, start, search) = match target {
            Target::Wallpaper { id } => {
                return stream::once(async move { self.single(&id).await })
                    .map(|result| match result {
                        Ok(events) => stream::iter(events.into_iter().map(Ok)).boxed(),
                        Err(e) => stream::once(async move { Err(e) }).boxed(),
                    })
                    .flatten()
                    .boxed();
            }
            Target::User { user } => return self.user(user),
            Target::Collections { user } => return self.collections(user),
            Target::Search { query } => {
                let mut endpoint = self.endpoint("api/v1/search");
                // Upstream starts at the URL's `page` and keeps it in `search`.
                let start = query
                    .iter()
                    .find(|(k, _)| k == "page")
                    .and_then(|(_, v)| v.trim().parse::<u64>().ok())
                    .unwrap_or(1)
                    .clamp(1, 1_000_000);
                let pairs: Vec<_> = query.iter().filter(|(k, _)| k != "page").collect();
                if !pairs.is_empty() {
                    endpoint.query_pairs_mut().extend_pairs(pairs);
                }
                let search: Map<String, Value> = query
                    .into_iter()
                    .map(|(k, v)| (k, Value::String(v)))
                    .collect();
                let extra = Map::from_iter([("search".into(), Value::Object(search))]);
                (endpoint, &SEARCH, extra, start, true)
            }
            Target::Uploads { user } => {
                let mut endpoint = self.endpoint("api/v1/search");
                endpoint
                    .query_pairs_mut()
                    .append_pair("q", &format!("@{user}"));
                let extra = Map::from_iter([("username".into(), user.into())]);
                (endpoint, &UPLOADS, extra, 1, false)
            }
            Target::Collection { user, id } => {
                let endpoint = self.endpoint(&format!("api/v1/collections/{user}/{id}"));
                let extra = Map::from_iter([
                    ("username".into(), user.into()),
                    ("collection_id".into(), id.into()),
                ]);
                (endpoint, &COLLECTION, extra, 1, false)
            }
        };
        let state = ListingState {
            endpoint,
            route,
            extra,
            search,
            page: start - 1,
            pages: 0,
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

    /// Upstream `WallhavenUserExtractor`: queue the selected profile views.
    fn user(&self, user: String) -> BoxStream<'_, Result<ExtractionEvent>> {
        let views = crate::dispatch::select(self.include.as_ref(), &["uploads"], USER_VIEWS, &[]);
        stream::iter(views)
            .flat_map(move |view| {
                let path = if view == "collections" {
                    "favorites"
                } else {
                    "uploads"
                };
                let url = Url::parse(&format!("https://wallhaven.cc/user/{user}/{path}"))
                    .expect("validated Wallhaven user");
                child_job(
                    ChildMetadata::from_raw("wallhaven", "user", &Map::new()),
                    move || self.events(&url),
                )
            })
            .boxed()
    }

    /// Upstream `WallhavenCollectionsExtractor`: queue every collection of
    /// `api/v1/collections/USER`, its object as the child's metadata.
    fn collections(&self, user: String) -> BoxStream<'_, Result<ExtractionEvent>> {
        struct State {
            user: String,
            page: u64,
            done: bool,
        }
        let state = State {
            user,
            page: 0,
            done: false,
        };
        stream::unfold(state, move |mut state| async move {
            if state.done {
                return None;
            }
            let mut endpoint = self.endpoint(&format!("api/v1/collections/{}", state.user));
            if state.page > 0 {
                endpoint
                    .query_pairs_mut()
                    .append_pair("page", &(state.page + 1).to_string());
            }
            let (items, meta) = match self
                .call(&endpoint)
                .await
                .and_then(|body| parse_listing(&body))
            {
                Ok(page) => page,
                Err(e) => {
                    state.done = true;
                    return Some((stream::once(async move { Err(e) }).boxed(), state));
                }
            };
            state.page += 1;
            // Upstream stops without `meta` or on its last page.
            state.done = match &meta {
                None => true,
                Some(meta) => {
                    let current = meta.get("current_page").and_then(Value::as_u64);
                    let last = meta.get("last_page").and_then(Value::as_u64);
                    current >= last
                }
            } || items.is_empty()
                || state.page >= MAX_PAGES;
            let base = format!("https://wallhaven.cc/user/{}/favorites/", state.user);
            let children: Vec<BoxStream<'_, Result<ExtractionEvent>>> = items
                .into_iter()
                .filter_map(|collection| {
                    let Value::Object(collection) = collection else {
                        return None;
                    };
                    let id = match collection.get("id")? {
                        Value::Number(n) => n.as_u64()?.to_string(),
                        Value::String(s) if valid_collection(s) => s.clone(),
                        _ => return None,
                    };
                    let url = Url::parse(&format!("{base}{id}")).ok()?;
                    Some(child_job(
                        ChildMetadata::from_raw("wallhaven", "collections", &collection),
                        move || self.events(&url),
                    ))
                })
                .collect();
            Some((stream::iter(children).flatten().boxed(), state))
        })
        .flatten()
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
        path::PathBuf,
        thread::{self, JoinHandle},
    };

    const FULL: &str = "https://w.wallhaven.cc/full/94/wallhaven-94x38z.png";

    fn item(id: &str) -> Value {
        json!({"id": id, "path": format!("https://w.wallhaven.cc/full/{id}/wallhaven-{id}.jpg"),
            "resolution": "1920x1080", "file_type": "image/jpeg", "category": "anime",
            "dimension_x": 1920, "dimension_y": 1080,
            "created_at": "2018-10-31 01:23:10"})
    }

    fn keywords(media: &Media) -> &Map<String, Value> {
        &media
            .metadata
            .as_ref()
            .unwrap()
            .site_keywords
            .as_ref()
            .unwrap()
            .fields
    }

    fn paths(media: &Media) -> (String, PathBuf) {
        let (file, dir) = crate::sites::default_templates(media).unwrap();
        (
            file.render(media).unwrap(),
            dir.render(media).unwrap().unwrap(),
        )
    }

    #[test]
    fn parses_wallpaper_and_listing() {
        let body = json!({"data": {"id": "94x38z", "path": FULL, "resolution": "2560x1440",
            "dimension_x": 2560, "dimension_y": 1440, "category": "general",
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
        let k = keywords(&media);
        assert_eq!(k["url"], FULL);
        assert_eq!(k["tags"], json!(["nature", "sky"]));
        assert_eq!(k["date"], "2018-10-31 01:23:10");
        assert_eq!(
            (k["width"].clone(), k["height"].clone()),
            (json!(2560), json!(1440))
        );
        assert_eq!(k["category"], "wallhaven");
        assert_eq!(k["wh_category"], "general");
        assert_eq!(k["subcategory"], "image");
        assert_eq!(
            (k["filename"].clone(), k["extension"].clone()),
            (json!("wallhaven-94x38z"), json!("png"))
        );
        assert!(!k.contains_key("path") && !k.contains_key("dimension_x"));
        assert_eq!(
            paths(&media),
            (
                "wallhaven_94x38z_2560x1440.png".into(),
                PathBuf::from("wallhaven")
            )
        );
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

        // Listing classes keep the wallpaper ID as their archive identity.
        let extra = Map::from_iter([("username".into(), json!("alice"))]);
        let upload = wallpaper_with(&item("a"), 1, &UPLOADS, &extra).unwrap();
        let image = wallpaper(&item("a"), 1).unwrap();
        assert_eq!(upload.archive_key(), image.archive_key());
        let mut search = Map::from_iter([("q".into(), json!("cat"))]);
        search_tags(
            &mut search,
            Some(&json!({"query": {"id": 5, "tag": "Cats"}})),
        );
        assert_eq!(search["tags"], "Cats");
        assert_eq!(search["tag_id"], 5);
        search_tags(&mut search, Some(&json!({"query": "dogs"})));
        assert_eq!(
            (search["tags"].clone(), search["tag_id"].clone()),
            (json!("dogs"), json!(0))
        );
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
            t("https://w.wallhaven.cc/full/94/wallhaven-94x38z.jpg").unwrap(),
            w
        );
        assert_eq!(
            t("https://w.wallhaven.cc/small/94/wallhaven-94x38z.jpg").unwrap(),
            w
        );
        assert_eq!(
            t("https://wallhaven.cc/search?q=touhou&categories=111&page=3&apikey=s&q=x").unwrap(),
            Target::Search {
                query: vec![
                    ("q".into(), "touhou".into()),
                    ("categories".into(), "111".into()),
                    ("page".into(), "3".into()),
                ]
            }
        );
        assert_eq!(
            t("https://wallhaven.cc/user/AksumkA/uploads").unwrap(),
            Target::Uploads {
                user: "AksumkA".into()
            }
        );
        assert_eq!(
            t("https://wallhaven.cc/user/AksumkA/favorites/74").unwrap(),
            Target::Collection {
                user: "AksumkA".into(),
                id: "74".into()
            }
        );
        assert_eq!(
            t("https://wallhaven.cc/user/AksumkA/favorites/").unwrap(),
            Target::Collections {
                user: "AksumkA".into()
            }
        );
        assert_eq!(
            t("https://wallhaven.cc/user/AksumkA").unwrap(),
            Target::User {
                user: "AksumkA".into()
            }
        );
        for raw in [
            "https://wallhaven.cc/",
            "https://wallhaven.cc/w/",
            "https://wallhaven.cc/user/a/favorites/x",
            "https://wallhaven.cc/user/a/likes",
            "https://w.wallhaven.cc/full/94/other-94x38z.jpg",
            "https://w.wallhaven.cc.evil.example/full/94/wallhaven-94x38z.jpg",
            "https://wallhaven.cc.evil.example/w/94x38z",
            "https://evilwallhaven.cc/w/94x38z",
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

    async fn collect(native: &Wallhaven, raw: &str) -> Vec<Media> {
        native
            .stream(&Url::parse(raw).unwrap())
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .map(|m| m.unwrap())
            .collect()
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
        let media = collect(&wallhaven(root), "https://wallhaven.cc/user/alice/uploads").await;
        let indexes: Vec<_> = media
            .iter()
            .map(|m| m.metadata.as_ref().unwrap().index)
            .collect();
        assert_eq!(indexes, [1, 2]);
        let k = keywords(&media[0]);
        assert_eq!(
            (k["username"].clone(), k["subcategory"].clone()),
            (json!("alice"), json!("uploads"))
        );
        assert_eq!(k["wh_category"], "anime");
        assert!(!k.contains_key("tags"));
        assert_eq!(
            paths(&media[1]),
            (
                "wallhaven_b_1920x1080.jpg".into(),
                ["wallhaven", "alice"].iter().collect::<PathBuf>()
            )
        );
        assert_eq!(
            media[0].metadata.as_ref().unwrap().username.as_deref(),
            Some("alice")
        );
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

    #[tokio::test]
    async fn search_starts_at_page_and_records_meta_query() {
        let (root, server) = serve(vec![ok(json!({"data": [item("a")],
            "meta": {"current_page": 3, "last_page": 3, "query": {"id": 9, "tag": "Touhou"}}}))]);
        let media = collect(
            &wallhaven(root),
            "https://wallhaven.cc/search?q=id%3A9&page=3&apikey=secret",
        )
        .await;
        assert_eq!(media.len(), 1);
        let k = keywords(&media[0]);
        assert_eq!(
            k["search"],
            json!({"q": "id:9", "page": "3", "tags": "Touhou", "tag_id": 9})
        );
        assert_eq!(
            paths(&media[0]),
            (
                "wallhaven_a_1920x1080.jpg".into(),
                ["wallhaven", "Touhou"].iter().collect::<PathBuf>()
            )
        );
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with("GET /api/v1/search?q=id%3A9&page=3 "),
            "{}",
            requests[0]
        );
        assert!(!requests[0].contains("secret"));
    }

    #[tokio::test]
    async fn metadata_option_fetches_each_listed_wallpaper() {
        let mut full = item("a");
        full["tags"] = json!([{"name": "sky"}]);
        full["uploader"] = json!({"username": "bob"});
        let (root, server) = serve(vec![
            ok(json!({"data": [item("a")], "meta": {"last_page": 1}})),
            ok(json!({"data": full})),
        ]);
        let options = Map::from_iter([("metadata".into(), json!(true))]);
        let native = wallhaven(root).with_options(Some(&options)).unwrap();
        let media = collect(&native, "https://wallhaven.cc/user/alice/favorites/7").await;
        let k = keywords(&media[0]);
        assert_eq!(k["tags"], json!(["sky"]));
        assert_eq!(k["uploader"]["username"], "bob");
        assert_eq!(
            (k["username"].clone(), k["collection_id"].clone()),
            (json!("alice"), json!("7"))
        );
        assert_eq!(
            paths(&media[0]).1,
            ["wallhaven", "alice", "7"].iter().collect::<PathBuf>()
        );
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /api/v1/w/a "));
        let bad = Map::from_iter([("metadata".into(), json!("yes"))]);
        assert!(
            Wallhaven::new(Duration::from_secs(1))
                .unwrap()
                .with_options(Some(&bad))
                .is_err()
        );
    }

    #[tokio::test]
    async fn queues_collections_and_user_views() {
        let (root, server) = serve(vec![
            ok(json!({"data": [{"id": 7, "label": "Best", "count": 1},
                {"id": "x"}, {"id": 8, "label": "Rest"}]})),
            ok(json!({"data": [item("a")], "meta": {"last_page": 1}})),
            ok(json!({"data": [], "meta": {"last_page": 1}})),
        ]);
        let native = wallhaven(root);
        let events: Vec<_> = native
            .events(&Url::parse("https://wallhaven.cc/user/alice/favorites").unwrap())
            .collect()
            .await;
        let children: Vec<_> = events
            .iter()
            .filter_map(|e| match e.as_ref().unwrap() {
                ExtractionEvent::BeginChild(scope) => Some(scope.metadata.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].subcategory(), "collections");
        assert_eq!(children[0].fields()["id"], 7);
        let media: Vec<_> = events
            .into_iter()
            .filter_map(|e| match e.unwrap() {
                ExtractionEvent::Media(media) => Some(*media),
                _ => None,
            })
            .collect();
        assert_eq!(keywords(&media[0])["collection_id"], "7");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/v1/collections/alice "));
        assert!(requests[1].starts_with("GET /api/v1/collections/alice/7?page=1 "));
        assert!(requests[2].starts_with("GET /api/v1/collections/alice/8?page=1 "));

        let (root, server) = serve(vec![
            ok(json!({"data": [item("a")], "meta": {"last_page": 1}})),
            ok(json!({"data": []})),
        ]);
        let native = wallhaven(root).with_include(Some(crate::dispatch::Include::All));
        let media = collect(&native, "https://wallhaven.cc/user/alice").await;
        assert_eq!(keywords(&media[0])["subcategory"], "uploads");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/v1/search?q=%40alice&page=1 "));
        assert!(requests[1].starts_with("GET /api/v1/collections/alice "));
    }
}
