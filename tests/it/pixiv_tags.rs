use gallery_dl_rs::{
    event::Post,
    extract::{self, Media},
    filter::FilterExpression,
    naming::FilenameTemplate,
};
use serde_json::{Value, json};

fn compat(category: &str, tags: Option<Value>) -> Media {
    let mut data = json!({"id":42,"num":0,"filename":"42_p0","extension":"jpg"});
    if let Some(tags) = tags {
        data["tags"] = tags;
    }
    extract::parse_messages(
        &serde_json::to_vec(&json!([[3, "https://i.pximg.net/42_p0.jpg", data]])).unwrap(),
        category,
    )
    .unwrap()
    .remove(0)
}

#[test]
fn compatibility_retains_already_transformed_upstream_tags_at_post_and_file_boundaries() {
    let source: Value = serde_json::from_str(include_str!("../fixtures/pixiv/tags.json")).unwrap();
    for case in source["cases"].as_array().unwrap() {
        for (category, kind) in [("pixiv", "illustration"), ("pixiv-novel", "novel")] {
            let expected = &case["expected"][kind];
            if !expected["error"].is_null() {
                continue;
            }
            let media = compat(category, Some(expected["tags"].clone()));
            let tags = media.metadata.as_ref().unwrap().tags.as_ref().unwrap();
            assert_eq!(
                serde_json::to_value(tags).unwrap(),
                expected["tags"],
                "{}",
                case["name"]
            );
            let post = Post::from_media(std::slice::from_ref(&media), category);
            let post_tags = post.metadata.as_ref().unwrap().tags.as_ref().unwrap();
            assert_eq!(tags.values().as_ptr(), post_tags.values().as_ptr());
            assert_eq!(
                serde_json::to_value(post.metadata.as_ref().unwrap()).unwrap()["tags"],
                expected["tags"]
            );
            assert_eq!(
                serde_json::to_value(&media).unwrap()["metadata"]["tags"],
                expected["tags"]
            );
        }
    }
}

#[test]
fn tags_support_post_and_file_predicates_nested_original_fields_and_templates() {
    for category in ["pixiv", "pixiv-novel"] {
        let media = compat(category, Some(json!(["cat", "cat", "dog"])));
        let filter = FilterExpression::parse("tags == ['cat', 'cat', 'dog'] and len(tags) == 3 and 'dog' in tags and metadata['tags'][0] == 'cat'").unwrap();
        assert!(filter.matches(&media).unwrap());
        assert!(
            filter
                .matches_post(&Post::from_media(std::slice::from_ref(&media), category))
                .unwrap()
        );
        assert_eq!(
            FilenameTemplate::parse("{tags[0]}_{tags[2]}_{id}.{extension}")
                .unwrap()
                .render(&media)
                .unwrap(),
            "cat_dog_42.jpg"
        );
        let original = compat(
            category,
            Some(
                json!([{"z":1,"name":"cat","translated_name":null,"extra":{"flags":[true,null,2.5]}}]),
            ),
        );
        assert!(FilterExpression::parse("tags[0]['name'] == 'cat' and tags[0]['translated_name'] is None and tags[0]['extra']['flags'] == [True, None, 2.5]").unwrap().matches(&original).unwrap());
        assert_eq!(
            FilenameTemplate::parse("{tags[0][name]}_{id}.{extension}")
                .unwrap()
                .render(&original)
                .unwrap(),
            "cat_42.jpg"
        );
        let rendered = FilenameTemplate::parse("{tags[0]}")
            .unwrap()
            .render(&original)
            .unwrap();
        assert!(
            rendered.find('z').unwrap() < rendered.find("name").unwrap(),
            "{rendered}"
        );
        assert!(
            FilterExpression::parse("str(tags[0]).startswith(\"{'z': 1, 'name':\")")
                .unwrap()
                .matches(&original)
                .unwrap()
        );
    }
}

#[test]
fn tag_changes_do_not_change_archive_identity_and_empty_is_distinct_from_absent() {
    for category in ["pixiv", "pixiv-novel"] {
        let original = compat(
            category,
            Some(json!([{"name":"cat","translated_name":"dog"}])),
        );
        let translated = compat(category, Some(json!(["dog"])));
        assert_eq!(original.archive_key(), translated.archive_key());
        assert!(original.archive_key().is_some());
        let absent = compat(category, None);
        assert!(absent.metadata.as_ref().unwrap().tags.is_none());
        let empty = compat(category, Some(json!([])));
        assert!(
            empty
                .metadata
                .as_ref()
                .unwrap()
                .tags
                .as_ref()
                .unwrap()
                .values()
                .is_empty()
        );
        assert_eq!(empty.archive_key(), absent.archive_key());
        assert!(
            FilterExpression::parse("tags == []")
                .unwrap()
                .matches(&empty)
                .unwrap()
        );
    }
    assert!(
        compat("twitter", Some(json!(["cat"])))
            .metadata
            .as_ref()
            .unwrap()
            .tags
            .is_none()
    );
}

#[test]
fn oversized_tag_metadata_is_omitted_privately_without_losing_the_work() {
    for category in ["pixiv", "pixiv-novel"] {
        let media = compat(category, Some(json!(["PRIVATE".repeat(10000)])));
        assert!(media.metadata.as_ref().unwrap().tags.is_none());
        assert!(!serde_json::to_string(&media).unwrap().contains("PRIVATE"));
        assert!(media.archive_key().is_some());
    }
}

#[test]
fn original_tag_numbers_and_date_named_fields_keep_their_types_in_templates() {
    let media = compat(
        "pixiv",
        Some(json!([{"name":"cat","date":17,"extra":{"date_updated":23},"number":i128::MAX}])),
    );
    let template = FilenameTemplate::parse(
        "{tags[0][date]}_{metadata[tags][0][extra][date_updated]}_{tags[0][number]}",
    )
    .unwrap();
    assert_eq!(
        template.render(&media).unwrap(),
        format!("17_23_{}", i128::MAX)
    );
    assert_eq!(
        FilenameTemplate::parse("{tags[0][number]!j}")
            .unwrap()
            .render(&media)
            .unwrap(),
        i128::MAX.to_string()
    );
    assert!(
        FilterExpression::parse(&format!("tags[0]['number'] == {}", i128::MAX))
            .unwrap()
            .matches(&media)
            .unwrap()
    );
    let json = FilenameTemplate::parse("{tags[0]!j}")
        .unwrap()
        .render(&media)
        .unwrap();
    assert!(!json.contains("1970"), "{json}");
}
