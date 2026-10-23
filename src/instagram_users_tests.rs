use super::*;
use crate::dispatch::Include;
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

/// Sequential fixture server that records complete requests, including bodies.
struct Server {
    base: Url,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Server {
    fn new(responses: Vec<Value>) -> Self {
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
            for body in responses {
                let mut socket = loop {
                    if server_stop.load(Ordering::Relaxed) {
                        return;
                    }
                    assert!(
                        std::time::Instant::now() < deadline,
                        "user fixture server deadline"
                    );
                    match listener.accept() {
                        Ok((socket, _)) => break socket,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2))
                        }
                        Err(e) => panic!("user fixture accept: {e}"),
                    }
                };
                socket.set_nonblocking(false).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let request = read_request(&mut socket);
                server_requests.lock().unwrap().push(request);
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

    fn paths(&self) -> Vec<String> {
        self.urls()
            .iter()
            .map(|url| url.path().to_owned())
            .collect()
    }

    fn urls(&self) -> Vec<Url> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|request| {
                Url::parse(&format!(
                    "http://localhost{}",
                    request.split_whitespace().nth(1).unwrap()
                ))
                .unwrap()
            })
            .collect()
    }

    fn bodies(&self) -> Vec<Vec<(String, String)>> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|request| {
                let body = request.split_once("\r\n\r\n").unwrap().1;
                url::form_urlencoded::parse(body.as_bytes())
                    .map(|(key, value)| (key.into_owned(), value.into_owned()))
                    .collect()
            })
            .collect()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap();
    }
}

