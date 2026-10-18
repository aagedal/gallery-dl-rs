//! Identity-bound parsing of the X media timeline GraphQL envelope.
//! No recursive search: quoted posts and recommendation payloads are not posts.
use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ProfileInput {
    Name(String),
    Id(String),
}

pub fn is_media_timeline_url(url: &Url) -> bool {
    input(url).is_ok()
}

pub(super) fn input(url: &Url) -> Result<ProfileInput> {
    let invalid = || {
        error(
            "native Twitter media timelines require a first-party /USER/media URL; use --extractor compatibility for other URLs",
        )
    };
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
        return Err(invalid());
    }
    let path = url.path().strip_suffix('/').unwrap_or(url.path());
    let mut parts = path.split('/');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(""), Some(user), Some("media"), None) => {
            if let Some(id) = user.strip_prefix("id:") {
                if valid_id(id) {
                    return Ok(ProfileInput::Id(id.into()));
                }
            } else if valid_name(user) {
                return Ok(ProfileInput::Name(user.into()));
            }
            Err(invalid())
        }
        _ => Err(invalid()),
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 15
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn document(bytes: &[u8]) -> Result<Value> {
    if bytes.len() > MAX_RESPONSE {
        return Err(error("Twitter response exceeded 16 MiB"));
    }
    let data = serde_json::from_slice(bytes).map_err(|_| error("Twitter returned a web page or invalid JSON; login or a client transaction challenge may be required"))?;
    check_errors(&data)?;
    Ok(data)
}

pub(super) fn parse_user(bytes: &[u8], input: &ProfileInput) -> Result<String> {
    let data = document(bytes)?;
    let user = data
        .pointer("/data/user/result")
        .ok_or_else(|| error("Twitter user unavailable or unexpected GraphQL response"))?;
    if user.get("__typename").and_then(Value::as_str) != Some("User") {
        return Err(error(
            "Twitter user unavailable or inaccessible to this session",
        ));
    }
    let id = user
        .get("rest_id")
        .and_then(Value::as_str)
        .filter(|id| valid_id(id))
        .ok_or_else(|| error("Twitter user identifier missing or invalid"))?;
    match input {
        ProfileInput::Name(expected) => {
            let names: Vec<_> = [
                user.pointer("/core/screen_name"),
                user.pointer("/legacy/screen_name"),
            ]
            .into_iter()
            .flatten()
            .collect();
            if names.is_empty()
                || names.iter().any(|name| {
                    !name
                        .as_str()
                        .is_some_and(|name| valid_name(name) && name.eq_ignore_ascii_case(expected))
                })
            {
                return Err(error("Twitter response does not match the requested user"));
            }
        }
        ProfileInput::Id(expected) if expected != id => {
            return Err(error("Twitter response does not match the requested user"));
        }
        ProfileInput::Id(_) => {}
    }
    Ok(id.into())
}

pub(super) struct Page {
    #[cfg(test)]
    pub(super) media: Vec<Media>,
    pub(super) posts: Vec<(String, Value)>,
    pub(super) cursor: Option<String>,
    pub(super) done: bool,
}

#[cfg(test)]
pub(super) fn parse_page(bytes: &[u8], owner_id: &str) -> Result<Page> {
    let mut page = parse_posts(bytes, owner_id)?;
    for (id, post) in std::mem::take(&mut page.posts) {
        let mut engine = expansion::Engine::new(post, id, ExpansionOptions::default(), true);
        loop {
            match engine.next()? {
                expansion::Action::Media(media) => page.media.extend(media),
                expansion::Action::Done => break,
                expansion::Action::Fetch(_) => {
                    return Err(error("Twitter linked post result missing"));
                }
            }
        }
    }
    Ok(page)
}

