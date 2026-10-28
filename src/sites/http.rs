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
    sleep::SleepRange,
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
    /// Upstream `sleep-request`, `sleep-retries` and `sleep-429`.
    sleep_request: Option<SleepRange>,
    sleep_retries: Option<SleepRange>,
    sleep_429: Option<SleepRange>,
}

/// Upstream `--print-traffic`: show requests and responses on stderr.
pub static PRINT_TRAFFIC: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static DUMP_INDEX: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Upstream `_dump_response` for `write-pages`: `true` writes bodies, "all"
/// adds headers without authentication values, "ALL" includes them.
fn dump_response(
    mode: &Value,
    url: &Url,
    status: StatusCode,
    headers: &reqwest::header::HeaderMap,
    body: &[u8],
) {
    let index = DUMP_INDEX.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    let name: String = format!(
        "{index:02}_{}",
        crate::sites::gdl::re!(r#"[\\|/<>:"?*&=#]+"#).replace_all(url.as_str(), "_")
    );
    let name: String = name.chars().take(251).collect();
    let mut out = Vec::new();
    let with_headers = matches!(mode.as_str(), Some("all" | "ALL"));
    if with_headers {
        out.extend_from_slice(format!("{} {}\n", status.as_u16(), url).as_bytes());
        for (key, value) in headers {
            let value = if mode.as_str() != Some("ALL") && sensitive(key.as_str()) {
                "***".to_owned()
            } else {
                String::from_utf8_lossy(value.as_bytes()).into_owned()
            };
            out.extend_from_slice(format!("{key}: {value}\n").as_bytes());
        }
        out.push(b'\n');
    }
    out.extend_from_slice(body);
    match std::fs::write(format!("{name}.txt"), out) {
        Ok(()) => eprintln!("Writing '{url}' response to '{name}.txt'"),
        Err(e) => eprintln!("warning: Failed to dump HTTP request ({e})"),
    }
}

/// Upstream `Extractor.request_timestamp`: when any extractor last sent a
/// request, shared so `sleep-request` spaces requests across sites.
static LAST_REQUEST: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);

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
    /// A copy of the session cookies, for `--cookies-export`.
    pub fn cookie_snapshot(&self) -> crate::cookies::CookieJar {
        self.cookies
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

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
            sleep_request: None,
            sleep_retries: None,
            sleep_429: None,
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

    /// The `Cookie` header the session would send to `url`, for downloads
    /// that need the extraction session (e.g. form POSTs).
    pub fn cookie_header(&self, url: &Url) -> Option<String> {
        self.cookies
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .header(url)
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
        let sleep = |key: &str| {
            self.option(key).and_then(|value| {
                SleepRange::from_value(value).unwrap_or_else(|_| {
                    eprintln!("warning: invalid '{key}' value; ignoring it");
                    None
                })
            })
        };
        let (request, retries, too_many) = (
            sleep("sleep-request"),
            sleep("sleep-retries"),
            sleep("sleep-429"),
        );
        self.sleep_request = request;
        self.sleep_retries = retries;
        self.sleep_429 = too_many;
        self
    }

    /// Wait until `sleep-request` has passed since the last request.
    async fn pace(&self) {
        let Some(interval) = self.sleep_request else {
            return;
        };
        let wait = {
            let last = LAST_REQUEST.lock().unwrap_or_else(|e| e.into_inner());
            last.map(|last| interval.sample().saturating_sub(last.elapsed()))
        };
        if let Some(wait) = wait.filter(|wait| !wait.is_zero()) {
            tokio::time::sleep(wait).await;
        }
    }

    fn mark_request(&self) {
        *LAST_REQUEST.lock().unwrap_or_else(|e| e.into_inner()) = Some(std::time::Instant::now());
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
        let (body, _, headers) = self.send_with(url, headers, None, false).await?;
        Ok(decode_text(&body, content_type(&headers)))
    }

    /// GET the raw body together with the response headers.
    pub async fn get_with_headers(
        &self,
        url: &Url,
        headers: &[(&str, &str)],
    ) -> Result<(Vec<u8>, reqwest::header::HeaderMap)> {
        let (body, _, headers) = self.send_with(url, headers, None, false).await?;
        Ok((body, headers))
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
        let (body, final_url, headers) = self.send_with(url, headers, None, false).await?;
        Ok((decode_text(&body, content_type(&headers)), final_url))
    }

    /// One GET without status handling or redirects: status and body text,
    /// for sites whose error pages carry data (e.g. 429 challenges).
    pub async fn get_status(&self, url: &Url, headers: &[(&str, &str)]) -> Result<(u16, String)> {
        self.rate_limits.wait(url, self.timeout).await?;
        let mut request = self.client.get(url.clone()).timeout(self.request_timeout);
        if let Some(cookie) = self.cookie_header(url) {
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
        let status = response.status().as_u16();
        let content_type = content_type(response.headers()).map(str::to_owned);
        let bytes = response
            .bytes()
            .await
            .map_err(|_| error(format!("could not read {} response", self.name)))?;
        if bytes.len() > MAX_RESPONSE {
            return Err(error(format!("{} response exceeded 16 MiB", self.name)));
        }
        Ok((status, decode_text(&bytes, content_type.as_deref())))
    }

    /// The status of one HEAD request without following redirects.
    pub async fn head_status(&self, url: &Url, headers: &[(&str, &str)]) -> Result<u16> {
        Ok(self.head(url, headers).await?.0)
    }

    /// Status and headers of one HEAD request without following redirects.
    pub async fn head(
        &self,
        url: &Url,
        headers: &[(&str, &str)],
    ) -> Result<(u16, reqwest::header::HeaderMap)> {
        self.rate_limits.wait(url, self.timeout).await?;
        let mut request = self.client.head(url.clone()).timeout(self.request_timeout);
        if let Some(cookie) = self.cookie_header(url) {
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
        Ok((response.status().as_u16(), response.headers().clone()))
    }

    /// The `Location` of one GET without following it, for short links that
    /// redirect to another site; `None` unless the response is a redirect.
    /// Session cookies are sent and updated, like any session request.
    pub async fn location(&self, url: &Url, headers: &[(&str, &str)]) -> Result<Option<Url>> {
        self.rate_limits.wait(url, self.timeout).await?;
        let mut request = self.client.get(url.clone()).timeout(self.request_timeout);
        if let Some(cookie) = self.cookie_header(url) {
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
        self.post_form_or_location(url, &[], headers).await
    }

    /// POST a form without following redirects: the redirect target, or the
    /// response text. Session cookies are sent and updated.
    pub async fn post_form_or_location(
        &self,
        url: &Url,
        form: &[(&str, &str)],
        headers: &[(&str, &str)],
    ) -> Result<std::result::Result<String, Url>> {
        self.rate_limits.wait(url, self.timeout).await?;
        let mut request = self
            .client
            .post(url.clone())
            .header("Content-Type", "application/x-www-form-urlencoded")
            .timeout(self.request_timeout);
        if !form.is_empty() {
            request = request.body(
                url::form_urlencoded::Serializer::new(String::new())
                    .extend_pairs(form.iter())
                    .finish(),
            );
        }
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
    ) -> Result<(Vec<u8>, Url, reqwest::header::HeaderMap)> {
        self.pace().await;
        tokio::time::timeout(self.timeout, async {
            for attempt in 0..=self.retries {
                let result = self.attempt(url, headers, body, attempt, strict).await;
                self.mark_request();
                match result {
                    Ok(body) => return Ok(body),
                    Err(failure) if failure.retry && attempt < self.retries => {
                        let configured = if failure.rate_limited {
                            self.sleep_429.or(self.sleep_retries)
                        } else {
                            self.sleep_retries
                        };
                        tokio::time::sleep(
                            configured.map_or_else(|| backoff(attempt), |range| range.sample()),
                        )
                        .await;
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
    ) -> std::result::Result<(Vec<u8>, Url, reqwest::header::HeaderMap), Failure> {
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
            let traffic = PRINT_TRAFFIC.load(std::sync::atomic::Ordering::Relaxed);
            if traffic {
                eprintln!(
                    "send: {} {target}",
                    if body.is_some() { "POST" } else { "GET" }
                );
            }
            let response = request.send().await.map_err(|_| {
                Failure::transient(format!("{name} request failed; check network access"))
            })?;
            if traffic {
                eprintln!("reply: {:?} {}", response.version(), response.status());
                for (key, value) in response.headers() {
                    eprintln!(
                        "header: {key}: {}",
                        String::from_utf8_lossy(value.as_bytes())
                    );
                }
            }
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
            let mut failure = Failure::transient(if status == StatusCode::TOO_MANY_REQUESTS {
                match delay {
                    Some(delay) => {
                        format!("{name} rate limit; retry after {} seconds", delay.as_secs())
                    }
                    None => format!("{name} rate limit; retry later"),
                }
            } else {
                format!("{name} returned HTTP {status}")
            });
            failure.rate_limited = status == StatusCode::TOO_MANY_REQUESTS;
            return Err(failure);
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
        let bytes = crate::transport::decode_api_body(bytes, &headers, MAX_RESPONSE).await?;
        if let Some(mode) = self
            .option("write-pages")
            .filter(|_| self.option_bool("write-pages", false))
        {
            dump_response(mode, &target, status, &headers, &bytes);
        }
        Ok((bytes, target, headers))
    }
}

fn content_type(headers: &reqwest::header::HeaderMap) -> Option<&str> {
    headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
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
    rate_limited: bool,
}

impl Failure {
    fn transient(message: impl Into<String>) -> Self {
        Self {
            error: error(message),
            retry: true,
            rate_limited: false,
        }
    }
}

impl From<crate::Error> for Failure {
    fn from(error: crate::Error) -> Self {
        Self {
            error,
            retry: false,
            rate_limited: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http, json, response, serve};

    #[tokio::test]
    async fn write_pages_dumps_response_bodies() {
        let (root, server) = serve(vec![json(serde_json::json!({"dumped": true}))]);
        let options = serde_json::Map::from_iter([("write-pages".into(), "all".into())]);
        let client = http("Test", root.clone()).with_options(Some(std::sync::Arc::new(options)));
        client.json(&root.join("page").unwrap()).await.unwrap();
        server.join().unwrap();
        let dumped: Vec<_> = std::fs::read_dir(".")
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                name.ends_with(".txt") && name.contains(&format!("{}_page", root.port().unwrap()))
            })
            .collect();
        assert_eq!(dumped.len(), 1);
        let text = std::fs::read_to_string(dumped[0].path()).unwrap();
        std::fs::remove_file(dumped[0].path()).unwrap();
        assert!(text.starts_with("200 http://"), "{text}");
        assert!(text.ends_with("{\"dumped\":true}"), "{text}");
    }

    #[tokio::test]
    async fn sleep_request_spaces_consecutive_requests() {
        let body = || json(serde_json::json!({}));
        let (root, server) = serve(vec![body(), body()]);
        let options = serde_json::Map::from_iter([("sleep-request".into(), "0.3".into())]);
        let client = http("Test", root.clone()).with_options(Some(std::sync::Arc::new(options)));
        client.json(&root.join("a").unwrap()).await.unwrap();
        let start = std::time::Instant::now();
        client.json(&root.join("b").unwrap()).await.unwrap();
        assert!(
            start.elapsed() >= Duration::from_millis(250),
            "{:?}",
            start.elapsed()
        );
        server.join().unwrap();
        let invalid = serde_json::Map::from_iter([("sleep-request".into(), "x".into())]);
        assert!(
            http("Test", root)
                .with_options(Some(std::sync::Arc::new(invalid)))
                .sleep_request
                .is_none()
        );
    }

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
