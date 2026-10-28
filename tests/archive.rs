use gallery_dl_rs::{
    archive::{Archive, ArchiveOutcome},
    download::{self, DownloadOptions, Outcome},
    extract::{self, Media, PostMetadata},
};
use rusqlite::Connection;
use std::{
    io::{Read, Write},
    net::TcpListener,
    path::Path,
    process::Command,
    thread,
    time::{Duration, Instant},
};

fn item(url: &str) -> Media {
    let mut media = extract::direct(&extract::http_url(url).unwrap()).unwrap();
    media.category = "instagram".into();
    media.metadata = Some(PostMetadata {
        site_keywords: None,
        source_num: None,
        booru_keywords: None,
        source_defaults: None,
        booru_user_id: None,
        booru_pool_name: None,
        booru_favorite_id: None,
        booru_artist_parent: None,
        booru_popular: None,
        source_suffix: None,
        search_tags: None,
        pool: None,
        favgroup: None,
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
        timestamp: None,
        index: 1,
    });
    media
}

fn server(response: &'static [u8]) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let handle = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
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
        socket.write_all(response).unwrap();
    });
    (format!("http://{address}/image.jpg"), handle)
}

fn records(path: &Path) -> i64 {
    Connection::open(path)
        .unwrap()
        .query_row("SELECT count(*) FROM completed_media", [], |row| row.get(0))
        .unwrap()
}

#[tokio::test]
async fn archive_persists_and_concurrent_connections_record_idempotently() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("archive.sqlite3");
    let (a, b) = tokio::join!(Archive::open(&path), Archive::open(&path));
    let (a, b) = (a.unwrap(), b.unwrap());
    assert!(!a.contains("first").await.unwrap());
    let (one, two, three) = tokio::join!(a.record("first"), b.record("first"), a.record("second"));
    one.unwrap();
    two.unwrap();
    three.unwrap();
    assert_eq!(records(&path), 2);
    drop((a, b));
    let reopened = Archive::open(&path).await.unwrap();
    assert!(reopened.contains("first").await.unwrap());
    assert!(reopened.contains("second").await.unwrap());
    // Keys are bound parameters, never interpreted as SQL.
    reopened
        .record("'); DROP TABLE completed_media;--")
        .await
        .unwrap();
    assert_eq!(records(&path), 3);
}

#[tokio::test]
async fn incompatible_archives_are_rejected_without_altering_their_contents() {
    let root = tempfile::tempdir().unwrap();
    // Python gallery-dl archives are used as they are.
    let foreign = root.path().join("python.sqlite3");
    let connection = Connection::open(&foreign).unwrap();
    connection.execute_batch("CREATE TABLE archive (entry TEXT PRIMARY KEY); INSERT INTO archive VALUES ('existing');").unwrap();
    let python = Archive::open(&foreign).await.unwrap();
    assert!(python.is_upstream());
    assert!(python.contains("existing").await.unwrap());
    let other = root.path().join("other.sqlite3");
    Connection::open(&other)
        .unwrap()
        .execute_batch("CREATE TABLE things (id INTEGER)")
        .unwrap();
    assert!(Archive::open(&other).await.is_err());
    let value: String = connection
        .query_row("SELECT entry FROM archive", [], |r| r.get(0))
        .unwrap();
    assert_eq!(value, "existing");
    let app: i64 = connection
        .query_row("PRAGMA application_id", [], |r| r.get(0))
        .unwrap();
    assert_eq!(app, 0);
    let future = root.path().join("future.sqlite3");
    drop(Archive::open(&future).await.unwrap());
    Connection::open(&future)
        .unwrap()
        .execute_batch("PRAGMA user_version = 999")
        .unwrap();
    assert!(Archive::open(&future).await.is_err());
    let invalid = root.path().join("invalid.sqlite3");
    std::fs::write(&invalid, b"this is not SQLite").unwrap();
    assert!(Archive::open(&invalid).await.is_err());
    assert_eq!(std::fs::read(&invalid).unwrap(), b"this is not SQLite");
}

