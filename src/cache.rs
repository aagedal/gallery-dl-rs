//! Persistent credential cache shared with upstream gallery-dl (cache.py).
//!
//! Upstream keeps `extractor.cache(...)` results in an SQLite table
//! `data(key TEXT PRIMARY KEY, value TEXT, expires INTEGER)`, keyed
//! `"{module}.{function}-{argument}"` (for example
//! `gallery_dl.extractor.deviantart._refresh_token_cache-#5388`) with pickled
//! values and `expires` as Unix seconds, 0 meaning never. This module reads
//! and writes the same layout and default file, so tokens obtained here (or
//! by `oauth:SITE` in Python gallery-dl) work for both. Values are limited to
//! strings, numbers, tuples/lists and string-keyed dicts; other pickles read
//! as absent. Values are never logged, and new files are created 0600.
use crate::{Result, error};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params, types::ValueRef};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

/// Where the cache lives: upstream's `cache.file` setting.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Location {
    /// Upstream's default `cache.sqlite3` in the user cache directory.
    #[default]
    Default,
    /// `cache.file: null` disables the persistent cache.
    Disabled,
    File(PathBuf),
}

impl Location {
    /// Resolve to a cache handle; the file is opened on first use.
    pub fn cache(&self) -> Option<Cache> {
        match self {
            Self::Disabled => None,
            Self::File(path) => Some(Cache::new(path.clone(), false)),
            Self::Default => default_path().map(|path| Cache::new(path, true)),
        }
    }
}

/// Upstream `cache.path()`: `%APPDATA%\gallery-dl\cache.sqlite3` on Windows,
/// else `$XDG_CACHE_HOME/gallery-dl/cache.sqlite3` (default `~/.cache`).
pub fn default_path() -> Option<PathBuf> {
    let var = |name| {
        std::env::var_os(name)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    };
    let home = || var(if cfg!(windows) { "USERPROFILE" } else { "HOME" });
    let directory = if cfg!(windows) {
        var("APPDATA").or_else(home)?
    } else {
        var("XDG_CACHE_HOME").or_else(|| home().map(|home| home.join(".cache")))?
    };
    Some(directory.join("gallery-dl").join("cache.sqlite3"))
}

/// Upstream's key for `extractor.cache(func, arg)` in `gallery_dl.{module}`.
pub fn key(module: &str, function: &str, argument: &str) -> String {
    format!("gallery_dl.{module}.{function}-{argument}")
}

/// Python's `repr()` of a tuple of strings, as used in tuple-valued keys
/// such as upstream's `_token_cache-('tumblr', 'KEY')`.
pub fn tuple_repr(items: &[&str]) -> String {
    let items: Vec<String> = items.iter().map(|item| str_repr(item)).collect();
    match items.as_slice() {
        [one] => format!("({one},)"),
        _ => format!("({})", items.join(", ")),
    }
}

fn str_repr(value: &str) -> String {
    let quote = if value.contains('\'') && !value.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(value.len() + 2);
    out.push(quote);
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// A lazily opened cache database; clones share one connection.
#[derive(Clone)]
pub struct Cache {
    inner: Arc<Inner>,
}

struct Inner {
    path: PathBuf,
    /// Upstream creates its default directory, but not that of `cache.file`.
    create_parent: bool,
    connection: Mutex<Option<Connection>>,
}

impl std::fmt::Debug for Cache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Cache { .. }")
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |now| now.as_secs() as i64)
}

impl Cache {
    fn new(path: PathBuf, create_parent: bool) -> Self {
        Self {
            inner: Arc::new(Inner {
                path,
                create_parent,
                connection: Mutex::new(None),
            }),
        }
    }

