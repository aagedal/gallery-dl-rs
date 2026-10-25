//! Lazy authenticated profile tab traversal. Query IDs are upstream snapshots.
use super::*;
use crate::event::{self, ExtractionEvent};
use sha2::{Digest, Sha256};
use std::collections::{HashSet, VecDeque};
use timeline::{ProfileInput, ProfileTab};

enum Source {
    Profile(ProfileTab),
    Search(search::Paging),
    Feed(collections::Collection),
    Detail(detail::Selection),
    RestId(String),
}
enum RootStage {
    Initial { last: Option<String> },
    Links { base_query: String },
    All,
}

struct State {
    input: Option<ProfileInput>,
    source: Source,
    root: Option<RootStage>,
    screen_name: Option<String>,
    pinned_hint: Option<String>,
    pinned_pending: bool,
    base: Url,
    owner: Option<String>,
    lookup_pending: bool,
    cursor: Option<String>,
    cursors: HashSet<[u8; 32]>,
    empty_pages: u16,
    seen: HashSet<[u8; 32]>,
    pending: VecDeque<ExtractionEvent>,
    posts: VecDeque<(String, Value)>,
    expansion: Option<expansion::Engine>,
    thread: Option<threads::Frame>,
    conversations: HashSet<[u8; 32]>,
    thread_cache: BTreeMap<String, Value>,
    done: bool,
    budget: Duration,
    /// A guest read only the profile's best posts; the rest needs a session.
    guest_partial: bool,
    /// Source keywords shared by every post (upstream `metadata()`, e.g.
    /// an event's `event` object), loaded before the first page.
    extra: Option<std::sync::Arc<serde_json::Map<String, Value>>>,
}

