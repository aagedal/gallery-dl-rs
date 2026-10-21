//! Compatibility boundaries for exact source booru keyword snapshots.
use crate::{
    event::{ExtractionEvent, KeywordPolicy, Post},
    extract::{self, Media},
    naming::{DirectoryTemplate, FilenameTemplate},
    postprocess::{
        Postprocessors, Specification,
        jobs::{self, JobConfiguration},
    },
};
use futures_util::StreamExt;
use serde_json::{Map, Value, json};
use std::{collections::BTreeMap, sync::Arc, time::Duration};

fn media(category: &str, fields: Value) -> Media {
    extract::parse_messages(
        &serde_json::to_vec(&json!([[3, "https://cdn.example/source.jpg", fields]])).unwrap(),
        category,
    )
    .unwrap()
    .remove(0)
}

#[test]
fn compatibility_booru_keywords_are_exact_scoped_and_not_serialized() {
    let fields = json!({"id":42,"filename":"PRIVATE_SOURCE_FILENAME","extension":"JPEG","md5":"PRIVATE_RAW_MD5"});
    for category in ["danbooru", "e621", "e926", "gelbooru", "safebooru"] {
        let mut media = media(category, fields.clone());
        // The delivery basename is independent of the source keyword snapshot.
        media.filename = "native_safe.jpg".into();
        let metadata = media.metadata.as_ref().unwrap();
        let keywords = metadata.booru_keywords.as_ref().unwrap();
        assert_eq!(keywords.as_ref(),json!({"filename":"PRIVATE_SOURCE_FILENAME","extension":"JPEG","md5":"PRIVATE_RAW_MD5"}).as_object().unwrap());
        let context = crate::postprocess::context(&media);
        for key in ["filename", "extension", "md5"] {
            assert_eq!(context[key], fields[key], "{category} {key}");
            assert_eq!(context["metadata"][key], fields[key], "{category} {key}");
        }
        let post = Post::from_media(std::slice::from_ref(&media), category);
        assert!(Arc::ptr_eq(
            post.metadata
                .as_ref()
                .unwrap()
                .booru_keywords
                .as_ref()
                .unwrap(),
            keywords
        ));
        let encoded = serde_json::to_string(metadata).unwrap();
        for private in [
            "booru_keywords",
            "PRIVATE_SOURCE_FILENAME",
            "PRIVATE_RAW_MD5",
        ] {
            assert!(!encoded.contains(private), "{category}: {encoded}");
        }
    }
    for category in [
        "twitter",
        "pixiv",
        "pixiv-novel",
        "kemono",
        "coomer",
        "instagram",
        "direct",
    ] {
        let media = media(category, fields.clone());
        assert!(
            media
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.booru_keywords.as_ref())
                .is_none(),
            "{category}"
        );
    }
}

#[test]
fn compatibility_keyword_snapshot_preserves_missing_null_empty_and_nonstring_values() {
    for values in [
        json!({"filename":"source"}),
        json!({"extension":""}),
        json!({"md5":"raw"}),
        json!({"filename":null,"extension":false,"md5":17}),
        json!({"filename":[],"extension":{"raw":true},"md5":null}),
    ] {
        let mut fields = Map::from_iter([("id".into(), json!(42))]);
        fields.extend(values.as_object().unwrap().clone());
        let mut media = media("danbooru", Value::Object(fields));
        media.filename = "canonical.png".into();
        assert_eq!(
            media
                .metadata
                .as_ref()
                .unwrap()
                .booru_keywords
                .as_ref()
                .unwrap()
                .as_ref(),
            values.as_object().unwrap()
        );
        let context = crate::postprocess::context(&media);
        for key in ["filename", "extension", "md5"] {
            assert_eq!(context.get(key), values.get(key), "{values}: {key}");
            assert_eq!(
                context["metadata"].get(key),
                values.get(key),
                "{values}: metadata.{key}"
            );
        }
    }
    let media = media("danbooru", json!({"id":42,"file":{"md5":"NESTED_ONLY"}}));
    assert!(media.metadata.as_ref().unwrap().booru_keywords.is_none());
    assert!(!crate::postprocess::context(&media).contains_key("md5"));
}

#[test]
fn filename_and_directory_templates_use_source_spelling_and_present_empty_extensions() {
    for (extension, expected) in [
        ("JPEG", "source.JPEG"),
        ("jpeg", "source.jpeg"),
        ("", "source"),
    ] {
        let mut media = media(
            "e621",
            json!({"id":42,"filename":"source","extension":extension}),
        );
        media.filename = "canonical.jpg".into();
        assert_eq!(
            FilenameTemplate::parse("{filename}.{extension}")
                .unwrap()
                .render(&media)
                .unwrap(),
            expected
        );
        assert_eq!(
            DirectoryTemplate::parse(&["{filename}.{extension}".into()])
                .unwrap()
                .render(&media)
                .unwrap()
                .unwrap(),
            std::path::PathBuf::from(expected)
        );
        assert_eq!(crate::postprocess::context(&media)["extension"], extension);
    }
}

