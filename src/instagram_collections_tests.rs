use super::*;
use serde_json::json;
use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

struct Server {
    base: Url,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Server {
    fn new(responses: Vec<Value>) -> Self {
        Self::delayed(
            responses
                .into_iter()
                .map(|value| (value, Duration::ZERO))
                .collect(),
        )
    }

    fn delayed(responses: Vec<(Value, Duration)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = Url::parse(&format!(
            "http://{}/api/v1/",
            listener.local_addr().unwrap()
        ))
        .unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let captured = requests.clone();
        let shutdown = stop.clone();
        let worker = thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            for (body, delay) in responses {
                let mut socket = loop {
                    if shutdown.load(Ordering::SeqCst) || std::time::Instant::now() > deadline {
                        return;
                    }
                    match listener.accept() {
                        Ok((socket, _)) => break socket,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2))
                        }
                        Err(e) => panic!("collection fixture accept: {e}"),
                    }
                };
                socket.set_nonblocking(false).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                socket
                    .set_write_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut request = Vec::new();
                let header_end = loop {
                    let mut bytes = [0; 1024];
                    let count = socket.read(&mut bytes).unwrap();
                    assert_ne!(count, 0, "collection fixture incomplete request");
                    request.extend_from_slice(&bytes[..count]);
                    assert!(request.len() < 64 * 1024);
                    if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                        break end + 4;
                    }
                };
                let headers = std::str::from_utf8(&request[..header_end]).unwrap();
                let length: usize = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse().unwrap())
                    })
                    .unwrap_or(0);
                assert!(length < 32 * 1024);
                while request.len() < header_end + length {
                    let mut bytes = [0; 1024];
                    let count = socket.read(&mut bytes).unwrap();
                    assert_ne!(count, 0);
                    request.extend_from_slice(&bytes[..count]);
                }
                captured
                    .lock()
                    .unwrap()
                    .push(String::from_utf8(request).unwrap());
                let ready = std::time::Instant::now() + delay;
                while std::time::Instant::now() < ready {
                    if shutdown.load(Ordering::SeqCst) {
                        return;
                    }
                    thread::sleep(Duration::from_millis(2));
                }
                let body = serde_json::to_vec(&body).unwrap();
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = socket.write_all(header.as_bytes());
                let _ = socket.write_all(&body);
            }
        });
        Self {
            base,
            requests,
            stop,
            worker: Some(worker),
        }
    }

    fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.worker.take().unwrap().join().unwrap();
    }
}

fn input(path: &str) -> collections::CollectionInput {
    collections::input(&Url::parse(&format!("https://www.instagram.com/{path}")).unwrap()).unwrap()
}

fn profile() -> Value {
    json!({"status":"ok","data":{"user":{"id":"42","username":"fixture_user","is_private":false}}})
}

fn post(id: &str, code: &str, owner: &str) -> Value {
    json!({"pk":id,"code":code,"media_type":1,"taken_at":1700000000,
        "user":{"pk":owner,"username":format!("owner_{owner}")},
        "image_versions2":{"candidates":[{"url":format!("https://cdn.example/{id}.jpg"),"width":100,"height":100}]}})
}

fn page(posts: Vec<Value>, cursor: Option<&str>) -> Value {
    let mut page = json!({"status":"ok","items":posts,"more_available":cursor.is_some()});
    if let Some(cursor) = cursor {
        page["next_max_id"] = json!(cursor);
    }
    page
}

fn reels(posts: Vec<Value>, cursor: Option<&str>) -> Value {
    let mut page = json!({"status":"ok","items":posts.into_iter().map(|media| json!({"media":media})).collect::<Vec<_>>(),
        "paging_info":{"more_available":cursor.is_some()}});
    if let Some(cursor) = cursor {
        page["paging_info"]["max_id"] = json!(cursor);
    }
    page
}

fn wrapped(posts: Vec<Value>, cursor: Option<&str>) -> Value {
    page(
        posts
            .into_iter()
            .map(|media| json!({"media":media}))
            .collect(),
        cursor,
    )
}

fn request_url(request: &str) -> Url {
    Url::parse(&format!(
        "http://localhost{}",
        request.split_whitespace().nth(1).unwrap()
    ))
    .unwrap()
}

