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

fn post_response(tweet: Value) -> Value {
    json!({"data":{"tweetResult":{"result":tweet}}})
}
fn quoted_tweet(id: &str, quoted: &str) -> Value {
    let mut value = tweet(id);
    value["legacy"]["quoted_status_id_str"] = quoted.into();
    value
}
fn expansion_options(retweets: RetweetMode) -> ExpansionOptions {
    ExpansionOptions {
        retweets,
        quoted: true,
    }
}
fn local_post_endpoint(server: &Server, id: &str) -> Url {
    let upstream = endpoint(id).unwrap();
    let mut local = server.base.clone();
    local.set_path(upstream.path());
    local.set_query(upstream.query());
    local
}
fn assert_scoped_expansion_requests(server: &Server, native: &Twitter) {
    for request in server.requests.lock().unwrap().iter() {
        let lower = request.to_ascii_lowercase();
        assert!(lower.contains("x-csrf-token: csrf\r\n"));
        assert!(lower.contains("auth_token=session"));
        let encoded = request
            .lines()
            .find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.eq_ignore_ascii_case("x-client-transaction-id")
                    .then(|| value.trim())
            })
            .unwrap();
        let bytes = STANDARD_NO_PAD.decode(encoded).unwrap();
        let transaction = native.transaction.lock().unwrap();
        let transaction = transaction.as_ref().unwrap();
        let mask = bytes[0];
        let offset = 1 + transaction.key.len();
        let seconds = u32::from_le_bytes(
            bytes[offset..offset + 4]
                .iter()
                .map(|byte| byte ^ mask)
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
}

#[tokio::test]
async fn post_quote_fetch_waits_for_consumer_and_drop_cancels_traversal() {
    let server = Server::new(vec![
        Response::json(post_response(quoted_tweet("100", "101"))),
        Response::json(post_response(tweet("101"))),
    ]);
    let native = extractor(Duration::from_secs(3))
        .await
        .with_expansion(expansion_options(RetweetMode::Skip));
    let mut stream = native.post_stream_at(local_post_endpoint(&server, "100"), "100".into());
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
        "100"
    );
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(server.count(), 1);
    drop(stream);
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(server.count(), 1);
}

#[tokio::test]
async fn post_quote_fetch_preserves_original_identity_and_scoped_transaction() {
    let server = Server::new(vec![
        Response::json(post_response(quoted_tweet("100", "101"))),
        Response::json(post_response(tweet("101"))),
    ]);
    let native = extractor(Duration::from_secs(3))
        .await
        .with_expansion(expansion_options(RetweetMode::Skip));
    let items: Vec<_> = native
        .post_stream_at(local_post_endpoint(&server, "100"), "100".into())
        .try_collect()
        .await
        .unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].metadata.as_ref().unwrap().post_id, "100");
    let quoted = items[1].metadata.as_ref().unwrap();
    assert_eq!(quoted.post_id, "101");
    assert_eq!(quoted.quoted_by_id.as_deref(), Some("100"));
    let original = parse_response(
        &serde_json::to_vec(&post_response(tweet("101"))).unwrap(),
        "101",
    )
    .unwrap();
    assert_eq!(items[1].archive_key(), original[0].archive_key());
    assert_eq!(items[1].filename, original[0].filename);
    let urls = server.urls();
    assert_eq!(urls.len(), 2);
    assert_eq!(urls[0].origin(), urls[1].origin());
    assert!(urls[1].path().ends_with("/TweetResultByRestId"));
    assert_eq!(variables(&urls[1])["tweetId"], "101");
    assert_scoped_expansion_requests(&server, &native);
    for item in items {
        let metadata = serde_json::to_string(item.metadata.as_ref().unwrap()).unwrap();
        assert!(!metadata.contains("session"));
        assert!(!metadata.contains("csrf"));
        assert!(!item.headers.keys().any(|key| {
            key.eq_ignore_ascii_case("cookie")
                || key.eq_ignore_ascii_case("authorization")
                || key.eq_ignore_ascii_case("x-client-transaction-id")
        }));
    }
}

#[tokio::test]
async fn timeline_expands_missing_quote_before_requesting_next_page() {
    let server = Server::new(vec![
        Response::json(page(vec![quoted_tweet("100", "101")], Some("next"))),
        Response::json(post_response(tweet("101"))),
        Response::json(page(vec![tweet("102")], None)),
    ]);
    let native = extractor(Duration::from_secs(3))
        .await
        .with_expansion(expansion_options(RetweetMode::Skip));
    let mut stream = native.media_stream(input("id:42"), server.base.clone());
    for (id, count) in [("100", 1), ("101", 2), ("102", 3)] {
        assert_eq!(
            stream
                .next()
                .await
                .unwrap()
                .unwrap()
                .metadata
                .unwrap()
                .post_id,
            id
        );
        assert_eq!(server.count(), count);
    }
    assert!(stream.next().await.is_none());
    let urls = server.urls();
    assert!(urls[0].path().ends_with("/UserMedia"));
    assert!(urls[1].path().ends_with("/TweetResultByRestId"));
    assert_eq!(variables(&urls[1])["tweetId"], "101");
    assert!(urls[2].path().ends_with("/UserMedia"));
    assert_eq!(variables(&urls[2])["cursor"], "next");
}

#[tokio::test]
async fn timeline_range_cutoff_avoids_missing_quote_and_next_page_requests() {
    let server = Server::new(vec![
        Response::json(page(vec![quoted_tweet("100", "101")], Some("next"))),
        Response::json(post_response(tweet("101"))),
        Response::json(page(vec![tweet("102")], None)),
    ]);
    let native = extractor(Duration::from_secs(3))
        .await
        .with_expansion(expansion_options(RetweetMode::Skip));
    let mut selected = crate::selection::RangeSelection::parse("1")
        .unwrap()
        .apply(native.media_stream(input("id:42"), server.base.clone()));
    assert!(selected.next().await.unwrap().is_ok());
    assert!(selected.next().await.is_none());
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(server.count(), 1);
}

#[tokio::test]
async fn missing_retweet_target_fetch_uses_selected_archive_identity() {
    for mode in [RetweetMode::Repost, RetweetMode::Original] {
        let mut repost = tweet("100");
        repost["legacy"]
            .as_object_mut()
            .unwrap()
            .remove("extended_entities");
        repost["legacy"]["retweeted_status_id_str"] = "101".into();
        let server = Server::new(vec![
            Response::json(post_response(repost)),
            Response::json(post_response(tweet("101"))),
        ]);
        let native = extractor(Duration::from_secs(3))
            .await
            .with_expansion(expansion_options(mode));
        let media: Vec<_> = native
            .post_stream_at(local_post_endpoint(&server, "100"), "100".into())
            .try_collect()
            .await
            .unwrap();
        assert_eq!(media.len(), 1);
        assert_eq!(server.count(), 2);
        assert_eq!(variables(&server.urls()[1])["tweetId"], "101");
        let metadata = media[0].metadata.as_ref().unwrap();
        let identity = match mode {
            RetweetMode::Repost => {
                assert_eq!(metadata.retweet_id.as_deref(), Some("101"));
                "100"
            }
            RetweetMode::Original => {
                assert_eq!(metadata.reposted_by_id.as_deref(), Some("100"));
                "101"
            }
            RetweetMode::Skip => unreachable!(),
        };
        assert_eq!(metadata.post_id, identity);
        let direct = parse_response(
            &serde_json::to_vec(&post_response(tweet(identity))).unwrap(),
            identity,
        )
        .unwrap();
        assert_eq!(media[0].archive_key(), direct[0].archive_key());
        assert_scoped_expansion_requests(&server, &native);
    }
}

#[tokio::test]
async fn quote_auth_failure_preserves_parent_and_ends_without_private_error_text() {
    let mut denied =
        Response::json(json!({"errors":[{"code":32,"message":"private session fixture"}]}))
            .delayed(20);
    denied.status = 401;
    let server = Server::new(vec![
        Response::json(post_response(quoted_tweet("100", "101"))),
        denied,
    ]);
    let native = extractor(Duration::from_secs(3))
        .await
        .with_expansion(expansion_options(RetweetMode::Skip));
    let mut stream = native.post_stream_at(local_post_endpoint(&server, "100"), "100".into());
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
    assert_eq!(server.count(), 1);
    let error = stream.next().await.unwrap().unwrap_err().to_string();
    assert!(!error.contains("private session fixture"));
    assert!(stream.next().await.is_none());
    assert_eq!(server.count(), 2);
}

#[tokio::test]
async fn dropping_pending_quote_request_stops_further_expansion() {
    let server = Server::new(vec![
        Response::json(post_response(quoted_tweet("100", "101"))),
        Response::json(post_response(quoted_tweet("101", "102"))).delayed(300),
        Response::json(post_response(tweet("102"))),
    ]);
    let native = extractor(Duration::from_secs(3))
        .await
        .with_expansion(expansion_options(RetweetMode::Skip));
    let mut stream = native.post_stream_at(local_post_endpoint(&server, "100"), "100".into());
    assert!(stream.next().await.unwrap().is_ok());
    {
        let pending = stream.next();
        tokio::pin!(pending);
        tokio::select! { result = &mut pending => panic!("unexpected completion: {result:?}"), () = server.wait_for(2) => {} }
    }
    drop(stream);
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(server.count(), 2);
}

#[tokio::test]
async fn quote_cycle_errors_without_refetching_the_ancestor() {
    let server = Server::new(vec![
        Response::json(post_response(quoted_tweet("100", "101"))),
        Response::json(post_response(quoted_tweet("101", "100"))),
    ]);
    let native = extractor(Duration::from_secs(3))
        .await
        .with_expansion(expansion_options(RetweetMode::Skip));
    let mut stream = native.post_stream_at(local_post_endpoint(&server, "100"), "100".into());
    assert!(stream.next().await.unwrap().is_ok());
    assert!(stream.next().await.unwrap().is_ok());
    assert!(stream.next().await.unwrap().is_err());
    assert!(stream.next().await.is_none());
    assert_eq!(server.count(), 2);
}

fn authored(mut value: Value, owner: &str) -> Value {
    value["legacy"]["user_id_str"] = owner.into();
    value["core"]["user_results"]["result"]["rest_id"] = owner.into();
    value
}

fn reply(id: &str, owner: &str, to: &str) -> Value {
    let mut value = authored(tweet(id), owner);
    value["legacy"]["in_reply_to_status_id_str"] = "90".into();
    value["legacy"]["in_reply_to_user_id_str"] = to.into();
    value
}

fn post_ids(media: &[Media]) -> Vec<&str> {
    media
        .iter()
        .map(|item| item.metadata.as_ref().unwrap().post_id.as_str())
        .collect()
}

#[tokio::test]
async fn explicit_profile_tabs_request_their_operations_lazily_and_preserve_page_order() {
    use timeline::ProfileTab;
    for (tab, operation) in [
        (ProfileTab::Tweets, "/E8Wq-_jFSaU7hxVcuOPR9g/UserTweets"),
        (
            ProfileTab::Replies,
            "/-O3QOHrVn1aOm_cF5wyTCQ/UserTweetsAndReplies",
        ),
        (
            ProfileTab::Highlights,
            "/gmHw9geMTncZ7jeLLUUNOw/UserHighlightsTweets",
        ),
    ] {
        let server = Server::new(vec![
            Response::json(page(vec![tweet("100"), tweet("101")], Some("next+/="))),
            Response::json(page(vec![tweet("102")], None)),
        ]);
        let native = extractor(Duration::from_secs(3)).await;
        let mut stream = native.profile_stream(input("id:42"), tab, server.base.clone());
        assert_eq!(server.count(), 0);
        let first = stream.next().await.unwrap().unwrap();
        assert_eq!(first.metadata.unwrap().post_id, "100");
        assert_eq!(server.count(), 1);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(server.count(), 1);
        let rest: Vec<_> = stream.try_collect().await.unwrap();
        assert_eq!(post_ids(&rest), ["101", "102"]);
        let urls = server.urls();
        assert_eq!(urls.len(), 2);
        for url in &urls {
            assert!(url.path().ends_with(operation), "{}", url.path());
            let vars = variables(url);
            assert_eq!(vars["userId"], "42");
            assert_eq!(vars["count"], 50);
            assert_eq!(vars["includePromotedContent"], false);
            assert_eq!(vars["withVoice"], true);
            let toggles: Value = serde_json::from_str(
                &url.query_pairs()
                    .find(|(key, _)| key == "fieldToggles")
                    .unwrap()
                    .1,
            )
            .unwrap();
            assert_eq!(toggles["withArticlePlainText"], false);
            if matches!(tab, ProfileTab::Tweets) {
                assert_eq!(vars["withQuickPromoteEligibilityTweetFields"], false);
            }
            if matches!(tab, ProfileTab::Replies) {
                assert_eq!(vars["withCommunity"], true);
            }
        }
        assert!(variables(&urls[0]).get("cursor").is_none());
        assert_eq!(variables(&urls[1])["cursor"], "next+/=");
        assert_scoped_expansion_requests(&server, &native);
    }
}

#[tokio::test]
async fn profile_tabs_keep_foreign_conversation_context_but_reject_conflicting_author_aliases() {
    for tab in [
        timeline::ProfileTab::Tweets,
        timeline::ProfileTab::Replies,
        timeline::ProfileTab::Highlights,
    ] {
        let server = Server::new(vec![Response::json(page(
            vec![authored(tweet("100"), "99"), tweet("101")],
            None,
        ))]);
        let native = extractor(Duration::from_secs(3)).await;
        let items: Vec<_> = native
            .profile_stream(input("id:42"), tab, server.base.clone())
            .try_collect()
            .await
            .unwrap();
        assert_eq!(post_ids(&items), ["100", "101"]);

        let mut conflicting = authored(tweet("102"), "99");
        conflicting["core"]["user_results"]["result"]["rest_id"] = "42".into();
        let server = Server::new(vec![Response::json(page(vec![conflicting], None))]);
        let mut stream = native.profile_stream(input("id:42"), tab, server.base.clone());
        assert!(stream.next().await.unwrap().is_err());
        assert!(stream.next().await.is_none());
    }
}

#[tokio::test]
async fn pinned_profile_posts_are_opt_in_and_precede_regular_posts_when_enabled() {
    for pinned in [false, true] {
        let mut body = page(vec![tweet("101")], None);
        body["data"]["user"]["result"]["timeline"]["timeline"]["instructions"]
            .as_array_mut()
            .unwrap()
            .insert(
                0,
                json!({"type":"TimelinePinEntry","entry":entry(tweet("100"))}),
            );
        let mut responses = Vec::new();
        if pinned {
            responses.push(Response::json(user()));
        }
        responses.push(Response::json(body));
        let server = Server::new(responses);
        let native = extractor(Duration::from_secs(3)).await;
        let native = if pinned {
            native.with_profile_options(ProfileOptions {
                pinned,
                replies: ReplyMode::All,
            })
        } else {
            native
        };
        let items: Vec<_> = native
            .profile_stream(
                input("id:42"),
                timeline::ProfileTab::Tweets,
                server.base.clone(),
            )
            .try_collect()
            .await
            .unwrap();
        assert_eq!(
            post_ids(&items),
            if pinned {
                vec!["100", "101"]
            } else {
                vec!["101"]
            }
        );
    }
}

#[tokio::test]
async fn reply_modes_filter_profile_replies_using_the_requested_profile_owner() {
    for (mode, expected) in [
        (ReplyMode::All, vec!["100", "101", "102", "103"]),
        (ReplyMode::Skip, vec!["100"]),
        (ReplyMode::SelfOnly, vec!["100", "101", "103"]),
    ] {
        let server = Server::new(vec![Response::json(page(
            vec![
                tweet("100"),
                reply("101", "42", "99"),
                reply("102", "99", "42"),
                reply("103", "42", "42"),
            ],
            None,
        ))]);
        let native = extractor(Duration::from_secs(3))
            .await
            .with_profile_options(ProfileOptions {
                pinned: false,
                replies: mode,
            });
        let items: Vec<_> = native
            .profile_stream(
                input("id:42"),
                timeline::ProfileTab::Replies,
                server.base.clone(),
            )
            .try_collect()
            .await
            .unwrap();
        assert_eq!(post_ids(&items), expected);
    }
}

#[tokio::test]
async fn duplicate_media_across_pins_pages_and_quotes_is_suppressed_per_profile_input() {
    let mut first = page(vec![tweet("100"), quoted_tweet("101", "102")], Some("next"));
    first["data"]["user"]["result"]["timeline"]["timeline"]["instructions"]
        .as_array_mut()
        .unwrap()
        .insert(
            0,
            json!({"type":"TimelinePinEntry","entry":entry(tweet("100"))}),
        );
    let server = Server::new(vec![
        Response::json(user()),
        Response::json(first),
        Response::json(post_response(tweet("102"))),
        Response::json(page(vec![tweet("102"), tweet("103")], None)),
    ]);
    let native = extractor(Duration::from_secs(3))
        .await
        .with_expansion(expansion_options(RetweetMode::Skip))
        .with_profile_options(ProfileOptions {
            pinned: true,
            replies: ReplyMode::All,
        });
    let items: Vec<_> = native
        .profile_stream(
            input("id:42"),
            timeline::ProfileTab::Tweets,
            server.base.clone(),
        )
        .try_collect()
        .await
        .unwrap();
    assert_eq!(post_ids(&items), ["100", "101", "102", "103"]);
    assert_eq!(server.count(), 4);

    let second = Server::new(vec![
        Response::json(user()),
        Response::json(page(vec![tweet("100")], None)),
    ]);
    let items: Vec<_> = native
        .profile_stream(
            input("id:42"),
            timeline::ProfileTab::Tweets,
            second.base.clone(),
        )
        .try_collect()
        .await
        .unwrap();
    assert_eq!(post_ids(&items), ["100"]);
}

