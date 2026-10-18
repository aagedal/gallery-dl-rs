use std::process::Command;

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_gallery-dl-rs"))
}

#[test]
fn metadata_is_opt_in_and_conflicts_with_inspection_modes() {
    let url = "http://127.0.0.1:1/image.jpg";
    let item =
        gallery_dl_rs::extract::direct(&gallery_dl_rs::extract::http_url(url).unwrap()).unwrap();
    let root = tempfile::tempdir().unwrap();
    let path = gallery_dl_rs::download::destination(root.path(), &item);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"existing").unwrap();
    assert!(
        binary()
            .arg("-d")
            .arg(root.path())
            .arg(url)
            .output()
            .unwrap()
            .status
            .success()
    );
    assert_eq!(
        std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
        1
    );
    let output = binary()
        .arg("-d")
        .arg(root.path())
        .args(["--write-metadata", url])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    assert_eq!(
        std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
        2
    );
    for mode in ["-j", "-g"] {
        let output = binary()
            .args(["--write-metadata", mode, url])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
    }
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
fn json_inspection_keeps_input_order_and_deduplicates_after_failures() {
    let output = binary()
        .args([
            "-j",
            "https://example.com/b.jpg",
            "https://example.com/page",
            "https://example.com/a.jpg",
            "https://example.com/b.jpg",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let items: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(items.as_array().unwrap().len(), 2);
    assert_eq!(items[0]["url"], "https://example.com/b.jpg");
    assert_eq!(items[1]["url"], "https://example.com/a.jpg");
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
        "https://x.com/person/lists",
        "https://facebook.com/person/videos/",
        "https://instagram.com/person/stories/",
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
fn twitter_expansion_config_is_forwarded_only_when_explicit_and_only_for_twitter() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let script = root.path().join("backend");
    let log = root.path().join("arguments");
    let config = root.path().join("native.json");
    std::fs::write(
        &script,
        r##"#!/bin/sh
printf '%s\n' "$@" > "$EXPANSION_ARGUMENTS"
printf '%s\n' '[[3,"https://cdn.example/photo.jpg",{"filename":"photo","extension":"jpg"}]]'
"##,
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let cases = [
        (serde_json::json!({}), None, None),
        (
            serde_json::json!({"extractor":{"twitter":{"retweets":"original","quoted":true}}}),
            Some("retweets=\"original\""),
            Some("quoted=true"),
        ),
        (
            serde_json::json!({"extractor":{"retweets":true,"quoted":true,"twitter":{"retweets":false,"quoted":false}}}),
            Some("retweets=false"),
            Some("quoted=false"),
        ),
        (
            serde_json::json!({"retweets":true,"quoted":true,"extractor":{"twitter":{"retweets":false,"quoted":false}}}),
            Some("retweets=true"),
            Some("quoted=true"),
        ),
    ];
    for (settings, retweets, quoted) in cases {
        std::fs::write(&config, serde_json::to_vec(&settings).unwrap()).unwrap();
        for (url, twitter) in [
            ("https://x.com/user/status/1", true),
            ("https://instagram.com/p/example/", false),
        ] {
            let output = binary()
                .env("EXPANSION_ARGUMENTS", &log)
                .arg("--native-config")
                .arg(&config)
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
            let arguments = std::fs::read_to_string(&log).unwrap();
            let arguments: Vec<_> = arguments.lines().collect();
            let forwarded: Vec<_> = arguments
                .windows(2)
                .filter(|pair| pair[0] == "--option")
                .map(|pair| pair[1])
                .filter(|value| value.starts_with("retweets=") || value.starts_with("quoted="))
                .collect();
            let expected: Vec<_> = if twitter {
                retweets.into_iter().chain(quoted).collect()
            } else {
                Vec::new()
            };
            assert_eq!(forwarded, expected);
            assert_eq!(&arguments[arguments.len() - 2..], &["--", url]);
        }
    }
}

#[cfg(unix)]
#[test]
fn twitter_profile_config_is_forwarded_only_when_explicit_and_only_for_twitter() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let script = root.path().join("backend");
    let log = root.path().join("arguments");
    let config = root.path().join("native.json");
    std::fs::write(
        &script,
        r##"#!/bin/sh
printf '%s\n' "$@" > "$PROFILE_ARGUMENTS"
printf '%s\n' '[[3,"https://cdn.example/photo.jpg",{"filename":"photo","extension":"jpg"}]]'
"##,
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    for (settings, expected) in [
        (serde_json::json!({}), vec![]),
        (
            serde_json::json!({"extractor":{"twitter":{"pinned":true,"replies":"self"}}}),
            vec!["pinned=true", "replies=\"self\""],
        ),
        (
            serde_json::json!({"extractor":{"pinned":true,"replies":true,"twitter":{"pinned":false,"replies":false}}}),
            vec!["pinned=false", "replies=false"],
        ),
        (
            serde_json::json!({"pinned":true,"replies":true,"extractor":{"twitter":{"pinned":false,"replies":false}}}),
            vec!["pinned=true", "replies=true"],
        ),
    ] {
        std::fs::write(&config, serde_json::to_vec(&settings).unwrap()).unwrap();
        for (url, twitter) in [
            ("https://x.com/user/tweets", true),
            ("https://instagram.com/p/example/", false),
        ] {
            let output = binary()
                .env("PROFILE_ARGUMENTS", &log)
                .arg("--native-config")
                .arg(&config)
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
            let arguments = std::fs::read_to_string(&log).unwrap();
            let arguments: Vec<_> = arguments.lines().collect();
            let forwarded: Vec<_> = arguments
                .windows(2)
                .filter(|pair| pair[0] == "--option")
                .map(|pair| pair[1])
                .filter(|value| value.starts_with("pinned=") || value.starts_with("replies="))
                .collect();
            assert_eq!(forwarded, if twitter { expected.clone() } else { vec![] });
            assert_eq!(&arguments[arguments.len() - 2..], &["--", url]);
        }
    }

    std::fs::remove_file(&log).unwrap();
    for settings in [
        serde_json::json!({"pinned":"PRIVATE-PINNED-DETAIL"}),
        serde_json::json!({"extractor":{"twitter":{"replies":"PRIVATE-REPLY-DETAIL"}}}),
    ] {
        std::fs::write(&config, serde_json::to_vec(&settings).unwrap()).unwrap();
        let output = binary()
            .env("PROFILE_ARGUMENTS", &log)
            .arg("--native-config")
            .arg(&config)
            .arg("--extractor-command")
            .arg(&script)
            .args(["-g", "https://x.com/user/with_replies"])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("PRIVATE-"));
        assert!(!log.exists());
    }
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

#[cfg(unix)]
mod cancellation {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        path::Path,
        process::{Child, Output, Stdio},
        thread,
        time::{Duration, Instant},
    };

    struct Running(Option<Child>);
    impl Drop for Running {
        fn drop(&mut self) {
            if let Some(child) = &mut self.0 {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    fn wait_until(mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready() {
            assert!(Instant::now() < deadline, "condition timed out");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn interrupt(mut child: Running) -> Output {
        let process = child.0.as_mut().unwrap();
        assert!(
            Command::new("kill")
                .args(["-INT", &process.id().to_string()])
                .status()
                .unwrap()
                .success()
        );
        wait_until(|| process.try_wait().unwrap().is_some());
        let output = child.0.take().unwrap().wait_with_output().unwrap();
        assert_eq!(
            output.status.code(),
            Some(130),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stderr).contains("cancelled"));
        output
    }

    fn file_count(path: &Path) -> usize {
        std::fs::read_dir(path)
            .map(|entries| entries.count())
            .unwrap_or(0)
    }

    fn accept_request(listener: &TcpListener) -> std::net::TcpStream {
        let mut connection = None;
        wait_until(|| match listener.accept() {
            Ok((socket, _)) => {
                connection = Some(socket);
                true
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => false,
            Err(e) => panic!("{e}"),
        });
        let mut socket = connection.unwrap();
        // Accepted sockets can inherit nonblocking mode on macOS.
        socket.set_nonblocking(false).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut request = Vec::new();
        let mut buffer = [0; 1024];
        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
            let count = socket.read(&mut buffer).unwrap();
            assert_ne!(count, 0);
            request.extend_from_slice(&buffer[..count]);
        }
        socket
    }

    fn waiting_backend(root: &Path, response: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let script = root.join("backend");
        // Keep the directly owned shell alive so cancellation can terminate it.
        std::fs::write(&script, format!(
            "#!/bin/sh\nprintf '%s' \"$$\" > \"$0.pid\"\nwhile [ ! -f \"$0.release\" ]; do sleep 0.02; done\nprintf '%s' '{}'\n", response
        )).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        script
    }

    #[test]
    fn downloads_publish_while_later_extraction_waits_and_failures_continue() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/photo.jpg", listener.local_addr().unwrap());
        let root = tempfile::tempdir().unwrap();
        let script = waiting_backend(root.path(), "[]");
        let item = gallery_dl_rs::extract::direct(&gallery_dl_rs::extract::http_url(&url).unwrap())
            .unwrap();
        let destination = root.path().join("downloads");
        let saved = gallery_dl_rs::download::destination(&destination, &item);
        let mut child = Running(Some(
            binary()
                .arg("-d")
                .arg(&destination)
                .arg("--extractor-command")
                .arg(script)
                .args([
                    "--jobs",
                    "2",
                    "--retries",
                    "0",
                    &url,
                    "https://x.com/user/status/1",
                    "https://example.com/page",
                    &url,
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        ));
        let mut socket = accept_request(&listener);
        wait_until(|| root.path().join("backend.pid").exists());
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nphoto")
            .unwrap();
        drop(socket);
        // Completion must be polled even with the next extractor still pending.
        wait_until(|| saved.exists());
        assert!(child.0.as_mut().unwrap().try_wait().unwrap().is_none());
        assert_eq!(std::fs::read(saved).unwrap(), b"photo");
        std::fs::write(root.path().join("backend.release"), b"").unwrap();
        wait_until(|| child.0.as_mut().unwrap().try_wait().unwrap().is_some());
        let output = child.0.take().unwrap().wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(1));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("2 operation(s) failed"), "{stderr}");
        assert_eq!(stderr.matches("downloaded ").count(), 1, "{stderr}");
        // Duplicate input must not even issue another request after extraction fails.
        assert!(matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock));
    }

    #[test]
    fn full_download_capacity_defers_the_next_extraction() {
        use std::os::unix::fs::PermissionsExt;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/photo.jpg", listener.local_addr().unwrap());
        let root = tempfile::tempdir().unwrap();
        let script = root.path().join("backend");
        // At jobs=1 the first download must publish before another input is pulled.
        std::fs::write(&script, "#!/bin/sh\ntest -f \"$PIPELINE_SAVED\" || exit 7\nprintf '%s' '[[3,\"https://example.com/unsupported.m3u8\",{}]]'\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let item = gallery_dl_rs::extract::direct(&gallery_dl_rs::extract::http_url(&url).unwrap())
            .unwrap();
        let destination = root.path().join("downloads");
        let saved = gallery_dl_rs::download::destination(&destination, &item);
        let mut child = Running(Some(
            binary()
                .arg("-d")
                .arg(&destination)
                .arg("--extractor-command")
                .arg(script)
                .env("PIPELINE_SAVED", saved)
                .args(["--jobs", "1", &url, "https://x.com/user/status/1"])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        ));
        let mut socket = accept_request(&listener);
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nphoto")
            .unwrap();
        drop(socket);
        wait_until(|| child.0.as_mut().unwrap().try_wait().unwrap().is_some());
        let output = child.0.take().unwrap().wait_with_output().unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1));
        assert!(stderr.contains("companion yt-dlp-rs adapter"), "{stderr}");
        assert!(!stderr.contains("extraction failed"), "{stderr}");
        assert!(stderr.contains("1 operation(s) failed"), "{stderr}");
    }

    #[test]
    fn ctrl_c_cancels_extraction_and_download_together() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/photo.jpg", listener.local_addr().unwrap());
        let root = tempfile::tempdir().unwrap();
        let script = waiting_backend(root.path(), "[]");
        let destination = root.path().join("downloads");
        let child = Running(Some(
            binary()
                .arg("-d")
                .arg(&destination)
                .arg("--extractor-command")
                .arg(script)
                .args(["--jobs", "2", &url, "https://x.com/user/status/1"])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        ));
        let mut socket = accept_request(&listener);
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 1000000\r\nConnection: close\r\n\r\npartial",
            )
            .unwrap();
        wait_until(|| file_count(&destination.join("direct")) == 1);
        let pid_file = root.path().join("backend.pid");
        wait_until(|| std::fs::read_to_string(&pid_file).is_ok_and(|s| !s.is_empty()));
        let pid = std::fs::read_to_string(pid_file).unwrap();
        interrupt(child);
        assert_eq!(file_count(&destination.join("direct")), 0);
        wait_until(|| {
            let state = Command::new("ps")
                .args(["-o", "stat=", "-p", &pid])
                .stderr(Stdio::null())
                .output()
                .unwrap();
            let state = String::from_utf8_lossy(&state.stdout);
            state.trim().is_empty() || state.trim().starts_with('Z')
        });
    }

    #[test]
    fn ctrl_c_removes_partial_downloads() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/photo.jpg", listener.local_addr().unwrap());
        let root = tempfile::tempdir().unwrap();
        let child = Running(Some(
            binary()
                .arg("-d")
                .arg(root.path())
                .arg("--write-metadata")
                .arg(url)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        ));
        let mut connection = None;
        wait_until(|| match listener.accept() {
            Ok((socket, _)) => {
                connection = Some(socket);
                true
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => false,
            Err(e) => panic!("{e}"),
        });
        let mut socket = connection.unwrap();
        socket.set_nonblocking(false).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut request = Vec::new();
        let mut buffer = [0; 1024];
        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
            let count = socket.read(&mut buffer).unwrap();
            assert_ne!(count, 0);
            request.extend_from_slice(&buffer[..count]);
        }
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 1000000\r\nConnection: close\r\n\r\npartial",
            )
            .unwrap();
        let category = root.path().join("direct");
        wait_until(|| file_count(&category) == 1);
        interrupt(child);
        assert_eq!(file_count(&category), 0);
    }

    #[test]
    fn ctrl_c_stops_compatibility_extraction() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let script = root.path().join("backend");
        // The exec keeps the fixture process as the directly owned child.
        std::fs::write(
            &script,
            "#!/bin/sh\nprintf '%s' \"$$\" > \"$0.pid\"\nexec sleep 30\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let child = Running(Some(
            binary()
                .arg("--extractor-command")
                .arg(&script)
                .args(["-g", "https://x.com/user/status/1"])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        ));
        let pid_file = root.path().join("backend.pid");
        wait_until(|| std::fs::read_to_string(&pid_file).is_ok_and(|s| !s.is_empty()));
        let pid = std::fs::read_to_string(pid_file).unwrap();
        interrupt(child);
        wait_until(|| {
            // A killed orphan can remain a zombie under a container's PID 1.
            // It is terminated even while kill -0 still finds the process ID.
            let state = Command::new("ps")
                .args(["-o", "stat=", "-p", &pid])
                .stderr(Stdio::null())
                .output()
                .unwrap();
            let state = String::from_utf8_lossy(&state.stdout);
            state.trim().is_empty() || state.trim().starts_with('Z')
        });
    }
}

