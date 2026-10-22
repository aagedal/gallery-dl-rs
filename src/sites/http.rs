//! Shared bounded HTTP access for registry site extractors.
//!
//! Requests follow same-origin redirects only, retry HTTP 429/5xx and
//! transport failures with backoff and `Retry-After` cooldowns, and cap
//! responses at 16 MiB. Diagnostics never include URLs or credentials.
use crate::{
    Result, error,
    network::NetworkOptions,
    transport::{RateLimits, Redirects, retry_after},
};
use reqwest::{Client, StatusCode};
use serde_json::Value;
use std::time::{Duration, SystemTime};
use url::Url;

const MAX_RESPONSE: usize = 16 * 1024 * 1024;

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
        let body = self.get(url, headers).await?;
        Ok(String::from_utf8_lossy(&body).into_owned())
    }

    pub async fn get(&self, url: &Url, headers: &[(&str, &str)]) -> Result<Vec<u8>> {
        self.send(url, headers, None).await
    }

    /// POST a JSON body; redirects are refused.
    pub async fn post_json(
        &self,
        url: &Url,
        body: &Value,
        headers: &[(&str, &str)],
    ) -> Result<Value> {
        let bytes = self.send(url, headers, Some(body)).await?;
        serde_json::from_slice(&bytes)
            .map_err(|_| error(format!("{} returned invalid JSON", self.name)))
    }

    async fn send(
        &self,
        url: &Url,
        headers: &[(&str, &str)],
        body: Option<&Value>,
    ) -> Result<Vec<u8>> {
        tokio::time::timeout(self.timeout, async {
            for attempt in 0..=self.retries {
                match self.attempt(url, headers, body, attempt).await {
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
        body: Option<&Value>,
        attempt: u8,
    ) -> std::result::Result<Vec<u8>, Failure> {
        let name = self.name;
        let mut target = endpoint.clone();
        let mut redirects = Redirects::new(endpoint);
        let response = loop {
            self.rate_limits.wait(&target, self.timeout).await?;
            let mut request = match body {
                Some(body) => self
                    .client
                    .post(target.clone())
                    .header("Content-Type", "application/json")
                    .body(body.to_string()),
                None => self.client.get(target.clone()),
            }
            .timeout(self.request_timeout);
            for (key, value) in headers {
                request = request.header(*key, *value);
            }
            let response = request.send().await.map_err(|_| {
                Failure::transient(format!("{name} request failed; check network access"))
            })?;
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
            return Err(error(format!(
                "{name} refused access; the resource may need authentication"
            ))
            .into());
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
        Ok(crate::transport::decode_api_body(bytes, &headers, MAX_RESPONSE).await?)
    }
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
