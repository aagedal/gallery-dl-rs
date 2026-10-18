//! Explicit, read-only Firefox cookie imports. Browser discovery is intentionally absent.

use std::{fmt, path::PathBuf, time::Duration};

use rusqlite::{Connection, OpenFlags, params};

use crate::{Result, cookies::CookieJar, error};

const MAX_COOKIES: usize = 4096;
const MAX_VALUE_BYTES: usize = 4096;
const MAX_ROW_BYTES: usize = 16 * 1024;
const MAX_EXPORT_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone)]
pub struct FirefoxProfile {
    path: PathBuf,
}

impl fmt::Debug for FirefoxProfile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("FirefoxProfile { profile: [redacted] }")
    }
}

impl FirefoxProfile {
    /// Accept only `firefox:PROFILE_PATH`, without profile discovery or containers.
    pub fn parse(spec: &str) -> Result<Self> {
        let Some(path) = spec.strip_prefix("firefox:") else {
            return Err(error("browser cookies require firefox:PROFILE_PATH"));
        };
        if path.is_empty()
            || path.chars().any(char::is_control)
            || path.contains("::")
            || path.starts_with('~')
        {
            return Err(error("invalid explicit Firefox profile path"));
        }
        Ok(Self { path: path.into() })
    }

    /// Import only the requested site's default, unpartitioned cookie context.
    /// Credentials remain in memory; the source database is never modified.
    pub async fn load(&self, site: &str) -> Result<CookieJar> {
        if !matches!(site, "instagram.com" | "facebook.com" | "x.com") {
            return Err(error("unsupported browser cookie site"));
        }
        let path = self.path.clone();
        let site = site.to_owned();
        tokio::task::spawn_blocking(move || load(path, &site))
            .await
            .map_err(|_| error("Firefox cookie import task failed"))?
    }
}

