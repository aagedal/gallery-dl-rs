use futures_util::{StreamExt, stream};
use gallery_dl_rs::{
    event::{ExtractionEvent, Post},
    naming::FilenameTemplate,
    postprocess::{
        Postprocessors, Specification,
        jobs::{self, JobConfiguration},
    },
};
use serde_json::json;
use std::{collections::BTreeMap, sync::Arc};

#[tokio::test]
async fn job_hash_rebuild_is_visible_to_next_metadata_hook_without_moving_file() {
    for custom in [false, true] {
        check_job_hash(custom).await;
    }
}

async fn check_job_hash(custom: bool) {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source.jpg");
    std::fs::write(&source, b"data").unwrap();
    let media =
        gallery_dl_rs::extract::direct(&url::Url::parse("https://example.org/source.jpg").unwrap())
            .unwrap();
    let category = media.category.clone();
    let processors = Postprocessors::new(
        [
            json!({"name":"hash", "hashes":"md5", "filename":true,"event":"post-after"}),
            json!({"name":"metadata","event":"post-after"}),
        ]
        .iter()
        .map(|v| Specification::parse(v, directory.path()).unwrap())
        .collect(),
    );
    let configurations = Arc::new(BTreeMap::from([(
        category,
        JobConfiguration {
            processors: Arc::new(processors),
            root: directory.path().to_owned(),
            filename: custom.then(|| FilenameTemplate::parse("{md5}.txt").unwrap()),
            directory: Some(gallery_dl_rs::naming::DirectoryTemplate::parse(&[]).unwrap()),
            init: false,
        },
    )]));
    let events = vec![
        Ok(ExtractionEvent::Post(Box::new(Post::from_media(
            std::slice::from_ref(&media),
            &media.category,
        )))),
        Ok(ExtractionEvent::Media(Box::new(media))),
        Ok(ExtractionEvent::EndPost),
    ];
    let mut output = jobs::apply(stream::iter(events).boxed(), configurations);
    let item = output.next().await.unwrap().unwrap();
    item.scope
        .as_ref()
        .unwrap()
        .complete_at(&gallery_dl_rs::postprocess::context(&item.media), &source);
    drop(item);
    assert!(output.next().await.is_none());
    let digest = "8d777f385d3dfec8815d20f7496026dc";
    let metadata = directory.path().join(if custom {
        format!("{digest}.txt.json")
    } else {
        "source.jpg.json".into()
    });
    let value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(metadata).unwrap()).unwrap();
    assert_eq!(value["md5"], digest);
    assert!(source.exists());
    assert!(!directory.path().join(format!("{digest}.txt")).exists());
}
