//! Experimental native Pinterest extraction mirroring
//! gallery_dl/extractor/pinterest.py.
//!
//! Every upstream class is supported: `pin`, `board` (queuing its sections),
//! `section`, `user` (queuing its boards), `allpins`, `created`, `search`,
//! `related-pin`, `related-board` and `pinit` short links. Requests use the
//! web "resource" API (`resource/NAMEResource/get/?data={"options":..}&
//! source_url=`) with upstream's header set, the session cookies (`--cookies`,
//! for secret boards) and a `csrftoken` cookie: the session's own, else a
//! random one like upstream, echoed in `X-CSRFToken`.
//! Feeds follow `resource.options.bookmarks` until it is missing, empty,
//! `-end-` or starts with `Y2JOb25lO`, capped at 200 pages.
//!
//! Files follow upstream `_extract_files`: story pins (image, video, audio
//! and paragraph blocks; stickers skipped), carousels, videos (HLS preferred
//! and emitted as an HLS manifest with an MP4 `_fallback`), else
//! `images.orig`. Each file carries upstream's keyword dictionary and the
//! default formats of the class that produced it. Only HTTPS media on
//! `i.pinimg.com` or `v*.pinimg.com` are emitted.
use crate::{
    Result, error,
    event::{
        ChildMetadata, ExtractionEvent, KeywordPolicy, child_job, group_with_keywords, media_only,
    },
    extract::{Media, MediaJob, PostMetadata},
    file_metadata::FileMetadata,
    network::NetworkOptions,
    sites::{Route, SiteKeywords},
    transport::{RateLimits, Redirects, retry_after},
};
use futures_util::{
    StreamExt, TryStreamExt,
    stream::{self, BoxStream},
};
use reqwest::{Client, StatusCode};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::Arc,
    time::{Duration, SystemTime},
};
use url::Url;

type Events<'a> = BoxStream<'a, Result<ExtractionEvent>>;

const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:128.0) Gecko/20100101 Firefox/128.0";
/// Upstream's resource API headers. `X-APP-VERSION` is a pinned web build
/// hash copied from upstream and may need updating if Pinterest enforces it.
const API_HEADERS: [(&str, &str); 9] = [
    ("Accept", "application/json, text/javascript, */*, q=0.01"),
    ("X-Requested-With", "XMLHttpRequest"),
    ("X-APP-VERSION", "a89153f"),
    ("X-Pinterest-AppState", "active"),
    ("X-Pinterest-PWS-Handler", "www/[username].js"),
    ("Alt-Used", "www.pinterest.com"),
    ("Sec-Fetch-Dest", "empty"),
    ("Sec-Fetch-Mode", "cors"),
    ("Sec-Fetch-Site", "same-origin"),
];
const MAX_RESPONSE: usize = 16 * 1024 * 1024;
const MAX_PAGES: u64 = 200;
/// Registrable Pinterest domains (with optional `www.` or a two-letter
/// country subdomain such as `br.pinterest.com`).
const DOMAINS: [&str; 30] = [
    "pinterest.com",
    "pinterest.co.uk",
    "pinterest.de",
    "pinterest.fr",
    "pinterest.ca",
    "pinterest.com.au",
    "pinterest.jp",
    "pinterest.es",
    "pinterest.it",
    "pinterest.at",
    "pinterest.ch",
    "pinterest.nz",
    "pinterest.ie",
    "pinterest.dk",
    "pinterest.se",
    "pinterest.pt",
    "pinterest.cl",
    "pinterest.com.mx",
    "pinterest.ph",
    "pinterest.co.kr",
    "pinterest.ru",
    "pinterest.nl",
    "pinterest.be",
    "pinterest.no",
    "pinterest.fi",
    "pinterest.pl",
    "pinterest.co.in",
    "pinterest.com.br",
    "pinterest.cz",
    "pinterest.ro",
];
/// First path segments that are site pages rather than users.
const RESERVED: [&str; 20] = [
    "pin",
    "search",
    "ideas",
    "today",
    "explore",
    "settings",
    "business",
    "categories",
    "topics",
    "resource",
    "url_shortener",
    "login",
    "signup",
    "about",
    "_",
    "news_hub",
    "videos",
    "shopping",
    "notifications",
    "password",
];

const FILENAME: &str = "{category}_{id}{media_id|page_id:?_//}.{extension}";
const ARCHIVE: &str = "{id}{media_id|page_id}";
const BOARD_ARCHIVE: &str = "{board[id]}_{id}";
static PIN: Route = Route::new("pin", FILENAME, &["{category}"], ARCHIVE);
static BOARD: Route = Route::new(
    "board",
    FILENAME,
    &["{category}", "{board[owner][username]}", "{board[name]}"],
    BOARD_ARCHIVE,
);
static SECTION: Route = Route::new(
    "section",
    FILENAME,
    &[
        "{category}",
        "{board[owner][username]}",
        "{board[name]}",
        "{section[title]}",
    ],
    BOARD_ARCHIVE,
);
static ALLPINS: Route = Route::new("allpins", FILENAME, &["{category}", "{user}"], ARCHIVE);
static CREATED: Route = Route::new("created", FILENAME, &["{category}", "{user}"], ARCHIVE);
static SEARCH: Route = Route::new(
    "search",
    FILENAME,
    &["{category}", "Search", "{search}"],
    ARCHIVE,
);
static RELATED_PIN: Route = Route::new(
    "related-pin",
    FILENAME,
    &["{category}", "related {original_pin[id]}"],
    ARCHIVE,
);
static RELATED_BOARD: Route = Route::new(
    "related-board",
    FILENAME,
    &[
        "{category}",
        "{board[owner][username]}",
        "{board[name]}",
        "related",
    ],
    BOARD_ARCHIVE,
);

/// Upstream extractor classes supported natively, for `--list-extractors`.
pub const SUBCATEGORIES: &[&str] = &[
    "allpins",
    "board",
    "created",
    "pin",
    "pinit",
    "related-board",
    "related-pin",
    "search",
    "section",
    "user",
];

/// Pin IDs (numeric, or upstream's alphanumeric form).
pub fn valid_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

fn valid_name(name: &str) -> bool {
    (1..=100).contains(&name.len())
        && !name.starts_with('_')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'%'))
}

fn valid_user(name: &str) -> bool {
    valid_name(name) && !RESERVED.contains(&name)
}

/// One upstream extractor class and its URL arguments (percent-decoded).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Pin {
        id: String,
    },
    RelatedPin {
        id: String,
    },
    Board {
        user: String,
        board: String,
    },
    RelatedBoard {
        user: String,
        board: String,
    },
    /// `section` is a slug or `id:SECTION_ID`.
    Section {
        user: String,
        board: String,
        section: String,
    },
    /// A user's boards (`/USER/` or `/USER/_saved/`).
    User {
        user: String,
    },
    AllPins {
        user: String,
    },
    Created {
        user: String,
    },
    Search {
        query: String,
    },
    Short {
        code: String,
    },
}

impl Target {
    /// Upstream subcategory of the extractor class.
    pub fn subcategory(&self) -> &'static str {
        match self {
            Self::Pin { .. } => "pin",
            Self::RelatedPin { .. } => "related-pin",
            Self::Board { .. } => "board",
            Self::RelatedBoard { .. } => "related-board",
            Self::Section { .. } => "section",
            Self::User { .. } => "user",
            Self::AllPins { .. } => "allpins",
            Self::Created { .. } => "created",
            Self::Search { .. } => "search",
            Self::Short { .. } => "pinit",
        }
    }

    /// Upstream `archive_fmt` of the extractor class.
    pub fn archive_format(&self) -> &'static str {
        match self {
            Self::Board { .. } | Self::RelatedBoard { .. } | Self::Section { .. } => BOARD_ARCHIVE,
            _ => ARCHIVE,
        }
    }

    /// Upstream `NotFoundError` resource name (`subcategory` after `-`).
    fn resource(&self) -> &'static str {
        let sub = self.subcategory();
        sub.rsplit('-').next().unwrap_or(sub)
    }
}

fn plain(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https")
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
}

fn pinterest_host(host: &str) -> bool {
    DOMAINS.iter().any(|domain| {
        host == *domain
            || host.strip_suffix(domain).is_some_and(|prefix| {
                prefix
                    .strip_suffix('.')
                    .is_some_and(|label| label == "www" || is_country(label))
            })
    })
}

fn is_country(label: &str) -> bool {
    label.len() == 2 && label.bytes().all(|b| b.is_ascii_lowercase())
}

fn decode(segment: &str) -> String {
    crate::sites::percent_decode(segment)
}

/// The raw `q` search parameter, percent-decoded like Python `unquote`
/// (a `+` stays a plus sign).
fn search_query(url: &Url) -> Option<String> {
    let query = url
        .query()?
        .split('&')
        .find_map(|pair| pair.strip_prefix("q="))?;
    let query = decode(query);
    (!query.is_empty() && query.len() <= 1024).then_some(query)
}

/// Classify a URL by the first matching upstream extractor pattern.
pub fn target(url: &Url) -> Result<Target> {
    let unsupported = || {
        error(
            "unsupported Pinterest URL; expected a pin, board, section, user, search or https://pin.it/CODE URL",
        )
    };
    if !plain(url) {
        return Err(unsupported());
    }
    let host = url.host_str().ok_or_else(unsupported)?;
    let parts: Vec<&str> = url
        .path_segments()
        .map(|s| s.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    if host == "pin.it" {
        return match parts[..] {
            [code, ..] if valid_id(code) => Ok(Target::Short { code: code.into() }),
            _ => Err(unsupported()),
        };
    }
    if !pinterest_host(host) {
        return Err(unsupported());
    }
    // Upstream `.*#related$` on the whole URL.
    let related = url
        .fragment()
        .is_some_and(|f| f == "related" || f.ends_with("#related"));
    let user = |user: &str| decode(user);
    match parts[..] {
        ["pin", id, ..] if valid_id(id) => Ok(if related {
            Target::RelatedPin { id: id.into() }
        } else {
            Target::Pin { id: id.into() }
        }),
        ["search", "pins"] => search_query(url)
            .map(|query| Target::Search { query })
            .ok_or_else(unsupported),
        [name] | [name, "_saved"] if valid_user(name) && !related => {
            Ok(Target::User { user: user(name) })
        }
        [name, "pins"] if valid_user(name) && !related => Ok(Target::AllPins { user: user(name) }),
        [name, "_created"] if valid_user(name) && !related => {
            Ok(Target::Created { user: user(name) })
        }
        [name, board, section, ..]
            if valid_user(name)
                && valid_name(board)
                && (valid_name(section) || section.strip_prefix("id:").is_some_and(valid_id)) =>
        {
            Ok(Target::Section {
                user: user(name),
                board: decode(board),
                section: decode(section),
            })
        }
        [name, board] if valid_user(name) && valid_name(board) => {
            let (user, board) = (user(name), decode(board));
            Ok(if related {
                Target::RelatedBoard { user, board }
            } else {
                Target::Board { user, board }
            })
        }
        _ => Err(unsupported()),
    }
}

/// Whether a URL is a Pinterest page handled by this extractor.
pub fn is_pinterest_url(url: &Url) -> bool {
    target(url).is_ok()
}

/// HTTPS media on `i.pinimg.com` or `v*.pinimg.com`.
pub fn media_url(raw: &str) -> Option<Url> {
    let url = Url::parse(raw.trim()).ok()?;
    let label = url.host_str()?.strip_suffix(".pinimg.com")?;
    let ok = label == "i"
        || label
            .strip_prefix('v')
            .is_some_and(|rest| rest.bytes().all(|b| b.is_ascii_digit()));
    (url.scheme() == "https" && plain(&url) && ok).then_some(url)
}

fn text(value: &Value, key: &str) -> Option<String> {
    scalar_text(value.get(key)?)
}

fn scalar_text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.trim().to_owned()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
    .filter(|s| !s.is_empty())
}

