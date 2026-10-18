use std::{path::Path, process::Output, time::Duration};

use tokio::{io::AsyncWriteExt, process::Command};

fn binary(cwd: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_gallery-dl-rs"));
    command.current_dir(cwd).kill_on_drop(true);
    command
}

async fn output(command: &mut Command) -> Output {
    tokio::time::timeout(Duration::from_secs(8), command.output())
        .await
        .expect("CLI exceeded its bounded test deadline")
        .unwrap()
}

fn firefox_profile(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
    let db = rusqlite::Connection::open(path.join("cookies.sqlite")).unwrap();
    db.execute_batch(
        "CREATE TABLE moz_cookies (
            name TEXT, value TEXT, host TEXT, path TEXT,
            isSecure INTEGER, expiry INTEGER, originAttributes TEXT
        );
        INSERT INTO moz_cookies VALUES
            ('auth_token', 'synthetic-private-session', '.x.com', '/', 1, 4000000000, '');",
    )
    .unwrap();
}

fn browser_spec(path: &Path) -> String {
    format!("firefox:{}", path.display())
}

#[tokio::test]
async fn direct_inspection_does_not_open_profile_or_create_download_directories() {
    let root = tempfile::tempdir().unwrap();
    let missing = root.path().join("private-missing-profile");
    let output = output(binary(root.path()).args([
        "--extractor",
        "native",
        "--cookies-from-browser",
        &browser_spec(&missing),
        "-g",
        "https://example.com/image.jpg",
    ]))
    .await;
    assert!(output.status.success(), "{:?}", output);
    assert_eq!(output.stdout, b"https://example.com/image.jpg\n");
    assert!(output.stderr.is_empty());
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn missing_profile_is_a_private_per_input_failure() {
    let root = tempfile::tempdir().unwrap();
    let output = output(binary(root.path()).args([
        "--extractor",
        "native",
        "--cookies-from-browser",
        &browser_spec(&root.path().join("private-missing-profile")),
        "-g",
        "https://instagram.com/person/stories/",
        "https://example.com/image.jpg",
    ]))
    .await;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(output.stdout, b"https://example.com/image.jpg\n");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("initialization failed"), "{stderr}");
    assert!(!stderr.contains("private-missing-profile"), "{stderr}");
    assert!(
        !stderr.contains(&root.path().display().to_string()),
        "{stderr}"
    );
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn empty_range_never_opens_the_profile() {
    let root = tempfile::tempdir().unwrap();
    let output = output(binary(root.path()).args([
        "--extractor",
        "native",
        "--cookies-from-browser",
        &browser_spec(&root.path().join("private-missing-profile")),
        "--range",
        "0",
        "-g",
        "https://instagram.com/p/BA/",
    ]))
    .await;
    assert!(output.status.success(), "{:?}", output);
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn browser_cookies_override_missing_configured_cookie_files() {
    let root = tempfile::tempdir().unwrap();
    let profile = root.path().join("synthetic profile");
    firefox_profile(&profile);
    let db = profile.join("cookies.sqlite");
    let original = std::fs::read(&db).unwrap();
    let config = root.path().join("settings.json");
    std::fs::write(
        &config,
        br#"{"extractor":{"cookies":"private-missing-cookies.txt","instagram":{"cookies":"private-missing-instagram.txt"},"twitter":{"cookies":"private-missing-x.txt"}}}"#,
    )
    .unwrap();
    for path in [&profile, &db] {
        let output = output(
            binary(root.path())
                .arg("--native-config")
                .arg(&config)
                .args([
                    "--extractor",
                    "native",
                    "--cookies-from-browser",
                    &browser_spec(path),
                    "-g",
                    "https://instagram.com/person/stories/",
                    "https://x.com/person/status/1",
                    "https://example.com/image.jpg",
                ]),
        )
        .await;
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(output.stdout, b"https://example.com/image.jpg\n");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("--extractor compatibility"), "{stderr}");
        // The synthetic profile deliberately omits ct0. X must reject it before
        // making any bootstrap request, after successfully importing the jar.
        assert!(stderr.contains("auth_token and ct0"), "{stderr}");
        assert!(!stderr.contains("initialization failed"), "{stderr}");
        assert!(!stderr.contains("synthetic-private-session"), "{stderr}");
        assert!(!stderr.contains("private-missing-"), "{stderr}");
    }
    assert_eq!(std::fs::read(&db).unwrap(), original);
    assert_eq!(std::fs::read_dir(&profile).unwrap().count(), 1);
    assert!(!root.path().join("downloads").exists());
}

#[tokio::test]
async fn file_and_stdin_only_inputs_initialize_browser_cookies_lazily() {
    let root = tempfile::tempdir().unwrap();
    let inputs = b"https://instagram.com/person/stories/\nhttps://example.com/image.jpg\n";
    let path = root.path().join("inputs.txt");
    std::fs::write(&path, inputs).unwrap();
    let spec = browser_spec(&root.path().join("private-missing-profile"));
    let file = output(
        binary(root.path())
            .args([
                "--extractor",
                "native",
                "--cookies-from-browser",
                &spec,
                "-g",
                "-i",
            ])
            .arg(&path),
    )
    .await;
    assert_eq!(file.status.code(), Some(1));
    assert_eq!(file.stdout, b"https://example.com/image.jpg\n");
    assert!(String::from_utf8_lossy(&file.stderr).contains("initialization failed"));

    let mut child = binary(root.path())
        .args([
            "--extractor",
            "native",
            "--cookies-from-browser",
            &spec,
            "-g",
            "-i",
            "-",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(inputs).await.unwrap();
    drop(stdin);
    let stdin = tokio::time::timeout(Duration::from_secs(8), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stdin.status.code(), Some(1));
    assert_eq!(stdin.stdout, file.stdout);
    let stderr = String::from_utf8_lossy(&stdin.stderr);
    assert!(stderr.contains("initialization failed"), "{stderr}");
    assert!(!stderr.contains("private-missing-profile"), "{stderr}");
}

#[tokio::test]
async fn browser_selection_conflicts_with_explicit_cookie_file() {
    let root = tempfile::tempdir().unwrap();
    for flag in ["-C", "--cookies"] {
        let output = output(binary(root.path()).args([
            "--cookies-from-browser",
            "firefox:/missing/profile",
            flag,
            "/missing/cookies.txt",
            "-g",
            "https://example.com/image.jpg",
        ]))
        .await;
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("cannot be used with"));
    }
}

#[tokio::test]
async fn unsupported_native_browser_selection_is_rejected_without_echoing_private_paths() {
    let root = tempfile::tempdir().unwrap();
    for spec in [
        "firefox",
        "chrome:/private-browser-profile",
        "unknown:/private-browser-profile",
    ] {
        let output = output(binary(root.path()).args([
            "--extractor",
            "native",
            "--cookies-from-browser",
            spec,
            "-g",
            "https://example.com/image.jpg",
        ]))
        .await;
        assert!(!output.status.success(), "{spec}");
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!stderr.contains("/private-browser-profile"), "{stderr}");
    }
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[cfg(unix)]
#[tokio::test]
async fn compatibility_passes_raw_browser_spec_as_one_argument_and_clears_cookie_file() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let backend = root.path().join("fake backend");
    std::fs::write(&backend, b"#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$BROWSER_CLI_LOG\"\nprintf '%s' '[[3,\"https://example.com/image.jpg\",{}]]'\n").unwrap();
    std::fs::set_permissions(&backend, std::fs::Permissions::from_mode(0o700)).unwrap();
    let config = root.path().join("settings.json");
    std::fs::write(
        &config,
        br#"{"extractor":{"cookies":"missing.txt","twitter":{"cookies":"missing-x.txt"}}}"#,
    )
    .unwrap();
    let log = root.path().join("arguments.txt");
    let spec = "chrome+basictext:/synthetic profile::container";
    let url = "https://x.com/person/status/1";
    let output = output(
        binary(root.path())
            .arg("--native-config")
            .arg(config)
            .arg("--extractor-command")
            .arg(backend)
            .env("BROWSER_CLI_LOG", &log)
            .args(["--cookies-from-browser", spec, "-g", url]),
    )
    .await;
    assert!(output.status.success(), "{:?}", output);
    assert_eq!(output.stdout, b"https://example.com/image.jpg\n");
    let log = std::fs::read_to_string(log).unwrap();
    let arguments: Vec<_> = log.lines().collect();
    let browser = arguments
        .iter()
        .position(|value| *value == "--cookies-from-browser")
        .unwrap();
    let separator = arguments.iter().position(|value| *value == "--").unwrap();
    assert_eq!(arguments[browser + 1], spec);
    assert!(browser < separator);
    assert_eq!(arguments[separator + 1], url);
    assert!(!arguments.contains(&"--cookies"));
}
