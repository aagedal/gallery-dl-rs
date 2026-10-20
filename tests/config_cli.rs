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
  *pixiv.net*) category=pixiv-novel ;;
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
            "pixiv-novel":{"base-directory":"novels","filename":"novels.jpg"},
            "direct":{"base-directory":"direct","filename":"direct.jpg"}
        }}),
    );
    for (folder, category) in [
        ("ig", "instagram"),
        ("tw", "twitter"),
        ("fb", "facebook"),
        ("novels", "pixiv-novel"),
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
            "https://www.pixiv.net/n/42",
            "https://example.com/remote.jpg",
        ]);
    let result = output(&mut command).await;
    success(&result);
    assert!(result.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert_eq!(stderr.matches("skipped ").count(), 5);
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

#[cfg(not(windows))]
#[tokio::test]
async fn default_native_configuration_is_discovered_and_can_be_ignored() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let xdg = root.path().join("xdg");
    std::fs::create_dir_all(xdg.join("gallery-dl-rs")).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    // An unknown setting proves the file was loaded, because parsing rejects it.
    std::fs::write(xdg.join("gallery-dl-rs/config.json"), br#"{"bogus":1}"#).unwrap();
    let run = |extra: &[&str], vars: &[(&str, &Path)]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_gallery-dl-rs"));
        command
            .kill_on_drop(true)
            .env_remove("GALLERY_DL_RS_CONFIG")
            .env("HOME", &home)
            .env_remove("XDG_CONFIG_HOME")
            .args(extra)
            .args(["-g", "http://127.0.0.1:1/a.jpg"]);
        for (name, value) in vars {
            command.env(name, value);
        }
        command
    };
    assert!(
        !output(&mut run(&[], &[("XDG_CONFIG_HOME", &xdg)]))
            .await
            .status
            .success()
    );
    success(
        &output(&mut run(
            &["--native-config-ignore"],
            &[("XDG_CONFIG_HOME", &xdg)],
        ))
        .await,
    );
    // Without XDG_CONFIG_HOME, ~/.config is searched, then ~/.gallery-dl-rs.conf.
    success(&output(&mut run(&[], &[])).await);
    std::fs::write(home.join(".gallery-dl-rs.conf"), br#"{"bogus":1}"#).unwrap();
    assert!(!output(&mut run(&[], &[])).await.status.success());
    // An explicit environment path takes precedence; explicit flags bypass discovery.
    let valid = config(root.path(), serde_json::json!({"retries": 1}));
    success(&output(&mut run(&[], &[("GALLERY_DL_RS_CONFIG", &valid)])).await);
    success(&output(&mut run(&["--native-config", valid.to_str().unwrap()], &[])).await);
}

#[cfg(unix)]
#[tokio::test]
async fn site_media_options_forward_only_to_their_compatibility_categories() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(
        root.path(),
        serde_json::json!({"extractor": {
            "imgur": {"mp4": false},
            "mastodon": {"reblogs": true, "replies": false, "text-posts": true, "cards": true}
        }}),
    );
    for (url, expected, absent) in [
        (
            "https://imgur.com/a/abcde",
            vec!["mp4=false"],
            vec!["reblogs=", "text-posts=", "replies="],
        ),
        (
            "https://mastodon.social/@alice",
            vec![
                "reblogs=true",
                "text-posts=true",
                "replies=false",
                "cards=true",
            ],
            vec!["mp4="],
        ),
        (
            "https://instagram.com/p/abc/",
            vec![],
            vec!["mp4=", "reblogs=", "text-posts=", "replies="],
        ),
    ] {
        let log = root.path().join("backend.log");
        let _ = std::fs::remove_file(&log);
        let mut command = binary(root.path(), &cfg);
        command
            .arg("--extractor-command")
            .arg(backend(root.path()))
            .env("CONFIG_TEST_LOG", &log)
            .args(["-g", url]);
        success(&output(&mut command).await);
        let log = std::fs::read_to_string(log).unwrap();
        for option in expected {
            assert!(log.contains(&format!("--option\n{option}\n")), "{log}");
        }
        for option in absent {
            assert!(!log.contains(option), "{log}");
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn bluesky_and_pixiv_selection_forward_only_to_matching_backends() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(
        root.path(),
        serde_json::json!({"extractor": {
            "bluesky": {"reposts": true, "quoted": true, "videos": false},
            "pixiv": {"ugoira": false},
            "pixiv-novel": {"covers": true, "full-series": false}
        }}),
    );
    for (url, expected, absent) in [
        (
            "https://bsky.app/profile/alice.test",
            vec!["reposts=true", "quoted=true", "videos=false"],
            vec!["ugoira=", "covers=", "full-series="],
        ),
        (
            "https://www.pixiv.net/artworks/42",
            vec!["ugoira=false"],
            vec!["reposts=", "quoted=", "videos=", "covers=", "full-series="],
        ),
        (
            "https://www.pixiv.net/novel/show.php?id=42",
            vec!["covers=true", "full-series=false"],
            vec!["ugoira=", "reposts=", "quoted=", "videos="],
        ),
        (
            "https://instagram.com/p/abc/",
            vec![],
            vec!["ugoira=", "reposts=", "quoted=", "videos="],
        ),
    ] {
        let log = root.path().join("backend.log");
        let _ = std::fs::remove_file(&log);
        let mut command = binary(root.path(), &cfg);
        command
            .arg("--extractor-command")
            .arg(backend(root.path()))
            .env("CONFIG_TEST_LOG", &log)
            .args(["-g", url]);
        success(&output(&mut command).await);
        let log = std::fs::read_to_string(log).unwrap();
        for option in expected {
            assert!(log.contains(&format!("--option\n{option}\n")), "{log}");
        }
        for option in absent {
            assert!(!log.contains(option), "{log}");
        }
    }
}

#[cfg(unix)]
fn mastodon_backend(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("mastodon-backend");
    std::fs::write(&path, br#"#!/bin/sh
printf 'BEGIN\n' >> "$CONFIG_TEST_LOG"
printf '%s\n' "$@" >> "$CONFIG_TEST_LOG"
for source do :; done
case "$source" in
  *mastodon.social*) instance=mastodon.social ;;
  *pawoo.net*) instance=pawoo.net ;;
  *baraag.net*) instance=baraag.net ;;
esac
printf '[[2,"",{"id":"100","account":{"acct":"alice"},"instance":"%s","date":"2020-01-01T00:00:00Z"}],[3,"https://cdn.example/shared.jpg",{"filename":"shared","extension":"jpg","id":"100","num":1,"media":{"id":"101"},"account":{"acct":"alice"},"instance":"%s","date":"2020-01-01T00:00:00Z"}],[2,"",{"id":"200","account":{"acct":"alice"},"instance":"%s","date":"2025-01-01T00:00:00Z"}],[3,"https://cdn.example/shared.jpg",{"filename":"shared","extension":"jpg","id":"200","num":1,"media":{"id":"102"},"account":{"acct":"alice"},"instance":"%s","date":"2025-01-01T00:00:00Z"}]]\n' "$instance" "$instance" "$instance" "$instance"
"#).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    path
}