/// Python truthiness of an optional JSON value.
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|n| n != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// `value or ""`.
fn or_empty(value: Option<&Value>) -> Value {
    if truthy(value) {
        value.cloned().unwrap_or_default()
    } else {
        Value::from("")
    }
}

/// gallery-dl `text.nameext_from_url` on the raw URL string.
fn nameext(url: &str) -> (String, String) {
    let path = url.split('?').next().unwrap_or_default();
    let filename = decode(path.rsplit('/').next().unwrap_or_default());
    match filename.rsplit_once('.') {
        Some((name, ext)) if !name.is_empty() && ext.chars().count() <= 16 => {
            (name.to_owned(), ext.to_lowercase())
        }
        _ => (filename, String::new()),
    }
}

/// Parse RFC 2822 dates such as `Mon, 01 Jan 2024 00:00:00 +0000`.
pub fn parse_rfc2822(input: &str) -> Option<i64> {
    let mut parts: Vec<&str> = input.split_whitespace().collect();
    if parts.first().is_some_and(|p| p.ends_with(',')) {
        parts.remove(0);
    }
    let [day, month, year, clock, offset] = parts[..] else {
        return None;
    };
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let month = MONTHS.iter().position(|m| *m == month)? + 1;
    let day: u8 = day.parse().ok()?;
    let offset = match offset {
        "GMT" | "UT" | "UTC" | "Z" => "+0000",
        other => other,
    };
    if year.len() != 4 || clock.len() != 8 {
        return None;
    }
    crate::date::parse_date(&format!("{year}-{month:02}-{day:02}T{clock}{offset}")).ok()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Photo,
    Video,
    Audio,
    Text,
}

/// One upstream file dict and how it is fetched.
struct File {
    fields: Map<String, Value>,
    kind: Kind,
}

/// A JSON object or an extraction failure (upstream's caught exceptions).
fn object(value: Option<&Value>) -> Option<Map<String, Value>> {
    value?.as_object().cloned()
}

/// Upstream `_extract_files`; `None` when upstream would raise.
fn extract_files(pin: &Map<String, Value>, stories: bool, videos: bool) -> Option<Vec<File>> {
    if stories && truthy(pin.get("story_pin_data")) {
        return extract_story(pin, &pin["story_pin_data"]);
    }
    if truthy(pin.get("carousel_data")) {
        return extract_carousel(&pin["carousel_data"]);
    }
    if videos && truthy(pin.get("videos")) {
        return Some(vec![extract_video(&pin["videos"])?]);
    }
    Some(
        object(pin.get("images").and_then(|i| i.get("orig")))
            .map(|fields| File {
                fields,
                kind: Kind::Photo,
            })
            .into_iter()
            .collect(),
    )
}

fn extract_story(pin: &Map<String, Value>, story: &Value) -> Option<Vec<File>> {
    let mut files = Vec::new();
    let story_id = story.get("id").cloned().unwrap_or_default();
    let pages = story.get("pages")?.as_array()?;
    for page in pages {
        let page_id = page.get("id").cloned().unwrap_or_default();
        let blocks = page.get("blocks")?.as_array()?;
        for block in blocks {
            let kind = block
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let mut file = match kind {
                "story_pin_image_block" => {
                    let orig = (blocks.len() == 1 && pages.len() == 1)
                        .then(|| object(pin.get("images").and_then(|i| i.get("orig"))))
                        .flatten();
                    match orig {
                        Some(fields) => File {
                            fields,
                            kind: Kind::Photo,
                        },
                        None => extract_image(page, block)?,
                    }
                }
                _ if kind == "story_pin_video_block" || block.get("video").is_some() => {
                    let video = block.get("video")?;
                    let mut file = extract_video(video)?;
                    file.fields
                        .insert("media_id".into(), or_empty(video.get("id")));
                    file
                }
                _ if kind == "story_pin_music_block" || block.get("audio").is_some() => {
                    let mut fields = object(block.get("audio"))?;
                    let url = fields.get("audio_url")?.clone();
                    fields.insert("url".into(), url);
                    let id = or_empty(fields.get("id"));
                    fields.insert("media_id".into(), id);
                    File {
                        fields,
                        kind: Kind::Audio,
                    }
                }
                "story_pin_paragraph_block" => {
                    let text = block.get("text")?.as_str()?;
                    let mut fields = Map::new();
                    fields.insert("url".into(), format!("text:{text}").into());
                    fields.insert("extension".into(), "txt".into());
                    fields.insert(
                        "media_id".into(),
                        block.get("id").cloned().unwrap_or_default(),
                    );
                    File {
                        fields,
                        kind: Kind::Text,
                    }
                }
                "story_pin_product_sticker_block" | "story_pin_static_sticker_block" => continue,
                // Upstream warns about the unsupported block type.
                _ => match extract_image(page, block) {
                    Some(file) => file,
                    None => continue,
                },
            };
            file.fields.insert("story_id".into(), story_id.clone());
            file.fields.insert("page_id".into(), page_id.clone());
            files.push(file);
        }
    }
    Some(files)
}

fn extract_carousel(carousel: &Value) -> Option<Vec<File>> {
    let mut files = Vec::new();
    for slot in carousel.get("carousel_slots")?.as_array()? {
        let mut fields = object(Some(slot))?;
        let (size, image) = fields.get("images")?.as_object()?.iter().next()?;
        let url = image
            .get("url")?
            .as_str()?
            .replacen(&format!("/{size}/"), "/originals/", 1);
        let id = fields.remove("id")?;
        fields.insert("media_id".into(), id);
        fields.insert("url".into(), url.into());
        files.push(File {
            fields,
            kind: Kind::Photo,
        });
    }
    Some(files)
}

fn extract_image(page: &Value, block: &Value) -> Option<File> {
    let signature = if truthy(block.get("image_signature")) {
        block.get("image_signature")
    } else {
        page.get("image_signature")
    };
    let sig = signature?.as_str()?;
    if sig.len() < 6 || sig.len() > 128 || !sig.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return None;
    }
    let base = format!(
        "https://i.pinimg.com/originals/{}/{}/{}/{sig}.",
        &sig[0..2],
        &sig[2..4],
        &sig[4..6]
    );
    let (jpg, png, webp) = (
        format!("{base}jpg"),
        format!("{base}png"),
        format!("{base}webp"),
    );
    let mut fields = match object(
        block
            .get("image")
            .and_then(|i| i.get("images"))
            .and_then(|i| i.get("originals")),
    ) {
        Some(fields) => fields,
        None => Map::from_iter([("url".to_owned(), Value::from(jpg.clone()))]),
    };
    let fallback = if fields.get("url")?.as_str()? == jpg {
        json!([png, webp])
    } else {
        json!([jpg, png, webp])
    };
    fields.insert("_fallback".into(), fallback);
    fields.insert("media_id".into(), sig.into());
    Some(File {
        fields,
        kind: Kind::Photo,
    })
}

/// Upstream `_extract_video`: HLS first, else the widest format, with the
/// `V_720P` MP4 as fallback. An HLS file without `V_720P` falls back to the
/// widest MP4 so that downloads without HLS support still succeed.
fn extract_video(video: &Value) -> Option<File> {
    let formats = video.get("video_list")?.as_object()?;
    fn width(entry: &Value) -> f64 {
        entry.get("width").and_then(Value::as_f64).unwrap_or(0.0)
    }
    // Python `max()` keeps the first of equally wide entries.
    fn widest<'v>(entries: impl Iterator<Item = &'v Value>) -> Option<&'v Value> {
        entries.fold(None, |best, entry| match best {
            Some(best) if width(best) >= width(entry) => Some(best),
            _ => Some(entry),
        })
    }
    let chosen = ["V_HLSV4", "V_HLSV3_WEB", "V_HLSV3_MOBILE"]
        .iter()
        .find_map(|key| formats.get(*key))
        .or_else(|| widest(formats.values()))?;
    let mut fields = chosen.as_object()?.clone();
    let is_mp4 = |entry: &Value| {
        entry
            .get("url")
            .and_then(Value::as_str)
            .is_some_and(|url| url.split('?').next().unwrap_or_default().ends_with(".mp4"))
    };
    let hls = fields
        .get("url")
        .and_then(Value::as_str)
        .is_some_and(|url| nameext(url).1 == "m3u8");
    let fallback = formats
        .get("V_720P")
        .or_else(|| {
            hls.then(|| widest(formats.values().filter(|e| is_mp4(e))))
                .flatten()
        })
        .and_then(|entry| entry.get("url"))
        .cloned();
    if let Some(url) = fallback {
        fields.insert("_fallback".into(), json!([url]));
    }
    Some(File {
        fields,
        kind: Kind::Video,
    })
}

/// Class metadata merged into every pin (`metadata()` upstream).
struct Context {
    route: &'static Route,
    data: Map<String, Value>,
}

impl Context {
    fn new(route: &'static Route, data: Map<String, Value>) -> Arc<Self> {
        Arc::new(Self { route, data })
    }
}

