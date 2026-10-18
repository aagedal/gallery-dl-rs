//! Identity-bound Facebook video delivery. Manifests are never fetched here.
use crate::{
    Result, error,
    extract::{Media, MediaJob, PostMetadata, http_url},
    facebook::{embedded_payloads, has_error_root},
};
use serde_json::Value;
use std::collections::BTreeMap;
use url::Url;

pub fn video_id(url: &Url) -> Result<String> {
    if !matches!(url.scheme(), "http" | "https")
        || !matches!(
            url.host_str(),
            Some("facebook.com" | "www.facebook.com" | "m.facebook.com")
        )
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return Err(error("native video extraction requires a Facebook URL"));
    }
    let path = url.path().trim_end_matches('/');
    let id = if path == "/watch" {
        let ids: Vec<_> = url
            .query_pairs()
            .filter(|(key, _)| key == "v")
            .map(|(_, value)| value.into_owned())
            .collect();
        if ids.len() != 1 {
            return Err(error("Facebook watch URL requires exactly one v parameter"));
        }
        ids[0].clone()
    } else {
        let parts: Vec<_> = path.split('/').collect();
        if parts.len() != 4 || parts[1].is_empty() || parts[2] != "videos" {
            return Err(error(
                "unsupported Facebook video URL; use --extractor compatibility",
            ));
        }
        if url.query_pairs().any(|(key, _)| key == "v") {
            return Err(error(
                "Facebook video URL has conflicting identity parameters",
            ));
        }
        parts[3].into()
    };
    validate_id(&id)?;
    Ok(id)
}

fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 32
        || !id.bytes().all(|b| b.is_ascii_digit())
        || id.bytes().all(|b| b == b'0')
    {
        return Err(error("invalid Facebook video identifier"));
    }
    Ok(())
}

/// Extract only delivery fields owned by the exact requested Video. DASH video
/// and audio are separate complete resources, matching upstream's file model.
pub fn parse_video(bytes: &[u8], requested_id: &str) -> Result<Vec<Media>> {
    validate_id(requested_id)?;
    let payloads = embedded_payloads(bytes)?;
    let mut nodes = Vec::new();
    for value in &payloads {
        find_videos(value, requested_id, &mut nodes);
    }
    let mut chosen: Option<Vec<Media>> = None;
    for node in nodes {
        let Some(urls) = delivery(node)? else {
            continue;
        };
        let media: Vec<_> = urls
            .into_iter()
            .enumerate()
            .map(|(index, (url, extension))| Media {
                url,
                job: MediaJob::DirectHttp,
                category: "facebook".into(),
                filename: format!("{requested_id}.{extension}"),
                handoff: BTreeMap::new(),
                metadata: Some(PostMetadata {
                    retweet_id: None,
                    quoted_by_id: None,
                    reposted_by_id: None,
                    article: None,
                    post_id: requested_id.into(),
                    media_id: format!("{requested_id}.{extension}"),
                    shortcode: String::new(),
                    username: node
                        .pointer("/owner/username")
                        .and_then(Value::as_str)
                        .filter(|name| !name.is_empty())
                        .map(std::sync::Arc::from),
                    caption: node
                        .pointer("/creation_story/message/text")
                        .or_else(|| node.pointer("/message/text"))
                        .and_then(Value::as_str)
                        .map(std::sync::Arc::from),
                    timestamp: node
                        .get("publish_time")
                        .or_else(|| node.get("created_time"))
                        .and_then(Value::as_i64),
                    index: index + 1,
                }),
                headers: BTreeMap::from([
                    ("User-Agent".into(), super::facebook::USER_AGENT.into()),
                    ("Referer".into(), "https://www.facebook.com/".into()),
                ]),
            })
            .collect();
        if let Some(prior) = &chosen {
            if prior
                .iter()
                .map(|item| &item.url)
                .ne(media.iter().map(|item| &item.url))
            {
                return Err(error(
                    "Facebook returned ambiguous delivery for the requested video",
                ));
            }
        } else {
            chosen = Some(media);
        }
    }
    if let Some(media) = chosen {
        return Ok(media);
    }
    if payloads.iter().any(has_error_root) {
        return Err(error(
            "Facebook temporarily blocked video access; retry later",
        ));
    }
    let page = std::str::from_utf8(bytes).map_err(|_| error("Facebook returned invalid UTF-8"))?;
    if page.contains("login_form")
        || page.contains("login.php?next=")
        || page.contains("checkpoint_url")
    {
        return Err(error(
            "Facebook authentication required or session challenge; refresh --cookies",
        ));
    }
    Err(error(
        "Facebook requested video unavailable or missing identity-bound media; login may be required",
    ))
}

