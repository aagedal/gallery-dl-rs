//! Experimental authenticated X posts and profile timelines.
//! GraphQL operation IDs/features are upstream snapshots, not a stable API.
//! Transaction bootstrap follows the upstream web client and may change without notice.
use crate::{
    Result,
    cookies::CookieJar,
    error,
    extract::{Media, PostMetadata, http_url},
    network::NetworkOptions,
    transport::{RateLimits, Redirects, retry_after},
};
#[path = "twitter_expansion.rs"]
mod expansion;
pub use expansion::{ExpansionOptions, RetweetMode, parse_response_with_options};
#[path = "twitter_search.rs"]
mod search;
pub use search::{
    SearchPagination, SearchProduct, TimelineOptions, TimelineStrategy, is_search_url,
};
#[path = "twitter_profile.rs"]
mod profile;
#[path = "twitter_timeline.rs"]
mod timeline;
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD},
};
use futures_util::{
    StreamExt, TryStreamExt,
    stream::{self, BoxStream},
};
use reqwest::{Client, StatusCode, header};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::Path,
    sync::Mutex,
    time::{Duration, SystemTime},
};
pub use timeline::{is_media_timeline_url, is_profile_timeline_url};
use url::Url;

const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
const MAX_RESPONSE: usize = 16 * 1024 * 1024;

/// Select which reply posts are emitted, following the upstream profile context.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ReplyMode {
    #[default]
    All,
    Skip,
    SelfOnly,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProfileOptions {
    pub pinned: bool,
    pub replies: ReplyMode,
}

pub struct Twitter {
    expansion: ExpansionOptions,
    profile_options: ProfileOptions,
    timeline_options: TimelineOptions,
    client: Client,
    network: NetworkOptions,
    cookies: Mutex<CookieJar>,
    timeout: Duration,
    request_timeout: Duration,
    retries: u8,
    rate_limits: RateLimits,
    transaction: Mutex<Option<Transaction>>,
    initialize: tokio::sync::Mutex<()>,
}

