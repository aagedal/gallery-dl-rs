//! Following queues public preview metadata before validating the selected user.
//! Forced PixivUserExtractor children retain the normal profile include dispatch.
use super::{Pixiv, id_of};
use crate::{
    Result, error,
    event::{ChildMetadata, ExtractionEvent, child_job},
};
use futures_util::{
    StreamExt,
    stream::{self, BoxStream},
};
use serde_json::Value;
use std::{
    collections::{BTreeSet, VecDeque},
    sync::{Arc, Mutex},
};
use url::Url;

struct User {
    metadata: ChildMetadata,
    id: Option<Value>,
}

struct State {
    owner: Option<String>,
    private: bool,
    next: Option<String>,
    started: bool,
    pages: usize,
    seen_pages: BTreeSet<String>,
    users: VecDeque<Value>,
    done: bool,
}

impl Pixiv {
    fn following_first(&self, owner: &str, private: bool) -> Url {
        let mut endpoint = self
            .api_root
            .join("v1/user/following")
            .expect("static endpoint");
        endpoint
            .query_pairs_mut()
            .append_pair("user_id", owner)
            .append_pair("restrict", if private { "private" } else { "public" });
        endpoint
    }

    fn following_next(&self, raw: &str, owner: &str, private: bool) -> Result<Url> {
        let invalid = || error("Pixiv returned an unexpected following next page");
        let url = Url::parse(raw).map_err(|_| invalid())?;
        let expected = self.following_first(owner, private);
        let pairs: Vec<_> = url.query_pairs().collect();
        let scope = expected.query_pairs().all(|(key, value)| {
            let mut entries = pairs.iter().filter(|(actual, _)| *actual == key);
            entries.next().is_some_and(|(_, actual)| *actual == value) && entries.next().is_none()
        });
        if url.origin() != self.api_root.origin()
            || url.path() != expected.path()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || !scope
        {
            return Err(invalid());
        }
        Ok(url)
    }

    async fn following_page(&self, state: &mut State) -> Result<()> {
        if state.pages >= super::MAX_PAGES {
            state.done = true;
            return Ok(());
        }
        if state.owner.is_none() {
            tokio::time::timeout(self.timeout, self.access_token(None))
                .await
                .map_err(|_| error("native Pixiv extraction timed out"))??;
            state.owner = self.authenticated_user.lock().await.clone();
            if state.owner.is_none() {
                return Err(error("Pixiv OAuth response contains no current user ID"));
            }
        }
        let owner = state.owner.as_deref().expect("following owner");
        let endpoint = if !state.started {
            state.started = true;
            self.following_first(owner, state.private)
        } else {
            let Some(raw) = state.next.take() else {
                state.done = true;
                return Ok(());
            };
            self.following_next(&raw, owner, state.private)?
        };
        let mut canonical = endpoint.clone();
        let mut pairs: Vec<_> = endpoint
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        pairs.sort();
        canonical.set_query(None);
        canonical.query_pairs_mut().extend_pairs(pairs);
        if !state.seen_pages.insert(canonical.to_string()) {
            return Err(error("Pixiv following pagination repeated a page"));
        }
        let mut body = super::json(&self.get(&endpoint).await?)?;
        let users = body
            .get_mut("user_previews")
            .and_then(Value::as_array_mut)
            .ok_or_else(|| error("Pixiv response contains no following user list"))?;
        // Move the already bounded response array without cloning or preparing
        // every child's metadata. One preview is prepared per consumer poll.
        state.users = std::mem::take(users).into();
        state.next = super::string(&body, "next_url").map(str::to_owned);
        state.pages += 1;
        Ok(())
    }

