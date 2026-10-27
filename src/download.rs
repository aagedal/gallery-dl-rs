#[path = "download_postprocess.rs"]
pub(crate) mod processing;
#[path = "download_ytdl.rs"]
mod ytdl;
use crate::{
    Result, error,
    extract::{Media, MediaJob},
    naming::{DirectoryTemplate, FilenameTemplate},
    network::NetworkOptions,
    size::{SizeLimits, SizeRejection},
    transport::{RateLimits, Redirects, retry_after},
};
use futures_util::StreamExt;
use reqwest::{
    Client,
    header::{HeaderMap, HeaderName, HeaderValue},
};
use sha2::{Digest, Sha256};
use std::{
    borrow::Cow,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};
use tokio::io::AsyncWriteExt;

/// Clones share the HTTP connection pool and per-origin rate-limit cooldowns.
#[derive(Clone)]
pub struct DownloadClient {
    http: Client,
    rate_limits: RateLimits,
    wait_limit: Duration,
    resume: bool,
    fallback: bool,
    network: NetworkOptions,
    mtime_http: bool,
    mtime_field: Option<String>,
    response_mtime: Option<std::sync::Arc<std::sync::Mutex<Option<SystemTime>>>>,
    sleep: Option<crate::sleep::SleepRange>,
    /// Maximum body bytes per second for each download.
    rate: Option<u64>,
    /// yt-dlp executable for video-page and HLS/DASH jobs.
    ytdl: Option<std::sync::Arc<Path>>,
    /// Session cookies forwarded to yt-dlp.
    ytdl_cookies: Option<std::sync::Arc<crate::cookies::YtdlCookies>>,
}

/// Per-attempt state shared between the attempt and its deadline. Throttling
/// pauses extend the deadline so rate limits cannot cause timeouts.
struct Io {
    remote: std::sync::atomic::AtomicBool,
    paused: std::sync::atomic::AtomicU64,
    received: std::sync::atomic::AtomicU64,
    rate: Option<u64>,
    started: std::time::Instant,
}

impl Io {
    fn new(rate: Option<u64>) -> Self {
        Self {
            remote: false.into(),
            paused: 0.into(),
            received: 0.into(),
            rate,
            started: std::time::Instant::now(),
        }
    }
    fn set_remote(&self, value: bool) {
        self.remote
            .store(value, std::sync::atomic::Ordering::Relaxed);
    }
    fn remote(&self) -> bool {
        self.remote.load(std::sync::atomic::Ordering::Relaxed)
    }
    fn deadline(&self, limit: Duration) -> std::time::Instant {
        let paused = self.paused.load(std::sync::atomic::Ordering::Relaxed);
        self.started + limit + Duration::from_nanos(paused)
    }
    /// Delay until the bytes received in this attempt fit the rate limit.
    async fn throttle(&self, bytes: usize) {
        let Some(rate) = self.rate else { return };
        let total = self
            .received
            .fetch_add(bytes as u64, std::sync::atomic::Ordering::Relaxed)
            + bytes as u64;
        let target = Duration::from_secs_f64(total as f64 / rate as f64);
        let elapsed = self.started.elapsed();
        if let Some(wait) = target.checked_sub(elapsed).filter(|wait| !wait.is_zero()) {
            self.paused.fetch_add(
                u64::try_from(wait.as_nanos()).unwrap_or(u64::MAX),
                std::sync::atomic::Ordering::Relaxed,
            );
            tokio::time::sleep(wait).await;
        }
    }
}

/// Modification time policy for newly published files.
#[derive(Clone, Copy)]
struct Mtime {
    /// A metadata-derived time, which takes precedence over HTTP headers.
    fixed: Option<SystemTime>,
    http: bool,
}

impl Mtime {
    fn select(self, headers: &HeaderMap) -> Option<SystemTime> {
        self.fixed.or_else(|| {
            self.http
                .then(|| headers.get(reqwest::header::LAST_MODIFIED))
                .flatten()
                .and_then(|value| value.to_str().ok())
                .and_then(|value| httpdate::parse_http_date(value).ok())
        })
    }
}

fn set_mtime(file: &std::fs::File, time: Option<SystemTime>) -> std::io::Result<()> {
    match time {
        Some(time) => file.set_modified(time),
        None => Ok(()),
    }
}

impl DownloadClient {
    fn capture_http_mtime(&self, headers: &HeaderMap) {
        if let Some(capture) = &self.response_mtime
            && let Ok(mut timestamp) = capture.lock()
        {
            *timestamp = headers
                .get(reqwest::header::LAST_MODIFIED)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| httpdate::parse_http_date(value).ok());
        }
    }

    /// Apply transport overrides while retaining shared per-origin cooldowns.
    pub fn with_network(mut self, network: &NetworkOptions) -> Result<Self> {
        if self.network != *network {
            self.http = network.client(
                concat!("gallery-dl-rs/", env!("CARGO_PKG_VERSION")),
                self.wait_limit,
            )?;
        }
        self.network = network.clone();
        Ok(self)
    }

    /// Include configured request headers in destination and resume identities.
    pub fn effective_media<'a>(&self, media: &'a Media) -> Cow<'a, Media> {
        if self.network.user_agent().is_some() {
            let mut media = media.clone();
            self.network.apply_media(&mut media);
            Cow::Owned(media)
        } else {
            Cow::Borrowed(media)
        }
    }

    /// Override the per-attempt deadline while sharing connections and cooldowns.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.wait_limit = timeout;
        self
    }
    /// Try explicitly supplied fallback URLs after remote failures.
    pub fn with_fallback(mut self, enabled: bool) -> Self {
        self.fallback = enabled;
        self
    }

    /// Set new files' modification times from HTTP `Last-Modified` (default).
    pub fn with_http_mtime(mut self, enabled: bool) -> Self {
        self.mtime_http = enabled;
        self
    }

    /// Set new files' modification times from a metadata date field, such as
    /// `date`, overriding `Last-Modified`. Absent or non-numeric values are ignored.
    pub fn with_metadata_mtime(mut self, field: Option<String>) -> Self {
        self.mtime_field = field;
        self
    }

    /// Limit each download's body to this many bytes per second.
    pub fn with_rate_limit(mut self, rate: Option<u64>) -> Self {
        self.rate = rate.filter(|rate| *rate > 0);
        self
    }

    /// The yt-dlp executable for video-page and HLS/DASH jobs; `None`
    /// disables them.
    pub fn with_ytdl(mut self, command: Option<&Path>) -> Self {
        self.ytdl = command.map(Into::into);
        self
    }

    /// Session cookies forwarded to yt-dlp downloads.
    pub fn with_ytdl_cookies(mut self, cookies: Option<crate::cookies::YtdlCookies>) -> Self {
        self.ytdl_cookies = cookies.map(Into::into);
        self
    }

    /// Wait before each network download, after archive and existing-file checks.
    pub fn with_sleep(mut self, sleep: Option<crate::sleep::SleepRange>) -> Self {
        self.sleep = sleep;
        self
    }

    fn mtime(&self, media: &Media) -> Mtime {
        let fixed = self.mtime_field.as_ref().and_then(|field| {
            let seconds = match crate::filter::context(media).get(field.as_str())? {
                crate::filter::Value::Int(value) => i64::try_from(*value).ok()?,
                crate::filter::Value::Float(value) if value.is_finite() => *value as i64,
                _ => return None,
            };
            if seconds >= 0 {
                SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(seconds as u64))
            } else {
                SystemTime::UNIX_EPOCH.checked_sub(Duration::from_secs(seconds.unsigned_abs()))
            }
        });
        Mtime {
            fixed,
            http: self.mtime_http,
        }
    }

    /// Retain validated partial HTTP downloads for retries and later invocations.
    pub fn with_resume(mut self, enabled: bool) -> Self {
        self.resume = enabled;
        self
    }
}

