//! Lazy authenticated profile tab traversal. Query IDs are upstream snapshots.
use super::*;
use sha2::{Digest, Sha256};
use std::collections::{HashSet, VecDeque};
use timeline::{ProfileInput, ProfileTab};

struct State {
    input: ProfileInput,
    tab: ProfileTab,
    pinned_hint: Option<String>,
    pinned_pending: bool,
    base: Url,
    owner: Option<String>,
    lookup_pending: bool,
    cursor: Option<String>,
    cursors: HashSet<[u8; 32]>,
    seen: HashSet<[u8; 32]>,
    pending: VecDeque<Media>,
    posts: VecDeque<(String, Value)>,
    expansion: Option<expansion::Engine>,
    done: bool,
    budget: Duration,
}

impl Twitter {
    /// Extract a status or lazily traverse experimental profile tabs.
    /// Downstream pauses do not consume the cumulative active extraction budget.
    pub fn stream(&self, url: &Url) -> BoxStream<'_, Result<Media>> {
        if let Ok(id) = post_id(url) {
            return match endpoint(&id) {
                Ok(endpoint) => self.post_stream_at(endpoint, id),
                Err(e) => stream::once(async move { Err(e) }).boxed(),
            };
        }
        match timeline::profile_input(url) {
            Ok((input, tab)) => self.profile_stream(
                input,
                tab,
                Url::parse("https://x.com/i/api/graphql/").unwrap(),
            ),
            Err(e) => stream::once(async move { Err(e) }).boxed(),
        }
    }

    pub(super) fn post_stream_at(&self, endpoint: Url, id: String) -> BoxStream<'_, Result<Media>> {
        struct PostState {
            endpoint: Url,
            id: String,
            engine: Option<expansion::Engine>,
            pending: VecDeque<Media>,
            done: bool,
            seen: HashSet<[u8; 32]>,
            budget: Duration,
        }
        let state = PostState {
            endpoint,
            id,
            engine: None,
            pending: VecDeque::new(),
            done: false,
            seen: HashSet::new(),
            budget: self.timeout,
        };
        stream::unfold(state, move |mut state| async move {
            loop {
                if let Some(media) = state.pending.pop_front() {
                    return Some((Ok(media), state));
                }
                if state.done {
                    return None;
                }
                let started = tokio::time::Instant::now();
                let result = tokio::time::timeout(state.budget, async {
                    if state.engine.is_none() {
                        self.ensure_transaction_at(&state.id, &state.endpoint)
                            .await?;
                        let bytes = self.fetch(&state.endpoint, &state.id, true).await?;
                        let root = expansion::response_root(&bytes, &state.id)?;
                        state.engine = Some(
                            expansion::Engine::new(root, state.id.clone(), self.expansion, false)
                                .with_replies(self.profile_options.replies, None),
                        );
                    }
                    self.expand_next(state.engine.as_mut().unwrap(), &state.endpoint)
                        .await
                })
                .await
                .unwrap_or_else(|_| Err(error("native Twitter extraction timed out")));
                state.budget = state.budget.saturating_sub(started.elapsed());
                match result {
                    Ok(Some(media)) => {
                        if !queue_unique(media, &mut state.pending, &mut state.seen) {
                            tokio::task::yield_now().await;
                        }
                    }
                    Ok(None) => state.done = true,
                    Err(e) => {
                        state.done = true;
                        return Some((Err(e), state));
                    }
                }
            }
        })
        .boxed()
    }

    async fn expand_next(
        &self,
        engine: &mut expansion::Engine,
        base: &Url,
    ) -> Result<Option<Vec<Media>>> {
        loop {
            match engine.next()? {
                expansion::Action::Done => return Ok(None),
                expansion::Action::Media(mut media) => {
                    for item in &mut media {
                        self.network.apply_media(item);
                    }
                    return Ok(Some(media));
                }
                expansion::Action::Fetch(id) => {
                    // Only validated decimal IDs can select another fixed API endpoint.
                    let mut target = endpoint(&id)?;
                    target
                        .set_scheme(base.scheme())
                        .map_err(|_| error("Twitter API origin invalid"))?;
                    target
                        .set_host(base.host_str())
                        .map_err(|_| error("Twitter API origin invalid"))?;
                    target
                        .set_port(base.port())
                        .map_err(|_| error("Twitter API origin invalid"))?;
                    self.ensure_transaction_at(&id, &target).await?;
                    let bytes = self.fetch(&target, &id, true).await?;
                    engine.supply(&bytes)?;
                }
            }
        }
    }

    #[cfg(test)]
    pub(super) fn media_stream(
        &self,
        input: ProfileInput,
        base: Url,
    ) -> BoxStream<'_, Result<Media>> {
        self.profile_stream(input, ProfileTab::Media, base)
    }

    pub(super) fn profile_stream(
        &self,
        input: ProfileInput,
        tab: ProfileTab,
        base: Url,
    ) -> BoxStream<'_, Result<Media>> {
        let owner = match &input {
            ProfileInput::Id(id) => Some(id.clone()),
            ProfileInput::Name(_) => None,
        };
        let lookup_pending = owner.is_none() || self.profile_options.pinned;
        let state = State {
            input,
            tab,
            pinned_hint: None,
            pinned_pending: self.profile_options.pinned,
            base,
            owner,
            lookup_pending,
            cursor: None,
            cursors: HashSet::new(),
            seen: HashSet::new(),
            pending: VecDeque::new(),
            posts: VecDeque::new(),
            expansion: None,
            done: false,
            budget: self.timeout,
        };
        stream::unfold(state, move |mut state| async move {
            loop {
                if let Some(item) = state.pending.pop_front() {
                    return Some((Ok(item), state));
                }
                if state.done && state.posts.is_empty() && state.expansion.is_none() {
                    return None;
                }
                let started = tokio::time::Instant::now();
                let result = tokio::time::timeout(state.budget, self.media_page(&mut state)).await;
                state.budget = state.budget.saturating_sub(started.elapsed());
                let result =
                    result.unwrap_or_else(|_| Err(error("native Twitter extraction timed out")));
                if let Err(e) = result {
                    state.done = true;
                    state.posts.clear();
                    state.expansion = None;
                    return Some((Err(e), state));
                }
            }
        })
        .boxed()
    }

    async fn media_page(&self, state: &mut State) -> Result<()> {
        if state.expansion.is_none()
            && let Some((id, post)) = state.posts.pop_front()
        {
            // Timeline parsing has already validated membership and post identity.
            state.expansion = Some(
                expansion::Engine::new(post, id, self.expansion, true)
                    .with_replies(self.profile_options.replies, state.owner.clone()),
            );
        }
        if let Some(engine) = state.expansion.as_mut() {
            match self.expand_next(engine, &state.base).await? {
                Some(media) => {
                    if !queue_unique(media, &mut state.pending, &mut state.seen) {
                        tokio::task::yield_now().await;
                    }
                }
                None => state.expansion = None,
            }
            return Ok(());
        }
        if state.lookup_pending {
            let endpoint = user_endpoint(&state.base, &state.input)?;
            self.ensure_transaction_at("", &endpoint).await?;
            let body = self.fetch(&endpoint, "", true).await?;
            let owner = timeline::parse_user(&body, &state.input)?;
            if state.pinned_pending {
                state.pinned_hint = timeline::user_pin(&body, &owner)?;
            }
            state.owner = Some(owner);
            state.lookup_pending = false;
            return Ok(());
        }
        let endpoint = profile_endpoint(
            &state.base,
            state.tab,
            state.owner.as_deref().unwrap(),
            state.cursor.as_deref(),
        )?;
        self.ensure_transaction_at("", &endpoint).await?;
        let body = self.fetch(&endpoint, "", true).await?;
        let page = timeline::parse_profile_posts_with_pin(
            &body,
            state.owner.as_deref().unwrap(),
            state.tab,
            state.pinned_pending,
            state.pinned_hint.as_deref(),
        )?;
        state.pinned_pending = false;
        if !page.done && page.cursor.is_none() {
            return Err(error("Twitter media timeline pagination is incomplete"));
        }
        let cursor = if page.done { None } else { page.cursor };
        if let Some(cursor) = &cursor {
            let digest = <[u8; 32]>::from(Sha256::digest(cursor.as_bytes()));
            if !state.cursors.insert(digest) {
                return Err(error("Twitter media timeline cursor repeated"));
            }
        }
        state.posts = page.posts.into();
        state.cursor = cursor;
        state.done = page.done;
        Ok(())
    }
}

