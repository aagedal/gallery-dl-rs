use gallery_dl_rs::{
    archive::{Archive, ArchiveOutcome},
    download::{self, DownloadOptions, Outcome},
    extract::{self, Media, PostMetadata},
    size::SizeLimits,
};
use std::{
    io::{Read, Write},
    net::TcpListener,
    path::Path,
    process::Output,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use tokio::process::Command;

/// Bind before spawning: there is no server-startup race. Every accept, read,
/// and intentionally withheld body is bounded, including assertion failures.
struct Server {
    url: String,
    requests: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Server {
    fn new(response: &'static [u8], hold_open: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/image.jpg", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (count, stopping) = (requests.clone(), stop.clone());
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(15);
            while !stopping.load(Ordering::SeqCst) && Instant::now() < deadline {
                let mut socket = match listener.accept() {
                    Ok((socket, _)) => socket,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(e) => panic!("{e}"),
                };
                socket.set_nonblocking(false).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                socket
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = Vec::new();
                let mut bytes = [0; 4096];
                while !request.ends_with(b"\r\n\r\n") {
                    let read = socket.read(&mut bytes).unwrap();
                    assert!(read > 0 && request.len() < 65536);
                    request.extend_from_slice(&bytes[..read]);
                }
                count.fetch_add(1, Ordering::SeqCst);
                socket.write_all(response).unwrap();
                if hold_open {
                    while !stopping.load(Ordering::SeqCst) && Instant::now() < deadline {
                        thread::sleep(Duration::from_millis(2));
                    }
                }
            }
        });
        Self {
            url,
            requests,
            stop,
            worker: Some(worker),
        }
    }
    fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.worker.take().unwrap().join().unwrap();
    }
}
fn media(url: &str) -> Media {
    extract::direct(&extract::http_url(url).unwrap()).unwrap()
}
fn options(minimum: Option<u64>, maximum: Option<u64>) -> DownloadOptions<'static> {
    DownloadOptions {
        limits: SizeLimits::new(minimum, maximum).unwrap(),
        retries: 2,
        write_metadata: true,
        ..Default::default()
    }
}
fn sidecar(path: &Path) -> std::path::PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(".json");
    value.into()
}
fn no_output(root: &Path, item: &Media) {
    let path = download::destination(root, item);
    assert!(!path.exists());
    assert!(!sidecar(&path).exists());
    if let Some(parent) = path.parent().filter(|path| path.exists()) {
        assert_eq!(
            std::fs::read_dir(parent).unwrap().count(),
            0,
            "temporary output leaked"
        );
    }
}

#[tokio::test]
async fn declared_sizes_are_filtered_without_waiting_for_body_or_retrying() {
    for (minimum, maximum) in [(Some(4), None), (None, Some(2))] {
        let server = Server::new(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\n", true);
        let root = tempfile::tempdir().unwrap();
        let item = media(&server.url);
        let client = download::client(Duration::from_secs(4)).unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            download::download_with_options(&client, root.path(), &item, options(minimum, maximum)),
        )
        .await
        .expect("known-size filtering waited for the withheld response body")
        .unwrap();
        assert!(matches!(result, Outcome::Filtered(_)), "{result:?}");
        assert!(result.saved_path().is_none());
        assert_eq!(server.requests(), 1);
        no_output(root.path(), &item);
    }
}

#[tokio::test]
async fn inclusive_size_bounds_publish_media_and_metadata() {
    for (minimum, maximum) in [(Some(3), None), (None, Some(3)), (Some(3), Some(3))] {
        let server = Server::new(
            b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nabc",
            false,
        );
        let root = tempfile::tempdir().unwrap();
        let item = media(&server.url);
        let client = download::client(Duration::from_secs(3)).unwrap();
        let result =
            download::download_with_options(&client, root.path(), &item, options(minimum, maximum))
                .await
                .unwrap();
        let path = download::destination(root.path(), &item);
        assert_eq!(result, Outcome::Downloaded(path.clone()));
        assert_eq!(result.saved_path(), Some(path.as_path()));
        assert_eq!(std::fs::read(&path).unwrap(), b"abc");
        assert!(sidecar(&path).exists());
        assert_eq!(server.requests(), 1);
    }
}

