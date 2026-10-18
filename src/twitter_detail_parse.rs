//! Explicit conversation envelopes. Inaccessible context is not a downloadable post.
use super::*;

pub(super) fn parse_page(bytes: &[u8], pinned: bool) -> Result<timeline::Page> {
    let mut data = timeline::document(bytes)?;
    let instructions = data
        .pointer_mut("/data/threaded_conversation_with_injections_v2/instructions")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| error("Twitter conversation instructions missing or invalid"))?;
    for instruction in instructions.iter_mut() {
        match instruction.get("type").and_then(Value::as_str) {
            Some("TimelineAddEntries") => {
                if let Some(entries) = instruction.get_mut("entries").and_then(Value::as_array_mut)
                {
                    normalize_entries(entries)?;
                }
            }
            Some("TimelineAddToModule") => {
                if let Some(items) = instruction
                    .get_mut("moduleItems")
                    .and_then(Value::as_array_mut)
                {
                    normalize_items(items)?;
                }
            }
            Some("TimelinePinEntry") if pinned => {
                if let Some(entry) = instruction.get_mut("entry")
                    && normalize_entry(entry)?
                {
                    *instruction = serde_json::json!({"type":"TimelineAddEntries","entries":[]});
                }
            }
            _ => {}
        }
    }
    timeline::parse_instructions(
        instructions,
        None,
        timeline::ProfileTab::Timeline,
        pinned,
        None,
    )
}

fn normalize_entries(entries: &mut Vec<Value>) -> Result<()> {
    let mut retained = Vec::with_capacity(entries.len());
    for mut entry in std::mem::take(entries) {
        if !normalize_entry(&mut entry)? {
            retained.push(entry);
        }
    }
    *entries = retained;
    Ok(())
}

// Return true only for an explicitly owned, recognized inaccessible item.
fn normalize_entry(entry: &mut Value) -> Result<bool> {
    if promoted(entry) {
        return Ok(false);
    }
    let Some(content) = entry.get_mut("content") else {
        return Ok(false);
    };
    if promoted(content) {
        return Ok(false);
    }
    match content.get("entryType").and_then(Value::as_str) {
        Some("TimelineTimelineItem") => content
            .get("itemContent")
            .map(unavailable)
            .transpose()
            .map(|v| v.unwrap_or(false)),
        Some("TimelineTimelineModule") => {
            if let Some(items) = content.get_mut("items").and_then(Value::as_array_mut) {
                normalize_items(items)?;
            }
            Ok(false)
        }
        _ => Ok(false),
    }
}

fn normalize_items(items: &mut Vec<Value>) -> Result<()> {
    let mut retained = Vec::with_capacity(items.len());
    for item in std::mem::take(items) {
        if promoted(&item)
            || item.get("item").is_some_and(promoted)
            || !item
                .pointer("/item/itemContent")
                .map(unavailable)
                .transpose()?
                .unwrap_or(false)
        {
            retained.push(item);
        }
    }
    *items = retained;
    Ok(())
}

fn unavailable(item: &Value) -> Result<bool> {
    if promoted(item) {
        return Ok(false);
    }
    match item.get("itemType").and_then(Value::as_str) {
        Some("TimelineTombstone") => return Ok(true),
        Some("TimelineTweet") => {}
        _ => return Ok(false),
    }
    let Some(mut result) = item.pointer("/tweet_results/result") else {
        return Ok(false);
    };
    for _ in 0..4 {
        if result["__typename"] != "TweetWithVisibilityResults" {
            break;
        }
        result = result
            .get("tweet")
            .ok_or_else(|| error("Twitter visibility result has no post"))?;
    }
    // A fifth wrapper is malformed even if it eventually contains a tombstone.
    if result["__typename"] == "TweetWithVisibilityResults" {
        return Err(error(
            "Twitter conversation visibility nesting exceeded its limit",
        ));
    }
    Ok(matches!(
        result.get("__typename").and_then(Value::as_str),
        Some("TweetUnavailable" | "TweetTombstone")
    ) || result.get("tombstone").is_some_and(Value::is_object))
}

fn promoted(value: &Value) -> bool {
    value
        .get("promotedMetadata")
        .is_some_and(|value| !value.is_null())
}