#[tokio::test]
async fn refreshed_social_urls_skip_without_requests_even_after_saved_media_is_removed() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("archive.sqlite3");
    let archive = Archive::open(&path).await.unwrap();
    let client = download::client(Duration::from_secs(2)).unwrap();
    let (url, handle) = server(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nabc");
    let media = item(&format!("{url}?token=PRIVATE_TOKEN"));
    let saved = download::destination(root.path(), &media);
    let result = archive
        .download(&client, root.path(), &media, DownloadOptions::default())
        .await
        .unwrap();
    assert_eq!(
        result,
        ArchiveOutcome::Completed(Outcome::Downloaded(saved.clone()))
    );
    handle.join().unwrap();
    std::fs::remove_file(saved).unwrap();
    let refreshed = item("http://127.0.0.1:1/refreshed.jpg?token=NEW_PRIVATE_TOKEN");
    assert_eq!(
        archive
            .download(&client, root.path(), &refreshed, DownloadOptions::default())
            .await
            .unwrap(),
        ArchiveOutcome::AlreadyArchived
    );
    assert_eq!(records(&path), 1);
    let db = std::fs::read(&path).unwrap();
    assert!(!String::from_utf8_lossy(&db).contains("PRIVATE_TOKEN"));
}

#[tokio::test]
async fn failures_and_missing_identities_do_not_enter_the_archive() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("archive.sqlite3");
    let archive = Archive::open(&path).await.unwrap();
    let client = download::client(Duration::from_secs(2)).unwrap();
    for response in [
        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".as_slice(),
        b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nabc".as_slice(),
    ] {
        let (url, handle) = server(response);
        assert!(
            archive
                .download(
                    &client,
                    root.path(),
                    &item(&url),
                    DownloadOptions::default()
                )
                .await
                .is_err()
        );
        handle.join().unwrap();
        assert_eq!(records(&path), 0);
    }
    let (url, handle) = server(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nabc");
    let mut unknown = item(&url);
    unknown.metadata = None;
    assert!(matches!(
        archive
            .download(&client, root.path(), &unknown, DownloadOptions::default())
            .await
            .unwrap(),
        ArchiveOutcome::Completed(Outcome::Downloaded(_))
    ));
    handle.join().unwrap();
    assert_eq!(records(&path), 0);
    let manifest =
        extract::direct(&extract::http_url("http://127.0.0.1:1/media.m3u8").unwrap()).unwrap();
    assert!(
        archive
            .download(&client, root.path(), &manifest, DownloadOptions::default())
            .await
            .is_err()
    );
    assert_eq!(records(&path), 0);
}

#[tokio::test]
async fn metadata_errors_remain_repairable_before_archiving_existing_media() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("archive.sqlite3");
    let archive = Archive::open(&path).await.unwrap();
    let client = download::client(Duration::from_secs(2)).unwrap();
    let media = item("http://127.0.0.1:1/image.jpg");
    let saved = download::destination(root.path(), &media);
    std::fs::create_dir_all(saved.parent().unwrap()).unwrap();
    std::fs::write(&saved, b"existing").unwrap();
    let sidecar = saved.with_file_name(format!(
        "{}.json",
        saved.file_name().unwrap().to_str().unwrap()
    ));
    std::fs::create_dir(&sidecar).unwrap();
    let options = DownloadOptions {
        write_metadata: true,
        ..Default::default()
    };
    assert!(
        archive
            .download(&client, root.path(), &media, options)
            .await
            .is_err()
    );
    assert_eq!(records(&path), 0);
    std::fs::remove_dir(&sidecar).unwrap();
    assert_eq!(
        archive
            .download(&client, root.path(), &media, options)
            .await
            .unwrap(),
        ArchiveOutcome::Completed(Outcome::Skipped(saved))
    );
    assert!(sidecar.is_file());
    assert_eq!(records(&path), 1);
}

#[tokio::test]
async fn archive_write_errors_report_saved_media_without_removing_it() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("archive.sqlite3");
    let archive = Archive::open(&path).await.unwrap();
    let connection = Connection::open(&path).unwrap();
    connection.execute_batch("CREATE TRIGGER fail_writes BEFORE INSERT ON completed_media BEGIN SELECT RAISE(FAIL, 'test write failure'); END;").unwrap();
    let client = download::client(Duration::from_secs(2)).unwrap();
    let (url, handle) = server(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nabc");
    let media = item(&url);
    let error = archive
        .download(&client, root.path(), &media, DownloadOptions::default())
        .await
        .unwrap_err()
        .to_string();
    handle.join().unwrap();
    assert!(error.contains("download archive update failed"), "{error}");
    assert_eq!(
        std::fs::read(download::destination(root.path(), &media)).unwrap(),
        b"abc"
    );
    assert_eq!(records(&path), 0);
    connection
        .execute_batch("DROP TRIGGER fail_writes")
        .unwrap();
    assert!(matches!(
        archive
            .download(&client, root.path(), &media, DownloadOptions::default())
            .await
            .unwrap(),
        ArchiveOutcome::Completed(Outcome::Skipped(_))
    ));
    assert_eq!(records(&path), 1);
}

