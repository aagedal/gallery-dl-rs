#![cfg(unix)]

use futures_util::StreamExt;
use gallery_dl_rs::{
    download::{self, DownloadOptions},
    event::ExtractionEvent,
    extract::{self, Backend, BackendOptions, Media},
    postprocess::{self, Hook, Postprocessors, Specification},
    source_archive,
};
use serde_json::{Value, json};
use std::{os::unix::fs::PermissionsExt, path::Path, time::Duration};
use url::Url;

fn parsed(category: &str, owner: Option<Value>) -> Media {
    let mut metadata = json!({"id":42,"filename":"raw","extension":"jpg"});
    if let Some(owner) = owner {
        metadata["user_id"] = owner;
    }
    extract::parse_messages(
        &serde_json::to_vec(&json!([[3, "https://cdn.example/file.jpg", metadata]])).unwrap(),
        category,
    )
    .unwrap()
    .remove(0)
}

fn backend(root: &Path, messages: &Value) -> Backend {
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

#[test]
fn compatibility_owner_preserves_exact_presence_types_and_category_scope() {
    for category in [
        "e621",
        "e926",
        "danbooru",
        "gelbooru",
        "safebooru",
        "twitter",
        "pixiv",
    ] {
        for owner in [
            None,
            Some(Value::Null),
            Some(json!("")),
            Some(json!("007")),
            Some(json!(7)),
            Some(json!(false)),
            Some(json!(["x"])),
            Some(json!({"id":7})),
        ] {
            let media = parsed(category, owner.clone());
            let metadata = media.metadata.as_ref().unwrap();
            let expected = matches!(category, "e621" | "e926")
                .then_some(owner.clone())
                .flatten();
            assert_eq!(metadata.booru_user_id, expected, "{category}");
            assert!(
                !serde_json::to_string(metadata)
                    .unwrap()
                    .contains("booru_user_id")
            );
            assert!(
                serde_json::to_value(metadata)
                    .unwrap()
                    .get("user_id")
                    .is_none()
            );
            if matches!(category, "e621" | "e926") {
                assert_eq!(postprocess::context(&media).get("user_id"), owner.as_ref());
            }
            assert!(metadata.source_defaults.is_none());
            assert_ne!(
                source_archive::for_media(&media).map(|d| d.format),
                Some("f_{user_id}_{id}".into())
            );
        }
    }
}

#[tokio::test]
async fn proven_backend_favorite_class_controls_paths_and_direct_metadata_archives() {
    let oracle: Value = serde_json::from_str(include_str!(
        "../fixtures/source_archive/booru_favorites.json"
    ))
    .unwrap();
    let client = download::client(Duration::from_secs(2)).unwrap();
    let mut checked = 0;
    for case in oracle["cases"].as_array().unwrap().iter().filter(|case| {
        [
            "e621_missing",
            "e621_owner",
            "e621_duplicates",
            "e926_missing",
            "e926_owner",
            "e926_duplicates",
        ]
        .contains(&case["name"].as_str().unwrap())
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
        let source = Url::parse(case["url"].as_str().unwrap()).unwrap();
        let backend = backend(root.path(), &messages);
        let mut events = backend.events_with_options(&source, BackendOptions::default());
        let mut paths = Vec::new();
        while let Some(event) = events.next().await {
            match event.unwrap() {
                ExtractionEvent::Post(post) => {
                    assert!(post.metadata.unwrap().source_defaults.is_some())
                }
                ExtractionEvent::Media(mut media) => {
                    let descriptor = source_archive::for_media(&media).unwrap();
                    assert_eq!(Some(descriptor.clone()), source_archive::for_url(&source));
                    assert_eq!(descriptor.format, "f_{user_id}_{id}");
                    assert_eq!(descriptor.category, case["category"].as_str().unwrap());
                    let mut context = postprocess::context(&media);
                    assert_eq!(context["user_id"], case["source_metadata"]["user_id"]);
                    // No Job/ScopeLease: source class provenance alone must select
                    // the favorite archive descriptor for a file metadata hook.
                    let archive = root.path().join("metadata.sqlite");
                    let processors = Postprocessors::new(vec![Specification::parse(&json!({"name":"metadata","mode":"custom","format":"fixture", "filename":"sidecar.txt","archive":archive}), root.path()).unwrap()]);
                    processors
                        .run_hook(
                            Hook::File,
                            &mut context,
                            &media,
                            &root.path().join("file.jpg"),
                            root.path(),
                        )
                        .await
                        .unwrap();
                    processors
                        .run_hook(
                            Hook::Finalize,
                            &mut context,
                            &media,
                            &root.path().join("file.jpg"),
                            root.path(),
                        )
                        .await
                        .unwrap();
                    let connection = rusqlite::Connection::open(&archive).unwrap();
                    let keys: Vec<String> = connection
                        .prepare("SELECT entry FROM archive")
                        .unwrap()
                        .query_map([], |row| row.get(0))
                        .unwrap()
                        .collect::<Result<_, _>>()
                        .unwrap();
                    assert_eq!(
                        keys,
                        vec![format!(
                            "{}_MD_f_{}_42",
                            descriptor.category,
                            context["user_id"].as_str().unwrap()
                        )]
                    );
                    (media.url, media.job) = extract::inline_text("fixture".into()).unwrap();
                    let outcome = download::download_with_template(
                        &client,
                        root.path(),
                        &media,
                        DownloadOptions::default(),
                        None,
                    )
                    .await
                    .unwrap();
                    paths.push(
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
        assert_eq!(paths, expected, "{}", case["name"]);
        checked += 1;
    }
    assert_eq!(checked, 6);
}

#[tokio::test]
async fn backend_unknown_route_owner_does_not_invent_favorite_provenance() {
    let root = tempfile::tempdir().unwrap();
    let backend = backend(
        root.path(),
        &json!([[3,"https://cdn.example/file.jpg",{"id":42,"user_id":"7","filename":"raw","extension":"jpg"}]]),
    );
    let source = Url::parse("https://e621.net/help").unwrap();
    let media = backend.stream(&source).next().await.unwrap().unwrap();
    assert!(media.metadata.as_ref().unwrap().source_defaults.is_none());
    assert_ne!(
        source_archive::for_media(&media).map(|d| d.format),
        Some("f_{user_id}_{id}".into())
    );
}
