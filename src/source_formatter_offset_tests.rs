//! Genuine Source O argument parsing and isolated local-time replay.
//!
//! Local fixtures replay captured TZif bytes in isolated Unix subprocesses,
//! avoiding host timezone-database/backzone differences. Their negative epochs
//! and year-9998 probes require 64-bit Unix; Windows and 32-bit Unix have an
//! explicit snapshot replay boundary (even where a 32-bit libc has 64-bit time_t).
use super::source_formatter_date_tests::{
    check_offset_payload, check_value, decode, decode_field, replay_date_jobs,
};
use crate::postprocess::{metadata::MetadataOptions, template::Template};
use serde_json::Value;
use std::path::Path;
#[cfg(all(unix, target_pointer_width = "64"))]
use std::process::Command;

fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "../tests/fixtures/postprocessors/formatter_offsets.json"
    ))
    .unwrap()
}
fn private_error(error: impl std::fmt::Display, name: &str) {
    let error = error.to_string();
    assert!(
        !error.contains("PRIVATE_BAD_ARGUMENT"),
        "{name}: raw argument leaked"
    );
}
fn expected_text(value: &Value) -> &str {
    match value["kind"].as_str().unwrap() {
        "datetime" | "NullDatetime" => value["text"].as_str().unwrap(),
        "value" => value["value"].as_str().unwrap(),
        kind => panic!("unexpected formatter output type {kind}"),
    }
}
fn replay_cases(cases: &[Value]) -> (usize, usize) {
    let mut boundaries = 0;
    let mut typed = 0;
    for case in cases {
        if case.get("native_representation_boundary").is_some() {
            check_aware_boundary(case);
            boundaries += 1;
            continue;
        }
        if case["requires_typed_context"] == true {
            replay_typed_case(case);
            typed += 1;
            continue;
        }
        let name = case["name"].as_str().unwrap();
        let source = case["template"].as_str().unwrap();
        let parsed = Template::parse(source);
        if case["default"]["phase"] == "parse" || case["identity"]["phase"] == "parse" {
            assert_eq!(
                case["default"]["phase"], "parse",
                "{name}: default parse phase"
            );
            assert_eq!(
                case["identity"]["phase"], "parse",
                "{name}: identity parse phase"
            );
            private_error(parsed.unwrap_err(), name);
            continue;
        }
        let template =
            parsed.unwrap_or_else(|error| panic!("{name}: unexpected parse failure: {error}"));
        let context = decode(&case["context"]);
        let context = context.as_object().unwrap();
        let text = template.render(context);
        if case["default"].get("error").is_some() {
            assert_eq!(
                case["default"]["phase"], "render",
                "{name}: source default failure stage"
            );
            private_error(text.unwrap_err(), name);
        } else {
            assert_eq!(
                text.unwrap_or_else(|error| panic!("{name}: {error}")),
                expected_text(&case["default"]["output"]),
                "{name}: default formatter"
            );
        }
        let identity = template.render_source_value(context);
        if case["identity"].get("error").is_some() {
            assert_eq!(
                case["identity"]["phase"], "render",
                "{name}: source identity failure stage"
            );
            private_error(identity.unwrap_err(), name);
        } else {
            let actual = identity.unwrap_or_else(|error| panic!("{name}: {error}"));
            check_value(&actual, &case["identity"]["output"], name);
            check_offset_payload(&actual, &case["identity"]["output"], name);
        }
    }
    (boundaries, typed)
}
async fn replay_modifies(cases: &[Value]) -> usize {
    let mut boundaries = 0;
    let root = tempfile::tempdir().unwrap();
    let media =
        crate::extract::direct(&"https://fixture.invalid/file.jpg".parse().unwrap()).unwrap();
    for case in cases {
        if case.get("native_representation_boundary").is_some() {
            check_aware_boundary(case);
            boundaries += 1;
            continue;
        }
        let name = case["name"].as_str().unwrap();
        let options = MetadataOptions::parse(case["options"].as_object().unwrap(), root.path());
        if case.get("parse_error").is_some() {
            private_error(options.unwrap_err(), name);
            continue;
        }
        let options = options
            .unwrap_or_else(|error| panic!("{name}: unexpected constructor failure: {error}"));
        let mut context = decode(&case["context"]).as_object().unwrap().clone();
        options
            .run(&mut context, &media, Path::new("file.jpg"), root.path())
            .await
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        let actual = Value::Object(context);
        check_value(&actual, &case["output"], name);
        check_offset_payload(&actual, &case["output"], name);
    }
    boundaries
}

