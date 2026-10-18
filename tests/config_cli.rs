use std::{
    io::{Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    process::Output,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use tokio::process::Command;

fn binary(cwd: &Path, config: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_gallery-dl-rs"));
    command
        .current_dir(cwd)
        .kill_on_drop(true)
        .arg("--native-config")
        .arg(config);
    command
}
async fn output(command: &mut Command) -> Output {
    tokio::time::timeout(Duration::from_secs(8), command.output())
        .await
        .unwrap()
        .unwrap()
}
fn success(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
fn config(dir: &Path, value: serde_json::Value) -> PathBuf {
    let path = dir.join("native.json");
    std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    path
}
fn touch(path: &Path) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, b"existing").unwrap();
}
#[cfg(unix)]
fn backend(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("fake backend");
    std::fs::write(
        &path,
        br#"#!/bin/sh
printf 'BEGIN\n' >> "$CONFIG_TEST_LOG"
printf '%s\n' "$@" >> "$CONFIG_TEST_LOG"
for source do :; done
case "$source" in
  *instagram.com*) category=instagram ;;
  *x.com*) category=twitter ;;
  *) category=facebook ;;
esac
printf '[[3,"https://cdn.example/%s.jpg",{"filename":"remote","extension":"jpg"}]]\n' "$category"
"#,
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    path
}

