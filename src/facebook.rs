//! Experimental native Facebook photo extraction from identity-bound Relay JSON.
use crate::{
    Result,
    cookies::CookieJar,
    error,
    extract::{Media, PostMetadata, http_url},
    transport::{RateLimits, Redirects, retry_after},
};
use reqwest::{Client, StatusCode, header};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::Path,
    sync::Mutex,
    time::{Duration, SystemTime},
};
use url::Url;

const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
const MAX_RESPONSE: usize = 16 * 1024 * 1024;

pub struct Facebook {
    client: Client,
    cookies: Mutex<CookieJar>,
    timeout: Duration,
    request_timeout: Duration,
    retries: u8,
    rate_limits: RateLimits,
}

impl Facebook {
    pub async fn new(cookies: Option<&Path>, timeout: Duration) -> Result<Self> {
        let cookies = match cookies {
            Some(path) => CookieJar::parse(&tokio::fs::read_to_string(path).await?)?,
            None => CookieJar::parse("")?,
        };
        Ok(Self {
            client: Client::builder()
                .user_agent(USER_AGENT)
                .timeout(timeout)
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            cookies: Mutex::new(cookies),
            timeout,
            request_timeout: timeout,
            retries: 0,
            rate_limits: RateLimits::default(),
        })
    }

    /// Configure additional attempts for HTTP 429/5xx and transport failures.
    /// The extraction timeout covers all attempts and waits. Defaults to zero.
    pub fn with_retries(mut self, retries: u8) -> Self {
        self.retries = retries;
        self
    }

    /// Bound each HTTP request separately from the overall extraction deadline.
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    pub async fn extract(&self, post: &Url) -> Result<Vec<Media>> {
        let code = photo_id(post)?;
        let mut endpoint = http_url("https://www.facebook.com/photo/")?;
        endpoint
            .query_pairs_mut()
            .append_pair("fbid", &code)
            .append_pair("set", "");
        self.extract_endpoint(&endpoint, &code).await
    }

    async fn extract_endpoint(&self, endpoint: &Url, code: &str) -> Result<Vec<Media>> {
        tokio::time::timeout(self.timeout, async {
            for attempt in 0..=self.retries {
                match self.attempt(endpoint, code, attempt).await {
                    Ok(media) => return Ok(media),
                    Err(failure) if failure.retry && attempt < self.retries => {
                        // Transport failures have no response to establish a cooldown.
                        // Also yield between attempts when Retry-After is zero.
                        tokio::time::sleep(backoff(attempt)).await;
                    }
                    Err(failure) => return Err(failure.error),
                }
            }
            unreachable!()
        })
        .await
        .map_err(|_| error("native Facebook extraction timed out"))?
    }