/// Convert one pin into media with upstream's keyword dictionary.
fn build_media(pin: Value, context: &Context, stories: bool, videos: bool) -> Vec<Media> {
    let Value::Object(pin) = pin else {
        return Vec::new();
    };
    let Some(id) = pin
        .get("id")
        .and_then(scalar_text)
        .filter(|id| valid_id(id))
    else {
        return Vec::new();
    };
    let Some(files) = extract_files(&pin, stories, videos) else {
        return Vec::new();
    };
    let files: Vec<(File, Url, MediaJob)> = files
        .into_iter()
        .filter_map(|mut file| {
            let (url, job) = if file.kind == Kind::Text {
                let raw = file.fields.get("url")?.as_str()?;
                crate::extract::inline_text(raw.strip_prefix("text:")?.to_owned()).ok()?
            } else {
                let url = media_url(file.fields.get("url")?.as_str()?)?;
                (url, MediaJob::DirectHttp)
            };
            if let Some(list) = file.fields.get("_fallback").and_then(Value::as_array) {
                let safe: Vec<Value> = list
                    .iter()
                    .filter(|u| u.as_str().and_then(media_url).is_some())
                    .cloned()
                    .collect();
                file.fields.insert("_fallback".into(), safe.into());
            }
            Some((file, url, job))
        })
        .collect();
    let pin_value = Value::Object(pin);
    let username = pin_value
        .get("pinner")
        .and_then(|p| text(p, "username"))
        .map(Arc::<str>::from);
    let caption = ["title", "grid_title", "description"]
        .iter()
        .find_map(|key| text(&pin_value, key))
        .map(Arc::<str>::from);
    let timestamp = text(&pin_value, "created_at").and_then(|d| parse_rfc2822(&d));
    let Value::Object(mut keywords) = pin_value else {
        unreachable!()
    };
    for (key, value) in &context.data {
        keywords.insert(key.clone(), value.clone());
    }
    keywords.insert("count".into(), files.len().into());
    for key in [
        "description",
        "closeup_description",
        "closeup_unified_description",
    ] {
        if let Some(Value::String(value)) = keywords.get_mut(key) {
            *value = value.trim().to_owned();
        }
    }
    files
        .into_iter()
        .enumerate()
        .map(|(i, (file, url, mut job))| {
            let index = i + 1;
            let raw = file.fields.get("url").and_then(Value::as_str).unwrap_or("");
            let (name, extension) = nameext(raw);
            keywords.insert("num".into(), index.into());
            keywords.insert("filename".into(), name.into());
            keywords.insert("extension".into(), extension.into());
            // Upstream reuses one dict; drop download hints of earlier files.
            keywords.remove("_fallback");
            keywords.remove("_ytdl_manifest");
            for key in ["media_id", "page_id"] {
                if !file.fields.contains_key(key) {
                    keywords.insert(key.into(), "".into());
                }
            }
            for (key, value) in &file.fields {
                keywords.insert(key.clone(), value.clone());
            }
            if keywords.get("extension").and_then(Value::as_str) == Some("m3u8") {
                keywords.insert("_ytdl_manifest".into(), "hls".into());
                keywords.insert("extension".into(), "mp4".into());
                job = MediaJob::Manifest {
                    protocol: crate::extract::ManifestProtocol::Hls,
                    base_url: url.clone(),
                    inline_data: None,
                };
            }
            keywords.insert("category".into(), "pinterest".into());
            keywords.insert("subcategory".into(), context.route.subcategory.into());
            let stem = keywords
                .get("filename")
                .map(crate::sites::scalar)
                .unwrap_or_default();
            let extension = keywords
                .get("extension")
                .map(crate::sites::scalar)
                .unwrap_or_default();
            let filename = if extension.is_empty() {
                stem
            } else {
                format!("{stem}.{extension}")
            };
            let media_id = ["media_id", "page_id"]
                .iter()
                .find_map(|key| keywords.get(*key).and_then(scalar_text))
                .unwrap_or_else(|| id.clone());
            let mut handoff = BTreeMap::new();
            if let Some(list) = file
                .fields
                .get("_fallback")
                .filter(|list| truthy(Some(list)))
            {
                handoff.insert("_fallback".into(), list.clone());
            }
            Media {
                filename,
                job,
                category: "pinterest".into(),
                metadata: Some(PostMetadata {
                    file: Some(FileMetadata {
                        width: file
                            .fields
                            .get("width")
                            .and_then(Value::as_u64)
                            .filter(|v| *v > 0),
                        height: file
                            .fields
                            .get("height")
                            .and_then(Value::as_u64)
                            .filter(|v| *v > 0),
                        media_type: match file.kind {
                            Kind::Photo => Some("photo"),
                            Kind::Video => Some("video"),
                            Kind::Audio => Some("audio"),
                            Kind::Text => None,
                        }
                        .map(Arc::from),
                        ..FileMetadata::default()
                    }),
                    post_id: id.clone(),
                    media_id,
                    username: username.clone(),
                    caption: caption.clone(),
                    timestamp,
                    index,
                    site_keywords: Some(Arc::new(SiteKeywords {
                        route: context.route,
                        fields: keywords.clone(),
                    })),
                    ..PostMetadata::default()
                }),
                url,
                headers: BTreeMap::new(),
                handoff,
            }
        })
        .collect()
}

/// Convert one pin object into its media with the `pin` class keywords and
/// default options (`stories` and `videos` enabled).
pub fn pin_media(pin: &Value) -> Vec<Media> {
    build_media(
        pin.clone(),
        &Context {
            route: &PIN,
            data: Map::new(),
        },
        true,
        true,
    )
}

/// `resource_response.data`, unwrapping `{"results": [...]}` pages.
fn page_results(body: &Value) -> Result<Vec<Value>> {
    let data = body.pointer("/resource_response/data");
    let data = match data {
        Some(Value::Object(map)) => map.get("results"),
        other => other,
    };
    data.and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| error("Pinterest response contains no result list"))
}

/// The next `bookmarks` option, or `None` at the end of a feed.
fn next_bookmarks(body: &Value) -> Option<Value> {
    let bookmarks = body.pointer("/resource/options/bookmarks")?.as_array()?;
    let first = bookmarks.first()?.as_str()?;
    if first == "-end-" || first.starts_with("Y2JOb25lO") {
        return None;
    }
    (bookmarks.len() <= 16
        && bookmarks
            .iter()
            .all(|b| b.as_str().is_some_and(|b| b.len() <= 4096)))
    .then(|| Value::Array(bookmarks.clone()))
}

fn data_object(body: Value, what: &str) -> Result<Map<String, Value>> {
    match body.pointer("/resource_response/data") {
        Some(Value::Object(map)) => Ok(map.clone()),
        _ => Err(error(format!("Pinterest {what} not found"))),
    }
}

/// A 32-digit hexadecimal token (upstream `util.generate_token()`).
fn generate_token() -> String {
    use std::hash::BuildHasher;
    let now = SystemTime::now();
    let state = std::collections::hash_map::RandomState::new();
    format!(
        "{:016x}{:016x}",
        state.hash_one((now, 0u8)),
        state.hash_one((now, 1u8))
    )
}

fn events(events: impl IntoIterator<Item = ExtractionEvent>) -> Events<'static> {
    stream::iter(events.into_iter().map(Ok).collect::<Vec<_>>()).boxed()
}

fn fail<'a>(e: crate::Error) -> Events<'a> {
    stream::once(async move { Err(e) }).boxed()
}

fn once<'a, F>(future: F) -> Events<'a>
where
    F: std::future::Future<Output = Result<Events<'a>>> + Send + 'a,
{
    stream::once(future)
        .map(|result| result.unwrap_or_else(fail))
        .flatten()
        .boxed()
}

fn flag(options: &Map<String, Value>, key: &str, default: bool) -> Result<bool> {
    match options.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(error(format!(
            "extractor.pinterest.{key} must be true or false"
        ))),
    }
}

/// Upstream `text.ensure_http_scheme(domain)`, restricted to Pinterest hosts.
fn domain_root(domain: &str) -> Result<Url> {
    let invalid = || error("extractor.pinterest.domain must be \"auto\" or a Pinterest domain");
    let raw = if domain.starts_with("https://") || domain.starts_with("http://") {
        domain.to_owned()
    } else {
        format!("https://{}", domain.trim_start_matches(['/', ':']))
    };
    let mut url = Url::parse(&raw).map_err(|_| invalid())?;
    if !plain(&url)
        || !url.host_str().is_some_and(pinterest_host)
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.path().trim_end_matches('/').is_empty()
    {
        return Err(invalid());
    }
    url.set_path("/");
    Ok(url)
}

pub struct Pinterest {
    client: Client,
    network: NetworkOptions,
    /// Test override for resource requests; the site root still comes from
    /// the input URL or the `domain` option.
    api_root: Option<Url>,
    short_root: Url,
    domain: Option<Url>,
    sections: bool,
    stories: bool,
    videos: bool,
    csrf_token: String,
    timeout: Duration,
    request_timeout: Duration,
    retries: u8,
    rate_limits: RateLimits,
    /// Session cookies (e.g. `_pinterest_sess` for secret boards).
    cookies: std::sync::Mutex<crate::cookies::CookieJar>,
}

enum Mode<'a> {
    Api { root: &'a Url, what: &'a str },
    Capture,
}

impl Pinterest {
    pub fn new(timeout: Duration) -> Result<Self> {
        Ok(Self {
            client: NetworkOptions::default().client(USER_AGENT, timeout)?,
            network: NetworkOptions::default(),
            api_root: None,
            short_root: Url::parse("https://api.pinterest.com/").expect("static URL"),
            domain: None,
            sections: true,
            stories: true,
            videos: true,
            csrf_token: generate_token(),
            timeout,
            request_timeout: timeout,
            retries: 0,
            rate_limits: RateLimits::default(),
            cookies: std::sync::Mutex::new(crate::cookies::CookieJar::default()),
        })
    }

    /// Apply proxy and user-agent settings to API requests and emitted media.
    pub fn with_network(mut self, network: &NetworkOptions) -> Result<Self> {
        self.client = network.client(USER_AGENT, self.timeout)?;
        self.network = network.clone();
        Ok(self)
    }

    /// Apply upstream extractor settings (`extractor.pinterest.*`):
    /// `sections`, `stories`, `videos` (booleans, default true) and `domain`
    /// (`"auto"` or a Pinterest domain).
    pub fn with_options(mut self, options: Option<&Map<String, Value>>) -> Result<Self> {
        let Some(options) = options else {
            return Ok(self);
        };
        self.sections = flag(options, "sections", true)?;
        self.stories = flag(options, "stories", true)?;
        self.videos = flag(options, "videos", true)?;
        self.domain = match options.get("domain") {
            None | Some(Value::Null) => None,
            Some(Value::String(domain)) if domain.is_empty() || domain == "auto" => None,
            Some(Value::String(domain)) => Some(domain_root(domain)?),
            Some(_) => {
                return Err(error(
                    "extractor.pinterest.domain must be \"auto\" or a Pinterest domain",
                ));
            }
        };
        Ok(self)
    }

    /// Session cookies sent with every Pinterest request; a logged-in
    /// session can read secret boards and its own private pins.
    pub fn with_cookies(self, cookies: crate::cookies::CookieJar) -> Self {
        *self.cookies.lock().unwrap() = cookies;
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

    /// Override the resource and short-link origins; intended for local tests.
    pub fn with_api_root(mut self, root: Url) -> Self {
        self.short_root = root.clone();
        self.api_root = Some(root);
        self
    }

    /// Upstream `root`: the `domain` option, else the input URL's origin.
    fn site_root(&self, url: &Url) -> Url {
        self.domain.clone().unwrap_or_else(|| {
            Url::parse(&format!("https://{}/", url.host_str().unwrap_or_default()))
                .expect("validated Pinterest host")
        })
    }

    pub fn stream(&self, url: &Url) -> BoxStream<'_, Result<Media>> {
        media_only(self.events(url))
    }