#[tokio::test]
async fn destination_directories_are_not_recorded_as_completed_files() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("archive.sqlite3");
    let archive = Archive::open(&path).await.unwrap();
    let media = item("http://127.0.0.1:1/image.jpg");
    let saved = download::destination(root.path(), &media);
    std::fs::create_dir_all(&saved).unwrap();
    let client = download::client(Duration::from_secs(2)).unwrap();
    assert!(
        archive
            .download(&client, root.path(), &media, DownloadOptions::default())
            .await
            .unwrap_err()
            .to_string()
            .contains("not a regular file")
    );
    assert_eq!(records(&path), 0);
}

#[test]
fn cli_archive_is_opt_in_persistent_and_conflicts_with_inspection() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("archive.sqlite3");
    let url = "http://127.0.0.1:1/image.jpg";
    let media = extract::direct(&extract::http_url(url).unwrap()).unwrap();
    let saved = download::destination(root.path(), &media);
    std::fs::create_dir_all(saved.parent().unwrap()).unwrap();
    std::fs::write(&saved, b"existing").unwrap();
    let run = || {
        Command::new(env!("CARGO_BIN_EXE_gallery-dl-rs"))
            .arg("--download-archive")
            .arg(&path)
            .arg("-d")
            .arg(root.path())
            .arg(url)
            .output()
            .unwrap()
    };
    let first = run();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(records(&path), 1);
    std::fs::remove_file(saved).unwrap();
    let second = run();
    assert!(second.status.success());
    assert!(String::from_utf8_lossy(&second.stderr).contains("skipped archived media"));
    for mode in ["--dump-media-json", "-g"] {
        let output = Command::new(env!("CARGO_BIN_EXE_gallery-dl-rs"))
            .arg("--download-archive")
            .arg(root.path().join("must-not-exist.sqlite3"))
            .args([mode, url])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(!root.path().join("must-not-exist.sqlite3").exists());
    }
}

#[tokio::test]
async fn concurrent_refreshed_urls_wait_for_successful_publication_before_skipping() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("archive.sqlite3");
    let archive = Archive::open(&path).await.unwrap();
    let client = download::client(Duration::from_secs(2)).unwrap();
    let (url, handle) = server(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nabc");
    let first = item(&url);
    // This URL must never be requested after the first download succeeds.
    let refreshed = item("http://127.0.0.1:1/refreshed.jpg");
    let (first, second) = tokio::join!(
        archive.download(&client, root.path(), &first, DownloadOptions::default()),
        archive.download(&client, root.path(), &refreshed, DownloadOptions::default()),
    );
    handle.join().unwrap();
    assert!(matches!(
        first.unwrap(),
        ArchiveOutcome::Completed(Outcome::Downloaded(_))
    ));
    assert_eq!(second.unwrap(), ArchiveOutcome::AlreadyArchived);
    assert_eq!(records(&path), 1);
}

#[tokio::test]
async fn failed_handoff_does_not_suppress_a_later_direct_representation() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("archive.sqlite3");
    let archive = Archive::open(&path).await.unwrap();
    let client = download::client(Duration::from_secs(2))
        .unwrap()
        .with_ytdl(None);
    let mut manifest = item("http://127.0.0.1:1/thumbnail.jpg");
    manifest.job = extract::MediaJob::VideoPage {
        page_url: manifest.url.clone(),
    };
    let (url, handle) = server(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nabc");
    let direct = item(&url);
    let (failed, successful) = tokio::join!(
        archive.download(&client, root.path(), &manifest, DownloadOptions::default()),
        archive.download(&client, root.path(), &direct, DownloadOptions::default()),
    );
    handle.join().unwrap();
    assert!(failed.is_err());
    assert!(matches!(
        successful.unwrap(),
        ArchiveOutcome::Completed(Outcome::Downloaded(_))
    ));
    assert_eq!(records(&path), 1);
}