#[test]
fn native_twitter_reports_authentication_without_launching_backend_and_continues_inputs() {
    for source in [
        "https://x.com/person/status/123",
        "https://x.com/person",
        "https://x.com/id:42",
        "https://x.com/search?q=rust&f=live",
        "https://x.com/hashtag/rust",
        "https://x.com/person/media",
        "https://x.com/id:42/media",
        "https://x.com/person/tweets",
        "https://x.com/person/with_replies",
        "https://x.com/person/highlights",
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
        let output = binary()
            .args([
                "--extractor",
                "native",
                "--extractor-command",
                "/missing/backend",
                "-g",
                source,
                "https://example.com/image.jpg",
            ])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            "https://example.com/image.jpg\n"
        );
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            stderr.contains("auth_token") && stderr.contains("ct0"),
            "{stderr}"
        );
        assert!(!stderr.contains("could not run"));
    }
}

#[test]
fn native_session_initialization_failure_does_not_block_unrelated_direct_inputs() {
    let root = tempfile::tempdir().unwrap();
    for url in [
        "https://x.com/person/status/123",
        "https://x.com/person",
        "https://x.com/id:42",
        "https://x.com/search?q=rust&f=live",
        "https://x.com/hashtag/rust",
        "https://x.com/person/media",
        "https://x.com/person/tweets",
        "https://x.com/person/with_replies",
        "https://x.com/person/highlights",
        "https://x.com/person/likes",
        "https://x.com/id:42/likes",
        "https://x.com/i/bookmarks",
        "https://x.com/i/lists/42",
        "https://x.com/home",
        "https://x.com/home/following",
        "https://x.com/home/foryou",
        "https://x.com/home/for-you",
        "https://x.com/home/for_you",
        "https://instagram.com/p/BA/",
        "https://instagram.com/person/",
        "https://facebook.com/photo/?fbid=1",
    ] {
        let output = binary()
            .arg("--cookies")
            .arg(root.path().join("missing-cookies.txt"))
            .args([
                "--extractor",
                "native",
                "-g",
                url,
                "https://example.com/image.jpg",
            ])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            "https://example.com/image.jpg\n"
        );
        assert!(String::from_utf8_lossy(&output.stderr).contains("initialization failed"));
    }
}

