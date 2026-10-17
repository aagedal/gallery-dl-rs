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

#[test]
fn native_selection_rejects_unsupported_inputs_without_running_backend() {
    for url in [
        "https://x.com/person/status/1",
        "https://facebook.com/photo/?fbid=1",
        "https://instagram.com/person/",
    ] {
        let output = binary()
            .args([
                "--extractor",
                "native",
                "--extractor-command",
                "/missing/backend",
                "-g",
                url,
            ])
            .output()
            .unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains("--extractor compatibility"), "{stderr}");
        assert!(!stderr.contains("could not run"));
    }
    let output = binary()
        .args([
            "--extractor",
            "native",
            "--config",
            "settings.json",
            "-g",
            "https://example.com/a.jpg",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("--config is only supported")
    );
}

#[test]
fn native_mode_still_lists_direct_links_without_backend() {
    let output = binary()
        .args(["--extractor", "native", "-g", "https://example.com/a.jpg"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "https://example.com/a.jpg\n"
    );
}

#[test]
fn manifests_are_inspectable_but_not_downloaded() {
    let url = "http://127.0.0.1:1/stream.M3U8?token=example";
    let output = binary().args(["-j", url]).output().unwrap();
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json[0]["job"]["kind"], "manifest");
    assert_eq!(json[0]["job"]["protocol"], "hls");
    let root = tempfile::tempdir().unwrap();
    let output = binary()
        .arg("-d")
        .arg(root.path())
        .arg(url)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("companion yt-dlp-rs adapter")
    );
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[cfg(unix)]
#[test]
fn backend_listing_preserves_distinct_inline_handoffs_and_exact_duplicates() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let script = root.path().join("backend");
    std::fs::write(&script, r##"#!/bin/sh
printf '%s\n' '[[3,"ytdl:https://example.com/synthetic",{"_ytdl_manifest":"hls","_ytdl_manifest_data":"first"}],[3,"ytdl:https://example.com/synthetic",{"_ytdl_manifest":"hls","_ytdl_manifest_data":"second"}],[3,"ytdl:https://example.com/synthetic",{"_ytdl_manifest":"hls","_ytdl_manifest_data":"first"}]]'
"##).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let output = binary()
        .arg("--extractor-command")
        .arg(script)
        .args(["-g", "https://x.com/person/status/1"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "ytdl:https://example.com/synthetic\nytdl:https://example.com/synthetic\n"
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
