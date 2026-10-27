#![cfg(unix)]
use md5::{Digest, Md5};
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use tokio::time::timeout;

struct Server {
    origin: String,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<Vec<String>>>,
}
impl Server {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let done = stop.clone();
        let worker = thread::spawn(move || {
            let mut requests = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(12);
            while !done.load(Ordering::Acquire) && Instant::now() < deadline {
                let (mut socket, _) = match listener.accept() {
                    Ok(s) => s,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(e) => panic!("{e}"),
                };
                socket.set_nonblocking(false).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut request = Vec::new();
                let mut buf = [0; 4096];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = socket.read(&mut buf).unwrap();
                    assert!(n > 0 && request.len() < 65536);
                    request.extend_from_slice(&buf[..n]);
                }
                let target = String::from_utf8_lossy(&request)
                    .lines()
                    .next()
                    .unwrap()
                    .split_whitespace()
                    .nth(1)
                    .unwrap()
                    .to_owned();
                let body = format!("body:{target}");
                requests.push(target);
                write!(
                    socket,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
            requests
        });
        Self {
            origin,
            stop,
            worker: Some(worker),
        }
    }
    fn finish(mut self) -> Vec<String> {
        self.stop.store(true, Ordering::Release);
        self.worker.take().unwrap().join().unwrap()
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}
fn backend(root: &Path, origin: &str) -> PathBuf {
    let path = root.join("backend");
    let mut rows = Vec::new();
    for (post, files) in [(1, 2), (2, 0), (3, 2)] {
        let metadata = json!({"post_id":post,"count":files,"username":"fixture","date":"2024-01-02T03:04:06Z"});
        rows.push(json!([2, metadata]));
        for index in 1..=files {
            rows.push(json!([3,format!("{origin}/{post}-{index}.jpg"),{"post_id":post,"media_id":format!("{post}-{index}"),"num":index,"count":files,"username":"fixture","date":"2024-01-02T03:04:06Z","filename":format!("{post}-{index}"),"extension":"jpg"}]))
        }
    }
    fs::write(
        root.join("messages.json"),
        serde_json::to_vec(&rows).unwrap(),
    )
    .unwrap();
    fs::write(
        &path,
        "#!/bin/sh\ncat \"$(dirname \"$0\")/messages.json\"\n",
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}
fn trace(root: &Path, event: &str) -> Value {
    json!({"name":"metadata","mode":"custom","event":event,"content-format":format!("{event}:{{post_id}}\n"),"open":"a","base-directory":root,"filename":"trace.txt"})
}
async fn cli(root: &Path, backend: &Path, config: &Path, extra: &[&str]) -> std::process::Output {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_gallery-dl-rs"));
    command
        .args([
            "--jobs",
            "2",
            "--retries",
            "0",
            "--timeout",
            "2",
            "--proxy",
            "",
        ])
        .arg("--extractor-command")
        .arg(backend)
        .arg("--native-config")
        .arg(config)
        .arg("-d")
        .arg(root.join("downloads"))
        .args(extra)
        .arg("https://instagram.com/p/fixture/")
        .kill_on_drop(true);
    timeout(Duration::from_secs(5), command.output())
        .await
        .expect("CLI post hooks deadlocked with --jobs 2")
        .unwrap()
}
fn files(root: &Path) -> Vec<PathBuf> {
    let mut result = Vec::new();
    if root.is_dir() {
        for e in fs::read_dir(root).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                result.extend(files(&p));
            } else {
                result.push(p)
            }
        }
    }
    result
}

