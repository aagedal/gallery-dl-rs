//! Experimental native Newgrounds extraction.
//!
//! Mirrors gallery_dl/extractor/newgrounds.py: the `image`
//! (`www.newgrounds.com/art/view/USER/SLUG` and `art.ngfiles.com` image
//! URLs), `media` (`/portal/view/ID`, `/audio/listen/ID`), `art`, `audio`,
//! `movies`, `games`, `favorite`, `following`, `search` and `user` (`include`
//! dispatch of `art`/`audio`/`games`/`movies`) classes. Post pages are
//! scraped like upstream's `extract_post`: art posts use `full_image_text`
//! plus `imageData`/`art-images` extra images, audio the page's file URL,
//! and movies the `portal/video/ID` sources picked by the `format` option
//! (other formats become download fallbacks); games and flash use the page's
//! file (`flash` option appends `/format/flash`). Images embedded in the
//! author comments follow. Files carry upstream's keyword dictionary (title,
//! description, date, rating, score, favorites, sorted tags, artists,
//! comment, `_type`/`_index`, `num`) with default
//! `{category}_{_index}_{title}.{extension}` filenames in
//! `newgrounds/{artist[:10]:J, }/`. Listings page lazily (capped at 200
//! pages). Session cookies (`ng_session`) from `--cookies` are sent to
//! newgrounds.com so adult-rated posts work; posts behind the adult wall
//! are skipped otherwise. Media must be HTTPS on `*.ngfiles.com` or
//! `*.ungrounded.net`.
use crate::{
    Result,
    cookies::CookieJar,
    error,
    event::{ChildMetadata, ExtractionEvent, child_job, group},
    extract::{Media, MediaJob, PostMetadata},
    file_metadata::FileMetadata,
    network::NetworkOptions,
    sites::{Route, Scan, SiteKeywords, extract_iter, parse_int, split_html, unescape},
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

type Events<'a> = BoxStream<'a, Result<ExtractionEvent>>;

const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:128.0) Gecko/20100101 Firefox/128.0";
const MAX_RESPONSE: usize = 16 * 1024 * 1024;
const MAX_PAGES: u64 = 200;
/// Subdomains that are not user pages.
const RESERVED_HOSTS: [&str; 6] = ["www", "art", "uploads", "img", "api", "css"];
const REFUSED: &str = "Newgrounds refused access (HTTP 403; its NG Guard bot check may need browser cookies from --cookies or --cookies-from-browser)";
const NO_FILE: &str = "Newgrounds post has no downloadable file (adult posts need session cookies)";

/// Upstream `NewgroundsExtractor` formats.
const FILENAME: &str = "{category}_{_index}_{title}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{artist[:10]:J, }"];
const ARCHIVE: &str = "{_type}{_index}";
static IMAGE: Route = Route::new("image", FILENAME, DIRECTORY, ARCHIVE);
static MEDIA: Route = Route::new("media", FILENAME, DIRECTORY, ARCHIVE);
static ART: Route = Route::new("art", FILENAME, DIRECTORY, ARCHIVE);
static AUDIO: Route = Route::new("audio", FILENAME, DIRECTORY, ARCHIVE);
static MOVIES: Route = Route::new("movies", FILENAME, DIRECTORY, ARCHIVE);
static GAMES: Route = Route::new("games", FILENAME, DIRECTORY, ARCHIVE);
static FAVORITE: Route = Route::new(
    "favorite",
    FILENAME,
    &["{category}", "{user}", "Favorites"],
    ARCHIVE,
);
static SEARCH: Route = Route::new(
    "search",
    FILENAME,
    &["{category}", "search", "{search_tags}"],
    ARCHIVE,
);

/// Views of upstream's `NewgroundsUserExtractor` dispatcher.
const USER_VIEWS: &[&str] = &["art", "audio", "games", "movies"];

/// Media IDs are `INDEX_NUM`, prefixed `a` for audio and `p` for portal
/// movies/games (art keeps its original unprefixed identity).
pub fn valid_id(id: &str) -> bool {
    let id = id
        .strip_prefix('a')
        .or_else(|| id.strip_prefix('p'))
        .unwrap_or(id);
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

/// A user's listing kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Art,
    Audio,
    Movies,
    Games,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Art => "art",
            Kind::Audio => "audio",
            Kind::Movies => "movies",
            Kind::Games => "games",
        }
    }

    fn route(self) -> &'static Route {
        match self {
            Kind::Art => &ART,
            Kind::Audio => &AUDIO,
            Kind::Movies => &MOVIES,
            Kind::Games => &GAMES,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// An art post.
    Post {
        user: String,
        slug: String,
    },
    /// `portal/view/ID` or `audio/listen/ID`.
    Media {
        path: String,
    },
    /// A user's `art`, `audio`, `movies` or `games` listing from `page`.
    Listing {
        user: String,
        kind: Kind,
        page: i64,
    },
    User {
        user: String,
    },
    /// Favorites of one kind (`art`, `audio`, `movies`) or all three.
    Favorites {
        user: String,
        kind: Option<String>,
        page: i64,
    },
    Following {
        user: String,
        page: i64,
    },
    /// `search/conduct/PATH?QUERY`.
    Search {
        path: String,
        query: Vec<(String, String)>,
    },
}

fn plain(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https")
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
}

/// `(/page/N | ?page=N)` of a listing URL, else 1.
fn page_number(rest: &[&str], url: &Url) -> Option<i64> {
    match rest {
        [] => Some(
            url.query_pairs()
                .find(|(k, _)| k == "page")
                .map_or(1, |(_, v)| parse_int(&v, 1)),
        ),
        ["page", n] if n.bytes().all(|b| b.is_ascii_digit()) && n.len() <= 9 => n.parse().ok(),
        _ => None,
    }
}

