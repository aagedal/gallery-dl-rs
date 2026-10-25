//! Experimental native Bluesky post, author-feed, custom-feed, list, search,
//! follows and bookmark extraction.
//!
//! Mirrors gallery_dl/extractor/bluesky.py: posts are read from the public,
//! unauthenticated AppView (or, after a `username`/`password` login, through
//! bsky.social), and files are downloaded as full-size blobs via
//! `com.atproto.sync.getBlob`, with URLs built from validated DIDs and CIDs only.
use crate::{
    Result,
    dispatch::{self, Include},
    error,
    event::{ChildMetadata, ExtractionEvent, KeywordPolicy, Post, child_job, group_with_keywords},
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
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::Arc,
    time::{Duration, SystemTime},
};
use url::Url;

#[path = "bluesky_likes.rs"]
mod likes;
pub use likes::LikesEndpoint;

#[path = "bluesky_session.rs"]
mod session;

const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
const API_ROOT: &str = "https://public.api.bsky.app/xrpc/";
const BLOB_ROOT: &str = "https://bsky.social/xrpc/com.atproto.sync.getBlob";
const MAX_RESPONSE: usize = 16 * 1024 * 1024;
const PAGE_LIMIT: &str = "100";
/// Bound each feed/list to 1,000 pages (100,000 posts).
const MAX_PAGES: usize = 1000;
const MAX_QUOTES: usize = 32;

/// Upstream author-feed and media selection defaults.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub reposts: bool,
    pub videos: bool,
    pub quoted: bool,
}

impl Default for Selection {
    fn default() -> Self {
        Self {
            reposts: false,
            videos: true,
            quoted: false,
        }
    }
}

/// Which author feed or mixed-author collection to read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Feed {
    Media,
    Posts,
    Replies,
    Video,
    ActorLikes,
    Custom(String),
    List(String),
    /// `app.bsky.feed.searchPosts`; hashtags are searches for `#NAME`.
    Search {
        query: String,
        sort: Option<String>,
    },
    /// The logged-in account's `app.bsky.bookmark.getBookmarks`.
    Bookmarks,
}

impl Feed {
    fn filter(&self) -> Option<&'static str> {
        match self {
            Self::Media => Some("posts_with_media"),
            Self::Posts => Some("posts_and_author_threads"),
            Self::Replies => Some("posts_with_replies"),
            Self::Video => Some("posts_with_video"),
            Self::Custom(_)
            | Self::List(_)
            | Self::ActorLikes
            | Self::Search { .. }
            | Self::Bookmarks => None,
        }
    }
}

/// A supported Bluesky input; `actor` is a validated handle or DID.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Post {
        actor: String,
        rkey: String,
    },
    Feed {
        actor: String,
        feed: Feed,
    },
    Profile {
        actor: String,
        view: ProfileView,
    },
    /// Accounts followed by `actor`, each queued as a user dispatcher child.
    Following {
        actor: String,
    },
    /// Post search (`/search?q=`) or hashtag (`/hashtag/NAME[/top|/latest]`).
    Search {
        query: String,
        sort: Option<String>,
    },
    /// The logged-in account's bookmarks (`/saved`).
    Bookmarks,
}

const MAX_QUERY: usize = 4096;

/// Upstream also accepts the bsky.app embed proxies (`fxbsky.app`, `vxbsky.app`,
/// `cbsky.app`, `bskye.app`, `bskyx.app`, ...) and `main.bsky.dev`.
pub fn is_host(host: &str) -> bool {
    if host == "main.bsky.dev" {
        return true;
    }
    let host = host.strip_prefix("www.").unwrap_or(host);
    let Some(name) = host.strip_suffix(".app") else {
        return false;
    };
    let name = ["fx", "vx", "c"]
        .iter()
        .find_map(|prefix| name.strip_prefix(prefix))
        .unwrap_or(name);
    let name = name
        .strip_suffix('e')
        .or_else(|| name.strip_suffix('x'))
        .filter(|base| matches!(*base, "bsky" | "bsyy"))
        .unwrap_or(name);
    matches!(name, "bsky" | "bsyy")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProfileView {
    Info,
    Avatar,
    Background,
    Likes,
}

/// `did:plc:` (base32) or `did:web:` (hostname) identifiers.
pub fn valid_did(did: &str) -> bool {
    if let Some(id) = did.strip_prefix("did:plc:") {
        id.len() == 24 && id.bytes().all(|b| matches!(b, b'a'..=b'z' | b'2'..=b'7'))
    } else if let Some(host) = did.strip_prefix("did:web:") {
        valid_handle(host)
    } else {
        false
    }
}

fn valid_handle(handle: &str) -> bool {
    let labels: Vec<_> = handle.split('.').collect();
    handle.len() <= 253
        && labels.len() >= 2
        && labels.iter().all(|label| {
            (1..=63).contains(&label.len())
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        })
}

fn valid_actor(actor: &str) -> bool {
    valid_did(actor) || valid_handle(actor)
}

/// Record keys are TIDs or other short `[A-Za-z0-9._:~-]` strings.
fn valid_rkey(rkey: &str) -> bool {
    (1..=512).contains(&rkey.len())
        && rkey != "."
        && rkey != ".."
        && rkey
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'~' | b'-'))
}

/// Blob CIDs are CIDv1 in lowercase base32 (`b...`).
pub fn valid_cid(cid: &str) -> bool {
    (8..=128).contains(&cid.len())
        && cid.starts_with('b')
        && cid.bytes().all(|b| matches!(b, b'a'..=b'z' | b'2'..=b'7'))
}

/// Classify a Bluesky URL. Unsupported views are rejected explicitly.
pub fn target(url: &Url) -> Result<Target> {
    let unsupported = || {
        error(
            "unsupported Bluesky URL; expected a post, profile, custom feed (/profile/NAME/feed/ID), list (/profile/NAME/lists/ID), follows, search or hashtag link",
        )
    };
    let host = url.host_str().ok_or_else(unsupported)?;
    if !matches!(url.scheme(), "http" | "https")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || !is_host(host)
    {
        return Err(unsupported());
    }
    let segments: Vec<_> = url
        .path_segments()
        .map(|segments| segments.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    let actor = |name: &str| {
        let name = name.to_ascii_lowercase();
        let name = name.strip_prefix('@').unwrap_or(&name).to_owned();
        valid_actor(&name).then_some(name).ok_or_else(unsupported)
    };
    match segments.as_slice() {
        ["profile", name, "post", rkey] if valid_rkey(rkey) => Ok(Target::Post {
            actor: actor(name)?,
            rkey: (*rkey).into(),
        }),
        ["profile", name] | ["profile", name, "media"] => Ok(Target::Feed {
            actor: actor(name)?,
            feed: Feed::Media,
        }),
        ["profile", name, "posts"] => Ok(Target::Feed {
            actor: actor(name)?,
            feed: Feed::Posts,
        }),
        ["profile", name, "replies"] => Ok(Target::Feed {
            actor: actor(name)?,
            feed: Feed::Replies,
        }),
        ["profile", name, "video"] => Ok(Target::Feed {
            actor: actor(name)?,
            feed: Feed::Video,
        }),
        [
            "profile",
            name,
            view @ ("info" | "avatar" | "banner" | "background" | "likes"),
        ] => Ok(Target::Profile {
            actor: actor(name)?,
            view: match *view {
                "info" => ProfileView::Info,
                "avatar" => ProfileView::Avatar,
                "likes" => ProfileView::Likes,
                _ => ProfileView::Background,
            },
        }),
        ["profile", name, "feed", rkey] if valid_rkey(rkey) => Ok(Target::Feed {
            actor: actor(name)?,
            feed: Feed::Custom((*rkey).into()),
        }),
        ["profile", name, "lists", rkey] if valid_rkey(rkey) => Ok(Target::Feed {
            actor: actor(name)?,
            feed: Feed::List((*rkey).into()),
        }),
        ["profile", name, "follows"] => Ok(Target::Following {
            actor: actor(name)?,
        }),
        ["search"] => {
            let queries: Vec<_> = url
                .query_pairs()
                .filter(|(key, _)| key == "q")
                .map(|(_, value)| value.into_owned())
                .collect();
            match <[String; 1]>::try_from(queries) {
                Ok([query]) => search_target(query, None),
                Err(_) => Err(error("Bluesky search requires one non-empty q parameter")),
            }
        }
        ["search", query] => search_target(decode_segment(query)?, None),
        ["hashtag", tag] | ["hashtag", tag, "top" | "latest"] => {
            let tag = decode_segment(tag)?;
            let sort = segments.get(2).map(|sort| (*sort).to_owned());
            if tag.starts_with('#') || tag.chars().any(char::is_whitespace) {
                return Err(error("invalid Bluesky hashtag"));
            }
            search_target(format!("#{tag}"), sort)
        }
        ["saved"] => Ok(Target::Bookmarks),
        _ => Err(unsupported()),
    }
}

fn decode_segment(segment: &str) -> Result<String> {
    // Path '+' is a space in upstream's `replace("+", " ")` before unquoting.
    let invalid = || error("Bluesky search query is not valid percent-encoded UTF-8");
    let bytes = segment.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                let hex = bytes
                    .get(index + 1..index + 3)
                    .filter(|hex| hex.iter().all(u8::is_ascii_hexdigit))
                    .ok_or_else(invalid)?;
                let hex = std::str::from_utf8(hex).map_err(|_| invalid())?;
                decoded.push(u8::from_str_radix(hex, 16).map_err(|_| invalid())?);
                index += 3;
            }
            b'+' => {
                decoded.push(b' ');
                index += 1;
            }
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(decoded).map_err(|_| invalid())
}

fn search_target(query: String, sort: Option<String>) -> Result<Target> {
    if query.trim().is_empty() || query.len() > MAX_QUERY || query.contains('\0') {
        return Err(error("Bluesky search query is empty or exceeds 4096 bytes"));
    }
    Ok(Target::Search { query, sort })
}

fn string<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn extension(mime: Option<&str>, fallback: &str) -> String {
    let sub = mime
        .and_then(|mime| mime.split_once('/'))
        .map(|(_, sub)| sub.to_ascii_lowercase())
        .filter(|sub| {
            !sub.is_empty() && sub.len() <= 10 && sub.bytes().all(|b| b.is_ascii_alphanumeric())
        });
    match sub.as_deref() {
        Some("jpeg") => "jpg".into(),
        Some(sub) => sub.into(),
        None => fallback.into(),
    }
}

/// Media blobs of a post record's embed: images, a video, or either inside
/// `recordWithMedia`. Quoted records are not followed.
fn blobs(embed: &Value) -> Vec<(&Value, &'static str)> {
    match embed.get("$type").and_then(Value::as_str) {
        Some("app.bsky.embed.images") => embed
            .get("images")
            .and_then(Value::as_array)
            .map(|images| images.iter().map(|image| (image, "image")).collect())
            .unwrap_or_default(),
        Some("app.bsky.embed.video") => vec![(embed, "video")],
        Some("app.bsky.embed.recordWithMedia") => embed.get("media").map(blobs).unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// Parse an `app.bsky.feed.defs#postView` into its files. Posts without
/// media yield an empty list.
pub fn parse_post_view(post: &Value) -> Result<Vec<Media>> {
    parse_post_view_selected(post, Selection::default())
}

pub fn parse_post_view_selected(post: &Value, selection: Selection) -> Result<Vec<Media>> {
    let uri = string(post, "uri").ok_or_else(|| error("Bluesky post is missing its URI"))?;
    let (did, rkey) = uri
        .strip_prefix("at://")
        .and_then(|rest| rest.split_once("/app.bsky.feed.post/"))
        .filter(|(did, rkey)| valid_did(did) && valid_rkey(rkey))
        .ok_or_else(|| error("Bluesky post has an invalid URI"))?;
    if post.pointer("/author/did").and_then(Value::as_str) != Some(did) {
        return Err(error("Bluesky post author does not match its URI"));
    }
    let username: Option<Arc<str>> = post
        .get("author")
        .and_then(|author| string(author, "handle"))
        .map(Arc::from);
    let record = post.get("record").unwrap_or(&Value::Null);
    let caption: Option<Arc<str>> = string(record, "text").map(Arc::from);
    let timestamp = string(record, "createdAt").and_then(|d| crate::date::parse_date(d).ok());
    let embed = record.get("embed").unwrap_or(&Value::Null);
    let mut media = Vec::new();
    for (item, kind) in blobs(embed) {
        if kind == "video" && !selection.videos {
            continue;
        }
        let blob = item.get(kind).unwrap_or(&Value::Null);
        let cid = blob
            .pointer("/ref/$link")
            .and_then(Value::as_str)
            .or_else(|| blob.get("cid").and_then(Value::as_str))
            .filter(|cid| valid_cid(cid))
            .ok_or_else(|| error("Bluesky media has an invalid blob CID"))?;
        let mime = string(blob, "mimeType");
        let ext = extension(mime, if kind == "video" { "mp4" } else { "jpg" });
        let mut url = Url::parse(BLOB_ROOT).unwrap();
        url.query_pairs_mut()
            .append_pair("did", did)
            .append_pair("cid", cid);
        let ratio = |key: &str| {
            item.get("aspectRatio")
                .and_then(|r| r.get(key))
                .and_then(Value::as_u64)
                .filter(|v| *v > 0)
        };
        let index = media.len() + 1;
        media.push(Media {
            url,
            job: MediaJob::DirectHttp,
            category: "bluesky".into(),
            filename: format!("{rkey}_{index}.{ext}"),
            metadata: Some(PostMetadata {
                file: Some(FileMetadata {
                    width: ratio("width"),
                    height: ratio("height"),
                    media_type: mime.map(Arc::from),
                    description: string(item, "alt").map(Arc::from),
                    ..FileMetadata::default()
                }),
                post_id: rkey.into(),
                media_id: cid.into(),
                username: username.clone(),
                caption: caption.clone(),
                timestamp,
                index,
                ..PostMetadata::default()
            }),
            headers: BTreeMap::new(),
            handoff: BTreeMap::new(),
        });
    }
    Ok(media)
}

/// Parse an `app.bsky.feed.getPostThread` response for `did`/`rkey`.
pub fn parse_thread(bytes: &[u8], did: &str, rkey: &str) -> Result<Vec<Media>> {
    let body: Value =
        serde_json::from_slice(bytes).map_err(|_| error("Bluesky returned invalid JSON"))?;
    let thread = body.get("thread").unwrap_or(&Value::Null);
    match thread.get("$type").and_then(Value::as_str) {
        Some("app.bsky.feed.defs#threadViewPost") => {}
        Some("app.bsky.feed.defs#blockedPost") => {
            return Err(error("Bluesky post is blocked"));
        }
        _ => return Err(error("Bluesky post not found or removed")),
    }
    let post = thread.get("post").unwrap_or(&Value::Null);
    if string(post, "uri") != Some(&format!("at://{did}/app.bsky.feed.post/{rkey}")) {
        return Err(error("Bluesky response does not match the requested post"));
    }
    let media = parse_post_view(post)?;
    if media.is_empty() {
        return Err(error("Bluesky post contains no media"));
    }
    Ok(media)
}

/// One parsed `getAuthorFeed` page using default selection: own posts with
/// media and the next cursor. Pinned own posts retain their normal identity.
pub struct Page {
    pub posts: Vec<Vec<Media>>,
    pub cursor: Option<String>,
}

pub fn parse_feed(bytes: &[u8], did: &str) -> Result<Page> {
    let body: Value =
        serde_json::from_slice(bytes).map_err(|_| error("Bluesky returned invalid JSON"))?;
    let items = body
        .get("feed")
        .and_then(Value::as_array)
        .ok_or_else(|| error("Bluesky response is missing its feed"))?;
    let mut posts = Vec::new();
    for post in feed_posts(items, Some(did), Selection::default()) {
        let media = parse_post_view(post)?;
        if !media.is_empty() {
            posts.push(media);
        }
    }
    let cursor = page_cursor(&body)?;
    Ok(Page { posts, cursor })
}

fn page_cursor(body: &Value) -> Result<Option<String>> {
    match body.get("cursor") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(cursor)) if cursor.is_empty() => Ok(None),
        Some(Value::String(cursor)) if cursor.len() <= 1024 => Ok(Some(cursor.clone())),
        Some(Value::String(_)) => Err(error("Bluesky cursor exceeded 1024 bytes")),
        Some(_) => Err(error("Bluesky returned an invalid cursor")),
    }
}

fn feed_posts<'a>(items: &'a [Value], did: Option<&str>, selection: Selection) -> Vec<&'a Value> {
    items
        .iter()
        .filter_map(|item| {
            let post = item.get("post")?;
            // Upstream selects by the underlying author, including pinned own posts.
            if !selection.reposts
                && let Some(did) = did
                && post.pointer("/author/did").and_then(Value::as_str) != Some(did)
            {
                return None;
            }
            Some(post)
        })
        .collect()
}

fn post_identity(uri: &str) -> Result<(&str, &str)> {
    uri.strip_prefix("at://")
        .and_then(|rest| rest.split_once("/app.bsky.feed.post/"))
        .filter(|(did, rkey)| valid_did(did) && valid_rkey(rkey))
        .ok_or_else(|| error("Bluesky quote has an invalid post URI"))
}

fn thread_post(bytes: &[u8], uri: &str) -> Result<Value> {
    thread_posts(bytes, uri).map(|mut posts| posts.swap_remove(0))
}

/// Upper bound for the posts of one thread (`depth` replies included).
const MAX_THREAD_POSTS: usize = 10_000;