#[cfg(unix)]
#[test]
fn twitter_search_settings_are_forwarded_sparsely_with_resolved_scope() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let script = root.path().join("backend");
    let log = root.path().join("arguments");
    let config = root.path().join("native.json");
    std::fs::write(
        &script,
        r##"#!/bin/sh
printf '%s\n' "$@" > "$SEARCH_ARGUMENTS"
printf '%s\n' '[[3,"https://cdn.example/photo.jpg",{"filename":"photo","extension":"jpg"}]]'
"##,
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    for (settings, expected) in [
        (serde_json::json!({}), vec![]),
        (
            serde_json::json!({"extractor":{"twitter":{"strategy":"tweets","search-pagination":"dt","search-results":"live","limit":75,"search-limit":100,"search-stop":0}}}),
            vec![
                "strategy=\"tweets\"",
                "search-pagination=\"date\"",
                "search-results=\"Latest\"",
                "limit=75",
                "search-limit=100",
                "search-stop=0",
            ],
        ),
        (
            serde_json::json!({"extractor":{"strategy":"media","search-limit":1,"twitter":{"strategy":"with_replies","search-limit":20,"search-stop":"auto"}}}),
            vec![
                "strategy=\"with_replies\"",
                "search-limit=20",
                "search-stop=3",
            ],
        ),
        (
            serde_json::json!({"strategy":"auto","search-pagination":"cursor","search-results":"list","search-stop":128,"extractor":{"twitter":{"strategy":"media","search-stop":0}}}),
            vec![
                "strategy=\"auto\"",
                "search-pagination=\"cursor\"",
                "search-results=\"Lists\"",
                "search-stop=128",
            ],
        ),
    ] {
        std::fs::write(&config, serde_json::to_vec(&settings).unwrap()).unwrap();
        for (url, twitter) in [
            ("https://x.com/user", true),
            ("https://x.com/search?q=rust", true),
            ("https://x.com/hashtag/rust", true),
            ("https://instagram.com/p/example/", false),
        ] {
            let output = binary()
                .env("SEARCH_ARGUMENTS", &log)
                .arg("--native-config")
                .arg(&config)
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
            let arguments = std::fs::read_to_string(&log).unwrap();
            let arguments: Vec<_> = arguments.lines().collect();
            let forwarded: Vec<_> = arguments
                .windows(2)
                .filter(|pair| pair[0] == "--option")
                .map(|pair| pair[1])
                .filter(|value| {
                    value.starts_with("strategy=")
                        || value.starts_with("search-")
                        || value.starts_with("limit=")
                })
                .collect();
            assert_eq!(forwarded, if twitter { expected.clone() } else { vec![] });
            assert_eq!(&arguments[arguments.len() - 2..], &["--", url]);
        }
    }
    std::fs::remove_file(&log).unwrap();
    for settings in [
        serde_json::json!({"strategy":"PRIVATE-STRATEGY"}),
        serde_json::json!({"extractor":{"twitter":{"search-pagination":"PRIVATE-PAGINATION"}}}),
        serde_json::json!({"search-results":{"PRIVATE-KEY":"PRIVATE-VALUE"}}),
        serde_json::json!({"limit":0}),
        serde_json::json!({"limit":101}),
        serde_json::json!({"search-limit":0}),
        serde_json::json!({"search-stop":129}),
    ] {
        std::fs::write(&config, serde_json::to_vec(&settings).unwrap()).unwrap();
        let output = binary()
            .env("SEARCH_ARGUMENTS", &log)
            .arg("--native-config")
            .arg(&config)
            .arg("--extractor-command")
            .arg(&script)
            .args(["-g", "https://x.com/search?q=rust"])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("PRIVATE-"));
        assert!(!log.exists());
    }
}

