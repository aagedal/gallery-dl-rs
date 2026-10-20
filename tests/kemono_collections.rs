use futures_util::StreamExt;
use gallery_dl_rs::{
    kemono::{self, Favorites, Kemono},
    network::NetworkOptions,
    post_selection::PostSelection,
    selection::RangeSelection,
};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::TcpListener,
    thread,
    time::Duration,
};
use url::Url;

fn serve(replies: Vec<(u16, Value)>) -> (Url, thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let root = format!("http://{}/", listener.local_addr().unwrap())
        .parse()
        .unwrap();
    let task = thread::spawn(move || {
        let mut requests = Vec::new();
        for (status, body) in replies {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                let count = socket.read(&mut buffer).unwrap();
                assert_ne!(count, 0);
                request.extend_from_slice(&buffer[..count]);
            }
            requests.push(String::from_utf8(request).unwrap());
            let body = body.to_string();
            write!(socket,"HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
        }
        requests
    });
    (root, task)
}
fn extractor(root: Url) -> Kemono {
    Kemono::new(Duration::from_secs(4))
        .unwrap()
        .with_metadata(false)
        .with_api_root(root)
        .with_network(&NetworkOptions::new(Some(String::new()), None).unwrap())
        .unwrap()
}
fn first_post() -> PostSelection {
    PostSelection::new(Some(RangeSelection::parse("1").unwrap()), None)
}
fn post(id: &str) -> Value {
    json!({"id":id,"user":"42","service":"patreon","title":"Actual post","file":{"name":"cover.jpg","path":"/aa/bb/aabb.jpg"},"attachments":[]})
}

#[tokio::test]
async fn favorite_artist_post_range_counts_empty_posts_and_stops_later_children() {
    let (root, server) = serve(vec![
        (
            200,
            json!([{"id":"42","service":"patreon","name":"First","updated":20},{"id":"43","service":"fanbox","name":"Later","updated":10}]),
        ),
        (
            200,
            json!([{"id":"7","file":{},"attachments":[]},post("8")]),
        ),
    ]);
    let extractor = extractor(root);
    let url = "https://kemono.cr/account/favorites".parse().unwrap();
    let selected: Vec<_> = first_post()
        .apply(extractor.events(&url), None, None, true)
        .collect()
        .await;
    assert!(selected.is_empty());
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].starts_with("GET /api/v1/account/favorites?type=artist "));
    assert!(requests[1].starts_with("GET /api/v1/patreon/user/42/posts?o=0 "));
}

#[tokio::test]
async fn favorite_post_children_keep_direct_archive_identity_and_ignore_queue_display_names() {
    let (root, server) = serve(vec![
        (
            200,
            json!([{"id":"7","user":"42","service":"patreon","faved_seq":2,"username":"PRIVATE_QUEUE_NAME","url":"https://private.invalid/post"},{"id":"8","user":"43","service":"fanbox","faved_seq":1}]),
        ),
        (200, json!({"post":post("7")})),
    ]);
    let extractor = extractor(root).with_favorites(Favorites::Post);
    let url = "https://kemono.cr/favorites".parse().unwrap();
    let selected: Vec<_> = first_post()
        .apply(extractor.events(&url), None, None, true)
        .collect()
        .await;
    assert_eq!(selected.len(), 1);
    let selected = selected[0].as_ref().unwrap();
    let direct = kemono::parse_post(&post("7"), "kemono.cr", "patreon", "42").remove(0);
    assert_eq!(selected.archive_key(), direct.archive_key());
    assert_eq!(
        serde_json::to_value(&selected.metadata).unwrap(),
        serde_json::to_value(&direct.metadata).unwrap()
    );
    assert!(
        !serde_json::to_string(selected)
            .unwrap()
            .contains("PRIVATE_QUEUE_NAME")
    );
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].starts_with("GET /api/v1/account/favorites?type=post "));
    assert!(requests[1].starts_with("GET /api/v1/patreon/user/42/post/7 "));
}

#[tokio::test]
async fn collection_child_error_keeps_later_child_available_through_post_selection() {
    let (root, server) = serve(vec![
        (
            200,
            json!([{"id":"41","service":"patreon","updated":20},{"id":"42","service":"patreon","updated":10}]),
        ),
        (404, json!({"private":"PRIVATE_ERROR"})),
        (200, json!([post("7")])),
    ]);
    let extractor = extractor(root);
    let url = "https://kemono.cr/favorites".parse().unwrap();
    let selected: Vec<_> = first_post()
        .apply(extractor.events(&url), None, None, true)
        .collect()
        .await;
    assert_eq!(selected.len(), 2);
    assert!(selected[0].is_err());
    assert!(
        !selected[0]
            .as_ref()
            .unwrap_err()
            .to_string()
            .contains("PRIVATE_ERROR")
    );
    assert_eq!(
        selected[1]
            .as_ref()
            .unwrap()
            .metadata
            .as_ref()
            .unwrap()
            .post_id,
        "7"
    );
    assert_eq!(server.join().unwrap().len(), 3);
}
