use gallery_dl_rs::{
    extract::{self, Media},
    pixiv::{self, ProfileKind},
};
use serde_json::json;

fn native(kind: ProfileKind) -> Media {
    pixiv::parse_profile(&serde_json::to_vec(&json!({
        "user":{"id":9,"account":"alice","profile_image_urls":{"medium":"https://i.pximg.net/user-profile/img/2024/01/01/00/00/00/9_170.jpg?token=private"}},
        "profile":{"background_image_url":"https://i.pximg.net/c/1200x1200_90_webp/background/img/2024/01/01/00/00/00/9_master1200.jpg?token=private"}
    })).unwrap(), "9", kind).unwrap().remove(0)
}

#[test]
fn pixiv_profile_archives_match_compatibility_and_separate_kind_owner_version() {
    for (kind, label) in [
        (ProfileKind::Avatar, "avatar"),
        (ProfileKind::Background, "background"),
    ] {
        let native = native(kind);
        let metadata = json!({"subcategory":label,"id":label,"num":0,"filename":label,"extension":"jpg","date":"2023-12-31T15:00:00+00:00","user":{"id":9,"account":"alice"}});
        let parse = |data| {
            extract::parse_messages(
                &serde_json::to_vec(&json!([[3, native.url, data]])).unwrap(),
                "pixiv",
            )
            .unwrap()
            .remove(0)
        };
        let compatible = parse(metadata.clone());
        assert!(native.archive_key().is_some());
        assert_eq!(native.archive_key(), compatible.archive_key());
        assert_eq!(
            native.metadata.as_ref().unwrap().media_id,
            compatible.metadata.as_ref().unwrap().media_id
        );
        assert!(gallery_dl_rs::filter::FilterExpression::parse(&format!("profile_asset['kind'] == '{label}' and metadata['profile_asset']['owner_id'] == '9'")).unwrap().matches(&compatible).unwrap());
        assert_eq!(
            gallery_dl_rs::naming::FilenameTemplate::parse(
                "{profile_asset[kind]}_{profile_asset[owner_id]}.{extension}"
            )
            .unwrap()
            .render(&compatible)
            .unwrap(),
            format!("{label}_9.jpg")
        );
        let serialized = serde_json::to_string(&compatible.metadata).unwrap();
        assert!(!serialized.contains("private"));
        assert!(serialized.contains("profile_asset"));
        for (key, value) in [
            ("date", json!("2024-01-01T15:00:00+00:00")),
            ("user", json!({"id":10})),
            ("id", json!("42")),
        ] {
            let mut changed = metadata.clone();
            changed[key] = value;
            assert_ne!(native.archive_key(), parse(changed).archive_key());
        }
        let mut invalid = metadata.clone();
        invalid["user"] = json!({"id":"bad/owner"});
        assert_eq!(parse(invalid).archive_key(), None);
        let mut unsigned = native.clone();
        unsigned.url.set_query(None);
        assert_eq!(native.archive_key(), unsigned.archive_key());
    }
    assert_ne!(
        native(ProfileKind::Avatar).archive_key(),
        native(ProfileKind::Background).archive_key()
    );
}

#[test]
fn malformed_profile_archive_identity_never_becomes_an_ordinary_pixiv_identity() {
    let mut image = native(ProfileKind::Avatar);
    for version in ["", "private/path", "version?token=private"] {
        image
            .metadata
            .as_mut()
            .unwrap()
            .profile_asset
            .as_mut()
            .unwrap()
            .version = version.into();
        assert_eq!(image.archive_key(), None);
    }
    image.metadata.as_mut().unwrap().profile_asset = None;
    assert_eq!(image.archive_key(), None);
}
