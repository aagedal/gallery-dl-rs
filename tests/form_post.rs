//! Loopback coverage of form POST media jobs and their redirect boundaries.
use gallery_dl_rs::{
    download,
    extract::{self, Media, MediaJob},
};
use std::{
    io::{Read, Write},
    net::TcpListener,
    thread,
    time::Duration,
};

/// Serve `replies` in order, capturing each request with its body.
fn serve(replies: Vec<String>) -> (String, thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let handle = thread::spawn(move || {
        let mut requests = Vec::new();
        for reply in replies {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            let complete = |request: &[u8]| {
                let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
                    return false;
                };
                let head = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                let length = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                request.len() >= end + 4 + length
            };
            while !complete(&request) {
                match socket.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => request.extend_from_slice(&buffer[..count]),
                }
            }
            requests.push(String::from_utf8_lossy(&request).into_owned());
            let _ = socket.write_all(reply.as_bytes());
        }
        requests
    });
    (base, handle)
}

fn ok(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn redirect(status: &str, location: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
}

fn form_media(url: &str) -> Media {
    let mut media = extract::direct(&extract::http_url(url).unwrap()).unwrap();
    media.job = MediaJob::FormPost {
        form: vec![
            ("authenticity_token".into(), "secret token".into()),
            ("upload_id".into(), "abc123".into()),
        ],
    };
    media
}

fn client() -> download::DownloadClient {
    download::client(Duration::from_secs(2)).unwrap()
}

#[tokio::test]
async fn form_jobs_post_url_encoded_fields_and_save_the_response() {
    let (base, server) = serve(vec![ok("archive bytes")]);
    let media = form_media(&format!("{base}/uploads/abc123/downloads.zip"));
    let root = tempfile::tempdir().unwrap();
    let outcome = download::download(&client(), root.path(), &media, 0)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(outcome.saved_path().unwrap()).unwrap(),
        b"archive bytes"
    );
    let requests = server.join().unwrap();
    assert!(
        requests[0].starts_with("POST /uploads/abc123/downloads.zip "),
        "{}",
        requests[0]
    );
    let lower = requests[0].to_ascii_lowercase();
    assert!(lower.contains("content-type: application/x-www-form-urlencoded"));
    assert!(requests[0].ends_with("\r\n\r\nauthenticity_token=secret+token&upload_id=abc123"));
    // The private form never reaches public JSON.
    let public = serde_json::to_string(&media).unwrap();
    assert!(!public.contains("secret") && public.contains("form_post"));
}

#[tokio::test]
async fn see_other_redirects_continue_as_get_and_307_repeats_the_form_on_its_origin() {
    let (base, server) = serve(vec![
        redirect("303 See Other", "/files/one.zip"),
        ok("one"),
        redirect("307 Temporary Redirect", "/files/two.zip"),
        ok("two"),
    ]);
    let root = tempfile::tempdir().unwrap();
    let first = form_media(&format!("{base}/one.zip"));
    download::download(&client(), root.path(), &first, 0)
        .await
        .unwrap();
    let second = form_media(&format!("{base}/two.zip"));
    download::download(&client(), root.path(), &second, 0)
        .await
        .unwrap();
    let requests = server.join().unwrap();
    assert!(requests[0].starts_with("POST /one.zip "));
    assert!(
        requests[1].starts_with("GET /files/one.zip "),
        "{}",
        requests[1]
    );
    assert!(!requests[1].contains("secret"));
    assert!(requests[2].starts_with("POST /two.zip "));
    assert!(
        requests[3].starts_with("POST /files/two.zip "),
        "{}",
        requests[3]
    );
    assert!(requests[3].ends_with("authenticity_token=secret+token&upload_id=abc123"));
}

#[tokio::test]
async fn method_preserving_redirects_to_other_origins_and_oversized_forms_are_refused() {
    let other = TcpListener::bind("127.0.0.1:0").unwrap();
    other.set_nonblocking(true).unwrap();
    let elsewhere = format!("http://{}/stolen.zip", other.local_addr().unwrap());
    let (base, server) = serve(vec![redirect("308 Permanent Redirect", &elsewhere)]);
    let root = tempfile::tempdir().unwrap();
    let media = form_media(&format!("{base}/three.zip"));
    let failure = download::download(&client(), root.path(), &media, 0)
        .await
        .unwrap_err();
    assert!(failure.to_string().contains("another origin"), "{failure}");
    assert!(server.join().unwrap()[0].starts_with("POST /three.zip "));
    assert!(
        other.accept().is_err(),
        "the form must not leave its origin"
    );

    let mut large = form_media(&format!("{base}/four.zip"));
    large.job = MediaJob::FormPost {
        form: vec![("data".into(), "x".repeat(extract::MAX_FORM_BYTES + 1))],
    };
    let failure = download::download(&client(), root.path(), &large, 0)
        .await
        .unwrap_err();
    assert!(failure.to_string().contains("size limit"), "{failure}");
}