impl Twitter {
    /// Extract a status or lazily traverse experimental profile tabs.
    /// Downstream pauses do not consume the cumulative active extraction budget.
    pub fn stream(&self, url: &Url) -> BoxStream<'_, Result<Media>> {
        event::media_only(self.events(url))
    }

    /// Explicit post boundaries from accepted native expansion batches.
    pub fn events(&self, url: &Url) -> BoxStream<'_, Result<ExtractionEvent>> {
        if let Some((id, format)) = images::image_input(url) {
            return match images::image_media(&id, &format, &self.image_options) {
                Ok(mut media) => {
                    self.network.apply_media(&mut media);
                    stream::iter(event::group(vec![media], "twitter").into_iter().map(Ok)).boxed()
                }
                Err(e) => stream::once(async move { Err(e) }).boxed(),
            };
        }
        if let Ok(id) = post_id(url) {
            return self.status_events(id);
        }
        match search::input(url) {
            Ok(Some(query)) => {
                return self
                    .search_events(query, Url::parse("https://x.com/i/api/graphql/").unwrap());
            }
            Err(e) => return stream::once(async move { Err(e) }).boxed(),
            Ok(None) => {}
        }
        match collections::input(url) {
            Ok(Some(kind)) => {
                return self.feed_events(kind, Url::parse("https://x.com/i/api/graphql/").unwrap());
            }
            Err(e) => return stream::once(async move { Err(e) }).boxed(),
            Ok(None) => {}
        }
        match users::input(url) {
            Ok(Some(target)) => {
                return self
                    .queue_events(target, Url::parse("https://x.com/i/api/graphql/").unwrap());
            }
            Err(e) => return stream::once(async move { Err(e) }).boxed(),
            Ok(None) => {}
        }
        match timeline::profile_input(url) {
            Ok((input, _)) if timeline::is_user_dispatch(url) => {
                self.user_events(input, Url::parse("https://x.com/i/api/graphql/").unwrap())
            }
            Ok((input, ProfileTab::Timeline)) => {
                self.timeline_events(input, Url::parse("https://x.com/i/api/graphql/").unwrap())
            }
            Ok((input, tab)) => self.profile_events(
                input,
                tab,
                Url::parse("https://x.com/i/api/graphql/").unwrap(),
            ),
            Err(e) => stream::once(async move { Err(e) }).boxed(),
        }
    }

    pub(super) fn user_events(
        &self,
        input: ProfileInput,
        base: Url,
    ) -> BoxStream<'_, Result<ExtractionEvent>> {
        let views = crate::dispatch::select(
            self.include.as_ref(),
            &["timeline"],
            &[
                "info",
                "avatar",
                "background",
                "timeline",
                "tweets",
                "media",
                "with-replies",
                "highlights",
                "likes",
            ],
            &[("replies", "with-replies")],
        );
        stream::iter(views)
            .flat_map(move |view| {
                let input = input.clone();
                let base = base.clone();
                event::child_job(
                    event::ChildMetadata::from_raw("twitter", "user", &serde_json::Map::new()),
                    move || {
                        let tab = match view.as_str() {
                            "info" => ProfileTab::Info,
                            "avatar" => ProfileTab::Avatar,
                            "background" => ProfileTab::Background,
                            "timeline" => ProfileTab::Timeline,
                            "tweets" => ProfileTab::Tweets,
                            "media" => ProfileTab::Media,
                            "with-replies" => ProfileTab::Replies,
                            "highlights" => ProfileTab::Highlights,
                            "likes" => ProfileTab::Likes,
                            _ => unreachable!("validated dispatcher view"),
                        };
                        self.profile_events(input, tab, base)
                    },
                )
            })
            .boxed()
    }

    pub(super) fn status_events(&self, id: String) -> BoxStream<'_, Result<ExtractionEvent>> {
        // Upstream `tweet-endpoint: auto` reads TweetDetail only when logged
        // in; guests are refused it and use TweetResultByRestId.
        if self.status_options.conversations != ConversationMode::Off
            || self.status_options.endpoint == TweetEndpoint::Detail
            || (self.status_options.endpoint == TweetEndpoint::Auto && self.has_session())
        {
            self.detail_events_at(Url::parse("https://x.com/i/api/graphql/").unwrap(), id)
        } else {
            match endpoint(&id) {
                Ok(target) => self.post_events_at(target, id),
                Err(e) => stream::once(async move { Err(e) }).boxed(),
            }
        }
    }

    pub(super) fn detail_events_at(
        &self,
        base: Url,
        id: String,
    ) -> BoxStream<'_, Result<ExtractionEvent>> {
        self.collection_events(
            None,
            Source::Detail(detail::Selection::new(
                id,
                self.status_options.conversations,
            )),
            None,
            base,
        )
    }

    pub(super) fn post_events_at(
        &self,
        endpoint: Url,
        id: String,
    ) -> BoxStream<'_, Result<ExtractionEvent>> {
        if self.thread_options.expand {
            return self.restid_events_at(endpoint, id);
        }
        struct PostState {
            endpoint: Url,
            id: String,
            engine: Option<expansion::Engine>,
            known: BTreeMap<String, Value>,
            pending: VecDeque<ExtractionEvent>,
            done: bool,
            seen: HashSet<[u8; 32]>,
            budget: Duration,
        }
        let state = PostState {
            endpoint,
            id,
            engine: None,
            known: BTreeMap::new(),
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
                        let owner = if self.profile_options.replies == ReplyMode::SelfOnly {
                            Some(
                                self.resolve_focal_owner(&root, &state.endpoint, &mut state.known)
                                    .await?,
                            )
                        } else {
                            None
                        };
                        state.engine = Some(
                            expansion::Engine::new(root, state.id.clone(), self.expansion, false)
                                .with_text_tweets(self.text_tweets)
                                .with_article_options(self.article_options)
                                .with_image_options(self.image_options.clone())
                                .with_card_options(self.card_options.clone())
                                .with_media_options(self.media_options)
                                .with_replies(self.profile_options.replies, owner),
                        );
                    }
                    self.expand_next_known(
                        state.engine.as_mut().unwrap(),
                        &state.endpoint,
                        Some(&state.known),
                        None,
                    )
                    .await
                })
                .await
                .unwrap_or_else(|_| Err(error("native Twitter extraction timed out")));
                state.budget = state.budget.saturating_sub(started.elapsed());
                match result {
                    Ok(Some(media)) => {
                        if !queue_unique(
                            media,
                            &mut state.pending,
                            &mut state.seen,
                            self.thread_options.unique,
                        ) {
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

    async fn expand_next_known(
        &self,
        engine: &mut expansion::Engine,
        base: &Url,
        known: Option<&BTreeMap<String, Value>>,
        extra: Option<&BTreeMap<String, Value>>,
    ) -> Result<Option<media::ParsedPost>> {
        loop {
            match engine.next()? {
                expansion::Action::Done => return Ok(None),
                expansion::Action::Post(mut post) => {
                    for item in &mut post.media {
                        self.network.apply_media(item);
                    }
                    return Ok(Some(*post));
                }
                expansion::Action::Fetch(id) => {
                    if let Some(value) = known
                        .and_then(|known| known.get(&id))
                        .or_else(|| extra.and_then(|known| known.get(&id)))
                    {
                        engine.supply_value(value.clone())?;
                        continue;
                    }
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
        event::media_only(self.profile_events(input, ProfileTab::Media, base))
    }

    pub(super) fn status_stream(&self, id: String) -> BoxStream<'_, Result<Media>> {
        event::media_only(self.status_events(id))
    }

    #[cfg(test)]
    pub(super) fn detail_stream_at(&self, base: Url, id: String) -> BoxStream<'_, Result<Media>> {
        event::media_only(self.detail_events_at(base, id))
    }
    #[cfg(test)]
    pub(super) fn post_stream_at(&self, endpoint: Url, id: String) -> BoxStream<'_, Result<Media>> {
        event::media_only(self.post_events_at(endpoint, id))
    }
    #[cfg(test)]
    pub(super) fn profile_stream(
        &self,
        input: ProfileInput,
        tab: ProfileTab,
        base: Url,
    ) -> BoxStream<'_, Result<Media>> {
        event::media_only(self.profile_events(input, tab, base))
    }
    #[cfg(test)]
    pub(super) fn timeline_stream(
        &self,
        input: ProfileInput,
        base: Url,
    ) -> BoxStream<'_, Result<Media>> {
        event::media_only(self.timeline_events(input, base))
    }
    #[cfg(test)]
    pub(super) fn search_stream(&self, query: String, base: Url) -> BoxStream<'_, Result<Media>> {
        event::media_only(self.search_events(query, base))
    }
    #[cfg(test)]
    pub(super) fn feed_stream(
        &self,
        kind: collections::Collection,
        base: Url,
    ) -> BoxStream<'_, Result<Media>> {
        event::media_only(self.feed_events(kind, base))
    }

    pub(super) fn profile_events(
        &self,
        input: ProfileInput,
        tab: ProfileTab,
        base: Url,
    ) -> BoxStream<'_, Result<ExtractionEvent>> {
        if tab == ProfileTab::Timeline {
            return self.timeline_events(input, base);
        }
        if matches!(
            tab,
            ProfileTab::Info | ProfileTab::Avatar | ProfileTab::Background
        ) {
            return stream::once(async move {
                let endpoint = user_endpoint(&base, &input)?;
                self.ensure_transaction_at("", &endpoint).await?;
                let bytes = self.fetch(&endpoint, "", true).await?;
                let mut events = profile_view(&bytes, &input, tab)?;
                for event in &mut events {
                    if let ExtractionEvent::Media(media) = event {
                        self.network.apply_media(media);
                    }
                }
                Ok::<_, crate::Error>(stream::iter(events.into_iter().map(Ok)))
            })
            .try_flatten()
            .boxed();
        }
        self.collection_events(Some(input), Source::Profile(tab), None, base)
    }

    pub(super) fn timeline_events(
        &self,
        input: ProfileInput,
        base: Url,
    ) -> BoxStream<'_, Result<ExtractionEvent>> {
        let tab = match self.timeline_options.strategy {
            TimelineStrategy::Auto if self.expansion.retweets == RetweetMode::Skip => {
                ProfileTab::Media
            }
            TimelineStrategy::Auto | TimelineStrategy::Tweets => ProfileTab::Tweets,
            TimelineStrategy::Media => ProfileTab::Media,
            TimelineStrategy::Replies => ProfileTab::Replies,
        };
        self.collection_events(
            Some(input),
            Source::Profile(tab),
            Some(RootStage::Initial { last: None }),
            base,
        )
    }

    pub(super) fn search_events(
        &self,
        query: String,
        base: Url,
    ) -> BoxStream<'_, Result<ExtractionEvent>> {
        if let Err(e) = search::validate_query(&query) {
            return stream::once(async move { Err(e) }).boxed();
        }
        let input = (self.profile_options.replies == ReplyMode::SelfOnly)
            .then(|| search::query_owner(&query))
            .flatten();
        self.collection_events(
            input,
            Source::Search(search::Paging::new(query)),
            None,
            base,
        )
    }

    pub(super) fn feed_events(
        &self,
        kind: collections::Collection,
        base: Url,
    ) -> BoxStream<'_, Result<ExtractionEvent>> {
        self.collection_events(None, Source::Feed(kind), None, base)
    }

    pub(super) fn restid_events_at(
        &self,
        target: Url,
        id: String,
    ) -> BoxStream<'_, Result<ExtractionEvent>> {
        // Keep the exact initial endpoint for fixture injection and request scoping.
        self.collection_events(None, Source::RestId(id), None, target)
    }

    fn collection_events(
        &self,
        input: Option<ProfileInput>,
        source: Source,
        root: Option<RootStage>,
        base: Url,
    ) -> BoxStream<'_, Result<ExtractionEvent>> {
        let owner = match &input {
            Some(ProfileInput::Id(id)) => Some(id.clone()),
            _ => None,
        };
        let profile_source = matches!(source, Source::Profile(_));
        let lookup_pending = input.is_some()
            && (owner.is_none() || self.profile_options.pinned && profile_source || root.is_some());
        let state = State {
            input,
            source,
            root,
            screen_name: None,
            pinned_hint: None,
            pinned_pending: self.profile_options.pinned,
            base,
            owner,
            lookup_pending,
            cursor: None,
            cursors: HashSet::new(),
            empty_pages: 0,
            seen: HashSet::new(),
            pending: VecDeque::new(),
            posts: VecDeque::new(),
            expansion: None,
            thread: None,
            conversations: HashSet::new(),
            thread_cache: BTreeMap::new(),
            done: false,
            budget: self.timeout,
            guest_partial: false,
            extra: None,
        };
        stream::unfold(state, move |mut state| async move {
            loop {
                if let Some(item) = state.pending.pop_front() {
                    return Some((Ok(item), state));
                }
                if state.done
                    && state.root.is_none()
                    && state.posts.is_empty()
                    && state.expansion.is_none()
                    && state.thread.is_none()
                {
                    if std::mem::take(&mut state.guest_partial) {
                        return Some((
                            Err(error(
                                "X shows guests only a profile's top posts; pass x.com \
                                 auth_token and ct0 cookies with --cookies for the full timeline",
                            )),
                            state,
                        ));
                    }
                    return None;
                }
                let started = tokio::time::Instant::now();
                let result = tokio::time::timeout(state.budget, self.media_page(&mut state)).await;
                state.budget = state.budget.saturating_sub(started.elapsed());
                let result =
                    result.unwrap_or_else(|_| Err(error("native Twitter extraction timed out")));
                if let Err(e) = result {
                    state.done = true;
                    state.root = None;
                    state.posts.clear();
                    state.expansion = None;
                    state.thread = None;
                    return Some((Err(e), state));
                }
            }
        })
        .boxed()
    }

    async fn media_page(&self, state: &mut State) -> Result<()> {
        if let Some(engine) = state.expansion.as_mut() {
            let known = if let Some(thread) = &state.thread {
                Some(&thread.known)
            } else {
                match &state.source {
                    Source::Detail(selection) => Some(&selection.known),
                    _ => None,
                }
            };
            match self
                .expand_next_known(engine, &state.base, known, Some(&state.thread_cache))
                .await?
            {
                Some(mut media) => {
                    if let Some(extra) = &state.extra {
                        media.metadata.extra_keywords = Some(extra.clone());
                        for item in &mut media.media {
                            if let Some(metadata) = item.metadata.as_mut() {
                                metadata.extra_keywords = Some(extra.clone());
                            }
                        }
                    }
                    if matches!(
                        &state.source,
                        Source::Feed(
                            collections::Collection::Community(_)
                                | collections::Collection::Communities
                        )
                    ) {
                        media.metadata.community_archive = true;
                        for item in &mut media.media {
                            if let Some(metadata) = item.metadata.as_mut() {
                                metadata.community_archive = true;
                            }
                        }
                    }
                    if !queue_unique(
                        media,
                        &mut state.pending,
                        &mut state.seen,
                        self.thread_options.unique,
                    ) {
                        tokio::task::yield_now().await;
                    }
                }
                None => state.expansion = None,
            }
            return Ok(());
        }
        if let Some(thread) = state.thread.as_mut() {
            if let Some((id, post)) = thread.posts.pop_front() {
                state.expansion = Some(
                    expansion::Engine::new(post, id, self.expansion, true)
                        .with_text_tweets(self.text_tweets)
                        .with_article_options(self.article_options)
                        .with_image_options(self.image_options.clone())
                        .with_card_options(self.card_options.clone())
                        .with_media_options(self.media_options)
                        .with_replies(self.profile_options.replies, state.owner.clone()),
                );
                return Ok(());
            }
            if thread.done {
                state.thread = None;
                return Ok(());
            }
            let result = self.thread_page(thread, &state.base).await;
            if result.is_err() {
                // Upstream falls back to the original raw post on detail API failure.
                let frame = state.thread.take().unwrap();
                state.expansion = Some(
                    expansion::Engine::new(
                        frame.original.1,
                        frame.original.0,
                        self.expansion,
                        true,
                    )
                    .with_text_tweets(self.text_tweets)
                    .with_article_options(self.article_options)
                    .with_image_options(self.image_options.clone())
                    .with_card_options(self.card_options.clone())
                    .with_media_options(self.media_options)
                    .with_replies(self.profile_options.replies, state.owner.clone()),
                );
            }
            return Ok(());
        }
        if let Some((id, post)) = state.posts.pop_front() {
            // RestId has only one parent and may have resolved its original author.
            if !matches!(state.source, Source::RestId(_)) {
                state.thread_cache.clear();
            }
            if self.thread_options.expand {
                let available = match &state.source {
                    Source::Detail(selection) => Some(&selection.known),
                    _ => None,
                };
                let lookup = self
                    .thread_lookup(&post, &state.base, &mut state.thread_cache, available)
                    .await?;
                match lookup {
                    expansion::ThreadLookup::Id(cid) => {
                        let digest = <[u8; 32]>::from(Sha256::digest(cid.as_bytes()));
                        if state.conversations.insert(digest) {
                            state.thread = Some(threads::Frame::new(
                                cid,
                                (id, post),
                                self.profile_options.pinned,
                            ));
                        }
                        tokio::task::yield_now().await;
                        return Ok(());
                    }
                    expansion::ThreadLookup::Skip => {
                        tokio::task::yield_now().await;
                        return Ok(());
                    }
                    expansion::ThreadLookup::Bypass => {}
                    expansion::ThreadLookup::Fetch(_) => unreachable!(),
                }
            }
            state.expansion = Some(
                expansion::Engine::new(post, id, self.expansion, true)
                    .with_text_tweets(self.text_tweets)
                    .with_article_options(self.article_options)
                    .with_image_options(self.image_options.clone())
                    .with_card_options(self.card_options.clone())
                    .with_media_options(self.media_options)
                    .with_replies(self.profile_options.replies, state.owner.clone()),
            );
            return Ok(());
        }
        if state.done {
            self.advance_root(state)?;
            return Ok(());
        }
        if state.lookup_pending {
            let input = state.input.as_ref().unwrap();
            let endpoint = user_endpoint(&state.base, input)?;
            self.ensure_transaction_at("", &endpoint).await?;
            let body = self.fetch(&endpoint, "", true).await?;
            let owner = timeline::parse_user(&body, input)?;
            if state.root.is_some() {
                state.screen_name = Some(timeline::parse_user_name(&body, &owner)?);
            }
            if state.pinned_pending {
                state.pinned_hint = timeline::user_pin(&body, &owner)?;
            }
            state.owner = Some(owner);
            state.lookup_pending = false;
            return Ok(());
        }
        if let Source::RestId(id) = &state.source {
            let target = state.base.clone();
            self.ensure_transaction_at(id, &target).await?;
            let body = self.fetch(&target, id, true).await?;
            let root = expansion::response_root(&body, id)?;
            if self.profile_options.replies == ReplyMode::SelfOnly {
                state.owner = Some(
                    self.resolve_focal_owner(&root, &state.base, &mut state.thread_cache)
                        .await?,
                );
            }
            state.posts.push_back((id.clone(), root));
            state.done = true;
            return Ok(());
        }
        let page = match &mut state.source {
            Source::RestId(_) => unreachable!(),
            Source::Profile(tab) => {
                // Guests are refused UserMedia but may read UserTweets, which
                // returns the profile's top posts in one page; text-only posts
                // yield no files there.
                let guest = self.is_guest();
                let tab = &mut if guest && *tab == ProfileTab::Media {
                    ProfileTab::Tweets
                } else {
                    *tab
                };
                let target = profile_endpoint(
                    &state.base,
                    *tab,
                    state.owner.as_deref().unwrap(),
                    state.cursor.as_deref(),
                    self.timeline_options.limit,
                )?;
                self.ensure_transaction_at("", &target).await?;
                let body = self.fetch(&target, "", true).await?;
                let page = timeline::parse_profile_posts_with_pin(
                    &body,
                    state.owner.as_deref().unwrap(),
                    *tab,
                    state.pinned_pending,
                    state.pinned_hint.as_deref(),
                )?;
                state.pinned_pending = false;
                if !page.done && page.cursor.is_none() && !guest {
                    return Err(error("Twitter profile timeline pagination is incomplete"));
                }
                state.done = page.done
                    || (*tab == ProfileTab::Likes
                        && empty_feed_finished(&page, &mut state.empty_pages, 0));
                let cursor = if state.done {
                    None
                } else {
                    page.cursor.clone()
                };
                if let Some(cursor) = &cursor {
                    let digest = <[u8; 32]>::from(Sha256::digest(cursor.as_bytes()));
                    if !state.cursors.insert(digest) {
                        return Err(error("Twitter profile timeline cursor repeated"));
                    }
                }
                state.cursor = cursor;
                // A guest's single page is never the whole timeline.
                if guest && matches!(*tab, ProfileTab::Tweets) {
                    state.done = true;
                    state.cursor = None;
                    state.guest_partial = true;
                }
                if let Some(RootStage::Initial { last }) = &mut state.root
                    && let Some((id, _)) = page.posts.last()
                {
                    *last = Some(id.clone());
                }
                page
            }
            Source::Search(paging) => {
                paging.request()?;
                let target = search_endpoint(&state.base, paging, self.timeline_options)?;
                self.ensure_transaction_at("", &target).await?;
                let body = self.fetch(&target, "", true).await?;
                let page = timeline::parse_search_posts_with_pin(&body, state.pinned_pending)?;
                state.pinned_pending = false;
                state.done = paging.advance(&page, self.timeline_options)?;
                page
            }
            Source::Detail(selection) => {
                let target =
                    detail::endpoint_at(&state.base, &selection.id, state.cursor.as_deref())?;
                self.ensure_transaction_at(&selection.id, &target).await?;
                let body = self.fetch(&target, &selection.id, true).await?;
                let mut page = detail_parse::parse_page(&body, state.pinned_pending)?;
                state.pinned_pending = false;
                let done = page.done || empty_feed_finished(&page, &mut state.empty_pages, 0);
                let selected = selection.select(std::mem::take(&mut page.posts), done)?;
                if let Some(focal) = &selected.focal
                    && self.profile_options.replies == ReplyMode::SelfOnly
                {
                    state.owner = Some(
                        self.resolve_focal_owner(focal, &state.base, &mut selection.known)
                            .await?,
                    );
                }
                state.done = selected.done;
                let cursor = if state.done {
                    None
                } else {
                    page.cursor.clone()
                };
                if let Some(cursor) = &cursor {
                    let digest = <[u8; 32]>::from(Sha256::digest(cursor.as_bytes()));
                    if !state.cursors.insert(digest) {
                        return Err(error("Twitter conversation cursor repeated"));
                    }
                }
                state.cursor = cursor;
                page.posts = selected.posts;
                page
            }
            Source::Feed(kind) => {
                if let collections::Collection::Event(id) = kind
                    && state.extra.is_none()
                {
                    // Upstream `TwitterEventExtractor.metadata`.
                    let target = collections::rest_endpoint(
                        &state.base,
                        &format!("1.1/live_event/1/{id}/timeline.json"),
                        &[("count", "0"), ("urt", "true")],
                    )?;
                    self.ensure_transaction_at("", &target).await?;
                    let body = self.fetch(&target, "", true).await?;
                    let event = collections::parse_live_event(&body, id)?;
                    state.extra = Some(std::sync::Arc::new(serde_json::Map::from_iter([(
                        "event".to_owned(),
                        event,
                    )])));
                    return Ok(());
                }
                let target = feed_endpoint(
                    &state.base,
                    kind,
                    state.cursor.as_deref(),
                    self.timeline_options.limit,
                    self.text_tweets,
                )?;
                if matches!(kind, collections::Collection::Notifications)
                    && !self.has_session_for(&target)
                {
                    // Guests cannot read notifications; fail before any request.
                    return Err(error(
                        "Twitter notifications require a logged-in session; pass x.com \
                         auth_token and ct0 cookies with --cookies",
                    ));
                }
                self.ensure_transaction_at("", &target).await?;
                let body = self.fetch(&target, "", true).await?;
                let page = if self.text_tweets {
                    collections::parse_collection_posts_with_text(
                        &body,
                        kind,
                        state.pinned_pending,
                        true,
                    )?
                } else {
                    collections::parse_collection_posts(&body, kind, state.pinned_pending)?
                };
                state.pinned_pending = false;
                let allowance = if matches!(kind, collections::Collection::Bookmarks) {
                    128
                } else {
                    0
                };
                state.done =
                    page.done || empty_feed_finished(&page, &mut state.empty_pages, allowance);
                let cursor = if state.done {
                    None
                } else {
                    page.cursor.clone()
                };
                if let Some(cursor) = &cursor {
                    let digest = <[u8; 32]>::from(Sha256::digest(cursor.as_bytes()));
                    if !state.cursors.insert(digest) {
                        return Err(error("Twitter collection cursor repeated"));
                    }
                }
                state.cursor = cursor;
                page
            }
        };
        state.posts = page.posts.into();
        Ok(())
    }

    async fn thread_page(&self, frame: &mut threads::Frame, base: &Url) -> Result<()> {
        // A RestId parent's base is a complete status URL; detail uses its API directory.
        let mut api_base = base.clone();
        api_base.set_path("/i/api/graphql/");
        api_base.set_query(None);
        api_base.set_fragment(None);
        let target = detail::endpoint_at(&api_base, &frame.id, frame.cursor.as_deref())?;
        self.ensure_transaction_at(&frame.id, &target).await?;
        let bytes = self.fetch(&target, &frame.id, true).await?;
        let page = detail_parse::parse_page(&bytes, frame.pinned)?;
        frame.pinned = false;
        frame.done = page.done || (page.posts.is_empty() && !page.continue_empty);
        let cursor = if frame.done { None } else { page.cursor };
        if let Some(cursor) = &cursor {
            let digest = <[u8; 32]>::from(Sha256::digest(cursor.as_bytes()));
            if !frame.cursors.insert(digest) {
                return Err(error("Twitter conversation cursor repeated"));
            }
        }
        frame.cursor = cursor;
        frame.known.clear();
        for (id, value) in &page.posts {
            frame
                .known
                .entry(id.clone())
                .or_insert_with(|| value.clone());
        }
        frame.posts = page.posts.into();
        Ok(())
    }

    async fn thread_lookup(
        &self,
        value: &Value,
        base: &Url,
        known: &mut BTreeMap<String, Value>,
        available: Option<&BTreeMap<String, Value>>,
    ) -> Result<expansion::ThreadLookup> {
        loop {
            match expansion::thread_lookup(value, self.expansion.retweets, known)? {
                expansion::ThreadLookup::Fetch(id) => {
                    if let Some(value) = available.and_then(|posts| posts.get(&id)) {
                        known.insert(id, value.clone());
                        continue;
                    }
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
                    known.insert(id.clone(), expansion::response_root(&bytes, &id)?);
                }
                lookup => return Ok(lookup),
            }
        }
    }

    async fn resolve_focal_owner(
        &self,
        value: &Value,
        base: &Url,
        known: &mut BTreeMap<String, Value>,
    ) -> Result<String> {
        loop {
            match expansion::owner_lookup(value, self.expansion.retweets, known)? {
                expansion::OwnerLookup::Owner(owner) => return Ok(owner),
                expansion::OwnerLookup::Fetch(id) => {
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
                    known.insert(id.clone(), expansion::response_root(&bytes, &id)?);
                }
            }
        }
    }

    fn advance_root(&self, state: &mut State) -> Result<()> {
        // Guests cannot search; their timeline ends with its top posts.
        if state.guest_partial {
            state.root = None;
        }
        let next = match state.root.take() {
            Some(RootStage::Initial { last: Some(id) }) => {
                let name = state
                    .screen_name
                    .as_deref()
                    .ok_or_else(|| error("Twitter profile name missing for timeline search"))?;
                let mut base_query = format!("from:{name} max_id:{id}");
                if self.expansion.retweets != RetweetMode::Skip {
                    base_query.push_str(" include:retweets include:nativeretweets");
                }
                let query = format!("{base_query} filter:links");
                state.root = Some(RootStage::Links { base_query });
                Some(query)
            }
            Some(RootStage::Links { base_query }) => {
                let any_posts = matches!(&state.source, Source::Search(paging) if paging.any_posts);
                if any_posts {
                    None
                } else {
                    state.root = Some(RootStage::All);
                    Some(base_query)
                }
            }
            _ => None,
        };
        if let Some(query) = next {
            search::validate_query(&query)?;
            state.source = Source::Search(search::Paging::new(query));
            state.cursor = None;
            state.cursors.clear();
            state.empty_pages = 0;
            state.pinned_pending = self.profile_options.pinned;
            state.done = false;
        }
        Ok(())
    }
}