#[tokio::test]
async fn standalone_reply_filter_uses_the_posts_own_author_for_self_only() {
    for (mode, author, target, expected) in [
        (ReplyMode::All, "42", "99", true),
        (ReplyMode::Skip, "42", "42", false),
        (ReplyMode::SelfOnly, "42", "42", true),
        (ReplyMode::SelfOnly, "42", "99", true),
        (ReplyMode::SelfOnly, "99", "99", true),
    ] {
        let server = Server::new(vec![Response::json(post_response(reply(
            "100", author, target,
        )))]);
        let native = extractor(Duration::from_secs(3))
            .await
            .with_profile_options(ProfileOptions {
                pinned: false,
                replies: mode,
            });
        let items: Vec<_> = native
            .post_stream_at(local_post_endpoint(&server, "100"), "100".into())
            .try_collect()
            .await
            .unwrap();
        assert_eq!(items.len(), usize::from(expected));
        assert_eq!(server.count(), 1);
    }
}

#[tokio::test]
async fn inferred_page_pin_reorders_only_first_page_after_numeric_metadata_lookup() {
    let mut first = page(vec![tweet("100"), tweet("101")], Some("next"));
    first["data"]["user"]["result"]["legacy"]["pinned_tweet_ids_str"] = json!(["101"]);
    let mut second = page(vec![tweet("102"), tweet("103")], None);
    second["data"]["user"]["result"]["legacy"]["pinned_tweet_ids_str"] = json!(["103"]);
    let server = Server::new(vec![
        Response::json(user()),
        Response::json(first),
        Response::json(second),
    ]);
    let native = extractor(Duration::from_secs(3))
        .await
        .with_profile_options(ProfileOptions {
            pinned: true,
            replies: ReplyMode::All,
        });
    let items: Vec<_> = native
        .profile_stream(
            input("id:42"),
            timeline::ProfileTab::Tweets,
            server.base.clone(),
        )
        .try_collect()
        .await
        .unwrap();
    assert_eq!(post_ids(&items), ["101", "100", "102", "103"]);
    assert_eq!(server.count(), 3);
    let urls = server.urls();
    assert!(
        urls[0]
            .path()
            .ends_with("/8r5oa_2vD0WkhIAOkY4TTA/UserByRestId")
    );
    assert_eq!(variables(&urls[0])["userId"], "42");
    assert!(
        urls[1..]
            .iter()
            .all(|url| url.path().ends_with("/UserTweets"))
    );
}

#[tokio::test]
async fn inferred_lookup_pin_is_retained_when_first_profile_page_omits_pin_ids() {
    let mut lookup = user();
    lookup["data"]["user"]["result"]["legacy"]["pinned_tweet_ids_str"] = json!(["101"]);
    let server = Server::new(vec![
        Response::json(lookup),
        Response::json(page(vec![tweet("100"), tweet("101")], None)),
    ]);
    let native = extractor(Duration::from_secs(3))
        .await
        .with_profile_options(ProfileOptions {
            pinned: true,
            replies: ReplyMode::All,
        });
    let items: Vec<_> = native
        .profile_stream(
            input("fixture"),
            timeline::ProfileTab::Tweets,
            server.base.clone(),
        )
        .try_collect()
        .await
        .unwrap();
    assert_eq!(post_ids(&items), ["101", "100"]);
    assert_eq!(server.count(), 2);
    assert!(server.urls()[0].path().ends_with("/UserByScreenName"));
}

#[tokio::test]
async fn default_pin_setting_ignores_malformed_lookup_and_page_hints() {
    let mut lookup = user();
    lookup["data"]["user"]["result"]["legacy"]["pinned_tweet_ids_str"] =
        json!("private malformed pin hint");
    let mut body = page(vec![tweet("100"), tweet("101")], None);
    body["data"]["user"]["result"]["legacy"]["pinned_tweet_ids_str"] = json!([false]);
    let server = Server::new(vec![Response::json(lookup), Response::json(body)]);
    let native = extractor(Duration::from_secs(3)).await;
    let items: Vec<_> = native
        .profile_stream(
            input("fixture"),
            timeline::ProfileTab::Tweets,
            server.base.clone(),
        )
        .try_collect()
        .await
        .unwrap();
    assert_eq!(post_ids(&items), ["100", "101"]);
    assert_eq!(server.count(), 2);
}

#[tokio::test]
async fn enabled_pin_setting_rejects_malformed_hints_without_exposing_values() {
    for from_lookup in [false, true] {
        let mut body = if from_lookup {
            user()
        } else {
            page(vec![tweet("100")], None)
        };
        body["data"]["user"]["result"]["legacy"]["pinned_tweet_ids_str"] =
            json!(["private malformed pin hint"]);
        let mut responses = Vec::new();
        if !from_lookup {
            responses.push(Response::json(user()));
        }
        responses.push(Response::json(body));
        let server = Server::new(responses);
        let native = extractor(Duration::from_secs(3))
            .await
            .with_profile_options(ProfileOptions {
                pinned: true,
                replies: ReplyMode::All,
            });
        let mut stream = native.profile_stream(
            input(if from_lookup { "fixture" } else { "id:42" }),
            timeline::ProfileTab::Tweets,
            server.base.clone(),
        );
        let error = stream.next().await.unwrap().unwrap_err().to_string();
        assert!(error.contains("pinned"));
        assert!(!error.contains("private malformed pin hint"));
        assert!(stream.next().await.is_none());
        assert_eq!(server.count(), if from_lookup { 1 } else { 2 });
    }
}

#[tokio::test]
async fn numeric_pin_lookup_validates_owner_and_uses_metadata_pin_hint() {
    for matches_owner in [false, true] {
        let mut lookup = user();
        lookup["data"]["user"]["result"]["legacy"]["pinned_tweet_ids_str"] = json!(["101"]);
        if !matches_owner {
            lookup["data"]["user"]["result"]["rest_id"] = "99".into();
        }
        let server = Server::new(vec![
            Response::json(lookup),
            Response::json(page(vec![tweet("100"), tweet("101")], None)),
        ]);
        let native = extractor(Duration::from_secs(3))
            .await
            .with_profile_options(ProfileOptions {
                pinned: true,
                replies: ReplyMode::All,
            });
        let mut stream = native.profile_stream(
            input("id:42"),
            timeline::ProfileTab::Tweets,
            server.base.clone(),
        );
        assert_eq!(server.count(), 0);
        if matches_owner {
            let items: Vec<_> = stream.try_collect().await.unwrap();
            assert_eq!(post_ids(&items), ["101", "100"]);
            assert_eq!(server.count(), 2);
        } else {
            assert!(stream.next().await.unwrap().is_err());
            assert!(stream.next().await.is_none());
            assert_eq!(server.count(), 1);
        }
        assert!(server.urls()[0].path().ends_with("/UserByRestId"));
        assert_eq!(variables(&server.urls()[0])["userId"], "42");
    }
}

#[tokio::test]
async fn duplicate_post_batches_keep_first_media_siblings_and_skip_refreshed_extra_media() {
    let mut original = tweet("100");
    let first_photo = original["legacy"]["extended_entities"]["media"][0].clone();
    let mut second_photo = first_photo.clone();
    second_photo["media_url_https"] = "https://pbs.example/media/second.jpg".into();
    original["legacy"]["extended_entities"]["media"] = json!([first_photo, second_photo]);
    let mut refreshed = original.clone();
    let mut third_photo = refreshed["legacy"]["extended_entities"]["media"][0].clone();
    third_photo["media_url_https"] = "https://pbs.example/media/third.jpg".into();
    refreshed["legacy"]["extended_entities"]["media"]
        .as_array_mut()
        .unwrap()
        .push(third_photo);
    let server = Server::new(vec![
        Response::json(page(vec![original], Some("next"))),
        Response::json(page(vec![refreshed, tweet("101")], None)),
    ]);
    let native = extractor(Duration::from_secs(3)).await;
    let mut stream = native.profile_stream(
        input("id:42"),
        timeline::ProfileTab::Tweets,
        server.base.clone(),
    );
    let first = stream.next().await.unwrap().unwrap();
    let second = stream.next().await.unwrap().unwrap();
    assert_eq!(server.count(), 1);
    assert_eq!(first.metadata.as_ref().unwrap().post_id, "100");
    assert_eq!(first.metadata.as_ref().unwrap().index, 1);
    assert_eq!(second.metadata.as_ref().unwrap().post_id, "100");
    assert_eq!(second.metadata.as_ref().unwrap().index, 2);
    assert_ne!(first.url, second.url);
    let rest: Vec<_> = stream.try_collect().await.unwrap();
    assert_eq!(post_ids(&rest), ["101"]);
    assert_eq!(server.count(), 2);
}

fn search_page(tweets: Vec<Value>, cursor: Option<&str>) -> Value {
    let profile = page(tweets, cursor);
    json!({"data":{"search_by_raw_query":{"search_timeline":{"timeline":{
        "instructions":profile["data"]["user"]["result"]["timeline"]["timeline"]["instructions"].clone()
    }}}}})
}

fn text_tweet(id: &str) -> Value {
    let mut value = tweet(id);
    value["legacy"]
        .as_object_mut()
        .unwrap()
        .remove("extended_entities");
    value["legacy"]["entities"] = json!({});
    value
}

#[tokio::test]
async fn search_max_id_pagination_is_lazy_and_binds_operation_query_and_headers() {
    let server = Server::new(vec![
        Response::json(search_page(
            vec![tweet("103"), tweet("101")],
            Some("opaque-a"),
        )),
        Response::json(search_page(vec![tweet("99")], None)),
    ]);
    let native = extractor(Duration::from_secs(3)).await;
    let mut stream = native.search_stream("from:fixture filter:images".into(), server.base.clone());
    assert_eq!(server.count(), 0);
    let first = stream.next().await.unwrap().unwrap();
    assert_eq!(first.metadata.as_ref().unwrap().post_id, "103");
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(server.count(), 1);
    let second = stream.next().await.unwrap().unwrap();
    assert_eq!(second.metadata.as_ref().unwrap().post_id, "101");
    assert_eq!(server.count(), 1);
    let third = stream.next().await.unwrap().unwrap();
    assert_eq!(third.metadata.as_ref().unwrap().post_id, "99");
    assert!(stream.next().await.is_none());
    let urls = server.urls();
    assert_eq!(urls.len(), 2);
    assert!(urls.iter().all(|url| {
        url.path()
            .ends_with("/4fpceYZ6-YQCx_JSl_Cn_A/SearchTimeline")
    }));
    assert_eq!(
        variables(&urls[0])["rawQuery"],
        "from:fixture filter:images"
    );
    assert_eq!(
        variables(&urls[1])["rawQuery"],
        "from:fixture filter:images max_id:100"
    );
    for url in &urls {
        let vars = variables(url);
        assert_eq!(vars["count"], 20);
        assert_eq!(vars["querySource"], "typed_query");
        assert_eq!(vars["product"], "Latest");
        assert_eq!(vars["withGrokTranslatedBio"], false);
        assert!(vars.get("cursor").is_none_or(Value::is_null));
        assert!(!url.query_pairs().any(|(key, _)| key == "fieldToggles"));
    }
    assert_scoped_expansion_requests(&server, &native);
    assert!(
        !first
            .headers
            .keys()
            .any(|key| key.eq_ignore_ascii_case("cookie")
                || key.eq_ignore_ascii_case("x-client-transaction-id"))
    );
}

#[tokio::test]
async fn search_cursor_mode_preserves_opaque_cursor_and_selected_product() {
    let opaque = "cursor+/=&? opaque";
    for product in [
        SearchProduct::Top,
        SearchProduct::Media,
        SearchProduct::People,
        SearchProduct::Lists,
    ] {
        let server = Server::new(vec![
            Response::json(search_page(vec![tweet("103")], Some(opaque))),
            Response::json(search_page(vec![tweet("102")], None)),
        ]);
        let native = extractor(Duration::from_secs(3))
            .await
            .with_timeline_options(TimelineOptions {
                search_pagination: SearchPagination::Cursor,
                search_product: product,
                search_limit: 7,
                ..Default::default()
            });
        let items: Vec<_> = native
            .search_stream("fixture max_id:500".into(), server.base.clone())
            .try_collect()
            .await
            .unwrap();
        assert_eq!(post_ids(&items), ["103", "102"]);
        let urls = server.urls();
        assert_eq!(variables(&urls[1])["rawQuery"], "fixture max_id:500");
        assert_eq!(variables(&urls[1])["cursor"], opaque);
        assert_eq!(variables(&urls[1])["count"], 7);
        let expected = match product {
            SearchProduct::Top => "Top",
            SearchProduct::Media => "Media",
            SearchProduct::People => "People",
            SearchProduct::Lists => "Lists",
            SearchProduct::Latest => unreachable!(),
        };
        assert_eq!(variables(&urls[1])["product"], expected);
    }
}

#[tokio::test]
async fn search_max_id_replaces_existing_bound_and_deduplicates_posts() {
    let server = Server::new(vec![
        Response::json(search_page(vec![tweet("105"), tweet("103")], Some("a"))),
        Response::json(search_page(vec![tweet("103"), tweet("100")], None)),
    ]);
    let native = extractor(Duration::from_secs(3)).await;
    let items: Vec<_> = native
        .search_stream(
            "fixture max_id:999 filter:links".into(),
            server.base.clone(),
        )
        .try_collect()
        .await
        .unwrap();
    assert_eq!(post_ids(&items), ["105", "103", "100"]);
    let vars = variables(&server.urls()[1]);
    let query = vars["rawQuery"].as_str().unwrap();
    assert!(query.contains("max_id:102"));
    assert!(!query.contains("max_id:999"));
    assert_eq!(query.matches("max_id:").count(), 1);
}

#[tokio::test]
async fn empty_search_pages_advance_with_cursor_then_stop_at_configured_limit() {
    let server = Server::new(vec![
        Response::json(search_page(vec![], Some("a"))),
        Response::json(search_page(vec![], Some("b"))),
        Response::json(search_page(vec![], Some("c"))),
        Response::json(search_page(vec![], Some("d"))),
        Response::json(search_page(vec![tweet("100")], None)),
    ]);
    let native = extractor(Duration::from_secs(3)).await;
    let items: Vec<_> = native
        .search_stream("fixture".into(), server.base.clone())
        .try_collect()
        .await
        .unwrap();
    assert!(items.is_empty());
    assert_eq!(server.count(), 4);
    let urls = server.urls();
    assert_eq!(variables(&urls[1])["cursor"], "a");
    assert_eq!(variables(&urls[2])["cursor"], "b");
}

#[tokio::test]
async fn search_repeated_cursor_and_auth_failures_end_privately_after_prior_media() {
    let mut denied =
        Response::json(json!({"errors":[{"code":32,"message":"private search token marker"}]}));
    denied.status = 401;
    for response in [Response::json(search_page(vec![], Some("a"))), denied] {
        let server = Server::new(vec![
            Response::json(search_page(vec![tweet("100")], Some("a"))),
            response,
        ]);
        let native = extractor(Duration::from_secs(3))
            .await
            .with_timeline_options(TimelineOptions {
                search_pagination: SearchPagination::Cursor,
                ..Default::default()
            });
        let mut stream = native.search_stream("private query marker".into(), server.base.clone());
        assert!(stream.next().await.unwrap().is_ok());
        let error = stream.next().await.unwrap().unwrap_err().to_string();
        assert!(!error.contains("private search token marker"));
        assert!(!error.contains("private query marker"));
        assert!(stream.next().await.is_none());
        assert_eq!(server.count(), 2);
    }
}

#[tokio::test]
async fn search_range_cutoff_and_dropped_pending_request_do_not_follow_more_pages() {
    let server = Server::new(vec![
        Response::json(search_page(vec![tweet("100")], Some("a"))),
        Response::json(search_page(vec![tweet("99")], None)),
    ]);
    let native = extractor(Duration::from_secs(3)).await;
    let mut stream = crate::selection::RangeSelection::parse("1")
        .unwrap()
        .apply(native.search_stream("fixture".into(), server.base.clone()));
    assert!(stream.next().await.unwrap().is_ok());
    assert!(stream.next().await.is_none());
    assert_eq!(server.count(), 1);

    let server = Server::new(vec![
        Response::json(search_page(vec![tweet("100")], Some("a"))).delayed(300),
        Response::json(search_page(vec![tweet("99")], None)),
    ]);
    let mut stream = native.search_stream("fixture".into(), server.base.clone());
    {
        let next = stream.next();
        tokio::pin!(next);
        tokio::select! { result = &mut next => panic!("unexpected search response: {result:?}"), () = server.wait_for(1) => {} }
    }
    drop(stream);
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(server.count(), 1);
}

