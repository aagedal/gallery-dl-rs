//! Native direct-link extraction and a temporary social-site compatibility layer.
use crate::{Result, error};
use serde::Serialize;
use serde_json::Value;
use std::{collections::BTreeMap, path::PathBuf, process::Stdio, time::Duration};
use tokio::process::Command;
use url::Url;

#[derive(Clone, Debug, Serialize)]
pub struct Media {
    pub url: Url,
    pub category: String,
    pub filename: String,
    #[serde(skip_serializing)]
    pub headers: BTreeMap<String, String>,
}

#[derive(Clone, Debug)]
pub struct Backend {
    pub executable: PathBuf,
    pub cookies: Option<PathBuf>,
    pub config: Option<PathBuf>,
    pub timeout: Duration,
}

pub fn http_url(value: &str) -> Result<Url> {
    let url = Url::parse(value)?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(error("only HTTP(S) URLs are supported"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(error("URLs containing credentials are not supported"));
    }
    Ok(url)
}

pub fn social_site(url: &Url) -> Option<&'static str> {
    let host = url.host_str()?;
    for (domain, category) in [
        ("instagram.com", "instagram"),
        ("facebook.com", "facebook"),
        ("fb.watch", "facebook"),
        ("twitter.com", "twitter"),
        ("x.com", "twitter"),
    ] {
        if host == domain || host.ends_with(&format!(".{domain}")) {
            return Some(category);
        }
    }
    None
}

pub fn direct(url: &Url) -> Option<Media> {
    let filename = url.path_segments()?.next_back()?;
    let (_, extension) = filename.rsplit_once('.')?;
    if ![
        "jpg", "jpeg", "png", "gif", "webp", "avif", "heic", "bmp", "svg", "mp4", "webm", "mov",
        "m4v", "mkv", "mp3", "ogg", "wav", "zip", "pdf",
    ]
    .contains(&extension.to_ascii_lowercase().as_str())
    {
        return None;
    }
    Some(Media {
        url: url.clone(),
        category: "direct".into(),
        filename: filename.into(),
        headers: BTreeMap::new(),
    })
}

impl Backend {
    pub async fn extract(&self, url: &Url) -> Result<Vec<Media>> {
        let category = social_site(url).ok_or_else(|| {
            error("unsupported URL; use an Instagram, Facebook, Twitter/X URL or direct media link")
        })?;
        // Ignore user defaults: extraction must not trigger configured hooks or downloads.
        let mut command = Command::new(&self.executable);
        command.args([
            "--config-ignore",
            "--no-input",
            "--no-colors",
            "--no-download",
            "--no-postprocessors",
            "--resolve-json",
        ]);
        if let Some(config) = &self.config {
            command.arg("--config").arg(config);
        }
        if let Some(cookies) = &self.cookies {
            command.arg("--cookies").arg(cookies);
        }
        command.args([
            "--option",
            "output.private=true",
            "--option",
            "output.jsonl=false",
        ]);
        command.arg("--").arg(url.as_str());
        command.stdin(Stdio::null()).kill_on_drop(true);
        let output = tokio::time::timeout(self.timeout, command.output())
            .await
            .map_err(|_| error("social extraction timed out"))?
            .map_err(|e| {
                error(format!(
                    "could not run {}: {e}. Install gallery-dl or set --extractor-command",
                    self.executable.display(),
                ))
            })?;
        if !output.status.success() {
            return Err(error(format!(
                "{category} extraction failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        parse_messages(&output.stdout, category)
    }
}

/// gallery-dl --resolve-json emits an array of [message_type, ..., metadata].
pub fn parse_messages(bytes: &[u8], category: &str) -> Result<Vec<Media>> {
    let messages: Vec<Value> = serde_json::from_slice(bytes)
        .map_err(|e| error(format!("invalid gallery-dl JSON: {e}")))?;
    let mut items = Vec::new();
    for message in messages {
        let row = message
            .as_array()
            .ok_or_else(|| error("invalid extractor message"))?;
        match row.first().and_then(Value::as_i64) {
            Some(2) => continue, // Directory metadata; Rust controls local paths.
            Some(3) => {}
            Some(6) => return Err(error("backend returned an unresolved child URL")),
            Some(-1) => {
                return Err(error(format!(
                    "backend extraction error: {}",
                    row.get(1).unwrap_or(&Value::Null)
                )));
            }
            _ => return Err(error("unsupported extractor message type")),
        }
        let source = row
            .get(1)
            .and_then(Value::as_str)
            .ok_or_else(|| error("media message has no URL"))?;
        let url = http_url(source).map_err(|_| {
            error(
                "backend returned a non-HTTP media URL (stream/yt-dlp handling is not implemented)",
            )
        })?;
        let metadata = row
            .get(2)
            .and_then(Value::as_object)
            .ok_or_else(|| error("media message has no metadata"))?;
        let mut filename = metadata
            .get("filename")
            .and_then(Value::as_str)
            .unwrap_or("media")
            .to_owned();
        if let Some(extension) = metadata.get("extension").and_then(Value::as_str) {
            filename.push('.');
            filename.push_str(extension);
        }
        let mut headers = BTreeMap::new();
        if let Some(values) = metadata.get("_http_headers").and_then(Value::as_object) {
            for (key, value) in values {
                if let Some(value) = value.as_str() {
                    headers.insert(key.clone(), value.to_owned());
                }
            }
        }
        items.push(Media {
            url,
            category: category.into(),
            filename,
            headers,
        });
    }
    if items.is_empty() {
        return Err(error(
            "no downloadable media found; the post may require login or be unsupported",
        ));
    }
    Ok(items)
}
