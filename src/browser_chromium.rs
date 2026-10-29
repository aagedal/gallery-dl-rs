//! Read-only Chromium cookie imports (Chrome, Brave, Helium, ...).
//!
//! The cookie database is copied into a private temporary directory so a
//! running browser's locks and journals never affect the import. Encrypted
//! values (`v10`) are decrypted with the browser's macOS Keychain secret.

use std::{
    fmt,
    path::{Path, PathBuf},
    process::Command,
};

use rusqlite::{Connection, OpenFlags, params};
use sha2::{Digest, Sha256};

use crate::{Result, cookies::CookieJar, error};

const MAX_COOKIES: usize = 4096;
const MAX_VALUE_BYTES: usize = 4096;
const MAX_ROW_BYTES: usize = 16 * 1024;
/// Seconds between 1601-01-01 (Chromium's epoch) and 1970-01-01.
const WINDOWS_EPOCH_OFFSET: i64 = 11_644_473_600;

/// A supported Chromium-based browser.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Browser {
    name: &'static str,
    /// User-data directory below `~/Library/Application Support`.
    directory: &'static str,
    /// macOS Keychain service and account holding the cookie password.
    service: &'static str,
    account: &'static str,
}

const BROWSERS: &[Browser] = &[
    Browser {
        name: "helium",
        directory: "net.imput.helium",
        service: "Helium Storage Key",
        account: "Helium",
    },
    Browser {
        name: "helium-neo",
        directory: "no.aagedal.helium-neo",
        service: "Helium Neo Storage Key",
        account: "Helium Neo",
    },
    Browser {
        name: "chrome",
        directory: "Google/Chrome",
        service: "Chrome Safe Storage",
        account: "Chrome",
    },
    Browser {
        name: "chromium",
        directory: "Chromium",
        service: "Chromium Safe Storage",
        account: "Chromium",
    },
    Browser {
        name: "brave",
        directory: "BraveSoftware/Brave-Browser",
        service: "Brave Safe Storage",
        account: "Brave",
    },
    Browser {
        name: "edge",
        directory: "Microsoft Edge",
        service: "Microsoft Edge Safe Storage",
        account: "Microsoft Edge",
    },
    Browser {
        name: "vivaldi",
        directory: "Vivaldi",
        service: "Vivaldi Safe Storage",
        account: "Vivaldi",
    },
];

/// Whether `name` is a supported Chromium browser.
pub(crate) fn is_browser(name: &str) -> bool {
    BROWSERS.iter().any(|browser| browser.name == name)
}

/// Where the cookie password comes from; tests supply it directly.
#[derive(Clone)]
enum Password {
    Keychain,
    #[cfg(test)]
    Fixed(Vec<u8>),
}

#[derive(Clone)]
pub struct ChromiumProfile {
    browser: Browser,
    /// A profile name below the user-data directory, or a path.
    profile: Option<String>,
    password: Password,
}

impl fmt::Debug for ChromiumProfile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "ChromiumProfile {{ browser: {}, profile: [redacted] }}",
            self.browser.name
        )
    }
}

impl ChromiumProfile {
    /// Accept `BROWSER[:PROFILE]`, where PROFILE is a profile name such as
    /// `Profile 1` or a path to a profile directory or `Cookies` database.
    /// Keyrings and containers are rejected.
    pub fn parse(spec: &str) -> Result<Self> {
        let invalid = || error("invalid Chromium browser cookie specification");
        if spec.chars().any(char::is_control) || spec.contains("::") {
            return Err(invalid());
        }
        let (name, profile) = match spec.split_once(':') {
            Some((name, profile)) => (name, Some(profile)),
            None => (spec, None),
        };
        if name.contains('+') {
            return Err(error("browser cookie keyrings are not supported"));
        }
        let browser = *BROWSERS
            .iter()
            .find(|browser| browser.name == name.to_ascii_lowercase())
            .ok_or_else(invalid)?;
        let profile = match profile {
            None => None,
            Some(profile) if profile.is_empty() || profile.starts_with('~') => {
                return Err(invalid());
            }
            Some(profile) => Some(profile.to_owned()),
        };
        Ok(Self {
            browser,
            profile,
            password: Password::Keychain,
        })
    }

