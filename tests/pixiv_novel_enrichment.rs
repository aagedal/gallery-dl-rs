use gallery_dl_rs::{
    event::Post,
    extract::{Media, parse_messages},
    filter::FilterExpression,
    naming::FilenameTemplate,
};
use serde_json::{Value, json};

fn media(data: Value, url: &str) -> Media {
    parse_messages(
        &serde_json::to_vec(&json!([[3, url, data]])).unwrap(),
        "pixiv-novel",
    )
    .unwrap()
    .remove(0)
}
fn novel() -> Value {
    json!({"id":42,"num":0,"filename":"42","extension":"txt","user":{"id":5,"account":"writer"}})
}

#[test]
fn novel_text_and_cover_keep_full_comment_metadata_and_bookmark_tag_order() {
    let mut data = novel();
    data["comments"] = json!([{"id":7,"comment":"Comment\nline","user":{"id":9,"name":"Reader"},"extra":{"preserved":true}}]);
    data["tags_bookmark"] = json!(["keep", "keep", ""]);
    let text = media(data.clone(), "text:Exact novel text\n");
    let post = Post::from_media(std::slice::from_ref(&text), "pixiv-novel");
    data["num"] = 1.into();
    data["extension"] = "jpg".into();
    let cover = media(
        data.clone(),
        "https://i.pximg.net/novel-cover-original/img/42.jpg",
    );
    for item in [&text, &cover] {
        let encoded = serde_json::to_value(item.metadata.as_ref().unwrap()).unwrap();
        assert_eq!(encoded["comments"], data["comments"]);
        assert_eq!(encoded["tags_bookmark"], data["tags_bookmark"]);
        assert!(
            FilterExpression::parse(
                "comments[0]['user']['id'] == 9 and tags_bookmark == ['keep', 'keep', '']"
            )
            .unwrap()
            .matches(item)
            .unwrap()
        );
        assert_eq!(
            FilenameTemplate::parse("{comments[0][user][name]}_{tags_bookmark[1]}.{extension}")
                .unwrap()
                .render(item)
                .unwrap(),
            format!(
                "Reader_keep.{}",
                if item.filename.ends_with(".txt") {
                    "txt"
                } else {
                    "jpg"
                }
            )
        );
    }
    assert!(
        FilterExpression::parse(
            "comments[0]['extra']['preserved'] is True and len(tags_bookmark) == 3"
        )
        .unwrap()
        .matches_post(&post)
        .unwrap()
    );
    assert_eq!(
        text.archive_key(),
        media(novel(), "text:Exact novel text\n").archive_key()
    );
}

#[test]
fn novel_empty_enrichment_is_preserved_separately_from_absent_fields() {
    let absent = media(novel(), "text:body");
    assert!(absent.metadata.as_ref().unwrap().comments.is_none());
    assert!(absent.metadata.as_ref().unwrap().tags_bookmark.is_none());
    let mut data = novel();
    data["comments"] = json!([]);
    data["tags_bookmark"] = json!([]);
    let empty = media(data, "text:body");
    assert!(
        empty
            .metadata
            .as_ref()
            .unwrap()
            .comments
            .as_ref()
            .unwrap()
            .is_empty()
    );
    assert!(
        empty
            .metadata
            .as_ref()
            .unwrap()
            .tags_bookmark
            .as_ref()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn compatibility_novel_enrichment_matches_actual_source_text_and_cover_events() {
    let source: Value =
        serde_json::from_str(include_str!("fixtures/pixiv/novel_enrichment.json")).unwrap();
    let mut compared = 0;
    let mut covers = 0;
    for case in source["cases"].as_array().unwrap() {
        for event in case["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["kind"] == "media")
        {
            let item = media(event["metadata"].clone(), event["url"].as_str().unwrap());
            let actual = serde_json::to_value(item.metadata.as_ref().unwrap()).unwrap();
            for key in ["comments", "tags_bookmark"] {
                assert_eq!(
                    actual.get(key),
                    event["metadata"].get(key),
                    "{}: {key}",
                    case["name"]
                );
            }
            assert_eq!(item.filename, event["default_filename"].as_str().unwrap());
            if event["url"].as_str().unwrap().starts_with("https:") {
                covers += 1;
            }
            compared += 1;
        }
    }
    assert!(compared > 20);
    assert!(covers > 0);
}

#[test]
fn novel_source_numbering_and_uploaded_fields_reach_filters_and_templates() {
    let mut data = novel();
    data["num"] = 2.into();
    data["suffix"] = "_p02".into();
    data["title"] = "Uploaded title".into();
    data["extension"] = "jpeg".into();
    data["extra_upload"] = json!({"nested":["first","second"]});
    let item = media(data.clone(), "https://i.pximg.net/upload.jpeg");
    let encoded = serde_json::to_value(item.metadata.as_ref().unwrap()).unwrap();
    assert_eq!(encoded["num"], 2);
    assert_eq!(encoded["suffix"], "_p02");
    assert_eq!(encoded["extra_upload"], data["extra_upload"]);
    assert!(
        FilterExpression::parse(
            "num == 2 and index == 3 and suffix == '_p02' and extra_upload['nested'][1] == 'second'"
        )
        .unwrap()
        .matches(&item)
        .unwrap()
    );
    assert_eq!(
        FilenameTemplate::parse("{title}_{num:02}_{metadata[extra_upload][nested][0]}.{extension}")
            .unwrap()
            .render(&item)
            .unwrap(),
        "Uploaded_title_02_first.jpg"
    );
}

#[test]
fn public_novel_child_snapshots_preserve_structured_fields_and_reject_transport_fields() {
    let raw = json!({"id":42,"num":3,"title":"Writer","user":{"id":5,"account":"artist"},"tags":["cat"],"extra_upload":{"nested":[1,2]},"_http_headers":{"Authorization":"SYNTHETIC_PRIVATE"},"_extractor":"internal","category":"wrong"});
    let child =
        gallery_dl_rs::event::ChildMetadata::from_pixiv_novel("novel", raw.as_object().unwrap());
    assert_eq!(child.category(), "pixiv-novel");
    assert!(!child.fields().contains_key("_http_headers"));
    assert!(!child.fields().contains_key("_extractor"));
    assert!(!child.fields().contains_key("category"));
    assert!(FilterExpression::parse("id == 42 and num == 3 and user['account'] == 'artist' and tags == ['cat'] and extra_upload['nested'][1] == 2").unwrap().matches_child(&child).unwrap());
}

#[test]
fn novel_snapshots_reject_excess_depth_and_encoded_size_before_publication() {
    let mut nested = json!(1);
    for _ in 0..130 {
        nested = json!({"next":nested});
    }
    let data = json!({"extra_upload":nested});
    assert!(gallery_dl_rs::pixiv::NovelMetadata::parse(data.as_object().unwrap()).is_none());
    // Escaping makes this value exceed the encoded limit while its raw string
    // remains comfortably below it; the serializer counts without a large buffer.
    let data = json!({"extra_upload":"\0".repeat(6*1024*1024)});
    assert!(gallery_dl_rs::pixiv::NovelMetadata::parse(data.as_object().unwrap()).is_none());
}
