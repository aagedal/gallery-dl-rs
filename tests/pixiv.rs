use gallery_dl_rs::{
    archive::{Archive, ArchiveOutcome},
    download::{self, DownloadOptions},
    extract::Media,
    filter::FilterExpression,
    naming::FilenameTemplate,
    pixiv,
};
use serde_json::json;
use std::{
    io::{Read, Write},
    net::TcpListener,
    thread,
    time::Duration,
};

fn animation() -> Media {
    pixiv::parse_ugoira(
        &json!({"id":42,"type":"ugoira","user":{"name":"Alice","account":"alice"}}),
        &serde_json::to_vec(&json!({"ugoira_metadata": {
            "zip_urls":{"medium":"https://i.pximg.net/img-zip-ugoira/42_ugoira600x600.zip"},
            "frames":[{"file":"000000.png","delay":70},{"file":"000001.png","delay":120}]
        }}))
        .unwrap(),
    )
    .unwrap()
    .remove(0)
}

#[test]
fn ordered_frame_timing_is_available_to_inspection_filters_and_templates() {
    let media = animation();
    let json = serde_json::to_value(&media).unwrap();
    assert_eq!(
        json["metadata"]["frames"],
        json!([
            {"file":"000000.png","delay":70}, {"file":"000001.png","delay":120}
        ])
    );
    assert!(FilterExpression::parse("len(frames) == 2 and frames[0]['delay'] == 70 and metadata['frames'][-1]['file'] == '000001.png'").unwrap().matches(&media).unwrap());
    assert_eq!(
        FilenameTemplate::parse(
            "{post_id}_{frames[0][delay]}_{metadata[frames][-1][delay]}.{extension}"
        )
        .unwrap()
        .render(&media)
        .unwrap(),
        "42_70_120.zip"
    );
    let mut changed = media.clone();
    changed.metadata.as_mut().unwrap().frames = None;
    assert_eq!(changed.archive_key(), media.archive_key());
    assert_eq!(changed.identity(), media.identity());
}

#[test]
fn compatibility_zip_frames_and_page_identity_match_native_metadata() {
    let native = animation();
    let metadata = json!({
        "id":42, "num":0, "filename":"42_ugoira1920x1080", "extension":"zip",
        "user":{"account":"alice"}, "frames":[
            {"file":"000000.png","delay":70,"private":"omitted"},
            {"file":"000001.png","delay":120}
        ], "_ugoira_frame_data":"private data", "private":"omitted"
    });
    let parse = |metadata| {
        gallery_dl_rs::extract::parse_messages(
            &serde_json::to_vec(&json!([[3, native.url, metadata]])).unwrap(),
            "pixiv",
        )
        .unwrap()
        .remove(0)
    };
    let media = parse(metadata.clone());
    assert_eq!(media.archive_key(), native.archive_key());
    let post = media.metadata.as_ref().unwrap();
    assert_eq!(post.index, 1);
    assert_eq!(post.frames, native.metadata.as_ref().unwrap().frames);
    assert!(!serde_json::to_string(&media).unwrap().contains("private"));
    let mut invalid = metadata;
    invalid["frames"][0]["file"] = json!("../private");
    let media = parse(invalid);
    assert!(media.metadata.as_ref().unwrap().frames.is_none());
    assert_eq!(media.archive_key(), native.archive_key());
}

#[tokio::test]
async fn ugoira_zip_publication_records_archive_and_preserves_timing_sidecar() {
    let bytes = include_bytes!("fixtures/pixiv/ugoira.zip");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/animation.zip", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut request = Vec::new();
        let mut buffer = [0; 1024];
        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = socket.read(&mut buffer).unwrap();
            assert!(n > 0 && request.len() < 64 * 1024);
            request.extend_from_slice(&buffer[..n]);
        }
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/zip\r\nConnection: close\r\n\r\n",
            bytes.len()
        );
        socket.write_all(header.as_bytes()).unwrap();
        socket.write_all(bytes).unwrap();
        String::from_utf8(request).unwrap()
    });
    let root = tempfile::tempdir().unwrap();
    let archive = Archive::open(root.path().join("history.sqlite"))
        .await
        .unwrap();
    let mut media = animation();
    let key = media.archive_key().unwrap();
    media.url = url.parse().unwrap();
    let client = download::client(Duration::from_secs(3)).unwrap();
    let outcome = archive
        .download(
            &client,
            root.path(),
            &media,
            DownloadOptions {
                write_metadata: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let ArchiveOutcome::Completed(outcome) = outcome else {
        panic!("expected publication")
    };
    let path = outcome.saved_path().unwrap();
    assert_eq!(std::fs::read(path).unwrap(), bytes);
    let sidecar: serde_json::Value =
        serde_json::from_slice(&std::fs::read(format!("{}.json", path.display())).unwrap())
            .unwrap();
    assert_eq!(
        sidecar["metadata"]["frames"],
        json!([
            {"file":"000000.png","delay":70}, {"file":"000001.png","delay":120}
        ])
    );
    assert!(!sidecar.to_string().contains("i.pximg.net"));
    assert!(archive.contains(&key).await.unwrap());
    assert!(matches!(
        archive
            .download(&client, root.path(), &media, DownloadOptions::default())
            .await
            .unwrap(),
        ArchiveOutcome::AlreadyArchived
    ));
    let request = server.join().unwrap().to_ascii_lowercase();
    assert!(request.contains("referer: https://app-api.pixiv.net/"));
    assert!(!request.contains("authorization:"));
}
