//! OAuth helpers: upstream's OAuth 1.0a request signing (gallery_dl/oauth.py)
//! and the interactive `oauth:SITE` inputs (gallery_dl/extractor/oauth.py).
//!
//! An `oauth:SITE` input prints an authorization URL (opening it in a browser
//! when the `browser` setting allows), waits on `127.0.0.1:PORT` (`port`,
//! default 6414) for the redirect carrying the user's grant, exchanges it for
//! a token and prints the token with upstream's instructions. With the
//! `cache` setting (default on), the token is also stored in the credential
//! cache under upstream's key, where the matching extractor finds it.
//! Supported: OAuth 1.0a for flickr, smugmug and tumblr; OAuth 2.0
//! authorization codes for deviantart, reddit and mastodon:INSTANCE (with
//! application registration for other instances); and pixiv's PKCE login,
//! whose code is pasted at a prompt. Only a request carrying the expected
//! parameters is answered, and OAuth 2.0 `state` must match.
use crate::{Result, cache::Cache, error, network::NetworkOptions};
use base64::Engine;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use url::Url;

pub const REDIRECT_URI_LOCALHOST: &str = "http://localhost:6414/";
/// Upstream's page forwarding the grant to the local listener.
pub const REDIRECT_URI_HTTPS: &str = "https://mikf.github.io/gallery-dl/oauth-redirect.html";
pub const DEFAULT_PORT: u16 = 6414;
const USER_AGENT: &str = concat!("gallery-dl-rs/", env!("CARGO_PKG_VERSION"));
const MAX_RESPONSE: usize = 1024 * 1024;
const MAX_REQUEST: usize = 16 * 1024;
/// The redirect listener binds the IPv4 loopback address only.
const LOCALHOST: std::net::Ipv4Addr = std::net::Ipv4Addr::LOCALHOST;
const PIXIV_USER_AGENT: &str = "PixivAndroidApp/5.0.234 (Android 11; Pixel 5)";

/// The sites with an `oauth:SITE` input.
pub const SITES: &[&str] = &[
    "deviantart",
    "flickr",
    "mastodon",
    "pixiv",
    "reddit",
    "smugmug",
    "tumblr",
];

// --------------------------------------------------------------------
// Randomness and encodings

/// Bytes from the operating system's CSPRNG.
pub(crate) fn random_bytes(buffer: &mut [u8]) -> Result<()> {
    getrandom::getrandom(buffer).map_err(|_| error("the system random number generator failed"))
}

/// Upstream `oauth.nonce`: `size` random ASCII letters.
pub(crate) fn nonce(size: usize) -> String {
    const LETTERS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let mut out = String::with_capacity(size);
    let mut buffer = [0u8; 64];
    while out.len() < size {
        if random_bytes(&mut buffer).is_err() {
            // Unreachable on supported platforms; a time-derived fallback
            // still yields distinct, if predictable, request nonces.
            let seed = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
                .to_le_bytes();
            for (index, byte) in buffer.iter_mut().enumerate() {
                *byte = seed[index % seed.len()].wrapping_add(index as u8);
            }
        }
        // Rejection sampling keeps the letters uniform.
        for byte in buffer {
            if (byte as usize) < LETTERS.len() * 4 && out.len() < size {
                out.push(LETTERS[byte as usize % LETTERS.len()] as char);
            }
        }
    }
    out
}

/// Upstream `util.generate_token`: `size` random bytes as hex digits.
pub(crate) fn generate_token(size: usize) -> Result<String> {
    let mut bytes = vec![0u8; size];
    random_bytes(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// RFC 7636 S256 code challenge: unpadded base64url of SHA-256.
pub(crate) fn pkce_challenge(verifier: &str) -> String {
    use sha2::{Digest, Sha256};
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// RFC 3986 percent-encoding of everything but unreserved characters, as
/// upstream's `oauth.quote`.
pub(crate) fn quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

pub(crate) fn hmac_sha1(key: &[u8], message: &[u8]) -> [u8; 20] {
    use sha1::{Digest, Sha1};
    let mut block = [0u8; 64];
    if key.len() > block.len() {
        block[..20].copy_from_slice(&Sha1::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let pad = |byte: u8| block.map(|b| b ^ byte);
    let inner = Sha1::new()
        .chain_update(pad(0x36))
        .chain_update(message)
        .finalize();
    Sha1::new()
        .chain_update(pad(0x5c))
        .chain_update(inner)
        .finalize()
        .into()
}

/// HMAC-SHA1 `oauth_signature` (unquoted base64) over the request method,
/// query-less URL and the sorted, encoded OAuth and query parameters.
pub(crate) fn signature(
    method: &str,
    url: &Url,
    oauth: &[(&str, String)],
    consumer_secret: &str,
    token_secret: &str,
) -> String {
    let mut base = url.clone();
    base.set_query(None);
    base.set_fragment(None);
    let mut params: Vec<(String, String)> = oauth
        .iter()
        .map(|(key, value)| (quote(key), quote(value)))
        .chain(
            url.query_pairs()
                .map(|(key, value)| (quote(&key), quote(&value))),
        )
        .collect();
    params.sort();
    let query = params
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("&");
    let message = [method, base.as_str(), &query].map(quote).join("&");
    let key = format!("{}&{}", quote(consumer_secret), quote(token_secret));
    base64::engine::general_purpose::STANDARD.encode(hmac_sha1(key.as_bytes(), message.as_bytes()))
}

/// OAuth 1.0a credentials signing requests (upstream `OAuth1Client`).
pub(crate) struct OAuth1<'a> {
    pub consumer_key: &'a str,
    pub consumer_secret: &'a str,
    /// Omitted while requesting a request token.
    pub token: Option<&'a str>,
    pub token_secret: &'a str,
}

impl OAuth1<'_> {
    /// The `Authorization` header value for a `method` request of `url`.
    pub fn authorization(&self, method: &str, url: &Url, nonce: &str, timestamp: u64) -> String {
        let mut params = vec![
            ("oauth_consumer_key", self.consumer_key.to_owned()),
            ("oauth_nonce", nonce.to_owned()),
            ("oauth_signature_method", "HMAC-SHA1".to_owned()),
            ("oauth_timestamp", timestamp.to_string()),
            ("oauth_version", "1.0".to_owned()),
        ];
        if let Some(token) = self.token {
            params.push(("oauth_token", token.to_owned()));
        }
        let signature = signature(
            method,
            url,
            &params,
            self.consumer_secret,
            self.token_secret,
        );
        params.push(("oauth_signature", signature));
        let fields = params
            .iter()
            .map(|(key, value)| format!("{key}=\"{}\"", quote(value)))
            .collect::<Vec<_>>()
            .join(",");
        format!("OAuth {fields}")
    }

    /// A sensitive header value with a fresh nonce and the current time.
    pub fn header(&self, method: &str, url: &Url) -> Result<reqwest::header::HeaderValue> {
        let timestamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |now| now.as_secs());
        let mut value = reqwest::header::HeaderValue::from_str(&self.authorization(
            method,
            url,
            &nonce(16),
            timestamp,
        ))
        .map_err(|_| error("invalid OAuth credentials"))?;
        value.set_sensitive(true);
        Ok(value)
    }
}

// --------------------------------------------------------------------
// OAuth1API

/// Upstream `OAuth1API`'s user settings: `api-secret`, `access-token` and
/// `access-token-secret`. Debug output never shows their values.
#[derive(Clone)]
pub(crate) struct OAuth1Settings {
    /// Configured consumer secret; `None` uses the site's default and an
    /// empty string disables signing.
    pub api_secret: Option<String>,
    pub access_token: Option<String>,
    pub access_token_secret: Option<String>,
    /// `access-token` is unset or `"cache"`: use the pair `oauth:SITE`
    /// cached for the API key instead of the configured tokens.
    pub token_cache: bool,
}

impl Default for OAuth1Settings {
    fn default() -> Self {
        Self {
            api_secret: None,
            access_token: None,
            access_token_secret: None,
            token_cache: true,
        }
    }
}

impl std::fmt::Debug for OAuth1Settings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let set = |value: &Option<String>| value.as_ref().map(|_| "<redacted>");
        f.debug_struct("OAuth1Settings")
            .field("api_secret", &set(&self.api_secret))
            .field("access_token", &set(&self.access_token))
            .field("access_token_secret", &set(&self.access_token_secret))
            .field("token_cache", &self.token_cache)
            .finish()
    }
}

