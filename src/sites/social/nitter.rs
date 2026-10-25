//! Nitter instances, mirroring gallery_dl/extractor/nitter.py by scraping
//! timeline and status pages: each tweet's attachments, with videos as
//! yt-dlp pages (`videos` option, default on; `"ytdl"` hands yt-dlp the
//! status page instead).
//!
//! Options: `retweets` (default off), `quoted` (default off) and `videos`.
//! Upstream's built-in `nitter.tiekoetter` root lacks its `.com` suffix;
//! requests here go to `nitter.tiekoetter.com`. Upstream's request
//! interval and its retry of files that answer 404 are not applied.
use crate::sites::art::{Scan, extr, extract_iter, parse_int, strptime};
use crate::sites::{Events, Http, Item, Route, Site, unescape};
use crate::{Result, error};
use base64::Engine;
use serde_json::{Map, Value, json};
use url::Url;

const FILENAME: &str = "{tweet_id}_{num}.{extension}";
const DIRECTORY: &[&str] = &["nitter", "{user[name]}"];
const ARCHIVE: &str = "{tweet_id}_{num}";
static TWEETS: Route = Route::new("tweets", FILENAME, DIRECTORY, ARCHIVE);
static REPLIES: Route = Route::new("replies", FILENAME, DIRECTORY, ARCHIVE);
static MEDIA: Route = Route::new("media", FILENAME, DIRECTORY, ARCHIVE);
static SEARCH: Route = Route::new("search", FILENAME, DIRECTORY, ARCHIVE);
static TWEET: Route = Route::new("tweet", FILENAME, DIRECTORY, ARCHIVE);

macro_rules! instance {
    ($ident:ident, $category:literal, $name:literal, $domain:literal) => {
        pub static $ident: Site = Site {
            category: $category,
            name: $name,
            matches: |url| target($domain, url).is_some(),
            extract: |http, url| extract(&$ident, $domain, http, url),
            routes: &[&TWEETS, &REPLIES, &MEDIA, &SEARCH, &TWEET],
            api_key: false,
            cookies: Some($domain),
        };
    };
}
instance!(NITTER_NET, "nitter.net", "Nitter", "nitter.net");
instance!(NITTER_SPACE, "nitter.space", "nitter.space", "nitter.space");
instance!(
    NITTER_TIEKOETTER,
    "nitter.tiekoetter",
    "nitter.tiekoetter",
    "nitter.tiekoetter.com"
);
instance!(XCANCEL, "xcancel", "XCancel", "xcancel.com");
instance!(LIGHTBRD, "lightbrd", "lightbrd", "lightbrd.com");

/// Configured `extractor.nitter.<category>` instances.
pub(crate) static FAMILY: crate::sites::instances::Family = crate::sites::instances::Family {
    basecategory: "nitter",
    builtin: &[
        &NITTER_NET,
        &NITTER_SPACE,
        &NITTER_TIEKOETTER,
        &XCANCEL,
        &LIGHTBRD,
    ],
    related: &[],
    template: &NITTER_NET,
    matches: |category, url| {
        crate::sites::instances::configured(&FAMILY, category)
            .is_some_and(|instance| configured_target(instance.spec, url).is_some())
    },
    extract: |category, http, url| {
        let Some(instance) = crate::sites::instances::configured(&FAMILY, category) else {
            return crate::sites::fail(error("unsupported Nitter URL"));
        };
        match configured_target(instance.spec, &url) {
            Some(target) => run(&instance.site, target, instance.root.to_owned(), http),
            None => crate::sites::fail(error("unsupported Nitter URL")),
        }
    },
};

#[derive(Debug)]
enum Target {
    /// The timeline route, the user path segment(s) and the numeric ID
    /// from `i/user/ID` or `id:ID`.
    User(&'static Route, String, Option<String>),
    Tweet(String),
}

impl PartialEq for Target {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::User(a, b, c), Self::User(x, y, z)) => std::ptr::eq(*a, *x) && b == y && c == z,
            (Self::Tweet(a), Self::Tweet(b)) => a == b,
            _ => false,
        }
    }
}

