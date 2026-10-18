use gallery_dl_rs::{
    extract::{Media, MediaJob},
    twitter::{
        CardMode, CardOptions, ExpansionOptions, MediaOptions, VideoMode,
        parse_response_with_card_options,
    },
};
use serde_json::{Value, json};

const ID: &str = "1234567890";
fn source(card: Value, ordinary: bool) -> Value {
    let mut data: Value =
        serde_json::from_slice(include_bytes!("fixtures/twitter/photo.json")).unwrap();
    data["data"]["tweetResult"]["result"]["card"] = card;
    if !ordinary {
        data["data"]["tweetResult"]["result"]["legacy"]
            .as_object_mut()
            .unwrap()
            .remove("extended_entities");
    }
    data
}
fn parse(
    data: &Value,
    mode: CardMode,
    blacklist: &[&str],
    media: MediaOptions,
) -> gallery_dl_rs::Result<Vec<Media>> {
    parse_response_with_card_options(
        &serde_json::to_vec(data).unwrap(),
        ID,
        ExpansionOptions::default(),
        media,
        CardOptions {
            mode,
            blacklist: blacklist.iter().map(|s| (*s).into()).collect(),
        },
    )
}
fn card(name: &str, bindings: Value) -> Value {
    json!({"name":name,"binding_values":bindings})
}
fn image(url: &str) -> Value {
    json!({"image_value":{"url":url}})
}
fn photo(url: &str) -> Value {
    json!({"type":"photo","media_url_https":url})
}
fn unified(encoded: &str) -> Value {
    card(
        "unified_card",
        json!({"unified_card":{"string_value":encoded}}),
    )
}

#[test]
fn summary_cards_follow_prefix_then_size_priority_in_both_binding_formats() {
    let bindings = json!({
        "photo_image_full_size_original":{"image_value":{}},
        "photo_image_full_size_small":image("https://cdn.example/chosen.gif?token=signature&name=small&after=kept"),
        "summary_photo_image_original":image("https://cdn.example/other.jpg"),
        "thumbnail_image_original":image("https://cdn.example/thumb.jpg"),
    });
    let array: Vec<_> = bindings
        .as_object()
        .unwrap()
        .iter()
        .map(|(key, value)| json!({"key":key,"value":value}))
        .collect();
    for bindings in [bindings, json!(array)] {
        for legacy in [false, true] {
            let raw = card("123:summary_large_image", bindings.clone());
            let data = source(if legacy { json!({"legacy":raw}) } else { raw }, true);
            let items = parse(&data, CardMode::Files, &[], MediaOptions::default()).unwrap();
            assert_eq!(items.len(), 2);
            let selected = &items[1];
            assert_eq!(selected.filename, format!("{ID}_2.gif"));
            assert_eq!(
                selected.url.as_str(),
                "https://cdn.example/chosen.gif?token=signature&after=kept&name=orig"
            );
            assert_eq!(selected.metadata.as_ref().unwrap().media_id, "2");
            assert!(selected.archive_key().is_some());
            assert_eq!(
                selected.metadata.as_ref().unwrap().caption.as_deref(),
                Some("Synthetic post")
            );
        }
    }
}

#[test]
fn blacklist_matches_exact_names_domains_and_name_domain_pairs_before_media_parse() {
    let data = source(
        card(
            "123:summary",
            json!({
                "vanity_url":{"string_value":"example.com"},
                "photo_image_full_size_original":image("file:///private/image.jpg"),
            }),
        ),
        false,
    );
    for blacklist in [["summary"], ["example.com"], ["summary:example.com"]] {
        assert!(
            parse(&data, CardMode::Ytdl, &blacklist, MediaOptions::default())
                .unwrap()
                .is_empty()
        );
    }
    for blacklist in [["SUMMARY"], ["other.example.com"], ["player:example.com"]] {
        let error = parse(&data, CardMode::Files, &blacklist, MediaOptions::default())
            .unwrap_err()
            .to_string();
        assert!(error.contains("card image"));
        assert!(!error.contains("private"));
    }
    let data = source(card("summary", json!(null)), false);
    assert!(
        parse(
            &data,
            CardMode::Files,
            &["summary"],
            MediaOptions::default()
        )
        .unwrap()
        .is_empty()
    );
}

#[test]
fn unified_cards_preserve_media_wire_order_then_component_and_url_order() {
    let z = photo("https://cdn.example/z.jpg");
    let a = photo("https://cdn.example/a.jpg");
    let encoded = format!(
        r#"{{"media_entities":{{"z":{z},"a":{a}}},"components":["z","a"],"component_objects":{{"a":{{"data":{{"conversation_preview":[{{"mediaUrls":["https://cdn.example/last.webp"]}}]}}}},"z":{{"data":{{"conversation_preview":[{{"mediaUrls":["https://cdn.example/first.png","https://cdn.example/second.mp4"]}},{{"mediaUrls":["https://cdn.example/third.jpg?name=small"]}}]}}}}}}}}"#
    );
    let data = source(unified(&encoded), true);
    for mode in [CardMode::Files, CardMode::Ytdl] {
        let items = parse(&data, mode, &[], MediaOptions::default()).unwrap();
        assert_eq!(items.len(), 7);
        assert_eq!(
            items.iter().map(|m| m.url.path()).collect::<Vec<_>>(),
            [
                "/media/fixture.jpg",
                "/z.jpg",
                "/a.jpg",
                "/first.png",
                "/second.mp4",
                "/third.jpg",
                "/last.webp"
            ]
        );
        assert_eq!(items[5].url.query(), Some("name=small"));
        for (i, item) in items.iter().enumerate() {
            assert_eq!(item.metadata.as_ref().unwrap().index, i + 1);
            assert!(matches!(item.job, MediaJob::DirectHttp));
            assert!(item.archive_key().is_some());
        }
    }
}

