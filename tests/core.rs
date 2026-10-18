use gallery_dl_rs::{
    download::{self, Outcome},
    extract::{self, Media},
};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::TcpListener,
    path::Path,
    thread,
    time::Duration,
};

fn media(url: &str) -> Media {
    Media {
        url: extract::http_url(url).unwrap(),
        job: extract::MediaJob::DirectHttp,
        handoff: BTreeMap::new(),
        category: "twitter".into(),
        filename: "photo.jpg".into(),
        metadata: None,
        headers: BTreeMap::new(),
    }
}

fn with_metadata() -> download::DownloadOptions<'static> {
    download::DownloadOptions {
        retries: 0,
        write_metadata: true,
        ..Default::default()
    }
}

fn sidecar(path: &Path) -> std::path::PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".json");
    name.into()
}

#[tokio::test]
async fn metadata_sidecars_preserve_post_fields_without_transport_secrets() {
    let (url, handle) = server(vec!["HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nabc"]);
    let mut item = media(&format!("{url}?token=SIGNED_SECRET"));
    item.headers.insert("Cookie".into(), "COOKIE_SECRET".into());
    item.handoff
        .insert("_fallback".into(), "FALLBACK_SECRET".into());
    item.metadata = Some(gallery_dl_rs::extract::PostMetadata {
        retweet_id: None,
        quoted_by_id: None,
        reposted_by_id: None,
        article: None,
        post_id: "post-1".into(),
        media_id: "media-2".into(),
        shortcode: "example".into(),
        username: Some("person".into()),
        caption: Some("A caption 🐈".into()),
        timestamp: Some(123),
        index: 2,
    });
    let root = tempfile::tempdir().unwrap();
    let client = download::client(Duration::from_secs(5)).unwrap();
    let path = download::destination(root.path(), &item);
    assert_eq!(
        download::download_with_options(&client, root.path(), &item, with_metadata())
            .await
            .unwrap(),
        Outcome::Downloaded(path.clone())
    );
    handle.join().unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"abc");
    let bytes = std::fs::read(sidecar(&path)).unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        json,
        serde_json::json!({
            "schema_version": 1,
            "category": "twitter",
            "filename": path.file_name().unwrap().to_str().unwrap(),
            "metadata": {
                "post_id": "post-1", "media_id": "media-2", "shortcode": "example",
                "username": "person", "caption": "A caption 🐈", "timestamp": 123, "index": 2
            }
        })
    );
    assert!(!String::from_utf8(bytes).unwrap().contains("SECRET"));
    assert_eq!(
        std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
        2
    );
}

#[tokio::test]
async fn metadata_backfills_existing_media_and_concurrent_writers_do_not_clobber() {
    // No listener: an attempted HTTP request would fail.
    let item = media("http://127.0.0.1:1/image.jpg");
    let root = tempfile::tempdir().unwrap();
    let path = download::destination(root.path(), &item);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"existing media").unwrap();
    let client = download::client(Duration::from_secs(1)).unwrap();
    let (first, second) = tokio::join!(
        download::download_with_options(&client, root.path(), &item, with_metadata()),
        download::download_with_options(&client, root.path(), &item, with_metadata()),
    );
    assert_eq!(first.unwrap(), Outcome::Skipped(path.clone()));
    assert_eq!(second.unwrap(), Outcome::Skipped(path.clone()));
    let json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(sidecar(&path)).unwrap()).unwrap();
    assert_eq!(json["schema_version"], 1);
    assert!(json.get("metadata").is_none());
    std::fs::write(sidecar(&path), b"user-edited sidecar").unwrap();
    download::download_with_options(&client, root.path(), &item, with_metadata())
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(sidecar(&path)).unwrap(),
        b"user-edited sidecar"
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"existing media");
    assert_eq!(
        std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
        2
    );
}