#[tokio::test]
async fn root_timeline_auto_uses_media_and_falls_back_only_after_draining_feed() {
    let server = Server::new(vec![
        Response::json(user()),
        Response::json(page(vec![tweet("103"), tweet("101")], None)),
        Response::json(search_page(vec![tweet("101"), tweet("99")], None)),
    ]);
    let native = extractor(Duration::from_secs(3)).await;
    let mut stream = native.timeline_stream(input("fixture"), server.base.clone());
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
        "103"
    );
    assert_eq!(server.count(), 2);
    tokio::time::sleep(Duration::from_millis(20)).await;
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
        "101"
    );
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
        "99"
    );
    assert!(stream.next().await.is_none());
    assert_eq!(server.count(), 3);
    let urls = server.urls();
    assert!(urls[1].path().ends_with("/UserMedia"));
    assert_eq!(
        variables(&urls[2])["rawQuery"],
        "from:fixture max_id:101 filter:links"
    );
    assert_scoped_expansion_requests(&server, &native);
}

#[tokio::test]
async fn root_timeline_strategy_selects_explicit_feed_and_retweet_search_modifiers() {
    for (strategy, mode, expected) in [
        (TimelineStrategy::Auto, RetweetMode::Repost, "/UserTweets"),
        (TimelineStrategy::Media, RetweetMode::Original, "/UserMedia"),
        (TimelineStrategy::Tweets, RetweetMode::Skip, "/UserTweets"),
        (
            TimelineStrategy::Replies,
            RetweetMode::Skip,
            "/UserTweetsAndReplies",
        ),
    ] {
        let server = Server::new(vec![
            Response::json(user()),
            Response::json(page(vec![tweet("100")], None)),
            Response::json(search_page(vec![tweet("99")], None)),
        ]);
        let native = extractor(Duration::from_secs(3))
            .await
            .with_timeline_options(TimelineOptions {
                strategy,
                ..Default::default()
            })
            .with_expansion(ExpansionOptions {
                retweets: mode,
                quoted: false,
            });
        let items: Vec<_> = native
            .timeline_stream(input("fixture"), server.base.clone())
            .try_collect()
            .await
            .unwrap();
        assert_eq!(post_ids(&items), ["100", "99"]);
        let urls = server.urls();
        assert!(urls[1].path().ends_with(expected));
        let vars = variables(&urls[2]);
        let query = vars["rawQuery"].as_str().unwrap();
        assert_eq!(
            query.contains("include:retweets include:nativeretweets"),
            mode != RetweetMode::Skip
        );
    }
}

#[tokio::test]
async fn root_timeline_empty_feed_stops_but_empty_link_search_widens_once() {
    let server = Server::new(vec![
        Response::json(user()),
        Response::json(page(vec![], None)),
        Response::json(search_page(vec![tweet("100")], None)),
    ]);
    let native = extractor(Duration::from_secs(3)).await;
    let items: Vec<_> = native
        .timeline_stream(input("fixture"), server.base.clone())
        .try_collect()
        .await
        .unwrap();
    assert!(items.is_empty());
    assert_eq!(server.count(), 2);

    let server = Server::new(vec![
        Response::json(user()),
        Response::json(page(vec![tweet("100")], None)),
        Response::json(search_page(vec![], None)),
        Response::json(search_page(vec![tweet("99")], None)),
    ]);
    let items: Vec<_> = native
        .timeline_stream(input("fixture"), server.base.clone())
        .try_collect()
        .await
        .unwrap();
    assert_eq!(post_ids(&items), ["100", "99"]);
    let urls = server.urls();
    assert_eq!(server.count(), 4);
    assert_eq!(
        variables(&urls[2])["rawQuery"],
        "from:fixture max_id:100 filter:links"
    );
    assert_eq!(variables(&urls[3])["rawQuery"], "from:fixture max_id:100");
}

#[tokio::test]
async fn root_timeline_text_posts_anchor_fallback_and_nonempty_text_search_prevents_widening() {
    let server = Server::new(vec![
        Response::json(user()),
        Response::json(page(vec![tweet("103"), text_tweet("101")], None)),
        Response::json(search_page(vec![text_tweet("99")], None)),
        Response::json(search_page(vec![tweet("98")], None)),
    ]);
    let native = extractor(Duration::from_secs(3)).await;
    let items: Vec<_> = native
        .timeline_stream(input("fixture"), server.base.clone())
        .try_collect()
        .await
        .unwrap();
    assert_eq!(post_ids(&items), ["103"]);
    assert_eq!(server.count(), 3);
    assert_eq!(
        variables(&server.urls()[2])["rawQuery"],
        "from:fixture max_id:101 filter:links"
    );
}

#[tokio::test]
async fn root_numeric_lookup_binds_owner_and_canonical_screen_name_for_fallback() {
    let server = Server::new(vec![
        Response::json(user()),
        Response::json(page(vec![tweet("100")], None)),
        Response::json(search_page(vec![tweet("99")], None)),
    ]);
    let native = extractor(Duration::from_secs(3)).await;
    let items: Vec<_> = native
        .timeline_stream(input("id:42"), server.base.clone())
        .try_collect()
        .await
        .unwrap();
    assert_eq!(post_ids(&items), ["100", "99"]);
    let urls = server.urls();
    assert!(urls[0].path().ends_with("/UserByRestId"));
    assert_eq!(variables(&urls[0])["userId"], "42");
    assert_eq!(
        variables(&urls[2])["rawQuery"],
        "from:fixture max_id:100 filter:links"
    );

    let mut invalid = user();
    invalid["data"]["user"]["result"]["core"]["screen_name"] = "foreign".into();
    let server = Server::new(vec![
        Response::json(invalid),
        Response::json(page(vec![tweet("100")], None)),
    ]);
    let mut stream = native.timeline_stream(input("id:42"), server.base.clone());
    assert!(stream.next().await.unwrap().is_err());
    assert!(stream.next().await.is_none());
    assert_eq!(server.count(), 1);
}

#[tokio::test]
async fn root_timeline_range_cutoff_prevents_fallback_request() {
    let server = Server::new(vec![
        Response::json(user()),
        Response::json(page(vec![tweet("100")], None)),
        Response::json(search_page(vec![tweet("99")], None)),
    ]);
    let native = extractor(Duration::from_secs(3)).await;
    let mut selected = crate::selection::RangeSelection::parse("1")
        .unwrap()
        .apply(native.timeline_stream(input("fixture"), server.base.clone()));
    assert!(selected.next().await.unwrap().is_ok());
    assert!(selected.next().await.is_none());
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(server.count(), 2);
}

#[tokio::test]
async fn search_date_pagination_uses_snowflake_timestamp_and_cursor_when_timestamp_repeats() {
    // Snowflakes encode milliseconds since Twitter's documented 2010 epoch.
    let snowflake = |milliseconds: u64| ((milliseconds - 1_288_834_974_657) << 22).to_string();
    let first = snowflake(1_704_196_800_000); // 2024-01-02 12:00 UTC.
    let second = snowflake(1_704_193_200_000); // Same UTC day.
    let third = snowflake(1_704_110_400_000); // 2024-01-01 12:00 UTC.
    let server = Server::new(vec![
        Response::json(search_page(vec![tweet(&first)], Some("day-a"))),
        Response::json(search_page(vec![tweet(&second)], Some("day-b"))),
        Response::json(search_page(vec![tweet(&second)], Some("day-c"))),
        Response::json(search_page(vec![tweet(&third)], None)),
    ]);
    let native = extractor(Duration::from_secs(3))
        .await
        .with_timeline_options(TimelineOptions {
            search_pagination: SearchPagination::Date,
            ..Default::default()
        });
    let items: Vec<_> = native
        .search_stream("fixture".into(), server.base.clone())
        .try_collect()
        .await
        .unwrap();
    assert_eq!(
        post_ids(&items),
        [first.as_str(), second.as_str(), third.as_str()]
    );
    let urls = server.urls();
    assert_eq!(variables(&urls[1])["rawQuery"], "fixture until:2024-01-02");
    assert!(variables(&urls[1]).get("cursor").is_none_or(Value::is_null));
    assert_eq!(variables(&urls[2])["rawQuery"], "fixture until:2024-01-02");
    assert!(variables(&urls[2]).get("cursor").is_none_or(Value::is_null));
    assert_eq!(variables(&urls[3])["rawQuery"], "fixture until:2024-01-02");
    assert_eq!(variables(&urls[3])["cursor"], "day-c");
    assert_eq!(urls.len(), 4);
}

#[tokio::test]
async fn root_timeline_filtered_reply_still_anchors_search_to_last_raw_post() {
    let server = Server::new(vec![
        Response::json(user()),
        Response::json(page(vec![tweet("103"), reply("101", "42", "99")], None)),
        Response::json(search_page(vec![tweet("99")], None)),
    ]);
    let native = extractor(Duration::from_secs(3))
        .await
        .with_profile_options(ProfileOptions {
            replies: ReplyMode::Skip,
            pinned: false,
        })
        .with_timeline_options(TimelineOptions {
            strategy: TimelineStrategy::Replies,
            ..Default::default()
        });
    let items: Vec<_> = native
        .timeline_stream(input("fixture"), server.base.clone())
        .try_collect()
        .await
        .unwrap();
    assert_eq!(post_ids(&items), ["103", "99"]);
    assert_eq!(
        variables(&server.urls()[2])["rawQuery"],
        "from:fixture max_id:101 filter:links"
    );
}

#[tokio::test]
async fn root_fallback_shares_active_budget_while_excluding_consumer_pause() {
    let server = Server::new(vec![
        Response::json(user()),
        Response::json(page(vec![tweet("100")], None)).delayed(20),
        Response::json(search_page(vec![tweet("99")], None)).delayed(20),
    ]);
    let native = extractor(Duration::from_millis(250)).await;
    let mut stream = native.timeline_stream(input("fixture"), server.base.clone());
    assert!(stream.next().await.unwrap().is_ok());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(stream.next().await.unwrap().is_ok());
    assert!(stream.next().await.is_none());

    let server = Server::new(vec![
        Response::json(user()),
        Response::json(page(vec![tweet("100")], None)).delayed(150),
        Response::json(search_page(vec![tweet("99")], None)).delayed(180),
    ]);
    let mut stream = native.timeline_stream(input("fixture"), server.base.clone());
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
    assert_eq!(server.count(), 3);
}

#[tokio::test]
async fn standalone_self_only_search_resolves_from_profile_and_reuses_rotated_cookies() {
    let mut lookup = Response::json(user());
    lookup.headers =
        "Set-Cookie: ct0=searchrotated; Path=/\r\nSet-Cookie: auth_token=searchsession; Path=/\r\n"
            .into();
    let server = Server::new(vec![
        lookup,
        Response::json(search_page(
            vec![reply("103", "42", "99"), reply("102", "99", "42")],
            Some("next"),
        )),
        Response::json(search_page(vec![tweet("100")], None)),
    ]);
    let native = extractor(Duration::from_secs(3))
        .await
        .with_profile_options(ProfileOptions {
            pinned: false,
            replies: ReplyMode::SelfOnly,
        });
    let items: Vec<_> = native
        .search_stream("from:fixture".into(), server.base.clone())
        .try_collect()
        .await
        .unwrap();
    assert_eq!(post_ids(&items), ["103", "100"]);
    let urls = server.urls();
    assert_eq!(urls.len(), 3);
    assert!(urls[0].path().ends_with("/UserByScreenName"));
    assert!(
        urls[1..]
            .iter()
            .all(|url| url.path().ends_with("/SearchTimeline"))
    );
    assert_eq!(variables(&urls[2])["rawQuery"], "from:fixture max_id:101");
    let requests = server.requests.lock().unwrap();
    for request in &requests[1..] {
        let lower = request.to_ascii_lowercase();
        assert!(lower.contains("x-csrf-token: searchrotated\r\n"));
        assert!(lower.contains("auth_token=searchsession"));
        assert!(!lower.contains("auth_token=session;"));
    }
    for item in items {
        assert!(
            !item
                .headers
                .values()
                .any(|value| value.contains("searchrotated") || value.contains("searchsession"))
        );
    }
}

#[tokio::test]
async fn standalone_self_only_search_rejects_foreign_from_profile_lookup() {
    let mut lookup = user();
    lookup["data"]["user"]["result"]["legacy"]["screen_name"] = "foreign".into();
    let server = Server::new(vec![
        Response::json(lookup),
        Response::json(search_page(vec![tweet("100")], None)),
    ]);
    let native = extractor(Duration::from_secs(3))
        .await
        .with_profile_options(ProfileOptions {
            pinned: false,
            replies: ReplyMode::SelfOnly,
        });
    let mut stream = native.search_stream("from:fixture".into(), server.base.clone());
    let error = stream.next().await.unwrap().unwrap_err().to_string();
    assert!(!error.contains("foreign"));
    assert!(stream.next().await.is_none());
    assert_eq!(server.count(), 1);
}

#[tokio::test]
async fn search_explicit_continue_empty_response_ignores_zero_empty_page_limit() {
    let mut empty = search_page(vec![], Some("continue-empty"));
    empty["data"]["search_by_raw_query"]["search_timeline"]["timeline"]["instructions"][0]["entries"]
        [0]["content"]["stopOnEmptyResponse"] = false.into();
    let server = Server::new(vec![
        Response::json(empty),
        Response::json(search_page(vec![tweet("100")], None)),
    ]);
    let native = extractor(Duration::from_secs(3))
        .await
        .with_timeline_options(TimelineOptions {
            search_stop: 0,
            ..Default::default()
        });
    let items: Vec<_> = native
        .search_stream("fixture".into(), server.base.clone())
        .try_collect()
        .await
        .unwrap();
    assert_eq!(post_ids(&items), ["100"]);
    assert_eq!(server.count(), 2);
    assert_eq!(variables(&server.urls()[1])["cursor"], "continue-empty");
}

fn feed_page(kind: &collections::Collection, tweets: Vec<Value>, cursor: Option<&str>) -> Value {
    let source = page(tweets, cursor);
    let instructions =
        source["data"]["user"]["result"]["timeline"]["timeline"]["instructions"].clone();
    match kind {
        collections::Collection::Bookmarks => {
            json!({"data":{"bookmark_timeline_v2":{"timeline":{"instructions":instructions}}}})
        }
        collections::Collection::List(_) => {
            json!({"data":{"list":{"tweets_timeline":{"timeline":{"instructions":instructions}}}}})
        }
        collections::Collection::Home | collections::Collection::HomeLatest => {
            json!({"data":{"home":{"home_timeline_urt":{"instructions":instructions}}}})
        }
    }
}
fn other_author(mut post: Value) -> Value {
    post["legacy"]["user_id_str"] = "99".into();
    post["core"]["user_results"]["result"]["rest_id"] = "99".into();
    post
}

#[tokio::test]
async fn feed_operations_are_lazy_cursor_bound_and_keep_original_media_identity() {
    use collections::Collection;
    for (kind, operation) in [
        (Collection::Bookmarks, "/pLtjrO4ubNh996M_Cubwsg/Bookmarks"),
        (
            Collection::List("55".into()),
            "/06JtmwM8k_1cthpFZITVVA/ListLatestTweetsTimeline",
        ),
        (
            Collection::HomeLatest,
            "/SFxmNKWfN9ySJcXG_tjX8g/HomeLatestTimeline",
        ),
        (Collection::Home, "/DXmgQYmIft1oLP6vMkJixw/HomeTimeline"),
    ] {
        let server = Server::new(vec![
            Response::json(feed_page(
                &kind,
                vec![tweet("100"), other_author(tweet("101"))],
                Some("opaque+/=?"),
            )),
            Response::json(feed_page(&kind, vec![tweet("101"), tweet("102")], None)),
        ]);
        let native = extractor(Duration::from_secs(3))
            .await
            .with_timeline_options(TimelineOptions {
                limit: 7,
                ..Default::default()
            });
        let mut stream = native.feed_stream(kind.clone(), server.base.clone());
        assert_eq!(server.count(), 0);
        let first = stream.next().await.unwrap().unwrap();
        assert_eq!(first.metadata.as_ref().unwrap().post_id, "100");
        tokio::time::sleep(Duration::from_millis(15)).await;
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
            "101"
        );
        assert_eq!(server.count(), 1);
        let rest: Vec<_> = stream.try_collect().await.unwrap();
        assert_eq!(post_ids(&rest), ["102"]);
        let direct = parse_response(
            &serde_json::to_vec(&post_response(tweet("100"))).unwrap(),
            "100",
        )
        .unwrap();
        assert_eq!(first.archive_key(), direct[0].archive_key());
        let urls = server.urls();
        assert_eq!(urls.len(), 2);
        assert!(urls.iter().all(|url| url.path().ends_with(operation)));
        let mut expected = match kind {
            Collection::Bookmarks => json!({"count":7,"includePromotedContent":false}),
            Collection::List(_) => json!({"listId":"55","count":7}),
            Collection::HomeLatest => {
                json!({"count":7,"includePromotedContent":false,"latestControlAvailable":true})
            }
            Collection::Home => {
                json!({"count":7,"includePromotedContent":false,"latestControlAvailable":true,"withCommunity":true})
            }
        };
        assert_eq!(variables(&urls[0]), expected);
        expected["cursor"] = "opaque+/=?".into();
        assert_eq!(variables(&urls[1]), expected);
        assert!(
            urls.iter()
                .all(|url| !url.query_pairs().any(|(key, _)| key == "fieldToggles"))
        );
        assert_scoped_expansion_requests(&server, &native);
    }
}

