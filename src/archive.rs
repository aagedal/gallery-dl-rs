//! Persistent completed-download identities. Archive hits intentionally do not
//! inspect the filesystem: removing a saved file does not clear its history.
use crate::{
    Result,
    download::{self, DownloadClient, DownloadOptions, Outcome},
    error,
    extract::Media,
    naming::FilenameTemplate,
};
use rusqlite::{Connection, OpenFlags, params};
use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};

// A separate format from Python gallery-dl's extractor-specific archive keys.
const APPLICATION_ID: i64 = 0x47444c52;
const SCHEMA_VERSION: i64 = 1;

#[derive(Clone)]
pub struct Archive {
    connection: Arc<Mutex<Connection>>,
    pending: Arc<Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>>,
    /// An existing Python gallery-dl archive (`archive` table of `entry`
    /// keys built from each extractor's `archive_fmt`).
    upstream: bool,
    /// Upstream `archive-event`: record downloaded files (`file`/`after`)
    /// and files skipped because they exist (`skip`).
    record_files: bool,
    record_skips: bool,
    /// Upstream `archive-mode: memory`: keys written by [`Archive::finalize`].
    memory: Option<Arc<Mutex<std::collections::BTreeSet<String>>>>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ArchiveOutcome {
    Completed(Outcome),
    AlreadyArchived,
}

impl Archive {
    /// Open/create an archive. Its parent directory must already exist.
    /// Database work runs on the blocking pool, including lock waits.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_owned();
        // Like upstream, create missing parent directories.
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty())
            && !parent.exists()
        {
            tokio::fs::create_dir_all(parent).await?;
        }
        let (connection, upstream) =
            tokio::task::spawn_blocking(move || open_connection(&path)).await??;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            pending: Arc::default(),
            upstream,
            record_files: true,
            // Native archives also record existing files; upstream records
            // only downloads unless `archive-event` includes `skip`.
            record_skips: !upstream,
            memory: None,
        })
    }

    /// Apply upstream `archive-event` (a list or comma-separated events;
    /// default `file`) and `archive-mode` (`file` or `memory`).
    pub fn with_options(
        mut self,
        events: Option<&serde_json::Value>,
        mode: Option<&serde_json::Value>,
    ) -> Self {
        if let Some(events) = events.filter(|v| !v.is_null()) {
            let events: Vec<String> = match events {
                serde_json::Value::String(text) => {
                    text.split(',').map(|e| e.trim().to_owned()).collect()
                }
                serde_json::Value::Array(list) => list
                    .iter()
                    .filter_map(|e| e.as_str().map(str::to_owned))
                    .collect(),
                _ => Vec::new(),
            };
            let has = |name: &str| events.iter().any(|e| e == name);
            self.record_files = has("file") || has("after");
            self.record_skips = has("skip");
        }
        if mode.and_then(serde_json::Value::as_str) == Some("memory") {
            self.memory = Some(Arc::default());
        }
        self
    }

    /// Write keys held back by `archive-mode: memory`.
    pub async fn finalize(&self) -> Result<()> {
        let Some(memory) = &self.memory else {
            return Ok(());
        };
        let keys = std::mem::take(&mut *memory.lock().map_err(|_| error("archive lock poisoned"))?);
        if keys.is_empty() {
            return Ok(());
        }
        let statement = if self.upstream {
            "INSERT OR IGNORE INTO archive (entry) VALUES (?1)"
        } else {
            "INSERT OR IGNORE INTO completed_media (identity) VALUES (?1)"
        };
        self.with_connection(move |connection| {
            let transaction = connection.transaction()?;
            for key in keys {
                transaction.execute(statement, params![key])?;
            }
            transaction.commit()?;
            Ok(())
        })
        .await
    }

    /// Whether this is a Python gallery-dl archive.
    pub fn is_upstream(&self) -> bool {
        self.upstream
    }

    /// The archive key of a file: the native identity, or for upstream
    /// archives the category followed by the rendered `archive_fmt`.
    pub fn key(&self, media: &Media) -> Option<String> {
        if self.upstream {
            upstream_key(media)
        } else {
            media.archive_key()
        }
    }

    pub async fn contains(&self, identity: &str) -> Result<bool> {
        if let Some(memory) = &self.memory
            && memory
                .lock()
                .map_err(|_| error("archive lock poisoned"))?
                .contains(identity)
        {
            return Ok(true);
        }
        let identity = identity.to_owned();
        let query = if self.upstream {
            "SELECT EXISTS(SELECT 1 FROM archive WHERE entry = ?1)"
        } else {
            "SELECT EXISTS(SELECT 1 FROM completed_media WHERE identity = ?1)"
        };
        self.with_connection(move |connection| {
            Ok(connection.query_row(query, [identity], |row| row.get(0))?)
        })
        .await
    }

    /// Record only after all requested output has been published successfully.
    /// Duplicate records are harmless, including concurrent processes.
    pub async fn record(&self, identity: &str) -> Result<()> {
        if let Some(memory) = &self.memory {
            memory
                .lock()
                .map_err(|_| error("archive lock poisoned"))?
                .insert(identity.to_owned());
            return Ok(());
        }
        let identity = identity.to_owned();
        let statement = if self.upstream {
            "INSERT OR IGNORE INTO archive (entry) VALUES (?1)"
        } else {
            "INSERT OR IGNORE INTO completed_media (identity) VALUES (?1)"
        };
        self.with_connection(move |connection| {
            connection.execute(statement, params![identity])?;
            Ok(())
        })
        .await
    }

    /// Check stable history, download, then record successful publication. Media
    /// without a reliable identity is downloaded normally and never archived.
    /// Existing destination files count as completed, after optional metadata repair.
    pub async fn download(
        &self,
        client: &DownloadClient,
        root: &Path,
        media: &Media,
        options: DownloadOptions<'_>,
    ) -> Result<ArchiveOutcome> {
        self.download_with_template(client, root, media, options, None)
            .await
    }

    pub async fn download_with_template(
        &self,
        client: &DownloadClient,
        root: &Path,
        media: &Media,
        options: DownloadOptions<'_>,
        template: Option<&FilenameTemplate>,
    ) -> Result<ArchiveOutcome> {
        let effective_media = client.effective_media(media);
        let media = effective_media.as_ref();
        let resolved_filename = crate::naming::effective_filename(media, template);
        let template = resolved_filename.as_ref();
        let source_booru = media
            .metadata
            .as_ref()
            .is_some_and(|metadata| metadata.booru_keywords.is_some());
        let empty_processors = (source_booru && options.postprocessors.is_none())
            .then(|| crate::postprocess::Postprocessors::new(Vec::new()));
        let options = DownloadOptions {
            postprocessors: options.postprocessors.or(empty_processors.as_ref()),
            ..options
        };
        if let Some(pp) = options.postprocessors
            && media
                .metadata
                .as_ref()
                .is_some_and(|m| m.ugoira_frame_index.is_some())
            && !pp.selected(media).is_empty()
        {
            pp.frames.register(
                media,
                &download::destination_with_templates(root, media, template, options.directory)?,
            )?;
        }
        let mut generic =
            if source_booru || options.postprocessors.is_some_and(|pp| pp.has_generic()) {
                Some(download::processing::prepare(client, root, media, options, template).await?)
            } else {
                None
            };
        let prepared = options.postprocessors.map(|pp| pp.prepared_media(media));
        let identity = self.key(
            generic
                .as_ref()
                .map(|job| &job.media)
                .or(prepared.as_ref())
                .unwrap_or(media),
        );
        // Serialize the same stable identity within this instance, without
        // suppressing a refreshed URL if a previous request failed. Weak entries
        // retain no per-item state once pending downloads have finished.
        let _guard = match &identity {
            Some(identity) => Some(self.lock_identity(identity).await?),
            None => None,
        };
        // Upstream `skip: false` also disables archive checks.
        if let Some(identity) = &identity
            && !client.overwrite
            && self.contains(identity).await?
        {
            if let Some(generic) = &mut generic
                && let Some(outcome) =
                    download::processing::skip(client, root, media, options, template, generic)
                        .await?
            {
                return Ok(ArchiveOutcome::Completed(outcome));
            }
            // An earlier archived frame can become ready after a newly downloaded
            // final frame when downloads run concurrently. Finish collection without
            // making another HTTP request or changing the archived frame's identity.
            if options
                .postprocessors
                .is_some_and(|pp| !pp.selected(media).is_empty())
                && media
                    .metadata
                    .as_ref()
                    .is_some_and(|m| m.ugoira_frame_index.is_some())
            {
                let path =
                    download::destination_with_templates(root, media, template, options.directory)?;
                if crate::postprocess::regular_file(&path)?
                    && let Some((path, _)) = download::complete_original(
                        client,
                        root,
                        media,
                        &Outcome::Skipped(path),
                        options,
                        template,
                    )
                    .await?
                {
                    return Ok(ArchiveOutcome::Completed(Outcome::Downloaded(path)));
                }
            }
            return Ok(ArchiveOutcome::AlreadyArchived);
        }
        let outcome = if let Some(generic) = generic {
            download::processing::download(client, root, media, options, template, generic).await?
        } else {
            download::download_with_template(client, root, media, options, template).await?
        };
        let wanted = match outcome {
            Outcome::Downloaded(_) => self.record_files,
            Outcome::Skipped(_) => self.record_skips,
            Outcome::Filtered(_) => false,
        };
        if let Some(identity) = &identity
            && wanted
            && let Some(path) = outcome.saved_path()
        {
            self.record(identity).await.map_err(|e| {
                error(format!(
                    "media is saved at {}; download archive update failed: {e}",
                    path.display()
                ))
            })?;
        }
        Ok(ArchiveOutcome::Completed(outcome))
    }

    async fn lock_identity(&self, identity: &str) -> Result<tokio::sync::OwnedMutexGuard<()>> {
        let lock = {
            let mut pending = self
                .pending
                .lock()
                .map_err(|_| error("download archive lock poisoned"))?;
            pending.retain(|_, lock| lock.strong_count() > 0);
            match pending.get(identity).and_then(Weak::upgrade) {
                Some(lock) => lock,
                None => {
                    let lock = Arc::new(tokio::sync::Mutex::new(()));
                    pending.insert(identity.to_owned(), Arc::downgrade(&lock));
                    lock
                }
            }
        };
        Ok(lock.lock_owned().await)
    }

    async fn with_connection<T, F>(&self, operation: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let connection = self.connection.clone();
        tokio::task::spawn_blocking(move || {
            let mut connection = connection
                .lock()
                .map_err(|_| error("download archive connection lock poisoned"))?;
            operation(&mut connection)
        })
        .await?
    }
}

