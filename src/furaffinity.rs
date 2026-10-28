//! Experimental native Fur Affinity extraction.
//!
//! Mirrors the submission and gallery parts of
//! gallery_dl/extractor/furaffinity.py. Submission pages
//! (`www.furaffinity.net/view/ID/` or `/full/ID/`) are scraped from HTML: the
//! `Download` link on `d.furaffinity.net`, the title, the artist's `/user/`
//! link, the `popup_date` posting time and the rating. Galleries and scraps
//! (`/gallery/USER/`, `/scraps/USER/`) page through `/gallery/USER/N/`
//! listing `<figure id="sid-ID">` items until a page adds no new IDs (capped
//! at 200 pages). Session cookies (`a`, `b`) from the cookie file are sent
//! when present so mature submissions work when logged in; otherwise those
//! submissions are skipped. Media must be HTTPS on `d.furaffinity.net` or
//! `*.facdn.net`.
use crate::{
    Result,
    cookies::CookieJar,
    error,
    event::{ExtractionEvent, KeywordPolicy, group_with_keywords},
    extract::{Media, MediaJob, PostMetadata},
    file_metadata::FileMetadata,
    imagehosts::{
        attribute, between, between_all, clean_text, meta_property, tag_around, unescape,
    },
    network::NetworkOptions,
    transport::{RateLimits, Redirects, retry_after},
};
use futures_util::{
    StreamExt,
    stream::{self, BoxStream},
};
use reqwest::{Client, StatusCode};
use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    sync::Arc,
    time::{Duration, SystemTime},
};
use url::Url;

const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:128.0) Gecko/20100101 Firefox/128.0";
const MAX_RESPONSE: usize = 16 * 1024 * 1024;
const MAX_PAGES: u64 = 200;
const NOT_AVAILABLE: &str = "Fur Affinity submission has no download link";

/// Submission IDs are decimal.
pub fn valid_id(id: &str) -> bool {
    (1..=20).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_digit())
}

fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~'))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Folder {
    Gallery,
    Scraps,
    /// `/favorites/USER/`, paged by "Next" links rather than numbers.
    Favorites,
    /// `/gallery/USER/folder/ID/`.
    Folder(String),
    /// `/search/?q=...` form parameters (POST, upstream `_pagination_search`).
    Search(Vec<(String, String)>),
    /// `/msg/submissions[/...]` (needs session cookies), paged by "Next 48".
    Submissions(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Submission {
        id: String,
    },
    Gallery {
        user: String,
        folder: Folder,
    },
    /// `/watchlist/by/USER/`: watched users, queued as user children.
    Following {
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
            "unsupported Fur Affinity URL; expected https://www.furaffinity.net/view/ID/ or https://www.furaffinity.net/gallery/USER/",
        )
    };
    // Upstream `BASE_PATTERN`: furaffinity.net and the fxraffinity,
    // xfuraffinity and fxfuraffinity embed mirrors, with `www.`/`sfw.`.
    let host = url.host_str().unwrap_or_default();
    let host = host
        .strip_prefix("www.")
        .or_else(|| host.strip_prefix("sfw."))
        .unwrap_or(host);
    if !plain(url)
        || !matches!(
            host,
            "furaffinity.net" | "fxraffinity.net" | "xfuraffinity.net" | "fxfuraffinity.net"
        )
    {
        return Err(unsupported());
    }
    let parts: Vec<&str> = url
        .path_segments()
        .map(|s| s.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    match parts[..] {
        ["view" | "full", id, ..] if valid_id(id) => Ok(Target::Submission { id: id.into() }),
        ["gallery", user, "folder", id, ..]
            if valid_name(user) && !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) =>
        {
            Ok(Target::Gallery {
                user: user.to_ascii_lowercase(),
                folder: Folder::Folder((*id).into()),
            })
        }
        // The user dispatcher defaults to the gallery.
        ["user", user, ..] if valid_name(user) => Ok(Target::Gallery {
            user: user.to_ascii_lowercase(),
            folder: Folder::Gallery,
        }),
        ["watchlist", "by", user, ..] if valid_name(user) => Ok(Target::Following {
            user: user.to_ascii_lowercase(),
        }),
        ["msg", "submissions", ..] => Ok(Target::Gallery {
            user: String::new(),
            folder: Folder::Submissions(url.path().trim_start_matches('/').to_owned()),
        }),
        ["search", ..] if url.query().is_some() => {
            let mut query: Vec<(String, String)> = url
                .query_pairs()
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect();
            // `/search/TERMS/?...` supplies `q` when the query has none.
            if let ["search", terms] = parts[..]
                && !query.iter().any(|(k, _)| k == "q")
            {
                query.push(("q".into(), crate::sites::percent_decode(terms)));
            }
            Ok(Target::Gallery {
                user: String::new(),
                folder: Folder::Search(query),
            })
        }
        [folder @ ("gallery" | "scraps" | "favorites"), user, ..] if valid_name(user) => {
            Ok(Target::Gallery {
                user: user.to_ascii_lowercase(),
                folder: match folder {
                    "gallery" => Folder::Gallery,
                    "scraps" => Folder::Scraps,
                    _ => Folder::Favorites,
                },
            })
        }
        _ => Err(unsupported()),
    }
}

