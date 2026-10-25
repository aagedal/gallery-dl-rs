//! Experimental native Fur Affinity extraction.
//!
//! Mirrors gallery_dl/extractor/furaffinity.py: the `post`, `gallery`,
//! `scraps`, `folder`, `favorite`, `search`, `submissions`, `following` and
//! `user` (`include` dispatch of `gallery`/`scraps`/`favorite`) classes.
//! Submission pages (`/view/ID/`) are scraped like upstream's `_parse_post`
//! for the new and old site layouts (`layout` option, detected by default):
//! the `d.furaffinity.net` file, title, artist, tags, description (`text` or
//! `descriptions: "html"`), statistics, rating, category, species, gender,
//! size and folders. Files carry upstream's keyword dictionary and default
//! `{id}{title:? //}.{extension}` filenames in the class directories; archive
//! identities remain the submission ID. Listings page lazily (capped at 200
//! pages); search pages are POSTed like upstream. The `external` option
//! queues description links to other extractors. Session cookies (`a`, `b`)
//! from the cookie file are sent to Fur Affinity when present so mature
//! submissions and `/msg/submissions` work; submissions without a download
//! link are skipped in listings. Media must be HTTPS on `d.furaffinity.net`
//! or `*.facdn.net`.
use crate::{
    Result,
    cookies::CookieJar,
    error,
    event::{ChildMetadata, ExtractionEvent, KeywordPolicy, Queue, child_job, group_with_keywords},
    extract::{Media, MediaJob, PostMetadata},
    file_metadata::FileMetadata,
    network::NetworkOptions,
    sites::{
        Route, Scan, SiteKeywords, extract_iter, parse_int, remove_html, split_html, unescape,
    },
    transport::{RateLimits, Redirects, retry_after},
};
use futures_util::{
    StreamExt,
    stream::{self, BoxStream},
};
use reqwest::{Client, StatusCode};
use serde_json::{Map, Value};
use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
    time::{Duration, SystemTime},
};
use url::Url;

type Events<'a> = BoxStream<'a, Result<ExtractionEvent>>;

const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:128.0) Gecko/20100101 Firefox/128.0";
const MAX_RESPONSE: usize = 16 * 1024 * 1024;
const MAX_PAGES: u64 = 200;
const NOT_AVAILABLE: &str = "Fur Affinity submission has no download link";

/// Upstream `FuraffinityExtractor.filename_fmt` and `directory_fmt`.
const FILENAME: &str = "{id}{title:? //}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{user!l}"];
static POST: Route = Route::new("post", FILENAME, DIRECTORY, "{id}");
static GALLERY: Route = Route::new("gallery", FILENAME, DIRECTORY, "{id}");
static SCRAPS: Route = Route::new(
    "scraps",
    FILENAME,
    &["{category}", "{user!l}", "Scraps"],
    "{id}",
);
static FOLDER: Route = Route::new(
    "folder",
    FILENAME,
    &[
        "{category}",
        "{user!l}",
        "Folders",
        "{folder_id}{folder_name:? //}",
    ],
    "{id}",
);
static FAVORITE: Route = Route::new(
    "favorite",
    FILENAME,
    &["{category}", "{user!l}", "Favorites"],
    "{id}",
);
static SEARCH: Route = Route::new(
    "search",
    FILENAME,
    &["{category}", "Search", "{search}"],
    "{id}",
);
static SUBMISSIONS: Route = Route::new("submissions", FILENAME, DIRECTORY, "{id}");

/// Views of upstream's `FuraffinityUserExtractor` dispatcher.
const USER_VIEWS: &[&str] = &["gallery", "scraps", "favorite"];

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

