//! File and album hosting services.
//!
//! Shared helpers mirror the gallery_dl `text`, `util` and `dt` functions
//! these extractors rely on, plus a small request helper for the HTTP
//! features the registry client does not offer (form POSTs, HEAD requests,
//! unfollowed redirects and response cookies).
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use futures_util::{StreamExt, stream};
use serde_json::{Map, Value};
use std::time::Duration;
use url::Url;

mod bunkr;
mod chevereto;
mod cyberdrop;
mod filester;
mod gofile;
mod imagebam;
mod imagechest;
mod imagehost;
mod imagepond;
mod imageshack;
mod imgpile;
mod imgth;
mod koofr;
mod lensdump;
mod lolisafe;
mod mixdrop;
mod telegraph;
mod turbo;
mod urlgalleries;
mod urlshortener;
mod webmshare;

pub(super) static SITES: &[&Site] = &[
    &gofile::SITE,
    &bunkr::SITE,
    &cyberdrop::SITE,
    &chevereto::JPGFISH,
    &chevereto::IMGLIKE,
    &chevereto::PUTMEGA,
    &turbo::SITE,
    &filester::SITE,
    &imagehost::IMXTO,
    &imagehost::ACIDIMG,
    &imagehost::IMAGEVENUE,
    &imagehost::IMAGETWIST,
    &imagehost::IMGADULT,
    &imagehost::IMGSPICE,
    &imagehost::PIXHOST,
    &imagehost::TURBOIMAGEHOST,
    &imagehost::VIPR,
    &imagehost::IMGCLICK,
    &imagehost::FAPPIC,
    &imagehost::PICSTATE,
    &imagehost::IMGDRIVE,
    &imagehost::IMGTAXI,
    &imagehost::IMGWALLET,
    &imagehost::SILVERPIC,
    &imagehost::IMGPV,
    &urlshortener::BITLY,
    &urlshortener::TCO,
    &mixdrop::SITE,
    &webmshare::SITE,
    &imgth::SITE,
    &telegraph::SITE,
    &lolisafe::SITE,
    &urlgalleries::SITE,
    &imagebam::SITE,
    &imgpile::SITE,
    &imagechest::SITE,
    &lensdump::SITE,
    &koofr::SITE,
    &imagepond::SITE,
    &imageshack::SITE,
];

// ---------------------------------------------------------------------------
// gallery_dl.text equivalents

/// `text.extract`: the text between `begin` and `end` at or after `pos`,
/// and the position after `end`.
pub(super) fn extract<'t>(
    text: &'t str,
    begin: &str,
    end: &str,
    pos: usize,
) -> Option<(&'t str, usize)> {
    let start = text.get(pos..)?.find(begin)? + pos + begin.len();
    let stop = text[start..].find(end)? + start;
    Some((&text[start..stop], stop + end.len()))
}

/// `text.extract_iter`.
pub(super) fn extract_iter<'t>(text: &'t str, begin: &str, end: &str) -> Vec<&'t str> {
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some((value, next)) = extract(text, begin, end, pos) {
        out.push(value);
        pos = next;
    }
    out
}

/// `text.extract_from`: sequential extraction that keeps its position when
/// a lookup fails and returns an empty string instead.
pub(super) struct Extr<'t> {
    text: &'t str,
    pos: usize,
}

impl<'t> Extr<'t> {
    pub fn new(text: &'t str) -> Self {
        Self { text, pos: 0 }
    }
    pub fn next(&mut self, begin: &str, end: &str) -> &'t str {
        match extract(self.text, begin, end, self.pos) {
            Some((value, pos)) => {
                self.pos = pos;
                value
            }
            None => "",
        }
    }
}

/// `text.rextr`: the text after the last `begin` that fits before `pos`.
pub(super) fn rextr<'t>(text: &'t str, begin: &str, end: &str, pos: Option<usize>) -> &'t str {
    let limit = pos.unwrap_or(text.len()).min(text.len());
    let Some(head) = text.get(..limit) else {
        return "";
    };
    let Some(first) = head.rfind(begin).map(|i| i + begin.len()) else {
        return "";
    };
    match text[first..].find(end) {
        Some(stop) => &text[first..first + stop],
        None => "",
    }
}

/// Python `str.rpartition`.
pub(super) fn rpartition<'t>(text: &'t str, sep: &str) -> (&'t str, &'t str, &'t str) {
    match text.rfind(sep) {
        Some(i) => (&text[..i], &text[i..i + sep.len()], &text[i + sep.len()..]),
        None => ("", "", text),
    }
}