#[tokio::test]
async fn streaming_upper_bound_aborts_before_unknown_length_body_finishes() {
    for response in [
        &b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nabc"[..],
        &b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n"[..],
    ] {
        let server = Server::new(response, true);
        let root = tempfile::tempdir().unwrap();
        let item = media(&server.url);
        let client = download::client(Duration::from_secs(4)).unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            download::download_with_options(&client, root.path(), &item, options(None, Some(2))),
        )
        .await
        .expect("upper bound waited for an unfinished response body")
        .unwrap();
        assert!(matches!(result, Outcome::Filtered(_)), "{result:?}");
        assert!(result.saved_path().is_none());
        assert_eq!(server.requests(), 1);
        no_output(root.path(), &item);
    }
}

#[tokio::test]
async fn unknown_length_small_complete_response_is_discarded() {
    for response in [
        &b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nabc"[..],
        &b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\n"[..],
    ] {
        let server = Server::new(response, false);
        let root = tempfile::tempdir().unwrap();
        let item = media(&server.url);
        let client = download::client(Duration::from_secs(3)).unwrap();
        let result =
            download::download_with_options(&client, root.path(), &item, options(Some(4), None))
                .await
                .unwrap();
        assert!(matches!(result, Outcome::Filtered(_)), "{result:?}");
        no_output(root.path(), &item);
        assert_eq!(server.requests(), 1);
    }
}

#[tokio::test]
async fn filtered_media_does_not_enter_archive_and_can_be_downloaded_later() {
    let server = Server::new(
        b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nabc",
        false,
    );
    let root = tempfile::tempdir().unwrap();
    let archive = Archive::open(root.path().join("archive.sqlite"))
        .await
        .unwrap();
    let mut item = media(&server.url);
    item.category = "instagram".into();
    item.metadata = Some(PostMetadata {
        tags: None,
        sketch: None,
        sketch_archive: false,
        user_bookmark: None,
        bookmark_archive: false,
        ajax: None,
        mypixiv: None,
        caption_html: None,
        pixiv_user_detail: None,
        comments: None,
        tags_bookmark: None,
        ugoira_original: None,
        ugoira_frame_index: None,
        pixivision: None,
        pixivision_archive: false,
        user_follow: None,
        followed_archive: false,
        series: None,
        num_series: None,
        search: None,
        search_archive: false,
        ranking: None,
        ranking_archive: false,
        profile_asset: None,
        file: None,
        frames: None,
        rating: None,
        score: None,
        revision: None,
        discord: None,
        creator: None,
        community: None,
        community_archive: false,
        retweet_id: None,
        quoted_by_id: None,
        reposted_by_id: None,
        article: None,
        instance: None,
        post_id: "100".into(),
        media_id: "101".into(),
        shortcode: "Example".into(),
        username: None,
        caption: None,
        timestamp: None,
        index: 1,
    });
    let identity = item.archive_key().unwrap();
    let client = download::client(Duration::from_secs(3)).unwrap();
    let result = archive
        .download(&client, root.path(), &item, options(Some(4), None))
        .await
        .unwrap();
    assert!(matches!(
        result,
        ArchiveOutcome::Completed(Outcome::Filtered(_))
    ));
    assert!(!archive.contains(&identity).await.unwrap());
    no_output(root.path(), &item);
    assert_eq!(server.requests(), 1);
    let result = archive
        .download(&client, root.path(), &item, options(None, Some(3)))
        .await
        .unwrap();
    assert!(matches!(
        result,
        ArchiveOutcome::Completed(Outcome::Downloaded(_))
    ));
    assert!(archive.contains(&identity).await.unwrap());
    assert_eq!(server.requests(), 2);
}

#[tokio::test]
async fn existing_files_are_preserved_when_size_limits_change() {
    let root = tempfile::tempdir().unwrap();
    let item = media("http://127.0.0.1:1/image.jpg");
    let path = download::destination(root.path(), &item);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"existing").unwrap();
    let client = download::client(Duration::from_secs(1)).unwrap();
    let result =
        download::download_with_options(&client, root.path(), &item, options(None, Some(1)))
            .await
            .unwrap();
    assert_eq!(result, Outcome::Skipped(path.clone()));
    assert_eq!(std::fs::read(&path).unwrap(), b"existing");
    assert!(sidecar(&path).exists());
}