pub fn is_furaffinity_url(url: &Url) -> bool {
    target(url).is_ok()
}

/// HTTPS media on `d.furaffinity.net` or `*.facdn.net`.
pub fn media_url(raw: &str) -> Option<Url> {
    let raw = unescape(raw.trim());
    let raw = match raw.strip_prefix("//") {
        Some(rest) => format!("https://{rest}"),
        None => raw,
    };
    let url = Url::parse(&raw).ok()?;
    let host = url.host_str()?;
    let allowed = host == "d.furaffinity.net"
        || host.strip_suffix(".facdn.net").is_some_and(|label| {
            !label.is_empty()
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.'))
        });
    (url.scheme() == "https" && plain(&url) && allowed).then_some(url)
}

fn strip_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut inside = false;
    for ch in text.chars() {
        match ch {
            '<' => inside = true,
            '>' => inside = false,
            _ if !inside => out.push(ch),
            _ => {}
        }
    }
    out
}

/// Parse `Sep 21, 2023 01:23 PM` (also full month names and `21st`).
pub fn parse_popup_date(raw: &str) -> Option<i64> {
    let raw = raw.trim().replace(',', " ");
    let parts: Vec<&str> = raw.split_whitespace().collect();
    let [month, day, year, time, meridiem] = parts[..] else {
        return None;
    };
    let month = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ]
    .iter()
    .position(|m| month.len() >= 3 && month.to_ascii_lowercase().starts_with(m))?
        + 1;
    let day: u32 = day
        .trim_end_matches(|c: char| c.is_ascii_alphabetic())
        .parse()
        .ok()?;
    let year: u32 = year.parse().ok().filter(|y| (1970..=9999).contains(y))?;
    let (hour, minute) = time.split_once(':')?;
    let (hour, minute): (u32, u32) = (hour.parse().ok()?, minute.parse().ok()?);
    if !(1..=12).contains(&hour) || minute > 59 {
        return None;
    }
    let hour = match meridiem.to_ascii_uppercase().as_str() {
        "AM" => hour % 12,
        "PM" => hour % 12 + 12,
        _ => return None,
    };
    crate::date::parse_date(&format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:00Z"
    ))
    .ok()
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

fn download_url(html: &str) -> Option<Url> {
    // Prefer the anchor whose text is "Download"; fall back to any art link.
    if let Some(at) = html.find(">Download</a>")
        && let Some(tag) = tag_around(html, at)
        && let Some(url) = attribute(tag, "href").and_then(media_url)
    {
        return Some(url);
    }
    between_all(html, "href=\"", "\"")
        .filter(|h| h.contains("/art/"))
        .find_map(media_url)
}

fn artist(html: &str) -> Option<String> {
    let header = html
        .find("submission-id-sub-container")
        .or_else(|| html.find("submission-title"))
        .map_or(html, |at| &html[at..]);
    between(header, "href=\"/user/", "/\"")
        .or_else(|| between(html, "href=\"/user/", "/\""))
        .filter(|name| valid_name(name))
        .map(str::to_ascii_lowercase)
}

fn rating(html: &str) -> Option<String> {
    let block = between(html, "class=\"rating-box", "</span>")
        .or_else(|| between(html, "class=\"rating\"", "</span>"))?;
    let text = block.rsplit('>').next()?;
    clean_text(text).map(|t| t.to_ascii_lowercase())
}

