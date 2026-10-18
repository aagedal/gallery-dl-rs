use gallery_dl_rs::facebook::{parse_photo, photo_id};
use serde_json::json;
use url::Url;

const ID: &str = "10160743390456729";
const PHOTO: &[u8] = include_bytes!("fixtures/facebook/photo.html");

#[test]
fn parses_identity_bound_photo_and_archive_metadata() {
    let media = parse_photo(PHOTO, ID).unwrap().remove(0);
    assert_eq!(
        media.url.as_str(),
        "https://cdn.example.test/full.jpg?sig=synthetic"
    );
    assert_eq!(media.filename, format!("{ID}.jpg"));
    assert_eq!(media.category, "facebook");
    let post = media.metadata.as_ref().unwrap();
    assert_eq!(post.post_id, ID);
    assert_eq!(post.media_id, format!("{ID}.jpg"));
    assert_eq!(post.username.as_deref(), Some("facebook"));
    assert_eq!(post.caption.as_deref(), Some("Photo 📸 caption"));
    assert_eq!(post.timestamp, Some(1622152519));
    assert!(media.archive_key().unwrap().starts_with("facebook:v1:"));
    assert!(!media.headers.contains_key("Cookie"));
}

#[test]
fn accepts_upstream_single_photo_urls() {
    for raw in [
        format!("https://www.facebook.com/photo/?fbid={ID}"),
        format!("https://facebook.com/photo.php/?fbs=home&fbid={ID}&set=t.123"),
        format!("https://m.facebook.com/photo?fbid={ID}"),
        format!("https://www.facebook.com/Facebook/photos/a.123/{ID}/"),
    ] {
        assert_eq!(photo_id(&Url::parse(&raw).unwrap()).unwrap(), ID);
    }
}

#[test]
fn rejects_collections_ambiguous_ids_and_foreign_urls() {
    for raw in [
        "https://facebook.com/media/set/?set=a.123",
        "https://facebook.com/facebook/photos",
        "https://facebook.com/photo/?fbid=1&set=a.123&setextract",
        "https://facebook.com/photo/?fbid=1&fbid=2",
        "https://facebook.com/photo/?fbid=abc",
        "https://facebook.com.evil.test/photo/?fbid=1",
        "https://user:secret@facebook.com/photo/?fbid=1",
        "https://facebook.com:444/photo/?fbid=1",
        "https://facebook.com/photo/?fbid=1%2f2",
        "https://facebook.com/photo/1",
    ] {
        assert!(photo_id(&Url::parse(raw).unwrap()).is_err(), "{raw}");
    }
}

#[test]
fn never_uses_global_images_preview_comments_or_different_photo() {
    for value in [
        json!({"image":{"uri":"https://example.test/preview.jpg"}}),
        json!({"__typename":"Photo","id":"999","image":{"uri":"https://example.test/wrong.jpg"}}),
        json!({"__typename":"Photo","id":ID,"comments":{"image":{"uri":"https://example.test/comment.jpg"}}}),
        json!({"id":ID,"image":{"uri":"https://example.test/no-type.jpg"}}),
    ] {
        assert!(parse_photo(&serde_json::to_vec(&value).unwrap(), ID).is_err());
    }
    assert!(parse_photo(PHOTO, "99999").is_err());
    assert!(
        parse_photo(
            b"<meta property='og:image' content='https://example.test/p.jpg'>",
            ID
        )
        .is_err()
    );
}

#[test]
fn handles_explicit_block_auth_and_missing_responses() {
    for (body, diagnostic) in [
        ("{\"__dr\":\"CometErrorRoot.react\"}", "blocked"),
        ("<form id=\"login_form\">", "authentication"),
        ("{\"checkpoint_url\":\"secret\"}", "authentication"),
        ("<html>Page Not Found</html>", "unavailable"),
    ] {
        let error = parse_photo(body.as_bytes(), ID).unwrap_err().to_string();
        assert!(error.contains(diagnostic), "{error}");
        assert!(!error.contains("secret"));
    }
}

#[test]
fn selects_largest_identity_bound_image_and_rejects_ambiguity() {
    let node = |name: &str, width: u64| json!({"__isNode":"Photo","id":ID,"image":{"uri":format!("https://example.test/{name}.png"),"width":width,"height":10},"owner":{"name":"Display Name"}});
    let bytes = serde_json::to_vec(&json!([node("small", 10), node("large", 20)])).unwrap();
    let media = parse_photo(&bytes, ID).unwrap().remove(0);
    assert!(media.url.path().ends_with("large.png"));
    assert!(media.metadata.unwrap().username.is_none());
    let bytes = serde_json::to_vec(&json!([node("one", 20), node("two", 20)])).unwrap();
    assert!(
        parse_photo(&bytes, ID)
            .unwrap_err()
            .to_string()
            .contains("ambiguous")
    );
}

#[test]
fn rejects_invalid_media_without_echoing_private_urls() {
    for uri in [
        "https://user:secret@example.test/full.jpg",
        "file:///secret.jpg",
        "https://example.test/secret.m3u8",
    ] {
        let bytes =
            serde_json::to_vec(&json!({"__isNode":"Photo","id":ID,"image":{"uri":uri}})).unwrap();
        let error = parse_photo(&bytes, ID).unwrap_err().to_string();
        assert!(!error.contains("secret"));
    }
}

#[test]
fn module_names_and_photo_captions_are_not_global_block_markers() {
    let bytes = serde_json::to_vec(&json!({
        "modules":["CometErrorRoot.react"],
        "node":{"__isNode":"Photo","id":ID,"image":{"uri":"https://example.test/p.jpg"},
            "message":{"text":"CometErrorRoot.react login_form checkpoint_url"}}
    }))
    .unwrap();
    assert_eq!(parse_photo(&bytes, ID).unwrap().len(), 1);
    let error = parse_photo(br#"{"modules":["CometErrorRoot.react"]}"#, ID)
        .unwrap_err()
        .to_string();
    assert!(error.contains("unavailable"));
    assert!(!error.contains("blocked"));
}
