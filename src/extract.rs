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
    InlineText {
        #[serde(skip_serializing)]
        data: String,
    },
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
            // Imgur media IDs are global; the album or gallery is not part of it.
            "imgur" => {
                let post = self.metadata.as_ref()?;
                let id = &post.media_id;
                if !matches!(id.len(), 5 | 7) || !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
                    return None;
                }
                ("imgur", serde_json::json!([id]).to_string())
            }
            // Blob CIDs are content addresses scoped to the author's repository.
            "bluesky" => {
                let post = self.metadata.as_ref()?;
                let did = self
                    .url
                    .query_pairs()
                    .find(|(key, _)| key == "did")
                    .map(|(_, did)| did.into_owned())?;
                if !crate::bluesky::valid_did(&did) || !crate::bluesky::valid_cid(&post.media_id) {
                    return None;
                }
                (
                    "bluesky",
                    serde_json::json!([did, post.media_id]).to_string(),
                )
            }
            // Reddit media IDs are scoped to their submission.
            "reddit" => {
                let post = self.metadata.as_ref()?;
                if !crate::reddit::valid_id(&post.post_id)
                    || !crate::reddit::valid_media_id(&post.media_id)
                {
                    return None;
                }
                (
                    "reddit",
                    serde_json::json!([post.post_id, post.media_id]).to_string(),
                )
            }
            // Mastodon attachment IDs are scoped to their instance.
            "mastodon" => {
                let post = self.metadata.as_ref()?;
                let instance = post.instance.as_deref()?;
                if !crate::mastodon::INSTANCES.contains(&instance)
                    || !crate::mastodon::valid_id(&post.media_id)
                {
                    return None;
                }
                (
                    "mastodon",
                    serde_json::json!([instance, post.media_id]).to_string(),
                )
            }
            // Kemono/Coomer file hashes are scoped to their service, creator and post.
            category @ ("kemono" | "coomer") => {
                let post = self.metadata.as_ref()?;
                let service = post.instance.as_deref()?;
                let user = post.username.as_deref()?;
                if crate::kemono::category(self.url.host_str()?) != Some(category)
                    || !crate::kemono::valid_service(service)
                    || !crate::kemono::valid_id(user)
                    || !crate::kemono::valid_id(&post.post_id)
                    || !crate::kemono::valid_hash(&post.media_id)
                {
                    return None;
                }
                (
                    category,
                    serde_json::json!([service, user, post.post_id, post.media_id]).to_string(),
                )
            }
            // Image host file IDs are global per site.
            category @ ("pixeldrain" | "catbox" | "imgbox" | "postimg") => {
                let post = self.metadata.as_ref()?;
                let site = crate::imagehosts::Site::from_category(category)?;
                if !site.media_host(&self.url)
                    || !crate::imagehosts::valid_media_id(site, &post.media_id)
                {
                    return None;
                }
                (category, serde_json::json!([post.media_id]).to_string())
            }
            // Tumblr media IDs are path hashes scoped to their blog and post.
            "tumblr" => {
                let post = self.metadata.as_ref()?;
                let blog = post.username.as_deref()?;
                if !crate::tumblr::valid_blog(blog)
                    || !crate::tumblr::valid_id(&post.post_id)
                    || !crate::tumblr::valid_media_id(&post.media_id)
                {
                    return None;
                }
                (
                    "tumblr",
                    serde_json::json!([blog, post.post_id, post.media_id]).to_string(),
                )
            }
            // Pinterest files are identified by pin ID and index.
            "pinterest" => {
                let post = self.metadata.as_ref()?;
                if !crate::pinterest::valid_id(&post.post_id) {
                    return None;
                }
                (
                    "pinterest",
                    serde_json::json!([post.post_id, post.index]).to_string(),
                )
            }
            // ArtStation files are identified by asset ID.
            "artstation" => {
                let post = self.metadata.as_ref()?;
                if !crate::artstation::valid_id(&post.media_id) {
                    return None;
                }
                ("artstation", serde_json::json!([post.media_id]).to_string())
            }
            // Wallhaven files are identified by wallpaper ID.
            "wallhaven" => {
                let post = self.metadata.as_ref()?;
                if !crate::wallhaven::valid_id(&post.media_id) {
                    return None;
                }
                ("wallhaven", serde_json::json!([post.media_id]).to_string())
            }
            // Flickr files are identified by photo ID.
            "flickr" => {
                let post = self.metadata.as_ref()?;
                if !crate::flickr::valid_id(&post.post_id) {
                    return None;
                }
                ("flickr", serde_json::json!([post.post_id]).to_string())
            }
            // DeviantArt files are identified by deviation UUID.
            "deviantart" => {
                let post = self.metadata.as_ref()?;
                if !crate::deviantart::valid_uuid(&post.post_id) {
                    return None;
                }
                ("deviantart", serde_json::json!([post.post_id]).to_string())
            }
            // Pixiv files are identified by illustration ID and page.
            "pixiv" => {
                let post = self.metadata.as_ref()?;
                if !crate::pixiv::valid_media_id(&post.post_id, &post.media_id) {
                    return None;
                }
                (
                    "pixiv",
                    serde_json::json!([post.post_id, post.media_id]).to_string(),
                )
            }
            // Booru files are identified by their site and post ID.
            category @ ("danbooru" | "e621" | "gelbooru" | "safebooru") => {
                let post = self.metadata.as_ref()?;
                if !crate::booru::valid_id(&post.post_id) {
                    return None;
                }
                (category, serde_json::json!([post.post_id]).to_string())
            }
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
                    if category == "twitter" && post.community_archive {
                        let community = post.community.as_ref()?;
                        if !crate::twitter::valid_id(&community.id) {
                            return None;
                        }
                        serde_json::json!(["community", community.id, post.post_id, post.media_id])
                            .to_string()
                    } else {
                        serde_json::json!([post.post_id, post.media_id]).to_string()
                    },
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
            MediaJob::InlineText { data } => Some(data.as_str()),
            _ => None,
        };
        serde_json::json!([self.url, self.job, inline_data, self.headers, self.handoff]).to_string()
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct PostMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<crate::file_metadata::FileMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub community: Option<crate::twitter::CommunityMetadata>,
    /// Community feeds use their own archive namespace; ordinary posts do not.
    #[serde(skip_serializing)]
    pub community_archive: bool,
    pub post_id: String,
    /// Site media ID; Twitter uses the upstream file number and Facebook uses
    /// `id.extension` because separate audio and video files share an ID.
    pub media_id: String,
    /// Instagram post shortcode; empty for sites without Instagram shortcodes.
    pub shortcode: String,
    pub username: Option<std::sync::Arc<str>>,
    pub caption: Option<std::sync::Arc<str>>,
    pub timestamp: Option<i64>,
    pub index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retweet_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quoted_by_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reposted_by_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub article: Option<crate::twitter::ArticleMetadata>,
    /// Mastodon instance host the status was fetched from, or Kemono/Coomer
    /// service name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
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

