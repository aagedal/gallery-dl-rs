//! Source job.py `_init`: `if init and init != "lazy": initialize()`.
use gallery_dl_rs::config::{Config, Options};
use serde_json::{Value, json};
use std::path::Path;
fn config(value: Value) -> Config {
    Config::parse(
        &serde_json::to_vec(&value).unwrap(),
        Path::new(if cfg!(windows) {
            r"C:\fixture"
        } else {
            "/fixture"
        }),
    )
    .unwrap()
}
#[test]
fn source_truthiness_and_exact_lazy_alias_apply_to_every_scope() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/postprocessors/init.json")).unwrap();
    for case in fixture["cases"].as_array().unwrap() {
        let value = &case["value"];
        let expected = case["eager"].as_bool().unwrap();
        for document in [
            json!({"init":value}),
            json!({"extractor":{"init":value}}),
            json!({"extractor":{"pixiv":{"init":value}}}),
        ] {
            assert_eq!(
                config(document).resolve("pixiv").unwrap().init,
                Some(expected),
                "{value:?}"
            );
        }
    }
    assert_eq!(config(json!({})).resolve("pixiv").unwrap().init, None);
}
#[test]
fn scoped_inheritance_null_clearing_and_root_override_match_other_scalar_options() {
    let settings = config(
        json!({"extractor":{"init":true,"pixiv":{"init":"lazy"},"pixiv-novel":{"init":null},"twitter":{"init":true}}}),
    );
    assert_eq!(settings.resolve("pixiv").unwrap().init, Some(false));
    assert_eq!(settings.resolve("pixiv-novel").unwrap().init, Some(false));
    assert_eq!(settings.resolve("twitter").unwrap().init, Some(true));
    assert_eq!(settings.resolve("direct").unwrap().init, Some(true));
    let root = config(json!({"init":null,"extractor":{"init":true,"pixiv":{"init":true}}}));
    for category in ["pixiv", "pixiv-novel", "direct"] {
        assert_eq!(root.resolve(category).unwrap().init, Some(false));
    }
    let root = config(json!({"init":true,"extractor":{"init":false,"pixiv":{"init":false}}}));
    assert_eq!(root.resolve("pixiv").unwrap().init, Some(true));
}
#[test]
fn programmatic_overlay_retains_absence_and_applies_explicit_false() {
    let enabled = Options {
        init: Some(true),
        ..Default::default()
    };
    assert_eq!(enabled.overlay(&Options::default()).init, Some(true));
    assert_eq!(
        enabled
            .overlay(&Options {
                init: Some(false),
                ..Default::default()
            })
            .init,
        Some(false)
    );
    assert!(!format!("{enabled:?}").contains("true"));
}
