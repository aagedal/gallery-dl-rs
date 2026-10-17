//! Experimental native single-post extraction using Instagram's web REST API.
use crate::{
    Result,
    cookies::CookieJar,
    error,
    extract::{Media, PostMetadata, http_url},
};
use reqwest::{Client, StatusCode, header};
use serde_json::Value;
use std::{collections::BTreeMap, path::Path, time::Duration};
use url::Url;

const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
const MAX_RESPONSE: usize = 16 * 1024 * 1024;

pub struct Instagram {
    client: Client,
    cookies: CookieJar,
    timeout: Duration,
}

impl Instagram {
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
            cookies,
            timeout,
        })
    }

    pub async fn extract(&self, post: &Url) -> Result<Vec<Media>> {
        let code = shortcode(post)?;
        let id = media_id(&code)?;
        let endpoint = http_url(&format!(
            "https://www.instagram.com/api/v1/media/{id}/info/"
        ))?;
        self.extract_endpoint(&endpoint, &code).await
    }

    async fn extract_endpoint(&self, endpoint: &Url, code: &str) -> Result<Vec<Media>> {
        tokio::time::timeout(self.timeout, async {
            let mut request = self.client.get(endpoint.clone())
                .header(header::ACCEPT, "application/json")
                .header(header::REFERER, format!("https://www.instagram.com/p/{code}/"))
                .header("X-IG-App-ID", "936619743392459")
                .header("X-Requested-With", "XMLHttpRequest")
                .header("X-IG-WWW-Claim", "0");
            if let Some(cookies) = self.cookies.header(endpoint) {
                if let Some(token) = cookies.split("; ").find_map(|s| s.strip_prefix("csrftoken=")) {
                    request = request.header("X-CSRFToken", token);
                }
                request = request.header(header::COOKIE, cookies);
            }
            let mut response = request.send().await
                .map_err(|_| error("Instagram request failed; check network access"))?;
            let status = response.status();
            if status == StatusCode::TOO_MANY_REQUESTS {
                let retry = response.headers().get(header::RETRY_AFTER)
                    .and_then(|s| s.to_str().ok()).and_then(|s| s.parse::<u64>().ok());
                return Err(error(match retry {
                    Some(seconds) => format!("Instagram rate limit; retry after {seconds} seconds"),
                    None => "Instagram rate limit; retry later".into(),
                }));
            }
            if status.is_redirection() {
                return Err(error("Instagram redirected the request; login or a session challenge may be required (redirects are not followed)"));
            }
            if matches!(status.as_u16(), 401 | 403) {
                return Err(error("Instagram authentication required or session expired; refresh --cookies"));
            }
            if matches!(status.as_u16(), 404 | 410) {
                return Err(error("Instagram post unavailable or inaccessible to this session"));
            }
            if !status.is_success() {
                return Err(error(format!("Instagram returned HTTP {status}")));
            }
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await
                .map_err(|_| error("could not read Instagram response"))? {
                if body.len() + chunk.len() > MAX_RESPONSE {
                    return Err(error("Instagram response exceeded 16 MiB"));
                }
                body.extend_from_slice(&chunk);
            }
            parse_response(&body, code)
        }).await.map_err(|_| error("native Instagram extraction timed out"))?
    }
}

pub fn shortcode(url: &Url) -> Result<String> {
    if !matches!(url.scheme(), "http" | "https")
        || !matches!(
            url.host_str(),
            Some("instagram.com" | "www.instagram.com" | "m.instagram.com")
        )
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return Err(error("native extraction requires an Instagram post URL"));
    }
    let parts: Vec<_> = url.path().trim_end_matches('/').split('/').collect();
    if parts.len() != 3 || !matches!(parts[1], "p" | "reel" | "tv") {
        return Err(error(
            "native Instagram supports /p/, /reel/, and /tv/ single posts; use --extractor compatibility for other URLs",
        ));
    }
    media_id(parts[2])?;
    Ok(parts[2].into())
}

fn media_id(code: &str) -> Result<u64> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let invalid = || error("invalid Instagram post shortcode");
    if code.is_empty() || code.len() > 11 {
        return Err(invalid());
    }
    code.bytes().try_fold(0u64, |id, c| {
        let digit = ALPHABET.iter().position(|&v| v == c).ok_or_else(invalid)? as u64;
        id.checked_mul(64)
            .and_then(|v| v.checked_add(digit))
            .ok_or_else(invalid)
    })
}

