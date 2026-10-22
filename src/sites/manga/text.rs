//! Ports of the `gallery_dl.text`, `util` and `dt` helpers that manga
//! extractors use for scraping and keyword values.
use serde_json::Value;

/// `text.extract`: the text between `begin` and `end` at or after `pos`,
/// and the position after `end`; `(None, pos)` when either is missing.
pub(crate) fn extract<'t>(
    txt: &'t str,
    begin: &str,
    end: &str,
    pos: usize,
) -> (Option<&'t str>, usize) {
    let Some(tail) = txt.get(pos..) else {
        return (None, pos);
    };
    let Some(start) = tail.find(begin).map(|i| pos + i + begin.len()) else {
        return (None, pos);
    };
    match txt[start..].find(end) {
        Some(stop) => (Some(&txt[start..start + stop]), start + stop + end.len()),
        None => (None, pos),
    }
}

/// `text.extr`: like [`extract`] from the start, defaulting to `""`.
pub(crate) fn extr<'t>(txt: &'t str, begin: &str, end: &str) -> &'t str {
    crate::sites::extr(txt, begin, end).unwrap_or_default()
}

/// `text.rextr`: search `begin` from the end of the text.
pub(crate) fn rextr<'t>(txt: &'t str, begin: &str, end: &str) -> &'t str {
    let Some(start) = txt.rfind(begin).map(|i| i + begin.len()) else {
        return "";
    };
    txt[start..]
        .find(end)
        .map_or("", |stop| &txt[start..start + stop])
}

/// `text.extract_iter`: every non-overlapping `begin`..`end` value.
pub(crate) fn extract_iter<'t>(txt: &'t str, begin: &str, end: &str) -> Vec<&'t str> {
    let mut out = Vec::new();
    let mut pos = 0;
    while let (Some(value), next) = extract(txt, begin, end, pos) {
        out.push(value);
        pos = next;
    }
    out
}

/// `text.extract_from`: sequential extraction that only advances on success
/// and yields `""` for missing values.
pub(crate) struct Extr<'t> {
    txt: &'t str,
    pos: usize,
}

impl<'t> Extr<'t> {
    pub fn new(txt: &'t str) -> Self {
        Self { txt, pos: 0 }
    }
    pub fn get(&mut self, begin: &str, end: &str) -> &'t str {
        match extract(self.txt, begin, end, self.pos) {
            (Some(value), pos) => {
                self.pos = pos;
                value
            }
            (None, _) => "",
        }
    }
}

/// `str.partition`.
pub(crate) fn partition<'t>(txt: &'t str, sep: &str) -> (&'t str, &'t str, &'t str) {
    match txt.find(sep) {
        Some(i) => (&txt[..i], &txt[i..i + sep.len()], &txt[i + sep.len()..]),
        None => (txt, "", ""),
    }
}

/// `str.rpartition`.
pub(crate) fn rpartition<'t>(txt: &'t str, sep: &str) -> (&'t str, &'t str, &'t str) {
    match txt.rfind(sep) {
        Some(i) => (&txt[..i], &txt[i..i + sep.len()], &txt[i + sep.len()..]),
        None => ("", "", txt),
    }
}

/// `text.parse_int`: Python `int()` of a string, or 0.
pub(crate) fn parse_int(txt: &str) -> i64 {
    let txt = txt.trim();
    let digits = txt.strip_prefix(['+', '-']).unwrap_or(txt);
    if digits.is_empty()
        || digits.starts_with('_')
        || digits.ends_with('_')
        || digits.contains("__")
        || !digits.bytes().all(|b| b.is_ascii_digit() || b == b'_')
    {
        return 0;
    }
    txt.replace('_', "").parse().unwrap_or(0)
}

/// `text.parse_int` for an optional JSON value (numbers pass through).
pub(crate) fn parse_int_value(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
        Some(Value::String(s)) => parse_int(s),
        Some(Value::Bool(b)) => i64::from(*b),
        _ => 0,
    }
}

/// `text.parse_float`: Python `float()` of a string, or 0.0.
pub(crate) fn parse_float(txt: &str) -> f64 {
    let txt = txt.trim();
    if txt.is_empty()
        || !txt
            .bytes()
            .all(|b| b.is_ascii_digit() || b"+-.eE".contains(&b))
    {
        return 0.0;
    }
    txt.parse().unwrap_or(0.0)
}

/// A float keyword value; non-finite values become 0.0.
pub(crate) fn float(value: f64) -> Value {
    serde_json::Number::from_f64(value).map_or_else(|| 0.into(), Value::Number)
}

/// `chnum, sep, minor = text.partition(".")` with `text.parse_int(chnum)`.
pub(crate) fn chapter(txt: &str) -> (i64, String) {
    let (number, sep, minor) = partition(txt, ".");
    (parse_int(number), format!("{sep}{minor}"))
}

