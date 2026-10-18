#![cfg(unix)]

use serde_json::{Value, json};
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Output, Stdio},
    time::Duration,
};
use tokio::{process::Command, time::timeout};

const FIRST: &str = "https://x.com/person/status/1";
const SECOND: &str = "https://x.com/person/status/2";

fn row(name: &str) -> Value {
    json!([3, format!("https://cdn.example/{name}.jpg"), {"filename":name,"extension":"jpg"}])
}

fn fixture(root: &Path, first: &[Value], second: &[Value]) -> PathBuf {
    std::fs::write(
        root.join("backend.first"),
        serde_json::to_vec(first).unwrap(),
    )
    .unwrap();
    std::fs::write(
        root.join("backend.second"),
        serde_json::to_vec(second).unwrap(),
    )
    .unwrap();
    script(
        root,
        "printf '%s\\n' invoked >> \"$0.log\"\nfor source do :; done\ncase \"$source\" in\n*/status/2) cat \"$0.second\";;\n*) cat \"$0.first\";;\nesac",
    )
}

fn script(root: &Path, body: &str) -> PathBuf {
    let executable = root.join("backend");
    std::fs::write(&executable, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    executable
}

fn binary(root: &Path, executable: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_gallery-dl-rs"));
    command
        .current_dir(root)
        .kill_on_drop(true)
        .arg("--extractor-command")
        .arg(executable);
    command
}

async fn output(command: &mut Command) -> Output {
    timeout(Duration::from_secs(8), command.output())
        .await
        .expect("CLI did not finish within the fixture deadline")
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

#[tokio::test]
async fn unions_and_slices_reset_for_each_input_and_preserve_source_order() {
    let root = tempfile::tempdir().unwrap();
    let first: Vec<_> = (1..=6).map(|n| row(&format!("a{n}"))).collect();
    let second: Vec<_> = (1..=6).map(|n| row(&format!("b{n}"))).collect();
    let backend = fixture(root.path(), &first, &second);
    for (range, positions) in [("4,1-2", vec![1, 2, 4]), ("1:6:2", vec![1, 3, 5])] {
        let result =
            output(binary(root.path(), &backend).args(["-g", "--range", range, FIRST, SECOND]))
                .await;
        success(&result);
        let expected: Vec<_> = ["a", "b"]
            .into_iter()
            .flat_map(|prefix| {
                positions
                    .iter()
                    .map(move |n| format!("https://cdn.example/{prefix}{n}.jpg"))
            })
            .collect();
        assert_eq!(urls(&result), expected, "range {range}");
    }
}

#[tokio::test]
async fn duplicate_media_occupy_positions_before_global_deduplication() {
    let root = tempfile::tempdir().unwrap();
    let backend = fixture(
        root.path(),
        &[row("a"), row("a"), row("b")],
        &[row("a"), row("c"), row("d")],
    );
    let result =
        output(binary(root.path(), &backend).args(["-g", "--range", "2-3", FIRST, SECOND])).await;
    success(&result);
    assert_eq!(
        urls(&result),
        ["a", "b", "c", "d"].map(|s| format!("https://cdn.example/{s}.jpg"))
    );
}

#[tokio::test]
async fn empty_category_range_never_starts_backend_and_later_inputs_still_work() {
    let root = tempfile::tempdir().unwrap();
    let backend = script(root.path(), "printf '%s' invoked > \"$0.log\"\nexit 9");
    let config = root.path().join("config.json");
    std::fs::write(
        &config,
        br#"{"extractor":{"twitter":{"file-range":"0"},"direct":{"file-range":"1"}}}"#,
    )
    .unwrap();
    let result = output(
        binary(root.path(), &backend)
            .arg("--native-config")
            .arg(config)
            .args(["-g", FIRST, "https://example.com/later.jpg"]),
    )
    .await;
    success(&result);
    assert_eq!(urls(&result), ["https://example.com/later.jpg"]);
    assert!(!root.path().join("backend.log").exists());
}

fn terminated(pid: &str) -> bool {
    let result = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", pid])
        .stderr(Stdio::null())
        .output()
        .unwrap();
    let state = String::from_utf8_lossy(&result.stdout);
    state.trim().is_empty() || state.trim().starts_with('Z')
}

#[tokio::test]
async fn finite_range_completes_and_terminates_backend_without_outer_array_end() {
    let root = tempfile::tempdir().unwrap();
    let body = format!(
        "printf '%s' \"$$\" > \"$0.pid\"\nprintf '%s' '[{}'\nwhile :; do sleep 0.02; done",
        row("first")
    );
    let backend = script(root.path(), &body);
    let result = output(binary(root.path(), &backend).args(["-g", "--range", "1", FIRST])).await;
    success(&result);
    assert_eq!(urls(&result), ["https://cdn.example/first.jpg"]);
    let pid = std::fs::read_to_string(root.path().join("backend.pid")).unwrap();
    timeout(Duration::from_secs(5), async {
        while !terminated(&pid) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("selected input's backend remained alive");
}

#[tokio::test]
async fn finite_range_ignores_unconsumed_invalid_row_but_errors_before_end_continue_inputs() {
    let root = tempfile::tempdir().unwrap();
    let backend = fixture(
        root.path(),
        &[row("first"), json!([3, "invalid", {}])],
        &[row("later1"), row("later2")],
    );
    let result =
        output(binary(root.path(), &backend).args(["-g", "--range", "1", FIRST, SECOND])).await;
    success(&result);
    assert_eq!(
        urls(&result),
        [
            "https://cdn.example/first.jpg",
            "https://cdn.example/later1.jpg"
        ]
    );
    let result =
        output(binary(root.path(), &backend).args(["-g", "--range", "2", FIRST, SECOND])).await;
    assert_eq!(result.status.code(), Some(1));
    assert_eq!(urls(&result), ["https://cdn.example/later2.jpg"]);
    assert!(String::from_utf8_lossy(&result.stderr).contains("1 operation(s) failed"));
}

#[tokio::test]
async fn only_selected_existing_media_receive_metadata_and_archive_records() {
    let root = tempfile::tempdir().unwrap();
    let rows: Vec<_> = (1..=3).map(|n| json!([3,format!("https://cdn.example/{n}.jpg"),{
        "filename": format!("image{n}"), "extension":"jpg", "post_id":"100", "media_id":n.to_string(), "num":n
    }])).collect();
    let backend = fixture(root.path(), &rows, &[]);
    let saved = root.path().join("saved");
    std::fs::create_dir(&saved).unwrap();
    for n in 1..=3 {
        std::fs::write(saved.join(format!("image{n}.jpg")), b"existing").unwrap();
    }
    let archive = root.path().join("history.sqlite");
    let result = output(
        binary(root.path(), &backend)
            .arg("-D")
            .arg(&saved)
            .args(["-f", "{filename}", "--range", "2", "--write-metadata"])
            .arg("--download-archive")
            .arg(&archive)
            .arg("https://instagram.com/p/test/"),
    )
    .await;
    success(&result);
    for n in 1..=3 {
        assert_eq!(
            std::fs::read(saved.join(format!("image{n}.jpg"))).unwrap(),
            b"existing"
        );
        assert_eq!(saved.join(format!("image{n}.jpg.json")).exists(), n == 2);
    }
    let expected =
        gallery_dl_rs::extract::parse_messages(&serde_json::to_vec(&rows).unwrap(), "instagram")
            .unwrap()[1]
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
async fn category_global_and_explicit_cli_file_ranges_follow_config_precedence() {
    let root = tempfile::tempdir().unwrap();
    let backend = fixture(root.path(), &[row("a1"), row("a2"), row("a3")], &[]);
    let config = root.path().join("config.json");
    std::fs::write(
        &config,
        br#"{"extractor":{"file-range":"2","twitter":{"file-range":"3"}}}"#,
    )
    .unwrap();
    let result = output(
        binary(root.path(), &backend)
            .arg("--native-config")
            .arg(&config)
            .args(["-g", FIRST, "https://example.com/direct.jpg"]),
    )
    .await;
    success(&result);
    assert_eq!(urls(&result), ["https://cdn.example/a3.jpg"]);
    std::fs::write(
        &config,
        br#"{"file-range":"2","extractor":{"file-range":"1","twitter":{"file-range":"3"}}}"#,
    )
    .unwrap();
    let result = output(
        binary(root.path(), &backend)
            .arg("--native-config")
            .arg(&config)
            .args(["-g", FIRST, "https://example.com/direct.jpg"]),
    )
    .await;
    success(&result);
    assert_eq!(urls(&result), ["https://cdn.example/a2.jpg"]);
    let result = output(
        binary(root.path(), &backend)
            .arg("--native-config")
            .arg(&config)
            .args([
                "-g",
                "--range",
                "1",
                FIRST,
                "https://example.com/direct.jpg",
            ]),
    )
    .await;
    success(&result);
    assert_eq!(
        urls(&result),
        [
            "https://cdn.example/a1.jpg",
            "https://example.com/direct.jpg"
        ]
    );
}
