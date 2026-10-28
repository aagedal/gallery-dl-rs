#![cfg(unix)]

use serde_json::{Value, json};
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Output,
    time::Duration,
};
use tokio::{process::Command, time::timeout};

const FIRST: &str = "https://x.com/person/status/1";
const SECOND: &str = "https://x.com/person/status/2";

fn row(name: &str, width: Option<u64>, extension: &str, index: usize) -> Value {
    let mut metadata = json!({"filename":name,"extension":extension,"tweet_id":"100","post_id":"100","media_id":index.to_string(),"num":index,"height":5,"type":"photo","date":10});
    if let Some(width) = width {
        metadata["width"] = json!(width);
    }
    json!([
        3,
        format!("https://cdn.example/{name}.{extension}"),
        metadata
    ])
}
fn script(root: &Path, body: &str) -> PathBuf {
    let backend = root.join("backend");
    std::fs::write(&backend, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&backend, std::fs::Permissions::from_mode(0o700)).unwrap();
    backend
}
fn fixture(root: &Path, first: &[Value], second: &[Value]) -> PathBuf {
    for (name, rows) in [("first", first), ("second", second)] {
        std::fs::write(
            root.join(format!("backend.{name}")),
            serde_json::to_vec(rows).unwrap(),
        )
        .unwrap();
    }
    script(
        root,
        "printf '%s\\n' \"$@\" >> \"$0.arguments\"\nprintf '%s\\n' invoked >> \"$0.log\"\nfor source do :; done\ncase \"$source\" in\n*/status/2) cat \"$0.second\";;\n*) cat \"$0.first\";;\nesac",
    )
}
fn binary(root: &Path, backend: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_gallery-dl-rs"));
    command
        .current_dir(root)
        .kill_on_drop(true)
        .arg("--extractor-command")
        .arg(backend);
    command
}
async fn output(command: &mut Command) -> Output {
    timeout(Duration::from_secs(8), command.output())
        .await
        .expect("filtered CLI fixture timed out")
        .unwrap()
}
fn success(result: &Output) {
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}
fn urls(result: &Output) -> Vec<String> {
    String::from_utf8(result.stdout.clone())
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}
fn url(name: &str) -> String {
    format!("https://cdn.example/{name}.jpg")
}

