//! Source-compatible metadata postprocessor archives. These intentionally use
//! gallery-dl's `entry` schema, separate from the native download archive.
use super::{metadata::expand, template::Template, truthy};
use crate::{Result, error};
use rusqlite::{Connection, OpenFlags, OptionalExtension, types::Value as SqlValue};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::{
    cell::RefCell,
    collections::HashMap,
    fmt,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{Mutex as AsyncMutex, Notify, OnceCell, OwnedMutexGuard};

const CACHE_KEY: &str = "_archive_metadata";
const MAX_PATH: usize = 4096;
const MAX_CONNECTIONS: usize = 128;
fn invalid() -> crate::Error {
    error("invalid postprocessor archive configuration")
}
fn failed() -> crate::Error {
    error("postprocessor archive operation failed")
}
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Clone)]
enum ArchivePath {
    Literal(String),
    Components(Vec<String>),
}
#[derive(Clone)]
pub(crate) struct ArchiveConfig {
    path: ArchivePath,
    prefix: Option<String>,
    format: Option<String>,
    table: Option<String>,
    reuse: bool,
    pragmas: Vec<String>,
}
impl fmt::Debug for ArchiveConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ArchiveConfig { .. }")
    }
}
fn string(options: &Map<String, Value>, key: &str) -> Result<Option<String>> {
    match options.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.len() <= 65536 && !s.contains('\0') => Ok(Some(s.clone())),
        _ => Err(invalid()),
    }
}
impl ArchiveConfig {
    pub(crate) fn uses_source_format(&self) -> bool {
        self.format.is_none()
    }
    pub(crate) fn parse(options: &Map<String, Value>, _base: &Path) -> Result<Option<Self>> {
        let Some(path) = options.get("archive").filter(|v| truthy(v)) else {
            return Ok(None);
        };
        let valid = |s: &str| s.len() <= MAX_PATH && !s.contains('\0');
        let path = match path {
            Value::String(s) if valid(s) => {
                if s.starts_with("postgres://") || s.starts_with("postgresql://") {
                    return Err(error("PostgreSQL postprocessor archives are unsupported"));
                }
                if s.starts_with("file:") {
                    return Err(error("SQLite URI postprocessor archives are unsupported"));
                }
                ArchivePath::Literal(s.clone())
            }
            Value::Array(parts) if parts.len() <= 128 => ArchivePath::Components(
                parts
                    .iter()
                    .map(|v| {
                        v.as_str()
                            .filter(|s| valid(s))
                            .map(str::to_owned)
                            .ok_or_else(invalid)
                    })
                    .collect::<Result<_>>()?,
            ),
            _ => return Err(invalid()),
        };
        let pragmas = match options.get("archive-pragma").filter(|v| truthy(v)) {
            None => Vec::new(),
            Some(Value::Array(values)) if values.len() <= 64 => values
                .iter()
                .map(|v| {
                    let s = v
                        .as_str()
                        .filter(|s| {
                            s.len() <= 4096
                                && !s.contains(['\0', ';', '\n', '\r'])
                                && !s.contains("--")
                                && !s.contains("/*")
                        })
                        .ok_or_else(invalid)?;
                    Ok(s.to_owned())
                })
                .collect::<Result<_>>()?,
            _ => return Err(invalid()),
        };
        Ok(Some(Self {
            path,
            prefix: string(options, "archive-prefix")?,
            format: string(options, "archive-format")?,
            table: string(options, "archive-table")?,
            reuse: options.get("archive-reuse").is_some_and(truthy),
            pragmas,
        }))
    }
    pub(crate) async fn open(
        &self,
        category: &str,
        archive_format: Option<&str>,
        keywords: &Map<String, Value>,
        root: &Path,
        directory: &Path,
    ) -> Result<ArchiveSession> {
        let prefix =
            self.prefix
                .as_deref()
                .unwrap_or(if self.table.is_none() { category } else { "" });
        let format = match &self.format {
            Some(f) => f.clone(),
            None => format!(
                "_MD_{}",
                archive_format.ok_or_else(|| error("source archive format is unavailable"))?
            ),
        };
        let keygen = Template::parse(&format!("{prefix}{format}"))?;
        let path = match &self.path {
            ArchivePath::Literal(s) => expand(s)?,
            ArchivePath::Components(parts) => component_path(parts, keywords, root, directory)?,
        };
        if path.as_os_str().len() > MAX_PATH || path.as_os_str().is_empty() {
            return Err(invalid());
        }
        let table = match &self.table {
            Some(t) if !t.is_empty() => Template::parse(t)?.render(keywords)?,
            Some(_) => String::new(),
            None => "archive".into(),
        };
        if table.len() > 4096 || table.contains('\0') {
            return Err(invalid());
        }
        let table = format!("\"{}\"", table.replace('"', "_"));
        let closed = Arc::new(AtomicBool::new(false));
        let db = if self.reuse {
            let cell = {
                let mut pool = lock(pool());
                // Failed/cancelled initializations do not permanently occupy slots.
                pool.retain(|_, c| c.get().is_some() || Arc::strong_count(c) > 1);
                if let Some(cell) = pool.get(&path) {
                    cell.clone()
                } else {
                    if pool.len() >= MAX_CONNECTIONS {
                        return Err(error("postprocessor archive connection limit exceeded"));
                    }
                    let cell = Arc::new(OnceCell::new());
                    pool.insert(path.clone(), cell.clone());
                    cell
                }
            };
            cell.get_or_try_init(|| open_database(path, self.pragmas.clone()))
                .await?
                .clone()
        } else {
            open_database(path, self.pragmas.clone()).await?
        };
        let state = Arc::new(SessionState {
            db: Mutex::new(Some(db)),
            closed,
            active: Arc::new(Active::default()),
            closed_notify: Notify::new(),
            table,
            keygen,
        });
        let session = ArchiveSession(state);
        let table = session.0.table.clone();
        session
            .operation(move |con, canceled, closed| {
                let tx = con.transaction().map_err(|_| failed())?;
                tx.execute(
                    &format!(
                        "CREATE TABLE IF NOT EXISTS {table} (entry TEXT PRIMARY KEY) WITHOUT ROWID"
                    ),
                    [],
                )
                .map_err(|_| failed())?;
                // Validate existing source tables without altering their schema.
                tx.prepare(&format!("SELECT entry FROM {table} LIMIT 0"))
                    .map_err(|_| failed())?;
                ensure(canceled, closed)?;
                tx.commit().map_err(|_| failed())
            })
            .await?;
        Ok(session)
    }
}
fn component_path(
    parts: &[String],
    keywords: &Map<String, Value>,
    root: &Path,
    directory: &Path,
) -> Result<PathBuf> {
    let mut out = PathBuf::new();
    let mut offset = 0;
    if let Some(first) = parts.first() {
        if first.is_empty() {
            return Err(invalid());
        }
        if first.starts_with(':') {
            if ":basedirectory".starts_with(first) {
                out = root.to_owned();
            } else if ":directory".starts_with(first) {
                out = directory.to_owned();
            } else if let Some(home) = first.strip_prefix(":~") {
                out = expand(&format!("~{home}"))?;
            } else if let Some(env) = first.strip_prefix(":$") {
                if let Some(value) = std::env::var_os(env) {
                    out = PathBuf::from(value);
                } else {
                    offset = usize::MAX;
                }
            } else {
                out = PathBuf::from(first);
            }
            offset = if offset == usize::MAX { 0 } else { 1 };
        } else if Path::new(first).is_absolute() {
            out = PathBuf::from(first);
            offset = 1;
        }
    }
    // Upstream generate_path indexes the explicit root's final character;
    // an empty :directory/:base/environment root fails initialization. It must
    // never be reinterpreted as permission to create an archive in the CWD.
    if offset == 1 && out.as_os_str().is_empty() {
        return Err(invalid());
    }
    for part in &parts[offset..] {
        let value = Template::parse(part)?.render(keywords)?;
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        if value.len() > MAX_PATH
            || out
                .as_os_str()
                .len()
                .saturating_add(value.len())
                .saturating_add(1)
                > MAX_PATH
        {
            return Err(invalid());
        }
        if value == "." || value == ".." || value.contains(['/', '\\', '\0']) {
            return Err(error("unsafe postprocessor archive path component"));
        }
        out.push(value);
    }
    Ok(out)
}

