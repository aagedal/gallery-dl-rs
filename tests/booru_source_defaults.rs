#![cfg(unix)]

use futures_util::StreamExt;
use gallery_dl_rs::{
    download::{self, DownloadOptions},
    event::ExtractionEvent,
    extract::{self, Backend, BackendOptions},
};
use serde_json::{Value, json};
use std::{os::unix::fs::PermissionsExt, path::Path, sync::Arc, time::Duration};
use url::Url;

fn fixture(root: &Path, messages: &Value) -> Backend {
    let executable = root.join("backend");
    std::fs::write(&executable, "#!/bin/sh\ncat \"$0.messages\"\n").unwrap();
    std::fs::write(
        executable.with_extension("messages"),
        serde_json::to_vec(messages).unwrap(),
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    Backend {
        executable,
        cookies: None,
        config: None,
        timeout: Duration::from_secs(3),
    }
}

#[tokio::test]
async fn backend_root_provenance_retains_private_defaults_and_publishes_source_paths() {
    let oracle: Value =
        serde_json::from_str(include_str!("fixtures/source_archive/booru_defaults.json")).unwrap();
    let client = download::client(Duration::from_secs(2)).unwrap();
    let mut checked = 0;
    for case in oracle["cases"].as_array().unwrap().iter().filter(|case| {
        case["name"].as_str().unwrap().ends_with("_default")
            && case["overrides"].as_object().unwrap().is_empty()
            && case["hook_options"].as_array().unwrap().is_empty()
    }) {
        let root = tempfile::tempdir().unwrap();
        let messages = Value::Array(
            case["extraction_events"]
                .as_array()
                .unwrap()
                .iter()
                .map(|event| match event["kind"].as_str().unwrap() {
                    "post" => json!([2, event["metadata"]]),
                    "media" => json!([3, event["url"], event["metadata"]]),
                    other => panic!("unexpected source event {other}"),
                })
                .collect(),
        );
        let category = case["category"].as_str().unwrap();
        // A bare message array has no extractor-class provenance.
        for media in
            extract::parse_messages(&serde_json::to_vec(&messages).unwrap(), category).unwrap()
        {
            assert!(media.metadata.unwrap().source_defaults.is_none());
        }
        let backend = fixture(root.path(), &messages);
        let url = Url::parse(case["url"].as_str().unwrap()).unwrap();
        let mut events = backend.events_with_options(&url, BackendOptions::default());
        let typed_category = category;
        let mut post_defaults = None;
        let mut published = Vec::new();
        while let Some(event) = events.next().await {
            match event.unwrap() {
                ExtractionEvent::Post(post) => {
                    assert_eq!(post.category, typed_category);
                    let post_media = extract::Media {
                        url: Url::parse("https://invalid.invalid/").unwrap(),
                        job: extract::MediaJob::DirectHttp,
                        category: post.category.clone(),
                        filename: "post".into(),
                        metadata: post.metadata.clone(),
                        headers: Default::default(),
                        handoff: Default::default(),
                    };
                    assert_eq!(
                        gallery_dl_rs::postprocess::context(&post_media)["category"],
                        category
                    );
                    let metadata = post.metadata.unwrap();
                    assert!(
                        !serde_json::to_string(&metadata)
                            .unwrap()
                            .contains("source_defaults")
                    );
                    post_defaults = Some(metadata.source_defaults.unwrap());
                }
                ExtractionEvent::Media(mut media) => {
                    assert_eq!(media.category, typed_category);
                    let descriptor = gallery_dl_rs::source_archive::for_media(&media).unwrap();
                    assert_eq!(descriptor.category, category);
                    assert_eq!(descriptor.format, case["archive_format"].as_str().unwrap());
                    assert_eq!(
                        gallery_dl_rs::postprocess::context(&media)["category"],
                        category
                    );
                    let metadata = media.metadata.as_ref().unwrap();
                    assert!(Arc::ptr_eq(
                        metadata.source_defaults.as_ref().unwrap(),
                        post_defaults.as_ref().unwrap()
                    ));
                    assert!(
                        !serde_json::to_string(metadata)
                            .unwrap()
                            .contains("source_defaults")
                    );
                    (media.url, media.job) = extract::inline_text("source fixture".into()).unwrap();
                    let outcome = download::download_with_template(
                        &client,
                        root.path(),
                        &media,
                        DownloadOptions::default(),
                        None,
                    )
                    .await
                    .unwrap();
                    published.push(
                        outcome
                            .saved_path()
                            .unwrap()
                            .strip_prefix(root.path())
                            .unwrap()
                            .to_string_lossy()
                            .into_owned(),
                    );
                }
                ExtractionEvent::EndPost => {}
                other => panic!("unexpected backend event {other:?}"),
            }
        }
        let expected: Vec<_> = case["published"]
            .as_array()
            .unwrap()
            .iter()
            .map(|file| file["path"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(published, expected, "{}", case["name"]);
        checked += 1;
    }
    assert_eq!(checked, 14);
}

#[tokio::test]
async fn backend_does_not_infer_source_defaults_from_unrelated_collection_keywords() {
    let messages = json!([
        [2, {"id":42,"filename":"raw","extension":"jpg","pool":{"id":9,"name":"pool"},"favgroup":{"id":8,"name":"group"}}],
        [3,"https://cdn.example/physical.jpg",{"id":42,"filename":"raw","extension":"jpg","pool":{"id":9,"name":"pool"},"favgroup":{"id":8,"name":"group"}}]
    ]);
    for url in [
        "https://danbooru.donmai.us/wiki_pages/help",
        "https://e621.net/help",
        "https://x.com/fixture/status/42",
    ] {
        let root = tempfile::tempdir().unwrap();
        let backend = fixture(root.path(), &messages);
        let mut events =
            backend.events_with_options(&Url::parse(url).unwrap(), BackendOptions::default());
        let mut count = 0;
        while let Some(event) = events.next().await {
            let metadata = match event.unwrap() {
                ExtractionEvent::Post(post) => post.metadata,
                ExtractionEvent::Media(media) => media.metadata,
                ExtractionEvent::EndPost => continue,
                other => panic!("unexpected event {other:?}"),
            };
            assert!(
                metadata.is_none_or(|metadata| metadata.source_defaults.is_none()),
                "{url}"
            );
            count += 1;
        }
        assert_eq!(count, 2, "{url}");
    }
}
