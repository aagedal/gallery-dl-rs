use gallery_dl_rs::{
    download::{self, DownloadOptions, Outcome},
    extract::{self, Media},
    naming::{DirectoryTemplate, FilenameTemplate},
    twitter,
};
use serde_json::{Value, json};
use std::time::Duration;
const ID: &str = "1234567890";
fn community() -> Value {
    json!({"id_str":"55","rest_id":"55","name":"Art / Cats", "description":"A public description",
        "created_at":1700000000123u64,"is_nsfw":false,"role":"Member","member_count":120,
        "rules":[{"name":"Be kind"},{"name":"Share art"}],
        "admin_results":{"result":{"core":{"screen_name":"admin"}}},
        "creator_results":{"result":{"core":{"screen_name":"creator"}}},
        "custom_banner_media":{"media_info":{"original_img_url":"https://cdn.example/banner?token=BANNER_SECRET"}},
        "private_unknown":"UNKNOWN_SECRET"})
}
fn native(value: Value) -> Vec<Media> {
    let mut data: Value =
        serde_json::from_slice(include_bytes!("../fixtures/twitter/photo.json")).unwrap();
    data["data"]["tweetResult"]["result"]["author_community_relationship"] =
        json!({"community_results":{"result":value}});
    twitter::parse_response(&serde_json::to_vec(&data).unwrap(), ID).unwrap()
}
fn compatibility(subcategory: &str, cid: Value) -> Media {
    extract::parse_messages(&serde_json::to_vec(&json!([[3,"https://cdn.example/photo.jpg",{
        "tweet_id":ID,"num":1,"filename":"photo","extension":"jpg", "subcategory":subcategory,
        "community":{"id":cid,"name":"Art / Cats","description":"A public description", "date":"2023-11-14 22:13:20",
            "nsfw":false,"role":"Member","member_count":120,"rules":["Be kind","Share art"],
            "admin":"admin","creator":"creator","banner":"https://cdn.example/banner?token=BANNER_SECRET",
            "private_unknown":"UNKNOWN_SECRET"}}]])).unwrap(), "twitter").unwrap().remove(0)
}

#[test]
fn native_community_metadata_is_allowlisted_and_does_not_change_ordinary_archives() {
    let item = native(community()).remove(0);
    let metadata = item.metadata.as_ref().unwrap();
    assert!(!metadata.community_archive);
    let parsed = metadata.community.as_ref().unwrap();
    assert_eq!(parsed.id, "55");
    assert_eq!(parsed.name.as_deref(), Some("Art / Cats"));
    assert_eq!(parsed.date, Some(1700000000));
    assert_eq!(parsed.member_count, Some(120));
    assert_eq!(parsed.nsfw, Some(false));
    assert_eq!(
        parsed.rules.iter().map(|s| s.as_ref()).collect::<Vec<_>>(),
        ["Be kind", "Share art"]
    );
    assert_eq!(parsed.admin.as_deref(), Some("admin"));
    assert_eq!(parsed.creator.as_deref(), Some("creator"));
    assert!(
        !serde_json::to_string(&item)
            .unwrap()
            .contains("UNKNOWN_SECRET")
    );
    let mut without = item.clone();
    without.metadata.as_mut().unwrap().community = None;
    assert_eq!(item.archive_key(), without.archive_key());
    assert_eq!(item.identity(), without.identity());
    assert_eq!(item.filename, without.filename);
}

#[test]
fn malformed_optional_community_metadata_never_replaces_post_identity() {
    for value in [
        Value::Null,
        json!({}),
        json!({"id_str":"0"}),
        json!({"id_str":"055"}),
        json!({"id_str":"55","rest_id":"56"}),
        json!({"id_str":true}),
        json!({"id_str":"PRIVATE_INVALID"}),
    ] {
        let item = native(value).remove(0);
        assert!(item.metadata.as_ref().unwrap().community.is_none());
        assert_eq!(item.metadata.as_ref().unwrap().post_id, ID);
        assert!(item.archive_key().is_some());
    }
    let item = native(json!({"rest_id":"55","rules":[{},null,{"name":false},{"name":"One rule"}],
        "admin_results":false,"custom_banner_media":{"media_info":{"original_img_url":"file:///PRIVATE"}}})).remove(0);
    let metadata = item.metadata.unwrap().community.unwrap();
    assert_eq!(metadata.rules.len(), 1);
    assert!(metadata.banner.is_none());
    assert!(metadata.admin.is_none());
}

