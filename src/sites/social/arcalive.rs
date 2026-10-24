//! Arca.live posts, boards and users, mirroring
//! gallery_dl/extractor/arcalive.py through the Android app's API: a post's
//! images and videos come from its HTML content; boards and user searches
//! queue their articles.
//!
//! Options: `emoticons` (default off) and `gifs` (default on; `"check"`
//! probes the GIF version with a GET instead of upstream's HEAD request).
//! Upstream's fallback URLs are not tried, and its request interval is
//! not applied.
use crate::sites::art::{extr, nameext_into, parse_int, str_of};
use crate::sites::media::{iso_ts, parse_query};
use crate::sites::{Events, Http, Item, Route, Site, unescape};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://arca.live";
const USER_AGENT: &str = "net.umanle.arca.android.playstore/0.9.75";
static POST: Route = Route::new(
    "post",
    "{id}_{num}{title:? //[b:230]}.{extension}",
    &["{category}", "{boardSlug}"],
    "{id}_{num}",
);
static BOARD: Route = Route::new("board", "{filename}.{extension}", &["{category}"], "");
static USER: Route = Route::new("user", "{filename}.{extension}", &["{category}"], "");

pub static SITE: Site = Site {
    category: "arcalive",
    name: "Arca.live",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&POST, &BOARD, &USER],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Post(String),
    Board(String, Option<String>),
    User(String, Option<String>),
}

/// `/b/\w+/(\d+)`, then `/b/([^/?#]+)/?(?:\?([^#]+))?$` and
/// `/u/@([^/?#]+)/?(?:\?([^#]+))?$`.
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::art::on_host(url, &["arca.live"]) {
        return None;
    }
    let parts: Vec<&str> = url.path().split('/').collect();
    if let ["", "b", board, rest, ..] = parts.as_slice()
        && !board.is_empty()
        && board.chars().all(crate::sites::media::word_char)
        && let Some(id) = crate::sites::art::leading_digits(rest)
    {
        return Some(Target::Post(id.into()));
    }
    let listing = |name: &str| -> Option<(String, Option<String>)> {
        if name.is_empty() || url.fragment().is_some() {
            return None;
        }
        let query = url.query().map(str::to_owned);
        if query.as_deref() == Some("") {
            return None;
        }
        Some((name.into(), query))
    };
    match parts.as_slice() {
        ["", "b", name] | ["", "b", name, ""] => {
            listing(name).map(|(name, query)| Target::Board(name, query))
        }
        ["", "u", name] | ["", "u", name, ""] => {
            let name = name.strip_prefix('@')?;
            listing(name).map(|(name, query)| Target::User(name, query))
        }
        _ => None,
    }
}

/// `util.generate_token(64)`: 128 hexadecimal digits.
fn device_token() -> String {
    use std::hash::BuildHasher;
    let now = std::time::SystemTime::now();
    let state = std::collections::hash_map::RandomState::new();
    (0u8..8)
        .map(|i| format!("{:016x}", state.hash_one((now, i))))
        .collect()
}

/// `ArcaliveAPI._call`.
async fn call(
    http: &Http,
    token: &str,
    endpoint: &str,
    params: &[(String, String)],
) -> Result<Value> {
    let mut url = http.api(&format!("{ROOT}/api/app{endpoint}"))?;
    if !params.is_empty() {
        url.query_pairs_mut().extend_pairs(params);
    }
    let headers = [("User-Agent", USER_AGENT), ("X-Device-Token", token)];
    http.json_with(&url, &headers)
        .await
        .map_err(|e| error(format!("Arca.live API request failed: {e}")))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let token = device_token();
    match target(&url) {
        Some(Target::Post(id)) => crate::sites::once(async move {
            let post = call(http, &token, &format!("/view/article/breaking/{id}"), &[]).await?;
            let items = parse_post(http, &token, &post).await?;
            Ok(crate::sites::events(crate::sites::post(
                &SITE, http, &POST, items,
            )))
        }),
        Some(Target::Board(board, query)) => {
            let params = parse_query(query.as_deref());
            articles(
                http,
                token,
                &BOARD,
                format!("/list/channel/{board}"),
                params,
                Some(board),
            )
        }
        Some(Target::User(user, query)) => {
            let mut params = parse_query(query.as_deref());
            crate::sites::media::set_param(&mut params, "target", "nickname".into());
            crate::sites::media::set_param(
                &mut params,
                "keyword",
                crate::sites::percent_decode(&user),
            );
            articles(
                http,
                token,
                &USER,
                "/list/channel/breaking".into(),
                params,
                None,
            )
        }
        None => crate::sites::fail(error("unsupported Arca.live URL")),
    }
}

