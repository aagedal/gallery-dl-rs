use gallery_dl_rs::{
    config::Config,
    postprocess::{Specification, ugoira::Mode},
};
use serde_json::{Value, json};
use std::path::Path;
// Configuration directories must be absolute on the host platform.
const CONFIG_DIRECTORY: &str = if cfg!(windows) {
    r"C:\synthetic\config"
} else {
    "/synthetic/config"
};
fn config(value: Value) -> Config {
    Config::parse(
        &serde_json::to_vec(&value).unwrap(),
        Path::new(CONFIG_DIRECTORY),
    )
    .unwrap()
}
fn specs(config: &Config, category: &str) -> Vec<Specification> {
    config
        .resolve(category)
        .unwrap()
        .postprocessors
        .unwrap()
        .specifications()
        .unwrap()
}
fn extensions(specifications: &[Specification]) -> Vec<&str> {
    specifications
        .iter()
        .map(|spec| spec.options.ugoira().unwrap().extension())
        .collect()
}
#[test]
fn scoped_lists_accumulate_category_then_extractor_then_root() {
    let configuration = config(
        json!({"postprocessors":[{"name":"ugoira","extension":"gif"}],"extractor":{"postprocessors":[{"name":"ugoira","extension":"webm"}],"pixiv":{"postprocessors":[{"name":"ugoira","extension":"mp4"}]},"pixiv-novel":{"postprocessors":[{"name":"ugoira","extension":"mkv"}]}}}),
    );
    assert_eq!(
        extensions(&specs(&configuration, "pixiv")),
        ["mp4", "webm", "gif"]
    );
    assert_eq!(
        extensions(&specs(&configuration, "pixiv-novel")),
        ["mkv", "webm", "gif"]
    );
    assert_eq!(
        extensions(&specs(&configuration, "direct")),
        ["webm", "gif"]
    );
    let configuration = config(
        json!({"postprocessors":[{"name":"ugoira","extension":"gif"}],"extractor":{"postprocessors":[{"name":"ugoira","extension":"webm"}],"pixiv":{"postprocessors":null}}}),
    );
    assert_eq!(extensions(&specs(&configuration, "pixiv")), ["webm", "gif"]);
}
#[test]
fn named_definitions_type_aliases_and_mode_event_suffixes_resolve() {
    let configuration = config(
        json!({"postprocessor":{"animation":{"name":"ugoira","extension":"mkv","keep-files":true},"archived":{"name":"ugoira/archive","metadata":"frames.json"}},"extractor":{"pixiv":{"postprocessors":["animation",{"type":"animation","extension":"mp4"},{"type":"archived","metadata":false},"ugoira/image2@after"]}}}),
    );
    let entries = specs(&configuration, "pixiv");
    assert_eq!(extensions(&entries), ["mkv", "mp4", "zip", "webm"]);
    assert!(entries[0].options.ugoira().unwrap().keep_files());
    assert!(entries[1].options.ugoira().unwrap().keep_files());
    assert_eq!(entries[2].options.ugoira().unwrap().mode(), Mode::Archive);
    assert_eq!(entries[2].options.ugoira().unwrap().metadata_name(), None);
    assert_eq!(entries[3].options.ugoira().unwrap().mode(), Mode::Image2);
}
#[test]
fn root_common_options_override_scoped_options_and_per_entry_values() {
    let configuration = config(
        json!({"postprocessor-options":{"extension":"avi","keep-files":true},"postprocessors":[{"name":"ugoira","extension":"gif"}],"extractor":{"postprocessor-options":{"extension":"webm"},"postprocessors":[{"name":"ugoira","extension":"mkv"}],"pixiv":{"postprocessor-options":{"extension":"mp4"},"postprocessors":[{"name":"ugoira","extension":"zip"}]}}}),
    );
    let entries = specs(&configuration, "pixiv");
    assert_eq!(extensions(&entries), ["avi", "avi", "avi"]);
    assert!(
        entries
            .iter()
            .all(|entry| entry.options.ugoira().unwrap().keep_files())
    );
}
#[test]
fn common_options_replace_instead_of_merging_and_explicit_falsy_values_clear() {
    let scoped = config(
        json!({"extractor":{"postprocessor-options":{"extension":"mkv","keep-files":true},"postprocessors":[{"name":"ugoira","extension":"gif"}],"pixiv":{"postprocessor-options":{"extension":"mp4"}}}}),
    );
    let entries = specs(&scoped, "pixiv");
    assert_eq!(extensions(&entries), ["mp4"]);
    assert!(!entries[0].options.ugoira().unwrap().keep_files());
    for clear in [Value::Null, json!({}), json!(false), json!("")] {
        let scoped = config(
            json!({"extractor":{"postprocessor-options":{"extension":"mkv"},"postprocessors":[{"name":"ugoira","extension":"gif"}],"pixiv":{"postprocessor-options":clear}}}),
        );
        assert_eq!(extensions(&specs(&scoped, "pixiv")), ["gif"]);
        let root = config(
            json!({"postprocessor-options":clear,"extractor":{"postprocessor-options":{"extension":"mkv"},"postprocessors":[{"name":"ugoira","extension":"gif"}],"pixiv":{"postprocessor-options":{"extension":"mp4"}}}}),
        );
        assert_eq!(extensions(&specs(&root, "pixiv")), ["gif"]);
    }
}
#[test]
fn definitions_remain_available_when_only_scoped_entries_or_common_options_are_present() {
    let configuration = config(
        json!({"postprocessor":{"animation":{"name":"ugoira","extension":"mkv","metadata":"frames.json"}},"postprocessor-options":{"repeat-last-frame":false},"extractor":{"pixiv":{"postprocessors":["animation"]}}}),
    );
    let entries = specs(&configuration, "pixiv");
    assert_eq!(extensions(&entries), ["mkv"]);
    assert_eq!(
        entries[0].options.ugoira().unwrap().metadata_name(),
        Some("frames.json")
    );
    assert!(!entries[0].options.ugoira().unwrap().repeat_last_frame());
    assert!(specs(&configuration, "direct").is_empty());
}
#[test]
fn invalid_postprocessor_options_errors_and_debug_never_quote_private_values() {
    const PRIVATE: &str = "PRIVATE_TOKEN_PATH_OR_CONFIGURATION_VALUE";
    let invalid = vec![
        json!({"postprocessor-options":PRIVATE}),
        json!({"postprocessor":[PRIVATE]}),
        json!({"postprocessors":[PRIVATE]}),
        json!({"postprocessors":[{"name":"ugoira","ffmpeg-args":PRIVATE}]}),
        json!({"postprocessors":[{"name":"ugoira","extension":format!("{PRIVATE}/unsafe")}]}),
        json!({"postprocessors":[{"name":"ugoira","filter":format!("{PRIVATE}('") }]}),
        json!({"postprocessors":[{"name":"ugoira","whitelist":[PRIVATE.repeat(10)]}]}),
        json!({"postprocessors":[{"name":"ugoira","ffmpeg-location":format!("/private/{PRIVATE}\0")}]}),
        json!({"postprocessors":[{"name":"ugoira","framerate":[PRIVATE]}]}),
    ];
    for value in invalid {
        let error = match Config::parse(
            &serde_json::to_vec(&value).unwrap(),
            Path::new(CONFIG_DIRECTORY),
        ) {
            Err(error) => error.to_string(),
            Ok(config) => {
                assert!(!format!("{config:?}").contains(PRIVATE));
                let resolved = config.resolve("pixiv").unwrap();
                assert!(!format!("{resolved:?}").contains(PRIVATE));
                match resolved.postprocessors.unwrap().specifications() {
                    Err(error) => error.to_string(),
                    Ok(_) => panic!("invalid postprocessor accepted"),
                }
            }
        };
        assert!(!error.contains(PRIVATE), "{error}");
    }
    let configuration = config(
        json!({"postprocessors":[{"name":"ugoira","ffmpeg-location":format!("/private/{PRIVATE}"),"ffmpeg-args":["-metadata",PRIVATE]}]}),
    );
    let resolved = configuration.resolve("pixiv").unwrap();
    assert!(!format!("{resolved:?}").contains(PRIVATE));
    let entries = resolved.postprocessors.unwrap().specifications().unwrap();
    assert!(!format!("{:?}", entries[0]).contains(PRIVATE));
    assert!(!format!("{:?}", entries[0].options).contains(PRIVATE));
}

