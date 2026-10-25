//! Explicit authenticated collection routes and their owned timeline envelopes.
use super::*;
use serde_json::json;
use timeline::{Page, ProfileTab};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Collection {
    Bookmarks,
    List(String),
    HomeLatest,
    Home,
    Community(String),
    Communities,
    /// `/i/events/ID`: upstream `TwitterEventExtractor`'s REST live event.
    Event(String),
}

pub fn is_collection_url(url: &Url) -> bool {
    matches!(input(url), Ok(Some(_)))
}

pub(super) fn input(url: &Url) -> Result<Option<Collection>> {
    if !matches!(url.scheme(), "http" | "https")
        || !matches!(
            url.host_str(),
            Some(
                "x.com"
                    | "www.x.com"
                    | "mobile.x.com"
                    | "twitter.com"
                    | "www.twitter.com"
                    | "mobile.twitter.com"
            )
        )
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return Err(error(
            "native Twitter collections require a supported first-party URL",
        ));
    }
    let path = url.path().strip_suffix('/').unwrap_or(url.path());
    Ok(match path {
        "/i/bookmarks" => Some(Collection::Bookmarks),
        "/home" | "/home/following" => Some(Collection::HomeLatest),
        "/home/foryou" | "/home/for-you" | "/home/for_you" | "/home/for%20you" => {
            Some(Collection::Home)
        }
        _ => {
            if let Some(rest) = path.strip_prefix("/i/events/") {
                // Upstream `/i/events/(\d+)`, unanchored.
                let id = rest.split('/').next().unwrap_or_default();
                if !valid_id(id) {
                    return Err(error("Twitter event identifier missing or invalid"));
                }
                Some(Collection::Event(id.into()))
            } else if let Some(id) = path
                .strip_prefix("/i/communities/")
                .filter(|id| !id.contains('/'))
            {
                if !valid_id(id) {
                    return Err(error("Twitter community identifier missing or invalid"));
                }
                Some(Collection::Community(id.into()))
            } else if path
                .strip_prefix('/')
                .and_then(|path| path.strip_suffix("/communities"))
                .filter(|name| {
                    !name.is_empty()
                        && name.len() <= 15
                        && name
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                })
                .is_some()
            {
                Some(Collection::Communities)
            } else if let Some(id) = path
                .strip_prefix("/i/lists/")
                .filter(|id| !id.contains('/'))
            {
                if !valid_id(id) {
                    return Err(error("Twitter list identifier missing or invalid"));
                }
                Some(Collection::List(id.into()))
            } else {
                None
            }
        }
    })
}

pub(super) fn parse_collection_posts(
    bytes: &[u8],
    collection: &Collection,
    pinned: bool,
) -> Result<Page> {
    parse_collection_posts_with_text(bytes, collection, pinned, false)
}