#[tokio::test]
async fn metadata_failure_keeps_completed_media_and_can_be_repaired() {
    let (url, handle) = server(vec!["HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nabc"]);
    let item = media(&url);
    let root = tempfile::tempdir().unwrap();
    let path = download::destination(root.path(), &item);
    std::fs::create_dir_all(sidecar(&path)).unwrap();
    let client = download::client(Duration::from_secs(5)).unwrap();
    let error = download::download_with_options(
        &client,
        root.path(),
        &item,
        download::DownloadOptions {
            retries: 2,
            write_metadata: true,
            ..Default::default()
        },
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("metadata sidecar failed"), "{error}");
    assert_eq!(std::fs::read(&path).unwrap(), b"abc");
    assert_eq!(handle.join().unwrap().len(), 1);
    std::fs::remove_dir(sidecar(&path)).unwrap();
    assert_eq!(
        download::download_with_options(&client, root.path(), &item, with_metadata())
            .await
            .unwrap(),
        Outcome::Skipped(path.clone())
    );
    assert!(sidecar(&path).is_file());
}

#[tokio::test]
async fn failed_downloads_and_unsupported_jobs_never_write_metadata() {
    for response in [
        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
        "HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nabc",
    ] {
        let (url, handle) = server(vec![response]);
        let item = media(&url);
        let root = tempfile::tempdir().unwrap();
        let client = download::client(Duration::from_secs(5)).unwrap();
        assert!(
            download::download_with_options(&client, root.path(), &item, with_metadata())
                .await
                .is_err()
        );
        handle.join().unwrap();
        let path = download::destination(root.path(), &item);
        assert!(!path.exists());
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            0
        );
    }
    let item =
        extract::direct(&extract::http_url("http://127.0.0.1:1/stream.m3u8").unwrap()).unwrap();
    let root = tempfile::tempdir().unwrap();
    let client = download::client(Duration::from_secs(1)).unwrap();
    assert!(
        download::download_with_options(&client, root.path(), &item, with_metadata())
            .await
            .is_err()
    );
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[cfg(unix)]
#[tokio::test]
async fn metadata_rejects_existing_and_dangling_sidecar_symlinks() {
    for dangling in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let item = media("http://127.0.0.1:1/image.jpg");
        let path = download::destination(root.path(), &item);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"existing media").unwrap();
        let target = root.path().join("target.json");
        if !dangling {
            std::fs::write(&target, b"untouched").unwrap();
        }
        std::os::unix::fs::symlink(&target, sidecar(&path)).unwrap();
        let client = download::client(Duration::from_secs(1)).unwrap();
        let error = download::download_with_options(&client, root.path(), &item, with_metadata())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("not a regular file"), "{error}");
        assert_eq!(std::fs::read_link(sidecar(&path)).unwrap(), target);
        assert_eq!(std::fs::read(&path).unwrap(), b"existing media");
        if dangling {
            assert!(!target.exists());
        } else {
            assert_eq!(std::fs::read(target).unwrap(), b"untouched");
        }
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            2
        );
    }
}

/// Small deterministic HTTP server: no external services or account credentials.
fn server(responses: Vec<&'static str>) -> (String, thread::JoinHandle<Vec<String>>) {
    delayed_server(
        responses
            .into_iter()
            .map(|response| (response, Duration::ZERO))
            .collect(),
    )
}

fn delayed_server(
    responses: Vec<(&'static str, Duration)>,
) -> (String, thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let handle = thread::spawn(move || {
        let mut requests = Vec::new();
        for (response, delay) in responses {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            let mut socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(std::time::Instant::now() < deadline, "server timed out");
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(e) => panic!("{e}"),
                }
            };
            // Windows can inherit the listener's nonblocking mode.
            socket.set_nonblocking(false).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            while !request.windows(4).any(|part| part == b"\r\n\r\n") {
                let count = socket.read(&mut buffer).unwrap();
                assert_ne!(count, 0);
                request.extend_from_slice(&buffer[..count]);
            }
            requests.push(String::from_utf8(request).unwrap());
            thread::sleep(delay);
            let _ = socket.write_all(response.as_bytes());
        }
        requests
    });
    (format!("http://{address}/image.jpg"), handle)
}

#[test]
fn routes_social_hosts_without_lookalike_domains() {
    for (url, expected) in [
        ("https://www.instagram.com/p/abc/", Some("instagram")),
        ("https://m.facebook.com/photo.php?fbid=1", Some("facebook")),
        ("https://fb.watch/abc/", Some("facebook")),
        ("https://x.com/user/status/123", Some("twitter")),
        ("https://twitter.com/user/status/123", Some("twitter")),
        ("https://notinstagram.com/p/abc", None),
        ("https://instagram.com.evil.example/", None),
    ] {
        assert_eq!(
            extract::social_site(&extract::http_url(url).unwrap()),
            expected
        );
    }
    assert!(extract::http_url("file:///etc/passwd").is_err());
    assert!(extract::http_url("https://user:password@example.com/a.jpg").is_err());
    assert!(
        extract::direct(&extract::http_url("https://example.com/PHOTO.JPG?size=large").unwrap())
            .is_some()
    );
}

