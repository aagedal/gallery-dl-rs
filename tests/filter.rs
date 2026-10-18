use futures_util::{StreamExt, stream};
use gallery_dl_rs::{
    extract::{self, Media},
    filter::FilterExpression,
};
use serde_json::json;
fn direct() -> Media {
    extract::direct(&extract::http_url("https://cdn.example/cat.jpg").unwrap()).unwrap()
}
fn social() -> Media {
    extract::parse_messages(&serde_json::to_vec(&json!([[3,"https://cdn.example/cat.jpg?token=SIGNED_SECRET",{
        "tweet_id":"9007199254740993","num":2,"filename":"cat","extension":"jpg", "author":{"name":"Artist"},
        "content":"A Cat 🍂", "width":1920,"height":1080,"duration":1.25,"type":"photo","description":"Alt text",
        "date":1700000000,"source_id":"999","source_user":{"name":"Creator"},"sensitive_flags":["Nudity"],
        "community":{"id":55,"name":"Art","member_count":120,"rules":["Be kind"]},
        "article":{"id":66,"title":"Article","date":1700000000},
        "_http_headers":{"Authorization":"HEADER_SECRET"},"_fallback":["https://cdn.example/FALLBACK_SECRET"]
    }]])).unwrap(),"twitter").unwrap().remove(0)
}
fn matches(expr: &str, item: &Media) -> bool {
    FilterExpression::parse(expr)
        .unwrap_or_else(|error| panic!("parse {expr}: {error}"))
        .matches(item)
        .unwrap_or_else(|error| panic!("eval {expr}: {error}"))
}
#[test]
fn metadata_context_exposes_native_fields_upstream_aliases_and_nested_maps() {
    let media = social();
    for expr in [
        "width == image_width == 1920 and height == image_height == 1080",
        "width / height > 1.7 and 0 < duration < 2",
        "extension in exts_image and extension not in exts_video",
        "category == 'twitter' and filename == 'cat' and name == filename",
        "num == index == 2 and tweet_id == 9007199254740993 and post_id == tweet_id",
        "quote_id == 0 and retweet_id == 0 and source_user['id'] == 999 and source_user['name'] == 'Creator'",
        "type == media_type == 'photo' and source_id == 999",
        "username.lower() == 'artist' and content == caption and 'Cat' in caption",
        "description == 'Alt text' and date == timestamp == 1700000000",
        "metadata['file']['width'] == width and metadata['file'].get('height') == height",
        "community['id'] == 55 and community['name'] == 'Art' and community['rules'][0] == 'Be kind'",
        "article['id'] == 66 and article['title'] == 'Article'",
        "metadata.get('_http_headers') is None and metadata.get('_fallback') is None",
        "'SIGNED_SECRET' in _url and url == _url",
    ] {
        assert!(matches(expr, &media), "{expr}");
    }
    assert_eq!(media.archive_key(), social().archive_key());
}
#[test]
fn missing_values_can_be_guarded_without_exposing_private_context() {
    let media = direct();
    assert!(matches(
        "metadata.get('file', {}).get('width', 0) > 1 if False else extension == 'jpg'",
        &media
    ));
    assert!(matches("True or PRIVATE_UNKNOWN", &media));
    assert!(!matches("False and PRIVATE_UNKNOWN", &media));
    for expr in [
        "PRIVATE_UNKNOWN",
        "_http_headers",
        "metadata['private']",
        "1 / 0",
    ] {
        let error = FilterExpression::parse(expr)
            .and_then(|filter| filter.matches(&media))
            .unwrap_err()
            .to_string();
        assert!(!error.contains("PRIVATE"));
        assert!(!error.contains("http"));
    }
    assert!(
        !format!(
            "{:?}",
            FilterExpression::parse("'PRIVATE_LITERAL'").unwrap()
        )
        .contains("PRIVATE_LITERAL")
    );
}
#[tokio::test]
async fn filters_preserve_errors_and_use_false_for_per_file_evaluation_failures() {
    let filter = FilterExpression::parse("width >= 1000").unwrap();
    let source = stream::iter(vec![
        Ok(direct()),
        Err(gallery_dl_rs::error("fixture extraction error")),
        Ok(social()),
        Ok(direct()),
    ])
    .boxed();
    let mut selected = filter.apply(source);
    assert_eq!(
        selected.next().await.unwrap().unwrap_err().to_string(),
        "fixture extraction error"
    );
    assert_eq!(
        selected
            .next()
            .await
            .unwrap()
            .unwrap()
            .metadata
            .unwrap()
            .index,
        2
    );
    assert!(selected.next().await.is_none());
}
#[tokio::test]
async fn filtering_infinite_rejections_yields_for_cancellation() {
    let filter = FilterExpression::parse("False").unwrap();
    let source = stream::repeat(Ok::<_, &'static str>(direct()))
        .map(|value| value.map_err(gallery_dl_rs::error))
        .boxed();
    let mut selected = filter.apply(source);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(20), selected.next())
            .await
            .is_err()
    );
}
#[test]
fn repeated_clauses_short_circuit_and_limits_fail_privately() {
    let filter =
        FilterExpression::parse_all(&["extension == 'mp4'".into(), "PRIVATE_MISSING".into()])
            .unwrap();
    assert!(!filter.matches(&direct()).unwrap());
    assert!(
        FilterExpression::parse_all(&[])
            .unwrap()
            .matches(&direct())
            .unwrap()
    );
    assert!(FilterExpression::parse_all(&vec!["True".into(); 33]).is_err());
    assert!(FilterExpression::parse(&" ".repeat(16385)).is_err());
    assert!(
        FilterExpression::parse(&format!("{}True{}", "(".repeat(100), ")".repeat(100))).is_err()
    );
}

#[test]
fn supported_expressions_match_recorded_cpython_reference_results() {
    let fixture: serde_json::Value =
        serde_json::from_slice(include_bytes!("fixtures/filter/python.json")).unwrap();
    for case in fixture["cases"].as_array().unwrap() {
        let expression = case["expression"].as_str().unwrap();
        let filter = FilterExpression::parse(expression)
            .unwrap_or_else(|error| panic!("reference parse {expression}: {error}"));
        assert_eq!(
            filter.matches(&direct()).unwrap_or(false),
            case["result"].as_bool().unwrap(),
            "reference evaluation: {expression}"
        );
    }
}
