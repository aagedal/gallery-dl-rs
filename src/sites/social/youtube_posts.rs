//! YouTube community posts: `youtube.com/post/ID` and a channel's posts tab
//! (`/@HANDLE/posts`, `/channel/ID/posts`, also `/community`).
//!
//! yt-dlp downloads videos only, so post images are read natively from the
//! page's `ytInitialData`; a post's attached video is queued for the yt-dlp
//! site. Images are requested at their original size (`=s0`), with the
//! extension taken from the file's signature. The posts tab continues
//! through the web client's `browse` endpoint.
use crate::error;
use crate::sites::http::Body;
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value, json};
use url::Url;

pub static SITE: Site = Site {
    category: "youtube",
    name: "YouTube",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&POST, &POSTS],
    api_key: false,
    cookies: Some("youtube.com"),
};

const FILENAME: &str = "{post_id}_{num}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{channel}", "posts"];
static POST: Route = Route::new("post", FILENAME, DIRECTORY, "{post_id}_{num}");
static POSTS: Route = Route::new("posts", FILENAME, DIRECTORY, "{post_id}_{num}");

const ROOT: &str = "https://www.youtube.com";
/// Skips the EU consent interstitial, like yt-dlp.
const CONSENT: &str = "SOCS=CAI; Path=/";

/// Add the consent cookie to the session unless it has one; an explicit
/// `Cookie` header would replace the user's own YouTube cookies.
fn consent(http: &Http, url: &Url) {
    if http.cookie(url, "SOCS").is_none() {
        http.set_cookie(url, CONSENT);
    }
}
/// Posts-tab continuation pages followed at most.
const MAX_PAGES: usize = 1000;

#[derive(Debug, PartialEq)]
enum Target {
    Post(String),
    /// The channel path, e.g. `@NASA` or `channel/UC…`.
    Posts(String),
}

fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url)
        || !matches!(
            url.host_str()?,
            "www.youtube.com" | "youtube.com" | "m.youtube.com"
        )
    {
        return None;
    }
    let segments: Vec<&str> = url.path().split('/').filter(|s| !s.is_empty()).collect();
    let token = |s: &str| {
        !s.is_empty()
            && s.len() <= 100
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    };
    match segments.as_slice() {
        ["post", id] if token(id) => Some(Target::Post((*id).to_owned())),
        [handle, "posts" | "community"] if handle.starts_with('@') && token(&handle[1..]) => {
            Some(Target::Posts((*handle).to_owned()))
        }
        [
            kind @ ("channel" | "c" | "user"),
            name,
            "posts" | "community",
        ] if token(name) => Some(Target::Posts(format!("{kind}/{name}"))),
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Post(id)) => crate::sites::once(async move {
            let page = page(http, &format!("{ROOT}/post/{id}")).await?;
            let posts = posts(&page.data);
            let post = posts
                .into_iter()
                .find(|post| post.get("postId").and_then(Value::as_str) == Some(id.as_str()))
                .ok_or_else(|| error("YouTube post not found or not public"))?;
            Ok(post_events(http, &POST, &post).await)
        }),
        Some(Target::Posts(channel)) => crate::sites::paginate(
            Some((Source::Page(channel), 0usize)),
            move |state| async move {
                let Some((source, count)) = state else {
                    return Ok(None);
                };
                let (data, config) = match source {
                    Source::Page(channel) => {
                        let page = page(http, &format!("{ROOT}/{channel}/posts")).await?;
                        (page.data, page.config)
                    }
                    Source::Continuation(token, config) => {
                        let endpoint =
                            http.api(&format!("{ROOT}/youtubei/v1/browse?prettyPrint=false"))?;
                        let body = json!({
                            "context": {"client": {
                                "clientName": "WEB",
                                "clientVersion": config.version,
                                "hl": "en",
                            }},
                            "continuation": token,
                        });
                        consent(http, &endpoint);
                        let headers = http.browser_headers(&[
                            ("Origin", ROOT),
                            ("Content-Type", "application/json"),
                        ]);
                        let reply = http.post(&endpoint, Body::Json(&body), &headers).await?;
                        let data = serde_json::from_slice(&reply)
                            .map_err(|_| error("YouTube returned invalid JSON"))?;
                        (data, config)
                    }
                };
                let posts = posts(&data);
                if count == 0 && posts.is_empty() && continuation(&data).is_none() {
                    return Err(error("YouTube channel has no public posts"));
                }
                let mut events = Vec::new();
                for post in &posts {
                    events.push(post_events(http, &POSTS, post).await);
                }
                let next = continuation(&data)
                    .filter(|_| count + 1 < MAX_PAGES && !posts.is_empty())
                    .map(|token| (Source::Continuation(token, config), count + 1));
                let events: Events<'_> = Box::pin(futures_util::StreamExt::flatten(
                    futures_util::stream::iter(events),
                ));
                Ok(Some((events, next)))
            },
        ),
        None => crate::sites::fail(error("unsupported YouTube posts URL")),
    }
}