    pub fn name(&self) -> &'static str {
        self.browser.name
    }

    /// Import the site's unpartitioned cookies. Credentials stay in memory.
    pub async fn load(&self, site: &str) -> Result<CookieJar> {
        let profile = self.clone();
        let site = site.to_ascii_lowercase();
        tokio::task::spawn_blocking(move || profile.load_blocking(&site))
            .await
            .map_err(|_| error("browser cookie import task failed"))?
    }

    fn profile_directory(&self) -> Result<PathBuf> {
        match self.profile.as_deref() {
            Some(profile) if profile.contains(std::path::MAIN_SEPARATOR) => Ok(profile.into()),
            profile => {
                let home = std::env::var_os("HOME")
                    .filter(|home| !home.is_empty())
                    .ok_or_else(|| error("browser cookie import requires HOME"))?;
                let root = Path::new(&home)
                    .join("Library/Application Support")
                    .join(self.browser.directory);
                let name = profile.unwrap_or("Default");
                Ok(match root.join(name).is_dir() {
                    true => root.join(name),
                    // A display name such as "Personal" selects its directory.
                    false => display_name_directory(&root, name).unwrap_or(root.join(name)),
                })
            }
        }
    }

    fn load_blocking(&self, site: &str) -> Result<CookieJar> {
        let name = self.browser.name;
        let invalid = || {
            error(format!(
                "could not read {name} cookies; check the profile and cookie database"
            ))
        };
        let profile = self.profile_directory()?;
        let database = [profile.join("Cookies"), profile.join("Network/Cookies")]
            .into_iter()
            .find(|path| path.is_file())
            .or_else(|| profile.is_file().then(|| profile.clone()))
            .ok_or_else(invalid)?;
        // Copy the database with its journal, so SQLite recovers a
        // consistent snapshot without touching the browser's files.
        let scratch = tempfile::tempdir().map_err(|_| invalid())?;
        let copy = scratch.path().join("Cookies");
        std::fs::copy(&database, &copy).map_err(|_| invalid())?;
        for suffix in ["-journal", "-wal"] {
            let mut side = database.clone().into_os_string();
            side.push(suffix);
            let side = PathBuf::from(side);
            if side.is_file() {
                std::fs::copy(&side, scratch.path().join(format!("Cookies{suffix}")))
                    .map_err(|_| invalid())?;
            }
        }
        let connection = Connection::open_with_flags(
            &copy,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|_| invalid())?;
        connection
            .execute_batch("PRAGMA trusted_schema=OFF;")
            .map_err(|_| invalid())?;
        let version = connection
            .query_row("SELECT value FROM meta WHERE key = 'version'", [], |row| {
                row.get::<_, rusqlite::types::Value>(0)
            })
            .ok()
            .and_then(|value| match value {
                rusqlite::types::Value::Integer(value) => Some(value),
                rusqlite::types::Value::Text(value) => value.parse().ok(),
                _ => None,
            })
            .unwrap_or(0);
        let partitioned = connection
            .prepare("SELECT 1 FROM pragma_table_info('cookies') WHERE name = 'top_frame_site_key'")
            .and_then(|mut statement| statement.exists([]))
            .map_err(|_| invalid())?;
        let query = format!(
            "SELECT host_key, name, value, encrypted_value, path, expires_utc, is_secure
             FROM cookies
             WHERE (lower(host_key) = ?1 OR lower(host_key) = ?2 OR lower(host_key) LIKE ?3)
               {}
             LIMIT {}",
            if partitioned {
                "AND top_frame_site_key = ''"
            } else {
                ""
            },
            MAX_COOKIES + 1
        );
        let mut statement = connection.prepare(&query).map_err(|_| invalid())?;
        let mut rows = statement
            .query(params![site, format!(".{site}"), format!("%.{site}")])
            .map_err(|_| invalid())?;
        let mut key = None;
        let mut export = String::new();
        let mut count = 0;
        while let Some(row) = rows.next().map_err(|_| invalid())? {
            count += 1;
            if count > MAX_COOKIES {
                return Err(error(format!(
                    "{name} cookie import exceeds the cookie count limit"
                )));
            }
            let host: String = row.get(0).map_err(|_| invalid())?;
            let cookie_name: String = row.get(1).map_err(|_| invalid())?;
            let plain: String = row.get(2).map_err(|_| invalid())?;
            let encrypted: Vec<u8> = row.get(3).map_err(|_| invalid())?;
            let path: String = row.get(4).map_err(|_| invalid())?;
            let expires: i64 = row.get(5).map_err(|_| invalid())?;
            let secure: i64 = row.get(6).map_err(|_| invalid())?;
            if host.len() + cookie_name.len() + path.len() + plain.len() + encrypted.len()
                > MAX_ROW_BYTES
            {
                return Err(error(format!(
                    "{name} cookie import exceeds the cookie size limit"
                )));
            }
            // Recheck the domain in Rust as well as SQL before admitting credentials.
            let domain = host.strip_prefix('.').unwrap_or(&host).to_ascii_lowercase();
            if domain != site && !domain.ends_with(&format!(".{site}")) {
                return Err(invalid());
            }
            let value = if encrypted.is_empty() {
                plain
            } else {
                if key.is_none() {
                    key = Some(self.key()?);
                }
                decrypt(key.as_ref().unwrap(), &encrypted, &host, version).ok_or_else(|| {
                    error(format!(
                        "could not decrypt {name} cookies; the Keychain secret did not match"
                    ))
                })?
            };
            if value.len() > MAX_VALUE_BYTES {
                return Err(error(format!(
                    "{name} cookie import exceeds the cookie size limit"
                )));
            }
            let subdomains = if host.starts_with('.') {
                "TRUE"
            } else {
                "FALSE"
            };
            let secure = if secure != 0 { "TRUE" } else { "FALSE" };
            let expiry = if expires <= 0 {
                0
            } else {
                (expires / 1_000_000 - WINDOWS_EPOCH_OFFSET).max(1)
            };
            let line = format!(
                "{host}\t{subdomains}\t{path}\t{secure}\t{expiry}\t{cookie_name}\t{value}\n"
            );
            // Browsers keep some values a Netscape file cannot express
            // (control characters, invalid names); skip those cookies.
            if CookieJar::parse(&line).is_ok() {
                export.push_str(&line);
            }
        }
        CookieJar::parse(&export).map_err(|_| invalid())
    }

    /// The AES-128 key derived from the browser's cookie password.
    fn key(&self) -> Result<[u8; 16]> {
        let password = match &self.password {
            Password::Keychain => keychain_password(&self.browser)?,
            #[cfg(test)]
            Password::Fixed(password) => password.clone(),
        };
        let mut key = [0u8; 16];
        pbkdf2::pbkdf2_hmac::<sha1::Sha1>(&password, b"saltysalt", 1003, &mut key);
        Ok(key)
    }
}

