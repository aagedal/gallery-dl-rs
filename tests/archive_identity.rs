use gallery_dl_rs::{
    extract::{self, Media},
    instagram,
};
use serde_json::{Value, json};

fn compatibility(category: &str, metadata: Value, token: &str) -> Media {
    let messages = json!([[
        3,
        format!("https://cdn.example/media.jpg?token={token}"),
        metadata
    ]]);
    extract::parse_messages(&serde_json::to_vec(&messages).unwrap(), category)
        .unwrap()
        .remove(0)
}

#[test]
fn novel_identity_matches_native_and_separates_illustrations_and_covers() {
    let raw = "日本語\r\n[newpage]\n100% %20 [pixivimage:42-1]\n";
    let mut native = Media {
        url: extract::inline_text(raw.into()).unwrap().0,
        job: extract::inline_text(raw.into()).unwrap().1,
        category: "pixiv-novel".into(),
        filename: "42_p0.txt".into(),
        metadata: Some(extract::PostMetadata {
            post_id: "42".into(),
            media_id: "42_p0".into(),
            index: 1,
            ..Default::default()
        }),
        headers: Default::default(),
        handoff: Default::default(),
    };
    let compat = extract::parse_messages(
        &serde_json::to_vec(&json!([[3, format!("text:{raw}"), {
            "id":42, "num":0, "filename":"42_p0", "extension":"txt",
            "title":"Title", "caption":"<p>Caption</p>",
            "user":{"account":"author"}
        }]]))
        .unwrap(),
        "pixiv-novel",
    )
    .unwrap()
    .remove(0);
    assert_eq!(native.archive_key(), compat.archive_key());
    assert!(matches!(&compat.job, extract::MediaJob::InlineText { data } if data == raw));
    let post = compat.metadata.as_ref().unwrap();
    assert_eq!(post.index, 1);
    assert_eq!(post.caption.as_deref(), Some("Title\n\nCaption"));
    assert_eq!(post.username.as_deref(), Some("author"));
    assert_ne!(
        native.archive_key(),
        compatibility("pixiv", json!({"id":42,"num":0}), "a").archive_key()
    );
    let cover = compatibility("pixiv-novel", json!({"id":42,"num":1}), "a");
    assert_ne!(native.archive_key(), cover.archive_key());
    let key = native.archive_key().unwrap();
    native.job = extract::inline_text("edited text".into()).unwrap().1;
    assert_eq!(native.archive_key().as_deref(), Some(key.as_str()));
    for private in ["日本語", "pixivimage", "Title", "author", "https"] {
        assert!(!key.contains(private));
    }
    native.metadata.as_mut().unwrap().media_id = "42_bad".into();
    assert!(native.archive_key().is_none());
}

#[test]
fn native_and_compatibility_instagram_use_the_same_stable_identity() {
    let native =
        instagram::parse_response(include_bytes!("fixtures/instagram/photo.json"), "BA").unwrap();
    let metadata = native[0].metadata.as_ref().unwrap();
    let mut compat = compatibility(
        "instagram",
        json!({
            "post_id": metadata.post_id, "media_id": metadata.media_id,
            "post_shortcode": "BA", "shortcode": "child-code", "num": 1,
            "username": "alice", "description": "caption", "date": 123,
            "_http_headers": {"Cookie": "secret"}, "_ytdl_private": "private-value",
            "password": "never-public", "user": {"sessionid": "never-public"}
        }),
        "old-secret",
    );
    assert_eq!(native[0].archive_key(), compat.archive_key());
    let old_request = compat.identity();
    let key = compat.archive_key().unwrap();
    compat.url = extract::http_url("https://other-cdn.example/new.jpg?token=new-secret").unwrap();
    compat
        .headers
        .insert("Cookie".into(), "rotated-cookie".into());
    assert_ne!(old_request, compat.identity());
    assert_eq!(Some(key.clone()), compat.archive_key());
    for private in [
        "secret",
        "private-value",
        "cookie",
        "alice",
        "caption",
        "https",
    ] {
        assert!(!key.contains(private));
    }
    let public = serde_json::to_string(&compat).unwrap();
    assert!(!public.contains("never-public"));
    assert!(!public.contains("rotated-cookie"));
    assert!(!public.contains("private-value"));
    let metadata = compat.metadata.unwrap();
    assert_eq!(metadata.shortcode, "BA");
    assert_eq!(metadata.username.as_deref(), Some("alice"));
    assert_eq!(metadata.caption.as_deref(), Some("caption"));
    assert_eq!(metadata.timestamp, Some(123));
}

#[test]
fn social_identities_separate_carousel_items_previews_and_categories() {
    let first = compatibility(
        "instagram",
        json!({"post_id": 64, "media_id": 65, "num": 1}),
        "a",
    );
    let second = compatibility(
        "instagram",
        json!({"post_id": 64, "media_id": 66, "num": 2}),
        "a",
    );
    let preview = compatibility(
        "instagram",
        json!({"post_id": 64, "media_id": "65p", "num": 1}),
        "a",
    );
    let twitter = compatibility("twitter", json!({"tweet_id": 64, "num": 65}), "a");
    for other in [second, preview, twitter] {
        assert_ne!(first.archive_key(), other.archive_key());
        assert!(other.archive_key().is_some());
    }
}

