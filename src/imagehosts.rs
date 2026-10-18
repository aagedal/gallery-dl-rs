//! Experimental native extraction for simple file and image hosts.
//!
//! Mirrors gallery_dl/extractor/pixeldrain.py, catbox.py, imgbox.py and the
//! postimg extractors in imagehosts.py. Each site is its own category:
//!
//! - `pixeldrain`: files (`/u/ID`) via `/api/file/ID/info` and lists (`/l/ID`)
//!   via `/api/list/ID`; files download from `/api/file/ID?download`.
//! - `catbox`: albums (`/c/ID`) parsed from the album page; plain
//!   `files.catbox.moe` links are not site hosts and stay direct downloads.
//! - `imgbox`: galleries (`/g/ID`), whose thumbnail URLs map to the original
//!   images, and single image pages (`/ID`).
//! - `postimg`: single image pages and gallery pages; gallery thumbnails link
//!   to image pages, which are fetched one by one.
//!
//! Media URLs must be HTTPS on the site's own file CDN.
use crate::{
    Result, error,
    event::{ExtractionEvent, group},
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
    " (native image host extractor)"
);
const MAX_RESPONSE: usize = 16 * 1024 * 1024;
/// Upper bound on per-image page requests for one postimg gallery.
const MAX_GALLERY_PAGES: usize = 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Site {
    Pixeldrain,
    Catbox,
    Imgbox,
    Postimg,
}

impl Site {
    pub fn category(self) -> &'static str {
        match self {
            Self::Pixeldrain => "pixeldrain",
            Self::Catbox => "catbox",
            Self::Imgbox => "imgbox",
            Self::Postimg => "postimg",
        }
    }

    pub fn from_category(category: &str) -> Option<Self> {
        Some(match category {
            "pixeldrain" => Self::Pixeldrain,
            "catbox" => Self::Catbox,
            "imgbox" => Self::Imgbox,
            "postimg" => Self::Postimg,
            _ => return None,
        })
    }

    /// Site for a page host (optionally `www.`); CDN hosts are not page hosts.
    pub fn from_host(host: &str) -> Option<Self> {
        Some(match host.strip_prefix("www.").unwrap_or(host) {
            "pixeldrain.com" => Self::Pixeldrain,
            "catbox.moe" => Self::Catbox,
            "imgbox.com" => Self::Imgbox,
            "postimg.cc" | "postimages.org" => Self::Postimg,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Self::Pixeldrain => "Pixeldrain",
            Self::Catbox => "Catbox",
            Self::Imgbox => "imgbox",
            Self::Postimg => "Postimg",
        }
    }

    fn root(self) -> &'static str {
        match self {
            Self::Pixeldrain => "https://pixeldrain.com/",
            Self::Catbox => "https://catbox.moe/",
            Self::Imgbox => "https://imgbox.com/",
            Self::Postimg => "https://postimg.cc/",
        }
    }

    /// Whether a media URL is HTTPS on this site's file host.
    pub fn media_host(self, url: &Url) -> bool {
        let Some(host) = url.host_str() else {
            return false;
        };
        url.scheme() == "https"
            && url.port().is_none()
            && url.username().is_empty()
            && url.password().is_none()
            && match self {
                Self::Pixeldrain => host == "pixeldrain.com",
                Self::Catbox => host == "files.catbox.moe",
                Self::Imgbox => host
                    .strip_suffix(".imgbox.com")
                    .and_then(|sub| sub.strip_prefix("images"))
                    .is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit())),
                Self::Postimg => host == "i.postimg.cc",
            }
    }
}

