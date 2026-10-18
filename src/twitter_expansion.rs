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
        }
    }

    pub(super) fn supply(&mut self, bytes: &[u8]) -> Result<()> {
        let job = self
            .jobs
            .front_mut()
            .ok_or_else(|| error("Twitter linked post state missing"))?;
        job.value = Some(response_root(bytes, &job.id)?);
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
