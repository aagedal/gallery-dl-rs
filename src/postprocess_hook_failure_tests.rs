//! Processor exceptions terminate their job, while failed children leave the parent alive.
use super::*;
use crate::{
    event::{ChildMetadata, child_job},
    postprocess::Specification,
};
use serde_json::json;

#[tokio::test]
async fn a_thrown_callback_does_not_resume_the_suspended_producer() {
    let shared = Shared::new(
        Map::new(),
        Map::new(),
        KeywordPolicy::shared(&[]),
        Arc::new(AtomicBool::new(false)),
    );
    *shared.halt.lock().unwrap() = Some(Arc::new(AtomicBool::new(false)));
    let lease = shared.lease().unwrap();
    let mut item = media("pixiv-novel");
    item.metadata = Some(crate::extract::PostMetadata {
        keyword_updates: Some(
            vec![
                crate::event::KeywordUpdate::Set {
                    name: "_fallback".into(),
                    value: json!(["cover.png"]),
                },
                crate::event::KeywordUpdate::OnResume(vec![
                    crate::event::KeywordUpdate::RemoveRequired("_fallback".into()),
                ]),
            ]
            .into(),
        ),
        ..Default::default()
    });
    lease.inherit_for(&item, &mut Map::new()).await.unwrap();
    lease.fail_hook();
    drop(lease);
    assert_eq!(shared.finish().await.0["_fallback"], json!(["cover.png"]));
}

fn media(category: &str) -> Media {
    let mut media =
        crate::extract::direct(&"https://fixture.invalid/file.jpg".parse().unwrap()).unwrap();
    media.category = category.into();
    media
}
fn post(category: &str) -> ExtractionEvent {
    ExtractionEvent::Post(Box::new(Post {
        category: category.into(),
        metadata: None,
        count: None,
        keywords: Default::default(),
    }))
}
fn configuration(root: &Path, fail: bool) -> JobConfiguration {
    let mut specs = vec![
        json!({"name":"metadata","mode":"custom","event":"finalize-error","filename":"failed.log","base-directory":root,"format":"error|","open":"a"}),
    ];
    if fail {
        specs.push(json!({"name":"metadata","mode":"custom","event":"prepare","filename":"sidecar.txt","format":"é","encoding":"ascii"}));
    }
    JobConfiguration {
        processors: Arc::new(Postprocessors::new(
            specs
                .iter()
                .map(|spec| Specification::parse(spec, root).unwrap())
                .collect(),
        )),
        root: root.to_owned(),
        filename: None,
        directory: None,
        init: false,
    }
}
async fn fail_prepare(root: &Path, configuration: &JobConfiguration, item: JobMedia) {
    let client = crate::download::client(std::time::Duration::from_secs(1)).unwrap();
    let result = crate::download::download_with_options(
        &client,
        root,
        &item.media,
        crate::download::DownloadOptions {
            postprocessors: Some(&configuration.processors),
            postprocessor_scope: item.scope.as_ref(),
            ..Default::default()
        },
    )
    .await;
    assert!(result.is_err());
}
#[tokio::test]
async fn thrown_root_prepare_hook_stops_future_media_and_runs_error_finalizer() {
    let root = tempfile::tempdir().unwrap();
    let config = configuration(root.path(), true);
    let events = futures_util::stream::iter(vec![
        Ok(post("pixiv")),
        Ok(ExtractionEvent::Media(Box::new(media("pixiv")))),
        Ok(ExtractionEvent::Media(Box::new(media("pixiv")))),
        Ok(ExtractionEvent::EndPost),
    ])
    .boxed();
    let mut output = apply(
        events,
        Arc::new(BTreeMap::from([("pixiv".into(), config.clone())])),
    );
    let first = output.next().await.unwrap().unwrap();
    fail_prepare(root.path(), &config, first).await;
    assert!(output.next().await.is_none());
    assert_eq!(
        std::fs::read_to_string(root.path().join("failed.log")).unwrap(),
        "error|"
    );
}
#[tokio::test]
async fn thrown_child_prepare_hook_cancels_only_child_and_parent_resumes() {
    let root = tempfile::tempdir().unwrap();
    let parent = configuration(root.path(), false);
    let child = configuration(root.path(), true);
    let inner = child_job(
        ChildMetadata::from_raw("pixiv-novel", "novel", &Map::new()),
        || {
            futures_util::stream::iter(vec![
                Ok(post("pixiv-novel")),
                Ok(ExtractionEvent::Media(Box::new(media("pixiv-novel")))),
                Ok(ExtractionEvent::Media(Box::new(media("pixiv-novel")))),
                Ok(ExtractionEvent::EndPost),
            ])
            .boxed()
        },
    );
    let input = futures_util::stream::iter(vec![Ok(post("pixiv"))])
        .chain(inner)
        .chain(futures_util::stream::iter(vec![
            Ok(ExtractionEvent::Media(Box::new(media("pixiv")))),
            Ok(ExtractionEvent::EndPost),
        ]))
        .boxed();
    let mut output = apply(
        input,
        Arc::new(BTreeMap::from([
            ("pixiv".into(), parent),
            ("pixiv-novel".into(), child.clone()),
        ])),
    );
    let first = output.next().await.unwrap().unwrap();
    fail_prepare(root.path(), &child, first).await;
    let resumed = output.next().await.unwrap().unwrap();
    assert_eq!(resumed.media.category, "pixiv");
    drop(resumed);
    assert!(output.next().await.is_none());
    assert_eq!(
        std::fs::read_to_string(root.path().join("failed.log")).unwrap(),
        "error|error|"
    );
}

