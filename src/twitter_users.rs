//! Queue-only X routes: quote searches and paginated user lists (following,
//! followers, list members). GraphQL operation IDs are upstream snapshots.
use super::*;
use crate::event::{self, ChildMetadata, ExtractionEvent};
use sha2::{Digest, Sha256};
use std::collections::{HashSet, VecDeque};
use timeline::ProfileInput;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Target {
    /// Upstream TwitterQuotesExtractor queues `quoted_tweet_id:ID` search.
    Quotes(String),
    Following(ProfileInput),
    Followers(ProfileInput),
    ListMembers(String),
}

impl Target {
    fn subcategory(&self) -> &'static str {
        match self {
            Self::Quotes(_) => "quotes",
            Self::Following(_) => "following",
            Self::Followers(_) => "followers",
            Self::ListMembers(_) => "list-members",
        }
    }
}

/// Routes whose results are queued child jobs rather than direct posts.
pub fn is_queue_url(url: &Url) -> bool {
    matches!(input(url), Ok(Some(_)))
}

pub(super) fn input(url: &Url) -> Result<Option<Target>> {
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
            "native Twitter user lists require a supported first-party URL",
        ));
    }
    let path = url.path().strip_suffix('/').unwrap_or(url.path());
    let parts: Vec<_> = path.split('/').collect();
    Ok(match parts.as_slice() {
        ["", "i", "web", "status", id, "quotes"] | ["", _, "status", id, "quotes"]
            if !parts[1].is_empty() =>
        {
            if !valid_id(id) {
                return Err(error("invalid Twitter post identifier"));
            }
            Some(Target::Quotes((*id).into()))
        }
        ["", "i", "lists", id, "members"] => {
            if !valid_id(id) {
                return Err(error("Twitter list identifier missing or invalid"));
            }
            Some(Target::ListMembers((*id).into()))
        }
        ["", user, kind @ ("following" | "followers")] => {
            let Some(user) = timeline::user_input(user) else {
                if user.starts_with("id:") {
                    return Err(error("Twitter user identifier missing or invalid"));
                }
                return Ok(None);
            };
            Some(if *kind == "following" {
                Target::Following(user)
            } else {
                Target::Followers(user)
            })
        }
        _ => None,
    })
}

#[derive(Debug)]
pub(super) struct UserPage {
    pub(super) users: Vec<(String, serde_json::Map<String, Value>)>,
    pub(super) cursor: Option<String>,
    pub(super) done: bool,
}

/// Parse one `_pagination_users` page bound to the requested owner or list.
pub(super) fn parse_users(bytes: &[u8], target: &Target, owner: &str) -> Result<UserPage> {
    if !valid_id(owner) {
        return Err(error("invalid Twitter user list owner"));
    }
    let data = timeline::document(bytes)?;
    let (envelope, path) = match target {
        Target::Following(_) | Target::Followers(_) => {
            ("/data/user/result", "/timeline/timeline/instructions")
        }
        Target::ListMembers(_) => ("/data/list", "/members_timeline/timeline/instructions"),
        Target::Quotes(_) => return Err(error("Twitter quotes are not a user list")),
    };
    let envelope = data
        .pointer(envelope)
        .filter(|value| value.is_object())
        .ok_or_else(|| error("Twitter user list timeline missing"))?;
    // Decimal identity aliases may be omitted, but must match when present.
    for field in ["rest_id", "id_str"] {
        if let Some(value) = envelope.get(field)
            && value.as_str() != Some(owner)
        {
            return Err(error(
                "Twitter user list does not match the requested owner",
            ));
        }
    }
    let instructions = envelope
        .pointer(path)
        .and_then(Value::as_array)
        .ok_or_else(|| error("Twitter user list instructions missing or invalid"))?;
    let mut page = UserPage {
        users: Vec::new(),
        cursor: None,
        done: false,
    };
    let mut entries_seen = false;
    for instruction in instructions {
        let kind = instruction
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| error("Twitter user list instruction type missing"))?;
        // Upstream ignores other instruction types (cache/alert/terminate).
        if kind != "TimelineAddEntries" {
            continue;
        }
        let entries = instruction
            .get("entries")
            .and_then(Value::as_array)
            .ok_or_else(|| error("Twitter user list entries missing or invalid"))?;
        for entry in entries {
            entries_seen = true;
            let id = entry
                .get("entryId")
                .and_then(Value::as_str)
                .ok_or_else(|| error("Twitter user list entry identifier missing"))?;
            if id.starts_with("user-") {
                // Unavailable users carry no result or rest_id; upstream skips them.
                let Some(user) = entry
                    .pointer("/content/itemContent/user_results/result")
                    .filter(|user| !user.is_null())
                else {
                    continue;
                };
                let user = user
                    .as_object()
                    .ok_or_else(|| error("Twitter user list result malformed"))?;
                let Some(rest_id) = user.get("rest_id") else {
                    continue;
                };
                let rest_id = rest_id
                    .as_str()
                    .filter(|id| valid_id(id))
                    .ok_or_else(|| error("Twitter user list identifier invalid"))?;
                page.users.push((rest_id.into(), user.clone()));
            } else if id.starts_with("cursor-bottom-") {
                let value = entry
                    .pointer("/content/value")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty() && value.len() <= 8192)
                    .ok_or_else(|| error("Twitter user list Bottom cursor missing or invalid"))?;
                if page.cursor.as_deref().is_some_and(|old| old != value) {
                    return Err(error("Twitter user list has conflicting Bottom cursors"));
                }
                page.cursor = Some(value.into());
            }
        }
    }
    page.done = !entries_seen
        || page
            .cursor
            .as_deref()
            .is_none_or(|cursor| cursor.starts_with("-1|") || cursor.starts_with("0|"));
    if page.done {
        page.cursor = None;
    }
    Ok(page)
}

