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
        let connection = tokio::task::spawn_blocking(move || open_connection(&path)).await??;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            pending: Arc::default(),
        })
    }

    pub async fn contains(&self, identity: &str) -> Result<bool> {
        let identity = identity.to_owned();
        self.with_connection(move |connection| {
            Ok(connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM completed_media WHERE identity = ?1)",
                [identity],
                |row| row.get(0),
            )?)
        })
        .await
    }

    /// Record only after all requested output has been published successfully.
    /// Duplicate records are harmless, including concurrent processes.
    pub async fn record(&self, identity: &str) -> Result<()> {
        let identity = identity.to_owned();
        self.with_connection(move |connection| {
            connection.execute(
                "INSERT OR IGNORE INTO completed_media (identity) VALUES (?1)",
                params![identity],
            )?;
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
        let mut generic = if options.postprocessors.is_some_and(|pp| pp.has_generic()) {
            Some(download::processing::prepare(client, root, media, options, template).await?)
        } else {
            None
        };
        let prepared = options.postprocessors.map(|pp| pp.prepared_media(media));
        let identity = generic
            .as_ref()
            .map(|job| &job.media)
            .or(prepared.as_ref())
            .unwrap_or(media)
            .archive_key();
        // Serialize the same stable identity within this instance, without
        // suppressing a refreshed URL if a previous request failed. Weak entries
        // retain no per-item state once pending downloads have finished.
        let _guard = match &identity {
            Some(identity) => Some(self.lock_identity(identity).await?),
            None => None,
        };
        if let Some(identity) = &identity
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
        if let Some(identity) = &identity
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

fn open_connection(path: &Path) -> Result<Connection> {
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
            "unsupported download archive format; use a gallery-dl-rs archive (Python gallery-dl archives are not interchangeable)",
        ));
    }
    // Check that even an otherwise correctly marked database has our columns.
    transaction.prepare("SELECT identity, completed_at FROM completed_media LIMIT 0")?;
    transaction.commit()?;
    Ok(connection)
}
