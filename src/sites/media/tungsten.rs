//! Tungsten posts, models and users, mirroring
//! gallery_dl/extractor/tungsten.py: a post page's Next.js data, or the
//! `api.tungsten.run` post listings page by page.
use super::{iso_ts, parse_query, set_param, str_of, web_url, word_char};
use crate::error;
use crate::sites::art::{extr, next_data, on_host, segment};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::Value;
use url::Url;

const ROOT: &str = "https://tungsten.run";
const API: &str = "https://api.tungsten.run/v1";
const FILENAME: &str = "{date} {title:?/ /}{uuid}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{user[username]}"];
static POST: Route = Route::new("post", FILENAME, DIRECTORY, "{uuid}");
static MODEL: Route = Route::new("model", FILENAME, DIRECTORY, "{uuid}");
static USER: Route = Route::new("user", FILENAME, DIRECTORY, "{uuid}");

pub static SITE: Site = Site {
    category: "tungsten",
    name: "Tungsten",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&POST, &MODEL, &USER],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Post(String),
    Model(String, Option<String>),
    User(String, Option<String>),
}

fn word(text: &str) -> Option<&str> {
    let len = text.find(|c: char| !word_char(c)).unwrap_or(text.len());
    (len > 0).then(|| &text[..len])
}

/// `/post/(\w+)`, `/model/(\w+)(?:/?\?model_version=(\w+))?` and
/// `/user/([^/?#]+)(?:/posts)?/?(?:\?([^#]+))?`.
fn target(url: &Url) -> Option<Target> {
    if !on_host(url, &["tungsten.run"]) {
        return None;
    }
    let path = url.path();
    if let Some(rest) = path.strip_prefix("/post/") {
        return Some(Target::Post(word(rest)?.into()));
    }
    if let Some(rest) = path.strip_prefix("/model/") {
        let id = word(rest)?;
        let tail = &rest[id.len()..];
        let version = if matches!(tail, "" | "/") {
            url.query()
                .and_then(|q| q.strip_prefix("model_version="))
                .and_then(word)
                .map(str::to_owned)
        } else {
            None
        };
        return Some(Target::Model(id.into(), version));
    }
    let rest = path.strip_prefix("/user/")?;
    let (user, tail) = rest.split_once('/').unwrap_or((rest, ""));
    if !segment(user) {
        return None;
    }
    let query = if matches!(tail, "" | "posts" | "posts/") {
        url.query().filter(|q| !q.is_empty()).map(str::to_owned)
    } else {
        None
    };
    Some(Target::User(user.into(), query))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Post(id)) => crate::sites::once(async move {
            let page = http.text(&http.api(&format!("{ROOT}/post/{id}"))?).await?;
            let data = next_data(&page, "Tungsten")?;
            let post = data
                .pointer("/props/pageProps/post")
                .cloned()
                .ok_or_else(|| error("Tungsten page has no post"))?;
            Ok(crate::sites::events(post_events(http, &POST, &post, None)))
        }),
        Some(Target::Model(id, version)) => crate::sites::once(async move {
            let version = match version {
                Some(version) => version,
                None => {
                    let page = http
                        .text(&http.api(&format!("{ROOT}/model/{id}/"))?)
                        .await?;
                    extr(&page, "\"modelVersionUUID\":\"", "\"").to_owned()
                }
            };
            let params = vec![
                ("sort".into(), "top_all_time".into()),
                ("tweakable_only".into(), "false".into()),
                ("following".into(), "false".into()),
                ("model_version_uuid".into(), version),
            ];
            Ok(pagination(
                http,
                &MODEL,
                format!("{API}/posts"),
                params,
                None,
            ))
        }),
        Some(Target::User(user, query)) => crate::sites::once(async move {
            let page = http
                .text(&http.api(&format!("{ROOT}/user/{user}"))?)
                .await?;
            let uuid = extr(&page, "\"user\":{\"uuid\":\"", "\"");
            if uuid.is_empty() || !uuid.chars().all(|c| c.is_alphanumeric() || c == '-') {
                return Err(error("Tungsten user page has no user ID"));
            }
            let mut params = parse_query(query.as_deref());
            if !params.iter().any(|(k, _)| k == "sort") {
                params.push(("sort".into(), "top_all_time".into()));
            }
            let tags = super::query_get(&params, "tag")
                .unwrap_or_default()
                .to_owned();
            Ok(pagination(
                http,
                &USER,
                format!("{API}/users/{uuid}/posts"),
                params,
                Some(tags),
            ))
        }),
        None => crate::sites::fail(error("unsupported Tungsten URL")),
    }
}

/// `_pagination`: 40 posts per page until a short page.
fn pagination<'a>(
    http: &'a Http,
    route: &'static Route,
    endpoint: String,
    mut params: Vec<(String, String)>,
    tags: Option<String>,
) -> Events<'a> {
    set_param(&mut params, "page", "1".into());
    set_param(&mut params, "per_page", "40".into());
    crate::sites::paginate(Some((params, 1u64)), move |state| {
        let endpoint = endpoint.clone();
        let tags = tags.clone();
        async move {
            let Some((mut params, page)) = state else {
                return Ok(None);
            };
            set_param(&mut params, "page", page.to_string());
            let mut url = http.api(&endpoint)?;
            url.query_pairs_mut().extend_pairs(&params);
            let headers = [
                ("Origin", ROOT),
                ("Sec-Fetch-Dest", "empty"),
                ("Sec-Fetch-Mode", "cors"),
                ("Sec-Fetch-Site", "same-site"),
            ];
            let data = http.json_with(&url, &headers).await?;
            let Value::Array(posts) = data else {
                return Err(error("Tungsten API response is not a list"));
            };
            let mut events = Vec::new();
            for post in &posts {
                events.extend(post_events(http, route, post, tags.as_deref()));
            }
            let next = (posts.len() >= 40).then_some((params, page + 1));
            Ok(Some((crate::sites::events(events), next)))
        }
    })
}

