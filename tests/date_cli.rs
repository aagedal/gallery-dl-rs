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

fn row(name: &str, date: Value) -> Value {
    json!([3, format!("https://cdn.example/{name}.jpg"), {
        "filename": name, "extension":"jpg", "date":date,
        "post_id":"100", "media_id":"1", "num":1
    }])
}

fn script(root: &Path, body: &str) -> PathBuf {
    let executable = root.join("backend");
    std::fs::write(&executable, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    executable
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
        "printf '%s\\n' invoked >> \"$0.log\"\nfor source do :; done\ncase \"$source\" in\n*/status/2) cat \"$0.second\";;\n*) cat \"$0.first\";;\nesac",
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
        .expect("CLI fixture deadline exceeded")
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
async fn source_order_dates_skip_old_entries_in_stories_reels_tagged_and_saved_feeds() {
    let root = tempfile::tempdir().unwrap();
    let backend = fixture(
        root.path(),
        &[
            row("old", json!(4)),
            row("after-boundary", json!(5)),
            row("ascending", json!(6)),
            row("before-boundary", json!(10)),
            row("too-new", json!(11)),
            row("mixed", json!(7)),
            row("unknown", Value::Null),
            row("older-again", json!(1)),
            row("later", json!(9)),
        ],
        &[],
    );
    for source in [
        "https://instagram.com/stories/fixture/",
        "https://instagram.com/stories/highlights/123/",
        "https://instagram.com/fixture/reels/",
        "https://instagram.com/fixture/tagged/",
        "https://instagram.com/fixture/saved/",
        "https://instagram.com/fixture/saved/collection/123/",
        "https://x.com/fixture",
        "https://x.com/id:42",
        "https://x.com/search?q=rust&f=live",
        "https://x.com/hashtag/rust",
        "https://x.com/fixture/media",
        "https://x.com/fixture/tweets",
        "https://x.com/fixture/with_replies",
        "https://x.com/fixture/highlights",
        "https://x.com/person/likes",
        "https://x.com/id:42/likes",
        "https://x.com/i/bookmarks",
        "https://x.com/i/lists/42",
        "https://x.com/home",
        "https://x.com/home/following",
        "https://x.com/home/foryou",
        "https://x.com/home/for-you",
        "https://x.com/home/for_you",
        "https://x.com/id:42/tweets",
        "https://x.com/id:42/with_replies",
        "https://x.com/id:42/highlights",
    ] {
        let result = output(binary(root.path(), &backend).args([
            "-g",
            "--date-after=5",
            "--date-before=10",
            source,
        ]))
        .await;
        success(&result);
        assert_eq!(
            urls(&result),
            [
                "https://cdn.example/ascending.jpg",
                "https://cdn.example/mixed.jpg",
                "https://cdn.example/unknown.jpg",
                "https://cdn.example/later.jpg",
            ]
        );
    }
}

#[tokio::test]
async fn quoted_single_status_dates_skip_old_root_and_exhaust_later_quotes() {
    let root = tempfile::tempdir().unwrap();
    let backend = script(
        root.path(),
        &format!(
            "printf '%s' '[{},'\nsleep 0.1\nprintf '%s' '{},{},{}'\nprintf '%s' exhausted > \"$0.exhausted\"\nprintf '%s' ']'",
            row("old-root", json!(4)),
            row("new-quote", json!(6)),
            row("old-quote", json!(3)),
            row("later-quote", json!(7)),
        ),
    );
    let config = root.path().join("native.json");
    std::fs::write(
        &config,
        serde_json::to_vec(&json!({"extractor":{"twitter":{"quoted":true}}})).unwrap(),
    )
    .unwrap();
    let result = output(
        binary(root.path(), &backend)
            .arg("--native-config")
            .arg(&config)
            .args(["-g", "--date-after=5", FIRST]),
    )
    .await;
    success(&result);
    assert_eq!(
        urls(&result),
        [
            "https://cdn.example/new-quote.jpg",
            "https://cdn.example/later-quote.jpg",
        ]
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("backend.exhausted")).unwrap(),
        "exhausted"
    );
}

#[tokio::test]
async fn strict_timestamp_boundaries_include_zero_and_negative_dates() {
    let root = tempfile::tempdir().unwrap();
    let rows: Vec<_> = (-3..=2)
        .rev()
        .map(|date| row(&format!("date{date}"), json!(date)))
        .collect();
    let backend = fixture(root.path(), &rows, &[]);
    let result = output(binary(root.path(), &backend).args([
        "-g",
        "--date-before=1",
        "--date-after=-2",
        FIRST,
    ]))
    .await;
    success(&result);
    assert_eq!(
        urls(&result),
        [
            "https://cdn.example/date0.jpg",
            "https://cdn.example/date-1.jpg"
        ]
    );
}

#[tokio::test]
async fn date_flags_use_source_iso_first_numeric_fallback_and_invalid_clearing() {
    let root = tempfile::tempdir().unwrap();
    let backend = fixture(
        root.path(),
        &[
            row("later", json!(1704240000)),
            row("boundary", json!(1704153600)),
            row("earlier", json!(1704110400)),
            row("epoch", json!(0)),
        ],
        &[],
    );
    for (bound, expected) in [
        ("20240102", vec!["earlier", "epoch"]),
        (" 20240102 ", vec!["epoch"]),
        ("2024-W01-2", vec!["earlier", "epoch"]),
        ("2024-01-01T24", vec!["earlier", "epoch"]),
        ("2024-01-02😀00:00:00", vec!["earlier", "epoch"]),
        ("2024-01-02T00:00:00+00:00:00.5", vec!["earlier", "epoch"]),
        ("2024-01-02 ", vec!["later", "boundary", "earlier", "epoch"]),
        (
            "PRIVATE_INVALID_BOUND",
            vec!["later", "boundary", "earlier", "epoch"],
        ),
        ("１２３", vec!["epoch"]),
    ] {
        let result =
            output(binary(root.path(), &backend).args(["-g", "--date-before", bound, FIRST])).await;
        success(&result);
        assert_eq!(
            urls(&result),
            expected
                .into_iter()
                .map(|name| format!("https://cdn.example/{name}.jpg"))
                .collect::<Vec<_>>(),
            "{bound:?}"
        );
        assert!(!String::from_utf8_lossy(&result.stderr).contains("PRIVATE_INVALID_BOUND"));
    }
}

#[tokio::test]
async fn before_filter_precedes_range_and_unknown_dates_remain_in_source_order() {
    let root = tempfile::tempdir().unwrap();
    let backend = fixture(
        root.path(),
        &[
            row("too-new", json!(20)),
            row("boundary", json!(10)),
            row("eligible", json!(9)),
            row("unknown", Value::Null),
            row("invalid-date", json!("2024-02-30 12:00:00")),
            row("older", json!(1)),
        ],
        &[],
    );
    let result = output(binary(root.path(), &backend).args([
        "-g",
        "--date-before=10",
        "--range",
        "2-3",
        FIRST,
    ]))
    .await;
    success(&result);
    assert_eq!(
        urls(&result),
        [
            "https://cdn.example/unknown.jpg",
            "https://cdn.example/invalid-date.jpg"
        ]
    );
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
async fn after_boundary_terminates_unfinished_backend_and_resets_for_next_input() {
    let root = tempfile::tempdir().unwrap();
    let body = format!(
        "for source do :; done\ncase \"$source\" in\n*/status/2) printf '%s' '[{}]' ;;\n*) printf '%s' \"$$\" > \"$0.pid\"\nprintf '%s' '[{},{},[3,\"invalid\",{{}}]'\nwhile :; do sleep 0.02; done ;;\nesac",
        row("later", json!(11)),
        row("first", json!(11)),
        row("boundary", json!(10))
    );
    let backend = script(root.path(), &body);
    let result =
        output(binary(root.path(), &backend).args(["-g", "--date-after=10", FIRST, SECOND])).await;
    success(&result);
    assert_eq!(
        urls(&result),
        [
            "https://cdn.example/first.jpg",
            "https://cdn.example/later.jpg"
        ]
    );
    let pid = std::fs::read_to_string(root.path().join("backend.pid")).unwrap();
    timeout(Duration::from_secs(5), async {
        while !terminated(&pid) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("date-limited backend remained alive");

    let backend = fixture(
        root.path(),
        &[
            row("before-error", json!(11)),
            json!([3, "invalid", {}]),
            row("boundary", json!(10)),
        ],
        &[row("later", json!(11))],
    );
    let result =
        output(binary(root.path(), &backend).args(["-g", "--date-after=10", FIRST, SECOND])).await;
    assert_eq!(result.status.code(), Some(1));
    assert_eq!(
        urls(&result),
        [
            "https://cdn.example/before-error.jpg",
            "https://cdn.example/later.jpg"
        ]
    );
    assert!(String::from_utf8_lossy(&result.stderr).contains("1 operation(s) failed"));
}

#[tokio::test]
async fn json_normalizes_backend_datetime_and_leaves_unknown_dates_empty() {
    let root = tempfile::tempdir().unwrap();
    let backend = fixture(
        root.path(),
        &[
            row("formatted", json!("1970-01-01 00:00:01")),
            row("epoch", json!(0)),
            row("unknown", Value::Null),
            row("invalid", json!("not a date")),
        ],
        &[],
    );
    let result = output(binary(root.path(), &backend).args(["-j", "--date-before=2", FIRST])).await;
    success(&result);
    let items: Value = serde_json::from_slice(&result.stdout).unwrap();
    let timestamps: Vec<_> = items
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["metadata"]["timestamp"].clone())
        .collect();
    assert_eq!(timestamps, [json!(1), json!(0), Value::Null, Value::Null]);
}

#[tokio::test]
async fn filtered_downloads_create_neither_metadata_nor_archive_records() {
    let root = tempfile::tempdir().unwrap();
    let mut rows = [
        row("new", json!(3)),
        row("selected", json!(2)),
        row("old", json!(1)),
    ];
    for (index, row) in rows.iter_mut().enumerate() {
        row[2]["media_id"] = json!((index + 1).to_string());
        row[2]["num"] = json!(index + 1);
    }
    let backend = fixture(root.path(), &rows, &[]);
    let saved = root.path().join("saved");
    std::fs::create_dir(&saved).unwrap();
    std::fs::write(saved.join("selected.jpg"), b"existing").unwrap();
    let archive = root.path().join("history.sqlite");
    let result = output(
        binary(root.path(), &backend)
            .arg("-D")
            .arg(&saved)
            .args([
                "-f",
                "{filename}.{extension}",
                "--date-before=3",
                "--date-after=1",
                "--write-metadata",
            ])
            .arg("--download-archive")
            .arg(&archive)
            .arg("https://instagram.com/p/test/"),
    )
    .await;
    success(&result);
    assert_eq!(
        std::fs::read(saved.join("selected.jpg")).unwrap(),
        b"existing"
    );
    for name in ["new", "old"] {
        assert!(!saved.join(format!("{name}.jpg")).exists());
        assert!(!saved.join(format!("{name}.jpg.json")).exists());
    }
    assert!(saved.join("selected.jpg.json").exists());
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
async fn date_configuration_preserves_source_bounds_precedence_and_private_schema_errors() {
    let root = tempfile::tempdir().unwrap();
    let backend = fixture(
        root.path(),
        &[
            row("three", json!(3)),
            row("two", json!(2)),
            row("one", json!(1)),
        ],
        &[],
    );
    let config = root.path().join("private-config.json");
    for (contents, extra, expected) in [
        (
            json!({"extractor":{"date-before":"3", "twitter":{"date-before":"2"}}}),
            vec![],
            vec!["one"],
        ),
        (
            json!({"date-before":"3", "extractor":{"date-before":"2", "twitter":{"date-before":"1"}}}),
            vec![],
            vec!["two", "one"],
        ),
        (
            json!({"date-before":"1"}),
            vec!["--date-before=3"],
            vec!["two", "one"],
        ),
        (
            json!({"date-before":"1970-01-01T01:00:03+01:00"}),
            vec![],
            vec!["two", "one"],
        ),
        (
            json!({"date-before":"PRIVATE_INVALID_DATE"}),
            vec![],
            vec!["three", "two", "one"],
        ),
        (
            json!({"date-before":null,"extractor":{"date-before":"2"}}),
            vec![],
            vec!["three", "two", "one"],
        ),
        (json!({"date-before":"2","date-after":"2"}), vec![], vec![]),
        (
            json!({"date-before":"1","date-after":"2"}),
            vec![],
            vec!["three"],
        ),
    ] {
        std::fs::write(&config, serde_json::to_vec(&contents).unwrap()).unwrap();
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
        assert_eq!(
            urls(&result),
            expected
                .iter()
                .map(|name| format!("https://cdn.example/{name}.jpg"))
                .collect::<Vec<_>>()
        );
    }
    std::fs::remove_file(root.path().join("backend.log")).unwrap();
    for contents in [
        json!({"date-before":"2024-02-30", "private":"PRIVATE-COOKIE-DETAIL"}),
        json!({"date-before":"2024-01-01T12:00:00+24:00", "private":"PRIVATE-COOKIE-DETAIL"}),
        json!({"date-before":"10", "date-after":"10", "private":"PRIVATE-COOKIE-DETAIL"}),
    ] {
        std::fs::write(&config, serde_json::to_vec(&contents).unwrap()).unwrap();
        let result = output(
            binary(root.path(), &backend)
                .arg("--native-config")
                .arg(&config)
                .args(["-g", FIRST]),
        )
        .await;
        assert!(!result.status.success());
        assert!(result.stdout.is_empty());
        assert!(!String::from_utf8_lossy(&result.stderr).contains("PRIVATE-COOKIE-DETAIL"));
        assert!(!root.path().join("backend.log").exists());
    }
}

#[tokio::test]
async fn conversations_keep_qualified_context_after_old_focal_and_reset_for_next_input() {
    let root = tempfile::tempdir().unwrap();
    let backend = fixture(
        root.path(),
        &[
            row("old-focal", json!(4)),
            row("context", json!(6)),
            row("old-reply", json!(3)),
            row("later-reply", json!(7)),
        ],
        &[row("next-input", json!(8))],
    );
    let config = root.path().join("native.json");
    for conversations in [json!(true), json!("accessible")] {
        std::fs::write(
            &config,
            serde_json::to_vec(&json!({"extractor":{"twitter":{"conversations":conversations}}}))
                .unwrap(),
        )
        .unwrap();
        let result = output(
            binary(root.path(), &backend)
                .arg("--native-config")
                .arg(&config)
                .args(["-g", "--date-after=5", "--date-before=8", FIRST, SECOND]),
        )
        .await;
        success(&result);
        assert_eq!(
            urls(&result),
            [
                "https://cdn.example/context.jpg",
                "https://cdn.example/later-reply.jpg"
            ]
        );
        let result = output(
            binary(root.path(), &backend)
                .arg("--native-config")
                .arg(&config)
                .args(["-g", "--date-after=5", FIRST, SECOND]),
        )
        .await;
        success(&result);
        assert_eq!(
            urls(&result),
            [
                "https://cdn.example/context.jpg",
                "https://cdn.example/later-reply.jpg",
                "https://cdn.example/next-input.jpg"
            ]
        );
    }
    // Explicit false keeps the existing descending single-status cutoff.
    std::fs::write(
        &config,
        br#"{"conversations":false,"extractor":{"twitter":{"conversations":true}}}"#,
    )
    .unwrap();
    let result = output(
        binary(root.path(), &backend)
            .arg("--native-config")
            .arg(&config)
            .args(["-g", "--date-after=5", FIRST, SECOND]),
    )
    .await;
    success(&result);
    assert_eq!(urls(&result), ["https://cdn.example/next-input.jpg"]);
}

#[tokio::test]
async fn expanded_single_status_keeps_recent_thread_after_old_source() {
    let root = tempfile::tempdir().unwrap();
    let backend = fixture(
        root.path(),
        &[
            row("old-source", json!(4)),
            row("new-thread", json!(6)),
            row("old-thread", json!(3)),
            row("later-thread", json!(7)),
        ],
        &[row("next-input", json!(8))],
    );
    let config = root.path().join("native.json");
    std::fs::write(
        &config,
        br#"{"extractor":{"twitter":{"expand":true,"tweet-endpoint":"restid"}}}"#,
    )
    .unwrap();
    let result = output(
        binary(root.path(), &backend)
            .arg("--native-config")
            .arg(&config)
            .args(["-g", "--date-after=5", FIRST, SECOND]),
    )
    .await;
    success(&result);
    assert_eq!(
        urls(&result),
        [
            "https://cdn.example/new-thread.jpg",
            "https://cdn.example/later-thread.jpg",
            "https://cdn.example/next-input.jpg"
        ]
    );
    // Root false wins, retaining descending cutoff for an unexpanded single post.
    std::fs::write(
        &config,
        br#"{"expand":false,"extractor":{"twitter":{"expand":true,"tweet-endpoint":"restid"}}}"#,
    )
    .unwrap();
    let result = output(
        binary(root.path(), &backend)
            .arg("--native-config")
            .arg(&config)
            .args(["-g", "--date-after=5", FIRST, SECOND]),
    )
    .await;
    success(&result);
    assert_eq!(urls(&result), ["https://cdn.example/next-input.jpg"]);
}
