//! Scripted loopback coverage of fallback sequencing and private transport boundaries.
use gallery_dl_rs::{
    archive::{Archive, ArchiveOutcome},
    download::{self, DownloadOptions, Outcome},
    extract::{self, ManifestProtocol, Media, MediaJob},
    size::SizeLimits,
};
use serde_json::json;
use std::{
    io::{Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

struct Response {
    status: &'static str,
    headers: String,
    body: Vec<u8>,
    stall: Duration,
    response_gate: Option<Arc<AtomicBool>>,
}
fn reply(status: &'static str, body: &[u8]) -> Response {
    Response {
        status,
        headers: format!("Content-Length: {}\r\n", body.len()),
        body: body.into(),
        stall: Duration::from_millis(80),
        response_gate: None,
    }
}
fn partial() -> Response {
    Response {
        status: "200 OK",
        headers: "Content-Length: 8192\r\nETag: \"fallback-version\"\r\n".into(),
        body: vec![b'a'; 4096],
        stall: Duration::from_millis(80),
        response_gate: None,
    }
}
fn remainder() -> Response {
    Response { status: "206 Partial Content", headers: "Content-Length: 4096\r\nContent-Range: bytes 4096-8191/8192\r\nETag: \"fallback-version\"\r\n".into(), body: vec![b'b'; 4096], stall: Duration::from_millis(80), response_gate: None }
}
struct Server {
    base: String,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    root: Arc<Mutex<Option<PathBuf>>>,
    handle: Option<thread::JoinHandle<()>>,
}
impl Server {
    fn new(script: Vec<Response>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let root = Arc::new(Mutex::new(None::<PathBuf>));
        let watched = root.clone();
        let handle = thread::spawn(move || {
            for response in script {
                let deadline = Instant::now() + Duration::from_secs(8);
                let mut socket = loop {
                    if stopped.load(Ordering::Relaxed) || Instant::now() >= deadline {
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
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                socket
                    .set_write_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 1024];
                while request.len() < 32768 && !request.windows(4).any(|bytes| bytes == b"\r\n\r\n")
                {
                    match socket.read(&mut buffer) {
                        Ok(0) | Err(_) => break,
                        Ok(count) => request.extend_from_slice(&buffer[..count]),
                    }
                }
                captured
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&request).into_owned());
                if let Some(gate) = &response.response_gate {
                    let deadline = Instant::now() + Duration::from_secs(3);
                    while !gate.load(Ordering::SeqCst)
                        && !stopped.load(Ordering::Relaxed)
                        && Instant::now() < deadline
                    {
                        thread::sleep(Duration::from_millis(2));
                    }
                }
                let _ = write!(
                    socket,
                    "HTTP/1.1 {}\r\nConnection: close\r\n{}\r\n",
                    response.status, response.headers
                );
                let _ = socket.write_all(&response.body);
                let _ = socket.flush();
                if response.headers.contains("Content-Length: 8192")
                    && let Some(root) = watched.lock().unwrap().clone()
                {
                    let deadline = Instant::now() + Duration::from_secs(2);
                    while Instant::now() < deadline && !stopped.load(Ordering::Relaxed) {
                        let written = std::fs::read_dir(root.join("direct/.gallery-dl-rs"))
                            .is_ok_and(|entries| {
                                entries.filter_map(Result::ok).any(|entry| {
                                    entry.path().extension().is_some_and(|ext| ext == "part")
                                        && entry
                                            .metadata()
                                            .is_ok_and(|metadata| metadata.len() == 4096)
                                })
                            });
                        if written {
                            break;
                        }
                        thread::sleep(Duration::from_millis(2));
                    }
                }
                // Briefly retain the socket while complete-body writes finish.
                let deadline = Instant::now() + response.stall;
                while Instant::now() < deadline && !stopped.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(2));
                }
            }
        });
        Self {
            base,
            requests,
            stop,
            root,
            handle: Some(handle),
        }
    }
    fn watch(&self, root: &Path) {
        *self.root.lock().unwrap() = Some(root.into());
    }
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }
    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.handle.take().unwrap().join().unwrap();
    }
}
fn item(primary: &str, fallbacks: serde_json::Value) -> Media {
    let mut media = extract::direct(&extract::http_url(primary).unwrap()).unwrap();
    media.handoff.insert("_fallback".into(), fallbacks);
    media
}
fn client() -> download::DownloadClient {
    download::client(Duration::from_secs(2)).unwrap()
}

