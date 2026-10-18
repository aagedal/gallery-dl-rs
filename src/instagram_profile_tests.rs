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
    thread: Option<thread::JoinHandle<()>>,
}

impl Server {
    fn new(responses: Vec<Value>) -> Self {
        Self::delayed(
            responses
                .into_iter()
                .map(|body| (body, Duration::ZERO))
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
        let server_requests = Arc::clone(&requests);
        let server_stop = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            for (body, delay) in responses {
                let mut socket = loop {
                    if server_stop.load(Ordering::Relaxed) {
                        return;
                    }
                    assert!(
                        std::time::Instant::now() < deadline,
                        "profile fixture server deadline"
                    );
                    match listener.accept() {
                        Ok((socket, _)) => break socket,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2))
                        }
                        Err(e) => panic!("profile fixture accept: {e}"),
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
                while !request.ends_with(b"\r\n\r\n") {
                    let mut bytes = [0; 1024];
                    let count = socket.read(&mut bytes).unwrap();
                    if count == 0 {
                        break;
                    }
                    request.extend_from_slice(&bytes[..count]);
                    assert!(request.len() < 64 * 1024);
                }
                server_requests
                    .lock()
                    .unwrap()
                    .push(String::from_utf8(request).unwrap());
                let response_at = std::time::Instant::now() + delay;
                while std::time::Instant::now() < response_at {
                    if server_stop.load(Ordering::Relaxed) {
                        return;
                    }
                    thread::sleep(Duration::from_millis(2));
                }
                let body = serde_json::to_vec(&body).unwrap();
                let headers = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = socket.write_all(headers.as_bytes());
                let _ = socket.write_all(&body);
            }
        });
        Self {
            base,
            requests,
            stop,
            thread: Some(thread),
        }
    }

    fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap();
    }
}

fn profile() -> Value {
    json!({"status":"ok", "data":{"user":{"id":"42", "username":"fixture_user", "is_private":false}}})
}

fn post(id: &str, code: &str) -> Value {
    let mut value: Value =
        serde_json::from_slice(include_bytes!("../tests/fixtures/instagram/photo.json")).unwrap();
    let mut post = value["items"][0].take();
    post["pk"] = json!(id);
    post["code"] = json!(code);
    post["user"]["pk"] = json!("42");
    post
}

fn page(posts: Vec<Value>, cursor: Option<&str>) -> Value {
    let mut value = json!({"status":"ok", "items":posts, "more_available":cursor.is_some()});
    if let Some(cursor) = cursor {
        value["next_max_id"] = json!(cursor);
    }
    value
}

#[tokio::test]
async fn profile_pages_preserve_order_and_encode_opaque_cursors() {
    let cursor = "opaque+/=&? space%#";
    let server = Server::new(vec![
        profile(),
        page(vec![post("64", "BA"), post("65", "BB")], Some(cursor)),
        page(vec![post("66", "BC")], None),
    ]);
    let instagram = Instagram::new(None, Duration::from_secs(3)).await.unwrap();
    let media: Vec<_> = instagram
        .profile_stream("fixture_user".into(), server.base.clone())
        .try_collect()
        .await
        .unwrap();
    assert_eq!(
        media
            .iter()
            .map(|m| m.metadata.as_ref().unwrap().post_id.as_str())
            .collect::<Vec<_>>(),
        ["64", "65", "66"]
    );
    let requests = server.requests.lock().unwrap();
    let urls: Vec<_> = requests
        .iter()
        .map(|request| {
            Url::parse(&format!(
                "http://localhost{}",
                request.split_whitespace().nth(1).unwrap()
            ))
            .unwrap()
        })
        .collect();
    assert_eq!(urls[0].path(), "/api/v1/users/web_profile_info/");
    assert_eq!(
        urls[0].query_pairs().collect::<Vec<_>>(),
        [("username".into(), "fixture_user".into())]
    );
    assert_eq!(urls[1].path(), "/api/v1/feed/user/42/");
    assert_eq!(urls[1].query(), Some("count=30"));
    assert_eq!(
        urls[2].query_pairs().collect::<Vec<_>>(),
        [
            ("count".into(), "30".into()),
            ("max_id".into(), cursor.into())
        ]
    );
}

#[tokio::test]
async fn profile_stream_is_lazy_and_does_not_prefetch_during_backpressure() {
    let server = Server::new(vec![
        profile(),
        page(vec![post("64", "BA"), post("65", "BB")], Some("next")),
        page(vec![post("66", "BC")], None),
    ]);
    let instagram = Instagram::new(None, Duration::from_secs(3)).await.unwrap();
    let mut stream = instagram.profile_stream("fixture_user".into(), server.base.clone());
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(server.count(), 0);
    assert!(stream.next().await.unwrap().is_ok());
    assert_eq!(server.count(), 2);
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(server.count(), 2);
    assert!(stream.next().await.unwrap().is_ok());
    assert_eq!(server.count(), 2);
    drop(stream);
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(server.count(), 2);
}