/// Python `str.partition`.
pub(super) fn partition<'t>(text: &'t str, sep: &str) -> (&'t str, &'t str, &'t str) {
    match text.find(sep) {
        Some(i) => (&text[..i], &text[i..i + sep.len()], &text[i + sep.len()..]),
        None => (text, "", ""),
    }
}

/// `text.parse_int`.
pub(super) fn parse_int(text: &str) -> i64 {
    text.trim().replace('_', "").parse().unwrap_or(0)
}

/// `text.parse_float`.
pub(super) fn parse_float(text: &str) -> f64 {
    let text = text.trim();
    if text.is_empty() {
        return 0.0;
    }
    text.parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
        .unwrap_or(0.0)
}

/// `text.parse_bytes` ("500k", "2.5M", ...).
pub(super) fn parse_bytes(text: &str) -> i64 {
    let text = text.trim();
    let Some(last) = text.chars().last() else {
        return 0;
    };
    let suffixes = "bkmgtp";
    let (number, factor) = match suffixes.find(last.to_ascii_lowercase()) {
        Some(i) => (
            &text[..text.len() - last.len_utf8()],
            1024f64.powi(i as i32),
        ),
        None => (text, 1.0),
    };
    match number.trim().parse::<f64>() {
        Ok(value) if value.is_finite() => (value * factor).round_ties_even() as i64,
        _ => 0,
    }
}

/// `text.nameext_from_name`.
pub(super) fn nameext_from_name(name: &str, data: &mut Map<String, Value>) {
    let (stem, sep, ext) = rpartition(name, ".");
    if !sep.is_empty() && !stem.is_empty() && ext.chars().count() <= 16 {
        data.insert("filename".into(), stem.into());
        data.insert("extension".into(), ext.to_lowercase().into());
    } else {
        data.insert("filename".into(), name.into());
        data.insert("extension".into(), "".into());
    }
}

/// `text.nameext_from_url` for arbitrary text.
pub(super) fn nameext_from_url(url: &str, data: &mut Map<String, Value>) {
    let path = partition(url, "?").0;
    let name = crate::sites::percent_decode(rpartition(path, "/").2);
    nameext_from_name(&name, data);
}

/// Parse an absolute HTTP(S) URL without ports or credentials.
pub(super) fn web_url(raw: &str) -> Option<Url> {
    Url::parse(raw.trim()).ok().filter(crate::sites::plain)
}

/// Strict host check: `host` itself or `www.host`, plain URL.
pub(super) fn on_host(url: &Url, hosts: &[&str]) -> bool {
    crate::sites::plain(url) && crate::sites::host_is(url, hosts)
}

// ---------------------------------------------------------------------------
// gallery_dl.dt equivalents

/// `dt.parse_iso` as Unix seconds.
pub(super) fn parse_iso(text: &str) -> Option<i64> {
    let text = text.trim();
    if text.len() < 10 || text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    crate::date::parse_date(text).ok()
}

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

/// A subset of Python `strptime` (`%Y %m %d %H %M %S %b %B`) as Unix
/// seconds, treating naive values as UTC.
pub(super) fn strptime(text: &str, format: &str) -> Option<i64> {
    let mut input = text;
    let (mut year, mut month, mut day, mut hour, mut minute, mut second) =
        (1900i32, 1u8, 1u8, 0u8, 0u8, 0u8);
    let mut chars = format.chars();
    let number = |input: &mut &str, max: usize| -> Option<i64> {
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
    };
    while let Some(c) = chars.next() {
        if c != '%' {
            if c.is_whitespace() {
                input = input.trim_start();
                continue;
            }
            input = input.strip_prefix(c)?;
            continue;
        }
        match chars.next()? {
            'Y' => year = number(&mut input, 4)? as i32,
            'm' => month = number(&mut input, 2)? as u8,
            'd' => day = number(&mut input, 2)? as u8,
            'H' => hour = number(&mut input, 2)? as u8,
            'M' => minute = number(&mut input, 2)? as u8,
            'S' => second = number(&mut input, 2)? as u8,
            spec @ ('b' | 'B') => {
                let lower = input.to_ascii_lowercase();
                let index = MONTHS.iter().position(|name| {
                    if spec == 'b' {
                        lower.starts_with(&name[..3])
                    } else {
                        lower.starts_with(name)
                    }
                })?;
                input = &input[if spec == 'b' { 3 } else { MONTHS[index].len() }..];
                month = index as u8 + 1;
            }
            '%' => input = input.strip_prefix('%')?,
            _ => return None,
        }
    }
    if !input.is_empty() {
        return None;
    }
    let date =
        time::Date::from_calendar_date(year, time::Month::try_from(month).ok()?, day).ok()?;
    let clock = time::Time::from_hms(hour, minute, second).ok()?;
    Some(
        time::PrimitiveDateTime::new(date, clock)
            .assume_utc()
            .unix_timestamp(),
    )
}

