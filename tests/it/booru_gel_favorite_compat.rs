#![cfg(unix)]
use futures_util::StreamExt;
use gallery_dl_rs::{
    download::{self, DownloadOptions},
    extract::{self, Backend},
    filter::FilterExpression,
    naming::FilenameTemplate,
    postprocess::{self, Hook, Postprocessors, Specification},
    source_archive,
};
use serde_json::json;
use std::{os::unix::fs::PermissionsExt, time::Duration};
use url::Url;

#[tokio::test]
async fn compatibility_dates_keep_source_types_private_and_use_proven_favorite_paths_and_archives()
{
    for (date, truthy, year) in [
        ("1970-01-01 00:00:00", true, "1970"),
        ("[Invalid DateTime]", false, "[Invalid DateTime]"),
    ] {
        let root = tempfile::tempdir().unwrap();
        let messages = json!([[3,"https://img3.gelbooru.com/images/raw.jpg",{"id":42,"favorite_id":7,"filename":"raw","extension":"jpg","md5":"0123456789abcdef0123456789abcdef","date_favorited":date}]]);
        let bytes = serde_json::to_vec(&messages).unwrap();
        let standalone = extract::parse_messages(&bytes, "gelbooru")
            .unwrap()
            .remove(0);
        assert!(
            standalone
                .metadata
                .as_ref()
                .unwrap()
                .source_defaults
                .is_none()
        );
        assert_eq!(
            FilterExpression::parse("bool(date_favorited)")
                .unwrap()
                .matches(&standalone)
                .unwrap(),
            truthy
        );
        assert_eq!(
            postprocess::public_keywords(&postprocess::context(&standalone))["date_favorited"],
            date
        );
        let serialized = serde_json::to_value(standalone.metadata.as_ref().unwrap()).unwrap();
        assert!(serialized.get("date_favorited").is_none());
        assert!(serialized.get("booru_keywords").is_none());
        let executable = root.path().join("backend");
        std::fs::write(&executable, "#!/bin/sh\ncat \"$0.messages\"\n").unwrap();
        std::fs::write(executable.with_extension("messages"), bytes).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let backend = Backend {
            executable,
            cookies: None,
            config: None,
            timeout: Duration::from_secs(3),
        };
        let source =
            Url::parse("https://gelbooru.com/index.php?page=favorites&s=view&id=0007").unwrap();
        let mut stream = backend.stream(&source);
        let mut media = stream.next().await.unwrap().unwrap();
        assert!(stream.next().await.is_none());
        assert_eq!(
            source_archive::for_media(&media),
            source_archive::for_url(&source)
        );
        assert_eq!(
            source_archive::for_media(&media).unwrap().format,
            "f_{favorite_id}_{id}"
        );
        let mut context = postprocess::context(&media);
        let archive = root.path().join("metadata.sqlite");
        let processors=Postprocessors::new(vec![Specification::parse(&json!({"name":"metadata","mode":"json","filename":"keywords.json","archive":archive}),root.path()).unwrap()]);
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
        let exported: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.path().join("keywords.json")).unwrap())
                .unwrap();
        assert_eq!(exported["date_favorited"], date);
        assert!(!exported.to_string().contains("$gallery_dl_source_date"));
        let connection = rusqlite::Connection::open(&archive).unwrap();
        let key: String = connection
            .query_row("SELECT entry FROM archive", [], |row| row.get(0))
            .unwrap();
        assert_eq!(key, "gelbooru_MD_f_7_42");
        (media.url, media.job) = extract::inline_text("fixture".into()).unwrap();
        let client = download::client(Duration::from_secs(2)).unwrap();
        let outcome = download::download_with_template(
            &client,
            root.path(),
            &media,
            DownloadOptions::default(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            outcome.saved_path().unwrap(),
            root.path()
                .join("gelbooru/favorites/7/gelbooru_42_0123456789abcdef0123456789abcdef.jpg")
        );
        assert_eq!(
            FilenameTemplate::parse("{date_favorited:%Y}.txt")
                .unwrap()
                .render(&media)
                .unwrap(),
            format!("{year}.txt")
        );
    }
}

#[test]
fn compatibility_date_hook_primitives_are_not_converted_into_datetimes() {
    for value in [
        json!(false),
        json!(0),
        json!(null),
        json!(17),
        json!(["raw"]),
        json!({"raw":true}),
    ] {
        let messages = json!([[3,"https://img3.gelbooru.com/images/raw.jpg",{"id":42,"favorite_id":7,"filename":"raw","extension":"jpg","date_favorited":value}]]);
        let media = extract::parse_messages(&serde_json::to_vec(&messages).unwrap(), "gelbooru")
            .unwrap()
            .remove(0);
        assert_eq!(postprocess::context(&media)["date_favorited"], value);
    }
    let messages = json!([[3,"https://cdn.example/file.jpg",{"id":42,"filename":"raw","extension":"jpg","favorite_id":7,"date_favorited":"[Invalid DateTime]"}]]);
    for category in ["danbooru", "e621", "twitter"] {
        let media = extract::parse_messages(&serde_json::to_vec(&messages).unwrap(), category)
            .unwrap()
            .remove(0);
        assert!(!postprocess::context(&media).contains_key("date_favorited"));
    }
}