#[cfg(unix)]
#[tokio::test]
async fn category_destinations_and_filenames_anchor_to_config_directory() {
    let root = tempfile::tempdir().unwrap();
    let settings = root.path().join("settings");
    let cwd = root.path().join("elsewhere");
    std::fs::create_dir_all(&settings).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    let cfg = config(
        &settings,
        serde_json::json!({"extractor": {
            "base-directory":"base", "filename":"root.jpg",
            "instagram":{"base-directory":"ig","filename":"ig.jpg"},
            "twitter":{"base-directory":"tw","filename":"tw.jpg"},
            "facebook":{"base-directory":"fb","filename":"fb.jpg"},
            "direct":{"base-directory":"direct","filename":"direct.jpg"}
        }}),
    );
    for (folder, category) in [
        ("ig", "instagram"),
        ("tw", "twitter"),
        ("fb", "facebook"),
        ("direct", "direct"),
    ] {
        touch(
            &settings
                .join(folder)
                .join(category)
                .join(format!("{folder}.jpg")),
        );
    }
    let mut command = binary(&cwd, &cfg);
    command
        .arg("--extractor-command")
        .arg(backend(root.path()))
        .env("CONFIG_TEST_LOG", root.path().join("backend.log"))
        .args([
            "https://instagram.com/p/post/",
            "https://x.com/user/status/1",
            "https://facebook.com/photo.php?fbid=1",
            "https://example.com/remote.jpg",
        ]);
    let result = output(&mut command).await;
    success(&result);
    assert!(result.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert_eq!(stderr.matches("skipped ").count(), 4);
    assert!(!cwd.join("downloads").exists());
    assert!(!settings.join("base").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn category_cookies_reach_backend_and_explicit_cli_cookie_overrides() {
    let root = tempfile::tempdir().unwrap();
    let canonical = std::fs::canonicalize(root.path()).unwrap();
    let cfg = config(
        root.path(),
        serde_json::json!({"extractor": {
            "cookies":"default.txt", "instagram":{"cookies":"ig.txt"}, "twitter":{"cookies":"tw.txt"}
        }}),
    );
    let executable = backend(root.path());
    let log = root.path().join("backend.log");
    let mut command = binary(root.path(), &cfg);
    command
        .arg("--extractor-command")
        .arg(&executable)
        .env("CONFIG_TEST_LOG", &log)
        .args([
            "-g",
            "https://instagram.com/p/post/",
            "https://x.com/user/status/1",
            "https://facebook.com/photo.php?fbid=1",
        ]);
    success(&output(&mut command).await);
    let logs = std::fs::read_to_string(&log).unwrap();
    let blocks: Vec<_> = logs.split("BEGIN\n").skip(1).collect();
    for (block, name) in blocks.iter().zip(["ig.txt", "tw.txt", "default.txt"]) {
        assert!(block.contains(&format!("--cookies\n{}\n", canonical.join(name).display())));
    }
    assert_eq!(blocks.len(), 3);
    std::fs::write(&log, b"").unwrap();
    let mut command = binary(root.path(), &cfg);
    command
        .arg("--extractor-command")
        .arg(executable)
        .env("CONFIG_TEST_LOG", &log)
        .args([
            "--cookies",
            "cli.txt",
            "-g",
            "https://instagram.com/p/post/",
            "https://x.com/user/status/1",
        ]);
    success(&output(&mut command).await);
    let logs = std::fs::read_to_string(log).unwrap();
    assert_eq!(logs.matches("--cookies\ncli.txt\n").count(), 2);
    assert!(!logs.contains("ig.txt"));
    assert!(!logs.contains("tw.txt"));
}

#[tokio::test]
async fn inspection_never_opens_configured_archives_or_creates_destinations() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(
        root.path(),
        serde_json::json!({"extractor": {
            "base-directory":"never-created", "filename":"configured.jpg",
            "archive":"missing-parent/archive.sqlite", "cookies":"missing-cookies.txt",
            "twitter":{"archive":"other-missing-parent/history.sqlite"}
        }}),
    );
    for mode in ["-g", "-j"] {
        let mut command = binary(root.path(), &cfg);
        command.args([
            "--extractor",
            "native",
            mode,
            "https://example.com/image.jpg",
        ]);
        let result = output(&mut command).await;
        success(&result);
        assert!(!result.stdout.is_empty());
    }
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
}

#[tokio::test]
async fn archives_open_only_for_relevant_categories_and_cli_paths_override_config() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(
        root.path(),
        serde_json::json!({"extractor": {
            "base-directory":"configured", "filename":"configured.jpg", "archive":"configured.sqlite",
            "twitter":{"archive":"missing-parent/twitter.sqlite"}
        }}),
    );
    touch(&root.path().join("downloads/direct/cli.jpg"));
    let mut command = binary(root.path(), &cfg);
    command.args([
        "--destination",
        "downloads",
        "--filename",
        "cli.jpg",
        "--download-archive",
        "cli.sqlite",
        "https://example.com/image.jpg",
    ]);
    success(&output(&mut command).await);
    assert!(root.path().join("cli.sqlite").exists());
    assert!(!root.path().join("configured.sqlite").exists());
    assert!(!root.path().join("configured").exists());
    assert!(!root.path().join("missing-parent").exists());

    // Without a CLI override, only the direct archive is needed. The Twitter
    // archive's missing parent must not make this direct input fail.
    touch(&root.path().join("configured/direct/configured.jpg"));
    let mut command = binary(root.path(), &cfg);
    command.arg("https://example.com/second.jpg");
    success(&output(&mut command).await);
    assert!(root.path().join("configured.sqlite").exists());
    assert!(!root.path().join("missing-parent").exists());
}

#[tokio::test]
async fn unsupported_config_fails_before_output_and_does_not_echo_private_input() {
    let root = tempfile::tempdir().unwrap();
    for value in [
        serde_json::json!({"PRIVATE_KEY":"PRIVATE_VALUE"}),
        serde_json::json!({"extractor":{"filename":"{PRIVATE_VALUE}"}}),
    ] {
        let cfg = config(root.path(), value);
        let mut command = binary(root.path(), &cfg);
        command.args([
            "--destination",
            "never-created",
            "https://example.com/image.jpg",
        ]);
        let result = output(&mut command).await;
        assert!(!result.status.success());
        assert!(result.stdout.is_empty());
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(!stderr.contains("PRIVATE_KEY"));
        assert!(!stderr.contains("PRIVATE_VALUE"));
        assert!(!root.path().join("never-created").exists());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn native_and_backend_config_flags_are_separate_and_can_coexist() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(
        root.path(),
        serde_json::json!({"extractor":{"cookies":"cookies.txt"}}),
    );
    let log = root.path().join("backend.log");
    let mut command = binary(root.path(), &cfg);
    command
        .arg("--extractor-command")
        .arg(backend(root.path()))
        .env("CONFIG_TEST_LOG", &log)
        .args([
            "--config",
            "opaque-backend.conf",
            "-g",
            "https://instagram.com/p/post/",
        ]);
    success(&output(&mut command).await);
    let logs = std::fs::read_to_string(log).unwrap();
    assert!(logs.contains("--config\nopaque-backend.conf\n"));
    assert!(logs.contains("--cookies\n"));
    let mut command = binary(root.path(), &cfg);
    command.args([
        "--extractor",
        "native",
        "--config",
        "opaque-backend.conf",
        "-g",
        "https://example.com/image.jpg",
    ]);
    let result = output(&mut command).await;
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr)
            .contains("--config is only supported with --extractor compatibility")
    );
}