/// The upstream `date` keyword text for a parsed timestamp.
pub(super) fn date_value(timestamp: Option<i64>) -> Value {
    crate::filter::source_date_text(timestamp).into()
}

// ---------------------------------------------------------------------------
// gallery_dl.util equivalents

/// `util.decrypt_xor`: base64 (optionally hex inside) decoded, XORed text.
pub(super) fn decrypt_xor(encrypted: &str, key: &[u8], fromhex: bool) -> Option<String> {
    use base64::Engine;
    let cleaned: String = encrypted.split_whitespace().collect();
    let mut bytes = base64::engine::general_purpose::STANDARD
        .decode(cleaned)
        .ok()?;
    if fromhex {
        let hex = std::str::from_utf8(&bytes).ok()?.trim().to_owned();
        if hex.len() % 2 != 0 {
            return None;
        }
        bytes = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
            .collect::<Option<Vec<u8>>>()?;
    }
    if key.is_empty() {
        return None;
    }
    let out: Vec<u8> = bytes
        .iter()
        .enumerate()
        .map(|(i, b)| b ^ key[i % key.len()])
        .collect();
    String::from_utf8(out).ok()
}

/// A cheap pseudo-random index below `n`, for upstream `random.choice`.
pub(super) fn random_index(n: usize) -> usize {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as usize)
        .unwrap_or(0);
    if n == 0 { 0 } else { (nanos / 7) % n }
}

/// SHA-256 as lowercase hex.
pub(super) fn sha256_hex(data: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

// ---------------------------------------------------------------------------
// Event helpers

/// Lazily resolve one file per input, each as its own post boundary, in
/// order. A failed input yields its error and the remaining inputs continue,
/// as upstream logs per-file failures and moves on.
pub(super) fn each<'a, T, F, Fut>(
    site: &'static Site,
    http: &'a Http,
    route: &'static Route,
    inputs: Vec<T>,
    resolve: F,
) -> Events<'a>
where
    T: Send + 'a,
    F: FnMut(T) -> Fut + Send + 'a,
    Fut: std::future::Future<Output = Result<Option<Item>>> + Send + 'a,
{
    stream::iter(inputs)
        .then(resolve)
        .map(move |result| match result {
            Ok(Some(item)) => {
                crate::sites::events(crate::sites::post(site, http, route, vec![item]))
            }
            Ok(None) => crate::sites::events(Vec::new()),
            Err(error) => crate::sites::fail(error),
        })
        .flatten()
        .boxed()
}

/// Queue child URLs lazily, in order.
pub(super) fn queue_all<'a>(
    http: &'a Http,
    category: &'static str,
    subcategory: &'static str,
    children: Vec<(Url, Map<String, Value>)>,
) -> Events<'a> {
    stream::iter(children)
        .map(move |(url, data)| crate::sites::queue(http, category, subcategory, url, &data))
        .flatten()
        .boxed()
}

// ---------------------------------------------------------------------------
// Requests the shared client does not cover

const MAX_BODY: usize = 16 * 1024 * 1024;

/// A response that was not followed or interpreted beyond its status.
pub(super) struct Response {
    pub status: u16,
    pub location: Option<String>,
    pub cookies: Vec<String>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    pub fn json(&self, name: &str) -> Result<Value> {
        serde_json::from_slice(&self.body)
            .map_err(|_| error(format!("{name} returned invalid JSON")))
    }
    /// `name=value` pairs from `Set-Cookie`, joined for a `Cookie` header.
    pub fn cookie_header(&self) -> String {
        self.cookies
            .iter()
            .filter_map(|c| c.split(';').next())
            .map(str::trim)
            .filter(|c| c.contains('='))
            .collect::<Vec<_>>()
            .join("; ")
    }
}

/// Request body for [`request`].
pub(super) enum Body<'b> {
    None,
    Empty,
    Form(&'b [(&'b str, &'b str)]),
}

pub(super) fn user_agent(http: &Http) -> String {
    http.network
        .user_agent()
        .map(str::to_owned)
        .unwrap_or_else(|| concat!("gallery-dl-rs/", env!("CARGO_PKG_VERSION")).to_owned())
}

