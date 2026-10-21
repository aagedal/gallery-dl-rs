use futures_util::StreamExt;
use gallery_dl_rs::{
    job_selection::JobSelection,
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
    listener.set_nonblocking(true).unwrap();
    let root = format!("http://{}/", listener.local_addr().unwrap())
        .parse()
        .unwrap();
    let task = thread::spawn(move || {
        let mut requests = Vec::new();
        for (status, body) in replies {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let mut socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "missing expected collection request"
                        );
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            socket.set_nonblocking(false).unwrap();
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
fn first_child_post() -> JobSelection {
    JobSelection {
        post_selection: Some(first_post()),
        child_range: Some(RangeSelection::parse("1").unwrap()),
        unordered: true,
        ..Default::default()
    }
}
fn post(id: &str) -> Value {
    json!({"id":id,"user":"42","service":"patreon","title":"Actual post","file":{"name":"cover.jpg","path":"/aa/bb/aabb.jpg"},"attachments":[]})
}

#[tokio::test]
async fn child_range_stops_after_fileless_first_artist_without_later_requests() {
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
    let selected: Vec<_> = first_child_post()
        .apply(extractor.events(&url))
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
    let selected: Vec<_> = first_child_post()
        .apply(extractor.events(&url))
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

#[tokio::test]
async fn post_ranges_reset_per_creator_and_cancel_each_job_before_later_pages() {
    let mut first_page = vec![json!({"id":"7","file":{},"attachments":[]})];
    first_page.extend((8..57).map(|id| post(&id.to_string())));
    let mut second = post("70");
    second["user"] = json!("43");
    let (root, server) = serve(vec![
        (
            200,
            json!([{"id":"42","service":"patreon","updated":2},{"id":"43","service":"patreon","updated":1}]),
        ),
        (200, json!(first_page)),
        (200, json!([second])),
    ]);
    let client = extractor(root);
    let url = "https://kemono.cr/favorites".parse().unwrap();
    let selected: Vec<_> = first_post()
        .apply(client.events(&url), None, None, true)
        .collect()
        .await;
    assert_eq!(selected.len(), 1);
    assert_eq!(
        selected[0]
            .as_ref()
            .unwrap()
            .metadata
            .as_ref()
            .unwrap()
            .post_id,
        "70"
    );
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(requests[1].contains("/patreon/user/42/posts?o=0"));
    assert!(requests[2].contains("/patreon/user/43/posts?o=0"));
}

#[tokio::test]
async fn file_filter_precedes_each_child_file_counter_and_finite_range_cancels_mid_post() {
    let source = |id: &str, user: &str| json!({"id":id,"user":user,"service":"patreon","file":{},"attachments":[{"name":"one.jpg","path":"/aa/bb/aabb1.jpg"},{"name":"two.jpg","path":"/aa/bb/aabb2.jpg"},{"name":"three.jpg","path":"/aa/bb/aabb3.jpg"}]});
    let (root, server) = serve(vec![
        (
            200,
            json!([{"id":"42","service":"patreon","updated":2},{"id":"43","service":"patreon","updated":1}]),
        ),
        (200, json!([source("7", "42")])),
        (200, json!([source("8", "43")])),
    ]);
    let client = extractor(root);
    let url = "https://kemono.cr/favorites".parse().unwrap();
    let selection = JobSelection {
        file_range: Some(RangeSelection::parse("2").unwrap()),
        file_filter: Some(gallery_dl_rs::filter::FilterExpression::parse("index != 2").unwrap()),
        ..Default::default()
    };
    let selected: Vec<_> = selection.apply(client.events(&url)).collect().await;
    assert_eq!(selected.len(), 2);
    let metadata = selected
        .iter()
        .map(|item| item.as_ref().unwrap().metadata.as_ref().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        metadata
            .iter()
            .map(|post| post.post_id.as_str())
            .collect::<Vec<_>>(),
        ["7", "8"]
    );
    assert!(metadata.iter().all(|post| post.index == 3));
    assert_eq!(server.join().unwrap().len(), 3);
}

#[tokio::test]
async fn child_filter_runs_before_child_range_and_invalid_identity_lookup() {
    let mut selected_post = post("7");
    selected_post["service"] = json!("fanbox");
    let (root, server) = serve(vec![
        (
            200,
            json!([{"id":"../private","service":"patreon","name":"Skip","updated":3},{"id":"42","service":"fanbox","name":"Selected","updated":2},{"id":"43","service":"patreon","name":"Later","updated":1}]),
        ),
        (200, json!([selected_post])),
    ]);
    let client = extractor(root);
    let url = "https://kemono.cr/favorites".parse().unwrap();
    let selection = JobSelection {
        child_range: Some(RangeSelection::parse("1").unwrap()),
        child_filter: Some(
            gallery_dl_rs::filter::FilterExpression::parse(
                "name == 'Selected' and service == 'fanbox'",
            )
            .unwrap(),
        ),
        ..Default::default()
    };
    let selected: Vec<_> = selection.apply(client.events(&url)).collect().await;
    assert_eq!(selected.len(), 1);
    assert!(selected[0].is_ok());
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].contains("/fanbox/user/42/posts?o=0"));
}

#[tokio::test]
async fn discord_child_filter_uses_actual_nested_channel_objects_before_requests() {
    let server: Value =
        serde_json::from_slice(include_bytes!("fixtures/kemono/discord-server.json")).unwrap();
    let message = json!({"id":"401","server":"100","channel":"201","content":"Image","attachments":[{"name":"cover.jpg","path":"/aa/bb/aabb.jpg"}]});
    let (root, server_task) = serve(vec![(200, server), (200, json!([message]))]);
    let client = extractor(root);
    let url = "https://kemono.cr/discord/server/100".parse().unwrap();
    let selection=JobSelection{child_range:Some(RangeSelection::parse("1").unwrap()),child_filter:Some(gallery_dl_rs::filter::FilterExpression::parse("channel['type'] == 0 and channel['id'] == '201' and server['name'] == 'Fixture Server'").unwrap()),..Default::default()};
    let selected: Vec<_> = selection.apply(client.events(&url)).collect().await;
    assert_eq!(selected.len(), 1);
    assert_eq!(
        selected[0]
            .as_ref()
            .unwrap()
            .metadata
            .as_ref()
            .unwrap()
            .discord
            .as_ref()
            .unwrap()
            .channel_id,
        "201"
    );
    let requests = server_task.join().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].contains("/discord/channel/201?o=0"));
}

#[tokio::test]
async fn nested_favorite_discord_child_ranges_stop_each_parent_after_selected_child() {
    let server: Value =
        serde_json::from_slice(include_bytes!("fixtures/kemono/discord-server.json")).unwrap();
    let messages: Value =
        serde_json::from_slice(include_bytes!("fixtures/kemono/discord-channel.json")).unwrap();
    let (root, server_task) = serve(vec![
        (
            200,
            json!([{"id":"100","service":"discord","updated":2},{"id":"42","service":"patreon","updated":1}]),
        ),
        (200, server),
        (200, messages),
    ]);
    let client = extractor(root);
    let url = "https://kemono.cr/favorites".parse().unwrap();
    let selected: Vec<_> = first_child_post()
        .apply(client.events(&url))
        .collect()
        .await;
    assert_eq!(selected.len(), 1);
    assert_eq!(
        selected[0]
            .as_ref()
            .unwrap()
            .metadata
            .as_ref()
            .unwrap()
            .post_id,
        "400"
    );
    let requests = server_task.join().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(requests[2].contains("/discord/channel/200?o=0"));
}
