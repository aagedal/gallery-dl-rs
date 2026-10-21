//! Actual source pagination replay over native local HTTP requests.
use super::tests::{booru, danbooru_post, e621_post, response};
use super::*;
use serde_json::json;

// Bound missing-request failures too: an implementation which prematurely
// truncates traversal must fail instead of hanging while joining its server.
fn serve(replies: Vec<Vec<u8>>) -> (Url, std::thread::JoinHandle<Vec<String>>) {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        time::Instant,
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let root = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    let task = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for reply in replies {
            let deadline = Instant::now() + Duration::from_secs(5);
            let (mut socket, _) = loop {
                match listener.accept() {
                    Ok(socket) => break socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "missing pagination request after {} requests",
                            requests.len()
                        );
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            socket.set_nonblocking(false).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let mut buffer = [0; 8192];
                let count = socket.read(&mut buffer).unwrap();
                assert_ne!(count, 0);
                request.extend_from_slice(&buffer[..count]);
                assert!(request.len() <= 64 * 1024);
            }
            requests.push(String::from_utf8(request).unwrap());
            let _ = socket.write_all(&reply);
        }
        requests
    });
    (root, task)
}

fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "../tests/fixtures/source_archive/booru_pagination.json"
    ))
    .unwrap()
}
fn body(site: Site, ids: &Value) -> Value {
    let posts: Vec<_> = ids
        .as_array()
        .unwrap()
        .iter()
        .map(|id| {
            let id = id.as_u64().unwrap();
            if site == Site::Danbooru {
                danbooru_post(id)
            } else {
                e621_post(id)
            }
        })
        .collect();
    if site == Site::Danbooru {
        Value::Array(posts)
    } else {
        json!({"posts":posts})
    }
}
fn scalar(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        Value::Number(number) => number.to_string(),
        _ => panic!("unexpected fixture query scalar"),
    }
}

#[tokio::test]
async fn native_tag_traversal_matches_actual_source_requests_order_and_cancellation() {
    let fixture = fixture();
    let cases = fixture["cases"].as_array().unwrap();
    let mut beyond_one_hundred = 0;
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let site = Site::from_category(case["family"].as_str().unwrap()).unwrap();
        let expected_calls = case["calls"].as_array().unwrap();
        // Only requests observed in the source trace get a response. In
        // particular canceled cases do not leave a fixture server waiting for
        // deliberately unconsumed input pages (and zero-limit cases get none).
        let replies: Vec<_> = case["input_pages"]
            .as_array()
            .unwrap()
            .iter()
            .take(expected_calls.len())
            .map(|ids| response("200 OK", "", body(site, ids).to_string().as_bytes()))
            .collect();
        let (root, server) = serve(replies);
        // Raw config parsing is independently covered by the config suite;
        // this engine test consumes the real source _init normalization.
        let configured = (case["threshold_set"] == true)
            .then(|| usize::try_from(case["threshold"].as_u64().unwrap()).unwrap());
        let client = booru(site, root.clone()).with_threshold(configured);
        let source = Url::parse(case["url"].as_str().unwrap()).unwrap();
        let mut output = client.stream(&source);
        let mut ids = Vec::new();
        if let Some(limit) = case["consume_limit"].as_u64() {
            for _ in 0..limit {
                let Some(item) = output.next().await else {
                    break;
                };
                ids.push(
                    item.unwrap_or_else(|error| panic!("{name}: {error}"))
                        .metadata
                        .unwrap()
                        .post_id,
                );
            }
        } else {
            while let Some(item) = output.next().await {
                ids.push(
                    item.unwrap_or_else(|error| panic!("{name}: {error}"))
                        .metadata
                        .unwrap()
                        .post_id,
                );
            }
        }
        drop(output); // Never poll N+1 after a finite consumer limit.
        let expected_ids: Vec<_> = case["emitted_ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(scalar)
            .collect();
        assert_eq!(ids, expected_ids, "{name}");
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), expected_calls.len(), "{name}");
        for (actual, expected) in requests.iter().zip(expected_calls) {
            let target = actual
                .lines()
                .next()
                .unwrap()
                .split_whitespace()
                .nth(1)
                .unwrap();
            let actual = root.join(target).unwrap();
            assert_eq!(
                actual.path(),
                Url::parse(expected["url"].as_str().unwrap())
                    .unwrap()
                    .path(),
                "{name}"
            );
            let actual_pairs: Vec<_> = actual
                .query_pairs()
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect();
            let expected_pairs: std::collections::BTreeMap<_, _> = expected["wire_params"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(key, value)| (key.clone(), scalar(value)))
                .collect();
            assert_eq!(
                actual_pairs.len(),
                expected_pairs.len(),
                "{name}: duplicate/extra query parameters"
            );
            assert_eq!(
                actual_pairs
                    .into_iter()
                    .collect::<std::collections::BTreeMap<_, _>>(),
                expected_pairs,
                "{name}"
            );
        }
        if name.ends_with("configured_one_beyond_100_pages") {
            assert_eq!(ids.len(), 101);
            assert_eq!(requests.len(), 102);
            beyond_one_hundred += 1;
        }
    }
    assert_eq!(cases.len(), 100);
    assert_eq!(beyond_one_hundred, 2);
}