/// A bounded printable-ASCII credential, as accepted from configuration and
/// the cache.
fn credential(value: &str) -> bool {
    (1..=1024).contains(&value.len()) && value.bytes().all(|b| b.is_ascii_graphic())
}

impl OAuth1Settings {
    pub const KEYS: [&'static str; 3] = ["api-secret", "access-token", "access-token-secret"];

    /// Read the settings through `get`; `site` names the site in errors,
    /// which never include the rejected value.
    pub fn parse<'v>(site: &str, get: impl Fn(&str) -> Option<&'v Value>) -> Result<Self> {
        let mut out = Self::default();
        for key in Self::KEYS {
            let Some(value) = get(key).filter(|value| !value.is_null()) else {
                continue;
            };
            if key == "access-token" {
                out.token_cache = value.as_str() == Some("cache");
            }
            let value = match value {
                Value::Bool(false) => None,
                Value::String(s) if s.is_empty() || s == "cache" => None,
                Value::String(s) if credential(s) => Some(s.clone()),
                _ => return Err(error(format!("invalid {site} setting: {key}"))),
            };
            match key {
                // An explicitly empty secret disables OAuth, as upstream.
                "api-secret" => out.api_secret = Some(value.unwrap_or_default()),
                "access-token" => out.access_token = value,
                _ => out.access_token_secret = value,
            }
        }
        Ok(out)
    }

    /// Upstream `OAuth1API.__init__`: the signing credentials once the API
    /// key, the secret (else `default_secret`) and a token pair (configured,
    /// or cached under `('CATEGORY', API_KEY)`) are all non-empty.
    pub async fn resolve(
        &self,
        category: &str,
        api_key: &str,
        default_secret: &str,
        cache: Option<&Cache>,
    ) -> Option<OAuth1Credentials> {
        let consumer_secret = self.api_secret.as_deref().unwrap_or(default_secret);
        let (token, token_secret) = if self.token_cache {
            match cache {
                Some(cache) => cached_tokens(cache, category, api_key).await,
                None => (None, None),
            }
        } else {
            (self.access_token.clone(), self.access_token_secret.clone())
        };
        let nonempty = |value: &str| (!value.is_empty()).then(|| value.to_owned());
        Some(OAuth1Credentials {
            consumer_key: nonempty(api_key)?,
            consumer_secret: nonempty(consumer_secret)?,
            token: token.filter(|t| !t.is_empty())?,
            token_secret: token_secret.filter(|t| !t.is_empty())?,
        })
    }
}

/// Upstream's `_token_cache-('CATEGORY', 'API_KEY')` cache key, written by
/// `oauth:CATEGORY`.
pub(crate) fn token_cache_key(category: &str, api_key: &str) -> String {
    crate::cache::key(
        "oauth",
        "_token_cache",
        &crate::cache::tuple_repr(&[category, api_key]),
    )
}

/// The cached `(token, secret)` pair for `category` and `api_key`; values
/// that are missing, unreadable or not printable ASCII count as unset.
pub(crate) async fn cached_tokens(
    cache: &Cache,
    category: &str,
    api_key: &str,
) -> (Option<String>, Option<String>) {
    let pair = cache
        .get(&token_cache_key(category, api_key))
        .await
        .ok()
        .flatten();
    let valid = |value: &Value| value.as_str().filter(|s| credential(s)).map(str::to_owned);
    match pair.as_ref().and_then(Value::as_array).map(Vec::as_slice) {
        Some([token, secret]) => (valid(token), valid(secret)),
        _ => (None, None),
    }
}