pub fn target(url: &Url) -> Result<Target> {
    let unsupported = || {
        error(
            "unsupported Newgrounds URL; expected https://www.newgrounds.com/art/view/USER/SLUG, /portal/view/ID, /audio/listen/ID, /search/conduct/..., or https://USER.newgrounds.com[/art|/audio|/movies|/games|/favorites[/KIND|/following]]",
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
        "www.newgrounds.com" | "newgrounds.com" => match parts[..] {
            ["art", "view", user, slug] if valid_name(user) && valid_name(slug) => {
                Ok(Target::Post {
                    user: user.to_ascii_lowercase(),
                    slug: slug.into(),
                })
            }
            [
                kind @ ("portal" | "audio"),
                view @ ("view" | "listen"),
                id,
                ..,
            ] if (kind, view) != ("portal", "listen")
                && (kind, view) != ("audio", "view")
                && (1..=20).contains(&id.len())
                && id.bytes().all(|b| b.is_ascii_digit()) =>
            {
                Ok(Target::Media {
                    path: format!("{kind}/{view}/{id}"),
                })
            }
            ["search", "conduct", path] if valid_name(path) && url.query().is_some() => {
                Ok(Target::Search {
                    path: path.into(),
                    query: crate::sites::parse_query(url.query().unwrap_or_default()),
                })
            }
            _ => Err(unsupported()),
        },
        // Upstream maps full-size art files back to their post page.
        "art.ngfiles.com" => match parts[..] {
            ["images", dir, name] if dir.bytes().all(|b| b.is_ascii_digit()) => {
                let (index, rest) = name.split_once('_').ok_or_else(unsupported)?;
                let (user, rest) = rest.split_once('_').ok_or_else(unsupported)?;
                let slug = rest.split('.').next().unwrap_or_default();
                if index.is_empty()
                    || !index.bytes().all(|b| b.is_ascii_digit())
                    || !valid_name(user)
                    || !valid_name(slug)
                {
                    return Err(unsupported());
                }
                Ok(Target::Post {
                    user: user.to_ascii_lowercase(),
                    slug: slug.into(),
                })
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
                [] => Ok(Target::User { user }),
                [kind @ ("art" | "audio" | "movies" | "games"), ref rest @ ..] => {
                    let kind = match kind {
                        "art" => Kind::Art,
                        "audio" => Kind::Audio,
                        "movies" => Kind::Movies,
                        _ => Kind::Games,
                    };
                    let page = page_number(rest, url).ok_or_else(unsupported)?;
                    Ok(Target::Listing { user, kind, page })
                }
                ["favorites", "following", ref rest @ ..] => Ok(Target::Following {
                    user,
                    page: page_number(rest, url).ok_or_else(unsupported)?,
                }),
                [
                    "favorites",
                    kind @ ("art" | "audio" | "movies"),
                    ref rest @ ..,
                ] => Ok(Target::Favorites {
                    user,
                    kind: Some(kind.into()),
                    page: page_number(rest, url).ok_or_else(unsupported)?,
                }),
                ["favorites"] => Ok(Target::Favorites {
                    user,
                    kind: None,
                    page: 1,
                }),
                _ => Err(unsupported()),
            }
        }
    }
}

pub fn is_newgrounds_url(url: &Url) -> bool {
    target(url).is_ok()
}

/// HTTPS media on `*.ngfiles.com` or `*.ungrounded.net` (movie and flash
/// uploads).
pub fn media_url(raw: &str) -> Option<Url> {
    let raw = unescape(raw.trim());
    let url = Url::parse(&ensure_http_scheme(&raw)).ok()?;
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

/// gallery-dl `text.ensure_http_scheme`.
fn ensure_http_scheme(url: &str) -> String {
    if url.is_empty() || url.starts_with("https://") || url.starts_with("http://") {
        url.to_owned()
    } else {
        format!("https://{}", url.trim_start_matches(['/', ':']))
    }
}

/// gallery-dl `text.ext_from_url`.
fn ext_from_url(raw: &str) -> String {
    let path = raw.split(['?', '#']).next().unwrap_or_default();
    match path.rsplit('/').next().unwrap_or_default().rsplit_once('.') {
        Some((name, ext)) if !name.is_empty() => ext.to_lowercase(),
        _ => String::new(),
    }
}

/// gallery-dl `text.parse_float`.
fn parse_float(text: &str) -> f64 {
    text.trim()
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
        .unwrap_or(0.0)
}

/// gallery-dl `text.remove_html(text, "", "")`.
fn strip_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('<') {
        match rest[start + 1..].find('>') {
            Some(end) if end > 0 => {
                out.push_str(&rest[..start]);
                rest = &rest[start + end + 2..];
            }
            _ => {
                out.push_str(&rest[..=start]);
                rest = &rest[start + 1..];
            }
        }
    }
    out.push_str(rest);
    out.trim().to_owned()
}

/// Upstream `_extract_comment_urls`: `<img src="…">` and
/// `data-smartload-src="…"` values in document order.
fn comment_urls(html: &str) -> Vec<String> {
    let mut urls = Vec::new();
    let mut rest = html;
    loop {
        let found = ["<img src=\"", "data-smartload-src=\""]
            .iter()
            .filter_map(|needle| rest.find(needle).map(|at| (at, needle.len())))
            .min();
        let Some((at, len)) = found else { break };
        rest = &rest[at + len..];
        match rest.find('"') {
            Some(end) if end > 0 => {
                urls.push(rest[..end].to_owned());
                rest = &rest[end + 1..];
            }
            _ => continue,
        }
    }
    urls
}

/// The upstream `format` option.
#[derive(Clone, Debug, PartialEq)]
pub enum Formats {
    /// Ordered names (`"mp4"`) or heights (`1080`).
    List(Vec<Value>),
    /// Every source up to this height, highest first.
    Limit(i64),
}

impl Default for Formats {
    fn default() -> Self {
        Self::List(
            ["mp4", "webm", "m4v", "mov", "mkv"]
                .into_iter()
                .map(Value::from)
                .chain([1080, 720, 360].into_iter().map(Value::from))
                .collect(),
        )
    }
}

fn source(sources: &Map<String, Value>, key: &str) -> Option<String> {
    sources
        .get(key)?
        .get(0)?
        .get("src")?
        .as_str()
        .map(str::to_owned)
}

/// Upstream `_video_formats`/`_video_formats_limit`: candidate movie URLs.
pub fn video_formats(sources: &Map<String, Value>, formats: &Formats) -> Vec<String> {
    match formats {
        Formats::List(list) => {
            // Upstream needs the 360p source before yielding anything.
            let Some(base) = source(sources, "360p") else {
                return Vec::new();
            };
            list.iter()
                .filter_map(|format| match format {
                    Value::Number(height) => source(sources, &format!("{height}p")),
                    Value::String(name) if sources.contains_key(name) => source(sources, name),
                    Value::String(name) => Some(replace_360p(&base, name)),
                    _ => None,
                })
                .collect()
        }
        Formats::Limit(limit) => {
            let mut found: Vec<(i64, String)> = sources
                .iter()
                .filter_map(|(format, _)| {
                    let width = parse_int(format.trim_end_matches('p'), 0);
                    (width <= *limit).then(|| Some((width, source(sources, format)?)))?
                })
                .collect();
            found.sort_by(|a, b| b.cmp(a));
            found.into_iter().map(|(_, src)| src).collect()
        }
    }
}

/// `re.sub(r"\.360p\.\w+", "." + name, src, 1)`.
fn replace_360p(src: &str, name: &str) -> String {
    let Some(at) = src.find(".360p.") else {
        return src.to_owned();
    };
    let tail = &src[at + 6..];
    let word = tail
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(tail.len());
    if word == 0 {
        return src.to_owned();
    }
    format!("{}.{name}{}", &src[..at], &tail[word..])
}

/// One post's upstream dictionary, its extra images and fallbacks.
struct PostData {
    data: Map<String, Value>,
    multi: Vec<Map<String, Value>>,
    fallback: Vec<String>,
}

/// One emitted file: URL, keyword dictionary and fallback URLs.
struct File {
    url: Url,
    fields: Map<String, Value>,
    fallback: Vec<Value>,
}

/// Upstream `items()`: the main file, extra images and comment images, with
/// `num`/`_index` and the cumulative post dictionary.
fn files(post: PostData) -> Vec<File> {
    let PostData {
        mut data,
        multi,
        fallback,
    } = post;
    let mut out = Vec::new();
    let Some(main) = data.get("url").and_then(Value::as_str).and_then(media_url) else {
        return out;
    };
    let index = data
        .get("index")
        .map(crate::sites::scalar)
        .unwrap_or_default();
    let nameext = |data: &mut Map<String, Value>, url: &Url| {
        let (name, ext) = crate::sites::nameext(url);
        data.insert("filename".into(), name.into());
        data.insert("extension".into(), ext.into());
    };
    data.insert("num".into(), 0.into());
    nameext(&mut data, &main);
    let fallback: Vec<Value> = fallback
        .iter()
        .filter(|u| media_url(u).is_some())
        .map(|u| Value::from(u.as_str()))
        .collect();
    out.push(File {
        url: main,
        fields: data.clone(),
        fallback,
    });
    let mut num = 0;
    for extra in multi {
        num += 1;
        data.insert("num".into(), num.into());
        data.insert("_index".into(), format!("{index}_{num:02}").into());
        for (key, value) in &extra {
            data.insert(key.clone(), value.clone());
        }
        let fallback: Vec<Value> = data
            .get("_fallback")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|u| u.as_str().and_then(media_url).is_some())
            .collect();
        if let Some(url) = extra
            .get("image")
            .and_then(Value::as_str)
            .and_then(media_url)
        {
            nameext(&mut data, &url);
            out.push(File {
                url,
                fields: data.clone(),
                fallback,
            });
        }
        // Upstream drops a per-image fallback after its file.
        data.remove("_fallback");
    }
    let comment = data
        .get("_comment")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    for raw in comment_urls(&comment) {
        num += 1;
        data.insert("num".into(), num.into());
        data.insert("_index".into(), format!("{index}_{num:02}").into());
        let Some(url) = media_url(&ensure_http_scheme(&raw)) else {
            continue;
        };
        nameext(&mut data, &url);
        out.push(File {
            url,
            fields: data.clone(),
            fallback: Vec::new(),
        });
    }
    out
}

