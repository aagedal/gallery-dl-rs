//! Date conversions use actual Source formatter/MetadataPP typed-value oracles.
use crate::{
    filter::FilterExpression,
    postprocess::{metadata::MetadataOptions, template::Template},
};
use serde_json::{Map, Value, json};
use std::path::Path;

fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "../tests/fixtures/postprocessors/formatter_dates.json"
    ))
    .unwrap()
}

fn decode(value: &Value) -> Value {
    match value["kind"].as_str().unwrap() {
        "value" => value["value"].clone(),
        "datetime" => {
            crate::filter::source_date_json(Some(value["epoch"].as_f64().unwrap() as i64))
        }
        "NullDatetime" => crate::filter::source_date_json(None),
        "list" => Value::Array(
            value["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(decode)
                .collect(),
        ),
        "map" => Value::Object(
            value["entries"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(key, value)| (key.clone(), decode(value)))
                .collect(),
        ),
        kind => panic!("unknown oracle value kind {kind}"),
    }
}

fn check_value(actual: &Value, expected: &Value, label: &str) {
    let actual = crate::filter::source_primitive_from_json(actual).unwrap_or(actual);
    match expected["kind"].as_str().unwrap() {
        "datetime" | "NullDatetime" => {
            let timestamp = crate::filter::source_date_from_json(actual)
                .unwrap_or_else(|| panic!("{label}: missing typed datetime identity: {actual:?}"));
            assert_eq!(
                timestamp,
                if expected["kind"] == "NullDatetime" {
                    None
                } else {
                    Some(expected["epoch"].as_f64().unwrap() as i64)
                },
                "{label}: datetime instant"
            );
            let context = Map::from_iter([("probe".into(), actual.clone())]);
            assert_eq!(
                FilterExpression::parse("probe")
                    .unwrap()
                    .matches_keywords(&context)
                    .unwrap(),
                expected["truthy"].as_bool().unwrap(),
                "{label}: datetime truthiness"
            );
            assert_eq!(
                Template::parse("{probe}")
                    .unwrap()
                    .render(&context)
                    .unwrap(),
                expected["text"],
                "{label}: datetime string"
            );
            assert_eq!(
                Template::parse("{probe:%Y}")
                    .unwrap()
                    .render(&context)
                    .unwrap(),
                expected["year_format"],
                "{label}: datetime format"
            );
            assert_eq!(
                crate::postprocess::public_keywords(&context)["probe"],
                serde_json::from_str::<Value>(expected["public_json"].as_str().unwrap()).unwrap(),
                "{label}: public JSON datetime export"
            );
        }
        "value" => assert_eq!(actual, &expected["value"], "{label}: primitive identity"),
        "list" => {
            let actual = actual.as_array().unwrap();
            let expected = expected["items"].as_array().unwrap();
            assert_eq!(actual.len(), expected.len(), "{label}: list length");
            for (index, (actual, expected)) in actual.iter().zip(expected).enumerate() {
                check_value(actual, expected, &format!("{label}[{index}]"));
            }
        }
        "map" => {
            let actual = actual.as_object().unwrap();
            let expected = expected["entries"].as_object().unwrap();
            assert_eq!(actual.len(), expected.len(), "{label}: map length");
            for (key, expected) in expected {
                check_value(
                    actual
                        .get(key)
                        .unwrap_or_else(|| panic!("{label}: missing key {key}")),
                    expected,
                    &format!("{label}[{key}]"),
                );
            }
        }
        kind => panic!("{label}: unknown oracle value kind {kind}"),
    }
}

#[test]
fn actual_source_date_formatter_text_and_typed_identity() {
    let oracle = fixture();
    assert_eq!(oracle["conversion_cases"].as_array().unwrap().len(), 1230);
    let mut replayed = 0;
    for case in oracle["conversion_cases"].as_array().unwrap() {
        if case["preexisting_subsecond_or_aware"] == true
            || contains_python_string(&case["context"])
        {
            continue;
        }
        replayed += 1;
        let name = case["name"].as_str().unwrap();
        let input = decode(&case["context"]);
        let context = input.as_object().unwrap();
        let source = case["template"].as_str().unwrap();
        let template = Template::parse(source).unwrap_or_else(|error| panic!("{name}: {error}"));
        let text = template.render(context);
        let identity = template.render_source_value(context);
        if case["default"].get("error").is_some() {
            assert!(
                text.is_err(),
                "{name}: expected content error, got {text:?}"
            );
        } else {
            let expected = &case["default"]["output"];
            let expected_text = match expected["kind"].as_str().unwrap() {
                "datetime" | "NullDatetime" => expected["text"].as_str().unwrap(),
                "value" => expected["value"].as_str().unwrap(),
                kind => panic!("{name}: unexpected content type {kind}"),
            };
            assert_eq!(
                text.unwrap_or_else(|error| panic!("{name} {source}: {error}")),
                expected_text,
                "{name} {source}: content text"
            );
        }
        if case["identity"].get("error").is_some() {
            assert!(
                identity.is_err(),
                "{name}: expected identity error, got {identity:?}"
            );
        } else {
            check_value(
                &identity.unwrap_or_else(|error| panic!("{name}: {error}")),
                &case["identity"]["output"],
                &format!("{name} {source}: identity"),
            );
        }
    }
    assert_eq!(replayed, 1144);
}

#[tokio::test]
async fn actual_source_metadata_modify_preserves_new_datetime_aliases_globally() {
    let oracle = fixture();
    let root = tempfile::tempdir().unwrap();
    let media =
        crate::extract::direct(&"https://fixture.invalid/file.jpg".parse().unwrap()).unwrap();
    assert_eq!(oracle["modify_cases"].as_array().unwrap().len(), 32);
    let mut replayed = 0;
    for case in oracle["modify_cases"].as_array().unwrap() {
        if case["preexisting_subsecond_or_aware"] == true {
            continue;
        }
        replayed += 1;
        let name = case["name"].as_str().unwrap();
        let mut input = decode(&case["context"]).as_object().unwrap().clone();
        let options =
            MetadataOptions::parse(case["options"].as_object().unwrap(), root.path()).unwrap();
        options
            .run(&mut input, &media, Path::new("file.jpg"), root.path())
            .await
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        check_value(&Value::Object(input), &case["output"], name);
    }
    assert_eq!(replayed, 28);
}

fn contains_python_string(value: &Value) -> bool {
    match value {
        Value::Object(map) => {
            map.get("kind").is_some_and(|kind| kind == "python_string")
                || map.values().any(contains_python_string)
        }
        Value::Array(values) => values.iter().any(contains_python_string),
        _ => false,
    }
}

// The Field API accepts actual datetime values, so preexisting microseconds are
// tested without rounding them through the whole-second JSON keyword wrapper.
fn decode_field(value: &Value) -> super::format::Value {
    use super::format::Value as F;
    match value["kind"].as_str().unwrap() {
        "datetime" => {
            let iso = value["iso"].as_str().unwrap();
            let local = iso
                .get(..26)
                .filter(|_| value["microsecond"] != 0)
                .unwrap_or(&iso[..19]);
            let local = format!("{local}Z");
            let wall =
                time::OffsetDateTime::parse(&local, &time::format_description::well_known::Rfc3339)
                    .unwrap();
            let offset = value["offset_seconds"].as_f64().unwrap();
            let utc = wall - time::Duration::microseconds((offset * 1_000_000.0) as i64);
            if offset.fract() == 0.0 {
                F::Date(utc.to_offset(time::UtcOffset::from_whole_seconds(offset as i32).unwrap()))
            } else {
                // Used only where the production field explicitly converts the
                // date to normalized UTC, or rejects it as a timestamp input.
                F::Date(utc)
            }
        }
        "NullDatetime" => F::InvalidDate,
        "value" => match &value["value"] {
            Value::Null => F::None,
            Value::Bool(value) => F::Bool(*value),
            Value::String(value) => F::Str(value.clone()),
            Value::Number(value) if value.as_i128().is_some() => F::Int(value.as_i128().unwrap()),
            Value::Number(value) => F::Float(value.as_f64().unwrap()),
            value => panic!("unexpected primitive {value}"),
        },
        "list" => F::List(
            value["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(decode_field)
                .collect(),
        ),
        "map" => F::Map(
            value["entries"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(key, value)| (key.clone(), decode_field(value)))
                .collect(),
        ),
        kind => panic!("unexpected field input {kind}"),
    }
}

fn check_field(actual: &super::format::Value, expected: &Value, label: &str) {
    use super::format::{Field, Value as F};
    match expected["kind"].as_str().unwrap() {
        "datetime" => {
            let date = match actual {
                F::Date(date) | F::SourceDate(date) => date,
                other => panic!("{label}: expected datetime, got {other:?}"),
            };
            assert_eq!(
                date.microsecond(),
                expected["microsecond"].as_u64().unwrap() as u32,
                "{label}: preserved subsecond identity"
            );
            let text = Field::parse("probe!s", |_| true)
                .unwrap()
                .render_content(|_| Some(actual.clone()))
                .unwrap()
                .unwrap();
            assert_eq!(text, expected["text"], "{label}: actual date text");
            assert!(expected["truthy"].as_bool().unwrap());
        }
        "NullDatetime" => assert!(
            matches!(actual, F::InvalidDate),
            "{label}: expected invalid datetime, got {actual:?}"
        ),
        "value" => match &expected["value"] {
            Value::String(value) => assert_eq!(actual, &F::Str(value.clone()), "{label}"),
            Value::Number(value) => {
                assert_eq!(actual, &F::Int(value.as_i128().unwrap()), "{label}")
            }
            Value::Null => assert_eq!(actual, &F::None, "{label}"),
            value => panic!("{label}: unexpected fixture output {value}"),
        },
        kind => panic!("{label}: unexpected field output {kind}"),
    }
}

#[test]
fn actual_source_preexisting_microsecond_dates_use_direct_field_values() {
    let oracle = fixture();
    let mut replayed = 0;
    for case in oracle["conversion_cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|case| case["preexisting_subsecond_or_aware"] == true)
    {
        let name = case["name"].as_str().unwrap();
        let source = case["template"].as_str().unwrap();
        // OffsetDateTime cannot encode a timezone whose offset has fractional
        // seconds. These two raw identity/string cases remain an explicit input
        // representation boundary, rather than rounding their timezone.
        if name == "special_nonutc_micro" && matches!(source, "{value}" | "x{value}y") {
            continue;
        }
        let super::format::Value::Map(context) = decode_field(&case["context"]) else {
            panic!("{name}: expected map");
        };
        let context = context
            .into_iter()
            .collect::<std::collections::BTreeMap<_, _>>();
        let template =
            Template::parse(source).unwrap_or_else(|error| panic!("{name} {source}: {error}"));
        let actual = template.render_format_context(&context);
        let label = format!("{name} {source} default");
        if case["default"].get("error").is_some() {
            assert!(actual.is_err(), "{label}: expected error, got {actual:?}");
        } else {
            let expected = &case["default"]["output"];
            let text = if expected["kind"] == "value" {
                &expected["value"]
            } else {
                &expected["text"]
            };
            assert_eq!(
                actual.unwrap_or_else(|error| panic!("{label}: {error}")),
                text.as_str().unwrap(),
                "{label}"
            );
        }
        if name == "special_utc_micro" && source == "{value}" {
            // Inspect the actual Field value before a JSON export would enter
            // the current runtime's whole-second identity representation.
            let actual = super::format::Field::parse("value", |_| true)
                .unwrap()
                .render_value(|key| context.get(key).cloned())
                .unwrap();
            check_field(
                &actual,
                &case["identity"]["output"],
                &format!("{name} {source}: identity"),
            );
        } else {
            let actual = template.render_source_format_context(&context);
            let label = format!("{name} {source} identity");
            if case["identity"].get("error").is_some() {
                assert!(actual.is_err(), "{label}: expected error, got {actual:?}");
            } else {
                check_value(
                    &actual.unwrap_or_else(|error| panic!("{label}: {error}")),
                    &case["identity"]["output"],
                    &label,
                );
            }
        }
        replayed += 1;
    }
    assert_eq!(replayed, 80);
}

#[tokio::test]
async fn actual_source_date_jobs_publish_exact_paths_bytes_and_metadata_archive_keys() {
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
        },
    };
    use futures_util::StreamExt;
    use std::{collections::BTreeMap, sync::Arc, time::Duration};
    let oracle = fixture();
    assert_eq!(oracle["job_cases"].as_array().unwrap().len(), 9);
    for case in oracle["job_cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        assert!(case["error"].is_null(), "{name}: source job must succeed");
        let source = case["url"].as_str().unwrap().parse().unwrap();
        let (api_root, server) = serve(replies(case, Site::Danbooru, &source));
        let root = tempfile::tempdir().unwrap();
        let (filename, directory) = source_templates(case, root.path());
        let events = ["post", "prepare", "file", "after", "post-after"];
        let keys = [
            "id",
            "category",
            "subcategory",
            "filename",
            "extension",
            "date",
            "raw",
            "copied",
            "year",
        ];
        let mut configs = case["hook_options"].as_array().unwrap().clone();
        for event in events {
            configs.push(json!({"name":"metadata","mode":"jsonl","event":event,"base-directory":root.path(),"directory":[],"filename":format!("oracle-{event}.jsonl"),"include":keys}));
        }
        let processors = Arc::new(Postprocessors::new(
            configs
                .iter()
                .map(|config| Specification::parse(config, root.path()).unwrap())
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
        let client = download::client(Duration::from_secs(2)).unwrap();
        let archive = Archive::open(root.path().join("history.sqlite3"))
            .await
            .unwrap();
        while let Some(result) = output.next().await {
            let item = result.unwrap_or_else(|error| panic!("{name}: {error}"));
            let original_id = case["input_posts"][0]["id"].as_u64().unwrap().to_string();
            let original_identity = format!(
                "danbooru:v1:{:x}",
                <sha2::Sha256 as sha2::Digest>::digest(json!([original_id]).to_string().as_bytes())
            );
            assert_eq!(
                item.media.archive_key().as_deref(),
                Some(original_identity.as_str()),
                "{name}: original canonical post identity"
            );
            let options = DownloadOptions {
                postprocessors: Some(&processors),
                postprocessor_scope: item.scope.as_ref(),
                directory: directory.as_ref(),
                ..Default::default()
            };
            let result = archive
                .download_with_template(
                    &client,
                    root.path(),
                    &item.media,
                    options,
                    filename.as_ref(),
                )
                .await
                .unwrap_or_else(|error| panic!("{name}: {error}"));
            assert!(
                matches!(result, ArchiveOutcome::Completed(_)),
                "{name}: canonical archive first download"
            );
            assert!(
                archive
                    .contains(&item.media.archive_key().unwrap())
                    .await
                    .unwrap(),
                "{name}: original canonical post history"
            );
        }
        assert_eq!(
            server.join().unwrap().len(),
            case["calls"].as_array().unwrap().len(),
            "{name}: source request count"
        );
        for event in events {
            let actual =
                std::fs::read_to_string(root.path().join(format!("oracle-{event}.jsonl"))).unwrap();
            let actual = actual
                .lines()
                .map(|line| serde_json::from_str::<Value>(line).unwrap())
                .collect::<Vec<_>>();
            let expected = case["typed_trace"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|trace| trace["event"] == event)
                .collect::<Vec<_>>();
            assert_eq!(actual.len(), expected.len(), "{name}: {event} callbacks");
            for (actual, expected) in actual.iter().zip(expected) {
                let expected = decode(&expected["metadata"]);
                let expected = crate::postprocess::public_keywords(expected.as_object().unwrap());
                for key in keys {
                    assert_eq!(actual.get(key), expected.get(key), "{name}: {event} {key}");
                }
            }
        }
        if let Some(expected) = case["archive_sql"].as_array() {
            let database = rusqlite::Connection::open(root.path().join("metadata.sqlite")).unwrap();
            let mut statement = database
                .prepare("SELECT entry FROM archive ORDER BY entry")
                .unwrap();
            let actual = statement
                .query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            assert_eq!(
                json!(actual),
                json!(expected),
                "{name}: actual Source MetadataPP archive keys"
            );
        }
        fn visit(root: &Path, directory: &Path, files: &mut Vec<Value>) {
            for entry in std::fs::read_dir(directory).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    visit(root, &path, files);
                    continue;
                }
                let relative = path.strip_prefix(root).unwrap().to_str().unwrap();
                if ["post", "prepare", "file", "after", "post-after"]
                    .iter()
                    .any(|event| relative == format!("oracle-{event}.jsonl"))
                    || matches!(
                        relative,
                        "history.sqlite3"
                            | "history.sqlite3-wal"
                            | "history.sqlite3-shm"
                            | "history.sqlite3-journal"
                            | "metadata.sqlite"
                            | "metadata.sqlite-wal"
                            | "metadata.sqlite-shm"
                            | "metadata.sqlite-journal"
                    )
                {
                    continue;
                }
                let bytes = std::fs::read(&path).unwrap();
                files.push(json!({"path":relative,"data_hex":bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>()}));
            }
        }
        let mut published = Vec::new();
        visit(root.path(), root.path(), &mut published);
        published.sort_by(|left, right| left["path"].as_str().cmp(&right["path"].as_str()));
        assert_eq!(
            json!(published),
            case["published"],
            "{name}: complete actual publication"
        );
    }
}

#[test]
fn source_only_string_and_fractional_offset_inputs_remain_explicit() {
    let oracle = fixture();
    let unrepresentable = oracle["conversion_cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|case| contains_python_string(&case["context"]))
        .collect::<Vec<_>>();
    assert_eq!(unrepresentable.len(), 4);
    for case in unrepresentable {
        let value = &case["context"]["entries"]["value"];
        assert_eq!(value["representable_json"], false);
        let escaped = value["codepoints"]
            .as_array()
            .unwrap()
            .iter()
            .map(|codepoint| format!("\\u{:04x}", codepoint.as_u64().unwrap()))
            .collect::<String>();
        assert!(
            serde_json::from_str::<Value>(&format!("\"{escaped}\"")).is_err(),
            "{}: no Rust Unicode string for a lone surrogate",
            case["name"]
        );
        assert!(matches!(
            case["identity"]["output"]["kind"].as_str().unwrap(),
            "datetime" | "NullDatetime"
        ));
    }
    let offset_identities = oracle["conversion_cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|case| {
            case["name"] == "special_nonutc_micro"
                && matches!(case["template"].as_str().unwrap(), "{value}" | "x{value}y")
        })
        .collect::<Vec<_>>();
    assert_eq!(offset_identities.len(), 2);
    for case in offset_identities {
        let value = &case["context"]["entries"]["value"];
        assert_ne!(value["offset_seconds"].as_f64().unwrap().fract(), 0.0);
        assert!(
            value["text"]
                .as_str()
                .unwrap()
                .ends_with("+05:30:01.500000")
        );
        assert_eq!(value["microsecond"], 123456);
    }
    let unsupported_modifies = oracle["modify_cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|case| case["preexisting_subsecond_or_aware"] == true)
        .collect::<Vec<_>>();
    assert_eq!(unsupported_modifies.len(), 4);
    for case in unsupported_modifies {
        // Runtime JSON date identities currently store whole seconds, so an
        // arbitrary existing datetime with microseconds cannot be injected by
        // decoding a Source descriptor into that wrapper without data loss.
        assert_eq!(case["context"]["entries"]["value"]["microsecond"], 123456);
    }
}

fn source_templates(
    case: &Value,
    root: &Path,
) -> (
    Option<crate::naming::FilenameTemplate>,
    Option<crate::naming::DirectoryTemplate>,
) {
    use crate::naming::{DirectoryTemplate, FilenameTemplate};
    let family = case["family"].as_str().unwrap();
    let config = crate::config::Config::parse(
        &serde_json::to_vec(&json!({"extractor":{family:case["overrides"]}})).unwrap(),
        root,
    )
    .unwrap();
    let options = config.resolve(family).unwrap();
    let filename = match &options.filename_conditions {
        Some(conditions) => Some(
            FilenameTemplate::parse_source_conditional(conditions, options.filename.as_deref())
                .unwrap(),
        ),
        None => options
            .filename
            .as_deref()
            .map(|source| FilenameTemplate::parse_source(source).unwrap()),
    };
    let directory = match &options.directory_conditions {
        Some(conditions) => Some(
            DirectoryTemplate::parse_source_conditional(conditions, options.directory.as_deref())
                .unwrap(),
        ),
        None => options
            .directory
            .as_deref()
            .map(|source| DirectoryTemplate::parse_source(source).unwrap()),
    };
    (filename, directory)
}

fn check_offset_payload(actual: &Value, expected: &Value, label: &str) {
    match expected["kind"].as_str().unwrap() {
        "datetime" | "NullDatetime" => {
            let context = Map::from_iter([("probe".into(), actual.clone())]);
            let epoch = expected["epoch"].as_f64().unwrap() as i64;
            if expected["kind"] == "NullDatetime" {
                if let Some(payload) = crate::filter::source_null_date_from_json(actual) {
                    assert_eq!(payload, epoch, "{label}: private NullDatetime payload");
                } else {
                    assert_eq!(
                        epoch, -62_135_596_800,
                        "{label}: shifted NullDatetime must retain a typed payload"
                    );
                }
            } else {
                assert!(
                    crate::filter::source_null_date_from_json(actual).is_none(),
                    "{label}: valid datetime cannot become NullDatetime"
                );
            }
            assert_eq!(
                Template::parse("{probe!T}")
                    .unwrap()
                    .render(&context)
                    .unwrap(),
                epoch.to_string(),
                "{label}: underlying shifted datetime epoch"
            );
            let iso = expected["iso"].as_str().unwrap();
            for (attribute, range) in [
                ("year", 0..4),
                ("month", 5..7),
                ("day", 8..10),
                ("hour", 11..13),
                ("minute", 14..16),
                ("second", 17..19),
            ] {
                let source = format!("{{probe.{attribute}}}");
                let value: u32 = iso[range].parse().unwrap();
                assert_eq!(
                    Template::parse(&source).unwrap().render(&context).unwrap(),
                    value.to_string(),
                    "{label}: shifted datetime {attribute}"
                );
            }
        }
        "map" => {
            for (key, expected) in expected["entries"].as_object().unwrap() {
                check_offset_payload(&actual[key], expected, &format!("{label}[{key}]"));
            }
        }
        "list" => {
            for (index, expected) in expected["items"].as_array().unwrap().iter().enumerate() {
                check_offset_payload(&actual[index], expected, &format!("{label}[{index}]"));
            }
        }
        "value" => (),
        kind => panic!("{label}: unknown offset value {kind}"),
    }
}

#[test]
fn actual_source_offset_fields_preserve_invalid_date_payload_and_fail_at_calendar_limits() {
    let oracle = fixture();
    assert_eq!(oracle["offset_cases"].as_array().unwrap().len(), 84);
    let mut count = 0;
    for case in oracle["offset_cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|case| case.get("template").is_some())
    {
        let name = case["name"].as_str().unwrap();
        let source = case["template"].as_str().unwrap();
        let context = decode(&case["context"]);
        let context = context.as_object().unwrap();
        let template = Template::parse(source).unwrap();
        let actual = template.render(context);
        if case["default"].get("error").is_some() {
            assert!(
                actual.is_err(),
                "{name}: default offset must fail, got {actual:?}"
            );
        } else {
            let expected = &case["default"]["output"];
            let text = if expected["kind"] == "value" {
                &expected["value"]
            } else {
                &expected["text"]
            };
            assert_eq!(
                actual.unwrap_or_else(|error| panic!("{name}: {error}")),
                text.as_str().unwrap(),
                "{name}: offset default formatter"
            );
        }
        let actual = template.render_source_value(context);
        if case["identity"].get("error").is_some() {
            assert!(
                actual.is_err(),
                "{name}: identity offset must fail, got {actual:?}"
            );
        } else {
            let actual = actual.unwrap_or_else(|error| panic!("{name}: {error}"));
            check_value(&actual, &case["identity"]["output"], name);
            check_offset_payload(&actual, &case["identity"]["output"], name);
        }
        count += 1;
    }
    assert_eq!(count, 72);
}

#[tokio::test]
async fn actual_source_offset_metadata_modify_keeps_false_invalid_dates_and_shifted_attributes() {
    let oracle = fixture();
    let root = tempfile::tempdir().unwrap();
    let media =
        crate::extract::direct(&"https://fixture.invalid/file.jpg".parse().unwrap()).unwrap();
    let mut count = 0;
    for case in oracle["offset_cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|case| case.get("options").is_some())
    {
        let name = case["name"].as_str().unwrap();
        let mut context = decode(&case["context"]).as_object().unwrap().clone();
        let options =
            MetadataOptions::parse(case["options"].as_object().unwrap(), root.path()).unwrap();
        options
            .run(&mut context, &media, Path::new("file.jpg"), root.path())
            .await
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        let actual = Value::Object(context);
        check_value(&actual, &case["output"], name);
        check_offset_payload(&actual, &case["output"], name);
        count += 1;
    }
    assert_eq!(count, 12);
}