#[tokio::test]
async fn collection_posts_resolve_identity_and_encode_query_cursor() {
    let cursor = "opaque+a&b=c /?%#";
    let server = Server::new(vec![
        profile(),
        page(vec![post("64", "BA", "42")], Some(cursor)),
        page(vec![post("65", "BB", "42")], None),
    ]);
    let instagram = Instagram::new(None, Duration::from_secs(3)).await.unwrap();
    let media: Vec<_> = instagram
        .collection_stream(input("fixture_user/posts/"), server.base.clone())
        .try_collect()
        .await
        .unwrap();
    assert_eq!(
        media
            .iter()
            .map(|item| item.metadata.as_ref().unwrap().post_id.as_str())
            .collect::<Vec<_>>(),
        ["64", "65"]
    );
    let requests = server.requests.lock().unwrap();
    assert_eq!(
        request_url(&requests[0]).path(),
        "/api/v1/users/web_profile_info/"
    );
    assert_eq!(
        request_url(&requests[0]).query(),
        Some("username=fixture_user")
    );
    assert_eq!(request_url(&requests[1]).path(), "/api/v1/feed/user/42/");
    assert_eq!(request_url(&requests[1]).query(), Some("count=30"));
    assert_eq!(
        request_url(&requests[2]).query_pairs().collect::<Vec<_>>(),
        [
            ("count".into(), "30".into()),
            ("max_id".into(), cursor.into())
        ]
    );
    assert!(requests.iter().all(|request| request.starts_with("GET ")));
}

#[tokio::test]
async fn reels_use_encoded_post_forms_and_accept_collaborative_owner() {
    let cursor = "opaque+a&b=c /?%#";
    let mut collaborative = post("65", "BB", "99");
    collaborative["coauthor_producers"] = json!([{"pk":"42","username":"fixture_user"}]);
    let server = Server::new(vec![
        reels(vec![post("64", "BA", "42")], Some(cursor)),
        reels(vec![collaborative], None),
    ]);
    let instagram = Instagram::new(None, Duration::from_secs(3)).await.unwrap();
    let media: Vec<_> = instagram
        .collection_stream(input("id:42/reels/"), server.base.clone())
        .try_collect()
        .await
        .unwrap();
    assert_eq!(media.len(), 2);
    assert_eq!(
        media[1].metadata.as_ref().unwrap().username.as_deref(),
        Some("owner_99")
    );
    let requests = server.requests.lock().unwrap();
    for (index, request) in requests.iter().enumerate() {
        assert!(request.starts_with("POST "));
        assert_eq!(request_url(request).path(), "/api/v1/clips/user/");
        let body = request.split_once("\r\n\r\n").unwrap().1;
        let form: std::collections::BTreeMap<_, _> = url::form_urlencoded::parse(body.as_bytes())
            .into_owned()
            .collect();
        assert_eq!(form.get("target_user_id").map(String::as_str), Some("42"));
        assert_eq!(form.get("page_size").map(String::as_str), Some("50"));
        assert_eq!(
            form.get("include_feed_video").map(String::as_str),
            Some("true")
        );
        assert_eq!(
            form.get("max_id").map(String::as_str),
            (index == 1).then_some(cursor)
        );
    }
}

#[tokio::test]
async fn tagged_and_saved_preserve_actual_owners_and_archive_identity() {
    for (route, path, count, wrap) in [
        ("id:42/tagged/", "/api/v1/usertags/42/feed/", "20", false),
        (
            "fixture_user/saved/",
            "/api/v1/feed/saved/posts/",
            "50",
            true,
        ),
        (
            "fixture_user/saved/all-posts/",
            "/api/v1/feed/saved/posts/",
            "50",
            true,
        ),
        (
            "fixture_user/saved/collection_name/123/",
            "/api/v1/feed/collection/123/posts/",
            "50",
            true,
        ),
    ] {
        let posts = vec![post("64", "BA", "99"), post("65", "BB", "100")];
        let response = if wrap {
            wrapped(posts, None)
        } else {
            page(posts, None)
        };
        let server = Server::new(vec![response]);
        let instagram = Instagram::new(None, Duration::from_secs(3)).await.unwrap();
        let media: Vec<_> = instagram
            .collection_stream(input(route), server.base.clone())
            .try_collect()
            .await
            .unwrap();
        assert_eq!(
            media
                .iter()
                .map(|item| item.metadata.as_ref().unwrap().post_id.as_str())
                .collect::<Vec<_>>(),
            ["64", "65"]
        );
        assert_eq!(
            media[0].metadata.as_ref().unwrap().username.as_deref(),
            Some("owner_99")
        );
        let standalone = parse_post(&post("64", "BA", "99"), "BA").unwrap();
        assert_eq!(media[0].archive_key(), standalone[0].archive_key());
        let requests = server.requests.lock().unwrap();
        assert_eq!(
            requests.len(),
            1,
            "saved and numeric IDs must not resolve a username"
        );
        assert_eq!(request_url(&requests[0]).path(), path);
        assert_eq!(
            request_url(&requests[0]).query(),
            Some(format!("count={count}").as_str())
        );
    }
}

