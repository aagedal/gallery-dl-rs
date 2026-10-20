//! Experimental native Kemono/Coomer post, creator, global and Discord extraction.
//!
//! Mirrors gallery_dl/extractor/kemono.py for single posts, creator listings
//! and global feeds using the public `/api/v1` JSON API (which requires
//! `Accept: text/css`). `kemono.*` hosts use the `kemono` category and
//! `coomer.*` hosts the `coomer` category. Files are requested from
//! `https://HOST/data{path}`; the root host redirects to an `nN.HOST` CDN
//! subdomain, which the downloader allows for these categories only. The
//! service name is kept in the `instance` metadata field and the archive key.
use crate::{
    Result, error,
    event::{ExtractionEvent, Post, group},
    extract::{Media, MediaJob, PostMetadata},
    file_metadata::FileMetadata,
    network::NetworkOptions,
    transport::{RateLimits, Redirects, retry_after},
};
use futures_util::{
    StreamExt,
    stream::{self, BoxStream},
};
use reqwest::{Client, StatusCode, header};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    sync::Arc,
    time::{Duration, SystemTime},
};
use url::Url;

#[path = "kemono_discord.rs"]
mod discord;
pub use discord::{DiscordMetadata, parse_discord_post, valid_discord_id};
#[path = "kemono_session.rs"]
mod session;
pub use session::CookieSession;
#[path = "kemono_profile.rs"]
mod profile;
pub(crate) use profile::parse_profile_value;
pub use profile::{CreatorMetadata, parse_creator_profile};
#[path = "kemono_revisions.rs"]
mod revisions;
use revisions::RevisionPost;
pub use revisions::{RevisionMetadata, RevisionMode, RevisionOrder};

const USER_AGENT: &str = concat!(
    "gallery-dl-rs/",
    env!("CARGO_PKG_VERSION"),
    " (native Kemono extractor)"
);
const MAX_RESPONSE: usize = 32 * 1024 * 1024;
const PAGE_SIZE: usize = 50;
const MAX_PAGES: usize = 200;
/// Site domains (without optional `www.`/`beta.` prefixes) and categories.
pub const HOSTS: [(&str, &str); 6] = [
    ("kemono.cr", "kemono"),
    ("kemono.su", "kemono"),
    ("kemono.party", "kemono"),
    ("coomer.st", "coomer"),
    ("coomer.su", "coomer"),
    ("coomer.party", "coomer"),
];