    async fn attempt(
        &self,
        endpoint: &Url,
        code: &str,
        attempt: u8,
    ) -> std::result::Result<Vec<Media>, Failure> {
        let mut target = endpoint.clone();
        let mut redirects = Redirects::new(endpoint);
        let mut response = loop {
            self.rate_limits.wait(&target, self.timeout).await?;
            let mut request = self
                .client
                .get(target.clone())
                .timeout(self.request_timeout)
                .header(header::ACCEPT, "text/html,application/xhtml+xml")
                .header(header::REFERER, "https://www.facebook.com/");
            if let Some(cookies) = self.cookies.lock().unwrap().header(&target) {
                request = request.header(header::COOKIE, cookies);
            }
            let response = request
                .send()
                .await
                .map_err(|_| Failure::transient("Facebook request failed; check network access"))?;
            self.cookies
                .lock()
                .unwrap()
                .store_response(response.url(), response.headers());
            match redirects.next(response.url(), response.status(), response.headers())? {
                Some(next) => {
                    if matches!(
                        next.path().trim_end_matches('/'),
                        "/login" | "/login.php" | "/checkpoint" | "/recover"
                    ) {
                        return Err(error("Facebook authentication required or session challenge; refresh --cookies").into());
                    }
                    if !matches!(next.path().trim_end_matches('/'), "/photo" | "/photo.php") {
                        return Err(error(
                            "Facebook redirected outside the photo endpoint; login may be required",
                        )
                        .into());
                    }
                    target = next;
                }
                None => break response,
            }
        };
        let status = response.status();
        if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
            let delay = retry_after(response.headers(), SystemTime::now());
            // Keep final-attempt cooldowns for subsequent posts on this instance.
            self.rate_limits
                .defer(endpoint, delay.unwrap_or_else(|| backoff(attempt)));
            return Err(Failure::transient(
                if status == StatusCode::TOO_MANY_REQUESTS {
                    match delay {
                        Some(delay) => format!(
                            "Facebook rate limit; retry after {} seconds",
                            delay.as_secs()
                        ),
                        None => "Facebook rate limit; retry later".into(),
                    }
                } else {
                    format!("Facebook returned HTTP {status}")
                },
            ));
        }
        if matches!(status.as_u16(), 401 | 403) {
            return Err(error(
                "Facebook authentication required or session expired; refresh --cookies",
            )
            .into());
        }
        if matches!(status.as_u16(), 404 | 410) {
            return Err(error("Facebook post unavailable or inaccessible to this session").into());
        }
        if !status.is_success() {
            return Err(error(format!("Facebook returned HTTP {status}")).into());
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| Failure::transient("could not read Facebook response"))?
        {
            if body.len() + chunk.len() > MAX_RESPONSE {
                return Err(error("Facebook response exceeded 16 MiB").into());
            }
            body.extend_from_slice(&chunk);
        }
        parse_photo(&body, code).map_err(Into::into)
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

/// Accept single-photo permalinks only. Collection URLs must remain explicit.
pub fn photo_id(url: &Url) -> Result<String> {
    if !matches!(url.scheme(), "http" | "https")
        || !matches!(
            url.host_str(),
            Some("facebook.com" | "www.facebook.com" | "m.facebook.com")
        )
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return Err(error("native extraction requires a Facebook photo URL"));
    }
    if url.query_pairs().any(|(key, _)| key == "setextract") {
        return Err(error(
            "native Facebook album traversal is not supported; use --extractor compatibility",
        ));
    }
    let path = url.path().trim_end_matches('/');
    let id = if matches!(path, "/photo" | "/photo.php") {
        let ids: Vec<_> = url
            .query_pairs()
            .filter(|(key, _)| key == "fbid")
            .map(|(_, value)| value.into_owned())
            .collect();
        if ids.len() != 1 {
            return Err(error("Facebook photo URL requires exactly one fbid"));
        }
        ids[0].clone()
    } else {
        let parts: Vec<_> = path.split('/').collect();
        if parts.len() != 5 || parts[1].is_empty() || parts[2] != "photos" || parts[3].is_empty() {
            return Err(error(
                "native Facebook supports single photo permalinks; use --extractor compatibility for other URLs",
            ));
        }
        parts[4].into()
    };
    validate_id(&id)?;
    Ok(id)
}

fn validate_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 32 || !id.bytes().all(|c| c.is_ascii_digit()) {
        return Err(error("invalid Facebook photo identifier"));
    }
    Ok(())
}

