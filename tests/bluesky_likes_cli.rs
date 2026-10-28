use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    io::{Read, Write},
    net::TcpListener,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use tokio::process::Command;
const DID: &str = "did:plc:abcdefghijklmnopqrstuvwx";
const OTHER: &str = "did:plc:zzzzzzzzzzzzzzzzzzzzzzzz";
const CID1: &str = "bafkreiaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const CID2: &str = "bafkreibbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
fn post(did: &str, rkey: &str, cid: &str) -> Value {
    json!({"uri":format!("at://{did}/app.bsky.feed.post/{rkey}"),"cid":"bafyreipost",
        "author":{"did":did,"handle":"fixture.bsky.social"},"record":{"$type":"app.bsky.feed.post","text":"fixture","createdAt":"2024-01-02T03:04:05Z",
        "embed":{"$type":"app.bsky.embed.images","images":[{"image":{"$type":"blob","ref":{"$link":cid},"mimeType":"image/jpeg","size":1},"alt":"fixture"}]}}})
}
struct Server {
    proxy: String,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Server {
    fn new(bodies: Vec<Value>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let proxy = format!("http://{}/", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(vec![]));
        let captured = requests.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let worker = thread::spawn(move || {
            let mut bodies: VecDeque<_> = bodies.into();
            let deadline = Instant::now() + Duration::from_secs(8);
            loop {
                let mut socket = loop {
                    if stopping.load(Ordering::SeqCst) || Instant::now() > deadline {
                        return;
                    }
                    match listener.accept() {
                        Ok((socket, _)) => break socket,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2))
                        }
                        Err(error) => panic!("fixture accept: {error}"),
                    }
                };
                socket.set_nonblocking(false).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                socket
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = vec![];
                while !request.ends_with(b"\r\n\r\n") && request.len() < 32768 {
                    let mut byte = [0];
                    if socket.read(&mut byte).unwrap_or(0) == 0 {
                        return;
                    }
                    request.push(byte[0]);
                }
                let request = String::from_utf8(request).unwrap();
                let discovery = request.starts_with("CONNECT ");
                captured.lock().unwrap().push(request);
                // Deliberately fail TLS discovery at the proxy. No public
                // connection is opened, and native routing must use its cache
                // and the documented bsky.social fallback.
                if discovery {
                    socket.write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                    continue;
                }
                let Some(body) = bodies.pop_front() else {
                    socket.write_all(b"HTTP/1.1 500 Unexpected Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                    continue;
                };
                let body = serde_json::to_vec(&body).unwrap();
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                socket.write_all(header.as_bytes()).unwrap();
                socket.write_all(&body).unwrap();
            }
        });
        Self {
            proxy,
            requests,
            stop,
            worker: Some(worker),
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.worker.take().unwrap().join().unwrap();
    }
}

#[tokio::test]
async fn category_api_server_and_likes_endpoint_drive_independent_native_children() {
    let server = Server::new(vec![
        json!({"feed":[{"post":post(OTHER,"liked",CID1)}],"cursor":"later-likes"}),
        json!({"feed":[{"post":post(DID,"own",CID2)}],"cursor":"later-media"}),
    ]);
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("native.json");
    std::fs::write(&config,serde_json::to_vec(&json!({"extractor":{"bluesky":{"api-server":"http://appview.test/fixture/","proxy":server.proxy,"endpoint":"getActorLikes","include":["likes","media"],"timeout":2,"retries":0}}})).unwrap()).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_gallery-dl-rs"));
    command
        .current_dir(root.path())
        .kill_on_drop(true)
        .arg("--native-config")
        .arg(&config)
        .args([
            "--extractor",
            "native",
            "--post-range",
            "1",
            "--range",
            "1",
            "--dump-media-json",
        ])
        .arg(format!("https://bsky.app/profile/{DID}"));
    let result = tokio::time::timeout(Duration::from_secs(8), command.output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let rows: Vec<Value> = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["metadata"]["post_id"], "liked");
    assert_eq!(rows[1]["metadata"]["post_id"], "own");
    for (row, did) in rows.iter().zip([OTHER, DID]) {
        let url = url::Url::parse(row["url"].as_str().unwrap()).unwrap();
        assert_eq!(
            url.query_pairs().find(|(key, _)| key == "did").unwrap().1,
            did
        );
        assert_eq!(url.host_str(), Some("bsky.social"));
    }
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    assert!(
        requests[0]
            .starts_with("GET http://appview.test/fixture/xrpc/app.bsky.feed.getActorLikes?")
    );
    assert!(requests[1].starts_with("CONNECT plc.directory:443 "));
    assert!(
        requests[2]
            .starts_with("GET http://appview.test/fixture/xrpc/app.bsky.feed.getAuthorFeed?")
    );
    assert!(requests[3].starts_with("CONNECT plc.directory:443 "));
    assert!(
        requests
            .iter()
            .all(|request| !request.to_ascii_lowercase().contains("authorization:"))
    );
}