#[tokio::test]
async fn fallback_follows_retry_exhaustion_and_preserves_destination() {
    let server = Server::new(vec![
        reply("503 Unavailable", b""),
        reply("503 Unavailable", b""),
        reply("200 OK", b"saved"),
    ]);
    let media = item(
        &server.url("/primary.jpg"),
        json!([server.url("/backup.png")]),
    );
    let root = tempfile::tempdir().unwrap();
    let outcome = download::download(&client(), root.path(), &media, 1)
        .await
        .unwrap();
    assert_eq!(
        outcome.saved_path().unwrap(),
        download::destination(root.path(), &media)
    );
    assert_eq!(
        std::fs::read(outcome.saved_path().unwrap()).unwrap(),
        b"saved"
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    assert!(requests[0].starts_with("GET /primary.jpg "), "{requests:?}");
    assert!(requests[1].starts_with("GET /primary.jpg "), "{requests:?}");
    assert!(requests[2].starts_with("GET /backup.png "), "{requests:?}");
}

#[tokio::test]
async fn fallback_is_lazy_disabled_and_deduplicated() {
    let success = Server::new(vec![reply("200 OK", b"ok")]);
    let media = item(
        &success.url("/primary.jpg"),
        json!({"secret": "invalid-unused"}),
    );
    let root = tempfile::tempdir().unwrap();
    assert!(
        download::download(&client(), root.path(), &media, 0)
            .await
            .is_ok()
    );
    let disabled = Server::new(vec![reply("404 Not Found", b"")]);
    let media = item(
        &disabled.url("/primary.jpg"),
        json!([disabled.url("/backup.jpg")]),
    );
    assert!(
        download::download(&client().with_fallback(false), root.path(), &media, 0)
            .await
            .is_err()
    );
    assert_eq!(disabled.requests().len(), 1);
    let repeated = Server::new(vec![
        reply("404 Not Found", b""),
        reply("404 Not Found", b""),
        reply("200 OK", b"ok"),
    ]);
    let media = item(
        &repeated.url("/primary.jpg"),
        json!([
            repeated.url("/primary.jpg"),
            repeated.url("/first.jpg"),
            repeated.url("/first.jpg"),
            repeated.url("/last.jpg")
        ]),
    );
    assert!(
        download::download(&client(), root.path(), &media, 0)
            .await
            .is_ok()
    );
    assert_eq!(repeated.requests().len(), 3);
}

#[tokio::test]
async fn unsupported_jobs_use_only_safe_direct_http_fallbacks() {
    // A yt-dlp that always fails, so the job advances to its fallbacks.
    let tools = tempfile::tempdir().unwrap();
    let failing = tools.path().join("yt-dlp");
    std::fs::write(&failing, "#!/bin/sh\nexit 1\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&failing, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    for (manifest, ytdl) in [(false, None), (true, None), (false, Some(&failing))] {
        let client = client().with_ytdl(ytdl.map(|path| path.as_path()));
        let server = Server::new(vec![reply("200 OK", b"video")]);
        let mut media = item(
            "https://unrequested.invalid/video.mp4",
            json!([
                "file:/private/secret",
                "data:text/plain,secret",
                "ytdl:https://secret.invalid/",
                "https://secret.invalid/a.m3u8",
                server.url("/video.mp4")
            ]),
        );
        media.job = if manifest {
            MediaJob::Manifest {
                protocol: ManifestProtocol::Hls,
                base_url: media.url.clone(),
                inline_data: None,
            }
        } else {
            MediaJob::VideoPage {
                page_url: media.url.clone(),
            }
        };
        let root = tempfile::tempdir().unwrap();
        assert!(
            download::download(&client, root.path(), &media, 0)
                .await
                .is_ok()
        );
        assert_eq!(server.requests().len(), 1);
    }
    let server = Server::new(vec![reply("404 Not Found", b"")]);
    let media = item(
        &server.url("/primary.jpg"),
        json!({"secret": "never-print"}),
    );
    let root = tempfile::tempdir().unwrap();
    let error = download::download(&client(), root.path(), &media, 0)
        .await
        .unwrap_err()
        .to_string();
    assert!(!error.contains("never-print"));
}

#[tokio::test]
async fn size_filtering_and_filesystem_failures_do_not_try_fallbacks() {
    let server = Server::new(vec![reply("200 OK", b"oversize")]);
    let media = item(
        &server.url("/primary.jpg"),
        json!([server.url("/backup.jpg")]),
    );
    let root = tempfile::tempdir().unwrap();
    let result = download::download_with_options(
        &client(),
        root.path(),
        &media,
        DownloadOptions {
            limits: SizeLimits::new(None, Some(1)).unwrap(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(matches!(result, Outcome::Filtered(_)));
    assert_eq!(server.requests().len(), 1);
    assert!(!download::destination(root.path(), &media).exists());
    let path = download::destination(root.path(), &media);
    std::fs::create_dir_all(&path).unwrap();
    assert!(
        download::download(&client(), root.path(), &media, 0)
            .await
            .is_err()
    );
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn fallback_same_origin_keeps_headers_and_cross_origin_clears_all_item_headers() {
    let first = Server::new(vec![
        reply("404 Not Found", b""),
        reply("404 Not Found", b""),
    ]);
    let second = Server::new(vec![reply("200 OK", b"ok")]);
    let mut media = item(
        &first.url("/primary.jpg"),
        json!([first.url("/same.jpg"), second.url("/cross.jpg")]),
    );
    for (key, value) in [
        ("Cookie", "secret-cookie"),
        ("Authorization", "Bearer secret"),
        ("X-Custom-Token", "private"),
        ("Referer", "https://private.invalid/"),
        ("User-Agent", "private-agent"),
    ] {
        media.headers.insert(key.into(), value.into());
    }
    let root = tempfile::tempdir().unwrap();
    download::download(&client(), root.path(), &media, 0)
        .await
        .unwrap();
    for request in first.requests() {
        assert!(
            request
                .to_ascii_lowercase()
                .contains("cookie: secret-cookie\r\n")
        );
    }
    let request = second.requests()[0].to_ascii_lowercase();
    for private in [
        "secret-cookie",
        "bearer secret",
        "x-custom-token",
        "private.invalid",
        "private-agent",
    ] {
        assert!(!request.contains(private), "{request}");
    }
}

#[tokio::test]
async fn cross_origin_redirects_carry_only_public_headers() {
    let cdn = Server::new(vec![reply("200 OK", b"moved")]);
    let origin = Server::new(vec![Response {
        status: "302 Found",
        headers: format!(
            "Location: {}\r\nContent-Length: 0\r\n",
            cdn.url("/moved.jpg")
        ),
        body: Vec::new(),
        stall: Duration::from_millis(80),
        response_gate: None,
    }]);
    let mut media = item(&origin.url("/file.jpg"), json!([]));
    for (key, value) in [
        ("Cookie", "secret-cookie"),
        ("Authorization", "Bearer secret"),
        ("X-Custom-Token", "private"),
        ("Referer", "https://site.invalid/post"),
        ("User-Agent", "public-agent"),
    ] {
        media.headers.insert(key.into(), value.into());
    }
    let root = tempfile::tempdir().unwrap();
    let result = download::download(&client(), root.path(), &media, 0)
        .await
        .unwrap();
    // The destination keeps the requested file's name.
    let saved = result.saved_path().unwrap();
    assert!(
        saved.to_string_lossy().ends_with("_file.jpg"),
        "{}",
        saved.display()
    );
    assert_eq!(std::fs::read(saved).unwrap(), b"moved");
    let first = origin.requests()[0].to_ascii_lowercase();
    assert!(first.contains("cookie: secret-cookie\r\n"), "{first}");
    let moved = cdn.requests()[0].to_ascii_lowercase();
    assert!(moved.starts_with("get /moved.jpg "), "{moved}");
    for private in ["secret-cookie", "bearer secret", "x-custom-token"] {
        assert!(!moved.contains(private), "{moved}");
    }
    assert!(moved.contains("referer: https://site.invalid/post\r\n"));
    assert!(moved.contains("user-agent: public-agent\r\n"));
}

#[tokio::test]
async fn failed_primary_body_is_discarded_and_last_error_is_private() {
    let server = Server::new(vec![partial(), reply("200 OK", b"backup")]);
    let media = item(
        &server.url("/primary.jpg"),
        json!([server.url("/backup.jpg")]),
    );
    let root = tempfile::tempdir().unwrap();
    let result = download::download(&client(), root.path(), &media, 0)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(result.saved_path().unwrap()).unwrap(),
        b"backup"
    );
    assert_eq!(
        std::fs::read_dir(result.saved_path().unwrap().parent().unwrap())
            .unwrap()
            .count(),
        1
    );
    let failed = Server::new(vec![
        reply("404 Not Found", b""),
        reply("404 Not Found", b""),
    ]);
    let media = item(
        &failed.url("/primary.jpg"),
        json!([failed.url("/private-token.jpg?secret-signature")]),
    );
    let error = download::download(&client(), root.path(), &media, 0)
        .await
        .unwrap_err()
        .to_string();
    assert!(!error.contains("private-token"));
    assert!(!error.contains("secret-signature"));
}

#[tokio::test]
async fn resume_retries_primary_then_resumes_only_its_fallback_candidate() {
    let server = Server::new(vec![
        reply("404 Not Found", b""),
        partial(),
        reply("404 Not Found", b""),
        remainder(),
    ]);
    let media = item(
        &server.url("/primary.jpg"),
        json!([server.url("/backup.jpg")]),
    );
    let root = tempfile::tempdir().unwrap();
    server.watch(root.path());
    let client = client().with_resume(true);
    assert!(
        download::download(&client, root.path(), &media, 0)
            .await
            .is_err()
    );
    let outcome = download::download(&client, root.path(), &media, 0)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(outcome.saved_path().unwrap()).unwrap(),
        [vec![b'a'; 4096], vec![b'b'; 4096]].concat()
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 4);
    assert!(!requests[2].to_ascii_lowercase().contains("range:"));
    let resumed = requests[3].to_ascii_lowercase();
    assert!(resumed.contains("range: bytes=4096-\r\n"), "{resumed}");
    assert!(resumed.contains("if-range: \"fallback-version\"\r\n"));
}

#[tokio::test]
async fn moving_candidates_never_appends_an_unrelated_partial() {
    let server = Server::new(vec![partial(), reply("200 OK", b"different-full-body")]);
    let media = item(
        &server.url("/primary.jpg"),
        json!([server.url("/backup.jpg")]),
    );
    let root = tempfile::tempdir().unwrap();
    server.watch(root.path());
    let result = download::download(&client().with_resume(true), root.path(), &media, 0)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(result.saved_path().unwrap()).unwrap(),
        b"different-full-body"
    );
    let request = server.requests()[1].to_ascii_lowercase();
    assert!(!request.contains("range:"));
    assert!(!request.contains("if-range:"));
}

#[tokio::test]
async fn needed_fallback_bounds_fail_privately() {
    for fallbacks in [
        json!(["https://secret.invalid/".repeat(900)]),
        json!(
            (0..129)
                .map(|index| format!("https://secret.invalid/{index}.jpg"))
                .collect::<Vec<_>>()
        ),
    ] {
        let server = Server::new(vec![reply("404 Not Found", b"")]);
        let media = item(&server.url("/primary.jpg"), fallbacks);
        let root = tempfile::tempdir().unwrap();
        let error = download::download(&client(), root.path(), &media, 0)
            .await
            .unwrap_err()
            .to_string();
        assert!(!error.contains("secret.invalid"));
        assert_eq!(server.requests().len(), 1);
    }
}

#[tokio::test]
async fn fallback_completion_keeps_original_archive_identity_and_sidecar() {
    let server = Server::new(vec![
        reply("404 Not Found", b""),
        reply("200 OK", b"backup"),
    ]);
    let media = item(
        &server.url("/original.jpg"),
        json!([server.url("/different.png")]),
    );
    let root = tempfile::tempdir().unwrap();
    let archive = Archive::open(root.path().join("archive.sqlite3"))
        .await
        .unwrap();
    let result = archive
        .download(
            &client(),
            root.path(),
            &media,
            DownloadOptions {
                write_metadata: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let ArchiveOutcome::Completed(Outcome::Downloaded(path)) = result else {
        panic!("{result:?}")
    };
    assert_eq!(path, download::destination(root.path(), &media));
    assert!(
        archive
            .contains(&media.archive_key().unwrap())
            .await
            .unwrap()
    );
    let sidecar: serde_json::Value =
        serde_json::from_slice(&std::fs::read(format!("{}.json", path.display())).unwrap())
            .unwrap();
    assert_eq!(
        sidecar["filename"],
        path.file_name().unwrap().to_str().unwrap()
    );
    assert!(
        path.file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .ends_with("_original.jpg")
    );
    assert!(matches!(
        archive
            .download(&client(), root.path(), &media, DownloadOptions::default())
            .await
            .unwrap(),
        ArchiveOutcome::AlreadyArchived
    ));
    assert_eq!(server.requests().len(), 2);
}

#[tokio::test]
async fn cancelling_an_active_fallback_cleans_temporary_files_and_stops_the_chain() {
    let mut stalled = partial();
    stalled.stall = Duration::from_secs(3);
    let server = Server::new(vec![
        reply("404 Not Found", b""),
        stalled,
        reply("200 OK", b"must-not-be-requested"),
    ]);
    let media = item(
        &server.url("/primary.jpg"),
        json!([server.url("/stalled.jpg"), server.url("/later.jpg")]),
    );
    let root = tempfile::tempdir().unwrap();
    let destination = download::destination(root.path(), &media);
    let task_root = root.path().to_owned();
    let worker = tokio::spawn(async move {
        let client = download::client(Duration::from_secs(5)).unwrap();
        download::download(&client, &task_root, &media, 2).await
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let has_temporary = std::fs::read_dir(root.path().join("direct")).is_ok_and(|entries| {
            // Windows directory listings cache sizes of files still open for
            // writing; query each file directly for its current length.
            entries.filter_map(Result::ok).any(|entry| {
                std::fs::metadata(entry.path())
                    .is_ok_and(|metadata| metadata.is_file() && metadata.len() == 4096)
            })
        });
        if server.requests().len() == 2 && has_temporary {
            break;
        }
        assert!(!worker.is_finished(), "download ended before cancellation");
        assert!(
            Instant::now() < deadline,
            "fallback never reached an active temporary file"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    worker.abort();
    assert!(worker.await.unwrap_err().is_cancelled());
    assert!(!destination.exists());
    assert_eq!(
        std::fs::read_dir(root.path().join("direct"))
            .unwrap()
            .count(),
        0,
        "cancellation must remove its incomplete temporary file"
    );
    assert_eq!(
        server.requests().len(),
        2,
        "later candidates must remain untouched"
    );
}

#[test]
fn local_blocking_io_timeout_does_not_fetch_a_fallback() {
    struct ReleaseOnDrop(Option<std::sync::mpsc::Sender<()>>);
    impl ReleaseOnDrop {
        fn release(&mut self) {
            if let Some(send) = self.0.take() {
                let _ = send.send(());
            }
        }
    }
    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            self.release();
        }
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(1)
        .enable_all()
        .build()
        .unwrap();
    // Declare after runtime, so panic unwinding releases the blocking worker
    // before runtime shutdown waits for it.
    let (release, released) = std::sync::mpsc::channel();
    let mut release = ReleaseOnDrop(Some(release));
    let response_gate = Arc::new(AtomicBool::new(false));
    let mut primary = reply("200 OK", b"primary body");
    primary.response_gate = Some(response_gate.clone());
    let server = Server::new(vec![primary, reply("200 OK", b"must-not-be-fetched")]);
    let media = item(
        &server.url("/primary.jpg"),
        json!([server.url("/fallback.jpg")]),
    );
    let root = tempfile::tempdir().unwrap();
    let destination = download::destination(root.path(), &media);
    let task_root = root.path().to_owned();
    runtime.block_on(async {
        let mut download = tokio::spawn(async move {
            let client = download::client(Duration::from_millis(250)).unwrap();
            download::download(&client, &task_root, &media, 2).await
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while server.requests().is_empty() {
            assert!(Instant::now() < deadline, "primary request never arrived");
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        // The directory is already created and DNS is unnecessary for this IP.
        // Hold the only blocking worker before releasing the HTTP response, so
        // the upcoming asynchronous temporary-file open cannot finish.
        let (started, ready) = std::sync::mpsc::channel();
        let blocker = tokio::task::spawn_blocking(move || {
            let _ = started.send(());
            let _ = released.recv_timeout(Duration::from_secs(5));
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while ready.try_recv().is_err() {
            assert!(Instant::now() < deadline, "blocking worker never started");
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        response_gate.store(true, Ordering::SeqCst);
        let result = tokio::time::timeout(Duration::from_secs(1), &mut download)
            .await
            .expect("local timeout must terminate without retrying or fetching a fallback")
            .unwrap()
            .unwrap_err();
        assert!(result.to_string().contains("timed out"));
        assert_eq!(
            server.requests().len(),
            1,
            "disk timeout must not advance fallback chain"
        );
        assert!(!destination.exists());
        release.release();
        blocker.await.unwrap();
        assert_eq!(
            std::fs::read_dir(root.path().join("direct"))
                .unwrap()
                .count(),
            0
        );
    });
}
