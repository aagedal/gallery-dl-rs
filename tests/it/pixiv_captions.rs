use gallery_dl_rs::{
    event::Post,
    extract::{Media, parse_messages},
    filter::FilterExpression,
    naming::FilenameTemplate,
};
use serde_json::{Value, json};
fn media(data: Value, category: &str, url: &str) -> Media {
    parse_messages(
        &serde_json::to_vec(&json!([[3, url, data]])).unwrap(),
        category,
    )
    .unwrap()
    .remove(0)
}
#[test]
fn complete_caption_html_is_available_separately_from_normalized_text() {
    for (category, url) in [
        ("pixiv", "https://i.pximg.net/42_p0.jpg"),
        ("pixiv-novel", "text:body"),
    ] {
        let mut data = json!({"id":42,"num":0,"extension":"jpg","user":{"id":5,"account":"artist"},"caption":"<p>A &amp; B</p><a href=\"https://example.org/\">Link</a>"});
        if category == "pixiv-novel" {
            data["extension"] = "txt".into();
        }
        let item = media(data.clone(), category, url);
        let post = Post::from_media(std::slice::from_ref(&item), category);
        let encoded = serde_json::to_value(item.metadata.as_ref().unwrap()).unwrap();
        assert_eq!(encoded["caption_html"], data["caption"]);
        assert_ne!(encoded["caption"], encoded["caption_html"]);
        let filter=FilterExpression::parse("caption_html.startswith('<p>A') and 'https://example.org/' in metadata['caption_html']").unwrap();
        assert!(filter.matches(&item).unwrap());
        assert!(filter.matches_post(&post).unwrap());
        assert_eq!(
            FilenameTemplate::parse("{caption_html[3:4]}.{extension}")
                .unwrap()
                .render(&item)
                .unwrap(),
            if category == "pixiv" {
                "A.jpg"
            } else {
                "A.txt"
            }
        );
        let key = item.archive_key();
        data["caption"] = "different".into();
        assert_eq!(key, media(data, category, url).archive_key());
    }
}
#[test]
fn compatibility_caption_html_matches_recorded_source_media() {
    let source: Value =
        serde_json::from_str(include_str!("../fixtures/pixiv/captions.json")).unwrap();
    let mut compared = 0;
    for case in source["cases"].as_array().unwrap() {
        let category = if case["novel"] == true {
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
            assert_eq!(
                item.metadata.as_ref().unwrap().caption_html.as_deref(),
                event["metadata"]["caption"].as_str(),
                "{}",
                case["name"]
            );
            compared += 1;
        }
    }
    assert!(compared > 20);
}
