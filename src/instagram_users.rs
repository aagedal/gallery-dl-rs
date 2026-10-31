//! Profile dispatcher, profile info/avatar, follow lists, hashtags and guides.
use super::*;
use crate::{
    dispatch::{self, Include},
    event::ChildMetadata,
    profile_asset::{ProfileAssetKind, ProfileAssetMetadata},
};

/// Upstream `InstagramUserExtractor` views in catalog order.
const VIEWS: &[&str] = &[
    "info",
    "avatar",
    "stories",
    "highlights",
    "posts",
    "reels",
    "tagged",
];
const MAX_PAGES: usize = 1000;
const MAX_TAG_CHARS: usize = 150;

#[derive(Clone)]
enum User {
    Name(String),
    Id(String),
}

impl User {
    /// Path component accepted by the native collection and story routes.
    fn key(&self) -> String {
        match self {
            Self::Name(name) => name.clone(),
            Self::Id(id) => format!("id:{id}"),
        }
    }
}

#[derive(Clone)]
enum Source {
    Dispatch(User),
    Info(User),
    Avatar(User),
    Followers(User),
    Following(User),
    Tag(String),
    Guide(String),
}

pub(super) struct UserInput {
    source: Source,
}

pub(super) fn input(url: &Url) -> Result<UserInput> {
    if !matches!(url.scheme(), "http" | "https")
        || !matches!(
            url.host_str(),
            Some("instagram.com" | "www.instagram.com" | "m.instagram.com")
        )
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return Err(error(
            "native Instagram profile views require an Instagram URL",
        ));
    }
    let parts: Vec<_> = url.path().trim_end_matches('/').split('/').collect();
    let source = match parts.as_slice() {
        ["", "explore", "tags", tag] => Source::Tag(hashtag(tag)?),
        ["", name] => Source::Dispatch(user(name)?),
        ["", name, "info"] => Source::Info(user(name)?),
        ["", name, "avatar"] => Source::Avatar(user(name)?),
        ["", name, "followers"] => Source::Followers(user(name)?),
        ["", name, "following"] => Source::Following(user(name)?),
        ["", name, "guide", label, id] if !label.is_empty() && label.len() <= 1024 => {
            user(name)?;
            Source::Guide(numeric(&Value::String((*id).into()))?)
        }
        _ => {
            return Err(error(
                "unsupported native Instagram profile URL; use --extractor compatibility",
            ));
        }
    };
    Ok(UserInput { source })
}

fn user(value: &str) -> Result<User> {
    if let Some(id) = value.strip_prefix("id:") {
        return Ok(User::Id(numeric(&Value::String(id.into()))?));
    }
    let url = Url::parse(&format!("https://www.instagram.com/{value}/"))?;
    Ok(User::Name(profile_username(&url)?))
}

fn numeric(value: &Value) -> Result<String> {
    let id = identifier(Some(value))?;
    if id.len() > 32 || id.bytes().all(|b| b == b'0') {
        return Err(error("invalid Instagram user identifier"));
    }
    Ok(id)
}

