//! Experimental native Newgrounds extraction.
//!
//! Mirrors the art parts of gallery_dl/extractor/newgrounds.py. Art posts
//! (`www.newgrounds.com/art/view/USER/SLUG`) are scraped from page HTML: the
//! main image from `<div class="image">` (falling back to `og:image`), extra
//! images from the `imageData` JSON and `art-images` block of multi-image
//! posts, the title from `og:title` and the date from
//! `itemprop="datePublished"`. User art listings (`USER.newgrounds.com/art`)
//! page through `art/page/N` with `X-Requested-With: XMLHttpRequest` (capped
//! at 200 pages) and fetch every listed post. Audio and portal pages
//! (`/audio/listen/ID`, `/portal/view/ID`, with `portal/video` JSON for
//! movies), user audio/movies/games listings, favorites, following and
//! search follow upstream. There is no login, so adult-rated posts are
//! skipped by the site. Media must be HTTPS on `*.ngfiles.com` or
//! `*.ungrounded.net`.
use crate::{
    Result, error,
    event::{ExtractionEvent, group},
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
use serde_json::Value;
use std::{
    collections::{BTreeMap, VecDeque},
    sync::Arc,
    time::{Duration, SystemTime},
};
use url::Url;

const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:128.0) Gecko/20100101 Firefox/128.0";
const MAX_RESPONSE: usize = 16 * 1024 * 1024;
const MAX_PAGES: u64 = 200;
/// Subdomains that are not user pages.
const RESERVED_HOSTS: [&str; 6] = ["www", "art", "uploads", "img", "api", "css"];

/// Media IDs are `POSTID_INDEX`.
pub fn valid_id(id: &str) -> bool {
    id.split_once('_').is_some_and(|(post, index)| {
        [post, index]
            .iter()
            .all(|p| (1..=20).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_digit()))
    })
}

fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Post {
        user: String,
        slug: String,
    },
    /// `/portal/view/ID` (movies, games) or `/audio/listen/ID`.
    Media {
        path: String,
    },
    /// `USER.newgrounds.com/{art,audio,movies,games}`; the bare user page
    /// dispatches to `art` like upstream.
    User {
        user: String,
        kind: &'static str,
    },
    /// `USER.newgrounds.com/favorites[/KIND]`.
    Favorites {
        user: String,
        kind: Option<&'static str>,
    },
    /// `USER.newgrounds.com/favorites/following`: queued user galleries.
    Following {
        user: String,
    },
    /// `www.newgrounds.com/search/conduct/KIND?QUERY`.
    Search {
        kind: String,
        query: Vec<(String, String)>,
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
            "unsupported Newgrounds URL; expected https://www.newgrounds.com/art/view/USER/SLUG or https://USER.newgrounds.com/art",
        )
    };
    if !plain(url) {
        return Err(unsupported());
    }
    let parts: Vec<&str> = url
        .path_segments()
        .map(|s| s.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    let digits =
        |id: &str| !id.is_empty() && id.len() <= 20 && id.bytes().all(|b| b.is_ascii_digit());
    let kind = |name: &str| match name {
        "art" => Some("art"),
        "audio" => Some("audio"),
        "movies" => Some("movies"),
        "games" => Some("games"),
        _ => None,
    };
    match url.host_str().ok_or_else(unsupported)? {
        "www.newgrounds.com" | "newgrounds.com" => match parts[..] {
            ["art", "view", user, slug] if valid_name(user) && valid_name(slug) => {
                Ok(Target::Post {
                    user: user.to_ascii_lowercase(),
                    slug: slug.into(),
                })
            }
            ["portal", "view", id, ..] | ["audio", "listen", id, ..] if digits(id) => {
                Ok(Target::Media {
                    path: format!("{}/{}/{id}", parts[0], parts[1]),
                })
            }
            ["search", "conduct", kind] if url.query().is_some() => Ok(Target::Search {
                kind: kind.to_owned(),
                query: url
                    .query_pairs()
                    .map(|(k, v)| (k.into_owned(), v.into_owned()))
                    .collect(),
            }),
            _ => Err(unsupported()),
        },
        // Upstream `NewgroundsImageExtractor` also accepts image files.
        "art.ngfiles.com" => match parts[..] {
            ["images", _, file] => {
                let stem = file.split('.').next().unwrap_or_default();
                let mut fields = stem.splitn(3, '_');
                match (fields.next(), fields.next(), fields.next()) {
                    (Some(id), Some(user), Some(slug))
                        if digits(id) && valid_name(user) && valid_name(slug) =>
                    {
                        Ok(Target::Post {
                            user: user.to_ascii_lowercase(),
                            slug: slug.to_owned(),
                        })
                    }
                    _ => Err(unsupported()),
                }
            }
            _ => Err(unsupported()),
        },
        host => {
            let user = host
                .strip_suffix(".newgrounds.com")
                .filter(|u| valid_name(u) && !RESERVED_HOSTS.contains(u))
                .ok_or_else(unsupported)?
                .to_ascii_lowercase();
            match parts[..] {
                [] => Ok(Target::User { user, kind: "art" }),
                [name, ..] if kind(name).is_some() => Ok(Target::User {
                    user,
                    kind: kind(name).unwrap_or("art"),
                }),
                ["favorites", "following", ..] => Ok(Target::Following { user }),
                ["favorites"] => Ok(Target::Favorites { user, kind: None }),
                ["favorites", name, ..] if kind(name).is_some_and(|k| k != "games") => {
                    Ok(Target::Favorites {
                        user,
                        kind: kind(name),
                    })
                }
                _ => Err(unsupported()),
            }
        }
    }
}

