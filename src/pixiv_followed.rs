//! New illustrations from followed artists retain the login user's archive scope.
use super::Pixiv;
use crate::{
    Result, error,
    event::{ExtractionEvent, Post},
};
use futures_util::{
    StreamExt,
    stream::{self, BoxStream},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeSet, VecDeque},
    sync::Arc,
};
use url::Url;

const MAX_TEXT: usize = 64 * 1024;
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct FollowedUserMetadata {
    pub id: String,
    pub name: Option<Arc<str>>,
    pub account: Option<Arc<str>>,
}
impl FollowedUserMetadata {
    pub fn valid(&self) -> bool {
        super::valid_id(&self.id)
            && self.name.as_ref().is_none_or(|name| name.len() <= MAX_TEXT)
            && self
                .account
                .as_ref()
                .is_none_or(|account| account.len() <= MAX_TEXT)
    }
    pub fn parse_compat(value: &Value) -> Option<Self> {
        let id = super::id_of(value)?;
        let text = |key| {
            value
                .get(key)
                .and_then(Value::as_str)
                .filter(|text| text.len() <= MAX_TEXT)
                .map(Arc::from)
        };
        let user = Self {
            id,
            name: text("name"),
            account: text("account"),
        };
        user.valid().then_some(user)
    }
}

struct State {
    budget: super::PostBudget,
    user: Option<Arc<FollowedUserMetadata>>,
    next: Option<String>,
    pages: usize,
    seen: BTreeSet<String>,
    works: VecDeque<Value>,
    pending: VecDeque<ExtractionEvent>,
    done: bool,
}
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64() != Some(0.),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