pub(crate) fn profile_asset_identity(
    kind: crate::profile_asset::ProfileAssetKind,
    owner: &str,
    url: &Url,
) -> Option<crate::profile_asset::ProfileAssetMetadata> {
    use crate::profile_asset::{ProfileAssetKind, ProfileAssetMetadata};
    if !valid_id(owner)
        || url.as_str().len() > 4096
        || url.scheme() != "https"
        || url.host_str() != Some("pbs.twimg.com")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    let parts: Vec<_> = url.path().strip_prefix('/')?.split('/').collect();
    let version = match (kind, parts.as_slice()) {
        (ProfileAssetKind::Avatar, ["profile_images", version, filename]) if valid_id(version) => {
            let (stem, extension) = filename.rsplit_once('.')?;
            if stem.is_empty()
                || !stem
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
                || !matches!(
                    extension.to_ascii_lowercase().as_str(),
                    "jpg" | "jpeg" | "png" | "gif" | "webp" | "avif"
                )
            {
                return None;
            }
            *version
        }
        (ProfileAssetKind::Background, ["profile_banners", source_owner, version])
            if *source_owner == owner && valid_id(version) =>
        {
            let seconds: u64 = version.parse().ok()?;
            u128::from(seconds)
                .checked_mul(1000)?
                .checked_sub(1_288_834_974_657)?;
            *version
        }
        _ => return None,
    };
    Some(ProfileAssetMetadata {
        kind,
        owner_id: owner.into(),
        version: version.into(),
    })
}

