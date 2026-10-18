use gallery_dl_rs::{
    extract::MediaJob,
    facebook::{parse_video, video_id},
};
use serde_json::{Value, json};
use url::Url;

const ID: &str = "123456789";
const FIXTURE: &[u8] = include_bytes!("fixtures/facebook/video.json");

fn video(manifest: &str) -> Value {
    json!({"__typename":"Video", "id":ID, "dash_manifest":manifest})
}

fn parse(value: Value) -> gallery_dl_rs::Result<Vec<gallery_dl_rs::extract::Media>> {
    parse_video(&serde_json::to_vec(&value).unwrap(), ID)
}

fn manifest(video: &str, audio: &str) -> String {
    format!(
        "<MPD><Period><AdaptationSet mimeType=\"video/mp4\">{video}</AdaptationSet>{audio}</Period></MPD>"
    )
}

fn representation(uri: &str) -> String {
    format!(
        "<Representation width=\"1280\" height=\"720\" bandwidth=\"1000000\"><BaseURL>{uri}</BaseURL></Representation>"
    )
}

#[test]
fn accepts_watch_and_user_video_urls() {
    for raw in [
        format!("https://www.facebook.com/watch/?v={ID}"),
        format!("https://facebook.com/watch?v={ID}&ref=sharing"),
        format!("http://m.facebook.com/watch/?v={ID}"),
        format!("https://facebook.com/creator/videos/{ID}"),
        format!("https://www.facebook.com/12345/videos/{ID}/"),
    ] {
        assert_eq!(video_id(&Url::parse(&raw).unwrap()).unwrap(), ID);
    }
}

#[test]
fn rejects_ambiguous_invalid_or_foreign_video_urls() {
    for raw in [
        "https://facebook.com/watch/",
        "https://facebook.com/watch/?v=0",
        "https://facebook.com/watch/?v=-1",
        "https://facebook.com/watch/?v=abc",
        "https://facebook.com/watch/?v=1&v=2",
        "https://facebook.com/watch/?v=1&v=1",
        "https://facebook.com/watch/?v=1%2F2",
        "https://facebook.com/creator/videos/0/",
        "https://facebook.com/creator/videos/1/extra",
        "https://facebook.com/videos/1",
        "https://facebook.com.evil.test/watch/?v=1",
        "https://user:secret@facebook.com/watch/?v=1",
        "https://facebook.com:444/watch/?v=1",
        "ftp://facebook.com/watch/?v=1",
    ] {
        assert!(video_id(&Url::parse(raw).unwrap()).is_err(), "{raw}");
    }
}

#[test]
fn selects_highest_quality_video_and_audio_with_distinct_archive_ids() {
    let media = parse_video(FIXTURE, ID).unwrap();
    assert_eq!(media.len(), 2);
    assert_eq!(
        media[0].url.as_str(),
        "https://cdn.example.test/high.mp4?sig=synthetic&kind=video"
    );
    assert_eq!(
        media[1].url.as_str(),
        "https://cdn.example.test/high.m4a?sig=synthetic&kind=audio"
    );
    for (index, item) in media.iter().enumerate() {
        let ext = if index == 0 { "mp4" } else { "m4a" };
        assert_eq!(item.category, "facebook");
        assert_eq!(item.filename, format!("{ID}.{ext}"));
        assert!(matches!(item.job, MediaJob::DirectHttp));
        let metadata = item.metadata.as_ref().unwrap();
        assert_eq!(metadata.post_id, ID);
        assert_eq!(metadata.media_id, format!("{ID}.{ext}"));
        assert_eq!(metadata.index, index + 1);
        assert_eq!(metadata.username.as_deref(), Some("synthetic_creator"));
        assert_eq!(
            metadata.caption.as_deref(),
            Some("Synthetic video caption 🎥")
        );
        assert_eq!(metadata.timestamp, Some(1700000000));
        assert!(!item.headers.contains_key("Cookie"));
    }
    assert_ne!(media[0].archive_key(), media[1].archive_key());
    assert!(media[0].archive_key().unwrap().starts_with("facebook:v1:"));
}

