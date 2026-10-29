//! Explicit native configuration: a strict subset of gallery-dl's JSON schema.
//!
//! See <https://gdl-org.github.io/docs/configuration.html>. Extractor defaults
//! are overridden by category settings, then by top-level global settings.
//! Paths are literal, relative to the configuration directory; expansion,
//! unsupported hooks, subcategories, and unsupported options are rejected.
use crate::{
    Result,
    date::source_date_bound,
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

/// Upstream extractor settings of dedicated native extractors that are not
/// typed options; they reach the extractor through `site_options`.
const NATIVE_SITE_OPTIONS: &[(&str, &[&str])] = &[
    (
        "reddit",
        &[
            "comments",
            "morecomments",
            "recursion",
            "videos",
            "previews",
            "embeds",
            "selftext",
            "only",
            "id-min",
            "id-max",
            "limit",
            "parent-directory",
        ],
    ),
    ("pinterest", &["sections", "stories", "videos", "domain"]),
    (
        "tumblr",
        &[
            "external",
            "avatar",
            "offset",
            "pagination",
            "fallback-delay",
            "fallback-retries",
            "ratelimit",
        ],
    ),
];

/// Sparse settings: absent fields leave the caller's defaults unchanged.
#[derive(Clone, Default, PartialEq)]
pub struct Options {
    /// Initialize job state before extraction; exact source string "lazy" disables it.
    pub init: Option<bool>,
    pub postprocessors: Option<crate::postprocess::Configuration>,
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
    /// A present invalid/falsy source bound clears less specific settings.
    pub date_before_set: bool,
    pub date_after_set: bool,
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
    /// Danbooru pool member ordering, independent of Kemono post ordering.
    pub booru_order_posts: Option<crate::booru::PoolOrder>,
    /// Gelbooru favorites retain source JSON types and first-element rules.
    pub booru_favorite_order: Option<Value>,
    /// Raw common inheritance keys; the primary key wins by presence.
    pub parent_metadata: Option<std::sync::Arc<Value>>,
    pub metadata_parent: Option<std::sync::Arc<Value>>,
    pub favorites: Option<crate::kemono::Favorites>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub user_id: Option<String>,
    /// Include Bluesky reposts in author feeds.
    pub reposts: Option<bool>,
    /// Select Pixiv ugoira ZIPs, individual original frames, or skip animations.
    pub ugoira: Option<crate::pixiv::UgoiraMode>,
    /// Pixiv tag names: Japanese strings, original objects or translated strings.
    pub tags: Option<crate::pixiv::TagMode>,
    /// Initial Pixiv bookmark cursor; explicit null clears inherited values.
    pub cursor: Option<crate::pixiv::BookmarkCursor>,
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
    /// Limit Kemono/Coomer or Pixiv source works; zero clears an inherited limit.
    pub max_posts: Option<usize>,
    /// Danbooru/e621 pagination threshold. Outer None is absent; inner None
    /// explicitly restores the source's per-page default for noninteger input.
    pub booru_threshold: Option<Option<usize>>,
    /// Look up Kemono/Coomer creator profiles or Pixiv author metadata.
    pub metadata: Option<bool>,
    /// Include Pixiv illustration or novel comments.
    pub comments: Option<bool>,
    /// Include Pixiv illustration HTML captions as inline text files.
    pub captions: Option<bool>,
    /// Enable the Pixiv workaround for App API sanity restrictions.
    pub sanity: Option<bool>,
    /// Expand images embedded in Pixiv novels.
    pub embeds: Option<bool>,
    /// Include Pixiv illustration or novel bookmark metadata.
    pub metadata_bookmark: Option<bool>,
    /// API key for supported API-based extractors, including Pixeldrain.
    pub api_key: Option<String>,
    /// Pixiv OAuth refresh token; accepted in pixiv and pixiv-novel scopes.
    pub refresh_token: Option<String>,
    pub covers: Option<bool>,
    pub full_series: Option<bool>,
    /// DeviantArt OAuth client ID; only accepted in the `deviantart` category.
    pub client_id: Option<String>,
    /// DeviantArt OAuth client secret; only accepted in the `deviantart` category.
    pub client_secret: Option<String>,
    /// Relative directory components; an empty list selects the base directory.
    pub directory: Option<Vec<String>>,
    /// Ordered `(filter, components)` pairs from an object-valued `directory`.
    pub directory_conditions: Option<Vec<(String, Vec<String>)>>,
    pub cookies: Option<PathBuf>,
    /// Browser cookie specification (`firefox:PATH`, `helium:Profile 1`, ...)
    /// selected by an upstream-style `cookies` array.
    /// Mutually exclusive with the cookie file during scope resolution.
    pub browser_cookies: Option<String>,
    pub proxy: Option<String>,
    pub user_agent: Option<String>,
    /// Uses gallery-dl-rs's archive identities, not upstream database IDs.
    pub archive: Option<PathBuf>,
    pub filesize_min: Option<u64>,
    pub filesize_max: Option<u64>,
    pub retries: Option<u8>,
    pub timeout: Option<Duration>,
    /// Site-specific settings for registry sites (`crate::sites`), keyed by
    /// their upstream option names and passed through unvalidated.
    pub site_options: Option<std::sync::Arc<serde_json::Map<String, Value>>>,
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

    /// Select the primary inheritance key by presence, including explicit null.
    pub fn artist_parent_metadata(&self) -> Option<&Value> {
        self.parent_metadata
            .as_deref()
            .or(self.metadata_parent.as_deref())
    }
    pub fn artist_parent_keywords(&self) -> Vec<String> {
        match self.artist_parent_metadata() {
            Some(Value::String(key)) if !key.is_empty() => vec![key.clone()],
            None => vec!["_parent".into()],
            _ => Vec::new(),
        }
    }
    /// Return a copy with every present field in `overrides` taking precedence.
    /// This can apply explicit CLI values after configuration resolution.
    /// Reblog fields are one setting; an explicit Tumblr mode takes precedence
    /// when a programmatic override supplies both representations.
    pub fn overlay(&self, overrides: &Self) -> Self {
        Self {
            init: overrides.init.or(self.init),
            postprocessors: match (&self.postprocessors, &overrides.postprocessors) {
                (Some(base), Some(specific)) => Some(base.overlaid(specific)),
                (base, specific) => specific.clone().or_else(|| base.clone()),
            },
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
            date_before: if overrides.date_before_set {
                overrides.date_before
            } else {
                overrides.date_before.or(self.date_before)
            },
            date_after: if overrides.date_after_set {
                overrides.date_after
            } else {
                overrides.date_after.or(self.date_after)
            },
            date_before_set: self.date_before_set || overrides.date_before_set,
            date_after_set: self.date_after_set || overrides.date_after_set,
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
            booru_order_posts: overrides
                .booru_order_posts
                .or_else(|| {
                    overrides.order_posts.map(|order| match order {
                        crate::kemono::PostOrder::Forward => crate::booru::PoolOrder::Pool,
                        crate::kemono::PostOrder::Reverse => crate::booru::PoolOrder::PoolReverse,
                    })
                })
                .or(self.booru_order_posts),
            booru_favorite_order: overrides
                .booru_favorite_order
                .clone()
                .or_else(|| {
                    overrides.order_posts.map(|order| match order {
                        crate::kemono::PostOrder::Forward => Value::String("asc".into()),
                        crate::kemono::PostOrder::Reverse => Value::String("desc".into()),
                    })
                })
                .or_else(|| self.booru_favorite_order.clone()),
            parent_metadata: overrides
                .parent_metadata
                .clone()
                .or_else(|| self.parent_metadata.clone()),
            metadata_parent: overrides
                .metadata_parent
                .clone()
                .or_else(|| self.metadata_parent.clone()),
            favorites: overrides.favorites.or(self.favorites),
            username: overrides.username.clone().or_else(|| self.username.clone()),
            password: overrides.password.clone().or_else(|| self.password.clone()),
            user_id: overrides.user_id.clone().or_else(|| self.user_id.clone()),
            reposts: overrides.reposts.or(self.reposts),
            ugoira: overrides.ugoira.or(self.ugoira),
            tags: overrides.tags.or(self.tags),
            cursor: overrides.cursor.clone().or_else(|| self.cursor.clone()),
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
            booru_threshold: overrides.booru_threshold.or(self.booru_threshold),
            metadata: overrides.metadata.or(self.metadata),
            comments: overrides.comments.or(self.comments),
            captions: overrides.captions.or(self.captions),
            sanity: overrides.sanity.or(self.sanity),
            embeds: overrides.embeds.or(self.embeds),
            metadata_bookmark: overrides.metadata_bookmark.or(self.metadata_bookmark),
            api_key: overrides.api_key.clone().or_else(|| self.api_key.clone()),
            refresh_token: overrides
                .refresh_token
                .clone()
                .or_else(|| self.refresh_token.clone()),
            covers: overrides.covers.or(self.covers),
            full_series: overrides.full_series.or(self.full_series),
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
            site_options: match (&self.site_options, &overrides.site_options) {
                (Some(base), Some(specific)) => {
                    let mut merged = (**base).clone();
                    merged.extend(specific.iter().map(|(k, v)| (k.clone(), v.clone())));
                    Some(std::sync::Arc::new(merged))
                }
                (base, specific) => specific.clone().or_else(|| base.clone()),
            },
        }
    }
}

/// One `-o KEY=VALUE` setting: dotted path, final key and JSON value.
#[derive(Clone)]
pub struct CliOption {
    pub path: Vec<String>,
    pub key: String,
    pub value: Value,
    /// The original argument, for forwarding to the compatibility backend.
    pub raw: String,
    /// Append to a list setting instead of replacing it.
    pub append: bool,
}

impl CliOption {
    /// Append one postprocessor definition to the root `postprocessors` list.
    pub fn postprocessor(value: Value) -> Self {
        Self {
            path: Vec::new(),
            key: "postprocessors".into(),
            value,
            raw: String::new(),
            append: true,
        }
    }
}

impl std::fmt::Debug for CliOption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CliOption { [redacted] }")
    }
}