#[test]
fn parses_backend_messages_and_preserves_media_headers() {
    for site in ["instagram", "facebook", "twitter"] {
        let items = extract::parse_messages(
            br#"[
          [2, {"category":"ignored"}],
          [3, "https://cdn.example/image.jpg?token=abc", {
            "filename":"image", "extension":"jpg",
            "_http_headers":{"Referer":"https://example.com/", "Cookie":"secret"}
          }]
        ]"#,
            site,
        )
        .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].category, site);
        assert_eq!(items[0].filename, "image.jpg");
        assert_eq!(items[0].headers["Referer"], "https://example.com/");
        assert!(!serde_json::to_string(&items).unwrap().contains("secret"));
    }
}

#[test]
fn rejects_incomplete_or_unsupported_extraction() {
    for input in [
        "[]",
        "{}",
        "[3]",
        "[[3]]",
        "[[6,\"https://x.com/child\",{}]]",
        "[[-1,{\"message\":\"login required\"}]]",
        "[[3,\"ytdl:file:///etc/passwd\",{}]]",
        "[[3,\"file:///etc/passwd\",{}]]",
        "[[3,\"ytdl:synthetic\",{\"_ytdl_manifest\":\"hls\"}]]",
        "[[3,\"https://example.com/video\",{\"_ytdl_manifest\":\"unknown\"}]]",
        "[[3,\"https://example.com/video\",{\"_ytdl_manifest\":false}]]",
        "[[3,\"https://example.com/video\",{\"_ytdl_manifest\":\"dash\",\"_ytdl_manifest_data\":{}}]]",
        "[[3,\"https://example.com/video\",{\"_ytdl_manifest_data\":\"inline\"}]]",
    ] {
        assert!(
            extract::parse_messages(input.as_bytes(), "twitter").is_err(),
            "{input}"
        );
    }
}

#[test]
fn preserves_video_handoffs_without_exposing_private_options() {
    let items = extract::parse_messages(
        br##"[
        [3,"ytdl:https://example.com/watch/1",{"_ytdl_params":{"password":"secret"}}],
        [3,"ytdl:https://example.com/synthetic",{
            "_ytdl_manifest":"hls", "_ytdl_manifest_data":"#EXTM3U\nsecret",
            "_ytdl_manifest_headers":{"Authorization":"secret"},
            "_ytdl_manifest_cookies":{"session":"secret"},
            "_ytdl_manifest_remux":"mp4",
            "_fallback":["https://example.com/fallback?token=secret"],
            "_http_headers":{"Referer":"secret"}
        }],
        [3,"https://example.com/stream.MPD?signature=abc",{}],
        [3,"https://example.com/inline",{"_ytdl_manifest":"dash","_ytdl_manifest_data":""}],
        [3,"https://example.com/null",{"_ytdl_manifest":null,"_ytdl_manifest_data":null}]
    ]"##,
        "twitter",
    )
    .unwrap();
    assert_eq!(items[0].url.as_str(), "ytdl:https://example.com/watch/1");
    assert!(
        matches!(&items[0].job, extract::MediaJob::VideoPage { page_url }
        if page_url.as_str() == "https://example.com/watch/1")
    );
    assert!(matches!(&items[1].job, extract::MediaJob::Manifest {
        protocol: extract::ManifestProtocol::Hls, base_url, inline_data: Some(data)
    } if base_url.as_str() == "https://example.com/synthetic" && data == "#EXTM3U\nsecret"));
    assert_eq!(items[1].handoff["_ytdl_manifest_remux"], "mp4");
    assert_eq!(
        items[1].handoff["_fallback"][0],
        "https://example.com/fallback?token=secret"
    );
    assert_eq!(items[1].headers["Referer"], "secret");
    assert!(matches!(
        &items[2].job,
        extract::MediaJob::Manifest {
            protocol: extract::ManifestProtocol::Dash,
            inline_data: None,
            ..
        }
    ));
    assert!(matches!(&items[3].job, extract::MediaJob::Manifest {
        inline_data: Some(data), ..
    } if data.is_empty()));
    assert!(matches!(items[4].job, extract::MediaJob::DirectHttp));
    let json = serde_json::to_string(&items).unwrap();
    assert!(!json.contains("secret"));
    assert!(!json.contains("_ytdl"));
    assert!(!json.contains("_fallback"));
    assert!(json.contains("\"kind\":\"manifest\""));
}

