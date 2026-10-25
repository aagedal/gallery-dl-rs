//! Experimental native Facebook photo, album, video and profile (`photos`,
//! `albums`, `avatar`, `info`) extraction.
pub use crate::facebook_video::{parse_video, video_id};
use crate::{
    Result,
    cookies::CookieJar,
    error,
    extract::{Media, PostMetadata, http_url},
    facebook_album::{
        AlbumInput, Grid, album_input, parse_grid, parse_grid_page, parse_navigation, parse_seed,
    },
    facebook_profile::{
        Tab, parse_albums, parse_avatar, parse_info, parse_photos_grid, parse_photos_set,
        profile_input,
    },
    network::NetworkOptions,
    transport::{RateLimits, Redirects, retry_after},
};
use futures_util::{
    StreamExt, TryStreamExt,
    stream::{self, BoxStream},
};
use reqwest::{Client, StatusCode, header};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    path::Path,
    sync::Mutex,
    time::{Duration, SystemTime},
};
use url::Url;

pub(crate) const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
const MAX_RESPONSE: usize = 16 * 1024 * 1024;

pub struct Facebook {
    client: Client,
    network: NetworkOptions,
    cookies: Mutex<CookieJar>,
    timeout: Duration,
    request_timeout: Duration,
    retries: u8,
    rate_limits: RateLimits,
}

impl Facebook {
    pub async fn new(cookies: Option<&Path>, timeout: Duration) -> Result<Self> {
        let cookies = match cookies {
            Some(path) => CookieJar::parse(&tokio::fs::read_to_string(path).await?)?,
            None => CookieJar::parse("")?,
        };
        Self::from_cookie_jar(cookies, timeout)
    }

