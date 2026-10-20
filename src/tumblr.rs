//! Experimental native Tumblr blog, post and tag extraction.
//!
//! Mirrors gallery_dl/extractor/tumblr.py using the public API v2 with an
//! `api_key` query parameter (upstream's default public key unless the
//! `api-key` setting overrides it) and the legacy (non-NPF) post format.
//! Photo posts select the largest API rendition and resolve capped originals;
//! video posts yield `video_url`, audio posts a direct `audio_url`, and every
//! post its inline `<img>` images, enlarged where Tumblr supports it.
//! Reblogs are included, as upstream does by default. Media must be served
//! over HTTPS from `*.media.tumblr.com` (audio also from `a.tumblr.com`).
use crate::{
    Result, error,
    event::{ExtractionEvent, group},
    extract::{Media, MediaJob, PostMetadata},
    file_metadata::FileMetadata,
    network::NetworkOptions,
    transport::{RateLimits, Redirects, retry_after},
};
use futures_util::{
    StreamExt,
    stream::{self, BoxStream},
};
use reqwest::{Client, StatusCode};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    sync::Arc,
    time::{Duration, SystemTime},
};
use url::Url;

const USER_AGENT: &str = concat!(
    "gallery-dl-rs/",
    env!("CARGO_PKG_VERSION"),
    " (native Tumblr extractor)"
);
/// Upstream gallery-dl's default public API key.
pub const DEFAULT_API_KEY: &str = "O3hU2tMi5e4Qs5t3vezEi6L0qRORJ5y9oUpSGsrWu8iA3UCc3B";
const MAX_RESPONSE: usize = 32 * 1024 * 1024;
const PAGE_SIZE: usize = 50;
const MAX_PAGES: usize = 200;
/// `*.tumblr.com` labels that are services rather than blogs.
const RESERVED_HOSTS: [&str; 12] = [
    "www", "api", "media", "assets", "static", "a", "va", "vt", "vtt", "ve", "help", "embed",
];
/// First `www.tumblr.com` path segments that are site pages, not blogs.
const RESERVED_PATHS: [&str; 30] = [
    "dashboard",
    "explore",
    "search",
    "tagged",
    "settings",
    "login",
    "register",
    "likes",
    "following",
    "inbox",
    "new",
    "blog",
    "communities",
    "about",
    "help",
    "privacy",
    "policy",
    "docs",
    "apps",
    "tv",
    "oauth",
    "logout",
    "reblog",
    "edit",
    "customize",
    "safe-mode",
    "live",
    "messaging",
    "activity",
    "post",
];

/// Blog names are 1-32 ASCII letters, digits and inner hyphens.
pub fn valid_blog(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-')
}

pub fn valid_id(id: &str) -> bool {
    (1..=24).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_digit())
}

/// Media IDs are the first 16 hex digits of the SHA-256 of the media path.
pub fn valid_media_id(id: &str) -> bool {
    id.len() == 16 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Blog name for a `BLOG.tumblr.com` host; service hosts are not blogs.
fn host_blog(host: &str) -> Option<&str> {
    let blog = host.strip_suffix(".tumblr.com")?;
    (!blog.contains('.') && !RESERVED_HOSTS.contains(&blog) && valid_blog(blog)).then_some(blog)
}

fn www_blog<'a>(segments: &[&'a str]) -> Option<&'a str> {
    let first = *segments.first()?;
    (!RESERVED_PATHS.contains(&first) && valid_blog(first)).then_some(first)
}

/// Whether a URL is a Tumblr blog page handled by this extractor.
pub fn is_blog_url(url: &Url) -> bool {
    match url.host_str() {
        Some("www.tumblr.com" | "tumblr.com") => www_blog(&segments(url)).is_some(),
        Some(host) => host_blog(host).is_some(),
        None => false,
    }
}

