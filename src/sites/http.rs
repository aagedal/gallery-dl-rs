//! Shared bounded HTTP access for registry site extractors.
//!
//! Requests follow same-origin redirects and HTTPS redirects within one
//! registrable domain; GET requests without credential headers follow
//! redirects to any site (session cookies stay scoped to their domains).
//! They retry HTTP 429/5xx and transport failures with backoff and
//! `Retry-After` cooldowns, and cap responses at 16 MiB. Diagnostics never include URLs or credentials.
use crate::{
    Result,
    cookies::CookieJar,
    error,
    network::NetworkOptions,
    transport::{RateLimits, Redirects, retry_after},
};
use reqwest::{Client, StatusCode};
use serde_json::Value;
use std::time::{Duration, SystemTime};
use url::Url;

const MAX_RESPONSE: usize = 16 * 1024 * 1024;

/// Desktop browser User-Agent (upstream gallery-dl's default) for sites that
/// refuse unknown clients.
pub const BROWSER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:128.0) Gecko/20100101 Firefox/128.0";

/// Per-site HTTP settings, created once per CLI invocation.
pub struct Http {
    name: &'static str,
    client: Client,
    pub(crate) network: NetworkOptions,
    timeout: Duration,
    request_timeout: Duration,
    retries: u8,
    rate_limits: RateLimits,
    api_key: Option<String>,
    options: Option<std::sync::Arc<serde_json::Map<String, Value>>>,
    root: Option<Url>,
    /// Session cookies sent with API requests; responses update them in
    /// memory. They are never attached to emitted media.
    cookies: std::sync::Mutex<CookieJar>,
    ytdl: std::path::PathBuf,
    ytdl_cookies: Option<crate::cookies::YtdlCookies>,
}