#[cfg(unix)]
#[tokio::test]
async fn mastodon_instance_scopes_preserve_credentials_metadata_and_distinct_archives() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(
        root.path(),
        serde_json::json!({"extractor": {
            "mastodon": {"reblogs":true, "post-filter":"id >= 100", "filename":"{category}_{id}.{extension}"},
            "mastodon.social":{"access-token":"social-token"},
            "pawoo":{"access-token":"pawoo-token"}
        }}),
    );
    let log = root.path().join("backend.log");
    let mut command = binary(root.path(), &cfg);
    command
        .arg("--extractor-command")
        .arg(mastodon_backend(root.path()))
        .env("CONFIG_TEST_LOG", &log)
        .args([
            "-j",
            "https://mastodon.social/bookmarks",
            "https://pawoo.net/bookmarks",
            "https://baraag.net/bookmarks",
        ]);
    let result = output(&mut command).await;
    success(&result);
    let items: Vec<serde_json::Value> = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(items.len(), 6, "{items:?}");
    for (pair, (category, instance)) in items.as_chunks::<2>().0.iter().zip([
        ("mastodon.social", "mastodon.social"),
        ("pawoo", "pawoo.net"),
        ("baraag", "baraag.net"),
    ]) {
        assert_eq!(pair[0]["category"], category);
        assert_eq!(pair[1]["category"], category);
        assert_eq!(pair[0]["metadata"]["instance"], instance);
        assert_eq!(pair[0]["metadata"]["media_id"], "101");
        assert_eq!(pair[1]["metadata"]["media_id"], "102");
    }
    let stdout = String::from_utf8(result.stdout).unwrap();
    assert!(!stdout.contains("social-token") && !stdout.contains("pawoo-token"));
    let log = std::fs::read_to_string(log).unwrap();
    let invocations: Vec<_> = log.split("BEGIN\n").skip(1).collect();
    assert_eq!(invocations.len(), 3);
    assert!(invocations[0].contains("access-token=\"social-token\""));
    assert!(!invocations[0].contains("pawoo-token"));
    assert!(invocations[1].contains("access-token=\"pawoo-token\""));
    assert!(!invocations[1].contains("social-token"));
    assert!(!invocations[2].contains("access-token="));
}

