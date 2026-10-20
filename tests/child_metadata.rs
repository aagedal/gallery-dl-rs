use futures_util::{StreamExt, stream};
use gallery_dl_rs::{
    event::{ChildControl, ChildMetadata, ChildScope, ExtractionEvent, media_only},
    extract,
    filter::FilterExpression,
};
use serde_json::{Value, json};

fn metadata(value: Value) -> ChildMetadata {
    ChildMetadata::from_raw("kemono", "favorite", value.as_object().unwrap())
}

fn matches(expression: &str, child: &ChildMetadata) -> bool {
    FilterExpression::parse(expression)
        .unwrap_or_else(|error| panic!("parse {expression}: {error}"))
        .matches_child(child)
        .unwrap_or_else(|error| panic!("evaluate {expression}: {error}"))
}

#[test]
fn child_filter_preserves_scalar_types_and_raw_dates_without_identity_validation() {
    let child = metadata(json!({
        "id":"  invalid/id  ", "name":"Artist 猫", "service":"patreon", "user":42,
        "indexed":1600000000, "updated":"2026-10-20T12:34:56", "favorited":7,
        "faved_seq":18446744073709551615u64, "server_id":"001", "channel_id":99,
        "server":"Fixture Server", "channel":"art", "channel_topic":null,
        "channel_nsfw":false, "channel_type":0, "parent_id":null, "post_count":1.5,
    }));
    for expression in [
        "category == 'kemono' and subcategory == 'favorite'",
        "id == '  invalid/id  ' and name == 'Artist 猫' and service == 'patreon'",
        "user == 42 and user != '42' and server_id == '001' and server_id != 1",
        "indexed == 1600000000 and updated == '2026-10-20T12:34:56'",
        "favorited == 7 and faved_seq == 18446744073709551615",
        "channel_id == 99 and server == 'Fixture Server' and channel == 'art'",
        "channel_topic is None and parent_id is None and channel_nsfw is False",
        "channel_type == 0 and post_count == 1.5",
        "child['id'] == id and metadata['service'] == service",
        "child['category'] == category and metadata['subcategory'] == subcategory",
    ] {
        assert!(matches(expression, &child), "{expression}");
    }
    assert!(!matches("id == 'valid'", &child));
    assert_eq!(child.fields().len(), 17);
}

#[test]
fn untrusted_queue_payloads_cannot_expose_private_fields_or_nested_values() {
    let mut raw = json!({
        "id":"PRIVATE_BOUNDED_ID", "name":"https://user:SECRET@private.example/path",
        "service":"patreon", "url":"https://private.example/URL_SECRET",
        "_url":"URL_SECRET", "Authorization":"AUTH_SECRET", "password":"PASSWORD_SECRET",
        "_http_headers":{"Cookie":"COOKIE_SECRET"}, "_extractor":"PRIVATE_CLASS",
        "_fallback":["https://private.example/FALLBACK_SECRET"],
        "channel_topic":"A topic https://private.example/TOPIC_SECRET",
        "channel":{"secret":"NESTED_SECRET"}, "server":["ARRAY_SECRET"],
        "user":"oversized", "category":"PRIVATE_CATEGORY", "subcategory":"PRIVATE_SUBCATEGORY",
    });
    raw["user"] = json!("X".repeat(65537));
    let child = metadata(raw.clone());
    raw["id"] = json!("CHANGED");
    assert_eq!(child.category(), "kemono");
    assert_eq!(child.subcategory(), "favorite");
    assert_eq!(child.fields().len(), 4);
    assert_eq!(child.fields()["id"], "PRIVATE_BOUNDED_ID");
    assert!(matches(
        "metadata.get('url') is None and child.get('_http_headers') is None",
        &child
    ));
    for field in ["channel", "server", "user"] {
        assert!(!child.fields().contains_key(field));
    }
    for expression in ["url", "Authorization", "password", "metadata['private']"] {
        let error = FilterExpression::parse(expression)
            .unwrap()
            .matches_child(&child)
            .unwrap_err()
            .to_string();
        assert!(!error.contains("SECRET"));
        assert!(!error.contains("PRIVATE"));
        assert!(!error.contains("http"));
    }
    let debug = format!("{:?}", ExtractionEvent::BeginChild(ChildScope::new(child)));
    for secret in ["PRIVATE_BOUNDED_ID", "patreon", "SECRET", "private.example"] {
        assert!(!debug.contains(secret), "{debug}");
    }
}

