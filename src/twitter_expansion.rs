//! Explicit post-link traversal, shared by the pure parser and lazy runtime.
use super::*;
use std::collections::VecDeque;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RetweetMode {
    #[default]
    Skip,
    Repost,
    Original,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExpansionOptions {
    pub retweets: RetweetMode,
    pub quoted: bool,
}

pub fn parse_response_with_options(
    bytes: &[u8],
    requested_id: &str,
    options: ExpansionOptions,
) -> Result<Vec<Media>> {
    let value = response_root(bytes, requested_id)?;
    let mut engine = Engine::new(value, requested_id.into(), options, false);
    let mut result = Vec::new();
    loop {
        match engine.next()? {
            Action::Media(media) => result.extend(media),
            Action::Fetch(_) => return Err(error("Twitter linked post result missing")),
            Action::Done => return Ok(result),
        }
    }
}

pub(super) fn response_root(bytes: &[u8], id: &str) -> Result<Value> {
    if bytes.len() > MAX_RESPONSE {
        return Err(error("Twitter response exceeded 16 MiB"));
    }
    if !valid_id(id) {
        return Err(error("invalid Twitter post identifier"));
    }
    let data: Value = serde_json::from_slice(bytes).map_err(|_| error("Twitter returned a web page or invalid JSON; login or a client transaction challenge may be required"))?;
    check_errors(&data)?;
    let root = data
        .pointer("/data/tweetResult/result")
        .ok_or_else(|| error("Twitter post unavailable or unexpected GraphQL response"))?;
    validated(root, id)?;
    Ok(root.clone())
}

fn unwrapped(mut tweet: &Value) -> Result<&Value> {
    for _ in 0..4 {
        if tweet.get("__typename").and_then(Value::as_str) != Some("TweetWithVisibilityResults") {
            break;
        }
        tweet = tweet
            .get("tweet")
            .ok_or_else(|| error("Twitter visibility result has no post"))?;
    }
    match tweet.get("__typename").and_then(Value::as_str) {
        Some("Tweet") => Ok(tweet),
        Some("TweetUnavailable" | "TweetTombstone") => {
            Err(error(match tweet.get("reason").and_then(Value::as_str) {
                Some("Protected") => "Twitter protected post; this session cannot access it",
                Some("NsfwViewerHasNoStatedAge" | "NsfwLoggedOut") => {
                    "Twitter authentication or age verification required"
                }
                _ => "Twitter post unavailable or inaccessible to this session",
            }))
        }
        _ => Err(error(
            "Twitter post unavailable or unexpected GraphQL result",
        )),
    }
}

fn validated<'a>(value: &'a Value, id: &str) -> Result<&'a Value> {
    let tweet = unwrapped(value)?;
    if !valid_id(id) || tweet.get("rest_id").and_then(Value::as_str) != Some(id) {
        return Err(error("Twitter response does not match the requested post"));
    }
    let legacy = tweet
        .get("legacy")
        .filter(|v| v.is_object())
        .ok_or_else(|| error("Twitter post media fields missing"))?;
    if legacy.get("id_str").is_some_and(|v| v.as_str() != Some(id)) {
        return Err(error("Twitter response does not match the requested post"));
    }
    Ok(tweet)
}

struct Link {
    id: String,
    value: Option<Value>,
}

// All present aliases must agree. Never search unrelated nested objects.
fn link(tweet: &Value, name: &str, id_key: &str) -> Result<Option<Link>> {
    let legacy = &tweet["legacy"];
    let declared = legacy
        .get(id_key)
        .map(|v| {
            v.as_str()
                .filter(|id| valid_id(id))
                .ok_or_else(|| error("Twitter linked post identifier invalid"))
        })
        .transpose()?;
    let mut id = declared.map(str::to_owned);
    let mut value = None;
    for parent in [tweet, legacy] {
        if let Some(container) = parent.get(name) {
            let target = container
                .get("result")
                .ok_or_else(|| error("Twitter linked post result missing"))?;
            let target_tweet = unwrapped(target)?;
            let target_id = target_tweet
                .get("rest_id")
                .and_then(Value::as_str)
                .filter(|id| valid_id(id))
                .ok_or_else(|| error("Twitter linked post identifier invalid"))?;
            validated(target, target_id)?;
            if id.as_deref().is_some_and(|id| id != target_id)
                || value.as_ref().is_some_and(|v| v != target)
            {
                return Err(error("Twitter linked post results conflict"));
            }
            id = Some(target_id.into());
            value = Some(target.clone());
        }
    }
    Ok(id.map(|id| Link { id, value }))
}

struct Job {
    id: String,
    value: Option<Value>,
    path: Vec<String>,
    quote_depth: usize,
    quoted_by: Option<String>,
    repost: Option<Value>,
    reposted_by: Option<String>,
    repost_quote: Option<(Link, String)>,
}

pub(super) enum Action {
    Media(Vec<Media>),
    Fetch(String),
    Done,
}

pub(super) struct Engine {
    jobs: VecDeque<Job>,
    options: ExpansionOptions,
    allow_empty: bool,
    replies: ReplyMode,
    profile_owner: Option<String>,
}