#[cfg(unix)]
#[tokio::test]
async fn mastodon_bookmark_date_filter_keeps_later_newer_posts() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(root.path(), serde_json::json!({}));
    let mut command = binary(root.path(), &cfg);
    command
        .arg("--extractor-command")
        .arg(mastodon_backend(root.path()))
        .env("CONFIG_TEST_LOG", root.path().join("backend.log"))
        .args([
            "-j",
            "--date-after",
            "2024-01-01",
            "https://mastodon.social/bookmarks",
        ]);
    let result = output(&mut command).await;
    success(&result);
    let items: Vec<serde_json::Value> = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0]["metadata"]["post_id"], "200");
}

#[cfg(unix)]
#[tokio::test]
async fn date_selection_preserves_later_newer_bluesky_posts_and_pixiv_bookmarks() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let cfg = config(root.path(), serde_json::json!({}));
    let executable = root.path().join("bluesky-backend");
    std::fs::write(&executable, br#"#!/bin/sh
printf '%s\n' '[[2,"",{"id":42,"post_id":"old","date":"2020-01-01T00:00:00Z","author":{"handle":"alice.test"}}],[3,"https://bsky.social/xrpc/com.atproto.sync.getBlob?did=did:plc:abcdefghijklmnopqrstuvwx&cid=bafykreia",{"filename":"bafykreia","extension":"png","id":42,"post_id":"old","num":0,"date":"2020-01-01T00:00:00Z","author":{"handle":"alice.test"}}],[2,"",{"id":43,"post_id":"new","date":"2025-01-01T00:00:00Z","author":{"handle":"alice.test"}}],[3,"https://bsky.social/xrpc/com.atproto.sync.getBlob?did=did:plc:abcdefghijklmnopqrstuvwx&cid=bafykreib",{"filename":"bafykreib","extension":"png","id":43,"post_id":"new","num":0,"date":"2025-01-01T00:00:00Z","author":{"handle":"alice.test"}}]]'
"#).unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    for (url, post_id, media_id) in [
        ("https://bsky.app/profile/alice.test", "new", "bafykreib"),
        (
            "https://www.pixiv.net/users/9/bookmarks/artworks",
            "43",
            "43_p0",
        ),
        (
            "https://www.pixiv.net/users/9/bookmarks/novels?rest=hide",
            "43",
            "43_p0",
        ),
    ] {
        for extra in [vec![], vec!["--post-range", "1"]] {
            let mut command = binary(root.path(), &cfg);
            command
                .arg("--extractor-command")
                .arg(&executable)
                .args(extra)
                .args(["-j", "--date-after", "2024-01-01", url]);
            let result = output(&mut command).await;
            success(&result);
            let items: Vec<serde_json::Value> = serde_json::from_slice(&result.stdout).unwrap();
            assert_eq!(items.len(), 1, "{items:?}");
            assert_eq!(items[0]["metadata"]["post_id"], post_id);
            assert_eq!(items[0]["metadata"]["media_id"], media_id);
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn tumblr_selections_and_booru_credentials_forward_with_exact_types_and_scopes() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(
        root.path(),
        serde_json::json!({"extractor": {
            "tumblr":{"reblogs":"same-blog","posts":["photo","video"],"inline":"original","original":false,"api-key":"tumblrkey"},
            "danbooru":{"username":"alice","password":"key:one?&="},
            "e621":{"username":"bob","password":""},
            "gelbooru":{"user-id":42,"api-key":"key:&=!?"},
            "pixeldrain":{"api-key":"pixel:key"}
        }}),
    );
    for (url, expected, absent) in [
        (
            "https://alice.tumblr.com/",
            vec![
                "reblogs=\"same-blog\"",
                "posts=[\"photo\",\"video\"]",
                "inline=\"original\"",
                "original=false",
                "api-key=\"tumblrkey\"",
            ],
            vec!["username=", "password=", "user-id="],
        ),
        (
            "https://danbooru.donmai.us/posts/42",
            vec!["username=\"alice\"", "password=\"key:one?&=\""],
            vec![
                "posts=",
                "inline=",
                "original=",
                "user-id=",
                "api-key=",
                "reblogs=",
            ],
        ),
        (
            "https://e621.net/posts/42",
            vec!["username=\"bob\"", "password=\"\""],
            vec!["api-key=", "user-id=", "posts=", "reblogs="],
        ),
        (
            "https://gelbooru.com/index.php?page=post&s=view&id=42",
            vec!["user-id=\"42\"", "api-key=\"key:&=!?\""],
            vec!["username=", "password=", "posts=", "reblogs="],
        ),
        (
            "https://pixeldrain.com/u/file",
            vec!["api-key=\"pixel:key\""],
            vec!["username=", "password=", "user-id=", "reblogs="],
        ),
        (
            "https://instagram.com/p/abc/",
            vec![],
            vec![
                "username=",
                "password=",
                "user-id=",
                "api-key=",
                "posts=",
                "inline=",
                "original=",
                "reblogs=",
            ],
        ),
    ] {
        let log = root.path().join("backend.log");
        let _ = std::fs::remove_file(&log);
        let mut command = binary(root.path(), &cfg);
        command
            .arg("--extractor-command")
            .arg(backend(root.path()))
            .env("CONFIG_TEST_LOG", &log)
            .args(["-g", url]);
        let result = output(&mut command).await;
        success(&result);
        let log = std::fs::read_to_string(log).unwrap();
        for value in expected {
            assert!(log.contains(&format!("--option\n{value}\n")), "{log}");
        }
        for value in absent {
            assert!(!log.contains(value), "{log}");
        }
        let stdout = String::from_utf8(result.stdout).unwrap();
        assert!(
            !stdout.contains("key:one")
                && !stdout.contains("pixel:key")
                && !stdout.contains("tumblrkey")
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn kemono_expansion_and_post_limits_forward_only_to_supported_categories() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(
        root.path(),
        serde_json::json!({"extractor": {
            "expand":true,"max-posts":10,
            "kemono":{"expand":false,"max-posts":2},
            "coomer":{"max-posts":0}
        }}),
    );
    for (url, expected, absent) in [
        (
            "https://kemono.cr/posts?q=cat",
            vec!["expand=false", "max-posts=2"],
            vec![],
        ),
        (
            "https://coomer.st/onlyfans/user/alice?tag=cat",
            vec!["expand=true", "max-posts=0"],
            vec![],
        ),
        (
            "https://x.com/alice/status/42",
            vec!["expand=true"],
            vec!["max-posts="],
        ),
        (
            "https://instagram.com/p/abc/",
            vec![],
            vec!["expand=", "max-posts="],
        ),
    ] {
        let log = root.path().join("backend.log");
        let _ = std::fs::remove_file(&log);
        let mut command = binary(root.path(), &cfg);
        command
            .arg("--extractor-command")
            .arg(backend(root.path()))
            .env("CONFIG_TEST_LOG", &log)
            .args(["-g", url]);
        success(&output(&mut command).await);
        let log = std::fs::read_to_string(log).unwrap();
        for value in expected {
            assert!(log.contains(&format!("--option\n{value}\n")), "{log}");
        }
        for value in absent {
            assert!(!log.contains(value), "{log}");
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn kemono_dates_do_not_terminate_before_later_newer_posts() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let cfg = config(root.path(), serde_json::json!({}));
    let executable = root.path().join("kemono-backend");
    std::fs::write(&executable, br#"#!/bin/sh
printf '%s\n' '[[2,"",{"id":"1","user":"42","service":"patreon","date":"2020-01-01T00:00:00Z"}],[3,"https://kemono.cr/data/aa/bb/hash1.jpg",{"filename":"old","extension":"jpg","id":"1","user":"42","service":"patreon","hash":"hash1","num":1,"date":"2020-01-01T00:00:00Z"}],[2,"",{"id":"2","user":"43","service":"fanbox","date":"2025-01-01T00:00:00Z"}],[3,"https://kemono.cr/data/aa/bb/hash2.jpg",{"filename":"new","extension":"jpg","id":"2","user":"43","service":"fanbox","hash":"hash2","num":1,"date":"2025-01-01T00:00:00Z"}]]'
"#).unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    for url in [
        "https://kemono.cr/posts",
        "https://coomer.st/onlyfans/user/alice",
    ] {
        for selection in [vec![], vec!["--post-range", "1"]] {
            let mut command = binary(root.path(), &cfg);
            command
                .arg("--extractor-command")
                .arg(&executable)
                .args(["-j", "--date-after", "2024-01-01"])
                .args(selection)
                .arg(url);
            let result = output(&mut command).await;
            success(&result);
            let items: Vec<serde_json::Value> = serde_json::from_slice(&result.stdout).unwrap();
            assert_eq!(items.len(), 1, "{items:?}");
            assert_eq!(items[0]["metadata"]["post_id"], "2");
            assert_eq!(items[0]["metadata"]["creator"]["id"], "43");
            assert_eq!(items[0]["metadata"]["instance"], "fanbox");
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn kemono_file_selection_forwards_only_to_its_category_and_keeps_duplicates() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let cfg = config(
        root.path(),
        serde_json::json!({"extractor":{
            "files":["attachments","file","inline"],"duplicates":true,"original":false,
            "coomer":{"files":[],"duplicates":false,"original":true}
        }}),
    );
    for (url, expected, absent) in [
        (
            "https://kemono.cr/posts",
            vec![
                "files=[\"attachments\",\"file\",\"inline\"]",
                "duplicates=[\"file\",\"attachment\",\"inline\"]",
                "original=false",
            ],
            vec![],
        ),
        (
            "https://coomer.st/posts",
            vec!["files=[]", "duplicates=[]", "original=true"],
            vec![],
        ),
        (
            "https://alice.tumblr.com/",
            vec!["original=false"],
            vec!["files=", "duplicates="],
        ),
        (
            "https://x.com/alice/status/42",
            vec![],
            vec!["files=", "duplicates=", "original="],
        ),
    ] {
        let log = root.path().join("backend.log");
        let _ = std::fs::remove_file(&log);
        let mut command = binary(root.path(), &cfg);
        command
            .arg("--extractor-command")
            .arg(backend(root.path()))
            .env("CONFIG_TEST_LOG", &log)
            .args(["-g", url]);
        success(&output(&mut command).await);
        let log = std::fs::read_to_string(log).unwrap();
        for value in expected {
            assert!(log.contains(&format!("--option\n{value}\n")), "{log}");
        }
        for value in absent {
            assert!(!log.contains(value), "{log}");
        }
    }
    let executable = root.path().join("duplicate-backend");
    std::fs::write(&executable,br#"#!/bin/sh
printf '%s\n' '[[3,"https://kemono.cr/data/aa/bb/aabb.jpg",{"id":"7","user":"42","service":"patreon","hash":"aabb","num":1,"type":"attachment","filename":"first","extension":"jpg"}],[3,"https://kemono.cr/data/aa/bb/aabb.jpg",{"id":"7","user":"42","service":"patreon","hash":"aabb","num":2,"type":"attachment","filename":"second","extension":"jpg"}]]'
"#).unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut command = binary(root.path(), &cfg);
    command
        .arg("--extractor-command")
        .arg(&executable)
        .args(["-j", "https://kemono.cr/posts"]);
    let result = output(&mut command).await;
    success(&result);
    let items: Vec<serde_json::Value> = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(items.len(), 2, "{items:?}");
    assert_eq!(items[0]["metadata"]["index"], 1);
    assert_eq!(items[1]["metadata"]["index"], 2);
}

#[cfg(unix)]
#[tokio::test]
async fn kemono_revision_options_forward_as_typed_scoped_values_and_keep_history() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let cfg = config(
        root.path(),
        serde_json::json!({"extractor":{
            "revisions":true,"order-revisions":"reverse",
            "kemono":{"revisions":"unique"},"coomer":{"revisions":false,"order-revisions":"desc"}
        }}),
    );
    for (url, expected, absent) in [
        (
            "https://kemono.cr/patreon/user/42/post/7/revisions",
            vec!["revisions=\"unique\"", "order-revisions=\"asc\""],
            vec![],
        ),
        (
            "https://coomer.st/onlyfans/user/alice",
            vec!["revisions=false", "order-revisions=\"desc\""],
            vec![],
        ),
        (
            "https://x.com/alice/status/42",
            vec![],
            vec!["revisions=", "order-revisions="],
        ),
        (
            "https://alice.tumblr.com/",
            vec![],
            vec!["revisions=", "order-revisions="],
        ),
    ] {
        let log = root.path().join("backend.log");
        let _ = std::fs::remove_file(&log);
        let mut command = binary(root.path(), &cfg);
        command
            .arg("--extractor-command")
            .arg(backend(root.path()))
            .env("CONFIG_TEST_LOG", &log)
            .args(["-g", url]);
        success(&output(&mut command).await);
        let log = std::fs::read_to_string(log).unwrap();
        for value in expected {
            assert!(log.contains(&format!("--option\n{value}\n")), "{log}");
        }
        for value in absent {
            assert!(!log.contains(value), "{log}");
        }
    }
    let executable = root.path().join("revision-backend");
    std::fs::write(&executable,br#"#!/bin/sh
printf '%s\n' '[[3,"https://kemono.cr/data/aa/bb/aabb.jpg",{"id":"7","user":"42","service":"patreon","hash":"aabb","num":1,"type":"file","filename":"current","extension":"jpg","revision_id":0,"revision_index":2,"revision_count":2}],[3,"https://kemono.cr/data/aa/bb/aabb.jpg",{"id":"7","user":"42","service":"patreon","hash":"aabb","num":1,"type":"file","filename":"history","extension":"jpg","revision_id":12,"revision_index":1,"revision_count":2}]]'
"#).unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut command = binary(root.path(), &cfg);
    command.arg("--extractor-command").arg(&executable).args([
        "-j",
        "--filter",
        "revision_count == 2",
        "https://kemono.cr/posts",
    ]);
    let result = output(&mut command).await;
    success(&result);
    let jobs: Vec<serde_json::Value> = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(jobs.len(), 2, "{jobs:?}");
    assert_eq!(jobs[0]["metadata"]["revision"]["id"], 0);
    assert_eq!(jobs[1]["metadata"]["revision"]["id"], 12);
}

#[cfg(unix)]
#[tokio::test]
async fn kemono_metadata_setting_forwards_only_to_kemono_and_coomer() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(
        root.path(),
        serde_json::json!({"extractor":{"metadata":false,"kemono":{"metadata":true}}}),
    );
    for (url, expected) in [
        ("https://kemono.cr/posts", Some("metadata=true")),
        ("https://coomer.st/posts", Some("metadata=false")),
        ("https://x.com/alice/status/42", None),
        ("https://alice.tumblr.com/", None),
    ] {
        let log = root.path().join("backend.log");
        let _ = std::fs::remove_file(&log);
        let mut command = binary(root.path(), &cfg);
        command
            .arg("--extractor-command")
            .arg(backend(root.path()))
            .env("CONFIG_TEST_LOG", &log)
            .args(["-g", url]);
        success(&output(&mut command).await);
        let log = std::fs::read_to_string(log).unwrap();
        match expected {
            Some(option) => assert!(log.contains(&format!("--option\n{option}\n")), "{log}"),
            None => assert!(!log.contains("metadata="), "{log}"),
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn kemono_endpoint_and_discord_order_forward_only_to_their_categories() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(
        root.path(),
        serde_json::json!({"extractor":{"endpoint":"legacy+","order-posts":"reverse","coomer":{"endpoint":"posts","order-posts":"asc"}}}),
    );
    for (url, expected) in [
        (
            "https://kemono.cr/discord/server/1/2",
            Some(["endpoint=\"posts+\"", "order-posts=\"desc\""]),
        ),
        (
            "https://coomer.st/onlyfans/user/alice",
            Some(["endpoint=\"posts\"", "order-posts=\"asc\""]),
        ),
        ("https://x.com/alice/status/42", None),
        ("https://alice.tumblr.com/", None),
    ] {
        let log = root.path().join("backend.log");
        let _ = std::fs::remove_file(&log);
        let mut command = binary(root.path(), &cfg);
        command
            .arg("--extractor-command")
            .arg(backend(root.path()))
            .env("CONFIG_TEST_LOG", &log)
            .args(["-g", url]);
        success(&output(&mut command).await);
        let log = std::fs::read_to_string(log).unwrap();
        if let Some(expected) = expected {
            for value in expected {
                assert!(log.contains(&format!("--option\n{value}\n")), "{log}");
            }
        } else {
            assert!(
                !log.contains("endpoint=") && !log.contains("order-posts="),
                "{log}"
            );
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn kemono_favorites_setting_forwards_only_to_supported_categories() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(
        root.path(),
        serde_json::json!({"extractor":{"favorites":"post","kemono":{"favorites":"artist"}}}),
    );
    for (url, expected) in [
        (
            "https://kemono.cr/account/favorites",
            Some("favorites=\"artist\""),
        ),
        ("https://coomer.st/favorites", Some("favorites=\"post\"")),
        ("https://x.com/alice/status/42", None),
        ("https://alice.tumblr.com/", None),
    ] {
        let log = root.path().join("backend.log");
        let _ = std::fs::remove_file(&log);
        let mut command = binary(root.path(), &cfg);
        command
            .arg("--extractor-command")
            .arg(backend(root.path()))
            .env("CONFIG_TEST_LOG", &log)
            .args(["-g", url]);
        success(&output(&mut command).await);
        let log = std::fs::read_to_string(log).unwrap();
        if let Some(expected) = expected {
            assert!(log.contains(&format!("--option\n{expected}\n")), "{log}");
        } else {
            assert!(!log.contains("favorites="), "{log}");
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn child_selection_forwards_to_backend_and_cli_overrides_scoped_settings() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(
        root.path(),
        serde_json::json!({"extractor":{"child-range":"2","child-filter":["service == 'patreon'"],"coomer":{"child-filter":[]}}}),
    );
    for (url, cli, expected) in [
        (
            "https://kemono.cr/artists",
            false,
            vec![
                "child-range=\"2\"",
                "child-filter=[\"service == 'patreon'\"]",
            ],
        ),
        (
            "https://coomer.st/favorites",
            false,
            vec!["child-range=\"2\"", "child-filter=\"\""],
        ),
        (
            "https://kemono.cr/artists",
            true,
            vec![
                "child-range=\"1\"",
                "child-filter=[\"id == '42'\",\"name != None\"]",
            ],
        ),
    ] {
        let log = root.path().join("backend.log");
        let _ = std::fs::remove_file(&log);
        let mut command = binary(root.path(), &cfg);
        command
            .arg("--extractor-command")
            .arg(backend(root.path()))
            .env("CONFIG_TEST_LOG", &log);
        if cli {
            command.args([
                "--child-range",
                "1",
                "--child-filter",
                "id == '42'",
                "--child-filter",
                "name != None",
            ]);
        }
        command.args(["-g", url]);
        success(&output(&mut command).await);
        let log = std::fs::read_to_string(log).unwrap();
        for expected in expected {
            assert!(log.contains(&format!("--option\n{expected}\n")), "{log}");
        }
    }
}

#[tokio::test]
async fn native_profile_child_selection_skips_requests_and_missing_credentials() {
    let root = tempfile::tempdir().unwrap();
    let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
    proxy.set_nonblocking(true).unwrap();
    let cfg = config(
        root.path(),
        serde_json::json!({"extractor": {
            "proxy": format!("http://{}", proxy.local_addr().unwrap()),
            "timeout": 1, "retries": 0
        }}),
    );
    for source in [
        "https://x.com/fixture",
        "https://bsky.app/profile/alice.bsky.social",
        "https://www.pixiv.net/users/9",
    ] {
        for option in [
            ["--child-filter", "False"],
            ["--child-range", "0"],
            ["--child-range", "2"],
        ] {
            let result = output(
                binary(root.path(), &cfg)
                    .args(["--extractor", "native", "-g"])
                    .args(option)
                    .arg(source),
            )
            .await;
            success(&result);
            assert!(
                result.stdout.is_empty(),
                "{}",
                String::from_utf8_lossy(&result.stdout)
            );
            assert_eq!(
                proxy.accept().unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn include_settings_forward_to_compatibility_with_all_and_empty_preserved() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(
        root.path(),
        serde_json::json!({"extractor": {
            "twitter":{"include":" media, replies,media "},
            "bluesky":{"include":"all"},"pixiv":{"include":false}
        }}),
    );
    let executable = backend(root.path());
    for (source, expected) in [
        (
            "https://x.com/fixture",
            "include=[\"media\",\"replies\",\"media\"]",
        ),
        (
            "https://bsky.app/profile/alice.bsky.social",
            "include=\"all\"",
        ),
        ("https://www.pixiv.net/users/9", "include=[]"),
    ] {
        let log = root.path().join("backend.log");
        let _ = std::fs::remove_file(&log);
        let result = output(
            binary(root.path(), &cfg)
                .arg("--extractor-command")
                .arg(&executable)
                .env("CONFIG_TEST_LOG", &log)
                .args(["-g", source]),
        )
        .await;
        success(&result);
        let log = std::fs::read_to_string(log).unwrap();
        assert!(log.contains(&format!("--option\n{expected}\n")), "{log}");
    }
}

#[tokio::test]
async fn disabled_native_profile_include_starts_no_children_or_requests() {
    let root = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let cfg = config(
        root.path(),
        serde_json::json!({"extractor": {
            "proxy":format!("http://{}",listener.local_addr().unwrap()),"timeout":1,"retries":0,
            "twitter":{"include":[]},"bluesky":{"include":false},"pixiv":{"include":null}
        }}),
    );
    for source in [
        "https://x.com/fixture",
        "https://bsky.app/profile/alice.bsky.social",
        "https://www.pixiv.net/users/9",
    ] {
        let result =
            output(binary(root.path(), &cfg).args(["--extractor", "native", "-g", source])).await;
        success(&result);
        assert!(result.stdout.is_empty());
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn bluesky_server_and_endpoint_forward_only_to_its_compatibility_backend() {
    let root = tempfile::tempdir().unwrap();
    for (endpoint, server) in [
        (
            serde_json::json!("getActorLikes"),
            serde_json::json!("https://appview.example/base/"),
        ),
        (serde_json::json!("other"), serde_json::json!(false)),
    ] {
        let cfg = config(
            root.path(),
            serde_json::json!({"extractor":{"bluesky":{"endpoint":endpoint,"api-server":server},"kemono":{"endpoint":"posts+"}}}),
        );
        for source in [
            "https://bsky.app/profile/alice.bsky.social/likes",
            "https://kemono.cr/patreon/user/9",
        ] {
            let log = root.path().join("backend.log");
            let _ = std::fs::remove_file(&log);
            let result = output(
                binary(root.path(), &cfg)
                    .arg("--extractor-command")
                    .arg(backend(root.path()))
                    .env("CONFIG_TEST_LOG", &log)
                    .args(["-g", source]),
            )
            .await;
            success(&result);
            let log = std::fs::read_to_string(log).unwrap();
            if source.contains("bsky.app") {
                let endpoint = if endpoint == "getActorLikes" {
                    "getActorLikes"
                } else {
                    "listRecords"
                };
                assert!(
                    log.contains(&format!("--option\nendpoint=\"{endpoint}\"\n")),
                    "{log}"
                );
                let server = server.as_str().unwrap_or("");
                assert!(
                    log.contains(&format!(
                        "--option\napi-server={}\n",
                        serde_json::json!(server)
                    )),
                    "{log}"
                );
                assert!(!log.contains("posts+"));
            } else {
                assert!(log.contains("endpoint=\"posts+\""));
                assert!(!log.contains("api-server="));
            }
        }
    }
}