pub(super) fn profile_view(
    bytes: &[u8],
    input: &ProfileInput,
    tab: ProfileTab,
) -> Result<VecDeque<ExtractionEvent>> {
    use crate::{extract::MediaJob, profile_asset::ProfileAssetKind};
    use std::sync::Arc;
    let invalid = || error("Twitter profile asset is missing valid public identity or metadata");
    let owner = timeline::parse_user(bytes, input)?;
    let name = timeline::parse_user_name(bytes, &owner)?;
    let document = timeline::document(bytes)?;
    let user = document.pointer("/data/user/result").ok_or_else(invalid)?;
    let bounded_text = |value: Option<&Value>| {
        value
            .and_then(Value::as_str)
            .filter(|value| value.len() <= 64 * 1024 && !value.contains('\0'))
            .map(Arc::<str>::from)
    };
    if tab == ProfileTab::Info {
        let created = user
            .pointer("/core/created_at")
            .or_else(|| user.pointer("/legacy/created_at"))
            .and_then(Value::as_str)
            .and_then(timestamp);
        return Ok(VecDeque::from([
            ExtractionEvent::Post(Box::new(event::Post {
                keywords: Default::default(),
                category: "twitter".into(),
                metadata: Some(PostMetadata {
                    post_id: owner,
                    username: Some(Arc::from(name)),
                    caption: bounded_text(user.pointer("/legacy/description")),
                    timestamp: created,
                    ..PostMetadata::default()
                }),
                count: Some(0),
            })),
            ExtractionEvent::EndPost,
        ]));
    }
    let (kind, source) = match tab {
        ProfileTab::Avatar => (
            ProfileAssetKind::Avatar,
            user.pointer("/avatar/image_url")
                .or_else(|| user.pointer("/legacy/profile_image_url_https")),
        ),
        ProfileTab::Background => (
            ProfileAssetKind::Background,
            user.pointer("/legacy/profile_banner_url"),
        ),
        _ => return Err(invalid()),
    };
    let Some(source) = source.filter(|source| !source.is_null()) else {
        return Ok(VecDeque::new());
    };
    let source = source
        .as_str()
        .filter(|source| source.len() <= 4096)
        .ok_or_else(invalid)?;
    if source.is_empty()
        || kind == ProfileAssetKind::Avatar
            && source
                == "https://abs.twimg.com/sticky/default_profile_images/default_profile_normal.png"
    {
        return Ok(VecDeque::new());
    }
    let mut url = Url::parse(source).map_err(|_| invalid())?;
    if url.scheme() != "https"
        || url.host_str() != Some("pbs.twimg.com")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid());
    }
    let parts: Vec<_> = url.path().trim_start_matches('/').split('/').collect();
    let (post_id, version, timestamp, extension) = match (kind, parts.as_slice()) {
        (ProfileAssetKind::Avatar, ["profile_images", version, filename]) if valid_id(version) => {
            let filename = filename.replace("_normal.", ".");
            let (stem, extension) = filename.rsplit_once('.').ok_or_else(invalid)?;
            if stem.is_empty()
                || !stem
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
                || !matches!(
                    extension.to_ascii_lowercase().as_str(),
                    "jpg" | "jpeg" | "png" | "gif" | "webp" | "avif"
                )
            {
                return Err(invalid());
            }
            let version_id: u64 = version.parse().map_err(|_| invalid())?;
            let timestamp = ((version_id >> 22) + 1_288_834_974_657) / 1000;
            (
                version.to_string(),
                version.to_string(),
                Some(timestamp as i64),
                extension.to_ascii_lowercase(),
            )
        }
        (ProfileAssetKind::Background, ["profile_banners", source_owner, version])
            if *source_owner == owner && valid_id(version) =>
        {
            let seconds: u64 = version.parse().map_err(|_| invalid())?;
            let id = u128::from(seconds)
                .checked_mul(1000)
                .and_then(|milliseconds| milliseconds.checked_sub(1_288_834_974_657))
                .and_then(|difference| difference.checked_mul(1 << 22))
                .ok_or_else(invalid)?;
            (
                id.to_string(),
                version.to_string(),
                i64::try_from(seconds).ok(),
                String::new(),
            )
        }
        _ => return Err(invalid()),
    };
    if kind == ProfileAssetKind::Avatar {
        url.set_path(&url.path().replace("_normal.", "."));
    }
    let asset = profile_asset_identity(kind, &owner, &url).ok_or_else(invalid)?;
    let filename = if extension.is_empty() {
        format!("{}_{}_{}", kind.as_str(), owner, version)
    } else {
        format!("{}_{}_{}.{}", kind.as_str(), owner, version, extension)
    };
    let media = Media {
        url,
        job: MediaJob::DirectHttp,
        category: "twitter".into(),
        filename,
        metadata: Some(PostMetadata {
            profile_asset: Some(asset),
            post_id,
            media_id: "1".into(),
            username: Some(Arc::from(name)),
            index: 1,
            timestamp,
            ..PostMetadata::default()
        }),
        headers: BTreeMap::new(),
        handoff: BTreeMap::new(),
    };
    Ok(event::group(vec![media], "twitter"))
}