/// Parse a complete REST media-info response, failing the whole carousel if any
/// child is missing. A video thumbnail is never substituted for missing video.
pub fn parse_response(bytes: &[u8], code: &str) -> Result<Vec<Media>> {
    let data: Value = serde_json::from_slice(bytes).map_err(|_| {
        error("Instagram returned a web page or invalid JSON; login may be required")
    })?;
    let message = data.get("message").and_then(Value::as_str).unwrap_or("");
    if data.get("challenge").is_some()
        || matches!(
            message,
            "login_required" | "challenge_required" | "checkpoint_required"
        )
    {
        return Err(error(
            "Instagram authentication required or session expired; refresh --cookies and resolve any session challenge",
        ));
    }
    if matches!(
        message,
        "Please wait a few minutes before you try again."
            | "feedback_required"
            | "rate_limit_error"
    ) {
        return Err(error("Instagram rate limit; retry later"));
    }
    if data.get("status").and_then(Value::as_str) != Some("ok") {
        return Err(error(
            "Instagram could not return this post; it may be unavailable or inaccessible",
        ));
    }
    let posts = data
        .get("items")
        .and_then(Value::as_array)
        .filter(|items| items.len() == 1)
        .ok_or_else(|| error("Instagram post unavailable or unexpected media-info response"))?;
    let post = &posts[0];
    if post.get("code").and_then(Value::as_str) != Some(code) {
        return Err(error(
            "Instagram response does not match the requested post",
        ));
    }
    let post_id = identifier(post.get("pk"))?;
    let kind = post.get("media_type").and_then(Value::as_u64);
    let children: Vec<&Value> = if kind == Some(8) {
        let children = post
            .get("carousel_media")
            .and_then(Value::as_array)
            .filter(|items| !items.is_empty())
            .ok_or_else(|| error("Instagram carousel has no media"))?;
        if post
            .get("carousel_media_count")
            .and_then(Value::as_u64)
            .is_some_and(|count| count != children.len() as u64)
        {
            return Err(error("Instagram returned an incomplete carousel"));
        }
        children.iter().collect()
    } else {
        vec![post]
    };
    children
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let kind = item.get("media_type").and_then(Value::as_u64);
            let candidates = match kind {
                Some(1) => item.pointer("/image_versions2/candidates"),
                Some(2) => item.get("video_versions"),
                _ => return Err(error("unsupported Instagram media type")),
            }
            .and_then(Value::as_array)
            .ok_or_else(|| error("Instagram media variants are missing"))?;
            let variant = candidates
                .iter()
                .filter(|v| v.get("url").and_then(Value::as_str).is_some())
                .max_by_key(|v| {
                    u128::from(v.get("width").and_then(Value::as_u64).unwrap_or(0))
                        * u128::from(v.get("height").and_then(Value::as_u64).unwrap_or(0))
                })
                .ok_or_else(|| error("Instagram has no direct media variant"))?;
            let url = http_url(variant["url"].as_str().unwrap())?;
            if url.path().ends_with(".m3u8") || url.path().ends_with(".mpd") {
                return Err(error(
                    "Instagram returned a streaming manifest instead of direct media",
                ));
            }
            let media_id = identifier(item.get("pk"))?;
            let extension = if kind == Some(2) {
                "mp4"
            } else {
                match url.path().rsplit('.').next().unwrap_or("") {
                    "png" => "png",
                    "webp" => "webp",
                    "avif" => "avif",
                    "heic" => "heic",
                    _ => "jpg",
                }
            };
            Ok(Media {
                url,
                category: "instagram".into(),
                filename: format!("{post_id}_{media_id}.{extension}"),
                metadata: Some(PostMetadata {
                    post_id: post_id.clone(),
                    media_id,
                    shortcode: code.into(),
                    username: post
                        .pointer("/user/username")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    caption: post
                        .pointer("/caption/text")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    timestamp: post.get("taken_at").and_then(Value::as_i64),
                    index: index + 1,
                }),
                headers: BTreeMap::from([
                    ("User-Agent".into(), USER_AGENT.into()),
                    ("Referer".into(), "https://www.instagram.com/".into()),
                ]),
            })
        })
        .collect()
}

fn identifier(value: Option<&Value>) -> Result<String> {
    let value = match value {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) if n.is_u64() => n.to_string(),
        _ => return Err(error("Instagram media identifier is missing")),
    };
    if value.is_empty() || !value.bytes().all(|c| c.is_ascii_digit()) {
        return Err(error("invalid Instagram media identifier"));
    }
    Ok(value)
}