fn user_endpoint(base: &Url, input: &ProfileInput) -> Result<Url> {
    let (operation, variables, toggles) = match input {
        ProfileInput::Name(name) => (
            "ck5KkZ8t5cOmoLssopN99Q/UserByScreenName",
            serde_json::json!({"screen_name":name,"withGrokTranslatedBio":false}),
            serde_json::json!({"withAuxiliaryUserLabels":true}),
        ),
        ProfileInput::Id(id) => (
            "8r5oa_2vD0WkhIAOkY4TTA/UserByRestId",
            serde_json::json!({"userId":id}),
            Value::Null,
        ),
    };
    let mut features = serde_json::json!({
        "hidden_profile_subscriptions_enabled":true,"payments_enabled":false,"rweb_xchat_enabled":false,
        "profile_label_improvements_pcf_label_in_post_enabled":true,"rweb_tipjar_consumption_enabled":true,
        "verified_phone_label_enabled":false,"highlights_tweets_tab_ui_enabled":true,
        "responsive_web_twitter_article_notes_tab_enabled":true,"subscriptions_feature_can_gift_premium":true,
        "creator_subscriptions_tweet_preview_api_enabled":true,
        "responsive_web_graphql_skip_user_profile_image_extensions_enabled":false,
        "responsive_web_graphql_timeline_navigation_enabled":true,
        "subscriptions_verification_info_is_identity_verified_enabled":true,
        "subscriptions_verification_info_verified_since_enabled":true
    });
    if matches!(input, ProfileInput::Id(_)) {
        let features = features.as_object_mut().unwrap();
        features.remove("subscriptions_verification_info_is_identity_verified_enabled");
        features.remove("subscriptions_verification_info_verified_since_enabled");
    }
    query(base, operation, variables, features, toggles)
}

