//! Read-only browser cookie imports: explicit Firefox profiles and
//! Chromium-based browsers (see [`crate::browser_chromium`]).

use std::{
    fmt,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use rusqlite::{Connection, OpenFlags, params};

use crate::{Result, cookies::CookieJar, error};

const MAX_COOKIES: usize = 4096;
const MAX_VALUE_BYTES: usize = 4096;
const MAX_ROW_BYTES: usize = 16 * 1024;
const MAX_EXPORT_BYTES: usize = 2 * 1024 * 1024;

#[derive(Default)]
struct ImportCancellation {
    cancelled: AtomicBool,
    interrupt: Mutex<Option<rusqlite::InterruptHandle>>,
}

impl ImportCancellation {
    fn check(&self) -> Result<()> {
        if self.cancelled.load(Ordering::Acquire) {
            Err(error("Firefox cookie import cancelled"))
        } else {
            Ok(())
        }
    }

    fn attach(self: &Arc<Self>, connection: &Connection) -> Result<()> {
        *self.interrupt.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(connection.get_interrupt_handle());
        let cancellation = self.clone();
        // An interrupt sent while SQLite is idle does not affect its next
        // statement. The progress handler also catches that cancellation race.
        connection
            .progress_handler(
                1000,
                Some(move || cancellation.cancelled.load(Ordering::Acquire)),
            )
            .map_err(|_| error("could not initialize Firefox cookie import"))?;
        self.check()
    }
}

struct CancelImport(Arc<ImportCancellation>);

impl Drop for CancelImport {
    fn drop(&mut self) {
        self.0.cancelled.store(true, Ordering::Release);
        if let Some(interrupt) = self
            .0
            .interrupt
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            interrupt.interrupt();
        }
    }
}

pub use crate::browser_chromium::ChromiumProfile;

/// A `--cookies-from-browser` source: `firefox:PROFILE_PATH`, or a Chromium
/// browser such as `helium`, `helium-neo:Profile 1` or `chrome:/path`.
#[derive(Clone, Debug)]
pub enum BrowserProfile {
    Firefox(FirefoxProfile),
    Chromium(ChromiumProfile),
}

impl BrowserProfile {
    pub fn parse(spec: &str) -> Result<Self> {
        let name = spec.split([':', '+']).next().unwrap_or_default();
        if crate::browser_chromium::is_browser(&name.to_ascii_lowercase()) {
            ChromiumProfile::parse(spec).map(Self::Chromium)
        } else {
            FirefoxProfile::parse(spec).map(Self::Firefox)
        }
    }

    /// Import only the requested site's cookies.
    pub async fn load(&self, site: &str) -> Result<CookieJar> {
        match self {
            Self::Firefox(profile) => profile.load(site).await,
            Self::Chromium(profile) => profile.load(site).await,
        }
    }

    /// Whether yt-dlp can read this browser itself with `--cookies-from-browser`.
    pub fn ytdl_supported(&self) -> bool {
        match self {
            Self::Firefox(_) => true,
            Self::Chromium(profile) => profile.name() != "helium" && profile.name() != "helium-neo",
        }
    }
}

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
            return Err(error(
                "browser cookies require firefox:PROFILE_PATH or a Chromium browser \
                 (helium, helium-neo, chrome, chromium, brave, edge, vivaldi)",
            ));
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
        let path = self.path.clone();
        let site = site.to_owned();
        let cancellation = Arc::new(ImportCancellation::default());
        import(path, site, cancellation).await
    }
}

async fn import(
    path: PathBuf,
    site: String,
    cancellation: Arc<ImportCancellation>,
) -> Result<CookieJar> {
    // This guard belongs to the async caller, not the blocking task.
    // Dropping the load future interrupts scans and cancels queued imports.
    let _cancel = CancelImport(cancellation.clone());
    tokio::task::spawn_blocking(move || load(path, &site, &cancellation))
        .await
        .map_err(|_| error("Firefox cookie import task failed"))?
}

