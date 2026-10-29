use gallery_dl_rs::{
    extract::Media,
    twitter::{ExpansionOptions, RetweetMode, parse_response_with_options},
};
use serde_json::{Value, json};

const ROOT: &str = "1234567890";
const ORIGINAL: &str = "2345678901";
const QUOTE: &str = "3456789012";

fn tweet(id: &str, user: &str) -> Value {
    let mut fixture: Value =
        serde_json::from_slice(include_bytes!("../fixtures/twitter/photo.json")).unwrap();
    let mut value = fixture["data"]["tweetResult"]["result"].take();
    value["rest_id"] = id.into();
    value["legacy"]["id_str"] = id.into();
    value["legacy"]["full_text"] = format!("Caption for {id}").into();
    value["core"]["user_results"]["result"]["legacy"]["screen_name"] = user.into();
    value
}

fn options(retweets: RetweetMode, quoted: bool) -> ExpansionOptions {
    ExpansionOptions { retweets, quoted }
}

fn parse(value: Value, id: &str, options: ExpansionOptions) -> gallery_dl_rs::Result<Vec<Media>> {
    let envelope = json!({"data":{"tweetResult":{"result":value}}});
    parse_response_with_options(&serde_json::to_vec(&envelope).unwrap(), id, options)
}

fn link_quote(parent: &mut Value, child: Value) {
    parent["legacy"]["quoted_status_id_str"] = child["rest_id"].clone();
    parent["quoted_status_result"] = json!({"result":child});
}

fn retweet() -> Value {
    let mut outer = tweet(ROOT, "reposter");
    outer["legacy"]["created_at"] = "Wed Nov 15 22:13:20 +0000 2023".into();
    outer["legacy"]["retweeted_status_id_str"] = ORIGINAL.into();
    outer["retweeted_status_result"] = json!({"result":tweet(ORIGINAL, "original_author")});
    outer
}

#[test]
fn retweet_modes_choose_identity_context_and_stable_archive() {
    assert!(
        parse(retweet(), ROOT, ExpansionOptions::default())
            .unwrap()
            .is_empty()
    );
    assert!(
        parse(retweet(), ROOT, options(RetweetMode::Skip, false))
            .unwrap()
            .is_empty()
    );
    let direct = parse(
        tweet(ORIGINAL, "original_author"),
        ORIGINAL,
        options(RetweetMode::Skip, false),
    )
    .unwrap();
    let original = parse(retweet(), ROOT, options(RetweetMode::Original, false)).unwrap();
    assert_eq!(original.len(), 1);
    assert_eq!(original[0].filename, direct[0].filename);
    assert_eq!(original[0].archive_key(), direct[0].archive_key());
    let metadata = original[0].metadata.as_ref().unwrap();
    assert_eq!(metadata.post_id, ORIGINAL);
    assert_eq!(metadata.username.as_deref(), Some("original_author"));
    assert_eq!(metadata.timestamp, Some(1700000000));
    assert_eq!(metadata.reposted_by_id.as_deref(), Some(ROOT));
    assert!(metadata.retweet_id.is_none());

    let repost = parse(retweet(), ROOT, options(RetweetMode::Repost, false)).unwrap();
    let metadata = repost[0].metadata.as_ref().unwrap();
    assert_eq!(metadata.post_id, ROOT);
    assert_eq!(metadata.username.as_deref(), Some("reposter"));
    assert_eq!(metadata.timestamp, Some(1700086400));
    assert_eq!(metadata.caption.as_deref(), Some("Caption for 2345678901"));
    assert_eq!(metadata.retweet_id.as_deref(), Some(ORIGINAL));
    assert!(metadata.reposted_by_id.is_none());
    assert_eq!(repost[0].filename, format!("{ROOT}_1.jpg"));
    assert_ne!(repost[0].archive_key(), direct[0].archive_key());
    assert_eq!(repost[0].url, direct[0].url);
}

#[test]
fn quote_order_uses_each_posts_identity_and_preserves_direct_archive() {
    let mut outer = tweet(ROOT, "outer");
    let mut child = tweet(ORIGINAL, "child");
    link_quote(&mut child, tweet(QUOTE, "grandchild"));
    link_quote(&mut outer, child);
    let media = parse(outer, ROOT, options(RetweetMode::Skip, true)).unwrap();
    assert_eq!(media.len(), 3);
    for (index, id) in [ROOT, ORIGINAL, QUOTE].into_iter().enumerate() {
        let metadata = media[index].metadata.as_ref().unwrap();
        assert_eq!(metadata.post_id, id);
        assert_eq!(metadata.index, 1);
        assert_eq!(
            metadata.quoted_by_id.as_deref(),
            [None, Some(ROOT), Some(ORIGINAL)][index]
        );
        let direct = parse(tweet(id, "direct"), id, options(RetweetMode::Skip, false)).unwrap();
        assert_eq!(media[index].archive_key(), direct[0].archive_key());
        assert_eq!(media[index].filename, direct[0].filename);
    }
}

