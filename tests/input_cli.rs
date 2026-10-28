use std::{
    process::{Command, Stdio},
    time::Duration,
};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_gallery-dl-rs"))
}

#[test]
fn files_follow_positionals_in_order_and_preserve_source_bytes() {
    let root = tempfile::tempdir().unwrap();
    let first = root.path().join("first.txt");
    let second = root.path().join("second.txt");
    let contents = b"\xef\xbb\xbf  # comment\r\n\r\n \thttps://example.com/a.jpg#fragment \t# private comment\r\nhttps://example.com/b.jpg\r\n";
    std::fs::write(&first, contents).unwrap();
    std::fs::write(&second, b"https://example.com/c.jpg").unwrap();
    let output = binary()
        .args(["-g", "--extractor-command", "/missing/backend", "-i"])
        .arg(&first)
        .arg("https://example.com/positional.jpg")
        .arg("--input-file")
        .arg(&second)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "https://example.com/positional.jpg\nhttps://example.com/a.jpg#fragment\nhttps://example.com/b.jpg\nhttps://example.com/c.jpg\n"
    );
    assert_eq!(std::fs::read(&first).unwrap(), contents);
    assert_eq!(
        std::fs::read(&second).unwrap(),
        b"https://example.com/c.jpg"
    );
}

#[test]
fn malformed_lines_and_missing_files_report_failure_and_continue_privately() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("private-filename.txt");
    let mut contents = b"private-invalid-utf8\xff\n-o private-cookie-value\n".to_vec();
    contents.extend(std::iter::repeat_n(b'x', 64 * 1024 + 1));
    contents.extend_from_slice(b"private-oversized-value\nhttps://example.com/a.jpg\n");
    std::fs::write(&path, &contents).unwrap();
    let output = binary()
        .args(["-g", "-i"])
        .arg(root.path().join("private-missing-name"))
        .arg("-i")
        .arg(&path)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "https://example.com/a.jpg\n"
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("input file 1"), "{stderr}");
    for line in [1, 2, 3] {
        assert!(
            stderr.contains(&format!("input file 2, line {line}")),
            "{stderr}"
        );
    }
    assert!(!stderr.contains("private-"), "{stderr}");
    assert_eq!(std::fs::read(path).unwrap(), contents);
}

