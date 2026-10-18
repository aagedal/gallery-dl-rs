use super::*;
use serde_json::json;

fn tweet(id: &str, author: &str, recipient: Option<&str>) -> Value {
    let value: Value =
        serde_json::from_slice(include_bytes!("../tests/fixtures/twitter/photo.json")).unwrap();
    let mut tweet = value["data"]["tweetResult"]["result"].clone();
    tweet["rest_id"] = id.into();
    tweet["legacy"]["id_str"] = id.into();
    tweet["legacy"]["user_id_str"] = author.into();
    tweet["core"]["user_results"]["result"]["rest_id"] = author.into();
    if let Some(recipient) = recipient {
        tweet["legacy"]["in_reply_to_user_id_str"] = recipient.into();
    }
    tweet
}

fn parse(
    value: Value,
    mode: ReplyMode,
    owner: Option<&str>,
    retweets: RetweetMode,
) -> Result<Vec<Media>> {
    let id = value["rest_id"].as_str().unwrap().to_owned();
    let mut engine = expansion::Engine::new(
        value,
        id,
        ExpansionOptions {
            retweets,
            quoted: true,
        },
        true,
    )
    .with_replies(mode, owner.map(str::to_owned));
    let mut media = Vec::new();
    loop {
        match engine.next()? {
            expansion::Action::Media(items) => media.extend(items),
            expansion::Action::Done => return Ok(media),
            expansion::Action::Fetch(_) => panic!("fixture target missing"),
        }
    }
}

#[test]
fn reply_filter_preserves_independent_quote_selection() {
    let mut root = tweet("100", "42", Some("99"));
    root["legacy"]["quoted_status_id_str"] = "101".into();
    root["quoted_status_result"] = json!({"result":tweet("101","99",None)});
    let media = parse(root, ReplyMode::Skip, Some("42"), RetweetMode::Skip).unwrap();
    assert_eq!(media.len(), 1);
    assert_eq!(media[0].metadata.as_ref().unwrap().post_id, "101");
}

#[test]
fn retweet_reply_filter_uses_the_emitted_post_context() {
    let mut root = tweet("100", "42", None);
    root["legacy"]["retweeted_status_id_str"] = "101".into();
    root["retweeted_status_result"] = json!({"result":tweet("101","99",Some("42"))});
    assert_eq!(
        parse(
            root.clone(),
            ReplyMode::Skip,
            Some("42"),
            RetweetMode::Repost
        )
        .unwrap()
        .len(),
        1
    );
    assert!(
        parse(root, ReplyMode::Skip, Some("42"), RetweetMode::Original)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn reply_identity_failures_are_private_and_null_recipient_is_not_a_reply() {
    for mode in [ReplyMode::Skip, ReplyMode::SelfOnly] {
        let mut root = tweet("100", "42", None);
        root["legacy"]["in_reply_to_user_id_str"] = Value::Null;
        assert_eq!(
            parse(root.clone(), mode, None, RetweetMode::Skip)
                .unwrap()
                .len(),
            1
        );
        root["legacy"]["in_reply_to_user_id_str"] = "secret".into();
        assert!(
            !parse(root, mode, None, RetweetMode::Skip)
                .unwrap_err()
                .to_string()
                .contains("secret")
        );
    }
    let mut root = tweet("100", "42", Some("42"));
    root["core"]["user_results"]["result"]["rest_id"] = "99".into();
    assert!(parse(root, ReplyMode::SelfOnly, None, RetweetMode::Skip).is_err());
    let mut root = tweet("100", "42", Some("42"));
    root["legacy"]
        .as_object_mut()
        .unwrap()
        .remove("user_id_str");
    root["core"]["user_results"]["result"]
        .as_object_mut()
        .unwrap()
        .remove("rest_id");
    assert!(parse(root, ReplyMode::SelfOnly, None, RetweetMode::Skip).is_err());
}

#[test]
fn focal_original_owner_resolution_bounds_missing_target_requests() {
    let mut leaf = tweet("116", "99", None);
    for id in (100..116).rev() {
        let mut parent = tweet(&id.to_string(), "42", None);
        parent["legacy"]["retweeted_status_id_str"] = leaf["rest_id"].clone();
        parent["retweeted_status_result"] = json!({"result":leaf});
        leaf = parent;
    }
    let known = BTreeMap::new();
    assert!(
        matches!(expansion::owner_lookup(&leaf, RetweetMode::Original, &known).unwrap(), expansion::OwnerLookup::Owner(owner) if owner == "99")
    );
    // One further declared link must fail before asking runtime to fetch it.
    let mut node = &mut leaf;
    for _ in 0..16 {
        node = &mut node["retweeted_status_result"]["result"];
    }
    node["legacy"]["retweeted_status_id_str"] = "117".into();
    assert!(expansion::owner_lookup(&leaf, RetweetMode::Original, &known).is_err());
}