impl Engine {
    pub(super) fn new(
        value: Value,
        id: String,
        options: ExpansionOptions,
        allow_empty: bool,
    ) -> Self {
        Self {
            jobs: VecDeque::from([Job {
                id,
                value: Some(value),
                path: Vec::new(),
                quote_depth: 0,
                quoted_by: None,
                repost: None,
                reposted_by: None,
                repost_quote: None,
            }]),
            options,
            allow_empty,
            replies: ReplyMode::All,
            profile_owner: None,
        }
    }

    pub(super) fn with_replies(mut self, replies: ReplyMode, owner: Option<String>) -> Self {
        self.replies = replies;
        self.profile_owner = owner;
        self
    }

    pub(super) fn supply(&mut self, bytes: &[u8]) -> Result<()> {
        let job = self
            .jobs
            .front_mut()
            .ok_or_else(|| error("Twitter linked post state missing"))?;
        job.value = Some(response_root(bytes, &job.id)?);
        Ok(())
    }

    pub(super) fn supply_value(&mut self, value: Value) -> Result<()> {
        let job = self
            .jobs
            .front_mut()
            .ok_or_else(|| error("Twitter linked post state missing"))?;
        validated(&value, &job.id)?;
        job.value = Some(value);
        Ok(())
    }

    pub(super) fn next(&mut self) -> Result<Action> {
        loop {
            let Some(mut job) = self.jobs.pop_front() else {
                return Ok(Action::Done);
            };
            if job.path.contains(&job.id) || job.path.len() > 16 {
                return Err(error(
                    "Twitter linked post cycle or traversal limit exceeded",
                ));
            }
            let Some(value) = job.value.take() else {
                let id = job.id.clone();
                self.jobs.push_front(job);
                return Ok(Action::Fetch(id));
            };
            let tweet = validated(&value, &job.id)?;
            job.path.push(job.id.clone());
            let is_retweet = tweet.get("retweeted_status_result").is_some()
                || tweet["legacy"].get("retweeted_status_result").is_some()
                || tweet["legacy"].get("retweeted_status_id_str").is_some();
            if is_retweet {
                if self.options.retweets == RetweetMode::Skip {
                    continue;
                }
                if self.options.retweets == RetweetMode::Repost
                    && self.options.quoted
                    && let Some(quote) =
                        link(tweet, "quoted_status_result", "quoted_status_id_str")?
                {
                    if job
                        .repost_quote
                        .as_ref()
                        .is_some_and(|(prior, _)| prior.id != quote.id)
                    {
                        return Err(error("Twitter linked post results conflict"));
                    }
                    if job.repost_quote.is_none() {
                        job.repost_quote = Some((quote, job.id.clone()));
                    }
                }
                let target = link(tweet, "retweeted_status_result", "retweeted_status_id_str")?
                    .ok_or_else(|| error("Twitter retweet result missing"))?;
                if self.options.retweets == RetweetMode::Repost && job.repost.is_none() {
                    job.repost = Some(tweet.clone());
                }
                if self.options.retweets == RetweetMode::Original && job.reposted_by.is_none() {
                    job.reposted_by = Some(job.id.clone());
                }
                job.id = target.id;
                job.value = target.value;
                self.jobs.push_front(job);
                continue;
            }
            let quote = if self.options.quoted {
                link(tweet, "quoted_status_result", "quoted_status_id_str")?
            } else {
                None
            };
            let (quote, quote_parent) = if let Some((outer_quote, parent)) = job.repost_quote {
                if quote.as_ref().is_some_and(|q| q.id != outer_quote.id) {
                    return Err(error("Twitter linked post results conflict"));
                }
                (Some(outer_quote), parent)
            } else {
                (
                    quote,
                    job.repost
                        .as_ref()
                        .and_then(|v| v["rest_id"].as_str())
                        .unwrap_or(&job.id)
                        .to_owned(),
                )
            };
            let has_quote = quote.is_some();
            if let Some(target) = quote {
                if job.quote_depth >= 8 {
                    return Err(error("Twitter quoted post depth limit exceeded"));
                }
                self.jobs.push_front(Job {
                    id: target.id,
                    value: target.value,
                    path: job.path.clone(),
                    quote_depth: job.quote_depth + 1,
                    quoted_by: Some(quote_parent),
                    repost: None,
                    reposted_by: None,
                    repost_quote: None,
                });
            }
            if !keep_reply(
                job.repost.as_ref().unwrap_or(tweet),
                self.replies,
                self.profile_owner.as_deref(),
            )? {
                return Ok(Action::Media(Vec::new()));
            }
            let mut media = parse_tweet_media(tweet, &job.id, self.allow_empty || has_quote)?;
            for item in &mut media {
                let metadata = item.metadata.as_mut().unwrap();
                metadata.quoted_by_id = job.quoted_by.clone();
                metadata.reposted_by_id = job.reposted_by.clone();
                if let Some(outer) = &job.repost {
                    let outer_id = outer["rest_id"].as_str().unwrap();
                    metadata.retweet_id = Some(job.id.clone());
                    metadata.post_id = outer_id.into();
                    metadata.username = outer
                        .pointer("/core/user_results/result/core/screen_name")
                        .or_else(|| outer.pointer("/core/user_results/result/legacy/screen_name"))
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    metadata.timestamp = outer
                        .pointer("/legacy/created_at")
                        .and_then(Value::as_str)
                        .and_then(timestamp);
                    let extension = item.filename.rsplit_once('.').unwrap().1;
                    item.filename = format!("{outer_id}_{}.{extension}", metadata.index);
                }
            }
            return Ok(Action::Media(media));
        }
    }
}

