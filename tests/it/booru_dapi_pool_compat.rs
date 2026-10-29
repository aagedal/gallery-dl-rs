#![cfg(unix)]

use futures_util::StreamExt;
use gallery_dl_rs::{
    booru::{Booru, Site},
    download::{self, DownloadOptions},
    extract::{self, Backend},
    postprocess, source_archive,
};
use serde_json::json;
use std::{os::unix::fs::PermissionsExt, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use url::Url;

#[tokio::test]
async fn compatibility_scalar_pool_and_exact_tags_keep_provenance_private() {
    for category in ["gelbooru", "safebooru"] {
        let root = tempfile::tempdir().unwrap();
        let tags = "first  second\tthird";
        let metadata = json!({"id":"00042","filename":"raw","extension":"jpg","md5":"0123456789abcdef0123456789abcdef","pool":29,"pool_name":"Pool & Name","tags":tags});
        let messages = json!([[3, "https://safebooru.org/images/1/raw.jpg", metadata]]);
        let bytes = serde_json::to_vec(&messages).unwrap();
        let standalone = extract::parse_messages(&bytes, category).unwrap().remove(0);
        assert!(
            standalone
                .metadata
                .as_ref()
                .unwrap()
                .source_defaults
                .is_none()
        );
        let context = postprocess::context(&standalone);
        assert_eq!(context["pool"], 29);
        assert_eq!(context["pool_name"], "Pool & Name");
        assert_eq!(context["tags"], tags);
        if category == "safebooru" {
            assert_eq!(context["id"], "00042");
        }
        let executable = root.path().join("backend");
        std::fs::write(&executable, "#!/bin/sh\ncat \"$0.messages\"\n").unwrap();
        std::fs::write(executable.with_extension("messages"), bytes).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let backend = Backend {
            executable,
            cookies: None,
            config: None,
            timeout: Duration::from_secs(3),
        };
        let source = Url::parse(&format!(
            "https://{}.{}{}",
            category,
            if category == "gelbooru" { "com" } else { "org" },
            "/index.php?page=pool&s=show&id=00029"
        ))
        .unwrap();
        let mut stream = backend.stream(&source);
        let mut media = stream.next().await.unwrap().unwrap();
        assert!(stream.next().await.is_none());
        let descriptor = source_archive::for_media(&media).unwrap();
        assert_eq!(Some(descriptor.clone()), source_archive::for_url(&source));
        assert_eq!(descriptor.format, "p_{pool}_{id}");
        let encoded = serde_json::to_string(media.metadata.as_ref().unwrap()).unwrap();
        for private in ["booru_keywords", "booru_pool_name", "source_defaults"] {
            assert!(!encoded.contains(private), "{private}");
        }
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
        let id = "00042";
        assert_eq!(
            outcome.saved_path().unwrap(),
            root.path().join(format!(
                "{category}/pool/29/{category}_{id}_0123456789abcdef0123456789abcdef.jpg"
            ))
        );
    }
}

async fn serve(
    responses: Vec<(u16, &'static str, &'static str)>,
) -> (Url, tokio::task::JoinHandle<Vec<String>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let root = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    let worker = tokio::spawn(async move {
        let mut requests = Vec::new();
        for (status, extra, body) in responses {
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
            socket.write_all(format!("HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n{body}",body.len()).as_bytes()).await.unwrap();
        }
        requests
    });
    (root, worker)
}

#[tokio::test]
async fn html_redirects_exclude_credentials_and_xml_redirects_keep_one_pair() {
    let (root,worker)=serve(vec![
        (302,"Location: /redirect-html\r\n",""),
        (200,"","<h4>Pool: fixture</h4><span class=\"thumb\" id=\"p00042\"/>"),
        (302,"Location: /redirect-xml?api_key=stale&user_id=stale\r\n",""),
        (200,"","<posts><post id=\"00042\" file_url=\"https://safebooru.org/images/1/raw.jpg\" md5=\"0123456789abcdef0123456789abcdef\" tags=\"  first  second  \" created_at=\"Tue Jan 02 03:04:05 +0000 2024\"/></posts>"),
    ]).await;
    let native = Booru::new(Site::Safebooru, Duration::from_secs(2))
        .unwrap()
        .with_api_root(root)
        .with_api_credentials(Some("456"), Some("secret-key"))
        .unwrap();
    let source = Url::parse("https://safebooru.org/index.php?page=pool&s=show&id=29").unwrap();
    let mut stream = native.stream(&source);
    let media = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(stream.next().await.is_none());
    let context = postprocess::context(&media);
    assert_eq!(context["id"], "00042");
    assert_eq!(context["tags"], "first  second");
    let requests = worker.await.unwrap();
    for request in &requests[..2] {
        assert!(!request.contains("api_key="));
        assert!(!request.contains("user_id="));
        assert!(!request.contains("secret-key"));
    }
    for request in &requests[2..] {
        assert_eq!(request.matches("api_key=").count(), 1);
        assert_eq!(request.matches("user_id=").count(), 1);
        assert!(request.contains("api_key=secret-key"));
        assert!(!request.contains("stale"));
    }
    assert!(
        !serde_json::to_string(&media)
            .unwrap()
            .contains("secret-key")
    );
}

#[tokio::test]
async fn html_cross_origin_redirect_fails_privately_before_any_api_request() {
    let (root, worker) = serve(vec![(
        302,
        "Location: https://example.invalid/PRIVATE_REDIRECT\r\n",
        "",
    )])
    .await;
    let native = Booru::new(Site::Safebooru, Duration::from_secs(2))
        .unwrap()
        .with_api_root(root)
        .with_api_credentials(Some("456"), Some("secret-key"))
        .unwrap();
    let source = Url::parse("https://safebooru.org/index.php?page=pool&s=show&id=29").unwrap();
    let error = native
        .stream(&source)
        .next()
        .await
        .unwrap()
        .unwrap_err()
        .to_string();
    assert!(!error.contains("secret-key"));
    assert!(!error.contains("PRIVATE_REDIRECT"));
    let requests = worker.await.unwrap();
    assert_eq!(requests.len(), 1);
    assert!(!requests[0].contains("api_key="));
}