fn segments(url: &Url) -> Vec<&str> {
    url.path_segments()
        .map(|s| s.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Posts { blog: String },
    Post { blog: String, id: String },
    Tagged { blog: String, tag: String },
}

fn percent_decode(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn tag_of(raw: &str) -> Option<String> {
    // Upstream: text.unquote(tag.replace("-", " ")).
    let tag = percent_decode(&raw.replace(['-', '+'], " "))?;
    let tag = tag.trim();
    ((1..=256).contains(&tag.len()) && !tag.chars().any(char::is_control)).then(|| tag.into())
}

pub fn target(url: &Url) -> Result<Target> {
    let unsupported = || {
        error(
            "unsupported Tumblr URL; expected https://BLOG.tumblr.com[/post/ID|/tagged/TAG] or https://www.tumblr.com/BLOG[/ID]",
        )
    };
    if !matches!(url.scheme(), "http" | "https")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(unsupported());
    }
    let host = url.host_str().ok_or_else(unsupported)?;
    let all = segments(url);
    let (blog, rest) = if matches!(host, "www.tumblr.com" | "tumblr.com") {
        (www_blog(&all).ok_or_else(unsupported)?, &all[1..])
    } else {
        (host_blog(host).ok_or_else(unsupported)?, &all[..])
    };
    let blog = blog.to_ascii_lowercase();
    match rest {
        [] | ["posts"] => Ok(Target::Posts { blog }),
        ["post", id] | ["post", id, _] if valid_id(id) => Ok(Target::Post {
            blog,
            id: (*id).into(),
        }),
        [id] | [id, _] if valid_id(id) && matches!(host, "www.tumblr.com" | "tumblr.com") => {
            Ok(Target::Post {
                blog,
                id: (*id).into(),
            })
        }
        ["tagged", tag] => Ok(Target::Tagged {
            blog,
            tag: tag_of(tag).ok_or_else(unsupported)?,
        }),
        _ => Err(unsupported()),
    }
}

fn string<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn id_of(post: &Value) -> Option<String> {
    match post.get("id_string").or_else(|| post.get("id"))? {
        Value::String(s) => Some(s.trim().to_owned()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
    .filter(|id| valid_id(id))
}

/// HTTPS media on `*.media.tumblr.com` (or `a.tumblr.com` for audio).
fn media_url(raw: &str, audio: bool) -> Option<Url> {
    let url = Url::parse(&raw.trim().replace("&amp;", "&")).ok()?;
    let host = url.host_str()?;
    (url.scheme() == "https"
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && (host.ends_with(".media.tumblr.com") || audio && host == "a.tumblr.com"))
        .then_some(url)
}

pub fn media_id(url: &Url) -> String {
    let digest = Sha256::digest(url.path().as_bytes());
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

fn extension(url: &Url, fallback: &str) -> String {
    url.path_segments()
        .and_then(|mut s| s.next_back())
        .and_then(|name| name.rsplit_once('.'))
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .filter(|ext| {
            (1..=5).contains(&ext.len()) && ext.bytes().all(|b| b.is_ascii_alphanumeric())
        })
        .map(|ext| if ext == "jpeg" { "jpg".into() } else { ext })
        .unwrap_or_else(|| fallback.into())
}

/// `src` values of `<img>` tags in an HTML fragment.
fn inline_images(html: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let lower = html.to_ascii_lowercase();
    let mut pos = 0;
    while let Some(start) = lower[pos..].find("<img") {
        let start = pos + start;
        let end = lower[start..].find('>').map_or(lower.len(), |e| start + e);
        let tag = &lower[start..end];
        if let Some(src) = tag.find("src=") {
            let value = start + src + 4;
            if let Some(quote) = html[value..]
                .chars()
                .next()
                .filter(|c| matches!(c, '"' | '\''))
                && let Some(len) = html[value + 1..end].find(quote)
            {
                out.push(&html[value + 1..value + 1 + len]);
            }
        }
        pos = end;
    }
    out
}

fn dimension(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(Value::as_u64).filter(|v| *v > 0)
}

/// Photo URL and size: start at `original_size`, considering larger `alt_sizes`.
fn photo(photo: &Value) -> Option<(Url, Option<u64>, Option<u64>)> {
    let sized = |size: &Value| {
        let url = media_url(string(size, "url")?, false)?;
        Some((url, dimension(size, "width"), dimension(size, "height")))
    };
    let mut best = photo.get("original_size").and_then(sized);
    for candidate in photo
        .get("alt_sizes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(sized)
    {
        // Upstream compares either dimension, even for narrower/taller images.
        if best.as_ref().is_none_or(|(_, width, height)| {
            candidate.1.unwrap_or(0) > width.unwrap_or(0)
                || candidate.2.unwrap_or(0) > height.unwrap_or(0)
        }) {
            best = Some(candidate);
        }
    }
    best
}

/// Request original image tokens only for Tumblr's documented resize paths.
fn original_image(url: &Url, width: Option<u64>, height: Option<u64>, inline: bool) -> Option<Url> {
    let path = url.path();
    let size = if inline {
        path.split('/').find(|segment| {
            segment
                .strip_prefix('s')
                .and_then(|size| size.split_once('x'))
                .is_some_and(|(w, h)| {
                    !w.is_empty()
                        && !h.is_empty()
                        && w.bytes().all(|b| b.is_ascii_digit())
                        && h.bytes().all(|b| b.is_ascii_digit())
                })
        })?
    } else if width == Some(2048) || height == Some(3072) {
        "s2048x3072"
    } else {
        return None;
    };
    let from = format!("/{size}/");
    if !path.contains(&from) {
        return None;
    }
    let mut original = url.clone();
    original.set_path(&path.replacen(&from, "/s99999x99999/", 1));
    Some(original)
}

/// Legacy inline images use the largest numbered rendition, `_1280`.
fn inline_image(mut url: Url) -> Url {
    let Some(host) = url
        .host_str()
        .and_then(|host| host.strip_suffix(".media.tumblr.com"))
    else {
        return url;
    };
    if !host.bytes().all(|b| b.is_ascii_digit()) || host.is_empty() {
        return url;
    }
    let path = url.path();
    let Some((stem, ext)) = path.rsplit_once('.') else {
        return url;
    };
    let Some((base, size)) = stem.rsplit_once('_') else {
        return url;
    };
    let name = base.rsplit('/').next().unwrap_or("");
    let hash = name
        .strip_prefix("tumblr_inline_")
        .or_else(|| name.strip_prefix("tumblr_"));
    if hash.is_some_and(|hash| !hash.is_empty() && !hash.contains('_'))
        && !size.is_empty()
        && size.bytes().all(|b| b.is_ascii_digit())
        && !ext.is_empty()
        && ext
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    {
        url.set_path(&format!("{base}_1280.{ext}"));
    }
    url
}

/// Which reblogs to include, matching upstream's `reblogs` option.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Reblogs {
    #[default]
    All,
    OriginalOnly,
    SameBlog,
}

/// Inline media scope, matching upstream's boolean and string modes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Inline {
    #[default]
    All,
    Disabled,
    Reblog,
    Original,
}

/// Native Tumblr post and image selection settings.
#[derive(Clone, Debug)]
pub struct Selection {
    pub reblogs: Reblogs,
    pub posts: Option<HashSet<String>>,
    pub inline: Inline,
    pub original: bool,
}

impl Default for Selection {
    fn default() -> Self {
        Self {
            reblogs: Reblogs::All,
            posts: None,
            inline: Inline::All,
            original: true,
        }
    }
}

impl Selection {
    /// Restrict extraction to the upstream legacy post types. An empty list
    /// selects no posts; unknown names are rejected rather than ignored.
    pub fn with_posts(mut self, types: impl IntoIterator<Item = String>) -> Result<Self> {
        let types: HashSet<_> = types.into_iter().collect();
        for kind in &types {
            if !matches!(
                kind.as_str(),
                "text"
                    | "quote"
                    | "link"
                    | "answer"
                    | "video"
                    | "audio"
                    | "photo"
                    | "chat"
                    | "search"
            ) {
                return Err(error(format!("unsupported Tumblr post type: {kind}")));
            }
        }
        self.posts = Some(types);
        Ok(self)
    }

    fn includes(&self, post: &Value, blog: &str) -> bool {
        if let Some(types) = &self.posts
            && !string(post, "type").is_some_and(|kind| types.contains(kind))
        {
            return false;
        }
        if post.get("reblogged_from_id").is_none() {
            return true;
        }
        match self.reblogs {
            Reblogs::All => true,
            Reblogs::OriginalOnly => false,
            Reblogs::SameBlog => {
                let fallback_blog = post
                    .get("blog")
                    .and_then(|blog| string(blog, "name"))
                    .unwrap_or(blog);
                let fallback = format!("{fallback_blog}.tumblr.com");
                let own_uuid = post
                    .get("blog")
                    .and_then(|blog| string(blog, "uuid"))
                    .unwrap_or(&fallback);
                string(post, "reblogged_root_uuid") == Some(own_uuid)
            }
        }
    }
}

/// URL, media type, width, height, fallback extension and inline origin.
type File<'a> = (Url, &'a str, Option<u64>, Option<u64>, &'a str, bool);

/// Parse one legacy-format post into its deduplicated media files.
pub fn parse_post(post: &Value, blog: &str) -> Vec<Media> {
    parse_post_selected(post, blog, &Selection::default())
}

fn parse_post_selected(post: &Value, blog: &str, selection: &Selection) -> Vec<Media> {
    if !selection.includes(post, blog) {
        return Vec::new();
    }
    let Some(id) = id_of(post) else {
        return Vec::new();
    };
    let username: Arc<str> = Arc::from(
        string(post, "blog_name")
            .filter(|name| valid_blog(name))
            .unwrap_or(blog),
    );
    let caption = string(post, "summary")
        .map(str::to_owned)
        .or_else(|| string(post, "caption").map(crate::mastodon::strip_html))
        .filter(|text| !text.is_empty())
        .map(Arc::<str>::from);
    let timestamp = post.get("timestamp").and_then(Value::as_i64);
    let mut files: Vec<File> = Vec::new();
    for item in post
        .get("photos")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some((url, width, height)) = photo(item) {
            files.push((url, "photo", width, height, "jpg", false));
        }
    }
    if let Some(url) = string(post, "video_url").and_then(|raw| media_url(raw, false)) {
        files.push((url, "video", None, None, "mp4", false));
    }
    if let Some(url) = string(post, "audio_url").and_then(|raw| media_url(raw, true)) {
        files.push((url, "audio", None, None, "mp3", false));
    }
    let mut inline_html = Vec::new();
    match selection.inline {
        Inline::Disabled => {}
        Inline::All => {
            for key in [
                "body",
                "caption",
                "answer",
                "text",
                "description",
                "question",
            ] {
                inline_html.extend(string(post, key));
            }
            if let Some(reblog) = post.get("reblog") {
                inline_html.extend(string(reblog, "comment"));
                inline_html.extend(string(reblog, "tree_html"));
            }
        }
        Inline::Reblog | Inline::Original => {
            inline_html.extend(string(post, "answer"));
            inline_html.extend(string(post, "question"));
            if let Some(reblog) = post.get("reblog") {
                inline_html.extend(if selection.inline == Inline::Reblog {
                    string(reblog, "comment")
                } else {
                    string(reblog, "tree_html").or_else(|| string(reblog, "comment"))
                });
            }
        }
    }
    for html in inline_html {
        for src in inline_images(html) {
            if let Some(url) = media_url(src, false) {
                files.push((inline_image(url), "photo", None, None, "jpg", true));
            }
        }
    }
    let mut seen = HashSet::new();
    let mut media = Vec::new();
    for (url, kind, width, height, fallback, inline) in files {
        if !seen.insert(url.path().to_owned()) {
            continue;
        }
        let index = media.len() + 1;
        let ext = extension(&url, fallback);
        let mut handoff = BTreeMap::new();
        if selection.original
            && kind == "photo"
            && let Some(original) = original_image(&url, width, height, inline)
        {
            handoff.insert("_tumblr_original".into(), Value::String(original.into()));
        }
        media.push(Media {
            filename: format!("{id}_{index}.{ext}"),
            job: MediaJob::DirectHttp,
            category: "tumblr".into(),
            metadata: Some(PostMetadata {
                file: Some(FileMetadata {
                    width,
                    height,
                    media_type: Some(Arc::from(kind)),
                    ..FileMetadata::default()
                }),
                post_id: id.clone(),
                media_id: media_id(&url),
                username: Some(username.clone()),
                caption: caption.clone(),
                timestamp,
                index,
                ..PostMetadata::default()
            }),
            url,
            headers: BTreeMap::new(),
            handoff,
        });
    }
    media
}

/// One API page: per-post media, the number of posts and the reported total.
pub struct Page {
    pub posts: Vec<Vec<Media>>,
    pub count: usize,
    pub total: Option<u64>,
}

pub fn parse_page(bytes: &[u8], blog: &str) -> Result<Page> {
    parse_page_selected(bytes, blog, &Selection::default())
}

fn parse_page_selected(bytes: &[u8], blog: &str, selection: &Selection) -> Result<Page> {
    let body: Value =
        serde_json::from_slice(bytes).map_err(|_| error("Tumblr returned invalid JSON"))?;
    let response = body.get("response").unwrap_or(&body);
    let entries = response
        .get("posts")
        .and_then(Value::as_array)
        .ok_or_else(|| error("Tumblr response contains no post list"))?;
    Ok(Page {
        posts: entries
            .iter()
            .map(|post| parse_post_selected(post, blog, selection))
            .filter(|media| !media.is_empty())
            .collect(),
        count: entries.len(),
        total: response.get("total_posts").and_then(Value::as_u64),
    })
}

/// Parse a `?id=` response, requiring the requested post.
pub fn parse_single(bytes: &[u8], blog: &str, id: &str) -> Result<Vec<Media>> {
    let page = parse_page(bytes, blog)?;
    let media = page
        .posts
        .into_iter()
        .find(|media| media[0].metadata.as_ref().is_some_and(|m| m.post_id == id));
    match media {
        Some(media) => Ok(media),
        None if page.count == 0 => Err(error("Tumblr post not found")),
        None => Err(error("Tumblr post contains no supported media")),
    }
}

pub struct Tumblr {
    client: Client,
    network: NetworkOptions,
    api_root: Url,
    api_key: String,
    timeout: Duration,
    request_timeout: Duration,
    retries: u8,
    rate_limits: RateLimits,
    selection: Selection,
}

struct ListingState {
    blog: String,
    tag: Option<String>,
    offset: usize,
    pages: usize,
    done: bool,
    pending: VecDeque<ExtractionEvent>,
}

impl Tumblr {
    pub fn new(timeout: Duration) -> Result<Self> {
        Ok(Self {
            client: NetworkOptions::default().client(USER_AGENT, timeout)?,
            network: NetworkOptions::default(),
            api_root: Url::parse("https://api.tumblr.com/").expect("static URL"),
            api_key: DEFAULT_API_KEY.into(),
            timeout,
            request_timeout: timeout,
            retries: 0,
            rate_limits: RateLimits::default(),
            selection: Selection::default(),
        })
    }

    /// Apply proxy and user-agent settings to API requests and emitted media.
    pub fn with_network(mut self, network: &NetworkOptions) -> Result<Self> {
        self.client = network.client(USER_AGENT, self.timeout)?;
        self.network = network.clone();
        Ok(self)
    }

    /// Replace the default public API key.
    pub fn with_api_key(mut self, key: Option<&str>) -> Self {
        if let Some(key) = key {
            self.api_key = key.into();
        }
        self
    }

    /// Select post types, reblogs, inline scope and original image resolution.
    pub fn with_selection(mut self, selection: Selection) -> Self {
        self.selection = selection;
        self
    }

    /// Additional attempts for HTTP 429/5xx and transport failures.
    pub fn with_retries(mut self, retries: u8) -> Self {
        self.retries = retries;
        self
    }

    /// Bound each HTTP request separately from the per-call deadline.
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// Override the API origin; intended for local tests.
    pub fn with_api_root(mut self, root: Url) -> Self {
        self.api_root = root;
        self
    }

    async fn call(&self, blog: &str, query: &[(&str, &str)]) -> Result<Vec<u8>> {
        let mut endpoint = self
            .api_root
            .join(&format!("v2/blog/{blog}.tumblr.com/posts"))
            .expect("validated Tumblr blog");
        endpoint
            .query_pairs_mut()
            .append_pair("api_key", &self.api_key)
            .append_pair("npf", "false")
            .append_pair("reblog_info", "true")
            .extend_pairs(query);
        tokio::time::timeout(self.timeout, self.request(&endpoint))
            .await
            .map_err(|_| error("native Tumblr extraction timed out"))?
    }

    async fn finish_event(&self, mut event: ExtractionEvent) -> ExtractionEvent {
        if let ExtractionEvent::Media(item) = &mut event {
            if let Some(Value::String(original)) = item.handoff.remove("_tumblr_original")
                && let Some(original) = media_url(&original, false)
            {
                self.finish_original(item, original).await;
            }
            if item
                .metadata
                .as_ref()
                .and_then(|meta| meta.file.as_ref())
                .and_then(|file| file.media_type.as_deref())
                == Some("photo")
                && extension(&item.url, "") == "gif"
            {
                let mut gifv = item.url.clone();
                gifv.set_path(&format!("{}v", gifv.path()));
                let fallbacks = item
                    .handoff
                    .entry("_fallback".into())
                    .or_insert_with(|| serde_json::json!([]));
                if let Some(fallbacks) = fallbacks.as_array_mut() {
                    fallbacks.insert(0, Value::String(gifv.into()));
                }
                item.headers.insert(
                    "Accept".into(),
                    "image/avif,image/webp,image/apng,image/svg+xml,image/*,*/*;q=0.8".into(),
                );
            }
            self.network.apply_media(item);
        }
        event
    }

    async fn finish_original(&self, item: &mut Media, original: Url) {
        let fallback = item.url.to_string();
        item.url = self.resolve_original(&original).await.unwrap_or_else(|| {
            item.handoff
                .insert("_fallback".into(), serde_json::json!([fallback]));
            original
        });
        // The archive identity stays tied to the API rendition, independent of
        // the rotating CDN token and whether original resolution succeeded.
        if let Some((stem, _)) = item.filename.rsplit_once('.') {
            item.filename = format!("{stem}.{}", extension(&item.url, "jpg"));
        }
    }

    /// Tumblr serves an HTML image page at the oversize URL containing a fresh
    /// CDN token. API credentials are confined to `call`, never these requests.
    async fn resolve_original(&self, original: &Url) -> Option<Url> {
        let body = tokio::time::timeout(
            self.timeout,
            self.request_with_accept(original, Some("text/html,*/*;q=0.8")),
        )
        .await
        .ok()?
        .ok()?;
        let html = std::str::from_utf8(&body).ok()?;
        inline_images(html)
            .into_iter()
            .find_map(|raw| media_url(raw, false))
    }

    async fn next_page(&self, state: &mut ListingState) -> Result<()> {
        let offset = state.offset.to_string();
        let limit = PAGE_SIZE.to_string();
        let mut query = vec![("offset", offset.as_str()), ("limit", limit.as_str())];
        if let Some(tag) = &state.tag {
            query.insert(0, ("tag", tag.as_str()));
        }
        let body = self.call(&state.blog, &query).await?;
        let page = parse_page_selected(&body, &state.blog, &self.selection)?;
        state.pages += 1;
        state.offset += page.count;
        // The API may cap `limit` below 50, so stop on an empty page or the total.
        state.done = page.count == 0
            || page.total.is_some_and(|total| state.offset as u64 >= total)
            || state.pages >= MAX_PAGES;
        for media in page.posts {
            state.pending.extend(group(media, "tumblr"));
        }
        Ok(())
    }

    pub fn stream(&self, url: &Url) -> BoxStream<'_, Result<Media>> {
        crate::event::media_only(self.events(url))
    }

    /// Emit one post boundary per post; listings paginate lazily.
    pub fn events(&self, url: &Url) -> BoxStream<'_, Result<ExtractionEvent>> {
        let target = match target(url) {
            Ok(target) => target,
            Err(e) => return stream::once(async move { Err(e) }).boxed(),
        };
        let (blog, tag) = match target {
            Target::Post { blog, id } => {
                return stream::once(async move {
                    let body = self.call(&blog, &[("id", &id)]).await?;
                    // Validate the requested post before applying selection; a
                    // deliberately filtered post is a successful empty result.
                    let value: Value = serde_json::from_slice(&body)
                        .map_err(|_| error("Tumblr returned invalid JSON"))?;
                    let entries = value
                        .get("response")
                        .unwrap_or(&value)
                        .get("posts")
                        .and_then(Value::as_array)
                        .ok_or_else(|| error("Tumblr response contains no post list"))?;
                    let post = entries
                        .iter()
                        .find(|post| id_of(post).as_deref() == Some(&id))
                        .ok_or_else(|| error("Tumblr post not found"))?;
                    let mut selection = self.selection.clone();
                    // Upstream single-post extraction ignores the posts filter.
                    selection.posts = None;
                    let media = parse_post_selected(post, &blog, &selection);
                    if media.is_empty()
                        && selection.includes(post, &blog)
                        && selection.inline == Inline::All
                    {
                        return Err(error("Tumblr post contains no supported media"));
                    }
                    Ok::<_, crate::Error>(
                        stream::iter(group(media, "tumblr"))
                            .then(move |event| async move { Ok(self.finish_event(event).await) }),
                    )
                })
                .map(|result| match result {
                    Ok(events) => events.boxed(),
                    Err(e) => stream::once(async move { Err(e) }).boxed(),
                })
                .flatten()
                .boxed();
            }
            Target::Posts { blog } => (blog, None),
            Target::Tagged { blog, tag } => (blog, Some(tag)),
        };
        let state = ListingState {
            blog,
            tag,
            offset: 0,
            pages: 0,
            done: false,
            pending: VecDeque::new(),
        };
        stream::unfold(Some(state), move |state| async move {
            let mut state = state?;
            loop {
                if let Some(event) = state.pending.pop_front() {
                    return Some((Ok(self.finish_event(event).await), Some(state)));
                }
                if state.done {
                    return None;
                }
                if let Err(e) = self.next_page(&mut state).await {
                    return Some((Err(e), None));
                }
            }
        })
        .boxed()
    }

    async fn request(&self, endpoint: &Url) -> Result<Vec<u8>> {
        self.request_with_accept(endpoint, None).await
    }

    async fn request_with_accept(&self, endpoint: &Url, accept: Option<&str>) -> Result<Vec<u8>> {
        for attempt in 0..=self.retries {
            match self.attempt(endpoint, attempt, accept).await {
                Ok(body) => return Ok(body),
                Err(failure) if failure.retry && attempt < self.retries => {
                    tokio::time::sleep(backoff(attempt)).await;
                }
                Err(failure) => return Err(failure.error),
            }
        }
        unreachable!()
    }

    async fn attempt(
        &self,
        endpoint: &Url,
        attempt: u8,
        accept: Option<&str>,
    ) -> std::result::Result<Vec<u8>, Failure> {
        let mut target = endpoint.clone();
        let mut redirects = Redirects::new(endpoint);
        let mut response = loop {
            self.rate_limits.wait(&target, self.timeout).await?;
            let mut request = self
                .client
                .get(target.clone())
                .timeout(self.request_timeout);
            if let Some(accept) = accept {
                request = request.header(reqwest::header::ACCEPT, accept);
            }
            let response = request
                .send()
                .await
                .map_err(|_| Failure::transient("Tumblr request failed; check network access"))?;
            match redirects.next(response.url(), response.status(), response.headers())? {
                Some(next) => target = next,
                None => break response,
            }
        };
        let status = response.status();
        if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
            let delay = retry_after(response.headers(), SystemTime::now());
            self.rate_limits
                .defer(endpoint, delay.unwrap_or_else(|| backoff(attempt)));
            return Err(Failure::transient(
                if status == StatusCode::TOO_MANY_REQUESTS {
                    match delay {
                        Some(delay) => {
                            format!("Tumblr rate limit; retry after {} seconds", delay.as_secs())
                        }
                        None => "Tumblr rate limit; retry later".into(),
                    }
                } else {
                    format!("Tumblr returned HTTP {status}")
                },
            ));
        }
        if matches!(status.as_u16(), 404 | 410) {
            return Err(error("Tumblr blog or post not found").into());
        }
        if matches!(status.as_u16(), 401 | 403) {
            return Err(error("Tumblr refused access; check the api-key setting").into());
        }
        if !status.is_success() {
            return Err(error(format!("Tumblr returned HTTP {status}")).into());
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| Failure::transient("could not read Tumblr response"))?
        {
            if body.len() + chunk.len() > MAX_RESPONSE {
                return Err(error("Tumblr response exceeded 32 MiB").into());
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }
}

fn backoff(attempt: u8) -> Duration {
    Duration::from_millis(250 * (1u64 << attempt.min(6)))
}

struct Failure {
    error: crate::Error,
    retry: bool,
}

impl Failure {
    fn transient(message: impl Into<String>) -> Self {
        Self {
            error: error(message),
            retry: true,
        }
    }
}

impl From<crate::Error> for Failure {
    fn from(error: crate::Error) -> Self {
        Self {
            error,
            retry: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread::{self, JoinHandle},
    };

    const PHOTO: &str = "https://64.media.tumblr.com/abc/def/s2048x3072/one.jpg";

    fn post(id: u64) -> Value {
        json!({"id": id, "id_string": id.to_string(), "blog_name": "alice", "type": "photo",
            "timestamp": 1700000000, "summary": "Hi there",
            "caption": "<p>Hi <b>there</b> <img src=\"https://64.media.tumblr.com/x/inline.png\"></p>",
            "photos": [
                {"original_size": {"url": PHOTO, "width": 2000, "height": 1500}},
                {"original_size": {"url": "http://64.media.tumblr.com/insecure.jpg"},
                 "alt_sizes": [
                    {"url": "https://64.media.tumblr.com/s/small.jpg", "width": 100},
                    {"url": "https://64.media.tumblr.com/s/big.jpg", "width": 500, "height": 400}]},
                {"original_size": {"url": "https://evil.example/x.jpg"}},
                {"original_size": {"url": PHOTO}}],
            "video_url": "https://va.media.tumblr.com/tumblr_v.mp4",
            "audio_url": "https://www.example.com/song.mp3"})
    }

    #[test]
    fn parses_legacy_post_media() {
        let media = parse_post(&post(7), "fallback");
        let urls: Vec<_> = media.iter().map(|m| m.url.as_str()).collect();
        assert_eq!(
            urls,
            [
                PHOTO,
                "https://64.media.tumblr.com/s/big.jpg",
                "https://va.media.tumblr.com/tumblr_v.mp4",
                "https://64.media.tumblr.com/x/inline.png",
            ]
        );
        assert_eq!(media[0].filename, "7_1.jpg");
        assert_eq!(media[2].filename, "7_3.mp4");
        assert_eq!(media[3].filename, "7_4.png");
        let meta = media[0].metadata.as_ref().unwrap();
        assert_eq!(meta.post_id, "7");
        assert_eq!(meta.username.as_deref(), Some("alice"));
        assert_eq!(meta.caption.as_deref(), Some("Hi there"));
        assert_eq!(meta.timestamp, Some(1700000000));
        assert!(valid_media_id(&meta.media_id));
        let file = meta.file.as_ref().unwrap();
        assert_eq!((file.width, file.height), (Some(2000), Some(1500)));
        let key = media[0].archive_key().unwrap();
        assert!(key.starts_with("tumblr:"), "{key}");
        assert_ne!(Some(key.clone()), media[1].archive_key());
        assert_ne!(Some(key), parse_post(&post(8), "alice")[0].archive_key());
        let audio = parse_post(
            &json!({"id": 9, "audio_url": "https://a.tumblr.com/tumblr_a.mp3"}),
            "bob",
        );
        assert_eq!(audio[0].filename, "9_1.mp3");
        assert_eq!(
            audio[0].metadata.as_ref().unwrap().username.as_deref(),
            Some("bob")
        );
    }

    #[test]
    fn parses_pages_and_single_posts() {
        let body = json!({"meta": {"status": 200},
            "response": {"posts": [post(1), {"id": 2, "type": "text"}], "total_posts": 2}})
        .to_string();
        let page = parse_page(body.as_bytes(), "alice").unwrap();
        assert_eq!((page.posts.len(), page.count, page.total), (1, 2, Some(2)));
        assert_eq!(
            parse_single(body.as_bytes(), "alice", "1").unwrap().len(),
            4
        );
        assert!(
            parse_single(body.as_bytes(), "alice", "2")
                .unwrap_err()
                .to_string()
                .contains("no supported media")
        );
        let empty = json!({"response": {"posts": []}}).to_string();
        assert!(parse_single(empty.as_bytes(), "alice", "1").is_err());
        assert!(parse_page(b"{}", "alice").is_err());
        assert!(parse_page(b"nope", "alice").is_err());
    }

    #[test]
    fn original_candidates_respect_photo_caps_and_inline_sizes() {
        let url = Url::parse(PHOTO).unwrap();
        let expected = "https://64.media.tumblr.com/abc/def/s99999x99999/one.jpg";
        assert_eq!(
            original_image(&url, Some(2048), None, false)
                .unwrap()
                .as_str(),
            expected
        );
        assert_eq!(
            original_image(&url, None, Some(3072), false)
                .unwrap()
                .as_str(),
            expected
        );
        assert!(original_image(&url, Some(2000), Some(1500), false).is_none());
        assert!(original_image(&url, None, None, false).is_none());
        let inline =
            Url::parse("https://64.media.tumblr.com/a/b/s500x750/x.png?token=one").unwrap();
        assert_eq!(
            original_image(&inline, None, None, true).unwrap().as_str(),
            "https://64.media.tumblr.com/a/b/s99999x99999/x.png?token=one"
        );
        let malformed = Url::parse("https://64.media.tumblr.com/a/sx750/x.png").unwrap();
        assert!(original_image(&malformed, None, None, true).is_none());
        let mut capped = post(10);
        capped["photos"][0]["original_size"]["width"] = json!(2048);
        let parsed = parse_post(&capped, "alice");
        assert_eq!(parsed[0].handoff["_tumblr_original"], expected);
        assert!(!parsed[1].handoff.contains_key("_tumblr_original"));
    }

    #[test]
    fn selects_larger_photo_alternatives_and_upscales_legacy_inline_images() {
        let parsed = parse_post(
            &json!({"id":1,"photos":[{
            "original_size":{"url":"https://64.media.tumblr.com/tiny.jpg","width":500,"height":400},
            "alt_sizes":[{"url":PHOTO,"width":2048,"height":1536}]
        }],"body":"<img src=\"https://64.media.tumblr.com/abc/tumblr_inline_HASH_500.jpg\">"}),
            "alice",
        );
        assert_eq!(parsed[0].url.as_str(), PHOTO);
        assert!(parsed[0].handoff.contains_key("_tumblr_original"));
        assert_eq!(
            parsed[1].url.as_str(),
            "https://64.media.tumblr.com/abc/tumblr_inline_HASH_1280.jpg"
        );
        for raw in [
            "https://va.media.tumblr.com/tumblr_HASH_500.jpg",
            "https://64.media.tumblr.com/arbitrary_500.jpg",
            "https://64.media.tumblr.com/tumblr_HASH_other_500.jpg",
        ] {
            assert_eq!(inline_image(Url::parse(raw).unwrap()).as_str(), raw);
        }
        let taller = photo(&json!({
            "original_size":{"url":PHOTO,"width":2048,"height":1000},
            "alt_sizes":[{"url":"https://64.media.tumblr.com/tall.jpg","width":1000,"height":2000}]
        }))
        .unwrap();
        assert_eq!(taller.0.as_str(), "https://64.media.tumblr.com/tall.jpg");
    }

    #[test]
    fn classifies_supported_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        let posts = Target::Posts {
            blog: "alice".into(),
        };
        let single = Target::Post {
            blog: "alice".into(),
            id: "123".into(),
        };
        assert_eq!(t("https://alice.tumblr.com").unwrap(), posts);
        assert_eq!(t("https://alice.tumblr.com/posts").unwrap(), posts);
        assert_eq!(t("https://www.tumblr.com/alice").unwrap(), posts);
        assert_eq!(t("https://alice.tumblr.com/post/123").unwrap(), single);
        assert_eq!(t("https://alice.tumblr.com/post/123/slug").unwrap(), single);
        assert_eq!(t("https://www.tumblr.com/alice/123").unwrap(), single);
        assert_eq!(t("https://www.tumblr.com/alice/123/slug").unwrap(), single);
        assert_eq!(
            t("https://alice.tumblr.com/tagged/cute-cats%21").unwrap(),
            Target::Tagged {
                blog: "alice".into(),
                tag: "cute cats!".into()
            }
        );
        for raw in [
            "https://www.tumblr.com/",
            "https://www.tumblr.com/dashboard",
            "https://www.tumblr.com/tagged/cats",
            "https://64.media.tumblr.com/abc/one.jpg",
            "https://media.tumblr.com/x.jpg",
            "https://api.tumblr.com/v2/blog",
            "https://alice.tumblr.com/post/abc",
            "https://alice.tumblr.com/about",
            "https://alice.tumblr.com:444/",
            "https://tumblr.com.evil.example/alice",
            "https://alice.tumblr.com.evil.example/",
        ] {
            assert!(t(raw).is_err(), "{raw}");
        }
    }

    fn response(status: &str, body: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\nRetry-After: 0\r\n\r\n",
            body.len()
        )
        .into_bytes();
        out.extend_from_slice(body);
        out
    }

    fn serve(responses: Vec<Vec<u8>>) -> (Url, JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let root = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let task = thread::spawn(move || {
            let mut requests = Vec::new();
            for reply in responses {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    let count = stream.read(&mut buffer).unwrap();
                    assert_ne!(count, 0);
                    request.extend_from_slice(&buffer[..count]);
                }
                requests.push(String::from_utf8(request).unwrap());
                let _ = stream.write_all(&reply);
            }
            requests
        });
        (root, task)
    }

    fn tumblr(root: Url) -> Tumblr {
        Tumblr::new(Duration::from_secs(5))
            .unwrap()
            .with_network(&NetworkOptions::new(Some(String::new()), None).unwrap())
            .unwrap()
            .with_api_root(root)
    }

    #[tokio::test]
    async fn resolves_original_tokens_without_api_credentials_and_preserves_identity() {
        let original = "https://64.media.tumblr.com/new/token/s4096x4096/one.jpg";
        let body = format!("<html><img src=\"{original}\"></html>");
        let (root, server) = serve(vec![response("200 OK", body.as_bytes())]);
        let native = tumblr(root.clone()).with_api_key(Some("private-api-key"));
        let mut media = parse_post(&post(7), "alice").remove(0);
        let identity = media.archive_key();
        native
            .finish_original(&mut media, root.join("s99999x99999/one.jpg").unwrap())
            .await;
        assert_eq!(media.url.as_str(), original);
        assert_eq!(media.archive_key(), identity);
        assert!(!media.handoff.contains_key("_fallback"));
        let requests = server.join().unwrap();
        let request = requests[0].to_ascii_lowercase();
        assert!(request.starts_with("get /s99999x99999/one.jpg "));
        assert!(request.contains("accept: text/html,*/*;q=0.8\r\n"));
        assert!(!request.contains("api_key") && !request.contains("private-api-key"));
        assert!(!request.contains("authorization:") && !request.contains("cookie:"));
    }

    #[tokio::test]
    async fn rejects_unsafe_token_urls_and_keeps_a_download_fallback() {
        for src in [
            "https://evil.example/one.jpg",
            "http://64.media.tumblr.com/one.jpg",
            "https://user:pass@64.media.tumblr.com/one.jpg",
            "https://64.media.tumblr.com:444/one.jpg",
            "https://64.media.tumblr.com.evil.example/one.jpg",
        ] {
            let html = format!("<img src=\"{src}\">");
            let (root, server) = serve(vec![response("200 OK", html.as_bytes())]);
            assert!(
                tumblr(root.clone()).resolve_original(&root).await.is_none(),
                "{src}"
            );
            assert_eq!(server.join().unwrap().len(), 1);
        }
        let (root, server) = serve(vec![response("404 Not Found", b"")]);
        let mut media = parse_post(&post(7), "alice").remove(0);
        let original = root.join("s99999x99999/one.jpg").unwrap();
        tumblr(root)
            .finish_original(&mut media, original.clone())
            .await;
        assert_eq!(media.url, original);
        assert_eq!(media.handoff["_fallback"], json!([PHOTO]));
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn uncapped_photos_skip_token_fetches_and_gifs_offer_gifv_fallback() {
        // No listener is started: finishing these files must not perform I/O.
        let native = tumblr(Url::parse("http://127.0.0.1:9/").unwrap());
        let mut count = 0;
        for event in group(parse_post(&post(7), "alice"), "tumblr") {
            native.finish_event(event).await;
            count += 1;
        }
        assert_eq!(count, 6);
        let gif = parse_post(
            &json!({"id":1,"body":"<img src=\"https://64.media.tumblr.com/tumblr_HASH_500.gif?token=yes\">"}),
            "alice",
        );
        let event = native
            .finish_event(ExtractionEvent::Media(Box::new(
                gif.into_iter().next().unwrap(),
            )))
            .await;
        let ExtractionEvent::Media(gif) = &event else {
            panic!("expected media");
        };
        assert_eq!(
            gif.url.as_str(),
            "https://64.media.tumblr.com/tumblr_HASH_1280.gif?token=yes"
        );
        assert_eq!(
            gif.handoff["_fallback"],
            json!(["https://64.media.tumblr.com/tumblr_HASH_1280.gifv?token=yes"])
        );
        assert!(gif.headers["Accept"].contains("image/webp"));
        assert!(!gif.handoff.contains_key("_tumblr_original"));
    }

    #[test]
    fn selection_scopes_reblogs_post_types_and_inline_images() {
        let value = json!({"id":1,"type":"photo","blog":{"uuid":"t:alice"},
            "reblogged_from_id":2,"reblogged_root_uuid":"t:alice",
            "photos":[{"original_size":{"url":PHOTO,"width":2048,"height":3072}}],
            "body":"<img src=\"https://64.media.tumblr.com/body.jpg\">",
            "question":"<img src=\"https://64.media.tumblr.com/question.jpg\">",
            "reblog":{"comment":"<img src=\"https://64.media.tumblr.com/comment.jpg\">",
                      "tree_html":"<img src=\"https://64.media.tumblr.com/tree.jpg\">"}});
        let selection = Selection {
            reblogs: Reblogs::OriginalOnly,
            ..Selection::default()
        };
        assert!(parse_post_selected(&value, "alice", &selection).is_empty());
        let selection = Selection {
            reblogs: Reblogs::SameBlog,
            inline: Inline::Reblog,
            ..Selection::default()
        };
        let media = parse_post_selected(&value, "alice", &selection);
        assert_eq!(
            media.iter().map(|m| m.url.path()).collect::<Vec<_>>(),
            [
                Url::parse(PHOTO).unwrap().path(),
                "/question.jpg",
                "/comment.jpg"
            ]
        );
        let mut other = value.clone();
        other["reblogged_root_uuid"] = json!("t:other");
        assert!(parse_post_selected(&other, "alice", &selection).is_empty());
        let mut fallback = value.clone();
        fallback["blog"] = json!({"name":"renamed"});
        fallback["reblogged_root_uuid"] = json!("renamed.tumblr.com");
        assert!(!parse_post_selected(&fallback, "alice", &selection).is_empty());
        fallback["reblogged_root_uuid"] = json!("alice.tumblr.com");
        assert!(parse_post_selected(&fallback, "alice", &selection).is_empty());
        let selection = Selection {
            inline: Inline::Original,
            ..Selection::default()
        };
        let media = parse_post_selected(&value, "alice", &selection);
        assert!(media.last().unwrap().url.path().ends_with("/tree.jpg"));
        other["reblog"]["tree_html"] = json!("");
        assert!(
            parse_post_selected(&other, "alice", &selection)
                .last()
                .unwrap()
                .url
                .path()
                .ends_with("/comment.jpg")
        );
        let selection = Selection {
            inline: Inline::Disabled,
            original: false,
            ..Selection::default()
        };
        let media = parse_post_selected(&value, "alice", &selection);
        assert_eq!(media.len(), 1);
        assert!(!media[0].handoff.contains_key("_tumblr_original"));
        assert_eq!(
            media[0].metadata.as_ref().unwrap().media_id,
            parse_post(&value, "alice")[0]
                .metadata
                .as_ref()
                .unwrap()
                .media_id
        );
        let selection = Selection::default().with_posts(["video".into()]).unwrap();
        assert!(parse_post_selected(&value, "alice", &selection).is_empty());
        assert!(Selection::default().with_posts(["unknown".into()]).is_err());
        for invalid in [Value::Null, json!(42), json!("unsupported")] {
            let mut malformed = value.clone();
            malformed["type"] = invalid;
            assert!(parse_post_selected(&malformed, "alice", &selection).is_empty());
        }
        assert!(
            parse_post_selected(
                &value,
                "alice",
                &Selection::default().with_posts([]).unwrap()
            )
            .is_empty()
        );
    }

    #[tokio::test]
    async fn filtered_pages_keep_offsets_and_single_posts_ignore_type_filter() {
        let photo = |id| json!({"id":id,"type":"photo","photos":[{"original_size":{"url":PHOTO}}]});
        let video =
            json!({"id":2,"type":"video","video_url":"https://64.media.tumblr.com/video.mp4"});
        let pages = [
            json!({"response":{"posts":[photo(1)],"total_posts":2}}),
            json!({"response":{"posts":[video],"total_posts":2}}),
        ];
        let (root, server) = serve(
            pages
                .iter()
                .map(|p| response("200 OK", p.to_string().as_bytes()))
                .collect(),
        );
        let native =
            tumblr(root).with_selection(Selection::default().with_posts(["video".into()]).unwrap());
        let files: Vec<_> = native
            .stream(&Url::parse("https://alice.tumblr.com/").unwrap())
            .collect()
            .await;
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0]
                .as_ref()
                .unwrap()
                .metadata
                .as_ref()
                .unwrap()
                .post_id,
            "2"
        );
        let requests = server.join().unwrap();
        assert!(requests[1].contains("&offset=1&limit=50 "));
        let (root, server) = serve(vec![response(
            "200 OK",
            json!({"response":{"posts":[photo(1)]}})
                .to_string()
                .as_bytes(),
        )]);
        let native = tumblr(root).with_selection(Selection::default().with_posts([]).unwrap());
        assert_eq!(
            native
                .stream(&Url::parse("https://alice.tumblr.com/post/1").unwrap())
                .collect::<Vec<_>>()
                .await
                .len(),
            1
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn original_disabled_emits_capped_images_without_token_requests() {
        let body = json!({"response":{"posts":[{"id":1,"photos":[{"original_size":{
            "url":"https://64.media.tumblr.com/abc/def/s2048x3072/one.jpg", "width":2048,"height":1536
        }}]}],"total_posts":1}}).to_string();
        // With no proxy and the localhost API fixture, a mistaken token lookup
        // would contact the CDN; the short timeout bounds this regression.
        let (root, server) = serve(vec![response("200 OK", body.as_bytes())]);
        let native = tumblr(root).with_selection(Selection {
            original: false,
            ..Selection::default()
        });
        let mut files = native.stream(&Url::parse("https://alice.tumblr.com/").unwrap());
        let first = tokio::time::timeout(Duration::from_secs(1), files.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        drop(files);
        assert_eq!(first.url.path(), "/abc/def/s2048x3072/one.jpg");
        assert!(!first.handoff.contains_key("_tumblr_original"));
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn fetches_post_with_retries_and_api_key() {
        let body = json!({"response": {"posts": [post(123)]}}).to_string();
        let (root, server) = serve(vec![
            response("503 Service Unavailable", b""),
            response("200 OK", body.as_bytes()),
        ]);
        let events: Vec<_> = tumblr(root)
            .with_retries(1)
            .with_api_key(Some("secret"))
            .events(&Url::parse("https://www.tumblr.com/alice/123").unwrap())
            .collect()
            .await;
        assert_eq!(events.len(), 6, "{events:?}");
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with(
            "GET /v2/blog/alice.tumblr.com/posts?api_key=secret&npf=false&reblog_info=true&id=123 "
        ));
    }

    #[tokio::test]
    async fn paginates_tagged_posts_by_offset() {
        let first: Vec<Value> = (1..=20).map(post).collect();
        let pages = [
            json!({"response": {"posts": first, "total_posts": 25}}),
            json!({"response": {"posts": [post(21)], "total_posts": 25}}),
            json!({"response": {"posts": []}}),
        ];
        let (root, server) = serve(
            pages
                .iter()
                .map(|page| response("200 OK", page.to_string().as_bytes()))
                .collect(),
        );
        let media: Vec<_> = tumblr(root)
            .stream(&Url::parse("https://alice.tumblr.com/tagged/a+b").unwrap())
            .collect()
            .await;
        assert_eq!(media.len(), 84);
        let requests = server.join().unwrap();
        let key = format!("api_key={DEFAULT_API_KEY}");
        assert!(requests[0].contains(&key));
        assert!(
            requests[0].contains("&tag=a+b&offset=0&limit=50 "),
            "{}",
            requests[0]
        );
        assert!(requests[1].contains("&offset=20&limit=50 "));
        assert!(requests[2].contains("&offset=21&limit=50 "));
    }

    #[tokio::test]
    async fn dropping_after_first_file_leaves_later_image_tokens_unrequested() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        for (input, skip_reblogs) in [
            ("https://alice.tumblr.com/", false),
            ("https://alice.tumblr.com/post/1", false),
            ("https://alice.tumblr.com/", true),
        ] {
            let photo = |name: &str| {
                json!({"original_size":{
                    "url":format!("https://64.media.tumblr.com/abc/def/s2048x3072/{name}.jpg"),
                    "width":2048,"height":1536
                }})
            };
            let mut body = json!({"response":{"total_posts":2,"posts":[
                {"id":1,"photos":[photo("one"),photo("two")]},
                {"id":2,"photos":[photo("three")]}
            ]}});
            if skip_reblogs {
                body["response"]["posts"].as_array_mut().unwrap().insert(
                    0,
                    json!({
                        "id":0,"reblogged_from_id":99,"photos":[photo("skipped")]
                    }),
                );
                body["response"]["total_posts"] = json!(3);
            }
            let body = body.to_string();
            // A local HTTP proxy records every attempted CDN token request as
            // CONNECT, without contacting Tumblr or needing a TLS fixture.
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let root = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let server_stop = stop.clone();
            let server = thread::spawn(move || {
                let mut requests = Vec::new();
                while !server_stop.load(Ordering::SeqCst) {
                    let (mut socket, _) = match listener.accept() {
                        Ok(connection) => connection,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(1));
                            continue;
                        }
                        Err(error) => panic!("fixture accept: {error}"),
                    };
                    socket.set_nonblocking(false).unwrap();
                    socket
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    let mut request = Vec::new();
                    let mut buffer = [0; 1024];
                    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                        let count = socket.read(&mut buffer).unwrap();
                        assert_ne!(count, 0);
                        request.extend_from_slice(&buffer[..count]);
                    }
                    let request = String::from_utf8(request).unwrap();
                    let reply = if request.starts_with("CONNECT ") {
                        response("502 Bad Gateway", b"")
                    } else {
                        response("200 OK", body.as_bytes())
                    };
                    requests.push(request);
                    socket.write_all(&reply).unwrap();
                }
                requests
            });
            let native = tumblr(root.clone())
                .with_selection(Selection {
                    reblogs: if skip_reblogs {
                        Reblogs::OriginalOnly
                    } else {
                        Reblogs::All
                    },
                    ..Selection::default()
                })
                .with_network(&NetworkOptions::new(Some(root.to_string()), None).unwrap())
                .unwrap();
            let mut files = native.stream(&Url::parse(input).unwrap());
            let first = files.next().await.unwrap().unwrap();
            drop(files);
            stop.store(true, Ordering::SeqCst);
            let requests = server.join().unwrap();
            assert_eq!(requests.len(), 2, "{input}: {requests:?}");
            assert!(requests[0].starts_with("GET "));
            assert!(requests[1].starts_with("CONNECT 64.media.tumblr.com:443 "));
            assert!(first.url.path().ends_with("/s99999x99999/one.jpg"));
            assert_eq!(
                first.handoff["_fallback"],
                json!(["https://64.media.tumblr.com/abc/def/s2048x3072/one.jpg"])
            );
            assert!(!first.handoff.contains_key("_tumblr_original"));
        }
    }

    #[tokio::test]
    async fn terminal_errors_do_not_leak_key() {
        let (root, server) = serve(vec![response("401 Unauthorized", b"{}")]);
        let error = tumblr(root)
            .with_retries(3)
            .with_api_key(Some("secret-key"))
            .stream(&Url::parse("https://alice.tumblr.com/").unwrap())
            .collect::<Vec<_>>()
            .await
            .remove(0)
            .unwrap_err()
            .to_string();
        assert!(error.contains("api-key"));
        assert!(!error.contains("secret-key") && !error.contains("127.0.0.1"));
        assert_eq!(server.join().unwrap().len(), 1);
    }
}