/// `/(i(?:/user/|d:)(\d+)|[^/?#]+)` followed by `(?:/tweets)?(?:$|\?|#)`,
/// `/with_replies`, `/media` or `/search`; else `/(i/web|[^/?#]+)/status/(\d+)`.
fn target(domain: &str, url: &Url) -> Option<Target> {
    if !crate::sites::art::on_host(url, &[domain]) {
        return None;
    }
    parse_path(url.path())
}

/// [`target`] below a configured instance's `host[:port][/prefix]`.
fn configured_target(spec: &str, url: &Url) -> Option<Target> {
    let rest = crate::sites::instances::below(url, spec)?;
    parse_path(rest.split('?').next().unwrap_or_default())
}

fn parse_path(path: &str) -> Option<Target> {
    let path = path.strip_prefix('/')?;
    let numeric = path
        .strip_prefix("i/user/")
        .or_else(|| path.strip_prefix("id:"))
        .and_then(|rest| {
            let id = crate::sites::art::leading_digits(rest)?;
            let user = &path[..path.len() - rest.len() + id.len()];
            Some((user, id, &rest[id.len()..]))
        });
    let segment = path.split('/').next().unwrap_or_default();
    let candidates = numeric
        .map(|(user, id, rest)| (user, Some(id), rest))
        .into_iter()
        .chain((!segment.is_empty()).then(|| (segment, None, &path[segment.len()..])));
    for (user, id, rest) in candidates {
        let route = match rest {
            "" | "/tweets" => Some(&TWEETS),
            _ if rest.starts_with("/with_replies") => Some(&REPLIES),
            _ if rest.starts_with("/media") => Some(&MEDIA),
            _ if rest.starts_with("/search") => Some(&SEARCH),
            _ => None,
        };
        if let Some(route) = route {
            return Some(Target::User(route, user.into(), id.map(str::to_owned)));
        }
    }
    let rest = path
        .strip_prefix("i/web/")
        .or_else(|| path.split_once('/').map(|(_, rest)| rest))?;
    let id = crate::sites::art::leading_digits(rest.strip_prefix("status/")?)?;
    (!segment.is_empty()).then(|| Target::Tweet(id.into()))
}

struct Options {
    retweets: bool,
    quoted: bool,
    videos: bool,
    ytdl: bool,
}

fn extract<'a>(site: &'static Site, domain: &'static str, http: &'a Http, url: Url) -> Events<'a> {
    let Some(target) = target(domain, &url) else {
        return crate::sites::fail(error("unsupported Nitter URL"));
    };
    run(site, target, format!("https://{domain}"), http)
}

fn run<'a>(site: &'static Site, target: Target, root: String, http: &'a Http) -> Events<'a> {
    let options = Options {
        retweets: http.option_bool("retweets", false),
        quoted: http.option_bool("quoted", false),
        videos: http.option_bool("videos", true),
        ytdl: http.option_str("videos") == Some("ytdl"),
    };
    if options.videos
        && let Ok(base) = http.api(&root)
    {
        http.set_cookie(&base, "hlsPlayback=on; Path=/");
    }
    match target {
        Target::Tweet(id) => crate::sites::once(async move {
            let page = http
                .text(&http.api(&format!("{root}/i/status/{id}"))?)
                .await?;
            let html = extr(
                &page,
                "class=\"main-tweet",
                "                </div>\n              </div></div></div>",
            );
            let (html, quote) = extract_quote(html);
            let tweet = tweet_from_html(&html, None);
            let mut tweets = vec![tweet.clone()];
            if let Some(quote) = quote.filter(|_| options.quoted) {
                let mut quoted = tweet_from_quote(&quote, None);
                quoted.insert("user".into(), tweet["user"].clone());
                tweets.push(quoted);
            }
            let mut events = Vec::new();
            for tweet in tweets {
                events.extend(tweet_events(site, http, &TWEET, &root, &options, tweet));
            }
            Ok(crate::sites::events(events))
        }),
        Target::User(route, user, id) => crate::sites::once(async move {
            let user = match id {
                Some(id) => {
                    let location = http
                        .location(&http.api(&format!("{root}/i/user/{id}"))?, &[])
                        .await?
                        .ok_or_else(|| error("Nitter did not redirect to the user"))?;
                    let name = location.path().rsplit('/').next().unwrap_or_default();
                    crate::sites::percent_decode(name)
                }
                None => user,
            };
            let path = match route.subcategory {
                "tweets" => "",
                "replies" => "/with_replies",
                "media" => "/media",
                _ => "/search",
            };
            let base = format!("{root}/{user}{path}");
            Ok(pagination(site, http, route, root, base, options))
        }),
    }
}