/// `text.split_html`: unescaped, stripped text between `<[^>]+>` tags.
pub(crate) fn split_html(txt: &str) -> Vec<String> {
    let mut pieces = Vec::new();
    let (mut piece_start, mut search) = (0, 0);
    while let Some(open) = txt[search..].find('<').map(|i| search + i) {
        match txt[open + 1..].find('>') {
            Some(len) if len > 0 => {
                pieces.push(&txt[piece_start..open]);
                piece_start = open + len + 2;
                search = piece_start;
            }
            Some(_) => search = open + 1,
            None => break,
        }
    }
    pieces.push(&txt[piece_start..]);
    pieces
        .into_iter()
        .filter(|p| !p.is_empty() && !p.chars().all(char::is_whitespace))
        .map(|p| crate::sites::unescape(p).trim().to_owned())
        .collect()
}

pub(crate) fn strings(values: Vec<String>) -> Value {
    Value::Array(values.into_iter().map(Value::String).collect())
}

pub(crate) fn every_other(values: Vec<String>, start: usize) -> Vec<String> {
    values.into_iter().skip(start).step_by(2).collect()
}

/// `text.ensure_http_scheme`.
pub(crate) fn ensure_http_scheme(url: &str) -> String {
    if !url.is_empty() && !url.starts_with("https://") && !url.starts_with("http://") {
        format!("https://{}", url.trim_start_matches(['/', ':']))
    } else {
        url.to_owned()
    }
}

/// `text.parse_query`: first value for each name, `+` as space.
pub(crate) fn parse_query(qs: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for pair in qs.split('&') {
        let (name, eq, value) = partition(pair, "=");
        if eq.is_empty() {
            continue;
        }
        let name = crate::sites::percent_decode(&name.replace('+', " "));
        if out.iter().all(|(n, _)| *n != name) {
            out.push((name, crate::sites::percent_decode(&value.replace('+', " "))));
        }
    }
    out
}

pub(crate) fn query_get<'q>(query: &'q [(String, String)], name: &str) -> Option<&'q str> {
    query
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
}

/// `util.code_to_language`.
pub(crate) fn code_to_language(code: &str) -> Value {
    const CODES: &[(&str, &str)] = &[
        ("ar", "Arabic"),
        ("bg", "Bulgarian"),
        ("bn", "Bengali"),
        ("ca", "Catalan"),
        ("cs", "Czech"),
        ("da", "Danish"),
        ("de", "German"),
        ("el", "Greek"),
        ("en", "English"),
        ("es", "Spanish"),
        ("fa", "Persian"),
        ("fi", "Finnish"),
        ("fr", "French"),
        ("he", "Hebrew"),
        ("hi", "Hindi"),
        ("hu", "Hungarian"),
        ("id", "Indonesian"),
        ("it", "Italian"),
        ("ja", "Japanese"),
        ("ko", "Korean"),
        ("ms", "Malay"),
        ("nl", "Dutch"),
        ("no", "Norwegian"),
        ("pl", "Polish"),
        ("pt", "Portuguese"),
        ("ro", "Romanian"),
        ("ru", "Russian"),
        ("sk", "Slovak"),
        ("sl", "Slovenian"),
        ("sr", "Serbian"),
        ("sv", "Swedish"),
        ("th", "Thai"),
        ("tr", "Turkish"),
        ("uk", "Ukrainian"),
        ("vi", "Vietnamese"),
        ("zh", "Chinese"),
    ];
    let code = code.to_lowercase();
    CODES
        .iter()
        .find(|(c, _)| *c == code)
        .map_or(Value::Null, |(_, name)| (*name).into())
}

/// `dt.parse_iso`: Unix seconds of an ISO 8601 date/time, UTC if naive.
pub(crate) fn iso(txt: &str) -> Option<i64> {
    let txt = txt.trim();
    // Python's fromisoformat needs a calendar date, not a bare number.
    if txt.len() < 8 || (txt.bytes().all(|b| b.is_ascii_digit()) && txt.len() != 8) {
        return None;
    }
    crate::source_date::parse_string(txt)
}

/// `dt.parse_ts`.
pub(crate) fn timestamp(value: &Value) -> Option<i64> {
    crate::source_date::parse_timestamp(value)
}

fn month(name: &str) -> Option<u8> {
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let lower = name.to_ascii_lowercase();
    MONTHS.iter().position(|m| *m == lower).map(|i| i as u8 + 1)
}

fn number(txt: &str, max_len: usize) -> Option<u32> {
    (!txt.is_empty() && txt.len() <= max_len && txt.bytes().all(|b| b.is_ascii_digit()))
        .then(|| txt.parse().ok())
        .flatten()
}

