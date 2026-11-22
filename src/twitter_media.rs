//! File selection shared by statuses, linked posts, and timeline traversal.
use super::*;
use crate::{extract::MediaJob, file_metadata::FileMetadata};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VideoMode {
    /// Highest-bitrate valid direct MP4 variant.
    #[default]
    Files,
    /// Omit video files; preview selection remains independent.
    Skip,
    /// Emit the existing typed video-page job for a companion adapter.
    Ytdl,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MediaOptions {
    pub videos: VideoMode,
    /// Append a video's poster after its file, or emit only the poster when skipped.
    pub previews: bool,
    /// Attempt supplied media URLs even when the API marks the media unavailable.
    pub unavailable: bool,
    /// Upstream `twitpic`: also download images of linked twitpic.com pages.
    pub twitpic: bool,
}

static TWEET: crate::sites::Route = crate::sites::Route::new(
    "tweet",
    "{tweet_id}_{num}.{extension}",
    &["{category}", "{user[name]}"],
    "{tweet_id}_{retweet_id}_{num}",
);

fn date_of(timestamp: Option<i64>) -> Value {
    crate::filter::source_date_text(timestamp).into()
}

fn parse_int(value: Option<&Value>) -> i64 {
    crate::sites::gdl::int_of(value)
}

/// Upstream `_transform_user`.
pub(super) fn transform_user(user: &Value) -> Value {
    let Some(uid) = user
        .get("rest_id")
        .or_else(|| user.get("id_str"))
        .and_then(Value::as_str)
    else {
        return Value::Object(serde_json::Map::new());
    };
    let nested = user.get("core").is_some_and(Value::is_object) && user.get("legacy").is_some();
    let (core, legacy) = if nested {
        (&user["core"], &user["legacy"])
    } else if let Some(legacy) = user.get("legacy") {
        // Older responses keep every field in `legacy`.
        (legacy, legacy)
    } else {
        (user, user)
    };
    let get = |key: &str| legacy.get(key).cloned().unwrap_or(Value::Null);
    let mut data = serde_json::Map::new();
    data.insert("id".into(), crate::sites::gdl::parse_int(uid).into());
    data.insert(
        "name".into(),
        core.get("screen_name").cloned().unwrap_or(Value::Null),
    );
    data.insert(
        "nick".into(),
        core.get("name").cloned().unwrap_or(Value::Null),
    );
    let created = core
        .get("created_at")
        .and_then(Value::as_str)
        .and_then(timestamp);
    data.insert("date".into(), date_of(created));
    let banner = legacy
        .get("profile_banner_url")
        .cloned()
        .unwrap_or_else(|| "".into());
    data.insert("profile_banner".into(), banner);
    for key in [
        "favourites_count",
        "followers_count",
        "friends_count",
        "listed_count",
        "media_count",
        "statuses_count",
    ] {
        data.insert(key.into(), get(key));
    }
    let image = if nested {
        data.insert(
            "location".into(),
            user.pointer("/location/location")
                .cloned()
                .unwrap_or(Value::Null),
        );
        data.insert(
            "verified".into(),
            user.pointer("/verification/verified")
                .cloned()
                .unwrap_or(Value::Null),
        );
        data.insert(
            "protected".into(),
            user.pointer("/privacy/protected")
                .cloned()
                .unwrap_or(Value::Null),
        );
        user.pointer("/avatar/image_url")
    } else {
        for key in ["location", "verified", "protected"] {
            data.insert(key.into(), user.get(key).cloned().unwrap_or(Value::Null));
        }
        user.get("profile_image_url_https")
    };
    let image = image
        .and_then(Value::as_str)
        .unwrap_or("")
        .replace("_normal.", ".");
    data.insert("profile_image".into(), image.into());
    let mut description = legacy
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    if let Some(entities) = legacy.get("entities") {
        for url in entities
            .pointer("/description/urls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let (Some(short), Some(long)) = (
                url.get("url").and_then(Value::as_str),
                url.get("expanded_url").and_then(Value::as_str),
            ) {
                description = description.replace(short, long);
            }
        }
        if let Some(url) = entities.pointer("/url/urls/0") {
            let link = url.get("expanded_url").or_else(|| url.get("url"));
            data.insert("url".into(), link.cloned().unwrap_or(Value::Null));
        }
    }
    data.insert("description".into(), description.into());
    Value::Object(data)
}