pub(super) fn user_endpoint(base: &Url, input: &ProfileInput) -> Result<Url> {
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

fn profile_endpoint(
    base: &Url,
    tab: ProfileTab,
    id: &str,
    cursor: Option<&str>,
    limit: u8,
) -> Result<Url> {
    validate_limit(limit)?;
    let mut variables = serde_json::json!({"userId":id,"count":limit,"includePromotedContent":false,"withVoice":true});
    let operation = match tab {
        ProfileTab::Media | ProfileTab::Likes => {
            variables["withClientEventToken"] = false.into();
            variables["withBirdwatchNotes"] = false.into();
            if tab == ProfileTab::Media {
                "jCRhbOzdgOHp6u9H4g2tEg/UserMedia"
            } else {
                "TGEKkJG_meudeaFcqaxM-Q/Likes"
            }
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
        ProfileTab::Timeline => return Err(error("Twitter timeline strategy was not resolved")),
        ProfileTab::Info | ProfileTab::Avatar | ProfileTab::Background => {
            return Err(error("Twitter profile view is not a post timeline"));
        }
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

fn validate_limit(limit: u8) -> Result<()> {
    if !(1..=100).contains(&limit) {
        return Err(error("Twitter page size exceeds its limits"));
    }
    Ok(())
}

fn empty_feed_finished(page: &timeline::Page, count: &mut u16, allowance: u16) -> bool {
    if !page.posts.is_empty() || page.continue_empty {
        *count = 0;
        return false;
    }
    if *count >= allowance {
        return true;
    }
    *count += 1;
    false
}

fn feed_endpoint(
    base: &Url,
    kind: &collections::Collection,
    cursor: Option<&str>,
    limit: u8,
    text_tweets: bool,
) -> Result<Url> {
    use collections::Collection;
    validate_limit(limit)?;
    if let Collection::Event(id) = kind {
        // Upstream `live_event_timeline` over the legacy REST API.
        if !valid_id(id) {
            return Err(error("Twitter event identifier invalid"));
        }
        let mut extra = vec![
            ("timeline_id", "recap"),
            ("urt", "true"),
            ("get_annotations", "true"),
        ];
        if let Some(cursor) = cursor {
            extra.push(("cursor", cursor));
        }
        return collections::rest_endpoint(
            base,
            &format!("2/live_event/timeline/{id}.json"),
            &extra,
        );
    }
    if let Collection::Notifications = kind {
        // Upstream `notifications_devicefollow`: `count` is the `limit` option.
        let count = limit.to_string();
        let mut extra = vec![("count", count.as_str())];
        if let Some(cursor) = cursor {
            extra.push(("cursor", cursor));
        }
        return collections::rest_endpoint(base, "2/notifications/device_follow.json", &extra);
    }
    let mut variables = serde_json::json!({"count":limit});
    let operation = match kind {
        Collection::Event(_) | Collection::Notifications => unreachable!("REST timeline"),
        Collection::Bookmarks => {
            variables["includePromotedContent"] = false.into();
            "pLtjrO4ubNh996M_Cubwsg/Bookmarks"
        }
        Collection::List(id) => {
            if !valid_id(id) {
                return Err(error("Twitter list identifier invalid"));
            }
            variables["listId"] = id.as_str().into();
            "06JtmwM8k_1cthpFZITVVA/ListLatestTweetsTimeline"
        }
        Collection::Community(id) => {
            if !valid_id(id) {
                return Err(error("Twitter community identifier invalid"));
            }
            variables["communityId"] = id.as_str().into();
            variables["withCommunity"] = true.into();
            if text_tweets {
                variables["displayLocation"] = "Community".into();
                variables["rankingMode"] = "Recency".into();
                "Nyt-88UX4-pPCImZNUl9RQ/CommunityTweetsTimeline"
            } else {
                "ZniZ7AAK_VVu1xtSx1V-gQ/CommunityMediaTimeline"
            }
        }
        Collection::Communities => {
            variables["withCommunity"] = true.into();
            "p048a9n3hTPppQyK7FQTFw/CommunitiesMainPageTimeline"
        }
        Collection::HomeLatest | Collection::Home => {
            variables["includePromotedContent"] = false.into();
            variables["latestControlAvailable"] = true.into();
            if matches!(kind, Collection::Home) {
                variables["withCommunity"] = true.into();
                "DXmgQYmIft1oLP6vMkJixw/HomeTimeline"
            } else {
                "SFxmNKWfN9ySJcXG_tjX8g/HomeLatestTimeline"
            }
        }
    };
    if let Some(cursor) = cursor {
        variables["cursor"] = cursor.into();
    }
    let mut features = tweet_features();
    features["rweb_video_screen_enabled"] = false.into();
    query(base, operation, variables, features, Value::Null)
}

fn search_endpoint(base: &Url, paging: &search::Paging, options: TimelineOptions) -> Result<Url> {
    if !(1..=100).contains(&options.search_limit) || options.search_stop > 128 {
        return Err(error("Twitter search options exceed their limits"));
    }
    let mut variables = serde_json::json!({"rawQuery":paging.query,"count":options.search_limit,
        "querySource":"typed_query","product":options.search_product.name(),"withGrokTranslatedBio":false});
    // Upstream resets the opaque cursor when max_id/until rewrites the query.
    variables["cursor"] = paging
        .cursor
        .as_deref()
        .map(Value::from)
        .unwrap_or(Value::Null);
    let mut features = tweet_features();
    features["rweb_video_screen_enabled"] = false.into();
    query(
        base,
        "4fpceYZ6-YQCx_JSl_Cn_A/SearchTimeline",
        variables,
        features,
        Value::Null,
    )
}

// Accepted posts retain independent metadata even when they have no files.
// Keep every sibling from the first occurrence, suppress overlapping posts.
fn queue_unique(
    post: media::ParsedPost,
    pending: &mut VecDeque<ExtractionEvent>,
    seen: &mut HashSet<[u8; 32]>,
    unique: bool,
) -> bool {
    if unique {
        let digest = <[u8; 32]>::from(Sha256::digest(post.metadata.post_id.as_bytes()));
        if !seen.insert(digest) {
            return false;
        }
    }
    pending.push_back(ExtractionEvent::Post(Box::new(event::Post {
        keywords: event::KeywordPolicy::copied_after_post(&["num", "index", "media_id"])
            .without_post_fields(&["source_id", "source_user", "sensitive_flags"]),
        category: "twitter".into(),
        metadata: Some(post.metadata),
        count: Some(post.media.len()),
    })));
    pending.extend(
        post.media
            .into_iter()
            .map(|media| ExtractionEvent::Media(Box::new(media))),
    );
    pending.push_back(ExtractionEvent::EndPost);
    true
}

pub(super) fn query(
    base: &Url,
    path: &str,
    variables: Value,
    features: Value,
    toggles: Value,
) -> Result<Url> {
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