/// Complete OAuth 1.0a credentials signing a site's API requests (upstream
/// `OAuth1Session`). Debug output shows none of them.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct OAuth1Credentials {
    pub consumer_key: String,
    pub consumer_secret: String,
    pub token: String,
    pub token_secret: String,
}

impl std::fmt::Debug for OAuth1Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OAuth1Credentials { .. }")
    }
}

impl OAuth1Credentials {
    pub fn signer(&self) -> OAuth1<'_> {
        OAuth1 {
            consumer_key: &self.consumer_key,
            consumer_secret: &self.consumer_secret,
            token: Some(&self.token),
            token_secret: &self.token_secret,
        }
    }

    /// A sensitive `Authorization` header value signing `method` `url`.
    pub fn header(&self, method: &str, url: &Url) -> Result<reqwest::header::HeaderValue> {
        self.signer().header(method, url)
    }
}

/// Test support: check the OAuth 1.0a `Authorization` header of a raw HTTP
/// request against `root` by recomputing its signature; returns the decoded
/// `oauth_*` fields.
#[cfg(test)]
pub(crate) fn verify_signed_request(
    request: &str,
    root: &Url,
    consumer_secret: &str,
    token_secret: &str,
) -> BTreeMap<String, String> {
    fn decode(raw: &str) -> String {
        let bytes = raw.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' {
                out.push(u8::from_str_radix(&raw[i + 1..i + 3], 16).unwrap());
                i += 3;
            } else {
                out.push(bytes[i]);
                i += 1;
            }
        }
        String::from_utf8(out).unwrap()
    }
    let mut lines = request.lines();
    let mut start = lines.next().unwrap().split(' ');
    let method = start.next().unwrap();
    let url = root.join(start.next().unwrap()).unwrap();
    let header = lines
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("authorization")
                .then(|| value.trim().to_owned())
        })
        .expect("signed request has an Authorization header");
    let fields: BTreeMap<String, String> = header
        .strip_prefix("OAuth ")
        .unwrap()
        .split(',')
        .map(|field| {
            let (key, value) = field.split_once('=').unwrap();
            let value = value.strip_prefix('"').unwrap().strip_suffix('"').unwrap();
            (key.to_owned(), decode(value))
        })
        .collect();
    let params: Vec<(&str, String)> = fields
        .iter()
        .filter(|(key, _)| key.as_str() != "oauth_signature")
        .map(|(key, value)| (key.as_str(), value.clone()))
        .collect();
    assert_eq!(fields["oauth_signature_method"], "HMAC-SHA1");
    assert_eq!(fields["oauth_version"], "1.0");
    assert_eq!(fields["oauth_nonce"].len(), 16);
    assert_eq!(
        fields["oauth_signature"],
        signature(method, &url, &params, consumer_secret, token_secret),
        "signature mismatch for {url}"
    );
    fields
}

// --------------------------------------------------------------------
// Settings

/// `extractor.oauth.*` settings plus the raw site credentials the flows use.
#[derive(Clone)]
pub struct Settings {
    /// Write obtained tokens to the credential cache.
    pub cache: bool,
    /// Try to open authorization URLs in a web browser.
    pub browser: bool,
    /// Local port receiving the redirect (0 picks any free port).
    pub port: u16,
    /// `extractor.CATEGORY.KEY` values: client IDs, API keys and token
    /// settings, which select credentials and the final message.
    pub values: BTreeMap<(String, String), Value>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            cache: true,
            browser: true,
            port: DEFAULT_PORT,
            values: BTreeMap::new(),
        }
    }
}

impl std::fmt::Debug for Settings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Settings { [redacted] }")
    }
}

/// Settings keys captured from site scopes for `oauth:SITE`.
pub const CREDENTIAL_KEYS: &[&str] = &[
    "client-id",
    "client-secret",
    "api-key",
    "api-secret",
    "refresh-token",
    "access-token",
    "access-token-secret",
    "username",
];

impl Settings {
    fn value(&self, category: &str, key: &str) -> Option<&Value> {
        self.values
            .get(&(category.to_owned(), key.to_owned()))
            .filter(|value| !value.is_null())
    }

    /// A string or integer setting; other types count as unset.
    fn string(&self, category: &str, key: &str) -> Option<String> {
        match self.value(category, key)? {
            Value::String(value) if !value.is_empty() => Some(value.clone()),
            Value::Number(number) => Some(number.to_string()),
            _ => None,
        }
    }
}

// --------------------------------------------------------------------
// Inputs

/// A parsed `oauth:SITE` input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Deviantart,
    Flickr,
    Reddit,
    Smugmug,
    Tumblr,
    Pixiv,
    Mastodon(String),
}

/// Parse the part after `oauth:` (upstream patterns `oauth:SITE$` and
/// `oauth:mastodon:(?:https?://)?([^/?#]+)`).
pub fn target(value: &str) -> Result<Target> {
    Ok(match value {
        "deviantart" => Target::Deviantart,
        "flickr" => Target::Flickr,
        "reddit" => Target::Reddit,
        "smugmug" => Target::Smugmug,
        "tumblr" => Target::Tumblr,
        "pixiv" => Target::Pixiv,
        _ => {
            let instance = value
                .strip_prefix("mastodon:")
                .ok_or_else(|| error("unsupported oauth: input"))?;
            let instance = instance
                .strip_prefix("https://")
                .or_else(|| instance.strip_prefix("http://"))
                .unwrap_or(instance);
            let instance = instance
                .split(['/', '?', '#'])
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            // A host name, optionally with a port.
            if instance.is_empty()
                || instance.len() > 253
                || !instance
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':'))
                || Url::parse(&format!("https://{instance}/")).is_err()
            {
                return Err(error("invalid oauth:mastodon instance"));
            }
            Target::Mastodon(instance)
        }
    })
}