#[tokio::test]
async fn collection_stream_is_lazy_and_range_cutoff_prevents_next_page() {
    let server = Server::new(vec![wrapped(
        vec![post("64", "BA", "99"), post("65", "BB", "100")],
        Some("later"),
    )]);
    let instagram = Instagram::new(None, Duration::from_secs(3)).await.unwrap();
    let stream = instagram.collection_stream(input("fixture_user/saved/"), server.base.clone());
    tokio::time::sleep(Duration::from_millis(25)).await;
    assert_eq!(server.count(), 0);
    let mut selected = crate::selection::RangeSelection::parse("1")
        .unwrap()
        .apply(stream);
    assert_eq!(
        selected
            .next()
            .await
            .unwrap()
            .unwrap()
            .metadata
            .unwrap()
            .post_id,
        "64"
    );
    tokio::time::sleep(Duration::from_millis(25)).await;
    assert_eq!(server.count(), 1);
    assert!(selected.next().await.is_none());
    assert_eq!(server.count(), 1);
}

#[tokio::test]
async fn collection_backpressure_keeps_current_page_without_prefetch() {
    let server = Server::new(vec![
        page(
            vec![post("64", "BA", "99"), post("65", "BB", "100")],
            Some("later"),
        ),
        page(vec![], None),
    ]);
    let instagram = Instagram::new(None, Duration::from_secs(3)).await.unwrap();
    let mut stream = instagram.collection_stream(input("id:42/tagged/"), server.base.clone());
    assert!(stream.next().await.unwrap().is_ok());
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(server.count(), 1);
    assert!(stream.next().await.unwrap().is_ok());
    assert_eq!(server.count(), 1);
    drop(stream);
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(server.count(), 1);
}

#[tokio::test]
async fn unordered_date_filter_keeps_recent_saved_items_after_old_items() {
    let mut old = post("64", "BA", "99");
    old["taken_at"] = json!(10);
    let mut recent = post("65", "BB", "100");
    recent["taken_at"] = json!(30);
    let server = Server::new(vec![
        wrapped(vec![old], Some("later")),
        wrapped(vec![recent], None),
    ]);
    let instagram = Instagram::new(None, Duration::from_secs(3)).await.unwrap();
    let media: Vec<_> = crate::date::DateFilter::new(None, Some(20))
        .unwrap()
        .apply_unordered(
            instagram.collection_stream(input("fixture_user/saved/"), server.base.clone()),
        )
        .try_collect()
        .await
        .unwrap();
    assert_eq!(media.len(), 1);
    assert_eq!(media[0].metadata.as_ref().unwrap().post_id, "65");
    assert_eq!(server.count(), 2);
}

#[tokio::test]
async fn invalid_late_cursor_is_page_atomic_and_preserves_prior_media() {
    for cursor in [None, Some("same")] {
        let mut invalid = wrapped(vec![post("65", "BB", "100")], cursor);
        invalid["more_available"] = json!(true);
        let server = Server::new(vec![
            wrapped(vec![post("64", "BA", "99")], Some("same")),
            invalid,
        ]);
        let instagram = Instagram::new(None, Duration::from_secs(3)).await.unwrap();
        let mut stream =
            instagram.collection_stream(input("fixture_user/saved/"), server.base.clone());
        assert_eq!(
            stream
                .next()
                .await
                .unwrap()
                .unwrap()
                .metadata
                .unwrap()
                .post_id,
            "64"
        );
        let error = stream.next().await.unwrap().unwrap_err().to_string();
        assert!(error.contains("cursor"), "{error}");
        assert!(stream.next().await.is_none());
        assert_eq!(server.count(), 2);
    }
}