#[test]
fn quote_only_post_can_emit_quoted_media_but_disabled_quotes_are_ignored() {
    let mut outer = tweet(ROOT, "outer");
    outer["legacy"]
        .as_object_mut()
        .unwrap()
        .remove("extended_entities");
    assert!(parse(outer.clone(), ROOT, options(RetweetMode::Skip, true)).is_err());
    link_quote(&mut outer, tweet(ORIGINAL, "child"));
    let media = parse(outer.clone(), ROOT, options(RetweetMode::Skip, true)).unwrap();
    assert_eq!(media.len(), 1);
    assert_eq!(media[0].metadata.as_ref().unwrap().post_id, ORIGINAL);
    assert!(parse(outer, ROOT, options(RetweetMode::Skip, false)).is_err());

    let mut outer = tweet(ROOT, "outer");
    outer["legacy"]["quoted_status_id_str"] = "secret invalid identity".into();
    outer["quoted_status_result"] = json!({"result":{"private":"secret payload"}});
    assert_eq!(
        parse(outer.clone(), ROOT, options(RetweetMode::Skip, false))
            .unwrap()
            .len(),
        1
    );
    let error = parse(outer, ROOT, options(RetweetMode::Skip, true))
        .unwrap_err()
        .to_string();
    assert!(!error.contains("secret"), "{error}");
}

#[test]
fn only_explicit_owned_links_are_followed_and_root_identity_is_mandatory() {
    let mut outer = tweet(ROOT, "outer");
    outer["unrelated"] = json!({"quoted_status_result":{"result":tweet(ORIGINAL,"other")}});
    assert_eq!(
        parse(outer.clone(), ROOT, options(RetweetMode::Original, true))
            .unwrap()
            .len(),
        1
    );
    link_quote(&mut outer, tweet(ORIGINAL, "child"));
    assert!(
        parse(
            outer.clone(),
            ORIGINAL,
            options(RetweetMode::Original, true)
        )
        .is_err()
    );
    outer["legacy"]["id_str"] = ORIGINAL.into();
    assert!(parse(outer, ROOT, options(RetweetMode::Original, true)).is_err());
}