/// A raw URL path segment kept as upstream's regex group.
fn valid_segment(segment: &str) -> bool {
    (1..=256).contains(&segment.len()) && !segment.bytes().any(|b| b.is_ascii_control())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Folder {
    Gallery,
    Scraps,
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
    /// A gallery folder; `name` is the raw optional URL segment.
    Folder {
        user: String,
        id: String,
        name: String,
    },
    /// Favorites, optionally from a `ID/next` or `ID/prev` cursor.
    Favorites {
        user: String,
        start: Option<String>,
    },
    /// Search form fields from the URL query (`q` from the path if absent).
    Search {
        query: Vec<(String, String)>,
    },
    /// The logged-in `/msg/submissions` inbox path.
    Submissions {
        path: String,
    },
    User {
        user: String,
    },
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

/// Upstream `BASE_PATTERN` hosts: `furaffinity.net` and its `fx`/`xfu`/
/// `fxfu` embed mirrors, optionally under `www.` or `sfw.`.
fn site_host(host: &str) -> bool {
    let host = host
        .strip_prefix("www.")
        .or_else(|| host.strip_prefix("sfw."))
        .unwrap_or(host);
    matches!(
        host,
        "furaffinity.net" | "fxraffinity.net" | "xfuraffinity.net" | "fxfuraffinity.net"
    )
}

pub fn target(url: &Url) -> Result<Target> {
    let unsupported = || {
        error(
            "unsupported Fur Affinity URL; expected https://www.furaffinity.net/view/ID/, /gallery/USER/, /scraps/USER/, /favorites/USER/, /user/USER/, /watchlist/by/USER/, /search/?q=... or /msg/submissions",
        )
    };
    if !plain(url) || !url.host_str().is_some_and(site_host) {
        return Err(unsupported());
    }
    let parts: Vec<&str> = url
        .path_segments()
        .map(|s| s.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    let user = |name: &str| valid_name(name).then(|| name.to_ascii_lowercase());
    match parts[..] {
        ["view" | "full", id, ..] if valid_id(id) => Ok(Target::Submission { id: id.into() }),
        ["gallery", name, "folder", id, ref rest @ ..] if valid_id(id) && rest.len() <= 1 => {
            let folder = rest.first().copied().unwrap_or_default();
            if !folder.is_empty() && !valid_segment(folder) {
                return Err(unsupported());
            }
            Ok(Target::Folder {
                user: user(name).ok_or_else(unsupported)?,
                id: id.into(),
                name: folder.into(),
            })
        }
        ["gallery", _, "folder", ..] => Err(unsupported()),
        [folder @ ("gallery" | "scraps"), name, ..] => Ok(Target::Gallery {
            user: user(name).ok_or_else(unsupported)?,
            folder: if folder == "gallery" {
                Folder::Gallery
            } else {
                Folder::Scraps
            },
        }),
        ["favorites", name, ref rest @ ..] => {
            let start = match rest {
                [] => None,
                [id, way @ ("next" | "prev"), ..] if valid_id(id) => Some(format!("{id}/{way}")),
                _ => None,
            };
            Ok(Target::Favorites {
                user: user(name).ok_or_else(unsupported)?,
                start,
            })
        }
        ["user", name, ..] => Ok(Target::User {
            user: user(name).ok_or_else(unsupported)?,
        }),
        ["watchlist", "by", name, ..] => Ok(Target::Following {
            user: user(name).ok_or_else(unsupported)?,
        }),
        ["msg", "submissions", ref rest @ ..] if rest.len() <= 1 => {
            if rest.first().is_some_and(|segment| !valid_segment(segment)) {
                return Err(unsupported());
            }
            Ok(Target::Submissions {
                path: url.path().trim_end_matches('/').to_owned(),
            })
        }
        ["search", ref rest @ ..] if rest.len() <= 1 => {
            let raw = url
                .query()
                .filter(|q| !q.is_empty())
                .ok_or_else(unsupported)?;
            let mut query = crate::sites::parse_query(raw);
            if let Some(term) = rest.first()
                && !query.iter().any(|(k, _)| k == "q")
            {
                query.push(("q".into(), crate::sites::percent_decode(term)));
            }
            Ok(Target::Search { query })
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

/// gallery-dl `text.rextr`: the text between the last `begin` before `pos`
/// and the following `end`.
fn rextr<'t>(text: &'t str, begin: &str, end: &str, pos: Option<usize>) -> &'t str {
    let head = &text[..pos.unwrap_or(text.len()).min(text.len())];
    let Some(first) = head.rfind(begin).map(|at| at + begin.len()) else {
        return "";
    };
    text[first..]
        .find(end)
        .map_or("", |last| &text[first..first + last])
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

/// Upstream `_parse_post` options.
#[derive(Clone, Copy, Debug, Default)]
pub struct Layout {
    /// `Some(true)` new, `Some(false)` old, `None` detected per page.
    pub new: Option<bool>,
    /// `descriptions: "html"` keeps the description's HTML.
    pub html_descriptions: bool,
}

/// One parsed submission: its file URL and upstream keyword dictionary.
pub struct Submission {
    pub url: Url,
    pub fields: Map<String, Value>,
}

/// `_parse_post`'s fields from the current submission page layout (stats
/// in `submission-page-stats`, details in `submission-content-stats`),
/// which upstream's new/old layout markers no longer match. Returns the raw
/// description HTML.
fn parse_current<'t>(html: &'t str, data: &mut Map<String, Value>) -> &'t str {
    let rh = remove_html;
    let find = |begin: &str, end: &str| crate::sites::extr(html, begin, end).unwrap_or_default();
    let tags: Vec<Value> = extract_iter(html, "data-tag-name=\"", "\"")
        .into_iter()
        .map(|tag| Value::from(unescape(tag)))
        .collect();
    data.insert("tags".into(), tags.into());
    data.insert("scraps".into(), false.into());
    let title = find("<div class=\"submission-title\">", "</div>");
    data.insert("title".into(), unescape(&strip_html(title)).into());
    let header = html
        .find("class=\"submission-title\"")
        .map_or("", |at| &html[at..]);
    let artist_url = crate::sites::extr(header, "href=\"/user/", "/\"").unwrap_or_default();
    data.insert("artist_url".into(), artist_url.trim().into());
    let artist = crate::sites::extr(header, "__displayName\"", "</span>")
        .and_then(|span| span.rsplit('>').next())
        .unwrap_or_default();
    data.insert("artist".into(), unescape(artist.trim()).into());
    let rest = html
        .find("class=\"submission-description-text user-submitted-links\">")
        .map_or("", |at| {
            &html[at + "class=\"submission-description-text user-submitted-links\">".len()..]
        });
    let end = ["<div class=\"submission-footer\">", "</div>"]
        .iter()
        .filter_map(|marker| rest.find(marker))
        .min()
        .unwrap_or(0);
    let description = &rest[..end];
    for (key, label) in [
        ("views", "Views"),
        ("comments", "Comments"),
        ("favorites", "Favorites"),
    ] {
        let value = parse_int(&rh(find(&format!("<div title=\"{label}\">"), "</div>")), 0);
        data.insert(key.into(), value.into());
    }
    let rating = crate::sites::extr(
        find("class=\"submission-page-stats\"", "Rating</div>"),
        "c-contentRating--",
        "</div>",
    )
    .and_then(|block| block.split_once('>'))
    .map_or("", |(_, text)| text.trim());
    data.insert("rating".into(), rating.into());
    let details = split_html(find("class=\"submission-content-stats\">", "</div>"));
    let (labels, values) = details.split_at(details.len() / 2);
    let detail = |label: &str| {
        labels
            .iter()
            .position(|l| l == label)
            .and_then(|i| values.get(i))
            .cloned()
            .unwrap_or_default()
    };
    data.insert("fa_category".into(), detail("Category").into());
    data.insert("theme".into(), detail("Theme").into());
    data.insert("species".into(), detail("Species").into());
    data.insert("gender".into(), detail("Gender").into());
    let resolution = detail("Resolution");
    let (width, height) = resolution.split_once('x').unwrap_or_default();
    data.insert("width".into(), parse_int(width, 0).into());
    data.insert("height".into(), parse_int(height, 0).into());
    let folders: Vec<Value> = extract_iter(html, "<div class=\"submission-folder\">", "</a>")
        .into_iter()
        .map(rh)
        .filter(|folder| !folder.is_empty())
        .map(Value::from)
        .collect();
    data.insert("folders".into(), folders.into());
    description
}

/// Upstream `_parse_post` for a submission page. `user` is the listing's
/// user; single posts use the artist.
pub fn parse_post(
    bytes: &[u8],
    id: &str,
    layout: Layout,
    user: Option<&str>,
) -> Result<Submission> {
    let html =
        std::str::from_utf8(bytes).map_err(|_| error("Fur Affinity returned invalid HTML"))?;
    let mut extr = Scan::new(html);
    let new_layout = layout
        .new
        .unwrap_or_else(|| !extr.extr("<meta ", ">").contains("http-equiv="));
    // Upstream takes the first `href="//d…`; current pages link a
    // stylesheet on `d.furaffinity.net` before the file, so skip non-art
    // links.
    let path = loop {
        match extr.find("href=\"//d", "\"") {
            Some(path) if path.contains("/art/") => break path,
            Some(_) => continue,
            None => return Err(error(NOT_AVAILABLE)),
        }
    };
    let url = media_url(&format!("//d{path}")).ok_or_else(|| error(NOT_AVAILABLE))?;
    let rh = remove_html;
    let pi = |text: &str| Value::from(parse_int(text, 0));

    let mut data = Map::new();
    data.insert("id".into(), pi(id));
    data.insert("url".into(), url.as_str().into());
    let (name, ext) = crate::sites::nameext(&url);
    data.insert("filename".into(), name.clone().into());
    data.insert("extension".into(), ext.into());
    let description;
    // Pages served now (`submission-page-stats`) match neither of
    // upstream's layouts; detected pages read the same fields from it.
    if layout.new.is_none() && html.contains("class=\"submission-page-stats\"") {
        description = parse_current(html, &mut data);
    } else if new_layout {
        let tags = split_html(extr.extr("<h3>Keywords</h3>", "</section>"));
        data.insert("tags".into(), tags.into());
        let scraps = extr.extr(" submissions\">", "<") == "Scraps";
        data.insert("scraps".into(), scraps.into());
        data.insert(
            "title".into(),
            unescape(extr.extr("<h2><p>", "</p></h2>")).into(),
        );
        data.insert(
            "artist_url".into(),
            extr.extr("title=\"", "\"").trim().into(),
        );
        data.insert("artist".into(), extr.extr(">", "<").into());
        description = extr.extr(
            "class=\"submission-description user-submitted-links\">",
            "                                    </div>",
        );
        data.insert(
            "views".into(),
            pi(&rh(extr.extr("class=\"views\">", "</span>"))),
        );
        data.insert(
            "favorites".into(),
            pi(&rh(extr.extr("class=\"favorites\">", "</span>"))),
        );
        data.insert(
            "comments".into(),
            pi(&rh(extr.extr("class=\"comments\">", "</span>"))),
        );
        data.insert(
            "rating".into(),
            rh(extr.extr("class=\"rating\">", "</span>")).into(),
        );
        data.insert(
            "fa_category".into(),
            rh(extr.extr(">Category</strong>", "</span>")).into(),
        );
        data.insert("theme".into(), rh(extr.extr(">", "<")).into());
        data.insert(
            "species".into(),
            rh(extr.extr(">Species</strong>", "</div>")).into(),
        );
        data.insert(
            "gender".into(),
            rh(extr.extr(">Gender</strong>", "</div>")).into(),
        );
        data.insert("width".into(), pi(extr.extr("<span>", "x")));
        data.insert("height".into(), pi(extr.extr("", "p")));
        let folders: Vec<Value> = extr
            .extr("<h3>Listed in Folders</h3>", "</section>")
            .split("</a>")
            .map(rh)
            .filter(|folder| !folder.is_empty())
            .map(Value::from)
            .collect();
        data.insert("folders".into(), folders.into());
    } else {
        let scraps = extr
            .extr("class=\"minigallery-title", "</a>")
            .contains("/scraps/");
        data.insert("scraps".into(), scraps.into());
        data.insert("title".into(), unescape(extr.extr("<h2>", "</h2>")).into());
        data.insert(
            "artist_url".into(),
            extr.extr("title=\"", "\"").trim().into(),
        );
        data.insert("artist".into(), extr.extr(">", "<").into());
        for (key, label) in [
            ("fa_category", "<b>Category:</b>"),
            ("theme", "<b>Theme:</b>"),
            ("species", "<b>Species:</b>"),
            ("gender", "<b>Gender:</b>"),
        ] {
            data.insert(key.into(), extr.extr(label, "<").trim().into());
        }
        for (key, label) in [
            ("favorites", "<b>Favorites:</b>"),
            ("comments", "<b>Comments:</b>"),
            ("views", "<b>Views:</b>"),
        ] {
            data.insert(key.into(), pi(extr.extr(label, "<")));
        }
        data.insert("width".into(), pi(extr.extr("<b>Resolution:</b>", "x")));
        data.insert("height".into(), pi(extr.extr("", "<")));
        let tags: Vec<String> = split_html(extr.extr("id=\"keywords\">", "</div>"))
            .into_iter()
            .step_by(2)
            .collect();
        data.insert("tags".into(), tags.into());
        data.insert("rating".into(), extr.extr("<img alt=\"", " ").into());
        description = extr.extr(
            "<td valign=\"top\" align=\"left\" width=\"70%\" class=\"alt1\" style=\"padding:8px\">",
            "                               </td>",
        );
        data.insert("folders".into(), Value::Array(Vec::new()));
    }
    data.insert("_description".into(), description.into());
    let user = user
        .filter(|user| !user.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            data.get("artist_url")
                .and_then(Value::as_str)
                .map(Into::into)
        })
        .unwrap_or_default();
    data.insert("user".into(), user.into());
    let timestamp = name
        .split('.')
        .next()
        .and_then(|stamp| crate::source_date::parse_timestamp(&Value::from(stamp)));
    data.insert(
        "date".into(),
        match timestamp {
            Some(_) => crate::filter::source_date_text(timestamp).into(),
            None => Value::Null,
        },
    );
    data.insert(
        "description".into(),
        if layout.html_descriptions {
            description.trim().into()
        } else {
            unescape(&strip_html(description)).into()
        },
    );
    let dir = path.rsplit('/').nth(1).unwrap_or_default();
    data.insert(
        "thumbnail".into(),
        format!("https://t.furaffinity.net/{id}@600-{dir}.jpg").into(),
    );
    Ok(Submission { url, fields: data })
}

/// The submission's media with the class keywords (`category`,
/// `subcategory` and class metadata added).
fn submission_media(
    submission: Submission,
    route: &'static Route,
    extra: &Map<String, Value>,
) -> Media {
    let Submission { url, mut fields } = submission;
    for (key, value) in extra {
        fields.insert(key.clone(), value.clone());
    }
    fields.insert("category".into(), "furaffinity".into());
    fields.insert("subcategory".into(), route.subcategory.into());
    let text = |key: &str| {
        fields
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    let id = fields
        .get("id")
        .map(crate::sites::scalar)
        .unwrap_or_default();
    let rating = text("rating").map(|r| r.to_ascii_lowercase());
    let sensitive: Arc<[Arc<str>]> = rating
        .filter(|r| r != "general")
        .map(|r| vec![Arc::<str>::from(r)])
        .unwrap_or_default()
        .into();
    let dimension = |key: &str| fields.get(key).and_then(Value::as_u64).filter(|v| *v > 0);
    let timestamp = text("date").and_then(|d| crate::date::parse_date(&d).ok());
    let filename = match (text("filename"), text("extension")) {
        (Some(name), Some(ext)) => format!("{name}.{ext}"),
        (name, _) => name.unwrap_or_default(),
    };
    Media {
        filename,
        job: MediaJob::DirectHttp,
        category: "furaffinity".into(),
        metadata: Some(PostMetadata {
            file: Some(FileMetadata {
                sensitive_flags: sensitive,
                width: dimension("width"),
                height: dimension("height"),
                ..FileMetadata::default()
            }),
            post_id: id.clone(),
            media_id: id,
            username: text("user").map(|u| Arc::from(u.to_ascii_lowercase())),
            caption: text("title").map(Arc::from),
            timestamp,
            index: 1,
            site_keywords: Some(Arc::new(SiteKeywords { route, fields })),
            ..PostMetadata::default()
        }),
        url,
        headers: BTreeMap::new(),
        handoff: BTreeMap::new(),
    }
}

/// Parse a submission page into its `post` class media.
pub fn parse_submission(bytes: &[u8], id: &str) -> Result<Vec<Media>> {
    let submission = parse_post(bytes, id, Layout::default(), None)?;
    Ok(vec![submission_media(submission, &POST, &Map::new())])
}

/// Submission IDs listed on one gallery page, in order.
pub fn parse_gallery(bytes: &[u8]) -> Result<Vec<String>> {
    let html =
        std::str::from_utf8(bytes).map_err(|_| error("Fur Affinity returned invalid HTML"))?;
    let mut ids = Vec::new();
    for id in extract_iter(html, "id=\"sid-", "\"") {
        if valid_id(id) && !ids.iter().any(|i| i == id) {
            ids.push(id.to_owned());
        }
    }
    Ok(ids)
}

/// One favorites page: `(submission ID, data-fav-id)` pairs and the next
/// page's path (upstream `_pagination_favorites`).
pub fn parse_favorites(bytes: &[u8]) -> Result<(Vec<(String, i64)>, String)> {
    let page =
        std::str::from_utf8(bytes).map_err(|_| error("Fur Affinity returned invalid HTML"))?;
    let mut extr = Scan::new(page);
    let mut posts = Vec::new();
    while let Some(id) = extr.find("id=\"sid-", "\"") {
        let favorite = parse_int(extr.extr("data-fav-id=\"", "\""), 0);
        if valid_id(id) {
            posts.push((id.to_owned(), favorite));
        }
    }
    let next = match page.find("type=\"submit\">Next</button>") {
        Some(pos) => rextr(page, "<form action=\"", "\"", Some(pos)),
        None => crate::sites::extr(page, "right\" href=\"", "\"").unwrap_or_default(),
    };
    Ok((posts, unescape(next)))
}

fn html(bytes: &[u8]) -> Result<&str> {
    std::str::from_utf8(bytes).map_err(|_| error("Fur Affinity returned invalid HTML"))
}

/// A same-site path for the next page, without a scheme or host.
fn site_path(path: &str, prefix: &str) -> Option<String> {
    (path.starts_with(prefix) && !path.contains("//") && !path.contains('\\'))
        .then(|| path.to_owned())
}

pub struct FurAffinity {
    client: Client,
    network: NetworkOptions,
    root: Url,
    cookie: Option<String>,
    layout: Layout,
    external: bool,
    include: Option<crate::dispatch::Include>,
    timeout: Duration,
    request_timeout: Duration,
    retries: u8,
    rate_limits: RateLimits,
}

/// Pagination of one listing class.
enum Pages {
    /// `PATH/USER[/folder/ID/a]` followed by `/NUM/`.
    Gallery {
        path: String,
    },
    Favorites {
        next: Option<String>,
    },
    Search {
        data: Vec<(String, String)>,
    },
    Submissions {
        next: Option<String>,
    },
}

struct ListState {
    pages: Pages,
    num: u64,
    fetched: u64,
    done: bool,
    seen: HashSet<String>,
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
            layout: Layout::default(),
            external: false,
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

    /// Apply upstream extractor settings (`extractor.furaffinity.*`):
    /// `descriptions` (`"html"` keeps description HTML), `external`
    /// (boolean) and `layout` (`"auto"`, `"new"` or `"old"`).
    pub fn with_options(mut self, options: Option<&Map<String, Value>>) -> Result<Self> {
        let Some(options) = options else {
            return Ok(self);
        };
        self.layout.html_descriptions =
            options.get("descriptions").and_then(Value::as_str) == Some("html");
        self.external = match options.get("external") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(value)) => *value,
            Some(_) => {
                return Err(error(
                    "extractor.furaffinity.external must be true or false",
                ));
            }
        };
        self.layout.new = match options.get("layout") {
            None | Some(Value::Null) => None,
            Some(Value::String(layout)) if layout.is_empty() || layout == "auto" => None,
            Some(Value::String(layout)) => Some(layout != "old"),
            Some(_) => {
                return Err(error(
                    "extractor.furaffinity.layout must be \"auto\", \"new\" or \"old\"",
                ));
            }
        };
        Ok(self)
    }

    /// Views queued for `/user/USER/` (default `gallery`).
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

    /// Override the site origin; intended for local tests.
    pub fn with_root(mut self, root: Url) -> Self {
        self.root = root;
        self
    }

    async fn call(&self, path: &str, form: Option<String>) -> Result<Vec<u8>> {
        let endpoint = self
            .root
            .join(path.trim_start_matches('/'))
            .map_err(|_| error("invalid Fur Affinity path"))?;
        tokio::time::timeout(self.timeout, self.request(&endpoint, form.as_deref()))
            .await
            .map_err(|_| error("native Fur Affinity extraction timed out"))?
    }

    async fn submission(
        &self,
        id: &str,
        route: &'static Route,
        extra: &Map<String, Value>,
        user: Option<&str>,
    ) -> Result<(Vec<ExtractionEvent>, Vec<(Url, Map<String, Value>)>)> {
        let body = self.call(&format!("view/{id}/"), None).await?;
        let submission = parse_post(&body, id, self.layout, user)?;
        let external: Vec<(Url, Map<String, Value>)> = if self.external {
            let description = submission
                .fields
                .get("_description")
                .and_then(Value::as_str)
                .unwrap_or_default();
            extract_iter(description, "href=\"http", "\"")
                .into_iter()
                .filter_map(|rest| Url::parse(&format!("http{}", unescape(rest))).ok())
                .filter(plain)
                .map(|url| (url, submission.fields.clone()))
                .collect()
        } else {
            Vec::new()
        };
        let mut media = submission_media(submission, route, extra);
        self.network.apply_media(&mut media);
        let keywords = KeywordPolicy::shared(&["num", "index", "media_id"])
            .with_post_fields(&media, &["filename", "name", "extension"]);
        let events = group_with_keywords(vec![media], "furaffinity", keywords);
        Ok((events.into(), external))
    }

    /// One submission's events, then its queued `external` links. Listings
    /// skip submissions without a download link, as upstream does.
    fn submission_events<'a>(
        &'a self,
        id: String,
        route: &'static Route,
        extra: Map<String, Value>,
        user: Option<String>,
        queue: Queue<'a>,
        listing: bool,
    ) -> Events<'a> {
        stream::once(async move { self.submission(&id, route, &extra, user.as_deref()).await })
            .flat_map(move |result| match result {
                Ok((events, external)) => {
                    let mut streams: Vec<Events<'a>> =
                        vec![stream::iter(events.into_iter().map(Ok)).boxed()];
                    for (url, fields) in external {
                        if let Some(events) = queue(url) {
                            streams.push(child_job(
                                ChildMetadata::from_raw("furaffinity", route.subcategory, &fields),
                                move || events,
                            ));
                        }
                    }
                    stream::iter(streams).flatten().boxed()
                }
                Err(e) if listing && e.to_string() == NOT_AVAILABLE => stream::empty().boxed(),
                Err(e) => stream::once(async move { Err(e) }).boxed(),
            })
            .boxed()
    }

    /// Submission IDs (with an optional `favorite_id`) of the next page.
    async fn next_ids(&self, state: &mut ListState) -> Result<Vec<(String, Option<i64>)>> {
        state.fetched += 1;
        let ids: Vec<(String, Option<i64>)> = match &mut state.pages {
            Pages::Gallery { path } => {
                state.num += 1;
                let body = self.call(&format!("{path}/{}/", state.num), None).await?;
                let ids = parse_gallery(&body)?;
                state.done = ids.is_empty();
                ids.into_iter().map(|id| (id, None)).collect()
            }
            Pages::Favorites { next } => {
                let path = next.take().unwrap_or_default();
                let body = self.call(&path, None).await?;
                let (posts, following) = parse_favorites(&body)?;
                *next = site_path(&following, "/favorites/").filter(|p| *p != path);
                state.done = next.is_none();
                posts.into_iter().map(|(id, fav)| (id, Some(fav))).collect()
            }
            Pages::Search { data } => {
                let form = url::form_urlencoded::Serializer::new(String::new())
                    .extend_pairs(data.iter())
                    .finish();
                let body = self.call("search/", Some(form)).await?;
                let ids = parse_gallery(&body)?;
                state.done = ids.is_empty();
                // Upstream adds `next_page` once, then increments `page`.
                if data.iter().any(|(k, _)| k == "next_page") {
                    if let Some((_, page)) = data.iter_mut().find(|(k, _)| k == "page") {
                        *page = (parse_int(page, 1) + 1).to_string();
                    }
                } else {
                    data.push(("next_page".into(), "Next".into()));
                }
                ids.into_iter().map(|id| (id, None)).collect()
            }
            Pages::Submissions { next } => {
                let path = next.take().unwrap_or_default();
                let body = self.call(&path, None).await?;
                let page = html(&body)?;
                let ids = parse_gallery(&body)?;
                let pos = page
                    .find(">Next 48</a>")
                    .or_else(|| page.find(">&gt;&gt;&gt; Next 48 &gt;&gt;"));
                *next = pos
                    .map(|pos| unescape(rextr(page, "href=\"", "\"", Some(pos))))
                    .and_then(|p| site_path(&p, "/msg/submissions"))
                    .filter(|p| *p != path);
                state.done = next.is_none();
                ids.into_iter().map(|id| (id, None)).collect()
            }
        };
        let fresh: Vec<_> = ids
            .into_iter()
            .filter(|(id, _)| state.seen.insert(id.clone()))
            .collect();
        // A page without new IDs would repeat forever.
        if fresh.is_empty() || state.fetched >= MAX_PAGES {
            state.done = true;
        }
        Ok(fresh)
    }

    /// Lazily page a listing and extract every listed submission.
    fn listing<'a>(
        &'a self,
        pages: Pages,
        route: &'static Route,
        extra: Map<String, Value>,
        user: Option<String>,
        queue: Queue<'a>,
    ) -> Events<'a> {
        let state = ListState {
            pages,
            num: 0,
            fetched: 0,
            done: false,
            seen: HashSet::new(),
        };
        stream::unfold(Some(state), move |state| async move {
            let mut state = state?;
            if state.done {
                return None;
            }
            match self.next_ids(&mut state).await {
                Ok(ids) => Some((Ok(ids), Some(state))),
                Err(e) => Some((Err(e), None)),
            }
        })
        .flat_map(move |page| match page {
            Ok(ids) => {
                let (extra, user, queue) = (extra.clone(), user.clone(), queue.clone());
                stream::iter(ids)
                    .flat_map(move |(id, favorite)| {
                        let mut extra = extra.clone();
                        if let Some(favorite) = favorite {
                            extra.insert("favorite_id".into(), favorite.into());
                        }
                        self.submission_events(id, route, extra, user.clone(), queue.clone(), true)
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

    /// [`FurAffinity::events_with_queue`] without routing `external` links.
    pub fn events(&self, url: &Url) -> Events<'_> {
        self.events_with_queue(url, Arc::new(|_| None))
    }

    /// Emit one post boundary per submission; listings paginate lazily.
    /// Profiles and watch lists queue child jobs; `queue` routes the
    /// `external` option's description links to other extractors.
    pub fn events_with_queue<'a>(&'a self, url: &Url, queue: Queue<'a>) -> Events<'a> {
        let target = match target(url) {
            Ok(target) => target,
            Err(e) => return stream::once(async move { Err(e) }).boxed(),
        };
        match target {
            Target::Submission { id } => {
                self.submission_events(id, &POST, Map::new(), None, queue, false)
            }
            Target::Gallery { user, folder } => {
                let (path, route) = match folder {
                    Folder::Gallery => ("gallery", &GALLERY),
                    Folder::Scraps => ("scraps", &SCRAPS),
                };
                let pages = Pages::Gallery {
                    path: format!("{path}/{user}"),
                };
                self.listing(pages, route, Map::new(), Some(user), queue)
            }
            Target::Folder { user, id, name } => {
                let pages = Pages::Gallery {
                    path: format!("gallery/{user}/folder/{id}/a"),
                };
                let extra = Map::from_iter([
                    ("folder_id".into(), Value::from(id)),
                    ("folder_name".into(), Value::from(name)),
                ]);
                self.listing(pages, &FOLDER, extra, Some(user), queue)
            }
            Target::Favorites { user, start } => {
                let mut path = format!("/favorites/{user}/");
                if let Some(start) = start {
                    path.push_str(&start);
                }
                let pages = Pages::Favorites { next: Some(path) };
                self.listing(pages, &FAVORITE, Map::new(), Some(user), queue)
            }
            Target::Search { query } => {
                let mut data: Vec<(String, String)> = [
                    ("page", "1"),
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
                for (key, value) in &query {
                    let value = if key == "page" {
                        parse_int(value, 0).to_string()
                    } else {
                        value.clone()
                    };
                    match data.iter_mut().find(|(k, _)| k == key) {
                        Some((_, slot)) => *slot = value,
                        None => data.push((key.clone(), value)),
                    }
                }
                let search = query
                    .iter()
                    .find(|(k, _)| k == "q")
                    .map_or(Value::Null, |(_, q)| Value::from(q.as_str()));
                let extra = Map::from_iter([("search".into(), search)]);
                self.listing(Pages::Search { data }, &SEARCH, extra, None, queue)
            }
            Target::Submissions { path } => {
                let pages = Pages::Submissions { next: Some(path) };
                self.listing(pages, &SUBMISSIONS, Map::new(), None, queue)
            }
            Target::User { user } => self.user(user, queue),
            Target::Following { user } => self.following(user, queue),
        }
    }

    /// Upstream `FuraffinityUserExtractor`: queue the selected views.
    fn user<'a>(&'a self, user: String, queue: Queue<'a>) -> Events<'a> {
        let views = crate::dispatch::select(self.include.as_ref(), &["gallery"], USER_VIEWS, &[]);
        stream::iter(views)
            .flat_map(move |view| {
                let path = if view == "favorite" {
                    "favorites"
                } else {
                    view.as_str()
                };
                let url = Url::parse(&format!("https://www.furaffinity.net/{path}/{user}/"))
                    .expect("validated Fur Affinity user");
                let queue = queue.clone();
                child_job(
                    ChildMetadata::from_raw("furaffinity", "user", &Map::new()),
                    move || self.events_with_queue(&url, queue),
                )
            })
            .boxed()
    }

    /// Upstream `FuraffinityFollowingExtractor`: queue every watched user's
    /// profile, following the watch list's form pagination.
    fn following<'a>(&'a self, user: String, queue: Queue<'a>) -> Events<'a> {
        struct State {
            path: Option<String>,
            pages: u64,
            seen: HashSet<String>,
        }
        let state = State {
            path: Some(format!("/watchlist/by/{user}/")),
            pages: 0,
            seen: HashSet::new(),
        };
        stream::unfold(state, move |mut state| {
            let queue = queue.clone();
            async move {
                let path = state.path.take()?;
                state.pages += 1;
                let body = match self.call(&path, None).await {
                    Ok(body) => body,
                    Err(e) => return Some((stream::once(async move { Err(e) }).boxed(), state)),
                };
                let Ok(page) = html(&body) else {
                    let e = error("Fur Affinity returned invalid HTML");
                    return Some((stream::once(async move { Err(e) }).boxed(), state));
                };
                let mut children: Vec<Events<'a>> = Vec::new();
                for href in extract_iter(page, "<a href=\"", "\"") {
                    let Ok(url) = Url::parse(&format!("https://www.furaffinity.net{href}")) else {
                        continue;
                    };
                    let Ok(Target::User { user }) = target(&url) else {
                        continue;
                    };
                    if !state.seen.insert(user) {
                        continue;
                    }
                    let queue = queue.clone();
                    children.push(child_job(
                        ChildMetadata::from_raw("furaffinity", "following", &Map::new()),
                        move || self.events_with_queue(&url, queue),
                    ));
                }
                let next = rextr(page, "action=\"", "\"", None);
                state.path = (!next.is_empty() && !path.ends_with(next) && state.pages < MAX_PAGES)
                    .then(|| site_path(next, "/watchlist/by/"))
                    .flatten();
                Some((stream::iter(children).flatten().boxed(), state))
            }
        })
        .flatten()
        .boxed()
    }

    async fn request(&self, endpoint: &Url, form: Option<&str>) -> Result<Vec<u8>> {
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
        form: Option<&str>,
        attempt: u8,
    ) -> std::result::Result<Vec<u8>, Failure> {
        let mut target = endpoint.clone();
        let mut redirects = Redirects::new(endpoint);
        let mut form = form;
        let mut response = loop {
            self.rate_limits.wait(&target, self.timeout).await?;
            let mut request = match form {
                Some(body) => self
                    .client
                    .post(target.clone())
                    .header("Content-Type", "application/x-www-form-urlencoded")
                    .body(body.to_owned()),
                None => self.client.get(target.clone()),
            }
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
                Some(next) => {
                    // Redirected form submissions continue as GET requests.
                    target = next;
                    form = None;
                }
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
    use serde_json::json;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        path::PathBuf,
        thread::{self, JoinHandle},
    };

    const DESCRIPTION_END: &str = "\n                                    </div>";

    fn submission_html() -> String {
        format!(
            r#"<html><head><meta charset="utf-8"><meta property="og:title" content="OG"></head><body>
<a class="button" href="//d.furaffinity.net/art/alice/1695300000/1695300000.alice_big.png">Download</a>
<section><h3>Keywords</h3><a href="/search/@keywords cat">cat</a> <a href="/search/@keywords dog">dog &amp; co</a></section>
<a href="/gallery/alice/" class="button submissions">Main Gallery</a>
<div class="submission-title"><h2><p>Big &amp; Bold</p></h2></div>
<a href="/user/alice/" title="alice">Alice</a>
<div class="submission-description user-submitted-links">Hello <b>there</b> &amp; <a href="https://example.com/more">more</a>{DESCRIPTION_END}
<div class="views"><span class="font-large">12</span></div>
<div class="favorites"><span class="font-large">3</span></div>
<div class="comments"><span class="font-large">4</span></div>
<div class="rating"><span class="rating-box inline mature">Mature</span></div>
<div><strong>Category</strong> <span class="category-name">Artwork (Digital)</span> / <span class="type-name">Fantasy</span></div>
<div><strong>Species</strong> <span>Unspecified / Any</span></div>
<div><strong>Gender</strong> <span>Any</span></div>
<div><strong>Size</strong> <span>1280 x 720p</span></div>
<section><h3>Listed in Folders</h3><a href="/gallery/alice/folder/1/Sketches/">Sketches</a><a href="/x">Comics</a></section>
</body></html>"#
        )
    }

    fn old_html() -> String {
        r#"<html><head><meta http-equiv="Content-Type" content="text/html"></head><body>
<a href="//d.furaffinity.net/art/bob/1600000000/1600000000.bob_old.jpg">Download</a>
<div class="minigallery-title"><a href="/scraps/bob/">More</a></div>
<h2>Old &amp; Gold</h2> <a href="/user/bob/" title="bob">Bob</a>
<b>Category:</b> Artwork<br><b>Theme:</b> General<br><b>Species:</b> Fox<br><b>Gender:</b> Male<br>
<b>Favorites:</b> 5<br><b>Comments:</b> 6<br><b>Views:</b> 70<br>
<b>Resolution:</b> 800x600<br>
<div id="keywords"><a>red</a> <a>blue</a></div>
<img alt="Adult rating" src="x">
<td valign="top" align="left" width="70%" class="alt1" style="padding:8px">Old <i>text</i>
                               </td>
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
    fn parses_submission_and_gallery() {
        let items = parse_submission(submission_html().as_bytes(), "42").unwrap();
        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(item.filename, "1695300000.alice_big.png");
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
        assert_eq!((meta.timestamp, meta.index), (Some(1695300000), 1));
        let file = meta.file.as_ref().unwrap();
        assert_eq!(file.sensitive_flags[0].as_ref(), "mature");
        assert_eq!((file.width, file.height), (Some(1280), Some(720)));
        let k = keywords(item);
        assert_eq!(k["id"], 42);
        assert_eq!(k["tags"], json!(["cat", "dog & co"]));
        assert_eq!(k["scraps"], false);
        assert_eq!(k["title"], "Big & Bold");
        assert_eq!(
            (k["artist"].clone(), k["artist_url"].clone()),
            (json!("Alice"), json!("alice"))
        );
        assert_eq!(k["user"], "alice");
        assert_eq!(k["description"], "Hello there & more");
        assert_eq!(
            (
                k["views"].clone(),
                k["favorites"].clone(),
                k["comments"].clone()
            ),
            (json!(12), json!(3), json!(4))
        );
        assert_eq!(k["rating"], "Mature");
        assert_eq!(k["fa_category"], "Artwork (Digital)");
        assert_eq!(k["theme"], "Fantasy");
        assert_eq!(k["species"], "Unspecified / Any");
        assert_eq!(k["gender"], "Any");
        assert_eq!(k["folders"], json!(["Sketches", "Comics"]));
        assert_eq!(k["date"], "2023-09-21 12:40:00");
        assert_eq!(
            k["thumbnail"],
            "https://t.furaffinity.net/42@600-1695300000.jpg"
        );
        assert_eq!(
            (k["category"].clone(), k["subcategory"].clone()),
            (json!("furaffinity"), json!("post"))
        );
        let (file, dir) = paths(item);
        assert_eq!(dir, ["furaffinity", "alice"].iter().collect::<PathBuf>());
        assert_eq!(file, "42 Big & Bold.png");
        assert!(item.archive_key().unwrap().starts_with("furaffinity:"));
        assert!(parse_submission(b"<html>System Message</html>", "1").is_err());
        let evil = r#"<a href="//d.furaffinity.net.evil.example/art/a.png">Download</a>"#;
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
    fn parses_old_layout_and_html_descriptions() {
        let layout = Layout {
            new: None,
            html_descriptions: true,
        };
        let post = parse_post(old_html().as_bytes(), "7", layout, Some("bob")).unwrap();
        let k = &post.fields;
        assert_eq!(k["title"], "Old & Gold");
        assert_eq!(k["scraps"], true);
        assert_eq!(k["artist"], "Bob");
        assert_eq!(
            (
                k["fa_category"].clone(),
                k["theme"].clone(),
                k["species"].clone(),
                k["gender"].clone()
            ),
            (
                json!("Artwork"),
                json!("General"),
                json!("Fox"),
                json!("Male")
            )
        );
        assert_eq!(
            (
                k["favorites"].clone(),
                k["comments"].clone(),
                k["views"].clone()
            ),
            (json!(5), json!(6), json!(70))
        );
        assert_eq!(
            (k["width"].clone(), k["height"].clone()),
            (json!(800), json!(600))
        );
        assert_eq!(k["tags"], json!(["red"]));
        assert_eq!(k["rating"], "Adult");
        assert_eq!(k["description"], "Old <i>text</i>");
        assert_eq!(k["folders"], json!([]));
        let current = r#"<html><head><meta charset="utf-8"><link href="//d.furaffinity.net/media/static/x.css" rel="stylesheet"></head><body>
<a href="//d.furaffinity.net/art/carol/1609906863/1609906714.carol_cat.png">Full</a>
<div class="submission-title"><h2>Cat &amp; Co</h2></div>
<span><a href="/user/carol/"><span class="c-usernameBlockSimple__displayName" title=" carol ">Carol</span></a></span>
<div class="submission-description-text user-submitted-links">teefs<br />more
<div class="submission-footer"><hr /></div></div>
<div class="submission-page-stats"><div title="Views"><div>5413</div><div class="highlight">Views</div></div>
<div title="Comments"><div>79</div></div><div title="Favorites"><div> 174 </div></div>
<div><div class="font-large inline c-contentRating--general">General</div><div class="highlight">Rating</div></div></div>
<div class="submission-content-stats"><span class="highlight"><span>Category</span><span>Theme</span><span>Species</span><span>Resolution</span></span>
<span><span>All</span><span>Fantasy</span><span>Cat</span><span>600 x 400</span></span></div>
<div class="submission-tags"><a data-tag-name="cat" class="tag-block"></a><a data-tag-name="cute &amp; fluffy"></a></div>
<div class="submission-folder"><a href="/gallery/carol/folder/1/A/"><strong>Main</strong> - <span>Sketches</span></a></div>
</body></html>"#;
        let post = parse_post(current.as_bytes(), "9", Layout::default(), None).unwrap();
        let k = &post.fields;
        assert_eq!(
            post.url.as_str(),
            "https://d.furaffinity.net/art/carol/1609906863/1609906714.carol_cat.png"
        );
        assert_eq!(k["title"], "Cat & Co");
        assert_eq!(
            (
                k["artist"].clone(),
                k["artist_url"].clone(),
                k["user"].clone()
            ),
            (json!("Carol"), json!("carol"), json!("carol"))
        );
        assert_eq!(k["description"], "teefsmore");
        assert_eq!(
            (
                k["views"].clone(),
                k["comments"].clone(),
                k["favorites"].clone()
            ),
            (json!(5413), json!(79), json!(174))
        );
        assert_eq!(k["rating"], "General");
        assert_eq!(
            (
                k["fa_category"].clone(),
                k["theme"].clone(),
                k["species"].clone(),
                k["gender"].clone()
            ),
            (json!("All"), json!("Fantasy"), json!("Cat"), json!(""))
        );
        assert_eq!(
            (k["width"].clone(), k["height"].clone()),
            (json!(600), json!(400))
        );
        assert_eq!(k["tags"], json!(["cat", "cute & fluffy"]));
        assert_eq!(k["folders"], json!(["Main - Sketches"]));
        // A forced new layout does not find the old page's fields.
        let forced = Layout {
            new: Some(true),
            html_descriptions: false,
        };
        let post = parse_post(old_html().as_bytes(), "7", forced, None).unwrap();
        assert_eq!(post.fields["title"], "");

        let (favorites, next) = parse_favorites(
            br#"<figure id="sid-9" data-fav-id="900"></figure><figure id="sid-8" data-fav-id="800"></figure>
<form action="/favorites/alice/800/next"><button type="submit">Next</button></form>"#,
        )
        .unwrap();
        assert_eq!(favorites, [("9".into(), 900), ("8".into(), 800)]);
        assert_eq!(next, "/favorites/alice/800/next");
        assert_eq!(rextr("a=1 a=2 a=3", "a=", " ", Some(5)), "1");
    }

    #[test]
    fn classifies_urls_and_hosts() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.furaffinity.net/view/42/").unwrap(),
            Target::Submission { id: "42".into() }
        );
        assert_eq!(
            t("https://fxfuraffinity.net/full/42").unwrap(),
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
            t("https://www.furaffinity.net/gallery/alice/folder/123/My-Folder").unwrap(),
            Target::Folder {
                user: "alice".into(),
                id: "123".into(),
                name: "My-Folder".into()
            }
        );
        assert_eq!(
            t("https://www.furaffinity.net/favorites/alice/12/next").unwrap(),
            Target::Favorites {
                user: "alice".into(),
                start: Some("12/next".into())
            }
        );
        assert_eq!(
            t("https://www.furaffinity.net/search/cute%20fox/?order-by=date").unwrap(),
            Target::Search {
                query: vec![
                    ("order-by".into(), "date".into()),
                    ("q".into(), "cute fox".into())
                ]
            }
        );
        assert_eq!(
            t("https://www.furaffinity.net/msg/submissions/new~1@48/").unwrap(),
            Target::Submissions {
                path: "/msg/submissions/new~1@48".into()
            }
        );
        assert_eq!(
            t("https://sfw.furaffinity.net/user/alice/").unwrap(),
            Target::User {
                user: "alice".into()
            }
        );
        assert_eq!(
            t("https://www.furaffinity.net/watchlist/by/alice/").unwrap(),
            Target::Following {
                user: "alice".into()
            }
        );
        for raw in [
            "https://www.furaffinity.net/",
            "https://www.furaffinity.net/view/abc/",
            "https://www.furaffinity.net/gallery/alice/folder/x/",
            "https://www.furaffinity.net/search/",
            "https://d.furaffinity.net/art/a/1/1.a.png",
            "https://www.furaffinity.net.evil.example/view/1/",
            "https://evilfuraffinity.net/view/1/",
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
                let mut buffer = [0; 4096];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    let count = stream.read(&mut buffer).unwrap();
                    assert_ne!(count, 0);
                    request.extend_from_slice(&buffer[..count]);
                }
                let text = String::from_utf8_lossy(&request).into_owned();
                let length = text
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                let head = request.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
                while request.len() < head + length {
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

    async fn collect(native: &FurAffinity, raw: &str) -> Vec<Media> {
        native
            .stream(&Url::parse(raw).unwrap())
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .map(|m| m.unwrap())
            .collect()
    }

    #[tokio::test]
    async fn fetches_submission_with_retry_and_cookies() {
        let (root, server) = serve(vec![
            response("503 Service Unavailable", b""),
            ok(&submission_html()),
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
            ok(r#"<figure id="sid-42"></figure><figure id="sid-7"></figure>"#),
            ok(&submission_html()),
            ok("<html>System Message</html>"),
            ok(r#"<figure id="sid-42"></figure>"#),
        ]);
        let media = collect(
            &furaffinity(root, CookieJar::default()),
            "https://www.furaffinity.net/gallery/alice/",
        )
        .await;
        assert_eq!(media.len(), 1);
        assert_eq!(keywords(&media[0])["subcategory"], "gallery");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /gallery/alice/1/ "));
        assert!(!requests[0].to_ascii_lowercase().contains("cookie:"));
        assert!(requests[2].starts_with("GET /view/7/ "));
        assert!(requests[3].starts_with("GET /gallery/alice/2/ "));
    }

    #[tokio::test]
    async fn folders_and_favorites_add_class_metadata() {
        let (root, server) = serve(vec![
            ok(r#"<figure id="sid-42"></figure>"#),
            ok(&submission_html()),
            ok(""),
        ]);
        let media = collect(
            &furaffinity(root, CookieJar::default()),
            "https://www.furaffinity.net/gallery/alice/folder/5/Sketches",
        )
        .await;
        let k = keywords(&media[0]);
        assert_eq!(
            (k["folder_id"].clone(), k["folder_name"].clone()),
            (json!("5"), json!("Sketches"))
        );
        assert_eq!(
            paths(&media[0]).1,
            ["furaffinity", "alice", "Folders", "5 Sketches"]
                .iter()
                .collect::<PathBuf>()
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /gallery/alice/folder/5/a/1/ "));
        assert!(requests[2].starts_with("GET /gallery/alice/folder/5/a/2/ "));

        let (root, server) = serve(vec![
            ok(
                r#"<figure id="sid-42" data-fav-id="900"></figure><a class="button right" href="/favorites/alice/900/next">Next</a>"#,
            ),
            ok(&submission_html()),
            ok(r#"<figure id="sid-42" data-fav-id="900"></figure>"#),
        ]);
        let media = collect(
            &furaffinity(root, CookieJar::default()),
            "https://www.furaffinity.net/favorites/Alice/",
        )
        .await;
        let k = keywords(&media[0]);
        assert_eq!(
            (k["favorite_id"].clone(), k["user"].clone()),
            (json!(900), json!("alice"))
        );
        assert_eq!(
            paths(&media[0]).1,
            ["furaffinity", "alice", "Favorites"]
                .iter()
                .collect::<PathBuf>()
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /favorites/alice/ "));
        assert!(requests[2].starts_with("GET /favorites/alice/900/next "));
    }

    #[tokio::test]
    async fn posts_search_form_and_follows_next_page() {
        let (root, server) = serve(vec![
            ok(r#"<figure id="sid-42"></figure>"#),
            ok(&submission_html()),
            ok(""),
        ]);
        let media = collect(
            &furaffinity(root, CookieJar::default()),
            "https://www.furaffinity.net/search/?q=fox&rating-adult=0",
        )
        .await;
        let k = keywords(&media[0]);
        assert_eq!(k["search"], "fox");
        assert_eq!(k["user"], "alice");
        assert_eq!(
            paths(&media[0]).1,
            ["furaffinity", "Search", "fox"].iter().collect::<PathBuf>()
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("POST /search/ "), "{}", requests[0]);
        assert!(requests[0].contains("page=1&order-by=relevancy"));
        assert!(requests[0].contains("rating-adult=0"));
        assert!(requests[0].contains("&q=fox"));
        assert!(!requests[0].contains("next_page"));
        assert!(requests[2].starts_with("POST /search/ "));
        assert!(requests[2].contains("page=1&") && requests[2].contains("next_page=Next"));
    }

    #[tokio::test]
    async fn queues_external_links_profiles_and_watched_users() {
        let (root, server) = serve(vec![ok(&submission_html())]);
        let options = Map::from_iter([("external".into(), json!(true))]);
        let native = furaffinity(root, CookieJar::default())
            .with_options(Some(&options))
            .unwrap();
        let queued = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = queued.clone();
        let queue: Queue<'_> = Arc::new(move |url: Url| {
            seen.lock().unwrap().push(url.to_string());
            Some(stream::empty().boxed())
        });
        let events: Vec<_> = native
            .events_with_queue(
                &Url::parse("https://www.furaffinity.net/view/42/").unwrap(),
                queue,
            )
            .collect()
            .await;
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Ok(ExtractionEvent::BeginChild(_))))
        );
        assert_eq!(*queued.lock().unwrap(), ["https://example.com/more"]);
        server.join().unwrap();

        let (root, server) = serve(vec![
            ok(
                r#"<a href="/user/bob/">Bob</a><a href="/view/1/">x</a><a href="/user/carol/">Carol</a>
<form action="/watchlist/by/alice/2/"></form>"#,
            ),
            ok(r#"<a href="/user/bob/">Bob</a><form action="/watchlist/by/alice/2/"></form>"#),
        ]);
        let events: Vec<_> = furaffinity(root, CookieJar::default())
            .with_include(Some(crate::dispatch::Include::Views(vec![])))
            .events(&Url::parse("https://www.furaffinity.net/watchlist/by/alice/").unwrap())
            .collect()
            .await;
        let children = events
            .iter()
            .filter(|e| matches!(e, Ok(ExtractionEvent::BeginChild(_))))
            .count();
        assert_eq!(children, 2);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /watchlist/by/alice/ "));
        assert!(requests[1].starts_with("GET /watchlist/by/alice/2/ "));

        let (root, server) = serve(vec![ok(""), ok("")]);
        let media = collect(
            &furaffinity(root, CookieJar::default()).with_include(Some(
                crate::dispatch::Include::Views(vec!["scraps".into(), "favorite".into()]),
            )),
            "https://www.furaffinity.net/user/alice/",
        )
        .await;
        assert!(media.is_empty());
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /scraps/alice/1/ "));
        assert!(requests[1].starts_with("GET /favorites/alice/ "));
        let bad = Map::from_iter([("layout".into(), json!(1))]);
        assert!(
            FurAffinity::new(Duration::from_secs(1))
                .unwrap()
                .with_options(Some(&bad))
                .is_err()
        );
    }
}
