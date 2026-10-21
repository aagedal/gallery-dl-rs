#![cfg(unix)]

use futures_util::StreamExt;
use gallery_dl_rs::{
    booru::{Booru, Site},
    download::{self, DownloadOptions},
    extract::{self, Backend},
    postprocess, source_archive,
};
use serde_json::{Value, json};
use std::{os::unix::fs::PermissionsExt, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use url::Url;

fn messages(owner: Option<Value>) -> Value {
    let mut metadata = json!({"id":"00042","filename":"raw","extension":"jpg","md5":"0123456789abcdef0123456789abcdef","tags":"first  second"});
    if let Some(owner) = owner {
        metadata["favorite_id"] = owner;
    }
    json!([[3, "https://safebooru.org/images/1/raw.jpg", metadata]])
}

#[test]
fn compatibility_favorite_owner_is_exact_scoped_private_and_not_class_provenance() {
    for category in ["gelbooru", "safebooru", "danbooru", "e621", "twitter"] {
        for owner in [
            None,
            Some(Value::Null),
            Some(json!("")),
            Some(json!("007")),
            Some(json!(7)),
            Some(json!(false)),
            Some(json!([7])),
            Some(json!({"id":7})),
        ] {
            let media = extract::parse_messages(
                &serde_json::to_vec(&messages(owner.clone())).unwrap(),
                category,
            )
            .unwrap()
            .remove(0);
            let metadata = media.metadata.as_ref().unwrap();
            let expected = if matches!(category, "gelbooru" | "safebooru") {
                owner.as_ref()
            } else {
                None
            };
            assert_eq!(
                postprocess::context(&media).get("favorite_id"),
                expected,
                "{category}"
            );
            assert!(metadata.source_defaults.is_none());
            assert_ne!(
                source_archive::for_media(&media).map(|d| d.format),
                Some("f_{favorite_id}_{id}".into())
            );
            let public = serde_json::to_value(metadata).unwrap();
            assert!(public.get("booru_favorite_id").is_none());
            assert!(public.get("favorite_id").is_none());
            if category == "safebooru" && owner.is_some() {
                let context = postprocess::context(&media);
                assert_eq!(context["id"], "00042");
                assert_eq!(context["tags"], "first  second");
            }
        }
    }
    assert!(
        source_archive::for_url(
            &Url::parse("https://gelbooru.com/index.php?page=favorites&s=view&id=7").unwrap()
        )
        .is_none()
    );
}

#[tokio::test]
async fn backend_safe_favorite_class_retains_scalar_default_path_and_archive_format() {
    let root = tempfile::tempdir().unwrap();
    let executable = root.path().join("backend");
    std::fs::write(&executable, "#!/bin/sh\ncat \"$0.messages\"\n").unwrap();
    std::fs::write(
        executable.with_extension("messages"),
        serde_json::to_vec(&messages(Some(json!(7)))).unwrap(),
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let backend = Backend {
        executable,
        cookies: None,
        config: None,
        timeout: Duration::from_secs(3),
    };
    let source =
        Url::parse("https://safebooru.org/index.php?page=favorites&s=view&id=0007").unwrap();
    let mut stream = backend.stream(&source);
    let mut media = stream.next().await.unwrap().unwrap();
    assert!(stream.next().await.is_none());
    let descriptor = source_archive::for_media(&media).unwrap();
    assert_eq!(Some(descriptor.clone()), source_archive::for_url(&source));
    assert_eq!(descriptor.format, "f_{favorite_id}_{id}");
    assert_eq!(postprocess::context(&media)["favorite_id"], 7);
    (media.url, media.job) = extract::inline_text("fixture".into()).unwrap();
    let outcome = download::download_with_template(
        &download::client(Duration::from_secs(2)).unwrap(),
        root.path(),
        &media,
        DownloadOptions::default(),
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        outcome.saved_path().unwrap(),
        root.path()
            .join("safebooru/favorites/7/safebooru_00042_0123456789abcdef0123456789abcdef.jpg")
    );
}

#[tokio::test]
async fn favorite_html_python_whitespace_and_decimal_capture_drive_only_xml_member_requests() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let root = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    let worker = tokio::spawn(async move {
        let mut requests = Vec::new();
        for body in [
            "\u{1c}id=\"p٠٧tail \u{a0}id=\"p12tail xid=\"p99\" id=\"pⅫ\"",
            "<posts><post id=\"00042\" file_url=\"https://safebooru.org/images/1/raw.jpg\" md5=\"0123456789abcdef0123456789abcdef\" tags=\" first  second \" created_at=\"Tue Jan 02 03:04:05 +0000 2024\"/></posts>",
            "<posts/>",
        ] {
            let (mut socket, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                assert_eq!(socket.read(&mut byte).await.unwrap(), 1);
                request.push(byte[0]);
                assert!(request.len() < 16384);
            }
            requests.push(String::from_utf8(request).unwrap());
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        }
        requests
    });
    let native = Booru::new(Site::Safebooru, Duration::from_secs(2))
        .unwrap()
        .with_api_root(root)
        .with_api_credentials(Some("456"), Some("secret-key"))
        .unwrap();
    let source =
        Url::parse("https://safebooru.org/index.php?page=favorites&s=view&id=0007").unwrap();
    let mut stream = native.stream(&source);
    let media = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(postprocess::context(&media)["favorite_id"], 7);
    assert_eq!(postprocess::context(&media)["id"], "00042");
    assert_eq!(postprocess::context(&media)["tags"], "first  second");
    assert!(
        tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .unwrap()
            .is_none()
    );
    let requests = worker.await.unwrap();
    assert_eq!(requests.len(), 3);
    assert!(!requests[0].contains("api_key="));
    assert!(!requests[0].contains("user_id="));
    for (request, id) in requests.iter().zip(["0007", "٠٧", "12"]) {
        let target = request
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap();
        let url = Url::parse(&format!("http://fixture{target}")).unwrap();
        assert_eq!(
            url.query_pairs().find(|(key, _)| key == "id").unwrap().1,
            id
        );
    }
    for request in &requests[1..] {
        assert!(request.contains("api_key=secret-key"));
        assert!(request.contains("user_id=456"));
        assert!(!request.contains("limit="));
    }
    assert!(!requests[0].contains("secret-key"));
}