#[test]
fn enabled_missing_conflicting_and_invalid_targets_fail_atomically_privately() {
    for mutate in 0..7 {
        let mut outer = tweet(ROOT, "outer");
        link_quote(&mut outer, tweet(ORIGINAL, "child"));
        match mutate {
            0 => {
                outer
                    .as_object_mut()
                    .unwrap()
                    .remove("quoted_status_result");
            }
            1 => outer["quoted_status_result"]["result"]["rest_id"] = QUOTE.into(),
            2 => outer["quoted_status_result"]["result"]["legacy"]["id_str"] = QUOTE.into(),
            3 => outer["legacy"]["quoted_status_id_str"] = "secret".into(),
            4 => outer["legacy"]["quoted_status_result"] = json!({"result":tweet(QUOTE,"other")}),
            5 => {
                outer["quoted_status_result"]["result"] =
                    json!({"__typename":"TweetUnavailable","reason":"secret"})
            }
            _ => {
                outer["quoted_status_result"]["result"]["legacy"]["extended_entities"]["media"][0]
                    ["media_url_https"] = "https://user:secret@pbs.example/private.jpg".into()
            }
        }
        let error = parse(outer, ROOT, options(RetweetMode::Skip, true))
            .unwrap_err()
            .to_string();
        assert!(!error.contains("secret"), "case {mutate}: {error}");
    }
    for mutate in 0..4 {
        let mut outer = retweet();
        match mutate {
            0 => {
                outer
                    .as_object_mut()
                    .unwrap()
                    .remove("retweeted_status_result");
            }
            1 => outer["legacy"]["retweeted_status_id_str"] = QUOTE.into(),
            2 => outer["retweeted_status_result"]["result"]["legacy"]["id_str"] = QUOTE.into(),
            _ => {
                outer["legacy"]["retweeted_status_result"] = json!({"result":tweet(QUOTE,"other")})
            }
        }
        assert!(parse(outer.clone(), ROOT, options(RetweetMode::Repost, false)).is_err());
        assert!(
            parse(outer, ROOT, options(RetweetMode::Skip, true))
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn owned_legacy_links_and_visibility_wrappers_are_supported_with_bounds() {
    let mut matching_aliases = retweet();
    matching_aliases["legacy"]["retweeted_status_result"] =
        matching_aliases["retweeted_status_result"].clone();
    assert_eq!(
        parse(
            matching_aliases,
            ROOT,
            options(RetweetMode::Original, false)
        )
        .unwrap()
        .len(),
        1
    );
    let mut outer = retweet();
    let mut target = outer["retweeted_status_result"]["result"].take();
    outer
        .as_object_mut()
        .unwrap()
        .remove("retweeted_status_result");
    for _ in 0..4 {
        target = json!({"__typename":"TweetWithVisibilityResults","tweet":target});
    }
    outer["legacy"]["retweeted_status_result"] = json!({"result":target.clone()});
    assert_eq!(
        parse(outer.clone(), ROOT, options(RetweetMode::Original, false))
            .unwrap()
            .len(),
        1
    );
    outer["legacy"]["retweeted_status_result"]["result"] =
        json!({"__typename":"TweetWithVisibilityResults","tweet":target});
    assert!(parse(outer, ROOT, options(RetweetMode::Original, false)).is_err());

    let mut outer = tweet(ROOT, "outer");
    outer["legacy"]["quoted_status_id_str"] = ORIGINAL.into();
    outer["legacy"]["quoted_status_result"] = json!({"result":tweet(ORIGINAL,"child")});
    assert_eq!(
        parse(outer, ROOT, options(RetweetMode::Skip, true))
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn quote_cycles_and_excessive_depth_are_explicit_errors() {
    let mut repost = tweet(ROOT, "self");
    repost["legacy"]["retweeted_status_id_str"] = ROOT.into();
    repost["retweeted_status_result"] = json!({"result":tweet(ROOT,"self")});
    assert!(parse(repost, ROOT, options(RetweetMode::Repost, false)).is_err());
    let mut outer = tweet(ROOT, "outer");
    link_quote(&mut outer, tweet(ROOT, "self"));
    assert!(parse(outer, ROOT, options(RetweetMode::Skip, true)).is_err());
    let mut child = tweet(ORIGINAL, "child");
    link_quote(&mut child, tweet(ROOT, "cycle"));
    let mut outer = tweet(ROOT, "outer");
    link_quote(&mut outer, child);
    assert!(parse(outer, ROOT, options(RetweetMode::Skip, true)).is_err());

    for links in [8, 9] {
        let mut nested = tweet(&(100 + links).to_string(), "leaf");
        for id in (100..100 + links).rev() {
            let mut parent = tweet(&id.to_string(), "parent");
            link_quote(&mut parent, nested);
            nested = parent;
        }
        let result = parse(nested, "100", options(RetweetMode::Skip, true));
        if links == 8 {
            assert_eq!(result.unwrap().len(), 9);
        } else {
            assert!(result.is_err());
        }
    }
}

#[test]
fn repost_preserves_its_own_quote_link_and_original_mode_uses_original_links() {
    let mut outer = retweet();
    link_quote(&mut outer, tweet(QUOTE, "quoted_author"));
    let media = parse(outer.clone(), ROOT, options(RetweetMode::Repost, true)).unwrap();
    assert_eq!(media.len(), 2);
    assert_eq!(media[0].metadata.as_ref().unwrap().post_id, ROOT);
    assert_eq!(media[1].metadata.as_ref().unwrap().post_id, QUOTE);
    assert_eq!(
        media[1].metadata.as_ref().unwrap().quoted_by_id.as_deref(),
        Some(ROOT)
    );
    assert_eq!(
        parse(outer.clone(), ROOT, options(RetweetMode::Original, true))
            .unwrap()
            .len(),
        1
    );
    let child = outer["quoted_status_result"]["result"].clone();
    link_quote(&mut outer["retweeted_status_result"]["result"], child);
    assert_eq!(
        parse(outer.clone(), ROOT, options(RetweetMode::Repost, true))
            .unwrap()
            .len(),
        2
    );
    outer["retweeted_status_result"]["result"]["legacy"]["quoted_status_id_str"] =
        "4567890123".into();
    outer["retweeted_status_result"]["result"]["quoted_status_result"] =
        json!({"result":tweet("4567890123", "conflict")});
    assert!(parse(outer, ROOT, options(RetweetMode::Repost, true)).is_err());
}

#[test]
fn quoted_media_context_names_the_emitted_repost_parent() {
    let mut outer = retweet();
    link_quote(
        &mut outer["retweeted_status_result"]["result"],
        tweet(QUOTE, "quoted_author"),
    );
    for (mode, parent) in [
        (RetweetMode::Repost, ROOT),
        (RetweetMode::Original, ORIGINAL),
    ] {
        let media = parse(outer.clone(), ROOT, options(mode, true)).unwrap();
        assert_eq!(media.len(), 2);
        assert_eq!(
            media[1].metadata.as_ref().unwrap().quoted_by_id.as_deref(),
            Some(parent)
        );
    }
}
