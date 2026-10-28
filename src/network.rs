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
    /// Upstream `geo-bypass`: the configured value and the generated address.
    xff: Option<(String, std::net::Ipv4Addr)>,
    /// Upstream `tls12: false`: allow only TLS 1.3.
    tls13_only: bool,
    /// Upstream `truststore`: verify with the system certificate store.
    system_roots: bool,
    /// Upstream `ciphers`: allowed cipher suites, in order.
    ciphers: Option<Vec<String>>,
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
    /// Upstream `tls12` (false: TLS 1.3 only) and `truststore` (system
    /// certificates instead of the bundled roots).
    pub fn with_tls(mut self, tls12: bool, truststore: bool) -> Self {
        self.tls13_only = !tls12;
        self.system_roots = truststore;
        self
    }
    /// Upstream `ciphers`: a list or `:`-separated string of OpenSSL or IANA
    /// cipher names. Browser presets (`firefox`, `chrome`) only imitate
    /// fingerprints and are not supported.
    pub fn with_ciphers(mut self, value: Option<&serde_json::Value>) -> Result<Self> {
        let names: Vec<String> = match value {
            None | Some(serde_json::Value::Null) => return Ok(self),
            Some(serde_json::Value::String(text))
                if text.starts_with("firefox") || text.starts_with("chrome") =>
            {
                static WARNED: std::sync::Once = std::sync::Once::new();
                WARNED.call_once(|| {
                    eprintln!("warning: browser cipher presets are not supported natively")
                });
                return Ok(self);
            }
            Some(serde_json::Value::String(text)) => text.split(':').map(str::to_owned).collect(),
            Some(serde_json::Value::Array(items)) => items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect(),
            Some(_) => return Err(error("ciphers must be a string or a list")),
        };
        if cipher_suites(&names).is_empty() {
            return Err(error("ciphers names no cipher suite supported natively"));
        }
        self.ciphers = Some(names);
        Ok(self)
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
        if let Some((value, _)) = &self.xff {
            args.extend(["--xff".into(), value.clone()]);
        }
        args
    }
    /// Send a fake `X-Forwarded-For` address from CIDR blocks or country
    /// codes (`--xff`). Like upstream, invalid values only warn.
    pub fn with_geo_bypass(mut self, value: Option<&str>) -> Result<Self> {
        self.xff = value.and_then(|value| match crate::geo::random_ipv4(value) {
            Some(address) => Some((value.to_owned(), address)),
            None => {
                static WARNED: std::sync::Once = std::sync::Once::new();
                WARNED.call_once(|| {
                    eprintln!("warning: xff: Invalid ISO 3166 country code '{value}'")
                });
                None
            }
        });
        Ok(self)
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
        if let Some(names) = self.ciphers.as_ref().filter(|_| !self.insecure) {
            builder = builder.use_preconfigured_tls(self.tls_config(names)?);
        }
        if self.tls13_only {
            builder = builder.min_tls_version(reqwest::tls::Version::TLS_1_3);
        }
        if self.system_roots {
            // Loaded only on request: reading the system store is slow.
            builder = builder.tls_built_in_root_certs(false);
            for certificate in rustls_native_certs::load_native_certs().certs {
                if let Ok(certificate) = reqwest::Certificate::from_der(certificate.as_ref()) {
                    builder = builder.add_root_certificate(certificate);
                }
            }
        }
        if let Some(address) = self.source_address {
            builder = builder.local_address(address);
        }
        if let Some((_, address)) = &self.xff {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(
                "X-Forwarded-For",
                HeaderValue::from_str(&address.to_string())
                    .map_err(|_| error("invalid X-Forwarded-For address"))?,
            );
            builder = builder.default_headers(headers);
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
    /// A rustls configuration limited to the configured cipher suites.
    fn tls_config(&self, names: &[String]) -> Result<rustls::ClientConfig> {
        let mut provider = rustls::crypto::ring::default_provider();
        provider.cipher_suites = cipher_suites(names);
        let mut roots = rustls::RootCertStore::empty();
        if self.system_roots {
            for certificate in rustls_native_certs::load_native_certs().certs {
                let _ = roots.add(certificate);
            }
        } else {
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        }
        let versions: &[&rustls::SupportedProtocolVersion] = if self.tls13_only {
            &[&rustls::version::TLS13]
        } else {
            rustls::DEFAULT_VERSIONS
        };
        let mut config = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(provider))
            .with_protocol_versions(versions)
            .map_err(|_| error("ciphers do not match the allowed TLS versions"))?
            .with_root_certificates(roots)
            .with_no_client_auth();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(config)
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

/// Supported suites for OpenSSL or IANA cipher names, in the given order.
fn cipher_suites(names: &[String]) -> Vec<rustls::SupportedCipherSuite> {
    use rustls::CipherSuite as S;
    let available = rustls::crypto::ring::default_provider().cipher_suites;
    let mut selected = Vec::new();
    for name in names {
        let suite = match name.trim() {
            "TLS_AES_128_GCM_SHA256" | "TLS13_AES_128_GCM_SHA256" => S::TLS13_AES_128_GCM_SHA256,
            "TLS_AES_256_GCM_SHA384" | "TLS13_AES_256_GCM_SHA384" => S::TLS13_AES_256_GCM_SHA384,
            "TLS_CHACHA20_POLY1305_SHA256" | "TLS13_CHACHA20_POLY1305_SHA256" => {
                S::TLS13_CHACHA20_POLY1305_SHA256
            }
            "ECDHE-ECDSA-AES128-GCM-SHA256" | "TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256" => {
                S::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256
            }
            "ECDHE-ECDSA-AES256-GCM-SHA384" | "TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384" => {
                S::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384
            }
            "ECDHE-RSA-AES128-GCM-SHA256" | "TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256" => {
                S::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256
            }
            "ECDHE-RSA-AES256-GCM-SHA384" | "TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384" => {
                S::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384
            }
            "ECDHE-ECDSA-CHACHA20-POLY1305" | "TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256" => {
                S::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256
            }
            "ECDHE-RSA-CHACHA20-POLY1305" | "TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256" => {
                S::TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256
            }
            // CBC and non-ECDHE suites are not implemented by rustls.
            _ => continue,
        };
        if let Some(found) = available.iter().find(|s| s.suite() == suite)
            && !selected
                .iter()
                .any(|s: &rustls::SupportedCipherSuite| s.suite() == suite)
        {
            selected.push(*found);
        }
    }
    selected
}

#[cfg(test)]
mod cipher_tests {
    #[test]
    fn maps_openssl_and_iana_names() {
        let names: Vec<String> = [
            "ECDHE-RSA-AES128-GCM-SHA256",
            "AES128-SHA",
            "TLS_AES_256_GCM_SHA384",
        ]
        .map(str::to_owned)
        .into();
        let suites = super::cipher_suites(&names);
        assert_eq!(suites.len(), 2);
        assert!(
            super::NetworkOptions::default()
                .with_ciphers(Some(&serde_json::json!("AES128-SHA")))
                .is_err()
        );
        assert!(
            super::NetworkOptions::default()
                .with_ciphers(Some(&serde_json::json!(names)))
                .is_ok()
        );
    }
}