#[test]
fn input_files_are_optional_alternatives_to_positional_urls() {
    assert_eq!(binary().output().unwrap().status.code(), Some(2));
    let root = tempfile::tempdir().unwrap();
    let empty = root.path().join("empty.txt");
    std::fs::write(&empty, b"# no URLs\n").unwrap();
    let output = binary().args(["-g", "-i"]).arg(empty).output().unwrap();
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    let output = binary()
        .args(["-g", "-i", "-", "-i", "-"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("standard input can only be used once")
    );
}

#[tokio::test]
async fn stdin_publishes_urls_before_eof_then_continues_to_the_next_file() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("after.txt");
    std::fs::write(&path, b"https://example.com/after.jpg\n").unwrap();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_gallery-dl-rs"))
        .args(["-g", "-i", "-", "-i"])
        .arg(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    stdin
        .write_all(b"https://example.com/first.jpg\n")
        .await
        .unwrap();
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5), stdout.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(line, "https://example.com/first.jpg\n");
    assert!(child.try_wait().unwrap().is_none());
    drop(stdin);
    line.clear();
    tokio::time::timeout(Duration::from_secs(5), stdout.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(line, "https://example.com/after.jpg\n");
    let output = tokio::time::timeout(Duration::from_secs(5), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[tokio::test]
async fn interrupt_while_waiting_for_stdin_exits_promptly() {
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_gallery-dl-rs"))
        .args(["-g", "-i", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    stdin
        .write_all(b"https://example.com/first.jpg\n")
        .await
        .unwrap();
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5), stdout.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(line, "https://example.com/first.jpg\n");
    assert!(
        Command::new("/bin/kill")
            .args(["-INT", &child.id().unwrap().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let output = tokio::time::timeout(Duration::from_secs(5), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(130),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    drop(stdin);
}

#[test]
fn file_only_native_initialization_failure_keeps_later_direct_input() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("urls.txt");
    std::fs::write(
        &path,
        b"https://instagram.com/p/BA/\nhttps://example.com/image.jpg\n",
    )
    .unwrap();
    let output = binary()
        .args(["--extractor", "native", "--cookies"])
        .arg(root.path().join("missing-cookies.txt"))
        .args(["-g", "-i"])
        .arg(path)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "https://example.com/image.jpg\n"
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("initialization failed"));
}

#[test]
fn file_only_download_uses_category_destination_metadata_and_archive() {
    let root = tempfile::tempdir().unwrap();
    let url = "http://127.0.0.1:1/existing.jpg";
    let item =
        gallery_dl_rs::extract::direct(&gallery_dl_rs::extract::http_url(url).unwrap()).unwrap();
    let destination = root.path().join("saved");
    let saved = gallery_dl_rs::download::destination(&destination, &item);
    std::fs::create_dir_all(saved.parent().unwrap()).unwrap();
    std::fs::write(&saved, b"unchanged image").unwrap();
    let config = root.path().join("config.json");
    std::fs::write(
        &config,
        br#"{"extractor":{"direct":{"base-directory":"saved","archive":"history.sqlite"}}}"#,
    )
    .unwrap();
    let inputs = root.path().join("urls.txt");
    std::fs::write(&inputs, format!("{url}\n")).unwrap();
    let output = binary()
        .args(["--write-metadata", "--native-config"])
        .arg(&config)
        .arg("-i")
        .arg(&inputs)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::read(&saved).unwrap(), b"unchanged image");
    assert_eq!(
        std::fs::read_dir(saved.parent().unwrap()).unwrap().count(),
        2
    );
    let connection = rusqlite::Connection::open(root.path().join("history.sqlite")).unwrap();
    let count: i64 = connection
        .query_row("SELECT count(*) FROM completed_media", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);
    std::fs::remove_file(&saved).unwrap();
    let output = binary()
        .args(["--native-config"])
        .arg(config)
        .arg("-i")
        .arg(inputs)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!saved.exists());
}

/// Serves `body` for paths other than `/missing.jpg` until `count` requests.
fn serve(count: usize) -> (String, std::thread::JoinHandle<()>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        for _ in 0..count {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 4096];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = socket.read(&mut buffer).unwrap();
                assert!(n > 0);
                request.extend_from_slice(&buffer[..n]);
            }
            let response: &[u8] = if request.starts_with(b"GET /missing.jpg ") {
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            } else {
                b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\ndata"
            };
            socket.write_all(response).unwrap();
        }
    });
    (origin, handle)
}

#[test]
fn inline_options_apply_per_url_and_delete_removes_successful_lines() {
    let root = tempfile::tempdir().unwrap();
    let (origin, handle) = serve(3);
    let list = root.path().join("urls.txt");
    std::fs::write(
        &list,
        format!(
            "-filename=\"custom.{{extension}}\"\n{origin}/a.jpg\n{origin}/b.jpg\n{origin}/missing.jpg\n"
        ),
    )
    .unwrap();
    let output = binary()
        .args(["--retries", "0", "--jobs", "1", "-d"])
        .arg(root.path().join("out"))
        .arg("-x")
        .arg(&list)
        .output()
        .unwrap();
    handle.join().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(root.path().join("out/direct/custom.jpg").is_file());
    assert_eq!(
        std::fs::read_dir(root.path().join("out/direct"))
            .unwrap()
            .count(),
        2
    );
    assert_eq!(
        std::fs::read_to_string(&list).unwrap(),
        format!("{origin}/missing.jpg\n")
    );
}
