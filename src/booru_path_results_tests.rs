//! Actual Source PathFormat result types, job failure checkpoints, and paths.
use super::{
    format::Value as F,
    source_formatter_date_tests::{check_value, decode, source_templates},
};
use crate::{
    archive::{Archive, ArchiveOutcome},
    booru::{
        Booru, Site,
        defaults_tests::{replies, serve},
    },
    download::{self, DownloadOptions},
    event::ExtractionEvent,
    network::NetworkOptions,
    postprocess::{
        Postprocessors, Specification,
        jobs::{self, JobConfiguration},
        template::Template,
    },
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path, sync::Arc, time::Duration};

const EVENTS: &[&str] = &["post", "prepare", "file", "after", "post-after"];
const KEYS: &[&str] = &[
    "id",
    "category",
    "subcategory",
    "raw",
    "filename",
    "extension",
    "date",
    "_url",
];
fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "../tests/fixtures/source_archive/booru_path_results.json"
    ))
    .unwrap()
}
fn media(case: &Value) -> crate::extract::Media {
    crate::booru::parse_post(Site::Danbooru, &case["case"]["input_posts"][0]).unwrap()
}
fn private_error(error: impl std::fmt::Display, name: &str) {
    let error = error.to_string();
    assert!(
        !error.contains("PRIVATE_BAD_"),
        "{name}: raw value leaked in error"
    );
    assert!(
        !error.contains("127.0.0.1"),
        "{name}: local transport URL leaked in error"
    );
    assert!(
        !error.contains("cdn.donmai.us"),
        "{name}: transport URL leaked in error"
    );
}
fn field_json(value: &F) -> Value {
    match value {
        F::Missing | F::None => Value::Null,
        F::Bool(value) => json!(value),
        F::Int(value) => json!(value),
        F::BigInt(value) => serde_json::from_str(value).unwrap(),
        F::Float(value) => json!(value),
        F::Str(value) => json!(value),
        F::List(values) => Value::Array(values.iter().map(field_json).collect()),
        F::Map(values) => Value::Object(
            values
                .iter()
                .map(|(key, value)| (key.clone(), field_json(value)))
                .collect(),
        ),
        F::Date(value) | F::SourceDate(value) => {
            crate::filter::source_date_json(Some(value.unix_timestamp()))
        }
        F::InvalidDate => crate::filter::source_date_json(None),
        F::NullDate(value) => crate::filter::source_null_date_json(value.unix_timestamp()),
        F::SourcePrimitive(value) => field_json(value),
    }
}

#[test]
fn actual_source_formatter_results_keep_types_before_path_cleanup() {
    let oracle = fixture();
    assert_eq!(oracle["cases"].as_array().unwrap().len(), 74);
    for case in oracle["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let media = media(case);
        let context = crate::postprocess::context(&media);
        let source = case["template"].as_str().unwrap();
        let template = Template::parse(source).unwrap();
        let result = template.render(&context);
        let expected = &case["direct_formatter"];
        if expected.get("error").is_some() {
            private_error(result.as_ref().unwrap_err(), name);
        } else {
            assert_eq!(
                result.unwrap_or_else(|error| panic!("{name}: {error}")),
                expected["text"],
                "{name}: actual formatter text"
            );
            if source.starts_with('{')
                && source.ends_with('}')
                && source.matches('{').count() == 1
                && !matches!(source, "{}" | "{!D}" | "{!d}")
            {
                let field =
                    super::format::Field::parse(&source[1..source.len() - 1], |_| true).unwrap();
                let context = crate::filter::context(&media);
                let value = field
                    .render_default_value(|key| super::lookup(&media, &context, key))
                    .unwrap_or_else(|error| panic!("{name}: {error}"));
                if expected["typed"]["kind"] == "formatter_default" {
                    assert!(
                        matches!(value, F::Missing),
                        "{name}: preserve missing default identity, got {value:?}"
                    );
                } else {
                    check_value(&field_json(&value), &expected["typed"], name);
                }
                assert_eq!(
                    value.identity_text().unwrap(),
                    expected["text"],
                    "{name}: typed result text"
                );
            }
            if case["empty_basename_transfer_caveat"] == true {
                assert_eq!(expected["type"], "str");
                assert_eq!(expected["text"], "");
                let error = super::FilenameTemplate::parse_source(source)
                    .unwrap()
                    .render(&media)
                    .unwrap_err();
                private_error(error, name);
            }
        }
    }
}

fn entries(path: &Path) -> Vec<String> {
    let connection = rusqlite::Connection::open(path).unwrap();
    connection
        .prepare("SELECT identity FROM completed_media ORDER BY identity")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}