#[tokio::test]
async fn cli_concurrent_hash_names_sidecars_mtime_and_post_hooks_complete_in_order() {
    let temp = tempfile::tempdir().unwrap();
    let server = Server::start();
    let backend = backend(temp.path(), &server.origin);
    let mut processors = vec![
        json!({"name":"hash","hashes":"md5","filename":true}),
        json!({"name":"mtime"}),
        json!({"name":"metadata","include":["md5","post_id","media_id"],"mtime":true}),
        json!({"name":"metadata","mode":"custom","format":"{md5}\n","extension":"hash"}),
    ];
    for event in ["post", "post-after", "finalize"] {
        processors.push(trace(temp.path(), event));
    }
    let config = temp.path().join("config.json");
    fs::write(
        &config,
        serde_json::to_vec(&json!({"postprocessors":processors})).unwrap(),
    )
    .unwrap();
    let output = cli(
        temp.path(),
        &backend,
        &config,
        &["--filename", "{md5}.{extension}"],
    )
    .await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let requests = server.finish();
    assert_eq!(requests.len(), 4);
    let downloaded = files(&temp.path().join("downloads"));
    let images = downloaded
        .iter()
        .filter(|p| p.extension().is_some_and(|e| e == "jpg"))
        .collect::<Vec<_>>();
    assert_eq!(images.len(), 4, "{downloaded:?}");
    for image in images {
        let digest = image.file_stem().unwrap().to_str().unwrap();
        assert_eq!(digest.len(), 32);
        let actual = format!("{:x}", Md5::digest(fs::read(image).unwrap()));
        assert_eq!(digest, actual);
        let sidecar = PathBuf::from(format!("{}.json", image.display()));
        let metadata: Value = serde_json::from_slice(&fs::read(sidecar).unwrap()).unwrap();
        assert_eq!(metadata["md5"], digest);
        assert!(
            [json!(1), json!(3), json!("1"), json!("3")].contains(&metadata["post_id"]),
            "{metadata}"
        );
        assert_eq!(
            fs::read_to_string(format!("{}.hash", image.display())).unwrap(),
            format!("{digest}\n")
        );
        assert_eq!(
            fs::metadata(image)
                .unwrap()
                .modified()
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            1704164646
        );
    }
    let trace = fs::read_to_string(temp.path().join("trace.txt")).unwrap();
    let lines = trace.lines().collect::<Vec<_>>();
    assert_eq!(
        lines.iter().filter(|s| s.starts_with("post:")).count(),
        3,
        "{trace}"
    );
    assert_eq!(
        lines
            .iter()
            .filter(|s| s.starts_with("post-after:"))
            .count(),
        3,
        "{trace}"
    );
    assert_eq!(
        lines.iter().filter(|s| s.starts_with("finalize:")).count(),
        1,
        "{trace}"
    );
    for id in ["1", "2", "3"] {
        let before = lines
            .iter()
            .position(|s| *s == format!("post:{id}"))
            .unwrap();
        let after = lines
            .iter()
            .position(|s| *s == format!("post-after:{id}"))
            .unwrap();
        assert!(before < after, "{trace}");
    }
    assert!(lines.last().unwrap().starts_with("finalize:"));
}

#[tokio::test]
async fn cli_skip_hooks_run_for_existing_files_and_archive_hits_without_http() {
    for use_archive in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let server = Server::start();
        let backend = backend(temp.path(), &server.origin);
        let config = temp.path().join("config.json");
        fs::write(&config,serde_json::to_vec(&json!({"postprocessors":[trace(temp.path(),"skip"),{"name":"metadata","skip":true,"event":["file","skip"]}]})).unwrap()).unwrap();
        let archive = temp.path().join("archive.sqlite3");
        let archive_str = archive.to_str().unwrap();
        let extra = if use_archive {
            vec!["--download-archive", archive_str]
        } else {
            vec![]
        };
        let first = cli(temp.path(), &backend, &config, &extra).await;
        assert!(
            first.status.success(),
            "{}",
            String::from_utf8_lossy(&first.stderr)
        );
        let sidecars = files(&temp.path().join("downloads"))
            .into_iter()
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
            .collect::<Vec<_>>();
        assert_eq!(sidecars.len(), 4);
        for p in &sidecars {
            fs::write(p, b"sentinel").unwrap();
        }
        let second = cli(temp.path(), &backend, &config, &extra).await;
        assert!(
            second.status.success(),
            "{}",
            String::from_utf8_lossy(&second.stderr)
        );
        assert_eq!(server.finish().len(), 4);
        let trace = fs::read_to_string(temp.path().join("trace.txt")).unwrap();
        assert_eq!(trace.lines().count(), 4, "{trace}");
        for p in sidecars {
            assert_eq!(fs::read(p).unwrap(), b"sentinel");
        }
    }
}

#[tokio::test]
async fn cli_selected_empty_post_runs_post_and_finalize_hooks_without_a_download() {
    let temp = tempfile::tempdir().unwrap();
    let server = Server::start();
    let backend = backend(temp.path(), &server.origin);
    let config = temp.path().join("config.json");
    fs::write(&config,serde_json::to_vec(&json!({"postprocessors":[trace(temp.path(),"post"),trace(temp.path(),"post-after"),trace(temp.path(),"finalize")]})).unwrap()).unwrap();
    let output = cli(temp.path(), &backend, &config, &["--post-range", "2"]).await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(server.finish().is_empty());
    assert_eq!(
        fs::read_to_string(temp.path().join("trace.txt")).unwrap(),
        "post:2\npost-after:2\nfinalize:2\n"
    );
}

