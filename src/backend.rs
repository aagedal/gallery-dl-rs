//! Bounded compatibility-process output streaming. Keep upstream child resolution
//! and its array protocol: JSONL mode does not preserve all resolved-child output.
use crate::{
    Result, error,
    event::{self, ExtractionEvent},
    extract::{self, Backend, BackendOptions, Media},
    messages::Decoder,
};
use futures_util::{
    StreamExt, TryStreamExt,
    stream::{self, BoxStream},
};
use std::{collections::VecDeque, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    process::{Child, ChildStdout},
    task::JoinHandle,
    time::Instant,
};
use url::Url;

const STDERR_LIMIT: usize = 8 * 1024;

pub(crate) fn stream(backend: Backend, url: Url) -> BoxStream<'static, Result<Media>> {
    stream_with_browser(backend, url, None)
}

pub(crate) fn stream_with_browser(
    backend: Backend,
    url: Url,
    browser: Option<String>,
) -> BoxStream<'static, Result<Media>> {
    stream_with_options(
        backend,
        url,
        BackendOptions {
            browser,
            ..Default::default()
        },
    )
}

pub(crate) fn stream_with_options(
    backend: Backend,
    url: Url,
    options: BackendOptions,
) -> BoxStream<'static, Result<Media>> {
    event::media_only(events_with_options(backend, url, options, true))
}

pub(crate) fn events_with_options(
    backend: Backend,
    url: Url,
    options: BackendOptions,
    require_media: bool,
) -> BoxStream<'static, Result<ExtractionEvent>> {
    // Do not start a child until a downstream consumer actually requests an item.
    stream::once(async move {
        let process = Process::start(backend, url, options, require_media)?;
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
    source_defaults: Option<std::sync::Arc<crate::naming::SourceDefaults>>,
    remaining: Duration,
    had_media: bool,
    had_post: bool,
    in_post: bool,
    done: bool,
    require_media: bool,
    pending: VecDeque<ExtractionEvent>,
}

impl Process {
    fn message_fields(
        message: &serde_json::Value,
    ) -> Option<&serde_json::Map<String, serde_json::Value>> {
        match message.as_array()?.as_slice() {
            [kind, data] if kind.as_i64() == Some(2) => data.as_object(),
            [_, _, data] => data.as_object(),
            _ => None,
        }
    }
    fn popular_metadata(
        &self,
        message: &serde_json::Value,
    ) -> Option<std::sync::Arc<serde_json::Map<String, serde_json::Value>>> {
        if self.source_defaults.as_ref()?.archive != "P_{scale[0]}_{date}_{id}" {
            return None;
        }
        let fields = Self::message_fields(message)?;
        Some(std::sync::Arc::new(
            ["date", "scale"]
                .into_iter()
                .filter_map(|key| fields.get(key).map(|value| (key.into(), value.clone())))
                .collect(),
        ))
    }
    fn start(
        backend: Backend,
        url: Url,
        options: BackendOptions,
        require_media: bool,
    ) -> Result<Self> {
        let category = extract::social_site(&url).ok_or_else(|| {
            error("unsupported URL; use an Instagram, Facebook, Twitter/X, Imgur, Bluesky, Reddit, Danbooru, e621, Gelbooru, Safebooru, Mastodon, Kemono, Coomer, Pixeldrain, Catbox, imgbox, Postimg, Tumblr, Pixiv, DeviantArt, Flickr, Wallhaven, ArtStation, Pinterest, VSCO, Newgrounds, Fur Affinity, Weasyl URL or direct media link")
        })?;
        let source_defaults = crate::booru::defaults_for_url(&url);
        crate::network::NetworkOptions::new(options.proxy.clone(), options.user_agent.clone())?;
        let mut child = backend
            .command(&url, &options)
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
            source_defaults,
            remaining: backend.timeout,
            had_media: false,
            had_post: false,
            in_post: false,
            done: false,
            require_media,
            pending: VecDeque::new(),
        })
    }

    async fn next(&mut self) -> Result<Option<ExtractionEvent>> {
        if let Some(event) = self.pending.pop_front() {
            return Ok(Some(event));
        }
        if self.done {
            return Ok(None);
        }
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

    async fn next_inner(&mut self) -> Result<Option<ExtractionEvent>> {
        if let Some(event) = self.pending.pop_front() {
            return Ok(Some(event));
        }
        if self.done {
            return Ok(None);
        }
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
                if !self.had_media && (self.require_media || !self.had_post) {
                    return Err(error(
                        "no downloadable media found; the post may require login or be unsupported",
                    ));
                }
                self.done = true;
                if self.in_post {
                    self.in_post = false;
                    return Ok(Some(ExtractionEvent::EndPost));
                }
                return Ok(None);
            }
            let (count, message) = self.decoder.feed(bytes)?;
            self.stdout.consume(count);
            if let Some(message) = message {
                let popular = self.popular_metadata(&message);
                let popular_timestamp = popular.as_ref().and_then(|_| {
                    Self::message_fields(&message)?
                        .get("created_at")
                        .and_then(|value| {
                            value
                                .as_str()
                                .and_then(|raw| crate::date::parse_date(raw).ok())
                                .or_else(|| value.as_i64())
                        })
                });
                if message
                    .as_array()
                    .and_then(|row| row.first())
                    .and_then(serde_json::Value::as_i64)
                    == Some(2)
                {
                    let mut post = extract::parse_post_message(&message, self.category)?;
                    if let Some(metadata) = &mut post.metadata {
                        metadata.source_defaults = self.source_defaults.clone();
                        if popular.is_some() {
                            metadata.booru_popular = popular.clone();
                            metadata.timestamp = popular_timestamp;
                        }
                    }
                    self.had_post = true;
                    if self.in_post {
                        self.pending
                            .push_back(ExtractionEvent::Post(Box::new(post)));
                        return Ok(Some(ExtractionEvent::EndPost));
                    }
                    self.in_post = true;
                    return Ok(Some(ExtractionEvent::Post(Box::new(post))));
                }
                if let Some(mut media) = extract::parse_message(message, self.category)? {
                    if let Some(metadata) = &mut media.metadata {
                        metadata.source_defaults = self.source_defaults.clone();
                        if popular.is_some() {
                            metadata.booru_popular = popular;
                            metadata.timestamp = popular_timestamp;
                        }
                    }
                    self.had_media = true;
                    return Ok(Some(ExtractionEvent::Media(Box::new(media))));
                }
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
