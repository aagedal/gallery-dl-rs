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

/// `text.parse_query`: name-value pairs, the first value of a name wins;
/// parts without `=` are ignored.
pub(crate) fn parse_query(query: &str) -> Map<String, Value> {
    let mut result = Map::new();
    for part in query.split('&') {
        let (name, eq, value) = partition(part, "=");
        if eq.is_empty() {
            continue;
        }
        let name = unquote(&name.replace('+', " "));
        if !result.contains_key(&name) {
            result.insert(name, Value::String(unquote(&value.replace('+', " "))));
        }
    }
    result
}

/// `text.parse_query_list`: like [`parse_query`], but values of names in
/// `as_list` are collected into lists.
pub(crate) fn parse_query_list(query: &str, as_list: &[&str]) -> Map<String, Value> {
    let mut result = Map::new();
    for part in query.split('&') {
        let (name, eq, value) = partition(part, "=");
        if eq.is_empty() {
            continue;
        }
        let name = unquote(&name.replace('+', " "));
        let value = Value::String(unquote(&value.replace('+', " ")));
        if as_list.contains(&name.as_str()) {
            match result.get_mut(&name) {
                Some(Value::Array(list)) => list.push(value),
                _ => {
                    result.insert(name, Value::Array(vec![value]));
                }
            }
        } else if !result.contains_key(&name) {
            result.insert(name, value);
        }
    }
    result
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

/// A lazily compiled, cached regex.
macro_rules! re {
    ($pattern:expr) => {{
        static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        // Compiled once per call site, even inside loops.
        #[allow(clippy::regex_creation_in_loops)]
        let compiled = RE.get_or_init(|| regex::Regex::new($pattern).expect("static regex"));
        compiled
    }};
}
pub(crate) use re;

/// Upstream `pattern.match(url)`: captures of a regex anchored at the start
/// of a plain HTTP(S) URL. Patterns should begin with `^`.
pub(crate) fn captures<'u>(pattern: &regex::Regex, url: &'u Url) -> Option<regex::Captures<'u>> {
    if !crate::sites::plain(url) {
        return None;
    }
    pattern.captures(url.as_str())
}

/// A capture group as text, or `""`.
pub(crate) fn group<'t>(caps: &regex::Captures<'t>, index: usize) -> &'t str {
    caps.get(index).map_or("", |m| m.as_str())
}

/// `text.parse_bytes` ("500k", "2.5M", ...).
pub(crate) fn parse_bytes(text: &str) -> i64 {
    crate::sites::filehosts::parse_bytes(text)
}

/// The site's `username`/`password` options, as `_get_auth_info`.
pub(crate) fn auth_info(http: &crate::sites::Http) -> Option<(String, String)> {
    let username = http.option_str("username").filter(|u| !u.is_empty())?;
    let password = http
        .option("password")
        .map(crate::sites::scalar)
        .unwrap_or_default();
    Some((username.to_owned(), password))
}

/// An HTTP basic `Authorization` header value.
pub(crate) fn basic_auth(username: &str, password: &str) -> String {
    use base64::Engine;
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"))
    )
}

/// `util.code_to_language`: the English name of a language code.
pub(crate) fn code_to_language(code: &str) -> Option<&'static str> {
    crate::sites::manga::webtoons::code_to_language(code)
}

/// `Extractor._extract_jsonld`: the page's JSON-LD script.
pub(crate) fn jsonld(page: &str) -> crate::Result<Value> {
    let raw = match extr(page, "<script type=\"application/ld+json\">", "</script>") {
        "" => extr(page, "<script type='application/ld+json'>", "</script>"),
        raw => raw,
    };
    serde_json::from_str(raw).map_err(|_| crate::error("page has no valid JSON-LD data"))
}

/// `Extractor._extract_nextdata`: the page's Next.js `__NEXT_DATA__`.
pub(crate) fn nextdata(page: &str) -> crate::Result<Value> {
    let raw = match extr(
        page,
        " id=\"__NEXT_DATA__\" type=\"application/json\">",
        "</script>",
    ) {
        "" => extr(
            page,
            " id='__NEXT_DATA__' type='application/json'>",
            "</script>",
        ),
        raw => raw,
    };
    serde_json::from_str(raw).map_err(|_| crate::error("page has no valid Next.js data"))
}