enum Source {
    Page(String),
    Continuation(String, Config),
}

#[derive(Clone)]
struct Config {
    version: String,
}

struct Page {
    data: Value,
    config: Config,
}

async fn page(http: &Http, url: &str) -> crate::Result<Page> {
    let url = http.api(url)?;
    consent(http, &url);
    let headers = http.browser_headers(&[("Accept-Language", "en-US,en")]);
    let html = http.text_with(&url, &headers).await?;
    let data = crate::sites::extr(&html, "var ytInitialData = ", ";</script>")
        .and_then(|json| serde_json::from_str(json).ok())
        .ok_or_else(|| error("YouTube page has no initial data"))?;
    let version = crate::sites::extr(&html, r#""INNERTUBE_CLIENT_VERSION":""#, "\"")
        .filter(|v| {
            !v.is_empty() && v.len() <= 32 && v.bytes().all(|b| b.is_ascii_digit() || b == b'.')
        })
        .unwrap_or("2.20260101.00.00")
        .to_owned();
    Ok(Page {
        data,
        config: Config { version },
    })
}

/// Every `backstagePostRenderer` in `data`, in page order, once each.
fn posts(data: &Value) -> Vec<Map<String, Value>> {
    fn walk(value: &Value, out: &mut Vec<Map<String, Value>>, depth: usize) {
        if depth > 96 {
            return;
        }
        match value {
            Value::Object(map) => {
                if let Some(Value::Object(post)) = map.get("backstagePostRenderer") {
                    let id = post.get("postId");
                    if !out.iter().any(|seen| seen.get("postId") == id) {
                        out.push(post.clone());
                    }
                    return;
                }
                for child in map.values() {
                    walk(child, out, depth + 1);
                }
            }
            Value::Array(list) => {
                for child in list {
                    walk(child, out, depth + 1);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(data, &mut out, 0);
    out
}

/// The posts tab's next continuation token, if any.
fn continuation(data: &Value) -> Option<String> {
    fn walk(value: &Value, depth: usize) -> Option<String> {
        if depth > 96 {
            return None;
        }
        match value {
            Value::Object(map) => {
                if let Some(token) = map
                    .get("continuationItemRenderer")
                    .and_then(|r| r.pointer("/continuationEndpoint/continuationCommand/token"))
                    .and_then(Value::as_str)
                    .filter(|t| !t.is_empty() && t.len() <= 4096)
                {
                    return Some(token.to_owned());
                }
                map.values().find_map(|v| walk(v, depth + 1))
            }
            Value::Array(list) => list.iter().find_map(|v| walk(v, depth + 1)),
            _ => None,
        }
    }
    walk(data, 0)
}

fn text(value: Option<&Value>) -> String {
    value
        .and_then(|v| v.get("runs"))
        .and_then(Value::as_array)
        .map(|runs| {
            runs.iter()
                .filter_map(|run| run.get("text").and_then(Value::as_str))
                .collect::<String>()
        })
        .unwrap_or_default()
}

/// The original-size URL of an image renderer's largest thumbnail.
fn original(image: &Value) -> Option<Url> {
    let raw = image
        .pointer("/image/thumbnails")
        .and_then(Value::as_array)?
        .last()?
        .get("url")?
        .as_str()?;
    let raw = if raw.starts_with("//") {
        format!("https:{raw}")
    } else {
        raw.to_owned()
    };
    let mut url = Url::parse(&raw).ok().filter(|u| u.scheme() == "https")?;
    if url
        .host_str()
        .is_some_and(|h| h.ends_with(".ggpht.com") || h.ends_with(".googleusercontent.com"))
    {
        let path = url.path().to_owned();
        let base = path.split_once('=').map_or(path.as_str(), |(base, _)| base);
        url.set_path(&format!("{base}=s0"));
    }
    Some(url)
}

/// The extension a file's first bytes identify.
fn sniff(bytes: &[u8]) -> &'static str {
    match bytes {
        [0x89, b'P', b'N', b'G', ..] => "png",
        [b'G', b'I', b'F', ..] => "gif",
        [
            b'R',
            b'I',
            b'F',
            b'F',
            _,
            _,
            _,
            _,
            b'W',
            b'E',
            b'B',
            b'P',
            ..,
        ] => "webp",
        _ => "jpg",
    }
}

async fn post_events<'a>(
    http: &'a Http,
    route: &'static Route,
    post: &Map<String, Value>,
) -> Events<'a> {
    let id = post
        .get("postId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let channel = text(post.get("authorText"));
    let content = text(post.get("contentText"));
    let attachment = post.get("backstageAttachment");
    let mut images: Vec<&Value> = Vec::new();
    if let Some(image) = attachment.and_then(|a| a.get("backstageImageRenderer")) {
        images.push(image);
    }
    for image in attachment
        .and_then(|a| a.pointer("/postMultiImageRenderer/images"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(image) = image.get("backstageImageRenderer") {
            images.push(image);
        }
    }
    let mut items = Vec::new();
    for image in images {
        let Some(url) = original(image) else { continue };
        let num = items.len() + 1;
        // Only the signature is needed; failures fall back to JPEG.
        let probe = match http.api(url.as_str()) {
            Ok(probe) => http
                .get(&probe, &http.browser_headers(&[("Range", "bytes=0-15")]))
                .await
                .map(|bytes| sniff(&bytes))
                .unwrap_or("jpg"),
            Err(_) => "jpg",
        };
        let mut keywords = Map::new();
        keywords.insert("post_id".into(), id.clone().into());
        keywords.insert("channel".into(), channel.clone().into());
        keywords.insert("content".into(), content.clone().into());
        keywords.insert(
            "published".into(),
            text(post.get("publishedTimeText")).into(),
        );
        keywords.insert("num".into(), num.into());
        keywords.insert("filename".into(), format!("{id}_{num}").into());
        keywords.insert("extension".into(), probe.into());
        items.push(
            Item::new(url, keywords)
                .ids(id.clone(), format!("{id}_{num}"))
                .user(Some(channel.clone()))
                .caption(Some(content.clone())),
        );
    }
    let mut events = crate::sites::events(crate::sites::post(&SITE, http, route, items));
    let video = attachment
        .and_then(|a| a.pointer("/videoRenderer/videoId"))
        .and_then(Value::as_str)
        .filter(|v| {
            v.len() == 11
                && v.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
        });
    if let Some(video) = video
        && let Ok(url) = Url::parse(&format!("{ROOT}/watch?v={video}"))
    {
        let mut data = Map::new();
        data.insert("post_id".into(), id.into());
        let child = crate::sites::queue(http, "youtube", route.subcategory, url, &data);
        events = Box::pin(futures_util::StreamExt::chain(events, child));
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, response, serve};

    #[test]
    fn matches_post_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.youtube.com/post/Ugkxvw3nKUJFYnzgAx5YBGKZ5wuaHdkelGlO"),
            Some(Target::Post("Ugkxvw3nKUJFYnzgAx5YBGKZ5wuaHdkelGlO".into()))
        );
        assert_eq!(
            t("https://www.youtube.com/@NASA/posts"),
            Some(Target::Posts("@NASA".into()))
        );
        assert_eq!(
            t("https://www.youtube.com/channel/UCLA_DiR1FfKNvjuUpBHmylQ/community"),
            Some(Target::Posts("channel/UCLA_DiR1FfKNvjuUpBHmylQ".into()))
        );
        for raw in [
            "https://www.youtube.com/watch?v=abcdefghijk",
            "https://www.youtube.com/@NASA",
            "https://www.youtube.com/@NASA/videos",
            "https://evil.test/post/abc",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
        assert_eq!(sniff(b"\x89PNG\r\n"), "png");
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8"), "webp");
        assert_eq!(sniff(b"\xff\xd8\xff"), "jpg");
    }

    fn page_with(data: Value) -> Vec<u8> {
        html(&format!(
            r#"<script>ytcfg.set({{"INNERTUBE_CLIENT_VERSION":"2.1"}});</script><script>var ytInitialData = {data};</script>"#
        ))
    }

    fn image(url: &str) -> Value {
        json!({"image": {"thumbnails": [{"url": format!("{url}=s100")}, {"url": format!("{url}=s427-c-rw")}]}})
    }

    #[tokio::test]
    async fn extracts_post_images_at_original_size() {
        let post = json!({"backstagePostThreadRenderer": {"post": {"backstagePostRenderer": {
            "postId": "Ugkx1", "authorText": {"runs": [{"text": "NASA"}]},
            "contentText": {"runs": [{"text": "Moon "}, {"text": "day"}]},
            "backstageAttachment": {"postMultiImageRenderer": {"images": [
                {"backstageImageRenderer": image("https://yt3.ggpht.com/a")},
                {"backstageImageRenderer": image("https://yt3.ggpht.com/b")}]}}}}}});
        let (root, server) = serve(vec![
            page_with(json!({"contents": [post]})),
            response("206 Partial Content", "image/png", b"\x89PNG\r\n\x1a\n"),
            response("206 Partial Content", "image/jpeg", b"\xff\xd8\xff\xe0"),
        ]);
        let http = http("youtube", root.clone());
        // A user's session cookie is sent together with the consent cookie.
        http.set_cookie(&root, "LOGIN_INFO=secret; Path=/");
        let files = media(extract(
            &http,
            Url::parse("https://www.youtube.com/post/Ugkx1").unwrap(),
        ))
        .await;
        let urls: Vec<&str> = files.iter().map(|f| f.url.as_str()).collect();
        assert_eq!(
            urls,
            ["https://yt3.ggpht.com/a=s0", "https://yt3.ggpht.com/b=s0"]
        );
        let names: Vec<String> = files
            .iter()
            .map(|f| {
                crate::sites::default_templates(f)
                    .unwrap()
                    .0
                    .render(f)
                    .unwrap()
            })
            .collect();
        assert_eq!(names, ["Ugkx1_1.png", "Ugkx1_2.jpg"]);
        assert_eq!(keywords(&files[0])["content"], "Moon day");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /post/Ugkx1 "));
        assert!(requests[0].contains("SOCS=CAI"));
        assert!(requests[0].contains("LOGIN_INFO=secret"));
        assert!(
            requests[1]
                .to_ascii_lowercase()
                .contains("range: bytes=0-15")
        );
    }

    #[tokio::test]
    async fn posts_tab_follows_continuations() {
        let post = |id: &str| {
            json!({"backstagePostThreadRenderer": {"post": {"backstagePostRenderer": {
                "postId": id, "authorText": {"runs": [{"text": "NASA"}]},
                "backstageAttachment": {"backstageImageRenderer": image(&format!("https://yt3.ggpht.com/{id}"))}}}}})
        };
        let more = json!({"continuationItemRenderer": {"continuationEndpoint":
            {"continuationCommand": {"token": "tok1"}}}});
        let (root, server) = serve(vec![
            page_with(json!({"contents": [post("p1"), more]})),
            response("206 Partial Content", "image/jpeg", b"\xff\xd8"),
            crate::sites::testing::json(json!({"onResponseReceivedActions": [{
                "appendContinuationItemsAction": {"continuationItems": [post("p2")]}}]})),
            response("206 Partial Content", "image/jpeg", b"\xff\xd8"),
        ]);
        let http = http("youtube", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.youtube.com/@NASA/posts").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /@NASA/posts "));
        assert!(requests[2].starts_with("POST /youtubei/v1/browse?prettyPrint=false "));
        assert!(requests[2].contains("\"continuation\":\"tok1\""));
        assert!(requests[2].contains("\"clientVersion\":\"2.1\""));
    }
}