#[tokio::test]
async fn likes_resolves_only_named_owner_accepts_other_authors_and_binds_requested_root() {
    for named in [false, true] {
        let mut responses = Vec::new();
        if named {
            responses.push(Response::json(user()));
        }
        responses.push(Response::json(page(
            vec![other_author(tweet("100")), tweet("101")],
            None,
        )));
        let server = Server::new(responses);
        let native = extractor(Duration::from_secs(3))
            .await
            .with_timeline_options(TimelineOptions {
                limit: 13,
                ..Default::default()
            });
        let items: Vec<_> = native
            .profile_stream(
                input(if named { "fixture" } else { "id:42" }),
                timeline::ProfileTab::Likes,
                server.base.clone(),
            )
            .try_collect()
            .await
            .unwrap();
        assert_eq!(post_ids(&items), ["100", "101"]);
        let urls = server.urls();
        assert_eq!(urls.len(), if named { 2 } else { 1 });
        let url = urls.last().unwrap();
        assert!(url.path().ends_with("/TGEKkJG_meudeaFcqaxM-Q/Likes"));
        assert_eq!(
            variables(url),
            json!({"userId":"42","count":13,"includePromotedContent":false,"withClientEventToken":false,"withBirdwatchNotes":false,"withVoice":true})
        );
        let toggles: Value = serde_json::from_str(
            &url.query_pairs()
                .find(|(key, _)| key == "fieldToggles")
                .unwrap()
                .1,
        )
        .unwrap();
        assert_eq!(toggles, json!({"withArticlePlainText":false}));
    }
    let mut wrong = page(vec![other_author(tweet("100"))], None);
    wrong["data"]["user"]["result"]["rest_id"] = "99".into();
    let server = Server::new(vec![Response::json(wrong)]);
    let native = extractor(Duration::from_secs(3)).await;
    let mut stream = native.profile_stream(
        input("id:42"),
        timeline::ProfileTab::Likes,
        server.base.clone(),
    );
    assert!(stream.next().await.unwrap().is_err());
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn collection_range_and_drop_prevent_followup_quotes_and_pages() {
    use collections::Collection;
    let server = Server::new(vec![
        Response::json(feed_page(
            &Collection::Bookmarks,
            vec![quoted_tweet("100", "101")],
            Some("next"),
        )),
        Response::json(post_response(tweet("101"))),
    ]);
    let native = extractor(Duration::from_secs(3))
        .await
        .with_expansion(expansion_options(RetweetMode::Skip));
    let mut selected = crate::selection::RangeSelection::parse("1")
        .unwrap()
        .apply(native.feed_stream(Collection::Bookmarks, server.base.clone()));
    assert!(selected.next().await.unwrap().is_ok());
    assert!(selected.next().await.is_none());
    assert_eq!(server.count(), 1);
    let server = Server::new(vec![
        Response::json(feed_page(
            &Collection::HomeLatest,
            vec![tweet("100")],
            Some("next"),
        ))
        .delayed(250),
        Response::json(feed_page(&Collection::HomeLatest, vec![tweet("101")], None)),
    ]);
    let mut stream = native.feed_stream(Collection::HomeLatest, server.base.clone());
    {
        let pending = stream.next();
        tokio::pin!(pending);
        tokio::select! { result = &mut pending => panic!("unexpected completion: {result:?}"), () = server.wait_for(1) => {} }
    }
    drop(stream);
    tokio::time::sleep(Duration::from_millis(15)).await;
    assert_eq!(server.count(), 1);
}

#[tokio::test]
async fn collection_late_auth_identity_and_cursor_errors_are_terminal_and_private() {
    use collections::Collection;
    let mut contradictory = tweet("101");
    contradictory["legacy"]["user_id_str"] = "99".into();
    let mut auth =
        Response::json(json!({"errors":[{"code":32,"message":"private collection token"}]}));
    auth.status = 401;
    for response in [
        auth,
        Response::json(feed_page(&Collection::Bookmarks, vec![contradictory], None)),
        Response::json(feed_page(
            &Collection::Bookmarks,
            vec![tweet("101")],
            Some("a"),
        )),
    ] {
        let server = Server::new(vec![
            Response::json(feed_page(
                &Collection::Bookmarks,
                vec![tweet("100")],
                Some("a"),
            )),
            response,
        ]);
        let native = extractor(Duration::from_secs(3)).await;
        let mut stream = native.feed_stream(Collection::Bookmarks, server.base.clone());
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
        assert!(!error.contains("private collection token"));
        assert!(stream.next().await.is_none());
        assert_eq!(server.count(), 2);
    }
}

#[tokio::test]
async fn bookmark_empty_continuations_are_bounded_while_other_feeds_stop_on_empty() {
    use collections::Collection;
    let mut responses: Vec<_> = (0..129)
        .map(|i| {
            Response::json(feed_page(
                &Collection::Bookmarks,
                vec![],
                Some(&format!("cursor-{i}")),
            ))
        })
        .collect();
    responses.push(Response::json(feed_page(
        &Collection::Bookmarks,
        vec![tweet("100")],
        None,
    )));
    let server = Server::new(responses);
    let native = extractor(Duration::from_secs(5)).await;
    let items: Vec<_> = native
        .feed_stream(Collection::Bookmarks, server.base.clone())
        .try_collect()
        .await
        .unwrap();
    assert!(items.is_empty());
    assert_eq!(server.count(), 129);
    for kind in [
        Collection::List("55".into()),
        Collection::HomeLatest,
        Collection::Home,
    ] {
        let server = Server::new(vec![
            Response::json(feed_page(&kind, vec![], Some("next"))),
            Response::json(feed_page(&kind, vec![tweet("100")], None)),
        ]);
        let items: Vec<_> = native
            .feed_stream(kind, server.base.clone())
            .try_collect()
            .await
            .unwrap();
        assert!(items.is_empty());
        assert_eq!(server.count(), 1);
    }
}

#[tokio::test]
async fn collection_explicit_continue_empty_hint_and_explicit_termination_are_honored() {
    use collections::Collection;
    let kind = Collection::HomeLatest;
    let mut empty = feed_page(&kind, vec![], Some("a"));
    empty["data"]["home"]["home_timeline_urt"]["instructions"][0]["entries"][0]["content"]["stopOnEmptyResponse"] =
        false.into();
    let mut final_page = feed_page(&kind, vec![tweet("100")], Some("a"));
    final_page["data"]["home"]["home_timeline_urt"]["instructions"]
        .as_array_mut()
        .unwrap()
        .push(json!({"type":"TimelineTerminateTimeline","direction":"Bottom"}));
    let server = Server::new(vec![Response::json(empty), Response::json(final_page)]);
    let native = extractor(Duration::from_secs(3)).await;
    let items: Vec<_> = native
        .feed_stream(kind, server.base.clone())
        .try_collect()
        .await
        .unwrap();
    assert_eq!(post_ids(&items), ["100"]);
    assert_eq!(server.count(), 2);
}

#[tokio::test]
async fn collection_active_budget_excludes_pause_but_accumulates_pages_and_quotes() {
    use collections::Collection;
    let native = extractor(Duration::from_millis(250))
        .await
        .with_expansion(expansion_options(RetweetMode::Skip));
    let server = Server::new(vec![
        Response::json(feed_page(
            &Collection::Bookmarks,
            vec![tweet("100")],
            Some("next"),
        ))
        .delayed(20),
        Response::json(feed_page(&Collection::Bookmarks, vec![tweet("101")], None)).delayed(20),
    ]);
    let mut stream = native.feed_stream(Collection::Bookmarks, server.base.clone());
    assert!(stream.next().await.unwrap().is_ok());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(stream.next().await.unwrap().is_ok());
    assert!(stream.next().await.is_none());
    let server = Server::new(vec![
        Response::json(feed_page(
            &Collection::Bookmarks,
            vec![quoted_tweet("100", "101")],
            Some("next"),
        ))
        .delayed(150),
        Response::json(post_response(tweet("101"))).delayed(180),
        Response::json(feed_page(&Collection::Bookmarks, vec![tweet("102")], None)),
    ]);
    let mut stream = native.feed_stream(Collection::Bookmarks, server.base.clone());
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
    assert_eq!(server.count(), 2);
}

#[tokio::test]
async fn collection_cookie_rotation_uses_actual_operation_paths_and_never_leaks_to_media() {
    use collections::Collection;
    let mut first = Response::json(feed_page(
        &Collection::Bookmarks,
        vec![quoted_tweet("100", "101")],
        Some("next"),
    ));
    first.headers =
        "Set-Cookie: ct0=feedrotated; Path=/\r\nSet-Cookie: auth_token=feedsession; Path=/\r\n"
            .into();
    let server = Server::new(vec![
        first,
        Response::json(post_response(tweet("101"))),
        Response::json(feed_page(&Collection::Bookmarks, vec![tweet("102")], None)),
    ]);
    let network = NetworkOptions::new(None, Some("gallery-dl-feed-test/1".into())).unwrap();
    let native = extractor(Duration::from_secs(3))
        .await
        .with_network(&network)
        .unwrap()
        .with_expansion(expansion_options(RetweetMode::Skip));
    let items: Vec<_> = native
        .feed_stream(Collection::Bookmarks, server.base.clone())
        .try_collect()
        .await
        .unwrap();
    assert_eq!(post_ids(&items), ["100", "101", "102"]);
    let urls = server.urls();
    assert!(urls[0].path().ends_with("/Bookmarks"));
    assert!(urls[1].path().ends_with("/TweetResultByRestId"));
    assert!(urls[2].path().ends_with("/Bookmarks"));
    let requests = server.requests.lock().unwrap();
    for (index, request) in requests.iter().enumerate() {
        let lower = request.to_ascii_lowercase();
        assert!(lower.contains("user-agent: gallery-dl-feed-test/1\r\n"));
        if index == 0 {
            assert!(lower.contains("x-csrf-token: csrf\r\n"));
        } else {
            assert!(lower.contains("x-csrf-token: feedrotated\r\n"));
            assert!(lower.contains("auth_token=feedsession"));
            assert!(!lower.contains("auth_token=session;"));
        }
        let encoded = request
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("x-client-transaction-id")
                    .then(|| value.trim())
            })
            .unwrap();
        let bytes = STANDARD_NO_PAD.decode(encoded).unwrap();
        let transaction = native.transaction.lock().unwrap();
        let transaction = transaction.as_ref().unwrap();
        let mask = bytes[0];
        let offset = 1 + transaction.key.len();
        let seconds = u32::from_le_bytes(
            bytes[offset..offset + 4]
                .iter()
                .map(|byte| byte ^ mask)
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
    for media in items {
        assert_eq!(
            media.headers.get("User-Agent").map(String::as_str),
            Some("gallery-dl-feed-test/1")
        );
        assert!(!media.headers.keys().any(|name| {
            [
                "cookie",
                "authorization",
                "x-csrf-token",
                "x-client-transaction-id",
            ]
            .iter()
            .any(|secret| name.eq_ignore_ascii_case(secret))
        }));
        assert!(
            !media
                .headers
                .values()
                .any(|value| value.contains("feedsession") || value.contains("feedrotated"))
        );
    }
}

#[tokio::test]
async fn collection_pins_are_opt_in_first_page_only_and_share_deduplication() {
    use collections::Collection;
    for pinned in [false, true] {
        let mut first = feed_page(&Collection::HomeLatest, vec![tweet("101")], Some("next"));
        first["data"]["home"]["home_timeline_urt"]["instructions"]
            .as_array_mut()
            .unwrap()
            .insert(
                0,
                json!({"type":"TimelinePinEntry","entry":entry(tweet("100"))}),
            );
        let mut second = feed_page(
            &Collection::HomeLatest,
            vec![tweet("100"), tweet("102")],
            None,
        );
        second["data"]["home"]["home_timeline_urt"]["instructions"]
            .as_array_mut()
            .unwrap()
            .insert(
                0,
                json!({"type":"TimelinePinEntry","entry":entry(tweet("103"))}),
            );
        let server = Server::new(vec![Response::json(first), Response::json(second)]);
        let native = extractor(Duration::from_secs(3))
            .await
            .with_profile_options(ProfileOptions {
                pinned,
                replies: ReplyMode::All,
            });
        let items: Vec<_> = native
            .feed_stream(Collection::HomeLatest, server.base.clone())
            .try_collect()
            .await
            .unwrap();
        assert_eq!(
            post_ids(&items),
            if pinned {
                vec!["100", "101", "102"]
            } else {
                vec!["101", "100", "102"]
            }
        );
        assert_eq!(server.count(), 2);
    }
}

#[tokio::test]
async fn collection_retweet_modes_and_quote_expansion_preserve_underlying_archive_keys() {
    use collections::Collection;
    for mode in [
        RetweetMode::Skip,
        RetweetMode::Repost,
        RetweetMode::Original,
    ] {
        let mut retweet = other_author(tweet("100"));
        retweet["legacy"]
            .as_object_mut()
            .unwrap()
            .remove("extended_entities");
        retweet["legacy"]["retweeted_status_id_str"] = "101".into();
        retweet["retweeted_status_result"] = json!({"result":tweet("101")});
        let server = Server::new(vec![
            Response::json(feed_page(
                &Collection::Bookmarks,
                vec![retweet, quoted_tweet("102", "103")],
                None,
            )),
            Response::json(post_response(other_author(tweet("103")))),
        ]);
        let native = extractor(Duration::from_secs(3))
            .await
            .with_expansion(expansion_options(mode));
        let items: Vec<_> = native
            .feed_stream(Collection::Bookmarks, server.base.clone())
            .try_collect()
            .await
            .unwrap();
        let expected = match mode {
            RetweetMode::Skip => vec!["102", "103"],
            RetweetMode::Repost => vec!["100", "102", "103"],
            RetweetMode::Original => vec!["101", "102", "103"],
        };
        assert_eq!(post_ids(&items), expected);
        for item in &items {
            let id = &item.metadata.as_ref().unwrap().post_id;
            let direct =
                parse_response(&serde_json::to_vec(&post_response(tweet(id))).unwrap(), id)
                    .unwrap();
            assert_eq!(item.archive_key(), direct[0].archive_key());
        }
        assert_eq!(
            items
                .last()
                .unwrap()
                .metadata
                .as_ref()
                .unwrap()
                .quoted_by_id
                .as_deref(),
            Some("102")
        );
        assert_eq!(server.count(), 2);
    }
}

#[tokio::test]
async fn search_pins_are_opt_in_and_only_apply_to_the_first_page() {
    for pinned in [false, true] {
        let mut first = search_page(vec![tweet("103")], Some("next"));
        first["data"]["search_by_raw_query"]["search_timeline"]["timeline"]["instructions"]
            .as_array_mut()
            .unwrap()
            .insert(
                0,
                json!({"type":"TimelinePinEntry","entry":entry(tweet("104"))}),
            );
        let mut second = search_page(vec![tweet("100")], None);
        second["data"]["search_by_raw_query"]["search_timeline"]["timeline"]["instructions"]
            .as_array_mut()
            .unwrap()
            .insert(
                0,
                json!({"type":"TimelinePinEntry","entry":entry(tweet("101"))}),
            );
        let server = Server::new(vec![Response::json(first), Response::json(second)]);
        let native = extractor(Duration::from_secs(3))
            .await
            .with_profile_options(ProfileOptions {
                pinned,
                replies: ReplyMode::All,
            })
            .with_timeline_options(TimelineOptions {
                search_pagination: SearchPagination::Cursor,
                ..Default::default()
            });
        let items: Vec<_> = native
            .search_stream("fixture".into(), server.base.clone())
            .try_collect()
            .await
            .unwrap();
        assert_eq!(
            post_ids(&items),
            if pinned {
                vec!["104", "103", "100"]
            } else {
                vec!["103", "100"]
            }
        );
        assert_eq!(server.count(), 2);
        assert_eq!(variables(&server.urls()[1])["cursor"], "next");
    }
}

#[tokio::test]
async fn likes_empty_pages_stop_unless_cursor_explicitly_permits_continuation() {
    for continue_empty in [false, true] {
        let mut empty = page(vec![], Some("next"));
        if continue_empty {
            empty["data"]["user"]["result"]["timeline"]["timeline"]["instructions"][0]["entries"]
                [0]["content"]["stopOnEmptyResponse"] = false.into();
        }
        let server = Server::new(vec![
            Response::json(empty),
            Response::json(page(vec![other_author(tweet("100"))], None)),
        ]);
        let native = extractor(Duration::from_secs(3)).await;
        let items: Vec<_> = native
            .profile_stream(
                input("id:42"),
                timeline::ProfileTab::Likes,
                server.base.clone(),
            )
            .try_collect()
            .await
            .unwrap();
        assert_eq!(
            post_ids(&items),
            if continue_empty { vec!["100"] } else { vec![] }
        );
        assert_eq!(server.count(), if continue_empty { 2 } else { 1 });
    }
}

fn detail_page(tweets: Vec<Value>, cursor: Option<&str>) -> Value {
    let mut entries: Vec<_> = tweets.into_iter().map(entry).collect();
    if let Some(cursor) = cursor {
        entries.push(json!({"entryId":"cursor-bottom-detail","content":{"entryType":"TimelineTimelineCursor","cursorType":"Bottom","value":cursor}}));
    }
    json!({"data":{"threaded_conversation_with_injections_v2":{"instructions":[{"type":"TimelineAddEntries","entries":entries}]}}})
}
fn conversation_options(conversations: ConversationMode) -> StatusOptions {
    StatusOptions {
        endpoint: TweetEndpoint::Detail,
        conversations,
    }
}

#[tokio::test]
async fn detail_selects_exact_focal_post_and_stops_before_followup_pages() {
    let server = Server::new(vec![
        Response::json(detail_page(
            vec![tweet("99"), tweet("100"), tweet("101")],
            Some("next"),
        )),
        Response::json(detail_page(vec![tweet("102")], None)),
    ]);
    let native = extractor(Duration::from_secs(3)).await;
    let items: Vec<_> = native
        .detail_stream_at(server.base.clone(), "100".into())
        .try_collect()
        .await
        .unwrap();
    assert_eq!(post_ids(&items), ["100"]);
    assert_eq!(server.count(), 1);
    let urls = server.urls();
    assert!(
        urls[0]
            .path()
            .ends_with("/iFEr5AcP121Og4wx9Yqo3w/TweetDetail")
    );
    let vars = variables(&urls[0]);
    assert_eq!(vars["focalTweetId"], "100");
    assert_eq!(vars["referrer"], "profile");
    for key in [
        "with_rux_injections",
        "includePromotedContent",
        "withQuickPromoteEligibilityTweetFields",
    ] {
        assert_eq!(vars[key], false);
    }
    for key in ["withCommunity", "withBirdwatchNotes", "withVoice"] {
        assert_eq!(vars[key], true);
    }
    assert!(vars.get("cursor").is_none());
    let toggles: Value = serde_json::from_str(
        &urls[0]
            .query_pairs()
            .find(|(key, _)| key == "fieldToggles")
            .unwrap()
            .1,
    )
    .unwrap();
    assert_eq!(toggles["withArticleRichContentState"], true);
    assert_eq!(toggles["withArticlePlainText"], false);
    assert_eq!(toggles["withGrokAnalyze"], false);
    assert_eq!(toggles["withDisallowedReplyControls"], false);
    assert_scoped_expansion_requests(&server, &native);
}

#[tokio::test]
async fn detail_find_focal_on_later_page_without_substituting_context() {
    let cursor = "detail opaque+/=&?";
    let server = Server::new(vec![
        Response::json(detail_page(vec![tweet("99")], Some(cursor))),
        Response::json(detail_page(
            vec![tweet("100"), tweet("101")],
            Some("unneeded"),
        )),
    ]);
    let native = extractor(Duration::from_secs(3)).await;
    let items: Vec<_> = native
        .detail_stream_at(server.base.clone(), "100".into())
        .try_collect()
        .await
        .unwrap();
    assert_eq!(post_ids(&items), ["100"]);
    assert_eq!(server.count(), 2);
    assert_eq!(variables(&server.urls()[1])["cursor"], cursor);

    let server = Server::new(vec![Response::json(detail_page(vec![tweet("99")], None))]);
    let mut stream = native.detail_stream_at(server.base.clone(), "100".into());
    assert!(stream.next().await.unwrap().is_err());
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn detail_conversation_modes_bind_prefix_and_reply_filters_to_focal_author() {
    for (mode, replies, expected) in [
        (
            ConversationMode::All,
            ReplyMode::All,
            vec!["98", "99", "100", "101", "102"],
        ),
        (
            ConversationMode::Accessible,
            ReplyMode::All,
            vec!["98", "99", "100", "101", "102"],
        ),
        (
            ConversationMode::All,
            ReplyMode::SelfOnly,
            vec!["99", "100", "101"],
        ),
        (ConversationMode::Accessible, ReplyMode::Skip, vec!["100"]),
    ] {
        let server = Server::new(vec![
            Response::json(detail_page(
                vec![reply("98", "77", "42"), reply("99", "42", "77")],
                Some("next"),
            )),
            Response::json(detail_page(
                vec![
                    tweet("100"),
                    reply("101", "42", "77"),
                    reply("102", "77", "42"),
                ],
                None,
            )),
        ]);
        let native = extractor(Duration::from_secs(3))
            .await
            .with_status_options(conversation_options(mode))
            .with_profile_options(ProfileOptions {
                pinned: false,
                replies,
            });
        let items: Vec<_> = native
            .detail_stream_at(server.base.clone(), "100".into())
            .try_collect()
            .await
            .unwrap();
        assert_eq!(post_ids(&items), expected);
        assert_eq!(server.count(), 2);
    }
}

#[tokio::test]
async fn detail_inaccessible_focal_returns_context_only_in_all_mode() {
    for (mode, expected) in [
        (ConversationMode::All, vec!["99", "101"]),
        (ConversationMode::Accessible, vec![]),
    ] {
        let mut first = detail_page(vec![tweet("99")], Some("next"));
        first["data"]["threaded_conversation_with_injections_v2"]["instructions"][0]["entries"]
            .as_array_mut().unwrap().push(json!({"entryId":"tweet-100","content":{"entryType":"TimelineTimelineItem","itemContent":{"itemType":"TimelineTweet","tweet_results":{"result":{"__typename":"TweetTombstone","tombstone":{"text":{"text":"private inaccessible context"}}}}}}}));
        let server = Server::new(vec![
            Response::json(first),
            Response::json(detail_page(vec![tweet("101")], None)),
        ]);
        let native = extractor(Duration::from_secs(3))
            .await
            .with_status_options(conversation_options(mode));
        let items: Vec<_> = native
            .detail_stream_at(server.base.clone(), "100".into())
            .try_collect()
            .await
            .unwrap();
        assert_eq!(post_ids(&items), expected);
        assert_eq!(server.count(), 2);
    }
}

#[tokio::test]
async fn detail_reuses_same_page_quote_target_preserving_order_and_archive_identity() {
    for mode in [ConversationMode::Off, ConversationMode::All] {
        let server = Server::new(vec![Response::json(detail_page(
            vec![
                tweet("99"),
                quoted_tweet("100", "101"),
                tweet("101"),
                tweet("102"),
            ],
            None,
        ))]);
        let native = extractor(Duration::from_secs(3))
            .await
            .with_status_options(conversation_options(mode))
            .with_expansion(expansion_options(RetweetMode::Skip));
        let items: Vec<_> = native
            .detail_stream_at(server.base.clone(), "100".into())
            .try_collect()
            .await
            .unwrap();
        assert_eq!(
            post_ids(&items),
            if mode == ConversationMode::Off {
                vec!["100", "101"]
            } else {
                vec!["99", "100", "101", "102"]
            }
        );
        let quote = items
            .iter()
            .find(|media| media.metadata.as_ref().unwrap().post_id == "101")
            .unwrap();
        let direct = parse_response(
            &serde_json::to_vec(&post_response(tweet("101"))).unwrap(),
            "101",
        )
        .unwrap();
        assert_eq!(quote.archive_key(), direct[0].archive_key());
        assert_eq!(
            quote.metadata.as_ref().unwrap().quoted_by_id.as_deref(),
            Some("100")
        );
        assert_eq!(server.count(), 1);
    }
}

#[tokio::test]
async fn detail_missing_quote_fetch_is_lazy_and_precedes_next_detail_page() {
    let server = Server::new(vec![
        Response::json(detail_page(vec![quoted_tweet("100", "101")], Some("next"))),
        Response::json(post_response(tweet("101"))),
        Response::json(detail_page(vec![tweet("102")], None)),
    ]);
    let native = extractor(Duration::from_secs(3))
        .await
        .with_status_options(conversation_options(ConversationMode::All))
        .with_expansion(expansion_options(RetweetMode::Skip));
    let mut stream = native.detail_stream_at(server.base.clone(), "100".into());
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
        "100"
    );
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
        "101"
    );
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
    assert!(stream.next().await.is_none());
    let urls = server.urls();
    assert!(urls[1].path().ends_with("/TweetResultByRestId"));
    assert_eq!(variables(&urls[1])["tweetId"], "101");
    assert!(urls[2].path().ends_with("/TweetDetail"));
    assert_eq!(variables(&urls[2])["cursor"], "next");
    assert_scoped_expansion_requests(&server, &native);
}

