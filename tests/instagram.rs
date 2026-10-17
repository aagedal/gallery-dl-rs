use gallery_dl_rs::{
    extract::http_url,
    instagram::{parse_response, shortcode},
};
use serde_json::Value;

const PHOTO: &[u8] = include_bytes!("fixtures/instagram/photo.json");
const VIDEO: &[u8] = include_bytes!("fixtures/instagram/video.json");
const CAROUSEL: &[u8] = include_bytes!("fixtures/instagram/carousel.json");

#[test]
fn single_photo_preserves_metadata_and_selects_largest_variant() {
    let media = parse_response(PHOTO, "BA").unwrap();
    assert_eq!(media.len(), 1);
    assert_eq!(media[0].url.as_str(), "https://cdn.example/64.jpg");
    assert_eq!(media[0].filename, "64_64.jpg");
    let metadata = media[0].metadata.as_ref().unwrap();
    assert_eq!(metadata.post_id, "64");
    assert_eq!(metadata.media_id, "64");
    assert_eq!(metadata.username.as_deref(), Some("fixture_user"));
    assert_eq!(metadata.timestamp, Some(1700000000));
    assert_eq!(metadata.caption.as_deref(), Some("Synthetic public post"));
    assert!(media[0].headers.contains_key("User-Agent"));
    assert!(media[0].headers.contains_key("Referer"));
    assert!(!media[0].headers.contains_key("Cookie"));
    assert!(!serde_json::to_string(&media).unwrap().contains("headers"));
}

#[test]
fn direct_video_and_full_mixed_carousel_keep_order() {
    let video = parse_response(VIDEO, "BA").unwrap();
    assert_eq!(video[0].url.as_str(), "https://cdn.example/64.mp4");
    let media = parse_response(CAROUSEL, "BA").unwrap();
    assert_eq!(media.len(), 3);
    for (index, item) in media.iter().enumerate() {
        assert_eq!(item.metadata.as_ref().unwrap().index, index + 1);
    }
    assert_eq!(media[1].filename, "64_66.mp4");
    assert_eq!(media[2].url.as_str(), "https://cdn.example/67.jpg");
}

#[test]
fn distinguishes_authentication_missing_and_rate_limit_failures() {
    for (fixture, expected) in [
        (
            include_bytes!("fixtures/instagram/login.json").as_slice(),
            "authentication",
        ),
        (
            include_bytes!("fixtures/instagram/expired.json").as_slice(),
            "session",
        ),
        (
            include_bytes!("fixtures/instagram/missing.json").as_slice(),
            "unavailable",
        ),
        (
            include_bytes!("fixtures/instagram/rate-limit.json").as_slice(),
            "rate limit",
        ),
        (b"<html>Login</html>".as_slice(), "login"),
    ] {
        assert!(
            parse_response(fixture, "BA")
                .unwrap_err()
                .to_string()
                .contains(expected)
        );
    }
}

#[test]
fn rejects_incomplete_carousels_and_video_without_direct_variant() {
    let mut data: Value = serde_json::from_slice(CAROUSEL).unwrap();
    data["items"][0]["carousel_media_count"] = 4.into();
    assert!(parse_response(&serde_json::to_vec(&data).unwrap(), "BA").is_err());
    data["items"][0]["carousel_media_count"] = 3.into();
    data["items"][0]["carousel_media"][1]["video_versions"] = Value::Null;
    assert!(parse_response(&serde_json::to_vec(&data).unwrap(), "BA").is_err());
    for url in [
        "file:///tmp/video.mp4",
        "https://user:pass@cdn.example/v.mp4",
        "https://cdn.example/v.m3u8",
    ] {
        let mut data: Value = serde_json::from_slice(VIDEO).unwrap();
        data["items"][0]["video_versions"][1]["url"] = url.into();
        assert!(parse_response(&serde_json::to_vec(&data).unwrap(), "BA").is_err());
    }
    assert!(parse_response(PHOTO, "different").is_err());
}

#[test]
fn accepts_only_single_post_urls_on_instagram_hosts() {
    for path in ["p", "reel", "tv"] {
        let url = http_url(&format!("https://www.instagram.com/{path}/BA/?img_index=2")).unwrap();
        assert_eq!(shortcode(&url).unwrap(), "BA");
    }
    for url in [
        "https://instagram.com.evil.test/p/BA/",
        "https://instagram.com/profile/",
        "https://instagram.com/p/BA/extra",
        "https://instagram.com/p/%41/",
        "https://instagram.com/p/zzzzzzzzzzz/",
        "https://instagram.com:8443/p/BA/",
    ] {
        assert!(shortcode(&http_url(url).unwrap()).is_err(), "{url}");
    }
}