fn load(path: PathBuf, site: &str, cancellation: &Arc<ImportCancellation>) -> Result<CookieJar> {
    cancellation.check()?;
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
    cancellation.attach(&connection)?;
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
        cancellation.check()?;
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

    async fn worker_finished(cancellation: &Arc<ImportCancellation>) {
        tokio::time::timeout(Duration::from_secs(3), async {
            // The worker owns one reference, and its connection's progress
            // handler owns another. Both disappear when SQLite is closed.
            while Arc::strong_count(cancellation) != 1 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("cancelled Firefox import did not release its connection");
    }

    #[tokio::test]
    async fn dropping_import_interrupts_nonmatching_sqlite_scan_and_closes_connection() {
        let (directory, connection) = database();
        // Cookie count limits cannot bound a query scanning nonmatching rows.
        // A recursive view makes the expensive scan deterministic and compact.
        connection
            .execute_batch(
                "DROP TABLE moz_cookies;
             CREATE VIEW moz_cookies AS
             WITH RECURSIVE sequence(n) AS (
                 SELECT 1 UNION ALL SELECT n + 1 FROM sequence WHERE n < 1000000000
             )
             SELECT 'name' AS name, 'private-value' AS value,
                    printf('other%d.test', n) AS host, '/' AS path, 0 AS isSecure,
                    0 AS expiry, '' AS originAttributes FROM sequence;",
            )
            .unwrap();
        let cancellation = Arc::new(ImportCancellation::default());
        let mut pending = Box::pin(import(
            directory.path().to_path_buf(),
            "instagram.com".into(),
            cancellation.clone(),
        ));
        assert!(futures_util::poll!(pending.as_mut()).is_pending());
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if cancellation.interrupt.lock().unwrap().is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(futures_util::poll!(pending.as_mut()).is_pending());
        drop(pending);
        assert!(cancellation.cancelled.load(Ordering::Acquire));
        worker_finished(&cancellation).await;
        connection.busy_timeout(Duration::ZERO).unwrap();
        connection
            .execute_batch("BEGIN EXCLUSIVE; ROLLBACK;")
            .unwrap();
    }

    #[test]
    fn cancelled_queued_import_does_not_open_profile() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async {
            let (release, wait) = std::sync::mpsc::channel();
            let (ready, started) = tokio::sync::oneshot::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                ready.send(()).unwrap();
                wait.recv().unwrap();
            });
            started.await.unwrap();
            let cancellation = Arc::new(ImportCancellation::default());
            let mut pending = Box::pin(import(
                PathBuf::from("private-missing-profile"),
                "instagram.com".into(),
                cancellation.clone(),
            ));
            assert!(futures_util::poll!(pending.as_mut()).is_pending());
            drop(pending);
            assert!(cancellation.cancelled.load(Ordering::Acquire));
            release.send(()).unwrap();
            blocker.await.unwrap();
            worker_finished(&cancellation).await;
            assert!(cancellation.interrupt.lock().unwrap().is_none());
            // Cancellation wins before even inspecting this nonexistent path.
            let failure = load(
                PathBuf::from("private-missing-profile"),
                "instagram.com",
                &cancellation,
            )
            .unwrap_err()
            .to_string();
            assert_eq!(failure, "Firefox cookie import cancelled");
        });
    }

    #[test]
    fn cancellation_while_sqlite_is_idle_interrupts_next_statement() {
        let connection = Connection::open_in_memory().unwrap();
        let cancellation = Arc::new(ImportCancellation::default());
        cancellation.attach(&connection).unwrap();
        drop(CancelImport(cancellation));
        let result = connection.query_row(
            "WITH RECURSIVE sequence(n) AS (
                 SELECT 1 UNION ALL SELECT n + 1 FROM sequence WHERE n < 1000000000
             ) SELECT sum(n) FROM sequence",
            [],
            |row| row.get::<_, i64>(0),
        );
        assert!(
            matches!(result, Err(rusqlite::Error::SqliteFailure(error, _))
            if error.code == rusqlite::ErrorCode::OperationInterrupted)
        );
    }

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
    async fn pixiv_import_keeps_only_default_context_and_matching_domains() {
        let (directory, connection) = database();
        insert(
            &connection,
            ".pixiv.net",
            "PHPSESSID",
            "synthetic-session",
            "",
        );
        insert(&connection, "www.pixiv.net", "web", "yes", "");
        insert(
            &connection,
            ".pixiv.net",
            "container",
            "excluded",
            "^userContextId=2",
        );
        insert(&connection, ".evilpixiv.net", "suffix", "excluded", "");
        insert(
            &connection,
            ".pixiv.net.evil.test",
            "prefix",
            "excluded",
            "",
        );
        insert(&connection, ".instagram.com", "other", "excluded", "");
        let jar = profile(&directory).load("pixiv.net").await.unwrap();
        assert_eq!(
            header(&jar, "https://www.pixiv.net/ajax/illust/42"),
            Some("PHPSESSID=synthetic-session; web=yes".into())
        );
        assert_eq!(
            header(&jar, "https://pixiv.net/"),
            Some("PHPSESSID=synthetic-session".into())
        );
        assert_eq!(header(&jar, "https://evilpixiv.net/"), None);
        assert_eq!(header(&jar, "https://instagram.com/"), None);
    }

    #[tokio::test]
    async fn every_native_cookie_consumer_can_import_its_own_domain() {
        let (directory, connection) = database();
        let sites = [
            "instagram.com",
            "facebook.com",
            "x.com",
            "pixiv.net",
            "kemono.cr",
            "coomer.st",
            "furaffinity.net",
        ];
        for site in sites {
            insert(&connection, &format!(".{site}"), "session", site, "");
        }
        for site in sites {
            let jar = profile(&directory).load(site).await.unwrap();
            assert_eq!(
                header(&jar, &format!("https://www.{site}/")),
                Some(format!("session={site}"))
            );
            for other in sites.into_iter().filter(|other| *other != site) {
                assert_eq!(header(&jar, &format!("https://{other}/")), None);
            }
        }
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
