//! Genuine Source parent-metadata modes through native artist child jobs.
use super::{
    artist_tests::{
        EVENTS, artifacts, assert_requests, entries, native_category, serve, templates,
    },
    *,
};
use crate::{
    archive::Archive,
    config::Config,
    download::{self, DownloadOptions},
    extract::MediaJob,
    filter::FilterExpression,
    job_selection::JobSelection,
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

const KEYS: &[&str] = &[
    "category",
    "subcategory",
    "id",
    "name",
    "filename",
    "extension",
    "search_tags",
    "tags",
    "num",
    "date",
    "date_updated",
    "date_url",
    "nested",
    "replacement",
    "primitives",
    "marker",
    "artist_data",
    "primary",
    "legacy",
    "_parent",
    "metadata",
    "_extractor",
    "raw",
    "copy",
    "_url",
];
fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "../tests/fixtures/source_archive/booru_artist_parent.json"
    ))
    .unwrap()
}
fn normalized(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(key, value)| {
                    !matches!(key.as_str(), "_path" | "_extr")
                        && !(key.as_str() == "_extractor"
                            && matches!(
                                value.as_str(),
                                Some("DanbooruTagExtractor" | "E621TagExtractor")
                            ))
                })
                .map(|(key, value)| (key.clone(), normalized(value)))
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.iter().map(normalized).collect()),
        value => value.clone(),
    }
}
fn config(case: &Value, root: &Path) -> Config {
    let category = native_category(case);
    let class = case["subcategory"].as_str().unwrap();
    Config::parse(
        &serde_json::to_vec(&json!({"extractor":{category:{class:case["options"]}}})).unwrap(),
        root,
    )
    .unwrap_or_else(|error| panic!("{}: {error}", case["name"]))
}
fn processors(values: Vec<Value>, root: &Path) -> Arc<Postprocessors> {
    Arc::new(Postprocessors::new(
        values
            .iter()
            .map(|value| Specification::parse(value, root).unwrap())
            .collect(),
    ))
}
fn observations(root: &Path) -> Vec<Value> {
    EVENTS.iter().map(|event| json!({"name":"metadata","mode":"jsonl","event":event,"base-directory":root,"directory":[],"filename":format!("oracle-{event}.jsonl"),"include":KEYS})).collect()
}
fn source_post_ids(case: &Value) -> Vec<String> {
    case["calls"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|call| Url::parse(call["url"].as_str().unwrap()).unwrap().path() == "/posts.json")
        .flat_map(|call| {
            call["response"]
                .as_array()
                .or_else(|| call["response"]["posts"].as_array())
                .unwrap()
        })
        .map(|post| match &post["id"] {
            Value::String(value) => value.parse::<u64>().unwrap().to_string(),
            Value::Number(value) => value.as_u64().unwrap().to_string(),
            _ => panic!("invalid fixture post identity"),
        })
        .collect()
}

