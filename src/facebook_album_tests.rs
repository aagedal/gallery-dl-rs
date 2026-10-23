use super::*;
use crate::facebook_album::AlbumInput;
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
        let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
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
fn photo(id: &str, next: Option<&str>) -> Value {
    json!({"__typename":"Photo","__isNode":"Photo","id":id,
        "url":format!("https://www.facebook.com/photo/?fbid={id}&set=a.10"),
        "image":{"uri":format!("https://cdn.example/{id}.jpg")},
        "nextMediaAfterNodeId":next.map(|id| json!({"__typename":"Photo","id":id}))})
}
fn input(start: Option<&str>) -> AlbumInput {
    AlbumInput {
        set: "a.10".into(),
        start: start.map(str::to_owned),
    }
}
async fn extractor(timeout: Duration) -> Facebook {
    Facebook::new(None, timeout).await.unwrap()
}

#[tokio::test]
async fn seeded_album_preserves_order_indices_query_and_archive_identity() {
    let server = Server::new(vec![
        Response::json(
            json!({"mediaSetToken":"a.10","photos":{"edges":[{"node":{"__typename":"Photo","id":"1"}},{"node":{"__typename":"Photo","id":"2"}}]}}),
        ),
        Response::json(photo("1", Some("2"))),
        Response::json(photo("2", None)),
    ]);
    let facebook = extractor(Duration::from_secs(3)).await;
    let items = facebook
        .album_stream(input(None), server.base.clone())
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    assert_eq!(items.len(), 2);
    for (index, item) in items.iter().enumerate() {
        let id = (index + 1).to_string();
        assert_eq!(item.metadata.as_ref().unwrap().post_id, id);
        assert_eq!(item.metadata.as_ref().unwrap().index, index + 1);
        let single = parse_photo(&serde_json::to_vec(&photo(&id, None)).unwrap(), &id).unwrap();
        assert_eq!(item.archive_key(), single[0].archive_key());
    }
    let requests = server.requests.lock().unwrap();
    assert!(requests[0].starts_with("GET /media/set/?set=a.10 "));
    // Album pages answer HTTP 400 to requests that are not navigations.
    assert!(requests[0].contains("sec-fetch-mode: navigate\r\n"));
    assert!(requests[0].contains("accept-language: en-US,en;q=0.5\r\n"));
    assert!(requests[1].starts_with("GET /photo/?fbid=1&set=a.10 "));
    assert!(requests[2].starts_with("GET /photo/?fbid=2&set=a.10 "));
}

#[tokio::test]
async fn direct_start_is_lazy_and_backpressure_does_not_prefetch() {
    let server = Server::new(vec![
        Response::json(photo("1", Some("2"))),
        Response::json(photo("2", None)),
    ]);
    let facebook = extractor(Duration::from_secs(3)).await;
    let mut stream = facebook.album_stream(input(Some("1")), server.base.clone());
    tokio::time::sleep(Duration::from_millis(25)).await;
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
        "1"
    );
    tokio::time::sleep(Duration::from_millis(25)).await;
    assert_eq!(server.count(), 1);
    assert_eq!(
        stream
            .next()
            .await
            .unwrap()
            .unwrap()
            .metadata
            .unwrap()
            .post_id,
        "2"
    );
    assert!(stream.next().await.is_none());
    assert_eq!(server.count(), 2);
}

#[tokio::test]
async fn late_authentication_and_missing_navigation_keep_prior_items_then_end() {
    for response in [
        Response {
            status: 403,
            body: Vec::new(),
            delay: Duration::ZERO,
        },
        Response::json({
            let mut value = photo("2", None);
            value
                .as_object_mut()
                .unwrap()
                .remove("nextMediaAfterNodeId");
            value
        }),
    ] {
        let server = Server::new(vec![Response::json(photo("1", Some("2"))), response]);
        let facebook = extractor(Duration::from_secs(3)).await;
        let mut stream = facebook.album_stream(input(Some("1")), server.base.clone());
        assert_eq!(
            stream
                .next()
                .await
                .unwrap()
                .unwrap()
                .metadata
                .unwrap()
                .post_id,
            "1"
        );
        assert!(stream.next().await.unwrap().is_err());
        assert!(stream.next().await.is_none());
        assert_eq!(server.count(), 2);
    }
}