#[cfg(unix)]
#[test]
fn cli_does_not_deduplicate_a_failed_representation_by_its_stable_id() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("archive.sqlite3");
    let (url, handle) = server(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nabc");
    let messages = serde_json::json!([
        [3, "ytdl:https://example.com/video", {"tweet_id": 123, "num": 1, "filename": "media", "extension": "mp4"}],
        [3, url, {"tweet_id": 123, "num": 1, "filename": "media", "extension": "mp4"}]
    ]);
    let fixture = root.path().join("messages.json");
    std::fs::write(&fixture, serde_json::to_vec(&messages).unwrap()).unwrap();
    let script = root.path().join("backend");
    std::fs::write(&script, format!("#!/bin/sh\ncat '{}'\n", fixture.display())).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_gallery-dl-rs"))
        .arg("--download-archive")
        .arg(&path)
        .arg("--extractor-command")
        .arg(&script)
        .arg("--no-ytdl")
        .arg("-d")
        .arg(root.path())
        .args(["--jobs", "2", "https://x.com/user/status/123"])
        .output()
        .unwrap();
    handle.join().unwrap();
    assert_eq!(output.status.code(), Some(4)); // first job still reports its error
    assert!(!String::from_utf8_lossy(&output.stdout).trim().is_empty());
    assert_eq!(records(&path), 1);
}

#[tokio::test]
async fn custom_names_publish_metadata_and_use_the_same_stable_archive() {
    use gallery_dl_rs::naming::FilenameTemplate;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("archive.sqlite3");
    let archive = Archive::open(&path).await.unwrap();
    let client = download::client(Duration::from_secs(2)).unwrap();
    let template = FilenameTemplate::parse("{post_id}_{media_id}_{index:03}.{extension}").unwrap();
    let (url, handle) = server(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nabc");
    let media = item(&url);
    let saved = root.path().join("instagram/100_101_001.jpg");
    assert_eq!(
        download::destination_with_template(root.path(), &media, &template).unwrap(),
        saved
    );
    let options = DownloadOptions {
        write_metadata: true,
        ..Default::default()
    };
    let result = archive
        .download_with_template(&client, root.path(), &media, options, Some(&template))
        .await
        .unwrap();
    handle.join().unwrap();
    assert_eq!(
        result,
        ArchiveOutcome::Completed(Outcome::Downloaded(saved.clone()))
    );
    assert_eq!(std::fs::read(&saved).unwrap(), b"abc");
    let sidecar: serde_json::Value =
        serde_json::from_slice(&std::fs::read(saved.with_extension("jpg.json")).unwrap()).unwrap();
    assert_eq!(sidecar["filename"], "100_101_001.jpg");
    let other_template = FilenameTemplate::parse("different.jpg").unwrap();
    let refreshed = item("http://127.0.0.1:1/refreshed.jpg");
    assert_eq!(
        archive
            .download_with_template(
                &client,
                root.path(),
                &refreshed,
                options,
                Some(&other_template)
            )
            .await
            .unwrap(),
        ArchiveOutcome::AlreadyArchived
    );
    assert!(!root.path().join("instagram/different.jpg").exists());
}

#[tokio::test]
async fn invalid_per_media_names_fail_before_requests_or_archive_records() {
    use gallery_dl_rs::naming::FilenameTemplate;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("archive.sqlite3");
    let archive = Archive::open(&path).await.unwrap();
    let client = download::client(Duration::from_secs(2)).unwrap();
    let media = item("http://127.0.0.1:1/image.jpg");
    let template = FilenameTemplate::parse("{username}_{media_id}.jpg").unwrap();
    let result = archive
        .download_with_template(
            &client,
            root.path(),
            &media,
            DownloadOptions::default(),
            Some(&template),
        )
        .await;
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("username is missing")
    );
    assert_eq!(records(&path), 0);
    assert!(!root.path().join("instagram").exists());
}

