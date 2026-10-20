use futures_util::StreamExt;
use gallery_dl_rs::{
    event::ExtractionEvent,
    extract::{MediaJob, parse_messages},
    job_selection::JobSelection,
    pixiv::{NovelOptions, Pixiv},
    selection::RangeSelection,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    io::{Read, Write},
    net::TcpListener,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};
use url::Url;
fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/pixiv/novel_embeds.json")).unwrap()
}
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(v) => *v,
        Value::Number(n) => n.as_f64() != Some(0.),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}
fn response(status: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}
fn reply(body: Value) -> Vec<u8> {
    response("200 OK", &body.to_string())
}
fn token(access: &str) -> Vec<u8> {
    reply(
        json!({"access_token":access,"expires_in":3600,"user":{"id":"5","account":"fixture","name":"Fixture"}}),
    )
}
fn serve(replies: Vec<Vec<u8>>) -> (Url, thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let root = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    let handle = thread::spawn(move || {
        let mut requests = Vec::new();
        for reply in replies {
            let deadline = Instant::now() + Duration::from_secs(5);
            let (mut socket, _) = loop {
                match listener.accept() {
                    Ok(socket) => break socket,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "missing expected request after {requests:?}"
                        );
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(e) => panic!("{e}"),
                }
            };
            socket.set_nonblocking(false).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut data = Vec::new();
            loop {
                let mut block = [0; 8192];
                let n = socket.read(&mut block).unwrap();
                assert_ne!(n, 0);
                data.extend_from_slice(&block[..n]);
                if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                    let length = String::from_utf8_lossy(&data[..end])
                        .lines()
                        .find_map(|line| {
                            let (key, value) = line.split_once(':')?;
                            key.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if data.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            requests.push(String::from_utf8_lossy(&data).into_owned());
            socket.write_all(&reply).unwrap();
        }
        let deadline = Instant::now() + Duration::from_millis(50);
        while Instant::now() < deadline {
            match listener.accept() {
                Ok(_) => panic!("unexpected request beyond selected work; saw {requests:?}"),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5))
                }
                Err(error) => panic!("HTTP accept failed: {error}"),
            }
        }
        requests
    });
    (root, handle)
}
fn native(root: Url, refresh: &str) -> Pixiv {
    Pixiv::new(Duration::from_secs(5))
        .unwrap()
        .with_api_root(root.clone())
        .with_oauth_root(root.clone())
        .with_web_root(root)
        .with_sanity(false)
        .with_refresh_token(Some(refresh))
        .with_retries(0)
}
fn artwork(id: &str) -> Value {
    json!({"id":id.parse::<u64>().unwrap(),"type":"illust","title":"Child illustration","caption":"Art caption","user":{"id":8,"account":"art.artist"},"tags":[{"name":"art","translated_name":"art"}],"meta_single_page":{"original_image_url":format!("https://i.pximg.net/img-original/img/2024/01/02/03/04/05/{id}_p0.jpg")},"meta_pages":[],"create_date":"2024-01-02T12:04:05+09:00","x_restrict":0})
}
#[tokio::test]
async fn actual_source_embed_cases_match_native_files_children_mutations_and_requests() {
    for case in fixture()["cases"].as_array().unwrap() {
        // The source emits unsupported targets verbatim; native validates them.
        if case["name"] == "empty_and_noncanonical_child_targets_preserved" {
            continue;
        }
        let mut replies = vec![
            token("NOVEL_ACCESS"),
            reply(json!({"novels":case["input_works"],"next_url":null})),
        ];
        let mut expected = vec![
            "POST /auth/token ".to_string(),
            "GET /v1/user/novels?user_id=5 ".into(),
        ];
        let mut visited = BTreeSet::new();
        let mut art_started = false;
        for event in case["events"].as_array().unwrap() {
            if event["kind"] == "post" {
                let id = event["metadata"]["id"].as_u64().unwrap();
                let text = if id == 42 {
                    case["webview_text"].as_str().unwrap()
                } else {
                    "Later text [pixivimage:70]"
                };
                let failing = id == 42 && case["text_fail"] == true;
                replies.push(if failing {
                    response("404 Not Found", "PRIVATE_TEXT_FAILURE")
                } else {
                    response("200 OK", &format!("novel: {},\n", json!({"text":text})))
                });
                expected.push(format!(
                    "GET /webview/v2/novel?id={id}&viewer_version=20221031_ai "
                ));
            }
            if event["kind"] == "media" && event["url"].as_str().unwrap().starts_with("text:") {
                let id = event["metadata"]["id"].as_u64().unwrap();
                if case["calls"].as_array().unwrap().iter().any(|call| {
                    call["method"] == "request_json"
                        && call["url"]
                            .as_str()
                            .unwrap()
                            .ends_with(&format!("/novel/{id}"))
                }) {
                    replies.push(if case["request_fail"] == true {
                        response("404 Not Found", r#"{"error":true}"#)
                    } else {
                        reply(case["ajax_response"].clone())
                    });
                    expected.push(format!("GET /ajax/novel/{id} "));
                }
            }
            if event["kind"] == "child" {
                let id = event["url"].as_str().unwrap().rsplit('/').next().unwrap();
                if visited.insert(id.to_owned()) {
                    if !art_started {
                        replies.push(token("ART_ACCESS"));
                        expected.push("POST /auth/token ".into());
                        art_started = true;
                    }
                    replies.push(reply(json!({"illust":artwork(id)})));
                    expected.push(format!("GET /v1/illust/detail?illust_id={id} "));
                }
            }
        }
        let (root, server) = serve(replies);
        let art = Arc::new(native(root.clone(), "ART_REFRESH"));
        let native = native(root, "NOVEL_REFRESH")
            .with_artwork_extractor(art)
            .with_novel_options(NovelOptions {
                covers: truthy(&case["options"]["covers"]),
                full_series: false,
                embeds: truthy(&case["options"]["embeds"]),
            })
            .with_tags(gallery_dl_rs::pixiv::TagMode::parse(
                case["options"]["tags"].as_str().unwrap_or("japanese"),
            ))
            .with_max_posts(case["options"]["max-posts"].as_u64().map(|n| n as usize));
        let events = native
            .events(&Url::parse("https://www.pixiv.net/users/5/novels").unwrap())
            .collect::<Vec<_>>()
            .await;
        let mut recorded = case["events"].as_array().unwrap().iter();
        let mut root_open = false;
        let mut child_depth = 0usize;
        let mut error_count = 0;
        for event in &events {
            match event {
                Err(_) => error_count += 1,
                Ok(ExtractionEvent::BeginChild(scope)) => {
                    let source = recorded.next().unwrap();
                    assert_eq!(source["kind"], "child", "{}", case["name"]);
                    assert!(root_open);
                    assert_eq!(scope.metadata.category(), "pixiv-novel");
                    for key in [
                        "num",
                        "suffix",
                        "embeddedImageId",
                        "extra_upload",
                        "later_extra",
                        "tags",
                    ] {
                        assert_eq!(
                            scope.metadata.fields().get(key),
                            source["metadata"].get(key),
                            "{} child {key}",
                            case["name"]
                        );
                    }
                    child_depth += 1;
                }
                Ok(ExtractionEvent::EndChild(_)) => child_depth -= 1,
                Ok(ExtractionEvent::Post(post)) if child_depth == 0 => {
                    let source = recorded.next().unwrap();
                    assert_eq!(source["kind"], "post");
                    assert!(!root_open);
                    root_open = true;
                    assert_eq!(
                        post.metadata.as_ref().unwrap().post_id,
                        source["metadata"]["id"].to_string()
                    );
                }
                Ok(ExtractionEvent::EndPost) if child_depth == 0 => {
                    assert!(root_open);
                    root_open = false;
                }
                Ok(ExtractionEvent::Media(file)) if child_depth == 0 => {
                    let source = recorded.next().unwrap();
                    assert_eq!(source["kind"], "media", "{}", case["name"]);
                    let raw = source["url"].as_str().unwrap();
                    if let Some(text) = raw.strip_prefix("text:") {
                        let MediaJob::InlineText { data } = &file.job else {
                            panic!("expected text")
                        };
                        assert_eq!(data, text);
                    } else {
                        assert_eq!(file.url.as_str(), raw, "{}", case["name"]);
                    }
                    let encoded = serde_json::to_value(file.metadata.as_ref().unwrap()).unwrap();
                    assert_eq!(
                        encoded.get("tags"),
                        source["metadata"].get("tags"),
                        "{} tags",
                        case["name"]
                    );
                    for key in ["embeddedImageId", "extra_upload", "later_extra"] {
                        assert_eq!(
                            encoded.get(key),
                            source["metadata"].get(key),
                            "{} media {key}",
                            case["name"]
                        );
                    }
                    assert_eq!(file.filename, source["default_filename"].as_str().unwrap());
                    assert!(file.archive_key().is_some());
                }
                _ => {}
            }
        }
        assert!(recorded.next().is_none(), "{}: {events:?}", case["name"]);
        assert!(!root_open);
        assert_eq!(child_depth, 0);
        if !case["error"].is_null() || case["text_fail"] == true {
            assert!(error_count > 0, "{}", case["name"]);
        } else {
            assert_eq!(error_count, 0, "{}: {events:?}", case["name"]);
        }
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), expected.len(), "{}", case["name"]);
        for (request, prefix) in requests.iter().zip(expected) {
            assert!(
                request.starts_with(&prefix),
                "{}: {request}; expected {prefix}",
                case["name"]
            );
            if prefix.starts_with("GET /ajax/") {
                assert!(!request.to_ascii_lowercase().contains("authorization:"));
            } else if prefix.starts_with("GET /v1/illust/detail") {
                assert!(
                    request
                        .to_ascii_lowercase()
                        .contains("authorization: bearer art_access")
                );
            } else if prefix.starts_with("GET /webview/") {
                assert!(
                    request
                        .to_ascii_lowercase()
                        .contains("authorization: bearer novel_access")
                );
            }
        }
    }
}
#[test]
fn novel_archive_retains_extension_and_distinguishes_jpeg_from_jpg() {
    let item = |extension: &str| {
        parse_messages(&serde_json::to_vec(&json!([[3,format!("https://i.pximg.net/42.{extension}"),{"id":42,"num":0,"extension":extension,"user":{"id":5,"account":"artist"}}]])).unwrap(),"pixiv-novel").unwrap().remove(0)
    };
    let txt = item("txt");
    let jpg = item("jpg");
    let jpeg = item("jpeg");
    assert_ne!(txt.archive_key(), jpg.archive_key());
    assert_ne!(jpg.archive_key(), jpeg.archive_key());
    assert_eq!(jpeg.filename, "42_p0.jpeg");
}
#[tokio::test]
async fn selected_text_cutoff_does_not_start_uploaded_ajax_or_artwork_children() {
    let source = fixture();
    let case = &source["cases"][1];
    let (root, server) = serve(vec![
        token("NOVEL_ACCESS"),
        reply(json!({"novels":case["input_works"],"next_url":null})),
        response(
            "200 OK",
            &format!("novel: {},\n", json!({"text":case["webview_text"]})),
        ),
    ]);
    let native = native(root, "NOVEL_REFRESH").with_novel_options(NovelOptions {
        embeds: true,
        ..Default::default()
    });
    let selected = JobSelection {
        file_range: Some(RangeSelection::parse("1").unwrap()),
        ..Default::default()
    }
    .apply(native.events(&Url::parse("https://www.pixiv.net/users/5/novels").unwrap()))
    .collect::<Vec<_>>()
    .await;
    assert_eq!(selected.len(), 1);
    assert!(matches!(
        selected[0].as_ref().unwrap().job,
        MediaJob::InlineText { .. }
    ));
    assert_eq!(server.join().unwrap().len(), 3);
}