#[tokio::test]
async fn invalid_last_cursor_is_reported_after_valid_page_files_and_balanced_post() {
    for site in [Site::Danbooru, Site::E621] {
        for cancel in [false, true] {
            let valid = if site == Site::Danbooru {
                danbooru_post(42)
            } else {
                e621_post(42)
            };
            let mut malformed = valid.clone();
            malformed.as_object_mut().unwrap().shift_remove("id");
            malformed["private_cursor_sentinel"] = json!("PRIVATE-invalid-cursor");
            let page = if site == Site::Danbooru {
                json!([valid, malformed])
            } else {
                json!({"posts":[valid,malformed]})
            };
            let (root, server) = serve(vec![response("200 OK", "", page.to_string().as_bytes())]);
            let client = booru(site, root).with_threshold(Some(2));
            let source = Url::parse(&format!(
                "https://{}/posts?tags=cat",
                if site == Site::Danbooru {
                    "danbooru.donmai.us"
                } else {
                    "e621.net"
                }
            ))
            .unwrap();
            let mut events = client.events(&source);
            let ExtractionEvent::Post(post) = events.next().await.unwrap().unwrap() else {
                panic!()
            };
            assert_eq!(post.metadata.unwrap().post_id, "42");
            let ExtractionEvent::Media(media) = events.next().await.unwrap().unwrap() else {
                panic!()
            };
            assert_eq!(media.metadata.unwrap().post_id, "42");
            if !cancel {
                assert!(matches!(
                    events.next().await.unwrap().unwrap(),
                    ExtractionEvent::EndPost
                ));
                let failure = events.next().await.unwrap().unwrap_err().to_string();
                assert_eq!(failure, "invalid booru pagination cursor");
                assert!(!failure.contains("PRIVATE"));
                assert!(events.next().await.is_none());
            }
            drop(events);
            assert_eq!(server.join().unwrap().len(), 1);
        }
    }
}

#[tokio::test]
async fn live_metadata_hooks_change_next_cursor_at_the_source_checkpoint() {
    use crate::postprocess::{
        Postprocessors, Specification,
        jobs::{self, JobConfiguration},
    };
    let fixture = fixture();
    for case in fixture["mutation_cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let site = Site::from_category(case["family"].as_str().unwrap()).unwrap();
        let expected_calls = case["calls"].as_array().unwrap();
        let replies = case["input_pages"]
            .as_array()
            .unwrap()
            .iter()
            .take(expected_calls.len())
            .map(|ids| response("200 OK", "", body(site, ids).to_string().as_bytes()))
            .collect();
        let (api_root, server) = serve(replies);
        let directory = tempfile::tempdir().unwrap();
        let mut options = case["hook_options"].as_array().unwrap().clone();
        // Run a real writing hook AFTER mutations: this is also evidence that
        // deleted IDs remain absent and typed arithmetic remains numeric.
        options.push(json!({"name":"metadata","mode":"jsonl","event":"file",
            "base-directory":directory.path(),"directory":[],"filename":"files.jsonl"}));
        let processors = Arc::new(Postprocessors::new(
            options
                .iter()
                .map(|option| Specification::parse(option, directory.path()).unwrap())
                .collect(),
        ));
        let configurations = Arc::new(std::collections::BTreeMap::from([(
            site.category().into(),
            JobConfiguration {
                processors: processors.clone(),
                root: directory.path().into(),
                filename: None,
                directory: None,
                init: false,
            },
        )]));
        let client = booru(site, api_root.clone()).with_threshold(Some(1));
        let source = Url::parse(case["url"].as_str().unwrap()).unwrap();
        let input = client
            .events(&source)
            .map(|event| {
                event.map(|mut event| {
                    if let ExtractionEvent::Media(media) = &mut event {
                        (media.url, media.job) =
                            crate::extract::inline_text("pagination fixture".into()).unwrap();
                    }
                    event
                })
            })
            .boxed();
        let mut output = jobs::apply_with_descriptor(
            input,
            configurations,
            Some(site.category().into()),
            crate::source_archive::for_url(&source),
        );
        let download = crate::download::client(Duration::from_secs(1)).unwrap();
        let mut errors = Vec::new();
        let mut files = 0;
        while let Some(item) = output.next().await {
            let job = match item {
                Ok(job) => job,
                Err(error) => {
                    errors.push(error.to_string());
                    continue;
                }
            };
            crate::download::download_with_options(
                &download,
                directory.path(),
                &job.media,
                crate::download::DownloadOptions {
                    postprocessors: Some(&processors),
                    postprocessor_scope: job.scope.as_ref(),
                    ..Default::default()
                },
            )
            .await
            .unwrap_or_else(|error| panic!("{name}: {error}"));
            files += 1;
        }
        drop(output);
        assert_eq!(files, case["files"].as_array().unwrap().len(), "{name}");
        if case["error"].is_null() {
            assert!(errors.is_empty(), "{name}: {errors:?}");
        } else {
            assert_eq!(errors, ["invalid booru pagination cursor"], "{name}");
        }
        let contexts = std::fs::read_to_string(directory.path().join("files.jsonl")).unwrap();
        let contexts: Vec<Value> = contexts
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(contexts.len(), files, "{name}");
        for (context, expected) in contexts.iter().zip(case["files"].as_array().unwrap()) {
            assert_eq!(
                context.as_object().unwrap().contains_key("id"),
                expected["present"].as_bool().unwrap(),
                "{name}"
            );
            if expected["present"] == true {
                assert_eq!(context["id"], expected["value"], "{name}");
            }
        }
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), expected_calls.len(), "{name}");
        for (request, expected) in requests.iter().zip(expected_calls) {
            let target = request
                .lines()
                .next()
                .unwrap()
                .split_whitespace()
                .nth(1)
                .unwrap();
            let url = api_root.join(target).unwrap();
            let actual: std::collections::BTreeMap<_, _> = url
                .query_pairs()
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect();
            let expected: std::collections::BTreeMap<_, _> = expected["wire_params"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(key, value)| (key.clone(), scalar(value)))
                .collect();
            assert_eq!(actual, expected, "{name}");
        }
    }
}
