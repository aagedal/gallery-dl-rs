use gallery_dl_rs::{
    archive::{Archive, ArchiveOutcome},
    download::{self, DownloadOptions, Outcome},
    extract::{self, Media, PostMetadata},
    network::NetworkOptions,
    pixiv::UgoiraFrame,
    postprocess::{Postprocessors, Specification, with_extension},
};
use serde_json::json;
use std::{
    fs,
    io::{Cursor, Read, Write},
    net::TcpListener,
    path::Path,
    thread,
    time::{Duration, Instant},
};
use zip::{CompressionMethod, ZipArchive, ZipWriter, write::SimpleFileOptions};

fn source_zip() -> Vec<u8> {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    for (name, bytes) in [
        ("000000.jpg", b"first frame".as_slice()),
        ("000001.jpg", b"second frame"),
    ] {
        writer
            .start_file(
                name,
                SimpleFileOptions::default().compression_method(CompressionMethod::Stored),
            )
            .unwrap();
        writer.write_all(bytes).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

fn serve(body: Vec<u8>) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let handle = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        let (mut socket, _) = loop {
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "ugoira HTTP request did not arrive"
                    );
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("{error}"),
            }
        };
        socket.set_nonblocking(false).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut request = Vec::new();
        let mut buffer = [0; 4096];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let count = socket.read(&mut buffer).unwrap();
            assert!(count > 0 && request.len() < 65_536);
            request.extend_from_slice(&buffer[..count]);
        }
        assert!(request.starts_with(b"GET /ugoira.zip "));
        write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/zip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
        socket.write_all(&body).unwrap();
    });
    (format!("http://{address}/ugoira.zip"), handle)
}

fn media(url: &str) -> Media {
    let mut media = extract::direct(&extract::http_url(url).unwrap()).unwrap();
    media.category = "pixiv".into();
    media.filename = "42.zip".into();
    media.metadata = Some(PostMetadata {
        post_id: "42".into(),
        media_id: "42_p0".into(),
        timestamp: Some(1_704_164_646),
        index: 1,
        frames: Some(
            vec![
                UgoiraFrame {
                    file: "000000.jpg".into(),
                    delay: 50,
                },
                UgoiraFrame {
                    file: "000001.jpg".into(),
                    delay: 125,
                },
            ]
            .into(),
        ),
        ..PostMetadata::default()
    });
    media
}

fn processors(base: &Path, options: serde_json::Value) -> Postprocessors {
    Postprocessors::new(vec![Specification::parse(&options, base).unwrap()])
}

fn client() -> download::DownloadClient {
    download::client(Duration::from_secs(5))
        .unwrap()
        .with_network(&NetworkOptions::new(Some(String::new()), None).unwrap())
        .unwrap()
}

fn verify_animation(path: &Path) {
    let mut zip = ZipArchive::new(fs::File::open(path).unwrap()).unwrap();
    assert_eq!(zip.len(), 3);
    let mut bytes = String::new();
    zip.by_name("animation.json")
        .unwrap()
        .read_to_string(&mut bytes)
        .unwrap();
    assert_eq!(
        bytes,
        "[{\"file\":\"000000.jpg\",\"delay\":50},{\"file\":\"000001.jpg\",\"delay\":125}]"
    );
}

#[tokio::test]
async fn archive_postprocessor_publishes_complete_zip_and_records_the_prepared_extension() {
    for extension in ["zip", "cbz"] {
        let root = tempfile::tempdir().unwrap();
        let source = source_zip();
        let (url, server) = serve(source);
        let media = media(&url);
        let processors = processors(
            root.path(),
            json!({"name":"ugoira","mode":"archive","extension":extension}),
        );
        let archive = Archive::open(root.path().join("archive.sqlite3"))
            .await
            .unwrap();
        let outcome = archive
            .download(
                &client(),
                root.path(),
                &media,
                DownloadOptions {
                    postprocessors: Some(&processors),
                    ..DownloadOptions::default()
                },
            )
            .await
            .unwrap();
        server.join().unwrap();
        let ArchiveOutcome::Completed(Outcome::Downloaded(path)) = outcome else {
            panic!("unexpected archive result: {outcome:?}")
        };
        assert_eq!(
            path,
            download::destination(
                root.path(),
                &client().effective_media(&with_extension(&media, extension))
            )
        );
        verify_animation(&path);
        let converted = with_extension(&media, extension);
        assert!(
            archive
                .contains(&converted.archive_key().unwrap())
                .await
                .unwrap()
        );
        if extension != "zip" {
            assert!(
                !download::destination(root.path(), &client().effective_media(&media)).exists()
            );
            assert!(
                !archive
                    .contains(&media.archive_key().unwrap())
                    .await
                    .unwrap()
            );
        }
        let mut refreshed = media.clone();
        refreshed.url = extract::http_url("http://127.0.0.1:1/never-request.zip").unwrap();
        assert_eq!(
            archive
                .download(
                    &client(),
                    root.path(),
                    &refreshed,
                    DownloadOptions {
                        postprocessors: Some(&processors),
                        ..DownloadOptions::default()
                    }
                )
                .await
                .unwrap(),
            ArchiveOutcome::AlreadyArchived
        );
    }
}