#[tokio::test]
async fn uploaded_extra_overrides_cached_artist_extra_once_in_serialized_metadata() {
    let source = fixture();
    let case = &source["cases"][1];
    let text = "Fixture [uploadedimage:a]";
    let detail = json!({"user":{"id":5,"account":"detail.artist","name":"Detail artist"},"extra_top":{"value":"from detail","nested":[1,true]}});
    let image = json!({"urls":{"original":"https://i.pximg.net/novel-cover-original/img/2024/01/03/04/05/42_upload.png"},"extra_top":{"value":"from uploaded image","nested":[2,false]}});
    let (root, server) = serve(vec![
        token("NOVEL_ACCESS"),
        reply(json!({"novels":case["input_works"],"next_url":null})),
        reply(detail),
        response("200 OK", &format!("novel: {},\n", json!({"text":text}))),
        reply(json!({"error":false,"body":{"textEmbeddedImages":{"a":image}}})),
    ]);
    let native = native(root, "NOVEL_REFRESH")
        .with_metadata(true)
        .with_novel_options(NovelOptions {
            embeds: true,
            ..Default::default()
        });
    let events = native
        .events(&Url::parse("https://www.pixiv.net/users/5/novels").unwrap())
        .collect::<Vec<_>>()
        .await;
    let mut files = 0;
    for event in events {
        let event = event.unwrap();
        let metadata = match &event {
            ExtractionEvent::Post(post) => post.metadata.as_ref().unwrap(),
            ExtractionEvent::Media(file) => file.metadata.as_ref().unwrap(),
            _ => continue,
        };
        let raw = serde_json::to_string(metadata).unwrap();
        assert_eq!(raw.matches("\"extra_top\":").count(), 1, "{raw}");
        let encoded: Value = serde_json::from_str(&raw).unwrap();
        let expected = if let ExtractionEvent::Media(file) = &event {
            files += 1;
            if matches!(file.job, MediaJob::DirectHttp) {
                "from uploaded image"
            } else {
                "from detail"
            }
        } else {
            "from detail"
        };
        assert_eq!(encoded["extra_top"]["value"], expected);
    }
    assert_eq!(files, 2);
    assert_eq!(server.join().unwrap().len(), 5);
}