/// Decode one percent-encoded path segment into a bounded hashtag name.
fn hashtag(segment: &str) -> Result<String> {
    let invalid = || error("invalid Instagram hashtag");
    let bytes = segment.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = bytes.get(index + 1..index + 3).ok_or_else(invalid)?;
            let hex = std::str::from_utf8(hex).map_err(|_| invalid())?;
            decoded.push(u8::from_str_radix(hex, 16).map_err(|_| invalid())?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    let tag = String::from_utf8(decoded).map_err(|_| invalid())?;
    let count = tag.chars().count();
    if count == 0 || count > MAX_TAG_CHARS || !tag.chars().all(|c| c.is_alphanumeric() || c == '_')
    {
        return Err(invalid());
    }
    Ok(tag)
}

/// Bare profiles queue their selected views; follow lists queue profiles;
/// hashtags and guides are not newest-first. Date selection must not stop
/// at the first older post for any of these sources.
pub fn is_unordered_user_url(url: &Url, include: Option<&Include>) -> bool {
    match input(url).map(|input| input.source) {
        Ok(Source::Dispatch(_)) => dispatch::select(include, &["posts"], VIEWS, &[])
            .iter()
            .any(|view| !matches!(view.as_str(), "posts" | "info")),
        Ok(Source::Followers(_) | Source::Following(_) | Source::Tag(_) | Source::Guide(_)) => true,
        _ => false,
    }
}

fn shortcode_from_id(mut id: u64) -> String {
    let mut code = Vec::new();
    loop {
        code.push(SHORTCODE_ALPHABET[(id % 64) as usize]);
        id /= 64;
        if id == 0 {
            break;
        }
    }
    code.reverse();
    String::from_utf8(code).unwrap()
}

/// Queued follow-list profile with bounded public metadata.
struct Friend {
    username: String,
    metadata: ChildMetadata,
}

struct FriendState {
    user: User,
    following: bool,
    base: Url,
    user_id: Option<String>,
    cursor: Option<String>,
    seen: HashSet<[u8; 32]>,
    visited: HashSet<String>,
    pages: usize,
    pending: VecDeque<Friend>,
    done: bool,
    budget: Duration,
}

enum FeedKind {
    Tag(String),
    Guide(String),
}

struct FeedState {
    kind: FeedKind,
    base: Url,
    cursor: Option<String>,
    page: Option<String>,
    seen: HashSet<[u8; 32]>,
    pages: usize,
    pending: VecDeque<ExtractionEvent>,
    done: bool,
    budget: Duration,
}

impl Instagram {
    pub(super) fn user_events(&self, input: UserInput) -> BoxStream<'_, Result<ExtractionEvent>> {
        let base = self.api_base.clone();
        match input.source {
            Source::Dispatch(user) => self.dispatch_events(user),
            Source::Info(user) => self.profile_view_events(user, false, base),
            Source::Avatar(user) => self.profile_view_events(user, true, base),
            Source::Followers(user) => self.friend_events(user, false, base),
            Source::Following(user) => self.friend_events(user, true, base),
            Source::Tag(tag) => self.feed_events(FeedKind::Tag(tag), base),
            Source::Guide(id) => self.feed_events(FeedKind::Guide(id), base),
        }
    }

    fn dispatch_events(&self, user: User) -> BoxStream<'_, Result<ExtractionEvent>> {
        let views = dispatch::select(self.include.as_ref(), &["posts"], VIEWS, &[]);
        stream::iter(views)
            .flat_map(move |view| {
                let user = user.clone();
                event::child_job(
                    ChildMetadata::from_raw("instagram", "user", &serde_json::Map::new()),
                    move || self.dispatch_view(user, &view),
                )
            })
            .boxed()
    }

    fn dispatch_view(&self, user: User, view: &str) -> BoxStream<'_, Result<ExtractionEvent>> {
        let base = self.api_base.clone();
        let key = user.key();
        let path = match (view, user) {
            // Preserve the established bare-profile feed for named profiles.
            ("posts", User::Name(name)) => return self.profile_or_logged_out(name, base),
            ("info", user) => return self.profile_view_events(user, false, base),
            ("avatar", user) => return self.profile_view_events(user, true, base),
            ("stories", _) => format!("stories/{key}/"),
            ("highlights", _) => format!("{key}/highlights/"),
            ("posts", _) => format!("{key}/posts/"),
            ("reels", _) => format!("{key}/reels/"),
            ("tagged", _) => format!("{key}/tagged/"),
            _ => unreachable!("dispatcher selected a known view"),
        };
        let url = Url::parse(&format!("https://www.instagram.com/{path}"))
            .expect("validated Instagram profile view URL");
        self.events(&url)
    }

    /// Resolve the complete public profile object for a name or numeric ID.
    async fn profile_object(&self, user: &User, base: &Url, budget: Duration) -> Result<Value> {
        let (endpoint, pointer) = match user {
            User::Name(name) => {
                let mut endpoint = base.join("users/web_profile_info/")?;
                endpoint.query_pairs_mut().append_pair("username", name);
                (endpoint, "/data/user")
            }
            User::Id(id) => (base.join(&format!("users/{id}/info/"))?, "/user"),
        };
        let mut data = self.request_json(&endpoint, "", budget).await?;
        validate_response(&data)?;
        let profile = data
            .pointer_mut(pointer)
            .filter(|user| user.is_object())
            .map(Value::take)
            .ok_or_else(|| error("Instagram profile unavailable"))?;
        let matches = match user {
            User::Name(name) => profile
                .get("username")
                .and_then(Value::as_str)
                .is_some_and(|actual| actual.eq_ignore_ascii_case(name)),
            User::Id(id) => profile_id(&profile).is_ok_and(|actual| &actual == id),
        };
        if !matches {
            return Err(error(
                "Instagram response does not match the requested profile",
            ));
        }
        profile_id(&profile)?;
        Ok(profile)
    }

    fn profile_view_events(
        &self,
        user: User,
        avatar: bool,
        base: Url,
    ) -> BoxStream<'_, Result<ExtractionEvent>> {
        stream::once(async move {
            let profile = self.profile_object(&user, &base, self.timeout).await?;
            let events = if avatar {
                let mut media = avatar_media(&profile)?;
                self.network.apply_media(&mut media);
                event::group(vec![media], "instagram")
            } else {
                VecDeque::from([
                    ExtractionEvent::Post(Box::new(event::Post {
                        keywords: Default::default(),
                        category: "instagram".into(),
                        count: Some(0),
                        metadata: Some(PostMetadata {
                            post_id: profile_id(&profile)?,
                            username: profile
                                .get("username")
                                .and_then(Value::as_str)
                                .map(std::sync::Arc::from),
                            caption: profile
                                .get("biography")
                                .and_then(Value::as_str)
                                .map(std::sync::Arc::from),
                            ..PostMetadata::default()
                        }),
                    })),
                    ExtractionEvent::EndPost,
                ])
            };
            Ok::<_, crate::Error>(stream::iter(events.into_iter().map(Ok)).boxed())
        })
        .map(|result| match result {
            Ok(events) => events,
            Err(e) => stream::once(async move { Err(e) }).boxed(),
        })
        .flatten()
        .boxed()
    }

    fn friend_events(
        &self,
        user: User,
        following: bool,
        base: Url,
    ) -> BoxStream<'_, Result<ExtractionEvent>> {
        let user_id = match &user {
            User::Id(id) => Some(id.clone()),
            User::Name(_) => None,
        };
        let state = FriendState {
            user,
            following,
            base,
            user_id,
            cursor: None,
            seen: HashSet::new(),
            visited: HashSet::new(),
            pages: 0,
            pending: VecDeque::new(),
            done: false,
            budget: self.timeout,
        };
        stream::unfold(state, move |mut state| async move {
            loop {
                if let Some(friend) = state.pending.pop_front() {
                    return Some((Ok(friend), state));
                }
                if state.done {
                    return None;
                }
                let started = tokio::time::Instant::now();
                let result = tokio::time::timeout(state.budget, self.friend_page(&mut state)).await;
                state.budget = state.budget.saturating_sub(started.elapsed());
                let result =
                    result.unwrap_or_else(|_| Err(error("native Instagram extraction timed out")));
                if let Err(e) = result {
                    state.done = true;
                    return Some((Err(e), state));
                }
            }
        })
        .flat_map(move |result| match result {
            Ok(friend) => event::child_job(friend.metadata, move || {
                // Re-enter the bare-profile dispatcher like upstream's queue.
                let url = Url::parse(&format!("https://www.instagram.com/{}/", friend.username))
                    .expect("validated Instagram username");
                self.events(&url)
            }),
            Err(e) => stream::once(async move { Err(e) }).boxed(),
        })
        .boxed()
    }

    async fn friend_page(&self, state: &mut FriendState) -> Result<()> {
        if state.user_id.is_none() {
            let profile = self
                .profile_object(&state.user, &state.base, state.budget)
                .await?;
            state.user_id = Some(profile_id(&profile)?);
            return Ok(());
        }
        if state.pages >= MAX_PAGES {
            return Err(error("Instagram follow list exceeded the page limit"));
        }
        state.pages += 1;
        let user_id = state.user_id.as_deref().unwrap();
        let kind = if state.following {
            "following"
        } else {
            "followers"
        };
        let mut endpoint = state.base.join(&format!("friendships/{user_id}/{kind}/"))?;
        endpoint.query_pairs_mut().append_pair("count", "12");
        if let Some(cursor) = &state.cursor {
            endpoint.query_pairs_mut().append_pair("max_id", cursor);
        }
        let data = self.request_json(&endpoint, "", state.budget).await?;
        validate_response(&data)?;
        let users = data
            .get("users")
            .and_then(Value::as_array)
            .ok_or_else(|| error("Instagram follow list users are missing"))?;
        // Upstream continues while `next_max_id` is truthy; it may be numeric.
        let cursor = match data.get("next_max_id") {
            None | Some(Value::Null) => None,
            Some(Value::String(cursor)) if cursor.is_empty() => None,
            Some(Value::String(cursor)) if cursor.len() <= 8192 => Some(cursor.clone()),
            Some(Value::Number(cursor)) if cursor.is_u64() => {
                Some(cursor.to_string()).filter(|cursor| cursor != "0")
            }
            _ => {
                return Err(error(
                    "Instagram follow list pagination cursor is missing or invalid",
                ));
            }
        };
        let digest = cursor
            .as_ref()
            .map(|cursor| <[u8; 32]>::from(Sha256::digest(cursor.as_bytes())));
        if digest.is_some_and(|digest| state.seen.contains(&digest)) {
            return Err(error("Instagram follow list pagination cursor repeated"));
        }
        let mut pending = VecDeque::new();
        let mut visited = Vec::new();
        for user in users {
            let id = identifier(user.get("pk").or_else(|| user.get("id")))?;
            if let (Some(pk), Some(alias)) = (user.get("pk"), user.get("id"))
                && identifier(Some(pk))? != identifier(Some(alias))?
            {
                return Err(error("Instagram follow list user identifiers conflict"));
            }
            let name = user
                .get("username")
                .and_then(Value::as_str)
                .ok_or_else(|| error("Instagram follow list username is missing"))?;
            let url = Url::parse(&format!("https://www.instagram.com/{name}/"))
                .map_err(|_| error("invalid Instagram follow list username"))?;
            let username = profile_username(&url)
                .ok()
                .filter(|username| username == name)
                .ok_or_else(|| error("invalid Instagram follow list username"))?;
            if state.visited.contains(&id) || visited.contains(&id) {
                continue;
            }
            visited.push(id.clone());
            let mut fields = user.as_object().cloned().unwrap_or_default();
            fields.insert("id".into(), Value::String(id));
            pending.push_back(Friend {
                username,
                metadata: ChildMetadata::from_raw("instagram", kind, &fields),
            });
        }
        state.visited.extend(visited);
        state.pending = pending;
        if let Some(digest) = digest {
            state.seen.insert(digest);
        }
        state.done = cursor.is_none();
        state.cursor = cursor;
        Ok(())
    }

    fn feed_events(&self, kind: FeedKind, base: Url) -> BoxStream<'_, Result<ExtractionEvent>> {
        let state = FeedState {
            kind,
            base,
            cursor: None,
            page: None,
            seen: HashSet::new(),
            pages: 0,
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
                let result = tokio::time::timeout(state.budget, self.feed_page(&mut state)).await;
                state.budget = state.budget.saturating_sub(started.elapsed());
                let result =
                    result.unwrap_or_else(|_| Err(error("native Instagram extraction timed out")));
                if let Err(e) = result {
                    state.done = true;
                    return Some((Err(e), state));
                }
            }
        })
        .boxed()
    }

    async fn feed_page(&self, state: &mut FeedState) -> Result<()> {
        if state.pages >= MAX_PAGES {
            return Err(error("Instagram feed exceeded the page limit"));
        }
        state.pages += 1;
        let (posts, cursor, page) = match &state.kind {
            FeedKind::Tag(tag) => {
                let mut endpoint = state.base.join("tags/")?;
                endpoint
                    .path_segments_mut()
                    .map_err(|_| error("invalid Instagram API base"))?
                    .pop_if_empty()
                    .push(tag)
                    .push("sections")
                    .push("");
                let mut form = vec![
                    ("include_persistent".to_owned(), "0".to_owned()),
                    ("surface".into(), "grid".into()),
                    ("tab".into(), "recent".into()),
                ];
                if let Some(cursor) = &state.cursor {
                    form.push(("max_id".into(), cursor.clone()));
                }
                if let Some(page) = &state.page {
                    form.push(("page".into(), page.clone()));
                }
                let data = self.request_form(&endpoint, &form, state.budget).await?;
                validate_response(&data)?;
                let sections = data
                    .get("sections")
                    .and_then(Value::as_array)
                    .ok_or_else(|| error("Instagram tag sections are missing"))?;
                let mut posts = Vec::new();
                for section in sections {
                    let medias = section
                        .pointer("/layout_content/medias")
                        .and_then(Value::as_array)
                        .ok_or_else(|| error("Instagram tag section media are missing"))?;
                    for media in medias {
                        posts.push(
                            media
                                .get("media")
                                .cloned()
                                .ok_or_else(|| error("Instagram tag media wrapper is missing"))?,
                        );
                    }
                }
                let more = data
                    .get("more_available")
                    .and_then(Value::as_bool)
                    .ok_or_else(|| error("Instagram tag pagination state is missing"))?;
                let (cursor, page) = if more {
                    let cursor = data
                        .get("next_max_id")
                        .and_then(Value::as_str)
                        .filter(|value| !value.is_empty() && value.len() <= 8192)
                        .ok_or_else(|| {
                            error("Instagram tag pagination cursor is missing or invalid")
                        })?;
                    let page = data
                        .get("next_page")
                        .and_then(Value::as_u64)
                        .ok_or_else(|| error("Instagram tag pagination page is missing"))?;
                    (Some(cursor.to_owned()), Some(page.to_string()))
                } else {
                    (None, None)
                };
                (posts, cursor, page)
            }
            FeedKind::Guide(id) => {
                let mut endpoint = state.base.join(&format!("guides/guide/{id}/"))?;
                if let Some(cursor) = &state.cursor {
                    endpoint.query_pairs_mut().append_pair("max_id", cursor);
                }
                let data = self.request_json(&endpoint, "", state.budget).await?;
                validate_response(&data)?;
                let items = data
                    .get("items")
                    .and_then(Value::as_array)
                    .ok_or_else(|| error("Instagram guide items are missing"))?;
                let mut posts = Vec::new();
                for item in items {
                    posts.extend(
                        item.get("media_items")
                            .and_then(Value::as_array)
                            .ok_or_else(|| error("Instagram guide media are missing"))?
                            .iter()
                            .cloned(),
                    );
                }
                let cursor = match data.get("next_max_id") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(cursor)) if cursor.is_empty() => None,
                    Some(Value::String(cursor)) if cursor.len() <= 8192 => Some(cursor.clone()),
                    _ => {
                        return Err(error(
                            "Instagram guide pagination cursor is missing or invalid",
                        ));
                    }
                };
                (posts, cursor, None)
            }
        };
        let digest = cursor
            .as_ref()
            .map(|cursor| <[u8; 32]>::from(Sha256::digest(cursor.as_bytes())));
        if digest.is_some_and(|digest| state.seen.contains(&digest)) {
            return Err(error("Instagram feed pagination cursor repeated"));
        }
        let mut pending = VecDeque::new();
        for post in &posts {
            let code = post
                .get("code")
                .and_then(Value::as_str)
                .ok_or_else(|| error("Instagram feed post shortcode is missing"))?;
            media_id(code)?;
            let media = self.post_files(post, code)?;
            pending.extend(event::group(media, "instagram"));
        }
        state.pending = pending;
        if let Some(digest) = digest {
            state.seen.insert(digest);
        }
        state.done = cursor.is_none();
        state.cursor = cursor;
        state.page = page;
        Ok(())
    }
}