#[tokio::test]
async fn child_dispatch_exceptions_resume_parent_but_finalizer_exceptions_abort_it() {
    for hook in ["init", "post", "post-after", "child", "finalize"] {
        let root = tempfile::tempdir().unwrap();
        let parent = configuration(root.path(), false);
        let mut child = configuration(root.path(), false);
        child.processors = Arc::new(Postprocessors::new(vec![
            Specification::parse(
                &json!({"name":"metadata", "mode":"custom", "event":"post-after",
                    "filename":"closed.log", "base-directory":root.path(),
                    "format":"closed|", "open":"a"}),
                root.path(),
            )
            .unwrap(),
            Specification::parse(
                &json!({"name":"metadata", "mode":"custom", "event":hook,
                    "filename":"sidecar.txt", "format":"é", "encoding":"ascii"}),
                root.path(),
            )
            .unwrap(),
        ]));
        let inner = child_job(
            ChildMetadata::from_raw("pixiv-novel", "novel", &Map::new()),
            || {
                futures_util::stream::iter(vec![
                    Ok(post("pixiv-novel")),
                    Ok(ExtractionEvent::EndPost),
                ])
                .chain(child_job(
                    ChildMetadata::from_raw("unused", "empty", &Map::new()),
                    || futures_util::stream::empty().boxed(),
                ))
                .chain(futures_util::stream::iter(vec![
                    // An earlier callback exception must cancel before this item.
                    Ok(ExtractionEvent::Media(Box::new(media("pixiv-novel")))),
                ]))
                .boxed()
            },
        );
        let input = futures_util::stream::iter(vec![Ok(post("pixiv"))])
            .chain(inner)
            .chain(futures_util::stream::iter(vec![
                Ok(ExtractionEvent::Media(Box::new(media("pixiv")))),
                Ok(ExtractionEvent::EndPost),
            ]))
            .boxed();
        let mut output = apply(
            input,
            Arc::new(BTreeMap::from([
                ("pixiv".into(), parent),
                ("pixiv-novel".into(), child),
            ])),
        );
        let mut categories = Vec::new();
        let mut errors = 0;
        while let Some(result) = output.next().await {
            match result {
                Ok(item) => categories.push(item.media.category.clone()),
                Err(_) => errors += 1,
            }
        }
        assert_eq!(errors, 1, "{hook}");
        assert_eq!(
            std::fs::read_to_string(root.path().join("closed.log")).unwrap(),
            "closed|",
            "{hook}: initialized jobs must close their post even after init failure"
        );
        assert_eq!(
            categories,
            if hook == "finalize" {
                vec!["pixiv-novel"]
            } else {
                vec!["pixiv"]
            },
            "{hook}"
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("failed.log")).unwrap(),
            "error|",
            "{hook}"
        );
    }
}

#[tokio::test]
async fn leaf_finalizer_exception_aborts_queue_only_parent_but_not_grandparent() {
    let root = tempfile::tempdir().unwrap();
    let parent = configuration(root.path(), false);
    let mut leaf = configuration(root.path(), false);
    leaf.processors = Arc::new(Postprocessors::new(vec![
        Specification::parse(
            &json!({"name":"metadata", "mode":"custom", "event":"finalize",
                "filename":"sidecar.txt", "format":"é", "encoding":"ascii"}),
            root.path(),
        )
        .unwrap(),
    ]));
    let middle = child_job(
        ChildMetadata::from_raw("queue-only", "empty", &Map::new()),
        || {
            child_job(
                ChildMetadata::from_raw("pixiv-novel", "novel", &Map::new()),
                || futures_util::stream::iter(vec![Ok(post("pixiv-novel"))]).boxed(),
            )
            .chain(futures_util::stream::iter(vec![Ok(
                ExtractionEvent::Media(Box::new(media("queue-only"))),
            )]))
            .boxed()
        },
    );
    let input = futures_util::stream::iter(vec![Ok(post("pixiv"))])
        .chain(middle)
        .chain(futures_util::stream::iter(vec![
            Ok(ExtractionEvent::Media(Box::new(media("pixiv")))),
            Ok(ExtractionEvent::EndPost),
        ]))
        .boxed();
    let mut output = apply(
        input,
        Arc::new(BTreeMap::from([
            ("pixiv".into(), parent),
            ("pixiv-novel".into(), leaf),
        ])),
    );
    assert!(output.next().await.unwrap().is_err());
    let resumed = output.next().await.unwrap().unwrap();
    assert_eq!(resumed.media.category, "pixiv");
    drop(resumed);
    assert!(output.next().await.is_none());
    assert_eq!(
        std::fs::read_to_string(root.path().join("failed.log")).unwrap(),
        "error|"
    );
}