    /// Build an extractor from an already scoped in-memory cookie jar.
    pub fn from_cookie_jar(cookies: CookieJar, timeout: Duration) -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .user_agent(USER_AGENT)
                .timeout(timeout)
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            network: NetworkOptions::default(),
            cookies: Mutex::new(cookies),
            timeout,
            request_timeout: timeout,
            retries: 0,
            rate_limits: RateLimits::default(),
        })
    }

    /// Apply proxy and user-agent settings to API requests and emitted media.
    pub fn with_network(mut self, network: &NetworkOptions) -> Result<Self> {
        self.client = network.client(USER_AGENT, self.timeout)?;
        self.network = network.clone();
        Ok(self)
    }

    /// Configure additional attempts for HTTP 429/5xx and transport failures.
    /// The extraction timeout covers all attempts and waits. Defaults to zero.
    pub fn with_retries(mut self, retries: u8) -> Self {
        self.retries = retries;
        self
    }

    /// Bound each HTTP request separately from the overall extraction deadline.
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    pub async fn extract(&self, post: &Url) -> Result<Vec<Media>> {
        if let Ok(id) = video_id(post) {
            let mut endpoint = http_url("https://www.facebook.com/watch/")?;
            endpoint.query_pairs_mut().append_pair("v", &id);
            return self.extract_video_endpoint(&endpoint, &id).await;
        }
        let code = photo_id(post)?;
        let mut endpoint = http_url("https://www.facebook.com/photo/")?;
        endpoint
            .query_pairs_mut()
            .append_pair("fbid", &code)
            .append_pair("set", "");
        self.extract_endpoint(&endpoint, &code).await
    }

    async fn extract_endpoint(&self, endpoint: &Url, code: &str) -> Result<Vec<Media>> {
        let body = self.request_body(endpoint, self.timeout).await?;
        let mut media = parse_photo(&body, code)?;
        for item in &mut media {
            self.network.apply_media(item);
        }
        Ok(media)
    }

    async fn extract_video_endpoint(&self, endpoint: &Url, id: &str) -> Result<Vec<Media>> {
        let body = self.request_body(endpoint, self.timeout).await?;
        let mut media = parse_video(&body, id)?;
        for item in &mut media {
            self.network.apply_media(item);
        }
        Ok(media)
    }

    async fn request_body(&self, endpoint: &Url, budget: Duration) -> Result<Vec<u8>> {
        tokio::time::timeout(budget, async {
            for attempt in 0..=self.retries {
                match self.attempt(endpoint, attempt).await {
                    Ok(body) => return Ok(body),
                    Err(failure) if failure.retry && attempt < self.retries => {
                        tokio::time::sleep(backoff(attempt)).await;
                    }
                    Err(failure) => return Err(failure.error),
                }
            }
            unreachable!()
        })
        .await
        .map_err(|_| error("native Facebook extraction timed out"))?
    }

    /// Lazily extract media while preserving the existing media-only API.
    pub fn stream(&self, url: &Url) -> BoxStream<'_, Result<Media>> {
        crate::event::media_only(self.events(url))
    }

    /// Emit an explicit post boundary before each photo or video and its files.
    /// Album navigation preserves one photo group per page without prefetching.
    pub fn events(&self, url: &Url) -> BoxStream<'_, Result<crate::event::ExtractionEvent>> {
        if is_share_link(url) {
            let url = url.clone();
            return stream::once(async move {
                let target = self.resolve_share(&url).await?;
                Ok::<_, crate::Error>(self.events(&target))
            })
            .try_flatten()
            .boxed();
        }
        if let Ok(id) = video_id(url) {
            let mut endpoint = Url::parse("https://www.facebook.com/watch/").unwrap();
            endpoint.query_pairs_mut().append_pair("v", &id);
            return self.post_events_endpoint(endpoint, id, true);
        }
        if let Ok(id) = photo_id(url) {
            let mut endpoint = Url::parse("https://www.facebook.com/photo/").unwrap();
            endpoint
                .query_pairs_mut()
                .append_pair("fbid", &id)
                .append_pair("set", "");
            return self.post_events_endpoint(endpoint, id, false);
        }
        if crate::facebook_profile::is_post_url(url) {
            let url = url.clone();
            return stream::once(async move {
                let mut page = url.clone();
                page.set_host(Some("www.facebook.com")).ok();
                let body = self.request_body(&page, self.timeout).await?;
                let (photos, video) = crate::facebook_profile::parse_post(&body)?;
                if photos.is_empty()
                    && let Some(id) = video
                {
                    let mut endpoint = Url::parse("https://www.facebook.com/watch/").unwrap();
                    endpoint.query_pairs_mut().append_pair("v", &id);
                    return Ok::<_, crate::Error>(self.post_events_endpoint(endpoint, id, true));
                }
                Ok(stream::iter(photos)
                    .map(move |id| {
                        let mut endpoint = Url::parse("https://www.facebook.com/photo/").unwrap();
                        endpoint
                            .query_pairs_mut()
                            .append_pair("fbid", &id)
                            .append_pair("set", "");
                        self.post_events_endpoint(endpoint, id, false)
                    })
                    .flatten()
                    .boxed())
            })
            .try_flatten()
            .boxed();
        }
        if let Some(profile) = profile_input(url) {
            return self.profile_events(profile);
        }
        match album_input(url) {
            Ok(input) => self.album_events(input, Url::parse("https://www.facebook.com/").unwrap()),
            Err(e) => stream::once(async move { Err(e) }).boxed(),
        }
    }

    /// The post, reel or video a mobile share link (`/share/p|r|v/TOKEN`)
    /// stands for: its redirect target, else the page's canonical URL.
    async fn resolve_share(&self, url: &Url) -> Result<Url> {
        let mut request = self
            .client
            .get(url.clone())
            .timeout(self.request_timeout)
            .header(header::ACCEPT, "text/html,application/xhtml+xml")
            .header(header::ACCEPT_LANGUAGE, "en-US,en;q=0.5")
            .header("Sec-Fetch-Mode", "navigate")
            .header("Sec-Fetch-Site", "none");
        if let Some(cookies) = self.cookies.lock().unwrap().header(url) {
            request = request.header(header::COOKIE, cookies);
        }
        let response = tokio::time::timeout(self.timeout, request.send())
            .await
            .map_err(|_| error("native Facebook extraction timed out"))?
            .map_err(|_| error("Facebook request failed; check network access"))?;
        self.cookies
            .lock()
            .unwrap()
            .store_response(response.url(), response.headers());
        let unresolved = || {
            error("Facebook share link did not resolve to a supported post; login may be required")
        };
        let target = if response.status().is_redirection() {
            response
                .headers()
                .get(header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| url.join(v).ok())
        } else if response.status().is_success() {
            let body = crate::transport::read_capped(response, MAX_RESPONSE)
                .await
                .ok_or_else(unresolved)?;
            let page = String::from_utf8_lossy(&body);
            crate::sites::extr(&page, r#"<link rel="canonical" href=""#, "\"")
                .or_else(|| crate::sites::extr(&page, r#"<meta property="og:url" content=""#, "\""))
                .map(|raw| raw.replace("&amp;", "&"))
                .and_then(|raw| Url::parse(&raw).ok())
        } else {
            None
        };
        target.filter(share_target).ok_or_else(unresolved)
    }

    /// Profile routes: the profile page names the photos set, the albums or
    /// the avatar photo, which are then read like album and photo URLs.
    fn profile_events(
        &self,
        input: crate::facebook_profile::ProfileInput,
    ) -> BoxStream<'_, Result<crate::event::ExtractionEvent>> {
        match input.page() {
            Ok(page) => self.profile_events_at(input, page),
            Err(e) => stream::once(async move { Err(e) }).boxed(),
        }
    }

    /// [`Self::profile_events`] reading the profile page from `page`.
    fn profile_events_at(
        &self,
        input: crate::facebook_profile::ProfileInput,
        page: Url,
    ) -> BoxStream<'_, Result<crate::event::ExtractionEvent>> {
        stream::once(async move {
            let base = Url::parse("https://www.facebook.com/").unwrap();
            let body = self.request_body(&page, self.timeout).await?;
            let events = match &input.tab {
                Tab::Photos => match parse_photos_set(&body)? {
                    Some(set) => {
                        let grid = parse_photos_grid(&body);
                        let start = grid.ids.first().cloned();
                        self.album_events_from(AlbumInput { set, start }, base, grid)
                    }
                    None => stream::empty().boxed(),
                },
                Tab::Avatar => {
                    let id = photo_id(&parse_avatar(&body)?)?;
                    let mut endpoint = base.join("photo/")?;
                    endpoint
                        .query_pairs_mut()
                        .append_pair("fbid", &id)
                        .append_pair("set", "");
                    self.post_events_endpoint(endpoint, id, false)
                }
                Tab::Info => {
                    let user = parse_info(&body)?;
                    let text = |key: &str| {
                        user.get(key)
                            .and_then(Value::as_str)
                            .filter(|value| !value.is_empty())
                            .map(std::sync::Arc::from)
                    };
                    let metadata = PostMetadata {
                        post_id: user
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        username: text("username"),
                        caption: text("biography"),
                        extra_keywords: Some(std::sync::Arc::new(user)),
                        ..PostMetadata::default()
                    };
                    stream::iter([
                        Ok(crate::event::ExtractionEvent::Post(Box::new(
                            crate::event::Post {
                                keywords: Default::default(),
                                category: "facebook".into(),
                                count: Some(0),
                                metadata: Some(metadata),
                            },
                        ))),
                        Ok(crate::event::ExtractionEvent::EndPost),
                    ])
                    .boxed()
                }
                Tab::Albums(name) => {
                    let list = parse_albums(&body, name.as_deref())?;
                    let referer = input.page()?;
                    self.album_list_events(list, name.clone(), base, referer, HashSet::new())
                }
            };
            Ok::<_, crate::Error>(events)
        })
        .try_flatten()
        .boxed()
    }

    fn post_events_endpoint(
        &self,
        endpoint: Url,
        id: String,
        video: bool,
    ) -> BoxStream<'_, Result<crate::event::ExtractionEvent>> {
        stream::once(async move {
            let media = if video {
                self.extract_video_endpoint(&endpoint, &id).await?
            } else {
                self.extract_endpoint(&endpoint, &id).await?
            };
            Ok::<_, crate::Error>(crate::event::group(media, "facebook"))
        })
        .map_ok(|events| stream::iter(events.into_iter().map(Ok)))
        .try_flatten()
        .boxed()
    }

    #[cfg(test)]
    fn album_stream(
        &self,
        input: crate::facebook_album::AlbumInput,
        base: Url,
    ) -> BoxStream<'_, Result<Media>> {
        crate::event::media_only(self.album_events(input, base))
    }

    fn album_events(
        &self,
        input: crate::facebook_album::AlbumInput,
        base: Url,
    ) -> BoxStream<'_, Result<crate::event::ExtractionEvent>> {
        self.album_events_from(input, base, Grid::default())
    }

    /// Album traversal with a known first grid page, which is followed when
    /// photo pages carry no navigation.
    fn album_events_from(
        &self,
        input: crate::facebook_album::AlbumInput,
        base: Url,
        grid: Grid,
    ) -> BoxStream<'_, Result<crate::event::ExtractionEvent>> {
        let state = AlbumState {
            base,
            set: input.set,
            next: input.start,
            first: None,
            seen: HashSet::new(),
            index: 0,
            pending: VecDeque::new(),
            done: false,
            budget: self.timeout,
            grid,
            truncated: false,
        };
        stream::unfold(state, move |mut state| async move {
            loop {
                if let Some(event) = state.pending.pop_front() {
                    return Some((Ok(event), state));
                }
                if state.done {
                    if std::mem::take(&mut state.truncated) {
                        return Some((
                            Err(error(
                                "Facebook album continues beyond the photos visible without \
                                 login; pass --cookies for the rest",
                            )),
                            state,
                        ));
                    }
                    return None;
                }
                let started = tokio::time::Instant::now();
                let result = tokio::time::timeout(state.budget, self.album_photo(&mut state)).await;
                state.budget = state.budget.saturating_sub(started.elapsed());
                match result.unwrap_or_else(|_| Err(error("native Facebook extraction timed out")))
                {
                    Ok(media) => state.pending = crate::event::group(vec![media], "facebook"),
                    Err(error) => {
                        state.done = true;
                        return Some((Err(error), state));
                    }
                }
            }
        })
        .boxed()
    }

    async fn album_photo(&self, state: &mut AlbumState) -> Result<Media> {
        if state.next.is_none() {
            let mut endpoint = state.base.join("media/set/")?;
            endpoint.query_pairs_mut().append_pair("set", &state.set);
            let body = self.request_body(&endpoint, state.budget).await?;
            state.next = Some(parse_seed(&body, &state.set)?);
            state.grid = parse_grid(&body, &state.set);
        }
        let id = state.next.as_deref().unwrap();
        if !state.seen.insert(id.to_owned()) {
            return Err(error("Facebook album navigation repeated a photo"));
        }
        if state.first.is_none() {
            state.first = Some(id.to_owned());
        }
        let mut endpoint = state.base.join("photo/")?;
        endpoint
            .query_pairs_mut()
            .append_pair("fbid", id)
            .append_pair("set", &state.set);
        let body = self.request_body(&endpoint, state.budget).await?;
        let mut media = parse_photo(&body, id)?;
        let next = match parse_navigation(&body, id, &state.set) {
            Ok(next) => next,
            // Without navigation (anonymous sessions), follow the grid.
            Err(e) => match state.grid.ids.iter().position(|grid| grid == id) {
                Some(pos) => {
                    let mut next = state.grid.ids.get(pos + 1).cloned();
                    if next.is_none() && state.grid.more {
                        next = self.next_grid_page(state).await;
                    }
                    state.truncated = next.is_none() && state.grid.more;
                    next
                }
                None => return Err(e),
            },
        };
        match next.as_deref() {
            None => state.done = true,
            Some(next) if Some(next) == state.first.as_deref() => state.done = true,
            Some(next) if state.seen.contains(next) => {
                return Err(error("Facebook album navigation looped before completion"));
            }
            Some(_) => {}
        }
        state.next = next;
        state.index = state
            .index
            .checked_add(1)
            .ok_or_else(|| error("Facebook album index overflow"))?;
        let mut item = media
            .pop()
            .ok_or_else(|| error("Facebook album photo has no media"))?;
        if let Some(metadata) = &mut item.metadata {
            metadata.index = state.index;
        }
        self.network.apply_media(&mut item);
        Ok(item)
    }

    /// A profile's albums, continuing the album list through
    /// `ProfileCometAppCollectionAlbumsRendererPaginationQuery`.
    fn album_list_events(
        &self,
        list: crate::facebook_profile::AlbumList,
        name: Option<String>,
        base: Url,
        referer: Url,
        mut cursors: HashSet<String>,
    ) -> BoxStream<'_, Result<crate::event::ExtractionEvent>> {
        let crate::facebook_profile::AlbumList {
            albums,
            more,
            cursor,
            collection,
            lsd,
        } = list;
        let base2 = base.clone();
        let current = stream::iter(albums)
            .map(move |url| match album_input(&url) {
                Ok(input) => self.album_events(input, base2.clone()),
                Err(e) => stream::once(async move { Err(e) }).boxed(),
            })
            .flatten();
        let rest = stream::once(async move {
            let (Some(cursor), Some(collection), Some(lsd)) = (cursor, collection, lsd) else {
                return stream::empty().boxed();
            };
            // Each cursor is followed once, and the list at most 500 pages.
            if !more || !cursors.insert(cursor.clone()) || cursors.len() > 500 {
                return stream::empty().boxed();
            }
            let variables = serde_json::json!({
                "count": 8, "cursor": cursor, "scale": 2, "id": collection,
            });
            let next = self
                .graphql(
                    &base,
                    &referer,
                    &lsd,
                    (
                        "ProfileCometAppCollectionAlbumsRendererPaginationQuery",
                        "27733032642961647",
                    ),
                    &variables,
                )
                .await
                .and_then(|body| crate::facebook_profile::parse_album_page(&body, name.as_deref()));
            match next {
                // Each page continues only with a new cursor.
                Some(mut next) if next.cursor.as_ref().is_some_and(|c| c != &cursor) => {
                    next.collection = Some(collection);
                    next.lsd = Some(lsd);
                    self.album_list_events(next, name, base, referer, cursors)
                }
                Some(next) => self.album_list_events(
                    crate::facebook_profile::AlbumList {
                        more: false,
                        ..next
                    },
                    name,
                    base,
                    referer,
                    cursors,
                ),
                None => stream::once(async {
                    Err(error(
                        "Facebook profile has more albums than are visible without login; \
                         pass --cookies for the rest",
                    ))
                })
                .boxed(),
            }
        })
        .flatten();
        current.chain(rest).boxed()
    }

    /// A logged-out GraphQL request with the page's LSD token; `None` when
    /// Facebook refuses it.
    async fn graphql(
        &self,
        base: &Url,
        referer: &Url,
        lsd: &str,
        (name, doc_id): (&str, &str),
        variables: &Value,
    ) -> Option<Vec<u8>> {
        let endpoint = base.join("api/graphql/").ok()?;
        let variables = variables.to_string();
        let form = [
            ("lsd", lsd),
            ("variables", variables.as_str()),
            ("doc_id", doc_id),
            ("fb_api_req_friendly_name", name),
            ("server_timestamps", "true"),
        ];
        self.rate_limits.wait(&endpoint, self.timeout).await.ok()?;
        let mut request = self
            .client
            .post(endpoint.clone())
            .timeout(self.request_timeout)
            .header(header::REFERER, referer.as_str())
            .header(header::ORIGIN, "https://www.facebook.com")
            .header(header::ACCEPT_LANGUAGE, "en-US,en;q=0.5")
            .header("X-FB-LSD", lsd)
            .header("X-FB-Friendly-Name", name)
            .header("Sec-Fetch-Site", "same-origin")
            .form(&form);
        if let Some(cookies) = self.cookies.lock().unwrap().header(&endpoint) {
            request = request.header(header::COOKIE, cookies);
        }
        let response = request.send().await.ok()?;
        if !response.status().is_success() {
            return None;
        }
        crate::transport::read_capped(response, MAX_RESPONSE).await
    }

    /// Extend an exhausted album grid through the album's pagination query,
    /// returning the first new photo. Without a usable page (for example
    /// after Facebook rotated the query ID) the grid stays truncated.
    async fn next_grid_page(&self, state: &mut AlbumState) -> Option<String> {
        let grid = &state.grid;
        let (cursor, node, lsd) = (grid.cursor.clone()?, grid.node.clone()?, grid.lsd.clone()?);
        let variables = serde_json::json!({"count": 8, "cursor": cursor, "scale": 2, "id": node});
        let mut referer = state.base.join("media/set/").ok()?;
        referer.query_pairs_mut().append_pair("set", &state.set);
        let query = grid.query;
        let body = self
            .graphql(&state.base, &referer, &lsd, query.operation(), &variables)
            .await?;
        let page = parse_grid_page(&body, query)?;
        let new: Vec<String> = page
            .ids
            .into_iter()
            .filter(|id| !state.seen.contains(id) && !state.grid.ids.contains(id))
            .collect();
        state.grid.more = page.more && page.cursor.is_some();
        state.grid.cursor = page.cursor;
        // A page without new photos ends traversal; `more` then marks the
        // album as truncated.
        let first = new.first().cloned();
        state.grid.ids.extend(new);
        first
    }

    async fn attempt(&self, endpoint: &Url, attempt: u8) -> std::result::Result<Vec<u8>, Failure> {
        let mut target = endpoint.clone();
        let mut redirects = Redirects::new(endpoint);
        let mut response = loop {
            self.rate_limits.wait(&target, self.timeout).await?;
            let mut request = self
                .client
                .get(target.clone())
                .timeout(self.request_timeout)
                .header(
                    header::ACCEPT,
                    "text/html,application/xhtml+xml,application/xml;q=0.9,\
                     image/avif,image/webp,image/png,image/svg+xml,*/*;q=0.8",
                )
                .header(header::REFERER, "https://www.facebook.com/")
                // gallery-dl's default; album titles are otherwise localized.
                .header(header::ACCEPT_LANGUAGE, "en-US,en;q=0.5")
                // Upstream's session headers; album pages answer HTTP 400
                // to requests that do not look like navigations.
                .header("Sec-Fetch-Dest", "empty")
                .header("Sec-Fetch-Mode", "navigate")
                .header("Sec-Fetch-Site", "same-origin");
            if let Some(cookies) = self.cookies.lock().unwrap().header(&target) {
                request = request.header(header::COOKIE, cookies);
            }
            let response = request
                .send()
                .await
                .map_err(|_| Failure::transient("Facebook request failed; check network access"))?;
            self.cookies
                .lock()
                .unwrap()
                .store_response(response.url(), response.headers());
            match redirects.next(response.url(), response.status(), response.headers())? {
                Some(next) => {
                    if matches!(
                        next.path().trim_end_matches('/'),
                        "/login" | "/login.php" | "/checkpoint" | "/recover"
                    ) {
                        return Err(error("Facebook authentication required or session challenge; refresh --cookies").into());
                    }
                    if !matches!(
                        next.path().trim_end_matches('/'),
                        "/photo" | "/photo.php" | "/media/set" | "/watch"
                    ) && video_id(&next).is_err()
                        && profile_input(&next).is_none()
                        && !crate::facebook_profile::is_post_url(&next)
                    {
                        return Err(error(
                            "Facebook redirected outside supported media endpoints; login may be required",
                        )
                        .into());
                    }
                    target = next;
                }
                None => break response,
            }
        };
        let status = response.status();
        if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
            let delay = retry_after(response.headers(), SystemTime::now());
            // Keep final-attempt cooldowns for subsequent posts on this instance.
            self.rate_limits
                .defer(endpoint, delay.unwrap_or_else(|| backoff(attempt)));
            return Err(Failure::transient(
                if status == StatusCode::TOO_MANY_REQUESTS {
                    match delay {
                        Some(delay) => format!(
                            "Facebook rate limit; retry after {} seconds",
                            delay.as_secs()
                        ),
                        None => "Facebook rate limit; retry later".into(),
                    }
                } else {
                    format!("Facebook returned HTTP {status}")
                },
            ));
        }
        if matches!(status.as_u16(), 401 | 403) {
            return Err(error(
                "Facebook authentication required or session expired; refresh --cookies",
            )
            .into());
        }
        if matches!(status.as_u16(), 404 | 410) {
            return Err(error("Facebook post unavailable or inaccessible to this session").into());
        }
        if !status.is_success() {
            return Err(error(format!("Facebook returned HTTP {status}")).into());
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| Failure::transient("could not read Facebook response"))?
        {
            if body.len() + chunk.len() > MAX_RESPONSE {
                return Err(error("Facebook response exceeded 16 MiB").into());
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }
}