impl Target {
    /// Upstream's subcategory: the settings scope for credentials.
    pub fn site(&self) -> &'static str {
        match self {
            Self::Deviantart => "deviantart",
            Self::Flickr => "flickr",
            Self::Reddit => "reddit",
            Self::Smugmug => "smugmug",
            Self::Tumblr => "tumblr",
            Self::Pixiv => "pixiv",
            Self::Mastodon(_) => "mastodon",
        }
    }
}

// --------------------------------------------------------------------
// Interactive flows

type Browser = dyn Fn(&Url, SocketAddr) -> bool + Send + Sync;
type Input = dyn Fn(&str) -> Result<String> + Send + Sync;

/// One `oauth:SITE` run.
pub struct Flow {
    client: reqwest::Client,
    settings: Settings,
    cache: Option<Cache>,
    timeout: Duration,
    /// Test override for every endpoint's origin.
    root: Option<Url>,
    browser: Box<Browser>,
    input: Arc<Input>,
    /// Captured standard output, for tests.
    output: Option<Arc<Mutex<Vec<u8>>>>,
}

/// An OAuth 2.0 authorization code grant (upstream
/// `_oauth2_authorization_code_grant`).
struct Grant<'a> {
    site: &'a str,
    client_id: String,
    client_secret: String,
    default_id: &'a str,
    auth_url: &'a str,
    token_url: &'a str,
    redirect_uri: &'a str,
    scope: &'a str,
    /// The token to keep: `refresh_token` or `access_token`.
    key: &'a str,
    cache_key: String,
    instance: Option<&'a str>,
    user_agent: &'a str,
}

/// An accepted redirect request, answered once the flow completes.
struct Callback {
    params: BTreeMap<String, String>,
    stream: TcpStream,
}

impl Flow {
    pub fn new(
        settings: Settings,
        cache: Option<Cache>,
        network: &NetworkOptions,
        timeout: Duration,
    ) -> Result<Self> {
        Ok(Self {
            client: network.client(USER_AGENT, timeout)?,
            settings,
            cache,
            timeout,
            root: None,
            browser: Box::new(|url, _| open_browser(url.as_str())),
            input: Arc::new(prompt),
            output: None,
        })
    }

    /// Send every request to `root` instead; intended for local tests.
    pub fn with_root(mut self, root: Url) -> Self {
        self.root = Some(root);
        self
    }

    /// Replace the browser launcher; it receives the authorization URL and
    /// the listener address and reports whether it opened the URL.
    pub fn with_browser(
        mut self,
        browser: impl Fn(&Url, SocketAddr) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.browser = Box::new(browser);
        self
    }

    /// Replace the terminal prompt used by `oauth:pixiv`.
    pub fn with_input(
        mut self,
        input: impl Fn(&str) -> Result<String> + Send + Sync + 'static,
    ) -> Self {
        self.input = Arc::new(input);
        self
    }

    /// Capture standard output instead of printing it.
    pub fn with_output(mut self, output: Arc<Mutex<Vec<u8>>>) -> Self {
        self.output = Some(output);
        self
    }

    fn print(&self, text: &str) {
        match &self.output {
            Some(output) => output
                .lock()
                .expect("output lock")
                .extend_from_slice(text.as_bytes()),
            None => {
                use std::io::Write;
                let mut stdout = std::io::stdout().lock();
                let _ = stdout.write_all(text.as_bytes());
                let _ = stdout.flush();
            }
        }
    }

    fn endpoint(&self, url: &str) -> Result<Url> {
        let mut url = Url::parse(url).map_err(|_| error("invalid OAuth endpoint"))?;
        if let Some(root) = &self.root {
            let mut local = root
                .join(url.path())
                .map_err(|_| error("invalid OAuth root"))?;
            local.set_query(url.query());
            url = local;
        }
        Ok(url)
    }

    /// Run the flow for an `oauth:` input (the text after the prefix).
    pub async fn run(&self, input: &str) -> Result<()> {
        let target = target(input)?;
        let mut cache = None;
        if self.settings.cache {
            match &self.cache {
                Some(handle) if handle.available().await => cache = Some(handle),
                _ => eprintln!("cache file is not writeable"),
            }
        }
        match &target {
            Target::Flickr => {
                self.oauth1(
                    &target,
                    cache,
                    "",
                    "",
                    REDIRECT_URI_HTTPS,
                    "https://www.flickr.com/services/oauth/request_token",
                    "https://www.flickr.com/services/oauth/authorize",
                    "https://www.flickr.com/services/oauth/access_token",
                )
                .await
            }
            Target::Smugmug => {
                self.oauth1(
                    &target,
                    cache,
                    crate::sites::art::smugmug::API_KEY,
                    crate::sites::art::smugmug::API_SECRET,
                    REDIRECT_URI_LOCALHOST,
                    "https://api.smugmug.com/services/oauth/1.0a/getRequestToken",
                    "https://api.smugmug.com/services/oauth/1.0a/authorize",
                    "https://api.smugmug.com/services/oauth/1.0a/getAccessToken",
                )
                .await
            }
            Target::Tumblr => {
                self.oauth1(
                    &target,
                    cache,
                    crate::tumblr::DEFAULT_API_KEY,
                    crate::tumblr::DEFAULT_API_SECRET,
                    REDIRECT_URI_LOCALHOST,
                    "https://www.tumblr.com/oauth/request_token",
                    "https://www.tumblr.com/oauth/authorize",
                    "https://www.tumblr.com/oauth/access_token",
                )
                .await
            }
            Target::Deviantart => {
                let (default_id, default_secret) = (
                    crate::deviantart::CLIENT_ID,
                    crate::deviantart::CLIENT_SECRET,
                );
                // Like upstream, a custom client without a secret still
                // falls back to the default secret.
                let client_id = self
                    .settings
                    .string("deviantart", "client-id")
                    .unwrap_or_else(|| default_id.to_owned());
                let client_secret = self
                    .settings
                    .string("deviantart", "client-secret")
                    .unwrap_or_else(|| default_secret.to_owned());
                let cache_key = crate::cache::key(
                    "extractor.deviantart",
                    "_refresh_token_cache",
                    &format!("#{client_id}"),
                );
                self.oauth2(
                    &target,
                    cache,
                    Grant {
                        site: "deviantart",
                        client_id,
                        client_secret,
                        default_id,
                        auth_url: "https://www.deviantart.com/oauth2/authorize",
                        token_url: "https://www.deviantart.com/oauth2/token",
                        redirect_uri: REDIRECT_URI_HTTPS,
                        scope: "browse user.manage",
                        key: "refresh_token",
                        cache_key,
                        instance: None,
                        user_agent: USER_AGENT,
                    },
                )
                .await
            }
            Target::Reddit => {
                let default_id = crate::reddit::CLIENT_ID;
                let client_id = self
                    .settings
                    .string("reddit", "client-id")
                    .unwrap_or_else(|| default_id.to_owned());
                let cache_key = crate::cache::key(
                    "extractor.reddit",
                    "_refresh_token_cache",
                    &format!("#{client_id}"),
                );
                self.oauth2(
                    &target,
                    cache,
                    Grant {
                        site: "reddit",
                        client_id,
                        client_secret: String::new(),
                        default_id,
                        auth_url: "https://www.reddit.com/api/v1/authorize",
                        token_url: "https://www.reddit.com/api/v1/access_token",
                        redirect_uri: REDIRECT_URI_LOCALHOST,
                        scope: "read history",
                        key: "refresh_token",
                        cache_key,
                        instance: None,
                        user_agent: crate::reddit::USER_AGENT,
                    },
                )
                .await
            }
            Target::Mastodon(instance) => self.mastodon(&target, cache, instance).await,
            Target::Pixiv => self.pixiv(cache).await,
        }
    }

