//! Real e621/e926 favorite extractor and DownloadJob source oracle replay.
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

const EVENTS: &[&str] = &["post", "prepare", "file", "after", "post-after"];
const KEYS: &[&str] = &["user_id", "id", "category", "filename", "extension"];
fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "../tests/fixtures/source_archive/booru_favorites.json"
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
        .map(|body| response("200 OK", "", body.to_string().as_bytes()))
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
                            "missing favorite request after {}",
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
    let source = Url::parse(case["url"].as_str().unwrap()).unwrap();
    booru(site_for_url(&source).unwrap(), root).with_threshold(
        case["options"]
            .get("threshold")
            .map(|v| usize::try_from(v.as_u64().unwrap()).unwrap()),
    )
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
fn favorite_constructor_queries_and_source_archive_descriptors_match_source() {
    let fixture = fixture();
    assert_eq!(fixture["route_cases"].as_array().unwrap().len(), 12);
    for case in fixture["route_cases"].as_array().unwrap() {
        let source = Url::parse(case["url"].as_str().unwrap()).unwrap();
        assert!(
            matches!(
                target(site_for_url(&source).unwrap(), &source).unwrap().0,
                Target::Favorites { .. }
            ),
            "{source}"
        );
        let descriptor = crate::source_archive::for_url(&source).unwrap();
        assert_eq!(descriptor.category, case["category"].as_str().unwrap());
        assert_eq!(descriptor.format, case["archive_format"].as_str().unwrap());
    }
    for case in fixture["cases"].as_array().unwrap() {
        let source = Url::parse(case["url"].as_str().unwrap()).unwrap();
        let Target::Favorites { params } =
            target(site_for_url(&source).unwrap(), &source).unwrap().0
        else {
            panic!("wrong favorite constructor");
        };
        if case["parsed_query_initial"].is_null() {
            // Zero-consumption source jobs never call metadata(), so this
            // oracle deliberately has no parsed-query snapshot to compare.
            assert_eq!(case["consume_limit"], 0);
            assert!(case["calls"].as_array().unwrap().is_empty());
            continue;
        }
        let expected = case["parsed_query_initial"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(params.len(), expected.len(), "{}", case["name"]);
        assert_eq!(
            params.into_iter().collect::<BTreeMap<_, _>>(),
            expected,
            "{}",
            case["name"]
        );
    }
}
#[tokio::test]
async fn favorites_stream_like_source_without_pool_buffering_or_later_cancelled_requests() {
    let fixture = fixture();
    let cases = fixture["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 56);
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let (root, server) = serve(case);
        let native = client(case, root.clone());
        let source = Url::parse(case["url"].as_str().unwrap()).unwrap();
        let mut stream = native.events(&source);
        let mut media = 0;
        let mut errors = Vec::new();
        if case["consume_limit"].as_u64() != Some(0) {
            while let Some(event) = stream.next().await {
                match event {
                    Ok(ExtractionEvent::Post(post)) => assert_eq!(
                        post.metadata.as_ref().unwrap().booru_user_id.as_ref(),
                        Some(&case["source_metadata"]["user_id"]),
                        "{name}: API owner is replaced with exact requested owner"
                    ),
                    Ok(ExtractionEvent::Media(_)) => {
                        media += 1;
                        if case["consume_limit"].as_u64() == Some(media) {
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
            media as usize,
            case["trace"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|v| v["event"] == "file")
                .count(),
            "{name}"
        );
        assert_eq!(
            errors.len(),
            usize::from(!case["error"].is_null()),
            "{name}: {errors:?}"
        );
        for error in errors {
            assert!(
                !error.contains("favorites.json") && !error.contains("API_OWNER"),
                "{name}: private error"
            );
        }
        assert_requests(case, &root, server.join().unwrap());
    }
}
#[tokio::test]
async fn favorite_job_owners_paths_and_sqlite_archive_keys_match_source_hooks() {
    let fixture = fixture();
    let cases = fixture["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 56);
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let (api_root, server) = serve(case);
        let native = client(case, api_root.clone());
        let source = Url::parse(case["url"].as_str().unwrap()).unwrap();
        // e926 is its own instance (and native archive category), as upstream.
        let category = site_for_url(&source).unwrap().category();
        let root = tempfile::tempdir().unwrap();
        let mut specifications = case["hook_options"].as_array().unwrap().clone();
        specifications.push(json!({"name":"metadata","mode":"custom","event":"file","format":"","empty":false,"filename":"unused","archive":root.path().join("metadata.sqlite3")}));
        for event in EVENTS {
            specifications.push(json!({"name":"metadata","mode":"jsonl","event":event,"base-directory":root.path(),"directory":[],"filename":format!("oracle-{event}.jsonl"),"include":KEYS}));
        }
        let processors = Arc::new(Postprocessors::new(
            specifications
                .iter()
                .map(|v| Specification::parse(v, root.path()).unwrap())
                .collect(),
        ));
        let config = JobConfiguration {
            processors: processors.clone(),
            root: root.path().to_owned(),
            filename: None,
            directory: None,
            init: false,
        };
        let control = case["consume_limit"].as_u64();
        let mut remaining = control;
        let mut finish = control == Some(0);
        let mut after_last_media = false;
        // Stop before polling another page, closing only the already-emitted
        // file's public EndPost frame so real job finalizers can run.
        let mut native_events = native
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
            let next = futures_util::Stream::poll_next(native_events.as_mut(), cx);
            if let std::task::Poll::Ready(Some(Ok(event))) = &next {
                if matches!(event, ExtractionEvent::Media(_)) {
                    if let Some(count) = remaining.as_mut() {
                        *count -= 1;
                        after_last_media = *count == 0;
                    }
                } else if after_last_media && matches!(event, ExtractionEvent::EndPost) {
                    finish = true;
                }
            }
            next
        })
        .boxed();
        let mut jobs = jobs::apply_with_descriptor(
            input,
            Arc::new(BTreeMap::from([(category.into(), config)])),
            Some(category.into()),
            crate::source_archive::for_url(&source),
        );
        let download = download::client(Duration::from_secs(30)).unwrap();
        let archive = Archive::open(root.path().join("history.sqlite3"))
            .await
            .unwrap();
        let mut errors = Vec::new();
        let mut identities = BTreeSet::new();
        let mut files = 0;
        let expected_posts = case["trace"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|v| v["event"] == "post")
            .collect::<Vec<_>>();
        while let Some(result) = jobs.next().await {
            let item = match result {
                Ok(item) => item,
                Err(error) => {
                    errors.push(error.to_string());
                    continue;
                }
            };
            let directory = expected_posts[files]["directory"]
                .as_str()
                .unwrap()
                .trim_end_matches('/');
            assert_eq!(
                item.scope.as_ref().unwrap().directory().unwrap(),
                root.path().join(directory),
                "{name}: owner directory freezes before Post hooks"
            );
            let identity = item.media.archive_key().unwrap();
            assert!(
                identity.starts_with(&format!("{category}:v1:")),
                "{name}: internal archive category is canonical"
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
            assert!(
                matches!(result, ArchiveOutcome::Completed(_)),
                "{name}: stable raw post identity must survive keyword edits"
            );
            assert!(archive.contains(&identity).await.unwrap());
            identities.insert(identity);
            files += 1;
        }
        assert_eq!(
            errors.len(),
            usize::from(!case["error"].is_null()),
            "{name}: {errors:?}"
        );
        assert_eq!(
            artifacts(root.path()),
            case["published"],
            "{name}: complete favorite paths and bytes"
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
                    .map(|v| serde_json::from_str::<Value>(v).unwrap())
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
                for key in KEYS {
                    assert_eq!(
                        actual.get(*key),
                        expected["metadata"].get(*key),
                        "{name}: {event} ownership {key}"
                    );
                }
            }
        }
        let expected_entries = case["trace"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|v| v["event"] == "file")
            .map(|v| {
                format!(
                    "{}_MD_{}",
                    case["category"].as_str().unwrap(),
                    v["archive_key"].as_str().unwrap()
                )
            })
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
            "{name}: real MetadataPP SQLite default favorite keys, deduplicated after keyword edits"
        );
        let expected_identities = case["input_pages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|v| v.as_array().into_iter().flatten())
            .take(files)
            .map(|post| {
                let id = post["id"].as_u64().unwrap().to_string();
                format!(
                    "{category}:v1:{:x}",
                    Sha256::digest(json!([id]).to_string().as_bytes())
                )
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(
            identities, expected_identities,
            "{name}: native history keys use original category/post ID"
        );
        assert_eq!(
            entries(
                &root.path().join("history.sqlite3"),
                "SELECT identity FROM completed_media ORDER BY identity"
            ),
            expected_identities.into_iter().collect::<Vec<_>>(),
            "{name}: committed native history"
        );
        assert_requests(case, &api_root, server.join().unwrap());
    }
}
