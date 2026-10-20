use gallery_dl_rs::{extract, filter::FilterExpression, kemono, naming::FilenameTemplate};
use serde_json::json;

#[test]
fn native_and_compatibility_kemono_metadata_share_filters_templates_and_archive_identity() {
    for (host, category) in [("kemono.cr", "kemono"), ("coomer.st", "coomer")] {
        let post = json!({"id":"7","user":"42","service":"patreon","title":"Hello", "published":"2024-01-02T03:04:05", "file":{"name":"cover.jpg","path":"/aa/bb/aabb.jpg"}});
        let native = kemono::parse_post(&post, host, "patreon", "42").remove(0);
        let data = json!({"id":"7","user":"42","service":"patreon","title":"Hello", "date":"2024-01-02T03:04:05", "hash":"aabb","name":"cover.jpg","type":"file","filename":"cover","extension":"jpg","num":1,"private":"PRIVATE_VALUE"});
        let compatible = extract::parse_messages(
            &serde_json::to_vec(&json!([[3, native.url, data]])).unwrap(),
            category,
        )
        .unwrap()
        .remove(0);
        assert_eq!(native.archive_key(), compatible.archive_key());
        assert_eq!(
            serde_json::to_value(&native.metadata).unwrap(),
            serde_json::to_value(&compatible.metadata).unwrap()
        );
        assert!(
            FilterExpression::parse("post_id == 7 and username == '42' and service == 'patreon'")
                .unwrap()
                .matches(&compatible)
                .unwrap()
        );
        assert_eq!(
            FilenameTemplate::parse("{service}_{user}_{post_id}_{index:02}.{extension}")
                .unwrap()
                .render(&compatible)
                .unwrap(),
            "patreon_42_7_01.jpg"
        );
        assert!(
            !serde_json::to_string(&compatible)
                .unwrap()
                .contains("PRIVATE_VALUE")
        );
    }
}

#[test]
fn compatibility_kemono_invalid_owner_service_or_hash_cannot_gain_archive_identity() {
    for (key, value) in [
        ("user", json!("../private")),
        ("service", json!("Patreon")),
        ("hash", json!("../private")),
        ("id", json!("../private")),
    ] {
        let mut data = json!({"id":"7","user":"42","service":"patreon","hash":"aabb","filename":"cover","extension":"jpg","num":1});
        data[key] = value;
        let media = extract::parse_messages(
            &serde_json::to_vec(&json!([[3, "https://kemono.cr/data/aa/bb/aabb.jpg", data]]))
                .unwrap(),
            "kemono",
        )
        .unwrap()
        .remove(0);
        assert_eq!(media.archive_key(), None);
    }
}

#[test]
fn compatibility_thumbnail_archive_keys_preserve_creator_scope_and_validate_origins() {
    for (domain, category) in [("kemono.cr", "kemono"), ("coomer.st", "coomer")] {
        let parse = |url: &str, user: &str| {
            let data = json!({"id":"7","user":user,"service":"patreon","hash":"aabb","filename":"cover","extension":"jpg","num":1});
            extract::parse_messages(
                &serde_json::to_vec(&json!([[3, url, data]])).unwrap(),
                category,
            )
            .map(|mut media| media.remove(0))
        };
        let original = parse(&format!("https://{domain}/data/aa/bb/aabb.jpg"), "42").unwrap();
        let thumbnail = parse(
            &format!("https://img.{domain}/thumbnail/data/aa/bb/aabb.jpg"),
            "42",
        )
        .unwrap();
        let other_creator = parse(
            &format!("https://img.{domain}/thumbnail/data/aa/bb/aabb.jpg"),
            "43",
        )
        .unwrap();
        assert!(thumbnail.archive_key().is_some());
        assert_eq!(original.archive_key(), thumbnail.archive_key());
        assert_ne!(thumbnail.archive_key(), other_creator.archive_key());
        assert_ne!(
            FilenameTemplate::parse("{service}_{user}_{id}.{extension}")
                .unwrap()
                .render(&thumbnail)
                .unwrap(),
            FilenameTemplate::parse("{service}_{user}_{id}.{extension}")
                .unwrap()
                .render(&other_creator)
                .unwrap()
        );
        for url in [
            format!("https://img.{domain}.evil.test/thumbnail/data/aabb.jpg"),
            format!("https://img.{domain}/other/aabb.jpg"),
            format!("http://img.{domain}/thumbnail/data/aabb.jpg"),
            format!("https://img.{domain}:444/thumbnail/data/aabb.jpg"),
            format!("https://private@img.{domain}/thumbnail/data/aabb.jpg"),
            "https://img.catbox.moe/thumbnail/data/aabb.jpg".into(),
        ] {
            // The shared URL parser rejects credentials before constructing
            // media. Other unsupported origins must have no archive identity.
            if let Ok(media) = parse(&url, "42") {
                assert_eq!(media.archive_key(), None, "{url}");
            }
        }
    }
}

