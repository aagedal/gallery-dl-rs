#![cfg(unix)]
use futures_util::StreamExt;
use gallery_dl_rs::{
    download::{self, DownloadOptions},
    event::ExtractionEvent,
    extract::{self, Backend, BackendOptions},
    filter::FilterExpression,
    naming::FilenameTemplate,
    postprocess::{self, Hook, Postprocessors, Specification},
    source_archive,
};
use serde_json::{Value, json};
use std::{os::unix::fs::PermissionsExt, time::Duration};
use url::Url;

#[tokio::test]
async fn compatibility_popular_defaults_and_scalar_archives_match_recorded_source_classes() {
    let oracle: Value =
        serde_json::from_str(include_str!("fixtures/source_archive/booru_popular.json")).unwrap();
    let client = download::client(Duration::from_secs(2)).unwrap();
    let mut checked = 0;
    for case in oracle["cases"].as_array().unwrap().iter().filter(|case| {
        let name = case["name"].as_str().unwrap();
        ["_defaults", "_week_calendar", "_month", "_unicode_scale"]
            .iter()
            .any(|suffix| name.ends_with(suffix))
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
                    other => panic!("unexpected event {other}"),
                })
                .collect(),
        );
        let bytes = serde_json::to_vec(&messages).unwrap();
        let category = case["category"].as_str().unwrap();
        let standalone = extract::parse_messages(&bytes, category).unwrap().remove(0);
        assert!(
            standalone
                .metadata
                .as_ref()
                .unwrap()
                .source_defaults
                .is_none()
        );
        assert!(
            !standalone
                .metadata
                .as_ref()
                .unwrap()
                .booru_popular
                .as_ref()
                .unwrap()
                .is_empty()
        );
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
        let source = Url::parse(case["url"].as_str().unwrap()).unwrap();
        let mut stream = backend.stream(&source);
        let mut media = stream.next().await.unwrap().unwrap();
        assert!(stream.next().await.is_none());
        let descriptor = source_archive::for_media(&media).unwrap();
        assert_eq!(Some(descriptor.clone()), source_archive::for_url(&source));
        assert_eq!(descriptor.category, category);
        assert_eq!(descriptor.format, "P_{scale[0]}_{date}_{id}");
        let mut context = postprocess::context(&media);
        let public = postprocess::public_keywords(&context);
        assert_eq!(public["date"], case["source_metadata"]["date"]);
        assert_eq!(public["scale"], case["source_metadata"]["scale"]);
        assert_eq!(public["category"], category);
        assert!(!public.contains_key("num"));
        assert!(
            FilenameTemplate::parse("{date:%Y}")
                .unwrap()
                .render(&media)
                .is_err()
        );
        assert!(
            serde_json::to_value(media.metadata.as_ref().unwrap())
                .unwrap()
                .get("booru_popular")
                .is_none()
        );
        let database = root.path().join("metadata.sqlite");
        let processors=Postprocessors::new(vec![Specification::parse(&json!({"name":"metadata","mode":"json","filename":"keywords.json","archive":database}),root.path()).unwrap()]);
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
        let connection = rusqlite::Connection::open(database).unwrap();
        let key: String = connection
            .query_row("SELECT entry FROM archive", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            key,
            format!(
                "{category}_MD_P_{}_{}_42",
                public["scale"].as_str().unwrap().chars().next().unwrap(),
                public["date"].as_str().unwrap()
            )
        );
        let encoded = std::fs::read_to_string(root.path().join("keywords.json")).unwrap();
        assert!(!encoded.contains("$gallery_dl_source_"));
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
        assert_eq!(
            outcome
                .saved_path()
                .unwrap()
                .strip_prefix(root.path())
                .unwrap()
                .to_string_lossy(),
            case["published"][0]["path"].as_str().unwrap(),
            "{}",
            case["name"]
        );
        checked += 1;
    }
    assert_eq!(checked, 12);
}

#[test]
fn popular_iso_strings_remain_strings_without_overriding_canonical_creation_timestamp() {
    let raw = json!([[3,"https://cdn.donmai.us/original/raw.jpg",{"id":42,"filename":"raw","extension":"jpg","scale":"day","date":"2024-02-03T04:05:06+00:00"}]]);
    for category in ["danbooru", "e621", "e926"] {
        let mut media = extract::parse_messages(&serde_json::to_vec(&raw).unwrap(), category)
            .unwrap()
            .remove(0);
        media.metadata.as_mut().unwrap().timestamp = Some(0);
        let context = postprocess::context(&media);
        assert_eq!(
            postprocess::public_keywords(&context)["date"],
            "2024-02-03T04:05:06+00:00"
        );
        assert_eq!(media.metadata.as_ref().unwrap().timestamp, Some(0));
        assert!(
            FilterExpression::parse("date.startswith('2024') and scale[0] == 'd'")
                .unwrap()
                .matches(&media)
                .unwrap()
        );
        assert!(
            FilenameTemplate::parse("{date:%Y}")
                .unwrap()
                .render(&media)
                .is_err()
        );
        assert!(
            source_archive::for_media(&media)
                .is_none_or(|descriptor| descriptor.format != "P_{scale[0]}_{date}_{id}")
        );
    }
}