/// `text.quote` (`urllib.parse.quote` with `safe="/"`).
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

/// `text.rextr`: the text between the last `begin` before `pos` and the
/// following `end`.
pub(crate) fn rextr<'t>(text: &'t str, begin: &str, end: &str, pos: Option<usize>) -> &'t str {
    let limit = pos.unwrap_or(text.len()).min(text.len());
    let Some(first) = text.get(..limit).and_then(|head| head.rfind(begin)) else {
        return "";
    };
    let first = first + begin.len();
    text[first..]
        .find(end)
        .map_or("", |last| &text[first..first + last])
}

/// `util.language_to_code`: the code of an English language name.
pub(crate) fn language_to_code(language: &str) -> Option<&'static str> {
    crate::sites::manga::webtoons::language_to_code(language)
}

/// `text.escape` (`html.escape` with quotes).
pub(crate) fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

/// `text.slugify`.
pub(crate) fn slugify(value: &str) -> String {
    let lower = value.to_lowercase();
    let cleaned: String = lower
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '_' || c.is_whitespace() || *c == '-')
        .collect();
    let mut out = String::new();
    let mut dash = false;
    for c in cleaned.chars() {
        if c == '-' || c.is_whitespace() {
            dash = true;
        } else {
            if dash {
                out.push('-');
                dash = false;
            }
            out.push(c);
        }
    }
    if dash {
        out.push('-');
    }
    out.trim_matches(['-', '_']).to_owned()
}

/// `Dispatch._dispatch_extractors`: the selected `(view, url)` pairs for
/// the `include` option (`default` when unset), in configured order.
pub(crate) fn dispatch<'v>(
    http: &crate::sites::Http,
    views: &'v [(&'v str, String)],
    default: &[&str],
) -> crate::Result<Vec<&'v (&'v str, String)>> {
    let include = match http.option("include") {
        None => None,
        Some(value) => Some(crate::dispatch::Include::parse(value)?),
    };
    let available: Vec<&str> = views.iter().map(|(view, _)| *view).collect();
    Ok(
        crate::dispatch::select(include.as_ref(), default, &available, &[])
            .into_iter()
            .filter_map(|name| views.iter().find(|(view, _)| *view == name))
            .collect(),
    )
}

/// An HLS stream for yt-dlp (`ytdl:` URL with `_ytdl_manifest = "hls"`).
pub(crate) fn hls_item(stream: &str, mut fields: Map<String, Value>) -> Option<crate::sites::Item> {
    fields.insert("_ytdl_manifest".into(), "hls".into());
    let base_url = Url::parse(stream).ok()?;
    let url = Url::parse(&format!("ytdl:{stream}")).ok()?;
    Some(
        crate::sites::Item::new(url, fields).job(crate::extract::MediaJob::Manifest {
            protocol: crate::extract::ManifestProtocol::Hls,
            base_url,
            inline_data: None,
        }),
    )
}

/// A `ytdl:` URL handed to yt-dlp as a video page.
pub(crate) fn ytdl_item(page: &str, fields: Map<String, Value>) -> Option<crate::sites::Item> {
    let page_url = Url::parse(page).ok()?;
    let url = Url::parse(&format!("ytdl:{page}")).ok()?;
    Some(crate::sites::Item::new(url, fields).job(crate::extract::MediaJob::VideoPage { page_url }))
}

/// A `text:` item saving `body` as an inline document.
pub(crate) fn text_item(body: String, fields: Map<String, Value>) -> Option<crate::sites::Item> {
    let (url, job) = crate::extract::inline_text(body).ok()?;
    Some(crate::sites::Item::new(url, fields).job(job))
}

/// `util.bencode` with the base-36 alphabet (`util.b36encode`).
pub(crate) fn b36encode(mut num: u64) -> String {
    const ALPHABET: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    while num > 0 {
        out.push(ALPHABET[(num % 36) as usize]);
        num /= 36;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}