#[tokio::test]
async fn inspections_filter_metadata_before_ranges_and_repeat_expressions_with_and() {
    let root = tempfile::tempdir().unwrap();
    let rows = |prefix: &str| {
        vec![
            row(&format!("{prefix}-small"), Some(500), "jpg", 1),
            row(&format!("{prefix}-png"), Some(1500), "png", 2),
            row(&format!("{prefix}-first"), Some(2000), "jpg", 3),
            row(&format!("{prefix}-second"), Some(2500), "jpg", 4),
        ]
    };
    let backend = fixture(root.path(), &rows("a"), &rows("b"));
    let result = output(binary(root.path(), &backend).args([
        "-j",
        "--filter",
        "width >= 1000 and extension in ['jpg', 'png']",
        FIRST,
    ]))
    .await;
    success(&result);
    let items: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(
        items
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["filename"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["a-png.png", "a-first.jpg", "a-second.jpg"]
    );
    assert_eq!(items[0]["metadata"]["file"]["width"], 1500);
    let result = output(binary(root.path(), &backend).args([
        "-g",
        "--filter",
        "image_width >= 1000",
        "--filter",
        "extension == 'jpg'",
        "--range",
        "2",
        FIRST,
        SECOND,
    ]))
    .await;
    success(&result);
    assert_eq!(urls(&result), [url("a-second"), url("b-second")]);
    let args = std::fs::read_to_string(root.path().join("backend.arguments")).unwrap();
    assert!(!args.contains("--filter"));
    assert!(!args.contains("width >="));
}

#[tokio::test]
async fn filter_configuration_aliases_scopes_clear_and_cli_precedence_are_applied_in_rust() {
    let root = tempfile::tempdir().unwrap();
    let backend = fixture(
        root.path(),
        &[
            row("small", Some(500), "jpg", 1),
            row("medium", Some(1500), "jpg", 2),
            row("large", Some(2500), "jpg", 3),
        ],
        &[],
    );
    let config = root.path().join("native.json");
    for (settings, extra, expected) in [
        (
            json!({"extractor":{"file-filter":"width > 1000","twitter":{"image-filter":"width > 2000"}}}),
            vec![],
            vec![url("large")],
        ),
        (
            json!({"file-filter":"width < 1000","extractor":{"twitter":{"image-filter":"width > 2000"}}}),
            vec![],
            vec![url("small")],
        ),
        (
            json!({"file-filter":[],"extractor":{"file-filter":"width > 1000"}}),
            vec![],
            vec![url("small"), url("medium"), url("large")],
        ),
        (
            json!({"file-filter":"","image-filter":"width == 1500"}),
            vec![],
            vec![url("medium")],
        ),
        (
            json!({"file-filter":"width > 2000"}),
            vec!["--filter", "width < 1000"],
            vec![url("small")],
        ),
    ] {
        std::fs::write(&config, serde_json::to_vec(&settings).unwrap()).unwrap();
        let result = output(
            binary(root.path(), &backend)
                .arg("--native-config")
                .arg(&config)
                .arg("-g")
                .args(extra)
                .arg(FIRST),
        )
        .await;
        success(&result);
        assert_eq!(urls(&result), expected);
    }
}

#[tokio::test]
async fn invalid_filter_syntax_fails_privately_before_backend_archive_or_destination_creation() {
    let root = tempfile::tempdir().unwrap();
    let backend = script(root.path(), "printf invoked > \"$0.log\"\nexit 9");
    let saved = root.path().join("not-created");
    let archive = root.path().join("not-created.sqlite");
    for expression in [
        "PRIVATE-FILTER-VALUE + )",
        "__import__('PRIVATE-FILTER-VALUE')",
    ] {
        let result = output(
            binary(root.path(), &backend)
                .arg("-D")
                .arg(&saved)
                .arg("--download-archive")
                .arg(&archive)
                .args(["--filter", expression, FIRST]),
        )
        .await;
        assert!(!result.status.success());
        assert!(result.stdout.is_empty());
        assert!(!String::from_utf8_lossy(&result.stderr).contains("PRIVATE-FILTER-VALUE"));
        assert!(!saved.exists());
        assert!(!archive.exists());
        assert!(!root.path().join("backend.log").exists());
    }
}

#[tokio::test]
async fn missing_fields_and_runtime_errors_reject_only_the_current_file_and_errors_pass_through() {
    let root = tempfile::tempdir().unwrap();
    let mut zero = row("zero-height", Some(2000), "jpg", 2);
    zero[2]["height"] = json!(0);
    let backend = fixture(
        root.path(),
        &[
            row("missing-width", None, "jpg", 1),
            zero,
            row("selected", Some(2000), "jpg", 3),
            json!([3, "invalid", {}]),
        ],
        &[row("next-input", Some(2000), "jpg", 1)],
    );
    let result = output(binary(root.path(), &backend).args([
        "-g",
        "--filter",
        "width >= 1000 and 10 / height > 1",
        FIRST,
        SECOND,
    ]))
    .await;
    assert_eq!(result.status.code(), Some(1));
    assert_eq!(urls(&result), [url("selected"), url("next-input")]);
    assert!(String::from_utf8_lossy(&result.stderr).contains("1 operation(s) failed"));
}

#[tokio::test]
async fn only_filtered_and_ranged_existing_media_receive_sidecars_and_archive_records() {
    let root = tempfile::tempdir().unwrap();
    let rows = [
        row("small", Some(500), "jpg", 1),
        row("first-large", Some(1500), "jpg", 2),
        row("selected", Some(2500), "jpg", 3),
    ];
    let backend = fixture(root.path(), &rows, &[]);
    let saved = root.path().join("saved");
    std::fs::create_dir(&saved).unwrap();
    for name in ["small", "first-large", "selected"] {
        std::fs::write(saved.join(format!("{name}.jpg")), b"existing").unwrap();
    }
    let archive = root.path().join("history.sqlite");
    let result = output(
        binary(root.path(), &backend)
            .arg("-D")
            .arg(&saved)
            .arg("--download-archive")
            .arg(&archive)
            .args([
                "-f",
                "{filename}.{extension}",
                "--write-metadata",
                "--filter",
                "width >= 1000",
                "--range",
                "2",
                FIRST,
            ]),
    )
    .await;
    success(&result);
    for name in ["small", "first-large", "selected"] {
        assert_eq!(
            std::fs::read(saved.join(format!("{name}.jpg"))).unwrap(),
            b"existing"
        );
        assert_eq!(
            saved.join(format!("{name}.jpg.json")).exists(),
            name == "selected"
        );
    }
    let expected =
        gallery_dl_rs::extract::parse_messages(&serde_json::to_vec(&rows).unwrap(), "twitter")
            .unwrap()[2]
            .archive_key()
            .unwrap();
    let connection = rusqlite::Connection::open(archive).unwrap();
    let records: Vec<String> = connection
        .prepare("SELECT identity FROM completed_media")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(records, [expected]);
}

#[tokio::test]
async fn finite_filtered_range_drops_unfinished_backend_before_later_rows() {
    let root = tempfile::tempdir().unwrap();
    let backend = script(
        root.path(),
        &format!(
            "printf '%s' \"$$\" > \"$0.pid\"\nprintf '%s' '[{},{}'\nwhile :; do sleep 0.02; done",
            row("rejected", Some(500), "jpg", 1),
            row("selected", Some(2000), "jpg", 2)
        ),
    );
    let result = output(binary(root.path(), &backend).args([
        "-g",
        "--filter",
        "width >= 1000",
        "--range",
        "1",
        FIRST,
    ]))
    .await;
    success(&result);
    assert_eq!(urls(&result), [url("selected")]);
    let pid = std::fs::read_to_string(root.path().join("backend.pid")).unwrap();
    timeout(Duration::from_secs(5), async {
        loop {
            let live = std::process::Command::new("kill")
                .args(["-0", pid.trim()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap()
                .success();
            if !live {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("filtered range backend remained alive");
}