/// Maximum raw UTF-8 article/document payload, independent of URL encoding.
pub(crate) const MAX_INLINE_TEXT: usize = 16 * 1024 * 1024;

/// Construct an opaque inspection URL without URL parsing changing the payload.
/// The job retains the original UTF-8 bytes used when saving the document.
pub fn inline_text(data: String) -> Result<(Url, MediaJob)> {
    if data.len() > MAX_INLINE_TEXT {
        return Err(error("inline text exceeds the payload size limit"));
    }
    let mut source = String::with_capacity(data.len() + 5);
    source.push_str("text:");
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for byte in data.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            source.push(byte as char);
        } else {
            source.push('%');
            source.push(HEX[(byte >> 4) as usize] as char);
            source.push(HEX[(byte & 15) as usize] as char);
        }
    }
    let url = Url::parse(&source).map_err(|_| error("invalid inline text URL"))?;
    Ok((url, MediaJob::InlineText { data }))
}

pub fn social_site(url: &Url) -> Option<&'static str> {
    let host = url.host_str()?;
    if crate::mastodon::INSTANCES.contains(&host) {
        return Some("mastodon");
    }
    if let Some(category) = crate::kemono::category(host) {
        return Some(category);
    }
    if crate::pinterest::is_pinterest_url(url) {
        return Some("pinterest");
    }
    if crate::artstation::is_artstation_url(url) {
        return Some("artstation");
    }
    if crate::wallhaven::is_wallhaven_url(url) {
        return Some("wallhaven");
    }
    if crate::flickr::is_flickr_url(url) {
        return Some("flickr");
    }
    if crate::deviantart::is_deviantart_url(url) {
        return Some("deviantart");
    }
    if crate::pixiv::is_pixiv_url(url) {
        return Some("pixiv");
    }
    if crate::tumblr::is_blog_url(url) {
        return Some("tumblr");
    }
    if let Some(site) = crate::imagehosts::Site::from_host(host) {
        return Some(site.category());
    }
    for (domain, category) in [
        ("instagram.com", "instagram"),
        ("facebook.com", "facebook"),
        ("fb.watch", "facebook"),
        ("bsky.app", "bluesky"),
        ("imgur.com", "imgur"),
        ("reddit.com", "reddit"),
        ("redd.it", "reddit"),
        ("danbooru.donmai.us", "danbooru"),
        ("e621.net", "e621"),
        ("e926.net", "e621"),
        ("gelbooru.com", "gelbooru"),
        ("safebooru.org", "safebooru"),
        ("imgur.io", "imgur"),
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
    pub expand: Option<bool>,
    pub unique: Option<bool>,
    pub videos: Option<crate::twitter::VideoMode>,
    pub previews: Option<bool>,
    pub unavailable: Option<bool>,
    pub text_tweets: Option<bool>,
    pub size: Option<crate::twitter::ImageOptions>,
    pub fallback: Option<bool>,
    pub articles: Option<crate::twitter::ArticleOptions>,
    pub cards: Option<crate::twitter::CardMode>,
    pub cards_blacklist: Option<Vec<String>>,
    pub tweet_endpoint: Option<crate::twitter::TweetEndpoint>,
    pub conversations: Option<crate::twitter::ConversationMode>,
    pub pinned: Option<bool>,
    pub replies: Option<crate::twitter::ReplyMode>,
    pub strategy: Option<crate::twitter::TimelineStrategy>,
    pub search_pagination: Option<crate::twitter::SearchPagination>,
    pub search_results: Option<crate::twitter::SearchProduct>,
    pub limit: Option<u8>,
    pub search_limit: Option<u8>,
    pub search_stop: Option<u8>,
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

    /// Preserve upstream Directory boundaries, including posts without files.
    pub fn events_with_options(
        &self,
        url: &Url,
        options: BackendOptions,
    ) -> BoxStream<'static, Result<crate::event::ExtractionEvent>> {
        crate::backend::events_with_options(self.clone(), url.clone(), options, false)
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
        if let Some(expand) = options.expand {
            command.arg("--option").arg(format!("expand={expand}"));
        }
        if let Some(unique) = options.unique {
            command.arg("--option").arg(format!("unique={unique}"));
        }
        if let Some(videos) = options.videos {
            let value = match videos {
                crate::twitter::VideoMode::Files => "true",
                crate::twitter::VideoMode::Skip => "false",
                crate::twitter::VideoMode::Ytdl => "ytdl",
            };
            command.arg("--option").arg(format!("videos={value}"));
        }
        if let Some(previews) = options.previews {
            command.arg("--option").arg(format!("previews={previews}"));
        }
        if let Some(unavailable) = options.unavailable {
            command
                .arg("--option")
                .arg(format!("unavailable={unavailable}"));
        }
        if let Some(text_tweets) = options.text_tweets {
            command
                .arg("--option")
                .arg(format!("text-tweets={text_tweets}"));
        }
        if let Some(size) = &options.size {
            command
                .arg("--option")
                .arg(format!("size={}", serde_json::json!(size.sizes())));
        }
        if let Some(fallback) = options.fallback {
            command.arg("--option").arg(format!("fallback={fallback}"));
        }
        if let Some(articles) = options.articles {
            let value = if articles.metadata
                && articles.cover
                && articles.media
                && !articles.html
                && !articles.document
            {
                "true".to_owned()
            } else if !articles.metadata
                && !articles.cover
                && !articles.media
                && !articles.html
                && !articles.document
            {
                "false".to_owned()
            } else {
                let flags: Vec<_> = [
                    ("meta", articles.metadata),
                    ("cover", articles.cover),
                    ("media", articles.media),
                    ("html", articles.html),
                    ("doc", articles.document),
                ]
                .into_iter()
                .filter_map(|(flag, enabled)| enabled.then_some(flag))
                .collect();
                serde_json::json!(flags).to_string()
            };
            command.arg("--option").arg(format!("articles={value}"));
        }
        if let Some(cards) = options.cards {
            let value = match cards {
                crate::twitter::CardMode::Files => "true",
                crate::twitter::CardMode::Skip => "false",
                crate::twitter::CardMode::Ytdl => "ytdl",
            };
            command.arg("--option").arg(format!("cards={value}"));
        }
        if let Some(blacklist) = &options.cards_blacklist {
            command
                .arg("--option")
                .arg(format!("cards-blacklist={}", serde_json::json!(blacklist)));
        }
        if let Some(endpoint) = options.tweet_endpoint {
            let value = match endpoint {
                crate::twitter::TweetEndpoint::Auto => "auto",
                crate::twitter::TweetEndpoint::Detail => "detail",
                crate::twitter::TweetEndpoint::RestId => "restid",
            };
            command
                .arg("--option")
                .arg(format!("tweet-endpoint=\"{value}\""));
        }
        if let Some(conversations) = options.conversations {
            let value = match conversations {
                crate::twitter::ConversationMode::Off => "false",
                crate::twitter::ConversationMode::All => "true",
                crate::twitter::ConversationMode::Accessible => "\"accessible\"",
            };
            command
                .arg("--option")
                .arg(format!("conversations={value}"));
        }
        if let Some(pinned) = options.pinned {
            command.arg("--option").arg(format!("pinned={pinned}"));
        }
        if let Some(replies) = options.replies {
            let value = match replies {
                crate::twitter::ReplyMode::All => "true",
                crate::twitter::ReplyMode::Skip => "false",
                crate::twitter::ReplyMode::SelfOnly => "\"self\"",
            };
            command.arg("--option").arg(format!("replies={value}"));
        }
        if let Some(strategy) = options.strategy {
            let value = match strategy {
                crate::twitter::TimelineStrategy::Auto => "auto",
                crate::twitter::TimelineStrategy::Media => "media",
                crate::twitter::TimelineStrategy::Tweets => "tweets",
                crate::twitter::TimelineStrategy::Replies => "with_replies",
            };
            command.arg("--option").arg(format!("strategy=\"{value}\""));
        }
        if let Some(pagination) = options.search_pagination {
            let value = match pagination {
                crate::twitter::SearchPagination::MaxId => "max_id",
                crate::twitter::SearchPagination::Cursor => "cursor",
                crate::twitter::SearchPagination::Date => "date",
            };
            command
                .arg("--option")
                .arg(format!("search-pagination=\"{value}\""));
        }
        if let Some(product) = options.search_results {
            let value = match product {
                crate::twitter::SearchProduct::Latest => "Latest",
                crate::twitter::SearchProduct::Top => "Top",
                crate::twitter::SearchProduct::Media => "Media",
                crate::twitter::SearchProduct::People => "People",
                crate::twitter::SearchProduct::Lists => "Lists",
            };
            command
                .arg("--option")
                .arg(format!("search-results=\"{value}\""));
        }
        if let Some(limit) = options.limit {
            command.arg("--option").arg(format!("limit={limit}"));
        }
        if let Some(limit) = options.search_limit {
            command.arg("--option").arg(format!("search-limit={limit}"));
        }
        if let Some(stop) = options.search_stop {
            command.arg("--option").arg(format!("search-stop={stop}"));
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

pub(crate) fn parse_post_message(message: &Value, category: &str) -> Result<crate::event::Post> {
    let row = message
        .as_array()
        .ok_or_else(|| error("invalid post message"))?;
    let data = match row.as_slice() {
        [kind, data] if kind.as_i64() == Some(2) => data.as_object(),
        [kind, _, data] if kind.as_i64() == Some(2) => data.as_object(),
        _ => None,
    }
    .ok_or_else(|| error("backend post message has no metadata"))?;
    let mut metadata = compatibility_metadata(data, category);
    if let Some(metadata) = &mut metadata {
        metadata.file = None;
        metadata.media_id.clear();
        metadata.index = 0;
    }
    Ok(crate::event::Post {
        category: category.into(),
        metadata,
        count: data
            .get("count")
            .and_then(Value::as_u64)
            .and_then(|count| usize::try_from(count).ok()),
    })
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
    let marker = metadata.get("_ytdl_manifest").filter(|v| !v.is_null());
    let inline_data = metadata.get("_ytdl_manifest_data").filter(|v| !v.is_null());
    let (url, job) = if let Some(data) = source.strip_prefix("text:") {
        if marker.is_some() || inline_data.is_some() {
            return Err(error("inline text cannot contain a manifest handoff"));
        }
        inline_text(data.to_owned())?
    } else {
        let delegated = source.strip_prefix("ytdl:");
        let target = http_url(delegated.unwrap_or(source)).map_err(|_| {
            error("backend media and video handoffs require an HTTP(S) target/base URL")
        })?;
        let url = if delegated.is_some() {
            Url::parse(source)?
        } else {
            target.clone()
        };
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
        (url, job)
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
        file: crate::file_metadata::compatibility(data),
        community: (category == "twitter")
            .then(|| {
                data.get("community")
                    .and_then(crate::twitter::community::compatibility)
            })
            .flatten(),
        community_archive: category == "twitter"
            && data
                .get("subcategory")
                .and_then(Value::as_str)
                .is_some_and(|value| matches!(value, "community" | "communities")),
        retweet_id: context_id("retweet_id"),
        quoted_by_id: context_id("quote_id"),
        reposted_by_id: None,
        article: compatibility_article(data.get("article")),
        instance: None,
        post_id,
        media_id,
        shortcode,
        username: username.map(std::sync::Arc::from),
        caption: caption.map(std::sync::Arc::from),
        timestamp: data.get("date").and_then(|date| match date {
            Value::String(value) => crate::date::parse_date(value).ok(),
            value => value.as_i64(),
        }),
        index,
    })
}

// Admit only public article fields; never retain an arbitrary backend object.
fn compatibility_article(value: Option<&Value>) -> Option<crate::twitter::ArticleMetadata> {
    let data = value?.as_object()?;
    let timestamp = |key: &str| {
        data.get(key)
            .and_then(|value| match value {
                Value::String(value) => crate::date::parse_date(value).ok(),
                value => value.as_i64(),
            })
            .filter(|value| *value >= 0)
    };
    let id = data
        .get("id")
        .and_then(|value| match value {
            Value::String(value) => Some(value.clone()),
            Value::Number(value) if value.is_u64() => Some(value.to_string()),
            _ => None,
        })
        .filter(|id| crate::twitter::valid_id(id));
    Some(crate::twitter::ArticleMetadata {
        id,
        title: data
            .get("title")
            .and_then(Value::as_str)
            .map(std::sync::Arc::from),
        date: timestamp("date"),
        date_updated: timestamp("date_updated"),
        html: data
            .get("html")
            .and_then(Value::as_str)
            .map(std::sync::Arc::from),
    })
}
