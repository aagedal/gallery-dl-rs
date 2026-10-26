//! Shared ports of gallery_dl `text`, `util` and `dt` helpers for registry
//! sites, so site modules can stay close to their upstream source.
#![allow(dead_code, unused_imports)]
pub(crate) use crate::sites::art::{
    Scan, date, ext_from_url, extr, extract_iter, filename_from_url, nameext_from_name,
    nameext_into, parse_iso, parse_ts, strptime,
};
pub(crate) use crate::sites::{remove_html, unescape};
use serde_json::{Map, Value};
use url::Url;

/// Python `str.partition`.
pub(crate) fn partition<'t>(text: &'t str, sep: &str) -> (&'t str, &'t str, &'t str) {
    match text.find(sep) {
        Some(i) => (&text[..i], &text[i..i + sep.len()], &text[i + sep.len()..]),
        None => (text, "", ""),
    }
}

/// Python `str.rpartition`.
pub(crate) fn rpartition<'t>(text: &'t str, sep: &str) -> (&'t str, &'t str, &'t str) {
    match text.rfind(sep) {
        Some(i) => (&text[..i], &text[i..i + sep.len()], &text[i + sep.len()..]),
        None => ("", "", text),
    }
}

/// `text.parse_int` with the `0` default.
pub(crate) fn parse_int(text: &str) -> i64 {
    crate::sites::art::parse_int(text, 0)
}

/// `text.parse_int` of a JSON value (numbers pass through).
pub(crate) fn int_of(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .unwrap_or(0),
        Some(Value::String(s)) => parse_int(s),
        Some(Value::Bool(b)) => i64::from(*b),
        _ => 0,
    }
}

/// `text.parse_float`.
pub(crate) fn parse_float(text: &str) -> f64 {
    text.trim()
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
        .unwrap_or(0.0)
}

/// `text.split_html`: unescaped, stripped text pieces between tags.
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
            parts.push(unescape(piece).trim().to_owned());
        }
        match tail {
            Some(tail) => rest = tail,
            None => break,
        }
    }
    parts
}

/// `text.unquote`.
pub(crate) fn unquote(text: &str) -> String {
    crate::sites::boorus::unquote(text)
}

/// `text.unquote_plus`.
pub(crate) fn unquote_plus(text: &str) -> String {
    crate::sites::boorus::unquote_plus(text)
}

/// `text.ensure_http_scheme`.
pub(crate) fn ensure_http_scheme(url: &str) -> String {
    if !url.is_empty() && !url.starts_with("https://") && !url.starts_with("http://") {
        format!("https://{}", url.trim_start_matches(['/', ':']))
    } else {
        url.to_owned()
    }
}

/// `text.root_from_url`: `scheme://host` of a URL.
pub(crate) fn root_of(url: &Url) -> String {
    match url.port() {
        Some(port) => format!(
            "{}://{}:{port}",
            url.scheme(),
            url.host_str().unwrap_or_default()
        ),
        None => format!("{}://{}", url.scheme(), url.host_str().unwrap_or_default()),
    }
}

/// `text.parse_query`: the last value of each non-empty query key.
pub(crate) fn parse_query(query: &str) -> Map<String, Value> {
    url::form_urlencoded::parse(query.as_bytes())
        .filter(|(k, v)| !k.is_empty() && !v.is_empty())
        .map(|(k, v)| (k.into_owned(), Value::String(v.into_owned())))
        .collect()
}

/// A keyword map from `(key, value)` pairs.
pub(crate) fn kw<const N: usize>(pairs: [(&str, Value); N]) -> Map<String, Value> {
    pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect()
}

/// A JSON string field, or `""`.
pub(crate) fn str_of<'v>(value: &'v Value, key: &str) -> &'v str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// A JSON field, or null.
pub(crate) fn get(value: &Value, key: &str) -> Value {
    value.get(key).cloned().unwrap_or(Value::Null)
}

/// The path segments of a URL as upstream regexes see them (raw, with
/// empty segments kept), without the leading empty segment.
pub(crate) fn path_parts(url: &Url) -> Vec<&str> {
    url.path().split('/').skip(1).collect()
}

/// Whether a URL is plain HTTP(S) on one of `hosts`, optionally behind
/// `www.`.
pub(crate) fn on(url: &Url, hosts: &[&str], www: bool) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = if www {
        host.strip_prefix("www.").unwrap_or(host)
    } else {
        host
    };
    crate::sites::plain(url) && hosts.contains(&host)
}

/// Whether `text` is non-empty and all ASCII digits.
pub(crate) fn digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

/// Whether `text` matches `[\w-]+` (ASCII word characters and hyphens,
/// plus non-ASCII letters as Python's Unicode `\w` does).
pub(crate) fn word(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
}

/// Python truthiness of a JSON value.
pub(crate) fn truthy(value: Option<&Value>) -> bool {
    crate::sites::art::truthy(value)
}
