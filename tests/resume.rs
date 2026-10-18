use gallery_dl_rs::{
    archive::{Archive, ArchiveOutcome},
    download::{self, DownloadOptions, Outcome},
    extract::{Media, MediaJob},
    naming::FilenameTemplate,
};
use std::{
    collections::BTreeMap,
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

const HALF: usize = 4096;
fn prefix() -> Vec<u8> {
    vec![b'a'; HALF]
}
fn suffix() -> Vec<u8> {
    vec![b'b'; HALF]
}
fn complete() -> Vec<u8> {
    [prefix(), suffix()].concat()
}

struct Response {
    head: String,
    body: Vec<u8>,
    stall: Duration,
    wait_for_prefix: bool,
}
fn response(status: &str, headers: &str, body: Vec<u8>) -> Response {
    Response {
        head: format!("HTTP/1.1 {status}\r\nConnection: close\r\n{headers}\r\n"),
        body,
        stall: Duration::from_millis(30),
        wait_for_prefix: false,
    }
}
fn initial(validator: &str) -> Response {
    let mut response = response(
        "200 OK",
        &format!("Content-Length: {}\r\n{validator}", HALF * 2),
        prefix(),
    );
    response.wait_for_prefix = true;
    response
}
fn remainder(validator: &str) -> Response {
    response(
        "206 Partial Content",
        &format!(
            "Content-Length: {HALF}\r\nContent-Range: bytes {HALF}-{}/{}\r\n{validator}",
            HALF * 2 - 1,
            HALF * 2
        ),
        suffix(),
    )
}
fn full(body: Vec<u8>) -> Response {
    response(
        "200 OK",
        &format!("Content-Length: {}\r\nETag: \"new\"\r\n", body.len()),
        body,
    )
}

/// Every accept/read is bounded and Drop stops the server, including when a
/// test panics before consuming its script. Keep requests for assertions on the
/// test thread, so a failed expectation cannot strand the downloader.
struct Server {
    url: String,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    root: Arc<Mutex<Option<PathBuf>>>,
    handle: Option<thread::JoinHandle<()>>,
}
impl Server {
    fn new(script: Vec<Response>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/media.dat", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let root = Arc::new(Mutex::new(None::<PathBuf>));
        let watched_root = root.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let handle = thread::spawn(move || {
            for response in script {
                let deadline = Instant::now() + Duration::from_secs(10);
                let mut socket = loop {
                    if stopping.load(Ordering::SeqCst) || Instant::now() > deadline {
                        return;
                    }
                    match listener.accept() {
                        Ok((socket, _)) => break socket,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
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
                let mut bytes = Vec::new();
                while !bytes.ends_with(b"\r\n\r\n") && bytes.len() < 32768 {
                    let mut byte = [0];
                    match socket.read(&mut byte) {
                        Ok(1) => bytes.push(byte[0]),
                        _ => return,
                    }
                }
                captured
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&bytes).into_owned());
                if socket.write_all(response.head.as_bytes()).is_err() {
                    continue;
                }
                let _ = socket.write_all(&response.body);
                let _ = socket.flush();
                if response.wait_for_prefix {
                    let deadline = Instant::now() + Duration::from_secs(2);
                    while Instant::now() < deadline && !stopping.load(Ordering::SeqCst) {
                        let persisted = watched_root.lock().unwrap().as_ref().is_some_and(|root| {
                            std::fs::read_dir(root.join("direct/.gallery-dl-rs")).is_ok_and(
                                |entries| {
                                    entries.filter_map(std::result::Result::ok).any(|entry| {
                                        entry.path().extension().is_some_and(|ext| ext == "part")
                                            && entry
                                                .metadata()
                                                .is_ok_and(|meta| meta.len() == HALF as u64)
                                    })
                                },
                            )
                        });
                        if persisted {
                            break;
                        }
                        thread::sleep(Duration::from_millis(2));
                    }
                }
                let deadline = Instant::now() + response.stall;
                while Instant::now() < deadline && !stopping.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(2));
                }
            }
        });
        Self {
            url,
            requests,
            stop,
            root,
            handle: Some(handle),
        }
    }
    fn watch(&self, root: &Path) {
        *self.root.lock().unwrap() = Some(root.to_owned());
    }
    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.handle.take().unwrap().join().unwrap();
    }
}
fn media(server: &Server) -> Media {
    Media {
        url: server.url.parse().unwrap(),
        job: MediaJob::DirectHttp,
        category: "direct".into(),
        filename: "media.dat".into(),
        metadata: None,
        headers: BTreeMap::new(),
        handoff: BTreeMap::new(),
    }
}
fn client() -> download::DownloadClient {
    download::client(Duration::from_secs(3))
        .unwrap()
        .with_resume(true)
}
fn header<'a>(request: &'a str, name: &str) -> Option<&'a str> {
    request
        .lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.trim())
}
fn assert_range(request: &str, offset: usize, validator: &str) {
    assert_eq!(
        header(request, "Range"),
        Some(format!("bytes={offset}-").as_str())
    );
    assert_eq!(header(request, "If-Range"), Some(validator));
    assert_eq!(header(request, "Accept-Encoding"), Some("identity"));
}
fn assert_full(request: &str) {
    assert_eq!(header(request, "Range"), None);
    assert_eq!(header(request, "If-Range"), None);
}
fn parts(root: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(root.join("direct/.gallery-dl-rs"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "part"))
        .collect()
}