    pub(super) fn following_events(
        &self,
        owner: Option<String>,
        private: bool,
    ) -> BoxStream<'_, Result<ExtractionEvent>> {
        if self.refresh_token.is_none() {
            return stream::once(async { Err(super::missing_token()) }).boxed();
        }
        let state = State {
            owner,
            private,
            next: None,
            started: false,
            pages: 0,
            seen_pages: BTreeSet::new(),
            users: VecDeque::new(),
            done: false,
        };
        let visited = Arc::new(Mutex::new(BTreeSet::new()));
        stream::unfold(state, move |mut state| async move {
            loop {
                if let Some(preview) = state.users.pop_front() {
                    let empty = serde_json::Map::new();
                    let raw = preview
                        .get("user")
                        .and_then(Value::as_object)
                        .unwrap_or(&empty);
                    // Queue predicates see the current producer ('following');
                    // its forced profile child later queues views as 'user'.
                    let metadata = ChildMetadata::from_raw("pixiv", "following", raw);
                    let id = metadata.fields().get("id").cloned();
                    return Some((Ok(User { metadata, id }), state));
                }
                if state.done {
                    return None;
                }
                if let Err(error) = self.following_page(&mut state).await {
                    state.done = true;
                    return Some((Err(error), state));
                }
            }
        })
        .flat_map(move |result| {
            let user = match result {
                Ok(user) => user,
                Err(error) => return stream::once(async move { Err(error) }).boxed(),
            };
            let visited = visited.clone();
            child_job(user.metadata, move || {
                // Retained ID came from bounded public metadata. Validation and
                // visited insertion happen only after queue selection accepted.
                let id = user.id.and_then(|id| id_of(&serde_json::json!({"id":id})));
                let Some(id) = id else {
                    return stream::once(async {
                        Err(error("Pixiv following preview contains an invalid user ID"))
                    })
                    .boxed();
                };
                if !visited
                    .lock()
                    .expect("following visited users")
                    .insert(id.clone())
                {
                    return stream::empty().boxed();
                }
                // Re-enter the forced bare-profile dispatcher, including its
                // independent nested view selection, rather than user artworks.
                let url = Url::parse(&format!("https://www.pixiv.net/users/{id}"))
                    .expect("validated user ID");
                self.events(&url)
            })
        })
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        dispatch::Include,
        event::media_only,
        filter::FilterExpression,
        job_selection::JobSelection,
        pixiv::Target,
        pixiv::tests::{pixiv, response, serve, serve_with, token},
        selection::RangeSelection,
    };
    use serde_json::json;

    fn preview(id: Value, account: &str) -> Value {
        json!({"user":{"id":id,"name":format!("Name {account}"),"account":account,
            "profile_image_urls":{"medium":"https://i.pximg.net/secret.jpg?SECRET"}, "private":"SECRET"},
            "illusts":[{"PRIVATE":"not retained"}]})
    }
    fn page(users: Vec<Value>, next: Option<Url>) -> Vec<u8> {
        response(
            "200 OK",
            json!({"user_previews":users,"next_url":next})
                .to_string()
                .as_bytes(),
        )
    }
    fn avatar(id: u64) -> Vec<u8> {
        response("200 OK", json!({"user":{"id":id,"account":format!("artist{id}"),"profile_image_urls":{
            "medium":format!("https://i.pximg.net/user-profile/img/2024/01/01/00/00/00/{id}_170.jpg")}}}).to_string().as_bytes())
    }
    fn artworks(id: u64, pages: usize) -> Vec<u8> {
        response("200 OK", json!({"illusts":[{"id":id,"type":"illust","user":{"id":77,"account":"original-artist"},
            "meta_pages":(0..pages).map(|index| json!({"image_urls":{"original":format!("https://i.pximg.net/img-original/{id}_p{index}.jpg")}})).collect::<Vec<_>>() }],"next_url":null}).to_string().as_bytes())
    }

    #[test]
    fn following_routes_preserve_first_query_values_and_ignore_optional_tag() {
        for (raw, id, private) in [
            ("https://www.pixiv.net/users/9/following", Some("9"), false),
            (
                "https://www.pixiv.net/en/users/9/following/ignored?rest=hide",
                Some("9"),
                true,
            ),
            (
                "https://www.pixiv.net/users/9/following/%00?rest=show&rest=hide",
                Some("9"),
                false,
            ),
            (
                "https://www.pixiv.net/bookmark.php?id=9&type=user&rest=hide",
                Some("9"),
                true,
            ),
            (
                "https://www.pixiv.net/bookmark.php?type=user&rest=hide&rest=show",
                None,
                true,
            ),
            (
                "https://www.pixiv.net/users/9/bookmarks/artworks/ignored?type=user",
                Some("9"),
                false,
            ),
        ] {
            assert_eq!(
                super::super::target(&Url::parse(raw).unwrap()).unwrap(),
                Target::Following {
                    id: id.map(str::to_owned),
                    private
                }
            );
        }
        assert!(matches!(
            super::super::target(
                &Url::parse("https://www.pixiv.net/bookmark.php?id=9&type=illust&type=user")
                    .unwrap()
            )
            .unwrap(),
            Target::Bookmarks { .. }
        ));
    }

    #[tokio::test]
    async fn modern_legacy_and_own_following_use_public_or_private_scope_and_profile_dispatch() {
        for (raw, owner, private) in [
            (
                "https://www.pixiv.net/users/9/following/ignored",
                "9",
                false,
            ),
            (
                "https://www.pixiv.net/users/9/bookmarks/artworks?type=user&rest=hide",
                "9",
                true,
            ),
            (
                "https://www.pixiv.net/bookmark.php?id=9&type=user&rest=hide",
                "9",
                true,
            ),
            (
                "https://www.pixiv.net/bookmark.php?type=user&rest=hide",
                "17",
                true,
            ),
        ] {
            let (root, server) = serve(vec![
                response(
                    "200 OK",
                    json!({"response":{"access_token":"following-access","user":{"id":17}}})
                        .to_string()
                        .as_bytes(),
                ),
                page(vec![preview(json!(20), "queued-artist")], None),
                avatar(20),
            ]);
            let native = pixiv(root).with_include(Some(Include::Views(vec!["avatar".into()])));
            let events = native
                .events(&Url::parse(raw).unwrap())
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .collect::<Result<Vec<_>>>()
                .unwrap();
            let scopes = events
                .iter()
                .filter_map(|event| {
                    if let ExtractionEvent::BeginChild(scope) = event {
                        Some(scope)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();
            assert_eq!(scopes.len(), 2);
            assert_eq!(scopes[0].metadata.subcategory(), "following");
            assert_eq!(scopes[0].metadata.fields()["id"], 20);
            assert_eq!(scopes[0].metadata.fields()["account"], "queued-artist");
            assert_eq!(scopes[1].metadata.subcategory(), "user");
            assert!(scopes[1].metadata.fields().is_empty());
            let media = events
                .iter()
                .find_map(|event| {
                    if let ExtractionEvent::Media(media) = event {
                        Some(media)
                    } else {
                        None
                    }
                })
                .unwrap();
            assert_eq!(media.metadata.as_ref().unwrap().post_id, "avatar");
            assert_eq!(
                media
                    .metadata
                    .as_ref()
                    .unwrap()
                    .profile_asset
                    .as_ref()
                    .unwrap()
                    .owner_id,
                "20"
            );
            assert_eq!(
                media.headers,
                std::collections::BTreeMap::from([(
                    "Referer".into(),
                    super::super::REFERER.into()
                )])
            );
            let requests = server.join().unwrap();
            assert!(requests[1].starts_with(&format!(
                "GET /v1/user/following?user_id={owner}&restrict={} ",
                if private { "private" } else { "public" }
            )));
            assert!(requests[2].starts_with("GET /v1/user/detail?user_id=20 "));
        }
    }

    #[tokio::test]
    async fn preview_scope_is_bounded_and_cancel_precedes_invalid_id_validation_or_child_http() {
        let (root, server) = serve(vec![
            token("t"),
            page(vec![preview(json!("invalid/id"), "skip")], None),
        ]);
        let native = pixiv(root);
        let mut events =
            native.events(&Url::parse("https://www.pixiv.net/users/9/following").unwrap());
        let ExtractionEvent::BeginChild(scope) = events.next().await.unwrap().unwrap() else {
            panic!("missing queued preview")
        };
        assert_eq!(scope.metadata.category(), "pixiv");
        assert_eq!(scope.metadata.subcategory(), "following");
        assert_eq!(scope.metadata.fields().len(), 3);
        assert_eq!(scope.metadata.fields()["id"], "invalid/id");
        assert_eq!(scope.metadata.fields()["account"], "skip");
        assert!(!format!("{:?}", scope.metadata).contains("SECRET"));
        scope.control.cancel();
        assert!(matches!(
            events.next().await.unwrap().unwrap(),
            ExtractionEvent::EndChild(_)
        ));
        assert!(events.next().await.is_none());
        assert_eq!(server.join().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn rejected_preview_does_not_validate_bad_id_or_authenticate_separate_novel_client() {
        let (root, server) = serve(vec![
            token("t"),
            page(vec![preview(json!("../bad"), "skip")], None),
        ]);
        let novel = Arc::new(Pixiv::new(std::time::Duration::from_secs(1)).unwrap());
        let native = pixiv(root)
            .with_include(Some(Include::Views(vec!["novel-user".into()])))
            .with_novel_extractor(novel);
        let items = JobSelection {
            child_filter: Some(
                FilterExpression::parse("subcategory == 'user' or account != 'skip'").unwrap(),
            ),
            ..Default::default()
        }
        .apply(native.events(&Url::parse("https://www.pixiv.net/users/9/following").unwrap()))
        .collect::<Vec<_>>()
        .await;
        assert!(items.is_empty(), "{items:?}");
        assert_eq!(server.join().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn rejected_duplicate_can_later_be_selected_and_accepted_duplicate_is_visited() {
        let (root, server) = serve(vec![
            token("t"),
            page(
                vec![
                    preview(json!(20), "skip"),
                    preview(json!(20), "accept"),
                    preview(json!("20"), "accept"),
                ],
                None,
            ),
            avatar(20),
        ]);
        let native = pixiv(root).with_include(Some(Include::Views(vec!["avatar".into()])));
        let files = JobSelection {
            child_filter: Some(
                FilterExpression::parse("subcategory == 'user' or account == 'accept'").unwrap(),
            ),
            ..Default::default()
        }
        .apply(native.events(&Url::parse("https://www.pixiv.net/users/9/following").unwrap()))
        .collect::<Vec<_>>()
        .await;
        assert_eq!(files.len(), 1, "{files:?}");
        assert!(files[0].is_ok());
        assert_eq!(server.join().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn selected_invalid_user_and_failed_child_leave_later_followed_siblings_available() {
        let (root, server) = serve(vec![
            token("t"),
            page(
                vec![
                    preview(json!("invalid"), "invalid"),
                    preview(json!(20), "first"),
                    preview(json!(20), "duplicate"),
                    preview(json!(21), "later"),
                ],
                None,
            ),
            response("503 Service Unavailable", b"SECRET"),
            avatar(21),
        ]);
        let native = pixiv(root).with_include(Some(Include::Views(vec!["avatar".into()])));
        let files = JobSelection::default()
            .apply(native.events(&Url::parse("https://www.pixiv.net/users/9/following").unwrap()))
            .collect::<Vec<_>>()
            .await;
        assert_eq!(files.len(), 3, "{files:?}");
        assert_eq!(
            files[0].as_ref().unwrap_err().to_string(),
            "Pixiv following preview contains an invalid user ID"
        );
        assert!(files[1].as_ref().unwrap_err().to_string().contains("503"));
        assert_eq!(
            files[2]
                .as_ref()
                .unwrap()
                .metadata
                .as_ref()
                .unwrap()
                .profile_asset
                .as_ref()
                .unwrap()
                .owner_id,
            "21"
        );
        assert_eq!(server.join().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn followed_profile_file_ranges_restart_and_do_not_fetch_more_views_or_users_early() {
        let (root, server) = serve(vec![
            token("t"),
            page(
                vec![preview(json!(20), "first"), preview(json!(21), "second")],
                None,
            ),
            artworks(41, 2),
            artworks(43, 2),
        ]);
        let native = pixiv(root);
        let files = JobSelection {
            file_range: Some(RangeSelection::parse("1").unwrap()),
            ..Default::default()
        }
        .apply(native.events(&Url::parse("https://www.pixiv.net/users/9/following").unwrap()))
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()
        .unwrap();
        assert_eq!(
            files
                .iter()
                .map(|file| file.filename.as_str())
                .collect::<Vec<_>>(),
            ["41_p0.jpg", "43_p0.jpg"]
        );
        for file in &files {
            assert_eq!(
                file.metadata.as_ref().unwrap().username.as_deref(),
                Some("original-artist")
            );
        }
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 4);
        assert!(requests[2].starts_with("GET /v1/user/illusts?user_id=20&type=illust "));
        assert!(requests[3].starts_with("GET /v1/user/illusts?user_id=21&type=illust "));
    }

    #[tokio::test]
    async fn consumer_drop_and_zero_child_range_leave_later_users_and_pages_unrequested() {
        for zero in [false, true] {
            let (root, server) = serve_with(|root| {
                let mut replies = vec![
                    token("t"),
                    page(
                        vec![preview(json!(20), "first"), preview(json!(21), "later")],
                        Some(
                            root.join("v1/user/following?user_id=9&restrict=public&offset=30")
                                .unwrap(),
                        ),
                    ),
                ];
                if !zero {
                    replies.push(avatar(20));
                }
                replies
            });
            let native = pixiv(root).with_include(Some(Include::Views(vec!["avatar".into()])));
            let url = Url::parse("https://www.pixiv.net/users/9/following").unwrap();
            if zero {
                let files = JobSelection {
                    child_range: Some(RangeSelection::parse("0").unwrap()),
                    ..Default::default()
                }
                .apply(native.events(&url))
                .collect::<Vec<_>>()
                .await;
                assert!(files.is_empty(), "{files:?}");
            } else {
                let mut files = media_only(native.events(&url));
                assert!(files.next().await.unwrap().is_ok());
                drop(files);
            }
            assert_eq!(server.join().unwrap().len(), if zero { 2 } else { 3 });
        }
    }

    #[tokio::test]
    async fn pagination_preserves_following_scope_and_fetches_only_after_current_children() {
        let (root, server) = serve_with(|root| {
            vec![
                token("t"),
                page(
                    vec![preview(json!(20), "first")],
                    Some(
                        root.join("v1/user/following?user_id=9&restrict=private&offset=30")
                            .unwrap(),
                    ),
                ),
                avatar(20),
                page(vec![preview(json!(21), "second")], None),
                avatar(21),
            ]
        });
        let native = pixiv(root).with_include(Some(Include::Views(vec!["avatar".into()])));
        let files = native
            .stream(&Url::parse("https://www.pixiv.net/users/9/following?rest=hide").unwrap())
            .collect::<Vec<_>>()
            .await;
        assert_eq!(files.len(), 2, "{files:?}");
        assert!(files.iter().all(Result::is_ok));
        let requests = server.join().unwrap();
        assert!(requests[2].starts_with("GET /v1/user/detail?user_id=20 "));
        assert!(
            requests[3].starts_with("GET /v1/user/following?user_id=9&restrict=private&offset=30 ")
        );
        assert!(requests[4].starts_with("GET /v1/user/detail?user_id=21 "));
    }

    #[tokio::test]
    async fn followed_profile_novel_include_uses_independent_category_credentials() {
        let (root, root_server) = serve(vec![
            token("root-following-access"),
            page(vec![preview(json!(20), "first")], None),
        ]);
        let (novel_api, novel_server) = serve(vec![token("novel-category-access"),
            response("200 OK",json!({"novels":[{"id":41,"title":"Novel","user":{"account":"original-author"}}],"next_url":null}).to_string().as_bytes()),
            response("200 OK",b"novel: {\"text\":\"raw novel text\"},\n")]);
        let native = pixiv(root)
            .with_include(Some(Include::Views(vec!["novel-user".into()])))
            .with_novel_extractor(Arc::new(pixiv(novel_api)));
        let files = native
            .stream(&Url::parse("https://www.pixiv.net/users/9/following").unwrap())
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].category, "pixiv-novel");
        assert_eq!(
            files[0].metadata.as_ref().unwrap().username.as_deref(),
            Some("original-author")
        );
        let root_requests = root_server.join().unwrap();
        let novel_requests = novel_server.join().unwrap();
        assert_eq!(root_requests.len(), 2);
        assert!(novel_requests[1].starts_with("GET /v1/user/novels?user_id=20 "));
        assert!(
            novel_requests[1]
                .to_ascii_lowercase()
                .contains("authorization: bearer novel-category-access")
        );
        assert!(!novel_requests[1].contains("root-following-access"));
        assert!(!root_requests[1].contains("novel-category-access"));
    }

    #[tokio::test]
    async fn followed_profile_post_ranges_reset_and_stop_before_later_work_or_manga_view() {
        use crate::post_selection::PostSelection;
        let work = |id| {
            json!({"id":id,"type":"illust","user":{"account":"original"},
            "meta_single_page":{"original_image_url":format!("https://i.pximg.net/img-original/{id}_p0.jpg")}})
        };
        let (root, server) = serve(vec![
            token("t"),
            page(
                vec![preview(json!(20), "first"), preview(json!(21), "second")],
                None,
            ),
            response(
                "200 OK",
                json!({"illusts":[work(41),work(42)],"next_url":null})
                    .to_string()
                    .as_bytes(),
            ),
            response(
                "200 OK",
                json!({"illusts":[work(43),work(44)],"next_url":null})
                    .to_string()
                    .as_bytes(),
            ),
        ]);
        let native = pixiv(root);
        let files = JobSelection {
            post_selection: Some(PostSelection::new(
                Some(RangeSelection::parse("1").unwrap()),
                None,
            )),
            ..Default::default()
        }
        .apply(native.events(&Url::parse("https://www.pixiv.net/users/9/following").unwrap()))
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()
        .unwrap();
        assert_eq!(
            files
                .iter()
                .map(|file| file.filename.as_str())
                .collect::<Vec<_>>(),
            ["41_p0.jpg", "43_p0.jpg"]
        );
        assert_eq!(server.join().unwrap().len(), 4);
    }

    #[test]
    fn next_page_rejects_origin_path_credentials_user_visibility_and_duplicate_scope() {
        let native = Pixiv::new(std::time::Duration::from_secs(1)).unwrap();
        for raw in [
            "https://evil.example/v1/user/following?user_id=9&restrict=private",
            "https://app-api.pixiv.net/v1/user/illusts?user_id=9&restrict=private",
            "https://user:pass@app-api.pixiv.net/v1/user/following?user_id=9&restrict=private",
            "https://app-api.pixiv.net/v1/user/following?user_id=10&restrict=private",
            "https://app-api.pixiv.net/v1/user/following?user_id=9&restrict=public",
            "https://app-api.pixiv.net/v1/user/following?user_id=9&restrict=private&restrict=private",
            "https://app-api.pixiv.net/v1/user/following?user_id=9&restrict=private#fragment",
        ] {
            assert!(native.following_next(raw, "9", true).is_err(), "{raw}");
        }
    }

    #[tokio::test]
    async fn canonical_cycle_guard_rejects_reordered_query_before_second_request() {
        let (root, server) = serve_with(|root| {
            vec![
                token("t"),
                page(
                    Vec::new(),
                    Some(
                        root.join("v1/user/following?restrict=public&user_id=9")
                            .unwrap(),
                    ),
                ),
            ]
        });
        let files = pixiv(root)
            .stream(&Url::parse("https://www.pixiv.net/users/9/following").unwrap())
            .collect::<Vec<_>>()
            .await;
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].as_ref().unwrap_err().to_string(),
            "Pixiv following pagination repeated a page"
        );
        assert_eq!(server.join().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn following_pagination_stops_at_two_hundred_pages() {
        let (root, server) = serve_with(|root| {
            let mut replies = vec![token("t")];
            replies.extend((1..=super::super::MAX_PAGES).map(|offset| {
                page(
                    Vec::new(),
                    Some(
                        root.join(&format!(
                            "v1/user/following?user_id=9&restrict=public&offset={offset}"
                        ))
                        .unwrap(),
                    ),
                )
            }));
            replies
        });
        let files = pixiv(root)
            .stream(&Url::parse("https://www.pixiv.net/users/9/following").unwrap())
            .collect::<Vec<_>>()
            .await;
        assert!(files.is_empty(), "{files:?}");
        assert_eq!(server.join().unwrap().len(), super::super::MAX_PAGES + 1);
    }
}