pub fn is_newgrounds_url(url: &Url) -> bool {
    target(url).is_ok()
}

/// HTTPS media on `*.ngfiles.com` or `*.ungrounded.net`.
pub fn media_url(raw: &str) -> Option<Url> {
    let raw = unescape(raw.trim());
    let raw = match raw.strip_prefix("//") {
        Some(rest) => format!("https://{rest}"),
        None => raw,
    };
    let url = Url::parse(&raw).ok()?;
    // Images and audio on ngfiles.com, movies on ungrounded.net.
    let host = url.host_str()?;
    let label = host
        .strip_suffix(".ngfiles.com")
        .or_else(|| host.strip_suffix(".ungrounded.net"))?;
    (url.scheme() == "https"
        && plain(&url)
        && !label.is_empty()
        && label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.')))
    .then_some(url)
}

fn itemprop<'a>(html: &'a str, name: &str) -> Option<&'a str> {
    for quote in ['"', '\''] {
        let needle = format!("itemprop={quote}{name}{quote}");
        if let Some(at) = html.find(&needle) {
            return tag_around(html, at).and_then(|tag| attribute(tag, "content"));
        }
    }
    None
}

fn image_urls(html: &str) -> Vec<Url> {
    let mut urls: Vec<Url> = Vec::new();
    let mut push = |raw: &str| {
        if let Some(url) = media_url(raw)
            && !urls.contains(&url)
        {
            urls.push(url);
        }
    };
    let main = between(html, "<div class=\"image\"", "</div>").and_then(|block| {
        between(block, "href=\"", "\"")
            .filter(|h| media_url(h).is_some())
            .or_else(|| between(block, "src=\"", "\""))
    });
    if let Some(main) = main.or_else(|| meta_property(html, "og:image")) {
        push(main);
    }
    if let Some(data) = between(html, "let imageData =", "];")
        && let Ok(Value::Array(images)) =
            serde_json::from_str::<Value>(&format!("{}]", data.trim()))
    {
        for image in images {
            if let Some(raw) = image.get("image").and_then(Value::as_str) {
                push(raw);
            }
        }
    }
    if let Some(block) = between(html, "class=\"art-images\"", "</div>") {
        for raw in between_all(block, "data-smartload-src=\"", "\"") {
            push(raw);
        }
        for raw in between_all(block, " src=\"", "\"") {
            push(raw);
        }
    }
    urls
}

