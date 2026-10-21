//! Real Source date predicates operate on live Directory keywords, before range.
use super::{
    artist_tests::{artifacts, assert_requests, entries, native_category, public_artist},
    *,
};
use crate::{
    archive::Archive,
    config::{Config, Options},
    download::{self, DownloadOptions},
    extract::MediaJob,
    filter::FilterExpression,
    job_selection::JobSelection,
    post_selection::PostSelection,
    postprocess::{
        Postprocessors, Specification,
        jobs::{self, JobConfiguration},
    },
    selection::RangeSelection,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

const EVENTS: &[&str] = &[
    "post",
    "prepare",
    "file",
    "after",
    "post-after",
    "child",
    "child-after",
];
const KEYS: &[&str] = &[
    "category",
    "subcategory",
    "id",
    "name",
    "filename",
    "extension",
    "date",
    "scale",
    "search_tags",
    "_parent",
    "num",
];
fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "../tests/fixtures/source_archive/booru_date_selection.json"
    ))
    .unwrap()
}
fn transport_case(wrapper: &Value) -> Value {
    let mut case = wrapper["case"].clone();
    if wrapper["kind"] == "popular" {
        let responses = case["responses"].as_array().unwrap().clone();
        for (call, response) in case["calls"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .zip(responses)
        {
            call["response"] = response["body"].clone();
            call["status"] = response["status"].clone();
        }
    }
    case
}
fn config(wrapper: &Value, root: &Path) -> Config {
    let case = &wrapper["case"];
    let category = native_category(case);
    let settings = if wrapper["kind"] == "artist" {
        let class = case["subcategory"].as_str().unwrap();
        json!({"extractor":{category:{class:case["options"],"tag":wrapper.get("tag_selection").unwrap_or(&wrapper["selection"])}}})
    } else {
        json!({"extractor":{category:case["options"]}})
    };
    Config::parse(&serde_json::to_vec(&settings).unwrap(), root)
        .unwrap_or_else(|error| panic!("{}: {error}", wrapper["name"]))
}
fn selected(options: &Options) -> JobSelection {
    JobSelection {
        post_selection: Some(PostSelection::new(
            options
                .post_range
                .as_deref()
                .map(RangeSelection::parse)
                .transpose()
                .unwrap(),
            options
                .post_filter
                .as_deref()
                .map(FilterExpression::parse_all)
                .transpose()
                .unwrap(),
        )),
        before: options.date_before,
        after: options.date_after,
        unordered: false,
        ..Default::default()
    }
}
fn processors(mut specs: Vec<Value>, root: &Path) -> Arc<Postprocessors> {
    for event in EVENTS {
        specs.push(json!({"name":"metadata","mode":"jsonl","event":event,"base-directory":root,"directory":[],"filename":format!("oracle-{event}.jsonl"),"include":KEYS}));
    }
    Arc::new(Postprocessors::new(
        specs
            .iter()
            .map(|value| Specification::parse(value, root).unwrap())
            .collect(),
    ))
}
fn clock(value: &str) -> time::Date {
    let parts = value
        .split('-')
        .map(|part| part.parse::<i32>().unwrap())
        .collect::<Vec<_>>();
    time::Date::from_calendar_date(
        parts[0],
        time::Month::try_from(parts[1] as u8).unwrap(),
        parts[2] as u8,
    )
    .unwrap()
}
fn typed_public(value: &Value) -> Value {
    if let Some(value) = value.get("$str") {
        return value.clone();
    }
    match value {
        Value::Array(values) => Value::Array(values.iter().map(typed_public).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| (key.clone(), typed_public(value)))
                .collect(),
        ),
        value => value.clone(),
    }
}
#[tokio::test]
async fn source_date_selection_uses_live_values_before_range_and_stops_only_its_job() {
    let oracle = fixture();
    assert!(oracle["cases"].as_array().unwrap().len() >= 141);
    replay_cases(&oracle).await;
}

