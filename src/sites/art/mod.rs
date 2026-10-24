//! Art portfolios, creator platforms and blogs.
//!
//! Shared helpers mirror the gallery_dl `text` and `dt` functions that the
//! upstream modules in this group rely on.

use crate::sites::Site;
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

mod ao3;
mod arena;
mod behance;
mod boosty;
mod booth;
mod cien;
mod civitai;
mod fantia;
mod fivehundredpx;
mod hatenablog;
mod inkbunny;
mod issuu;
mod itaku;
mod itchio;
mod lightroom;
mod livedoor;
mod lofter;
mod myportfolio;
mod naverblog;
mod nijie;
mod picarto;
mod piczel;
mod pillowfort;
mod poipiku;
mod skeb;
mod slideshare;
mod smugmug;
mod speakerdeck;
mod steamgriddb;
mod subscribestar;
mod thirtyfivephoto;
mod toyhouse;
mod wikiart;
mod xfolio;

pub(super) static SITES: &[&Site] = &[
    &behance::SITE,
    &hatenablog::SITE,
    &livedoor::SITE,
    &lofter::SITE,
    &myportfolio::SITE,
    &naverblog::SITE,
    &steamgriddb::SITE,
    &thirtyfivephoto::SITE,
    &xfolio::SITE,
    &ao3::SITE,
    &arena::SITE,
    &boosty::SITE,
    &booth::SITE,
    &cien::SITE,
    &civitai::SITE,
    &fantia::SITE,
    &fivehundredpx::SITE,
    &inkbunny::SITE,
    &issuu::SITE,
    &itchio::SITE,
    &itaku::SITE,
    &lightroom::SITE,
    &nijie::NIJIE,
    &nijie::HORNE,
    &picarto::SITE,
    &piczel::SITE,
    &pillowfort::SITE,
    &poipiku::SITE,
    &skeb::SITE,
    &slideshare::SITE,
    &smugmug::SITE,
    &speakerdeck::SITE,
    &subscribestar::SITE,
    &toyhouse::SITE,
    &wikiart::SITE,
];

/// gallery-dl `text.extract_from`: sequential `extr` calls that advance a
/// cursor; a miss returns `""` and leaves the cursor in place.
pub(crate) struct Scan<'t> {
    text: &'t str,
    pos: usize,
}

impl<'t> Scan<'t> {
    pub fn new(text: &'t str) -> Self {
        Self { text, pos: 0 }
    }

    pub fn extr(&mut self, begin: &str, end: &str) -> &'t str {
        self.find(begin, end).unwrap_or_default()
    }

    /// Like [`Scan::extr`], but `None` when either delimiter is missing.
    pub fn find(&mut self, begin: &str, end: &str) -> Option<&'t str> {
        let rest = &self.text[self.pos..];
        let first = rest.find(begin)? + begin.len();
        let last = rest[first..].find(end)? + first;
        let value = &rest[first..last];
        self.pos += last + end.len();
        Some(value)
    }
}

/// gallery-dl `text.extract_iter`.
pub(crate) fn extract_iter<'t>(text: &'t str, begin: &str, end: &str) -> Vec<&'t str> {
    let mut scan = Scan::new(text);
    std::iter::from_fn(|| scan.find(begin, end)).collect()
}

/// gallery-dl `text.extr` with the `""` default.
pub(crate) fn extr<'t>(text: &'t str, begin: &str, end: &str) -> &'t str {
    crate::sites::extr(text, begin, end).unwrap_or_default()
}

/// gallery-dl `text.split_html`.
pub(crate) fn split_html(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut rest = text;
    loop {
        let (piece, tail) = match rest.find('<') {
            Some(start) => match rest[start + 1..].find('>') {
                Some(end) if end > 0 => (&rest[..start], Some(&rest[start + end + 2..])),
                _ => (rest, None),
            },
            None => (rest, None),
        };
        if !piece.trim().is_empty() {
            parts.push(crate::sites::unescape(piece).trim().to_owned());
        }
        match tail {
            Some(tail) => rest = tail,
            None => break,
        }
    }
    parts
}

/// gallery-dl `text.parse_int`: Python `int()` of a string, else `default`.
pub(crate) fn parse_int(text: &str, default: i64) -> i64 {
    let trimmed = text.trim();
    let digits = trimmed.strip_prefix(['+', '-']).unwrap_or(trimmed);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit() || b == b'_') {
        return default;
    }
    trimmed.replace('_', "").parse().unwrap_or(default)
}