impl Twitter {
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
            expansion: ExpansionOptions::default(),
            profile_options: ProfileOptions::default(),
            timeline_options: TimelineOptions::default(),
            network: NetworkOptions::default(),
            cookies: Mutex::new(cookies),
            timeout,
            request_timeout: timeout,
            retries: 0,
            rate_limits: RateLimits::default(),
            transaction: Mutex::new(None),
            initialize: tokio::sync::Mutex::new(()),
        })
    }

    /// Select repost identity and opt into explicit quoted-post traversal.
    pub fn with_expansion(mut self, options: ExpansionOptions) -> Self {
        self.expansion = options;
        self
    }

    pub fn with_profile_options(mut self, options: ProfileOptions) -> Self {
        self.profile_options = options;
        self
    }

    pub fn with_timeline_options(mut self, options: TimelineOptions) -> Self {
        self.timeline_options = options;
        self
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
        let id = post_id(post)?;
        self.post_stream_at(endpoint(&id)?, id).try_collect().await
    }

    #[cfg(test)]
    async fn extract_endpoint(&self, endpoint: &Url, code: &str) -> Result<Vec<Media>> {
        self.post_stream_at(endpoint.clone(), code.into())
            .try_collect()
            .await
    }

    async fn ensure_transaction_at(&self, code: &str, auth_endpoint: &Url) -> Result<()> {
        let _guard = self.initialize.lock().await;
        if self
            .transaction
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|t| t.created.elapsed() < Duration::from_secs(10800))
        {
            return Ok(());
        }
        let scoped = self
            .cookies
            .lock()
            .unwrap()
            .header(auth_endpoint)
            .unwrap_or_default();
        for name in ["auth_token=", "ct0="] {
            if !scoped
                .split("; ")
                .any(|part| part.strip_prefix(name).is_some_and(|v| !v.is_empty()))
            {
                return Err(error(
                    "Twitter authentication requires x.com auth_token and ct0 cookies; refresh --cookies",
                ));
            }
        }
        self.bootstrap(code, &http_url("https://x.com/")?, None)
            .await
    }

    // Endpoint injection keeps bootstrap behavior testable with local fixtures.
    // Production always derives the fixed abs.twimg.com asset URL from HTML.
    async fn bootstrap(&self, code: &str, home: &Url, script_override: Option<&Url>) -> Result<()> {
        let mut homepage = self.fetch(home, code, false).await?;
        let mut html = std::str::from_utf8(&homepage)
            .map_err(|_| error("Twitter transaction homepage is not UTF-8"))?;
        // The new logged-out application no longer contains the webpack map.
        // X's first-party /i/jf/ shell still exposes verification/SVG/ondemand
        // data. Derive its current hash; never execute downloaded JavaScript.
        if Transaction::script_url(html).is_err() && Transaction::module_entry_url(html).is_some() {
            let fallback = home
                .join("/i/jf/")
                .map_err(|_| error("Twitter transaction fallback URL is invalid"))?;
            homepage = self.fetch(&fallback, code, false).await?;
            html = std::str::from_utf8(&homepage)
                .map_err(|_| error("Twitter transaction homepage is not UTF-8"))?;
        }
        let js = Transaction::script_url(html)?;
        let script = self
            .fetch(script_override.unwrap_or(&js), code, false)
            .await?;
        let script = std::str::from_utf8(&script)
            .map_err(|_| error("Twitter transaction script is not UTF-8"))?;
        *self.transaction.lock().unwrap() = Some(Transaction::parse(html, script)?);
        Ok(())
    }

    async fn fetch(&self, endpoint: &Url, code: &str, api: bool) -> Result<Vec<u8>> {
        tokio::time::timeout(self.timeout, async {
            for attempt in 0..=self.retries {
                match self.attempt(endpoint, code, attempt, api).await {
                    Ok(media) => return Ok(media),
                    Err(failure) if failure.retry && attempt < self.retries => {
                        // Transport failures have no response to establish a cooldown.
                        // Also yield between attempts when Retry-After is zero.
                        tokio::time::sleep(backoff(attempt)).await;
                    }
                    Err(failure) => return Err(failure.error),
                }
            }
            unreachable!()
        })
        .await
        .map_err(|_| error("native Twitter extraction timed out"))?
    }

    async fn attempt(
        &self,
        endpoint: &Url,
        code: &str,
        attempt: u8,
        api: bool,
    ) -> std::result::Result<Vec<u8>, Failure> {
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
                    if api {
                        "application/json"
                    } else if target.host_str() == Some("abs.twimg.com") {
                        "*/*"
                    } else {
                        "text/html"
                    },
                )
                .header(
                    header::REFERER,
                    if code.is_empty() {
                        "https://x.com/".to_owned()
                    } else {
                        format!("https://x.com/i/web/status/{code}")
                    },
                );
            if api {
                request = request
                    .header(header::AUTHORIZATION, BEARER)
                    .header("x-twitter-auth-type", "OAuth2Session")
                    .header("x-twitter-active-user", "yes")
                    .header("x-twitter-client-language", "en");
                let scoped = self.cookies.lock().unwrap().header(&target)
                .ok_or_else(|| error("Twitter authentication requires scoped auth_token and ct0 cookies; refresh --cookies"))?;
                let cookie = |name: &str| {
                    scoped
                        .split("; ")
                        .find_map(|part| part.strip_prefix(name))
                        .filter(|value| !value.is_empty())
                };
                if cookie("auth_token=").is_none() {
                    return Err(error("Twitter authentication requires scoped auth_token and ct0 cookies; refresh --cookies").into());
                }
                let csrf = cookie("ct0=").ok_or_else(|| error("Twitter authentication requires scoped auth_token and ct0 cookies; refresh --cookies"))?;
                request = request
                    .header("x-csrf-token", csrf)
                    .header(header::COOKIE, scoped);
                if let Some(transaction) = self.transaction.lock().unwrap().as_ref() {
                    request = request.header(
                        "x-client-transaction-id",
                        transaction.generate("GET", target.path())?,
                    );
                } else {
                    return Err(error("Twitter client transaction has not been initialized").into());
                }
            } else if target.host_str() != Some("abs.twimg.com")
                && let Some(cookies) = self.cookies.lock().unwrap().header(&target)
            {
                request = request.header(header::COOKIE, cookies);
            }
            let response = request
                .send()
                .await
                .map_err(|_| Failure::transient("Twitter request failed; check network access"))?;
            self.cookies
                .lock()
                .unwrap()
                .store_response(response.url(), response.headers());
            match redirects.next(response.url(), response.status(), response.headers())? {
                Some(next) => {
                    // API normalization is allowed; login/challenge pages are terminal.
                    if api && !next.path().starts_with("/i/api/graphql/") {
                        return Err(error("Twitter redirected outside the API; login or a session challenge may be required").into());
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
                            "Twitter rate limit; retry after {} seconds",
                            delay.as_secs()
                        ),
                        None => "Twitter rate limit; retry later".into(),
                    }
                } else {
                    format!("Twitter returned HTTP {status}")
                },
            ));
        }
        if matches!(status.as_u16(), 401 | 403) {
            return Err(error(
                "Twitter authentication required or session expired; refresh --cookies",
            )
            .into());
        }
        if matches!(status.as_u16(), 404 | 410) {
            return Err(error("Twitter post unavailable or inaccessible to this session").into());
        }
        if !status.is_success() {
            return Err(error(format!("Twitter returned HTTP {status}")).into());
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| Failure::transient("could not read Twitter response"))?
        {
            if body.len() + chunk.len() > MAX_RESPONSE {
                return Err(error("Twitter response exceeded 16 MiB").into());
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }
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

// Public web-client token, not a user session credential.
const BEARER: &str = "Bearer AAAAAAAAAAAAAAAAAAAAANRILgAAAAAAnNwIzUejRCOuH5E6I8xnZz4puTs%3D1Zv7ttfk8LF81IUq16cHjhLTvJu4FA33AGWWjCpTnA";

fn endpoint(id: &str) -> Result<Url> {
    let mut url =
        http_url("https://x.com/i/api/graphql/qxWQxcMLiTPcavz9Qy5hwQ/TweetResultByRestId")?;
    let variables = serde_json::json!({"tweetId":id,"withCommunity":false,"includePromotedContent":false,"withVoice":false});
    url.query_pairs_mut()
        .append_pair("variables", &variables.to_string())
        .append_pair("features", &tweet_features().to_string())
        .append_pair("fieldToggles", &serde_json::json!({"withArticleRichContentState":true,"withArticlePlainText":false,"withGrokAnalyze":false,"withDisallowedReplyControls":false}).to_string());
    Ok(url)
}

fn tweet_features() -> Value {
    let mut features = serde_json::Map::new();
    for name in [
        "profile_label_improvements_pcf_label_in_post_enabled",
        "rweb_tipjar_consumption_enabled",
        "creator_subscriptions_tweet_preview_api_enabled",
        "responsive_web_graphql_timeline_navigation_enabled",
        "communities_web_enable_tweet_community_results_fetch",
        "c9s_tweet_anatomy_moderator_badge_enabled",
        "responsive_web_grok_analyze_post_followups_enabled",
        "responsive_web_jetfuel_frame",
        "responsive_web_grok_share_attachment_enabled",
        "articles_preview_enabled",
        "responsive_web_edit_tweet_api_enabled",
        "graphql_is_translatable_rweb_tweet_is_translatable_enabled",
        "view_counts_everywhere_api_enabled",
        "longform_notetweets_consumption_enabled",
        "responsive_web_twitter_article_tweet_consumption_enabled",
        "responsive_web_grok_analysis_button_from_backend",
        "freedom_of_speech_not_reach_fetch_enabled",
        "standardized_nudges_misinfo",
        "tweet_with_visibility_results_prefer_gql_limited_actions_policy_enabled",
        "longform_notetweets_rich_text_read_enabled",
        "longform_notetweets_inline_media_enabled",
        "responsive_web_grok_image_annotation_enabled",
        "responsive_web_grok_imagine_annotation_enabled",
    ] {
        features.insert(name.into(), true.into());
    }
    for name in [
        "payments_enabled",
        "rweb_xchat_enabled",
        "verified_phone_label_enabled",
        "responsive_web_graphql_skip_user_profile_image_extensions_enabled",
        "premium_content_api_read_enabled",
        "responsive_web_grok_analyze_button_fetch_trends_enabled",
        "tweet_awards_web_tipping_enabled",
        "responsive_web_grok_show_grok_translated_post",
        "creator_subscriptions_quote_tweet_preview_enabled",
        "responsive_web_grok_community_note_auto_translation_is_enabled",
        "responsive_web_enhance_cards_enabled",
    ] {
        features.insert(name.into(), false.into());
    }
    Value::Object(features)
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 20
        && id.bytes().all(|b| b.is_ascii_digit())
        && id.parse::<u64>().is_ok_and(|n| n > 0)
        && !id.starts_with('0')
}

/// Parse first-party single-post URLs. `Twitter::stream` also handles explicit profile tabs.
pub fn post_id(url: &Url) -> Result<String> {
    if !matches!(url.scheme(), "http" | "https")
        || !matches!(
            url.host_str(),
            Some(
                "x.com"
                    | "www.x.com"
                    | "mobile.x.com"
                    | "twitter.com"
                    | "www.twitter.com"
                    | "mobile.twitter.com"
            )
        )
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return Err(error(
            "native Twitter extraction requires a first-party single-post URL",
        ));
    }
    let p: Vec<_> = url.path().trim_end_matches('/').split('/').collect();
    let id = match p.as_slice() {
        ["", user, "status", id] if !user.is_empty() => *id,
        ["", "i", "web", "status", id] => *id,
        ["", user, "status", id, kind, n]
            if !user.is_empty()
                && matches!(*kind, "photo" | "video")
                && n.parse::<u8>().is_ok_and(|n| n > 0) =>
        {
            *id
        }
        _ => {
            return Err(error(
                "native Twitter supports single posts; use --extractor compatibility for other URLs",
            ));
        }
    };
    if !valid_id(id) {
        return Err(error("invalid Twitter post identifier"));
    }
    Ok(id.into())
}