pub fn client(timeout: Duration) -> Result<DownloadClient> {
    Ok(DownloadClient {
        http: Client::builder()
            .user_agent(concat!("gallery-dl-rs/", env!("CARGO_PKG_VERSION")))
            .timeout(timeout)
            // Redirects are validated manually before forwarding request headers.
            .redirect(reqwest::redirect::Policy::none())
            .build()?,
        rate_limits: RateLimits::default(),
        wait_limit: timeout,
        resume: false,
        fallback: true,
        network: NetworkOptions::default(),
        mtime_http: true,
        mtime_field: None,
        response_mtime: None,
        sleep: None,
        rate: None,
        ytdl: Some(Path::new("yt-dlp").into()),
        ytdl_cookies: None,
    })
}

pub fn safe_component(value: &str) -> String {
    let value: String = value
        .chars()
        .take(100)
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let value = value.trim_matches('.');
    if value.is_empty() {
        "media".into()
    } else {
        value.into()
    }
}

pub fn destination(root: &Path, media: &Media) -> PathBuf {
    // Hash URL and job/request context so different manifests or authenticated
    // representations cannot share a path. This is not a stable archive identity.
    let digest = format!("{:x}", Sha256::digest(media.identity().as_bytes()));
    root.join(safe_component(&media.category))
        .join(format!("{digest}_{}", safe_component(&media.filename)))
}

/// Custom names replace the default digest-prefixed basename. Publication still
/// never overwrites; callers should include post/media identifiers to avoid collisions.
pub fn destination_with_template(
    root: &Path,
    media: &Media,
    template: &FilenameTemplate,
) -> Result<PathBuf> {
    destination_with_templates(root, media, Some(template), None)
}

/// Render a complete destination. An explicit directory replaces the default
/// category component while the basename follows the normal filename policy.
pub fn destination_with_templates(
    root: &Path,
    media: &Media,
    filename: Option<&FilenameTemplate>,
    directory: Option<&DirectoryTemplate>,
) -> Result<PathBuf> {
    let filename = crate::naming::effective_filename(media, filename);
    let directory = crate::naming::effective_directory(media, directory);
    let default = match filename
        .as_ref()
        .map(|template| template.render_optional(media))
        .transpose()?
        .flatten()
    {
        Some(name) => root.join(safe_component(&media.category)).join(name),
        None => destination(root, media),
    };
    match directory
        .as_ref()
        .map(|d| d.render(media))
        .transpose()?
        .flatten()
    {
        None => Ok(default),
        Some(directory) => Ok(root.join(directory).join(
            default
                .file_name()
                .ok_or_else(|| error("invalid media filename"))?,
        )),
    }
}

/// Hook-aware destination rendering without reconstructing typed API metadata.
pub(crate) fn destination_with_keyword_templates(
    root: &Path,
    media: &Media,
    filename: Option<&FilenameTemplate>,
    directory: Option<&DirectoryTemplate>,
    keywords: &serde_json::Map<String, serde_json::Value>,
) -> Result<PathBuf> {
    let filename = crate::naming::effective_filename(media, filename);
    let default = destination(root, media);
    let name = filename
        .as_ref()
        .map(|template| template.render_keywords(media, keywords))
        .transpose()?
        .flatten();
    let basename = name
        .as_deref()
        .map(std::ffi::OsStr::new)
        .or_else(|| default.file_name())
        .ok_or_else(|| error("invalid media filename"))?;
    Ok(directory_with_keywords(root, media, directory, keywords)?.join(basename))
}

/// Render the directory checkpoint without evaluating any filename formatter.
pub(crate) fn directory_with_keywords(
    root: &Path,
    media: &Media,
    directory: Option<&DirectoryTemplate>,
    keywords: &serde_json::Map<String, serde_json::Value>,
) -> Result<PathBuf> {
    let directory = crate::naming::effective_directory(media, directory);
    let directory = directory
        .as_ref()
        .map(|template| template.render_keywords(media, keywords))
        .transpose()?
        .flatten();
    Ok(match directory {
        Some(directory) => root.join(directory),
        None => root.join(safe_component(&media.category)),
    })
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Downloaded(PathBuf),
    Skipped(PathBuf),
    Filtered(SizeRejection),
}