/// Parse `KEY=VALUE` like upstream: VALUE is JSON when it parses, otherwise
/// text; KEY is split on dots into a path and final key.
pub fn parse_option(option: &str) -> Result<CliOption> {
    let (key, value) = option.split_once('=').unwrap_or((option, ""));
    if key.len() > 1024 || value.len() > MAX_CONFIG_BYTES {
        return Err(error("option exceeds size limits"));
    }
    let value = serde_json::from_str(value).unwrap_or_else(|_| Value::String(value.into()));
    let mut path: Vec<String> = key.split('.').map(str::to_owned).collect();
    let key = path.pop().unwrap_or_default();
    Ok(CliOption {
        path,
        key,
        value,
        raw: option.to_owned(),
        append: false,
    })
}

/// Upstream `config.remap_categories`: settings of renamed categories
/// (`extractor.config-map`, by default e.g. `kemonoparty` → `kemono`) apply
/// to the new name unless it has its own settings.
fn remap_categories(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut root: Value = serde_json::from_slice(bytes).ok()?;
    let extractor = root.get_mut("extractor")?.as_object_mut()?;
    let had_map = extractor.contains_key("config-map");
    let pairs: Vec<(String, String)> = match extractor.remove("config-map") {
        None | Some(Value::Null) => [
            ("coomerparty", "coomer"),
            ("kemonoparty", "kemono"),
            ("giantessbooru", "sizebooru"),
            ("naver", "naver-blog"),
            ("chzzk", "naver-chzzk"),
            ("naverwebtoon", "naver-webtoon"),
            ("pixiv", "pixiv-novel"),
            ("saint", "turbo"),
        ]
        .into_iter()
        .map(|(a, b)| (a.to_owned(), b.to_owned()))
        .collect(),
        Some(Value::Object(map)) => map
            .into_iter()
            .filter_map(|(old, new)| Some((old, new.as_str()?.to_owned())))
            .collect(),
        Some(Value::Array(list)) => list
            .into_iter()
            .filter_map(|pair| {
                let pair = pair.as_array()?;
                Some((
                    pair.first()?.as_str()?.to_owned(),
                    pair.get(1)?.as_str()?.to_owned(),
                ))
            })
            .collect(),
        Some(_) => Vec::new(),
    };
    let mut changed = had_map;
    for (old, new) in pairs {
        if let Some(value) = extractor.get(&old).cloned()
            && !extractor.contains_key(&new)
        {
            extractor.insert(new, value);
            changed = true;
        }
    }
    // Upstream `cookies-select`: choose one of several cookie sources.
    fn select_cookies(scope: &mut serde_json::Map<String, Value>, inherited: Option<&str>) -> bool {
        let select = scope
            .remove("cookies-select")
            .and_then(|v| v.as_str().map(str::to_owned))
            .or_else(|| inherited.map(str::to_owned));
        let Some(select) = select else { return false };
        let Some(Value::Array(sources)) = scope.get("cookies") else {
            return false;
        };
        if sources.is_empty() {
            return false;
        }
        // Each process is one run of the category's extractor: `rotate`
        // starts with the first source, `random` picks any.
        let index = if select == "rotate" {
            0
        } else {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos() as usize)
                .unwrap_or(0);
            nanos % sources.len()
        };
        let chosen = sources[index].clone();
        scope.insert("cookies".into(), chosen);
        true
    }
    let inherited = extractor
        .get("cookies-select")
        .and_then(Value::as_str)
        .map(str::to_owned);
    changed |= select_cookies(extractor, None);
    for value in extractor.values_mut() {
        if let Value::Object(scope) = value {
            changed |= select_cookies(scope, inherited.as_deref());
        }
    }
    // Rewriting would hide duplicate keys from later validation.
    changed.then(|| serde_json::to_vec(&root).ok()).flatten()
}

