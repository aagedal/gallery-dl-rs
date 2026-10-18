#![cfg(unix)]

use futures_util::{FutureExt, StreamExt};
use gallery_dl_rs::extract::Backend;
use std::{os::unix::fs::PermissionsExt, path::Path, process::Stdio, time::Duration};
use tokio::time::{sleep, timeout};
use url::Url;

const ROW: &str = "[3,\"https://cdn.example/first.jpg\",{}]";

fn fixture(root: &Path, body: &str, budget: Duration) -> Backend {
    let executable = root.join("backend");
    std::fs::write(&executable, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    Backend {
        executable,
        cookies: None,
        config: None,
        timeout: budget,
    }
}

fn social_url() -> Url {
    Url::parse("https://x.com/person/status/1").unwrap()
}

async fn wait_until(mut predicate: impl FnMut() -> bool) {
    timeout(Duration::from_secs(5), async {
        while !predicate() {
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("fixture condition did not become true");
}

fn pid_alive(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
}

#[tokio::test]
async fn yields_before_outer_array_and_backend_finish() {
    let root = tempfile::tempdir().unwrap();
    let backend = fixture(
        root.path(),
        &format!(
            "printf '%s' '[{ROW}'\nwhile [ ! -f \"$0.release\" ]; do sleep 0.02; done\nprintf '%s' ']'"
        ),
        Duration::from_secs(5),
    );
    let mut stream = backend.stream(&social_url());
    let first = timeout(Duration::from_secs(2), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(first.url.as_str(), "https://cdn.example/first.jpg");
    assert!(!root.path().join("backend.release").exists());
    std::fs::write(root.path().join("backend.release"), b"").unwrap();
    assert!(
        timeout(Duration::from_secs(2), stream.next())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn collecting_api_preserves_resolved_array_command_and_failure_semantics() {
    let root = tempfile::tempdir().unwrap();
    let backend = fixture(
        root.path(),
        &format!(
            "resolved=false\narray=false\nfor arg do\ncase \"$arg\" in\n--resolve-json) resolved=true;;\noutput.jsonl=false) array=true;;\noutput.jsonl=true) exit 8;;\nesac\ndone\n$resolved && $array || exit 9\nprintf '%s' '[{ROW},{ROW}]'"
        ),
        Duration::from_secs(5),
    );
    let items = timeout(Duration::from_secs(2), backend.extract(&social_url()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(items.len(), 2);
    let backend = fixture(
        root.path(),
        &format!("printf '%s' '[{ROW}]'; exit 7"),
        Duration::from_secs(5),
    );
    assert!(
        timeout(Duration::from_secs(2), backend.extract(&social_url()))
            .await
            .unwrap()
            .is_err()
    );
}

#[tokio::test]
async fn reports_late_malformed_output_and_failed_exit_after_earlier_media() {
    for trailer in [
        "printf '%s' ']private-trailing-data'",
        "printf '%s' ']'; exit 7",
    ] {
        let root = tempfile::tempdir().unwrap();
        let backend = fixture(
            root.path(),
            &format!("printf '%s' '[{ROW}'\n{trailer}"),
            Duration::from_secs(5),
        );
        let mut stream = backend.stream(&social_url());
        assert!(stream.next().await.unwrap().is_ok());
        let failure = timeout(Duration::from_secs(2), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap_err()
            .to_string();
        assert!(!failure.contains("private-trailing-data"));
        assert!(
            failure.contains("trailing data") || failure.contains("extraction failed"),
            "{failure}"
        );
        assert!(stream.next().await.is_none());
    }
}

#[tokio::test]
async fn cli_listing_preserves_items_and_continues_after_late_failure() {
    for trailer in ["printf '%s' ']bad'", "printf '%s' ']'; exit 7"] {
        let root = tempfile::tempdir().unwrap();
        let backend = fixture(
            root.path(),
            &format!("printf '%s' '[{ROW}'\n{trailer}"),
            Duration::from_secs(5),
        );
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_gallery-dl-rs"));
        command
            .arg("--extractor-command")
            .arg(backend.executable)
            .args(["-g", social_url().as_str(), "https://example.com/later.jpg"])
            .kill_on_drop(true);
        let output = timeout(Duration::from_secs(5), command.output())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            "https://cdn.example/first.jpg\nhttps://example.com/later.jpg\n"
        );
    }
}

#[tokio::test]
async fn drains_large_stderr_and_reports_only_its_bounded_tail() {
    let root = tempfile::tempdir().unwrap();
    let mut diagnostics = b"PRIVATE-BEGINNING".to_vec();
    diagnostics.extend(vec![b'x'; 128 * 1024]);
    diagnostics.extend_from_slice(b"DIAGNOSTIC-END");
    std::fs::write(root.path().join("backend.stderr"), diagnostics).unwrap();
    let backend = fixture(
        root.path(),
        &format!("cat \"$0.stderr\" >&2\nprintf '%s' '[{ROW}]'\nexit 9"),
        Duration::from_secs(5),
    );
    let mut stream = backend.stream(&social_url());
    assert!(
        timeout(Duration::from_secs(3), stream.next())
            .await
            .unwrap()
            .unwrap()
            .is_ok()
    );
    let failure = timeout(Duration::from_secs(3), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap_err()
        .to_string();
    assert!(failure.contains("DIAGNOSTIC-END"));
    assert!(!failure.contains("PRIVATE-BEGINNING"));
    assert!(
        failure.len() < 8 * 1024 + 256,
        "diagnostic length {}",
        failure.len()
    );
}

#[tokio::test]
async fn unpolled_stream_starts_nothing_and_dropping_polled_stream_stops_child() {
    let root = tempfile::tempdir().unwrap();
    let backend = fixture(
        root.path(),
        &format!(
            "printf '%s' \"$$\" > \"$0.pid\"\nprintf '%s' '[{ROW}'\nwhile :; do sleep 0.02; done"
        ),
        Duration::from_secs(5),
    );
    drop(backend.stream(&social_url()));
    sleep(Duration::from_millis(80)).await;
    assert!(!root.path().join("backend.pid").exists());
    let mut stream = backend.stream(&social_url());
    stream.next().await.unwrap().unwrap();
    let pid = std::fs::read_to_string(root.path().join("backend.pid")).unwrap();
    assert!(pid_alive(&pid));
    drop(stream);
    wait_until(|| !pid_alive(&pid)).await;
}

#[tokio::test]
async fn consumer_backpressure_blocks_large_producer_until_more_items_are_read() {
    let root = tempfile::tempdir().unwrap();
    let row = format!(
        ",[3,\"https://cdn.example/next.jpg\",{{\"caption\":\"{}\"}}]",
        "x".repeat(4096)
    );
    std::fs::write(root.path().join("backend.rows"), row.repeat(1024)).unwrap();
    let backend = fixture(
        root.path(),
        &format!("printf '%s' '[{ROW}'\ncat \"$0.rows\"\n: > \"$0.finished\"\nprintf '%s' ']'"),
        Duration::from_secs(10),
    );
    let mut stream = backend.stream(&social_url());
    stream.next().await.unwrap().unwrap();
    sleep(Duration::from_millis(150)).await;
    assert!(!root.path().join("backend.finished").exists());
    let mut count = 1;
    timeout(Duration::from_secs(10), async {
        while let Some(item) = stream.next().await {
            item.unwrap();
            count += 1;
        }
    })
    .await
    .unwrap();
    assert_eq!(count, 1025);
    assert!(root.path().join("backend.finished").exists());
}

#[tokio::test]
async fn extraction_budget_is_cumulative_and_excludes_consumer_pause() {
    let root = tempfile::tempdir().unwrap();
    let backend = fixture(
        root.path(),
        &format!(
            "printf '%s' \"$$\" > \"$0.pid\"\nprintf '%s' '[{ROW}'\nwhile [ ! -f \"$0.release\" ]; do sleep 0.02; done\nprintf '%s' ',{ROW}'\nwhile [ ! -f \"$0.third\" ]; do sleep 0.02; done\nprintf '%s' ',{ROW}]'"
        ),
        Duration::from_secs(10),
    );
    let mut stream = backend.stream(&social_url());
    stream.next().await.unwrap().unwrap();
    // Move virtual time only while no process future is being awaited. This
    // avoids relying on shell startup or sleep precision under parallel tests.
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(30)).await;
    tokio::time::resume();
    let mut second = Box::pin(stream.next());
    assert!(second.as_mut().now_or_never().is_none());
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(6)).await;
    tokio::time::resume();
    std::fs::write(root.path().join("backend.release"), b"").unwrap();
    timeout(Duration::from_secs(2), &mut second)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    drop(second);
    let mut third = Box::pin(stream.next());
    assert!(third.as_mut().now_or_never().is_none());
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(5)).await;
    tokio::time::resume();
    let failure = timeout(Duration::from_secs(2), third)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(failure.to_string().contains("timed out"));
    let pid = std::fs::read_to_string(root.path().join("backend.pid")).unwrap();
    wait_until(|| !pid_alive(&pid)).await;
}

#[tokio::test]
async fn active_wait_timeout_stops_the_owned_backend() {
    let root = tempfile::tempdir().unwrap();
    let backend = fixture(
        root.path(),
        "printf '%s' \"$$\" > \"$0.pid\"\nwhile :; do sleep 0.02; done",
        Duration::from_secs(10),
    );
    let mut stream = backend.stream(&social_url());
    let mut next = Box::pin(stream.next());
    assert!(next.as_mut().now_or_never().is_none());
    // Let the real child start before advancing the extraction clock.
    wait_until(|| root.path().join("backend.pid").exists()).await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(11)).await;
    tokio::time::resume();
    let failure = timeout(Duration::from_secs(2), next)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(failure.to_string().contains("timed out"));
    let pid = std::fs::read_to_string(root.path().join("backend.pid")).unwrap();
    wait_until(|| !pid_alive(&pid)).await;
}

#[tokio::test]
async fn oversized_row_has_explicit_error_without_private_payload() {
    let root = tempfile::tempdir().unwrap();
    let mut payload = b"[[2,{\"caption\":\"PRIVATE-PAYLOAD".to_vec();
    payload.extend(vec![b'x'; 16 * 1024 * 1024]);
    payload.extend_from_slice(b"\"}]]");
    std::fs::write(root.path().join("backend.rows"), payload).unwrap();
    let backend = fixture(root.path(), "cat \"$0.rows\"", Duration::from_secs(10));
    let mut stream = backend.stream(&social_url());
    let failure = timeout(Duration::from_secs(10), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap_err()
        .to_string();
    assert!(failure.contains("size limit"), "{failure}");
    assert!(!failure.contains("PRIVATE-PAYLOAD"));
    assert!(failure.len() < 256);
}

#[tokio::test]
async fn failed_backend_without_json_preserves_its_failure_diagnostic() {
    let root = tempfile::tempdir().unwrap();
    let backend = fixture(
        root.path(),
        "printf '%s' 'login failed' >&2\nexit 1",
        Duration::from_secs(5),
    );
    let mut items = backend.stream(&social_url());
    let message = items.next().await.unwrap().unwrap_err().to_string();
    assert!(
        message.contains("extraction failed") && message.contains("login failed"),
        "{message}"
    );
    assert!(items.next().await.is_none());
}
