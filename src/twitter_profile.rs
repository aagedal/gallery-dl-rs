//! Lazy authenticated UserMedia traversal. Query IDs are upstream snapshots.
use super::*;
use sha2::{Digest, Sha256};
use std::collections::{HashSet, VecDeque};
use timeline::ProfileInput;

struct State {
    input: ProfileInput,
    base: Url,
    owner: Option<String>,
    cursor: Option<String>,
    cursors: HashSet<[u8; 32]>,
    pending: VecDeque<Media>,
    done: bool,
    budget: Duration,
}

impl Twitter {
    /// Extract a status or lazily traverse an experimental profile media tab.
    /// Downstream pauses do not consume the cumulative active extraction budget.
    pub fn stream(&self, url: &Url) -> BoxStream<'_, Result<Media>> {
        if post_id(url).is_ok() {
            let url = url.clone();
            return stream::once(async move { self.extract(&url).await })
                .map_ok(|items| stream::iter(items.into_iter().map(Ok)))
                .try_flatten()
                .boxed();
        }
        match timeline::input(url) {
            Ok(input) => {
                self.media_stream(input, Url::parse("https://x.com/i/api/graphql/").unwrap())
            }
            Err(e) => stream::once(async move { Err(e) }).boxed(),
        }
    }

    pub(super) fn media_stream(
        &self,
        input: ProfileInput,
        base: Url,
    ) -> BoxStream<'_, Result<Media>> {
        let owner = match &input {
            ProfileInput::Id(id) => Some(id.clone()),
            ProfileInput::Name(_) => None,
        };
        let state = State {
            input,
            base,
            owner,
            cursor: None,
            cursors: HashSet::new(),
            pending: VecDeque::new(),
            done: false,
            budget: self.timeout,
        };
        stream::unfold(state, move |mut state| async move {
            loop {
                if let Some(item) = state.pending.pop_front() {
                    return Some((Ok(item), state));
                }
                if state.done {
                    return None;
                }
                let started = tokio::time::Instant::now();
                let result = tokio::time::timeout(state.budget, self.media_page(&mut state)).await;
                state.budget = state.budget.saturating_sub(started.elapsed());
                let result =
                    result.unwrap_or_else(|_| Err(error("native Twitter extraction timed out")));
                if let Err(e) = result {
                    state.done = true;
                    return Some((Err(e), state));
                }
            }
        })
        .boxed()
    }

    async fn media_page(&self, state: &mut State) -> Result<()> {
        if state.owner.is_none() {
            let ProfileInput::Name(name) = &state.input else {
                unreachable!()
            };
            let endpoint = user_endpoint(&state.base, name)?;
            self.ensure_transaction_at("", &endpoint).await?;
            let body = self.fetch(&endpoint, "", true).await?;
            state.owner = Some(timeline::parse_user(&body, &state.input)?);
            return Ok(());
        }
        let endpoint = media_endpoint(
            &state.base,
            state.owner.as_deref().unwrap(),
            state.cursor.as_deref(),
        )?;
        self.ensure_transaction_at("", &endpoint).await?;
        let body = self.fetch(&endpoint, "", true).await?;
        let page = timeline::parse_page(&body, state.owner.as_deref().unwrap())?;
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
        let mut media = page.media;
        for item in &mut media {
            self.network.apply_media(item);
        }
        state.pending = media.into();
        state.cursor = cursor;
        state.done = page.done;
        Ok(())
    }
}

fn user_endpoint(base: &Url, name: &str) -> Result<Url> {
    let variables = serde_json::json!({"screen_name":name,"withGrokTranslatedBio":false});
    let features = serde_json::json!({
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
    query(
        base,
        "ck5KkZ8t5cOmoLssopN99Q/UserByScreenName",
        variables,
        features,
        serde_json::json!({"withAuxiliaryUserLabels":true}),
    )
}

fn media_endpoint(base: &Url, id: &str, cursor: Option<&str>) -> Result<Url> {
    let mut variables = serde_json::json!({"userId":id,"count":50,"includePromotedContent":false,"withClientEventToken":false,"withBirdwatchNotes":false,"withVoice":true});
    if let Some(cursor) = cursor {
        variables["cursor"] = cursor.into();
    }
    let mut features = tweet_features();
    features["rweb_video_screen_enabled"] = false.into();
    query(
        base,
        "jCRhbOzdgOHp6u9H4g2tEg/UserMedia",
        variables,
        features,
        serde_json::json!({"withArticlePlainText":false}),
    )
}

fn query(base: &Url, path: &str, variables: Value, features: Value, toggles: Value) -> Result<Url> {
    let mut endpoint = base.join(path)?;
    endpoint
        .query_pairs_mut()
        .append_pair("variables", &variables.to_string())
        .append_pair("features", &features.to_string())
        .append_pair("fieldToggles", &toggles.to_string());
    Ok(endpoint)
}
