use futures_util::{StreamExt, stream};
use gallery_dl_rs::{
    download::{self, DownloadOptions},
    event::{KeywordPolicy, group_with_keywords},
    extract::{Media, MediaJob, PostMetadata},
    postprocess::{
        Postprocessors, Specification,
        jobs::{self, JobConfiguration},
    },
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs, sync::Arc, time::Duration};

fn media(index: usize) -> Media {
    Media {
        url: format!("https://fixture.invalid/{index}.txt")
            .parse()
            .unwrap(),
        job: MediaJob::InlineText {
            data: format!("file {index}"),
        },
        category: "pixiv".into(),
        filename: format!("{index}.txt"),
        metadata: Some(PostMetadata {
            post_id: "42".into(),
            media_id: format!("42_p{}", index - 1),
            index,
            caption: Some("original".into()),
            ..Default::default()
        }),
        headers: BTreeMap::new(),
        handoff: BTreeMap::new(),
    }
}

#[tokio::test]
async fn actual_file_pipeline_observes_producer_ownership_with_concurrent_consumption() {
    for policy in [
        KeywordPolicy::Independent,
        KeywordPolicy::copied_after_post(&[
            "num",
            "index",
            "media_id",
            "filename",
            "name",
            "extension",
            "url",
            "_url",
        ]),
        KeywordPolicy::shared(&[
            "num",
            "index",
            "media_id",
            "filename",
            "name",
            "extension",
            "url",
            "_url",
        ]),
    ] {
        let shared = matches!(policy, KeywordPolicy::Shared { .. });
        let independent = matches!(policy, KeywordPolicy::Independent);
        let root = tempfile::tempdir().unwrap();
        let specs = [
            json!({"name":"metadata","mode":"modify","event":"post","fields":{"post_only":"from-post"}}),
            json!({"name":"metadata","mode":"delete","event":"post","fields":["caption"]}),
            json!({"name":"metadata","event":"prepare","extension":"before.json","include":["post_only","caption","last_file","num"]}),
            json!({"name":"metadata","mode":"modify","event":"file","fields":{"last_file":"{num}"}}),
            json!({"name":"metadata","mode":"delete","event":"file","fields":["post_only"]}),
        ];
        let pp = Arc::new(Postprocessors::new(
            specs
                .iter()
                .map(|v| Specification::parse(v, root.path()).unwrap())
                .collect(),
        ));
        let configs = Arc::new(BTreeMap::from([(
            "pixiv".into(),
            JobConfiguration {
                processors: pp.clone(),
                root: root.path().to_owned(),
                filename: None,
                directory: None,
                init: false,
            },
        )]));
        let events = group_with_keywords(vec![media(1), media(2)], "pixiv", policy);
        let input = jobs::apply(stream::iter(events.into_iter().map(Ok)).boxed(), configs);
        let client = download::client(Duration::from_secs(2)).unwrap();
        let outcomes = input
            .map(|job| {
                let root = root.path();
                let client = &client;
                let pp = &pp;
                async move {
                    let job = job.unwrap();
                    download::download_with_options(
                        client,
                        root,
                        &job.media,
                        DownloadOptions {
                            postprocessors: Some(pp),
                            postprocessor_scope: job.scope.as_ref(),
                            ..Default::default()
                        },
                    )
                    .await
                    .unwrap()
                }
            })
            .buffer_unordered(2)
            .collect::<Vec<_>>()
            .await;
        assert_eq!(outcomes.len(), 2);
        let read = |index| -> Value {
            serde_json::from_slice(
                &fs::read(std::path::PathBuf::from(format!(
                    "{}.before.json",
                    download::destination(root.path(), &media(index)).display()
                )))
                .unwrap(),
            )
            .unwrap()
        };
        let first = read(1);
        let second = read(2);
        assert_eq!(
            first.get("caption"),
            independent.then_some(&json!("original"))
        );
        assert_eq!(
            second.get("caption"),
            independent.then_some(&json!("original"))
        );
        assert_eq!(
            first.get("post_only"),
            (!independent).then_some(&json!("from-post"))
        );
        assert_eq!(
            second.get("post_only"),
            (!independent && !shared).then_some(&json!("from-post"))
        );
        assert!(first.get("last_file").is_none());
        assert_eq!(second.get("last_file"), shared.then_some(&json!(1)));
    }
}

#[tokio::test]
async fn pre_directory_filename_deletion_is_not_reintroduced_from_file_snapshot() {
    let root = tempfile::tempdir().unwrap();
    let item = media(1);
    let policy = KeywordPolicy::shared(&["num", "index", "media_id"])
        .with_post_fields(&item, &["filename", "name", "extension"]);
    let options = [
        json!({"name":"metadata","mode":"delete","event":"post","fields":["filename"]}),
        json!({"name":"metadata","event":"prepare","include":["filename","extension"]}),
    ];
    let pp = Arc::new(Postprocessors::new(
        options
            .iter()
            .map(|v| Specification::parse(v, root.path()).unwrap())
            .collect(),
    ));
    let configs = Arc::new(BTreeMap::from([(
        "pixiv".into(),
        JobConfiguration {
            processors: pp.clone(),
            root: root.path().to_owned(),
            filename: None,
            directory: None,
            init: false,
        },
    )]));
    let input = jobs::apply(
        stream::iter(
            group_with_keywords(vec![item], "pixiv", policy)
                .into_iter()
                .map(Ok),
        )
        .boxed(),
        configs,
    );
    let mut input = Box::pin(input);
    let item = input.next().await.unwrap().unwrap();
    let client = download::client(Duration::from_secs(2)).unwrap();
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
    drop(item);
    assert!(input.next().await.is_none());
    let value: Value = serde_json::from_slice(
        &fs::read(std::path::PathBuf::from(format!(
            "{}.json",
            download::destination(root.path(), &media(1)).display()
        )))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(value, json!({"extension":"txt"}));
}