/// Upstream `DownloadArchive` key: `archive-prefix` (the category) and the
/// extractor's `archive_fmt` rendered with the file's keywords.
pub fn upstream_key(media: &Media) -> Option<String> {
    let descriptor = crate::source_archive::for_media(media)?;
    if descriptor.format.is_empty() {
        return None;
    }
    let keywords = match media
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.site_keywords.as_ref())
    {
        Some(keywords) => keywords.fields.clone(),
        None => crate::postprocess::context(media),
    };
    let key = crate::postprocess::template::Template::parse(&descriptor.format)
        .ok()?
        .render(&keywords)
        .ok()?;
    Some(format!("{}{key}", descriptor.category))
}

/// Open an archive, creating a native one in a new or empty database.
/// Returns whether it is a Python gallery-dl archive.
fn open_connection(path: &Path) -> Result<(Connection, bool)> {
    let mut connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.busy_timeout(Duration::from_secs(5))?;
    // Serialize initialization between processes and validate before touching an
    // existing database. Never turn another application's database into ours.
    let transaction =
        connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let application: i64 = transaction.query_row("PRAGMA application_id", [], |row| row.get(0))?;
    let version: i64 = transaction.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let tables: i64 = transaction.query_row(
        "SELECT count(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'",
        [],
        |row| row.get(0),
    )?;
    if application == 0
        && transaction
            .prepare("SELECT entry FROM archive LIMIT 0")
            .is_ok()
    {
        transaction.commit()?;
        return Ok((connection, true));
    }
    if application == 0 && version == 0 && tables == 0 {
        transaction.execute_batch(&format!(
            "CREATE TABLE completed_media (
                identity TEXT PRIMARY KEY NOT NULL,
                completed_at INTEGER NOT NULL DEFAULT (unixepoch())
            ) WITHOUT ROWID;
            PRAGMA application_id = {APPLICATION_ID};
            PRAGMA user_version = {SCHEMA_VERSION};"
        ))?;
    } else if application != APPLICATION_ID || version != SCHEMA_VERSION {
        return Err(error(
            "unsupported download archive format; use a gallery-dl-rs or gallery-dl archive",
        ));
    }
    // Check that even an otherwise correctly marked database has our columns.
    transaction.prepare("SELECT identity, completed_at FROM completed_media LIMIT 0")?;
    transaction.commit()?;
    Ok((connection, false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::{MediaJob, PostMetadata};
    use crate::sites::{Route, SiteKeywords};

    static ROUTE: Route = Route::new("post", "{id}.{extension}", &[], "{id}_{num}");

    #[tokio::test]
    async fn reads_and_writes_python_gallery_dl_archives() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("archive.sqlite3");
        Connection::open(&path)
            .unwrap()
            .execute_batch(
                "CREATE TABLE archive (entry TEXT PRIMARY KEY) WITHOUT ROWID;
                 INSERT INTO archive VALUES ('bunkr42_1');",
            )
            .unwrap();
        let archive = Archive::open(&path).await.unwrap();
        assert!(archive.is_upstream());
        let media = |id: i64| Media {
            url: url::Url::parse("https://example.com/a.jpg").unwrap(),
            job: MediaJob::DirectHttp,
            category: "bunkr".into(),
            filename: "a.jpg".into(),
            metadata: Some(PostMetadata {
                site_keywords: Some(Arc::new(SiteKeywords {
                    route: &ROUTE,
                    fields: serde_json::json!({"id": id, "num": 1})
                        .as_object()
                        .unwrap()
                        .clone(),
                })),
                ..PostMetadata::default()
            }),
            headers: Default::default(),
            handoff: Default::default(),
        };
        assert_eq!(archive.key(&media(42)).as_deref(), Some("bunkr42_1"));
        assert!(archive.contains("bunkr42_1").await.unwrap());
        let key = archive.key(&media(7)).unwrap();
        assert!(!archive.contains(&key).await.unwrap());
        archive.record(&key).await.unwrap();
        assert!(archive.contains("bunkr7_1").await.unwrap());

        // New archives keep the native schema.
        let native = Archive::open(dir.path().join("new.sqlite3")).await.unwrap();
        assert!(!native.is_upstream());
    }
}