/// Upstream `_tweetid_to_datetime`.
fn snowflake_seconds(id: i64) -> i64 {
    ((id >> 22) + 1_288_834_974_657) / 1000
}

/// Upstream `_transform_tweet`.
pub(super) fn transform_tweet(tweet: &Value, tweet_id: &str) -> serde_json::Map<String, Value> {
    let legacy = tweet.get("legacy").unwrap_or(tweet);
    let get = |key: &str| legacy.get(key).cloned().unwrap_or(Value::Null);
    let id = crate::sites::gdl::parse_int(tweet_id);
    let author = tweet
        .get("author")
        .or_else(|| tweet.pointer("/core/user_results/result"))
        .or_else(|| tweet.get("user"))
        .map(transform_user)
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
    let date = if id >= 300_000_000_000_000 {
        Some(snowflake_seconds(id))
    } else {
        legacy
            .get("created_at")
            .and_then(Value::as_str)
            .and_then(timestamp)
    };
    let mut data = serde_json::Map::new();
    data.insert("tweet_id".into(), id.into());
    for (key, field) in [
        ("retweet_id", "retweeted_status_id_str"),
        ("quote_id", "quoted_by_id_str"),
    ] {
        // Zero would hide the native identity of expanded retweets/quotes.
        let value = parse_int(legacy.get(field));
        if value != 0 {
            data.insert(key.into(), value.into());
        }
    }
    data.insert(
        "reply_id".into(),
        parse_int(legacy.get("in_reply_to_status_id_str")).into(),
    );
    data.insert(
        "conversation_id".into(),
        parse_int(legacy.get("conversation_id_str")).into(),
    );
    data.insert("date".into(), date_of(date));
    data.insert("author".into(), author.clone());
    data.insert("user".into(), author.clone());
    data.insert("lang".into(), get("lang"));
    let source = tweet.get("source").and_then(Value::as_str).unwrap_or("");
    let source = crate::sites::extr(source, ">", "<").unwrap_or("");
    data.insert("source".into(), source.into());
    data.insert("sensitive".into(), get("possibly_sensitive"));
    for key in [
        "favorite_count",
        "quote_count",
        "reply_count",
        "retweet_count",
        "bookmark_count",
    ] {
        data.insert(key.into(), get(key));
    }
    let views = tweet
        .pointer("/views/count")
        .and_then(|v| {
            v.as_str()
                .map(str::to_owned)
                .or_else(|| v.as_i64().map(|n| n.to_string()))
        })
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0);
    data.insert("view_count".into(), views.into());
    let note = tweet.pointer("/note_tweet/note_tweet_results/result");
    let (content, entities) = match note {
        Some(note) => (
            note.get("text").and_then(Value::as_str).unwrap_or(""),
            note.get("entity_set").unwrap_or(&Value::Null),
        ),
        None => (
            legacy
                .get("full_text")
                .or_else(|| legacy.get("text"))
                .and_then(Value::as_str)
                .unwrap_or(""),
            legacy.get("entities").unwrap_or(&Value::Null),
        ),
    };
    if let Some(tags) = entities
        .get("hashtags")
        .and_then(Value::as_array)
        .filter(|t| !t.is_empty())
    {
        let tags: Vec<Value> = tags.iter().filter_map(|t| t.get("text").cloned()).collect();
        data.insert("hashtags".into(), tags.into());
    }
    if let Some(mentions) = entities
        .get("user_mentions")
        .and_then(Value::as_array)
        .filter(|m| !m.is_empty())
    {
        let mentions: Vec<Value> = mentions
            .iter()
            .map(|user| {
                serde_json::json!({
                    "id": parse_int(user.get("id_str")),
                    "name": user.get("screen_name"),
                    "nick": user.get("name"),
                })
            })
            .collect();
        data.insert("mentions".into(), mentions.into());
    }
    let mut content = crate::sites::unescape(content);
    for url in entities
        .get("urls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let (Some(short), Some(long)) = (
            url.get("url").and_then(Value::as_str),
            url.get("expanded_url").and_then(Value::as_str),
        ) {
            content = content.replace(short, long);
        }
    }
    let content = match content.rsplit_once(' ') {
        Some((text, tco)) if tco.starts_with("https://t.co/") => text.to_owned(),
        _ => content,
    };
    data.insert("content".into(), content.into());
    if tweet.get("pinned").is_some() {
        data.insert("pinned".into(), true.into());
    }
    if let Some(note) = tweet.pointer("/birdwatch_pivot/subtitle/text") {
        data.insert("birdwatch".into(), note.clone());
    }
    if let Some(article) = tweet.get("article") {
        data.insert("article".into(), article.clone());
    }
    if let Some(name) = legacy.get("in_reply_to_screen_name") {
        data.insert("reply_to".into(), name.clone());
    }
    if let Some(by) = legacy.get("quoted_by") {
        data.insert("quote_by".into(), by.clone());
    }
    if let Some(retweet) = data.get("retweet_id").and_then(Value::as_i64) {
        let name = author.get("name").and_then(Value::as_str).unwrap_or("");
        let content = data
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        data.insert("content".into(), format!("RT @{name}: {content}").into());
        data.insert(
            "date_original".into(),
            date_of(Some(snowflake_seconds(retweet))),
        );
    }
    data
}