/// `_pagination` over `articles`, queueing each post; `next` holds the
/// following page's parameters.
fn articles<'a>(
    http: &'a Http,
    token: String,
    route: &'static Route,
    endpoint: String,
    params: Vec<(String, String)>,
    board: Option<String>,
) -> Events<'a> {
    crate::sites::paginate(Some(params), move |params| {
        let (token, endpoint, board) = (token.clone(), endpoint.clone(), board.clone());
        async move {
            let Some(mut params) = params else {
                return Ok(None);
            };
            let data = call(http, &token, &endpoint, &params).await?;
            let posts = match data.get("articles") {
                Some(Value::Array(posts)) if !posts.is_empty() => posts.clone(),
                _ => return Ok(None),
            };
            let mut children = Vec::new();
            for article in posts {
                let Value::Object(article) = article else {
                    continue;
                };
                let slug = board
                    .clone()
                    .or_else(|| {
                        article
                            .get("boardSlug")
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                            .map(str::to_owned)
                    })
                    .unwrap_or_else(|| "breaking".into());
                let id = article
                    .get("id")
                    .map(crate::sites::scalar)
                    .unwrap_or_default();
                if let Ok(url) = Url::parse(&format!("{ROOT}/b/{slug}/{id}"))
                    && matches!(target(&url), Some(Target::Post(_)))
                {
                    children.push((url, article));
                }
            }
            for (key, value) in data
                .get("next")
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
            {
                crate::sites::media::set_param(&mut params, key, crate::sites::scalar(value));
            }
            let subcategory = route.subcategory;
            let events = futures_util::StreamExt::boxed(futures_util::StreamExt::flatten(
                futures_util::stream::iter(children.into_iter().map(move |(url, data)| {
                    crate::sites::queue(http, "arcalive", subcategory, url, &data)
                })),
            ));
            Ok(Some((events, Some(params))))
        }
    })
}

/// The `<img …>` and `<video …>` attributes in post content, with whether
/// the tag is a video (`<(?:img|vide(o)) ([^>]+)`).
fn media_tags(content: &str) -> Vec<(bool, &str)> {
    let mut tags = Vec::new();
    let mut rest = content;
    while let Some(start) = rest.find('<') {
        rest = &rest[start + 1..];
        let (video, attrs) = if let Some(attrs) = rest.strip_prefix("img ") {
            (false, attrs)
        } else if let Some(attrs) = rest.strip_prefix("video ") {
            (true, attrs)
        } else {
            continue;
        };
        let end = attrs.find('>').unwrap_or(attrs.len());
        if end > 0 {
            tags.push((video, &attrs[..end]));
        }
    }
    tags
}

