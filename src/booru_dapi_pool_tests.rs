//! Actual Gelbooru/Safebooru HTML pool and per-member API source replay.
use super::{
    tests::{booru, response},
    *,
};
use crate::{
    archive::{Archive, ArchiveOutcome},
    download::{self, DownloadOptions},
    postprocess::{
        Postprocessors, Specification,
        jobs::{self, JobConfiguration},
    },
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

const EVENTS: &[&str] = &["post", "prepare", "file", "after", "post-after", "skip"];
const KEYS: &[&str] = &[
    "pool",
    "pool_name",
    "num",
    "id",
    "category",
    "filename",
    "extension",
];
fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "../tests/fixtures/source_archive/booru_dapi_pool.json"
    ))
    .unwrap()
}
fn serve(case: &Value) -> (Url, std::thread::JoinHandle<Vec<String>>) {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        time::Instant,
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let root = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    let replies = case["responses"]
        .as_array()
        .unwrap()
        .iter()
        .take(case["calls"].as_array().unwrap().len())
        .map(|item| {
            let body = if item["kind"] == "json" {
                item["body"].to_string()
            } else {
                item["body"].as_str().unwrap().to_owned()
            };
            assert_eq!(item["status"], 200);
            response("200 OK", "", body.as_bytes())
        })
        .collect::<Vec<_>>();
    let task = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for reply in replies {
            let deadline = Instant::now() + Duration::from_secs(30);
            let (mut socket, _) = loop {
                match listener.accept() {
                    Ok(socket) => break socket,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "missing DAPI pool request after {}",
                            requests.len()
                        );
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(e) => panic!("{e}"),
                }
            };
            socket.set_nonblocking(false).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(30)))
                .unwrap();
            let mut request = Vec::new();
            while !request.windows(4).any(|v| v == b"\r\n\r\n") {
                let mut bytes = [0; 8192];
                let n = socket.read(&mut bytes).unwrap();
                assert!(n > 0);
                request.extend_from_slice(&bytes[..n]);
                assert!(request.len() <= 64 * 1024);
            }
            requests.push(String::from_utf8(request).unwrap());
            let _ = socket.write_all(&reply);
        }
        requests
    });
    (root, task)
}
fn assert_requests(case: &Value, root: &Url, requests: Vec<String>) {
    let name = case["name"].as_str().unwrap();
    let calls = case["calls"].as_array().unwrap();
    assert_eq!(requests.len(), calls.len(), "{name}: request count");
    for (request, call) in requests.iter().zip(calls) {
        let path = request
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap();
        let actual = root.join(path).unwrap();
        assert_eq!(
            actual.path(),
            Url::parse(call["url"].as_str().unwrap()).unwrap().path(),
            "{name}"
        );
        let pairs = actual
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect::<Vec<_>>();
        let wanted = call["wire_params"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    match v {
                        Value::String(v) => v.clone(),
                        _ => v.to_string(),
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        assert_eq!(pairs.len(), wanted.len(), "{name}: duplicate query keys");
        assert_eq!(
            pairs.into_iter().collect::<BTreeMap<_, _>>(),
            wanted,
            "{name}: exact first-decoded-key and numeric pagination params"
        );
    }
}
fn client(case: &Value, root: Url) -> Booru {
    booru(
        Site::from_category(case["family"].as_str().unwrap()).unwrap(),
        root,
    )
    .with_api_credentials(
        case["options"]["user-id"].as_str(),
        case["options"]["api-key"].as_str(),
    )
    .unwrap()
}

fn artifacts(root: &Path) -> Value {
    fn visit(root: &Path, directory: &Path, files: &mut Vec<Value>) {
        for entry in std::fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(root, &path, files);
            } else {
                let relative = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .replace(std::path::MAIN_SEPARATOR, "/");
                if EVENTS
                    .iter()
                    .any(|event| relative == format!("oracle-{event}.jsonl"))
                    || matches!(
                        relative.as_str(),
                        "metadata.sqlite3"
                            | "metadata.sqlite3-journal"
                            | "history.sqlite3"
                            | "history.sqlite3-journal"
                            | "history.sqlite3-wal"
                            | "history.sqlite3-shm"
                    )
                {
                    continue;
                }
                let bytes = std::fs::read(path).unwrap();
                files.push(json!({"path":relative,"data_hex":bytes.iter().map(|b|format!("{b:02x}")).collect::<String>()}));
            }
        }
    }
    let mut files = Vec::new();
    visit(root, root, &mut files);
    files.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
    json!(files)
}
fn entries(path: &Path, query: &str) -> Vec<String> {
    let connection = rusqlite::Connection::open(path).unwrap();
    connection
        .prepare(query)
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap()
}
#[test]
fn dapi_pool_constructor_identity_and_scalar_archive_descriptors_match_source() {
    let fixture = fixture();
    let routes = fixture["route_cases"].as_array().unwrap();
    assert_eq!(routes.len(), 16);
    for case in routes {
        let source = Url::parse(case["url"].as_str().unwrap()).unwrap();
        let site = match source.host_str() {
            Some("gelbooru.com") => Site::Gelbooru,
            Some("safebooru.org") => Site::Safebooru,
            _ => panic!("unexpected source pool host"),
        };
        if let Some(id) = case["captured_id"].as_str() {
            assert_eq!(
                target(site, &source).unwrap().0,
                Target::Pool { id: id.into() },
                "{source}"
            );
            let descriptor = crate::source_archive::for_url(&source).unwrap();
            assert_eq!(descriptor.category, site.category());
            assert_eq!(descriptor.format, case["archive_format"].as_str().unwrap());
        } else {
            assert!(target(site, &source).is_err(), "{source}");
        }
    }
}
#[tokio::test]
async fn dapi_pool_html_and_per_member_requests_keep_source_order_and_cancellation() {
    let fixture = fixture();
    let cases = fixture["cases"].as_array().unwrap();
    assert!(
        cases.len() >= 51,
        "the original complete source matrix must remain"
    );
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let (root, server) = serve(case);
        let native = client(case, root.clone());
        let source = Url::parse(case["url"].as_str().unwrap()).unwrap();
        let mut stream = native.events(&source);
        let mut files = 0;
        let mut errors = Vec::new();
        if case["consume_limit"].as_u64() != Some(0) {
            while let Some(event) = stream.next().await {
                match event {
                    Ok(ExtractionEvent::Post(post)) => {
                        let metadata = post.metadata.unwrap();
                        assert_eq!(
                            metadata.pool.as_deref(),
                            Some(&case["source_metadata"]["pool"]),
                            "{name}: raw scalar pool overrides API fields"
                        );
                        assert_eq!(
                            metadata.booru_pool_name.as_ref(),
                            Some(&case["source_metadata"]["pool_name"]),
                            "{name}: raw name override"
                        );
                        assert!(
                            metadata.source_num.is_none(),
                            "{name}: DAPI pools have no synthetic member ordinal"
                        );
                    }
                    Ok(ExtractionEvent::Media(media)) => {
                        let expected = case["extraction_events"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .filter(|event| event["kind"] == "media")
                            .nth(files as usize)
                            .unwrap();
                        assert_eq!(
                            media.url.as_str(),
                            expected["url"].as_str().unwrap(),
                            "{name}: actual primary media URL"
                        );
                        assert_eq!(
                            media.handoff.get("_fallback"),
                            expected["metadata"].get("_fallback"),
                            "{name}: exact source original-URL fallback"
                        );
                        files += 1;
                        if case["consume_limit"].as_u64() == Some(files) {
                            break;
                        }
                    }
                    Ok(_) => (),
                    Err(error) => errors.push(error.to_string()),
                }
            }
        }
        drop(stream);
        assert_eq!(
            files as usize,
            case["trace"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|v| v["event"] == "post")
                .count(),
            "{name}: duplicate and multi-result rows are retained"
        );
        assert_eq!(
            errors.len(),
            usize::from(!case["error"].is_null()),
            "{name}: {errors:?}"
        );
        for error in errors {
            for private in [
                "SYNTHETIC_KEY",
                "Missing authentication credentials",
                "Bad request",
                "127.0.0.1",
            ] {
                assert!(
                    !error.contains(private),
                    "{name}: error leaked response or credentials"
                );
            }
        }
        assert_requests(case, &root, server.join().unwrap());
    }
}
#[tokio::test]
async fn dapi_pool_job_scalar_ownership_paths_and_both_sqlite_archives_match_source() {
    let fixture = fixture();
    let cases = fixture["cases"].as_array().unwrap();
    assert!(
        cases.len() >= 51,
        "the original complete source matrix must remain"
    );
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let family = case["family"].as_str().unwrap();
        let (api_root, server) = serve(case);
        let native = client(case, api_root.clone());
        let source = Url::parse(case["url"].as_str().unwrap()).unwrap();
        let root = tempfile::tempdir().unwrap();
        let mut specifications = case["hook_options"].as_array().unwrap().clone();
        let mut keys = KEYS.to_vec();
        keys.push("tags");
        specifications.push(json!({"name":"metadata","mode":"custom","event":"file","format":"","empty":false,"filename":"unused","archive":root.path().join("metadata.sqlite3")}));
        for event in EVENTS {
            specifications.push(json!({"name":"metadata","mode":"jsonl","event":event,"base-directory":root.path(),"directory":[],"filename":format!("oracle-{event}.jsonl"),"include":keys}));
        }
        let processors = Arc::new(Postprocessors::new(
            specifications
                .iter()
                .map(|v| Specification::parse(v, root.path()).unwrap())
                .collect(),
        ));
        let configuration = JobConfiguration {
            processors: processors.clone(),
            root: root.path().to_owned(),
            filename: None,
            directory: None,
            init: false,
        };
        let mut remaining = case["consume_limit"].as_u64();
        let mut finish = remaining == Some(0);
        let mut after_last = false;
        let mut events = native
            .events(&source)
            .map(|event| {
                event.map(|mut event| {
                    if let ExtractionEvent::Media(media) = &mut event {
                        (media.url, media.job) = crate::extract::inline_text(
                            case["download_body"].as_str().unwrap().to_owned(),
                        )
                        .unwrap();
                    }
                    event
                })
            })
            .boxed();
        let input = futures_util::stream::poll_fn(|cx| {
            if finish {
                return std::task::Poll::Ready(None);
            }
            let next = futures_util::Stream::poll_next(events.as_mut(), cx);
            if let std::task::Poll::Ready(Some(Ok(event))) = &next {
                if matches!(event, ExtractionEvent::Media(_)) {
                    if let Some(count) = remaining.as_mut() {
                        *count -= 1;
                        after_last = *count == 0;
                    }
                } else if after_last && matches!(event, ExtractionEvent::EndPost) {
                    finish = true;
                }
            }
            next
        })
        .boxed();
        let mut jobs = jobs::apply_with_descriptor(
            input,
            Arc::new(BTreeMap::from([(family.into(), configuration)])),
            Some(family.into()),
            crate::source_archive::for_url(&source),
        );
        let download = download::client(Duration::from_secs(30)).unwrap();
        let archive = Archive::open(root.path().join("history.sqlite3"))
            .await
            .unwrap();
        let expected_posts = case["trace"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|v| v["event"] == "post")
            .collect::<Vec<_>>();
        let mut errors = Vec::new();
        let mut identities = BTreeSet::new();
        let mut index = 0;
        while let Some(result) = jobs.next().await {
            let item = match result {
                Ok(item) => item,
                Err(e) => {
                    errors.push(e.to_string());
                    continue;
                }
            };
            assert_eq!(
                item.scope.as_ref().unwrap().directory().unwrap(),
                root.path().join(
                    expected_posts[index]["directory"]
                        .as_str()
                        .unwrap()
                        .trim_end_matches('/')
                ),
                "{name}: pre-Post scalar pool directory"
            );
            let post_id = &item.media.metadata.as_ref().unwrap().post_id;
            let source_id = &expected_posts[index]["metadata"]["id"];
            let canonical_id = match source_id {
                Value::String(value) => value.parse::<u64>().unwrap().to_string(),
                Value::Number(value) => value.as_u64().unwrap().to_string(),
                _ => panic!("unexpected source post identity"),
            };
            assert_eq!(
                post_id, &canonical_id,
                "{name}: stable identity is the canonical source post ID"
            );
            let identity = format!(
                "{family}:v1:{:x}",
                Sha256::digest(json!([post_id]).to_string().as_bytes())
            );
            assert_eq!(
                item.media.archive_key().as_ref(),
                Some(&identity),
                "{name}: history identity uses canonical category/post ID"
            );
            let result = archive
                .download_with_template(
                    &download,
                    root.path(),
                    &item.media,
                    DownloadOptions {
                        postprocessors: Some(&processors),
                        postprocessor_scope: item.scope.as_ref(),
                        ..Default::default()
                    },
                    None,
                )
                .await
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            match result {
                ArchiveOutcome::Completed(outcome) => {
                    assert!(outcome.saved_path().unwrap().is_file())
                }
                ArchiveOutcome::AlreadyArchived => assert!(
                    identities.contains(&identity),
                    "{name}: only a previously published post may be skipped"
                ),
            };
            assert!(archive.contains(&identity).await.unwrap());
            identities.insert(identity);
            index += 1;
        }
        assert_eq!(
            errors.len(),
            usize::from(!case["error"].is_null()),
            "{name}: {errors:?}"
        );
        assert_eq!(
            artifacts(root.path()),
            case["published"],
            "{name}: full scalar-pool default paths and bytes"
        );
        for event in EVENTS {
            let expected = case["trace"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|v| v["event"] == *event)
                .collect::<Vec<_>>();
            let path = root.path().join(format!("oracle-{event}.jsonl"));
            let actual = if path.exists() {
                std::fs::read_to_string(path)
                    .unwrap()
                    .lines()
                    .map(|line| serde_json::from_str::<Value>(line).unwrap())
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            assert_eq!(
                actual.len(),
                expected.len(),
                "{name}: {event} callback count"
            );
            for (actual, expected) in actual.iter().zip(expected) {
                for key in &keys {
                    assert_eq!(
                        actual.get(*key),
                        expected["metadata"].get(*key),
                        "{name}: {event} scalar ownership {key}"
                    );
                }
            }
        }
        let expected_entries = case["trace"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|v| v["event"] == "file")
            .map(|v| format!("{family}_MD_{}", v["archive_key"].as_str().unwrap()))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let metadata_path = root.path().join("metadata.sqlite3");
        let actual_entries = if metadata_path.exists() {
            entries(&metadata_path, "SELECT entry FROM archive ORDER BY entry")
        } else {
            Vec::new()
        };
        assert_eq!(
            actual_entries, expected_entries,
            "{name}: actual default scalar-pool metadata archive"
        );
        assert_eq!(
            entries(
                &root.path().join("history.sqlite3"),
                "SELECT identity FROM completed_media ORDER BY identity"
            ),
            identities.into_iter().collect::<Vec<_>>(),
            "{name}: committed stable history identities"
        );
        assert_requests(case, &api_root, server.join().unwrap());
    }
}