struct AlbumState {
    base: Url,
    set: String,
    next: Option<String>,
    first: Option<String>,
    seen: HashSet<String>,
    index: usize,
    pending: VecDeque<crate::event::ExtractionEvent>,
    done: bool,
    budget: Duration,
    /// The album page's first grid page, used when photos lack navigation.
    grid: Grid,
    /// The grid ended before the album did.
    truncated: bool,
}

/// `facebook.com/share/p|r|v/TOKEN` (and `/share/TOKEN`) mobile share links.
pub(crate) fn is_share_link(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https")
        && matches!(
            url.host_str(),
            Some("facebook.com" | "www.facebook.com" | "m.facebook.com" | "web.facebook.com")
        )
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none()
        && url.path().starts_with("/share/")
}

/// A share link may only resolve to a URL this extractor reads, never to
/// another share link (or a login page).
fn share_target(url: &Url) -> bool {
    !is_share_link(url)
        && (video_id(url).is_ok()
            || photo_id(url).is_ok()
            || album_input(url).is_ok()
            || profile_input(url).is_some()
            || crate::facebook_profile::is_post_url(url))
}

fn backoff(attempt: u8) -> Duration {
    Duration::from_millis(250 * (1u64 << attempt.min(6)))
}

struct Failure {
    error: crate::Error,
    retry: bool,
}

