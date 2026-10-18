use crate::{
    Result, error,
    extract::{Media, MediaJob},
    naming::{DirectoryTemplate, FilenameTemplate},
    transport::{RateLimits, Redirects, retry_after},
};
use futures_util::StreamExt;
use reqwest::{
    Client,
    header::{HeaderMap, HeaderName, HeaderValue},
};
use sha2::{Digest, Sha256};
use std::{
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
}

impl DownloadClient {
    /// Override the per-attempt deadline while sharing connections and cooldowns.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.wait_limit = timeout;
        self
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
    Ok(root
        .join(safe_component(&media.category))
        .join(template.render(media)?))
}

/// Render a complete destination. An explicit directory replaces the default
/// category component while the basename follows the normal filename policy.
pub fn destination_with_templates(
    root: &Path,
    media: &Media,
    filename: Option<&FilenameTemplate>,
    directory: Option<&DirectoryTemplate>,
) -> Result<PathBuf> {
    let default = match filename {
        Some(template) => destination_with_template(root, media, template)?,
        None => destination(root, media),
    };
    match directory {
        None => Ok(default),
        Some(directory) => Ok(root.join(directory.render(media)?).join(
            default
                .file_name()
                .ok_or_else(|| error("invalid media filename"))?,
        )),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Downloaded(PathBuf),
    Skipped(PathBuf),
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DownloadOptions<'a> {
    pub retries: u8,
    /// Override category folders with per-media directory components.
    pub directory: Option<&'a DirectoryTemplate>,
    /// Save versioned post metadata beside completed or already existing media.
    pub write_metadata: bool,
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
    let outcome = download_media(
        client,
        root,
        media,
        options.retries,
        template,
        options.directory,
    )
    .await?;
    if options.write_metadata {
        let (Outcome::Downloaded(path) | Outcome::Skipped(path)) = &outcome;
        write_metadata(path, media).await.map_err(|e| {
            error(format!(
                "media is saved at {}; metadata sidecar failed: {e}",
                path.display()
            ))
        })?;
    }
    Ok(outcome)
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
    let mut bytes = serde_json::to_vec_pretty(&MetadataSidecar {
        schema_version: 1,
        category: &media.category,
        filename,
        metadata: media.metadata.as_ref(),
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
    retries: u8,
    template: Option<&FilenameTemplate>,
    directory: Option<&DirectoryTemplate>,
) -> Result<Outcome> {
    match &media.job {
        MediaJob::DirectHttp => {
            crate::extract::http_url(media.url.as_str())?;
        }
        MediaJob::VideoPage { .. } => {
            return Err(error(
                "video-page downloads require the companion yt-dlp-rs adapter, which is not implemented",
            ));
        }
        MediaJob::Manifest { .. } => {
            return Err(error(
                "HLS/DASH downloads require the companion yt-dlp-rs adapter, which is not implemented",
            ));
        }
    }
    let path = destination_with_templates(root, media, template, directory)?;
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() => return Ok(Outcome::Skipped(path)),
        Ok(_) => return Err(error("media destination is not a regular file")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let parent = path.parent().ok_or_else(|| error("invalid destination"))?;
    tokio::fs::create_dir_all(parent).await?;
    let mut headers = HeaderMap::new();
    for (name, value) in &media.headers {
        let name = HeaderName::from_bytes(name.as_bytes())?;
        // Transport framing and proxy credentials must be controlled by reqwest.
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
    let mut partial = if client.resume {
        Some(crate::resume::Partial::open(&path, &media.identity()).await?)
    } else {
        None
    };
    for attempt in 0..=retries {
        let result = tokio::time::timeout(client.wait_limit, async {
            match &mut partial {
                Some(partial) => attempt_resume(client, media, &headers, &path, partial).await,
                None => attempt_download(client, media, &headers, parent, &path).await,
            }
        })
        .await
        .unwrap_or_else(|_| {
            Err(Failure {
                error: error("media download attempt timed out"),
                retry: true,
            })
        });
        match result {
            Ok(outcome) => return Ok(outcome),
            Err(failure) if failure.retry && attempt < retries => {
                tokio::time::sleep(Duration::from_millis(250 * (1u64 << attempt.min(6)))).await;
            }
            Err(failure) => return Err(failure.error),
        }
    }
    unreachable!()
}

struct Failure {
    error: crate::Error,
    retry: bool,
}
impl From<std::io::Error> for Failure {
    fn from(error: std::io::Error) -> Self {
        Self {
            error: error.into(),
            retry: false,
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
        }
    }
}

async fn attempt_download(
    client: &DownloadClient,
    media: &Media,
    headers: &HeaderMap,
    parent: &Path,
    path: &Path,
) -> std::result::Result<Outcome, Failure> {
    let response = request_media(client, media, headers).await?;
    check_status(client, media, &response, false)?;
    if response
        .headers()
        .contains_key(reqwest::header::CONTENT_RANGE)
    {
        return Err(terminal("unexpected Content-Range in full media response"));
    }
    let expected = response.content_length();
    validate_media(&response)?;
    // A unique temporary file prevents concurrent workers from sharing a partial
    // file. Dropping TempPath removes incomplete downloads, including on failure.
    let temporary = tempfile::NamedTempFile::new_in(parent)?.into_temp_path();
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&temporary)
        .await?;
    let mut stream = response.bytes_stream();
    let mut received = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        file.write_all(&chunk).await?;
        received += chunk.len() as u64;
    }
    if expected.is_some_and(|length| length != received) {
        return Err(Failure {
            error: error("incomplete response body"),
            retry: true,
        });
    }
    file.flush().await?;
    file.sync_all().await?;
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
    let mut redirects = Redirects::new(&target);
    let response = loop {
        client
            .rate_limits
            .wait(&target, client.wait_limit)
            .await
            .map_err(|error| Failure {
                error,
                retry: false,
            })?;
        let response = client
            .http
            .get(target.clone())
            .timeout(client.wait_limit)
            .headers(headers.clone())
            .send()
            .await?;
        match redirects
            .next(response.url(), response.status(), response.headers())
            .map_err(|error| Failure {
                error,
                retry: false,
            })? {
            Some(next) => target = next,
            None => break response,
        }
    };
    Ok(response)
}

fn check_status(
    client: &DownloadClient,
    media: &Media,
    response: &reqwest::Response,
    allow_partial: bool,
) -> std::result::Result<(), Failure> {
    if response.status() != reqwest::StatusCode::OK
        && !(allow_partial && response.status() == reqwest::StatusCode::PARTIAL_CONTENT)
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
        });
    }
    Ok(())
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
        });
    }
    Ok(())
}