#[tokio::test]
async fn detail_range_and_drop_cancel_quote_and_pagination_work() {
    for use_range in [false, true] {
        let server = Server::new(vec![
            Response::json(detail_page(vec![quoted_tweet("100", "101")], Some("next"))),
            Response::json(post_response(tweet("101"))),
            Response::json(detail_page(vec![tweet("102")], None)),
        ]);
        let native = extractor(Duration::from_secs(3))
            .await
            .with_status_options(conversation_options(ConversationMode::All))
            .with_expansion(expansion_options(RetweetMode::Skip));
        let stream = native.detail_stream_at(server.base.clone(), "100".into());
        if use_range {
            let mut selected = crate::selection::RangeSelection::parse("1")
                .unwrap()
                .apply(stream);
            assert!(selected.next().await.unwrap().is_ok());
            assert!(selected.next().await.is_none());
        } else {
            let mut stream = stream;
            assert!(stream.next().await.unwrap().is_ok());
            drop(stream);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(server.count(), 1);
    }
}

#[tokio::test]
async fn detail_late_auth_failure_keeps_emitted_context_and_hides_response_data() {
    let server = Server::new(vec![
        Response::json(detail_page(vec![tweet("99"), tweet("100")], Some("next"))),
        Response::json(json!({"errors":[{"code":89,"message":"private session csrf status-id"}]})),
    ]);
    let native = extractor(Duration::from_secs(3))
        .await
        .with_status_options(conversation_options(ConversationMode::All));
    let mut stream = native.detail_stream_at(server.base.clone(), "100".into());
    assert_eq!(
        stream
            .next()
            .await
            .unwrap()
            .unwrap()
            .metadata
            .unwrap()
            .post_id,
        "99"
    );
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
    assert!(!error.contains("private session"));
    assert!(stream.next().await.is_none());
    assert_eq!(server.count(), 2);
}

#[tokio::test]
async fn detail_active_budget_excludes_consumer_pause_and_accumulates_quote_requests() {
    let native = extractor(Duration::from_millis(250))
        .await
        .with_status_options(conversation_options(ConversationMode::All))
        .with_expansion(expansion_options(RetweetMode::Skip));
    let server = Server::new(vec![
        Response::json(detail_page(vec![tweet("100")], Some("next"))).delayed(20),
        Response::json(detail_page(vec![tweet("101")], None)).delayed(20),
    ]);
    let mut stream = native.detail_stream_at(server.base.clone(), "100".into());
    assert!(stream.next().await.unwrap().is_ok());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(stream.next().await.unwrap().is_ok());
    assert!(stream.next().await.is_none());
    let server = Server::new(vec![
        Response::json(detail_page(vec![quoted_tweet("100", "101")], Some("next"))).delayed(150),
        Response::json(post_response(tweet("101"))).delayed(180),
        Response::json(detail_page(vec![tweet("102")], None)),
    ]);
    let mut stream = native.detail_stream_at(server.base.clone(), "100".into());
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
    assert_eq!(server.count(), 2);
}

#[tokio::test]
async fn detail_repeated_cursor_fails_without_fetching_or_emitting_loop_page() {
    let server = Server::new(vec![
        Response::json(detail_page(vec![tweet("100")], Some("same"))),
        Response::json(detail_page(vec![tweet("101")], Some("same"))),
        Response::json(detail_page(vec![tweet("102")], None)),
    ]);
    let native = extractor(Duration::from_secs(3))
        .await
        .with_status_options(conversation_options(ConversationMode::All));
    let mut stream = native.detail_stream_at(server.base.clone(), "100".into());
    assert!(stream.next().await.unwrap().is_ok());
    assert!(stream.next().await.unwrap().is_err());
    assert!(stream.next().await.is_none());
    assert_eq!(server.count(), 2);
}

#[tokio::test]
async fn detail_cookies_rotate_on_actual_operation_path_without_reaching_media_headers() {
    let mut first = Response::json(detail_page(vec![quoted_tweet("100", "101")], None));
    first.headers = "Set-Cookie: ct0=detailrotated; Path=/i/api/graphql/\r\nSet-Cookie: auth_token=detailsession; Path=/i/api/graphql/\r\n".into();
    let server = Server::new(vec![first, Response::json(post_response(tweet("101")))]);
    let network = NetworkOptions::new(None, Some("gallery-dl-detail-test/1".into())).unwrap();
    let native = extractor(Duration::from_secs(3))
        .await
        .with_network(&network)
        .unwrap()
        .with_expansion(expansion_options(RetweetMode::Skip));
    let items: Vec<_> = native
        .detail_stream_at(server.base.clone(), "100".into())
        .try_collect()
        .await
        .unwrap();
    assert_eq!(post_ids(&items), ["100", "101"]);
    let requests = server.requests.lock().unwrap();
    assert!(
        requests[0]
            .to_ascii_lowercase()
            .contains("x-csrf-token: csrf\r\n")
    );
    let second = requests[1].to_ascii_lowercase();
    assert!(second.contains("x-csrf-token: detailrotated\r\n"));
    assert!(second.contains("auth_token=detailsession"));
    drop(requests);
    for media in items {
        assert_eq!(
            media.headers.get("User-Agent").map(String::as_str),
            Some("gallery-dl-detail-test/1")
        );
        assert!(!media.headers.keys().any(|key| {
            [
                "cookie",
                "authorization",
                "x-csrf-token",
                "x-client-transaction-id",
            ]
            .iter()
            .any(|private| key.eq_ignore_ascii_case(private))
        }));
        assert!(
            !media
                .headers
                .values()
                .any(|value| value.contains("detailrotated") || value.contains("detailsession"))
        );
    }
}

#[tokio::test]
async fn detail_dropping_pending_initial_request_stops_navigation() {
    let server = Server::new(vec![
        Response::json(detail_page(vec![tweet("100")], Some("next"))).delayed(300),
        Response::json(detail_page(vec![tweet("101")], None)),
    ]);
    let native = extractor(Duration::from_secs(3))
        .await
        .with_status_options(conversation_options(ConversationMode::All));
    let mut stream = native.detail_stream_at(server.base.clone(), "100".into());
    {
        let pending = stream.next();
        tokio::pin!(pending);
        tokio::select! {
            result = &mut pending => panic!("unexpected completion: {result:?}"),
            () = server.wait_for(1) => {}
        }
    }
    drop(stream);
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(server.count(), 1);
}

#[tokio::test]
async fn detail_original_retweet_self_only_binds_original_author_and_reuses_resolved_target() {
    for same_page_target in [false, true] {
        let original = reply("101", "99", "42");
        let mut repost = authored(tweet("100"), "42");
        repost["legacy"]["retweeted_status_id_str"] = "101".into();
        let mut posts = vec![repost, reply("102", "42", "99"), reply("103", "99", "42")];
        if same_page_target {
            posts.push(original.clone());
        }
        let mut responses = vec![Response::json(detail_page(posts, None))];
        if !same_page_target {
            responses.push(Response::json(post_response(original.clone())));
        }
        let server = Server::new(responses);
        let native = extractor(Duration::from_secs(3))
            .await
            .with_status_options(conversation_options(ConversationMode::All))
            .with_expansion(expansion_options(RetweetMode::Original))
            .with_profile_options(ProfileOptions {
                pinned: false,
                replies: ReplyMode::SelfOnly,
            });
        let items: Vec<_> = native
            .detail_stream_at(server.base.clone(), "100".into())
            .try_collect()
            .await
            .unwrap();
        assert_eq!(post_ids(&items), ["101", "103"]);
        assert_eq!(
            items[0]
                .metadata
                .as_ref()
                .unwrap()
                .reposted_by_id
                .as_deref(),
            Some("100")
        );
        let direct = parse_response(
            &serde_json::to_vec(&post_response(original)).unwrap(),
            "101",
        )
        .unwrap();
        assert_eq!(items[0].archive_key(), direct[0].archive_key());
        assert_eq!(server.count(), if same_page_target { 1 } else { 2 });
        if !same_page_target {
            assert!(server.urls()[1].path().ends_with("/TweetResultByRestId"));
            assert_eq!(variables(&server.urls()[1])["tweetId"], "101");
        }
        assert_scoped_expansion_requests(&server, &native);
    }
}

#[tokio::test]
async fn standalone_original_retweet_self_only_fetches_target_once_and_uses_original_archive() {
    let original = reply("101", "99", "42");
    let mut repost = authored(tweet("100"), "42");
    repost["legacy"]["retweeted_status_id_str"] = "101".into();
    let server = Server::new(vec![
        Response::json(post_response(repost)),
        Response::json(post_response(original.clone())),
    ]);
    let native = extractor(Duration::from_secs(3))
        .await
        .with_expansion(expansion_options(RetweetMode::Original))
        .with_profile_options(ProfileOptions {
            pinned: false,
            replies: ReplyMode::SelfOnly,
        });
    let items: Vec<_> = native
        .post_stream_at(local_post_endpoint(&server, "100"), "100".into())
        .try_collect()
        .await
        .unwrap();
    assert_eq!(post_ids(&items), ["101"]);
    assert_eq!(
        items[0]
            .metadata
            .as_ref()
            .unwrap()
            .reposted_by_id
            .as_deref(),
        Some("100")
    );
    let direct = parse_response(
        &serde_json::to_vec(&post_response(original)).unwrap(),
        "101",
    )
    .unwrap();
    assert_eq!(items[0].archive_key(), direct[0].archive_key());
    assert_eq!(server.count(), 2);
    assert_eq!(variables(&server.urls()[1])["tweetId"], "101");
    assert_scoped_expansion_requests(&server, &native);
}

#[tokio::test]
async fn detail_missing_focal_self_only_uses_unbound_actual_self_reply_filter() {
    let server = Server::new(vec![
        Response::json(detail_page(
            vec![reply("100", "42", "42"), reply("101", "42", "99")],
            Some("next"),
        )),
        Response::json(detail_page(
            vec![reply("102", "99", "42"), reply("103", "99", "99")],
            None,
        )),
    ]);
    let native = extractor(Duration::from_secs(3))
        .await
        .with_status_options(conversation_options(ConversationMode::All))
        .with_profile_options(ProfileOptions {
            pinned: false,
            replies: ReplyMode::SelfOnly,
        });
    let items: Vec<_> = native
        .detail_stream_at(server.base.clone(), "999".into())
        .try_collect()
        .await
        .unwrap();
    assert_eq!(post_ids(&items), ["100", "103"]);
    assert_eq!(server.count(), 2);
}

fn in_thread(mut post: Value, conversation: Value) -> Value {
    post["legacy"]["conversation_id_str"] = conversation;
    post
}

fn expanded_threads(unique: bool) -> ThreadOptions {
    ThreadOptions {
        expand: true,
        unique,
    }
}

#[tokio::test]
async fn unique_setting_applies_without_thread_expansion_to_all_paginated_sources() {
    for unique in [true, false] {
        for source in ["profile", "search", "bookmarks", "detail"] {
            let mut post = tweet("100");
            let photo = post["legacy"]["extended_entities"]["media"][0].clone();
            post["legacy"]["extended_entities"]["media"] = json!([photo.clone(), photo]);
            let bodies = match source {
                "profile" => vec![
                    page(vec![post.clone()], Some("next")),
                    page(vec![post], None),
                ],
                "search" => vec![
                    search_page(vec![post.clone()], Some("next")),
                    search_page(vec![post], None),
                ],
                "bookmarks" => vec![
                    feed_page(
                        &collections::Collection::Bookmarks,
                        vec![post.clone()],
                        Some("next"),
                    ),
                    feed_page(&collections::Collection::Bookmarks, vec![post], None),
                ],
                _ => vec![
                    detail_page(vec![post.clone()], Some("next")),
                    detail_page(vec![post], None),
                ],
            };
            let server = Server::new(bodies.into_iter().map(Response::json).collect());
            let native = extractor(Duration::from_secs(3))
                .await
                .with_thread_options(ThreadOptions {
                    expand: false,
                    unique,
                })
                .with_timeline_options(TimelineOptions {
                    search_pagination: SearchPagination::Cursor,
                    ..Default::default()
                })
                .with_status_options(conversation_options(ConversationMode::All));
            let stream = match source {
                "profile" => native.media_stream(input("id:42"), server.base.clone()),
                "search" => native.search_stream("fixture".into(), server.base.clone()),
                "bookmarks" => {
                    native.feed_stream(collections::Collection::Bookmarks, server.base.clone())
                }
                _ => native.detail_stream_at(server.base.clone(), "100".into()),
            };
            let items: Vec<_> = stream.try_collect().await.unwrap();
            assert_eq!(
                post_ids(&items),
                vec!["100"; if unique { 2 } else { 4 }],
                "{source}, unique={unique}"
            );
            let indices: Vec<_> = items
                .iter()
                .map(|item| item.metadata.as_ref().unwrap().index)
                .collect();
            assert_eq!(indices, if unique { vec![1, 2] } else { vec![1, 2, 1, 2] });
            assert_eq!(server.count(), 2);
        }
    }
}

#[tokio::test]
async fn thread_expansion_is_lazy_ordered_once_per_conversation_and_nonrecursive() {
    let server = Server::new(vec![
        Response::json(page(
            vec![
                in_thread(tweet("100"), json!("90")),
                in_thread(tweet("101"), json!("90")),
                in_thread(tweet("102"), json!(false)),
            ],
            Some("parent-next"),
        )),
        Response::json(detail_page(
            vec![
                in_thread(tweet("90"), json!("80")),
                in_thread(tweet("100"), json!("90")),
            ],
            Some("child-next"),
        )),
        Response::json(detail_page(
            vec![in_thread(tweet("101"), json!("90"))],
            None,
        )),
        Response::json(page(vec![in_thread(tweet("103"), json!(false))], None)),
    ]);
    let native = extractor(Duration::from_secs(3))
        .await
        .with_thread_options(expanded_threads(true));
    let mut stream = native.media_stream(input("id:42"), server.base.clone());
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
        "90"
    );
    assert_eq!(server.count(), 2);
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(server.count(), 2);
    let rest: Vec<_> = stream.try_collect().await.unwrap();
    assert_eq!(post_ids(&rest), ["100", "101", "102", "103"]);
    let urls = server.urls();
    assert_eq!(urls.len(), 4);
    assert!(urls[1].path().ends_with("/TweetDetail"));
    assert_eq!(variables(&urls[1])["focalTweetId"], "90");
    assert_eq!(variables(&urls[2])["focalTweetId"], "90");
    assert_eq!(variables(&urls[2])["cursor"], "child-next");
    assert_eq!(variables(&urls[3])["cursor"], "parent-next");
    assert_scoped_expansion_requests(&server, &native);
}