#[tokio::test]
async fn child_filter_uses_mutated_parent_snapshot_before_artwork_request_and_range() {
    let source = fixture();
    let case = &source["cases"][1];
    let mut later = case["input_works"][0].clone();
    later["id"] = 43.into();
    let (root, server) = serve(vec![
        token("NOVEL_ACCESS"),
        reply(json!({"novels":[case["input_works"][0],later],"next_url":null})),
        response(
            "200 OK",
            &format!("novel: {},\n", json!({"text":case["webview_text"]})),
        ),
        reply(case["ajax_response"].clone()),
        token("ART_ACCESS"),
        reply(json!({"illust":artwork("71")})),
    ]);
    let art = Arc::new(native(root.clone(), "ART_REFRESH"));
    let native = native(root, "NOVEL_REFRESH")
        .with_artwork_extractor(art)
        .with_novel_options(NovelOptions {
            embeds: true,
            ..Default::default()
        });
    let selection = JobSelection {
        child_filter: Some(
            gallery_dl_rs::filter::FilterExpression::parse(
                "id == 42 and num == 4 and embeddedImageId == 'b' and extra_upload['second'] == 2",
            )
            .unwrap(),
        ),
        child_range: Some(RangeSelection::parse("1").unwrap()),
        ..Default::default()
    };
    let files = selection
        .apply(native.events(&Url::parse("https://www.pixiv.net/users/5/novels").unwrap()))
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<gallery_dl_rs::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(files.len(), 4);
    assert_eq!(files.last().unwrap().category, "pixiv");
    assert_eq!(
        files.last().unwrap().metadata.as_ref().unwrap().post_id,
        "71"
    );
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 6);
    assert!(requests[5].starts_with("GET /v1/illust/detail?illust_id=71 "));
    assert!(
        !requests
            .iter()
            .any(|request| request.contains("illust_id=70") || request.contains("novel?id=43"))
    );
}

