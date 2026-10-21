use gallery_dl_rs::{
    event::Post,
    extract::{self, Media},
    filter::FilterExpression,
    naming::FilenameTemplate,
    pixiv::{self, FollowedUserMetadata, Target},
};
use serde_json::{Value, json};
use std::sync::Arc;

fn owner() -> Value {
    json!({"id":17,"name":"Follower","account":"follower"})
}
fn compat(owner: Option<Value>) -> Media {
    let mut data = json!({"id":42,"num":0,"filename":"42_p0","extension":"jpg","title":"Title","caption":"<p>Caption</p>"});
    if let Some(owner) = owner {
        data["user_follow"] = owner;
    }
    extract::parse_messages(
        &serde_json::to_vec(&json!([[3, "https://i.pximg.net/42_p0.jpg", data]])).unwrap(),
        "pixiv",
    )
    .unwrap()
    .remove(0)
}

#[test]
fn followed_metadata_matches_recorded_initial_owner_without_private_oauth_fields() {
    let reference: Value =
        serde_json::from_str(include_str!("fixtures/pixiv/followed.json")).unwrap();
    let url = extract::http_url(reference["url"].as_str().unwrap()).unwrap();
    assert_eq!(pixiv::target(&url).unwrap(), Target::Followed);
    assert_eq!(extract::social_site(&url), Some("pixiv"));
    for work in reference["works"].as_array().unwrap() {
        let media = compat(Some(work["user_follow"].clone()));
        let owner = media
            .metadata
            .as_ref()
            .unwrap()
            .user_follow
            .as_ref()
            .unwrap();
        assert_eq!(owner.id, "17");
        assert_eq!(owner.name.as_deref(), Some("Follower"));
        assert_eq!(owner.account.as_deref(), Some("follower"));
        assert_eq!(
            media.metadata.as_ref().unwrap().caption.as_deref(),
            Some("Title\n\nCaption")
        );
        let public = serde_json::to_string(&media).unwrap();
        assert!(!public.contains("PRIVATE"));
        assert!(!public.contains("mail_address"));
        assert!(!public.contains("followed_archive"));
    }
}

#[test]
fn followed_archives_include_owner_and_artwork_page_but_ignore_owner_display_fields() {
    let media = compat(Some(owner()));
    let key = media.archive_key().unwrap();
    assert_ne!(Some(key.clone()), compat(None).archive_key());
    let mut native = compat(None);
    native.metadata.as_mut().unwrap().user_follow = Some(Arc::new(FollowedUserMetadata {
        id: "17".into(),
        name: Some("Renamed".into()),
        account: None,
    }));
    assert_eq!(Some(key.clone()), native.archive_key());
    assert_ne!(
        Some(key.clone()),
        compat(Some(json!({"id":99}))).archive_key()
    );
    native.metadata.as_mut().unwrap().media_id = "42_p1".into();
    assert_ne!(Some(key.clone()), native.archive_key());
    assert!(!key.contains("Follower"));
    assert!(!key.contains("follower"));
}

#[test]
fn malformed_or_conflicting_followed_context_cannot_fall_back_to_other_archive_namespaces() {
    for owner in [
        Value::Null,
        json!({}),
        json!("PRIVATE"),
        json!({"id":"PRIVATE"}),
        json!({"id":0}),
    ] {
        let media = compat(Some(owner));
        assert!(media.metadata.as_ref().unwrap().user_follow.is_none());
        assert!(media.archive_key().is_none());
        assert!(!serde_json::to_string(&media).unwrap().contains("PRIVATE"));
    }
    for kind in ["search", "ranking"] {
        let mut media = compat(Some(owner()));
        if kind == "search" {
            media.metadata.as_mut().unwrap().search_archive = true;
        } else {
            media.metadata.as_mut().unwrap().ranking_archive = true;
        }
        assert!(media.archive_key().is_none());
    }
}

#[test]
fn followed_owner_is_available_to_post_and_file_filters_and_templates() {
    let media = compat(Some(owner()));
    let filter = FilterExpression::parse("user_follow['id'] == 17 and user_follow['name'] == 'Follower' and user_follow['account'] == 'follower'").unwrap();
    assert!(filter.matches(&media).unwrap());
    assert!(
        filter
            .matches_post(&Post::from_media(std::slice::from_ref(&media), "pixiv"))
            .unwrap()
    );
    assert!(
        FilterExpression::parse("metadata['user_follow']['id'] == user_follow['id']")
            .unwrap()
            .matches(&media)
            .unwrap()
    );
    assert_eq!(
        FilenameTemplate::parse("{user_follow[id]}_{id}.{extension}")
            .unwrap()
            .render(&media)
            .unwrap(),
        "17_42.jpg"
    );
}