fn post_media(post: PostData, route: &'static Route, extra: &Map<String, Value>) -> Vec<Media> {
    let kind = post
        .data
        .get("_type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let media_type = match kind.as_str() {
        "i" => "photo",
        "a" => "audio",
        _ => "video",
    };
    files(post)
        .into_iter()
        .enumerate()
        .map(
            |(
                n,
                File {
                    url,
                    mut fields,
                    fallback,
                },
            )| {
                for (key, value) in extra {
                    fields.insert(key.clone(), value.clone());
                }
                fields.insert("category".into(), "newgrounds".into());
                fields.insert("subcategory".into(), route.subcategory.into());
                let text = |key: &str| {
                    fields
                        .get(key)
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                };
                let index = fields
                    .get("index")
                    .map(crate::sites::scalar)
                    .unwrap_or_default();
                let prefix = match kind.as_str() {
                    "i" => "",
                    "a" => "a",
                    _ => "p",
                };
                let dimension = |key: &str| {
                    (n == 0)
                        .then(|| fields.get(key).and_then(Value::as_u64))
                        .flatten()
                        .filter(|v| *v > 0)
                };
                let filename = match (text("filename"), text("extension")) {
                    (Some(name), Some(ext)) => format!("{name}.{ext}"),
                    (name, _) => name.unwrap_or_default(),
                };
                let mut handoff = BTreeMap::new();
                if !fallback.is_empty() {
                    handoff.insert("_fallback".into(), Value::Array(fallback));
                }
                Media {
                    filename,
                    job: MediaJob::DirectHttp,
                    category: "newgrounds".into(),
                    metadata: Some(PostMetadata {
                        file: Some(FileMetadata {
                            width: dimension("width"),
                            height: dimension("height"),
                            media_type: Some(Arc::from(media_type)),
                            ..FileMetadata::default()
                        }),
                        post_id: index.clone(),
                        media_id: format!("{prefix}{index}_{}", n + 1),
                        username: text("user").map(|u| Arc::from(u.to_ascii_lowercase())),
                        caption: text("title").map(Arc::from),
                        timestamp: text("date").and_then(|d| crate::date::parse_date(&d).ok()),
                        rating: text("rating").map(Arc::from),
                        index: n + 1,
                        site_keywords: Some(Arc::new(SiteKeywords { route, fields })),
                        ..PostMetadata::default()
                    }),
                    url,
                    headers: BTreeMap::new(),
                    handoff,
                }
            },
        )
        .collect()
}

fn date(timestamp: Option<i64>) -> Value {
    match timestamp {
        Some(_) => crate::filter::source_date_text(timestamp).into(),
        None => Value::Null,
    }
}

/// One listing page's post URLs (upstream `_pagination`), and whether
/// another page follows.
pub fn parse_listing(bytes: &[u8]) -> Result<(Vec<String>, bool)> {
    let Ok(body) = serde_json::from_slice::<Value>(bytes) else {
        return Ok((Vec::new(), false));
    };
    if let Some(errors) = body.get("errors").and_then(Value::as_array) {
        let message: Vec<String> = errors
            .iter()
            .filter_map(Value::as_str)
            .map(unescape)
            .collect();
        return Err(error(format!("Newgrounds: {}", message.join(", "))));
    }
    let mut posts = Vec::new();
    if let Some(years) = body.get("items").and_then(Value::as_object) {
        for item in years.values().filter_map(Value::as_array).flatten() {
            if let Some(href) = item
                .as_str()
                .and_then(|f| crate::sites::extr(f, "href=\"", "\""))
                .filter(|href| !href.is_empty())
            {
                posts.push(href.to_owned());
            }
        }
    }
    let more = body
        .get("load_more")
        .and_then(Value::as_str)
        .is_some_and(|more| more.len() >= 8);
    let more = more && !posts.is_empty();
    Ok((posts, more))
}

pub struct Newgrounds {
    client: Client,
    network: NetworkOptions,
    root: Url,
    user_root: Option<Url>,
    cookie: Option<String>,
    flash: bool,
    formats: Formats,
    include: Option<crate::dispatch::Include>,
    timeout: Duration,
    request_timeout: Duration,
    retries: u8,
    rate_limits: RateLimits,
}

/// A fetched page with its final URL after redirects.
struct Reply {
    status: StatusCode,
    body: Vec<u8>,
    url: Url,
}

/// Pagination of one listing class.
enum Pages {
    /// `USER.newgrounds.com/KIND?page=N&isAjaxRequest=1`.
    Listing { user: String, kind: &'static str },
    /// `favorites/KIND` pages, kinds in order.
    Favorites {
        user: String,
        kinds: VecDeque<String>,
    },
    Search {
        path: String,
        params: Vec<(String, String)>,
    },
}

struct ListState {
    pages: Pages,
    page: i64,
    fetched: u64,
    done: bool,
    seen: HashSet<String>,
}

impl Newgrounds {
    pub fn new(timeout: Duration) -> Result<Self> {
        Self::from_cookie_jar(CookieJar::default(), timeout)
    }

