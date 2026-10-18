use super::*;
use futures_util::StreamExt;
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
    body: Value,
    delay: Duration,
    headers: String,
}
impl Response {
    fn json(body: Value) -> Self {
        Self {
            status: 200,
            body,
            delay: Duration::ZERO,
            headers: String::new(),
        }
    }
    fn delayed(mut self, ms: u64) -> Self {
        self.delay = Duration::from_millis(ms);
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
            "http://{}/i/api/graphql/",
            listener.local_addr().unwrap()
        ))
        .unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let stop = Arc::new(AtomicBool::new(false));
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
                while !request.ends_with(b"\r\n\r\n") && request.len() < 65536 {
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
                let body = serde_json::to_vec(&response.body).unwrap();
                let header = format!(
                    "HTTP/1.1 {} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n{}\r\n",
                    response.status,
                    body.len(),
                    response.headers
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
    fn urls(&self) -> Vec<Url> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|r| {
                self.base
                    .join(r.split_whitespace().nth(1).unwrap())
                    .unwrap()
            })
            .collect()
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
async fn extractor(timeout: Duration) -> Twitter {
    let native = Twitter::new(None, timeout).await.unwrap();
    *native.cookies.lock().unwrap() = CookieJar::parse("127.0.0.1\tFALSE\t/\tFALSE\t0\tauth_token\tsession\n127.0.0.1\tFALSE\t/\tFALSE\t0\tct0\tcsrf\n").unwrap();
    *native.transaction.lock().unwrap() = Some(
        Transaction::parse(
            include_str!("../tests/fixtures/twitter/transaction-home.html"),
            include_str!("../tests/fixtures/twitter/transaction-script.js"),
        )
        .unwrap(),
    );
    native
}
fn input(name: &str) -> timeline::ProfileInput {
    timeline::input(&Url::parse(&format!("https://x.com/{name}/media")).unwrap()).unwrap()
}
fn user() -> Value {
    json!({"data":{"user":{"result":{"__typename":"User","rest_id":"42","legacy":{"screen_name":"fixture"}}}}})
}
fn tweet(id: &str) -> Value {
    let fixture: Value =
        serde_json::from_str(include_str!("../tests/fixtures/twitter/photo.json")).unwrap();
    let mut tweet = fixture["data"]["tweetResult"]["result"].clone();
    tweet["rest_id"] = id.into();
    tweet["legacy"]["id_str"] = id.into();
    tweet["legacy"]["user_id_str"] = "42".into();
    tweet["core"]["user_results"]["result"]["rest_id"] = "42".into();
    tweet["core"]["user_results"]["result"]["legacy"]["screen_name"] = "fixture".into();
    tweet
}
fn entry(tweet: Value) -> Value {
    json!({"entryId":format!("tweet-{}",tweet["rest_id"].as_str().unwrap()),"content":{"entryType":"TimelineTimelineItem","itemContent":{"itemType":"TimelineTweet","tweet_results":{"result":tweet}}}})
}
fn page(tweets: Vec<Value>, cursor: Option<&str>) -> Value {
    let mut entries: Vec<_> = tweets.into_iter().map(entry).collect();
    if let Some(cursor) = cursor {
        entries.push(json!({"entryId":"cursor-bottom-0","content":{"entryType":"TimelineTimelineCursor","cursorType":"Bottom","value":cursor}}));
    }
    json!({"data":{"user":{"result":{"__typename":"User","rest_id":"42","legacy":{"screen_name":"fixture"},"timeline":{"timeline":{"instructions":[{"type":"TimelineAddEntries","entries":entries}]}}}}}})
}
fn variables(url: &Url) -> Value {
    serde_json::from_str(
        &url.query_pairs()
            .find(|(key, _)| key == "variables")
            .unwrap()
            .1,
    )
    .unwrap()
}

#[tokio::test]
async fn resolves_profile_and_streams_ordered_pages_with_opaque_cursor_and_scoped_headers() {
    let cursor = "next+/=&? opaque";
    let server = Server::new(vec![
        Response::json(user()),
        Response::json(page(vec![tweet("100"), tweet("101")], Some(cursor))),
        Response::json(page(vec![tweet("102")], None)),
    ]);
    let native = extractor(Duration::from_secs(3)).await;
    let mut stream = native.media_stream(input("fixture"), server.base.clone());
    assert_eq!(server.count(), 0);
    let mut items = Vec::new();
    while let Some(item) = stream.next().await {
        items.push(item.unwrap());
    }
    assert_eq!(
        items
            .iter()
            .map(|m| m.metadata.as_ref().unwrap().post_id.as_str())
            .collect::<Vec<_>>(),
        ["100", "101", "102"]
    );
    let single =
        serde_json::to_vec(&json!({"data":{"tweetResult":{"result":tweet("100")}}})).unwrap();
    assert_eq!(
        items[0].archive_key(),
        parse_response(&single, "100").unwrap()[0].archive_key()
    );
    let urls = server.urls();
    assert!(
        urls[0]
            .path()
            .ends_with("/ck5KkZ8t5cOmoLssopN99Q/UserByScreenName")
    );
    assert_eq!(variables(&urls[0])["screen_name"], "fixture");
    assert_eq!(variables(&urls[0])["withGrokTranslatedBio"], false);
    assert!(
        urls[1]
            .path()
            .ends_with("/jCRhbOzdgOHp6u9H4g2tEg/UserMedia")
    );
    let initial = variables(&urls[1]);
    assert_eq!(initial["userId"], "42");
    assert_eq!(initial["count"], 50);
    assert_eq!(initial["includePromotedContent"], false);
    assert_eq!(initial["withVoice"], true);
    assert!(initial.get("cursor").is_none());
    assert_eq!(variables(&urls[2])["cursor"], cursor);
    for request in server.requests.lock().unwrap().iter() {
        let lower = request.to_ascii_lowercase();
        assert!(lower.contains("x-csrf-token: csrf\r\n"));
        assert!(lower.contains("cookie: auth_token=session; ct0=csrf\r\n"));
        let encoded = request
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("x-client-transaction-id")
                    .then(|| value.trim())
            })
            .unwrap();
        let bytes = STANDARD_NO_PAD.decode(encoded).unwrap();
        let mask = bytes[0];
        let transaction = native.transaction.lock().unwrap();
        let transaction = transaction.as_ref().unwrap();
        let offset = 1 + transaction.key.len();
        let seconds = u32::from_le_bytes(
            bytes[offset..offset + 4]
                .iter()
                .map(|b| b ^ mask)
                .collect::<Vec<_>>()
                .try_into()
                .unwrap(),
        );
        let path = request
            .split_whitespace()
            .nth(1)
            .unwrap()
            .split('?')
            .next()
            .unwrap();
        assert_eq!(
            encoded,
            transaction.generate_at("GET", path, u64::from(seconds), mask)
        );
    }
    assert!(
        !items[0]
            .headers
            .values()
            .any(|s| s.contains("session") || s.contains("csrf"))
    );
}