#[tokio::test]
async fn post_filename_hooks_flow_to_file_templates_while_initial_directory_stays_fixed() {
    let root = tempfile::tempdir().unwrap();
    let mut media = media(
        "danbooru",
        json!({"id":42,"filename":"RAW","extension":"jpeg","md5":"RAW_HASH"}),
    );
    media.filename = "native_safe.jpg".into();
    (media.url, media.job) = extract::inline_text("fixture file".into()).unwrap();
    let mut post = Post::from_media(std::slice::from_ref(&media), "danbooru");
    post.keywords = KeywordPolicy::shared(&["num", "index", "media_id"])
        .with_post_fields(&media, &["filename", "name", "extension"]);
    let processors=Arc::new(Postprocessors::new(vec![
        Specification::parse(&json!({"name":"metadata","mode":"modify","event":"post","fields":{"filename":"HOOK","extension":"gif"}}),root.path()).unwrap(),
        Specification::parse(&json!({"name":"metadata","mode":"custom","event":"file","base-directory":root.path(),"filename":"seen.txt","format":"{filename}|{extension}|{md5}"}),root.path()).unwrap(),
    ]));
    let directory = DirectoryTemplate::parse(&["{filename}.{extension}".into()]).unwrap();
    let filename = FilenameTemplate::parse("{filename}.{extension}").unwrap();
    let configurations = Arc::new(BTreeMap::from([(
        "danbooru".into(),
        JobConfiguration {
            processors: processors.clone(),
            root: root.path().to_owned(),
            filename: Some(filename.clone()),
            directory: Some(directory.clone()),
            init: false,
        },
    )]));
    let input = futures_util::stream::iter(vec![
        Ok(ExtractionEvent::Post(Box::new(post))),
        Ok(ExtractionEvent::Media(Box::new(media))),
        Ok(ExtractionEvent::EndPost),
    ])
    .boxed();
    let mut jobs = jobs::apply(input, configurations);
    let client = crate::download::client(Duration::from_secs(2)).unwrap();
    let item = jobs.next().await.unwrap().unwrap();
    let outcome = crate::download::download_with_template(
        &client,
        root.path(),
        &item.media,
        crate::download::DownloadOptions {
            postprocessors: Some(&processors),
            postprocessor_scope: item.scope.as_ref(),
            directory: Some(&directory),
            ..Default::default()
        },
        Some(&filename),
    )
    .await
    .unwrap();
    assert_eq!(
        outcome.saved_path().unwrap(),
        &root.path().join("RAW.jpeg/HOOK.gif")
    );
    drop(item);
    assert!(jobs.next().await.is_none());
    assert_eq!(
        std::fs::read_to_string(root.path().join("seen.txt")).unwrap(),
        "HOOK|gif|RAW_HASH"
    );
}

#[tokio::test]
async fn source_filename_preparation_runs_without_hooks_for_downloads_and_archives() {
    use crate::{
        archive::{Archive, ArchiveOutcome},
        download::{self, DownloadOptions},
    };
    let client = download::client(Duration::from_secs(2)).unwrap();
    let empty = Postprocessors::new(Vec::new());
    let raw_directory = DirectoryTemplate::parse(&["{extension}".into()]).unwrap();
    let conditional_directory = DirectoryTemplate::parse_conditional(
        &[(
            "extension == 'jpg'".into(),
            vec!["matched-after-normalization".into()],
        )],
        None,
    )
    .unwrap();
    let filename = FilenameTemplate::parse("{filename}.{extension}").unwrap();
    for (extension, prepared) in [("jpeg", "jpg"), ("JPEG", "JPEG")] {
        for explicit_empty in [false, true] {
            for (directory, folder) in [
                (&raw_directory, extension),
                (&conditional_directory, "danbooru"),
            ] {
                let root = tempfile::tempdir().unwrap();
                let mut item = media(
                    "danbooru",
                    json!({"id":42,"filename":"source","extension":extension,"md5":"source-hash"}),
                );
                item.filename = "native-default.jpg".into();
                (item.url, item.job) = extract::inline_text("no processor hooks".into()).unwrap();
                let options = DownloadOptions {
                    postprocessors: explicit_empty.then_some(&empty),
                    directory: Some(directory),
                    ..Default::default()
                };
                let direct_root = root.path().join("direct");
                let path = direct_root.join(folder).join(format!("source.{prepared}"));
                let outcome = download::download_with_template(
                    &client,
                    &direct_root,
                    &item,
                    options,
                    Some(&filename),
                )
                .await
                .unwrap();
                assert_eq!(
                    outcome.saved_path(),
                    Some(path.as_path()),
                    "{extension} empty processors {explicit_empty}, directory {folder}"
                );
                assert_eq!(std::fs::read(&path).unwrap(), b"no processor hooks");
                assert_eq!(
                    crate::postprocess::context(&item)["extension"],
                    extension,
                    "raw producer snapshot stays unchanged"
                );

                let archive = Archive::open(root.path().join("history.sqlite3"))
                    .await
                    .unwrap();
                let archived_root = root.path().join("archived");
                let archived_path = archived_root
                    .join(folder)
                    .join(format!("source.{prepared}"));
                let identity = item.archive_key().unwrap();
                assert!(!archive.contains(&identity).await.unwrap());
                let outcome = archive
                    .download_with_template(
                        &client,
                        &archived_root,
                        &item,
                        options,
                        Some(&filename),
                    )
                    .await
                    .unwrap();
                let ArchiveOutcome::Completed(outcome) = outcome else {
                    panic!("first download must publish");
                };
                assert_eq!(outcome.saved_path(), Some(archived_path.as_path()));
                assert_eq!(
                    std::fs::read(&archived_path).unwrap(),
                    b"no processor hooks"
                );
                assert!(archive.contains(&identity).await.unwrap());
                std::fs::remove_file(&archived_path).unwrap();
                assert_eq!(
                    archive
                        .download_with_template(
                            &client,
                            &archived_root,
                            &item,
                            options,
                            Some(&filename)
                        )
                        .await
                        .unwrap(),
                    ArchiveOutcome::AlreadyArchived
                );
                assert!(!archived_path.exists(), "archive hit must skip publication");
            }
        }
    }
}