/// Per-file keywords for `extended_entities` media.
fn file_keywords(item: &Value, preview: bool, url: &Url) -> serde_json::Map<String, Value> {
    let mut file = serde_json::Map::new();
    let video = item.get("video_info").is_some() && !preview;
    if video {
        let duration = item
            .pointer("/video_info/duration_millis")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        file.insert("duration".into(), (duration / 1000.0).into());
    }
    let kind = if preview {
        "preview".into()
    } else {
        item.get("type").cloned().unwrap_or(Value::Null)
    };
    file.insert("type".into(), kind);
    let info = item.get("original_info");
    let size = |key: &str| {
        info.and_then(|i| i.get(key))
            .filter(|v| v.is_u64())
            .cloned()
            .unwrap_or(0.into())
    };
    file.insert("width".into(), size("width"));
    file.insert("height".into(), size("height"));
    file.insert(
        "description".into(),
        item.get("ext_alt_text").cloned().unwrap_or(Value::Null),
    );
    let flags: Vec<Value> = [
        ("adult_content", "Nudity"),
        ("other", "Sensitive"),
        ("graphic_violence", "Violence"),
    ]
    .into_iter()
    .filter(|(key, _)| {
        item.get("sensitive_media_warning")
            .is_some_and(|w| w.get(key).is_some())
    })
    .map(|(_, flag)| flag.into())
    .collect();
    file.insert("sensitive_flags".into(), flags.into());
    let source = parse_int(item.get("source_status_id_str"));
    file.insert("source_id".into(), source.into());
    if source != 0
        && let Some(user) = item.pointer("/additional_media_info/source_user/user_results/result")
    {
        file.insert("source_user".into(), transform_user(user));
    }
    // Images are named after their `media_url_https`, videos after the variant.
    let named = if video {
        url.clone()
    } else {
        item.get("media_url_https")
            .and_then(Value::as_str)
            .and_then(|raw| Url::parse(raw).ok())
            .unwrap_or_else(|| url.clone())
    };
    let (filename, extension) = crate::sites::nameext(&named);
    let extension = crate::path_options::map_extension(&extension).unwrap_or(extension);
    file.insert("filename".into(), filename.into());
    file.insert("extension".into(), extension.into());
    file
}

pub(super) struct FileOptions<'a> {
    pub(super) media: MediaOptions,
    pub(super) cards: &'a CardOptions,
    pub(super) articles: ArticleOptions,
    pub(super) images: &'a ImageOptions,
}

pub(super) struct ParsedPost {
    pub(super) metadata: PostMetadata,
    pub(super) media: Vec<Media>,
    /// Linked twitpic.com pages, resolved by the async caller.
    pub(super) twitpic: Vec<Url>,
    /// The tweet's keywords, for files appended later.
    pub(super) keywords: serde_json::Map<String, Value>,
}