#[tokio::test]
async fn explicit_user_id_is_lazy_and_range_cutoff_prevents_next_page() {
    let server = Server::new(vec![
        Response::json(page(vec![tweet("100"), tweet("101")], Some("next"))),
        Response::json(page(vec![tweet("102")], None)),
    ]);
    let native = extractor(Duration::from_secs(3)).await;
    let stream = native.media_stream(input("id:42"), server.base.clone());
    assert_eq!(server.count(), 0);
    let mut stream = crate::selection::RangeSelection::parse("1")
        .unwrap()
        .apply(stream);
    assert!(stream.next().await.unwrap().is_ok());
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(server.count(), 1);
    assert!(stream.next().await.is_none());
    assert_eq!(server.count(), 1);
}

#[tokio::test]
async fn consumer_backpressure_and_empty_advancing_page_do_not_lose_media() {
    let server = Server::new(vec![
        Response::json(page(vec![tweet("100"), tweet("101")], Some("a"))),
        Response::json(page(vec![], Some("b"))),
        Response::json(page(vec![tweet("102")], None)),
    ]);
    let native = extractor(Duration::from_secs(3)).await;
    let mut stream = native.media_stream(input("id:42"), server.base.clone());
    assert!(stream.next().await.unwrap().is_ok());
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(server.count(), 1);
    assert!(stream.next().await.unwrap().is_ok());
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
        "102"
    );
    assert!(stream.next().await.is_none());
    assert_eq!(server.count(), 3);
}

