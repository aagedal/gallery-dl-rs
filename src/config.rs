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
    pub date_before: Option<i64>,
    pub date_after: Option<i64>,
    /// Whether to include reposts, optionally under the original author.
    pub retweets: Option<crate::twitter::RetweetMode>,
    /// Include media from quoted posts.
    pub quoted: Option<bool>,
    /// Expand each source post's conversation once per input.
    pub expand: Option<bool>,
    /// Suppress duplicate Twitter posts.
    pub unique: Option<bool>,
    /// Select native video files, skip videos, or delegate video pages.
    pub videos: Option<crate::twitter::VideoMode>,
    /// Include video and animated-GIF preview images.
    pub previews: Option<bool>,
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
    /// Return a copy with every present field in `overrides` taking precedence.
    /// This can apply explicit CLI values after configuration resolution.
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
            date_before: overrides.date_before.or(self.date_before),
            date_after: overrides.date_after.or(self.date_after),
            retweets: overrides.retweets.or(self.retweets),
            quoted: overrides.quoted.or(self.quoted),
            expand: overrides.expand.or(self.expand),
            unique: overrides.unique.or(self.unique),
            videos: overrides.videos.or(self.videos),
            previews: overrides.previews.or(self.previews),
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
    date_before: Field,
    date_after: Field,
    retweets: Field,
    quoted: Field,
    expand: Field,
    unique: Field,
    videos: Field,
    previews: Field,
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
    date_before: Field,
    date_after: Field,
    retweets: Field,
    quoted: Field,
    expand: Field,
    unique: Field,
    videos: Field,
    previews: Field,
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
    kemono: Box<RawOptions>,
    coomer: Box<RawOptions>,
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
    date_before: Field,
    date_after: Field,
    retweets: Field,
    quoted: Field,
    expand: Field,
    unique: Field,
    videos: Field,
    previews: Field,
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
                "kemono",
                "coomer",
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
            date_before: root.date_before,
            date_after: root.date_after,
            retweets: root.retweets,
            quoted: root.quoted,
            expand: root.expand,
            unique: root.unique,
            videos: root.videos,
            previews: root.previews,
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
            date_before: extractor.date_before,
            date_after: extractor.date_after,
            retweets: extractor.retweets,
            quoted: extractor.quoted,
            expand: extractor.expand,
            unique: extractor.unique,
            videos: extractor.videos,
            previews: extractor.previews,
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
        for (name, settings) in [
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
            ("kemono", extractor.kemono),
            ("coomer", extractor.coomer),
            ("direct", extractor.direct),
        ] {
            // Boxed: unoptimized builds overflow the stack with inline settings.
            categories.insert(name.into(), (*settings).resolve(directory)?);
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
                | "kemono"
                | "coomer"
                | "direct"
        ) {
            return Err(error("unsupported native configuration category"));
        }
        let settings = self.categories.get(category).map_or_else(
            || self.extractor.clone(),
            |specific| self.extractor.overlay(specific),
        );
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
        let user_agent = network_string(self.user_agent)?;
        NetworkOptions::new(proxy.clone(), user_agent.clone())?;
        let (cookies, browser_cookies) = cookie_source(self.cookies, directory)?;
        Ok(Options {
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
            date_before: date(self.date_before)?,
            date_after: date(self.date_after)?,
            retweets,
            quoted,
            expand,
            unique,
            videos,
            previews,
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