/// gallery-dl `text.parse_query`: name-value pairs in order, keeping the
/// first value of repeated names and skipping pairs without `=`.
pub(crate) fn parse_query(qs: &str) -> Vec<(String, String)> {
    let mut result: Vec<(String, String)> = Vec::new();
    for pair in qs.split('&') {
        let Some((name, value)) = pair.split_once('=') else {
            continue;
        };
        let name = crate::sites::percent_decode(&name.replace('+', " "));
        if !result.iter().any(|(n, _)| *n == name) {
            result.push((name, crate::sites::percent_decode(&value.replace('+', " "))));
        }
    }
    result
}

/// gallery-dl `text.filename_from_url` on a raw string.
pub(crate) fn filename_from_url(raw: &str) -> &str {
    let path = raw.split('?').next().unwrap_or_default();
    path.rsplit('/').next().unwrap_or_default()
}

/// gallery-dl `text.ext_from_url`.
pub(crate) fn ext_from_url(raw: &str) -> String {
    match filename_from_url(raw).rsplit_once('.') {
        Some((name, ext)) if !name.is_empty() => ext.to_lowercase(),
        _ => String::new(),
    }
}

/// gallery-dl `text.nameext_from_name`.
pub(crate) fn nameext_from_name(name: &str) -> (String, String) {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && ext.chars().count() <= 16 => {
            (stem.to_owned(), ext.to_lowercase())
        }
        _ => (name.to_owned(), String::new()),
    }
}

/// gallery-dl `text.nameext_from_url` on a raw string, written into `data`.
pub(crate) fn nameext_into(raw: &str, data: &mut Map<String, Value>) {
    let (name, ext) = nameext_from_name(&crate::sites::percent_decode(filename_from_url(raw)));
    data.insert("filename".into(), name.into());
    data.insert("extension".into(), ext.into());
}

/// A media URL that must be plain HTTP(S); relative URLs resolve against
/// `base`.
pub(crate) fn file_url(raw: &str, base: &str) -> Option<Url> {
    let raw = raw.trim();
    let url = if raw.starts_with("//") {
        Url::parse(&format!("https:{raw}")).ok()?
    } else {
        Url::parse(base).ok()?.join(raw).ok()?
    };
    crate::sites::plain(&url).then_some(url)
}

/// Parse an absolute URL built from site data.
pub(crate) fn url(raw: &str) -> Result<Url> {
    Url::parse(raw).map_err(|_| error("invalid site URL"))
}

/// The upstream `date` keyword value for an optional Unix timestamp.
pub(crate) fn date(seconds: Option<i64>) -> Value {
    crate::filter::source_date_text(seconds).into()
}

/// gallery-dl `dt.parse_iso` (Python `datetime.fromisoformat`), UTC seconds.
pub(crate) fn parse_iso(text: &str) -> Option<i64> {
    if text.is_empty() || text.bytes().all(|b| b.is_ascii_digit()) && text.len() != 8 {
        return None;
    }
    crate::source_date::parse_string(text)
}

/// gallery-dl `dt.parse_ts` for JSON values (ints and numeric strings).
pub(crate) fn parse_ts(value: &Value) -> Option<i64> {
    crate::source_date::parse_timestamp(value)
}

const WEEKDAYS: [&str; 7] = [
    "monday",
    "tuesday",
    "wednesday",
    "thursday",
    "friday",
    "saturday",
    "sunday",
];

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