#[tokio::test]
async fn resumes_across_invocations_and_does_not_persist_transport_secrets() {
    let server = Server::new(vec![
        initial("ETag: \"v1\"\r\n"),
        remainder("ETag: \"v1\"\r\n"),
    ]);
    let mut item = media(&server);
    item.url.set_query(Some("token=PRIVATE_URL"));
    item.headers
        .insert("Cookie".into(), "PRIVATE_COOKIE".into());
    let root = tempfile::tempdir().unwrap();
    server.watch(root.path());
    let path = download::destination(root.path(), &item);
    assert!(
        download::download(&client(), root.path(), &item, 0)
            .await
            .is_err()
    );
    assert!(!path.exists());
    assert_eq!(std::fs::read(&parts(root.path())[0]).unwrap(), prefix());
    for entry in std::fs::read_dir(path.parent().unwrap().join(".gallery-dl-rs")).unwrap() {
        let entry = entry.unwrap();
        assert!(!entry.file_name().to_string_lossy().contains("PRIVATE"));
        assert!(
            !String::from_utf8_lossy(&std::fs::read(entry.path()).unwrap()).contains("PRIVATE")
        );
    }
    assert_eq!(
        download::download(&client(), root.path(), &item, 0)
            .await
            .unwrap(),
        Outcome::Downloaded(path.clone())
    );
    assert_eq!(std::fs::read(path).unwrap(), complete());
    assert!(parts(root.path()).is_empty());
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_full(&requests[0]);
    assert_range(&requests[1], HALF, "\"v1\"");
}

#[tokio::test]
async fn retry_resumes_within_the_same_download() {
    let server = Server::new(vec![
        initial("ETag: \"v1\"\r\n"),
        remainder("ETag: \"v1\"\r\n"),
    ]);
    let item = media(&server);
    let root = tempfile::tempdir().unwrap();
    server.watch(root.path());
    download::download(&client(), root.path(), &item, 1)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(download::destination(root.path(), &item)).unwrap(),
        complete()
    );
    assert_range(&server.requests()[1], HALF, "\"v1\"");
}

