//! Experimental REST stories/highlights, preserving the API's source order.
use super::*;
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD},
};

#[derive(Clone)]
enum User {
    Name(String),
    Id(String),
}
#[derive(Clone)]
enum Source {
    User(User),
    Highlight(String),
    Highlights(User),
    Tray,
}
#[derive(Clone)]
pub(super) struct StoryInput {
    source: Source,
    media: Option<String>,
}

pub(super) fn input(url: &Url) -> Result<StoryInput> {
    if !matches!(url.scheme(), "http" | "https")
        || !matches!(
            url.host_str(),
            Some("instagram.com" | "www.instagram.com" | "m.instagram.com")
        )
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return Err(error("native stories require an Instagram URL"));
    }
    let parts: Vec<_> = url.path().trim_end_matches('/').split('/').collect();
    let mut selected = None;
    for (_, value) in url.query_pairs().filter(|(key, _)| key == "story_media_id") {
        if selected
            .replace(numeric(&Value::String(value.into_owned()))?)
            .is_some()
        {
            return Err(error("Instagram story URL has duplicate media selectors"));
        }
    }
    let source = match parts.as_slice() {
        ["", "stories", "me"] if selected.is_none() => Source::Tray,
        ["", "stories", "highlights", id] => {
            Source::Highlight(numeric(&Value::String((*id).into()))?)
        }
        ["", "stories", name] if *name != "me" && *name != "highlights" => {
            Source::User(user(name)?)
        }
        ["", "stories", name, id] if *name != "me" && *name != "highlights" => {
            if selected.is_some() {
                return Err(error("Instagram story URL has conflicting media selectors"));
            }
            selected = Some(numeric(&Value::String((*id).into()))?);
            Source::User(user(name)?)
        }
        ["", name, "highlights"] => Source::Highlights(user(name)?),
        ["", "s", encoded] => {
            if encoded.len() > 128 {
                return Err(error("invalid Instagram highlight share token"));
            }
            let decoded = STANDARD
                .decode(encoded)
                .or_else(|_| STANDARD_NO_PAD.decode(encoded))
                .map_err(|_| error("invalid Instagram highlight share token"))?;
            let decoded = std::str::from_utf8(&decoded)
                .map_err(|_| error("invalid Instagram highlight share token"))?;
            let id = decoded
                .strip_prefix("highlight:")
                .ok_or_else(|| error("invalid Instagram highlight share token"))?;
            Source::Highlight(numeric(&Value::String(id.into()))?)
        }
        _ => {
            return Err(error(
                "unsupported native Instagram story URL; use --extractor compatibility",
            ));
        }
    };
    if matches!(source, Source::Highlights(_)) && selected.is_some() {
        return Err(error(
            "Instagram highlight listing cannot select a single story",
        ));
    }
    Ok(StoryInput {
        source,
        media: selected,
    })
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
    if id.len() > 20 || id.parse::<u64>().ok().is_none_or(|id| id == 0) {
        return Err(error("invalid Instagram story identifier"));
    }
    Ok(id)
}

/// True only for story/highlight routes implemented by the native streamer.
/// These collections do not promise newest-first order for date selection.
pub fn is_story_url(url: &Url) -> bool {
    input(url).is_ok()
}

struct Reel {
    id: String,
    owner: Option<String>,
    username: Option<String>,
    media: Option<String>,
}
struct State {
    input: StoryInput,
    base: Url,
    initialized: bool,
    reels: VecDeque<Reel>,
    pending: VecDeque<ExtractionEvent>,
    done: bool,
    budget: Duration,
}