/// `_pagination`: timeline pages linked by `show-more`.
fn pagination<'a>(
    site: &'static Site,
    http: &'a Http,
    route: &'static Route,
    root: String,
    base: String,
    options: Options,
) -> Events<'a> {
    let options = std::sync::Arc::new(options);
    crate::sites::paginate(Some((base.clone(), None::<Value>)), move |state| {
        let (root, base, options) = (root.clone(), base.clone(), options.clone());
        async move {
            let Some((url, user)) = state else {
                return Ok(None);
            };
            let page = http.text(&http.api(&url)?).await?;
            let chunks: Vec<&str> = page.split("<div class=\"timeline-item").collect();
            let user = match user {
                Some(user) => user,
                None => user_from_html(chunks[0], &root)?,
            };
            let mut events = Vec::new();
            for chunk in &chunks[1..] {
                let (html, quote) = extract_quote(chunk);
                let tweet = tweet_from_html(&html, Some(&user));
                if timestamp(&tweet).is_none() {
                    continue;
                }
                events.extend(tweet_events(site, http, route, &root, &options, tweet));
                if let Some(quote) = quote.filter(|_| options.quoted) {
                    let quoted = tweet_from_quote(&quote, Some(&user));
                    events.extend(tweet_events(site, http, route, &root, &options, quoted));
                }
            }
            let more = extr(
                chunks.last().copied().unwrap_or_default(),
                "<div class=\"show-more\"><a href=\"?",
                "\"",
            );
            let next =
                (!more.is_empty()).then(|| (format!("{base}?{}", unescape(more)), Some(user)));
            Ok(Some((crate::sites::events(events), next)))
        }
    })
}

/// `binascii.a2b_base64` of the last path segment of an `/enc/` URL.
fn decode_enc(url: &str) -> Option<String> {
    let data = url.rsplit('/').next()?.trim_end_matches('=');
    let bytes = base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(data)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(data))
        .ok()?;
    String::from_utf8(bytes).ok()
}

/// The file name of an attachment URL.
fn attachment_name(url: &str) -> String {
    if url.contains("/enc/") {
        decode_enc(url)
            .map(|path| path.rsplit('/').next().unwrap_or_default().to_owned())
            .unwrap_or_default()
    } else {
        url.rsplit("%2F").next().unwrap_or_default().to_owned()
    }
}

fn absolute(url: &str, root: &str) -> String {
    if url.starts_with('/') {
        format!("{root}{url}")
    } else {
        url.to_owned()
    }
}