#[tokio::test]
async fn ignored_range_replaces_partial_without_mixing_bytes() {
    let replacement = vec![b'z'; HALF * 2];
    let server = Server::new(vec![initial("ETag: \"v1\"\r\n"), full(replacement.clone())]);
    let item = media(&server);
    let root = tempfile::tempdir().unwrap();
    server.watch(root.path());
    assert!(
        download::download(&client(), root.path(), &item, 0)
            .await
            .is_err()
    );
    download::download(&client(), root.path(), &item, 0)
        .await
        .unwrap_or_else(|error| panic!("{error}; requests: {:?}", server.requests()));
    assert_eq!(
        std::fs::read(download::destination(root.path(), &item)).unwrap(),
        replacement
    );
    assert_range(&server.requests()[1], HALF, "\"v1\"");
}

#[tokio::test]
async fn missing_weak_or_duplicate_validators_restart_full() {
    for validator in [
        "",
        "ETag: W/\"v1\"\r\n",
        "ETag: \"v1\"\r\nETag: \"v1\"\r\n",
        "ETag: W/\"v1\"\r\nLast-Modified: Wed, 21 Oct 2015 07:28:00 GMT\r\nDate: Wed, 21 Oct 2015 07:30:00 GMT\r\n",
    ] {
        let server = Server::new(vec![initial(validator), full(complete())]);
        let item = media(&server);
        let root = tempfile::tempdir().unwrap();
        server.watch(root.path());
        assert!(
            download::download(&client(), root.path(), &item, 0)
                .await
                .is_err()
        );
        download::download(&client(), root.path(), &item, 0)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(download::destination(root.path(), &item)).unwrap(),
            complete()
        );
        assert_full(&server.requests()[1]);
    }
}

#[tokio::test]
async fn modification_date_requires_a_strong_sixty_second_gap() {
    let modified = "Wed, 21 Oct 2015 07:28:00 GMT";
    for (date, strong) in [
        ("Wed, 21 Oct 2015 07:29:00 GMT", true),
        ("Wed, 21 Oct 2015 07:28:59 GMT", false),
    ] {
        let validator = format!("Last-Modified: {modified}\r\nDate: {date}\r\n");
        let second = if strong {
            remainder(&validator)
        } else {
            full(complete())
        };
        let server = Server::new(vec![initial(&validator), second]);
        let item = media(&server);
        let root = tempfile::tempdir().unwrap();
        server.watch(root.path());
        assert!(
            download::download(&client(), root.path(), &item, 0)
                .await
                .is_err()
        );
        download::download(&client(), root.path(), &item, 0)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(download::destination(root.path(), &item)).unwrap(),
            complete()
        );
        if strong {
            assert_range(&server.requests()[1], HALF, modified);
        } else {
            assert_full(&server.requests()[1]);
        }
    }
}

#[tokio::test]
async fn malformed_or_mismatched_ranges_never_publish_or_append() {
    for (range, length) in [
        ("bytes 0-4095/8192", HALF),
        ("bytes 4096-8191/8193", HALF),
        ("bytes 4096-8192/8192", HALF),
        ("bytes 4096-8191/*", HALF),
        ("nonsense", HALF),
        ("bytes 4096-8191/8192", HALF - 1),
    ] {
        let bad = response(
            "206 Partial Content",
            &format!("Content-Length: {length}\r\nContent-Range: {range}\r\nETag: \"v1\"\r\n"),
            suffix(),
        );
        let server = Server::new(vec![initial("ETag: \"v1\"\r\n"), bad]);
        let item = media(&server);
        let root = tempfile::tempdir().unwrap();
        server.watch(root.path());
        assert!(
            download::download(&client(), root.path(), &item, 0)
                .await
                .is_err()
        );
        assert!(
            download::download(&client(), root.path(), &item, 0)
                .await
                .is_err(),
            "accepted {range}"
        );
        assert!(!download::destination(root.path(), &item).exists());
        assert_eq!(std::fs::read(&parts(root.path())[0]).unwrap(), prefix());
    }
}