#[test]
fn source_numeric_offset_arguments_distinguish_parse_and_runtime_failures() {
    let oracle = fixture();
    assert_eq!(oracle["cases"].as_array().unwrap().len(), 594);
    assert_eq!(replay_cases(oracle["cases"].as_array().unwrap()), (6, 0));
    let unterminated = oracle["unterminated_cases"].as_array().unwrap();
    assert_eq!(unterminated.len(), 4);
    assert_eq!(replay_cases(unterminated), (0, 0));
}
#[tokio::test]
async fn source_offset_metadata_modify_preserves_typed_copies() {
    let oracle = fixture();
    assert_eq!(oracle["modify_cases"].as_array().unwrap().len(), 100);
    assert_eq!(
        replay_modifies(oracle["modify_cases"].as_array().unwrap()).await,
        2
    );
}
#[tokio::test]
async fn source_offset_jobs_publish_exact_paths_metadata_and_archive_keys() {
    let oracle = fixture();
    let cases = oracle["job_cases"].as_array().unwrap();
    assert_eq!(cases.len(), 4);
    replay_date_jobs(cases, &["shifted"]).await;
}

#[cfg(all(unix, target_pointer_width = "64"))]
const DRIVER: &str = "naming::source_formatter_offset_tests::local_offset_replay_subprocess_driver";
#[cfg(all(unix, target_pointer_width = "64"))]
const ZONE_ENV: &str = "GDL_SOURCE_OFFSET_REPLAY_ZONE";
#[cfg(all(unix, target_pointer_width = "64"))]
#[test]
fn source_local_offsets_run_in_isolated_tz_processes() {
    let oracle = fixture();
    let groups = oracle["local_cases"].as_array().unwrap();
    assert_eq!(groups.len(), 3);
    let parent_tz = std::env::var_os("TZ");
    for group in groups {
        let zone = group["tz"].as_str().unwrap();
        let zone_root = tempfile::tempdir().unwrap();
        let zone_path = zone_root.path().join("source-zone.tzif");
        std::fs::write(&zone_path, source_tzif_bytes(group)).unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", DRIVER, "--nocapture", "--test-threads=1"])
            .env("TZ", format!(":{}", zone_path.display()))
            .env(ZONE_ENV, zone)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "isolated TZ={zone} replay failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            std::env::var_os("TZ"),
            parent_tz,
            "parent TZ remains unchanged"
        );
    }
}
#[cfg(all(unix, target_pointer_width = "64"))]
#[test]
fn local_offset_replay_subprocess_driver() {
    let Ok(zone) = std::env::var(ZONE_ENV) else {
        return;
    };
    let tz = std::env::var("TZ").unwrap();
    let path = Path::new(
        tz.strip_prefix(':')
            .expect("TZ must name captured TZif data"),
    );
    assert!(path.is_absolute() && path.is_file());
    let oracle = fixture();
    let group = oracle["local_cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|group| group["tz"] == zone)
        .unwrap();
    assert_eq!(std::fs::read(path).unwrap(), source_tzif_bytes(group));
    assert_eq!(group["cases"].as_array().unwrap().len(), 90);
    assert_eq!(group["modify_cases"].as_array().unwrap().len(), 22);
    assert_eq!(replay_cases(group["cases"].as_array().unwrap()), (6, 24));
    let unterminated = group["unterminated_cases"].as_array().unwrap();
    assert_eq!(unterminated.len(), 2);
    assert_eq!(replay_cases(unterminated), (0, 0));
    let boundaries = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(replay_modifies(group["modify_cases"].as_array().unwrap()));
    assert_eq!(boundaries, 2);
}

// A timezone-aware UTC datetime and a naive UTC datetime share the same
// OffsetDateTime value. Current epoch-only keyword snapshots do not retain the
// awareness bit; keep these Source-only inputs visible and counted explicitly.
fn check_aware_boundary(case: &Value) {
    let value = &case["context"]["entries"]["value"];
    assert_eq!(value["kind"], "datetime");
    assert!(value["iso"].as_str().unwrap().ends_with("+00:00"));
    assert!(value["epoch"].is_null());
    assert_eq!(value["epoch_error"], "TypeError");
    assert_eq!(value["offset_seconds"].as_f64(), Some(0.0));
    assert_eq!(value["truthy"], true);
    assert!(
        case["native_representation_boundary"]
            .as_str()
            .unwrap()
            .contains("aware")
    );
}

fn replay_typed_case(case: &Value) {
    let name = case["name"].as_str().unwrap();
    let source = case["template"].as_str().unwrap();
    let super::format::Value::Map(values) = decode_field(&case["context"]) else {
        panic!("{name}: context must be map");
    };
    let context = values
        .into_iter()
        .collect::<std::collections::BTreeMap<_, _>>();
    let template = Template::parse(source).unwrap_or_else(|error| panic!("{name}: {error}"));
    let text = template.render_format_context(&context);
    if case["default"].get("error").is_some() {
        assert_eq!(case["default"]["phase"], "render");
        private_error(text.unwrap_err(), name);
    } else {
        assert_eq!(
            text.unwrap_or_else(|error| panic!("{name}: {error}")),
            expected_text(&case["default"]["output"]),
            "{name}: raw subsecond default formatter"
        );
    }
    // These fixture templates consist of one field. Inspect its actual typed
    // result before serializing through the whole-second runtime JSON wrapper.
    assert!(source.starts_with('{') && source.ends_with('}') && source.matches('{').count() == 1);
    let field = super::format::Field::parse(&source[1..source.len() - 1], |_| true).unwrap();
    let result = field.render_value(|key| context.get(key).cloned());
    if case["identity"].get("error").is_some() {
        assert_eq!(case["identity"]["phase"], "render");
        private_error(result.unwrap_err(), name);
        return;
    }
    let expected = &case["identity"]["output"];
    let actual = result.unwrap_or_else(|error| panic!("{name}: {error}"));
    match expected["kind"].as_str().unwrap() {
        "datetime" => {
            let date = match &actual {
                super::format::Value::Date(date) | super::format::Value::SourceDate(date) => date,
                value => panic!("{name}: expected datetime, got {value:?}"),
            };
            assert_eq!(
                date.microsecond(),
                expected["microsecond"].as_u64().unwrap() as u32,
                "{name}: preserve actual input fraction"
            );
            assert_eq!(
                actual.identity_text().unwrap(),
                expected["text"],
                "{name}: typed shifted date retains wall clock and fraction"
            );
            let json = super::format::Field::parse("probe!j", |_| true)
                .unwrap()
                .render_value(|_| Some(actual.clone()))
                .unwrap();
            assert_eq!(
                json,
                super::format::Value::Str(expected["public_json"].as_str().unwrap().into()),
                "{name}: raw typed public serialization"
            );
        }
        "value" => assert_eq!(
            actual,
            super::format::Value::Str(expected["value"].as_str().unwrap().into()),
            "{name}: typed context formatter result"
        ),
        kind => panic!("{name}: unexpected raw typed output {kind}"),
    }
}

#[cfg(not(all(unix, target_pointer_width = "64")))]
#[test]
fn historical_iana_local_offset_vectors_have_an_explicit_platform_boundary() {
    let oracle = fixture();
    let groups = oracle["local_cases"].as_array().unwrap();
    assert_eq!(groups.len(), 3);
    for group in groups {
        assert!(!group["platform"].as_str().unwrap().is_empty());
        assert_eq!(group["cases"].as_array().unwrap().len(), 90);
        assert_eq!(group["modify_cases"].as_array().unwrap().len(), 22);
    }
}

#[cfg(all(unix, target_pointer_width = "64"))]
fn source_tzif_bytes(group: &Value) -> Vec<u8> {
    use base64::Engine;
    let provenance = &group["tzif"];
    assert_eq!(provenance["verified_all_observations_equal"], true);
    assert_eq!(provenance["verified_tz_template"], ":$TZIF");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(provenance["base64"].as_str().unwrap())
        .unwrap();
    assert_eq!(
        bytes.len() as u64,
        provenance["byte_length"].as_u64().unwrap()
    );
    assert!(bytes.starts_with(b"TZif"));
    assert_eq!(
        format!("{:x}", <sha2::Sha256 as sha2::Digest>::digest(&bytes)),
        provenance["sha256"],
        "captured timezone bytes match Source provenance"
    );
    bytes
}
