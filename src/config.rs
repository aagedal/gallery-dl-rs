//! Explicit native configuration: a strict subset of gallery-dl's JSON schema.
//!
//! See <https://gdl-org.github.io/docs/configuration.html>. Extractor defaults
//! are overridden by category settings, then by top-level global settings.
//! Paths are literal, relative to the configuration directory; expansion,
//! conditions, hooks, subcategories, and unsupported options are rejected.
use crate::{
    Result,
    date::parse_date,
    error,
    naming::{DirectoryTemplate, FilenameTemplate},
    network::NetworkOptions,
    selection::RangeSelection,
    size::parse_bytes,
};
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::io::AsyncReadExt;

/// Maximum encoded JSON size, including whitespace.
pub const MAX_CONFIG_BYTES: usize = 1024 * 1024;

/// Sparse settings: absent fields leave the caller's defaults unchanged.
#[derive(Clone, Default, PartialEq)]
pub struct Options {
    pub base_directory: Option<PathBuf>,
    pub filename: Option<String>,
    /// Ordered `(filter, template)` pairs from an object-valued `filename`;
    /// `filename` then holds the optional `""` default. Overlaid together.
    pub filename_conditions: Option<Vec<(String, String)>>,
    /// One-based media position ranges, applied separately to each input.
    pub file_range: Option<String>,
    /// AND-combined media filters, with an empty list clearing inherited clauses.
    pub file_filter: Option<Vec<String>>,
    /// One-based explicit post ranges, independent of file positions.
    pub post_range: Option<String>,
    /// AND-combined post metadata filters.
    pub post_filter: Option<Vec<String>>,
    /// One-based child job ranges and metadata predicates.
    pub child_range: Option<String>,
    pub child_filter: Option<Vec<String>>,
    /// Ordered profile child views; an explicit empty setting disables dispatch.
    pub include: Option<crate::dispatch::Include>,
    pub date_before: Option<i64>,
    pub date_after: Option<i64>,
    /// Whether to include reposts, optionally under the original author.
    pub retweets: Option<crate::twitter::RetweetMode>,
    /// Include media from quoted posts.
    pub quoted: Option<bool>,
    /// Expand Twitter conversations or fetch full Kemono/Coomer post details.
    pub expand: Option<bool>,
    /// Suppress duplicate Twitter posts.
    pub unique: Option<bool>,
    /// Select native video files, skip videos, or delegate video pages.
    pub videos: Option<crate::twitter::VideoMode>,
    /// Include video and animated-GIF preview images.
    pub previews: Option<bool>,
    /// Prefer MP4 over animated GIF files on Imgur.
    pub mp4: Option<bool>,
    /// Include Mastodon reblogs and their original attachments.
    pub reblogs: Option<bool>,
    pub tumblr_reblogs: Option<crate::tumblr::Reblogs>,
    pub posts: Option<Vec<String>>,
    pub inline: Option<crate::tumblr::Inline>,
    pub original: Option<bool>,
    pub files: Option<Vec<crate::kemono::FileKind>>,
    pub duplicates: Option<crate::kemono::DuplicateOptions>,
    pub revisions: Option<crate::kemono::RevisionMode>,
    pub order_revisions: Option<crate::kemono::RevisionOrder>,
    pub endpoint: Option<crate::kemono::Endpoint>,
    /// Bluesky's category-scoped endpoint selects the likes API.
    pub bluesky_endpoint: Option<crate::bluesky::LikesEndpoint>,
    /// Explicit Bluesky AppView server; empty restores the default.
    pub api_server: Option<String>,
    pub order_posts: Option<crate::kemono::PostOrder>,
    pub favorites: Option<crate::kemono::Favorites>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub user_id: Option<String>,
    /// Include Bluesky reposts in author feeds.
    pub reposts: Option<bool>,
    /// Include Pixiv ugoira frame ZIPs.
    pub ugoira: Option<bool>,
    /// Explicit Mastodon bearer token, confined to API requests. Empty disables.
    pub access_token: Option<String>,
    /// Include Mastodon statuses without selected media.
    pub text_posts: Option<bool>,
    /// Attempt explicitly unavailable Twitter media when a URL is provided.
    pub unavailable: Option<bool>,
    /// Emit Twitter posts without selected media as file-less post events.
    pub text_tweets: Option<bool>,
    /// Ordered Twitter image sizes.
    pub size: Option<crate::twitter::ImageOptions>,
    /// Try media fallback URLs after the primary URL fails.
    pub fallback: Option<bool>,
    /// Select article metadata, cover, media, and generated documents.
    pub articles: Option<crate::twitter::ArticleOptions>,
    /// Select embedded Twitter card media or delegated video pages.
    pub cards: Option<crate::twitter::CardMode>,
    /// Card names excluded from extraction; an empty list clears inherited entries.
    pub cards_blacklist: Option<Vec<String>>,
    /// Endpoint used to resolve a single Twitter post.
    pub tweet_endpoint: Option<crate::twitter::TweetEndpoint>,
    /// Include posts from the requested status conversation.
    pub conversations: Option<crate::twitter::ConversationMode>,
    /// Include the pinned post in account timelines.
    pub pinned: Option<bool>,
    /// Which reply posts to include in account timelines.
    pub replies: Option<crate::twitter::ReplyMode>,
    /// Root profile timeline strategy and search pagination settings.
    pub strategy: Option<crate::twitter::TimelineStrategy>,
    pub search_pagination: Option<crate::twitter::SearchPagination>,
    pub search_results: Option<crate::twitter::SearchProduct>,
    /// Number of posts requested per account or collection page.
    pub limit: Option<u8>,
    pub search_limit: Option<u8>,
    pub search_stop: Option<u8>,
    /// Limit Kemono/Coomer source posts; zero clears an inherited limit.
    pub max_posts: Option<usize>,
    /// Look up Kemono/Coomer creator profiles.
    pub metadata: Option<bool>,
    /// API key for supported API-based extractors, including Pixeldrain.
    pub api_key: Option<String>,
    /// Pixiv OAuth refresh token; only accepted in the `pixiv` category.
    pub refresh_token: Option<String>,
    /// DeviantArt OAuth client ID; only accepted in the `deviantart` category.
    pub client_id: Option<String>,
    /// DeviantArt OAuth client secret; only accepted in the `deviantart` category.
    pub client_secret: Option<String>,
    /// Relative directory components; an empty list selects the base directory.
    pub directory: Option<Vec<String>>,
    /// Ordered `(filter, components)` pairs from an object-valued `directory`.
    pub directory_conditions: Option<Vec<(String, Vec<String>)>>,
    pub cookies: Option<PathBuf>,
    /// Explicit Firefox profile selected by an upstream-style `cookies` array.
    /// Mutually exclusive with the cookie file during scope resolution.
    pub browser_cookies: Option<PathBuf>,
    pub proxy: Option<String>,
    pub user_agent: Option<String>,
    /// Uses gallery-dl-rs's archive identities, not upstream database IDs.
    pub archive: Option<PathBuf>,
    pub filesize_min: Option<u64>,
    pub filesize_max: Option<u64>,
    pub retries: Option<u8>,
    pub timeout: Option<Duration>,
}

impl std::fmt::Debug for Options {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Options { settings: [redacted] }")
    }
}

impl Options {
    pub fn kemono_selection(&self) -> crate::kemono::Selection {
        crate::kemono::Selection {
            files: self.files.clone(),
            duplicates: self.duplicates.unwrap_or_default(),
            original: self.original.unwrap_or(false),
        }
    }

    pub fn tumblr_selection(&self) -> crate::tumblr::Selection {
        crate::tumblr::Selection {
            reblogs: self.tumblr_reblogs.unwrap_or_else(|| {
                if self.reblogs == Some(false) {
                    crate::tumblr::Reblogs::OriginalOnly
                } else {
                    crate::tumblr::Reblogs::All
                }
            }),
            posts: self.posts.as_ref().and_then(|types| {
                (types.as_slice() != ["all"]).then(|| types.iter().cloned().collect())
            }),
            inline: self.inline.unwrap_or_default(),
            original: self.original.unwrap_or(true),
        }
    }

