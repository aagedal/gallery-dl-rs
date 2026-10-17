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
        category: "twitter".into(),
        filename: "photo.jpg".into(),
        metadata: None,
        headers: BTreeMap::new(),
    }
}

/// Small deterministic HTTP server: no external services or account credentials.
fn server(responses: Vec<&'static str>) -> (String, thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let handle = thread::spawn(move || {
        let mut requests = Vec::new();
        for response in responses {
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
            socket.write_all(response.as_bytes()).unwrap();
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
        "[[3,\"ytdl:https://example.com/video\",{}]]",
        "[[3,\"file:///etc/passwd\",{}]]",
    ] {
        assert!(
            extract::parse_messages(input.as_bytes(), "twitter").is_err(),
            "{input}"
        );
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
