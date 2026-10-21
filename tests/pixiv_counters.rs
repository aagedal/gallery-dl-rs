use gallery_dl_rs::{
    event::Post,
    extract,
    filter::FilterExpression,
    naming::{DirectoryTemplate, FilenameTemplate},
    pixiv, postprocess,
};
use serde_json::{Value, json};

#[test]
fn ordinary_artwork_counters_match_consumed_upstream_generator_before_each_file() {
    let source: Value =
        serde_json::from_str(include_str!("fixtures/postprocessors/pixiv_ownership.json")).unwrap();
    for name in [
        "ordinary_multi_page_shared_and_enumerate_reset",
        "ordinary_single_suffix_hook_persists",
        "pixivision_inherits_shared_items",
    ] {
        let case = source["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|case| case["name"] == name)
            .unwrap();
        let native = pixiv::parse_illust(&case["input"]).unwrap();
        let files = case["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["kind"] == "file")
            .collect::<Vec<_>>();
        assert_eq!(native.len(), files.len());
        for (position, (native, recorded)) in native.iter().zip(files).enumerate() {
            let counter = recorded["before_hooks"]["num"].as_u64().unwrap();
            assert_eq!(
                counter, position as u64,
                "source enumerate overwrites hook-modified counters"
            );
            let compatible = extract::parse_messages(
                &serde_json::to_vec(&json!([[3, recorded["url"], recorded["before_hooks"]]]))
                    .unwrap(),
                "pixiv",
            )
            .unwrap()
            .remove(0);
            for media in [native, &compatible] {
                assert_eq!(media.metadata.as_ref().unwrap().index, position + 1);
                assert_eq!(postprocess::context(media)["num"], counter);
                assert!(
                    FilterExpression::parse(&format!(
                        "num == {counter} and index == {}",
                        position + 1
                    ))
                    .unwrap()
                    .matches(media)
                    .unwrap()
                );
                assert_eq!(
                    FilenameTemplate::parse("{num:02}_{index:02}.{extension}")
                        .unwrap()
                        .render(media)
                        .unwrap(),
                    format!("{counter:02}_{:02}.jpg", position + 1)
                );
                assert_eq!(
                    DirectoryTemplate::parse(&["{num}".into()])
                        .unwrap()
                        .render(media)
                        .unwrap()
                        .unwrap(),
                    std::path::PathBuf::from(counter.to_string())
                );
                let serialized = serde_json::to_value(media).unwrap();
                assert!(serialized["metadata"].get("source_num").is_none());
            }
            if !name.contains("pixivision") {
                assert_eq!(native.archive_key(), compatible.archive_key());
            }
        }
        let post = Post::from_media(&native, "pixiv");
        assert!(
            FilterExpression::parse("num == 0")
                .unwrap()
                .matches_post(&post)
                .unwrap()
        );
    }
}

#[test]
fn compatibility_preserves_explicit_counter_types_instead_of_rebuilding_from_index() {
    for (counter, name) in [
        (json!(-2), Some("-2")),
        (json!(0), Some("0")),
        (json!(42), Some("42")),
        (json!(false), Some("False")),
        (json!("page"), Some("page")),
        (Value::Null, None),
    ] {
        let data = json!({"id":42,"num":counter,"filename":"42_p0","extension":"jpg"});
        let media = extract::parse_messages(
            &serde_json::to_vec(&json!([[
                3,
                "https://i.pximg.net/img-original/img/2024/01/02/03/04/05/42_p0.jpg",
                data
            ]]))
            .unwrap(),
            "pixiv",
        )
        .unwrap()
        .remove(0);
        assert_eq!(postprocess::context(&media)["num"], counter);
        if let Some(name) = name {
            assert_eq!(
                FilenameTemplate::parse("{num}")
                    .unwrap()
                    .render(&media)
                    .unwrap(),
                name
            );
        } else {
            assert!(
                FilterExpression::parse("num is None")
                    .unwrap()
                    .matches(&media)
                    .unwrap()
            );
        }
    }
}
