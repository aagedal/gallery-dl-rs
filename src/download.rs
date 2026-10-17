use crate::{
    Result, error,
    extract::{Media, MediaJob},
    transport::{RateLimits, retry_after},
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
            // Media URLs should already be resolved. Avoid forwarding extracted secrets
            // to an unexpected host via redirect.
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

pub async fn download(
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
    if path.try_exists()? {
        return Ok(Outcome::Skipped(path));
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
        client
            .rate_limits
            .wait(&media.url, client.wait_limit)
            .await?;
        match attempt_download(client, media, &headers, parent, &path).await {
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
            error: error.into(),
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
    let response = client
        .http
        .get(media.url.clone())
        .headers(headers.clone())
        .send()
        .await?;
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
    ) || crate::extract::manifest_protocol(&media.url).is_some()
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
            Ok(Outcome::Skipped(path.into()))
        }
        Err(e) => Err(e.error.into()),
    }
}