fn terminal(message: &str) -> Failure {
    Failure {
        error: error(message),
        retry: false,
    }
}

async fn attempt_resume(
    client: &DownloadClient,
    media: &Media,
    headers: &HeaderMap,
    path: &Path,
    partial: &mut crate::resume::Partial,
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
                HeaderValue::from_str(crate::range::if_range(checkpoint).unwrap())
                    .map_err(|_| terminal("invalid saved media validator"))?,
            );
        }
        let response = request_media(client, media, &headers).await?;
        if !(response.status() == reqwest::StatusCode::RANGE_NOT_SATISFIABLE
            && checkpoint.is_some())
        {
            check_status(client, media, &response, checkpoint.is_some())?;
        }
        let plan = crate::range::plan(&response, offset, checkpoint.as_ref()).map_err(|error| {
            Failure {
                error,
                retry: false,
            }
        })?;
        let (start, expected, total) = match plan {
            crate::range::Plan::Restart if !restarted => {
                partial.reset(None).await.map_err(|error| Failure {
                    error,
                    retry: false,
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
                return partial.publish(path).await.map_err(|error| Failure {
                    error,
                    retry: false,
                });
            }
            crate::range::Plan::Full(checkpoint) => {
                validate_media(&response)?;
                partial.reset(checkpoint).await.map_err(|error| Failure {
                    error,
                    retry: false,
                })?;
                (0, response.content_length(), response.content_length())
            }
            crate::range::Plan::Append { end, total } => {
                validate_media(&response)?;
                if partial
                    .checkpoint()
                    .is_some_and(|checkpoint| checkpoint.total.is_none())
                {
                    partial.set_total(total).await.map_err(|error| Failure {
                        error,
                        retry: false,
                    })?;
                }
                (offset, Some(end - offset + 1), Some(total))
            }
        };
        let mut stream = response.bytes_stream();
        let mut received = 0u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            received = received
                .checked_add(chunk.len() as u64)
                .ok_or_else(|| terminal("media response exceeds size limit"))?;
            if expected.is_some_and(|expected| received > expected) {
                return Err(terminal("media body exceeds declared range length"));
            }
            partial.append(&chunk).await.map_err(|error| Failure {
                error,
                retry: false,
            })?;
        }
        if expected.is_some_and(|expected| received != expected) {
            return Err(Failure {
                error: error("incomplete response body"),
                retry: true,
            });
        }
        let completed = start
            .checked_add(received)
            .ok_or_else(|| terminal("media response exceeds size limit"))?;
        if total.is_none_or(|total| completed == total) {
            return partial.publish(path).await.map_err(|error| Failure {
                error,
                retry: false,
            });
        }
        // Some servers return only part of an open-ended range. Continue within
        // this attempt's existing deadline; never publish a truncated segment.
    }
}