impl Pixiv {
    async fn followed_user(&self) -> Result<Arc<FollowedUserMetadata>> {
        tokio::time::timeout(self.timeout, async {
            // All token refreshes take this lock. Snapshot its associated public
            // profile before releasing it, then retain that immutable Arc.
            let mut cached = self.access_token.lock().await;
            if cached.is_none() {
                *cached = Some(self.authenticate().await?);
            }
            self.authenticated_profile
                .lock()
                .await
                .clone()
                .ok_or_else(|| error("Pixiv OAuth response contains no valid followed user ID"))
        })
        .await
        .map_err(|_| error("native Pixiv extraction timed out"))?
    }
    fn followed_first(&self) -> Url {
        let mut endpoint = self
            .api_root
            .join("v2/illust/follow")
            .expect("static followed endpoint");
        endpoint.query_pairs_mut().append_pair("restrict", "all");
        endpoint
    }
    fn followed_next(&self, raw: &str) -> Result<Url> {
        let invalid = || error("Pixiv returned an unexpected followed feed next page");
        let endpoint = Url::parse(raw).map_err(|_| invalid())?;
        let expected = self.followed_first();
        let query: Vec<_> = endpoint.query_pairs().collect();
        let mut scope = query.iter().filter(|(key, _)| key == "restrict");
        let valid = scope.next().is_some_and(|(_, value)| value == "all") && scope.next().is_none();
        if endpoint.origin() != self.api_root.origin()
            || endpoint.path() != expected.path()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.fragment().is_some()
            || !valid
        {
            return Err(invalid());
        }
        Ok(endpoint)
    }
    async fn followed_page(&self, state: &mut State) -> Result<()> {
        if state.pages >= super::MAX_PAGES {
            state.done = true;
            return Ok(());
        }
        let Some(raw) = state.next.take() else {
            state.done = true;
            return Ok(());
        };
        let endpoint = self.followed_next(&raw)?;
        let mut canonical = endpoint.clone();
        let mut query: Vec<_> = endpoint
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        query.sort();
        canonical.set_query(None);
        canonical.query_pairs_mut().extend_pairs(query);
        if !state.seen.insert(canonical.to_string()) {
            return Err(error("Pixiv followed feed pagination repeated a page"));
        }
        let mut body = super::json(&self.get(&endpoint).await?)?;
        let works = body
            .get_mut("illusts")
            .and_then(Value::as_array_mut)
            .ok_or_else(|| error("Pixiv response contains no followed illustration list"))?;
        state.works = std::mem::take(works).into();
        state.next = super::string(&body, "next_url").map(str::to_owned);
        state.pages += 1;
        Ok(())
    }
    pub(super) fn followed_events(&self) -> BoxStream<'_, Result<ExtractionEvent>> {
        if self.refresh_token.is_none() {
            return stream::once(async { Err(super::missing_token()) }).boxed();
        }
        let state = State {
            budget: super::PostBudget::new(self.max_posts),
            user: None,
            next: Some(self.followed_first().to_string()),
            pages: 0,
            seen: BTreeSet::new(),
            works: VecDeque::new(),
            pending: VecDeque::new(),
            done: false,
        };
        stream::unfold(state, move |mut state| async move {
            let mut skipped = 0u8;
            loop {
                if let Some(event) = state.pending.pop_front() {
                    return Some((Ok(event), state));
                }
                if state.budget.exhausted() {
                    return None;
                }
                if state.done {
                    return None;
                }
                if state.user.is_none() {
                    match self.followed_user().await {
                        Ok(user) => state.user = Some(user),
                        Err(error) => {
                            state.done = true;
                            return Some((Err(error), state));
                        }
                    }
                }
                if let Some(work) = state.works.pop_front() {
                    state.budget.consume();
                    skipped = skipped.wrapping_add(1);
                    if skipped == 0 {
                        tokio::task::yield_now().await;
                    }
                    if !work
                        .get("user")
                        .and_then(|user| user.get("id"))
                        .is_some_and(truthy)
                    {
                        continue;
                    }
                    let user = state
                        .user
                        .as_ref()
                        .expect("followed owner snapshot")
                        .clone();
                    let (mut media, failure) = match self.work(&work, true).await {
                        Ok(media) => (media, None),
                        Err(error) => (Vec::new(), Some(error)),
                    };
                    if media.is_empty() {
                        let Some(id) = super::id_of(&work) else {
                            return Some((
                                Err(error("Pixiv followed illustration contains an invalid ID")),
                                state,
                            ));
                        };
                        let mut metadata = super::illustration_metadata(&work, &id);
                        metadata.user_follow = Some(user);
                        metadata.followed_archive = true;
                        state.pending = VecDeque::from([
                            ExtractionEvent::Post(Box::new(Post {
                                category: "pixiv".into(),
                                metadata: Some(metadata),
                                count: Some(0),
                            })),
                            ExtractionEvent::EndPost,
                        ]);
                        if let Some(error) = failure {
                            return Some((Err(error), state));
                        }
                        continue;
                    }
                    for item in &mut media {
                        let metadata = item.metadata.as_mut().expect("Pixiv illustration metadata");
                        metadata.user_follow = Some(user.clone());
                        metadata.followed_archive = true;
                    }
                    state.pending = self.finish(media);
                    continue;
                }
                if let Err(error) = self.followed_page(&mut state).await {
                    state.done = true;
                    return Some((Err(error), state));
                }
            }
        })
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        event::ExtractionEvent,
        job_selection::JobSelection,
        pixiv::tests::{pixiv, response, serve, serve_with},
        post_selection::PostSelection,
        selection::RangeSelection,
    };
    use serde_json::json;
    fn login(token: &str, id: Value) -> Vec<u8> {
        response("200 OK",json!({"response":{"access_token":token,"user":{"id":id,"name":"First user","account":"first-account","email":"PRIVATE_EMAIL","access_token":"PRIVATE_USER_TOKEN"}}}).to_string().as_bytes())
    }
    fn work(id: u64, kind: &str, artist: &str) -> Value {
        json!({"id":id,"type":kind,"title":"Title","user":{"id":5,"account":artist},"create_date":"2024-01-01T09:00:00+09:00","meta_single_page":{"original_image_url":format!("https://i.pximg.net/img-original/{id}_p0.jpg")},"meta_pages":[]})
    }
    fn page(works: Vec<Value>, next: Option<Url>) -> Vec<u8> {
        response(
            "200 OK",
            json!({"illusts":works,"next_url":next})
                .to_string()
                .as_bytes(),
        )
    }
    fn url() -> Url {
        Url::parse("https://www.pixiv.net/bookmark_new_illust.php?rest=hide&type=user&page=100")
            .unwrap()
    }

    #[test]
    fn followed_route_ignores_query_and_public_owner_metadata_excludes_private_fields() {
        assert_eq!(
            super::super::target(&url()).unwrap(),
            super::super::Target::Followed
        );
        assert!(
            super::super::target(
                &Url::parse("https://www.pixiv.net/en/bookmark_new_illust.php").unwrap()
            )
            .is_err()
        );
        let owner = FollowedUserMetadata::parse_compat(
            &json!({"id":17,"name":"Name","account":"account","email":"SECRET","token":"SECRET"}),
        )
        .unwrap();
        assert!(owner.valid());
        assert_eq!(
            serde_json::to_value(owner).unwrap(),
            json!({"id":"17","name":"Name","account":"account"})
        );
        for value in [
            json!({}),
            json!({"id":0}),
            json!({"id":"invalid"}),
            json!({"id":{}}),
        ] {
            assert!(FollowedUserMetadata::parse_compat(&value).is_none());
        }
        let owner = FollowedUserMetadata::parse_compat(
            &json!({"id":17,"name":"x".repeat(MAX_TEXT+1),"account":{}}),
        )
        .unwrap();
        assert_eq!(owner.name, None);
        assert_eq!(owner.account, None);
    }

    #[tokio::test]
    async fn followed_snapshot_is_immutable_across_token_refresh_and_mixed_artists() {
        let (root, server) = serve_with(|root| {
            vec![
                login("first-access", json!(17)),
                page(
                    vec![
                        work(41, "illust", "artist-one"),
                        work(42, "illust", "artist-two"),
                    ],
                    Some(
                        root.join("v2/illust/follow?restrict=all&offset=30")
                            .unwrap(),
                    ),
                ),
                response(
                    "400 Bad Request",
                    br#"{"error":{"message":"invalid_grant"}}"#,
                ),
                login("second-access", json!(99)),
                page(vec![work(43, "illust", "artist-three")], None),
            ]
        });
        let native = pixiv(root);
        let events = native
            .events(&url())
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        let mut files = Vec::new();
        let mut posts = 0;
        for event in events {
            match event {
                ExtractionEvent::Post(post) => {
                    posts += 1;
                    let metadata = post.metadata.unwrap();
                    assert!(metadata.followed_archive);
                    assert_eq!(metadata.user_follow.as_ref().unwrap().id, "17");
                }
                ExtractionEvent::Media(media) => {
                    let metadata = media.metadata.as_ref().unwrap();
                    assert!(metadata.followed_archive);
                    let owner = metadata.user_follow.as_ref().unwrap();
                    assert_eq!(owner.id, "17");
                    assert_eq!(owner.name.as_deref(), Some("First user"));
                    assert_eq!(owner.account.as_deref(), Some("first-account"));
                    assert!(media.archive_key().is_some());
                    assert!(!serde_json::to_string(&media).unwrap().contains("PRIVATE"));
                    assert_eq!(
                        media.headers,
                        std::collections::BTreeMap::from([(
                            "Referer".into(),
                            super::super::REFERER.into()
                        )])
                    );
                    files.push(media);
                }
                _ => {}
            }
        }
        assert_eq!(posts, 3);
        assert_eq!(
            files
                .iter()
                .map(|file| file.metadata.as_ref().unwrap().username.as_deref().unwrap())
                .collect::<Vec<_>>(),
            ["artist-one", "artist-two", "artist-three"]
        );
        assert_eq!(
            native
                .authenticated_profile
                .lock()
                .await
                .as_ref()
                .unwrap()
                .id,
            "99"
        );
        let ordinary = super::super::parse_illust(&work(41, "illust", "artist-one"))
            .unwrap()
            .remove(0);
        assert_ne!(files[0].archive_key(), ordinary.archive_key());
        let mut changed = files[0].clone();
        let mut owner = (**changed
            .metadata
            .as_ref()
            .unwrap()
            .user_follow
            .as_ref()
            .unwrap())
        .clone();
        owner.name = Some(Arc::from("Renamed"));
        owner.account = Some(Arc::from("changed"));
        changed.metadata.as_mut().unwrap().user_follow = Some(Arc::new(owner.clone()));
        assert_eq!(files[0].archive_key(), changed.archive_key());
        owner.id = "99".into();
        changed.metadata.as_mut().unwrap().user_follow = Some(Arc::new(owner));
        assert_ne!(files[0].archive_key(), changed.archive_key());
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 5);
        assert!(requests[1].starts_with("GET /v2/illust/follow?restrict=all "));
        assert!(!requests[1].contains("user_id="));
        assert!(
            requests[2]
                .to_ascii_lowercase()
                .contains("authorization: bearer first-access")
        );
        assert!(
            requests[4]
                .to_ascii_lowercase()
                .contains("authorization: bearer second-access")
        );
        assert!(requests[4].starts_with("GET /v2/illust/follow?restrict=all&offset=30 "));
    }

    #[tokio::test]
    async fn recorded_upstream_followed_oracle_matches_http_and_frozen_public_owner() {
        let oracle: Value =
            serde_json::from_slice(include_bytes!("../tests/fixtures/pixiv/followed.json"))
                .unwrap();
        let (root, server) = serve_with(|root| {
            let mut replies = vec![response(
                "200 OK",
                json!({"response":{"access_token":"ORACLE_TOKEN","user":oracle["owner"]}})
                    .to_string()
                    .as_bytes(),
            )];
            for original in oracle["responses"].as_array().unwrap() {
                let works = original["illusts"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|original| {
                        let mut enriched = work(
                            original["id"].as_u64().unwrap(),
                            "illust",
                            original["user"]["account"].as_str().unwrap(),
                        );
                        enriched["user"] = original["user"].clone();
                        enriched
                    })
                    .collect();
                let next = original["next_url"].as_str().map(|raw| {
                    let source = Url::parse(raw).unwrap();
                    let mut next = root.join(source.path().trim_start_matches('/')).unwrap();
                    next.set_query(source.query());
                    next
                });
                replies.push(page(works, next));
            }
            replies
        });
        let native = pixiv(root);
        let url = Url::parse(oracle["url"].as_str().unwrap()).unwrap();
        let mut stream = native.stream(&url);
        let first = stream.next().await.unwrap().unwrap();
        let cache = native.authenticated_profile.lock().await.clone().unwrap();
        assert!(!serde_json::to_string(&cache).unwrap().contains("PRIVATE"));
        {
            let _token = native.access_token.lock().await;
            let replacement =
                FollowedUserMetadata::parse_compat(&oracle["final_api_user"]).unwrap();
            *native.authenticated_user.lock().await = Some(replacement.id.clone());
            *native.authenticated_profile.lock().await = Some(Arc::new(replacement));
        }
        let mut files = vec![first];
        files.extend(
            stream
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .collect::<Result<Vec<_>>>()
                .unwrap(),
        );
        assert_eq!(files.len(), oracle["works"].as_array().unwrap().len());
        for (file, original) in files.iter().zip(oracle["works"].as_array().unwrap()) {
            let metadata = file.metadata.as_ref().unwrap();
            assert_eq!(
                metadata.post_id,
                original["id"].as_u64().unwrap().to_string()
            );
            assert_eq!(
                metadata.username.as_deref(),
                original["user"]["account"].as_str()
            );
            let expected = FollowedUserMetadata::parse_compat(&original["user_follow"]).unwrap();
            assert_eq!(metadata.user_follow.as_deref(), Some(&expected));
            assert!(!serde_json::to_string(file).unwrap().contains("PRIVATE"));
            assert!(!format!("{file:?}").contains("ORACLE_TOKEN"));
        }
        let requests = server.join().unwrap();
        let calls = oracle["calls"].as_array().unwrap();
        assert_eq!(
            requests.len(),
            calls.len() + oracle["login_count"].as_u64().unwrap() as usize
        );
        for (request, call) in requests[1..].iter().zip(calls) {
            let actual = Url::parse(&format!(
                "http://localhost{}",
                request.split_whitespace().nth(1).unwrap()
            ))
            .unwrap();
            assert_eq!(actual.path(), call["endpoint"].as_str().unwrap());
            let actual: std::collections::BTreeMap<_, _> = actual
                .query_pairs()
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect();
            let expected: std::collections::BTreeMap<_, _> = call["params"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(key, value)| (key.clone(), value.as_str().unwrap().to_owned()))
                .collect();
            assert_eq!(actual, expected);
        }
    }

    #[tokio::test]
    async fn missing_or_invalid_login_owner_fails_privately_before_feed_http() {
        for owner in [Value::Null, json!(0), json!("bad-id"), json!({})] {
            let (root, server) = serve(vec![login("PRIVATE_ACCESS", owner)]);
            let results = pixiv(root).stream(&url()).collect::<Vec<_>>().await;
            assert_eq!(results.len(), 1);
            assert_eq!(
                results[0].as_ref().unwrap_err().to_string(),
                "Pixiv OAuth response contains no valid followed user ID"
            );
            assert_eq!(server.join().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn unpolled_feed_and_consumer_drop_keep_auth_work_metadata_and_pages_lazy() {
        let (root, server) = serve_with(|root| {
            vec![
                login("t", json!(17)),
                page(
                    vec![work(41, "illust", "one"), work(42, "ugoira", "two")],
                    Some(
                        root.join("v2/illust/follow?restrict=all&offset=30")
                            .unwrap(),
                    ),
                ),
            ]
        });
        let native = pixiv(root);
        let unpolled = native.events(&url());
        drop(unpolled);
        let mut files = native.stream(&url());
        assert_eq!(files.next().await.unwrap().unwrap().filename, "41_p0.jpg");
        drop(files);
        assert_eq!(server.join().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn empty_or_failed_animations_keep_owner_metadata_and_post_range_positions() {
        for failed in [false, true] {
            for range in ["1", "2"] {
                let mut replies = vec![
                    login("t", json!(17)),
                    page(
                        vec![work(41, "ugoira", "one"), work(42, "illust", "two")],
                        None,
                    ),
                ];
                if failed {
                    replies.push(response("503 Service Unavailable", b"PRIVATE"));
                }
                let (root, server) = serve(replies);
                let native = pixiv(root).with_ugoira(failed);
                let results = JobSelection {
                    post_selection: Some(PostSelection::new(
                        Some(RangeSelection::parse(range).unwrap()),
                        None,
                    )),
                    ..Default::default()
                }
                .apply(native.events(&url()))
                .collect::<Vec<_>>()
                .await;
                assert_eq!(
                    results.iter().filter(|result| result.is_err()).count(),
                    usize::from(failed)
                );
                let files = results
                    .iter()
                    .filter_map(|result| result.as_ref().ok())
                    .collect::<Vec<_>>();
                assert_eq!(files.len(), usize::from(range == "2"));
                if let Some(file) = files.first() {
                    assert_eq!(file.filename, "42_p0.jpg");
                    assert_eq!(
                        file.metadata
                            .as_ref()
                            .unwrap()
                            .user_follow
                            .as_ref()
                            .unwrap()
                            .id,
                        "17"
                    );
                }
                assert_eq!(server.join().unwrap().len(), if failed { 3 } else { 2 });
            }
        }
        let (root, server) = serve(vec![
            login("t", json!(17)),
            page(vec![work(41, "ugoira", "one")], None),
        ]);
        let events = pixiv(root)
            .with_ugoira(false)
            .events(&url())
            .collect::<Vec<_>>()
            .await;
        let Ok(ExtractionEvent::Post(post)) = &events[0] else {
            panic!("missing empty followed post")
        };
        assert_eq!(post.count, Some(0));
        assert_eq!(
            post.metadata
                .as_ref()
                .unwrap()
                .user_follow
                .as_ref()
                .unwrap()
                .id,
            "17"
        );
        assert!(matches!(events[1], Ok(ExtractionEvent::EndPost)));
        assert_eq!(server.join().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn enabled_ugoira_inherits_owner_snapshot_and_media_transport_has_no_api_auth() {
        let(root,server)=serve(vec![login("ACCESS_SECRET",json!(17)),page(vec![work(41,"ugoira","artist")],None),response("200 OK",json!({"ugoira_metadata":{"zip_urls":{"medium":"https://i.pximg.net/img-zip-ugoira/41_ugoira600x600.zip"},"frames":[{"file":"000000.jpg","delay":80}]}}).to_string().as_bytes())]);
        let files = pixiv(root).stream(&url()).collect::<Vec<_>>().await;
        assert_eq!(files.len(), 1);
        let media = files[0].as_ref().unwrap();
        assert_eq!(media.filename, "41_p0.zip");
        assert_eq!(
            media
                .metadata
                .as_ref()
                .unwrap()
                .user_follow
                .as_ref()
                .unwrap()
                .id,
            "17"
        );
        assert_eq!(
            media
                .metadata
                .as_ref()
                .unwrap()
                .frames
                .as_ref()
                .unwrap()
                .len(),
            1
        );
        assert!(!format!("{media:?}").contains("ACCESS_SECRET"));
        assert_eq!(server.join().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn post_filters_and_file_ranges_keep_original_artists_and_leave_later_ugoira_unrequested()
    {
        use crate::filter::FilterExpression;
        let mut missing = work(40, "illust", "missing");
        missing["user"]["id"] = json!(0);
        let (root, server) = serve(vec![
            login("t", json!(17)),
            page(
                vec![
                    missing,
                    work(41, "illust", "one"),
                    work(42, "illust", "two"),
                    work(43, "ugoira", "three"),
                ],
                None,
            ),
        ]);
        let native = pixiv(root);
        let files = JobSelection {
            post_selection: Some(PostSelection::new(
                None,
                Some(FilterExpression::parse("username == 'two'").unwrap()),
            )),
            file_range: Some(RangeSelection::parse("1").unwrap()),
            ..Default::default()
        }
        .apply(native.events(&url()))
        .collect::<Vec<_>>()
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].as_ref().unwrap().filename, "42_p0.jpg");
        assert_eq!(server.join().unwrap().len(), 2);
    }

    #[test]
    fn next_page_pins_exact_origin_path_and_single_all_restriction() {
        let native = Pixiv::new(std::time::Duration::from_secs(1)).unwrap();
        assert!(
            native
                .followed_next("https://app-api.pixiv.net/v2/illust/follow?restrict=all&offset=30")
                .is_ok()
        );
        for raw in [
            "https://evil.example/v2/illust/follow?restrict=all",
            "https://app-api.pixiv.net/v1/user/following?restrict=all",
            "https://u:p@app-api.pixiv.net/v2/illust/follow?restrict=all",
            "https://app-api.pixiv.net/v2/illust/follow?restrict=public",
            "https://app-api.pixiv.net/v2/illust/follow?restrict=all&restrict=all",
            "https://app-api.pixiv.net/v2/illust/follow?restrict=all#fragment",
        ] {
            assert!(native.followed_next(raw).is_err(), "{raw}");
        }
    }

    #[tokio::test]
    async fn canonical_cycles_page_bounds_and_empty_page_recovery_are_safe() {
        let (root, server) = serve_with(|root| {
            vec![
                login("t", json!(17)),
                page(
                    Vec::new(),
                    Some(root.join("v2/illust/follow?restrict=all").unwrap()),
                ),
            ]
        });
        let results = pixiv(root).stream(&url()).collect::<Vec<_>>().await;
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].as_ref().unwrap_err().to_string(),
            "Pixiv followed feed pagination repeated a page"
        );
        assert_eq!(server.join().unwrap().len(), 2);
        let (root, server) = serve_with(|root| {
            vec![
                login("t", json!(17)),
                page(
                    Vec::new(),
                    Some(
                        root.join("v2/illust/follow?restrict=all&offset=30")
                            .unwrap(),
                    ),
                ),
                page(vec![work(41, "illust", "artist")], None),
            ]
        });
        let results = pixiv(root).stream(&url()).collect::<Vec<_>>().await;
        assert_eq!(results.len(), 1);
        assert!(results[0].is_ok());
        assert_eq!(server.join().unwrap().len(), 3);
        let (root, server) = serve_with(|root| {
            let mut replies = vec![login("t", json!(17))];
            replies.extend((1..=super::super::MAX_PAGES).map(|offset| {
                page(
                    Vec::new(),
                    Some(
                        root.join(&format!("v2/illust/follow?restrict=all&offset={offset}"))
                            .unwrap(),
                    ),
                )
            }));
            replies
        });
        let results = pixiv(root).stream(&url()).collect::<Vec<_>>().await;
        assert!(results.is_empty(), "{results:?}");
        assert_eq!(server.join().unwrap().len(), super::super::MAX_PAGES + 1);
    }
    #[tokio::test]
    async fn max_posts_counts_missing_user_before_common_processing_without_fetching_next_page() {
        let mut missing = work(41, "illust", "missing");
        missing["user"]["id"] = json!(0);
        let (root, server) = serve(vec![
            login("t", json!(17)),
            page(
                vec![missing, work(42, "ugoira", "artist")],
                Some(Url::parse("https://unexpected.invalid/page").unwrap()),
            ),
        ]);
        let events = pixiv(root)
            .with_max_posts(Some(1))
            .events(&url())
            .collect::<Vec<_>>()
            .await;
        assert!(events.is_empty());
        assert_eq!(server.join().unwrap().len(), 2);
    }
}