    async fn store(&self, cache: Option<&Cache>, key: &str, value: &Value, message: &str) {
        if let Some(cache) = cache {
            match cache.set(key, value, None).await {
                Ok(()) => eprintln!("{message}"),
                Err(e) => eprintln!("cache update failed: {e}"),
            }
        }
    }

    /// Upstream `_oauth1_authorization_flow`.
    #[allow(clippy::too_many_arguments)]
    async fn oauth1(
        &self,
        target: &Target,
        cache: Option<&Cache>,
        default_key: &str,
        default_secret: &str,
        redirect_uri: &str,
        request_token_url: &str,
        authorize_url: &str,
        access_token_url: &str,
    ) -> Result<()> {
        let site = target.site();
        let api_key = self
            .settings
            .string(site, "api-key")
            .unwrap_or_else(|| default_key.to_owned());
        let api_secret = self
            .settings
            .string(site, "api-secret")
            .unwrap_or_else(|| default_secret.to_owned());
        if api_key.is_empty() || api_secret.is_empty() {
            return Err(error(format!(
                "oauth:{site} requires extractor.{site}.api-key and api-secret"
            )));
        }
        eprintln!(
            "Using {} {site} API key ({api_key})",
            if api_key == default_key {
                "default"
            } else {
                "custom"
            }
        );

        // get a request token
        let mut url = self.endpoint(request_token_url)?;
        url.query_pairs_mut()
            .append_pair("oauth_callback", redirect_uri);
        let signer = OAuth1 {
            consumer_key: &api_key,
            consumer_secret: &api_secret,
            token: None,
            token_secret: "",
        };
        let data = self.oauth1_request(&signer, url).await?;
        let (request_token, request_secret) = oauth1_tokens(&data)?;

        // get the user's authorization
        let listener = self.listen().await?;
        let mut url = self.endpoint(authorize_url)?;
        url.query_pairs_mut()
            .append_pair("oauth_token", &request_token)
            .append_pair("perms", "read");
        self.open(&url, &listener)?;
        let mut callback = self
            .recv(&listener, &["oauth_token", "oauth_verifier"])
            .await?;
        if callback.params.get("oauth_token") != Some(&request_token) {
            self.send(&mut callback, "'oauth_token' mismatch.\n").await;
            return Err(error("OAuth response does not match the request token"));
        }
        let Some(verifier) = callback.params.get("oauth_verifier").cloned() else {
            let message = "Authorization denied or incomplete.\n";
            self.send(&mut callback, message).await;
            return Err(error("OAuth authorization was not granted"));
        };

        // exchange the request token for an access token
        let mut url = self.endpoint(access_token_url)?;
        url.query_pairs_mut()
            .append_pair("oauth_token", &request_token)
            .append_pair("oauth_verifier", &verifier);
        let signer = OAuth1 {
            token_secret: &request_secret,
            ..signer
        };
        let data = match self.oauth1_request(&signer, url).await {
            Ok(data) => data,
            Err(e) => {
                self.send(&mut callback, "Failed to obtain an access token.\n")
                    .await;
                return Err(e);
            }
        };
        let (token, token_secret) = oauth1_tokens(&data)?;

        // write to cache
        let key = token_cache_key(site, &api_key);
        self.store(
            cache,
            &key,
            &Value::from(vec![token.clone(), token_secret.clone()]),
            "Writing tokens to cache",
        )
        .await;

        // display tokens
        let message = self.message(
            site,
            &["access-token", "access-token-secret"],
            &[&token, &token_secret],
            cache.is_some(),
        );
        self.send(&mut callback, &message).await;
        Ok(())
    }

    async fn oauth1_request(&self, signer: &OAuth1<'_>, url: Url) -> Result<Vec<(String, String)>> {
        let header = signer.header("GET", &url)?;
        let (status, body) = self
            .fetch(
                self.client
                    .get(url)
                    .header(reqwest::header::AUTHORIZATION, header),
            )
            .await?;
        if !status.is_success() {
            return Err(error(format!("OAuth token request returned HTTP {status}")));
        }
        Ok(url::form_urlencoded::parse(&body)
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect())
    }

