//! Native direct-link extraction and a temporary social-site compatibility layer.
use crate::{Result, error};
use futures_util::{TryStreamExt, stream::BoxStream};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
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
    /// A durable archive identity, separate from request-level deduplication.
    /// Social IDs never derive from signed URLs or private transport options.
    /// Direct links explicitly use the full request identity, hashed for privacy.
    pub fn archive_key(&self) -> Option<String> {
        let (namespace, identity) = match self.category.as_str() {
            "direct" => ("url", self.identity()),
            category @ ("instagram" | "twitter" | "facebook") => {
                let post = self.metadata.as_ref()?;
                if !decimal_id(&post.post_id) {
                    return None;
                }
                let valid_media = match category {
                    "instagram" => {
                        decimal_id(post.media_id.strip_suffix('p').unwrap_or(&post.media_id))
                    }
                    "twitter" => {
                        decimal_id(&post.media_id)
                            && post.index > 0
                            && post.media_id == post.index.to_string()
                    }
                    "facebook" => post
                        .media_id
                        .split_once('.')
                        .is_some_and(|(id, extension)| {
                            id == post.post_id && valid_extension(extension)
                        }),
                    _ => unreachable!(),
                };
                if !valid_media {
                    return None;
                }
                (
                    category,
                    serde_json::json!([post.post_id, post.media_id]).to_string(),
                )
            }
            _ => return None,
        };
        Some(format!(
            "{namespace}:v1:{:x}",
            Sha256::digest(identity.as_bytes())
        ))
    }

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
    /// Site media ID; Twitter uses the upstream file number and Facebook uses
    /// `id.extension` because separate audio and video files share an ID.
    pub media_id: String,
    /// Instagram post shortcode; empty for sites without Instagram shortcodes.
    pub shortcode: String,
    pub username: Option<String>,
    pub caption: Option<String>,
    pub timestamp: Option<i64>,
    pub index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retweet_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quoted_by_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reposted_by_id: Option<String>,
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

/// Runtime overrides passed as individual arguments to the compatibility process.
#[derive(Clone, Default)]
pub struct BackendOptions {
    pub browser: Option<String>,
    pub proxy: Option<String>,
    pub user_agent: Option<String>,
    pub retweets: Option<crate::twitter::RetweetMode>,
    pub quoted: Option<bool>,
}
impl std::fmt::Debug for BackendOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BackendOptions { settings: [redacted] }")
    }
}

impl Backend {
    /// Collect all results. Use `stream` for bounded per-item consumption.
    pub async fn extract(&self, url: &Url) -> Result<Vec<Media>> {
        self.stream(url).try_collect().await
    }

    /// Incrementally parse the resolved JSON array without buffering a collection.
    /// Dropping the stream stops the directly owned compatibility process.
    pub fn stream(&self, url: &Url) -> BoxStream<'static, Result<Media>> {
        crate::backend::stream(self.clone(), url.clone())
    }

    /// Let the compatibility executable import its own browser session. No
    /// browser cookie values are read or exported by Rust on this path.
    pub fn stream_with_browser(
        &self,
        url: &Url,
        specification: &str,
    ) -> BoxStream<'static, Result<Media>> {
        crate::backend::stream_with_browser(self.clone(), url.clone(), Some(specification.into()))
    }

    pub fn stream_with_options(
        &self,
        url: &Url,
        options: BackendOptions,
    ) -> BoxStream<'static, Result<Media>> {
        crate::backend::stream_with_options(self.clone(), url.clone(), options)
    }

    pub(crate) fn command(&self, url: &Url, options: &BackendOptions) -> Command {
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
        if let Some(browser) = &options.browser {
            command.arg("--cookies-from-browser").arg(browser);
        } else if let Some(cookies) = &self.cookies {
            command.arg("--cookies").arg(cookies);
        }
        if let Some(proxy) = &options.proxy {
            command.arg("--proxy").arg(proxy);
        }
        if let Some(agent) = &options.user_agent {
            command.arg("--user-agent").arg(agent);
        }
        if let Some(retweets) = options.retweets {
            let value = match retweets {
                crate::twitter::RetweetMode::Skip => "false",
                crate::twitter::RetweetMode::Repost => "true",
                crate::twitter::RetweetMode::Original => "\"original\"",
            };
            command.arg("--option").arg(format!("retweets={value}"));
        }
        if let Some(quoted) = options.quoted {
            command.arg("--option").arg(format!("quoted={quoted}"));
        }
        command.args([
            "--option",
            "output.private=true",
            "--option",
            "output.jsonl=false",
        ]);
        command.arg("--").arg(url.as_str());
        command.stdin(Stdio::null()).kill_on_drop(true);
        command
    }
}

