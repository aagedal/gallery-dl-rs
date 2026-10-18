use gallery_dl_rs::config::{Config, MAX_CONFIG_BYTES, Options};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

fn directory() -> PathBuf {
    std::env::temp_dir().join("gallery-dl-config-fixtures")
}

fn parse(json: &str) -> gallery_dl_rs::Result<Config> {
    Config::parse(json.as_bytes(), &directory())
}

#[test]
fn defaults_are_sparse_and_cli_values_take_precedence() {
    let empty = parse("{}").unwrap();
    assert_eq!(empty.resolve("direct").unwrap(), Options::default());
    let config =
        parse(r#"{"extractor":{"retries":4,"timeout":2.5,"filename":"{name}.{extension}"}}"#)
            .unwrap();
    let settings = config.resolve("instagram").unwrap();
    assert_eq!(settings.timeout, Some(Duration::from_millis(2500)));
    assert_eq!(settings.retries, Some(4));
    let overrides = Options {
        retries: Some(0),
        base_directory: Some("/cli".into()),
        ..Options::default()
    };
    let merged = settings.overlay(&overrides);
    assert_eq!(merged.retries, Some(0));
    assert_eq!(merged.timeout, settings.timeout);
    assert_eq!(merged.filename, settings.filename);
    assert_eq!(merged.base_directory, Some("/cli".into()));
}

#[test]
fn category_overrides_defaults_and_global_overrides_category() {
    let config = parse(r#"{"timeout":9,"extractor":{"retries":2,"timeout":3,"filename":"{filename}","instagram":{"retries":0,"timeout":5},"twitter":{"filename":"twitter.{extension}"},"facebook":{"retries":7},"direct":{"retries":1}}}"#).unwrap();
    for (category, retries) in [
        ("instagram", 0),
        ("twitter", 2),
        ("facebook", 7),
        ("direct", 1),
    ] {
        let settings = config.resolve(category).unwrap();
        assert_eq!(settings.retries, Some(retries));
        assert_eq!(settings.timeout, Some(Duration::from_secs(9)));
    }
    assert_eq!(
        config.resolve("twitter").unwrap().filename.as_deref(),
        Some("twitter.{extension}")
    );
    assert!(config.resolve("not-supported-secret").is_err());
}

#[test]
fn literal_paths_resolve_against_configuration_directory() {
    let absolute = directory().join("absolute/history.sqlite");
    let settings = parse(
        &serde_json::json!({"extractor": {
            "base-directory": "../downloads", "cookies": "cookies.txt", "archive": absolute
        }})
        .to_string(),
    )
    .unwrap()
    .resolve("instagram")
    .unwrap();
    assert_eq!(
        settings.base_directory,
        Some(directory().join("../downloads"))
    );
    assert_eq!(settings.cookies, Some(directory().join("cookies.txt")));
    assert_eq!(settings.archive, Some(absolute));
    assert!(Config::parse(b"{}", Path::new("relative")).is_err());
}

#[test]
fn browser_cookie_sources_replace_files_across_scopes_and_anchor_profiles() {
    let config = parse(r#"{"extractor":{"cookies":"default.txt","instagram":{"cookies":["firefox","profiles/instagram"]},"twitter":{"cookies":["firefox","profiles/x",null,"none"]}}}"#).unwrap();
    for (category, profile) in [("instagram", "instagram"), ("twitter", "x")] {
        let selected = config.resolve(category).unwrap();
        assert!(selected.cookies.is_none());
        assert_eq!(
            selected.browser_cookies,
            Some(directory().join("profiles").join(profile))
        );
        let cli = selected.overlay(&Options {
            cookies: Some("cli.txt".into()),
            ..Default::default()
        });
        assert_eq!(cli.cookies, Some("cli.txt".into()));
        assert!(cli.browser_cookies.is_none());
    }
    assert!(
        config
            .resolve("facebook")
            .unwrap()
            .browser_cookies
            .is_none()
    );
    assert_eq!(
        config.resolve("facebook").unwrap().cookies,
        Some(directory().join("default.txt"))
    );
    let config = parse(
        r#"{"extractor":{"cookies":["firefox","default-profile"],"twitter":{"cookies":"x.txt"}}}"#,
    )
    .unwrap();
    let selected = config.resolve("twitter").unwrap();
    assert!(selected.browser_cookies.is_none());
    assert_eq!(selected.cookies, Some(directory().join("x.txt")));
    let global = parse(r#"{"cookies":["firefox","global-profile"],"extractor":{"cookies":"default.txt","twitter":{"cookies":"x.txt"}}}"#).unwrap();
    let selected = global.resolve("twitter").unwrap();
    assert!(selected.cookies.is_none());
    assert_eq!(
        selected.browser_cookies,
        Some(directory().join("global-profile"))
    );
}

#[test]
fn browser_cookie_arrays_validate_every_scope_without_private_diagnostics() {
    use serde_json::json;
    for source in [
        json!([]),
        json!(["firefox"]),
        json!(["chrome", "private-profile"]),
        json!(["firefox", null]),
        json!(["firefox", ""]),
        json!(["firefox", "~private-profile"]),
        json!(["firefox", "$private-profile"]),
        json!(["firefox", "private-profile::container"]),
        json!(["firefox", "private-profile", "private-keyring"]),
        json!(["firefox", "private-profile", null, "private-container"]),
        json!(["firefox", "private-profile", null, null, ".x.com"]),
    ] {
        for document in [
            json!({"cookies":source}),
            json!({"extractor":{"cookies":source}}),
            json!({"extractor":{"twitter":{"cookies":source}}}),
        ] {
            let message = parse(&document.to_string()).unwrap_err().to_string();
            assert!(!message.contains("private-"), "{message}");
        }
    }
    for suffix in [
        vec![],
        vec![json!(null)],
        vec![json!(null), json!(null)],
        vec![json!(null), json!("none")],
    ] {
        let mut source = vec![json!("firefox"), json!("private-profile")];
        source.extend(suffix);
        let selected = parse(&json!({"cookies":source}).to_string())
            .unwrap()
            .resolve("direct")
            .unwrap();
        assert_eq!(
            selected.browser_cookies,
            Some(directory().join("private-profile"))
        );
        assert!(!format!("{selected:?}").contains("private-profile"));
    }
    assert!(parse(r#"{"cookies":"a.txt","cookies":["firefox","private-profile"]}"#).is_err());
}

#[test]
fn unsupported_schema_and_values_fail_without_echoing_private_input() {
    for json in [
        r#"{"secret-key":"secret-value"}"#,
        r#"{"extractor":{"secret-key":"secret-value"}}"#,
        r#"{"extractor":{"instagram":{"secret-key":"secret-value"}}}"#,
        r#"{"extractor":{"instagram":{"post":{"cookies":"secret-value"}}}}"#,
        r#"{"extractor":{"pixiv":{"cookies":"secret-value"}}}"#,
        r#"{"extractor":{"cookies":{"sessionid":"secret-value"}}}"#,
        r#"{"extractor":{"cookies":["secret-value"]}}"#,
        r#"{"extractor":{"filename":{"secret-value":"{filename}"}}}"#,
        r#"{"extractor":{"filename":"{secret-value}"}}"#,
        r#"{"extractor":{"filename":"secret-value/{filename}"}}"#,
        r#"{"extractor":{"postprocessors":[{"name":"exec","command":"secret-value"}]}}"#,
        r#"{"extractor":{"archive":"postgresql://secret-value"}}"#,
        r#"{"extractor":{"archive":"$secret-value"}}"#,
        r#"{"extractor":{"archive":"~secret-value"}}"#,
        r#"{"extractor":{"archive":["{secret-value}"]}}"#,
        r#"{"extractor":{"archive":"{secret-value}"}}"#,
        r#"{"timeout":"secret-value"}"#,
        r#"{"retries":"secret-value"}"#,
        r#"{"secret-key":secret-value}"#,
        r#"{"extractor":null}"#,
        r#"{"extractor":{"instagram":null}}"#,
        r#"{"extractor":{"cookies":null}}"#,
        r#"{"filename":null}"#,
        r#"{"timeout":null}"#,
        r#"{"retries":null}"#,
        r#"{"base-directory":null}"#,
    ] {
        let message = parse(json).unwrap_err().to_string();
        assert!(!message.contains("secret-key"), "{message}");
        assert!(!message.contains("secret-value"), "{message}");
    }
}

#[test]
fn bounds_types_duplicate_keys_and_malformed_json_are_rejected() {
    for json in [
        "[]",
        "null",
        "{} trailing",
        "{",
        r#"{"extractor":[]}"#,
        r#"{"retries":-1}"#,
        r#"{"retries":11}"#,
        r#"{"retries":1.5}"#,
        r#"{"retries":true}"#,
        r#"{"timeout":0}"#,
        r#"{"timeout":-1}"#,
        r#"{"timeout":1e100}"#,
        r#"{"timeout":1e-100}"#,
        r#"{"filename":""}"#,
        r#"{"cookies":""}"#,
        r#"{"cookies":"bad\u0000path"}"#,
        r#"{"retries":1,"retries":2}"#,
        r#"{"extractor":{"retries":1,"retries":2}}"#,
        r#"{"extractor":{"instagram":{"retries":1,"retries":2}}}"#,
        r#"{"extractor":{"instagram":{},"instagram":{}}}"#,
    ] {
        assert!(parse(json).is_err(), "accepted: {json}");
    }
    let mut large = vec![b' '; MAX_CONFIG_BYTES + 1];
    large[..2].copy_from_slice(b"{}");
    assert!(Config::parse(&large, &directory()).is_err());
    assert_eq!(
        parse(r#"{"retries":10,"timeout":0.001}"#)
            .unwrap()
            .resolve("direct")
            .unwrap()
            .timeout,
        Some(Duration::from_millis(1))
    );
}

#[tokio::test]
async fn file_loading_anchors_paths_and_enforces_size_limit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    tokio::fs::write(&path, br#"{"cookies":"cookies.txt"}"#)
        .await
        .unwrap();
    let expected = tokio::fs::canonicalize(dir.path())
        .await
        .unwrap()
        .join("cookies.txt");
    assert_eq!(
        Config::load(&path)
            .await
            .unwrap()
            .resolve("twitter")
            .unwrap()
            .cookies,
        Some(expected)
    );
    tokio::fs::write(&path, vec![b' '; MAX_CONFIG_BYTES + 1])
        .await
        .unwrap();
    assert!(
        Config::load(&path)
            .await
            .unwrap_err()
            .to_string()
            .contains("1 MiB")
    );
    let secret_path = dir.path().join("secret-path");
    assert!(
        !Config::load(&secret_path)
            .await
            .unwrap_err()
            .to_string()
            .contains("secret-path")
    );
}

#[test]
fn directory_templates_resolve_scopes_and_preserve_explicit_empty_overrides() {
    let config = parse(
        r#"{"extractor":{"directory":["{category}","{username}"],"instagram":{"directory":[]},"twitter":{"directory":["tweets","{post_id}"]}}}"#,
    )
    .unwrap();
    assert_eq!(
        config.resolve("direct").unwrap().directory,
        Some(vec!["{category}".into(), "{username}".into()])
    );
    assert_eq!(config.resolve("instagram").unwrap().directory, Some(vec![]));
    assert_eq!(
        config.resolve("twitter").unwrap().directory,
        Some(vec!["tweets".into(), "{post_id}".into()])
    );
    let settings = config.resolve("twitter").unwrap();
    assert_eq!(
        settings.overlay(&Options::default()).directory,
        settings.directory
    );
    assert_eq!(
        settings
            .overlay(&Options {
                directory: Some(vec![]),
                ..Options::default()
            })
            .directory,
        Some(vec![])
    );
    let global = parse(r#"{"directory":[],"extractor":{"directory":["{category}"],"instagram":{"directory":["posts"]}}}"#).unwrap();
    assert_eq!(global.resolve("instagram").unwrap().directory, Some(vec![]));
}

#[test]
fn directory_templates_reject_invalid_types_paths_and_unknown_fields_privately() {
    for value in [
        serde_json::json!(null),
        serde_json::json!("secret-value"),
        serde_json::json!({"secret-key":"secret-value"}),
        serde_json::json!([null]),
        serde_json::json!([42]),
        serde_json::json!([""]),
        serde_json::json!([".."]),
        serde_json::json!(["/secret-value"]),
        serde_json::json!(["secret-value/child"]),
        serde_json::json!(["{secret-value}"]),
        serde_json::json!(["secret-value\0"]),
        serde_json::json!(vec!["secret-value"; 33]),
        serde_json::json!(["secret-value".repeat(8193)]),
    ] {
        for json in [
            serde_json::json!({"directory":value}),
            serde_json::json!({"extractor":{"directory":value}}),
            serde_json::json!({"extractor":{"instagram":{"directory":value}}}),
        ] {
            let message = parse(&json.to_string()).unwrap_err().to_string();
            assert!(!message.contains("secret-value"), "{message}");
            assert!(!message.contains("secret-key"), "{message}");
        }
    }
    for json in [
        r#"{"directory":[],"directory":["posts"]}"#,
        r#"{"extractor":{"directory":[],"directory":["posts"]}}"#,
        r#"{"extractor":{"instagram":{"directory":[],"directory":["posts"]}}}"#,
    ] {
        assert!(parse(json).is_err());
    }
}

#[test]
fn media_ranges_follow_scopes_and_explicit_overrides() {
    let config =
        parse(r#"{"extractor":{"file-range":"1-3","twitter":{"file-range":["5","8-10"]}}}"#)
            .unwrap();
    assert_eq!(
        config.resolve("instagram").unwrap().file_range.as_deref(),
        Some("1-3")
    );
    assert_eq!(
        config.resolve("twitter").unwrap().file_range.as_deref(),
        Some("5,8-10")
    );
    let global = parse(r#"{"file-range":"4:","extractor":{"file-range":"1"}}"#).unwrap();
    let settings = global.resolve("direct").unwrap();
    assert_eq!(settings.file_range.as_deref(), Some("4:"));
    assert_eq!(
        settings
            .overlay(&Options {
                file_range: Some("1".into()),
                ..Default::default()
            })
            .file_range
            .as_deref(),
        Some("1")
    );
    for value in [serde_json::json!([]), serde_json::json!("")] {
        let config = parse(
            &serde_json::json!({"file-range":value,"extractor":{"file-range":"0"}}).to_string(),
        )
        .unwrap();
        let spec = config.resolve("direct").unwrap().file_range.unwrap();
        assert!(
            gallery_dl_rs::selection::RangeSelection::parse(&spec)
                .unwrap()
                .contains(1)
        );
    }
}

#[test]
fn media_range_validation_never_quotes_private_specs() {
    for value in [
        serde_json::json!(null),
        serde_json::json!(true),
        serde_json::json!(42),
        serde_json::json!({"secret":"secret"}),
        serde_json::json!([42]),
        serde_json::json!("secret"),
        serde_json::json!("1::0"),
        serde_json::json!("1".repeat(4097)),
    ] {
        for doc in [
            serde_json::json!({"file-range":value}),
            serde_json::json!({"extractor":{"file-range":value}}),
            serde_json::json!({"extractor":{"twitter":{"file-range":value}}}),
        ] {
            let message = parse(&doc.to_string()).unwrap_err().to_string();
            assert!(!message.contains("secret"));
        }
    }
    assert!(parse(r#"{"file-range":"1","file-range":"2"}"#).is_err());
}

#[test]
fn size_limits_support_bytes_units_and_explicit_zero_overlays() {
    let config = parse(r#"{"filesize-min":"2.5k","extractor":{"filesize-max":"1M","direct":{"filesize-max":9000}}}"#).unwrap();
    let direct = config.resolve("direct").unwrap();
    assert_eq!(direct.filesize_min, Some(2560));
    assert_eq!(direct.filesize_max, Some(9000));
    assert_eq!(
        config.resolve("instagram").unwrap().filesize_max,
        Some(1048576)
    );
    assert_eq!(
        direct
            .overlay(&Options {
                filesize_max: Some(0),
                ..Default::default()
            })
            .filesize_max,
        Some(0)
    );
    assert_eq!(
        parse(r#"{"filesize-min":1.5}"#)
            .unwrap()
            .resolve("direct")
            .unwrap()
            .filesize_min,
        Some(2)
    );
}

#[test]
fn invalid_size_configuration_fails_without_echoing_values() {
    for value in [
        serde_json::json!(null),
        serde_json::json!(true),
        serde_json::json!([]),
        serde_json::json!(-1),
        serde_json::json!("private-limit"),
        serde_json::json!("1e9999"),
        serde_json::json!("18446744073709551616"),
    ] {
        for field in ["filesize-min", "filesize-max"] {
            let message = parse(&serde_json::json!({field:value}).to_string())
                .unwrap_err()
                .to_string();
            assert!(!message.contains("private-limit"));
        }
    }
    assert!(parse(r#"{"filesize-min":1,"filesize-min":2}"#).is_err());
}

#[test]
fn date_limits_resolve_scopes_and_preserve_epoch_zero() {
    let config =
        parse(r#"{"extractor":{"date-before":"2000-01-01","twitter":{"date-after":0}}}"#).unwrap();
    let twitter = config.resolve("twitter").unwrap();
    assert_eq!(twitter.date_before, Some(946684800));
    assert_eq!(twitter.date_after, Some(0));
    assert_eq!(config.resolve("instagram").unwrap().date_after, None);
    assert_eq!(
        twitter
            .overlay(&Options {
                date_after: Some(-1),
                ..Default::default()
            })
            .date_after,
        Some(-1)
    );
    let global = parse(r#"{"date-before":123,"extractor":{"date-before":456}}"#).unwrap();
    assert_eq!(global.resolve("direct").unwrap().date_before, Some(123));
}

#[test]
fn date_limits_reject_invalid_private_values_and_duplicate_keys() {
    for field in ["date-before", "date-after"] {
        for value in [
            serde_json::json!(null),
            serde_json::json!(false),
            serde_json::json!(1.5),
            serde_json::json!([]),
            serde_json::json!("private-date"),
            serde_json::json!("2025-02-29"),
            serde_json::json!("2026-01-01T24:00:00"),
            serde_json::json!("18446744073709551615"),
        ] {
            for doc in [
                serde_json::json!({(field):value}),
                serde_json::json!({"extractor":{(field):value}}),
                serde_json::json!({"extractor":{"instagram":{(field):value}}}),
            ] {
                let message = parse(&doc.to_string()).unwrap_err().to_string();
                assert!(!message.contains("private-date"));
            }
        }
    }
    assert!(parse(r#"{"date-before":1,"date-before":2}"#).is_err());
}

#[test]
fn invalid_network_values_fail_privately_in_every_scope() {
    for (field, value) in [
        ("proxy", serde_json::json!(null)),
        ("proxy", serde_json::json!(["private"])),
        (
            "proxy",
            serde_json::json!("http://user:private@proxy.test/path"),
        ),
        (
            "proxy",
            serde_json::json!("socks5://user:private@proxy.test"),
        ),
        ("user-agent", serde_json::json!(false)),
        ("user-agent", serde_json::json!("")),
        (
            "user-agent",
            serde_json::json!("private\r\nInjected: value"),
        ),
    ] {
        for doc in [
            serde_json::json!({(field):value}),
            serde_json::json!({"extractor":{(field):value}}),
            serde_json::json!({"extractor":{"instagram":{(field):value}}}),
        ] {
            assert!(
                !parse(&doc.to_string())
                    .unwrap_err()
                    .to_string()
                    .contains("private")
            );
        }
    }
    assert!(parse(r#"{"proxy":"","proxy":"http://proxy.test"}"#).is_err());
    let options =
        parse(r#"{"proxy":"http://user:private@proxy.test","user-agent":"private-agent"}"#)
            .unwrap()
            .resolve("direct")
            .unwrap();
    assert!(!format!("{options:?}").contains("private"));
}

#[test]
fn twitter_expansion_settings_follow_scopes_and_preserve_false_overlays() {
    use gallery_dl_rs::twitter::RetweetMode;
    let config = parse(
        r#"{"extractor":{"retweets":true,"quoted":true,"twitter":{"retweets":"original","quoted":false},"instagram":{"retweets":false}}}"#,
    )
    .unwrap();
    let twitter = config.resolve("twitter").unwrap();
    assert_eq!(twitter.retweets, Some(RetweetMode::Original));
    assert_eq!(twitter.quoted, Some(false));
    assert_eq!(
        config.resolve("facebook").unwrap().retweets,
        Some(RetweetMode::Repost)
    );
    assert_eq!(config.resolve("facebook").unwrap().quoted, Some(true));
    assert_eq!(
        config.resolve("instagram").unwrap().retweets,
        Some(RetweetMode::Skip)
    );
    assert_eq!(twitter.overlay(&Options::default()), twitter);
    let disabled = twitter.overlay(&Options {
        retweets: Some(RetweetMode::Skip),
        quoted: Some(false),
        ..Default::default()
    });
    assert_eq!(disabled.retweets, Some(RetweetMode::Skip));
    assert_eq!(disabled.quoted, Some(false));
    let global = parse(
        r#"{"retweets":false,"quoted":false,"extractor":{"retweets":true,"quoted":true,"twitter":{"retweets":"original","quoted":true}}}"#,
    )
    .unwrap();
    for category in ["twitter", "instagram", "facebook", "direct"] {
        let settings = global.resolve(category).unwrap();
        assert_eq!(settings.retweets, Some(RetweetMode::Skip));
        assert_eq!(settings.quoted, Some(false));
    }
    let absent = parse("{}").unwrap().resolve("twitter").unwrap();
    assert_eq!(absent.retweets, None);
    assert_eq!(absent.quoted, None);
}

#[test]
fn twitter_expansion_configuration_rejects_types_privately_in_all_scopes() {
    use serde_json::json;
    for (field, value) in [
        ("retweets", json!(null)),
        ("retweets", json!(0)),
        ("retweets", json!(1)),
        ("retweets", json!([])),
        ("retweets", json!({"private-key":"private-value"})),
        ("retweets", json!("private-value")),
        ("retweets", json!("true")),
        ("retweets", json!("Original")),
        ("quoted", json!(null)),
        ("quoted", json!(0)),
        ("quoted", json!([])),
        ("quoted", json!({"private-key":"private-value"})),
        ("quoted", json!("private-value")),
        ("quoted", json!("false")),
    ] {
        for document in [
            json!({(field):value}),
            json!({"extractor":{(field):value}}),
            json!({"extractor":{"twitter":{(field):value}}}),
            json!({"extractor":{"instagram":{(field):value}}}),
            json!({"extractor":{"facebook":{(field):value}}}),
            json!({"extractor":{"direct":{(field):value}}}),
        ] {
            let message = parse(&document.to_string()).unwrap_err().to_string();
            assert!(!message.contains("private-key"), "{message}");
            assert!(!message.contains("private-value"), "{message}");
        }
    }
}

#[test]
fn twitter_expansion_configuration_rejects_duplicate_keys() {
    for field in ["retweets", "quoted"] {
        let duplicate = format!(r#""{field}":false,"{field}":true"#);
        for document in [
            format!("{{{duplicate}}}"),
            format!("{{\"extractor\":{{{duplicate}}}}}"),
            format!("{{\"extractor\":{{\"twitter\":{{{duplicate}}}}}}}"),
            format!("{{\"extractor\":{{\"instagram\":{{{duplicate}}}}}}}"),
            format!("{{\"extractor\":{{\"facebook\":{{{duplicate}}}}}}}"),
            format!("{{\"extractor\":{{\"direct\":{{{duplicate}}}}}}}"),
        ] {
            assert!(parse(&document).is_err(), "accepted {document}");
        }
    }
}