    /// Build an extractor that sends Newgrounds session cookies.
    pub fn from_cookie_jar(cookies: CookieJar, timeout: Duration) -> Result<Self> {
        let site = Url::parse("https://www.newgrounds.com/").expect("static URL");
        Ok(Self {
            client: NetworkOptions::default().client(USER_AGENT, timeout)?,
            network: NetworkOptions::default(),
            cookie: cookies.header(&site),
            root: site,
            user_root: None,
            flash: true,
            formats: Formats::default(),
            include: None,
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

    /// Apply upstream extractor settings (`extractor.newgrounds.*`): `flash`
    /// (boolean, default true) and `format` (`"original"`, a list of format
    /// names/heights, or a maximum height such as `"720p"`).
    pub fn with_options(mut self, options: Option<&Map<String, Value>>) -> Result<Self> {
        let Some(options) = options else {
            return Ok(self);
        };
        self.flash = match options.get("flash") {
            None | Some(Value::Null) => true,
            Some(Value::Bool(value)) => *value,
            Some(_) => return Err(error("extractor.newgrounds.flash must be true or false")),
        };
        self.formats = match options.get("format") {
            None | Some(Value::Null) | Some(Value::Bool(false)) => Formats::default(),
            Some(Value::String(format)) if format.is_empty() || format == "original" => {
                Formats::default()
            }
            Some(Value::Array(list))
                if list.len() <= 64 && list.iter().all(|v| v.is_string() || v.is_i64()) =>
            {
                Formats::List(list.clone())
            }
            Some(Value::Number(height)) if height.is_i64() => {
                Formats::Limit(height.as_i64().unwrap_or_default())
            }
            Some(Value::String(format)) => {
                Formats::Limit(parse_int(format.trim_end_matches('p'), 0))
            }
            Some(_) => {
                return Err(error(
                    "extractor.newgrounds.format must be \"original\", a format list or a maximum height",
                ));
            }
        };
        Ok(self)
    }

    /// Views queued for `USER.newgrounds.com` (default `art`).
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

    fn join(base: &Url, path: &str) -> Result<Url> {
        base.join(path.trim_start_matches('/'))
            .map_err(|_| error("invalid Newgrounds path"))
    }

    /// One request; any status other than 429/5xx is returned to the caller.
    /// HTTP 403 is always fatal: it is the site's NG Guard bot check (or a
    /// refused session), which would otherwise look like an empty listing.
    async fn fetch(&self, endpoint: Url, ajax: bool, referer: Option<&str>) -> Result<Reply> {
        let reply = tokio::time::timeout(self.timeout, self.request(&endpoint, ajax, referer))
            .await
            .map_err(|_| error("native Newgrounds extraction timed out"))??;
        if reply.status == StatusCode::FORBIDDEN {
            return Err(error(REFUSED));
        }
        Ok(reply)
    }

    /// One request whose HTTP errors are fatal.
    async fn call(&self, endpoint: Url, ajax: bool, referer: Option<&str>) -> Result<Reply> {
        let reply = self.fetch(endpoint, ajax, referer).await?;
        let status = reply.status;
        if matches!(status.as_u16(), 404 | 410) {
            return Err(error("Newgrounds post or user not found"));
        }
        if matches!(status.as_u16(), 401 | 403) {
            return Err(error(REFUSED));
        }
        if !status.is_success() {
            return Err(error(format!("Newgrounds returned HTTP {status}")));
        }
        Ok(reply)
    }

    /// Upstream `extract_post`; `None` for adult-walled, missing or
    /// file-less posts.
    async fn extract_post(&self, path: &str, user: Option<&str>) -> Result<Option<PostData>> {
        let post_url = format!("https://www.newgrounds.com/{path}");
        let image = path.starts_with("art/view/");
        let audio = path.starts_with("audio/listen/");
        let mut request = path.to_owned();
        if !image && !audio && self.flash {
            request.push_str("/format/flash");
        }
        let reply = self
            .fetch(Self::join(&self.root, &request)?, false, None)
            .await?;
        let page = String::from_utf8_lossy(&reply.body).into_owned();
        if page.contains("id=\"adults_only\"") || reply.status.as_u16() >= 400 {
            return Ok(None);
        }
        let mut extr = Scan::new(&page);
        let mut multi = Vec::new();
        let mut fallback = Vec::new();
        let mut data = if image {
            let full: String =
                serde_json::from_str(extr.extr("\"full_image_text\":", "});").trim())
                    .unwrap_or_default();
            let mut full = Scan::new(&full);
            let mut data = Map::new();
            data.insert(
                "title".into(),
                unescape(extr.extr("\"og:title\" content=\"", "\"")).into(),
            );
            data.insert(
                "description".into(),
                unescape(extr.extr(":description\" content=\"", "\"")).into(),
            );
            data.insert("type".into(), "art".into());
            data.insert("_type".into(), "i".into());
            let published = extr.extr("itemprop=\"datePublished\" content=\"", "\"");
            data.insert("date".into(), date(crate::date::parse_date(published).ok()));
            data.insert("rating".into(), extr.extr("class=\"rated-", "\"").into());
            let mut url = full.extr("src=\"", "\"").to_owned();
            data.insert(
                "width".into(),
                parse_int(full.extr("width=\"", "\""), 0).into(),
            );
            data.insert(
                "height".into(),
                parse_int(full.extr("height=\"", "\""), 0).into(),
            );
            if url.is_empty() {
                url = extr.extr("<a href=\"", "\"").to_owned();
            }
            let index = url
                .rsplit('/')
                .next()
                .unwrap_or_default()
                .split('_')
                .next()
                .unwrap_or_default()
                .to_owned();
            data.insert("url".into(), url.clone().into());
            data.insert("index".into(), parse_int(&index, 0).into());
            data.insert("_index".into(), index.into());
            let images = extr.extr("let imageData =", "\n];");
            if !images.is_empty() {
                if let Ok(Value::Array(list)) = serde_json::from_str::<Value>(&format!("{images}]"))
                {
                    multi = list
                        .into_iter()
                        .skip(1)
                        .filter_map(|v| v.as_object().cloned())
                        .collect();
                }
            } else {
                let art = extr.extr("<div class=\"art-images", "\n\t\t</div>");
                if !art.is_empty() {
                    let ext = ext_from_url(&url);
                    for raw in extract_iter(art, "data-smartload-src=\"", "\"") {
                        let raw = ensure_http_scheme(raw).replacen("/medium_views/", "/images/", 1);
                        let mut item = Map::new();
                        if ext_from_url(&raw) == "webp" {
                            let mut list: Vec<Value> = ["jpg", "png", "gif"]
                                .into_iter()
                                .filter(|e| *e != ext)
                                .map(|e| Value::from(raw.replace(".webp", &format!(".{e}"))))
                                .collect();
                            list.push(raw.clone().into());
                            item.insert(
                                "image".into(),
                                raw.replace(".webp", &format!(".{ext}")).into(),
                            );
                            item.insert("_fallback".into(), list.into());
                        } else {
                            item.insert("image".into(), raw.into());
                        }
                        multi.push(item);
                    }
                }
            }
            data
        } else if audio {
            let index = path.split('/').nth(2).unwrap_or_default().to_owned();
            let mut data = Map::new();
            data.insert(
                "title".into(),
                unescape(extr.extr("\"og:title\" content=\"", "\"")).into(),
            );
            data.insert(
                "description".into(),
                unescape(extr.extr(":description\" content=\"", "\"")).into(),
            );
            data.insert("type".into(), "audio".into());
            data.insert("_type".into(), "a".into());
            let published = extr.extr("itemprop=\"datePublished\" content=\"", "\"");
            data.insert("date".into(), date(crate::date::parse_date(published).ok()));
            data.insert(
                "url".into(),
                extr.extr("{\"url\":\"", "\"").replace("\\/", "/").into(),
            );
            data.insert("index".into(), parse_int(&index, 0).into());
            data.insert("_index".into(), index.into());
            data.insert("rating".into(), "".into());
            data
        } else {
            let index = path.split('/').nth(2).unwrap_or_default().to_owned();
            let title = extr.extr("\"og:title\" content=\"", "\"").to_owned();
            // Upstream reads `og:type`, then replaces it below.
            let _ = extr.extr("og:type\" content=\"", "\"");
            let mut kind;
            let descr = extr.extr("\"og:description\" content=\"", "\"").to_owned();
            let mut src = extr.extr("{\"url\":\"", "\"").to_owned();
            let timestamp;
            if !src.is_empty() {
                src = src.replace("\\/", "/");
                kind = extr.extr(",\"description\":\"", "\"").to_owned();
                let published = extr.extr("itemprop=\"datePublished\" content=\"", "\"");
                timestamp = crate::date::parse_date(published).ok();
                kind = if kind.is_empty() {
                    // Upstream checks the post URL, which never ends in `.swf`.
                    if ext_from_url(&post_url) == "swf" {
                        "flash".into()
                    } else {
                        "game".into()
                    }
                } else {
                    kind.rsplit(' ').next().unwrap_or_default().to_lowercase()
                };
            } else {
                let endpoint = Self::join(&self.root, &format!("portal/video/{index}"))?;
                let reply = self.call(endpoint, true, None).await?;
                let sources = serde_json::from_slice::<Value>(&reply.body)
                    .ok()
                    .and_then(|body| body.get("sources").and_then(Value::as_object).cloned())
                    .ok_or_else(|| error("Newgrounds returned no movie sources"))?;
                let mut formats = video_formats(&sources, &self.formats).into_iter();
                src = formats.next().unwrap_or_default();
                fallback = formats.collect();
                timestamp = crate::source_date::parse_timestamp(&Value::from(
                    src.rsplit_once('?').map_or("", |(_, query)| query),
                ));
                kind = "movie".into();
            }
            let mut description = unescape(&descr);
            if descr.is_empty() {
                description = unescape(extr.extr("itemprop=\"description\" content=\"", "\""));
            }
            let mut data = Map::new();
            data.insert("title".into(), unescape(&title).into());
            data.insert("url".into(), src.into());
            data.insert("date".into(), date(timestamp));
            data.insert("type".into(), kind.into());
            data.insert("_type".into(), "".into());
            data.insert("description".into(), description.into());
            data.insert("rating".into(), extr.extr("class=\"rated-", "\"").into());
            data.insert("index".into(), parse_int(&index, 0).into());
            data.insert("_index".into(), index.into());
            data
        };
        if data
            .get("url")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        {
            return Ok(None);
        }
        let comment = extr
            .extr("id=\"author_comments\"", "</div>")
            .split_once('>')
            .map_or("", |(_, rest)| rest)
            .trim()
            .to_owned();
        data.insert("comment_html".into(), comment.clone().into());
        data.insert(
            "comment".into(),
            unescape(&strip_html(
                &comment.replace("<p><br></p>", "\n\n").replace("<br>", "\n"),
            ))
            .into(),
        );
        data.insert("_comment".into(), comment.into());
        data.insert(
            "favorites".into(),
            parse_int(&extr.extr("id=\"faves_load\">", "<").replace(',', ""), 0).into(),
        );
        data.insert(
            "score".into(),
            parse_float(extr.extr("id=\"score_number\">", "<")).into(),
        );
        let mut tags: Vec<String> = split_html(extr.extr("<dd class=\"tags\">", "</dd>"))
            .into_iter()
            .filter(|tag| !tag.contains("(function("))
            .collect();
        tags.sort();
        data.insert("tags".into(), tags.into());
        let artists: Vec<String> = extract_iter(&page, "<div class=\"item-user\">", ">")
            .into_iter()
            .map(|user| {
                crate::sites::extr(user, "//", ".")
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect();
        let user = user
            .filter(|user| !user.is_empty())
            .map(str::to_owned)
            .or_else(|| artists.first().cloned())
            .unwrap_or_default();
        data.insert("artist".into(), artists.into());
        data.insert("user".into(), user.into());
        data.insert(
            "slug".into(),
            post_url.rsplit('/').next().unwrap_or_default().into(),
        );
        data.insert("post_url".into(), post_url.into());
        if !fallback.is_empty() {
            data.insert(
                "_fallback".into(),
                fallback.iter().map(|u| Value::from(u.as_str())).collect(),
            );
        }
        Ok(Some(PostData {
            data,
            multi,
            fallback,
        }))
    }

    async fn post(
        &self,
        path: &str,
        route: &'static Route,
        extra: &Map<String, Value>,
        user: Option<&str>,
    ) -> Result<Option<VecDeque<ExtractionEvent>>> {
        let Some(post) = self.extract_post(path, user).await? else {
            return Ok(None);
        };
        let mut items = post_media(post, route, extra);
        if items.is_empty() {
            return Ok(None);
        }
        for item in &mut items {
            self.network.apply_media(item);
        }
        Ok(Some(group(items, "newgrounds")))
    }

    /// One post's events; listings skip posts without a file like upstream.
    fn post_events<'a>(
        &'a self,
        path: String,
        route: &'static Route,
        extra: Map<String, Value>,
        user: Option<String>,
        listing: bool,
    ) -> Events<'a> {
        stream::once(async move { self.post(&path, route, &extra, user.as_deref()).await })
            .flat_map(move |result| match result {
                Ok(Some(events)) => stream::iter(events.into_iter().map(Ok)).boxed(),
                Ok(None) if listing => stream::empty().boxed(),
                Ok(None) => stream::once(async { Err(error(NO_FILE)) }).boxed(),
                Err(e) => stream::once(async move { Err(e) }).boxed(),
            })
            .boxed()
    }

    /// A post page path (`art/view/U/S`, `audio/listen/N`, `portal/view/N`)
    /// from a listing href.
    fn post_path(href: &str) -> Option<String> {
        let href = unescape(href);
        let url = if href.starts_with('/') && !href.starts_with("//") {
            Url::parse(&format!("https://www.newgrounds.com{href}")).ok()?
        } else {
            Url::parse(&ensure_http_scheme(&href)).ok()?
        };
        match target(&url).ok()? {
            Target::Post { user, slug } => Some(format!("art/view/{user}/{slug}")),
            Target::Media { path } => Some(path),
            _ => None,
        }
    }

    /// Post paths of the next listing page.
    async fn next_posts(&self, state: &mut ListState) -> Result<Vec<String>> {
        state.fetched += 1;
        let (hrefs, more) = match &mut state.pages {
            Pages::Listing { user, kind } => {
                let base = Self::join(&self.user_root(user), kind)?;
                let referer = format!("https://{user}.newgrounds.com/{kind}");
                let mut endpoint = base.clone();
                endpoint
                    .query_pairs_mut()
                    .append_pair("page", &state.page.to_string())
                    .append_pair("isAjaxRequest", "1");
                let reply = self.fetch(endpoint, true, Some(&referer)).await?;
                parse_listing(&reply.body)?
            }
            Pages::Favorites { user, kinds } => {
                let Some(kind) = kinds.front().cloned() else {
                    state.done = true;
                    return Ok(Vec::new());
                };
                let base = Self::join(&self.user_root(user), &format!("favorites/{kind}"))?;
                let referer = format!("https://{user}.newgrounds.com/favorites/{kind}");
                let mut endpoint = base.clone();
                endpoint
                    .query_pairs_mut()
                    .append_pair("page", &state.page.to_string())
                    .append_pair("isAjaxRequest", "1");
                let reply = self.call(endpoint.clone(), true, Some(&referer)).await?;
                // Upstream stops a kind when the page redirects.
                let favorites = if reply.url != endpoint {
                    Vec::new()
                } else {
                    let component = serde_json::from_slice::<Value>(&reply.body)
                        .map_err(|_| error("Newgrounds returned invalid JSON"))?
                        .get("component")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    extract_iter(&component, "href=\"https://www.newgrounds.com", "\"")
                        .into_iter()
                        .map(str::to_owned)
                        .collect()
                };
                let more = favorites.len() >= 24;
                if !more {
                    kinds.pop_front();
                    state.page = 0;
                }
                (favorites, more || !kinds.is_empty())
            }
            Pages::Search { path, params } => {
                let mut endpoint = Self::join(&self.root, &format!("search/conduct/{path}"))?;
                {
                    let mut pairs = endpoint.query_pairs_mut();
                    for (key, value) in params.iter() {
                        pairs.append_pair(key, value);
                    }
                    pairs
                        .append_pair("inner", "1")
                        .append_pair("page", &state.page.to_string());
                }
                let reply = self.call(endpoint, true, None).await?;
                let content = serde_json::from_slice::<Value>(&reply.body)
                    .map_err(|_| error("Newgrounds returned invalid JSON"))?
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let hrefs: Vec<String> = extract_iter(&content, "href=\"", "\"")
                    .into_iter()
                    .filter(|href| !href.starts_with("/search/"))
                    .map(str::to_owned)
                    .collect();
                let more = !hrefs.is_empty();
                (hrefs, more)
            }
        };
        state.page += 1;
        let paths: Vec<String> = hrefs
            .iter()
            .filter_map(|href| Self::post_path(href))
            .filter(|path| state.seen.insert(path.clone()))
            .collect();
        let favorites = matches!(state.pages, Pages::Favorites { .. });
        // A page without new posts would repeat forever.
        if !more || (paths.is_empty() && !favorites) || state.fetched >= MAX_PAGES {
            state.done = true;
        }
        Ok(paths)
    }

    /// Lazily page a listing and extract every listed post.
    fn listing<'a>(
        &'a self,
        pages: Pages,
        page: i64,
        route: &'static Route,
        extra: Map<String, Value>,
        user: Option<String>,
    ) -> Events<'a> {
        let state = ListState {
            pages,
            page: page.max(1),
            fetched: 0,
            done: false,
            seen: HashSet::new(),
        };
        stream::unfold(Some(state), move |state| async move {
            let mut state = state?;
            if state.done {
                return None;
            }
            match self.next_posts(&mut state).await {
                Ok(paths) => Some((Ok(paths), Some(state))),
                Err(e) => Some((Err(e), None)),
            }
        })
        .flat_map(move |page| match page {
            Ok(paths) => {
                let (extra, user) = (extra.clone(), user.clone());
                stream::iter(paths)
                    .flat_map(move |path| {
                        self.post_events(path, route, extra.clone(), user.clone(), true)
                    })
                    .boxed()
            }
            Err(e) => stream::once(async move { Err(e) }).boxed(),
        })
        .boxed()
    }

    pub fn stream(&self, url: &Url) -> BoxStream<'_, Result<Media>> {
        crate::event::media_only(self.events(url))
    }