/// Source oracles were recorded on POSIX; Windows path restriction also
/// replaces the characters gallery-dl's "auto" mode forbids there.
fn native_published(published: &Value) -> Value {
    let mut published = published.clone();
    if cfg!(windows) {
        for file in published.as_array_mut().unwrap() {
            let path = file["path"]
                .as_str()
                .unwrap()
                .replace(['\\', '|', '<', '>', ':', '"', '?', '*'], "_");
            file["path"] = Value::String(path);
        }
    }
    published
}
fn artifacts(root: &Path) -> Value {
    fn visit(root: &Path, directory: &Path, files: &mut Vec<Value>) {
        for entry in std::fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(root, &path, files);
                continue;
            }
            let relative = path
                .strip_prefix(root)
                .unwrap()
                .to_str()
                .unwrap()
                .replace(std::path::MAIN_SEPARATOR, "/");
            if EVENTS
                .iter()
                .any(|event| relative == format!("oracle-{event}.jsonl"))
                || matches!(
                    relative.as_str(),
                    "history.sqlite3"
                        | "history.sqlite3-wal"
                        | "history.sqlite3-shm"
                        | "history.sqlite3-journal"
                )
            {
                continue;
            }
            files.push(json!({"path":relative,"data_hex":std::fs::read(path).unwrap().iter().map(|byte|format!("{byte:02x}")).collect::<String>()}));
        }
    }
    let mut files = Vec::new();
    visit(root, root, &mut files);
    files.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
    json!(files)
}