    /// Upstream `_oauth2_authorization_code_grant`.
    async fn oauth2(&self, target: &Target, cache: Option<&Cache>, grant: Grant<'_>) -> Result<()> {
        eprintln!(
            "Using {} {} client ID ({})",
            if grant.client_id == grant.default_id {
                "default"
            } else {
                "custom"
            },
            grant.instance.unwrap_or(grant.site),
            grant.client_id
        );
        let state = format!("gallery-dl_{}_{}", grant.site, nonce(16));

        // receive an authorization code
        let listener = self.listen().await?;
        let mut url = self.endpoint(grant.auth_url)?;
        url.query_pairs_mut()
            .append_pair("client_id", &grant.client_id)
            .append_pair("response_type", "code")
            .append_pair("state", &state)
            .append_pair("redirect_uri", grant.redirect_uri)
            .append_pair("duration", "permanent")
            .append_pair("scope", grant.scope);
        self.open(&url, &listener)?;
        let mut callback = self.recv(&listener, &["state", "code", "error"]).await?;

        // check authorization response
        if callback.params.get("state") != Some(&state) {
            let message = format!(
                "'state' mismatch: expected {state}, got {}.\n",
                callback
                    .params
                    .get("state")
                    .map_or("None".to_owned(), |s| printable(s))
            );
            self.send(&mut callback, &message).await;
            return Err(error("OAuth response 'state' mismatch"));
        }
        if callback.params.contains_key("error") {
            let message = remote_error(&callback.params);
            self.send(&mut callback, &message).await;
            return Err(error("OAuth authorization failed"));
        }
        let Some(code) = callback.params.get("code").cloned() else {
            self.send(&mut callback, "No authorization code received.\n")
                .await;
            return Err(error("OAuth response contains no authorization code"));
        };

        // exchange authorization code for a token
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", "authorization_code")
            .append_pair("code", &code)
            .append_pair("redirect_uri", grant.redirect_uri)
            .finish();
        let url = self.endpoint(grant.token_url)?;
        let request = self
            .client
            .post(url)
            .header(reqwest::header::USER_AGENT, grant.user_agent)
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .header(reqwest::header::ACCEPT, "application/json")
            .basic_auth(&grant.client_id, Some(&grant.client_secret))
            .body(body);
        let data = match self.fetch_json(request).await {
            Ok(data) => data,
            Err(e) => {
                self.send(&mut callback, "Failed to obtain a token.\n")
                    .await;
                return Err(e);
            }
        };

        // check token response
        if data.get("error").is_some() {
            let message = remote_error(&value_params(&data));
            self.send(&mut callback, &message).await;
            return Err(error("OAuth token request failed"));
        }
        let Some(token) = data
            .get(grant.key)
            .and_then(Value::as_str)
            .filter(|t| valid_token(t))
        else {
            self.send(&mut callback, "The token response contains no token.\n")
                .await;
            return Err(error("OAuth token response contains no token"));
        };
        let token_name = grant.key.replace('_', "-");

        // write to cache
        self.store(
            cache,
            &grant.cache_key,
            &Value::from(token),
            &format!("Writing '{token_name}' to cache"),
        )
        .await;

        // display token
        let site = match (target, grant.instance) {
            (Target::Mastodon(_), Some(instance)) => {
                crate::mastodon::instance_category(instance).unwrap_or("mastodon")
            }
            _ => grant.site,
        };
        let message = self.message(site, &[&token_name], &[token], cache.is_some());
        self.send(&mut callback, &message).await;
        Ok(())
    }

    /// `oauth:mastodon:INSTANCE`, registering an application for instances
    /// without one of upstream's built-in applications.
    async fn mastodon(&self, target: &Target, cache: Option<&Cache>, instance: &str) -> Result<()> {
        let redirect_uri = REDIRECT_URI_LOCALHOST;
        let (client_id, client_secret) = match crate::mastodon::application(instance) {
            Some((id, secret)) => (id.to_owned(), secret.to_owned()),
            None => {
                let key = crate::cache::key("extractor.oauth", "_register", instance);
                let cached = match cache {
                    Some(cache) => cache.get(&key).await.ok().flatten(),
                    None => None,
                };
                let application = match cached {
                    Some(application) => application,
                    None => {
                        let application = self.register(instance, redirect_uri).await?;
                        if let Some(cache) = cache
                            && cache.set(&key, &application, None).await.is_err()
                        {
                            eprintln!("cache update failed");
                        }
                        application
                    }
                };
                let field = |name| {
                    application
                        .get(name)
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .ok_or_else(|| error("invalid cached Mastodon application"))
                };
                (field("client-id")?, field("client-secret")?)
            }
        };
        let auth_url = format!("https://{instance}/oauth/authorize");
        let token_url = format!("https://{instance}/oauth/token");
        let cache_key = crate::cache::key("extractor.mastodon", "_access_token_cache", instance);
        let default_id = client_id.clone();
        self.oauth2(
            target,
            cache,
            Grant {
                site: "mastodon",
                default_id: &default_id,
                client_id,
                client_secret,
                auth_url: &auth_url,
                token_url: &token_url,
                redirect_uri,
                scope: "read",
                key: "access_token",
                cache_key,
                instance: Some(instance),
                user_agent: USER_AGENT,
            },
        )
        .await
    }

    /// Upstream `OAuthMastodon._register`; the client secret is not printed.
    async fn register(&self, instance: &str, redirect_uri: &str) -> Result<Value> {
        eprintln!("Registering application for '{instance}'");
        let url = self.endpoint(&format!("https://{instance}/api/v1/apps"))?;
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("client_name", &format!("gdl:{}", nonce(8)))
            .append_pair("redirect_uris", redirect_uri)
            .append_pair("scopes", "read")
            .finish();
        let data = self
            .fetch_json(
                self.client
                    .post(url)
                    .header(
                        reqwest::header::CONTENT_TYPE,
                        "application/x-www-form-urlencoded",
                    )
                    .body(body),
            )
            .await?;
        let field = |name| {
            data.get(name)
                .and_then(Value::as_str)
                .filter(|value| valid_token(value))
        };
        let (Some(id), Some(secret)) = (field("client_id"), field("client_secret")) else {
            return Err(error("Failed to register new application"));
        };
        eprintln!("client-id:\n{id}");
        Ok(serde_json::json!({"client-id": id, "client-secret": secret}))
    }

