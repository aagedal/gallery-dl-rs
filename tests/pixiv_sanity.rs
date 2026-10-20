use futures_util::StreamExt;
use gallery_dl_rs::{
    event::ExtractionEvent,
    pixiv::{Pixiv, UgoiraMode},
};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::TcpListener,
    thread,
    time::{Duration, Instant},
};
use url::Url;
fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/pixiv/sanity.json")).unwrap()
}
fn reply(status: &str, body: &Value) -> Vec<u8> {
    let body = body.to_string();
    format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",body.len()).into_bytes()
}
fn serve(replies: Vec<Vec<u8>>) -> (Url, thread::JoinHandle<Vec<String>>) {
    serve_inner(replies, false)
}
fn serve_inner(
    replies: Vec<Vec<u8>>,
    assert_quiet: bool,
) -> (Url, thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let root = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    listener.set_nonblocking(true).unwrap();
    let handle = thread::spawn(move || {
        let mut seen = Vec::new();
        for reply in replies {
            let deadline = Instant::now() + Duration::from_secs(6);
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(pair) => break pair,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "expected HTTP request did not arrive; saw {seen:?}"
                        );
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("HTTP accept failed: {error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut bytes = Vec::new();
            loop {
                let mut block = [0; 8192];
                let n = stream.read(&mut block).unwrap();
                assert_ne!(n, 0);
                bytes.extend_from_slice(&block[..n]);
                if bytes.windows(4).any(|b| b == b"\r\n\r\n") {
                    break;
                }
            }
            seen.push(String::from_utf8_lossy(&bytes).into_owned());
            stream.write_all(&reply).unwrap();
        }
        if assert_quiet {
            let deadline = Instant::now() + Duration::from_millis(100);
            while Instant::now() < deadline {
                match listener.accept() {
                    Ok(_) => panic!("unexpected request after bounded final post; saw {seen:?}"),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5))
                    }
                    Err(error) => panic!("HTTP accept failed: {error}"),
                }
            }
        }
        seen
    });
    (root, handle)
}
fn token() -> Vec<u8> {
    reply(
        "200 OK",
        &json!({"access_token":"SYNTHETIC_TOKEN","expires_in":3600,"user":{"id":"5","name":"Fixture","account":"fixture"}}),
    )
}
fn native(root: Url) -> Pixiv {
    Pixiv::new(Duration::from_secs(5))
        .unwrap()
        .with_api_root(root.clone())
        .with_oauth_root(root.clone())
        .with_web_root(root.clone())
        .with_ugoira_probe_root(root)
        .with_refresh_token(Some("SYNTHETIC_REFRESH"))
        .with_retries(0)
}
#[tokio::test]
async fn limited_work_recovery_matches_actual_source_urls_metadata_and_request_order() {
    for case in fixture()["file_cases"].as_array().unwrap() {
        let mut work = case["input_work"].clone();
        if case["direct_ajax"] == true {
            work["meta_single_page"]["original_image_url"] =
                json!("https://s.pximg.net/common/images/limit_sanity_level_360.png");
        }
        let mut replies = vec![token(), reply("200 OK", &json!({"illust":work}))];
        let mut expected = vec![
            ("POST".to_owned(), "/auth/token".to_owned()),
            (
                "GET".to_owned(),
                "/v1/illust/detail?illust_id=42".to_owned(),
            ),
        ];
        // Direct helper cases enter through the same limited-work AJAX request.
        if case["direct_ajax"] == true {
            replies.push(reply(
                "200 OK",
                &json!({"error":false,"body":case["ajax_body"]}),
            ));
            expected.push(("GET".into(), "/ajax/illust/42".into()));
        }
        for call in case["calls"].as_array().unwrap() {
            let url = Url::parse(call["url"].as_str().unwrap()).unwrap();
            let path = match url.query() {
                Some(q) => format!("{}?{q}", url.path()),
                None => url.path().into(),
            };
            if call["method"] == "request_json" {
                let body = if url.path().ends_with("/ugoira_meta") {
                    &case["ugoira_body"]
                } else {
                    &case["ajax_body"]
                };
                replies.push(reply("200 OK", &json!({"error":false,"body":body})));
                expected.push(("GET".into(), path));
            } else {
                let success = call["outcome"] == "success";
                replies.push(reply(
                    if success { "200 OK" } else { "404 Not Found" },
                    &json!({}),
                ));
                expected.push(("HEAD".into(), path));
            }
        }
        let (root, server) = serve(replies);
        let mode = UgoiraMode::parse(&case["ugoira"]);
        let events = native(root)
            .with_sanity(case["sanity"].as_bool().unwrap())
            .with_ugoira_mode(mode)
            .events(&Url::parse("https://www.pixiv.net/artworks/42").unwrap())
            .collect::<Vec<_>>()
            .await;
        let actual = events
            .iter()
            .filter_map(|event| match event {
                Ok(ExtractionEvent::Media(file)) => Some(file.url.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        let source = case["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|file| file["url"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(actual, source, "{}", case["name"]);
        if case["error"].is_null() {
            assert!(
                events.iter().all(Result::is_ok),
                "{}: {events:?}",
                case["name"]
            );
            let post = events
                .iter()
                .find_map(|event| match event {
                    Ok(ExtractionEvent::Post(post)) => Some(post),
                    _ => None,
                })
                .unwrap();
            assert_eq!(post.count, Some(source.len()), "{}", case["name"]);
            let metadata = post.metadata.as_ref().unwrap();
            assert_eq!(
                metadata.ajax,
                case["work"]["_ajax"].as_bool(),
                "{}",
                case["name"]
            );
            assert_eq!(
                metadata.mypixiv,
                case["work"]["_mypixiv"].as_bool(),
                "{}",
                case["name"]
            );
            for event in &events {
                if let Ok(ExtractionEvent::Media(file)) = event {
                    let metadata = file.metadata.as_ref().unwrap();
                    assert_eq!(metadata.ajax, case["work"]["_ajax"].as_bool());
                    assert_eq!(metadata.mypixiv, case["work"]["_mypixiv"].as_bool());
                    assert_eq!(
                        metadata.username.as_deref(),
                        case["work"]["user"]["account"].as_str()
                    );
                    if let Some(detail) = &metadata.pixiv_user_detail {
                        assert_eq!(
                            detail.fields()["user"]["name"],
                            case["work"]["user"]["name"]
                        );
                    }
                    let expected_tags = case["work"]["tags"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|tag| tag["name"].clone())
                        .collect::<Vec<_>>();
                    assert_eq!(metadata.tags.as_ref().unwrap().values(), expected_tags);
                    if let Some(info) = &metadata.file {
                        if let Some(width) = info.width {
                            assert_eq!(Some(width), case["work"]["width"].as_u64());
                        }
                        if let Some(height) = info.height {
                            assert_eq!(Some(height), case["work"]["height"].as_u64());
                        }
                    }
                }
            }

            if let Some(caption) = case["work"]["caption"].as_str() {
                assert_eq!(
                    post.metadata.as_ref().unwrap().caption_html.as_deref(),
                    Some(caption),
                    "{}",
                    case["name"]
                );
            }
        } else {
            assert!(events.iter().any(Result::is_err), "{}", case["name"]);
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, Ok(ExtractionEvent::Post(_))))
            );
        }
        let seen = server.join().unwrap();
        assert_eq!(seen.len(), expected.len());
        for (request, (method, path)) in seen.iter().zip(expected) {
            assert!(
                request.starts_with(&format!("{method} {path} ")),
                "{}: {request}",
                case["name"]
            );
            if path.starts_with("/ajax/") || method == "HEAD" {
                assert!(!request.to_ascii_lowercase().contains("authorization:"));
            }
        }
    }
}
#[test]
fn recorded_merge_duplicates_tail_and_alias_scope_remain_explicit() {
    let source = fixture();
    let cases = source["extend_cases"].as_array().unwrap();
    let ids = |name: &str| {
        cases.iter().find(|case| case["name"] == name).unwrap()["works"]
            .as_array()
            .unwrap()
            .iter()
            .map(|work| work["id"].as_u64().unwrap())
            .collect::<Vec<_>>()
    };
    assert_eq!(ids("interleave"), [9, 8, 7, 5, 3, 2, 1]);
    assert_eq!(ids("duplicates"), [7, 5, 3, 3, 1]);
    assert_eq!(ids("empty_app_tail"), [3, 2, 1]);
    assert_eq!(ids("repeated_app"), [5, 5, 3]);
    for case in source["listing_cases"].as_array().unwrap() {
        let calls = case["calls"].as_array().unwrap();
        assert_eq!(calls[0], json!({"method":"user_illusts","user_id":"5"}));
        if case["name"].as_str().unwrap().contains("tag_after_merge") {
            assert_eq!(case["works"].as_array().unwrap().len(), 1);
            assert_eq!(case["works"][0]["id"], 8);
        }
    }
}

#[tokio::test]
async fn artwork_aliases_tags_duplicates_and_tail_match_source_listing_merge() {
    let source = fixture();
    let recovered = source["file_cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["name"] == "limited_illustration_recovers_multiple")
        .unwrap()["ajax_body"]
        .clone();
    for case in source["listing_cases"].as_array().unwrap() {
        let sanity = case["options"]["sanity"].as_bool().unwrap_or(true);
        let mut replies = Vec::new();
        if sanity {
            replies.push(reply(
                "200 OK",
                &json!({"error":false,"body":case["profile_body"]}),
            ));
        }
        replies.extend([
            token(),
            reply(
                "200 OK",
                &json!({"illusts":case["input_works"],"next_url":null}),
            ),
        ]);
        let expected = case["works"].as_array().unwrap();
        for work in expected {
            if work["user"]["id"] == 1 {
                let mut body = recovered.clone();
                body["id"] = work["id"].to_string().into();
                body["pageCount"] = 1.into();
                body["urls"]["original"] = format!(
                    "https://i.pximg.net/img-original/img/2024/01/02/03/04/05/{}_p0.jpg",
                    work["id"]
                )
                .into();
                replies.push(reply("200 OK", &json!({"error":false,"body":body})));
            }
        }
        let (root, server) = serve(replies);
        let native = native(root).with_sanity(sanity);
        let events = native
            .events(&Url::parse(case["url"].as_str().unwrap()).unwrap())
            .collect::<Vec<_>>()
            .await;
        let posts = events
            .iter()
            .filter_map(|event| match event {
                Ok(ExtractionEvent::Post(post)) => {
                    Some(post.metadata.as_ref().unwrap().post_id.clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let source_posts = expected
            .iter()
            .map(|work| work["id"].to_string())
            .collect::<Vec<_>>();
        assert!(
            events.iter().all(Result::is_ok),
            "{}: {events:?}",
            case["name"]
        );
        assert_eq!(posts, source_posts, "{}", case["name"]);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, Ok(ExtractionEvent::EndPost)))
                .count(),
            posts.len()
        );
        let requests = server.join().unwrap();
        let feeds = requests
            .iter()
            .filter(|request| request.starts_with("GET /v1/user/illusts?"))
            .collect::<Vec<_>>();
        assert_eq!(feeds.len(), 1, "{}", case["name"]);
        assert!(
            feeds[0].starts_with("GET /v1/user/illusts?user_id=5 "),
            "{}: {}",
            case["name"],
            feeds[0]
        );
        let recovered_ids = expected
            .iter()
            .filter(|work| work["user"]["id"] == 1)
            .map(|work| format!("GET /ajax/illust/{} ", work["id"]))
            .collect::<Vec<_>>();
        let actual = requests
            .iter()
            .filter(|request| request.starts_with("GET /ajax/illust/"))
            .collect::<Vec<_>>();
        assert_eq!(actual.len(), recovered_ids.len(), "{}", case["name"]);
        for (request, prefix) in actual.iter().zip(recovered_ids) {
            assert!(request.starts_with(&prefix));
            assert!(!request.to_ascii_lowercase().contains("authorization:"));
        }
    }
}

#[tokio::test]
async fn disabled_sanity_final_empty_post_drains_each_artwork_feed_without_later_requests() {
    let source = fixture();
    let first = source["file_cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["name"] == "sanity_false_limit_sanity_level_360.png")
        .unwrap()["input_work"]
        .clone();
    let mut later = first.clone();
    later["id"] = 43.into();
    for route in [
        "https://www.pixiv.net/artworks/42",
        "https://www.pixiv.net/users/5/illustrations",
        "https://www.pixiv.net/ranking.php?date=20240101",
        "https://www.pixiv.net/tags/cat",
        "https://www.pixiv.net/user/5/series/7",
        "https://www.pixiv.net/bookmark_new_illust.php",
        "https://www.pixivision.net/a/77",
        "https://www.pixiv.net/users/5/bookmarks/artworks",
    ] {
        let article = route.contains("pixivision");
        let direct = route.contains("/artworks/42");
        let bookmark = route.contains("/bookmarks/");
        let mut replies = Vec::new();
        if article {
            let body = "<title>Fixture article</title><a href=\"https://www.pixiv.net/en/artworks/42\"><a href=\"https://www.pixiv.net/en/artworks/43\">";
            replies.push(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .into_bytes(),
            );
        }
        replies.push(token());
        if bookmark {
            replies.push(reply(
                "200 OK",
                &json!({"user":{"id":5,"account":"fixture","name":"Fixture"}}),
            ));
        }
        let mut body = if direct || article {
            json!({"illust":first})
        } else {
            json!({"illusts":[first,later],"next_url":null})
        };
        if route.contains("/series/") {
            body["illust_series_detail"] =
                json!({"id":7,"title":"Fixture series","caption":"","series_work_count":2});
        }
        replies.push(reply("200 OK", &body));
        let expected_requests = replies.len();
        let (root, server) = serve_inner(replies, true);
        let native = native(root.clone())
            .with_pixivision_root(root)
            .with_sanity(false)
            .with_comments(true)
            .with_captions(true)
            .with_max_posts(Some(1));
        let events = native
            .events(&Url::parse(route).unwrap())
            .collect::<Vec<_>>()
            .await;
        assert_eq!(events.len(), 2, "{route}: {events:?}");
        let Ok(ExtractionEvent::Post(post)) = &events[0] else {
            panic!("{route}: {events:?}")
        };
        assert_eq!(post.count, Some(0), "{route}");
        let metadata = post.metadata.as_ref().unwrap();
        assert_eq!(metadata.post_id, "42");
        assert_eq!(metadata.ajax, Some(true), "{route}");
        assert_eq!(metadata.mypixiv, None, "{route}");
        assert!(metadata.comments.as_ref().unwrap().is_empty(), "{route}");
        assert!(matches!(events[1], Ok(ExtractionEvent::EndPost)), "{route}");
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), expected_requests, "{route}");
        assert!(
            !requests
                .iter()
                .any(|request| request.starts_with("GET /ajax/")
                    || request.contains("/comments?")
                    || request.contains("illust_id=43")),
            "{route}: {requests:?}"
        );
    }
}