/// The requested post followed by its replies in breadth-first order, as
/// upstream's `get_post_thread_uri`. Replies that are not available post
/// views (not found, blocked) carry no record and are skipped.
fn thread_posts(bytes: &[u8], uri: &str) -> Result<Vec<Value>> {
    let mut body: Value =
        serde_json::from_slice(bytes).map_err(|_| error("Bluesky returned invalid JSON"))?;
    let thread = body.get_mut("thread").map(Value::take).unwrap_or_default();
    match thread.get("$type").and_then(Value::as_str) {
        Some("app.bsky.feed.defs#threadViewPost") => {}
        Some("app.bsky.feed.defs#blockedPost") => return Err(error("Bluesky post is blocked")),
        _ => return Err(error("Bluesky post not found or removed")),
    }
    if thread.get("post").and_then(|post| string(post, "uri")) != Some(uri) {
        return Err(error("Bluesky response does not match the requested post"));
    }
    let mut posts = Vec::new();
    let mut queue = VecDeque::from([thread]);
    while let Some(mut node) = queue.pop_front() {
        if posts.len() >= MAX_THREAD_POSTS {
            break;
        }
        if let Some(Value::Array(replies)) = node.get_mut("replies").map(Value::take) {
            queue.extend(replies.into_iter().filter(|reply| {
                reply.get("$type").and_then(Value::as_str)
                    == Some("app.bsky.feed.defs#threadViewPost")
                    && reply.pointer("/post/record").is_some_and(Value::is_object)
            }));
        }
        if let Some(post) = node.get_mut("post").map(Value::take) {
            posts.push(post);
        }
    }
    Ok(posts)
}

/// A quote can be fully hydrated in the AppView embed, or only a strong record
/// reference. Never use a quoted author or URI to construct arbitrary URLs.
type Quote = (String, Option<String>, Option<Value>);

fn quoted_post(post: &Value) -> Result<Option<Quote>> {
    let Some(embed) = post.pointer("/record/embed") else {
        return Ok(None);
    };
    let reference = match embed.get("$type").and_then(Value::as_str) {
        Some("app.bsky.embed.record") => embed.get("record"),
        Some("app.bsky.embed.recordWithMedia") => embed.pointer("/record/record"),
        _ => return Ok(None),
    };
    let Some(reference) = reference else {
        return Ok(None);
    };
    let Some(uri) = string(reference, "uri") else {
        return Err(error("Bluesky quote is missing its post URI"));
    };
    post_identity(uri)?;
    if let Some(cid) = string(reference, "cid")
        && !valid_cid(cid)
    {
        return Err(error("Bluesky quote has an invalid record CID"));
    }
    let view = match post.get("embed").and_then(|v| string(v, "$type")) {
        Some("app.bsky.embed.record#view") => post.pointer("/embed/record"),
        Some("app.bsky.embed.recordWithMedia#view") => post.pointer("/embed/record/record"),
        _ => None,
    };
    let Some(view) = view else {
        return Ok(Some((
            uri.into(),
            string(reference, "cid").map(str::to_owned),
            None,
        )));
    };
    // Unavailable/blocked quotes must not be fetched around the AppView's decision.
    if matches!(
        string(view, "$type"),
        Some(
            "app.bsky.embed.record#viewNotFound"
                | "app.bsky.embed.record#viewBlocked"
                | "app.bsky.embed.record#viewDetached"
        )
    ) {
        return Ok(None);
    }
    if string(view, "uri") != Some(uri) {
        return Err(error(
            "Bluesky quoted view does not match its record reference",
        ));
    }
    if let Some(expected) = string(reference, "cid")
        && Some(expected) != string(view, "cid")
    {
        return Err(error("Bluesky quoted view has a mismatched record CID"));
    }
    let Some(value) = view.get("value") else {
        return Ok(Some((
            uri.into(),
            string(reference, "cid").map(str::to_owned),
            None,
        )));
    };
    let mut quoted = serde_json::json!({"uri": uri, "author": view.get("author"), "record": value});
    if let Some(embed) = view
        .get("embeds")
        .and_then(Value::as_array)
        .and_then(|v| v.first())
    {
        quoted["embed"] = embed.clone();
    }
    // Validate identities even on media-free hydrated quotes.
    let (did, _) = post_identity(uri)?;
    if quoted.pointer("/author/did").and_then(Value::as_str) != Some(did) {
        return Err(error("Bluesky quoted author does not match its URI"));
    }
    Ok(Some((
        uri.into(),
        string(reference, "cid").map(str::to_owned),
        Some(quoted),
    )))
}

/// Upstream synthesizes one JPEG blob post with fixed dimensions for each
/// profile asset; the CDN URL is used only to extract a validated CID.
fn profile_asset(profile: &Value, did: &str, view: ProfileView) -> Result<Option<Media>> {
    if profile.get("did").and_then(Value::as_str) != Some(did) || !valid_did(did) {
        return Err(error("Bluesky profile does not match the requested DID"));
    }
    let (kind, prefix) = match view {
        ProfileView::Avatar => ("avatar", "avatar"),
        ProfileView::Background => ("banner", "background"),
        _ => unreachable!("only profile assets have blob files"),
    };
    let Some(raw) = profile.get(kind).filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let asset = raw
        .as_str()
        .and_then(|value| Url::parse(value).ok())
        .filter(|url| {
            url.scheme() == "https" && url.username().is_empty() && url.password().is_none()
        })
        .ok_or_else(|| error("Bluesky profile asset has an invalid URL"))?;
    let cid = asset
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .and_then(|name| name.split('@').next())
        .filter(|cid| valid_cid(cid))
        .ok_or_else(|| error("Bluesky profile asset has an invalid blob CID"))?;
    let mut url = Url::parse(BLOB_ROOT).unwrap();
    url.query_pairs_mut()
        .append_pair("did", did)
        .append_pair("cid", cid);
    Ok(Some(Media {
        url,
        job: MediaJob::DirectHttp,
        category: "bluesky".into(),
        filename: format!("{prefix}_{cid}.jpg"),
        metadata: Some(PostMetadata {
            post_id: cid.into(),
            media_id: cid.into(),
            username: string(profile, "handle").map(Arc::from),
            index: 1,
            file: Some(FileMetadata {
                width: Some(1000),
                height: Some(1000),
                media_type: Some("image/jpeg".into()),
                description: Some(kind.into()),
                ..FileMetadata::default()
            }),
            ..PostMetadata::default()
        }),
        headers: BTreeMap::new(),
        handoff: BTreeMap::new(),
    }))
}

pub struct Bluesky {
    client: Client,
    network: NetworkOptions,
    api_root: Url,
    timeout: Duration,
    request_timeout: Duration,
    retries: u8,
    rate_limits: RateLimits,
    selection: Selection,
    include: Option<Include>,
    likes_endpoint: LikesEndpoint,
    discovery_root: Option<Url>,
    pds_fallback: Url,
    discovery_cache: std::sync::Mutex<likes::DiscoveryCache>,
    /// Upstream `depth`: reply levels fetched with single-post threads.
    depth: u64,
    /// Whether `api-server` chose the API root (a login then keeps it).
    api_server_configured: bool,
    credentials: Option<session::Credentials>,
    session: tokio::sync::Mutex<Option<session::Session>>,
    /// Persists the session refresh token per login (upstream `_refresh_token_cache`).
    token_cache: Option<crate::cache::Cache>,
    /// Upstream `metadata`: `user` and `facets` keywords.
    metadata_user: bool,
    metadata_facets: bool,
    /// `metadata: user` profiles by DID.
    profiles: tokio::sync::Mutex<std::collections::HashMap<String, Arc<Value>>>,
    #[cfg(test)]
    blob_root_override: Option<Url>,
}

struct FeedState {
    did: Option<String>,
    actor: String,
    feed: Feed,
    cursor: Option<String>,
    seen_cursors: BTreeSet<String>,
    pages: usize,
    done: bool,
    posts: VecDeque<Value>,
    active: Option<SelectedState>,
}

struct SelectedState {
    post: Option<Value>,
    /// DID of the URL's actor, the `metadata` option's `user`.
    user: Option<String>,
    quote: Option<Quote>,
    quote_parent: Option<Value>,
    pending: VecDeque<ExtractionEvent>,
    seen: BTreeSet<String>,
    depth: usize,
    emitted: bool,
}

impl SelectedState {
    fn new(post: Value) -> Self {
        Self {
            post: Some(post),
            user: None,
            quote: None,
            quote_parent: None,
            pending: VecDeque::new(),
            seen: BTreeSet::new(),
            depth: 0,
            emitted: false,
        }
    }
}

impl SelectedState {
    fn with_user(mut self, user: Option<String>) -> Self {
        self.user = user;
        self
    }
}

/// Upstream `_instance`: the last two labels of a handle.
fn handle_instance(handle: &str) -> String {
    let labels: Vec<&str> = handle.rsplitn(3, '.').collect();
    labels
        .iter()
        .take(2)
        .rev()
        .copied()
        .collect::<Vec<_>>()
        .join(".")
}

/// Upstream `metadata: facets`: tags, mentioned DIDs and link URIs of a
/// record's facets (the first feature of each); posts without facets get
/// empty lists. Malformed facets are skipped.
fn facet_keywords(record: &Value) -> serde_json::Map<String, Value> {
    let mut tags = Vec::new();
    let mut dids = Vec::new();
    let mut uris = Vec::new();
    for facet in record
        .get("facets")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(features) = facet.pointer("/features/0") else {
            continue;
        };
        if let Some(tag) = features.get("tag") {
            tags.push(tag.clone());
        } else if let Some(did) = features.get("did") {
            dids.push(did.clone());
        } else if let Some(uri) = features.get("uri") {
            uris.push(uri.clone());
        }
    }
    serde_json::Map::from_iter([
        ("hashtags".into(), Value::Array(tags)),
        ("mentions".into(), Value::Array(dids)),
        ("uris".into(), Value::Array(uris)),
    ])
}

/// Bound for each `metadata` keyword value.
const MAX_EXTRA_BYTES: usize = 1024 * 1024;
/// Bound for cached `metadata: user` profiles.
const MAX_PROFILES: usize = 1024;

/// Upstream's `metadata` keywords (`user`, `hashtags`, `mentions`, `uris`)
/// from compatibility output, bounded like native values.
pub(crate) fn extras_from_keywords(
    data: &serde_json::Map<String, Value>,
) -> Option<Arc<serde_json::Map<String, Value>>> {
    let extras: serde_json::Map<String, Value> = ["user", "hashtags", "mentions", "uris"]
        .into_iter()
        .filter_map(|key| Some((key.to_owned(), data.get(key)?.clone())))
        .filter(|(_, value)| {
            serde_json::to_vec(value).is_ok_and(|bytes| bytes.len() <= MAX_EXTRA_BYTES)
        })
        .collect();
    (!extras.is_empty()).then(|| Arc::new(extras))
}

impl Bluesky {
    pub fn new(timeout: Duration) -> Result<Self> {
        Ok(Self {
            client: NetworkOptions::default().client(USER_AGENT, timeout)?,
            network: NetworkOptions::default(),
            api_root: Url::parse(API_ROOT).unwrap(),
            timeout,
            request_timeout: timeout,
            retries: 0,
            rate_limits: RateLimits::default(),
            selection: Selection::default(),
            include: None,
            likes_endpoint: LikesEndpoint::default(),
            discovery_root: None,
            pds_fallback: Url::parse("https://bsky.social/").unwrap(),
            discovery_cache: std::sync::Mutex::new(likes::DiscoveryCache::default()),
            depth: 0,
            api_server_configured: false,
            credentials: None,
            session: tokio::sync::Mutex::new(None),
            token_cache: None,
            metadata_user: false,
            metadata_facets: false,
            profiles: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            #[cfg(test)]
            blob_root_override: None,
        })
    }

    pub fn with_selection(mut self, selection: Selection) -> Self {
        self.selection = selection;
        self
    }

    pub fn with_include(mut self, include: Option<Include>) -> Self {
        self.include = include;
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

    /// Override the AppView origin; intended for local tests.
    pub fn with_api_root(mut self, root: Url) -> Self {
        self.api_root = root;
        self
    }

    fn endpoint(&self, method: &str, query: &[(&str, &str)]) -> Url {
        let mut url = self.api_root.join(method).expect("static XRPC method");
        url.query_pairs_mut().extend_pairs(query);
        url
    }

    /// Each API call is bounded by the overall timeout.
    async fn call(&self, method: &str, query: &[(&str, &str)]) -> Result<Vec<u8>> {
        let endpoint = self.endpoint(method, query);
        tokio::time::timeout(self.timeout, self.request(&endpoint))
            .await
            .map_err(|_| error("native Bluesky extraction timed out"))?
    }

    async fn resolve(&self, actor: &str) -> Result<String> {
        if valid_did(actor) {
            return Ok(actor.into());
        }
        let body = self
            .call("com.atproto.identity.resolveHandle", &[("handle", actor)])
            .await?;
        let body: Value =
            serde_json::from_slice(&body).map_err(|_| error("Bluesky returned invalid JSON"))?;
        string(&body, "did")
            .filter(|did| valid_did(did))
            .map(str::to_owned)
            .ok_or_else(|| error("Bluesky handle did not resolve to a valid DID"))
    }

    /// The post and, with upstream's `depth`, its replies.
    async fn post_thread(&self, actor: &str, rkey: &str) -> Result<Vec<Value>> {
        let did = self.resolve(actor).await?;
        let uri = format!("at://{did}/app.bsky.feed.post/{rkey}");
        let depth = self.depth.to_string();
        let body = self
            .call(
                "app.bsky.feed.getPostThread",
                &[("uri", &uri), ("depth", &depth), ("parentHeight", "0")],
            )
            .await?;
        thread_posts(&body, &uri)
    }

    async fn next_selected(&self, state: &mut SelectedState) -> Result<Option<ExtractionEvent>> {
        loop {
            if let Some(event) = state.pending.pop_front() {
                return Ok(Some(event));
            }
            // Upstream emits a post's own files before attempting its quote.
            // Invalid quote metadata must not erase an already-valid group.
            if let Some(parent) = state.quote_parent.take() {
                state.quote = quoted_post(&parent)?;
            }
            let post = match state.post.take() {
                Some(post) => post,
                None => match state.quote.take() {
                    None => return Ok(None),
                    Some((uri, expected_cid, hydrated)) => {
                        if state.seen.contains(&uri) {
                            return Ok(None);
                        }
                        if state.depth >= MAX_QUOTES {
                            return Err(error("Bluesky quoted-post chain exceeded 32 records"));
                        }
                        state.depth += 1;
                        match hydrated {
                            Some(post) => post,
                            None => {
                                let body = self
                                    .call(
                                        "app.bsky.feed.getPostThread",
                                        &[("uri", &uri), ("depth", "0"), ("parentHeight", "0")],
                                    )
                                    .await?;
                                let quoted = thread_post(&body, &uri)?;
                                if let Some(cid) = expected_cid
                                    && string(&quoted, "cid") != Some(cid.as_str())
                                {
                                    return Err(error(
                                        "Bluesky fetched quote has a mismatched record CID",
                                    ));
                                }
                                quoted
                            }
                        }
                    }
                },
            };
            let uri =
                string(&post, "uri").ok_or_else(|| error("Bluesky post is missing its URI"))?;
            if !state.seen.insert(uri.to_owned()) {
                return Ok(None);
            }
            let mut media = parse_post_view_selected(&post, self.selection)?;
            let extras = if media.is_empty() {
                None
            } else {
                self.metadata_keywords(&post, state.user.as_deref()).await?
            };
            if self.selection.quoted {
                state.quote_parent = Some(post);
            }
            state.emitted |= !media.is_empty();
            for item in &mut media {
                self.route_blob(item).await?;
                self.network.apply_media(item);
                if let Some(metadata) = &mut item.metadata {
                    metadata.extra_keywords = extras.clone();
                }
            }
            if !media.is_empty() {
                state.pending.extend(group(media, "bluesky"));
            }
        }
    }

    /// Upstream `metadata` keywords of one post: `user` is the URL actor's
    /// profile (fetched once per DID) or else the post author, each with an
    /// `instance`; `facets` adds `hashtags`, `mentions` and `uris`.
    async fn metadata_keywords(
        &self,
        post: &Value,
        user: Option<&str>,
    ) -> Result<Option<Arc<serde_json::Map<String, Value>>>> {
        if !(self.metadata_user || self.metadata_facets) {
            return Ok(None);
        }
        let mut keywords = serde_json::Map::new();
        if self.metadata_facets {
            keywords.extend(facet_keywords(post.get("record").unwrap_or(&Value::Null)));
        }
        if self.metadata_user {
            let mut user = match user {
                Some(did) => self.profile(did).await?.as_ref().clone(),
                None => post.get("author").cloned().unwrap_or(Value::Null),
            };
            if let Some(user) = user.as_object_mut()
                && let Some(handle) = user.get("handle").and_then(Value::as_str)
            {
                let instance = handle_instance(handle);
                user.insert("instance".into(), instance.into());
            }
            if serde_json::to_vec(&user).is_ok_and(|bytes| bytes.len() <= MAX_EXTRA_BYTES) {
                keywords.insert("user".into(), user);
            }
        }
        Ok(Some(Arc::new(keywords)))
    }

    /// `app.bsky.actor.getProfile` for `did`, cached per DID.
    async fn profile(&self, did: &str) -> Result<Arc<Value>> {
        let mut profiles = self.profiles.lock().await;
        if let Some(profile) = profiles.get(did) {
            return Ok(profile.clone());
        }
        let body = self
            .call("app.bsky.actor.getProfile", &[("actor", did)])
            .await?;
        let profile: Value =
            serde_json::from_slice(&body).map_err(|_| error("Bluesky returned invalid JSON"))?;
        if profile.get("did").and_then(Value::as_str) != Some(did) {
            return Err(error("Bluesky profile does not match the requested DID"));
        }
        if profiles.len() >= MAX_PROFILES {
            profiles.clear();
        }
        let profile = Arc::new(profile);
        profiles.insert(did.to_owned(), profile.clone());
        Ok(profile)
    }

    async fn next_page(&self, state: &mut FeedState) -> Result<()> {
        let did = match &state.did {
            Some(did) => did.clone(),
            // Searches and bookmarks are not owned by an actor; nothing to resolve.
            None if matches!(state.feed, Feed::Search { .. } | Feed::Bookmarks) => String::new(),
            None => {
                let did = self.resolve(&state.actor).await?;
                state.did = Some(did.clone());
                did
            }
        };
        let (method, parameter, resource) = match &state.feed {
            Feed::Media | Feed::Posts | Feed::Replies | Feed::Video => {
                ("app.bsky.feed.getAuthorFeed", "actor", did.clone())
            }
            Feed::ActorLikes => ("app.bsky.feed.getActorLikes", "actor", did.clone()),
            Feed::Custom(rkey) => (
                "app.bsky.feed.getFeed",
                "feed",
                format!("at://{did}/app.bsky.feed.generator/{rkey}"),
            ),
            Feed::List(rkey) => (
                "app.bsky.feed.getListFeed",
                "list",
                format!("at://{did}/app.bsky.graph.list/{rkey}"),
            ),
            Feed::Search { query, .. } => ("app.bsky.feed.searchPosts", "q", query.clone()),
            Feed::Bookmarks => ("app.bsky.bookmark.getBookmarks", "", String::new()),
        };
        let mut query = Vec::new();
        if !parameter.is_empty() {
            query.push((parameter, resource.as_str()));
        }
        if let Some(filter) = state.feed.filter() {
            query.push(("filter", filter));
        }
        if let Feed::Search {
            sort: Some(sort), ..
        } = &state.feed
        {
            query.push(("sort", sort));
        }
        query.push(("limit", PAGE_LIMIT));
        if let Some(cursor) = &state.cursor {
            query.push(("cursor", cursor));
        }
        let body = self.call(method, &query).await?;
        let body: Value =
            serde_json::from_slice(&body).map_err(|_| error("Bluesky returned invalid JSON"))?;
        let search = matches!(state.feed, Feed::Search { .. });
        let bookmarks = state.feed == Feed::Bookmarks;
        let items = body
            .get(match &state.feed {
                Feed::Search { .. } => "posts",
                Feed::Bookmarks => "bookmarks",
                _ => "feed",
            })
            .and_then(Value::as_array)
            .ok_or_else(|| error("Bluesky response is missing its feed"))?;
        let cursor = page_cursor(&body)?;
        state.pages += 1;
        state.done = cursor
            .as_ref()
            .is_none_or(|cursor| !state.seen_cursors.insert(cursor.clone()))
            || state.pages >= MAX_PAGES
            || matches!(state.feed, Feed::ActorLikes | Feed::Bookmarks) && items.is_empty();
        state.cursor = cursor;
        // Collection owners identify the feed/list record, not the returned post
        // authors. Upstream's reposts author restriction applies only to author feeds.
        let author = state.feed.filter().map(|_| did.as_str());
        if bookmarks {
            // Bookmarked posts are `item` views; deleted or blocked posts have
            // no record and are skipped, as upstream does.
            state.posts.extend(
                items
                    .iter()
                    .filter_map(|bookmark| bookmark.get("item"))
                    .filter(|post| post.get("record").is_some_and(Value::is_object))
                    .cloned(),
            );
        } else if search {
            // Search results are bare post views rather than feed items.
            state
                .posts
                .extend(items.iter().filter(|post| post.is_object()).cloned());
        } else {
            for post in feed_posts(items, author, self.selection) {
                state.posts.push_back(post.clone());
            }
        }
        Ok(())
    }

    pub fn stream(&self, url: &Url) -> BoxStream<'_, Result<Media>> {
        crate::event::media_only(self.events(url))
    }