#[tokio::test]
async fn profile_late_cursor_errors_retain_previous_pages_and_terminate() {
    for cursor in [None, Some("same")] {
        let mut invalid = page(vec![post("65", "BB")], cursor);
        invalid["more_available"] = json!(true);
        let server = Server::new(vec![
            profile(),
            page(vec![post("64", "BA")], Some("same")),
            invalid,
        ]);
        let instagram = Instagram::new(None, Duration::from_secs(3)).await.unwrap();
        let mut stream = instagram.profile_stream("fixture_user".into(), server.base.clone());
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
        assert!(
            stream
                .next()
                .await
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("cursor")
        );
        assert!(stream.next().await.is_none());
        assert_eq!(server.count(), 3);
    }
}

#[tokio::test]
async fn profile_rejects_mismatched_resolution_and_private_accounts() {
    let mut wrong = profile();
    wrong["data"]["user"]["username"] = json!("other_user");
    let mut private = profile();
    private["data"]["user"]["is_private"] = json!(true);
    for (response, message) in [
        (wrong, "requested profile"),
        (private, "authorized session"),
    ] {
        let server = Server::new(vec![response]);
        let instagram = Instagram::new(None, Duration::from_secs(3)).await.unwrap();
        let mut stream = instagram.profile_stream("fixture_user".into(), server.base.clone());
        assert!(
            stream
                .next()
                .await
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains(message)
        );
        assert!(stream.next().await.is_none());
        assert_eq!(server.count(), 1);
    }
}

#[tokio::test]
async fn profile_rejects_posts_owned_by_a_different_account() {
    let mut foreign = post("64", "BA");
    foreign["user"]["pk"] = json!("99");
    let server = Server::new(vec![profile(), page(vec![foreign], None)]);
    let instagram = Instagram::new(None, Duration::from_secs(3)).await.unwrap();
    let mut stream = instagram.profile_stream("fixture_user".into(), server.base.clone());
    assert!(
        stream
            .next()
            .await
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("requested profile")
    );
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn profile_consumer_pause_does_not_consume_extraction_budget() {
    let server = Server::new(vec![
        profile(),
        page(vec![post("64", "BA")], Some("next")),
        page(vec![post("65", "BB")], None),
    ]);
    let instagram = Instagram::new(None, Duration::from_secs(1)).await.unwrap();
    let mut stream = instagram.profile_stream("fixture_user".into(), server.base.clone());
    assert!(stream.next().await.unwrap().is_ok());
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(stream.next().await.unwrap().is_ok());
    assert!(stream.next().await.is_none());
}

#[test]
fn profile_urls_validate_hosts_routes_and_usernames() {
    for url in [
        "https://instagram.com/fixture_user/",
        "https://www.instagram.com/fixture_user/feed/",
        "https://m.instagram.com/fixture.user/?ignored=yes",
    ] {
        assert!(profile_username(&Url::parse(url).unwrap()).is_ok(), "{url}");
    }
    for url in [
        "https://instagram.com/",
        "https://instagram.com/p/BA/",
        "https://instagram.com/accounts/",
        "https://instagram.com/explore/",
        "https://instagram.com/user/reels/",
        "https://instagram.com/user/feed/extra",
        "https://instagram.com/user%2Fother/",
        "https://instagram.com/user-name/",
        "https://evil.example/user/",
        "https://instagram.com.evil.example/user/",
        "https://user:pass@instagram.com/user/",
        "https://instagram.com:8443/user/",
        "ftp://instagram.com/user/",
        "https://instagram.com/abcdefghijklmnopqrstuvwxyz12345/",
    ] {
        assert!(
            profile_username(&Url::parse(url).unwrap()).is_err(),
            "{url}"
        );
    }
}

#[tokio::test]
async fn empty_profile_completes_without_media() {
    let server = Server::new(vec![profile(), page(vec![], None)]);
    let instagram = Instagram::new(None, Duration::from_secs(3)).await.unwrap();
    let mut stream = instagram.profile_stream("fixture_user".into(), server.base.clone());
    assert!(stream.next().await.is_none());
    assert_eq!(server.count(), 2);
}

#[tokio::test]
async fn profile_active_budget_accumulates_across_pages() {
    let server = Server::delayed(vec![
        (profile(), Duration::from_millis(200)),
        (
            page(vec![post("64", "BA")], Some("next")),
            Duration::from_millis(200),
        ),
        (
            page(vec![post("65", "BB")], None),
            Duration::from_millis(800),
        ),
    ]);
    let instagram = Instagram::new(None, Duration::from_secs(1)).await.unwrap();
    let mut stream = instagram.profile_stream("fixture_user".into(), server.base.clone());
    assert!(stream.next().await.unwrap().is_ok());
    let error = stream.next().await.unwrap().unwrap_err().to_string();
    assert!(error.contains("timed out"), "{error}");
    assert!(stream.next().await.is_none());
    assert_eq!(server.count(), 3);
}

#[tokio::test]
async fn dropping_pending_profile_request_does_not_start_feed() {
    let server = Server::delayed(vec![
        (profile(), Duration::from_secs(2)),
        (page(vec![post("64", "BA")], None), Duration::ZERO),
    ]);
    let instagram = Instagram::new(None, Duration::from_secs(3)).await.unwrap();
    let mut stream = instagram.profile_stream("fixture_user".into(), server.base.clone());
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
async fn file_range_stops_native_collection_without_requesting_later_pages() {
    let server = Server::new(vec![
        profile(),
        page(vec![post("64", "BA"), post("65", "BB")], Some("next")),
    ]);
    let instagram = Instagram::new(None, Duration::from_secs(3)).await.unwrap();
    let mut selected = crate::selection::RangeSelection::parse("1")
        .unwrap()
        .apply(instagram.profile_stream("fixture_user".into(), server.base.clone()));
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
    assert!(selected.next().await.is_none());
    assert_eq!(server.count(), 2);

    let server = Server::new(vec![profile()]);
    let mut empty = crate::selection::RangeSelection::parse("0")
        .unwrap()
        .apply(instagram.profile_stream("fixture_user".into(), server.base.clone()));
    assert!(empty.next().await.is_none());
    assert_eq!(server.count(), 0);
}

#[tokio::test]
async fn date_after_stops_native_profile_without_following_older_cursor() {
    let mut recent = post("64", "BA");
    recent["taken_at"] = json!(1700000001);
    let old = post("65", "BB");
    let server = Server::new(vec![profile(), page(vec![recent, old], Some("older-page"))]);
    let instagram = Instagram::new(None, Duration::from_secs(3)).await.unwrap();
    let dates = crate::date::DateFilter::new(None, Some(1700000000)).unwrap();
    let items: Vec<_> = dates
        .apply(instagram.profile_stream("fixture_user".into(), server.base.clone()))
        .try_collect()
        .await
        .unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].metadata.as_ref().unwrap().post_id, "64");
    assert_eq!(server.count(), 2);
}