fn find_videos<'a>(value: &'a Value, id: &str, nodes: &mut Vec<&'a Value>) {
    match value {
        Value::Object(object) => {
            let typed = ["__typename", "__isNode"]
                .iter()
                .any(|key| value.get(key).and_then(Value::as_str) == Some("Video"));
            let types_agree = ["__typename", "__isNode"].iter().all(|key| {
                value
                    .get(key)
                    .is_none_or(|value| value.as_str() == Some("Video"))
            });
            let matches = if typed {
                value.get("id").and_then(Value::as_str) == Some(id)
            } else {
                !object.contains_key("__typename")
                    && !object.contains_key("__isNode")
                    && value.get("video_id").and_then(Value::as_str) == Some(id)
            };
            let aliases_agree = ["id", "video_id", "videoId"].iter().all(|key| {
                value
                    .get(key)
                    .is_none_or(|value| value.as_str() == Some(id))
            });
            if matches && aliases_agree && types_agree {
                nodes.push(value);
            }
            for child in object.values() {
                find_videos(child, id, nodes);
            }
        }
        Value::Array(array) => {
            for child in array {
                find_videos(child, id, nodes);
            }
        }
        _ => {}
    }
}

fn delivery(node: &Value) -> Result<Option<Vec<(Url, &'static str)>>> {
    let legacy = node
        .get("videoDeliveryLegacyFields")
        .filter(|value| value.as_object().is_some_and(|object| !object.is_empty()))
        .unwrap_or(node);
    let modern = node.pointer("/videoDeliveryResponseFragment/videoDeliveryResponseResult");
    let mut manifests = Vec::new();
    for key in ["dash_manifest", "dash_manifest_xml_string", "manifest_xml"] {
        if let Some(value) = legacy.get(key).filter(|v| !v.is_null()) {
            manifests.push(
                value
                    .as_str()
                    .ok_or_else(|| error("Facebook returned invalid video manifest data"))?,
            );
        }
    }
    if let Some(values) = modern
        .and_then(|v| v.get("dash_manifests"))
        .filter(|v| !v.is_null())
    {
        for value in values
            .as_array()
            .ok_or_else(|| error("Facebook returned invalid video manifests"))?
        {
            let manifest = value
                .get("manifest_xml")
                .and_then(Value::as_str)
                .ok_or_else(|| error("Facebook returned incomplete video manifest data"))?;
            manifests.push(manifest);
        }
    }
    manifests.retain(|xml| !xml.is_empty());
    if !manifests.is_empty() {
        let mut chosen = None;
        for xml in manifests {
            let parsed = crate::facebook_dash::parse_manifest(xml)?;
            if chosen.as_ref().is_some_and(|prior| prior != &parsed) {
                return Err(error("Facebook returned ambiguous video manifests"));
            }
            chosen = Some(parsed);
        }
        return Ok(chosen);
    }
    // Legacy and current progressive URLs are full muxed files. Prefer HD over
    // SD when no inline DASH representation is supplied.
    let mut progressive = Vec::new();
    for (key, quality) in [
        ("playable_url_quality_hd", 2),
        ("browser_native_hd_url", 2),
        ("playable_url", 1),
        ("browser_native_sd_url", 1),
    ] {
        if let Some(value) = legacy.get(key).filter(|v| !v.is_null()) {
            if value.as_str() == Some("") {
                continue;
            }
            progressive.push((quality, direct_mp4(value)?));
        }
    }
    if let Some(values) = modern
        .and_then(|v| v.get("progressive_urls"))
        .filter(|v| !v.is_null())
    {
        for value in values
            .as_array()
            .ok_or_else(|| error("Facebook returned invalid progressive video data"))?
        {
            let quality = match value
                .pointer("/metadata/quality")
                .and_then(Value::as_str)
                .map(str::to_ascii_lowercase)
                .as_deref()
            {
                Some("hd") => 2,
                Some("sd") => 1,
                _ => 0,
            };
            progressive.push((
                quality,
                direct_mp4(value.get("progressive_url").ok_or_else(|| {
                    error("Facebook returned incomplete progressive video data")
                })?)?,
            ));
        }
    }
    progressive.sort_by_key(|(quality, _)| std::cmp::Reverse(*quality));
    if let Some((quality, url)) = progressive.first() {
        if progressive
            .iter()
            .skip(1)
            .any(|(other_quality, other_url)| other_quality == quality && other_url != url)
        {
            return Err(error("Facebook returned ambiguous progressive video URLs"));
        }
        return Ok(Some(vec![(url.clone(), "mp4")]));
    }
    if legacy
        .get("playable_url_dash")
        .is_some_and(|v| !v.is_null())
        || modern.is_some_and(|v| {
            ["dash_manifest_urls", "hls_playlist_urls"]
                .iter()
                .any(|key| {
                    v.get(key)
                        .is_some_and(|value| value.as_array().is_none_or(|array| !array.is_empty()))
                })
        })
    {
        return Err(error(
            "Facebook external or segmented video manifests require --extractor compatibility",
        ));
    }
    Ok(None)
}

fn direct_mp4(value: &Value) -> Result<Url> {
    let invalid = || error("Facebook returned an unsupported direct video URL");
    let url = http_url(value.as_str().ok_or_else(invalid)?).map_err(|_| invalid())?;
    if !url.path().to_ascii_lowercase().ends_with(".mp4") {
        return Err(invalid());
    }
    Ok(url)
}