#[cfg(unix)]
#[test]
fn twitter_status_config_forwarding_preserves_precedence_and_category_isolation() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let script = root.path().join("backend");
    let log = root.path().join("arguments");
    let config = root.path().join("native.json");
    std::fs::write(
        &script,
        r##"#!/bin/sh
printf '%s\n' "$@" > "$STATUS_ARGUMENTS"
printf '%s\n' '[[3,"https://cdn.example/photo.jpg",{"filename":"photo","extension":"jpg"}]]'
"##,
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    for (settings, expected) in [
        (serde_json::json!({}), vec![]),
        (
            serde_json::json!({"extractor":{"twitter":{"tweet-endpoint":"restid","conversations":"accessible"}}}),
            vec!["tweet-endpoint=\"restid\"", "conversations=\"accessible\""],
        ),
        (
            serde_json::json!({"extractor":{"tweet-endpoint":"auto","conversations":true,"twitter":{"tweet-endpoint":"detail","conversations":false}}}),
            vec!["tweet-endpoint=\"detail\"", "conversations=false"],
        ),
        (
            serde_json::json!({"tweet-endpoint":"auto","conversations":false,"extractor":{"twitter":{"tweet-endpoint":"detail","conversations":"accessible"}}}),
            vec!["tweet-endpoint=\"auto\"", "conversations=false"],
        ),
        (
            serde_json::json!({"conversations":true}),
            vec!["conversations=true"],
        ),
    ] {
        std::fs::write(&config, serde_json::to_vec(&settings).unwrap()).unwrap();
        for (url, twitter) in [
            ("https://x.com/user/status/1", true),
            ("https://instagram.com/p/example/", false),
            ("https://facebook.com/photo/?fbid=1", false),
        ] {
            let result = binary()
                .env("STATUS_ARGUMENTS", &log)
                .arg("--native-config")
                .arg(&config)
                .arg("--extractor-command")
                .arg(&script)
                .args(["-g", url])
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            let arguments = std::fs::read_to_string(&log).unwrap();
            let arguments: Vec<_> = arguments.lines().collect();
            let forwarded: Vec<_> = arguments
                .windows(2)
                .filter(|pair| pair[0] == "--option")
                .map(|pair| pair[1])
                .filter(|value| {
                    value.starts_with("tweet-endpoint=") || value.starts_with("conversations=")
                })
                .collect();
            assert_eq!(forwarded, if twitter { expected.clone() } else { vec![] });
            assert_eq!(&arguments[arguments.len() - 2..], &["--", url]);
        }
    }
    std::fs::remove_file(&log).unwrap();
    for settings in [
        serde_json::json!({"tweet-endpoint":"PRIVATE-SETTING"}),
        serde_json::json!({"extractor":{"twitter":{"conversations":"PRIVATE-SETTING"}}}),
    ] {
        std::fs::write(&config, serde_json::to_vec(&settings).unwrap()).unwrap();
        let result = binary()
            .env("STATUS_ARGUMENTS", &log)
            .arg("--native-config")
            .arg(&config)
            .arg("--extractor-command")
            .arg(&script)
            .args(["-g", "https://x.com/user/status/1"])
            .output()
            .unwrap();
        assert!(!result.status.success());
        assert!(result.stdout.is_empty());
        assert!(!String::from_utf8_lossy(&result.stderr).contains("PRIVATE-SETTING"));
        assert!(!log.exists());
    }
}