#[test]
fn sanitized_public_listing_and_detail_captures_preserve_media_order_and_identity() {
    let page = kemono::parse_global_listing(
        include_bytes!("fixtures/kemono/global-posts.json"),
        "kemono.cr",
    )
    .unwrap();
    assert_eq!(page.count, 1);
    assert_eq!(page.posts.len(), 1);
    let detail = kemono::parse_single(
        include_bytes!("fixtures/kemono/post.json"),
        "kemono.cr",
        "patreon",
        "42",
        "700",
    )
    .unwrap();
    assert_eq!(detail.len(), 2);
    assert_eq!(page.posts[0].len(), 2);
    for (listing, detail) in page.posts[0].iter().zip(&detail) {
        assert_eq!(listing.url, detail.url);
        assert_eq!(listing.filename, detail.filename);
        assert_eq!(listing.archive_key(), detail.archive_key());
        assert_eq!(
            listing.metadata.as_ref().unwrap().timestamp,
            Some(1704164645)
        );
    }
}

#[test]
fn source_positions_keep_duplicate_files_distinct_and_archive_hashless_inline_files() {
    let data = json!({"id":"7","user":"42","service":"patreon","hash":"aabb","filename":"file","extension":"jpg","num":1,"type":"attachment"});
    let parse = |data| {
        extract::parse_messages(
            &serde_json::to_vec(&json!([[3, "https://kemono.cr/data/aa/bb/aabb.jpg", data]]))
                .unwrap(),
            "kemono",
        )
        .unwrap()
        .remove(0)
    };
    let first = parse(data.clone());
    let mut second = data.clone();
    second["num"] = json!(2);
    assert_ne!(first.archive_key(), parse(second).archive_key());
    let mut inline = data;
    inline["hash"] = json!("");
    inline["type"] = json!("inline");
    assert!(parse(inline.clone()).archive_key().is_some());
    inline["num"] = json!(0);
    assert!(parse(inline).archive_key().is_none());
}

#[test]
fn hashless_inline_archives_keep_native_and_compatibility_source_identity() {
    use gallery_dl_rs::kemono::{FileKind, Selection};
    let post = json!({"id":"7","user":"42","service":"patreon","content":"<a src=\"/inline/file.zip\">file</a>"});
    let native = kemono::parse_post_with_selection(
        &post,
        "kemono.cr",
        "patreon",
        "42",
        &Selection {
            original: true,
            files: Some(vec![FileKind::Inline]),
            ..Default::default()
        },
    )
    .remove(0);
    assert_eq!(
        native
            .metadata
            .as_ref()
            .unwrap()
            .file
            .as_ref()
            .unwrap()
            .media_type
            .as_deref(),
        Some("archive")
    );
    assert!(native.archive_key().is_some());
    let data = json!({"id":"7","user":"42","service":"patreon","num":1,"hash":"","type":"archive","filename":"file","extension":"zip"});
    let compatible = extract::parse_messages(
        &serde_json::to_vec(&json!([[3, native.url, data]])).unwrap(),
        "kemono",
    )
    .unwrap()
    .remove(0);
    assert_eq!(native.archive_key(), compatible.archive_key());
}