fn check_errors(data: &Value) -> Result<()> {
    if let Some(errors) = data
        .get("errors")
        .and_then(Value::as_array)
        .filter(|e| !e.is_empty())
    {
        let has = |codes: &[u64]| {
            errors.iter().any(|e| {
                e.get("code")
                    .and_then(Value::as_u64)
                    .is_some_and(|c| codes.contains(&c))
            })
        };
        return Err(error(if has(&[88]) {
            "Twitter rate limit; retry later"
        } else if has(&[32, 89, 99, 135, 215, 239, 353]) {
            "Twitter authentication required or session expired; refresh --cookies"
        } else if has(&[179]) {
            "Twitter protected post; this session cannot access it"
        } else if has(&[34, 63, 144]) {
            "Twitter post unavailable or inaccessible to this session"
        } else {
            "Twitter API rejected this request; session or client transaction challenge may be required"
        }));
    }
    Ok(())
}

/// Parse the requested root with upstream-default expansion settings.
pub fn parse_response(bytes: &[u8], requested_post_id: &str) -> Result<Vec<Media>> {
    parse_response_with_options(bytes, requested_post_id, ExpansionOptions::default())
}

fn parse_tweet_media(
    tweet: &Value,
    requested_post_id: &str,
    allow_empty: bool,
) -> Result<Vec<Media>> {
    let legacy = &tweet["legacy"];
    let items = match legacy.get("extended_entities") {
        Some(extended) => extended
            .get("media")
            .and_then(Value::as_array)
            .ok_or_else(|| error("Twitter post media fields malformed"))?,
        None => {
            if legacy
                .pointer("/entities/media")
                .is_some_and(|v| !v.as_array().is_some_and(Vec::is_empty))
            {
                return Err(error("Twitter post extended media missing"));
            }
            if allow_empty {
                return Ok(Vec::new());
            }
            return Err(error("Twitter post has no downloadable media"));
        }
    };
    if items.is_empty() && !allow_empty {
        return Err(error("Twitter post has no downloadable media"));
    }
    let username = tweet
        .pointer("/core/user_results/result/core/screen_name")
        .or_else(|| tweet.pointer("/core/user_results/result/legacy/screen_name"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let caption = tweet
        .pointer("/note_tweet/note_tweet_results/result/text")
        .or_else(|| legacy.get("full_text"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let timestamp = legacy
        .get("created_at")
        .and_then(Value::as_str)
        .and_then(timestamp);
    items.iter().enumerate().map(|(i,item)| {
        if item.pointer("/ext_media_availability/status").and_then(Value::as_str) == Some("Unavailable") { return Err(error("Twitter media is unavailable")); }
        let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
        let (url, extension) = if matches!(kind,"video"|"animated_gif") {
            let (url, _) = item.pointer("/video_info/variants").and_then(Value::as_array).into_iter().flatten()
                .filter(|v| v.get("content_type").and_then(Value::as_str) == Some("video/mp4"))
                .filter_map(|v| {
                    let url = v.get("url").and_then(Value::as_str).and_then(|s| http_url(s).ok())?;
                    if !url.path().ends_with(".mp4") { return None; }
                    Some((url, v.get("bitrate").and_then(Value::as_u64).unwrap_or(0)))
                }).max_by_key(|(_,rate)| *rate).ok_or_else(|| error("Twitter video has no direct MP4 variant; thumbnail fallback is disabled"))?;
            (url,"mp4".to_string())
        } else if kind == "photo" {
            let mut url = item.get("media_url_https").or_else(|| item.get("media_url")).and_then(Value::as_str).ok_or_else(|| error("Twitter photo URL missing")).and_then(http_url).map_err(|_| error("Twitter photo URL invalid"))?;
            let extension = url.path().rsplit_once('.').map(|(_,ext)| ext).filter(|ext| matches!(*ext,"jpg"|"jpeg"|"png"|"webp"|"avif")).map(str::to_owned)
                .or_else(|| url.query_pairs().find(|(k,v)| k=="format" && matches!(v.as_ref(),"jpg"|"jpeg"|"png"|"webp"|"avif")).map(|(_,v)|v.into_owned()))
                .ok_or_else(|| error("Twitter photo has unsupported extension"))?;
            // Preserve signed/other query parameters, replacing only the size.
            let pairs: Vec<_> = url.query_pairs().filter(|(k,_)| k != "name").map(|(k,v)|(k.into_owned(),v.into_owned())).collect();
            url.set_query(None);
            url.query_pairs_mut().extend_pairs(pairs).append_pair("name","orig");
            (url,extension)
        } else { return Err(error("unsupported Twitter media type")); };
        Ok(Media { url, job: crate::extract::MediaJob::DirectHttp, handoff: BTreeMap::new(), category: "twitter".into(), filename: format!("{requested_post_id}_{}.{extension}",i+1),
            metadata: Some(PostMetadata {retweet_id:None,quoted_by_id:None,reposted_by_id:None,post_id:requested_post_id.into(),media_id:(i+1).to_string(),shortcode:String::new(),username:username.clone(),caption:caption.clone(),timestamp,index:i+1}),
            headers: BTreeMap::from([("User-Agent".into(),USER_AGENT.into()),("Referer".into(),"https://x.com/".into())]) })
    }).collect()
}

// Twitter dates use a fixed English month and UTC offset; httpdate does not
// accept this layout, so normalize only the documented UTC form to IMF-fixdate.
fn timestamp(date: &str) -> Option<i64> {
    let p: Vec<_> = date.split_ascii_whitespace().collect();
    if p.len() != 6 || p[4] != "+0000" {
        return None;
    }
    let date = format!("{}, {} {} {} {} GMT", p[0], p[2], p[1], p[5], p[3]);
    httpdate::parse_http_date(&date)
        .ok()?
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?
        .as_secs()
        .try_into()
        .ok()
}

// Native port of gallery-dl's twitter_transaction_id.py algorithm (GPL-2.0+),
// Copyright 2025-2026 Mike Fährmann; originally XClientTransaction by
// iSarabjitDhiman. https://github.com/mikf/gallery-dl/blob/master/gallery_dl/extractor/utils/twitter_transaction_id.py
// Parsing is intentionally bounded and fail-closed when the web bundle changes.
struct Transaction {
    key: Vec<u8>,
    animation: String,
    created: std::time::Instant,
}

impl Transaction {
    fn module_entry_url(home: &str) -> Option<Url> {
        home.split("<script")
            .skip(1)
            .filter_map(|part| part.split_once('>').map(|(tag, _)| tag))
            .filter(|tag| tag.contains("type=\"module\""))
            .filter_map(|tag| between(tag, "src=\"", "\"").and_then(|src| Url::parse(src).ok()))
            .find(|url| {
                url.scheme() == "https"
                    && url.host_str() == Some("abs.twimg.com")
                    && url.port().is_none()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none()
                    && url.path().starts_with("/x-web/x-web/entry-client-")
                    && url.path().ends_with(".js")
                    && url.path().rsplit('/').next().is_some_and(|file| {
                        file.bytes()
                            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
                    })
            })
    }

    fn script_url(home: &str) -> Result<Url> {
        let invalid = || error("Twitter client transaction bootstrap changed: missing script");
        let pos = home.find("\"ondemand.s\"").ok_or_else(invalid)?;
        let prefix = &home[..pos];
        let key = prefix
            .rsplit_once(',')
            .and_then(|(_, s)| s.split_once(':'))
            .map(|(s, _)| s.trim())
            .ok_or_else(invalid)?;
        if key.is_empty() || !key.bytes().all(|b| b.is_ascii_digit()) {
            return Err(invalid());
        }
        let hash = between(&home[pos..], &format!("{key}:\""), "\"").ok_or_else(invalid)?;
        if hash.is_empty() || hash.len() > 128 || !hash.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err(invalid());
        }
        http_url(&format!(
            "https://abs.twimg.com/responsive-web/client-web/ondemand.s.{hash}a.js"
        ))
    }

    fn parse(home: &str, js: &str) -> Result<Self> {
        let invalid = || {
            error(
                "Twitter client transaction bootstrap changed: invalid verification or animation data",
            )
        };
        let pos = home
            .find("name=\"twitter-site-verification\"")
            .ok_or_else(invalid)?;
        let start = home[..pos].rfind('<').ok_or_else(invalid)?;
        let end = home[pos..].find('>').map(|n| pos + n).ok_or_else(invalid)?;
        let key = between(&home[start..end], "content=\"", "\"")
            .and_then(|text| STANDARD.decode(text).ok())
            .filter(|k| k.len() >= 6 && k.len() <= 128)
            .ok_or_else(invalid)?;
        let mut indices = Vec::new();
        // Equivalent to upstream `(\w\[(\d\d?)\],\s*16)`.
        for piece in js.split('(').skip(1) {
            let b = piece.as_bytes();
            if b.len() < 6 || !(b[0].is_ascii_alphanumeric() || b[0] == b'_') || b[1] != b'[' {
                continue;
            }
            let Some((index, tail)) = piece[2..].split_once("],") else {
                continue;
            };
            if !(1..=2).contains(&index.len())
                || !index.bytes().all(|b| b.is_ascii_digit())
                || !tail.trim_start().starts_with("16)")
            {
                continue;
            }
            let index = index.parse::<usize>().map_err(|_| invalid())?;
            if index >= key.len() || indices.len() >= 16 {
                return Err(invalid());
            }
            indices.push(index);
        }
        if indices.len() < 2 {
            return Err(invalid());
        }
        let frame = home
            .split("id=\"loading-x-anim-")
            .skip(1)
            .nth((key[5] % 4) as usize)
            .and_then(|s| s.split_once("</svg>"))
            .map(|(s, _)| s)
            .ok_or_else(invalid)?;
        let path = between(frame, "</path><path d=\"", "\"")
            .and_then(|s| s.get(9..))
            .ok_or_else(invalid)?;
        let row = path
            .split('C')
            .nth((key[indices[0]] % 16) as usize)
            .ok_or_else(invalid)?;
        let row: Vec<f64> = row
            .split(|c: char| !c.is_ascii_digit())
            .filter(|s| !s.is_empty())
            .map(|s| s.parse::<f64>().unwrap_or(f64::NAN))
            .collect();
        if row.len() != 11 || row.iter().any(|n| !n.is_finite() || *n > 255.0) {
            return Err(invalid());
        }
        let mut time = 1u64;
        for index in &indices[1..] {
            time = time
                .checked_mul(u64::from(key[*index] % 16))
                .ok_or_else(invalid)?;
        }
        let target = ((time as f64 / 10.0).round() * 10.0) / 4096.0;
        let animation = animation(&row, target);
        Ok(Self {
            key,
            animation,
            created: std::time::Instant::now(),
        })
    }

    fn generate(&self, method: &str, path: &str) -> Result<String> {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_err(|_| error("Twitter transaction clock is invalid"))?;
        let seconds = now
            .as_secs()
            .checked_sub(1682924400)
            .ok_or_else(|| error("Twitter transaction clock predates protocol epoch"))?;
        // The protocol's mask is obfuscation, not a cryptographic secret.
        // RandomState supplies a platform-independent random per-instance seed.
        use std::hash::BuildHasher;
        let nibble = std::collections::hash_map::RandomState::new().hash_one(now) as u8 & 0xf0;
        Ok(self.generate_at(
            method,
            path,
            seconds,
            nibble | ((now.subsec_nanos() as u64 * 16 / 1_000_000_000) as u8),
        ))
    }

    fn generate_at(&self, method: &str, path: &str, seconds: u64, mask: u8) -> String {
        use sha2::{Digest, Sha256};
        let payload = format!("{method}!{path}!{seconds}obfiowerehiring{}", self.animation);
        let hash = Sha256::digest(payload.as_bytes());
        let bytes = std::iter::once(0)
            .chain(self.key.iter().copied())
            .chain((seconds as u32).to_le_bytes())
            .chain(hash[..16].iter().copied())
            .chain(std::iter::once(3))
            .map(|b| b ^ mask)
            .collect::<Vec<_>>();
        STANDARD_NO_PAD.encode(&bytes)
    }
}

fn between<'a>(s: &'a str, start: &str, end: &str) -> Option<&'a str> {
    s.split_once(start)?.1.split_once(end).map(|(s, _)| s)
}

// Decimal rounding matches Python round(x, 2), including binary half cases.
fn round_two(value: f64) -> f64 {
    format!("{value:.2}").parse().unwrap()
}

fn animation(row: &[f64], t: f64) -> String {
    let curve: Vec<_> = row[7..]
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let min = if i % 2 == 0 { 0.0 } else { -1.0 };
            round_two(v * (1.0 - min) / 255.0 + min)
        })
        .collect();
    let cubic = |a: f64, b: f64, m: f64| {
        3.0 * a * (1.0 - m).powi(2) * m + 3.0 * b * (1.0 - m) * m * m + m * m * m
    };
    let x = if t <= 0.0 {
        let slope = if curve[0] > 0.0 {
            curve[1] / curve[0]
        } else if curve[1] == 0.0 && curve[2] > 0.0 {
            curve[3] / curve[2]
        } else {
            0.0
        };
        slope * t
    } else if t >= 1.0 {
        let slope = if curve[2] < 1.0 {
            (curve[3] - 1.0) / (curve[2] - 1.0)
        } else if curve[2] == 1.0 && curve[0] < 1.0 {
            (curve[1] - 1.0) / (curve[0] - 1.0)
        } else {
            0.0
        };
        1.0 + slope * (t - 1.0)
    } else {
        let (mut low, mut high) = (0.0, 1.0);
        let mut result = 0.0;
        for _ in 0..100 {
            let mid = (low + high) / 2.0;
            let estimate = cubic(curve[0], curve[2], mid);
            result = cubic(curve[1], curve[3], mid);
            if (t - estimate).abs() < 0.00001 {
                break;
            }
            if estimate < t {
                low = mid;
            } else {
                high = mid;
            }
        }
        result
    };
    let mut out = String::new();
    for i in 0..3 {
        out.push_str(&format!(
            "{:x}",
            (row[i] * (1.0 - x) + row[i + 3] * x)
                .clamp(0.0, 255.0)
                .round_ties_even() as u64
        ));
    }
    let radians = (x * (row[6] * 300.0 / 255.0 + 60.0).floor()).to_radians();
    for value in [radians.cos(), -radians.sin(), radians.sin(), radians.cos()] {
        let value = round_two(value).abs();
        out.push_str(&format!("{:x}", value as u64));
        let mut fraction = value.fract();
        for _ in 0..64 {
            if fraction == 0.0 {
                break;
            }
            fraction *= 16.0;
            let digit = fraction as u32;
            out.push(char::from_digit(digit, 16).unwrap());
            fraction -= digit as f64;
        }
    }
    out.push_str("00");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    const HOME: &str = include_str!("../tests/fixtures/twitter/transaction-home.html");
    const SCRIPT: &str = include_str!("../tests/fixtures/twitter/transaction-script.js");
    const PHOTO: &[u8] = include_bytes!("../tests/fixtures/twitter/photo.json");
    const ID: &str = "1234567890";

    #[tokio::test]
    #[ignore = "manual public-network bootstrap check; does not validate authenticated extraction"]
    async fn live_public_transaction_bootstrap() {
        let native = Twitter::new(None, Duration::from_secs(30))
            .await
            .unwrap()
            .with_request_timeout(Duration::from_secs(10));
        native
            .bootstrap(ID, &http_url("https://x.com/").unwrap(), None)
            .await
            .unwrap();
        let transaction = native.transaction.lock().unwrap();
        let transaction = transaction.as_ref().unwrap();
        assert!(!transaction.animation.is_empty());
        assert!(
            !transaction
                .generate("GET", endpoint(ID).unwrap().path())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn transaction_matches_deterministic_upstream_python_reference() {
        let vector: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/twitter/transaction-vector.json"
        ))
        .unwrap();
        let transaction = Transaction::parse(HOME, SCRIPT).unwrap();
        assert_eq!(transaction.animation, vector["animation"].as_str().unwrap());
        assert_eq!(
            transaction.generate_at("GET", vector["path"].as_str().unwrap(), 1000, 164),
            vector["transaction"].as_str().unwrap()
        );
        assert_eq!(
            Transaction::script_url(HOME).unwrap().as_str(),
            "https://abs.twimg.com/responsive-web/client-web/ondemand.s.abc123a.js"
        );
    }

    #[test]
    fn animation_matches_reference_curves_and_decimal_rounding() {
        let vectors: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/twitter/animation-vectors.json"
        ))
        .unwrap();
        for vector in vectors.as_array().unwrap() {
            let row: Vec<f64> = vector["row"]
                .as_array()
                .unwrap()
                .iter()
                .map(|n| n.as_f64().unwrap())
                .collect();
            assert_eq!(
                animation(&row, vector["time"].as_f64().unwrap()),
                vector["animation"].as_str().unwrap(),
                "{vector}"
            );
        }
    }

    #[test]
    fn transaction_bundle_changes_fail_without_private_payload() {
        for home in [
            "secret",
            &HOME.replace("twitter-site-verification", "missing"),
            &HOME.replace("30,60", "30"),
            &HOME.replace("AAEC", "!!!!"),
        ] {
            assert!(Transaction::parse(home, SCRIPT).is_err());
        }
        for script in ["secret", "(a[99],16);(b[3],16)", "(a[3],16)"] {
            assert!(Transaction::parse(HOME, script).is_err());
        }
        assert!(Transaction::script_url(&HOME.replace("abc123", "../evil")).is_err());
    }

    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };
    fn response(status: &str, headers: &str, body: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n",
            body.len()
        )
        .into_bytes();
        out.extend_from_slice(body);
        out
    }
    fn serve(responses: Vec<(Vec<u8>, Duration)>) -> (Url, thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = http_url(&format!(
            "http://{}/i/api/graphql/query/TweetResultByRestId",
            listener.local_addr().unwrap()
        ))
        .unwrap();
        let server = thread::spawn(move || {
            let mut requests = Vec::new();
            for (response, delay) in responses {
                let deadline = std::time::Instant::now() + Duration::from_secs(4);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((s, _)) => break s,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(std::time::Instant::now() < deadline, "server timed out");
                            thread::sleep(Duration::from_millis(2));
                        }
                        Err(e) => panic!("{e}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = Vec::new();
                let mut buf = [0; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = stream.read(&mut buf).unwrap();
                    assert!(n > 0);
                    request.extend_from_slice(&buf[..n]);
                    assert!(request.len() < 65536);
                }
                requests.push(String::from_utf8(request).unwrap());
                thread::sleep(delay);
                let _ = stream.write_all(&response);
            }
            requests
        });
        (endpoint, server)
    }
    async fn native() -> Twitter {
        let native = Twitter::new(None, Duration::from_secs(3)).await.unwrap();
        *native.cookies.lock().unwrap()=CookieJar::parse("127.0.0.1\tFALSE\t/\tFALSE\t0\tauth_token\tsession\n127.0.0.1\tFALSE\t/\tFALSE\t0\tct0\toriginal\n").unwrap();
        *native.transaction.lock().unwrap() = Some(Transaction::parse(HOME, SCRIPT).unwrap());
        native
    }
    #[tokio::test]
    async fn configured_user_agent_matches_api_and_media_requests() {
        let (endpoint, server) = serve(vec![(response("200 OK", "", PHOTO), Duration::ZERO)]);
        let network = NetworkOptions::new(None, Some("gallery-dl-native-test/1".into())).unwrap();
        let native = native().await.with_network(&network).unwrap();
        let media = native.extract_endpoint(&endpoint, ID).await.unwrap();
        assert!(!media.is_empty());
        for item in media {
            assert_eq!(
                item.headers.get("User-Agent").map(String::as_str),
                Some("gallery-dl-native-test/1")
            );
        }
        let requests = server.join().unwrap();
        assert!(
            requests[0]
                .to_ascii_lowercase()
                .contains("user-agent: gallery-dl-native-test/1\r\n")
        );
    }

    #[tokio::test]
    async fn api_requests_use_configured_http_proxy() {
        let (proxy, server) = serve(vec![(response("200 OK", "", PHOTO), Duration::ZERO)]);
        let network =
            NetworkOptions::new(Some(proxy.origin().ascii_serialization()), None).unwrap();
        let native = native().await.with_network(&network).unwrap();
        *native.cookies.lock().unwrap() = CookieJar::parse("native-api.invalid\tFALSE\t/\tFALSE\t0\tauth_token\tsession\nnative-api.invalid\tFALSE\t/\tFALSE\t0\tct0\toriginal\n").unwrap();
        let mut endpoint = proxy;
        endpoint.set_host(Some("native-api.invalid")).unwrap();
        endpoint.set_port(None).unwrap();
        let media = native.extract_endpoint(&endpoint, ID).await.unwrap();
        assert!(!media.is_empty());
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with(&format!("GET {endpoint} HTTP/1.1\r\n")));
    }

    #[tokio::test]
    async fn scope_rotations_retries_and_media_exclude_session_credentials() {
        let (endpoint, server) = serve(vec![
            (
                response(
                    "302 Found",
                    "Location: /i/api/graphql/next\r\nSet-Cookie: ct0=redirected; Path=/\r\nSet-Cookie: scoped=private; Path=/i/api/graphql/next\r\nSet-Cookie: foreign=secret; Domain=evil.test; Path=/\r\n",
                    b"",
                ),
                Duration::ZERO,
            ),
            (
                response(
                    "429 Too Many Requests",
                    "Retry-After: 0\r\nSet-Cookie: ct0=retried; Path=/\r\n",
                    b"",
                ),
                Duration::ZERO,
            ),
            (response("200 OK", "", PHOTO), Duration::ZERO),
        ]);
        let native = native().await.with_retries(1);
        let media = native.extract_endpoint(&endpoint, ID).await.unwrap();
        assert_eq!(media.len(), 1);
        let json = serde_json::to_string(&media).unwrap();
        for secret in ["session", "original", "retried", "private"] {
            assert!(!json.contains(secret));
        }
        assert_eq!(media[0].headers.len(), 2);
        let requests = server.join().unwrap();
        for (i, csrf) in ["original", "redirected", "retried"].iter().enumerate() {
            let r = requests[i].to_lowercase();
            assert!(r.contains(&format!("x-csrf-token: {csrf}\r\n")));
            assert!(r.contains("authorization: bearer "));
            assert!(r.contains("x-client-transaction-id: "));
            assert!(r.contains("auth_token=session"));
            assert!(!r.contains("foreign="));
            assert_eq!(r.contains("scoped=private"), i == 1);
        }
        assert!(requests[0].starts_with("GET /i/api/graphql/query/"));
        assert!(requests[2].starts_with("GET /i/api/graphql/query/"));
    }
    #[tokio::test]
    async fn retries_transport_and_server_failures_and_bounds_each_request() {
        for first in [
            response("503 Service Unavailable", "Retry-After: 0\r\n", b""),
            Vec::new(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{".to_vec(),
        ] {
            let (endpoint, server) = serve(vec![
                (first, Duration::ZERO),
                (response("200 OK", "", PHOTO), Duration::ZERO),
            ]);
            assert_eq!(
                native()
                    .await
                    .with_retries(1)
                    .extract_endpoint(&endpoint, ID)
                    .await
                    .unwrap()
                    .len(),
                1
            );
            assert_eq!(server.join().unwrap().len(), 2);
        }
        let (endpoint, server) = serve(vec![(
            response("200 OK", "", PHOTO),
            Duration::from_millis(150),
        )]);
        assert!(
            native()
                .await
                .with_request_timeout(Duration::from_millis(20))
                .extract_endpoint(&endpoint, ID)
                .await
                .unwrap_err()
                .to_string()
                .contains("request failed")
        );
        server.join().unwrap();
    }
    #[tokio::test]
    async fn terminal_errors_redact_redirect_payload_and_do_not_retry() {
        for (status, headers, body, expected) in [
            (
                "401 Unauthorized",
                "",
                b"secret".as_slice(),
                "authentication",
            ),
            ("404 Not Found", "", b"secret".as_slice(), "unavailable"),
            (
                "302 Found",
                "Location: /login?secret=hidden\r\n",
                b"".as_slice(),
                "outside",
            ),
            (
                "302 Found",
                "Location: https://evil.test/secret\r\n",
                b"".as_slice(),
                "same origin",
            ),
            ("200 OK", "", b"secret".as_slice(), "invalid JSON"),
        ] {
            let (endpoint, server) = serve(vec![(response(status, headers, body), Duration::ZERO)]);
            let err = native()
                .await
                .with_retries(2)
                .extract_endpoint(&endpoint, ID)
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains(expected), "{err}");
            assert!(!err.contains("secret"));
            assert_eq!(server.join().unwrap().len(), 1);
        }
    }
    #[tokio::test]
    async fn overall_timeout_and_dropped_future_stop_retries() {
        for cancel in [false, true] {
            let (endpoint, server) = serve(vec![(
                response("429 Too Many Requests", "Retry-After: 5\r\n", b""),
                Duration::ZERO,
            )]);
            let mut native = native().await.with_retries(2);
            native.timeout = Duration::from_millis(100);
            if cancel {
                let _ = tokio::time::timeout(
                    Duration::from_millis(30),
                    native.extract_endpoint(&endpoint, ID),
                )
                .await;
            } else {
                assert!(native.extract_endpoint(&endpoint, ID).await.is_err());
            }
            assert_eq!(server.join().unwrap().len(), 1);
        }
    }
    #[tokio::test]
    async fn modern_module_home_falls_back_to_first_party_legacy_shell() {
        let module = include_bytes!("../tests/fixtures/twitter/transaction-module-home.html");
        let (endpoint, server) = serve(vec![
            (response("200 OK", "", module), Duration::ZERO),
            (
                response(
                    "200 OK",
                    "Set-Cookie: ct0=fallback-rotated; Path=/\r\n",
                    HOME.as_bytes(),
                ),
                Duration::ZERO,
            ),
            (response("200 OK", "", SCRIPT.as_bytes()), Duration::ZERO),
            (response("200 OK", "", PHOTO), Duration::ZERO),
        ]);
        let native = native().await;
        *native.transaction.lock().unwrap() = None;
        let mut home = endpoint.clone();
        home.set_path("/");
        let mut script = endpoint.clone();
        script.set_path("/fixture-script.js");
        native.bootstrap(ID, &home, Some(&script)).await.unwrap();
        assert_eq!(
            native.extract_endpoint(&endpoint, ID).await.unwrap().len(),
            1
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET / HTTP/1.1"));
        assert!(requests[1].starts_with("GET /i/jf/ HTTP/1.1"));
        assert!(requests[2].starts_with("GET /fixture-script.js HTTP/1.1"));
        assert!(
            requests[3]
                .to_lowercase()
                .contains("x-csrf-token: fallback-rotated")
        );
    }

    #[test]
    fn module_detection_allows_only_first_party_assets_and_keeps_legacy_schema() {
        let module = include_str!("../tests/fixtures/twitter/transaction-module-home.html");
        assert!(Transaction::module_entry_url(module).is_some());
        for (from, to) in [
            ("https:", "http:"),
            ("abs.twimg.com/", "abs.twimg.com.evil/"),
            ("abs.twimg.com/", "user:secret@abs.twimg.com/"),
            ("/x-web/x-web/entry-client-", "/other/entry-client-"),
            (".js\"", ".js?secret=token\""),
        ] {
            assert!(Transaction::module_entry_url(&module.replace(from, to)).is_none());
        }
        assert!(Transaction::module_entry_url(HOME).is_none());
        assert!(Transaction::script_url(HOME).is_ok());
    }

    #[tokio::test]
    async fn bootstrap_initializes_header_and_rotates_csrf_before_first_api_request() {
        let (endpoint, server) = serve(vec![
            (
                response(
                    "200 OK",
                    "Set-Cookie: ct0=bootstrap-rotated; Path=/\r\n",
                    HOME.as_bytes(),
                ),
                Duration::ZERO,
            ),
            (response("200 OK", "", SCRIPT.as_bytes()), Duration::ZERO),
            (response("200 OK", "", PHOTO), Duration::ZERO),
        ]);
        let native = native().await;
        *native.transaction.lock().unwrap() = None;
        let mut home = endpoint.clone();
        home.set_path("/");
        let mut script = endpoint.clone();
        script.set_path("/fixture-script.js");
        native.bootstrap(ID, &home, Some(&script)).await.unwrap();
        assert_eq!(
            native.extract_endpoint(&endpoint, ID).await.unwrap().len(),
            1
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET / HTTP/1.1"));
        assert!(requests[1].starts_with("GET /fixture-script.js HTTP/1.1"));
        for request in &requests[..2] {
            let request = request.to_lowercase();
            assert!(!request.contains("authorization:"));
            assert!(!request.contains("x-client-transaction-id:"));
        }
        let api = requests[2].to_lowercase();
        assert!(api.contains("x-csrf-token: bootstrap-rotated"));
        assert!(api.contains("x-client-transaction-id: "));
    }

    #[tokio::test]
    async fn bootstrap_requests_exclude_api_headers_and_bound_html_script_bodies() {
        let (endpoint, server) = serve(vec![(
            response(
                "200 OK",
                "Set-Cookie: ct0=updated; Path=/\r\n",
                HOME.as_bytes(),
            ),
            Duration::ZERO,
        )]);
        let native = native().await;
        let body = native.fetch(&endpoint, ID, false).await.unwrap();
        assert_eq!(body, HOME.as_bytes());
        let requests = server.join().unwrap();
        let request = requests[0].to_lowercase();
        assert!(request.contains("accept: text/html"));
        assert!(!request.contains("authorization:"));
        assert!(!request.contains("x-csrf-token:"));
        assert!(!request.contains("x-client-transaction-id:"));
        assert!(
            native
                .cookies
                .lock()
                .unwrap()
                .header(&endpoint)
                .unwrap()
                .contains("ct0=updated")
        );
        let (endpoint, server) = serve(vec![(
            response("200 OK", "", &vec![b'x'; MAX_RESPONSE + 1]),
            Duration::ZERO,
        )]);
        assert!(
            native
                .fetch(&endpoint, ID, false)
                .await
                .unwrap_err()
                .to_string()
                .contains("16 MiB")
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn dropping_extraction_during_an_active_request_does_not_retry() {
        let (endpoint, server) = serve(vec![(
            response("503 Service Unavailable", "", b""),
            Duration::from_millis(150),
        )]);
        let native = native().await.with_retries(2);
        assert!(
            tokio::time::timeout(
                Duration::from_millis(30),
                native.extract_endpoint(&endpoint, ID)
            )
            .await
            .is_err()
        );
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn missing_x_scoped_auth_fails_before_any_network_request() {
        let native = Twitter::new(None, Duration::from_secs(1)).await.unwrap();
        let err = native
            .extract(&http_url("https://x.com/test/status/1234567890").unwrap())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("auth_token"), "{err}");
    }
}

#[cfg(test)]
#[path = "twitter_profile_tests.rs"]
mod profile_tests;

#[cfg(test)]
#[path = "twitter_reply_tests.rs"]
mod reply_tests;