#[tokio::test]
async fn inspection_modes_never_execute_configured_lifecycle_or_file_processors() {
    for mode in ["--get-urls", "--dump-json"] {
        let temp = tempfile::tempdir().unwrap();
        let server = Server::start();
        let backend = backend(temp.path(), &server.origin);
        let config = temp.path().join("config.json");
        let mut processors = vec![json!({"name":"metadata"})];
        for event in ["init", "post", "post-after", "finalize"] {
            processors.push(trace(temp.path(), event));
        }
        fs::write(
            &config,
            serde_json::to_vec(&json!({"postprocessors":processors})).unwrap(),
        )
        .unwrap();
        let output = cli(temp.path(), &backend, &config, &[mode]).await;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!output.stdout.is_empty());
        assert!(server.finish().is_empty());
        assert!(!temp.path().join("trace.txt").exists());
        assert!(!temp.path().join("downloads").exists());
    }
}

#[tokio::test]
async fn terminate_after_existing_file_still_closes_post_and_finalizes_without_later_posts() {
    let temp = tempfile::tempdir().unwrap();
    let server = Server::start();
    let backend = backend(temp.path(), &server.origin);
    let config = temp.path().join("config.json");
    fs::write(&config,serde_json::to_vec(&json!({"postprocessors":[trace(temp.path(),"post"),trace(temp.path(),"post-after"),trace(temp.path(),"finalize")]})).unwrap()).unwrap();
    let initial = cli(temp.path(), &backend, &config, &[]).await;
    assert!(
        initial.status.success(),
        "{}",
        String::from_utf8_lossy(&initial.stderr)
    );
    fs::remove_file(temp.path().join("trace.txt")).unwrap();
    let output = cli(temp.path(), &backend, &config, &["--terminate", "1"]).await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(server.finish().len(), 4);
    assert_eq!(
        fs::read_to_string(temp.path().join("trace.txt")).unwrap(),
        "post:1\npost-after:1\nfinalize:1\n"
    );
}

#[tokio::test]
async fn abort_finalizes_current_input_and_continues_a_distinct_next_input() {
    let temp = tempfile::tempdir().unwrap();
    let server = Server::start();
    let backend = backend(temp.path(), &server.origin);
    let config = temp.path().join("config.json");
    fs::write(&config,serde_json::to_vec(&json!({"postprocessors":[trace(temp.path(),"post"),trace(temp.path(),"post-after"),trace(temp.path(),"finalize")]})).unwrap()).unwrap();
    let initial = cli(temp.path(), &backend, &config, &[]).await;
    assert!(
        initial.status.success(),
        "{}",
        String::from_utf8_lossy(&initial.stderr)
    );
    fs::remove_file(temp.path().join("trace.txt")).unwrap();
    let output = cli(
        temp.path(),
        &backend,
        &config,
        &["--abort", "1", "https://instagram.com/p/other-fixture/"],
    )
    .await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(server.finish().len(), 4);
    let trace = fs::read_to_string(temp.path().join("trace.txt")).unwrap();
    assert!(
        trace.starts_with("post:1\npost-after:1\nfinalize:1\n"),
        "{trace}"
    );
    assert_eq!(
        trace
            .lines()
            .filter(|line| line.starts_with("finalize:"))
            .count(),
        2,
        "{trace}"
    );
    assert!(trace.ends_with('\n'));
}

#[tokio::test]
async fn failed_http_runs_error_finalization_before_unconditional_finalization() {
    let temp = tempfile::tempdir().unwrap();
    let backend = backend(temp.path(), "http://127.0.0.1:1");
    fs::write(temp.path().join("messages.json"),serde_json::to_vec(&json!([[2,{"post_id":1,"count":1}],[3,"http://127.0.0.1:1/fail.jpg",{"post_id":1,"media_id":"1-1","num":1,"filename":"fail","extension":"jpg"}]])).unwrap()).unwrap();
    let config = temp.path().join("config.json");
    fs::write(&config,serde_json::to_vec(&json!({"postprocessors":[trace(temp.path(),"finalize-error"),trace(temp.path(),"finalize-success"),trace(temp.path(),"finalize")]})).unwrap()).unwrap();
    let output = cli(temp.path(), &backend, &config, &[]).await;
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("trace.txt")).unwrap(),
        "finalize-error:1\nfinalize:1\n"
    );
}