#[tokio::test]
async fn changed_etag_or_redirect_target_restarts_full_before_publication() {
    for redirect in [false, true] {
        let mut changed = remainder("ETag: \"different\"\r\n");
        if redirect {
            changed = remainder("ETag: \"v1\"\r\n");
        }
        let mut script = vec![initial("ETag: \"v1\"\r\n")];
        if redirect {
            script.push(response(
                "302 Found",
                "Content-Length: 0\r\nLocation: /different.dat\r\n",
                Vec::new(),
            ));
        }
        script.extend([changed, full(vec![b'z'; HALF * 2])]);
        let server = Server::new(script);
        let item = media(&server);
        let root = tempfile::tempdir().unwrap();
        server.watch(root.path());
        assert!(
            download::download(&client(), root.path(), &item, 0)
                .await
                .is_err()
        );
        download::download(&client(), root.path(), &item, 0)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(download::destination(root.path(), &item)).unwrap(),
            vec![b'z'; HALF * 2]
        );
        let requests = server.requests();
        assert_full(requests.last().unwrap());
        if redirect {
            assert!(requests[2].starts_with("GET /different.dat "));
        }
    }
}

#[tokio::test]
async fn unsolicited_partial_responses_are_rejected_with_resume_on_or_off() {
    for resume in [false, true] {
        let server = Server::new(vec![remainder("ETag: \"v1\"\r\n")]);
        let item = media(&server);
        let root = tempfile::tempdir().unwrap();
        server.watch(root.path());
        let client = download::client(Duration::from_secs(3))
            .unwrap()
            .with_resume(resume);
        assert!(
            download::download(&client, root.path(), &item, 0)
                .await
                .is_err()
        );
        assert!(!download::destination(root.path(), &item).exists());
        assert_full(&server.requests()[0]);
    }
}

#[tokio::test]
async fn range_416_only_completes_an_exact_validated_partial() {
    for (total, etag, valid) in [
        (HALF, "v1", true),
        (HALF + 1, "v1", false),
        (HALF, "different", false),
    ] {
        let chunk = [
            format!("{HALF:x}\r\n").into_bytes(),
            prefix(),
            b"\r\n".to_vec(),
        ]
        .concat();
        let mut truncated_chunked = response(
            "200 OK",
            "Transfer-Encoding: chunked\r\nETag: \"v1\"\r\n",
            chunk,
        );
        truncated_chunked.wait_for_prefix = true;
        let mut script = vec![
            truncated_chunked,
            response(
                "416 Range Not Satisfiable",
                &format!(
                    "Content-Length: 0\r\nETag: \"{etag}\"\r\nContent-Range: bytes */{total}\r\n"
                ),
                Vec::new(),
            ),
        ];
        if !valid {
            script.push(full(complete()));
        }
        let server = Server::new(script);
        let item = media(&server);
        let root = tempfile::tempdir().unwrap();
        server.watch(root.path());
        assert!(
            download::download(&client(), root.path(), &item, 0)
                .await
                .is_err()
        );
        download::download(&client(), root.path(), &item, 0)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(download::destination(root.path(), &item)).unwrap(),
            if valid { prefix() } else { complete() }
        );
        let requests = server.requests();
        assert_range(&requests[1], HALF, "\"v1\"");
        if !valid {
            assert_full(&requests[2]);
        }
    }
}

#[tokio::test]
async fn cancellation_retains_checkpoint_and_concurrent_downloads_cannot_share_it() {
    let mut first = initial("ETag: \"v1\"\r\n");
    first.stall = Duration::from_millis(400);
    let server = Server::new(vec![first, remainder("ETag: \"v1\"\r\n")]);
    let item = media(&server);
    let root = tempfile::tempdir().unwrap();
    server.watch(root.path());
    let task_item = item.clone();
    let task_root = root.path().to_owned();
    let task =
        tokio::spawn(async move { download::download(&client(), &task_root, &task_item, 0).await });
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(entries) = std::fs::read_dir(root.path().join("direct/.gallery-dl-rs"))
                && entries.filter_map(std::result::Result::ok).any(|entry| {
                    entry.path().extension().is_some_and(|ext| ext == "part")
                        && entry.metadata().is_ok_and(|meta| meta.len() == HALF as u64)
                })
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
    let error = download::download(&client(), root.path(), &item, 0)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("another download"), "{error}");
    assert_eq!(server.requests().len(), 1);
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(!download::destination(root.path(), &item).exists());
    // A cancelled append can still own its blocking disk operation. Wait for
    // that operation to release the lock before starting another HTTP request.
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match download::download(&client(), root.path(), &item, 0).await {
                Ok(_) => break,
                Err(error) if error.to_string().contains("another download") => {
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
                Err(error) => panic!("resume after cancellation failed: {error}"),
            }
        }
    })
    .await
    .unwrap();
    assert!(std::fs::read(download::destination(root.path(), &item)).unwrap() == complete());
    assert_range(&server.requests()[1], HALF, "\"v1\"");
}