#[test]
fn parses_json_scripts_and_nested_manifest_envelopes() {
    let source: Value = serde_json::from_slice(FIXTURE).unwrap();
    let mpd = source["data"]["video"]["dash_manifest"].as_str().unwrap();
    assert_eq!(
        parse(
            json!({"__typename":"Video","id":ID,"dash_manifest":mpd,"videoDeliveryLegacyFields":{}})
        )
        .unwrap()
        .len(),
        2
    );
    for envelope in [
        json!({"__typename":"Video","id":ID,"videoDeliveryLegacyFields":{"dash_manifest":mpd}}),
        json!({"__typename":"Video","id":ID,"videoDeliveryResponseFragment":{"videoDeliveryResponseResult":{"dash_manifests":[{"manifest_xml":mpd}]}}}),
    ] {
        let html = format!(
            "<html><meta property='og:video' content='https://wrong.example.test/preview.mp4'><script type=\"application/json\">{envelope}</script></html>"
        );
        assert_eq!(parse_video(html.as_bytes(), ID).unwrap().len(), 2);
    }
}

#[test]
fn accepts_owned_fallback_metadata_and_omits_display_name_as_username() {
    let mpd = manifest(&representation("https://cdn.example.test/video.mp4"), "");
    let media = parse(json!({"__typename":"Video","id":ID,"dash_manifest":mpd,
        "owner":{"name":"Display Name"},"message":{"text":"Fallback caption"},
        "created_time":1700000001}))
    .unwrap();
    let metadata = media[0].metadata.as_ref().unwrap();
    assert!(metadata.username.is_none());
    assert_eq!(metadata.caption.as_deref(), Some("Fallback caption"));
    assert_eq!(metadata.timestamp, Some(1700000001));
}

#[test]
fn binds_manifest_and_metadata_to_exact_typed_video() {
    let mpd = manifest(&representation("https://cdn.example.test/video.mp4"), "");
    for value in [
        json!({"dash_manifest":mpd}),
        json!({"id":ID,"dash_manifest":mpd}),
        json!({"__typename":"Video","id":"999","dash_manifest":mpd}),
        json!({"__typename":"Photo","id":ID,"dash_manifest":mpd}),
        json!({"__typename":"Video","id":ID,"comments":{"dash_manifest":mpd}}),
        json!({"node":{"__typename":"Video","id":ID},"dash_manifest":mpd}),
    ] {
        assert!(
            parse(value.clone()).is_err(),
            "unexpected node accepted: {value}"
        );
    }
    let media = parse(json!({"owner":{"username":"wrong"}, "publish_time":123,
        "node":video(&mpd)}))
    .unwrap();
    assert_eq!(media.len(), 1);
    let metadata = media[0].metadata.as_ref().unwrap();
    assert!(metadata.username.is_none());
    assert!(metadata.caption.is_none());
    assert!(metadata.timestamp.is_none());
    assert!(parse_video(FIXTURE, "999").is_err());
}

#[test]
fn rejects_segmented_drm_missing_video_and_malformed_audio_manifests() {
    let full = representation("https://cdn.example.test/video.mp4");
    for mpd in [
        manifest(&format!("<SegmentTemplate media=\"part-$Number$.m4s\"/>{full}"), ""),
        manifest(&format!("<SegmentList><SegmentURL media=\"part.m4s\"/></SegmentList>{full}"), ""),
        manifest(&format!("<ContentProtection schemeIdUri=\"urn:uuid:synthetic\"/>{full}"), ""),
        "<MPD><Period><AdaptationSet mimeType=\"audio/mp4\"><Representation><BaseURL>https://cdn.example.test/a.m4a</BaseURL></Representation></AdaptationSet></Period></MPD>".to_owned(),
        manifest(&full, "<AdaptationSet mimeType=\"audio/mp4\"><Representation bandwidth=\"1000\"/></AdaptationSet>"),
        manifest(&full, "<AdaptationSet mimeType=\"audio/mp4\"><Representation><BaseURL>file:///private-audio</BaseURL></Representation></AdaptationSet>"),
        "<MPD><Period>malformed".to_owned(),
    ] {
        assert!(parse(video(&mpd)).is_err(), "accepted unsupported manifest");
    }
}

#[test]
fn rejects_equal_quality_distinct_urls_and_keeps_diagnostics_private() {
    let mpd = manifest(
        &format!(
            "{}{}",
            representation("https://cdn.example.test/a.mp4?token=private-token"),
            representation("https://cdn.example.test/b.mp4?token=private-token")
        ),
        "",
    );
    let error = parse(video(&mpd)).unwrap_err().to_string();
    assert!(error.contains("ambiguous"), "{error}");
    assert!(!error.contains("private-token"));
    for uri in [
        "file:///private-token",
        "https://user:private-token@cdn.example.test/a.mp4",
    ] {
        let mpd = manifest(&representation(uri), "");
        let error = parse(video(&mpd)).unwrap_err().to_string();
        assert!(!error.contains("private-token"));
    }
}