#[tokio::test]
async fn actual_source_path_results_replay_jobs_errors_and_canonical_sql_history() {
    let oracle = fixture();
    let cases = oracle["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 74);
    assert_eq!(replay_jobs(cases).await, 7);
}

async fn replay_jobs(cases: &[Value]) -> usize {
    let mut caveats = 0;
    for wrapper in cases {
        let name = wrapper["name"].as_str().unwrap();
        let case = &wrapper["case"];
        let source = case["url"].as_str().unwrap().parse().unwrap();
        let (api_root, server) = serve(replies(case, Site::Danbooru, &source));
        let root = tempfile::tempdir().unwrap();
        let (filename, directory) = source_templates(case, root.path());
        let mut configs = case["hook_options"].as_array().unwrap().clone();
        for event in EVENTS {
            configs.push(json!({"name":"metadata","mode":"jsonl","event":event,"base-directory":root.path(),"directory":[],"filename":format!("oracle-{event}.jsonl"),"include":KEYS}));
        }
        let processors = Arc::new(Postprocessors::new(
            configs
                .iter()
                .map(|value| Specification::parse(value, root.path()).unwrap())
                .collect(),
        ));
        let client = Booru::new(Site::Danbooru, Duration::from_secs(5))
            .unwrap()
            .with_network(&NetworkOptions::new(Some(String::new()), None).unwrap())
            .unwrap()
            .with_api_root(api_root);
        let input = client
            .events(&source)
            .map(|result| {
                result.map(|mut event| {
                    if let ExtractionEvent::Media(media) = &mut event {
                        media.job = crate::extract::MediaJob::InlineText {
                            data: "source-default-path-fixture".into(),
                        };
                    }
                    event
                })
            })
            .boxed();
        let configuration = JobConfiguration {
            processors: processors.clone(),
            root: root.path().to_owned(),
            filename: filename.clone(),
            directory: directory.clone(),
            init: false,
        };
        let mut output = jobs::apply_with_descriptor(
            input,
            Arc::new(BTreeMap::from([("danbooru".into(), configuration)])),
            Some("danbooru".into()),
            crate::source_archive::for_url(&source),
        );
        let download = download::client(Duration::from_secs(30)).unwrap();
        let archive_path = root.path().join("history.sqlite3");
        let archive = Archive::open(&archive_path).await.unwrap();
        let original_id = case["input_posts"][0]["id"].as_u64().unwrap().to_string();
        let identity = format!(
            "danbooru:v1:{:x}",
            <sha2::Sha256 as sha2::Digest>::digest(json!([original_id]).to_string().as_bytes())
        );
        let mut errors = Vec::new();
        let mut completed = 0;
        while let Some(result) = output.next().await {
            let item = match result {
                Ok(item) => item,
                Err(error) => {
                    private_error(&error, name);
                    errors.push(error.to_string());
                    continue;
                }
            };
            assert_eq!(
                item.media.archive_key().as_deref(),
                Some(identity.as_str()),
                "{name}: immutable original post identity"
            );
            let options = DownloadOptions {
                postprocessors: Some(&processors),
                postprocessor_scope: item.scope.as_ref(),
                directory: directory.as_ref(),
                ..Default::default()
            };
            match archive
                .download_with_template(
                    &download,
                    root.path(),
                    &item.media,
                    options,
                    filename.as_ref(),
                )
                .await
            {
                Ok(ArchiveOutcome::Completed(_)) => completed += 1,
                Ok(ArchiveOutcome::AlreadyArchived) => {
                    panic!("{name}: unexpected prior canonical archive row")
                }
                Err(error) => {
                    private_error(&error, name);
                    errors.push(error.to_string());
                }
            }
        }
        assert_eq!(
            server.join().unwrap().len(),
            case["calls"].as_array().unwrap().len(),
            "{name}: actual API request count"
        );
        let caveat = wrapper["empty_basename_transfer_caveat"] == true;
        let source_error = !case["error"].is_null();
        assert_eq!(
            !errors.is_empty(),
            source_error || caveat,
            "{name}: Source error outcome or explicit empty-basename policy boundary: {errors:?}"
        );
        if source_error || caveat {
            assert_eq!(
                completed, 0,
                "{name}: failed primary media must not complete or archive"
            );
            let expected = if caveat {
                json!([])
            } else {
                native_published(&case["published"])
            };
            assert_eq!(
                artifacts(root.path()),
                expected,
                "{name}: preserve exact Source sidecars after failure; no unproven primary publication"
            );
            assert!(
                entries(&archive_path).is_empty(),
                "{name}: failed work cannot enter canonical SQLite history"
            );
        } else {
            assert_eq!(completed, 1, "{name}: one source file completes");
            assert_eq!(
                artifacts(root.path()),
                native_published(&case["published"]),
                "{name}: complete published relative paths and bytes"
            );
            assert_eq!(
                entries(&archive_path),
                vec![identity],
                "{name}: canonical SQLite row"
            );
        }
        let mut actual_phases = Vec::new();
        for event in EVENTS {
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
            actual_phases.extend(std::iter::repeat_n(*event, actual.len()));
            let expected = case["typed_trace"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|trace| trace["event"] == *event)
                .collect::<Vec<_>>();
            if caveat && matches!(*event, "file" | "after") {
                assert!(
                    actual.is_empty(),
                    "{name}: native rejects basename before file publication"
                );
                continue;
            }
            assert_eq!(
                actual.len(),
                expected.len(),
                "{name}: {event} callback count"
            );
            for (actual, expected) in actual.iter().zip(expected) {
                let expected = decode(&expected["metadata"]);
                let expected = crate::postprocess::public_keywords(expected.as_object().unwrap());
                for key in KEYS {
                    assert_eq!(
                        actual.get(*key),
                        expected.get(*key),
                        "{name}: {event} live keyword {key}"
                    );
                }
            }
        }
        if caveat {
            caveats += 1;
            assert_eq!(
                actual_phases,
                vec!["post", "prepare", "post-after"],
                "{name}: explicit early basename rejection checkpoint"
            );
            assert_eq!(
                case["trace"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|trace| trace["event"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                EVENTS,
                "{name}: synthetic transfer caveat is not a claimed Native publication match"
            );
        }
    }
    caveats
}

#[test]
fn actual_source_empty_basename_open_probes_explain_the_earlier_native_boundary() {
    let oracle = fixture();
    let probes = oracle["actual_empty_open_cases"].as_array().unwrap();
    assert_eq!(probes.len(), 4);
    for probe in probes {
        assert_eq!(probe["filename"], "");
        assert_eq!(probe["path"], "control/");
        assert_eq!(probe["realpath"], "control/");
        if probe["part_enabled"] == false {
            assert_eq!(probe["open_error"]["class"], "IsADirectoryError");
            assert_eq!(probe["remaining_files"], json!([]));
        } else {
            assert_eq!(probe["open"], "success");
            assert_eq!(probe["temppath_at_open"], "control/.part");
            assert_eq!(probe["finalize_error"]["class"], "IsADirectoryError");
            assert_eq!(
                probe["remaining_files"],
                json!([{"path":"control/.part","data_hex":"61637475616c2d736f757263652d6f70656e"}])
            );
        }
    }
}

#[tokio::test]
async fn actual_source_prepare_hooks_can_repair_or_break_paths_and_preserve_failure_sidecars() {
    let oracle = fixture();
    let cases = oracle["prepare_cases"].as_array().unwrap();
    assert_eq!(cases.len(), 6);
    assert_eq!(replay_jobs(cases).await, 0);
}

#[test]
fn actual_source_conversion_failures_propagate_instead_of_becoming_empty_fields() {
    let oracle = fixture();
    let cases = oracle["conversion_failure_cases"].as_array().unwrap();
    assert_eq!(cases.len(), 5);
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let source = case["template"].as_str().unwrap();
        let context = case["context"].as_object().unwrap();
        let template = Template::parse(source).unwrap();
        assert!(case["default"].get("error").is_some());
        assert!(case["identity"].get("error").is_some());
        let text = template.render(context).unwrap_err();
        let identity = template.render_source_value(context).unwrap_err();
        private_error(text, name);
        private_error(identity, name);
    }
}
