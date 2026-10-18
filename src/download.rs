use crate::{
    Result, error,
    extract::{Media, MediaJob},
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

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Downloaded(PathBuf),
    Skipped(PathBuf),
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DownloadOptions {
    pub retries: u8,
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
    options: DownloadOptions,
) -> Result<Outcome> {
    let outcome = download_media(client, root, media, options.retries).await?;
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
    let path = destination(root, media);
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
            "host" | "content-length" | "transfer-encoding" | "connection" | "proxy-authorization"
        ) {
            continue;
        }
        headers.insert(name, HeaderValue::from_str(value)?);
    }
    for attempt in 0..=retries {
        let result = tokio::time::timeout(
            client.wait_limit,
            attempt_download(client, media, &headers, parent, &path),
        )
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
    if !response.status().is_success() {
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
    let expected = response.content_length();
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
    ) || crate::extract::manifest_protocol(response.url()).is_some()
    {
        return Err(Failure {
            error: error(
                "server returned a web page or streaming manifest; only direct media downloads are implemented",
            ),
            retry: false,
        });
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