#[tokio::test]
async fn resumed_custom_filename_records_archive_only_after_publication() {
    let server = Server::new(vec![
        initial("ETag: \"v1\"\r\n"),
        remainder("ETag: \"v1\"\r\n"),
    ]);
    let item = media(&server);
    let root = tempfile::tempdir().unwrap();
    server.watch(root.path());
    let archive = Archive::open(root.path().join("archive.sqlite"))
        .await
        .unwrap();
    let key = item.archive_key().unwrap();
    let template = FilenameTemplate::parse("custom.{extension}").unwrap();
    let client = client();
    let directory =
        gallery_dl_rs::naming::DirectoryTemplate::parse(&["nested".into(), "{category}".into()])
            .unwrap();
    let options = DownloadOptions {
        directory: Some(&directory),
        write_metadata: true,
        ..Default::default()
    };
    assert!(
        archive
            .download_with_template(&client, root.path(), &item, options, Some(&template))
            .await
            .is_err()
    );
    assert!(!archive.contains(&key).await.unwrap());
    let path =
        download::destination_with_templates(root.path(), &item, Some(&template), Some(&directory))
            .unwrap();
    assert_eq!(
        archive
            .download_with_template(&client, root.path(), &item, options, Some(&template))
            .await
            .unwrap(),
        ArchiveOutcome::Completed(Outcome::Downloaded(path.clone()))
    );
    assert_eq!(std::fs::read(&path).unwrap(), complete());
    assert!(path.parent().unwrap().ends_with("nested/direct"));
    assert!(path.with_file_name("custom.dat.json").is_file());
    assert!(archive.contains(&key).await.unwrap());
    assert_eq!(
        archive
            .download_with_template(&client, root.path(), &item, options, Some(&template))
            .await
            .unwrap(),
        ArchiveOutcome::AlreadyArchived
    );
    assert_eq!(server.requests().len(), 2);
}

#[tokio::test]
async fn short_ranges_continue_until_the_known_total_is_complete() {
    let middle = response(
        "206 Partial Content",
        "Content-Length: 2048\r\nContent-Range: bytes 4096-6143/8192\r\nETag: \"v1\"\r\n",
        vec![b'b'; HALF / 2],
    );
    let last = response(
        "206 Partial Content",
        "Content-Length: 2048\r\nContent-Range: bytes 6144-8191/8192\r\nETag: \"v1\"\r\n",
        vec![b'b'; HALF / 2],
    );
    let server = Server::new(vec![initial("ETag: \"v1\"\r\n"), middle, last]);
    let item = media(&server);
    let root = tempfile::tempdir().unwrap();
    server.watch(root.path());
    assert!(
        download::download(&client(), root.path(), &item, 0)
            .await
            .is_err()
    );
    download::download(&client(), root.path(), &item, 0)
        .await
        .unwrap();
    assert!(std::fs::read(download::destination(root.path(), &item)).unwrap() == complete());
    let requests = server.requests();
    assert_range(&requests[1], HALF, "\"v1\"");
    assert_range(&requests[2], HALF + HALF / 2, "\"v1\"");
}

