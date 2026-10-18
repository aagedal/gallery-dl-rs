use gallery_dl_rs::{
    extract::http_url,
    twitter::{parse_response, post_id},
};
use serde_json::{Value, json};
const ID: &str = "1234567890";
const PHOTO: &[u8] = include_bytes!("fixtures/twitter/photo.json");
const MIXED: &[u8] = include_bytes!("fixtures/twitter/mixed.json");

#[test]
fn photo_original_and_mixed_direct_video_preserve_archive_identity() {
    let media = parse_response(MIXED, ID).unwrap();
    assert_eq!(media.len(), 3);
    assert_eq!(
        media[0].url.as_str(),
        "https://pbs.example/media/fixture.jpg?name=orig"
    );
    assert_eq!(
        media[1].url.as_str(),
        "https://video.example/high.mp4?tag=1"
    );
    for (i, item) in media.iter().enumerate() {
        let post = item.metadata.as_ref().unwrap();
        assert_eq!(post.post_id, ID);
        assert_eq!(post.media_id, (i + 1).to_string());
        assert_eq!(post.index, i + 1);
        assert_eq!(post.shortcode, "");
        assert_eq!(post.username.as_deref(), Some("fixture_user"));
        assert_eq!(post.caption.as_deref(), Some("Synthetic post"));
        assert_eq!(post.timestamp, Some(1700000000));
        assert!(item.archive_key().is_some());
        assert_eq!(item.headers.len(), 2);
        assert!(item.headers.contains_key("Referer"));
        assert!(item.headers.contains_key("User-Agent"));
        assert!(!serde_json::to_string(item).unwrap().contains("headers"));
    }
    assert_eq!(media[1].filename, "1234567890_2.mp4");
}

#[test]
fn distinguishes_unavailable_protected_authentication_and_rate_errors_without_payload() {
    for (fixture, expected) in [
        (
            include_bytes!("fixtures/twitter/protected.json").as_slice(),
            "protected",
        ),
        (
            include_bytes!("fixtures/twitter/missing.json").as_slice(),
            "unavailable",
        ),
        (
            include_bytes!("fixtures/twitter/expired.json").as_slice(),
            "authentication",
        ),
        (
            include_bytes!("fixtures/twitter/rate-limit.json").as_slice(),
            "rate limit",
        ),
        (b"<html>secret session</html>".as_slice(), "invalid JSON"),
        (
            br#"{"errors":[{"code":123456,"message":"secret session"}]}"#.as_slice(),
            "rejected",
        ),
    ] {
        let err = parse_response(fixture, ID).unwrap_err().to_string();
        assert!(err.contains(expected), "{err}");
        assert!(!err.contains("secret"));
    }
}

#[test]
fn unwraps_visibility_rejects_wrong_id_and_skips_retweets() {
    let mut data: Value = serde_json::from_slice(PHOTO).unwrap();
    let root = data["data"]["tweetResult"]["result"].take();
    data["data"]["tweetResult"]["result"] =
        json!({"__typename":"TweetWithVisibilityResults","tweet":root});
    assert_eq!(
        parse_response(&serde_json::to_vec(&data).unwrap(), ID)
            .unwrap()
            .len(),
        1
    );
    assert!(
        parse_response(&serde_json::to_vec(&data).unwrap(), "987654321")
            .unwrap_err()
            .to_string()
            .contains("does not match")
    );
    let mut data: Value = serde_json::from_slice(PHOTO).unwrap();
    data["data"]["tweetResult"]["result"]["legacy"]["retweeted_status_id_str"] = "987654321".into();
    assert!(
        parse_response(&serde_json::to_vec(&data).unwrap(), ID)
            .unwrap()
            .is_empty()
    );
    data["data"]["tweetResult"]["result"]["legacy"]["id_str"] = "987654321".into();
    assert!(
        parse_response(&serde_json::to_vec(&data).unwrap(), ID)
            .unwrap_err()
            .to_string()
            .contains("does not match")
    );
}

#[test]
fn no_partial_mixed_post_or_thumbnail_fallback_for_unsupported_media() {
    for variants in [
        json!([]),
        json!([{"content_type":"application/x-mpegURL","url":"https://cdn.example/v.m3u8"}]),
        json!([{"content_type":"video/mp4","url":"file:///tmp/v.mp4"}]),
        json!([{"content_type":"video/mp4","url":"https://user:secret@cdn.example/v.mp4"}]),
        json!([{"content_type":"video/mp4","url":"https://cdn.example/v.m3u8"}]),
    ] {
        let mut data: Value = serde_json::from_slice(MIXED).unwrap();
        data["data"]["tweetResult"]["result"]["legacy"]["extended_entities"]["media"][1]["video_info"]
            ["variants"] = variants;
        let err = parse_response(&serde_json::to_vec(&data).unwrap(), ID)
            .unwrap_err()
            .to_string();
        assert!(err.contains("direct MP4"));
        assert!(!err.contains("secret"));
    }
    for url in [
        "file:///secret/image.jpg",
        "https://cdn.example/image.exe",
        "https://user:secret@cdn.example/photo.jpg",
    ] {
        let mut data: Value = serde_json::from_slice(PHOTO).unwrap();
        data["data"]["tweetResult"]["result"]["legacy"]["extended_entities"]["media"][0]["media_url_https"] =
            url.into();
        assert!(parse_response(&serde_json::to_vec(&data).unwrap(), ID).is_err());
    }
}

#[test]
fn accepts_first_party_post_urls_only() {
    for url in [
        "https://x.com/fixture/status/1234567890",
        "https://twitter.com/i/web/status/1234567890",
        "https://mobile.twitter.com/user/status/1234567890/photo/2?x=1",
    ] {
        assert_eq!(post_id(&http_url(url).unwrap()).unwrap(), ID);
    }
    for url in [
        "https://x.com/user",
        "https://x.com.evil/user/status/1234567890",
        "https://x.com/user/status/0",
        "https://x.com/user/status/001",
        "https://x.com/user/status/18446744073709551616",
        "https://x.com/user/status/1234567890/extra",
        "https://x.com:444/user/status/1234567890",
    ] {
        assert!(post_id(&http_url(url).unwrap()).is_err(), "{url}");
    }
}

#[test]
fn current_user_core_metadata_takes_precedence_over_legacy() {
    let mut data: Value = serde_json::from_slice(PHOTO).unwrap();
    data["data"]["tweetResult"]["result"]["core"]["user_results"]["result"]["core"] =
        json!({"screen_name":"current_user"});
    let media = parse_response(&serde_json::to_vec(&data).unwrap(), ID).unwrap();
    assert_eq!(
        media[0].metadata.as_ref().unwrap().username.as_deref(),
        Some("current_user")
    );
}
