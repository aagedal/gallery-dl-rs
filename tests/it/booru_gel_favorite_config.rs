#![cfg(unix)]
use futures_util::StreamExt;
use gallery_dl_rs::{
    config::{Config, Options},
    extract::{Backend, BackendOptions},
};
use serde_json::{Value, json};
use std::{os::unix::fs::PermissionsExt, time::Duration};
use url::Url;

fn parse(value: Value) -> Config {
    Config::parse(&serde_json::to_vec(&value).unwrap(), &std::env::temp_dir()).unwrap()
}

#[test]
fn gel_favorite_order_keeps_raw_category_types_null_and_global_precedence() {
    assert!(
        parse(json!({}))
            .resolve("gelbooru")
            .unwrap()
            .booru_favorite_order
            .is_none()
    );
    for value in [
        json!(null),
        json!(false),
        json!(0),
        json!(1),
        json!(1.5),
        json!(""),
        json!("reverse"),
        json!("R"),
        json!(["a"]),
        json!([{}]),
        json!({"0":"r"}),
    ] {
        let config = parse(json!({"extractor":{"gelbooru":{"order-posts":value}}}));
        assert_eq!(
            config.resolve("gelbooru").unwrap().booru_favorite_order,
            Some(value.clone())
        );
        assert!(
            config
                .resolve("danbooru")
                .unwrap()
                .booru_favorite_order
                .is_none()
        );
        assert!(
            config
                .resolve("e621")
                .unwrap()
                .booru_favorite_order
                .is_none()
        );
        let settings = config.resolve("gelbooru").unwrap();
        assert_eq!(
            settings
                .overlay(&Options {
                    booru_favorite_order: Some(Value::Null),
                    ..Default::default()
                })
                .booru_favorite_order,
            Some(Value::Null)
        );
    }
    for root in ["reverse", "desc"] {
        let config =
            parse(json!({"order-posts":root,"extractor":{"gelbooru":{"order-posts":["a"]}}}));
        assert_eq!(
            config.resolve("gelbooru").unwrap().booru_favorite_order,
            Some(json!(root))
        );
        let inherited = parse(json!({"extractor":{"order-posts":root}}));
        assert_eq!(
            inherited.resolve("gelbooru").unwrap().booru_favorite_order,
            Some(json!(root))
        );
    }
}

#[tokio::test]
async fn backend_forwards_exact_raw_order_json_in_one_structured_argument() {
    for value in [
        json!(null),
        json!(false),
        json!(0),
        json!("reverse"),
        json!("desc"),
        json!(["a"]),
        json!({"0":"r","space":"a b"}),
    ] {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("backend");
        std::fs::write(&executable,"#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$0.args\"\nprintf '%s' '[[3,\"https://img3.gelbooru.com/images/raw.jpg\",{\"id\":42}]]'\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let backend = Backend {
            executable: executable.clone(),
            cookies: None,
            config: None,
            timeout: Duration::from_secs(3),
        };
        let source =
            Url::parse("https://gelbooru.com/index.php?page=favorites&s=view&id=7").unwrap();
        let mut stream = backend.stream_with_options(
            &source,
            BackendOptions {
                booru_favorite_order: Some(value.clone()),
                ..Default::default()
            },
        );
        assert!(stream.next().await.unwrap().is_ok());
        assert!(stream.next().await.is_none());
        let args = std::fs::read_to_string(executable.with_extension("args")).unwrap();
        let args = args.lines().collect::<Vec<_>>();
        let expected = format!("order-posts={value}");
        assert_eq!(
            args.iter()
                .filter(|arg| arg.starts_with("order-posts="))
                .count(),
            1
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--option", expected.as_str()])
        );
    }
}
