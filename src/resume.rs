//! Private, locked, crash-persistent HTTP download checkpoints.
use crate::{Result, download::Outcome, error};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

const MAX_STATE: u64 = 16 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) enum Validator {
    Etag(String),
    Modified(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Checkpoint {
    pub validator: Validator,
    pub total: Option<u64>,
    /// Digest of the effective response URL, never the URL itself.
    pub target: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    version: u8,
    checkpoint: Checkpoint,
}

pub(crate) struct Partial {
    part: PathBuf,
    state: PathBuf,
    lock: Arc<CheckpointLock>,
    operations: Arc<tokio::sync::Mutex<()>>,
    checkpoint: Option<Checkpoint>,
}

struct CheckpointLock(File);

impl Drop for CheckpointLock {
    fn drop(&mut self) {
        // Closing our descriptor alone leaves flock held by descriptors that a
        // concurrent process creation inherited until exec. Release explicitly
        // when the last owner (including non-cancellable disk work) finishes.
        let _ = self.0.unlock();
    }
}

impl Partial {
    pub(crate) async fn open(destination: &Path, request_identity: &str) -> Result<Self> {
        let destination = destination.to_owned();
        let request_identity = request_identity.to_owned();
        tokio::task::spawn_blocking(move || {
            let parent = destination
                .parent()
                .ok_or_else(|| error("missing output directory"))?;
            fs::create_dir_all(parent)?;
            let directory = parent.join(".gallery-dl-rs");
            match fs::create_dir(&directory) {
                Ok(()) => (),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let metadata = fs::symlink_metadata(&directory)?;
                    if !metadata.is_dir() || metadata.file_type().is_symlink() {
                        return Err(error("resume directory is not a regular directory"));
                    }
                }
                Err(e) => return Err(e.into()),
            }
            let basename = destination
                .file_name()
                .ok_or_else(|| error("missing output filename"))?;
            // Length-delimited fields prevent ambiguous concatenations; no secrets
            // or source URLs occur in persistent names or state.
            let mut digest = Sha256::new();
            for field in [request_identity.as_bytes(), basename.as_encoded_bytes()] {
                digest.update((field.len() as u64).to_le_bytes());
                digest.update(field);
            }
            let key = format!("{:x}", digest.finalize());
            let lock = open_regular(&directory.join(format!("{key}.lock")))?;
            lock.try_lock().map_err(lock_error)?;
            let lock = Arc::new(CheckpointLock(lock));
            let part = directory.join(format!("{key}.part"));
            let state = directory.join(format!("{key}.json"));
            open_regular(&part)?;
            ensure_regular_if_exists(&state)?;
            let checkpoint = read_checkpoint(&state)?;
            Ok(Self {
                part,
                state,
                lock,
                operations: Arc::new(tokio::sync::Mutex::new(())),
                checkpoint,
            })
        })
        .await?
    }

    pub(crate) fn checkpoint(&self) -> Option<&Checkpoint> {
        self.checkpoint.as_ref()
    }

    /// Remove a failed candidate's bytes before another representation is tried.
    /// Keep the lock inode/reservation so concurrent writers cannot reuse it.
    pub(crate) async fn discard(&mut self) -> Result<()> {
        self.reset(None).await?;
        let part = self.part.clone();
        let lock = self.lock.clone();
        let operation = self.operations.clone().lock_owned().await;
        tokio::task::spawn_blocking(move || -> Result<()> {
            let _lock = lock;
            let _operation = operation;
            ensure_regular_if_exists(&part)?;
            match fs::remove_file(&part) {
                Ok(()) => sync_parent(&part),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e.into()),
            }
        })
        .await?
    }

    pub(crate) async fn reset(&mut self, checkpoint: Option<Checkpoint>) -> Result<()> {
        let (part, state) = (self.part.clone(), self.state.clone());
        let lock = self.lock.clone();
        let operation = self.operations.clone().lock_owned().await;
        let new_checkpoint = checkpoint.clone();
        self.checkpoint = None;
        tokio::task::spawn_blocking(move || -> Result<()> {
            let _lock = lock;
            let _operation = operation;
            ensure_regular_if_exists(&state)?;
            match fs::remove_file(&state) {
                Ok(()) => sync_parent(&state)?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => return Err(e.into()),
            }
            // Never let old validators describe bytes from a new response, even
            // if execution stops halfway through reset.
            let file = open_regular(&part)?;
            file.set_len(0)?;
            file.sync_all()?;
            if let Some(checkpoint) = new_checkpoint {
                write_checkpoint(&state, checkpoint)?;
            }
            Ok(())
        })
        .await??;
        self.checkpoint = checkpoint;
        Ok(())
    }

    /// Persist a size learned from Content-Range without discarding saved bytes.
    pub(crate) async fn set_total(&mut self, total: u64) -> Result<()> {
        let mut checkpoint = self
            .checkpoint
            .clone()
            .ok_or_else(|| error("cannot update a missing resume checkpoint"))?;
        if checkpoint.total.is_some_and(|previous| previous != total) {
            return Err(error("resume total conflicts with the saved checkpoint"));
        }
        if self.len().await? > total {
            return Err(error("resume partial exceeds the response total"));
        }
        checkpoint.total = Some(total);
        let (part, state) = (self.part.clone(), self.state.clone());
        let lock = self.lock.clone();
        let operation = self.operations.clone().lock_owned().await;
        // Once an update is queued, a cancelled caller must still enforce the
        // newly learned bound on subsequent responses using this Partial.
        self.checkpoint = Some(checkpoint.clone());
        tokio::task::spawn_blocking(move || -> Result<()> {
            let _lock = lock;
            let _operation = operation;
            ensure_regular_if_exists(&part)?;
            if fs::metadata(&part)?.len() > total {
                return Err(error("resume partial exceeds the response total"));
            }
            write_checkpoint(&state, checkpoint)
        })
        .await?
    }

    pub(crate) async fn append(&mut self, bytes: &[u8]) -> Result<()> {
        let bytes = bytes.to_vec();
        let part = self.part.clone();
        let lock = self.lock.clone();
        let operation = self.operations.clone().lock_owned().await;
        tokio::task::spawn_blocking(move || -> Result<()> {
            let _lock = lock;
            let _operation = operation;
            ensure_regular_if_exists(&part)?;
            let mut file = OpenOptions::new().append(true).open(&part)?;
            if !file.metadata()?.is_file() {
                return Err(error("resume partial is not a regular file"));
            }
            file.write_all(&bytes)?;
            file.sync_data()?;
            Ok(())
        })
        .await?
    }

    pub(crate) async fn len(&self) -> Result<u64> {
        let part = self.part.clone();
        let lock = self.lock.clone();
        let operation = self.operations.clone().lock_owned().await;
        tokio::task::spawn_blocking(move || -> Result<u64> {
            let _lock = lock;
            let _operation = operation;
            ensure_regular_if_exists(&part)?;
            Ok(fs::metadata(part)?.len())
        })
        .await?
    }

    pub(crate) async fn publish(&mut self, destination: &Path) -> Result<Outcome> {
        let destination = destination.to_owned();
        let (part, state) = (self.part.clone(), self.state.clone());
        let lock = self.lock.clone();
        let operation = self.operations.clone().lock_owned().await;
        let outcome = tokio::task::spawn_blocking(move || -> Result<Outcome> {
            let _lock = lock;
            let _operation = operation;
            ensure_regular_if_exists(&part)?;
            ensure_regular_if_exists(&state)?;
            let file = OpenOptions::new().read(true).write(true).open(&part)?;
            if !file.metadata()?.is_file() {
                return Err(error("resume partial is not a regular file"));
            }
            file.sync_all()?;
            let outcome = match fs::hard_link(&part, &destination) {
                Ok(()) => {
                    sync_parent(&destination)?;
                    Outcome::Downloaded(destination)
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    ensure_regular_if_exists(&destination)?;
                    Outcome::Skipped(destination)
                }
                Err(e) => return Err(e.into()),
            };
            // A persistent lock inode is deliberately never unlinked.
            fs::remove_file(part)?;
            match fs::remove_file(&state) {
                Ok(()) => (),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => return Err(e.into()),
            }
            sync_parent(&state)?;
            Ok(outcome)
        })
        .await??;
        self.checkpoint = None;
        Ok(outcome)
    }
}

