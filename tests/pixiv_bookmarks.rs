use gallery_dl_rs::{
    event::Post,
    extract::{self, Media},
    filter::FilterExpression,
    naming::FilenameTemplate,
};
use serde_json::{Value, json};

fn media(owner: Option<Value>) -> Media {
    let mut data = json!({"id":42,"num":0,"filename":"42_p0","extension":"jpg","user":{"id":9,"account":"original-artist"},"title":"Work title","caption":"<p>Caption</p>"});
    if let Some(owner) = owner {
        data["user_bookmark"] = owner;
    }
    from_data(data, "pixiv")
}
fn from_data(data: Value, category: &str) -> Media {
    extract::parse_messages(
        &serde_json::to_vec(&json!([[
            3,
            "https://i.pximg.net/img-original/42_p0.jpg",
            data
        ]]))
        .unwrap(),
        category,
    )
    .unwrap()
    .remove(0)
}
fn owner() -> Value {
    json!({"id":17,"name":"Owner","account":"owner-account"})
}

#[test]
fn bookmark_archives_use_owner_artwork_page_and_extension_without_display_fields() {
    let item = media(Some(owner()));
    let key = item.archive_key().unwrap();
    assert_ne!(Some(key.clone()), media(None).archive_key());
    assert_eq!(
        Some(key.clone()),
        media(Some(
            json!({"id":17,"name":"Changed name","account":"new-account"})
        ))
        .archive_key()
    );
    assert_ne!(
        Some(key.clone()),
        media(Some(json!({"id":18}))).archive_key()
    );
    let mut page = item.clone();
    page.metadata.as_mut().unwrap().media_id = "42_p1".into();
    assert_ne!(Some(key.clone()), page.archive_key());
    let mut zip = item.clone();
    zip.filename = "42_p0.zip".into();
    assert_ne!(Some(key.clone()), zip.archive_key());
    let mut unflagged = item.clone();
    unflagged.metadata.as_mut().unwrap().bookmark_archive = false;
    assert_eq!(Some(key.clone()), unflagged.archive_key());
    assert!(!key.contains("Owner") && !key.contains("owner-account"));
}

#[test]
fn declared_invalid_or_conflicting_bookmark_scope_never_uses_another_archive_namespace() {
    for value in [
        Value::Null,
        json!({"id":0,"name":"PRIVATE"}),
        json!({"id":"PRIVATE"}),
        json!({"name":"PRIVATE"}),
    ] {
        let item = media(Some(value));
        assert!(item.metadata.as_ref().unwrap().bookmark_archive);
        assert!(item.metadata.as_ref().unwrap().user_bookmark.is_none());
        assert!(item.archive_key().is_none());
        assert!(!serde_json::to_string(&item).unwrap().contains("PRIVATE"));
    }
    for subtype in ["favorite", "bookmark"] {
        let item = from_data(
            json!({"id":42,"num":0,"filename":"42_p0","extension":"jpg","subcategory":subtype}),
            "pixiv",
        );
        assert!(item.metadata.as_ref().unwrap().bookmark_archive);
        assert!(item.archive_key().is_none());
    }
    for conflict in ["search", "ranking", "followed", "pixivision", "sketch"] {
        let mut item = media(Some(owner()));
        let post = item.metadata.as_mut().unwrap();
        match conflict {
            "search" => post.search_archive = true,
            "ranking" => post.ranking_archive = true,
            "followed" => post.followed_archive = true,
            "pixivision" => post.pixivision_archive = true,
            _ => post.sketch_archive = true,
        }
        assert!(item.archive_key().is_none());
    }
}

#[test]
fn bookmark_owner_is_separate_from_artist_and_shared_across_post_and_file_contexts() {
    let item = media(Some(
        json!({"id":17,"name":"Owner","account":"owner-account","mail":"PRIVATE_MAIL","access_token":"PRIVATE_TOKEN"}),
    ));
    let metadata = item.metadata.as_ref().unwrap();
    assert_eq!(metadata.username.as_deref(), Some("original-artist"));
    assert_eq!(metadata.caption.as_deref(), Some("Work title\n\nCaption"));
    let owner = metadata.user_bookmark.as_ref().unwrap();
    let post = Post::from_media(std::slice::from_ref(&item), "pixiv");
    assert!(std::sync::Arc::ptr_eq(
        owner,
        post.metadata
            .as_ref()
            .unwrap()
            .user_bookmark
            .as_ref()
            .unwrap()
    ));
    let filter=FilterExpression::parse("user_bookmark['id'] == 17 and user_bookmark['account'] == 'owner-account' and username == 'original-artist' and metadata['user_bookmark']['name'] == 'Owner'").unwrap();
    assert!(filter.matches(&item).unwrap());
    assert!(filter.matches_post(&post).unwrap());
    assert_eq!(
        FilenameTemplate::parse("{user_bookmark[id]}_{user_bookmark[account]}_{id}.{extension}")
            .unwrap()
            .render(&item)
            .unwrap(),
        "17_owner-account_42.jpg"
    );
    let public = serde_json::to_string(&item).unwrap();
    assert!(!public.contains("PRIVATE") && !public.contains("bookmark_archive"));
}

#[test]
fn recorded_source_bookmark_owners_and_archive_equivalences_match_compatibility() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/pixiv/bookmark_owner.json")).unwrap();
    let mut keys = std::collections::BTreeMap::new();
    for case in fixture["cases"].as_array().unwrap() {
        let category = if case["url"].as_str().unwrap().contains("/bookmarks/novels") {
            "pixiv-novel"
        } else {
            "pixiv"
        };
        for event in case["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["kind"] == "media")
        {
            let item = from_data(event["metadata"].clone(), category);
            if category == "pixiv" {
                let expected = &event["metadata"]["user_bookmark"];
                let metadata = item.metadata.as_ref().unwrap();
                let owner = metadata.user_bookmark.as_ref().unwrap();
                let scalar = |v: &Value| {
                    v.as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| v.to_string())
                };
                assert_eq!(owner.id, scalar(&expected["id"]));
                assert_eq!(owner.name.as_deref(), expected["name"].as_str());
                assert_eq!(owner.account.as_deref(), expected["account"].as_str());
                let source = event["archive"].as_str().unwrap().to_owned();
                let key = item.archive_key().unwrap();
                if let Some(previous) = keys.insert(source, key.clone()) {
                    assert_eq!(previous, key);
                }
            } else {
                assert!(item.metadata.as_ref().unwrap().user_bookmark.is_none());
                assert!(!item.metadata.as_ref().unwrap().bookmark_archive);
                let mut ordinary = item.clone();
                ordinary.metadata.as_mut().unwrap().bookmark_archive = false;
                assert_eq!(item.archive_key(), ordinary.archive_key());
            }
        }
    }
}