#[cfg(unix)]
#[test]
fn twitter_thread_options_forward_scopes_and_preserve_duplicates_when_disabled() {
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let backend = root.path().join("backend");
    let log = root.path().join("arguments");
    let config = root.path().join("native.json");
    std::fs::write(&backend, r##"#!/bin/sh
printf '%s\n' "$@" > "$THREAD_ARGUMENTS"
printf '%s\n' '[[3,"https://cdn.example/first.jpg",{"filename":"first","extension":"jpg"}],[3,"https://cdn.example/sibling.jpg",{"filename":"sibling","extension":"jpg"}],[3,"https://cdn.example/first.jpg",{"filename":"first","extension":"jpg"}]]'
"##).unwrap();
    std::fs::set_permissions(&backend, std::fs::Permissions::from_mode(0o700)).unwrap();
    for (settings, expected) in [
        (json!({}), vec![]),
        (
            json!({"extractor":{"twitter":{"expand":true,"unique":false}}}),
            vec!["expand=true", "unique=false"],
        ),
        (
            json!({"extractor":{"expand":false,"unique":false,"twitter":{"expand":true,"unique":true}}}),
            vec!["expand=true", "unique=true"],
        ),
        (
            json!({"expand":false,"unique":false,"extractor":{"twitter":{"expand":true,"unique":true}}}),
            vec!["expand=false", "unique=false"],
        ),
    ] {
        std::fs::write(&config, serde_json::to_vec(&settings).unwrap()).unwrap();
        for (url, twitter) in [
            ("https://x.com/person/status/1", true),
            ("https://instagram.com/p/example/", false),
        ] {
            let output = binary()
                .env("THREAD_ARGUMENTS", &log)
                .arg("--native-config")
                .arg(&config)
                .arg("--extractor-command")
                .arg(&backend)
                .args(["-g", url, url])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let arguments = std::fs::read_to_string(&log).unwrap();
            let args: Vec<_> = arguments.lines().collect();
            let forwarded: Vec<_> = args
                .windows(2)
                .filter(|pair| pair[0] == "--option")
                .map(|pair| pair[1])
                .filter(|value| value.starts_with("expand=") || value.starts_with("unique="))
                .collect();
            assert_eq!(forwarded, if twitter { expected.clone() } else { vec![] });
            let one = [
                "https://cdn.example/first.jpg",
                "https://cdn.example/sibling.jpg",
            ];
            let duplicates = [
                "https://cdn.example/first.jpg",
                "https://cdn.example/sibling.jpg",
                "https://cdn.example/first.jpg",
                "https://cdn.example/first.jpg",
                "https://cdn.example/sibling.jpg",
                "https://cdn.example/first.jpg",
            ];
            let urls: Vec<_> = std::str::from_utf8(&output.stdout)
                .unwrap()
                .lines()
                .collect();
            assert_eq!(
                urls,
                if twitter && expected.contains(&"unique=false") {
                    duplicates.as_slice()
                } else {
                    one.as_slice()
                }
            );
        }
    }
    std::fs::remove_file(&log).unwrap();
    for field in ["expand", "unique"] {
        std::fs::write(
            &config,
            serde_json::to_vec(&json!({"extractor":{"twitter":{(field):"PRIVATE-THREAD-VALUE"}}}))
                .unwrap(),
        )
        .unwrap();
        let output = binary()
            .env("THREAD_ARGUMENTS", &log)
            .arg("--native-config")
            .arg(&config)
            .arg("--extractor-command")
            .arg(&backend)
            .args(["-g", "https://x.com/person/status/1"])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("PRIVATE-THREAD-VALUE"));
        assert!(!log.exists());
    }
}
