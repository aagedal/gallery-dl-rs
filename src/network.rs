//! Validated, private network settings shared by extractors and downloads.
use crate::{Result, error, extract::Media};
use reqwest::{Client, Proxy, header::HeaderValue};
use std::{fmt, time::Duration};
use url::Url;

#[derive(Clone, Default, PartialEq)]
pub struct NetworkOptions {
    proxy: Option<String>,
    user_agent: Option<String>,
    /// Upstream `verify: false`: accept invalid TLS certificates.
    insecure: bool,
    /// Upstream `source-address`: bind outgoing connections to this address.
    source_address: Option<std::net::IpAddr>,
}
impl fmt::Debug for NetworkOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NetworkOptions { settings: [redacted] }")
    }
}
impl NetworkOptions {
    pub fn new(proxy: Option<String>, user_agent: Option<String>) -> Result<Self> {
        if let Some(proxy) = &proxy
            && !proxy.is_empty()
        {
            let invalid = || error("proxy must be an HTTP(S) proxy URL or an empty string");
            if proxy.len() > 8192 || proxy.chars().any(char::is_control) {
                return Err(invalid());
            }
            let url = Url::parse(proxy).map_err(|_| invalid())?;
            if !matches!(url.scheme(), "http" | "https")
                || url.host_str().is_none()
                || !matches!(url.path(), "" | "/")
                || url.query().is_some()
                || url.fragment().is_some()
            {
                return Err(invalid());
            }
            Proxy::all(proxy).map_err(|_| invalid())?;
        }
        if let Some(agent) = &user_agent
            && (agent.trim().is_empty()
                || agent.len() > 1024
                || !agent.is_ascii()
                || agent.bytes().any(|c| c.is_ascii_control())
                || HeaderValue::from_str(agent).is_err())
        {
            return Err(error(
                "user-agent must be a nonempty ASCII HTTP header of at most 1024 bytes",
            ));
        }
        Ok(Self {
            proxy,
            user_agent,
            ..Self::default()
        })
    }
    /// Disable TLS certificate verification (`--no-check-certificate`).
    pub fn with_insecure(mut self, insecure: bool) -> Self {
        self.insecure = insecure;
        self
    }
    /// Equivalent upstream CLI arguments for the compatibility backend.
    pub fn backend_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if self.insecure {
            args.push("--no-check-certificate".into());
        }
        if let Some(address) = self.source_address {
            args.extend(["--source-address".into(), address.to_string()]);
        }
        args
    }
    /// Bind outgoing connections to a local address (`--source-address`, `-4`, `-6`).
    pub fn with_source_address(mut self, address: Option<std::net::IpAddr>) -> Self {
        self.source_address = address;
        self
    }
    pub fn proxy(&self) -> Option<&str> {
        self.proxy.as_deref()
    }
    pub fn user_agent(&self) -> Option<&str> {
        self.user_agent.as_deref()
    }

    /// Preserve environment proxy defaults when absent; explicit empty disables
    /// all proxy discovery. Redirects remain under the caller's manual policy.
    pub fn client(&self, default_agent: &str, timeout: Duration) -> Result<Client> {
        let mut builder = Client::builder()
            .timeout(timeout)
            .user_agent(self.user_agent().unwrap_or(default_agent))
            .redirect(reqwest::redirect::Policy::none());
        if self.insecure {
            builder = builder.danger_accept_invalid_certs(true);
        }
        if let Some(address) = self.source_address {
            builder = builder.local_address(address);
        }
        if let Some(proxy) = self.proxy() {
            builder = if proxy.is_empty() {
                builder.no_proxy()
            } else {
                builder.proxy(Proxy::all(proxy).map_err(|_| error("invalid proxy configuration"))?)
            };
        }
        builder
            .build()
            .map_err(|_| error("could not initialize HTTP client"))
    }
    /// An explicit override is also part of the job's effective request identity.
    pub fn apply_media(&self, media: &mut Media) {
        if let Some(agent) = self.user_agent() {
            media
                .headers
                .retain(|name, _| !name.eq_ignore_ascii_case("user-agent"));
            media.headers.insert("User-Agent".into(), agent.into());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_without_leaking_private_values() {
        for proxy in [
            "socks5://private:secret@localhost:1234",
            "private-value",
            "http://user:secret@proxy.test/path",
            "http://proxy.test/?secret",
            "http://proxy.test/#secret",
            "http://proxy.test/\nsecret",
        ] {
            let message = NetworkOptions::new(Some(proxy.into()), None)
                .unwrap_err()
                .to_string();
            assert!(!message.contains("secret"));
            assert!(!message.contains("private-value"));
        }
        for agent in ["", " ", "secret\r\nx: injected", "秘密"] {
            assert!(
                !NetworkOptions::new(None, Some(agent.into()))
                    .unwrap_err()
                    .to_string()
                    .contains("secret")
            );
        }
        assert!(NetworkOptions::new(None, Some("a".repeat(1025))).is_err());
        let options = NetworkOptions::new(
            Some("http://user:secret@proxy.test:1234".into()),
            Some("private-agent".into()),
        )
        .unwrap();
        assert_eq!(
            format!("{options:?}"),
            "NetworkOptions { settings: [redacted] }"
        );
        assert_eq!(options.user_agent(), Some("private-agent"));
        assert_eq!(
            NetworkOptions::new(Some(String::new()), None)
                .unwrap()
                .proxy(),
            Some("")
        );
    }
}