#[tokio::test]
async fn late_auth_parser_and_cursor_failures_preserve_prior_items_then_end() {
    let mut auth = Response::json(json!({"errors":[{"code":32,"message":"private fixture"}]}));
    auth.status = 401;
    for response in [
        auth,
        Response::json(json!({"data":{}})),
        Response::json(page(vec![], Some("a"))),
    ] {
        let server = Server::new(vec![
            Response::json(page(vec![tweet("100")], Some("a"))),
            response,
        ]);
        let native = extractor(Duration::from_secs(3)).await;
        let mut stream = native.media_stream(input("id:42"), server.base.clone());
        assert_eq!(
            stream
                .next()
                .await
                .unwrap()
                .unwrap()
                .metadata
                .unwrap()
                .post_id,
            "100"
        );
        let error = stream.next().await.unwrap().unwrap_err().to_string();
        assert!(!error.contains("private fixture"));
        assert!(stream.next().await.is_none());
        assert_eq!(server.count(), 2);
    }
}

#[tokio::test]
async fn explicit_termination_ends_even_if_response_repeats_last_cursor() {
    let mut final_page = page(vec![tweet("101")], Some("a"));
    final_page["data"]["user"]["result"]["timeline"]["timeline"]["instructions"]
        .as_array_mut()
        .unwrap()
        .push(json!({"type":"TimelineTerminateTimeline","direction":"Bottom"}));
    let server = Server::new(vec![
        Response::json(page(vec![tweet("100")], Some("a"))),
        Response::json(final_page),
    ]);
    let native = extractor(Duration::from_secs(3)).await;
    let media: Vec<_> = native
        .media_stream(input("id:42"), server.base.clone())
        .try_collect()
        .await
        .unwrap();
    assert_eq!(media.len(), 2);
    assert_eq!(server.count(), 2);
}

#[tokio::test]
async fn rejects_wrong_profile_and_foreign_tweet_owner() {
    let mut wrong_profile = page(vec![tweet("100")], None);
    wrong_profile["data"]["user"]["result"]["rest_id"] = "99".into();
    let mut foreign = tweet("100");
    foreign["legacy"]["user_id_str"] = "99".into();
    foreign["core"]["user_results"]["result"]["rest_id"] = "99".into();
    for body in [wrong_profile, page(vec![foreign], None)] {
        let server = Server::new(vec![Response::json(body)]);
        let native = extractor(Duration::from_secs(3)).await;
        let mut stream = native.media_stream(input("id:42"), server.base.clone());
        assert!(stream.next().await.unwrap().is_err());
        assert!(stream.next().await.is_none());
    }
}

#[tokio::test]
async fn unordered_dates_continue_after_older_media() {
    let mut old = tweet("100");
    old["legacy"]["created_at"] = "Tue Nov 14 22:13:20 +0000 2023".into();
    let mut recent = tweet("101");
    recent["legacy"]["created_at"] = "Wed Nov 15 22:13:20 +0000 2023".into();
    let server = Server::new(vec![
        Response::json(page(vec![old], Some("next"))),
        Response::json(page(vec![recent], None)),
    ]);
    let native = extractor(Duration::from_secs(3)).await;
    let source = native.media_stream(input("id:42"), server.base.clone());
    let mut selected = crate::date::DateFilter::new(None, Some(1700000001))
        .unwrap()
        .apply_unordered(source);
    assert_eq!(
        selected
            .next()
            .await
            .unwrap()
            .unwrap()
            .metadata
            .unwrap()
            .post_id,
        "101"
    );
    assert!(selected.next().await.is_none());
    assert_eq!(server.count(), 2);
}