#[test]
fn job_identity_includes_manifest_and_request_context() {
    let items = extract::parse_messages(br#"[
        [3,"ytdl:https://example.com/synthetic",{"_ytdl_manifest":"hls","_ytdl_manifest_data":"first"}],
        [3,"ytdl:https://example.com/synthetic",{"_ytdl_manifest":"hls","_ytdl_manifest_data":"second"}],
        [3,"ytdl:https://example.com/synthetic",{"_ytdl_manifest":"hls","_ytdl_manifest_data":"first"}]
    ]"#, "twitter").unwrap();
    assert_ne!(items[0].identity(), items[1].identity());
    assert_eq!(items[0].identity(), items[2].identity());
    assert_ne!(
        download::destination(Path::new("root"), &items[0]),
        download::destination(Path::new("root"), &items[1])
    );
    let direct = media("https://example.com/a.jpg");
    assert_eq!(direct.identity(), direct.url.as_str());
    let mut changed = direct.clone();
    changed.headers.insert("Authorization".into(), "one".into());
    assert_ne!(direct.identity(), changed.identity());
    changed = direct.clone();
    changed.job = extract::MediaJob::VideoPage {
        page_url: direct.url.clone(),
    };
    assert_ne!(direct.identity(), changed.identity());
}

#[tokio::test]
async fn unsupported_jobs_fail_before_requests_or_filesystem_changes() {
    let root = tempfile::tempdir().unwrap();
    let client = download::client(Duration::from_secs(1)).unwrap();
    let items = extract::parse_messages(br##"[
        [3,"ytdl:http://127.0.0.1:1/page",{}],
        [3,"ytdl:http://127.0.0.1:1/inline",{"_ytdl_manifest":"hls","_ytdl_manifest_data":"#EXTM3U"}]
    ]"##, "twitter").unwrap();
    for item in items {
        let error = download::download(&client, root.path(), &item, 0)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("companion yt-dlp-rs adapter"));
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        let path = download::destination(root.path(), &item);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"existing").unwrap();
        assert!(
            download::download(&client, root.path(), &item, 0)
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(path).unwrap(), b"existing");
        std::fs::remove_dir_all(root.path().join("twitter")).unwrap();
    }
}

#[test]
fn paths_cannot_escape_destination_and_do_not_collide() {
    let mut first = media("https://example.com/a.jpg");
    first.category = "../../".into();
    first.filename = "../../etc/passwd".into();
    let second = media("https://example.com/other/a.jpg");
    let root = Path::new("downloads");
    let path = download::destination(root, &first);
    assert!(path.starts_with(root));
    assert_eq!(path.components().count(), 3);
    assert_ne!(
        download::destination(root, &media("https://example.com/a.jpg")),
        download::destination(root, &second)
    );
}

#[tokio::test]
async fn streams_file_forwards_headers_and_skips_existing() {
    let (url, handle) = server(vec![
        "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
    ]);
    let mut item = media(&url);
    item.headers
        .insert("Referer".into(), "https://x.com/".into());
    item.headers.insert("Host".into(), "wrong.example".into());
    let root = tempfile::tempdir().unwrap();
    let client = download::client(Duration::from_secs(5)).unwrap();
    let path = download::destination(root.path(), &item);
    assert_eq!(
        download::download(&client, root.path(), &item, 0)
            .await
            .unwrap(),
        Outcome::Downloaded(path.clone())
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"hello");
    assert_eq!(
        download::download(&client, root.path(), &item, 0)
            .await
            .unwrap(),
        Outcome::Skipped(path)
    );
    let requests = handle.join().unwrap();
    assert!(requests[0].contains("referer: https://x.com/"));
    assert!(!requests[0].contains("wrong.example"));
}