impl Instagram {
    #[cfg(test)]
    pub(super) fn story_stream(
        &self,
        input: StoryInput,
        base: Url,
    ) -> BoxStream<'_, Result<Media>> {
        event::media_only(self.story_events(input, base))
    }

    pub(super) fn story_events(
        &self,
        input: StoryInput,
        base: Url,
    ) -> BoxStream<'_, Result<ExtractionEvent>> {
        let state = State {
            input,
            base,
            initialized: false,
            reels: VecDeque::new(),
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
                let result = tokio::time::timeout(state.budget, self.story_page(&mut state)).await;
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

    async fn story_user(&self, user: &User, state: &State) -> Result<(String, Option<String>)> {
        if let User::Id(id) = user {
            return Ok((id.clone(), None));
        }
        let User::Name(name) = user else {
            unreachable!()
        };
        let mut endpoint = state.base.join("users/web_profile_info/")?;
        endpoint.query_pairs_mut().append_pair("username", name);
        let data = self.request_json(&endpoint, "", state.budget).await?;
        validate_response(&data)?;
        let user = data
            .pointer("/data/user")
            .ok_or_else(|| error("Instagram story profile unavailable"))?;
        if !user
            .get("username")
            .and_then(Value::as_str)
            .is_some_and(|value| value.eq_ignore_ascii_case(name))
        {
            return Err(error(
                "Instagram story profile does not match the requested user",
            ));
        }
        let id = numeric(
            user.get("id")
                .ok_or_else(|| error("Instagram story profile identifier is missing"))?,
        )?;
        validate_owner(user, Some(&id), Some(name))?;
        Ok((id, Some(name.clone())))
    }

    async fn initialize_stories(&self, state: &mut State) -> Result<()> {
        let mut reels = VecDeque::new();
        match state.input.source.clone() {
            Source::User(user) => {
                let (id, username) = self.story_user(&user, state).await?;
                reels.push_back(Reel {
                    id: id.clone(),
                    owner: Some(id),
                    username,
                    media: state.input.media.clone(),
                });
            }
            Source::Highlight(id) => reels.push_back(Reel {
                id: format!("highlight:{id}"),
                owner: None,
                username: None,
                media: state.input.media.clone(),
            }),
            Source::Highlights(user) => {
                let (owner, username) = self.story_user(&user, state).await?;
                let endpoint = state
                    .base
                    .join(&format!("highlights/{owner}/highlights_tray/"))?;
                let data = self.request_json(&endpoint, "", state.budget).await?;
                validate_response(&data)?;
                let tray = tray(&data)?;
                let mut seen = HashSet::new();
                for reel in tray {
                    let id = reel
                        .get("id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| error("Instagram highlight identifier is missing"))?;
                    numeric(&Value::String(
                        id.strip_prefix("highlight:")
                            .ok_or_else(|| error("invalid Instagram highlight identifier"))?
                            .into(),
                    ))?;
                    if !seen.insert(id.to_owned()) {
                        return Err(error("Instagram highlights tray repeated a reel"));
                    }
                    if let Some(user) = reel.get("user") {
                        validate_owner(user, Some(&owner), username.as_deref())?;
                    }
                    reels.push_back(Reel {
                        id: id.into(),
                        owner: Some(owner.clone()),
                        username: username.clone(),
                        media: None,
                    });
                }
            }
            Source::Tray => {
                let endpoint = state.base.join("feed/reels_tray/")?;
                let data = self.request_json(&endpoint, "", state.budget).await?;
                validate_response(&data)?;
                let mut seen = HashSet::new();
                for reel in tray(&data)? {
                    let id = numeric(
                        reel.get("id")
                            .ok_or_else(|| error("Instagram story tray identifier is missing"))?,
                    )?;
                    if !seen.insert(id.clone()) {
                        return Err(error("Instagram story tray repeated a reel"));
                    }
                    let username = if let Some(user) = reel.get("user") {
                        validate_owner(user, Some(&id), None)?;
                        user.get("username")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    } else {
                        None
                    };
                    reels.push_back(Reel {
                        id: id.clone(),
                        owner: Some(id),
                        username,
                        media: None,
                    });
                }
            }
        }
        state.reels = reels;
        state.initialized = true;
        Ok(())
    }

    async fn story_page(&self, state: &mut State) -> Result<()> {
        if !state.initialized {
            self.initialize_stories(state).await?;
        }
        let Some(reel) = state.reels.pop_front() else {
            state.done = true;
            return Ok(());
        };
        let mut endpoint = state.base.join("feed/reels_media/")?;
        endpoint.query_pairs_mut().append_pair("reel_ids", &reel.id);
        let data = self.request_json(&endpoint, "", state.budget).await?;
        validate_response(&data)?;
        let items = parse_reel_with(&data, &reel, self.static_videos)?;
        state.done = state.reels.is_empty();
        if data
            .get("reels_media")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty)
        {
            // An absent/expired reel is not an upstream directory. A returned
            // reel whose items array is empty still emits its post boundary.
            return Ok(());
        }
        let items = self.finish_files(items);
        let metadata = reel_metadata(&data, &reel)?;
        state.pending = event::group(items, "instagram");
        if let Some(ExtractionEvent::Post(post)) = state.pending.front_mut() {
            post.metadata = Some(metadata);
        }
        Ok(())
    }
}