impl Failure {
    fn transient(message: impl Into<String>) -> Self {
        Self {
            error: error(message),
            retry: true,
        }
    }
}

impl From<crate::Error> for Failure {
    fn from(error: crate::Error) -> Self {
        Self {
            error,
            retry: false,
        }
    }
}

/// Parse single-photo permalinks. Album inputs are handled by `Facebook::stream`.
pub fn photo_id(url: &Url) -> Result<String> {
    if !matches!(url.scheme(), "http" | "https")
        || !matches!(
            url.host_str(),
            Some("facebook.com" | "www.facebook.com" | "m.facebook.com")
        )
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return Err(error("native extraction requires a Facebook photo URL"));
    }
    if url.query_pairs().any(|(key, _)| key == "setextract") {
        return Err(error(
            "Facebook URL selects album traversal; use Facebook::stream or --extractor compatibility",
        ));
    }
    let path = url.path().trim_end_matches('/');
    let id = if matches!(path, "/photo" | "/photo.php") {
        let ids: Vec<_> = url
            .query_pairs()
            .filter(|(key, _)| key == "fbid")
            .map(|(_, value)| value.into_owned())
            .collect();
        if ids.len() != 1 {
            return Err(error("Facebook photo URL requires exactly one fbid"));
        }
        ids[0].clone()
    } else {
        let parts: Vec<_> = path.split('/').collect();
        if parts.len() != 5 || parts[1].is_empty() || parts[2] != "photos" || parts[3].is_empty() {
            return Err(error(
                "native Facebook supports single photo permalinks; use --extractor compatibility for other URLs",
            ));
        }
        parts[4].into()
    };
    validate_id(&id)?;
    Ok(id)
}

fn validate_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 32 || !id.bytes().all(|c| c.is_ascii_digit()) {
        return Err(error("invalid Facebook photo identifier"));
    }
    Ok(())
}