    /// Emit one post boundary per pin and one child job per queued URL;
    /// feeds paginate lazily and children run in order when reached.
    pub fn events(&self, url: &Url) -> BoxStream<'_, Result<ExtractionEvent>> {
        match target(url) {
            Ok(target) => self.extract(target, self.site_root(url)),
            Err(e) => fail(e),
        }
    }

    fn extract(&self, job: Target, root: Url) -> Events<'_> {
        let what = job.resource();
        let related = matches!(job, Target::RelatedBoard { .. });
        match job {
            Target::Pin { id } => once(async move {
                let pin = self.pin(&root, what, &id).await?;
                Ok(events(
                    self.pin_events(pin, &Context::new(&PIN, Map::new())),
                ))
            }),
            Target::RelatedPin { id } => once(async move {
                let original = self.pin(&root, what, &id).await?;
                let context = Context::new(
                    &RELATED_PIN,
                    Map::from_iter([("original_pin".to_owned(), original)]),
                );
                let options = json!({"pin": id, "add_vase": true, "pins_only": true});
                Ok(self.pins(root, what, "RelatedPinFeedResource", options, context))
            }),
            Target::Board { user, board } | Target::RelatedBoard { user, board } => {
                once(async move {
                    let board = self.board(&root, what, &user, &board).await?;
                    let id = board
                        .get("id")
                        .and_then(scalar_text)
                        .filter(|id| valid_id(id))
                        .ok_or_else(|| error("Pinterest board not found"))?;
                    let sections =
                        (!related && self.sections && truthy(board.get("section_count")))
                            .then(|| board.get("url").and_then(Value::as_str).map(str::to_owned))
                            .flatten();
                    let board = Map::from_iter([("board".to_owned(), Value::Object(board))]);
                    if related {
                        let options = json!({"id": id, "type": "board", "add_vase": true});
                        let context = Context::new(&RELATED_BOARD, board);
                        return Ok(self.pins(
                            root,
                            what,
                            "BoardContentRecommendationResource",
                            options,
                            context,
                        ));
                    }
                    let options = json!({"board_id": id, "field_set_key": "react_grid_pin",
                        "prepend": false, "bookmarks": null});
                    let pins = self.pins(
                        root.clone(),
                        what,
                        "BoardFeedResource",
                        options,
                        Context::new(&BOARD, board),
                    );
                    Ok(match sections {
                        Some(url) => pins.chain(self.sections(root, what, id, url)).boxed(),
                        None => pins,
                    })
                })
            }
            Target::Section {
                user,
                board,
                section,
            } => once(async move {
                let options = match section.strip_prefix("id:") {
                    Some(id) => json!({"section_id": id}),
                    None => json!({"board_slug": board, "section_slug": section,
                        "username": user}),
                };
                let body = self
                    .call(&root, what, "BoardSectionResource", &options)
                    .await?;
                let mut section = data_object(body, what)?;
                let id = section
                    .get("id")
                    .and_then(scalar_text)
                    .filter(|id| valid_id(id))
                    .ok_or_else(|| error("Pinterest section not found"))?;
                section.remove("preview_pins");
                let board = section
                    .remove("board")
                    .filter(Value::is_object)
                    .ok_or_else(|| error("Pinterest section has no board"))?;
                let context = Context::new(
                    &SECTION,
                    Map::from_iter([
                        ("board".to_owned(), board),
                        ("section".to_owned(), Value::Object(section)),
                    ]),
                );
                let options = json!({"section_id": id});
                Ok(self.pins(root, what, "BoardSectionPinsResource", options, context))
            }),
            Target::AllPins { user } => {
                let options = json!({"is_own_profile_pins": false, "username": user,
                    "field_set_key": "grid_item", "pin_filter": null});
                let context =
                    Context::new(&ALLPINS, Map::from_iter([("user".into(), user.into())]));
                self.pins(root, what, "UserPinsResource", options, context)
            }
            Target::Created { user } => {
                let options = json!({"exclude_add_pin_rep": true, "field_set_key": "grid_item",
                    "is_own_profile_pins": false, "username": user});
                let context =
                    Context::new(&CREATED, Map::from_iter([("user".into(), user.into())]));
                self.pins(root, what, "UserActivityPinsResource", options, context)
            }
            Target::Search { query } => {
                let options = json!({"query": query, "scope": "pins", "rs": "typed"});
                let context =
                    Context::new(&SEARCH, Map::from_iter([("search".into(), query.into())]));
                self.pins(root, what, "BaseSearchResource", options, context)
            }
            Target::User { user } => {
                let options = json!({"sort": "last_pinned_to",
                    "field_set_key": "profile_grid_item", "filter_stories": false,
                    "username": user, "page_size": 25, "include_archived": true});
                let base = root.as_str().trim_end_matches('/').to_owned();
                self.feed(root, what, "BoardsResource", options)
                    .map_ok(move |boards| {
                        let children: Vec<Events<'_>> = boards
                            .into_iter()
                            .filter_map(|board| {
                                let Value::Object(board) = board else {
                                    return None;
                                };
                                let url = child_url(&base, board.get("url")?.as_str()?)?;
                                let Ok(child @ Target::Board { .. }) = target(&url) else {
                                    return None;
                                };
                                let root = self.site_root(&url);
                                Some(child_job(
                                    ChildMetadata::from_raw("pinterest", "user", &board),
                                    move || self.extract(child, root),
                                ))
                            })
                            .collect();
                        stream::iter(children).flatten()
                    })
                    .try_flatten()
                    .boxed()
            }
            Target::Short { code } => once(async move {
                let location = self.resolve_short(&code).await?;
                let child = match target(&location) {
                    Ok(pin @ Target::Pin { .. }) => pin,
                    Ok(Target::Board { user, board } | Target::Section { user, board, .. }) => {
                        Target::Board { user, board }
                    }
                    _ => return Err(error("Pinterest short link did not resolve to a pin")),
                };
                let root = self.site_root(&location);
                Ok(child_job(
                    ChildMetadata::from_raw("pinterest", "pinit", &Map::new()),
                    move || self.extract(child, root),
                ))
            }),
        }
    }

    /// Queue each board section as a `section` child after the board's pins.
    fn sections(&self, root: Url, what: &'static str, board_id: String, url: String) -> Events<'_> {
        let base = format!("{}{url}id:", root.as_str().trim_end_matches('/'));
        let options = json!({"board_id": board_id});
        self.feed(root, what, "BoardSectionsResource", options)
            .map_ok(move |sections| {
                let children: Vec<Events<'_>> = sections
                    .iter()
                    .filter_map(|section| {
                        let id = text(section, "id").filter(|id| valid_id(id))?;
                        let url = Url::parse(&format!("{base}{id}")).ok()?;
                        let Ok(child @ Target::Section { .. }) = target(&url) else {
                            return None;
                        };
                        let root = self.site_root(&url);
                        Some(child_job(
                            ChildMetadata::from_raw("pinterest", "board", &Map::new()),
                            move || self.extract(child, root),
                        ))
                    })
                    .collect();
                stream::iter(children).flatten()
            })
            .try_flatten()
            .boxed()
    }

    fn pin_events(&self, pin: Value, context: &Context) -> VecDeque<ExtractionEvent> {
        let mut items = build_media(pin, context, self.stories, self.videos);
        if items.is_empty() {
            return VecDeque::new();
        }
        for item in &mut items {
            self.network.apply_media(item);
        }
        group_with_keywords(items, "pinterest", KeywordPolicy::default())
    }

    /// Files of every pin in a paginated feed.
    fn pins(
        &self,
        root: Url,
        what: &'static str,
        resource: &'static str,
        options: Value,
        context: Arc<Context>,
    ) -> Events<'_> {
        self.feed(root, what, resource, options)
            .map_ok(move |pins| {
                events(
                    pins.into_iter()
                        .flat_map(|pin| self.pin_events(pin, &context))
                        .collect::<Vec<_>>(),
                )
            })
            .try_flatten()
            .boxed()
    }

    /// Upstream `_pagination`: lazily yield each page's results.
    fn feed(
        &self,
        root: Url,
        what: &'static str,
        resource: &'static str,
        options: Value,
    ) -> BoxStream<'_, Result<Vec<Value>>> {
        stream::try_unfold(Some((options, 1u64)), move |state| {
            let root = root.clone();
            async move {
                let Some((mut options, page)) = state else {
                    return Ok(None);
                };
                let body = self.call(&root, what, resource, &options).await?;
                let results = page_results(&body)?;
                let next = next_bookmarks(&body)
                    .filter(|_| page < MAX_PAGES)
                    .map(|bookmarks| {
                        options["bookmarks"] = bookmarks;
                        (options, page + 1)
                    });
                Ok(Some((results, next)))
            }
        })
        .boxed()
    }

    async fn pin(&self, root: &Url, what: &str, id: &str) -> Result<Value> {
        let options = json!({"id": id, "field_set_key": "detailed"});
        let body = self.call(root, what, "PinResource", &options).await?;
        let pin = data_object(body, "pin")?;
        if pin.get("id").and_then(scalar_text).as_deref() != Some(id) {
            return Err(error("Pinterest pin not found"));
        }
        Ok(Value::Object(pin))
    }

    async fn board(
        &self,
        root: &Url,
        what: &str,
        user: &str,
        board: &str,
    ) -> Result<Map<String, Value>> {
        let options = json!({"slug": board, "username": user, "field_set_key": "detailed"});
        let body = self.call(root, what, "BoardResource", &options).await?;
        data_object(body, "board")
    }

    /// One resource API call (upstream `PinterestAPI._call`).
    async fn call(&self, root: &Url, what: &str, resource: &str, options: &Value) -> Result<Value> {
        let mut endpoint = self
            .api_root
            .as_ref()
            .unwrap_or(root)
            .join(&format!("resource/{resource}/get/"))
            .expect("static Pinterest path");
        endpoint
            .query_pairs_mut()
            .append_pair("data", &json!({"options": options}).to_string())
            .append_pair("source_url", "");
        let body = tokio::time::timeout(
            self.timeout,
            self.request(&endpoint, Mode::Api { root, what }),
        )
        .await
        .map_err(|_| error("native Pinterest extraction timed out"))??;
        serde_json::from_slice(&body).map_err(|_| error("Pinterest returned invalid JSON"))
    }

    /// Upstream `request_location` on the `url_shortener` redirect.
    async fn resolve_short(&self, code: &str) -> Result<Url> {
        let endpoint = self
            .short_root
            .join(&format!("url_shortener/{code}/redirect/"))
            .expect("validated Pinterest code");
        let location = tokio::time::timeout(self.timeout, self.request(&endpoint, Mode::Capture))
            .await
            .map_err(|_| error("native Pinterest extraction timed out"))??;
        let unresolved = || error("Pinterest short link did not resolve to a pin");
        let location = String::from_utf8(location).map_err(|_| unresolved())?;
        endpoint.join(&location).map_err(|_| unresolved())
    }

    async fn request(&self, endpoint: &Url, mode: Mode<'_>) -> Result<Vec<u8>> {
        for attempt in 0..=self.retries {
            match self.attempt(endpoint, attempt, &mode).await {
                Ok(body) => return Ok(body),
                Err(failure) if failure.retry && attempt < self.retries => {
                    tokio::time::sleep(backoff(attempt)).await;
                }
                Err(failure) => return Err(failure.error),
            }
        }
        unreachable!()
    }

    /// In capture mode a redirect's Location is returned instead of followed.
    async fn attempt(
        &self,
        endpoint: &Url,
        attempt: u8,
        mode: &Mode<'_>,
    ) -> std::result::Result<Vec<u8>, Failure> {
        let capture = matches!(mode, Mode::Capture);
        let mut target = endpoint.clone();
        let mut redirects = Redirects::new(endpoint);
        let mut response = loop {
            self.rate_limits.wait(&target, self.timeout).await?;
            let mut request = self
                .client
                .get(target.clone())
                .timeout(self.request_timeout);
            let jar = self.cookies.lock().unwrap().header(&target);
            if let Mode::Api { root, .. } = mode {
                for (name, value) in API_HEADERS {
                    request = request.header(name, value);
                }
                // Upstream's random `csrftoken`, unless the session brings
                // its own; the header echoes it for authenticated calls.
                let session = self.cookies.lock().unwrap().value(&target, "csrftoken");
                let token = session.clone().unwrap_or_else(|| self.csrf_token.clone());
                let cookie = match (jar, session) {
                    (Some(jar), Some(_)) => jar,
                    (Some(jar), None) => format!("{jar}; csrftoken={token}"),
                    (None, _) => format!("csrftoken={token}"),
                };
                request = request
                    .header("Referer", root.as_str())
                    .header("X-CSRFToken", token)
                    .header("Cookie", cookie);
            } else if let Some(jar) = jar {
                request = request.header("Cookie", jar);
            }
            let response = request.send().await.map_err(|_| {
                Failure::transient("Pinterest request failed; check network access")
            })?;
            self.cookies
                .lock()
                .unwrap()
                .store_response(response.url(), response.headers());
            if capture && response.status().is_redirection() {
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .filter(|v| v.len() <= 4096)
                    .ok_or_else(|| error("Pinterest short link did not resolve to a pin"))?;
                return Ok(location.as_bytes().to_vec());
            }
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
                            "Pinterest rate limit; retry after {} seconds",
                            delay.as_secs()
                        ),
                        None => "Pinterest rate limit; retry later".into(),
                    }
                } else {
                    format!("Pinterest returned HTTP {status}")
                },
            ));
        }
        let what = match mode {
            Mode::Capture => {
                return Err(error("Pinterest short link did not resolve to a pin").into());
            }
            Mode::Api { what, .. } => what,
        };
        if matches!(status.as_u16(), 404 | 410) {
            return Err(error(format!("Pinterest {what} not found")).into());
        }
        if matches!(status.as_u16(), 401 | 403) {
            return Err(error("Pinterest refused access (secret board or login required; pass pinterest.com cookies with --cookies)").into());
        }
        if !status.is_success() {
            return Err(error(format!("Pinterest returned HTTP {status}")).into());
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| Failure::transient("could not read Pinterest response"))?
        {
            if body.len() + chunk.len() > MAX_RESPONSE {
                return Err(error("Pinterest response exceeded 16 MiB").into());
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }
}

