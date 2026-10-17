use std::process::Command;

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_gallery-dl-rs"))
}

#[test]
fn direct_url_listing_does_not_need_backend() {
    let output = binary()
        .args([
            "--extractor-command",
            "/missing/backend",
            "-g",
            "https://example.com/a.jpg",
            "https://example.com/a.jpg",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "https://example.com/a.jpg\n"
    );
}

#[test]
fn missing_social_backend_is_actionable() {
    let output = binary()
        .args([
            "--extractor-command",
            "/missing/backend",
            "-g",
            "https://instagram.com/p/example/",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("Install gallery-dl")
    );
}

#[test]
fn partial_input_failure_keeps_successful_results_and_returns_failure() {
    let output = binary()
        .args([
            "-g",
            "https://example.com/page",
            "https://example.com/a.jpg",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "https://example.com/a.jpg\n"
    );
}

#[test]
fn zero_concurrency_is_rejected() {
    assert!(
        !binary()
            .args(["--jobs", "0", "https://example.com/a.jpg"])
            .output()
            .unwrap()
            .status
            .success()
    );
}

#[cfg(unix)]
#[test]
fn social_backend_is_invoked_without_a_shell() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let script = root.path().join("fake backend");
    std::fs::write(
        &script,
        r##"#!/bin/sh
test "$1" = '--config-ignore' || exit 1
printf '%s\n' '[[2,{}],[3,"https://cdn.example/photo.jpg",{"filename":"photo","extension":"jpg"}]]'
"##,
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    for url in [
        "https://instagram.com/p/abc/",
        "https://facebook.com/photo/?fbid=1",
        "https://x.com/person/status/1",
    ] {
        let output = binary()
            .arg("--extractor-command")
            .arg(&script)
            .args(["-g", url])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            "https://cdn.example/photo.jpg\n"
        );
    }
}