fn alnum(id: &str, len: std::ops::RangeInclusive<usize>) -> bool {
    len.contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// File/image IDs used as media IDs and archive keys.
pub fn valid_media_id(site: Site, id: &str) -> bool {
    match site {
        Site::Pixeldrain | Site::Postimg => alnum(id, 1..=32),
        Site::Catbox => alnum(id, 1..=32),
        Site::Imgbox => alnum(id, 8..=8),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    PixeldrainFile(String),
    PixeldrainList(String),
    CatboxAlbum(String),
    ImgboxGallery(String),
    ImgboxImage(String),
    PostimgImage(String),
    PostimgGallery(String),
}

impl Target {
    pub fn site(&self) -> Site {
        match self {
            Self::PixeldrainFile(_) | Self::PixeldrainList(_) => Site::Pixeldrain,
            Self::CatboxAlbum(_) => Site::Catbox,
            Self::ImgboxGallery(_) | Self::ImgboxImage(_) => Site::Imgbox,
            Self::PostimgImage(_) | Self::PostimgGallery(_) => Site::Postimg,
        }
    }
}

pub fn target(url: &Url) -> Result<Target> {
    let unsupported = || {
        error(
            "unsupported image host URL; expected pixeldrain.com/u/ID or /l/ID, catbox.moe/c/ID, imgbox.com/g/ID or /ID, postimg.cc/ID or /gallery/ID",
        )
    };
    let site = url
        .host_str()
        .and_then(Site::from_host)
        .ok_or_else(unsupported)?;
    if !matches!(url.scheme(), "http" | "https")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(unsupported());
    }
    let segments: Vec<_> = url
        .path_segments()
        .map(|segments| segments.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    let id = |id: &str, len| alnum(id, len).then(|| id.to_owned());
    match (site, segments.as_slice()) {
        (Site::Pixeldrain, ["u", file]) => id(file, 1..=32).map(Target::PixeldrainFile),
        (Site::Pixeldrain, ["l", list]) => id(list, 1..=32).map(Target::PixeldrainList),
        (Site::Catbox, ["c", album]) => id(album, 1..=32).map(Target::CatboxAlbum),
        (Site::Imgbox, ["g", gallery]) => id(gallery, 10..=10).map(Target::ImgboxGallery),
        (Site::Imgbox, [image]) => id(image, 8..=8).map(Target::ImgboxImage),
        (Site::Postimg, ["gallery", gallery]) => id(gallery, 1..=32).map(Target::PostimgGallery),
        (Site::Postimg, ["image" | "img", image] | [image]) if *image != "gallery" => {
            id(image, 1..=32).map(Target::PostimgImage)
        }
        _ => None,
    }
    .ok_or_else(unsupported)
}

// ---------------------------------------------------------------------------
// Small HTML scanning helpers.

/// Text between the first `start` and the following `end`.
fn between<'a>(text: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let rest = &text[text.find(start)? + start.len()..];
    Some(&rest[..rest.find(end)?])
}

/// Every non-overlapping text between `start` and `end`.
fn between_all<'a>(text: &'a str, start: &'a str, end: &'a str) -> impl Iterator<Item = &'a str> {
    let mut rest = text;
    std::iter::from_fn(move || {
        let from = rest.find(start)? + start.len();
        let len = rest[from..].find(end)?;
        let found = &rest[from..from + len];
        rest = &rest[from + len + end.len()..];
        Some(found)
    })
}

/// The opening tag (`<...>`) containing byte offset `at`.
fn tag_around(text: &str, at: usize) -> Option<&str> {
    let open = text[..at].rfind('<')?;
    let close = at + text[at..].find('>')?;
    Some(&text[open..=close])
}

/// A double- or single-quoted attribute value from one tag.
fn attribute<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    for quote in ['"', '\''] {
        let needle = format!(" {name}={quote}");
        if let Some(start) = tag.find(&needle) {
            let rest = &tag[start + needle.len()..];
            return Some(&rest[..rest.find(quote)?]);
        }
    }
    None
}

/// `content` of `<meta property="NAME">`.
fn meta_property<'a>(html: &'a str, name: &str) -> Option<&'a str> {
    for quote in ['"', '\''] {
        let needle = format!("property={quote}{name}{quote}");
        if let Some(at) = html.find(&needle) {
            return tag_around(html, at).and_then(|tag| attribute(tag, "content"));
        }
    }
    None
}