#[tokio::test]
async fn cli_classify_directory_and_exec_relocate_files_and_run_commands() {
    let temp = tempfile::tempdir().unwrap();
    let server = Server::start();
    let backend = backend(temp.path(), &server.origin);
    let config = temp.path().join("config.json");
    let log = temp.path().join("exec.log");
    fs::write(
        &config,
        serde_json::to_vec(&json!({
            "directory": ["{username}"],
            "filename": "{filename}.{extension}",
            "postprocessors": [
                {"name": "classify", "mapping": {"Images": ["jpg"]}},
                {"name": "exec", "command": format!("echo after {{_filename}} >> '{}'", log.display())},
                {"name": "exec", "event": "post", "command": ["sh", "-c", format!("echo post $0 >> '{}'", log.display()), "{post_id}"]},
            ]
        }))
        .unwrap(),
    )
    .unwrap();
    let output = cli(temp.path(), &backend, &config, &["--jobs", "1"]).await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    server.finish();
    let mut names: Vec<_> = files(&temp.path().join("downloads"))
        .into_iter()
        .map(|p| {
            p.strip_prefix(temp.path().join("downloads"))
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "fixture/Images/1-1.jpg",
            "fixture/Images/1-2.jpg",
            "fixture/Images/3-1.jpg",
            "fixture/Images/3-2.jpg"
        ]
    );
    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        "post 1\nafter 1-1.jpg\nafter 1-2.jpg\npost 2\npost 3\nafter 3-1.jpg\nafter 3-2.jpg\n"
    );
}

#[tokio::test]
async fn cli_zip_stores_job_files_in_directory_archive() {
    let temp = tempfile::tempdir().unwrap();
    let server = Server::start();
    let backend = backend(temp.path(), &server.origin);
    let config = temp.path().join("config.json");
    fs::write(
        &config,
        serde_json::to_vec(&json!({
            "directory": ["{username}"],
            "filename": "{filename}.{extension}",
        }))
        .unwrap(),
    )
    .unwrap();
    let output = cli(temp.path(), &backend, &config, &["--cbz"]).await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    server.finish();
    let downloads = temp.path().join("downloads");
    assert_eq!(files(&downloads), [downloads.join("fixture.cbz")]);
    let archive =
        zip::ZipArchive::new(fs::File::open(downloads.join("fixture.cbz")).unwrap()).unwrap();
    let mut names: Vec<_> = archive
        .file_names()
        .map(|n| n.unwrap().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, ["1-1.jpg", "1-2.jpg", "3-1.jpg", "3-2.jpg"]);
}

#[tokio::test]
async fn cli_rename_moves_existing_files_between_formats_without_downloading() {
    let temp = tempfile::tempdir().unwrap();
    let directory = temp.path().join("downloads/fixture");
    fs::create_dir_all(&directory).unwrap();
    for name in ["old-1-1.jpg", "old-1-2.jpg", "old-3-1.jpg", "old-3-2.jpg"] {
        fs::write(directory.join(name), b"kept").unwrap();
    }
    let config = temp.path().join("config.json");
    fs::write(
        &config,
        serde_json::to_vec(&json!({
            "directory": ["{username}"],
            "filename": "{filename}.{extension}",
        }))
        .unwrap(),
    )
    .unwrap();
    let server = Server::start();
    let script = backend(temp.path(), &server.origin);
    let output = cli(
        temp.path(),
        &script,
        &config,
        &["--rename", "old-{filename}.{extension}"],
    )
    .await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(server.finish().is_empty(), "renamed files were downloaded");
    let mut names: Vec<_> = fs::read_dir(&directory)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, ["1-1.jpg", "1-2.jpg", "3-1.jpg", "3-2.jpg"]);

    // --rename-to renames skipped files to the new format.
    let server = Server::start();
    let script = backend(temp.path(), &server.origin);
    let output = cli(
        temp.path(),
        &script,
        &config,
        &["--rename-to", "new-{filename}.{extension}"],
    )
    .await;
    assert!(output.status.success());
    assert!(server.finish().is_empty());
    let mut names: Vec<_> = fs::read_dir(&directory)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(
        names,
        ["new-1-1.jpg", "new-1-2.jpg", "new-3-1.jpg", "new-3-2.jpg"]
    );
}
