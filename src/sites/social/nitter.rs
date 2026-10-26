//! Nitter instances (and `nitter:https://...`): tweets, replies, media,
//! searches and single tweets, mirroring gallery_dl/extractor/nitter.py.
use crate::error;
use crate::sites::gdl::{
    Scan, date, extr, extract_iter, nameext_into, parse_int, re, rpartition, strptime, truthy,
    unescape, ytdl_item,
};
use crate::sites::{Events, Http, Item, Route, Site};
use base64::Engine;
use serde_json::{Map, Value, json};
use std::time::Duration;
use url::Url;

const USER_AGENT: &str = "gallery-dl/1.32.17";
const FILENAME: &str = "{tweet_id}_{num}.{extension}";
const DIRECTORY: &[&str] = &["nitter", "{user[name]}"];

macro_rules! routes {
    ($($ident:ident $sub:literal),*) => { $(static $ident: Route = Route::new($sub, FILENAME, DIRECTORY, "{tweet_id}_{num}");)* };
}
routes!(TWEETS "tweets", REPLIES "replies", MEDIA "media", SEARCH "search", TWEET "tweet");
const ROUTES: &[&Route] = &[&TWEETS, &REPLIES, &MEDIA, &SEARCH, &TWEET];

struct Instance {
    root: &'static str,
    /// Host pattern without the scheme; empty for the `nitter:` prefix.
    pattern: &'static str,
}

macro_rules! instance {
    ($site:ident, $inst:ident, $category:literal, $name:literal, $root:literal, $pattern:literal) => {
        static $inst: Instance = Instance {
            root: $root,
            pattern: $pattern,
        };
        pub static $site: Site = Site {
            category: $category,
            name: $name,
            matches: |url| target(&$inst, url).is_some(),
            extract: |http, url| extract(&$inst, &$site, http, url),
            routes: ROUTES,
            api_key: false,
            cookies: None,
        };
    };
}
instance!(
    NITTER_NET,
    NITTER_NET_I,
    "nitter.net",
    "Nitter",
    "https://nitter.net",
    r"(?:www\.)?nitter\.net"
);
instance!(
    NITTER_SPACE,
    NITTER_SPACE_I,
    "nitter.space",
    "nitter.space",
    "https://nitter.space",
    r"(?:www\.)?nitter\.space"
);
instance!(
    NITTER_TIEKOETTER,
    NITTER_TIEKOETTER_I,
    "nitter.tiekoetter",
    "nitter.tiekoetter",
    "https://nitter.tiekoetter",
    r"(?:www\.)?nitter\.tiekoetter\.com"
);
instance!(
    XCANCEL,
    XCANCEL_I,
    "xcancel",
    "XCancel",
    "https://xcancel.com",
    r"(?:www\.)?xcancel\.com"
);
instance!(
    LIGHTBRD,
    LIGHTBRD_I,
    "lightbrd",
    "lightbrd",
    "https://lightbrd.com",
    r"(?:www\.)?lightbrd\.com"
);
// `nitter:https://host/...` for other Nitter instances.
instance!(NITTER, NITTER_I, "nitter", "Nitter", "", "");