    /// Return a copy with every present field in `overrides` taking precedence.
    /// This can apply explicit CLI values after configuration resolution.
    /// Reblog fields are one setting; an explicit Tumblr mode takes precedence
    /// when a programmatic override supplies both representations.
    pub fn overlay(&self, overrides: &Self) -> Self {
        Self {
            base_directory: overrides
                .base_directory
                .clone()
                .or_else(|| self.base_directory.clone()),
            filename: if overrides.filename_conditions.is_some() {
                overrides.filename.clone()
            } else {
                overrides.filename.clone().or_else(|| self.filename.clone())
            },
            filename_conditions: if overrides.filename.is_some()
                || overrides.filename_conditions.is_some()
            {
                overrides.filename_conditions.clone()
            } else {
                self.filename_conditions.clone()
            },
            file_range: overrides
                .file_range
                .clone()
                .or_else(|| self.file_range.clone()),
            file_filter: overrides
                .file_filter
                .clone()
                .or_else(|| self.file_filter.clone()),
            post_range: overrides
                .post_range
                .clone()
                .or_else(|| self.post_range.clone()),
            post_filter: overrides
                .post_filter
                .clone()
                .or_else(|| self.post_filter.clone()),
            child_range: overrides
                .child_range
                .clone()
                .or_else(|| self.child_range.clone()),
            child_filter: overrides
                .child_filter
                .clone()
                .or_else(|| self.child_filter.clone()),
            include: overrides.include.clone().or_else(|| self.include.clone()),
            date_before: overrides.date_before.or(self.date_before),
            date_after: overrides.date_after.or(self.date_after),
            retweets: overrides.retweets.or(self.retweets),
            quoted: overrides.quoted.or(self.quoted),
            expand: overrides.expand.or(self.expand),
            unique: overrides.unique.or(self.unique),
            videos: overrides.videos.or(self.videos),
            previews: overrides.previews.or(self.previews),
            mp4: overrides.mp4.or(self.mp4),
            reblogs: if overrides.tumblr_reblogs.is_some() {
                None
            } else {
                overrides.reblogs.or(self.reblogs)
            },
            tumblr_reblogs: if overrides.tumblr_reblogs.is_some() {
                overrides.tumblr_reblogs
            } else if overrides.reblogs.is_some() {
                None
            } else {
                self.tumblr_reblogs
            },
            posts: overrides.posts.clone().or_else(|| self.posts.clone()),
            inline: overrides.inline.or(self.inline),
            original: overrides.original.or(self.original),
            files: overrides.files.clone().or_else(|| self.files.clone()),
            duplicates: overrides.duplicates.or(self.duplicates),
            revisions: overrides.revisions.or(self.revisions),
            order_revisions: overrides.order_revisions.or(self.order_revisions),
            endpoint: overrides.endpoint.or(self.endpoint),
            bluesky_endpoint: overrides.bluesky_endpoint.or(self.bluesky_endpoint),
            api_server: overrides
                .api_server
                .clone()
                .or_else(|| self.api_server.clone()),
            order_posts: overrides.order_posts.or(self.order_posts),
            favorites: overrides.favorites.or(self.favorites),
            username: overrides.username.clone().or_else(|| self.username.clone()),
            password: overrides.password.clone().or_else(|| self.password.clone()),
            user_id: overrides.user_id.clone().or_else(|| self.user_id.clone()),
            reposts: overrides.reposts.or(self.reposts),
            ugoira: overrides.ugoira.or(self.ugoira),
            access_token: overrides
                .access_token
                .clone()
                .or_else(|| self.access_token.clone()),
            text_posts: overrides.text_posts.or(self.text_posts),
            unavailable: overrides.unavailable.or(self.unavailable),
            text_tweets: overrides.text_tweets.or(self.text_tweets),
            size: overrides.size.clone().or_else(|| self.size.clone()),
            fallback: overrides.fallback.or(self.fallback),
            articles: overrides.articles.or(self.articles),
            cards: overrides.cards.or(self.cards),
            cards_blacklist: overrides
                .cards_blacklist
                .clone()
                .or_else(|| self.cards_blacklist.clone()),
            tweet_endpoint: overrides.tweet_endpoint.or(self.tweet_endpoint),
            conversations: overrides.conversations.or(self.conversations),
            pinned: overrides.pinned.or(self.pinned),
            replies: overrides.replies.or(self.replies),
            strategy: overrides.strategy.or(self.strategy),
            search_pagination: overrides.search_pagination.or(self.search_pagination),
            search_results: overrides.search_results.or(self.search_results),
            limit: overrides.limit.or(self.limit),
            search_limit: overrides.search_limit.or(self.search_limit),
            search_stop: overrides.search_stop.or(self.search_stop),
            max_posts: overrides.max_posts.or(self.max_posts),
            metadata: overrides.metadata.or(self.metadata),
            api_key: overrides.api_key.clone().or_else(|| self.api_key.clone()),
            refresh_token: overrides
                .refresh_token
                .clone()
                .or_else(|| self.refresh_token.clone()),
            client_id: overrides
                .client_id
                .clone()
                .or_else(|| self.client_id.clone()),
            client_secret: overrides
                .client_secret
                .clone()
                .or_else(|| self.client_secret.clone()),
            directory: if overrides.directory_conditions.is_some() {
                overrides.directory.clone()
            } else {
                overrides
                    .directory
                    .clone()
                    .or_else(|| self.directory.clone())
            },
            directory_conditions: if overrides.directory.is_some()
                || overrides.directory_conditions.is_some()
            {
                overrides.directory_conditions.clone()
            } else {
                self.directory_conditions.clone()
            },
            cookies: if overrides.browser_cookies.is_some() {
                None
            } else {
                overrides.cookies.clone().or_else(|| self.cookies.clone())
            },
            browser_cookies: if overrides.browser_cookies.is_some() {
                overrides.browser_cookies.clone()
            } else if overrides.cookies.is_some() {
                None
            } else {
                self.browser_cookies.clone()
            },
            proxy: overrides.proxy.clone().or_else(|| self.proxy.clone()),
            user_agent: overrides
                .user_agent
                .clone()
                .or_else(|| self.user_agent.clone()),
            archive: overrides.archive.clone().or_else(|| self.archive.clone()),
            filesize_min: overrides.filesize_min.or(self.filesize_min),
            filesize_max: overrides.filesize_max.or(self.filesize_max),
            retries: overrides.retries.or(self.retries),
            timeout: overrides.timeout.or(self.timeout),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Config {
    global: Options,
    extractor: Options,
    categories: BTreeMap<String, Options>,
}

// Separate structs deliberately avoid serde flatten: duplicate and unknown
// keys must fail at every level, including global versus extractor scopes.
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
struct RawRoot {
    base_directory: Field,
    filename: Field,
    file_range: Field,
    file_filter: Field,
    image_filter: Field,
    post_range: Field,
    post_filter: Field,
    child_range: Field,
    child_filter: Field,
    include: Field,
    date_before: Field,
    date_after: Field,
    retweets: Field,
    quoted: Field,
    expand: Field,
    unique: Field,
    videos: Field,
    previews: Field,
    mp4: Field,
    reblogs: Field,
    posts: Field,
    inline: Field,
    original: Field,
    files: Field,
    duplicates: Field,
    revisions: Field,
    order_revisions: Field,
    endpoint: Field,
    order_posts: Field,
    favorites: Field,
    reposts: Field,
    ugoira: Field,
    text_posts: Field,
    unavailable: Field,
    text_tweets: Field,
    size: Field,
    fallback: Field,
    articles: Field,
    cards: Field,
    cards_blacklist: Field,
    tweet_endpoint: Field,
    conversations: Field,
    pinned: Field,
    replies: Field,
    strategy: Field,
    search_pagination: Field,
    search_results: Field,
    limit: Field,
    search_limit: Field,
    search_stop: Field,
    max_posts: Field,
    metadata: Field,
    directory: Field,
    cookies: Field,
    proxy: Field,
    user_agent: Field,
    archive: Field,
    filesize_min: Field,
    filesize_max: Field,
    retries: Field,
    timeout: Field,
    extractor: RawExtractor,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
struct RawExtractor {
    base_directory: Field,
    filename: Field,
    file_range: Field,
    file_filter: Field,
    image_filter: Field,
    post_range: Field,
    post_filter: Field,
    child_range: Field,
    child_filter: Field,
    include: Field,
    date_before: Field,
    date_after: Field,
    retweets: Field,
    quoted: Field,
    expand: Field,
    unique: Field,
    videos: Field,
    previews: Field,
    mp4: Field,
    reblogs: Field,
    posts: Field,
    inline: Field,
    original: Field,
    files: Field,
    duplicates: Field,
    revisions: Field,
    order_revisions: Field,
    endpoint: Field,
    order_posts: Field,
    favorites: Field,
    reposts: Field,
    ugoira: Field,
    text_posts: Field,
    unavailable: Field,
    text_tweets: Field,
    size: Field,
    fallback: Field,
    articles: Field,
    cards: Field,
    cards_blacklist: Field,
    tweet_endpoint: Field,
    conversations: Field,
    pinned: Field,
    replies: Field,
    strategy: Field,
    search_pagination: Field,
    search_results: Field,
    limit: Field,
    search_limit: Field,
    search_stop: Field,
    max_posts: Field,
    metadata: Field,
    directory: Field,
    cookies: Field,
    proxy: Field,
    user_agent: Field,
    archive: Field,
    filesize_min: Field,
    filesize_max: Field,
    retries: Field,
    timeout: Field,
    instagram: Box<RawOptions>,
    twitter: Box<RawOptions>,
    facebook: Box<RawOptions>,
    imgur: Box<RawOptions>,
    bluesky: Box<RawOptions>,
    reddit: Box<RawOptions>,
    danbooru: Box<RawOptions>,
    e621: Box<RawOptions>,
    gelbooru: Box<RawOptions>,
    safebooru: Box<RawOptions>,
    mastodon: Box<RawOptions>,
    #[serde(rename = "mastodon.social")]
    mastodon_social: Box<RawOptions>,
    pawoo: Box<RawOptions>,
    baraag: Box<RawOptions>,
    kemono: Box<RawOptions>,
    coomer: Box<RawOptions>,
    pixeldrain: Box<RawOptions>,
    catbox: Box<RawOptions>,
    imgbox: Box<RawOptions>,
    postimg: Box<RawOptions>,
    tumblr: Box<RawOptions>,
    pixiv: Box<RawOptions>,
    deviantart: Box<RawOptions>,
    flickr: Box<RawOptions>,
    wallhaven: Box<RawOptions>,
    artstation: Box<RawOptions>,
    pinterest: Box<RawOptions>,
    newgrounds: Box<RawOptions>,
    furaffinity: Box<RawOptions>,
    weasyl: Box<RawOptions>,
    vsco: Box<RawOptions>,
    direct: Box<RawOptions>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
struct RawOptions {
    base_directory: Field,
    filename: Field,
    file_range: Field,
    file_filter: Field,
    image_filter: Field,
    post_range: Field,
    post_filter: Field,
    child_range: Field,
    child_filter: Field,
    include: Field,
    date_before: Field,
    date_after: Field,
    retweets: Field,
    quoted: Field,
    expand: Field,
    unique: Field,
    videos: Field,
    previews: Field,
    mp4: Field,
    reblogs: Field,
    posts: Field,
    inline: Field,
    original: Field,
    files: Field,
    duplicates: Field,
    revisions: Field,
    order_revisions: Field,
    endpoint: Field,
    order_posts: Field,
    favorites: Field,
    reposts: Field,
    ugoira: Field,
    text_posts: Field,
    unavailable: Field,
    text_tweets: Field,
    size: Field,
    fallback: Field,
    articles: Field,
    cards: Field,
    cards_blacklist: Field,
    tweet_endpoint: Field,
    conversations: Field,
    pinned: Field,
    replies: Field,
    strategy: Field,
    search_pagination: Field,
    search_results: Field,
    limit: Field,
    search_limit: Field,
    search_stop: Field,
    max_posts: Field,
    metadata: Field,
    api_key: Field,
    username: Field,
    password: Field,
    user_id: Field,
    access_token: Field,
    refresh_token: Field,
    api_server: Field,
    client_id: Field,
    client_secret: Field,
    directory: Field,
    cookies: Field,
    proxy: Field,
    user_agent: Field,
    archive: Field,
    filesize_min: Field,
    filesize_max: Field,
    retries: Field,
    timeout: Field,
}

/// Unlike Option<Value>, preserve explicit null so it is rejected rather than
/// silently inheriting a potentially surprising parent value.
#[derive(Default)]
struct Field(Option<Value>);
impl<'de> Deserialize<'de> for Field {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        Value::deserialize(deserializer).map(|value| Self(Some(value)))
    }
}

impl Config {
    /// Read at most MAX_CONFIG_BYTES + 1 bytes. Diagnostics never include file
    /// paths, JSON source, unknown key names, or credential values.
    pub async fn load(path: &Path) -> Result<Self> {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(|_| error("cannot resolve native configuration directory"))?
                .join(path)
        };
        let parent = absolute
            .parent()
            .ok_or_else(|| error("native configuration has no parent directory"))?;
        let parent = tokio::fs::canonicalize(parent)
            .await
            .map_err(|_| error("cannot resolve native configuration directory"))?;
        let file = tokio::fs::File::open(&absolute)
            .await
            .map_err(|_| error("cannot open native configuration"))?;
        let mut bytes = Vec::new();
        file.take((MAX_CONFIG_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| error("cannot read native configuration"))?;
        Self::parse(&bytes, &parent)
    }

    /// Parse using an absolute directory for relative paths. This never reads
    /// cookies or archives and never evaluates shell/environment expressions.
    pub fn parse(bytes: &[u8], directory: &Path) -> Result<Self> {
        if bytes.len() > MAX_CONFIG_BYTES {
            return Err(error("native configuration exceeds 1 MiB"));
        }
        if !directory.is_absolute() {
            return Err(error("native configuration directory must be absolute"));
        }
        // Do not forward serde's diagnostics: they can quote private values or
        // a malicious unknown key. Known setting failures are reported below.
        // serde's struct deserializer also accepts positional arrays. Restrict
        // every configuration scope to JSON objects before typed decoding.
        let shape: Value = serde_json::from_slice(bytes)
            .map_err(|_| error("invalid native configuration JSON or unsupported option"))?;
        let object = shape
            .as_object()
            .ok_or_else(|| error("native configuration must be a JSON object"))?;
        if let Some(extractor) = object.get("extractor") {
            let extractor = extractor
                .as_object()
                .ok_or_else(|| error("native configuration extractor must be a JSON object"))?;
            for name in [
                "instagram",
                "twitter",
                "facebook",
                "imgur",
                "bluesky",
                "reddit",
                "danbooru",
                "e621",
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
            ] {
                if extractor.get(name).is_some_and(|value| !value.is_object()) {
                    return Err(error("native configuration category must be a JSON object"));
                }
            }
        }
        drop(shape);
        let root: RawRoot = serde_json::from_slice(bytes)
            .map_err(|_| error("invalid native configuration JSON or unsupported option"))?;
        let global = RawOptions {
            base_directory: root.base_directory,
            filename: root.filename,
            file_range: root.file_range,
            file_filter: root.file_filter,
            image_filter: root.image_filter,
            post_range: root.post_range,
            post_filter: root.post_filter,
            child_range: root.child_range,
            child_filter: root.child_filter,
            include: root.include,
            date_before: root.date_before,
            date_after: root.date_after,
            retweets: root.retweets,
            quoted: root.quoted,
            expand: root.expand,
            unique: root.unique,
            videos: root.videos,
            previews: root.previews,
            mp4: root.mp4,
            reblogs: root.reblogs,
            posts: root.posts,
            inline: root.inline,
            original: root.original,
            files: root.files,
            duplicates: root.duplicates,
            revisions: root.revisions,
            order_revisions: root.order_revisions,
            endpoint: root.endpoint,
            order_posts: root.order_posts,
            favorites: root.favorites,
            reposts: root.reposts,
            ugoira: root.ugoira,
            text_posts: root.text_posts,
            unavailable: root.unavailable,
            text_tweets: root.text_tweets,
            size: root.size,
            fallback: root.fallback,
            articles: root.articles,
            cards: root.cards,
            cards_blacklist: root.cards_blacklist,
            tweet_endpoint: root.tweet_endpoint,
            conversations: root.conversations,
            pinned: root.pinned,
            replies: root.replies,
            strategy: root.strategy,
            search_pagination: root.search_pagination,
            search_results: root.search_results,
            limit: root.limit,
            search_limit: root.search_limit,
            search_stop: root.search_stop,
            max_posts: root.max_posts,
            metadata: root.metadata,
            api_key: Field::default(),
            username: Field::default(),
            password: Field::default(),
            user_id: Field::default(),
            access_token: Field::default(),
            refresh_token: Field::default(),
            api_server: Field::default(),
            client_id: Field::default(),
            client_secret: Field::default(),
            directory: root.directory,
            cookies: root.cookies,
            proxy: root.proxy,
            user_agent: root.user_agent,
            archive: root.archive,
            filesize_min: root.filesize_min,
            filesize_max: root.filesize_max,
            retries: root.retries,
            timeout: root.timeout,
        }
        .resolve(directory)?;
        let extractor = root.extractor;
        let defaults = RawOptions {
            base_directory: extractor.base_directory,
            filename: extractor.filename,
            file_range: extractor.file_range,
            file_filter: extractor.file_filter,
            image_filter: extractor.image_filter,
            post_range: extractor.post_range,
            post_filter: extractor.post_filter,
            child_range: extractor.child_range,
            child_filter: extractor.child_filter,
            include: extractor.include,
            date_before: extractor.date_before,
            date_after: extractor.date_after,
            retweets: extractor.retweets,
            quoted: extractor.quoted,
            expand: extractor.expand,
            unique: extractor.unique,
            videos: extractor.videos,
            previews: extractor.previews,
            mp4: extractor.mp4,
            reblogs: extractor.reblogs,
            posts: extractor.posts,
            inline: extractor.inline,
            original: extractor.original,
            files: extractor.files,
            duplicates: extractor.duplicates,
            revisions: extractor.revisions,
            order_revisions: extractor.order_revisions,
            endpoint: extractor.endpoint,
            order_posts: extractor.order_posts,
            favorites: extractor.favorites,
            reposts: extractor.reposts,
            ugoira: extractor.ugoira,
            text_posts: extractor.text_posts,
            unavailable: extractor.unavailable,
            text_tweets: extractor.text_tweets,
            size: extractor.size,
            fallback: extractor.fallback,
            articles: extractor.articles,
            cards: extractor.cards,
            cards_blacklist: extractor.cards_blacklist,
            tweet_endpoint: extractor.tweet_endpoint,
            conversations: extractor.conversations,
            pinned: extractor.pinned,
            replies: extractor.replies,
            strategy: extractor.strategy,
            search_pagination: extractor.search_pagination,
            search_results: extractor.search_results,
            limit: extractor.limit,
            search_limit: extractor.search_limit,
            search_stop: extractor.search_stop,
            max_posts: extractor.max_posts,
            metadata: extractor.metadata,
            api_key: Field::default(),
            username: Field::default(),
            password: Field::default(),
            user_id: Field::default(),
            access_token: Field::default(),
            refresh_token: Field::default(),
            api_server: Field::default(),
            client_id: Field::default(),
            client_secret: Field::default(),
            directory: extractor.directory,
            cookies: extractor.cookies,
            proxy: extractor.proxy,
            user_agent: extractor.user_agent,
            archive: extractor.archive,
            filesize_min: extractor.filesize_min,
            filesize_max: extractor.filesize_max,
            retries: extractor.retries,
            timeout: extractor.timeout,
        }
        .resolve(directory)?;
        let mut categories = BTreeMap::new();
        for (name, mut settings) in [
            ("instagram", extractor.instagram),
            ("twitter", extractor.twitter),
            ("facebook", extractor.facebook),
            ("imgur", extractor.imgur),
            ("bluesky", extractor.bluesky),
            ("reddit", extractor.reddit),
            ("danbooru", extractor.danbooru),
            ("e621", extractor.e621),
            ("gelbooru", extractor.gelbooru),
            ("safebooru", extractor.safebooru),
            ("mastodon", extractor.mastodon),
            ("mastodon.social", extractor.mastodon_social),
            ("pawoo", extractor.pawoo),
            ("baraag", extractor.baraag),
            ("kemono", extractor.kemono),
            ("coomer", extractor.coomer),
            ("pixeldrain", extractor.pixeldrain),
            ("catbox", extractor.catbox),
            ("imgbox", extractor.imgbox),
            ("postimg", extractor.postimg),
            ("tumblr", extractor.tumblr),
            ("pixiv", extractor.pixiv),
            ("deviantart", extractor.deviantart),
            ("flickr", extractor.flickr),
            ("wallhaven", extractor.wallhaven),
            ("artstation", extractor.artstation),
            ("pinterest", extractor.pinterest),
            ("newgrounds", extractor.newgrounds),
            ("furaffinity", extractor.furaffinity),
            ("weasyl", extractor.weasyl),
            ("vsco", extractor.vsco),
            ("direct", extractor.direct),
        ] {
            if !matches!(
                name,
                "tumblr" | "flickr" | "wallhaven" | "weasyl" | "pixeldrain" | "gelbooru"
            ) && settings.api_key.0.is_some()
            {
                return Err(error(
                    "native configuration api-key is only supported for tumblr, flickr, wallhaven, weasyl, pixeldrain and gelbooru",
                ));
            }
            if !matches!(name, "danbooru" | "e621")
                && (settings.username.0.is_some() || settings.password.0.is_some())
            {
                return Err(error(
                    "native configuration username/password are only supported for danbooru and e621",
                ));
            }
            if name != "gelbooru" && settings.user_id.0.is_some() {
                return Err(error(
                    "native configuration user-id is only supported for gelbooru",
                ));
            }
            if name != "bluesky" && settings.api_server.0.is_some() {
                return Err(error(
                    "native configuration api-server is only supported for bluesky",
                ));
            }
            let bluesky_endpoint = if name == "bluesky" {
                match settings.endpoint.0.take() {
                    None => None,
                    Some(Value::String(value)) if value.len() <= 128 && !value.contains('\0') => {
                        Some(if value == "getActorLikes" {
                            crate::bluesky::LikesEndpoint::ActorLikes
                        } else {
                            crate::bluesky::LikesEndpoint::Records
                        })
                    }
                    Some(Value::Null | Value::Bool(false)) => {
                        Some(crate::bluesky::LikesEndpoint::Records)
                    }
                    Some(_) => {
                        return Err(error(
                            "native configuration Bluesky endpoint must be a bounded string, false or null",
                        ));
                    }
                }
            } else {
                None
            };
            let tumblr_reblogs = if name == "tumblr" {
                match settings.reblogs.0.take() {
                    None => None,
                    Some(Value::Bool(true)) => Some(crate::tumblr::Reblogs::All),
                    Some(Value::Bool(false)) => Some(crate::tumblr::Reblogs::OriginalOnly),
                    Some(Value::String(value)) if value == "same-blog" => {
                        Some(crate::tumblr::Reblogs::SameBlog)
                    }
                    Some(_) => {
                        return Err(error(
                            "native configuration Tumblr reblogs must be a boolean or same-blog",
                        ));
                    }
                }
            } else {
                None
            };
            if !matches!(name, "mastodon.social" | "pawoo" | "baraag")
                && settings.access_token.0.is_some()
            {
                return Err(error(
                    "native configuration access-token requires a mastodon.social, pawoo or baraag instance scope",
                ));
            }
            if name != "pixiv" && settings.refresh_token.0.is_some() {
                return Err(error(
                    "native configuration refresh-token is only supported for pixiv",
                ));
            }
            if name != "deviantart"
                && (settings.client_id.0.is_some() || settings.client_secret.0.is_some())
            {
                return Err(error(
                    "native configuration client-id and client-secret are only supported for deviantart",
                ));
            }
            let opaque_key = if matches!(name, "pixeldrain" | "gelbooru") {
                match settings.api_key.0.take() {
                    None => None,
                    Some(Value::String(key))
                        if key.len() <= 4096 && key.bytes().all(|b| b.is_ascii_graphic()) =>
                    {
                        Some(key)
                    }
                    Some(_) => {
                        return Err(error(
                            "native configuration api-key must contain at most 4096 printable ASCII characters without spaces",
                        ));
                    }
                }
            } else {
                None
            };
            // Boxed: unoptimized builds overflow the stack with inline settings.
            let mut resolved = (*settings).resolve(directory)?;
            resolved.bluesky_endpoint = bluesky_endpoint;
            if matches!(name, "pixeldrain" | "gelbooru") {
                resolved.api_key = opaque_key;
            }
            resolved.tumblr_reblogs = tumblr_reblogs;
            categories.insert(name.into(), resolved);
        }
        Ok(Self {
            global,
            extractor: defaults,
            categories,
        })
    }

    pub fn resolve(&self, category: &str) -> Result<Options> {
        if !matches!(
            category,
            "instagram"
                | "twitter"
                | "facebook"
                | "imgur"
                | "bluesky"
                | "reddit"
                | "danbooru"
                | "e621"
                | "gelbooru"
                | "safebooru"
                | "mastodon"
                | "mastodon.social"
                | "pawoo"
                | "baraag"
                | "kemono"
                | "coomer"
                | "pixeldrain"
                | "catbox"
                | "imgbox"
                | "postimg"
                | "tumblr"
                | "pixiv"
                | "deviantart"
                | "flickr"
                | "wallhaven"
                | "artstation"
                | "pinterest"
                | "newgrounds"
                | "furaffinity"
                | "weasyl"
                | "vsco"
                | "direct"
        ) {
            return Err(error("unsupported native configuration category"));
        }
        let mut settings = self.extractor.clone();
        if matches!(category, "mastodon.social" | "pawoo" | "baraag")
            && let Some(base) = self.categories.get("mastodon")
        {
            settings = settings.overlay(base);
        }
        if let Some(specific) = self.categories.get(category) {
            settings = settings.overlay(specific);
        }
        Ok(settings.overlay(&self.global))
    }
}

impl RawOptions {
    fn resolve(self, directory: &Path) -> Result<Options> {
        let primary_filter = filter_clauses(self.file_filter, "file-filter")?;
        let alias_filter = filter_clauses(self.image_filter, "image-filter")?;
        let file_filter = match primary_filter {
            Some(primary) if !primary.is_empty() => Some(primary),
            primary => alias_filter.or(primary),
        };
        let post_filter = filter_clauses(self.post_filter, "post-filter")?;
        let post_range = range_spec(self.post_range, "post-range")?;
        let child_range = range_spec(self.child_range, "child-range")?;
        let child_filter = filter_clauses(self.child_filter, "child-filter")?;
        let include = self
            .include
            .0
            .as_ref()
            .map(crate::dispatch::Include::parse)
            .transpose()
            .map_err(|_| error("invalid native configuration include selection"))?;
        let retweets = match self.retweets.0 {
            None => None,
            Some(Value::Bool(false)) => Some(crate::twitter::RetweetMode::Skip),
            Some(Value::Bool(true)) => Some(crate::twitter::RetweetMode::Repost),
            Some(Value::String(value)) if value == "original" => {
                Some(crate::twitter::RetweetMode::Original)
            }
            Some(_) => {
                return Err(error(
                    "native configuration retweets must be a boolean or original",
                ));
            }
        };
        let quoted = match self.quoted.0 {
            None => None,
            Some(Value::Bool(value)) => Some(value),
            Some(_) => return Err(error("native configuration quoted must be a boolean")),
        };
        let expand = match self.expand.0 {
            None => None,
            Some(Value::Bool(value)) => Some(value),
            Some(_) => return Err(error("native configuration expand must be a boolean")),
        };
        let unique = match self.unique.0 {
            None => None,
            Some(Value::Bool(value)) => Some(value),
            Some(_) => return Err(error("native configuration unique must be a boolean")),
        };
        let videos = match self.videos.0 {
            None => None,
            Some(Value::Bool(true)) => Some(crate::twitter::VideoMode::Files),
            Some(Value::Bool(false)) => Some(crate::twitter::VideoMode::Skip),
            Some(Value::String(value)) if value == "ytdl" => Some(crate::twitter::VideoMode::Ytdl),
            Some(_) => {
                return Err(error(
                    "native configuration videos must be a boolean or ytdl",
                ));
            }
        };
        let previews = match self.previews.0 {
            None => None,
            Some(Value::Bool(value)) => Some(value),
            Some(_) => return Err(error("native configuration previews must be a boolean")),
        };
        let mp4 = boolean(self.mp4, "mp4")?;
        let reblogs = boolean(self.reblogs, "reblogs")?;
        let posts = tumblr_posts(self.posts)?;
        let original = boolean(self.original, "original")?;
        let files = kemono_files(self.files)?;
        let duplicates = kemono_duplicates(self.duplicates)?;
        let revisions = match self.revisions.0 {
            None => None,
            Some(Value::Bool(false)) => Some(crate::kemono::RevisionMode::None),
            Some(Value::Bool(true)) => Some(crate::kemono::RevisionMode::All),
            Some(Value::String(value)) if value == "unique" => {
                Some(crate::kemono::RevisionMode::Unique)
            }
            Some(_) => {
                return Err(error(
                    "native configuration revisions must be a boolean or unique",
                ));
            }
        };
        let order_revisions = match self.order_revisions.0 {
            None => None,
            Some(Value::String(value)) => Some(match value.as_str() {
                "asc" | "reverse" => crate::kemono::RevisionOrder::Oldest,
                "desc" => crate::kemono::RevisionOrder::Newest,
                _ => {
                    return Err(error(
                        "native configuration order-revisions must be asc, reverse or desc",
                    ));
                }
            }),
            Some(_) => {
                return Err(error(
                    "native configuration order-revisions must be asc, reverse or desc",
                ));
            }
        };
        let endpoint = match self.endpoint.0 {
            None => None,
            Some(Value::String(value)) => Some(match value.as_str() {
                "posts" | "legacy" => crate::kemono::Endpoint::Posts,
                "posts+" | "legacy+" => crate::kemono::Endpoint::Expanded,
                _ => {
                    return Err(error(
                        "native configuration endpoint must be posts, legacy, posts+ or legacy+",
                    ));
                }
            }),
            Some(_) => return Err(error("native configuration endpoint must be a string")),
        };
        let favorites = match self.favorites.0 {
            None => None,
            Some(Value::String(value)) => Some(match value.as_str() {
                "artist" => crate::kemono::Favorites::Artist,
                "post" => crate::kemono::Favorites::Post,
                _ => {
                    return Err(error(
                        "native configuration favorites must be artist or post",
                    ));
                }
            }),
            Some(_) => return Err(error("native configuration favorites must be a string")),
        };
        let order_posts = match self.order_posts.0 {
            None => None,
            Some(Value::String(value)) => Some(match value.as_str() {
                "asc" => crate::kemono::PostOrder::Forward,
                "desc" | "reverse" => crate::kemono::PostOrder::Reverse,
                _ => {
                    return Err(error(
                        "native configuration order-posts must be asc, desc or reverse",
                    ));
                }
            }),
            Some(_) => return Err(error("native configuration order-posts must be a string")),
        };
        let inline = match self.inline.0 {
            None => None,
            Some(Value::Bool(true)) => Some(crate::tumblr::Inline::All),
            Some(Value::Bool(false)) => Some(crate::tumblr::Inline::Disabled),
            Some(Value::String(value)) if value == "reblog" => Some(crate::tumblr::Inline::Reblog),
            Some(Value::String(value)) if value == "original" => {
                Some(crate::tumblr::Inline::Original)
            }
            Some(_) => {
                return Err(error(
                    "native configuration inline must be a boolean, reblog or original",
                ));
            }
        };
        let username = private_string(self.username, "username", 256, true)?;
        let password = private_string(self.password, "password", 4096, false)?;
        let user_id = match self.user_id.0 {
            None => None,
            Some(Value::String(value))
                if value.is_empty()
                    || (value.len() <= 20
                        && value.bytes().all(|b| b.is_ascii_digit())
                        && value.bytes().any(|b| b != b'0')) =>
            {
                Some(value)
            }
            Some(Value::Number(value)) if value.as_u64().is_some_and(|n| n > 0) => {
                Some(value.to_string())
            }
            Some(_) => {
                return Err(error(
                    "native configuration user-id must be a positive decimal ID or empty string",
                ));
            }
        };
        let reposts = boolean(self.reposts, "reposts")?;
        let ugoira = boolean(self.ugoira, "ugoira")?;
        let access_token = match self.access_token.0 {
            None => None,
            Some(Value::Bool(false)) => Some(String::new()),
            Some(Value::String(value))
                if value != "cache"
                    && value.len() <= 4096
                    && value
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-._~+/=".contains(&b)) =>
            {
                Some(value)
            }
            Some(_) => {
                return Err(error(
                    "native configuration Mastodon access-token must be a bearer token string or false; OAuth cache discovery is not implemented",
                ));
            }
        };
        let text_posts = boolean(self.text_posts, "text-posts")?;
        let unavailable = match self.unavailable.0 {
            None => None,
            Some(Value::Bool(value)) => Some(value),
            Some(_) => return Err(error("native configuration unavailable must be a boolean")),
        };
        let text_tweets = match self.text_tweets.0 {
            None => None,
            Some(Value::Bool(value)) => Some(value),
            Some(_) => return Err(error("native configuration text-tweets must be a boolean")),
        };
        let fallback = match self.fallback.0 {
            None => None,
            Some(Value::Bool(value)) => Some(value),
            Some(_) => return Err(error("native configuration fallback must be a boolean")),
        };
        let image_size = match self.size.0 {
            None => None,
            Some(value) => {
                let invalid = || error("native configuration size requires image size names");
                let names = match value {
                    Value::String(value) if value.len() <= 32 * 33 => {
                        value.split(',').map(str::to_owned).collect()
                    }
                    Value::Array(values) if values.len() <= 32 => values
                        .into_iter()
                        .map(|value| {
                            if let Value::String(value) = value {
                                Ok(value)
                            } else {
                                Err(invalid())
                            }
                        })
                        .collect::<Result<Vec<_>>>()?,
                    _ => return Err(invalid()),
                };
                let names = names
                    .into_iter()
                    .map(|name| {
                        if name.chars().any(char::is_control) {
                            Err(invalid())
                        } else {
                            Ok(name.trim().to_owned())
                        }
                    })
                    .collect::<Result<Vec<_>>>()?;
                Some(crate::twitter::ImageOptions::new(names).map_err(|_| invalid())?)
            }
        };
        let articles = article_options(self.articles)?;
        let cards = match self.cards.0 {
            None => None,
            Some(Value::Bool(true)) => Some(crate::twitter::CardMode::Files),
            Some(Value::Bool(false)) => Some(crate::twitter::CardMode::Skip),
            Some(Value::String(value)) if value == "ytdl" => Some(crate::twitter::CardMode::Ytdl),
            Some(_) => {
                return Err(error(
                    "native configuration cards must be a boolean or ytdl",
                ));
            }
        };
        let cards_blacklist = match self.cards_blacklist.0 {
            None => None,
            Some(Value::Array(values)) if values.len() <= 256 => {
                let mut names = Vec::with_capacity(values.len());
                for value in values {
                    let Value::String(name) = value else {
                        return Err(error(
                            "native configuration cards-blacklist must contain card names",
                        ));
                    };
                    if name.is_empty() || name.len() > 2048 || name.chars().any(char::is_control) {
                        return Err(error(
                            "native configuration cards-blacklist contains an invalid card name",
                        ));
                    }
                    names.push(name);
                }
                Some(names)
            }
            Some(_) => {
                return Err(error(
                    "native configuration cards-blacklist must be a bounded array of card names",
                ));
            }
        };
        let tweet_endpoint = match self.tweet_endpoint.0 {
            None => None,
            Some(Value::String(value)) => Some(match value.as_str() {
                "auto" => crate::twitter::TweetEndpoint::Auto,
                "detail" => crate::twitter::TweetEndpoint::Detail,
                "restid" => crate::twitter::TweetEndpoint::RestId,
                _ => return Err(error("unsupported native configuration tweet-endpoint")),
            }),
            Some(_) => {
                return Err(error(
                    "native configuration tweet-endpoint must be a string",
                ));
            }
        };
        let conversations = match self.conversations.0 {
            None => None,
            Some(Value::Bool(false)) => Some(crate::twitter::ConversationMode::Off),
            Some(Value::Bool(true)) => Some(crate::twitter::ConversationMode::All),
            Some(Value::String(value)) if value == "accessible" => {
                Some(crate::twitter::ConversationMode::Accessible)
            }
            Some(_) => {
                return Err(error(
                    "native configuration conversations must be a boolean or accessible",
                ));
            }
        };
        let pinned = match self.pinned.0 {
            None => None,
            Some(Value::Bool(value)) => Some(value),
            Some(_) => return Err(error("native configuration pinned must be a boolean")),
        };
        let replies = match self.replies.0 {
            None => None,
            Some(Value::Bool(true)) => Some(crate::twitter::ReplyMode::All),
            Some(Value::Bool(false)) => Some(crate::twitter::ReplyMode::Skip),
            Some(Value::String(value)) if value == "self" => {
                Some(crate::twitter::ReplyMode::SelfOnly)
            }
            Some(_) => {
                return Err(error(
                    "native configuration replies must be a boolean or self",
                ));
            }
        };
        let strategy = match self.strategy.0 {
            None => None,
            Some(Value::String(value)) => Some(match value.as_str() {
                "auto" => crate::twitter::TimelineStrategy::Auto,
                "media" => crate::twitter::TimelineStrategy::Media,
                "tweets" => crate::twitter::TimelineStrategy::Tweets,
                "with_replies" => crate::twitter::TimelineStrategy::Replies,
                _ => return Err(error("unsupported native configuration strategy")),
            }),
            Some(_) => return Err(error("native configuration strategy must be a string")),
        };
        let search_pagination = match self.search_pagination.0 {
            None => None,
            Some(Value::String(value)) => Some(match value.as_str() {
                "max_id" | "maxid" | "id" => crate::twitter::SearchPagination::MaxId,
                "cursor" => crate::twitter::SearchPagination::Cursor,
                "until" | "date" | "datetime" | "dt" => crate::twitter::SearchPagination::Date,
                _ => return Err(error("unsupported native configuration search-pagination")),
            }),
            Some(_) => {
                return Err(error(
                    "native configuration search-pagination must be a string",
                ));
            }
        };
        let search_results = match self.search_results.0 {
            None => None,
            Some(Value::String(value)) => Some(match value.to_ascii_lowercase().as_str() {
                "latest" | "live" => crate::twitter::SearchProduct::Latest,
                "top" => crate::twitter::SearchProduct::Top,
                "media" => crate::twitter::SearchProduct::Media,
                "people" | "user" => crate::twitter::SearchProduct::People,
                "lists" | "list" => crate::twitter::SearchProduct::Lists,
                _ => return Err(error("unsupported native configuration search-results")),
            }),
            Some(_) => {
                return Err(error(
                    "native configuration search-results must be a string",
                ));
            }
        };
        let limit = bounded_integer(self.limit, 1, 100, "limit")?;
        let search_limit = bounded_integer(self.search_limit, 1, 100, "search-limit")?;
        let search_stop = match self.search_stop.0 {
            Some(Value::String(value)) if value == "auto" => Some(3),
            value => bounded_integer(Field(value), 0, 128, "search-stop")?,
        };
        let metadata = boolean(self.metadata, "metadata")?;
        let max_posts = match self.max_posts.0 {
            None => None,
            Some(Value::Number(value)) => Some(
                value
                    .as_u64()
                    .and_then(|n| usize::try_from(n).ok())
                    .ok_or_else(|| {
                        error("native configuration max-posts must be a nonnegative integer")
                    })?,
            ),
            Some(_) => {
                return Err(error(
                    "native configuration max-posts must be a nonnegative integer",
                ));
            }
        };
        let api_key = match self.api_key.0 {
            None => None,
            Some(Value::String(key))
                if (1..=128).contains(&key.len())
                    && key.bytes().all(|b| b.is_ascii_alphanumeric()) =>
            {
                Some(key)
            }
            Some(_) => {
                return Err(error(
                    "native configuration api-key must be 1-128 ASCII letters or digits",
                ));
            }
        };
        let refresh_token = match self.refresh_token.0 {
            None => None,
            Some(Value::String(token))
                if (1..=256).contains(&token.len())
                    && token
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')) =>
            {
                Some(token)
            }
            Some(_) => {
                return Err(error(
                    "native configuration refresh-token must be 1-256 ASCII letters, digits, '-', '_' or '.'",
                ));
            }
        };
        let credential = |field: Field, name: &str, digits: bool| match field.0 {
            None => Ok(None),
            Some(Value::String(value))
                if (1..=128).contains(&value.len())
                    && value.bytes().all(|b| {
                        if digits {
                            b.is_ascii_digit()
                        } else {
                            b.is_ascii_alphanumeric()
                        }
                    }) =>
            {
                Ok(Some(value))
            }
            Some(_) => Err(error(format!(
                "native configuration {name} must be 1-128 ASCII {}",
                if digits {
                    "digits"
                } else {
                    "letters or digits"
                }
            ))),
        };
        let client_id = credential(self.client_id, "client-id", true)?;
        let client_secret = credential(self.client_secret, "client-secret", false)?;
        let file_range = range_spec(self.file_range, "file-range")?;
        let (filename, filename_conditions) = match self.filename.0 {
            None => (None, None),
            Some(Value::String(value)) => {
                FilenameTemplate::parse(&value)
                    .map_err(|_| error("unsupported native configuration filename template"))?;
                (Some(value), None)
            }
            Some(Value::Object(entries)) => {
                let mut default = None;
                let mut conditions = Vec::new();
                for (filter, template) in entries {
                    let template = template.as_str().map(str::to_owned).ok_or_else(|| {
                        error(
                            "native configuration filename conditions must map to string templates",
                        )
                    })?;
                    if filter.is_empty() {
                        default = Some(template);
                    } else {
                        conditions.push((filter, template));
                    }
                }
                FilenameTemplate::parse_conditional(&conditions, default.as_deref())
                    .map_err(|_| error("unsupported native configuration filename template"))?;
                (default, Some(conditions))
            }
            Some(_) => {
                return Err(error(
                    "native configuration filename must be a supported string template",
                ));
            }
        };
        let components = |value: Value| -> Result<Vec<String>> {
            let invalid =
                || error("native configuration directory must be an array of string templates");
            let Value::Array(values) = value else {
                return Err(invalid());
            };
            values
                .into_iter()
                .map(|value| value.as_str().map(str::to_owned).ok_or_else(invalid))
                .collect()
        };
        let (templates, directory_conditions) = match self.directory.0 {
            None => (None, None),
            Some(Value::Object(entries)) => {
                let mut default = None;
                let mut conditions = Vec::new();
                for (filter, value) in entries {
                    let value = components(value)?;
                    if filter.is_empty() {
                        default = Some(value);
                    } else {
                        conditions.push((filter, value));
                    }
                }
                DirectoryTemplate::parse_conditional(&conditions, default.as_deref())
                    .map_err(|_| error("unsupported native configuration directory template"))?;
                (default, Some(conditions))
            }
            Some(value) => {
                let value = components(value)?;
                DirectoryTemplate::parse(&value)
                    .map_err(|_| error("unsupported native configuration directory template"))?;
                (Some(value), None)
            }
        };
        let retries = match self.retries.0 {
            None => None,
            Some(value) => Some(value.as_u64().filter(|value| *value <= 10).ok_or_else(|| {
                error("native configuration retries must be an integer from 0 to 10")
            })? as u8),
        };
        let timeout = match self.timeout.0 {
            None => None,
            Some(value) => {
                let seconds = value
                    .as_f64()
                    .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
                    .ok_or_else(|| {
                        error("native configuration timeout must be positive finite seconds")
                    })?;
                let timeout = Duration::try_from_secs_f64(seconds)
                    .map_err(|_| error("native configuration timeout is out of range"))?;
                if timeout.is_zero() || std::time::Instant::now().checked_add(timeout).is_none() {
                    return Err(error(
                        "native configuration timeout is outside the clock range or precision",
                    ));
                }
                Some(timeout)
            }
        };
        let proxy = network_string(self.proxy)?;
        let api_server = match self.api_server.0 {
            None => None,
            Some(Value::Null | Value::Bool(false)) => Some(String::new()),
            Some(Value::String(value)) if value.is_empty() => Some(value),
            Some(Value::String(value))
                if value.len() <= 4096
                    && value.trim() == value
                    && !value.chars().any(char::is_control) =>
            {
                let url = url::Url::parse(&value)
                    .map_err(|_| error("invalid native configuration api-server"))?;
                if !matches!(url.scheme(), "http" | "https")
                    || url.host_str().is_none()
                    || !url.username().is_empty()
                    || url.password().is_some()
                    || url.query().is_some()
                    || url.fragment().is_some()
                {
                    return Err(error("invalid native configuration api-server"));
                }
                Some(value)
            }
            Some(_) => return Err(error("invalid native configuration api-server")),
        };
        let user_agent = network_string(self.user_agent)?;
        NetworkOptions::new(proxy.clone(), user_agent.clone())?;
        let (cookies, browser_cookies) = cookie_source(self.cookies, directory)?;
        Ok(Options {
            api_server,
            bluesky_endpoint: None,
            proxy,
            user_agent,
            base_directory: path(self.base_directory, directory, "base-directory")?,
            cookies,
            browser_cookies,
            archive: path(self.archive, directory, "archive")?,
            filename,
            filename_conditions,
            file_range,
            file_filter,
            post_range,
            post_filter,
            child_range,
            child_filter,
            include,
            date_before: date(self.date_before)?,
            date_after: date(self.date_after)?,
            retweets,
            quoted,
            expand,
            unique,
            videos,
            previews,
            mp4,
            reblogs,
            tumblr_reblogs: None,
            posts,
            inline,
            original,
            files,
            duplicates,
            revisions,
            order_revisions,
            endpoint,
            order_posts,
            favorites,
            username,
            password,
            user_id,
            reposts,
            ugoira,
            access_token,
            text_posts,
            unavailable,
            text_tweets,
            size: image_size,
            fallback,
            articles,
            cards,
            cards_blacklist,
            tweet_endpoint,
            conversations,
            pinned,
            replies,
            strategy,
            search_pagination,
            search_results,
            limit,
            search_limit,
            search_stop,
            max_posts,
            metadata,
            api_key,
            refresh_token,
            client_id,
            client_secret,
            directory: templates,
            directory_conditions,
            filesize_min: size(self.filesize_min)?,
            filesize_max: size(self.filesize_max)?,
            retries,
            timeout,
        })
    }
}

fn range_spec(field: Field, name: &str) -> Result<Option<String>> {
    let Some(value) = field.0 else {
        return Ok(None);
    };
    let invalid = || {
        error(format!(
            "native configuration {name} must be a supported range string or list of strings"
        ))
    };
    let spec = match value {
        Value::String(spec) if spec.is_empty() => ":".into(),
        Value::String(spec) => spec,
        Value::Array(values) if values.is_empty() => ":".into(),
        Value::Array(values) if values.len() <= 256 => values
            .iter()
            .map(|value| value.as_str().ok_or_else(invalid))
            .collect::<Result<Vec<_>>>()?
            .join(","),
        _ => return Err(invalid()),
    };
    RangeSelection::parse(&spec).map_err(|_| invalid())?;
    Ok(Some(spec))
}

fn filter_clauses(field: Field, name: &str) -> Result<Option<Vec<String>>> {
    let invalid = || {
        error(format!(
            "native configuration {name} requires valid bounded expressions"
        ))
    };
    let Some(value) = field.0 else {
        return Ok(None);
    };
    let clauses = match value {
        Value::String(value) if value.is_empty() => Vec::new(),
        Value::String(value) => vec![value],
        Value::Array(values) if values.len() <= 32 => values
            .into_iter()
            .map(|value| {
                if let Value::String(value) = value {
                    Ok(value)
                } else {
                    Err(invalid())
                }
            })
            .collect::<Result<Vec<_>>>()?,
        _ => return Err(invalid()),
    };
    let mut bytes = 0usize;
    for clause in &clauses {
        bytes = bytes.checked_add(clause.len()).ok_or_else(invalid)?;
        if clause.is_empty() || clause.len() > 16 * 1024 || bytes > 64 * 1024 {
            return Err(invalid());
        }
        crate::filter::FilterExpression::parse(clause).map_err(|_| invalid())?;
    }
    Ok(Some(clauses))
}

fn article_options(field: Field) -> Result<Option<crate::twitter::ArticleOptions>> {
    use crate::twitter::ArticleOptions;
    let invalid = || error("native configuration articles requires a boolean or article flags");
    let tokens = match field.0 {
        None => return Ok(None),
        Some(Value::Bool(true)) => return Ok(Some(ArticleOptions::default())),
        Some(Value::Bool(false)) => return Ok(Some(ArticleOptions::disabled())),
        Some(Value::String(value)) => {
            if value.len() > 32 * 129 || value.chars().any(char::is_control) {
                return Err(invalid());
            }
            if value.trim().is_empty() {
                Vec::new()
            } else {
                value.split(',').map(str::to_owned).collect()
            }
        }
        Some(Value::Array(values)) if values.len() <= 32 => values
            .into_iter()
            .map(|value| {
                if let Value::String(value) = value {
                    Ok(value)
                } else {
                    Err(invalid())
                }
            })
            .collect::<Result<Vec<_>>>()?,
        Some(_) => return Err(invalid()),
    };
    if tokens.len() > 32 {
        return Err(invalid());
    }
    let mut options = ArticleOptions::disabled();
    for token in tokens {
        if token.len() > 128 || token.chars().any(char::is_control) {
            return Err(invalid());
        }
        match token.trim() {
            "meta" | "metadata" => options.metadata = true,
            "cover" => options.cover = true,
            "media" => options.media = true,
            "html" => options.html = true,
            "doc" | "document" => options.document = true,
            _ => return Err(invalid()),
        }
    }
    Ok(Some(options))
}

fn cookie_source(field: Field, directory: &Path) -> Result<(Option<PathBuf>, Option<PathBuf>)> {
    let Some(value) = field.0 else {
        return Ok((None, None));
    };
    if let Value::Array(values) = value {
        let invalid = || {
            error(
                "native configuration cookies browser source requires an explicit Firefox profile without keyring or container selection",
            )
        };
        if !(2..=4).contains(&values.len())
            || values[0].as_str() != Some("firefox")
            || values.get(2).is_some_and(|value| !value.is_null())
            || values
                .get(3)
                .is_some_and(|value| !value.is_null() && value.as_str() != Some("none"))
        {
            return Err(invalid());
        }
        let profile =
            path(Field(Some(values[1].clone())), directory, "cookies")?.ok_or_else(invalid)?;
        // Keep configuration and the CLI's explicit profile grammar aligned.
        let profile_text = profile.to_str().ok_or_else(invalid)?;
        crate::browser::FirefoxProfile::parse(&format!("firefox:{profile_text}"))
            .map_err(|_| invalid())?;
        Ok((None, Some(profile)))
    } else {
        Ok((path(Field(Some(value)), directory, "cookies")?, None))
    }
}

fn path(field: Field, directory: &Path, name: &str) -> Result<Option<PathBuf>> {
    let Some(value) = field.0 else {
        return Ok(None);
    };
    let invalid = || {
        error(format!(
            "native configuration {name} must be a nonempty literal path without expansion"
        ))
    };
    let value = value.as_str().ok_or_else(invalid)?;
    if value.is_empty()
        || value.chars().any(char::is_control)
        || value.starts_with('~')
        || value.contains('$')
        || value.contains(['{', '}'])
        || value.contains("://")
    {
        return Err(invalid());
    }
    let value = Path::new(value);
    Ok(Some(if value.is_absolute() {
        value.into()
    } else {
        directory.join(value)
    }))
}

fn size(field: Field) -> Result<Option<u64>> {
    let Some(value) = field.0 else {
        return Ok(None);
    };
    let invalid = || {
        error(
            "native configuration file size limit must be nonnegative bytes or a supported binary size string",
        )
    };
    match value {
        Value::Number(number) => number
            .as_u64()
            .map(Ok)
            .unwrap_or_else(|| parse_bytes(&number.to_string()))
            .map(Some)
            .map_err(|_| invalid()),
        Value::String(value) => parse_bytes(&value).map(Some).map_err(|_| invalid()),
        _ => Err(invalid()),
    }
}

fn date(field: Field) -> Result<Option<i64>> {
    let Some(value) = field.0 else {
        return Ok(None);
    };
    let invalid = || {
        error(
            "native configuration date limit must be a supported ISO date or integer Unix timestamp",
        )
    };
    match value {
        Value::String(value) => parse_date(&value).map(Some).map_err(|_| invalid()),
        Value::Number(value) => value.as_i64().map(Some).ok_or_else(invalid),
        _ => Err(invalid()),
    }
}

fn network_string(field: Field) -> Result<Option<String>> {
    match field.0 {
        None => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => Err(error("native network settings must be strings")),
    }
}

fn bounded_integer(field: Field, min: u64, max: u64, name: &str) -> Result<Option<u8>> {
    match field.0 {
        None => Ok(None),
        Some(Value::Number(value)) => value
            .as_u64()
            .filter(|value| (min..=max).contains(value))
            .map(|value| Some(value as u8))
            .ok_or_else(|| {
                error(format!(
                    "native configuration {name} must be an integer from {min} to {max}"
                ))
            }),
        Some(_) => Err(error(format!(
            "native configuration {name} must be an integer from {min} to {max}"
        ))),
    }
}

fn boolean(field: Field, name: &str) -> Result<Option<bool>> {
    match field.0 {
        None => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(value)),
        Some(_) => Err(error(format!(
            "native configuration {name} must be a boolean"
        ))),
    }
}

fn private_string(field: Field, name: &str, max: usize, username: bool) -> Result<Option<String>> {
    match field.0 {
        None => Ok(None),
        Some(Value::String(value))
            if value.len() <= max
                && !value.chars().any(char::is_control)
                && !(username && value.contains(':')) =>
        {
            Ok(Some(value))
        }
        Some(_) => Err(error(format!(
            "native configuration {name} must be a bounded string without control characters"
        ))),
    }
}

fn tumblr_posts(field: Field) -> Result<Option<Vec<String>>> {
    let invalid =
        || error("native configuration posts must be all, false, or supported Tumblr post types");
    let types = match field.0 {
        None => return Ok(None),
        Some(Value::String(value)) if value == "all" => return Ok(Some(vec![value])),
        Some(Value::Bool(false)) => Vec::new(),
        Some(Value::String(value)) if value.len() <= 1024 => {
            if value.is_empty() {
                Vec::new()
            } else {
                value.split(',').map(|s| s.trim().to_owned()).collect()
            }
        }
        Some(Value::Array(values)) if values.len() <= 64 => values
            .into_iter()
            .map(|value| value.as_str().map(str::to_owned).ok_or_else(invalid))
            .collect::<Result<Vec<_>>>()?,
        _ => return Err(invalid()),
    };
    crate::tumblr::Selection::default()
        .with_posts(types.clone())
        .map_err(|_| invalid())?;
    Ok(Some(types))
}

fn kemono_files(field: Field) -> Result<Option<Vec<crate::kemono::FileKind>>> {
    use crate::kemono::FileKind;
    let Some(value) = field.0 else {
        return Ok(None);
    };
    let invalid = || {
        error(
            "native configuration files must be a comma-separated string or list of file, attachments, inline",
        )
    };
    let names = selection_names(value).ok_or_else(invalid)?;
    let files = names
        .into_iter()
        .map(|name| match name.as_str() {
            "file" => Ok(FileKind::File),
            "attachments" => Ok(FileKind::Attachments),
            "inline" => Ok(FileKind::Inline),
            _ => Err(invalid()),
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Some(files))
}

fn kemono_duplicates(field: Field) -> Result<Option<crate::kemono::DuplicateOptions>> {
    use crate::kemono::DuplicateOptions;
    let Some(value) = field.0 else {
        return Ok(None);
    };
    match value {
        Value::Bool(true) => return Ok(Some(DuplicateOptions::all())),
        Value::Bool(false) => return Ok(Some(DuplicateOptions::default())),
        _ => {}
    }
    let invalid = || {
        error(
            "native configuration duplicates must be a boolean, comma-separated string or list of file, attachment, inline",
        )
    };
    let names = if value.as_str() == Some("") {
        Vec::new()
    } else {
        selection_names(value).ok_or_else(invalid)?
    };
    let mut selected = DuplicateOptions::default();
    for name in names {
        match name.as_str() {
            "file" => selected.file = true,
            "attachment" => selected.attachment = true,
            "inline" => selected.inline = true,
            _ => return Err(invalid()),
        }
    }
    Ok(Some(selected))
}

fn selection_names(value: Value) -> Option<Vec<String>> {
    match value {
        Value::String(value) if value.len() <= 1024 => Some(
            value
                .split(',')
                .map(|name| name.trim().to_owned())
                .collect(),
        ),
        Value::Array(names) if names.len() <= 64 => names
            .into_iter()
            .map(|name| name.as_str().map(str::to_owned))
            .collect(),
        _ => None,
    }
}
