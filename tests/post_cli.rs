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

fn post(id: &str, count: usize, date: i64) -> Value {
    json!([2,{"tweet_id":id,"count":count,"date":date,"content":format!("post-{id}")}])
}
fn row(name: &str, id: &str, num: usize, width: u64) -> Value {
    json!([3,format!("https://cdn.example/{name}.jpg"),{"filename":name,"extension":"jpg","tweet_id":id,"num":num,"width":width,"date":10}])
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
        "printf '%s\\n' \"$@\" >> \"$0.arguments\"\nprintf invoked >> \"$0.log\"\nfor source do :; done\ncase \"$source\" in\n*/status/2) cat \"$0.second\";;\n*) cat \"$0.first\";;\nesac",
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
        .expect("post CLI fixture deadline exceeded")
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
async fn empty_posts_and_carousels_occupy_explicit_slots_even_when_post_ids_repeat() {
    let root = tempfile::tempdir().unwrap();
    let backend = fixture(
        root.path(),
        &[
            post("100", 0, 10),
            post("100", 2, 10),
            row("carousel-a", "100", 1, 500),
            row("carousel-b", "100", 2, 1500),
            post("100", 1, 10),
            row("repeat-post", "100", 3, 2000),
        ],
        &[
            post("200", 0, 10),
            post("200", 1, 10),
            row("next-input", "200", 1, 2000),
        ],
    );
    let result =
        output(binary(root.path(), &backend).args(["-g", "--post-range", "2", FIRST, SECOND]))
            .await;
    success(&result);
    assert_eq!(
        urls(&result),
        [url("carousel-a"), url("carousel-b"), url("next-input")]
    );
    let result =
        output(binary(root.path(), &backend).args(["-g", "--post-range", "3", FIRST])).await;
    success(&result);
    assert_eq!(urls(&result), [url("repeat-post")]);
    let args = std::fs::read_to_string(root.path().join("backend.arguments")).unwrap();
    assert!(!args.contains("--post-range"));
    assert!(!args.contains("--post-filter"));
}

#[tokio::test]
async fn post_predicates_and_dates_precede_post_ranges_and_file_selection_follows() {
    let root = tempfile::tempdir().unwrap();
    let backend = fixture(
        root.path(),
        &[
            post("500", 2, 20),
            row("too-new-first", "500", 1, 2000),
            row("too-new-second", "500", 2, 2000),
            post("200", 1, 10),
            row("post-rejected", "200", 1, 2000),
            post("300", 3, 9),
            row("small", "300", 1, 500),
            row("first-large", "300", 2, 1500),
            row("selected", "300", 3, 2500),
            post("400", 1, 8),
            row("later", "400", 1, 2000),
        ],
        &[],
    );
    let result = output(binary(root.path(), &backend).args([
        "-g",
        "--date-before=15",
        "--post-filter",
        "post_id >= 300",
        "--post-filter",
        "count > 1",
        "--post-range",
        "1",
        "--filter",
        "width >= 1000",
        "--range",
        "2",
        FIRST,
    ]))
    .await;
    success(&result);
    assert_eq!(urls(&result), [url("selected")]);
    // File-like fields in Directory metadata cannot leak into post predicates.
    let mut directory = post("600", 1, 10);
    directory[1]["width"] = json!(2000);
    directory[1]["extension"] = json!("jpg");
    directory[1]["type"] = json!("photo");
    let backend = fixture(
        root.path(),
        &[directory, row("file-fields", "600", 1, 2000)],
        &[],
    );
    for predicate in [
        "extension == 'jpg'",
        "width >= 1000",
        "metadata['file']['width'] >= 1000",
    ] {
        let result =
            output(binary(root.path(), &backend).args(["-j", "--post-filter", predicate, FIRST]))
                .await;
        success(&result);
        assert_eq!(
            serde_json::from_slice::<Value>(&result.stdout).unwrap(),
            json!([])
        );
    }
}

#[tokio::test]
async fn post_configuration_scopes_clear_and_cli_values_replace_settings() {
    let root = tempfile::tempdir().unwrap();
    let backend = fixture(
        root.path(),
        &[
            post("100", 1, 10),
            row("one", "100", 1, 1000),
            post("200", 1, 10),
            row("two", "200", 1, 1000),
            post("300", 1, 10),
            row("three", "300", 1, 1000),
        ],
        &[],
    );
    let config = root.path().join("native.json");
    for (settings, extra, expected) in [
        (
            json!({"extractor":{"post-range":"1","twitter":{"post-range":"2"}}}),
            vec![],
            vec![url("two")],
        ),
        (
            json!({"post-range":"3","extractor":{"twitter":{"post-range":"2"}}}),
            vec![],
            vec![url("three")],
        ),
        (
            json!({"post-range":[],"post-filter":"","extractor":{"post-range":"0","post-filter":"False"}}),
            vec![],
            vec![url("one"), url("two"), url("three")],
        ),
        (
            json!({"post-range":"3","post-filter":"post_id == 300"}),
            vec!["--post-filter", "post_id < 300", "--post-range", "1"],
            vec![url("one")],
        ),
        (
            json!({"extractor":{"post-filter":"post_id > 100","twitter":{"post-filter":"post_id > 200"}}}),
            vec![],
            vec![url("three")],
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
async fn invalid_post_arguments_fail_privately_before_side_effects_and_zero_never_extracts() {
    let root = tempfile::tempdir().unwrap();
    let backend = script(root.path(), "printf invoked > \"$0.log\"\nexit 9");
    let saved = root.path().join("not-created");
    let archive = root.path().join("not-created.sqlite");
    for (option, value) in [
        ("--post-filter", "PRIVATE-POST-VALUE > )"),
        ("--post-range", "PRIVATE-POST-VALUE"),
    ] {
        let result = output(
            binary(root.path(), &backend)
                .arg("-D")
                .arg(&saved)
                .arg("--download-archive")
                .arg(&archive)
                .args([option, value, FIRST]),
        )
        .await;
        assert!(!result.status.success());
        assert!(result.stdout.is_empty());
        assert!(!String::from_utf8_lossy(&result.stderr).contains("PRIVATE-POST-VALUE"));
        assert!(!saved.exists());
        assert!(!archive.exists());
        assert!(!root.path().join("backend.log").exists());
    }
    let result =
        output(binary(root.path(), &backend).args(["-j", "--post-range", "0", FIRST])).await;
    success(&result);
    assert_eq!(
        serde_json::from_slice::<Value>(&result.stdout).unwrap(),
        json!([])
    );
    assert!(!root.path().join("backend.log").exists());
}

#[tokio::test]
async fn selected_post_files_alone_receive_archive_and_sidecars() {
    let root = tempfile::tempdir().unwrap();
    let rows = [
        post("100", 1, 10),
        row("one", "100", 1, 1000),
        post("200", 2, 10),
        row("two-a", "200", 1, 1000),
        row("two-b", "200", 2, 1000),
        post("300", 1, 10),
        row("three", "300", 1, 1000),
    ];
    let backend = fixture(root.path(), &rows, &[]);
    let saved = root.path().join("saved");
    std::fs::create_dir(&saved).unwrap();
    for name in ["one", "two-a", "two-b", "three"] {
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
                "--post-range",
                "2",
                FIRST,
            ]),
    )
    .await;
    success(&result);
    for name in ["one", "two-a", "two-b", "three"] {
        assert_eq!(
            std::fs::read(saved.join(format!("{name}.jpg"))).unwrap(),
            b"existing"
        );
        assert_eq!(
            saved.join(format!("{name}.jpg.json")).exists(),
            name.starts_with("two-")
        );
    }
    let mut expected: Vec<_> =
        gallery_dl_rs::extract::parse_messages(&serde_json::to_vec(&rows).unwrap(), "twitter")
            .unwrap()
            .into_iter()
            .filter(|item| item.metadata.as_ref().unwrap().post_id == "200")
            .map(|item| item.archive_key().unwrap())
            .collect();
    expected.sort();
    let connection = rusqlite::Connection::open(archive).unwrap();
    let mut records: Vec<String> = connection
        .prepare("SELECT identity FROM completed_media")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    records.sort();
    assert_eq!(records, expected);
}

#[tokio::test]
async fn finite_post_range_uses_next_directory_not_declared_count_and_drops_later_rows() {
    let root = tempfile::tempdir().unwrap();
    let backend = script(
        root.path(),
        &format!(
            "printf '%s' \"$$\" > \"$0.pid\"\nprintf '%s' '[{},{},{},{},[3,\"PRIVATE-LATER-INVALID-URL\",{{}}],'\nwhile :; do sleep 0.02; done",
            post("100", 1, 10),
            row("first", "100", 1, 1000),
            row("second", "100", 2, 1000),
            post("200", 1, 10)
        ),
    );
    let result =
        output(binary(root.path(), &backend).args(["-g", "--post-range", "1", FIRST])).await;
    success(&result);
    assert_eq!(urls(&result), [url("first"), url("second")]);
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
    .expect("post-range backend remained alive");
}

#[tokio::test]
async fn raw_media_without_boundaries_stays_compatible_until_post_selection_is_requested() {
    let root = tempfile::tempdir().unwrap();
    let backend = fixture(
        root.path(),
        &[row("unframed", "100", 1, 1000)],
        &[post("200", 1, 10), row("later-input", "200", 1, 1000)],
    );
    let result = output(binary(root.path(), &backend).args(["-g", FIRST])).await;
    success(&result);
    assert_eq!(urls(&result), [url("unframed")]);
    let result =
        output(binary(root.path(), &backend).args(["-g", "--post-range", "1", FIRST, SECOND]))
            .await;
    assert_eq!(result.status.code(), Some(1));
    assert_eq!(urls(&result), [url("later-input")]);
    assert!(String::from_utf8_lossy(&result.stderr).contains("explicit post boundaries"));
    assert!(String::from_utf8_lossy(&result.stderr).contains("1 operation(s) failed"));
}

#[tokio::test]
async fn tag_blacklist_and_whitelist_select_posts_by_tags() {
    let root = tempfile::tempdir().unwrap();
    let tagged = |id: &str, tags: Value| json!([2,{"category":"zz","id":id,"count":1,"tags":tags}]);
    let file = |name: &str, id: &str| json!([3,format!("https://cdn.example/{name}.jpg"),{"category":"zz","filename":name,"extension":"jpg","id":id,"num":1}]);
    let rows = [
        tagged("1", json!(["cat", "dog"])),
        file("one", "1"),
        tagged("2", json!("bird fish tags joined by single spaces")),
        file("two", "2"),
        tagged("3", json!([])),
        file("three", "3"),
    ];
    let backend = fixture(root.path(), &rows, &[]);
    for (flags, expected) in [
        (
            ["--tags-blacklist", "dog"],
            "https://cdn.example/two.jpg\nhttps://cdn.example/three.jpg\n",
        ),
        (
            ["--tags-whitelist", "fish,cat"],
            "https://cdn.example/one.jpg\nhttps://cdn.example/two.jpg\nhttps://cdn.example/three.jpg\n",
        ),
        (
            ["--tags-blacklist", "bird -fish"],
            "https://cdn.example/one.jpg\nhttps://cdn.example/two.jpg\nhttps://cdn.example/three.jpg\n",
        ),
    ] {
        let result = output(
            binary(root.path(), &backend)
                .arg("-g")
                .args(flags)
                .arg("https://unknown.example/g/1"),
        )
        .await;
        success(&result);
        assert_eq!(
            String::from_utf8_lossy(&result.stdout),
            expected,
            "{flags:?}"
        );
    }
}
