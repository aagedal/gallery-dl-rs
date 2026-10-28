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
    // Like upstream, unknown keys are ignored (or passed to registry sites)
    // and `~`/`$VAR` in paths are expanded; see `upstream_configs_load`.
    for json in [
        r#"{"extractor":{"cookies":{"sessionid":"secret-value"}}}"#,
        r#"{"extractor":{"cookies":["secret-value"]}}"#,
        r#"{"extractor":{"filename":{"secret-value(":"{filename}"}}}"#,
        r#"{"extractor":{"filename":{"1":["secret-value"]}}}"#,
        r#"{"extractor":{"filename":"{secret-value[}"}}"#,
        r#"{"extractor":{"filename":"secret-value/{filename}"}}"#,
        r#"{"extractor":{"postprocessors":["ugoira", {"secret-value": true}, 1]}}"#,
        r#"{"extractor":{"archive":"postgresql://secret-value"}}"#,
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
fn source_templates_accept_dynamic_api_fields_before_extraction() {
    // Source formatter resolves these API dictionary keys at rendering time,
    // including keys that are not Python identifiers.
    let filename = "{private-field!D:%Y}.{extension}";
    let directory = vec!["{category}".to_owned(), "{private-field!d:%Y}".to_owned()];
    let config = parse(
        &serde_json::json!({
            "extractor": {"filename": filename, "directory": directory}
        })
        .to_string(),
    )
    .unwrap();
    let settings = config.resolve("danbooru").unwrap();
    assert_eq!(settings.filename.as_deref(), Some(filename));
    assert_eq!(settings.directory.as_ref(), Some(&directory));
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
fn directory_templates_reject_invalid_types_paths_and_malformed_fields_privately() {
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
        serde_json::json!(["{secret-value[}"]),
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
fn date_limits_resolve_scopes_and_preserve_string_epoch_zero() {
    let config =
        parse(r#"{"extractor":{"date-before":"2000-01-01","twitter":{"date-after":"0"}}}"#)
            .unwrap();
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
fn date_limits_disable_invalid_or_falsy_values_and_convert_source_numeric_bounds() {
    for field in ["date-before", "date-after"] {
        for (value, expected) in [
            (serde_json::json!(null), None),
            (serde_json::json!(false), None),
            (serde_json::json!(0), None),
            (serde_json::json!(true), Some(1)),
            (serde_json::json!(1.5), Some(1)),
            (serde_json::json!(-1.5), Some(-1)),
            (serde_json::json!(0.1), Some(0)),
            (serde_json::json!([]), None),
            (serde_json::json!({"private":"PRIVATE_DATE"}), None),
            (serde_json::json!("private-date"), None),
            (serde_json::json!("2025-02-29"), None),
            (serde_json::json!("2026-01-01T24:00:00"), Some(1767312000)),
            (serde_json::json!("18446744073709551615"), None),
        ] {
            for doc in [
                serde_json::json!({(field):value}),
                serde_json::json!({"extractor":{(field):value}}),
                serde_json::json!({"extractor":{"instagram":{(field):value}}}),
            ] {
                let config = parse(&doc.to_string()).unwrap();
                let settings = config.resolve("instagram").unwrap();
                assert_eq!(
                    if field == "date-before" {
                        settings.date_before
                    } else {
                        settings.date_after
                    },
                    expected
                );
            }
        }
    }
    let settings = parse(r#"{"date-after":null,"extractor":{"date-after":"2000-01-01"}}"#)
        .unwrap()
        .resolve("twitter")
        .unwrap();
    assert_eq!(settings.date_after, None);
    assert_eq!(
        settings
            .overlay(&Options {
                date_after: Some(0),
                ..Default::default()
            })
            .date_after,
        Some(0)
    );
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

#[test]
fn twitter_profile_settings_follow_scopes_and_preserve_false_overlays() {
    use gallery_dl_rs::twitter::ReplyMode;
    let config = parse(
        r#"{"extractor":{"pinned":true,"replies":true,"twitter":{"pinned":false,"replies":"self"},"instagram":{"replies":false}}}"#,
    )
    .unwrap();
    let twitter = config.resolve("twitter").unwrap();
    assert_eq!(twitter.pinned, Some(false));
    assert_eq!(twitter.replies, Some(ReplyMode::SelfOnly));
    assert_eq!(config.resolve("facebook").unwrap().pinned, Some(true));
    assert_eq!(
        config.resolve("facebook").unwrap().replies,
        Some(ReplyMode::All)
    );
    assert_eq!(
        config.resolve("instagram").unwrap().replies,
        Some(ReplyMode::Skip)
    );
    assert_eq!(twitter.overlay(&Options::default()), twitter);
    let disabled = twitter.overlay(&Options {
        pinned: Some(false),
        replies: Some(ReplyMode::Skip),
        ..Default::default()
    });
    assert_eq!(disabled.pinned, Some(false));
    assert_eq!(disabled.replies, Some(ReplyMode::Skip));
    let enabled = disabled.overlay(&Options {
        pinned: Some(true),
        replies: Some(ReplyMode::All),
        ..Default::default()
    });
    assert_eq!(enabled.pinned, Some(true));
    assert_eq!(enabled.replies, Some(ReplyMode::All));
    let global = parse(
        r#"{"pinned":false,"replies":false,"extractor":{"pinned":true,"replies":true,"twitter":{"pinned":true,"replies":"self"}}}"#,
    )
    .unwrap();
    for category in ["twitter", "instagram", "facebook", "direct"] {
        let settings = global.resolve(category).unwrap();
        assert_eq!(settings.pinned, Some(false));
        assert_eq!(settings.replies, Some(ReplyMode::Skip));
    }
    let absent = parse("{}").unwrap().resolve("twitter").unwrap();
    assert_eq!(absent.pinned, None);
    assert_eq!(absent.replies, None);
}

#[test]
fn twitter_profile_configuration_rejects_types_privately_in_all_scopes() {
    use serde_json::json;
    for (field, value) in [
        ("pinned", json!(null)),
        ("pinned", json!(0)),
        ("pinned", json!(1)),
        ("pinned", json!([])),
        ("pinned", json!({"private-key":"private-value"})),
        ("pinned", json!("private-value")),
        ("pinned", json!("false")),
        ("replies", json!(null)),
        ("replies", json!(0)),
        ("replies", json!(1)),
        ("replies", json!([])),
        ("replies", json!({"private-key":"private-value"})),
        ("replies", json!("private-value")),
        ("replies", json!("true")),
        ("replies", json!("Self")),
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
fn twitter_profile_configuration_rejects_duplicate_keys() {
    for field in ["pinned", "replies"] {
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

#[test]
fn twitter_search_configuration_preserves_scopes_aliases_and_explicit_zero() {
    use gallery_dl_rs::twitter::{SearchPagination, SearchProduct, TimelineStrategy};
    let config = parse(r#"{"extractor":{"strategy":"media","search-pagination":"cursor","search-results":"top","search-limit":50,"search-stop":8,"twitter":{"strategy":"with_replies","search-pagination":"dt","search-results":"LiVe","search-limit":1,"search-stop":0}}}"#).unwrap();
    let twitter = config.resolve("twitter").unwrap();
    assert_eq!(twitter.strategy, Some(TimelineStrategy::Replies));
    assert_eq!(twitter.search_pagination, Some(SearchPagination::Date));
    assert_eq!(twitter.search_results, Some(SearchProduct::Latest));
    assert_eq!(twitter.search_limit, Some(1));
    assert_eq!(twitter.search_stop, Some(0));
    let generic = config.resolve("instagram").unwrap();
    assert_eq!(generic.strategy, Some(TimelineStrategy::Media));
    assert_eq!(generic.search_pagination, Some(SearchPagination::Cursor));
    assert_eq!(generic.search_results, Some(SearchProduct::Top));
    assert_eq!(generic.search_limit, Some(50));
    assert_eq!(generic.search_stop, Some(8));
    let global = parse(r#"{"strategy":"auto","search-pagination":"maxid","search-results":"user","search-limit":100,"search-stop":"auto","extractor":{"twitter":{"strategy":"tweets","search-pagination":"cursor","search-results":"media","search-limit":1,"search-stop":0}}}"#).unwrap();
    for category in ["twitter", "instagram", "facebook", "direct"] {
        let settings = global.resolve(category).unwrap();
        assert_eq!(settings.strategy, Some(TimelineStrategy::Auto));
        assert_eq!(settings.search_pagination, Some(SearchPagination::MaxId));
        assert_eq!(settings.search_results, Some(SearchProduct::People));
        assert_eq!(settings.search_limit, Some(100));
        assert_eq!(settings.search_stop, Some(3));
    }
    assert_eq!(twitter.overlay(&Options::default()), twitter);
    let overrides = Options {
        strategy: Some(TimelineStrategy::Tweets),
        search_pagination: Some(SearchPagination::MaxId),
        search_results: Some(SearchProduct::Lists),
        search_limit: Some(20),
        search_stop: Some(0),
        ..Default::default()
    };
    let overlaid = twitter.overlay(&overrides);
    assert_eq!(overlaid.strategy, overrides.strategy);
    assert_eq!(overlaid.search_pagination, overrides.search_pagination);
    assert_eq!(overlaid.search_results, overrides.search_results);
    assert_eq!(overlaid.search_limit, overrides.search_limit);
    assert_eq!(overlaid.search_stop, overrides.search_stop);
    let absent = parse("{}").unwrap().resolve("twitter").unwrap();
    assert_eq!(absent.strategy, None);
    assert_eq!(absent.search_pagination, None);
    assert_eq!(absent.search_results, None);
    assert_eq!(absent.search_limit, None);
    assert_eq!(absent.search_stop, None);
    for (field, aliases) in [
        ("strategy", vec!["auto", "media", "tweets", "with_replies"]),
        (
            "search-pagination",
            vec![
                "max_id", "maxid", "id", "cursor", "until", "date", "datetime", "dt",
            ],
        ),
        (
            "search-results",
            vec![
                "Latest", "live", "Top", "Media", "People", "user", "Lists", "list",
            ],
        ),
    ] {
        for alias in aliases {
            assert!(parse(&serde_json::json!({(field):alias}).to_string()).is_ok());
        }
    }
    assert_eq!(
        parse(r#"{"search-stop":128}"#)
            .unwrap()
            .resolve("twitter")
            .unwrap()
            .search_stop,
        Some(128)
    );
}

#[test]
fn twitter_search_configuration_rejects_invalid_and_duplicate_values_privately() {
    use serde_json::json;
    for field in [
        "strategy",
        "search-pagination",
        "search-results",
        "limit",
        "search-limit",
        "search-stop",
    ] {
        let mut invalid = vec![
            json!(null),
            json!(true),
            json!([]),
            json!({"private-key":"private-value"}),
            json!("private-value"),
        ];
        if matches!(field, "strategy" | "search-pagination" | "search-results") {
            invalid.extend([json!(0), json!(1)]);
        } else {
            invalid.extend([json!(-1), json!(1.5), json!("20"), json!(129), json!(1000)]);
            if matches!(field, "limit" | "search-limit") {
                invalid.extend([json!(0), json!(101), json!("auto")]);
            }
        }
        for value in invalid {
            for document in [
                json!({(field):value}),
                json!({"extractor":{(field):value}}),
                json!({"extractor":{"twitter":{(field):value}}}),
                json!({"extractor":{"instagram":{(field):value}}}),
                json!({"extractor":{"facebook":{(field):value}}}),
                json!({"extractor":{"direct":{(field):value}}}),
            ] {
                let message = parse(&document.to_string()).unwrap_err().to_string();
                assert!(
                    !message.contains("private-key") && !message.contains("private-value"),
                    "{message}"
                );
            }
        }
        let duplicate = format!(r#""{field}":null,"{field}":null"#);
        for document in [
            format!("{{{duplicate}}}"),
            format!("{{\"extractor\":{{{duplicate}}}}}"),
            format!("{{\"extractor\":{{\"twitter\":{{{duplicate}}}}}}}"),
            format!("{{\"extractor\":{{\"instagram\":{{{duplicate}}}}}}}"),
            format!("{{\"extractor\":{{\"facebook\":{{{duplicate}}}}}}}"),
            format!("{{\"extractor\":{{\"direct\":{{{duplicate}}}}}}}"),
        ] {
            assert!(parse(&document).is_err());
        }
    }
    for document in [
        r#"{"strategy":"Auto"}"#,
        r#"{"search-pagination":"MAX_ID"}"#,
    ] {
        assert!(parse(document).is_err());
    }
}

#[test]
fn twitter_collection_limit_is_sparse_scoped_and_independent_from_search_limit() {
    let scoped =
        parse(r#"{"extractor":{"limit":1,"search-limit":20,"twitter":{"limit":100}}}"#).unwrap();
    let twitter = scoped.resolve("twitter").unwrap();
    assert_eq!(twitter.limit, Some(100));
    assert_eq!(twitter.search_limit, Some(20));
    assert_eq!(scoped.resolve("instagram").unwrap().limit, Some(1));
    let global = parse(r#"{"limit":50,"extractor":{"limit":1,"twitter":{"limit":100}}}"#).unwrap();
    assert_eq!(global.resolve("twitter").unwrap().limit, Some(50));
    assert_eq!(twitter.overlay(&Options::default()), twitter);
    assert_eq!(
        twitter
            .overlay(&Options {
                limit: Some(1),
                ..Default::default()
            })
            .limit,
        Some(1)
    );
    assert_eq!(parse("{}").unwrap().resolve("twitter").unwrap().limit, None);
}

#[test]
fn twitter_collection_limit_rejects_duplicate_valid_values_in_every_scope() {
    for document in [
        r#"{"limit":1,"limit":100}"#,
        r#"{"extractor":{"limit":1,"limit":100}}"#,
        r#"{"extractor":{"twitter":{"limit":1,"limit":100}}}"#,
        r#"{"extractor":{"instagram":{"limit":1,"limit":100}}}"#,
        r#"{"extractor":{"facebook":{"limit":1,"limit":100}}}"#,
        r#"{"extractor":{"direct":{"limit":1,"limit":100}}}"#,
    ] {
        assert!(parse(document).is_err());
    }
}

#[test]
fn twitter_status_options_are_sparse_and_resolve_all_scopes() {
    use gallery_dl_rs::twitter::{ConversationMode, TweetEndpoint};
    let config = parse(r#"{"extractor":{"tweet-endpoint":"restid","conversations":true,"twitter":{"tweet-endpoint":"detail","conversations":"accessible"}}}"#).unwrap();
    let twitter = config.resolve("twitter").unwrap();
    assert_eq!(twitter.tweet_endpoint, Some(TweetEndpoint::Detail));
    assert_eq!(twitter.conversations, Some(ConversationMode::Accessible));
    let facebook = config.resolve("facebook").unwrap();
    assert_eq!(facebook.tweet_endpoint, Some(TweetEndpoint::RestId));
    assert_eq!(facebook.conversations, Some(ConversationMode::All));
    let global = parse(r#"{"tweet-endpoint":"auto","conversations":false,"extractor":{"tweet-endpoint":"restid","conversations":true,"twitter":{"tweet-endpoint":"detail","conversations":"accessible"}}}"#).unwrap();
    for category in ["twitter", "instagram", "facebook", "direct"] {
        let resolved = global.resolve(category).unwrap();
        assert_eq!(resolved.tweet_endpoint, Some(TweetEndpoint::Auto));
        assert_eq!(resolved.conversations, Some(ConversationMode::Off));
    }
    let overrides = Options {
        tweet_endpoint: Some(TweetEndpoint::RestId),
        conversations: Some(ConversationMode::Off),
        ..Options::default()
    };
    let overlaid = twitter.overlay(&overrides);
    assert_eq!(overlaid.tweet_endpoint, overrides.tweet_endpoint);
    assert_eq!(overlaid.conversations, overrides.conversations);
    let absent = parse("{}").unwrap().resolve("twitter").unwrap();
    assert_eq!(absent.tweet_endpoint, None);
    assert_eq!(absent.conversations, None);
}

#[test]
fn twitter_status_configuration_rejects_malformed_and_duplicate_values_privately() {
    use serde_json::json;
    for field in ["tweet-endpoint", "conversations"] {
        let mut invalid = vec![
            json!(null),
            json!(0),
            json!(1),
            json!([]),
            json!({"private-key":"private-value"}),
            json!("private-value"),
            json!("Auto"),
        ];
        if field == "tweet-endpoint" {
            invalid.extend([json!(true), json!(false), json!("rest_id")]);
        } else {
            invalid.extend([json!("true"), json!("Accessible")]);
        }
        for value in invalid {
            for document in [
                json!({(field):value}),
                json!({"extractor":{(field):value}}),
                json!({"extractor":{"twitter":{(field):value}}}),
                json!({"extractor":{"instagram":{(field):value}}}),
                json!({"extractor":{"facebook":{(field):value}}}),
                json!({"extractor":{"direct":{(field):value}}}),
            ] {
                let message = parse(&document.to_string()).unwrap_err().to_string();
                assert!(
                    !message.contains("private-key") && !message.contains("private-value"),
                    "{message}"
                );
            }
        }
        let value = if field == "tweet-endpoint" {
            "\"auto\""
        } else {
            "false"
        };
        let duplicate = format!(r#""{field}":{value},"{field}":{value}"#);
        for document in [
            format!("{{{duplicate}}}"),
            format!("{{\"extractor\":{{{duplicate}}}}}"),
            format!("{{\"extractor\":{{\"twitter\":{{{duplicate}}}}}}}"),
            format!("{{\"extractor\":{{\"instagram\":{{{duplicate}}}}}}}"),
            format!("{{\"extractor\":{{\"facebook\":{{{duplicate}}}}}}}"),
            format!("{{\"extractor\":{{\"direct\":{{{duplicate}}}}}}}"),
        ] {
            assert!(parse(&document).is_err());
        }
    }
}

#[test]
fn twitter_thread_settings_follow_scopes_and_preserve_false_overlays() {
    let config = parse(
        r#"{"extractor":{"expand":true,"unique":true,"twitter":{"expand":false,"unique":false}}}"#,
    )
    .unwrap();
    let twitter = config.resolve("twitter").unwrap();
    assert_eq!((twitter.expand, twitter.unique), (Some(false), Some(false)));
    let inherited = config.resolve("instagram").unwrap();
    assert_eq!(
        (inherited.expand, inherited.unique),
        (Some(true), Some(true))
    );
    assert_eq!(twitter.overlay(&Options::default()), twitter);
    let disabled = inherited.overlay(&Options {
        expand: Some(false),
        unique: Some(false),
        ..Default::default()
    });
    assert_eq!(
        (disabled.expand, disabled.unique),
        (Some(false), Some(false))
    );
    let root = parse(r#"{"expand":false,"unique":false,"extractor":{"expand":true,"unique":true,"twitter":{"expand":true,"unique":true}}}"#).unwrap();
    for category in ["twitter", "instagram", "facebook", "direct"] {
        let options = root.resolve(category).unwrap();
        assert_eq!((options.expand, options.unique), (Some(false), Some(false)));
    }
    let absent = parse("{}").unwrap().resolve("twitter").unwrap();
    assert_eq!((absent.expand, absent.unique), (None, None));
}

#[test]
fn twitter_thread_settings_reject_types_and_duplicates_privately_in_every_scope() {
    use serde_json::json;
    for field in ["expand", "unique"] {
        for value in [
            json!(null),
            json!(0),
            json!(1),
            json!("false"),
            json!([]),
            json!({"private-key":"private-value"}),
            json!("private-value"),
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
                assert!(!message.contains("private-key"));
                assert!(!message.contains("private-value"));
            }
        }
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

#[test]
fn twitter_media_settings_resolve_scopes_and_preserve_false_overlays() {
    use gallery_dl_rs::twitter::VideoMode;
    let config = parse(r#"{"extractor":{"videos":true,"previews":true,"twitter":{"videos":"ytdl","previews":false},"instagram":{"videos":false}}}"#).unwrap();
    let twitter = config.resolve("twitter").unwrap();
    assert_eq!(
        (twitter.videos, twitter.previews),
        (Some(VideoMode::Ytdl), Some(false))
    );
    let inherited = config.resolve("facebook").unwrap();
    assert_eq!(
        (inherited.videos, inherited.previews),
        (Some(VideoMode::Files), Some(true))
    );
    assert_eq!(
        config.resolve("instagram").unwrap().videos,
        Some(VideoMode::Skip)
    );
    assert_eq!(twitter.overlay(&Options::default()), twitter);
    let disabled = twitter.overlay(&Options {
        videos: Some(VideoMode::Skip),
        previews: Some(false),
        ..Default::default()
    });
    assert_eq!(
        (disabled.videos, disabled.previews),
        (Some(VideoMode::Skip), Some(false))
    );
    let root = parse(r#"{"videos":false,"previews":false,"extractor":{"videos":true,"previews":true,"twitter":{"videos":"ytdl","previews":true}}}"#).unwrap();
    for category in ["twitter", "instagram", "facebook", "direct"] {
        let options = root.resolve(category).unwrap();
        assert_eq!(
            (options.videos, options.previews),
            (Some(VideoMode::Skip), Some(false))
        );
    }
    let absent = parse("{}").unwrap().resolve("twitter").unwrap();
    assert_eq!((absent.videos, absent.previews), (None, None));
}

#[test]
fn twitter_media_settings_reject_malformed_and_duplicate_values_privately() {
    use serde_json::json;
    for field in ["videos", "previews"] {
        let mut values = vec![
            json!(null),
            json!(0),
            json!(1),
            json!("false"),
            json!("true"),
            json!([]),
            json!({"private-key":"private-value"}),
            json!("private-value"),
        ];
        values.push(if field == "videos" {
            json!("Ytdl")
        } else {
            json!("ytdl")
        });
        for value in values {
            for document in [
                json!({(field):value}),
                json!({"extractor":{(field):value}}),
                json!({"extractor":{"twitter":{(field):value}}}),
                json!({"extractor":{"instagram":{(field):value}}}),
                json!({"extractor":{"facebook":{(field):value}}}),
                json!({"extractor":{"direct":{(field):value}}}),
            ] {
                let message = parse(&document.to_string()).unwrap_err().to_string();
                assert!(
                    !message.contains("private-key") && !message.contains("private-value"),
                    "{message}"
                );
            }
        }
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

#[test]
fn twitter_card_settings_resolve_scopes_and_clear_inherited_blacklists() {
    use gallery_dl_rs::twitter::CardMode;
    let config = parse(r#"{"extractor":{"cards":true,"cards-blacklist":["summary","player"],"twitter":{"cards":"ytdl","cards-blacklist":[]},"instagram":{"cards":false}}}"#).unwrap();
    let twitter = config.resolve("twitter").unwrap();
    assert_eq!(twitter.cards, Some(CardMode::Ytdl));
    assert_eq!(twitter.cards_blacklist, Some(vec![]));
    let inherited = config.resolve("facebook").unwrap();
    assert_eq!(inherited.cards, Some(CardMode::Files));
    assert_eq!(
        inherited.cards_blacklist,
        Some(vec!["summary".into(), "player".into()])
    );
    assert_eq!(
        config.resolve("instagram").unwrap().cards,
        Some(CardMode::Skip)
    );
    assert_eq!(twitter.overlay(&Options::default()), twitter);
    let disabled = inherited.overlay(&Options {
        cards: Some(CardMode::Skip),
        cards_blacklist: Some(vec![]),
        ..Default::default()
    });
    assert_eq!(disabled.cards, Some(CardMode::Skip));
    assert_eq!(disabled.cards_blacklist, Some(vec![]));
    let root = parse(r#"{"cards":false,"cards-blacklist":[],"extractor":{"cards":true,"cards-blacklist":["summary"],"twitter":{"cards":"ytdl","cards-blacklist":["player"]}}}"#).unwrap();
    for category in ["twitter", "instagram", "facebook", "direct"] {
        let options = root.resolve(category).unwrap();
        assert_eq!(options.cards, Some(CardMode::Skip));
        assert_eq!(options.cards_blacklist, Some(vec![]));
    }
    let absent = parse("{}").unwrap().resolve("twitter").unwrap();
    assert_eq!(absent.cards, None);
    assert_eq!(absent.cards_blacklist, None);
    let duplicate_values = parse(r#"{"cards-blacklist":["summary","summary"]}"#)
        .unwrap()
        .resolve("twitter")
        .unwrap();
    assert_eq!(
        duplicate_values.cards_blacklist.unwrap(),
        ["summary", "summary"]
    );
    let max = serde_json::json!({"cards-blacklist":vec!["x".repeat(2048); 256]});
    assert!(parse(&max.to_string()).is_ok());
}

#[test]
fn twitter_card_settings_reject_private_malformed_values_and_duplicate_keys() {
    use serde_json::json;
    let bad_cards = [
        json!(null),
        json!(0),
        json!("true"),
        json!("Ytdl"),
        json!([]),
        json!({"private-key":"private-value"}),
        json!("private-value"),
    ];
    let bad_blacklists = [
        json!(null),
        json!(false),
        json!("private-value"),
        json!({"private-key":"private-value"}),
        json!(["valid", 1]),
        json!([""]),
        json!(["private-value\n"]),
        json!(["private-value\u{7f}"]),
        json!(["x".repeat(2049)]),
        json!(vec!["private-value"; 257]),
    ];
    for (field, values) in [
        ("cards", bad_cards.as_slice()),
        ("cards-blacklist", bad_blacklists.as_slice()),
    ] {
        for value in values {
            for document in [
                json!({(field):value}),
                json!({"extractor":{(field):value}}),
                json!({"extractor":{"twitter":{(field):value}}}),
                json!({"extractor":{"instagram":{(field):value}}}),
                json!({"extractor":{"facebook":{(field):value}}}),
                json!({"extractor":{"direct":{(field):value}}}),
            ] {
                let message = parse(&document.to_string()).unwrap_err().to_string();
                assert!(
                    !message.contains("private-key") && !message.contains("private-value"),
                    "{message}"
                );
            }
        }
        let value = if field == "cards" { "false" } else { "[]" };
        let duplicate = format!(r#""{field}":{value},"{field}":{value}"#);
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

#[test]
fn twitter_unavailable_is_sparse_strict_and_preserves_false_precedence() {
    use serde_json::json;
    assert_eq!(
        parse("{}").unwrap().resolve("twitter").unwrap().unavailable,
        None
    );
    let config =
        parse(r#"{"extractor":{"unavailable":true,"twitter":{"unavailable":false}}}"#).unwrap();
    let twitter = config.resolve("twitter").unwrap();
    assert_eq!(twitter.unavailable, Some(false));
    let inherited = config.resolve("instagram").unwrap();
    assert_eq!(inherited.unavailable, Some(true));
    assert_eq!(twitter.overlay(&Options::default()), twitter);
    assert_eq!(
        inherited
            .overlay(&Options {
                unavailable: Some(false),
                ..Default::default()
            })
            .unavailable,
        Some(false)
    );
    assert_eq!(parse(r#"{"unavailable":false,"extractor":{"unavailable":true,"twitter":{"unavailable":true}}}"#).unwrap().resolve("twitter").unwrap().unavailable, Some(false));
    for value in [
        json!(null),
        json!(1),
        json!("false"),
        json!([]),
        json!({"private-key":"private-value"}),
    ] {
        for document in [
            json!({"unavailable":value}),
            json!({"extractor":{"unavailable":value}}),
            json!({"extractor":{"twitter":{"unavailable":value}}}),
            json!({"extractor":{"instagram":{"unavailable":value}}}),
            json!({"extractor":{"facebook":{"unavailable":value}}}),
            json!({"extractor":{"direct":{"unavailable":value}}}),
        ] {
            let message = parse(&document.to_string()).unwrap_err().to_string();
            assert!(!message.contains("private-key") && !message.contains("private-value"));
        }
    }
    for document in [
        r#"{"unavailable":false,"unavailable":true}"#,
        r#"{"extractor":{"unavailable":false,"unavailable":true}}"#,
        r#"{"extractor":{"twitter":{"unavailable":false,"unavailable":true}}}"#,
    ] {
        assert!(parse(document).is_err());
    }
}

#[test]
fn twitter_article_flags_are_sparse_scoped_and_accept_canonical_aliases() {
    use gallery_dl_rs::twitter::ArticleOptions;
    let disabled = ArticleOptions::disabled();
    let config = parse(r#"{"extractor":{"articles":true,"twitter":{"articles":[" html ","metadata","document","meta"]},"instagram":{"articles":false}}}"#).unwrap();
    let twitter = config.resolve("twitter").unwrap();
    let selected = ArticleOptions {
        metadata: true,
        cover: false,
        media: false,
        html: true,
        document: true,
    };
    assert_eq!(twitter.articles, Some(selected));
    assert_eq!(
        config.resolve("facebook").unwrap().articles,
        Some(ArticleOptions::default())
    );
    assert_eq!(
        config.resolve("instagram").unwrap().articles,
        Some(disabled)
    );
    assert_eq!(twitter.overlay(&Options::default()), twitter);
    assert_eq!(
        twitter
            .overlay(&Options {
                articles: Some(disabled),
                ..Default::default()
            })
            .articles,
        Some(disabled)
    );
    let root = parse(
        r#"{"articles":false,"extractor":{"articles":true,"twitter":{"articles":"cover,media"}}}"#,
    )
    .unwrap();
    for category in ["twitter", "instagram", "facebook", "direct"] {
        assert_eq!(root.resolve(category).unwrap().articles, Some(disabled));
    }
    for value in [
        serde_json::json!(""),
        serde_json::json!("  "),
        serde_json::json!([]),
    ] {
        let document =
            serde_json::json!({"extractor":{"articles":true,"twitter":{"articles":value}}});
        assert_eq!(
            parse(&document.to_string())
                .unwrap()
                .resolve("twitter")
                .unwrap()
                .articles,
            Some(disabled)
        );
    }
    let equivalent = parse(r#"{"articles":" meta, html, doc "}"#)
        .unwrap()
        .resolve("twitter")
        .unwrap();
    assert_eq!(equivalent.articles, Some(selected));
    assert_eq!(
        parse("{}").unwrap().resolve("twitter").unwrap().articles,
        None
    );
    assert!(parse(&serde_json::json!({"articles":vec!["meta";32]}).to_string()).is_ok());
}

#[test]
fn twitter_article_flags_reject_malformed_values_privately_in_all_scopes() {
    use serde_json::json;
    for value in [
        json!(null),
        json!(0),
        json!({"private-key":"private-value"}),
        json!([true]),
        json!(["meta", 1]),
        json!("private-value"),
        json!("Meta"),
        json!("meta,,cover"),
        json!([""]),
        json!(["meta,cover"]),
        json!(["meta\n"]),
        json!(vec!["meta"; 33]),
        json!(["x".repeat(129)]),
    ] {
        for document in [
            json!({"articles":value}),
            json!({"extractor":{"articles":value}}),
            json!({"extractor":{"twitter":{"articles":value}}}),
            json!({"extractor":{"instagram":{"articles":value}}}),
            json!({"extractor":{"facebook":{"articles":value}}}),
            json!({"extractor":{"direct":{"articles":value}}}),
        ] {
            let message = parse(&document.to_string()).unwrap_err().to_string();
            assert!(!message.contains("private-key") && !message.contains("private-value"));
        }
    }
    for document in [
        r#"{"articles":true,"articles":false}"#,
        r#"{"extractor":{"articles":[],"articles":true}}"#,
        r#"{"extractor":{"twitter":{"articles":"html","articles":"doc"}}}"#,
    ] {
        assert!(parse(document).is_err());
    }
}

#[test]
fn image_size_order_replaces_scopes_and_fallback_preserves_explicit_false() {
    use serde_json::json;
    let config = parse(r#"{"extractor":{"size":["orig","large"],"fallback":true,"twitter":{"size":" medium,small ","fallback":false}}}"#).unwrap();
    let twitter = config.resolve("twitter").unwrap();
    assert_eq!(twitter.size.as_ref().unwrap().sizes(), ["medium", "small"]);
    assert_eq!(twitter.fallback, Some(false));
    let inherited = config.resolve("instagram").unwrap();
    assert_eq!(inherited.size.as_ref().unwrap().sizes(), ["orig", "large"]);
    assert_eq!(inherited.fallback, Some(true));
    assert_eq!(twitter.overlay(&Options::default()), twitter);
    let overlay = inherited.overlay(&Options {
        size: twitter.size.clone(),
        fallback: Some(false),
        ..Default::default()
    });
    assert_eq!(overlay, twitter);
    let root = parse(r#"{"size":["4096x4096"],"fallback":false,"extractor":{"size":"large","fallback":true,"twitter":{"size":"small","fallback":true}}}"#).unwrap();
    for category in ["twitter", "instagram", "facebook", "direct"] {
        let options = root.resolve(category).unwrap();
        assert_eq!(options.size.as_ref().unwrap().sizes(), ["4096x4096"]);
        assert_eq!(options.fallback, Some(false));
    }
    let absent = parse("{}").unwrap().resolve("twitter").unwrap();
    assert_eq!(absent.size, None);
    assert_eq!(absent.fallback, None);
    assert!(parse(&json!({"size":vec!["x".repeat(32);32]}).to_string()).is_ok());
    assert!(parse(&json!({"size":["custom_size-2"]}).to_string()).is_ok());
}

#[test]
fn image_size_and_fallback_configuration_reject_invalid_private_values_and_duplicate_keys() {
    use serde_json::json;
    let bad_size = [
        json!(null),
        json!(true),
        json!(1),
        json!([]),
        json!(""),
        json!("orig,,large"),
        json!(["orig", 1]),
        json!(["private-value\n"]),
        json!("private-value/path"),
        json!(["x".repeat(33)]),
        json!(vec!["orig"; 33]),
    ];
    let bad_fallback = [
        json!(null),
        json!(0),
        json!("false"),
        json!([]),
        json!({"private-key":"private-value"}),
    ];
    for (field, values) in [
        ("size", bad_size.as_slice()),
        ("fallback", bad_fallback.as_slice()),
    ] {
        for value in values {
            for document in [
                json!({(field):value}),
                json!({"extractor":{(field):value}}),
                json!({"extractor":{"twitter":{(field):value}}}),
                json!({"extractor":{"instagram":{(field):value}}}),
                json!({"extractor":{"facebook":{(field):value}}}),
                json!({"extractor":{"direct":{(field):value}}}),
            ] {
                let message = parse(&document.to_string()).unwrap_err().to_string();
                assert!(!message.contains("private-key") && !message.contains("private-value"));
            }
        }
        let value = if field == "size" { "\"orig\"" } else { "false" };
        let duplicate = format!(r#""{field}":{value},"{field}":{value}"#);
        for document in [
            format!("{{{duplicate}}}"),
            format!("{{\"extractor\":{{{duplicate}}}}}"),
            format!("{{\"extractor\":{{\"twitter\":{{{duplicate}}}}}}}"),
        ] {
            assert!(parse(&document).is_err());
        }
    }
}

#[test]
fn file_filter_scopes_replace_clauses_and_resolve_image_filter_alias_within_each_layer() {
    let config = parse(r#"{"extractor":{"file-filter":"width > 10","image-filter":"height > 10","twitter":{"file-filter":[],"image-filter":["width > 20","height > 20"]},"instagram":{"image-filter":"width > 30"}}}"#).unwrap();
    let twitter = config.resolve("twitter").unwrap();
    assert_eq!(
        twitter.file_filter,
        Some(vec!["width > 20".into(), "height > 20".into()])
    );
    assert_eq!(
        config.resolve("instagram").unwrap().file_filter,
        Some(vec!["width > 30".into()])
    );
    assert_eq!(
        config.resolve("facebook").unwrap().file_filter,
        Some(vec!["width > 10".into()])
    );
    assert_eq!(twitter.overlay(&Options::default()), twitter);
    assert_eq!(
        twitter
            .overlay(&Options {
                file_filter: Some(vec![]),
                ..Default::default()
            })
            .file_filter,
        Some(vec![])
    );
    let primary = parse(r#"{"file-filter":["width > 50"],"image-filter":"height > 50","extractor":{"twitter":{"file-filter":"width > 1"}}}"#).unwrap();
    assert_eq!(
        primary.resolve("twitter").unwrap().file_filter,
        Some(vec!["width > 50".into()])
    );
    for empty in [serde_json::json!(""), serde_json::json!([])] {
        let clear = serde_json::json!({"file-filter":empty,"extractor":{"file-filter":"width > 1","twitter":{"image-filter":"height > 1"}}});
        for category in ["twitter", "instagram", "facebook", "direct"] {
            assert_eq!(
                parse(&clear.to_string())
                    .unwrap()
                    .resolve(category)
                    .unwrap()
                    .file_filter,
                Some(vec![])
            );
        }
        let alias = serde_json::json!({"file-filter":empty,"image-filter":"height > 99"});
        assert_eq!(
            parse(&alias.to_string())
                .unwrap()
                .resolve("twitter")
                .unwrap()
                .file_filter,
            Some(vec!["height > 99".into()])
        );
    }
    assert_eq!(
        parse("{}").unwrap().resolve("twitter").unwrap().file_filter,
        None
    );
    assert!(parse(&serde_json::json!({"file-filter":vec!["width > 0";32]}).to_string()).is_ok());
}

#[test]
fn file_filter_aliases_reject_private_invalid_syntax_types_bounds_and_duplicate_keys() {
    use serde_json::json;
    for field in ["file-filter", "image-filter"] {
        for value in [
            json!(null),
            json!(false),
            json!(1),
            json!({"private-key":"private-value"}),
            json!([1]),
            json!([""]),
            json!(["width > 0", ""]),
            json!("width >"),
            json!("PRIVATE-FILTER-VALUE + )"),
            json!(vec!["width > 0"; 33]),
            json!(format!("width > 0{}", " ".repeat(16 * 1024))),
            json!(vec![format!("width > 0{}", " ".repeat(16 * 1024 - 9)); 5]),
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
                for private in ["private-key", "private-value", "PRIVATE-FILTER-VALUE"] {
                    assert!(!message.contains(private));
                }
            }
        }
        let duplicate = format!(r#""{field}":"width > 0","{field}":"width > 1""#);
        for document in [
            format!("{{{duplicate}}}"),
            format!("{{\"extractor\":{{{duplicate}}}}}"),
            format!("{{\"extractor\":{{\"twitter\":{{{duplicate}}}}}}}"),
            format!("{{\"extractor\":{{\"instagram\":{{{duplicate}}}}}}}"),
            format!("{{\"extractor\":{{\"facebook\":{{{duplicate}}}}}}}"),
            format!("{{\"extractor\":{{\"direct\":{{{duplicate}}}}}}}"),
        ] {
            assert!(parse(&document).is_err());
        }
    }
}

#[test]
fn post_selection_scopes_replace_and_clear_independently_from_file_selection() {
    let config = parse(r#"{"extractor":{"post-range":"1-3","post-filter":"post_id > 100","file-range":"5","file-filter":"width > 1000","twitter":{"post-range":["2","4-6"],"post-filter":["post_id > 200","count > 0"]}}}"#).unwrap();
    let twitter = config.resolve("twitter").unwrap();
    assert_eq!(twitter.post_range.as_deref(), Some("2,4-6"));
    assert_eq!(
        twitter.post_filter,
        Some(vec!["post_id > 200".into(), "count > 0".into()])
    );
    assert_eq!(twitter.file_range.as_deref(), Some("5"));
    assert_eq!(twitter.file_filter, Some(vec!["width > 1000".into()]));
    let inherited = config.resolve("instagram").unwrap();
    assert_eq!(inherited.post_range.as_deref(), Some("1-3"));
    assert_eq!(inherited.post_filter, Some(vec!["post_id > 100".into()]));
    assert_eq!(twitter.overlay(&Options::default()), twitter);
    let explicit = twitter.overlay(&Options {
        post_range: Some("7:".into()),
        post_filter: Some(vec![]),
        ..Default::default()
    });
    assert_eq!(explicit.post_range.as_deref(), Some("7:"));
    assert_eq!(explicit.post_filter, Some(vec![]));
    assert_eq!(explicit.file_filter, twitter.file_filter);
    for empty in [serde_json::json!(""), serde_json::json!([])] {
        let root = serde_json::json!({"post-range":empty,"post-filter":empty,"extractor":{"post-range":"0","post-filter":"False","twitter":{"post-range":"2","post-filter":"True"}}});
        for category in ["twitter", "instagram", "facebook", "direct"] {
            let resolved = parse(&root.to_string()).unwrap().resolve(category).unwrap();
            assert_eq!(resolved.post_range.as_deref(), Some(":"));
            assert_eq!(resolved.post_filter, Some(vec![]));
        }
    }
    let absent = parse("{}").unwrap().resolve("direct").unwrap();
    assert_eq!(absent.post_range, None);
    assert_eq!(absent.post_filter, None);
    assert!(
        parse(
            &serde_json::json!({"post-range":vec!["1";256],"post-filter":vec!["count > 0";32]})
                .to_string()
        )
        .is_ok()
    );
}

#[test]
fn post_selection_invalid_types_syntax_and_bounds_fail_privately_in_all_scopes() {
    use serde_json::json;
    let ranges = [
        json!(null),
        json!(false),
        json!(1),
        json!({"private-key":"private-value"}),
        json!([1]),
        json!("PRIVATE-POST-VALUE"),
        json!("1::0"),
        json!("1".repeat(4097)),
        json!(vec!["1"; 257]),
    ];
    let filters = [
        json!(null),
        json!(false),
        json!(1),
        json!({"private-key":"private-value"}),
        json!([1]),
        json!([""]),
        json!("PRIVATE-POST-VALUE > )"),
        json!(vec!["True"; 33]),
        json!(format!("True{}", " ".repeat(16 * 1024))),
        json!(vec![format!("True{}", " ".repeat(16 * 1024 - 4)); 5]),
    ];
    for (field, values) in [
        ("post-range", ranges.as_slice()),
        ("post-filter", filters.as_slice()),
    ] {
        for value in values {
            for document in [
                json!({(field):value}),
                json!({"extractor":{(field):value}}),
                json!({"extractor":{"twitter":{(field):value}}}),
                json!({"extractor":{"instagram":{(field):value}}}),
                json!({"extractor":{"facebook":{(field):value}}}),
                json!({"extractor":{"direct":{(field):value}}}),
            ] {
                let message = parse(&document.to_string()).unwrap_err().to_string();
                assert!(message.contains(field), "{message}");
                for private in ["private-key", "private-value", "PRIVATE-POST-VALUE"] {
                    assert!(!message.contains(private));
                }
            }
        }
        let value = if field == "post-range" {
            "\"1\""
        } else {
            "\"True\""
        };
        let duplicate = format!(r#""{field}":{value},"{field}":{value}"#);
        for document in [
            format!("{{{duplicate}}}"),
            format!("{{\"extractor\":{{{duplicate}}}}}"),
            format!("{{\"extractor\":{{\"twitter\":{{{duplicate}}}}}}}"),
            format!("{{\"extractor\":{{\"instagram\":{{{duplicate}}}}}}}"),
            format!("{{\"extractor\":{{\"facebook\":{{{duplicate}}}}}}}"),
            format!("{{\"extractor\":{{\"direct\":{{{duplicate}}}}}}}"),
        ] {
            assert!(parse(&document).is_err());
        }
    }
}

#[test]
fn text_tweet_settings_are_sparse_scoped_and_preserve_explicit_false() {
    let config =
        parse(r#"{"extractor":{"text-tweets":true,"twitter":{"text-tweets":false}}}"#).unwrap();
    let twitter = config.resolve("twitter").unwrap();
    assert_eq!(twitter.text_tweets, Some(false));
    let inherited = config.resolve("instagram").unwrap();
    assert_eq!(inherited.text_tweets, Some(true));
    assert_eq!(twitter.overlay(&Options::default()), twitter);
    assert_eq!(
        inherited
            .overlay(&Options {
                text_tweets: Some(false),
                ..Default::default()
            })
            .text_tweets,
        Some(false)
    );
    let root = parse(
        r#"{"text-tweets":false,"extractor":{"text-tweets":true,"twitter":{"text-tweets":true}}}"#,
    )
    .unwrap();
    for category in ["twitter", "instagram", "facebook", "direct"] {
        assert_eq!(root.resolve(category).unwrap().text_tweets, Some(false));
    }
    assert_eq!(
        parse("{}").unwrap().resolve("twitter").unwrap().text_tweets,
        None
    );
}

#[test]
fn text_tweet_configuration_rejects_private_types_and_duplicate_keys() {
    use serde_json::json;
    for value in [
        json!(null),
        json!(1),
        json!("false"),
        json!([]),
        json!({"private-key":"PRIVATE-TEXT-VALUE"}),
    ] {
        for document in [
            json!({"text-tweets":value}),
            json!({"extractor":{"text-tweets":value}}),
            json!({"extractor":{"twitter":{"text-tweets":value}}}),
            json!({"extractor":{"instagram":{"text-tweets":value}}}),
            json!({"extractor":{"facebook":{"text-tweets":value}}}),
            json!({"extractor":{"direct":{"text-tweets":value}}}),
        ] {
            let message = parse(&document.to_string()).unwrap_err().to_string();
            assert!(message.contains("text-tweets"));
            assert!(!message.contains("private-key") && !message.contains("PRIVATE-TEXT-VALUE"));
        }
    }
    for document in [
        r#"{"text-tweets":true,"text-tweets":false}"#,
        r#"{"extractor":{"text-tweets":false,"text-tweets":true}}"#,
        r#"{"extractor":{"twitter":{"text-tweets":false,"text-tweets":true}}}"#,
    ] {
        assert!(parse(document).is_err());
    }
}

#[test]
fn conditional_templates_preserve_order_and_replace_whole_settings() {
    let config = parse(
        r#"{"extractor":{"filename":{"index > 1":"b.{extension}","index > 0":"a.{extension}","":"{filename}"},"directory":{"username == 'x'":["x"],"":["{category}"]},"twitter":{"filename":"t.{extension}","directory":["t"]}}}"#,
    )
    .unwrap();
    let direct = config.resolve("direct").unwrap();
    assert_eq!(direct.filename.as_deref(), Some("{filename}"));
    assert_eq!(
        direct.filename_conditions,
        Some(vec![
            ("index > 1".to_string(), "b.{extension}".to_string()),
            ("index > 0".to_string(), "a.{extension}".to_string()),
        ])
    );
    assert_eq!(direct.directory, Some(vec!["{category}".to_string()]));
    assert_eq!(
        direct.directory_conditions,
        Some(vec![("username == 'x'".to_string(), vec!["x".to_string()])])
    );
    // A plain category value replaces inherited conditional settings.
    let twitter = config.resolve("twitter").unwrap();
    assert_eq!(twitter.filename.as_deref(), Some("t.{extension}"));
    assert_eq!(twitter.filename_conditions, None);
    assert_eq!(twitter.directory_conditions, None);
    // An explicit CLI filename replaces the inherited conditions, too.
    let cli = direct.overlay(&Options {
        filename: Some("{name}".into()),
        ..Options::default()
    });
    assert_eq!(cli.filename_conditions, None);
    for bad in [
        r#"{"filename":{"index ==":"x"}}"#,
        r#"{"filename":{"1":"a/b"}}"#,
        r#"{"filename":{"1":1}}"#,
        r#"{"directory":{"1":"x"}}"#,
        r#"{"directory":{"1":["../x"]}}"#,
    ] {
        assert!(parse(bad).is_err(), "{bad}");
    }
}

#[test]
fn api_key_is_tumblr_flickr_wallhaven_and_weasyl_only_and_never_echoed() {
    assert!(
        parse(r#"{"extractor":{"artstation":{"filename":"{post_id}_{media_id}.{extension}"}}}"#)
            .is_ok()
    );
    assert!(
        parse(r#"{"extractor":{"pinterest":{"filename":"{post_id}_{index}.{extension}"}}}"#)
            .is_ok()
    );
    assert!(
        parse(r#"{"extractor":{"newgrounds":{"filename":"{post_id}_{index}.{extension}"}}}"#)
            .is_ok()
    );
    assert!(parse(r#"{"extractor":{"furaffinity":{"filename":"{post_id}.{extension}"}}}"#).is_ok());
    assert!(parse(r#"{"extractor":{"vsco":{"filename":"{media_id}.{extension}"}}}"#).is_ok());
    let config =
        parse(r#"{"extractor":{"tumblr":{"api-key":"abc123"},"flickr":{"api-key":"f00d"},"wallhaven":{"api-key":"beef"},"weasyl":{"api-key":"cafe"}}}"#)
            .unwrap();
    assert_eq!(
        config.resolve("wallhaven").unwrap().api_key.as_deref(),
        Some("beef")
    );
    assert_eq!(
        config.resolve("flickr").unwrap().api_key.as_deref(),
        Some("f00d")
    );
    assert_eq!(
        config.resolve("tumblr").unwrap().api_key.as_deref(),
        Some("abc123")
    );
    assert_eq!(
        config.resolve("weasyl").unwrap().api_key.as_deref(),
        Some("cafe")
    );
    assert_eq!(config.resolve("reddit").unwrap().api_key, None);
    for json in [
        r#"{"api-key":"abc123"}"#,
        r#"{"extractor":{"api-key":"abc123"}}"#,
        r#"{"extractor":{"reddit":{"api-key":"abc123"}}}"#,
        r#"{"extractor":{"tumblr":{"api-key":"abc 123!"}}}"#,
        r#"{"extractor":{"tumblr":{"api-key":7}}}"#,
        r#"{"extractor":{"flickr":{"api-key":"abc-123"}}}"#,
        r#"{"extractor":{"artstation":{"api-key":"abc123"}}}"#,
        r#"{"extractor":{"pinterest":{"api-key":"abc123"}}}"#,
        r#"{"extractor":{"newgrounds":{"api-key":"abc123"}}}"#,
        r#"{"extractor":{"furaffinity":{"api-key":"abc123"}}}"#,
        r#"{"extractor":{"vsco":{"api-key":"abc123"}}}"#,
        r#"{"extractor":{"wallhaven":{"api-key":"abc 123"}}}"#,
        r#"{"extractor":{"weasyl":{"api-key":"abc-123"}}}"#,
    ] {
        let error = parse(json).err().unwrap().to_string();
        assert!(!error.contains("abc"), "{error}");
    }
}

#[test]
fn refresh_token_is_pixiv_scoped_and_never_echoed() {
    let config = parse(r#"{"extractor":{"pixiv":{"refresh-token":"abc_123-x"}}}"#).unwrap();
    assert_eq!(
        config.resolve("pixiv").unwrap().refresh_token.as_deref(),
        Some("abc_123-x")
    );
    assert_eq!(config.resolve("tumblr").unwrap().refresh_token, None);
    for json in [
        r#"{"refresh-token":"abc123"}"#,
        r#"{"extractor":{"refresh-token":"abc123"}}"#,
        r#"{"extractor":{"tumblr":{"refresh-token":"abc123"}}}"#,
        r#"{"extractor":{"pixiv":{"refresh-token":"abc 123!"}}}"#,
        r#"{"extractor":{"pixiv":{"refresh-token":7}}}"#,
    ] {
        let error = parse(json).err().unwrap().to_string();
        assert!(!error.contains("abc"), "{error}");
    }
}

#[test]
fn novel_settings_and_credentials_are_separate_from_illustrations() {
    let config = parse(r#"{"extractor":{"pixiv":{"refresh-token":"art_token"},"pixiv-novel":{"refresh-token":"novel_token","covers":true,"full-series":true,"filename":"novel.{extension}"}}}"#).unwrap();
    let novels = config.resolve("pixiv-novel").unwrap();
    assert_eq!(novels.refresh_token.as_deref(), Some("novel_token"));
    assert_eq!(novels.covers, Some(true));
    assert_eq!(novels.full_series, Some(true));
    assert_eq!(novels.filename.as_deref(), Some("novel.{extension}"));
    let art = config.resolve("pixiv").unwrap();
    assert_eq!(art.refresh_token.as_deref(), Some("art_token"));
    assert_eq!(art.covers, None);
    assert_eq!(art.full_series, None);
    let overridden = novels.overlay(&Options {
        covers: Some(false),
        full_series: Some(false),
        ..Default::default()
    });
    assert_eq!(overridden.covers, Some(false));
    assert_eq!(overridden.full_series, Some(false));
    for (document, category, covers, full_series) in [
        (r#"{"covers":true}"#, "pixiv-novel", Some(true), None),
        (
            r#"{"extractor":{"covers":true}}"#,
            "pixiv-novel",
            Some(true),
            None,
        ),
        (
            r#"{"extractor":{"pixiv":{"full-series":true}}}"#,
            "pixiv",
            None,
            Some(true),
        ),
        (
            r#"{"extractor":{"tumblr":{"covers":false}}}"#,
            "tumblr",
            Some(false),
            None,
        ),
        (
            r#"{"extractor":{"pixiv-novel":{"covers":"secret-value"}}}"#,
            "pixiv-novel",
            Some(true),
            None,
        ),
        (
            r#"{"extractor":{"pixiv-novel":{"full-series":1}}}"#,
            "pixiv-novel",
            None,
            Some(true),
        ),
    ] {
        let resolved = parse(document).unwrap().resolve(category).unwrap();
        assert_eq!(resolved.covers, covers);
        assert_eq!(resolved.full_series, full_series);
        assert!(!format!("{resolved:?}").contains("secret-value"));
    }
    let message = parse(r#"{"extractor":{"pixiv-novel":{"refresh-token":"secret value"}}}"#)
        .unwrap_err()
        .to_string();
    assert!(!message.contains("secret"));
}

#[test]
fn client_credentials_are_deviantart_only_and_never_echoed() {
    let config =
        parse(r#"{"extractor":{"deviantart":{"client-id":"123","client-secret":"abcdef0"}}}"#)
            .unwrap();
    let resolved = config.resolve("deviantart").unwrap();
    assert_eq!(resolved.client_id.as_deref(), Some("123"));
    assert_eq!(resolved.client_secret.as_deref(), Some("abcdef0"));
    assert_eq!(config.resolve("pixiv").unwrap().client_secret, None);
    for json in [
        r#"{"client-secret":"abc123"}"#,
        r#"{"extractor":{"client-id":"123"}}"#,
        r#"{"extractor":{"pixiv":{"client-secret":"abc123"}}}"#,
        r#"{"extractor":{"deviantart":{"client-id":"abc1"}}}"#,
        r#"{"extractor":{"deviantart":{"client-secret":"abc 123!"}}}"#,
        r#"{"extractor":{"deviantart":{"client-secret":7}}}"#,
    ] {
        let error = parse(json).err().unwrap().to_string();
        assert!(!error.contains("abc"), "{error}");
    }
}

#[test]
fn site_media_booleans_obey_scope_precedence_and_reject_private_invalid_values() {
    let cfg = parse(r#"{"extractor":{"mp4":true,"reblogs":false,"text-posts":false,"imgur":{"mp4":false},"mastodon":{"reblogs":true,"text-posts":true,"replies":false}}}"#).unwrap();
    assert_eq!(cfg.resolve("imgur").unwrap().mp4, Some(false));
    assert_eq!(cfg.resolve("reddit").unwrap().mp4, Some(true));
    let m = cfg.resolve("mastodon").unwrap();
    assert_eq!(m.reblogs, Some(true));
    assert_eq!(m.text_posts, Some(true));
    assert_eq!(m.replies, Some(gallery_dl_rs::twitter::ReplyMode::Skip));
    assert_eq!(
        m.overlay(&Options {
            reblogs: Some(false),
            ..Options::default()
        })
        .reblogs,
        Some(false)
    );
    let cfg = parse(r#"{"mp4":true,"reblogs":false,"text-posts":false,"extractor":{"imgur":{"mp4":false},"mastodon":{"reblogs":true,"text-posts":true}}}"#).unwrap();
    assert_eq!(cfg.resolve("imgur").unwrap().mp4, Some(true));
    assert_eq!(cfg.resolve("mastodon").unwrap().reblogs, Some(false));
    assert_eq!(cfg.resolve("mastodon").unwrap().text_posts, Some(false));
    for key in ["mp4", "reblogs", "text-posts"] {
        for bad in [
            serde_json::json!(null),
            serde_json::json!(1),
            serde_json::json!("private-value"),
            serde_json::json!([]),
        ] {
            let body = serde_json::json!({"extractor": {"mastodon": {key: bad}}}).to_string();
            let error = parse(&body).err().unwrap().to_string();
            assert!(error.contains(key), "{error}");
            assert!(!error.contains("private-value"));
        }
    }
}

#[test]
fn pixeldrain_api_keys_are_opaque_and_confined_to_the_category() {
    let key = "opaque:key-_.!";
    let cfg = parse(&serde_json::json!({"extractor":{"pixeldrain":{"api-key":key}}}).to_string())
        .unwrap();
    assert_eq!(
        cfg.resolve("pixeldrain").unwrap().api_key.as_deref(),
        Some(key)
    );
    assert_eq!(cfg.resolve("catbox").unwrap().api_key, None);
    for value in [
        serde_json::json!(null),
        serde_json::json!(7),
        serde_json::json!("private key"),
        serde_json::json!("private\nkey"),
        serde_json::json!("x".repeat(4097)),
    ] {
        let body = serde_json::json!({"extractor":{"pixeldrain":{"api-key":value}}}).to_string();
        let error = parse(&body).err().unwrap().to_string();
        assert!(!error.contains("private"));
    }
}

#[test]
fn reposts_are_strict_booleans_and_ugoira_defaults_remain_scoped() {
    use gallery_dl_rs::pixiv::UgoiraMode;
    let cfg = parse(r#"{"extractor":{"reposts":false,"ugoira":false,"bluesky":{"reposts":true},"pixiv":{"ugoira":true}}}"#).unwrap();
    assert_eq!(cfg.resolve("bluesky").unwrap().reposts, Some(true));
    assert_eq!(cfg.resolve("pixiv").unwrap().ugoira, Some(UgoiraMode::Zip));
    assert_eq!(cfg.resolve("direct").unwrap().reposts, Some(false));
    let cfg = parse(r#"{"reposts":false,"ugoira":false,"extractor":{"bluesky":{"reposts":true},"pixiv":{"ugoira":true}}}"#).unwrap();
    assert_eq!(cfg.resolve("bluesky").unwrap().reposts, Some(false));
    assert_eq!(cfg.resolve("pixiv").unwrap().ugoira, Some(UgoiraMode::Skip));
    for value in [
        serde_json::json!(null),
        serde_json::json!(1),
        serde_json::json!("private-value"),
        serde_json::json!([]),
    ] {
        let error =
            parse(&serde_json::json!({"extractor":{"bluesky":{"reposts":value}}}).to_string())
                .err()
                .unwrap()
                .to_string();
        assert!(!error.contains("private-value"));
    }
}

#[test]
fn mastodon_instance_credentials_are_separate_and_inherit_shared_settings() {
    let cfg = parse(r#"{"extractor":{"retries":1,"mastodon":{"reblogs":true,"text-posts":true,"timeout":5},"mastodon.social":{"access-token":"social-token","replies":false},"pawoo":{"access-token":"pawoo-token","timeout":7}}}"#).unwrap();
    let social = cfg.resolve("mastodon.social").unwrap();
    assert_eq!(social.access_token.as_deref(), Some("social-token"));
    assert_eq!(social.reblogs, Some(true));
    assert_eq!(
        social.replies,
        Some(gallery_dl_rs::twitter::ReplyMode::Skip)
    );
    assert_eq!(social.timeout, Some(Duration::from_secs(5)));
    let pawoo = cfg.resolve("pawoo").unwrap();
    assert_eq!(pawoo.access_token.as_deref(), Some("pawoo-token"));
    assert_eq!(pawoo.reblogs, Some(true));
    assert_eq!(pawoo.timeout, Some(Duration::from_secs(7)));
    let baraag = cfg.resolve("baraag").unwrap();
    assert_eq!(baraag.access_token, None);
    assert_eq!(baraag.text_posts, Some(true));
    assert_eq!(cfg.resolve("mastodon").unwrap().access_token, None);
    let disabled = parse(r#"{"extractor":{"pawoo":{"access-token":false}}}"#).unwrap();
    assert_eq!(
        disabled.resolve("pawoo").unwrap().access_token.as_deref(),
        Some("")
    );
    for scope in ["mastodon", "twitter", "bluesky", "direct"] {
        assert!(
            parse(
                &serde_json::json!({"extractor":{scope:{"access-token":"private-token"}}})
                    .to_string()
            )
            .is_err()
        );
    }
    for value in [
        serde_json::json!(null),
        serde_json::json!(true),
        serde_json::json!(7),
        serde_json::json!("private token"),
        serde_json::json!("private\r\ntoken"),
        serde_json::json!("cache"),
        serde_json::json!("x".repeat(4097)),
    ] {
        let error =
            parse(&serde_json::json!({"extractor":{"pawoo":{"access-token":value}}}).to_string())
                .err()
                .unwrap()
                .to_string();
        assert!(!error.contains("private"));
    }
    for text in [
        r#"{"access-token":"private-token"}"#,
        r#"{"extractor":{"access-token":"private-token"}}"#,
        r#"{"extractor":{"pawoo":{"access-token":"a","access-token":"b"}}}"#,
    ] {
        assert!(parse(text).is_err());
    }
    assert!(!format!("{social:?}").contains("social-token"));
}

#[test]
fn tumblr_selections_replace_inherited_modes_and_preserve_global_precedence() {
    use gallery_dl_rs::tumblr::{Inline, Reblogs};
    let cfg = parse(r#"{"extractor":{"reblogs":false,"posts":"photo,video","inline":false,"original":false,"tumblr":{"reblogs":"same-blog","posts":["audio"],"inline":"reblog"}}}"#).unwrap();
    let selection = cfg.resolve("tumblr").unwrap().tumblr_selection();
    assert_eq!(selection.reblogs, Reblogs::SameBlog);
    assert_eq!(selection.inline, Inline::Reblog);
    assert!(!selection.original);
    assert_eq!(
        selection.posts.unwrap().into_iter().collect::<Vec<_>>(),
        vec!["audio"]
    );
    assert_eq!(cfg.resolve("mastodon").unwrap().reblogs, Some(false));
    let cfg = parse(r#"{"reblogs":false,"posts":"all","inline":"original","original":true,"extractor":{"tumblr":{"reblogs":"same-blog","posts":["photo"],"inline":false,"original":false}}}"#).unwrap();
    let options = cfg.resolve("tumblr").unwrap();
    let selection = options.tumblr_selection();
    assert_eq!(selection.reblogs, Reblogs::OriginalOnly);
    assert_eq!(selection.inline, Inline::Original);
    assert!(selection.original);
    assert!(selection.posts.is_none());
    let explicit = options.overlay(&Options {
        reblogs: Some(true),
        tumblr_reblogs: Some(Reblogs::SameBlog),
        ..Default::default()
    });
    assert_eq!(explicit.reblogs, None);
    assert_eq!(explicit.tumblr_selection().reblogs, Reblogs::SameBlog);
    let explicit = explicit.overlay(&Options {
        reblogs: Some(false),
        posts: Some(vec![]),
        ..Default::default()
    });
    assert_eq!(explicit.tumblr_selection().reblogs, Reblogs::OriginalOnly);
    assert!(explicit.tumblr_selection().posts.unwrap().is_empty());
}

#[test]
fn tumblr_invalid_modes_and_types_fail_without_echoing_values() {
    for (key, value) in [
        ("reblogs", serde_json::json!("PRIVATE_VALUE")),
        ("posts", serde_json::json!("PRIVATE_VALUE")),
        ("posts", serde_json::json!(["all"])),
        ("posts", serde_json::json!(true)),
        ("posts", serde_json::json!(null)),
        ("inline", serde_json::json!("PRIVATE_VALUE")),
        ("inline", serde_json::json!(1)),
        ("original", serde_json::json!("PRIVATE_VALUE")),
    ] {
        let error = parse(&serde_json::json!({"extractor":{"tumblr":{key:value}}}).to_string())
            .err()
            .unwrap()
            .to_string();
        assert!(!error.contains("PRIVATE_VALUE"));
    }
    for value in [
        serde_json::json!(false),
        serde_json::json!([]),
        serde_json::json!(""),
    ] {
        let options =
            parse(&serde_json::json!({"extractor":{"tumblr":{"posts":value}}}).to_string())
                .unwrap()
                .resolve("tumblr")
                .unwrap();
        assert!(options.tumblr_selection().posts.unwrap().is_empty());
    }
}

#[test]
fn booru_credentials_stay_in_their_category_and_are_private() {
    let cfg = parse(r#"{"extractor":{"danbooru":{"username":"alice","password":"key:one"},"e621":{"username":"bob","password":"key-two"},"gelbooru":{"user-id":42,"api-key":"key:&=!?"},"safebooru":{"user-id":"00043","api-key":"safe-key"}}}"#).unwrap();
    let danbooru = cfg.resolve("danbooru").unwrap();
    assert_eq!(danbooru.username.as_deref(), Some("alice"));
    assert_eq!(danbooru.password.as_deref(), Some("key:one"));
    assert_eq!(
        cfg.resolve("e621").unwrap().username.as_deref(),
        Some("bob")
    );
    assert_eq!(
        cfg.resolve("gelbooru").unwrap().user_id.as_deref(),
        Some("42")
    );
    assert_eq!(
        cfg.resolve("gelbooru").unwrap().api_key.as_deref(),
        Some("key:&=!?")
    );
    let safebooru = cfg.resolve("safebooru").unwrap();
    assert_eq!(safebooru.user_id.as_deref(), Some("00043"));
    assert_eq!(safebooru.api_key.as_deref(), Some("safe-key"));
    assert!(safebooru.username.is_none() && safebooru.password.is_none());
    assert!(!format!("{safebooru:?}").contains("safe-key"));
    for category in ["twitter", "direct", "tumblr"] {
        let options = cfg.resolve(category).unwrap();
        assert!(
            options.username.is_none() && options.password.is_none() && options.user_id.is_none()
        );
    }
    assert!(!format!("{danbooru:?}").contains("key:one"));
    for body in [
        serde_json::json!({"username":"PRIVATE_VALUE"}),
        serde_json::json!({"extractor":{"password":"PRIVATE_VALUE"}}),
        serde_json::json!({"extractor":{"twitter":{"username":"PRIVATE_VALUE"}}}),
        serde_json::json!({"extractor":{"gelbooru":{"password":"PRIVATE_VALUE"}}}),
        serde_json::json!({"extractor":{"safebooru":{"user-id":"PRIVATE_VALUE"}}}),
        serde_json::json!({"extractor":{"danbooru":{"username":"PRIVATE:VALUE"}}}),
        serde_json::json!({"extractor":{"e621":{"password":"PRIVATE\nVALUE"}}}),
        serde_json::json!({"extractor":{"gelbooru":{"api-key":"PRIVATE VALUE"}}}),
        serde_json::json!({"extractor":{"gelbooru":{"user-id":0}}}),
    ] {
        let error = parse(&body.to_string()).err().unwrap().to_string();
        assert!(!error.contains("PRIVATE"), "{error}");
    }
}

#[test]
fn source_post_limits_inherit_and_zero_explicitly_clears_limits() {
    let config = parse(r#"{"extractor":{"max-posts":20,"expand":true,"kemono":{"max-posts":2,"expand":false},"coomer":{"max-posts":0},"pixiv":{"max-posts":4},"pixiv-novel":{"max-posts":0}}}"#).unwrap();
    let kemono = config.resolve("kemono").unwrap();
    assert_eq!((kemono.max_posts, kemono.expand), (Some(2), Some(false)));
    let coomer = config.resolve("coomer").unwrap();
    assert_eq!((coomer.max_posts, coomer.expand), (Some(0), Some(true)));
    assert_eq!(config.resolve("pixiv").unwrap().max_posts, Some(4));
    assert_eq!(config.resolve("pixiv-novel").unwrap().max_posts, Some(0));
    let generic = config.resolve("instagram").unwrap();
    assert_eq!(generic.max_posts, Some(20));
    assert_eq!(
        generic
            .overlay(&Options {
                max_posts: Some(0),
                ..Default::default()
            })
            .max_posts,
        Some(0)
    );
    assert_eq!(kemono.overlay(&Options::default()), kemono);
    let root =
        parse(r#"{"max-posts":3,"extractor":{"max-posts":20,"kemono":{"max-posts":2}}}"#).unwrap();
    assert_eq!(root.resolve("kemono").unwrap().max_posts, Some(3));
    assert_eq!(
        parse("{}").unwrap().resolve("kemono").unwrap().max_posts,
        None
    );
}

#[test]
fn source_post_limits_reject_invalid_types_privately_in_all_scopes() {
    use serde_json::json;
    for value in [
        json!(null),
        json!(true),
        json!(false),
        json!(-1),
        json!(1.5),
        json!("PRIVATE_VALUE"),
        json!([]),
        json!({"PRIVATE_VALUE": 1}),
    ] {
        for document in [
            json!({"max-posts":value}),
            json!({"extractor":{"max-posts":value}}),
            json!({"extractor":{"kemono":{"max-posts":value}}}),
            json!({"extractor":{"coomer":{"max-posts":value}}}),
            json!({"extractor":{"pixiv":{"max-posts":value}}}),
            json!({"extractor":{"pixiv-novel":{"max-posts":value}}}),
        ] {
            let failure = parse(&document.to_string()).unwrap_err().to_string();
            assert!(failure.contains("max-posts"), "{failure}");
            assert!(!failure.contains("PRIVATE_VALUE"), "{failure}");
        }
    }
    assert!(parse(r#"{"max-posts":1,"max-posts":2}"#).is_err());
}

#[test]
fn kemono_file_selection_is_typed_scoped_and_preserves_explicit_empty_values() {
    use gallery_dl_rs::kemono::{DuplicateOptions, FileKind};
    let config = parse(r#"{"extractor":{"files":"file, inline","duplicates":true,"original":true,"kemono":{"files":["attachments","file","attachments"],"duplicates":["attachment"],"original":false},"coomer":{"files":[],"duplicates":false}}}"#).unwrap();
    let kemono = config.resolve("kemono").unwrap();
    assert_eq!(
        kemono.files,
        Some(vec![
            FileKind::Attachments,
            FileKind::File,
            FileKind::Attachments
        ])
    );
    assert_eq!(
        kemono.duplicates,
        Some(DuplicateOptions {
            attachment: true,
            ..Default::default()
        })
    );
    assert!(!kemono.kemono_selection().original);
    let coomer = config.resolve("coomer").unwrap();
    assert_eq!(coomer.files, Some(vec![]));
    assert_eq!(coomer.duplicates, Some(DuplicateOptions::default()));
    assert!(coomer.kemono_selection().original);
    let inherited = config.resolve("instagram").unwrap();
    assert_eq!(
        inherited.files,
        Some(vec![FileKind::File, FileKind::Inline])
    );
    assert_eq!(inherited.duplicates, Some(DuplicateOptions::all()));
    assert_eq!(
        inherited
            .overlay(&Options {
                files: Some(vec![]),
                duplicates: Some(Default::default()),
                ..Default::default()
            })
            .files,
        Some(vec![])
    );
    let root =
        parse(r#"{"files":[],"duplicates":"","extractor":{"files":"file","duplicates":true}}"#)
            .unwrap()
            .resolve("kemono")
            .unwrap();
    assert_eq!(root.files, Some(vec![]));
    assert_eq!(root.duplicates, Some(DuplicateOptions::default()));
    let defaults = Options::default();
    assert!(!defaults.kemono_selection().original);
    assert!(defaults.tumblr_selection().original);
}

#[test]
fn kemono_file_selection_rejects_unknown_names_and_types_privately() {
    use serde_json::json;
    for (field, values) in [
        (
            "files",
            vec![
                json!(null),
                json!(false),
                json!(1),
                json!(""),
                json!("attachment"),
                json!("PRIVATE_VALUE"),
                json!(["file", 1]),
            ],
        ),
        (
            "duplicates",
            vec![
                json!(null),
                json!(1),
                json!("attachments"),
                json!("PRIVATE_VALUE"),
                json!(["inline", false]),
            ],
        ),
    ] {
        for value in values {
            for document in [
                json!({field:value}),
                json!({"extractor":{field:value}}),
                json!({"extractor":{"kemono":{field:value}}}),
            ] {
                let error = parse(&document.to_string()).unwrap_err().to_string();
                assert!(error.contains(field), "{error}");
                assert!(!error.contains("PRIVATE_VALUE"), "{error}");
            }
        }
    }
    assert!(parse(&json!({"files":vec!["file";65]}).to_string()).is_err());
    assert!(parse(&json!({"duplicates":vec!["file";65]}).to_string()).is_err());
}

#[test]
fn kemono_revision_modes_and_orders_inherit_without_losing_explicit_false() {
    use gallery_dl_rs::kemono::{RevisionMode, RevisionOrder};
    let cfg=parse(r#"{"extractor":{"revisions":true,"order-revisions":"reverse","kemono":{"revisions":"unique","order-revisions":"desc"},"coomer":{"revisions":false}}}"#).unwrap();
    let kemono = cfg.resolve("kemono").unwrap();
    assert_eq!(
        (kemono.revisions, kemono.order_revisions),
        (Some(RevisionMode::Unique), Some(RevisionOrder::Newest))
    );
    let coomer = cfg.resolve("coomer").unwrap();
    assert_eq!(
        (coomer.revisions, coomer.order_revisions),
        (Some(RevisionMode::None), Some(RevisionOrder::Oldest))
    );
    assert_eq!(
        kemono
            .overlay(&Options {
                revisions: Some(RevisionMode::None),
                ..Default::default()
            })
            .revisions,
        Some(RevisionMode::None)
    );
    let root=parse(r#"{"revisions":false,"order-revisions":"asc","extractor":{"revisions":true,"order-revisions":"desc","kemono":{"revisions":"unique"}}}"#).unwrap().resolve("kemono").unwrap();
    assert_eq!(
        (root.revisions, root.order_revisions),
        (Some(RevisionMode::None), Some(RevisionOrder::Oldest))
    );
    let empty = parse("{}").unwrap().resolve("kemono").unwrap();
    assert_eq!((empty.revisions, empty.order_revisions), (None, None));
}

#[test]
fn kemono_revision_settings_reject_unknown_types_and_values_privately() {
    use serde_json::json;
    for (field, values) in [
        (
            "revisions",
            vec![
                json!(null),
                json!(1),
                json!([]),
                json!("true"),
                json!("PRIVATE_VALUE"),
            ],
        ),
        (
            "order-revisions",
            vec![
                json!(null),
                json!(true),
                json!(1),
                json!([]),
                json!("PRIVATE_VALUE"),
            ],
        ),
    ] {
        for value in values {
            for doc in [
                json!({field:value}),
                json!({"extractor":{field:value}}),
                json!({"extractor":{"kemono":{field:value}}}),
                json!({"extractor":{"coomer":{field:value}}}),
            ] {
                let error = parse(&doc.to_string()).unwrap_err().to_string();
                assert!(error.contains(field), "{error}");
                assert!(!error.contains("PRIVATE_VALUE"), "{error}");
            }
        }
    }
}

#[test]
fn kemono_metadata_option_is_sparse_and_scoped_with_explicit_false_overlays() {
    let cfg = parse(r#"{"extractor":{"metadata":false,"kemono":{"metadata":true}}}"#).unwrap();
    assert_eq!(cfg.resolve("kemono").unwrap().metadata, Some(true));
    assert_eq!(cfg.resolve("coomer").unwrap().metadata, Some(false));
    assert_eq!(
        cfg.resolve("kemono")
            .unwrap()
            .overlay(&Options {
                metadata: Some(false),
                ..Default::default()
            })
            .metadata,
        Some(false)
    );
    assert_eq!(
        parse(r#"{"metadata":false,"extractor":{"metadata":true,"kemono":{"metadata":true}}}"#)
            .unwrap()
            .resolve("kemono")
            .unwrap()
            .metadata,
        Some(false)
    );
    assert_eq!(
        parse("{}").unwrap().resolve("kemono").unwrap().metadata,
        None
    );
    for (value, expected) in [
        (serde_json::json!(null), false),
        (serde_json::json!(1), true),
        (serde_json::json!("PRIVATE_VALUE"), true),
        (serde_json::json!([]), false),
    ] {
        for doc in [
            serde_json::json!({"metadata":value}),
            serde_json::json!({"extractor":{"kemono":{"metadata":value}}}),
        ] {
            let resolved = parse(&doc.to_string()).unwrap().resolve("kemono").unwrap();
            assert_eq!(resolved.metadata, Some(expected));
            assert!(!format!("{resolved:?}").contains("PRIVATE_VALUE"));
        }
    }
}

#[test]
fn kemono_endpoint_and_discord_post_order_are_scoped_sparse_and_strict() {
    use gallery_dl_rs::kemono::{Endpoint, PostOrder};
    let cfg = parse(r#"{"extractor":{"endpoint":"legacy+","order-posts":"reverse","kemono":{"endpoint":"posts","order-posts":"asc"}}}"#).unwrap();
    let kemono = cfg.resolve("kemono").unwrap();
    assert_eq!(
        (kemono.endpoint, kemono.order_posts),
        (Some(Endpoint::Posts), Some(PostOrder::Forward))
    );
    let coomer = cfg.resolve("coomer").unwrap();
    assert_eq!(
        (coomer.endpoint, coomer.order_posts),
        (Some(Endpoint::Expanded), Some(PostOrder::Reverse))
    );
    let overlaid = coomer.overlay(&Options {
        endpoint: Some(Endpoint::Posts),
        order_posts: Some(PostOrder::Forward),
        ..Default::default()
    });
    assert_eq!(
        (overlaid.endpoint, overlaid.order_posts),
        (kemono.endpoint, kemono.order_posts)
    );
    let cfg = parse(r#"{"endpoint":"posts+","order-posts":"desc","extractor":{"kemono":{"endpoint":"legacy","order-posts":"asc"}}}"#).unwrap();
    let resolved = cfg.resolve("kemono").unwrap();
    assert_eq!(
        (resolved.endpoint, resolved.order_posts),
        (Some(Endpoint::Expanded), Some(PostOrder::Reverse))
    );
    let empty = parse("{}").unwrap().resolve("kemono").unwrap();
    assert_eq!((empty.endpoint, empty.order_posts), (None, None));
    for field in ["endpoint", "order-posts"] {
        for value in [
            serde_json::json!(null),
            serde_json::json!(true),
            serde_json::json!(1),
            serde_json::json!([]),
            serde_json::json!("PRIVATE_VALUE"),
        ] {
            for doc in [
                serde_json::json!({field:value}),
                serde_json::json!({"extractor":{field:value}}),
                serde_json::json!({"extractor":{"kemono":{field:value}}}),
            ] {
                let error = parse(&doc.to_string()).unwrap_err().to_string();
                assert!(
                    error.contains(field) && !error.contains("PRIVATE_VALUE"),
                    "{error}"
                );
            }
        }
    }
}

#[test]
fn kemono_favorites_are_sparse_scoped_and_validated_privately() {
    use gallery_dl_rs::kemono::Favorites;
    let cfg =
        parse(r#"{"extractor":{"favorites":"post","kemono":{"favorites":"artist"}}}"#).unwrap();
    assert_eq!(
        cfg.resolve("kemono").unwrap().favorites,
        Some(Favorites::Artist)
    );
    assert_eq!(
        cfg.resolve("coomer").unwrap().favorites,
        Some(Favorites::Post)
    );
    let cfg =
        parse(r#"{"favorites":"post","extractor":{"kemono":{"favorites":"artist"}}}"#).unwrap();
    assert_eq!(
        cfg.resolve("kemono").unwrap().favorites,
        Some(Favorites::Post)
    );
    assert_eq!(
        cfg.resolve("kemono")
            .unwrap()
            .overlay(&Options {
                favorites: Some(Favorites::Artist),
                ..Default::default()
            })
            .favorites,
        Some(Favorites::Artist)
    );
    assert_eq!(
        parse("{}").unwrap().resolve("kemono").unwrap().favorites,
        None
    );
    for value in [
        serde_json::json!(null),
        serde_json::json!(true),
        serde_json::json!(1),
        serde_json::json!([]),
        serde_json::json!("PRIVATE_VALUE"),
    ] {
        for doc in [
            serde_json::json!({"favorites":value}),
            serde_json::json!({"extractor":{"favorites":value}}),
            serde_json::json!({"extractor":{"coomer":{"favorites":value}}}),
        ] {
            let error = parse(&doc.to_string()).unwrap_err().to_string();
            assert!(
                error.contains("favorites") && !error.contains("PRIVATE_VALUE"),
                "{error}"
            );
        }
    }
}

#[test]
fn child_selection_is_scoped_sparse_and_explicitly_clearable() {
    let cfg=parse(r#"{"extractor":{"child-range":["2","4"],"child-filter":["service == 'patreon'"],"kemono":{"child-range":"1","child-filter":[]}}}"#).unwrap();
    let kemono = cfg.resolve("kemono").unwrap();
    assert_eq!(kemono.child_range.as_deref(), Some("1"));
    assert_eq!(kemono.child_filter, Some(vec![]));
    let coomer = cfg.resolve("coomer").unwrap();
    assert_eq!(coomer.child_range.as_deref(), Some("2,4"));
    assert_eq!(
        coomer.child_filter,
        Some(vec!["service == 'patreon'".into()])
    );
    let overlaid = coomer.overlay(&Options {
        child_range: Some("3".into()),
        child_filter: Some(vec![]),
        ..Default::default()
    });
    assert_eq!(overlaid.child_range.as_deref(), Some("3"));
    assert_eq!(overlaid.child_filter, Some(vec![]));
    let empty = parse("{}").unwrap().resolve("kemono").unwrap();
    assert!(empty.child_range.is_none() && empty.child_filter.is_none());
    assert_eq!(
        parse(r#"{"child-range":"5","extractor":{"kemono":{"child-range":"1"}}}"#)
            .unwrap()
            .resolve("kemono")
            .unwrap()
            .child_range
            .as_deref(),
        Some("5")
    );
    for (field, values) in [
        (
            "child-range",
            vec![
                serde_json::json!(null),
                serde_json::json!(true),
                serde_json::json!({}),
                serde_json::json!("PRIVATE_VALUE"),
            ],
        ),
        (
            "child-filter",
            vec![
                serde_json::json!(null),
                serde_json::json!(true),
                serde_json::json!([1]),
                serde_json::json!({"private":"PRIVATE_VALUE"}),
            ],
        ),
    ] {
        for value in values {
            let doc = serde_json::json!({"extractor":{"kemono":{field:value}}});
            let error = parse(&doc.to_string()).unwrap_err().to_string();
            assert!(
                error.contains(field) && !error.contains("PRIVATE_VALUE"),
                "{error}"
            );
        }
    }
}

#[test]
fn include_configuration_is_sparse_ordered_and_explicitly_clearable() {
    use gallery_dl_rs::dispatch::Include;
    use serde_json::json;
    assert_eq!(
        parse("{}").unwrap().resolve("twitter").unwrap().include,
        None
    );
    let cfg = parse(
        &json!({"extractor":{"include":" media, replies,media ",
        "pixiv":{"include":"all"},"twitter":{"include":[]},"bluesky":{"include":null}}})
        .to_string(),
    )
    .unwrap();
    assert_eq!(cfg.resolve("pixiv").unwrap().include, Some(Include::All));
    for category in ["twitter", "bluesky"] {
        assert_eq!(
            cfg.resolve(category).unwrap().include,
            Some(Include::Views(vec![]))
        );
    }
    assert_eq!(
        cfg.resolve("tumblr").unwrap().include,
        Some(Include::Views(vec![
            "media".into(),
            "replies".into(),
            "media".into()
        ]))
    );
    let merged = cfg.resolve("pixiv").unwrap().overlay(&Options {
        include: Some(Include::Views(vec![])),
        ..Default::default()
    });
    assert_eq!(merged.include, Some(Include::Views(vec![])));
    let cfg = parse(
        &json!({"include":"all","extractor":{"include":false,"pixiv":{"include":[]}}}).to_string(),
    )
    .unwrap();
    assert_eq!(cfg.resolve("pixiv").unwrap().include, Some(Include::All));
    for value in [
        json!(true),
        json!(3),
        json!(["media", null]),
        json!({"private":"include-secret"}),
    ] {
        let error = parse(&json!({"extractor":{"include":value}}).to_string())
            .unwrap_err()
            .to_string();
        assert!(!error.contains("include-secret"));
        assert!(error.contains("include"));
    }
}

#[test]
fn bluesky_likes_endpoint_and_api_server_are_scoped_sparse_and_private() {
    use gallery_dl_rs::bluesky::LikesEndpoint;
    use serde_json::json;
    let cfg=parse(&json!({"extractor":{"endpoint":"posts+","bluesky":{"endpoint":"getActorLikes","api-server":"https://appview.example/base/"}}}).to_string()).unwrap();
    let blue = cfg.resolve("bluesky").unwrap();
    assert_eq!(blue.bluesky_endpoint, Some(LikesEndpoint::ActorLikes));
    assert_eq!(
        blue.api_server.as_deref(),
        Some("https://appview.example/base/")
    );
    let kemono = cfg.resolve("kemono").unwrap();
    assert!(kemono.bluesky_endpoint.is_none());
    assert!(kemono.api_server.is_none());
    assert_eq!(
        kemono.endpoint,
        Some(gallery_dl_rs::kemono::Endpoint::Expanded)
    );
    let cleared = blue.overlay(&Options {
        api_server: Some(String::new()),
        bluesky_endpoint: Some(LikesEndpoint::Records),
        ..Default::default()
    });
    assert_eq!(cleared.api_server.as_deref(), Some(""));
    assert_eq!(cleared.bluesky_endpoint, Some(LikesEndpoint::Records));
    for value in [json!(""), json!(false), json!(null)] {
        let cfg = parse(
            &json!({"extractor":{"bluesky":{"api-server":value,"endpoint":null}}}).to_string(),
        )
        .unwrap();
        assert_eq!(
            cfg.resolve("bluesky").unwrap().api_server.as_deref(),
            Some("")
        );
        assert_eq!(
            cfg.resolve("bluesky").unwrap().bluesky_endpoint,
            Some(LikesEndpoint::Records)
        );
    }
    let cfg = parse(r#"{"extractor":{"bluesky":{"endpoint":"other"}}}"#).unwrap();
    assert_eq!(
        cfg.resolve("bluesky").unwrap().bluesky_endpoint,
        Some(LikesEndpoint::Records)
    );
    for value in [
        json!(true),
        json!(7),
        json!("https://private:secret@appview.example/"),
        json!("https://appview.example/?token=secret"),
        json!("https://appview.example/#secret"),
        json!("file:///private/secret"),
        json!("https://appview.example/\nsecret"),
    ] {
        let error = parse(&json!({"extractor":{"bluesky":{"api-server":value}}}).to_string())
            .unwrap_err()
            .to_string();
        assert!(!error.contains("secret"));
        assert!(error.contains("api-server"));
    }
    for document in [
        json!({"api-server":"https://private.example/"}),
        json!({"extractor":{"api-server":"https://private.example/"}}),
        json!({"extractor":{"twitter":{"api-server":"https://private.example/"}}}),
    ] {
        assert!(parse(&document.to_string()).is_err());
    }
}

#[test]
fn pixiv_tag_modes_are_sparse_inherited_and_independent_between_categories() {
    use gallery_dl_rs::pixiv::TagMode;
    assert_eq!(parse("{}").unwrap().resolve("pixiv").unwrap().tags, None);
    assert_eq!(
        parse("{}").unwrap().resolve("pixiv-novel").unwrap().tags,
        None
    );
    let cfg = parse(r#"{"extractor":{"tags":"translated","pixiv":{"tags":"original"},"pixiv-novel":{"tags":"japanese"}}}"#).unwrap();
    assert_eq!(cfg.resolve("pixiv").unwrap().tags, Some(TagMode::Original));
    assert_eq!(
        cfg.resolve("pixiv-novel").unwrap().tags,
        Some(TagMode::Japanese)
    );
    assert_eq!(
        cfg.resolve("direct").unwrap().tags,
        Some(TagMode::Translated)
    );
    let cfg = parse(r#"{"extractor":{"tags":"translated","pixiv":{"tags":"original"}}}"#).unwrap();
    assert_eq!(
        cfg.resolve("pixiv-novel").unwrap().tags,
        Some(TagMode::Translated)
    );
    let cfg = parse(r#"{"tags":"original","extractor":{"tags":"japanese","pixiv":{"tags":"translated"},"pixiv-novel":{"tags":"translated"}}}"#).unwrap();
    for category in ["pixiv", "pixiv-novel"] {
        let options = cfg.resolve(category).unwrap();
        assert_eq!(options.tags, Some(TagMode::Original));
        assert_eq!(
            options
                .overlay(&Options {
                    tags: Some(TagMode::Japanese),
                    ..Options::default()
                })
                .tags,
            Some(TagMode::Japanese)
        );
        assert_eq!(
            options.overlay(&Options::default()).tags,
            Some(TagMode::Original)
        );
    }
}

#[test]
fn pixiv_tag_modes_use_upstream_fallback_for_every_other_present_value() {
    use gallery_dl_rs::pixiv::TagMode;
    use serde_json::json;
    for value in [
        json!("japanese"),
        json!("unknown-private-value"),
        json!("ORIGINAL"),
        json!("Translated"),
        json!(" original"),
        json!(""),
        json!(null),
        json!(true),
        json!(false),
        json!(0),
        json!(1.5),
        json!([]),
        json!(["private-payload"]),
        json!({"private":"payload"}),
    ] {
        for category in ["pixiv", "pixiv-novel"] {
            let cfg = parse(
                &json!({"extractor":{"tags":"translated", category: {"tags":value}}}).to_string(),
            )
            .unwrap();
            let resolved = cfg.resolve(category).unwrap();
            assert_eq!(resolved.tags, Some(TagMode::Japanese));
            let debug = format!("{resolved:?}");
            assert!(!debug.contains("private"));
            assert!(!debug.contains("payload"));
        }
        for document in [json!({"tags":value}), json!({"extractor":{"tags":value}})] {
            let cfg = parse(&document.to_string()).unwrap();
            assert_eq!(cfg.resolve("pixiv").unwrap().tags, Some(TagMode::Japanese));
            assert_eq!(
                cfg.resolve("pixiv-novel").unwrap().tags,
                Some(TagMode::Japanese)
            );
        }
    }
    let cfg = parse(r#"{"extractor":{"pixiv":{"tags":"translated","ugoira":false,"max-posts":3},"pixiv-novel":{"tags":"original","covers":true}}}"#).unwrap();
    let pixiv = cfg.resolve("pixiv").unwrap();
    assert_eq!(pixiv.tags, Some(TagMode::Translated));
    assert_eq!(pixiv.ugoira, Some(gallery_dl_rs::pixiv::UgoiraMode::Skip));
    assert_eq!(pixiv.max_posts, Some(3));
    let novels = cfg.resolve("pixiv-novel").unwrap();
    assert_eq!(novels.tags, Some(TagMode::Original));
    assert_eq!(novels.covers, Some(true));
    assert_eq!(novels.ugoira, None);
    assert_eq!(cfg.resolve("direct").unwrap(), Options::default());
}

#[test]
fn pixiv_bookmark_cursors_preserve_scalar_types_and_clear_inherited_settings() {
    use gallery_dl_rs::pixiv::BookmarkCursor;
    use serde_json::json;
    let cursor = |value: serde_json::Value| BookmarkCursor::parse(&value).unwrap();
    let cfg = parse(
        r#"{"extractor":{"cursor":42,"pixiv":{"cursor":"0"},"pixiv-novel":{"cursor":null}}}"#,
    )
    .unwrap();
    assert_eq!(
        cfg.resolve("pixiv").unwrap().cursor,
        Some(cursor(json!("0")))
    );
    assert_eq!(
        cfg.resolve("pixiv-novel").unwrap().cursor,
        Some(cursor(json!(null)))
    );
    assert_eq!(
        cfg.resolve("direct").unwrap().cursor,
        Some(cursor(json!(42)))
    );
    let cfg = parse(r#"{"cursor":"root","extractor":{"cursor":-1,"pixiv":{"cursor":"art"},"pixiv-novel":{"cursor":"novel"}}}"#).unwrap();
    for category in ["pixiv", "pixiv-novel"] {
        let options = cfg.resolve(category).unwrap();
        assert_eq!(options.cursor, Some(cursor(json!("root"))));
        let cleared = options.overlay(&Options {
            cursor: Some(cursor(json!(null))),
            ..Options::default()
        });
        assert_eq!(cleared.cursor.as_ref().unwrap().query_value(), None);
        let blank = cleared.overlay(&Options {
            cursor: Some(cursor(json!(""))),
            ..Options::default()
        });
        assert_eq!(blank.cursor.as_ref().unwrap().query_value(), Some(""));
    }
    for value in [
        json!(null),
        json!(0),
        json!(-15),
        json!(""),
        json!("猫 \n"),
        json!("a".repeat(1024)),
    ] {
        for document in [
            json!({"cursor":value}),
            json!({"extractor":{"cursor":value}}),
            json!({"extractor":{"pixiv":{"cursor":value}}}),
        ] {
            let cfg = parse(&document.to_string()).unwrap();
            assert_eq!(
                cfg.resolve("pixiv")
                    .unwrap()
                    .cursor
                    .as_ref()
                    .unwrap()
                    .as_json(),
                &value
            );
        }
    }
    let big = "9".repeat(1024);
    let cfg = parse(&format!(
        r#"{{"extractor":{{"pixiv-novel":{{"cursor":{big}}}}}}}"#
    ))
    .unwrap();
    assert_eq!(
        cfg.resolve("pixiv-novel")
            .unwrap()
            .cursor
            .unwrap()
            .query_value(),
        Some(big.as_str())
    );
    assert_eq!(parse("{}").unwrap().resolve("pixiv").unwrap().cursor, None);
}

#[test]
fn pixiv_bookmark_cursor_invalid_types_and_bounds_fail_privately_in_every_scope() {
    use serde_json::json;
    for value in [
        json!(true),
        json!(false),
        json!(1.5),
        json!([]),
        json!(["cursor-private-payload"]),
        json!({"private":"cursor-private-payload"}),
        json!("private-payload".repeat(100)),
    ] {
        for document in [
            json!({"cursor":value}),
            json!({"extractor":{"cursor":value}}),
            json!({"extractor":{"pixiv":{"cursor":value}}}),
            json!({"extractor":{"pixiv-novel":{"cursor":value}}}),
        ] {
            let message = parse(&document.to_string()).unwrap_err().to_string();
            assert!(message.contains("cursor"));
            assert!(!message.contains("private"));
            assert!(!message.contains("payload"));
        }
    }
    for value in ["1e10", "1.0", &"9".repeat(1025)] {
        assert!(parse(&format!(r#"{{"cursor":{value}}}"#)).is_err());
    }
}

#[test]
fn pixiv_ugoira_modes_preserve_python_truthiness_and_discard_private_payloads() {
    use gallery_dl_rs::pixiv::UgoiraMode;
    use serde_json::json;
    assert_eq!(parse("{}").unwrap().resolve("pixiv").unwrap().ugoira, None);
    for (value, expected) in [
        (json!(null), UgoiraMode::Skip),
        (json!(false), UgoiraMode::Skip),
        (json!(0), UgoiraMode::Skip),
        (json!(0.0), UgoiraMode::Skip),
        (json!(""), UgoiraMode::Skip),
        (json!([]), UgoiraMode::Skip),
        (json!({}), UgoiraMode::Skip),
        (json!(true), UgoiraMode::Zip),
        (json!(1), UgoiraMode::Zip),
        (json!(-2), UgoiraMode::Zip),
        (json!(0.5), UgoiraMode::Zip),
        (json!("original"), UgoiraMode::Original),
        (json!("ORIGINAL"), UgoiraMode::Zip),
        (json!("Original"), UgoiraMode::Zip),
        (json!("false"), UgoiraMode::Zip),
        (json!(" "), UgoiraMode::Zip),
        (json!("private-payload"), UgoiraMode::Zip),
        (json!([false]), UgoiraMode::Zip),
        (json!({"private":"payload"}), UgoiraMode::Zip),
    ] {
        for document in [
            json!({"ugoira":value}),
            json!({"extractor":{"ugoira":value}}),
            json!({"extractor":{"pixiv":{"ugoira":value}}}),
            json!({"extractor":{"pixiv-novel":{"ugoira":value}}}),
        ] {
            let config = parse(&document.to_string()).unwrap();
            let category = if document["extractor"].get("pixiv-novel").is_some() {
                "pixiv-novel"
            } else {
                "pixiv"
            };
            let resolved = config.resolve(category).unwrap();
            assert_eq!(resolved.ugoira, Some(expected));
            let debug = format!("{resolved:?}");
            assert!(!debug.contains("private"));
            assert!(!debug.contains("payload"));
        }
    }
}

#[test]
fn pixiv_ugoira_modes_override_original_with_false_and_null_without_scope_leaks() {
    use gallery_dl_rs::pixiv::UgoiraMode;
    let cfg = parse(r#"{"extractor":{"ugoira":"original","pixiv":{"ugoira":false},"pixiv-novel":{"ugoira":null}}}"#).unwrap();
    assert_eq!(cfg.resolve("pixiv").unwrap().ugoira, Some(UgoiraMode::Skip));
    assert_eq!(
        cfg.resolve("pixiv-novel").unwrap().ugoira,
        Some(UgoiraMode::Skip)
    );
    assert_eq!(
        cfg.resolve("direct").unwrap().ugoira,
        Some(UgoiraMode::Original)
    );
    let cfg = parse(r#"{"extractor":{"ugoira":true,"pixiv":{"ugoira":"original"}}}"#).unwrap();
    let pixiv = cfg.resolve("pixiv").unwrap();
    assert_eq!(pixiv.ugoira, Some(UgoiraMode::Original));
    assert_eq!(
        cfg.resolve("pixiv-novel").unwrap().ugoira,
        Some(UgoiraMode::Zip)
    );
    assert_eq!(
        pixiv
            .overlay(&Options {
                ugoira: Some(UgoiraMode::Skip),
                ..Options::default()
            })
            .ugoira,
        Some(UgoiraMode::Skip)
    );
    assert_eq!(
        pixiv.overlay(&Options::default()).ugoira,
        Some(UgoiraMode::Original)
    );
    let cfg = parse(r#"{"ugoira":null,"extractor":{"ugoira":"original","pixiv":{"ugoira":true}}}"#)
        .unwrap();
    assert_eq!(cfg.resolve("pixiv").unwrap().ugoira, Some(UgoiraMode::Skip));
    let cfg =
        parse(r#"{"ugoira":"original","extractor":{"ugoira":false,"pixiv":{"ugoira":null}}}"#)
            .unwrap();
    assert_eq!(
        cfg.resolve("pixiv").unwrap().ugoira,
        Some(UgoiraMode::Original)
    );
}

#[test]
fn pixiv_enrichment_switches_accept_truthiness_and_discard_private_values() {
    use serde_json::json;
    let sparse = parse("{}").unwrap().resolve("pixiv").unwrap();
    assert_eq!(sparse.comments, None);
    assert_eq!(sparse.metadata_bookmark, None);
    for (value, expected) in [
        (json!(null), false),
        (json!(false), false),
        (json!(0), false),
        (json!(0.0), false),
        (json!(""), false),
        (json!([]), false),
        (json!({}), false),
        (json!(true), true),
        (json!(1), true),
        (json!(-1), true),
        (json!(0.5), true),
        (json!("false"), true),
        (json!("private-payload"), true),
        (json!([false]), true),
        (json!({"private":"payload"}), true),
    ] {
        for document in [
            json!({"comments":value,"metadata-bookmark":value}),
            json!({"extractor":{"comments":value,"metadata-bookmark":value}}),
            json!({"extractor":{"pixiv":{"comments":value,"metadata-bookmark":value}}}),
        ] {
            let resolved = parse(&document.to_string())
                .unwrap()
                .resolve("pixiv")
                .unwrap();
            assert_eq!(resolved.comments, Some(expected));
            assert_eq!(resolved.metadata_bookmark, Some(expected));
            assert!(!format!("{resolved:?}").contains("private"));
            assert!(!format!("{resolved:?}").contains("payload"));
        }
    }
}

#[test]
fn pixiv_enrichment_switches_are_scoped_and_explicit_null_clears_defaults() {
    let cfg = parse(r#"{"extractor":{"comments":true,"metadata-bookmark":true,"pixiv":{"comments":null,"metadata-bookmark":false},"pixiv-novel":{"comments":false}}}"#).unwrap();
    let pixiv = cfg.resolve("pixiv").unwrap();
    assert_eq!(pixiv.comments, Some(false));
    assert_eq!(pixiv.metadata_bookmark, Some(false));
    let novel = cfg.resolve("pixiv-novel").unwrap();
    assert_eq!(novel.comments, Some(false));
    assert_eq!(novel.metadata_bookmark, Some(true));
    let generic = cfg.resolve("direct").unwrap();
    assert_eq!(generic.comments, Some(true));
    assert_eq!(generic.metadata_bookmark, Some(true));
    let cfg = parse(r#"{"comments":null,"metadata-bookmark":false,"extractor":{"comments":true,"metadata-bookmark":true,"pixiv":{"comments":true,"metadata-bookmark":true}}}"#).unwrap();
    let resolved = cfg.resolve("pixiv").unwrap();
    assert_eq!(resolved.comments, Some(false));
    assert_eq!(resolved.metadata_bookmark, Some(false));
    let overridden = resolved.overlay(&Options {
        comments: Some(true),
        metadata_bookmark: Some(true),
        ..Options::default()
    });
    assert_eq!(overridden.comments, Some(true));
    assert_eq!(overridden.metadata_bookmark, Some(true));
    assert_eq!(overridden.overlay(&Options::default()), overridden);
    for (document, expected) in [
        (r#"{"metadata":null}"#, false),
        (r#"{"extractor":{"metadata":"private-payload"}}"#, true),
    ] {
        let resolved = parse(document).unwrap().resolve("pixiv").unwrap();
        assert_eq!(resolved.metadata, Some(expected));
        assert!(!format!("{resolved:?}").contains("private-payload"));
    }
    let cfg =
        parse(r#"{"extractor":{"pixiv":{"comments":true,"metadata-bookmark":true}}}"#).unwrap();
    assert_eq!(cfg.resolve("pixiv-novel").unwrap().comments, None);
    assert_eq!(cfg.resolve("pixiv-novel").unwrap().metadata_bookmark, None);
    assert_eq!(cfg.resolve("direct").unwrap(), Options::default());
}

#[test]
fn metadata_switch_matches_source_truthiness_across_pixiv_and_kemono_scopes() {
    use serde_json::json;
    for (value, expected) in [
        (json!(null), false),
        (json!(false), false),
        (json!(0), false),
        (json!(-0.0), false),
        (json!(""), false),
        (json!([]), false),
        (json!({}), false),
        (json!(true), true),
        (json!(1), true),
        (json!(-1), true),
        (json!(0.5), true),
        (json!("false"), true),
        (json!("private-payload"), true),
        (json!([false]), true),
        (json!({"private":"payload"}), true),
    ] {
        for category in ["pixiv", "pixiv-novel", "kemono", "coomer"] {
            for document in [
                json!({"metadata":value}),
                json!({"extractor":{"metadata":value}}),
                json!({"extractor":{category:{"metadata":value}}}),
            ] {
                let resolved = parse(&document.to_string())
                    .unwrap()
                    .resolve(category)
                    .unwrap();
                assert_eq!(resolved.metadata, Some(expected));
                assert!(!format!("{resolved:?}").contains("private"));
                assert!(!format!("{resolved:?}").contains("payload"));
            }
        }
    }
}

#[test]
fn metadata_switch_preserves_independent_pixiv_novel_scopes_and_explicit_null_clearing() {
    let cfg = parse(r#"{"extractor":{"metadata":true,"pixiv":{"metadata":null},"pixiv-novel":{"metadata":"false"},"kemono":{"metadata":[]}}}"#).unwrap();
    assert_eq!(cfg.resolve("pixiv").unwrap().metadata, Some(false));
    assert_eq!(cfg.resolve("pixiv-novel").unwrap().metadata, Some(true));
    assert_eq!(cfg.resolve("kemono").unwrap().metadata, Some(false));
    assert_eq!(cfg.resolve("coomer").unwrap().metadata, Some(true));
    let cfg = parse(r#"{"extractor":{"pixiv":{"metadata":true}}}"#).unwrap();
    assert_eq!(cfg.resolve("pixiv-novel").unwrap().metadata, None);
    let cfg = parse(r#"{"metadata":null,"extractor":{"metadata":true,"pixiv":{"metadata":true},"pixiv-novel":{"metadata":true}}}"#).unwrap();
    for category in ["pixiv", "pixiv-novel", "kemono"] {
        let resolved = cfg.resolve(category).unwrap();
        assert_eq!(resolved.metadata, Some(false));
        assert_eq!(
            resolved
                .overlay(&Options {
                    metadata: Some(true),
                    ..Options::default()
                })
                .metadata,
            Some(true)
        );
        assert_eq!(resolved.overlay(&Options::default()).metadata, Some(false));
    }
    for category in ["pixiv", "pixiv-novel", "kemono", "coomer"] {
        assert_eq!(
            parse("{}").unwrap().resolve(category).unwrap().metadata,
            None
        );
    }
}

#[test]
fn pixiv_captions_are_sparse_truthy_and_discard_private_configuration_values() {
    use serde_json::json;
    assert_eq!(
        parse("{}").unwrap().resolve("pixiv").unwrap().captions,
        None
    );
    for (value, expected) in [
        (json!(null), false),
        (json!(false), false),
        (json!(0), false),
        (json!(-0.0), false),
        (json!(""), false),
        (json!([]), false),
        (json!({}), false),
        (json!(true), true),
        (json!(1), true),
        (json!(-1), true),
        (json!(0.5), true),
        (json!("false"), true),
        (json!("private-payload"), true),
        (json!([false]), true),
        (json!({"private":"payload"}), true),
    ] {
        for document in [
            json!({"captions":value}),
            json!({"extractor":{"captions":value}}),
            json!({"extractor":{"pixiv":{"captions":value}}}),
        ] {
            let resolved = parse(&document.to_string())
                .unwrap()
                .resolve("pixiv")
                .unwrap();
            assert_eq!(resolved.captions, Some(expected));
            assert!(!format!("{resolved:?}").contains("private"));
            assert!(!format!("{resolved:?}").contains("payload"));
        }
    }
}

#[test]
fn pixiv_captions_obey_hierarchy_null_clearing_and_independent_novel_scopes() {
    let cfg = parse(r#"{"extractor":{"captions":true,"pixiv":{"captions":null},"pixiv-novel":{"captions":"false"}}}"#).unwrap();
    assert_eq!(cfg.resolve("pixiv").unwrap().captions, Some(false));
    assert_eq!(cfg.resolve("pixiv-novel").unwrap().captions, Some(true));
    assert_eq!(cfg.resolve("direct").unwrap().captions, Some(true));
    let cfg = parse(r#"{"captions":null,"extractor":{"captions":true,"pixiv":{"captions":true}}}"#)
        .unwrap();
    let resolved = cfg.resolve("pixiv").unwrap();
    assert_eq!(resolved.captions, Some(false));
    assert_eq!(
        resolved
            .overlay(&Options {
                captions: Some(true),
                ..Options::default()
            })
            .captions,
        Some(true)
    );
    assert_eq!(resolved.overlay(&Options::default()).captions, Some(false));
    let cfg =
        parse(r#"{"captions":"enabled","extractor":{"captions":false,"pixiv":{"captions":null}}}"#)
            .unwrap();
    assert_eq!(cfg.resolve("pixiv").unwrap().captions, Some(true));
    let cfg = parse(r#"{"extractor":{"pixiv":{"captions":true,"comments":false,"metadata":null,"metadata-bookmark":true}}}"#).unwrap();
    let pixiv = cfg.resolve("pixiv").unwrap();
    assert_eq!(pixiv.captions, Some(true));
    assert_eq!(pixiv.comments, Some(false));
    assert_eq!(pixiv.metadata, Some(false));
    assert_eq!(pixiv.metadata_bookmark, Some(true));
    assert_eq!(cfg.resolve("pixiv-novel").unwrap().captions, None);
    assert_eq!(cfg.resolve("direct").unwrap(), Options::default());
}

#[test]
fn pixiv_sanity_is_sparse_and_matches_source_truthiness_in_all_scopes() {
    use serde_json::json;
    assert_eq!(parse("{}").unwrap().resolve("pixiv").unwrap().sanity, None);
    for (value, expected) in [
        (json!(null), false),
        (json!(false), false),
        (json!(0), false),
        (json!(-0.0), false),
        (json!(""), false),
        (json!([]), false),
        (json!({}), false),
        (json!(true), true),
        (json!(1), true),
        (json!(-1), true),
        (json!(0.5), true),
        (json!("false"), true),
        (json!("private-payload"), true),
        (json!([false]), true),
        (json!({"private":"payload"}), true),
    ] {
        for document in [
            json!({"sanity":value}),
            json!({"extractor":{"sanity":value}}),
            json!({"extractor":{"pixiv":{"sanity":value}}}),
        ] {
            let resolved = parse(&document.to_string())
                .unwrap()
                .resolve("pixiv")
                .unwrap();
            assert_eq!(resolved.sanity, Some(expected));
            assert!(!format!("{resolved:?}").contains("private"));
            assert!(!format!("{resolved:?}").contains("payload"));
        }
    }
}

#[test]
fn pixiv_sanity_preserves_null_clearing_hierarchy_and_independent_categories() {
    let cfg = parse(
        r#"{"extractor":{"sanity":true,"pixiv":{"sanity":null},"pixiv-novel":{"sanity":"false"}}}"#,
    )
    .unwrap();
    assert_eq!(cfg.resolve("pixiv").unwrap().sanity, Some(false));
    assert_eq!(cfg.resolve("pixiv-novel").unwrap().sanity, Some(true));
    assert_eq!(cfg.resolve("direct").unwrap().sanity, Some(true));
    let cfg =
        parse(r#"{"sanity":null,"extractor":{"sanity":true,"pixiv":{"sanity":true}}}"#).unwrap();
    let resolved = cfg.resolve("pixiv").unwrap();
    assert_eq!(resolved.sanity, Some(false));
    assert_eq!(
        resolved
            .overlay(&Options {
                sanity: Some(true),
                ..Options::default()
            })
            .sanity,
        Some(true)
    );
    assert_eq!(resolved.overlay(&Options::default()).sanity, Some(false));
    let cfg =
        parse(r#"{"sanity":true,"extractor":{"sanity":false,"pixiv":{"sanity":null}}}"#).unwrap();
    assert_eq!(cfg.resolve("pixiv").unwrap().sanity, Some(true));
    let cfg = parse(r#"{"extractor":{"pixiv":{"sanity":false,"captions":true,"comments":false,"metadata":null,"metadata-bookmark":true}}}"#).unwrap();
    let pixiv = cfg.resolve("pixiv").unwrap();
    assert_eq!(pixiv.sanity, Some(false));
    assert_eq!(pixiv.captions, Some(true));
    assert_eq!(pixiv.comments, Some(false));
    assert_eq!(pixiv.metadata, Some(false));
    assert_eq!(pixiv.metadata_bookmark, Some(true));
    assert_eq!(cfg.resolve("pixiv-novel").unwrap().sanity, None);
    assert_eq!(cfg.resolve("direct").unwrap(), Options::default());
}

#[test]
fn pixiv_novel_embeds_are_sparse_truthy_and_discard_private_values() {
    use serde_json::json;
    assert_eq!(
        parse("{}").unwrap().resolve("pixiv-novel").unwrap().embeds,
        None
    );
    for (value, expected) in [
        (json!(null), false),
        (json!(false), false),
        (json!(0), false),
        (json!(-0.0), false),
        (json!(""), false),
        (json!([]), false),
        (json!({}), false),
        (json!(true), true),
        (json!(1), true),
        (json!(-1), true),
        (json!(0.5), true),
        (json!("false"), true),
        (json!("private-payload"), true),
        (json!([false]), true),
        (json!({"private":"payload"}), true),
    ] {
        for document in [
            json!({"embeds":value}),
            json!({"extractor":{"embeds":value}}),
            json!({"extractor":{"pixiv-novel":{"embeds":value}}}),
        ] {
            let resolved = parse(&document.to_string())
                .unwrap()
                .resolve("pixiv-novel")
                .unwrap();
            assert_eq!(resolved.embeds, Some(expected));
            assert!(!format!("{resolved:?}").contains("private"));
            assert!(!format!("{resolved:?}").contains("payload"));
        }
    }
}

#[test]
fn pixiv_novel_embeds_preserve_null_clearing_hierarchy_and_independent_categories() {
    let cfg = parse(
        r#"{"extractor":{"embeds":true,"pixiv-novel":{"embeds":null},"pixiv":{"embeds":"false"}}}"#,
    )
    .unwrap();
    assert_eq!(cfg.resolve("pixiv-novel").unwrap().embeds, Some(false));
    assert_eq!(cfg.resolve("pixiv").unwrap().embeds, Some(true));
    assert_eq!(cfg.resolve("direct").unwrap().embeds, Some(true));
    let cfg = parse(r#"{"embeds":null,"extractor":{"embeds":true,"pixiv-novel":{"embeds":true}}}"#)
        .unwrap();
    let resolved = cfg.resolve("pixiv-novel").unwrap();
    assert_eq!(resolved.embeds, Some(false));
    assert_eq!(
        resolved
            .overlay(&Options {
                embeds: Some(true),
                ..Options::default()
            })
            .embeds,
        Some(true)
    );
    assert_eq!(resolved.overlay(&Options::default()).embeds, Some(false));
    let cfg =
        parse(r#"{"embeds":"enabled","extractor":{"embeds":false,"pixiv-novel":{"embeds":null}}}"#)
            .unwrap();
    assert_eq!(cfg.resolve("pixiv-novel").unwrap().embeds, Some(true));
    let cfg = parse(r#"{"extractor":{"pixiv-novel":{"embeds":true,"captions":false,"comments":true,"metadata":null,"metadata-bookmark":false}}}"#).unwrap();
    let novel = cfg.resolve("pixiv-novel").unwrap();
    assert_eq!(novel.embeds, Some(true));
    assert_eq!(novel.captions, Some(false));
    assert_eq!(novel.comments, Some(true));
    assert_eq!(novel.metadata, Some(false));
    assert_eq!(novel.metadata_bookmark, Some(false));
    assert_eq!(cfg.resolve("pixiv").unwrap().embeds, None);
    assert_eq!(cfg.resolve("direct").unwrap(), Options::default());
}

#[test]
fn novel_cover_and_series_switches_match_source_truthiness_in_all_scopes() {
    use serde_json::json;
    for (value, expected) in [
        (json!(null), false),
        (json!(false), false),
        (json!(0), false),
        (json!(-0.0), false),
        (json!(""), false),
        (json!([]), false),
        (json!({}), false),
        (json!(true), true),
        (json!(1), true),
        (json!(-1), true),
        (json!(0.5), true),
        (json!("false"), true),
        (json!("private-payload"), true),
        (json!([false]), true),
        (json!({"private":"payload"}), true),
    ] {
        for document in [
            json!({"covers":value,"full-series":value}),
            json!({"extractor":{"covers":value,"full-series":value}}),
            json!({"extractor":{"pixiv-novel":{"covers":value,"full-series":value}}}),
        ] {
            let resolved = parse(&document.to_string())
                .unwrap()
                .resolve("pixiv-novel")
                .unwrap();
            assert_eq!(
                (resolved.covers, resolved.full_series),
                (Some(expected), Some(expected))
            );
            assert!(!format!("{resolved:?}").contains("private"));
            assert!(!format!("{resolved:?}").contains("payload"));
        }
    }
}

#[test]
fn novel_cover_and_series_switches_preserve_null_clearing_and_hierarchy() {
    let cfg = parse(r#"{"extractor":{"covers":true,"full-series":true,"pixiv-novel":{"covers":null,"full-series":false},"pixiv":{"covers":"false","full-series":[]}}}"#).unwrap();
    let novel = cfg.resolve("pixiv-novel").unwrap();
    assert_eq!(
        (novel.covers, novel.full_series),
        (Some(false), Some(false))
    );
    let art = cfg.resolve("pixiv").unwrap();
    assert_eq!((art.covers, art.full_series), (Some(true), Some(false)));
    let generic = cfg.resolve("direct").unwrap();
    assert_eq!(
        (generic.covers, generic.full_series),
        (Some(true), Some(true))
    );
    let cfg = parse(r#"{"covers":null,"full-series":null,"extractor":{"covers":true,"full-series":true,"pixiv-novel":{"covers":true,"full-series":true}}}"#).unwrap();
    let resolved = cfg.resolve("pixiv-novel").unwrap();
    assert_eq!(
        (resolved.covers, resolved.full_series),
        (Some(false), Some(false))
    );
    let overridden = resolved.overlay(&Options {
        covers: Some(true),
        full_series: Some(true),
        ..Options::default()
    });
    assert_eq!(
        (overridden.covers, overridden.full_series),
        (Some(true), Some(true))
    );
    assert_eq!(resolved.overlay(&Options::default()), resolved);
    let sparse = parse("{}").unwrap().resolve("pixiv-novel").unwrap();
    assert_eq!((sparse.covers, sparse.full_series), (None, None));
    let cfg = parse(r#"{"extractor":{"pixiv-novel":{"covers":true,"full-series":true}}}"#).unwrap();
    assert_eq!(cfg.resolve("pixiv").unwrap().covers, None);
    assert_eq!(cfg.resolve("pixiv").unwrap().full_series, None);
}

#[test]
fn booru_threshold_preserves_absence_integer_clamping_and_explicit_defaults() {
    assert_eq!(
        parse("{}")
            .unwrap()
            .resolve("danbooru")
            .unwrap()
            .booru_threshold,
        None
    );
    for (raw, expected) in [
        ("17", 17),
        ("1", 1),
        ("0", 1),
        ("-1", 1),
        ("-0", 1),
        ("-99999999999999999999999999999999999999999999", 1),
    ] {
        for document in [
            format!(r#"{{"threshold":{raw}}}"#),
            format!(r#"{{"extractor":{{"threshold":{raw}}}}}"#),
            [r#"{"extractor":{"danbooru":{"threshold":"#, raw, "}}}"].concat(),
            [r#"{"extractor":{"e621":{"threshold":"#, raw, "}}}"].concat(),
        ] {
            let config = parse(&document).unwrap();
            let category = if document.contains("e621") {
                "e621"
            } else {
                "danbooru"
            };
            assert_eq!(
                config.resolve(category).unwrap().booru_threshold,
                Some(Some(expected)),
                "{document}"
            );
        }
    }
    let max = usize::MAX;
    assert_eq!(
        parse(&format!(r#"{{"threshold":{max}}}"#))
            .unwrap()
            .resolve("danbooru")
            .unwrap()
            .booru_threshold,
        Some(Some(max))
    );
    for raw in [
        "null",
        "false",
        "true",
        "1.0",
        "1e0",
        "-1.0",
        "1e99999",
        r#""PRIVATE_VALUE""#,
        "[]",
        r#"{"PRIVATE_VALUE": 1}"#,
    ] {
        let config = parse(&format!(
            r#"{{"extractor":{{"threshold":23,"danbooru":{{"threshold":{raw}}}}}}}"#
        ))
        .unwrap();
        assert_eq!(
            config.resolve("danbooru").unwrap().booru_threshold,
            Some(None),
            "{raw}"
        );
        assert_eq!(
            config.resolve("e621").unwrap().booru_threshold,
            Some(Some(23)),
            "{raw}"
        );
    }
}

#[test]
fn booru_threshold_scope_precedence_and_reset_overlays_are_sparse() {
    let config = parse(
        r#"{"extractor":{"threshold":25,"danbooru":{"threshold":null},"e621":{"threshold":12}}}"#,
    )
    .unwrap();
    assert_eq!(
        config.resolve("danbooru").unwrap().booru_threshold,
        Some(None)
    );
    assert_eq!(
        config.resolve("e621").unwrap().booru_threshold,
        Some(Some(12))
    );
    let root =
        parse(r#"{"threshold":false,"extractor":{"threshold":25,"danbooru":{"threshold":12}}}"#)
            .unwrap();
    assert_eq!(
        root.resolve("danbooru").unwrap().booru_threshold,
        Some(None)
    );
    let root =
        parse(r#"{"threshold":7,"extractor":{"threshold":25,"danbooru":{"threshold":null}}}"#)
            .unwrap();
    assert_eq!(
        root.resolve("danbooru").unwrap().booru_threshold,
        Some(Some(7))
    );
    let options = Options {
        booru_threshold: Some(Some(23)),
        ..Default::default()
    };
    assert_eq!(
        options.overlay(&Options::default()).booru_threshold,
        Some(Some(23))
    );
    assert_eq!(
        options
            .overlay(&Options {
                booru_threshold: Some(None),
                ..Default::default()
            })
            .booru_threshold,
        Some(None)
    );
    assert_eq!(
        options
            .overlay(&Options {
                booru_threshold: Some(Some(1)),
                ..Default::default()
            })
            .booru_threshold,
        Some(Some(1))
    );
}

#[test]
fn booru_threshold_rejects_unsupported_categories_and_positive_overflow_privately() {
    for category in ["gelbooru", "safebooru", "twitter", "direct", "pixiv"] {
        let document = serde_json::json!({"extractor":{category:{"threshold":"PRIVATE_VALUE"}}});
        let failure = parse(&document.to_string()).unwrap_err().to_string();
        assert!(failure.contains("threshold"));
        assert!(!failure.contains("PRIVATE_VALUE"));
    }
    let raw = "99999999999999999999999999999999999999999999999999999999999999";
    for document in [
        format!(r#"{{"threshold":{raw}}}"#),
        format!(r#"{{"extractor":{{"threshold":{raw}}}}}"#),
        [r#"{"extractor":{"danbooru":{"threshold":"#, raw, "}}}"].concat(),
        [r#"{"extractor":{"e621":{"threshold":"#, raw, "}}}"].concat(),
    ] {
        let failure = parse(&document).unwrap_err().to_string();
        assert!(failure.contains("threshold"));
        assert!(!failure.contains(raw));
    }
    assert!(parse(r#"{"threshold":1,"threshold":2}"#).is_err());
}

#[cfg(unix)]
#[test]
fn booru_threshold_compatibility_forwarding_is_sparse_scoped_and_retains_default() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let backend = root.path().join("backend");
    let log = root.path().join("arguments");
    let config = root.path().join("native.json");
    std::fs::write(
        &backend,
        r##"#!/bin/sh
printf '%s\n' "$@" > "$BOORU_THRESHOLD_ARGUMENTS"
printf '%s\n' '[[3,"https://cdn.example/photo.jpg",{"filename":"photo","extension":"jpg"}]]'
"##,
    )
    .unwrap();
    std::fs::set_permissions(&backend, std::fs::Permissions::from_mode(0o700)).unwrap();
    for (document, expected) in [
        (serde_json::json!({}), None),
        (serde_json::json!({"threshold":11}), Some("threshold=11")),
        (
            serde_json::json!({"extractor":{"threshold":23,"danbooru":{"threshold":null},"e621":{"threshold":false}}}),
            Some("threshold=null"),
        ),
        (serde_json::json!({"threshold":0}), Some("threshold=1")),
        (
            serde_json::json!({"threshold":true,"extractor":{"danbooru":{"threshold":17}}}),
            Some("threshold=null"),
        ),
    ] {
        std::fs::write(&config, serde_json::to_vec(&document).unwrap()).unwrap();
        for (url, booru) in [
            ("https://danbooru.donmai.us/posts?tags=cat", true),
            ("https://e621.net/posts?tags=cat", true),
            (
                "https://gelbooru.com/index.php?page=post&s=list&tags=cat",
                false,
            ),
            ("https://instagram.com/p/Example/", false),
        ] {
            let output = std::process::Command::new(env!("CARGO_BIN_EXE_gallery-dl-rs"))
                .env("BOORU_THRESHOLD_ARGUMENTS", &log)
                .arg("--native-config")
                .arg(&config)
                .arg("--extractor-command")
                .arg(&backend)
                .args(["--extractor", "compatibility", "-g", url])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let arguments = std::fs::read_to_string(&log).unwrap();
            let args: Vec<_> = arguments.lines().collect();
            let forwarded: Vec<_> = args
                .windows(2)
                .filter(|pair| pair[0] == "--option" && pair[1].starts_with("threshold="))
                .map(|pair| pair[1])
                .collect();
            assert_eq!(
                forwarded,
                if booru {
                    expected.into_iter().collect::<Vec<_>>()
                } else {
                    Vec::new()
                },
                "{document} {url}"
            );
        }
    }
}

#[test]
fn danbooru_pool_order_aliases_and_falsy_resets_are_independent_of_kemono() {
    use gallery_dl_rs::{booru::PoolOrder, kemono::PostOrder};
    for (values, expected) in [
        (
            vec![
                serde_json::json!(null),
                serde_json::json!(false),
                serde_json::json!(0),
                serde_json::json!(""),
                serde_json::json!([]),
                serde_json::json!({}),
                serde_json::json!("asc"),
                serde_json::json!("pool"),
                serde_json::json!("pool_asc"),
                serde_json::json!("asc_pool"),
            ],
            PoolOrder::Pool,
        ),
        (
            vec![
                serde_json::json!("desc"),
                serde_json::json!("desc_pool"),
                serde_json::json!("pool_desc"),
            ],
            PoolOrder::PoolReverse,
        ),
        (
            vec![
                serde_json::json!("id"),
                serde_json::json!("desc_id"),
                serde_json::json!("id_desc"),
            ],
            PoolOrder::IdDescending,
        ),
        (
            vec![serde_json::json!("asc_id"), serde_json::json!("id_asc")],
            PoolOrder::IdAscending,
        ),
    ] {
        for value in values {
            let doc = serde_json::json!({"extractor":{"order-posts":"reverse","danbooru":{"order-posts":value},"kemono":{"order-posts":"asc"}}});
            let cfg = parse(&doc.to_string()).unwrap();
            let dan = cfg.resolve("danbooru").unwrap();
            assert_eq!(dan.booru_order_posts, Some(expected), "{value}");
            assert_eq!(dan.order_posts, Some(PostOrder::Reverse));
            assert_eq!(
                cfg.resolve("kemono").unwrap().order_posts,
                Some(PostOrder::Forward)
            );
            let override_settings = dan.overlay(&Options {
                order_posts: Some(PostOrder::Forward),
                ..Default::default()
            });
            assert_eq!(override_settings.booru_order_posts, Some(PoolOrder::Pool));
        }
    }
    let cfg = parse(r#"{"order-posts":"desc","extractor":{"danbooru":{"order-posts":"id_asc"}}}"#)
        .unwrap();
    assert_eq!(
        cfg.resolve("danbooru").unwrap().booru_order_posts,
        Some(PoolOrder::PoolReverse)
    );
    assert_eq!(
        parse("{}")
            .unwrap()
            .resolve("danbooru")
            .unwrap()
            .booru_order_posts,
        None
    );
    for value in [
        serde_json::json!(true),
        serde_json::json!(1),
        serde_json::json!(["pool"]),
        serde_json::json!("PRIVATE_INVALID_ORDER"),
    ] {
        let doc = serde_json::json!({"extractor":{"danbooru":{"order-posts":value}}});
        let err = parse(&doc.to_string()).unwrap_err().to_string();
        assert!(
            err.contains("order-posts") && !err.contains("PRIVATE_INVALID_ORDER"),
            "{err}"
        );
        // Unlike Danbooru, upstream e621 does not consult this option.
        let doc = serde_json::json!({"extractor":{"e621":{"order-posts":value}}});
        assert_eq!(
            parse(&doc.to_string())
                .unwrap()
                .resolve("e621")
                .unwrap()
                .booru_order_posts,
            None
        );
    }
}

#[test]
fn upstream_configs_load_with_unknown_keys_and_sections() {
    let config = parse(
        r#"{
            "extractor": {
                "sleep-request": 1.5,
                "pixiv": {"unknown-option": 1, "ugoira": true},
                "imgur": {"mp4": true}
            },
            "downloader": {"rate": "1M", "retries": 4},
            "output": {"mode": "auto"},
            "custom-root": false
        }"#,
    )
    .unwrap();
    assert_eq!(
        config.sections()["downloader"]["retries"],
        serde_json::json!(4)
    );
    let site = config.resolve("4chan").unwrap().site_options.unwrap();
    assert_eq!(site["sleep-request"], serde_json::json!(1.5));
    assert_eq!(site["custom-root"], serde_json::json!(false));
    let pixiv = config.resolve("pixiv").unwrap();
    assert_eq!(
        pixiv.site_options.unwrap()["unknown-option"],
        serde_json::json!(1)
    );
    assert!(parse(r#"{"extractor":{"password":"x"}}"#).is_err());
}

#[tokio::test]
async fn yaml_and_toml_configurations_load_by_extension_or_declaration() {
    let root = tempfile::tempdir().unwrap();
    let yaml = root.path().join("c.yml");
    std::fs::write(&yaml, "extractor:\n  imgur:\n    mp4: false\n").unwrap();
    let config = Config::load(&yaml).await.unwrap();
    assert_eq!(config.resolve("imgur").unwrap().mp4, Some(false));
    let toml = root.path().join("c.conf");
    std::fs::write(&toml, "[extractor.imgur]\nmp4 = true\n").unwrap();
    assert!(Config::load(&toml).await.is_err());
    gallery_dl_rs::config::set_format(&toml, gallery_dl_rs::config::Format::Toml);
    let config = Config::load(&toml).await.unwrap();
    assert_eq!(config.resolve("imgur").unwrap().mp4, Some(true));
}