#[tokio::test]
async fn totals_learned_from_a_range_cannot_change_in_a_later_range() {
    let chunk = [
        format!("{HALF:x}\r\n").into_bytes(),
        prefix(),
        b"\r\n".to_vec(),
    ]
    .concat();
    let mut first = response(
        "200 OK",
        "Transfer-Encoding: chunked\r\nETag: \"v1\"\r\n",
        chunk,
    );
    first.wait_for_prefix = true;
    let middle = response(
        "206 Partial Content",
        "Content-Length: 4096\r\nContent-Range: bytes 4096-8191/12288\r\nETag: \"v1\"\r\n",
        suffix(),
    );
    let inconsistent = response(
        "206 Partial Content",
        "Content-Length: 4096\r\nContent-Range: bytes 8192-12287/16384\r\nETag: \"v1\"\r\n",
        vec![b'c'; HALF],
    );
    let server = Server::new(vec![first, middle, inconsistent]);
    let item = media(&server);
    let root = tempfile::tempdir().unwrap();
    server.watch(root.path());
    assert!(
        download::download(&client(), root.path(), &item, 0)
            .await
            .is_err()
    );
    assert!(
        download::download(&client(), root.path(), &item, 0)
            .await
            .is_err()
    );
    assert!(!download::destination(root.path(), &item).exists());
    assert!(std::fs::read(&parts(root.path())[0]).unwrap() == complete());
    assert_range(&server.requests()[2], HALF * 2, "\"v1\"");
}