    /// Emit one post boundary per post; listings paginate lazily and user
    /// profiles and followed users queue child jobs.
    pub fn events(&self, url: &Url) -> Events<'_> {
        let target = match target(url) {
            Ok(target) => target,
            Err(e) => return stream::once(async move { Err(e) }).boxed(),
        };
        match target {
            Target::Post { user, slug } => self.post_events(
                format!("art/view/{user}/{slug}"),
                &IMAGE,
                Map::new(),
                Some(user),
                false,
            ),
            Target::Media { path } => self.post_events(path, &MEDIA, Map::new(), None, false),
            Target::Listing { user, kind, page } => {
                let pages = Pages::Listing {
                    user: user.clone(),
                    kind: kind.name(),
                };
                self.listing(pages, page, kind.route(), Map::new(), Some(user))
            }
            Target::Favorites { user, kind, page } => {
                let kinds: VecDeque<String> = match kind {
                    Some(kind) => VecDeque::from([kind]),
                    None => ["art", "audio", "movies"].map(String::from).into(),
                };
                let pages = Pages::Favorites {
                    user: user.clone(),
                    kinds,
                };
                self.listing(pages, page, &FAVORITE, Map::new(), Some(user))
            }
            Target::Search { path, query } => {
                let search = query
                    .iter()
                    .find(|(k, _)| k == "terms")
                    .map_or(String::new(), |(_, v)| v.clone());
                let page = query
                    .iter()
                    .find(|(k, _)| k == "page")
                    .map_or(1, |(_, v)| parse_int(v, 1));
                let params: Vec<(String, String)> = query
                    .into_iter()
                    .filter(|(k, _)| k != "page" && k != "inner")
                    .collect();
                let extra = Map::from_iter([("search_tags".into(), Value::from(search))]);
                // Upstream's search class takes its `user` from the path.
                let user = Some(path.clone());
                self.listing(Pages::Search { path, params }, page, &SEARCH, extra, user)
            }
            Target::User { user } => self.user(user),
            Target::Following { user, page } => self.following(user, page),
        }
    }