#[tokio::test]
async fn threads_missing_ids_skip_false_bypasses_and_malformed_ids_are_private() {
    let server = Server::new(vec![Response::json(page(
        vec![
            tweet("100"),
            in_thread(tweet("101"), Value::Null),
            in_thread(tweet("102"), json!("")),
            in_thread(tweet("103"), json!(false)),
        ],
        None,
    ))]);
    let native = extractor(Duration::from_secs(3))
        .await
        .with_thread_options(expanded_threads(true));
    let items: Vec<_> = native
        .media_stream(input("id:42"), server.base.clone())
        .try_collect()
        .await
        .unwrap();
    assert_eq!(post_ids(&items), ["103"]);
    assert_eq!(server.count(), 1);
    for bad in [
        json!("private-thread-marker"),
        json!("090"),
        json!(true),
        json!(90),
        json!({"secret":"private-thread-marker"}),
    ] {
        let server = Server::new(vec![Response::json(page(
            vec![in_thread(tweet("100"), bad)],
            None,
        ))]);
        let mut stream = native.media_stream(input("id:42"), server.base.clone());
        let error = stream.next().await.unwrap().unwrap_err().to_string();
        assert!(!error.contains("private-thread-marker"));
        assert!(stream.next().await.is_none());
        assert_eq!(server.count(), 1);
    }
}

#[tokio::test]
async fn thread_failure_falls_back_to_original_and_continues_parent_without_leaking_error() {
    for late in [false, true] {
        let mut responses = vec![Response::json(page(
            vec![
                in_thread(tweet("100"), json!("90")),
                in_thread(tweet("102"), json!(false)),
            ],
            None,
        ))];
        if late {
            responses.push(Response::json(detail_page(vec![tweet("90")], Some("next"))));
        }
        responses.push(Response::json(
            json!({"errors":[{"code":89,"message":"private-auth-thread-marker"}]}),
        ));
        let server = Server::new(responses);
        let native = extractor(Duration::from_secs(3))
            .await
            .with_thread_options(expanded_threads(true));
        let items: Vec<_> = native
            .media_stream(input("id:42"), server.base.clone())
            .try_collect()
            .await
            .unwrap();
        assert_eq!(
            post_ids(&items),
            if late {
                vec!["90", "100", "102"]
            } else {
                vec!["100", "102"]
            }
        );
        assert_eq!(server.count(), if late { 3 } else { 2 });
    }
}

#[tokio::test]
async fn thread_failure_fallback_respects_unique_setting_after_partial_child_output() {
    for unique in [true, false] {
        let server = Server::new(vec![
            Response::json(page(vec![in_thread(tweet("100"), json!("90"))], None)),
            Response::json(detail_page(vec![tweet("100")], Some("next"))),
            Response::json(json!({"data":{"threaded_conversation_with_injections_v2":null}})),
        ]);
        let native = extractor(Duration::from_secs(3))
            .await
            .with_thread_options(expanded_threads(unique));
        let items: Vec<_> = native
            .media_stream(input("id:42"), server.base.clone())
            .try_collect()
            .await
            .unwrap();
        assert_eq!(
            post_ids(&items),
            if unique {
                vec!["100"]
            } else {
                vec!["100", "100"]
            }
        );
        assert_eq!(server.count(), 3);
    }
}

#[tokio::test]
async fn nonunique_threads_preserve_overlapping_posts_but_never_expand_a_conversation_twice() {
    let server = Server::new(vec![
        Response::json(page(
            vec![
                in_thread(tweet("100"), json!("90")),
                in_thread(tweet("101"), json!("90")),
            ],
            Some("next"),
        )),
        Response::json(detail_page(
            vec![tweet("90"), tweet("100"), tweet("100")],
            None,
        )),
        Response::json(page(vec![in_thread(tweet("100"), json!(false))], None)),
    ]);
    let native = extractor(Duration::from_secs(3))
        .await
        .with_thread_options(expanded_threads(false));
    let items: Vec<_> = native
        .media_stream(input("id:42"), server.base.clone())
        .try_collect()
        .await
        .unwrap();
    assert_eq!(post_ids(&items), ["90", "100", "100", "100"]);
    assert_eq!(server.count(), 3);
}

#[tokio::test]
async fn thread_range_cutoff_and_dropped_pending_child_stop_all_followup_work() {
    let native = extractor(Duration::from_secs(3))
        .await
        .with_thread_options(expanded_threads(true));
    let server = Server::new(vec![
        Response::json(page(
            vec![
                in_thread(tweet("100"), json!("90")),
                in_thread(tweet("101"), json!("80")),
            ],
            Some("parent"),
        )),
        Response::json(detail_page(vec![tweet("90"), tweet("100")], Some("child"))),
        Response::json(detail_page(vec![tweet("101")], None)),
    ]);
    let items: Vec<_> = native
        .media_stream(input("id:42"), server.base.clone())
        .take(1)
        .try_collect()
        .await
        .unwrap();
    assert_eq!(post_ids(&items), ["90"]);
    assert_eq!(server.count(), 2);
    let server = Server::new(vec![
        Response::json(page(
            vec![in_thread(tweet("100"), json!("90"))],
            Some("parent"),
        )),
        Response::json(detail_page(vec![tweet("90")], Some("child"))).delayed(300),
        Response::json(detail_page(vec![tweet("100")], None)),
    ]);
    let mut stream = native.media_stream(input("id:42"), server.base.clone());
    assert!(
        tokio::time::timeout(Duration::from_millis(60), stream.next())
            .await
            .is_err()
    );
    server.wait_for(2).await;
    drop(stream);
    tokio::time::sleep(Duration::from_millis(330)).await;
    assert_eq!(server.count(), 2);
}

#[tokio::test]
async fn thread_expansion_shares_parent_active_budget_and_excludes_consumer_pause() {
    let native = extractor(Duration::from_millis(250))
        .await
        .with_thread_options(expanded_threads(true));
    let server = Server::new(vec![
        Response::json(page(
            vec![in_thread(tweet("100"), json!("90"))],
            Some("parent"),
        ))
        .delayed(20),
        Response::json(detail_page(vec![tweet("90")], Some("child"))).delayed(20),
        Response::json(detail_page(vec![tweet("100")], None)).delayed(20),
        Response::json(page(vec![in_thread(tweet("101"), json!(false))], None)).delayed(20),
    ]);
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
        "90"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    let rest: Vec<_> = stream.try_collect().await.unwrap();
    assert_eq!(post_ids(&rest), ["100", "101"]);
    let server = Server::new(vec![
        Response::json(page(
            vec![in_thread(tweet("100"), json!("90"))],
            Some("parent"),
        ))
        .delayed(100),
        Response::json(detail_page(vec![tweet("90")], Some("child"))).delayed(100),
        Response::json(detail_page(vec![tweet("100")], None)).delayed(100),
        Response::json(page(vec![in_thread(tweet("101"), json!(false))], None)),
    ]);
    let mut stream = native.media_stream(input("id:42"), server.base.clone());
    assert!(stream.next().await.unwrap().is_ok());
    let results: Vec<_> = stream.collect().await;
    assert!(results.iter().any(|item| {
        item.as_ref()
            .is_err_and(|error| error.to_string().contains("timed out"))
    }));
    assert_eq!(server.count(), 3);
}

#[tokio::test]
async fn thread_expansion_applies_to_search_bookmarks_rest_id_and_detail_sources() {
    for source in ["search", "bookmarks", "rest", "detail"] {
        let post = in_thread(tweet("100"), json!("90"));
        let body = match source {
            "search" => search_page(vec![post], None),
            "bookmarks" => feed_page(&collections::Collection::Bookmarks, vec![post], None),
            "rest" => post_response(post),
            _ => detail_page(vec![post], None),
        };
        let server = Server::new(vec![
            Response::json(body),
            Response::json(detail_page(vec![tweet("90"), tweet("100")], None)),
        ]);
        let native = extractor(Duration::from_secs(3))
            .await
            .with_thread_options(expanded_threads(true));
        let stream = match source {
            "search" => native.search_stream("fixture".into(), server.base.clone()),
            "bookmarks" => {
                native.feed_stream(collections::Collection::Bookmarks, server.base.clone())
            }
            "rest" => native.post_stream_at(local_post_endpoint(&server, "100"), "100".into()),
            _ => native.detail_stream_at(server.base.clone(), "100".into()),
        };
        let items: Vec<_> = stream.try_collect().await.unwrap();
        assert_eq!(post_ids(&items), ["90", "100"], "{source}");
        assert_eq!(server.count(), 2, "{source}");
        assert_eq!(
            server.urls()[1].path(),
            "/i/api/graphql/iFEr5AcP121Og4wx9Yqo3w/TweetDetail",
            "expanded {source} must use the canonical detail path"
        );
        assert_eq!(variables(&server.urls()[1])["focalTweetId"], "90");
    }
}

#[tokio::test]
async fn expanded_profile_replies_keep_the_requested_owner_for_self_only_filter() {
    let server = Server::new(vec![
        Response::json(page(vec![in_thread(tweet("100"), json!("90"))], None)),
        Response::json(detail_page(
            vec![
                authored(tweet("90"), "77"),
                reply("100", "42", "42"),
                reply("101", "77", "77"),
                reply("102", "42", "77"),
            ],
            None,
        )),
    ]);
    let native = extractor(Duration::from_secs(3))
        .await
        .with_thread_options(expanded_threads(true))
        .with_profile_options(ProfileOptions {
            replies: ReplyMode::SelfOnly,
            ..Default::default()
        });
    let items: Vec<_> = native
        .profile_stream(
            input("id:42"),
            timeline::ProfileTab::Replies,
            server.base.clone(),
        )
        .try_collect()
        .await
        .unwrap();
    assert_eq!(post_ids(&items), ["90", "100", "102"]);
    assert_eq!(server.count(), 2);
}

#[test]
fn thread_lookup_preserves_original_retweet_and_disabled_link_semantics() {
    use expansion::{ThreadLookup, thread_lookup};
    let mut outer = in_thread(tweet("100"), json!("90"));
    outer["legacy"]["retweeted_status_id_str"] = json!("101");
    let known = BTreeMap::new();
    assert!(
        matches!(thread_lookup(&outer, RetweetMode::Original, &known).unwrap(), ThreadLookup::Fetch(id) if id == "101")
    );
    for mode in [RetweetMode::Skip, RetweetMode::Repost] {
        assert!(
            matches!(thread_lookup(&outer, mode, &known).unwrap(), ThreadLookup::Id(id) if id == "90")
        );
    }
    let mut known = BTreeMap::new();
    known.insert("101".into(), in_thread(tweet("101"), json!("80")));
    assert!(
        matches!(thread_lookup(&outer, RetweetMode::Original, &known).unwrap(), ThreadLookup::Id(id) if id == "80")
    );
    outer["retweeted_status_result"] = json!({"result":in_thread(tweet("101"), json!(false))});
    assert!(matches!(
        thread_lookup(&outer, RetweetMode::Original, &known).unwrap(),
        ThreadLookup::Bypass
    ));
    outer["retweeted_status_result"] = json!({"result":in_thread(tweet("101"), Value::Null)});
    assert!(matches!(
        thread_lookup(&outer, RetweetMode::Original, &known).unwrap(),
        ThreadLookup::Skip
    ));
    outer["retweeted_status_result"] =
        json!({"result":in_thread(tweet("101"), json!("private-thread-id"))});
    let error = match thread_lookup(&outer, RetweetMode::Original, &known) {
        Err(error) => error,
        Ok(_) => panic!("malformed original conversation must fail"),
    };
    assert!(!error.to_string().contains("private-thread-id"));
    // Disabled original expansion ignores malformed nested metadata.
    assert!(
        matches!(thread_lookup(&outer, RetweetMode::Repost, &known).unwrap(), ThreadLookup::Id(id) if id == "90")
    );
}

#[test]
fn thread_lookup_rejects_original_retweet_cycles_and_conflicting_targets_privately() {
    use expansion::thread_lookup;
    let mut outer = in_thread(tweet("100"), json!("90"));
    outer["legacy"]["retweeted_status_id_str"] = json!("101");
    let mut original = in_thread(tweet("101"), json!("80"));
    original["legacy"]["retweeted_status_id_str"] = json!("100");
    let mut known = BTreeMap::new();
    known.insert("100".into(), outer.clone());
    known.insert("101".into(), original);
    let error = match thread_lookup(&outer, RetweetMode::Original, &known) {
        Err(error) => error,
        Ok(_) => panic!("original retweet cycle must fail"),
    };
    assert!(error.to_string().contains("cycle"));
    outer["retweeted_status_result"] = json!({"result":tweet("102")});
    let error = match thread_lookup(&outer, RetweetMode::Original, &known) {
        Err(error) => error,
        Ok(_) => panic!("conflicting retweet identity must fail"),
    };
    assert!(!error.to_string().contains("102"));
    assert!(!error.to_string().contains("101"));
}

