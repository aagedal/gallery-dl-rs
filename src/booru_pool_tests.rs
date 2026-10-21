//! Native pool/favorite-group replay against actual offline source extraction.
use super::tests::{booru, response};
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

fn cases() -> Vec<(&'static str, Value)> {
    [
        (
            "pool",
            include_str!("../tests/fixtures/source_archive/booru_pool.json"),
        ),
        (
            "favgroup",
            include_str!("../tests/fixtures/source_archive/booru_favgroup.json"),
        ),
    ]
    .into_iter()
    .flat_map(|(kind, source)| {
        let fixture: Value = serde_json::from_str(source).unwrap();
        fixture["cases"]
            .as_array()
            .unwrap()
            .iter()
            .cloned()
            .map(|case| (kind, case))
            .collect::<Vec<_>>()
    })
    .collect()
}
fn collection_response<'a>(kind: &str, case: &'a Value) -> &'a Value {
    &case[if kind == "pool" {
        "pool_response"
    } else {
        "favgroup_response"
    }]
}

fn input_post(family: &str) -> Value {
    let fixture: Value = serde_json::from_str(include_str!(
        "../tests/fixtures/source_archive/booru_search.json"
    ))
    .unwrap();
    fixture["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["family"] == family && case.get("input_post").is_some())
        .unwrap()["input_post"]
        .clone()
}
fn scalar(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        Value::Number(value) => value.to_string(),
        _ => panic!("unexpected source query scalar"),
    }
}