/// Decode the common named and numeric HTML entities.
fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let decoded = rest[1..].find(';').filter(|&n| n <= 10).and_then(|n| {
            let entity = &rest[1..=n];
            let ch = match entity {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some('\u{a0}'),
                _ => entity
                    .strip_prefix("#x")
                    .or_else(|| entity.strip_prefix("#X"))
                    .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                    .or_else(|| entity.strip_prefix('#').and_then(|d| d.parse().ok()))
                    .and_then(char::from_u32),
            }?;
            Some((ch, n + 2))
        });
        match decoded {
            Some((ch, len)) => {
                out.push(ch);
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn clean_text(text: &str) -> Option<String> {
    let text = unescape(text.trim());
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

fn clean_extension(ext: &str) -> Option<String> {
    let ext = ext.to_ascii_lowercase();
    ((1..=8).contains(&ext.len()) && ext.bytes().all(|b| b.is_ascii_alphanumeric()))
        .then(|| if ext == "jpeg" { "jpg".into() } else { ext })
}

fn extension_of(name: &str) -> Option<String> {
    name.rsplit_once('.')
        .and_then(|(_, ext)| clean_extension(ext))
}

fn url_extension(url: &Url) -> Option<String> {
    url.path_segments()?.next_back().and_then(extension_of)
}

fn mime_extension(mime: &str) -> Option<String> {
    let sub = mime.split(';').next()?.trim().split_once('/')?.1;
    clean_extension(match sub {
        "quicktime" => "mov",
        "x-matroska" => "mkv",
        "svg+xml" => "svg",
        "mpeg" if mime.starts_with("audio/") => "mp3",
        other => other,
    })
}

/// Parse a media URL (unescaping HTML) and require the site's HTTPS CDN.
fn media_url(site: Site, raw: &str) -> Option<Url> {
    let url = Url::parse(&unescape(raw.trim())).ok()?;
    site.media_host(&url).then_some(url)
}

struct Item {
    url: Url,
    media_id: String,
    ext: String,
    name: Option<String>,
    mime: Option<String>,
    timestamp: Option<i64>,
}

/// Build media for one post/gallery; `single` uses file-ID filenames.
fn build(
    site: Site,
    post_id: &str,
    caption: Option<String>,
    timestamp: Option<i64>,
    items: Vec<Item>,
    single: bool,
) -> Vec<Media> {
    let caption: Option<Arc<str>> = caption.map(Arc::from);
    let mut seen = HashSet::new();
    let mut media = Vec::new();
    for item in items {
        if !site.media_host(&item.url)
            || !valid_media_id(site, &item.media_id)
            || !seen.insert(item.media_id.clone())
        {
            continue;
        }
        let index = media.len() + 1;
        let filename = if single {
            format!("{}.{}", item.media_id, item.ext)
        } else {
            format!("{post_id}_{index:02}.{}", item.ext)
        };
        media.push(Media {
            url: item.url,
            job: MediaJob::DirectHttp,
            category: site.category().into(),
            filename,
            metadata: Some(PostMetadata {
                file: Some(FileMetadata {
                    media_type: item.mime.map(Arc::from),
                    description: item.name.clone().map(Arc::from),
                    ..FileMetadata::default()
                }),
                post_id: post_id.into(),
                media_id: item.media_id,
                caption: caption
                    .clone()
                    .or_else(|| single.then(|| item.name.map(Arc::from)).flatten()),
                timestamp: item.timestamp.or(timestamp),
                index,
                ..PostMetadata::default()
            }),
            headers: BTreeMap::new(),
            handoff: BTreeMap::new(),
        });
    }
    media
}

// ---------------------------------------------------------------------------
// Pixeldrain.

fn json(site: Site, bytes: &[u8]) -> Result<Value> {
    serde_json::from_slice(bytes)
        .map_err(|_| error(format!("{} returned invalid JSON", site.name())))
}

fn string<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn pixeldrain_item(file: &Value) -> Option<Item> {
    let id = string(file, "id").filter(|id| alnum(id, 1..=32))?;
    let name = string(file, "name").map(str::to_owned);
    let mime = string(file, "mime_type").map(str::to_owned);
    let ext = name
        .as_deref()
        .and_then(extension_of)
        .or_else(|| mime.as_deref().and_then(mime_extension))
        .unwrap_or_else(|| "bin".into());
    let url = Url::parse(&format!("https://pixeldrain.com/api/file/{id}?download")).ok()?;
    Some(Item {
        url,
        media_id: id.into(),
        ext,
        name,
        mime,
        timestamp: string(file, "date_upload").and_then(|d| crate::date::parse_date(d).ok()),
    })
}

/// Parse `/api/file/ID/info`.
pub fn parse_pixeldrain_file(bytes: &[u8], id: &str) -> Result<Vec<Media>> {
    let body = json(Site::Pixeldrain, bytes)?;
    let item = pixeldrain_item(&body)
        .filter(|item| item.media_id == id)
        .ok_or_else(|| error("Pixeldrain response does not match the requested file"))?;
    Ok(build(Site::Pixeldrain, id, None, None, vec![item], true))
}

/// Parse `/api/list/ID`.
pub fn parse_pixeldrain_list(bytes: &[u8], id: &str) -> Result<Vec<Media>> {
    let body = json(Site::Pixeldrain, bytes)?;
    if string(&body, "id").is_some_and(|found| found != id) {
        return Err(error(
            "Pixeldrain response does not match the requested list",
        ));
    }
    let files = body
        .get("files")
        .and_then(Value::as_array)
        .ok_or_else(|| error("Pixeldrain response is not a file list"))?;
    let items = files.iter().filter_map(pixeldrain_item).collect();
    let title = string(&body, "title").map(str::to_owned);
    let timestamp = string(&body, "date_created").and_then(|d| crate::date::parse_date(d).ok());
    non_empty(
        Site::Pixeldrain,
        build(Site::Pixeldrain, id, title, timestamp, items, false),
    )
}

fn non_empty(site: Site, media: Vec<Media>) -> Result<Vec<Media>> {
    if media.is_empty() {
        return Err(error(format!("{} page contains no media", site.name())));
    }
    Ok(media)
}

fn html(site: Site, bytes: &[u8]) -> Result<&str> {
    std::str::from_utf8(bytes).map_err(|_| error(format!("{} returned invalid HTML", site.name())))
}

// ---------------------------------------------------------------------------
// Catbox.

/// `DD Month YYYY` (as on catbox album pages) to Unix seconds at midnight UTC.
fn day_month_year(text: &str) -> Option<i64> {
    const MONTHS: [&str; 12] = [
        "january",
        "february",
        "march",
        "april",
        "may",
        "june",
        "july",
        "august",
        "september",
        "october",
        "november",
        "december",
    ];
    let mut parts = text.split_whitespace();
    let day: u8 = parts.next()?.parse().ok()?;
    let month = parts.next()?.to_ascii_lowercase();
    let month = MONTHS.iter().position(|m| *m == month)? as u8 + 1;
    let year: i32 = parts.next()?.parse().ok()?;
    let date =
        time::Date::from_calendar_date(year, time::Month::try_from(month).ok()?, day).ok()?;
    Some(date.midnight().assume_utc().unix_timestamp())
}

/// Parse an album page: `<h1>` title, `Uploaded DATE` and file links.
pub fn parse_catbox_album(bytes: &[u8], id: &str) -> Result<Vec<Media>> {
    let page = html(Site::Catbox, bytes)?;
    let title = between(page, "<h1>", "<").and_then(clean_text);
    let timestamp = between(page, "<p>Uploaded ", "</p>").and_then(day_month_year);
    let items = between_all(page, "href=\"https://files.catbox.moe/", "\"")
        .filter_map(|name| {
            if !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.') {
                return None;
            }
            let url = media_url(Site::Catbox, &format!("https://files.catbox.moe/{name}"))?;
            let name = url.path().strip_prefix('/')?.to_owned();
            let (stem, ext) = name.rsplit_once('.')?;
            Some(Item {
                media_id: stem.into(),
                ext: clean_extension(ext)?,
                url,
                name: Some(name),
                mime: None,
                timestamp: None,
            })
        })
        .collect();
    non_empty(
        Site::Catbox,
        build(Site::Catbox, id, title, timestamp, items, false),
    )
}

// ---------------------------------------------------------------------------
// imgbox.

/// `https://thumbs2.imgbox.com/xx/yy/ID_t.ext` → original image URL and ID.
pub fn imgbox_original(thumb: &str) -> Option<(Url, String)> {
    let thumb = Url::parse(&unescape(thumb)).ok()?;
    let host = thumb.host_str()?;
    let n = host
        .strip_suffix(".imgbox.com")?
        .strip_prefix("thumbs")
        .filter(|n| n.bytes().all(|b| b.is_ascii_digit()))?;
    if thumb.scheme() != "https" || thumb.port().is_some() {
        return None;
    }
    let path = thumb.path().strip_prefix('/')?;
    let segments: Vec<_> = path.split('/').collect();
    let [a, b, file] = segments.as_slice() else {
        return None;
    };
    if !alnum(a, 1..=4) || !alnum(b, 1..=4) {
        return None;
    }
    let (stem, ext) = file.rsplit_once('.')?;
    let id = stem.strip_suffix("_t")?;
    let ext = clean_extension(ext)?;
    if !valid_media_id(Site::Imgbox, id) {
        return None;
    }
    let url = Url::parse(&format!(
        "https://images{n}.imgbox.com/{a}/{b}/{id}_o.{ext}"
    ))
    .ok()?;
    Some((url, id.into()))
}

/// Parse a gallery page: `<h1>` title and thumbnails linked to `/ID` pages.
pub fn parse_imgbox_gallery(bytes: &[u8], id: &str) -> Result<Vec<Media>> {
    let page = html(Site::Imgbox, bytes)?;
    let title = between(page, "<h1>", "</h1>").and_then(clean_text);
    let mut items = Vec::new();
    for (at, _) in page.match_indices("<img") {
        let Some(tag) = tag_around(page, at + 1) else {
            continue;
        };
        let Some((url, media_id)) = attribute(tag, "src").and_then(imgbox_original) else {
            continue;
        };
        items.push(Item {
            ext: url_extension(&url).unwrap_or_else(|| "jpg".into()),
            name: attribute(tag, "title")
                .or_else(|| attribute(tag, "alt"))
                .and_then(clean_text),
            url,
            media_id,
            mime: None,
            timestamp: None,
        });
    }
    non_empty(
        Site::Imgbox,
        build(Site::Imgbox, id, title, None, items, false),
    )
}

/// Parse an image page: the `id="img"` element (or `og:image`).
pub fn parse_imgbox_image(bytes: &[u8], id: &str) -> Result<Vec<Media>> {
    let page = html(Site::Imgbox, bytes)?;
    let tag = page
        .find(" id=\"img\"")
        .and_then(|at| tag_around(page, at + 1));
    let url = tag
        .and_then(|tag| attribute(tag, "src"))
        .and_then(|src| media_url(Site::Imgbox, src))
        .or_else(|| meta_property(page, "og:image").and_then(|src| media_url(Site::Imgbox, src)))
        .ok_or_else(|| error("imgbox page has no image"))?;
    let name = tag
        .and_then(|tag| attribute(tag, "title").or_else(|| attribute(tag, "alt")))
        .and_then(clean_text);
    let item = Item {
        ext: url_extension(&url).unwrap_or_else(|| "jpg".into()),
        url,
        media_id: id.into(),
        name,
        mime: None,
        timestamp: None,
    };
    non_empty(
        Site::Imgbox,
        build(Site::Imgbox, id, None, None, vec![item], true),
    )
}

// ---------------------------------------------------------------------------
// Postimg.

/// Parse an image page: the `href` of the `id="download"` link (as upstream),
/// falling back to `og:image`, and the `imagename` caption.
pub fn parse_postimg_image(bytes: &[u8], id: &str) -> Result<Vec<Media>> {
    let page = html(Site::Postimg, bytes)?;
    let download = page
        .find(" id=\"download\"")
        .and_then(|at| tag_around(page, at + 1))
        .and_then(|tag| attribute(tag, "href"))
        .and_then(|href| media_url(Site::Postimg, href));
    let url = download
        .or_else(|| meta_property(page, "og:image").and_then(|src| media_url(Site::Postimg, src)))
        .ok_or_else(|| error("Postimg page has no image"))?;
    let name = between(page, " class=\"imagename\">", "<").and_then(clean_text);
    let ext = url_extension(&url)
        .or_else(|| name.as_deref().and_then(extension_of))
        .unwrap_or_else(|| "jpg".into());
    let item = Item {
        url,
        media_id: id.into(),
        ext,
        name,
        mime: None,
        timestamp: None,
    };
    non_empty(
        Site::Postimg,
        build(Site::Postimg, id, None, None, vec![item], true),
    )
}

/// Image page IDs linked from gallery thumbnails (` class="thumb"><a href=`).
pub fn parse_postimg_gallery(bytes: &[u8]) -> Result<(Option<String>, Vec<String>)> {
    let page = html(Site::Postimg, bytes)?;
    let title = meta_property(page, "og:title").and_then(clean_text);
    let mut seen = HashSet::new();
    let ids: Vec<String> = between_all(page, " class=\"thumb\"><a href=\"", "\"")
        .filter_map(|href| {
            let url = Url::parse(&unescape(href)).ok()?;
            if url.scheme() != "https"
                || url.host_str().and_then(Site::from_host) != Some(Site::Postimg)
            {
                return None;
            }
            match target(&url).ok()? {
                Target::PostimgImage(id) => Some(id),
                _ => None,
            }
        })
        .filter(|id| seen.insert(id.clone()))
        .take(MAX_GALLERY_PAGES)
        .collect();
    if ids.is_empty() {
        return Err(error("Postimg gallery contains no images"));
    }
    Ok((title, ids))
}

// ---------------------------------------------------------------------------
// Client.

pub struct ImageHosts {
    client: Client,
    network: NetworkOptions,
    root: Option<Url>,
    timeout: Duration,
    request_timeout: Duration,
    retries: u8,
    rate_limits: RateLimits,
}

impl ImageHosts {
    pub fn new(timeout: Duration) -> Result<Self> {
        Ok(Self {
            client: NetworkOptions::default().client(USER_AGENT, timeout)?,
            network: NetworkOptions::default(),
            root: None,
            timeout,
            request_timeout: timeout,
            retries: 0,
            rate_limits: RateLimits::default(),
        })
    }

    /// Apply proxy and user-agent settings to page requests and emitted media.
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

    /// Override the page/API origin for every site; intended for local tests.
    pub fn with_api_root(mut self, root: Url) -> Self {
        self.root = Some(root);
        self
    }

    async fn call(&self, site: Site, path: &str) -> Result<Vec<u8>> {
        let root = self
            .root
            .clone()
            .unwrap_or_else(|| Url::parse(site.root()).expect("known root"));
        let endpoint = root.join(path).expect("validated path");
        tokio::time::timeout(self.timeout, self.request(site, &endpoint))
            .await
            .map_err(|_| error(format!("native {} extraction timed out", site.name())))?
    }

    async fn extract(&self, target: Target) -> Result<Vec<Media>> {
        let site = target.site();
        match target {
            Target::PixeldrainFile(id) => {
                parse_pixeldrain_file(&self.call(site, &format!("api/file/{id}/info")).await?, &id)
            }
            Target::PixeldrainList(id) => {
                parse_pixeldrain_list(&self.call(site, &format!("api/list/{id}")).await?, &id)
            }
            Target::CatboxAlbum(id) => {
                parse_catbox_album(&self.call(site, &format!("c/{id}")).await?, &id)
            }
            Target::ImgboxGallery(id) => {
                parse_imgbox_gallery(&self.call(site, &format!("g/{id}")).await?, &id)
            }
            Target::ImgboxImage(id) => parse_imgbox_image(&self.call(site, &id).await?, &id),
            Target::PostimgImage(id) => parse_postimg_image(&self.call(site, &id).await?, &id),
            Target::PostimgGallery(id) => {
                let page = self.call(site, &format!("gallery/{id}")).await?;
                let (title, images) = parse_postimg_gallery(&page)?;
                let mut items = Vec::new();
                for image in images {
                    let page = self.call(site, &image).await?;
                    let mut media = parse_postimg_image(&page, &image)?;
                    let media = media.remove(0);
                    let meta = media.metadata.expect("built with metadata");
                    let file = meta.file.unwrap_or_default();
                    items.push(Item {
                        ext: media
                            .filename
                            .rsplit_once('.')
                            .map_or_else(|| "jpg".into(), |(_, e)| e.to_owned()),
                        url: media.url,
                        media_id: meta.media_id,
                        name: file.description.map(|d| d.to_string()),
                        mime: None,
                        timestamp: None,
                    });
                }
                non_empty(site, build(site, &id, title, None, items, false))
            }
        }
    }

    pub fn stream(&self, url: &Url) -> BoxStream<'_, Result<Media>> {
        crate::event::media_only(self.events(url))
    }

    /// Emit one post boundary per file, list, album or gallery.
    pub fn events(&self, url: &Url) -> BoxStream<'_, Result<ExtractionEvent>> {
        let target = match target(url) {
            Ok(target) => target,
            Err(e) => return stream::once(async move { Err(e) }).boxed(),
        };
        stream::once(async move {
            let category = target.site().category();
            let mut media = self.extract(target).await?;
            for item in &mut media {
                self.network.apply_media(item);
            }
            Ok::<VecDeque<_>, crate::Error>(group(media, category))
        })
        .map(|result| match result {
            Ok(events) => stream::iter(events.into_iter().map(Ok)).boxed(),
            Err(e) => stream::once(async move { Err(e) }).boxed(),
        })
        .flatten()
        .boxed()
    }

    async fn request(&self, site: Site, endpoint: &Url) -> Result<Vec<u8>> {
        for attempt in 0..=self.retries {
            match self.attempt(site, endpoint, attempt).await {
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
        site: Site,
        endpoint: &Url,
        attempt: u8,
    ) -> std::result::Result<Vec<u8>, Failure> {
        let name = site.name();
        let mut target = endpoint.clone();
        let mut redirects = Redirects::new(endpoint);
        let mut response = loop {
            self.rate_limits.wait(&target, self.timeout).await?;
            let response = self
                .client
                .get(target.clone())
                .timeout(self.request_timeout)
                .send()
                .await
                .map_err(|_| {
                    Failure::transient(format!("{name} request failed; check network access"))
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
                        Some(delay) => {
                            format!("{name} rate limit; retry after {} seconds", delay.as_secs())
                        }
                        None => format!("{name} rate limit; retry later"),
                    }
                } else {
                    format!("{name} returned HTTP {status}")
                },
            ));
        }
        if matches!(status.as_u16(), 404 | 410) {
            return Err(error(format!("{name} file or gallery not found")).into());
        }
        if matches!(status.as_u16(), 401 | 403) {
            return Err(error(format!("{name} refused access")).into());
        }
        if !status.is_success() {
            return Err(error(format!("{name} returned HTTP {status}")).into());
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| Failure::transient(format!("could not read {name} response")))?
        {
            if body.len() + chunk.len() > MAX_RESPONSE {
                return Err(error(format!("{name} response exceeded 16 MiB")).into());
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

    fn meta(media: &Media) -> &PostMetadata {
        media.metadata.as_ref().unwrap()
    }

    #[test]
    fn classifies_urls_and_cdn_hosts() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://pixeldrain.com/u/abc123").unwrap(),
            Target::PixeldrainFile("abc123".into())
        );
        assert_eq!(
            t("https://pixeldrain.com/l/Lst1").unwrap(),
            Target::PixeldrainList("Lst1".into())
        );
        assert_eq!(
            t("https://catbox.moe/c/ab12cd").unwrap(),
            Target::CatboxAlbum("ab12cd".into())
        );
        assert_eq!(
            t("https://imgbox.com/g/AbCdEfGhIj").unwrap(),
            Target::ImgboxGallery("AbCdEfGhIj".into())
        );
        assert_eq!(
            t("https://imgbox.com/AbCd1234").unwrap(),
            Target::ImgboxImage("AbCd1234".into())
        );
        assert_eq!(
            t("https://postimg.cc/image/xyz").unwrap(),
            Target::PostimgImage("xyz".into())
        );
        assert_eq!(
            t("https://www.postimages.org/gallery/G1").unwrap(),
            Target::PostimgGallery("G1".into())
        );
        for raw in [
            "https://pixeldrain.com/",
            "https://pixeldrain.com/u/a%2Fb",
            "https://catbox.moe/",
            "https://files.catbox.moe/abc.png",
            "https://imgbox.com/short",
            "https://imgbox.com/g/short",
            "https://postimg.cc/gallery",
            "https://i.postimg.cc/abc/x.png",
            "https://imgbox.com:444/AbCd1234",
            "https://imgbox.com.evil.example/AbCd1234",
        ] {
            assert!(t(raw).is_err(), "{raw}");
        }
        let host = |site: Site, raw: &str| site.media_host(&Url::parse(raw).unwrap());
        assert!(host(Site::Imgbox, "https://images2.imgbox.com/a/b/x_o.png"));
        assert!(!host(Site::Imgbox, "http://images2.imgbox.com/a/b/x_o.png"));
        assert!(!host(Site::Imgbox, "https://imagesx.imgbox.com/x"));
        assert!(!host(
            Site::Catbox,
            "https://files.catbox.moe.evil.example/x"
        ));
        assert!(!host(Site::Postimg, "https://postimg.cc/x.png"));
    }

    #[test]
    fn parses_pixeldrain_file_and_list() {
        let file = json!({"id": "abc123", "name": "Report.PDF", "size": 10,
            "mime_type": "application/pdf", "date_upload": "2024-01-02T03:04:05.123Z"});
        let media = parse_pixeldrain_file(file.to_string().as_bytes(), "abc123").unwrap();
        assert_eq!(media.len(), 1);
        assert_eq!(
            media[0].url.as_str(),
            "https://pixeldrain.com/api/file/abc123?download"
        );
        assert_eq!(media[0].category, "pixeldrain");
        assert_eq!(media[0].filename, "abc123.pdf");
        let m = meta(&media[0]);
        assert_eq!(
            (m.post_id.as_str(), m.media_id.as_str()),
            ("abc123", "abc123")
        );
        assert_eq!(m.caption.as_deref(), Some("Report.PDF"));
        assert_eq!(m.timestamp, Some(1704164645));
        let file_meta = m.file.as_ref().unwrap();
        assert_eq!(file_meta.media_type.as_deref(), Some("application/pdf"));
        assert_eq!(file_meta.description.as_deref(), Some("Report.PDF"));
        assert!(parse_pixeldrain_file(file.to_string().as_bytes(), "other").is_err());
        assert!(parse_pixeldrain_file(b"<html>", "abc123").is_err());

        let list = json!({"id": "L1", "title": "My list", "files": [
            {"id": "f1", "name": "a.png", "mime_type": "image/png"},
            {"id": "bad/id", "name": "b.png"},
            {"id": "f2", "mime_type": "video/mp4"},
            {"id": "f1", "name": "dup.png"}]});
        let media = parse_pixeldrain_list(list.to_string().as_bytes(), "L1").unwrap();
        assert_eq!(media.len(), 2);
        assert_eq!(media[0].filename, "L1_01.png");
        assert_eq!(media[1].filename, "L1_02.mp4");
        assert_eq!(meta(&media[1]).index, 2);
        assert_eq!(meta(&media[1]).caption.as_deref(), Some("My list"));
        assert_ne!(media[0].archive_key(), media[1].archive_key());
        assert!(media[0].archive_key().unwrap().starts_with("pixeldrain:"));
        assert!(parse_pixeldrain_list(b"{\"files\": []}", "L1").is_err());
        assert!(parse_pixeldrain_list(list.to_string().as_bytes(), "L2").is_err());
    }

    const CATBOX: &str = r#"<html><body><div class="title"><h1>Cats &amp; Dogs</h1>
        <p>Uploaded 05 March 2023</p><p>desc</p></div>
        <a href="https://files.catbox.moe/abc123.PNG" target="_blank">x</a>
        <a href="https://files.catbox.moe/def456.mp4">y</a>
        <a href="https://files.catbox.moe/abc123.PNG">dup</a>
        <a href="https://files.catbox.moe/../evil.png">bad</a>
        <a href="https://catbox.moe/c/other">album</a></body></html>"#;

    #[test]
    fn parses_catbox_album() {
        let media = parse_catbox_album(CATBOX.as_bytes(), "alb1").unwrap();
        assert_eq!(media.len(), 2, "{media:?}");
        assert_eq!(media[0].url.as_str(), "https://files.catbox.moe/abc123.PNG");
        assert_eq!(media[0].filename, "alb1_01.png");
        assert_eq!(media[1].filename, "alb1_02.mp4");
        let m = meta(&media[0]);
        assert_eq!(m.caption.as_deref(), Some("Cats & Dogs"));
        assert_eq!(m.media_id, "abc123");
        assert_eq!(m.timestamp, Some(1677974400));
        assert!(parse_catbox_album(b"<h1>Empty</h1>", "alb1").is_err());
    }

    const IMGBOX_GALLERY: &str = r#"<h1>Holiday &quot;2024&quot;</h1>
        <div id="gallery-view-content">
        <a href="/AbCd1234"><img alt="AbCd1234" src="https://thumbs2.imgbox.com/ab/cd/AbCd1234_t.JPG" title="one.jpg" /></a>
        <a href="/EfGh5678"><img alt="EfGh5678" src="https://thumbs2.imgbox.com/ef/gh/EfGh5678_t.png" /></a>
        <a href="/Bad"><img src="https://thumbs2.imgbox.com.evil.example/ab/cd/AbCd9999_t.jpg" /></a>
        <a href="/Bad2"><img src="http://thumbs2.imgbox.com/ab/cd/AbCd9998_t.jpg" /></a>
        <img src="https://imgbox.com/logo.png"></div>"#;

    #[test]
    fn parses_imgbox_gallery_and_image() {
        let media = parse_imgbox_gallery(IMGBOX_GALLERY.as_bytes(), "AbCdEfGhIj").unwrap();
        assert_eq!(media.len(), 2, "{media:?}");
        assert_eq!(
            media[0].url.as_str(),
            "https://images2.imgbox.com/ab/cd/AbCd1234_o.jpg"
        );
        assert_eq!(media[0].filename, "AbCdEfGhIj_01.jpg");
        let m = meta(&media[0]);
        assert_eq!(m.caption.as_deref(), Some("Holiday \"2024\""));
        assert_eq!(m.media_id, "AbCd1234");
        assert_eq!(
            m.file.as_ref().unwrap().description.as_deref(),
            Some("one.jpg")
        );
        assert_eq!(meta(&media[1]).media_id, "EfGh5678");

        let page = r#"<meta property="og:image" content="https://thumbs2.imgbox.com/ab/cd/AbCd1234_t.jpg">
            <img alt="pic.png" class="image-content" id="img" onclick="x" src="https://images2.imgbox.com/ab/cd/AbCd1234_o.png" title="pic.png" />"#;
        let media = parse_imgbox_image(page.as_bytes(), "AbCd1234").unwrap();
        assert_eq!(
            media[0].url.as_str(),
            "https://images2.imgbox.com/ab/cd/AbCd1234_o.png"
        );
        assert_eq!(media[0].filename, "AbCd1234.png");
        assert_eq!(meta(&media[0]).caption.as_deref(), Some("pic.png"));
        let evil = r#"<img id="img" src="https://evil.example/AbCd1234_o.png">"#;
        assert!(parse_imgbox_image(evil.as_bytes(), "AbCd1234").is_err());
    }

    const POSTIMG_IMAGE: &str = r#"<meta property="og:image" content="https://i.postimg.cc/Ab12/photo.jpg">
        <span class="imagename">photo &amp; co.jpg</span>
        <a class="btn" href="https://i.postimg.cc/Ab12/photo.jpg?dl=1" id="download">Download</a>"#;

    #[test]
    fn parses_postimg_image_and_gallery() {
        let media = parse_postimg_image(POSTIMG_IMAGE.as_bytes(), "Xy12").unwrap();
        assert_eq!(
            media[0].url.as_str(),
            "https://i.postimg.cc/Ab12/photo.jpg?dl=1"
        );
        assert_eq!(media[0].filename, "Xy12.jpg");
        assert_eq!(meta(&media[0]).caption.as_deref(), Some("photo & co.jpg"));
        let fallback = r#"<meta property="og:image" content="https://i.postimg.cc/Ab12/p.png">
            <a href="https://evil.example/p.png" id="download">"#;
        let media = parse_postimg_image(fallback.as_bytes(), "Xy12").unwrap();
        assert_eq!(media[0].url.as_str(), "https://i.postimg.cc/Ab12/p.png");
        assert!(parse_postimg_image(b"<html></html>", "Xy12").is_err());

        let gallery = r#"<meta property="og:title" content="Trip">
            <div class="thumb"><a href="https://postimg.cc/Img1"><img></a></div>
            <div class="thumb"><a href="https://postimg.cc/Img2"><img></a></div>
            <div class="thumb"><a href="https://postimg.cc/Img1"><img></a></div>
            <div class="thumb"><a href="https://evil.example/Img3"><img></a></div>"#;
        let (title, ids) = parse_postimg_gallery(gallery.as_bytes()).unwrap();
        assert_eq!(title.as_deref(), Some("Trip"));
        assert_eq!(ids, ["Img1", "Img2"]);
        assert!(parse_postimg_gallery(b"<html></html>").is_err());
    }

    #[test]
    fn unescapes_entities() {
        assert_eq!(
            unescape("a &amp; b &#39;c&#x27; &bogus; &"),
            "a & b 'c' &bogus; &"
        );
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

    fn hosts(root: Url) -> ImageHosts {
        ImageHosts::new(Duration::from_secs(5))
            .unwrap()
            .with_network(&NetworkOptions::new(Some(String::new()), None).unwrap())
            .unwrap()
            .with_api_root(root)
    }

    #[tokio::test]
    async fn fetches_pixeldrain_file_with_retry() {
        let body = json!({"id": "abc", "name": "x.png"}).to_string();
        let (root, server) = serve(vec![
            response("503 Service Unavailable", b""),
            response("200 OK", body.as_bytes()),
        ]);
        let events: Vec<_> = hosts(root)
            .with_retries(1)
            .events(&Url::parse("https://pixeldrain.com/u/abc").unwrap())
            .collect()
            .await;
        assert_eq!(events.len(), 3, "{events:?}");
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /api/file/abc/info "));
        assert!(
            requests[1]
                .to_ascii_lowercase()
                .contains("user-agent: gallery-dl-rs/")
        );
    }

    #[tokio::test]
    async fn fetches_postimg_gallery_image_pages() {
        let gallery = r#"<div class="thumb"><a href="https://postimg.cc/Img1"></a></div>
            <div class="thumb"><a href="https://postimg.cc/Img2"></a></div>"#;
        let (root, server) = serve(vec![
            response("200 OK", gallery.as_bytes()),
            response("200 OK", POSTIMG_IMAGE.as_bytes()),
            response(
                "200 OK",
                br#"<a href="https://i.postimg.cc/Cd34/two.png" id="download">"#,
            ),
        ]);
        let media: Vec<_> = hosts(root)
            .stream(&Url::parse("https://postimg.cc/gallery/G1").unwrap())
            .collect()
            .await;
        let media: Vec<_> = media.into_iter().map(|m| m.unwrap()).collect();
        assert_eq!(media.len(), 2);
        assert_eq!(media[1].filename, "G1_02.png");
        assert_eq!(meta(&media[1]).media_id, "Img2");
        assert_eq!(meta(&media[1]).post_id, "G1");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /gallery/G1 "));
        assert!(requests[2].starts_with("GET /Img2 "));
    }

    #[tokio::test]
    async fn terminal_errors_are_not_retried() {
        let (root, server) = serve(vec![response("404 Not Found", b"")]);
        let error = hosts(root)
            .with_retries(3)
            .stream(&Url::parse("https://catbox.moe/c/abc").unwrap())
            .collect::<Vec<_>>()
            .await
            .remove(0)
            .unwrap_err();
        assert!(error.to_string().contains("not found"));
        assert!(!error.to_string().contains("127.0.0.1"));
        assert_eq!(server.join().unwrap().len(), 1);
    }
}