pub(super) fn parse_posts(bytes: &[u8], owner_id: &str) -> Result<Page> {
    if !valid_id(owner_id) {
        return Err(error("invalid Twitter timeline owner identifier"));
    }
    let data = document(bytes)?;
    let user = data
        .pointer("/data/user/result")
        .ok_or_else(|| error("Twitter media timeline user missing"))?;
    if user.get("__typename").and_then(Value::as_str) != Some("User")
        || user.get("rest_id").and_then(Value::as_str) != Some(owner_id)
    {
        return Err(error("Twitter timeline does not match the requested user"));
    }
    let instructions = match (
        user.pointer("/timeline/timeline/instructions"),
        user.pointer("/timeline_v2/timeline/instructions"),
    ) {
        (Some(instructions), None) | (None, Some(instructions)) => instructions,
        _ => {
            return Err(error(
                "Twitter media timeline instructions missing or ambiguous",
            ));
        }
    }
    .as_array()
    .ok_or_else(|| error("Twitter media timeline instructions are invalid"))?;
    let mut page = Page {
        #[cfg(test)]
        media: Vec::new(),
        posts: Vec::new(),
        cursor: None,
        done: false,
    };
    let mut entries_seen = false;
    for instruction in instructions {
        match instruction.get("type").and_then(Value::as_str) {
            Some("TimelineAddEntries") => {
                let entries = instruction
                    .get("entries")
                    .and_then(Value::as_array)
                    .ok_or_else(|| error("Twitter media timeline entries missing"))?;
                entries_seen = true;
                for entry in entries {
                    parse_entry(entry, owner_id, &mut page)?;
                }
            }
            Some("TimelineAddToModule") => {
                let items = instruction
                    .get("moduleItems")
                    .and_then(Value::as_array)
                    .ok_or_else(|| error("Twitter media timeline module items missing"))?;
                entries_seen = true;
                for item in items {
                    parse_module_item(item, owner_id, &mut page)?;
                }
            }
            Some("TimelinePinEntry") => {
                parse_entry(
                    instruction
                        .get("entry")
                        .ok_or_else(|| error("Twitter pinned timeline entry missing"))?,
                    owner_id,
                    &mut page,
                )?;
            }
            Some("TimelineReplaceEntry") => {
                let entry = instruction
                    .get("entry")
                    .ok_or_else(|| error("Twitter replacement timeline entry missing"))?;
                let content = entry
                    .get("content")
                    .ok_or_else(|| error("Twitter replacement timeline content missing"))?;
                if !is_cursor(content) {
                    return Err(error("unsupported Twitter media timeline replacement"));
                }
                parse_cursor(content, &mut page)?;
            }
            Some("TimelineTerminateTimeline") => {
                match instruction.get("direction").and_then(Value::as_str) {
                    Some("Bottom") => page.done = true,
                    Some("Top") => {}
                    _ => {
                        return Err(error(
                            "Twitter media timeline termination direction missing or unsupported",
                        ));
                    }
                }
            }
            Some("TimelineClearCache") => {
                return Err(error(
                    "Twitter media timeline requested a cache reset; retry extraction",
                ));
            }
            _ => return Err(error("unsupported Twitter media timeline instruction")),
        }
    }
    if !entries_seen && !page.done && page.cursor.is_none() {
        return Err(error(
            "Twitter media timeline has no entries or explicit end",
        ));
    }
    // A well-formed entries response without a Bottom cursor is the API's end.
    // Empty responses with an advancing cursor must continue, not truncate.
    page.done |= page.cursor.is_none();
    Ok(page)
}

fn promoted(value: &Value) -> bool {
    value.get("promotedMetadata").is_some_and(|v| !v.is_null())
}

fn is_cursor(content: &Value) -> bool {
    content.get("entryType").and_then(Value::as_str) == Some("TimelineTimelineCursor")
        || content.get("cursorType").is_some()
        || content.pointer("/itemContent/cursorType").is_some()
        || content
            .pointer("/itemContent/itemType")
            .and_then(Value::as_str)
            == Some("TimelineTimelineCursor")
}

fn parse_cursor(content: &Value, page: &mut Page) -> Result<()> {
    let field = |name: &str| -> Result<Option<&Value>> {
        let outer = content.get(name);
        let inner = content.get("itemContent").and_then(|item| item.get(name));
        if outer.is_some() && inner.is_some() && outer != inner {
            return Err(error("Twitter media timeline cursor fields conflict"));
        }
        Ok(outer.or(inner))
    };
    match field("cursorType")?.and_then(Value::as_str) {
        Some("Top" | "ShowMore" | "ShowMoreThreads") => Ok(()),
        Some("Bottom") => {
            let value = field("value")?
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty() && v.len() <= 8192)
                .ok_or_else(|| error("Twitter media timeline Bottom cursor missing or invalid"))?;
            if page.cursor.as_deref().is_some_and(|old| old != value) {
                return Err(error(
                    "Twitter media timeline has conflicting Bottom cursors",
                ));
            }
            page.cursor = Some(value.into());
            Ok(())
        }
        _ => Err(error(
            "Twitter media timeline cursor type missing or unsupported",
        )),
    }
}

