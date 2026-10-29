use gallery_dl_rs::dispatch::{Include, select};
use serde_json::{Value, json};

#[test]
fn include_selection_matches_recorded_upstream_dispatch_oracles() {
    let fixture: Value =
        serde_json::from_str(include_str!("../fixtures/dispatch/include.json")).unwrap();
    for case in fixture["cases"].as_array().unwrap() {
        let include = case["present"]
            .as_bool()
            .unwrap()
            .then(|| Include::parse(&case["include"]).unwrap());
        assert_eq!(
            json!(select(
                include.as_ref(),
                &["timeline"],
                &["timeline", "media", "with-replies"],
                &[("replies", "with-replies")]
            )),
            case["expected"],
            "{}",
            case["case"]
        );
        if let Some(include) = include {
            assert_eq!(Include::parse(&include.value()).unwrap(), include);
        }
    }
}

#[test]
fn include_bounds_fail_privately_and_do_not_trim_list_items() {
    for value in [
        json!(true),
        json!(7),
        json!({"private":"include-secret"}),
        json!(["media", 7]),
        json!("include-secret\u{0}"),
        json!("x".repeat(129)),
        json!(vec!["media"; 129]),
    ] {
        let error = Include::parse(&value).unwrap_err().to_string();
        assert!(!error.contains("include-secret"));
    }
    let parsed = Include::parse(&json!([" media ", "media"])).unwrap();
    assert_eq!(select(Some(&parsed), &[], &["media"], &[]), ["media"]);
}