fn load(path: PathBuf, site: &str) -> Result<CookieJar> {
    let invalid = || error("could not read Firefox cookies; check the profile and cookie database");
    let metadata = std::fs::metadata(&path).map_err(|_| invalid())?;
    let database = if metadata.is_dir() {
        path.join("cookies.sqlite")
    } else if metadata.is_file() {
        path
    } else {
        return Err(invalid());
    };
    let mut connection = Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| invalid())?;
    connection
        .busy_timeout(Duration::from_secs(2))
        .map_err(|_| invalid())?;
    connection
        .execute_batch("PRAGMA query_only=ON; PRAGMA trusted_schema=OFF;")
        .map_err(|_| invalid())?;
    // A transaction sees committed WAL records without copying a live database.
    let transaction = connection.transaction().map_err(|_| invalid())?;
    let mut statement = transaction
        .prepare(
            "SELECT length(CAST(name AS BLOB)), length(CAST(value AS BLOB)),
                    length(CAST(host AS BLOB)), length(CAST(path AS BLOB)),
                    name, value, host, path, isSecure, expiry
             FROM moz_cookies
             WHERE originAttributes = '' AND typeof(originAttributes) = 'text'
               AND (lower(host) = ?1 OR lower(host) = ?2 OR lower(host) LIKE ?3)
             LIMIT 4097",
        )
        .map_err(|_| invalid())?;
    let suffix = format!("%.{site}");
    let dotted = format!(".{site}");
    let mut rows = statement
        .query(params![site, dotted, suffix])
        .map_err(|_| invalid())?;
    let mut export = String::new();
    let mut count = 0;
    while let Some(row) = rows.next().map_err(|_| invalid())? {
        count += 1;
        if count > MAX_COOKIES {
            return Err(error(
                "Firefox cookie import exceeds the cookie count limit",
            ));
        }
        let mut total = 0usize;
        for column in 0..4 {
            let length = row.get::<_, i64>(column).map_err(|_| invalid())?;
            let length = usize::try_from(length).map_err(|_| invalid())?;
            if length > MAX_ROW_BYTES || (column == 1 && length > MAX_VALUE_BYTES) {
                return Err(error("Firefox cookie import exceeds the cookie size limit"));
            }
            total = total.checked_add(length).ok_or_else(invalid)?;
        }
        if total > MAX_ROW_BYTES {
            return Err(error("Firefox cookie import exceeds the cookie size limit"));
        }
        let name = row.get::<_, String>(4).map_err(|_| invalid())?;
        let value = row.get::<_, String>(5).map_err(|_| invalid())?;
        let host = row.get::<_, String>(6).map_err(|_| invalid())?;
        let path = row.get::<_, String>(7).map_err(|_| invalid())?;
        if [&name, &value, &host, &path]
            .iter()
            .any(|field| field.chars().any(char::is_control))
        {
            return Err(invalid());
        }
        let secure = match row.get::<_, i64>(8).map_err(|_| invalid())? {
            0 => "FALSE",
            1 => "TRUE",
            _ => return Err(invalid()),
        };
        let expiry = row.get::<_, i64>(9).map_err(|_| invalid())?;
        if expiry < 0 {
            return Err(invalid());
        }
        // Recheck the domain in Rust as well as SQL before admitting credentials.
        let domain = host.strip_prefix('.').unwrap_or(&host).to_ascii_lowercase();
        if domain != site && !domain.ends_with(&format!(".{site}")) {
            return Err(invalid());
        }
        let subdomains = if host.starts_with('.') {
            "TRUE"
        } else {
            "FALSE"
        };
        let line = format!("{host}\t{subdomains}\t{path}\t{secure}\t{expiry}\t{name}\t{value}\n");
        if export.len().saturating_add(line.len()) > MAX_EXPORT_BYTES {
            return Err(error("Firefox cookie import exceeds the total size limit"));
        }
        export.push_str(&line);
    }
    CookieJar::parse(&export).map_err(|_| invalid())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use url::Url;

    fn database() -> (TempDir, Connection) {
        let directory = tempfile::tempdir().unwrap();
        let connection = Connection::open(directory.path().join("cookies.sqlite")).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE moz_cookies (
                name TEXT, value TEXT, host TEXT, path TEXT, isSecure INTEGER,
                expiry INTEGER, originAttributes TEXT
             );",
            )
            .unwrap();
        (directory, connection)
    }

    fn insert(connection: &Connection, host: &str, name: &str, value: &str, context: &str) {
        connection
            .execute(
                "INSERT INTO moz_cookies VALUES (?1, ?2, ?3, '/', 0, 0, ?4)",
                params![name, value, host, context],
            )
            .unwrap();
    }

    fn profile(directory: &TempDir) -> FirefoxProfile {
        FirefoxProfile::parse(&format!("firefox:{}", directory.path().display())).unwrap()
    }

    fn header(jar: &CookieJar, address: &str) -> Option<String> {
        jar.header(&Url::parse(address).unwrap())
    }

    #[test]
    fn requires_explicit_supported_profile_and_redacts_debug() {
        for spec in [
            "firefox",
            "firefox:",
            "chrome:/secret",
            "firefox+keyring:/secret",
            "firefox:~/secret",
            "firefox:/secret::container",
            "firefox:/secret\n",
        ] {
            let failure = FirefoxProfile::parse(spec).unwrap_err().to_string();
            assert!(!failure.contains("secret"));
        }
        let profile = FirefoxProfile::parse("firefox:private/profile").unwrap();
        assert_eq!(
            format!("{profile:?}"),
            "FirefoxProfile { profile: [redacted] }"
        );
        assert_eq!(format!("{:?}", profile.clone()), format!("{profile:?}"));
    }

    #[tokio::test]
    async fn scopes_site_domain_and_default_context() {
        let (directory, connection) = database();
        insert(&connection, ".instagram.com", "root", "yes", "");
        insert(&connection, "www.instagram.com", "host", "yes", "");
        insert(
            &connection,
            ".instagram.com",
            "container",
            "secret",
            "^userContextId=1",
        );
        insert(
            &connection,
            ".instagram.com",
            "partitioned",
            "secret",
            "^partitionKey=%28https%2Cexample.com%29",
        );
        insert(&connection, ".facebook.com", "facebook", "secret", "");
        insert(&connection, ".x.com", "x", "secret", "");
        insert(&connection, ".evilinstagram.com", "suffix", "secret", "");
        insert(
            &connection,
            ".instagram.com.evil.test",
            "prefix",
            "secret",
            "",
        );
        let jar = profile(&directory).load("instagram.com").await.unwrap();
        assert_eq!(
            header(&jar, "https://www.instagram.com/"),
            Some("root=yes; host=yes".into())
        );
        assert_eq!(
            header(&jar, "https://instagram.com/"),
            Some("root=yes".into())
        );
        assert_eq!(
            header(&jar, "https://sub.www.instagram.com/"),
            Some("root=yes".into())
        );
        assert_eq!(header(&jar, "https://facebook.com/"), None);
        assert_eq!(header(&jar, "https://x.com/"), None);
        let jar = profile(&directory).load("x.com").await.unwrap();
        assert_eq!(header(&jar, "https://x.com/"), Some("x=secret".into()));
    }

    #[tokio::test]
    async fn preserves_path_secure_expiry_and_explicit_database_file() {
        let (directory, connection) = database();
        connection
            .execute_batch(
                "INSERT INTO moz_cookies VALUES ('secure','yes','.instagram.com','/api',1,0,'');
             INSERT INTO moz_cookies VALUES ('expired','no','.instagram.com','/',0,1,'');
             INSERT INTO moz_cookies VALUES ('future','yes','.instagram.com','/',0,4102444800,'');",
            )
            .unwrap();
        let profile = FirefoxProfile::parse(&format!(
            "firefox:{}",
            directory.path().join("cookies.sqlite").display()
        ))
        .unwrap();
        let jar = profile.load("instagram.com").await.unwrap();
        assert_eq!(
            header(&jar, "https://instagram.com/api/v1/"),
            Some("secure=yes; future=yes".into())
        );
        assert_eq!(
            header(&jar, "http://instagram.com/api/"),
            Some("future=yes".into())
        );
        assert_eq!(
            header(&jar, "https://instagram.com/apix/"),
            Some("future=yes".into())
        );
    }

    #[tokio::test]
    async fn reads_committed_wal_without_writing_source() {
        let (directory, connection) = database();
        connection
            .execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
            .unwrap();
        insert(&connection, ".instagram.com", "committed", "yes", "");
        connection.execute_batch("BEGIN IMMEDIATE;").unwrap();
        insert(&connection, ".instagram.com", "uncommitted", "no", "");
        let files = ["cookies.sqlite", "cookies.sqlite-wal"];
        let before: Vec<_> = files
            .iter()
            .map(|name| std::fs::read(directory.path().join(name)).unwrap())
            .collect();
        let jar = profile(&directory).load("instagram.com").await.unwrap();
        assert_eq!(
            header(&jar, "https://instagram.com/"),
            Some("committed=yes".into())
        );
        for (name, before) in files.iter().zip(before) {
            assert_eq!(std::fs::read(directory.path().join(name)).unwrap(), before);
        }
        connection.execute_batch("ROLLBACK;").unwrap();
    }

    #[tokio::test]
    async fn fails_privately_for_missing_or_malformed_database_and_records() {
        let (directory, connection) = database();
        let missing = FirefoxProfile::parse(&format!(
            "firefox:{}/private-missing",
            directory.path().display()
        ))
        .unwrap();
        let failure = missing.load("instagram.com").await.unwrap_err().to_string();
        assert!(!failure.contains("private-missing"));
        assert!(!failure.contains(&directory.path().display().to_string()));
        insert(
            &connection,
            ".instagram.com",
            "secret-name",
            "secret-value\ninvalid",
            "",
        );
        let failure = profile(&directory)
            .load("instagram.com")
            .await
            .unwrap_err()
            .to_string();
        assert!(!failure.contains("secret"));
        connection
            .execute_batch("DROP TABLE moz_cookies; CREATE TABLE moz_cookies (name TEXT);")
            .unwrap();
        let failure = profile(&directory)
            .load("instagram.com")
            .await
            .unwrap_err()
            .to_string();
        assert!(!failure.contains("moz_cookies"));
        drop(connection);
        std::fs::write(
            directory.path().join("cookies.sqlite"),
            b"private corrupt database",
        )
        .unwrap();
        let failure = profile(&directory)
            .load("instagram.com")
            .await
            .unwrap_err()
            .to_string();
        assert!(!failure.contains("private"));
        assert!(profile(&directory).load("evil.test").await.is_err());
    }

    #[tokio::test]
    async fn bounds_cookie_values_rows_count_and_total() {
        let (directory, connection) = database();
        insert(
            &connection,
            ".instagram.com",
            "name",
            &"x".repeat(MAX_VALUE_BYTES + 1),
            "",
        );
        assert!(
            profile(&directory)
                .load("instagram.com")
                .await
                .unwrap_err()
                .to_string()
                .contains("size limit")
        );
        connection
            .execute_batch("DELETE FROM moz_cookies;")
            .unwrap();
        insert(
            &connection,
            ".instagram.com",
            &"n".repeat(MAX_ROW_BYTES),
            "x",
            "",
        );
        assert!(
            profile(&directory)
                .load("instagram.com")
                .await
                .unwrap_err()
                .to_string()
                .contains("size limit")
        );
        connection
            .execute_batch("DELETE FROM moz_cookies;")
            .unwrap();
        for _ in 0..=MAX_COOKIES {
            insert(&connection, ".instagram.com", "name", "x", "");
        }
        assert!(
            profile(&directory)
                .load("instagram.com")
                .await
                .unwrap_err()
                .to_string()
                .contains("count limit")
        );
        connection
            .execute_batch("DELETE FROM moz_cookies;")
            .unwrap();
        for _ in 0..600 {
            insert(
                &connection,
                ".instagram.com",
                "name",
                &"x".repeat(MAX_VALUE_BYTES),
                "",
            );
        }
        assert!(
            profile(&directory)
                .load("instagram.com")
                .await
                .unwrap_err()
                .to_string()
                .contains("total size limit")
        );
    }

    #[tokio::test]
    async fn rejects_wrong_sql_types_flags_and_cookie_syntax() {
        let (directory, connection) = database();
        for sql in [
            "INSERT INTO moz_cookies VALUES ('name','value','.instagram.com','/',2,0,'')",
            "INSERT INTO moz_cookies VALUES ('name','value','.instagram.com','/',0,-1,'')",
            "INSERT INTO moz_cookies VALUES ('name','value','.instagram.com','/',0,'private-invalid','')",
            "INSERT INTO moz_cookies VALUES ('name',X'FE','.instagram.com','/',0,0,'')",
            "INSERT INTO moz_cookies VALUES ('invalid name','value','.instagram.com','/',0,0,'')",
            "INSERT INTO moz_cookies VALUES ('name','value','.instagram.com','invalid',0,0,'')",
        ] {
            connection
                .execute_batch("DELETE FROM moz_cookies;")
                .unwrap();
            connection.execute_batch(sql).unwrap();
            let failure = profile(&directory)
                .load("instagram.com")
                .await
                .unwrap_err()
                .to_string();
            assert!(!failure.contains("private-invalid"));
        }
    }
}
