use gallery_dl_rs::{
    event::Post, extract::parse_messages, filter::FilterExpression, naming::FilenameTemplate,
};
use serde_json::json;

#[test]
fn recovery_flags_reach_metadata_filters_and_templates_without_changing_archives() {
    for (ajax, mypixiv) in [(true, false), (false, true)] {
        let data = json!({"id":42,"num":0,"extension":"jpg","user":{"id":5,"account":"artist"},"_ajax":ajax,"_mypixiv":mypixiv});
        let mut items = parse_messages(
            &serde_json::to_vec(&json!([[3, "https://i.pximg.net/42_p0.jpg", data]])).unwrap(),
            "pixiv",
        )
        .unwrap();
        let item = items.pop().unwrap();
        let metadata = item.metadata.as_ref().unwrap();
        assert_eq!(metadata.ajax, Some(ajax));
        assert_eq!(metadata.mypixiv, Some(mypixiv));
        let encoded = serde_json::to_value(metadata).unwrap();
        assert_eq!(encoded["ajax"], ajax);
        assert_eq!(encoded["mypixiv"], mypixiv);
        let predicate = FilterExpression::parse(if ajax {
            "_ajax and ajax and not _mypixiv and not mypixiv"
        } else {
            "not _ajax and not ajax and _mypixiv and mypixiv"
        })
        .unwrap();
        assert!(predicate.matches(&item).unwrap());
        assert!(
            predicate
                .matches_post(&Post::from_media(std::slice::from_ref(&item), "pixiv"))
                .unwrap()
        );
        assert_eq!(
            FilenameTemplate::parse("{_ajax}_{mypixiv}.{extension}")
                .unwrap()
                .render(&item)
                .unwrap(),
            format!(
                "{}_{}.jpg",
                if ajax { "True" } else { "False" },
                if mypixiv { "True" } else { "False" }
            )
        );
        let mut changed = item.clone();
        changed.metadata.as_mut().unwrap().ajax = None;
        changed.metadata.as_mut().unwrap().mypixiv = None;
        assert_eq!(item.archive_key(), changed.archive_key());
    }
}

#[test]
fn ordinary_and_unrelated_posts_do_not_gain_recovery_flags() {
    for (category, data) in [
        ("pixiv", json!({"id":42,"num":0,"extension":"jpg"})),
        (
            "instagram",
            json!({"id":42,"num":0,"extension":"jpg","_ajax":true,"_mypixiv":true}),
        ),
    ] {
        let items = parse_messages(
            &serde_json::to_vec(&json!([[3, "https://cdn.example/42.jpg", data]])).unwrap(),
            category,
        )
        .unwrap();
        let encoded = serde_json::to_value(items[0].metadata.as_ref().unwrap()).unwrap();
        assert!(encoded.get("ajax").is_none());
        assert!(encoded.get("mypixiv").is_none());
    }
}

#[test]
fn original_artwork_names_preserve_jpeg_and_longer_source_extensions() {
    for (raw, expected) in [
        ("JPEG", "jpeg"),
        ("webp", "webp"),
        ("abcdefghijklmnop", "abcdefghijklmnop"),
    ] {
        let work = json!({"id":42,"type":"illust","meta_single_page":{"original_image_url":format!("https://i.pximg.net/42_p0.{raw}?signature=fixture")}});
        let files = gallery_dl_rs::pixiv::parse_illust(&work).unwrap();
        assert_eq!(files[0].filename, format!("42_p0.{expected}"));
    }
}
