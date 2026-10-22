//! Booru-style image boards outside the dedicated `crate::booru` module.
//!
//! Shared helpers mirror gallery_dl/text.py, gallery_dl/dt.py and the
//! `BooruExtractor.items` loop in gallery_dl/extractor/booru.py.
use crate::event::ExtractionEvent;
use crate::sites::{Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

mod moebooru;
mod philomena;
mod twibooru;

pub(super) static SITES: &[&Site] = &[
    &moebooru::YANDERE,
    &moebooru::KONACHAN,
    &moebooru::SAKUGABOORU,
    &moebooru::LOLIBOORU,
    &philomena::DERPIBOORU,
    &philomena::PONYBOORU,
    &philomena::FURBOORU,
    &twibooru::SITE,
];

// ---------------------------------------------------------------------------
// URL matching

/// The URL's path and query (`/path?query`) below one of `roots`, where each
/// root is an exact `host` or `host/prefix` (no implicit `www.`). Rejects
/// ports, credentials and non-HTTP schemes. The returned text keeps the
/// URL's percent-encoding, like the upstream regexes see it.
pub(crate) fn below(url: &Url, roots: &[&str]) -> Option<String> {
    if !crate::sites::plain(url) {
        return None;
    }
    let host = url.host_str()?;
    let mut rest = url.path().to_owned();
    if let Some(query) = url.query() {
        rest.push('?');
        rest.push_str(query);
    }
    for root in roots {
        let (root_host, prefix) = match root.split_once('/') {
            Some((h, p)) => (h, format!("/{p}")),
            None => (*root, String::new()),
        };
        if !host.eq_ignore_ascii_case(root_host) {
            continue;
        }
        if prefix.is_empty() {
            return Some(rest);
        }
        if let Some(tail) = rest.strip_prefix(&prefix)
            && (tail.is_empty() || tail.starts_with(['/', '?']))
        {
            return Some(tail.to_owned());
        }
    }
    None
}

/// Leading ASCII digits of `text` (at least one, at most 20).
pub(crate) fn leading_digits(text: &str) -> Option<&str> {
    let end = text
        .bytes()
        .position(|b| !b.is_ascii_digit())
        .unwrap_or(text.len());
    (1..=20).contains(&end).then(|| &text[..end])
}

/// The text up to the first of `stops` (or all of it).
pub(crate) fn until<'t>(text: &'t str, stops: &[char]) -> &'t str {
    text.find(stops).map_or(text, |end| &text[..end])
}

// ---------------------------------------------------------------------------
// gallery_dl/text.py

/// Python `urllib.parse.unquote` (`+` is kept).
pub(crate) fn unquote(text: &str) -> String {
    crate::sites::percent_decode(text)
}

/// `text.unquote(value.replace("+", " "))`.
pub(crate) fn unquote_plus(text: &str) -> String {
    unquote(&text.replace('+', " "))
}

/// `text.parse_query`: name/value pairs in order; the first value of a
/// repeated name wins. Without `empty`, pairs lacking `=` are ignored.
pub(crate) fn parse_query(query: &str, empty: bool) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    if query.is_empty() {
        return out;
    }
    for pair in query.split('&') {
        let (name, value, eq) = match pair.split_once('=') {
            Some((n, v)) => (n, v, true),
            None => (pair, "", false),
        };
        if eq || empty {
            let name = unquote_plus(name);
            if !out.iter().any(|(n, _)| *n == name) {
                out.push((name, unquote_plus(value)));
            }
        }
    }
    out
}

/// Look up a name in [`parse_query`] output.
pub(crate) fn query_get<'q>(query: &'q [(String, String)], name: &str) -> Option<&'q str> {
    query
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
}

/// `str.rpartition(sep)`: (head, tail); without `sep`, ("", text).
pub(crate) fn rpartition<'t>(text: &'t str, sep: &str) -> (&'t str, &'t str) {
    text.rsplit_once(sep).unwrap_or(("", text))
}

// ---------------------------------------------------------------------------
// gallery_dl/dt.py

/// `dt.parse_iso` as Unix seconds; `None` stands for the invalid datetime.
pub(crate) fn iso_timestamp(text: &str) -> Option<i64> {
    let text = text.trim();
    if text.len() < 10 || text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    crate::date::parse_date(text).ok()
}

/// The `date` keyword for Unix seconds (`None` is the invalid datetime).
pub(crate) fn date_value(timestamp: Option<i64>) -> Value {
    crate::filter::source_date_text(timestamp).into()
}

// ---------------------------------------------------------------------------
// BooruExtractor.items

/// One post boundary per item, as booru extractors yield one Directory per
/// post.
pub(crate) fn per_post(
    site: &Site,
    http: &Http,
    route: &'static Route,
    items: Vec<Item>,
) -> Vec<ExtractionEvent> {
    let mut events = Vec::new();
    for item in items {
        events.extend(crate::sites::post(site, http, route, vec![item]));
    }
    events
}

/// The file URL for a post: `root`-relative paths are joined to `root`.
/// Only plain HTTP(S) URLs are accepted.
pub(crate) fn file_url(root: &str, raw: &str) -> Option<Url> {
    let raw = raw.trim();
    let raw = if raw.starts_with("//") {
        format!("https:{raw}")
    } else if raw.starts_with('/') {
        format!("{root}{raw}")
    } else {
        raw.to_owned()
    };
    Url::parse(&raw).ok().filter(crate::sites::plain)
}

/// `BooruExtractor.items` bookkeeping for one post: fill `filename` and
/// `extension` from `url` unless `extension` exists, then merge `data`.
pub(crate) fn booru_fields(url: &Url, fields: &mut Map<String, Value>, data: &Map<String, Value>) {
    if !fields.contains_key("extension") {
        let (name, ext) = crate::sites::nameext(url);
        fields.insert("filename".into(), name.into());
        fields.insert("extension".into(), ext.into());
    }
    for (key, value) in data {
        fields.insert(key.clone(), value.clone());
    }
}

/// An [`Item`] whose ids, user and caption come from the given keyword names.
pub(crate) fn item(
    url: Url,
    fields: Map<String, Value>,
    user_key: &str,
    caption_key: &str,
    timestamp: Option<i64>,
) -> Item {
    let id = fields
        .get("id")
        .map(crate::sites::scalar)
        .unwrap_or_default();
    let text = |key: &str| match fields.get(key) {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Array(items)) => Some(
            items
                .iter()
                .map(crate::sites::scalar)
                .collect::<Vec<_>>()
                .join(" "),
        ),
        _ => None,
    };
    let user = text(user_key);
    let caption = text(caption_key);
    Item::new(url, fields)
        .ids(id.clone(), id)
        .user(user)
        .caption(caption)
        .timestamp(timestamp)
}
