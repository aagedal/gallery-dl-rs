#![cfg(unix)]
mod common;
use futures_util::StreamExt;
use gallery_dl_rs::{
    config::{Config, Options},
    extract::{Backend, BackendOptions},
    naming::{DirectoryTemplate, FilenameTemplate},
};
use serde_json::{Value, json};
use std::{os::unix::fs::PermissionsExt, time::Duration};
use url::Url;

fn parse(value: Value) -> Config {
    Config::parse(&serde_json::to_vec(&value).unwrap(), &std::env::temp_dir()).unwrap()
}

#[test]
fn inheritance_preserves_types_and_primary_presence_across_scopes() {
    for value in [
        json!(null),
        json!(false),
        json!(0),
        json!(""),
        json!([]),
        json!({}),
        json!(true),
        json!("artist-data"),
        json!([1]),
        json!({"a":1}),
    ] {
        let config = parse(json!({"extractor":{
            "parent-metadata":value,
            "danbooru":{"metadata-parent":"legacy","artist":{"metadata-parent":true}}
        }}));
        let settings = config.resolve_class("danbooru", "artist").unwrap();
        assert_eq!(settings.artist_parent_metadata(), Some(&value));
        assert_eq!(settings.metadata_parent.as_deref(), Some(&json!(true)));
        let overridden = settings.overlay(&Options {
            parent_metadata: Some(Value::Null.into()),
            ..Default::default()
        });
        assert_eq!(overridden.artist_parent_metadata(), Some(&Value::Null));
    }
    let config = parse(
        json!({"extractor":{"metadata-parent":"global","danbooru":{"artist":{"metadata-parent":false},"artist-search":{"metadata-parent":"search"}}}}),
    );
    assert_eq!(
        config
            .resolve_class("danbooru", "artist")
            .unwrap()
            .artist_parent_metadata(),
        Some(&json!(false))
    );
    assert_eq!(
        config
            .resolve_class("danbooru", "artist-search")
            .unwrap()
            .artist_parent_metadata(),
        Some(&json!("search"))
    );
    assert_eq!(
        config
            .resolve_class("danbooru", "tag")
            .unwrap()
            .artist_parent_metadata(),
        Some(&json!("global"))
    );
}

#[test]
fn class_job_options_and_declared_custom_names_resolve_without_keyword_guesses() {
    let config = parse(json!({"extractor":{"danbooru":{
        "artist":{"init":true,"parent-metadata":"artist-data"},
        "tag":{"filename":"{artist-data[name]}.{extension}","directory":["{artist-data[name]}"]}
    }}}));
    assert_eq!(
        config.resolve_class("danbooru", "artist").unwrap().init,
        Some(true)
    );
    let tag = config.resolve_class("danbooru", "tag").unwrap();
    assert!(tag.init.is_none());
    FilenameTemplate::parse_with_keywords(
        tag.filename.as_deref().unwrap(),
        config.parent_keywords(),
    )
    .unwrap();
    DirectoryTemplate::parse_with_keywords(
        tag.directory.as_deref().unwrap(),
        config.parent_keywords(),
    )
    .unwrap();
    assert!(
        FilenameTemplate::parse_with_keywords("{never_declared}", config.parent_keywords())
            .is_err()
    );
    assert!(Config::parse(br#"{"extractor":{"danbooru":{"artist":{"parent-metadata":false,"parent-metadata":true}}}}"#, &std::env::temp_dir()).is_err());
    for class in ["tag", "artist", "artist-search"] {
        for value in [json!(null), json!({})] {
            assert!(
                Config::parse(
                    &serde_json::to_vec(&json!({"extractor":{"twitter":{class:value}}})).unwrap(),
                    &std::env::temp_dir(),
                )
                .is_err()
            );
        }
    }
    for value in [
        json!([]),
        json!({"private-unknown":"PRIVATE_VALUE"}),
        json!({"threshold":1}),
    ] {
        let error = Config::parse(
            &serde_json::to_vec(&json!({"extractor":{"danbooru":{"tag":value}}})).unwrap(),
            &std::env::temp_dir(),
        )
        .unwrap_err();
        assert!(!error.to_string().contains("PRIVATE_VALUE"));
    }
}

#[tokio::test]
async fn compatibility_forwards_both_exact_json_keys_without_conflating_null_or_absence() {
    let root = tempfile::tempdir().unwrap();
    let executable = root.path().join("backend");
    std::fs::write(
        &executable,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$0.args\"\nprintf '%s' '[[3,\"https://cdn.donmai.us/original/raw.jpg\",{\"id\":42}]]'\n",
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let backend = Backend {
        executable: executable.clone(),
        cookies: None,
        config: None,
        timeout: Duration::from_secs(5),
    };
    let primary = json!({"nested":[true,null,"value with spaces `literal` $(literal)"]});
    let legacy = json!(null);
    let url = Url::parse("https://danbooru.donmai.us/artists/7").unwrap();
    let mut stream = backend.events_with_options(
        &url,
        BackendOptions {
            parent_metadata: Some(primary.clone()),
            metadata_parent: Some(legacy.clone()),
            ..Default::default()
        },
    );
    assert!(stream.next().await.unwrap().is_ok());
    assert!(stream.next().await.is_none());
    let args = std::fs::read_to_string(executable.with_extension("args")).unwrap();
    let args: Vec<_> = args.lines().collect();
    for (key, value) in [("parent-metadata", primary), ("metadata-parent", legacy)] {
        let expected = format!("{key}={value}");
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--option", expected.as_str()])
        );
    }
}

#[test]
fn cli_loads_class_scoped_templates_with_a_parent_key_declared_in_another_scope() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("native.json");
    std::fs::write(
        &config,
        serde_json::to_vec(&json!({"extractor":{"danbooru":{
            "artist":{"parent-metadata":"artist-data"},
            "tag":{"filename":"{artist-data[name]}.{extension}","directory":["{artist-data[name]}"]}
        }}}))
        .unwrap(),
    )
    .unwrap();
    let output = common::binary_command()
        .args(["--extractor", "native", "--get-urls", "--native-config"])
        .arg(&config)
        .arg("https://example.invalid/file.jpg")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        "https://example.invalid/file.jpg"
    );
    assert!(!root.path().join("downloads").exists());
}

#[test]
fn root_inheritance_values_share_allocations_across_categories_classes_and_overlays() {
    let config = parse(
        json!({"parent-metadata":{"rows":[0,1,null,false,{"name":"fixture"}]},"metadata-parent":{"legacy":["a","b"]},"extractor":{"danbooru":{"artist":{"init":true}}}}),
    );
    let category = config.resolve("danbooru").unwrap();
    let other = config.resolve("twitter").unwrap();
    let class = config.resolve_class("danbooru", "artist").unwrap();
    let overlaid = class.overlay(&Options::default());
    for settings in [&other, &class, &overlaid] {
        assert!(std::sync::Arc::ptr_eq(
            category.parent_metadata.as_ref().unwrap(),
            settings.parent_metadata.as_ref().unwrap()
        ));
        assert!(std::sync::Arc::ptr_eq(
            category.metadata_parent.as_ref().unwrap(),
            settings.metadata_parent.as_ref().unwrap()
        ));
    }
    assert_eq!(
        class.artist_parent_metadata().unwrap()["rows"][4]["name"],
        "fixture"
    );
}
