//! Arca.live posts, boards and users over its app API, mirroring
//! gallery_dl/extractor/arcalive.py.
use crate::error;
use crate::sites::gdl::{
    captures, date, extr, generate_token, group, nameext_into, parse_int, parse_iso, parse_query,
    partition, re, rpartition, str_of, truthy, unescape, unquote,
};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use std::sync::OnceLock;
use std::time::Duration;
use url::Url;

const ROOT: &str = "https://arca.live";
const API: &str = "https://arca.live/api/app";
const BASE: &str = r"^https?://(?:www\.)?arca\.live";
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
    cookies: Some("arca.live"),
};

#[derive(Debug, PartialEq)]
enum Target {
    Post(String),
    Board(String, String),
    User(String, String),
}

fn target(url: &Url) -> Option<Target> {
    if let Some(c) = captures(re!(&format!(r"{BASE}/b/(?:\w+)/(\d+)")), url) {
        return Some(Target::Post(group(&c, 1).into()));
    }
    if let Some(c) = captures(re!(&format!(r"{BASE}/b/([^/?#]+)/?(?:\?([^#]+))?$")), url) {
        return Some(Target::Board(group(&c, 1).into(), group(&c, 2).into()));
    }
    captures(re!(&format!(r"{BASE}/u/@([^/?#]+)/?(?:\?([^#]+))?$")), url)
        .map(|c| Target::User(group(&c, 1).into(), group(&c, 2).into()))
}

/// The session's `X-Device-Token` (upstream `util.generate_token(64)`).
fn device_token() -> &'static str {
    static TOKEN: OnceLock<String> = OnceLock::new();
    TOKEN.get_or_init(|| generate_token(64))
}