#[test]
fn cli_native_names_skip_without_overwriting_and_repair_metadata() {
    let root = tempfile::tempdir().unwrap();
    let saved = root.path().join("direct/picture.jpg");
    std::fs::create_dir_all(saved.parent().unwrap()).unwrap();
    std::fs::write(&saved, b"keep existing").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_gallery-dl-rs"))
        .arg("-d")
        .arg(root.path())
        .args([
            "-f",
            "{name}.{extension}",
            "--write-metadata",
            "http://127.0.0.1:1/picture.jpg",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::read(&saved).unwrap(), b"keep existing");
    assert!(saved.with_extension("jpg.json").is_file());
    let invalid_archive = root.path().join("must-not-exist.sqlite3");
    let output = Command::new(env!("CARGO_BIN_EXE_gallery-dl-rs"))
        .arg("--download-archive")
        .arg(&invalid_archive)
        .args(["-f", "../{filename}", "http://127.0.0.1:1/picture.jpg"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(!invalid_archive.exists());
    for mode in ["--dump-media-json", "-g"] {
        let output = Command::new(env!("CARGO_BIN_EXE_gallery-dl-rs"))
            .args([mode, "-f", "{filename}", "https://example.com/a.jpg"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
    }
}

#[tokio::test]
async fn cancellation_does_not_archive_partial_media_or_keep_its_identity_locked() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("archive.sqlite3");
    let archive = Archive::open(&path).await.unwrap();
    let client = download::client(Duration::from_secs(2)).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/image.jpg", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let (ready, received) = tokio::sync::oneshot::channel();
    let (release, held) = std::sync::mpsc::channel();
    let handle = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut socket = loop {
            match listener.accept() {
                Ok((socket, _)) => break socket,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline);
                    thread::sleep(Duration::from_millis(5));
                }
                Err(e) => panic!("{e}"),
            }
        };
        // Answer only after the full request so the client is reading a response.
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
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nabc")
            .unwrap();
        ready.send(()).unwrap();
        held.recv_timeout(Duration::from_secs(5)).unwrap();
    });
    let media = item(&url);
    let mut pending =
        Box::pin(archive.download(&client, root.path(), &media, DownloadOptions::default()));
    tokio::select! {
        result = &mut pending => panic!("partial response completed: {result:?}"),
        result = received => result.unwrap(),
    }
    // Keep polling while the response is held open to enter the streaming phase.
    tokio::select! {
        result = &mut pending => panic!("partial response completed: {result:?}"),
        _ = tokio::time::sleep(Duration::from_millis(20)) => {},
    }
    drop(pending);
    release.send(()).unwrap();
    handle.join().unwrap();
    assert_eq!(records(&path), 0);
    assert!(!download::destination(root.path(), &media).exists());
    assert_eq!(
        std::fs::read_dir(root.path().join("instagram"))
            .unwrap()
            .count(),
        0
    );
    let (url, handle) = server(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nabc");
    let refreshed = item(&url);
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        archive.download(&client, root.path(), &refreshed, DownloadOptions::default()),
    )
    .await
    .unwrap()
    .unwrap();
    handle.join().unwrap();
    assert!(matches!(
        result,
        ArchiveOutcome::Completed(Outcome::Downloaded(_))
    ));
    assert_eq!(records(&path), 1);
}

#[test]
fn terminate_stops_after_consecutive_skipped_files() {
    let root = tempfile::tempdir().unwrap();
    let urls = [
        "http://127.0.0.1:1/one.jpg",
        "http://127.0.0.1:1/two.jpg",
        "http://127.0.0.1:1/missing.jpg",
    ];
    for url in &urls[..2] {
        let media = extract::direct(&extract::http_url(url).unwrap()).unwrap();
        let saved = download::destination(root.path(), &media);
        std::fs::create_dir_all(saved.parent().unwrap()).unwrap();
        std::fs::write(&saved, b"existing").unwrap();
    }
    let run = |extra: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_gallery-dl-rs"))
            .args(["--jobs", "1", "--retries", "0"])
            .args(extra)
            .arg("-d")
            .arg(root.path())
            .args(urls)
            .output()
            .unwrap()
    };
    let output = run(&["-T", "2"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(stderr.contains("stopping after 2 consecutive skipped files"));
    assert!(!stderr.contains("download failed"), "{stderr}");
    // Without a limit, the unreachable third file is attempted and fails.
    assert!(!run(&[]).status.success());
    assert!(!run(&["-T", "3"]).status.success());
    assert_eq!(run(&["-T", "0"]).status.code(), Some(2));
}

#[test]
fn abort_stops_only_the_input_with_consecutive_skips() {
    let root = tempfile::tempdir().unwrap();
    let existing = "http://127.0.0.1:1/existing.jpg";
    let media = extract::direct(&extract::http_url(existing).unwrap()).unwrap();
    let saved = download::destination(root.path(), &media);
    std::fs::create_dir_all(saved.parent().unwrap()).unwrap();
    std::fs::write(&saved, b"existing").unwrap();
    let run = |flag: &str| {
        Command::new(env!("CARGO_BIN_EXE_gallery-dl-rs"))
            .args(["--jobs", "1", "--retries", "0", flag, "1", "-d"])
            .arg(root.path())
            .args([existing, "http://127.0.0.1:1/missing.jpg"])
            .output()
            .unwrap()
    };
    let output = run("-A");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("stopping input 1 after 1 consecutive skipped files"),
        "{stderr}"
    );
    // The second input still runs and fails against the unreachable server.
    assert!(!output.status.success());
    assert!(stderr.contains("download failed"), "{stderr}");
    assert!(run("-T").status.success());
}
