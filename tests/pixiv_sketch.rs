use gallery_dl_rs::{
    event::Post,
    extract::{self, Media},
    filter::FilterExpression,
    naming::FilenameTemplate,
    pixiv::{self, Target},
};
use serde_json::{Value, json};

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/pixiv/sketch.json")).unwrap()
}
fn media(data: Value) -> Media {
    extract::parse_messages(
        &serde_json::to_vec(&json!([[
            3,
            "https://images.example.test/image.jpeg",
            data
        ]]))
        .unwrap(),
        "pixiv",
    )
    .unwrap()
    .remove(0)
}
fn first_data() -> Value {
    fixture()["cases"][0]["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["kind"] == "media")
        .unwrap()["metadata"]
        .clone()
}

#[test]
fn sketch_routes_and_compatibility_metadata_match_recorded_source_media() {
    let reference = fixture();
    let url = extract::http_url(reference["cases"][0]["url"].as_str().unwrap()).unwrap();
    assert_eq!(
        pixiv::target(&url).unwrap(),
        Target::Sketch("fixture.user".into())
    );
    assert_eq!(extract::social_site(&url), Some("pixiv"));
    for event in reference["cases"][0]["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["kind"] == "media")
    {
        let item = media(event["metadata"].clone());
        let post = item.metadata.as_ref().unwrap();
        let data = &event["metadata"];
        let scalar = |value: &Value| {
            value
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| value.to_string())
        };
        assert_eq!(post.post_id, scalar(&data["post_id"]));
        assert_eq!(post.media_id, scalar(&data["id"]));
        assert_eq!(post.timestamp, event["timestamp"].as_i64());
        assert_eq!(
            post.username.as_deref(),
            data["user"]["unique_name"].as_str()
        );
        assert_eq!(post.caption.as_deref(), data["text"].as_str());
        assert_eq!(item.filename, event["default_filename"].as_str().unwrap());
        assert_eq!(post.file.as_ref().unwrap().width, data["width"].as_u64());
        assert_eq!(post.file.as_ref().unwrap().height, data["height"].as_u64());
        assert!(post.sketch_archive);
        assert!(post.sketch.as_ref().unwrap().valid());
    }
}

#[test]
fn sketch_archives_use_owner_and_media_identity_separately_from_artworks_and_articles() {
    let data = first_data();
    let item = media(data.clone());
    let key = item.archive_key().unwrap();
    let mut changed = data.clone();
    changed["post_id"] = "moved-to-post".into();
    changed["text"] = "changed caption".into();
    changed["user"]["unique_name"] = "new-name".into();
    assert_eq!(media(changed.clone()).archive_key(), Some(key.clone()));
    changed["user"]["id"] = "different-owner".into();
    assert_ne!(media(changed).archive_key(), Some(key.clone()));
    let mut changed = data;
    changed["id"] = "different-photo".into();
    assert_ne!(media(changed).archive_key(), Some(key));
    let ordinary = media(json!({"id":42,"num":0,"filename":"42_p0","extension":"jpg"}));
    let sketch = media(
        json!({"post_id":42,"id":42,"user":{"id":17,"unique_name":"artist"},"filename":"image","extension":"jpg"}),
    );
    assert_ne!(sketch.archive_key(), ordinary.archive_key());
    assert!(sketch.archive_key().is_some());
    let mut article = ordinary;
    article.metadata.as_mut().unwrap().pixivision =
        Some(std::sync::Arc::new(pixiv::PixivisionMetadata {
            id: "77".into(),
            title: "Article".into(),
        }));
    assert_ne!(sketch.archive_key(), article.archive_key());
}

#[test]
fn malformed_or_conflicting_sketch_scope_cannot_fall_back_to_other_archives() {
    for data in [
        json!({"post_id":"post-A","id":42,"user":{"unique_name":"PRIVATE"}}),
        json!({"post_id":"post-A","id":42,"user":{"id":null,"unique_name":"PRIVATE"}}),
        json!({"post_id":null,"id":42,"user":{"id":17,"unique_name":"PRIVATE"}}),
    ] {
        let item = media(data);
        assert!(item.metadata.as_ref().unwrap().sketch_archive);
        assert!(item.archive_key().is_none());
        assert!(item.metadata.as_ref().unwrap().sketch.is_none());
    }
    for scope in ["search", "ranking", "followed", "pixivision"] {
        let mut item = media(first_data());
        let post = item.metadata.as_mut().unwrap();
        match scope {
            "search" => post.search_archive = true,
            "ranking" => post.ranking_archive = true,
            "followed" => post.followed_archive = true,
            _ => post.pixivision_archive = true,
        }
        assert!(item.archive_key().is_none());
    }
}

#[test]
fn sketch_public_fields_and_photo_identity_support_filters_templates_and_inspection() {
    let item = media(first_data());
    let filter = FilterExpression::parse("subcategory == 'sketch' and id == 'photo-X' and post_id == 'post-A' and user['id'] == 'owner-A' and user['unique_name'] == 'fixture.user' and favorite_count == 3 and metadata['id'] == id and sketch['user'] == user and width == 640").unwrap();
    assert!(filter.matches(&item).unwrap());
    let post = Post::from_media(std::slice::from_ref(&item), "pixiv");
    assert!(
        FilterExpression::parse(
            "post_id == 'post-A' and user['id'] == 'owner-A' and subcategory == 'sketch'"
        )
        .unwrap()
        .matches_post(&post)
        .unwrap()
    );
    assert_eq!(
        FilenameTemplate::parse("{post_id}_{id}_{user[unique_name]}.{extension}")
            .unwrap()
            .render(&item)
            .unwrap(),
        "post-A_photo-X_fixture.user.jpg"
    );
    let public = serde_json::to_value(&item).unwrap();
    assert_eq!(public["metadata"]["sketch"]["favorite_count"], 3);
    assert!(public["metadata"].get("sketch_archive").is_none());
    assert!(public["metadata"]["sketch"].get("_http_headers").is_none());
}

#[test]
fn sketch_original_dictionaries_keep_numeric_date_fields_in_templates() {
    let mut data = first_data();
    data["user"]["date"] = 17.into();
    data["extra"] = json!({"date_updated":23});
    let item = media(data);
    assert_eq!(
        FilenameTemplate::parse(
            "{user[date]}_{metadata[extra][date_updated]}_{sketch[user][date]}"
        )
        .unwrap()
        .render(&item)
        .unwrap(),
        "17_23_17"
    );
    assert_eq!(
        FilenameTemplate::parse("{date:%Y}")
            .unwrap()
            .render(&item)
            .unwrap(),
        "2024"
    );
    assert_eq!(
        FilenameTemplate::parse("{metadata[date]:%Y}")
            .unwrap()
            .render(&item)
            .unwrap(),
        "2024"
    );
}

#[test]
fn sketch_opaque_integer_ids_match_native_identity_rules_without_unsigned_truncation() {
    for id in [-1, i128::MAX] {
        let item = media(
            json!({"post_id":id,"id":id,"user":{"id":17,"unique_name":"artist"},"filename":"image","extension":"png"}),
        );
        let post = item.metadata.as_ref().unwrap();
        assert_eq!(post.post_id, id.to_string());
        assert_eq!(post.media_id, id.to_string());
        assert!(item.archive_key().is_some());
        let mut mixed = item.clone();
        mixed.metadata.as_mut().unwrap().post_id = "another-post".into();
        assert!(mixed.archive_key().is_none());
    }
}