pub(super) fn users_endpoint(
    base: &Url,
    target: &Target,
    owner: &str,
    cursor: Option<&str>,
) -> Result<Url> {
    if !valid_id(owner) {
        return Err(error("invalid Twitter user list owner"));
    }
    let (operation, mut variables) = match target {
        Target::Following(_) | Target::Followers(_) => (
            if matches!(target, Target::Following(_)) {
                "SaWqzw0TFAWMx1nXWjXoaQ/Following"
            } else {
                "i6PPdIMm1MO7CpAqjau7sw/Followers"
            },
            serde_json::json!({"userId":owner,"count":100,"includePromotedContent":false,
                "withGrokTranslatedBio":false}),
        ),
        Target::ListMembers(_) => (
            "naea_MSad4pOb-D6_oVv_g/ListMembers",
            serde_json::json!({"listId":owner,"count":100}),
        ),
        Target::Quotes(_) => return Err(error("Twitter quotes are not a user list")),
    };
    if let Some(cursor) = cursor {
        variables["cursor"] = cursor.into();
    }
    // Upstream features_pagination equals the timeline feature set.
    let mut features = tweet_features();
    features["rweb_video_screen_enabled"] = false.into();
    profile::query(base, operation, variables, features, Value::Null)
}

struct State {
    target: Target,
    base: Url,
    owner: Option<String>,
    cursor: Option<String>,
    cursors: HashSet<[u8; 32]>,
    users: VecDeque<(String, serde_json::Map<String, Value>)>,
    done: bool,
    budget: Duration,
}