#[tokio::test]
async fn keep_files_retains_raw_zip_and_cbz_while_archiving_source_identity() {
    let root = tempfile::tempdir().unwrap();
    let source = source_zip();
    let (url, server) = serve(source.clone());
    let media = media(&url);
    let processors = processors(
        root.path(),
        json!({"name":"ugoira","mode":"archive","extension":"cbz","keep-files":true}),
    );
    let archive = Archive::open(root.path().join("archive.sqlite3"))
        .await
        .unwrap();
    let outcome = archive
        .download(
            &client(),
            root.path(),
            &media,
            DownloadOptions {
                postprocessors: Some(&processors),
                ..DownloadOptions::default()
            },
        )
        .await
        .unwrap();
    server.join().unwrap();
    let raw = download::destination(root.path(), &client().effective_media(&media));
    assert_eq!(
        outcome,
        ArchiveOutcome::Completed(Outcome::Downloaded(raw.clone()))
    );
    assert_eq!(fs::read(&raw).unwrap(), source);
    verify_animation(&download::destination(
        root.path(),
        &client().effective_media(&with_extension(&media, "cbz")),
    ));
    assert!(
        archive
            .contains(&media.archive_key().unwrap())
            .await
            .unwrap()
    );
    assert!(
        !archive
            .contains(&with_extension(&media, "cbz").archive_key().unwrap())
            .await
            .unwrap()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn encoder_failure_preserves_raw_zip_without_publishing_or_archiving_partial_output() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let encoder = root.path().join("failing encoder");
    fs::write(&encoder, b"#!/bin/sh\nfor value do output=\"$value\"; done\nprintf partial > \"$output\"\nprintf PRIVATE_ENCODER_ERROR >&2\nexit 7\n").unwrap();
    fs::set_permissions(&encoder, fs::Permissions::from_mode(0o700)).unwrap();
    let source = source_zip();
    let (url, server) = serve(source.clone());
    let media = media(&url);
    let processors = processors(
        root.path(),
        json!({"name":"ugoira","mode":"concat","extension":"webm","ffmpeg-location":encoder}),
    );
    let archive = Archive::open(root.path().join("archive.sqlite3"))
        .await
        .unwrap();
    let error = archive
        .download(
            &client(),
            root.path(),
            &media,
            DownloadOptions {
                postprocessors: Some(&processors),
                ..DownloadOptions::default()
            },
        )
        .await
        .unwrap_err()
        .to_string();
    server.join().unwrap();
    assert!(!error.contains("PRIVATE_ENCODER_ERROR"));
    assert_eq!(
        fs::read(download::destination(
            root.path(),
            &client().effective_media(&media)
        ))
        .unwrap(),
        source
    );
    assert!(
        !download::destination(
            root.path(),
            &client().effective_media(&with_extension(&media, "webm"))
        )
        .exists()
    );
    assert!(
        !archive
            .contains(&media.archive_key().unwrap())
            .await
            .unwrap()
    );
    assert!(
        !archive
            .contains(&with_extension(&media, "webm").archive_key().unwrap())
            .await
            .unwrap()
    );
    let public_files = fs::read_dir(root.path().join("pixiv"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    assert_eq!(
        public_files,
        [
            download::destination(root.path(), &client().effective_media(&media))
                .file_name()
                .unwrap()
                .to_owned()
        ]
    );
}

fn original(url: &str, index: usize) -> Media {
    let mut media = media(url);
    media.filename = format!("42_p{index}.jpg");
    let metadata = media.metadata.as_mut().unwrap();
    metadata.media_id = format!("42_p{index}");
    metadata.index = index + 1;
    metadata.ugoira_original = Some(true);
    metadata.ugoira_frame_index = Some(index);
    media
}

fn path(root: &Path, media: &Media) -> std::path::PathBuf {
    download::destination(root, &client().effective_media(media))
}

#[tokio::test]
async fn original_final_finishing_first_waits_for_downloaded_existing_or_archived_earlier_frame() {
    for earlier in ["download", "existing", "archived"] {
        let root = tempfile::tempdir().unwrap();
        let processors = processors(
            root.path(),
            json!({"name":"ugoira","mode":"archive","extension":"cbz"}),
        );
        let archive = Archive::open(root.path().join("archive.sqlite3"))
            .await
            .unwrap();
        let (last_url, last_server) = serve(b"second frame".to_vec());
        let last = original(&last_url, 1);
        let result = archive
            .download(
                &client(),
                root.path(),
                &last,
                DownloadOptions {
                    postprocessors: Some(&processors),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        last_server.join().unwrap();
        assert_eq!(
            result,
            ArchiveOutcome::Completed(Outcome::Downloaded(path(root.path(), &last)))
        );
        let mut converted = last.clone();
        converted.filename = "42_p0.cbz".into();
        converted.metadata.as_mut().unwrap().media_id = "42_p0".into();
        converted.metadata.as_mut().unwrap().index = 1;
        let output = path(root.path(), &converted);
        assert!(!output.exists(), "conversion must await the earlier frame");
        let (first, server) = if earlier == "download" {
            let (url, server) = serve(b"first frame".to_vec());
            (original(&url, 0), Some(server))
        } else {
            let first = original("http://127.0.0.1:1/never-request.jpg", 0);
            fs::write(path(root.path(), &first), b"first frame").unwrap();
            if earlier == "archived" {
                archive.record(&first.archive_key().unwrap()).await.unwrap();
            }
            (first, None)
        };
        let result = archive
            .download(
                &client(),
                root.path(),
                &first,
                DownloadOptions {
                    postprocessors: Some(&processors),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        if let Some(server) = server {
            server.join().unwrap();
        }
        assert_eq!(
            result,
            ArchiveOutcome::Completed(Outcome::Downloaded(output.clone())),
            "{earlier}"
        );
        verify_animation(&output);
        let mut zip = ZipArchive::new(fs::File::open(&output).unwrap()).unwrap();
        let mut first_bytes = Vec::new();
        zip.by_name("000000.jpg")
            .unwrap()
            .read_to_end(&mut first_bytes)
            .unwrap();
        assert_eq!(first_bytes, b"first frame");
        assert!(!path(root.path(), &first).exists());
        assert!(!path(root.path(), &last).exists());
    }
}

#[tokio::test]
async fn existing_or_archived_final_frame_never_runs_conversion() {
    for archived in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let processors = processors(
            root.path(),
            json!({"name":"ugoira","mode":"archive","extension":"cbz"}),
        );
        let archive = Archive::open(root.path().join("archive.sqlite3"))
            .await
            .unwrap();
        let first = original("http://127.0.0.1:1/never-first.jpg", 0);
        let last = original("http://127.0.0.1:1/never-last.jpg", 1);
        fs::create_dir_all(root.path().join("pixiv")).unwrap();
        fs::write(path(root.path(), &first), b"first frame").unwrap();
        fs::write(path(root.path(), &last), b"second frame").unwrap();
        if archived {
            archive.record(&last.archive_key().unwrap()).await.unwrap();
        }
        for media in [&first, &last] {
            archive
                .download(
                    &client(),
                    root.path(),
                    media,
                    DownloadOptions {
                        postprocessors: Some(&processors),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
        }
        assert_eq!(fs::read_dir(root.path().join("pixiv")).unwrap().count(), 2);
        assert_eq!(fs::read(path(root.path(), &first)).unwrap(), b"first frame");
        assert_eq!(fs::read(path(root.path(), &last)).unwrap(), b"second frame");
    }
}

#[tokio::test]
async fn same_extension_failed_archive_retains_input_without_publishing_primary() {
    let root = tempfile::tempdir().unwrap();
    let source = b"invalid ZIP source retained for retry".to_vec();
    let (url, server) = serve(source.clone());
    let media = media(&url);
    let processors = processors(root.path(), json!({"name":"ugoira","mode":"archive"}));
    let archive = Archive::open(root.path().join("archive.sqlite3"))
        .await
        .unwrap();
    assert!(
        archive
            .download(
                &client(),
                root.path(),
                &media,
                DownloadOptions {
                    postprocessors: Some(&processors),
                    ..Default::default()
                }
            )
            .await
            .is_err()
    );
    server.join().unwrap();
    assert!(!path(root.path(), &media).exists());
    assert!(
        !archive
            .contains(&media.archive_key().unwrap())
            .await
            .unwrap()
    );
    let entries = fs::read_dir(root.path().join("pixiv"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect::<Vec<_>>();
    assert_eq!(entries.len(), 1);
    assert!(
        entries[0]
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with(".gallery-dl-ugoira-source-")
    );
    assert_eq!(fs::read(&entries[0]).unwrap(), source);
}

#[tokio::test]
async fn interrupted_zip_resumes_from_stable_public_checkpoint_before_private_conversion() {
    let root = tempfile::tempdir().unwrap();
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    for name in ["000000.jpg", "000001.jpg"] {
        writer
            .start_file(
                name,
                SimpleFileOptions::default().compression_method(CompressionMethod::Stored),
            )
            .unwrap();
        writer.write_all(&vec![b'x'; 32_768]).unwrap();
    }
    let body = writer.finish().unwrap().into_inner();
    let split = 16_384;
    let prefix = body[..split].to_vec();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let server = thread::spawn(move || {
        for attempt in 0..2 {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "resume request did not arrive");
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            socket.set_nonblocking(false).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                let mut bytes = [0; 4096];
                let count = socket.read(&mut bytes).unwrap();
                assert!(count > 0 && request.len() < 65_536);
                request.extend_from_slice(&bytes[..count]);
            }
            let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
            if attempt == 0 {
                assert!(!request.contains("range:"));
                write!(socket, "HTTP/1.1 200 OK\r\nETag: \"zip-v1\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
                socket.write_all(&body[..split]).unwrap();
                socket.flush().unwrap();
                thread::sleep(Duration::from_millis(75));
            } else {
                assert!(
                    request.contains(&format!("range: bytes={split}-\r\n")),
                    "{request}"
                );
                assert!(request.contains("if-range: \"zip-v1\"\r\n"), "{request}");
                write!(socket, "HTTP/1.1 206 Partial Content\r\nETag: \"zip-v1\"\r\nContent-Range: bytes {}-{}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", split, body.len()-1, body.len(), body.len()-split).unwrap();
                socket.write_all(&body[split..]).unwrap();
            }
        }
    });
    let media = media(&format!("http://{address}/ugoira.zip"));
    let processors = processors(
        root.path(),
        json!({"name":"ugoira","mode":"archive","extension":"cbz"}),
    );
    let client = client().with_resume(true);
    let options = DownloadOptions {
        postprocessors: Some(&processors),
        ..Default::default()
    };
    assert!(
        download::download_with_options(&client, root.path(), &media, options)
            .await
            .is_err()
    );
    let checkpoints = root.path().join("pixiv/.gallery-dl-rs");
    let parts = fs::read_dir(&checkpoints)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "part"))
        .collect::<Vec<_>>();
    assert_eq!(parts.len(), 1);
    assert_eq!(fs::read(&parts[0]).unwrap(), prefix);
    let outcome = download::download_with_options(&client, root.path(), &media, options)
        .await
        .unwrap();
    server.join().unwrap();
    let output = path(root.path(), &with_extension(&media, "cbz"));
    assert_eq!(outcome, Outcome::Downloaded(output.clone()));
    verify_animation(&output);
    assert!(!parts[0].exists());
}

#[tokio::test]
async fn chained_extension_filters_publish_each_selected_output_and_return_existing_final_path() {
    let root = tempfile::tempdir().unwrap();
    let (url, server) = serve(source_zip());
    let media = media(&url);
    let processors = Postprocessors::new([
        json!({"name":"ugoira","mode":"archive","extension":"cbz"}),
        json!({"name":"ugoira","mode":"archive","extension":"zip","filter":"extension == 'cbz'"}),
    ].iter().map(|value| Specification::parse(value, root.path()).unwrap()).collect());
    let outcome = download::download_with_options(
        &client(),
        root.path(),
        &media,
        DownloadOptions {
            postprocessors: Some(&processors),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    server.join().unwrap();
    let final_path = path(root.path(), &media);
    assert_eq!(outcome, Outcome::Downloaded(final_path.clone()));
    verify_animation(&final_path);
    verify_animation(&path(root.path(), &with_extension(&media, "cbz")));
}