    /// `oauth:pixiv`: upstream's PKCE login, completed by pasting the code.
    async fn pixiv(&self, cache: Option<&Cache>) -> Result<()> {
        let verifier = generate_token(32)?;
        let mut url = self.endpoint("https://app-api.pixiv.net/web/v1/login")?;
        url.query_pairs_mut()
            .append_pair("code_challenge", &pkce_challenge(&verifier))
            .append_pair("code_challenge_method", "S256")
            .append_pair("client", "pixiv-android");
        self.announce(&url, None);
        self.print(
            "1) Open your browser's Developer Tools (F12) and switch to the Network tab
2) Login
3) Select the last network monitor entry ('callback?state=...')
4) Copy its 'code' query parameter, paste it below, and press Enter

- This 'code' will expire 30 seconds after logging in.
- Copy-pasting more than just the 'code' value will work as well,
  like the entire URL or several query parameters.

",
        );
        // The prompt blocks, so it runs off the async workers; Ctrl+C
        // still ends the process meanwhile.
        let prompt = self.input.clone();
        let input = tokio::task::spawn_blocking(move || prompt("code: ")).await??;
        let code = input
            .rsplit_once('=')
            .map_or(input.as_str(), |(_, code)| code)
            .trim()
            .to_owned();

        let client_id = self
            .settings
            .string("pixiv", "client-id")
            .unwrap_or_else(|| crate::pixiv::CLIENT_ID.to_owned());
        let client_secret = self
            .settings
            .string("pixiv", "client-secret")
            .unwrap_or_else(|| crate::pixiv::CLIENT_SECRET.to_owned());
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("client_id", &client_id)
            .append_pair("client_secret", &client_secret)
            .append_pair("code", &code)
            .append_pair("code_verifier", &verifier)
            .append_pair("grant_type", "authorization_code")
            .append_pair("include_policy", "true")
            .append_pair(
                "redirect_uri",
                "https://app-api.pixiv.net/web/v1/users/auth/pixiv/callback",
            )
            .finish();
        let url = self.endpoint("https://oauth.secure.pixiv.net/auth/token")?;
        let data = self
            .fetch_json(
                self.client
                    .post(url)
                    .header(reqwest::header::USER_AGENT, PIXIV_USER_AGENT)
                    .header(
                        reqwest::header::CONTENT_TYPE,
                        "application/x-www-form-urlencoded",
                    )
                    .body(body),
            )
            .await?;
        if let Some(code) = data.get("error") {
            let code = code.as_str().map(printable).unwrap_or_default();
            self.print(&format!("\n{}", remote_error(&value_params(&data))));
            if matches!(code.as_str(), "invalid_request" | "invalid_grant") {
                self.print("'code' expired, try again\n\n");
            }
            return Err(error("Pixiv rejected the authorization code"));
        }
        let data = data.get("response").unwrap_or(&data);
        let token = data
            .get("refresh_token")
            .and_then(Value::as_str)
            .filter(|t| valid_token(t))
            .ok_or_else(|| error("Pixiv OAuth response contains no refresh token"))?;
        let username = self
            .settings
            .string("pixiv", "username")
            .unwrap_or_else(|| "None".to_owned());
        let key = crate::cache::key("extractor.pixiv", "_refresh_token_cache", &username);
        self.store(
            cache,
            &key,
            &Value::from(token),
            "Writing 'refresh-token' to cache",
        )
        .await;
        self.print(&self.message("pixiv", &["refresh-token"], &[token], cache.is_some()));
        Ok(())
    }

    /// Upstream `_generate_message`.
    fn message(&self, site: &str, names: &[&str], values: &[&str], cached: bool) -> String {
        let (vh, va, is, it) = if names.len() == 1 {
            ("This value has", "this value", "is", "it")
        } else {
            ("These values have", "these values", "are", "them")
        };
        let key = names
            .iter()
            .map(|name| format!("'{name}'"))
            .collect::<Vec<_>>()
            .join(" and ");
        let mut message = format!("\nYour {key} {is}\n\n{}\n\n", values.join("\n"));
        let option = self.settings.value(site, names[0]);
        if cached && option.is_none_or(|option| option == "cache") {
            message.push_str(vh);
            message.push_str(" been cached and will automatically be used.\n");
        } else {
            message.push_str(&format!("Put {va} into your configuration file as \n"));
            message.push_str(
                &names
                    .iter()
                    .map(|name| format!("'extractor.{site}.{name}'"))
                    .collect::<Vec<_>>()
                    .join(" and\n"),
            );
            if cached {
                message.push_str(&format!(
                    "\nor set\n'extractor.{site}.{}' to \"cache\"",
                    names[0]
                ));
            }
            message.push_str(&format!("\nto use {it}.\n"));
        }
        message
    }

    async fn listen(&self) -> Result<TcpListener> {
        TcpListener::bind((LOCALHOST, self.settings.port))
            .await
            .map_err(|_| {
                error(format!(
                    "cannot listen on 127.0.0.1:{} for the OAuth redirect",
                    self.settings.port
                ))
            })
    }

    fn announce(&self, url: &Url, listener: Option<SocketAddr>) {
        let address = listener.unwrap_or_else(|| SocketAddr::from((LOCALHOST, 0)));
        if self.settings.browser && (self.browser)(url, address) {
            eprintln!("Opening URL in your browser:");
        } else {
            eprintln!("Please open this URL in your browser:");
        }
        self.print(&format!("\n{url}\n\n"));
    }

    /// Show the authorization URL; the listener is already bound, so even
    /// an immediate redirect is received.
    fn open(&self, url: &Url, listener: &TcpListener) -> Result<()> {
        let address = listener
            .local_addr()
            .map_err(|_| error("OAuth listener has no address"))?;
        self.announce(url, Some(address));
        self.print("Waiting for response. (Cancel with Ctrl+c)\n");
        Ok(())
    }

    /// Upstream `recv`: wait for the redirect, answering unrelated requests
    /// (such as a browser's favicon request) with 404.
    async fn recv(&self, listener: &TcpListener, expected: &[&str]) -> Result<Callback> {
        loop {
            let (mut stream, _) = listener
                .accept()
                .await
                .map_err(|_| error("Failed to receive OAuth response"))?;
            let request = tokio::time::timeout(self.timeout, read_request(&mut stream)).await;
            match request {
                Ok(Some(params)) if expected.iter().any(|key| params.contains_key(*key)) => {
                    return Ok(Callback { params, stream });
                }
                _ => {
                    let _ = stream
                        .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .await;
                    let _ = stream.shutdown().await;
                }
            }
        }
    }

    /// Upstream `send`: print `message` and answer the browser with it.
    async fn send(&self, callback: &mut Callback, message: &str) {
        self.print(message);
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{message}",
            message.len()
        );
        let _ = callback.stream.write_all(response.as_bytes()).await;
        let _ = callback.stream.shutdown().await;
    }

    async fn fetch(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<(reqwest::StatusCode, Vec<u8>)> {
        let call = async {
            let mut response = request
                .timeout(self.timeout)
                .send()
                .await
                .map_err(|_| error("OAuth request failed; check network access"))?;
            let status = response.status();
            let mut body = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| error("could not read the OAuth response"))?
            {
                if body.len() + chunk.len() > MAX_RESPONSE {
                    return Err(error("OAuth response is too large"));
                }
                body.extend_from_slice(&chunk);
            }
            Ok((status, body))
        };
        tokio::time::timeout(self.timeout, call)
            .await
            .map_err(|_| error("OAuth request timed out"))?
    }

    /// A JSON reply; error statuses with a JSON body are returned for the
    /// caller to report, as upstream does.
    async fn fetch_json(&self, request: reqwest::RequestBuilder) -> Result<Value> {
        let (status, body) = self.fetch(request).await?;
        match serde_json::from_slice::<Value>(&body) {
            Ok(data @ Value::Object(_)) => Ok(data),
            _ => Err(error(format!(
                "OAuth endpoint returned HTTP {status} without JSON"
            ))),
        }
    }
}