fn frame_media(category: &str) -> gallery_dl_rs::extract::Media {
    gallery_dl_rs::extract::Media {
        url: url::Url::parse("https://i.pximg.net/42.zip").unwrap(),
        filename: "42.zip".into(),
        category: category.into(),
        metadata: Some(gallery_dl_rs::extract::PostMetadata {
            frames: Some(std::sync::Arc::from([gallery_dl_rs::pixiv::UgoiraFrame {
                file: "000000.jpg".into(),
                delay: 50,
            }])),
            ..Default::default()
        }),
        job: gallery_dl_rs::extract::MediaJob::DirectHttp,
        headers: Default::default(),
        handoff: Default::default(),
    }
}
#[test]
fn category_filter_strings_and_literal_wildcards_match_actual_source_vectors() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/pixiv/ugoira_postprocessor.json")).unwrap();
    for case in fixture["category_filter_vectors"].as_array().unwrap() {
        let configuration = config(
            json!({"postprocessors":[{"name":"ugoira","whitelist":case["whitelist"],"blacklist":case["blacklist"]}]}),
        );
        let result = configuration
            .resolve("pixiv")
            .unwrap()
            .postprocessors
            .unwrap()
            .specifications();
        if case["unsupported_qualifier"] == true {
            let Err(error) = result else {
                panic!("unsupported qualifier accepted")
            };
            assert_eq!(
                error.to_string(),
                "unsupported postprocessor category filter"
            );
        } else {
            let specifications = result.unwrap();
            assert_eq!(
                specifications[0].applies(&frame_media(case["category"].as_str().unwrap())),
                case["allowed"].as_bool().unwrap(),
                "{}",
                case["name"]
            );
        }
    }
}
#[test]
fn scope_accumulation_and_common_precedence_match_actual_source_config_helpers() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/pixiv/ugoira_postprocessor.json")).unwrap();
    for case in fixture["config_scope_vectors"].as_array().unwrap() {
        let configuration = config(case["config"].clone());
        let entries = specs(&configuration, "pixiv");
        let expected = case["accumulated"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| {
                case["common_options"]
                    .get("extension")
                    .or_else(|| entry.get("extension"))
                    .unwrap()
                    .as_str()
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(extensions(&entries), expected, "{}", case["name"]);
    }
}
#[test]
fn category_selector_bounds_qualifier_errors_and_unsupported_processors_are_private() {
    const PRIVATE: &str = "PRIVATE_SELECTOR_OR_EXEC_COMMAND";
    for selector in [
        json!(vec!["pixiv"; 129]),
        json!(PRIVATE.repeat(20)),
        json!([format!("pixiv:{PRIVATE}")]),
        json!([format!("*:{PRIVATE}")]),
    ] {
        let configuration =
            config(json!({"postprocessors":[{"name":"ugoira","whitelist":selector}]}));
        let Err(error) = configuration
            .resolve("pixiv")
            .unwrap()
            .postprocessors
            .unwrap()
            .specifications()
        else {
            panic!("invalid selector accepted")
        };
        assert!(!error.to_string().contains(PRIVATE));
    }
    let configuration = config(json!({"postprocessors":[{"name":"python","function":PRIVATE}]}));
    let Err(error) = configuration
        .resolve("pixiv")
        .unwrap()
        .postprocessors
        .unwrap()
        .specifications()
    else {
        panic!("python accepted")
    };
    assert!(!error.to_string().contains(PRIVATE));
    assert!(error.to_string().contains("supported: ugoira"));
}

#[test]
fn ugoira_retains_fixed_hooks_without_interpreting_generic_event_options() {
    for event in [json!({"ignored":true}), json!(42), json!(["unknown"])] {
        let spec = gallery_dl_rs::postprocess::Specification::parse(
            &json!({"name":"ugoira","mode":"archive","event":event}),
            std::path::Path::new("."),
        )
        .unwrap();
        assert!(spec.events.is_empty());
    }
}

#[test]
fn standard_fixed_hash_names_are_usable_by_native_filename_templates() {
    for name in [
        "md5",
        "sha1",
        "sha224",
        "sha256",
        "sha384",
        "sha512",
        "sha512_224",
        "sha512_256",
        "sha3_224",
        "sha3_256",
        "sha3_384",
        "sha3_512",
        "blake2b",
        "blake2s",
    ] {
        gallery_dl_rs::naming::FilenameTemplate::parse(&format!("{{{name}}}.{{extension}}"))
            .unwrap();
        gallery_dl_rs::postprocess::hash::HashOptions::parse(
            json!({"hashes":name,"filename":true}).as_object().unwrap(),
        )
        .unwrap();
    }
}