impl Outcome {
    pub fn saved_path(&self) -> Option<&Path> {
        match self {
            Self::Downloaded(path) | Self::Skipped(path) => Some(path),
            Self::Filtered(_) => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DownloadOptions<'a> {
    pub retries: u8,
    pub postprocessor_scope: Option<&'a crate::postprocess::jobs::ScopeLease>,
    pub postprocessors: Option<&'a crate::postprocess::Postprocessors>,
    /// Override category folders with per-media directory components.
    pub directory: Option<&'a DirectoryTemplate>,
    /// Save versioned post metadata beside completed or already existing media.
    pub write_metadata: bool,
    /// Inclusive limits on the complete HTTP representation size.
    pub limits: SizeLimits,
}

pub async fn download(
    client: &DownloadClient,
    root: &Path,
    media: &Media,
    retries: u8,
) -> Result<Outcome> {
    download_with_options(
        client,
        root,
        media,
        DownloadOptions {
            retries,
            ..DownloadOptions::default()
        },
    )
    .await
}

pub async fn download_with_options(
    client: &DownloadClient,
    root: &Path,
    media: &Media,
    options: DownloadOptions<'_>,
) -> Result<Outcome> {
    download_with_template(client, root, media, options, None).await
}

pub async fn download_with_template(
    client: &DownloadClient,
    root: &Path,
    media: &Media,
    options: DownloadOptions<'_>,
    template: Option<&FilenameTemplate>,
) -> Result<Outcome> {
    let effective_media = client.effective_media(media);
    let media = effective_media.as_ref();
    let resolved_filename = crate::naming::effective_filename(media, template);
    let template = resolved_filename.as_ref();
    let source_booru = media
        .metadata
        .as_ref()
        .is_some_and(|metadata| metadata.booru_keywords.is_some());
    // Source filename preparation also runs when no hooks are configured.
    let empty_processors = (source_booru && options.postprocessors.is_none())
        .then(|| crate::postprocess::Postprocessors::new(Vec::new()));
    let options = DownloadOptions {
        postprocessors: options.postprocessors.or(empty_processors.as_ref()),
        ..options
    };
    if source_booru || options.postprocessors.is_some_and(|pp| pp.has_generic()) {
        let prepared = processing::prepare(client, root, media, options, template).await?;
        return processing::download(client, root, media, options, template, prepared).await;
    }
    let prepared = options.postprocessors.map(|pp| pp.prepared_media(media));
    let output_media = prepared.as_ref().unwrap_or(media);
    let original_frames = media
        .metadata
        .as_ref()
        .is_some_and(|m| m.ugoira_frame_index.is_some());
    let selected = options
        .postprocessors
        .map(|pp| pp.selected(media))
        .unwrap_or_default();
    if original_frames && !selected.is_empty() {
        let path = destination_with_templates(root, media, template, options.directory)?;
        options
            .postprocessors
            .unwrap()
            .frames
            .register(media, &path)?;
    }
    let mut converted_media = None;
    let mut outcome = if !original_frames && !selected.is_empty() {
        download_ugoira(
            client,
            root,
            media,
            output_media,
            options,
            template,
            &selected,
        )
        .await?
    } else {
        download_media(client, root, media, options, template, None).await?
    };
    if original_frames
        && !selected.is_empty()
        && let Some((path, converted)) =
            complete_original(client, root, media, &outcome, options, template).await?
    {
        outcome = Outcome::Downloaded(path);
        converted_media = Some(converted);
    }
    if options.write_metadata
        && let Some(path) = outcome.saved_path()
    {
        write_metadata(path, converted_media.as_ref().unwrap_or(output_media))
            .await
            .map_err(|e| {
                error(format!(
                    "media is saved at {}; metadata sidecar failed: {e}",
                    path.display()
                ))
            })?;
    }
    Ok(outcome)
}

pub(crate) async fn complete_original(
    client: &DownloadClient,
    root: &Path,
    media: &Media,
    outcome: &Outcome,
    options: DownloadOptions<'_>,
    template: Option<&FilenameTemplate>,
) -> Result<Option<(PathBuf, Media)>> {
    if let Some(pp) = options.postprocessors
        && let Some(path) = outcome.saved_path()
        && let Some(collection) =
            pp.frames
                .complete(media, path, matches!(outcome, Outcome::Downloaded(_)))?
    {
        let mut converted_media = None;
        let fingerprints = match &collection.input {
            crate::postprocess::FrameInput::Files(frames) => frames
                .iter()
                .map(|frame| {
                    std::fs::symlink_metadata(&frame.path)
                        .ok()
                        .map(|metadata| (frame.path.clone(), metadata))
                })
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        };
        let mut last = None;
        for spec in options.postprocessors.unwrap().selected(&collection.media) {
            let converted = crate::postprocess::with_extension(&collection.media, spec.extension());
            let path = destination_with_templates(root, &converted, template, options.directory)?;
            crate::postprocess::convert(
                spec,
                &collection.input,
                &converted,
                &path,
                client.wait_limit,
            )
            .await?;
            last = Some(path);
            converted_media = Some(converted);
        }
        // Delay removal until every converter has finished using the source frames.
        if options
            .postprocessors
            .unwrap()
            .selected(&collection.media)
            .iter()
            .any(|spec| !spec.keep_files())
            && let crate::postprocess::FrameInput::Files(frames) = &collection.input
        {
            for (frame, before) in frames.iter().zip(fingerprints) {
                if last.as_ref() != Some(&frame.path)
                    && let Some((_, before)) = before
                    && let Ok(after) = std::fs::symlink_metadata(&frame.path)
                    && unchanged_file(&before, &after)
                {
                    tokio::fs::remove_file(&frame.path).await?;
                }
            }
        }
        let result = last.zip(converted_media);
        if options.write_metadata
            && let Some((path, media)) = &result
        {
            write_metadata(path, media).await.map_err(|e| {
                error(format!(
                    "media is saved at {}; metadata sidecar failed: {e}",
                    path.display()
                ))
            })?;
        }
        return Ok(result);
    }
    Ok(None)
}

fn unchanged_file(before: &std::fs::Metadata, after: &std::fs::Metadata) -> bool {
    if !after.is_file()
        || before.len() != after.len()
        || before.modified().ok() != after.modified().ok()
    {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if before.dev() != after.dev() || before.ino() != after.ino() {
            return false;
        }
    }
    true
}

/// Preserve a fully downloaded source even when the conversion future is cancelled.
struct UgoiraSource {
    directory: tempfile::TempDir,
    source: Option<PathBuf>,
    recovery: PathBuf,
    recover_at_original: bool,
}
impl Drop for UgoiraSource {
    fn drop(&mut self) {
        if let Some(source) = &self.source
            && (!self.recover_at_original || std::fs::hard_link(source, &self.recovery).is_err())
            && let Some(parent) = self.recovery.parent()
            && let Ok(file) = tempfile::Builder::new()
                .prefix(".gallery-dl-ugoira-source-")
                .suffix(&format!(
                    ".{}",
                    self.recovery
                        .extension()
                        .and_then(|ext| ext.to_str())
                        .unwrap_or("bin")
                ))
                .tempfile_in(parent)
        {
            let path = file.into_temp_path();
            if std::fs::rename(source, &path).is_ok() {
                let _ = path.keep();
            }
        }
    }
}
async fn download_ugoira(
    client: &DownloadClient,
    root: &Path,
    media: &Media,
    prepared: &Media,
    options: DownloadOptions<'_>,
    template: Option<&FilenameTemplate>,
    selected: &[&crate::postprocess::UgoiraOptions],
) -> Result<Outcome> {
    let path = destination_with_templates(root, prepared, template, options.directory)?;
    if crate::postprocess::regular_file(&path)? {
        return Ok(Outcome::Skipped(path));
    }
    let parent = path
        .parent()
        .ok_or_else(|| error("invalid postprocessor destination"))?;
    tokio::fs::create_dir_all(parent).await?;
    let recovery = destination_with_templates(root, media, template, options.directory)?;
    if let Some(parent) = recovery.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut source = UgoiraSource {
        directory: tempfile::TempDir::new_in(parent)?,
        source: None,
        recover_at_original: recovery != path,
        recovery,
    };
    let downloaded = download_media(
        client,
        source.directory.path(),
        media,
        DownloadOptions {
            directory: None,
            ..options
        },
        None,
        Some(&source.recovery),
    )
    .await?;
    let Some(input_path) = downloaded.saved_path() else {
        return Ok(downloaded);
    };
    source.source = Some(input_path.to_owned());
    let frames = media
        .metadata
        .as_ref()
        .and_then(|m| m.frames.clone())
        .ok_or_else(|| error("ugoira frame metadata missing"))?;
    let input = crate::postprocess::FrameInput::Zip {
        path: input_path.to_owned(),
        frames,
    };
    let mut published = std::collections::HashSet::new();
    for spec in selected {
        let converted = crate::postprocess::with_extension(prepared, spec.extension());
        let output = destination_with_templates(root, &converted, template, options.directory)?;
        crate::postprocess::convert(spec, &input, &converted, &output, client.wait_limit).await?;
        published.insert(output);
    }
    if selected.iter().all(|spec| spec.keep_files()) && !published.contains(&path) {
        match std::fs::hard_link(input_path, &path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                crate::postprocess::regular_file(&path)?;
            }
            Err(e) => return Err(e.into()),
        }
    }
    source.source = None;
    Ok(Outcome::Downloaded(path))
}

/// Deliberately exclude URLs and private transport/adapter options, including
/// signed media URLs. This schema is independent of inspection JSON.
#[derive(serde::Serialize)]
struct MetadataSidecar<'a> {
    schema_version: u8,
    category: &'a str,
    filename: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<&'a crate::extract::PostMetadata>,
}