/// Process-wide upstream settings read from the root or `extractor` scope
/// (the `extractor` value wins), applied once rather than per category.
pub const GLOBAL_KEYS: &[&str] = &[
    "actions",
    "category-map",
    "archive-event",
    "archive-mode",
    "skip-filter",
    "retry-codes",
    "keywords-default",
    "path-restrict",
    "path-replace",
    "path-remove",
    "path-strip",
    "skip",
    "download",
    "cookies-update",
    "extension-map",
    "geo-bypass",
    "netrc",
    "sleep",
    "sleep-extractor",
    "sleep-skip",
    "source-address",
    "proxy-env",
    "tls12",
    "ciphers",
    "truststore",
    "metadata-http",
    "http-metadata",
    "tags-blacklist",
    "tags-whitelist",
    "blacklist-tags",
    "whitelist-tags",
];

/// Field names (after renaming) of a `Deserialize` struct, captured from
/// serde's generated `deserialize_struct` call.
fn struct_fields<T: serde::de::DeserializeOwned>() -> &'static [&'static str] {
    use serde::de::{Error, Visitor, value::Error as E};
    struct Capture(std::cell::Cell<&'static [&'static str]>);
    impl<'de> Deserializer<'de> for &Capture {
        type Error = E;
        fn deserialize_any<V: Visitor<'de>>(self, _: V) -> std::result::Result<V::Value, E> {
            Err(E::custom("capture"))
        }
        fn deserialize_struct<V: Visitor<'de>>(
            self,
            _: &'static str,
            fields: &'static [&'static str],
            _: V,
        ) -> std::result::Result<V::Value, E> {
            self.0.set(fields);
            Err(E::custom("capture"))
        }
        serde::forward_to_deserialize_any! {
            bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
            bytes byte_buf option unit unit_struct newtype_struct seq tuple
            tuple_struct map enum identifier ignored_any
        }
    }
    let capture = Capture(std::cell::Cell::new(&[]));
    let _ = T::deserialize(&capture);
    capture.0.get()
}

/// Move keys of `object` outside `known` into a map: upstream ignores
/// options it does not know, and site options inherit like its
/// `config.interpolate`.
fn take_unknown(
    object: &mut serde_json::Map<String, Value>,
    known: &[&str],
) -> serde_json::Map<String, Value> {
    // Credentials stay confined to their categories: they remain in place so
    // that typed decoding rejects them outside a supported scope.
    const CONFINED: &[&str] = &[
        "username",
        "password",
        "api-key",
        "api-server",
        "access-token",
        "refresh-token",
        "client-id",
        "client-secret",
        "user-id",
    ];
    let unknown: Vec<String> = object
        .keys()
        .filter(|key| !known.contains(&key.as_str()) && !CONFINED.contains(&key.as_str()))
        .cloned()
        .collect();
    unknown
        .into_iter()
        .filter_map(|key| object.remove_entry(&key))
        .collect()
}

/// Configuration file formats, like upstream's `--config-json`,
/// `--config-yaml` and `--config-toml`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Json,
    Yaml,
    Toml,
}

static FORMATS: std::sync::Mutex<Vec<(PathBuf, Format)>> = std::sync::Mutex::new(Vec::new());

/// Declare the format of a configuration file regardless of its extension.
pub fn set_format(path: &Path, format: Format) {
    FORMATS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((path.to_path_buf(), format));
}

/// Convert YAML and TOML configuration files to JSON. The format is declared
/// with [`set_format`] or follows the `.yaml`/`.yml`/`.toml` extension.
fn to_json(path: &Path, bytes: Vec<u8>) -> Result<Vec<u8>> {
    let declared = FORMATS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|(declared, _)| {
            declared == path || std::path::absolute(declared).ok().as_deref() == Some(path)
        })
        .map(|(_, format)| *format);
    let format = declared.unwrap_or_else(|| {
        match path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("yaml" | "yml") => Format::Yaml,
            Some("toml") => Format::Toml,
            _ => Format::Json,
        }
    });
    let value: Value = match format {
        Format::Json => return Ok(bytes),
        Format::Yaml => {
            serde_yaml_ng::from_slice(&bytes).map_err(|_| error("invalid YAML configuration"))?
        }
        Format::Toml => std::str::from_utf8(&bytes)
            .ok()
            .and_then(|text| toml::from_str(text).ok())
            .ok_or_else(|| error("invalid TOML configuration"))?,
    };
    serde_json::to_vec(&value).map_err(|_| error("invalid configuration"))
}

/// Categories with dedicated native extractors and typed scopes.
const DEDICATED_CATEGORIES: &[&str] = &[
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
];

/// Upstream top-level sections other than `extractor` and `postprocessor`.
pub const SECTIONS: &[&str] = &["downloader", "output", "cache"];

#[derive(Clone, Default)]
pub struct Config {
    /// Upstream `downloader`, `output` and `cache` sections.
    sections: serde_json::Map<String, Value>,
    /// Values of [`GLOBAL_KEYS`].
    globals: serde_json::Map<String, Value>,
    global: Options,
    extractor: Options,
    categories: BTreeMap<String, Options>,
    classes: BTreeMap<(String, String), Options>,
    parent_keywords: Vec<String>,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("sections", &"[redacted]")
            .field("globals", &"[redacted]")
            .field("global", &self.global)
            .field("extractor", &self.extractor)
            .field("categories", &self.categories)
            .field("classes", &self.classes)
            .finish_non_exhaustive()
    }
}

