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
            file: None,
            community: None,
            community_archive: false,
            retweet_id: None,
            quoted_by_id: None,
            reposted_by_id: None,
            article: None,
            instance: None,
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
        "{index:0241}",
        "{index:03:04}",
        "{index:03q}",
        "{url}",
        "{_url}",
        "{username!z}",
        "{username!lu}",
        "{username:?_/}",
        "{username:L5}",
        "{username:Ax1/}",
        "{username:Cz/}",
        "{username:Ofoo/}",
        "{username[}",
        "{username[0}",
        "{username[a b]}",
        "{username|unknown}",
        "{'unterminated}",
        "{username.__class__.__init__()}",
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
        Some(std::path::PathBuf::from("instagram/_.._A___B/123"))
    );
    assert_eq!(
        DirectoryTemplate::parse(&[])
            .unwrap()
            .render(&item)
            .unwrap(),
        Some(std::path::PathBuf::new())
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
        Some(std::path::PathBuf::from("_CON/_LPT1.jpg"))
    );
}

fn render(template: &str, item: &Media) -> gallery_dl_rs::Result<String> {
    FilenameTemplate::parse(template)
        .unwrap_or_else(|error| panic!("{template:?}: {error}"))
        .render(item)
}

#[test]
fn gallery_dl_conversions_slices_and_alternatives() {
    let mut item = media();
    item.metadata.as_mut().unwrap().caption = Some("  Hello <b>World</b> &amp; more  ".into());
    for (template, expected) in [
        ("{username!u}", "ALICE"),
        ("{username!c}", "Alice"),
        ("{username[1:3]}", "li"),
        ("{username[-2:]}", "ce"),
        ("{username[0]}", "a"),
        ("{caption!t}", "Hello__b_World__b___amp__more"),
        ("{caption!H}", "Hello_World___more"),
        ("{caption!g}", "hello-bworldb-amp-more"),
        ("{caption!W:L11/short/}", "short"),
        ("{caption!W:L40/~/}", "Hello__b_World__b___amp__more"),
        ("{caption:CHl/}", "hello_world___more"),
        ("{source_id|username}", "alice"),
        ("{shortcode|username}", "BA"),
        ("{'literal'}", "literal"),
        ("{source_username|'nobody'}", "nobody"),
        ("{username:Rl/L/}", "aLice"),
        ("{index:A+10/}", "12"),
        ("{index:A*3/>04}", "0006"),
        ("{index:>4}", "___2"),
        ("{index:^5}", "__2__"),
        ("{index:*<4}", "2___"),
        ("{index:+}", "_2"),
        ("{index:#06x}", "0x0002"),
        ("{index:b}", "10"),
        ("{post_id:,}", "123"),
        ("{timestamp:,}", "123_456"),
        ("{timestamp:_x}", "1_e240"),
        ("{index:.2f}", "2.00"),
        ("{index:.1%}", "200.0_"),
        ("{index:e}", "2.000000e_00"),
        ("{username:>8.3}", "_____ali"),
        ("{username:x<6}", "alicex"),
        ("{username!r}", "_alice_"),
        ("{username!j}", "_alice_"),
    ] {
        assert_eq!(render(template, &item).unwrap(), expected, "{template}");
    }
    let rendered = render("{caption!S:L10/long/}.jpg", &item).unwrap();
    assert_eq!(rendered, "long.jpg");
    assert!(render("{username:d}", &item).is_err());
    assert!(render("{index:s}", &item).is_err());
    assert!(render("{username:A+1/}", &item).is_err());
}

#[test]
fn optional_fields_render_empty_without_hiding_required_ones() {
    let mut item = media();
    assert_eq!(
        render("{username}{community_name:?[/]/}_{index}", &item).unwrap(),
        "alice_2"
    );
    assert_eq!(
        render("{username:?[/]/}.jpg", &item).unwrap(),
        "_alice_.jpg"
    );
    item.metadata.as_mut().unwrap().username = None;
    assert_eq!(render("x{username:?@/@/}.jpg", &item).unwrap(), "x.jpg");
    assert!(render("{username:L5/~/}.jpg", &item).is_err());
    assert!(render("{username!S}.jpg", &item).is_err());
    assert!(render("{source_id|username}", &item).is_err());
    item.metadata.as_mut().unwrap().index = 0;
    assert_eq!(render("p{index:?_//}", &item).unwrap(), "p");
    assert_eq!(render("p{index:?_//>03}", &item).unwrap(), "p");
    item.metadata.as_mut().unwrap().index = 7;
    assert_eq!(render("p{index:?_//>03}", &item).unwrap(), "p_007");
}

