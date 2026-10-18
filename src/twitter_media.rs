//! File selection shared by statuses, linked posts, and timeline traversal.
use super::*;
use crate::extract::MediaJob;

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

pub(super) fn parse_tweet_media(
    tweet: &Value,
    requested_post_id: &str,
    allow_empty: bool,
    options: MediaOptions,
    card_options: &CardOptions,
    card_source: &Value,
    article_options: ArticleOptions,
) -> Result<Vec<Media>> {
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
    let mut selected = select_entities(items.iter(), requested_post_id, options)?;
    selected.extend(cards::select(
        card_source,
        requested_post_id,
        options,
        card_options,
    )?);
    let (article_files, article) = articles::select(card_source, article_options);
    selected.extend(article_files);
    // gallery-dl numbers files after selection, including previews after their video.
    Ok(selected
        .into_iter()
        .enumerate()
        .map(|(i, (url, extension, job))| Media {
            url,
            job,
            handoff: BTreeMap::new(),
            category: "twitter".into(),
            filename: format!("{requested_post_id}_{}.{extension}", i + 1),
            metadata: Some(PostMetadata {
                retweet_id: None,
                quoted_by_id: None,
                reposted_by_id: None,
                article: article.clone(),
                post_id: requested_post_id.into(),
                media_id: (i + 1).to_string(),
                shortcode: String::new(),
                username: username.clone(),
                caption: caption.clone(),
                timestamp,
                index: i + 1,
            }),
            headers: BTreeMap::from([
                ("User-Agent".into(), USER_AGENT.into()),
                ("Referer".into(), "https://x.com/".into()),
            ]),
        })
        .collect())
}

pub(super) type SelectedFile = (Url, String, MediaJob);

pub(super) fn select_entities<'a>(
    items: impl IntoIterator<Item = &'a Value>,
    requested_post_id: &str,
    options: MediaOptions,
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
                    selected.push((video_url(item)?, "mp4".into(), MediaJob::DirectHttp))
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
                    ));
                }
            }
            if !options.previews {
                continue;
            }
        }
        let (url, extension) = image_url(item)?;
        selected.push((url, extension, MediaJob::DirectHttp));
    }
    Ok(selected)
}

fn video_url(item: &Value) -> Result<Url> {
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
            Some((url, v.get("bitrate").and_then(Value::as_u64).unwrap_or(0)))
        })
        .max_by_key(|(_, rate)| *rate)
        .map(|(url, _)| url)
        .ok_or_else(|| {
            error("Twitter video has no direct MP4 variant; thumbnail fallback is disabled")
        })
}

fn image_url(item: &Value) -> Result<(Url, String)> {
    let mut url = item
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
    // Preserve signed/other query parameters, replacing only the size.
    let pairs: Vec<_> = url
        .query_pairs()
        .filter(|(k, _)| k != "name")
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    url.set_query(None);
    url.query_pairs_mut()
        .extend_pairs(pairs)
        .append_pair("name", "orig");
    Ok((url, extension))
}
