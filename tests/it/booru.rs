use gallery_dl_rs::{
    booru::{self, Site},
    event::Post,
    extract,
    filter::FilterExpression,
    naming::FilenameTemplate,
};
use serde_json::json;

#[test]
fn post_rating_score_and_file_size_reach_filters_templates_and_compatibility() {
    let md5 = "0123456789abcdef0123456789abcdef";
    let url = format!("https://cdn.donmai.us/original/01/23/{md5}.jpg");
    let post = json!({"id":42,"md5":md5,"file_url":url,"file_ext":"jpg",
        "file_size":1234,"rating":"s","score":-7,"tag_string":"cat","uploader_name":"alice"});
    let native = booru::parse_post(Site::Danbooru, &post).unwrap();
    let context = Post::from_media(std::slice::from_ref(&native), "danbooru");
    assert!(
        FilterExpression::parse("rating == 's' and score == -7 and metadata['score'] == score")
            .unwrap()
            .matches_post(&context)
            .unwrap()
    );
    assert!(
        FilterExpression::parse("filesize == 1234 and metadata['file']['filesize'] == filesize")
            .unwrap()
            .matches(&native)
            .unwrap()
    );
    assert_eq!(
        FilenameTemplate::parse("{post_id}_{rating}_{score}_{filesize}.{extension}")
            .unwrap()
            .render(&native)
            .unwrap(),
        "42_s_-7_1234.jpg"
    );
    let mut data = post.clone();
    data["filename"] = json!(md5);
    data["extension"] = json!("jpg");
    data["private_password"] = json!("PRIVATE_SECRET");
    let compatible = extract::parse_messages(
        &serde_json::to_vec(&json!([[3, url, data]])).unwrap(),
        "danbooru",
    )
    .unwrap()
    .remove(0);
    assert_eq!(native.archive_key(), compatible.archive_key());
    assert_eq!(
        compatible.metadata.as_ref().unwrap().rating.as_deref(),
        Some("s")
    );
    assert_eq!(compatible.metadata.as_ref().unwrap().score, Some(-7));
    assert_eq!(
        compatible
            .metadata
            .as_ref()
            .unwrap()
            .file
            .as_ref()
            .unwrap()
            .filesize,
        Some(1234)
    );
    assert!(
        !serde_json::to_string(&compatible)
            .unwrap()
            .contains("PRIVATE_SECRET")
    );
    let mut descriptive = native.clone();
    let meta = descriptive.metadata.as_mut().unwrap();
    meta.score = None;
    meta.rating = None;
    meta.file.as_mut().unwrap().filesize = None;
    assert_eq!(descriptive.archive_key(), native.archive_key());
    assert_eq!(descriptive.identity(), native.identity());
}

#[test]
fn optional_booru_metadata_keeps_zero_and_signed_values_and_omits_bad_shapes() {
    for (score, size) in [
        (json!(0), json!(0)),
        (json!(-3), json!("234")),
        (json!({"total":-8}), json!(234)),
    ] {
        let data = json!({"id":42,"filename":"photo","extension":"jpg","rating":"q","score":score,"file_size":size});
        let media = extract::parse_messages(
            &serde_json::to_vec(&json!([[3, "https://static1.e621.net/photo.jpg", data]])).unwrap(),
            "e621",
        )
        .unwrap()
        .remove(0);
        let metadata = media.metadata.as_ref().unwrap();
        assert!(metadata.score.is_some());
        assert!(metadata.file.as_ref().unwrap().filesize.is_some());
    }
    let data = json!({"id":42,"filename":"photo","extension":"jpg","rating":{},"score":1.5,"file_size":-1});
    let media = extract::parse_messages(
        &serde_json::to_vec(&json!([[3, "https://cdn.donmai.us/photo.jpg", data]])).unwrap(),
        "danbooru",
    )
    .unwrap()
    .remove(0);
    let metadata = media.metadata.as_ref().unwrap();
    assert!(metadata.score.is_none() && metadata.rating.is_none() && metadata.file.is_none());
}

#[test]
fn compatibility_booru_hashes_match_native_validation_and_normalization() {
    for (hash, expected) in [
        (
            "0123456789ABCDEF0123456789ABCDEF",
            "0123456789abcdef0123456789abcdef",
        ),
        ("invalid", "42"),
    ] {
        let post = json!({"id":42,"md5":hash,"file_url":"https://cdn.donmai.us/photo.jpg","file_ext":"jpg"});
        let native = booru::parse_post(Site::Danbooru, &post).unwrap();
        let compatible = extract::parse_messages(
            &serde_json::to_vec(&json!([[3, "https://cdn.donmai.us/photo.jpg", post]])).unwrap(),
            "danbooru",
        )
        .unwrap()
        .remove(0);
        assert_eq!(native.metadata.as_ref().unwrap().media_id, expected);
        assert_eq!(compatible.metadata.as_ref().unwrap().media_id, expected);
        assert_eq!(native.archive_key(), compatible.archive_key());
    }
}