/// One request without following redirects. Redirects (3xx) are returned
/// to the caller; 4xx/5xx statuses become errors without URLs.
pub(super) async fn request(
    http: &Http,
    method: reqwest::Method,
    url: &Url,
    headers: &[(&str, &str)],
    body: Body<'_>,
) -> Result<Response> {
    let name = http.name();
    let client = http
        .network
        .client(&user_agent(http), Duration::from_secs(60))?;
    let mut request = client.request(method, url.clone());
    for (key, value) in headers {
        request = request.header(*key, *value);
    }
    request = match body {
        Body::None => request,
        Body::Empty => request.header("Content-Length", "0"),
        Body::Form(form) => {
            let encoded = url::form_urlencoded::Serializer::new(String::new())
                .extend_pairs(form)
                .finish();
            request
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(encoded)
        }
    };
    let mut response = request
        .send()
        .await
        .map_err(|_| error(format!("{name} request failed; check network access")))?;
    let status = response.status();
    match status.as_u16() {
        404 | 410 => return Err(error(format!("{name} resource not found"))),
        401 | 403 => {
            return Err(error(format!(
                "{name} refused access; the resource may need authentication"
            )));
        }
        429 => return Err(error(format!("{name} rate limit; retry later"))),
        400.. => return Err(error(format!("{name} returned HTTP {status}"))),
        _ => {}
    }
    let header = |key: reqwest::header::HeaderName| {
        response
            .headers()
            .get(key)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    let location = header(reqwest::header::LOCATION);
    let cookies = response
        .headers()
        .get_all(reqwest::header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok().map(str::to_owned))
        .collect();
    let headers = response.headers().clone();
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| error(format!("could not read {name} response")))?
    {
        if bytes.len() + chunk.len() > MAX_BODY {
            return Err(error(format!("{name} response exceeded 16 MiB")));
        }
        bytes.extend_from_slice(&chunk);
    }
    let body = crate::transport::decode_api_body(bytes, &headers, MAX_BODY).await?;
    Ok(Response {
        status: status.as_u16(),
        location,
        cookies,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_helpers_match_python() {
        let mut extr = Extr::new("a<1>b<2>c");
        assert_eq!(extr.next("<", ">"), "1");
        assert_eq!(extr.next("x", ">"), "");
        assert_eq!(extr.next("<", ">"), "2");
        assert_eq!(extract_iter("[a][b]", "[", "]"), vec!["a", "b"]);
        assert_eq!(rextr("x/a.b\"/c.d\"", "/", "\"", None), "c.d");
        assert_eq!(rextr("x/a.b\"/c.d\"", "/", "\"", Some(6)), "a.b");
        assert_eq!(rpartition("a-b-c", "-"), ("a-b", "-", "c"));
        assert_eq!(rpartition("abc", "-"), ("", "", "abc"));
        assert_eq!(parse_bytes("2.5M"), 2_621_440);
        assert_eq!(parse_bytes("12 K"), 12288);
        assert_eq!(parse_bytes("x"), 0);
        assert_eq!(parse_int(" 42 "), 42);
        let mut map = Map::new();
        nameext_from_name("Archive.TAR.GZ", &mut map);
        assert_eq!(map["filename"], "Archive.TAR");
        assert_eq!(map["extension"], "gz");
        nameext_from_url("https://x.test/a/b%20c.png?x=1", &mut map);
        assert_eq!(map["filename"], "b c");
    }

    #[test]
    fn dates_match_python() {
        assert_eq!(
            strptime("12:30:00 05/06/2024", "%H:%M:%S %d/%m/%Y"),
            Some(1_717_590_600)
        );
        assert_eq!(strptime("June 5, 2024", "%B %d, %Y"), Some(1_717_545_600));
        assert_eq!(strptime("Jun 5, 2024", "%b %d, %Y"), Some(1_717_545_600));
        assert_eq!(strptime("05.06.2024", "%d.%m.%Y"), Some(1_717_545_600));
        assert_eq!(strptime("bad", "%d.%m.%Y"), None);
        assert_eq!(parse_iso("2024-06-05 00:00:00"), Some(1_717_545_600));
        assert_eq!(parse_iso("2024-06-05T02:00:00+02:00"), Some(1_717_545_600));
        assert_eq!(parse_iso("1717545600"), None);
    }

    #[test]
    fn xor_decrypts() {
        use base64::Engine;
        let key = b"k3y";
        let plain = b"https://cdn.test/a.png";
        let xored: Vec<u8> = plain
            .iter()
            .enumerate()
            .map(|(i, b)| b ^ key[i % 3])
            .collect();
        let encoded = base64::engine::general_purpose::STANDARD.encode(&xored);
        assert_eq!(
            decrypt_xor(&encoded, key, false).unwrap(),
            "https://cdn.test/a.png"
        );
        let hex: String = xored.iter().map(|b| format!("{b:02x}")).collect();
        let encoded = base64::engine::general_purpose::STANDARD.encode(hex);
        assert_eq!(
            decrypt_xor(&encoded, key, true).unwrap(),
            "https://cdn.test/a.png"
        );
    }
}
