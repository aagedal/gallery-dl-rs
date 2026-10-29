use gallery_dl_rs::{
    event::Post,
    extract::{self, Media},
    filter::FilterExpression,
    naming::FilenameTemplate,
    pixiv::{self, SearchMetadata, Target},
};
use serde_json::{Value, json};
use std::sync::Arc;

fn context() -> Value {
    json!({"word":"猫 + cat","sort":"date_desc","target":"exact_match_for_tags","date_start":null,"date_end":"2026-10-20"})
}

fn compat(search: Option<Value>) -> Media {
    let mut data = json!({"id":42,"num":0,"filename":"42_p0","extension":"jpg","title":"Title","caption":"<p>Description</p>"});
    if let Some(search) = search {
        data["search"] = search;
    }
    extract::parse_messages(
        &serde_json::to_vec(&json!([[3, "https://i.pximg.net/42_p0.jpg", data]])).unwrap(),
        "pixiv",
    )
    .unwrap()
    .remove(0)
}

#[test]
fn search_options_match_recorded_upstream_reference_vectors() {
    let fixture: Value =
        serde_json::from_str(include_str!("../fixtures/pixiv/search.json")).unwrap();
    for case in fixture["cases"].as_array().unwrap() {
        let url = extract::http_url(case["url"].as_str().unwrap()).unwrap();
        let target = pixiv::target(&url);
        if case["error"] == true {
            assert!(target.is_err(), "{case}");
            continue;
        }
        let Target::Search(options) = target.unwrap() else {
            panic!("{case}")
        };
        assert_eq!(
            serde_json::to_value(&options).unwrap(),
            case["metadata"],
            "{case}"
        );
        assert_eq!(extract::social_site(&url), Some("pixiv"));
    }
}

#[test]
fn search_archives_scope_by_term_independently_of_sort_target_and_date_bounds() {
    let media = compat(Some(context()));
    let key = media.archive_key().unwrap();
    let mut native = compat(None);
    native.metadata.as_mut().unwrap().search = Some(Arc::new(SearchMetadata {
        word: "猫 + cat".into(),
        sort: "popular_desc".into(),
        target: "title_and_caption".into(),
        date_start: Some("2020-01-01".into()),
        date_end: None,
    }));
    assert_eq!(Some(key.clone()), native.archive_key());
    assert_ne!(Some(key.clone()), compat(None).archive_key());
    let mut other = context();
    other["word"] = json!("other");
    assert_ne!(Some(key.clone()), compat(Some(other)).archive_key());
    native.metadata.as_mut().unwrap().media_id = "42_p1".into();
    assert_ne!(Some(key.clone()), native.archive_key());
    assert!(!key.contains("cat"));
    assert!(!key.contains("猫"));
    let public = serde_json::to_value(&media).unwrap();
    assert_eq!(public["metadata"]["search"], context());
    assert!(public["metadata"].get("search_archive").is_none());
    assert_eq!(
        media.metadata.as_ref().unwrap().caption.as_deref(),
        Some("Title\n\nDescription")
    );
}

#[test]
fn malformed_or_conflicting_search_context_cannot_use_an_ordinary_or_ranking_archive() {
    for search in [
        Value::Null,
        json!({}),
        json!("PRIVATE"),
        json!({"word":{},"sort":"date_desc","target":"exact_match_for_tags"}),
        json!({"word":"cat","sort":"PRIVATE","target":"exact_match_for_tags"}),
        json!({"word":"cat","sort":"date_desc","target":"PRIVATE"}),
        json!({"word":"cat","sort":"date_desc","target":"exact_match_for_tags","date_end":{"token":"PRIVATE"}}),
    ] {
        let media = compat(Some(search));
        assert!(media.archive_key().is_none());
        assert!(media.metadata.as_ref().unwrap().search.is_none());
        assert!(!serde_json::to_string(&media).unwrap().contains("PRIVATE"));
    }
    let mut mixed = compat(Some(context()));
    mixed.metadata.as_mut().unwrap().ranking_archive = true;
    assert!(mixed.archive_key().is_none());
    let mut empty = context();
    empty["word"] = json!("");
    assert!(compat(Some(empty)).archive_key().is_some());
}

#[test]
fn search_context_is_shared_by_post_file_predicates_and_templates() {
    let media = compat(Some(context()));
    let filter = FilterExpression::parse("search['word'] == '猫 + cat' and search['sort'] == 'date_desc' and search['target'] == 'exact_match_for_tags' and search['date_start'] is None and search['date_end'] == '2026-10-20'").unwrap();
    assert!(filter.matches(&media).unwrap());
    assert!(
        filter
            .matches_post(&Post::from_media(std::slice::from_ref(&media), "pixiv"))
            .unwrap()
    );
    assert!(
        FilterExpression::parse("metadata['search']['word'] == search['word']")
            .unwrap()
            .matches(&media)
            .unwrap()
    );
    assert_eq!(
        FilenameTemplate::parse("{search[word]}_{search[sort]}_{id}.{extension}")
            .unwrap()
            .render(&media)
            .unwrap(),
        "____cat_date_desc_42.jpg"
    );
}