/// Parse JSON script payloads, never a page-wide image/OG preview match.
/// A photo must identify the exact requested ID and own its `image.uri`.
pub fn parse_photo(bytes: &[u8], requested_id: &str) -> Result<Vec<Media>> {
    validate_id(requested_id)?;
    if bytes.len() > MAX_RESPONSE {
        return Err(error("Facebook response exceeded 16 MiB"));
    }
    let page = std::str::from_utf8(bytes).map_err(|_| error("Facebook returned invalid UTF-8"))?;
    let mut payloads = Vec::new();
    if let Ok(value) = serde_json::from_str::<Value>(page) {
        payloads.push(value);
    } else {
        // Script contents are raw text in HTML. JSON escapes (including slash
        // and surrogate pairs) are decoded exactly once by serde_json.
        let lower = page.to_ascii_lowercase();
        let mut offset = 0;
        while let Some(start) = lower[offset..].find("<script") {
            let start = offset + start + 7;
            if !lower
                .as_bytes()
                .get(start)
                .is_some_and(|b| b.is_ascii_whitespace() || *b == b'>')
            {
                offset = start;
                continue;
            }
            let Some(open) = lower[start..].find('>') else {
                break;
            };
            let content = start + open + 1;
            let Some(end) = lower[content..].find("</script") else {
                break;
            };
            let end = content + end;
            if let Ok(value) = serde_json::from_str::<Value>(page[content..end].trim()) {
                payloads.push(value);
            }
            offset = end + 8;
        }
    }
    let mut photos = Vec::new();
    for payload in &payloads {
        find_photos(payload, requested_id, &mut photos, 0);
    }
    let mut candidates = Vec::new();
    for photo in photos {
        let Some(uri) = photo.pointer("/image/uri").and_then(Value::as_str) else {
            continue;
        };
        let url = http_url(uri).map_err(|_| error("Facebook photo has an invalid image URL"))?;
        let extension = match url
            .path()
            .rsplit('.')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str()
        {
            "jpg" | "jpeg" => "jpg",
            "png" => "png",
            "webp" => "webp",
            "avif" => "avif",
            _ => {
                return Err(error(
                    "Facebook photo image has no supported file extension",
                ));
            }
        };
        let area = u128::from(
            photo
                .pointer("/image/width")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        ) * u128::from(
            photo
                .pointer("/image/height")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        );
        let media = Media {
            url,
            job: crate::extract::MediaJob::DirectHttp,
            category: "facebook".into(),
            filename: format!("{requested_id}.{extension}"),
            handoff: BTreeMap::new(),
            metadata: Some(PostMetadata {
                post_id: requested_id.into(),
                media_id: format!("{requested_id}.{extension}"),
                shortcode: String::new(),
                username: photo
                    .pointer("/owner/username")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned),
                caption: photo
                    .pointer("/creation_story/message/text")
                    .or_else(|| photo.pointer("/message/text"))
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                timestamp: photo.get("created_time").and_then(Value::as_i64),
                index: 1,
            }),
            headers: BTreeMap::from([
                ("User-Agent".into(), USER_AGENT.into()),
                ("Referer".into(), "https://www.facebook.com/".into()),
            ]),
        };
        candidates.push((area, media));
    }
    candidates.sort_by_key(|(area, _)| std::cmp::Reverse(*area));
    let Some((area, chosen)) = candidates.first() else {
        if payloads.iter().any(has_error_root) {
            return Err(error(
                "Facebook temporarily blocked photo access; retry later",
            ));
        }
        if page.contains("login_form")
            || page.contains("login.php?next=")
            || page.contains("checkpoint_url")
        {
            return Err(error(
                "Facebook authentication required or session challenge; refresh --cookies",
            ));
        }
        return Err(error(
            "Facebook requested photo unavailable or missing identity-bound media; login may be required",
        ));
    };
    if candidates
        .iter()
        .skip(1)
        .any(|(other_area, other)| other_area == area && other.url != chosen.url)
    {
        return Err(error(
            "Facebook returned ambiguous images for the requested photo",
        ));
    }
    Ok(vec![chosen.clone()])
}

fn has_error_root(value: &Value) -> bool {
    match value {
        Value::Object(object) => {
            object.get("__dr").and_then(Value::as_str) == Some("CometErrorRoot.react")
                || object.values().any(has_error_root)
        }
        Value::Array(array) => array.iter().any(has_error_root),
        _ => false,
    }
}