#[tokio::test]
async fn retries_server_failures() {
    let (url, handle) = server(vec![
        "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
    ]);
    let root = tempfile::tempdir().unwrap();
    let client = download::client(Duration::from_secs(5)).unwrap();
    download::download(&client, root.path(), &media(&url), 1)
        .await
        .unwrap();
    assert_eq!(handle.join().unwrap().len(), 2);
}

#[tokio::test]
async fn incomplete_downloads_leave_no_file() {
    let (url, handle) = server(vec![
        "HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\nshort",
    ]);
    let root = tempfile::tempdir().unwrap();
    let client = download::client(Duration::from_secs(5)).unwrap();
    let item = media(&url);
    assert!(
        download::download(&client, root.path(), &item, 0)
            .await
            .is_err()
    );
    assert!(!download::destination(root.path(), &item).exists());
    assert_eq!(
        std::fs::read_dir(root.path().join("twitter"))
            .unwrap()
            .count(),
        0
    );
    handle.join().unwrap();
}

#[tokio::test]
async fn follows_same_origin_redirects_with_headers_and_preserves_destination() {
    let (url, handle) = server(vec![
        "HTTP/1.1 302 Found\r\nLocation: /resolved.jpg\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        "HTTP/1.1 503 Service Unavailable\r\nRetry-After: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        "HTTP/1.1 308 Permanent Redirect\r\nLocation: /resolved.jpg\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
    ]);
    let root = tempfile::tempdir().unwrap();
    let client = download::client(Duration::from_secs(3)).unwrap();
    let mut item = media(&url);
    item.headers
        .insert("Authorization".into(), "Bearer local-secret".into());
    let path = download::destination(root.path(), &item);
    assert_eq!(
        download::download(&client, root.path(), &item, 1)
            .await
            .unwrap(),
        Outcome::Downloaded(path.clone())
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"ok");
    assert_eq!(
        download::download(&client, root.path(), &item, 0)
            .await
            .unwrap(),
        Outcome::Skipped(path)
    );
    for (index, request) in handle.join().unwrap().iter().enumerate() {
        let expected = if index % 2 == 0 {
            "/image.jpg"
        } else {
            "/resolved.jpg"
        };
        assert!(request.starts_with(&format!("GET {expected} HTTP/1.1\r\n")));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer local-secret\r\n")
        );
    }
}

#[tokio::test]
async fn redirected_body_failures_redact_signed_urls_and_clean_temporary_files() {
    let (url, handle) = server(vec![
        "HTTP/1.1 302 Found\r\nLocation: /resolved.jpg?token=private-session\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        "HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\npartial",
    ]);
    let root = tempfile::tempdir().unwrap();
    let client = download::client(Duration::from_secs(2)).unwrap();
    let item = media(&url);
    let error = download::download(&client, root.path(), &item, 0)
        .await
        .unwrap_err()
        .to_string();
    assert!(!error.contains("private-session"));
    assert!(!error.contains("resolved.jpg"));
    assert!(!download::destination(root.path(), &item).exists());
    assert_eq!(
        std::fs::read_dir(root.path().join("twitter"))
            .unwrap()
            .count(),
        0
    );
    assert_eq!(handle.join().unwrap().len(), 2);
}

#[tokio::test]
async fn redirect_loops_are_terminal_and_redirected_manifests_are_rejected() {
    for responses in [
        vec![
            "HTTP/1.1 302 Found\r\nLocation: /image.jpg#fragment\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        ],
        vec![
            "HTTP/1.1 302 Found\r\nLocation: /playlist.M3U8\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\n#EXTM3U",
        ],
    ] {
        let count = responses.len();
        let (url, handle) = server(responses);
        let root = tempfile::tempdir().unwrap();
        let client = download::client(Duration::from_secs(2)).unwrap();
        let item = media(&url);
        let error = download::download(&client, root.path(), &item, 2)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(if count == 1 { "loop" } else { "manifest" }),
            "{error}"
        );
        assert!(!download::destination(root.path(), &item).exists());
        assert_eq!(
            std::fs::read_dir(root.path().join("twitter"))
                .unwrap()
                .count(),
            0
        );
        assert_eq!(handle.join().unwrap().len(), count);
    }
}