/// Parse JSON script payloads, never a page-wide image/OG preview match.
/// A photo must identify the exact requested ID and own its `image.uri`.
pub fn parse_photo(bytes: &[u8], requested_id: &str) -> Result<Vec<Media>> {
    validate_id(requested_id)?;
    let payloads = embedded_payloads(bytes)?;
    let page = std::str::from_utf8(bytes).map_err(|_| error("Facebook returned invalid UTF-8"))?;
    let mut photos = Vec::new();
    for payload in &payloads {
        find_photos(payload, requested_id, &mut photos, 0);
    }
    let mut candidates = Vec::new();
    for photo in photos {
        let Some(uri) = photo.pointer("/image/uri").and_then(Value::as_str) else {
            continue;
        };
        let url = http_url(uri).map_err(|_| error("Facebook photo has an invalid image URL"))?;
        let extension = match url
            .path()
            .rsplit('.')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str()
        {
            "jpg" | "jpeg" => "jpg",
            "png" => "png",
            "webp" => "webp",
            "avif" => "avif",
            _ => {
                return Err(error(
                    "Facebook photo image has no supported file extension",
                ));
            }
        };
        let area = u128::from(
            photo
                .pointer("/image/width")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        ) * u128::from(
            photo
                .pointer("/image/height")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        );
        let media = Media {
            url,
            job: crate::extract::MediaJob::DirectHttp,
            category: "facebook".into(),
            filename: format!("{requested_id}.{extension}"),
            handoff: BTreeMap::new(),
            metadata: Some(PostMetadata {
                source_num: None,
                booru_keywords: None,
                source_defaults: None,
                booru_user_id: None,
                booru_pool_name: None,
                booru_favorite_id: None,
                booru_artist_parent: None,
                booru_popular: None,
                source_suffix: None,
                pool: None,
                favgroup: None,
                search_tags: None,
                keyword_updates: None,
                tags: None,
                sketch: None,
                sketch_archive: false,
                user_bookmark: None,
                bookmark_archive: false,
                ajax: None,
                mypixiv: None,
                pixiv_novel_metadata: None,
                caption_html: None,
                pixiv_user_detail: None,
                comments: None,
                tags_bookmark: None,
                ugoira_original: None,
                ugoira_frame_index: None,
                pixivision: None,
                pixivision_archive: false,
                user_follow: None,
                followed_archive: false,
                series: None,
                num_series: None,
                search: None,
                search_archive: false,
                ranking: None,
                ranking_archive: false,
                profile_asset: None,
                file: Some(crate::file_metadata::FileMetadata {
                    width: photo
                        .pointer("/image/width")
                        .and_then(Value::as_u64)
                        .filter(|value| *value > 0),
                    height: photo
                        .pointer("/image/height")
                        .and_then(Value::as_u64)
                        .filter(|value| *value > 0),
                    media_type: Some(std::sync::Arc::from("photo")),
                    description: photo
                        .get("accessibility_caption")
                        .and_then(Value::as_str)
                        .map(std::sync::Arc::from),
                    ..Default::default()
                }),
                frames: None,
                rating: None,
                score: None,
                revision: None,
                extra_keywords: None,
                discord: None,
                creator: None,
                community: None,
                community_archive: false,
                retweet_id: None,
                quoted_by_id: None,
                reposted_by_id: None,
                article: None,
                instance: None,
                site_keywords: None,
                post_id: requested_id.into(),
                media_id: format!("{requested_id}.{extension}"),
                shortcode: String::new(),
                username: photo
                    .pointer("/owner/username")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(std::sync::Arc::from),
                caption: photo
                    .pointer("/creation_story/message/text")
                    .or_else(|| photo.pointer("/message/text"))
                    .and_then(Value::as_str)
                    .map(std::sync::Arc::from),
                timestamp: photo.get("created_time").and_then(Value::as_i64),
                index: 1,
            }),
            headers: BTreeMap::from([
                ("User-Agent".into(), USER_AGENT.into()),
                ("Referer".into(), "https://www.facebook.com/".into()),
            ]),
        };
        candidates.push((area, media));
    }
    candidates.sort_by_key(|(area, _)| std::cmp::Reverse(*area));
    let Some((area, chosen)) = candidates.first() else {
        if payloads.iter().any(has_error_root) {
            return Err(error(
                "Facebook temporarily blocked photo access; retry later",
            ));
        }
        if page.contains("login_form")
            || page.contains("login.php?next=")
            || page.contains("checkpoint_url")
        {
            return Err(error(
                "Facebook authentication required or session challenge; refresh --cookies",
            ));
        }
        return Err(error(
            "Facebook requested photo unavailable or missing identity-bound media; login may be required",
        ));
    };
    if candidates
        .iter()
        .skip(1)
        .any(|(other_area, other)| other_area == area && other.url != chosen.url)
    {
        return Err(error(
            "Facebook returned ambiguous images for the requested photo",
        ));
    }
    Ok(vec![chosen.clone()])
}

pub(crate) fn embedded_payloads(bytes: &[u8]) -> Result<Vec<Value>> {
    if bytes.len() > MAX_RESPONSE {
        return Err(error("Facebook response exceeded 16 MiB"));
    }
    let page = std::str::from_utf8(bytes).map_err(|_| error("Facebook returned invalid UTF-8"))?;
    let mut payloads = Vec::new();
    if let Ok(value) = serde_json::from_str::<Value>(page) {
        payloads.push(value);
    } else {
        // Script contents are raw text in HTML. JSON escapes (including slash
        // and surrogate pairs) are decoded exactly once by serde_json.
        let lower = page.to_ascii_lowercase();
        let mut offset = 0;
        while let Some(start) = lower[offset..].find("<script") {
            let start = offset + start + 7;
            if !lower
                .as_bytes()
                .get(start)
                .is_some_and(|b| b.is_ascii_whitespace() || *b == b'>')
            {
                offset = start;
                continue;
            }
            let Some(open) = lower[start..].find('>') else {
                break;
            };
            let content = start + open + 1;
            let Some(end) = lower[content..].find("</script") else {
                break;
            };
            let end = content + end;
            if let Ok(value) = serde_json::from_str::<Value>(page[content..end].trim()) {
                payloads.push(value);
            }
            offset = end + 8;
        }
    }
    Ok(payloads)
}

pub(crate) fn has_error_root(value: &Value) -> bool {
    match value {
        Value::Object(object) => {
            object.get("__dr").and_then(Value::as_str) == Some("CometErrorRoot.react")
                || object.values().any(has_error_root)
        }
        Value::Array(array) => array.iter().any(has_error_root),
        _ => false,
    }
}