/// The profile directory whose display name (`Local State`
/// `profile.info_cache.*.name`) is `name`.
fn display_name_directory(root: &Path, name: &str) -> Option<PathBuf> {
    let file = std::fs::File::open(root.join("Local State")).ok()?;
    if file.metadata().ok()?.len() > 16 * 1024 * 1024 {
        return None;
    }
    let state: serde_json::Value = serde_json::from_reader(std::io::BufReader::new(file)).ok()?;
    let directory = state
        .pointer("/profile/info_cache")?
        .as_object()?
        .iter()
        .find(|(_, info)| info.get("name").and_then(serde_json::Value::as_str) == Some(name))?
        .0;
    // Directory names are single components such as "Profile 1".
    (!directory.contains(['/', '\\']) && directory != ".." && directory != ".")
        .then(|| root.join(directory))
}

#[cfg(target_os = "macos")]
fn keychain_password(browser: &Browser) -> Result<Vec<u8>> {
    let failure = || {
        error(format!(
            "could not read the {} cookie password from the macOS Keychain",
            browser.name
        ))
    };
    let output = Command::new("/usr/bin/security")
        .args([
            "find-generic-password",
            "-w",
            "-s",
            browser.service,
            "-a",
            browser.account,
        ])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .map_err(|_| failure())?;
    if !output.status.success() {
        return Err(failure());
    }
    let mut password = output.stdout;
    while password
        .last()
        .is_some_and(|byte| *byte == b'\n' || *byte == b'\r')
    {
        password.pop();
    }
    if password.is_empty() {
        return Err(failure());
    }
    Ok(password)
}