struct Server {
    url: String,
    count: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}
impl Server {
    fn new(script: Vec<(u16, Duration)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/image.jpg", listener.local_addr().unwrap());
        let count = Arc::new(AtomicUsize::new(0));
        let received = count.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let handle = thread::spawn(move || {
            for (status, delay) in script {
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut socket = loop {
                    if stopping.load(Ordering::SeqCst) || Instant::now() > deadline {
                        return;
                    }
                    match listener.accept() {
                        Ok((socket, _)) => break socket,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2))
                        }
                        Err(_) => return,
                    }
                };
                socket.set_nonblocking(false).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                socket
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = Vec::new();
                while !request.windows(4).any(|part| part == b"\r\n\r\n") && request.len() < 32768 {
                    let mut bytes = [0; 1024];
                    match socket.read(&mut bytes) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => request.extend_from_slice(&bytes[..n]),
                    }
                }
                received.fetch_add(1, Ordering::SeqCst);
                let deadline = Instant::now() + delay;
                while Instant::now() < deadline && !stopping.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(2));
                }
                let _ = socket.write_all(format!("HTTP/1.1 {status} Test\r\nContent-Length: 3\r\nConnection: close\r\n\r\nabc").as_bytes());
            }
        });
        Self {
            url,
            count,
            stop,
            handle: Some(handle),
        }
    }
    fn count(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.handle.take().unwrap().join().unwrap();
    }
}

#[tokio::test]
async fn configured_retry_count_is_used_and_explicit_default_looking_cli_value_wins() {
    for override_cli in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let cfg = config(
            root.path(),
            serde_json::json!({"extractor":{"retries":1,"filename":"result.jpg"}}),
        );
        let server = Server::new(vec![
            (503, Duration::ZERO),
            (503, Duration::ZERO),
            (200, Duration::ZERO),
        ]);
        let mut command = binary(root.path(), &cfg);
        if override_cli {
            command.args(["--retries", "2"]);
        }
        command.arg(&server.url);
        let result = output(&mut command).await;
        assert_eq!(
            result.status.success(),
            override_cli,
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(server.count(), if override_cli { 3 } else { 2 });
        assert_eq!(
            root.path().join("downloads/direct/result.jpg").exists(),
            override_cli
        );
    }
}

#[tokio::test]
async fn fractional_configured_timeout_is_applied_and_explicit_sixty_seconds_wins() {
    for override_cli in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let cfg = config(
            root.path(),
            serde_json::json!({"extractor":{"timeout":0.05,"retries":0,"filename":"result.jpg"}}),
        );
        let server = Server::new(vec![(200, Duration::from_millis(250))]);
        let mut command = binary(root.path(), &cfg);
        if override_cli {
            command.args(["--timeout", "60"]);
        }
        command.arg(&server.url);
        let result = output(&mut command).await;
        assert_eq!(
            result.status.success(),
            override_cli,
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(server.count(), 1);
        let path = root.path().join("downloads/direct/result.jpg");
        assert_eq!(path.exists(), override_cli);
        if override_cli {
            assert_eq!(std::fs::read(path).unwrap(), b"abc");
        } else {
            assert!(String::from_utf8_lossy(&result.stderr).contains("timed out"));
        }
    }
}

