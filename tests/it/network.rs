//! Network configuration tests use only bounded loopback servers and synthetic data.
use gallery_dl_rs::{
    config::{Config, Options},
    download,
    extract::{self, PostMetadata},
    network::NetworkOptions,
};
use std::{
    io::{Read, Write},
    net::TcpListener,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

struct Server {
    url: String,
    requests: mpsc::Receiver<String>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Server {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let (send, requests) = mpsc::channel();
        let thread = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(8);
            while !stopped.load(Ordering::Relaxed) && Instant::now() < deadline {
                let (mut socket, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(_) => break,
                };
                socket.set_nonblocking(false).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                socket
                    .set_write_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut bytes = Vec::new();
                let mut buffer = [0; 1024];
                while bytes.len() < 16 * 1024 && !bytes.windows(4).any(|w| w == b"\r\n\r\n") {
                    match socket.read(&mut buffer) {
                        Ok(0) | Err(_) => break,
                        Ok(count) => bytes.extend_from_slice(&buffer[..count]),
                    }
                }
                let _ = send.send(String::from_utf8_lossy(&bytes).into_owned());
                let _ = socket.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nimage",
                );
            }
        });
        Self {
            url,
            requests,
            stop,
            thread: Some(thread),
        }
    }
    fn request(&self) -> String {
        self.requests.recv_timeout(Duration::from_secs(2)).unwrap()
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap();
    }
}
fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_gallery-dl-rs"))
}
fn direct(value: &str) -> extract::Media {
    extract::direct(&extract::http_url(value).unwrap()).unwrap()
}

#[tokio::test]
async fn explicit_proxy_routes_without_origin_dns_and_overrides_all_user_agent_casings() {
    let proxy = Server::new();
    let options =
        NetworkOptions::new(Some(proxy.url.clone()), Some("Synthetic Agent/1".into())).unwrap();
    let client = download::client(Duration::from_secs(2))
        .unwrap()
        .with_network(&options)
        .unwrap();
    let mut media = direct("http://origin.invalid/image.jpg");
    media
        .headers
        .insert("User-Agent".into(), "old-agent".into());
    media
        .headers
        .insert("uSeR-aGeNt".into(), "other-agent".into());
    media
        .headers
        .insert("Referer".into(), "https://referrer.invalid/".into());
    let root = tempfile::tempdir().unwrap();
    let outcome = download::download(&client, root.path(), &media, 0)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(outcome.saved_path().unwrap()).unwrap(),
        b"image"
    );
    let effective = client.effective_media(&media);
    let effective_path = download::destination(root.path(), effective.as_ref());
    assert_eq!(outcome.saved_path(), Some(effective_path.as_path()));
    assert_ne!(effective_path, download::destination(root.path(), &media));
    let request = proxy.request();
    assert!(
        request.starts_with("GET http://origin.invalid/image.jpg HTTP/1.1\r\n"),
        "{request}"
    );
    let headers = request.to_ascii_lowercase();
    assert_eq!(headers.matches("user-agent:").count(), 1);
    assert!(headers.contains("user-agent: synthetic agent/1\r\n"));
    assert!(headers.contains("referer: https://referrer.invalid/\r\n"));
    assert_eq!(media.headers["User-Agent"], "old-agent");
}

#[test]
fn proxy_basic_auth_is_private_from_inspection_and_metadata() {
    let proxy = Server::new();
    let proxy_url = proxy
        .url
        .replacen("http://", "http://synthetic-user:synthetic-password@", 1);
    let root = tempfile::tempdir().unwrap();
    let output = binary()
        .args([
            "--proxy",
            &proxy_url,
            "--write-metadata",
            "--retries",
            "0",
            "--timeout",
            "2",
            "-D",
        ])
        .arg(root.path())
        .arg("http://origin.invalid/image.jpg")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        proxy.request().to_ascii_lowercase().contains(
            "proxy-authorization: basic c3ludghldgljlxvzzxi6c3ludghldgljlxbhc3n3b3jk\r\n"
        )
    );
    let metadata = std::fs::read_dir(root.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|ext| ext == "json"))
        .unwrap();
    let metadata = std::fs::read_to_string(metadata).unwrap();
    assert!(!metadata.contains("synthetic-password"));
    assert!(!metadata.contains("proxy-authorization"));
    let inspection = binary()
        .args([
            "--proxy",
            &proxy_url,
            "--dump-media-json",
            "http://origin.invalid/image.jpg",
        ])
        .output()
        .unwrap();
    assert!(inspection.status.success());
    let inspection = String::from_utf8(inspection.stdout).unwrap();
    assert!(!inspection.contains("synthetic-user"));
    assert!(!inspection.contains("synthetic-password"));
    assert!(
        proxy.requests.try_recv().is_err(),
        "inspection must not make a media request"
    );
}

