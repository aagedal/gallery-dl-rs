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
            FilterExpression::parse("post_id == 7 and user == '42' and service == 'patreon'")
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

#[test]
fn revision_metadata_reaches_post_filters_templates_and_stable_historical_archives() {
    use gallery_dl_rs::event::Post;
    let hash = "0123456789abcdef0123456789abcdef01234567";
    let base = json!({"id":"7","user":"42","service":"patreon","hash":"aabb","num":1,"type":"file","filename":"file","extension":"jpg"});
    let parse = |data| {
        extract::parse_messages(
            &serde_json::to_vec(&json!([[3, "https://kemono.cr/data/aa/bb/aabb.jpg", data]]))
                .unwrap(),
            "kemono",
        )
        .unwrap()
        .remove(0)
    };
    let ordinary = parse(base.clone());
    let mut current = base.clone();
    current["revision_id"] = json!(0);
    current["revision_hash"] = json!(hash);
    current["revision_index"] = json!(3);
    current["revision_count"] = json!(3);
    let current = parse(current);
    assert_eq!(ordinary.archive_key(), current.archive_key());
    let mut historical = base;
    historical["revision_id"] = json!(12);
    historical["revision_hash"] = json!(hash.to_ascii_uppercase());
    historical["revision_index"] = json!(1);
    historical["revision_count"] = json!(3);
    let historical = parse(historical);
    assert_ne!(current.archive_key(), historical.archive_key());
    assert!(FilterExpression::parse("revision_id == 12 and revision_index == 1 and revision_count == 3 and revision['hash'] == revision_hash").unwrap().matches(&historical).unwrap());
    assert!(
        FilterExpression::parse("revision_id == 12 and metadata['revision']['count'] == 3")
            .unwrap()
            .matches_post(&Post::from_media(
                std::slice::from_ref(&historical),
                "kemono"
            ))
            .unwrap()
    );
    assert_eq!(
        FilenameTemplate::parse(
            "{post_id}_r{revision_id}_{revision_index:02}_{revision_count}.{extension}"
        )
        .unwrap()
        .render(&historical)
        .unwrap(),
        "7_r12_01_3.jpg"
    );
    assert_eq!(
        FilenameTemplate::parse("{revision[id]}_{metadata[revision][index]}.{extension}")
            .unwrap()
            .render(&historical)
            .unwrap(),
        "12_1.jpg"
    );
    let mut renamed = historical.clone();
    let revision = renamed
        .metadata
        .as_mut()
        .unwrap()
        .revision
        .as_mut()
        .unwrap();
    revision.hash = None;
    revision.index = Some(2);
    revision.count = Some(4);
    assert_eq!(historical.archive_key(), renamed.archive_key());
}

#[test]
fn malformed_revision_identity_does_not_silently_gain_current_post_archive_scope() {
    for revision_id in [
        json!(null),
        json!(false),
        json!(-1),
        json!(1.5),
        json!("PRIVATE_VALUE"),
    ] {
        let data = json!({"id":"7","user":"42","service":"patreon","hash":"aabb","num":1,"type":"file","filename":"file","extension":"jpg","revision_id":revision_id,"revision_hash":"PRIVATE_VALUE","revision_index":0,"revision_count":{}});
        let media = extract::parse_messages(
            &serde_json::to_vec(&json!([[3, "https://kemono.cr/data/aa/bb/aabb.jpg", data]]))
                .unwrap(),
            "kemono",
        )
        .unwrap()
        .remove(0);
        assert_eq!(media.archive_key(), None);
        assert_eq!(media.metadata.as_ref().unwrap().post_id, "7");
        let revision = media.metadata.as_ref().unwrap().revision.as_ref().unwrap();
        assert!(
            revision.id.is_none()
                && revision.hash.is_none()
                && revision.index.is_none()
                && revision.count.is_none()
        );
        assert!(
            !serde_json::to_string(&media)
                .unwrap()
                .contains("PRIVATE_VALUE")
        );
    }
}

