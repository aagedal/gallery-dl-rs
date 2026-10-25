//! User-configured Danbooru/e621 instances and `E621:URL` prefixes through
//! the binary, against a local instance.
mod common;
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};
use tokio::process::Command;

const MD5: &str = "0123456789abcdef0123456789abcdef";

/// A local instance answering each request path from `routes` until
/// `expected` requests were served.
struct Server {
    root: String,
    requests: Arc<Mutex<Vec<String>>>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Server {
    fn new(expected: usize, routes: impl Fn(&str, &str) -> Vec<u8> + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let root = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(vec![]));
        let captured = requests.clone();
        let origin = root.clone();
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(20);
            for _ in 0..expected {
                let mut socket = loop {
                    if Instant::now() > deadline {
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
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = vec![];
                while !request.ends_with(b"\r\n\r\n") && request.len() < 32768 {
                    let mut byte = [0];
                    if socket.read(&mut byte).unwrap_or(0) == 0 {
                        break;
                    }
                    request.push(byte[0]);
                }
                let request = String::from_utf8(request).unwrap();
                let path = request.split(' ').nth(1).unwrap_or_default().to_owned();
                captured.lock().unwrap().push(request);
                let body = routes(&origin, &path);
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = socket.write_all(header.as_bytes());
                let _ = socket.write_all(&body);
            }
        });
        Self {
            root,
            requests,
            worker: Some(worker),
        }
    }

    fn requests(mut self) -> Vec<String> {
        self.worker.take().unwrap().join().unwrap();
        self.requests.lock().unwrap().clone()
    }
}

async fn run(config: &Value, directory: &std::path::Path, args: &[&str]) -> std::process::Output {
    let path = directory.join("native.json");
    std::fs::write(&path, serde_json::to_vec(config).unwrap()).unwrap();
    let mut command = Command::from(common::binary_command());
    command
        .current_dir(directory)
        .kill_on_drop(true)
        .arg("--native-config")
        .arg(&path)
        .args(["--extractor", "native"])
        .args(args);
    tokio::time::timeout(Duration::from_secs(30), command.output())
        .await
        .unwrap()
        .unwrap()
}

fn basic(credentials: &str) -> String {
    use base64::Engine;
    format!(
        "authorization: basic {}",
        base64::engine::general_purpose::STANDARD
            .encode(credentials)
            .to_ascii_lowercase()
    )
}

#[tokio::test]
async fn configured_danbooru_instance_downloads_under_its_own_category() {
    let server = Server::new(2, |origin, path| {
        if path.starts_with("/posts/7.json") {
            json!({"id": 7, "md5": MD5, "file_ext": "png",
                   "file_url": format!("{origin}/data/{MD5}.png"),
                   "tag_string": "cat", "uploader_name": "alice"})
            .to_string()
            .into_bytes()
        } else {
            b"PNGDATA".to_vec()
        }
    });
    let directory = tempfile::tempdir().unwrap();
    let base = directory.path().join("out");
    let config = json!({"extractor": {
        "base-directory": base,
        "retries": 0,
        "timeout": 10,
        "Danbooru": {"username": "base", "password": "base-key",
                     "localbooru": {"root": server.root}},
        "localbooru": {"username": "local", "password": "local-key"}}});
    let output = run(
        &config,
        directory.path(),
        &[&format!("{}/posts/7", server.root)],
    )
    .await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let file = base
        .join("localbooru")
        .join(format!("localbooru_7_{MD5}.png"));
    assert_eq!(std::fs::read(&file).unwrap(), b"PNGDATA");
    let requests = server.requests();
    assert_eq!(requests.len(), 2, "{requests:?}");
    assert!(
        requests[0].starts_with("GET /posts/7.json "),
        "{}",
        requests[0]
    );
    // The instance's own credentials reach only its API requests.
    assert!(
        requests[0]
            .to_ascii_lowercase()
            .contains(&basic("local:local-key"))
    );
    assert!(requests[1].starts_with(&format!("GET /data/{MD5}.png ")));
    assert!(!requests[1].to_ascii_lowercase().contains("authorization:"));
}

#[tokio::test]
async fn prefixed_e621_input_uses_the_basecategory_scope_without_its_credentials() {
    let server = Server::new(1, |origin, _| {
        json!({"post": {"id": 8, "file": {"url": format!("{origin}/data/{MD5}.png"),
                         "ext": "png", "md5": MD5},
                        "tags": {"general": ["cat"]}}})
        .to_string()
        .into_bytes()
    });
    let directory = tempfile::tempdir().unwrap();
    let config = json!({"extractor": {
        "retries": 0,
        "timeout": 10,
        "E621": {"username": "base", "password": "PRIVATE", "user-agent": "base-agent/1"},
        "e621": {"username": "e621-only", "password": "PRIVATE"}}});
    let output = run(
        &config,
        directory.path(),
        &["-j", &format!("E621:{}/posts/8", server.root)],
    )
    .await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rows: Vec<Value> = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    let netloc = server.root.strip_prefix("http://").unwrap().to_owned();
    assert_eq!(rows[0]["category"], netloc.as_str());
    assert_eq!(rows[0]["url"], format!("{}/data/{MD5}.png", server.root));
    let requests = server.requests();
    assert!(
        requests[0].starts_with("GET /posts/8.json "),
        "{}",
        requests[0]
    );
    // The basecategory scope applies, but its credentials stay with
    // configured instances: an arbitrary prefixed host is anonymous.
    assert!(
        requests[0]
            .to_ascii_lowercase()
            .contains("user-agent: base-agent/1\r\n"),
        "{}",
        requests[0]
    );
    assert!(!requests[0].to_ascii_lowercase().contains("authorization:"));
    assert!(!requests[0].contains("e621-only"));
}
