use gallery_dl_rs::{
    download::{self, Outcome},
    extract::{self, Media, PostMetadata},
};
use std::{
    io::{Read, Write},
    net::TcpListener,
    thread,
    time::{Duration, Instant, SystemTime},
};

const RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nContent-Length: 4\r\nLast-Modified: Sun, 06 Nov 1994 08:49:37 GMT\r\nConnection: close\r\n\r\njpeg";
const LAST_MODIFIED: u64 = 784111777;

fn item(url: &str, timestamp: Option<i64>) -> Media {
    let mut media = extract::direct(&extract::http_url(url).unwrap()).unwrap();
    media.category = "instagram".into();
    media.metadata = Some(PostMetadata {
        source_num: None,
        source_suffix: None,
        search_tags: None,
        keyword_updates: None,
        tags: None,
        sketch: None,
        sketch_archive: false,
        user_bookmark: None,
        bookmark_archive: false,
        ajax: None,
        mypixiv: None,
        pixiv_novel_metadata: None,
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
        timestamp,
        index: 1,
    });
    media
}

fn server() -> (String, thread::JoinHandle<()>) {
    server_with(RESPONSE.to_vec())
}

fn server_with(response: Vec<u8>) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let handle = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut socket = loop {
            match listener.accept() {
                Ok((socket, _)) => break socket,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "request never arrived");
                    thread::sleep(Duration::from_millis(5));
                }
                Err(e) => panic!("{e}"),
            }
        };
        socket.set_nonblocking(false).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut bytes = [0; 4096];
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            let read = socket.read(&mut bytes).unwrap();
            assert!(read > 0 && request.len() < 65536);
            request.extend_from_slice(&bytes[..read]);
        }
        let _ = socket.write_all(&response);
    });
    (format!("http://{address}/image.jpg"), handle)
}

fn modified(outcome: &Outcome) -> u64 {
    let Outcome::Downloaded(path) = outcome else {
        panic!("{outcome:?}");
    };
    std::fs::metadata(path)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

async fn fetch(client: download::DownloadClient, timestamp: Option<i64>) -> u64 {
    let root = tempfile::tempdir().unwrap();
    let (url, handle) = server();
    let outcome = download::download(&client, root.path(), &item(&url, timestamp), 0)
        .await
        .unwrap();
    handle.join().unwrap();
    modified(&outcome)
}

#[tokio::test]
async fn last_modified_sets_mtime_by_default_for_plain_and_resumable_downloads() {
    let client = download::client(Duration::from_secs(5)).unwrap();
    assert_eq!(fetch(client.clone(), None).await, LAST_MODIFIED);
    assert_eq!(fetch(client.with_resume(true), None).await, LAST_MODIFIED);
}

#[tokio::test]
async fn http_mtime_can_be_disabled() {
    let client = download::client(Duration::from_secs(5))
        .unwrap()
        .with_http_mtime(false);
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert!(fetch(client, None).await + 600 > now);
}

#[tokio::test]
async fn metadata_dates_override_http_and_absent_values_fall_back() {
    let client = download::client(Duration::from_secs(5))
        .unwrap()
        .with_metadata_mtime(Some("date".into()));
    assert_eq!(
        fetch(client.clone(), Some(1_600_000_000)).await,
        1_600_000_000
    );
    assert_eq!(
        fetch(client.clone().with_resume(true), Some(1_600_000_000)).await,
        1_600_000_000
    );
    assert_eq!(fetch(client, None).await, LAST_MODIFIED);
}

#[tokio::test]
async fn sleep_waits_before_network_downloads_but_not_existing_files() {
    let client = download::client(Duration::from_secs(5))
        .unwrap()
        .with_sleep(Some(
            gallery_dl_rs::sleep::SleepRange::parse("0.3").unwrap(),
        ));
    let root = tempfile::tempdir().unwrap();
    let (url, handle) = server();
    let media = item(&url, None);
    let start = Instant::now();
    let outcome = download::download(&client, root.path(), &media, 0)
        .await
        .unwrap();
    handle.join().unwrap();
    assert!(start.elapsed() >= Duration::from_millis(300));
    assert!(matches!(outcome, Outcome::Downloaded(_)));
    let start = Instant::now();
    let outcome = download::download(&client, root.path(), &media, 0)
        .await
        .unwrap();
    assert!(matches!(outcome, Outcome::Skipped(_)));
    assert!(start.elapsed() < Duration::from_millis(300));
}

#[tokio::test]
async fn rate_limits_slow_bodies_without_tripping_the_attempt_timeout() {
    let body = vec![b'x'; 3000];
    let mut response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(&body);
    let client = download::client(Duration::from_millis(500))
        .unwrap()
        .with_rate_limit(Some(2000));
    let root = tempfile::tempdir().unwrap();
    let (url, handle) = server_with(response);
    let start = Instant::now();
    let outcome = download::download(&client, root.path(), &item(&url, None), 0)
        .await
        .unwrap();
    handle.join().unwrap();
    // 3000 bytes at 2000 B/s take at least one second beyond the first chunk.
    assert!(
        start.elapsed() >= Duration::from_millis(900),
        "{:?}",
        start.elapsed()
    );
    let Outcome::Downloaded(path) = outcome else {
        panic!("{outcome:?}")
    };
    assert_eq!(std::fs::read(path).unwrap(), body);
}

#[cfg(not(windows))]
#[test]
fn exec_runs_after_downloaded_files_and_once_after_all() {
    let root = tempfile::tempdir().unwrap();
    let log = root.path().join("log.txt");
    let (url, handle) = server();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_gallery-dl-rs"))
        .arg("-d")
        .arg(root.path().join("out"))
        .arg("--exec")
        .arg(format!("wc -c < {{}} >> '{}'", log.display()))
        .arg("--exec-after")
        .arg(format!("echo done >> '{}'", log.display()))
        .arg(&url)
        .output()
        .unwrap();
    handle.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let log_text = std::fs::read_to_string(&log).unwrap();
    let lines: Vec<_> = log_text.lines().map(str::trim).collect();
    assert_eq!(lines, ["4", "done"]);
    // A failing command fails the run.
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_gallery-dl-rs"))
        .arg("--exec-after")
        .arg("exit 3")
        .args(["-d", root.path().join("empty").to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!output.status.success());
}