#[tokio::test]
async fn redirect_chain_uses_one_download_attempt_deadline() {
    let (url, handle) = delayed_server(vec![
        (
            "HTTP/1.1 302 Found\r\nLocation: /resolved.jpg\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            Duration::from_millis(400),
        ),
        (
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
            Duration::from_millis(800),
        ),
    ]);
    let root = tempfile::tempdir().unwrap();
    let client = download::client(Duration::from_secs(1)).unwrap();
    let item = media(&url);
    let error = download::download(&client, root.path(), &item, 0)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("timed out"), "{error}");
    assert!(!download::destination(root.path(), &item).exists());
    assert_eq!(handle.join().unwrap().len(), 2);
}

#[tokio::test]
async fn refuses_redirects_instead_of_forwarding_secrets() {
    let (url, handle) = server(vec![
        "HTTP/1.1 302 Found\r\nLocation: https://other.example/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    ]);
    let root = tempfile::tempdir().unwrap();
    let client = download::client(Duration::from_secs(5)).unwrap();
    assert!(
        download::download(&client, root.path(), &media(&url), 0)
            .await
            .is_err()
    );
    handle.join().unwrap();
}

#[tokio::test]
async fn honors_retry_after_before_retrying_rate_limits_and_service_failures() {
    for status in ["429 Too Many Requests", "503 Service Unavailable"] {
        let response = if status.starts_with("429") {
            "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        } else {
            "HTTP/1.1 503 Service Unavailable\r\nRetry-After: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        };
        let (url, handle) = server(vec![
            response,
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
        ]);
        let root = tempfile::tempdir().unwrap();
        let client = download::client(Duration::from_secs(3)).unwrap();
        let start = std::time::Instant::now();
        let outcome = download::download(&client, root.path(), &media(&url), 1)
            .await
            .unwrap();
        assert!(start.elapsed() >= Duration::from_secs(1));
        let Outcome::Downloaded(path) = outcome else {
            panic!("expected download")
        };
        assert_eq!(std::fs::read(path).unwrap(), b"ok");
        assert_eq!(handle.join().unwrap().len(), 2);
    }
}

#[tokio::test]
async fn final_rate_limit_response_defers_other_downloads_on_cloned_client() {
    let (url, handle) = server(vec![
        "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
    ]);
    let root = tempfile::tempdir().unwrap();
    let client = download::client(Duration::from_secs(3)).unwrap();
    assert!(
        download::download(&client, root.path(), &media(&url), 0)
            .await
            .is_err()
    );
    let start = std::time::Instant::now();
    let next = media(&format!("{url}?second"));
    download::download(&client.clone(), root.path(), &next, 0)
        .await
        .unwrap();
    // The cooldown began just before the first call returned.
    assert!(start.elapsed() >= Duration::from_millis(900));
    assert_eq!(handle.join().unwrap().len(), 2);
}

#[tokio::test]
async fn excessive_retry_after_fails_without_early_retry_or_partial_files() {
    let (url, handle) = server(vec![
        "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 999999999999999999999999\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    ]);
    let root = tempfile::tempdir().unwrap();
    let client = download::client(Duration::from_secs(1)).unwrap();
    let start = std::time::Instant::now();
    let failure = download::download(&client, root.path(), &media(&url), 2)
        .await
        .unwrap_err();
    assert!(failure.to_string().contains("retry later"));
    assert!(start.elapsed() < Duration::from_secs(1));
    assert_eq!(
        std::fs::read_dir(root.path().join("twitter"))
            .unwrap()
            .count(),
        0
    );
    assert_eq!(handle.join().unwrap().len(), 1);
}

#[tokio::test]
async fn invalid_retry_after_uses_backoff_and_rate_limit_retries_are_bounded() {
    let response = "HTTP/1.1 429 Too Many Requests\r\nRetry-After: invalid\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    let (url, handle) = server(vec![response, response]);
    let root = tempfile::tempdir().unwrap();
    let client = download::client(Duration::from_secs(2)).unwrap();
    let start = std::time::Instant::now();
    let failure = download::download(&client, root.path(), &media(&url), 1)
        .await
        .unwrap_err();
    assert!(failure.to_string().contains("429"));
    assert!(start.elapsed() >= Duration::from_millis(250));
    assert_eq!(handle.join().unwrap().len(), 2);
}