    /// Emit one post boundary per post; feeds and lists are paginated lazily.
    pub fn events(&self, url: &Url) -> BoxStream<'_, Result<ExtractionEvent>> {
        let target = match target(url) {
            Ok(target) => target,
            Err(e) => return stream::once(async move { Err(e) }).boxed(),
        };
        match target {
            Target::Post { actor, rkey } => {
                stream::once(async move { self.post_thread(&actor, &rkey).await })
                    .map(move |result| match result {
                        Ok(posts) => self.thread_events(posts.into()),
                        Err(e) => stream::once(async move { Err(e) }).boxed(),
                    })
                    .flatten()
                    .boxed()
            }
            Target::Feed { actor, feed } => {
                let bare = url
                    .path_segments()
                    .is_some_and(|segments| segments.filter(|part| !part.is_empty()).count() == 2);
                if bare {
                    let default = if self.selection.quoted || self.selection.reposts {
                        "posts"
                    } else {
                        "media"
                    };
                    let views = dispatch::select(
                        self.include.as_ref(),
                        &[default],
                        &[
                            "info",
                            "avatar",
                            "background",
                            "posts",
                            "replies",
                            "media",
                            "video",
                            "likes",
                        ],
                        &[],
                    );
                    return stream::iter(views)
                        .flat_map(move |view| {
                            let actor = actor.clone();
                            child_job(
                                ChildMetadata::from_raw("bluesky", "user", &serde_json::Map::new()),
                                move || self.view_events(actor, &view),
                            )
                        })
                        .boxed();
                }
                self.feed_events(actor, feed)
            }
            Target::Profile { actor, view } => self.profile_events(actor, view),
            Target::Following { actor } => self.following_events(actor),
            Target::Search { query, sort } => {
                self.feed_events(String::new(), Feed::Search { query, sort })
            }
            Target::Bookmarks if !self.logged_in() => stream::once(async {
                Err(error(
                    "native Bluesky bookmarks require the username and password settings",
                ))
            })
            .boxed(),
            Target::Bookmarks => self.feed_events(String::new(), Feed::Bookmarks),
        }
    }

    /// Lazily page `app.bsky.graph.getFollows`, queueing each followed account
    /// as an independent user dispatcher child like upstream's Message.Queue.
    fn following_events(&self, actor: String) -> BoxStream<'_, Result<ExtractionEvent>> {
        struct State {
            actor: String,
            did: Option<String>,
            cursor: Option<String>,
            seen_cursors: BTreeSet<String>,
            pages: usize,
            done: bool,
            users: VecDeque<serde_json::Map<String, Value>>,
        }
        let state = State {
            actor,
            did: None,
            cursor: None,
            seen_cursors: BTreeSet::new(),
            pages: 0,
            done: false,
            users: VecDeque::new(),
        };
        stream::unfold(Some(state), move |state| async move {
            let mut state = state?;
            loop {
                if let Some(user) = state.users.pop_front() {
                    return Some((Ok(user), Some(state)));
                }
                if state.done {
                    return None;
                }
                let page = async {
                    let did = match &state.did {
                        Some(did) => did.clone(),
                        None => {
                            let did = self.resolve(&state.actor).await?;
                            state.did = Some(did.clone());
                            did
                        }
                    };
                    let mut query = vec![("actor", did.as_str()), ("limit", PAGE_LIMIT)];
                    if let Some(cursor) = &state.cursor {
                        query.push(("cursor", cursor));
                    }
                    let body = self.call("app.bsky.graph.getFollows", &query).await?;
                    let body: Value = serde_json::from_slice(&body)
                        .map_err(|_| error("Bluesky returned invalid JSON"))?;
                    let follows = body
                        .get("follows")
                        .and_then(Value::as_array)
                        .ok_or_else(|| error("Bluesky response is missing its follows"))?;
                    let cursor = page_cursor(&body)?;
                    state.pages += 1;
                    state.done = cursor
                        .as_ref()
                        .is_none_or(|cursor| !state.seen_cursors.insert(cursor.clone()))
                        || state.pages >= MAX_PAGES;
                    state.cursor = cursor;
                    for user in follows {
                        state.users.push_back(follow_fields(user)?);
                    }
                    Ok::<_, crate::Error>(())
                };
                if let Err(e) = page.await {
                    return Some((Err(e), None));
                }
            }
        })
        .flat_map(move |result| match result {
            Err(e) => stream::once(async move { Err(e) }).boxed(),
            Ok(user) => {
                let did = user
                    .get("did")
                    .and_then(Value::as_str)
                    .expect("validated follow DID")
                    .to_owned();
                child_job(
                    ChildMetadata::from_raw("bluesky", "following", &user),
                    move || {
                        let url = Url::parse(&format!("https://bsky.app/profile/{did}"))
                            .expect("validated DID");
                        self.events(&url)
                    },
                )
            }
        })
        .boxed()
    }

    fn view_events(&self, actor: String, view: &str) -> BoxStream<'_, Result<ExtractionEvent>> {
        match view {
            "posts" => self.feed_events(actor, Feed::Posts),
            "replies" => self.feed_events(actor, Feed::Replies),
            "media" => self.feed_events(actor, Feed::Media),
            "video" => self.feed_events(actor, Feed::Video),
            "avatar" => self.profile_events(actor, ProfileView::Avatar),
            "background" => self.profile_events(actor, ProfileView::Background),
            "likes" => self.profile_events(actor, ProfileView::Likes),
            "info" => self.profile_events(actor, ProfileView::Info),
            _ => unreachable!("dispatcher selected a known view"),
        }
    }

    fn profile_events(
        &self,
        actor: String,
        view: ProfileView,
    ) -> BoxStream<'_, Result<ExtractionEvent>> {
        match view {
            ProfileView::Likes => self.likes_events(actor),
            ProfileView::Info | ProfileView::Avatar | ProfileView::Background => {
                stream::once(async move {
                    let did = self.resolve(&actor).await?;
                    let body = self
                        .call("app.bsky.actor.getProfile", &[("actor", &did)])
                        .await?;
                    let profile: Value = serde_json::from_slice(&body)
                        .map_err(|_| error("Bluesky returned invalid JSON"))?;
                    if profile.get("did").and_then(Value::as_str) != Some(did.as_str()) {
                        return Err(error("Bluesky profile does not match the requested DID"));
                    }
                    let events = if view == ProfileView::Info {
                        // Normalized public subset; full upstream profile metadata has
                        // fields outside the native PostMetadata schema.
                        VecDeque::from([
                            ExtractionEvent::Post(Box::new(Post {
                                keywords: Default::default(),
                                category: "bluesky".into(),
                                count: Some(0),
                                metadata: Some(PostMetadata {
                                    post_id: did,
                                    username: string(&profile, "handle").map(Arc::from),
                                    caption: string(&profile, "description").map(Arc::from),
                                    ..PostMetadata::default()
                                }),
                            })),
                            ExtractionEvent::EndPost,
                        ])
                    } else {
                        match profile_asset(&profile, &did, view)? {
                            Some(mut media) => {
                                self.route_blob(&mut media).await?;
                                self.network.apply_media(&mut media);
                                group(vec![media], "bluesky")
                            }
                            None => VecDeque::new(),
                        }
                    };
                    Ok::<_, crate::Error>(stream::iter(events.into_iter().map(Ok)).boxed())
                })
                .map(|result| match result {
                    Ok(events) => events,
                    Err(error) => stream::once(async move { Err(error) }).boxed(),
                })
                .flatten()
                .boxed()
            }
        }
    }

    /// A post followed by its `depth` replies; an error only if none of them
    /// had selected media.
    fn thread_events(&self, posts: VecDeque<Value>) -> BoxStream<'_, Result<ExtractionEvent>> {
        struct State {
            posts: VecDeque<Value>,
            active: Option<SelectedState>,
            emitted: bool,
            user: Option<String>,
        }
        // The post URL's actor is the thread root's validated author.
        let user = posts
            .front()
            .and_then(|post| post.pointer("/author/did"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let state = State {
            posts,
            active: None,
            emitted: false,
            user,
        };
        stream::unfold(Some(state), move |state| async move {
            let mut state = state?;
            loop {
                if let Some(active) = &mut state.active {
                    match self.next_selected(active).await {
                        Ok(Some(event)) => return Some((Ok(event), Some(state))),
                        Ok(None) => {
                            state.emitted |= active.emitted;
                            state.active = None;
                        }
                        Err(e) => return Some((Err(e), None)),
                    }
                }
                if let Some(post) = state.posts.pop_front() {
                    let user = state.user.clone();
                    state.active = Some(SelectedState::new(post).with_user(user));
                    continue;
                }
                if state.emitted {
                    return None;
                }
                return Some((Err(error("Bluesky post contains no selected media")), None));
            }
        })
        .boxed()
    }

    fn feed_events(&self, actor: String, feed: Feed) -> BoxStream<'_, Result<ExtractionEvent>> {
        let state = FeedState {
            did: None,
            actor,
            feed,
            cursor: None,
            seen_cursors: BTreeSet::new(),
            pages: 0,
            done: false,
            posts: VecDeque::new(),
            active: None,
        };
        stream::unfold(Some(state), move |state| async move {
            let mut state = state?;
            loop {
                if let Some(active) = &mut state.active {
                    match self.next_selected(active).await {
                        Ok(Some(event)) => return Some((Ok(event), Some(state))),
                        Ok(None) => state.active = None,
                        Err(e) => return Some((Err(e), None)),
                    }
                }
                if let Some(post) = state.posts.pop_front() {
                    let user = state.did.clone().filter(|did| !did.is_empty());
                    state.active = Some(SelectedState::new(post).with_user(user));
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
        let mut attempt = 0;
        let mut renewed = false;
        loop {
            let token = self.access_token(endpoint).await?;
            match self.attempt(endpoint, attempt, token.as_ref()).await {
                Ok(body) => return Ok(body),
                // Upstream re-authenticates once its cached access token expires.
                Err(failure) if failure.expired && !renewed => {
                    renewed = true;
                    if let Some(token) = &token {
                        self.renew_session(token).await?;
                    }
                }
                Err(failure) if failure.retry && attempt < self.retries => {
                    tokio::time::sleep(backoff(attempt)).await;
                    attempt += 1;
                }
                Err(failure) => return Err(failure.error),
            }
        }
    }

    async fn attempt(
        &self,
        endpoint: &Url,
        attempt: u8,
        token: Option<&header::HeaderValue>,
    ) -> std::result::Result<Vec<u8>, Failure> {
        let mut target = endpoint.clone();
        let mut redirects = Redirects::new(endpoint);
        let mut response = loop {
            self.rate_limits.wait(&target, self.timeout).await?;
            let mut request = self
                .client
                .get(target.clone())
                .timeout(self.request_timeout)
                .header(header::ACCEPT, "application/json");
            // Session tokens never follow a redirect to another origin.
            if let Some(token) = token
                && target.origin() == self.api_root.origin()
            {
                request = request.header(header::AUTHORIZATION, token.clone());
            }
            let response = request
                .send()
                .await
                .map_err(|_| Failure::transient("Bluesky request failed; check network access"))?;
            match redirects.next(response.url(), response.status(), response.headers())? {
                Some(next) => target = next,
                None => break response,
            }
        };
        let status = response.status();
        if token.is_some() && matches!(status.as_u16(), 400 | 401) {
            let mut body = Vec::new();
            while let Ok(Some(chunk)) = response.chunk().await {
                if body.len() + chunk.len() > 64 * 1024 {
                    break;
                }
                body.extend_from_slice(&chunk);
            }
            let code = serde_json::from_slice::<Value>(&body)
                .ok()
                .and_then(|body| string(&body, "error").map(str::to_owned));
            if status == StatusCode::UNAUTHORIZED
                || matches!(code.as_deref(), Some("ExpiredToken" | "InvalidToken"))
            {
                return Err(Failure {
                    expired: true,
                    ..error("Bluesky refused the session token; check username and password").into()
                });
            }
        }
        if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
            let delay = retry_after(response.headers(), SystemTime::now());
            self.rate_limits
                .defer(endpoint, delay.unwrap_or_else(|| backoff(attempt)));
            return Err(Failure::transient(
                if status == StatusCode::TOO_MANY_REQUESTS {
                    match delay {
                        Some(delay) => {
                            format!(
                                "Bluesky rate limit; retry after {} seconds",
                                delay.as_secs()
                            )
                        }
                        None => "Bluesky rate limit; retry later".into(),
                    }
                } else {
                    format!("Bluesky returned HTTP {status}")
                },
            ));
        }
        if status == StatusCode::BAD_REQUEST {
            // XRPC reports unknown actors, handles and posts as 400 errors.
            return Err(error("Bluesky profile, feed, list or post not found").into());
        }
        if matches!(status.as_u16(), 404 | 410) {
            return Err(error("Bluesky profile, feed, list or post not found or removed").into());
        }
        if matches!(status.as_u16(), 401 | 403) {
            return Err(error("Bluesky refused access to this content").into());
        }
        if !status.is_success() {
            return Err(error(format!("Bluesky returned HTTP {status}")).into());
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| Failure::transient("could not read Bluesky response"))?
        {
            if body.len() + chunk.len() > MAX_RESPONSE {
                return Err(error("Bluesky response exceeded 16 MiB").into());
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }
}

/// Bounded public queue fields of a followed profile view; the DID is validated
/// before it can become a child URL.
fn follow_fields(user: &Value) -> Result<serde_json::Map<String, Value>> {
    let did = string(user, "did")
        .filter(|did| valid_did(did))
        .ok_or_else(|| error("Bluesky follow has an invalid DID"))?;
    let mut fields = serde_json::Map::new();
    fields.insert("did".into(), did.into());
    for key in ["handle", "displayName", "description"] {
        if let Some(value) = string(user, key) {
            fields.insert(key.into(), value.into());
        }
    }
    Ok(fields)
}

fn backoff(attempt: u8) -> Duration {
    Duration::from_millis(250 * (1u64 << attempt.min(6)))
}

struct Failure {
    error: crate::Error,
    retry: bool,
    /// The session's access token was refused; renew it once and retry.
    expired: bool,
}

impl Failure {
    fn transient(message: impl Into<String>) -> Self {
        Self {
            error: error(message),
            retry: true,
            expired: false,
        }
    }
}

impl From<crate::Error> for Failure {
    fn from(error: crate::Error) -> Self {
        Self {
            error,
            retry: false,
            expired: false,
        }
    }
}

// BlueskyExtractor.items reuses post and updates each prepared media map.
// Include native file aliases without resetting unrelated post-level fields.
fn group(media: Vec<Media>, category: &str) -> VecDeque<ExtractionEvent> {
    group_with_keywords(
        media,
        category,
        KeywordPolicy::shared(&[
            "num",
            "index",
            "media_id",
            "filename",
            "name",
            "extension",
            "url",
            "_url",
            "_filesize",
            "description",
            "width",
            "height",
            "file",
        ]),
    )
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

    const DID: &str = "did:plc:abcdefghijklmnopqrstuvwx";
    const CID1: &str = "bafkreiaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const CID2: &str = "bafkreibbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const CID3: &str = "bafkreicccccccccccccccccccccccccccccccccccccccccccccccccccc";

    fn image(cid: &str, alt: &str) -> Value {
        json!({"alt": alt, "aspectRatio": {"width": 800, "height": 600},
               "image": {"$type": "blob", "ref": {"$link": cid}, "mimeType": "image/jpeg", "size": 1}})
    }

    fn post(rkey: &str, embed: Value) -> Value {
        json!({
            "uri": format!("at://{DID}/app.bsky.feed.post/{rkey}"),
            "cid": "bafyreipost",
            "author": {"did": DID, "handle": "alice.bsky.social"},
            "record": {"$type": "app.bsky.feed.post", "text": "hello",
                       "createdAt": "2024-01-02T03:04:05.678Z", "embed": embed},
        })
    }

    fn thread(rkey: &str, embed: Value) -> Vec<u8> {
        json!({"thread": {"$type": "app.bsky.feed.defs#threadViewPost", "post": post(rkey, embed)}})
            .to_string()
            .into_bytes()
    }

    #[test]
    fn shared_keyword_policy_declares_source_file_assignments() {
        let media=parse_thread(&thread("3kabc",json!({"$type":"app.bsky.embed.images","images":[image(CID1,"first"),image(CID2,"second")]})),DID,"3kabc").unwrap();
        let events = group(media, "bluesky");
        let ExtractionEvent::Post(post) = &events[0] else {
            panic!("missing post");
        };
        let KeywordPolicy::Shared { file_fields, .. } = &post.keywords else {
            panic!("expected shared Bluesky keywords");
        };
        for field in ["num", "index", "media_id", "description", "width", "height"] {
            assert!(file_fields.iter().any(|f| f == field));
        }
        assert!(!file_fields.iter().any(|f| f == "caption"));
    }

    #[test]
    fn parses_images_with_metadata_and_blob_urls() {
        let embed = json!({"$type": "app.bsky.embed.images",
                           "images": [image(CID1, "first"), image(CID2, "")]});
        let media = parse_thread(&thread("3kabc", embed), DID, "3kabc").unwrap();
        assert_eq!(media.len(), 2);
        assert_eq!(
            media[0].url.as_str(),
            format!(
                "https://bsky.social/xrpc/com.atproto.sync.getBlob?did=did%3Aplc%3Aabcdefghijklmnopqrstuvwx&cid={CID1}"
            )
        );
        assert_eq!(media[0].filename, "3kabc_1.jpg");
        assert_eq!(media[1].filename, "3kabc_2.jpg");
        let meta = media[0].metadata.as_ref().unwrap();
        assert_eq!(meta.post_id, "3kabc");
        assert_eq!(meta.media_id, CID1);
        assert_eq!(meta.index, 1);
        assert_eq!(meta.username.as_deref(), Some("alice.bsky.social"));
        assert_eq!(meta.caption.as_deref(), Some("hello"));
        assert_eq!(meta.timestamp, Some(1704164645));
        let file = meta.file.as_ref().unwrap();
        assert_eq!((file.width, file.height), (Some(800), Some(600)));
        assert_eq!(file.description.as_deref(), Some("first"));
        assert_eq!(file.media_type.as_deref(), Some("image/jpeg"));
        assert!(
            media[1]
                .metadata
                .as_ref()
                .unwrap()
                .file
                .as_ref()
                .unwrap()
                .description
                .is_none()
        );
        assert!(media[0].archive_key().unwrap().starts_with("bluesky:v1:"));
        assert_ne!(media[0].archive_key(), media[1].archive_key());
    }

    #[test]
    fn parses_video_and_record_with_media() {
        let video = json!({"$type": "app.bsky.embed.video", "alt": "clip",
                           "aspectRatio": {"width": 1280, "height": 720},
                           "video": {"ref": {"$link": CID3}, "mimeType": "video/mp4"}});
        let media = parse_thread(&thread("3kvid", video.clone()), DID, "3kvid").unwrap();
        assert_eq!(media[0].filename, "3kvid_1.mp4");
        let wrapped = json!({"$type": "app.bsky.embed.recordWithMedia",
                             "record": {"record": {"uri": "at://x/y/z"}}, "media": video});
        let quoted = parse_thread(&thread("3kq", wrapped), DID, "3kq").unwrap();
        assert_eq!(quoted.len(), 1);
        assert_eq!(quoted[0].archive_key(), media[0].archive_key());
    }

    #[test]
    fn rejects_invalid_mismatched_and_empty_posts() {
        let embed = json!({"$type": "app.bsky.embed.images", "images": [image(CID1, "")]});
        assert!(parse_thread(&thread("3kabc", embed.clone()), DID, "other").is_err());
        let bad = json!({"$type": "app.bsky.embed.images", "images": [image("../x", "")]});
        assert!(parse_thread(&thread("3kabc", bad), DID, "3kabc").is_err());
        let none = json!({"$type": "app.bsky.embed.external", "external": {}});
        assert!(parse_thread(&thread("3kabc", none), DID, "3kabc").is_err());
        assert!(
            parse_thread(
                br#"{"thread":{"$type":"app.bsky.feed.defs#notFoundPost"}}"#,
                DID,
                "3kabc"
            )
            .is_err()
        );
        assert!(parse_thread(b"nope", DID, "3kabc").is_err());
        let mut forged = post("3kabc", embed);
        forged["author"]["did"] = json!("did:plc:zzzzzzzzzzzzzzzzzzzzzzzz");
        assert!(parse_post_view(&forged).is_err());
    }

    #[test]
    fn feed_keeps_own_posts_with_reason_and_skips_media_free_posts() {
        let embed = json!({"$type": "app.bsky.embed.images", "images": [image(CID1, "")]});
        let body = json!({"cursor": "c2", "feed": [
            {"post": post("3ka", embed.clone())},
            {"post": post("3kb", embed.clone()), "reason": {"$type": "app.bsky.feed.defs#reasonRepost"}},
            {"post": post("3kc", json!(null))},
        ]});
        let page = parse_feed(body.to_string().as_bytes(), DID).unwrap();
        assert_eq!(page.posts.len(), 2);
        assert_eq!(page.posts[0][0].metadata.as_ref().unwrap().post_id, "3ka");
        assert_eq!(page.cursor.as_deref(), Some("c2"));
    }

    #[test]
    fn classifies_supported_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://bsky.app/profile/Alice.bsky.social/post/3kabc").unwrap(),
            Target::Post {
                actor: "alice.bsky.social".into(),
                rkey: "3kabc".into()
            }
        );
        assert_eq!(
            t(&format!("https://bsky.app/profile/{DID}")).unwrap(),
            Target::Feed {
                actor: DID.into(),
                feed: Feed::Media
            }
        );
        assert_eq!(
            t("https://bsky.app/profile/alice.bsky.social/media").unwrap(),
            Target::Feed {
                actor: "alice.bsky.social".into(),
                feed: Feed::Media
            }
        );
        assert_eq!(
            t("https://bsky.app/profile/alice.bsky.social/posts").unwrap(),
            Target::Feed {
                actor: "alice.bsky.social".into(),
                feed: Feed::Posts
            }
        );
        for raw in [
            "https://bsky.app/profile/alice/post/3kabc",
            "https://bsky.app/profile/alice.bsky.social/post/a%2Fb",
            "https://bsky.app/search",
            "https://bsky.app.evil.test/profile/alice.bsky.social",
            "https://bsky.app:444/profile/alice.bsky.social",
            "https://bsky.app/",
        ] {
            assert!(t(raw).is_err(), "{raw}");
        }
        assert!(valid_did("did:web:example.com"));
        assert!(!valid_did("did:plc:short"));
    }

    fn response(status: &str, headers: &str, body: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n",
            body.len()
        )
        .into_bytes();
        out.extend_from_slice(body);
        out
    }

    fn serve(responses: Vec<Vec<u8>>) -> (Url, JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let root = Url::parse(&format!("http://{}/xrpc/", listener.local_addr().unwrap())).unwrap();
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

    fn bluesky(root: Url) -> Bluesky {
        Bluesky::new(Duration::from_secs(5))
            .unwrap()
            .with_network(&NetworkOptions::new(Some(String::new()), None).unwrap())
            .unwrap()
            .with_api_root(root)
            .with_blob_test_root(Some(Url::parse("https://bsky.social/").unwrap()))
    }

    #[tokio::test]
    async fn resolves_handle_then_fetches_post_with_retries() {
        let embed = json!({"$type": "app.bsky.embed.images", "images": [image(CID1, "")]});
        let resolved = json!({"did": DID}).to_string();
        let (root, server) = serve(vec![
            response("200 OK", "", resolved.as_bytes()),
            response("429 Too Many Requests", "Retry-After: 0\r\n", b""),
            response("200 OK", "", &thread("3kabc", embed)),
        ]);
        let events: Vec<_> = bluesky(root)
            .with_retries(1)
            .events(&Url::parse("https://bsky.app/profile/alice.bsky.social/post/3kabc").unwrap())
            .collect()
            .await;
        assert_eq!(events.len(), 3, "{events:?}");
        assert!(matches!(events[1], Ok(ExtractionEvent::Media(_))));
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with(
                "GET /xrpc/com.atproto.identity.resolveHandle?handle=alice.bsky.social "
            )
        );
        assert!(requests[2].starts_with(
            "GET /xrpc/app.bsky.feed.getPostThread?uri=at%3A%2F%2Fdid%3Aplc%3Aabcdefghijklmnopqrstuvwx%2Fapp.bsky.feed.post%2F3kabc&depth=0"
        ));
    }

    #[tokio::test]
    async fn paginates_author_feed_until_cursor_ends() {
        let embed = json!({"$type": "app.bsky.embed.images", "images": [image(CID1, "")]});
        let first = json!({"cursor": "next", "feed": [{"post": post("3ka", embed.clone())}]});
        let second = json!({"feed": [{"post": post("3kb", embed)}]});
        let (root, server) = serve(vec![
            response("200 OK", "", first.to_string().as_bytes()),
            response("200 OK", "", second.to_string().as_bytes()),
        ]);
        let media: Vec<_> = bluesky(root)
            .stream(&Url::parse(&format!("https://bsky.app/profile/{DID}/media")).unwrap())
            .collect()
            .await;
        assert_eq!(media.len(), 2);
        let requests = server.join().unwrap();
        assert!(requests[0].contains("filter=posts_with_media&limit=100 "));
        assert!(requests[1].contains("&cursor=next "));
    }

    #[tokio::test]
    async fn async_blobs_follow_each_post_and_quote_author_pds_without_changing_archives() {
        let other = "did:web:bob.example";
        let mut quoted = post(
            "quoted",
            json!({"$type":"app.bsky.embed.images","images":[image(CID1, "quote")]}),
        );
        quoted["uri"] = json!(format!("at://{other}/app.bsky.feed.post/quoted"));
        quoted["author"]["did"] = json!(other);
        let mut original = quote_post("original", &quoted, true);
        let reference = original["record"]["embed"].clone();
        let view = original["embed"].clone();
        original["record"]["embed"] = json!({"$type":"app.bsky.embed.recordWithMedia","record":reference,
            "media":{"$type":"app.bsky.embed.images","images":[image(CID1, "own"),image(CID2, "second")]}});
        original["embed"] = json!({"$type":"app.bsky.embed.recordWithMedia#view","record":view});
        let (own_pds, own_server) = serve(vec![
            response("200 OK", "", b"own-1"),
            response("200 OK", "", b"own-2"),
        ]);
        let (quote_pds, quote_server) = serve(vec![response("200 OK", "", b"quote")]);
        let own_base = own_pds.join("../owner/").unwrap();
        let quote_base = quote_pds.join("../quote/").unwrap();
        let (discovery, discovery_server) = serve(vec![
            response("200 OK", "", &did_document(DID, &own_base)),
            response("200 OK", "", &did_document(other, &quote_base)),
        ]);
        let (api, api_server) = serve(vec![response("200 OK", "", &thread_view(original.clone()))]);
        let client = bluesky(api)
            .with_blob_test_root(None)
            .with_likes_test_roots(discovery.join("../").unwrap(), own_base.clone())
            .with_selection(Selection {
                quoted: true,
                ..Selection::default()
            });
        let result = client
            .stream(&Url::parse(&format!("https://bsky.app/profile/{DID}/post/original")).unwrap())
            .collect::<Vec<_>>()
            .await;
        assert_eq!(result.len(), 3, "{result:?}");
        let expected = parse_post_view(&original)
            .unwrap()
            .into_iter()
            .chain(parse_post_view(&quoted).unwrap())
            .collect::<Vec<_>>();
        for (index, item) in result.iter().enumerate() {
            let item = item.as_ref().unwrap();
            assert_eq!(item.archive_key(), expected[index].archive_key());
            let base = if index < 2 { &own_base } else { &quote_base };
            assert_eq!(item.url.origin(), base.origin());
            assert_eq!(
                item.url.path(),
                format!("{}xrpc/com.atproto.sync.getBlob", base.path())
            );
            assert_eq!(
                item.url
                    .query_pairs()
                    .find(|(key, _)| key == "did")
                    .unwrap()
                    .1,
                if index < 2 { DID } else { other }
            );
            let response = client.client.get(item.url.clone()).send().await.unwrap();
            assert!(response.status().is_success());
        }
        assert_ne!(
            result[0].as_ref().unwrap().archive_key(),
            result[2].as_ref().unwrap().archive_key()
        );
        let discovery_requests = discovery_server.join().unwrap();
        assert_eq!(discovery_requests.len(), 2);
        assert!(discovery_requests[0].starts_with(&format!("GET /{DID} ")));
        assert!(discovery_requests[1].starts_with("GET /.well-known/did.json "));
        for request in own_server
            .join()
            .unwrap()
            .into_iter()
            .chain(quote_server.join().unwrap())
        {
            let request = request.to_ascii_lowercase();
            assert!(!request.contains("authorization:"));
            assert!(!request.contains("cookie:"));
        }
        assert_eq!(api_server.join().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn concurrent_streams_share_one_inflight_author_discovery() {
        let body = thread(
            "same",
            json!({"$type":"app.bsky.embed.images","images":[image(CID1, "")]}),
        );
        let (api, api_server) = serve(vec![response("200 OK", "", &body); 2]);
        let base = Url::parse("http://127.0.0.1:9/pds/").unwrap();
        let (discovery, discovery_server) =
            serve(vec![response("200 OK", "", &did_document(DID, &base))]);
        let client = bluesky(api)
            .with_blob_test_root(None)
            .with_likes_test_roots(
                discovery.join("../").unwrap(),
                Url::parse("http://127.0.0.1:9/fallback/").unwrap(),
            );
        let url = Url::parse(&format!("https://bsky.app/profile/{DID}/post/same")).unwrap();
        let (first, second) = futures_util::future::join(
            client.stream(&url).collect::<Vec<_>>(),
            client.stream(&url).collect::<Vec<_>>(),
        )
        .await;
        for result in [first, second] {
            assert_eq!(result.len(), 1);
            assert_eq!(
                result[0].as_ref().unwrap().url.path(),
                "/pds/xrpc/com.atproto.sync.getBlob"
            );
        }
        assert_eq!(api_server.join().unwrap().len(), 2);
        assert_eq!(discovery_server.join().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn profile_assets_and_repeated_feed_pages_share_author_service_and_cache_fallbacks() {
        for use_assets in [true, false] {
            let (pds, pds_server) = serve(vec![
                response("200 OK", "", b"one"),
                response("200 OK", "", b"two"),
            ]);
            let base = pds.join("../base/").unwrap();
            let discovery_reply = if use_assets {
                did_document(DID, &base)
            } else {
                b"invalid private document".to_vec()
            };
            let (discovery, discovery_server) =
                serve(vec![response("200 OK", "", &discovery_reply)]);
            let responses = if use_assets {
                vec![response("200 OK", "", profile().to_string().as_bytes()); 2]
            } else {
                let mut first: Value = serde_json::from_str(&feed_body("first", CID1)).unwrap();
                first["cursor"] = json!("next");
                vec![
                    response("200 OK", "", first.to_string().as_bytes()),
                    response("200 OK", "", feed_body("second", CID2).as_bytes()),
                ]
            };
            let (api, api_server) = serve(responses);
            let mut client = bluesky(api)
                .with_blob_test_root(None)
                .with_likes_test_roots(discovery.join("../").unwrap(), base.clone());
            let suffix = if use_assets {
                client = client.with_include(Some(Include::Views(vec![
                    "avatar".into(),
                    "background".into(),
                ])));
                ""
            } else {
                "/media"
            };
            let result = client
                .stream(&Url::parse(&format!("https://bsky.app/profile/{DID}{suffix}")).unwrap())
                .collect::<Vec<_>>()
                .await;
            assert_eq!(result.len(), 2, "{result:?}");
            for item in result {
                let item = item.unwrap();
                assert_eq!(item.url.origin(), base.origin());
                assert!(item.url.path().starts_with("/base/xrpc/"));
                assert!(item.archive_key().is_some());
                assert!(
                    !serde_json::to_string(&item.metadata)
                        .unwrap()
                        .contains("private")
                );
                assert!(
                    client
                        .client
                        .get(item.url)
                        .send()
                        .await
                        .unwrap()
                        .status()
                        .is_success()
                );
            }
            assert_eq!(discovery_server.join().unwrap().len(), 1);
            assert_eq!(api_server.join().unwrap().len(), 2);
            assert_eq!(pds_server.join().unwrap().len(), 2);
        }
    }

    #[tokio::test]
    async fn liked_owner_pds_discovery_is_reused_for_same_author_blobs_including_fallback() {
        for discover in [true, false] {
            let liked = post(
                "liked",
                json!({"$type":"app.bsky.embed.images","images":[image(CID1, "")]}),
            );
            let records = json!({"records":[like_record(DID,"a",&liked)]}).to_string();
            let (pds, pds_server) = serve(vec![
                response("200 OK", "", records.as_bytes()),
                response("200 OK", "", b"blob"),
            ]);
            let base = pds.join("../").unwrap();
            let document = if discover {
                did_document(DID, &base)
            } else {
                b"invalid".to_vec()
            };
            let (discovery, discovery_server) = serve(vec![response("200 OK", "", &document)]);
            let (api, api_server) =
                serve(vec![response("200 OK", "", &thread_view(liked.clone()))]);
            let client = bluesky(api)
                .with_blob_test_root(None)
                .with_likes_test_roots(discovery.join("../").unwrap(), base.clone());
            let result = client
                .stream(&Url::parse(&format!("https://bsky.app/profile/{DID}/likes")).unwrap())
                .collect::<Vec<_>>()
                .await;
            assert_eq!(result.len(), 1, "{result:?}");
            let media = result[0].as_ref().unwrap();
            assert_eq!(media.url.origin(), base.origin());
            assert_eq!(
                media.archive_key(),
                parse_post_view(&liked).unwrap()[0].archive_key()
            );
            assert!(
                client
                    .client
                    .get(media.url.clone())
                    .send()
                    .await
                    .unwrap()
                    .status()
                    .is_success()
            );
            let requests = pds_server.join().unwrap();
            assert!(requests[0].contains("com.atproto.repo.listRecords"));
            assert!(requests[1].contains("com.atproto.sync.getBlob"));
            assert_eq!(discovery_server.join().unwrap().len(), 1);
            assert_eq!(api_server.join().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn selected_media_only_triggers_current_author_discovery_before_drop() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let discovery = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        for suffix in ["media", "info"] {
            let body = if suffix == "info" {
                profile().to_string()
            } else {
                json!({"feed":[{"post":post("video",json!({"$type":"app.bsky.embed.video","video":{"ref":{"$link":CID1},"mimeType":"video/mp4"}}))}]}).to_string()
            };
            let (api, api_server) = serve(vec![response("200 OK", "", body.as_bytes())]);
            let client = bluesky(api)
                .with_blob_test_root(None)
                .with_likes_test_roots(
                    discovery.clone(),
                    Url::parse("http://127.0.0.1:9/").unwrap(),
                )
                .with_selection(Selection {
                    videos: false,
                    ..Selection::default()
                });
            let results = client
                .stream(&Url::parse(&format!("https://bsky.app/profile/{DID}/{suffix}")).unwrap())
                .collect::<Vec<_>>()
                .await;
            assert!(results.is_empty());
            assert_eq!(api_server.join().unwrap().len(), 1);
            assert_eq!(
                listener.accept().unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
        }
        let mut quoted = post(
            "quoted",
            json!({"$type":"app.bsky.embed.images","images":[image(CID2, "")]}),
        );
        quoted["uri"] = json!("at://did:web:later.example/app.bsky.feed.post/quoted");
        quoted["author"]["did"] = json!("did:web:later.example");
        let mut original = quote_post("original", &quoted, true);
        let reference = original["record"]["embed"].clone();
        let view = original["embed"].clone();
        original["record"]["embed"] = json!({"$type":"app.bsky.embed.recordWithMedia","record":reference,"media":{"$type":"app.bsky.embed.images","images":[image(CID1, "")]}});
        original["embed"] = json!({"$type":"app.bsky.embed.recordWithMedia#view","record":view});
        let (api, api_server) = serve(vec![response("200 OK", "", &thread_view(original))]);
        let base = Url::parse("http://127.0.0.1:9/base/").unwrap();
        let (discovery, discovery_server) =
            serve(vec![response("200 OK", "", &did_document(DID, &base))]);
        let client = bluesky(api)
            .with_blob_test_root(None)
            .with_likes_test_roots(discovery.join("../").unwrap(), base)
            .with_selection(Selection {
                quoted: true,
                ..Selection::default()
            });
        let results = client
            .stream(&Url::parse(&format!("https://bsky.app/profile/{DID}/post/original")).unwrap())
            .take(1)
            .collect::<Vec<_>>()
            .await;
        assert_eq!(results.len(), 1);
        assert_eq!(api_server.join().unwrap().len(), 1);
        assert_eq!(discovery_server.join().unwrap().len(), 1);
    }

    fn like_record(owner: &str, key: &str, liked: &Value) -> Value {
        json!({"uri": format!("at://{owner}/app.bsky.feed.like/{key}"), "cid": CID3,
            "value": {"$type": "app.bsky.feed.like", "subject": {"uri": liked["uri"], "cid": liked["cid"]}}})
    }

    fn did_document(owner: &str, service: &Url) -> Vec<u8> {
        json!({"id":owner,"service":[{"id":"#atproto_pds","type":"AtprotoPersonalDataServer","serviceEndpoint":service}]}).to_string().into_bytes()
    }

    #[tokio::test]
    async fn record_likes_discover_owner_pds_and_preserve_mixed_post_identity_and_opaque_cursors() {
        let owner = "did:web:owner.example";
        let first = post(
            "first",
            json!({"$type":"app.bsky.embed.images","images":[image(CID1, "")]}),
        );
        let mut second = post(
            "second",
            json!({"$type":"app.bsky.embed.images","images":[image(CID2, "")]}),
        );
        second["uri"] = json!("at://did:web:bob.example/app.bsky.feed.post/second");
        second["author"]["did"] = json!("did:web:bob.example");
        second["author"]["handle"] = json!("bob.example");
        let (pds, pds_server) = serve(vec![
            response(
                "200 OK",
                "",
                br#"{"records":[],"cursor":" page+1?x=y&z=1 "}"#,
            ),
            response(
                "200 OK",
                "",
                json!({"records":[like_record(owner,"a",&first),like_record(owner,"b",&second)]})
                    .to_string()
                    .as_bytes(),
            ),
        ]);
        let pds_base = pds.join("../pds/").unwrap();
        let (discovery, discovery_server) = serve(vec![response(
            "200 OK",
            "",
            &did_document(owner, &pds_base),
        )]);
        let (api, api_server) = serve(vec![
            response("200 OK", "", json!({"did":owner}).to_string().as_bytes()),
            response("200 OK", "", &thread_view(first.clone())),
            response("200 OK", "", &thread_view(second.clone())),
        ]);
        let client = bluesky(api)
            .with_likes_test_roots(discovery.join("../").unwrap(), pds.join("../").unwrap());
        let result = client
            .stream(&Url::parse("https://bsky.app/profile/owner.example/likes").unwrap())
            .collect::<Vec<_>>()
            .await;
        assert_eq!(result.len(), 2, "{result:?}");
        for (actual, expected) in result.iter().zip([&first, &second]) {
            assert_eq!(
                actual.as_ref().unwrap().archive_key(),
                parse_post_view(expected).unwrap()[0].archive_key()
            );
        }
        let requests = pds_server.join().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].starts_with("GET /pds/xrpc/com.atproto.repo.listRecords?repo=did%3Aweb%3Aowner.example&collection=app.bsky.feed.like&limit=100 "));
        let second_url = Url::parse(&format!(
            "http://test{}",
            requests[1]
                .lines()
                .next()
                .unwrap()
                .split_whitespace()
                .nth(1)
                .unwrap()
        ))
        .unwrap();
        assert_eq!(
            second_url
                .query_pairs()
                .find(|(key, _)| key == "cursor")
                .unwrap()
                .1,
            " page+1?x=y&z=1 "
        );
        let discovery_requests = discovery_server.join().unwrap();
        assert!(discovery_requests[0].starts_with("GET /.well-known/did.json "));
        let api_requests = api_server.join().unwrap();
        assert_eq!(api_requests.len(), 3);
        assert!(api_requests[0].contains("resolveHandle"));
        assert!(
            api_requests[1..]
                .iter()
                .all(|request| request.contains("getPostThread"))
        );
        for request in requests
            .iter()
            .chain(&discovery_requests)
            .chain(&api_requests)
        {
            let request = request.to_ascii_lowercase();
            assert!(!request.contains("authorization:"));
            assert!(!request.contains("cookie:"));
        }
    }

    #[tokio::test]
    async fn malformed_deleted_inaccessible_and_contradictory_likes_do_not_poison_later_records() {
        let image_embed = json!({"$type":"app.bsky.embed.images","images":[image(CID1, "")]});
        let valid = post("valid", image_embed.clone());
        let missing = post("missing", image_embed.clone());
        let forbidden = post("forbidden", image_embed.clone());
        let mismatched = post("mismatch", image_embed.clone());
        let bad_author = post("author", image_embed);
        let mut wrong_owner = like_record(DID, "owner", &valid);
        wrong_owner["uri"] = json!("at://did:web:wrong.example/app.bsky.feed.like/a");
        let mut wrong_collection = like_record(DID, "collection", &valid);
        wrong_collection["uri"] = json!(format!("at://{DID}/app.bsky.feed.post/a"));
        let mut wrong_uri = like_record(DID, "uri", &valid);
        wrong_uri["value"]["subject"]["uri"] = json!("https://private.example/secret");
        let mut wrong_cid = like_record(DID, "cid", &valid);
        wrong_cid["value"]["subject"]["cid"] = json!("private/secret");
        let records = vec![
            wrong_owner,
            wrong_collection,
            wrong_uri,
            wrong_cid,
            like_record(DID, "missing", &missing),
            like_record(DID, "forbidden", &forbidden),
            like_record(DID, "mismatch", &mismatched),
            like_record(DID, "author", &bad_author),
            like_record(DID, "good", &valid),
        ];
        let (pds, pds_server) = serve(vec![response(
            "200 OK",
            "",
            json!({"records":records}).to_string().as_bytes(),
        )]);
        let (discovery, discovery_server) = serve(vec![response(
            "200 OK",
            "",
            &did_document(DID, &pds.join("../").unwrap()),
        )]);
        let mut mismatched = mismatched;
        mismatched["cid"] = json!(CID2);
        let mut bad_author = bad_author;
        bad_author["author"]["did"] = json!("did:web:forged.example");
        let (api, api_server) = serve(vec![
            response("400 Bad Request", "", b"{}"),
            response("403 Forbidden", "", b"{}"),
            response("200 OK", "", &thread_view(mismatched)),
            response("200 OK", "", &thread_view(bad_author)),
            response("200 OK", "", &thread_view(valid)),
        ]);
        let client = bluesky(api)
            .with_likes_test_roots(discovery.join("../").unwrap(), pds.join("../").unwrap());
        let results = client
            .stream(&Url::parse(&format!("https://bsky.app/profile/{DID}/likes")).unwrap())
            .collect::<Vec<_>>()
            .await;
        assert_eq!(results.len(), 1, "{results:?}");
        assert_eq!(
            results[0]
                .as_ref()
                .unwrap()
                .metadata
                .as_ref()
                .unwrap()
                .post_id,
            "valid"
        );
        assert_eq!(api_server.join().unwrap().len(), 5);
        assert_eq!(pds_server.join().unwrap().len(), 1);
        assert!(discovery_server.join().unwrap()[0].starts_with(&format!("GET /{DID} ")));
    }

    #[tokio::test]
    async fn liked_own_media_survives_bad_quotes_and_later_likes_remain_balanced() {
        let mut first = post(
            "first",
            json!({"$type":"app.bsky.embed.recordWithMedia",
            "media":{"$type":"app.bsky.embed.images","images":[image(CID1, "")]},
            "record":{"$type":"app.bsky.embed.record","record":{"uri":"https://private.example/secret","cid":CID3}}}),
        );
        first["embed"] = json!(null);
        let second = post(
            "second",
            json!({"$type":"app.bsky.embed.images","images":[image(CID2, "")]}),
        );
        let (pds, pds_server) = serve(vec![response(
            "200 OK",
            "",
            json!({"records":[like_record(DID,"a",&first),like_record(DID,"b",&second)]})
                .to_string()
                .as_bytes(),
        )]);
        let (discovery, discovery_server) = serve(vec![response(
            "200 OK",
            "",
            &did_document(DID, &pds.join("../").unwrap()),
        )]);
        let (api, api_server) = serve(vec![
            response("200 OK", "", &thread_view(first)),
            response("200 OK", "", &thread_view(second)),
        ]);
        let client = bluesky(api)
            .with_selection(Selection {
                quoted: true,
                ..Selection::default()
            })
            .with_likes_test_roots(discovery.join("../").unwrap(), pds.join("../").unwrap());
        let results = crate::job_selection::JobSelection::default()
            .apply(
                client
                    .events(&Url::parse(&format!("https://bsky.app/profile/{DID}/likes")).unwrap()),
            )
            .collect::<Vec<_>>()
            .await;
        assert_eq!(results.len(), 2, "{results:?}");
        assert_eq!(
            results[0]
                .as_ref()
                .unwrap()
                .metadata
                .as_ref()
                .unwrap()
                .post_id,
            "first"
        );
        assert_eq!(
            results[1]
                .as_ref()
                .unwrap()
                .metadata
                .as_ref()
                .unwrap()
                .post_id,
            "second"
        );
        assert_eq!(api_server.join().unwrap().len(), 2);
        assert_eq!(pds_server.join().unwrap().len(), 1);
        assert_eq!(discovery_server.join().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn invalid_or_failed_discovery_falls_back_without_disclosing_or_fetching_unsafe_endpoints()
     {
        for document in [
            br#"invalid"#.to_vec(),
            json!({"id":"did:web:wrong.example","service":[]})
                .to_string()
                .into_bytes(),
            did_document(
                DID,
                &Url::parse("https://user:secret@private.example/").unwrap(),
            ),
            did_document(
                DID,
                &Url::parse("https://private.example/?secret=value").unwrap(),
            ),
            did_document(DID, &Url::parse("file:///private/secret").unwrap()),
        ] {
            let (pds, pds_server) = serve(vec![response("200 OK", "", br#"{"records":[]}"#)]);
            let (discovery, discovery_server) = serve(vec![response("200 OK", "", &document)]);
            let client = bluesky(Url::parse("http://127.0.0.1:9/xrpc/").unwrap())
                .with_likes_test_roots(discovery.join("../").unwrap(), pds.join("../").unwrap());
            let results = client
                .stream(&Url::parse(&format!("https://bsky.app/profile/{DID}/likes")).unwrap())
                .collect::<Vec<_>>()
                .await;
            assert!(results.is_empty(), "{results:?}");
            assert_eq!(pds_server.join().unwrap().len(), 1);
            assert_eq!(discovery_server.join().unwrap().len(), 1);
        }
        let (pds, pds_server) = serve(vec![response("200 OK", "", br#"{"records":[]}"#)]);
        let (discovery, discovery_server) = serve(vec![response(
            "503 Service Unavailable",
            "",
            b"private body",
        )]);
        let client = bluesky(Url::parse("http://127.0.0.1:9/xrpc/").unwrap())
            .with_likes_test_roots(discovery.join("../").unwrap(), pds.join("../").unwrap());
        assert!(
            client
                .stream(&Url::parse(&format!("https://bsky.app/profile/{DID}/likes")).unwrap())
                .collect::<Vec<_>>()
                .await
                .is_empty()
        );
        assert_eq!(pds_server.join().unwrap().len(), 1);
        assert_eq!(discovery_server.join().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn liked_post_hydration_and_later_pages_are_cancelled_before_siblings_resume() {
        use crate::{job_selection::JobSelection, selection::RangeSelection};
        let liked = post(
            "liked",
            json!({"$type":"app.bsky.embed.images","images":[image(CID1, "")]}),
        );
        let later = post(
            "later",
            json!({"$type":"app.bsky.embed.images","images":[image(CID2, "")]}),
        );
        let (pds,pds_server)=serve(vec![response("200 OK","",json!({"cursor":"next","records":[like_record(DID,"a",&liked),like_record(DID,"b",&later)]}).to_string().as_bytes())]);
        let (discovery, discovery_server) = serve(vec![response(
            "200 OK",
            "",
            &did_document(DID, &pds.join("../").unwrap()),
        )]);
        let (api, api_server) = serve(vec![
            response("200 OK", "", &thread_view(liked)),
            response("200 OK", "", feed_body("sibling", CID3).as_bytes()),
        ]);
        let client = bluesky(api)
            .with_likes_test_roots(discovery.join("../").unwrap(), pds.join("../").unwrap())
            .with_include(Some(Include::Views(vec!["likes".into(), "media".into()])));
        let jobs = JobSelection {
            file_range: Some(RangeSelection::parse("1").unwrap()),
            ..JobSelection::default()
        };
        let results = jobs
            .apply(client.events(&Url::parse(&format!("https://bsky.app/profile/{DID}")).unwrap()))
            .collect::<Vec<_>>()
            .await;
        assert_eq!(results.len(), 2, "{results:?}");
        assert_eq!(
            results[0]
                .as_ref()
                .unwrap()
                .metadata
                .as_ref()
                .unwrap()
                .post_id,
            "liked"
        );
        assert_eq!(
            results[1]
                .as_ref()
                .unwrap()
                .metadata
                .as_ref()
                .unwrap()
                .post_id,
            "sibling"
        );
        assert_eq!(api_server.join().unwrap().len(), 2);
        assert_eq!(pds_server.join().unwrap().len(), 1);
        assert_eq!(discovery_server.join().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn record_likes_cursor_cycles_stop_and_actor_likes_empty_pages_stop_without_pds_discovery()
     {
        let (pds, pds_server) = serve(vec![
            response("200 OK", "", br#"{"records":[],"cursor":"a"}"#),
            response("200 OK", "", br#"{"records":[],"cursor":"b"}"#),
            response("200 OK", "", br#"{"records":[],"cursor":"a"}"#),
        ]);
        let (discovery, discovery_server) = serve(vec![response(
            "200 OK",
            "",
            &did_document(DID, &pds.join("../").unwrap()),
        )]);
        let client = bluesky(Url::parse("http://127.0.0.1:9/xrpc/").unwrap())
            .with_likes_test_roots(discovery.join("../").unwrap(), pds.join("../").unwrap());
        assert!(
            client
                .stream(&Url::parse(&format!("https://bsky.app/profile/{DID}/likes")).unwrap())
                .collect::<Vec<_>>()
                .await
                .is_empty()
        );
        assert_eq!(pds_server.join().unwrap().len(), 3);
        assert_eq!(discovery_server.join().unwrap().len(), 1);
        let (api, api_server) = serve(vec![response(
            "200 OK",
            "",
            br#"{"feed":[],"cursor":"next"}"#,
        )]);
        let client = bluesky(api)
            .with_likes_endpoint(LikesEndpoint::ActorLikes)
            .with_include(Some(Include::Views(vec![])));
        assert!(
            client
                .stream(&Url::parse(&format!("https://bsky.app/profile/{DID}/likes")).unwrap())
                .collect::<Vec<_>>()
                .await
                .is_empty()
        );
        assert!(api_server.join().unwrap()[0].contains("app.bsky.feed.getActorLikes?actor="));
    }

    fn profile() -> Value {
        json!({"did": DID, "handle": "alice.bsky.social", "description": "About Alice",
            "avatar": format!("https://cdn.bsky.app/img/avatar/plain/{DID}/{CID1}@jpeg"),
            "banner": format!("https://cdn.bsky.app/img/banner/plain/{DID}/{CID2}@jpeg")})
    }

    fn feed_body(rkey: &str, cid: &str) -> String {
        json!({"feed": [{"post": post(rkey, json!({"$type": "app.bsky.embed.images", "images": [image(cid, "")] }))}]}).to_string()
    }

    #[test]
    fn profile_assets_are_validated_blob_jobs_with_upstream_synthetic_metadata() {
        for (view, prefix, cid, alt) in [
            (ProfileView::Avatar, "avatar", CID1, "avatar"),
            (ProfileView::Background, "background", CID2, "banner"),
        ] {
            let asset = profile_asset(&profile(), DID, view).unwrap().unwrap();
            assert_eq!(asset.filename, format!("{prefix}_{cid}.jpg"));
            assert_eq!(
                asset.url.query_pairs().find(|(k, _)| k == "did").unwrap().1,
                DID
            );
            let meta = asset.metadata.as_ref().unwrap();
            assert_eq!(meta.post_id, cid);
            assert_eq!(meta.media_id, cid);
            assert_eq!(meta.username.as_deref(), Some("alice.bsky.social"));
            assert_eq!(meta.timestamp, None);
            let file = meta.file.as_ref().unwrap();
            assert_eq!((file.width, file.height), (Some(1000), Some(1000)));
            assert_eq!(file.description.as_deref(), Some(alt));
            assert_eq!(file.media_type.as_deref(), Some("image/jpeg"));
            assert!(asset.archive_key().is_some());
        }
        let mut invalid = profile();
        invalid["did"] = json!("did:web:other.example");
        assert!(profile_asset(&invalid, DID, ProfileView::Avatar).is_err());
        for raw in [
            "https://cdn.bsky.app/invalid@jpeg",
            "file:///private/blob",
            "https://user:secret@cdn.bsky.app/blob",
        ] {
            let mut invalid = profile();
            invalid["avatar"] = json!(raw);
            let err = profile_asset(&invalid, DID, ProfileView::Avatar)
                .unwrap_err()
                .to_string();
            assert!(!err.contains("secret"));
        }
        assert!(
            profile_asset(&json!({"did": DID}), DID, ProfileView::Avatar)
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn include_all_preserves_complete_catalog_order_including_actor_likes() {
        let profile = profile().to_string();
        let mut responses = vec![response("200 OK", "", profile.as_bytes()); 3];
        for (rkey, cid) in [
            ("posts", CID3),
            ("replies", CID1),
            ("media", CID2),
            ("video", CID3),
        ] {
            responses.push(response("200 OK", "", feed_body(rkey, cid).as_bytes()));
        }
        responses.push(response("200 OK", "", br#"{"feed":[]}"#));
        let (root, server) = serve(responses);
        let client = bluesky(root)
            .with_include(Some(Include::All))
            .with_likes_endpoint(LikesEndpoint::ActorLikes);
        let url = Url::parse(&format!("https://bsky.app/profile/{DID}")).unwrap();
        let events = client.events(&url).collect::<Vec<_>>().await;
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, Ok(ExtractionEvent::BeginChild(_))))
                .count(),
            8
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, Ok(ExtractionEvent::EndChild(_))))
                .count(),
            8
        );
        let posts = events
            .iter()
            .filter_map(|e| match e {
                Ok(ExtractionEvent::Post(post)) => Some(post),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(posts.len(), 7);
        assert_eq!(posts[0].count, Some(0));
        let info = posts[0].metadata.as_ref().unwrap();
        assert_eq!(info.post_id, DID);
        assert_eq!(info.username.as_deref(), Some("alice.bsky.social"));
        assert_eq!(info.caption.as_deref(), Some("About Alice"));
        assert_eq!(
            posts
                .iter()
                .map(|p| p.metadata.as_ref().unwrap().post_id.as_str())
                .collect::<Vec<_>>(),
            vec![DID, CID1, CID2, "posts", "replies", "media", "video"]
        );
        assert_eq!(events.iter().filter(|e| e.is_err()).count(), 0);
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 8);
        assert!(requests[7].contains("app.bsky.feed.getActorLikes?actor="));
        for request in &requests[..3] {
            assert!(request.contains("app.bsky.actor.getProfile?actor="));
        }
        for (request, filter) in requests[3..].iter().zip([
            "posts_and_author_threads",
            "posts_with_replies",
            "posts_with_media",
            "posts_with_video",
        ]) {
            assert!(request.contains(&format!("filter={filter}&limit=100 ")));
        }
    }

    #[tokio::test]
    async fn include_order_and_duplicates_are_preserved_and_unknown_or_empty_views_do_no_http() {
        let (root, server) = serve(vec![
            response("200 OK", "", feed_body("first", CID1).as_bytes()),
            response("200 OK", "", feed_body("second", CID2).as_bytes()),
            response("200 OK", "", feed_body("third", CID3).as_bytes()),
        ]);
        let client = bluesky(root).with_include(Some(Include::Views(vec![
            "video".into(),
            "media".into(),
            "video".into(),
            "banner".into(),
            "unknown".into(),
        ])));
        let url = Url::parse(&format!("https://bsky.app/profile/{DID}/")).unwrap();
        let result = client.stream(&url).collect::<Vec<_>>().await;
        assert_eq!(
            result
                .iter()
                .map(|e| e
                    .as_ref()
                    .unwrap()
                    .metadata
                    .as_ref()
                    .unwrap()
                    .post_id
                    .as_str())
                .collect::<Vec<_>>(),
            vec!["first", "second", "third"]
        );
        let requests = server.join().unwrap();
        assert!(requests[0].contains("filter=posts_with_video"));
        assert!(requests[1].contains("filter=posts_with_media"));
        assert!(requests[2].contains("filter=posts_with_video"));
        for views in [vec![], vec!["unknown".into(), "banner".into()]] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let root =
                Url::parse(&format!("http://{}/xrpc/", listener.local_addr().unwrap())).unwrap();
            let client = bluesky(root).with_include(Some(Include::Views(views)));
            assert!(client.events(&url).next().await.is_none());
            assert_eq!(
                listener.accept().unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
        }
    }

    #[tokio::test]
    async fn per_child_file_and_post_limits_resume_include_siblings_without_paging() {
        use crate::{
            job_selection::JobSelection, post_selection::PostSelection, selection::RangeSelection,
        };
        let page = |rkey: &str, cid: &str| {
            json!({"cursor": "next", "feed": [
            {"post": post(rkey, json!({"$type": "app.bsky.embed.images", "images": [image(cid, "")]}))},
            {"post": post("later", json!({"$type": "app.bsky.embed.images", "images": [image(CID3, "")]}))}
        ]}).to_string()
        };
        for by_post in [false, true] {
            let (root, server) = serve(vec![
                response("200 OK", "", page("first", CID1).as_bytes()),
                response("200 OK", "", page("second", CID2).as_bytes()),
            ]);
            let client = bluesky(root)
                .with_include(Some(Include::Views(vec!["media".into(), "replies".into()])));
            let jobs = if by_post {
                JobSelection {
                    post_selection: Some(PostSelection::new(
                        Some(RangeSelection::parse("1").unwrap()),
                        None,
                    )),
                    ..JobSelection::default()
                }
            } else {
                JobSelection {
                    file_range: Some(RangeSelection::parse("1").unwrap()),
                    ..JobSelection::default()
                }
            };
            let url = Url::parse(&format!("https://bsky.app/profile/{DID}")).unwrap();
            let result = jobs.apply(client.events(&url)).collect::<Vec<_>>().await;
            assert_eq!(result.len(), 2, "{result:?}");
            assert_eq!(
                result[0]
                    .as_ref()
                    .unwrap()
                    .metadata
                    .as_ref()
                    .unwrap()
                    .post_id,
                "first"
            );
            assert_eq!(
                result[1]
                    .as_ref()
                    .unwrap()
                    .metadata
                    .as_ref()
                    .unwrap()
                    .post_id,
                "second"
            );
            assert_eq!(server.join().unwrap().len(), 2);
        }
    }

    #[tokio::test]
    async fn scoped_likes_request_error_and_cancelled_asset_do_not_block_later_include_siblings() {
        use crate::job_selection::JobSelection;
        let (root, server) = serve(vec![
            response("401 Unauthorized", "", b"{}"),
            response("200 OK", "", feed_body("later", CID1).as_bytes()),
        ]);
        let client = bluesky(root)
            .with_likes_endpoint(LikesEndpoint::ActorLikes)
            .with_include(Some(Include::Views(vec!["likes".into(), "media".into()])));
        let url = Url::parse(&format!("https://bsky.app/profile/{DID}")).unwrap();
        let result = JobSelection::default()
            .apply(client.events(&url))
            .collect::<Vec<_>>()
            .await;
        assert_eq!(result.len(), 2, "{result:?}");
        assert!(result[0].is_err());
        assert_eq!(
            result[1]
                .as_ref()
                .unwrap()
                .metadata
                .as_ref()
                .unwrap()
                .post_id,
            "later"
        );
        assert_eq!(server.join().unwrap().len(), 2);
        let (root, server) = serve(vec![response(
            "200 OK",
            "",
            feed_body("after-cancel", CID1).as_bytes(),
        )]);
        let client =
            bluesky(root).with_include(Some(Include::Views(vec!["avatar".into(), "media".into()])));
        let mut events = client.events(&url);
        let ExtractionEvent::BeginChild(first) = events.next().await.unwrap().unwrap() else {
            panic!("missing asset child")
        };
        first.control.cancel();
        assert!(matches!(
            events.next().await.unwrap().unwrap(),
            ExtractionEvent::EndChild(_)
        ));
        assert!(matches!(
            events.next().await.unwrap().unwrap(),
            ExtractionEvent::BeginChild(_)
        ));
        let remaining = events.collect::<Vec<_>>().await;
        assert!(
            remaining
                .iter()
                .any(|e| matches!(e, Ok(ExtractionEvent::Media(_))))
        );
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn explicit_new_profile_views_ignore_include_and_validate_profile_identity() {
        for (suffix, filter) in [
            ("replies", "posts_with_replies"),
            ("video", "posts_with_video"),
        ] {
            let (root, server) = serve(vec![response(
                "200 OK",
                "",
                feed_body("root", CID1).as_bytes(),
            )]);
            let client = bluesky(root).with_include(Some(Include::Views(vec![])));
            let url = Url::parse(&format!("https://bsky.app/profile/{DID}/{suffix}")).unwrap();
            assert_eq!(client.stream(&url).collect::<Vec<_>>().await.len(), 1);
            assert!(server.join().unwrap()[0].contains(&format!("filter={filter}")));
        }
        for (suffix, prefix) in [
            ("avatar", "avatar"),
            ("banner", "background"),
            ("background", "background"),
        ] {
            let (root, server) = serve(vec![response(
                "200 OK",
                "",
                profile().to_string().as_bytes(),
            )]);
            let client = bluesky(root).with_include(Some(Include::Views(vec![])));
            let result = client
                .stream(&Url::parse(&format!("https://bsky.app/profile/{DID}/{suffix}")).unwrap())
                .collect::<Vec<_>>()
                .await;
            assert_eq!(result.len(), 1);
            assert!(
                result[0]
                    .as_ref()
                    .unwrap()
                    .filename
                    .starts_with(&format!("{prefix}_"))
            );
            assert_eq!(server.join().unwrap().len(), 1);
        }
        let mut forged = profile();
        forged["did"] = json!("did:web:other.example");
        let (root, server) = serve(vec![response("200 OK", "", forged.to_string().as_bytes())]);
        let client = bluesky(root).with_include(Some(Include::Views(vec![])));
        let results = client
            .events(&Url::parse(&format!("https://bsky.app/profile/{DID}/info")).unwrap())
            .collect::<Vec<_>>()
            .await;
        assert!(
            results[0]
                .as_ref()
                .unwrap_err()
                .to_string()
                .contains("requested DID")
        );
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn bare_profile_opens_an_empty_child_before_actor_resolution_and_cancels_without_http() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let root = Url::parse(&format!("http://{}/xrpc/", listener.local_addr().unwrap())).unwrap();
        let client = bluesky(root);
        let url = Url::parse("https://bsky.app/profile/alice.bsky.social/").unwrap();
        let mut events = client.events(&url);
        let ExtractionEvent::BeginChild(scope) = events.next().await.unwrap().unwrap() else {
            panic!("missing bare-profile dispatcher child");
        };
        assert_eq!(scope.metadata.category(), "bluesky");
        assert_eq!(scope.metadata.subcategory(), "user");
        assert!(scope.metadata.fields().is_empty());
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        scope.control.cancel();
        let ExtractionEvent::EndChild(end) = events.next().await.unwrap().unwrap() else {
            panic!("missing canceled child end");
        };
        assert!(scope.control.same_scope(&end));
        assert!(events.next().await.is_none());
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[tokio::test]
    async fn rejected_or_empty_bare_profile_child_selection_makes_no_requests() {
        use crate::{
            filter::FilterExpression, job_selection::JobSelection, selection::RangeSelection,
        };
        for selection in [
            JobSelection {
                child_filter: Some(FilterExpression::parse("category == 'other'").unwrap()),
                ..JobSelection::default()
            },
            JobSelection {
                child_range: Some(RangeSelection::parse("2").unwrap()),
                ..JobSelection::default()
            },
            JobSelection {
                child_range: Some(RangeSelection::parse("").unwrap()),
                ..JobSelection::default()
            },
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let root =
                Url::parse(&format!("http://{}/xrpc/", listener.local_addr().unwrap())).unwrap();
            let client = bluesky(root).with_request_timeout(Duration::from_millis(50));
            let url = Url::parse("https://bsky.app/profile/alice.bsky.social").unwrap();
            let results = selection
                .apply(client.events(&url))
                .collect::<Vec<_>>()
                .await;
            assert!(results.is_empty(), "{results:?}");
            assert_eq!(
                listener.accept().unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
        }
    }

    #[tokio::test]
    async fn accepted_bare_profile_child_uses_upstream_default_and_keeps_direct_media_identity() {
        use crate::{
            filter::FilterExpression, job_selection::JobSelection, selection::RangeSelection,
        };
        let image_embed = json!({"$type": "app.bsky.embed.images", "images": [image(CID1, "")]});
        let body = json!({"feed": [{"post": post("same", image_embed)}]}).to_string();
        let resolved = json!({"did": DID}).to_string();
        let expected = parse_post_view(&post(
            "same",
            json!({"$type": "app.bsky.embed.images", "images": [image(CID1, "")]}),
        ))
        .unwrap()
        .remove(0);
        for (selection, suffix, filter) in [
            (Selection::default(), "media", "posts_with_media"),
            (
                Selection {
                    quoted: true,
                    ..Selection::default()
                },
                "posts",
                "posts_and_author_threads",
            ),
            (
                Selection {
                    reposts: true,
                    ..Selection::default()
                },
                "posts",
                "posts_and_author_threads",
            ),
        ] {
            for input in ["".to_owned(), format!("/{suffix}")] {
                let (root, server) = serve(vec![
                    response("200 OK", "", resolved.as_bytes()),
                    response("200 OK", "", body.as_bytes()),
                ]);
                let client = bluesky(root).with_selection(selection);
                let url = Url::parse(&format!(
                    "https://bsky.app/profile/alice.bsky.social{input}"
                ))
                .unwrap();
                let jobs = JobSelection {
                    child_range: Some(RangeSelection::parse("1").unwrap()),
                    child_filter: Some(
                        FilterExpression::parse("category == 'bluesky' and subcategory == 'user'")
                            .unwrap(),
                    ),
                    ..JobSelection::default()
                };
                let result = jobs.apply(client.events(&url)).collect::<Vec<_>>().await;
                assert_eq!(result.len(), 1, "{result:?}");
                let actual = result[0].as_ref().unwrap();
                assert_eq!(actual.archive_key(), expected.archive_key());
                assert_eq!(actual.filename, expected.filename);
                assert_eq!(actual.url, expected.url);
                let requests = server.join().unwrap();
                assert_eq!(requests.len(), 2);
                assert!(requests[0].contains("resolveHandle?handle=alice.bsky.social "));
                assert!(requests[1].contains(&format!("filter={filter}&limit=100 ")));
            }
        }
    }

    #[tokio::test]
    async fn explicit_bluesky_views_remain_roots_despite_rejected_or_empty_child_predicates() {
        use crate::{
            filter::FilterExpression, job_selection::JobSelection, selection::RangeSelection,
        };
        let embed = json!({"$type": "app.bsky.embed.images", "images": [image(CID1, "")]});
        for suffix in ["media", "posts", "feed/chosen", "lists/chosen", "post/root"] {
            let body = if suffix.starts_with("post/") {
                thread("root", embed.clone())
            } else {
                json!({"feed": [{"post": post("root", embed.clone())}]})
                    .to_string()
                    .into_bytes()
            };
            let (root, server) = serve(vec![response("200 OK", "", &body)]);
            let client = bluesky(root);
            let url = Url::parse(&format!("https://bsky.app/profile/{DID}/{suffix}")).unwrap();
            let jobs = JobSelection {
                child_range: Some(RangeSelection::parse("").unwrap()),
                child_filter: Some(FilterExpression::parse("category == 'other'").unwrap()),
                ..JobSelection::default()
            };
            let result = jobs.apply(client.events(&url)).collect::<Vec<_>>().await;
            assert_eq!(result.len(), 1, "{suffix}: {result:?}");
            assert!(result[0].is_ok());
            assert_eq!(server.join().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn bare_profile_dispatches_one_job_for_all_posts_in_the_selected_feed() {
        let body = json!({"feed": [
            {"post": post("first", json!({"$type": "app.bsky.embed.images", "images": [image(CID1, "")]}))},
            {"post": post("second", json!({"$type": "app.bsky.embed.images", "images": [image(CID2, "")]}))}
        ]}).to_string();
        let (root, server) = serve(vec![response("200 OK", "", body.as_bytes())]);
        let client = bluesky(root);
        let url = Url::parse(&format!("https://bsky.app/profile/{DID}")).unwrap();
        let events = client.events(&url).collect::<Vec<_>>().await;
        assert_eq!(events.len(), 8, "{events:?}");
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, Ok(ExtractionEvent::BeginChild(_))))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, Ok(ExtractionEvent::EndChild(_))))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, Ok(ExtractionEvent::Post(_))))
                .count(),
            2
        );
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[test]
    fn classifies_feed_and_list_records_with_validated_owners_and_keys() {
        for (suffix, feed) in [
            ("feed/custom-name", Feed::Custom("custom-name".into())),
            ("lists/3klist", Feed::List("3klist".into())),
        ] {
            let url = Url::parse(&format!(
                "https://bsky.app/profile/Owner.bsky.social/{suffix}"
            ))
            .unwrap();
            assert_eq!(
                target(&url).unwrap(),
                Target::Feed {
                    actor: "owner.bsky.social".into(),
                    feed
                }
            );
        }
        let url = Url::parse(&format!("https://bsky.app/profile/{DID}/feed/3kfeed")).unwrap();
        assert_eq!(
            target(&url).unwrap(),
            Target::Feed {
                actor: DID.into(),
                feed: Feed::Custom("3kfeed".into())
            }
        );
        for path in [
            "alice/feed/key",
            "alice.bsky.social/feed/a%2Fb",
            "alice.bsky.social/lists/a%3Fb",
            "alice.bsky.social/feed/key/extra",
            "alice.bsky.social/lists",
            "did:plc:short/lists/key",
        ] {
            assert!(
                target(&Url::parse(&format!("https://bsky.app/profile/{path}")).unwrap()).is_err(),
                "{path}"
            );
        }
    }

    #[test]
    fn feed_cursors_preserve_opaque_values_and_reject_unsupported_shapes_and_sizes() {
        let body = json!({"feed": [], "cursor": " opaque +/=? cursor "});
        assert_eq!(
            parse_feed(body.to_string().as_bytes(), DID)
                .unwrap()
                .cursor
                .as_deref(),
            Some(" opaque +/=? cursor ")
        );
        for cursor in [json!(false), json!([]), json!({}), json!("x".repeat(1025))] {
            let body = json!({"feed": [], "cursor": cursor});
            assert!(parse_feed(body.to_string().as_bytes(), DID).is_err());
        }
    }

    #[tokio::test]
    async fn collection_owners_resolve_once_and_mixed_authors_and_reposts_are_retained() {
        for (path, method, parameter, collection) in [
            ("feed", "getFeed", "feed", "app.bsky.feed.generator"),
            ("lists", "getListFeed", "list", "app.bsky.graph.list"),
        ] {
            let owner = "did:web:owner.example";
            let page = |rkey: &str, cid: &str, cursor: &str| {
                json!({"cursor": cursor, "feed": [{
                "post": post(rkey, json!({"$type": "app.bsky.embed.images", "images": [image(cid, "")] })),
                "reason": {"$type": "app.bsky.feed.defs#reasonRepost"}
            }]}).to_string()
            };
            let cursor = " page+1?cursor=x&limit=1 ";
            let (root, server) = serve(vec![
                response("200 OK", "", json!({"did": owner}).to_string().as_bytes()),
                response("200 OK", "", page("first", CID1, cursor).as_bytes()),
                response("200 OK", "", page("second", CID2, "page2").as_bytes()),
                response("200 OK", "", page("third", CID3, cursor).as_bytes()),
            ]);
            let media = bluesky(root)
                .stream(
                    &Url::parse(&format!(
                        "https://bsky.app/profile/owner.bsky.social/{path}/chosen"
                    ))
                    .unwrap(),
                )
                .collect::<Vec<_>>()
                .await;
            assert_eq!(media.len(), 3, "{path}: {media:?}");
            for (index, file) in media.into_iter().enumerate() {
                let file = file.unwrap();
                assert_eq!(
                    file.url
                        .query_pairs()
                        .find(|(key, _)| key == "did")
                        .unwrap()
                        .1,
                    DID
                );
                assert_eq!(
                    file.metadata.as_ref().unwrap().post_id,
                    ["first", "second", "third"][index]
                );
                assert!(file.archive_key().is_some());
            }
            let requests = server.join().unwrap();
            assert_eq!(requests.len(), 4);
            assert!(requests[0].contains("resolveHandle?handle=owner.bsky.social "));
            for (index, request) in requests[1..].iter().enumerate() {
                let target = request
                    .lines()
                    .next()
                    .unwrap()
                    .split_whitespace()
                    .nth(1)
                    .unwrap();
                let url = Url::parse(&format!("http://test{target}")).unwrap();
                assert_eq!(url.path(), format!("/xrpc/app.bsky.feed.{method}"));
                let query = url.query_pairs().collect::<BTreeMap<_, _>>();
                assert_eq!(
                    query.get(parameter).unwrap(),
                    &format!("at://{owner}/{collection}/chosen")
                );
                assert_eq!(query.get("limit").unwrap(), "100");
                assert!(!query.contains_key("actor"));
                assert!(!query.contains_key("filter"));
                if index == 0 {
                    assert!(!query.contains_key("cursor"));
                } else {
                    assert_eq!(
                        query.get("cursor").unwrap(),
                        if index == 1 { cursor } else { "page2" }
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn collections_cancel_before_later_posts_quotes_and_pages() {
        for path in ["feed", "lists"] {
            let quoted = post("quoted", Value::Null);
            let mut original = quote_post("root", &quoted, false);
            let reference = original["record"]["embed"].clone();
            original["record"]["embed"] = json!({"$type": "app.bsky.embed.recordWithMedia",
                "record": reference, "media": {"$type": "app.bsky.embed.images", "images": [image(CID1, "own")]}});
            let body = json!({"cursor": "next", "feed": [{"post": original},
                {"post": {"uri": "https://evil.example/invalid"}}]})
            .to_string();
            let (root, server) = serve(vec![response("200 OK", "", body.as_bytes())]);
            let media = bluesky(root)
                .with_selection(Selection {
                    quoted: true,
                    ..Selection::default()
                })
                .stream(
                    &Url::parse(&format!("https://bsky.app/profile/{DID}/{path}/chosen")).unwrap(),
                )
                .take(1)
                .collect::<Vec<_>>()
                .await;
            assert_eq!(
                media[0]
                    .as_ref()
                    .unwrap()
                    .metadata
                    .as_ref()
                    .unwrap()
                    .post_id,
                "root"
            );
            assert_eq!(server.join().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn collections_validate_post_identity_independently_of_owner() {
        let mut forged = post(
            "forged",
            json!({"$type": "app.bsky.embed.images", "images": [image(CID1, "")] }),
        );
        forged["author"]["did"] = json!("did:web:other.example");
        for path in ["feed", "lists"] {
            let body = json!({"feed": [{"post": forged}]}).to_string();
            let (root, server) = serve(vec![response("200 OK", "", body.as_bytes())]);
            let results = bluesky(root)
                .stream(
                    &Url::parse(&format!("https://bsky.app/profile/{DID}/{path}/chosen")).unwrap(),
                )
                .collect::<Vec<_>>()
                .await;
            assert!(
                results[0]
                    .as_ref()
                    .unwrap_err()
                    .to_string()
                    .contains("author does not match")
            );
            assert_eq!(server.join().unwrap().len(), 1);
        }
    }

    fn quote_post(rkey: &str, quoted: &Value, hydrated: bool) -> Value {
        let mut root = post(
            rkey,
            json!({"$type": "app.bsky.embed.record", "record": {
                "uri": quoted["uri"], "cid": quoted["cid"]
            }}),
        );
        if hydrated {
            root["embed"] = json!({"$type": "app.bsky.embed.record#view", "record": {
                "$type": "app.bsky.embed.record#viewRecord", "uri": quoted["uri"],
                "cid": quoted["cid"], "author": quoted["author"], "value": quoted["record"]
            }});
            if let Some(embed) = quoted.get("embed") {
                root["embed"]["record"]["embeds"] = json!([embed]);
            }
        }
        root
    }

    fn thread_view(post: Value) -> Vec<u8> {
        json!({"thread": {"$type": "app.bsky.feed.defs#threadViewPost", "post": post}})
            .to_string()
            .into_bytes()
    }

    #[tokio::test]
    async fn quoted_inline_media_is_opt_in_and_preserves_underlying_identity() {
        let embed = json!({"$type": "app.bsky.embed.images", "images": [image(CID1, "quote")]});
        let quoted = post("quoted", embed);
        let root_post = quote_post("root", &quoted, true);
        let expected = parse_post_view(&quoted).unwrap().remove(0);
        let (root, server) = serve(vec![response(
            "200 OK",
            "",
            &thread_view(root_post.clone()),
        )]);
        let url = Url::parse(&format!("https://bsky.app/profile/{DID}/post/root")).unwrap();
        let result = bluesky(root).stream(&url).collect::<Vec<_>>().await;
        assert!(
            result[0]
                .as_ref()
                .unwrap_err()
                .to_string()
                .contains("no selected media")
        );
        assert_eq!(server.join().unwrap().len(), 1);
        let (root, server) = serve(vec![response("200 OK", "", &thread_view(root_post))]);
        let result = bluesky(root)
            .with_selection(Selection {
                quoted: true,
                ..Selection::default()
            })
            .stream(&url)
            .collect::<Vec<_>>()
            .await;
        let actual = result[0].as_ref().unwrap();
        assert_eq!(actual.archive_key(), expected.archive_key());
        assert_eq!(actual.metadata.as_ref().unwrap().post_id, "quoted");
        assert_eq!(actual.filename, "quoted_1.jpg");
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn fetches_only_missing_selected_quotes_and_checks_fetched_identity() {
        let quoted = post(
            "quoted",
            json!({"$type": "app.bsky.embed.images", "images": [image(CID1, "")] }),
        );
        let root_post = quote_post("root", &quoted, false);
        let url = Url::parse(&format!("https://bsky.app/profile/{DID}/post/root")).unwrap();
        let (root, server) = serve(vec![
            response("200 OK", "", &thread_view(root_post.clone())),
            response("200 OK", "", &thread_view(quoted.clone())),
        ]);
        let result = bluesky(root)
            .with_selection(Selection {
                quoted: true,
                ..Selection::default()
            })
            .stream(&url)
            .collect::<Vec<_>>()
            .await;
        assert_eq!(result.len(), 1);
        assert!(result[0].is_ok());
        let requests = server.join().unwrap();
        assert!(requests[1].contains("%2Fquoted&depth=0"));
        let mut forged = quoted;
        forged["cid"] = json!(CID2);
        let (root, server) = serve(vec![
            response("200 OK", "", &thread_view(root_post)),
            response("200 OK", "", &thread_view(forged)),
        ]);
        let result = bluesky(root)
            .with_selection(Selection {
                quoted: true,
                ..Selection::default()
            })
            .stream(&url)
            .collect::<Vec<_>>()
            .await;
        assert!(
            result[0]
                .as_ref()
                .unwrap_err()
                .to_string()
                .contains("mismatched record CID")
        );
        assert_eq!(server.join().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn record_with_media_quote_emits_separate_post_boundaries_in_feeds() {
        let quoted = post(
            "quoted",
            json!({"$type": "app.bsky.embed.images", "images": [image(CID1, "quote")] }),
        );
        let mut original = quote_post("root", &quoted, true);
        let reference = original["record"]["embed"].clone();
        let view = original["embed"].clone();
        original["record"]["embed"] = json!({"$type": "app.bsky.embed.recordWithMedia",
            "record": reference, "media": {"$type": "app.bsky.embed.images", "images": [image(CID2, "own")]}});
        original["embed"] = json!({"$type": "app.bsky.embed.recordWithMedia#view", "record": view});
        let body = json!({"feed": [{"post": original}]}).to_string();
        let (root, server) = serve(vec![response("200 OK", "", body.as_bytes())]);
        let events = bluesky(root)
            .with_selection(Selection {
                quoted: true,
                ..Selection::default()
            })
            .events(&Url::parse(&format!("https://bsky.app/profile/{DID}/media")).unwrap())
            .collect::<Vec<_>>()
            .await;
        assert_eq!(events.len(), 6);
        let ExtractionEvent::Media(first) = events[1].as_ref().unwrap() else {
            panic!("missing own media")
        };
        let ExtractionEvent::Media(second) = events[4].as_ref().unwrap() else {
            panic!("missing quote media")
        };
        assert_eq!(first.metadata.as_ref().unwrap().post_id, "root");
        assert_eq!(second.metadata.as_ref().unwrap().post_id, "quoted");
        assert_eq!(
            second.archive_key(),
            parse_post_view(&quoted).unwrap()[0].archive_key()
        );
        assert!(server.join().unwrap()[0].contains("filter=posts_with_media"));
    }

    #[tokio::test]
    async fn quote_requests_are_lazy_after_the_original_post_media() {
        let quoted = post("quoted", Value::Null);
        let mut original = quote_post("root", &quoted, false);
        let reference = original["record"]["embed"].clone();
        original["record"]["embed"] = json!({"$type": "app.bsky.embed.recordWithMedia",
            "record": reference, "media": {"$type": "app.bsky.embed.images", "images": [image(CID1, "own")]}});
        let (root, server) = serve(vec![response("200 OK", "", &thread_view(original))]);
        let result = bluesky(root)
            .with_selection(Selection {
                quoted: true,
                ..Selection::default()
            })
            .stream(&Url::parse(&format!("https://bsky.app/profile/{DID}/post/root")).unwrap())
            .take(1)
            .collect::<Vec<_>>()
            .await;
        assert_eq!(
            result[0]
                .as_ref()
                .unwrap()
                .metadata
                .as_ref()
                .unwrap()
                .post_id,
            "root"
        );
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn quote_chains_are_bounded_and_cycles_do_not_request_again() {
        let client = Bluesky::new(Duration::from_secs(1))
            .unwrap()
            .with_selection(Selection {
                quoted: true,
                ..Selection::default()
            });
        let mut quoted = post("leaf", Value::Null);
        for index in 0..=MAX_QUOTES {
            quoted = quote_post(&format!("chain{index}"), &quoted, true);
        }
        let mut state = SelectedState::new(quoted);
        assert!(
            client
                .next_selected(&mut state)
                .await
                .unwrap_err()
                .to_string()
                .contains("exceeded 32")
        );
        let same = post("same", Value::Null);
        let cycle = quote_post("same", &same, false);
        let mut state = SelectedState::new(cycle);
        assert!(client.next_selected(&mut state).await.unwrap().is_none());
    }

    #[test]
    fn quoted_view_identity_is_validated_and_blocked_quotes_are_not_fetched() {
        let quoted = post("quoted", Value::Null);
        let mut root = quote_post("root", &quoted, true);
        root["embed"]["record"]["uri"] = json!(format!("at://{DID}/app.bsky.feed.post/other"));
        assert!(quoted_post(&root).is_err());
        root = quote_post("root", &quoted, true);
        root["embed"]["record"]["author"]["did"] = json!("did:web:evil.example");
        assert!(quoted_post(&root).is_err());
        root = quote_post("root", &quoted, true);
        root["embed"]["record"]["$type"] = json!("app.bsky.embed.record#viewBlocked");
        assert!(quoted_post(&root).unwrap().is_none());
        root = quote_post("root", &quoted, false);
        root["record"]["embed"]["record"]["uri"] = json!("https://evil.example/post");
        assert!(quoted_post(&root).is_err());
    }

    #[tokio::test]
    async fn selection_switches_default_feed_and_keeps_repost_author_and_video_identity() {
        let embed = json!({"$type": "app.bsky.embed.video", "video": {"ref": {"$link": CID3}, "mimeType": "video/mp4"}});
        let mut repost = post("repost", embed.clone());
        let other = "did:web:other.example";
        repost["uri"] = json!(format!("at://{other}/app.bsky.feed.post/repost"));
        repost["author"]["did"] = json!(other);
        let body = json!({"feed": [{"post": repost.clone(), "reason": {"$type": "app.bsky.feed.defs#reasonRepost"}}]});
        assert!(
            parse_feed(body.to_string().as_bytes(), DID)
                .unwrap()
                .posts
                .is_empty()
        );
        assert!(
            parse_post_view_selected(
                &repost,
                Selection {
                    videos: false,
                    ..Selection::default()
                }
            )
            .unwrap()
            .is_empty()
        );
        let (root, server) = serve(vec![response("200 OK", "", body.to_string().as_bytes())]);
        let result = bluesky(root)
            .with_selection(Selection {
                reposts: true,
                ..Selection::default()
            })
            .stream(&Url::parse(&format!("https://bsky.app/profile/{DID}")).unwrap())
            .collect::<Vec<_>>()
            .await;
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0].as_ref().unwrap().archive_key(),
            parse_post_view(&repost).unwrap()[0].archive_key()
        );
        assert!(server.join().unwrap()[0].contains("filter=posts_and_author_threads"));
    }

    #[tokio::test]
    async fn terminal_errors_are_not_retried() {
        let (root, server) = serve(vec![response("400 Bad Request", "", b"{}")]);
        let error = bluesky(root)
            .with_retries(3)
            .stream(&Url::parse("https://bsky.app/profile/nobody.example/post/3kabc").unwrap())
            .collect::<Vec<_>>()
            .await
            .remove(0)
            .unwrap_err();
        assert!(error.to_string().contains("not found"));
        assert!(!error.to_string().contains("127.0.0.1"));
        assert_eq!(server.join().unwrap().len(), 1);
    }

    const DID2: &str = "did:plc:bcdefghijklmnopqrstuvwxy";

    #[test]
    fn classifies_follows_search_hashtag_and_mirror_hosts() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://bsky.app/profile/Alice.bsky.social/follows").unwrap(),
            Target::Following {
                actor: "alice.bsky.social".into()
            }
        );
        for (raw, query, sort) in [
            ("https://bsky.app/search?q=cats+dogs", "cats dogs", None),
            ("https://bsky.app/search?q=a%26b&x=1", "a&b", None),
            ("https://bsky.app/search/cats+%23dogs", "cats #dogs", None),
            ("https://bsky.app/hashtag/art", "#art", None),
            ("https://bsky.app/hashtag/art/top", "#art", Some("top")),
            (
                "https://bsky.app/hashtag/%E7%8C%AB/latest/",
                "#\u{732b}",
                Some("latest"),
            ),
        ] {
            assert_eq!(
                t(raw).unwrap(),
                Target::Search {
                    query: query.into(),
                    sort: sort.map(str::to_owned)
                },
                "{raw}"
            );
        }
        for raw in [
            "https://bsky.app/search?q=",
            "https://bsky.app/search?q=a&q=b",
            "https://bsky.app/search/%ZZ",
            "https://bsky.app/search/%FF",
            "https://bsky.app/hashtag/art/oldest",
            "https://bsky.app/hashtag/%23art",
            "https://bsky.app/hashtag/a+b",
            "https://bsky.app/profile/alice/follows",
        ] {
            assert!(t(raw).is_err(), "{raw}");
        }
        let long = format!("https://bsky.app/search?q={}", "a".repeat(MAX_QUERY + 1));
        assert!(t(&long).is_err());
        assert_eq!(t("https://bsky.app/saved").unwrap(), Target::Bookmarks);
        for host in [
            "bsky.app",
            "www.bsky.app",
            "main.bsky.dev",
            "fxbsky.app",
            "vxbsky.app",
            "cbsky.app",
            "bskye.app",
            "bskyx.app",
            "www.fxbskye.app",
            "bsyy.app",
        ] {
            assert!(is_host(host), "{host}");
            let url = Url::parse(&format!("https://{host}/profile/{DID}/post/3kabc")).unwrap();
            assert!(target(&url).is_ok(), "{host}");
            assert_eq!(crate::extract::social_site(&url), Some("bluesky"), "{host}");
        }
        for host in [
            "bsky.social",
            "xbsky.app",
            "fxbsky.app.evil.test",
            "bskyee.app",
            "c.app",
            "evilbsky.app",
            "api.bsky.dev",
        ] {
            assert!(!is_host(host), "{host}");
        }
    }

    fn search_body(posts: Vec<Value>, cursor: Option<&str>) -> Vec<u8> {
        response(
            "200 OK",
            "",
            json!({"posts": posts, "cursor": cursor})
                .to_string()
                .as_bytes(),
        )
    }

    fn foreign_post(rkey: &str, cid: &str) -> Value {
        let mut value = post(
            rkey,
            json!({"$type": "app.bsky.embed.images", "images": [image(cid, "")]}),
        );
        value["uri"] = json!(format!("at://{DID2}/app.bsky.feed.post/{rkey}"));
        value["author"] = json!({"did": DID2, "handle": "bob.bsky.social"});
        value
    }

    #[tokio::test]
    async fn search_pages_bare_post_views_without_resolving_or_author_filtering() {
        let embed = json!({"$type": "app.bsky.embed.images", "images": [image(CID1, "")]});
        let (root, server) = serve(vec![
            search_body(
                vec![
                    post("3ka", embed),
                    json!("not a post"),
                    foreign_post("3kb", CID2),
                ],
                Some("c2"),
            ),
            search_body(vec![post("3kc", json!(null))], Some("c2")),
        ]);
        let url = Url::parse("https://bsky.app/hashtag/art/latest").unwrap();
        let media = bluesky(root)
            .stream(&url)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            media
                .iter()
                .map(|m| m.metadata.as_ref().unwrap().post_id.as_str())
                .collect::<Vec<_>>(),
            ["3ka", "3kb"]
        );
        assert_eq!(
            media[1].metadata.as_ref().unwrap().username.as_deref(),
            Some("bob.bsky.social")
        );
        let requests = server.join().unwrap();
        // A repeated cursor ends pagination after the second page.
        assert_eq!(requests.len(), 2);
        assert!(requests[0].starts_with(
            "GET /xrpc/app.bsky.feed.searchPosts?q=%23art&sort=latest&limit=100 HTTP/1.1"
        ));
        assert!(requests[1].contains("&cursor=c2"));
    }

    #[tokio::test]
    async fn plain_search_omits_sort_and_rejects_malformed_responses() {
        let (root, server) = serve(vec![search_body(vec![], None)]);
        let url = Url::parse("https://bsky.app/search?q=cats+dogs").unwrap();
        assert!(bluesky(root).events(&url).next().await.is_none());
        let requests = server.join().unwrap();
        assert!(
            requests[0]
                .starts_with("GET /xrpc/app.bsky.feed.searchPosts?q=cats+dogs&limit=100 HTTP/1.1")
        );
        let (root, server) = serve(vec![response(
            "200 OK",
            "",
            json!({"feed": []}).to_string().as_bytes(),
        )]);
        let error = bluesky(root)
            .events(&url)
            .next()
            .await
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("missing its feed"));
        server.join().unwrap();
    }

    fn follows_body(users: Vec<Value>, cursor: Option<&str>) -> Vec<u8> {
        response(
            "200 OK",
            "",
            json!({"subject": {"did": DID}, "follows": users, "cursor": cursor})
                .to_string()
                .as_bytes(),
        )
    }

    #[tokio::test]
    async fn following_queues_each_account_as_a_lazy_user_dispatcher_child() {
        let resolved = json!({"did": DID}).to_string();
        let (root, server) = serve(vec![
            response("200 OK", "", resolved.as_bytes()),
            follows_body(
                vec![
                    json!({"did": DID2, "handle": "bob.bsky.social", "displayName": "Bob",
                    "avatar": "https://cdn.bsky.app/secret", "viewer": {"x": 1}}),
                ],
                Some("next"),
            ),
            // First child: Bob's default media feed, fetched before the next page.
            response(
                "200 OK",
                "",
                json!({"feed": [{"post": foreign_post("3kb", CID2)}]})
                    .to_string()
                    .as_bytes(),
            ),
            follows_body(
                vec![json!({"did": DID, "handle": "alice.bsky.social"})],
                None,
            ),
            response("200 OK", "", feed_body("3ka", CID1).as_bytes()),
        ]);
        let url = Url::parse("https://bsky.app/profile/alice.bsky.social/follows").unwrap();
        let events = bluesky(root)
            .events(&url)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        let children: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                ExtractionEvent::BeginChild(scope) => Some(&scope.metadata),
                _ => None,
            })
            .collect();
        // Each follow is a 'following' child that re-enters the 'user' dispatcher.
        assert_eq!(
            children
                .iter()
                .map(|m| (
                    m.subcategory(),
                    m.fields().get("did").and_then(Value::as_str)
                ))
                .collect::<Vec<_>>(),
            [
                ("following", Some(DID2)),
                ("user", None),
                ("following", Some(DID)),
                ("user", None),
            ]
        );
        let bob = children[0].fields();
        assert_eq!(bob["handle"], "bob.bsky.social");
        assert_eq!(bob["displayName"], "Bob");
        assert!(!bob.contains_key("avatar") && !bob.contains_key("viewer"));
        let posts: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                ExtractionEvent::Media(media) => {
                    Some(media.metadata.as_ref().unwrap().post_id.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(posts, ["3kb", "3ka"]);
        let requests = server.join().unwrap();
        assert!(requests[0].contains("resolveHandle?handle=alice.bsky.social"));
        assert!(requests[1].starts_with(&format!(
            "GET /xrpc/app.bsky.graph.getFollows?actor={}&limit=100 HTTP/1.1",
            DID.replace(':', "%3A")
        )));
        assert!(requests[2].contains(&format!("actor={}", DID2.replace(':', "%3A"))));
        assert!(requests[2].contains("filter=posts_with_media"));
        assert!(requests[3].contains("getFollows") && requests[3].contains("cursor=next"));
    }

    #[tokio::test]
    async fn following_rejects_invalid_dids_and_missing_lists_before_queueing() {
        for body in [
            follows_body(
                vec![json!({"did": "did:plc:../../x", "handle": "x.test"})],
                None,
            ),
            response("200 OK", "", json!({"cursor": null}).to_string().as_bytes()),
        ] {
            let (root, server) = serve(vec![body]);
            let url = Url::parse(&format!("https://bsky.app/profile/{DID}/follows")).unwrap();
            let events = bluesky(root).events(&url).collect::<Vec<_>>().await;
            assert_eq!(events.len(), 1);
            assert!(events[0].is_err());
            server.join().unwrap();
        }
    }

    fn extras(events: &[Result<ExtractionEvent>]) -> Vec<serde_json::Map<String, Value>> {
        events
            .iter()
            .filter_map(|event| match event.as_ref().unwrap() {
                ExtractionEvent::Post(post) => post.metadata.as_ref(),
                ExtractionEvent::Media(media) => media.metadata.as_ref(),
                _ => None,
            })
            .map(|metadata| {
                metadata
                    .extra_keywords
                    .as_deref()
                    .cloned()
                    .unwrap_or_default()
            })
            .collect()
    }

    fn metadata_options(value: Value) -> serde_json::Map<String, Value> {
        serde_json::Map::from_iter([("metadata".into(), value)])
    }

    #[tokio::test]
    async fn metadata_option_adds_actor_profile_and_facets_without_changing_naming() {
        let embed = json!({"$type": "app.bsky.embed.images", "images": [image(CID1, "")]});
        let mut tagged = post("3ka", embed.clone());
        tagged["record"]["facets"] = json!([
            {"features": [{"$type": "app.bsky.richtext.facet#tag", "tag": "art"}]},
            {"features": [{"$type": "app.bsky.richtext.facet#mention", "did": "did:plc:bob"}]},
            {"features": [{"$type": "app.bsky.richtext.facet#link", "uri": "https://a.test/"}]},
            {"features": []}]);
        let body = json!({"feed": [{"post": tagged}, {"post": post("3kb", embed)}]});
        let profile = json!({"did": DID, "handle": "alice.example.co.uk", "followersCount": 3});
        let (root, server) = serve(vec![
            response("200 OK", "", body.to_string().as_bytes()),
            response("200 OK", "", profile.to_string().as_bytes()),
        ]);
        let url = Url::parse(&format!("https://bsky.app/profile/{DID}/media")).unwrap();
        let options = metadata_options(json!("user, facets"));
        let client = bluesky(root.clone()).with_options(Some(&options)).unwrap();
        let events: Vec<_> = client.events(&url).collect().await;
        let extras = extras(&events);
        assert_eq!(extras.len(), 4, "{events:?}");
        let mut user = profile.clone();
        user["instance"] = json!("co.uk");
        assert!(extras.iter().all(|extras| extras["user"] == user));
        assert_eq!(extras[0]["hashtags"], json!(["art"]));
        assert_eq!(extras[0]["mentions"], json!(["did:plc:bob"]));
        assert_eq!(extras[0]["uris"], json!(["https://a.test/"]));
        assert_eq!(extras[3]["hashtags"], json!([]));
        let ExtractionEvent::Media(media) = events[1].as_ref().unwrap() else {
            panic!("expected media");
        };
        assert_eq!(media.filename, "3ka_1.jpg");
        let values = crate::filter::context(media);
        assert!(values.contains_key("user") && values.contains_key("hashtags"));
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2, "the profile is fetched once");
        assert!(requests[1].starts_with(&format!(
            "GET /xrpc/app.bsky.actor.getProfile?actor={}",
            DID.replace(':', "%3A")
        )));
    }

    #[tokio::test]
    async fn metadata_user_falls_back_to_post_authors_without_an_actor() {
        let embed = json!({"$type": "app.bsky.embed.images", "images": [image(CID1, "")]});
        let (root, server) = serve(vec![search_body(vec![post("3ka", embed)], None)]);
        let options = metadata_options(json!(["user"]));
        let events: Vec<_> = bluesky(root)
            .with_options(Some(&options))
            .unwrap()
            .events(&Url::parse("https://bsky.app/search?q=cat").unwrap())
            .collect()
            .await;
        let extras = extras(&events);
        assert_eq!(extras.len(), 2, "{events:?}");
        assert_eq!(
            extras[1]["user"],
            json!({"did": DID, "handle": "alice.bsky.social", "instance": "bsky.social"})
        );
        assert!(!extras[1].contains_key("hashtags"));
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[test]
    fn metadata_selection_follows_upstream_truthiness_and_compat_keywords() {
        for (value, user, facets) in [
            (json!(true), true, true),
            (json!(1), true, true),
            (json!("facets"), false, true),
            (json!(["user"]), true, false),
            (json!(false), false, false),
            (json!(""), false, false),
            (Value::Null, false, false),
        ] {
            let client = Bluesky::new(Duration::from_secs(1))
                .unwrap()
                .with_options(Some(&metadata_options(value.clone())))
                .unwrap();
            assert_eq!(
                (client.metadata_user, client.metadata_facets),
                (user, facets),
                "{value}"
            );
        }
        assert_eq!(handle_instance("bsky.app"), "bsky.app");
        assert_eq!(handle_instance("a.b.c.d"), "c.d");
        let data = json!({"user": {"did": DID}, "uris": [], "text": "x"});
        let extras = extras_from_keywords(data.as_object().unwrap()).unwrap();
        assert_eq!(extras.keys().collect::<Vec<_>>(), ["user", "uris"]);
        assert!(extras_from_keywords(&serde_json::Map::new()).is_none());
    }
}