fn civil(year: u32, month: u8, day: u32, hms: (u32, u32, u32)) -> Option<i64> {
    let date = time::Date::from_calendar_date(
        i32::try_from(year).ok()?,
        time::Month::try_from(month).ok()?,
        u8::try_from(day).ok()?,
    )
    .ok()?;
    let time = time::Time::from_hms(
        u8::try_from(hms.0).ok()?,
        u8::try_from(hms.1).ok()?,
        u8::try_from(hms.2).ok()?,
    )
    .ok()?;
    Some(
        time::PrimitiveDateTime::new(date, time)
            .assume_utc()
            .unix_timestamp(),
    )
}

/// `dt.parse(txt, "%b %d, %Y")`, e.g. `Jan 05, 2020`.
pub(crate) fn month_day_year(txt: &str) -> Option<i64> {
    let mut parts = txt.split_whitespace();
    let (m, d, y) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    civil(
        number(y, 4)?,
        month(m)?,
        number(d.strip_suffix(',')?, 2)?,
        (0, 0, 0),
    )
}

/// `dt.parse(txt, "%b-%d-%Y %I:%M:%S %p")`, e.g. `Jan-05-2020 01:02:03 PM`.
pub(crate) fn month_day_year_clock(txt: &str) -> Option<i64> {
    let mut parts = txt.split_whitespace();
    let (date, clock, meridiem) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let mut date = date.split('-');
    let (m, d, y) = (date.next()?, date.next()?, date.next()?);
    let mut clock = clock.split(':');
    let (h, mi, s) = (clock.next()?, clock.next()?, clock.next()?);
    let hour = number(h, 2).filter(|h| (1..=12).contains(h))? % 12;
    let hour = match meridiem.to_ascii_uppercase().as_str() {
        "AM" => hour,
        "PM" => hour + 12,
        _ => return None,
    };
    civil(
        number(y, 4)?,
        month(m)?,
        number(d, 2)?,
        (hour, number(mi, 2)?, number(s, 2)?),
    )
}

/// The `date` keyword text for Unix seconds, or `[Invalid DateTime]`.
pub(crate) fn date(seconds: Option<i64>) -> Value {
    crate::filter::source_date_text(seconds).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mirrors_gallery_dl_text() {
        let page = "<a>1</a><a>2</a><b>3";
        assert_eq!(extract_iter(page, "<a>", "</a>"), ["1", "2"]);
        assert_eq!(extract(page, "<b>", "<", 0), (None, 0));
        let mut e = Extr::new("x=1; y=2; z=3;");
        assert_eq!(e.get("y=", ";"), "2");
        assert_eq!(e.get("x=", ";"), "");
        assert_eq!(e.get("z=", ";"), "3");
        assert_eq!(rextr("a=1 a=2|", "a=", "|"), "2");
        assert_eq!(partition("12.5", "."), ("12", ".", "5"));
        assert_eq!(rpartition("a/b/c", "/"), ("a/b", "/", "c"));
        assert_eq!(rpartition("abc", "/"), ("", "", "abc"));
        assert_eq!(chapter("012.5"), (12, ".5".into()));
        assert_eq!(chapter("x"), (0, String::new()));
        assert_eq!(parse_int(" 42 "), 42);
        assert_eq!(parse_int("4a"), 0);
        assert_eq!(parse_float("8.5"), 8.5);
        assert_eq!(
            split_html("<li><b>Author</b>: <a>A &amp; B</a> </li>"),
            ["Author", ":", "A & B"]
        );
        assert_eq!(
            ensure_http_scheme("//cdn.test/a.jpg"),
            "https://cdn.test/a.jpg"
        );
        assert_eq!(
            parse_query("a=1&b=x+y&a=2&c"),
            [("a".into(), "1".into()), ("b".into(), "x y".into())]
        );
        assert_eq!(code_to_language("EN"), "English");
        assert_eq!(code_to_language("xx"), Value::Null);
    }

    #[test]
    fn parses_upstream_dates() {
        assert_eq!(iso("2017-07-14T02:40:00+00:00"), Some(1500000000));
        assert_eq!(iso("2017-07-14T04:40:00.123+02:00"), Some(1500000000));
        assert_eq!(iso("2017-07-14T02:40:00Z"), Some(1500000000));
        assert_eq!(iso("2017-07-14 02:40:00"), Some(1500000000));
        assert_eq!(iso(""), None);
        assert_eq!(iso("12345"), None);
        assert_eq!(month_day_year("Jul 14, 2017"), Some(1499990400));
        assert_eq!(month_day_year("Jul 14 2017"), None);
        assert_eq!(
            month_day_year_clock("Jul-14-2017 02:40:00 AM"),
            Some(1500000000)
        );
        assert_eq!(
            month_day_year_clock("Jul-14-2017 12:00:00 PM"),
            Some(1500033600)
        );
        assert_eq!(date(Some(1500000000)), "2017-07-14 02:40:00");
        assert_eq!(date(None), "[Invalid DateTime]");
    }
}
