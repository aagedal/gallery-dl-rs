use futures_util::{StreamExt, stream};
use gallery_dl_rs::{
    event::{ExtractionEvent, Post},
    extract::{Media, PostMetadata},
    naming::DirectoryTemplate,
    postprocess::{
        Postprocessors, Specification,
        jobs::{self, JobConfiguration},
    },
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs, path::Path, sync::Arc};

fn media(id: &str) -> Media {
    let mut media =
        gallery_dl_rs::extract::direct(&url::Url::parse("https://example.org/file.jpg").unwrap())
            .unwrap();
    media.category = "pixiv".into();
    media.filename = format!("{id}_1.txt");
    media.metadata = Some(PostMetadata {
        post_id: id.into(),
        media_id: format!("{id}_p1"),
        index: 2,
        ..Default::default()
    });
    media
}
fn post(id: &str) -> ExtractionEvent {
    ExtractionEvent::Post(Box::new(Post {
        keywords: Default::default(),
        category: "pixiv".into(),
        metadata: media(id).metadata,
        count: None,
    }))
}
fn configuration(root: &Path, options: Vec<Value>) -> Arc<BTreeMap<String, JobConfiguration>> {
    Arc::new(
        [(
            "pixiv".into(),
            JobConfiguration {
                processors: Arc::new(Postprocessors::new(
                    options
                        .iter()
                        .map(|options| Specification::parse(options, root).unwrap())
                        .collect(),
                )),
                root: root.to_owned(),
                filename: None,
                init: false,
                directory: Some(DirectoryTemplate::parse(&["{post_id}".into()]).unwrap()),
            },
        )]
        .into_iter()
        .collect(),
    )
}
fn source_case(name: &str) -> Value {
    let source: Value =
        serde_json::from_str(include_str!("fixtures/postprocessors/job_context.json")).unwrap();
    source["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["name"] == name)
        .unwrap()
        .clone()
}
fn read(path: impl AsRef<Path>) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

#[tokio::test]
async fn empty_first_post_writes_metadata_basename_without_invented_media_filename() {
    let source = source_case("only_empty_post");
    for record in source["records"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|record| record["event"] == "hook-before")
    {
        assert_eq!(record["path_state"]["filename"], "");
        assert_eq!(record["path_state"]["realpath"], "");
        assert_eq!(record["path_state"]["extension"], "");
        assert_eq!(record["path_state"]["directory"], "$WORK/1/");
    }
    let root = tempfile::tempdir().unwrap();
    let options=["init","post","post-after","finalize"].into_iter().map(|hook|json!({"name":"metadata","event":hook,"extension":format!("{hook}.json"),"include":["post_id","filename","extension"]})).collect();
    let input = stream::iter(vec![Ok(post("1")), Ok(ExtractionEvent::EndPost)]).boxed();
    assert!(
        jobs::apply(input, configuration(root.path(), options))
            .collect::<Vec<_>>()
            .await
            .is_empty()
    );
    for hook in ["init", "post", "post-after", "finalize"] {
        assert_eq!(
            read(root.path().join("1").join(format!("metadata.{hook}.json"))),
            json!({"post_id":1})
        );
    }
    assert_eq!(fs::read_dir(root.path().join("1")).unwrap().count(), 4);
}

#[tokio::test]
async fn new_empty_post_changes_directory_and_keeps_last_physical_basename() {
    for name in ["shared_then_empty_post", "precomputed_then_empty_post"] {
        let source = source_case(name);
        for record in source["records"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|record| record["event"] == "hook-before" && record["keywords"]["id"] == 2)
        {
            assert_eq!(record["path_state"]["filename"], "1_1.txt");
            assert_eq!(record["path_state"]["realpath"], "$WORK/1/1_1.txt");
            assert_eq!(record["path_state"]["directory"], "$WORK/2/");
            assert_eq!(record["path_state"]["extension"], "txt");
        }
    }
    let root = tempfile::tempdir().unwrap();
    let options=["init","post","post-after","finalize"].into_iter().map(|hook|json!({"name":"metadata","event":hook,"extension":format!("{hook}.json"),"include":["post_id","filename","extension"]})).collect();
    let input = stream::iter(vec![
        Ok(post("1")),
        Ok(ExtractionEvent::Media(Box::new(media("1")))),
        Ok(ExtractionEvent::EndPost),
        Ok(post("2")),
        Ok(ExtractionEvent::EndPost),
    ])
    .boxed();
    let mut jobs = jobs::apply(input, configuration(root.path(), options));
    let job = jobs.next().await.unwrap().unwrap();
    let physical = root.path().join("1/1_1.txt");
    fs::create_dir_all(physical.parent().unwrap()).unwrap();
    fs::write(&physical, b"abc").unwrap();
    let scope = job.scope.as_ref().unwrap();
    let mut keywords = scope.keywords();
    keywords.insert("filename".into(), "file-1".into());
    keywords.insert("extension".into(), "txt".into());
    scope.complete_at(&keywords, &physical);
    drop(job);
    assert!(jobs.next().await.is_none());
    assert_eq!(
        read(root.path().join("1/metadata.init.json")),
        json!({"post_id":1})
    );
    assert_eq!(
        read(root.path().join("1/1_1.txt.post-after.json")),
        json!({"post_id":1,"filename":"file-1","extension":"txt"})
    );
    for hook in ["post", "post-after", "finalize"] {
        assert_eq!(
            read(root.path().join("2").join(format!("1_1.txt.{hook}.json"))),
            json!({"post_id":2})
        );
    }
    assert!(!root.path().join("2/metadata.post.json").exists());
    assert_eq!(fs::read(&physical).unwrap(), b"abc");
}

#[tokio::test]
async fn new_empty_post_hashes_retained_realpath_and_keeps_post_after_mutations_for_finalize() {
    let root = tempfile::tempdir().unwrap();
    let options = vec![
        json!({"name":"hash","event":"post-after","hashes":"sha256"}),
        json!({"name":"metadata","event":"finalize","include":["post_id","sha256"]}),
    ];
    let input = stream::iter(vec![
        Ok(post("1")),
        Ok(ExtractionEvent::Media(Box::new(media("1")))),
        Ok(ExtractionEvent::EndPost),
        Ok(post("2")),
        Ok(ExtractionEvent::EndPost),
    ])
    .boxed();
    let mut jobs = jobs::apply(input, configuration(root.path(), options));
    let job = jobs.next().await.unwrap().unwrap();
    let physical = root.path().join("1/1_1.txt");
    fs::create_dir_all(physical.parent().unwrap()).unwrap();
    fs::write(&physical, b"abc").unwrap();
    let scope = job.scope.as_ref().unwrap();
    let mut keywords = scope.keywords();
    keywords.insert("extension".into(), "txt".into());
    scope.complete_at(&keywords, &physical);
    drop(job);
    assert!(jobs.next().await.is_none());
    assert_eq!(
        read(root.path().join("2/1_1.txt.json")),
        json!({"post_id":2,"sha256":"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"})
    );
    assert!(!root.path().join("2/1_1.txt").exists());
}
