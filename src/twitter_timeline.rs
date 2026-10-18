//! Identity-bound parsing of explicit X profile timeline GraphQL envelopes.
//! No recursive search: quoted posts and recommendation payloads are not posts.
use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ProfileInput {
    Name(String),
    Id(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProfileTab {
    Media,
    Tweets,
    Replies,
    Highlights,
    Likes,
    Timeline,
}

pub fn is_media_timeline_url(url: &Url) -> bool {
    matches!(profile_input(url), Ok((_, ProfileTab::Media)))
}

pub fn is_profile_timeline_url(url: &Url) -> bool {
    profile_input(url).is_ok()
}

#[cfg(test)]
pub(super) fn input(url: &Url) -> Result<ProfileInput> {
    match profile_input(url)? {
        (user, ProfileTab::Media) => Ok(user),
        _ => Err(error("native Twitter media timeline URL required")),
    }
}

pub(super) fn profile_input(url: &Url) -> Result<(ProfileInput, ProfileTab)> {
    let invalid = || {
        error(
            "native Twitter profile timelines require a supported first-party profile URL; use --extractor compatibility for other URLs",
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
    let parts: Vec<_> = path.split('/').collect();
    if let ["", "intent", "user"] = parts.as_slice() {
        let ids: Vec<_> = url
            .query_pairs()
            .filter(|(key, _)| key == "user_id")
            .collect();
        let [(_, id)] = ids.as_slice() else {
            return Err(invalid());
        };
        if !valid_id(id) {
            return Err(invalid());
        }
        return Ok((ProfileInput::Id(id.to_string()), ProfileTab::Timeline));
    }
    if let ["", "i", "user", id] = parts.as_slice() {
        if !valid_id(id) {
            return Err(invalid());
        }
        return Ok((ProfileInput::Id((*id).into()), ProfileTab::Timeline));
    }
    let (user, tab) = match parts.as_slice() {
        ["", user] => (*user, ProfileTab::Timeline),
        ["", user, tab] => (
            *user,
            match *tab {
                "media" => ProfileTab::Media,
                "tweets" => ProfileTab::Tweets,
                "with_replies" => ProfileTab::Replies,
                "highlights" => ProfileTab::Highlights,
                "likes" => ProfileTab::Likes,
                "timeline" => ProfileTab::Timeline,
                _ => return Err(invalid()),
            },
        ),
        _ => return Err(invalid()),
    };
    let user = if let Some(id) = user.strip_prefix("id:") {
        if !valid_id(id) {
            return Err(invalid());
        }
        ProfileInput::Id(id.into())
    } else if valid_name(user) && !reserved_name(user) {
        ProfileInput::Name(user.into())
    } else {
        return Err(invalid());
    };
    Ok((user, tab))
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 15
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn reserved_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "home"
            | "search"
            | "explore"
            | "settings"
            | "i"
            | "notifications"
            | "messages"
            | "hashtag"
            | "intent"
            | "login"
            | "logout"
            | "signup"
            | "compose"
            | "account"
            | "tos"
            | "privacy"
            | "help"
            | "about"
            | "jobs"
            | "download"
            | "share"
            | "widgets"
            | "oauth"
    )
}

pub(super) fn document(bytes: &[u8]) -> Result<Value> {
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

pub(super) fn parse_user_name(bytes: &[u8], owner: &str) -> Result<String> {
    if !valid_id(owner) {
        return Err(error("invalid Twitter user identifier"));
    }
    let data = document(bytes)?;
    let user = data
        .pointer("/data/user/result")
        .ok_or_else(|| error("Twitter user result missing"))?;
    if user["__typename"] != "User" || user["rest_id"].as_str() != Some(owner) {
        return Err(error("Twitter response does not match the requested user"));
    }
    let names: Vec<_> = [
        user.pointer("/core/screen_name"),
        user.pointer("/legacy/screen_name"),
    ]
    .into_iter()
    .flatten()
    .collect();
    let name = names
        .first()
        .and_then(|v| v.as_str())
        .filter(|name| valid_name(name))
        .ok_or_else(|| error("Twitter user screen name missing or invalid"))?;
    if names.iter().any(|value| {
        !value
            .as_str()
            .is_some_and(|other| valid_name(other) && other.eq_ignore_ascii_case(name))
    }) {
        return Err(error("Twitter user screen names conflict"));
    }
    Ok(name.into())
}

pub(super) fn user_pin(bytes: &[u8], owner_id: &str) -> Result<Option<String>> {
    let data = document(bytes)?;
    let user = data
        .pointer("/data/user/result")
        .ok_or_else(|| error("Twitter user result missing"))?;
    if user["__typename"] != "User" || user["rest_id"].as_str() != Some(owner_id) {
        return Err(error("Twitter response does not match the requested user"));
    }
    pinned_id(user)
}

fn pinned_id(user: &Value) -> Result<Option<String>> {
    let Some(value) = user
        .pointer("/legacy/pinned_tweet_ids_str")
        .filter(|v| !v.is_null())
    else {
        return Ok(None);
    };
    let ids = value
        .as_array()
        .filter(|ids| ids.len() <= 32)
        .ok_or_else(|| error("Twitter pinned post identifiers malformed"))?;
    if ids.iter().any(|id| !id.as_str().is_some_and(valid_id)) {
        return Err(error("Twitter pinned post identifier invalid"));
    }
    Ok(ids.first().and_then(Value::as_str).map(str::to_owned))
}

pub(super) struct Page {
    #[cfg(test)]
    pub(super) media: Vec<Media>,
    pub(super) posts: Vec<(String, Value)>,
    pub(super) cursor: Option<String>,
    pub(super) done: bool,
    pub(super) continue_empty: bool,
    tab: ProfileTab,
}

#[cfg(test)]
pub(super) fn parse_page(bytes: &[u8], owner_id: &str) -> Result<Page> {
    let mut page = parse_profile_posts(bytes, owner_id, ProfileTab::Media, true)?;
    for (id, post) in std::mem::take(&mut page.posts) {
        let mut engine = expansion::Engine::new(post, id, ExpansionOptions::default(), true);
        loop {
            match engine.next()? {
                expansion::Action::Post(post) => page.media.extend(post.media),
                expansion::Action::Done => break,
                expansion::Action::Fetch(_) => {
                    return Err(error("Twitter linked post result missing"));
                }
            }
        }
    }
    Ok(page)
}

#[cfg(test)]
pub(super) fn parse_profile_posts(
    bytes: &[u8],
    owner_id: &str,
    tab: ProfileTab,
    pinned: bool,
) -> Result<Page> {
    parse_profile_posts_with_pin(bytes, owner_id, tab, pinned, None)
}

pub(super) fn parse_profile_posts_with_pin(
    bytes: &[u8],
    owner_id: &str,
    tab: ProfileTab,
    pinned: bool,
    pinned_hint: Option<&str>,
) -> Result<Page> {
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
    let inferred_pin = if pinned
        && !instructions
            .iter()
            .any(|instruction| instruction["type"] == "TimelinePinEntry")
    {
        pinned_id(user)?.or_else(|| pinned_hint.map(str::to_owned))
    } else {
        None
    };
    parse_instructions(instructions, Some(owner_id), tab, pinned, inferred_pin)
}

#[cfg(test)]
pub(super) fn parse_search_posts(bytes: &[u8]) -> Result<Page> {
    parse_search_posts_with_pin(bytes, true)
}

pub(super) fn parse_search_posts_with_pin(bytes: &[u8], pinned: bool) -> Result<Page> {
    let data = document(bytes)?;
    let instructions = data
        .pointer("/data/search_by_raw_query/search_timeline/timeline/instructions")
        .and_then(Value::as_array)
        .ok_or_else(|| error("Twitter search timeline instructions missing or invalid"))?;
    parse_instructions(instructions, None, ProfileTab::Timeline, pinned, None)
}

pub(super) fn parse_instructions(
    instructions: &[Value],
    owner_id: Option<&str>,
    tab: ProfileTab,
    pinned: bool,
    inferred_pin: Option<String>,
) -> Result<Page> {
    let mut page = Page {
        #[cfg(test)]
        media: Vec::new(),
        posts: Vec::new(),
        cursor: None,
        done: false,
        continue_empty: false,
        tab,
    };
    let mut pinned_entry = None;
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
                entries_seen = true;
                if pinned {
                    let entry = instruction
                        .get("entry")
                        .ok_or_else(|| error("Twitter pinned timeline entry missing"))?;
                    if pinned_entry.is_some() {
                        return Err(error("Twitter pinned timeline entries conflict"));
                    }
                    pinned_entry = Some(entry);
                }
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
    if let Some(entry) = pinned_entry {
        let ordinary = std::mem::take(&mut page.posts);
        parse_entry(entry, owner_id, &mut page)?;
        page.posts.extend(ordinary);
        entries_seen = true;
    }
    if let Some(id) = inferred_pin
        && let Some(index) = page.posts.iter().position(|(post_id, _)| post_id == &id)
    {
        let post = page.posts.remove(index);
        page.posts.insert(0, post);
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
            let stop_on_empty = field("stopOnEmptyResponse")?
                .map(|value| {
                    value.as_bool().ok_or_else(|| {
                        error("Twitter timeline cursor stop-on-empty setting is invalid")
                    })
                })
                .transpose()?;
            page.continue_empty |= stop_on_empty == Some(false);
            page.cursor = Some(value.into());
            Ok(())
        }
        _ => Err(error(
            "Twitter media timeline cursor type missing or unsupported",
        )),
    }
}

fn parse_entry(entry: &Value, owner_id: Option<&str>, page: &mut Page) -> Result<()> {
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

fn parse_module_item(item: &Value, owner_id: Option<&str>, page: &mut Page) -> Result<()> {
    if promoted(item) || item.get("item").is_some_and(promoted) {
        return Ok(());
    }
    let content = item
        .pointer("/item/itemContent")
        .ok_or_else(|| error("Twitter media timeline module item content missing"))?;
    parse_item(content, owner_id, page)
}

fn parse_item(item: &Value, owner_id: Option<&str>, page: &mut Page) -> Result<()> {
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
    let author = owners
        .first()
        .and_then(|v| v.as_str())
        .filter(|id| valid_id(id))
        .ok_or_else(|| error("Twitter timeline post owner identifier missing or invalid"))?;
    if owners.iter().any(|v| v.as_str() != Some(author)) {
        return Err(error("Twitter timeline post owner fields conflict"));
    }
    // Conversation modules on posts/replies/highlights can include other authors.
    // The enclosing user result binds membership; each post still proves its own ID.
    if page.tab == ProfileTab::Media && owner_id != Some(author) {
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
    fn explicit_profile_tabs_accept_only_valid_first_party_paths() {
        for (route, tab) in [
            ("media", ProfileTab::Media),
            ("tweets", ProfileTab::Tweets),
            ("with_replies", ProfileTab::Replies),
            ("highlights", ProfileTab::Highlights),
            ("likes", ProfileTab::Likes),
        ] {
            for user in ["User_123", "id:42"] {
                let url = Url::parse(&format!(
                    "https://twitter.com/{user}/{route}/?s=20#fragment"
                ))
                .unwrap();
                let (_, actual) = profile_input(&url).unwrap();
                assert_eq!(actual, tab);
                assert!(is_profile_timeline_url(&url));
                assert_eq!(is_media_timeline_url(&url), tab == ProfileTab::Media);
            }
        }
        for url in [
            "https://x.com/user/followers",
            "https://x.com/user/tweets/extra",
            "https://x.com/user/with-replies",
            "https://x.com/id:0/tweets",
            "https://x.com/id:042/tweets",
            "https://x.com/a%2fb/highlights",
            "https://x.com/a/tweets//",
            "https://x.com.evil/a/tweets",
            "https://user:secret@x.com/a/tweets",
            "https://x.com:444/a/tweets",
        ] {
            assert!(!is_profile_timeline_url(&Url::parse(url).unwrap()), "{url}");
        }
    }

    #[test]
    fn timeline_routes_include_numeric_profiles_and_reject_reserved_routes() {
        for route in ["Fixture_User", "Fixture_User/timeline"] {
            assert_eq!(
                profile_input(&Url::parse(&format!("https://x.com/{route}")).unwrap()).unwrap(),
                (
                    ProfileInput::Name("Fixture_User".into()),
                    ProfileTab::Timeline
                )
            );
        }
        for route in [
            "id:42",
            "id:42/timeline",
            "i/user/42",
            "intent/user?user_id=42",
            "intent/user/?s=20&user_id=42",
        ] {
            assert_eq!(
                profile_input(&Url::parse(&format!("https://x.com/{route}")).unwrap()).unwrap(),
                (ProfileInput::Id("42".into()), ProfileTab::Timeline)
            );
        }
        for route in [
            "home",
            "HOME",
            "search",
            "search/media",
            "explore",
            "settings",
            "i",
            "notifications",
            "messages",
            "hashtag",
            "intent",
            "i/user/0",
            "i/user/042",
            "i/user/18446744073709551616",
            "i/user/42/extra",
            "intent/user",
            "intent/user?user_id=42&user_id=42",
            "intent/user?user_id=42&user_id=43",
            "intent/user?user_id=042",
            "intent/user?user_id=",
            "intent/user?USER_ID=42",
        ] {
            assert!(
                profile_input(&Url::parse(&format!("https://x.com/{route}")).unwrap()).is_err(),
                "{route}"
            );
        }
    }

    #[test]
    fn numeric_lookup_name_is_typed_bound_and_alias_consistent() {
        let user =
            |value: Value| serde_json::to_vec(&json!({"data":{"user":{"result":value}}})).unwrap();
        let good = json!({"__typename":"User","rest_id":"42","core":{"screen_name":"Fixture_User"},"legacy":{"screen_name":"fixture_USER"}});
        assert_eq!(
            parse_user_name(&user(good.clone()), "42").unwrap(),
            "Fixture_User"
        );
        assert!(parse_user_name(&user(good.clone()), "43").is_err());
        assert!(parse_user_name(&user(good), "042").is_err());
        for bad in [
            json!({"__typename":"UserUnavailable","rest_id":"42","core":{"screen_name":"PRIVATE"}}),
            json!({"__typename":"User","rest_id":"42"}),
            json!({"__typename":"User","rest_id":"42","core":{"screen_name":"a/b"}}),
            json!({"__typename":"User","rest_id":"42","core":{"screen_name":"valid"},"legacy":{"screen_name":"PRIVATE"}}),
            json!({"__typename":"User","rest_id":"42","core":{"screen_name":"valid"},"legacy":{"screen_name":null}}),
        ] {
            let message = parse_user_name(&user(bad), "42").unwrap_err().to_string();
            assert!(!message.contains("PRIVATE"));
        }
    }

    fn search(instructions: Value) -> Vec<u8> {
        serde_json::to_vec(&json!({"data":{"search_by_raw_query":{"search_timeline":{"timeline":{"instructions":instructions}}}}})).unwrap()
    }

    #[test]
    fn search_posts_keep_structured_order_and_allow_different_authors() {
        let mut other = tweet("12");
        other["legacy"]["user_id_str"] = "43".into();
        other["core"]["user_results"]["result"]["rest_id"] = "43".into();
        let bytes = search(json!([
            {"type":"TimelineAddEntries","entries":[entry(tweet("11")), {"content":{"entryType":"TimelineTimelineModule","items":[{"item":{"itemContent":item(other)}}]}}, cursor("next")]},
            {"type":"TimelineAddToModule","moduleItems":[{"item":{"itemContent":item(tweet("13"))}}]}
        ]));
        let parsed = parse_search_posts(&bytes).unwrap();
        assert_eq!(
            parsed
                .posts
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
            ["11", "12", "13"]
        );
        assert_eq!(parsed.cursor.as_deref(), Some("next"));
        assert!(!parsed.done);
        let empty =
            parse_search_posts(&search(json!([{"type":"TimelineAddEntries","entries":[]}])))
                .unwrap();
        assert!(empty.posts.is_empty());
        assert!(empty.done);
        let advancing = parse_search_posts(&search(
            json!([{"type":"TimelineAddEntries","entries":[cursor("next")]}]),
        ))
        .unwrap();
        assert!(advancing.posts.is_empty());
        assert!(!advancing.done);
        let terminated = parse_search_posts(&search(json!([{"type":"TimelineAddEntries","entries":[cursor("next")]},{"type":"TimelineTerminateTimeline","direction":"Bottom"}]))).unwrap();
        assert!(terminated.done);
    }

    #[test]
    fn search_posts_validate_only_owned_bounded_post_results() {
        let mut wrapped = tweet("11");
        for _ in 0..4 {
            wrapped = json!({"__typename":"TweetWithVisibilityResults","tweet":wrapped});
        }
        assert_eq!(
            parse_search_posts(&search(
                json!([{"type":"TimelineAddEntries","entries":[entry(wrapped.clone())]}])
            ))
            .unwrap()
            .posts
            .len(),
            1
        );
        let mut conflict = tweet("12");
        conflict["legacy"]["id_str"] = "13".into();
        let mut owner_conflict = tweet("12");
        owner_conflict["legacy"]["user_id_str"] = "PRIVATE".into();
        let mut missing_owner = tweet("12");
        missing_owner["legacy"]
            .as_object_mut()
            .unwrap()
            .remove("user_id_str");
        missing_owner["core"]["user_results"]["result"]
            .as_object_mut()
            .unwrap()
            .remove("rest_id");
        for bad in [
            conflict,
            owner_conflict,
            missing_owner,
            json!({"__typename":"TweetWithVisibilityResults","tweet":wrapped}),
            json!({"__typename":"TweetUnavailable","reason":"PRIVATE","nested":tweet("11")}),
        ] {
            let message = parse_search_posts(&search(
                json!([{"type":"TimelineAddEntries","entries":[entry(bad)]}]),
            ))
            .err()
            .unwrap()
            .to_string();
            assert!(!message.contains("PRIVATE"));
        }
        let mut recommended = entry(tweet("12"));
        recommended["content"]["itemContent"] =
            json!({"itemType":"TimelineUser","nested":tweet("99")});
        let parsed = parse_search_posts(&search(
            json!([{"type":"TimelineAddEntries","entries":[recommended]}]),
        ))
        .unwrap();
        assert!(parsed.posts.is_empty());
    }

    #[test]
    fn only_explicit_boolean_bottom_cursor_setting_keeps_empty_pages_going() {
        for (fields, expected) in [
            (json!({}), false),
            (json!({"stopOnEmptyResponse":true}), false),
            (json!({"stopOnEmptyResponse":false}), true),
            (json!({"itemContent":{"stopOnEmptyResponse":false}}), true),
            (
                json!({"stopOnEmptyResponse":false,"itemContent":{"stopOnEmptyResponse":false}}),
                true,
            ),
        ] {
            let mut bottom = cursor("next");
            bottom["content"]
                .as_object_mut()
                .unwrap()
                .extend(fields.as_object().unwrap().clone());
            let parsed = parse_search_posts(&search(
                json!([{"type":"TimelineAddEntries","entries":[bottom]}]),
            ))
            .unwrap();
            assert_eq!(parsed.continue_empty, expected);
            assert!(!parsed.done);
            assert!(parsed.posts.is_empty());
        }
        for fields in [
            json!({"stopOnEmptyResponse":null}),
            json!({"stopOnEmptyResponse":"PRIVATE"}),
            json!({"stopOnEmptyResponse":0}),
            json!({"itemContent":{"stopOnEmptyResponse":[]}}),
            json!({"stopOnEmptyResponse":false,"itemContent":{"stopOnEmptyResponse":true}}),
        ] {
            let mut bottom = cursor("next");
            bottom["content"]
                .as_object_mut()
                .unwrap()
                .extend(fields.as_object().unwrap().clone());
            let message = parse_search_posts(&search(
                json!([{"type":"TimelineAddEntries","entries":[bottom]}]),
            ))
            .err()
            .unwrap()
            .to_string();
            assert!(!message.contains("PRIVATE"));
        }
        for kind in ["Top", "ShowMore", "ShowMoreThreads"] {
            let parsed = parse_search_posts(&search(json!([{"type":"TimelineAddEntries","entries":[{"content":{"entryType":"TimelineTimelineCursor","cursorType":kind,"value":"ignored","stopOnEmptyResponse":false}}]}]))).unwrap();
            assert!(!parsed.continue_empty);
            assert!(parsed.done);
        }
    }

    #[test]
    fn search_envelopes_unknown_instructions_and_private_errors_fail_closed() {
        for bytes in [
            entries(json!([entry(tweet("11"))])),
            search(json!([{"type":"PRIVATE","nested":entry(tweet("11"))}])),
            search(
                json!([{"type":"TimelineAddEntries","entries":[{"content":{"entryType":"PRIVATE","nested":tweet("11")}}]}]),
            ),
            search(json!([{"type":"TimelineAddEntries","entries":[cursor("a"),cursor("b")]}])),
            search(json!([])),
            br#"{"errors":[{"code":89,"message":"PRIVATE"}]}"#.to_vec(),
            vec![b' '; MAX_RESPONSE + 1],
        ] {
            let message = parse_search_posts(&bytes).err().unwrap().to_string();
            assert!(!message.contains("PRIVATE"));
        }
    }

    #[test]
    fn pins_are_skipped_without_parsing_when_disabled_and_validate_when_enabled() {
        let bytes = page(json!([{"type":"TimelinePinEntry","entry":{"private":"secret"}}]));
        let parsed = parse_profile_posts(&bytes, "42", ProfileTab::Tweets, false).unwrap();
        assert!(parsed.posts.is_empty());
        assert!(parsed.done);
        let message = parse_profile_posts(&bytes, "42", ProfileTab::Tweets, true)
            .err()
            .unwrap()
            .to_string();
        assert!(!message.contains("secret"));
        let bytes = page(json!([
            {"type":"TimelineAddEntries","entries":[entry(tweet("11"))]},
            {"type":"TimelinePinEntry","entry":entry(tweet("10"))}
        ]));
        let parsed = parse_profile_posts(&bytes, "42", ProfileTab::Tweets, true).unwrap();
        assert_eq!(
            parsed
                .posts
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
            ["10", "11"]
        );
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