type Pool = HashMap<PathBuf, Arc<OnceCell<Arc<Database>>>>;
fn pool() -> &'static Mutex<Pool> {
    static POOL: OnceLock<Mutex<Pool>> = OnceLock::new();
    POOL.get_or_init(Mutex::default)
}
struct Database {
    connection: Mutex<Connection>,
    path: PathBuf,
}
type Leases = HashMap<(PathBuf, [u8; 32]), Weak<AsyncMutex<()>>>;
fn leases() -> &'static Mutex<Leases> {
    static LEASES: OnceLock<Mutex<Leases>> = OnceLock::new();
    LEASES.get_or_init(Mutex::default)
}
#[derive(Default)]
struct Active {
    count: AtomicUsize,
    notify: Notify,
}
struct ActiveGuard(Arc<Active>);
impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.count.fetch_sub(1, Ordering::AcqRel);
        self.0.notify.notify_waiters();
    }
}
struct SessionState {
    db: Mutex<Option<Arc<Database>>>,
    closed: Arc<AtomicBool>,
    active: Arc<Active>,
    closed_notify: Notify,
    table: String,
    keygen: Template,
}
impl Drop for SessionState {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Release);
        self.closed_notify.notify_waiters();
    }
}
#[derive(Clone)]
pub(crate) struct ArchiveSession(Arc<SessionState>);
impl fmt::Debug for ArchiveSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ArchiveSession { .. }")
    }
}
pub(crate) struct ArchiveCheck {
    pub(crate) hit: bool,
    pub(crate) guard: ArchiveGuard,
}
pub(crate) struct ArchiveGuard {
    _guard: OwnedMutexGuard<()>,
}
impl ArchiveSession {
    fn database(&self) -> Result<Arc<Database>> {
        lock(&self.0.db).clone().ok_or_else(failed)
    }
    pub(crate) async fn check(&self, keywords: &mut Map<String, Value>) -> Result<ArchiveCheck> {
        let key = self.0.keygen.render(keywords)?;
        keywords.insert(CACHE_KEY.into(), Value::String(key.clone()));
        let db = self.database()?;
        let mut digest = Sha256::new();
        digest.update(self.0.table.as_bytes());
        digest.update([0]);
        digest.update(key.as_bytes());
        let lease_key = (db.path.clone(), digest.finalize().into());
        let lease = {
            let mut leases = lock(leases());
            leases.retain(|_, v| v.strong_count() > 0);
            if let Some(lease) = leases.get(&lease_key).and_then(Weak::upgrade) {
                lease
            } else {
                if leases.len() >= 65536 {
                    return Err(error("postprocessor archive execution limit exceeded"));
                }
                let lease = Arc::new(AsyncMutex::new(()));
                leases.insert(lease_key, Arc::downgrade(&lease));
                lease
            }
        };
        let notified = self.0.closed_notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.0.closed.load(Ordering::Acquire) {
            return Err(failed());
        }
        let guard = tokio::select! {
            guard = lease.lock_owned() => ArchiveGuard { _guard: guard },
            _ = notified => return Err(failed()),
        };
        let table = self.0.table.clone();
        let hit = self
            .operation(move |con, _, _| {
                con.query_row(
                    &format!("SELECT 1 FROM {table} WHERE entry=? LIMIT 1"),
                    [key],
                    |_| Ok(()),
                )
                .optional()
                .map(|v| v.is_some())
                .map_err(|_| failed())
            })
            .await?;
        Ok(ArchiveCheck { hit, guard })
    }
    pub(crate) async fn add(&self, keywords: &Map<String, Value>) -> Result<()> {
        let key = match keywords.get(CACHE_KEY).filter(|v| truthy(v)) {
            Some(Value::String(s)) => SqlValue::Text(s.clone()),
            Some(Value::Bool(b)) => SqlValue::Integer(i64::from(*b)),
            Some(Value::Number(n)) => {
                if let Some(i) = n.as_i64() {
                    SqlValue::Integer(i)
                } else if n.to_string().contains(['.', 'e', 'E'])
                    && let Some(f) = n.as_f64().filter(|v| v.is_finite())
                {
                    SqlValue::Real(f)
                } else {
                    return Err(failed());
                }
            }
            Some(_) => return Err(failed()),
            None => SqlValue::Text(self.0.keygen.render(keywords)?),
        };
        let table = self.0.table.clone();
        self.operation(move |con, canceled, closed| {
            let tx = con.transaction().map_err(|_| failed())?;
            tx.execute(
                &format!("INSERT OR IGNORE INTO {table}(entry) VALUES (?)"),
                [key],
            )
            .map_err(|_| failed())?;
            ensure(canceled, closed)?;
            tx.commit().map_err(|_| failed())
        })
        .await
    }
    pub(crate) async fn close(&self) -> Result<()> {
        {
            let mut db = lock(&self.0.db);
            self.0.closed.store(true, Ordering::Release);
            self.0.closed_notify.notify_waiters();
            db.take();
        }
        loop {
            let notified = self.0.active.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.0.active.count.load(Ordering::Acquire) == 0 {
                return Ok(());
            }
            notified.await;
        }
    }
    async fn operation<T: Send + 'static>(
        &self,
        work: impl FnOnce(&mut Connection, &AtomicBool, &AtomicBool) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let (db, active) = {
            let db = lock(&self.0.db);
            if self.0.closed.load(Ordering::Acquire) {
                return Err(failed());
            }
            let db = db.clone().ok_or_else(failed)?;
            self.0.active.count.fetch_add(1, Ordering::AcqRel);
            (db, ActiveGuard(self.0.active.clone()))
        };
        let closed = self.0.closed.clone();
        let cancel = Cancel::default();
        let state = cancel.0.clone();
        let task = tokio::task::spawn_blocking(move || {
            let result = {
                match connection_lock(&db.connection, &state.canceled, &closed) {
                    Ok(mut con) => execute(&mut con, &state, &closed, work),
                    Err(failure) => Err(failure),
                }
            };
            drop(db);
            drop(active);
            result
        });
        task.await.map_err(|_| failed())?
    }
}
#[derive(Default)]
struct Cancel(Arc<CancelState>);
#[derive(Default)]
struct CancelState {
    canceled: Arc<AtomicBool>,
    interrupt: Mutex<Option<rusqlite::InterruptHandle>>,
}
impl Drop for Cancel {
    fn drop(&mut self) {
        self.0.canceled.store(true, Ordering::Release);
        if let Some(h) = lock(&self.0.interrupt).as_ref() {
            h.interrupt();
        }
    }
}
fn ensure(canceled: &AtomicBool, closed: &AtomicBool) -> Result<()> {
    if canceled.load(Ordering::Acquire) || closed.load(Ordering::Acquire) {
        Err(failed())
    } else {
        Ok(())
    }
}
// A blocking mutex acquisition cannot observe dropped async callers. Polling
// try_lock on this blocking worker bounds retention even when another session
// owns the physical connection for an unrelated operation.
fn connection_lock<'a>(
    connection: &'a Mutex<Connection>,
    canceled: &AtomicBool,
    closed: &AtomicBool,
) -> Result<std::sync::MutexGuard<'a, Connection>> {
    loop {
        ensure(canceled, closed)?;
        match connection.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(std::sync::TryLockError::Poisoned(e)) => return Ok(e.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => {
                std::thread::sleep(Duration::from_millis(5))
            }
        }
    }
}
type BusyState = (Arc<AtomicBool>, Arc<AtomicBool>, Instant);
thread_local! { static BUSY: RefCell<Option<BusyState>> = const { RefCell::new(None) }; }
fn busy(_: i32) -> bool {
    let keep = BUSY.with(|v| {
        v.borrow().as_ref().is_some_and(|(cancel, closed, start)| {
            !cancel.load(Ordering::Acquire)
                && !closed.load(Ordering::Acquire)
                && start.elapsed() < Duration::from_secs(60)
        })
    });
    if keep {
        std::thread::sleep(Duration::from_millis(5));
    }
    keep
}
fn execute<T>(
    con: &mut Connection,
    state: &Arc<CancelState>,
    closed: &Arc<AtomicBool>,
    work: impl FnOnce(&mut Connection, &AtomicBool, &AtomicBool) -> Result<T>,
) -> Result<T> {
    ensure(&state.canceled, closed)?;
    *lock(&state.interrupt) = Some(con.get_interrupt_handle());
    let canceled = state.canceled.clone();
    let close = closed.clone();
    con.progress_handler(
        128,
        Some(move || canceled.load(Ordering::Acquire) || close.load(Ordering::Acquire)),
    )
    .map_err(|_| failed())?;
    BUSY.with(|v| *v.borrow_mut() = Some((state.canceled.clone(), closed.clone(), Instant::now())));
    con.busy_handler(Some(busy)).map_err(|_| failed())?;
    let result = ensure(&state.canceled, closed).and_then(|_| work(con, &state.canceled, closed));
    let _ = con.progress_handler(0, None::<fn() -> bool>);
    let _ = con.busy_handler(None);
    BUSY.with(|v| *v.borrow_mut() = None);
    lock(&state.interrupt).take();
    result
}
async fn open_database(path: PathBuf, pragmas: Vec<String>) -> Result<Arc<Database>> {
    let cancel = Cancel::default();
    let state = cancel.0.clone();
    tokio::task::spawn_blocking(move || {
        let closed = Arc::new(AtomicBool::new(false));
        ensure(&state.canceled, &closed)?;
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW;
        if path != Path::new(":memory:")
            && let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent).map_err(|_| failed())?;
        }
        ensure(&state.canceled, &closed)?;
        // Resolve parent symlinks (e.g. macOS /var) while retaining the
        // original path as the source reuse key. NOFOLLOW protects the DB file.
        let physical = if path == Path::new(":memory:") {
            path.clone()
        } else {
            let parent = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            std::fs::canonicalize(parent)
                .map_err(|_| failed())?
                .join(path.file_name().ok_or_else(failed)?)
        };
        let mut con = Connection::open_with_flags(&physical, flags).map_err(|_| failed())?;
        execute(&mut con, &state, &closed, |con, cancel, closed| {
            for pragma in pragmas {
                ensure(cancel, closed)?;
                let mut stmt = con
                    .prepare(&format!("PRAGMA {pragma}"))
                    .map_err(|_| failed())?;
                let mut rows = stmt.query([]).map_err(|_| failed())?;
                while rows.next().map_err(|_| failed())?.is_some() {
                    ensure(cancel, closed)?;
                }
            }
            Ok(())
        })?;
        Ok(Arc::new(Database {
            connection: Mutex::new(con),
            path,
        }))
    })
    .await
    .map_err(|_| failed())?
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn config(value: Value) -> ArchiveConfig {
        ArchiveConfig::parse(
            value.as_object().unwrap(),
            Path::new("/ignored/config/base"),
        )
        .unwrap()
        .unwrap()
    }
    fn keywords() -> Map<String, Value> {
        json!({"id":17,"num":2,"extension":"jpg"})
            .as_object()
            .unwrap()
            .clone()
    }
    async fn open(config: &ArchiveConfig, root: &Path) -> ArchiveSession {
        config
            .open(
                "fixture",
                Some("{id}_{num}.{extension}"),
                &keywords(),
                root,
                root,
            )
            .await
            .unwrap()
    }
    #[tokio::test]
    async fn genuine_source_key_schema_cache_vectors() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/postprocessors/archive.json"
        ))
        .unwrap();
        for case in fixture["cases"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["action"] == "check_add")
        {
            let dir = tempfile::tempdir().unwrap();
            let mut options = case["options"].as_object().unwrap().clone();
            options.insert("archive".into(), json!(dir.path().join("source.sqlite")));
            let session = open(&config(Value::Object(options)), dir.path()).await;
            let mut kw = case["keywords"].as_object().unwrap().clone();
            let first = session.check(&mut kw).await.unwrap();
            assert_eq!(
                first.hit,
                case["before"].as_bool().unwrap(),
                "{}",
                case["name"]
            );
            assert_eq!(kw[CACHE_KEY], case["cached"], "{}", case["name"]);
            if let Some(mutation) = case["mutation"].as_object() {
                kw.extend(mutation.clone());
            }
            session.add(&kw).await.unwrap();
            drop(first.guard);
            let second = session.check(&mut kw).await.unwrap();
            assert_eq!(
                second.hit,
                case["after"].as_bool().unwrap(),
                "{}",
                case["name"]
            );
            drop(second.guard);
            session.close().await.unwrap();
            let con = Connection::open(dir.path().join("source.sqlite")).unwrap();
            let expected = &case["tables"][0];
            let table = expected["name"].as_str().unwrap();
            let (actual_name, actual_sql): (String, String) = con
                .query_row(
                    "SELECT name,sql FROM sqlite_master WHERE type='table'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(actual_name, table);
            // SQLite stores CREATE text with the original identifier spelling;
            // compare source schema structurally and preserve WITHOUT ROWID.
            assert!(actual_sql.ends_with("WITHOUT ROWID"));
            let mut stmt = con
                .prepare(&format!(
                    "SELECT entry FROM \"{}\" ORDER BY entry",
                    table.replace('"', "\"\"")
                ))
                .unwrap();
            let rows: Vec<String> = stmt
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<std::result::Result<_, _>>()
                .unwrap();
            assert_eq!(json!(rows), expected["rows"], "{}", case["name"]);
        }
    }
    #[tokio::test]
    async fn source_database_interoperability_default_descriptor_and_explicit_override() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("existing.sqlite");
        {
            let con = Connection::open(&path).unwrap();
            con.execute_batch("CREATE TABLE archive(entry TEXT PRIMARY KEY) WITHOUT ROWID; INSERT INTO archive VALUES('fixture_MD_17_2.jpg');").unwrap();
        }
        let cfg = config(json!({"archive":path}));
        assert!(
            cfg.open("fixture", None, &keywords(), dir.path(), dir.path())
                .await
                .is_err()
        );
        let s = open(&cfg, dir.path()).await;
        assert!(s.check(&mut keywords()).await.unwrap().hit);
        s.close().await.unwrap();
        let explicit = config(json!({"archive":path,"archive-format":"{id}"}));
        explicit
            .open("fixture", None, &keywords(), dir.path(), dir.path())
            .await
            .unwrap()
            .close()
            .await
            .unwrap();
    }
    #[tokio::test]
    async fn leases_serialize_separate_sessions_without_reuse_and_close_is_shared() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(json!({"archive":dir.path().join("db")}));
        let a = open(&cfg, dir.path()).await;
        let b = open(&cfg, dir.path()).await;
        let first = a.check(&mut keywords()).await.unwrap();
        assert!(!first.hit);
        let pending = tokio::spawn({
            let b = b.clone();
            async move { b.check(&mut keywords()).await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!pending.is_finished());
        a.add(&keywords()).await.unwrap();
        drop(first.guard);
        assert!(pending.await.unwrap().unwrap().hit);
        let clone = a.clone();
        a.close().await.unwrap();
        a.close().await.unwrap();
        assert!(clone.add(&keywords()).await.is_err());
        assert!(b.check(&mut keywords()).await.unwrap().hit);
        b.close().await.unwrap();
    }
    #[tokio::test]
    async fn close_wakes_a_check_waiting_for_another_sessions_key_lease() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(json!({"archive":dir.path().join("lease.sqlite")}));
        let a = open(&cfg, dir.path()).await;
        let b = open(&cfg, dir.path()).await;
        let held = a.check(&mut keywords()).await.unwrap();
        let pending = tokio::spawn({
            let b = b.clone();
            async move { b.check(&mut keywords()).await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!pending.is_finished());
        b.close().await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), pending)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        drop(held.guard);
        a.close().await.unwrap();
    }
    #[tokio::test]
    async fn reused_connections_keep_state_and_first_pragmas_but_sessions_close_independently() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reuse.sqlite");
        let a = open(
            &config(
                json!({"archive":path,"archive-reuse":true,"archive-pragma":["cache_size=23"]}),
            ),
            dir.path(),
        )
        .await;
        a.add(&keywords()).await.unwrap();
        let db = a.database().unwrap();
        a.close().await.unwrap();
        let b = open(
            &config(
                json!({"archive":path,"archive-reuse":true,"archive-pragma":["cache_size=47"]}),
            ),
            dir.path(),
        )
        .await;
        assert!(Arc::ptr_eq(&db, &b.database().unwrap()));
        assert_eq!(
            lock(&db.connection)
                .query_row("PRAGMA cache_size", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            23
        );
        assert!(b.check(&mut keywords()).await.unwrap().hit);
        b.close().await.unwrap();
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn canceled_queued_insert_does_not_mutate_and_close_waits_for_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let s = open(
            &config(json!({"archive":dir.path().join("db")})),
            dir.path(),
        )
        .await;
        let db = s.database().unwrap();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn({
            let db = db.clone();
            move || {
                let _held = lock(&db.connection);
                ready_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            }
        });
        ready_rx.await.unwrap();
        let writer = tokio::spawn({
            let s = s.clone();
            async move { s.add(&keywords()).await }
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while s.0.active.count.load(Ordering::Acquire) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        writer.abort();
        assert!(writer.await.unwrap_err().is_cancelled());
        let closer = tokio::spawn({
            let s = s.clone();
            async move { s.close().await }
        });
        // Canceled work must release its active lease without waiting for the
        // unrelated connection owner to finish.
        tokio::time::timeout(Duration::from_secs(2), closer)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        release_tx.send(()).unwrap();
        holder.join().unwrap();
        assert_eq!(
            lock(&db.connection)
                .query_row("SELECT count(*) FROM archive", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_interrupts_busy_sqlite_without_late_commit_and_nonreuse_drops() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("busy.sqlite");
        let s = open(&config(json!({"archive":path})), dir.path()).await;
        let db = Arc::downgrade(&s.database().unwrap());
        let external = Connection::open(&path).unwrap();
        external.execute_batch("BEGIN IMMEDIATE").unwrap();
        let writer = tokio::spawn({
            let s = s.clone();
            async move { s.add(&keywords()).await }
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while s.0.active.count.load(Ordering::Acquire) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(25)).await;
        writer.abort();
        let _ = writer.await;
        tokio::time::timeout(Duration::from_secs(1), s.close())
            .await
            .unwrap()
            .unwrap();
        assert!(db.upgrade().is_none());
        external.execute_batch("ROLLBACK").unwrap();
        assert_eq!(
            external
                .query_row("SELECT count(*) FROM archive", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
    #[tokio::test]
    async fn custom_completed_media_is_allowed_but_incompatible_schema_is_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("custom.sqlite");
        let cfg = config(json!({"archive":path,"archive-table":"completed_media"}));
        let s = open(&cfg, dir.path()).await;
        s.add(&keywords()).await.unwrap();
        s.close().await.unwrap();
        assert_eq!(
            Connection::open(&path)
                .unwrap()
                .query_row("SELECT count(*) FROM completed_media", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        let native = dir.path().join("native.sqlite");
        let con = Connection::open(&native).unwrap();
        con.execute_batch("PRAGMA application_id=1195658322; CREATE TABLE completed_media(identity TEXT PRIMARY KEY); INSERT INTO completed_media VALUES('sentinel');").unwrap();
        let cfg = config(json!({"archive":native,"archive-table":"completed_media"}));
        assert!(
            cfg.open("fixture", Some("{id}"), &keywords(), dir.path(), dir.path())
                .await
                .is_err()
        );
        assert_eq!(
            con.query_row("SELECT identity FROM completed_media", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "sentinel"
        );
        assert_eq!(
            con.query_row("PRAGMA application_id", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1195658322
        );
    }
    #[test]
    fn empty_explicit_component_roots_fail_without_falling_back_to_cwd() {
        for root in [":directory", ":dir", ":basedirectory", ":base"] {
            let parts = vec![root.to_owned(), "archive.sqlite".to_owned()];
            assert!(component_path(&parts, &keywords(), Path::new(""), Path::new("")).is_err());
        }
        let missing = "GALLERY_DL_RS_ARCHIVE_TEST_UNSET_30CDF27BE9";
        assert!(std::env::var_os(missing).is_none());
        let parts = vec![format!(":${missing}"), "archive.sqlite".to_owned()];
        // An absent environment root is different upstream: root=None means
        // format the entire component list, including its literal first entry.
        assert_eq!(
            component_path(&parts, &keywords(), Path::new(""), Path::new("")).unwrap(),
            PathBuf::from(&parts[0]).join("archive.sqlite")
        );
    }
    #[tokio::test]
    async fn component_roots_string_literals_and_private_errors() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(json!({"archive":[":base","{id}","archive.sqlite"]}));
        open(&cfg, dir.path()).await.close().await.unwrap();
        assert!(dir.path().join("17/archive.sqlite").exists());
        let literal = dir.path().join("{id}.sqlite");
        open(&config(json!({"archive":literal})), dir.path())
            .await
            .close()
            .await
            .unwrap();
        assert!(literal.exists());
        let bad = config(json!({"archive":[":base","{id}"]}));
        let mut kw = keywords();
        kw.insert("id".into(), json!("../secret-token"));
        let err = bad
            .open("fixture", Some("{id}"), &kw, dir.path(), dir.path())
            .await
            .unwrap_err()
            .to_string();
        assert!(!err.contains("secret-token"));
        for value in [
            json!({"archive":"postgresql://secret-token"}),
            json!({"archive":"file:secret-token?mode=ro"}),
            json!({"archive":"db","archive-pragma":["cache_size=1; DROP TABLE archive"]}),
        ] {
            let err = ArchiveConfig::parse(value.as_object().unwrap(), dir.path())
                .unwrap_err()
                .to_string();
            assert!(!err.contains("secret-token"));
        }
    }
}
