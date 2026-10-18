//! Explicit REST collections; only one validated page is retained at a time.
use super::*;

#[derive(Clone)]
enum User {
    Name(String),
    Id(String),
}
#[derive(Clone)]
enum Source {
    Posts(User),
    Reels(User),
    Tagged(User),
    Saved,
    Collection(String),
}
pub(super) struct CollectionInput {
    source: Source,
}

pub(super) fn input(url: &Url) -> Result<CollectionInput> {
    if !matches!(url.scheme(), "http" | "https")
        || !matches!(
            url.host_str(),
            Some("instagram.com" | "www.instagram.com" | "m.instagram.com")
        )
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return Err(error("native collections require an Instagram URL"));
    }
    let parts: Vec<_> = url.path().trim_end_matches('/').split('/').collect();
    let source = match parts.as_slice() {
        ["", name, "posts"] => Source::Posts(user(name)?),
        ["", name, "reels"] => Source::Reels(user(name)?),
        ["", name, "tagged"] => Source::Tagged(user(name)?),
        ["", name, "saved"] | ["", name, "saved", "all-posts"] => {
            user(name)?;
            Source::Saved
        }
        ["", name, "saved", label, id] if !label.is_empty() && label.len() <= 1024 => {
            user(name)?;
            Source::Collection(numeric(Some(&Value::String((*id).into())))?)
        }
        _ => {
            return Err(error(
                "unsupported native Instagram collection URL; use --extractor compatibility",
            ));
        }
    };
    Ok(CollectionInput { source })
}

fn user(value: &str) -> Result<User> {
    if let Some(id) = value.strip_prefix("id:") {
        return Ok(User::Id(numeric(Some(&Value::String(id.into())))?));
    }
    let url = Url::parse(&format!("https://www.instagram.com/{value}/"))?;
    Ok(User::Name(profile_username(&url)?))
}

fn numeric(value: Option<&Value>) -> Result<String> {
    let id = identifier(value)?;
    if id.len() > 32 || id.bytes().all(|b| b == b'0') {
        return Err(error("invalid Instagram collection identifier"));
    }
    Ok(id)
}

// Profile responses use `id`; REST media owners use `pk`. Any alias supplied
// alongside the preferred identifier must refer to that same account.
fn user_identity(user: &Value, require_pk: bool) -> Result<String> {
    let primary = if require_pk {
        user.get("pk")
    } else {
        user.get("id").or_else(|| user.get("pk"))
    };
    let id = numeric(primary)?;
    for alias in [user.get("id"), user.get("pk")].into_iter().flatten() {
        if numeric(Some(alias))? != id {
            return Err(error(
                "Instagram collection user identifier aliases conflict",
            ));
        }
    }
    Ok(id)
}

fn validate_media_identity(media: &Value, owner: &str) -> Result<()> {
    let pk = numeric(media.get("pk"))?;
    if let Some(alias) = media.get("id") {
        // REST commonly represents `id` as MEDIA_PK_OWNER_PK.
        let alias = match alias {
            Value::String(id) => id.clone(),
            Value::Number(id) if id.is_u64() => id.to_string(),
            _ => return Err(error("invalid Instagram collection media identifier alias")),
        };
        let valid = if let Some((media_id, user_id)) = alias.split_once('_') {
            media_id == pk && user_id == owner
        } else {
            alias == pk
        };
        if !valid {
            return Err(error(
                "Instagram collection media identifier aliases conflict",
            ));
        }
    }
    Ok(())
}

/// These source orders cannot safely stop at the first old post.
pub fn is_unordered_collection_url(url: &Url) -> bool {
    input(url).is_ok_and(|input| !matches!(input.source, Source::Posts(_)))
}

struct State {
    source: Source,
    base: Url,
    user_id: Option<String>,
    cursor: Option<String>,
    seen: HashSet<[u8; 32]>,
    pending: VecDeque<ExtractionEvent>,
    done: bool,
    budget: Duration,
}