#[derive(Debug, PartialEq)]
enum Kind {
    /// Timeline route, user name and numeric user id.
    Timeline(&'static str, String, String),
    Tweet(String),
}

fn compiled(pattern: &'static str) -> regex::Regex {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<&'static str, regex::Regex>>> = OnceLock::new();
    let mut cache = CACHE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    cache
        .entry(pattern)
        .or_insert_with(|| {
            regex::Regex::new(&format!(r"^https?://{pattern}")).expect("static nitter pattern")
        })
        .clone()
}

fn target(inst: &Instance, url: &Url) -> Option<(String, Kind)> {
    let raw = url.as_str();
    let (root, rest) = if inst.pattern.is_empty() {
        let c = re!(r"^nitter:(https?://[^/?#]+)(.*)$").captures(raw)?;
        (c[1].to_owned(), c[2].to_owned())
    } else {
        let c = compiled(inst.pattern).captures(raw)?;
        (inst.root.to_owned(), raw[c.get(0)?.end()..].to_owned())
    };
    if let Some(c) = re!(r"^/(i/web|[^/?#]+)/status/(\d+)").captures(&rest) {
        return Some((root, Kind::Tweet(c[2].to_owned())));
    }
    const USER: &str = r"^/(i(?:/user/|d:)(\d+)|[^/?#]+)";
    let user = |c: &regex::Captures| {
        (
            c[1].to_owned(),
            c.get(2).map_or(String::new(), |m| m.as_str().to_owned()),
        )
    };
    for name in ["tweets", "replies", "media", "search"] {
        let pattern = match name {
            "tweets" => re!(&format!(r"{USER}(?:/tweets)?(?:$|\?|#)")),
            "replies" => re!(&format!("{USER}/with_replies")),
            "media" => re!(&format!("{USER}/media")),
            _ => re!(&format!("{USER}/search")),
        };
        if let Some(c) = pattern.captures(&rest) {
            let (name_, id) = user(&c);
            return Some((root, Kind::Timeline(name, name_, id)));
        }
    }
    None
}

/// Upstream `request_interval = (0.5, 1.5)`.
async fn pace() {
    if !cfg!(test) {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// `%b %d, %Y · %I:%M %p UTC` and similar Nitter dates.
fn parse_date(text: &str, format: &str) -> Value {
    date(strptime(text.trim_end_matches(" UTC"), format))
}

fn decode_name(url: &str) -> String {
    if url.contains("/enc/") {
        let encoded = rpartition(url, "/").2;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .ok()
            .and_then(|b| String::from_utf8(b).ok())
            .unwrap_or_default();
        rpartition(&decoded, "/").2.to_owned()
    } else {
        rpartition(url, "%2F").2.to_owned()
    }
}

/// `_tweet_from_html` / `_tweet_from_quote`.
fn tweet_from_html(html: &str, user: Option<&Value>, quoted: bool) -> Map<String, Value> {
    let mut scan = Scan::new(html);
    let author = json!({"name": scan.extr("class=\"fullname\" href=\"/", "\""), "nick": scan.extr("title=\"", "\"")});
    scan.extr("<span class=\"tweet-date", "");
    let link = scan.extr("href=\"", "\"").to_owned();
    let mut tweet = Map::new();
    tweet.insert(
        "user".into(),
        user.cloned().unwrap_or_else(|| author.clone()),
    );
    tweet.insert("author".into(), author);
    tweet.insert(
        "date".into(),
        parse_date(scan.extr("title=\"", "\""), "%b %d, %Y · %I:%M %p"),
    );
    tweet.insert(
        "tweet_id".into(),
        rpartition(&link, "/")
            .2
            .split('#')
            .next()
            .unwrap_or_default()
            .into(),
    );
    if quoted {
        tweet.insert(
            "content".into(),
            scan.extr("class=\"quote-text", "</div")
                .split_once('>')
                .map_or("", |(_, c)| c)
                .into(),
        );
        tweet.insert(
            "_attach".into(),
            scan.extr("class=\"attachments", "\n                </div>")
                .into(),
        );
        tweet.insert("retweet".into(), false.into());
        tweet.insert("quoted".into(), true.into());
        return tweet;
    }
    tweet.insert(
        "content".into(),
        scan.extr("class=\"tweet-content", "</div")
            .split_once('>')
            .map_or("", |(_, c)| c)
            .into(),
    );
    tweet.insert(
        "_attach".into(),
        scan.extr("class=\"attachments", "class=\"tweet-stats")
            .into(),
    );
    for (key, icon) in [
        ("comments", "icon-comment"),
        ("retweets", "icon-retweet"),
        ("quotes", "icon-quote"),
        ("likes", "icon-heart"),
    ] {
        tweet.insert(
            key.into(),
            parse_int(rpartition(scan.extr(&format!("class=\"{icon}"), "</div>"), ">").2).into(),
        );
    }
    tweet.insert(
        "retweet".into(),
        html.contains("class=\"retweet-header").into(),
    );
    tweet.insert("quoted".into(), false.into());
    tweet
}

/// `_user_from_html`: profile fields from the timeline header.
fn user_from_html(root: &str, html: &str) -> Value {
    let start = html.find("class=\"profile-tabs").unwrap_or(0);
    let mut scan = Scan::new(&html[start..]);
    let banner = scan
        .extr("class=\"profile-banner\"><a href=\"", "\"")
        .to_owned();
    let uid = if banner.contains("/enc/") {
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(rpartition(&banner, "/").2)
            .ok()
            .and_then(|b| String::from_utf8(b).ok())
            .unwrap_or_default();
        decoded
            .split('/')
            .nth(4)
            .map_or(Value::from(0), Value::from)
    } else {
        banner
            .split("%2F")
            .nth(4)
            .map_or(Value::from(0), Value::from)
    };
    let mut scan2 = Scan::new(&html[start..]);
    scan2.extr("class=\"profile-banner\"><a href=\"", "\"");
    let image = scan2
        .extr("class=\"profile-card-avatar\" href=\"", "\"")
        .to_owned();
    let nick = scan2.extr("title=\"", "\"").to_owned();
    let name = scan2.extr("title=\"@", "\"").to_owned();
    let description = scan2.extr("<p dir=\"auto\">", "<").to_owned();
    let joined = parse_date(
        scan2.extr("class=\"profile-joindate\"><span title=\"", "\""),
        "%I:%M %p - %d %b %Y",
    );
    let mut stats = [0i64; 4];
    for value in &mut stats {
        *value = parse_int(
            &scan2
                .extr("class=\"profile-stat-num\">", "<")
                .replace(',', ""),
        );
    }
    json!({
        "id": uid,
        "profile_banner": if banner.is_empty() { String::new() } else { format!("{root}{banner}") },
        "profile_image": format!("{root}{image}"),
        "nick": nick,
        "name": name,
        "description": description,
        "date": joined,
        "statuses_count": stats[0],
        "friends_count": stats[1],
        "followers_count": stats[2],
        "favourites_count": stats[3],
        "verified": html.contains("title=\"Verified account\""),
    })
}

/// `_extract_quote`: the tweet without its quote, and the quote.
fn split_quote(html: &str) -> (String, Option<String>) {
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

/// `NitterExtractor.items` for one tweet.
fn tweet_items(http: &Http, root: &str, mut tweet: Map<String, Value>) -> Vec<Item> {
    let videos = http.option("videos").cloned().unwrap_or(Value::Bool(true));
    let ytdl = videos.as_str() == Some("ytdl");
    let videos = ytdl || truthy(Some(&videos));
    let attachments = tweet
        .remove("_attach")
        .map(|a| crate::sites::scalar(&a))
        .unwrap_or_default();
    let absolute = |url: &str| {
        if url.starts_with('/') {
            format!("{root}{url}")
        } else {
            url.to_owned()
        }
    };
    let mut files: Vec<(String, Map<String, Value>)> = Vec::new();
    for url in extract_iter(&attachments, "href=\"", "\"") {
        if url.contains("/i/broadcasts/") {
            continue;
        }
        let name = decode_name(url);
        let (stem, _, ext) = rpartition(&name, ".");
        let mut file = Map::new();
        file.insert("filename".into(), stem.into());
        file.insert("extension".into(), ext.into());
        files.push((absolute(url), file));
    }
    if videos && files.is_empty() && !attachments.is_empty() {
        if ytdl {
            let mut file = Map::new();
            file.insert("extension".into(), "mp4".into());
            files.push((
                format!(
                    "ytdl:{root}/i/status/{}",
                    crate::sites::scalar(&tweet["tweet_id"])
                ),
                file,
            ));
        } else {
            for url in extract_iter(&attachments, "data-url=\"", "\"") {
                let name = decode_name(url);
                let mut file = Map::new();
                file.insert("filename".into(), rpartition(&name, ".").0.into());
                file.insert("extension".into(), "mp4".into());
                files.push((format!("ytdl:{}", absolute(url)), file));
            }
            for url in extract_iter(&attachments, "<source src=\"", "\"") {
                let url = absolute(url);
                let mut file = Map::new();
                nameext_into(&url, &mut file);
                files.push((url, file));
            }
        }
    }
    tweet.insert("count".into(), files.len().into());
    let tweet_id = crate::sites::scalar(&tweet["tweet_id"]);
    files
        .into_iter()
        .enumerate()
        .filter_map(|(index, (url, mut file))| {
            file.extend(tweet.clone());
            file.insert("num".into(), (index + 1).into());
            let item = match url.strip_prefix("ytdl:") {
                Some(page) => ytdl_item(page, file)?,
                None => Item::new(Url::parse(&url).ok()?, file),
            };
            Some(
                item.ids(tweet_id.clone(), (index + 1).to_string())
                    .header("User-Agent", USER_AGENT),
            )
        })
        .collect()
}

async fn page(http: &Http, root: &str, url: &str) -> crate::Result<String> {
    pace().await;
    if let Ok(base) = http.api(root) {
        http.set_cookie(&base, "hlsPlayback=on; Path=/");
    }
    http.text_with(&http.api(url)?, &[("User-Agent", USER_AGENT)])
        .await
}

fn extract<'a>(
    inst: &'static Instance,
    site: &'static Site,
    http: &'a Http,
    url: Url,
) -> Events<'a> {
    let Some((root, kind)) = target(inst, &url) else {
        return crate::sites::fail(error("unsupported Nitter URL"));
    };
    let retweets = truthy(http.option("retweets"));
    let quoted = truthy(http.option("quoted"));
    match kind {
        Kind::Tweet(id) => crate::sites::once(async move {
            let html = page(http, &root, &format!("{root}/i/status/{id}")).await?;
            let html = extr(
                &html,
                "class=\"main-tweet",
                "                </div>\n              </div></div></div>",
            );
            let (html, quote) = split_quote(html);
            let tweet = tweet_from_html(&html, None, false);
            let mut items = tweet_items(http, &root, tweet.clone());
            if let (Some(quote), true) = (quote, quoted) {
                let mut quoted = tweet_from_html(&quote, None, true);
                quoted.insert("user".into(), tweet["user"].clone());
                items.extend(tweet_items(http, &root, quoted));
            }
            Ok(crate::sites::events(crate::sites::post(
                site, http, &TWEET, items,
            )))
        }),
        Kind::Timeline(name, user, user_id) => {
            let route: &'static Route = match name {
                "tweets" => &TWEETS,
                "replies" => &REPLIES,
                "media" => &MEDIA,
                _ => &SEARCH,
            };
            let path = match name {
                "tweets" => "",
                "replies" => "/with_replies",
                "media" => "/media",
                _ => "/search",
            };
            crate::sites::paginate(
                Some((None::<String>, None::<Value>, String::new())),
                move |state| {
                    let root = root.clone();
                    let user = user.clone();
                    let user_id = user_id.clone();
                    async move {
                        let Some((url, user_obj, base)) = state else {
                            return Ok(None);
                        };
                        let (url, base) = match url {
                            Some(url) => (url, base),
                            None => {
                                let name = if user_id.is_empty() {
                                    user
                                } else {
                                    let location = http
                                        .location(
                                            &http.api(&format!("{root}/i/user/{user_id}"))?,
                                            &[("User-Agent", USER_AGENT)],
                                        )
                                        .await?;
                                    location
                                        .map(|l| rpartition(l.as_str(), "/").2.to_owned())
                                        .ok_or_else(|| error("Nitter user not found"))?
                                };
                                let base = format!("{root}/{name}{path}");
                                (base.clone(), base)
                            }
                        };
                        let html = page(http, &root, &url).await?;
                        let parts: Vec<&str> = html.split("<div class=\"timeline-item").collect();
                        let user_obj = user_obj.unwrap_or_else(|| user_from_html(&root, parts[0]));
                        let mut items = Vec::new();
                        for part in &parts[1..] {
                            let (body, quote) = split_quote(part);
                            let tweet = tweet_from_html(&body, Some(&user_obj), false);
                            if tweet["date"] == "[Invalid DateTime]"
                                || (!retweets && tweet["retweet"] == true)
                            {
                                continue;
                            }
                            items.push(tweet_items(http, &root, tweet));
                            if let (true, Some(quote)) = (quoted, quote) {
                                items.push(tweet_items(
                                    http,
                                    &root,
                                    tweet_from_html(&quote, Some(&user_obj), true),
                                ));
                            }
                        }
                        let more = extr(
                            parts.last().copied().unwrap_or_default(),
                            "<div class=\"show-more\"><a href=\"?",
                            "\"",
                        );
                        let next = (!more.is_empty()).then(|| {
                            (
                                Some(format!("{base}?{}", unescape(more))),
                                Some(user_obj.clone()),
                                base.clone(),
                            )
                        });
                        let events = items
                            .into_iter()
                            .flat_map(|items| crate::sites::post(site, http, route, items))
                            .collect();
                        Ok(Some((crate::sites::events(events), next)))
                    }
                },
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http as client, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t =
            |inst: &Instance, raw: &str| target(inst, &Url::parse(raw).unwrap()).map(|(_, k)| k);
        assert_eq!(
            t(&NITTER_NET_I, "https://nitter.net/supernaturepics"),
            Some(Kind::Timeline(
                "tweets",
                "supernaturepics".into(),
                String::new()
            ))
        );
        assert_eq!(
            t(&XCANCEL_I, "https://xcancel.com/i/user/2976459548/media"),
            Some(Kind::Timeline(
                "media",
                "i/user/2976459548".into(),
                "2976459548".into()
            ))
        );
        assert_eq!(
            t(
                &NITTER_NET_I,
                "https://nitter.net/i/web/status/1170676200615788544"
            ),
            Some(Kind::Tweet("1170676200615788544".into()))
        );
        assert_eq!(
            t(&NITTER_I, "nitter:https://nitter.example/user/status/1"),
            Some(Kind::Tweet("1".into()))
        );
    }

    #[tokio::test]
    async fn tweet_attachments() {
        let page = r##"<div class="main-tweet"><a class="fullname" href="/jane" title="Jane">Jane</a><span class="tweet-date"><a href="/jane/status/123#m" title="Jan 2, 2024 · 3:04 PM UTC">x</a></span>
            <div class="tweet-content media-body">Hello</div><div class="attachments"><a class="still-image" href="/pic/orig/media%2FAbCd.jpg"></a></div><div class="tweet-stats">
            <span class="icon-heart"></span> 12</div>
                </div>
              </div></div></div>"##;
        let (root, server) = serve(vec![html(page)]);
        let http = client("nitter.net", root);
        let files = media(extract(
            &NITTER_NET_I,
            &NITTER_NET,
            &http,
            Url::parse("https://nitter.net/jane/status/123").unwrap(),
        ))
        .await;
        assert_eq!(
            files[0].url.as_str(),
            "https://nitter.net/pic/orig/media%2FAbCd.jpg"
        );
        let k = keywords(&files[0]);
        assert_eq!(
            (
                k["filename"].as_str(),
                k["likes"].as_i64(),
                k["date"].as_str()
            ),
            (Some("AbCd"), Some(12), Some("2024-01-02 15:04:00"))
        );
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "123_1.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("nitter/jane")
        );
        server.join().unwrap();
    }
}