#[tokio::test]
async fn invalid_source_artwork_targets_keep_child_metadata_and_balance_without_requests() {
    let source = fixture();
    let case = source["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["name"] == "empty_and_noncanonical_child_targets_preserved")
        .unwrap();
    let (root, server) = serve(vec![
        token("NOVEL_ACCESS"),
        reply(json!({"novels":case["input_works"],"next_url":null})),
        response(
            "200 OK",
            &format!("novel: {},\n", json!({"text":case["webview_text"]})),
        ),
    ]);
    let art = Arc::new(native(root.clone(), "ART_REFRESH"));
    let native = native(root, "NOVEL_REFRESH")
        .with_artwork_extractor(art)
        .with_novel_options(NovelOptions {
            embeds: true,
            ..Default::default()
        });
    let events = native
        .events(&Url::parse("https://www.pixiv.net/users/5/novels").unwrap())
        .collect::<Vec<_>>()
        .await;
    let expected = case["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["kind"] == "child")
        .collect::<Vec<_>>();
    assert_eq!(expected.len(), 3);
    let mut child = 0;
    let mut control = None;
    let mut errors = 0;
    let mut root_open = false;
    let mut ended = false;
    for event in events {
        match event {
            Ok(ExtractionEvent::Post(post)) => {
                assert!(!root_open);
                assert_eq!(post.category, "pixiv-novel");
                root_open = true;
            }
            Ok(ExtractionEvent::Media(file)) => {
                assert!(root_open && control.is_none());
                assert!(matches!(file.job, MediaJob::InlineText { .. }));
            }
            Ok(ExtractionEvent::BeginChild(scope)) => {
                assert!(root_open && control.is_none());
                let recorded = expected[child];
                for key in ["id", "num", "suffix", "tags", "caption", "date_url"] {
                    assert_eq!(
                        scope.metadata.fields().get(key),
                        recorded["metadata"].get(key),
                        "child {child}, {key}"
                    );
                }
                control = Some(scope.control);
                child += 1;
            }
            Err(error) => {
                assert!(control.is_some());
                assert!(!error.to_string().contains("ART_REFRESH"));
                errors += 1;
            }
            Ok(ExtractionEvent::EndChild(closed)) => {
                assert!(control.take().unwrap().same_scope(&closed));
            }
            Ok(ExtractionEvent::EndPost) => {
                assert!(root_open && control.is_none());
                root_open = false;
                ended = true;
            }
        }
    }
    assert_eq!(child, 3);
    assert_eq!(errors, 3);
    assert!(!root_open && control.is_none() && ended);
    assert_eq!(server.join().unwrap().len(), 3);
}

#[tokio::test]
async fn forced_artwork_prefixes_preserve_request_zeros_and_deduplicate_raw_targets() {
    let source = fixture();
    let vectors = source["forced_target_vectors"].as_array().unwrap();
    assert_eq!(
        vectors.iter().find(|row| row["raw"] == "070tail").unwrap()["groups"],
        json!(["070", null])
    );
    let text = "[pixivimage:070tail][pixivimage:70tail][pixivimage:70?x][pixivimage:70#x][pixivimage:70/path][pixivimage:070tail][pixivimage:70]";
    let mut replies = vec![
        token("NOVEL_ACCESS"),
        reply(json!({"novels":source["cases"][1]["input_works"],"next_url":null})),
        response("200 OK", &format!("novel: {},\n", json!({"text":text}))),
        token("ART_ACCESS"),
    ];
    for _ in 0..6 {
        replies.push(reply(json!({"illust":artwork("70")})));
    }
    let (root, server) = serve(replies);
    let art = Arc::new(native(root.clone(), "ART_REFRESH"));
    let native = native(root, "NOVEL_REFRESH")
        .with_artwork_extractor(art)
        .with_novel_options(NovelOptions {
            embeds: true,
            ..Default::default()
        });
    let events = native
        .events(&Url::parse("https://www.pixiv.net/users/5/novels").unwrap())
        .collect::<Vec<_>>()
        .await;
    let mut begins = 0;
    let mut ends = 0;
    let mut illustrations = 0;
    for event in events {
        match event.unwrap() {
            ExtractionEvent::BeginChild(scope) => {
                begins += 1;
                assert_eq!(scope.metadata.fields()["num"], begins);
            }
            ExtractionEvent::EndChild(_) => ends += 1,
            ExtractionEvent::Media(file) if file.category == "pixiv" => {
                illustrations += 1;
                assert_eq!(file.metadata.as_ref().unwrap().post_id, "70");
            }
            _ => {}
        }
    }
    assert_eq!((begins, ends, illustrations), (6, 6, 6));
    let requests = server.join().unwrap();
    let detail = requests
        .iter()
        .filter(|request| request.starts_with("GET /v1/illust/detail?"))
        .collect::<Vec<_>>();
    assert_eq!(detail.len(), 6);
    assert!(detail[0].starts_with("GET /v1/illust/detail?illust_id=070 "));
    assert!(
        detail[1..]
            .iter()
            .all(|request| request.starts_with("GET /v1/illust/detail?illust_id=70 "))
    );
    assert_eq!(requests.len(), 10);
}

#[tokio::test]
async fn forced_leading_zero_empty_artwork_post_uses_canonical_response_identity() {
    let source = fixture();
    let text = "[pixivimage:070tail]";
    let mut animation = artwork("70");
    animation["type"] = "ugoira".into();
    let (root, server) = serve(vec![
        token("NOVEL_ACCESS"),
        reply(json!({"novels":source["cases"][1]["input_works"],"next_url":null})),
        response("200 OK", &format!("novel: {},\n", json!({"text":text}))),
        token("ART_ACCESS"),
        reply(json!({"illust":animation})),
    ]);
    let art = Arc::new(native(root.clone(), "ART_REFRESH").with_ugoira(false));
    let native = native(root, "NOVEL_REFRESH")
        .with_artwork_extractor(art)
        .with_novel_options(NovelOptions {
            embeds: true,
            ..Default::default()
        });
    let events = native
        .events(&Url::parse("https://www.pixiv.net/users/5/novels").unwrap())
        .collect::<Vec<_>>()
        .await;
    let mut child_posts = 0;
    for event in events {
        if let ExtractionEvent::Post(post) = event.unwrap()
            && post.category == "pixiv"
        {
            child_posts += 1;
            assert_eq!(post.count, Some(0));
            assert_eq!(post.metadata.as_ref().unwrap().post_id, "70");
        }
    }
    assert_eq!(child_posts, 1);
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 5);
    assert!(requests[4].starts_with("GET /v1/illust/detail?illust_id=070 "));
}
