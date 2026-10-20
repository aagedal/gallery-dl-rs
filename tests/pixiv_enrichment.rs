use gallery_dl_rs::{
    event::Post,
    extract::{Media, parse_messages},
    filter::FilterExpression,
    naming::FilenameTemplate,
};
use serde_json::{Value, json};

fn media(data: Value) -> Media {
    parse_messages(
        &serde_json::to_vec(&json!([[3, "https://i.pximg.net/42_p0.jpg", data]])).unwrap(),
        "pixiv",
    )
    .unwrap()
    .remove(0)
}
fn work() -> Value {
    json!({"id":42,"num":0,"extension":"jpg","user":{"id":5,"account":"artist"}})
}

#[test]
fn full_comments_and_ordered_bookmark_tags_reach_posts_filters_and_templates() {
    let mut data = work();
    data["comments"] = json!([{"id":7,"comment":"猫 & <text>","user":{"id":9,"name":"Reader"},"stamp":{"stamp_url":"https://example.org/stamp.png"},"extra":{"flag":true}}]);
    data["tags_bookmark"] = json!(["保存", "保存", ""]);
    let item = media(data.clone());
    let post = Post::from_media(std::slice::from_ref(&item), "pixiv");
    let encoded = serde_json::to_value(item.metadata.as_ref().unwrap()).unwrap();
    assert_eq!(encoded["comments"], data["comments"]);
    assert_eq!(encoded["tags_bookmark"], data["tags_bookmark"]);
    assert_eq!(
        serde_json::to_value(post.metadata.as_ref().unwrap()).unwrap()["comments"],
        data["comments"]
    );
    let filter = FilterExpression::parse("comments[0]['user']['id'] == 9 and comments[0]['extra']['flag'] is True and tags_bookmark == ['保存', '保存', '']").unwrap();
    assert!(filter.matches(&item).unwrap());
    assert!(filter.matches_post(&post).unwrap());
    assert_eq!(
        FilenameTemplate::parse("{comments[0][user][name]}_{tags_bookmark[1]}.{extension}")
            .unwrap()
            .render(&item)
            .unwrap(),
        "Reader___.jpg"
    );
    assert_eq!(item.archive_key(), media(work()).archive_key());
}

#[test]
fn absent_empty_and_malformed_enrichment_remain_distinct() {
    let absent = media(work());
    assert!(absent.metadata.as_ref().unwrap().comments.is_none());
    assert!(absent.metadata.as_ref().unwrap().tags_bookmark.is_none());
    let mut data = work();
    data["comments"] = json!([]);
    data["tags_bookmark"] = json!([]);
    let empty = media(data.clone());
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
    data["comments"] = json!({"not":"an array"});
    data["tags_bookmark"] = json!(["valid", 5]);
    let malformed = media(data);
    assert!(malformed.metadata.as_ref().unwrap().comments.is_none());
    assert!(malformed.metadata.as_ref().unwrap().tags_bookmark.is_none());
}

#[test]
fn compatibility_enrichment_matches_recorded_upstream_events() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/pixiv/enrichment.json")).unwrap();
    let mut compared = 0;
    for case in fixture["cases"].as_array().unwrap() {
        for event in case["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["kind"] == "media")
        {
            let item = media(event["metadata"].clone());
            let actual = serde_json::to_value(item.metadata.as_ref().unwrap()).unwrap();
            for key in ["comments", "tags_bookmark"] {
                assert_eq!(
                    actual.get(key),
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