#[tokio::test]
async fn proven_popular_class_preserves_deleted_keywords_and_reassigns_iso_text_as_primitive() {
    for (date, scale) in [
        (None, Some("day")),
        (Some("2024-02-03"), None),
        (None, None),
    ] {
        let root = tempfile::tempdir().unwrap();
        let mut metadata = json!({"id":42,"filename":"raw","extension":"jpg","md5":"0123456789abcdef0123456789abcdef","created_at":"2020-01-02T03:04:05+00:00"});
        let expected_timestamp = if date.is_none() && scale.is_none() {
            metadata["created_at"] = json!(0);
            0
        } else {
            1_577_934_245
        };
        if let Some(date) = date {
            metadata["date"] = date.into();
        }
        if let Some(scale) = scale {
            metadata["scale"] = scale.into();
        }
        let messages = json!([
            [2, metadata.clone()],
            [3, "https://cdn.donmai.us/original/raw.jpg", metadata]
        ]);
        let executable = root.path().join("backend");
        std::fs::write(&executable, "#!/bin/sh\ncat \"$0.messages\"\n").unwrap();
        std::fs::write(
            executable.with_extension("messages"),
            serde_json::to_vec(&messages).unwrap(),
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let backend = Backend {
            executable,
            cookies: None,
            config: None,
            timeout: Duration::from_secs(3),
        };
        let source = Url::parse("https://danbooru.donmai.us/explore/posts/popular").unwrap();
        let mut stream = backend.events_with_options(&source, BackendOptions::default());
        let post = match stream.next().await.unwrap().unwrap() {
            ExtractionEvent::Post(post) => post,
            event => panic!("expected two-element Directory Post, got {event:?}"),
        };
        assert!(post.metadata.as_ref().unwrap().booru_popular.is_some());
        assert_eq!(
            post.metadata.as_ref().unwrap().timestamp,
            Some(expected_timestamp)
        );
        let post_media = extract::Media {
            url: source.clone(),
            job: extract::MediaJob::DirectHttp,
            category: post.category,
            filename: "post".into(),
            metadata: post.metadata,
            headers: Default::default(),
            handoff: Default::default(),
        };
        let post_keywords = postprocess::public_keywords(&postprocess::context(&post_media));
        assert_eq!(post_keywords.get("date"), date.map(Value::from).as_ref());
        assert_eq!(post_keywords.get("scale"), scale.map(Value::from).as_ref());
        let mut media = match stream.next().await.unwrap().unwrap() {
            ExtractionEvent::Media(media) => *media,
            event => panic!("expected Media, got {event:?}"),
        };
        assert!(matches!(
            stream.next().await.unwrap().unwrap(),
            ExtractionEvent::EndPost
        ));
        assert!(stream.next().await.is_none());
        assert!(media.metadata.as_ref().unwrap().booru_popular.is_some());
        assert_eq!(
            media.metadata.as_ref().unwrap().timestamp,
            Some(expected_timestamp)
        );
        let mut context = postprocess::context(&media);
        let public = postprocess::public_keywords(&context);
        assert_eq!(public.get("date"), date.map(Value::from).as_ref());
        assert_eq!(public.get("scale"), scale.map(Value::from).as_ref());
        assert_eq!(
            public["metadata"].get("date"),
            date.map(Value::from).as_ref()
        );
        assert_eq!(
            public["metadata"].get("scale"),
            scale.map(Value::from).as_ref()
        );
        let database = root.path().join("metadata.sqlite");
        let processors=Postprocessors::new(vec![Specification::parse(&json!({"name":"metadata","mode":"json","filename":"keywords.json","archive":database}),root.path()).unwrap()]);
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
        let key: String = rusqlite::Connection::open(database)
            .unwrap()
            .query_row("SELECT entry FROM archive", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            key,
            format!(
                "danbooru_MD_P_{}_{}_42",
                scale
                    .map(|s| s.chars().next().unwrap().to_string())
                    .unwrap_or_else(|| "None".into()),
                date.unwrap_or("None")
            )
        );
        (media.url, media.job) = extract::inline_text("fixture".into()).unwrap();
        let outcome = download::download_with_template(
            &download::client(Duration::from_secs(2)).unwrap(),
            root.path(),
            &media,
            DownloadOptions::default(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            outcome.saved_path().unwrap(),
            root.path().join(format!(
                "danbooru/popular/{}/{}/danbooru_42_raw.jpg",
                scale.unwrap_or("None"),
                date.unwrap_or("None")
            ))
        );
        let modify=Postprocessors::new(vec![Specification::parse(&json!({"name":"metadata","mode":"modify","event":"post","fields":{"date":"1970-01-01T00:00:00+00:00"}}),root.path()).unwrap()]);
        modify
            .run_hook(
                Hook::Post,
                &mut context,
                &media,
                &root.path().join("file.jpg"),
                root.path(),
            )
            .await
            .unwrap();
        assert_eq!(
            postprocess::public_keywords(&context)["date"],
            "1970-01-01T00:00:00+00:00"
        );
        assert!(
            postprocess::template::Template::parse("{date:%Y}")
                .unwrap()
                .render(&context)
                .is_err()
        );
        assert_eq!(
            postprocess::template::Template::parse("{date!r}")
                .unwrap()
                .render(&context)
                .unwrap(),
            "'1970-01-01T00:00:00+00:00'"
        );
    }
}
