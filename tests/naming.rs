use gallery_dl_rs::{
    extract::{Media, MediaJob, PostMetadata},
    naming::{DirectoryTemplate, FilenameTemplate},
};
use std::collections::BTreeMap;

fn media() -> Media {
    Media {
        url: "https://cdn.example/opaque?token=secret".parse().unwrap(),
        job: MediaJob::DirectHttp,
        category: "instagram".into(),
        filename: "native.photo.jpg".into(),
        metadata: Some(PostMetadata {
            community: None,
            community_archive: false,
            retweet_id: None,
            quoted_by_id: None,
            reposted_by_id: None,
            article: None,
            post_id: "123".into(),
            media_id: "456".into(),
            shortcode: "BA".into(),
            username: Some("alice".into()),
            caption: None,
            timestamp: Some(123456),
            index: 2,
        }),
        headers: BTreeMap::new(),
        handoff: BTreeMap::new(),
    }
}

#[test]
fn native_post_names_ignore_signed_transport_urls() {
    let template = FilenameTemplate::parse(
        "{category}_{username}_{post_id}_{media_id}_{shortcode}_{index:03}_{timestamp}.{extension}",
    )
    .unwrap();
    let mut item = media();
    let expected = "instagram_alice_123_456_BA_002_123456.jpg";
    assert_eq!(template.render(&item).unwrap(), expected);
    item.url = "https://other.example/different.png?token=rotated"
        .parse()
        .unwrap();
    assert_eq!(template.render(&item).unwrap(), expected);
    assert_eq!(
        FilenameTemplate::parse("{filename}")
            .unwrap()
            .render(&item)
            .unwrap(),
        "native.photo.jpg"
    );
    assert_eq!(
        FilenameTemplate::parse("{name}.{extension}")
            .unwrap()
            .render(&item)
            .unwrap(),
        "native.photo.jpg"
    );
    assert_eq!(
        FilenameTemplate::parse("{index:03d}")
            .unwrap()
            .render(&item)
            .unwrap(),
        "002"
    );
}

#[test]
fn missing_fields_fail_instead_of_inventing_names() {
    let mut item = media();
    item.metadata.as_mut().unwrap().username = None;
    item.metadata.as_mut().unwrap().shortcode.clear();
    item.metadata.as_mut().unwrap().timestamp = None;
    item.metadata.as_mut().unwrap().index = 0;
    for field in ["username", "shortcode", "timestamp", "index"] {
        assert!(
            FilenameTemplate::parse(&format!("{{{field}}}.jpg"))
                .unwrap()
                .render(&item)
                .is_err()
        );
    }
    item.metadata = None;
    assert!(
        FilenameTemplate::parse("{post_id}.jpg")
            .unwrap()
            .render(&item)
            .is_err()
    );
    assert!(
        FilenameTemplate::parse("{filename}")
            .unwrap()
            .render(&item)
            .is_ok()
    );
    item.filename = "no-extension".into();
    assert!(
        FilenameTemplate::parse("{extension}")
            .unwrap()
            .render(&item)
            .is_err()
    );
}

#[test]
fn templates_reject_paths_and_arbitrary_expressions() {
    for template in [
        "",
        ".",
        "..",
        "../{filename}",
        "/tmp/{filename}",
        "C:\\{filename}",
        "foo/bar",
        "foo\\bar",
        "{",
        "}",
        "{{{",
        "{unknown}",
        "{username.upper()}",
        "{index:3}",
        "{index:00}",
        "{index:0241}",
        "{index:03x}",
        "{index:03:04}",
        "{filename:03}",
    ] {
        assert!(
            FilenameTemplate::parse(template).is_err(),
            "accepted {template:?}"
        );
    }
}

#[test]
fn substitutions_and_escaped_literals_are_safe_basenames() {
    let mut item = media();
    item.metadata.as_mut().unwrap().username = Some("../../C:\\evil/\n雪".into());
    let name = FilenameTemplate::parse("{{{username}}}_{filename}")
        .unwrap()
        .render(&item)
        .unwrap();
    assert_eq!(name, "_.._.._C__evil_____native.photo.jpg");
    assert!(!name.contains(['/', '\\', ':', '\n']));
    assert!(name.is_ascii());
    assert_eq!(
        FilenameTemplate::parse("{{snow雪}}.jpg")
            .unwrap()
            .render(&item)
            .unwrap(),
        "_snow__.jpg"
    );
    for reserved in ["CON.jpg", "nul", "LPT1.png", "COM9.txt"] {
        assert!(
            FilenameTemplate::parse(reserved)
                .unwrap()
                .render(&item)
                .unwrap()
                .starts_with('_')
        );
    }
    item.filename = "..".into();
    assert!(
        FilenameTemplate::parse("{filename}")
            .unwrap()
            .render(&item)
            .is_err()
    );
}

