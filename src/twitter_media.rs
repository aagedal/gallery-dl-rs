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
    let has_card = card_options.mode != CardMode::Skip && card_source.get("card").is_some();
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
                return Err(error("Twitter post has no downloadable media"));
            }
            &empty
        }
    };
    if items.is_empty() && !allow_empty && !has_card && !has_article {
        return Err(error("Twitter post has no downloadable media"));
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
    let mut selected = select_entities(items.iter(), requested_post_id, options, image_options)?;
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
        .enumerate()
        .map(|(i, (url, extension, job, fallback, file))| Media {
            url,
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
                ..metadata.clone()
            }),
            headers: BTreeMap::from([
                ("User-Agent".into(), USER_AGENT.into()),
                ("Referer".into(), "https://x.com/".into()),
            ]),
        })
        .collect();
    Ok(ParsedPost { metadata, media })
}

pub(super) type SelectedFile = (Url, String, MediaJob, Vec<Url>, FileMetadata);

pub(super) fn select_entities<'a>(
    items: impl IntoIterator<Item = &'a Value>,
    requested_post_id: &str,
    options: MediaOptions,
    image_options: &ImageOptions,
) -> Result<Vec<SelectedFile>> {
    let mut selected = Vec::new();
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
                    selected.push((
                        url,
                        "mp4".into(),
                        MediaJob::DirectHttp,
                        Vec::new(),
                        entity_metadata(item, false, bitrate),
                    ));
                }
                VideoMode::Skip => {}
                VideoMode::Ytdl => {
                    // Fixed first-party page; never take a delegate target from response data.
                    let page_url =
                        Url::parse(&format!("https://x.com/i/web/status/{requested_post_id}"))?;
                    selected.push((
                        Url::parse(&format!("ytdl:{page_url}"))?,
                        "mp4".into(),
                        MediaJob::VideoPage { page_url },
                        Vec::new(),
                        entity_metadata(item, false, None),
                    ));
                }
            }
            if !options.previews {
                continue;
            }
        }
        let (url, extension) = image_url(item)?;
        let (url, fallback) = image_options.apply(url);
        selected.push((
            url,
            extension,
            MediaJob::DirectHttp,
            fallback,
            entity_metadata(item, video, None),
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
