//! Native novel producer writes run against the live postprocessor dictionary.
use futures_util::StreamExt;
use gallery_dl_rs::{
    download::{self, DownloadOptions},
    pixiv::{NovelOptions, Pixiv},
    postprocess::{
        Postprocessors, Specification,
        jobs::{self, JobConfiguration},
    },
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    net::TcpListener,
    path::Path,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};
use url::Url;
fn response(body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}
fn reply(body: Value) -> Vec<u8> {
    response(&body.to_string())
}
fn token(name: &str) -> Vec<u8> {
    reply(json!({"access_token":name,"expires_in":3600,"user":{"id":"5","account":"artist"}}))
}
fn serve() -> (Url, thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let root = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    let novel = json!({"id":42,"title":"Novel","caption":"raw","counter":10,"counter_after":20,"counter_child":30,"counter_after_child":40,"user":{"id":5,"account":"artist"},"tags":[],"x_restrict":0,"create_date":"2024-01-02T12:04:05+09:00"});
    let replies = vec![
        token("NOVEL"),
        reply(json!({"novel":novel})),
        response(&format!(
            "<script>novel: {},\n</script>",
            json!({"text":"[uploadedimage:a][pixivimage:70][pixivimage:71]"})
        )),
        reply(
            json!({"error":false,"body":{"textEmbeddedImages":{"a":{"urls":{"original":root.join("upload.png").unwrap()},"width":10,"height":20}}}}),
        ),
        response("uploaded bytes"),
        token("ART"),
        empty_art(70),
        empty_art(71),
    ];
    let handle = thread::spawn(move || {
        let mut requests = Vec::new();
        for reply in replies {
            let deadline = Instant::now() + Duration::from_secs(10);
            let (mut socket, _) = loop {
                match listener.accept() {
                    Ok(socket) => break socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "missing fixture request after {requests:?}"
                        );
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            socket.set_nonblocking(false).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            loop {
                let mut block = [0; 8192];
                let count = socket.read(&mut block).unwrap();
                assert_ne!(count, 0);
                request.extend_from_slice(&block[..count]);
                assert!(request.len() < 64 * 1024);
                if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let length = String::from_utf8_lossy(&request[..end])
                        .lines()
                        .find_map(|line| {
                            line.split_once(':')
                                .filter(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                                .map(|(_, value)| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            requests.push(String::from_utf8(request).unwrap());
            socket.write_all(&reply).unwrap();
        }
        requests
    });
    (root, handle)
}
fn empty_art(id: u64) -> Vec<u8> {
    reply(
        json!({"illust":{"id":id,"type":"ugoira","user":{"id":8},"meta_pages":[],"meta_single_page":{"original_image_url":format!("https://i.pximg.net/{id}_ugoira0.jpg")},"create_date":"2024-01-02T12:04:05+09:00"}}),
    )
}
fn native(root: &Url, token: &str) -> Pixiv {
    Pixiv::new(Duration::from_secs(5))
        .unwrap()
        .with_api_root(root.clone())
        .with_oauth_root(root.clone())
        .with_web_root(root.clone())
        .with_refresh_token(Some(token))
        .with_sanity(false)
        .with_retries(0)
}
fn processors(root: &Path) -> Arc<Postprocessors> {
    let mut options = vec![
        json!({"name":"metadata","mode":"modify","event":"post","fields":{"num":"{counter!i}","suffix":"post-suffix","caption":"post-caption"}}),
        json!({"name":"metadata","mode":"modify","event":"file","filter":"extension == 'txt'","fields":{"num":"{num:A+10/}","suffix":"file-suffix"}}),
        json!({"name":"metadata","mode":"delete","event":"file","filter":"extension == 'txt'","fields":["caption"]}),
    ];
    for hook in ["prepare", "after", "child", "child-after", "post-after"] {
        options
            .push(json!({"name":"metadata","mode":"modify","event":hook,"fields":{"phase":hook}}));
        options.push(json!({"name":"metadata","mode":"jsonl","event":hook,"base-directory":root,"directory":[],"filename":"trace.jsonl","extension":"jsonl","include":["phase","num","suffix","extension","date_url","caption"]}));
        if hook == "child" {
            options.push(json!({"name":"metadata","mode":"modify","event":"child","fields":{"num":"{counter_child!i}","suffix":"child-suffix","date_url":"child-hook"}}));
        }
        if hook == "child-after" {
            options.push(json!({"name":"metadata","mode":"modify","event":"child-after","fields":{"num":"{num:A+10/}","suffix":"after-child","date_url":"after-child-date"}}));
        }
    }
    Arc::new(Postprocessors::new(
        options
            .iter()
            .map(|option| Specification::parse(option, root).unwrap())
            .collect(),
    ))
}
#[tokio::test]
async fn native_text_upload_and_children_observe_live_post_file_and_child_mutations() {
    let root = tempfile::tempdir().unwrap();
    let (api, server) = serve();
    let leaf = Arc::new(native(&api, "ART_REFRESH").with_ugoira(false));
    let novel = native(&api, "NOVEL_REFRESH")
        .with_novel_options(NovelOptions {
            embeds: true,
            ..Default::default()
        })
        .with_artwork_extractor(leaf);
    let pp = processors(root.path());
    let artwork_pp=Arc::new(Postprocessors::new([
        json!({"name":"metadata","mode":"modify","event":"init","fields":{"artwork_init":"artwork"}}),
        json!({"name":"metadata","mode":"custom","event":"init","base-directory":root.path(),"directory":[],"filename":"artwork-init.log","open":"a","format":"{artwork_init}|"}),
    ].iter().map(|options|Specification::parse(options,root.path()).unwrap()).collect()));
    let configurations = Arc::new(BTreeMap::from([
        (
            "pixiv-novel".into(),
            JobConfiguration {
                processors: pp.clone(),
                root: root.path().to_owned(),
                filename: None,
                directory: None,
                init: false,
            },
        ),
        (
            "pixiv".into(),
            JobConfiguration {
                processors: artwork_pp,
                root: root.path().to_owned(),
                filename: None,
                directory: None,
                init: true,
            },
        ),
    ]));
    let url = Url::parse("https://www.pixiv.net/n/42").unwrap();
    let mut output = jobs::apply(novel.events(&url), configurations);
    let client = download::client(Duration::from_secs(5)).unwrap();
    let mut names = Vec::new();
    while let Some(item) = output.next().await {
        let item = item.unwrap();
        names.push(item.media.filename.clone());
        download::download_with_options(
            &client,
            root.path(),
            &item.media,
            DownloadOptions {
                postprocessors: Some(&pp),
                postprocessor_scope: item.scope.as_ref(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }
    assert_eq!(names, ["42_p0.txt", "42_p1.png"]);
    let records = fs::read_to_string(root.path().join("trace.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    let selected = records
        .iter()
        .map(|row| {
            (
                row["phase"].as_str().unwrap(),
                row["num"].as_i64().unwrap(),
                row["suffix"].as_str().unwrap(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        selected,
        [
            ("prepare", 10, "post-suffix"),
            ("after", 20, "file-suffix"),
            ("prepare", 21, "_p21"),
            ("after", 21, "_p21"),
            ("child", 22, "_p22"),
            ("child-after", 30, "child-suffix"),
            ("child", 41, "_p41"),
            ("child-after", 30, "child-suffix"),
            ("post-after", 40, "after-child")
        ]
    );
    assert_eq!(records[0]["caption"], "post-caption");
    assert!(
        records
            .iter()
            .skip(1)
            .all(|row| row.get("caption").is_none())
    );
    assert!(records[4]["date_url"].is_null());
    assert_eq!(records[6]["date_url"], "after-child-date");
    assert_eq!(
        fs::read_to_string(root.path().join("artwork-init.log")).unwrap(),
        "artwork|artwork|"
    );
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 8);
    assert!(requests[4].starts_with("GET /upload.png "));
    assert!(!requests[4].to_ascii_lowercase().contains("authorization:"));
}