fn reel_metadata(data: &Value, expected: &Reel) -> Result<PostMetadata> {
    // Reel IDs and dates belong to the upstream directory event. Story file IDs
    // remain independent, including when selecting one item inside a reel.
    let id = expected.id.rsplit(':').next().unwrap();
    let numeric_id = id
        .parse::<u64>()
        .map_err(|_| error("invalid Instagram reel identifier"))?;
    let reel = data
        .get("reels_media")
        .and_then(Value::as_array)
        .and_then(|reels| reels.first());
    let timestamp = reel.and_then(|reel| {
        ["taken_at", "created_at", "seen"]
            .into_iter()
            .filter_map(|field| reel.get(field).and_then(Value::as_i64))
            .find(|timestamp| *timestamp != 0)
            .or_else(|| {
                reel.get("expiring_at")
                    .and_then(Value::as_i64)
                    .and_then(|expires| expires.checked_sub(86400))
            })
    });
    Ok(PostMetadata {
        post_id: id.into(),
        shortcode: shortcode_from_id(numeric_id),
        username: reel
            .and_then(|reel| reel.pointer("/user/username"))
            .and_then(Value::as_str)
            .map(std::sync::Arc::from)
            .or_else(|| expected.username.as_deref().map(std::sync::Arc::from)),
        timestamp,
        ..Default::default()
    })
}

fn tray(data: &Value) -> Result<&Vec<Value>> {
    data.get("tray")
        .and_then(Value::as_array)
        .ok_or_else(|| error("Instagram story/highlight tray is missing"))
}

fn validate_owner(user: &Value, expected: Option<&str>, username: Option<&str>) -> Result<String> {
    let id = numeric(
        user.get("pk")
            .or_else(|| user.get("id"))
            .ok_or_else(|| error("Instagram story owner is missing"))?,
    )?;
    if user
        .get("pk")
        .zip(user.get("id"))
        .is_some_and(|(pk, other)| numeric(pk).ok() != numeric(other).ok())
        || expected.is_some_and(|expected| expected != id)
        || username
            .zip(user.get("username").and_then(Value::as_str))
            .is_some_and(|(name, actual)| !name.eq_ignore_ascii_case(actual))
    {
        return Err(error(
            "Instagram story owner does not match the requested reel",
        ));
    }
    Ok(id)
}

#[cfg(test)]
fn parse_reel(data: &Value, expected: &Reel) -> Result<Vec<Media>> {
    parse_reel_with(data, expected, true)
}

fn parse_reel_with(data: &Value, expected: &Reel, static_videos: bool) -> Result<Vec<Media>> {
    let reels = data
        .get("reels_media")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            error("Instagram reels response is missing; authenticated cookies may be required")
        })?;
    if reels.is_empty() {
        return if expected.media.is_some() {
            Err(error("Instagram requested story is unavailable"))
        } else {
            Ok(Vec::new())
        };
    }
    if reels.len() != 1
        || reels[0]
            .get("id")
            .map(|value| match value {
                Value::String(s) => s.clone(),
                _ => value.to_string(),
            })
            .as_deref()
            != Some(expected.id.as_str())
    {
        return Err(error(
            "Instagram reels response does not match the requested reel",
        ));
    }
    let reel = &reels[0];
    let user = reel
        .get("user")
        .ok_or_else(|| error("Instagram story reel owner is missing"))?;
    let owner = validate_owner(
        user,
        expected.owner.as_deref(),
        expected.username.as_deref(),
    )?;
    let items = reel
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| error("Instagram story reel items are missing"))?;
    let selected: Vec<_> = if let Some(id) = &expected.media {
        let matches: Vec<_> = items
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                item.get("pk")
                    .is_some_and(|pk| numeric(pk).ok().as_ref() == Some(id))
            })
            .collect();
        if matches.len() != 1 {
            return Err(error(
                "Instagram requested story is unavailable or ambiguous",
            ));
        }
        matches
    } else {
        items.iter().enumerate().collect()
    };
    let mut media = Vec::new();
    let mut seen = HashSet::new();
    for (index, item) in selected {
        let id = numeric(
            item.get("pk")
                .ok_or_else(|| error("Instagram story media identifier is missing"))?,
        )?;
        if !seen.insert(id.clone()) {
            return Err(error("Instagram story reel repeated a media item"));
        }
        if let Some(user) = item.get("user") {
            validate_owner(
                user,
                Some(&owner),
                reel.pointer("/user/username").and_then(Value::as_str),
            )?;
        }
        let code = if let Some(code) = item.get("code") {
            let code = code
                .as_str()
                .ok_or_else(|| error("invalid Instagram story shortcode"))?;
            if media_id(code)?.to_string() != id {
                return Err(error(
                    "Instagram story shortcode does not match its media identifier",
                ));
            }
            code.to_owned()
        } else {
            shortcode_from_id(id.parse::<u64>().unwrap())
        };
        if !matches!(item.get("media_type").and_then(Value::as_u64), Some(1 | 2)) {
            return Err(error("unsupported Instagram story media type"));
        }
        let mut post = item.clone();
        post["user"] = user.clone();
        if post.get("taken_at").is_none()
            && let Some(created) = post.get("created_at").cloned()
        {
            post["taken_at"] = created;
        }
        let mut parsed = parse_post_with(&post, &code, static_videos)?;
        for item in &mut parsed {
            if let Some(meta) = &mut item.metadata {
                meta.index = index + 1;
            }
        }
        media.extend(parsed);
    }
    Ok(media)
}