#[tokio::test]
async fn expanded_rest_id_original_retweet_reuses_self_only_owner_resolution_target() {
    let mut outer = in_thread(tweet("100"), json!("90"));
    outer["legacy"]["retweeted_status_id_str"] = json!("101");
    let original = in_thread(reply("101", "99", "42"), json!("80"));
    let server = Server::new(vec![
        Response::json(post_response(outer)),
        Response::json(post_response(original.clone())),
        Response::json(detail_page(
            vec![authored(tweet("80"), "99"), original],
            None,
        )),
    ]);
    let native = extractor(Duration::from_secs(3))
        .await
        .with_thread_options(expanded_threads(true))
        .with_expansion(expansion_options(RetweetMode::Original))
        .with_profile_options(ProfileOptions {
            replies: ReplyMode::SelfOnly,
            ..Default::default()
        });
    let items: Vec<_> = native
        .post_stream_at(local_post_endpoint(&server, "100"), "100".into())
        .try_collect()
        .await
        .unwrap();
    assert_eq!(post_ids(&items), ["80", "101"]);
    let urls = server.urls();
    assert_eq!(urls.len(), 3);
    assert!(urls[1].path().ends_with("/TweetResultByRestId"));
    assert_eq!(variables(&urls[1])["tweetId"], "101");
    assert_eq!(
        urls[2].path(),
        "/i/api/graphql/iFEr5AcP121Og4wx9Yqo3w/TweetDetail"
    );
    assert_eq!(variables(&urls[2])["focalTweetId"], "80");
}

#[tokio::test]
async fn expanded_detail_original_retweet_reuses_same_page_target_for_conversation_lookup() {
    let mut outer = in_thread(tweet("100"), json!("90"));
    outer["legacy"]["retweeted_status_id_str"] = json!("101");
    let original = in_thread(authored(tweet("101"), "99"), json!("80"));
    let server = Server::new(vec![
        Response::json(detail_page(vec![outer, original.clone()], None)),
        Response::json(detail_page(
            vec![authored(tweet("80"), "99"), original],
            None,
        )),
    ]);
    let native = extractor(Duration::from_secs(3))
        .await
        .with_thread_options(expanded_threads(true))
        .with_expansion(expansion_options(RetweetMode::Original));
    let items: Vec<_> = native
        .detail_stream_at(server.base.clone(), "100".into())
        .try_collect()
        .await
        .unwrap();
    assert_eq!(post_ids(&items), ["80", "101"]);
    let urls = server.urls();
    assert_eq!(urls.len(), 2);
    assert!(
        urls.iter()
            .all(|url| url.path() == "/i/api/graphql/iFEr5AcP121Og4wx9Yqo3w/TweetDetail")
    );
    assert_eq!(variables(&urls[0])["focalTweetId"], "100");
    assert_eq!(variables(&urls[1])["focalTweetId"], "80");
}

fn video_post(id: &str) -> Value {
    let fixture: Value =
        serde_json::from_str(include_str!("../tests/fixtures/twitter/video.json")).unwrap();
    let mut post = tweet(id);
    post["legacy"]["extended_entities"]["media"] =
        fixture["data"]["tweetResult"]["result"]["legacy"]["extended_entities"]["media"].clone();
    post
}

#[tokio::test]
async fn media_previews_number_mixed_siblings_consecutively_and_deduplicate_whole_batches() {
    let mut post = tweet("100");
    let photo = post["legacy"]["extended_entities"]["media"][0].clone();
    let video = video_post("100")["legacy"]["extended_entities"]["media"][0].clone();
    post["legacy"]["extended_entities"]["media"] = json!([photo.clone(), video, photo]);
    let server = Server::new(vec![
        Response::json(page(vec![post.clone()], Some("next"))),
        Response::json(page(vec![post], None)),
    ]);
    let native = extractor(Duration::from_secs(3))
        .await
        .with_media_options(MediaOptions {
            videos: VideoMode::Files,
            previews: true,
            ..Default::default()
        });
    let items: Vec<_> = native
        .media_stream(input("id:42"), server.base.clone())
        .try_collect()
        .await
        .unwrap();
    assert_eq!(post_ids(&items), ["100", "100", "100", "100"]);
    assert_eq!(
        items
            .iter()
            .map(|item| item.metadata.as_ref().unwrap().index)
            .collect::<Vec<_>>(),
        [1, 2, 3, 4]
    );
    assert_eq!(
        items
            .iter()
            .map(|item| item.filename.as_str())
            .collect::<Vec<_>>(),
        ["100_1.jpg", "100_2.mp4", "100_3.jpg", "100_4.jpg"]
    );
    assert_eq!(
        items[1].url.as_str(),
        "https://video.example/high.mp4?tag=1"
    );
    assert!(items[2].url.as_str().contains("thumbnail.jpg"));
    let keys: std::collections::HashSet<_> = items
        .iter()
        .map(|item| item.archive_key().unwrap())
        .collect();
    assert_eq!(keys.len(), 4);
    assert_eq!(server.count(), 2);
}

#[tokio::test]
async fn skipped_video_preview_range_stops_before_fetching_missing_quote() {
    let mut post = video_post("100");
    post["legacy"]["quoted_status_id_str"] = json!("101");
    post["legacy"]["extended_entities"]["media"][0]["video_info"] =
        json!({"variants":"private-invalid-variants"});
    let server = Server::new(vec![
        Response::json(post_response(post)),
        Response::json(post_response(video_post("101"))),
    ]);
    let native = extractor(Duration::from_secs(3))
        .await
        .with_media_options(MediaOptions {
            videos: VideoMode::Skip,
            previews: true,
            ..Default::default()
        })
        .with_expansion(expansion_options(RetweetMode::Skip));
    let items: Vec<_> = native
        .post_stream_at(local_post_endpoint(&server, "100"), "100".into())
        .take(1)
        .try_collect()
        .await
        .unwrap();
    assert_eq!(post_ids(&items), ["100"]);
    assert_eq!(items[0].filename, "100_1.jpg");
    assert!(items[0].url.as_str().contains("thumbnail.jpg"));
    assert_eq!(server.count(), 1);
}

#[tokio::test]
async fn original_and_repost_expansion_apply_video_preview_options_and_keep_archive_context() {
    for mode in [RetweetMode::Original, RetweetMode::Repost] {
        let original = video_post("101");
        let mut outer = tweet("100");
        outer["legacy"]
            .as_object_mut()
            .unwrap()
            .remove("extended_entities");
        outer["legacy"]["retweeted_status_id_str"] = json!("101");
        outer["retweeted_status_result"] = json!({"result":original.clone()});
        let server = Server::new(vec![Response::json(post_response(outer))]);
        let native = extractor(Duration::from_secs(3))
            .await
            .with_media_options(MediaOptions {
                videos: VideoMode::Files,
                previews: true,
                ..Default::default()
            })
            .with_expansion(expansion_options(mode));
        let items: Vec<_> = native
            .post_stream_at(local_post_endpoint(&server, "100"), "100".into())
            .try_collect()
            .await
            .unwrap();
        let expected = if mode == RetweetMode::Original {
            "101"
        } else {
            "100"
        };
        assert_eq!(post_ids(&items), [expected, expected]);
        assert!(items[0].filename.ends_with("_1.mp4"));
        assert!(items[1].filename.ends_with("_2.jpg"));
        assert_ne!(items[0].archive_key(), items[1].archive_key());
        assert_eq!(server.count(), 1);
        let direct_server = Server::new(vec![Response::json(post_response(original))]);
        let direct: Vec<_> = native
            .post_stream_at(local_post_endpoint(&direct_server, "101"), "101".into())
            .try_collect()
            .await
            .unwrap();
        assert_eq!(
            items[0].archive_key() == direct[0].archive_key(),
            mode == RetweetMode::Original
        );
        assert_eq!(
            items[1].archive_key() == direct[1].archive_key(),
            mode == RetweetMode::Original
        );
    }
}

#[tokio::test]
async fn ytdl_video_jobs_apply_to_quotes_and_expanded_threads_without_parsing_variants() {
    for expanded in [false, true] {
        let mut video = video_post("101");
        video["legacy"]["extended_entities"]["media"][0]["video_info"] =
            json!({"variants":"private-invalid-variants"});
        let responses = if expanded {
            vec![
                Response::json(page(vec![in_thread(tweet("100"), json!("90"))], None)),
                Response::json(detail_page(vec![video], Some("child-next"))),
                Response::json(detail_page(vec![tweet("102")], None)),
            ]
        } else {
            vec![
                Response::json(post_response(quoted_tweet("100", "101"))),
                Response::json(post_response(video)),
            ]
        };
        let server = Server::new(responses);
        let native = extractor(Duration::from_secs(3))
            .await
            .with_media_options(MediaOptions {
                videos: VideoMode::Ytdl,
                previews: false,
                ..Default::default()
            })
            .with_expansion(expansion_options(RetweetMode::Skip))
            .with_thread_options(ThreadOptions {
                expand: expanded,
                unique: true,
            });
        let stream = if expanded {
            native.media_stream(input("id:42"), server.base.clone())
        } else {
            native.post_stream_at(local_post_endpoint(&server, "100"), "100".into())
        };
        let items: Vec<_> = stream
            .take(if expanded { 1 } else { 2 })
            .try_collect()
            .await
            .unwrap();
        let item = items.last().unwrap();
        assert_eq!(item.metadata.as_ref().unwrap().post_id, "101");
        assert_eq!(item.url.as_str(), "ytdl:https://x.com/i/web/status/101");
        match &item.job {
            crate::extract::MediaJob::VideoPage { page_url } => {
                assert_eq!(page_url.as_str(), "https://x.com/i/web/status/101")
            }
            _ => panic!("video mode must produce an adapter job"),
        }
        assert_eq!(server.count(), 2);
    }
}

#[tokio::test]
async fn invalid_video_poster_is_ignored_when_disabled_and_errors_privately_when_enabled() {
    for previews in [false, true] {
        let mut post = video_post("100");
        post["legacy"]["extended_entities"]["media"][0]["media_url_https"] =
            json!({"private-poster-marker":true});
        let server = Server::new(vec![Response::json(post_response(post))]);
        let native = extractor(Duration::from_secs(3))
            .await
            .with_media_options(MediaOptions {
                videos: VideoMode::Files,
                previews,
                ..Default::default()
            });
        let mut stream = native.post_stream_at(local_post_endpoint(&server, "100"), "100".into());
        if previews {
            let error = stream.next().await.unwrap().unwrap_err().to_string();
            assert!(!error.contains("private-poster-marker"));
        } else {
            let item = stream.next().await.unwrap().unwrap();
            assert_eq!(item.filename, "100_1.mp4");
        }
        assert!(stream.next().await.is_none());
        assert_eq!(server.count(), 1);
    }
}

#[tokio::test]
async fn ytdl_repost_delegate_uses_output_post_identity_and_preserves_quote_parent_context() {
    for mode in [RetweetMode::Original, RetweetMode::Repost] {
        let mut original = video_post("101");
        original["legacy"]["quoted_status_id_str"] = json!("102");
        original["quoted_status_result"] = json!({"result":video_post("102")});
        let mut outer = tweet("100");
        outer["legacy"]
            .as_object_mut()
            .unwrap()
            .remove("extended_entities");
        outer["legacy"]["retweeted_status_id_str"] = json!("101");
        outer["retweeted_status_result"] = json!({"result":original});
        let server = Server::new(vec![Response::json(post_response(outer))]);
        let native = extractor(Duration::from_secs(3))
            .await
            .with_media_options(MediaOptions {
                videos: VideoMode::Ytdl,
                previews: false,
                ..Default::default()
            })
            .with_expansion(expansion_options(mode));
        let items: Vec<_> = native
            .post_stream_at(local_post_endpoint(&server, "100"), "100".into())
            .try_collect()
            .await
            .unwrap();
        let parent = if mode == RetweetMode::Original {
            "101"
        } else {
            "100"
        };
        assert_eq!(post_ids(&items), [parent, "102"]);
        for (item, expected) in items.iter().zip([parent, "102"]) {
            assert_eq!(
                item.url.as_str(),
                format!("ytdl:https://x.com/i/web/status/{expected}")
            );
            match &item.job {
                crate::extract::MediaJob::VideoPage { page_url } => assert_eq!(
                    page_url.as_str(),
                    format!("https://x.com/i/web/status/{expected}")
                ),
                _ => panic!("Ytdl mode must keep its typed adapter job"),
            }
            assert_eq!(item.filename, format!("{expected}_1.mp4"));
        }
        assert_eq!(
            items[1].metadata.as_ref().unwrap().quoted_by_id.as_deref(),
            Some(parent)
        );
        assert_eq!(server.count(), 1);
    }
}

fn summary_card(mut post: Value, name: &str) -> Value {
    post["card"] = json!({"legacy":{"name":"summary_large_image","binding_values":{
        "photo_image_full_size_original":{"image_value":{"url":format!("https://cards.example/{name}.jpg")}}
    }}});
    post
}

fn file_cards() -> CardOptions {
    CardOptions {
        mode: CardMode::Files,
        blacklist: Vec::new(),
    }
}

#[tokio::test]
async fn card_only_posts_are_extracted_by_rest_id_and_detail_with_original_identity() {
    for detail in [false, true] {
        let post = summary_card(text_tweet("100"), "standalone");
        let body = if detail {
            detail_page(vec![post], None)
        } else {
            post_response(post)
        };
        let server = Server::new(vec![Response::json(body)]);
        let native = extractor(Duration::from_secs(3))
            .await
            .with_card_options(file_cards());
        let stream = if detail {
            native.detail_stream_at(server.base.clone(), "100".into())
        } else {
            native.post_stream_at(local_post_endpoint(&server, "100"), "100".into())
        };
        let items: Vec<_> = stream.try_collect().await.unwrap();
        assert_eq!(post_ids(&items), ["100"]);
        assert_eq!(items[0].url.path(), "/standalone.jpg");
        assert_eq!(items[0].filename, "100_1.jpg");
        assert!(items[0].archive_key().is_some());
        assert_eq!(server.count(), 1);
    }
}

#[tokio::test]
async fn cards_follow_ordinary_media_before_lazy_quotes_and_keep_numbering_and_archive_keys() {
    for cutoff in [false, true] {
        let mut parent = summary_card(tweet("100"), "parent");
        parent["legacy"]["quoted_status_id_str"] = json!("101");
        let server = Server::new(vec![
            Response::json(post_response(parent)),
            Response::json(post_response(summary_card(text_tweet("101"), "quoted"))),
        ]);
        let native = extractor(Duration::from_secs(3))
            .await
            .with_card_options(file_cards())
            .with_expansion(expansion_options(RetweetMode::Skip));
        let stream = native.post_stream_at(local_post_endpoint(&server, "100"), "100".into());
        let items: Vec<_> = stream
            .take(if cutoff { 1 } else { 3 })
            .try_collect()
            .await
            .unwrap();
        if cutoff {
            assert_eq!(post_ids(&items), ["100"]);
            assert_eq!(server.count(), 1);
        } else {
            assert_eq!(post_ids(&items), ["100", "100", "101"]);
            assert_eq!(
                items
                    .iter()
                    .map(|item| item.filename.as_str())
                    .collect::<Vec<_>>(),
                ["100_1.jpg", "100_2.jpg", "101_1.jpg"]
            );
            assert_eq!(items[1].url.path(), "/parent.jpg");
            assert_eq!(items[2].url.path(), "/quoted.jpg");
            assert_eq!(
                items[2].metadata.as_ref().unwrap().quoted_by_id.as_deref(),
                Some("100")
            );
            let keys: std::collections::HashSet<_> = items
                .iter()
                .map(|item| item.archive_key().unwrap())
                .collect();
            assert_eq!(keys.len(), 3);
            assert_eq!(server.count(), 2);
        }
    }
}

#[tokio::test]
async fn cards_select_outer_repost_card_and_original_card_with_effective_ytdl_identity() {
    for mode in [RetweetMode::Original, RetweetMode::Repost] {
        for delegate in [false, true] {
            let mut original = summary_card(text_tweet("101"), "original");
            let mut outer = summary_card(text_tweet("100"), "outer");
            if delegate {
                original["card"]["legacy"]["name"] = json!("unsupported-original");
                outer["card"]["legacy"]["name"] = json!("unsupported-outer");
            }
            outer["legacy"]["retweeted_status_id_str"] = json!("101");
            outer["retweeted_status_result"] = json!({"result":original});
            let server = Server::new(vec![Response::json(post_response(outer))]);
            let native = extractor(Duration::from_secs(3))
                .await
                .with_card_options(CardOptions {
                    mode: if delegate {
                        CardMode::Ytdl
                    } else {
                        CardMode::Files
                    },
                    blacklist: Vec::new(),
                })
                .with_expansion(expansion_options(mode));
            let items: Vec<_> = native
                .post_stream_at(local_post_endpoint(&server, "100"), "100".into())
                .try_collect()
                .await
                .unwrap();
            let (id, image) = if mode == RetweetMode::Original {
                ("101", "/original.jpg")
            } else {
                ("100", "/outer.jpg")
            };
            assert_eq!(post_ids(&items), [id]);
            if delegate {
                assert_eq!(
                    items[0].url.as_str(),
                    format!("ytdl:https://x.com/i/web/status/{id}")
                );
                match &items[0].job {
                    crate::extract::MediaJob::VideoPage { page_url } => assert_eq!(
                        page_url.as_str(),
                        format!("https://x.com/i/web/status/{id}")
                    ),
                    _ => panic!("card fallback must remain a typed adapter job"),
                }
            } else {
                assert_eq!(items[0].url.path(), image);
            }
            assert!(items[0].archive_key().is_some());
            assert_eq!(server.count(), 1);
        }
    }
}