#[test]
fn twitter_uses_upstream_tweet_and_file_number_and_public_post_fields() {
    let first = compatibility(
        "twitter",
        json!({"tweet_id": 123, "num": 1,
        "author": {"name": "alice", "token": "private"}, "content": "hello"}),
        "a",
    );
    let rotated = compatibility("twitter", json!({"tweet_id": 123, "num": 1}), "b");
    let second = compatibility("twitter", json!({"tweet_id": 123, "num": 2}), "a");
    assert_eq!(first.archive_key(), rotated.archive_key());
    assert_ne!(first.archive_key(), second.archive_key());
    let post = first.metadata.as_ref().unwrap();
    assert_eq!(post.post_id, "123");
    assert_eq!(post.media_id, "1");
    assert_eq!(post.shortcode, "");
    assert_eq!(post.username.as_deref(), Some("alice"));
    assert_eq!(post.caption.as_deref(), Some("hello"));
    assert!(!serde_json::to_string(&first).unwrap().contains("private"));
}

#[test]
fn facebook_distinguishes_audio_and_video_but_ignores_album_position() {
    let video = compatibility(
        "facebook",
        json!({"id": "123", "extension": "mp4", "num": 1}),
        "a",
    );
    let audio = compatibility("facebook", json!({"id": "123", "extension": "m4a"}), "a");
    let rotated = compatibility(
        "facebook",
        json!({"id": 123, "extension": "mp4", "num": 9}),
        "b",
    );
    assert_ne!(video.archive_key(), audio.archive_key());
    assert_eq!(video.archive_key(), rotated.archive_key());
    assert_eq!(video.metadata.unwrap().media_id, "123.mp4");
}

#[test]
fn incomplete_unknown_and_ambiguous_social_metadata_has_no_archive_identity() {
    for (category, metadata) in [
        ("instagram", json!({"post_id": 123})),
        ("instagram", json!({"media_id": 123})),
        (
            "instagram",
            json!({"post_id": "https://secret", "media_id": 123}),
        ),
        ("instagram", json!({"post_id": 0, "media_id": 123})),
        ("instagram", json!({"post_id": 123, "media_id": "123:456"})),
        ("twitter", json!({"tweet_id": 123})),
        ("twitter", json!({"tweet_id": 123, "num": 0})),
        ("twitter", json!({"tweet_id": 123, "num": "1"})),
        ("twitter", json!({"post_id": 123, "media_id": 1, "num": 1})),
        ("facebook", json!({"id": 123})),
        ("facebook", json!({"id": 123, "extension": "../mp4"})),
        ("unknown", json!({"post_id": 123, "media_id": 123})),
    ] {
        assert!(
            compatibility(category, metadata.clone(), "secret")
                .archive_key()
                .is_none(),
            "{category}: {metadata}"
        );
    }
}

#[test]
fn direct_links_have_explicit_private_full_request_identities() {
    let mut first =
        extract::direct(&extract::http_url("https://example.com/a.jpg?token=secret").unwrap())
            .unwrap();
    let key = first.archive_key().unwrap();
    assert!(key.starts_with("url:v1:"));
    assert!(!key.contains("secret"));
    assert_eq!(Some(key.clone()), first.clone().archive_key());
    first
        .headers
        .insert("Authorization".into(), "private".into());
    assert_ne!(Some(key), first.archive_key());
    let header_key = first.archive_key();
    first
        .handoff
        .insert("_fallback".into(), json!(["https://example.com/other.jpg"]));
    assert_ne!(header_key, first.archive_key());
}

#[test]
fn compatibility_dates_normalize_to_utc_without_affecting_stable_ids() {
    for category in ["instagram", "twitter", "facebook"] {
        let base = json!({"post_id":"100","media_id":"101","tweet_id":"100","id":"100","extension":"jpg","num":1});
        let original = compatibility(category, base.clone(), "original").archive_key();
        for (date, expected) in [
            (json!("1970-01-01 00:00:00"), Some(0)),
            (json!("2000-01-01T01:00:00+01:00"), Some(946684800)),
            (json!("1969-12-31T23:59:59.9Z"), Some(-1)),
            (json!(123), Some(123)),
            (json!("not-a-date"), None),
            (json!(null), None),
        ] {
            let mut data = base.clone();
            data["date"] = date;
            let item = compatibility(category, data, "rotated");
            assert_eq!(item.metadata.as_ref().unwrap().timestamp, expected);
            assert_eq!(item.archive_key(), original);
        }
    }
}

#[test]
fn twitter_expansion_context_is_allowlisted_and_does_not_change_archive_identity() {
    let plain = compatibility("twitter", json!({"tweet_id":64,"num":1}), "old");
    let expanded = compatibility(
        "twitter",
        json!({"tweet_id":64,"num":1,"retweet_id":65,"quote_id":"66","private":"secret"}),
        "new",
    );
    assert_eq!(plain.archive_key(), expanded.archive_key());
    let metadata = expanded.metadata.unwrap();
    assert_eq!(metadata.retweet_id.as_deref(), Some("65"));
    assert_eq!(metadata.quoted_by_id.as_deref(), Some("66"));
    for invalid in [
        json!(0),
        json!("001"),
        json!("private"),
        json!(-1),
        json!(true),
    ] {
        let item = compatibility(
            "twitter",
            json!({"tweet_id":64,"num":1,"retweet_id":invalid,"quote_id":invalid}),
            "a",
        );
        let metadata = item.metadata.unwrap();
        assert!(metadata.retweet_id.is_none());
        assert!(metadata.quoted_by_id.is_none());
    }
    let instagram = compatibility(
        "instagram",
        json!({"post_id":64,"media_id":65,"retweet_id":66,"quote_id":67}),
        "a",
    );
    assert!(instagram.metadata.unwrap().retweet_id.is_none());
}