/// Parse a submission page.
pub fn parse_submission(bytes: &[u8], id: &str) -> Result<Vec<Media>> {
    let html =
        std::str::from_utf8(bytes).map_err(|_| error("Fur Affinity returned invalid HTML"))?;
    let url = download_url(html).ok_or_else(|| error(NOT_AVAILABLE))?;
    let caption = between(html, "<div class=\"submission-title\">", "</div>")
        .map(strip_tags)
        .and_then(|t| clean_text(&t))
        .or_else(|| meta_property(html, "og:title").and_then(clean_text))
        .map(Arc::<str>::from);
    let timestamp =
        between(html, "<span class=\"popup_date\" title=\"", "\"").and_then(parse_popup_date);
    let rating = rating(html);
    let sensitive: Arc<[Arc<str>]> = rating
        .filter(|r| r != "general")
        .map(|r| vec![Arc::<str>::from(r)])
        .unwrap_or_default()
        .into();
    Ok(vec![Media {
        filename: format!("{id}.{}", extension(&url)),
        job: MediaJob::DirectHttp,
        category: "furaffinity".into(),
        metadata: Some(PostMetadata {
            file: Some(FileMetadata {
                sensitive_flags: sensitive,
                ..FileMetadata::default()
            }),
            post_id: id.into(),
            media_id: id.into(),
            username: artist(html).map(Arc::from),
            caption,
            timestamp,
            index: 1,
            ..PostMetadata::default()
        }),
        url,
        headers: BTreeMap::new(),
        handoff: BTreeMap::new(),
    }])
}

/// Submission IDs listed on one gallery page, in order.
/// Upstream `_pagination_favorites`: the "Next" form's action, else the
/// `right` link; a site-relative path without its leading `/`.
fn favorites_next(bytes: &[u8]) -> Option<String> {
    let html = std::str::from_utf8(bytes).ok()?;
    let path = match html.find("type=\"submit\">Next</button>") {
        Some(pos) => {
            let start = html[..pos].rfind("<form action=\"")? + 14;
            html[start..].split('"').next()?
        }
        None => between(html, "right\" href=\"", "\"")?,
    };
    let path = path.trim_start_matches('/');
    (path.starts_with("favorites/") && !path.contains("..")).then(|| path.to_owned())
}