#[tokio::test]
async fn profile_events_preserve_carousel_boundaries_and_file_identity() {
    let mut carousel: Value =
        serde_json::from_slice(include_bytes!("../tests/fixtures/instagram/carousel.json"))
            .unwrap();
    let mut carousel = carousel["items"][0].take();
    carousel["user"]["pk"] = "42".into();
    let server = Server::new(vec![
        profile(),
        page(vec![carousel.clone(), post("65", "BB")], None),
    ]);
    let native = Instagram::new(None, Duration::from_secs(3)).await.unwrap();
    let events: Vec<_> = native
        .profile_events("fixture_user".into(), server.base.clone())
        .try_collect()
        .await
        .unwrap();
    assert_eq!(events.len(), 8);
    let ExtractionEvent::Post(first) = &events[0] else {
        panic!("expected explicit carousel post")
    };
    assert_eq!(first.count, Some(3));
    let metadata = first.metadata.as_ref().unwrap();
    assert_eq!(metadata.post_id, "64");
    assert_eq!(metadata.username.as_deref(), Some("fixture_user"));
    assert!(metadata.file.is_none());
    assert!(metadata.media_id.is_empty());
    assert_eq!(metadata.index, 0);
    assert!(matches!(events[4], ExtractionEvent::EndPost));
    let ExtractionEvent::Post(second) = &events[5] else {
        panic!("expected next post boundary")
    };
    assert_eq!(second.count, Some(1));
    assert!(matches!(events[7], ExtractionEvent::EndPost));
    let expected = parse_post(&carousel, "BA").unwrap();
    for (event, expected) in events[1..4].iter().zip(expected) {
        let ExtractionEvent::Media(media) = event else {
            panic!("expected carousel file")
        };
        assert_eq!(media.identity(), expected.identity());
        assert_eq!(media.archive_key(), expected.archive_key());
    }
}

#[tokio::test]
async fn profile_events_are_lazy_and_dropping_after_post_avoids_next_page() {
    let server = Server::new(vec![
        profile(),
        page(vec![post("64", "BA")], Some("unused")),
        page(vec![post("65", "BB")], None),
    ]);
    let native = Instagram::new(None, Duration::from_secs(3)).await.unwrap();
    let mut events = native.profile_events("fixture_user".into(), server.base.clone());
    assert_eq!(server.count(), 0);
    assert!(matches!(
        events.next().await.unwrap().unwrap(),
        ExtractionEvent::Post(_)
    ));
    assert_eq!(server.count(), 2);
    drop(events);
    tokio::time::sleep(Duration::from_millis(15)).await;
    assert_eq!(server.count(), 2);
}
