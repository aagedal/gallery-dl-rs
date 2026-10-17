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
    pub job: MediaJob,
    pub category: String,
    pub filename: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<PostMetadata>,
    #[serde(skip_serializing)]
    pub headers: BTreeMap<String, String>,
    /// Opaque compatibility options for a future adapter, never public JSON.
    #[serde(skip_serializing)]
    pub handoff: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MediaJob {
    DirectHttp,
    VideoPage {
        page_url: Url,
    },
    Manifest {
        protocol: ManifestProtocol,
        base_url: Url,
        #[serde(skip_serializing)]
        inline_data: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ManifestProtocol {
    Hls,
    Dash,
}

impl Media {
    /// Include private request/manifest context so distinct jobs are not lost.
    pub fn identity(&self) -> String {
        // Preserve existing paths for ordinary direct links without request context.
        if matches!(self.job, MediaJob::DirectHttp)
            && self.headers.is_empty()
            && self.handoff.is_empty()
        {
            return self.url.to_string();
        }
        let inline_data = match &self.job {
            MediaJob::Manifest { inline_data, .. } => inline_data.as_deref(),
            _ => None,
        };
        serde_json::json!([self.url, self.job, inline_data, self.headers, self.handoff]).to_string()
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct PostMetadata {
    pub post_id: String,
    pub media_id: String,
    pub shortcode: String,
    pub username: Option<String>,
    pub caption: Option<String>,
    pub timestamp: Option<i64>,
    pub index: usize,
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
    let manifest = manifest_protocol(url);
    if manifest.is_none()
        && ![
            "jpg", "jpeg", "png", "gif", "webp", "avif", "heic", "bmp", "svg", "mp4", "webm",
            "mov", "m4v", "mkv", "mp3", "ogg", "wav", "zip", "pdf",
        ]
        .contains(&extension.to_ascii_lowercase().as_str())
    {
        return None;
    }
    Some(Media {
        url: url.clone(),
        job: match manifest {
            Some(protocol) => MediaJob::Manifest {
                protocol,
                base_url: url.clone(),
                inline_data: None,
            },
            None => MediaJob::DirectHttp,
        },
        category: "direct".into(),
        filename: filename.into(),
        metadata: None,
        headers: BTreeMap::new(),
        handoff: BTreeMap::new(),
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
        let metadata = row
            .get(2)
            .and_then(Value::as_object)
            .ok_or_else(|| error("media message has no metadata"))?;
        let delegated = source.strip_prefix("ytdl:");
        let target = http_url(delegated.unwrap_or(source)).map_err(|_| {
            error("backend media and video handoffs require an HTTP(S) target/base URL")
        })?;
        let url = if delegated.is_some() {
            Url::parse(source)?
        } else {
            target.clone()
        };
        let marker = metadata.get("_ytdl_manifest").filter(|v| !v.is_null());
        let inline_data = metadata.get("_ytdl_manifest_data").filter(|v| !v.is_null());
        if marker.is_none() && inline_data.is_some() {
            return Err(error("inline manifest data requires _ytdl_manifest"));
        }
        let job = if let Some(marker) = marker {
            let protocol = match marker.as_str() {
                Some("hls") => ManifestProtocol::Hls,
                Some("dash") => ManifestProtocol::Dash,
                _ => return Err(error("unsupported backend manifest type")),
            };
            let inline_data = inline_data
                .map(|data| {
                    data.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| error("backend inline manifest data must be a string"))
                })
                .transpose()?;
            MediaJob::Manifest {
                protocol,
                base_url: target,
                inline_data,
            }
        } else if delegated.is_some() {
            MediaJob::VideoPage { page_url: target }
        } else if let Some(protocol) = manifest_protocol(&target) {
            MediaJob::Manifest {
                protocol,
                base_url: target,
                inline_data: None,
            }
        } else {
            MediaJob::DirectHttp
        };
        let handoff = metadata
            .iter()
            .filter(|(key, _)| key.starts_with("_ytdl_") || key.as_str() == "_fallback")
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
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
            job,
            category: category.into(),
            filename,
            metadata: None,
            headers,
            handoff,
        });
    }
    if items.is_empty() {
        return Err(error(
            "no downloadable media found; the post may require login or be unsupported",
        ));
    }
    Ok(items)
}

pub fn manifest_protocol(url: &Url) -> Option<ManifestProtocol> {
    let path = url.path().to_ascii_lowercase();
    if path.ends_with(".m3u8") {
        Some(ManifestProtocol::Hls)
    } else if path.ends_with(".mpd") {
        Some(ManifestProtocol::Dash)
    } else {
        None
    }
}