#[test]
fn empty_proxy_disables_all_environment_proxy_settings() {
    let origin = Server::new();
    let trap = Server::new();
    let root = tempfile::tempdir().unwrap();
    let output = binary()
        .env("ALL_PROXY", &trap.url)
        .env("all_proxy", &trap.url)
        .env("HTTP_PROXY", &trap.url)
        .env("http_proxy", &trap.url)
        .env("HTTPS_PROXY", &trap.url)
        .env("https_proxy", &trap.url)
        .env("NO_PROXY", "")
        .env("no_proxy", "")
        .args(["--proxy", "", "--retries", "0", "--timeout", "2", "-D"])
        .arg(root.path())
        .arg(format!("{}/image.jpg", origin.url))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(origin.request().starts_with("GET /image.jpg HTTP/1.1\r\n"));
    assert!(
        trap.requests.try_recv().is_err(),
        "disabled environment proxy received a request"
    );
}

#[test]
fn network_settings_obey_global_category_and_cli_precedence() {
    let config = Config::parse(br#"{"user-agent":"global","extractor":{"proxy":"http://default.invalid:8080","user-agent":"default","instagram":{"proxy":"","user-agent":"instagram"},"twitter":{"proxy":"http://twitter.invalid:8080"}}}"#, &std::env::current_dir().unwrap()).unwrap();
    let instagram = config.resolve("instagram").unwrap();
    assert_eq!(instagram.proxy.as_deref(), Some(""));
    assert_eq!(instagram.user_agent.as_deref(), Some("global"));
    assert_eq!(
        config.resolve("twitter").unwrap().proxy.as_deref(),
        Some("http://twitter.invalid:8080")
    );
    assert_eq!(
        config.resolve("direct").unwrap().proxy.as_deref(),
        Some("http://default.invalid:8080")
    );
    let cli = Options {
        proxy: Some("http://cli.invalid:8080".into()),
        user_agent: Some("cli".into()),
        ..Options::default()
    };
    let merged = instagram.overlay(&cli);
    assert_eq!(merged.proxy, cli.proxy);
    assert_eq!(merged.user_agent, cli.user_agent);
}

#[test]
fn effective_user_agent_changes_request_identity_but_preserves_social_archive_identity() {
    let first = NetworkOptions::new(None, Some("first".into())).unwrap();
    let second = NetworkOptions::new(None, Some("second".into())).unwrap();
    let client = download::client(Duration::from_secs(2))
        .unwrap()
        .with_network(&first)
        .unwrap();
    let media = direct("https://cdn.invalid/image.jpg");
    let first_media = client.effective_media(&media);
    let mut second_media = media.clone();
    second.apply_media(&mut second_media);
    assert_ne!(first_media.identity(), second_media.identity());
    assert_ne!(first_media.archive_key(), second_media.archive_key());
    assert_ne!(
        download::destination(std::path::Path::new("gallery-dl"), first_media.as_ref()),
        download::destination(std::path::Path::new("gallery-dl"), &second_media)
    );
    assert!(media.headers.is_empty());
    let mut social = media.clone();
    social.category = "instagram".into();
    social.metadata = Some(PostMetadata {
        source_num: None,
        booru_keywords: None,
        source_defaults: None,
        booru_user_id: None,
        booru_pool_name: None,
        booru_favorite_id: None,
        booru_artist_parent: None,
        booru_popular: None,
        source_suffix: None,
        search_tags: None,
        pool: None,
        favgroup: None,
        keyword_updates: None,
        tags: None,
        sketch: None,
        sketch_archive: false,
        user_bookmark: None,
        bookmark_archive: false,
        ajax: None,
        mypixiv: None,
        pixiv_novel_metadata: None,
        caption_html: None,
        pixiv_user_detail: None,
        comments: None,
        tags_bookmark: None,
        ugoira_original: None,
        ugoira_frame_index: None,
        pixivision: None,
        pixivision_archive: false,
        user_follow: None,
        followed_archive: false,
        series: None,
        num_series: None,
        search: None,
        search_archive: false,
        ranking: None,
        ranking_archive: false,
        profile_asset: None,
        file: None,
        frames: None,
        rating: None,
        score: None,
        revision: None,
        discord: None,
        creator: None,
        community: None,
        community_archive: false,
        retweet_id: None,
        quoted_by_id: None,
        reposted_by_id: None,
        article: None,
        instance: None,
        site_keywords: None,
        post_id: "64".into(),
        media_id: "65".into(),
        shortcode: "BA".into(),
        username: None,
        caption: None,
        timestamp: None,
        index: 1,
    });
    let original_archive = social.archive_key();
    first.apply_media(&mut social);
    assert_eq!(social.archive_key(), original_archive);
    let normalized = client.effective_media(&social);
    assert_eq!(
        normalized.identity(),
        social.identity(),
        "normalizing twice must be idempotent"
    );
}

#[test]
fn invalid_network_options_fail_preflight_without_private_values() {
    for (option, value, secret) in [
        (
            "--proxy",
            "ftp://private-user:private-pass@proxy.invalid",
            "private-pass",
        ),
        (
            "--proxy",
            "http://private-user:private-pass@proxy.invalid/path",
            "private-pass",
        ),
        (
            "--user-agent",
            "private-agent\r\nInjected: private-value",
            "private-value",
        ),
    ] {
        let output = binary()
            .args([option, value, "-g", "https://example.com/image.jpg"])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let diagnostic = String::from_utf8_lossy(&output.stderr);
        assert!(
            !diagnostic.contains(secret),
            "private value appeared in diagnostic: {diagnostic}"
        );
    }
}

#[cfg(unix)]
#[test]
fn compatibility_backend_receives_exact_network_and_browser_arguments_before_separator() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let args = root.path().join("arguments");
    let backend = root.path().join("backend");
    std::fs::write(&backend, "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$NETWORK_ARGUMENTS\"\nprintf '%s\\n' '[[3,\"https://cdn.invalid/image.jpg\",{\"_http_headers\":{\"User-Agent\":\"first\"}}],[3,\"https://cdn.invalid/image.jpg\",{\"_http_headers\":{\"uSeR-aGeNt\":\"second\"}}]]'\n").unwrap();
    std::fs::set_permissions(&backend, std::fs::Permissions::from_mode(0o700)).unwrap();
    let output = binary()
        .env("NETWORK_ARGUMENTS", &args)
        .arg("--extractor-command")
        .arg(&backend)
        .args([
            "--proxy",
            "http://user:pass@proxy.invalid:8080",
            "-a",
            "Agent with spaces",
            "--cookies-from-browser",
            "firefox:/synthetic profile",
            "-g",
            "https://x.com/person/status/1",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "https://cdn.invalid/image.jpg\n",
        "the configured agent must normalize headers before global deduplication"
    );
    let text = std::fs::read_to_string(args).unwrap();
    let args: Vec<_> = text.lines().collect();
    let separator = args.iter().position(|arg| *arg == "--").unwrap();
    for (flag, value) in [
        ("--proxy", "http://user:pass@proxy.invalid:8080"),
        ("--user-agent", "Agent with spaces"),
        ("--cookies-from-browser", "firefox:/synthetic profile"),
    ] {
        let index = args.iter().position(|arg| *arg == flag).unwrap();
        assert!(index + 1 < separator);
        assert_eq!(args[index + 1], value);
    }
    assert_eq!(args[separator + 1], "https://x.com/person/status/1");
}

#[test]
fn proxy_connection_failure_does_not_disclose_credentials() {
    let root = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let proxy = format!("http://synthetic-private-user:synthetic-private-password@{address}");
    let output = binary()
        .args(["--proxy", &proxy, "--retries", "0", "--timeout", "1", "-D"])
        .arg(root.path())
        .arg("http://origin.invalid/image.jpg")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    assert!(
        !diagnostic.contains("synthetic-private-user"),
        "{diagnostic}"
    );
    assert!(
        !diagnostic.contains("synthetic-private-password"),
        "{diagnostic}"
    );
}
