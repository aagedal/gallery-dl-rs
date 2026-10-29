use gallery_dl_rs::{
    extract::Media,
    facebook::{parse_photo, parse_video},
};
use serde_json::{Value, json};

const ID: &str = "123456789";

fn photo(value: Value) -> Vec<Media> {
    parse_photo(&serde_json::to_vec(&value).unwrap(), ID).unwrap()
}
fn video(value: Value) -> Vec<Media> {
    parse_video(&serde_json::to_vec(&value).unwrap(), ID).unwrap()
}

#[test]
fn selected_photo_metadata_belongs_to_selected_identity_bound_image() {
    let selected = json!({"__typename":"Photo", "id":ID,
        "image":{"uri":"https://cdn.example/large.jpg","width":2048,"height":1365},
        "accessibility_caption":"An accessible description",
        "private_delivery":{"token":"PRIVATE-DELIVERY-TOKEN"}});
    let lower = json!({"__typename":"Photo","id":ID,
        "image":{"uri":"https://cdn.example/small.jpg","width":300,"height":200},
        "accessibility_caption":"Wrong smaller description"});
    let unrelated = json!({"__typename":"Photo","id":"999",
        "image":{"uri":"https://cdn.example/preview.jpg","width":99999,"height":99999},
        "accessibility_caption":"UNRELATED-PREVIEW-SECRET"});
    let items = photo(json!({"records":[lower, selected.clone(), unrelated]}));
    assert_eq!(items[0].url.path(), "/large.jpg");
    let file = items[0].metadata.as_ref().unwrap().file.as_ref().unwrap();
    assert_eq!((file.width, file.height), (Some(2048), Some(1365)));
    assert_eq!(
        file.description.as_deref(),
        Some("An accessible description")
    );
    assert_eq!(file.media_type.as_deref(), Some("photo"));
    assert_eq!((file.width_original, file.height_original), (None, None));
    let serialized = serde_json::to_string(file).unwrap();
    for private in [
        "PRIVATE-DELIVERY-TOKEN",
        "UNRELATED-PREVIEW-SECRET",
        "https://",
        "private_delivery",
    ] {
        assert!(!serialized.contains(private));
    }
    let mut bare = selected;
    bare.as_object_mut()
        .unwrap()
        .remove("accessibility_caption");
    let without_description = photo(bare);
    assert_eq!(items[0].archive_key(), without_description[0].archive_key());
    assert_eq!(items[0].identity(), without_description[0].identity());
}

#[test]
fn dash_file_metadata_retains_selected_video_and_audio_quality_without_dimension_leakage() {
    let mut value: Value =
        serde_json::from_slice(include_bytes!("../fixtures/facebook/video.json")).unwrap();
    let node = &mut value["data"]["video"];
    node["playable_duration_in_ms"] = json!(12345);
    node["savable_description"] = json!({"text":"Owned video description"});
    // These belong to another delivery quality and must not replace DASH dimensions.
    node["width"] = json!(8000);
    node["height"] = json!(6000);
    let xml = node["dash_manifest"].as_str().unwrap().replace(
        "mimeType=\"audio/mp4\"",
        "mimeType=\"audio/mp4\" width=\"999\" height=\"888\"",
    );
    node["dash_manifest"] = json!(xml);
    let items = video(value.clone());
    assert_eq!(items.len(), 2);
    let video_file = items[0].metadata.as_ref().unwrap().file.as_ref().unwrap();
    assert_eq!(
        (video_file.width, video_file.height),
        (Some(1920), Some(1080))
    );
    assert_eq!(video_file.bitrate, Some(2_000_000));
    assert_eq!(video_file.duration, Some(12.345));
    assert_eq!(video_file.media_type.as_deref(), Some("video"));
    let audio_file = items[1].metadata.as_ref().unwrap().file.as_ref().unwrap();
    assert_eq!((audio_file.width, audio_file.height), (None, None));
    assert_eq!(
        (audio_file.width_original, audio_file.height_original),
        (None, None)
    );
    assert_eq!(audio_file.bitrate, Some(128_000));
    assert_eq!(audio_file.media_type.as_deref(), Some("audio"));
    for item in &items {
        let file = item.metadata.as_ref().unwrap().file.as_ref().unwrap();
        assert_eq!(file.duration, Some(12.345));
        assert_eq!(file.description.as_deref(), Some("Owned video description"));
        assert!(!serde_json::to_string(file).unwrap().contains("https://"));
    }
    value["data"]["video"]
        .as_object_mut()
        .unwrap()
        .remove("playable_duration_in_ms");
    let no_duration = video(value);
    assert_eq!(
        items.iter().map(Media::archive_key).collect::<Vec<_>>(),
        no_duration
            .iter()
            .map(Media::archive_key)
            .collect::<Vec<_>>()
    );
}