/// `ArcalivePostExtractor.items` and `_extract_files`.
async fn parse_post(http: &Http, token: &str, post: &Value) -> Result<Vec<Item>> {
    let Some(mut data) = post.as_object().cloned() else {
        return Err(error("Arca.live post response is not an object"));
    };
    let emoticons = http.option_bool("emoticons", false);
    let gifs = http.option_bool("gifs", true);
    let check = http.option_str("gifs") == Some("check");
    let content = str_of(post, "content").to_owned();
    let mut files = Vec::new();
    for (video, attrs) in media_tags(&content) {
        if !emoticons && attrs.contains("class=\"arca-emoticon\"") {
            continue;
        }
        let src = match extr(attrs, "data-originalurl=\"", "\"") {
            "" => extr(attrs, "src=\"", "\""),
            src => src,
        };
        if src.is_empty() {
            continue;
        }
        let src = unescape(src);
        let (src, query) = src.split_once('?').unwrap_or((&src, ""));
        let mut url = if src.starts_with("//") {
            format!("https:{}", src.replacen("//ac-p.namu", "//ac-o.namu", 1))
        } else if src.starts_with('/') {
            format!("{ROOT}{src}")
        } else {
            src.to_owned()
        };
        let query = format!("?type=orig&{query}");
        let orig = extr(attrs, "data-orig=\"", "\"");
        if !orig.is_empty() {
            if let Some((path, ext)) = url.rsplit_once('.')
                && ext != orig
            {
                url = format!("{path}.{orig}");
            }
        } else if video && gifs {
            let gif = format!("{}.gif", url.rsplit_once('.').map_or("", |(path, _)| path));
            if !check {
                url = gif;
            } else if let Ok(probe) = http.api(&format!("{gif}{query}"))
                && http
                    .get(
                        &probe,
                        &[("User-Agent", USER_AGENT), ("X-Device-Token", token)],
                    )
                    .await
                    .is_ok()
            {
                url = gif;
            }
        }
        files.push((
            format!("{url}{query}"),
            parse_int(extr(attrs, "width=\"", "\""), 0),
            parse_int(extr(attrs, "height=\"", "\""), 0),
        ));
    }
    data.insert("count".into(), files.len().into());
    let created = str_of(post, "createdAt");
    let ts = iso_ts(created.get(..19).unwrap_or(created));
    data.insert("date".into(), crate::filter::source_date_json(ts));
    let post_url = format!(
        "{ROOT}/b/{}/{}",
        str_of(post, "boardSlug"),
        post.get("id").map(crate::sites::scalar).unwrap_or_default()
    );
    data.insert("post_url".into(), post_url.clone().into());
    let referer = format!("{post_url}?p=1");
    let id = data.get("id").map(crate::sites::scalar).unwrap_or_default();
    let title = data.get("title").and_then(Value::as_str).map(str::to_owned);
    let author = data
        .get("nickname")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let mut items = Vec::new();
    for (index, (raw, width, height)) in files.into_iter().enumerate() {
        let Some(url) = crate::sites::media::web_url(&raw) else {
            continue;
        };
        let mut fields: Map<String, Value> = data.clone();
        fields.insert("num".into(), (index + 1).into());
        fields.insert("url".into(), raw.clone().into());
        fields.insert("width".into(), width.into());
        fields.insert("height".into(), height.into());
        nameext_into(&raw, &mut fields);
        items.push(
            Item::new(url, fields)
                .ids(id.clone(), (index + 1).to_string())
                .user(author.clone())
                .caption(title.clone())
                .timestamp(ts)
                .header("Referer", &referer)
                .header("User-Agent", USER_AGENT),
        );
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http, json, keywords, media, queued, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://arca.live/b/breaking/123456789?p=1"),
            Some(Target::Post("123456789".into()))
        );
        assert_eq!(
            t("https://arca.live/b/breaking"),
            Some(Target::Board("breaking".into(), None))
        );
        assert_eq!(
            t("https://arca.live/b/bluearchive/?category=x"),
            Some(Target::Board(
                "bluearchive".into(),
                Some("category=x".into())
            ))
        );
        assert_eq!(
            t("https://www.arca.live/u/@Nick%20Name/"),
            Some(Target::User("Nick%20Name".into(), None))
        );
        for raw in [
            "https://arca.live/b/x-y/123",
            "https://arca.live/b/x/y",
            "https://arca.live/b/x#top",
            "https://arca.live/u/Nick",
            "https://arca.live.evil/b/x/1",
            "https://arca.live:81/b/x/1",
            "https://u@arca.live/b/x/1",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[test]
    fn finds_media_tags() {
        let tags = media_tags(r#"<p><img src="a"><video src="b" ><vid><img >x<img"#);
        assert_eq!(tags, [(false, "src=\"a\""), (true, "src=\"b\" ")]);
    }

    #[tokio::test]
    async fn extracts_post() {
        let content = r#"<img src="//ac-p.namu.la/a/one.png?expires=1&amp;key=k" width="10" height="20">
            <img class="arca-emoticon" src="//ac-p.namu.la/e.png">
            <video src="//ac-p.namu.la/b/two.mp4?k=2" width="5">
            <img src="//ac-p.namu.la/c/three.webp?k=3" data-orig="gif">"#;
        let post = json!({"id": 99, "boardSlug": "breaking", "title": "Hello",
            "nickname": "Nick", "createdAt": "2024-01-02T03:04:05.000Z", "content": content});
        let (root, server) = serve(vec![json(post)]);
        let http = http("arcalive", root);
        let files = media(extract(
            &http,
            Url::parse("https://arca.live/b/breaking/99").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 3);
        assert_eq!(
            files[0].url.as_str(),
            "https://ac-o.namu.la/a/one.png?type=orig&expires=1&key=k"
        );
        assert_eq!(
            files[1].url.as_str(),
            "https://ac-o.namu.la/b/two.gif?type=orig&k=2"
        );
        assert_eq!(
            files[2].url.as_str(),
            "https://ac-o.namu.la/c/three.gif?type=orig&k=3"
        );
        assert_eq!(
            files[0].headers["Referer"],
            "https://arca.live/b/breaking/99?p=1"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["width"], 10);
        assert_eq!(k["count"], 3);
        assert_eq!(k["post_url"], "https://arca.live/b/breaking/99");
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "99_2 Hello.gif");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("arcalive/breaking")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{id}_{num}",["99","1"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/app/view/article/breaking/99 "));
        assert!(requests[0].contains(&format!("user-agent: {USER_AGENT}")));
        assert!(requests[0].contains("x-device-token: "));
    }

    #[tokio::test]
    async fn queues_board_articles() {
        let (root, server) = serve(vec![
            json(
                json!({"articles": [{"id": 1}, {"id": 2, "boardSlug": "other"}],
                "next": {"before": "2"}}),
            ),
            json(json!({"articles": []})),
        ]);
        let http = http("arcalive", root);
        let children = queued(extract(
            &http,
            Url::parse("https://arca.live/b/games?mode=best").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 2);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/app/list/channel/games?mode=best "));
        assert!(requests[1].starts_with("GET /api/app/list/channel/games?mode=best&before=2 "));
    }

    #[tokio::test]
    async fn user_searches_breaking() {
        let (root, server) = serve(vec![json(json!({"articles": []}))]);
        let http = http("arcalive", root);
        assert!(
            queued(extract(
                &http,
                Url::parse("https://arca.live/u/@Nick%20A").unwrap()
            ))
            .await
            .is_empty()
        );
        let requests = server.join().unwrap();
        assert!(
            requests[0]
                .starts_with("GET /api/app/list/channel/breaking?target=nickname&keyword=Nick+A ")
        );
    }
}