fn profile_endpoint(base: &Url, tab: ProfileTab, id: &str, cursor: Option<&str>) -> Result<Url> {
    let mut variables =
        serde_json::json!({"userId":id,"count":50,"includePromotedContent":false,"withVoice":true});
    let operation = match tab {
        ProfileTab::Media => {
            variables["withClientEventToken"] = false.into();
            variables["withBirdwatchNotes"] = false.into();
            "jCRhbOzdgOHp6u9H4g2tEg/UserMedia"
        }
        ProfileTab::Tweets => {
            variables["withQuickPromoteEligibilityTweetFields"] = false.into();
            "E8Wq-_jFSaU7hxVcuOPR9g/UserTweets"
        }
        ProfileTab::Replies => {
            variables["withCommunity"] = true.into();
            "-O3QOHrVn1aOm_cF5wyTCQ/UserTweetsAndReplies"
        }
        ProfileTab::Highlights => "gmHw9geMTncZ7jeLLUUNOw/UserHighlightsTweets",
    };
    if let Some(cursor) = cursor {
        variables["cursor"] = cursor.into();
    }
    let mut features = tweet_features();
    features["rweb_video_screen_enabled"] = false.into();
    query(
        base,
        operation,
        variables,
        features,
        serde_json::json!({"withArticlePlainText":false}),
    )
}

// Engine media batches belong to one emitted post. Keep all siblings from its
// first occurrence, then suppress the complete post on overlapping pages/pins.
fn queue_unique(
    media: Vec<Media>,
    pending: &mut VecDeque<Media>,
    seen: &mut HashSet<[u8; 32]>,
) -> bool {
    let Some(first) = media.first() else {
        return false;
    };
    let metadata = first.metadata.as_ref().unwrap();
    let digest = <[u8; 32]>::from(Sha256::digest(metadata.post_id.as_bytes()));
    if !seen.insert(digest) {
        return false;
    }
    pending.extend(media);
    true
}

fn query(base: &Url, path: &str, variables: Value, features: Value, toggles: Value) -> Result<Url> {
    let mut endpoint = base.join(path)?;
    endpoint
        .query_pairs_mut()
        .append_pair("variables", &variables.to_string())
        .append_pair("features", &features.to_string());
    if !toggles.is_null() {
        endpoint
            .query_pairs_mut()
            .append_pair("fieldToggles", &toggles.to_string());
    }
    Ok(endpoint)
}
