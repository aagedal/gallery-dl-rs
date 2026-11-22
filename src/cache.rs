//! Upstream's `cache.sqlite3`: a `data (key, value, expires)` table whose
//! values are Python pickles. Only string values (OAuth tokens) are read and
//! written, using the same keys and encoding as upstream, so tokens cached by
//! either implementation are shared.

use crate::{Result, error};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Install `cache.file` (or `--cache-file`); `None` uses upstream's default
/// location. Later calls are ignored.
pub fn set_path(path: Option<&str>) {
    let _ = PATH.set(path.map(|path| match path {
        ":memory:" => PathBuf::from(path),
        path => crate::postprocess::expand_path(path),
    }));
}

/// `$XDG_CACHE_HOME/gallery-dl/cache.sqlite3` (`~/.cache` by default), or
/// `%APPDATA%\gallery-dl\cache.sqlite3` on Windows. An empty `cache.file`
/// disables the cache.
pub fn path() -> Option<PathBuf> {
    if let Some(Some(path)) = PATH.get() {
        return (!path.as_os_str().is_empty()).then(|| path.clone());
    }
    // Unit tests never touch the user's cache.
    if cfg!(test) {
        return None;
    }
    let base = if cfg!(windows) {
        std::env::var("APPDATA").unwrap_or_else(|_| "~".into())
    } else {
        std::env::var("XDG_CACHE_HOME").unwrap_or_else(|_| "~/.cache".into())
    };
    Some(
        crate::postprocess::expand_path(&base)
            .join("gallery-dl")
            .join("cache.sqlite3"),
    )
}

fn open(path: &Path, create: bool) -> Result<Connection> {
    if path.as_os_str() == ":memory:" {
        return Err(error("the cache database is in memory"));
    }
    if create {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|_| error("cannot create the cache directory"))?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(path)
                .map_err(|_| error("cannot create the cache database"))?;
        }
    } else if !path.exists() {
        return Err(error("no cache database"));
    }
    let db = Connection::open(path).map_err(|_| error("cannot open the cache database"))?;
    db.busy_timeout(std::time::Duration::from_secs(60))
        .map_err(|_| error("cannot open the cache database"))?;
    db.execute(
        "CREATE TABLE IF NOT EXISTS data (key TEXT PRIMARY KEY, value TEXT, expires INTEGER)",
        [],
    )
    .map_err(|_| error("cannot initialize the cache database"))?;
    Ok(db)
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Upstream's key: `gallery_dl.extractor.MODULE.FUNCTION-ARGUMENT`.
pub fn key(module: &str, function: &str, argument: &str) -> String {
    format!("gallery_dl.extractor.{module}.{function}-{argument}")
}

/// A cached, unexpired string value.
pub fn get(key: &str) -> Option<String> {
    let db = open(&path()?, false).ok()?;
    let (value, expires): (Vec<u8>, i64) = db
        .query_row(
            "SELECT value, expires FROM data WHERE key=? LIMIT 1",
            [key],
            |row| Ok((value_bytes(row.get_ref(0)?), row.get(1)?)),
        )
        .optional()
        .ok()??;
    if expires != 0 && expires <= now() {
        return None;
    }
    unpickle_str(&value)
}

/// Store a string value without expiry.
pub fn set(key: &str, value: &str) -> Result<()> {
    let path = path().ok_or_else(|| error("the cache is disabled"))?;
    open(&path, true)?
        .execute(
            "INSERT OR REPLACE INTO data VALUES (?,?,?)",
            params![key, pickle_str(value), 0],
        )
        .map_err(|_| error("cannot write the cache database"))?;
    Ok(())
}

/// Delete a stored value.
pub fn remove(key: &str) {
    if let Some(db) = path().and_then(|path| open(&path, false).ok()) {
        let _ = db.execute("DELETE FROM data WHERE key=?", [key]);
    }
}

fn value_bytes(value: rusqlite::types::ValueRef<'_>) -> Vec<u8> {
    match value {
        rusqlite::types::ValueRef::Blob(bytes) | rusqlite::types::ValueRef::Text(bytes) => {
            bytes.to_vec()
        }
        _ => Vec::new(),
    }
}