#[test]
fn unified_videos_use_shared_video_and_preview_selection_without_parsing_disabled_variants() {
    let video = json!({"type":"video","media_url_https":"https://cdn.example/poster.jpg",
        "video_info":{"variants":[{"content_type":"video/mp4","url":"https://cdn.example/v.mp4","bitrate":100}]}});
    let encoded = format!(r#"{{"media_entities":{{"v":{video}}}}}"#);
    let data = source(unified(&encoded), false);
    for (videos, previews, expected) in [
        (VideoMode::Files, false, 1),
        (VideoMode::Files, true, 2),
        (VideoMode::Skip, false, 0),
        (VideoMode::Skip, true, 1),
        (VideoMode::Ytdl, true, 2),
    ] {
        let items = parse(
            &data,
            CardMode::Files,
            &[],
            MediaOptions {
                videos,
                previews,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(items.len(), expected);
        if videos == VideoMode::Ytdl {
            assert!(matches!(items[0].job, MediaJob::VideoPage { .. }));
            assert_eq!(
                items[0].url.as_str(),
                format!("ytdl:https://x.com/i/web/status/{ID}")
            );
        }
        if previews {
            assert_eq!(items.last().unwrap().url.path(), "/poster.jpg");
        }
    }
    let mut malformed = video;
    malformed["video_info"] = json!("private malformed variants");
    let data = source(
        unified(&format!(r#"{{"media_entities":{{"v":{malformed}}}}}"#)),
        false,
    );
    assert!(
        parse(
            &data,
            CardMode::Files,
            &[],
            MediaOptions {
                videos: VideoMode::Skip,
                previews: false,
                ..Default::default()
            }
        )
        .unwrap()
        .is_empty()
    );
}

#[test]
fn only_unhandled_cards_delegate_and_disabled_cards_ignore_malformed_data() {
    for name in ["player", "summary"] {
        let data = source(card(name, json!({})), false);
        assert!(
            parse(&data, CardMode::Files, &[], MediaOptions::default())
                .unwrap()
                .is_empty()
        );
        let items = parse(&data, CardMode::Ytdl, &[], MediaOptions::default()).unwrap();
        assert_eq!(items.len(), 1);
        assert!(
            matches!(&items[0].job,MediaJob::VideoPage {page_url} if page_url.as_str()==format!("https://x.com/i/web/status/{ID}"))
        );
    }
    // A recognized empty unified card returns directly, without a delegated fallback.
    let data = source(unified("{}"), false);
    assert!(
        parse(&data, CardMode::Ytdl, &[], MediaOptions::default())
            .unwrap()
            .is_empty()
    );
    let data = source(json!({"private":"malformed card"}), true);
    assert_eq!(
        parse(&data, CardMode::Skip, &[], MediaOptions::default())
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn enabled_malformed_card_fields_fail_atomically_with_private_errors() {
    let duplicate_media = r#"{"media_entities":{"same":{"type":"photo","media_url_https":"https://cdn.example/a.jpg"},"same":{"type":"photo","media_url_https":"https://cdn.example/b.jpg"}}}"#;
    let cases = [
        unified("private invalid JSON"),
        unified(duplicate_media),
        unified(r#"{"media_entities":[]}"#),
        unified(r#"{"components":["missing"],"component_objects":{}}"#),
        unified(
            r#"{"components":["a","a"],"component_objects":{"a":{"data":{"conversation_preview":[{"mediaUrls":["https://cdn.example/image.jpg"]}]}}}}"#,
        ),
        unified(
            r#"{"components":["a"],"component_objects":{"a":{"data":{"conversation_preview":[null]}}}}"#,
        ),
        unified(
            r#"{"components":["a"],"component_objects":{"a":{"data":{"conversation_preview":[{"mediaUrls":["file:///private/image.jpg"]}]}}}}"#,
        ),
        card(
            "summary",
            json!([{"key":"same","value":{}},{"key":"same","value":{}}]),
        ),
        card(
            "summary",
            json!({"photo_image_full_size_original":image("https://user:private@cdn.example/image.jpg")}),
        ),
    ];
    for raw in cases {
        let data = source(raw, true);
        assert_eq!(
            parse(&data, CardMode::Skip, &[], MediaOptions::default())
                .unwrap()
                .len(),
            1
        );
        let error = parse(&data, CardMode::Files, &[], MediaOptions::default())
            .unwrap_err()
            .to_string();
        assert!(error.contains("card"));
        assert!(!error.contains("private"));
    }
}