impl ParsedPost {
    /// Append a twitpic image after the tweet's other files.
    pub(super) fn push_twitpic(&mut self, url: Url) {
        let num = self.media.len() + 1;
        let post_id = self.metadata.post_id.clone();
        let (name, ext) = crate::sites::nameext(&url);
        let ext = crate::path_options::map_extension(&ext).unwrap_or(ext);
        let mut fields = self.keywords.clone();
        fields.insert("filename".into(), name.into());
        fields.insert("extension".into(), ext.clone().into());
        fields.insert("num".into(), num.into());
        for item in &mut self.media {
            if let Some(keywords) = item
                .metadata
                .as_mut()
                .and_then(|m| m.site_keywords.as_mut())
            {
                Arc::make_mut(keywords)
                    .fields
                    .insert("count".into(), num.into());
            }
        }
        fields.insert("count".into(), num.into());
        self.media.push(Media {
            filename: format!("{post_id}_{num}.{ext}"),
            job: MediaJob::DirectHttp,
            handoff: BTreeMap::new(),
            category: "twitter".into(),
            metadata: Some(PostMetadata {
                media_id: num.to_string(),
                index: num,
                site_keywords: Some(Arc::new(crate::sites::SiteKeywords {
                    route: &TWEET,
                    fields,
                })),
                ..self.metadata.clone()
            }),
            url,
            headers: BTreeMap::from([
                ("User-Agent".into(), USER_AGENT.into()),
                ("Referer".into(), "https://x.com/".into()),
            ]),
        });
    }
}