/// Tokens are printable ASCII without spaces.
fn valid_token(token: &str) -> bool {
    (1..=8192).contains(&token.len()) && token.bytes().all(|b| b.is_ascii_graphic())
}

/// A server-provided value, truncated and stripped of control characters.
fn printable(value: &str) -> String {
    value
        .chars()
        .filter(|c| !c.is_control())
        .take(200)
        .collect()
}

fn value_params(data: &Value) -> BTreeMap<String, String> {
    ["error", "error_description"]
        .into_iter()
        .filter_map(|key| {
            let value = data.get(key)?;
            Some((
                key.to_owned(),
                value
                    .as_str()
                    .map_or_else(|| value.to_string(), str::to_owned),
            ))
        })
        .collect()
}

/// Upstream `error`: only the error code and description are shown.
fn remote_error(params: &BTreeMap<String, String>) -> String {
    let field = |key| params.get(key).map(|value| printable(value));
    let mut message = String::from("Remote server reported an error:\n\n");
    message.push_str(&field("error").unwrap_or_else(|| "unknown error".into()));
    if let Some(description) = field("error_description") {
        message.push_str(&format!(" ({description})"));
    }
    message.push('\n');
    message
}

fn oauth1_tokens(data: &[(String, String)]) -> Result<(String, String)> {
    let field = |name| {
        data.iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
            .filter(|value| valid_token(value))
            .ok_or_else(|| error("OAuth response contains no token"))
    };
    Ok((field("oauth_token")?, field("oauth_token_secret")?))
}

/// The query parameters of one HTTP request head, or None when malformed.
async fn read_request(stream: &mut TcpStream) -> Option<BTreeMap<String, String>> {
    let mut request = Vec::new();
    let mut buffer = [0u8; 2048];
    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
        let count = stream.read(&mut buffer).await.ok()?;
        if count == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..count]);
        if request.len() > MAX_REQUEST {
            return None;
        }
    }
    let head = std::str::from_utf8(&request).ok()?;
    let mut line = head.lines().next()?.split(' ');
    let (method, target) = (line.next()?, line.next()?);
    if method != "GET" || !target.starts_with('/') {
        return None;
    }
    let query = target.split_once('?').map_or("", |(_, query)| query);
    Some(
        url::form_urlencoded::parse(query.as_bytes())
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect(),
    )
}

/// Open `url` with the platform's URL handler, if one starts.
fn open_browser(url: &str) -> bool {
    let mut command = if cfg!(target_os = "macos") {
        std::process::Command::new("open")
    } else if cfg!(windows) {
        let mut command = std::process::Command::new("rundll32");
        command.arg("url.dll,FileProtocolHandler");
        command
    } else {
        std::process::Command::new("xdg-open")
    };
    command
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    match command.spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || child.wait());
            true
        }
        Err(_) => false,
    }
}

/// Read one line from an interactive terminal (upstream `Extractor.input`).
fn prompt(prompt: &str) -> Result<String> {
    use std::io::{BufRead, IsTerminal, Write};
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        return Err(error(format!(
            "User input required ({})",
            prompt.trim().trim_end_matches(':')
        )));
    }
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(prompt.as_bytes());
    let _ = stdout.flush();
    let mut line = String::new();
    stdin
        .lock()
        .read_line(&mut line)
        .map_err(|_| error("cannot read input"))?;
    Ok(line)
}

#[cfg(test)]
#[path = "oauth_tests.rs"]
mod tests;
