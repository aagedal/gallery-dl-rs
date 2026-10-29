use gallery_dl_rs::{
    event::Post,
    extract::{self, Media},
    filter::FilterExpression,
    naming::FilenameTemplate,
};
use serde_json::{Value, json};

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/pixiv/ugoira_original.json")).unwrap()
}
fn media(data: Value, url: &str) -> Media {
    extract::parse_messages(
        &serde_json::to_vec(&json!([[3, url, data]])).unwrap(),
        "pixiv",
    )
    .unwrap()
    .remove(0)
}
fn first(original: bool) -> (Value, String) {
    let fixture = fixture();
    for case in fixture["cases"].as_array().unwrap() {
        for event in case["events"].as_array().unwrap() {
            if event["kind"] == "media" && event["metadata"]["_ugoira_original"] == original {
                return (
                    event["metadata"].clone(),
                    event["url"].as_str().unwrap().to_owned(),
                );
            }
        }
    }
    panic!("missing fixture media")
}

#[test]
fn compatibility_original_and_zip_metadata_match_actual_source_vectors() {
    let fixture = fixture();
    for case in fixture["cases"].as_array().unwrap() {
        for event in case["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["kind"] == "media")
        {
            let item = media(event["metadata"].clone(), event["url"].as_str().unwrap());
            let metadata = item.metadata.as_ref().unwrap();
            assert_eq!(
                metadata.ugoira_original,
                event["metadata"]["_ugoira_original"].as_bool(),
                "{}",
                case["name"]
            );
            assert_eq!(
                metadata.ugoira_frame_index,
                event["metadata"]["_ugoira_frame_index"]
                    .as_u64()
                    .map(|n| n as usize)
            );
            assert_eq!(
                serde_json::to_value(metadata.frames.as_ref().unwrap()).unwrap(),
                case["frames"]
            );
            assert_eq!(item.filename, event["default_filename"].as_str().unwrap());
            assert!(item.archive_key().is_some(), "{}", case["name"]);
        }
    }
}

#[test]
fn frame_flags_and_zero_based_indices_are_available_to_filters_and_templates() {
    let (data, url) = first(true);
    let item = media(data, &url);
    let filter=FilterExpression::parse("ugoira_original is True and _ugoira_original is True and ugoira_frame_index == 0 and _ugoira_frame_index == 0 and metadata['_ugoira_frame_index'] == 0 and len(frames) == 2 and frames[1]['delay'] == 125 and _ugoira_frame_data[1]['delay'] == 125").unwrap();
    assert!(filter.matches(&item).unwrap());
    assert_eq!(
        FilenameTemplate::parse("{id}_{_ugoira_frame_index:03}_{frames[1][delay]}.{extension}")
            .unwrap()
            .render(&item)
            .unwrap(),
        "42_000_125.jpg"
    );
    let post = Post::from_media(std::slice::from_ref(&item), "pixiv");
    assert_eq!(post.metadata.as_ref().unwrap().ugoira_original, Some(true));
    assert!(post.metadata.as_ref().unwrap().ugoira_frame_index.is_none());
    assert!(
        FilterExpression::parse("ugoira_original is True and len(_ugoira_frame_data) == 2")
            .unwrap()
            .matches_post(&post)
            .unwrap()
    );
    let public = serde_json::to_value(&item).unwrap();
    assert_eq!(public["metadata"]["ugoira_frame_index"], 0);
    assert!(public["metadata"].get("_ugoira_frame_index").is_none());
    assert_eq!(
        serde_json::to_value(post.metadata.as_ref().unwrap()).unwrap()["frames"],
        public["metadata"]["frames"]
    );
}

#[test]
fn zip_and_first_original_frame_never_share_archive_keys_in_any_artwork_scope() {
    let (original, original_url) = first(true);
    let (zip, zip_url) = first(false);
    for context in [
        json!({}),
        json!({"ranking":{"mode":"day","date":"2024-01-02","rank":1,"type":"all"}}),
        json!({"search":{"word":"cat","sort":"date_desc","target":"exact_match_for_tags","date_start":null,"date_end":null}}),
        json!({"user_follow":{"id":17,"name":"Owner","account":"owner"}}),
        json!({"user_bookmark":{"id":17,"name":"Owner","account":"owner"}}),
        json!({"pixivision_id":77,"pixivision_title":"Article"}),
        json!({"num_series":3,"series":{"id":8,"title":"Series","caption":"","total":4}}),
    ] {
        let mut original = original.clone();
        let mut zip = zip.clone();
        for (key, value) in context.as_object().unwrap() {
            original[key] = value.clone();
            zip[key] = value.clone();
        }
        let original = media(original, &original_url);
        let zip = media(zip, &zip_url);
        assert!(original.archive_key().is_some(), "{context}");
        assert!(zip.archive_key().is_some(), "{context}");
        assert_ne!(original.archive_key(), zip.archive_key(), "{context}");
        let mut changed = original.clone();
        changed.url = extract::http_url("https://i.pximg.net/frame.jpg?token=changed").unwrap();
        changed.metadata.as_mut().unwrap().frames = None;
        changed.metadata.as_mut().unwrap().ugoira_frame_index = None;
        changed.metadata.as_mut().unwrap().ugoira_original = None;
        assert_eq!(original.archive_key(), changed.archive_key(), "{context}");
    }
}

#[test]
fn compatibility_supports_five_digit_original_frame_numbers_and_keeps_series_names() {
    let (mut data, url) = first(true);
    data["num"] = 99999.into();
    data["_ugoira_frame_index"] = 99999.into();
    let item = media(data.clone(), &url);
    assert_eq!(item.filename, "42_p99999.jpg");
    assert_eq!(item.metadata.as_ref().unwrap().media_id, "42_p99999");
    assert!(item.archive_key().is_some());
    data["series"] = json!({"id":8,"title":"Series","caption":"","total":4});
    data["num_series"] = 3.into();
    assert_eq!(media(data.clone(), &url).filename, "003_42_p99999.jpg");
    data["num"] = 100000.into();
    data["_ugoira_frame_index"] = 100000.into();
    let invalid = media(data, &url);
    assert!(invalid.archive_key().is_none());
    assert!(
        invalid
            .metadata
            .as_ref()
            .unwrap()
            .ugoira_frame_index
            .is_none()
    );
}