/// Upstream `_extract_twitpic`: twitpic.com page links in the tweet's URL
/// entities and text (not direct `/photos/` images).
fn twitpic_links(legacy: &Value) -> Vec<Url> {
    let mut links: Vec<String> = Vec::new();
    for entity in legacy
        .pointer("/entities/urls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let url = entity
            .get("expanded_url")
            .or_else(|| entity.get("url"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if url.is_empty() || !url.contains("//twitpic.com/") || url.contains("/photos/") {
            continue;
        }
        let url = match url.strip_prefix("http:") {
            Some(rest) => format!("https:{rest}"),
            None => url.to_owned(),
        };
        if !links.contains(&url) {
            links.push(url);
        }
    }
    let text = legacy
        .get("full_text")
        .or_else(|| legacy.get("text"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    for (at, _) in text.match_indices("://twitpic.com/") {
        if !(text[..at].ends_with("http") || text[..at].ends_with("https")) {
            continue;
        }
        let rest = &text[at + "://twitpic.com/".len()..];
        let word: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if word.is_empty() || (word == "photos" && rest[word.len()..].starts_with('/')) {
            continue;
        }
        let url = format!("https://twitpic.com/{word}");
        if !links.contains(&url) {
            links.push(url);
        }
    }
    links
        .into_iter()
        .filter_map(|url| Url::parse(&url).ok())
        .collect()
}

/// The image of a twitpic.com page.
pub(super) fn twitpic_image(page: &str) -> Option<Url> {
    const META: &str = r#"name="twitter:image" value=""#;
    let start = page.find(META)? + META.len();
    let end = page[start..].find('"')? + start;
    Url::parse(&page[start..end]).ok()
}

pub(super) fn parse_tweet_media(
    tweet: &Value,
    requested_post_id: &str,
    allow_empty: bool,
    card_source: &Value,
    selection: FileOptions<'_>,
) -> Result<ParsedPost> {
    let FileOptions {
        media: options,
        cards: card_options,
        articles: article_options,
        images: image_options,
    } = selection;
    let twitpic = if options.twitpic {
        twitpic_links(&tweet["legacy"])
    } else {
        Vec::new()
    };
    let has_card = (card_options.mode != CardMode::Skip && card_source.get("card").is_some())
        || !twitpic.is_empty();
    // Cards are skipped by default (as upstream); say so when one is present.
    let no_media = || {
        if card_options.mode == CardMode::Skip && card_source.get("card").is_some() {
            error(
                "Twitter post has no downloadable media outside its card; \
                 set the cards option (-o cards=ytdl) to download card videos",
            )
        } else {
            error("Twitter post has no downloadable media")
        }
    };
    let has_article =
        article_options != ArticleOptions::disabled() && card_source.get("article").is_some();
    let empty = Vec::new();
    let legacy = &tweet["legacy"];
    let items = match legacy.get("extended_entities") {
        Some(extended) => extended
            .get("media")
            .and_then(Value::as_array)
            .ok_or_else(|| error("Twitter post media fields malformed"))?,
        None => {
            if legacy
                .pointer("/entities/media")
                .is_some_and(|v| !v.as_array().is_some_and(Vec::is_empty))
            {
                return Err(error("Twitter post extended media missing"));
            }
            if !allow_empty && !has_card && !has_article {
                return Err(no_media());
            }
            &empty
        }
    };
    if items.is_empty() && !allow_empty && !has_card && !has_article {
        return Err(no_media());
    }
    let username = tweet
        .pointer("/core/user_results/result/core/screen_name")
        .or_else(|| tweet.pointer("/core/user_results/result/legacy/screen_name"))
        .and_then(Value::as_str)
        .map(std::sync::Arc::from);
    let caption = tweet
        .pointer("/note_tweet/note_tweet_results/result/text")
        .or_else(|| legacy.get("full_text"))
        .and_then(Value::as_str)
        .map(std::sync::Arc::from);
    let timestamp = legacy
        .get("created_at")
        .and_then(Value::as_str)
        .and_then(timestamp);
    let mut selected =
        select_entities_with_keywords(items.iter(), requested_post_id, options, image_options)?;
    let (mut selected, mut file_fields): (Vec<_>, Vec<_>) = selected.drain(..).unzip();
    selected.extend(cards::select(
        card_source,
        requested_post_id,
        options,
        card_options,
        image_options,
    )?);
    let (article_files, article) =
        articles::select_with_images(card_source, article_options, image_options);
    selected.extend(article_files);
    // Cards and articles carry no per-entity keywords.
    file_fields.resize_with(selected.len(), serde_json::Map::new);
    let mut tweet_fields = transform_tweet(tweet, requested_post_id);
    tweet_fields.insert("count".into(), selected.len().into());
    // Raw article objects may carry malformed payloads; the parsed
    // article metadata is exposed instead.
    tweet_fields.remove("article");
    let community = community::native(card_source);
    let metadata = PostMetadata {
        community,
        article,
        post_id: requested_post_id.into(),
        username,
        caption,
        timestamp,
        ..Default::default()
    };
    // gallery-dl numbers files after selection, including previews after their video.
    let media = selected
        .into_iter()
        .zip(file_fields)
        .enumerate()
        .map(
            |(i, ((url, extension, job, fallback, file), mut fields))| Media {
                url: url.clone(),
                job,
                handoff: if fallback.is_empty() {
                    BTreeMap::new()
                } else {
                    BTreeMap::from([("_fallback".into(), serde_json::json!(fallback))])
                },
                category: "twitter".into(),
                filename: format!("{requested_post_id}_{}.{extension}", i + 1),
                metadata: Some(PostMetadata {
                    file: (file != FileMetadata::default()).then_some(file),
                    media_id: (i + 1).to_string(),
                    index: i + 1,
                    site_keywords: Some(Arc::new(crate::sites::SiteKeywords {
                        route: &TWEET,
                        fields: {
                            if !fields.contains_key("extension") {
                                let (name, ext) = crate::sites::nameext(&url);
                                let ext = crate::path_options::map_extension(&ext).unwrap_or(ext);
                                fields.insert("filename".into(), name.into());
                                fields.insert("extension".into(), ext.into());
                            }
                            let mut data = tweet_fields.clone();
                            data.extend(fields);
                            data.insert("num".into(), (i + 1).into());
                            data
                        },
                    })),
                    ..metadata.clone()
                }),
                headers: BTreeMap::from([
                    ("User-Agent".into(), USER_AGENT.into()),
                    ("Referer".into(), "https://x.com/".into()),
                ]),
            },
        )
        .collect();
    tweet_fields.remove("count");
    Ok(ParsedPost {
        metadata,
        media,
        twitpic,
        keywords: tweet_fields,
    })
}

pub(super) type SelectedFile = (Url, String, MediaJob, Vec<Url>, FileMetadata);

pub(super) fn select_entities<'a>(
    items: impl IntoIterator<Item = &'a Value>,
    requested_post_id: &str,
    options: MediaOptions,
    image_options: &ImageOptions,
) -> Result<Vec<SelectedFile>> {
    select_entities_with_keywords(items, requested_post_id, options, image_options)
        .map(|files| files.into_iter().map(|(file, _)| file).collect())
}

/// [`select_entities`] with each file's upstream keywords.
fn select_entities_with_keywords<'a>(
    items: impl IntoIterator<Item = &'a Value>,
    requested_post_id: &str,
    options: MediaOptions,
    image_options: &ImageOptions,
) -> Result<Vec<(SelectedFile, serde_json::Map<String, Value>)>> {
    let mut selected: Vec<(SelectedFile, serde_json::Map<String, Value>)> = Vec::new();
    for item in items {
        if !options.unavailable
            && item
                .pointer("/ext_media_availability/status")
                .and_then(Value::as_str)
                == Some("Unavailable")
        {
            continue;
        }
        let video = match item.get("type").and_then(Value::as_str) {
            Some("video" | "animated_gif") => true,
            Some("photo") => false,
            _ => return Err(error("unsupported Twitter media type")),
        };
        if video && options.videos == VideoMode::Skip && !options.previews {
            continue;
        }
        if video {
            match options.videos {
                VideoMode::Files => {
                    let (url, bitrate) = video_url(item)?;
                    let mut keywords = file_keywords(item, false, &url);
                    keywords.insert("bitrate".into(), bitrate.unwrap_or(0).into());
                    selected.push((
                        (
                            url,
                            "mp4".into(),
                            MediaJob::DirectHttp,
                            Vec::new(),
                            entity_metadata(item, false, bitrate),
                        ),
                        keywords,
                    ));
                }
                VideoMode::Skip => {}
                VideoMode::Ytdl => {
                    // Fixed first-party page; never take a delegate target from response data.
                    let page_url =
                        Url::parse(&format!("https://x.com/i/web/status/{requested_post_id}"))?;
                    let url = Url::parse(&format!("ytdl:{page_url}"))?;
                    let mut keywords = file_keywords(item, false, &url);
                    keywords.insert("extension".into(), "mp4".into());
                    selected.push((
                        (
                            url,
                            "mp4".into(),
                            MediaJob::VideoPage { page_url },
                            Vec::new(),
                            entity_metadata(item, false, None),
                        ),
                        keywords,
                    ));
                }
            }
            if !options.previews {
                continue;
            }
        }
        let (url, extension) = image_url(item)?;
        let (url, fallback) = image_options.apply(url);
        let keywords = file_keywords(item, video, &url);
        selected.push((
            (
                url,
                extension,
                MediaJob::DirectHttp,
                fallback,
                entity_metadata(item, video, None),
            ),
            keywords,
        ));
    }
    Ok(selected)
}

pub(super) fn dimension(value: Option<&Value>) -> Option<u64> {
    value.and_then(Value::as_u64).filter(|value| *value > 0)
}

pub(super) fn image_dimensions(value: &Value) -> FileMetadata {
    FileMetadata {
        width: dimension(value.get("width")),
        height: dimension(value.get("height")),
        width_original: dimension(value.get("width_original")),
        height_original: dimension(value.get("height_original")),
        ..Default::default()
    }
}

fn entity_metadata(item: &Value, preview: bool, bitrate: Option<u64>) -> FileMetadata {
    let info = item.get("original_info");
    let width = dimension(info.and_then(|info| info.get("width")));
    let height = dimension(info.and_then(|info| info.get("height")));
    let video = matches!(
        item.get("type").and_then(Value::as_str),
        Some("video" | "animated_gif")
    );
    let source_id = item
        .get("source_status_id_str")
        .and_then(Value::as_str)
        .filter(|id| valid_id(id))
        .map(str::to_owned);
    let source = item.pointer("/additional_media_info/source_user/user_results/result");
    let source_username = source
        .and_then(|source| {
            source
                .pointer("/core/screen_name")
                .or_else(|| source.pointer("/legacy/screen_name"))
        })
        .and_then(Value::as_str)
        .map(Arc::from);
    let flags: Arc<[Arc<str>]> = [
        ("adult_content", "Nudity"),
        ("other", "Sensitive"),
        ("graphic_violence", "Violence"),
    ]
    .into_iter()
    .filter(|(key, _)| {
        item.get("sensitive_media_warning")
            .is_some_and(|value| value.get(key).is_some())
    })
    .map(|(_, value)| Arc::from(value))
    .collect();
    FileMetadata {
        width,
        height,
        width_original: width,
        height_original: height,
        duration: (video && !preview)
            .then(|| {
                item.pointer("/video_info/duration_millis")
                    .and_then(Value::as_f64)
                    .filter(|value| value.is_finite() && *value >= 0.0)
                    .map(|value| value / 1000.0)
            })
            .flatten(),
        bitrate: if preview { None } else { bitrate },
        filesize: None,
        media_type: Some(Arc::from(if preview {
            "preview"
        } else {
            item["type"].as_str().unwrap()
        })),
        description: item
            .get("ext_alt_text")
            .and_then(Value::as_str)
            .map(Arc::from),
        sensitive_flags: flags,
        source_id,
        source_username,
    }
}

fn video_url(item: &Value) -> Result<(Url, Option<u64>)> {
    item.pointer("/video_info/variants")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|v| v.get("content_type").and_then(Value::as_str) == Some("video/mp4"))
        .filter_map(|v| {
            let url = v
                .get("url")
                .and_then(Value::as_str)
                .and_then(|s| http_url(s).ok())?;
            if !url.path().ends_with(".mp4") {
                return None;
            }
            Some((url, v.get("bitrate").and_then(Value::as_u64)))
        })
        .max_by_key(|(_, rate)| rate.unwrap_or(0))
        .ok_or_else(|| {
            error("Twitter video has no direct MP4 variant; thumbnail fallback is disabled")
        })
}

fn image_url(item: &Value) -> Result<(Url, String)> {
    let url = item
        .get("media_url_https")
        .or_else(|| item.get("media_url"))
        .and_then(Value::as_str)
        .ok_or_else(|| error("Twitter photo URL missing"))
        .and_then(http_url)
        .map_err(|_| error("Twitter photo URL invalid"))?;
    let supported = |ext: &str| matches!(ext, "jpg" | "jpeg" | "png" | "webp" | "avif");
    let extension = url
        .path()
        .rsplit_once('.')
        .map(|(_, ext)| ext)
        .filter(|ext| supported(ext))
        .map(str::to_owned)
        .or_else(|| {
            url.query_pairs()
                .find(|(k, v)| k == "format" && supported(v))
                .map(|(_, v)| v.into_owned())
        })
        .ok_or_else(|| error("Twitter photo has unsupported extension"))?;
    Ok((url, extension))
}

#[cfg(test)]
mod twitpic_tests {
    use super::*;

    #[test]
    fn twitpic_links_come_from_entities_and_text() {
        let legacy = serde_json::json!({
            "entities": {"urls": [
                {"expanded_url": "http://twitpic.com/abc1"},
                {"expanded_url": "https://twitpic.com/photos/large/1.jpg"},
                {"url": "https://t.co/x"}]},
            "full_text": "see https://twitpic.com/def2 and http://twitpic.com/abc1 \
                and https://twitpic.com/photos/x"});
        let links: Vec<_> = twitpic_links(&legacy)
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            links,
            ["https://twitpic.com/abc1", "https://twitpic.com/def2"]
        );
        let page =
            r#"<meta name="twitter:image" value="https://twitpic.com/photos/large/77.jpg" />"#;
        assert_eq!(
            twitpic_image(page).unwrap().as_str(),
            "https://twitpic.com/photos/large/77.jpg"
        );
    }
}
