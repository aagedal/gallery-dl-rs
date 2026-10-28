//! Upstream `oauth:` helpers: link accounts and print the resulting tokens
//! for the configuration file. gallery-dl-rs keeps no cache, so tokens are
//! always displayed rather than cached.
use crate::{Result, error, sites::gdl::oauth1_header};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    io::{BufRead, Read, Write},
    net::TcpStream,
};
use url::Url;

const REDIRECT_URI_LOCALHOST: &str = "http://localhost:6414/";
const REDIRECT_URI_HTTPS: &str = "https://mikf.github.io/gallery-dl/oauth-redirect.html";

/// Settings lookup: `(category, key)` → value, following upstream's
/// `extractor.CATEGORY.KEY` options.
pub type Options<'a> = &'a dyn Fn(&str, &str) -> Option<String>;

/// Whether `url` names an `oauth:` helper.
pub fn is_oauth(url: &str) -> bool {
    url.starts_with("oauth:")
}

/// Random bytes from the operating system, or a time/hash mix elsewhere.
fn random_bytes(count: usize) -> Vec<u8> {
    let mut bytes = vec![0; count];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .is_ok()
    {
        return bytes;
    }
    use std::hash::{BuildHasher, Hasher};
    for (index, chunk) in bytes.chunks_mut(8).enumerate() {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_usize(index);
        hasher.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
        );
        let value = hasher.finish().to_le_bytes();
        chunk.copy_from_slice(&value[..chunk.len()]);
    }
    bytes
}

/// Upstream `oauth.nonce`: random ASCII letters.
fn nonce(size: usize) -> String {
    const LETTERS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
    random_bytes(size)
        .into_iter()
        .map(|b| LETTERS[b as usize % LETTERS.len()] as char)
        .collect()
}