#[tokio::test]
async fn wrap_to_first_ends_and_nonfirst_cycles_fail_without_refetch() {
    for responses in [
        vec![Response::json(photo("1", Some("1")))],
        vec![
            Response::json(photo("1", Some("2"))),
            Response::json(photo("2", Some("1"))),
        ],
    ] {
        let count = responses.len();
        let server = Server::new(responses);
        let facebook = extractor(Duration::from_secs(3)).await;
        let items = facebook
            .album_stream(input(Some("1")), server.base.clone())
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        assert_eq!(items.len(), count);
        assert_eq!(server.count(), count);
    }
    let server = Server::new(vec![
        Response::json(photo("1", Some("2"))),
        Response::json(photo("2", Some("3"))),
        Response::json(photo("3", Some("2"))),
    ]);
    let facebook = extractor(Duration::from_secs(3)).await;
    let mut stream = facebook.album_stream(input(Some("1")), server.base.clone());
    assert!(stream.next().await.unwrap().is_ok());
    assert!(stream.next().await.unwrap().is_ok());
    assert!(stream.next().await.unwrap().is_err());
    assert!(stream.next().await.is_none());
    assert_eq!(server.count(), 3);
}

#[tokio::test]
async fn consumer_pauses_do_not_consume_active_budget_but_requests_share_it() {
    let server = Server::new(vec![
        Response::json(photo("1", Some("2"))),
        Response::json(photo("2", None)),
    ]);
    let facebook = extractor(Duration::from_millis(200)).await;
    let mut stream = facebook.album_stream(input(Some("1")), server.base.clone());
    assert!(stream.next().await.unwrap().is_ok());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(stream.next().await.unwrap().is_ok());
    assert!(stream.next().await.is_none());

    let server = Server::new(vec![
        Response::json(photo("1", Some("2"))).delayed(Duration::from_millis(130)),
        Response::json(photo("2", None)).delayed(Duration::from_millis(130)),
    ]);
    let facebook = extractor(Duration::from_millis(210)).await;
    let mut stream = facebook.album_stream(input(Some("1")), server.base.clone());
    assert!(stream.next().await.unwrap().is_ok());
    let failure = stream.next().await.unwrap().unwrap_err();
    assert!(failure.to_string().contains("timed out"), "{failure}");
    assert!(stream.next().await.is_none());
    assert_eq!(server.count(), 2);
}

#[tokio::test]
async fn dropping_after_selection_or_during_request_stops_future_navigation() {
    let server = Server::new(vec![
        Response::json(photo("1", Some("2"))),
        Response::json(photo("2", None)),
    ]);
    let facebook = extractor(Duration::from_secs(3)).await;
    let mut stream = facebook.album_stream(input(Some("1")), server.base.clone());
    assert!(stream.next().await.unwrap().is_ok());
    drop(stream);
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(server.count(), 1);

    let server = Server::new(vec![
        Response::json(photo("1", Some("2"))).delayed(Duration::from_secs(1)),
        Response::json(photo("2", None)),
    ]);
    let facebook = extractor(Duration::from_secs(3)).await;
    let mut stream = facebook.album_stream(input(Some("1")), server.base.clone());
    tokio::select! {
        result = stream.next() => panic!("request unexpectedly completed: {result:?}"),
        () = server.wait_for(1) => {}
    }
    drop(stream);
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(server.count(), 1);
}

#[tokio::test]
async fn range_and_date_cutoffs_stop_before_the_next_photo_request() {
    let server = Server::new(vec![
        Response::json(photo("1", Some("2"))),
        Response::json(photo("2", None)),
    ]);
    let facebook = extractor(Duration::from_secs(3)).await;
    let selected = crate::selection::RangeSelection::parse("1").unwrap();
    let mut stream = selected.apply(facebook.album_stream(input(Some("1")), server.base.clone()));
    assert!(stream.next().await.unwrap().is_ok());
    assert!(stream.next().await.is_none());
    assert_eq!(server.count(), 1);

    let mut first = photo("1", Some("2"));
    first["created_time"] = json!(100);
    let server = Server::new(vec![
        Response::json(first),
        Response::json(photo("2", None)),
    ]);
    let facebook = extractor(Duration::from_secs(3)).await;
    let dates = crate::date::DateFilter::new(None, Some(100)).unwrap();
    let mut stream = dates.apply(facebook.album_stream(input(Some("1")), server.base.clone()));
    assert!(stream.next().await.is_none());
    assert_eq!(server.count(), 1);
}