#[test]
fn public_url_names_and_multiline_topics_keep_their_filter_meaning() {
    let child = metadata(json!({
        "id":"https://example.test/invalid-id", "name":"https://example.test/artist",
        "channel_topic":"First line\nhttps://example.test/topic\nLast line",
        "url":"https://transport.test/PRIVATE", "_http_headers":{"Cookie":"PRIVATE"},
    }));
    assert_eq!(child.fields()["name"], "https://example.test/artist");
    assert_eq!(
        child.fields()["channel_topic"],
        "First line\nhttps://example.test/topic\nLast line"
    );
    assert!(matches(
        "id == 'https://example.test/invalid-id' and name == 'https://example.test/artist'",
        &child
    ));
    assert!(matches(
        "channel_topic.startswith('First line') and 'https://example.test/topic' in channel_topic",
        &child
    ));
    assert!(!child.fields().contains_key("url"));
    assert!(!child.fields().contains_key("_http_headers"));
}

#[test]
fn child_metadata_limits_reject_values_instead_of_truncating_them() {
    let child = metadata(json!({
        "id":"A".repeat(65536), "name":"B".repeat(65537), "service":"bad\0value",
        "server_id":{"id":"nested"},
    }));
    assert_eq!(child.fields().len(), 1);
    assert_eq!(child.fields()["id"].as_str().unwrap().len(), 65536);
    let invalid_labels =
        ChildMetadata::from_raw("https://SECRET", &"X".repeat(65), &serde_json::Map::new());
    assert_eq!(invalid_labels.category(), "");
    assert_eq!(invalid_labels.subcategory(), "");
    assert!(!format!("{invalid_labels:?}").contains("SECRET"));
}

#[test]
fn child_controls_share_cancellation_only_with_their_own_scope() {
    let scope = ChildScope::new(metadata(json!({"id":"42"})));
    let clone = scope.clone();
    let independent = ChildControl::default();
    assert!(scope.control.same_scope(&clone.control));
    assert!(!scope.control.same_scope(&independent));
    assert!(!scope.control.is_cancelled());
    std::thread::spawn(move || clone.control.cancel())
        .join()
        .unwrap();
    assert!(scope.control.is_cancelled());
    assert!(!independent.is_cancelled());
    scope.control.cancel();
    assert!(scope.control.is_cancelled());
}

#[tokio::test]
async fn file_only_adapter_ignores_child_boundaries_without_cancelling_them() {
    let scope = ChildScope::new(metadata(json!({"id":"42"})));
    let control = scope.control.clone();
    let media =
        extract::direct(&extract::http_url("https://cdn.example/file.jpg").unwrap()).unwrap();
    let expected = media.url.clone();
    let source = stream::iter([
        Ok(ExtractionEvent::BeginChild(scope)),
        Ok(ExtractionEvent::Media(Box::new(media))),
        Ok(ExtractionEvent::EndChild(control.clone())),
    ])
    .boxed();
    let output = media_only(source).collect::<Vec<_>>().await;
    assert_eq!(output.len(), 1);
    assert_eq!(output[0].as_ref().unwrap().url, expected);
    assert!(!control.is_cancelled());
}