/// Category for a supported site host; CDN subdomains (`n1.kemono.cr`) are
/// not site hosts.
pub fn category(host: &str) -> Option<&'static str> {
    let host = host
        .strip_prefix("www.")
        .or_else(|| host.strip_prefix("beta."))
        .unwrap_or(host);
    HOSTS
        .iter()
        .find(|(domain, _)| *domain == host)
        .map(|(_, category)| *category)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Posts {
        host: String,
    },
    Discord {
        host: String,
        server: String,
        channel: Option<String>,
    },
    Post {
        host: String,
        service: String,
        user: String,
        post: String,
    },
    Revisions {
        host: String,
        service: String,
        user: String,
        post: String,
        revision: Option<u64>,
    },
    User {
        host: String,
        service: String,
        user: String,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PostOrder {
    #[default]
    Forward,
    Reverse,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Endpoint {
    #[default]
    Posts,
    Expanded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    File,
    Attachments,
    Inline,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DuplicateOptions {
    pub file: bool,
    pub attachment: bool,
    pub inline: bool,
}
impl DuplicateOptions {
    pub const fn all() -> Self {
        Self {
            file: true,
            attachment: true,
            inline: true,
        }
    }
    fn allows(self, kind: FileKind) -> bool {
        match kind {
            FileKind::File => self.file,
            FileKind::Attachments => self.attachment,
            FileKind::Inline => self.inline,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Selection {
    pub files: Option<Vec<FileKind>>,
    pub duplicates: DuplicateOptions,
    pub original: bool,
}

pub fn valid_service(service: &str) -> bool {
    (1..=32).contains(&service.len())
        && service
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// Creator and post IDs are numeric on most services and alphanumeric on a few.
pub fn valid_id(id: &str) -> bool {
    (1..=128).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
        && id != "."
        && id != ".."
}

/// File hashes are the stem of the hash-named `/xx/yy/HASH.ext` path.
pub fn valid_hash(hash: &str) -> bool {
    (1..=128).contains(&hash.len()) && hash.bytes().all(|b| b.is_ascii_alphanumeric())
}

pub fn target(url: &Url) -> Result<Target> {
    let unsupported = || {
        error(
            "unsupported Kemono/Coomer URL; expected /posts, /SERVICE/user/USER_ID[/post/POST_ID] or /discord/server/SERVER_ID[/CHANNEL_ID] on kemono.cr, kemono.su, coomer.st or coomer.su",
        )
    };
    let host = url.host_str().ok_or_else(unsupported)?;
    if !matches!(url.scheme(), "http" | "https")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || category(host).is_none()
    {
        return Err(unsupported());
    }
    let segments: Vec<_> = url
        .path_segments()
        .map(|segments| segments.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    let host = host.to_owned();
    match segments.as_slice() {
        ["discord", "server", server] if valid_discord_id(server) => {
            let channel = match url.fragment().filter(|fragment| !fragment.is_empty()) {
                Some(fragment) => {
                    let fragment = fragment.strip_prefix('/').unwrap_or(fragment);
                    let channel = fragment.strip_prefix("channel/").unwrap_or(fragment);
                    if !valid_discord_id(channel) {
                        return Err(unsupported());
                    }
                    Some(channel.into())
                }
                None => None,
            };
            Ok(Target::Discord {
                host,
                server: (*server).into(),
                channel,
            })
        }
        ["discord", "server", server, channel]
        | ["discord", "server", server, "channel", channel]
            if valid_discord_id(server) && valid_discord_id(channel) =>
        {
            Ok(Target::Discord {
                host,
                server: (*server).into(),
                channel: Some((*channel).into()),
            })
        }
        ["posts"] => {
            listing_query(url)?;
            Ok(Target::Posts { host })
        }
        [service, "user", user] if valid_service(service) && valid_id(user) => {
            listing_query(url)?;
            Ok(Target::User {
                host,
                service: (*service).into(),
                user: (*user).into(),
            })
        }
        [
            service,
            "user",
            user,
            "post",
            post,
            "revision" | "revisions",
        ] if valid_service(service) && valid_id(user) && valid_id(post) => Ok(Target::Revisions {
            host,
            service: (*service).into(),
            user: (*user).into(),
            post: (*post).into(),
            revision: None,
        }),
        [
            service,
            "user",
            user,
            "post",
            post,
            "revision" | "revisions",
            revision,
        ] if valid_service(service)
            && valid_id(user)
            && valid_id(post)
            && !revision.is_empty()
            && revision.len() <= 20
            && revision.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            let revision = revision.parse::<u64>().map_err(|_| unsupported())?;
            Ok(Target::Revisions {
                host,
                service: (*service).into(),
                user: (*user).into(),
                post: (*post).into(),
                revision: Some(revision),
            })
        }
        [service, "user", user, "post", post]
            if valid_service(service) && valid_id(user) && valid_id(post) =>
        {
            Ok(Target::Post {
                host,
                service: (*service).into(),
                user: (*user).into(),
                post: (*post).into(),
            })
        }
        _ => Err(unsupported()),
    }
}

#[derive(Default)]
struct ListingQuery {
    offset: usize,
    query: Option<String>,
    tag: Option<String>,
}

fn listing_query(url: &Url) -> Result<ListingQuery> {
    let invalid = || error("invalid Kemono listing query");
    if url.query().is_some_and(|query| query.len() > 8192) {
        return Err(invalid());
    }
    let mut parsed = ListingQuery::default();
    let mut seen = HashSet::new();
    for (key, value) in url.query_pairs() {
        if !matches!(key.as_ref(), "o" | "q" | "tag") {
            continue;
        }
        if !seen.insert(key.to_string()) {
            return Err(invalid());
        }
        if key == "o" {
            if value.is_empty()
                || value.len() > 20
                || !value.bytes().all(|byte| byte.is_ascii_digit())
            {
                return Err(invalid());
            }
            let offset: usize = value.parse().map_err(|_| invalid())?;
            parsed.offset = offset - offset % PAGE_SIZE;
            if parsed.offset.checked_add(MAX_PAGES * PAGE_SIZE).is_none() {
                return Err(invalid());
            }
        } else {
            if value.len() > 1024 || value.chars().any(char::is_control) {
                return Err(invalid());
            }
            if key == "q" {
                parsed.query = Some(value.into_owned());
            } else {
                parsed.tag = Some(value.into_owned());
            }
        }
    }
    Ok(parsed)
}

#[derive(Clone)]
struct Identity {
    service: String,
    user: String,
    id: String,
}

fn post_identity(
    post: &Value,
    context: Option<(&str, &str)>,
    expected: Option<&str>,
) -> Result<Identity> {
    let invalid = || error("Kemono response has invalid or contradictory post identity");
    if !post.is_object() {
        return Err(invalid());
    }
    let service = match post.get("service") {
        Some(Value::String(service)) if valid_service(service) => service.clone(),
        None => context
            .map(|(service, _)| service.to_owned())
            .ok_or_else(invalid)?,
        _ => return Err(invalid()),
    };
    let user = match post.get("user") {
        Some(_) => id_of(post, "user").ok_or_else(invalid)?,
        None => context
            .map(|(_, user)| user.to_owned())
            .ok_or_else(invalid)?,
    };
    if !valid_service(&service)
        || !valid_id(&user)
        || context.is_some_and(|(owner_service, owner)| service != owner_service || user != owner)
    {
        return Err(invalid());
    }
    let id = id_of(post, "id").ok_or_else(invalid)?;
    if expected.is_some_and(|expected| id != expected) {
        return Err(invalid());
    }
    Ok(Identity { service, user, id })
}

fn post_metadata(post: &Value, identity: &Identity) -> PostMetadata {
    PostMetadata {
        post_id: identity.id.clone(),
        username: None,
        creator: Some(CreatorMetadata::stub(&identity.service, &identity.user)),
        instance: Some(identity.service.clone()),
        caption: string(post, "title").map(Arc::from),
        timestamp: ["published", "added"]
            .into_iter()
            .find_map(|key| string(post, key).and_then(|date| crate::date::parse_date(date).ok())),
        ..PostMetadata::default()
    }
}

fn string<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// IDs may be JSON strings or numbers.
fn id_of(value: &Value, key: &str) -> Option<String> {
    match value.get(key)? {
        Value::String(s) => Some(s.to_owned()),
        Value::Number(n) if n.is_u64() => Some(n.to_string()),
        _ => None,
    }
    .filter(|id| valid_id(id))
}

/// Validate a server file path and return its hash and lowercase extension.
fn split_path(path: &str) -> Option<(&str, Option<String>)> {
    let rest = path.strip_prefix('/')?;
    if rest.is_empty()
        || !rest.split('/').all(|segment| {
            !segment.is_empty()
                && segment != "."
                && segment != ".."
                && segment
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        })
    {
        return None;
    }
    let base = rest.rsplit('/').next()?;
    let (stem, ext) = match base.rsplit_once('.') {
        Some((stem, ext)) => (stem, Some(ext)),
        None => (base, None),
    };
    valid_hash(stem).then(|| (stem, ext.and_then(clean_extension)))
}

fn clean_extension(ext: &str) -> Option<String> {
    let ext = ext.to_ascii_lowercase();
    ((1..=8).contains(&ext.len()) && ext.bytes().all(|b| b.is_ascii_alphanumeric()))
        .then(|| if ext == "jpeg" { "jpg".into() } else { ext })
}

const MAX_POST_FILES: usize = 10_000;

fn safe_file_path(path: &str) -> bool {
    path.len() <= 4096
        && path.starts_with('/')
        && path.len() > 1
        && path[1..].split('/').all(|segment| {
            !segment.is_empty()
                && segment != "."
                && segment != ".."
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        })
}

fn image_extension(ext: &str) -> bool {
    matches!(
        ext,
        "jpg"
            | "jpeg"
            | "png"
            | "gif"
            | "bmp"
            | "svg"
            | "psd"
            | "ico"
            | "webp"
            | "avif"
            | "heic"
            | "heif"
    )
}
fn archive_extension(ext: &str) -> bool {
    matches!(
        ext,
        "zip" | "rar" | "7z" | "tar" | "gz" | "bz2" | "lzma" | "xz"
    )
}

/// Hash recognition follows upstream's `/xx/yy/64-lowercase-hex` pattern.
fn content_hash(path: &str) -> Option<&str> {
    let mut parts = path.strip_prefix('/')?.split('/');
    let hex = |value: &str, len| {
        value.len() == len
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    };
    if !hex(parts.next()?, 2) || !hex(parts.next()?, 2) {
        return None;
    }
    let file = parts.next()?;
    let hash = file.split('.').next()?;
    (parts.next().is_none() && hex(hash, 64)).then_some(hash)
}

/// Inline extraction is deliberately limited to upstream's double-quoted src
/// values on its two canonical origins and safe relative paths.
fn inline_paths(post: &Value) -> Vec<String> {
    let Some(content) = post.get("content").and_then(Value::as_str) else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    for (at, _) in content.match_indices("src=\"") {
        let rest = &content[at + 5..];
        let Some((raw, _)) = rest.split_once('"') else {
            continue;
        };
        if raw.len() > 4096 {
            continue;
        }
        let normalized = raw.replace('\\', "/");
        if normalized.contains('%') || normalized.split('/').any(|part| matches!(part, "." | ".."))
        {
            continue;
        }
        let path = if normalized.starts_with('/') {
            normalized
        } else {
            let Some(path) = [
                "https://kemono.cr",
                "http://kemono.cr",
                "https://coomer.st",
                "http://coomer.st",
            ]
            .into_iter()
            .find_map(|origin| normalized.strip_prefix(origin)) else {
                continue;
            };
            path.to_owned()
        };
        if safe_file_path(&path)
            && (path.starts_with("/inline/")
                || content_hash(&path).is_some_and(|_| {
                    path.rsplit_once('.')
                        .is_some_and(|(_, ext)| !ext.is_empty())
                }))
        {
            paths.push(path);
        }
        if paths.len() >= MAX_POST_FILES {
            break;
        }
    }
    paths
}

#[derive(Clone)]
struct SelectedFile {
    path: String,
    name: Option<Arc<str>>,
    kind: FileKind,
}

fn source_file<'a>(
    file: &'a Value,
    kind: FileKind,
    names: &mut HashMap<&'a str, Arc<str>>,
) -> Option<SelectedFile> {
    // Reject oversized source paths before allocating a normalized copy.
    let path = file.get("path")?.as_str()?;
    if path.len() > 4096 || path.trim().is_empty() {
        return None;
    }
    let name = string(file, "name")
        .map(|name| names.entry(name).or_insert_with(|| Arc::from(name)).clone());
    Some(SelectedFile {
        path: path.trim().replace('\\', "/"),
        name,
        kind,
    })
}

fn selected_files(post: &Value, category: &str, selection: &Selection) -> Vec<SelectedFile> {
    let default = if category == "coomer" {
        [FileKind::File, FileKind::Attachments, FileKind::Inline]
    } else {
        [FileKind::Attachments, FileKind::File, FileKind::Inline]
    };
    let kinds = selection.files.as_deref().unwrap_or(&default);
    let mut files = Vec::new();
    let mut names = HashMap::new();
    let mut inline = None;
    for kind in kinds {
        match kind {
            FileKind::File => {
                if let Some(file) = post
                    .get("file")
                    .and_then(|file| source_file(file, *kind, &mut names))
                {
                    files.push(file);
                }
            }
            FileKind::Attachments => {
                if let Some(attachments) = post.get("attachments").and_then(Value::as_array) {
                    for file in attachments {
                        if let Some(file) = source_file(file, *kind, &mut names) {
                            files.push(file);
                        }
                        if files.len() >= MAX_POST_FILES {
                            break;
                        }
                    }
                }
            }
            FileKind::Inline => {
                let inline = inline.get_or_insert_with(|| {
                    inline_paths(post)
                        .into_iter()
                        .map(|path| SelectedFile {
                            name: Some(Arc::from(path.as_str())),
                            path,
                            kind: FileKind::Inline,
                        })
                        .collect::<Vec<_>>()
                });
                files.extend(inline.iter().take(MAX_POST_FILES - files.len()).cloned());
            }
        }
        if files.len() >= MAX_POST_FILES {
            break;
        }
    }
    files
}

/// Parse a post using the category's upstream file order and thumbnail defaults.
pub fn parse_post(post: &Value, host: &str, service: &str, user: &str) -> Vec<Media> {
    parse_post_with_selection(post, host, service, user, &Selection::default())
}

pub fn parse_post_with_selection(
    post: &Value,
    host: &str,
    service: &str,
    user: &str,
    selection: &Selection,
) -> Vec<Media> {
    selected_post(post, host, service, user, selection).0
}

/// The count includes selected nonimages that thumbnail mode does not emit.
fn selected_post(
    post: &Value,
    host: &str,
    service: &str,
    user: &str,
    selection: &Selection,
) -> (Vec<Media>, usize) {
    let Some(category) = category(host) else {
        return (Vec::new(), 0);
    };
    let Ok(identity) = post_identity(post, Some((service, user)), None) else {
        return (Vec::new(), 0);
    };
    let host = host
        .strip_prefix("www.")
        .or_else(|| host.strip_prefix("beta."))
        .unwrap_or(host);
    let base = post_metadata(post, &identity);
    let mut hashes = HashSet::new();
    let mut count = 0;
    let mut media = Vec::new();
    for file in selected_files(post, category, selection) {
        if !safe_file_path(&file.path) {
            continue;
        }
        let hash = content_hash(&file.path);
        let parsed = split_path(&file.path);
        if file.kind != FileKind::Inline && parsed.is_none() {
            continue;
        }
        if let Some(hash) = hash {
            if !selection.duplicates.allows(file.kind) && hashes.contains(hash) {
                continue;
            }
            hashes.insert(hash.to_owned());
        }
        let path_ext = file
            .path
            .rsplit('/')
            .next()
            .and_then(|file| file.rsplit_once('.'))
            .and_then(|(_, ext)| clean_extension(ext));
        let name_ext = file
            .name
            .as_deref()
            .and_then(|name| name.rsplit(['/', '\\']).next())
            .and_then(|name| name.rsplit_once('.'))
            .filter(|(stem, _)| !stem.is_empty())
            .and_then(|(_, ext)| clean_extension(ext));
        let output_ext = name_ext
            .or_else(|| path_ext.clone())
            .unwrap_or_else(|| "bin".into());
        count += 1;
        if !selection.original && !path_ext.as_deref().is_some_and(image_extension) {
            continue;
        }
        let url = if selection.original {
            format!("https://{host}/data{}", file.path)
        } else {
            format!("https://img.{host}/thumbnail/data{}", file.path)
        };
        let Ok(url) = Url::parse(&url) else {
            continue;
        };
        let ext = if selection.original {
            output_ext
        } else {
            "jpg".into()
        };
        let archive = path_ext.as_deref().is_some_and(archive_extension)
            || path_ext.as_deref() == Some("bin") && archive_extension(&ext);
        let kind = if archive {
            "archive"
        } else {
            match file.kind {
                FileKind::File => "file",
                FileKind::Attachments => "attachment",
                FileKind::Inline => "inline",
            }
        };
        let mut metadata = base.clone();
        metadata.file = Some(FileMetadata {
            description: file.name,
            media_type: Some(Arc::from(kind)),
            ..FileMetadata::default()
        });
        metadata.media_id = hash
            .or_else(|| {
                if file.kind != FileKind::Inline {
                    parsed.as_ref().map(|(hash, _)| *hash)
                } else {
                    None
                }
            })
            .unwrap_or_default()
            .to_owned();
        metadata.index = count;
        media.push(Media {
            url,
            job: MediaJob::DirectHttp,
            category: category.into(),
            filename: format!("{}_{count:02}.{ext}", identity.id),
            metadata: Some(metadata),
            headers: BTreeMap::from([
                ("Accept-Encoding".into(), "identity".into()),
                (
                    "Referer".into(),
                    format!("https://{host}/{service}/user/{user}/post/{}", identity.id),
                ),
            ]),
            handoff: BTreeMap::new(),
        });
    }
    (media, count)
}

fn json(bytes: &[u8]) -> Result<Value> {
    if bytes.len() > MAX_RESPONSE {
        return Err(error("Kemono response exceeds limit"));
    }
    serde_json::from_slice(bytes).map_err(|_| error("Kemono returned invalid JSON"))
}

/// Parse a post response: either `{"post": {...}, ...}` or a bare post.
/// Empty posts are valid and still have post boundaries in the event stream.
pub fn parse_single(
    bytes: &[u8],
    host: &str,
    service: &str,
    user: &str,
    id: &str,
) -> Result<Vec<Media>> {
    let post = single_post(bytes, service, user, id)?;
    Ok(parse_post(&post, host, service, user))
}

fn single_post(bytes: &[u8], service: &str, user: &str, id: &str) -> Result<Value> {
    let body = single_response(bytes, service, user, id)?;
    Ok(body.get("post").unwrap_or(&body).clone())
}

fn single_response(bytes: &[u8], service: &str, user: &str, id: &str) -> Result<Value> {
    let body = json(bytes)?;
    post_identity(
        body.get("post").unwrap_or(&body),
        Some((service, user)),
        Some(id),
    )?;
    Ok(body)
}

/// One listing page: per-post files (including empty posts) and raw post count.
pub struct Page {
    pub posts: Vec<Vec<Media>>,
    pub count: usize,
}

fn listing_entries(bytes: &[u8], global: bool) -> Result<Vec<Value>> {
    let body = json(bytes)?;
    let entries = if global {
        body.get("posts").and_then(Value::as_array)
    } else {
        body.as_array()
    }
    .ok_or_else(|| error("Kemono response is not a post list"))?;
    if entries.len() > PAGE_SIZE {
        return Err(error("Kemono post list exceeds page limit"));
    }
    Ok(entries.clone())
}

pub fn parse_listing(bytes: &[u8], host: &str, service: &str, user: &str) -> Result<Page> {
    parse_listing_context(bytes, host, Some((service, user)))
}

pub fn parse_global_listing(bytes: &[u8], host: &str) -> Result<Page> {
    parse_listing_context(bytes, host, None)
}

fn parse_listing_context(bytes: &[u8], host: &str, context: Option<(&str, &str)>) -> Result<Page> {
    let entries = listing_entries(bytes, context.is_none())?;
    let posts = entries
        .iter()
        .map(|post| {
            let identity = post_identity(post, context, None)?;
            Ok(parse_post(post, host, &identity.service, &identity.user))
        })
        .collect::<Result<_>>()?;
    Ok(Page {
        posts,
        count: entries.len(),
    })
}

pub struct Kemono {
    client: Client,
    network: NetworkOptions,
    api_root: Option<Url>,
    timeout: Duration,
    request_timeout: Duration,
    retries: u8,
    rate_limits: RateLimits,
    expand: bool,
    max_posts: Option<usize>,
    selection: Selection,
    revisions: RevisionMode,
    revision_order: RevisionOrder,
    metadata: bool,
    profiles: tokio::sync::Mutex<profile::Cache>,
    session: CookieSession,
    post_order: PostOrder,
    endpoint: Endpoint,
}

struct ListingState {
    host: String,
    context: Option<(String, String)>,
    query: ListingQuery,
    offset: usize,
    pages: usize,
    emitted: usize,
    done: bool,
    raw: VecDeque<Value>,
    revisions: VecDeque<RevisionPost>,
    revision_identity: Option<Identity>,
    revision_creator: Option<Arc<CreatorMetadata>>,
    seen_pages: HashSet<[u8; 32]>,
    pending: VecDeque<ExtractionEvent>,
}

impl Kemono {
    pub fn new(timeout: Duration) -> Result<Self> {
        Ok(Self {
            client: NetworkOptions::default().client(USER_AGENT, timeout)?,
            network: NetworkOptions::default(),
            api_root: None,
            timeout,
            request_timeout: timeout,
            retries: 0,
            rate_limits: RateLimits::default(),
            expand: false,
            max_posts: None,
            selection: Selection::default(),
            revisions: RevisionMode::None,
            revision_order: RevisionOrder::Newest,
            metadata: true,
            profiles: tokio::sync::Mutex::new(profile::Cache::default()),
            session: CookieSession::default(),
            post_order: PostOrder::Forward,
            endpoint: Endpoint::Posts,
        })
    }

    pub fn with_post_order(mut self, order: PostOrder) -> Self {
        self.post_order = order;
        self
    }
    pub fn with_endpoint(mut self, endpoint: Endpoint) -> Self {
        self.endpoint = endpoint;
        self
    }

    pub fn with_cookies(mut self, cookies: crate::cookies::CookieJar) -> Self {
        self.session = CookieSession::from_cookie_jar(cookies);
        self
    }

    /// Enable optional descriptive creator profiles; immutable creator IDs are always retained.
    pub fn with_metadata(mut self, metadata: bool) -> Self {
        self.metadata = metadata;
        self
    }

    async fn creator(&self, host: &str, identity: &Identity) -> Arc<CreatorMetadata> {
        let stub = || CreatorMetadata::stub(&identity.service, &identity.user);
        if !self.metadata {
            return Arc::new(stub());
        }
        let key = (
            host.to_owned(),
            identity.service.clone(),
            identity.user.clone(),
        );
        // The guard is cancellation-safe and serializes first lookups. A dropped
        // HTTP future releases it without inserting a partial cache entry.
        let mut profiles = self.profiles.lock().await;
        if let Some(profile) = profiles.get(&key) {
            return profile;
        }
        let path = format!("api/v1/{}/user/{}/profile", identity.service, identity.user);
        let profile = match self.call(host, &path, &[]).await {
            Ok(bytes) => parse_creator_profile(&bytes, &identity.service, &identity.user)
                .unwrap_or_else(|_| stub()),
            Err(_) => stub(),
        };
        let profile = Arc::new(profile);
        profiles.insert(key, profile.clone());
        profile
    }

    pub fn with_revisions(mut self, revisions: RevisionMode) -> Self {
        self.revisions = revisions;
        self
    }

    pub fn with_revision_order(mut self, order: RevisionOrder) -> Self {
        self.revision_order = order;
        self
    }

    pub fn with_selection(mut self, selection: Selection) -> Self {
        self.selection = selection;
        self
    }

    /// Fetch each listing post's full detail response only when it is consumed.
    pub fn with_expand(mut self, expand: bool) -> Self {
        self.expand = expand;
        self
    }

    /// Limit raw posts, including posts with no files. Zero means unlimited.
    pub fn with_max_posts(mut self, max_posts: Option<usize>) -> Self {
        self.max_posts = max_posts.filter(|limit| *limit > 0);
        self
    }

    /// Apply proxy and user-agent settings to API requests and emitted media.
    pub fn with_network(mut self, network: &NetworkOptions) -> Result<Self> {
        self.client = network.client(USER_AGENT, self.timeout)?;
        self.network = network.clone();
        Ok(self)
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

    /// Override the API origin for every host; intended for local tests.
    pub fn with_api_root(mut self, root: Url) -> Self {
        self.api_root = Some(root);
        self
    }

    async fn call(&self, host: &str, path: &str, query: &[(&str, &str)]) -> Result<Vec<u8>> {
        let root = self
            .api_root
            .clone()
            .unwrap_or_else(|| Url::parse(&format!("https://{host}/")).expect("known host"));
        let mut endpoint = root.join(path).expect("validated Kemono path");
        if !query.is_empty() {
            endpoint.query_pairs_mut().extend_pairs(query);
        }
        tokio::time::timeout(self.timeout, self.request(&endpoint))
            .await
            .map_err(|_| error("native Kemono extraction timed out"))?
    }

    fn finish(&self, host: &str, post: &Value, identity: &Identity) -> VecDeque<ExtractionEvent> {
        let (mut media, count) = selected_post(
            post,
            host,
            &identity.service,
            &identity.user,
            &self.selection,
        );
        let category = category(host).expect("validated host");
        for item in &mut media {
            self.network.apply_media(item);
        }
        if media.is_empty() {
            VecDeque::from([
                ExtractionEvent::Post(Box::new(Post {
                    category: category.into(),
                    metadata: Some(post_metadata(post, identity)),
                    count: Some(count),
                })),
                ExtractionEvent::EndPost,
            ])
        } else {
            let mut events = group(media, category);
            if let Some(ExtractionEvent::Post(post)) = events.front_mut() {
                post.count = Some(count);
            }
            events
        }
    }

    fn finish_revision(
        &self,
        host: &str,
        post: RevisionPost,
        identity: &Identity,
        creator: &CreatorMetadata,
    ) -> VecDeque<ExtractionEvent> {
        let mut events = self.finish(host, &post.post, identity);
        for event in &mut events {
            match event {
                ExtractionEvent::Post(boundary) => {
                    if let Some(metadata) = &mut boundary.metadata {
                        metadata.creator = Some(creator.clone());
                        metadata.username = creator.name.clone();
                        metadata.revision = post.metadata.clone();
                    }
                }
                ExtractionEvent::Media(media) => {
                    if let Some(metadata) = &mut media.metadata {
                        metadata.creator = Some(creator.clone());
                        metadata.username = creator.name.clone();
                        metadata.revision = post.metadata.clone();
                    }
                    if let Some(id) = post
                        .metadata
                        .as_ref()
                        .and_then(|revision| revision.id)
                        .filter(|id| *id > 0)
                        && let Some((stem, ext)) = media.filename.rsplit_once('.')
                    {
                        media.filename = format!("{stem}_r{id}.{ext}");
                    }
                }
                ExtractionEvent::EndPost => {}
            }
        }
        events
    }

    async fn prepare_revisions(
        &self,
        host: &str,
        body: Value,
        identity: &Identity,
        explicit: Option<Option<u64>>,
    ) -> Result<VecDeque<RevisionPost>> {
        if self.revisions == RevisionMode::None && explicit.is_none() {
            let post = body.get("post").unwrap_or(&body).clone();
            return Ok(VecDeque::from([RevisionPost {
                post,
                metadata: None,
            }]));
        }
        let historical = match revisions::embedded(&body)? {
            Some(revisions) => revisions,
            None => {
                let path = format!(
                    "api/v1/{}/user/{}/post/{}/revisions",
                    identity.service, identity.user, identity.id
                );
                let bytes = self.call(host, &path, &[]).await?;
                revisions::response(&bytes)?
            }
        };
        let current = body.get("post").unwrap_or(&body).clone();
        revisions::prepare(
            current,
            historical,
            identity,
            self.revisions,
            self.revision_order,
            explicit,
        )
        .map(Into::into)
    }

    fn single_events(
        &self,
        host: String,
        service: String,
        user: String,
        post: String,
        explicit: Option<Option<u64>>,
    ) -> BoxStream<'_, Result<ExtractionEvent>> {
        stream::once(async move {
            let path = format!("api/v1/{service}/user/{user}/post/{post}");
            let body = self.call(&host, &path, &[]).await?;
            let body = single_response(&body, &service, &user, &post)?;
            let identity = Identity {
                service,
                user,
                id: post,
            };
            let revisions = self
                .prepare_revisions(&host, body, &identity, explicit)
                .await?;
            let creator = if revisions.is_empty() {
                None
            } else {
                Some(self.creator(&host, &identity).await)
            };
            Ok::<_, crate::Error>(
                stream::unfold(
                    (host, identity, creator, revisions, VecDeque::new()),
                    move |(host, identity, creator, mut revisions, mut pending)| async move {
                        loop {
                            if let Some(event) = pending.pop_front() {
                                return Some((
                                    Ok(event),
                                    (host, identity, creator, revisions, pending),
                                ));
                            }
                            pending = self.finish_revision(
                                &host,
                                revisions.pop_front()?,
                                &identity,
                                creator.as_deref().expect("prepared creator"),
                            );
                        }
                    },
                )
                .boxed(),
            )
        })
        .map(|result| match result {
            Ok(events) => events,
            Err(e) => stream::once(async move { Err(e) }).boxed(),
        })
        .flatten()
        .boxed()
    }

    async fn next_page(&self, state: &mut ListingState) -> Result<()> {
        let path = state.context.as_ref().map_or_else(
            || "api/v1/posts".to_owned(),
            |(service, user)| format!("api/v1/{service}/user/{user}/posts"),
        );
        let offset = state.offset.to_string();
        let mut query = vec![("o", offset.as_str())];
        if let Some(value) = &state.query.query {
            query.push(("q", value.as_str()));
        }
        if let Some(value) = &state.query.tag {
            query.push(("tag", value.as_str()));
        }
        let body = self.call(&state.host, &path, &query).await?;
        let entries = listing_entries(&body, state.context.is_none())?;
        // Identify the ordered source posts, independent of JSON formatting or
        // unrelated metadata. Do not validate entries eagerly: a raw post limit
        // may intentionally stop before a later malformed entry is consumed.
        let identities: Vec<_> = entries
            .iter()
            .map(|post| {
                (
                    post.get("service")
                        .and_then(Value::as_str)
                        .or_else(|| state.context.as_ref().map(|(service, _)| service.as_str())),
                    id_of(post, "user")
                        .or_else(|| state.context.as_ref().map(|(_, user)| user.clone())),
                    id_of(post, "id"),
                )
            })
            .collect();
        let fingerprint =
            Sha256::digest(serde_json::to_vec(&identities).expect("serializable identities"));
        if !entries.is_empty() && !state.seen_pages.insert(fingerprint.into()) {
            return Err(error("Kemono listing repeated a page"));
        }
        state.pages += 1;
        state.offset += PAGE_SIZE;
        state.done = entries.len() < PAGE_SIZE || state.pages >= MAX_PAGES;
        state.raw = entries.into();
        Ok(())
    }

    pub fn stream(&self, url: &Url) -> BoxStream<'_, Result<Media>> {
        crate::event::media_only(self.events(url))
    }

    /// Emit every raw post boundary; page and detail requests are both lazy.
    pub fn events(&self, url: &Url) -> BoxStream<'_, Result<ExtractionEvent>> {
        let target = match target(url) {
            Ok(target) => target,
            Err(e) => return stream::once(async move { Err(e) }).boxed(),
        };
        let (host, context) = match target {
            Target::Discord {
                host,
                server,
                channel,
            } => return discord::events(self, host, server, channel),
            Target::Post {
                host,
                service,
                user,
                post,
            } => return self.single_events(host, service, user, post, None),
            Target::Revisions {
                host,
                service,
                user,
                post,
                revision,
            } => return self.single_events(host, service, user, post, Some(revision)),
            Target::User {
                host,
                service,
                user,
            } => (host, Some((service, user))),
            Target::Posts { host } => (host, None),
        };
        let query = match listing_query(url) {
            Ok(query) => query,
            Err(e) => return stream::once(async move { Err(e) }).boxed(),
        };
        let state = ListingState {
            host,
            context,
            offset: query.offset,
            query,
            pages: 0,
            emitted: 0,
            done: false,
            raw: VecDeque::new(),
            revisions: VecDeque::new(),
            revision_identity: None,
            revision_creator: None,
            seen_pages: HashSet::new(),
            pending: VecDeque::new(),
        };
        stream::unfold(Some(state), move |state| async move {
            let mut state = state?;
            loop {
                if let Some(event) = state.pending.pop_front() {
                    return Some((Ok(event), Some(state)));
                }
                if let Some(revision) = state.revisions.pop_front() {
                    let identity = state
                        .revision_identity
                        .as_ref()
                        .expect("prepared source identity");
                    state.pending = self.finish_revision(
                        &state.host,
                        revision,
                        identity,
                        state.revision_creator.as_deref().expect("prepared creator"),
                    );
                    continue;
                }
                if self.max_posts.is_some_and(|limit| state.emitted >= limit) {
                    return None;
                }
                if let Some(mut post) = state.raw.pop_front() {
                    state.emitted += 1;
                    let context = state
                        .context
                        .as_ref()
                        .map(|(service, user)| (service.as_str(), user.as_str()));
                    let identity = match post_identity(&post, context, None) {
                        Ok(identity) => identity,
                        Err(e) => return Some((Err(e), None)),
                    };
                    if self.expand || self.endpoint == Endpoint::Expanded && state.context.is_some()
                    {
                        let path = format!(
                            "api/v1/{}/user/{}/post/{}",
                            identity.service, identity.user, identity.id
                        );
                        let result = async {
                            let body = self.call(&state.host, &path, &[]).await?;
                            single_response(&body, &identity.service, &identity.user, &identity.id)
                        }
                        .await;
                        match result {
                            Ok(detail) => post = detail,
                            Err(e) => return Some((Err(e), None)),
                        }
                    }
                    match self
                        .prepare_revisions(&state.host, post, &identity, None)
                        .await
                    {
                        Ok(revisions) => state.revisions = revisions,
                        Err(e) => return Some((Err(e), None)),
                    }
                    state.revision_creator = if state.revisions.is_empty() {
                        None
                    } else {
                        Some(self.creator(&state.host, &identity).await)
                    };
                    state.revision_identity = Some(identity);
                    continue;
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
        for attempt in 0..=self.retries {
            match self.attempt(endpoint, attempt).await {
                Ok(body) => return Ok(body),
                Err(failure) if failure.retry && attempt < self.retries => {
                    tokio::time::sleep(backoff(attempt)).await;
                }
                Err(failure) => return Err(failure.error),
            }
        }
        unreachable!()
    }

    async fn attempt(&self, endpoint: &Url, attempt: u8) -> std::result::Result<Vec<u8>, Failure> {
        let mut target = endpoint.clone();
        let mut redirects = Redirects::new(endpoint);
        let mut response = loop {
            self.rate_limits.wait(&target, self.timeout).await?;
            let response = self
                .client
                .get(target.clone())
                .headers(self.session.request_headers(&target)?)
                .timeout(self.request_timeout)
                // The Kemono API only answers with JSON for this Accept value.
                .header(header::ACCEPT, "text/css")
                .header(header::ACCEPT_ENCODING, "gzip")
                .send()
                .await
                .map_err(|_| Failure::transient("Kemono request failed; check network access"))?;
            self.session
                .store_response(response.url(), response.headers());
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
                            format!("Kemono rate limit; retry after {} seconds", delay.as_secs())
                        }
                        None => "Kemono rate limit; retry later".into(),
                    }
                } else {
                    format!("Kemono returned HTTP {status}")
                },
            ));
        }
        if matches!(status.as_u16(), 404 | 410) {
            return Err(error("Kemono post or creator not found").into());
        }
        if matches!(status.as_u16(), 401 | 403) {
            return Err(error("Kemono refused access").into());
        }
        if !status.is_success() {
            return Err(error(format!("Kemono returned HTTP {status}")).into());
        }
        let response_headers = response.headers().clone();
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| Failure::transient("could not read Kemono response"))?
        {
            if body.len() + chunk.len() > MAX_RESPONSE {
                return Err(error("Kemono response exceeded 32 MiB").into());
            }
            body.extend_from_slice(&chunk);
        }
        crate::transport::decode_api_body(body, &response_headers, MAX_RESPONSE)
            .await
            .map_err(Into::into)
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

    const HASH_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const HASH_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn post(id: &str) -> Value {
        json!({"id": id, "user": "42", "service": "patreon", "title": "Hello",
            "published": "2024-01-02T03:04:05", "added": "2024-02-02T00:00:00",
            "file": {"name": "cover.JPEG", "path": format!("/aa/11/{HASH_A}.jpeg")},
            "attachments": [
                {"name": "cover.JPEG", "path": format!("/aa/11/{HASH_A}.jpeg")},
                {"name": "bad", "path": "/../etc/passwd"},
                {"name": "clip.mp4", "path": format!("/bb/22/{HASH_B}")}]})
    }

    #[test]
    fn parses_post_and_dedupes_files() {
        for body in [json!({"post": post("7"), "attachments": []}), post("7")] {
            let body = body.to_string();
            let raw = single_post(body.as_bytes(), "patreon", "42", "7").unwrap();
            let media = parse_post_with_selection(
                &raw,
                "kemono.cr",
                "patreon",
                "42",
                &Selection {
                    original: true,
                    ..Selection::default()
                },
            );
            assert_eq!(media.len(), 2);
            assert_eq!(
                media[0].url.as_str(),
                format!("https://kemono.cr/data/aa/11/{HASH_A}.jpeg")
            );
            assert_eq!(media[0].category, "kemono");
            assert_eq!(media[0].filename, "7_01.jpg");
            assert_eq!(media[1].filename, "7_02.mp4");
            let meta = media[0].metadata.as_ref().unwrap();
            assert_eq!(meta.post_id, "7");
            assert_eq!(meta.media_id, HASH_A);
            assert_eq!(
                meta.creator.as_ref().map(|creator| creator.id.as_str()),
                Some("42")
            );
            assert_eq!(meta.caption.as_deref(), Some("Hello"));
            assert_eq!(meta.timestamp, Some(1704164645));
            assert_eq!(meta.index, 1);
            assert_eq!(meta.instance.as_deref(), Some("patreon"));
            assert_eq!(
                meta.file.as_ref().unwrap().description.as_deref(),
                Some("cover.JPEG")
            );
            let key = media[0].archive_key().unwrap();
            assert!(key.starts_with("kemono:"), "{key}");
            assert_ne!(Some(key.clone()), media[1].archive_key());
            let coomer = parse_post(&post("7"), "coomer.st", "patreon", "42");
            assert_eq!(coomer[0].category, "coomer");
            assert_ne!(Some(key.clone()), coomer[0].archive_key());
            let mut different_service = post("7");
            different_service["service"] = json!("fanbox");
            let other = parse_post(&different_service, "kemono.cr", "fanbox", "42");
            assert_ne!(Some(key), other[0].archive_key());
        }
    }

    #[test]
    fn rejects_mismatched_posts_and_retains_empty_posts() {
        let body = post("7").to_string();
        assert!(parse_single(body.as_bytes(), "kemono.cr", "patreon", "42", "8").is_err());
        let empty = json!({"id": 5, "attachments": []}).to_string();
        assert!(
            parse_single(empty.as_bytes(), "kemono.cr", "patreon", "42", "5")
                .unwrap()
                .is_empty()
        );
        assert!(parse_single(b"nope", "kemono.cr", "patreon", "42", "5").is_err());
        let page = parse_listing(
            json!([post("1"), {"id": "2"}]).to_string().as_bytes(),
            "kemono.cr",
            "patreon",
            "42",
        )
        .unwrap();
        assert_eq!((page.posts.len(), page.count), (2, 2));
        assert!(parse_listing(b"{}", "kemono.cr", "patreon", "42").is_err());
    }

    #[test]
    fn classifies_supported_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://kemono.su/patreon/user/42/post/7").unwrap(),
            Target::Post {
                host: "kemono.su".into(),
                service: "patreon".into(),
                user: "42".into(),
                post: "7".into(),
            }
        );
        assert_eq!(
            t("https://coomer.st/onlyfans/user/some_name").unwrap(),
            Target::User {
                host: "coomer.st".into(),
                service: "onlyfans".into(),
                user: "some_name".into(),
            }
        );
        assert!(t("https://www.kemono.cr/fanbox/user/1/").is_ok());
        for raw in [
            "https://kemono.cr/",
            "https://kemono.cr/patreon/user/",
            "https://kemono.cr/Patreon/user/1",
            "https://kemono.cr/patreon/user/1/post/",
            "https://kemono.cr/patreon/user/1/post/a%2Fb",
            "https://n1.kemono.cr/patreon/user/1",
            "https://kemono.cr:444/patreon/user/1",
            "https://kemono.cr.evil.example/patreon/user/1",
            "https://notkemono.cr/patreon/user/1",
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

    fn kemono(root: Url) -> Kemono {
        Kemono::new(Duration::from_secs(5))
            .unwrap()
            .with_network(&NetworkOptions::new(Some(String::new()), None).unwrap())
            .unwrap()
            .with_api_root(root)
            .with_metadata(false)
            .with_selection(Selection {
                original: true,
                ..Selection::default()
            })
    }

    #[tokio::test]
    async fn fetches_post_with_retries_and_css_accept() {
        let (root, server) = serve(vec![
            response("503 Service Unavailable", b""),
            response("200 OK", json!({"post": post("7")}).to_string().as_bytes()),
        ]);
        let events: Vec<_> = kemono(root)
            .with_retries(1)
            .events(&Url::parse("https://kemono.cr/patreon/user/42/post/7").unwrap())
            .collect()
            .await;
        assert_eq!(events.len(), 4, "{events:?}");
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /api/v1/patreon/user/42/post/7 "));
        let lower = requests[1].to_ascii_lowercase();
        assert!(lower.contains("accept: text/css"));
        assert!(lower.contains("user-agent: gallery-dl-rs/"));
    }

    #[tokio::test]
    async fn paginates_creator_posts_by_offset() {
        let full: Vec<Value> = (1..=50)
            .map(|i| {
                let mut post = post(&format!("{}", 100 + i));
                post["service"] = json!("onlyfans");
                post["user"] = json!("alice");
                post
            })
            .collect();
        let (root, server) = serve(vec![
            response("200 OK", Value::Array(full).to_string().as_bytes()),
            response("200 OK", b"[]"),
        ]);
        let media: Vec<_> = kemono(root)
            .stream(&Url::parse("https://coomer.su/onlyfans/user/alice").unwrap())
            .collect()
            .await;
        assert_eq!(media.len(), 100);
        assert_eq!(media[0].as_ref().unwrap().category, "coomer");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/v1/onlyfans/user/alice/posts?o=0 "));
        assert!(requests[1].starts_with("GET /api/v1/onlyfans/user/alice/posts?o=50 "));
    }

    #[test]
    fn global_listings_use_each_posts_owner_and_preserve_archive_identity() {
        let first = post("7");
        let mut second = post("7");
        second["service"] = json!("fanbox");
        second["user"] = json!("other_creator");
        let page = parse_global_listing(
            json!({"posts": [first, second]}).to_string().as_bytes(),
            "kemono.cr",
        )
        .unwrap();
        assert_eq!((page.count, page.posts.len()), (2, 2));
        let first = &page.posts[0][0];
        let second = &page.posts[1][0];
        assert_eq!(first.filename, second.filename);
        assert_eq!(
            first.metadata.as_ref().unwrap().instance.as_deref(),
            Some("patreon")
        );
        assert_eq!(
            second.metadata.as_ref().unwrap().instance.as_deref(),
            Some("fanbox")
        );
        assert_eq!(
            second
                .metadata
                .as_ref()
                .unwrap()
                .creator
                .as_ref()
                .map(|creator| creator.id.as_str()),
            Some("other_creator")
        );
        assert_ne!(first.archive_key(), second.archive_key());
        assert_eq!(
            first.archive_key(),
            parse_post(&post("7"), "kemono.cr", "patreon", "42")[0].archive_key()
        );
        assert!(parse_global_listing(b"[]", "kemono.cr").is_err());
        assert!(parse_global_listing(br#"{"posts":{}}"#, "kemono.cr").is_err());
    }

    #[test]
    fn listing_and_detail_identity_reject_contradictions_privately() {
        for (field, value) in [
            ("service", json!("secretservice")),
            ("user", json!("secretuser")),
            ("id", json!("secretpost")),
        ] {
            let mut body = post("7");
            body[field] = value;
            let error = parse_single(
                body.to_string().as_bytes(),
                "kemono.cr",
                "patreon",
                "42",
                "7",
            )
            .unwrap_err()
            .to_string();
            assert!(!error.contains("secret"));
            assert!(error.contains("identity"));
        }
        for body in [
            json!({"id": "7", "service": "patreon"}),
            json!({"id": "7", "user": "42"}),
            json!({"id": "7", "user": "../private", "service": "patreon"}),
            json!({"id": "7", "user": "42", "service": null}),
            json!({"id": 7.5, "user": "42", "service": "patreon"}),
        ] {
            assert!(
                parse_global_listing(json!({"posts": [body]}).to_string().as_bytes(), "kemono.cr")
                    .is_err()
            );
        }
        let minimal = json!({"id": "7", "file": {"path": "/aa/11/aa11.jpg"}});
        assert_eq!(
            parse_single(
                minimal.to_string().as_bytes(),
                "kemono.cr",
                "patreon",
                "42",
                "7"
            )
            .unwrap()
            .len(),
            1
        );
        assert!(
            parse_listing(
                json!([{ "id": "7", "user": "other", "service": "patreon" }])
                    .to_string()
                    .as_bytes(),
                "kemono.cr",
                "patreon",
                "42"
            )
            .is_err()
        );
    }

    #[test]
    fn queries_are_bounded_private_and_offsets_align_down() {
        assert_eq!(
            target(&Url::parse("https://kemono.cr/posts").unwrap()).unwrap(),
            Target::Posts {
                host: "kemono.cr".into()
            }
        );
        for path in ["/posts", "/patreon/user/42"] {
            let url = Url::parse(&format!(
                "https://kemono.cr{path}?o=149&q=red%20%26%20blue&tag=%E7%8C%AB"
            ))
            .unwrap();
            let query = listing_query(&url).unwrap();
            assert_eq!(query.offset, 100);
            assert_eq!(query.query.as_deref(), Some("red & blue"));
            assert_eq!(query.tag.as_deref(), Some("猫"));
            for query in [
                "o=-1",
                "o=secret",
                "o=999999999999999999999999",
                "q=secret%00",
                "tag=secret%0A",
                "q=secret&q=again",
                "o=1&o=2",
            ] {
                let error =
                    target(&Url::parse(&format!("https://kemono.cr{path}?{query}")).unwrap())
                        .unwrap_err()
                        .to_string();
                assert!(!error.contains("secret"));
                assert_eq!(error, "invalid Kemono listing query");
            }
        }
        let long = "s".repeat(1025);
        assert!(
            target(&Url::parse(&format!("https://kemono.cr/posts?q={long}")).unwrap()).is_err()
        );
    }

    #[tokio::test]
    async fn global_queries_are_encoded_and_mixed_creators_keep_order() {
        let mut other = post("8");
        other["service"] = json!("fanbox");
        other["user"] = json!("alice");
        let (root, server) = serve(vec![response(
            "200 OK",
            json!({"posts": [post("7"), other]}).to_string().as_bytes(),
        )]);
        let items: Vec<_> = kemono(root)
            .stream(
                &Url::parse("https://kemono.cr/posts?o=149&q=red%20%26%20blue&tag=%E7%8C%AB")
                    .unwrap(),
            )
            .collect()
            .await;
        let ids: Vec<_> = items
            .iter()
            .map(|item| {
                item.as_ref()
                    .unwrap()
                    .metadata
                    .as_ref()
                    .unwrap()
                    .post_id
                    .as_str()
            })
            .collect();
        assert_eq!(ids, ["7", "7", "8", "8"]);
        assert_eq!(
            items[2]
                .as_ref()
                .unwrap()
                .metadata
                .as_ref()
                .unwrap()
                .creator
                .as_ref()
                .map(|creator| creator.id.as_str()),
            Some("alice")
        );
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("GET /api/v1/posts?o=100&q=red+%26+blue&tag=%E7%8C%AB "));
    }

    #[tokio::test]
    async fn creator_queries_preserve_filters_on_later_pages() {
        let full: Vec<_> = (0..50)
            .map(|index| post(&(index + 1).to_string()))
            .collect();
        let (root, server) = serve(vec![
            response("200 OK", json!(full).to_string().as_bytes()),
            response("200 OK", b"[]"),
        ]);
        let items: Vec<_> = kemono(root)
            .stream(
                &Url::parse("https://kemono.cr/patreon/user/42?o=99&q=a%26b&tag=a%2Fb").unwrap(),
            )
            .collect()
            .await;
        assert_eq!(items.len(), 100);
        assert!(items.iter().all(Result::is_ok));
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with("GET /api/v1/patreon/user/42/posts?o=50&q=a%26b&tag=a%2Fb ")
        );
        assert!(
            requests[1].starts_with("GET /api/v1/patreon/user/42/posts?o=100&q=a%26b&tag=a%2Fb ")
        );
    }

    #[tokio::test]
    async fn expand_is_lazy_per_post_and_cancels_without_fetching_next_detail() {
        let listing = json!({"posts": [
            {"id": "7", "service": "patreon", "user": "42", "title": "summary"},
            {"id": "8", "service": "fanbox", "user": "alice"}
        ]});
        let mut detail = post("7");
        detail["title"] = json!("expanded");
        let (root, server) = serve(vec![
            response("200 OK", listing.to_string().as_bytes()),
            response("200 OK", json!({"post": detail}).to_string().as_bytes()),
        ]);
        let client = kemono(root).with_expand(true);
        let input = Url::parse("https://kemono.cr/posts").unwrap();
        let mut events = client.events(&input);
        let event = events.next().await.unwrap().unwrap();
        let ExtractionEvent::Post(post) = event else {
            panic!("expected boundary")
        };
        assert_eq!(
            post.metadata.as_ref().unwrap().caption.as_deref(),
            Some("expanded")
        );
        let first = events.next().await.unwrap().unwrap();
        let ExtractionEvent::Media(first) = first else {
            panic!("expected first file")
        };
        assert_eq!(first.filename, "7_01.jpg");
        assert_eq!(
            first.archive_key(),
            parse_post_with_selection(
                &detail,
                "kemono.cr",
                "patreon",
                "42",
                &Selection {
                    original: true,
                    ..Selection::default()
                }
            )[0]
            .archive_key()
        );
        drop(events);
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].starts_with("GET /api/v1/patreon/user/42/post/7 "));
    }

    #[tokio::test]
    async fn global_expansion_fetches_each_author_in_order_and_honors_raw_cutoff() {
        let mut second = post("8");
        second["service"] = json!("fanbox");
        second["user"] = json!("alice");
        let listing = json!({"posts": [
            {"id": "7", "service": "patreon", "user": "42"},
            {"id": "8", "service": "fanbox", "user": "alice"},
            {"id": "invalid", "service": null}
        ]});
        let (root, server) = serve(vec![
            response("200 OK", listing.to_string().as_bytes()),
            response("200 OK", post("7").to_string().as_bytes()),
            response("200 OK", json!({"post": second}).to_string().as_bytes()),
        ]);
        let items: Vec<_> = kemono(root)
            .with_expand(true)
            .with_max_posts(Some(2))
            .stream(&Url::parse("https://kemono.cr/posts").unwrap())
            .collect()
            .await;
        assert_eq!(items.len(), 4);
        let ids: Vec<_> = items
            .iter()
            .map(|item| {
                item.as_ref()
                    .unwrap()
                    .metadata
                    .as_ref()
                    .unwrap()
                    .post_id
                    .as_str()
            })
            .collect();
        assert_eq!(ids, ["7", "7", "8", "8"]);
        assert_eq!(
            items[2]
                .as_ref()
                .unwrap()
                .metadata
                .as_ref()
                .unwrap()
                .creator
                .as_ref()
                .map(|creator| creator.id.as_str()),
            Some("alice")
        );
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[1].starts_with("GET /api/v1/patreon/user/42/post/7 "));
        assert!(requests[2].starts_with("GET /api/v1/fanbox/user/alice/post/8 "));
    }

    #[tokio::test]
    async fn max_posts_counts_empty_posts_and_prevents_later_details_and_pages() {
        let listing: Vec<_> = (1..=50)
            .map(|index| json!({"id": index, "service": "patreon", "user": "42"}))
            .collect();
        let empty = json!({"id": "1", "title": "empty", "published": "2024-01-02T03:04:05"});
        let (root, server) = serve(vec![
            response("200 OK", json!(listing).to_string().as_bytes()),
            response("200 OK", empty.to_string().as_bytes()),
        ]);
        let client = kemono(root).with_expand(true).with_max_posts(Some(1));
        let events: Vec<_> = client
            .events(&Url::parse("https://coomer.st/patreon/user/42").unwrap())
            .collect()
            .await;
        assert_eq!(events.len(), 2);
        let ExtractionEvent::Post(post) = events[0].as_ref().unwrap() else {
            panic!("expected empty boundary")
        };
        assert_eq!(post.category, "coomer");
        assert_eq!(post.count, Some(0));
        let metadata = post.metadata.as_ref().unwrap();
        assert_eq!(metadata.post_id, "1");
        assert_eq!(
            metadata.creator.as_ref().map(|creator| creator.id.as_str()),
            Some("42")
        );
        assert_eq!(metadata.instance.as_deref(), Some("patreon"));
        assert_eq!(metadata.caption.as_deref(), Some("empty"));
        assert_eq!(metadata.timestamp, Some(1704164645));
        assert!(matches!(events[1], Ok(ExtractionEvent::EndPost)));
        assert_eq!(server.join().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn empty_single_posts_have_metadata_boundaries_and_zero_limit_is_unlimited() {
        let (root, server) = serve(vec![response("200 OK", br#"{"id":"7","title":"empty"}"#)]);
        let events: Vec<_> = kemono(root)
            .events(&Url::parse("https://coomer.st/onlyfans/user/alice/post/7").unwrap())
            .collect()
            .await;
        assert_eq!(events.len(), 2);
        let ExtractionEvent::Post(empty_post) = events[0].as_ref().unwrap() else {
            panic!("expected post")
        };
        assert_eq!(empty_post.category, "coomer");
        assert_eq!(
            empty_post
                .metadata
                .as_ref()
                .unwrap()
                .creator
                .as_ref()
                .map(|creator| creator.id.as_str()),
            Some("alice")
        );
        server.join().unwrap();
        let (root, server) = serve(vec![response(
            "200 OK",
            json!([{ "id": "1" }, post("2")]).to_string().as_bytes(),
        )]);
        let events: Vec<_> = kemono(root)
            .with_max_posts(Some(1))
            .with_max_posts(Some(0))
            .events(&Url::parse("https://kemono.cr/patreon/user/42").unwrap())
            .collect()
            .await;
        assert_eq!(events.len(), 6);
        assert!(events.iter().all(Result::is_ok));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn expansion_rejects_changed_owner_service_or_post_before_files() {
        for (field, value) in [
            ("service", "private-service"),
            ("user", "private-user"),
            ("id", "private-post"),
        ] {
            let mut detail = post("7");
            detail[field] = json!(value);
            let (root, server) = serve(vec![
                response("200 OK", json!([post("7")]).to_string().as_bytes()),
                response("200 OK", detail.to_string().as_bytes()),
            ]);
            let events: Vec<_> = kemono(root)
                .with_expand(true)
                .events(&Url::parse("https://kemono.cr/patreon/user/42").unwrap())
                .collect()
                .await;
            assert_eq!(events.len(), 1);
            let error = events[0].as_ref().unwrap_err().to_string();
            assert!(error.contains("identity"));
            assert!(!error.contains("private"));
            server.join().unwrap();
        }
    }

    #[tokio::test]
    async fn repeated_pages_terminate_with_private_diagnostic() {
        let page: Vec<_> = (1..=50).map(|index| post(&index.to_string())).collect();
        let body = json!(page).to_string();
        // The server may repeat the same posts with different field ordering,
        // formatting and refreshed non-identity metadata.
        let repeated: Vec<_> = (1..=50).map(|index| json!({"title":"refreshed", "service":"patreon", "id":index.to_string(), "user":"42"})).collect();
        let repeated = serde_json::to_vec_pretty(&repeated).unwrap();
        let (root, server) = serve(vec![
            response("200 OK", body.as_bytes()),
            response("200 OK", repeated.as_slice()),
        ]);
        let items: Vec<_> = kemono(root)
            .stream(&Url::parse("https://kemono.cr/patreon/user/42").unwrap())
            .collect()
            .await;
        assert_eq!(items.len(), 101);
        assert!(
            items
                .last()
                .unwrap()
                .as_ref()
                .unwrap_err()
                .to_string()
                .contains("repeated a page")
        );
        assert_eq!(server.join().unwrap().len(), 2);
    }

    fn selection_post() -> Value {
        let hash_c = "c".repeat(64);
        json!({"id": "7", "user": "42", "service": "patreon",
            "file": {"name": "original.webp", "path": format!("/aa/11/{HASH_A}.png")},
            "attachments": [
                {"name": "video.mp4", "path": format!("/bb/22/{HASH_B}.mp4")},
                {"name": "photo.png", "path": format!("/cc/33/{hash_c}.jpg")}
            ],
            "content": format!(r#"<img src="/aa/11/{HASH_A}.png"><img src="https://kemono.cr/inline/extra-image.jpg">"#)
        })
    }

    #[test]
    fn defaults_use_category_file_order_thumbnails_and_source_indices() {
        let post = selection_post();
        for (host, indices, kinds) in [
            (
                "kemono.cr",
                vec![2, 3, 4],
                vec!["attachment", "file", "inline"],
            ),
            (
                "coomer.st",
                vec![1, 3, 4],
                vec!["file", "attachment", "inline"],
            ),
        ] {
            let (media, count) = selected_post(&post, host, "patreon", "42", &Selection::default());
            assert_eq!(count, 4);
            assert_eq!(media.len(), 3);
            for ((media, index), kind) in media.iter().zip(indices).zip(kinds) {
                assert_eq!(media.metadata.as_ref().unwrap().index, index);
                assert_eq!(media.filename, format!("7_{index:02}.jpg"));
                assert!(
                    media
                        .url
                        .as_str()
                        .starts_with(&format!("https://img.{host}/thumbnail/data/"))
                );
                assert_eq!(
                    media
                        .metadata
                        .as_ref()
                        .unwrap()
                        .file
                        .as_ref()
                        .unwrap()
                        .media_type
                        .as_deref(),
                    Some(kind)
                );
                assert_eq!(
                    media.headers.get("Accept-Encoding").map(String::as_str),
                    Some("identity")
                );
                assert_eq!(
                    media.headers.get("Referer").map(String::as_str),
                    Some(format!("https://{host}/patreon/user/42/post/7").as_str())
                );
                let public = serde_json::to_string(media).unwrap();
                assert!(!public.contains("Referer"));
                assert!(!public.contains("Accept-Encoding"));
            }
        }
        let normalized = parse_post(&post, "www.kemono.cr", "patreon", "42");
        assert!(
            normalized[0]
                .url
                .as_str()
                .starts_with("https://img.kemono.cr/")
        );
    }

    #[test]
    fn explicit_file_order_and_original_names_preserve_selection_positions() {
        let selection = Selection {
            original: true,
            files: Some(vec![
                FileKind::Inline,
                FileKind::File,
                FileKind::Attachments,
            ]),
            ..Selection::default()
        };
        let (media, count) =
            selected_post(&selection_post(), "kemono.cr", "patreon", "42", &selection);
        assert_eq!(count, 4);
        assert_eq!(
            media
                .iter()
                .map(|media| media.filename.as_str())
                .collect::<Vec<_>>(),
            ["7_01.png", "7_02.jpg", "7_03.mp4", "7_04.png"]
        );
        assert_eq!(
            media
                .iter()
                .map(|media| media
                    .metadata
                    .as_ref()
                    .unwrap()
                    .file
                    .as_ref()
                    .unwrap()
                    .media_type
                    .as_deref()
                    .unwrap())
                .collect::<Vec<_>>(),
            ["inline", "inline", "attachment", "attachment"]
        );
        assert!(
            media
                .iter()
                .all(|media| media.url.as_str().starts_with("https://kemono.cr/data/"))
        );
        assert_eq!(media[1].metadata.as_ref().unwrap().media_id, "");

        let mut post = selection_post();
        post["file"]["path"] = json!(format!(r"\aa\11\{HASH_A}.png"));
        let selection = Selection {
            files: Some(vec![FileKind::File]),
            original: true,
            ..Selection::default()
        };
        let media = parse_post_with_selection(&post, "kemono.cr", "patreon", "42", &selection);
        assert_eq!(media[0].filename, "7_01.webp");
        assert_eq!(
            media[0].url.as_str(),
            format!("https://kemono.cr/data/aa/11/{HASH_A}.png")
        );
        assert_eq!(media[0].metadata.as_ref().unwrap().media_id, HASH_A);
        assert!(
            parse_post_with_selection(
                &post,
                "kemono.cr",
                "patreon",
                "42",
                &Selection {
                    files: Some(Vec::new()),
                    original: true,
                    ..Selection::default()
                }
            )
            .is_empty()
        );
    }

    #[test]
    fn original_name_extension_uses_basename_and_dotfiles_fall_back_to_path() {
        let selection = Selection {
            original: true,
            files: Some(vec![FileKind::File]),
            ..Selection::default()
        };
        for (name, extension) in [
            (".png", "zip"),
            ("folder.jpg/.png", "zip"),
            (r"folder.jpg\.png", "zip"),
            ("folder.jpg/no_extension", "zip"),
            (r"folder.jpg\no_extension", "zip"),
            ("folder.jpg/photo.webp", "webp"),
            (r"folder.jpg\photo.gif", "gif"),
        ] {
            let post = json!({"id": "7", "user": "42", "service": "patreon",
                "file": {"name": name, "path": format!("/aa/11/{HASH_A}.zip")}});
            let media = parse_post_with_selection(&post, "kemono.cr", "patreon", "42", &selection);
            assert_eq!(media.len(), 1);
            assert_eq!(media[0].filename, format!("7_01.{extension}"), "{name}");
            assert_eq!(
                media[0]
                    .metadata
                    .as_ref()
                    .unwrap()
                    .file
                    .as_ref()
                    .unwrap()
                    .description
                    .as_deref(),
                Some(name)
            );
        }
    }

    #[test]
    fn repeated_generators_share_descriptive_names_and_reject_oversized_paths() {
        let name = format!("{}.webp", "descriptive-name-".repeat(128));
        let post = json!({"id": "7", "user": "42", "service": "patreon",
            "file": {"name": name, "path": format!("/aa/11/{HASH_A}.png")}});
        let selection = Selection {
            original: true,
            files: Some(vec![FileKind::File; 64]),
            duplicates: DuplicateOptions::all(),
        };
        let media = parse_post_with_selection(&post, "kemono.cr", "patreon", "42", &selection);
        assert_eq!(media.len(), 64);
        let first = media[0]
            .metadata
            .as_ref()
            .unwrap()
            .file
            .as_ref()
            .unwrap()
            .description
            .as_ref()
            .unwrap();
        assert_eq!(first.as_ref(), name);
        for (index, media) in media.iter().enumerate() {
            assert_eq!(media.filename, format!("7_{:02}.webp", index + 1));
            let description = media
                .metadata
                .as_ref()
                .unwrap()
                .file
                .as_ref()
                .unwrap()
                .description
                .as_ref()
                .unwrap();
            assert!(Arc::ptr_eq(first, description));
        }
        let huge_path = format!("/inline/{}.jpg", "s".repeat(4096));
        let rejected = json!({"id": "7", "user": "42", "service": "patreon",
            "file": {"name": name, "path": huge_path},
            "attachments": [{"name": "attachment.jpg", "path": huge_path}],
            "content": format!(r#"<img src="{huge_path}">"#)});
        let selection = Selection {
            original: true,
            files: Some([FileKind::File, FileKind::Attachments, FileKind::Inline].repeat(64)),
            duplicates: DuplicateOptions::all(),
        };
        assert!(selected_files(&rejected, "kemono", &selection).is_empty());
        assert!(
            parse_post_with_selection(&rejected, "kemono.cr", "patreon", "42", &selection)
                .is_empty()
        );

        let inline = json!({"id": "7", "user": "42", "service": "patreon", "content": r#"<img src="/inline/shared-name.jpg">"#});
        let selection = Selection {
            original: true,
            files: Some(vec![FileKind::Inline; 64]),
            duplicates: DuplicateOptions::all(),
        };
        let media = parse_post_with_selection(&inline, "kemono.cr", "patreon", "42", &selection);
        assert_eq!(media.len(), 64);
        let first = media[0]
            .metadata
            .as_ref()
            .unwrap()
            .file
            .as_ref()
            .unwrap()
            .description
            .as_ref()
            .unwrap();
        assert!(media.iter().all(|media| {
            Arc::ptr_eq(
                first,
                media
                    .metadata
                    .as_ref()
                    .unwrap()
                    .file
                    .as_ref()
                    .unwrap()
                    .description
                    .as_ref()
                    .unwrap(),
            )
        }));
    }

    #[test]
    fn duplicates_are_hash_based_and_enabled_per_incoming_kind() {
        let post = json!({"id": "7", "user": "42", "service": "patreon",
            "file": {"path": format!("/aa/11/{HASH_A}.png")},
            "attachments": [{"path": format!("/bb/22/{HASH_A}.jpg")}, {"path": format!("/cc/33/{HASH_A}.webp")}],
            "content": format!(r#"<img src="/dd/44/{HASH_A}.png"><img src="/ee/55/{HASH_A}.jpg">"#)
        });
        for (duplicates, expected_kinds) in [
            (DuplicateOptions::default(), vec!["attachment"]),
            (
                DuplicateOptions {
                    file: true,
                    ..DuplicateOptions::default()
                },
                vec!["attachment", "file"],
            ),
            (
                DuplicateOptions {
                    attachment: true,
                    ..DuplicateOptions::default()
                },
                vec!["attachment", "attachment"],
            ),
            (
                DuplicateOptions {
                    inline: true,
                    ..DuplicateOptions::default()
                },
                vec!["attachment", "inline", "inline"],
            ),
            (
                DuplicateOptions::all(),
                vec!["attachment", "attachment", "file", "inline", "inline"],
            ),
        ] {
            let media = parse_post_with_selection(
                &post,
                "kemono.cr",
                "patreon",
                "42",
                &Selection {
                    original: true,
                    duplicates,
                    files: None,
                },
            );
            assert_eq!(
                media
                    .iter()
                    .map(|media| media
                        .metadata
                        .as_ref()
                        .unwrap()
                        .file
                        .as_ref()
                        .unwrap()
                        .media_type
                        .as_deref()
                        .unwrap())
                    .collect::<Vec<_>>(),
                expected_kinds
            );
            for (index, media) in media.iter().enumerate() {
                assert_eq!(media.metadata.as_ref().unwrap().index, index + 1);
            }
        }
        let media = parse_post_with_selection(
            &post,
            "kemono.cr",
            "patreon",
            "42",
            &Selection {
                original: true,
                files: Some(vec![
                    FileKind::File,
                    FileKind::Attachments,
                    FileKind::Inline,
                ]),
                ..Selection::default()
            },
        );
        assert_eq!(media.len(), 1);
        assert_eq!(
            media[0]
                .metadata
                .as_ref()
                .unwrap()
                .file
                .as_ref()
                .unwrap()
                .media_type
                .as_deref(),
            Some("file")
        );
    }

    #[test]
    fn inline_paths_accept_fixed_firstparty_sources_and_reject_unsafe_values() {
        let path = format!("/aa/11/{HASH_A}.png");
        let post = json!({"id": "7", "user": "42", "service": "patreon", "content": format!(r#"
            <img src="{path}"><img src="http://kemono.cr{path}"><img src="https://coomer.st/inline/another-image.jpg">
            <img src="/inline/nohash.jpg"><img src="/inline/nohash.jpg">
            <img src="https://evil.example{path}"><img src="https://kemono.cr.evil.example{path}">
            <img src="https://kemono.su{path}"><img src="//kemono.cr/inline/foo.jpg">
            <img src="https://user:secret@kemono.cr{path}"><img src="https://kemono.cr:443/inline/port-image.jpg">
            <img src="/inline/../escape.jpg"><img src="https://kemono.cr/inline/foo/../../inline/escape.jpg">
            <img src="/inline/%2e%2e/escape.jpg"><img src="https://kemono.cr/inline/file.jpg?token=secret">
            <img src="https://kemono.cr/inline/file.jpg#secret"><img src="/logo.png">
            <img src='{path}'><img src="/aa/11/short.png">
        "#)});
        let media = parse_post_with_selection(
            &post,
            "kemono.cr",
            "patreon",
            "42",
            &Selection {
                original: true,
                files: Some(vec![FileKind::Inline]),
                ..Selection::default()
            },
        );
        assert_eq!(media.len(), 4, "{media:?}");
        assert_eq!(media[0].metadata.as_ref().unwrap().media_id, HASH_A);
        assert!(
            media[1..]
                .iter()
                .all(|media| media.metadata.as_ref().unwrap().media_id.is_empty())
        );
        assert!(
            media
                .iter()
                .all(|media| !media.url.as_str().contains("secret"))
        );
    }

    #[test]
    fn thumbnail_eligibility_uses_path_extension_and_archives_keep_source_type() {
        let post = json!({"id": "7", "user": "42", "service": "patreon",
        "attachments": [
            {"name": "video.jpg", "path": format!("/aa/11/{HASH_A}.mp4")},
            {"name": "saved.zip", "path": format!("/bb/22/{HASH_B}.bin")},
            {"name": "photo.jpg", "path": format!("/cc/33/{}.png", "c".repeat(64))}
        ]});
        let (media, count) =
            selected_post(&post, "kemono.cr", "patreon", "42", &Selection::default());
        assert_eq!(count, 3);
        assert_eq!(media.len(), 1);
        assert_eq!(media[0].filename, "7_03.jpg");
        let original = parse_post_with_selection(
            &post,
            "kemono.cr",
            "patreon",
            "42",
            &Selection {
                original: true,
                ..Selection::default()
            },
        );
        assert_eq!(original.len(), 3);
        assert_eq!(original[0].filename, "7_01.jpg");
        assert_eq!(original[1].filename, "7_02.zip");
        assert_eq!(
            original[1]
                .metadata
                .as_ref()
                .unwrap()
                .file
                .as_ref()
                .unwrap()
                .media_type
                .as_deref(),
            Some("archive")
        );
    }

    #[tokio::test]
    async fn thumbnail_post_boundaries_count_all_selected_files_before_skipping_nonimages() {
        let (root, server) = serve(vec![response(
            "200 OK",
            json!({"post": selection_post()}).to_string().as_bytes(),
        )]);
        let client = kemono(root).with_selection(Selection::default());
        let events: Vec<_> = client
            .events(&Url::parse("https://kemono.cr/patreon/user/42/post/7").unwrap())
            .collect()
            .await;
        assert_eq!(events.len(), 5);
        let ExtractionEvent::Post(post) = events[0].as_ref().unwrap() else {
            panic!("expected post")
        };
        assert_eq!(post.count, Some(4));
        let indices: Vec<_> = events
            .iter()
            .filter_map(|event| match event.as_ref().unwrap() {
                ExtractionEvent::Media(media) => Some(media.metadata.as_ref().unwrap().index),
                _ => None,
            })
            .collect();
        assert_eq!(indices, [2, 3, 4]);
        server.join().unwrap();

        let post = json!({"id": "7", "user": "42", "service": "patreon", "file": {"path": format!("/aa/11/{HASH_A}.mp4")}});
        let (root, server) = serve(vec![response("200 OK", post.to_string().as_bytes())]);
        let client = kemono(root).with_selection(Selection::default());
        let events: Vec<_> = client
            .events(&Url::parse("https://coomer.st/patreon/user/42/post/7").unwrap())
            .collect()
            .await;
        assert_eq!(events.len(), 2);
        let ExtractionEvent::Post(post) = events[0].as_ref().unwrap() else {
            panic!("expected post")
        };
        assert_eq!(post.count, Some(1));
        assert!(matches!(events[1], Ok(ExtractionEvent::EndPost)));
        server.join().unwrap();
    }

    fn revision(id: u64, title: &str) -> Value {
        let mut post = post("7");
        post["revision_id"] = json!(id);
        post["title"] = json!(title);
        post
    }

    fn post_revisions(events: &[Result<ExtractionEvent>]) -> Vec<RevisionMetadata> {
        events
            .iter()
            .filter_map(|event| match event.as_ref().unwrap() {
                ExtractionEvent::Post(post) => post.metadata.as_ref().unwrap().revision.clone(),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn embedded_revisions_preserve_outer_props_and_normalize_encoded_files() {
        let mut old = revision(2, "historical");
        old["file"] = json!(old["file"].to_string());
        old["attachments"] = Value::Array(
            old["attachments"]
                .as_array()
                .unwrap()
                .iter()
                .map(|file| json!(file.to_string()))
                .collect(),
        );
        let body = json!({"post": post("7"), "props": {"revisions": [["ignored",old],[0,{"id":"skipped"}]]}});
        let (root, server) = serve(vec![response("200 OK", body.to_string().as_bytes())]);
        let events: Vec<_> = kemono(root)
            .with_revisions(RevisionMode::All)
            .events(&Url::parse("https://kemono.cr/patreon/user/42/post/7").unwrap())
            .collect()
            .await;
        assert_eq!(events.len(), 8, "{events:?}");
        let revisions = post_revisions(&events);
        assert_eq!(
            revisions
                .iter()
                .map(|revision| (revision.id, revision.index, revision.count))
                .collect::<Vec<_>>(),
            [(Some(0), Some(2), Some(2)), (Some(2), Some(1), Some(2))]
        );
        assert_eq!(
            revisions[0].hash.as_ref().unwrap(),
            &revisions::hash(&post("7")).unwrap()
        );
        let media: Vec<_> = events
            .iter()
            .filter_map(|event| match event.as_ref().unwrap() {
                ExtractionEvent::Media(media) => Some(media),
                _ => None,
            })
            .collect();
        assert_eq!(
            media
                .iter()
                .map(|media| media.filename.as_str())
                .collect::<Vec<_>>(),
            ["7_01.jpg", "7_02.mp4", "7_01_r2.jpg", "7_02_r2.mp4"]
        );
        assert_eq!(
            media[0].archive_key(),
            parse_post(&post("7"), "kemono.cr", "patreon", "42")[0].archive_key()
        );
        assert_ne!(media[0].archive_key(), media[2].archive_key());
        assert_eq!(
            media[2]
                .metadata
                .as_ref()
                .unwrap()
                .revision
                .as_ref()
                .unwrap()
                .id,
            Some(2)
        );
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn revision_endpoint_fallback_assigns_indices_before_oldest_order() {
        let (root, server) = serve(vec![
            response("200 OK", post("7").to_string().as_bytes()),
            response(
                "200 OK",
                json!([revision(2, "older"), revision(1, "oldest")])
                    .to_string()
                    .as_bytes(),
            ),
        ]);
        let events: Vec<_> = kemono(root)
            .with_revisions(RevisionMode::All)
            .with_revision_order(RevisionOrder::Oldest)
            .events(&Url::parse("https://coomer.st/patreon/user/42/post/7").unwrap())
            .collect()
            .await;
        let revisions = post_revisions(&events);
        assert_eq!(
            revisions
                .iter()
                .map(|revision| (revision.id, revision.index, revision.count))
                .collect::<Vec<_>>(),
            [
                (Some(1), Some(1), Some(3)),
                (Some(2), Some(2), Some(3)),
                (Some(0), Some(3), Some(3))
            ]
        );
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].starts_with("GET /api/v1/patreon/user/42/post/7/revisions "));
    }

    #[tokio::test]
    async fn explicit_revision_routes_ignore_mode_and_select_historical_ids() {
        for (suffix, mode, expected) in [
            ("revision", RevisionMode::None, vec![2, 1]),
            ("revisions", RevisionMode::Unique, vec![2, 1]),
            ("revision/2", RevisionMode::Unique, vec![2]),
        ] {
            let body = json!({"post":post("7"),"props":{"revisions":[[0,revision(2,"Hello")],[0,revision(1,"Hello")]]}});
            let (root, server) = serve(vec![response("200 OK", body.to_string().as_bytes())]);
            let events: Vec<_> = kemono(root)
                .with_revisions(mode)
                .events(
                    &Url::parse(&format!(
                        "https://kemono.cr/patreon/user/42/post/7/{suffix}"
                    ))
                    .unwrap(),
                )
                .collect()
                .await;
            let revisions = post_revisions(&events);
            assert_eq!(
                revisions
                    .iter()
                    .map(|revision| revision.id.unwrap())
                    .collect::<Vec<_>>(),
                expected
            );
            assert!(revisions.iter().all(|revision| revision.count == Some(2)));
            assert_eq!(revisions[0].index, Some(2));
            assert_eq!(server.join().unwrap().len(), 1);
        }
        for suffix in ["revisions/99", "revision/0"] {
            let body = json!({"post":post("7"),"props":{"revisions":[[0,revision(1,"old")]]}});
            let (root, server) = serve(vec![response("200 OK", body.to_string().as_bytes())]);
            let events: Vec<_> = kemono(root)
                .events(
                    &Url::parse(&format!(
                        "https://kemono.cr/patreon/user/42/post/7/{suffix}"
                    ))
                    .unwrap(),
                )
                .collect()
                .await;
            assert_eq!(events.len(), 1);
            assert_eq!(
                events[0].as_ref().unwrap_err().to_string(),
                "Kemono revision not found"
            );
            server.join().unwrap();
        }
        for suffix in [
            "revision/secret",
            "revision/-1",
            "revisions/9999999999999999999999999",
        ] {
            let error = target(
                &Url::parse(&format!(
                    "https://kemono.cr/patreon/user/42/post/7/{suffix}"
                ))
                .unwrap(),
            )
            .unwrap_err()
            .to_string();
            assert!(!error.contains("secret"));
        }
    }

    #[tokio::test]
    async fn expansion_keeps_embedded_revisions_and_cutoff_drains_empty_history() {
        let listing: Vec<_> = (1..=50)
            .map(|index| {
                if index == 1 {
                    post("7")
                } else {
                    json!({"id":"later-invalid"})
                }
            })
            .collect();
        let empty = json!({"id":"7","revision_id":1,"title":"empty historical","file":"{}","attachments":[]});
        let detail = json!({"post":post("7"),"props":{"revisions":[[0,empty]]}});
        let (root, server) = serve(vec![
            response("200 OK", json!({"posts":listing}).to_string().as_bytes()),
            response("200 OK", detail.to_string().as_bytes()),
        ]);
        let events: Vec<_> = kemono(root)
            .with_expand(true)
            .with_revisions(RevisionMode::All)
            .with_max_posts(Some(1))
            .events(&Url::parse("https://kemono.cr/posts").unwrap())
            .collect()
            .await;
        assert_eq!(events.len(), 6, "{events:?}");
        let revisions = post_revisions(&events);
        assert_eq!(
            revisions
                .iter()
                .map(|revision| revision.id)
                .collect::<Vec<_>>(),
            [Some(0), Some(1)]
        );
        let ExtractionEvent::Post(empty) = events[4].as_ref().unwrap() else {
            panic!("expected historical boundary")
        };
        assert_eq!(empty.count, Some(0));
        assert_eq!(
            empty
                .metadata
                .as_ref()
                .unwrap()
                .creator
                .as_ref()
                .map(|creator| creator.id.as_str()),
            Some("42")
        );
        assert_eq!(
            empty
                .metadata
                .as_ref()
                .unwrap()
                .revision
                .as_ref()
                .unwrap()
                .index,
            Some(1)
        );
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].starts_with("GET /api/v1/patreon/user/42/post/7 "));
    }

    #[tokio::test]
    async fn dropping_revision_events_avoids_later_parent_expansion_and_history_requests() {
        let (root, server) = serve(vec![
            response(
                "200 OK",
                json!([post("7"), post("8")]).to_string().as_bytes(),
            ),
            response("200 OK", post("7").to_string().as_bytes()),
            response("200 OK", json!([revision(1, "old")]).to_string().as_bytes()),
        ]);
        let client = kemono(root)
            .with_expand(true)
            .with_revisions(RevisionMode::All);
        let input = Url::parse("https://kemono.cr/patreon/user/42").unwrap();
        let mut events = client.events(&input);
        assert!(matches!(
            events.next().await.unwrap().unwrap(),
            ExtractionEvent::Post(_)
        ));
        drop(events);
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[2].starts_with("GET /api/v1/patreon/user/42/post/7/revisions "));
    }

    #[tokio::test]
    async fn invalid_historical_identity_fails_before_any_current_or_historical_output() {
        for field in ["id", "user", "service"] {
            let mut old = revision(1, "old");
            old[field] = json!("private-invalid");
            let body = json!({"post":post("7"),"props":{"revisions":[[0,old]]}});
            let (root, server) = serve(vec![response("200 OK", body.to_string().as_bytes())]);
            let events: Vec<_> = kemono(root)
                .with_revisions(RevisionMode::All)
                .events(&Url::parse("https://kemono.cr/patreon/user/42/post/7").unwrap())
                .collect()
                .await;
            assert_eq!(events.len(), 1);
            let error = events[0].as_ref().unwrap_err().to_string();
            assert!(!error.contains("private"));
            assert!(error.contains("identity"));
            server.join().unwrap();
        }
    }

    #[tokio::test]
    async fn ordinary_selection_ignores_malformed_disabled_file_groups() {
        for files in [vec![], vec![FileKind::Inline]] {
            let body = json!({"id":"7","file":false,"attachments":{"malformed":"optional"},"content":r#"<img src="/inline/only-inline.jpg">"#});
            let (root, server) = serve(vec![response("200 OK", body.to_string().as_bytes())]);
            let expected = usize::from(!files.is_empty());
            let selection = Selection {
                files: Some(files),
                original: true,
                ..Selection::default()
            };
            let events: Vec<_> = kemono(root)
                .with_selection(selection)
                .events(&Url::parse("https://kemono.cr/patreon/user/42/post/7").unwrap())
                .collect()
                .await;
            assert_eq!(events.len(), expected + 2, "{events:?}");
            let ExtractionEvent::Post(post) = events[0].as_ref().unwrap() else {
                panic!("expected post")
            };
            assert_eq!(post.count, Some(expected));
            assert!(post.metadata.as_ref().unwrap().revision.is_none());
            assert_eq!(server.join().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn no_history_and_empty_posts_keep_revision_metadata_without_extra_requests() {
        for explicit in [false, true] {
            let body = json!({"post":{"id":"7","title":"empty","file":{},"attachments":[]},"props":{"revisions":[]}});
            let (root, server) = serve(vec![response("200 OK", body.to_string().as_bytes())]);
            let suffix = if explicit { "/revisions" } else { "" };
            let events: Vec<_> = kemono(root)
                .with_revisions(RevisionMode::All)
                .events(
                    &Url::parse(&format!("https://coomer.st/patreon/user/42/post/7{suffix}"))
                        .unwrap(),
                )
                .collect()
                .await;
            if explicit {
                assert!(events.is_empty());
            } else {
                assert_eq!(events.len(), 2);
                let revisions = post_revisions(&events);
                assert_eq!(revisions.len(), 1);
                assert_eq!(
                    (revisions[0].id, revisions[0].index, revisions[0].count),
                    (Some(0), Some(1), Some(1))
                );
            }
            assert_eq!(server.join().unwrap().len(), 1);
        }
    }

    fn creator_profile(service: &str, user: &str, name: &str) -> Value {
        json!({"id":user,"service":service,"name":name,"public_id":"public-handle",
            "indexed":"2024-01-02T03:04:05","updated":"2024-02-02T00:00:00","post_count":8})
    }

    #[tokio::test]
    async fn profile_cache_reuses_lookup_across_pages_posts_and_inputs() {
        let full: Vec<_> = (1..=50).map(|id| post(&id.to_string())).collect();
        let (root, server) = serve(vec![
            response("200 OK", json!(full).to_string().as_bytes()),
            response(
                "200 OK",
                creator_profile("patreon", "42", "Display Name")
                    .to_string()
                    .as_bytes(),
            ),
            response("200 OK", b"[]"),
            response("200 OK", post("7").to_string().as_bytes()),
        ]);
        let client = kemono(root).with_metadata(true);
        let media: Vec<_> = client
            .stream(&Url::parse("https://kemono.cr/patreon/user/42").unwrap())
            .collect()
            .await;
        assert_eq!(media.len(), 100);
        for media in &media {
            let metadata = media.as_ref().unwrap().metadata.as_ref().unwrap();
            assert_eq!(metadata.username.as_deref(), Some("Display Name"));
            assert_eq!(metadata.creator.as_ref().unwrap().id, "42");
            assert_eq!(metadata.creator.as_ref().unwrap().indexed, Some(1704164645));
        }
        let again: Vec<_> = client
            .stream(&Url::parse("https://kemono.cr/patreon/user/42/post/7").unwrap())
            .collect()
            .await;
        assert_eq!(again.len(), 2);
        assert_eq!(
            again[0]
                .as_ref()
                .unwrap()
                .metadata
                .as_ref()
                .unwrap()
                .username
                .as_deref(),
            Some("Display Name")
        );
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 4);
        assert!(requests[1].starts_with("GET /api/v1/patreon/user/42/profile "));
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.contains("/profile "))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn profile_enrichment_is_shared_across_revisions_without_changing_revision_hashes() {
        let body = json!({"post":post("7"),"props":{"revisions":[[0,revision(1,"old")]]}});
        let (root, server) = serve(vec![
            response("200 OK", body.to_string().as_bytes()),
            response(
                "200 OK",
                creator_profile("patreon", "42", "  Display 猫  ")
                    .to_string()
                    .as_bytes(),
            ),
        ]);
        let events: Vec<_> = kemono(root)
            .with_metadata(true)
            .with_revisions(RevisionMode::All)
            .events(&Url::parse("https://kemono.cr/patreon/user/42/post/7").unwrap())
            .collect()
            .await;
        let revisions = post_revisions(&events);
        assert_eq!(revisions.len(), 2);
        assert_eq!(
            revisions[0].hash.as_ref().unwrap(),
            &revisions::hash(&post("7")).unwrap()
        );
        let mut first = None;
        for event in &events {
            let metadata = match event.as_ref().unwrap() {
                ExtractionEvent::Post(post) => post.metadata.as_ref(),
                ExtractionEvent::Media(media) => media.metadata.as_ref(),
                _ => None,
            };
            if let Some(metadata) = metadata {
                let name = metadata.username.as_ref().unwrap();
                assert_eq!(name.as_ref(), "  Display 猫  ");
                if let Some(first) = &first {
                    assert!(Arc::ptr_eq(first, name));
                } else {
                    first = Some(name.clone());
                }
                assert_eq!(metadata.creator.as_ref().unwrap().id, "42");
            }
        }
        assert_eq!(server.join().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn profile_cache_isolates_host_service_and_creator_identity() {
        let inputs = [
            ("kemono.cr", "patreon", "42", "first"),
            ("kemono.cr", "fanbox", "42", "second"),
            ("coomer.st", "patreon", "42", "third"),
            ("kemono.cr", "patreon", "43", "fourth"),
        ];
        let mut replies = Vec::new();
        for (_, service, user, name) in inputs {
            let mut payload = post("7");
            payload["service"] = json!(service);
            payload["user"] = json!(user);
            replies.push(response("200 OK", payload.to_string().as_bytes()));
            replies.push(response(
                "200 OK",
                creator_profile(service, user, name).to_string().as_bytes(),
            ));
        }
        let (root, server) = serve(replies);
        let client = kemono(root).with_metadata(true);
        for (host, service, user, name) in inputs {
            let media: Vec<_> = client
                .stream(
                    &Url::parse(&format!("https://{host}/{service}/user/{user}/post/7")).unwrap(),
                )
                .collect()
                .await;
            let metadata = media[0].as_ref().unwrap().metadata.as_ref().unwrap();
            assert_eq!(metadata.username.as_deref(), Some(name));
            assert_eq!(metadata.creator.as_ref().unwrap().id, user);
            assert_eq!(metadata.creator.as_ref().unwrap().service, service);
        }
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 8);
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.contains("/profile "))
                .count(),
            4
        );
    }

    #[tokio::test]
    async fn profile_failures_and_invalid_identity_cache_stubs_without_losing_files() {
        for reply in [
            response("404 Not Found", b"{}"),
            response("403 Forbidden", b"{}"),
            response("200 OK", b"private-invalid-json"),
            response(
                "200 OK",
                creator_profile("patreon", "private-other", "wrong display")
                    .to_string()
                    .as_bytes(),
            ),
        ] {
            let (root, server) = serve(vec![
                response(
                    "200 OK",
                    json!([post("7"), post("8")]).to_string().as_bytes(),
                ),
                reply,
            ]);
            let media: Vec<_> = kemono(root)
                .with_metadata(true)
                .stream(&Url::parse("https://kemono.cr/patreon/user/42").unwrap())
                .collect()
                .await;
            assert_eq!(media.len(), 4);
            for media in media {
                let media = media.unwrap();
                let metadata = media.metadata.as_ref().unwrap();
                assert!(metadata.username.is_none());
                assert_eq!(
                    metadata.creator.as_ref().unwrap(),
                    &CreatorMetadata::stub("patreon", "42")
                );
                assert!(!serde_json::to_string(&media).unwrap().contains("private"));
            }
            assert_eq!(server.join().unwrap().len(), 2);
        }
    }

    #[tokio::test]
    async fn profile_lookups_apply_to_empty_and_thumbnail_omitted_posts_but_can_be_disabled() {
        assert!(Kemono::new(Duration::from_secs(5)).unwrap().metadata);
        for nonimage in [false, true] {
            let mut payload = json!({"id":"7","file":{},"attachments":[]});
            if nonimage {
                payload["file"] = json!({"path":format!("/aa/11/{HASH_A}.mp4")});
            }
            let (root, server) = serve(vec![
                response("200 OK", payload.to_string().as_bytes()),
                response(
                    "200 OK",
                    creator_profile("patreon", "42", "Name")
                        .to_string()
                        .as_bytes(),
                ),
            ]);
            let events: Vec<_> = kemono(root)
                .with_metadata(true)
                .with_selection(Selection::default())
                .events(&Url::parse("https://kemono.cr/patreon/user/42/post/7").unwrap())
                .collect()
                .await;
            assert_eq!(events.len(), 2);
            let ExtractionEvent::Post(post) = events[0].as_ref().unwrap() else {
                panic!("expected post")
            };
            assert_eq!(post.count, Some(usize::from(nonimage)));
            assert_eq!(
                post.metadata.as_ref().unwrap().username.as_deref(),
                Some("Name")
            );
            assert_eq!(server.join().unwrap().len(), 2);
        }
        let (root, server) = serve(vec![response("200 OK", post("7").to_string().as_bytes())]);
        let events: Vec<_> = kemono(root)
            .with_metadata(true)
            .with_metadata(false)
            .events(&Url::parse("https://kemono.cr/patreon/user/42/post/7").unwrap())
            .collect()
            .await;
        for event in events {
            let metadata = match event.unwrap() {
                ExtractionEvent::Post(post) => post.metadata,
                ExtractionEvent::Media(media) => media.metadata,
                _ => None,
            };
            if let Some(metadata) = metadata {
                assert!(metadata.username.is_none());
                assert_eq!(
                    metadata.creator,
                    Some(CreatorMetadata::stub("patreon", "42"))
                );
            }
        }
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn raw_cutoff_and_consumer_drop_avoid_later_creator_lookups() {
        for cutoff in [true, false] {
            let mut other = post("8");
            other["user"] = json!("43");
            let (root, server) = serve(vec![
                response(
                    "200 OK",
                    json!({"posts":[post("7"),other]}).to_string().as_bytes(),
                ),
                response(
                    "200 OK",
                    creator_profile("patreon", "42", "first")
                        .to_string()
                        .as_bytes(),
                ),
            ]);
            let client = kemono(root).with_metadata(true).with_max_posts(if cutoff {
                Some(1)
            } else {
                None
            });
            let input = Url::parse("https://kemono.cr/posts").unwrap();
            let mut events = client.events(&input);
            if cutoff {
                assert_eq!(events.collect::<Vec<_>>().await.len(), 4);
            } else {
                assert!(matches!(
                    events.next().await.unwrap().unwrap(),
                    ExtractionEvent::Post(_)
                ));
                drop(events);
            }
            assert_eq!(server.join().unwrap().len(), 2);
        }
    }

    #[tokio::test]
    async fn simultaneous_profile_requests_share_one_completed_lookup() {
        let (root, server) = serve(vec![response(
            "200 OK",
            creator_profile("patreon", "42", "shared")
                .to_string()
                .as_bytes(),
        )]);
        let client = kemono(root).with_metadata(true);
        let identity = Identity {
            service: "patreon".into(),
            user: "42".into(),
            id: "7".into(),
        };
        let (first, second) = futures_util::future::join(
            client.creator("kemono.cr", &identity),
            client.creator("kemono.cr", &identity),
        )
        .await;
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(first.name.as_deref(), Some("shared"));
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn cancelling_profile_lookup_releases_http_and_cache_lock_for_retry() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let root = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let server = thread::spawn(move || {
            let read_request = |stream: &mut std::net::TcpStream| {
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 1024];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    let count = stream.read(&mut buffer).unwrap();
                    assert_ne!(count, 0);
                    request.extend_from_slice(&buffer[..count]);
                }
                String::from_utf8(request).unwrap()
            };
            let (mut first, _) = listener.accept().unwrap();
            let first_request = read_request(&mut first);
            started_tx.send(()).unwrap();
            let mut byte = [0];
            match first.read(&mut byte) {
                Ok(0) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::UnexpectedEof
                    ) => {}
                other => panic!("profile request was not cancelled: {other:?}"),
            };
            let (mut second, _) = listener.accept().unwrap();
            let second_request = read_request(&mut second);
            second
                .write_all(&response(
                    "200 OK",
                    creator_profile("patreon", "42", "retry")
                        .to_string()
                        .as_bytes(),
                ))
                .unwrap();
            vec![first_request, second_request]
        });
        let client = Arc::new(kemono(root).with_metadata(true));
        let lookup = tokio::spawn({
            let client = client.clone();
            async move {
                let identity = Identity {
                    service: "patreon".into(),
                    user: "42".into(),
                    id: "7".into(),
                };
                client.creator("kemono.cr", &identity).await
            }
        });
        tokio::task::spawn_blocking(move || {
            started_rx.recv_timeout(Duration::from_secs(3)).unwrap()
        })
        .await
        .unwrap();
        lookup.abort();
        assert!(lookup.await.unwrap_err().is_cancelled());
        let identity = Identity {
            service: "patreon".into(),
            user: "42".into(),
            id: "7".into(),
        };
        let profile = tokio::time::timeout(
            Duration::from_secs(3),
            client.creator("kemono.cr", &identity),
        )
        .await
        .unwrap();
        assert_eq!(profile.name.as_deref(), Some("retry"));
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            requests
                .iter()
                .all(|request| request.starts_with("GET /api/v1/patreon/user/42/profile "))
        );
    }

    #[tokio::test]
    async fn terminal_errors_are_not_retried() {
        let (root, server) = serve(vec![response("404 Not Found", b"{}")]);
        let error = kemono(root)
            .with_retries(3)
            .stream(&Url::parse("https://kemono.su/fanbox/user/1").unwrap())
            .collect::<Vec<_>>()
            .await
            .remove(0)
            .unwrap_err();
        assert!(error.to_string().contains("not found"));
        assert!(!error.to_string().contains("127.0.0.1"));
        assert_eq!(server.join().unwrap().len(), 1);
    }
}
