//! Experimental native Weasyl extraction.
//!
//! Mirrors gallery_dl/extractor/weasyl.py: the `submission`, `submissions`,
//! `folder`, `journal`, `journals` and `favorite` classes. Submissions
//! (`weasyl.com/~USER/submissions/ID/...`, `/submission/ID`, `/view/ID`) use
//! `api/submissions/ID/view`; user galleries (`/~USER`, `/submissions/USER`)
//! and folders (`?folderid=N`) page `api/users/USER/gallery?nextid=N` until
//! `nextid` is null (capped at 200 pages). Journals use
//! `api/journals/ID/view` and are saved as `.html` text files like upstream's
//! `text:` URLs; journal lists and favorites are scraped from site pages.
//! Files carry the API object as upstream's keyword dictionary (`url`, `date`,
//! `filename`, `extension`, `folderid`, `folder_name`, `user`) with default
//! `{submitid} {title}.{extension}` filenames in `weasyl/{owner_login}/`.
//! Archive identities remain the submission ID (journals use `jID`). The
//! upstream `metadata` option fetches every gallery item's full submission.
//! An optional `api-key` is sent as the `X-Weasyl-API-Key` header.
//! Submissions without `media.submission` are skipped like upstream; only
//! its first file is used, as upstream does. Media must be HTTPS on
//! `*.weasyl.com`.
use crate::{
    Result, error,
    event::{ExtractionEvent, group},
    extract::{Media, MediaJob, PostMetadata},
    file_metadata::FileMetadata,
    imagehosts::clean_text,
    network::NetworkOptions,
    sites::{Route, SiteKeywords, extract_iter, unescape},
    transport::{RateLimits, Redirects, retry_after},
};
use futures_util::{
    StreamExt,
    stream::{self, BoxStream},
};
use reqwest::{Client, StatusCode};
use serde_json::{Map, Value};
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

/// Upstream `WeasylExtractor` formats.
const FILENAME: &str = "{submitid} {title}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{owner_login}"];
static SUBMISSION: Route = Route::new("submission", FILENAME, DIRECTORY, "{submitid}");
static SUBMISSIONS: Route = Route::new("submissions", FILENAME, DIRECTORY, "{submitid}");
static FOLDER: Route = Route::new(
    "folder",
    FILENAME,
    &["{category}", "{owner_login}", "{folder_name}"],
    "{submitid}",
);
static JOURNAL: Route = Route::new(
    "journal",
    "{journalid} {title}.{extension}",
    DIRECTORY,
    "{journalid}",
);
static JOURNALS: Route = Route::new(
    "journals",
    "{journalid} {title}.{extension}",
    DIRECTORY,
    "{journalid}",
);
static FAVORITE: Route = Route::new(
    "favorite",
    FILENAME,
    &["{category}", "{user}", "Favorites"],
    "{submitid}",
);

/// Upstream extractor classes supported natively, for `--list-extractors`.
pub const SUBCATEGORIES: &[&str] = &[
    "favorite",
    "folder",
    "journal",
    "journals",
    "submission",
    "submissions",
];

/// Submission IDs are decimal.
pub fn valid_id(id: &str) -> bool {
    (1..=20).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_digit())
}

/// File identities: submission IDs, or `jID` for journals.
pub fn valid_media_id(id: &str) -> bool {
    valid_id(id) || id.strip_prefix('j').is_some_and(valid_id)
}

fn valid_user(user: &str) -> bool {
    (1..=64).contains(&user.len())
        && user
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Submission {
        id: String,
    },
    Gallery {
        user: String,
    },
    Folder {
        user: String,
        id: String,
    },
    Journal {
        id: String,
    },
    Journals {
        user: String,
    },
    /// A user's favorites by name (`/favorites/USER`) or `?userid=N`.
    Favorites {
        user: Option<String>,
        userid: Option<String>,
    },
}

fn plain(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https")
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
}

fn query(url: &Url, name: &str) -> Option<String> {
    url.query_pairs()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
}