#[tokio::test]
async fn active_budget_excludes_consumer_pause_but_accumulates_requests() {
    let server = Server::new(vec![
        Response::json(page(vec![tweet("100")], Some("a"))).delayed(20),
        Response::json(page(vec![tweet("101")], None)).delayed(20),
    ]);
    let native = extractor(Duration::from_millis(250)).await;
    let mut stream = native.media_stream(input("id:42"), server.base.clone());
    assert!(stream.next().await.unwrap().is_ok());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(stream.next().await.unwrap().is_ok());
    assert!(stream.next().await.is_none());

    let server = Server::new(vec![
        Response::json(page(vec![tweet("100")], Some("a"))).delayed(150),
        Response::json(page(vec![tweet("101")], None)).delayed(180),
    ]);
    let native = extractor(Duration::from_millis(250)).await;
    let mut stream = native.media_stream(input("id:42"), server.base.clone());
    assert!(stream.next().await.unwrap().is_ok());
    assert!(
        stream
            .next()
            .await
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("timed out")
    );
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn dropped_pending_request_never_follows_navigation() {
    let server = Server::new(vec![
        Response::json(page(vec![tweet("100")], Some("a"))).delayed(300),
        Response::json(page(vec![tweet("101")], None)),
    ]);
    let native = extractor(Duration::from_secs(3)).await;
    let mut stream = native.media_stream(input("id:42"), server.base.clone());
    {
        let pending = stream.next();
        tokio::pin!(pending);
        tokio::select! { result = &mut pending => panic!("unexpected completion: {result:?}"), () = server.wait_for(1) => {} }
    }
    drop(stream);
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(server.count(), 1);
}

#[tokio::test]
async fn lookup_cookie_rotation_and_user_agent_reach_all_pages_without_leaking_to_media() {
    let mut lookup = Response::json(user());
    lookup.headers =
        "Set-Cookie: ct0=rotated; Path=/\r\nSet-Cookie: auth_token=afterlookup; Path=/\r\n".into();
    let server = Server::new(vec![
        lookup,
        Response::json(page(vec![tweet("100")], Some("next"))),
        Response::json(page(vec![tweet("101")], None)),
    ]);
    let network = NetworkOptions::new(None, Some("gallery-dl-timeline-test/1".into())).unwrap();
    let native = extractor(Duration::from_secs(3))
        .await
        .with_network(&network)
        .unwrap();
    let mut stream = native.media_stream(input("fixture"), server.base.clone());
    let mut count = 0;
    while let Some(media) = stream.next().await {
        let media = media.unwrap();
        assert_eq!(
            media.headers.get("User-Agent").map(String::as_str),
            Some("gallery-dl-timeline-test/1")
        );
        assert!(
            !media
                .headers
                .keys()
                .any(|key| key.eq_ignore_ascii_case("cookie")
                    || key.eq_ignore_ascii_case("x-csrf-token")
                    || key.eq_ignore_ascii_case("authorization")
                    || key.eq_ignore_ascii_case("x-client-transaction-id"))
        );
        assert!(
            !media
                .headers
                .values()
                .any(|value| value.contains("afterlookup") || value.contains("rotated"))
        );
        count += 1;
    }
    assert_eq!(count, 2);
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    for (index, request) in requests.iter().enumerate() {
        let lower = request.to_ascii_lowercase();
        assert!(lower.contains("user-agent: gallery-dl-timeline-test/1\r\n"));
        if index == 0 {
            assert!(lower.contains("x-csrf-token: csrf\r\n"));
            assert!(lower.contains("auth_token=session"));
        } else {
            assert!(lower.contains("x-csrf-token: rotated\r\n"));
            assert!(lower.contains("auth_token=afterlookup"));
            assert!(lower.contains("ct0=rotated"));
            assert!(!lower.contains("auth_token=session"));
        }
    }
}