fn keep_reply(tweet: &Value, mode: ReplyMode, profile_owner: Option<&str>) -> Result<bool> {
    if mode == ReplyMode::All {
        return Ok(true);
    }
    let Some(recipient) = tweet
        .pointer("/legacy/in_reply_to_user_id_str")
        .filter(|v| !v.is_null())
    else {
        return Ok(true);
    };
    let recipient = recipient
        .as_str()
        .filter(|id| valid_id(id))
        .ok_or_else(|| error("Twitter reply recipient identifier invalid"))?;
    if mode == ReplyMode::Skip {
        return Ok(false);
    }
    let authors: Vec<_> = [
        tweet.pointer("/legacy/user_id_str"),
        tweet.pointer("/core/user_results/result/rest_id"),
    ]
    .into_iter()
    .flatten()
    .collect();
    let author = authors
        .first()
        .and_then(|v| v.as_str())
        .filter(|id| valid_id(id))
        .ok_or_else(|| error("Twitter reply author identifier missing or invalid"))?;
    if authors.iter().any(|v| v.as_str() != Some(author)) {
        return Err(error("Twitter reply author fields conflict"));
    }
    Ok(author == profile_owner.unwrap_or(recipient))
}

pub(super) enum OwnerLookup {
    Owner(String),
    Fetch(String),
}
pub(super) fn owner_lookup(
    value: &Value,
    mode: RetweetMode,
    known: &BTreeMap<String, Value>,
) -> Result<OwnerLookup> {
    let mut value = value.clone();
    let mut visited = std::collections::HashSet::new();
    for _ in 0..=16 {
        let tweet = unwrapped(&value)?;
        let id = tweet["rest_id"]
            .as_str()
            .filter(|id| valid_id(id))
            .ok_or_else(|| error("Twitter focal post identifier invalid"))?;
        validated(tweet, id)?;
        if !visited.insert(id.to_owned()) {
            return Err(error(
                "Twitter linked post cycle or traversal limit exceeded",
            ));
        }
        if mode == RetweetMode::Original
            && let Some(target) = link(tweet, "retweeted_status_result", "retweeted_status_id_str")?
        {
            if visited.len() > 16 {
                return Err(error("Twitter linked post traversal limit exceeded"));
            }
            match target.value.or_else(|| known.get(&target.id).cloned()) {
                Some(target) => value = target,
                None => return Ok(OwnerLookup::Fetch(target.id)),
            }
        } else {
            return Ok(OwnerLookup::Owner(detail_parse::post_owner(tweet)?));
        }
    }
    Err(error("Twitter linked post traversal limit exceeded"))
}

pub(super) enum ThreadLookup {
    Id(String),
    Bypass,
    Skip,
    Fetch(String),
}
pub(super) fn thread_lookup(
    value: &Value,
    mode: RetweetMode,
    known: &BTreeMap<String, Value>,
) -> Result<ThreadLookup> {
    let mut value = value.clone();
    let mut seen = std::collections::HashSet::new();
    for _ in 0..=16 {
        let tweet = unwrapped(&value)?;
        let id = tweet["rest_id"]
            .as_str()
            .filter(|id| valid_id(id))
            .ok_or_else(|| error("Twitter thread post identifier invalid"))?;
        validated(tweet, id)?;
        if !seen.insert(id.to_owned()) {
            return Err(error(
                "Twitter linked post cycle or traversal limit exceeded",
            ));
        }
        if mode == RetweetMode::Original
            && let Some(target) = link(tweet, "retweeted_status_result", "retweeted_status_id_str")?
        {
            if seen.len() > 16 {
                return Err(error("Twitter linked post traversal limit exceeded"));
            }
            match target.value.or_else(|| known.get(&target.id).cloned()) {
                Some(target) => value = target,
                None => return Ok(ThreadLookup::Fetch(target.id)),
            }
            continue;
        }
        return match tweet.pointer("/legacy/conversation_id_str") {
            None | Some(Value::Null) => Ok(ThreadLookup::Skip),
            Some(Value::Bool(false)) => Ok(ThreadLookup::Bypass),
            Some(Value::String(id)) if id.is_empty() => Ok(ThreadLookup::Skip),
            Some(Value::String(id)) if valid_id(id) => Ok(ThreadLookup::Id(id.clone())),
            _ => Err(error("Twitter conversation identifier invalid")),
        };
    }
    Err(error("Twitter linked post traversal limit exceeded"))
}
