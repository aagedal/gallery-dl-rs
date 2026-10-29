use futures_util::StreamExt;
use gallery_dl_rs::{event, mastodon::Mastodon, network::NetworkOptions};
use std::{
    io::{Read, Write},
    net::TcpListener,
    thread,
    time::{Duration, Instant},
};

#[tokio::test]
async fn an_exhausted_input_budget_cancels_a_later_native_page_request() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let root = format!("http://{}/", listener.local_addr().unwrap())
        .parse()
        .unwrap();
    let server = thread::spawn(move || {
        for page in 0..2 {
            let deadline = Instant::now() + Duration::from_secs(5);
            let (mut socket, _) = loop {
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "native page request missing");
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(e) => panic!("fixture accept failed: {e}"),
                }
            };
            socket.set_nonblocking(false).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(4)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = socket.read(&mut buffer).unwrap();
                assert!(n > 0 && request.len() < 64 * 1024);
                request.extend_from_slice(&buffer[..n]);
            }
            if page == 0 {
                let body = br#"[{"id":"100","account":{"acct":"alice"},"media_attachments":[{"id":"101","type":"image","url":"https://cdn.example/photo.jpg"}]}]"#;
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                socket.write_all(header.as_bytes()).unwrap();
                socket.write_all(body).unwrap();
            } else {
                assert!(String::from_utf8(request).unwrap().contains("&max_id=100 "));
                // The API's own request deadline is ten seconds. The shared
                // one-second input budget must drop the request first.
                let mut byte = [0];
                match socket.read(&mut byte) {
                    Ok(0) => {}
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::ConnectionReset
                                | std::io::ErrorKind::ConnectionAborted
                        ) => {}
                    other => panic!("timed-out extraction left its request alive: {other:?}"),
                }
            }
        }
    });
    let native = Mastodon::new(Duration::from_secs(10))
        .unwrap()
        .with_network(&NetworkOptions::new(Some(String::new()), None).unwrap())
        .unwrap()
        .with_api_root(root);
    let url = "https://mastodon.social/@id:9".parse().unwrap();
    let mut media = event::media_only(event::with_extraction_budget(
        native.events(&url),
        Duration::from_secs(1),
    ));
    assert_eq!(
        media
            .next()
            .await
            .unwrap()
            .unwrap()
            .metadata
            .unwrap()
            .post_id,
        "100"
    );
    let result = tokio::time::timeout(Duration::from_secs(4), media.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.unwrap_err().to_string(), "extraction timed out");
    assert!(media.next().await.is_none());
    tokio::task::spawn_blocking(move || server.join())
        .await
        .unwrap()
        .unwrap();
}
