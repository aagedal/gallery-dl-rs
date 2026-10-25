//! Configured and prefixed registry family instances through the command
//! line, against a local server.
mod common;
use std::{
    io::{Read, Write},
    net::TcpListener,
    process::Stdio,
    thread::{self, JoinHandle},
    time::Duration,
};

/// Serve `replies` in order; returns the request lines.
fn serve(replies: Vec<(&'static str, Vec<u8>)>) -> (String, JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let root = format!("http://{}", listener.local_addr().unwrap());
    let task = thread::spawn(move || {
        let mut requests = Vec::new();
        for (content_type, body) in replies {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 4096];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                let count = stream.read(&mut buffer).unwrap();
                assert_ne!(count, 0);
                request.extend_from_slice(&buffer[..count]);
            }
            let text = String::from_utf8_lossy(&request);
            requests.push(text.lines().next().unwrap_or_default().to_owned());
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&body);
        }
        requests
    });
    (root, task)
}

fn post_json(id: u64) -> Vec<u8> {
    format!(
        r#"[{{"id":{id},"md5":"abc","created_at":1500000000,"file_url":"/data/abc.jpg","author":"u","tags":"x y"}}]"#
    )
    .into_bytes()
}

fn gallery_dl() -> std::process::Command {
    let mut command = common::binary_command();
    command
        .args(["--native-config-ignore", "-o", "cache.file=null"])
        .args(["--extractor", "native"])
        .stdin(Stdio::null());
    command
}

#[test]
fn configured_instance_downloads_with_base_and_instance_settings() {
    let (root, server) = serve(vec![
        ("application/json", post_json(7)),
        ("image/jpeg", b"\xff\xd8jpeg".to_vec()),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let output = gallery_dl()
        .arg("-o")
        .arg(format!("extractor.moebooru.localbooru.root={root}"))
        // The base category scope applies to the instance...
        .args([
            "-o",
            "extractor.moebooru.filename={category}+{id}.{extension}",
        ])
        // ...and the instance scope overrides it.
        .args([
            "-o",
            "extractor.localbooru.directory=[\"{category}\",\"{subcategory}\"]",
        ])
        .arg("-d")
        .arg(dir.path())
        .arg(format!("{root}/post/show/7"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let file = dir
        .path()
        .join("localbooru")
        .join("post")
        .join("localbooru+7.jpg");
    assert_eq!(std::fs::read(&file).unwrap(), b"\xff\xd8jpeg");
    let requests = server.join().unwrap();
    assert_eq!(requests[0], "GET /post.json?tags=id:7 HTTP/1.1");
    assert_eq!(requests[1], "GET /data/abc.jpg HTTP/1.1");
}

#[test]
fn prefixed_input_selects_the_family_for_any_host() {
    let (root, server) = serve(vec![("application/json", post_json(8))]);
    let output = gallery_dl()
        .arg("-g")
        .arg(format!("moebooru:{root}/post/show/8"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(stdout.trim(), format!("{root}/data/abc.jpg"));
    let requests = server.join().unwrap();
    assert_eq!(requests[0], "GET /post.json?tags=id:8 HTTP/1.1");
}

#[test]
fn foolslide_works_only_through_configured_or_prefixed_instances() {
    let page = br#"<h1 class="tbtitle dnone"><a title="Manga"></a><a title="Chapter 1: One"></a></h1>
<script>var pages = [{"id":"5","chapter_id":"9","filename":"01.jpg","url":"https://cdn.fool.test/01.jpg","thumb_url":"t","width":"1","height":"1","size":"1"}];</script>"#;
    let (root, server) = serve(vec![
        ("text/html", page.to_vec()),
        ("text/html", page.to_vec()),
    ]);
    let configured = gallery_dl()
        .arg("-o")
        .arg(format!("extractor.foolslide.myslide.root={root}/reader"))
        .arg("-g")
        .arg(format!("{root}/reader/read/manga/en/0/1/"))
        .output()
        .unwrap();
    assert!(
        configured.status.success(),
        "{}",
        String::from_utf8_lossy(&configured.stderr)
    );
    let stdout = String::from_utf8(configured.stdout).unwrap();
    assert_eq!(stdout.trim(), "https://cdn.fool.test/01.jpg");
    let prefixed = gallery_dl()
        .arg("-g")
        .arg(format!("foolslide:{root}/read/manga/en/0/1/"))
        .output()
        .unwrap();
    assert!(
        prefixed.status.success(),
        "{}",
        String::from_utf8_lossy(&prefixed.stderr)
    );
    let stdout = String::from_utf8(prefixed.stdout).unwrap();
    assert_eq!(stdout.trim(), "https://cdn.fool.test/01.jpg");
    let requests = server.join().unwrap();
    assert_eq!(requests[0], "POST /reader/read/manga/en/0/1 HTTP/1.1");
    assert_eq!(requests[1], "POST /read/manga/en/0/1 HTTP/1.1");
}

#[test]
fn invalid_instance_settings_are_rejected() {
    let output = gallery_dl()
        .args(["-o", "extractor.moebooru.bad.root=ftp://x.test"])
        .arg("https://x.test/post/show/1")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("configured instance root must be an http(s) URL"),
        "{stderr}"
    );
}