async fn write_metadata(path: &Path, media: &Media) -> Result<()> {
    let mut name = path.as_os_str().to_os_string();
    name.push(".json");
    let sidecar = PathBuf::from(name);
    if metadata_exists(&sidecar)? {
        return Ok(());
    }
    let parent = path.parent().ok_or_else(|| error("invalid destination"))?;
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| error("invalid media filename"))?;
    let mut metadata = media.metadata.clone();
    if let Some(article) = metadata.as_mut().and_then(|post| post.article.as_mut()) {
        // Article HTML can embed signed image URLs; sidecars retain only the
        // descriptive article fields, just as they omit the media request URL.
        article.html = None;
    }
    if let Some(community) = metadata.as_mut().and_then(|post| post.community.as_mut()) {
        // Like article HTML, the banner can contain a private signed media URL.
        community.banner = None;
    }
    let mut bytes = serde_json::to_vec_pretty(&MetadataSidecar {
        schema_version: 1,
        category: &media.category,
        filename,
        metadata: metadata.as_ref(),
    })?;
    bytes.push(b'\n');
    let temporary = tempfile::NamedTempFile::new_in(parent)?.into_temp_path();
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&temporary)
        .await?;
    file.write_all(&bytes).await?;
    file.flush().await?;
    file.sync_all().await?;
    drop(file);
    match temporary.persist_noclobber(&sidecar) {
        Ok(_) => Ok(()),
        Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {
            if metadata_exists(&sidecar)? {
                Ok(())
            } else {
                Err(e.error.into())
            }
        }
        Err(e) => Err(e.error.into()),
    }
}