impl Twitter {
    pub(super) fn queue_events(
        &self,
        target: Target,
        base: Url,
    ) -> BoxStream<'_, Result<ExtractionEvent>> {
        if let Target::Quotes(id) = target {
            let query = format!("quoted_tweet_id:{id}");
            return event::child_job(
                ChildMetadata::from_raw("twitter", "quotes", &serde_json::Map::new()),
                move || self.search_events(query, base),
            );
        }
        let subcategory = target.subcategory();
        let owner = match &target {
            Target::Following(ProfileInput::Id(id))
            | Target::Followers(ProfileInput::Id(id))
            | Target::ListMembers(id) => Some(id.clone()),
            _ => None,
        };
        let state = State {
            target,
            base: base.clone(),
            owner,
            cursor: None,
            cursors: HashSet::new(),
            users: VecDeque::new(),
            done: false,
            budget: self.timeout,
        };
        stream::unfold(state, move |mut state| async move {
            loop {
                if let Some(user) = state.users.pop_front() {
                    return Some((Ok(user), state));
                }
                if state.done {
                    return None;
                }
                let started = tokio::time::Instant::now();
                let result = tokio::time::timeout(state.budget, self.users_page(&mut state)).await;
                state.budget = state.budget.saturating_sub(started.elapsed());
                if let Err(e) =
                    result.unwrap_or_else(|_| Err(error("native Twitter extraction timed out")))
                {
                    state.done = true;
                    state.users.clear();
                    return Some((Err(e), state));
                }
            }
        })
        .flat_map(move |result| match result {
            // Upstream `users` default: queue /i/user/{rest_id} to the user dispatcher.
            Ok((id, user)) => {
                let base = base.clone();
                event::child_job(
                    ChildMetadata::from_raw("twitter", subcategory, &user),
                    move || self.user_events(ProfileInput::Id(id), base),
                )
            }
            Err(e) => stream::once(async move { Err(e) }).boxed(),
        })
        .boxed()
    }

    async fn users_page(&self, state: &mut State) -> Result<()> {
        let Some(owner) = state.owner.clone() else {
            let (Target::Following(input) | Target::Followers(input)) = &state.target else {
                return Err(error("Twitter user list owner missing"));
            };
            let endpoint = profile::user_endpoint(&state.base, input)?;
            self.ensure_transaction_at("", &endpoint).await?;
            let body = self.fetch(&endpoint, "", true).await?;
            state.owner = Some(timeline::parse_user(&body, input)?);
            return Ok(());
        };
        let target = users_endpoint(&state.base, &state.target, &owner, state.cursor.as_deref())?;
        self.ensure_transaction_at("", &target).await?;
        let body = self.fetch(&target, "", true).await?;
        let page = parse_users(&body, &state.target, &owner)?;
        if let Some(cursor) = &page.cursor {
            let digest = <[u8; 32]>::from(Sha256::digest(cursor.as_bytes()));
            if !state.cursors.insert(digest) {
                return Err(error("Twitter user list cursor repeated"));
            }
        }
        state.done = page.done;
        state.cursor = page.cursor;
        state.users = page.users.into();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn url(path: &str) -> Url {
        Url::parse(&format!("https://x.com{path}")).unwrap()
    }
    fn user(id: &str) -> Value {
        json!({"entryId":format!("user-{id}"),"content":{"entryType":"TimelineTimelineItem",
            "itemContent":{"itemType":"TimelineUser","user_results":{"result":{
                "__typename":"User","id":"opaque","rest_id":id,"legacy":{"screen_name":"name"}}}}}})
    }
    fn cursor(kind: &str, value: &str) -> Value {
        json!({"entryId":format!("cursor-{kind}-1"),"content":{"entryType":"TimelineTimelineCursor","cursorType":"Bottom","value":value}})
    }
    fn following(instructions: Value) -> Vec<u8> {
        serde_json::to_vec(&json!({"data":{"user":{"result":{"__typename":"User",
            "timeline":{"timeline":{"instructions":instructions}}}}}}))
        .unwrap()
    }
    fn members(instructions: Value) -> Vec<u8> {
        serde_json::to_vec(
            &json!({"data":{"list":{"members_timeline":{"timeline":{"instructions":instructions}}}}}),
        )
        .unwrap()
    }
    fn ids(page: &UserPage) -> Vec<&str> {
        page.users.iter().map(|(id, _)| id.as_str()).collect()
    }

    #[test]
    fn queue_routes_are_recognized_without_changing_existing_routes() {
        for host in [
            "x.com",
            "www.x.com",
            "mobile.x.com",
            "twitter.com",
            "www.twitter.com",
            "mobile.twitter.com",
        ] {
            for (path, expected) in [
                ("fixture/status/123/quotes", Target::Quotes("123".into())),
                ("fixture/status/123/quotes/", Target::Quotes("123".into())),
                ("i/web/status/123/quotes", Target::Quotes("123".into())),
                (
                    "fixture/following",
                    Target::Following(ProfileInput::Name("fixture".into())),
                ),
                (
                    "id:42/following/",
                    Target::Following(ProfileInput::Id("42".into())),
                ),
                (
                    "fixture/followers",
                    Target::Followers(ProfileInput::Name("fixture".into())),
                ),
                ("i/lists/55/members", Target::ListMembers("55".into())),
            ] {
                let url = Url::parse(&format!("https://{host}/{path}?s=20#top")).unwrap();
                assert_eq!(input(&url).unwrap(), Some(expected), "{url}");
                assert!(is_queue_url(&url));
                // Quote lists are never single posts, and user lists are not profile tabs.
                assert!(post_id(&url).is_err());
                assert!(!timeline::is_profile_timeline_url(&url));
                assert!(!collections::is_collection_url(&url));
                assert!(!search::is_search_url(&url));
            }
        }
        for path in [
            "/fixture/status/123",
            "/i/web/status/123",
            "/fixture/status/123/photo/1",
            "/fixture/status/123/quotes/more",
            "/fixture/status/123/retweets",
            "/i/lists/55",
            "/i/lists/55/members/more",
            "/i/lists/55/followers",
            "/home/following",
            "/fixture",
            "/fixture/media",
            "/fixture/followers_you_follow",
            "/fixture/verified_followers",
            "/fixture/following/more",
            "/search/following",
            "/bad!name/following",
            "/search",
        ] {
            assert_eq!(input(&url(path)).unwrap(), None, "{path}");
            assert!(!is_queue_url(&url(path)));
        }
        assert!(post_id(&url("/fixture/status/123")).is_ok());
        assert!(collections::is_collection_url(&url("/home/following")));
        assert!(collections::is_collection_url(&url("/i/lists/55")));
        for url in [
            "https://x.com/fixture/status/0/quotes",
            "https://x.com/fixture/status/012/quotes",
            "https://x.com/fixture/status/PRIVATE/quotes",
            "https://x.com/i/lists/0/members",
            "https://x.com/i/lists/PRIVATE/members",
            "https://x.com/id:0/followers",
            "https://x.com/id:PRIVATE/following",
            "https://user:PRIVATE@x.com/fixture/following",
            "https://x.com:444/fixture/following",
            "https://x.com.evil/fixture/following",
        ] {
            let error = input(&Url::parse(url).unwrap()).unwrap_err().to_string();
            assert!(!error.contains("PRIVATE"), "{url}");
        }
    }

    #[test]
    fn user_pages_follow_upstream_entry_and_cursor_rules() {
        let target = Target::Following(ProfileInput::Id("42".into()));
        let mut unavailable = user("9");
        unavailable["content"]["itemContent"]["user_results"]["result"] =
            json!({"__typename":"UserUnavailable","reason":"Suspended"});
        let mut empty = user("10");
        empty["content"]["itemContent"]
            .as_object_mut()
            .unwrap()
            .remove("user_results");
        let page = parse_users(
            &following(json!([
                {"type":"TimelineClearCache"},
                {"type":"TimelineAddEntries","entries":[user("7"), unavailable, empty,
                    {"entryId":"who-to-follow-1","content":{}}, user("8"),
                    cursor("top", "top-cursor"), cursor("bottom", "next+/=")]}
            ])),
            &target,
            "42",
        )
        .unwrap();
        assert_eq!(ids(&page), ["7", "8"]);
        assert_eq!(page.users[0].1["legacy"]["screen_name"], "name");
        assert_eq!(page.cursor.as_deref(), Some("next+/="));
        assert!(!page.done);

        for terminal in ["0|1234", "-1|1234"] {
            let page = parse_users(
                &following(json!([{"type":"TimelineAddEntries","entries":[user("7"), cursor("bottom", terminal)]}])),
                &target,
                "42",
            )
            .unwrap();
            assert_eq!(ids(&page), ["7"]);
            assert!(page.done);
            assert!(page.cursor.is_none());
        }
        let no_cursor = parse_users(
            &following(json!([{"type":"TimelineAddEntries","entries":[user("7")]}])),
            &target,
            "42",
        )
        .unwrap();
        assert!(no_cursor.done);
        // No entries at all ends pagination, as upstream does.
        let no_entries = parse_users(
            &following(json!([{"type":"TimelineAddEntries","entries":[]},{"type":"TimelineTerminateTimeline"}])),
            &target,
            "42",
        )
        .unwrap();
        assert!(no_entries.done && no_entries.users.is_empty());

        let list = Target::ListMembers("55".into());
        let page = parse_users(
            &members(json!([{"type":"TimelineAddEntries","entries":[user("7"), cursor("bottom", "more")]}])),
            &list,
            "55",
        )
        .unwrap();
        assert_eq!(ids(&page), ["7"]);
        assert_eq!(page.cursor.as_deref(), Some("more"));
        // The user and list envelopes are not interchangeable.
        assert!(parse_users(&members(json!([])), &target, "42").is_err());
        assert!(parse_users(&following(json!([])), &list, "55").is_err());
        assert!(parse_users(&following(json!([])), &Target::Quotes("1".into()), "42").is_err());
    }

    #[test]
    fn user_pages_bind_identity_and_fail_closed_without_leaking_payloads() {
        let target = Target::Followers(ProfileInput::Id("42".into()));
        let mut bound = json!({"data":{"user":{"result":{"__typename":"User","rest_id":"42",
            "timeline":{"timeline":{"instructions":[{"type":"TimelineAddEntries","entries":[user("7")]}]}}}}}});
        assert!(parse_users(&serde_json::to_vec(&bound).unwrap(), &target, "42").is_ok());
        bound["data"]["user"]["result"]["rest_id"] = "PRIVATE".into();
        let mut list = json!({"data":{"list":{"id_str":"PRIVATE","members_timeline":{"timeline":{"instructions":[]}}}}});
        let mut bad_id = user("7");
        bad_id["content"]["itemContent"]["user_results"]["result"]["rest_id"] = "PRIVATE".into();
        let mut numeric_id = user("7");
        numeric_id["content"]["itemContent"]["user_results"]["result"]["rest_id"] = json!(7);
        let mut scalar = user("7");
        scalar["content"]["itemContent"]["user_results"]["result"] = "PRIVATE".into();
        let mut bad_cursor = cursor("bottom", "x");
        bad_cursor["content"]["value"] = json!(7);
        let cases = vec![
            serde_json::to_vec(&bound).unwrap(),
            following(json!([{"type":"TimelineAddEntries","entries":[bad_id]}])),
            following(json!([{"type":"TimelineAddEntries","entries":[numeric_id]}])),
            following(json!([{"type":"TimelineAddEntries","entries":[scalar]}])),
            following(json!([{"type":"TimelineAddEntries","entries":[bad_cursor]}])),
            following(
                json!([{"type":"TimelineAddEntries","entries":[cursor("bottom","a"),cursor("bottom","PRIVATE")]}]),
            ),
            following(json!([{"type":"TimelineAddEntries","entries":[{"content":"PRIVATE"}]}])),
            following(json!([{"type":"TimelineAddEntries","entries":"PRIVATE"}])),
            following(json!([{"entries":[]}])),
            following(json!("PRIVATE")),
            br#"{"data":{"user":{"result":{"__typename":"User"}}}}"#.to_vec(),
            br#"{"data":{}}"#.to_vec(),
            br#"{"errors":[{"code":89,"message":"PRIVATE"}]}"#.to_vec(),
            b"<html>PRIVATE</html>".to_vec(),
            vec![b' '; MAX_RESPONSE + 1],
        ];
        for bytes in cases {
            let error = parse_users(&bytes, &target, "42").unwrap_err().to_string();
            assert!(!error.contains("PRIVATE"), "{error}");
        }
        let error = parse_users(
            &serde_json::to_vec(&list).unwrap(),
            &Target::ListMembers("55".into()),
            "55",
        )
        .unwrap_err()
        .to_string();
        assert!(!error.contains("PRIVATE"));
        list["data"]["list"]["id_str"] = "55".into();
        assert!(
            parse_users(
                &serde_json::to_vec(&list).unwrap(),
                &Target::ListMembers("55".into()),
                "55"
            )
            .unwrap()
            .done
        );
        assert!(parse_users(&following(json!([])), &target, "042").is_err());
    }

    #[test]
    fn user_list_endpoints_use_upstream_operations_and_pagination_features() {
        let base = Url::parse("https://x.com/i/api/graphql/").unwrap();
        let variables = |url: &Url| -> Value {
            serde_json::from_str(&url.query_pairs().find(|(k, _)| k == "variables").unwrap().1)
                .unwrap()
        };
        for (target, operation) in [
            (
                Target::Following(ProfileInput::Name("fixture".into())),
                "/i/api/graphql/SaWqzw0TFAWMx1nXWjXoaQ/Following",
            ),
            (
                Target::Followers(ProfileInput::Id("42".into())),
                "/i/api/graphql/i6PPdIMm1MO7CpAqjau7sw/Followers",
            ),
        ] {
            let url = users_endpoint(&base, &target, "42", None).unwrap();
            assert_eq!(url.path(), operation);
            assert_eq!(
                variables(&url),
                json!({"userId":"42","count":100,"includePromotedContent":false,"withGrokTranslatedBio":false})
            );
            let url = users_endpoint(&base, &target, "42", Some("c+/=")).unwrap();
            assert_eq!(variables(&url)["cursor"], "c+/=");
            let features: Value =
                serde_json::from_str(&url.query_pairs().find(|(k, _)| k == "features").unwrap().1)
                    .unwrap();
            assert_eq!(features.as_object().unwrap().len(), 35);
            assert_eq!(features["rweb_video_screen_enabled"], false);
            assert_eq!(features["articles_preview_enabled"], true);
            assert!(!url.query_pairs().any(|(k, _)| k == "fieldToggles"));
        }
        let url = users_endpoint(&base, &Target::ListMembers("55".into()), "55", None).unwrap();
        assert_eq!(
            url.path(),
            "/i/api/graphql/naea_MSad4pOb-D6_oVv_g/ListMembers"
        );
        assert_eq!(variables(&url), json!({"listId":"55","count":100}));
        assert!(users_endpoint(&base, &Target::Quotes("1".into()), "1", None).is_err());
        assert!(users_endpoint(&base, &Target::ListMembers("55".into()), "0", None).is_err());
    }
}