fn find_photos<'a>(value: &'a Value, id: &str, photos: &mut Vec<&'a Value>, depth: usize) {
    if depth > 100 {
        return;
    }
    match value {
        Value::Object(object) => {
            if (object.get("__isNode").and_then(Value::as_str) == Some("Photo")
                || object.get("__typename").and_then(Value::as_str) == Some("Photo"))
                && object.get("id").and_then(Value::as_str) == Some(id)
            {
                photos.push(value);
            }
            for child in object.values() {
                find_photos(child, id, photos, depth + 1);
            }
        }
        Value::Array(array) => {
            for child in array {
                find_photos(child, id, photos, depth + 1);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod transport_tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread::{self, JoinHandle},
    };

    const PHOTO: &[u8] = include_bytes!("../tests/fixtures/facebook/photo.html");

    #[tokio::test]
    async fn request_timeout_is_separate_from_the_overall_extraction_deadline() {
        let (endpoint, server) = serve_once("200 OK", "", PHOTO, Duration::from_millis(200));
        let native = Facebook::new(None, Duration::from_secs(3))
            .await
            .unwrap()
            .with_request_timeout(Duration::from_millis(50));
        let message = native
            .extract_endpoint(&endpoint, "10160743390456729")
            .await
            .unwrap_err()
            .to_string();
        assert!(message.contains("Facebook request failed"), "{message}");
        server.join().unwrap();
    }

    fn serve_once(
        status: &str,
        extra_headers: &str,
        body: &[u8],
        delay: Duration,
    ) -> (Url, JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint =
            Url::parse(&format!("http://{}/photo/", listener.local_addr().unwrap())).unwrap();
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{extra_headers}\r\n",
            body.len()
        );
        let body = body.to_vec();
        listener.set_nonblocking(true).unwrap();
        let task = thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(std::time::Instant::now() < deadline, "server timed out");
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(e) => panic!("{e}"),
                }
            };
            // Windows can inherit the listener's nonblocking mode.
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let count = stream.read(&mut buffer).unwrap();
                assert_ne!(count, 0, "client closed before completing request headers");
                request.extend_from_slice(&buffer[..count]);
                assert!(request.len() < 64 * 1024);
            }
            thread::sleep(delay);
            // A timed-out client can disconnect before the response is written.
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.write_all(&body);
            String::from_utf8(request).unwrap()
        });
        (endpoint, task)
    }

    // Raw responses also allow exercising connection loss and truncated bodies.
    type Arrivals = Vec<(std::time::Instant, SystemTime, String)>;

    fn serve_sequence(responses: Vec<Vec<u8>>) -> (Url, JoinHandle<Arrivals>) {
        serve_delayed_sequence(
            responses
                .into_iter()
                .map(|body| (body, Duration::ZERO))
                .collect(),
        )
    }

    fn serve_delayed_sequence(responses: Vec<(Vec<u8>, Duration)>) -> (Url, JoinHandle<Arrivals>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint =
            Url::parse(&format!("http://{}/photo/", listener.local_addr().unwrap())).unwrap();
        let task = thread::spawn(move || {
            let mut arrivals = Vec::new();
            for (response, delay) in responses {
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(std::time::Instant::now() < deadline, "server timed out");
                            thread::sleep(Duration::from_millis(2));
                        }
                        Err(e) => panic!("{e}"),
                    }
                };
                let arrival = (std::time::Instant::now(), SystemTime::now());
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 1024];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    let count = stream.read(&mut buffer).unwrap();
                    assert_ne!(count, 0);
                    request.extend_from_slice(&buffer[..count]);
                    assert!(request.len() < 64 * 1024);
                }
                arrivals.push((arrival.0, arrival.1, String::from_utf8(request).unwrap()));
                thread::sleep(delay);
                let _ = stream.write_all(&response);
            }
            arrivals
        });
        (endpoint, task)
    }

    fn response(status: &str, headers: &str, body: &[u8]) -> Vec<u8> {
        let mut response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(body);
        response
    }

    #[tokio::test]
    async fn retries_transient_statuses_and_transport_failures() {
        for transient in [
            response("429 Too Many Requests", "Retry-After: 0\r\n", b""),
            response("503 Service Unavailable", "Retry-After: invalid\r\n", b""),
            Vec::new(), // Connection closed before response headers.
            b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\n{".to_vec(),
        ] {
            let (endpoint, server) = serve_sequence(vec![transient, response("200 OK", "", PHOTO)]);
            let facebook = Facebook::new(None, Duration::from_secs(3))
                .await
                .unwrap()
                .with_retries(1);
            assert_eq!(
                facebook
                    .extract_endpoint(&endpoint, "10160743390456729")
                    .await
                    .unwrap()
                    .len(),
                1
            );
            let arrivals = server.join().unwrap();
            assert!(arrivals[1].0.duration_since(arrivals[0].0) >= Duration::from_millis(250));
        }
    }

    #[tokio::test]
    async fn honors_numeric_and_http_date_cooldowns() {
        for use_date in [false, true] {
            let value = if use_date {
                httpdate::fmt_http_date(SystemTime::now() + Duration::from_secs(2))
            } else {
                "1".to_owned()
            };
            let (endpoint, server) = serve_sequence(vec![
                response(
                    "429 Too Many Requests",
                    &format!("Retry-After: {value}\r\n"),
                    b"",
                ),
                response("200 OK", "", PHOTO),
            ]);
            let facebook = Facebook::new(None, Duration::from_secs(4))
                .await
                .unwrap()
                .with_retries(1);
            facebook
                .extract_endpoint(&endpoint, "10160743390456729")
                .await
                .unwrap();
            let arrivals = server.join().unwrap();
            if use_date {
                assert!(arrivals[1].1 >= httpdate::parse_http_date(&value).unwrap());
            } else {
                assert!(arrivals[1].0.duration_since(arrivals[0].0) >= Duration::from_secs(1));
            }
        }
    }

    #[tokio::test]
    async fn retains_final_attempt_cooldown_for_the_next_post() {
        let (endpoint, server) = serve_sequence(vec![
            response("429 Too Many Requests", "Retry-After: 1\r\n", b""),
            response("200 OK", "", PHOTO),
        ]);
        let facebook = Facebook::new(None, Duration::from_secs(3)).await.unwrap();
        assert!(
            facebook
                .extract_endpoint(&endpoint, "10160743390456729")
                .await
                .unwrap_err()
                .to_string()
                .contains("rate limit")
        );
        let mut next = endpoint.clone();
        next.set_path("/photo.php/");
        facebook
            .extract_endpoint(&next, "10160743390456729")
            .await
            .unwrap();
        let arrivals = server.join().unwrap();
        assert!(arrivals[1].0.duration_since(arrivals[0].0) >= Duration::from_secs(1));
    }

    #[tokio::test]
    async fn bounds_retries_and_does_not_retry_terminal_errors() {
        let (endpoint, server) =
            serve_sequence(vec![response("503 Service Unavailable", "", b""); 2]);
        let facebook = Facebook::new(None, Duration::from_secs(3))
            .await
            .unwrap()
            .with_retries(1);
        assert!(
            facebook
                .extract_endpoint(&endpoint, "10160743390456729")
                .await
                .unwrap_err()
                .to_string()
                .contains("503")
        );
        assert_eq!(server.join().unwrap().len(), 2);
        for (status, body, expected) in [
            ("401 Unauthorized", b"".as_slice(), "authentication"),
            ("403 Forbidden", b"".as_slice(), "authentication"),
            ("404 Not Found", b"".as_slice(), "unavailable"),
            ("410 Gone", b"".as_slice(), "unavailable"),
            ("302 Found", b"".as_slice(), "redirect"),
            ("400 Bad Request", b"".as_slice(), "400"),
            ("200 OK", b"<html>login</html>".as_slice(), "unavailable"),
            (
                "200 OK",
                br#"{"__dr":"CometErrorRoot.react"}"#.as_slice(),
                "blocked",
            ),
        ] {
            let (endpoint, server) = serve_sequence(vec![response(status, "", body)]);
            let facebook = Facebook::new(None, Duration::from_secs(3))
                .await
                .unwrap()
                .with_retries(2);
            assert!(
                facebook
                    .extract_endpoint(&endpoint, "10160743390456729")
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains(expected)
            );
            assert_eq!(server.join().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn bounds_total_retry_time_and_rejects_excessive_cooldowns() {
        let (endpoint, server) =
            serve_sequence(vec![response("503 Service Unavailable", "", b""); 3]);
        let facebook = Facebook::new(None, Duration::from_millis(1500))
            .await
            .unwrap()
            .with_retries(10);
        let start = std::time::Instant::now();
        let error = facebook
            .extract_endpoint(&endpoint, "10160743390456729")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("timed out"), "{error}");
        assert!(start.elapsed() < Duration::from_secs(3));
        assert_eq!(server.join().unwrap().len(), 3);

        let (endpoint, server) = serve_sequence(vec![response(
            "429 Too Many Requests",
            "Retry-After: 999999999999999999999999\r\n",
            b"",
        )]);
        let facebook = Facebook::new(None, Duration::from_secs(1))
            .await
            .unwrap()
            .with_retries(1);
        let error = facebook
            .extract_endpoint(&endpoint, "10160743390456729")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("retry later"), "{error}");
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn cancellation_stops_cooldown_wait_without_losing_the_deadline() {
        let (endpoint, server) = serve_sequence(vec![
            response("429 Too Many Requests", "Retry-After: 1\r\n", b""),
            response("200 OK", "", PHOTO),
        ]);
        let facebook = Facebook::new(None, Duration::from_secs(3))
            .await
            .unwrap()
            .with_retries(1);
        assert!(
            tokio::time::timeout(
                Duration::from_millis(400),
                facebook.extract_endpoint(&endpoint, "10160743390456729")
            )
            .await
            .is_err()
        );
        // Dropping the previous future must not leave a worker making retries,
        // and a new call must still honor the previously observed cooldown.
        facebook
            .extract_endpoint(&endpoint, "10160743390456729")
            .await
            .unwrap();
        let arrivals = server.join().unwrap();
        assert_eq!(arrivals.len(), 2);
        assert!(arrivals[1].0.duration_since(arrivals[0].0) >= Duration::from_secs(1));
    }

    #[tokio::test]
    async fn redirects_update_and_reselect_scoped_cookies_without_forwarding_to_media() {
        let (endpoint, server) = serve_sequence(vec![
            response(
                "302 Found",
                "Location: /photo.php/?fbid=10160743390456729\r\nSet-Cookie: session=rotated; Path=/\r\n",
                b"",
            ),
            response(
                "200 OK",
                "Set-Cookie: local=; Path=/photo.php/; Max-Age=0\r\n",
                PHOTO,
            ),
            response("200 OK", "", PHOTO),
        ]);
        let mut native = Facebook::new(None, Duration::from_secs(3)).await.unwrap();
        native.cookies = CookieJar::parse(concat!(
            "127.0.0.1\tFALSE\t/\tFALSE\t0\tsession\tinitial\n",
            "127.0.0.1\tFALSE\t/photo.php/\tFALSE\t0\tlocal\tpath-only\n",
            "elsewhere.test\tFALSE\t/\tFALSE\t0\tforeign\texcluded\n",
        ))
        .unwrap()
        .into();
        let media = native
            .extract_endpoint(&endpoint, "10160743390456729")
            .await
            .unwrap();
        assert!(!media[0].headers.contains_key("Cookie"));
        let mut next = endpoint.clone();
        next.set_path("/photo.php/");
        native
            .extract_endpoint(&next, "10160743390456729")
            .await
            .unwrap();
        let arrivals = server.join().unwrap();
        let first = arrivals[0].2.to_ascii_lowercase();
        assert!(first.contains("session=initial"));
        assert!(!first.contains("path-only"));
        let redirect = arrivals[1].2.to_ascii_lowercase();
        assert!(redirect.contains("cookie: local=path-only; session=rotated\r\n"));
        assert!(!arrivals[2].2.contains("path-only"));
        for (_, _, request) in arrivals {
            assert!(!request.contains("excluded"));
        }
    }

    #[tokio::test]
    async fn refuses_unsafe_login_and_loop_redirects_without_disclosing_locations() {
        for (location, expected) in [
            ("https://foreign.test/secret", "same origin"),
            ("/login/?next=secret", "authentication"),
            ("/checkpoint/?secret", "challenge"),
            ("/photo/#secret", "loop"),
            ("/unexpected/secret", "outside"),
        ] {
            let (endpoint, server) = serve_sequence(vec![response(
                "302 Found",
                &format!("Location: {location}\r\n"),
                b"",
            )]);
            let native = Facebook::new(None, Duration::from_secs(2))
                .await
                .unwrap()
                .with_retries(2);
            let message = native
                .extract_endpoint(&endpoint, "10160743390456729")
                .await
                .unwrap_err()
                .to_string();
            assert!(message.contains(expected), "{message}");
            assert!(!message.contains("secret"));
            assert_eq!(server.join().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn limits_response_size_without_retrying_oversized_payloads() {
        let body = vec![b'x'; MAX_RESPONSE + 1];
        let (endpoint, server) = serve_sequence(vec![response("200 OK", "", &body)]);
        let native = Facebook::new(None, Duration::from_secs(3))
            .await
            .unwrap()
            .with_retries(1);
        let message = native
            .extract_endpoint(&endpoint, "10160743390456729")
            .await
            .unwrap_err()
            .to_string();
        assert!(message.contains("exceeded 16 MiB"), "{message}");
        assert_eq!(server.join().unwrap().len(), 1);
    }
}