fn metadata_exists(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(true),
        Ok(_) => Err(error("metadata sidecar path is not a regular file")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

async fn download_media(
    client: &DownloadClient,
    root: &Path,
    media: &Media,
    options: DownloadOptions<'_>,
    template: Option<&FilenameTemplate>,
    checkpoint_destination: Option<&Path>,
) -> Result<Outcome> {
    let DownloadOptions {
        retries,
        directory,
        limits,
        ..
    } = options;
    let primary_validation = validate_job(client, media);
    if let Err(failure) = &primary_validation
        && (!failure.fallback || !client.fallback || !media.handoff.contains_key("_fallback"))
    {
        return Err(error(failure.error.to_string()));
    }
    // Destination and publication metadata always belong to the original job.
    let path = destination_with_templates(root, media, template, directory)?;
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() => return Ok(Outcome::Skipped(path)),
        Ok(_) => return Err(error("media destination is not a regular file")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    if let MediaJob::InlineText { data } = &media.job {
        if let Some(rejection) = limits.check(data.len() as u64) {
            return Ok(Outcome::Filtered(rejection));
        }
        let parent = path.parent().ok_or_else(|| error("invalid destination"))?;
        tokio::fs::create_dir_all(parent).await?;
        return write_inline_text(parent, &path, data, client.mtime(media).fixed).await;
    }
    let parent = path.parent().ok_or_else(|| error("invalid destination"))?;
    let mut candidate = Cow::Borrowed(media);
    let mut seen = std::collections::HashSet::new();
    seen.insert(target_digest(&media.url));
    let mut next_index = 0;
    let mut partial: Option<crate::resume::Partial> = None;
    // Keep the primary checkpoint's reservation until the whole logical job ends.
    let mut _primary_reservation = None;
    let mut first = true;
    let mtime = client.mtime(media);
    if let Some(sleep) = &client.sleep {
        sleep.wait().await;
    }
    loop {
        if !first {
            match std::fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.is_file() => return Ok(Outcome::Skipped(path)),
                Ok(_) => return Err(error("media destination is not a regular file")),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        let result = match validate_job(client, &candidate) {
            Err(failure) => Err(failure),
            Ok(()) if ytdl::source(&candidate).is_some() => {
                ytdl::download(ytdl::Request {
                    command: client.ytdl.as_deref().expect("validated yt-dlp job"),
                    media: &candidate,
                    path: &path,
                    network: &client.network,
                    retries,
                    rate: client.rate,
                    limits,
                    mtime: mtime.fixed,
                    cookies: client.ytdl_cookies.as_deref(),
                })
                .await
            }
            Ok(()) => {
                tokio::fs::create_dir_all(parent).await?;
                let mut headers = HeaderMap::new();
                for (name, value) in &candidate.headers {
                    let name = HeaderName::from_bytes(name.as_bytes())?;
                    if matches!(
                        name.as_str(),
                        "host"
                            | "content-length"
                            | "transfer-encoding"
                            | "connection"
                            | "proxy-authorization"
                            | "range"
                            | "if-range"
                    ) {
                        continue;
                    }
                    headers.insert(name, HeaderValue::from_str(value)?);
                }
                partial = if client.resume {
                    Some(
                        crate::resume::Partial::open(
                            checkpoint_destination.unwrap_or(&path),
                            &candidate.identity(),
                        )
                        .await?,
                    )
                } else {
                    None
                };
                download_candidate(
                    client,
                    &candidate,
                    &headers,
                    &path,
                    retries,
                    limits,
                    &mut partial,
                    mtime,
                )
                .await
            }
        };
        let failure = match result {
            Ok(outcome) => return Ok(outcome),
            Err(failure) if failure.fallback && client.fallback => failure,
            Err(failure) => return Err(failure.error),
        };
        let Some(target) = next_fallback(media, &mut next_index, &mut seen)? else {
            return Err(failure.error);
        };
        if let Some(mut old) = partial.take() {
            old.discard().await?;
            if first {
                _primary_reservation = Some(old);
            }
        }
        first = false;
        let mut next = media.clone();
        next.url = target;
        next.job = MediaJob::DirectHttp;
        if !same_origin(&media.url, &next.url) {
            // Explicit cross-origin fallbacks may be public, but never inherit
            // per-item credentials or arbitrary private headers from the first URL.
            next.headers.clear();
        }
        candidate = Cow::Owned(next);
    }
}

fn validate_job(client: &DownloadClient, media: &Media) -> std::result::Result<(), Failure> {
    if client.ytdl.is_some() && ytdl::source(media).is_some() {
        return Ok(());
    }
    match &media.job {
        MediaJob::InlineText { data } if data.len() > crate::extract::MAX_INLINE_TEXT => {
            Err(Failure {
                error: error("inline text exceeds the payload size limit"),
                retry: false,
                fallback: false,
            })
        }
        MediaJob::InlineText { .. } => Ok(()),
        MediaJob::DirectHttp => crate::extract::http_url(media.url.as_str())
            .map(|_| ())
            .map_err(|_| Failure {
                error: error("invalid direct media URL"),
                retry: false,
                fallback: false,
            }),
        MediaJob::VideoPage { .. } => Err(Failure {
            error: error("video-page downloads need yt-dlp, which is disabled"),
            retry: false,
            fallback: true,
        }),
        MediaJob::Manifest { .. } => Err(Failure {
            error: error(
                "HLS/DASH downloads need yt-dlp, which is disabled or cannot read inline manifests",
            ),
            retry: false,
            fallback: true,
        }),
    }
}

fn same_origin(first: &url::Url, second: &url::Url) -> bool {
    first.scheme() == second.scheme()
        && first.host_str() == second.host_str()
        && first.port_or_known_default() == second.port_or_known_default()
}

fn target_digest(target: &url::Url) -> [u8; 32] {
    let mut target = target.clone();
    target.set_fragment(None);
    Sha256::digest(target.as_str().as_bytes()).into()
}

fn next_fallback(
    media: &Media,
    index: &mut usize,
    seen: &mut std::collections::HashSet<[u8; 32]>,
) -> Result<Option<url::Url>> {
    let Some(value) = media.handoff.get("_fallback") else {
        return Ok(None);
    };
    let values = value
        .as_array()
        .filter(|values| values.len() <= 128)
        .ok_or_else(|| error("media fallback list must contain at most 128 URLs"))?;
    while let Some(value) = values.get(*index) {
        *index += 1;
        let Some(value) = value.as_str().filter(|value| value.len() <= 16 * 1024) else {
            continue;
        };
        let Ok(mut target) = crate::extract::http_url(value) else {
            continue;
        };
        if crate::extract::manifest_protocol(&target).is_some() {
            continue;
        }
        target.set_fragment(None);
        if seen.insert(target_digest(&target)) {
            return Ok(Some(target));
        }
    }
    Ok(None)
}

#[allow(clippy::too_many_arguments)]
async fn download_candidate(
    client: &DownloadClient,
    media: &Media,
    headers: &HeaderMap,
    path: &Path,
    retries: u8,
    limits: SizeLimits,
    partial: &mut Option<crate::resume::Partial>,
    mtime: Mtime,
) -> std::result::Result<Outcome, Failure> {
    let parent = path.parent().ok_or_else(|| Failure {
        error: error("invalid destination"),
        retry: false,
        fallback: false,
    })?;
    for attempt in 0..=retries {
        // Only a timeout while awaiting the network may advance candidates.
        // Disk/checkpoint/publication timeouts must preserve this representation.
        let io = Io::new(client.rate);
        let attempt_future = async {
            match partial {
                Some(partial) => {
                    attempt_resume(client, media, headers, path, partial, limits, mtime, &io).await
                }
                None => {
                    attempt_download(client, media, headers, parent, path, limits, mtime, &io).await
                }
            }
        };
        tokio::pin!(attempt_future);
        let result = loop {
            let deadline = io.deadline(client.wait_limit);
            tokio::select! {
                result = &mut attempt_future => break result,
                _ = tokio::time::sleep_until(deadline.into()) => {
                    if std::time::Instant::now() >= io.deadline(client.wait_limit) {
                        break Err(Failure {
                            error: error("media download attempt timed out"),
                            retry: io.remote(),
                            fallback: io.remote(),
                        });
                    }
                }
            }
        };
        match result {
            Ok(outcome) => return Ok(outcome),
            Err(failure) if failure.retry && attempt < retries => {
                tokio::time::sleep(Duration::from_millis(250 * (1u64 << attempt.min(6)))).await;
            }
            Err(failure) => return Err(failure),
        }
    }
    unreachable!()
}

async fn write_inline_text(
    parent: &Path,
    path: &Path,
    data: &str,
    mtime: Option<SystemTime>,
) -> Result<Outcome> {
    // TempPath removes an incomplete file if this future is cancelled.
    let temporary = tempfile::NamedTempFile::new_in(parent)?.into_temp_path();
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&temporary)
        .await?;
    file.write_all(data.as_bytes()).await?;
    file.flush().await?;
    let file = file.into_std().await;
    set_mtime(&file, mtime)?;
    file.sync_all()?;
    drop(file);
    match temporary.persist_noclobber(path) {
        Ok(_) => Ok(Outcome::Downloaded(path.into())),
        Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {
            if std::fs::symlink_metadata(path)?.is_file() {
                Ok(Outcome::Skipped(path.into()))
            } else {
                Err(error("media destination is not a regular file"))
            }
        }
        Err(e) => Err(e.error.into()),
    }
}

#[derive(Debug)]
struct Failure {
    error: crate::Error,
    retry: bool,
    fallback: bool,
}
impl From<std::io::Error> for Failure {
    fn from(error: std::io::Error) -> Self {
        Self {
            error: error.into(),
            retry: false,
            fallback: false,
        }
    }
}
impl From<reqwest::Error> for Failure {
    fn from(error: reqwest::Error) -> Self {
        Self {
            // Redirect URLs can contain session tokens or signed query strings.
            error: if error.is_timeout() {
                crate::error("media download attempt timed out")
            } else {
                error.without_url().into()
            },
            retry: true,
            fallback: true,
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn attempt_download(
    client: &DownloadClient,
    media: &Media,
    headers: &HeaderMap,
    parent: &Path,
    path: &Path,
    limits: SizeLimits,
    mtime: Mtime,
    io: &Io,
) -> std::result::Result<Outcome, Failure> {
    io.set_remote(true);
    let response = request_media(client, media, headers).await;
    io.set_remote(false);
    let response = response?;
    check_status(client, media, &response, false)?;
    if response
        .headers()
        .contains_key(reqwest::header::CONTENT_RANGE)
    {
        return Err(terminal("unexpected Content-Range in full media response"));
    }
    let expected = response.content_length();
    client.capture_http_mtime(response.headers());
    let modified = mtime.select(response.headers());
    validate_media(&response)?;
    if let Some(rejection) = expected.and_then(|size| limits.check(size)) {
        return Ok(Outcome::Filtered(rejection));
    }
    // A unique temporary file prevents concurrent workers from sharing a partial
    // file. Dropping TempPath removes incomplete downloads, including on failure.
    let temporary = tempfile::NamedTempFile::new_in(parent)?.into_temp_path();
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&temporary)
        .await?;
    let mut stream = response.bytes_stream();
    let mut received = 0u64;
    let check_placeholder = kemono_txt_resource(media);
    let mut prefix = Vec::new();
    loop {
        io.set_remote(true);
        let chunk = stream.next().await;
        io.set_remote(false);
        let Some(chunk) = chunk else { break };
        let chunk = chunk?;
        received = received
            .checked_add(chunk.len() as u64)
            .ok_or_else(|| terminal("media response exceeds size limit"))?;
        if expected.is_some_and(|expected| received > expected) {
            return Err(terminal("media body exceeds declared length"));
        }
        if let Some(rejection) = limits.check_max(received) {
            return Ok(Outcome::Filtered(rejection));
        }
        if check_placeholder && prefix.len() < 9 {
            prefix.extend_from_slice(&chunk[..chunk.len().min(9 - prefix.len())]);
        }
        file.write_all(&chunk).await?;
        io.throttle(chunk.len()).await;
    }
    if expected.is_some_and(|length| length != received) {
        return Err(Failure {
            error: error("incomplete response body"),
            retry: true,
            fallback: true,
        });
    }
    if let Some(rejection) = limits.check(received) {
        return Ok(Outcome::Filtered(rejection));
    }
    if check_placeholder && received == 9 && prefix == b"not found" {
        return Err(terminal("Kemono media returned a missing-file placeholder"));
    }
    file.flush().await?;
    let file = file.into_std().await;
    set_mtime(&file, modified)?;
    file.sync_all()?;
    drop(file);
    match temporary.persist_noclobber(path) {
        Ok(_) => Ok(Outcome::Downloaded(path.into())),
        Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {
            if std::fs::symlink_metadata(path)?.is_file() {
                Ok(Outcome::Skipped(path.into()))
            } else {
                Err(Failure {
                    error: error("media destination is not a regular file"),
                    retry: false,
                    fallback: false,
                })
            }
        }
        Err(e) => Err(e.error.into()),
    }
}

async fn request_media(
    client: &DownloadClient,
    media: &Media,
    headers: &HeaderMap,
) -> std::result::Result<reqwest::Response, Failure> {
    let mut target = media.url.clone();
    // Kemono/Coomer serve files through CDN redirects. Only their public
    // resource headers may cross that origin boundary.
    let mut redirects =
        Redirects::new(&target).allow_subdomains(kemono_public_redirects(media, headers));
    // Upstream `_http_method = "POST"` with an `_http_data` form; redirects
    // after the POST are followed with GET, as `requests` does.
    let mut form = post_form(media);
    let response = loop {
        client
            .rate_limits
            .wait(&target, client.wait_limit)
            .await
            .map_err(|error| Failure {
                error,
                retry: false,
                fallback: true,
            })?;
        let request = match form.take() {
            Some(form) => client.http.post(target.clone()).form(&form),
            None => client.http.get(target.clone()),
        };
        let response = request
            // Rate-limited bodies may outlast the attempt limit; the attempt's
            // extended deadline enforces it instead.
            .timeout(if client.rate.is_some() {
                Duration::from_secs(u32::MAX as u64)
            } else {
                client.wait_limit
            })
            .headers(headers.clone())
            .send()
            .await?;
        match redirects
            .next(response.url(), response.status(), response.headers())
            .map_err(|error| Failure {
                error,
                retry: false,
                fallback: true,
            })? {
            Some(next) => target = next,
            None => break response,
        }
    };
    Ok(response)
}

/// The form fields of a media POST request, if its handoff asks for one.
fn post_form(media: &Media) -> Option<Vec<(String, String)>> {
    let method = media.handoff.get("_http_method")?.as_str()?;
    if !method.eq_ignore_ascii_case("post") {
        return None;
    }
    let data = media.handoff.get("_http_data").and_then(|d| d.as_object());
    Some(
        data.into_iter()
            .flatten()
            .map(|(key, value)| {
                let value = match value {
                    serde_json::Value::String(text) => text.clone(),
                    other => other.to_string(),
                };
                (key.clone(), value)
            })
            .collect(),
    )
}

fn kemono_public_redirects(media: &Media, headers: &HeaderMap) -> bool {
    use reqwest::header::{ACCEPT_ENCODING, IF_RANGE, RANGE, REFERER, USER_AGENT};
    let category = media.category.as_str();
    if !matches!(category, "kemono" | "coomer") {
        return false;
    }
    let Some(host) = media.url.host_str() else {
        return false;
    };
    let known_host = crate::kemono::category(host) == Some(category)
        || host
            .strip_prefix("img.")
            .is_some_and(|domain| crate::kemono::HOSTS.contains(&(domain, category)));
    if !known_host
        || headers.keys().any(|name| {
            !matches!(
                *name,
                USER_AGENT | ACCEPT_ENCODING | RANGE | IF_RANGE | REFERER
            )
        })
    {
        return false;
    }
    let mut referers = headers.get_all(REFERER).iter();
    if let Some(referer) = referers.next() {
        if referers.next().is_some() {
            return false;
        }
        let Some(referer) = referer
            .to_str()
            .ok()
            .and_then(|value| url::Url::parse(value).ok())
        else {
            return false;
        };
        if referer.scheme() != "https"
            || referer.query().is_some()
            || referer.fragment().is_some()
            || referer.host_str().and_then(crate::kemono::category) != Some(category)
            || !matches!(
                crate::kemono::target(&referer),
                Ok(crate::kemono::Target::Post { .. }
                    | crate::kemono::Target::Discord {
                        channel: Some(_),
                        ..
                    })
            )
        {
            return false;
        }
    }
    true
}

fn kemono_txt_resource(media: &Media) -> bool {
    matches!(media.category.as_str(), "kemono" | "coomer")
        && media
            .url
            .path()
            .rsplit('.')
            .next()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("txt"))
        && media
            .filename
            .rsplit('.')
            .next()
            .is_some_and(|ext| !ext.eq_ignore_ascii_case("txt"))
}

async fn validate_kemono_partial(
    media: &Media,
    partial: &mut crate::resume::Partial,
) -> std::result::Result<(), Failure> {
    if kemono_txt_resource(media)
        && partial
            .has_content(b"not found")
            .await
            .map_err(|error| Failure {
                error,
                retry: false,
                fallback: false,
            })?
    {
        partial.reset(None).await.map_err(|error| Failure {
            error,
            retry: false,
            fallback: false,
        })?;
        return Err(terminal("Kemono media returned a missing-file placeholder"));
    }
    Ok(())
}

fn check_status(
    client: &DownloadClient,
    media: &Media,
    response: &reqwest::Response,
    allow_partial: bool,
) -> std::result::Result<(), Failure> {
    if response.status() != reqwest::StatusCode::OK
        && !(allow_partial && response.status() == reqwest::StatusCode::PARTIAL_CONTENT)
        && !expected_status(media, response.status())
    {
        let retry = response.status().is_server_error()
            || response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS;
        if retry && let Some(delay) = retry_after(response.headers(), SystemTime::now()) {
            // Record even on the final attempt so other workers honor the server.
            client.rate_limits.defer(&media.url, delay);
        }
        return Err(Failure {
            error: error(format!("media server returned HTTP {}", response.status())),
            retry,
            fallback: true,
        });
    }
    Ok(())
}

/// Upstream `_http_expected_status`: extra status codes that count as OK.
fn expected_status(media: &Media, status: reqwest::StatusCode) -> bool {
    media
        .handoff
        .get("_http_expected_status")
        .and_then(|codes| codes.as_array())
        .is_some_and(|codes| {
            codes
                .iter()
                .any(|code| code.as_u64() == Some(u64::from(status.as_u16())))
        })
}

fn validate_media(response: &reqwest::Response) -> std::result::Result<(), Failure> {
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if matches!(
        content_type.as_str(),
        "text/html"
            | "application/xhtml+xml"
            | "application/dash+xml"
            | "application/vnd.apple.mpegurl"
            | "application/x-mpegurl"
            | "multipart/byteranges"
    ) || crate::extract::manifest_protocol(response.url()).is_some()
    {
        return Err(Failure {
            error: error(
                "server returned a web page or streaming manifest; only direct media downloads are implemented",
            ),
            retry: false,
            fallback: true,
        });
    }
    Ok(())
}

fn terminal(message: &str) -> Failure {
    Failure {
        error: error(message),
        retry: false,
        fallback: true,
    }
}

#[allow(clippy::too_many_arguments)]
async fn attempt_resume(
    client: &DownloadClient,
    media: &Media,
    headers: &HeaderMap,
    path: &Path,
    partial: &mut crate::resume::Partial,
    limits: SizeLimits,
    mtime: Mtime,
    io: &Io,
) -> std::result::Result<Outcome, Failure> {
    let mut restarted = false;
    loop {
        let offset = match partial.len().await {
            Ok(offset) => offset,
            Err(error) => {
                // A timed-out blocking publication may have completed before
                // this retry acquired the checkpoint's operation guard.
                if std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file()) {
                    return Ok(Outcome::Skipped(path.into()));
                }
                return Err(Failure {
                    error,
                    retry: false,
                    fallback: false,
                });
            }
        };
        let checkpoint = partial
            .checkpoint()
            .filter(|checkpoint| {
                offset > 0
                    && checkpoint.total.is_none_or(|total| offset <= total)
                    && crate::range::if_range(checkpoint).is_some()
            })
            .cloned();
        let mut headers = headers.clone();
        headers.insert(
            reqwest::header::ACCEPT_ENCODING,
            HeaderValue::from_static("identity"),
        );
        if let Some(checkpoint) = &checkpoint {
            headers.insert(
                reqwest::header::RANGE,
                HeaderValue::from_str(&format!("bytes={offset}-")).unwrap(),
            );
            headers.insert(
                reqwest::header::IF_RANGE,
                HeaderValue::from_str(crate::range::if_range(checkpoint).unwrap()).map_err(
                    |_| Failure {
                        error: error("invalid saved media validator"),
                        retry: false,
                        fallback: false,
                    },
                )?,
            );
        }
        io.set_remote(true);
        let response = request_media(client, media, &headers).await;
        io.set_remote(false);
        let response = response?;
        if !(response.status() == reqwest::StatusCode::RANGE_NOT_SATISFIABLE
            && checkpoint.is_some())
        {
            check_status(client, media, &response, checkpoint.is_some())?;
        }
        let plan = crate::range::plan(&response, offset, checkpoint.as_ref()).map_err(|error| {
            Failure {
                error,
                retry: false,
                fallback: true,
            }
        })?;
        let (start, expected, total) = match plan {
            crate::range::Plan::Restart if !restarted => {
                partial.reset(None).await.map_err(|error| Failure {
                    error,
                    retry: false,
                    fallback: false,
                })?;
                restarted = true;
                continue;
            }
            crate::range::Plan::Restart => {
                return Err(terminal(
                    "media representation changed repeatedly during resume",
                ));
            }
            crate::range::Plan::Complete => {
                if let Some(rejection) = limits.check(offset) {
                    return filter_partial(partial, rejection).await;
                }
                // A 416 completion carries no representation headers; use
                // the saved date validator when it is the only source.
                let saved = match partial.checkpoint().map(|c| &c.validator) {
                    Some(crate::resume::Validator::Modified(value)) if mtime.http => {
                        httpdate::parse_http_date(value).ok()
                    }
                    _ => None,
                };
                let modified = mtime.fixed.or(saved);
                validate_kemono_partial(media, partial).await?;
                return partial
                    .publish(path, modified)
                    .await
                    .map_err(|error| Failure {
                        error,
                        retry: false,
                        fallback: false,
                    });
            }
            crate::range::Plan::Full(checkpoint) => {
                validate_media(&response)?;
                if let Some(rejection) = response
                    .content_length()
                    .and_then(|size| limits.check(size))
                {
                    return filter_partial(partial, rejection).await;
                }
                partial.reset(checkpoint).await.map_err(|error| Failure {
                    error,
                    retry: false,
                    fallback: false,
                })?;
                (0, response.content_length(), response.content_length())
            }
            crate::range::Plan::Append { end, total } => {
                validate_media(&response)?;
                if let Some(rejection) = limits.check(total) {
                    return filter_partial(partial, rejection).await;
                }
                if partial
                    .checkpoint()
                    .is_some_and(|checkpoint| checkpoint.total.is_none())
                {
                    partial.set_total(total).await.map_err(|error| Failure {
                        error,
                        retry: false,
                        fallback: false,
                    })?;
                }
                (offset, Some(end - offset + 1), Some(total))
            }
        };
        client.capture_http_mtime(response.headers());
        let modified = mtime.select(response.headers());
        let mut stream = response.bytes_stream();
        let mut received = 0u64;
        loop {
            io.set_remote(true);
            let chunk = stream.next().await;
            io.set_remote(false);
            let Some(chunk) = chunk else { break };
            let chunk = chunk?;
            received = received
                .checked_add(chunk.len() as u64)
                .ok_or_else(|| terminal("media response exceeds size limit"))?;
            if expected.is_some_and(|expected| received > expected) {
                return Err(terminal("media body exceeds declared range length"));
            }
            let size = start
                .checked_add(received)
                .ok_or_else(|| terminal("media response exceeds size limit"))?;
            if let Some(rejection) = limits.check_max(size) {
                return filter_partial(partial, rejection).await;
            }
            partial.append(&chunk).await.map_err(|error| Failure {
                error,
                retry: false,
                fallback: false,
            })?;
            io.throttle(chunk.len()).await;
        }
        if expected.is_some_and(|expected| received != expected) {
            return Err(Failure {
                error: error("incomplete response body"),
                retry: true,
                fallback: true,
            });
        }
        let completed = start
            .checked_add(received)
            .ok_or_else(|| terminal("media response exceeds size limit"))?;
        if total.is_none_or(|total| completed == total) {
            if let Some(rejection) = limits.check(completed) {
                return filter_partial(partial, rejection).await;
            }
            validate_kemono_partial(media, partial).await?;
            return partial
                .publish(path, modified)
                .await
                .map_err(|error| Failure {
                    error,
                    retry: false,
                    fallback: false,
                });
        }
        // Some servers return only part of an open-ended range. Continue within
        // this attempt's existing deadline; never publish a truncated segment.
    }
}

async fn filter_partial(
    partial: &mut crate::resume::Partial,
    rejection: SizeRejection,
) -> std::result::Result<Outcome, Failure> {
    partial.reset(None).await.map_err(|error| Failure {
        error,
        retry: false,
        fallback: false,
    })?;
    Ok(Outcome::Filtered(rejection))
}

#[cfg(test)]
mod kemono_redirect_tests {
    use super::*;
    use reqwest::{
        StatusCode,
        header::{ACCEPT_ENCODING, AUTHORIZATION, COOKIE, LOCATION, REFERER, USER_AGENT},
    };

    fn media() -> Media {
        Media {
            url: url::Url::parse("https://kemono.cr/data/aa/bb/file.jpg").unwrap(),
            job: MediaJob::DirectHttp,
            category: "kemono".into(),
            filename: "file.jpg".into(),
            metadata: None,
            headers: Default::default(),
            handoff: Default::default(),
        }
    }
    #[test]
    fn public_kemono_headers_follow_cdn_redirects_but_credentials_do_not() {
        let media = media();
        let mut headers = HeaderMap::new();
        headers.insert(
            REFERER,
            HeaderValue::from_static("https://kemono.cr/patreon/user/42/post/7"),
        );
        headers.insert(
            USER_AGENT,
            HeaderValue::from_static("gallery-dl-rs fixture"),
        );
        headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
        let mut location = HeaderMap::new();
        location.insert(
            LOCATION,
            HeaderValue::from_static("https://n1.kemono.cr/data/aa/bb/file.jpg"),
        );
        let mut redirects =
            Redirects::new(&media.url).allow_subdomains(kemono_public_redirects(&media, &headers));
        assert!(
            redirects
                .next(&media.url, StatusCode::FOUND, &location)
                .unwrap()
                .is_some()
        );
        for credential in [COOKIE, AUTHORIZATION, HeaderName::from_static("x-api-key")] {
            let mut headers = headers.clone();
            headers.insert(credential, HeaderValue::from_static("PRIVATE_VALUE"));
            let mut redirects = Redirects::new(&media.url)
                .allow_subdomains(kemono_public_redirects(&media, &headers));
            assert!(
                redirects
                    .next(&media.url, StatusCode::FOUND, &location)
                    .is_err()
            );
        }
    }
    #[test]
    fn discord_public_referers_allow_site_cdn_redirects_without_cookies() {
        let media = media();
        for referer in [
            "https://kemono.cr/discord/server/100/200",
            "https://kemono.cr/discord/server/100/channel/200",
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(REFERER, HeaderValue::from_str(referer).unwrap());
            let mut location = HeaderMap::new();
            location.insert(
                LOCATION,
                HeaderValue::from_static("https://n1.kemono.cr/data/aa/bb/file.jpg"),
            );
            let mut redirects = Redirects::new(&media.url)
                .allow_subdomains(kemono_public_redirects(&media, &headers));
            assert!(
                redirects
                    .next(&media.url, StatusCode::FOUND, &location)
                    .unwrap()
                    .is_some()
            );
            headers.insert(COOKIE, HeaderValue::from_static("PRIVATE_SESSION"));
            assert!(!kemono_public_redirects(&media, &headers));
        }
        for referer in [
            "https://kemono.cr/discord/server/100",
            "https://kemono.cr/discord/server/100/200?private=token",
            "https://kemono.cr/discord/server/100#200",
            "https://coomer.st/discord/server/100/200",
            "https://kemono.cr/discord/server/100/not-a-channel",
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(REFERER, HeaderValue::from_str(referer).unwrap());
            assert!(!kemono_public_redirects(&media, &headers));
        }
    }
    #[test]
    fn kemono_redirect_allowance_rejects_private_or_foreign_referers_and_other_origins() {
        let mut media = media();
        for referer in [
            "https://kemono.cr/patreon/user/42/post/7?token=private",
            "https://private@kemono.cr/patreon/user/42/post/7",
            "https://kemono.cr/patreon/user/42/post/7#private",
            "https://coomer.st/onlyfans/user/42/post/7",
            "https://evil.test/patreon/user/42/post/7",
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(REFERER, HeaderValue::from_str(referer).unwrap());
            assert!(!kemono_public_redirects(&media, &headers));
        }
        for url in [
            "https://evil.test/file.jpg",
            "https://kemono.cr.evil.test/file.jpg",
        ] {
            media.url = url::Url::parse(url).unwrap();
            assert!(!kemono_public_redirects(&media, &HeaderMap::new()));
        }
    }
}