/// Profile responses use `id`; REST user objects use `pk`. Aliases must agree.
fn profile_id(profile: &Value) -> Result<String> {
    let id = numeric(
        profile
            .get("id")
            .or_else(|| profile.get("pk"))
            .ok_or_else(|| error("Instagram profile identifier is missing"))?,
    )?;
    for alias in [profile.get("id"), profile.get("pk")].into_iter().flatten() {
        if numeric(alias)? != id {
            return Err(error("Instagram profile identifier aliases conflict"));
        }
    }
    Ok(id)
}

/// Build the upstream avatar pseudo-post: the HD variant when the profile
/// carries one, otherwise the public `profile_pic_url_hd`/`profile_pic_url`.
fn avatar_media(profile: &Value) -> Result<Media> {
    let owner = profile_id(profile)?;
    let invalid = || error("Instagram profile has no valid avatar URL");
    let variant = profile
        .get("hd_profile_pic_url_info")
        .filter(|info| info.get("url").and_then(Value::as_str).is_some())
        .or_else(|| {
            profile
                .get("hd_profile_pic_versions")
                .and_then(Value::as_array)
                .and_then(|versions| versions.last())
                .filter(|info| info.get("url").and_then(Value::as_str).is_some())
        });
    let (raw, width, height) = match variant {
        Some(info) => (
            info["url"].as_str().unwrap(),
            info.get("width").and_then(Value::as_u64),
            info.get("height").and_then(Value::as_u64),
        ),
        None => (
            profile
                .get("profile_pic_url_hd")
                .and_then(Value::as_str)
                .filter(|url| !url.is_empty())
                .or_else(|| profile.get("profile_pic_url").and_then(Value::as_str))
                .ok_or_else(invalid)?,
            None,
            None,
        ),
    };
    if raw.len() > 8192 {
        return Err(invalid());
    }
    let url = http_url(raw).map_err(|_| invalid())?;
    if url.port().is_some() || url.path().ends_with(".m3u8") || url.path().ends_with(".mpd") {
        return Err(invalid());
    }
    let filename = url.path().rsplit('/').next().unwrap_or("");
    let (stem, extension) = filename.rsplit_once('.').unwrap_or((filename, ""));
    let extension = match extension.to_ascii_lowercase().as_str() {
        "png" => "png",
        "webp" => "webp",
        "avif" => "avif",
        "heic" => "heic",
        _ => "jpg",
    };
    // Upstream identifies the avatar by the numeric part of `profile_pic_id`.
    let picture = profile
        .get("profile_pic_id")
        .and_then(Value::as_str)
        .map(|id| id.split('_').next().unwrap_or(""))
        .filter(|id| id.len() <= 32 && id.parse::<u64>().is_ok_and(|id| id > 0));
    let asset = ProfileAssetMetadata {
        kind: ProfileAssetKind::Avatar,
        owner_id: owner.clone(),
        version: picture.unwrap_or(stem).to_owned(),
    };
    let asset = asset.valid_version().then_some(asset);
    let version = asset
        .as_ref()
        .map_or_else(|| "avatar".to_owned(), |asset| asset.version.clone());
    let shortcode = picture
        .and_then(|id| id.parse().ok())
        .map(shortcode_from_id)
        .unwrap_or_default();
    let dimension = |value: Option<u64>| value.filter(|value| *value > 0);
    Ok(Media {
        url,
        job: crate::extract::MediaJob::DirectHttp,
        handoff: BTreeMap::new(),
        category: "instagram".into(),
        filename: format!("avatar_{owner}_{version}.{extension}"),
        metadata: Some(PostMetadata {
            profile_asset: asset,
            post_id: picture.map_or_else(|| owner.clone(), str::to_owned),
            // Without a valid asset identity, avoid inventing a media archive key.
            media_id: picture.map(str::to_owned).unwrap_or_default(),
            shortcode,
            username: profile
                .get("username")
                .and_then(Value::as_str)
                .map(std::sync::Arc::from),
            index: 1,
            file: Some(crate::file_metadata::FileMetadata {
                width: dimension(width),
                height: dimension(height),
                media_type: Some(std::sync::Arc::from("image")),
                ..Default::default()
            }),
            ..PostMetadata::default()
        }),
        headers: BTreeMap::from([
            ("User-Agent".into(), USER_AGENT.into()),
            ("Referer".into(), "https://www.instagram.com/".into()),
        ]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_view_routes_are_recognized_and_others_rejected() {
        for path in [
            "alice/",
            "alice",
            "id:42/",
            "alice/info/",
            "id:42/info",
            "alice/avatar/",
            "alice/followers/",
            "alice/following",
            "id:42/following/",
            "explore/tags/sunset/",
            "explore/tags/%E6%97%A5%E6%9C%AC/",
            "alice/guide/some-name/17900000000000000/",
        ] {
            let url = Url::parse(&format!("https://www.instagram.com/{path}")).unwrap();
            assert!(input(&url).is_ok(), "{path}");
        }
        for url in [
            "https://example.com/alice/info/",
            "https://u@instagram.com/alice/info/",
            "https://instagram.com:99/alice/info/",
            "https://instagram.com/p/info/",
            "https://instagram.com/alice/info/extra/",
            "https://instagram.com/alice/likes/",
            "https://instagram.com/id:0/info/",
            "https://instagram.com/id:x/info/",
            "https://instagram.com/explore/tags/",
            "https://instagram.com/explore/tags/a%2Fb/",
            "https://instagram.com/explore/tags/a%ZZ/",
            "https://instagram.com/explore/tags/a-b/",
            "https://instagram.com/explore/locations/1/",
            "https://instagram.com/alice/guide/name/0/",
            "https://instagram.com/alice/guide/name/x/",
            "https://instagram.com/alice//info/",
        ] {
            assert!(input(&Url::parse(url).unwrap()).is_err(), "{url}");
        }
        let long = "a".repeat(MAX_TAG_CHARS + 1);
        let url = Url::parse(&format!("https://instagram.com/explore/tags/{long}/")).unwrap();
        assert!(input(&url).is_err());
        assert_eq!(hashtag("%E6%97%A5%E6%9C%AC").unwrap(), "日本");
    }

    #[test]
    fn unordered_sources_follow_dispatch_selection() {
        let url = |path: &str| Url::parse(&format!("https://www.instagram.com/{path}")).unwrap();
        assert!(!is_unordered_user_url(&url("alice/"), None));
        assert!(!is_unordered_user_url(
            &url("alice/"),
            Some(&Include::Views(vec!["info".into(), "posts".into()]))
        ));
        assert!(is_unordered_user_url(&url("alice/"), Some(&Include::All)));
        assert!(is_unordered_user_url(
            &url("alice/"),
            Some(&Include::Views(vec!["reels".into()]))
        ));
        for path in [
            "alice/followers/",
            "alice/following/",
            "explore/tags/x/",
            "alice/guide/g/1/",
        ] {
            assert!(is_unordered_user_url(&url(path), None), "{path}");
        }
        assert!(!is_unordered_user_url(
            &url("alice/info/"),
            Some(&Include::All)
        ));
        assert!(!is_unordered_user_url(&url("p/BA/"), Some(&Include::All)));
    }

    #[test]
    fn shortcodes_round_trip_media_ids() {
        for id in [1u64, 63, 64, 3_141_592_653_589_793_238] {
            assert_eq!(media_id(&shortcode_from_id(id)).unwrap(), id);
        }
        assert_eq!(shortcode_from_id(0), "A");
    }
}
