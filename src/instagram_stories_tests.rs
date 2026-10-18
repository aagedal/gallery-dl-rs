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

struct Response {
    status: u16,
    body: Vec<u8>,
    delay: Duration,
}
impl Response {
    fn json(body: Value) -> Self {
        Self {
            status: 200,
            body: serde_json::to_vec(&body).unwrap(),
            delay: Duration::ZERO,
        }
    }
    fn delayed(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }
}
struct Server {
    base: Url,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Server {
    fn new(responses: Vec<Response>) -> Self {
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
            for response in responses {
                let mut socket = loop {
                    if shutdown.load(Ordering::SeqCst) || std::time::Instant::now() > deadline {
                        return;
                    }
                    match listener.accept() {
                        Ok((socket, _)) => break socket,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2))
                        }
                        Err(e) => panic!("accept: {e}"),
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
                while !request.ends_with(b"\r\n\r\n") && request.len() < 32768 {
                    let mut byte = [0];
                    match socket.read(&mut byte) {
                        Ok(1) => request.push(byte[0]),
                        _ => break,
                    }
                }
                captured
                    .lock()
                    .unwrap()
                    .push(String::from_utf8(request).unwrap());
                let ready = std::time::Instant::now() + response.delay;
                while std::time::Instant::now() < ready {
                    if shutdown.load(Ordering::SeqCst) {
                        return;
                    }
                    thread::sleep(Duration::from_millis(2));
                }
                let header = format!(
                    "HTTP/1.1 {} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    response.status,
                    response.body.len()
                );
                let _ = socket.write_all(header.as_bytes());
                let _ = socket.write_all(&response.body);
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
    async fn wait_for(&self, count: usize) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while self.count() < count {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.worker.take().unwrap().join().unwrap();
    }
}

fn image(id: &str) -> Value {
    json!({"pk":id,"media_type":1,"taken_at":1700000000,
        "image_versions2":{"candidates":[{"width":640,"height":480,
            "url":format!("https://cdn.example/{id}.jpg")} ]}})
}
fn video(id: &str) -> Value {
    json!({"pk":id,"media_type":2,"taken_at":1700000001,
        "video_versions":[{"width":1280,"height":720,
            "url":format!("https://cdn.example/{id}.mp4")} ]})
}
fn reel(id: &str, owner: &str, items: Vec<Value>) -> Value {
    json!({"status":"ok","reels_media":[{"id":id,
        "user":{"pk":owner,"username":"fixture"},"items":items}]})
}
fn profile() -> Value {
    json!({"status":"ok","data":{"user":{"id":"123","username":"fixture",
        "is_private":false,"followed_by_viewer":false}}})
}
fn input(path: &str) -> stories::StoryInput {
    stories::input(&Url::parse(&format!("https://www.instagram.com{path}")).unwrap()).unwrap()
}
fn extractor(timeout: Duration) -> Instagram {
    Instagram::from_cookie_jar(CookieJar::parse("").unwrap(), timeout).unwrap()
}
fn request_target(server: &Server, index: usize) -> Url {
    let requests = server.requests.lock().unwrap();
    let target = requests[index]
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap();
    server.base.join(target).unwrap()
}

#[tokio::test]
async fn stories_resolve_username_and_keep_photo_video_order_and_source_indices() {
    let server = Server::new(vec![
        Response::json(profile()),
        Response::json(reel("123", "123", vec![image("101"), video("102")])),
    ]);
    let native = extractor(Duration::from_secs(3));
    let items: Vec<_> = native
        .story_stream(input("/stories/fixture/"), server.base.clone())
        .try_collect()
        .await
        .unwrap();
    assert_eq!(items.len(), 2);
    for (index, item) in items.iter().enumerate() {
        let metadata = item.metadata.as_ref().unwrap();
        assert_eq!(metadata.post_id, (101 + index).to_string());
        assert_eq!(metadata.media_id, metadata.post_id);
        assert_eq!(metadata.username.as_deref(), Some("fixture"));
        assert_eq!(metadata.index, index + 1);
        assert!(item.archive_key().is_some());
    }
    assert!(items[0].url.path().ends_with("101.jpg"));
    assert!(items[1].url.path().ends_with("102.mp4"));
    let profile = request_target(&server, 0);
    assert_eq!(profile.path(), "/api/v1/users/web_profile_info/");
    assert_eq!(
        profile.query_pairs().collect::<Vec<_>>(),
        vec![("username".into(), "fixture".into())]
    );
    let reel = request_target(&server, 1);
    assert_eq!(reel.path(), "/api/v1/feed/reels_media/");
    assert_eq!(
        reel.query_pairs().collect::<Vec<_>>(),
        vec![("reel_ids".into(), "123".into())]
    );
}

#[tokio::test]
async fn exact_story_selection_skips_unselected_malformed_media_and_preserves_identity() {
    let response = reel(
        "123",
        "123",
        vec![json!({"pk":"100","media_type":999}), image("101")],
    );
    let server = Server::new(vec![
        Response::json(response),
        Response::json(reel("highlight:12", "123", vec![image("101")])),
    ]);
    let native = extractor(Duration::from_secs(3));
    let direct: Vec<_> = native
        .story_stream(input("/stories/id:123/101/"), server.base.clone())
        .try_collect()
        .await
        .unwrap();
    assert_eq!(direct.len(), 1);
    assert_eq!(direct[0].metadata.as_ref().unwrap().index, 2);
    let highlights: Vec<_> = native
        .story_stream(input("/stories/highlights/12/"), server.base.clone())
        .try_collect()
        .await
        .unwrap();
    assert_eq!(direct[0].archive_key(), highlights[0].archive_key());
    let endpoint = request_target(&server, 1);
    assert_eq!(
        endpoint.query_pairs().collect::<Vec<_>>(),
        vec![("reel_ids".into(), "highlight:12".into())]
    );
    assert!(endpoint.query().unwrap().contains("%3A"));
}

#[tokio::test]
async fn empty_story_reels_are_successful_but_selected_missing_story_is_an_error() {
    for body in [
        json!({"status":"ok","reels_media":[]}),
        reel("123", "123", vec![]),
    ] {
        let server = Server::new(vec![Response::json(body.clone()), Response::json(body)]);
        let native = extractor(Duration::from_secs(3));
        let items: Vec<_> = native
            .story_stream(input("/stories/id:123/"), server.base.clone())
            .try_collect()
            .await
            .unwrap();
        assert!(items.is_empty());
        let mut selected = native.story_stream(input("/stories/id:123/101/"), server.base.clone());
        assert!(selected.next().await.unwrap().is_err());
        assert!(selected.next().await.is_none());
    }
}

#[tokio::test]
async fn foreign_reels_owners_and_private_profiles_fail_without_media() {
    let mut wrong_item = image("101");
    wrong_item["user"] = json!({"pk":"999","username":"other"});
    for body in [
        reel("999", "123", vec![image("101")]),
        reel("123", "999", vec![image("101")]),
        reel("123", "123", vec![wrong_item]),
        json!({"status":"fail","message":"login_required"}),
    ] {
        let server = Server::new(vec![Response::json(body)]);
        let native = extractor(Duration::from_secs(3));
        let mut stream = native.story_stream(input("/stories/id:123/"), server.base.clone());
        assert!(stream.next().await.unwrap().is_err());
        assert!(stream.next().await.is_none());
        assert_eq!(server.count(), 1);
    }
    let mut private = profile();
    private["data"]["user"]["is_private"] = json!(true);
    let server = Server::new(vec![
        Response::json(private.clone()),
        Response::json(json!({"status":"fail","message":"login_required"})),
    ]);
    let native = extractor(Duration::from_secs(3));
    let mut stream = native.story_stream(input("/stories/fixture/"), server.base.clone());
    assert!(stream.next().await.unwrap().is_err());
    assert!(stream.next().await.is_none());
    assert_eq!(server.count(), 2);
    // A private profile can be the viewer's own account. The reel API decides
    // session authorization; the public profile flag cannot establish denial.
    let server = Server::new(vec![
        Response::json(private),
        Response::json(reel("123", "123", vec![image("101")])),
    ]);
    let mut stream = native.story_stream(input("/stories/fixture/"), server.base.clone());
    assert!(stream.next().await.unwrap().is_ok());
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn highlight_trays_are_lazy_and_file_range_drops_before_next_reel() {
    let server = Server::new(vec![
        Response::json(profile()),
        Response::json(json!({"status":"ok","tray":[{"id":"highlight:12"},{"id":"highlight:13"}]})),
        Response::json(reel("highlight:12", "123", vec![image("101")])),
        Response::json(reel("highlight:13", "123", vec![image("102")])),
    ]);
    let native = extractor(Duration::from_secs(3));
    let selected = crate::selection::RangeSelection::parse("1").unwrap();
    let mut stream =
        selected.apply(native.story_stream(input("/fixture/highlights/"), server.base.clone()));
    assert_eq!(server.count(), 0);
    assert!(stream.next().await.unwrap().is_ok());
    assert_eq!(
        request_target(&server, 1).path(),
        "/api/v1/highlights/123/highlights_tray/"
    );
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(server.count(), 3);
    assert!(stream.next().await.is_none());
    assert_eq!(server.count(), 3);
}

#[tokio::test]
async fn story_tray_visits_reels_in_order_without_prefetch_and_retains_late_errors() {
    let server = Server::new(vec![
        Response::json(json!({"status":"ok","tray":[
            {"id":"123","user":{"pk":"123","username":"fixture"}},
            {"id":"456","user":{"pk":"456","username":"other"}}]})),
        Response::json(reel("123", "123", vec![image("101"), image("102")])),
        Response {
            status: 403,
            body: b"{}".to_vec(),
            delay: Duration::ZERO,
        },
    ]);
    let native = extractor(Duration::from_secs(3));
    let mut stream = native.story_stream(input("/stories/me/"), server.base.clone());
    assert_eq!(server.count(), 0);
    assert_eq!(
        stream
            .next()
            .await
            .unwrap()
            .unwrap()
            .metadata
            .unwrap()
            .post_id,
        "101"
    );
    assert_eq!(
        request_target(&server, 0).path(),
        "/api/v1/feed/reels_tray/"
    );
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(server.count(), 2);
    assert_eq!(
        stream
            .next()
            .await
            .unwrap()
            .unwrap()
            .metadata
            .unwrap()
            .post_id,
        "102"
    );
    assert_eq!(server.count(), 2);
    assert!(stream.next().await.unwrap().is_err());
    assert!(stream.next().await.is_none());
    assert_eq!(
        request_target(&server, 2)
            .query_pairs()
            .find(|(key, _)| key == "reel_ids")
            .unwrap()
            .1,
        "456"
    );
}

#[tokio::test]
async fn story_stream_active_deadline_excludes_consumer_pause() {
    let server = Server::new(vec![
        Response::json(profile()),
        Response::json(json!({"status":"ok","tray":[{"id":"highlight:12"},{"id":"highlight:13"}]})),
        Response::json(reel("highlight:12", "123", vec![image("101")]))
            .delayed(Duration::from_millis(15)),
        Response::json(reel("highlight:13", "123", vec![image("102")]))
            .delayed(Duration::from_millis(15)),
    ]);
    let native = extractor(Duration::from_millis(500));
    let mut stream = native.story_stream(input("/fixture/highlights/"), server.base.clone());
    assert!(stream.next().await.unwrap().is_ok());
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(stream.next().await.unwrap().is_ok());
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn story_reels_share_one_cumulative_active_budget() {
    let server = Server::new(vec![
        Response::json(json!({"status":"ok","tray":[{"id":"123"},{"id":"456"}]})),
        Response::json(reel("123", "123", vec![image("101")])).delayed(Duration::from_millis(300)),
        Response::json(reel("456", "456", vec![image("102")])).delayed(Duration::from_millis(300)),
    ]);
    let native = extractor(Duration::from_millis(500));
    let mut stream = native.story_stream(input("/stories/me/"), server.base.clone());
    assert!(stream.next().await.unwrap().is_ok());
    let error = stream.next().await.unwrap().unwrap_err().to_string();
    assert!(error.contains("timed out"), "{error}");
    assert!(stream.next().await.is_none());
    assert_eq!(server.count(), 3);
}

#[tokio::test]
async fn dropping_pending_story_request_prevents_remaining_tray_requests() {
    let server = Server::new(vec![
        Response::json(json!({"status":"ok","tray":[
            {"id":"123","user":{"pk":"123","username":"fixture"}},
            {"id":"456","user":{"pk":"456","username":"other"}}]})),
        Response::json(reel("123", "123", vec![image("101")])).delayed(Duration::from_secs(2)),
        Response::json(reel("456", "456", vec![image("102")])),
    ]);
    let native = extractor(Duration::from_secs(3));
    let mut stream = native.story_stream(input("/stories/me/"), server.base.clone());
    {
        let next = stream.next();
        tokio::pin!(next);
        tokio::select! {
            result = &mut next => panic!("request completed before cancellation: {result:?}"),
            _ = server.wait_for(2) => {}
        }
    }
    drop(stream);
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(server.count(), 2);
}

#[tokio::test]
async fn story_events_group_the_entire_reel_with_its_own_metadata() {
    let mut response = reel("123", "123", vec![image("101"), video("102")]);
    response["reels_media"][0]["seen"] = 444.into();
    let server = Server::new(vec![Response::json(response)]);
    let native = extractor(Duration::from_secs(3));
    let events: Vec<_> = native
        .story_events(input("/stories/id:123/"), server.base.clone())
        .try_collect()
        .await
        .unwrap();
    assert_eq!(events.len(), 4);
    let ExtractionEvent::Post(post) = &events[0] else {
        panic!("expected one reel directory")
    };
    assert_eq!(post.count, Some(2));
    let metadata = post.metadata.as_ref().unwrap();
    assert_eq!(metadata.post_id, "123");
    assert_eq!(metadata.username.as_deref(), Some("fixture"));
    assert_eq!(metadata.timestamp, Some(444));
    assert_eq!(metadata.index, 0);
    assert!(metadata.media_id.is_empty());
    assert!(metadata.file.is_none());
    for (event, id) in events[1..3].iter().zip(["101", "102"]) {
        let ExtractionEvent::Media(media) = event else {
            panic!("expected story file")
        };
        assert_eq!(media.metadata.as_ref().unwrap().media_id, id);
        assert_eq!(media.metadata.as_ref().unwrap().post_id, id);
        assert!(media.archive_key().is_some());
    }
    assert!(matches!(events[3], ExtractionEvent::EndPost));
    assert_eq!(server.count(), 1);
}

#[tokio::test]
async fn story_events_preserve_empty_reels_and_selected_item_boundaries() {
    {
        let response = reel("123", "123", vec![]);
        let server = Server::new(vec![Response::json(response)]);
        let native = extractor(Duration::from_secs(3));
        let events: Vec<_> = native
            .story_events(input("/stories/id:123/"), server.base.clone())
            .try_collect()
            .await
            .unwrap();
        assert_eq!(events.len(), 2);
        let ExtractionEvent::Post(post) = &events[0] else {
            panic!("empty reel still counts as a post")
        };
        assert_eq!(post.count, Some(0));
        assert_eq!(post.metadata.as_ref().unwrap().post_id, "123");
        assert!(matches!(events[1], ExtractionEvent::EndPost));
    }
    let server = Server::new(vec![Response::json(reel(
        "123",
        "123",
        vec![image("101"), video("102")],
    ))]);
    let native = extractor(Duration::from_secs(3));
    let events: Vec<_> = native
        .story_events(input("/stories/id:123/102/"), server.base.clone())
        .try_collect()
        .await
        .unwrap();
    assert_eq!(events.len(), 3);
    let ExtractionEvent::Post(post) = &events[0] else {
        panic!("selected story belongs to reel directory")
    };
    assert_eq!(post.count, Some(1));
    assert_eq!(post.metadata.as_ref().unwrap().post_id, "123");
    let ExtractionEvent::Media(media) = &events[1] else {
        panic!("expected selected file")
    };
    assert_eq!(media.metadata.as_ref().unwrap().media_id, "102");
    assert_eq!(media.metadata.as_ref().unwrap().index, 2);
}

#[tokio::test]
async fn highlight_events_emit_one_directory_per_reel_and_remain_lazy() {
    let tray = json!({"status":"ok","tray":[{"id":"highlight:12"},{"id":"highlight:13"}]});
    let mut first = reel("highlight:12", "123", vec![image("101"), video("102")]);
    first["reels_media"][0]["created_at"] = 555.into();
    let server = Server::new(vec![
        Response::json(tray),
        Response::json(first),
        Response::json(reel("highlight:13", "123", vec![])),
    ]);
    let native = extractor(Duration::from_secs(3));
    let mut events = native.story_events(input("/id:123/highlights/"), server.base.clone());
    assert_eq!(server.count(), 0);
    let ExtractionEvent::Post(post) = events.next().await.unwrap().unwrap() else {
        panic!("expected highlight directory")
    };
    assert_eq!(post.count, Some(2));
    assert_eq!(post.metadata.as_ref().unwrap().post_id, "12");
    assert_eq!(post.metadata.as_ref().unwrap().timestamp, Some(555));
    assert_eq!(server.count(), 2);
    for _ in 0..2 {
        assert!(matches!(
            events.next().await.unwrap().unwrap(),
            ExtractionEvent::Media(_)
        ));
    }
    assert!(matches!(
        events.next().await.unwrap().unwrap(),
        ExtractionEvent::EndPost
    ));
    assert_eq!(
        server.count(),
        2,
        "directory and its files cannot prefetch another reel"
    );
    drop(events);
    tokio::time::sleep(Duration::from_millis(15)).await;
    assert_eq!(server.count(), 2);
}

#[tokio::test]
async fn absent_reels_do_not_consume_post_range_slots_but_returned_empty_reels_do() {
    let absent = json!({"status":"ok","reels_media":[]});
    let server = Server::new(vec![Response::json(absent.clone())]);
    let native = extractor(Duration::from_secs(3));
    let events: Vec<_> = native
        .story_events(input("/stories/id:123/"), server.base.clone())
        .try_collect()
        .await
        .unwrap();
    assert!(
        events.is_empty(),
        "a missing reel must not manufacture a directory"
    );

    for missing in [false, true] {
        let first = if missing {
            absent.clone()
        } else {
            reel("highlight:12", "123", vec![])
        };
        let server = Server::new(vec![
            Response::json(
                json!({"status":"ok","tray":[{"id":"highlight:12"},{"id":"highlight:13"}]}),
            ),
            Response::json(first),
            Response::json(reel("highlight:13", "123", vec![image("102")])),
        ]);
        let selection = crate::post_selection::PostSelection::new(
            Some(crate::selection::RangeSelection::parse("1").unwrap()),
            None,
        );
        let selected: Vec<_> = selection
            .apply(
                native.story_events(input("/id:123/highlights/"), server.base.clone()),
                None,
                None,
                true,
            )
            .try_collect()
            .await
            .unwrap();
        if missing {
            assert_eq!(selected.len(), 1);
            assert_eq!(selected[0].metadata.as_ref().unwrap().media_id, "102");
            assert_eq!(server.count(), 3);
        } else {
            assert!(selected.is_empty());
            assert_eq!(
                server.count(),
                2,
                "a returned empty first reel occupies the range slot and stops before the next request"
            );
        }
    }
}