fn find_photos<'a>(value: &'a Value, id: &str, photos: &mut Vec<&'a Value>, depth: usize) {
    if depth > 100 {
        return;
    }
    match value {
        Value::Object(object) => {
            if (object.get("__isNode").and_then(Value::as_str) == Some("Photo")
                || object.get("__typename").and_then(Value::as_str) == Some("Photo"))
                && object.get("id").and_then(Value::as_str) == Some(id)
            {
                photos.push(value);
            }
            for child in object.values() {
                find_photos(child, id, photos, depth + 1);
            }
        }
        Value::Array(array) => {
            for child in array {
                find_photos(child, id, photos, depth + 1);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod transport_tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread::{self, JoinHandle},
    };

    const PHOTO: &[u8] = include_bytes!("../tests/fixtures/facebook/photo.html");
    const VIDEO: &[u8] = include_bytes!("../tests/fixtures/facebook/video.json");

    #[tokio::test]
    async fn native_post_events_group_video_audio_once_and_preserve_photo_media() {
        use crate::event::ExtractionEvent;
        use futures_util::StreamExt;
        for (body, id, video, count) in [
            (VIDEO, "123456789", true, 2),
            (PHOTO, "10160743390456729", false, 1),
        ] {
            let (endpoint, server) = serve_once("200 OK", "", body, Duration::ZERO);
            let native = Facebook::new(None, Duration::from_secs(3)).await.unwrap();
            let mut events = native.post_events_endpoint(endpoint, id.into(), video);
            let ExtractionEvent::Post(post) = events.next().await.unwrap().unwrap() else {
                panic!("native post boundary missing");
            };
            assert_eq!(post.category, "facebook");
            assert_eq!(post.count, Some(count));
            let metadata = post.metadata.unwrap();
            assert_eq!(metadata.post_id, id);
            assert_eq!(metadata.media_id, "");
            assert_eq!(metadata.index, 0);
            assert!(metadata.file.is_none());
            let mut media = Vec::new();
            for _ in 0..count {
                let ExtractionEvent::Media(item) = events.next().await.unwrap().unwrap() else {
                    panic!("native post media missing");
                };
                assert_eq!(item.metadata.as_ref().unwrap().post_id, id);
                assert!(item.metadata.as_ref().unwrap().file.is_some());
                media.push(item);
            }
            assert!(matches!(
                events.next().await.unwrap().unwrap(),
                ExtractionEvent::EndPost
            ));
            assert!(events.next().await.is_none());
            let expected = if video {
                parse_video(body, id)
            } else {
                parse_photo(body, id)
            }
            .unwrap();
            assert_eq!(
                media
                    .iter()
                    .map(|item| (&item.url, &item.filename, item.archive_key()))
                    .collect::<Vec<_>>(),
                expected
                    .iter()
                    .map(|item| (&item.url, &item.filename, item.archive_key()))
                    .collect::<Vec<_>>()
            );
            assert!(server.join().unwrap().starts_with("GET "));
        }
    }

    #[tokio::test]
    async fn profile_info_emits_one_fileless_post_with_upstream_user_keywords() {
        use crate::event::ExtractionEvent;
        use futures_util::StreamExt;
        let page = br#"<script>{"userVanity":"NASA","userID":"100044561550831","x":{"__typename":"User","user":{"id":"pfbid0abc","name":"NASA","profile_tabs":{"profile_user":{"timeline_nav_app_sections":{"edges":[]}}}},"viewer":{}},"best_description":{"text":"Explore"}}</script>"#;
        let (endpoint, server) = serve_once("200 OK", "", page, Duration::ZERO);
        let native = Facebook::new(None, Duration::from_secs(3)).await.unwrap();
        let input = crate::facebook_profile::ProfileInput {
            profile: "NASA".into(),
            tab: Tab::Info,
        };
        let events = native
            .profile_events_at(input, endpoint)
            .map(Result::unwrap)
            .collect::<Vec<_>>()
            .await;
        assert_eq!(events.len(), 2);
        let ExtractionEvent::Post(post) = &events[0] else {
            panic!("info post missing");
        };
        assert!(matches!(events[1], ExtractionEvent::EndPost));
        assert_eq!((post.category.as_str(), post.count), ("facebook", Some(0)));
        let metadata = post.metadata.as_ref().unwrap();
        assert_eq!(metadata.post_id, "100044561550831");
        assert_eq!(metadata.username.as_deref(), Some("NASA"));
        assert_eq!(metadata.caption.as_deref(), Some("Explore"));
        let user = metadata.extra_keywords.as_ref().unwrap();
        assert_eq!(
            (&user["user_pfbid"], &user["set_id"], &user["profile_tabs"]),
            (
                &Value::from("pfbid0abc"),
                &Value::from(""),
                &serde_json::json!([])
            )
        );
        assert!(server.join().unwrap().starts_with("GET /photo/ "));
    }

    #[tokio::test]
    async fn video_requests_retry_rotate_cookies_and_apply_network_identity_to_both_files() {
        let (mut endpoint, server) = serve_sequence(vec![
            response(
                "503 Service Unavailable",
                "Retry-After: 0\r\nSet-Cookie: session=rotated; Path=/\r\n",
                b"",
            ),
            response(
                "302 Found",
                "Location: /watch/?v=123456789&ref=redirect\r\n",
                b"",
            ),
            response("200 OK", "", VIDEO),
        ]);
        endpoint.set_path("/watch/");
        endpoint.set_query(Some("v=123456789"));
        let network = NetworkOptions::new(None, Some("native-video-test/1".into())).unwrap();
        let mut native = Facebook::new(None, Duration::from_secs(3))
            .await
            .unwrap()
            .with_retries(1)
            .with_network(&network)
            .unwrap();
        native.cookies = CookieJar::parse("127.0.0.1\tFALSE\t/\tFALSE\t0\tsession\tinitial\n")
            .unwrap()
            .into();
        let media = native
            .extract_video_endpoint(&endpoint, "123456789")
            .await
            .unwrap();
        assert_eq!(media.len(), 2);
        for item in media {
            assert_eq!(
                item.headers.get("User-Agent").map(String::as_str),
                Some("native-video-test/1")
            );
            assert!(
                !item
                    .headers
                    .keys()
                    .any(|key| key.eq_ignore_ascii_case("cookie"))
            );
        }
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[0].2.contains("session=initial"));
        for (_, _, request) in &requests[1..] {
            assert!(request.contains("session=rotated"));
        }
    }

    #[tokio::test]
    async fn video_parser_and_authentication_failures_are_terminal() {
        for (status, body) in [
            ("200 OK", &b"{\"data\":{}}"[..]),
            ("403 Forbidden", &b""[..]),
        ] {
            let (endpoint, server) = serve_sequence(vec![response(status, "", body)]);
            let native = Facebook::new(None, Duration::from_secs(3))
                .await
                .unwrap()
                .with_retries(2);
            assert!(
                native
                    .extract_video_endpoint(&endpoint, "123456789")
                    .await
                    .is_err()
            );
            assert_eq!(server.join().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn request_timeout_is_separate_from_the_overall_extraction_deadline() {
        let (endpoint, server) = serve_once("200 OK", "", PHOTO, Duration::from_millis(200));
        let native = Facebook::new(None, Duration::from_secs(3))
            .await
            .unwrap()
            .with_request_timeout(Duration::from_millis(50));
        let message = native
            .extract_endpoint(&endpoint, "10160743390456729")
            .await
            .unwrap_err()
            .to_string();
        assert!(message.contains("Facebook request failed"), "{message}");
        server.join().unwrap();
    }

    #[tokio::test]
    async fn configured_user_agent_matches_api_and_media_requests() {
        let (endpoint, server) = serve_once("200 OK", "", PHOTO, Duration::ZERO);
        let network = NetworkOptions::new(None, Some("gallery-dl-native-test/1".into())).unwrap();
        let native = Facebook::new(None, Duration::from_secs(3))
            .await
            .unwrap()
            .with_network(&network)
            .unwrap();
        let media = native
            .extract_endpoint(&endpoint, "10160743390456729")
            .await
            .unwrap();
        assert!(!media.is_empty());
        for item in media {
            assert_eq!(
                item.headers.get("User-Agent").map(String::as_str),
                Some("gallery-dl-native-test/1")
            );
        }
        let request = server.join().unwrap().to_ascii_lowercase();
        assert!(request.contains("user-agent: gallery-dl-native-test/1\r\n"));
    }

    #[tokio::test]
    async fn api_requests_use_configured_http_proxy() {
        let (proxy, server) = serve_once("200 OK", "", PHOTO, Duration::ZERO);
        let network =
            NetworkOptions::new(Some(proxy.origin().ascii_serialization()), None).unwrap();
        let native = Facebook::new(None, Duration::from_secs(3))
            .await
            .unwrap()
            .with_network(&network)
            .unwrap();
        let mut endpoint = proxy;
        endpoint.set_host(Some("native-api.invalid")).unwrap();
        endpoint.set_port(None).unwrap();
        let media = native
            .extract_endpoint(&endpoint, "10160743390456729")
            .await
            .unwrap();
        assert!(!media.is_empty());
        let request = server.join().unwrap();
        assert!(request.starts_with(&format!("GET {endpoint} HTTP/1.1\r\n")));
    }

    #[tokio::test]
    async fn share_links_resolve_to_supported_targets_only() {
        let (endpoint, server) =
            serve_sequence(vec![
            response(
                "302 Found",
                "Location: https://www.facebook.com/reel/1195289147628387/?mibextid=x\r\n",
                b"",
            ),
            response(
                "200 OK",
                "Content-Type: text/html\r\n",
                br#"<link rel="canonical" href="https://www.facebook.com/watch/?v=42&amp;x=1">"#,
            ),
            response("302 Found", "Location: https://www.facebook.com/login/\r\n", b""),
        ]);
        let native = Facebook::new(None, Duration::from_secs(3)).await.unwrap();
        let share = endpoint.join("/share/r/AbC/").unwrap();
        let reel = native.resolve_share(&share).await.unwrap();
        assert_eq!(video_id(&reel).unwrap(), "1195289147628387");
        let canonical = native.resolve_share(&share).await.unwrap();
        assert_eq!(video_id(&canonical).unwrap(), "42");
        let login = native.resolve_share(&share).await.unwrap_err().to_string();
        assert!(login.contains("did not resolve"), "{login}");
        let requests = server.join().unwrap();
        assert!(requests[0].2.starts_with("GET /share/r/AbC/ "));
        assert!(is_share_link(
            &Url::parse("https://www.facebook.com/share/v/1x/").unwrap()
        ));
        assert!(!is_share_link(
            &Url::parse("https://www.facebook.com/reel/1").unwrap()
        ));
        assert!(!share_target(
            &Url::parse("https://www.facebook.com/share/r/x/").unwrap()
        ));
    }

    fn serve_once(
        status: &str,
        extra_headers: &str,
        body: &[u8],
        delay: Duration,
    ) -> (Url, JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint =
            Url::parse(&format!("http://{}/photo/", listener.local_addr().unwrap())).unwrap();
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{extra_headers}\r\n",
            body.len()
        );
        let body = body.to_vec();
        listener.set_nonblocking(true).unwrap();
        let task = thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(std::time::Instant::now() < deadline, "server timed out");
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(e) => panic!("{e}"),
                }
            };
            // Windows can inherit the listener's nonblocking mode.
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let count = stream.read(&mut buffer).unwrap();
                assert_ne!(count, 0, "client closed before completing request headers");
                request.extend_from_slice(&buffer[..count]);
                assert!(request.len() < 64 * 1024);
            }
            thread::sleep(delay);
            // A timed-out client can disconnect before the response is written.
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.write_all(&body);
            String::from_utf8(request).unwrap()
        });
        (endpoint, task)
    }

    // Raw responses also allow exercising connection loss and truncated bodies.
    type Arrivals = Vec<(std::time::Instant, SystemTime, String)>;

    fn serve_sequence(responses: Vec<Vec<u8>>) -> (Url, JoinHandle<Arrivals>) {
        serve_delayed_sequence(
            responses
                .into_iter()
                .map(|body| (body, Duration::ZERO))
                .collect(),
        )
    }

    fn serve_delayed_sequence(responses: Vec<(Vec<u8>, Duration)>) -> (Url, JoinHandle<Arrivals>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint =
            Url::parse(&format!("http://{}/photo/", listener.local_addr().unwrap())).unwrap();
        let task = thread::spawn(move || {
            let mut arrivals = Vec::new();
            for (response, delay) in responses {
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(std::time::Instant::now() < deadline, "server timed out");
                            thread::sleep(Duration::from_millis(2));
                        }
                        Err(e) => panic!("{e}"),
                    }
                };
                let arrival = (std::time::Instant::now(), SystemTime::now());
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 1024];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    let count = stream.read(&mut buffer).unwrap();
                    assert_ne!(count, 0);
                    request.extend_from_slice(&buffer[..count]);
                    assert!(request.len() < 64 * 1024);
                }
                arrivals.push((arrival.0, arrival.1, String::from_utf8(request).unwrap()));
                thread::sleep(delay);
                let _ = stream.write_all(&response);
            }
            arrivals
        });
        (endpoint, task)
    }

    fn response(status: &str, headers: &str, body: &[u8]) -> Vec<u8> {
        let mut response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(body);
        response
    }

    #[tokio::test]
    async fn retries_transient_statuses_and_transport_failures() {
        for transient in [
            response("429 Too Many Requests", "Retry-After: 0\r\n", b""),
            response("503 Service Unavailable", "Retry-After: invalid\r\n", b""),
            Vec::new(), // Connection closed before response headers.
            b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\n{".to_vec(),
        ] {
            let (endpoint, server) = serve_sequence(vec![transient, response("200 OK", "", PHOTO)]);
            let facebook = Facebook::new(None, Duration::from_secs(3))
                .await
                .unwrap()
                .with_retries(1);
            assert_eq!(
                facebook
                    .extract_endpoint(&endpoint, "10160743390456729")
                    .await
                    .unwrap()
                    .len(),
                1
            );
            let arrivals = server.join().unwrap();
            assert!(arrivals[1].0.duration_since(arrivals[0].0) >= Duration::from_millis(250));
        }
    }

    #[tokio::test]
    async fn honors_numeric_and_http_date_cooldowns() {
        for use_date in [false, true] {
            let value = if use_date {
                httpdate::fmt_http_date(SystemTime::now() + Duration::from_secs(2))
            } else {
                "1".to_owned()
            };
            let (endpoint, server) = serve_sequence(vec![
                response(
                    "429 Too Many Requests",
                    &format!("Retry-After: {value}\r\n"),
                    b"",
                ),
                response("200 OK", "", PHOTO),
            ]);
            let facebook = Facebook::new(None, Duration::from_secs(4))
                .await
                .unwrap()
                .with_retries(1);
            facebook
                .extract_endpoint(&endpoint, "10160743390456729")
                .await
                .unwrap();
            let arrivals = server.join().unwrap();
            if use_date {
                assert!(arrivals[1].1 >= httpdate::parse_http_date(&value).unwrap());
            } else {
                assert!(arrivals[1].0.duration_since(arrivals[0].0) >= Duration::from_secs(1));
            }
        }
    }

    #[tokio::test]
    async fn retains_final_attempt_cooldown_for_the_next_post() {
        let (endpoint, server) = serve_sequence(vec![
            response("429 Too Many Requests", "Retry-After: 1\r\n", b""),
            response("200 OK", "", PHOTO),
        ]);
        let facebook = Facebook::new(None, Duration::from_secs(3)).await.unwrap();
        assert!(
            facebook
                .extract_endpoint(&endpoint, "10160743390456729")
                .await
                .unwrap_err()
                .to_string()
                .contains("rate limit")
        );
        let mut next = endpoint.clone();
        next.set_path("/photo.php/");
        facebook
            .extract_endpoint(&next, "10160743390456729")
            .await
            .unwrap();
        let arrivals = server.join().unwrap();
        assert!(arrivals[1].0.duration_since(arrivals[0].0) >= Duration::from_secs(1));
    }

    #[tokio::test]
    async fn bounds_retries_and_does_not_retry_terminal_errors() {
        let (endpoint, server) =
            serve_sequence(vec![response("503 Service Unavailable", "", b""); 2]);
        let facebook = Facebook::new(None, Duration::from_secs(3))
            .await
            .unwrap()
            .with_retries(1);
        assert!(
            facebook
                .extract_endpoint(&endpoint, "10160743390456729")
                .await
                .unwrap_err()
                .to_string()
                .contains("503")
        );
        assert_eq!(server.join().unwrap().len(), 2);
        for (status, body, expected) in [
            ("401 Unauthorized", b"".as_slice(), "authentication"),
            ("403 Forbidden", b"".as_slice(), "authentication"),
            ("404 Not Found", b"".as_slice(), "unavailable"),
            ("410 Gone", b"".as_slice(), "unavailable"),
            ("302 Found", b"".as_slice(), "redirect"),
            ("400 Bad Request", b"".as_slice(), "400"),
            ("200 OK", b"<html>login</html>".as_slice(), "unavailable"),
            (
                "200 OK",
                br#"{"__dr":"CometErrorRoot.react"}"#.as_slice(),
                "blocked",
            ),
        ] {
            let (endpoint, server) = serve_sequence(vec![response(status, "", body)]);
            let facebook = Facebook::new(None, Duration::from_secs(3))
                .await
                .unwrap()
                .with_retries(2);
            assert!(
                facebook
                    .extract_endpoint(&endpoint, "10160743390456729")
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains(expected)
            );
            assert_eq!(server.join().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn bounds_total_retry_time_and_rejects_excessive_cooldowns() {
        let (endpoint, server) =
            serve_sequence(vec![response("503 Service Unavailable", "", b""); 3]);
        let facebook = Facebook::new(None, Duration::from_millis(1500))
            .await
            .unwrap()
            .with_retries(10);
        let start = std::time::Instant::now();
        let error = facebook
            .extract_endpoint(&endpoint, "10160743390456729")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("timed out"), "{error}");
        assert!(start.elapsed() < Duration::from_secs(3));
        assert_eq!(server.join().unwrap().len(), 3);

        let (endpoint, server) = serve_sequence(vec![response(
            "429 Too Many Requests",
            "Retry-After: 999999999999999999999999\r\n",
            b"",
        )]);
        let facebook = Facebook::new(None, Duration::from_secs(1))
            .await
            .unwrap()
            .with_retries(1);
        let error = facebook
            .extract_endpoint(&endpoint, "10160743390456729")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("retry later"), "{error}");
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn cancellation_stops_cooldown_wait_without_losing_the_deadline() {
        let (endpoint, server) = serve_sequence(vec![
            response("429 Too Many Requests", "Retry-After: 1\r\n", b""),
            response("200 OK", "", PHOTO),
        ]);
        let facebook = Facebook::new(None, Duration::from_secs(3))
            .await
            .unwrap()
            .with_retries(1);
        assert!(
            tokio::time::timeout(
                Duration::from_millis(400),
                facebook.extract_endpoint(&endpoint, "10160743390456729")
            )
            .await
            .is_err()
        );
        // Dropping the previous future must not leave a worker making retries,
        // and a new call must still honor the previously observed cooldown.
        facebook
            .extract_endpoint(&endpoint, "10160743390456729")
            .await
            .unwrap();
        let arrivals = server.join().unwrap();
        assert_eq!(arrivals.len(), 2);
        assert!(arrivals[1].0.duration_since(arrivals[0].0) >= Duration::from_secs(1));
    }

    #[tokio::test]
    async fn redirects_update_and_reselect_scoped_cookies_without_forwarding_to_media() {
        let (endpoint, server) = serve_sequence(vec![
            response(
                "302 Found",
                "Location: /photo.php/?fbid=10160743390456729\r\nSet-Cookie: session=rotated; Path=/\r\n",
                b"",
            ),
            response(
                "200 OK",
                "Set-Cookie: local=; Path=/photo.php/; Max-Age=0\r\n",
                PHOTO,
            ),
            response("200 OK", "", PHOTO),
        ]);
        let mut native = Facebook::new(None, Duration::from_secs(3)).await.unwrap();
        native.cookies = CookieJar::parse(concat!(
            "127.0.0.1\tFALSE\t/\tFALSE\t0\tsession\tinitial\n",
            "127.0.0.1\tFALSE\t/photo.php/\tFALSE\t0\tlocal\tpath-only\n",
            "elsewhere.test\tFALSE\t/\tFALSE\t0\tforeign\texcluded\n",
        ))
        .unwrap()
        .into();
        let media = native
            .extract_endpoint(&endpoint, "10160743390456729")
            .await
            .unwrap();
        assert!(!media[0].headers.contains_key("Cookie"));
        let mut next = endpoint.clone();
        next.set_path("/photo.php/");
        native
            .extract_endpoint(&next, "10160743390456729")
            .await
            .unwrap();
        let arrivals = server.join().unwrap();
        let first = arrivals[0].2.to_ascii_lowercase();
        assert!(first.contains("session=initial"));
        assert!(!first.contains("path-only"));
        let redirect = arrivals[1].2.to_ascii_lowercase();
        assert!(redirect.contains("cookie: local=path-only; session=rotated\r\n"));
        assert!(!arrivals[2].2.contains("path-only"));
        for (_, _, request) in arrivals {
            assert!(!request.contains("excluded"));
        }
    }

    #[tokio::test]
    async fn refuses_unsafe_login_and_loop_redirects_without_disclosing_locations() {
        for (location, expected) in [
            ("https://foreign.test/secret", "same origin"),
            ("/login/?next=secret", "authentication"),
            ("/checkpoint/?secret", "challenge"),
            ("/photo/#secret", "loop"),
            ("/unexpected/secret", "outside"),
        ] {
            let (endpoint, server) = serve_sequence(vec![response(
                "302 Found",
                &format!("Location: {location}\r\n"),
                b"",
            )]);
            let native = Facebook::new(None, Duration::from_secs(2))
                .await
                .unwrap()
                .with_retries(2);
            let message = native
                .extract_endpoint(&endpoint, "10160743390456729")
                .await
                .unwrap_err()
                .to_string();
            assert!(message.contains(expected), "{message}");
            assert!(!message.contains("secret"));
            assert_eq!(server.join().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn limits_response_size_without_retrying_oversized_payloads() {
        let body = vec![b'x'; MAX_RESPONSE + 1];
        let (endpoint, server) = serve_sequence(vec![response("200 OK", "", &body)]);
        let native = Facebook::new(None, Duration::from_secs(3))
            .await
            .unwrap()
            .with_retries(1);
        let message = native
            .extract_endpoint(&endpoint, "10160743390456729")
            .await
            .unwrap_err()
            .to_string();
        assert!(message.contains("exceeded 16 MiB"), "{message}");
        assert_eq!(server.join().unwrap().len(), 1);
    }
}

#[cfg(test)]
#[path = "facebook_album_tests.rs"]
mod album_tests;