/// `root + url` for a site-relative path from the API.
fn child_url(base: &str, path: &str) -> Option<Url> {
    (path.starts_with('/') && !path.starts_with("//") && path.len() <= 2048)
        .then(|| Url::parse(&format!("{base}{path}")).ok())
        .flatten()
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
    use crate::event::ChildScope;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        path::PathBuf,
        thread::{self, JoinHandle},
    };

    fn pin(id: &str) -> Value {
        json!({"id": id, "type": "pin", "title": "", "grid_title": "Cat",
            "description": "  A cat  ",
            "created_at": "Mon, 01 Jan 2024 00:00:00 +0000",
            "pinner": {"username": "alice"},
            "images": {"orig": {"url": format!("https://i.pinimg.com/originals/aa/bb/{id}.jpg"),
                "width": 800, "height": 600}}})
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

    fn context(route: &'static Route, data: Value) -> Context {
        Context {
            route,
            data: data.as_object().cloned().unwrap_or_default(),
        }
    }

    #[test]
    fn classifies_every_upstream_class() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        let pin = Target::Pin { id: "123".into() };
        assert_eq!(t("https://www.pinterest.com/pin/123/").unwrap(), pin);
        assert_eq!(t("https://www.pinterest.co.uk/pin/123").unwrap(), pin);
        assert_eq!(
            t("https://br.pinterest.com/pin/123/sent/?x=1").unwrap(),
            pin
        );
        assert_eq!(t("https://pinterest.com.au/pin/123/").unwrap(), pin);
        assert_eq!(
            t("https://www.pinterest.com/pin/123/#related").unwrap(),
            Target::RelatedPin { id: "123".into() }
        );
        let (alice, cats) = (String::from("alice"), String::from("cats"));
        assert_eq!(
            t("https://www.pinterest.de/alice/cats/").unwrap(),
            Target::Board {
                user: alice.clone(),
                board: cats.clone()
            }
        );
        assert_eq!(
            t("https://www.pinterest.com/alice/cats/#related").unwrap(),
            Target::RelatedBoard {
                user: alice.clone(),
                board: cats.clone()
            }
        );
        assert_eq!(
            t("https://www.pinterest.com/alice/cats/tabby").unwrap(),
            Target::Section {
                user: alice.clone(),
                board: cats.clone(),
                section: "tabby".into()
            }
        );
        assert_eq!(
            t("https://www.pinterest.com/alice/cats/id:4567").unwrap(),
            Target::Section {
                user: alice.clone(),
                board: cats.clone(),
                section: "id:4567".into()
            }
        );
        for raw in [
            "https://www.pinterest.com/alice/",
            "https://www.pinterest.com/alice",
            "https://www.pinterest.com/alice/_saved/",
        ] {
            assert_eq!(
                t(raw).unwrap(),
                Target::User {
                    user: alice.clone()
                },
                "{raw}"
            );
        }
        assert_eq!(
            t("https://www.pinterest.com/alice/pins/").unwrap(),
            Target::AllPins {
                user: alice.clone()
            }
        );
        assert_eq!(
            t("https://www.pinterest.com/alice/_created/").unwrap(),
            Target::Created {
                user: alice.clone()
            }
        );
        assert_eq!(
            t("https://www.pinterest.com/search/pins/?q=red%20cat+x&rs=typed").unwrap(),
            Target::Search {
                query: "red cat+x".into()
            }
        );
        assert_eq!(
            t("https://pin.it/AbC12").unwrap(),
            Target::Short {
                code: "AbC12".into()
            }
        );
        assert_eq!(
            t("https://www.pinterest.com/j%C3%BCrgen/pins/").unwrap(),
            Target::AllPins {
                user: "jürgen".into()
            }
        );
        for raw in [
            "https://www.pinterest.com/",
            "https://www.pinterest.com/search/pins/",
            "https://www.pinterest.com/search/pins/?rs=typed",
            "https://www.pinterest.com/ideas/",
            "https://www.pinterest.com/alice/#related",
            "https://www.pinterest.com/alice/cats/id:bad:id",
            "https://i.pinimg.com/originals/aa/a.jpg",
            "https://www.pinterest.com.evil.example/pin/1/",
            "https://evilpinterest.com/pin/1/",
            "https://www.pinterest.com:444/pin/1/",
            "https://pin.it/",
        ] {
            assert!(t(raw).is_err(), "{raw}");
        }
        let section = t("https://www.pinterest.com/a/b/c").unwrap();
        assert_eq!(
            (section.subcategory(), section.archive_format()),
            ("section", "{board[id]}_{id}")
        );
        assert_eq!(pin.archive_format(), "{id}{media_id|page_id}");
        assert_eq!(
            t("https://www.pinterest.com/pin/1/#related")
                .unwrap()
                .resource(),
            "pin"
        );
        assert!(media_url("https://v.pinimg.com/a.mp4").is_some());
        assert!(media_url("https://v1.pinimg.com/a.mp4").is_some());
        assert!(media_url("https://s.pinimg.com/a.jpg").is_none());
        assert!(media_url("http://i.pinimg.com/a.jpg").is_none());
        assert_eq!(
            parse_rfc2822("Tue, 02 Jan 2024 01:00:00 +0100"),
            Some(1704153600)
        );
        assert_eq!(parse_rfc2822("garbage"), None);
    }

    #[test]
    fn validates_options() {
        let options = |value: Value| {
            Pinterest::new(Duration::from_secs(1))
                .unwrap()
                .with_options(Some(value.as_object().unwrap()))
        };
        let native = options(json!({"sections": false, "stories": false, "videos": false,
            "domain": "auto"}))
        .unwrap();
        assert!(!native.sections && !native.stories && !native.videos);
        assert!(native.domain.is_none());
        let url = Url::parse("https://br.pinterest.com/pin/1/").unwrap();
        assert_eq!(native.site_root(&url).as_str(), "https://br.pinterest.com/");
        let defaults = Pinterest::new(Duration::from_secs(1))
            .unwrap()
            .with_options(None)
            .unwrap();
        assert!(defaults.sections && defaults.stories && defaults.videos);
        assert_eq!(defaults.csrf_token.len(), 32);
        assert!(defaults.csrf_token.bytes().all(|b| b.is_ascii_hexdigit()));
        for domain in [
            "pinterest.de",
            "https://www.pinterest.de/",
            "//www.pinterest.de",
        ] {
            let native = options(json!({ "domain": domain })).unwrap();
            assert_eq!(
                native.site_root(&url).as_str(),
                if domain.contains("www") {
                    "https://www.pinterest.de/"
                } else {
                    "https://pinterest.de/"
                }
            );
        }
        for bad in [
            json!({"sections": "yes"}),
            json!({"stories": 1}),
            json!({"videos": []}),
            json!({"domain": true}),
            json!({"domain": "evil.example"}),
            json!({"domain": "https://www.pinterest.com/path"}),
            json!({"domain": "https://www.pinterest.com:8080"}),
        ] {
            let message = options(bad.clone()).err().unwrap().to_string();
            assert!(message.contains("extractor.pinterest."), "{bad} {message}");
        }
    }

    #[test]
    fn single_image_pin_keywords_and_default_paths() {
        let items = pin_media(&pin("123"));
        assert_eq!(items.len(), 1);
        let media = &items[0];
        assert_eq!(media.filename, "123.jpg");
        assert!(matches!(media.job, MediaJob::DirectHttp));
        let meta = media.metadata.as_ref().unwrap();
        assert_eq!(
            (meta.post_id.as_str(), meta.media_id.as_str()),
            ("123", "123")
        );
        assert_eq!(meta.username.as_deref(), Some("alice"));
        assert_eq!(meta.caption.as_deref(), Some("Cat"));
        assert_eq!((meta.timestamp, meta.index), (Some(1704067200), 1));
        let file = meta.file.as_ref().unwrap();
        assert_eq!((file.width, file.height), (Some(800), Some(600)));
        assert!(media.archive_key().unwrap().starts_with("pinterest:"));
        let k = keywords(media);
        assert_eq!(k["id"], "123");
        assert_eq!(k["description"], "A cat");
        assert_eq!((&k["count"], &k["num"]), (&json!(1), &json!(1)));
        assert_eq!(
            (&k["filename"], &k["extension"]),
            (&json!("123"), &json!("jpg"))
        );
        assert_eq!((&k["media_id"], &k["page_id"]), (&json!(""), &json!("")));
        assert_eq!(k["url"], "https://i.pinimg.com/originals/aa/bb/123.jpg");
        assert_eq!((&k["width"], &k["height"]), (&json!(800), &json!(600)));
        assert_eq!(
            (&k["category"], &k["subcategory"]),
            (&json!("pinterest"), &json!("pin"))
        );
        assert_eq!(k["created_at"], "Mon, 01 Jan 2024 00:00:00 +0000");
        assert!(!k.contains_key("date"));
        assert_eq!(
            paths(media),
            ("pinterest_123.jpg".into(), PathBuf::from("pinterest"))
        );

        // Promoted pins are not skipped upstream; foreign hosts are.
        let mut promoted = pin("126");
        promoted["is_promoted"] = json!(true);
        assert_eq!(pin_media(&promoted).len(), 1);
        let mut foreign = pin("127");
        foreign["images"] = json!({"orig": {"url": "https://evil.example/a.jpg"}});
        assert!(pin_media(&foreign).is_empty());
        assert!(pin_media(&json!({"id": "a/b", "images": pin("1")["images"]})).is_empty());
        assert!(pin_media(&json!({"id": "5"})).is_empty());
    }

    #[test]
    fn story_pin_blocks_follow_upstream() {
        let mut story = pin("125");
        story["story_pin_data"] = json!({"id": "S1", "pages": [
            {"id": "P1", "image_signature": "abcdef0123", "blocks": [
                {"type": "story_pin_image_block",
                    "image": {"images": {"originals": {
                        "url": "https://i.pinimg.com/originals/ab/cd/ef/abcdef0123.png",
                        "width": 10, "height": 20}}}},
                {"type": "story_pin_image_block", "image_signature": "ffeedd9999"},
                {"type": "story_pin_static_sticker_block"},
                {"type": "story_pin_product_sticker_block"}]},
            {"id": "P2", "blocks": [
                {"type": "story_pin_video_block", "video": {"id": "V9", "video_list": {
                    "V_EXP7": {"url": "https://v1.pinimg.com/videos/s2.mp4", "width": 720},
                    "V_EXP3": {"url": "https://v1.pinimg.com/videos/s3.mp4", "width": 360}}}},
                {"type": "story_pin_music_block", "audio": {"id": "A1",
                    "audio_url": "https://v1.pinimg.com/audio/a.m4a"}},
                {"type": "story_pin_paragraph_block", "id": "T1", "text": "Hello world. Bye"},
                {"type": "story_pin_paragraph_block", "text": "No id"},
                {"type": "story_pin_unknown_block", "image_signature": "123456abcd"},
                {"type": "story_pin_unknown_block"}]}]});
        let items = pin_media(&story);
        let urls: Vec<&str> = items.iter().map(|m| m.url.as_str()).collect();
        assert_eq!(urls.len(), 7, "{urls:?}");
        assert_eq!(
            urls[0],
            "https://i.pinimg.com/originals/ab/cd/ef/abcdef0123.png"
        );
        assert_eq!(
            urls[1],
            "https://i.pinimg.com/originals/ff/ee/dd/ffeedd9999.jpg"
        );
        assert_eq!(urls[2], "https://v1.pinimg.com/videos/s2.mp4");
        assert_eq!(urls[3], "https://v1.pinimg.com/audio/a.m4a");
        assert_eq!(
            urls[6],
            "https://i.pinimg.com/originals/12/34/56/123456abcd.jpg"
        );
        assert_eq!(
            items[0].handoff["_fallback"],
            json!([
                "https://i.pinimg.com/originals/ab/cd/ef/abcdef0123.jpg",
                "https://i.pinimg.com/originals/ab/cd/ef/abcdef0123.png",
                "https://i.pinimg.com/originals/ab/cd/ef/abcdef0123.webp"
            ])
        );
        assert_eq!(
            items[1].handoff["_fallback"],
            json!([
                "https://i.pinimg.com/originals/ff/ee/dd/ffeedd9999.png",
                "https://i.pinimg.com/originals/ff/ee/dd/ffeedd9999.webp"
            ])
        );
        let k = keywords(&items[0]);
        assert_eq!(
            (&k["story_id"], &k["page_id"], &k["media_id"]),
            (&json!("S1"), &json!("P1"), &json!("abcdef0123"))
        );
        assert_eq!((&k["count"], &k["width"]), (&json!(7), &json!(10)));
        assert_eq!(paths(&items[0]).0, "pinterest_125_abcdef0123.png");
        // The second image has no width; upstream's shared dict keeps it.
        assert_eq!(keywords(&items[1])["width"], 10);
        assert!(!keywords(&items[2]).contains_key("_fallback"));
        assert_eq!(keywords(&items[2])["media_id"], "V9");
        assert_eq!(keywords(&items[2])["page_id"], "P2");
        assert_eq!(keywords(&items[3])["media_id"], "A1");
        assert_eq!(
            items[3]
                .metadata
                .as_ref()
                .unwrap()
                .file
                .as_ref()
                .unwrap()
                .media_type
                .as_deref(),
            Some("audio")
        );

        let text = &items[4];
        let MediaJob::InlineText { data } = &text.job else {
            panic!("paragraph is not inline text");
        };
        assert_eq!(data, "Hello world. Bye");
        let k = keywords(text);
        assert_eq!(
            (&k["extension"], &k["media_id"]),
            (&json!("txt"), &json!("T1"))
        );
        assert_eq!(k["filename"], "text:Hello world");
        assert_eq!(text.filename, "text:Hello world.txt");
        // Upstream merges the audio dict into the shared pin dict, so its
        // `id` replaces the pin ID for this and later files.
        assert_eq!(k["id"], "A1");
        assert_eq!(text.metadata.as_ref().unwrap().post_id, "125");
        assert_eq!(paths(text).0, "pinterest_A1_T1.txt");
        // A paragraph without an ID falls back to the page ID.
        assert_eq!(keywords(&items[5])["media_id"], Value::Null);
        assert_eq!(paths(&items[5]).0, "pinterest_A1_P2.txt");
        assert_eq!(items[5].metadata.as_ref().unwrap().media_id, "P2");
        let keys: Vec<_> = items.iter().map(|m| m.archive_key()).collect();
        assert!(keys.iter().all(Option::is_some));
        assert_eq!(
            keys.iter().collect::<std::collections::BTreeSet<_>>().len(),
            7
        );

        // A single image block on a single page uses `images.orig`.
        let mut single = pin("130");
        single["story_pin_data"] = json!({"pages": [{"blocks": [
            {"type": "story_pin_image_block", "image_signature": "abcdef"}]}]});
        let items = pin_media(&single);
        assert_eq!(
            items[0].url.as_str(),
            "https://i.pinimg.com/originals/aa/bb/130.jpg"
        );
        assert_eq!(keywords(&items[0])["page_id"], Value::Null);
        // A malformed story skips the pin, as upstream's caught exception.
        let mut broken = pin("131");
        broken["story_pin_data"] =
            json!({"pages": [{"blocks": [{"type": "story_pin_video_block"}]}]});
        assert!(pin_media(&broken).is_empty());
        // `stories: false` ignores story data.
        let items = build_media(story.clone(), &context(&PIN, json!({})), false, true);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].filename, "125.jpg");
    }

    #[test]
    fn carousel_and_video_files() {
        let mut carousel = pin("140");
        carousel["carousel_data"] = json!({"carousel_slots": [
            {"id": "C1", "title": "one", "images": {"736x": {
                "url": "https://i.pinimg.com/736x/aa/bb/one.jpg"}}},
            {"id": "C2", "images": {"236x": {
                "url": "https://i.pinimg.com/236x/aa/bb/two.png"}, "736x": {}}}]});
        let items = pin_media(&carousel);
        assert_eq!(items.len(), 2);
        assert_eq!(
            items[0].url.as_str(),
            "https://i.pinimg.com/originals/aa/bb/one.jpg"
        );
        assert_eq!(
            items[1].url.as_str(),
            "https://i.pinimg.com/originals/aa/bb/two.png"
        );
        let k = keywords(&items[1]);
        assert_eq!((&k["media_id"], &k["num"]), (&json!("C2"), &json!(2)));
        assert_eq!(k["title"], "one", "the shared dict keeps earlier keys");
        assert_eq!(paths(&items[1]).0, "pinterest_140_C2.png");

        let mut video = pin("150");
        video["videos"] = json!({"video_list": {
            "V_HLSV3_MOBILE": {"url": "https://v1.pinimg.com/videos/hls/a.m3u8",
                "width": 1920, "height": 1080, "duration": 5000},
            "V_720P": {"url": "https://v1.pinimg.com/videos/720p/a.mp4", "width": 720},
            "V_EXP4": {"url": "https://v1.pinimg.com/videos/360p/a.mp4", "width": 360}}});
        let items = pin_media(&video);
        assert_eq!(items.len(), 1);
        let MediaJob::Manifest {
            protocol, base_url, ..
        } = &items[0].job
        else {
            panic!("HLS video is not a manifest job");
        };
        assert!(matches!(protocol, crate::extract::ManifestProtocol::Hls));
        assert_eq!(base_url.as_str(), "https://v1.pinimg.com/videos/hls/a.m3u8");
        assert_eq!(items[0].filename, "a.mp4");
        assert_eq!(
            items[0].handoff["_fallback"],
            json!(["https://v1.pinimg.com/videos/720p/a.mp4"])
        );
        let k = keywords(&items[0]);
        assert_eq!(
            (&k["extension"], &k["_ytdl_manifest"], &k["duration"]),
            (&json!("mp4"), &json!("hls"), &json!(5000))
        );
        assert_eq!(paths(&items[0]).0, "pinterest_150.mp4");
        assert_eq!(
            items[0]
                .metadata
                .as_ref()
                .unwrap()
                .file
                .as_ref()
                .unwrap()
                .media_type
                .as_deref(),
            Some("video")
        );

        // Without V_720P an HLS video falls back to the widest MP4.
        video["videos"]["video_list"]
            .as_object_mut()
            .unwrap()
            .remove("V_720P");
        let items = pin_media(&video);
        assert_eq!(
            items[0].handoff["_fallback"],
            json!(["https://v1.pinimg.com/videos/360p/a.mp4"])
        );
        // Without HLS the widest format is chosen.
        video["videos"] = json!({"video_list": {
            "V_EXP4": {"url": "https://v1.pinimg.com/videos/360p/b.mp4", "width": 360},
            "V_EXP7": {"url": "https://v1.pinimg.com/videos/720p/b.mp4", "width": 720}}});
        let items = pin_media(&video);
        assert_eq!(
            items[0].url.as_str(),
            "https://v1.pinimg.com/videos/720p/b.mp4"
        );
        assert!(matches!(items[0].job, MediaJob::DirectHttp));
        assert!(items[0].handoff.is_empty());
        assert!(!keywords(&items[0]).contains_key("_ytdl_manifest"));
        // `videos: false` downloads the preview image.
        let items = build_media(video, &context(&PIN, json!({})), true, false);
        assert_eq!(items[0].filename, "150.jpg");
    }

    #[test]
    fn class_metadata_and_directories() {
        let board = json!({"id": "900", "name": "Cats", "url": "/alice/cats/",
            "owner": {"username": "alice"}});
        let cases = [
            (
                &BOARD,
                json!({ "board": board }),
                "pinterest/alice/Cats",
                "{board[id]}_{id}",
            ),
            (
                &SECTION,
                json!({"board": board, "section": {"id": "77", "title": "Tabby"}}),
                "pinterest/alice/Cats/Tabby",
                "{board[id]}_{id}",
            ),
            (
                &RELATED_BOARD,
                json!({ "board": board }),
                "pinterest/alice/Cats/related",
                "{board[id]}_{id}",
            ),
            (
                &ALLPINS,
                json!({"user": "alice"}),
                "pinterest/alice",
                ARCHIVE,
            ),
            (
                &CREATED,
                json!({"user": "alice"}),
                "pinterest/alice",
                ARCHIVE,
            ),
            (
                &SEARCH,
                json!({"search": "red cat"}),
                "pinterest/Search/red cat",
                ARCHIVE,
            ),
            (
                &RELATED_PIN,
                json!({"original_pin": {"id": "55"}}),
                "pinterest/related 55",
                ARCHIVE,
            ),
        ];
        for (route, data, directory, archive) in cases {
            let items = build_media(pin("1"), &context(route, data.clone()), true, true);
            let k = keywords(&items[0]);
            for (key, value) in data.as_object().unwrap() {
                assert_eq!(&k[key], value);
            }
            assert_eq!(k["subcategory"], route.subcategory);
            assert_eq!(route.archive, archive);
            assert_eq!(
                paths(&items[0]),
                ("pinterest_1.jpg".into(), PathBuf::from(directory))
            );
        }
    }

    #[test]
    fn pagination_stops_like_upstream() {
        let body = |bookmarks: Value| json!({"resource": {"options": {"bookmarks": bookmarks}}});
        assert_eq!(next_bookmarks(&body(json!(["BM"]))), Some(json!(["BM"])));
        for end in [
            json!(["-end-"]),
            json!(["Y2JOb25lOnRydWU="]),
            json!([]),
            json!(null),
            json!([null]),
        ] {
            assert_eq!(next_bookmarks(&body(end.clone())), None, "{end}");
        }
        assert_eq!(next_bookmarks(&json!({"resource_response": {}})), None);
        let results = json!({"resource_response": {"data": {"results": [1, 2]}}});
        assert_eq!(page_results(&results).unwrap(), vec![json!(1), json!(2)]);
        assert!(page_results(&json!({"resource_response": {"data": null}})).is_err());
    }

    fn response(status: &str, extra: &str, body: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\nRetry-After: 0\r\n{extra}\r\n",
            body.len()
        )
        .into_bytes();
        out.extend_from_slice(body);
        out
    }

    /// A resource response; `bookmarks` of `None` omits `resource.options`.
    fn ok(data: Value, bookmarks: Option<&str>) -> Vec<u8> {
        let mut body = json!({"resource_response": {"status": "success", "data": data}});
        if let Some(bookmark) = bookmarks {
            body["resource"] = json!({"options": {"bookmarks": [bookmark]}});
        }
        response("200 OK", "", body.to_string().as_bytes())
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

    fn pinterest(root: Url) -> Pinterest {
        Pinterest::new(Duration::from_secs(5))
            .unwrap()
            .with_network(&NetworkOptions::new(Some(String::new()), None).unwrap())
            .unwrap()
            .with_api_root(root)
    }

    /// The request path and its decoded `data` options.
    fn decoded(request: &str) -> (String, Value) {
        let line = request.lines().next().unwrap();
        let url = Url::parse(&format!("http://x{}", line.split(' ').nth(1).unwrap())).unwrap();
        let pairs: Vec<(String, String)> = url.query_pairs().into_owned().collect();
        assert_eq!(pairs[1], ("source_url".into(), String::new()));
        assert_eq!(pairs[0].0, "data");
        let data: Value = serde_json::from_str(&pairs[0].1).unwrap();
        (url.path().to_owned(), data["options"].clone())
    }

    /// Events as labels: `+sub` begin child, `-` end child, `P` post,
    /// file names, `.` end post.
    fn labels(events: &[ExtractionEvent]) -> Vec<String> {
        events
            .iter()
            .map(|event| match event {
                ExtractionEvent::BeginChild(ChildScope { metadata, .. }) => {
                    format!("+{}", metadata.subcategory())
                }
                ExtractionEvent::EndChild(_) => "-".into(),
                ExtractionEvent::Post(_) => "P".into(),
                ExtractionEvent::Media(media) => media.filename.clone(),
                ExtractionEvent::EndPost => ".".into(),
            })
            .collect()
    }

    async fn run(native: &Pinterest, url: &str) -> Vec<ExtractionEvent> {
        native
            .events(&Url::parse(url).unwrap())
            .map(|event| event.unwrap())
            .collect()
            .await
    }

    fn media_at(events: &[ExtractionEvent], index: usize) -> &Media {
        match &events[index] {
            ExtractionEvent::Media(media) => media,
            _ => panic!("event {index} is not media"),
        }
    }

    #[tokio::test]
    async fn short_link_queues_pin_with_upstream_request() {
        let (root, server) = serve(vec![
            response(
                "302 Found",
                "Location: https://www.pinterest.com/pin/123/sent/?utm=x\r\n",
                b"",
            ),
            response("503 Service Unavailable", "", b""),
            ok(pin("123"), None),
        ]);
        let native = pinterest(root).with_retries(1);
        let events = run(&native, "https://pin.it/AbC").await;
        assert_eq!(labels(&events), ["+pinit", "P", "123.jpg", ".", "-"]);
        assert_eq!(keywords(media_at(&events, 2))["subcategory"], "pin");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /url_shortener/AbC/redirect/ "));
        let (path, options) = decoded(&requests[2]);
        assert_eq!(path, "/resource/PinResource/get/");
        assert_eq!(options, json!({"id": "123", "field_set_key": "detailed"}));
        let lower = requests[2].to_ascii_lowercase();
        for header in [
            "x-pinterest-appstate: active",
            "x-requested-with: xmlhttprequest",
            "x-app-version: a89153f",
            "x-pinterest-pws-handler: www/[username].js",
            "sec-fetch-mode: cors",
            "referer: https://www.pinterest.com/",
            "cookie: csrftoken=",
        ] {
            assert!(lower.contains(header), "{header}");
        }
        assert!(!requests[0].to_ascii_lowercase().contains("x-app-version"));

        // A short link to a board queues the board extractor.
        let (root, server) = serve(vec![
            response(
                "302 Found",
                "Location: https://www.pinterest.com/alice/cats/\r\n",
                b"",
            ),
            ok(
                json!({"id": "900", "name": "Cats", "section_count": 0}),
                None,
            ),
            ok(json!([pin("1")]), None),
        ]);
        let events = run(&pinterest(root), "https://pin.it/Board").await;
        assert_eq!(labels(&events), ["+pinit", "P", "1.jpg", ".", "-"]);
        let requests = server.join().unwrap();
        assert_eq!(decoded(&requests[1]).0, "/resource/BoardResource/get/");

        // Other locations are not found.
        let (root, server) = serve(vec![response(
            "302 Found",
            "Location: https://example.com/\r\n",
            b"",
        )]);
        let error = pinterest(root)
            .events(&Url::parse("https://pin.it/Elsewhere").unwrap())
            .collect::<Vec<_>>()
            .await
            .remove(0)
            .unwrap_err()
            .to_string();
        assert!(error.contains("did not resolve"), "{error}");
        server.join().unwrap();
    }

    #[tokio::test]
    async fn board_paginates_then_queues_sections() {
        let board = json!({"id": "900", "name": "Cats", "url": "/alice/cats/",
            "section_count": 2, "owner": {"username": "alice"}});
        let section = json!({"id": "77", "title": "Tabby", "preview_pins": [],
            "board": board});
        let (root, server) = serve(vec![
            ok(board.clone(), None),
            ok(json!([pin("1"), pin("2")]), Some("BM1")),
            ok(json!({"results": [pin("3")]}), Some("-end-")),
            ok(json!([{"id": "77"}, {"id": "bad/id"}]), Some("SB1")),
            // Children run lazily: the first section's lookup and pins come
            // before the next page of sections.
            ok(section, None),
            ok(json!([pin("4")]), None),
            ok(json!([{"id": "78"}]), Some("Y2JOb25lO123")),
            ok(json!({"id": "78", "title": "Black", "board": board}), None),
            ok(json!([]), None),
        ]);
        let native = pinterest(root);
        let events = run(&native, "https://www.pinterest.com/alice/cats/").await;
        assert_eq!(
            labels(&events),
            [
                "P", "1.jpg", ".", "P", "2.jpg", ".", "P", "3.jpg", ".", "+board", "P", "4.jpg",
                ".", "-", "+board", "-"
            ]
        );
        let first = media_at(&events, 1);
        assert_eq!(keywords(first)["board"]["id"], "900");
        assert_eq!(paths(first).1, PathBuf::from("pinterest/alice/Cats"));
        let sectioned = media_at(&events, 11);
        let k = keywords(sectioned);
        assert_eq!(k["subcategory"], "section");
        assert_eq!(k["section"], json!({"id": "77", "title": "Tabby"}));
        assert_eq!(k["board"]["url"], "/alice/cats/");
        assert_eq!(
            paths(sectioned).1,
            PathBuf::from("pinterest/alice/Cats/Tabby")
        );

        let requests = server.join().unwrap();
        let requests: Vec<_> = requests.iter().map(|r| decoded(r)).collect();
        assert_eq!(requests[0].0, "/resource/BoardResource/get/");
        assert_eq!(
            requests[0].1,
            json!({"slug": "cats", "username": "alice", "field_set_key": "detailed"})
        );
        assert_eq!(requests[1].0, "/resource/BoardFeedResource/get/");
        assert_eq!(
            requests[1].1,
            json!({"board_id": "900", "field_set_key": "react_grid_pin", "prepend": false,
                "bookmarks": null})
        );
        assert_eq!(requests[2].1["bookmarks"], json!(["BM1"]));
        assert_eq!(requests[3].0, "/resource/BoardSectionsResource/get/");
        assert_eq!(requests[3].1, json!({"board_id": "900"}));
        assert_eq!(requests[4].0, "/resource/BoardSectionResource/get/");
        assert_eq!(requests[4].1, json!({"section_id": "77"}));
        assert_eq!(requests[5].0, "/resource/BoardSectionPinsResource/get/");
        assert_eq!(requests[5].1, json!({"section_id": "77"}));
        assert_eq!(requests[6].0, "/resource/BoardSectionsResource/get/");
        assert_eq!(requests[6].1["bookmarks"], json!(["SB1"]));
        assert_eq!(requests[7].1, json!({"section_id": "78"}));

        // `sections: false` skips BoardSectionsResource.
        let (root, server) = serve(vec![ok(board.clone(), None), ok(json!([pin("1")]), None)]);
        let native = pinterest(root)
            .with_options(Some(json!({"sections": false}).as_object().unwrap()))
            .unwrap();
        let events = run(&native, "https://www.pinterest.com/alice/cats/").await;
        assert_eq!(labels(&events), ["P", "1.jpg", "."]);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn section_by_slug_and_related_board() {
        let board = json!({"id": "900", "name": "Cats", "owner": {"username": "alice"}});
        let (root, server) = serve(vec![
            ok(json!({"id": "77", "title": "Tabby", "board": board}), None),
            ok(json!([pin("4")]), Some("-end-")),
        ]);
        let events = run(
            &pinterest(root),
            "https://www.pinterest.com/alice/cats/tabby%20cat",
        )
        .await;
        assert_eq!(labels(&events), ["P", "4.jpg", "."]);
        let requests = server.join().unwrap();
        let (path, options) = decoded(&requests[0]);
        assert_eq!(path, "/resource/BoardSectionResource/get/");
        assert_eq!(
            options,
            json!({"board_slug": "cats", "section_slug": "tabby cat", "username": "alice"})
        );

        let (root, server) = serve(vec![
            ok(
                json!({"id": "900", "name": "Cats", "section_count": 3, "url": "/alice/cats/",
                "owner": {"username": "alice"}}),
                None,
            ),
            ok(json!([pin("5")]), None),
        ]);
        let events = run(
            &pinterest(root),
            "https://www.pinterest.com/alice/cats/#related",
        )
        .await;
        assert_eq!(labels(&events), ["P", "5.jpg", "."]);
        let media = media_at(&events, 1);
        assert_eq!(keywords(media)["subcategory"], "related-board");
        assert_eq!(
            paths(media).1,
            PathBuf::from("pinterest/alice/Cats/related")
        );
        let requests = server.join().unwrap();
        let (path, options) = decoded(&requests[1]);
        assert_eq!(path, "/resource/BoardContentRecommendationResource/get/");
        assert_eq!(
            options,
            json!({"id": "900", "type": "board", "add_vase": true})
        );
    }

    #[tokio::test]
    async fn user_queues_each_board() {
        let (root, server) = serve(vec![
            ok(
                json!([{"id": "900", "name": "Cats", "url": "/alice/cats/"},
                    {"id": "901", "name": "No URL"},
                    {"id": "902", "name": "Foreign", "url": "//evil.example/x/y/"}]),
                Some("BM"),
            ),
            ok(json!({"id": "900", "name": "Cats"}), None),
            ok(json!([pin("1")]), None),
            ok(
                json!([{"id": "903", "name": "Dogs", "url": "/alice/dogs/"}]),
                None,
            ),
            ok(json!({"id": "903", "name": "Dogs"}), None),
            ok(json!([]), None),
        ]);
        let events = run(&pinterest(root), "https://www.pinterest.com/alice/_saved/").await;
        assert_eq!(
            labels(&events),
            ["+user", "P", "1.jpg", ".", "-", "+user", "-"]
        );
        let ExtractionEvent::BeginChild(scope) = &events[0] else {
            unreachable!()
        };
        assert_eq!(scope.metadata.fields()["name"], "Cats");
        assert_eq!(scope.metadata.fields()["id"], "900");
        let requests = server.join().unwrap();
        let requests: Vec<_> = requests.iter().map(|r| decoded(r)).collect();
        assert_eq!(requests[0].0, "/resource/BoardsResource/get/");
        assert_eq!(
            requests[0].1,
            json!({"sort": "last_pinned_to", "field_set_key": "profile_grid_item",
                "filter_stories": false, "username": "alice", "page_size": 25,
                "include_archived": true})
        );
        assert_eq!(requests[1].1["slug"], "cats");
        assert_eq!(requests[3].0, "/resource/BoardsResource/get/");
        assert_eq!(requests[3].1["bookmarks"], json!(["BM"]));
        assert_eq!(requests[4].1["slug"], "dogs");
    }

    #[tokio::test]
    async fn feeds_use_upstream_resources() {
        let cases = [
            (
                "https://www.pinterest.com/alice/pins/",
                "/resource/UserPinsResource/get/",
                json!({"is_own_profile_pins": false, "username": "alice",
                    "field_set_key": "grid_item", "pin_filter": null}),
                ("user", json!("alice")),
                "pinterest/alice",
            ),
            (
                "https://www.pinterest.com/alice/_created/",
                "/resource/UserActivityPinsResource/get/",
                json!({"exclude_add_pin_rep": true, "field_set_key": "grid_item",
                    "is_own_profile_pins": false, "username": "alice"}),
                ("user", json!("alice")),
                "pinterest/alice",
            ),
            (
                "https://www.pinterest.com/search/pins/?q=red%20cat",
                "/resource/BaseSearchResource/get/",
                json!({"query": "red cat", "scope": "pins", "rs": "typed"}),
                ("search", json!("red cat")),
                "pinterest/Search/red cat",
            ),
        ];
        for (url, resource, options, (key, value), directory) in cases {
            let (root, server) = serve(vec![
                ok(json!({"results": [pin("1")]}), Some("NEXT")),
                ok(json!({"results": [pin("2")]}), None),
            ]);
            let events = run(&pinterest(root), url).await;
            assert_eq!(
                labels(&events),
                ["P", "1.jpg", ".", "P", "2.jpg", "."],
                "{url}"
            );
            let media = media_at(&events, 1);
            assert_eq!(keywords(media)[key], value);
            assert_eq!(paths(media).1, PathBuf::from(directory));
            let requests = server.join().unwrap();
            assert_eq!(decoded(&requests[0]), (resource.to_owned(), options));
            assert_eq!(decoded(&requests[1]).1["bookmarks"], json!(["NEXT"]));
        }

        let (root, server) = serve(vec![ok(pin("55"), None), ok(json!([pin("56")]), None)]);
        let events = run(
            &pinterest(root),
            "https://www.pinterest.com/pin/55/#related",
        )
        .await;
        assert_eq!(labels(&events), ["P", "56.jpg", "."]);
        let media = media_at(&events, 1);
        assert_eq!(keywords(media)["original_pin"]["id"], "55");
        assert_eq!(paths(media).1, PathBuf::from("pinterest/related 55"));
        let requests = server.join().unwrap();
        assert_eq!(
            decoded(&requests[1]),
            (
                "/resource/RelatedPinFeedResource/get/".to_owned(),
                json!({"pin": "55", "add_vase": true, "pins_only": true})
            )
        );
    }

    #[tokio::test]
    async fn errors_name_the_resource_without_urls() {
        for (url, what) in [
            ("https://www.pinterest.com/alice/pins/", "allpins"),
            ("https://www.pinterest.com/pin/1/#related", "pin"),
            ("https://www.pinterest.com/alice/cats/x", "section"),
        ] {
            let (root, server) = serve(vec![response("404 Not Found", "", b"")]);
            let error = pinterest(root)
                .events(&Url::parse(url).unwrap())
                .collect::<Vec<_>>()
                .await
                .remove(0)
                .unwrap_err()
                .to_string();
            assert_eq!(error, format!("Pinterest {what} not found"));
            server.join().unwrap();
        }
        let (root, server) = serve(vec![
            ok(json!([pin("1")]), Some("NEXT")),
            response("200 OK", "", b"<html>"),
        ]);
        let events: Vec<_> = pinterest(root)
            .events(&Url::parse("https://www.pinterest.com/alice/pins/").unwrap())
            .collect()
            .await;
        assert_eq!(events.len(), 4);
        let error = events[3].as_ref().unwrap_err().to_string();
        assert!(error.contains("invalid JSON") && !error.contains("127.0.0.1"));
        server.join().unwrap();
        let (root, server) = serve(vec![ok(Value::Null, None)]);
        let error = pinterest(root)
            .stream(&Url::parse("https://www.pinterest.com/pin/9/").unwrap())
            .collect::<Vec<_>>()
            .await
            .remove(0)
            .unwrap_err()
            .to_string();
        assert_eq!(error, "Pinterest pin not found");
        server.join().unwrap();
    }

    #[tokio::test]
    async fn session_cookies_unlock_secret_boards_with_a_matching_csrf_token() {
        let board = json!({"id": "901", "name": "Secret", "url": "/alice/secret/",
            "privacy": "secret", "section_count": 0, "owner": {"username": "alice"}});
        let (root, server) = serve(vec![
            ok(board.clone(), None),
            ok(json!([pin("1")]), None),
            ok(board, None),
            ok(json!([pin("2")]), None),
            response("403 Forbidden", "", b"{}"),
        ]);
        let host = root.host_str().unwrap().to_owned();
        let jar = |extra: &str| {
            crate::cookies::CookieJar::parse(&format!(
                "{host}\tFALSE\t/\tFALSE\t0\t_pinterest_sess\tsess1\n{extra}"
            ))
            .unwrap()
        };
        // Without its own token the session gets upstream's random one.
        let native = pinterest(root.clone()).with_cookies(jar(""));
        let events = run(&native, "https://www.pinterest.com/alice/secret/").await;
        assert_eq!(labels(&events), ["P", "1.jpg", "."]);
        let token = native.csrf_token.clone();
        // A logged-in session's own token is used instead.
        let own = jar(&format!("{host}\tFALSE\t/\tFALSE\t0\tcsrftoken\tusertok\n"));
        let native = pinterest(root.clone()).with_cookies(own);
        let events = run(&native, "https://www.pinterest.com/alice/secret/").await;
        assert_eq!(labels(&events), ["P", "2.jpg", "."]);
        let error = pinterest(root)
            .events(&Url::parse("https://www.pinterest.com/pin/9/").unwrap())
            .collect::<Vec<_>>()
            .await
            .remove(0)
            .unwrap_err()
            .to_string();
        assert!(error.contains("--cookies"), "{error}");
        let requests = server.join().unwrap();
        for request in &requests[..2] {
            let lower = request.to_ascii_lowercase();
            assert!(lower.contains("_pinterest_sess=sess1"), "{request}");
            assert!(lower.contains(&format!("csrftoken={token}")), "{request}");
            assert!(
                lower.contains(&format!("x-csrftoken: {token}")),
                "{request}"
            );
        }
        for request in &requests[2..4] {
            let lower = request.to_ascii_lowercase();
            assert!(lower.contains("_pinterest_sess=sess1"), "{request}");
            assert_eq!(lower.matches("csrftoken=").count(), 1, "{request}");
            assert!(lower.contains("csrftoken=usertok"), "{request}");
            assert!(lower.contains("x-csrftoken: usertok"), "{request}");
        }
        assert!(!requests[4].contains("_pinterest_sess"));
    }
}