pub(super) fn parse_collection_posts_with_text(
    bytes: &[u8],
    collection: &Collection,
    pinned: bool,
    text_tweets: bool,
) -> Result<Page> {
    if matches!(collection, Collection::Event(_)) {
        let instructions = rest_instructions(bytes)?;
        return timeline::parse_instructions(
            &instructions,
            None,
            ProfileTab::Timeline,
            pinned,
            None,
        );
    }
    let data = timeline::document(bytes)?;
    let path = match collection {
        Collection::Event(_) => unreachable!("REST timeline"),
        Collection::Bookmarks => "/data/bookmark_timeline_v2/timeline/instructions",
        Collection::Home | Collection::HomeLatest => "/data/home/home_timeline_urt/instructions",
        Collection::Communities => "/data/viewer/communities_timeline/timeline/instructions",
        Collection::Community(expected) => {
            if !valid_id(expected) {
                return Err(error("invalid Twitter community identifier"));
            }
            let community = data
                .pointer("/data/communityResults/result")
                .filter(|value| value.is_object())
                .ok_or_else(|| error("Twitter community timeline missing"))?;
            for field in ["rest_id", "id_str", "id"] {
                if let Some(value) = community.get(field) {
                    let numeric;
                    let decoded;
                    let id = if field == "id" && value.is_u64() {
                        numeric = value.as_u64().unwrap().to_string();
                        numeric.as_str()
                    } else if field == "id"
                        && value
                            .as_str()
                            .is_some_and(|value| !value.bytes().all(|byte| byte.is_ascii_digit()))
                    {
                        decoded = value
                            .as_str()
                            .and_then(|value| {
                                STANDARD
                                    .decode(value)
                                    .or_else(|_| STANDARD_NO_PAD.decode(value))
                                    .ok()
                            })
                            .and_then(|value| String::from_utf8(value).ok())
                            .ok_or_else(|| error("Twitter community identity alias invalid"))?;
                        decoded
                            .strip_prefix("Community:")
                            .ok_or_else(|| error("Twitter community identity alias invalid"))?
                    } else {
                        value.as_str().unwrap_or("")
                    };
                    if !valid_id(id) || id != expected {
                        return Err(error(
                            "Twitter timeline does not match the requested community",
                        ));
                    }
                }
            }
            if text_tweets {
                "/data/communityResults/result/ranked_community_timeline/timeline/instructions"
            } else {
                "/data/communityResults/result/community_media_timeline/timeline/instructions"
            }
        }
        Collection::List(expected) => {
            if !valid_id(expected) {
                return Err(error("invalid Twitter list identifier"));
            }
            let list = data
                .pointer("/data/list")
                .filter(|v| v.is_object())
                .ok_or_else(|| error("Twitter list timeline missing"))?;
            // The decimal identity fields can be omitted. A GraphQL `id` may
            // instead be opaque, so only its decimal form is an identity alias.
            for field in ["rest_id", "id_str", "id"] {
                if let Some(value) = list.get(field) {
                    if field == "id"
                        && value.as_str().is_some_and(|id| {
                            !id.is_empty() && !id.bytes().all(|b| b.is_ascii_digit())
                        })
                    {
                        let encoded = value.as_str().unwrap();
                        if let Ok(decoded) = STANDARD
                            .decode(encoded)
                            .or_else(|_| STANDARD_NO_PAD.decode(encoded))
                            && let Ok(decoded) = std::str::from_utf8(&decoded)
                            && let Some(id) = decoded.strip_prefix("List:")
                            && (!valid_id(id) || id != expected)
                        {
                            return Err(error(
                                "Twitter timeline does not match the requested list",
                            ));
                        }
                        continue;
                    }
                    let numeric;
                    let id = if field == "id" && value.is_u64() {
                        numeric = value.as_u64().unwrap().to_string();
                        numeric.as_str()
                    } else {
                        value.as_str().unwrap_or("")
                    };
                    if !valid_id(id) || id != expected {
                        return Err(error("Twitter timeline does not match the requested list"));
                    }
                }
            }
            "/data/list/tweets_timeline/timeline/instructions"
        }
    };
    let instructions = data
        .pointer(path)
        .and_then(Value::as_array)
        .ok_or_else(|| error("Twitter collection timeline instructions missing or invalid"))?;
    timeline::parse_instructions(instructions, None, ProfileTab::Timeline, pinned, None)
}

/// Upstream `TwitterAPI.params`, sent with the legacy REST endpoints.
pub(super) const REST_PARAMS: &[(&str, &str)] = &[
    ("include_profile_interstitial_type", "1"),
    ("include_blocking", "1"),
    ("include_blocked_by", "1"),
    ("include_followed_by", "1"),
    ("include_want_retweets", "1"),
    ("include_mute_edge", "1"),
    ("include_can_dm", "1"),
    ("include_can_media_tag", "1"),
    ("include_ext_is_blue_verified", "1"),
    ("include_ext_verified_type", "1"),
    ("include_ext_profile_image_shape", "1"),
    ("skip_status", "1"),
    ("cards_platform", "Web-12"),
    ("include_cards", "1"),
    ("include_ext_alt_text", "true"),
    ("include_ext_limited_action_results", "true"),
    ("include_quote_count", "true"),
    ("include_reply_count", "1"),
    ("tweet_mode", "extended"),
    ("include_ext_views", "true"),
    ("include_entities", "true"),
    ("include_user_entities", "true"),
    ("include_ext_media_color", "true"),
    ("include_ext_media_availability", "true"),
    ("include_ext_sensitive_media_warning", "true"),
    ("include_ext_trusted_friends_metadata", "true"),
    ("send_error_codes", "true"),
    ("simple_quoted_tweet", "true"),
    ("count", "20"),
    (
        "ext",
        "mediaStats,highlightedLabel,parodyCommentaryFanLabel,voiceInfo,birdwatchPivot,superFollowMetadata,unmentionInfo,editControl,article",
    ),
];

