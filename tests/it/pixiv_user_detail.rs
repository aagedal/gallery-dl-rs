use gallery_dl_rs::{
    event::Post,
    extract::{Media, parse_messages},
    filter::FilterExpression,
    naming::FilenameTemplate,
    pixiv::UserDetail,
};
use serde_json::{Value, json};
use std::sync::Arc;
fn media(data: Value, category: &str, url: &str) -> Media {
    parse_messages(
        &serde_json::to_vec(&json!([[3, url, data]])).unwrap(),
        category,
    )
    .unwrap()
    .remove(0)
}
#[test]
fn flattened_artist_profiles_reach_posts_filters_and_templates_without_changing_archives() {
    let fields = json!({"user":{"id":5,"account":"artist","name":"Artist","extra":{"kept":true}},"profile":{"webpage":"https://example.org/","total_illusts":12},"profile_publicity":{"gender":"public"},"workspace":{"desk":"wood"},"api_extra":{"nested":[1,true]}});
    let detail = Arc::new(UserDetail::parse(&fields).unwrap());
    for (category, url, ext) in [
        ("pixiv", "https://i.pximg.net/42.jpg", "jpg"),
        ("pixiv-novel", "text:body", "txt"),
    ] {
        let mut item = media(
            json!({"id":42,"num":0,"extension":ext,"user":{"id":5,"account":"artist"}}),
            category,
            url,
        );
        let key = item.archive_key();
        item.metadata.as_mut().unwrap().pixiv_user_detail = Some(detail.clone());
        let encoded = serde_json::to_value(&item).unwrap();
        for key in fields.as_object().unwrap().keys() {
            assert_eq!(encoded["metadata"][key], fields[key]);
        }
        assert!(encoded["metadata"].get("pixiv_user_detail").is_none());
        let post = Post::from_media(std::slice::from_ref(&item), category);
        let predicate=FilterExpression::parse("user['id'] == 5 and profile['total_illusts'] == 12 and workspace['desk'] == 'wood' and api_extra['nested'][1] is True").unwrap();
        assert!(predicate.matches(&item).unwrap());
        assert!(predicate.matches_post(&post).unwrap());
        assert_eq!(
            FilenameTemplate::parse("{user[name]}_{workspace[desk]}.{extension}")
                .unwrap()
                .render(&item)
                .unwrap(),
            format!("Artist_wood.{ext}")
        );
        assert_eq!(item.archive_key(), key);
        assert!(Arc::ptr_eq(
            post.metadata
                .as_ref()
                .unwrap()
                .pixiv_user_detail
                .as_ref()
                .unwrap(),
            &detail
        ));
    }
}
#[test]
fn compatibility_known_detail_roots_match_actual_source_snapshots() {
    let fixture: Value =
        serde_json::from_str(include_str!("../fixtures/pixiv/user_detail.json")).unwrap();
    let mut compared = 0;
    for case in fixture["cases"].as_array().unwrap() {
        let category = if case["kind"] == "novel" {
            "pixiv-novel"
        } else {
            "pixiv"
        };
        for event in case["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["kind"] == "media")
        {
            let item = media(
                event["metadata"].clone(),
                category,
                event["url"].as_str().unwrap(),
            );
            let encoded = serde_json::to_value(item.metadata.as_ref().unwrap()).unwrap();
            for key in ["user", "profile", "profile_publicity", "workspace"] {
                assert_eq!(
                    encoded.get(key),
                    event["metadata"].get(key),
                    "{}: {key}",
                    case["name"]
                );
            }
            compared += 1;
        }
    }
    assert!(compared > 20);
}