#[test]
fn long_names_keep_the_complete_extension_within_byte_limit() {
    let mut item = media();
    item.filename = format!("{}.jpeg", "雪a".repeat(200));
    let name = FilenameTemplate::parse("{filename}")
        .unwrap()
        .render(&item)
        .unwrap();
    assert_eq!(name.len(), 240);
    assert!(name.ends_with(".jpeg"));
    assert!(name.is_ascii());
    item.filename = "a".repeat(1000);
    assert_eq!(
        FilenameTemplate::parse("{filename}")
            .unwrap()
            .render(&item)
            .unwrap()
            .len(),
        240
    );
    item.filename = format!("a.{}", "x".repeat(240));
    assert!(
        FilenameTemplate::parse("{filename}")
            .unwrap()
            .render(&item)
            .is_err()
    );
    item.filename = format!("aaaa.{}", "x".repeat(238));
    let name = FilenameTemplate::parse("{filename}")
        .unwrap()
        .render(&item)
        .unwrap();
    assert_eq!(name, format!("a.{}", "x".repeat(238)));
}

#[test]
fn truncation_cannot_create_reserved_windows_stems() {
    let mut item = media();
    let template = FilenameTemplate::parse("{filename}").unwrap();
    for (stem, extension_length, expected_stem) in [
        ("CONextra", 236, "_CO"),
        ("COM1extra", 235, "_COM"),
        ("con", 236, "_co"),
    ] {
        let extension = "x".repeat(extension_length);
        item.filename = format!("{stem}.{extension}");
        let name = template.render(&item).unwrap();
        assert_eq!(name.len(), 240);
        assert_eq!(name, format!("{expected_stem}.{extension}"));
    }
}

#[test]
fn dotfiles_have_no_extension_and_trailing_dots_are_trimmed() {
    let mut item = media();
    item.filename = ".hidden".into();
    assert_eq!(
        FilenameTemplate::parse("{name}")
            .unwrap()
            .render(&item)
            .unwrap(),
        "hidden"
    );
    assert!(
        FilenameTemplate::parse("{extension}")
            .unwrap()
            .render(&item)
            .is_err()
    );
    item.filename = "photo.jpg...".into();
    assert_eq!(
        FilenameTemplate::parse("{filename}")
            .unwrap()
            .render(&item)
            .unwrap(),
        "photo.jpg"
    );
}

#[test]
fn directory_components_replace_category_and_cannot_add_paths() {
    let mut item = media();
    item.metadata.as_mut().unwrap().username = Some("../../A:/\\B".into());
    let template =
        DirectoryTemplate::parse(&["{category}".into(), "{username}".into(), "{post_id}".into()])
            .unwrap();
    assert_eq!(
        template.render(&item).unwrap(),
        std::path::PathBuf::from("instagram/_.._A___B/123")
    );
    assert_eq!(
        DirectoryTemplate::parse(&[])
            .unwrap()
            .render(&item)
            .unwrap(),
        std::path::PathBuf::new()
    );
    for bad in [
        "",
        ".",
        "..",
        "../escape",
        "C:\\folder",
        "/absolute",
        "x\0",
        "{missing}",
    ] {
        assert!(DirectoryTemplate::parse(&[bad.into()]).is_err(), "{bad:?}");
    }
    item.metadata = None;
    assert!(template.render(&item).is_err());
}

#[test]
fn directory_limits_and_reserved_names_apply_to_each_component() {
    assert!(DirectoryTemplate::parse(&vec!["a".into(); 33]).is_err());
    assert!(DirectoryTemplate::parse(&["a".repeat(8193)]).is_err());
    let long = DirectoryTemplate::parse(&vec!["a".repeat(240); 13]).unwrap();
    assert!(long.render(&media()).is_err());
    let reserved = DirectoryTemplate::parse(&["CON".into(), "LPT1.jpg".into()]).unwrap();
    assert_eq!(
        reserved.render(&media()).unwrap(),
        std::path::PathBuf::from("_CON/_LPT1.jpg")
    );
}