pub fn target(url: &Url) -> Result<Target> {
    let unsupported = || {
        error(
            "unsupported Weasyl URL; expected https://www.weasyl.com/~USER/submissions/ID/..., /submission/ID, /~USER, /submissions/USER[?folderid=N], /journal/ID, /journals/USER or /favorites/USER",
        )
    };
    if !plain(url) || !matches!(url.host_str(), Some("www.weasyl.com" | "weasyl.com")) {
        return Err(unsupported());
    }
    let parts: Vec<&str> = url
        .path_segments()
        .map(|s| s.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    let tilde = |raw: &str| {
        raw.strip_prefix('~')
            .filter(|u| valid_user(u))
            .map(str::to_ascii_lowercase)
    };
    let user = |raw: &str| valid_user(raw).then(|| raw.to_ascii_lowercase());
    match parts[..] {
        ["submission" | "view", id, ..] if valid_id(id) => Ok(Target::Submission { id: id.into() }),
        [owner, "submissions", id, ..] if tilde(owner).is_some() && valid_id(id) => {
            Ok(Target::Submission { id: id.into() })
        }
        ["submissions", owner] => {
            let user = user(owner).ok_or_else(unsupported)?;
            match query(url, "folderid") {
                Some(id) if valid_id(&id) => Ok(Target::Folder { user, id }),
                Some(_) => Err(unsupported()),
                None => Ok(Target::Gallery { user }),
            }
        }
        ["favorites"] => match query(url, "userid") {
            Some(id) if valid_id(&id) => Ok(Target::Favorites {
                user: None,
                userid: Some(id),
            }),
            _ => Err(unsupported()),
        },
        [owner] | [owner, "gallery"] => tilde(owner)
            .map(|user| Target::Gallery { user })
            .ok_or_else(unsupported),
        ["journal", id, ..] if valid_id(id) => Ok(Target::Journal { id: id.into() }),
        ["journals", owner, ..] => Ok(Target::Journals {
            user: user(owner).ok_or_else(unsupported)?,
        }),
        ["favorites", owner, ..] => Ok(Target::Favorites {
            user: Some(user(owner).ok_or_else(unsupported)?),
            userid: None,
        }),
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

fn id_string(value: &Value) -> Option<String> {
    match value {
        Value::Number(n) => n.as_u64().map(|n| n.to_string()),
        Value::String(s) if valid_id(s) => Some(s.clone()),
        _ => None,
    }
}

/// Upstream `parse_datetime_iso` of the API's `posted_at`.
fn posted_at(item: &Value, prefix: bool) -> Option<i64> {
    let raw = item.get("posted_at")?.as_str()?;
    let raw = if prefix {
        raw.get(..19).unwrap_or(raw)
    } else {
        raw
    };
    crate::date::parse_date(raw).ok()
}

fn date(timestamp: Option<i64>) -> Value {
    match timestamp {
        Some(_) => crate::filter::source_date_text(timestamp).into(),
        None => Value::Null,
    }
}

fn sensitive(item: &Value) -> Arc<[Arc<str>]> {
    item.get("rating")
        .and_then(Value::as_str)
        .filter(|r| *r != "general")
        .map(|r| vec![Arc::<str>::from(r.to_ascii_lowercase())])
        .unwrap_or_default()
        .into()
}

fn owner(item: &Value) -> Option<Arc<str>> {
    item.get("owner_login")
        .and_then(Value::as_str)
        .filter(|u| valid_user(u))
        .map(Arc::from)
}

/// Convert one API submission object with the `submission` class keywords;
/// `None` when it has no submission file.
pub fn submission_media(item: &Value) -> Option<Media> {
    submission_with(item, &SUBMISSION, &Map::new())
}

/// Upstream `populate_submission` plus class metadata.
fn submission_with(
    item: &Value,
    route: &'static Route,
    extra: &Map<String, Value>,
) -> Option<Media> {
    let id = id_string(item.get("submitid")?)?;
    let url = item
        .pointer("/media/submission/0/url")
        .and_then(Value::as_str)
        .and_then(media_url)?;
    let timestamp = posted_at(item, true);
    let mut fields = item.as_object().cloned().unwrap_or_default();
    fields.insert("url".into(), url.as_str().into());
    fields.insert("date".into(), date(timestamp));
    let (name, ext) = crate::sites::nameext(&url);
    fields.insert("filename".into(), name.clone().into());
    fields.insert("extension".into(), ext.clone().into());
    for (key, value) in extra {
        fields.insert(key.clone(), value.clone());
    }
    fields.insert("category".into(), "weasyl".into());
    fields.insert("subcategory".into(), route.subcategory.into());
    Some(Media {
        filename: if ext.is_empty() {
            name
        } else {
            format!("{name}.{ext}")
        },
        job: MediaJob::DirectHttp,
        category: "weasyl".into(),
        metadata: Some(PostMetadata {
            file: Some(FileMetadata {
                sensitive_flags: sensitive(item),
                ..FileMetadata::default()
            }),
            post_id: id.clone(),
            media_id: id,
            username: owner(item),
            caption: item
                .get("title")
                .and_then(Value::as_str)
                .and_then(clean_text)
                .map(Arc::from),
            timestamp,
            index: 1,
            site_keywords: Some(Arc::new(SiteKeywords { route, fields })),
            ..PostMetadata::default()
        }),
        url,
        headers: BTreeMap::new(),
        handoff: BTreeMap::new(),
    })
}

/// Upstream `retrieve_journal`: the journal saved as an `.html` text file.
pub fn journal_media(item: &Value, route: &'static Route) -> Result<Media> {
    let id = item
        .get("journalid")
        .and_then(id_string)
        .ok_or_else(|| error("Weasyl journal response has no journal ID"))?;
    let content = item
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| error("Weasyl journal has no content"))?;
    let (url, job) = crate::extract::inline_text(content.to_owned())?;
    let timestamp = posted_at(item, false);
    let mut fields = item.as_object().cloned().unwrap_or_default();
    fields.insert("extension".into(), "html".into());
    fields.insert("html".into(), format!("text:{content}").into());
    fields.insert("date".into(), date(timestamp));
    fields.insert("category".into(), "weasyl".into());
    fields.insert("subcategory".into(), route.subcategory.into());
    let media_id = format!("j{id}");
    Ok(Media {
        filename: format!("{id}.html"),
        job,
        category: "weasyl".into(),
        metadata: Some(PostMetadata {
            post_id: media_id.clone(),
            media_id,
            username: owner(item),
            caption: item
                .get("title")
                .and_then(Value::as_str)
                .and_then(clean_text)
                .map(Arc::from),
            timestamp,
            index: 1,
            site_keywords: Some(Arc::new(SiteKeywords { route, fields })),
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

fn html(bytes: &[u8]) -> Result<&str> {
    std::str::from_utf8(bytes).map_err(|_| error("Weasyl returned invalid HTML"))
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

/// Journal IDs linked from a `/journals/USER` page, in order.
pub fn parse_journals(bytes: &[u8]) -> Result<Vec<String>> {
    let mut ids: Vec<String> = Vec::new();
    for id in extract_iter(html(bytes)?, "href=\"/journal/", "/") {
        if valid_id(id) && !ids.iter().any(|i| i == id) {
            ids.push(id.into());
        }
    }
    Ok(ids)
}

/// One favorites page: the owner's login, submission IDs (consecutive
/// duplicates skipped) and the `Next` link's path.
pub fn parse_favorites(bytes: &[u8]) -> Result<(String, Vec<String>, Option<String>)> {
    let page = html(bytes)?;
    let pos = page
        .find("id=\"favorites-content\"")
        .ok_or_else(|| error("Weasyl favorites page has no favorites list"))?;
    let owner = crate::sites::extr(page, "<a href=\"/~", "\"")
        .unwrap_or_default()
        .to_owned();
    let mut ids: Vec<String> = Vec::new();
    for id in extract_iter(&page[pos..], "/submissions/", "/") {
        if valid_id(id) && ids.last().is_none_or(|last| last != id) {
            ids.push(id.into());
        }
    }
    let next = page[pos..].find("\">Next (").map(|at| {
        let head = &page[..pos + at];
        let start = head.rfind("href=\"").map_or(0, |i| i + 6);
        unescape(&page[start..pos + at])
    });
    Ok((owner, ids, next))
}

pub struct Weasyl {
    client: Client,
    network: NetworkOptions,
    root: Url,
    api_key: Option<String>,
    metadata: bool,
    timeout: Duration,
    request_timeout: Duration,
    retries: u8,
    rate_limits: RateLimits,
}

struct GalleryState {
    user: String,
    folder: Option<String>,
    /// The folder's name from its first submission's full object.
    folder_name: Option<Value>,
    next: Option<String>,
    page: u64,
    done: bool,
    seen: HashSet<String>,
    pending: VecDeque<ExtractionEvent>,
}

/// Lazily walked journal or favorites listings.
struct ListState {
    /// Journal IDs, or favorites' submission IDs, still to fetch.
    ids: VecDeque<String>,
    /// The next favorites page path (with query).
    next: Option<String>,
    owner: Option<String>,
    pages: u64,
    seen: HashSet<String>,
}

impl Weasyl {
    pub fn new(timeout: Duration) -> Result<Self> {
        Ok(Self {
            client: NetworkOptions::default().client(USER_AGENT, timeout)?,
            network: NetworkOptions::default(),
            root: Url::parse("https://www.weasyl.com/").expect("static URL"),
            api_key: None,
            metadata: false,
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

    /// Apply upstream extractor settings (`extractor.weasyl.*`): `metadata`
    /// (boolean, default false) fetches every gallery item's full submission.
    pub fn with_options(mut self, options: Option<&Map<String, Value>>) -> Result<Self> {
        let Some(options) = options else {
            return Ok(self);
        };
        self.metadata = match options.get("metadata") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(value)) => *value,
            Some(_) => return Err(error("extractor.weasyl.metadata must be true or false")),
        };
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

    async fn call(&self, path: &str) -> Result<Vec<u8>> {
        let endpoint = self
            .root
            .join(path.trim_start_matches('/'))
            .map_err(|_| error("invalid Weasyl path"))?;
        tokio::time::timeout(self.timeout, self.request(&endpoint))
            .await
            .map_err(|_| error("native Weasyl extraction timed out"))?
    }

    fn emit(&self, mut media: Media) -> VecDeque<ExtractionEvent> {
        self.network.apply_media(&mut media);
        group(vec![media], "weasyl")
    }

    async fn request_submission(&self, id: &str) -> Result<Value> {
        json(&self.call(&format!("api/submissions/{id}/view")).await?)
    }

    async fn submission(&self, id: &str) -> Result<VecDeque<ExtractionEvent>> {
        let media = submission_media(&self.request_submission(id).await?)
            .ok_or_else(|| error("Weasyl submission has no downloadable file"))?;
        Ok(self.emit(media))
    }

    async fn journal(&self, id: &str, route: &'static Route) -> Result<VecDeque<ExtractionEvent>> {
        let item = json(&self.call(&format!("api/journals/{id}/view")).await?)?;
        Ok(self.emit(journal_media(&item, route)?))
    }

    async fn next_page(&self, state: &mut GalleryState) -> Result<()> {
        state.page += 1;
        let mut path = format!("api/users/{}/gallery", state.user);
        let params = {
            let mut params = url::form_urlencoded::Serializer::new(String::new());
            if let Some(next) = &state.next {
                params.append_pair("nextid", next);
            }
            if let Some(folder) = &state.folder {
                params.append_pair("folderid", folder);
            }
            params.finish()
        };
        if !params.is_empty() {
            path.push('?');
            path.push_str(&params);
        }
        let (items, next) = parse_gallery(&self.call(&path).await?)?;
        for item in items {
            let item = match (self.metadata, item.get("submitid").and_then(id_string)) {
                (true, Some(id)) => self.request_submission(&id).await?,
                _ => item,
            };
            let Some(id) = item.get("submitid").and_then(id_string) else {
                continue;
            };
            if item.pointer("/media/submission/0").is_none() || !state.seen.insert(id.clone()) {
                continue;
            }
            let mut extra = Map::new();
            let route = match &state.folder {
                Some(folder) => {
                    // Upstream's folder directory uses the first submission's
                    // full object, the only one carrying `folder_name`.
                    if state.folder_name.is_none() {
                        let details = self.request_submission(&id).await?;
                        state.folder_name =
                            Some(details.get("folder_name").cloned().unwrap_or(Value::Null));
                    }
                    extra.insert("folderid".into(), folder.clone().into());
                    extra.insert(
                        "folder_name".into(),
                        state.folder_name.clone().unwrap_or(Value::Null),
                    );
                    &FOLDER
                }
                None => {
                    extra.insert("folderid".into(), Value::Null);
                    &SUBMISSIONS
                }
            };
            if let Some(media) = submission_with(&item, route, &extra) {
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

    /// Emit one post boundary per submission or journal; listings paginate
    /// lazily.
    pub fn events(&self, url: &Url) -> BoxStream<'_, Result<ExtractionEvent>> {
        let (user, folder) = match target(url) {
            Ok(Target::Gallery { user }) => (user, None),
            Ok(Target::Folder { user, id }) => (user, Some(id)),
            Ok(Target::Submission { id }) => {
                return stream::once(async move { self.submission(&id).await })
                    .map(|result| match result {
                        Ok(events) => stream::iter(events.into_iter().map(Ok)).boxed(),
                        Err(e) => stream::once(async move { Err(e) }).boxed(),
                    })
                    .flatten()
                    .boxed();
            }
            Ok(Target::Journal { id }) => {
                return stream::once(async move { self.journal(&id, &JOURNAL).await })
                    .map(|result| match result {
                        Ok(events) => stream::iter(events.into_iter().map(Ok)).boxed(),
                        Err(e) => stream::once(async move { Err(e) }).boxed(),
                    })
                    .flatten()
                    .boxed();
            }
            Ok(Target::Journals { user }) => {
                return self.listing(ListState {
                    ids: VecDeque::new(),
                    next: Some(format!("journals/{user}")),
                    owner: None,
                    pages: 0,
                    seen: HashSet::new(),
                });
            }
            Ok(Target::Favorites { user, userid }) => {
                let next = match (&user, userid) {
                    (Some(user), _) => format!("favorites/{user}?feature=submit"),
                    (None, Some(id)) => format!("favorites?userid={id}&feature=submit"),
                    (None, None) => unreachable!("validated favorites target"),
                };
                return self.listing(ListState {
                    ids: VecDeque::new(),
                    next: Some(next),
                    owner: user,
                    pages: 0,
                    seen: HashSet::new(),
                });
            }
            Err(e) => return stream::once(async move { Err(e) }).boxed(),
        };
        let state = GalleryState {
            user,
            folder,
            folder_name: None,
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

    /// Journal lists (`journals/USER`, one page) and favorites pages: fetch
    /// each listed item lazily, one request at a time.
    fn listing(&self, state: ListState) -> BoxStream<'_, Result<ExtractionEvent>> {
        let journals = state
            .next
            .as_deref()
            .is_some_and(|path| path.starts_with("journals/"));
        stream::unfold(Some(state), move |state| async move {
            let mut state = state?;
            loop {
                if let Some(id) = state.ids.pop_front() {
                    let result = if journals {
                        self.journal(&id, &JOURNALS).await
                    } else {
                        self.favorite(&id, state.owner.as_deref()).await
                    };
                    return match result {
                        Ok(events) => Some((Ok(events), Some(state))),
                        Err(e) => Some((Err(e), None)),
                    };
                }
                let path = state.next.take()?;
                state.pages += 1;
                let body = match self.call(&path).await {
                    Ok(body) => body,
                    Err(e) => return Some((Err(e), None)),
                };
                if journals {
                    match parse_journals(&body) {
                        Ok(ids) => state.ids.extend(ids),
                        Err(e) => return Some((Err(e), None)),
                    }
                    continue;
                }
                let (owner, ids, next) = match parse_favorites(&body) {
                    Ok(page) => page,
                    Err(e) => return Some((Err(e), None)),
                };
                if state.owner.is_none() && !owner.is_empty() {
                    state.owner = Some(owner);
                }
                let fresh: Vec<String> = ids
                    .into_iter()
                    .filter(|id| state.seen.insert(id.clone()))
                    .collect();
                state.next = next
                    .filter(|next| {
                        !fresh.is_empty()
                            && state.pages < MAX_PAGES
                            && next.starts_with("/favorites")
                            && !next.contains("//")
                    })
                    .filter(|next| next.trim_start_matches('/') != path);
                state.ids.extend(fresh);
            }
        })
        .map(|result| match result {
            Ok(events) => stream::iter(events.into_iter().map(Ok)).boxed(),
            Err(e) => stream::once(async move { Err(e) }).boxed(),
        })
        .flatten()
        .boxed()
    }

    /// One favorited submission; ones without a file yield nothing.
    async fn favorite(&self, id: &str, owner: Option<&str>) -> Result<VecDeque<ExtractionEvent>> {
        let item = self.request_submission(id).await?;
        let extra = Map::from_iter([(
            "user".into(),
            owner.map_or(Value::Null, |owner| owner.into()),
        )]);
        Ok(submission_with(&item, &FAVORITE, &extra)
            .map(|media| self.emit(media))
            .unwrap_or_default())
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
        let api = endpoint.path().starts_with("/api/");
        let mut response = loop {
            self.rate_limits.wait(&target, self.timeout).await?;
            let mut request = self
                .client
                .get(target.clone())
                .timeout(self.request_timeout)
                .header(
                    "Accept",
                    if api {
                        "application/json"
                    } else {
                        "text/html,application/xhtml+xml"
                    },
                );
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
            return Err(error("Weasyl submission, journal or user not found").into());
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
    use serde_json::json;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        path::PathBuf,
        thread::{self, JoinHandle},
    };

    fn submission_json(id: u64) -> String {
        format!(
            r#"{{"submitid":{id},"title":"Big &amp; Bold","owner_login":"alice","posted_at":"2023-09-21T13:23:00Z","rating":"mature","media":{{"submission":[{{"url":"https://cdn.weasyl.com/~alice/submissions/{id}/abc/alice-big.PNG"}}]}}}}"#
        )
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
    fn parses_submissions_and_urls() {
        let value: Value = serde_json::from_str(&submission_json(42)).unwrap();
        let item = submission_media(&value).unwrap();
        assert_eq!(item.filename, "alice-big.png");
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
        let k = keywords(&item);
        assert_eq!(k["submitid"], 42);
        assert_eq!(k["date"], "2023-09-21 13:23:00");
        assert_eq!(
            (k["filename"].clone(), k["extension"].clone()),
            (json!("alice-big"), json!("png"))
        );
        assert_eq!(k["subcategory"], "submission");
        assert_eq!(
            paths(&item),
            (
                "42 Big &amp; Bold.png".into(),
                ["weasyl", "alice"].iter().collect::<PathBuf>()
            )
        );
        assert!(item.archive_key().unwrap().starts_with("weasyl:"));
        let evil = submission_json(1).replace("cdn.weasyl.com", "cdn.weasyl.com.evil.example");
        assert!(submission_media(&serde_json::from_str(&evil).unwrap()).is_none());
        assert!(submission_media(&json!({"submitid":1,"media":{}})).is_none());

        let journal = journal_media(
            &json!({"journalid": 7, "title": "News", "owner_login": "alice",
                "content": "<p>Hi &amp; bye</p>", "posted_at": "2024-01-02T03:04:05Z"}),
            &JOURNAL,
        )
        .unwrap();
        let crate::extract::MediaJob::InlineText { data } = &journal.job else {
            panic!("journals are inline text")
        };
        assert_eq!(data, "<p>Hi &amp; bye</p>");
        assert_eq!(journal.metadata.as_ref().unwrap().media_id, "j7");
        let k = keywords(&journal);
        assert_eq!(k["extension"], "html");
        assert_eq!(k["html"], "text:<p>Hi &amp; bye</p>");
        assert_eq!(k["date"], "2024-01-02 03:04:05");
        assert_eq!(paths(&journal).0, "7 News.html");
        let key = journal.archive_key().unwrap();
        assert_ne!(Some(key), item.archive_key());
        assert!(valid_media_id("j7") && !valid_media_id("jx") && !valid_media_id("7j"));

        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        let sub = Target::Submission { id: "42".into() };
        assert_eq!(
            t("https://www.weasyl.com/~alice/submissions/42/big-bold").unwrap(),
            sub
        );
        assert_eq!(t("https://weasyl.com/submission/42").unwrap(), sub);
        assert_eq!(t("https://www.weasyl.com/view/42").unwrap(), sub);
        let gallery = Target::Gallery {
            user: "alice".into(),
        };
        assert_eq!(t("https://www.weasyl.com/~Alice").unwrap(), gallery);
        assert_eq!(t("https://www.weasyl.com/~alice/gallery").unwrap(), gallery);
        assert_eq!(
            t("https://www.weasyl.com/submissions/alice").unwrap(),
            gallery
        );
        assert_eq!(
            t("https://www.weasyl.com/submissions/alice?folderid=12").unwrap(),
            Target::Folder {
                user: "alice".into(),
                id: "12".into()
            }
        );
        assert_eq!(
            t("https://www.weasyl.com/journal/7").unwrap(),
            Target::Journal { id: "7".into() }
        );
        assert_eq!(
            t("https://www.weasyl.com/journals/alice").unwrap(),
            Target::Journals {
                user: "alice".into()
            }
        );
        assert_eq!(
            t("https://www.weasyl.com/favorites/alice").unwrap(),
            Target::Favorites {
                user: Some("alice".into()),
                userid: None
            }
        );
        assert_eq!(
            t("https://www.weasyl.com/favorites?userid=5").unwrap(),
            Target::Favorites {
                user: None,
                userid: Some("5".into())
            }
        );
        for raw in [
            "https://www.weasyl.com/",
            "https://www.weasyl.com/~alice/favorites",
            "https://www.weasyl.com/submission/abc",
            "https://www.weasyl.com/submissions/alice?folderid=x",
            "https://www.weasyl.com/favorites",
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

    fn ok(body: &str) -> Vec<u8> {
        response("200 OK", body.as_bytes())
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

    async fn collect(native: &Weasyl, raw: &str) -> Vec<Media> {
        native
            .stream(&Url::parse(raw).unwrap())
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .map(|m| m.unwrap())
            .collect()
    }

    #[tokio::test]
    async fn fetches_submission_with_retry_and_api_key() {
        let (root, server) = serve(vec![
            response("429 Too Many Requests", b""),
            ok(&submission_json(42)),
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
        let (root, server) = serve(vec![ok(&page1), ok(&page2)]);
        let media = collect(&weasyl(root), "https://www.weasyl.com/~alice/gallery").await;
        let ids: Vec<_> = media
            .iter()
            .map(|m| m.metadata.as_ref().unwrap().media_id.clone())
            .collect();
        assert_eq!(ids, ["42", "6"]);
        let k = keywords(&media[0]);
        assert_eq!(
            (k["subcategory"].clone(), k["folderid"].clone()),
            (json!("submissions"), Value::Null)
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/users/alice/gallery "));
        assert!(
            !requests[0]
                .to_ascii_lowercase()
                .contains("x-weasyl-api-key")
        );
        assert!(requests[1].starts_with("GET /api/users/alice/gallery?nextid=7 "));
    }

    #[tokio::test]
    async fn folders_use_first_submission_folder_name_and_metadata_option() {
        let page = format!(
            r#"{{"submissions":[{},{}],"nextid":null}}"#,
            submission_json(42),
            submission_json(6)
        );
        let mut details: Value = serde_json::from_str(&submission_json(42)).unwrap();
        details["folder_name"] = json!("Sketches");
        details["tags"] = json!(["fox"]);
        let mut second: Value = serde_json::from_str(&submission_json(6)).unwrap();
        second["tags"] = json!(["cat"]);
        let (root, server) = serve(vec![
            ok(&page),
            ok(&details.to_string()),
            ok(&details.to_string()),
            ok(&second.to_string()),
        ]);
        let options = Map::from_iter([("metadata".into(), json!(true))]);
        let native = weasyl(root).with_options(Some(&options)).unwrap();
        let media = collect(
            &native,
            "https://www.weasyl.com/submissions/alice?folderid=12",
        )
        .await;
        assert_eq!(media.len(), 2);
        let k = keywords(&media[1]);
        assert_eq!(
            (
                k["folderid"].clone(),
                k["folder_name"].clone(),
                k["tags"].clone()
            ),
            (json!("12"), json!("Sketches"), json!(["cat"]))
        );
        assert_eq!(
            paths(&media[1]).1,
            ["weasyl", "alice", "Sketches"].iter().collect::<PathBuf>()
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/users/alice/gallery?folderid=12 "));
        assert!(requests[1].starts_with("GET /api/submissions/42/view "));
        assert!(requests[3].starts_with("GET /api/submissions/6/view "));
        let bad = Map::from_iter([("metadata".into(), json!("yes"))]);
        assert!(
            Weasyl::new(Duration::from_secs(1))
                .unwrap()
                .with_options(Some(&bad))
                .is_err()
        );
    }

    #[tokio::test]
    async fn journals_and_favorites_are_scraped_lazily() {
        let journal = json!({"journalid": 7, "title": "News", "owner_login": "alice",
            "content": "<p>Hi</p>", "posted_at": "2024-01-02T03:04:05Z"});
        let (root, server) = serve(vec![
            ok(r#"<a href="/journal/7/news">News</a><a href="/journal/7/news">again</a>"#),
            ok(&journal.to_string()),
        ]);
        let media = collect(&weasyl(root), "https://www.weasyl.com/journals/alice").await;
        assert_eq!(media.len(), 1);
        assert_eq!(keywords(&media[0])["subcategory"], "journals");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /journals/alice "));
        assert!(requests[1].starts_with("GET /api/journals/7/view "));

        let page1 = r#"<a href="/~alice">Alice</a><div id="favorites-content">
<a href="/~bob/submissions/42/big"><img></a><a href="/~bob/submissions/42/big">Big</a>
<a href="/~carol/submissions/8/none">None</a>
<a href="/favorites?userid=5&amp;feature=submit&amp;nextid=8">Next (20)</a></div>"#;
        let page2 =
            r#"<div id="favorites-content"><a href="/~bob/submissions/42/big">Big</a></div>"#;
        let (root, server) = serve(vec![
            ok(page1),
            ok(&submission_json(42)),
            ok(r#"{"submitid": 8, "media": {}}"#),
            ok(page2),
        ]);
        let media = collect(&weasyl(root), "https://www.weasyl.com/favorites?userid=5").await;
        assert_eq!(media.len(), 1);
        let k = keywords(&media[0]);
        assert_eq!(
            (k["user"].clone(), k["subcategory"].clone()),
            (json!("alice"), json!("favorite"))
        );
        assert_eq!(
            paths(&media[0]).1,
            ["weasyl", "alice", "Favorites"].iter().collect::<PathBuf>()
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /favorites?userid=5&feature=submit "));
        assert!(requests[1].starts_with("GET /api/submissions/42/view "));
        assert!(requests[2].starts_with("GET /api/submissions/8/view "));
        assert!(
            requests[3].starts_with("GET /favorites?userid=5&feature=submit&nextid=8 "),
            "{}",
            requests[3]
        );
    }
}