#[test]
fn progressive_duration_uses_owned_fields_without_inventing_selected_resolution() {
    let value = json!({"__typename":"Video","id":ID,
        "length_in_second":2.5, "playable_duration_in_ms":-1,
        "width":8000,"height":6000,
        "videoDeliveryResponseFragment":{"videoDeliveryResponseResult":{"progressive_urls":[
            {"progressive_url":"https://cdn.example/sd.mp4","metadata":{"quality":"sd","width":640,"height":360}},
            {"progressive_url":"https://cdn.example/hd.mp4","metadata":{"quality":"hd"}}
        ]}},
        "preview":{"playable_duration_in_ms":99999,"accessibility_caption":"UNRELATED-SECRET"}});
    let items = video(value);
    assert_eq!(items[0].url.path(), "/hd.mp4");
    let file = items[0].metadata.as_ref().unwrap().file.as_ref().unwrap();
    assert_eq!(file.duration, Some(2.5));
    assert_eq!((file.width, file.height, file.bitrate), (None, None, None));
    assert_eq!(file.description, None);
    assert_eq!(file.media_type.as_deref(), Some("video"));
}

#[test]
fn malformed_optional_file_fields_are_omitted_without_changing_media_selection() {
    let items = photo(json!({"__typename":"Photo","id":ID,
        "image":{"uri":"https://cdn.example/image.jpg","width":-1,"height":"PRIVATE-INVALID-FIELD"},
        "accessibility_caption":{"text":"PRIVATE-INVALID-FIELD"}}));
    let file = items[0].metadata.as_ref().unwrap().file.as_ref().unwrap();
    assert_eq!((file.width, file.height), (None, None));
    assert_eq!(file.description, None);
    let items = video(json!({"__typename":"Video","id":ID,
        "playable_url_quality_hd":"https://cdn.example/video.mp4",
        "playable_duration_in_ms":"PRIVATE-INVALID-FIELD","length_in_second":-10,
        "savable_description":{"text":{"private":"PRIVATE-INVALID-FIELD"}}}));
    let file = items[0].metadata.as_ref().unwrap().file.as_ref().unwrap();
    assert_eq!(file.duration, None);
    assert_eq!(file.description, None);
    assert!(
        !serde_json::to_string(file)
            .unwrap()
            .contains("PRIVATE-INVALID-FIELD")
    );
}

#[test]
fn equivalent_manifest_urls_do_not_become_ambiguous_due_to_descriptive_quality_fields() {
    let mut value: Value =
        serde_json::from_slice(include_bytes!("../fixtures/facebook/video.json")).unwrap();
    let node = &mut value["data"]["video"];
    let alternate = node["dash_manifest"]
        .as_str()
        .unwrap()
        .replace("width=\"1920\"", "width=\"2560\"")
        .replace("height=\"1080\"", "height=\"1440\"")
        .replace("bandwidth=\"2000000\"", "bandwidth=\"3000000\"")
        .replace("bandwidth=\"128000\"", "bandwidth=\"192000\"");
    node["dash_manifest_xml_string"] = json!(alternate);
    let items = video(value);
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].url.path(), "/high.mp4");
    assert_eq!(items[1].url.path(), "/high.m4a");
    let file = items[0].metadata.as_ref().unwrap().file.as_ref().unwrap();
    assert_eq!(
        (file.width, file.height, file.bitrate),
        (Some(1920), Some(1080), Some(2_000_000))
    );
    assert_eq!(
        items[1]
            .metadata
            .as_ref()
            .unwrap()
            .file
            .as_ref()
            .unwrap()
            .bitrate,
        Some(128_000)
    );
}