/// A subset of Python `datetime.strptime` returning UTC seconds: `%Y %m %d
/// %H %M %S %y %B %b %A %a %p %I %z %f %%` and literal text; weekday names
/// are matched but not checked against the date. Like Python,
/// whitespace in the format matches any run of whitespace.
pub(crate) fn strptime(text: &str, format: &str) -> Option<i64> {
    let (mut year, mut month, mut day) = (1900i32, 1u8, 1u8);
    let (mut hour, mut minute, mut second) = (0u8, 0u8, 0u8);
    let mut pm = None;
    let mut offset = 0i64;
    let mut input = text;
    let mut fmt = format.chars().peekable();
    fn number(input: &mut &str, max: usize) -> Option<i64> {
        let len = input
            .bytes()
            .take(max)
            .take_while(u8::is_ascii_digit)
            .count();
        if len == 0 {
            return None;
        }
        let value = input[..len].parse().ok()?;
        *input = &input[len..];
        Some(value)
    }
    while let Some(c) = fmt.next() {
        if c.is_whitespace() {
            input = input.trim_start();
            continue;
        }
        if c != '%' {
            input = input.strip_prefix(c)?;
            continue;
        }
        match fmt.next()? {
            'Y' => year = number(&mut input, 4)? as i32,
            'y' => {
                let y = number(&mut input, 2)? as i32;
                year = if y < 69 { 2000 + y } else { 1900 + y };
            }
            'm' => month = number(&mut input, 2)? as u8,
            'd' => day = number(&mut input, 2)? as u8,
            'H' | 'I' => hour = number(&mut input, 2)? as u8,
            'M' => minute = number(&mut input, 2)? as u8,
            'S' => second = number(&mut input, 2)? as u8,
            'f' => {
                number(&mut input, 6)?;
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
            'B' | 'b' => {
                let lower = input.to_lowercase();
                let (index, len) = MONTHS.iter().enumerate().find_map(|(i, name)| {
                    if lower.starts_with(name) {
                        Some((i, name.len()))
                    } else if lower.starts_with(&name[..3]) {
                        Some((i, 3))
                    } else {
                        None
                    }
                })?;
                month = index as u8 + 1;
                input = input.get(len..)?;
            }
            'A' | 'a' => {
                let lower = input.to_lowercase();
                let len = WEEKDAYS.iter().find_map(|name| {
                    if lower.starts_with(name) {
                        Some(name.len())
                    } else {
                        lower.starts_with(&name[..3]).then_some(3)
                    }
                })?;
                input = input.get(len..)?;
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
                    let h = number(&mut input, 2)?;
                    input = input.strip_prefix(':').unwrap_or(input);
                    let m = number(&mut input, 2)?;
                    offset = sign * (h * 3600 + m * 60);
                }
            }
            '%' => input = input.strip_prefix('%')?,
            _ => return None,
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
    let date =
        time::Date::from_calendar_date(year, time::Month::try_from(month).ok()?, day).ok()?;
    let time = time::Time::from_hms(hour, minute, second).ok()?;
    Some(
        time::PrimitiveDateTime::new(date, time)
            .assume_utc()
            .unix_timestamp()
            - offset,
    )
}

/// Whether `label` is one DNS label (letters, digits, inner hyphens).
pub(crate) fn valid_label(label: &str) -> bool {
    (1..=63).contains(&label.len())
        && label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !label.starts_with('-')
        && !label.ends_with('-')
}

/// The `USER` in `USER.domain` for a plain URL, with exactly one label.
pub(crate) fn subdomain<'u>(url: &'u Url, domain: &str) -> Option<&'u str> {
    if !crate::sites::plain(url) {
        return None;
    }
    let host = url.host_str()?;
    let user = host.strip_suffix(domain)?.strip_suffix('.')?;
    valid_label(user).then_some(user)
}

/// Exact host, or `www.` + host, on a plain URL.
pub(crate) fn on_host(url: &Url, hosts: &[&str]) -> bool {
    crate::sites::plain(url) && crate::sites::host_is(url, hosts)
}

/// ASCII digits only, 1..=20 characters.
pub(crate) fn digits(text: &str) -> bool {
    (1..=20).contains(&text.len()) && text.bytes().all(|b| b.is_ascii_digit())
}

/// The leading ASCII digits of `text`, as upstream `(\d+)` without an
/// anchor; at most 20 digits.
pub(crate) fn leading_digits(text: &str) -> Option<&str> {
    let len = text.bytes().take_while(u8::is_ascii_digit).count();
    (1..=20).contains(&len).then(|| &text[..len])
}

/// A path segment matching upstream `[^/?#]+`.
pub(crate) fn segment(text: &str) -> bool {
    !text.is_empty() && !text.contains(['/', '?', '#'])
}

/// The raw (still percent-encoded) path segments, keeping empty ones out.
pub(crate) fn raw_segments(url: &Url) -> Vec<&str> {
    url.path_segments()
        .map(|s| s.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default()
}

/// A JSON object field as a string, `""` when missing.
pub(crate) fn str_of<'v>(value: &'v Value, key: &str) -> &'v str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// Python truthiness of a JSON value.
pub(crate) fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// Merge `extra` into `data`, overwriting like Python `dict.update`.
pub(crate) fn update(data: &mut Map<String, Value>, extra: &Map<String, Value>) {
    for (key, value) in extra {
        data.insert(key.clone(), value.clone());
    }
}

/// Decode a JSON document embedded in HTML.
pub(crate) fn json_loads(text: &str, name: &str) -> Result<Value> {
    serde_json::from_str(text).map_err(|_| error(format!("{name} page contains no valid JSON")))
}

/// `Extractor._extract_nextdata`: a Next.js page's `__NEXT_DATA__` JSON.
pub(crate) fn next_data(page: &str, name: &str) -> Result<Value> {
    let data = match extr(
        page,
        " id=\"__NEXT_DATA__\" type=\"application/json\">",
        "</script>",
    ) {
        "" => extr(
            page,
            " id='__NEXT_DATA__' type='application/json'>",
            "</script>",
        ),
        data => data,
    };
    json_loads(data, name)
}

