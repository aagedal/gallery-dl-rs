use gallery_dl_rs::instagram::parse_response;
use serde_json::{Value, json};

fn carousel() -> Value {
    serde_json::from_slice(include_bytes!("fixtures/instagram/carousel.json")).unwrap()
}
fn parse(data: &Value) -> Vec<gallery_dl_rs::extract::Media> {
    parse_response(&serde_json::to_vec(data).unwrap(), "BA").unwrap()
}

#[test]
fn mixed_carousel_metadata_follows_selected_variant_and_child() {
    let mut data = carousel();
    data["items"][0]["original_width"] = 99999.into();
    data["items"][0]["original_height"] = 88888.into();
    data["items"][0]["accessibility_caption"] = "parent must not be inherited".into();
    data["items"][0]["video_duration"] = 777.into();
    let children = data["items"][0]["carousel_media"].as_array_mut().unwrap();
    children[0]["original_width"] = 2048.into();
    children[0]["original_height"] = 2048.into();
    children[0]["accessibility_caption"] = "An image 🍂".into();
    children[0]["video_duration"] = 99.into();
    children[1]["original_width"] = 3840.into();
    children[1]["original_height"] = 2160.into();
    children[1]["video_duration"] = 12.75.into();
    children[1]["accessibility_caption"] = "A video".into();
    children[1]["image_versions2"]["candidates"][0]["width"] = 5000.into();
    children[1]["image_versions2"]["candidates"][0]["height"] = 5000.into();
    let media = parse(&data);
    let image = media[0].metadata.as_ref().unwrap().file.as_ref().unwrap();
    assert_eq!((image.width, image.height), (Some(1080), Some(1080)));
    assert_eq!(
        (image.width_original, image.height_original),
        (Some(2048), Some(2048))
    );
    assert_eq!(image.duration, None);
    assert_eq!(image.description.as_deref(), Some("An image 🍂"));
    assert_eq!(image.media_type.as_deref(), Some("image"));
    let video = media[1].metadata.as_ref().unwrap().file.as_ref().unwrap();
    assert_eq!((video.width, video.height), (Some(1920), Some(1080)));
    assert_eq!(
        (video.width_original, video.height_original),
        (Some(3840), Some(2160))
    );
    assert_eq!(video.duration, Some(12.75));
    assert_eq!(video.description.as_deref(), Some("A video"));
    assert_eq!(video.media_type.as_deref(), Some("video"));
    let sibling = media[2].metadata.as_ref().unwrap().file.as_ref().unwrap();
    assert_eq!(
        (sibling.width_original, sibling.height_original),
        (None, None)
    );
    assert_eq!(sibling.description, None);
    assert_eq!(sibling.duration, None);
    assert_eq!(media[1].url.as_str(), "https://cdn.example/66.mp4");
}

#[test]
fn malformed_optional_metadata_is_omitted_without_changing_selection_or_identity() {
    let original = carousel();
    let expected = parse(&original);
    let mut data = original;
    for item in data["items"][0]["carousel_media"].as_array_mut().unwrap() {
        item["original_width"] = json!("private malformed dimension");
        item["original_height"] = json!(-1);
        item["video_duration"] = json!(-5.0);
        item["accessibility_caption"] = json!({"private":"malformed"});
    }
    let actual = parse(&data);
    for (before, after) in expected.iter().zip(&actual) {
        assert_eq!(before.url, after.url);
        assert_eq!(before.filename, after.filename);
        assert_eq!(before.identity(), after.identity());
        assert_eq!(before.archive_key(), after.archive_key());
        let metadata = after.metadata.as_ref().unwrap();
        let file = metadata.file.as_ref().unwrap();
        assert_eq!((file.width_original, file.height_original), (None, None));
        assert_eq!(file.duration, None);
        assert_eq!(file.description, None);
    }
    for value in [json!("12.0"), json!({}), json!(-0.5), Value::Null] {
        data["items"][0]["carousel_media"][1]["video_duration"] = value;
        assert_eq!(
            parse(&data)[1]
                .metadata
                .as_ref()
                .unwrap()
                .file
                .as_ref()
                .unwrap()
                .duration,
            None
        );
    }
    data["items"][0]["carousel_media"][1]["video_duration"] = 0.into();
    assert_eq!(
        parse(&data)[1]
            .metadata
            .as_ref()
            .unwrap()
            .file
            .as_ref()
            .unwrap()
            .duration,
        Some(0.0)
    );
}

#[test]
fn invalid_selected_dimensions_are_not_replaced_with_post_dimensions() {
    let mut data: Value =
        serde_json::from_slice(include_bytes!("fixtures/instagram/photo.json")).unwrap();
    let post = &mut data["items"][0];
    post["original_width"] = 4096.into();
    post["original_height"] = 2048.into();
    post["image_versions2"]["candidates"] =
        json!([{"url":"https://cdn.example/photo.jpg","width":0,"height":"invalid"}]);
    let media = parse(&data);
    let file = media[0].metadata.as_ref().unwrap().file.as_ref().unwrap();
    assert_eq!((file.width, file.height), (None, None));
    assert_eq!(
        (file.width_original, file.height_original),
        (Some(4096), Some(2048))
    );
}