/// Upstream's row selection for `--cache-show`/`--cache-clear`: `ALL`,
/// `VAL` (unexpired), `EXP` (expired) or a module name.
fn selection(module: &str) -> (String, Vec<rusqlite::types::Value>) {
    use rusqlite::types::Value;
    match module {
        "ALL" => (String::new(), Vec::new()),
        "VAL" => ("WHERE expires > ?".into(), vec![Value::Integer(now())]),
        "EXP" => (
            "WHERE expires < ? AND expires <> 0".into(),
            vec![Value::Integer(now())],
        ),
        module => {
            let module = module.to_lowercase();
            (
                "WHERE key LIKE 'gallery_dl.extractor.' || ? || '.%' \
                 OR key LIKE 'gallery_dl.extractor.utils.' || ? || '_%'"
                    .into(),
                vec![Value::Text(module.clone()), Value::Text(module)],
            )
        }
    }
}

/// `--cache-clear MODULE`: the number of deleted entries.
pub fn clear(module: &str) -> Result<usize> {
    let path = path().ok_or_else(|| error("the cache is disabled"))?;
    let db = open(&path, true)?;
    let (filter, values) = selection(module);
    db.execute(
        &format!("DELETE FROM data {filter}"),
        rusqlite::params_from_iter(values),
    )
    .map_err(|_| error("cannot modify the cache database"))
}

/// `--cache-show MODULE`: `(key, value, expires)` rows, values as JSON.
pub fn show(module: &str) -> Result<Vec<(String, String, i64)>> {
    let path = path().ok_or_else(|| error("the cache is disabled"))?;
    let db = open(&path, true)?;
    let (filter, values) = selection(module);
    let mut statement = db
        .prepare(&format!("SELECT key, value, expires FROM data {filter}"))
        .map_err(|_| error("cannot read the cache database"))?;
    let rows = statement
        .query_map(rusqlite::params_from_iter(values), |row| {
            Ok((
                row.get::<_, String>(0)?,
                value_bytes(row.get_ref(1)?),
                row.get::<_, Option<i64>>(2)?.unwrap_or(0),
            ))
        })
        .map_err(|_| error("cannot read the cache database"))?;
    let cut = if matches!(module, "ALL" | "EXP" | "VAL") {
        2
    } else {
        3
    };
    Ok(rows
        .filter_map(std::result::Result::ok)
        .map(|(key, value, expires)| {
            let key = key.split('.').skip(cut).collect::<Vec<_>>().join(".");
            let value = unpickle_str(&value).map_or_else(
                || "<Invalid Value>".to_owned(),
                |value| serde_json::Value::String(value).to_string(),
            );
            (key, value, expires)
        })
        .collect())
}

/// `--cache-status`: the file, its size and entry counts per module.
pub struct Status {
    pub path: PathBuf,
    pub size: u64,
    pub counts: Vec<(String, usize)>,
}

pub fn status() -> Result<Status> {
    let path = path().ok_or_else(|| error("the cache is disabled"))?;
    let db = open(&path, true)?;
    let mut statement = db
        .prepare("SELECT key FROM data")
        .map_err(|_| error("cannot read the cache database"))?;
    let keys = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|_| error("cannot read the cache database"))?;
    let mut counts = std::collections::BTreeMap::<String, usize>::new();
    let mut total = 0;
    for key in keys.filter_map(std::result::Result::ok) {
        let parts: Vec<&str> = key.split('.').collect();
        let name = match parts.as_slice() {
            [_, _, "utils", rest, ..] => rest.split('_').next().unwrap_or(rest).to_owned(),
            [_, "oauth", rest, ..] => rest.split('\'').nth(1).unwrap_or_default().to_owned(),
            [_, _, name, ..] => (*name).to_owned(),
            _ => key.clone(),
        };
        *counts.entry(name).or_default() += 1;
        total += 1;
    }
    let mut counts: Vec<_> = counts.into_iter().collect();
    counts.push(("Total".into(), total));
    counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let size = std::fs::metadata(&path).map_or(0, |m| m.len());
    Ok(Status { path, size, counts })
}

/// `--cache-vacuum`: bytes saved.
pub fn vacuum() -> Result<u64> {
    let path = path().ok_or_else(|| error("the cache is disabled"))?;
    let db = open(&path, true)?;
    let before = std::fs::metadata(&path).map_or(0, |m| m.len());
    db.execute("VACUUM", [])
        .map_err(|_| error("cannot vacuum the cache database"))?;
    let after = std::fs::metadata(&path).map_or(0, |m| m.len());
    Ok(before.saturating_sub(after))
}