    /// Upstream `NewgroundsUserExtractor`: queue the selected views.
    fn user(&self, user: String) -> Events<'_> {
        let views = crate::dispatch::select(self.include.as_ref(), &["art"], USER_VIEWS, &[]);
        stream::iter(views)
            .flat_map(move |view| {
                let url = Url::parse(&format!("https://{user}.newgrounds.com/{view}"))
                    .expect("validated Newgrounds user");
                child_job(
                    ChildMetadata::from_raw("newgrounds", "user", &Map::new()),
                    move || self.events(&url),
                )
            })
            .boxed()
    }

    /// Upstream `NewgroundsFollowingExtractor`: queue every followed user's
    /// profile from `favorites/following` pages.
    fn following(&self, user: String, page: i64) -> Events<'_> {
        struct State {
            user: String,
            page: i64,
            pages: u64,
            done: bool,
            seen: HashSet<String>,
        }
        let state = State {
            user,
            page: page.max(1),
            pages: 0,
            done: false,
            seen: HashSet::new(),
        };
        stream::unfold(state, move |mut state| async move {
            if state.done {
                return None;
            }
            state.pages += 1;
            let result = async {
                let base = Self::join(&self.user_root(&state.user), "favorites/following")?;
                let mut endpoint = base.clone();
                endpoint
                    .query_pairs_mut()
                    .append_pair("page", &state.page.to_string())
                    .append_pair("isAjaxRequest", "1");
                let referer = format!("https://{}.newgrounds.com/favorites/following", state.user);
                let reply = self.call(endpoint.clone(), true, Some(&referer)).await?;
                if reply.url != endpoint {
                    return Ok(Vec::new());
                }
                let component = serde_json::from_slice::<Value>(&reply.body)
                    .map_err(|_| error("Newgrounds returned invalid JSON"))?
                    .get("component")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                Ok(extract_iter(&component, "class=\"item-user", "\"><img")
                    .into_iter()
                    .map(|user| ensure_http_scheme(user.rsplit('"').next().unwrap_or_default()))
                    .collect::<Vec<String>>())
            }
            .await;
            let users = match result {
                Ok(users) => users,
                Err(e) => {
                    state.done = true;
                    return Some((stream::once(async move { Err(e) }).boxed(), state));
                }
            };
            state.page += 1;
            state.done = users.len() < 24 || state.pages >= MAX_PAGES;
            let children: Vec<Events<'_>> = users
                .into_iter()
                .filter_map(|raw| {
                    let url = Url::parse(&raw).ok()?;
                    let Ok(Target::User { user }) = target(&url) else {
                        return None;
                    };
                    state.seen.insert(user).then_some(url)
                })
                .map(|url| {
                    child_job(
                        ChildMetadata::from_raw("newgrounds", "following", &Map::new()),
                        move || self.events(&url),
                    )
                })
                .collect();
            Some((stream::iter(children).flatten().boxed(), state))
        })
        .flatten()
        .boxed()
    }

    /// Whether session cookies may be sent to this URL.
    fn cookie_allowed(&self, target: &Url) -> bool {
        target.origin() == self.root.origin()
            || self
                .user_root
                .as_ref()
                .is_some_and(|root| root.origin() == target.origin())
            || (target.scheme() == "https"
                && target.port().is_none()
                && target.host_str().is_some_and(|host| {
                    host == "newgrounds.com" || host.ends_with(".newgrounds.com")
                }))
    }

    async fn request(&self, endpoint: &Url, ajax: bool, referer: Option<&str>) -> Result<Reply> {
        for attempt in 0..=self.retries {
            match self.attempt(endpoint, ajax, referer, attempt).await {
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
        ajax: bool,
        referer: Option<&str>,
        attempt: u8,
    ) -> std::result::Result<Reply, Failure> {
        let mut target = endpoint.clone();
        let mut redirects = Redirects::new(endpoint);
        let mut response = loop {
            self.rate_limits.wait(&target, self.timeout).await?;
            let mut request = self
                .client
                .get(target.clone())
                .timeout(self.request_timeout)
                .header("Accept-Language", "en-US,en;q=0.5")
                .header("Referer", referer.unwrap_or("https://www.newgrounds.com/"));
            if ajax {
                request = request
                    .header("X-Requested-With", "XMLHttpRequest")
                    .header("Accept", "application/json, text/javascript, */*; q=0.01");
            }
            if let Some(cookie) = &self.cookie
                && self.cookie_allowed(&target)
            {
                request = request.header("Cookie", cookie);
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
        Ok(Reply {
            status,
            body,
            url: target,
        })
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

    fn post_html() -> String {
        r#"<html><head>
<script>PHP.merge({"full_image_text":"<img src=\"https:\/\/art.ngfiles.com\/images\/1\/1234567_alice_ship.png?f1\" width=\"800\" height=\"600\">"});</script>
<meta property="og:title" content="Big &amp; Bold">
<meta property="og:description" content="A &amp; B">
<meta itemprop="datePublished" content="2024-01-01T00:00:00-00:00">
</head><body>
<div class="item-user"><a href="https://alice.newgrounds.com"><img></a></div>
<div class="item-user"><a href="https://bob.newgrounds.com"><img></a></div>
<div class="rated-t"></div>
<script>let imageData = [{"image":"https:\/\/art.ngfiles.com\/images\/1\/1234567_alice_ship.png?f1"},{"image":"https:\/\/art.ngfiles.com\/images\/1\/1234567_9_alice_ship.jpeg"},{"image":"https:\/\/evil.example\/a.png"}
];</script>
<div id="author_comments"><p>Hello<br>there &amp; you</p><p><br></p><img src="//art.ngfiles.com/comments/1/c.png"></div>
<span id="faves_load">1,234</span>
<span id="score_number">4.52</span>
<dl><dd class="tags"><a>zeta</a><a>alpha</a><script>(function(){})</script></dd></dl>
</body></html>"#
            .into()
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
    fn parses_helpers_and_listing() {
        assert!(valid_id("1234567_1") && valid_id("a7_1") && valid_id("p5_2"));
        assert!(!valid_id("x5_1") && !valid_id("5") && !valid_id("a_1"));
        let sources = json!({
            "360p": [{"src": "https://uploads.ungrounded.net/alternate/1/5_alternate.360p.mp4?1600000000"}],
            "720p": [{"src": "https://uploads.ungrounded.net/alternate/1/5_alternate.720p.mp4?1600000000"}],
            "1080p": [{"src": "https://uploads.ungrounded.net/alternate/1/5_alternate.1080p.mp4?1600000000"}]
        });
        let sources = sources.as_object().unwrap();
        let all = video_formats(sources, &Formats::default());
        assert_eq!(
            all[0],
            "https://uploads.ungrounded.net/alternate/1/5_alternate.mp4?1600000000"
        );
        assert_eq!(
            all[1],
            "https://uploads.ungrounded.net/alternate/1/5_alternate.webm?1600000000"
        );
        assert_eq!(all.len(), 8);
        assert!(all[5].contains(".1080p.") && all[7].contains(".360p."));
        let limited = video_formats(sources, &Formats::Limit(720));
        assert_eq!(limited.len(), 2);
        assert!(limited[0].contains(".720p.") && limited[1].contains(".360p."));
        assert!(video_formats(&Map::new(), &Formats::default()).is_empty());
        assert_eq!(
            comment_urls(r#"<img src="a"> <i data-smartload-src="b"> <img alt="x" src="c">"#),
            ["a", "b"]
        );
        assert_eq!(
            ensure_http_scheme("//x.ngfiles.com/a"),
            "https://x.ngfiles.com/a"
        );

        let listing = json!({"items": {"2024": [
            "<a href=\"https://www.newgrounds.com/art/view/alice/ship\" class=\"item\">",
            "<a href=\"/portal/view/5\">"]}, "load_more": "<a href=x>more</a>"});
        let (posts, more) = parse_listing(listing.to_string().as_bytes()).unwrap();
        assert_eq!(
            posts,
            [
                "https://www.newgrounds.com/art/view/alice/ship",
                "/portal/view/5"
            ]
        );
        assert!(more);
        assert_eq!(parse_listing(b"<html>").unwrap(), (vec![], false));
        assert!(parse_listing(br#"{"errors":["Nope &amp; no"]}"#).is_err());
        assert_eq!(
            Newgrounds::post_path("/portal/view/5").as_deref(),
            Some("portal/view/5")
        );
        assert_eq!(
            Newgrounds::post_path("https://evil.example/art/view/a/b"),
            None
        );
    }

    #[test]
    fn classifies_urls_and_hosts() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        let post = Target::Post {
            user: "alice".into(),
            slug: "ship".into(),
        };
        assert_eq!(
            t("https://www.newgrounds.com/art/view/Alice/ship").unwrap(),
            post
        );
        assert_eq!(
            t("https://art.ngfiles.com/images/1/1234567_alice_ship.png?f1").unwrap(),
            post
        );
        assert_eq!(
            t("https://www.newgrounds.com/portal/view/5").unwrap(),
            Target::Media {
                path: "portal/view/5".into()
            }
        );
        assert_eq!(
            t("https://www.newgrounds.com/audio/listen/7").unwrap(),
            Target::Media {
                path: "audio/listen/7".into()
            }
        );
        assert_eq!(
            t("https://alice.newgrounds.com/art/").unwrap(),
            Target::Listing {
                user: "alice".into(),
                kind: Kind::Art,
                page: 1
            }
        );
        assert_eq!(
            t("https://alice.newgrounds.com/movies/page/3").unwrap(),
            Target::Listing {
                user: "alice".into(),
                kind: Kind::Movies,
                page: 3
            }
        );
        assert_eq!(
            t("https://alice.newgrounds.com/audio?page=2").unwrap(),
            Target::Listing {
                user: "alice".into(),
                kind: Kind::Audio,
                page: 2
            }
        );
        assert_eq!(
            t("https://alice.newgrounds.com").unwrap(),
            Target::User {
                user: "alice".into()
            }
        );
        assert_eq!(
            t("https://alice.newgrounds.com/favorites").unwrap(),
            Target::Favorites {
                user: "alice".into(),
                kind: None,
                page: 1
            }
        );
        assert_eq!(
            t("https://alice.newgrounds.com/favorites/audio/page/2").unwrap(),
            Target::Favorites {
                user: "alice".into(),
                kind: Some("audio".into()),
                page: 2
            }
        );
        assert_eq!(
            t("https://alice.newgrounds.com/favorites/following").unwrap(),
            Target::Following {
                user: "alice".into(),
                page: 1
            }
        );
        assert_eq!(
            t("https://www.newgrounds.com/search/conduct/art?terms=space+ship").unwrap(),
            Target::Search {
                path: "art".into(),
                query: vec![("terms".into(), "space ship".into())]
            }
        );
        for raw in [
            "https://www.newgrounds.com/",
            "https://www.newgrounds.com/portal/listen/1",
            "https://www.newgrounds.com/art",
            "https://www.newgrounds.com/search/conduct/art",
            "https://alice.newgrounds.com/news",
            "https://www.newgrounds.com/art/view/alice",
            "https://art.ngfiles.com/thumbnails/1/1_a.png",
            "https://www.newgrounds.com.evil.example/art/view/a/b",
            "https://alice.newgrounds.com.evil.example/art",
        ] {
            assert!(t(raw).is_err(), "{raw}");
        }
        assert!(media_url("https://art.ngfiles.com/a.png").is_some());
        assert!(media_url("//uploads.ungrounded.net/a.mp4").is_some());
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

    fn newgrounds(root: Url) -> Newgrounds {
        Newgrounds::new(Duration::from_secs(5))
            .unwrap()
            .with_network(&NetworkOptions::new(Some(String::new()), None).unwrap())
            .unwrap()
            .with_roots(root.clone(), root)
    }

    async fn collect(native: &Newgrounds, raw: &str) -> Vec<Media> {
        native
            .stream(&Url::parse(raw).unwrap())
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .map(|m| m.unwrap())
            .collect()
    }

    #[tokio::test]
    async fn fetches_art_post_with_cookies_and_upstream_keywords() {
        let (root, server) = serve(vec![
            response("503 Service Unavailable", b""),
            ok(&post_html()),
        ]);
        let jar =
            CookieJar::parse(".newgrounds.com\tTRUE\t/\tTRUE\t0\tng_session\tsecret\n").unwrap();
        let native = Newgrounds::from_cookie_jar(jar, Duration::from_secs(5))
            .unwrap()
            .with_network(&NetworkOptions::new(Some(String::new()), None).unwrap())
            .unwrap()
            .with_roots(root.clone(), root)
            .with_retries(1);
        let media = collect(&native, "https://www.newgrounds.com/art/view/alice/ship").await;
        let ids: Vec<_> = media
            .iter()
            .map(|m| m.metadata.as_ref().unwrap().media_id.as_str())
            .collect();
        assert_eq!(ids, ["1234567_1", "1234567_2", "1234567_3"]);
        let urls: Vec<_> = media.iter().map(|m| m.url.as_str()).collect();
        assert_eq!(
            urls,
            [
                "https://art.ngfiles.com/images/1/1234567_alice_ship.png?f1",
                "https://art.ngfiles.com/images/1/1234567_9_alice_ship.jpeg",
                "https://art.ngfiles.com/comments/1/c.png"
            ]
        );
        let k = keywords(&media[0]);
        assert_eq!(k["title"], "Big & Bold");
        assert_eq!(k["description"], "A & B");
        assert_eq!(
            (k["type"].clone(), k["_type"].clone()),
            (json!("art"), json!("i"))
        );
        assert_eq!(k["date"], "2024-01-01 00:00:00");
        assert_eq!(k["rating"], "t");
        assert_eq!(
            (k["width"].clone(), k["height"].clone()),
            (json!(800), json!(600))
        );
        assert_eq!(
            (k["index"].clone(), k["_index"].clone()),
            (json!(1234567), json!("1234567"))
        );
        assert_eq!(k["num"], 0);
        assert_eq!(k["favorites"], 1234);
        assert_eq!(k["score"], json!(4.52));
        assert_eq!(k["tags"], json!(["alpha", "zeta"]));
        assert_eq!(k["artist"], json!(["alice", "bob"]));
        assert_eq!(k["user"], "alice");
        assert_eq!(k["comment"], "Hello\nthere & you");
        assert_eq!(k["slug"], "ship");
        assert_eq!(
            k["post_url"],
            "https://www.newgrounds.com/art/view/alice/ship"
        );
        assert_eq!(k["subcategory"], "image");
        let meta = media[0].metadata.as_ref().unwrap();
        assert_eq!(meta.rating.as_deref(), Some("t"));
        assert_eq!(meta.caption.as_deref(), Some("Big & Bold"));
        assert_eq!(meta.timestamp, Some(1704067200));
        assert_eq!(
            paths(&media[0]),
            (
                "newgrounds_1234567_Big & Bold.png".into(),
                ["newgrounds", "alice, bob"].iter().collect::<PathBuf>()
            )
        );
        let second = keywords(&media[1]);
        assert_eq!(
            (second["num"].clone(), second["_index"].clone()),
            (json!(1), json!("1234567_01"))
        );
        assert_eq!(paths(&media[1]).0, "newgrounds_1234567_01_Big & Bold.jpeg");
        // The rejected off-site image keeps its upstream position.
        assert_eq!(keywords(&media[2])["_index"], "1234567_03");
        assert!(media[0].archive_key().unwrap().starts_with("newgrounds:"));
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /art/view/alice/ship "));
        assert!(requests[1].contains("ng_session=secret"), "{}", requests[1]);
    }

    #[tokio::test]
    async fn fetches_movies_audio_and_skips_adult_wall() {
        let page = r#"<meta property="og:title" content="Space"><meta property="og:type" content="video">
<meta property="og:description" content="">
<meta itemprop="description" content="Long &amp; story"><div class="rated-m"></div>
<div class="item-user"><a href="https://carol.newgrounds.com"></a></div>"#;
        let sources = json!({"sources": {
            "360p": [{"src": "https://uploads.ungrounded.net/alternate/1/5_alternate.360p.mp4?1600000000"}],
            "720p": [{"src": "https://uploads.ungrounded.net/alternate/1/5_alternate.720p.mp4?1600000000"}]}});
        let (root, server) = serve(vec![ok(page), ok(&sources.to_string())]);
        let options = Map::from_iter([("format".into(), json!("720p"))]);
        let native = newgrounds(root).with_options(Some(&options)).unwrap();
        let media = collect(&native, "https://www.newgrounds.com/portal/view/5").await;
        assert_eq!(media.len(), 1);
        assert!(media[0].url.as_str().contains(".720p.mp4"));
        assert_eq!(
            media[0].handoff["_fallback"],
            json!(["https://uploads.ungrounded.net/alternate/1/5_alternate.360p.mp4?1600000000"])
        );
        let k = keywords(&media[0]);
        assert_eq!(
            (k["type"].clone(), k["_type"].clone()),
            (json!("movie"), json!(""))
        );
        assert_eq!(k["description"], "Long & story");
        assert_eq!(k["rating"], "m");
        assert_eq!(k["date"], "2020-09-13 12:26:40");
        assert_eq!(k["user"], "carol");
        assert_eq!(media[0].metadata.as_ref().unwrap().media_id, "p5_1");
        assert_eq!(paths(&media[0]).0, "newgrounds_5_Space.mp4");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /portal/view/5/format/flash "));
        assert!(requests[1].starts_with("GET /portal/video/5 "));
        assert!(
            requests[1]
                .to_ascii_lowercase()
                .contains("x-requested-with: xmlhttprequest")
        );

        let audio = r#"<meta property="og:title" content="Song"><meta property="og:description" content="Tune">
<meta itemprop="datePublished" content="2024-01-01T00:00:00Z">
<script>var p = {"url":"https:\/\/audio.ngfiles.com\/7000\/7_song.mp3?f1","x":1}</script>
<div class="item-user"><a href="https://dave.newgrounds.com"></a></div>"#;
        let (root, server) = serve(vec![
            ok(audio),
            ok(r#"<div id="adults_only"><span class="highlight">Adults only</span></div>"#),
        ]);
        let options = Map::from_iter([("flash".into(), json!(false))]);
        let native = newgrounds(root).with_options(Some(&options)).unwrap();
        let media = collect(&native, "https://www.newgrounds.com/audio/listen/7").await;
        assert_eq!(
            media[0].url.as_str(),
            "https://audio.ngfiles.com/7000/7_song.mp3?f1"
        );
        assert_eq!(media[0].metadata.as_ref().unwrap().media_id, "a7_1");
        assert_eq!(keywords(&media[0])["type"], "audio");
        let error = native
            .stream(&Url::parse("https://www.newgrounds.com/portal/view/9").unwrap())
            .collect::<Vec<_>>()
            .await
            .remove(0)
            .unwrap_err()
            .to_string();
        assert!(error.contains("session cookies"), "{error}");
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /portal/view/9 "));
        // NG Guard's HTTP 403 page must not look like an empty listing.
        let (root, server) = serve(vec![response("403 Forbidden", b"<title>NG Guard</title>")]);
        let error = newgrounds(root)
            .stream(&Url::parse("https://alice.newgrounds.com/art").unwrap())
            .collect::<Vec<_>>()
            .await
            .remove(0)
            .unwrap_err()
            .to_string();
        assert!(error.contains("NG Guard"), "{error}");
        server.join().unwrap();
        let bad = Map::from_iter([("format".into(), json!({"a": 1}))]);
        assert!(
            Newgrounds::new(Duration::from_secs(1))
                .unwrap()
                .with_options(Some(&bad))
                .is_err()
        );
    }

    #[tokio::test]
    async fn paginates_user_art_and_skips_adult_posts() {
        let page = |slug: &str, more: &str| {
            json!({"items": {"2024": [format!("<a href=\"https://www.newgrounds.com/art/view/alice/{slug}\">")]}, "load_more": more})
                .to_string()
        };
        let (root, server) = serve(vec![
            ok(&page("ship", "<a>more page</a>")),
            ok(&post_html()),
            ok(&page("adult", "")),
            ok(r#"<div id="adults_only"></div>"#),
        ]);
        let media = collect(&newgrounds(root), "https://alice.newgrounds.com/art").await;
        assert_eq!(media.len(), 3);
        let k = keywords(&media[0]);
        assert_eq!(
            (k["subcategory"].clone(), k["user"].clone()),
            (json!("art"), json!("alice"))
        );
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with("GET /art?page=1&isAjaxRequest=1 "),
            "{}",
            requests[0]
        );
        assert!(
            requests[0]
                .to_ascii_lowercase()
                .contains("x-requested-with: xmlhttprequest")
        );
        assert!(
            requests[0]
                .to_ascii_lowercase()
                .contains("referer: https://alice.newgrounds.com/art")
        );
        assert!(requests[2].starts_with("GET /art?page=2&isAjaxRequest=1 "));
        assert!(requests[3].starts_with("GET /art/view/alice/adult "));
    }

    #[tokio::test]
    async fn favorites_following_search_and_user_views() {
        let component = |hrefs: &[&str]| {
            json!({"component": hrefs.iter().map(|h| format!("<a href=\"https://www.newgrounds.com{h}\">")).collect::<String>()})
                .to_string()
        };
        let (root, server) = serve(vec![
            ok(&component(&["/art/view/alice/ship"])),
            ok(&post_html()),
            ok(&component(&[])),
            ok(&component(&[])),
        ]);
        let media = collect(&newgrounds(root), "https://bob.newgrounds.com/favorites").await;
        assert_eq!(media.len(), 3);
        let k = keywords(&media[0]);
        assert_eq!(
            (k["subcategory"].clone(), k["user"].clone()),
            (json!("favorite"), json!("bob"))
        );
        assert_eq!(
            paths(&media[0]).1,
            ["newgrounds", "bob", "Favorites"]
                .iter()
                .collect::<PathBuf>()
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /favorites/art?page=1&isAjaxRequest=1 "));
        assert!(requests[2].starts_with("GET /favorites/audio?page=1&isAjaxRequest=1 "));
        assert!(requests[3].starts_with("GET /favorites/movies?page=1&isAjaxRequest=1 "));

        let users = json!({"component": "<div class=\"item-user\"><a href=\"https://carol.newgrounds.com\"><img></a></div><div class=\"item-user\"><a href=\"https://dave.newgrounds.com\"><img></a></div>"});
        let (root, server) = serve(vec![ok(&users.to_string())]);
        let events: Vec<_> = newgrounds(root)
            .with_include(Some(crate::dispatch::Include::Views(vec![])))
            .events(&Url::parse("https://bob.newgrounds.com/favorites/following").unwrap())
            .collect()
            .await;
        let children = events
            .iter()
            .filter(|e| matches!(e, Ok(ExtractionEvent::BeginChild(_))))
            .count();
        assert_eq!(children, 2);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /favorites/following?page=1&isAjaxRequest=1 "));

        let content = json!({"content": "<a href=\"/search/conduct/art?page=2\">next</a><a href=\"https://www.newgrounds.com/art/view/alice/ship\">x</a>"});
        let (root, server) = serve(vec![
            ok(&content.to_string()),
            ok(&post_html()),
            ok(&json!({"content": ""}).to_string()),
        ]);
        let media = collect(
            &newgrounds(root),
            "https://www.newgrounds.com/search/conduct/art?terms=space+ship&page=2",
        )
        .await;
        let k = keywords(&media[0]);
        assert_eq!(k["search_tags"], "space ship");
        assert_eq!(
            paths(&media[0]).1,
            ["newgrounds", "search", "space ship"]
                .iter()
                .collect::<PathBuf>()
        );
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with("GET /search/conduct/art?terms=space+ship&inner=1&page=2 "),
            "{}",
            requests[0]
        );
        assert!(requests[2].contains("&page=3 "));

        let empty = json!({"items": {}}).to_string();
        let (root, server) = serve(vec![ok(&empty), ok(&empty), ok(&empty), ok(&empty)]);
        let media = collect(
            &newgrounds(root).with_include(Some(crate::dispatch::Include::All)),
            "https://alice.newgrounds.com/",
        )
        .await;
        assert!(media.is_empty());
        let requests = server.join().unwrap();
        let paths: Vec<_> = requests
            .iter()
            .map(|r| r.split(' ').nth(1).unwrap().split('?').next().unwrap())
            .collect();
        assert_eq!(paths, ["/art", "/audio", "/games", "/movies"]);
    }
}
