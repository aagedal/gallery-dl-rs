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
        if crate::sites::is_category(&self.category) {
            let (namespace, identity) = crate::sites::archive_key(self)?;
            return Some(format!(
                "{namespace}:v1:{:x}",
                Sha256::digest(identity.as_bytes())
            ));
        }
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
            "mastodon" | "mastodon.social" | "pawoo" | "baraag" => {
                let post = self.metadata.as_ref()?;
                let instance = post.instance.as_deref()?;
                if !crate::mastodon::INSTANCES.contains(&instance)
                    || (self.category != "mastodon"
                        && crate::mastodon::instance_category(instance)
                            != Some(self.category.as_str()))
                    || !crate::mastodon::valid_media_id(&post.media_id)
                {
                    return None;
                }
                (
                    "mastodon",
                    serde_json::json!([instance, post.media_id]).to_string(),
                )
            }
            // Discord message IDs are scoped to a server, while source numbering
            // preserves attachment/inline order even when thumbnails skip files.
            category @ ("kemono" | "coomer")
                if self
                    .metadata
                    .as_ref()
                    .is_some_and(|post| post.discord.is_some()) =>
            {
                let post = self.metadata.as_ref()?;
                let discord = post.discord.as_ref()?;
                let host = self.url.host_str()?;
                let site_host = crate::kemono::category(host) == Some(category)
                    && self.url.path().starts_with("/data/");
                let thumbnail = host.strip_prefix("img.").is_some_and(|host| {
                    crate::kemono::HOSTS.contains(&(host, category))
                        && self.url.path().starts_with("/thumbnail/data/")
                });
                let inline = matches!(
                    post.file
                        .as_ref()
                        .and_then(|file| file.media_type.as_deref()),
                    Some("inline" | "archive")
                ) && post.media_id.is_empty();
                let discord_host =
                    inline && matches!(host, "cdn.discordapp.com" | "media.discordapp.net");
                if self.url.scheme() != "https"
                    || self.url.port().is_some()
                    || !self.url.username().is_empty()
                    || self.url.password().is_some()
                    || !(site_host || thumbnail || discord_host)
                    || !crate::kemono::valid_discord_id(&discord.server_id)
                    || !crate::kemono::valid_discord_id(&discord.channel_id)
                    || !crate::kemono::valid_discord_id(&post.post_id)
                    || post.index == 0
                    || !(crate::kemono::valid_hash(&post.media_id)
                        || inline && post.media_id.is_empty())
                {
                    return None;
                }
                (
                    category,
                    serde_json::json!(["discord", discord.server_id, post.post_id, post.index])
                        .to_string(),
                )
            }
            // Kemono/Coomer source positions preserve explicitly retained
            // duplicates and hashless inline files, as upstream does.
            category @ ("kemono" | "coomer") => {
                let post = self.metadata.as_ref()?;
                let service = post.instance.as_deref()?;
                let user = match &post.creator {
                    Some(creator) if creator.service == service => creator.id.as_str(),
                    Some(_) => return None,
                    None => post.username.as_deref()?,
                };
                let host = self.url.host_str()?;
                let thumbnail = host.strip_prefix("img.").is_some_and(|domain| {
                    crate::kemono::HOSTS.contains(&(domain, category))
                        && self.url.scheme() == "https"
                        && self.url.port().is_none()
                        && self.url.username().is_empty()
                        && self.url.password().is_none()
                        && self.url.path().starts_with("/thumbnail/data/")
                });
                if (crate::kemono::category(host) != Some(category) && !thumbnail)
                    || !crate::kemono::valid_service(service)
                    || !crate::kemono::valid_id(user)
                    || !crate::kemono::valid_id(&post.post_id)
                    || post.index == 0
                    || !(crate::kemono::valid_hash(&post.media_id)
                        || post.media_id.is_empty()
                            && match post
                                .file
                                .as_ref()
                                .and_then(|file| file.media_type.as_deref())
                            {
                                Some("inline") => true,
                                Some("archive") => self.url.path().starts_with("/data/inline/"),
                                _ => false,
                            })
                {
                    return None;
                }
                (
                    category,
                    match &post.revision {
                        Some(revision) => match revision.id? {
                            0 => serde_json::json!([service, user, post.post_id, post.index]),
                            revision_id => serde_json::json!([
                                service,
                                user,
                                post.post_id,
                                post.index,
                                "revision",
                                revision_id
                            ]),
                        },
                        None => serde_json::json!([service, user, post.post_id, post.index]),
                    }
                    .to_string(),
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
            // VSCO files are identified by media ID.
            "vsco" => {
                let post = self.metadata.as_ref()?;
                if !crate::vsco::valid_id(&post.media_id) {
                    return None;
                }
                ("vsco", serde_json::json!([post.media_id]).to_string())
            }
            // Weasyl files are identified by submission ID (journals by `jID`).
            "weasyl" => {
                let post = self.metadata.as_ref()?;
                if !crate::weasyl::valid_media_id(&post.media_id) {
                    return None;
                }
                ("weasyl", serde_json::json!([post.media_id]).to_string())
            }
            // Fur Affinity files are identified by submission ID.
            "furaffinity" => {
                let post = self.metadata.as_ref()?;
                if !crate::furaffinity::valid_id(&post.media_id) {
                    return None;
                }
                (
                    "furaffinity",
                    serde_json::json!([post.media_id]).to_string(),
                )
            }
            // Newgrounds files are identified by media ID.
            "newgrounds" => {
                let post = self.metadata.as_ref()?;
                if !crate::newgrounds::valid_id(&post.media_id) {
                    return None;
                }
                ("newgrounds", serde_json::json!([post.media_id]).to_string())
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
                let extension = self
                    .filename
                    .rsplit_once('.')
                    .map(|(_, extension)| extension)
                    .filter(|extension| valid_extension(extension));
                if post.bookmark_archive || post.user_bookmark.is_some() {
                    let owner = post.user_bookmark.as_ref()?;
                    let extension = self.filename.rsplit_once('.')?.1;
                    if !owner.valid()
                        || !valid_extension(extension)
                        || post.sketch_archive
                        || post.sketch.is_some()
                        || post.pixivision_archive
                        || post.pixivision.is_some()
                        || post.followed_archive
                        || post.user_follow.is_some()
                        || post.search_archive
                        || post.search.is_some()
                        || post.ranking_archive
                        || post.ranking.is_some()
                        || post.profile_asset.is_some()
                        || post.series.is_some()
                        || post.num_series.is_some()
                        || !crate::pixiv::valid_media_id(&post.post_id, &post.media_id)
                    {
                        return None;
                    }
                    (
                        "pixiv",
                        serde_json::json!([
                            "bookmarks",
                            owner.id,
                            post.post_id,
                            post.media_id,
                            extension
                        ])
                        .to_string(),
                    )
                } else if post.sketch_archive || post.sketch.is_some() {
                    let sketch = post.sketch.as_ref()?;
                    if !sketch.valid()
                        || sketch
                            .fields()
                            .get("post_id")
                            .and_then(crate::pixiv::sketch::id_from_value)
                            .as_deref()
                            != Some(post.post_id.as_str())
                        || post.pixivision_archive
                        || post.pixivision.is_some()
                        || post.followed_archive
                        || post.user_follow.is_some()
                        || post.search_archive
                        || post.search.is_some()
                        || post.ranking_archive
                        || post.ranking.is_some()
                        || post.profile_asset.is_some()
                        || post.series.is_some()
                        || post.num_series.is_some()
                        || !crate::pixiv::sketch::valid_id(&post.post_id)
                        || !crate::pixiv::sketch::valid_id(&post.media_id)
                    {
                        return None;
                    }
                    (
                        "pixiv",
                        serde_json::json!(["sketch", sketch.owner_id()?, post.media_id])
                            .to_string(),
                    )
                } else if post.pixivision_archive || post.pixivision.is_some() {
                    let article = post.pixivision.as_ref()?;
                    if !article.valid()
                        || post.followed_archive
                        || post.user_follow.is_some()
                        || post.search_archive
                        || post.search.is_some()
                        || post.ranking_archive
                        || post.ranking.is_some()
                        || post.profile_asset.is_some()
                        || !crate::pixiv::valid_media_id(&post.post_id, &post.media_id)
                    {
                        return None;
                    }
                    (
                        "pixiv",
                        serde_json::json!([
                            "pixivision",
                            article.id,
                            post.post_id,
                            post.media_id,
                            extension?
                        ])
                        .to_string(),
                    )
                } else if post.followed_archive || post.user_follow.is_some() {
                    let owner = post.user_follow.as_ref()?;
                    if !owner.valid()
                        || post.search_archive
                        || post.search.is_some()
                        || post.ranking_archive
                        || post.ranking.is_some()
                        || post.profile_asset.is_some()
                        || !crate::pixiv::valid_media_id(&post.post_id, &post.media_id)
                    {
                        return None;
                    }
                    (
                        "pixiv",
                        serde_json::json!([
                            "followed",
                            owner.id,
                            post.post_id,
                            post.media_id,
                            extension?
                        ])
                        .to_string(),
                    )
                } else if post.search_archive || post.search.is_some() {
                    let search = post.search.as_ref()?;
                    if !search.valid()
                        || post.ranking_archive
                        || post.ranking.is_some()
                        || post.profile_asset.is_some()
                        || !crate::pixiv::valid_media_id(&post.post_id, &post.media_id)
                    {
                        return None;
                    }
                    (
                        "pixiv",
                        serde_json::json!([
                            "search",
                            search.word,
                            post.post_id,
                            post.media_id,
                            extension?
                        ])
                        .to_string(),
                    )
                } else if post.ranking_archive || post.ranking.is_some() {
                    let ranking = post.ranking.as_ref()?;
                    if !ranking.valid()
                        || post.profile_asset.is_some()
                        || !crate::pixiv::valid_media_id(&post.post_id, &post.media_id)
                    {
                        return None;
                    }
                    (
                        "pixiv",
                        serde_json::json!([
                            "ranking",
                            ranking.mode,
                            ranking.date,
                            post.post_id,
                            post.media_id,
                            extension?
                        ])
                        .to_string(),
                    )
                } else if let Some(asset) = &post.profile_asset {
                    if !crate::pixiv::valid_id(&asset.owner_id) || !asset.valid_version() {
                        return None;
                    }
                    (
                        "pixiv",
                        serde_json::json!(["profile", asset.kind, asset.owner_id, asset.version])
                            .to_string(),
                    )
                } else {
                    if !crate::pixiv::valid_media_id(&post.post_id, &post.media_id) {
                        return None;
                    }
                    (
                        "pixiv",
                        serde_json::json!([post.post_id, post.media_id, extension?]).to_string(),
                    )
                }
            }
            "pixiv-novel" => {
                let post = self.metadata.as_ref()?;
                let extension = self
                    .filename
                    .rsplit_once('.')
                    .map(|(_, extension)| extension)
                    .filter(|extension| valid_extension(extension));
                if !crate::pixiv::valid_media_id(&post.post_id, &post.media_id) {
                    return None;
                }
                (
                    "pixiv-novel",
                    serde_json::json!([post.post_id, post.media_id, extension?]).to_string(),
                )
            }
            category @ ("twitter" | "instagram")
                if self
                    .metadata
                    .as_ref()
                    .is_some_and(|post| post.profile_asset.is_some()) =>
            {
                let asset = self.metadata.as_ref()?.profile_asset.as_ref()?;
                let valid_owner = if category == "twitter" {
                    crate::twitter::valid_id(&asset.owner_id)
                } else {
                    decimal_id(&asset.owner_id)
                };
                if !valid_owner || !asset.valid_version() {
                    return None;
                }
                (
                    category,
                    serde_json::json!(["profile", asset.kind, asset.owner_id, asset.version])
                        .to_string(),
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
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub pixiv_novel_metadata: Option<std::sync::Arc<crate::pixiv::NovelMetadata>>,
    /// Pixiv artwork marked for browser AJAX recovery, including disabled attempts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ajax: Option<bool>,
    /// Pixiv artwork classified by upstream as My Pixiv or an unknown restriction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mypixiv: Option<bool>,
    /// Original Pixiv caption HTML, including source AJAX URL/entity decoding.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caption_html: Option<std::sync::Arc<str>>,
    /// Complete Pixiv artist detail fields, flattened into the public metadata.
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub pixiv_user_detail: Option<std::sync::Arc<crate::pixiv::UserDetail>>,
    /// Optional complete Pixiv illustration or novel comment objects, in API order.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comments: Option<std::sync::Arc<[Value]>>,
    /// Registered bookmark tag names, retaining source order and duplicates.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags_bookmark: Option<std::sync::Arc<[String]>>,
    /// Whether ugoira output consists of original frames rather than a ZIP.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ugoira_original: Option<bool>,
    /// Zero-based original ugoira frame number, absent on post boundaries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ugoira_frame_index: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_bookmark: Option<std::sync::Arc<crate::pixiv::FollowedUserMetadata>>,
    /// Preserve a declared bookmark scope if its owner metadata is invalid.
    #[serde(skip_serializing)]
    pub bookmark_archive: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sketch: Option<std::sync::Arc<crate::pixiv::SketchMetadata>>,
    /// Retain declared Sketch scope when its owner metadata is invalid.
    #[serde(skip_serializing)]
    pub sketch_archive: bool,
    /// Pixiv tag names or original App API tag objects, in source order.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<crate::pixiv::Tags>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pixivision: Option<std::sync::Arc<crate::pixiv::PixivisionMetadata>>,
    #[serde(skip_serializing)]
    pub pixivision_archive: bool,
    /// Initial OAuth owner for Pixiv's followed-artists feed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_follow: Option<std::sync::Arc<crate::pixiv::FollowedUserMetadata>>,
    /// Preserve declared feed scope if its public owner metadata is invalid.
    #[serde(skip_serializing)]
    pub followed_archive: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub series: Option<std::sync::Arc<crate::pixiv::SeriesMetadata>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub num_series: Option<i64>,
    /// Pixiv search context; only the search term scopes archive identity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search: Option<std::sync::Arc<crate::pixiv::SearchMetadata>>,
    /// Retain a declared search scope even if its public metadata is invalid.
    #[serde(skip_serializing)]
    pub search_archive: bool,
    /// Pixiv ranking context; ranks do not participate in archive identity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ranking: Option<std::sync::Arc<crate::pixiv::RankingMetadata>>,
    /// Retain a declared ranking scope even if its public metadata is invalid.
    #[serde(skip_serializing)]
    pub ranking_archive: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_asset: Option<crate::profile_asset::ProfileAssetMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub discord: Option<crate::kemono::DiscordMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub creator: Option<crate::kemono::CreatorMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<crate::kemono::RevisionMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<crate::file_metadata::FileMetadata>,
    /// Ordered Pixiv ugoira ZIP member names and millisecond frame delays.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frames: Option<std::sync::Arc<[crate::pixiv::UgoiraFrame]>>,
    /// Site-declared post rating and score (currently native boorus).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rating: Option<std::sync::Arc<str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<i64>,
    /// Exact booru constructor search text, separate from API wire sentinels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search_tags: Option<std::sync::Arc<str>>,
    /// Booru pool response without member IDs, or the scalar DAPI pool keyword.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pool: Option<std::sync::Arc<Value>>,
    /// Complete Danbooru favorite-group response without its member ID list.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub favgroup: Option<std::sync::Arc<Value>>,
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
    /// Producer counter, which can be zero even though the native file index is one-based.
    #[serde(skip_serializing)]
    pub source_num: Option<Value>,
    /// Exact booru filename, extension and root MD5 keywords, including absence/null.
    #[serde(skip_serializing)]
    pub booru_keywords: Option<std::sync::Arc<serde_json::Map<String, Value>>>,
    /// Exact favorite owner keyword, independent of API authentication/identity.
    #[serde(skip_serializing)]
    pub booru_user_id: Option<Value>,
    /// Exact scalar DAPI pool display name, separate from nested pool objects.
    #[serde(skip_serializing)]
    pub booru_pool_name: Option<Value>,
    /// Exact scalar DAPI favorites owner keyword.
    #[serde(skip_serializing)]
    pub booru_favorite_id: Option<Value>,
    /// Shared shallow artist queue copy inherited by a tag child.
    #[serde(skip_serializing)]
    pub booru_artist_parent: Option<crate::event::ArtistInheritance>,
    /// Popular view keyword overrides, separate from canonical post creation date.
    #[serde(skip_serializing)]
    pub booru_popular: Option<std::sync::Arc<serde_json::Map<String, Value>>>,
    /// Immutable source class path defaults, separate from live keywords.
    #[serde(skip_serializing)]
    pub source_defaults: Option<std::sync::Arc<crate::naming::SourceDefaults>>,
    /// Exact producer suffix, retained independently of the native filename.
    #[serde(skip_serializing)]
    pub source_suffix: Option<Value>,
    /// Ordered producer writes, separate from the canonical emitted snapshot.
    #[serde(skip_serializing)]
    pub keyword_updates: Option<std::sync::Arc<[crate::event::KeywordUpdate]>>,
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
    /// Upstream gallery-dl keyword dictionary for registry sites.
    #[serde(rename = "keywords", skip_serializing_if = "Option::is_none")]
    pub site_keywords: Option<std::sync::Arc<crate::sites::SiteKeywords>>,
}

#[derive(Clone, Debug)]
pub struct Backend {
    pub executable: PathBuf,
    pub cookies: Option<PathBuf>,
    pub config: Option<PathBuf>,
    pub timeout: Duration,
}

/// A command-line input: an HTTP(S) URL, or one behind upstream's
/// `g:`/`generic:` (case-insensitive) or `r:`/`recursive:` prefix, kept as a
/// `generic:` or `recursive:` URL around the HTTP(S) target. A target
/// without a scheme gets `https://`, as upstream does. Local `file://`
/// sources for `recursive:` are not supported; `--input-file` reads them.
pub fn input_url(value: &str) -> Result<Url> {
    let prefixed = |value: &str| -> Option<(&'static str, String)> {
        let (prefix, rest) = value.split_once(':')?;
        let scheme = if prefix.eq_ignore_ascii_case("g") || prefix.eq_ignore_ascii_case("generic") {
            "generic"
        } else if matches!(prefix, "r" | "recursive") {
            "recursive"
        } else {
            return None;
        };
        Some((scheme, rest.to_owned()))
    };
    let Some((scheme, target)) = prefixed(value) else {
        return http_url(value);
    };
    if scheme == "recursive" && target.starts_with("file:") {
        return Err(error(
            "recursive: file sources are not supported; use --input-file",
        ));
    }
    let lower = target.to_ascii_lowercase();
    let target = if lower.starts_with("http://") || lower.starts_with("https://") {
        target
    } else {
        format!("https://{target}")
    };
    let target = http_url(&target)?;
    Url::parse(&format!("{scheme}:{target}")).map_err(|_| error("invalid prefixed URL"))
}

/// The HTTP(S) URL inside a `generic:` or `recursive:` input URL.
pub fn prefixed_target(url: &Url) -> Option<Url> {
    let inner = url.as_str().split_once(':')?.1;
    http_url(inner).ok()
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
    // Registry sites include the host-less `generic:`/`recursive:` inputs.
    if let Some(site) = crate::sites::find(url) {
        return Some(site.category);
    }
    let host = url.host_str()?;
    if crate::mastodon::INSTANCES.contains(&host) {
        return Some("mastodon");
    }
    if let Some(category) = crate::kemono::category(host) {
        return Some(category);
    }
    if crate::vsco::is_vsco_url(url) {
        return Some("vsco");
    }
    if crate::weasyl::is_weasyl_url(url) {
        return Some("weasyl");
    }
    if crate::furaffinity::is_furaffinity_url(url) {
        return Some("furaffinity");
    }
    if crate::newgrounds::is_newgrounds_url(url) {
        return Some("newgrounds");
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
    if let Some(category) = crate::pixiv::category(url) {
        return Some(category);
    }
    if crate::tumblr::is_blog_url(url) {
        return Some("tumblr");
    }
    if crate::twitter::is_image_url(url) {
        return Some("twitter");
    }
    if let Some(site) = crate::imagehosts::Site::from_host(host) {
        return Some(site.category());
    }
    if crate::bluesky::is_host(host) {
        return Some("bluesky");
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
    /// Raw `extractor.*` KEY=VALUE settings forwarded with `--option`.
    pub options: Vec<String>,
    pub include: Option<crate::dispatch::Include>,
    pub child_range: Option<String>,
    pub child_filter: Option<Vec<String>>,
    pub browser: Option<String>,
    pub proxy: Option<String>,
    pub user_agent: Option<String>,
    pub retweets: Option<crate::twitter::RetweetMode>,
    pub quoted: Option<bool>,
    pub expand: Option<bool>,
    pub unique: Option<bool>,
    pub videos: Option<crate::twitter::VideoMode>,
    pub previews: Option<bool>,
    pub mp4: Option<bool>,
    pub reblogs: Option<bool>,
    pub tumblr_reblogs: Option<crate::tumblr::Reblogs>,
    pub posts: Option<Vec<String>>,
    pub inline: Option<crate::tumblr::Inline>,
    pub original: Option<bool>,
    pub files: Option<Vec<crate::kemono::FileKind>>,
    pub duplicates: Option<crate::kemono::DuplicateOptions>,
    pub revisions: Option<crate::kemono::RevisionMode>,
    pub order_revisions: Option<crate::kemono::RevisionOrder>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub user_id: Option<String>,
    pub api_key: Option<String>,
    pub reposts: Option<bool>,
    pub sanity: Option<bool>,
    pub captions: Option<bool>,
    pub comments: Option<bool>,
    pub metadata_bookmark: Option<bool>,
    pub ugoira: Option<crate::pixiv::UgoiraMode>,
    pub tags: Option<crate::pixiv::TagMode>,
    pub cursor: Option<crate::pixiv::BookmarkCursor>,
    pub embeds: Option<bool>,
    pub covers: Option<bool>,
    pub full_series: Option<bool>,
    pub access_token: Option<String>,
    pub text_posts: Option<bool>,
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
    pub max_posts: Option<usize>,
    pub booru_threshold: Option<Option<usize>>,
    pub metadata: Option<bool>,
    pub endpoint: Option<crate::kemono::Endpoint>,
    pub bluesky_endpoint: Option<crate::bluesky::LikesEndpoint>,
    pub api_server: Option<String>,
    pub order_posts: Option<crate::kemono::PostOrder>,
    pub booru_order_posts: Option<crate::booru::PoolOrder>,
    pub booru_favorite_order: Option<Value>,
    pub parent_metadata: Option<Value>,
    pub metadata_parent: Option<Value>,
    pub favorites: Option<crate::kemono::Favorites>,
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
        for option in &options.options {
            command.arg("--option").arg(option);
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
        if let Some(range) = &options.child_range {
            command
                .arg("--option")
                .arg(format!("child-range={}", serde_json::json!(range)));
        }
        if let Some(include) = &options.include {
            command
                .arg("--option")
                .arg(format!("include={}", include.value()));
        }
        if let Some(covers) = options.covers {
            command.arg("--option").arg(format!("covers={covers}"));
        }
        if let Some(full_series) = options.full_series {
            command
                .arg("--option")
                .arg(format!("full-series={full_series}"));
        }
        if let Some(clauses) = &options.child_filter {
            // An empty clause list explicitly clears inherited backend filters.
            let value = if clauses.is_empty() {
                serde_json::json!("")
            } else {
                serde_json::json!(clauses)
            };
            command.arg("--option").arg(format!("child-filter={value}"));
        }
        if let Some(size) = &options.size {
            command
                .arg("--option")
                .arg(format!("size={}", serde_json::json!(size.sizes())));
        }
        for (name, value) in [
            ("metadata", options.metadata),
            ("embeds", options.embeds),
            ("sanity", options.sanity),
            ("captions", options.captions),
            ("comments", options.comments),
            ("metadata-bookmark", options.metadata_bookmark),
            ("mp4", options.mp4),
            ("reblogs", options.reblogs),
            ("original", options.original),
            ("reposts", options.reposts),
            ("text-posts", options.text_posts),
        ] {
            if let Some(value) = value {
                command.arg("--option").arg(format!("{name}={value}"));
            }
        }
        for (key, value) in [
            ("parent-metadata", &options.parent_metadata),
            ("metadata-parent", &options.metadata_parent),
        ] {
            if let Some(value) = value {
                command.arg("--option").arg(format!("{key}={value}"));
            }
        }
        if let Some(threshold) = options.booru_threshold {
            command
                .arg("--option")
                .arg(format!("threshold={}", serde_json::json!(threshold)));
        }
        if let Some(mode) = options.ugoira {
            command
                .arg("--option")
                .arg(format!("ugoira={}", mode.value()));
        }
        for (name, value) in [
            ("username", &options.username),
            ("password", &options.password),
            ("user-id", &options.user_id),
            ("api-key", &options.api_key),
        ] {
            if let Some(value) = value {
                command
                    .arg("--option")
                    .arg(format!("{name}={}", serde_json::json!(value)));
            }
        }
        if let Some(reblogs) = options.tumblr_reblogs {
            let value = match reblogs {
                crate::tumblr::Reblogs::All => serde_json::json!(true),
                crate::tumblr::Reblogs::OriginalOnly => serde_json::json!(false),
                crate::tumblr::Reblogs::SameBlog => serde_json::json!("same-blog"),
            };
            command.arg("--option").arg(format!("reblogs={value}"));
        }
        if let Some(inline) = options.inline {
            let value = match inline {
                crate::tumblr::Inline::All => serde_json::json!(true),
                crate::tumblr::Inline::Disabled => serde_json::json!(false),
                crate::tumblr::Inline::Reblog => serde_json::json!("reblog"),
                crate::tumblr::Inline::Original => serde_json::json!("original"),
            };
            command.arg("--option").arg(format!("inline={value}"));
        }
        if let Some(revisions) = options.revisions {
            let value = match revisions {
                crate::kemono::RevisionMode::None => serde_json::json!(false),
                crate::kemono::RevisionMode::All => serde_json::json!(true),
                crate::kemono::RevisionMode::Unique => serde_json::json!("unique"),
            };
            command.arg("--option").arg(format!("revisions={value}"));
        }
        if let Some(order) = options.order_revisions {
            let value = match order {
                crate::kemono::RevisionOrder::Newest => "desc",
                crate::kemono::RevisionOrder::Oldest => "asc",
            };
            command
                .arg("--option")
                .arg(format!("order-revisions={}", serde_json::json!(value)));
        }
        if let Some(files) = &options.files {
            let names: Vec<_> = files
                .iter()
                .map(|kind| match kind {
                    crate::kemono::FileKind::File => "file",
                    crate::kemono::FileKind::Attachments => "attachments",
                    crate::kemono::FileKind::Inline => "inline",
                })
                .collect();
            command
                .arg("--option")
                .arg(format!("files={}", serde_json::json!(names)));
        }
        if let Some(duplicates) = options.duplicates {
            let names: Vec<_> = [
                ("file", duplicates.file),
                ("attachment", duplicates.attachment),
                ("inline", duplicates.inline),
            ]
            .into_iter()
            .filter_map(|(name, enabled)| enabled.then_some(name))
            .collect();
            command
                .arg("--option")
                .arg(format!("duplicates={}", serde_json::json!(names)));
        }
        if let Some(endpoint) = options.endpoint {
            let endpoint = match endpoint {
                crate::kemono::Endpoint::Posts => "posts",
                crate::kemono::Endpoint::Expanded => "posts+",
            };
            command
                .arg("--option")
                .arg(format!("endpoint={}", serde_json::json!(endpoint)));
        }
        if let Some(endpoint) = options.bluesky_endpoint {
            let value = match endpoint {
                crate::bluesky::LikesEndpoint::Records => "listRecords",
                crate::bluesky::LikesEndpoint::ActorLikes => "getActorLikes",
            };
            command
                .arg("--option")
                .arg(format!("endpoint={}", serde_json::json!(value)));
        }
        if let Some(server) = &options.api_server {
            command
                .arg("--option")
                .arg(format!("api-server={}", serde_json::json!(server)));
        }
        if let Some(favorites) = options.favorites {
            let value = match favorites {
                crate::kemono::Favorites::Artist => "artist",
                crate::kemono::Favorites::Post => "post",
            };
            command
                .arg("--option")
                .arg(format!("favorites={}", serde_json::json!(value)));
        }
        if let Some(order) = &options.booru_favorite_order {
            command.arg("--option").arg(format!("order-posts={order}"));
        } else if let Some(order) = options.booru_order_posts {
            let order = match order {
                crate::booru::PoolOrder::Pool => "pool",
                crate::booru::PoolOrder::PoolReverse => "pool_desc",
                crate::booru::PoolOrder::IdDescending => "id",
                crate::booru::PoolOrder::IdAscending => "id_asc",
            };
            command
                .arg("--option")
                .arg(format!("order-posts={}", serde_json::json!(order)));
        } else if let Some(order) = options.order_posts {
            let order = match order {
                crate::kemono::PostOrder::Forward => "asc",
                crate::kemono::PostOrder::Reverse => "desc",
            };
            command
                .arg("--option")
                .arg(format!("order-posts={}", serde_json::json!(order)));
        }
        if let Some(posts) = &options.posts {
            let value = if posts.as_slice() == ["all"] {
                serde_json::json!("all")
            } else {
                serde_json::json!(posts)
            };
            command.arg("--option").arg(format!("posts={value}"));
        }
        if let Some(token) = &options.access_token {
            command
                .arg("--option")
                .arg(format!("access-token={}", serde_json::json!(token)));
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
        if let Some(limit) = options.max_posts {
            command.arg("--option").arg(format!("max-posts={limit}"));
        }
        if let Some(mode) = options.tags {
            command
                .arg("--option")
                .arg(format!("tags=\"{}\"", mode.as_str()));
        }
        if let Some(cursor) = &options.cursor {
            command
                .arg("--option")
                .arg(format!("cursor={}", cursor.as_json()));
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
    let category = &message_category(category, data);
    let mut metadata = compatibility_metadata(data, category);
    if let Some(metadata) = &mut metadata {
        metadata.file = None;
        metadata.ugoira_frame_index = None;
        metadata.media_id.clear();
        metadata.index = 0;
    }
    Ok(crate::event::Post {
        keywords: Default::default(),
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
    let category = &message_category(category, metadata);
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
    let mut normalized = compatibility_metadata(metadata, category);
    if category == "pixiv-novel"
        && let Some(post) = &normalized
        && crate::pixiv::valid_id(&post.post_id)
        && let Some(page) = metadata
            .get("num")
            .and_then(Value::as_u64)
            .filter(|page| *page < 100_000)
        && let Some(extension) = metadata
            .get("extension")
            .and_then(Value::as_str)
            .filter(|extension| valid_extension(extension))
    {
        filename = format!("{}_p{page}.{extension}", post.post_id);
    }
    if category == "pixiv"
        && metadata
            .get("_ugoira_original")
            .and_then(Value::as_bool)
            .is_some()
        && let Some(post) = &normalized
        && crate::pixiv::valid_id(&post.post_id)
        && let Some(page) = metadata.get("num").and_then(Value::as_u64).filter(|page| {
            *page
                < if post.ugoira_original == Some(true) {
                    100_000
                } else {
                    10_000
                }
        })
        && let Some(extension) = metadata
            .get("extension")
            .and_then(Value::as_str)
            .filter(|extension| valid_extension(extension))
    {
        filename = format!("{}_p{page}.{extension}", post.post_id);
    }
    if category == "pixiv"
        && let Some(post) = &normalized
        && post.sketch.is_some()
        && crate::pixiv::sketch::valid_id(&post.post_id)
        && crate::pixiv::sketch::valid_id(&post.media_id)
        && let Some(extension) = metadata
            .get("extension")
            .and_then(Value::as_str)
            .filter(|extension| valid_extension(extension))
    {
        filename = format!(
            "{} {}.{extension}",
            crate::download::safe_component(&post.post_id),
            crate::download::safe_component(&post.media_id)
        );
    }
    if category == "pixiv"
        && let Some(post) = &normalized
        && post.series.is_some()
        && let Some(number) = post.num_series
        && crate::pixiv::valid_id(&post.post_id)
        && let Some(page) = metadata.get("num").and_then(Value::as_u64).filter(|n| {
            *n < if metadata.get("_ugoira_original").and_then(Value::as_bool) == Some(true) {
                100_000
            } else {
                10_000
            }
        })
        && let Some(extension) = metadata
            .get("extension")
            .and_then(Value::as_str)
            .filter(|s| valid_extension(s))
    {
        // Backend messages carry the URL's filename, not the extractor's
        // formatted default name. Rebuild the series default from public IDs.
        filename = format!("{number:0>3}_{}_p{page}.{extension}", post.post_id);
    }
    if category == "twitter"
        && let Some(kind) = metadata
            .get("subcategory")
            .and_then(Value::as_str)
            .and_then(|kind| match kind {
                "avatar" => Some(crate::profile_asset::ProfileAssetKind::Avatar),
                "background" => Some(crate::profile_asset::ProfileAssetKind::Background),
                _ => None,
            })
        && let Some(post) = &mut normalized
    {
        let owner = metadata
            .get("user")
            .and_then(|user| user.get("id"))
            .and_then(|id| {
                id.as_str()
                    .map(str::to_owned)
                    .or_else(|| id.as_u64().map(|id| id.to_string()))
            });
        post.profile_asset =
            owner.and_then(|owner| crate::twitter::profile_asset_identity(kind, &owner, &url));
        if post.profile_asset.is_none() {
            // Profile rows must not acquire an ordinary tweet archive identity
            // when their public owner/version or canonical source is invalid.
            post.media_id.clear();
        }
    }
    Ok(Some(Media {
        url,
        job,
        category: category.into(),
        filename,
        metadata: normalized,
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

/// Bounded source metadata shared by native and compatibility Pixiv paths.
pub(crate) fn pixiv_comments(value: &Value) -> Option<std::sync::Arc<[Value]>> {
    let values = value.as_array().filter(|values| values.len() <= 100_000)?;
    if serde_json::to_vec(values).ok()?.len() > 32 * 1024 * 1024 {
        return None;
    }
    Some(values.clone().into())
}

pub(crate) fn pixiv_bookmark_tags(value: &Value) -> Option<std::sync::Arc<[String]>> {
    let values = value.as_array().filter(|values| values.len() <= 100_000)?;
    let mut size = 0usize;
    let tags = values
        .iter()
        .map(|value| {
            let tag = value.as_str()?;
            size = size.checked_add(tag.len())?;
            (size <= 32 * 1024 * 1024).then(|| tag.to_owned())
        })
        .collect::<Option<Vec<_>>>()?;
    Some(tags.into())
}

fn valid_extension(value: &str) -> bool {
    !value.is_empty() && value.len() <= 16 && value.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

/// Allowlist the public fields emitted by upstream gallery-dl extractors.
/// See gallery_dl/extractor/{instagram,twitter,facebook}.py: Instagram uses
/// post_id/media_id, Twitter tweet_id/num, Facebook id/extension (including
/// separate audio and video files with the same id). Missing IDs stay empty;
/// they never acquire an inferred identity from a URL, filename or row number.
/// Categories with dedicated handling in this crate; output from any other
/// gallery-dl extractor is treated generically.
const DEDICATED_CATEGORIES: &[&str] = &[
    "instagram",
    "twitter",
    "facebook",
    "imgur",
    "bluesky",
    "reddit",
    "danbooru",
    "e621",
    "e926",
    "gelbooru",
    "safebooru",
    "mastodon",
    "mastodon.social",
    "pawoo",
    "baraag",
    "kemono",
    "coomer",
    "pixeldrain",
    "catbox",
    "imgbox",
    "postimg",
    "tumblr",
    "pixiv",
    "pixiv-novel",
    "deviantart",
    "flickr",
    "wallhaven",
    "artstation",
    "pinterest",
    "newgrounds",
    "furaffinity",
    "weasyl",
    "vsco",
    "direct",
    "directlink",
];

/// Whether a category came from generic gallery-dl compatibility output.
pub(crate) fn generic_category(category: &str) -> bool {
    !DEDICATED_CATEGORIES.contains(&category) && !crate::sites::is_category(category)
}

/// The category for a compatibility message. Generic runs take the upstream
/// extractor's own category when it is a plain label.
fn message_category(category: &str, data: &serde_json::Map<String, Value>) -> String {
    if category != crate::sites::GENERIC_CATEGORY {
        return category.to_owned();
    }
    data.get("category")
        .and_then(Value::as_str)
        .filter(|value| {
            (1..=64).contains(&value.len())
                && value
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
                && !matches!(*value, "." | "..")
                && !DEDICATED_CATEGORIES.contains(value)
        })
        .unwrap_or(crate::sites::GENERIC_CATEGORY)
        .to_owned()
}

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
    let index = if crate::booru::Site::from_category(category).is_some() || category == "e926" {
        1
    } else if matches!(category, "pixiv" | "pixiv-novel") {
        data.get("num")
            .and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok()?.checked_add(1))
            .unwrap_or(0)
    } else {
        index
    };
    let sketch_archive = category == "pixiv"
        && (data.contains_key("post_id")
            || data.get("subcategory").and_then(Value::as_str) == Some("sketch"));
    let (post_id, media_id, shortcode, username, caption) = match category {
        "pixiv" if sketch_archive => (
            data.get("post_id")
                .and_then(crate::pixiv::sketch::id_from_value)
                .unwrap_or_default(),
            data.get("id")
                .and_then(crate::pixiv::sketch::id_from_value)
                .unwrap_or_default(),
            String::new(),
            data.get("user")
                .and_then(|user| user.get("unique_name"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            string("text"),
        ),
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
        "mastodon" | "mastodon.social" | "pawoo" | "baraag" => (
            id("id"),
            data.get("media")
                .and_then(|m| m.get("id"))
                .and_then(Value::as_str)
                .filter(|id| crate::mastodon::valid_media_id(id))
                .unwrap_or_default()
                .into(),
            String::new(),
            data.get("account")
                .and_then(|a| a.get("acct"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            string("content").map(|s| crate::mastodon::strip_html(&s)),
        ),
        "bluesky" => (
            id("post_id"),
            string("filename")
                .filter(|id| crate::bluesky::valid_cid(id))
                .unwrap_or_default(),
            String::new(),
            data.get("author")
                .and_then(|author| author.get("handle"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            string("text"),
        ),
        "pixiv" | "pixiv-novel" => {
            let post_id = id("id");
            let media_id = data
                .get("num")
                .and_then(Value::as_u64)
                .filter(|page| {
                    *page
                        < if category == "pixiv-novel"
                            || data.get("_ugoira_original").and_then(Value::as_bool) == Some(true)
                        {
                            100_000
                        } else {
                            10_000
                        }
                })
                .map(|page| format!("{post_id}_p{page}"))
                .unwrap_or_default();
            (
                post_id,
                media_id,
                String::new(),
                data.get("user")
                    .and_then(|user| user.get("account"))
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                if category == "pixiv-novel"
                    || data.contains_key("ranking")
                    || data.contains_key("search")
                    || data.contains_key("num_series")
                    || data.contains_key("user_follow")
                    || data.contains_key("user_bookmark")
                    || data.contains_key("pixivision_id")
                {
                    let caption = string("caption")
                        .map(|s| crate::mastodon::strip_html(&s))
                        .filter(|s| !s.trim().is_empty());
                    match (string("title"), caption) {
                        (Some(title), Some(caption)) => {
                            Some(format!("{title}\n\n{}", caption.trim()))
                        }
                        (Some(title), None) => Some(title),
                        (None, caption) if category == "pixiv" => {
                            caption.map(|s| s.trim().to_owned())
                        }
                        (None, caption) => caption,
                    }
                } else {
                    string("title")
                        .or_else(|| string("caption").map(|s| crate::mastodon::strip_html(&s)))
                },
            )
        }
        "danbooru" | "e621" | "e926" | "gelbooru" | "safebooru" => {
            let post_id = id("id");
            (
                post_id.clone(),
                string("md5")
                    .or_else(|| string("hash"))
                    .filter(|md5| crate::booru::valid_md5(md5))
                    .map(|md5| md5.to_ascii_lowercase())
                    .unwrap_or(post_id),
                String::new(),
                string("uploader_name").or_else(|| string("owner")),
                string("tag_string").or_else(|| string("tags")),
            )
        }
        "kemono" | "coomer" => (
            id("id"),
            string("hash")
                .filter(|hash| crate::kemono::valid_hash(hash))
                .unwrap_or_default(),
            String::new(),
            None,
            string("title"),
        ),
        // Registry sites and generic gallery-dl output keep the whole
        // keyword dictionary; identities come from upstream's `id`.
        _ if crate::sites::is_category(category) || generic_category(category) => (
            id("id"),
            id("id"),
            String::new(),
            string("username").or_else(|| string("user")),
            None,
        ),
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
    let discord = compatibility_discord(data, category);
    let mut file = crate::file_metadata::compatibility(data);
    if matches!(category, "kemono" | "coomer")
        && let Some(name) = string("name")
    {
        let kind = data.get("type").and_then(Value::as_str);
        let name = if discord.is_some()
            && (kind == Some("inline")
                || kind == Some("archive")
                    && (name.starts_with('/')
                        || name.starts_with("https://cdn.discordapp.com/")
                        || name.starts_with("https://media.discordapp.net/")))
        {
            name.split(['?', '#']).next().unwrap_or_default().to_owned()
        } else {
            name
        };
        file.get_or_insert_default().description = Some(std::sync::Arc::from(name));
    }
    if matches!(category, "kemono" | "coomer")
        && let Some(kind) = string("type")
            .filter(|kind| matches!(kind.as_str(), "file" | "attachment" | "inline" | "archive"))
    {
        file.get_or_insert_default().media_type = Some(std::sync::Arc::from(kind));
    }
    let creator = if discord.is_some() {
        None
    } else {
        compatibility_creator(data, category)
    };
    let username = if matches!(category, "kemono" | "coomer") {
        creator
            .as_ref()
            .and_then(|creator| creator.name.as_ref())
            .map(ToString::to_string)
    } else {
        username
    };
    let timestamp = data.get("date").and_then(|date| match date {
        Value::String(value) => crate::date::parse_date(value).ok(),
        value => value.as_i64(),
    });
    let profile_asset = if category == "pixiv"
        && data.get("subcategory").and_then(Value::as_str) == Some(post_id.as_str())
        && matches!(post_id.as_str(), "avatar" | "background")
    {
        let owner = data
            .get("user")
            .and_then(|user| user.get("id"))
            .and_then(|id| {
                id.as_str()
                    .map(str::to_owned)
                    .or_else(|| id.as_u64().map(|id| id.to_string()))
            });
        owner.and_then(|owner| crate::pixiv::profile_asset_identity(&post_id, &owner, timestamp))
    } else {
        None
    };
    let media_id = profile_asset.as_ref().map_or(media_id, |asset| {
        format!(
            "{}_{}_{}",
            asset.kind.as_str(),
            asset.owner_id,
            asset.version
        )
    });
    Some(PostMetadata {
        pixiv_novel_metadata: (category == "pixiv-novel")
            .then(|| crate::pixiv::NovelMetadata::parse(data).map(std::sync::Arc::new))
            .flatten(),
        ajax: (category == "pixiv")
            .then(|| data.get("_ajax").and_then(Value::as_bool))
            .flatten(),
        mypixiv: (category == "pixiv")
            .then(|| data.get("_mypixiv").and_then(Value::as_bool))
            .flatten(),
        caption_html: matches!(category, "pixiv" | "pixiv-novel")
            .then(|| {
                data.get("caption")
                    .and_then(Value::as_str)
                    .map(std::sync::Arc::from)
            })
            .flatten(),
        pixiv_user_detail: matches!(category, "pixiv" | "pixiv-novel")
            .then(|| crate::pixiv::UserDetail::parse_compat(data).map(std::sync::Arc::new))
            .flatten(),
        comments: matches!(category, "pixiv" | "pixiv-novel")
            .then(|| data.get("comments").and_then(pixiv_comments))
            .flatten(),
        tags_bookmark: matches!(category, "pixiv" | "pixiv-novel")
            .then(|| data.get("tags_bookmark").and_then(pixiv_bookmark_tags))
            .flatten(),
        favgroup: if category == "danbooru" {
            data.get("favgroup")
                .filter(|value| value.is_object())
                .cloned()
                .map(std::sync::Arc::new)
        } else {
            None
        },
        pool: if matches!(category, "danbooru" | "e621" | "e926") {
            data.get("pool")
                .filter(|value| value.is_object())
                .cloned()
                .map(std::sync::Arc::new)
        } else if matches!(category, "gelbooru" | "safebooru") {
            data.get("pool").cloned().map(std::sync::Arc::new)
        } else {
            None
        },
        search_tags: crate::booru::Site::from_category(category)
            .or_else(|| (category == "e926").then_some(crate::booru::Site::E621))
            .and_then(|_| data.get("search_tags").and_then(Value::as_str))
            .filter(|tags| tags.len() <= 1024)
            .map(std::sync::Arc::from),
        source_defaults: None,
        booru_artist_parent: None,
        booru_popular: matches!(category, "danbooru" | "e621" | "e926")
            .then(|| {
                data.get("scale").map(|_| {
                    std::sync::Arc::new(
                        ["date", "scale"]
                            .into_iter()
                            .filter_map(|key| data.get(key).map(|v| (key.into(), v.clone())))
                            .collect(),
                    )
                })
            })
            .flatten(),
        booru_favorite_id: matches!(category, "gelbooru" | "safebooru")
            .then(|| data.get("favorite_id").cloned())
            .flatten(),
        booru_pool_name: matches!(category, "gelbooru" | "safebooru")
            .then(|| data.get("pool_name").cloned())
            .flatten(),
        booru_user_id: matches!(category, "e621" | "e926")
            .then(|| data.get("user_id").cloned())
            .flatten(),
        booru_keywords: (matches!(
            category,
            "danbooru" | "e621" | "e926" | "gelbooru" | "safebooru"
        ) && (data.contains_key("filename")
            || data.contains_key("extension")
            || data.contains_key("md5")))
        .then(|| {
            std::sync::Arc::new(
                ["filename", "extension", "md5"]
                    .into_iter()
                    .chain(["id", "tags", "date_favorited"].into_iter().filter(|key| {
                        matches!(category, "gelbooru" | "safebooru")
                            && (data.contains_key("pool_name") || data.contains_key("favorite_id"))
                            && (*key != "id" || category == "safebooru")
                    }))
                    .filter_map(|key| data.get(key).map(|value| (key.to_owned(), value.clone())))
                    .collect(),
            )
        }),
        source_num: data.get("num").cloned(),
        source_suffix: data.get("suffix").cloned(),
        keyword_updates: None,
        ugoira_original: (category == "pixiv")
            .then(|| data.get("_ugoira_original").and_then(Value::as_bool))
            .flatten(),
        ugoira_frame_index: (category == "pixiv")
            .then(|| {
                data.get("_ugoira_frame_index")
                    .and_then(Value::as_u64)
                    .and_then(|index| usize::try_from(index).ok())
                    .filter(|index| *index < 100_000)
            })
            .flatten(),
        user_bookmark: (category == "pixiv")
            .then(|| {
                data.get("user_bookmark")
                    .and_then(crate::pixiv::FollowedUserMetadata::parse_compat)
                    .map(std::sync::Arc::new)
            })
            .flatten(),
        bookmark_archive: category == "pixiv"
            && (data.contains_key("user_bookmark")
                || data
                    .get("subcategory")
                    .and_then(Value::as_str)
                    .is_some_and(|value| matches!(value, "favorite" | "bookmark"))),
        sketch: sketch_archive
            .then(|| crate::pixiv::SketchMetadata::parse_compat(data).map(std::sync::Arc::new))
            .flatten(),
        sketch_archive,
        tags: matches!(category, "pixiv" | "pixiv-novel")
            .then(|| data.get("tags").and_then(crate::pixiv::Tags::parse_compat))
            .flatten(),
        pixivision: (category == "pixiv")
            .then(|| crate::pixiv::PixivisionMetadata::parse_flat(data).map(std::sync::Arc::new))
            .flatten(),
        pixivision_archive: category == "pixiv"
            && (data.contains_key("pixivision_id") || data.contains_key("pixivision_title")),
        user_follow: (category == "pixiv")
            .then(|| {
                data.get("user_follow")
                    .and_then(crate::pixiv::FollowedUserMetadata::parse_compat)
                    .map(std::sync::Arc::new)
            })
            .flatten(),
        followed_archive: category == "pixiv" && data.contains_key("user_follow"),
        series: (category == "pixiv")
            .then(|| {
                data.get("series")
                    .and_then(crate::pixiv::SeriesMetadata::parse_compat)
                    .map(std::sync::Arc::new)
            })
            .flatten(),
        num_series: (category == "pixiv")
            .then(|| data.get("num_series").and_then(Value::as_i64))
            .flatten(),
        search: (category == "pixiv")
            .then(|| {
                data.get("search")
                    .and_then(crate::pixiv::SearchMetadata::parse_compat)
                    .map(std::sync::Arc::new)
            })
            .flatten(),
        search_archive: category == "pixiv" && data.contains_key("search"),
        ranking: (category == "pixiv")
            .then(|| {
                data.get("ranking")
                    .and_then(crate::pixiv::RankingMetadata::parse_compat)
                    .map(std::sync::Arc::new)
            })
            .flatten(),
        ranking_archive: category == "pixiv" && data.contains_key("ranking"),
        profile_asset,
        discord: discord.clone(),
        creator,
        revision: compatibility_revision(data, category),
        rating: (category == "pixiv-novel"
            || crate::booru::Site::from_category(category).is_some())
        .then(|| string("rating"))
        .flatten()
        .filter(|r| r.len() <= 32 && !r.chars().any(char::is_control))
        .map(std::sync::Arc::from),
        score: crate::booru::Site::from_category(category)
            .and_then(|_| data.get("score"))
            .and_then(|score| {
                score
                    .as_object()
                    .and_then(|s| s.get("total"))
                    .or(Some(score))
            })
            .and_then(|score| score.as_i64().or_else(|| score.as_str()?.parse().ok())),
        file,
        frames: (category == "pixiv")
            .then(|| data.get("frames").and_then(crate::pixiv::parse_frames))
            .flatten(),
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
        instance: matches!(
            category,
            "mastodon" | "mastodon.social" | "pawoo" | "baraag"
        )
        .then(|| {
            string("instance").filter(|host| crate::mastodon::INSTANCES.contains(&host.as_str()))
        })
        .flatten()
        .or_else(|| discord.as_ref().map(|_| "discord".to_owned()))
        .or_else(|| {
            matches!(category, "kemono" | "coomer")
                .then(|| string("service").filter(|service| crate::kemono::valid_service(service)))
                .flatten()
        }),
        site_keywords: match crate::sites::site(category) {
            Some(site) => Some(std::sync::Arc::new(crate::sites::SiteKeywords {
                route: crate::sites::route_for(site, data),
                fields: data.clone(),
            })),
            None if generic_category(category) => {
                Some(std::sync::Arc::new(crate::sites::SiteKeywords {
                    route: &crate::sites::GENERIC,
                    fields: data.clone(),
                }))
            }
            None => None,
        },
        post_id,
        media_id,
        shortcode,
        username: username.map(std::sync::Arc::from),
        caption: if discord.is_some() {
            data.get("content")
                .and_then(Value::as_str)
                .map(str::to_owned)
        } else {
            caption
        }
        .map(std::sync::Arc::from),
        timestamp,
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

fn compatibility_revision(
    data: &serde_json::Map<String, Value>,
    category: &str,
) -> Option<crate::kemono::RevisionMetadata> {
    if !matches!(category, "kemono" | "coomer")
        || ![
            "revision_id",
            "revision_hash",
            "revision_index",
            "revision_count",
        ]
        .iter()
        .any(|key| data.contains_key(*key))
    {
        return None;
    }
    let integer = |key: &str| {
        data.get(key).and_then(|value| {
            value.as_u64().or_else(|| {
                let value = value.as_str()?;
                (!value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()))
                    .then(|| value.parse().ok())
                    .flatten()
            })
        })
    };
    let position = |key| {
        integer(key)
            .and_then(|value| usize::try_from(value).ok())
            .filter(|value| *value > 0)
    };
    Some(crate::kemono::RevisionMetadata {
        id: integer("revision_id"),
        hash: data
            .get("revision_hash")
            .and_then(Value::as_str)
            .filter(|hash| hash.len() == 40 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
            .map(|hash| std::sync::Arc::from(hash.to_ascii_lowercase())),
        index: position("revision_index"),
        count: position("revision_count"),
    })
}

fn compatibility_creator(
    data: &serde_json::Map<String, Value>,
    category: &str,
) -> Option<crate::kemono::CreatorMetadata> {
    if !matches!(category, "kemono" | "coomer") {
        return None;
    }
    let id = data
        .get("user")
        .and_then(|value| match value {
            Value::String(value) => Some(value.clone()),
            Value::Number(value) if value.is_u64() => Some(value.to_string()),
            _ => None,
        })
        .filter(|id| crate::kemono::valid_id(id))
        .unwrap_or_default();
    let service = data
        .get("service")
        .and_then(Value::as_str)
        .filter(|service| crate::kemono::valid_service(service))
        .unwrap_or_default()
        .to_owned();
    let mut creator = crate::kemono::CreatorMetadata {
        id,
        service,
        ..Default::default()
    };
    if let Some(profile) = data.get("user_profile") {
        if !creator.id.is_empty() && !creator.service.is_empty() {
            return Some(
                crate::kemono::parse_profile_value(profile, &creator.service, &creator.id)
                    .unwrap_or(creator),
            );
        }
    } else {
        creator.name = data
            .get("username")
            .and_then(Value::as_str)
            .filter(|name| {
                !name.is_empty() && name.len() <= 64 * 1024 && !name.chars().any(char::is_control)
            })
            .map(std::sync::Arc::from);
    }
    Some(creator)
}

// Recognized Discord output always retains its own scope, even if malformed
// identity prevents an archive key. Descriptive fields never supply IDs.
fn compatibility_discord(
    data: &serde_json::Map<String, Value>,
    category: &str,
) -> Option<crate::kemono::DiscordMetadata> {
    if !matches!(category, "kemono" | "coomer")
        || !(data.get("subcategory").and_then(Value::as_str) == Some("discord")
            || data.contains_key("server_id")
            || data.contains_key("channel_id"))
    {
        return None;
    }
    let id = |key| {
        data.get(key)
            .and_then(|value| match value {
                Value::String(value) => Some(value.clone()),
                Value::Number(value) if value.is_u64() => Some(value.to_string()),
                _ => None,
            })
            .filter(|id| crate::kemono::valid_discord_id(id))
    };
    let text = |key| {
        data.get(key)
            .and_then(Value::as_str)
            .filter(|text| text.len() <= 64 * 1024 && !text.contains('\0'))
            .map(std::sync::Arc::from)
    };
    Some(crate::kemono::DiscordMetadata {
        server_id: id("server_id").unwrap_or_default(),
        channel_id: id("channel_id").unwrap_or_default(),
        server: text("server"),
        channel: text("channel"),
        channel_topic: text("channel_topic"),
        channel_nsfw: data.get("channel_nsfw").and_then(Value::as_bool),
        channel_type: data.get("channel_type").and_then(Value::as_u64),
        parent_id: id("parent_id"),
    })
}

#[cfg(test)]
mod collection_metadata_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn compatibility_collections_belong_only_to_their_booru_categories() {
        let fields = json!({"id":42,"pool":{"id":99},"favgroup":{"id":77}});
        for category in [
            "danbooru",
            "e621",
            "gelbooru",
            "safebooru",
            "pixiv",
            "pixiv-novel",
            "twitter",
            "kemono",
        ] {
            let metadata = compatibility_metadata(fields.as_object().unwrap(), category).unwrap();
            assert_eq!(
                metadata.pool.is_some(),
                matches!(category, "danbooru" | "e621" | "gelbooru" | "safebooru"),
                "{category}"
            );
            assert_eq!(
                metadata.favgroup.is_some(),
                category == "danbooru",
                "{category}"
            );
        }
    }
}