fn lock_error(failure: fs::TryLockError) -> crate::Error {
    match failure {
        fs::TryLockError::WouldBlock => error("another download owns this resume checkpoint"),
        fs::TryLockError::Error(failure) => error(format!(
            "resume checkpoint lock failed ({:?}, OS code {:?})",
            failure.kind(),
            failure.raw_os_error()
        )),
    }
}

fn ensure_regular_if_exists(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(error("resume path is not a regular file")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

fn open_regular(path: &Path) -> Result<File> {
    match OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(file) => Ok(file),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            ensure_regular_if_exists(path)?;
            let file = OpenOptions::new().read(true).write(true).open(path)?;
            if !file.metadata()?.is_file() {
                return Err(error("resume path is not a regular file"));
            }
            Ok(file)
        }
        Err(e) => Err(e.into()),
    }
}

fn write_checkpoint(state: &Path, checkpoint: Checkpoint) -> Result<()> {
    let bytes = serde_json::to_vec(&State {
        version: 1,
        checkpoint,
    })?;
    if bytes.len() as u64 > MAX_STATE {
        return Err(error("resume checkpoint is too large"));
    }
    let parent = state
        .parent()
        .ok_or_else(|| error("missing resume directory"))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    ensure_regular_if_exists(state)?;
    temporary.persist(state).map_err(|e| e.error)?;
    sync_parent(state)
}