/// PKCE `code_challenge` (S256) for a verifier.
pub fn code_challenge(verifier: &str) -> String {
    use base64::Engine;
    use sha2::{Digest, Sha256};
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn query_map(query: &str) -> BTreeMap<String, String> {
    url::form_urlencoded::parse(query.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

struct Helper<'a> {
    category: String,
    options: Options<'a>,
    client: reqwest::Client,
    connection: Option<TcpStream>,
}

impl Helper<'_> {
    fn option(&self, key: &str) -> Option<String> {
        (self.options)(&self.category, key)
    }

    /// Open a local HTTP server and receive the callback's query parameters.
    fn recv(&mut self) -> Result<BTreeMap<String, String>> {
        println!("Waiting for response. (Cancel with Ctrl+c)");
        let host = (self.options)("oauth", "host").unwrap_or_else(|| "localhost".into());
        let port = (self.options)("oauth", "port")
            .and_then(|p| p.parse::<u16>().ok())
            .unwrap_or(6414);
        let listener = std::net::TcpListener::bind((host.as_str(), port))?;
        let (stream, _) = listener.accept()?;
        let mut line = String::new();
        std::io::BufReader::new(stream.try_clone()?).read_line(&mut line)?;
        let path = line
            .split(' ')
            .nth(1)
            .ok_or_else(|| error("Received invalid OAuth response"))?;
        let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
        stream.set_nonblocking(false)?;
        self.connection = Some(stream);
        Ok(query_map(query))
    }

    /// Print `message` and answer the pending callback with it.
    fn send(&mut self, message: &str) {
        print!("{message}");
        if let Some(mut stream) = self.connection.take() {
            let _ = stream.write_all(format!("HTTP/1.1 200 OK\r\n\r\n{message}").as_bytes());
        }
    }

    fn open_url(&self, url: &Url) {
        let browser = (self.options)("oauth", "browser").is_none_or(|b| b != "false");
        let opener = if cfg!(target_os = "macos") {
            "open"
        } else if cfg!(windows) {
            "explorer"
        } else {
            "xdg-open"
        };
        let opened = browser
            && std::process::Command::new(opener)
                .arg(url.as_str())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success() || cfg!(windows));
        if !opened {
            eprintln!("Please open this URL in your browser:");
        }
        println!("\n{url}\n");
    }

    fn message(&self, names: &[&str], values: &[String]) -> String {
        let (is, value, it) = if names.len() == 1 {
            ("is", "this value", "it")
        } else {
            ("are", "these values", "them")
        };
        let keys = names
            .iter()
            .map(|n| format!("'{n}'"))
            .collect::<Vec<_>>()
            .join(" and ");
        let options = names
            .iter()
            .map(|n| format!("'extractor.{}.{n}'", self.category))
            .collect::<Vec<_>>()
            .join(" and\n");
        format!(
            "\nYour {keys} {is}\n\n{}\n\nPut {value} into your configuration file as \n{options}\nto use {it}.\n",
            values.join("\n")
        )
    }

    async fn oauth1(
        &mut self,
        default: (&str, &str),
        request_token: &str,
        authorize: &str,
        access_token: &str,
        redirect: &str,
    ) -> Result<()> {
        let key = self.option("api-key").unwrap_or_else(|| default.0.into());
        let secret = self
            .option("api-secret")
            .unwrap_or_else(|| default.1.into());
        eprintln!(
            "Using {} {} API key ({key})",
            if key == default.0 {
                "default"
            } else {
                "custom"
            },
            self.category
        );
        let signed = |url: &Url, token: (&str, &str)| {
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            oauth1_header("GET", url, (&key, &secret), token, &nonce(16), timestamp)
        };
        let mut url = Url::parse(request_token)?;
        url.query_pairs_mut()
            .append_pair("oauth_callback", redirect);
        let response = self
            .client
            .get(url.clone())
            .header("Authorization", signed(&url, ("", "")))
            .send()
            .await?
            .text()
            .await?;
        let data = query_map(&response);
        let (token, token_secret) = match (data.get("oauth_token"), data.get("oauth_token_secret"))
        {
            (Some(t), Some(s)) => (t.clone(), s.clone()),
            _ => {
                return Err(error(format!(
                    "unexpected request token response: {response}"
                )));
            }
        };
        let mut url = Url::parse(authorize)?;
        url.query_pairs_mut()
            .append_pair("oauth_token", &token)
            .append_pair("perms", "read");
        self.open_url(&url);
        let params = self.recv()?;
        let mut url = Url::parse(access_token)?;
        for (k, v) in &params {
            url.query_pairs_mut().append_pair(k, v);
        }
        let response = self
            .client
            .get(url.clone())
            .header("Authorization", signed(&url, (&token, &token_secret)))
            .send()
            .await?
            .text()
            .await?;
        let data = query_map(&response);
        match (data.get("oauth_token"), data.get("oauth_token_secret")) {
            (Some(token), Some(secret)) => {
                let message = self.message(
                    &["access-token", "access-token-secret"],
                    &[token.clone(), secret.clone()],
                );
                self.send(&message);
                Ok(())
            }
            _ => {
                self.send(&format!("Remote server reported an error:\n\n{response}\n"));
                Ok(())
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn oauth2(
        &mut self,
        client: (&str, &str),
        auth_url: &str,
        token_url: &str,
        scope: &str,
        key: &str,
        basic_auth: bool,
        redirect: &str,
    ) -> Result<()> {
        let state = format!("gallery-dl_{}_{}", self.category, nonce(8));
        let mut url = Url::parse(auth_url)?;
        url.query_pairs_mut()
            .append_pair("client_id", client.0)
            .append_pair("response_type", "code")
            .append_pair("state", &state)
            .append_pair("redirect_uri", redirect)
            .append_pair("duration", "permanent")
            .append_pair("scope", scope);
        self.open_url(&url);
        let params = self.recv()?;
        if params.get("state") != Some(&state) {
            let got = params.get("state").cloned().unwrap_or_default();
            self.send(&format!("'state' mismatch: expected {state}, got {got}.\n"));
            return Ok(());
        }
        if params.contains_key("error") {
            self.send(&format!("Remote server reported an error:\n\n{params:?}\n"));
            return Ok(());
        }
        let code = params.get("code").cloned().unwrap_or_default();
        let mut form = vec![
            ("grant_type", "authorization_code".to_owned()),
            ("code", code),
            ("redirect_uri", redirect.to_owned()),
        ];
        let mut request = self.client.post(token_url);
        if basic_auth {
            request = request.basic_auth(client.0, Some(client.1));
        } else {
            form.push(("client_id", client.0.to_owned()));
            form.push(("client_secret", client.1.to_owned()));
        }
        let data = request.form(&form).send().await?.bytes().await?;
        let data: Value = serde_json::from_slice(&data)?;
        match data.get(key).and_then(Value::as_str) {
            Some(token) if data.get("error").is_none() => {
                let message = self.message(&[&key.replace('_', "-")], &[token.to_owned()]);
                self.send(&message);
            }
            _ => self.send(&format!("Remote server reported an error:\n\n{data}\n")),
        }
        Ok(())
    }

    async fn pixiv(&mut self) -> Result<()> {
        let verifier = random_bytes(32)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let mut url = Url::parse("https://app-api.pixiv.net/web/v1/login")?;
        url.query_pairs_mut()
            .append_pair("code_challenge", &code_challenge(&verifier))
            .append_pair("code_challenge_method", "S256")
            .append_pair("client", "pixiv-android");
        self.open_url(&url);
        println!(
            "1) Open your browser's Developer Tools (F12) and switch to the Network tab\n\
             2) Login\n\
             3) Select the last network monitor entry ('callback?state=...')\n\
             4) Copy its 'code' query parameter, paste it below, and press Enter\n\n\
             - This 'code' will expire 30 seconds after logging in.\n\
             - Copy-pasting more than just the 'code' value will work as well,\n  \
             like the entire URL or several query parameters.\n"
        );
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        let input = input.trim();
        let code = input
            .split_once("code=")
            .map_or(input, |(_, rest)| rest.split('&').next().unwrap_or(rest))
            .to_owned();
        let client_id = self
            .option("client-id")
            .unwrap_or_else(|| crate::pixiv::CLIENT_ID.into());
        let client_secret = self
            .option("client-secret")
            .unwrap_or_else(|| crate::pixiv::CLIENT_SECRET.into());
        let form = [
            ("client_id", client_id.as_str()),
            ("client_secret", client_secret.as_str()),
            ("code", code.as_str()),
            ("code_verifier", verifier.as_str()),
            ("grant_type", "authorization_code"),
            ("include_policy", "true"),
            (
                "redirect_uri",
                "https://app-api.pixiv.net/web/v1/users/auth/pixiv/callback",
            ),
        ];
        let data = self
            .client
            .post("https://oauth.secure.pixiv.net/auth/token")
            .header(
                "User-Agent",
                "PixivAndroidApp/5.0.234 (Android 11; Pixel 5)",
            )
            .form(&form)
            .send()
            .await?
            .bytes()
            .await?;
        let data: Value = serde_json::from_slice(&data)?;
        if let Some(kind) = data.get("error").and_then(Value::as_str) {
            println!("\n{data}");
            if matches!(kind, "invalid_request" | "invalid_grant") {
                println!("'code' expired, try again\n");
            }
            return Ok(());
        }
        let token = data
            .get("refresh_token")
            .and_then(Value::as_str)
            .ok_or_else(|| error("pixiv returned no refresh token"))?;
        print!("{}", self.message(&["refresh-token"], &[token.to_owned()]));
        Ok(())
    }

    async fn mastodon(&mut self, instance: &str) -> Result<()> {
        let known = [
            (
                "mastodon.social",
                "dBSHdpsnOUZgxOnjKSQrWEPakO3ctM7HmsyoOd4FcRo",
                "DdrODTHs_XoeOsNVXnILTMabtdpWrWOAtrmw91wU1zI",
            ),
            (
                "pawoo.net",
                "978a25f843ec01e53d09be2c290cd75c782bc3b7fdbd7ea4164b9f3c3780c8ff",
                "9208e3d4a7997032cf4f1b0e12e5df388428ef1fadb446dcfeb4f5ed6872d97b",
            ),
            (
                "baraag.net",
                "czxx2qilLElYHQ_sm-lO8yXuGwOHxLX9RYYaD0-nq1o",
                "haMaFdMBgK_-BIxufakmI2gFgkYjqmgXGEO2tB-R2xY",
            ),
        ];
        let (id, secret) = match known.iter().find(|(host, _, _)| *host == instance) {
            Some((_, id, secret)) => ((*id).to_owned(), (*secret).to_owned()),
            None => {
                eprintln!("Registering application for '{instance}'");
                let data = self
                    .client
                    .post(format!("https://{instance}/api/v1/apps"))
                    .form(&[
                        ("client_name", format!("gdl:{}", nonce(8))),
                        ("redirect_uris", REDIRECT_URI_LOCALHOST.to_owned()),
                        ("scopes", "read".to_owned()),
                    ])
                    .send()
                    .await?
                    .bytes()
                    .await?;
                let data: Value = serde_json::from_slice(&data)?;
                match (
                    data.get("client_id").and_then(Value::as_str),
                    data.get("client_secret").and_then(Value::as_str),
                ) {
                    (Some(id), Some(secret)) => {
                        eprintln!("client-id:\n{id}\nclient-secret:\n{secret}");
                        (id.to_owned(), secret.to_owned())
                    }
                    _ => {
                        return Err(error(format!(
                            "Failed to register new application: '{data}'"
                        )));
                    }
                }
            }
        };
        eprintln!("Using default {instance} client ID ({id})");
        self.category = "mastodon".into();
        self.oauth2(
            (&id, &secret),
            &format!("https://{instance}/oauth/authorize"),
            &format!("https://{instance}/oauth/token"),
            "read",
            "access_token",
            true,
            REDIRECT_URI_LOCALHOST,
        )
        .await
    }
}

/// Run the `oauth:` helper named by `target` (e.g. `oauth:pixiv`).
pub async fn run(target: &str, options: Options<'_>) -> Result<()> {
    let name = target.strip_prefix("oauth:").unwrap_or(target);
    let (category, instance) = match name.split_once(':') {
        Some(("mastodon", instance)) => {
            let instance = instance
                .trim_start_matches("https://")
                .trim_start_matches("http://");
            (
                "mastodon",
                Some(instance.split('/').next().unwrap_or(instance).to_owned()),
            )
        }
        _ => (name, None),
    };
    let mut helper = Helper {
        category: category.to_owned(),
        options,
        client: reqwest::Client::builder()
            .user_agent(concat!("gallery-dl-rs/", env!("CARGO_PKG_VERSION")))
            .build()?,
        connection: None,
    };
    let custom = |helper: &Helper<'_>, key: &str, default: &str| {
        helper.option(key).unwrap_or_else(|| default.to_owned())
    };
    match (category, instance) {
        ("pixiv", None) => helper.pixiv().await,
        ("deviantart", None) => {
            let id = custom(&helper, "client-id", crate::deviantart::CLIENT_ID);
            let secret = custom(&helper, "client-secret", crate::deviantart::CLIENT_SECRET);
            helper
                .oauth2(
                    (&id, &secret),
                    "https://www.deviantart.com/oauth2/authorize",
                    "https://www.deviantart.com/oauth2/token",
                    "browse user.manage",
                    "refresh_token",
                    true,
                    REDIRECT_URI_HTTPS,
                )
                .await
        }
        ("reddit", None) => {
            let id = custom(&helper, "client-id", "6N9uN0krSDE-ig");
            helper.client = reqwest::Client::builder()
                .user_agent("Python:gallery-dl:0.8.4 (by /u/mikf1)")
                .build()?;
            helper
                .oauth2(
                    (&id, ""),
                    "https://www.reddit.com/api/v1/authorize",
                    "https://www.reddit.com/api/v1/access_token",
                    "read history",
                    "refresh_token",
                    true,
                    REDIRECT_URI_LOCALHOST,
                )
                .await
        }
        ("mastodon", Some(instance)) => helper.mastodon(&instance).await,
        ("tumblr", None) => {
            helper
                .oauth1(
                    (
                        crate::tumblr::DEFAULT_API_KEY,
                        "sFdsK3PDdP2QpYMRAoq0oDnw0sFS24XigXmdfnaeNZpJpqAn03",
                    ),
                    "https://www.tumblr.com/oauth/request_token",
                    "https://www.tumblr.com/oauth/authorize",
                    "https://www.tumblr.com/oauth/access_token",
                    REDIRECT_URI_LOCALHOST,
                )
                .await
        }
        ("smugmug", None) => {
            helper
                .oauth1(
                    crate::sites::art::smugmug::API_CREDENTIALS,
                    "https://api.smugmug.com/services/oauth/1.0a/getRequestToken",
                    "https://api.smugmug.com/services/oauth/1.0a/authorize",
                    "https://api.smugmug.com/services/oauth/1.0a/getAccessToken",
                    REDIRECT_URI_LOCALHOST,
                )
                .await
        }
        ("flickr", None) => {
            helper
                .oauth1(
                    ("", ""),
                    "https://www.flickr.com/services/oauth/request_token",
                    "https://www.flickr.com/services/oauth/authorize",
                    "https://www.flickr.com/services/oauth/access_token",
                    REDIRECT_URI_HTTPS,
                )
                .await
        }
        _ => Err(error(format!(
            "unsupported OAuth helper '{target}'; supported: oauth:pixiv, oauth:deviantart, \
             oauth:reddit, oauth:tumblr, oauth:smugmug, oauth:flickr, oauth:mastodon:INSTANCE"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_matches_rfc_7636_example() {
        assert_eq!(
            code_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert_eq!(nonce(8).len(), 8);
        assert_ne!(random_bytes(16), random_bytes(16));
    }

    #[tokio::test]
    async fn recv_returns_callback_parameters_and_send_answers() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let options = move |category: &str, key: &str| match (category, key) {
            ("oauth", "port") => Some(port.to_string()),
            ("oauth", "host") => Some("127.0.0.1".to_owned()),
            _ => None,
        };
        let mut helper = Helper {
            category: "reddit".into(),
            options: &options,
            client: reqwest::Client::new(),
            connection: None,
        };
        let client = std::thread::spawn(move || {
            let mut stream = loop {
                if let Ok(stream) = TcpStream::connect(("127.0.0.1", port)) {
                    break stream;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            };
            stream
                .write_all(b"GET /?state=s&code=abc%20d HTTP/1.1\r\nHost: x\r\n\r\n")
                .unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).unwrap();
            response
        });
        let params = helper.recv().unwrap();
        assert_eq!(params["code"], "abc d");
        let message = helper.message(&["refresh-token"], &["TOKEN".into()]);
        helper.send(&message);
        let response = client.join().unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("'extractor.reddit.refresh-token'"));
    }
}