/// `pickle.dumps(value)` with protocol 4.
fn pickle_str(value: &str) -> Vec<u8> {
    let bytes = value.as_bytes();
    let mut body = Vec::with_capacity(bytes.len() + 16);
    if bytes.len() < 256 {
        body.push(0x8c);
        body.push(bytes.len() as u8);
    } else {
        body.push(b'X');
        body.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    }
    body.extend_from_slice(bytes);
    body.extend_from_slice(&[0x94, b'.']);
    let mut out = vec![0x80, 4, 0x95];
    out.extend_from_slice(&(body.len() as u64).to_le_bytes());
    out.extend(body);
    out
}

/// A pickled `str` (protocols 2 to 5); anything else is `None`.
fn unpickle_str(data: &[u8]) -> Option<String> {
    let mut at = 0usize;
    let mut take = |count: usize| {
        let slice = data.get(at..at.checked_add(count)?)?;
        at += count;
        Some(slice)
    };
    let mut value = None;
    loop {
        match take(1)?[0] {
            0x80 => {
                take(1)?;
            }
            0x95 => {
                take(8)?;
            }
            0x94 => {}
            b'q' => {
                take(1)?;
            }
            b'r' => {
                take(4)?;
            }
            0x8c if value.is_none() => {
                let len = take(1)?[0] as usize;
                value = Some(take(len)?);
            }
            b'X' if value.is_none() => {
                let len = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
                value = Some(take(len)?);
            }
            0x8d if value.is_none() => {
                let len = usize::try_from(u64::from_le_bytes(take(8)?.try_into().ok()?)).ok()?;
                value = Some(take(len)?);
            }
            b'.' => return String::from_utf8(value?.to_vec()).ok(),
            _ => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pickled_strings_match_python() {
        assert_eq!(
            pickle_str("abc"),
            b"\x80\x04\x95\x07\x00\x00\x00\x00\x00\x00\x00\x8c\x03abc\x94."
        );
        assert_eq!(unpickle_str(&pickle_str("abc")).as_deref(), Some("abc"));
        let long = "a".repeat(300);
        assert_eq!(
            &pickle_str(&long)[..16],
            b"\x80\x04\x953\x01\x00\x00\x00\x00\x00\x00X,\x01\x00\x00"
        );
        assert_eq!(unpickle_str(&pickle_str(&long)), Some(long));
        assert_eq!(
            unpickle_str(b"\x80\x02X\x03\x00\x00\x00abcq\x00.").as_deref(),
            Some("abc")
        );
        assert_eq!(
            unpickle_str(b"\x80\x04\x95\x02\x00\x00\x00\x00\x00\x00\x00N."),
            None
        );
        assert_eq!(unpickle_str(b"\x80\x04\x8c\x05ab"), None);
    }

    #[test]
    fn values_round_trip_and_modules_are_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("cache.sqlite3");
        let db = open(&file, true).unwrap();
        let pixiv = key("pixiv", "_refresh_token_cache", "None");
        db.execute(
            "INSERT INTO data VALUES (?,?,0)",
            params![pixiv, pickle_str("TOKEN")],
        )
        .unwrap();
        db.execute(
            "INSERT INTO data VALUES ('gallery_dl.extractor.reddit._refresh_token_cache-#x',?,1)",
            params![pickle_str("OLD")],
        )
        .unwrap();
        drop(db);
        let _ = PATH.set(Some(file));
        assert_eq!(get(&pixiv).as_deref(), Some("TOKEN"));
        assert_eq!(
            get("gallery_dl.extractor.reddit._refresh_token_cache-#x"),
            None
        );
        let shown = show("pixiv").unwrap();
        assert_eq!(shown[0].0, "_refresh_token_cache-None");
        assert_eq!(shown[0].1, "\"TOKEN\"");
        assert_eq!(status().unwrap().counts[0], ("Total".into(), 2));
        assert_eq!(clear("EXP").unwrap(), 1);
        set(&pixiv, "NEW").unwrap();
        assert_eq!(get(&pixiv).as_deref(), Some("NEW"));
        assert_eq!(clear("PIXIV").unwrap(), 1);
        assert_eq!(get(&pixiv), None);
    }
}