    /// A cache at an explicit path, whose directory must exist.
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self::new(path.into(), false)
    }

    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// Check that the database can be opened (upstream `cache.database()`).
    pub async fn available(&self) -> bool {
        self.with_connection(|_| Ok(())).await.is_ok()
    }

    /// The value at `key`, or None when missing, expired or not representable.
    pub async fn get(&self, key: &str) -> Result<Option<Value>> {
        let key = key.to_owned();
        self.with_connection(move |connection| {
            let row = connection
                .query_row(
                    "SELECT value, expires FROM data WHERE key=? LIMIT 1",
                    [key],
                    |row| {
                        let value = match row.get_ref(0)? {
                            ValueRef::Blob(bytes) | ValueRef::Text(bytes) => Some(bytes.to_vec()),
                            _ => None,
                        };
                        Ok((value, row.get::<_, Option<i64>>(1)?.unwrap_or(0)))
                    },
                )
                .optional()?;
            Ok(match row {
                Some((Some(value), expires)) if expires == 0 || expires > now() => {
                    pickle::loads(&value)
                }
                _ => None,
            })
        })
        .await
    }

    /// A string value, as stored for refresh and access tokens.
    pub async fn get_str(&self, key: &str) -> Result<Option<String>> {
        Ok(match self.get(key).await? {
            Some(Value::String(value)) => Some(value),
            _ => None,
        })
    }

    /// Store `value` at `key`, expiring after `lifetime` (never when None).
    pub async fn set(&self, key: &str, value: &Value, lifetime: Option<Duration>) -> Result<()> {
        let key = key.to_owned();
        let bytes = pickle::dumps(value).ok_or_else(|| error("value cannot be cached"))?;
        let expires = lifetime.map_or(0, |lifetime| now() + lifetime.as_secs() as i64);
        self.with_connection(move |connection| {
            connection.execute(
                "INSERT OR REPLACE INTO data VALUES (?,?,?)",
                params![key, bytes, expires],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn delete(&self, key: &str) -> Result<()> {
        let key = key.to_owned();
        self.with_connection(move |connection| {
            connection.execute("DELETE FROM data WHERE key=?", [key])?;
            Ok(())
        })
        .await
    }

    async fn with_connection<T, F>(&self, operation: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
    {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let mut slot = inner
                .connection
                .lock()
                .map_err(|_| error("cache connection lock poisoned"))?;
            if slot.is_none() {
                *slot = Some(
                    open(&inner.path, inner.create_parent)
                        .map_err(|_| error("cannot open the cache file"))?,
                );
            }
            operation(slot.as_ref().expect("opened above"))
                .map_err(|_| error("cache database operation failed"))
        })
        .await?
    }
}

fn open(path: &Path, create_parent: bool) -> Result<Connection> {
    if create_parent && let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Restrict new files to the current user before SQLite opens them.
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    drop(options.open(path)?);
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.busy_timeout(Duration::from_secs(60))?;
    connection.execute(
        "CREATE TABLE IF NOT EXISTS data (key TEXT PRIMARY KEY, value TEXT, expires INTEGER)",
        [],
    )?;
    Ok(connection)
}

/// The subset of Python's pickle format used for cached credentials.
pub(crate) mod pickle {
    use serde_json::{Map, Number, Value};

    const MAX_INPUT: usize = 1024 * 1024;

    /// Protocol 4 without framing or memoization, readable by any Python 3.
    /// Arrays become tuples, as upstream stores token pairs.
    pub fn dumps(value: &Value) -> Option<Vec<u8>> {
        let mut out = vec![0x80, 4];
        write(value, &mut out)?;
        out.push(b'.');
        Some(out)
    }

    fn write(value: &Value, out: &mut Vec<u8>) -> Option<()> {
        match value {
            Value::Null => out.push(b'N'),
            Value::Bool(true) => out.push(0x88),
            Value::Bool(false) => out.push(0x89),
            Value::Number(number) => {
                if let Some(n) = number.as_i64() {
                    if let Ok(n) = i32::try_from(n) {
                        out.push(b'J');
                        out.extend_from_slice(&n.to_le_bytes());
                    } else {
                        out.extend_from_slice(&[0x8a, 8]);
                        out.extend_from_slice(&n.to_le_bytes());
                    }
                } else {
                    out.push(b'G');
                    out.extend_from_slice(&number.as_f64()?.to_be_bytes());
                }
            }
            Value::String(text) => {
                if text.len() < 256 {
                    out.extend_from_slice(&[0x8c, text.len() as u8]);
                } else {
                    out.push(b'X');
                    out.extend_from_slice(&u32::try_from(text.len()).ok()?.to_le_bytes());
                }
                out.extend_from_slice(text.as_bytes());
            }
            Value::Array(items) => match items.len() {
                0 => out.push(b')'),
                n @ 1..=3 => {
                    for item in items {
                        write(item, out)?;
                    }
                    out.push([0x85, 0x86, 0x87][n - 1]);
                }
                _ => {
                    out.push(b'(');
                    for item in items {
                        write(item, out)?;
                    }
                    out.push(b't');
                }
            },
            Value::Object(fields) => {
                out.push(b'}');
                if !fields.is_empty() {
                    out.push(b'(');
                    for (key, value) in fields {
                        write(&Value::String(key.clone()), out)?;
                        write(value, out)?;
                    }
                    out.push(b'u');
                }
            }
        }
        Some(())
    }

    enum Item {
        Mark,
        Value(Value),
    }

    struct Reader<'a> {
        data: &'a [u8],
        position: usize,
    }

    impl<'a> Reader<'a> {
        fn take(&mut self, count: usize) -> Option<&'a [u8]> {
            let end = self.position.checked_add(count)?;
            let bytes = self.data.get(self.position..end)?;
            self.position = end;
            Some(bytes)
        }
        fn byte(&mut self) -> Option<u8> {
            self.take(1).map(|b| b[0])
        }
        fn u32(&mut self) -> Option<usize> {
            Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?) as usize)
        }
        fn u64(&mut self) -> Option<usize> {
            usize::try_from(u64::from_le_bytes(self.take(8)?.try_into().ok()?)).ok()
        }
        fn text(&mut self, length: usize) -> Option<Value> {
            Some(Value::String(
                std::str::from_utf8(self.take(length)?).ok()?.to_owned(),
            ))
        }
    }

    fn pop_value(stack: &mut Vec<Item>) -> Option<Value> {
        match stack.pop()? {
            Item::Value(value) => Some(value),
            Item::Mark => None,
        }
    }

    /// Values since the topmost mark, which is removed.
    fn pop_mark(stack: &mut Vec<Item>) -> Option<Vec<Value>> {
        let mark = stack.iter().rposition(|item| matches!(item, Item::Mark))?;
        let items = stack.split_off(mark + 1);
        stack.pop();
        items
            .into_iter()
            .map(|item| match item {
                Item::Value(value) => Some(value),
                Item::Mark => None,
            })
            .collect()
    }

    fn set_items(stack: &mut [Item], pairs: Vec<Value>) -> Option<()> {
        let Some(Item::Value(Value::Object(map))) = stack.last_mut() else {
            return None;
        };
        let mut pairs = pairs.into_iter();
        while let Some(key) = pairs.next() {
            let Value::String(key) = key else { return None };
            map.insert(key, pairs.next()?);
        }
        Some(())
    }

    fn append(stack: &mut [Item], items: Vec<Value>) -> Option<()> {
        let Some(Item::Value(Value::Array(list))) = stack.last_mut() else {
            return None;
        };
        list.extend(items);
        Some(())
    }

    fn int(bytes: &[u8]) -> Option<Value> {
        if bytes.len() > 8 {
            return None;
        }
        let negative = bytes.last().is_some_and(|b| b & 0x80 != 0);
        let mut buffer = if negative { [0xff; 8] } else { [0; 8] };
        buffer[..bytes.len()].copy_from_slice(bytes);
        Some(Value::from(i64::from_le_bytes(buffer)))
    }

    /// Decode a pickle of plain data; anything else (objects, bytes,
    /// recursion) yields None. Tuples and lists become arrays.
    pub fn loads(data: &[u8]) -> Option<Value> {
        if data.len() > MAX_INPUT {
            return None;
        }
        let mut reader = Reader { data, position: 0 };
        let mut stack: Vec<Item> = Vec::new();
        let mut memo: std::collections::HashMap<usize, Value> = Default::default();
        loop {
            let value = match reader.byte()? {
                0x80 => {
                    reader.byte()?;
                    continue;
                }
                0x95 => {
                    reader.u64()?;
                    continue;
                }
                b'.' => {
                    let value = pop_value(&mut stack)?;
                    return stack.is_empty().then_some(value);
                }
                b'(' => {
                    stack.push(Item::Mark);
                    continue;
                }
                b'N' => Value::Null,
                0x88 => Value::Bool(true),
                0x89 => Value::Bool(false),
                b'J' => Value::from(i32::from_le_bytes(reader.take(4)?.try_into().ok()?)),
                b'K' => Value::from(reader.byte()?),
                b'M' => Value::from(u16::from_le_bytes(reader.take(2)?.try_into().ok()?)),
                0x8a => {
                    let length = reader.byte()? as usize;
                    int(reader.take(length)?)?
                }
                b'G' => Value::Number(Number::from_f64(f64::from_be_bytes(
                    reader.take(8)?.try_into().ok()?,
                ))?),
                0x8c => {
                    let length = reader.byte()? as usize;
                    reader.text(length)?
                }
                b'X' => {
                    let length = reader.u32()?;
                    reader.text(length)?
                }
                0x8d => {
                    let length = reader.u64()?;
                    reader.text(length)?
                }
                b')' => Value::Array(Vec::new()),
                n @ 0x85..=0x87 => {
                    let count = (n - 0x84) as usize;
                    let start = stack.len().checked_sub(count)?;
                    let items = stack
                        .split_off(start)
                        .into_iter()
                        .map(|item| match item {
                            Item::Value(value) => Some(value),
                            Item::Mark => None,
                        })
                        .collect::<Option<Vec<_>>>()?;
                    Value::Array(items)
                }
                b't' | b'l' => Value::Array(pop_mark(&mut stack)?),
                b']' => Value::Array(Vec::new()),
                b'}' => Value::Object(Map::new()),
                b'd' => {
                    let pairs = pop_mark(&mut stack)?;
                    stack.push(Item::Value(Value::Object(Map::new())));
                    set_items(&mut stack, pairs)?;
                    continue;
                }
                b'a' => {
                    let item = pop_value(&mut stack)?;
                    append(&mut stack, vec![item])?;
                    continue;
                }
                b'e' => {
                    let items = pop_mark(&mut stack)?;
                    append(&mut stack, items)?;
                    continue;
                }
                b's' => {
                    let value = pop_value(&mut stack)?;
                    let key = pop_value(&mut stack)?;
                    set_items(&mut stack, vec![key, value])?;
                    continue;
                }
                b'u' => {
                    let pairs = pop_mark(&mut stack)?;
                    set_items(&mut stack, pairs)?;
                    continue;
                }
                0x94 => {
                    let Some(Item::Value(top)) = stack.last() else {
                        return None;
                    };
                    memo.insert(memo.len(), top.clone());
                    continue;
                }
                op @ (b'q' | b'r') => {
                    let index = if op == b'q' {
                        reader.byte()? as usize
                    } else {
                        reader.u32()?
                    };
                    let Some(Item::Value(top)) = stack.last() else {
                        return None;
                    };
                    memo.insert(index, top.clone());
                    continue;
                }
                op @ (b'h' | b'j') => {
                    let index = if op == b'h' {
                        reader.byte()? as usize
                    } else {
                        reader.u32()?
                    };
                    memo.get(&index)?.clone()
                }
                _ => return None,
            };
            stack.push(Item::Value(value));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn keys_match_upstream_layout() {
        assert_eq!(
            key("extractor.deviantart", "_refresh_token_cache", "#5388"),
            "gallery_dl.extractor.deviantart._refresh_token_cache-#5388"
        );
        assert_eq!(
            tuple_repr(&["tumblr", "abc"]),
            "('tumblr', 'abc')",
            "Python repr of a tuple"
        );
        assert_eq!(tuple_repr(&["it's"]), "(\"it's\",)");
        assert_eq!(tuple_repr(&["a'\"\\\n"]), "('a\\'\"\\\\\\n',)");
    }

    #[test]
    fn pickles_round_trip_and_read_python_output() {
        for value in [
            json!("token"),
            json!("x".repeat(300)),
            json!(["token", "secret"]),
            json!(["a", "b", "c", "d"]),
            json!([]),
            json!({"client-id": "id", "n": 5, "big": 1_i64 << 40, "neg": -3,
                   "list": ["x"], "none": null, "flag": true, "f": 1.5}),
            json!({}),
        ] {
            assert_eq!(pickle::loads(&pickle::dumps(&value).unwrap()), Some(value));
        }
        // Python 3.12: pickle.dumps("abc"), pickle.dumps(("t", "s")),
        // pickle.dumps({"a": "b", "c": None}) and pickle.dumps("ab", 0).
        let python: [(&[u8], Value); 4] = [
            (
                b"\x80\x04\x95\x07\x00\x00\x00\x00\x00\x00\x00\x8c\x03abc\x94.",
                json!("abc"),
            ),
            (
                b"\x80\x04\x95\x0b\x00\x00\x00\x00\x00\x00\x00\x8c\x01t\x94\x8c\x01s\x94\x86\x94.",
                json!(["t", "s"]),
            ),
            (
                b"\x80\x04\x95\x12\x00\x00\x00\x00\x00\x00\x00}\x94(\x8c\x01a\x94\x8c\x01b\x94\x8c\x01c\x94Nu.",
                json!({"a": "b", "c": null}),
            ),
            (b"\x80\x04\x8c\x01a\x94h\x00\x86\x94.", json!(["a", "a"])),
        ];
        for (bytes, value) in python {
            assert_eq!(pickle::loads(bytes), Some(value));
        }
        // Objects, truncated input and trailing stack items are rejected.
        for bytes in [
            &b"\x80\x04\x8c\x08builtins\x8c\x03set\x93."[..],
            b"\x80\x04\x8c\x05ab",
            b"\x80\x04NN.",
            b"",
        ] {
            assert_eq!(pickle::loads(bytes), None);
        }
    }

    #[tokio::test]
    async fn values_round_trip_expire_and_delete() {
        let directory = tempfile::tempdir().unwrap();
        let cache = Cache::at(directory.path().join("cache.sqlite3"));
        assert_eq!(cache.get("missing").await.unwrap(), None);
        cache.set("k", &json!("v1"), None).await.unwrap();
        cache.set("k", &json!("v2"), None).await.unwrap();
        assert_eq!(cache.get_str("k").await.unwrap().as_deref(), Some("v2"));
        cache
            .set("pair", &json!(["t", "s"]), Some(Duration::from_secs(3600)))
            .await
            .unwrap();
        assert_eq!(cache.get("pair").await.unwrap(), Some(json!(["t", "s"])));
        // A non-string value is not a token.
        assert_eq!(cache.get_str("pair").await.unwrap(), None);
        cache.delete("k").await.unwrap();
        assert_eq!(cache.get("k").await.unwrap(), None);

        // Expired rows (Python-written, with TEXT values) read as absent.
        let path = cache.path().to_owned();
        tokio::task::spawn_blocking(move || {
            let connection = Connection::open(path).unwrap();
            connection
                .execute(
                    "INSERT INTO data VALUES ('old', ?, 1), ('text', ?, 0)",
                    params![pickle::dumps(&json!("stale")).unwrap(), "N."],
                )
                .unwrap();
        })
        .await
        .unwrap();
        assert_eq!(cache.get("old").await.unwrap(), None);
        assert_eq!(cache.get("text").await.unwrap(), Some(Value::Null));
        // A second handle on the same file sees the data.
        let other = Cache::at(cache.path());
        assert_eq!(
            other.get("pair").await.unwrap(),
            Some(json!(["t", "s"])),
            "shared file"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn new_files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let cache = Cache::at(directory.path().join("cache.sqlite3"));
        cache.set("k", &json!("v"), None).await.unwrap();
        let mode = std::fs::metadata(cache.path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[tokio::test]
    async fn disabled_and_unopenable_caches() {
        assert!(Location::Disabled.cache().is_none());
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("missing").join("cache.sqlite3");
        let cache = Location::File(path.clone()).cache().unwrap();
        assert!(!cache.available().await);
        let failure = cache.get("k").await.unwrap_err().to_string();
        assert_eq!(failure, "cannot open the cache file");
        assert!(!path.exists());
    }
}