pub(crate) async fn replay_cases(oracle: &Value) {
    let cases = oracle["cases"].as_array().unwrap();
    // Artificial helper skip callbacks are not a booru extractor setting.
    // Every actual booru construction in these jobs has skip_date=None.
    for wrapper in cases {
        let name = wrapper["name"].as_str().unwrap();
        assert!(
            wrapper["predicate_constructions"]
                .as_array()
                .unwrap()
                .iter()
                .all(|item| item["skip_supplied"] == false)
        );
        let case = &wrapper["case"];
        let artist = wrapper["kind"] == "artist";
        let category = native_category(case);
        let root_class = case["subcategory"].as_str().unwrap();
        let transport = transport_case(wrapper);
        let (api_root, server) = super::artist_tests::serve(&transport);
        let root = tempfile::tempdir().unwrap();
        let config = config(wrapper, root.path());
        let root_options = config.resolve_class(category, root_class).unwrap();
        let tag_options = if artist {
            config.resolve_class(category, "tag").unwrap()
        } else {
            root_options.clone()
        };
        let native = super::artist_tests::client(case, api_root.clone())
            .with_popular_today(clock(oracle["frozen_today"].as_str().unwrap()))
            .with_artist_parent_metadata(root_options.artist_parent_metadata().cloned());
        let url = Url::parse(case["url"].as_str().unwrap()).unwrap();
        let root_processors = processors(
            case["hook_options"].as_array().unwrap().clone(),
            root.path(),
        );
        let tag_processors = if artist {
            let mut specs = case["child_hook_options"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            specs.push(json!({"name":"metadata","mode":"jsonl","event":"file","filename":"files.jsonl","base-directory":root.path(),"directory":[],"archive":[":basedirectory","metadata.sqlite"],"include":["category","subcategory","id","search_tags","filename","extension"]}));
            processors(specs, root.path())
        } else {
            root_processors.clone()
        };
        let root_config = JobConfiguration {
            processors: root_processors,
            root: root.path().into(),
            filename: None,
            directory: None,
            init: root_options.init.unwrap_or(false),
        };
        let child_config = JobConfiguration {
            processors: tag_processors.clone(),
            root: root.path().into(),
            filename: None,
            directory: None,
            init: tag_options.init.unwrap_or(false),
        };
        let consumed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed = consumed.clone();
        let source = native.events(&url).inspect(move |result| {
            let item = match result {
                Ok(ExtractionEvent::BeginChild(scope)) => Some(json!({"subcategory":scope.metadata.subcategory(),"kind":"child","url":scope.metadata.booru_artist_url().unwrap()})),
                Ok(ExtractionEvent::Post(_)) => Some(json!({"subcategory":if artist {"tag"} else {"popular"},"kind":"post","url":""})),
                Ok(ExtractionEvent::Media(media)) => Some(json!({"subcategory":if artist {"tag"} else {"popular"},"kind":"media","url":media.url.as_str()})),
                _ => None,
            };
            if let Some(item) = item { observed.lock().unwrap().push(item); }
        }).boxed();
        let class_selections = Arc::new(BTreeMap::from([(
            format!("{category}.tag"),
            selected(&tag_options),
        )]));
        let input = selected(&root_options)
            .apply_events_with_classes(source, class_selections)
            .map(|result| {
                result.map(|mut event| {
                    if let ExtractionEvent::Media(media) = &mut event {
                        media.job = MediaJob::InlineText {
                            data: case["download_body"].as_str().unwrap().into(),
                        };
                    }
                    event
                })
            })
            .boxed();
        let configurations = Arc::new(BTreeMap::from([
            (format!("{category}.{root_class}"), root_config),
            (format!("{category}.tag"), child_config),
        ]));
        let mut jobs = jobs::apply_with_source_class(
            input,
            configurations,
            Some(category.into()),
            Some(root_class.into()),
            crate::source_archive::for_url(&url),
        );
        let client = download::client(Duration::from_secs(2)).unwrap();
        let history = Archive::open(root.path().join("history.sqlite3"))
            .await
            .unwrap();
        let mut identities = BTreeSet::new();
        let mut errors = Vec::new();
        while let Some(result) = jobs.next().await {
            let item = match result {
                Ok(item) => item,
                Err(error) => {
                    errors.push(error.to_string());
                    continue;
                }
            };
            let id = &item.media.metadata.as_ref().unwrap().post_id;
            let identity = format!(
                "{category}:v1:{:x}",
                Sha256::digest(json!([id]).to_string().as_bytes())
            );
            assert_eq!(
                item.media.archive_key().as_ref(),
                Some(&identity),
                "{name}: canonical media identity"
            );
            let outcome = download::download_with_template(
                &client,
                root.path(),
                &item.media,
                DownloadOptions {
                    postprocessors: Some(&tag_processors),
                    postprocessor_scope: item.scope.as_ref(),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap_or_else(|error| panic!("{name}: {error}"));
            assert!(outcome.saved_path().unwrap().is_file());
            history.record(&identity).await.unwrap();
            identities.insert(identity);
        }
        let expected_errors = wrapper["date_predicate_calls"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| {
                item.get("exception").is_some() && item["exception"]["class"] != "StopExtraction"
            })
            .count();
        assert_eq!(
            errors.len(),
            expected_errors,
            "{name}: predicate errors {errors:?}"
        );
        for error in errors {
            for private in [
                "PRIVATE_INVALID_DATE",
                "PRIVATE_DATE",
                "PRIVATE_INVALID_BOUND",
                "127.0.0.1",
            ] {
                assert!(
                    !error.contains(private),
                    "{name}: date predicate error leaked private values"
                );
            }
        }
        assert_eq!(
            json!(*consumed.lock().unwrap()),
            wrapper["consumed_messages"],
            "{name}: exact source message consumption / date stop cancellation"
        );
        assert_eq!(
            artifacts(root.path()),
            case["published"],
            "{name}: publication paths and bytes"
        );
        for event in EVENTS {
            let expected = case["trace"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|value| value["event"] == *event)
                .collect::<Vec<_>>();
            let path = root.path().join(format!("oracle-{event}.jsonl"));
            let actual = if path.exists() {
                std::fs::read_to_string(path)
                    .unwrap()
                    .lines()
                    .map(|line| serde_json::from_str::<Value>(line).unwrap())
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            assert_eq!(
                actual.len(),
                expected.len(),
                "{name}: {event} callback count"
            );
            for (actual, expected) in actual.iter().zip(expected) {
                for key in KEYS {
                    assert_eq!(
                        actual.get(*key).map(public_artist),
                        expected["metadata"]
                            .get(*key)
                            .map(typed_public)
                            .as_ref()
                            .map(public_artist),
                        "{name}: {event} live {key}"
                    );
                }
            }
        }
        let metadata = root.path().join("metadata.sqlite");
        let rows = if metadata.exists() {
            entries(&metadata, "SELECT entry FROM archive ORDER BY entry")
        } else {
            Vec::new()
        };
        assert_eq!(
            json!(rows),
            case["archive_sql"],
            "{name}: actual metadata SQLite rows"
        );
        assert_eq!(
            entries(
                &root.path().join("history.sqlite3"),
                "SELECT identity FROM completed_media ORDER BY identity"
            ),
            identities.into_iter().collect::<Vec<_>>(),
            "{name}: native canonical history"
        );
        assert_requests(case, &api_root, server.join().unwrap());
    }
}