#[tokio::test]
async fn expanded_child_threads_apply_card_options_and_range_prevents_later_child_pages() {
    let server = Server::new(vec![
        Response::json(page(
            vec![in_thread(tweet("100"), json!("90"))],
            Some("parent-next"),
        )),
        Response::json(detail_page(
            vec![summary_card(text_tweet("90"), "child")],
            Some("child-next"),
        )),
        Response::json(detail_page(
            vec![summary_card(text_tweet("101"), "later")],
            None,
        )),
    ]);
    let native = extractor(Duration::from_secs(3))
        .await
        .with_card_options(file_cards())
        .with_thread_options(expanded_threads(true));
    let items: Vec<_> = native
        .media_stream(input("id:42"), server.base.clone())
        .take(1)
        .try_collect()
        .await
        .unwrap();
    assert_eq!(post_ids(&items), ["90"]);
    assert_eq!(items[0].url.path(), "/child.jpg");
    assert_eq!(server.count(), 2);
    let server = Server::new(vec![
        Response::json(page(
            vec![in_thread(
                summary_card(text_tweet("100"), "fallback"),
                json!("90"),
            )],
            None,
        )),
        Response::json(json!({"errors":[{"code":89,"message":"private card child failure"}]})),
    ]);
    let items: Vec<_> = native
        .media_stream(input("id:42"), server.base.clone())
        .try_collect()
        .await
        .unwrap();
    assert_eq!(post_ids(&items), ["100"]);
    assert_eq!(items[0].url.path(), "/fallback.jpg");
    assert_eq!(server.count(), 2);
}

#[tokio::test]
async fn card_blacklist_and_default_skip_ignore_malformed_payloads_while_enabled_errors_are_private()
 {
    for case in ["default", "name", "domain", "combined", "enabled"] {
        let mut post = tweet("100");
        post["card"] = json!({"name":"unified_card","binding_values":{
            "vanity_url":{"string_value":"cards.example"},
            "unified_card":{"string_value":"private malformed card payload"}
        }});
        let options = match case {
            "default" => CardOptions::default(),
            "name" => CardOptions {
                mode: CardMode::Files,
                blacklist: vec!["unified_card".into()],
            },
            "domain" => CardOptions {
                mode: CardMode::Files,
                blacklist: vec!["cards.example".into()],
            },
            "combined" => CardOptions {
                mode: CardMode::Files,
                blacklist: vec!["unified_card:cards.example".into()],
            },
            _ => file_cards(),
        };
        let server = Server::new(vec![Response::json(post_response(post))]);
        let native = extractor(Duration::from_secs(3))
            .await
            .with_card_options(options);
        let mut stream = native.post_stream_at(local_post_endpoint(&server, "100"), "100".into());
        if case == "enabled" {
            let error = stream.next().await.unwrap().unwrap_err().to_string();
            assert!(!error.contains("private malformed card payload"));
        } else {
            let item = stream.next().await.unwrap().unwrap();
            assert_eq!(item.filename, "100_1.jpg");
        }
        assert!(stream.next().await.is_none());
        assert_eq!(server.count(), 1);
    }
}

fn unavailable_media(mut media: Value) -> Value {
    media["ext_media_availability"] =
        json!({"status":"Unavailable","reason":"private unavailable media reason"});
    media
}

#[tokio::test]
async fn unavailable_only_pages_do_not_stop_profile_search_or_collection_pagination() {
    for source in ["profile", "search", "bookmarks"] {
        let mut hidden = tweet("100");
        hidden["legacy"]["extended_entities"]["media"] = json!([unavailable_media(
            json!({"type":"private malformed unavailable type"})
        )]);
        let kind = collections::Collection::Bookmarks;
        let build_page = |tweets, cursor| match source {
            "profile" => page(tweets, cursor),
            "search" => search_page(tweets, cursor),
            _ => feed_page(&kind, tweets, cursor),
        };
        let server = Server::new(vec![
            Response::json(build_page(vec![hidden], Some("next"))),
            Response::json(build_page(vec![tweet("99")], None)),
        ]);
        let native = extractor(Duration::from_secs(3))
            .await
            .with_timeline_options(TimelineOptions {
                search_pagination: SearchPagination::Cursor,
                ..Default::default()
            });
        let stream = match source {
            "profile" => native.media_stream(input("id:42"), server.base.clone()),
            "search" => native.search_stream("fixture".into(), server.base.clone()),
            _ => native.feed_stream(kind, server.base.clone()),
        };
        let items: Vec<_> = stream.try_collect().await.unwrap();
        assert_eq!(post_ids(&items), ["99"], "{source}");
        assert_eq!(server.count(), 2, "{source}");
        assert_eq!(variables(&server.urls()[1])["cursor"], "next", "{source}");
    }
}

#[tokio::test]
async fn unavailable_media_default_skips_malformed_items_and_numbers_only_remaining_files() {
    let mut post = tweet("100");
    let mut first = post["legacy"]["extended_entities"]["media"][0].clone();
    first["media_url_https"] = json!("https://cdn.example/first.jpg");
    let mut last = first.clone();
    last["media_url_https"] = json!("https://cdn.example/last.jpg");
    post["legacy"]["extended_entities"]["media"] = json!([
        unavailable_media(json!({"type":"private unknown media type","video_info":"malformed"})),
        first,
        unavailable_media(json!({"type":"photo","media_url_https":{"private":"invalid URL"}})),
        last
    ]);
    let server = Server::new(vec![Response::json(post_response(post))]);
    let native = extractor(Duration::from_secs(3)).await;
    let items: Vec<_> = native
        .post_stream_at(local_post_endpoint(&server, "100"), "100".into())
        .try_collect()
        .await
        .unwrap();
    assert_eq!(post_ids(&items), ["100", "100"]);
    assert_eq!(
        items.iter().map(|item| item.url.path()).collect::<Vec<_>>(),
        ["/first.jpg", "/last.jpg"]
    );
    assert_eq!(
        items
            .iter()
            .map(|item| item.filename.as_str())
            .collect::<Vec<_>>(),
        ["100_1.jpg", "100_2.jpg"]
    );
    assert_eq!(
        items
            .iter()
            .map(|item| item.metadata.as_ref().unwrap().index)
            .collect::<Vec<_>>(),
        [1, 2]
    );
    assert_ne!(items[0].archive_key(), items[1].archive_key());
    assert_eq!(server.count(), 1);
}

#[tokio::test]
async fn unavailable_flag_attempts_supplied_links_and_keeps_enabled_errors_atomic_and_private() {
    for malformed in [false, true] {
        let mut post = tweet("100");
        let mut video = video_post("100")["legacy"]["extended_entities"]["media"][0].clone();
        if malformed {
            video["video_info"] = json!({"variants":"private unavailable variants"});
        }
        post["legacy"]["extended_entities"]["media"]
            .as_array_mut()
            .unwrap()
            .push(unavailable_media(video));
        let server = Server::new(vec![Response::json(post_response(post))]);
        let native = extractor(Duration::from_secs(3))
            .await
            .with_media_options(MediaOptions {
                unavailable: true,
                ..Default::default()
            });
        let mut stream = native.post_stream_at(local_post_endpoint(&server, "100"), "100".into());
        if malformed {
            let error = stream.next().await.unwrap().unwrap_err().to_string();
            assert!(!error.contains("private unavailable"));
            assert!(stream.next().await.is_none());
        } else {
            let items: Vec<_> = stream.by_ref().try_collect().await.unwrap();
            assert_eq!(post_ids(&items), ["100", "100"]);
            assert_eq!(
                items[1].url.as_str(),
                "https://video.example/high.mp4?tag=1"
            );
            assert_eq!(items[1].filename, "100_2.mp4");
        }
        assert_eq!(server.count(), 1);
    }
}

#[tokio::test]
async fn unavailable_selection_propagates_to_quoted_and_threaded_unified_cards_without_extra_fetches()
 {
    for thread in [false, true] {
        for unavailable in [false, true] {
            let hidden = unavailable_media(
                json!({"type":"photo","media_url_https":"https://cdn.example/unavailable.jpg"}),
            );
            let encoded = json!({"media_entities":{
                "a":hidden,
                "b":{"type":"photo","media_url_https":"https://cdn.example/available.jpg"}
            }})
            .to_string();
            let mut child = text_tweet("101");
            child["card"] = json!({"name":"unified_card","binding_values":{"unified_card":{"string_value":encoded}}});
            child["legacy"]["quoted_status_id_str"] = json!("102");
            let responses = if thread {
                vec![
                    Response::json(page(vec![in_thread(tweet("100"), json!("90"))], None)),
                    Response::json(detail_page(vec![child], Some("child-next"))),
                    Response::json(detail_page(vec![tweet("102")], None)),
                ]
            } else {
                let mut parent = text_tweet("100");
                parent["legacy"]["quoted_status_id_str"] = json!("101");
                vec![
                    Response::json(post_response(parent)),
                    Response::json(post_response(child)),
                    Response::json(post_response(tweet("102"))),
                ]
            };
            let server = Server::new(responses);
            let native = extractor(Duration::from_secs(3))
                .await
                .with_media_options(MediaOptions {
                    unavailable,
                    ..Default::default()
                })
                .with_card_options(file_cards())
                .with_expansion(expansion_options(RetweetMode::Skip))
                .with_thread_options(ThreadOptions {
                    expand: thread,
                    unique: true,
                });
            let stream = if thread {
                native.media_stream(input("id:42"), server.base.clone())
            } else {
                native.post_stream_at(local_post_endpoint(&server, "100"), "100".into())
            };
            let items: Vec<_> = stream
                .take(if unavailable { 2 } else { 1 })
                .try_collect()
                .await
                .unwrap();
            assert_eq!(
                post_ids(&items),
                vec!["101"; if unavailable { 2 } else { 1 }]
            );
            assert_eq!(
                items[0].url.path(),
                if unavailable {
                    "/unavailable.jpg"
                } else {
                    "/available.jpg"
                }
            );
            assert_eq!(items[0].filename, "101_1.jpg");
            if unavailable {
                assert_eq!(items[1].filename, "101_2.jpg");
            }
            assert_eq!(server.count(), 2);
        }
    }
}

fn article_post(mut post: Value) -> Value {
    post["article"] = json!({"article_results":{"result":{
        "rest_id":"900", "title":"Runtime article",
        "metadata":{"first_published_at_secs":1700000000},
        "lifecycle_state":{"modified_at_secs":1700000001},
        "cover_media":{"media_info":{"original_img_url":"https://cdn.example/article-cover.jpg"}},
        "media_entities":[{"media_id":"901","media_info":{"original_img_url":"https://cdn.example/article-body.png"}}],
        "content_state":{"blocks":[{"key":"a","type":"unstyled","text":"Runtime article body"}],"entityMap":[]}
    }}});
    post
}

#[tokio::test]
async fn articles_emit_documents_and_files_through_rest_id_and_detail_with_shared_metadata() {
    for detail in [false, true] {
        let post = article_post(tweet("100"));
        let response = if detail {
            detail_page(vec![post], None)
        } else {
            post_response(post)
        };
        let server = Server::new(vec![Response::json(response)]);
        let native = extractor(Duration::from_secs(3))
            .await
            .with_article_options(ArticleOptions {
                document: true,
                html: true,
                ..Default::default()
            });
        let stream = if detail {
            native.detail_stream_at(server.base.clone(), "100".into())
        } else {
            native.post_stream_at(local_post_endpoint(&server, "100"), "100".into())
        };
        let items: Vec<_> = stream.try_collect().await.unwrap();
        assert_eq!(post_ids(&items), ["100"; 4]);
        assert_eq!(
            items
                .iter()
                .map(|item| item.filename.as_str())
                .collect::<Vec<_>>(),
            ["100_1.jpg", "100_2.html", "100_3.jpg", "100_4.png"]
        );
        let crate::extract::MediaJob::InlineText { data } = &items[1].job else {
            panic!("article document was not inline text")
        };
        assert!(data.contains("Runtime article body"));
        assert_eq!(items[2].url.path(), "/article-cover.jpg");
        assert_eq!(items[3].url.path(), "/article-body.png");
        let first = items[0]
            .metadata
            .as_ref()
            .unwrap()
            .article
            .as_ref()
            .unwrap();
        assert_eq!(first.id.as_deref(), Some("900"));
        assert_eq!(first.title.as_deref(), Some("Runtime article"));
        assert_eq!(first.date, Some(1700000000));
        assert_eq!(first.date_updated, Some(1700000001));
        assert!(
            first
                .html
                .as_deref()
                .unwrap()
                .contains("Runtime article body")
        );
        for item in &items[1..] {
            let article = item.metadata.as_ref().unwrap().article.as_ref().unwrap();
            assert!(Arc::ptr_eq(
                first.html.as_ref().unwrap(),
                article.html.as_ref().unwrap()
            ));
        }
        let keys: std::collections::HashSet<_> = items
            .iter()
            .map(|item| item.archive_key().unwrap())
            .collect();
        assert_eq!(keys.len(), 4);
        assert_eq!(server.count(), 1);
    }
}

#[tokio::test]
async fn quoted_and_expanded_thread_articles_keep_inline_options_and_drop_before_followups() {
    for thread in [false, true] {
        let id = if thread { "90" } else { "101" };
        let mut child = article_post(text_tweet(id));
        child["legacy"]["quoted_status_id_str"] = json!("102");
        let responses = if thread {
            vec![
                Response::json(page(
                    vec![in_thread(tweet("100"), json!("90"))],
                    Some("parent-next"),
                )),
                Response::json(detail_page(vec![child], Some("child-next"))),
                Response::json(detail_page(vec![tweet("102")], None)),
            ]
        } else {
            vec![
                Response::json(post_response(quoted_tweet("100", id))),
                Response::json(post_response(child)),
                Response::json(post_response(tweet("102"))),
            ]
        };
        let server = Server::new(responses);
        let native = extractor(Duration::from_secs(3))
            .await
            .with_article_options(ArticleOptions {
                document: true,
                html: true,
                cover: false,
                media: false,
                ..Default::default()
            })
            .with_expansion(expansion_options(RetweetMode::Skip))
            .with_thread_options(ThreadOptions {
                expand: thread,
                unique: true,
            });
        let stream = if thread {
            native.media_stream(input("id:42"), server.base.clone())
        } else {
            native.post_stream_at(local_post_endpoint(&server, "100"), "100".into())
        };
        let items: Vec<_> = stream
            .take(if thread { 1 } else { 2 })
            .try_collect()
            .await
            .unwrap();
        let document = items.last().unwrap();
        assert_eq!(document.metadata.as_ref().unwrap().post_id, id);
        assert_eq!(document.filename, format!("{id}_1.html"));
        assert!(matches!(
            document.job,
            crate::extract::MediaJob::InlineText { .. }
        ));
        assert!(
            document
                .metadata
                .as_ref()
                .unwrap()
                .article
                .as_ref()
                .unwrap()
                .html
                .is_some()
        );
        assert_eq!(
            document.metadata.as_ref().unwrap().quoted_by_id.as_deref(),
            if thread { None } else { Some("100") }
        );
        assert_eq!(server.count(), 2);
    }
}

#[tokio::test]
async fn default_articles_ignore_null_and_malformed_optional_data_and_continue_later_pages() {
    let mut null_article = tweet("100");
    null_article["article"] = Value::Null;
    let mut malformed_text = text_tweet("101");
    malformed_text["article"] = json!({"article_results":{"result":{"rest_id":"900","title":{"private":"PRIVATE-ARTICLE-PAYLOAD"}}}});
    let mut malformed_photo = tweet("102");
    malformed_photo["article"] = json!({"article_results":"PRIVATE-ARTICLE-PAYLOAD"});
    let server = Server::new(vec![
        Response::json(page(
            vec![null_article, malformed_text, malformed_photo],
            Some("later-page"),
        )),
        Response::json(page(vec![tweet("103")], None)),
    ]);
    let native = extractor(Duration::from_secs(3)).await;
    let items: Vec<_> = native
        .media_stream(input("id:42"), server.base.clone())
        .try_collect()
        .await
        .unwrap();
    assert_eq!(post_ids(&items), ["100", "102", "103"]);
    assert_eq!(server.count(), 2);
    assert_eq!(variables(&server.urls()[1])["cursor"], "later-page");
    assert!(
        !serde_json::to_string(&items)
            .unwrap()
            .contains("PRIVATE-ARTICLE-PAYLOAD")
    );
    assert!(
        items
            .iter()
            .all(|item| item.metadata.as_ref().unwrap().article.is_none())
    );
}