#[test]
fn selects_owned_progressive_hd_from_current_and_legacy_fields() {
    for fields in [
        json!({"playable_url":"https://cdn.example.test/sd.mp4", "playable_url_quality_hd":"https://cdn.example.test/hd.mp4"}),
        json!({"videoDeliveryLegacyFields":{"browser_native_sd_url":"https://cdn.example.test/sd.mp4", "browser_native_hd_url":"https://cdn.example.test/hd.mp4"}}),
        json!({"videoDeliveryResponseFragment":{"videoDeliveryResponseResult":{"progressive_urls":[
            {"progressive_url":"https://cdn.example.test/sd.mp4","metadata":{"quality":"SD"}},
            {"progressive_url":"https://cdn.example.test/hd.mp4","metadata":{"quality":"HD"}}
        ]}}}),
    ] {
        let mut node = json!({"__typename":"Video","id":ID});
        node.as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        let media = parse(node).unwrap();
        assert_eq!(media.len(), 1);
        assert_eq!(media[0].url.as_str(), "https://cdn.example.test/hd.mp4");
        assert_eq!(media[0].filename, format!("{ID}.mp4"));
    }
}

#[test]
fn progressive_ambiguity_and_malformed_offers_fail_privately() {
    for fields in [
        json!({"playable_url_quality_hd":"https://cdn.example.test/one.mp4?token=private-token", "browser_native_hd_url":"https://cdn.example.test/two.mp4?token=private-token"}),
        json!({"playable_url_quality_hd":"https://cdn.example.test/good.mp4", "playable_url":"file:///private-token.mp4"}),
        json!({"playable_url":"https://user:private-token@cdn.example.test/a.mp4"}),
        json!({"playable_url":"https://cdn.example.test/private-token.m3u8"}),
        json!({"playable_url":123}),
        json!({"videoDeliveryResponseFragment":{"videoDeliveryResponseResult":{"progressive_urls":[{"metadata":{"quality":"HD"}}]}}}),
    ] {
        let mut node = json!({"__typename":"Video","id":ID});
        node.as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        let error = parse(node).unwrap_err().to_string();
        assert!(!error.contains("private-token"), "{error}");
    }
}

#[test]
fn accepts_legacy_owned_video_id_and_rejects_conflicting_identity_aliases() {
    let mpd = manifest(&representation("https://cdn.example.test/video.mp4"), "");
    assert_eq!(
        parse(json!({"video_id":ID,"dash_manifest":mpd}))
            .unwrap()
            .len(),
        1
    );
    for value in [
        json!({"__typename":"Video","id":ID,"video_id":"999","dash_manifest":mpd}),
        json!({"__typename":"Video","id":ID,"videoId":"999","dash_manifest":mpd}),
        json!({"__typename":"Video","id":ID,"video_id":null,"dash_manifest":mpd}),
        json!({"__typename":"Photo","id":ID,"video_id":ID,"dash_manifest":mpd}),
        json!({"__typename":"Video","__isNode":"Photo","id":ID,"dash_manifest":mpd}),
        json!({"video_id":ID,"id":"999","dash_manifest":mpd}),
    ] {
        assert!(parse(value).is_err());
    }
}

#[test]
fn reports_block_and_auth_failures_without_matching_caption_or_modules() {
    for (body, diagnostic) in [
        ("{\"__dr\":\"CometErrorRoot.react\"}", "blocked"),
        ("<form id=\"login_form\">", "authentication"),
        ("{\"checkpoint_url\":\"private-token\"}", "authentication"),
    ] {
        let error = parse_video(body.as_bytes(), ID).unwrap_err().to_string();
        assert!(error.contains(diagnostic), "{error}");
        assert!(!error.contains("private-token"));
    }
    let value = json!({"modules":["CometErrorRoot.react"],"node":{
        "__typename":"Video","id":ID,"playable_url":"https://cdn.example.test/video.mp4",
        "message":{"text":"CometErrorRoot.react login_form checkpoint_url"}
    }});
    assert_eq!(parse(value).unwrap().len(), 1);
    let error = parse(json!({"modules":["CometErrorRoot.react"]}))
        .unwrap_err()
        .to_string();
    assert!(!error.contains("blocked"));
}