impl Instagram {
    #[cfg(test)]
    pub(super) fn collection_stream(
        &self,
        input: CollectionInput,
        base: Url,
    ) -> BoxStream<'_, Result<Media>> {
        event::media_only(self.collection_events(input, base))
    }

    pub(super) fn collection_events(
        &self,
        input: CollectionInput,
        base: Url,
    ) -> BoxStream<'_, Result<ExtractionEvent>> {
        let user_id = match &input.source {
            Source::Posts(User::Id(id))
            | Source::Reels(User::Id(id))
            | Source::Tagged(User::Id(id)) => Some(id.clone()),
            _ => None,
        };
        let state = State {
            source: input.source,
            base,
            user_id,
            cursor: None,
            seen: HashSet::new(),
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
                let result =
                    tokio::time::timeout(state.budget, self.collection_page(&mut state)).await;
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

    async fn collection_page(&self, state: &mut State) -> Result<()> {
        if state.user_id.is_none() {
            let name = match &state.source {
                Source::Posts(User::Name(name))
                | Source::Reels(User::Name(name))
                | Source::Tagged(User::Name(name)) => Some(name),
                _ => None,
            };
            if let Some(name) = name {
                let mut endpoint = state.base.join("users/web_profile_info/")?;
                endpoint.query_pairs_mut().append_pair("username", name);
                let data = self.request_json(&endpoint, "", state.budget).await?;
                validate_response(&data)?;
                let user = data
                    .pointer("/data/user")
                    .ok_or_else(|| error("Instagram profile unavailable"))?;
                if !user
                    .get("username")
                    .and_then(Value::as_str)
                    .is_some_and(|actual| actual.eq_ignore_ascii_case(name))
                {
                    return Err(error(
                        "Instagram response does not match the requested profile",
                    ));
                }
                state.user_id = Some(user_identity(user, false)?);
                return Ok(());
            }
        }
        let reels = matches!(state.source, Source::Reels(_));
        let wrapped = matches!(
            state.source,
            Source::Reels(_) | Source::Saved | Source::Collection(_)
        );
        let data = if reels {
            let endpoint = state.base.join("clips/user/")?;
            let mut form = vec![
                ("target_user_id".into(), state.user_id.clone().unwrap()),
                ("page_size".into(), "50".into()),
                ("include_feed_video".into(), "true".into()),
            ];
            if let Some(cursor) = &state.cursor {
                form.push(("max_id".into(), cursor.clone()));
            }
            self.request_form(&endpoint, &form, state.budget).await?
        } else {
            let (path, count) = match &state.source {
                Source::Posts(_) => (
                    format!("feed/user/{}/", state.user_id.as_ref().unwrap()),
                    "30",
                ),
                Source::Tagged(_) => (
                    format!("usertags/{}/feed/", state.user_id.as_ref().unwrap()),
                    "20",
                ),
                Source::Saved => ("feed/saved/posts/".into(), "50"),
                Source::Collection(id) => (format!("feed/collection/{id}/posts/"), "50"),
                Source::Reels(_) => unreachable!(),
            };
            let mut endpoint = state.base.join(&path)?;
            endpoint.query_pairs_mut().append_pair("count", count);
            if let Some(cursor) = &state.cursor {
                endpoint.query_pairs_mut().append_pair("max_id", cursor);
            }
            self.request_json(&endpoint, "", state.budget).await?
        };
        validate_response(&data)?;
        let posts = data
            .get("items")
            .and_then(Value::as_array)
            .ok_or_else(|| error("Instagram collection items are missing"))?;
        let pagination = if reels {
            data.get("paging_info")
                .ok_or_else(|| error("Instagram collection pagination state is missing"))?
        } else {
            &data
        };
        let more = pagination
            .get("more_available")
            .and_then(Value::as_bool)
            .ok_or_else(|| error("Instagram collection pagination state is missing"))?;
        let cursor = if more {
            let key = if reels { "max_id" } else { "next_max_id" };
            let cursor = pagination
                .get(key)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty() && value.len() <= 8192)
                .ok_or_else(|| {
                    error("Instagram collection pagination cursor is missing or invalid")
                })?;
            let digest = <[u8; 32]>::from(Sha256::digest(cursor.as_bytes()));
            if state.seen.contains(&digest) {
                return Err(error("Instagram collection pagination cursor repeated"));
            }
            Some((cursor.to_owned(), digest))
        } else {
            None
        };
        let mut pending = VecDeque::new();
        for entry in posts {
            let post = if wrapped {
                entry
                    .get("media")
                    .ok_or_else(|| error("Instagram collection media wrapper is missing"))?
            } else {
                entry
            };
            let owner = user_identity(
                post.get("user")
                    .ok_or_else(|| error("Instagram collection owner is missing"))?,
                true,
            )?;
            validate_media_identity(post, &owner)?;
            if let Some(children) = post.get("carousel_media").and_then(Value::as_array) {
                for child in children {
                    validate_media_identity(child, &owner)?;
                }
            }
            if matches!(state.source, Source::Posts(_) | Source::Reels(_)) {
                let target = state.user_id.as_deref().unwrap();
                let coauthor = reels
                    && post
                        .get("coauthor_producers")
                        .and_then(Value::as_array)
                        .is_some_and(|users| {
                            users
                                .iter()
                                .any(|user| user_identity(user, true).is_ok_and(|id| id == target))
                        });
                if owner != target && !coauthor {
                    return Err(error(
                        "Instagram collection post does not match the requested profile",
                    ));
                }
            }
            let code = post
                .get("code")
                .and_then(Value::as_str)
                .ok_or_else(|| error("Instagram collection post shortcode is missing"))?;
            media_id(code)?;
            let mut media = parse_post(post, code)?;
            for item in &mut media {
                self.network.apply_media(item);
            }
            pending.extend(event::group(media, "instagram"));
        }
        state.pending = pending;
        state.cursor = cursor.map(|(cursor, digest)| {
            state.seen.insert(digest);
            cursor
        });
        state.done = !more;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_collection_routes_and_date_order() {
        for path in [
            "alice/posts",
            "id:123/posts",
            "alice/reels",
            "alice/tagged",
            "alice/saved",
            "alice/saved/all-posts",
            "alice/saved/%E2%9C%93/123",
        ] {
            let url = Url::parse(&format!("https://www.instagram.com/{path}/")).unwrap();
            assert!(input(&url).is_ok(), "{path}");
            assert_eq!(
                is_unordered_collection_url(&url),
                !path.ends_with("posts") || path.contains("saved")
            );
        }
    }

    #[test]
    fn identifier_aliases_must_agree() {
        use serde_json::json;
        assert_eq!(
            user_identity(&json!({"id":"12","pk":12}), false).unwrap(),
            "12"
        );
        assert!(user_identity(&json!({"id":"12","pk":13}), false).is_err());
        assert!(user_identity(&json!({"id":"12"}), true).is_err());
        assert!(validate_media_identity(&json!({"pk":"64","id":"64_12"}), "12").is_ok());
        for alias in ["65", "65_12", "64_13", "64_12_extra"] {
            assert!(validate_media_identity(&json!({"pk":"64","id":alias}), "12").is_err());
        }
    }

    #[test]
    fn invalid_collection_routes_and_identifiers_are_rejected() {
        for url in [
            "https://example.com/alice/reels/",
            "https://u@instagram.com/alice/reels/",
            "https://instagram.com:99/alice/reels/",
            "https://instagram.com/p/reels/",
            "https://instagram.com/id:0/posts/",
            "https://instagram.com/id:-1/posts/",
            "https://instagram.com/id:123456789012345678901234567890123/posts/",
            "https://instagram.com/alice/saved/name/0/",
            "https://instagram.com/alice/saved/name/1/extra/",
            "https://instagram.com/alice//reels/",
        ] {
            assert!(input(&Url::parse(url).unwrap()).is_err(), "{url}");
        }
    }
}