fn read_request(stream: &mut std::net::TcpStream) -> String {
    let mut request = Vec::new();
    let mut buffer = [0; 1024];
    loop {
        if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            let headers = std::str::from_utf8(&request[..end]).unwrap();
            let length = headers
                .lines()
                .find_map(|line| {
                    line.split_once(':')
                        .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                        .map(|(_, value)| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            if request.len() >= end + 4 + length {
                break;
            }
        }
        let count = stream.read(&mut buffer).unwrap();
        assert_ne!(count, 0, "client closed before completing request");
        request.extend_from_slice(&buffer[..count]);
        assert!(request.len() <= 128 * 1024);
    }
    String::from_utf8(request).unwrap()
}

fn native(server: &Server) -> Instagram {
    Instagram::from_cookie_jar(CookieJar::parse("").unwrap(), Duration::from_secs(5))
        .unwrap()
        .with_api_base(server.base.clone())
}

fn url(path: &str) -> Url {
    Url::parse(&format!("https://www.instagram.com/{path}")).unwrap()
}

fn profile(name: &str, id: &str) -> Value {
    json!({"status":"ok","data":{"user":{
        "id": id,
        "username": name,
        "is_private": false,
        "biography": format!("Bio of {name}"),
        "profile_pic_url": "https://cdn.example/t51.2885-19/small_pic_n.jpg?stp=s150",
        "profile_pic_url_hd": "https://cdn.example/t51.2885-19/hd_pic_n.jpg?stp=s320",
    }}})
}

fn post(id: &str, code: &str, owner: &str) -> Value {
    let mut value: Value =
        serde_json::from_slice(include_bytes!("../tests/fixtures/instagram/photo.json")).unwrap();
    let mut post = value["items"][0].take();
    post["pk"] = json!(id);
    post["code"] = json!(code);
    post["user"]["pk"] = json!(owner);
    post
}

fn feed(posts: Vec<Value>) -> Value {
    json!({"status":"ok","items":posts,"more_available":false})
}

async fn collect(stream: BoxStream<'_, Result<ExtractionEvent>>) -> Result<Vec<ExtractionEvent>> {
    stream.try_collect().await
}

/// Compact event trace: `[` / `]` child scopes, `P` posts, `M` media, `E` ends.
fn trace(events: &[ExtractionEvent]) -> String {
    events
        .iter()
        .map(|event| match event {
            ExtractionEvent::BeginChild(_) => '[',
            ExtractionEvent::EndChild(_) => ']',
            ExtractionEvent::Post(_) => 'P',
            ExtractionEvent::Media(_) => 'M',
            ExtractionEvent::EndPost => 'E',
        })
        .collect()
}

fn media(events: &[ExtractionEvent]) -> Vec<&Media> {
    events
        .iter()
        .filter_map(|event| match event {
            ExtractionEvent::Media(media) => Some(media.as_ref()),
            _ => None,
        })
        .collect()
}

fn posts(events: &[ExtractionEvent]) -> Vec<&event::Post> {
    events
        .iter()
        .filter_map(|event| match event {
            ExtractionEvent::Post(post) => Some(post.as_ref()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn bare_profile_defaults_to_one_posts_child() {
    let server = Server::new(vec![
        profile("fixture_user", "42"),
        feed(vec![post("64", "BA", "42")]),
    ]);
    let events = collect(native(&server).events(&url("fixture_user/")))
        .await
        .unwrap();
    assert_eq!(trace(&events), "[PME]");
    let ExtractionEvent::BeginChild(scope) = &events[0] else {
        unreachable!()
    };
    assert_eq!(scope.metadata.category(), "instagram");
    assert_eq!(scope.metadata.subcategory(), "user");
    assert_eq!(media(&events)[0].metadata.as_ref().unwrap().post_id, "64");
    assert_eq!(
        server.paths(),
        ["/api/v1/users/web_profile_info/", "/api/v1/feed/user/42/"]
    );
}

#[tokio::test]
async fn bare_profile_feed_path_stays_a_direct_posts_feed() {
    let server = Server::new(vec![
        profile("fixture_user", "42"),
        feed(vec![post("64", "BA", "42")]),
    ]);
    let events = collect(native(&server).events(&url("fixture_user/feed/")))
        .await
        .unwrap();
    assert_eq!(trace(&events), "PME");
}

#[tokio::test]
async fn explicit_include_preserves_order_and_duplicates_and_skips_unknown_views() {
    let server = Server::new(vec![
        profile("fixture_user", "42"),
        profile("fixture_user", "42"),
        profile("fixture_user", "42"),
    ]);
    let native = native(&server).with_include(Some(Include::Views(vec![
        "avatar".into(),
        "bogus".into(),
        "info".into(),
        "avatar".into(),
    ])));
    let events = collect(native.events(&url("fixture_user/"))).await.unwrap();
    assert_eq!(trace(&events), "[PME][PE][PME]");
    let info = posts(&events)[1].metadata.as_ref().unwrap();
    assert_eq!(posts(&events)[1].count, Some(0));
    assert_eq!(info.post_id, "42");
    assert_eq!(info.caption.as_deref(), Some("Bio of fixture_user"));
    assert_eq!(server.paths().len(), 3);
}

#[tokio::test]
async fn empty_include_does_no_requests() {
    let server = Server::new(vec![]);
    let native = native(&server).with_include(Some(Include::Views(vec![])));
    let events = collect(native.events(&url("fixture_user/"))).await.unwrap();
    assert!(events.is_empty());
    assert!(server.paths().is_empty());
}

#[tokio::test]
async fn include_all_runs_the_complete_catalog_in_upstream_order() {
    let empty_page = json!({"status":"ok","items":[],"more_available":false});
    let server = Server::new(vec![
        // info, avatar
        profile("fixture_user", "42"),
        profile("fixture_user", "42"),
        // stories
        profile("fixture_user", "42"),
        json!({"status":"ok","reels_media":[]}),
        // highlights
        profile("fixture_user", "42"),
        json!({"status":"ok","tray":[]}),
        // posts
        profile("fixture_user", "42"),
        feed(vec![post("64", "BA", "42")]),
        // reels
        profile("fixture_user", "42"),
        json!({"status":"ok","items":[],"paging_info":{"more_available":false}}),
        // tagged
        profile("fixture_user", "42"),
        empty_page,
    ]);
    let native = native(&server).with_include(Some(Include::All));
    let events = collect(native.events(&url("fixture_user/"))).await.unwrap();
    assert_eq!(trace(&events), "[PE][PME][][][PME][][]");
    assert_eq!(
        server.paths(),
        [
            "/api/v1/users/web_profile_info/",
            "/api/v1/users/web_profile_info/",
            "/api/v1/users/web_profile_info/",
            "/api/v1/feed/reels_media/",
            "/api/v1/users/web_profile_info/",
            "/api/v1/highlights/42/highlights_tray/",
            "/api/v1/users/web_profile_info/",
            "/api/v1/feed/user/42/",
            "/api/v1/users/web_profile_info/",
            "/api/v1/clips/user/",
            "/api/v1/users/web_profile_info/",
            "/api/v1/usertags/42/feed/",
        ]
    );
}

#[tokio::test]
async fn numeric_bare_profile_dispatches_by_id() {
    let server = Server::new(vec![
        json!({"status":"ok","user":{"pk":42,"id":"42","username":"fixture_user","biography":"b"}}),
        feed(vec![post("64", "BA", "42")]),
    ]);
    let native =
        native(&server).with_include(Some(Include::Views(vec!["info".into(), "posts".into()])));
    let events = collect(native.events(&url("id:42/"))).await.unwrap();
    assert_eq!(trace(&events), "[PE][PME]");
    assert_eq!(
        server.paths(),
        ["/api/v1/users/42/info/", "/api/v1/feed/user/42/"]
    );
}

#[tokio::test]
async fn info_view_emits_metadata_only_post_and_validates_identity() {
    let server = Server::new(vec![profile("fixture_user", "42")]);
    let events = collect(native(&server).events(&url("fixture_user/info/")))
        .await
        .unwrap();
    assert_eq!(trace(&events), "PE");
    let post = posts(&events)[0];
    assert_eq!(post.category, "instagram");
    let metadata = post.metadata.as_ref().unwrap();
    assert_eq!(metadata.username.as_deref(), Some("fixture_user"));
    assert_eq!(server.urls()[0].query(), Some("username=fixture_user"));

    for response in [
        profile("someone_else", "42"),
        profile("fixture_user", "0"),
        json!({"status":"ok","data":{"user":{"id":"42","pk":"43","username":"fixture_user"}}}),
        json!({"status":"ok","data":{}}),
        json!({"status":"fail","message":"login_required"}),
    ] {
        let server = Server::new(vec![response]);
        assert!(
            collect(native(&server).events(&url("fixture_user/info/")))
                .await
                .is_err()
        );
    }
    let server = Server::new(vec![
        json!({"status":"ok","user":{"pk":"43","username":"fixture_user"}}),
    ]);
    assert!(
        collect(native(&server).events(&url("id:42/info/")))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn avatar_prefers_hd_info_and_uses_profile_picture_identity() {
    let server = Server::new(vec![json!({"status":"ok","user":{
        "pk": "42",
        "username": "fixture_user",
        "profile_pic_id": "3141592653589793238_42",
        "profile_pic_url": "https://cdn.example/small.jpg",
        "hd_profile_pic_versions": [
            {"url":"https://cdn.example/v320.jpg","width":320,"height":320},
            {"url":"https://cdn.example/v640.jpg","width":640,"height":640},
        ],
        "hd_profile_pic_url_info": {"url":"https://cdn.example/hd.webp?sig=1","width":1080,"height":1080},
    }})]);
    let events = collect(native(&server).events(&url("id:42/avatar/")))
        .await
        .unwrap();
    assert_eq!(trace(&events), "PME");
    let item = media(&events)[0];
    assert_eq!(item.url.as_str(), "https://cdn.example/hd.webp?sig=1");
    assert_eq!(item.filename, "avatar_42_3141592653589793238.webp");
    let metadata = item.metadata.as_ref().unwrap();
    let asset = metadata.profile_asset.as_ref().unwrap();
    assert_eq!(asset.kind, crate::profile_asset::ProfileAssetKind::Avatar);
    assert_eq!(
        (asset.owner_id.as_str(), asset.version.as_str()),
        ("42", "3141592653589793238")
    );
    assert_eq!(metadata.post_id, "3141592653589793238");
    assert_eq!(media_id(&metadata.shortcode).unwrap(), 3141592653589793238);
    let file = metadata.file.as_ref().unwrap();
    assert_eq!((file.width, file.height), (Some(1080), Some(1080)));
    let key = item.archive_key().unwrap();
    assert!(key.starts_with("instagram:v1:"), "{key}");
    assert_eq!(server.paths(), ["/api/v1/users/42/info/"]);

    // Versions list is the second choice.
    let mut value = json!({"status":"ok","user":{
        "pk":"42","username":"fixture_user",
        "hd_profile_pic_versions":[{"url":"https://cdn.example/v640.jpg","width":640,"height":640}],
    }});
    let server = Server::new(vec![value.take()]);
    let events = collect(native(&server).events(&url("id:42/avatar/")))
        .await
        .unwrap();
    assert_eq!(
        media(&events)[0].url.as_str(),
        "https://cdn.example/v640.jpg"
    );
}

#[tokio::test]
async fn avatar_falls_back_to_public_profile_urls() {
    let server = Server::new(vec![profile("fixture_user", "42")]);
    let events = collect(native(&server).events(&url("fixture_user/avatar/")))
        .await
        .unwrap();
    let item = media(&events)[0];
    assert_eq!(
        item.url.as_str(),
        "https://cdn.example/t51.2885-19/hd_pic_n.jpg?stp=s320"
    );
    assert_eq!(item.filename, "avatar_42_hd_pic_n.jpg");
    let metadata = item.metadata.as_ref().unwrap();
    assert_eq!(metadata.profile_asset.as_ref().unwrap().version, "hd_pic_n");
    assert_eq!(metadata.media_id, "");
    let file = metadata.file.as_ref().unwrap();
    assert_eq!((file.width, file.height), (None, None));
    assert!(item.archive_key().is_some());

    let mut missing = profile("fixture_user", "42");
    let user = missing["data"]["user"].as_object_mut().unwrap();
    user.remove("profile_pic_url_hd");
    user.insert("profile_pic_url".into(), json!("ftp://cdn.example/a.jpg"));
    let server = Server::new(vec![missing]);
    let message = collect(native(&server).events(&url("fixture_user/avatar/")))
        .await
        .unwrap_err()
        .to_string();
    assert!(message.contains("no valid avatar URL"), "{message}");
}

fn friends(names: &[(&str, &str)], cursor: Value) -> Value {
    let users: Vec<_> = names
        .iter()
        .map(|(id, name)| json!({"pk":id,"pk_id":id,"username":name,"full_name":"Name"}))
        .collect();
    json!({"status":"ok","users":users,"next_max_id":cursor})
}

#[tokio::test]
async fn followers_paginate_lazily_and_queue_deduplicated_profiles() {
    let server = Server::new(vec![
        profile("fixture_user", "42"),
        friends(&[("1", "alice"), ("2", "bob")], json!("12")),
        profile("alice", "1"),
        profile("bob", "2"),
        friends(&[("3", "carol"), ("2", "bob")], json!(24)),
        profile("carol", "3"),
        friends(&[], Value::Null),
    ]);
    let native = native(&server).with_include(Some(Include::Views(vec!["info".into()])));
    let events = collect(native.events(&url("fixture_user/followers/")))
        .await
        .unwrap();
    assert_eq!(trace(&events), "[[PE]][[PE]][[PE]]");
    let scopes: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            ExtractionEvent::BeginChild(scope) => Some(&scope.metadata),
            _ => None,
        })
        .collect();
    assert_eq!(scopes[0].subcategory(), "followers");
    assert_eq!(scopes[0].fields().get("id"), Some(&json!("1")));
    assert_eq!(scopes[1].subcategory(), "user");
    let names: Vec<_> = posts(&events)
        .iter()
        .map(|post| post.metadata.as_ref().unwrap().username.clone().unwrap())
        .collect();
    assert_eq!(
        names.iter().map(|name| &**name).collect::<Vec<_>>(),
        ["alice", "bob", "carol"]
    );
    let urls = server.urls();
    assert_eq!(urls[1].path(), "/api/v1/friendships/42/followers/");
    assert_eq!(urls[1].query(), Some("count=12"));
    assert_eq!(urls[2].query(), Some("username=alice"));
    assert_eq!(urls[4].query(), Some("count=12&max_id=12"));
    assert_eq!(urls[6].query(), Some("count=12&max_id=24"));
    assert_eq!(urls.len(), 7);
}

#[tokio::test]
async fn following_by_id_skips_lookup_and_stops_without_cursor() {
    let server = Server::new(vec![friends(&[("1", "alice")], json!(""))]);
    let native = native(&server).with_include(Some(Include::Views(vec![])));
    let events = collect(native.events(&url("id:42/following/")))
        .await
        .unwrap();
    assert_eq!(trace(&events), "[]");
    let ExtractionEvent::BeginChild(scope) = &events[0] else {
        unreachable!()
    };
    assert_eq!(scope.metadata.subcategory(), "following");
    assert_eq!(server.paths(), ["/api/v1/friendships/42/following/"]);
}

#[tokio::test]
async fn follow_lists_reject_repeated_cursors_and_invalid_users() {
    let server = Server::new(vec![
        friends(&[], json!("same")),
        friends(&[], json!("same")),
    ]);
    let message = collect(native(&server).events(&url("id:42/followers/")))
        .await
        .unwrap_err()
        .to_string();
    assert!(message.contains("cursor repeated"), "{message}");

    for response in [
        json!({"status":"ok","users":[{"pk":"1","username":"bad/name"}]}),
        json!({"status":"ok","users":[{"pk":"1"}]}),
        json!({"status":"ok","users":[{"username":"alice"}]}),
        json!({"status":"ok","users":[{"pk":"1","id":"2","username":"alice"}]}),
        json!({"status":"ok","users":[],"next_max_id":{"x":1}}),
        json!({"status":"ok"}),
    ] {
        let server = Server::new(vec![response.clone()]);
        assert!(
            collect(native(&server).events(&url("id:42/followers/")))
                .await
                .is_err(),
            "{response}"
        );
    }
}

fn sections(posts: Vec<Value>, next: Option<(&str, u64)>) -> Value {
    let medias: Vec<_> = posts
        .into_iter()
        .map(|post| json!({"media":post}))
        .collect();
    let mut value = json!({
        "status":"ok",
        "sections":[{"layout_type":"media_grid","layout_content":{"medias":medias}}],
        "more_available": next.is_some(),
    });
    if let Some((cursor, page)) = next {
        value["next_max_id"] = json!(cursor);
        value["next_page"] = json!(page);
    }
    value
}

#[tokio::test]
async fn tag_sections_post_form_and_paginate() {
    let server = Server::new(vec![
        sections(
            vec![post("64", "BA", "1"), post("65", "BB", "2")],
            Some(("opaque+cursor", 1)),
        ),
        sections(vec![post("66", "BC", "3")], None),
    ]);
    let events = collect(native(&server).events(&url("explore/tags/%E6%97%A5%E6%9C%AC/")))
        .await
        .unwrap();
    assert_eq!(trace(&events), "PMEPMEPME");
    let ids: Vec<_> = media(&events)
        .iter()
        .map(|item| item.metadata.as_ref().unwrap().post_id.clone())
        .collect();
    assert_eq!(ids, ["64", "65", "66"]);
    let paths = server.paths();
    assert_eq!(paths[0], "/api/v1/tags/%E6%97%A5%E6%9C%AC/sections/");
    let requests = server.requests.lock().unwrap().clone();
    assert!(requests.iter().all(|request| request.starts_with("POST ")));
    let bodies = server.bodies();
    let base = vec![
        ("include_persistent".to_owned(), "0".to_owned()),
        ("surface".into(), "grid".into()),
        ("tab".into(), "recent".into()),
    ];
    assert_eq!(bodies[0], base);
    let mut next = base;
    next.push(("max_id".into(), "opaque+cursor".into()));
    next.push(("page".into(), "1".into()));
    assert_eq!(bodies[1], next);
}

#[tokio::test]
async fn tag_rejects_repeated_cursors_and_malformed_sections() {
    let server = Server::new(vec![
        sections(vec![], Some(("again", 1))),
        sections(vec![], Some(("again", 2))),
    ]);
    let message = collect(native(&server).events(&url("explore/tags/sunset/")))
        .await
        .unwrap_err()
        .to_string();
    assert!(message.contains("cursor repeated"), "{message}");
    for response in [
        json!({"status":"ok","sections":[{"layout_content":{}}],"more_available":false}),
        json!({"status":"ok","sections":[{"layout_content":{"medias":[{}]}}],"more_available":false}),
        json!({"status":"ok","sections":[],"more_available":true,"next_max_id":"x"}),
        json!({"status":"ok","sections":[]}),
    ] {
        let server = Server::new(vec![response.clone()]);
        assert!(
            collect(native(&server).events(&url("explore/tags/sunset/")))
                .await
                .is_err(),
            "{response}"
        );
    }
}

#[tokio::test]
async fn guide_media_items_paginate_until_cursor_is_absent() {
    let server = Server::new(vec![
        json!({"status":"ok","items":[{"media_items":[post("64","BA","1")]},{"media_items":[post("65","BB","1")]}],"next_max_id":"next"}),
        json!({"status":"ok","items":[{"media_items":[post("66","BC","2")]}]}),
    ]);
    let events = collect(native(&server).events(&url("fixture_user/guide/my-guide/17900/")))
        .await
        .unwrap();
    assert_eq!(trace(&events), "PMEPMEPME");
    let urls = server.urls();
    assert_eq!(urls[0].path(), "/api/v1/guides/guide/17900/");
    assert_eq!(urls[0].query(), None);
    assert_eq!(urls[1].query(), Some("max_id=next"));
}