/// A request body.
#[derive(Clone, Copy)]
pub enum Body<'b> {
    Json(&'b Value),
    /// `application/x-www-form-urlencoded` pairs.
    Form(&'b [(&'b str, &'b str)]),
    /// Raw bytes with the given content type.
    Raw(&'b str, &'b [u8]),
}

impl Http {
    pub fn new(name: &'static str, timeout: Duration) -> Result<Self> {
        let network = NetworkOptions::default();
        Ok(Self {
            name,
            client: network.client(&user_agent(), timeout)?,
            network,
            timeout,
            request_timeout: timeout,
            retries: 0,
            rate_limits: RateLimits::default(),
            api_key: None,
            options: None,
            root: None,
            cookies: std::sync::Mutex::new(CookieJar::default()),
            ytdl: "yt-dlp".into(),
            ytdl_cookies: None,
        })
    }

    /// Apply proxy and user-agent settings to API requests and emitted media.
    pub fn with_network(mut self, network: &NetworkOptions) -> Result<Self> {
        self.client = network.client(&user_agent(), self.timeout)?;
        self.network = network.clone();
        Ok(self)
    }

    /// Additional attempts for HTTP 429/5xx and transport failures.
    pub fn with_retries(mut self, retries: u8) -> Self {
        self.retries = retries;
        self
    }

    /// Bound each HTTP request separately from the per-call deadline.
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// Optional site API key; each site decides how it is sent.
    pub fn with_api_key(mut self, key: Option<&str>) -> Self {
        self.api_key = key.map(Into::into);
        self
    }

    /// The yt-dlp executable for sites extracted through it.
    pub fn with_ytdl(mut self, command: &std::path::Path) -> Self {
        self.ytdl = command.to_owned();
        self
    }

    pub fn ytdl(&self) -> &std::path::Path {
        &self.ytdl
    }

    /// Session cookies forwarded to yt-dlp.
    pub fn with_ytdl_cookies(mut self, cookies: Option<crate::cookies::YtdlCookies>) -> Self {
        self.ytdl_cookies = cookies;
        self
    }

    pub fn ytdl_cookies(&self) -> Option<&crate::cookies::YtdlCookies> {
        self.ytdl_cookies.as_ref()
    }

    /// The configured User-Agent, else [`BROWSER_AGENT`]; sites that refuse
    /// unknown clients send it as an explicit header.
    pub fn browser_agent(&self) -> &str {
        self.network.user_agent().unwrap_or(BROWSER_AGENT)
    }

    /// `headers` plus a browser `User-Agent` unless one is already present.
    pub fn browser_headers<'h>(
        &'h self,
        headers: &[(&'h str, &'h str)],
    ) -> Vec<(&'h str, &'h str)> {
        let mut all = headers.to_vec();
        if !all
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("user-agent"))
        {
            all.push(("User-Agent", self.browser_agent()));
        }
        all
    }

    /// The deadline for one complete extraction call.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Session cookies for API requests.
    pub fn with_cookies(self, cookies: CookieJar) -> Self {
        *self.cookies.lock().unwrap_or_else(|e| e.into_inner()) = cookies;
        self
    }

    /// The value of a session cookie that would be sent to `url`.
    /// Names of the session cookies sent to `url`.
    pub fn cookie_names(&self, url: &Url) -> Vec<String> {
        self.cookies
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .header(url)
            .map(|header| {
                header
                    .split("; ")
                    .filter_map(|pair| pair.split_once('=').map(|(name, _)| name.to_owned()))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn cookie(&self, url: &Url, name: &str) -> Option<String> {
        self.cookies
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .value(url, name)
    }

    /// Store a cookie as if `url` had set it with this `Set-Cookie` value.
    pub fn set_cookie(&self, url: &Url, set_cookie: &str) {
        self.cookies
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .store(url, set_cookie);
    }

    /// Whether any session cookies are loaded.
    pub fn has_cookies(&self) -> bool {
        !self
            .cookies
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }

    /// Site-specific configuration values, by upstream option name.
    pub fn with_options(
        mut self,
        options: Option<std::sync::Arc<serde_json::Map<String, Value>>>,
    ) -> Self {
        self.options = options;
        self
    }

    /// A site-specific configuration value.
    pub fn option(&self, key: &str) -> Option<&Value> {
        self.options.as_ref()?.get(key)
    }

    /// A site option interpreted with Python truthiness, or `default`.
    pub fn option_bool(&self, key: &str, default: bool) -> bool {
        match self.option(key) {
            None => default,
            Some(Value::Null) => false,
            Some(Value::Bool(value)) => *value,
            Some(Value::Number(n)) => n.as_f64() != Some(0.0),
            Some(Value::String(s)) => !s.is_empty(),
            Some(Value::Array(a)) => !a.is_empty(),
            Some(Value::Object(o)) => !o.is_empty(),
        }
    }

    /// A string site option.
    pub fn option_str(&self, key: &str) -> Option<&str> {
        self.option(key)?.as_str()
    }

    /// Send every API request to this origin instead; intended for local tests.
    pub fn with_api_root(mut self, root: Url) -> Self {
        self.root = Some(root);
        self
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    pub fn api_key(&self) -> Option<&str> {
        self.api_key.as_deref()
    }

    /// Rewrite an absolute API URL to the test origin, keeping path and query.
    pub fn api(&self, url: &str) -> Result<Url> {
        let mut url = Url::parse(url).map_err(|_| error("invalid site API URL"))?;
        if let Some(root) = &self.root {
            url.set_scheme(root.scheme())
                .map_err(|_| error("invalid site API root"))?;
            url.set_host(root.host_str())
                .map_err(|_| error("invalid site API root"))?;
            url.set_port(root.port())
                .map_err(|_| error("invalid site API root"))?;
        }
        Ok(url)
    }

    pub async fn json(&self, url: &Url) -> Result<Value> {
        self.json_with(url, &[]).await
    }

    pub async fn json_with(&self, url: &Url, headers: &[(&str, &str)]) -> Result<Value> {
        let body = self.get(url, headers).await?;
        serde_json::from_slice(&body)
            .map_err(|_| error(format!("{} returned invalid JSON", self.name)))
    }

    pub async fn text(&self, url: &Url) -> Result<String> {
        self.text_with(url, &[]).await
    }

    pub async fn text_with(&self, url: &Url, headers: &[(&str, &str)]) -> Result<String> {
        let (body, _, content_type) = self.send_with(url, headers, None, false).await?;
        Ok(decode_text(&body, content_type.as_deref()))
    }

    pub async fn get(&self, url: &Url, headers: &[(&str, &str)]) -> Result<Vec<u8>> {
        self.send(url, headers, None).await
    }

    /// GET that never follows a redirect to another site, for sites that
    /// detect login walls by such redirects.
    pub async fn get_strict(&self, url: &Url, headers: &[(&str, &str)]) -> Result<Vec<u8>> {
        self.send_with(url, headers, None, true)
            .await
            .map(|(body, _, _)| body)
    }

    /// GET text and the URL it was finally served from after redirects.
    pub async fn text_final(&self, url: &Url, headers: &[(&str, &str)]) -> Result<(String, Url)> {
        let (body, final_url, content_type) = self.send_with(url, headers, None, false).await?;
        Ok((decode_text(&body, content_type.as_deref()), final_url))
    }

    /// The `Location` of one GET without following it, for short links that
    /// redirect to another site; `None` unless the response is a redirect.
    pub async fn location(&self, url: &Url, headers: &[(&str, &str)]) -> Result<Option<Url>> {
        self.rate_limits.wait(url, self.timeout).await?;
        let mut request = self.client.get(url.clone()).timeout(self.request_timeout);
        for (key, value) in headers {
            request = request.header(*key, *value);
        }
        let response = tokio::time::timeout(self.timeout, request.send())
            .await
            .map_err(|_| error(format!("native {} extraction timed out", self.name)))?
            .map_err(|_| {
                error(format!(
                    "{} request failed; check network access",
                    self.name
                ))
            })?;
        if !response.status().is_redirection() {
            return Ok(None);
        }
        Ok(response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .filter(|value| value.len() <= 8192)
            .and_then(|value| url.join(value).ok())
            .filter(|target| {
                matches!(target.scheme(), "http" | "https")
                    && target.username().is_empty()
                    && target.password().is_none()
            }))
    }

    /// POST an (empty) form without following redirects: the redirect
    /// target, or the response text. Session cookies are sent and updated.
    pub async fn post_or_location(
        &self,
        url: &Url,
        headers: &[(&str, &str)],
    ) -> Result<std::result::Result<String, Url>> {
        self.rate_limits.wait(url, self.timeout).await?;
        let mut request = self
            .client
            .post(url.clone())
            .header("Content-Type", "application/x-www-form-urlencoded")
            .timeout(self.request_timeout);
        if let Some(cookie) = self
            .cookies
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .header(url)
        {
            request = request.header("Cookie", cookie);
        }
        for (key, value) in headers {
            request = request.header(*key, *value);
        }
        let response = tokio::time::timeout(self.timeout, request.send())
            .await
            .map_err(|_| error(format!("native {} extraction timed out", self.name)))?
            .map_err(|_| {
                error(format!(
                    "{} request failed; check network access",
                    self.name
                ))
            })?;
        self.cookies
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .store_response(url, response.headers());
        let status = response.status();
        if status.is_redirection() {
            return response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| url.join(value).ok())
                .filter(|target| matches!(target.scheme(), "http" | "https"))
                .map(Err)
                .ok_or_else(|| error(format!("{} sent an invalid redirect", self.name)));
        }
        if !status.is_success() {
            return Err(error(format!("{} returned HTTP {status}", self.name)));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|_| error(format!("could not read {} response", self.name)))?;
        if bytes.len() > MAX_RESPONSE {
            return Err(error(format!("{} response exceeded 16 MiB", self.name)));
        }
        Ok(Ok(String::from_utf8_lossy(&bytes).into_owned()))
    }

    /// POST a body and return the raw response; redirects are refused.
    pub async fn post(
        &self,
        url: &Url,
        body: Body<'_>,
        headers: &[(&str, &str)],
    ) -> Result<Vec<u8>> {
        self.send(url, headers, Some(body)).await
    }

    /// POST a form and parse the JSON response; redirects are refused.
    /// POST a form and return the response text (lossy UTF-8).
    pub async fn post_form_text(
        &self,
        url: &Url,
        form: &[(&str, &str)],
        headers: &[(&str, &str)],
    ) -> Result<String> {
        let bytes = self.send(url, headers, Some(Body::Form(form))).await?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    pub async fn post_form(
        &self,
        url: &Url,
        form: &[(&str, &str)],
        headers: &[(&str, &str)],
    ) -> Result<Value> {
        let bytes = self.send(url, headers, Some(Body::Form(form))).await?;
        serde_json::from_slice(&bytes)
            .map_err(|_| error(format!("{} returned invalid JSON", self.name)))
    }

    /// POST a JSON body; redirects are refused.
    pub async fn post_json(
        &self,
        url: &Url,
        body: &Value,
        headers: &[(&str, &str)],
    ) -> Result<Value> {
        let bytes = self.send(url, headers, Some(Body::Json(body))).await?;
        serde_json::from_slice(&bytes)
            .map_err(|_| error(format!("{} returned invalid JSON", self.name)))
    }

    async fn send(
        &self,
        url: &Url,
        headers: &[(&str, &str)],
        body: Option<Body<'_>>,
    ) -> Result<Vec<u8>> {
        self.send_with(url, headers, body, false)
            .await
            .map(|(body, _, _)| body)
    }

    async fn send_with(
        &self,
        url: &Url,
        headers: &[(&str, &str)],
        body: Option<Body<'_>>,
        strict: bool,
    ) -> Result<(Vec<u8>, Url, Option<String>)> {
        tokio::time::timeout(self.timeout, async {
            for attempt in 0..=self.retries {
                match self.attempt(url, headers, body, attempt, strict).await {
                    Ok(body) => return Ok(body),
                    Err(failure) if failure.retry && attempt < self.retries => {
                        tokio::time::sleep(backoff(attempt)).await;
                    }
                    Err(failure) => return Err(failure.error),
                }
            }
            unreachable!()
        })
        .await
        .map_err(|_| error(format!("native {} extraction timed out", self.name)))?
    }

    async fn attempt(
        &self,
        endpoint: &Url,
        headers: &[(&str, &str)],
        body: Option<Body<'_>>,
        attempt: u8,
        strict: bool,
    ) -> std::result::Result<(Vec<u8>, Url, Option<String>), Failure> {
        let name = self.name;
        let mut target = endpoint.clone();
        let mut redirects = Redirects::new(endpoint)
            .allow_same_site(true)
            .allow_any_site(
                !strict && body.is_none() && !headers.iter().any(|(key, _)| sensitive(key)),
            );
        let response = loop {
            self.rate_limits.wait(&target, self.timeout).await?;
            let mut request = match body {
                Some(Body::Json(body)) => self
                    .client
                    .post(target.clone())
                    .header("Content-Type", "application/json")
                    .body(body.to_string()),
                Some(Body::Form(form)) => self
                    .client
                    .post(target.clone())
                    .header("Content-Type", "application/x-www-form-urlencoded")
                    .body(
                        url::form_urlencoded::Serializer::new(String::new())
                            .extend_pairs(form)
                            .finish(),
                    ),
                Some(Body::Raw(content_type, bytes)) => self
                    .client
                    .post(target.clone())
                    .header("Content-Type", content_type)
                    .body(bytes.to_vec()),
                None => self.client.get(target.clone()),
            }
            .timeout(self.request_timeout);
            let explicit_cookie = headers
                .iter()
                .any(|(key, _)| key.eq_ignore_ascii_case("cookie"));
            if !explicit_cookie
                && let Some(cookie) = self
                    .cookies
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .header(&target)
            {
                request = request.header("Cookie", cookie);
            }
            for (key, value) in headers {
                request = request.header(*key, *value);
            }
            let response = request.send().await.map_err(|_| {
                Failure::transient(format!("{name} request failed; check network access"))
            })?;
            self.cookies
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .store_response(&target, response.headers());
            if body.is_some() && response.status().is_redirection() {
                return Err(error(format!("{name} redirected a POST request")).into());
            }
            match redirects.next(response.url(), response.status(), response.headers())? {
                Some(next) => target = next,
                None => break response,
            }
        };
        let status = response.status();
        if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
            let delay = retry_after(response.headers(), SystemTime::now());
            self.rate_limits
                .defer(endpoint, delay.unwrap_or_else(|| backoff(attempt)));
            return Err(Failure::transient(
                if status == StatusCode::TOO_MANY_REQUESTS {
                    match delay {
                        Some(delay) => {
                            format!("{name} rate limit; retry after {} seconds", delay.as_secs())
                        }
                        None => format!("{name} rate limit; retry later"),
                    }
                } else {
                    format!("{name} returned HTTP {status}")
                },
            ));
        }
        if matches!(status.as_u16(), 404 | 410) {
            return Err(error(format!("{name} resource not found")).into());
        }
        if matches!(status.as_u16(), 401 | 403) {
            let refused: crate::Error = Box::new(Refused {
                status: status.as_u16(),
                message: format!("{name} refused access; the resource may need authentication"),
            });
            return Err(refused.into());
        }
        if !status.is_success() {
            return Err(error(format!("{name} returned HTTP {status}")).into());
        }
        let headers = response.headers().clone();
        let mut response = response;
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| Failure::transient(format!("could not read {name} response")))?
        {
            if bytes.len() + chunk.len() > MAX_RESPONSE {
                return Err(error(format!("{name} response exceeded 16 MiB")).into());
            }
            bytes.extend_from_slice(&chunk);
        }
        let content_type = headers
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        Ok((
            crate::transport::decode_api_body(bytes, &headers, MAX_RESPONSE).await?,
            target,
            content_type,
        ))
    }
}

/// Decode a text response by its `Content-Type` charset, else a `<meta>`
/// charset declaration near the start of an HTML page, else UTF-8.
pub(crate) fn decode_text(body: &[u8], content_type: Option<&str>) -> String {
    let declared = content_type
        .and_then(|value| {
            value.split(';').find_map(|part| {
                part.trim()
                    .strip_prefix("charset=")
                    .map(|charset| charset.trim_matches('"').to_owned())
            })
        })
        .or_else(|| {
            let head = String::from_utf8_lossy(&body[..body.len().min(2048)]).to_ascii_lowercase();
            let at = head.find("charset=")? + "charset=".len();
            let value: String = head[at..]
                .trim_start_matches(['"', '\''])
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
                .collect();
            (!value.is_empty()).then_some(value)
        });
    match declared.and_then(|label| encoding_rs::Encoding::for_label(label.as_bytes())) {
        Some(encoding) if encoding != encoding_rs::UTF_8 => encoding.decode(body).0.into_owned(),
        _ => String::from_utf8_lossy(body).into_owned(),
    }
}

/// Whether a request header carries credentials that must not follow a
/// redirect to another site.
fn sensitive(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    matches!(
        name.as_str(),
        "authorization" | "proxy-authorization" | "cookie"
    ) || [
        "auth",
        "token",
        "key",
        "session",
        "csrf",
        "secret",
        "signature",
    ]
    .iter()
    .any(|word| name.contains(word))
}

/// An HTTP 401/403 response. Displays like any other site error (no URL or
/// credentials); [`refused_status`] lets a site tell the two apart.
#[derive(Debug)]
pub struct Refused {
    status: u16,
    message: String,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Refused {}

/// The HTTP status (401 or 403) of a refused request, if `error` is one.
pub fn refused_status(error: &crate::Error) -> Option<u16> {
    error
        .downcast_ref::<Refused>()
        .map(|refused| refused.status)
}

fn user_agent() -> String {
    concat!("gallery-dl-rs/", env!("CARGO_PKG_VERSION")).to_owned()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http, json, response, serve};

    #[tokio::test]
    async fn cookies_are_sent_and_updated_from_responses() {
        let set = response(
            "200 OK\r\nSet-Cookie: token=fresh; Path=/",
            "application/json",
            b"{}",
        );
        let (root, server) = serve(vec![set, json(serde_json::json!({"ok": true}))]);
        let jar = CookieJar::parse(&format!(
            "{}\tFALSE\t/\tFALSE\t0\tsession\tabc\n",
            root.host_str().unwrap()
        ))
        .unwrap();
        let http = http("Test", root.clone()).with_cookies(jar);
        assert!(http.has_cookies());
        http.json(&root.join("first").unwrap()).await.unwrap();
        assert_eq!(http.cookie(&root, "token").as_deref(), Some("fresh"));
        let form = [("a", "1 2"), ("b", "&")];
        http.post_form(&root.join("second").unwrap(), &form, &[])
            .await
            .unwrap();
        let requests = server.join().unwrap();
        assert!(requests[0].contains("cookie: session=abc"));
        assert!(!requests[0].contains("token="));
        let second = requests[1].to_ascii_lowercase();
        assert!(second.starts_with("post /second"));
        assert!(second.contains("cookie: session=abc; token=fresh"));
        assert!(second.contains("content-type: application/x-www-form-urlencoded"));
        assert!(requests[1].ends_with("a=1+2&b=%26"));
    }

    #[tokio::test]
    async fn explicit_cookie_header_replaces_the_jar() {
        let (root, server) = serve(vec![json(serde_json::json!({}))]);
        let http = http("Test", root.clone());
        http.set_cookie(&root, "jar=1");
        http.json_with(&root, &[("Cookie", "explicit=1")])
            .await
            .unwrap();
        let requests = server.join().unwrap();
        assert!(requests[0].contains("cookie: explicit=1"));
        assert!(!requests[0].contains("jar=1"));
    }
}

#[cfg(test)]
mod charset_tests {
    use super::decode_text;

    #[test]
    fn decodes_declared_charsets() {
        let (sjis, _, _) = encoding_rs::SHIFT_JIS.encode("二次元裏＠ふたば");
        assert_eq!(
            decode_text(&sjis, Some("text/html; charset=Shift_JIS")),
            "二次元裏＠ふたば"
        );
        let page = [b"<meta charset=\"shift_jis\">".as_slice(), &sjis].concat();
        assert!(decode_text(&page, Some("text/html")).ends_with("二次元裏＠ふたば"));
        assert_eq!(decode_text("ä".as_bytes(), None), "ä");
        assert_eq!(
            decode_text(b"\xe4", Some("text/plain; charset=iso-8859-1")),
            "ä"
        );
    }
}