#[test]
fn discord_queue_maps_support_upstream_channel_and_server_predicates() {
    let child = ChildMetadata::from_raw(
        "kemono",
        "discord-channel",
        json!({
            "id":"99", "service":"discord",
            "server":{
                "id":"42", "name":"Fixture Server", "indexed":1600000000,
                "updated":"2026-10-20T12:34:56", "url":"https://private.test/SERVER_SECRET",
                "cookies":"SERVER_COOKIE", "_http_headers":{"Authorization":"PRIVATE"},
            },
            "channel":{
                "id":"99", "server_id":"42", "name":"art", "parent_channel_id":null,
                "topic":"First line\nhttps://example.test/public-topic", "is_nsfw":false,
                "type":0, "post_count":123, "position":2, "theme_color":"#123456",
                "icon_emoji":"猫", "url":"https://private.test/CHANNEL_SECRET",
                "cookies":["CHANNEL_COOKIE"], "private":{"token":"NESTED_SECRET"},
            },
        })
        .as_object()
        .unwrap(),
    );
    for expression in [
        "category == 'kemono' and subcategory == 'discord-channel'",
        "server['id'] == '42' and server['name'] == 'Fixture Server'",
        "server['indexed'] == 1600000000 and server['updated'] == '2026-10-20T12:34:56'",
        "channel['id'] == '99' and channel['server_id'] == server['id']",
        "channel['name'] == 'art' and channel['post_count'] > 100",
        "channel['parent_channel_id'] is None and channel['is_nsfw'] is False",
        "channel['type'] == 0 and channel['position'] == 2",
        "channel['theme_color'] == '#123456' and channel['icon_emoji'] == '猫'",
        "channel['topic'].startswith('First line') and 'https://example.test/public-topic' in channel['topic']",
        "metadata['channel']['id'] == child['channel']['id'] == channel['id']",
        "server.get('url') is None and server.get('cookies') is None and server.get('_http_headers') is None",
        "channel.get('url') is None and channel.get('cookies') is None and channel.get('private') is None",
    ] {
        assert!(matches(expression, &child), "{expression}");
    }
    assert_eq!(child.fields()["server"].as_object().unwrap().len(), 4);
    assert_eq!(child.fields()["channel"].as_object().unwrap().len(), 11);
    let public = serde_json::to_string(child.fields()).unwrap();
    for secret in ["SECRET", "COOKIE", "PRIVATE", "private.test"] {
        assert!(!public.contains(secret), "{public}");
    }
}

#[test]
fn discord_queue_maps_omit_excess_depth_arrays_and_oversized_scalars() {
    let child = metadata(json!({
        "server":{
            "id":{"value":"42"}, "name":["nested"], "indexed":1700000000,
            "updated":"X".repeat(65537), "channel":{"id":"unexpected"},
        },
        "channel":{
            "id":"invalid/id", "server_id":"42", "name":"https://example.test/public-name",
            "topic":"X".repeat(65536), "is_nsfw":{"value":true}, "post_count":[123],
            "type":null, "position":1, "parent_channel_id":{"id":"nested"},
            "theme_color":"bad\0value", "icon_emoji":["nested"],
        },
    }));
    assert_eq!(child.fields()["server"], json!({"indexed":1700000000}));
    let channel = child.fields()["channel"].as_object().unwrap();
    assert_eq!(channel.len(), 6);
    assert_eq!(channel["topic"].as_str().unwrap().len(), 65536);
    assert!(matches(
        "server.get('id') is None and server.get('name') is None and server.get('updated') is None",
        &child
    ));
    assert!(matches(
        "channel['id'] == 'invalid/id' and channel['name'] == 'https://example.test/public-name'",
        &child
    ));
    assert!(matches(
        "channel.get('is_nsfw') is None and channel.get('post_count') is None and channel['type'] is None",
        &child
    ));
    assert!(matches(
        "channel.get('parent_channel_id') is None and channel.get('theme_color') is None and channel.get('icon_emoji') is None",
        &child
    ));
}

#[test]
fn child_numeric_values_have_an_encoded_size_bound_without_losing_u64_precision() {
    let number = |digits| serde_json::from_str::<Value>(&"9".repeat(digits)).unwrap();
    let boundary = number(128);
    let oversized = number(129);
    let child = metadata(json!({
        "id":boundary, "user":oversized, "faved_seq":u64::MAX,
        "server":{"indexed":boundary,"updated":oversized},
    }));
    assert_eq!(child.fields().len(), 3);
    assert_eq!(child.fields()["id"], boundary);
    assert!(!child.fields().contains_key("user"));
    assert_eq!(child.fields()["faved_seq"].as_u64(), Some(u64::MAX));
    assert_eq!(child.fields()["server"], json!({"indexed":boundary}));
    assert!(matches("faved_seq == 18446744073709551615", &child));
}

#[test]
fn child_filter_preserves_integer_precision_across_the_full_supported_i128_range() {
    let raw: Value = serde_json::from_str(
        r#"{
        "id":18446744073709551617,
        "server":{"indexed":-9223372036854775809,"updated":170141183460469231731687303715884105727}
    }"#,
    )
    .unwrap();
    let child = metadata(raw);
    assert!(matches(
        "id == 18446744073709551617 and id != 18446744073709551616",
        &child
    ));
    assert!(matches(
        "server['indexed'] == -9223372036854775809 and server['indexed'] != -9223372036854775808",
        &child
    ));
    assert!(matches(
        "server['updated'] == 170141183460469231731687303715884105727",
        &child
    ));
}