/// `items()` for one tweet: its attachments numbered from one.
fn tweet_events(
    site: &'static Site,
    http: &Http,
    route: &'static Route,
    root: &str,
    options: &Options,
    mut tweet: Map<String, Value>,
) -> Vec<crate::event::ExtractionEvent> {
    if !options.retweets && tweet.get("retweet") == Some(&Value::Bool(true)) {
        return Vec::new();
    }
    let attachments = tweet
        .remove("_attach")
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default();
    // (URL, filename, extension, whether yt-dlp handles it)
    let mut files: Vec<(String, String, String, bool)> = Vec::new();
    if !attachments.is_empty() {
        for url in extract_iter(&attachments, "href=\"", "\"") {
            if url.contains("/i/broadcasts/") {
                continue;
            }
            let name = attachment_name(url);
            let (stem, ext) = name.rsplit_once('.').unwrap_or(("", &name));
            files.push((absolute(url, root), stem.into(), ext.into(), false));
        }
        if options.videos && files.is_empty() {
            if options.ytdl {
                let id = tweet
                    .get("tweet_id")
                    .map(crate::sites::scalar)
                    .unwrap_or_default();
                files.push((
                    format!("{root}/i/status/{id}"),
                    String::new(),
                    "mp4".into(),
                    true,
                ));
            } else {
                for url in extract_iter(&attachments, "data-url=\"", "\"") {
                    let name = attachment_name(url);
                    let stem = name.rsplit_once('.').map_or("", |(stem, _)| stem);
                    files.push((absolute(url, root), stem.into(), "mp4".into(), true));
                }
                for url in extract_iter(&attachments, "<source src=\"", "\"") {
                    let url = absolute(url, root);
                    let (stem, ext) = crate::sites::art::nameext_from_name(
                        &crate::sites::percent_decode(crate::sites::art::filename_from_url(&url)),
                    );
                    files.push((url, stem, ext, false));
                }
            }
        }
    }
    tweet.insert("count".into(), files.len().into());
    let id = tweet
        .get("tweet_id")
        .map(crate::sites::scalar)
        .unwrap_or_default();
    let user = tweet
        .get("user")
        .and_then(|u| u.get("name"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let content = tweet
        .get("content")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let ts = timestamp(&tweet);
    let mut items = Vec::new();
    for (index, (url, stem, ext, ytdl)) in files.into_iter().enumerate() {
        let mut fields = Map::new();
        fields.insert(
            "url".into(),
            if ytdl {
                format!("ytdl:{url}")
            } else {
                url.clone()
            }
            .into(),
        );
        if !(ytdl && options.ytdl) {
            fields.insert("filename".into(), stem.into());
        }
        fields.insert("extension".into(), ext.into());
        crate::sites::art::update(&mut fields, &tweet);
        fields.insert("num".into(), (index + 1).into());
        let item = if ytdl {
            super::ytdl_item(&url, fields)
        } else {
            Url::parse(url.trim())
                .ok()
                .filter(|u| crate::sites::instances::plain_or_root(u, root))
                .map(|u| Item::new(u, fields))
        };
        if let Some(item) = item {
            items.push(
                item.ids(id.clone(), (index + 1).to_string())
                    .user(user.clone())
                    .caption(content.clone())
                    .timestamp(ts),
            );
        }
    }
    crate::sites::post(site, http, route, items)
}

/// The tweet's `date` as Unix seconds.
fn timestamp(tweet: &Map<String, Value>) -> Option<i64> {
    crate::filter::source_date_from_json(tweet.get("date")?).flatten()
}

/// `%b %d, %Y · %I:%M %p %Z` for a UTC date.
fn tweet_date(text: &str) -> Option<i64> {
    strptime(text.strip_suffix(" UTC")?, "%b %d, %Y · %I:%M %p")
}

fn author(scan: &mut Scan<'_>) -> Value {
    json!({
        "name": scan.extr("class=\"fullname\" href=\"/", "\""),
        "nick": scan.extr("title=\"", "\""),
    })
}

fn stat(scan: &mut Scan<'_>, icon: &str) -> i64 {
    let text = scan.extr(&format!("class=\"{icon}"), "</div>");
    parse_int(text.rsplit('>').next().unwrap_or_default(), 0)
}

/// `_tweet_from_html`.
pub(crate) fn tweet_from_html(html: &str, user: Option<&Value>) -> Map<String, Value> {
    let mut scan = Scan::new(html);
    let author = author(&mut scan);
    scan.extr("<span class=\"tweet-date", "");
    let link = scan.extr("href=\"", "\"");
    let date = tweet_date(scan.extr("title=\"", "\""));
    let id = link.rsplit('/').next().unwrap_or_default();
    let id = id.split('#').next().unwrap_or_default();
    let content = scan.extr("class=\"tweet-content", "</div");
    let content = content.split_once('>').map_or("", |(_, c)| c);
    let attach = scan.extr("class=\"attachments", "class=\"tweet-stats");
    let mut tweet = Map::new();
    tweet.insert("author".into(), author.clone());
    tweet.insert("user".into(), user.cloned().unwrap_or(author));
    tweet.insert("date".into(), crate::filter::source_date_json(date));
    tweet.insert("tweet_id".into(), id.into());
    tweet.insert("content".into(), content.into());
    tweet.insert("_attach".into(), attach.into());
    for (key, icon) in [
        ("comments", "icon-comment"),
        ("retweets", "icon-retweet"),
        ("quotes", "icon-quote"),
        ("likes", "icon-heart"),
    ] {
        tweet.insert(key.into(), stat(&mut scan, icon).into());
    }
    tweet.insert(
        "retweet".into(),
        html.contains("class=\"retweet-header").into(),
    );
    tweet.insert("quoted".into(), false.into());
    tweet
}

/// `_tweet_from_quote`.
pub(crate) fn tweet_from_quote(html: &str, user: Option<&Value>) -> Map<String, Value> {
    let mut scan = Scan::new(html);
    let author = author(&mut scan);
    scan.extr("<span class=\"tweet-date", "");
    let link = scan.extr("href=\"", "\"");
    let date = tweet_date(scan.extr("title=\"", "\""));
    let id = link.rsplit('/').next().unwrap_or_default();
    let id = id.split('#').next().unwrap_or_default();
    let content = scan.extr("class=\"quote-text", "</div");
    let content = content.split_once('>').map_or("", |(_, c)| c);
    let attach = scan.extr("class=\"attachments", "\n                </div>");
    let mut tweet = Map::new();
    tweet.insert("author".into(), author.clone());
    tweet.insert("user".into(), user.cloned().unwrap_or(author));
    tweet.insert("date".into(), crate::filter::source_date_json(date));
    tweet.insert("tweet_id".into(), id.into());
    tweet.insert("content".into(), content.into());
    tweet.insert("_attach".into(), attach.into());
    tweet.insert("retweet".into(), false.into());
    tweet.insert("quoted".into(), true.into());
    tweet
}

/// `_user_from_html`: the profile card of a timeline page.
pub(crate) fn user_from_html(html: &str, root: &str) -> Result<Value> {
    let start = html
        .find("class=\"profile-tabs")
        .ok_or_else(|| error("Nitter page has no profile"))?;
    let mut scan = Scan::new(&html[start..]);
    let banner = scan.extr("class=\"profile-banner\"><a href=\"", "\"");
    let uid = if banner.contains("/enc/") {
        decode_enc(banner).and_then(|path| path.split('/').nth(4).map(str::to_owned))
    } else {
        banner.split("%2F").nth(4).map(str::to_owned)
    };
    let image = scan.extr("class=\"profile-card-avatar\" href=\"", "\"");
    let nick = scan.extr("title=\"", "\"");
    let name = scan.extr("title=\"@", "\"");
    let description = scan.extr("<p dir=\"auto\">", "<");
    let date = strptime(
        scan.extr("class=\"profile-joindate\"><span title=\"", "\""),
        "%I:%M %p - %d %b %Y",
    );
    let mut counts = Vec::new();
    for _ in 0..4 {
        counts.push(parse_int(
            &scan
                .extr("class=\"profile-stat-num\">", "<")
                .replace(',', ""),
            0,
        ));
    }
    Ok(json!({
        "id": uid.map_or(Value::from(0), Value::from),
        "profile_banner": if banner.is_empty() { String::new() } else { format!("{root}{banner}") },
        "profile_image": format!("{root}{image}"),
        "nick": nick,
        "name": name,
        "description": description,
        "date": crate::filter::source_date_json(date),
        "statuses_count": counts[0],
        "friends_count": counts[1],
        "followers_count": counts[2],
        "favourites_count": counts[3],
        "verified": html.contains("title=\"Verified account\""),
    }))
}

/// `_extract_quote`: the tweet without its quote block, and the quote.
fn extract_quote(html: &str) -> (String, Option<String>) {
    match html.split_once("class=\"quote") {
        Some((head, quote)) => {
            let (quote, tail) = quote
                .split_once("class=\"tweet-published")
                .unwrap_or((quote, ""));
            (format!("{head}{tail}"), Some(quote.to_owned()))
        }
        None => (html.to_owned(), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, response, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target("nitter.net", &Url::parse(raw).unwrap());
        assert_eq!(
            t("https://nitter.net/supernaturepics"),
            Some(Target::User(&TWEETS, "supernaturepics".into(), None))
        );
        assert_eq!(
            t("https://nitter.net/i/user/2976459548"),
            Some(Target::User(
                &TWEETS,
                "i/user/2976459548".into(),
                Some("2976459548".into())
            ))
        );
        assert_eq!(
            t("https://www.nitter.net/id:123/media"),
            Some(Target::User(&MEDIA, "id:123".into(), Some("123".into())))
        );
        assert_eq!(
            t("https://nitter.net/bob/with_replies"),
            Some(Target::User(&REPLIES, "bob".into(), None))
        );
        assert_eq!(
            t("https://nitter.net/bob/tweets?x=1"),
            Some(Target::User(&TWEETS, "bob".into(), None))
        );
        assert_eq!(
            t("https://nitter.net/bob/status/1234#m"),
            Some(Target::Tweet("1234".into()))
        );
        assert_eq!(
            t("https://nitter.net/i/web/status/99"),
            Some(Target::Tweet("99".into()))
        );
        for raw in [
            "https://nitter.net/",
            "https://nitter.net/bob/likes",
            "https://nitter.net/bob/status/x",
            "https://nitter.net.evil/bob",
            "https://evil.nitter.net/bob",
            "https://nitter.net:81/bob",
            "https://u@nitter.net/bob",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
        let tie = Url::parse("https://nitter.tiekoetter.com/bob").unwrap();
        assert!((NITTER_TIEKOETTER.matches)(&tie));
    }

    const PROFILE: &str = r#"<div class="profile-tabs">
        <div class="profile-banner"><a href="/pic/https%3A%2F%2Fpbs.twimg.com%2Fprofile_banners%2F2976459548%2F1421058583%2F1500x500"></a>
        <a class="profile-card-avatar" href="/pic/avatar.jpg" title="Nature Pics">
        <a title="@supernaturepics"></a><p dir="auto">Bio</p>
        <div class="profile-joindate"><span title="2:20 PM - 11 Jan 2015">
        <span class="profile-stat-num">1,234</span><span class="profile-stat-num">5</span>
        <span class="profile-stat-num">6</span><span class="profile-stat-num">7</span>"#;

    fn item(id: u64, attach: &str, retweet: bool) -> String {
        format!(
            r#"<div class="timeline-item">{}<a class="fullname" href="/supernaturepics" title="Nature Pics">
            <span class="tweet-date"><a href="/supernaturepics/status/{id}#m" title="Jan 2, 2020 · 3:04 PM UTC">x</a>
            <div class="tweet-content media-body">Text {id}</div>
            <div class="attachments">{attach}</div>
            <div class="tweet-stats"><span class="icon-comment"></span> 1</div>
            <span class="icon-retweet"></span> 2</div><span class="icon-quote"></span> 3</div>
            <span class="icon-heart"></span> 4</div>"#,
            if retweet {
                r#"<div class="retweet-header">"#
            } else {
                ""
            }
        )
    }

    #[tokio::test]
    async fn pages_through_timeline() {
        let first = format!(
            "{PROFILE}{}{}{}<div class=\"show-more\"><a href=\"?cursor=abc&amp;x=1\">",
            item(
                1,
                r#"<a href="/pic/orig/media%2FAbC.jpg">x</a><a href="/pic/orig/media%2FDeF.png">y</a>"#,
                false
            ),
            item(2, r#"<a href="/pic/orig/media%2Fskip.jpg">x</a>"#, true),
            item(
                3,
                r#"<video data-url="/video/enc/aHR0cHM6Ly92aWRlby50d2ltZy5jb20vdi9jbGlwLm0zdTg="><source src="/video/clip.mp4">"#,
                false
            ),
        );
        let (root, server) = serve(vec![html(&first), html(PROFILE)]);
        let http = http("nitter.net", root);
        let files = media((NITTER_NET.extract)(
            &http,
            Url::parse("https://nitter.net/supernaturepics").unwrap(),
        ))
        .await;
        let urls: Vec<&str> = files.iter().map(|f| f.url.as_str()).collect();
        assert_eq!(
            urls,
            [
                "https://nitter.net/pic/orig/media%2FAbC.jpg",
                "https://nitter.net/pic/orig/media%2FDeF.png",
                "ytdl:https://nitter.net/video/enc/aHR0cHM6Ly92aWRlby50d2ltZy5jb20vdi9jbGlwLm0zdTg=",
                "https://nitter.net/video/clip.mp4",
            ]
        );
        let k = keywords(&files[0]);
        assert_eq!(k["filename"], "AbC");
        assert_eq!(k["tweet_id"], "1");
        assert_eq!(k["content"], "Text 1");
        assert_eq!(k["likes"], 4);
        assert_eq!(k["user"]["id"], "2976459548");
        assert_eq!(k["user"]["statuses_count"], 1234);
        assert_eq!(k["user"]["name"], "supernaturepics");
        assert_eq!(keywords(&files[2])["filename"], "clip");
        assert_eq!(keywords(&files[2])["count"], 2);
        assert_eq!(
            files[0].metadata.as_ref().unwrap().timestamp,
            strptime("2020-01-02 15:04", "%Y-%m-%d %H:%M")
        );
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "1_2.png");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("nitter/supernaturepics")
        );
        let (_, identity) = crate::sites::archive_key(&files[3]).unwrap();
        assert_eq!(identity, r#"["{tweet_id}_{num}",["3","2"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /supernaturepics "));
        assert!(requests[0].contains("hlsPlayback=on"));
        assert!(requests[1].starts_with("GET /supernaturepics?cursor=abc&x=1 "));
    }

    #[tokio::test]
    async fn configured_instance_extracts_on_its_root() {
        crate::sites::instances::define(
            "nitter",
            "nitter-configured",
            serde_json::json!({
                "root": "https://nitter.configured.test/mirror",
                "pattern": r"(?:www\.)?nitter\.configured\.test/mirror|nitter-alias\.test",
            }),
        );
        let first = format!(
            "{PROFILE}{}",
            item(1, r#"<a href="/pic/orig/media%2FAbC.jpg">x</a>"#, false)
        );
        let (root, server) = serve(vec![html(&first)]);
        let http = http("nitter-configured", root);
        let url = Url::parse("https://nitter-alias.test/supernaturepics/media").unwrap();
        let site = crate::sites::find(&url).unwrap();
        assert_eq!(site.category, "nitter-configured");
        assert!(
            crate::sites::find(&Url::parse("https://nitter-alias.test/a/likes").unwrap())
                .is_none_or(|site| site.category != "nitter-configured")
        );
        let files = media((site.extract)(&http, url)).await;
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].url.as_str(),
            "https://nitter.configured.test/mirror/pic/orig/media%2FAbC.jpg"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["category"], "nitter-configured");
        assert_eq!(k["subcategory"], "media");
        assert_eq!(k["tweet_id"], "1");
        assert_eq!(k["user"]["name"], "supernaturepics");
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with("GET /mirror/supernaturepics/media "),
            "{}",
            requests[0]
        );
    }

    #[tokio::test]
    async fn resolves_numeric_user() {
        let redirect = response("302 Found\r\nLocation: /bob", "text/html", b"");
        let (root, server) = serve(vec![redirect, html(PROFILE)]);
        let http = http("xcancel", root);
        let files = media((XCANCEL.extract)(
            &http,
            Url::parse("https://xcancel.com/i/user/42/media").unwrap(),
        ))
        .await;
        assert!(files.is_empty());
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /i/user/42 "));
        assert!(requests[1].starts_with("GET /bob/media "));
    }

    #[tokio::test]
    async fn extracts_tweet_with_quote() {
        let page = format!(
            "<div class=\"main-tweet\">{}<div class=\"quote\"><a class=\"fullname\" href=\"/other\" title=\"Other\">\
             <span class=\"tweet-date\"><a href=\"/other/status/9#m\" title=\"Feb 3, 2021 · 11:00 AM UTC\">\
             <div class=\"quote-text\">Quoted</div><div class=\"attachments\"><a href=\"/pic/orig/media%2FQ.jpg\">\n                </div>\
             <p class=\"tweet-published\">x</p>                </div>\n              </div></div></div>",
            item(5, r#"<a href="/pic/orig/media%2FMain.jpg">x</a>"#, false)
                .trim_start_matches("<div class=\"timeline-item\">")
        );
        let (root, server) = serve(vec![html(&page)]);
        let options = serde_json::json!({"quoted": true}).as_object().cloned();
        let http = http("nitter.net", root).with_options(options.map(std::sync::Arc::new));
        let files = media((NITTER_NET.extract)(
            &http,
            Url::parse("https://nitter.net/supernaturepics/status/5").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let quoted = keywords(&files[1]);
        assert_eq!(quoted["tweet_id"], "9");
        assert_eq!(quoted["quoted"], true);
        assert_eq!(quoted["content"], "Quoted");
        assert_eq!(quoted["user"]["name"], "supernaturepics");
        assert_eq!(quoted["author"]["name"], "other");
        assert_eq!(keywords(&files[0])["subcategory"], "tweet");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /i/status/5 "));
    }
}