#[tokio::test]
async fn parent_modes_replay_actual_source_scopes_keywords_paths_and_archives() {
    let oracle = fixture();
    let cases = oracle["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 168);
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let category = native_category(case);
        let root_class = case["subcategory"].as_str().unwrap();
        let (api_root, server) = serve(case);
        let root = tempfile::tempdir().unwrap();
        let config = config(case, root.path());
        let root_options = config.resolve_class(category, root_class).unwrap();
        let tag_options = config.resolve_class(category, "tag").unwrap();
        let native = super::artist_tests::client(case, api_root.clone())
            .with_artist_parent_metadata(root_options.artist_parent_metadata().cloned());
        let url = Url::parse(case["url"].as_str().unwrap()).unwrap();
        let mut root_specs = case["hook_options"].as_array().unwrap().clone();
        root_specs.extend(observations(root.path()));
        let root_processors = processors(root_specs, root.path());
        let mut tag_specs = case["child_hook_options"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        tag_specs.push(json!({"name":"metadata","mode":"jsonl","event":"file","filename":"files.jsonl","base-directory":root.path(),"directory":[],"archive":[":basedirectory","metadata.sqlite"],"include":["category","subcategory","id","search_tags","filename","extension"]}));
        tag_specs.extend(observations(root.path()));
        let tag_processors = processors(tag_specs, root.path());
        let (filename, directory) = templates(case, root.path());
        let root_configuration = JobConfiguration {
            processors: root_processors,
            root: root.path().to_owned(),
            filename: filename.clone(),
            directory: directory.clone(),
            init: root_options.init.unwrap_or(false),
        };
        let tag_configuration = JobConfiguration {
            processors: tag_processors.clone(),
            root: root.path().to_owned(),
            filename: filename.clone(),
            directory: directory.clone(),
            init: tag_options.init.unwrap_or(false),
        };
        let selection = JobSelection {
            child_range: root_options
                .child_range
                .as_deref()
                .map(RangeSelection::parse)
                .transpose()
                .unwrap(),
            child_filter: root_options
                .child_filter
                .as_deref()
                .map(FilterExpression::parse_all)
                .transpose()
                .unwrap(),
            ..Default::default()
        };
        let input = selection
            .apply_events(native.events(&url))
            .map(|result| {
                result.map(|mut event| {
                    if let ExtractionEvent::Media(media) = &mut event {
                        // Keep the genuine URL in Source keyword contexts while the
                        // public inline job supplies deterministic download bytes.
                        media.job = MediaJob::InlineText {
                            data: case["download_body"].as_str().unwrap().to_owned(),
                        };
                    }
                    event
                })
            })
            .boxed();
        let configurations = Arc::new(BTreeMap::from([
            (format!("{category}.{root_class}"), root_configuration),
            (format!("{category}.tag"), tag_configuration),
        ]));
        let mut jobs = jobs::apply_with_source_class(
            input,
            configurations,
            Some(category.into()),
            Some(root_class.into()),
            crate::source_archive::for_url(&url),
        );
        let client = download::client(Duration::from_secs(30)).unwrap();
        let history = Archive::open(root.path().join("history.sqlite3"))
            .await
            .unwrap();
        let ids = source_post_ids(case);
        let mut index = 0;
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
            let post_id = &item.media.metadata.as_ref().unwrap().post_id;
            assert_eq!(
                post_id, &ids[index],
                "{name}: inherited id cannot replace canonical media identity"
            );
            let key = format!(
                "{category}:v1:{:x}",
                Sha256::digest(json!([post_id]).to_string().as_bytes())
            );
            assert_eq!(
                item.media.archive_key().as_ref(),
                Some(&key),
                "{name}: stable history identity"
            );
            let outcome = download::download_with_template(
                &client,
                root.path(),
                &item.media,
                DownloadOptions {
                    postprocessors: Some(&tag_processors),
                    postprocessor_scope: item.scope.as_ref(),
                    directory: directory.as_ref(),
                    ..Default::default()
                },
                filename.as_ref(),
            )
            .await
            .unwrap_or_else(|error| panic!("{name}: {error}"));
            assert!(outcome.saved_path().unwrap().is_file());
            history.record(&key).await.unwrap();
            assert!(history.contains(&key).await.unwrap());
            identities.insert(key);
            index += 1;
        }
        assert!(errors.is_empty(), "{name}: {errors:?}");
        assert_eq!(
            index,
            ids.len(),
            "{name}: all hydrated source posts reached job processing"
        );
        assert_eq!(
            artifacts(root.path()),
            case["published"],
            "{name}: complete actual paths and bytes"
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
                "{name}: {event} callback count; actual={actual:?}"
            );
            for (actual, expected) in actual.iter().zip(expected) {
                let actual = normalized(actual);
                let expected = normalized(&expected["metadata"]);
                for key in KEYS {
                    assert_eq!(
                        actual.get(*key),
                        expected.get(*key),
                        "{name}: {event} exact live {key}"
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
            "{name}: actual source metadata archive keys"
        );
        assert_eq!(
            entries(
                &root.path().join("history.sqlite3"),
                "SELECT identity FROM completed_media ORDER BY identity"
            ),
            identities.into_iter().collect::<Vec<_>>(),
            "{name}: canonical native SQLite rows"
        );
        assert_requests(case, &api_root, server.join().unwrap());
    }
}