/// Upstream `_pagination_search` form data for request number `page`
/// (from one): the first request has no `next_page`, later ones send
/// `next_page=Next` with the previous page number.
fn search_form(query: &[(String, String)], page: u64) -> Vec<(String, String)> {
    let mut form: Vec<(String, String)> = [
        ("order-by", "relevancy"),
        ("order-direction", "desc"),
        ("range", "all"),
        ("range_from", ""),
        ("range_to", ""),
        ("rating-general", "1"),
        ("rating-mature", "1"),
        ("rating-adult", "1"),
        ("type-art", "1"),
        ("type-music", "1"),
        ("type-flash", "1"),
        ("type-story", "1"),
        ("type-photo", "1"),
        ("type-poetry", "1"),
        ("mode", "extended"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect();
    for (key, value) in query {
        form.retain(|(k, _)| k != key);
        form.push((key.clone(), value.clone()));
    }
    let start = query
        .iter()
        .find(|(k, _)| k == "page")
        .and_then(|(_, v)| v.parse::<u64>().ok())
        .unwrap_or(1);
    form.retain(|(k, _)| k != "page");
    let number = if page <= 1 { start } else { start + page - 2 };
    form.push(("page".into(), number.to_string()));
    if page > 1 {
        form.push(("next_page".into(), "Next".into()));
    }
    form
}

/// Upstream `_pagination_submissions`: the link before "Next 48".
fn submissions_next(bytes: &[u8]) -> Option<String> {
    let html = std::str::from_utf8(bytes).ok()?;
    let pos = html
        .find(">Next 48</a>")
        .or_else(|| html.find(">&gt;&gt;&gt; Next 48 &gt;&gt;"))?;
    let start = html[..pos].rfind("href=\"")? + 6;
    let path = unescape(html[start..pos].split('"').next()?);
    let path = path.trim_start_matches('/');
    (path.starts_with("msg/submissions") && !path.contains("..")).then(|| path.to_owned())
}

pub fn parse_gallery(bytes: &[u8]) -> Result<Vec<String>> {
    let html =
        std::str::from_utf8(bytes).map_err(|_| error("Fur Affinity returned invalid HTML"))?;
    let mut ids = Vec::new();
    for id in between_all(html, "<figure id=\"sid-", "\"") {
        if valid_id(id) && !ids.iter().any(|i| i == id) {
            ids.push(id.to_owned());
        }
    }
    Ok(ids)
}

pub struct FurAffinity {
    client: Client,
    network: NetworkOptions,
    root: Url,
    cookie: Option<String>,
    timeout: Duration,
    request_timeout: Duration,
    retries: u8,
    rate_limits: RateLimits,
}

struct GalleryState {
    user: String,
    folder: Folder,
    page: u64,
    /// The next favorites page path, as linked from the current one.
    next: Option<String>,
    done: bool,
    seen: HashSet<String>,
    pending: VecDeque<ExtractionEvent>,
}

impl FurAffinity {
    pub fn new(timeout: Duration) -> Result<Self> {
        Self::from_cookie_jar(CookieJar::default(), timeout)
    }

    /// Build an extractor that sends Fur Affinity session cookies.
    pub fn from_cookie_jar(cookies: CookieJar, timeout: Duration) -> Result<Self> {
        let site = Url::parse("https://www.furaffinity.net/").expect("static URL");
        Ok(Self {
            client: NetworkOptions::default().client(USER_AGENT, timeout)?,
            network: NetworkOptions::default(),
            root: site.clone(),
            cookie: cookies.header(&site),
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

    async fn call(&self, path: &str) -> Result<Vec<u8>> {
        let endpoint = self
            .root
            .join(path)
            .map_err(|_| error("invalid Fur Affinity path"))?;
        tokio::time::timeout(self.timeout, self.request(&endpoint))
            .await
            .map_err(|_| error("native Fur Affinity extraction timed out"))?
    }

    async fn call_form(&self, path: &str, form: &[(String, String)]) -> Result<Vec<u8>> {
        let endpoint = self
            .root
            .join(path)
            .map_err(|_| error("invalid Fur Affinity path"))?;
        tokio::time::timeout(self.timeout, self.request_with(&endpoint, Some(form)))
            .await
            .map_err(|_| error("native Fur Affinity extraction timed out"))?
    }

    /// Upstream `FuraffinityFollowingExtractor`: queue each watched user.
    fn following(&self, user: String) -> BoxStream<'_, Result<ExtractionEvent>> {
        stream::try_unfold(
            (Some(format!("watchlist/by/{user}/")), 0u64),
            move |(path, pages)| async move {
                let Some(path) = path.filter(|_| pages < MAX_PAGES) else {
                    return Ok(None);
                };
                let body = self.call(&path).await?;
                let html = String::from_utf8_lossy(&body).into_owned();
                let mut seen = HashSet::new();
                let users: Vec<String> = between_all(&html, "<a href=\"/user/", "\"")
                    .map(|name| name.trim_end_matches('/').to_ascii_lowercase())
                    .filter(|name| valid_name(name) && seen.insert(name.clone()))
                    .collect();
                let next = html
                    .rfind("action=\"")
                    .and_then(|start| html[start + 8..].split('"').next())
                    .map(|next| next.trim_start_matches('/').to_owned())
                    .filter(|next| {
                        next.starts_with("watchlist/") && *next != path && !next.contains("..")
                    });
                let children: Vec<_> = users
                    .into_iter()
                    .map(|name| {
                        crate::event::child_job(
                            crate::event::ChildMetadata::from_raw(
                                "furaffinity",
                                "following",
                                &serde_json::Map::new(),
                            ),
                            move || {
                                let url = Url::parse(&format!(
                                    "https://www.furaffinity.net/user/{name}/"
                                ))
                                .expect("validated user");
                                self.events(&url)
                            },
                        )
                    })
                    .collect();
                Ok::<_, crate::Error>(Some((stream::iter(children).flatten(), (next, pages + 1))))
            },
        )
        .map(|result| match result {
            Ok(events) => events.boxed(),
            Err(e) => stream::once(async move { Err(e) }).boxed(),
        })
        .flatten()
        .boxed()
    }

    async fn submission(&self, id: &str) -> Result<VecDeque<ExtractionEvent>> {
        let body = self.call(&format!("view/{id}/")).await?;
        let mut items = parse_submission(&body, id)?;
        for item in &mut items {
            self.network.apply_media(item);
        }
        let mut keywords = KeywordPolicy::shared(&["num", "index", "media_id"]);
        if let Some(media) = items.first() {
            keywords = keywords.with_post_fields(media, &["filename", "name", "extension"]);
        }
        Ok(group_with_keywords(items, "furaffinity", keywords))
    }

    async fn next_page(&self, state: &mut GalleryState) -> Result<()> {
        state.page += 1;
        let mut form = None;
        let path = match &state.folder {
            Folder::Gallery => format!("gallery/{}/{}/", state.user, state.page),
            Folder::Scraps => format!("scraps/{}/{}/", state.user, state.page),
            Folder::Folder(id) => format!("gallery/{}/folder/{id}/a/{}/", state.user, state.page),
            Folder::Favorites => state
                .next
                .take()
                .unwrap_or_else(|| format!("favorites/{}/", state.user)),
            Folder::Submissions(first) => state.next.take().unwrap_or_else(|| first.clone()),
            Folder::Search(query) => {
                form = Some(search_form(query, state.page));
                "search/".to_owned()
            }
        };
        let body = match form {
            Some(form) => self.call_form(&path, &form).await?,
            None => self.call(&path).await?,
        };
        match state.folder {
            Folder::Favorites => state.next = favorites_next(&body),
            Folder::Submissions(_) => state.next = submissions_next(&body),
            _ => {}
        }
        let ids: Vec<String> = parse_gallery(&body)?
            .into_iter()
            .filter(|id| state.seen.insert(id.clone()))
            .collect();
        state.done = ids.is_empty()
            || state.page >= MAX_PAGES
            || (matches!(state.folder, Folder::Favorites | Folder::Submissions(_))
                && state.next.is_none());
        for id in ids {
            match self.submission(&id).await {
                Ok(events) => state.pending.extend(events),
                // Mature or removed submissions do not abort the gallery.
                Err(e) if e.to_string() == NOT_AVAILABLE => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    pub fn stream(&self, url: &Url) -> BoxStream<'_, Result<Media>> {
        crate::event::media_only(self.events(url))
    }

    /// Emit one post boundary per submission; galleries paginate lazily.
    pub fn events(&self, url: &Url) -> BoxStream<'_, Result<ExtractionEvent>> {
        let (user, folder) = match target(url) {
            Ok(Target::Gallery { user, folder }) => (user, folder),
            Ok(Target::Following { user }) => return self.following(user),
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
            folder,
            page: 0,
            next: None,
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
        self.request_with(endpoint, None).await
    }

    async fn request_with(
        &self,
        endpoint: &Url,
        form: Option<&[(String, String)]>,
    ) -> Result<Vec<u8>> {
        for attempt in 0..=self.retries {
            match self.attempt(endpoint, form, attempt).await {
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
        form: Option<&[(String, String)]>,
        attempt: u8,
    ) -> std::result::Result<Vec<u8>, Failure> {
        let mut target = endpoint.clone();
        let mut redirects = Redirects::new(endpoint);
        let mut response = loop {
            self.rate_limits.wait(&target, self.timeout).await?;
            // Only the first request of a form submission is a POST.
            let request = match form.filter(|_| target == *endpoint) {
                Some(form) => self.client.post(target.clone()).form(form),
                None => self.client.get(target.clone()),
            };
            let mut request = request
                .timeout(self.request_timeout)
                .header("Accept-Language", "en-US,en;q=0.5")
                .header("Referer", "https://www.furaffinity.net/");
            // Only send session cookies to the configured origin.
            if let Some(cookie) = &self.cookie
                && target.origin() == self.root.origin()
            {
                request = request.header("Cookie", cookie);
            }
            let response = request.send().await.map_err(|_| {
                Failure::transient("Fur Affinity request failed; check network access")
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
                            "Fur Affinity rate limit; retry after {} seconds",
                            delay.as_secs()
                        ),
                        None => "Fur Affinity rate limit; retry later".into(),
                    }
                } else {
                    format!("Fur Affinity returned HTTP {status}")
                },
            ));
        }
        if matches!(status.as_u16(), 404 | 410) {
            return Err(error("Fur Affinity submission or user not found").into());
        }
        if matches!(status.as_u16(), 401 | 403) {
            return Err(error("Fur Affinity refused access").into());
        }
        if !status.is_success() {
            return Err(error(format!("Fur Affinity returned HTTP {status}")).into());
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| Failure::transient("could not read Fur Affinity response"))?
        {
            if body.len() + chunk.len() > MAX_RESPONSE {
                return Err(error("Fur Affinity response exceeded 16 MiB").into());
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

    fn submission_html() -> String {
        r#"<html><head><meta property="og:title" content="OG Title by alice"></head><body>
<div class="submission-id-sub-container">
<div class="submission-title"><h2><p>Big &amp; Bold</p></h2></div>
by <a href="/user/alice/"><strong>Alice</strong></a>
<span class="popup_date" title="Sep 21, 2023 01:23 PM">3 days ago</span>
<span class="rating-box inline mature">Mature</span>
</div>
<a href="//d.furaffinity.net/art/alice/1695300000/1695300000.alice_big.png">Download</a>
</body></html>"#
            .into()
    }

    #[test]
    fn parses_submission_and_gallery() {
        let items = parse_submission(submission_html().as_bytes(), "42").unwrap();
        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(item.filename, "42.png");
        assert_eq!(
            item.url.as_str(),
            "https://d.furaffinity.net/art/alice/1695300000/1695300000.alice_big.png"
        );
        let meta = item.metadata.as_ref().unwrap();
        assert_eq!(
            (meta.post_id.as_str(), meta.media_id.as_str()),
            ("42", "42")
        );
        assert_eq!(meta.caption.as_deref(), Some("Big & Bold"));
        assert_eq!(meta.username.as_deref(), Some("alice"));
        assert_eq!((meta.timestamp, meta.index), (Some(1695302580), 1));
        assert_eq!(
            meta.file.as_ref().unwrap().sensitive_flags[0].as_ref(),
            "mature"
        );
        assert!(item.archive_key().unwrap().starts_with("furaffinity:"));
        assert!(parse_submission(b"<html>System Message</html>", "1").is_err());
        let evil = r#"<a href="https://d.furaffinity.net.evil.example/art/a.png">Download</a>"#;
        assert!(parse_submission(evil.as_bytes(), "1").is_err());

        let gallery = r#"<figure id="sid-3" class="r-general"></figure><figure id="sid-2"></figure><figure id="sid-3"></figure><figure id="sid-x"></figure>"#;
        assert_eq!(parse_gallery(gallery.as_bytes()).unwrap(), ["3", "2"]);
        assert_eq!(
            parse_popup_date("September 1st, 2020 12:05 AM"),
            Some(1598918700)
        );
        assert_eq!(parse_popup_date("yesterday"), None);
    }

    #[test]
    fn favorites_pages() {
        assert_eq!(
            target(&Url::parse("https://www.furaffinity.net/favorites/Mirlinthloth/").unwrap())
                .unwrap(),
            Target::Gallery {
                user: "mirlinthloth".into(),
                folder: Folder::Favorites
            }
        );
        let page = br#"<figure id="sid-1"></figure><form action="/favorites/mirlinthloth/1605617063/next" method="get">
            <button class="button standard" type="submit">Next</button></form>"#;
        assert_eq!(
            favorites_next(page).as_deref(),
            Some("favorites/mirlinthloth/1605617063/next")
        );
        assert_eq!(favorites_next(b"<figure id=\"sid-1\"></figure>"), None);
    }

    #[test]
    fn classifies_urls_and_hosts() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.furaffinity.net/view/42/").unwrap(),
            Target::Submission { id: "42".into() }
        );
        assert_eq!(
            t("https://www.furaffinity.net/full/42").unwrap(),
            Target::Submission { id: "42".into() }
        );
        assert_eq!(
            t("https://www.furaffinity.net/scraps/Alice/2/").unwrap(),
            Target::Gallery {
                user: "alice".into(),
                folder: Folder::Scraps
            }
        );
        assert_eq!(
            t("https://www.furaffinity.net/user/Alice/").unwrap(),
            Target::Gallery {
                user: "alice".into(),
                folder: Folder::Gallery
            }
        );
        assert_eq!(
            t("https://xfuraffinity.net/view/42/").unwrap(),
            Target::Submission { id: "42".into() }
        );
        assert_eq!(
            t("https://www.furaffinity.net/gallery/a/folder/173240/Name/").unwrap(),
            Target::Gallery {
                user: "a".into(),
                folder: Folder::Folder("173240".into())
            }
        );
        assert_eq!(
            t("https://www.furaffinity.net/watchlist/by/a/").unwrap(),
            Target::Following { user: "a".into() }
        );
        assert_eq!(
            t("https://www.furaffinity.net/msg/submissions/new~56789000@48/").unwrap(),
            Target::Gallery {
                user: String::new(),
                folder: Folder::Submissions("msg/submissions/new~56789000@48/".into())
            }
        );
        let Target::Gallery {
            folder: Folder::Search(query),
            ..
        } = t("https://www.furaffinity.net/search/?q=leaf&range=1day").unwrap()
        else {
            panic!("search")
        };
        let form = search_form(&query, 2);
        assert!(form.contains(&("range".into(), "1day".into())));
        assert!(form.contains(&("page".into(), "1".into())));
        assert!(form.contains(&("next_page".into(), "Next".into())));
        for raw in [
            "https://www.furaffinity.net/",
            "https://www.furaffinity.net/view/abc/",
            "https://d.furaffinity.net/art/a/1/1.a.png",
            "https://www.furaffinity.net.evil.example/view/1/",
        ] {
            assert!(t(raw).is_err(), "{raw}");
        }
        assert!(media_url("//d.furaffinity.net/art/a.png").is_some());
        assert!(media_url("https://t.facdn.net/a.png").is_some());
        assert!(media_url("http://d.furaffinity.net/art/a.png").is_none());
        assert!(media_url("https://facdn.net.evil.example/a.png").is_none());
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

    fn furaffinity(root: Url, cookies: CookieJar) -> FurAffinity {
        FurAffinity::from_cookie_jar(cookies, Duration::from_secs(5))
            .unwrap()
            .with_network(&NetworkOptions::new(Some(String::new()), None).unwrap())
            .unwrap()
            .with_root(root)
    }

    #[tokio::test]
    async fn fetches_submission_with_retry_and_cookies() {
        let (root, server) = serve(vec![
            response("503 Service Unavailable", b""),
            response("200 OK", submission_html().as_bytes()),
        ]);
        let jar = CookieJar::parse(
            ".furaffinity.net\tTRUE\t/\tTRUE\t0\ta\tsecret-a\n.furaffinity.net\tTRUE\t/\tTRUE\t0\tb\tsecret-b\n",
        )
        .unwrap();
        let events: Vec<_> = furaffinity(root, jar)
            .with_retries(1)
            .events(&Url::parse("https://www.furaffinity.net/full/42/").unwrap())
            .collect()
            .await;
        assert_eq!(events.len(), 3);
        let ExtractionEvent::Post(post) = events[0].as_ref().unwrap() else {
            panic!("missing source post")
        };
        let KeywordPolicy::Shared { file_fields, .. } = &post.keywords else {
            panic!("FurAffinity must reuse Directory keywords")
        };
        assert_eq!(file_fields.as_ref(), &["num", "index", "media_id"]);
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /view/42/ "));
        assert!(requests[1].contains("a=secret-a"), "{}", requests[1]);
    }

    #[tokio::test]
    async fn paginates_gallery_until_no_new_ids() {
        let (root, server) = serve(vec![
            response(
                "200 OK",
                br#"<figure id="sid-42"></figure><figure id="sid-7"></figure>"#,
            ),
            response("200 OK", submission_html().as_bytes()),
            response("200 OK", b"<html>System Message</html>"),
            response("200 OK", br#"<figure id="sid-42"></figure>"#),
        ]);
        let media: Vec<_> = furaffinity(root, CookieJar::default())
            .stream(&Url::parse("https://www.furaffinity.net/gallery/alice/").unwrap())
            .collect()
            .await;
        let media: Vec<_> = media.into_iter().map(|m| m.unwrap()).collect();
        assert_eq!(media.len(), 1);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /gallery/alice/1/ "));
        assert!(!requests[0].to_ascii_lowercase().contains("cookie:"));
        assert!(requests[2].starts_with("GET /view/7/ "));
        assert!(requests[3].starts_with("GET /gallery/alice/2/ "));
    }
}