#[tokio::test]
async fn cli_timeouts_accept_fractions_and_reject_unusable_clock_values() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(root.path(), serde_json::json!({}));
    let mut command = binary(root.path(), &cfg);
    command.args(["--timeout", "0.025", "-g", "https://example.com/image.jpg"]);
    success(&output(&mut command).await);
    for timeout in ["0", "NaN", "inf", "-1", "1e100", "1e-100"] {
        let mut command = binary(root.path(), &cfg);
        command.args(["--timeout", timeout, "-g", "https://example.com/image.jpg"]);
        assert_eq!(output(&mut command).await.status.code(), Some(2));
    }
}

#[tokio::test]
async fn directory_config_cli_override_and_empty_list_choose_exact_parent() {
    let root = tempfile::tempdir().unwrap();
    let elsewhere = root.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    let url = "http://127.0.0.1:1/photo.jpg";
    let cfg = config(
        root.path(),
        serde_json::json!({
            "base-directory":"output", "directory":["config", "{category}"], "filename":"custom.jpg"
        }),
    );
    let configured = root.path().join("output/config/direct/custom.jpg");
    touch(&configured);
    let result = output(binary(&elsewhere, &cfg).arg(url)).await;
    success(&result);
    let exact = root.path().join("exact");
    touch(&exact.join("custom.jpg"));
    success(&output(binary(&elsewhere, &cfg).arg("-D").arg(&exact).arg(url)).await);
    assert!(!exact.join("direct").exists());
    let custom = root.path().join("output/cli/direct/custom.jpg");
    touch(&custom);
    let result = output(binary(&elsewhere, &cfg).args([
        "--directory-template",
        "cli",
        "--directory-template",
        "{category}",
        "--write-metadata",
        url,
    ]))
    .await;
    success(&result);
    assert!(custom.with_file_name("custom.jpg.json").exists());
    let cfg = config(
        root.path(),
        serde_json::json!({"base-directory":"output", "directory":[], "filename":"at-base.jpg"}),
    );
    touch(&root.path().join("output/at-base.jpg"));
    success(&output(binary(&elsewhere, &cfg).arg(url)).await);
    assert!(!root.path().join("output/direct").exists());
}

#[tokio::test]
async fn missing_directory_metadata_fails_before_requests_or_output() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(
        root.path(),
        serde_json::json!({"base-directory":"output", "directory":["{username}"]}),
    );
    let result = output(binary(root.path(), &cfg).arg("http://127.0.0.1:1/photo.jpg")).await;
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("requires post metadata"));
    assert!(!root.path().join("output").exists());
    let inspection =
        output(binary(root.path(), &cfg).args(["-g", "https://example.com/photo.jpg"])).await;
    success(&inspection);
    assert!(!root.path().join("output").exists());
}

#[tokio::test]
async fn exact_directory_rejects_ambiguous_cli_layout_flags() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(root.path(), serde_json::json!({}));
    for flags in [
        vec!["-D", "out", "-d", "other"],
        vec!["-D", "out", "--directory-template", "other"],
    ] {
        let result = output(
            binary(root.path(), &cfg)
                .args(flags)
                .arg("https://example.com/photo.jpg"),
        )
        .await;
        assert_eq!(result.status.code(), Some(2));
    }
}

#[tokio::test]
async fn empty_media_selection_opens_no_archive_or_destination() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(
        root.path(),
        serde_json::json!({
            "file-range":"0", "base-directory":"output", "archive":"missing-parent/archive.sqlite"
        }),
    );
    success(&output(binary(root.path(), &cfg).arg("http://127.0.0.1:1/photo.jpg")).await);
    assert!(!root.path().join("output").exists());
    assert!(!root.path().join("missing-parent").exists());
}