fn binary(cwd: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_gallery-dl-rs"));
    command.current_dir(cwd).kill_on_drop(true);
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
fn config(root: &Path, value: serde_json::Value) -> std::path::PathBuf {
    let path = root.join("config.json");
    std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    path
}

#[tokio::test]
async fn scoped_configuration_and_explicit_zero_override_size_limits() {
    let server = Server::new(
        b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nabc",
        false,
    );
    let root = tempfile::tempdir().unwrap();
    let cfg = config(
        root.path(),
        serde_json::json!({"extractor": {
            "base-directory": "output", "filename":"saved.jpg", "filesize-min": "1k",
            "direct":{"filesize-min": 0, "filesize-max": 2}
        }}),
    );
    let mut command = binary(root.path());
    command.arg("--native-config").arg(&cfg).arg(&server.url);
    success(&output(&mut command).await);
    let path = root.path().join("output/direct/saved.jpg");
    assert!(!path.exists());
    let mut command = binary(root.path());
    command
        .arg("--native-config")
        .arg(&cfg)
        .args(["--filesize-max", "0"])
        .arg(&server.url);
    success(&output(&mut command).await);
    assert_eq!(std::fs::read(path).unwrap(), b"abc");
    assert_eq!(server.requests(), 2);
}

#[tokio::test]
async fn global_size_setting_takes_precedence_over_category() {
    let server = Server::new(
        b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nabc",
        false,
    );
    let root = tempfile::tempdir().unwrap();
    let cfg = config(
        root.path(),
        serde_json::json!({
            "filesize-max": 0,
            "extractor": {"base-directory":"output", "filename":"saved.jpg", "filesize-max":1,
                "direct":{"filesize-max":2}}
        }),
    );
    let mut command = binary(root.path());
    command.arg("--native-config").arg(cfg).arg(&server.url);
    success(&output(&mut command).await);
    assert_eq!(
        std::fs::read(root.path().join("output/direct/saved.jpg")).unwrap(),
        b"abc"
    );
    assert_eq!(server.requests(), 1);
}

#[tokio::test]
async fn invalid_size_bounds_fail_before_destination_or_archive_creation() {
    for limits in [
        ["--filesize-min", "4", "--filesize-max", "3"],
        ["--filesize-min", "-1", "--filesize-max", "3"],
    ] {
        let root = tempfile::tempdir().unwrap();
        let mut command = binary(root.path());
        command.args(limits).args([
            "--destination",
            "output",
            "--download-archive",
            "archive.sqlite",
            "http://127.0.0.1:1/image.jpg",
        ]);
        let result = output(&mut command).await;
        assert!(!result.status.success());
        assert!(!root.path().join("output").exists());
        assert!(!root.path().join("archive.sqlite").exists());
    }
    let root = tempfile::tempdir().unwrap();
    let cfg = config(
        root.path(),
        serde_json::json!({"extractor":{
        "base-directory":"output", "archive":"archive.sqlite", "filesize-min":4,"filesize-max":3}}),
    );
    let mut command = binary(root.path());
    command
        .arg("--native-config")
        .arg(cfg)
        .arg("http://127.0.0.1:1/image.jpg");
    assert!(!output(&mut command).await.status.success());
    assert!(!root.path().join("output").exists());
    assert!(!root.path().join("archive.sqlite").exists());
}

#[tokio::test]
async fn inspection_ignores_size_filters_without_fetching_media_or_opening_archive() {
    let server = Server::new(
        b"HTTP/1.1 500 Unexpected Media Request\r\nContent-Length: 0\r\n\r\n",
        false,
    );
    let root = tempfile::tempdir().unwrap();
    let cfg = config(
        root.path(),
        serde_json::json!({"extractor":{
        "base-directory":"output", "archive":"missing-parent/archive.sqlite", "filesize-min":"1g"}}),
    );
    for mode in ["-g", "-j"] {
        let mut command = binary(root.path());
        command
            .arg("--native-config")
            .arg(&cfg)
            .args([mode, "--filesize-max", "0"])
            .arg(&server.url);
        let result = output(&mut command).await;
        success(&result);
        if mode == "-g" {
            assert_eq!(String::from_utf8(result.stdout).unwrap().trim(), server.url);
        } else {
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&result.stdout)
                    .unwrap()
                    .as_array()
                    .unwrap()
                    .len(),
                1
            );
        }
    }
    assert_eq!(server.requests(), 0);
    assert!(!root.path().join("output").exists());
    assert!(!root.path().join("missing-parent").exists());
}