/// gallery-dl --resolve-json emits an array of [message_type, ..., metadata].
pub fn parse_messages(bytes: &[u8], category: &str) -> Result<Vec<Media>> {
    let messages: Vec<Value> = serde_json::from_slice(bytes)
        .map_err(|e| error(format!("invalid gallery-dl JSON: {e}")))?;
    let mut items = Vec::new();
    for message in messages {
        if let Some(item) = parse_message(message, category)? {
            items.push(item);
        }
    }
    if items.is_empty() {
        return Err(error(
            "no downloadable media found; the post may require login or be unsupported",
        ));
    }
    Ok(items)
}

pub(crate) fn parse_message(message: Value, category: &str) -> Result<Option<Media>> {
    let row = message
        .as_array()
        .ok_or_else(|| error("invalid extractor message"))?;
    match row.first().and_then(Value::as_i64) {
        Some(2) => return Ok(None), // Directory metadata; Rust controls local paths.
        Some(3) => {}
        Some(6) => return Err(error("backend returned an unresolved child URL")),
        Some(-1) => return Err(error("backend reported an extraction error")),
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
    Ok(Some(Media {
        url,
        job,
        category: category.into(),
        filename,
        metadata: compatibility_metadata(metadata, category),
        headers,
        handoff,
    }))
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

fn decimal_id(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && value.bytes().any(|byte| byte != b'0')
}

fn valid_extension(value: &str) -> bool {
    !value.is_empty() && value.len() <= 10 && value.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

/// Allowlist the public fields emitted by upstream gallery-dl extractors.
/// See gallery_dl/extractor/{instagram,twitter,facebook}.py: Instagram uses
/// post_id/media_id, Twitter tweet_id/num, Facebook id/extension (including
/// separate audio and video files with the same id). Missing IDs stay empty;
/// they never acquire an inferred identity from a URL, filename or row number.
fn compatibility_metadata(
    data: &serde_json::Map<String, Value>,
    category: &str,
) -> Option<PostMetadata> {
    let string = |key: &str| data.get(key).and_then(Value::as_str).map(str::to_owned);
    let id = |key: &str| match data.get(key) {
        Some(Value::String(value)) => value.clone(),
        Some(Value::Number(value)) if value.is_u64() => value.to_string(),
        _ => String::new(),
    };
    let index = data
        .get("num")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(0);
    let (post_id, media_id, shortcode, username, caption) = match category {
        "instagram" => (
            id("post_id"),
            id("media_id"),
            string("post_shortcode").unwrap_or_default(),
            string("username"),
            string("description"),
        ),
        "twitter" => (
            id("tweet_id"),
            if index > 0 {
                index.to_string()
            } else {
                String::new()
            },
            String::new(),
            data.get("author")
                .and_then(|author| author.get("name"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            string("content"),
        ),
        "facebook" => {
            let post_id = id("id");
            let media_id = match string("extension").filter(|extension| valid_extension(extension))
            {
                Some(extension) if !post_id.is_empty() => format!("{post_id}.{extension}"),
                _ => String::new(),
            };
            (
                post_id,
                media_id,
                String::new(),
                string("username"),
                string("caption"),
            )
        }
        _ => return None,
    };
    let context_id = |key: &str| {
        let value = id(key);
        (category == "twitter"
            && value
                .parse::<u64>()
                .is_ok_and(|n| n > 0 && n.to_string() == value))
        .then_some(value)
    };
    Some(PostMetadata {
        retweet_id: context_id("retweet_id"),
        quoted_by_id: context_id("quote_id"),
        reposted_by_id: None,
        post_id,
        media_id,
        shortcode,
        username,
        caption,
        timestamp: data.get("date").and_then(|date| match date {
            Value::String(value) => crate::date::parse_date(value).ok(),
            value => value.as_i64(),
        }),
        index,
    })
}