/// A REST endpoint below the GraphQL base's `/i/api/` root.
pub(super) fn rest_endpoint(base: &Url, path: &str, extra: &[(&str, &str)]) -> Result<Url> {
    let mut url = base
        .join(&format!("../{path}"))
        .map_err(|_| error("invalid Twitter REST endpoint"))?;
    {
        let mut query = url.query_pairs_mut();
        // Upstream copies `params` and then sets the extra keys.
        for (key, value) in REST_PARAMS
            .iter()
            .filter(|(key, _)| !extra.iter().any(|(name, _)| name == key))
            .chain(extra)
        {
            query.append_pair(key, value);
        }
    }
    Ok(url)
}

/// Upstream `live_event`: the event object of `/1.1/live_event/1/ID/timeline.json`.
pub(super) fn parse_live_event(bytes: &[u8], id: &str) -> Result<Value> {
    timeline::document(bytes)?
        .get("twitter_objects")
        .and_then(|objects| objects.get("live_events"))
        .and_then(|events| events.get(id))
        .filter(|event| event.is_object())
        .cloned()
        .ok_or_else(|| error("Twitter event not found"))
}

/// Upstream `_pagination_rest`: the legacy `globalObjects` timeline as
/// GraphQL-shaped instructions, so the shared timeline parser handles
/// retweets, quotes and cursors. Missing tweets are skipped like upstream;
/// a page without a bottom cursor, or without tweets and a keep-going
/// cursor, ends the timeline.
pub(super) fn rest_instructions(bytes: &[u8]) -> Result<Vec<Value>> {
    let data = timeline::document(bytes)?;
    let invalid = || error("Twitter REST timeline is invalid");
    let instructions = data
        .pointer("/timeline/instructions")
        .and_then(Value::as_array)
        .ok_or_else(invalid)?;
    let terminate = json!({"type": "TimelineTerminateTimeline", "direction": "Bottom"});
    if instructions.is_empty() {
        return Ok(vec![terminate]);
    }
    let tweets = data
        .pointer("/globalObjects/tweets")
        .and_then(Value::as_object)
        .ok_or_else(invalid)?;
    let users = data
        .pointer("/globalObjects/users")
        .and_then(Value::as_object)
        .ok_or_else(invalid)?;
    const BOTTOM: [&str; 2] = ["cursor-bottom-", "sq-cursor-bottom"];
    let mut cursor: Option<(String, bool)> = None;
    let mut entries: &[Value] = &[];
    for instruction in instructions {
        if let Some(added) = instruction.pointer("/addEntries/entries") {
            entries = added.as_array().map(Vec::as_slice).ok_or_else(invalid)?;
        } else if let Some(entry) = instruction.pointer("/replaceEntry/entry")
            && entry
                .get("entryId")
                .and_then(Value::as_str)
                .is_some_and(|id| BOTTOM.iter().any(|prefix| id.starts_with(prefix)))
            && let Some(value) = entry
                .pointer("/content/operation/cursor/value")
                .and_then(Value::as_str)
        {
            cursor = Some((value.to_owned(), false));
        }
    }
    let mut ids: Vec<String> = Vec::new();
    let mut keep_going = false;
    for entry in entries {
        let entry_id = entry.get("entryId").and_then(Value::as_str).unwrap_or("");
        let content = entry.get("content").unwrap_or(&Value::Null);
        if entry_id.starts_with("tweet-") || entry_id.starts_with("sq-I-t-") {
            if let Some(id) = content.pointer("/item/content/tweet/id") {
                ids.push(match id {
                    Value::String(id) => id.clone(),
                    other => other.to_string(),
                });
            }
        } else if entry_id.starts_with("homeConversation-") {
            if let Some(all) = content
                .pointer("/timelineModule/metadata/conversationMetadata/allTweetIds")
                .and_then(Value::as_array)
            {
                ids.extend(
                    all.iter()
                        .rev()
                        .filter_map(|id| id.as_str().map(str::to_owned)),
                );
            }
        } else if BOTTOM.iter().any(|prefix| entry_id.starts_with(prefix)) {
            let operation = content.pointer("/operation/cursor").ok_or_else(invalid)?;
            let stop = operation
                .get("stopOnEmptyResponse")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            keep_going |= !stop;
            if let Some(value) = operation.get("value").and_then(Value::as_str) {
                cursor = Some((value.to_owned(), !stop));
            }
        } else if entry_id.starts_with("conversationThread-") {
            for item in content
                .pointer("/timelineModule/items")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(id) = item
                    .get("entryId")
                    .and_then(Value::as_str)
                    .and_then(|id| id.strip_prefix("tweet-"))
                {
                    ids.push(id.to_owned());
                }
            }
        }
    }
    let convert = |tweet: &Value| -> Option<Value> {
        let id = tweet.get("id_str")?.as_str()?;
        let user_id = tweet.get("user_id_str")?.as_str()?;
        let user = users.get(user_id)?;
        Some(json!({
            "__typename": "Tweet",
            "rest_id": id,
            "core": {"user_results": {"result": {
                "__typename": "User", "rest_id": user_id, "legacy": user,
            }}},
            "legacy": tweet,
        }))
    };
    let mut output = Vec::new();
    for id in &ids {
        let Some(tweet) = tweets.get(id) else {
            continue; // deleted
        };
        let Some(mut result) = convert(tweet) else {
            continue;
        };
        if let Some(retweet) = tweet
            .get("retweeted_status_id_str")
            .and_then(Value::as_str)
            .and_then(|id| tweets.get(id))
            .and_then(convert)
        {
            result["legacy"]["retweeted_status_result"] = json!({"result": retweet});
        }
        if let Some(quoted) = tweet
            .get("quoted_status_id_str")
            .and_then(Value::as_str)
            .and_then(|id| tweets.get(id))
            .and_then(convert)
        {
            result["quoted_status_result"] = json!({"result": quoted});
        }
        output.push(json!({
            "entryId": format!("tweet-{id}"),
            "content": {
                "entryType": "TimelineTimelineItem",
                "itemContent": {"itemType": "TimelineTweet", "tweet_results": {"result": result}},
            },
        }));
    }
    let more = (!tweets.is_empty() || keep_going)
        .then_some(cursor)
        .flatten();
    let mut instructions = Vec::new();
    if let Some((value, keep_going)) = more {
        output.push(json!({
            "entryId": "cursor-bottom-0",
            "content": {
                "entryType": "TimelineTimelineCursor",
                "cursorType": "Bottom",
                "value": value,
                "stopOnEmptyResponse": !keep_going,
            },
        }));
        instructions.push(json!({"type": "TimelineAddEntries", "entries": output}));
    } else {
        instructions.push(json!({"type": "TimelineAddEntries", "entries": output}));
        instructions.push(terminate);
    }
    Ok(instructions)
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
    fn entry(tweet: Value) -> Value {
        json!({"content":{"entryType":"TimelineTimelineItem","itemContent":{"itemType":"TimelineTweet","tweet_results":{"result":tweet}}}})
    }
    fn cursor(value: &str) -> Value {
        json!({"content":{"entryType":"TimelineTimelineCursor","cursorType":"Bottom","value":value,"stopOnEmptyResponse":false}})
    }
    fn envelope(collection: &Collection, instructions: Value) -> Value {
        match collection {
            Collection::Bookmarks => {
                json!({"data":{"bookmark_timeline_v2":{"timeline":{"instructions":instructions}}}})
            }
            Collection::Home | Collection::HomeLatest => {
                json!({"data":{"home":{"home_timeline_urt":{"instructions":instructions}}}})
            }
            Collection::List(_) => {
                json!({"data":{"list":{"tweets_timeline":{"timeline":{"instructions":instructions}}}}})
            }
            Collection::Community(_) => {
                json!({"data":{"communityResults":{"result":{"community_media_timeline":{"timeline":{"instructions":instructions}}}}}})
            }
            Collection::Communities => {
                json!({"data":{"viewer":{"communities_timeline":{"timeline":{"instructions":instructions}}}}})
            }
            Collection::Event(_) => unreachable!("REST timeline"),
        }
    }
    fn parse(data: Value, collection: &Collection, pinned: bool) -> Result<Page> {
        parse_collection_posts(&serde_json::to_vec(&data).unwrap(), collection, pinned)
    }

    #[test]
    fn routes_are_exact_first_party_and_list_ids_are_canonical() {
        for host in [
            "x.com",
            "www.x.com",
            "mobile.x.com",
            "twitter.com",
            "www.twitter.com",
            "mobile.twitter.com",
        ] {
            for (path, expected) in [
                ("i/bookmarks", Collection::Bookmarks),
                ("i/lists/42", Collection::List("42".into())),
                ("home", Collection::HomeLatest),
                ("home/following", Collection::HomeLatest),
                ("home/foryou", Collection::Home),
                ("home/for-you", Collection::Home),
                ("home/for_you", Collection::Home),
                ("home/for%20you", Collection::Home),
                ("i/communities/42", Collection::Community("42".into())),
                ("i/communities", Collection::Communities),
                ("fixture/communities", Collection::Communities),
                ("i/events/77", Collection::Event("77".into())),
                ("i/events/77/more", Collection::Event("77".into())),
            ] {
                let url = Url::parse(&format!("https://{host}/{path}/?s=20#fragment")).unwrap();
                assert_eq!(input(&url).unwrap(), Some(expected));
                assert!(is_collection_url(&url));
            }
        }
        for path in [
            "i/bookmarks/more",
            "i/bookmarks//",
            "i/lists/42/members",
            "home/following/more",
            "HOME",
            "user/media",
            "i/communities/42/members",
            "fixture/communities/more",
            "bad!name/communities",
            "i/events",
        ] {
            assert_eq!(
                input(&Url::parse(&format!("https://x.com/{path}")).unwrap()).unwrap(),
                None
            );
        }
        for url in [
            "https://x.com/i/lists/0",
            "https://x.com/i/lists/042",
            "https://x.com/i/lists/18446744073709551616",
            "https://x.com/i/lists/PRIVATE",
            "https://x.com/i/communities/0",
            "https://x.com/i/communities/042",
            "https://x.com/i/communities/18446744073709551616",
            "https://x.com/i/communities/PRIVATE",
            "https://x.com/i/events/PRIVATE",
            "https://x.com/i/events//7",
            "https://user:PRIVATE@x.com/home",
            "https://x.com:444/home",
            "https://x.com.evil/home",
            "ftp://x.com/home",
        ] {
            let error = input(&Url::parse(url).unwrap()).unwrap_err().to_string();
            assert!(!error.contains("PRIVATE"));
        }
    }

    #[test]
    fn every_collection_uses_its_owned_envelope_and_keeps_mixed_author_order() {
        let collections = [
            Collection::Bookmarks,
            Collection::List("42".into()),
            Collection::HomeLatest,
            Collection::Home,
            Collection::Community("42".into()),
            Collection::Communities,
        ];
        for collection in &collections {
            let instructions = json!([
                {"type":"TimelineAddEntries","entries":[entry(tweet("11","42")), {"content":{"entryType":"TimelineTimelineModule","items":[{"item":{"itemContent":{"itemType":"TimelineTweet","tweet_results":{"result":tweet("12","43")}}}}]}}, cursor("next")]},
                {"type":"TimelineAddToModule","moduleItems":[{"item":{"itemContent":{"itemType":"TimelineTweet","tweet_results":{"result":tweet("13","44")}}}}]}
            ]);
            let page = parse(envelope(collection, instructions), collection, true).unwrap();
            assert_eq!(
                page.posts
                    .iter()
                    .map(|(id, _)| id.as_str())
                    .collect::<Vec<_>>(),
                ["11", "12", "13"]
            );
            assert_eq!(page.cursor.as_deref(), Some("next"));
            assert!(!page.done);
            assert!(page.continue_empty);
            let wrong = if matches!(collection, Collection::Bookmarks) {
                Collection::Home
            } else {
                Collection::Bookmarks
            };
            assert!(
                parse(
                    envelope(&wrong, json!([{"type":"TimelineAddEntries","entries":[]}])),
                    collection,
                    true
                )
                .is_err()
            );
        }
    }

    #[test]
    fn list_aliases_if_present_all_bind_to_requested_identity() {
        let collection = Collection::List("42".into());
        let base = envelope(
            &collection,
            json!([{"type":"TimelineAddEntries","entries":[]}]),
        );
        assert!(parse(base.clone(), &collection, true).unwrap().done);
        let mut aliases = base.clone();
        aliases["data"]["list"]["rest_id"] = "42".into();
        aliases["data"]["list"]["id_str"] = "42".into();
        aliases["data"]["list"]["id"] = 42.into();
        assert!(parse(aliases.clone(), &collection, true).is_ok());
        aliases["data"]["list"]["id"] = "42".into();
        assert!(parse(aliases.clone(), &collection, true).is_ok());
        aliases["data"]["list"]["id"] = "TGlzdDo0Mg==".into();
        assert!(parse(aliases.clone(), &collection, true).is_ok());
        aliases["data"]["list"]["id"] = "TGlzdDo0Mw==".into();
        assert!(parse(aliases, &collection, true).is_err());
        for field in ["rest_id", "id_str", "id"] {
            for value in [
                json!("PRIVATE"),
                json!("43"),
                json!(null),
                json!(42.0),
                json!(0),
                json!(-1),
                json!("042"),
            ] {
                if field == "id" && value == json!("PRIVATE") {
                    continue;
                }
                let mut data = base.clone();
                data["data"]["list"][field] = value;
                let error = parse(data, &collection, true).err().unwrap().to_string();
                assert!(!error.contains("PRIVATE"));
            }
        }
        assert!(parse(base, &Collection::List("042".into()), true).is_err());
    }

    #[test]
    fn community_envelope_aliases_bind_to_requested_identity_without_attributing_post_community() {
        let collection = Collection::Community("42".into());
        let base = envelope(
            &collection,
            json!([{"type":"TimelineAddEntries","entries":[entry(tweet("11","99"))]}]),
        );
        assert_eq!(
            parse(base.clone(), &collection, false).unwrap().posts.len(),
            1
        );
        let mut aliases = base.clone();
        aliases["data"]["communityResults"]["result"]["rest_id"] = json!("42");
        aliases["data"]["communityResults"]["result"]["id_str"] = json!("42");
        for id in [
            json!(42),
            json!("42"),
            json!(STANDARD.encode("Community:42")),
            json!(STANDARD_NO_PAD.encode("Community:42")),
        ] {
            aliases["data"]["communityResults"]["result"]["id"] = id;
            assert!(parse(aliases.clone(), &collection, false).is_ok());
        }
        for field in ["rest_id", "id_str", "id"] {
            for value in [
                json!("private alias"),
                json!("43"),
                json!(null),
                json!(42.0),
                json!(-1),
                json!(0),
                json!("042"),
                json!(STANDARD.encode("Community:43")),
                json!(STANDARD.encode("List:42")),
            ] {
                let mut bad = base.clone();
                bad["data"]["communityResults"]["result"][field] = value;
                let error = parse(bad, &collection, false).err().unwrap().to_string();
                assert!(!error.contains("private alias"));
            }
        }
        assert!(parse(base, &Collection::Community("042".into()), false).is_err());
    }

    #[test]
    fn pins_recommendations_and_cursor_end_rules_are_shared() {
        let collection = Collection::Bookmarks;
        let data = envelope(
            &collection,
            json!([
                {"type":"TimelineAddEntries","entries":[entry(tweet("11","42")), {"content":{"entryType":"TimelineTimelineItem","itemContent":{"itemType":"TimelineUser","nested":tweet("99","99")}}}, cursor("next")]},
                {"type":"TimelinePinEntry","entry":entry(tweet("10","44"))}
            ]),
        );
        assert_eq!(
            parse(data.clone(), &collection, true)
                .unwrap()
                .posts
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
            ["10", "11"]
        );
        assert_eq!(
            parse(data, &collection, false)
                .unwrap()
                .posts
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
            ["11"]
        );
        let empty = parse(
            envelope(
                &collection,
                json!([{"type":"TimelineAddEntries","entries":[cursor("next")]}]),
            ),
            &collection,
            false,
        )
        .unwrap();
        assert!(empty.posts.is_empty());
        assert!(empty.continue_empty);
        assert!(!empty.done);
        let ended = parse(envelope(&collection, json!([{"type":"TimelineAddEntries","entries":[cursor("next")]},{"type":"TimelineTerminateTimeline","direction":"Bottom"}])), &collection, false).unwrap();
        assert!(ended.done);
        assert!(
            parse(
                envelope(
                    &collection,
                    json!([{"type":"TimelineAddEntries","entries":[]}])
                ),
                &collection,
                false
            )
            .unwrap()
            .done
        );
    }

    #[test]
    fn malformed_owned_posts_and_private_payloads_fail_atomically() {
        let collection = Collection::Home;
        let mut bad_id = tweet("12", "42");
        bad_id["legacy"]["id_str"] = "13".into();
        let mut bad_author = tweet("12", "42");
        bad_author["core"]["user_results"]["result"]["rest_id"] = "43".into();
        let mut missing_author = tweet("12", "42");
        missing_author["legacy"]
            .as_object_mut()
            .unwrap()
            .remove("user_id_str");
        missing_author["core"]["user_results"]["result"]
            .as_object_mut()
            .unwrap()
            .remove("rest_id");
        let mut wrapped = tweet("12", "42");
        for _ in 0..4 {
            wrapped = json!({"__typename":"TweetWithVisibilityResults","tweet":wrapped});
        }
        assert_eq!(
            parse(
                envelope(
                    &collection,
                    json!([{"type":"TimelineAddEntries","entries":[entry(wrapped.clone())]}])
                ),
                &collection,
                true
            )
            .unwrap()
            .posts
            .len(),
            1
        );
        for tweet in [
            bad_id,
            bad_author,
            missing_author,
            json!({"__typename":"TweetUnavailable","reason":"PRIVATE","nested":tweet("99","42")}),
            json!({"__typename":"TweetWithVisibilityResults","tweet":wrapped}),
        ] {
            let error = parse(
                envelope(
                    &collection,
                    json!([{"type":"TimelineAddEntries","entries":[entry(tweet)]}]),
                ),
                &collection,
                true,
            )
            .err()
            .unwrap()
            .to_string();
            assert!(!error.contains("PRIVATE"));
        }
        for bytes in [
            serde_json::to_vec(&envelope(&collection, json!([{"type":"PRIVATE"}]))).unwrap(),
            serde_json::to_vec(&envelope(
                &collection,
                json!([{"type":"TimelineAddEntries","entries":[cursor("a"),cursor("b")]}]),
            ))
            .unwrap(),
            serde_json::to_vec(&envelope(&collection, json!([]))).unwrap(),
            br#"{"errors":[{"code":89,"message":"PRIVATE"}]}"#.to_vec(),
            vec![b' '; MAX_RESPONSE + 1],
        ] {
            let error = parse_collection_posts(&bytes, &collection, true)
                .err()
                .unwrap()
                .to_string();
            assert!(!error.contains("PRIVATE"));
        }
    }
    #[test]
    fn community_text_mode_selects_only_the_ranked_owned_timeline() {
        let collection = Collection::Community("42".into());
        let instructions = |post: Value| json!([{"type":"TimelineAddEntries","entries":[entry(post),cursor("opaque+/=?")] }]);
        let mut text = tweet("12", "99");
        text["legacy"]
            .as_object_mut()
            .unwrap()
            .remove("extended_entities");
        text["legacy"]["full_text"] = "A community text post".into();
        let data = json!({"data":{"communityResults":{"result":{
            "rest_id":"42",
            "community_media_timeline":{"timeline":{"instructions":instructions(tweet("11", "98"))}},
            "ranked_community_timeline":{"timeline":{"instructions":instructions(text)}}}}}});
        for (enabled, expected) in [(false, "11"), (true, "12")] {
            let page = parse_collection_posts_with_text(
                &serde_json::to_vec(&data).unwrap(),
                &collection,
                false,
                enabled,
            )
            .unwrap();
            assert_eq!(page.posts.len(), 1);
            assert_eq!(page.posts[0].0, expected);
            assert_eq!(page.posts[0].1["legacy"]["id_str"], expected);
            assert_eq!(page.cursor.as_deref(), Some("opaque+/=?"));
            assert!(!page.done);
            assert!(page.continue_empty);
        }
        assert_eq!(
            parse(data.clone(), &collection, false).unwrap().posts[0].0,
            "11",
            "legacy API keeps media mode"
        );
        for enabled in [false, true] {
            let selected = if enabled {
                "ranked_community_timeline"
            } else {
                "community_media_timeline"
            };
            for invalid in [
                Value::Null,
                json!({"timeline":{"instructions":"private malformed instructions"}}),
            ] {
                let mut wrong = data.clone();
                wrong["data"]["communityResults"]["result"][selected] = invalid;
                let error = parse_collection_posts_with_text(
                    &serde_json::to_vec(&wrong).unwrap(),
                    &collection,
                    false,
                    enabled,
                )
                .err()
                .unwrap()
                .to_string();
                assert!(!error.contains("private malformed instructions"));
            }
            let mut missing = data.clone();
            missing["data"]["communityResults"]["result"]
                .as_object_mut()
                .unwrap()
                .remove(selected);
            assert!(
                parse_collection_posts_with_text(
                    &serde_json::to_vec(&missing).unwrap(),
                    &collection,
                    false,
                    enabled
                )
                .is_err(),
                "must not fall back to the other envelope"
            );
        }
    }

    #[test]
    fn community_text_mode_preserves_identity_checks_and_pin_selection() {
        let collection = Collection::Community("42".into());
        let data = json!({"data":{"communityResults":{"result":{
            "id":STANDARD.encode("Community:42"), "id_str":"42",
            "ranked_community_timeline":{"timeline":{"instructions":[
                {"type":"TimelinePinEntry","entry":entry(tweet("10","88"))},
                {"type":"TimelineAddEntries","entries":[entry(tweet("11","99"))]}
            ]}}
        }}}});
        for (pinned, expected) in [(false, vec!["11"]), (true, vec!["10", "11"])] {
            let page = parse_collection_posts_with_text(
                &serde_json::to_vec(&data).unwrap(),
                &collection,
                pinned,
                true,
            )
            .unwrap();
            assert_eq!(
                page.posts
                    .iter()
                    .map(|(id, _)| id.as_str())
                    .collect::<Vec<_>>(),
                expected
            );
            assert!(page.done);
        }
        for field in ["rest_id", "id_str", "id"] {
            let mut wrong = data.clone();
            wrong["data"]["communityResults"]["result"][field] = "private-wrong-community".into();
            let error = parse_collection_posts_with_text(
                &serde_json::to_vec(&wrong).unwrap(),
                &collection,
                false,
                true,
            )
            .err()
            .unwrap()
            .to_string();
            assert!(!error.contains("private-wrong-community"));
        }
        let bookmark = envelope(
            &Collection::Bookmarks,
            json!([{"type":"TimelineAddEntries","entries":[entry(tweet("11","99"))]}]),
        );
        let ordinary = parse(bookmark.clone(), &Collection::Bookmarks, false).unwrap();
        let text = parse_collection_posts_with_text(
            &serde_json::to_vec(&bookmark).unwrap(),
            &Collection::Bookmarks,
            false,
            true,
        )
        .unwrap();
        assert_eq!(
            ordinary.posts, text.posts,
            "other feeds retain their exact owned envelopes"
        );
    }
}