// Separate structs deliberately avoid serde flatten: duplicate and unknown
// keys must fail at every level, including global versus extractor scopes.
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
struct RawRoot {
    postprocessor: Field,
    init: Field,
    postprocessors: Field,
    postprocessor_options: Field,
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
    parent_metadata: Field,
    metadata_parent: Field,
    favorites: Field,
    reposts: Field,
    ugoira: Field,
    tags: Field,
    cursor: Field,
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
    threshold: Field,
    metadata: Field,
    comments: Field,
    captions: Field,
    sanity: Field,
    embeds: Field,
    metadata_bookmark: Field,
    covers: Field,
    full_series: Field,
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
    init: Field,
    postprocessors: Field,
    postprocessor_options: Field,
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
    parent_metadata: Field,
    metadata_parent: Field,
    favorites: Field,
    reposts: Field,
    ugoira: Field,
    tags: Field,
    cursor: Field,
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
    threshold: Field,
    metadata: Field,
    comments: Field,
    captions: Field,
    sanity: Field,
    embeds: Field,
    metadata_bookmark: Field,
    covers: Field,
    full_series: Field,
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
    pixiv_novel: Box<RawOptions>,
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
    tag: Option<Box<RawOptions>>,
    artist: Option<Box<RawOptions>>,
    artist_search: Option<Box<RawOptions>>,
    #[serde(skip)]
    postprocessor_definitions: Field,
    init: Field,
    postprocessors: Field,
    postprocessor_options: Field,
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
    parent_metadata: Field,
    metadata_parent: Field,
    favorites: Field,
    reposts: Field,
    ugoira: Field,
    tags: Field,
    cursor: Field,
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
    threshold: Field,
    metadata: Field,
    comments: Field,
    captions: Field,
    sanity: Field,
    embeds: Field,
    metadata_bookmark: Field,
    covers: Field,
    full_series: Field,
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

/// Validates that no JSON object repeats a key, at any depth.
struct NoDuplicateKeys;
impl<'de> Deserialize<'de> for NoDuplicateKeys {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = NoDuplicateKeys;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("JSON")
            }
            fn visit_bool<E>(self, _: bool) -> std::result::Result<Self::Value, E> {
                Ok(NoDuplicateKeys)
            }
            fn visit_i64<E>(self, _: i64) -> std::result::Result<Self::Value, E> {
                Ok(NoDuplicateKeys)
            }
            fn visit_u64<E>(self, _: u64) -> std::result::Result<Self::Value, E> {
                Ok(NoDuplicateKeys)
            }
            fn visit_f64<E>(self, _: f64) -> std::result::Result<Self::Value, E> {
                Ok(NoDuplicateKeys)
            }
            fn visit_str<E>(self, _: &str) -> std::result::Result<Self::Value, E> {
                Ok(NoDuplicateKeys)
            }
            fn visit_unit<E>(self) -> std::result::Result<Self::Value, E> {
                Ok(NoDuplicateKeys)
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                while seq.next_element::<NoDuplicateKeys>()?.is_some() {}
                Ok(NoDuplicateKeys)
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut keys = std::collections::HashSet::new();
                while let Some(key) = map.next_key::<String>()? {
                    if !keys.insert(key) {
                        return Err(serde::de::Error::custom("duplicate key"));
                    }
                    map.next_value::<NoDuplicateKeys>()?;
                }
                Ok(NoDuplicateKeys)
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}
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
        let bytes = to_json(&absolute, bytes)?;
        Self::parse(&bytes, &parent)
    }

    /// Load like [`Config::load`], then apply `-o` settings in order.
    pub async fn load_with_options(path: &Path, options: &[CliOption]) -> Result<Self> {
        if options.is_empty() {
            return Self::load(path).await;
        }
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
        let bytes = to_json(&absolute, bytes)?;
        Self::parse_with_options(&bytes, &parent, options)
    }

    /// Parse, then set each `-o` value at its dotted path, as upstream
    /// `config.set` does. Duplicate keys in `bytes` are still rejected.
    pub fn parse_with_options(
        bytes: &[u8],
        directory: &Path,
        options: &[CliOption],
    ) -> Result<Self> {
        if options.is_empty() {
            return Self::parse(bytes, directory);
        }
        if bytes.len() > MAX_CONFIG_BYTES {
            return Err(error("native configuration exceeds 1 MiB"));
        }
        let invalid = || error("invalid native configuration JSON or unsupported option");
        let normalized = crate::source_date_json::normalize(bytes);
        let bytes = normalized.as_deref().unwrap_or(bytes);
        serde_json::from_slice::<NoDuplicateKeys>(bytes).map_err(|_| invalid())?;
        let mut root: Value = serde_json::from_slice(bytes).map_err(|_| invalid())?;
        for option in options {
            let mut node = &mut root;
            for part in &option.path {
                let object = node
                    .as_object_mut()
                    .ok_or_else(|| error("option path crosses a non-object setting"))?;
                node = object
                    .entry(part.clone())
                    .or_insert_with(|| Value::Object(Default::default()));
            }
            let object = node
                .as_object_mut()
                .ok_or_else(|| error("option path crosses a non-object setting"))?;
            if option.append {
                let list = object
                    .entry(option.key.clone())
                    .or_insert_with(|| Value::Array(Vec::new()));
                list.as_array_mut()
                    .ok_or_else(|| error("option appends to a non-list setting"))?
                    .push(option.value.clone());
            } else {
                object.insert(option.key.clone(), option.value.clone());
            }
        }
        let bytes = serde_json::to_vec(&root).map_err(|_| invalid())?;
        Self::parse(&bytes, directory)
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
        let normalized = crate::source_date_json::normalize(bytes);
        let bytes = normalized.as_deref().unwrap_or(bytes);
        let remapped = remap_categories(bytes);
        let bytes = remapped.as_deref().unwrap_or(bytes);
        // Do not forward serde's diagnostics: they can quote private values or
        // a malicious unknown key. Known setting failures are reported below.
        // serde's struct deserializer also accepts positional arrays. Restrict
        // every configuration scope to JSON objects before typed decoding.
        let mut shape: Value = serde_json::from_slice(bytes)
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
            ]
            .into_iter()
            .chain(crate::sites::categories())
            {
                if extractor.get(name).is_some_and(|value| !value.is_object()) {
                    return Err(error("native configuration category must be a JSON object"));
                }
                if !matches!(name, "danbooru" | "e621")
                    && extractor
                        .get(name)
                        .and_then(Value::as_object)
                        .is_some_and(|settings| {
                            ["tag", "artist", "artist-search"]
                                .iter()
                                .any(|key| settings.contains_key(*key))
                        })
                {
                    return Err(error(
                        "native class settings are only supported for danbooru and e621",
                    ));
                }
            }
        }
        let mut raw_classes = Vec::new();
        if let Some(extractor) = shape.get_mut("extractor").and_then(Value::as_object_mut) {
            for category in ["danbooru", "e621"] {
                if let Some(settings) = extractor.get_mut(category).and_then(Value::as_object_mut) {
                    for subcategory in ["artist", "artist-search", "tag"] {
                        if let Some(value) = settings.remove(subcategory) {
                            let fields = value.as_object().ok_or_else(|| {
                                error("native configuration class must be a JSON object")
                            })?;
                            if fields.keys().any(|key| {
                                !matches!(
                                    key.as_str(),
                                    "parent-metadata"
                                        | "metadata-parent"
                                        | "init"
                                        | "postprocessors"
                                        | "postprocessor-options"
                                        | "base-directory"
                                        | "filename"
                                        | "directory"
                                        | "child-filter"
                                        | "child-range"
                                        | "post-filter"
                                        | "post-range"
                                        | "date-before"
                                        | "date-after"
                                )
                            }) {
                                return Err(error("unsupported native booru class option"));
                            }
                            let raw: RawOptions = serde_json::from_value(value)
                                .map_err(|_| error("invalid native booru class settings"))?;
                            raw_classes.push((category.to_owned(), subcategory.to_owned(), raw));
                        }
                    }
                }
            }
        }
        let mut parent_keywords = vec!["_parent".to_owned()];
        fn parent_keys(value: &Value, keys: &mut Vec<String>) {
            if let Some(fields) = value.as_object() {
                for (key, value) in fields {
                    if matches!(key.as_str(), "parent-metadata" | "metadata-parent")
                        && let Some(key) = value.as_str().filter(|s| !s.is_empty())
                    {
                        keys.push(key.into());
                    }
                }
            }
        }
        parent_keys(&shape, &mut parent_keywords);
        if let Some(extractor) = shape.get("extractor") {
            parent_keys(extractor, &mut parent_keywords);
            for category in ["danbooru", "e621"] {
                if let Some(settings) = extractor.get(category) {
                    parent_keys(settings, &mut parent_keywords);
                }
            }
        }
        for (_, _, raw) in &raw_classes {
            for field in [&raw.parent_metadata, &raw.metadata_parent] {
                if let Some(Value::String(key)) = &field.0 {
                    parent_keywords.push(key.clone());
                }
            }
        }
        parent_keywords.sort();
        parent_keywords.dedup();
        // Registry site scopes are decoded separately, so the typed root
        // decoder does not need one field per site. Reject duplicate keys in
        // the original bytes first, because the reserialized shape cannot.
        serde_json::from_slice::<NoDuplicateKeys>(bytes)
            .map_err(|_| error("invalid native configuration JSON or unsupported option"))?;
        let mut site_scopes = Vec::new();
        let mut site_options = BTreeMap::new();
        let mut original: Value = serde_json::from_slice(bytes)
            .map_err(|_| error("invalid native configuration JSON or unsupported option"))?;
        if let Some(extractor) = original.get_mut("extractor").and_then(Value::as_object_mut) {
            // Shared job options keep their typed meaning; all other keys
            // belong to the site, even where another site uses the name.
            const SHARED: &[&str] = &[
                "postprocessor",
                "init",
                "postprocessors",
                "postprocessor-options",
                "base-directory",
                "filename",
                "directory",
                "file-range",
                "file-filter",
                "image-filter",
                "post-range",
                "post-filter",
                "child-range",
                "child-filter",
                "date-before",
                "date-after",
                "parent-metadata",
                "metadata-parent",
                "cookies",
                "proxy",
                "user-agent",
                "archive",
                "filesize-min",
                "filesize-max",
                "retries",
                "timeout",
                "api-key",
            ];
            for category in crate::sites::categories() {
                if let Some(mut value) = extractor.remove(category) {
                    // Keys outside the shared option set are site options.
                    if let Some(fields) = value.as_object_mut() {
                        let extra: serde_json::Map<String, Value> = fields
                            .keys()
                            .filter(|key| !SHARED.contains(&key.as_str()))
                            .cloned()
                            .collect::<Vec<_>>()
                            .into_iter()
                            .filter_map(|key| fields.remove_entry(&key))
                            .collect();
                        if !extra.is_empty() {
                            site_options.insert(category, std::sync::Arc::new(extra));
                        }
                    }
                    let raw: RawOptions = serde_json::from_value(value).map_err(|_| {
                        error("invalid native configuration JSON or unsupported option")
                    })?;
                    site_scopes.push((category, Box::new(raw)));
                }
            }
            // Dedicated native extractors keep their typed scope; their
            // upstream-specific settings are forwarded as site options.
            for (category, keys) in NATIVE_SITE_OPTIONS {
                if let Some(fields) = extractor.get_mut(*category).and_then(Value::as_object_mut) {
                    let extra: serde_json::Map<String, Value> = keys
                        .iter()
                        .filter_map(|key| fields.remove_entry(*key))
                        .collect();
                    if !extra.is_empty() {
                        site_options.insert(*category, std::sync::Arc::new(extra));
                    }
                }
            }
        }
        let mut globals = serde_json::Map::new();
        if let Some(object) = original.as_object_mut() {
            for key in GLOBAL_KEYS {
                if let Some(value) = object.remove(*key) {
                    globals.insert((*key).into(), value);
                }
            }
            if let Some(extractor) = object.get_mut("extractor").and_then(Value::as_object_mut) {
                for key in GLOBAL_KEYS {
                    if let Some(value) = extractor.remove(*key) {
                        globals.insert((*key).into(), value);
                    }
                }
            }
        }
        let mut sections = serde_json::Map::new();
        let mut root_extra = serde_json::Map::new();
        let mut extractor_extra = serde_json::Map::new();
        if let Some(object) = original.as_object_mut() {
            for section in SECTIONS {
                if let Some(value) = object.remove(*section) {
                    sections.insert((*section).into(), value);
                }
            }
            let mut known = struct_fields::<RawRoot>().to_vec();
            known.push("extractor");
            root_extra = take_unknown(object, &known);
            if let Some(extractor) = object.get_mut("extractor").and_then(Value::as_object_mut) {
                extractor_extra = take_unknown(extractor, struct_fields::<RawExtractor>());
                let options = struct_fields::<RawOptions>();
                for category in DEDICATED_CATEGORIES {
                    let Some(fields) = extractor.get_mut(*category).and_then(Value::as_object_mut)
                    else {
                        continue;
                    };
                    let extra = take_unknown(fields, options);
                    if !extra.is_empty() {
                        let entry = site_options
                            .entry(*category)
                            .or_insert_with(|| std::sync::Arc::new(Default::default()));
                        std::sync::Arc::make_mut(entry).extend(extra);
                    }
                }
            }
        }
        let stripped = serde_json::to_vec(&original)
            .map_err(|_| error("invalid native configuration JSON or unsupported option"))?;
        let root: RawRoot = serde_json::from_slice(&stripped)
            .map_err(|_| error("invalid native configuration JSON or unsupported option"))?;
        let mut global = RawOptions {
            tag: None,
            artist: None,
            artist_search: None,
            init: root.init,
            postprocessors: root.postprocessors,
            postprocessor_options: root.postprocessor_options,
            postprocessor_definitions: root.postprocessor,
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
            parent_metadata: root.parent_metadata,
            metadata_parent: root.metadata_parent,
            favorites: root.favorites,
            reposts: root.reposts,
            ugoira: root.ugoira,
            tags: root.tags,
            cursor: root.cursor,
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
            threshold: root.threshold,
            metadata: root.metadata,
            comments: root.comments,
            captions: root.captions,
            sanity: root.sanity,
            embeds: root.embeds,
            metadata_bookmark: root.metadata_bookmark,
            api_key: Field::default(),
            username: Field::default(),
            password: Field::default(),
            user_id: Field::default(),
            access_token: Field::default(),
            refresh_token: Field::default(),
            covers: root.covers,
            full_series: root.full_series,
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
        .resolve(directory, &parent_keywords)?;
        let extractor = root.extractor;
        let mut defaults = RawOptions {
            tag: None,
            artist: None,
            artist_search: None,
            init: extractor.init,
            postprocessors: extractor.postprocessors,
            postprocessor_options: extractor.postprocessor_options,
            postprocessor_definitions: Field::default(),
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
            parent_metadata: extractor.parent_metadata,
            metadata_parent: extractor.metadata_parent,
            favorites: extractor.favorites,
            reposts: extractor.reposts,
            ugoira: extractor.ugoira,
            tags: extractor.tags,
            cursor: extractor.cursor,
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
            threshold: extractor.threshold,
            metadata: extractor.metadata,
            comments: extractor.comments,
            captions: extractor.captions,
            sanity: extractor.sanity,
            embeds: extractor.embeds,
            metadata_bookmark: extractor.metadata_bookmark,
            api_key: Field::default(),
            username: Field::default(),
            password: Field::default(),
            user_id: Field::default(),
            access_token: Field::default(),
            refresh_token: Field::default(),
            covers: extractor.covers,
            full_series: extractor.full_series,
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
        .resolve(directory, &parent_keywords)?;
        if !root_extra.is_empty() {
            global.site_options = Some(std::sync::Arc::new(root_extra));
        }
        if !extractor_extra.is_empty() {
            defaults.site_options = Some(std::sync::Arc::new(extractor_extra));
        }
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
            ("pixiv-novel", extractor.pixiv_novel),
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
        ]
        .into_iter()
        .chain(site_scopes)
        {
            if !matches!(name, "danbooru" | "e621")
                && (settings.tag.is_some()
                    || settings.artist.is_some()
                    || settings.artist_search.is_some())
            {
                return Err(error(
                    "native class settings are only supported for danbooru and e621",
                ));
            }
            if !matches!(
                name,
                "tumblr"
                    | "flickr"
                    | "wallhaven"
                    | "weasyl"
                    | "pixeldrain"
                    | "gelbooru"
                    | "safebooru"
            ) && !crate::sites::site(name).is_some_and(|site| site.api_key)
                && settings.api_key.0.is_some()
            {
                return Err(error(
                    "native configuration api-key is only supported for tumblr, flickr, wallhaven, weasyl, pixeldrain, gelbooru and safebooru",
                ));
            }
            if !matches!(name, "danbooru" | "e621") && settings.threshold.0.is_some() {
                return Err(error(
                    "native configuration threshold is only supported for danbooru and e621",
                ));
            }
            if !matches!(name, "danbooru" | "e621")
                && (settings.username.0.is_some() || settings.password.0.is_some())
            {
                return Err(error(
                    "native configuration username/password are only supported for danbooru and e621",
                ));
            }
            if !matches!(name, "gelbooru" | "safebooru") && settings.user_id.0.is_some() {
                return Err(error(
                    "native configuration user-id is only supported for gelbooru and safebooru",
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
            if !matches!(name, "mastodon.social" | "pawoo" | "baraag" | "tumblr")
                && settings.access_token.0.is_some()
            {
                return Err(error(
                    "native configuration access-token requires a tumblr, mastodon.social, pawoo or baraag scope",
                ));
            }
            if !matches!(name, "pixiv" | "pixiv-novel" | "reddit")
                && settings.refresh_token.0.is_some()
            {
                return Err(error(
                    "native configuration refresh-token is only supported for pixiv, pixiv-novel or reddit",
                ));
            }
            if !matches!(name, "deviantart" | "reddit") && settings.client_id.0.is_some()
                || name != "deviantart" && settings.client_secret.0.is_some()
            {
                return Err(error(
                    "native configuration client-id is only supported for deviantart or reddit, \
                     and client-secret for deviantart",
                ));
            }
            if name == "deviantart"
                && settings
                    .client_id
                    .0
                    .as_ref()
                    .and_then(Value::as_str)
                    .is_some_and(|id| !id.bytes().all(|b| b.is_ascii_digit()))
            {
                return Err(error(
                    "native configuration client-id must be 1-128 ASCII digits",
                ));
            }
            let opaque_key = if matches!(name, "pixeldrain" | "gelbooru" | "safebooru") {
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
            let booru_favorite_order = if name == "gelbooru" {
                settings.order_posts.0.take()
            } else {
                None
            };
            let booru_order_posts = if name == "danbooru" {
                settings
                    .order_posts
                    .0
                    .take()
                    .map(danbooru_pool_order)
                    .transpose()?
            } else {
                if name == "e621" {
                    settings.order_posts.0.take();
                }
                None
            };
            // Boxed: unoptimized builds overflow the stack with inline settings.
            let mut resolved = (*settings).resolve(directory, &parent_keywords)?;
            resolved.bluesky_endpoint = bluesky_endpoint;
            resolved.booru_order_posts = booru_order_posts;
            resolved.booru_favorite_order = booru_favorite_order;
            if matches!(name, "pixeldrain" | "gelbooru" | "safebooru") {
                resolved.api_key = opaque_key;
            }
            resolved.tumblr_reblogs = tumblr_reblogs;
            resolved.site_options = site_options.remove(name);
            categories.insert(name.into(), resolved);
        }
        let classes = raw_classes
            .into_iter()
            .map(|(category, subcategory, raw)| {
                raw.resolve(directory, &parent_keywords)
                    .map(|settings| ((category, subcategory), settings))
            })
            .collect::<Result<_>>()?;
        Ok(Self {
            sections,
            globals,
            global,
            extractor: defaults,
            categories,
            classes,
            parent_keywords,
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
                | "pixiv-novel"
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
        ) && !crate::sites::is_category(category)
        {
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
        let mut resolved = settings.overlay(&self.global);
        // Postprocessor lists accumulate most-specific first, unlike scalar overrides.
        resolved.postprocessors = match (&self.global.postprocessors, &settings.postprocessors) {
            (Some(global), Some(specific)) => {
                Some(global.overlaid(specific).with_global_options(global))
            }
            (global, specific) => specific.clone().or_else(|| global.clone()),
        };
        Ok(resolved)
    }
    /// Upstream `downloader`, `output` and `cache` sections.
    pub fn sections(&self) -> &serde_json::Map<String, Value> {
        &self.sections
    }

    /// Process-wide settings ([`GLOBAL_KEYS`]).
    pub fn globals(&self) -> &serde_json::Map<String, Value> {
        &self.globals
    }

    pub fn parent_keywords(&self) -> &[String] {
        &self.parent_keywords
    }
    pub fn resolve_class(&self, category: &str, subcategory: &str) -> Result<Options> {
        let mut resolved = self.resolve(category)?;
        if let Some(class) = self.classes.get(&(category.into(), subcategory.into())) {
            resolved = resolved.overlay(class).overlay(&self.global);
            let category_settings = self.categories.get(category);
            let mut processors = self.extractor.postprocessors.clone();
            for settings in [category_settings, Some(class)].into_iter().flatten() {
                processors = match (processors, &settings.postprocessors) {
                    (Some(base), Some(specific)) => Some(base.overlaid(specific)),
                    (base, specific) => specific.clone().or(base),
                };
            }
            resolved.postprocessors = match (&self.global.postprocessors, processors) {
                (Some(global), Some(specific)) => {
                    Some(global.overlaid(&specific).with_global_options(global))
                }
                (global, specific) => specific.or_else(|| global.clone()),
            };
        }
        Ok(resolved)
    }
}

impl RawOptions {
    fn resolve(self, directory: &Path, _parent_keywords: &[String]) -> Result<Options> {
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
        let booru_favorite_order = self.order_posts.0.clone();
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
        let ugoira = self.ugoira.0.as_ref().map(crate::pixiv::UgoiraMode::parse);
        // Upstream has special behavior for exactly two strings; every other
        // present value uses Japanese names and overrides inherited settings.
        let tags = self
            .tags
            .0
            .map(|value| crate::pixiv::TagMode::parse(value.as_str().unwrap_or("")));
        let cursor = self.cursor.0.as_ref().map(|value| {
            crate::pixiv::BookmarkCursor::parse(value)
                .ok_or_else(|| error("native configuration cursor must be null, a string or an integer of at most 1024 bytes"))
        }).transpose()?;
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
                    "native configuration access-token must be a token string or false; OAuth cache discovery is not implemented",
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
        let metadata = self.metadata.0.as_ref().map(value_truthy);
        let comments = self.comments.0.as_ref().map(value_truthy);
        let captions = self.captions.0.as_ref().map(value_truthy);
        let sanity = self.sanity.0.as_ref().map(value_truthy);
        let embeds = self.embeds.0.as_ref().map(value_truthy);
        let metadata_bookmark = self.metadata_bookmark.0.as_ref().map(value_truthy);
        let booru_threshold = self
            .threshold
            .0
            .map(|value| -> Result<Option<usize>> {
                if let Value::Number(number) = value {
                    let raw = number.to_string();
                    // Python accepts only int here, excluding bool and float even
                    // when a floating-point value has an integral numeric value.
                    if !raw.contains(['.', 'e', 'E']) {
                        if raw.starts_with('-') {
                            return Ok(Some(1));
                        }
                        let threshold = raw.parse::<usize>().map_err(|_| {
                            error("native configuration threshold integer exceeds the supported range")
                        })?;
                        return Ok(Some(threshold.max(1)));
                    }
                }
                Ok(None)
            })
            .transpose()?;
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
        // Client IDs are DeviantArt digits (checked per category) or
        // Reddit identifiers with '-' and '_'.
        let credential = |field: Field, name: &str, identifier: bool| match field.0 {
            None => Ok(None),
            Some(Value::String(value))
                if (1..=128).contains(&value.len())
                    && value.bytes().all(|b| {
                        b.is_ascii_alphanumeric() || identifier && matches!(b, b'-' | b'_')
                    }) =>
            {
                Ok(Some(value))
            }
            Some(_) => Err(error(format!(
                "native configuration {name} must be 1-128 ASCII {}",
                if identifier {
                    "letters, digits, '-' or '_'"
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
                FilenameTemplate::parse_source(&value)
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
                FilenameTemplate::parse_source_conditional(&conditions, default.as_deref())
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
                DirectoryTemplate::parse_source_conditional(&conditions, default.as_deref())
                    .map_err(|_| error("unsupported native configuration directory template"))?;
                (default, Some(conditions))
            }
            Some(value) => {
                let value = components(value)?;
                DirectoryTemplate::parse_source(&value)
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
            init: self
                .init
                .0
                .as_ref()
                .map(|value| value_truthy(value) && value.as_str() != Some("lazy")),
            postprocessors: crate::postprocess::Configuration::parse(
                self.postprocessors.0,
                self.postprocessor_options.0,
                self.postprocessor_definitions.0,
                directory,
            )?,
            covers: self.covers.0.as_ref().map(value_truthy),
            full_series: self.full_series.0.as_ref().map(value_truthy),
            api_server,
            bluesky_endpoint: None,
            proxy,
            user_agent,
            base_directory: path(self.base_directory, directory, "base-directory")?,
            cookies,
            browser_cookies,
            archive: archive_path(self.archive, directory)?,
            filename,
            filename_conditions,
            file_range,
            file_filter,
            post_range,
            post_filter,
            child_range,
            child_filter,
            include,
            date_before_set: self.date_before.0.is_some(),
            date_after_set: self.date_after.0.is_some(),
            date_before: date(self.date_before),
            date_after: date(self.date_after),
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
            booru_favorite_order,
            parent_metadata: self.parent_metadata.0.map(std::sync::Arc::new),
            metadata_parent: self.metadata_parent.0.map(std::sync::Arc::new),
            booru_order_posts: order_posts.map(|order| match order {
                crate::kemono::PostOrder::Forward => crate::booru::PoolOrder::Pool,
                crate::kemono::PostOrder::Reverse => crate::booru::PoolOrder::PoolReverse,
            }),
            order_posts,
            favorites,
            username,
            password,
            user_id,
            reposts,
            ugoira,
            tags,
            cursor,
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
            booru_threshold,
            metadata,
            comments,
            captions,
            sanity,
            embeds,
            metadata_bookmark,
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
            site_options: None,
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

fn cookie_source(field: Field, directory: &Path) -> Result<(Option<PathBuf>, Option<String>)> {
    let Some(value) = field.0 else {
        return Ok((None, None));
    };
    if let Value::Array(values) = value {
        let invalid = || {
            error(
                "native configuration cookies browser source requires an explicit Firefox profile or a Chromium browser, without keyring or container selection",
            )
        };
        let browser = values.first().and_then(Value::as_str).ok_or_else(invalid)?;
        let chromium = crate::browser_chromium::is_browser(&browser.to_ascii_lowercase());
        if !(if chromium { 1..=4 } else { 2..=4 }).contains(&values.len())
            || values.get(2).is_some_and(|value| !value.is_null())
            || values
                .get(3)
                .is_some_and(|value| !value.is_null() && value.as_str() != Some("none"))
        {
            return Err(invalid());
        }
        let spec = match values.get(1) {
            // Chromium profile names (`Profile 1`) stay names; paths resolve.
            None | Some(Value::Null) if chromium => browser.to_owned(),
            Some(Value::String(name)) if chromium && !name.contains(std::path::MAIN_SEPARATOR) => {
                format!("{browser}:{name}")
            }
            Some(profile) if browser == "firefox" || chromium => {
                let profile = path_with(Field(Some(profile.clone())), directory, "cookies", false)?
                    .ok_or_else(invalid)?;
                let profile_text = profile.to_str().ok_or_else(invalid)?;
                format!("{browser}:{profile_text}")
            }
            _ => return Err(invalid()),
        };
        // Keep configuration and the CLI's explicit profile grammar aligned.
        crate::browser::BrowserProfile::parse(&spec).map_err(|_| invalid())?;
        Ok((None, Some(spec)))
    } else {
        Ok((path(Field(Some(value)), directory, "cookies")?, None))
    }
}

/// Upstream archive paths: a string, or a list of path segments where
/// `{category}` is replaced per category (see [`render_archive`]).
fn archive_path(field: Field, directory: &Path) -> Result<Option<PathBuf>> {
    let Some(Value::Array(segments)) = &field.0 else {
        return path(field, directory, "archive");
    };
    let invalid = || error("native configuration archive segments must be strings");
    let mut joined = PathBuf::new();
    for (index, segment) in segments.iter().enumerate() {
        let segment = segment.as_str().ok_or_else(invalid)?;
        let without = segment.replace("{category}", "");
        if without.contains(['{', '}'])
            || segment.is_empty()
            || segment.chars().any(char::is_control)
        {
            return Err(error(
                "native configuration archive segments support only the {category} field",
            ));
        }
        if index == 0 {
            joined.push(crate::postprocess::ugoira::expanded(segment));
        } else if matches!(segment, "." | "..") || segment.contains(['/', '\\']) {
            return Err(invalid());
        } else {
            joined.push(segment);
        }
    }
    if joined.as_os_str().is_empty() {
        return Err(invalid());
    }
    // Like upstream, relative paths resolve against the working directory.
    let _ = directory;
    Ok(Some(joined))
}

/// Replace `{category}` in an archive path from a segment list.
pub fn render_archive(path: &Path, category: &str) -> PathBuf {
    match path.to_str() {
        Some(text) if text.contains("{category}") => text.replace("{category}", category).into(),
        _ => path.to_owned(),
    }
}

fn path(field: Field, directory: &Path, name: &str) -> Result<Option<PathBuf>> {
    path_with(field, directory, name, true)
}

/// A local path; `expand` applies upstream's `~` and environment variable
/// expansion first, otherwise such values are rejected.
fn path_with(field: Field, directory: &Path, name: &str, expand: bool) -> Result<Option<PathBuf>> {
    let Some(value) = field.0 else {
        return Ok(None);
    };
    let invalid = || {
        error(format!(
            "native configuration {name} must be a nonempty local path"
        ))
    };
    let value = value.as_str().ok_or_else(invalid)?;
    // Like upstream `util.expand_path`: `~` and environment variables.
    let value = if expand {
        crate::postprocess::ugoira::expanded(value)
    } else if value.starts_with('~') || value.contains('$') || value.contains(['{', '}']) {
        return Err(invalid());
    } else {
        value.to_owned()
    };
    if value.is_empty()
        || value.chars().any(char::is_control)
        || value.starts_with('~')
        || value.contains(['{', '}'])
        || value.contains("://")
    {
        return Err(invalid());
    }
    // Like upstream, relative paths resolve against the working directory.
    let _ = directory;
    Ok(Some(PathBuf::from(value)))
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

fn date(field: Field) -> Option<i64> {
    source_date_bound(field.0.as_ref()?)
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

/// Resolve the source Danbooru collection ordering aliases.
fn danbooru_pool_order(value: Value) -> Result<crate::booru::PoolOrder> {
    use crate::booru::PoolOrder;
    if !value_truthy(&value) {
        return Ok(PoolOrder::Pool);
    }
    match value.as_str() {
        Some("asc" | "pool" | "pool_asc" | "asc_pool") => Ok(PoolOrder::Pool),
        Some("desc" | "desc_pool" | "pool_desc") => Ok(PoolOrder::PoolReverse),
        Some("id" | "desc_id" | "id_desc") => Ok(PoolOrder::IdDescending),
        Some("asc_id" | "id_asc") => Ok(PoolOrder::IdAscending),
        _ => Err(error(
            "native configuration Danbooru order-posts must select pool or ID ordering",
        )),
    }
}

/// Match Python truthiness for configuration switches that accept arbitrary JSON.
fn value_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64() != Some(0.),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

#[cfg(test)]
mod site_scope_tests {
    #[test]
    fn registry_scopes_decode_and_reject_duplicates() {
        let dir = std::env::temp_dir();
        let config = super::Config::parse(
            br#"{"extractor":{"4chan":{"filename":"{tim}.{extension}","retries":2}}}"#,
            &dir,
        )
        .unwrap();
        let resolved = config.resolve("4chan").unwrap();
        assert_eq!(resolved.retries, Some(2));
        assert_eq!(resolved.site_options, None);
        let config = super::Config::parse(
            br#"{"extractor":{"yandere":{"tags":true,"notes":"x","retries":1}}}"#,
            &dir,
        )
        .unwrap();
        let resolved = config.resolve("yandere").unwrap();
        assert_eq!(resolved.retries, Some(1));
        let options = resolved.site_options.unwrap();
        assert_eq!(options.get("notes"), Some(&serde_json::json!("x")));
        assert_eq!(options.get("tags"), Some(&serde_json::json!(true)));
        let cli = [
            super::parse_option("extractor.4chan.retries=5").unwrap(),
            super::parse_option("extractor.yandere.notes=true").unwrap(),
            super::parse_option("extractor.yandere.lang=en").unwrap(),
        ];
        let config = super::Config::parse_with_options(
            br#"{"extractor":{"4chan":{"retries":1}}}"#,
            &dir,
            &cli,
        )
        .unwrap();
        assert_eq!(config.resolve("4chan").unwrap().retries, Some(5));
        let options = config.resolve("yandere").unwrap().site_options.unwrap();
        assert_eq!(options["notes"], serde_json::json!(true));
        assert_eq!(options["lang"], serde_json::json!("en"));
        assert!(super::Config::parse_with_options(br#"{"a":1,"a":2}"#, &dir, &cli).is_err());
        let config = super::Config::parse_with_options(
            br#"{"extractor":{"reddit":{"comments":500,"videos":"ytdl","retries":3},
                "pinterest":{"sections":false},
                "tumblr":{"external":true,"posts":"photo","api-key":"k"}}}"#,
            &dir,
            &[super::parse_option("extractor.reddit.recursion=2").unwrap()],
        )
        .unwrap();
        let reddit = config.resolve("reddit").unwrap();
        assert_eq!(reddit.retries, Some(3));
        let options = reddit.site_options.unwrap();
        assert_eq!(options["comments"], serde_json::json!(500));
        assert_eq!(options["videos"], serde_json::json!("ytdl"));
        assert_eq!(options["recursion"], serde_json::json!(2));
        let options = config.resolve("pinterest").unwrap().site_options.unwrap();
        assert_eq!(options["sections"], serde_json::json!(false));
        let tumblr = config.resolve("tumblr").unwrap();
        assert_eq!(tumblr.api_key.as_deref(), Some("k"));
        assert_eq!(tumblr.posts.as_deref(), Some(&["photo".to_owned()][..]));
        let options = tumblr.site_options.unwrap();
        assert_eq!(options["external"], serde_json::json!(true));
        assert!(!options.contains_key("posts"));
        for bad in [
            &br#"{"extractor":{"4chan":{"retries":1},"4chan":{"retries":2}}}"#[..],
            br#"{"extractor":{"4chan":{"retries":1,"retries":2}}}"#,
            br#"{"extractor":{"4chan":[]}}"#,
            br#"{"retries":1,"retries":2}"#,
        ] {
            assert!(super::Config::parse(bad, &dir).is_err());
        }
    }
}