#[test]
fn dates_use_upstream_datetime_formatting() {
    let mut item = media();
    item.metadata.as_mut().unwrap().timestamp = Some(1_234_567_890);
    for (template, expected) in [
        ("{date}", "2009-02-13_23_31_30"),
        ("{date:%Y-%m-%d}", "2009-02-13"),
        ("{date:%Y%m%d_%H%M%S}", "20090213_233130"),
        (
            "{date:%a_%A_%b_%B_%j_%y_%I%p_%w_%%}",
            "Fri_Friday_Feb_February_044_09_11PM_5__",
        ),
        ("{date:%z_%Z}", "_0000_UTC"),
        ("{date:O+02:00/%H%z}", "01_0200"),
        ("{date:O-05:30/%d_%H%M}", "13_1801"),
        ("{timestamp!d:%Y}", "2009"),
        ("{date!T}", "1234567890"),
        ("{date!j}", "_2009-02-13_23_31_30_"),
    ] {
        assert_eq!(render(template, &item).unwrap(), expected, "{template}");
    }
    assert!(render("{date:%Q}", &item).is_err());
    assert!(render("{username:O+01:00/}", &item).is_err());
    item.metadata.as_mut().unwrap().timestamp = None;
    assert!(render("{date:%Y}", &item).is_err());
    assert_eq!(render("x{date:?_/_/%Y}", &item).unwrap(), "x");
}

#[test]
fn nested_metadata_and_lists_are_addressable() {
    let mut item = media();
    item.category = "twitter".into();
    item.metadata.as_mut().unwrap().file = Some(gallery_dl_rs::file_metadata::FileMetadata {
        width: Some(1920),
        height: Some(1080),
        duration: Some(12.5),
        media_type: Some("video".into()),
        sensitive_flags: vec!["adult".into(), "graphic".into()].into(),
        ..Default::default()
    });
    for (template, expected) in [
        (
            "{metadata[file][width]}x{metadata.file.height}",
            "1920x1080",
        ),
        ("{width}x{height}", "1920x1080"),
        ("{duration}", "12.5"),
        ("{duration:.0f}", "12"),
        ("{tweet_id}", "123"),
        ("{retweet_id}", "0"),
        ("{retweet_id:?r//}", ""),
        ("{sensitive_flags}", "__adult____graphic__"),
        ("{sensitive_flags!S}", "adult__graphic"),
        ("{sensitive_flags:J+/}", "adult_graphic"),
        ("{sensitive_flags:Sr/J-/}", "graphic-adult"),
        ("{sensitive_flags[-1]}", "graphic"),
        ("{sensitive_flags[:1]:J/}", "adult"),
        ("{metadata[post_id]}", "123"),
        ("{media_type!u}", "VIDEO"),
    ] {
        let rendered = if template == "{retweet_id:?r//}" {
            render(&format!("x{template}"), &item).map(|v| v[1..].to_string())
        } else {
            render(template, &item)
        };
        assert_eq!(rendered.unwrap(), expected, "{template}");
    }
}

#[test]
fn conditional_templates_select_the_first_matching_filter() {
    let template = FilenameTemplate::parse_conditional(
        &[
            (
                "username == 'bob'".into(),
                "bob_{post_id}.{extension}".into(),
            ),
            ("index >= 2".into(), "late_{index}.{extension}".into()),
            ("username == 'alice'".into(), "alice.{extension}".into()),
        ],
        Some("{filename}"),
    )
    .unwrap();
    let mut item = media();
    assert_eq!(template.render(&item).unwrap(), "late_2.jpg");
    item.metadata.as_mut().unwrap().index = 1;
    assert_eq!(template.render(&item).unwrap(), "alice.jpg");
    item.metadata.as_mut().unwrap().username = Some("carol".into());
    assert_eq!(template.render(&item).unwrap(), "native.photo.jpg");

    // Without a default, unmatched media keep the normal naming policy.
    let bare =
        FilenameTemplate::parse_conditional(&[("index > 5".into(), "x.jpg".into())], None).unwrap();
    assert_eq!(bare.render_optional(&item).unwrap(), None);
    assert!(bare.render(&item).is_err());
    let root = std::path::Path::new("/root");
    assert_eq!(
        gallery_dl_rs::download::destination_with_template(root, &item, &bare).unwrap(),
        gallery_dl_rs::download::destination(root, &item)
    );

    // Missing keys in a condition fail the file, as upstream does.
    let missing =
        FilenameTemplate::parse_conditional(&[("nope == 1".into(), "x.jpg".into())], None).unwrap();
    assert!(missing.render_optional(&item).is_err());
    for bad in [
        ("index ==".to_string(), "x".to_string()),
        ("1".into(), "a/b".into()),
    ] {
        assert!(FilenameTemplate::parse_conditional(&[bad], None).is_err());
    }
}

#[test]
fn conditional_directories_fall_back_to_category_folders() {
    let directory = DirectoryTemplate::parse_conditional(
        &[
            (
                "username == 'alice'".into(),
                vec!["people".into(), "{username}".into()],
            ),
            ("index == 9".into(), vec![]),
        ],
        None,
    )
    .unwrap();
    let mut item = media();
    assert_eq!(
        directory.render(&item).unwrap(),
        Some(std::path::Path::new("people").join("alice"))
    );
    item.metadata.as_mut().unwrap().username = None;
    item.metadata.as_mut().unwrap().index = 9;
    assert_eq!(
        directory.render(&item).unwrap(),
        Some(std::path::PathBuf::new())
    );
    item.metadata.as_mut().unwrap().index = 1;
    assert_eq!(directory.render(&item).unwrap(), None);
    let root = std::path::Path::new("/root");
    assert_eq!(
        gallery_dl_rs::download::destination_with_templates(root, &item, None, Some(&directory))
            .unwrap(),
        gallery_dl_rs::download::destination(root, &item)
    );
}