/// Upstream `Dispatch._dispatch_extractors`: the subcategories named by the
/// `include` option (`"all"`, a comma-separated string or a list), else
/// `default`, in option order; unknown names are dropped.
pub(crate) fn include(
    http: &crate::sites::Http,
    all: &[&'static str],
    default: &[&'static str],
) -> Vec<&'static str> {
    let names: Vec<String> = match http.option("include") {
        None => default.iter().map(|s| (*s).to_owned()).collect(),
        Some(Value::String(s)) if s == "all" => all.iter().map(|s| (*s).to_owned()).collect(),
        Some(Value::String(s)) => s.replace(' ', "").split(',').map(str::to_owned).collect(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        Some(_) => Vec::new(),
    };
    names
        .iter()
        .filter_map(|name| all.iter().copied().find(|sub| sub == name))
        .collect()
}

/// Send the session's cookies with a file, as upstream downloads share the
/// extractor's cookie jar; only cookies scoped to the file's host apply.
pub(crate) fn session_cookies(
    http: &crate::sites::Http,
    item: crate::sites::Item,
) -> crate::sites::Item {
    let cookie = http
        .api(item.url.as_str())
        .ok()
        .and_then(|url| http.cookie_header(&url));
    match cookie {
        Some(cookie) => item.header("Cookie", &cookie),
        None => item,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_helpers() {
        let mut scan = Scan::new("<a>1</a><a>2</a>");
        assert_eq!(scan.extr("<a>", "</a>"), "1");
        assert_eq!(scan.extr("<a>", "</a>"), "2");
        assert_eq!(scan.extr("<a>", "</a>"), "");
        assert_eq!(extract_iter("[1][2][3", "[", "]"), ["1", "2"]);
        assert_eq!(parse_int(" 12 ", 0), 12);
        assert_eq!(parse_int("x", 7), 7);
        assert_eq!(ext_from_url("https://x/a/b.JPG?x=1"), "jpg");
        let mut map = Map::new();
        nameext_into("https://x/a/My%20F.png?1", &mut map);
        assert_eq!(map["filename"], "My F");
        assert_eq!(map["extension"], "png");
        assert_eq!(
            parse_query("a=1&b&a=2&c+d=x%20y+z"),
            [("a".into(), "1".into()), ("c d".into(), "x y z".into())]
        );
        assert_eq!(leading_digits("12ab"), Some("12"));
        assert_eq!(leading_digits("ab"), None);
    }

    #[test]
    fn dates() {
        assert_eq!(
            strptime("2020-01-02 03:04:05", "%Y-%m-%d %H:%M:%S"),
            Some(1577934245)
        );
        assert_eq!(strptime("January 2, 2020", "%B %d, %Y"), Some(1577923200));
        assert_eq!(
            strptime("2020-01-02T03:04:05+0100", "%Y-%m-%dT%H:%M:%S%z"),
            Some(1577934245 - 3600)
        );
        assert_eq!(strptime("2020-13-02", "%Y-%m-%d"), None);
        assert_eq!(
            strptime(
                "Thu, 02 Jan 2020 12:04:05 +0900",
                "%a, %d %b %Y %H:%M:%S %z"
            ),
            Some(1577934245)
        );
        assert_eq!(
            strptime("Thursday Jan 02 03:04:05 2020", "%a %b %d %H:%M:%S %Y"),
            Some(1577934245)
        );
        assert_eq!(parse_iso("2020-01-02T03:04:05Z"), Some(1577934245));
        assert_eq!(parse_iso("12345"), None);
        assert_eq!(date(Some(1577934245)), Value::from("2020-01-02 03:04:05"));
    }

    #[test]
    fn hosts() {
        let u = |raw: &str| Url::parse(raw).unwrap();
        assert_eq!(
            subdomain(&u("https://abc.myportfolio.com/x"), "myportfolio.com"),
            Some("abc")
        );
        assert_eq!(
            subdomain(&u("https://a.b.myportfolio.com/"), "myportfolio.com"),
            None
        );
        assert_eq!(
            subdomain(&u("https://evilmyportfolio.com/"), "myportfolio.com"),
            None
        );
        assert_eq!(
            subdomain(&u("https://a.myportfolio.com:81/"), "myportfolio.com"),
            None
        );
        assert!(on_host(&u("https://www.x.test/"), &["x.test"]));
        assert!(!on_host(&u("https://u:p@x.test/"), &["x.test"]));
    }
}