#[tokio::test]
async fn native_collection_traversal_matches_actual_source_order_context_and_requests() {
    use crate::postprocess::template::Template;
    let mut counts = std::collections::BTreeMap::<&str, usize>::new();
    for (collection, case) in cases() {
        if !case["hook_options"].as_array().unwrap().is_empty() {
            continue; // Live nested-object ownership has a separate runtime suite.
        }
        let family = case["family"].as_str().unwrap();
        let site = Site::from_category(family).unwrap();
        if site == Site::Danbooru && case["options"]["order-posts"] == "invalid" {
            // Source raises UnboundLocalError; the native typed builder cannot
            // construct this invalid configuration. Retain it in the oracle.
            assert!(
                case["error"]
                    .as_str()
                    .unwrap()
                    .starts_with("UnboundLocalError:")
            );
            continue;
        }
        let order = match case["options"]["order-posts"].as_str() {
            None | Some("asc" | "pool" | "pool_asc" | "asc_pool") => PoolOrder::Pool,
            Some("desc" | "desc_pool" | "pool_desc") => PoolOrder::PoolReverse,
            Some("id" | "desc_id" | "id_desc") => PoolOrder::IdDescending,
            Some("asc_id" | "id_asc") => PoolOrder::IdAscending,
            Some("invalid") if site == Site::E621 => PoolOrder::Pool,
            value => panic!("unexpected source order {value:?}"),
        };
        let name = case["name"].as_str().unwrap();
        let expected_calls = case["calls"].as_array().unwrap();
        let input = input_post(family);
        let mut replies = vec![response(
            "200 OK",
            "",
            collection_response(collection, &case)
                .to_string()
                .as_bytes(),
        )];
        replies.extend(
            case["input_pages"]
                .as_array()
                .unwrap()
                .iter()
                .take(expected_calls.len() - 1)
                .map(|ids| {
                    let rows: Vec<Value> = ids
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|id| {
                            let mut post = input.clone();
                            post["id"] = id.clone();
                            post
                        })
                        .collect();
                    let body = if site == Site::E621 {
                        json!({"posts":rows})
                    } else {
                        Value::Array(rows)
                    };
                    response("200 OK", "", body.to_string().as_bytes())
                }),
        );
        let (root, server) = serve(replies);
        let threshold = case["options"]
            .get("threshold")
            .map(|value| usize::try_from(value.as_u64().unwrap()).unwrap());
        let client = booru(site, root.clone())
            .with_threshold(threshold)
            .with_pool_order(order);
        let source = Url::parse(case["url"].as_str().unwrap()).unwrap();
        let mut events = client.events(&source);
        let mut outputs = Vec::new();
        let mut errors = Vec::new();
        let mut ended = 0;
        let mut media_count = 0;
        while let Some(event) = events.next().await {
            match event {
                Ok(ExtractionEvent::Post(post)) => {
                    let metadata = post.metadata.unwrap();
                    outputs.push(("post", metadata, None));
                }
                Ok(ExtractionEvent::Media(media)) => {
                    let context = crate::postprocess::context(&media);
                    outputs.push(("media", media.metadata.unwrap(), Some(context)));
                    media_count += 1;
                    if case["consume_limit"].as_u64() == Some(media_count) {
                        break; // No N+1 poll, including no EndPost/nextPage.
                    }
                }
                Ok(ExtractionEvent::EndPost) => ended += 1,
                Ok(_) => panic!("unexpected child boundary in pool {name}"),
                Err(error) => errors.push(error.to_string()),
            }
        }
        drop(events);
        let expected = case["events"].as_array().unwrap();
        assert_eq!(outputs.len(), expected.len(), "{name}: {errors:?}");
        assert_eq!(
            ended,
            outputs
                .iter()
                .filter(|(kind, _, _)| *kind == "post")
                .count()
                - usize::from(case["stopped_by_consumer"] == true),
            "{name}"
        );
        for ((kind, metadata, context), expected) in outputs.iter().zip(expected) {
            assert_eq!(*kind, expected["kind"].as_str().unwrap(), "{name}");
            let fields = &expected["metadata"];
            assert_eq!(metadata.post_id, scalar(&fields["id"]), "{name}");
            assert_eq!(metadata.source_num.as_ref(), Some(&fields["num"]), "{name}");
            let value = if collection == "pool" {
                metadata.pool.as_deref()
            } else {
                metadata.favgroup.as_deref()
            };
            assert_eq!(value, Some(&fields[collection]), "{name}");
            if let Some(context) = context {
                assert_eq!(
                    Template::parse(case["archive_format"].as_str().unwrap())
                        .unwrap()
                        .render(context)
                        .unwrap(),
                    expected["archive"],
                    "{name}"
                );
                assert_eq!(
                    Template::parse("{num:>04}_{id}")
                        .unwrap()
                        .render(context)
                        .unwrap(),
                    expected["default_filename"]
                        .as_str()
                        .unwrap()
                        .split('_')
                        .take(2)
                        .collect::<Vec<_>>()
                        .join("_"),
                    "{name}"
                );
                for (format, expected) in [
                    "{category}".to_owned(),
                    if collection == "pool" {
                        "pool"
                    } else {
                        "Favorite Groups"
                    }
                    .to_owned(),
                    format!("{{{collection}[id]}} {{{collection}[name]}}"),
                ]
                .iter()
                .zip(expected["default_directory"].as_array().unwrap())
                {
                    assert_eq!(
                        Template::parse(format).unwrap().render(context).unwrap(),
                        *expected,
                        "{name}"
                    );
                }
            }
        }
        if case["error"].is_null() {
            assert!(errors.is_empty(), "{name}: {errors:?}");
        } else {
            let expected = if site == Site::E621 {
                "invalid repeated e621 pool member"
            } else {
                "invalid booru collection member"
            };
            assert_eq!(errors, [expected], "{name}");
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
            let actual = root.join(target).unwrap();
            assert_eq!(
                actual.path(),
                Url::parse(expected["url"].as_str().unwrap())
                    .unwrap()
                    .path(),
                "{name}"
            );
            let pairs: Vec<_> = actual
                .query_pairs()
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect();
            let wanted: std::collections::BTreeMap<_, _> = expected["wire_params"]
                .as_object()
                .map(|params| {
                    params
                        .iter()
                        .map(|(key, value)| (key.clone(), scalar(value)))
                        .collect()
                })
                .unwrap_or_default();
            assert_eq!(pairs.len(), wanted.len(), "{name}: duplicate parameters");
            assert_eq!(
                pairs
                    .into_iter()
                    .collect::<std::collections::BTreeMap<_, _>>(),
                wanted,
                "{name}"
            );
        }
        *counts.entry(collection).or_default() += 1;
    }
    assert_eq!(counts.get("pool"), Some(&39));
    assert_eq!(counts.get("favgroup"), Some(&31));
}