/// Leading numeric ID of `images/NNN/NNNNNNN_user_slug.ext`.
fn image_index(url: &Url) -> Option<String> {
    let name = url.path_segments()?.next_back()?;
    let digits: String = name
        .bytes()
        .take_while(u8::is_ascii_digit)
        .map(char::from)
        .collect();
    (1..=20).contains(&digits.len()).then_some(digits)
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

/// Parse an art post page into its images.
pub fn parse_post(bytes: &[u8], user: &str) -> Result<Vec<Media>> {
    let html = std::str::from_utf8(bytes).map_err(|_| error("Newgrounds returned invalid HTML"))?;
    let urls = image_urls(html);
    let post_id = urls
        .first()
        .and_then(image_index)
        .ok_or_else(|| error("Newgrounds post contains no images (adult posts need a login)"))?;
    let caption = meta_property(html, "og:title")
        .and_then(clean_text)
        .map(Arc::<str>::from);
    let timestamp = itemprop(html, "datePublished")
        .or_else(|| between(html, "\"datePublished\":\"", "\""))
        .and_then(|d| crate::date::parse_date(d.trim()).ok());
    let username: Arc<str> = Arc::from(user);
    Ok(urls
        .into_iter()
        .enumerate()
        .map(|(n, url)| {
            let index = n + 1;
            Media {
                filename: format!("{post_id}_{index}.{}", extension(&url)),
                job: MediaJob::DirectHttp,
                category: "newgrounds".into(),
                metadata: Some(PostMetadata {
                    file: Some(FileMetadata {
                        media_type: Some(Arc::from("photo")),
                        ..FileMetadata::default()
                    }),
                    post_id: post_id.clone(),
                    media_id: format!("{post_id}_{index}"),
                    username: Some(username.clone()),
                    caption: caption.clone(),
                    timestamp,
                    index,
                    ..PostMetadata::default()
                }),
                url,
                headers: BTreeMap::new(),
                handoff: BTreeMap::new(),
            }
        })
        .collect())
}

/// Upstream `_extract_audio_data`/`_extract_media_data`: the file of an
/// audio or portal page (`src` may come from `portal/video`).
pub fn parse_media(html: &str, path: &str, video: Option<&Value>) -> Result<Vec<Media>> {
    let index = path.rsplit('/').next().unwrap_or_default().to_owned();
    let audio = path.starts_with("audio/");
    let mut candidates: Vec<String> = Vec::new();
    if let Some(src) = between(html, "{\"url\":\"", "\"").filter(|s| !s.is_empty()) {
        candidates.push(src.replace("\\/", "/"));
    } else if let Some(sources) = video.and_then(|v| v.get("sources")) {
        // Original first (the 360p name without its resolution), then the
        // largest listed resolutions, like upstream's default `format`.
        if let Some(base) = sources
            .pointer("/360p/0/src")
            .and_then(Value::as_str)
            .map(|s| s.replace("\\/", "/"))
        {
            let regex = crate::sites::gdl::re!(r"\.360p\.\w+");
            for ext in ["mp4", "webm", "m4v", "mov", "mkv"] {
                candidates.push(regex.replacen(&base, 1, format!(".{ext}")).into_owned());
            }
        }
        for size in ["1080p", "720p", "360p"] {
            if let Some(src) = sources
                .get(size)
                .and_then(|v| v.pointer("/0/src"))
                .and_then(Value::as_str)
            {
                candidates.push(src.replace("\\/", "/"));
            }
        }
    }
    let mut urls = candidates.iter().filter_map(|raw| media_url(raw));
    let url = urls.next().ok_or_else(|| {
        error("Newgrounds media has no downloadable file (adult posts need a login)")
    })?;
    let fallback: Vec<Value> = urls.map(|u| Value::String(u.to_string())).collect();
    let caption = meta_property(html, "og:title")
        .and_then(clean_text)
        .map(Arc::<str>::from);
    let timestamp =
        itemprop(html, "datePublished").and_then(|d| crate::date::parse_date(d.trim()).ok());
    let username = between(html, "<div class=\"item-user\">", ">")
        .and_then(|tag| between(tag, "//", "."))
        .filter(|u| valid_name(u))
        .map(Arc::<str>::from);
    // Audio and portal IDs are separate namespaces (upstream `_type`).
    let media_id = if audio {
        format!("{index}_0")
    } else {
        format!("{index}_00")
    };
    let mut handoff = BTreeMap::new();
    if !fallback.is_empty() {
        handoff.insert("_fallback".to_owned(), Value::Array(fallback));
    }
    Ok(vec![Media {
        filename: format!("{index}.{}", extension(&url)),
        job: MediaJob::DirectHttp,
        category: "newgrounds".into(),
        metadata: Some(PostMetadata {
            file: Some(FileMetadata {
                media_type: Some(Arc::from(if audio { "audio" } else { "video" })),
                ..FileMetadata::default()
            }),
            post_id: index,
            media_id,
            username,
            caption,
            timestamp,
            index: 1,
            ..PostMetadata::default()
        }),
        url,
        headers: BTreeMap::new(),
        handoff,
    }])
}

/// Post links (`href="..."`) of an HTML fragment, made absolute.
fn post_links(html: &str) -> Vec<String> {
    let mut links = Vec::new();
    for href in between_all(html, "href=\"", "\"") {
        let href = match href.strip_prefix("//") {
            Some(rest) => format!("https://{rest}"),
            None if href.starts_with('/') => format!("https://www.newgrounds.com{href}"),
            None => href.to_owned(),
        };
        if let Ok(url) = Url::parse(&href)
            && matches!(target(&url), Ok(Target::Post { .. } | Target::Media { .. }))
            && !links.contains(&href)
        {
            links.push(href);
        }
    }
    links
}

/// Parse one `art/page/N` response into (user, slug) posts and `more`.
pub fn parse_listing(bytes: &[u8]) -> Result<(Vec<(String, String)>, bool)> {
    let body: Value =
        serde_json::from_slice(bytes).map_err(|_| error("Newgrounds returned invalid JSON"))?;
    let mut posts = Vec::new();
    if let Some(years) = body.get("items").and_then(Value::as_object) {
        for fragment in years.values().filter_map(Value::as_array).flatten() {
            let Some(href) = fragment.as_str().and_then(|f| between(f, "href=\"", "\"")) else {
                continue;
            };
            let href = match href.strip_prefix("//") {
                Some(rest) => format!("https://{rest}"),
                None => href.to_owned(),
            };
            if let Ok(Target::Post { user, slug }) = Url::parse(&href)
                .map_err(|_| ())
                .and_then(|u| target(&u).map_err(|_| ()))
            {
                let post = (user, slug);
                if !posts.contains(&post) {
                    posts.push(post);
                }
            }
        }
    }
    let more = match body.get("more") {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => !s.trim().is_empty(),
        Some(Value::Null) | None => false,
        Some(_) => true,
    };
    Ok((posts, more))
}

pub struct Newgrounds {
    client: Client,
    network: NetworkOptions,
    root: Url,
    user_root: Option<Url>,
    timeout: Duration,
    request_timeout: Duration,
    retries: u8,
    rate_limits: RateLimits,
}

struct ListingState {
    user: String,
    kind: &'static str,
    page: u64,
    done: bool,
    pending: VecDeque<ExtractionEvent>,
}

impl Newgrounds {
    pub fn new(timeout: Duration) -> Result<Self> {
        Ok(Self {
            client: NetworkOptions::default().client(USER_AGENT, timeout)?,
            network: NetworkOptions::default(),
            root: Url::parse("https://www.newgrounds.com/").expect("static URL"),
            user_root: None,
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

    /// Override the www and user-page origins; intended for local tests.
    pub fn with_roots(mut self, root: Url, user_root: Url) -> Self {
        self.root = root;
        self.user_root = Some(user_root);
        self
    }

    fn user_root(&self, user: &str) -> Url {
        self.user_root.clone().unwrap_or_else(|| {
            Url::parse(&format!("https://{user}.newgrounds.com/")).expect("validated user")
        })
    }

    async fn call(&self, endpoint: Url, ajax: bool) -> Result<Vec<u8>> {
        tokio::time::timeout(self.timeout, self.request(&endpoint, ajax))
            .await
            .map_err(|_| error("native Newgrounds extraction timed out"))?
    }

    async fn post(&self, user: &str, slug: &str) -> Result<VecDeque<ExtractionEvent>> {
        let endpoint = self
            .root
            .join(&format!("art/view/{user}/{slug}"))
            .map_err(|_| error("invalid Newgrounds path"))?;
        let body = self.call(endpoint, false).await?;
        let mut items = parse_post(&body, user)?;
        for item in &mut items {
            self.network.apply_media(item);
        }
        Ok(group(items, "newgrounds"))
    }

    /// Upstream `extract_post` for an art, audio or portal page URL.
    async fn by_url(&self, link: &str) -> Result<VecDeque<ExtractionEvent>> {
        let url = Url::parse(link).map_err(|_| error("invalid Newgrounds link"))?;
        match target(&url)? {
            Target::Post { user, slug } => self.post(&user, &slug).await,
            Target::Media { path } => self.media(&path).await,
            _ => Err(error("unsupported Newgrounds post link")),
        }
    }

    /// Audio and portal pages (`flash` format for games, like upstream).
    async fn media(&self, path: &str) -> Result<VecDeque<ExtractionEvent>> {
        let page_path = if path.starts_with("portal/") {
            format!("{path}/format/flash")
        } else {
            path.to_owned()
        };
        let endpoint = self
            .root
            .join(&page_path)
            .map_err(|_| error("invalid Newgrounds path"))?;
        let body = self.call(endpoint, false).await?;
        let html = String::from_utf8_lossy(&body).into_owned();
        let has_source = between(&html, "{\"url\":\"", "\"").is_some_and(|s| !s.is_empty());
        let video = if !has_source && path.starts_with("portal/") {
            let index = path.rsplit('/').next().unwrap_or_default();
            let endpoint = self
                .root
                .join(&format!("portal/video/{index}"))
                .map_err(|_| error("invalid Newgrounds path"))?;
            serde_json::from_slice::<Value>(&self.call(endpoint, true).await?).ok()
        } else {
            None
        };
        let mut items = parse_media(&html, path, video.as_ref())?;
        for item in &mut items {
            self.network.apply_media(item);
        }
        Ok(group(items, "newgrounds"))
    }

    async fn next_page(&self, state: &mut ListingState) -> Result<()> {
        state.page += 1;
        let endpoint = self
            .user_root(&state.user)
            .join(&format!("{}/page/{}", state.kind, state.page))
            .map_err(|_| error("invalid Newgrounds path"))?;
        let body = self.call(endpoint, true).await?;
        let value: Value =
            serde_json::from_slice(&body).map_err(|_| error("Newgrounds returned invalid JSON"))?;
        let mut links = Vec::new();
        if let Some(years) = value.get("items").and_then(Value::as_object) {
            for fragment in years.values().filter_map(Value::as_array).flatten() {
                for link in post_links(fragment.as_str().unwrap_or_default()) {
                    if !links.contains(&link) {
                        links.push(link);
                    }
                }
            }
        }
        let more = match value.get("more").or_else(|| value.get("load_more")) {
            Some(Value::Bool(b)) => *b,
            Some(Value::String(s)) => !s.trim().is_empty(),
            Some(Value::Null) | None => false,
            Some(_) => true,
        };
        state.done = links.is_empty() || !more || state.page >= MAX_PAGES;
        for link in links {
            match self.by_url(&link).await {
                Ok(events) => state.pending.extend(events),
                // Adult or removed posts do not abort the whole listing.
                Err(e)
                    if e.to_string().contains("contains no images")
                        || e.to_string().contains("no downloadable file") => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Upstream `_pagination_favorites` (24 entries per page) for one kind;
    /// `following` yields user URLs instead of posts.
    fn favorites(
        &self,
        user: String,
        kind: &'static str,
    ) -> BoxStream<'_, Result<ExtractionEvent>> {
        stream::try_unfold(Some(1u64), move |page| {
            let user = user.clone();
            async move {
                let Some(page) = page.filter(|p| *p <= MAX_PAGES) else {
                    return Ok(None);
                };
                let mut endpoint = self
                    .user_root(&user)
                    .join(&format!("favorites/{kind}"))
                    .map_err(|_| error("invalid Newgrounds path"))?;
                endpoint
                    .query_pairs_mut()
                    .append_pair("page", &page.to_string())
                    .append_pair("isAjaxRequest", "1");
                let value: Value = serde_json::from_slice(&self.call(endpoint, true).await?)
                    .map_err(|_| error("Newgrounds returned invalid JSON"))?;
                let component = value
                    .get("component")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                if kind == "following" {
                    let users: Vec<String> =
                        between_all(&component, "class=\"item-user", "\"><img")
                            .filter_map(|tag| tag.rsplit('"').next())
                            .filter_map(|href| {
                                let href =
                                    href.trim_start_matches("https:").trim_start_matches("//");
                                let name = href.split('.').next()?;
                                valid_name(name).then(|| name.to_ascii_lowercase())
                            })
                            .collect();
                    let next = (users.len() >= 24).then_some(page + 1);
                    let children: Vec<_> = users
                        .into_iter()
                        .map(|name| {
                            crate::event::child_job(
                                crate::event::ChildMetadata::from_raw(
                                    "newgrounds",
                                    "following",
                                    &serde_json::Map::new(),
                                ),
                                move || {
                                    let url =
                                        Url::parse(&format!("https://{name}.newgrounds.com/"))
                                            .expect("validated user");
                                    self.events(&url)
                                },
                            )
                        })
                        .collect();
                    return Ok::<_, crate::Error>(Some((
                        stream::iter(children).flatten().boxed(),
                        next,
                    )));
                }
                let links: Vec<String> =
                    between_all(&component, "href=\"https://www.newgrounds.com/", "\"")
                        .map(|path| format!("https://www.newgrounds.com/{path}"))
                        .filter(|link| {
                            Url::parse(link).is_ok_and(|u| {
                                matches!(target(&u), Ok(Target::Post { .. } | Target::Media { .. }))
                            })
                        })
                        .collect();
                let next = (links.len() >= 24).then_some(page + 1);
                Ok(Some((self.links(links), next)))
            }
        })
        .map(|result| match result {
            Ok(events) => events,
            Err(e) => stream::once(async move { Err(e) }).boxed(),
        })
        .flatten()
        .boxed()
    }

    /// Posts for links, skipping adult or removed ones.
    fn links(&self, links: Vec<String>) -> BoxStream<'_, Result<ExtractionEvent>> {
        stream::iter(links)
            .then(move |link| async move {
                match self.by_url(&link).await {
                    Err(e)
                        if e.to_string().contains("contains no images")
                            || e.to_string().contains("no downloadable file") =>
                    {
                        Ok(VecDeque::new())
                    }
                    result => result,
                }
            })
            .map(|result| match result {
                Ok(events) => stream::iter(events.into_iter().map(Ok)).boxed(),
                Err(e) => stream::once(async move { Err(e) }).boxed(),
            })
            .flatten()
            .boxed()
    }

    /// Upstream `_pagination_search`: `inner=1` JSON pages until no links.
    fn search(
        &self,
        kind: String,
        query: Vec<(String, String)>,
    ) -> BoxStream<'_, Result<ExtractionEvent>> {
        let start = query
            .iter()
            .find(|(k, _)| k == "page")
            .and_then(|(_, v)| v.parse::<u64>().ok())
            .unwrap_or(1);
        stream::try_unfold(Some(start), move |page| {
            let (kind, query) = (kind.clone(), query.clone());
            async move {
                let Some(page) = page.filter(|p| *p < start + MAX_PAGES) else {
                    return Ok(None);
                };
                let mut endpoint = self
                    .root
                    .join(&format!("search/conduct/{kind}"))
                    .map_err(|_| error("invalid Newgrounds path"))?;
                {
                    let mut pairs = endpoint.query_pairs_mut();
                    for (key, value) in query.iter().filter(|(k, _)| k != "page" && k != "inner") {
                        pairs.append_pair(key, value);
                    }
                    pairs
                        .append_pair("inner", "1")
                        .append_pair("page", &page.to_string());
                }
                let value: Value = serde_json::from_slice(&self.call(endpoint, true).await?)
                    .map_err(|_| error("Newgrounds returned invalid JSON"))?;
                let content = value
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let links: Vec<String> = post_links(content);
                if links.is_empty() {
                    return Ok(None);
                }
                Ok::<_, crate::Error>(Some((self.links(links), Some(page + 1))))
            }
        })
        .map(|result| match result {
            Ok(events) => events,
            Err(e) => stream::once(async move { Err(e) }).boxed(),
        })
        .flatten()
        .boxed()
    }

    pub fn stream(&self, url: &Url) -> BoxStream<'_, Result<Media>> {
        crate::event::media_only(self.events(url))
    }

    /// Emit one post boundary per art post; listings paginate lazily.
    pub fn events(&self, url: &Url) -> BoxStream<'_, Result<ExtractionEvent>> {
        let (user, kind) = match target(url) {
            Ok(Target::User { user, kind }) => (user, kind),
            Ok(Target::Media { path }) => {
                return stream::once(async move { self.media(&path).await })
                    .map(|result| match result {
                        Ok(events) => stream::iter(events.into_iter().map(Ok)).boxed(),
                        Err(e) => stream::once(async move { Err(e) }).boxed(),
                    })
                    .flatten()
                    .boxed();
            }
            Ok(Target::Favorites { user, kind }) => {
                return match kind {
                    Some(kind) => self.favorites(user, kind),
                    None => stream::iter(["art", "audio", "movies"])
                        .flat_map(move |kind| self.favorites(user.clone(), kind))
                        .boxed(),
                };
            }
            Ok(Target::Following { user }) => return self.favorites(user, "following"),
            Ok(Target::Search { kind, query }) => return self.search(kind, query),
            Ok(Target::Post { user, slug }) => {
                return stream::once(async move { self.post(&user, &slug).await })
                    .map(|result| match result {
                        Ok(events) => stream::iter(events.into_iter().map(Ok)).boxed(),
                        Err(e) => stream::once(async move { Err(e) }).boxed(),
                    })
                    .flatten()
                    .boxed();
            }
            Err(e) => return stream::once(async move { Err(e) }).boxed(),
        };
        let state = ListingState {
            user,
            kind,
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

    async fn request(&self, endpoint: &Url, ajax: bool) -> Result<Vec<u8>> {
        for attempt in 0..=self.retries {
            match self.attempt(endpoint, ajax, attempt).await {
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
        ajax: bool,
        attempt: u8,
    ) -> std::result::Result<Vec<u8>, Failure> {
        let mut target = endpoint.clone();
        let mut redirects = Redirects::new(endpoint);
        let mut response = loop {
            self.rate_limits.wait(&target, self.timeout).await?;
            let mut request = self
                .client
                .get(target.clone())
                .timeout(self.request_timeout)
                .header("Accept-Language", "en-US,en;q=0.5")
                .header("Referer", "https://www.newgrounds.com/");
            if ajax {
                request = request
                    .header("X-Requested-With", "XMLHttpRequest")
                    .header("Accept", "application/json, text/javascript, */*; q=0.01");
            }
            let response = request.send().await.map_err(|_| {
                Failure::transient("Newgrounds request failed; check network access")
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
                            "Newgrounds rate limit; retry after {} seconds",
                            delay.as_secs()
                        ),
                        None => "Newgrounds rate limit; retry later".into(),
                    }
                } else {
                    format!("Newgrounds returned HTTP {status}")
                },
            ));
        }
        if matches!(status.as_u16(), 404 | 410) {
            return Err(error("Newgrounds post or user not found").into());
        }
        if matches!(status.as_u16(), 401 | 403) {
            return Err(error("Newgrounds refused access").into());
        }
        if !status.is_success() {
            return Err(error(format!("Newgrounds returned HTTP {status}")).into());
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| Failure::transient("could not read Newgrounds response"))?
        {
            if body.len() + chunk.len() > MAX_RESPONSE {
                return Err(error("Newgrounds response exceeded 16 MiB").into());
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
        thread::{self, JoinHandle},
    };

    fn post_html() -> String {
        r#"<html><head>
<meta property="og:title" content="Big &amp; Bold">
<meta property="og:image" content="https://art.ngfiles.com/thumbnails/1/1234567.png">
<meta itemprop="datePublished" content="2024-01-01T00:00:00-00:00">
</head><body>
<div class="image"><a href="https://art.ngfiles.com/images/1/1234567_alice_ship.png?f1"><img src="x"></a></div>
<script>let imageData = [{"image":"https:\/\/art.ngfiles.com\/images\/1\/1234567_alice_ship.png?f1"},{"image":"https:\/\/art.ngfiles.com\/images\/1\/1234567_9_alice_ship.jpeg"},{"image":"https:\/\/evil.example\/a.png"}];</script>
<div class="art-images"><img data-smartload-src="http://art.ngfiles.com/images/1/x.png"></div>
</body></html>"#
            .into()
    }

    #[test]
    fn parses_post_and_listing() {
        let items = parse_post(post_html().as_bytes(), "alice").unwrap();
        let names: Vec<_> = items.iter().map(|m| m.filename.as_str()).collect();
        assert_eq!(names, ["1234567_1.png", "1234567_2.jpg"]);
        let meta = items[1].metadata.as_ref().unwrap();
        assert_eq!(
            (meta.post_id.as_str(), meta.media_id.as_str()),
            ("1234567", "1234567_2")
        );
        assert_eq!(meta.caption.as_deref(), Some("Big & Bold"));
        assert_eq!((meta.timestamp, meta.index), (Some(1704067200), 2));
        assert_eq!(meta.username.as_deref(), Some("alice"));
        assert!(items[0].archive_key().unwrap().starts_with("newgrounds:"));
        assert!(parse_post(b"<html>login required</html>", "alice").is_err());

        let fallback =
            r#"<meta property="og:image" content="https://art.ngfiles.com/images/2/42_bob_x.gif">"#;
        assert_eq!(
            parse_post(fallback.as_bytes(), "bob").unwrap()[0].filename,
            "42_1.gif"
        );

        let listing = json!({"items": {"2024": [
            "<a href=\"https://www.newgrounds.com/art/view/alice/ship\" class=\"item\">",
            "<a href=\"//www.newgrounds.com/art/view/alice/boat\">",
            "<a href=\"https://www.newgrounds.com/portal/view/1\">"]}, "more": "x"});
        let (posts, more) = parse_listing(listing.to_string().as_bytes()).unwrap();
        assert_eq!(
            posts,
            [
                ("alice".into(), "ship".into()),
                ("alice".into(), "boat".into())
            ]
        );
        assert!(more);
    }

    #[test]
    fn parses_audio_and_movie_pages() {
        let audio = r#"<meta property="og:title" content="Song">
<div class="item-user"><a href="//tomfulp.newgrounds.com">
<script>var player = {"url":"https:\/\/audio.ngfiles.com\/609000\/609768_song.mp3?f1"}</script>"#;
        let media = parse_media(audio, "audio/listen/609768", None).unwrap();
        assert_eq!(
            media[0].url.as_str(),
            "https://audio.ngfiles.com/609000/609768_song.mp3?f1"
        );
        let meta = media[0].metadata.as_ref().unwrap();
        assert_eq!(
            (meta.post_id.as_str(), meta.media_id.as_str()),
            ("609768", "609768_0")
        );
        assert_eq!(meta.username.as_deref(), Some("tomfulp"));
        assert_eq!(meta.caption.as_deref(), Some("Song"));

        let video = json!({"sources": {
            "360p": [{"src": "https://uploads.ungrounded.net/a/595355.360p.mp4?1"}],
            "720p": [{"src": "https://uploads.ungrounded.net/a/595355.720p.mp4?1"}],
        }});
        let media = parse_media("<html></html>", "portal/view/595355", Some(&video)).unwrap();
        assert_eq!(
            media[0].url.as_str(),
            "https://uploads.ungrounded.net/a/595355.mp4?1"
        );
        let fallback = media[0].handoff["_fallback"].as_array().unwrap();
        assert!(
            fallback
                .iter()
                .any(|u| u == "https://uploads.ungrounded.net/a/595355.720p.mp4?1")
        );
        assert_eq!(media[0].metadata.as_ref().unwrap().media_id, "595355_00");
        assert!(parse_media("<html></html>", "portal/view/1", None).is_err());
    }

    #[test]
    fn classifies_urls_and_hosts() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.newgrounds.com/art/view/Alice/ship").unwrap(),
            Target::Post {
                user: "alice".into(),
                slug: "ship".into()
            }
        );
        assert_eq!(
            t("https://alice.newgrounds.com/art/").unwrap(),
            Target::User {
                user: "alice".into(),
                kind: "art"
            }
        );
        for (raw, kind) in [
            ("https://alice.newgrounds.com/", "art"),
            ("https://alice.newgrounds.com/audio?page=3", "audio"),
            ("https://alice.newgrounds.com/movies/", "movies"),
            ("https://alice.newgrounds.com/games", "games"),
        ] {
            assert_eq!(
                t(raw).unwrap(),
                Target::User {
                    user: "alice".into(),
                    kind
                },
                "{raw}"
            );
        }
        assert_eq!(
            t("https://www.newgrounds.com/portal/view/161181/format/flash").unwrap(),
            Target::Media {
                path: "portal/view/161181".into()
            }
        );
        assert_eq!(
            t("https://www.newgrounds.com/audio/listen/609768").unwrap(),
            Target::Media {
                path: "audio/listen/609768".into()
            }
        );
        assert_eq!(
            t("https://alice.newgrounds.com/favorites/art?page=3").unwrap(),
            Target::Favorites {
                user: "alice".into(),
                kind: Some("art")
            }
        );
        assert_eq!(
            t("https://alice.newgrounds.com/favorites/").unwrap(),
            Target::Favorites {
                user: "alice".into(),
                kind: None
            }
        );
        assert_eq!(
            t("https://alice.newgrounds.com/favorites/following").unwrap(),
            Target::Following {
                user: "alice".into()
            }
        );
        assert!(matches!(
            t("https://www.newgrounds.com/search/conduct/art?terms=tree").unwrap(),
            Target::Search { ref kind, .. } if kind == "art"
        ));
        assert_eq!(
            t("https://art.ngfiles.com/images/1/1234567_alice_ship.png?f1").unwrap(),
            Target::Post {
                user: "alice".into(),
                slug: "ship".into()
            }
        );
        for raw in [
            "https://www.newgrounds.com/",
            "https://www.newgrounds.com/art",
            "https://www.newgrounds.com/art/view/alice",
            "https://art.ngfiles.com/images/1/1_a.png",
            "https://www.newgrounds.com.evil.example/art/view/a/b",
        ] {
            assert!(t(raw).is_err(), "{raw}");
        }
        assert!(media_url("https://art.ngfiles.com/a.png").is_some());
        assert!(media_url("http://art.ngfiles.com/a.png").is_none());
        assert!(media_url("https://ngfiles.com.evil.example/a.png").is_none());
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

    fn newgrounds(root: Url) -> Newgrounds {
        Newgrounds::new(Duration::from_secs(5))
            .unwrap()
            .with_network(&NetworkOptions::new(Some(String::new()), None).unwrap())
            .unwrap()
            .with_roots(root.clone(), root)
    }

    #[tokio::test]
    async fn fetches_post_with_retry() {
        let (root, server) = serve(vec![
            response("503 Service Unavailable", b""),
            response("200 OK", post_html().as_bytes()),
        ]);
        let media: Vec<_> = newgrounds(root)
            .with_retries(1)
            .stream(&Url::parse("https://www.newgrounds.com/art/view/alice/ship").unwrap())
            .collect()
            .await;
        assert_eq!(media.len(), 2);
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /art/view/alice/ship "));
    }

    #[tokio::test]
    async fn paginates_user_art() {
        let page = |slug: &str, more: bool| {
            json!({"items": {"2024": [format!("<a href=\"https://www.newgrounds.com/art/view/alice/{slug}\">")]}, "more": more})
                .to_string()
        };
        let (root, server) = serve(vec![
            response("200 OK", page("ship", true).as_bytes()),
            response("200 OK", post_html().as_bytes()),
            response("200 OK", page("adult", false).as_bytes()),
            response("200 OK", b"<html>login</html>"),
        ]);
        let media: Vec<_> = newgrounds(root)
            .stream(&Url::parse("https://alice.newgrounds.com/art").unwrap())
            .collect()
            .await;
        let media: Vec<_> = media.into_iter().map(|m| m.unwrap()).collect();
        assert_eq!(media.len(), 2);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /art/page/1 "));
        assert!(
            requests[0]
                .to_ascii_lowercase()
                .contains("x-requested-with: xmlhttprequest")
        );
        assert!(requests[2].starts_with("GET /art/page/2 "));
        assert!(requests[3].starts_with("GET /art/view/alice/adult "));
    }
}