fn parse_entry(entry: &Value, owner_id: &str, page: &mut Page) -> Result<()> {
    let content = entry
        .get("content")
        .ok_or_else(|| error("Twitter media timeline entry content missing"))?;
    if promoted(entry) || promoted(content) {
        return Ok(());
    }
    if is_cursor(content) {
        return parse_cursor(content, page);
    }
    match content.get("entryType").and_then(Value::as_str) {
        Some("TimelineTimelineItem") => parse_item(
            content
                .get("itemContent")
                .ok_or_else(|| error("Twitter media timeline item missing"))?,
            owner_id,
            page,
        ),
        Some("TimelineTimelineModule") => {
            let items = content
                .get("items")
                .and_then(Value::as_array)
                .ok_or_else(|| error("Twitter media timeline module items missing"))?;
            for item in items {
                parse_module_item(item, owner_id, page)?;
            }
            Ok(())
        }
        _ => Err(error("unsupported Twitter media timeline entry")),
    }
}

fn parse_module_item(item: &Value, owner_id: &str, page: &mut Page) -> Result<()> {
    if promoted(item) || item.get("item").is_some_and(promoted) {
        return Ok(());
    }
    let content = item
        .pointer("/item/itemContent")
        .ok_or_else(|| error("Twitter media timeline module item content missing"))?;
    parse_item(content, owner_id, page)
}