fn read_checkpoint(path: &Path) -> Result<Option<Checkpoint>> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if file.metadata()?.len() > MAX_STATE {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    (&mut file).take(MAX_STATE + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_STATE {
        return Ok(None);
    }
    Ok(serde_json::from_slice::<State>(&bytes)
        .ok()
        .filter(|state| state.version == 1)
        .map(|state| state.checkpoint))
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn sync_parent(_path: &Path) -> Result<()> {
    // File contents are synced before publication. Directory handles cannot be
    // opened and flushed portably with std on these platforms.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_errors_distinguish_contention_without_exposing_io_messages() {
        assert_eq!(
            lock_error(fs::TryLockError::WouldBlock).to_string(),
            "another download owns this resume checkpoint"
        );
        assert_eq!(
            lock_error(fs::TryLockError::Error(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "PRIVATE_PATH_OR_VALUE"
            )))
            .to_string(),
            "resume checkpoint lock failed (Interrupted, OS code None)"
        );
        let failure = std::io::Error::from_raw_os_error(9);
        let expected = format!(
            "resume checkpoint lock failed ({:?}, OS code Some(9))",
            failure.kind()
        );
        assert_eq!(
            lock_error(fs::TryLockError::Error(failure)).to_string(),
            expected
        );
    }

    #[tokio::test]
    async fn owner_release_unlocks_even_while_a_duplicated_descriptor_exists() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("media");
        let partial = Partial::open(&destination, "identity").await.unwrap();
        // A process spawned concurrently can inherit the same open file
        // description until exec closes its CLOEXEC descriptor. A duplicate
        // models that lifetime deterministically without forking the runtime.
        let inherited = partial.lock.0.try_clone().unwrap();
        drop(partial);
        let reopened = Partial::open(&destination, "identity").await.unwrap();
        drop(inherited);
        // Closing an inherited descriptor must not release the new owner's lock.
        assert!(Partial::open(&destination, "identity").await.is_err());
        drop(reopened);
        assert!(Partial::open(&destination, "identity").await.is_ok());
    }

    fn checkpoint() -> Checkpoint {
        Checkpoint {
            validator: Validator::Etag("\"version\"".into()),
            total: Some(6),
            target: "hashed-target".into(),
        }
    }

    #[tokio::test]
    async fn checkpoint_and_bytes_survive_reopen_and_publish() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("media.jpg");
        let mut partial = Partial::open(&destination, "private identity")
            .await
            .unwrap();
        partial.reset(Some(checkpoint())).await.unwrap();
        partial.append(b"abc").await.unwrap();
        drop(partial);
        let mut partial = Partial::open(&destination, "private identity")
            .await
            .unwrap();
        assert_eq!(partial.len().await.unwrap(), 3);
        assert_eq!(partial.checkpoint().unwrap().total, Some(6));
        partial.append(b"def").await.unwrap();
        assert_eq!(
            partial.publish(&destination).await.unwrap(),
            Outcome::Downloaded(destination.clone())
        );
        assert_eq!(fs::read(&destination).unwrap(), b"abcdef");
        assert!(!partial.part.exists());
        assert!(!partial.state.exists());
        assert_eq!(
            fs::read_dir(directory.path().join(".gallery-dl-rs"))
                .unwrap()
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn learned_total_persists_without_changing_bytes_and_rejects_conflicts() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("media");
        let mut partial = Partial::open(&destination, "identity").await.unwrap();
        let mut initial = checkpoint();
        initial.total = None;
        partial.reset(Some(initial)).await.unwrap();
        partial.append(b"abc").await.unwrap();
        assert!(partial.set_total(2).await.is_err());
        assert!(partial.checkpoint().unwrap().total.is_none());
        partial.set_total(6).await.unwrap();
        assert_eq!(partial.len().await.unwrap(), 3);
        assert_eq!(fs::read(&partial.part).unwrap(), b"abc");
        assert_eq!(partial.checkpoint().unwrap().total, Some(6));
        assert!(partial.set_total(7).await.is_err());
        drop(partial);
        let mut partial = Partial::open(&destination, "identity").await.unwrap();
        let saved = partial.checkpoint().unwrap();
        assert_eq!(saved.total, Some(6));
        assert!(matches!(&saved.validator, Validator::Etag(value) if value == "\"version\""));
        assert_eq!(partial.len().await.unwrap(), 3);
        partial.set_total(6).await.unwrap();
        partial.append(b"def").await.unwrap();
        partial.publish(&destination).await.unwrap();
        assert_eq!(fs::read(destination).unwrap(), b"abcdef");
    }

    #[tokio::test]
    async fn persistent_lock_excludes_and_releases() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("media");
        let partial = Partial::open(&destination, "identity").await.unwrap();
        assert!(Partial::open(&destination, "identity").await.is_err());
        drop(partial);
        assert!(Partial::open(&destination, "identity").await.is_ok());
    }

    #[tokio::test]
    async fn orphan_corrupt_and_unrecognized_state_cannot_resume() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("media");
        let mut partial = Partial::open(&destination, "identity").await.unwrap();
        partial.append(b"orphan").await.unwrap();
        let state = partial.state.clone();
        drop(partial);
        for bytes in [None, Some(b"broken".to_vec()), Some(br#"{"version":99,"checkpoint":{"validator":{"Etag":"v"},"total":6,"target":"hash"}}"#.to_vec()), Some(vec![b' '; MAX_STATE as usize + 1])] {
            if let Some(bytes) = bytes { fs::write(&state, bytes).unwrap(); }
            let partial = Partial::open(&destination, "identity").await.unwrap();
            assert!(partial.checkpoint().is_none());
            assert_eq!(partial.len().await.unwrap(), 6);
        }
        let mut partial = Partial::open(&destination, "identity").await.unwrap();
        partial.reset(Some(checkpoint())).await.unwrap();
        assert_eq!(partial.len().await.unwrap(), 0);
        partial.append(b"new").await.unwrap();
        partial.reset(None).await.unwrap();
        assert!(!state.exists());
        assert_eq!(partial.len().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn publication_never_overwrites_existing_file() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("media");
        let mut partial = Partial::open(&destination, "identity").await.unwrap();
        partial.reset(Some(checkpoint())).await.unwrap();
        partial.append(b"new").await.unwrap();
        fs::write(&destination, b"existing").unwrap();
        assert_eq!(
            partial.publish(&destination).await.unwrap(),
            Outcome::Skipped(destination.clone())
        );
        assert_eq!(fs::read(destination).unwrap(), b"existing");
        assert!(!partial.part.exists());
        assert!(!partial.state.exists());
    }

    #[tokio::test]
    async fn request_secrets_never_appear_in_persistent_state_or_names() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("media");
        let secret = "https://example.test/private?token=secret&Authorization=credential";
        let mut partial = Partial::open(&destination, secret).await.unwrap();
        partial.reset(Some(checkpoint())).await.unwrap();
        for entry in fs::read_dir(directory.path().join(".gallery-dl-rs")).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().into_owned();
            let data = String::from_utf8_lossy(&fs::read(entry.path()).unwrap()).into_owned();
            for secret in [
                "example.test",
                "token",
                "secret",
                "Authorization",
                "credential",
            ] {
                assert!(!name.contains(secret));
                assert!(!data.contains(secret));
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_symlinks_without_touching_targets() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("media");
        let target = directory.path().join("untouched");
        fs::write(&target, b"safe").unwrap();
        let partial = Partial::open(&destination, "identity").await.unwrap();
        let part = partial.part.clone();
        let state = partial.state.clone();
        let lock = part.with_extension("lock");
        drop(partial);
        for path in [&part, &state, &lock] {
            let _ = fs::remove_file(path);
            symlink(&target, path).unwrap();
            assert!(Partial::open(&destination, "identity").await.is_err());
            assert_eq!(fs::read(&target).unwrap(), b"safe");
            fs::remove_file(path).unwrap();
        }
        let mut partial = Partial::open(&destination, "identity").await.unwrap();
        partial.append(b"new").await.unwrap();
        symlink(&target, &destination).unwrap();
        assert!(partial.publish(&destination).await.is_err());
        assert!(part.exists());
        assert_eq!(fs::read(&target).unwrap(), b"safe");
        drop(partial);
        fs::remove_dir_all(directory.path().join(".gallery-dl-rs")).unwrap();
        symlink(&target, directory.path().join(".gallery-dl-rs")).unwrap();
        assert!(Partial::open(&destination, "identity").await.is_err());
    }

    #[test]
    fn cancelled_queued_publish_finishes_without_releasing_lock_or_overwriting() {
        use std::{future::Future, task::Poll};
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async {
            for existing in [false, true] {
                let directory = tempfile::tempdir().unwrap();
                let destination = directory.path().join("media");
                let mut partial = Partial::open(&destination, "identity").await.unwrap();
                partial.reset(Some(checkpoint())).await.unwrap();
                partial.append(b"abcdef").await.unwrap();
                if existing {
                    fs::write(&destination, b"existing").unwrap();
                }
                let (started_tx, started_rx) = tokio::sync::oneshot::channel();
                let (release_tx, release_rx) = std::sync::mpsc::channel();
                let blocker = tokio::task::spawn_blocking(move || {
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                });
                started_rx.await.unwrap();
                let mut publication = Box::pin(partial.publish(&destination));
                std::future::poll_fn(|cx| {
                    assert!(publication.as_mut().poll(cx).is_pending());
                    Poll::Ready(())
                })
                .await;
                drop(publication);
                assert!(partial.operations.try_lock().is_err());
                release_tx.send(()).unwrap();
                // len waits for the abandoned publication to finish, then sees
                // its removed partial. The downloader can recover by checking
                // the now-published regular destination after this barrier.
                assert!(partial.len().await.is_err());
                blocker.await.unwrap();
                assert_eq!(
                    fs::read(&destination).unwrap(),
                    if existing {
                        b"existing".as_slice()
                    } else {
                        b"abcdef".as_slice()
                    }
                );
                assert!(!partial.part.exists());
                assert!(!partial.state.exists());
                assert!(Partial::open(&destination, "identity").await.is_err());
                drop(partial);
                assert!(Partial::open(&destination, "identity").await.is_ok());
            }
        });
    }

    #[test]
    fn cancelled_queued_append_holds_operation_until_reset_can_run() {
        use std::{future::Future, task::Poll};
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async {
            let directory = tempfile::tempdir().unwrap();
            let destination = directory.path().join("media");
            let mut partial = Partial::open(&destination, "identity").await.unwrap();
            partial.reset(Some(checkpoint())).await.unwrap();
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            });
            started_rx.await.unwrap();
            let mut append = Box::pin(partial.append(b"old bytes"));
            std::future::poll_fn(|cx| {
                assert!(append.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            drop(append);
            assert!(partial.operations.try_lock().is_err());
            let mut reset = Box::pin(partial.reset(Some(checkpoint())));
            std::future::poll_fn(|cx| {
                assert!(reset.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            release_tx.send(()).unwrap();
            reset.await.unwrap();
            blocker.await.unwrap();
            assert_eq!(partial.len().await.unwrap(), 0);
            partial.append(b"new bytes").await.unwrap();
            partial.publish(&destination).await.unwrap();
            assert_eq!(fs::read(destination).unwrap(), b"new bytes");
        });
    }

    #[tokio::test]
    async fn running_blocking_operation_keeps_lock_after_owner_drops() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("media");
        let partial = Partial::open(&destination, "identity").await.unwrap();
        let retained = partial.lock.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = std::sync::mpsc::channel();
        let operation = tokio::task::spawn_blocking(move || {
            let _lock = retained;
            started_tx.send(()).unwrap();
            finish_rx.recv().unwrap();
        });
        started_rx.await.unwrap();
        drop(partial);
        assert!(Partial::open(&destination, "identity").await.is_err());
        finish_tx.send(()).unwrap();
        operation.await.unwrap();
        assert!(Partial::open(&destination, "identity").await.is_ok());
    }
}
