//! Booru-style image boards outside the dedicated `crate::booru` module.
//!
//! Shared helpers mirror gallery_dl/text.py, gallery_dl/dt.py and the
//! `BooruExtractor.items` loop in gallery_dl/extractor/booru.py.
use crate::event::ExtractionEvent;
use crate::sites::{Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

mod agnph;
mod comicvine;
mod furry34;
mod gelbooru_v01;
mod gelbooru_v02;
mod moebooru;
mod nozomi;
mod paheal;
mod philomena;
mod realbooru;
mod rule34us;
mod rule34vault;
mod rule34xyz;
mod sankaku;
mod scatbooru;
mod shimmie2;
mod sizebooru;
mod szurubooru;
mod threedbooru;
mod twibooru;
mod yiffverse;
mod zerochan;

pub(super) static SITES: &[&Site] = &[
    &moebooru::YANDERE,
    &moebooru::KONACHAN,
    &moebooru::SAKUGABOORU,
    &moebooru::LOLIBOORU,
    &philomena::DERPIBOORU,
    &philomena::PONYBOORU,
    &philomena::FURBOORU,
    &twibooru::SITE,
    &shimmie2::CAVEMANON,
    &shimmie2::RULE34HENTAI,
    &shimmie2::VIDYAPICS,
    &shimmie2::NOZRIP,
    &shimmie2::THECOLLECTIONS,
    &shimmie2::SOYBOORU,
    &paheal::SITE,
    &realbooru::SITE,
    &rule34us::SITE,
    &zerochan::ZEROCHAN,
    &agnph::AGNPH,
    &threedbooru::THREEDBOORU,
    &szurubooru::BCBNSFW,
    &szurubooru::SNOOTBOORU,
    &szurubooru::VISUABUSTERS,
    &gelbooru_v02::RULE34,
    &gelbooru_v02::TBIB,
    &gelbooru_v02::HYPNOHUB,
    &gelbooru_v02::XBOORU,
    &gelbooru_v01::THECOLLECTION,
    &gelbooru_v01::ILLUSIONCARDSBOORU,
    &gelbooru_v01::ALLGIRLBOORU,
    &gelbooru_v01::DRAWFRIENDS,
    &gelbooru_v01::VIDYART2,
    &rule34xyz::RULE34XYZ,
    &rule34xyz::RULE34WORLD,
    &rule34vault::SITE,
    &furry34::SITE,
    &yiffverse::SITE,
    &comicvine::SITE,
    &scatbooru::SITE,
    &sizebooru::SITE,
    &nozomi::SITE,
    &sankaku::SITE,
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

/// `text.parse_int`: an integer, or 0 for empty/invalid input.
pub(crate) fn parse_int(text: &str) -> i64 {
    text.trim().replace('_', "").parse().unwrap_or(0)
}

/// `text.parse_int` as a JSON value (Python `int`).
pub(crate) fn int_value(text: &str) -> Value {
    parse_int(text).into()
}

/// `text.extract_iter`: every substring between `begin` and the next `end`.
pub(crate) fn extract_iter<'t>(text: &'t str, begin: &str, end: &str) -> Vec<&'t str> {
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(start) = text[pos..].find(begin) {
        let first = pos + start + begin.len();
        let Some(stop) = text[first..].find(end) else {
            break;
        };
        out.push(&text[first..first + stop]);
        pos = first + stop + end.len();
    }
    out
}

/// `text.extract_from`: a cursor returning the text between `begin` and the
/// next `end` after the previous match, or `""` without moving.
pub(crate) struct Extr<'t> {
    text: &'t str,
    pub pos: usize,
}

impl<'t> Extr<'t> {
    pub fn new(text: &'t str) -> Self {
        Self { text, pos: 0 }
    }

    pub fn next(&mut self, begin: &str, end: &str) -> &'t str {
        let Some(start) = self.text[self.pos..].find(begin) else {
            return "";
        };
        let first = self.pos + start + begin.len();
        let Some(stop) = self.text[first..].find(end) else {
            return "";
        };
        self.pos = first + stop + end.len();
        &self.text[first..first + stop]
    }
}

/// `str.rpartition(sep)`: (head, tail); without `sep`, ("", text).
pub(crate) fn rpartition<'t>(text: &'t str, sep: &str) -> (&'t str, &'t str) {
    text.rsplit_once(sep).unwrap_or(("", text))
}

/// `str.partition(sep)[0]`.
pub(crate) fn before<'t>(text: &'t str, sep: &str) -> &'t str {
    text.split_once(sep).map_or(text, |(head, _)| head)
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

/// Python `urllib.parse.quote` with its default `safe="/"`.
pub(crate) fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-' | b'~' | b'/') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// `text.parse_float`: a float, or 0.0 for empty/invalid input.
pub(crate) fn parse_float(text: &str) -> f64 {
    text.trim()
        .parse::<f64>()
        .ok()
        .filter(|f| f.is_finite())
        .unwrap_or(0.0)
}

/// A float as a JSON value, keeping Python's `1.0` spelling.
pub(crate) fn float_value(value: f64) -> Value {
    serde_json::Number::from_f64(value).map_or(Value::Null, Value::Number)
}

