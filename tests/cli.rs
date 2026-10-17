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