#[cfg(not(target_os = "macos"))]
fn keychain_password(browser: &Browser) -> Result<Vec<u8>> {
    let _ = Command::new;
    Err(error(format!(
        "decrypting {} cookies is only supported on macOS",
        browser.name
    )))
}

/// Decrypt a `v10` value: AES-128-CBC with a space IV. Database version 24
/// and later prefix the plaintext with SHA-256 of the cookie's host.
fn decrypt(key: &[u8; 16], encrypted: &[u8], host: &str, version: i64) -> Option<String> {
    use aes::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
    let ciphertext = encrypted.strip_prefix(b"v10")?;
    let mut buffer = ciphertext.to_vec();
    let plain = cbc::Decryptor::<aes::Aes128>::new(key.into(), &[b' '; 16].into())
        .decrypt_padded_mut::<Pkcs7>(&mut buffer)
        .ok()?;
    let plain = if version >= 24 {
        let (digest, rest) = plain.split_at_checked(32)?;
        if digest != Sha256::digest(host.as_bytes()).as_slice() {
            return None;
        }
        rest
    } else {
        plain
    };
    String::from_utf8(plain.to_vec()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use url::Url;

    const PASSWORD: &[u8] = b"synthetic-keychain-secret";

    fn encrypt(host: &str, value: &str, version: i64) -> Vec<u8> {
        use aes::cipher::{BlockEncryptMut, KeyIvInit, block_padding::Pkcs7};
        let mut key = [0u8; 16];
        pbkdf2::pbkdf2_hmac::<sha1::Sha1>(PASSWORD, b"saltysalt", 1003, &mut key);
        let mut plain = Vec::new();
        if version >= 24 {
            plain.extend_from_slice(&Sha256::digest(host.as_bytes()));
        }
        plain.extend_from_slice(value.as_bytes());
        let length = plain.len();
        plain.resize(length + 16, 0);
        let ciphertext = cbc::Encryptor::<aes::Aes128>::new(&key.into(), &[b' '; 16].into())
            .encrypt_padded_mut::<Pkcs7>(&mut plain, length)
            .unwrap();
        [b"v10".as_slice(), ciphertext].concat()
    }

    fn database(directory: &Path, version: i64) -> Connection {
        let connection = Connection::open(directory.join("Cookies")).unwrap();
        connection
            .execute_batch(&format!(
                "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
                 INSERT INTO meta VALUES ('version', '{version}');
                 CREATE TABLE cookies (
                     host_key TEXT, top_frame_site_key TEXT, name TEXT, value TEXT,
                     encrypted_value BLOB, path TEXT, expires_utc INTEGER, is_secure INTEGER
                 );"
            ))
            .unwrap();
        connection
    }

    fn insert(connection: &Connection, host: &str, name: &str, value: &str, version: i64) {
        connection
            .execute(
                "INSERT INTO cookies VALUES (?1, '', ?2, '', ?3, '/', 14000000000000000, 1)",
                params![host, name, encrypt(host, value, version)],
            )
            .unwrap();
    }

    fn profile(directory: &Path, password: &[u8]) -> ChromiumProfile {
        let mut profile =
            ChromiumProfile::parse(&format!("helium:{}", directory.display())).unwrap();
        profile.password = Password::Fixed(password.to_vec());
        profile
    }

    fn header(jar: &CookieJar, address: &str) -> Option<String> {
        jar.header(&Url::parse(address).unwrap())
    }

    #[test]
    fn parses_browser_names_profiles_and_redacts_debug() {
        for spec in [
            "helium",
            "helium-neo",
            "chrome:Profile 1",
            "Brave:/x/Default",
        ] {
            ChromiumProfile::parse(spec).unwrap();
        }
        for spec in [
            "firefox:/secret",
            "helium:",
            "helium:~/secret",
            "helium+keyring:/secret",
            "helium:/secret::container",
            "helium:/secret\n",
        ] {
            let failure = ChromiumProfile::parse(spec).unwrap_err().to_string();
            assert!(!failure.contains("secret"), "{failure}");
        }
        let profile = ChromiumProfile::parse("helium-neo:/private/profile").unwrap();
        assert_eq!(
            format!("{profile:?}"),
            "ChromiumProfile { browser: helium-neo, profile: [redacted] }"
        );
        assert_eq!(
            ChromiumProfile::parse("helium:Profile 1")
                .unwrap()
                .profile_directory()
                .unwrap()
                .file_name()
                .unwrap(),
            "Profile 1"
        );
    }

    #[test]
    fn display_names_select_profile_directories() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("Local State"),
            r#"{"profile":{"info_cache":{"Default":{"name":"Privat"},"Profile 1":{"name":"Work"},"../x":{"name":"Evil"}}}}"#,
        )
        .unwrap();
        assert_eq!(
            display_name_directory(root.path(), "Work"),
            Some(root.path().join("Profile 1"))
        );
        assert_eq!(
            display_name_directory(root.path(), "Privat"),
            Some(root.path().join("Default"))
        );
        assert_eq!(display_name_directory(root.path(), "Evil"), None);
        assert_eq!(display_name_directory(root.path(), "Missing"), None);
    }

    #[tokio::test]
    async fn decrypts_and_scopes_site_cookies() {
        for version in [23, 24] {
            let directory = tempfile::tempdir().unwrap();
            let connection = database(directory.path(), version);
            insert(&connection, ".reddit.com", "reddit_session", "abc", version);
            insert(&connection, "www.reddit.com", "host", "yes", version);
            insert(&connection, ".notreddit.com", "other", "no", version);
            connection
                .execute(
                    "INSERT INTO cookies VALUES ('.reddit.com', 'https://x.test', 'partitioned',
                     'p', x'', '/', 0, 0),
                     ('.reddit.com', '', 'plain', 'value', x'', '/', 0, 0)",
                    [],
                )
                .unwrap();
            let jar = profile(directory.path(), PASSWORD)
                .load("reddit.com")
                .await
                .unwrap();
            assert_eq!(
                header(&jar, "https://www.reddit.com/"),
                Some("reddit_session=abc; host=yes; plain=value".into())
            );
            assert_eq!(
                header(&jar, "https://old.reddit.com/"),
                Some("reddit_session=abc; plain=value".into())
            );
            assert_eq!(header(&jar, "https://notreddit.com/"), None);
        }
    }

    #[tokio::test]
    async fn wrong_password_fails_without_leaking_values() {
        let directory = tempfile::tempdir().unwrap();
        let connection = database(directory.path(), 24);
        insert(
            &connection,
            ".reddit.com",
            "reddit_session",
            "private-value",
            24,
        );
        let failure = profile(directory.path(), b"wrong")
            .load("reddit.com")
            .await
            .unwrap_err()
            .to_string();
        assert!(
            failure.contains("could not decrypt helium cookies"),
            "{failure}"
        );
        assert!(!failure.contains("private-value"));
    }

    #[tokio::test]
    async fn reads_network_cookies_and_leaves_the_source_untouched() {
        let directory = tempfile::tempdir().unwrap();
        let network = directory.path().join("Network");
        std::fs::create_dir(&network).unwrap();
        let connection = database(&network, 24);
        insert(&connection, ".tumblr.com", "pfg", "1", 24);
        drop(connection);
        let before = std::fs::read(network.join("Cookies")).unwrap();
        let jar = profile(directory.path(), PASSWORD)
            .load("tumblr.com")
            .await
            .unwrap();
        assert_eq!(
            header(&jar, "https://www.tumblr.com/"),
            Some("pfg=1".into())
        );
        assert_eq!(std::fs::read(network.join("Cookies")).unwrap(), before);
        assert_eq!(std::fs::read_dir(&network).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn missing_profile_fails_privately() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("private-missing");
        let failure = profile(&missing, PASSWORD)
            .load("reddit.com")
            .await
            .unwrap_err()
            .to_string();
        assert!(!failure.contains("private-missing"), "{failure}");
    }
}