#[tokio::test]
async fn live_collection_hooks_preserve_nested_aliases_and_detach_replaced_roots() {
    use crate::postprocess::{
        Postprocessors, Specification,
        jobs::{self, JobConfiguration},
    };
    let mut counts = std::collections::BTreeMap::<&str, usize>::new();
    for (collection, case) in cases() {
        if case["hook_options"].as_array().unwrap().is_empty() {
            continue;
        }
        let name = case["name"].as_str().unwrap();
        let family = case["family"].as_str().unwrap();
        let site = Site::from_category(family).unwrap();
        let source = Url::parse(case["url"].as_str().unwrap()).unwrap();
        let expected_calls = case["calls"].as_array().unwrap();
        let mut input = input_post(family);
        input
            .as_object_mut()
            .unwrap()
            .extend(case["post_extra"].as_object().unwrap().clone());
        let mut replies = vec![response(
            "200 OK",
            "",
            collection_response(collection, &case)
                .to_string()
                .as_bytes(),
        )];
        replies.extend(
            case["input_pages"]
                .as_array()
                .unwrap()
                .iter()
                .take(expected_calls.len() - 1)
                .map(|ids| {
                    let rows: Vec<Value> = ids
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|id| {
                            let mut post = input.clone();
                            post["id"] = id.clone();
                            post
                        })
                        .collect();
                    let body = if site == Site::E621 {
                        json!({"posts": rows})
                    } else {
                        Value::Array(rows)
                    };
                    response("200 OK", "", body.to_string().as_bytes())
                }),
        );
        let (api_root, server) = serve(replies);
        let directory = tempfile::tempdir().unwrap();
        let mut options = case["hook_options"].as_array().unwrap().clone();
        for event in ["post", "file"] {
            let mut observer = json!({"name":"metadata","mode":"jsonl","event":event,
                "base-directory":directory.path(),"directory":[],"filename":format!("{event}.jsonl")});
            if event == "file" && name.contains(&format!("nested_{collection}_")) {
                // Exercise actual implicit source archive initialization and
                // writes against the live (possibly hook-mutated) pool object.
                observer["archive"] = json!(directory.path().join("metadata.sqlite"));
            }
            options.push(observer);
        }
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
        let input = client
            .events(&source)
            .map(|event| {
                event.map(|mut event| {
                    if let ExtractionEvent::Media(media) = &mut event {
                        (media.url, media.job) =
                            crate::extract::inline_text("pool fixture".into()).unwrap();
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
        let mut files = 0;
        while let Some(item) = output.next().await {
            let job = item.unwrap_or_else(|error| panic!("{name}: {error}"));
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
        assert_eq!(files, 2, "{name}");
        for (event, kind) in [("post", "post"), ("file", "media")] {
            let written =
                std::fs::read_to_string(directory.path().join(format!("{event}.jsonl"))).unwrap();
            let written: Vec<Value> = written
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            let expected: Vec<_> = case["events"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|item| item["kind"] == kind)
                .collect();
            assert_eq!(written.len(), expected.len(), "{name} {event}");
            for (actual, expected) in written.iter().zip(expected) {
                let expected = expected["metadata"].as_object().unwrap();
                assert_eq!(
                    actual.get(collection),
                    expected.get(collection),
                    "{name} {event}"
                );
                assert_eq!(actual.get("num"), expected.get("num"), "{name} {event}");
            }
        }
        if name.contains(&format!("nested_{collection}_")) {
            let connection =
                rusqlite::Connection::open(directory.path().join("metadata.sqlite")).unwrap();
            let entries: Vec<String> = connection
                .prepare("SELECT entry FROM archive ORDER BY entry")
                .unwrap()
                .query_map([], |row| row.get(0))
                .unwrap()
                .collect::<std::result::Result<_, _>>()
                .unwrap();
            let mut expected: Vec<String> = case["events"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|event| event["kind"] == "media")
                .map(|event| {
                    format!(
                        "{}_MD_{}",
                        site.category(),
                        event["archive"].as_str().unwrap()
                    )
                })
                .collect();
            expected.sort();
            assert_eq!(
                entries, expected,
                "{name}: source default metadata archive keys"
            );
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
            let actual = api_root.join(target).unwrap();
            let actual: std::collections::BTreeMap<_, _> = actual
                .query_pairs()
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect();
            let expected: std::collections::BTreeMap<_, _> = expected["wire_params"]
                .as_object()
                .map(|params| {
                    params
                        .iter()
                        .map(|(key, value)| (key.clone(), scalar(value)))
                        .collect()
                })
                .unwrap_or_default();
            assert_eq!(actual, expected, "{name}");
        }
        *counts.entry(collection).or_default() += 1;
    }
    assert_eq!(counts.get("pool"), Some(&8));
    assert_eq!(counts.get("favgroup"), Some(&4));
}

#[test]
fn favorite_group_routes_preserve_actual_source_captured_identity_and_archive_scope() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../tests/fixtures/source_archive/booru_favgroup.json"
    ))
    .unwrap();
    let routes = fixture["route_cases"].as_array().unwrap();
    assert_eq!(routes.len(), 10);
    for case in routes {
        let url = Url::parse(case["url"].as_str().unwrap()).unwrap();
        if let Some(id) = case["captured_id"].as_str() {
            assert_eq!(
                target(Site::Danbooru, &url).unwrap().0,
                Target::Favgroup { id: id.into() },
                "{url}"
            );
            let descriptor = crate::source_archive::for_url(&url).unwrap();
            assert_eq!(descriptor.category, "danbooru", "{url}");
            assert_eq!(
                descriptor.format,
                case["archive_format"].as_str().unwrap(),
                "{url}"
            );
        } else {
            assert!(target(Site::Danbooru, &url).is_err(), "{url}");
        }
    }
}

#[tokio::test]
async fn collection_routes_and_wire_identities_match_actual_source_constructor_probes() {
    use crate::postprocess::template::Template;
    let fixture: Value = serde_json::from_str(include_str!(
        "../tests/fixtures/source_archive/booru_collection_routes.json"
    ))
    .unwrap();
    let cases = fixture["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 187);
    let mut matched = 0;
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let source = Url::parse(case["url"].as_str().unwrap()).unwrap();
        let site = Site::from_category(case["family"].as_str().unwrap()).unwrap();
        if case["extractor"].is_null() {
            assert!(
                target(site, &source).is_err(),
                "{name}: source found no collection extractor"
            );
            continue;
        }
        matched += 1;
        let collection = if case["extractor"] == "DanbooruFavgroupExtractor" {
            "favgroup"
        } else {
            "pool"
        };
        let id = case["captured_id"].as_str().unwrap().to_owned();
        let expected = if collection == "pool" {
            Target::Pool { id }
        } else {
            Target::Favgroup { id }
        };
        let (actual, root) = target(site, &source).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(actual, expected, "{name}: preserve raw constructor lexeme");
        assert_eq!(
            root.as_str().trim_end_matches('/'),
            case["root"].as_str().unwrap(),
            "{name}"
        );
        let descriptor = crate::source_archive::for_url(&source).unwrap();
        assert_eq!(
            descriptor.category,
            case["category"].as_str().unwrap(),
            "{name}"
        );
        assert_eq!(
            descriptor.format,
            case["archive_format"].as_str().unwrap(),
            "{name}"
        );
        let mut post = input_post(case["family"].as_str().unwrap());
        post["id"] = case["posts"][0]["id"].clone();
        let body = if site == Site::E621 {
            json!({"posts":[post]})
        } else {
            json!([post])
        };
        let (api_root, server) = serve(vec![
            response(
                "200 OK",
                "",
                case["metadata_response"].to_string().as_bytes(),
            ),
            response("200 OK", "", body.to_string().as_bytes()),
        ]);
        let client = booru(site, api_root.clone());
        let events = client.events(&source).collect::<Vec<_>>().await;
        assert_eq!(events.len(), 3, "{name}: {events:?}");
        let ExtractionEvent::Media(media) = events[1].as_ref().unwrap() else {
            panic!("{name}: missing media");
        };
        let context = crate::postprocess::context(media);
        assert_eq!(
            context.get(collection),
            case["metadata"].get(collection),
            "{name}"
        );
        assert_eq!(context["num"], case["posts"][0]["num"], "{name}");
        assert_eq!(
            Template::parse(&descriptor.format)
                .unwrap()
                .render(&context)
                .unwrap(),
            case["archive_keys"][0],
            "{name}: response ID owns archive identity"
        );
        let requests = server.join().unwrap();
        assert_eq!(
            requests.len(),
            case["calls"].as_array().unwrap().len(),
            "{name}"
        );
        for (request, call) in requests.iter().zip(case["calls"].as_array().unwrap()) {
            let path = request
                .lines()
                .next()
                .unwrap()
                .split_whitespace()
                .nth(1)
                .unwrap();
            let actual = api_root.join(path).unwrap();
            assert_eq!(
                actual.path(),
                Url::parse(call["url"].as_str().unwrap()).unwrap().path(),
                "{name}: requested metadata ID"
            );
            let actual = actual
                .query_pairs()
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect::<std::collections::BTreeMap<_, _>>();
            let expected = call["params"]
                .as_object()
                .map(|params| {
                    params
                        .iter()
                        .filter(|(_, v)| !v.is_null())
                        .map(|(k, v)| (k.clone(), scalar(v)))
                        .collect::<std::collections::BTreeMap<_, _>>()
                })
                .unwrap_or_default();
            assert_eq!(actual, expected, "{name}: requested feed ID");
        }
    }
    assert_eq!(matched, 112);
}