fn parse_item(item: &Value, owner_id: &str, page: &mut Page) -> Result<()> {
    if promoted(item) {
        return Ok(());
    }
    match item.get("itemType").and_then(Value::as_str) {
        Some("TimelineUser" | "TimelineTrend" | "TimelineTwitterList") => return Ok(()),
        Some("TimelineTweet") => {}
        _ => return Err(error("unsupported Twitter media timeline item")),
    }
    let original = item
        .pointer("/tweet_results/result")
        .ok_or_else(|| error("Twitter media timeline post result missing"))?;
    let mut tweet = original;
    for _ in 0..4 {
        if tweet.get("__typename").and_then(Value::as_str) != Some("TweetWithVisibilityResults") {
            break;
        }
        tweet = tweet
            .get("tweet")
            .ok_or_else(|| error("Twitter visibility result has no post"))?;
    }
    if tweet.get("__typename").and_then(Value::as_str) != Some("Tweet") {
        return Err(error(
            "Twitter timeline post unavailable or inaccessible to this session",
        ));
    }
    let id = tweet
        .get("rest_id")
        .and_then(Value::as_str)
        .filter(|id| valid_id(id))
        .ok_or_else(|| error("Twitter media timeline post identifier missing or invalid"))?;
    let legacy = tweet
        .get("legacy")
        .filter(|v| v.is_object())
        .ok_or_else(|| error("Twitter timeline post media fields missing"))?;
    if legacy.get("id_str").is_some_and(|v| v.as_str() != Some(id)) {
        return Err(error("Twitter response does not match the requested post"));
    }
    let owners: Vec<_> = [
        legacy.get("user_id_str"),
        tweet.pointer("/core/user_results/result/rest_id"),
    ]
    .into_iter()
    .flatten()
    .collect();
    if owners.is_empty() || owners.iter().any(|v| v.as_str() != Some(owner_id)) {
        return Err(error(
            "Twitter timeline post does not match the requested user",
        ));
    }
    page.posts.push((id.into(), original.clone()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tweet(id: &str) -> Value {
        let mut data: Value =
            serde_json::from_str(include_str!("../tests/fixtures/twitter/photo.json")).unwrap();
        let tweet = &mut data["data"]["tweetResult"]["result"];
        tweet["rest_id"] = id.into();
        tweet["legacy"]["id_str"] = id.into();
        tweet["legacy"]["user_id_str"] = "42".into();
        tweet["core"]["user_results"]["result"]["rest_id"] = "42".into();
        tweet.clone()
    }

    fn item(tweet: Value) -> Value {
        json!({"itemType":"TimelineTweet", "tweet_results":{"result":tweet}})
    }
    fn entry(tweet: Value) -> Value {
        json!({"content":{"entryType":"TimelineTimelineItem","itemContent":item(tweet)}})
    }
    fn cursor(value: &str) -> Value {
        json!({"content":{"entryType":"TimelineTimelineCursor","cursorType":"Bottom","value":value}})
    }
    fn page(instructions: Value) -> Vec<u8> {
        serde_json::to_vec(&json!({"data":{"user":{"result":{"__typename":"User","rest_id":"42","timeline":{"timeline":{"instructions":instructions}}}}}})).unwrap()
    }
    fn entries(entries: Value) -> Vec<u8> {
        page(json!([{"type":"TimelineAddEntries","entries":entries}]))
    }
    fn parse(bytes: Vec<u8>) -> Page {
        parse_page(&bytes, "42").unwrap()
    }

    #[test]
    fn validates_exact_first_party_media_urls() {
        for host in [
            "x.com",
            "www.x.com",
            "mobile.x.com",
            "twitter.com",
            "www.twitter.com",
            "mobile.twitter.com",
        ] {
            assert_eq!(
                input(&Url::parse(&format!("https://{host}/User_123/media/?s=20")).unwrap())
                    .unwrap(),
                ProfileInput::Name("User_123".into())
            );
            assert_eq!(
                input(&Url::parse(&format!("http://{host}/id:42/media")).unwrap()).unwrap(),
                ProfileInput::Id("42".into())
            );
        }
        for url in [
            "https://x.com/user",
            "https://x.com/user/media//",
            "https://x.com/user/media/more",
            "https://x.com/id:0/media",
            "https://x.com/id:01/media",
            "https://x.com/id:18446744073709551616/media",
            "https://x.com/a-b/media",
            "https://x.com/abcdefghijklmnop/media",
            "https://x.com/a%2fb/media",
            "https://x.com:444/user/media",
            "https://user@x.com/user/media",
            "https://x.com.evil/user/media",
            "ftp://x.com/user/media",
        ] {
            assert!(input(&Url::parse(url).unwrap()).is_err(), "{url}");
        }
    }

    #[test]
    fn user_lookup_is_typed_and_identity_bound() {
        let user =
            |value: Value| serde_json::to_vec(&json!({"data":{"user":{"result":value}}})).unwrap();
        let good = json!({"__typename":"User", "rest_id":"42", "core":{"screen_name":"Fixture_User"}, "legacy":{"screen_name":"fixture_user"}});
        assert_eq!(
            parse_user(
                &user(good.clone()),
                &ProfileInput::Name("FIXTURE_user".into())
            )
            .unwrap(),
            "42"
        );
        assert_eq!(
            parse_user(&user(good.clone()), &ProfileInput::Id("42".into())).unwrap(),
            "42"
        );
        assert!(parse_user(&user(good.clone()), &ProfileInput::Name("other".into())).is_err());
        assert!(parse_user(&user(good.clone()), &ProfileInput::Id("43".into())).is_err());
        for bad in [
            json!({"__typename":"UserUnavailable","rest_id":"42"}),
            json!({"__typename":"User","rest_id":"0"}),
            json!({"__typename":"User","rest_id":"42","core":{"screen_name":"fixture_user"},"legacy":{"screen_name":"other"}}),
        ] {
            assert!(parse_user(&user(bad), &ProfileInput::Name("fixture_user".into())).is_err());
        }
        let err = parse_user(
            br#"{"errors":[{"code":89,"message":"PRIVATE SESSION VALUE"}]}"#,
            &ProfileInput::Id("42".into()),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("authentication"));
        assert!(!err.contains("PRIVATE"));
    }

    #[test]
    fn preserves_structured_grid_and_incremental_module_order() {
        let instructions = json!([
            {"type":"TimelinePinEntry","entry":entry(tweet("10"))},
            {"type":"TimelineAddEntries","entries":[entry(tweet("11")),{"content":{"entryType":"TimelineTimelineModule","items":[{"item":{"itemContent":item(tweet("12"))}},{"item":{"itemContent":item(tweet("13"))}}]}},cursor("next")]},
            {"type":"TimelineAddToModule","moduleItems":[{"item":{"itemContent":item(tweet("14"))}}]}
        ]);
        let output = parse(page(instructions));
        assert_eq!(
            output
                .media
                .iter()
                .map(|m| m.metadata.as_ref().unwrap().post_id.as_str())
                .collect::<Vec<_>>(),
            ["10", "11", "12", "13", "14"]
        );
        assert_eq!(output.cursor.as_deref(), Some("next"));
        assert!(!output.done);
    }

    #[test]
    fn empty_pages_and_explicit_cursor_replacement_and_termination() {
        assert!(parse(entries(json!([]))).done);
        let empty_advancing = parse(entries(json!([cursor("next")])));
        assert!(empty_advancing.media.is_empty());
        assert!(!empty_advancing.done);
        assert_eq!(empty_advancing.cursor.as_deref(), Some("next"));
        let replacement = parse(page(json!([
            {"type":"TimelineAddEntries","entries":[]},
            {"type":"TimelineReplaceEntry","entry":cursor("next")}
        ])));
        assert_eq!(replacement.cursor.as_deref(), Some("next"));
        assert!(!replacement.done);
        let nested = parse(entries(
            json!([{"content":{"entryType":"TimelineTimelineCursor","cursorType":"Bottom","itemContent":{"value":"nested"}}}]),
        ));
        assert_eq!(nested.cursor.as_deref(), Some("nested"));
        assert!(!nested.done);
        assert!(parse_page(&entries(json!([{"content":{"entryType":"TimelineTimelineCursor","cursorType":"Bottom","value":"a","itemContent":{"value":"b"}}}])), "42").is_err());
        let end = parse(page(
            json!([{"type":"TimelineTerminateTimeline","direction":"Bottom"}]),
        ));
        assert!(end.done);
        assert!(parse_page(&page(json!([])), "42").is_err());
        assert!(parse_page(&entries(json!([cursor("a"), cursor("b")])), "42").is_err());
        assert!(parse_page(&entries(json!([cursor("")])), "42").is_err());
        assert!(parse_page(&entries(json!([cursor(&"x".repeat(8193))])), "42").is_err());
    }

    #[test]
    fn skips_text_recommendations_and_promoted_items_without_recursive_search() {
        let mut text = tweet("10");
        text["legacy"]
            .as_object_mut()
            .unwrap()
            .remove("extended_entities");
        let mut promoted = entry(tweet("11"));
        promoted["content"]["itemContent"]["promotedMetadata"] = json!({"advertiser":"synthetic"});
        let output = parse(entries(
            json!([entry(text),promoted,{"content":{"entryType":"TimelineTimelineItem","itemContent":{"itemType":"TimelineUser","nested":tweet("12")}}},entry(tweet("13"))]),
        ));
        assert_eq!(output.media.len(), 1);
        assert_eq!(output.media[0].metadata.as_ref().unwrap().post_id, "13");
    }

    #[test]
    fn rejects_missing_owner_mismatches_unavailable_and_malformed_media() {
        let mut wrong_owner = tweet("10");
        wrong_owner["legacy"]["user_id_str"] = "43".into();
        let mut missing_owner = tweet("11");
        missing_owner["legacy"]
            .as_object_mut()
            .unwrap()
            .remove("user_id_str");
        missing_owner["core"]["user_results"]["result"]
            .as_object_mut()
            .unwrap()
            .remove("rest_id");
        let mut malformed = tweet("12");
        malformed["legacy"]["extended_entities"]["media"] = Value::Null;
        let mut incomplete = tweet("13");
        incomplete["legacy"]
            .as_object_mut()
            .unwrap()
            .remove("extended_entities");
        incomplete["legacy"]["entities"] = json!({"media":[{"id":"1"}]});
        let mut retweet = tweet("14");
        retweet["legacy"]["retweeted_status_id_str"] = "99".into();
        assert!(
            parse_page(&entries(json!([entry(retweet)])), "42")
                .unwrap()
                .media
                .is_empty()
        );
        for bad in [
            wrong_owner,
            missing_owner,
            malformed,
            incomplete,
            json!({"__typename":"TweetUnavailable","reason":"PRIVATE"}),
        ] {
            let error = parse_page(&entries(json!([entry(bad)])), "42")
                .err()
                .unwrap()
                .to_string();
            assert!(!error.contains("PRIVATE"));
        }
        assert!(parse_page(&entries(json!([])), "43").is_err());
    }

    #[test]
    fn unknown_envelopes_entries_and_instructions_fail_instead_of_truncating() {
        for instructions in [
            json!([{"type":"NewInstruction","entries":[]}]),
            json!([{"type":"TimelineAddEntries","entries":null}]),
            json!([{"type":"TimelineAddEntries","entries":[{"content":{"entryType":"Unknown","nested":tweet("10")}}]}]),
            json!([{"type":"TimelineAddEntries","entries":[{"content":{"entryType":"TimelineTimelineCursor","value":"x"}}]}]),
            json!([{"type":"TimelineReplaceEntry","entry":entry(tweet("10"))}]),
        ] {
            assert!(parse_page(&page(instructions), "42").is_err());
        }
        let mut envelope: Value = serde_json::from_slice(&entries(json!([]))).unwrap();
        envelope["data"]["user"]["result"]["timeline_v2"] =
            envelope["data"]["user"]["result"]["timeline"].take();
        assert!(parse_page(&serde_json::to_vec(&envelope).unwrap(), "42").is_ok());
        envelope["data"]["user"]["result"]["timeline"] =
            envelope["data"]["user"]["result"]["timeline_v2"].clone();
        assert!(parse_page(&serde_json::to_vec(&envelope).unwrap(), "42").is_err());
    }
}