/// `text.parse_bytes`: "500k", "2.5M" or a plain count, as bytes.
pub(crate) fn parse_bytes(text: &str) -> i64 {
    let value = text.trim();
    let Some(last) = value.chars().last() else {
        return 0;
    };
    let (number, mul) = match "bkmgtp".find(last.to_ascii_lowercase()) {
        Some(power) => (
            &value[..value.len() - last.len_utf8()],
            1024f64.powi(power as i32),
        ),
        None => (value, 1.0),
    };
    match number.trim().parse::<f64>() {
        Ok(n) if n.is_finite() => (n * mul).round_ties_even() as i64,
        _ => 0,
    }
}

/// `text.split_html`: unescaped, stripped text between HTML tags.
pub(crate) fn split_html(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut rest = text;
    loop {
        let (chunk, next) = match rest.find('<') {
            Some(start) => match rest[start + 1..].find('>') {
                Some(end) if end > 0 => (&rest[..start], Some(&rest[start + end + 2..])),
                _ => (rest, None),
            },
            None => (rest, None),
        };
        if !chunk.trim().is_empty() {
            parts.push(crate::sites::unescape(chunk).trim().to_owned());
        }
        match next {
            Some(next) => rest = next,
            None => break,
        }
    }
    parts
}

/// directives; naive values are UTC. The whole input must match, and a
/// space in `format` matches one or more whitespace characters.
pub(crate) fn strptime(text: &str, format: &str) -> Option<i64> {
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
    const DAYS: [&str; 7] = [
        "monday",
        "tuesday",
        "wednesday",
        "thursday",
        "friday",
        "saturday",
        "sunday",
    ];
    fn number(input: &mut &str, max: usize) -> Option<i64> {
        let end = input
            .bytes()
            .take(max)
            .position(|b| !b.is_ascii_digit())
            .unwrap_or(input.len().min(max));
        if end == 0 {
            return None;
        }
        let value = input[..end].parse().ok()?;
        *input = &input[end..];
        Some(value)
    }
    fn name(input: &mut &str, names: &[&str], full: bool) -> Option<usize> {
        let lower = input.to_ascii_lowercase();
        let (index, len) = names.iter().enumerate().find_map(|(i, n)| {
            let candidate = if full { n } else { &n[..3] };
            lower.starts_with(candidate).then_some((i, candidate.len()))
        })?;
        *input = &input[len..];
        Some(index)
    }
    let (mut year, mut month, mut day) = (1900i64, 1i64, 1i64);
    let (mut hour, mut minute, mut second) = (0i64, 0i64, 0i64);
    let mut offset = 0i64;
    let mut pm = None;
    let mut input = text;
    let mut chars = format.chars();
    while let Some(c) = chars.next() {
        if c == '%' {
            match chars.next()? {
                'Y' => year = number(&mut input, 4)?,
                'm' => month = number(&mut input, 2)?,
                'd' => day = number(&mut input, 2)?,
                'H' => hour = number(&mut input, 2)?,
                'I' => hour = number(&mut input, 2)?,
                'M' => minute = number(&mut input, 2)?,
                'S' => second = number(&mut input, 2)?,
                'b' => month = name(&mut input, &MONTHS, false)? as i64 + 1,
                'B' => month = name(&mut input, &MONTHS, true)? as i64 + 1,
                'a' => {
                    name(&mut input, &DAYS, false)?;
                }
                'A' => {
                    name(&mut input, &DAYS, true)?;
                }
                'p' => {
                    let lower = input.get(..2)?.to_ascii_lowercase();
                    pm = Some(match lower.as_str() {
                        "am" => false,
                        "pm" => true,
                        _ => return None,
                    });
                    input = &input[2..];
                }
                'z' => {
                    if let Some(rest) = input.strip_prefix('Z') {
                        input = rest;
                    } else {
                        let sign = match input.as_bytes().first()? {
                            b'+' => 1,
                            b'-' => -1,
                            _ => return None,
                        };
                        input = &input[1..];
                        let hours = input.get(..2)?.parse::<i64>().ok()?;
                        input = &input[2..];
                        input = input.strip_prefix(':').unwrap_or(input);
                        let minutes = input.get(..2)?.parse::<i64>().ok()?;
                        input = &input[2..];
                        offset = sign * (hours * 3600 + minutes * 60);
                    }
                }
                '%' => input = input.strip_prefix('%')?,
                _ => return None,
            }
        } else if c.is_whitespace() {
            let trimmed = input.trim_start();
            if trimmed.len() == input.len() {
                return None;
            }
            input = trimmed;
        } else {
            input = input.strip_prefix(c)?;
        }
    }
    if !input.is_empty() {
        return None;
    }
    if let Some(pm) = pm {
        if !(1..=12).contains(&hour) {
            return None;
        }
        hour = hour % 12 + if pm { 12 } else { 0 };
    }
    let date = time::Date::from_calendar_date(
        i32::try_from(year).ok()?,
        time::Month::try_from(u8::try_from(month).ok()?).ok()?,
        u8::try_from(day).ok()?,
    )
    .ok()?;
    let clock = time::Time::from_hms(
        u8::try_from(hour).ok()?,
        u8::try_from(minute).ok()?,
        u8::try_from(second).ok()?,
    )
    .ok()?;
    Some(
        time::PrimitiveDateTime::new(date, clock)
            .assume_utc()
            .unix_timestamp()
            - offset,
    )
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
