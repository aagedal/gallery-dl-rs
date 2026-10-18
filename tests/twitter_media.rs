use gallery_dl_rs::{
    extract::{Media, MediaJob},
    twitter::{
        ExpansionOptions, MediaOptions, VideoMode, parse_response,
        parse_response_with_media_options,
    },
};
use serde_json::{Value, json};

const ID: &str = "1234567890";
fn mixed() -> Value {
    serde_json::from_slice(include_bytes!("fixtures/twitter/mixed.json")).unwrap()
}
fn parse(value: &Value, videos: VideoMode, previews: bool) -> gallery_dl_rs::Result<Vec<Media>> {
    parse_response_with_media_options(
        &serde_json::to_vec(value).unwrap(),
        ID,
        ExpansionOptions::default(),
        MediaOptions { videos, previews },
    )
}
fn entries(value: &mut Value) -> &mut Vec<Value> {
    value["data"]["tweetResult"]["result"]["legacy"]["extended_entities"]["media"]
        .as_array_mut()
        .unwrap()
}

#[test]
fn selected_files_are_ordered_and_numbered_after_video_and_preview_choices() {
    let data = mixed();
    for (videos, previews, expected) in [
        (
            VideoMode::Files,
            false,
            vec!["fixture.jpg", "high.mp4", "fixture.jpg"],
        ),
        (
            VideoMode::Files,
            true,
            vec!["fixture.jpg", "high.mp4", "thumbnail.jpg", "fixture.jpg"],
        ),
        (VideoMode::Skip, false, vec!["fixture.jpg", "fixture.jpg"]),
        (
            VideoMode::Skip,
            true,
            vec!["fixture.jpg", "thumbnail.jpg", "fixture.jpg"],
        ),
    ] {
        let media = parse(&data, videos, previews).unwrap();
        assert_eq!(media.len(), expected.len());
        for (i, (item, expected)) in media.iter().zip(expected).enumerate() {
            assert!(item.url.path().ends_with(expected));
            let metadata = item.metadata.as_ref().unwrap();
            assert_eq!(metadata.post_id, ID);
            assert_eq!(metadata.index, i + 1);
            assert_eq!(metadata.media_id, (i + 1).to_string());
            assert_eq!(metadata.username.as_deref(), Some("fixture_user"));
            assert_eq!(metadata.timestamp, Some(1700000000));
            assert!(item.archive_key().is_some());
            assert_eq!(
                item.filename,
                format!("{ID}_{}.{}", i + 1, expected.rsplit('.').next().unwrap())
            );
            assert!(matches!(item.job, MediaJob::DirectHttp));
        }
    }
    assert_eq!(
        serde_json::to_value(parse(&data, VideoMode::Files, false).unwrap()).unwrap(),
        serde_json::to_value(parse_response(&serde_json::to_vec(&data).unwrap(), ID).unwrap())
            .unwrap(),
    );
}

#[test]
fn animated_gifs_support_poster_only_and_skip_without_reading_variants() {
    let mut data = mixed();
    let mut gif = entries(&mut data)[1].clone();
    gif["type"] = json!("animated_gif");
    gif["video_info"] =
        json!({"variants":[{"content_type":"video/mp4","url":"https://cdn.example/gif.mp4"}]});
    *entries(&mut data) = vec![gif];
    assert!(
        parse(&data, VideoMode::Files, false).unwrap()[0]
            .url
            .path()
            .ends_with("gif.mp4")
    );
    assert_eq!(parse(&data, VideoMode::Files, true).unwrap().len(), 2);
    entries(&mut data)[0]["video_info"] = json!({"variants":"private malformed variant"});
    assert!(parse(&data, VideoMode::Skip, false).unwrap().is_empty());
    assert_eq!(
        parse(&data, VideoMode::Skip, true).unwrap()[0].url.path(),
        "/thumbnail.jpg"
    );
    let error = parse(&data, VideoMode::Files, true)
        .unwrap_err()
        .to_string();
    assert!(error.contains("direct MP4"));
    assert!(!error.contains("private"));
}

#[test]
fn requested_previews_validate_atomically_without_silent_video_fallback() {
    for invalid in [
        json!(null),
        json!("file:///private/poster.jpg"),
        json!("https://user:private@cdn.example/p.jpg"),
        json!("https://cdn.example/private.exe"),
    ] {
        let mut data = mixed();
        entries(&mut data)[1]["media_url_https"] = invalid;
        // The valid video remains usable without a requested preview.
        assert_eq!(parse(&data, VideoMode::Files, false).unwrap().len(), 3);
        for videos in [VideoMode::Files, VideoMode::Skip, VideoMode::Ytdl] {
            let error = parse(&data, videos, true).unwrap_err().to_string();
            assert!(error.contains("photo"));
            assert!(!error.contains("private"));
        }
    }
    let mut data = mixed();
    entries(&mut data)[1]["video_info"] = json!({"variants":[]});
    assert!(parse(&data, VideoMode::Files, true).is_err());
    assert_eq!(parse(&data, VideoMode::Skip, true).unwrap().len(), 3);
}

#[test]
fn ytdl_selection_uses_only_validated_first_party_identity_and_preserves_poster_order() {
    let mut data = mixed();
    entries(&mut data)[1]["video_info"] = json!({"variants":[{"url":"file:///private/video.mp4"}]});
    for previews in [false, true] {
        let media = parse(&data, VideoMode::Ytdl, previews).unwrap();
        let item = &media[1];
        assert_eq!(
            item.url.as_str(),
            format!("ytdl:https://x.com/i/web/status/{ID}")
        );
        assert!(
            matches!(&item.job, MediaJob::VideoPage {page_url} if page_url.as_str() == format!("https://x.com/i/web/status/{ID}"))
        );
        assert_eq!(media.len(), if previews { 4 } else { 3 });
        assert!(item.archive_key().is_some());
        assert!(item.handoff.is_empty());
    }
    data["data"]["tweetResult"]["result"]["legacy"]["id_str"] = json!("999");
    assert!(parse(&data, VideoMode::Ytdl, false).is_err());
}

#[test]
fn preview_images_preserve_signed_query_and_normalize_only_size() {
    let mut data = mixed();
    entries(&mut data)[1]["media_url_https"] =
        json!("https://pbs.example/poster?format=webp&name=small&token=signature&name=large");
    let media = parse(&data, VideoMode::Files, true).unwrap();
    let poster = &media[2];
    assert_eq!(poster.filename, format!("{ID}_3.webp"));
    let pairs: Vec<_> = poster
        .url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    assert_eq!(
        pairs,
        [
            ("format".into(), "webp".into()),
            ("token".into(), "signature".into()),
            ("name".into(), "orig".into())
        ]
    );
    assert!(!poster.headers.contains_key("Cookie"));
    assert!(!serde_json::to_string(poster).unwrap().contains("headers"));
}