#[tokio::test]
async fn cli_resume_opt_in_persists_between_processes() {
    let server = Server::new(vec![
        initial("ETag: \"v1\"\r\n"),
        remainder("ETag: \"v1\"\r\n"),
    ]);
    let root = tempfile::tempdir().unwrap();
    server.watch(root.path());
    let url = server.url.replace("media.dat", "media.jpg");
    for expected_success in [false, true] {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_gallery-dl-rs"));
        command
            .kill_on_drop(true)
            .arg("--resume")
            .arg("--retries")
            .arg("0")
            .arg("--timeout")
            .arg("2")
            .arg("--filename")
            .arg("custom.jpg")
            .arg("--destination")
            .arg(root.path())
            .arg(&url);
        let output = tokio::time::timeout(Duration::from_secs(5), command.output())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            output.status.success(),
            expected_success,
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(std::fs::read(root.path().join("direct/custom.jpg")).unwrap() == complete());
    assert_range(&server.requests()[1], HALF, "\"v1\"");
}

#[tokio::test]
async fn size_limits_use_full_resumed_total_and_clear_rejected_checkpoints() {
    for limits in [
        gallery_dl_rs::size::SizeLimits::new(None, Some(6000)).unwrap(),
        gallery_dl_rs::size::SizeLimits::new(Some(9000), None).unwrap(),
    ] {
        let server = Server::new(vec![
            initial("ETag: \"v1\"\r\n"),
            remainder("ETag: \"v1\"\r\n"),
            full(b"ok".to_vec()),
        ]);
        let item = media(&server);
        let root = tempfile::tempdir().unwrap();
        server.watch(root.path());
        assert!(
            download::download(&client(), root.path(), &item, 0)
                .await
                .is_err()
        );
        let result = download::download_with_options(
            &client(),
            root.path(),
            &item,
            DownloadOptions {
                limits,
                retries: 2,
                write_metadata: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(matches!(result, Outcome::Filtered(_)));
        let path = download::destination(root.path(), &item);
        assert!(!path.exists());
        assert!(
            !path
                .with_file_name(format!(
                    "{}.json",
                    path.file_name().unwrap().to_str().unwrap()
                ))
                .exists()
        );
        for entry in std::fs::read_dir(root.path().join("direct/.gallery-dl-rs")).unwrap() {
            let entry = entry.unwrap();
            assert!(!entry.path().extension().is_some_and(|ext| ext == "json"));
            if entry.path().extension().is_some_and(|ext| ext == "part") {
                assert_eq!(entry.metadata().unwrap().len(), 0);
            }
        }
        download::download(&client(), root.path(), &item, 0)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"ok");
        let requests = server.requests();
        assert_eq!(requests.len(), 3);
        assert_range(&requests[1], HALF, "\"v1\"");
        assert_full(&requests[2]);
    }
}

#[tokio::test]
async fn size_limits_validate_new_representation_before_rejecting_stale_partial() {
    let server = Server::new(vec![initial("ETag: \"v1\"\r\n"), full(b"ok".to_vec())]);
    let item = media(&server);
    let root = tempfile::tempdir().unwrap();
    server.watch(root.path());
    assert!(
        download::download(&client(), root.path(), &item, 0)
            .await
            .is_err()
    );
    let limits = gallery_dl_rs::size::SizeLimits::new(None, Some(5)).unwrap();
    let result = download::download_with_options(
        &client(),
        root.path(),
        &item,
        DownloadOptions {
            limits,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(matches!(result, Outcome::Downloaded(_)));
    assert_eq!(
        std::fs::read(download::destination(root.path(), &item)).unwrap(),
        b"ok"
    );
    assert_range(&server.requests()[1], HALF, "\"v1\"");
}

#[tokio::test]
async fn size_limits_filter_validated_416_completion_before_publication() {
    let chunk = [
        format!("{HALF:x}\r\n").into_bytes(),
        prefix(),
        b"\r\n".to_vec(),
    ]
    .concat();
    let mut truncated = response(
        "200 OK",
        "Transfer-Encoding: chunked\r\nETag: \"v1\"\r\n",
        chunk,
    );
    truncated.wait_for_prefix = true;
    let server = Server::new(vec![
        truncated,
        response(
            "416 Range Not Satisfiable",
            &format!("Content-Length: 0\r\nETag: \"v1\"\r\nContent-Range: bytes */{HALF}\r\n"),
            Vec::new(),
        ),
    ]);
    let item = media(&server);
    let root = tempfile::tempdir().unwrap();
    server.watch(root.path());
    assert!(
        download::download(&client(), root.path(), &item, 0)
            .await
            .is_err()
    );
    let outcome = download::download_with_options(
        &client(),
        root.path(),
        &item,
        DownloadOptions {
            limits: gallery_dl_rs::size::SizeLimits::new(None, Some(4000)).unwrap(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(matches!(outcome, Outcome::Filtered(_)));
    assert!(!download::destination(root.path(), &item).exists());
    assert_eq!(server.requests().len(), 2);
}

#[tokio::test]
async fn changed_configured_user_agent_does_not_resume_another_representation() {
    let server = Server::new(vec![
        initial("ETag: \"v1\"\r\n"),
        full(b"new agent".to_vec()),
    ]);
    let item = media(&server);
    let root = tempfile::tempdir().unwrap();
    server.watch(root.path());
    let template = FilenameTemplate::parse("same.dat").unwrap();
    let first = client()
        .with_network(
            &gallery_dl_rs::network::NetworkOptions::new(
                Some(String::new()),
                Some("first-agent".into()),
            )
            .unwrap(),
        )
        .unwrap();
    let second = client()
        .with_network(
            &gallery_dl_rs::network::NetworkOptions::new(
                Some(String::new()),
                Some("second-agent".into()),
            )
            .unwrap(),
        )
        .unwrap();
    assert!(
        download::download_with_template(
            &first,
            root.path(),
            &item,
            DownloadOptions::default(),
            Some(&template)
        )
        .await
        .is_err()
    );
    let outcome = download::download_with_template(
        &second,
        root.path(),
        &item,
        DownloadOptions::default(),
        Some(&template),
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read(outcome.saved_path().unwrap()).unwrap(),
        b"new agent"
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(header(&requests[0], "User-Agent"), Some("first-agent"));
    assert_eq!(header(&requests[1], "User-Agent"), Some("second-agent"));
    assert_full(&requests[1]);
    assert!(!item.headers.contains_key("User-Agent"));
}