#[test]
fn creator_profiles_reach_filters_and_templates_without_changing_archive_ownership() {
    use gallery_dl_rs::event::Post;
    let profile: serde_json::Value =
        serde_json::from_slice(include_bytes!("fixtures/kemono/profile.json")).unwrap();
    let parsed = kemono::parse_creator_profile(
        include_bytes!("fixtures/kemono/profile.json"),
        "patreon",
        "42",
    )
    .unwrap();
    assert_eq!(parsed.indexed, Some(1704067200));
    assert_eq!(parsed.updated, Some(1704153600));
    let post = json!({"id":"7","user":"42","service":"patreon","file":{"name":"cover.jpg","path":"/aa/bb/aabb.jpg"}});
    let mut native = kemono::parse_post(&post, "kemono.cr", "patreon", "42").remove(0);
    let before = native.archive_key();
    assert!(before.is_some());
    native.metadata.as_mut().unwrap().creator = Some(parsed.clone());
    native.metadata.as_mut().unwrap().username = parsed.name.clone();
    assert_eq!(before, native.archive_key());
    let data = json!({"id":"7","user":"42","service":"patreon","hash":"aabb","num":1,"type":"file","name":"cover.jpg","filename":"cover","extension":"jpg","username":"Alice","user_profile":profile,"private_key":"PRIVATE_VALUE"});
    let compatible = extract::parse_messages(
        &serde_json::to_vec(&json!([[3, native.url, data]])).unwrap(),
        "kemono",
    )
    .unwrap()
    .remove(0);
    assert_eq!(
        serde_json::to_value(&native.metadata).unwrap(),
        serde_json::to_value(&compatible.metadata).unwrap()
    );
    assert_eq!(native.archive_key(), compatible.archive_key());
    assert!(FilterExpression::parse("username == 'Alice' and user == '42' and creator_id == '42' and creator_name == 'Alice' and user_profile['post_count'] == 7 and metadata['creator']['dm_count'] == 0").unwrap().matches_post(&Post::from_media(std::slice::from_ref(&native),"kemono")).unwrap());
    assert_eq!(
        FilenameTemplate::parse(
            "{user}_{username}_{creator[name]}_{user_profile[post_count]}.{extension}"
        )
        .unwrap()
        .render(&native)
        .unwrap(),
        "42_Alice_Alice_7.jpg"
    );
    let meta = native.metadata.as_mut().unwrap();
    meta.username = Some("Bob".into());
    meta.creator.as_mut().unwrap().name = Some("Bob".into());
    assert_eq!(before, native.archive_key());
    native
        .metadata
        .as_mut()
        .unwrap()
        .creator
        .as_mut()
        .unwrap()
        .id = "43".into();
    assert_ne!(before, native.archive_key());
    assert!(
        !serde_json::to_string(&compatible)
            .unwrap()
            .contains("PRIVATE_VALUE")
    );
}

#[test]
fn compatibility_display_names_cannot_supply_missing_creator_identity() {
    for (user, profile) in [
        (json!("../private"), None),
        (
            json!("42"),
            Some(json!({"id":"43","service":"patreon","name":"PRIVATE_VALUE"})),
        ),
    ] {
        let mut data = json!({"id":"7","user":user,"service":"patreon","hash":"aabb","num":1,"filename":"file","extension":"jpg","username":"999"});
        if let Some(profile) = profile {
            data["user_profile"] = profile;
        }
        let media = extract::parse_messages(
            &serde_json::to_vec(&json!([[3, "https://kemono.cr/data/aa/bb/aabb.jpg", data]]))
                .unwrap(),
            "kemono",
        )
        .unwrap()
        .remove(0);
        let creator = media.metadata.as_ref().unwrap().creator.as_ref().unwrap();
        if creator.id.is_empty() {
            assert_eq!(media.archive_key(), None);
        } else {
            assert_eq!(creator.id, "42");
            assert!(media.archive_key().is_some());
            assert!(media.metadata.as_ref().unwrap().username.is_none());
        }
        assert!(
            !serde_json::to_string(&media)
                .unwrap()
                .contains("PRIVATE_VALUE")
        );
    }
}

#[tokio::test]
async fn gzip_post_and_profile_responses_decode_without_changing_media_encoding() {
    use futures_util::StreamExt;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
        time::Duration,
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let root = format!("http://{}/", listener.local_addr().unwrap())
        .parse()
        .unwrap();
    let server = thread::spawn(move || {
        let bodies=[json!({"post":{"id":"7","user":"42","service":"patreon","file":{"name":"cover.jpg","path":"/aa/bb/aabb.jpg"}}}).to_string().into_bytes(),include_bytes!("fixtures/kemono/profile.json").to_vec()];
        let mut requests = Vec::new();
        for body in bodies {
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(&body).unwrap();
            let compressed = encoder.finish().unwrap();
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                let count = socket.read(&mut buffer).unwrap();
                assert_ne!(count, 0);
                request.extend_from_slice(&buffer[..count]);
            }
            requests.push(String::from_utf8(request).unwrap());
            write!(socket,"HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",compressed.len()).unwrap();
            socket.write_all(&compressed).unwrap();
        }
        requests
    });
    let extractor = kemono::Kemono::new(Duration::from_secs(4))
        .unwrap()
        .with_api_root(root)
        .with_network(
            &gallery_dl_rs::network::NetworkOptions::new(Some(String::new()), None).unwrap(),
        )
        .unwrap();
    let media = extractor
        .stream(&"https://kemono.cr/patreon/user/42/post/7".parse().unwrap())
        .collect::<Vec<_>>()
        .await;
    assert_eq!(media.len(), 1);
    let media = media[0].as_ref().unwrap();
    assert_eq!(
        media.metadata.as_ref().unwrap().username.as_deref(),
        Some("Alice")
    );
    assert_eq!(
        media
            .metadata
            .as_ref()
            .unwrap()
            .creator
            .as_ref()
            .unwrap()
            .id,
        "42"
    );
    assert_eq!(
        media.headers.get("Accept-Encoding").map(String::as_str),
        Some("identity")
    );
    let requests = server.join().unwrap();
    assert!(requests[1].starts_with("GET /api/v1/patreon/user/42/profile "));
    assert!(requests.iter().all(|request| {
        request
            .to_ascii_lowercase()
            .contains("accept-encoding: gzip")
    }));
}