pub(super) fn post_owner(value: &Value) -> Result<String> {
    let mut tweet = value;
    for _ in 0..4 {
        if tweet["__typename"] != "TweetWithVisibilityResults" {
            break;
        }
        tweet = tweet
            .get("tweet")
            .ok_or_else(|| error("Twitter visibility result has no post"))?;
    }
    if tweet["__typename"] != "Tweet" {
        return Err(error("Twitter conversation post unavailable or invalid"));
    }
    let id = tweet
        .get("rest_id")
        .and_then(Value::as_str)
        .filter(|id| valid_id(id))
        .ok_or_else(|| error("Twitter conversation post identifier missing or invalid"))?;
    let legacy = tweet
        .get("legacy")
        .filter(|value| value.is_object())
        .ok_or_else(|| error("Twitter conversation post fields missing"))?;
    if legacy
        .get("id_str")
        .is_some_and(|value| value.as_str() != Some(id))
    {
        return Err(error("Twitter conversation post identifiers conflict"));
    }
    let aliases: Vec<_> = [
        legacy.get("user_id_str"),
        tweet.pointer("/core/user_results/result/rest_id"),
    ]
    .into_iter()
    .flatten()
    .collect();
    let owner = aliases
        .first()
        .and_then(|value| value.as_str())
        .filter(|id| valid_id(id))
        .ok_or_else(|| error("Twitter conversation owner identifier missing or invalid"))?;
    if aliases.iter().any(|value| value.as_str() != Some(owner)) {
        return Err(error("Twitter conversation owner identifiers conflict"));
    }
    Ok(owner.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tweet(id: &str, owner: &str) -> Value {
        let mut data: Value =
            serde_json::from_str(include_str!("../tests/fixtures/twitter/photo.json")).unwrap();
        let tweet = &mut data["data"]["tweetResult"]["result"];
        tweet["rest_id"] = id.into();
        tweet["legacy"]["id_str"] = id.into();
        tweet["legacy"]["user_id_str"] = owner.into();
        tweet["core"]["user_results"]["result"]["rest_id"] = owner.into();
        tweet.clone()
    }
    fn item(result: Value) -> Value {
        json!({"itemType":"TimelineTweet","tweet_results":{"result":result}})
    }
    fn entry(item: Value) -> Value {
        json!({"content":{"entryType":"TimelineTimelineItem","itemContent":item}})
    }
    fn module(items: Vec<Value>) -> Value {
        json!({"content":{"entryType":"TimelineTimelineModule","items":items.into_iter().map(|item|json!({"item":{"itemContent":item}})).collect::<Vec<_>>()}})
    }
    fn bytes(instructions: Value) -> Vec<u8> {
        serde_json::to_vec(&json!({"data":{"threaded_conversation_with_injections_v2":{"instructions":instructions}}})).unwrap()
    }
    fn entries(items: Vec<Value>) -> Vec<u8> {
        bytes(json!([{"type":"TimelineAddEntries","entries":items}]))
    }
    fn ids(page: &timeline::Page) -> Vec<&str> {
        page.posts.iter().map(|(id, _)| id.as_str()).collect()
    }
    fn private_error(bytes: &[u8]) {
        let message = parse_page(bytes, true)
            .err()
            .expect("must fail")
            .to_string();
        assert!(!message.contains("PRIVATE"), "{message}");
    }

    #[test]
    fn exact_conversation_envelope_preserves_context_order_and_each_author() {
        let page = parse_page(
            &entries(vec![
                entry(item(tweet("10", "41"))),
                module(vec![item(tweet("11", "42")), item(tweet("12", "43"))]),
            ]),
            true,
        )
        .unwrap();
        assert_eq!(ids(&page), ["10", "11", "12"]);
        assert_eq!(
            page.posts
                .iter()
                .map(|(_, post)| post_owner(post).unwrap())
                .collect::<Vec<_>>(),
            ["41", "42", "43"]
        );
        assert!(page.done);
        let decoy = serde_json::to_vec(&json!({"data":{"other":{"instructions":[{"type":"TimelineAddEntries","entries":[entry(item(tweet("10","41")))]}]}}})).unwrap();
        private_error(&decoy);
    }

    #[test]
    fn explicit_inaccessible_context_is_skipped_in_entries_modules_and_pins() {
        let tombstone = json!({"itemType":"TimelineTombstone","tombstoneInfo":{"text":"PRIVATE"}});
        let unavailable = item(json!({"__typename":"TweetUnavailable","reason":"PRIVATE"}));
        let legacy_tombstone = item(json!({"tombstone":{"text":"PRIVATE"}}));
        let wrapped = item(
            json!({"__typename":"TweetWithVisibilityResults","tweet":{"__typename":"TweetTombstone","reason":"PRIVATE"}}),
        );
        let page = parse_page(&bytes(json!([
            {"type":"TimelinePinEntry","entry":entry(tombstone.clone())},
            {"type":"TimelineAddEntries","entries":[entry(unavailable),module(vec![legacy_tombstone, item(tweet("11","42")), wrapped])]},
            {"type":"TimelineAddToModule","moduleItems":[{"item":{"itemContent":tombstone}},{"item":{"itemContent":item(tweet("12","43"))}}]}
        ])),true).unwrap();
        assert_eq!(ids(&page), ["11", "12"]);
        assert!(page.done);
    }

    #[test]
    fn tombstone_only_page_keeps_bottom_cursor_and_empty_continuation() {
        let page = parse_page(&entries(vec![entry(json!({"itemType":"TimelineTombstone"})),json!({"content":{"entryType":"TimelineTimelineCursor","cursorType":"Bottom","value":"PRIVATE-cursor","stopOnEmptyResponse":false}})]),true).unwrap();
        assert!(page.posts.is_empty());
        assert_eq!(page.cursor.as_deref(), Some("PRIVATE-cursor"));
        assert!(!page.done);
        assert!(page.continue_empty);
        let terminal = parse_page(
            &bytes(json!([{"type":"TimelineTerminateTimeline","direction":"Bottom"}])),
            true,
        )
        .unwrap();
        assert!(terminal.done);
    }

    #[test]
    fn visible_pins_precede_context_and_disabled_pins_are_ignored() {
        let instructions = bytes(json!([
            {"type":"TimelineAddEntries","entries":[entry(item(tweet("10","41")))]},
            {"type":"TimelinePinEntry","entry":entry(item(tweet("11","42")))}
        ]));
        assert_eq!(ids(&parse_page(&instructions, true).unwrap()), ["11", "10"]);
        assert_eq!(ids(&parse_page(&instructions, false).unwrap()), ["10"]);
    }

    #[test]
    fn accessible_identity_failures_are_atomic_and_private() {
        let valid = entry(item(tweet("10", "41")));
        for bad in [
            {
                let mut post = tweet("11", "42");
                post["legacy"]["id_str"] = "PRIVATE".into();
                post
            },
            {
                let mut post = tweet("11", "42");
                post["legacy"]["user_id_str"] = "PRIVATE".into();
                post
            },
            {
                let mut post = tweet("11", "42");
                post["core"]["user_results"]["result"]["rest_id"] = "43".into();
                post
            },
            {
                let mut post = tweet("11", "42");
                post["rest_id"] = "PRIVATE".into();
                post
            },
        ] {
            assert!(post_owner(&bad).is_err());
            private_error(&entries(vec![valid.clone(), entry(item(bad))]));
        }
    }

    #[test]
    fn author_aliases_require_one_canonical_identifier_and_all_present_aliases_agree() {
        let mut one_alias = tweet("10", "41");
        one_alias["legacy"]
            .as_object_mut()
            .unwrap()
            .remove("user_id_str");
        assert_eq!(post_owner(&one_alias).unwrap(), "41");
        one_alias["legacy"]["user_id_str"] = "41".into();
        one_alias["core"]["user_results"]["result"]
            .as_object_mut()
            .unwrap()
            .remove("rest_id");
        assert_eq!(post_owner(&one_alias).unwrap(), "41");
        one_alias["legacy"]
            .as_object_mut()
            .unwrap()
            .remove("user_id_str");
        assert!(post_owner(&one_alias).is_err());
        for invalid in [
            Value::Null,
            json!(41),
            json!("0"),
            json!("041"),
            json!("18446744073709551616"),
            json!("PRIVATE"),
        ] {
            one_alias["legacy"]["user_id_str"] = invalid;
            assert!(post_owner(&one_alias).is_err());
            private_error(&entries(vec![entry(item(one_alias.clone()))]));
        }
    }

    #[test]
    fn only_structured_owned_tombstones_are_skipped_and_unknowns_fail() {
        for bad in [
            entry(json!({"itemType":"PRIVATE-unknown","nested":{"itemType":"TimelineTombstone"}})),
            entry(item(
                json!({"__typename":"PRIVATE-unknown","nested":{"tombstone":{}}}),
            )),
            entry(item(json!({"__typename":"Tweet","tombstone":"PRIVATE"}))),
            json!({"content":{"entryType":"PRIVATE-unknown","itemContent":{"itemType":"TimelineTombstone"}}}),
        ] {
            private_error(&entries(vec![bad]));
        }
        private_error(&bytes(json!([{"type":"PRIVATE-unknown","entries":[]}])))
    }

    #[test]
    fn visibility_nesting_is_bounded_for_accessible_and_inaccessible_context() {
        for unavailable in [false, true] {
            let mut result = if unavailable {
                json!({"__typename":"TweetTombstone"})
            } else {
                tweet("10", "41")
            };
            for _ in 0..4 {
                result = json!({"__typename":"TweetWithVisibilityResults","tweet":result});
            }
            let page = parse_page(&entries(vec![entry(item(result.clone()))]), true).unwrap();
            assert_eq!(page.posts.len(), usize::from(!unavailable));
            if !unavailable {
                assert_eq!(post_owner(&result).unwrap(), "41");
            }
            let excessive = json!({"__typename":"TweetWithVisibilityResults","tweet":result});
            assert!(post_owner(&excessive).is_err());
            private_error(&entries(vec![entry(item(excessive))]));
        }
    }

    #[test]
    fn envelope_and_response_limits_and_graphql_errors_are_private() {
        for value in [
            json!({"data":{"threaded_conversation_with_injections_v2":{"instructions":"PRIVATE"}}}),
            json!({"errors":[{"code":179,"message":"PRIVATE"}]}),
        ] {
            private_error(&serde_json::to_vec(&value).unwrap());
        }
        private_error(&vec![b'x'; MAX_RESPONSE + 1]);
        private_error(b"PRIVATE-invalid-json");
    }
}