#[test]
fn community_feed_archive_namespaces_are_explicit_stable_and_shared_between_backends() {
    let ordinary = compatibility("tweet", json!(55));
    let first = compatibility("community", json!(55));
    let second = compatibility("communities", json!("55"));
    let other = compatibility("community", json!(56));
    assert!(first.archive_key().is_some());
    assert_eq!(first.archive_key(), second.archive_key());
    assert_ne!(first.archive_key(), ordinary.archive_key());
    assert_ne!(first.archive_key(), other.archive_key());
    assert_eq!(first.identity(), ordinary.identity());
    let mut native = native(community()).remove(0);
    native.metadata.as_mut().unwrap().community_archive = true;
    assert_eq!(native.archive_key(), first.archive_key());
    for invalid in [
        json!(0),
        json!("055"),
        json!("PRIVATE_INVALID"),
        json!(false),
        Value::Null,
    ] {
        let item = compatibility("community", invalid);
        assert!(item.metadata.as_ref().unwrap().community.is_none());
        assert!(item.archive_key().is_none());
    }
    let parsed = first.metadata.unwrap().community.unwrap();
    assert_eq!(parsed.date, Some(1700000000));
    assert_eq!(parsed.rules.len(), 2);
}

#[test]
fn community_template_fields_are_sanitized_and_require_metadata() {
    let item = native(community()).remove(0);
    let template =
        FilenameTemplate::parse("{community_id}_{community_name}_{post_id}.{extension}").unwrap();
    assert_eq!(
        template.render(&item).unwrap(),
        "55_Art___Cats_1234567890.jpg"
    );
    let directory = DirectoryTemplate::parse(&[
        "Communities".into(),
        "{community_name}_{community_id}".into(),
    ])
    .unwrap();
    assert_eq!(
        directory.render(&item).unwrap(),
        Some(std::path::Path::new("Communities").join("Art___Cats_55"))
    );
    let mut absent = item;
    absent.metadata.as_mut().unwrap().community = None;
    assert!(template.render(&absent).is_err());
    assert!(directory.render(&absent).is_err());
}

#[tokio::test]
async fn community_sidecars_keep_public_descriptions_without_banner_urls_or_archive_context() {
    let item = native(community()).remove(0);
    let root = tempfile::tempdir().unwrap();
    let path = download::destination(root.path(), &item);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"already downloaded").unwrap();
    let client = download::client(Duration::from_secs(1)).unwrap();
    let outcome = download::download_with_options(
        &client,
        root.path(),
        &item,
        DownloadOptions {
            write_metadata: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(outcome, Outcome::Skipped(path.clone()));
    let bytes = std::fs::read(format!("{}.json", path.display())).unwrap();
    let text = String::from_utf8(bytes).unwrap();
    assert!(!text.contains("SECRET"));
    assert!(!text.contains("banner"));
    assert!(!text.contains("community_archive"));
    let data: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(data["metadata"]["community"]["id"], "55");
    assert_eq!(data["metadata"]["community"]["name"], "Art / Cats");
    assert!(item.metadata.unwrap().community.unwrap().banner.is_some());
}

#[test]
fn repost_and_original_modes_use_their_selected_posts_community_relationship() {
    use twitter::{ExpansionOptions, RetweetMode};
    let mut data: Value =
        serde_json::from_slice(include_bytes!("../fixtures/twitter/photo.json")).unwrap();
    let mut original = data["data"]["tweetResult"]["result"].clone();
    original["rest_id"] = json!("2345678901");
    original["legacy"]["id_str"] = json!("2345678901");
    original["author_community_relationship"] =
        json!({"community_results":{"result":{"id_str":"66","name":"Original"}}});
    let outer = &mut data["data"]["tweetResult"]["result"];
    outer["legacy"]["retweeted_status_id_str"] = json!("2345678901");
    outer["retweeted_status_result"] = json!({"result":original});
    for relationship in [
        Value::Null,
        json!({"community_results":{"result":{"id_str":"55","name":"Repost"}}}),
    ] {
        data["data"]["tweetResult"]["result"]["author_community_relationship"] =
            relationship.clone();
        for (mode, expected) in [
            (
                RetweetMode::Repost,
                if relationship.is_null() {
                    None
                } else {
                    Some("55")
                },
            ),
            (RetweetMode::Original, Some("66")),
        ] {
            let items = twitter::parse_response_with_options(
                &serde_json::to_vec(&data).unwrap(),
                ID,
                ExpansionOptions {
                    retweets: mode,
                    quoted: false,
                },
            )
            .unwrap();
            assert_eq!(
                items[0]
                    .metadata
                    .as_ref()
                    .unwrap()
                    .community
                    .as_ref()
                    .map(|value| value.id.as_str()),
                expected
            );
        }
    }
}