async fn call(http: &Http, endpoint: &str, params: &[(String, String)]) -> crate::Result<Value> {
    // Upstream `request_interval = (0.5, 1.5)`.
    if !cfg!(test) {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let mut url = http.api(&format!("{API}{endpoint}"))?;
    if !params.is_empty() {
        url.query_pairs_mut().extend_pairs(params.iter());
    }
    http.json_with(
        &url,
        &[
            ("User-Agent", USER_AGENT),
            ("X-Device-Token", device_token()),
        ],
    )
    .await
    .map_err(|e| error(format!("API request failed ({e})")))
}

/// `_extract_files`: images and videos in the article body.
async fn files(http: &Http, content: &str) -> Vec<Map<String, Value>> {
    let emoticons = truthy(http.option("emoticons"));
    let gifs = http.option("gifs").cloned().unwrap_or(Value::Bool(true));
    let check_gifs = gifs.as_str() == Some("check");
    let gifs = truthy(Some(&gifs));
    let mut out = Vec::new();
    for c in re!(r"<(?:img|vide(o)) ([^>]+)").captures_iter(content) {
        let video = c.get(1).is_some();
        let media = &c[2];
        if !emoticons && media.contains("class=\"arca-emoticon\"") {
            continue;
        }
        let src = match extr(media, "data-originalurl=\"", "\"") {
            "" => extr(media, "src=\"", "\""),
            src => src,
        };
        if src.is_empty() {
            continue;
        }
        let src = unescape(src);
        let (src, _, query) = partition(&src, "?");
        let mut url = if src.starts_with("//") {
            format!("https:{}", src.replacen("//ac-p.namu", "//ac-o.namu", 1))
        } else if src.starts_with('/') {
            format!("{ROOT}{src}")
        } else {
            src.to_owned()
        };
        let query = format!("?type=orig&{query}");
        let mut fallback = Vec::new();
        let orig = extr(media, "data-orig=\"", "\"");
        if !orig.is_empty() {
            let (path, _, ext) = rpartition(&url, ".");
            if ext != orig {
                fallback.push(Value::from(format!("{url}{query}")));
                url = format!("{path}.{orig}");
            }
        } else if video && gifs {
            let gif = format!("{}.gif", rpartition(&url, ".").0);
            let use_gif = if check_gifs {
                match http.api(&format!("{gif}{query}")) {
                    Ok(check) => http
                        .head_status(&check, &[("User-Agent", USER_AGENT)])
                        .await
                        .is_ok_and(|status| status < 400),
                    Err(_) => false,
                }
            } else {
                true
            };
            if use_gif {
                fallback.push(Value::from(format!("{url}{query}")));
                url = gif;
            }
        }
        let mut file = Map::new();
        file.insert("url".into(), format!("{url}{query}").into());
        file.insert(
            "width".into(),
            parse_int(extr(media, "width=\"", "\"")).into(),
        );
        file.insert(
            "height".into(),
            parse_int(extr(media, "height=\"", "\"")).into(),
        );
        file.insert("_fallback".into(), fallback.into());
        out.push(file);
    }
    out
}

/// Queue each article's post URL.
fn articles<'a>(
    http: &'a Http,
    route: &'static Route,
    board: Option<String>,
    endpoint: String,
    params: Vec<(String, String)>,
) -> Events<'a> {
    crate::sites::paginate(Some(params), move |state| {
        let board = board.clone();
        let endpoint = endpoint.clone();
        async move {
            let Some(mut params) = state else {
                return Ok(None);
            };
            let data = call(http, &endpoint, &params).await?;
            let posts = data["articles"].as_array().cloned().unwrap_or_default();
            if posts.is_empty() {
                return Ok(None);
            }
            let children = posts
                .iter()
                .filter_map(|article| {
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
                    let url = Url::parse(&format!(
                        "{ROOT}/b/{slug}/{}",
                        crate::sites::scalar(&article["id"])
                    ))
                    .ok()?;
                    Some((url, article.as_object()?.clone()))
                })
                .collect();
            for (key, value) in data["next"].as_object().into_iter().flatten() {
                params.retain(|(k, _)| k != key);
                params.push((key.clone(), crate::sites::scalar(value)));
            }
            Ok(Some((
                crate::sites::galleries::queue(http, &SITE, route, children),
                Some(params),
            )))
        }
    })
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Arca.live URL"));
    };
    let pairs = |query: &str| -> Vec<(String, String)> {
        parse_query(query)
            .into_iter()
            .map(|(k, v)| (k, crate::sites::scalar(&v)))
            .collect()
    };
    match target {
        Target::Post(id) => crate::sites::once(async move {
            let mut post = call(http, &format!("/view/article/breaking/{id}"), &[])
                .await?
                .as_object()
                .cloned()
                .ok_or_else(|| error("Arca.live returned no article"))?;
            let content = post
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let files = files(http, &content).await;
            post.insert("count".into(), files.len().into());
            let created = str_of(&Value::Object(post.clone()), "createdAt").to_owned();
            post.insert(
                "date".into(),
                date(parse_iso(created.get(..19).unwrap_or(&created))),
            );
            let post_url = format!(
                "{ROOT}/b/{}/{}",
                crate::sites::scalar(&post["boardSlug"]),
                crate::sites::scalar(&post["id"])
            );
            post.insert("post_url".into(), post_url.as_str().into());
            let referer = format!("{post_url}?p=1");
            let items = files
                .into_iter()
                .enumerate()
                .filter_map(|(index, file)| {
                    let url = crate::sites::scalar(&file["url"]);
                    post.insert("num".into(), (index + 1).into());
                    post.extend(file);
                    nameext_into(&url, &mut post);
                    Some(
                        Item::new(Url::parse(&url).ok()?, post.clone())
                            .ids(id.clone(), (index + 1).to_string())
                            .header("Referer", &referer)
                            .header("User-Agent", USER_AGENT),
                    )
                })
                .collect();
            Ok(crate::sites::events(crate::sites::post(
                &SITE, http, &POST, items,
            )))
        }),
        Target::Board(board, query) => articles(
            http,
            &BOARD,
            Some(board.clone()),
            format!("/list/channel/{board}"),
            pairs(&query),
        ),
        Target::User(user, query) => {
            let mut params = pairs(&query);
            params.retain(|(k, _)| k != "target" && k != "keyword");
            params.push(("target".into(), "nickname".into()));
            params.push(("keyword".into(), unquote(&user)));
            articles(http, &USER, None, "/list/channel/breaking".into(), params)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{children, http as client, json, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://arca.live/b/breaking/123456789"),
            Some(Target::Post("123456789".into()))
        );
        assert_eq!(
            t("https://arca.live/b/bluearchive?category=x"),
            Some(Target::Board("bluearchive".into(), "category=x".into()))
        );
        assert_eq!(
            t("https://arca.live/u/@%EC%9C%A0%EC%A0%80"),
            Some(Target::User("%EC%9C%A0%EC%A0%80".into(), String::new()))
        );
    }

    #[tokio::test]
    async fn post_extracts_images_and_gif_videos() {
        let content = r#"<img src="//ac-p.namu.la/2024/a.png?expires=1" data-orig="jpg" width="10" height="20"><img class="arca-emoticon" src="//ac-p.namu.la/e.png">
            <video src="//ac-p.namu.la/2024/b.mp4?expires=2" width="30"></video>"#;
        let post = json!({"id": 5, "boardSlug": "art", "title": "Hi", "createdAt": "2024-01-02T03:04:05.000Z", "content": content});
        let (root, server) = serve(vec![json(post)]);
        let http = client("arcalive", root);
        let files = media(extract(
            &http,
            Url::parse("https://arca.live/b/art/5").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].url.as_str(),
            "https://ac-o.namu.la/2024/a.jpg?type=orig&expires=1"
        );
        assert_eq!(
            files[0].handoff["_fallback"],
            json!(["https://ac-o.namu.la/2024/a.png?type=orig&expires=1"])
        );
        assert_eq!(
            files[1].url.as_str(),
            "https://ac-o.namu.la/2024/b.gif?type=orig&expires=2"
        );
        assert_eq!(files[0].headers["Referer"], "https://arca.live/b/art/5?p=1");
        assert_eq!(keywords(&files[1])["width"], 30);
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "5_2 Hi.gif");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("arcalive/art")
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/app/view/article/breaking/5 "));
        assert!(requests[0].contains(USER_AGENT));
    }

    #[tokio::test]
    async fn board_pages_with_next_params() {
        let (root, server) = serve(vec![
            json(json!({"articles": [{"id": 1}], "next": {"before": "x1"}})),
            json(json!({"articles": [], "next": {}})),
        ]);
        let http = client("arcalive", root);
        assert_eq!(
            children(extract(
                &http,
                Url::parse("https://arca.live/b/art").unwrap()
            ))
            .await
            .len(),
            1
        );
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /api/app/list/channel/art?before=x1 "));
    }
}
