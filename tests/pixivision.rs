use gallery_dl_rs::{
    event::Post,
    extract::{self, Media},
    filter::FilterExpression,
    naming::FilenameTemplate,
    pixiv::{self, PixivisionMetadata, Target},
};
use serde_json::{Value, json};
use std::sync::Arc;

fn compat(article: Option<Value>) -> Media {
    let mut data = json!({"id":42,"num":0,"filename":"42_p0","extension":"jpg","title":"Work title","caption":"<p>Caption</p>"});
    if let Some(article) = article {
        if let Some(id) = article.get("id") {
            data["pixivision_id"] = id.clone();
        }
        if let Some(title) = article.get("title") {
            data["pixivision_title"] = title.clone();
        }
    }
    extract::parse_messages(
        &serde_json::to_vec(&json!([[3, "https://i.pximg.net/42_p0.jpg", data]])).unwrap(),
        "pixiv",
    )
    .unwrap()
    .remove(0)
}
fn article() -> Value {
    json!({"id":"77","title":"Article title"})
}

#[test]
fn pixivision_routes_and_decoded_metadata_match_recorded_upstream() {
    let reference: Value =
        serde_json::from_str(include_str!("fixtures/pixiv/pixivision.json")).unwrap();
    let url = extract::http_url(reference["url"].as_str().unwrap()).unwrap();
    assert_eq!(
        pixiv::target(&url).unwrap(),
        Target::Pixivision("77".into())
    );
    assert_eq!(extract::social_site(&url), Some("pixiv"));
    let media = compat(Some(
        json!({"id":reference["metadata"]["pixivision_id"],"title":reference["metadata"]["pixivision_title"]}),
    ));
    let post = media.metadata.as_ref().unwrap();
    let article = post.pixivision.as_ref().unwrap();
    assert_eq!(article.id, "77");
    assert_eq!(
        article.title.as_ref(),
        reference["metadata"]["pixivision_title"].as_str().unwrap()
    );
    assert_eq!(post.caption.as_deref(), Some("Work title\n\nCaption"));
}

#[test]
fn article_archives_include_article_and_artwork_page_but_ignore_title() {
    let media = compat(Some(article()));
    let key = media.archive_key().unwrap();
    assert_ne!(Some(key.clone()), compat(None).archive_key());
    let mut native = compat(None);
    native.metadata.as_mut().unwrap().pixivision = Some(Arc::new(PixivisionMetadata {
        id: "77".into(),
        title: "Changed title".into(),
    }));
    assert_eq!(Some(key.clone()), native.archive_key());
    assert_ne!(
        Some(key.clone()),
        compat(Some(json!({"id":"78","title":"Article title"}))).archive_key()
    );
    native.metadata.as_mut().unwrap().media_id = "42_p1".into();
    assert_ne!(Some(key.clone()), native.archive_key());
    assert!(!key.contains("Article title"));
    assert!(
        compat(Some(json!({"id":"77","title":""})))
            .archive_key()
            .is_some()
    );
}

#[test]
fn malformed_or_conflicting_article_scope_cannot_use_other_archive_namespaces() {
    for value in [
        json!({"id":null,"title":"PRIVATE"}),
        json!({"title":"PRIVATE"}),
        json!({"id":"PRIVATE","title":""}),
        json!({"id":"77","title":{"private":"PRIVATE"}}),
    ] {
        let media = compat(Some(value));
        assert!(media.metadata.as_ref().unwrap().pixivision.is_none());
        assert!(media.archive_key().is_none());
        assert!(!serde_json::to_string(&media).unwrap().contains("PRIVATE"));
    }
    for kind in ["search", "ranking", "followed"] {
        let mut media = compat(Some(article()));
        let post = media.metadata.as_mut().unwrap();
        match kind {
            "search" => post.search_archive = true,
            "ranking" => post.ranking_archive = true,
            _ => post.followed_archive = true,
        }
        assert!(media.archive_key().is_none());
    }
}

#[test]
fn article_fields_support_source_aliases_in_filters_and_templates() {
    let media = compat(Some(article()));
    let filter = FilterExpression::parse("pixivision_id == 77 and pixivision_title == 'Article title' and pixivision['id'] == pixivision_id and pixivision['title'] == pixivision_title").unwrap();
    assert!(filter.matches(&media).unwrap());
    assert!(
        filter
            .matches_post(&Post::from_media(std::slice::from_ref(&media), "pixiv"))
            .unwrap()
    );
    assert!(
        FilterExpression::parse("metadata['pixivision_id'] == pixivision_id")
            .unwrap()
            .matches(&media)
            .unwrap()
    );
    assert_eq!(
        FilenameTemplate::parse("{pixivision_id}_{id}.{extension}")
            .unwrap()
            .render(&media)
            .unwrap(),
        "77_42.jpg"
    );
    let public = serde_json::to_value(&media).unwrap();
    assert_eq!(public["metadata"]["pixivision"], article());
    assert!(public["metadata"].get("pixivision_archive").is_none());
}