#[tokio::test]
async fn collections_reject_foreign_profile_resolution_and_reel_owners() {
    let mut foreign = profile();
    foreign["data"]["user"]["username"] = json!("someone_else");
    let server = Server::new(vec![foreign]);
    let instagram = Instagram::new(None, Duration::from_secs(3)).await.unwrap();
    let mut stream =
        instagram.collection_stream(input("fixture_user/tagged/"), server.base.clone());
    assert!(stream.next().await.unwrap().is_err());
    assert!(stream.next().await.is_none());
    assert_eq!(server.count(), 1);
    let server = Server::new(vec![reels(vec![post("64", "BA", "99")], None)]);
    let mut stream = instagram.collection_stream(input("id:42/reels/"), server.base.clone());
    assert!(stream.next().await.unwrap().is_err());
    assert!(stream.next().await.is_none());
    assert_eq!(server.count(), 1);
}

#[tokio::test]
async fn empty_collections_complete_without_media() {
    for (route, response) in [
        ("id:42/posts/", page(vec![], None)),
        ("id:42/reels/", reels(vec![], None)),
        ("fixture_user/saved/", wrapped(vec![], None)),
    ] {
        let server = Server::new(vec![response]);
        let instagram = Instagram::new(None, Duration::from_secs(3)).await.unwrap();
        let mut stream = instagram.collection_stream(input(route), server.base.clone());
        assert!(stream.next().await.is_none());
        assert_eq!(server.count(), 1);
    }
}

#[tokio::test]
async fn collection_deadline_accumulates_requests_but_excludes_consumer_pause() {
    let server = Server::new(vec![
        wrapped(vec![post("64", "BA", "99")], Some("later")),
        wrapped(vec![post("65", "BB", "100")], None),
    ]);
    let instagram = Instagram::new(None, Duration::from_secs(1)).await.unwrap();
    let mut stream = instagram.collection_stream(input("fixture_user/saved/"), server.base.clone());
    assert!(stream.next().await.unwrap().is_ok());
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(stream.next().await.unwrap().is_ok());
    assert!(stream.next().await.is_none());
    let server = Server::delayed(vec![
        (
            wrapped(vec![post("64", "BA", "99")], Some("later")),
            Duration::from_millis(350),
        ),
        (
            wrapped(vec![post("65", "BB", "100")], None),
            Duration::from_millis(800),
        ),
    ]);
    let mut stream = instagram.collection_stream(input("fixture_user/saved/"), server.base.clone());
    assert!(stream.next().await.unwrap().is_ok());
    let error = stream.next().await.unwrap().unwrap_err().to_string();
    assert!(error.contains("timed out"), "{error}");
    assert!(stream.next().await.is_none());
    assert_eq!(server.count(), 2);
}

#[tokio::test]
async fn dropping_pending_collection_request_prevents_next_page() {
    let server = Server::delayed(vec![
        (
            wrapped(vec![post("64", "BA", "99")], Some("later")),
            Duration::from_secs(2),
        ),
        (wrapped(vec![], None), Duration::ZERO),
    ]);
    let instagram = Instagram::new(None, Duration::from_secs(3)).await.unwrap();
    let mut stream = instagram.collection_stream(input("fixture_user/saved/"), server.base.clone());
    assert!(
        tokio::time::timeout(Duration::from_millis(80), stream.next())
            .await
            .is_err()
    );
    assert_eq!(server.count(), 1);
    drop(stream);
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(server.count(), 1);
}

#[tokio::test]
async fn collection_events_use_each_parsed_post_boundary_instead_of_media_id_changes() {
    // Repeated source posts remain separate upstream directories even when their
    // stable file IDs happen to match. Selection must not infer boundaries.
    let server = Server::new(vec![wrapped(
        vec![post("64", "BA", "42"), post("64", "BA", "42")],
        None,
    )]);
    let native = Instagram::new(None, Duration::from_secs(3)).await.unwrap();
    let events: Vec<_> = native
        .collection_events(input("fixture/saved/"), server.base.clone())
        .try_collect()
        .await
        .unwrap();
    assert_eq!(events.len(), 6);
    for chunk in events.chunks(3) {
        let ExtractionEvent::Post(post) = &chunk[0] else {
            panic!("expected parsed post boundary")
        };
        assert_eq!(post.count, Some(1));
        assert_eq!(post.metadata.as_ref().unwrap().post_id, "64");
        assert!(matches!(chunk[1], ExtractionEvent::Media(_)));
        assert!(matches!(chunk[2], ExtractionEvent::EndPost));
    }
    assert_eq!(server.count(), 1);
}
