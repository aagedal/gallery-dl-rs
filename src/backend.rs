//! Bounded compatibility-process output streaming. Keep upstream child resolution
//! and its array protocol: JSONL mode does not preserve all resolved-child output.
use crate::{
    Result, error,
    extract::{self, Backend, Media},
    messages::Decoder,
};
use futures_util::{
    StreamExt, TryStreamExt,
    stream::{self, BoxStream},
};
use std::{process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    process::{Child, ChildStdout},
    task::JoinHandle,
    time::Instant,
};
use url::Url;

const STDERR_LIMIT: usize = 8 * 1024;

pub(crate) fn stream(backend: Backend, url: Url) -> BoxStream<'static, Result<Media>> {
    // Do not start a child until a downstream consumer actually requests an item.
    stream::once(async move {
        let process = Process::start(backend, url)?;
        Ok::<_, crate::Error>(stream::try_unfold(process, |mut process| async move {
            let item = process.next().await?;
            Ok(item.map(|item| (item, process)))
        }))
    })
    .try_flatten()
    .boxed()
}

struct Process {
    child: Child,
    stdout: BufReader<ChildStdout>,
    stderr: Diagnostics,
    decoder: Decoder,
    category: &'static str,
    remaining: Duration,
    had_media: bool,
}

impl Process {
    fn start(backend: Backend, url: Url) -> Result<Self> {
        let category = extract::social_site(&url).ok_or_else(|| {
            error("unsupported URL; use an Instagram, Facebook, Twitter/X URL or direct media link")
        })?;
        let mut child = backend
            .command(&url)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                error(format!(
                    "could not run {}: {e}. Install gallery-dl or set --extractor-command",
                    backend.executable.display()
                ))
            })?;
        let stdout = BufReader::new(
            child
                .stdout
                .take()
                .ok_or_else(|| error("missing backend stdout"))?,
        );
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| error("missing backend stderr"))?;
        let task = tokio::spawn(async move {
            let mut tail = Vec::new();
            let mut buffer = [0; 4096];
            loop {
                let count = stderr.read(&mut buffer).await?;
                if count == 0 {
                    return Ok(tail);
                }
                let remove = (tail.len() + count).saturating_sub(STDERR_LIMIT);
                tail.drain(..remove);
                tail.extend_from_slice(&buffer[..count]);
            }
        });
        Ok(Self {
            child,
            stdout,
            stderr: Diagnostics(Some(task)),
            decoder: Decoder::default(),
            category,
            remaining: backend.timeout,
            had_media: false,
        })
    }

    async fn next(&mut self) -> Result<Option<Media>> {
        if self.remaining.is_zero() {
            return Err(error("social extraction timed out"));
        }
        let started = Instant::now();
        let result = tokio::time::timeout(self.remaining, self.next_inner()).await;
        // Queue backpressure is not extraction time. Count only time spent
        // actively reading/parsing/waiting for this process, across all items.
        self.remaining = self.remaining.saturating_sub(started.elapsed());
        result.map_err(|_| error("social extraction timed out"))?
    }

    async fn next_inner(&mut self) -> Result<Option<Media>> {
        loop {
            let bytes = self.stdout.fill_buf().await?;
            if bytes.is_empty() {
                let status = self.child.wait().await?;
                let diagnostic = self.stderr.finish().await?;
                if !status.success() {
                    return Err(error(format!(
                        "{} extraction failed ({}): {}",
                        self.category,
                        status,
                        String::from_utf8_lossy(&diagnostic).trim()
                    )));
                }
                self.decoder.finish()?;
                if !self.had_media {
                    return Err(error(
                        "no downloadable media found; the post may require login or be unsupported",
                    ));
                }
                return Ok(None);
            }
            let (count, message) = self.decoder.feed(bytes)?;
            self.stdout.consume(count);
            if let Some(message) = message
                && let Some(media) = extract::parse_message(message, self.category)?
            {
                self.had_media = true;
                return Ok(Some(media));
            }
        }
    }
}

struct Diagnostics(Option<JoinHandle<std::io::Result<Vec<u8>>>>);
impl Diagnostics {
    async fn finish(&mut self) -> Result<Vec<u8>> {
        let result = self
            .0
            .as_mut()
            .ok_or_else(|| error("backend diagnostics already consumed"))?
            .await??;
        self.0.take();
        Ok(result)
    }
}
impl Drop for Diagnostics {
    fn drop(&mut self) {
        if let Some(task) = &self.0 {
            task.abort();
        }
    }
}