fn post_events(
    http: &Http,
    route: &'static Route,
    post: &Value,
    tags: Option<&str>,
) -> Vec<crate::event::ExtractionEvent> {
    match parse_post(post, tags) {
        Some(item) => crate::sites::post(&SITE, http, route, vec![item]),
        None => Vec::new(),
    }
}

/// `items()` for one post: its `original_url` as a WebP file.
pub(crate) fn parse_post(post: &Value, tags: Option<&str>) -> Option<Item> {
    let mut data = post.as_object()?.clone();
    let raw = str_of(post, "original_url");
    let url = web_url(raw)?;
    let ts = iso_ts(str_of(post, "created_at"));
    data.insert("date".into(), crate::filter::source_date_json(ts));
    data.insert(
        "filename".into(),
        raw.rsplit('/').next().unwrap_or_default().into(),
    );
    data.insert("extension".into(), "webp".into());
    if let Some(tags) = tags {
        data.insert("search_tags".into(), tags.into());
    }
    let uuid = data
        .get("uuid")
        .map(crate::sites::scalar)
        .unwrap_or_default();
    let user = post
        .pointer("/user/username")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let title = post.get("title").and_then(Value::as_str).map(str::to_owned);
    Some(
        Item::new(url, data)
            .ids(uuid.clone(), uuid)
            .user(user)
            .caption(title)
            .timestamp(ts),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, json, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://tungsten.run/post/AbC_1"),
            Some(Target::Post("AbC_1".into()))
        );
        assert_eq!(
            t("https://tungsten.run/model/m1/?model_version=v2"),
            Some(Target::Model("m1".into(), Some("v2".into())))
        );
        assert_eq!(
            t("https://www.tungsten.run/model/m1"),
            Some(Target::Model("m1".into(), None))
        );
        assert_eq!(
            t("https://tungsten.run/user/bob/posts?tag=cat&sort=new"),
            Some(Target::User("bob".into(), Some("tag=cat&sort=new".into())))
        );
        for raw in [
            "https://tungsten.run/post/",
            "https://tungsten.run/user/",
            "https://tungsten.run.evil/post/a",
            "https://tungsten.run:81/post/a",
            "https://u@tungsten.run/post/a",
            "https://api.tungsten.run/post/a",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn post(uuid: &str) -> Value {
        json!({"uuid": uuid, "title": "Cat", "created_at": "2024-01-02T03:04:05Z",
            "original_url": format!("https://cdn.tungsten.run/o/{uuid}"),
            "user": {"username": "bob"}})
    }

    #[tokio::test]
    async fn extracts_post() {
        let page = format!(
            r#"<script id="__NEXT_DATA__" type="application/json">{}</script>"#,
            json!({"props": {"pageProps": {"post": post("u1")}}})
        );
        let (root, server) = serve(vec![html(&page)]);
        let http = http("tungsten", root);
        let files = media(extract(
            &http,
            Url::parse("https://tungsten.run/post/u1").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let k = keywords(&files[0]);
        assert_eq!(k["filename"], "u1");
        assert_eq!(k["extension"], "webp");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            file.render(&files[0]).unwrap(),
            "2024-01-02 03:04:05 Cat u1.webp"
        );
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("tungsten/bob")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{uuid}",["u1"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /post/u1 "));
    }

    #[tokio::test]
    async fn pages_through_user_posts() {
        let full: Vec<Value> = (0..40).map(|i| post(&format!("a{i}"))).collect();
        let (root, server) = serve(vec![
            html(r#"{"user":{"uuid":"abc-1","name":"bob"}}"#),
            json(Value::Array(full)),
            json(json!([post("b")])),
        ]);
        let http = http("tungsten", root);
        let files = media(extract(
            &http,
            Url::parse("https://tungsten.run/user/bob?tag=cat").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 41);
        assert_eq!(keywords(&files[40])["search_tags"], "cat");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /user/bob "));
        assert!(requests[1].starts_with(
            "GET /v1/users/abc-1/posts?tag=cat&sort=top_all_time&page=1&per_page=40 "
        ));
        assert!(requests[1].contains("origin: https://tungsten.run"));
        assert!(requests[2].contains("&page=2&"));
    }

    #[tokio::test]
    async fn model_reads_version_from_page() {
        let (root, server) = serve(vec![html(r#"{"modelVersionUUID":"v9"}"#), json(json!([]))]);
        let http = http("tungsten", root);
        let files = media(extract(
            &http,
            Url::parse("https://tungsten.run/model/m1").unwrap(),
        ))
        .await;
        assert!(files.is_empty());
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /model/m1/ "));
        assert!(requests[1].starts_with(
            "GET /v1/posts?sort=top_all_time&tweakable_only=false&following=false&model_version_uuid=v9&page=1&per_page=40 "
        ));
    }
}
