//! Genuine installed-source dt.convert vectors and DownloadJob boundary effects.
use crate::{config::Config, date::source_date_bound};
use serde_json::{Value, json};

fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "../tests/fixtures/source_archive/source_date_bound.json"
    ))
    .unwrap()
}
fn expected(case: &Value) -> Option<i64> {
    match &case["epoch"] {
        Value::Null => None,
        value => Some(value.as_i64().unwrap()),
    }
}
#[test]
fn source_date_bound_conversion_matches_all_representable_source_values() {
    let oracle = fixture();
    let cases = oracle["conversion_cases"].as_array().unwrap();
    assert_eq!(cases.len(), 328);
    let mut count = 0;
    for case in cases
        .iter()
        .filter(|case| case["representable_json"] == true)
    {
        let name = case["name"].as_str().unwrap();
        let value = case.get("value").unwrap();
        let actual = source_date_bound(value);
        assert_eq!(actual, expected(case), "{name}: source input {value:?}");
        assert_eq!(
            crate::source_date::convert(value),
            expected(case),
            "{name}: shared converter"
        );
        assert_eq!(
            actual.is_some(),
            case["truthy"].as_bool().unwrap(),
            "{name}: valid datetime versus NullDatetime"
        );
        assert_eq!(
            crate::filter::source_date_text(actual),
            case["normalized"].as_str().unwrap(),
            "{name}: naive UTC normalization"
        );
        count += 1;
    }
    assert_eq!(count, 326);
}
#[test]
fn source_date_bound_config_preserves_conversion_and_explicit_clearing_in_each_scope() {
    let oracle = fixture();
    let cases = oracle["conversion_cases"].as_array().unwrap();
    let base = cases
        .iter()
        .find(|case| case["representable_json"] == true && !case["epoch"].is_null())
        .unwrap();
    let base_value = &base["value"];
    let root = tempfile::tempdir().unwrap();
    for case in cases
        .iter()
        .filter(|case| case["representable_json"] == true)
    {
        let name = case["name"].as_str().unwrap();
        let value = case.get("value").unwrap();
        let category_settings = json!({"extractor":{"date-before":base_value,"date-after":base_value,"danbooru":{"date-before":value,"date-after":value}}});
        let category = Config::parse(
            &serde_json::to_vec(&category_settings).unwrap(),
            root.path(),
        )
        .unwrap_or_else(|error| panic!("{name}: {error}"));
        let options = category.resolve("danbooru").unwrap();
        assert_eq!(
            options.date_before,
            expected(case),
            "{name}: category before conversion/clear"
        );
        assert_eq!(
            options.date_after,
            expected(case),
            "{name}: category after conversion/clear"
        );
        let class_settings = json!({"extractor":{"danbooru":{"date-before":base_value,"date-after":base_value,"tag":{"date-before":value,"date-after":value}}}});
        let class = Config::parse(&serde_json::to_vec(&class_settings).unwrap(), root.path())
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        let options = class.resolve_class("danbooru", "tag").unwrap();
        assert_eq!(
            options.date_before,
            expected(case),
            "{name}: tag before conversion/clear"
        );
        assert_eq!(
            options.date_after,
            expected(case),
            "{name}: tag after conversion/clear"
        );
        let sibling = class.resolve_class("danbooru", "artist").unwrap();
        assert_eq!(
            sibling.date_before,
            expected(base),
            "{name}: class value must not mutate its parent/sibling"
        );
        assert_eq!(
            sibling.date_after,
            expected(base),
            "{name}: class value must not mutate its parent/sibling"
        );
    }
}
#[test]
fn raw_config_date_bounds_preserve_python_surrogate_separator_semantics() {
    let oracle = fixture();
    let cases = oracle["conversion_cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|case| case["representable_json"] == false)
        .collect::<Vec<_>>();
    assert_eq!(cases.len(), 2);
    for case in cases {
        // Python permits lone UTF-16 surrogates in str; Rust String and
        // serde_json::Value cannot represent them. Raw Config input has a narrow
        // date-bound lexical adapter; exercise it without inventing a Value.
        assert!(expected(case).is_some());
        let escaped = case["value_codepoints"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| format!("\\u{:04x}", value.as_u64().unwrap()))
            .collect::<String>();
        let raw_value = format!("\"{escaped}\"");
        assert!(
            serde_json::from_str::<Value>(&raw_value).is_err(),
            "{}",
            case["name"]
        );
        let document = format!(
            "{{\"extractor\":{{\"danbooru\":{{\"date-before\":{raw_value},\"date-after\":{raw_value}}}}}}}"
        );
        let root = tempfile::tempdir().unwrap();
        let config = Config::parse(document.as_bytes(), root.path())
            .unwrap_or_else(|error| panic!("{}: {error}", case["name"]));
        let options = config.resolve("danbooru").unwrap();
        assert_eq!(options.date_before, expected(case), "{}", case["name"]);
        assert_eq!(options.date_after, expected(case), "{}", case["name"]);
    }
}
#[tokio::test]
async fn source_date_bounds_replay_actual_publication_and_selection_jobs() {
    let oracle = fixture();
    assert_eq!(oracle["job_cases"].as_array().unwrap().len(), 66);
    let jobs = json!({"frozen_today":oracle["frozen_today"],"cases":oracle["job_cases"]});
    crate::booru::date_selection_tests::replay_cases(&jobs).await;
}
