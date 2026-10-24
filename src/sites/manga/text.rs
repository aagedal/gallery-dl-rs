//! Ports of the `gallery_dl.text`, `util` and `dt` helpers that manga
//! extractors use for scraping and keyword values.
use serde_json::Value;

/// `str.partition`.
pub(crate) fn partition<'t>(txt: &'t str, sep: &str) -> (&'t str, &'t str, &'t str) {
    match txt.find(sep) {
        Some(i) => (&txt[..i], &txt[i..i + sep.len()], &txt[i + sep.len()..]),
        None => (txt, "", ""),
    }
}

/// `text.rextr`: the text after the last `begin`, up to the next `end`.
pub(crate) fn rextr<'t>(txt: &'t str, begin: &str, end: &str) -> &'t str {
    let Some(first) = txt.rfind(begin).map(|i| i + begin.len()) else {
        return "";
    };
    txt[first..]
        .find(end)
        .map_or("", |last| &txt[first..first + last])
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

/// `chnum, sep, minor = text.partition(".")` with `text.parse_int(chnum)`.
pub(crate) fn chapter(txt: &str) -> (i64, String) {
    let (number, sep, minor) = partition(txt, ".");
    (parse_int(number), format!("{sep}{minor}"))
}

/// `text.ensure_http_scheme`.
pub(crate) fn ensure_http_scheme(url: &str) -> String {
    if !url.is_empty() && !url.starts_with("https://") && !url.starts_with("http://") {
        format!("https://{}", url.trim_start_matches(['/', ':']))
    } else {
        url.to_owned()
    }
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

/// The `date` keyword text for Unix seconds, or `[Invalid DateTime]`.
pub(crate) fn date(seconds: Option<i64>) -> Value {
    crate::filter::source_date_text(seconds).into()
}

/// `util.CODES`, for `code_to_language`.
const LANGUAGES: &[(&str, &str)] = &[
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

pub(crate) fn code_to_language(code: &str) -> Option<&'static str> {
    let code = code.to_lowercase();
    LANGUAGES.iter().find(|(c, _)| *c == code).map(|(_, l)| *l)
}