fn shortcode_from_id(mut id: u64) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut bytes = Vec::new();
    while id > 0 {
        bytes.push(ALPHABET[(id % 64) as usize]);
        id /= 64;
    }
    bytes.reverse();
    String::from_utf8(bytes).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn story_urls_cover_direct_highlight_shares_and_trays() {
        for path in [
            "/stories/fixture/",
            "/stories/fixture/101/",
            "/stories/id:123/",
            "/stories/highlights/123/",
            "/stories/highlights/123/?story_media_id=101",
            "/fixture/highlights/",
            "/id:123/highlights/",
            "/stories/me/",
            "/s/aGlnaGxpZ2h0OjEyMw==/?story_media_id=101",
            "/s/aGlnaGxpZ2h0OjEyMw/",
        ] {
            assert!(
                is_story_url(&Url::parse(&format!("https://www.instagram.com{path}")).unwrap()),
                "{path}"
            );
        }
        for raw in [
            "https://instagram.com/stories/",
            "https://instagram.com/stories/me/101/",
            "https://instagram.com/stories/highlights/",
            "https://instagram.com/stories/id:0/",
            "https://instagram.com/stories/fixture/0/",
            "https://instagram.com/stories/fixture/101/?story_media_id=101",
            "https://instagram.com/stories/fixture/?story_media_id=1&story_media_id=2",
            "https://instagram.com/fixture/highlights/?story_media_id=1",
            "https://instagram.com/s/cHJpdmF0ZS12YWx1ZQ==/",
            "https://instagram.com/s/aGlnaGxpZ2h0OjA=/",
            "https://instagram.com/stories/fixture/18446744073709551616/",
            "https://instagram.com.evil.invalid/stories/fixture/",
            "https://user:private-value@instagram.com/stories/fixture/",
            "https://instagram.com:444/stories/fixture/",
        ] {
            assert!(!is_story_url(&Url::parse(raw).unwrap()), "{raw}");
        }
    }

    #[test]
    fn shortcodes_round_trip_full_unsigned_media_ids() {
        for id in [1, 63, 64, 101, 123456789, u64::MAX] {
            assert_eq!(media_id(&shortcode_from_id(id)).unwrap(), id);
        }
    }

    fn image(id: &str) -> Value {
        json!({"pk":id,"media_type":1,"image_versions2":{"candidates":[{"url":"https://example.test/story.jpg","width":10,"height":10}]}})
    }

    fn reel(items: Value) -> Value {
        json!({"status":"ok","reels_media":[{"id":"123","user":{"pk":"123","username":"fixture"},"items":items}]})
    }

    fn expected() -> Reel {
        Reel {
            id: "123".into(),
            owner: Some("123".into()),
            username: Some("fixture".into()),
            media: None,
        }
    }

    #[test]
    fn reel_identity_conflicts_and_partial_media_fail_privately() {
        for data in [
            reel(json!([image("101"), image("101")])),
            reel(json!([image("101"),{"pk":"102","media_type":2}])),
            reel(
                json!([{"pk":"101","code":"BA","media_type":1,"image_versions2":{"candidates":[{"url":"https://example.test/private-value.jpg"}]}}]),
            ),
            json!({"reels_media":[{"id":"123","user":{"pk":"123","id":"456"},"items":[image("101")]}]}),
            json!({"reels_media":[{"id":"123","user":{"pk":"123","username":"private-value"},"items":[image("101")]}]}),
            json!({"reels_media":[{"id":"123","user":{"pk":"123"},"items":[]},{"id":"456","user":{"pk":"456"},"items":[]}]}),
        ] {
            let message = parse_reel(&data, &expected()).unwrap_err().to_string();
            assert!(!message.contains("private-value"), "{message}");
        }
    }

    #[test]
    fn stories_use_owned_user_item_date_and_consistent_shortcode() {
        let mut item = image("101");
        item["created_at"] = json!(100);
        item["code"] = json!("Bl");
        let media = parse_reel(&reel(json!([item])), &expected()).unwrap();
        let meta = media[0].metadata.as_ref().unwrap();
        assert_eq!(meta.shortcode, "Bl");
        assert_eq!(meta.timestamp, Some(100));
        assert_eq!(meta.username.as_deref(), Some("fixture"));
    }
}
