//! Actual Danbooru/e621/e926 artist child dispatch source replay.
use super::{
    tests::{booru, response},
    *,
};
use crate::{
    archive::Archive,
    config::Config,
    download::{self, DownloadOptions},
    filter::FilterExpression,
    job_selection::JobSelection,
    naming::{DirectoryTemplate, FilenameTemplate},
    postprocess::{
        Postprocessors, Specification,
        jobs::{self, JobConfiguration},
    },
    selection::RangeSelection,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

pub(super) const EVENTS: &[&str] = &[
    "post",
    "prepare",
    "file",
    "after",
    "post-after",
    "skip",
    "child",
    "child-after",
];
const KEYS: &[&str] = &[
    "id",
    "name",
    "marker",
    "category",
    "subcategory",
    "filename",
    "extension",
    "search_tags",
    "_parent",
    "num",
];
fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "../tests/fixtures/source_archive/booru_artist.json"
    ))
    .unwrap()
}
pub(super) fn serve(case: &Value) -> (Url, std::thread::JoinHandle<Vec<String>>) {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        time::Instant,
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let root = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    let replies = case["calls"]
        .as_array()
        .unwrap()
        .iter()
        .map(|call| {
            if call.get("transport_error").is_some() {
                None
            } else {
                assert_eq!(call["status"], 200);
                Some(response(
                    "200 OK",
                    "",
                    call["response"].to_string().as_bytes(),
                ))
            }
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
                            "missing artist request after {}",
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
            if let Some(reply) = reply {
                let _ = socket.write_all(&reply);
            }
        }
        requests
    });
    (root, task)
}
pub(super) fn assert_requests(case: &Value, root: &Url, requests: Vec<String>) {
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
pub(super) fn native_category(case: &Value) -> &str {
    // e926 is its own instance, as upstream.
    case["family"].as_str().unwrap()
}
pub(super) fn client(case: &Value, root: Url) -> Booru {
    booru(Site::from_category(native_category(case)).unwrap(), root).with_threshold(
        case["options"]["threshold"]
            .as_u64()
            .map(|v| usize::try_from(v).unwrap()),
    )
}

pub(super) fn templates(
    case: &Value,
    root: &Path,
) -> (Option<FilenameTemplate>, Option<DirectoryTemplate>) {
    let family = native_category(case);
    // Only naming options belong to this helper. Source's separate root
    // DownloadArchive path-array configuration is not a native naming option.
    let naming = case["options"]
        .as_object()
        .unwrap()
        .iter()
        .filter(|(key, _)| matches!(key.as_str(), "filename" | "directory"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<serde_json::Map<_, _>>();
    let cfg = Config::parse(
        &serde_json::to_vec(&json!({"extractor":{family:naming}})).unwrap(),
        root,
    )
    .unwrap();
    let options = cfg.resolve(family).unwrap();
    let filename = match &options.filename_conditions {
        Some(conditions) => Some(
            FilenameTemplate::parse_conditional(conditions, options.filename.as_deref()).unwrap(),
        ),
        None => options
            .filename
            .as_deref()
            .map(|v| FilenameTemplate::parse(v).unwrap()),
    };
    let directory = match &options.directory_conditions {
        Some(conditions) => Some(
            DirectoryTemplate::parse_conditional(conditions, options.directory.as_deref()).unwrap(),
        ),
        None => options
            .directory
            .as_deref()
            .map(|v| DirectoryTemplate::parse(v).unwrap()),
    };
    (filename, directory)
}
pub(super) fn artifacts(root: &Path) -> Value {
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
                        "metadata.sqlite"
                            | "root.sqlite"
                            | "root.sqlite-journal"
                            | "metadata.sqlite3"
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
pub(super) fn entries(path: &Path, query: &str) -> Vec<String> {
    let connection = rusqlite::Connection::open(path).unwrap();
    connection
        .prepare(query)
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap()
}
// Source's queue carries a Python extractor class and live path/extractor
// proxies; native public metadata retains the serializable artist values.
pub(super) fn public_artist(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(key, _)| !matches!(key.as_str(), "_extractor" | "_path" | "_extr"))
                .map(|(key, value)| (key.clone(), public_artist(value)))
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.iter().map(public_artist).collect()),
        value => value.clone(),
    }
}
pub(super) fn selection(case: &Value) -> JobSelection {
    JobSelection {
        child_filter: case["options"]["child-filter"]
            .as_str()
            .map(|v| FilterExpression::parse(v).unwrap()),
        child_range: case["options"]["child-range"]
            .as_str()
            .map(|v| RangeSelection::parse(v).unwrap()),
        ..Default::default()
    }
}
#[test]
fn artist_constructor_retains_source_lexical_id_and_query() {
    let fixture = fixture();
    let routes = fixture["route_cases"].as_array().unwrap();
    assert_eq!(routes.len(), 24);
    for case in routes {
        let url = Url::parse(case["url"].as_str().unwrap()).unwrap();
        let site = site_for_url(&url).unwrap_or(Site::E621);
        if case["extractor"].is_null() {
            assert!(target(site, &url).is_err(), "{url}");
            continue;
        }
        let actual = target(site, &url).unwrap().0;
        let captured = case["groups"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()
            .as_str()
            .unwrap();
        if case["subcategory"] == "artist" {
            assert_eq!(
                actual,
                Target::Artist {
                    id: captured.into()
                },
                "{url}"
            );
        } else {
            let Target::ArtistSearch { params } = actual else {
                panic!("{url}: wrong artist search target")
            };
            let mut seen = BTreeSet::new();
            let expected = captured
                .split('&')
                .filter(|pair| pair.contains('='))
                .flat_map(|pair| url::form_urlencoded::parse(pair.as_bytes()))
                .filter_map(|(k, v)| {
                    seen.insert(k.to_string())
                        .then(|| (k.into_owned(), v.into_owned()))
                })
                .collect::<Vec<_>>();
            assert_eq!(params, expected, "{url}: first decoded query keys");
        }
    }
}
#[tokio::test]
async fn artist_queues_keep_all_public_fields_and_select_before_child_requests() {
    let fixture = fixture();
    let cases = fixture["cases"].as_array().unwrap();
    assert!(cases.len() >= 129);
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let (root, server) = serve(case);
        let native = client(case, root.clone());
        let url = Url::parse(case["url"].as_str().unwrap()).unwrap();
        let queues = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = queues.clone();
        let input = native
            .events(&url)
            .inspect(move |event| {
                if let Ok(ExtractionEvent::BeginChild(scope)) = event {
                    let mut fields = scope.metadata.fields().clone();
                    fields.insert("category".into(), scope.metadata.category().into());
                    fields.insert("subcategory".into(), scope.metadata.subcategory().into());
                    captured
                        .lock()
                        .unwrap()
                        .push(public_artist(&Value::Object(fields)));
                }
            })
            .boxed();
        let mut input = selection(case).apply_events(input);
        let mut files = 0;
        let mut errors = Vec::new();
        if case["consume_limit"].as_u64() != Some(0) {
            while let Some(event) = input.next().await {
                match event {
                    Ok(ExtractionEvent::Media(_)) => {
                        files += 1;
                        if case["consume_limit"].as_u64() == Some(files) {
                            break;
                        }
                    }
                    Err(e) => errors.push(e.to_string()),
                    _ => (),
                }
            }
        }
        drop(input);
        let expected_queues = case["queue_events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|queue| {
                let mut fields = queue["metadata"].as_object().unwrap().clone();
                fields.insert("category".into(), case["family"].clone());
                fields.insert("subcategory".into(), case["subcategory"].clone());
                fields.insert("_url".into(), queue["url"].clone());
                public_artist(&Value::Object(fields))
            })
            .collect::<Vec<_>>();
        assert_eq!(
            *queues.lock().unwrap(),
            expected_queues,
            "{name}: full public candidate metadata before selection/dedup"
        );
        assert_eq!(
            files as usize,
            case["trace"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|event| event["event"] == "post")
                .count(),
            "{name}: source child media count"
        );
        assert_eq!(
            errors.len(),
            case["source_errors"].as_array().unwrap().len(),
            "{name}: {errors:?}"
        );
        for error in errors {
            assert!(
                !error.contains("127.0.0.1") && !error.contains("SyntheticArtistFailure"),
                "{name}: private transport response leaked"
            );
        }
        assert_requests(case, &root, server.join().unwrap());
    }
}
fn scoped_specifications(values: Option<&Vec<Value>>, tag: bool) -> Vec<Value> {
    values
        .into_iter()
        .flatten()
        .map(|value| {
            let mut value = value.clone();
            let scope = if tag {
                "subcategory == 'tag'"
            } else {
                "subcategory != 'tag'"
            };
            let filter = value.get("filter").and_then(Value::as_str).map_or_else(
                || scope.to_owned(),
                |filter| format!("{scope} and ({filter})"),
            );
            value["filter"] = filter.into();
            value
        })
        .collect()
}
#[tokio::test]
async fn artist_actual_jobs_keep_parent_ownership_paths_and_archive_rows() {
    let fixture = fixture();
    let cases = fixture["cases"].as_array().unwrap();
    assert!(cases.len() >= 129);
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let family = native_category(case);
        let (api_root, server) = serve(case);
        let native = client(case, api_root.clone());
        let url = Url::parse(case["url"].as_str().unwrap()).unwrap();
        let root = tempfile::tempdir().unwrap();
        // JobConfiguration is category scoped. Explicit subcategory filters
        // faithfully select the source root/tag hook executions; initialization
        // of distinct subclass configurations remains a separate API boundary.
        let mut specifications = scoped_specifications(case["hook_options"].as_array(), false);
        specifications.extend(scoped_specifications(
            case["child_hook_options"].as_array(),
            true,
        ));
        specifications.push(json!({"name":"metadata","mode":"jsonl","event":"file","filename":"files.jsonl","base-directory":root.path(),"directory":[],"archive":[":basedirectory","metadata.sqlite"],"include":["category","subcategory","id","search_tags","filename","extension"],"filter":"subcategory == 'tag'"}));
        for event in EVENTS {
            specifications.push(json!({"name":"metadata","mode":"jsonl","event":event,"base-directory":root.path(),"directory":[],"filename":format!("oracle-{event}.jsonl"),"include":KEYS}));
        }
        let processors = Arc::new(Postprocessors::new(
            specifications
                .iter()
                .map(|value| Specification::parse(value, root.path()).unwrap())
                .collect(),
        ));
        let (filename, directory) = templates(case, root.path());
        let configuration = JobConfiguration {
            processors: processors.clone(),
            root: root.path().to_owned(),
            filename: filename.clone(),
            directory: directory.clone(),
            init: case["options"]["init"] == true,
        };
        let mut remaining = case["consume_limit"].as_u64();
        let mut finish = remaining == Some(0);
        let mut after_last = false;
        let mut events = selection(case)
            .apply_events(native.events(&url))
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
            crate::source_archive::for_url(&url),
        );
        let download = download::client(Duration::from_secs(30)).unwrap();
        let archive = Archive::open(root.path().join("history.sqlite3"))
            .await
            .unwrap();
        let mut identities = BTreeSet::new();
        let expected_posts = case["trace"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["event"] == "post")
            .collect::<Vec<_>>();
        let mut post_index = 0;
        let mut errors = Vec::new();
        while let Some(result) = jobs.next().await {
            let item = match result {
                Ok(item) => item,
                Err(error) => {
                    errors.push(error.to_string());
                    continue;
                }
            };
            let post_id = &item.media.metadata.as_ref().unwrap().post_id;
            let source_id = &expected_posts[post_index]["metadata"]["id"];
            let canonical_id = match source_id {
                Value::String(value) => value.parse::<u64>().unwrap().to_string(),
                Value::Number(value) => value.as_u64().unwrap().to_string(),
                _ => panic!("{name}: unexpected source post identity"),
            };
            assert_eq!(
                post_id, &canonical_id,
                "{name}: native identity uses underlying post, never queued artist"
            );
            post_index += 1;
            let identity = format!(
                "{family}:v1:{:x}",
                Sha256::digest(json!([post_id]).to_string().as_bytes())
            );
            assert_eq!(
                item.media.archive_key().as_ref(),
                Some(&identity),
                "{name}: native archive is independent of artist name and parent mutations"
            );
            // Source publishes the same post under distinct artist/tag paths.
            // Record canonical history after each direct publication rather
            // than suppressing those paths with the native optional archive.
            let outcome = download::download_with_template(
                &download,
                root.path(),
                &item.media,
                DownloadOptions {
                    postprocessors: Some(&processors),
                    postprocessor_scope: item.scope.as_ref(),
                    directory: directory.as_ref(),
                    ..Default::default()
                },
                filename.as_ref(),
            )
            .await
            .unwrap_or_else(|error| panic!("{name}: {error}"));
            assert!(
                outcome.saved_path().unwrap().is_file(),
                "{name}: publication completed"
            );
            archive.record(&identity).await.unwrap();
            assert!(archive.contains(&identity).await.unwrap());
            identities.insert(identity);
        }
        assert_eq!(
            post_index,
            expected_posts.len(),
            "{name}: every source post reaches native job processing"
        );
        assert_eq!(
            errors.len(),
            case["source_errors"].as_array().unwrap().len(),
            "{name}: {errors:?}"
        );
        assert_eq!(
            artifacts(root.path()),
            case["published"],
            "{name}: complete source paths and bytes"
        );
        for event in EVENTS {
            let expected = case["trace"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|item| item["event"] == *event)
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
                for key in KEYS {
                    assert_eq!(
                        actual.get(*key).map(public_artist),
                        expected["metadata"].get(*key).map(public_artist),
                        "{name}: {event} ownership {key}"
                    );
                }
            }
        }
        let metadata = root.path().join("metadata.sqlite");
        let actual_entries = if metadata.exists() {
            entries(&metadata, "SELECT entry FROM archive ORDER BY entry")
        } else {
            Vec::new()
        };
        assert_eq!(
            json!(actual_entries),
            case["archive_sql"],
            "{name}: actual tag MetadataPP SQL entries"
        );
        assert_eq!(
            entries(
                &root.path().join("history.sqlite3"),
                "SELECT identity FROM completed_media ORDER BY identity"
            ),
            identities.into_iter().collect::<Vec<_>>(),
            "{name}: stable native canonical history"
        );
        assert_requests(case, &api_root, server.join().unwrap());
    }
}