#[tokio::test]
async fn album_events_preserve_photo_boundaries_and_defer_navigation_until_group_drains() {
    use crate::event::ExtractionEvent;
    use futures_util::StreamExt;
    let server = Server::new(vec![
        Response::json(photo("1", Some("2"))),
        Response::json(photo("2", None)),
    ]);
    let facebook = Facebook::new(None, Duration::from_secs(3)).await.unwrap();
    let mut events = facebook.album_events(input(Some("1")), server.base.clone());
    assert_eq!(server.count(), 0);
    for (position, id) in [(1, "1"), (2, "2")] {
        let ExtractionEvent::Post(post) = events.next().await.unwrap().unwrap() else {
            panic!("album post missing")
        };
        assert_eq!(post.count, Some(1));
        assert_eq!(post.category, "facebook");
        let metadata = post.metadata.unwrap();
        assert_eq!(metadata.post_id, id);
        assert!(metadata.file.is_none());
        assert_eq!(metadata.index, 0);
        assert_eq!(server.count(), position);
        let ExtractionEvent::Media(item) = events.next().await.unwrap().unwrap() else {
            panic!("album media missing")
        };
        assert_eq!(item.metadata.as_ref().unwrap().post_id, id);
        assert_eq!(item.metadata.as_ref().unwrap().index, position);
        assert_eq!(server.count(), position);
        assert!(matches!(
            events.next().await.unwrap().unwrap(),
            ExtractionEvent::EndPost
        ));
        assert_eq!(server.count(), position);
    }
    assert!(events.next().await.is_none());
    assert_eq!(server.count(), 2);
}

#[tokio::test]
async fn stopping_after_album_post_boundary_does_not_fetch_next_photo() {
    use crate::event::ExtractionEvent;
    use futures_util::StreamExt;
    for cutoff in [1, 3] {
        let server = Server::new(vec![
            Response::json(photo("1", Some("2"))),
            Response::json(photo("2", None)),
        ]);
        let facebook = Facebook::new(None, Duration::from_secs(3)).await.unwrap();
        let events: Vec<_> = facebook
            .album_events(input(Some("1")), server.base.clone())
            .take(cutoff)
            .try_collect()
            .await
            .unwrap();
        assert!(matches!(events.first(), Some(ExtractionEvent::Post(_))));
        if cutoff == 3 {
            assert!(matches!(events.last(), Some(ExtractionEvent::EndPost)));
        }
        tokio::time::sleep(Duration::from_millis(15)).await;
        assert_eq!(server.count(), 1);
    }
}

#[tokio::test]
async fn anonymous_albums_follow_the_deferred_grid_and_report_truncation() {
    let unnavigable = |id: &str| {
        let mut page = photo(id, None);
        page.as_object_mut().unwrap().remove("nextMediaAfterNodeId");
        Response::json(page)
    };
    let grid = |more: bool| {
        json!([{"mediaSetToken":"a.10"},
            {"label":"Query$defer$Grid_mediaset","path":["mediaset"],"data":{"grid_media":{
                "edges":[{"node":{"__typename":"Photo","id":"1"}},
                         {"node":{"__typename":"Photo","id":"2"}}],
                "page_info":{"has_next_page":more}}}}])
    };
    let server = Server::new(vec![
        Response::json(grid(true)),
        unnavigable("1"),
        unnavigable("2"),
        Response::json(grid(false)),
        unnavigable("1"),
        unnavigable("2"),
    ]);
    let facebook = extractor(Duration::from_secs(3)).await;
    let results: Vec<_> = facebook
        .album_stream(input(None), server.base.clone())
        .collect()
        .await;
    assert_eq!(results.len(), 3);
    assert_eq!(
        results[1]
            .as_ref()
            .unwrap()
            .metadata
            .as_ref()
            .unwrap()
            .index,
        2
    );
    let message = results[2].as_ref().unwrap_err().to_string();
    assert!(message.contains("--cookies"), "{message}");
    // A complete grid ends without an error.
    let items = facebook
        .album_stream(input(None), server.base.clone())
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    assert_eq!(items.len(), 2);
    let requests = server.requests.lock().unwrap();
    assert!(requests[2].starts_with("GET /photo/?fbid=2&set=a.10 "));
}

#[test]
fn grid_requires_the_page_to_name_the_set() {
    let page = serde_json::to_vec(&json!([{"mediaSetToken":"a.99"},
        {"path":["mediaset"],"data":{"grid_media":{"edges":[
            {"node":{"__typename":"Photo","id":"1"}}]}}}]))
    .unwrap();
    assert!(
        crate::facebook_album::parse_grid(&page, "a.10")
            .ids
            .is_empty()
    );
    assert_eq!(crate::facebook_album::parse_grid(&page, "a.99").ids, ["1"]);
    assert!(parse_seed(&page, "a.10").is_err());
    assert_eq!(parse_seed(&page, "a.99").unwrap(), "1");
}