#[cfg(test)]
mod transport_tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread::{self, JoinHandle},
    };

    const PHOTO: &[u8] = include_bytes!("../tests/fixtures/instagram/photo.json");

    fn serve_once(
        status: &str,
        extra_headers: &str,
        body: &[u8],
        delay: Duration,
    ) -> (Url, JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = Url::parse(&format!(
            "http://{}/api/v1/media/64/info/",
            listener.local_addr().unwrap()
        ))
        .unwrap();
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

    #[tokio::test]
    async fn sends_api_headers_and_scoped_cookies_then_extracts_fixture() {
        let (endpoint, server) = serve_once("200 OK", "", PHOTO, Duration::ZERO);
        let mut instagram = Instagram::new(None, Duration::from_secs(2)).await.unwrap();
        instagram.cookies = CookieJar::parse(concat!(
            "127.0.0.1\tFALSE\t/api/\tFALSE\t0\tcsrftoken\tlocal-token\n",
            "127.0.0.1\tFALSE\t/\tFALSE\t0\tsessionid\tlocal-session\n",
            "127.0.0.1\tFALSE\t/elsewhere\tFALSE\t0\twrongpath\texcluded\n",
            "127.0.0.1\tFALSE\t/\tTRUE\t0\tsecure\texcluded\n",
            ".instagram.com\tTRUE\t/\tFALSE\t0\tforeign\texcluded\n",
        ))
        .unwrap();

        let media = instagram.extract_endpoint(&endpoint, "BA").await.unwrap();
        assert_eq!(media.len(), 1);
        assert_eq!(media[0].url.as_str(), "https://cdn.example/64.jpg");
        assert!(!media[0].headers.keys().any(
            |key| key.eq_ignore_ascii_case("cookie") || key.eq_ignore_ascii_case("x-csrftoken")
        ));
        let request = server.join().unwrap().to_ascii_lowercase();
        assert!(request.starts_with("get /api/v1/media/64/info/ http/1.1\r\n"));
        for header in [
            "accept: application/json\r\n",
            "referer: https://www.instagram.com/p/ba/\r\n",
            "x-ig-app-id: 936619743392459\r\n",
            "x-requested-with: xmlhttprequest\r\n",
            "x-ig-www-claim: 0\r\n",
            "x-csrftoken: local-token\r\n",
            "cookie: csrftoken=local-token; sessionid=local-session\r\n",
        ] {
            assert!(
                request.contains(header),
                "missing expected header {header:?}"
            );
        }
        assert!(request.contains(&format!(
            "user-agent: {}\r\n",
            USER_AGENT.to_ascii_lowercase()
        )));
        assert!(!request.contains("excluded"));
    }

    #[tokio::test]
    async fn refuses_redirect_without_forwarding_request_or_credentials() {
        let target = TcpListener::bind("127.0.0.1:0").unwrap();
        target.set_nonblocking(true).unwrap();
        let location = format!(
            "Location: http://{}/stolen\r\n",
            target.local_addr().unwrap()
        );
        let (endpoint, server) = serve_once("302 Found", &location, b"", Duration::ZERO);
        let mut instagram = Instagram::new(None, Duration::from_secs(2)).await.unwrap();
        instagram.cookies =
            CookieJar::parse("127.0.0.1\tFALSE\t/\tFALSE\t0\tsessionid\tsecret\n").unwrap();
        let error = instagram
            .extract_endpoint(&endpoint, "BA")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("redirects are not followed"));
        assert!(!error.contains("secret"));
        assert!(server.join().unwrap().contains("sessionid=secret"));
        assert_eq!(
            target.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[tokio::test]
    async fn reports_authentication_unavailable_and_rate_limit_statuses() {
        for (status, headers, expected) in [
            (
                "401 Unauthorized",
                "",
                "authentication required or session expired",
            ),
            (
                "403 Forbidden",
                "",
                "authentication required or session expired",
            ),
            ("404 Not Found", "", "post unavailable or inaccessible"),
            (
                "429 Too Many Requests",
                "Retry-After: 37\r\n",
                "rate limit; retry after 37 seconds",
            ),
        ] {
            let (endpoint, server) = serve_once(status, headers, b"", Duration::ZERO);
            let instagram = Instagram::new(None, Duration::from_secs(2)).await.unwrap();
            let error = instagram
                .extract_endpoint(&endpoint, "BA")
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains(expected), "unexpected diagnostic: {error}");
            server.join().unwrap();
        }
    }

    #[tokio::test]
    async fn bounds_slow_response_time() {
        let (endpoint, server) = serve_once("200 OK", "", PHOTO, Duration::from_millis(250));
        let instagram = Instagram::new(None, Duration::from_millis(50))
            .await
            .unwrap();
        let start = std::time::Instant::now();
        let error = instagram
            .extract_endpoint(&endpoint, "BA")
            .await
            .unwrap_err()
            .to_string();
        assert!(start.elapsed() < Duration::from_secs(1));
        // The client and outer deadline can race; either failure is bounded.
        assert!(error.contains("timed out") || error.contains("request failed"));
        server.join().unwrap();
    }
}
