//! Fantia fanclubs, posts and supported fanclubs, mirroring
//! gallery_dl/extractor/fantia.py through the `api/v1/posts` JSON API.
//!
//! Paid content needs a `_session_id` cookie from `--cookies`; files on the
//! site's own hosts are fetched with it.
use super::{date, digits, extr, extract_iter, leading_digits, on_host, str_of, truthy};
use crate::error;
use crate::sites::{Events, Http, Item, Route, Site};
use futures_util::{StreamExt, stream};
use serde_json::{Map, Value, json};
use std::sync::Arc;
use url::Url;

const ROOT: &str = "https://fantia.jp";

pub static SITE: Site = Site {
    category: "fantia",
    name: "Fantia",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&CREATOR, &POST, &SUPPORTING],
    api_key: false,
    cookies: Some("fantia.jp"),
};

const FILENAME: &str = "{post_id}_{file_id}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{fanclub_id}"];
const ARCHIVE: &str = "{post_id}_{file_id}";

static CREATOR: Route = Route::new("creator", FILENAME, DIRECTORY, ARCHIVE);
static POST: Route = Route::new("post", FILENAME, DIRECTORY, ARCHIVE);
static SUPPORTING: Route = Route::new("supporting", FILENAME, DIRECTORY, ARCHIVE);

#[derive(Debug, PartialEq)]
enum Target {
    Creator(String),
    Post(String),
    /// The `type` filter; `None` lists paid and then free plans.
    Supporting(Option<&'static str>),
}

fn target(url: &Url) -> Option<Target> {
    if !on_host(url, &["fantia.jp"]) {
        return None;
    }
    let path = url.path();
    if let Some(id) = path.strip_prefix("/fanclubs/") {
        return Some(Target::Creator(leading_digits(id)?.to_owned()));
    }
    if let Some(id) = path.strip_prefix("/posts/") {
        return Some(Target::Post(leading_digits(id)?.to_owned()));
    }
    let rest = path.strip_prefix("/mypage/users/plans")?;
    // `(?:\?type=((?:not_)?free))?` directly after the path.
    let kind = match url.query() {
        Some(q) if rest.is_empty() && q.starts_with("type=not_free") => Some("not_free"),
        Some(q) if rest.is_empty() && q.starts_with("type=free") => Some("free"),
        _ => None,
    };
    Some(Target::Supporting(kind))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Fantia URL"));
    };
    match target {
        Target::Post(id) => crate::sites::once(async move {
            let page = http.text(&http.api(&format!("{ROOT}/"))?).await?;
            Ok(posts(http, &POST, vec![id], csrf_token(&page)))
        }),
        Target::Creator(id) => pagination(
            http,
            format!("{ROOT}/fanclubs/{id}/posts"),
            None,
            "class=\"link-block\" href=\"/posts/",
            Arc::new(move |ids, token| posts(http, &CREATOR, ids, token)),
        ),
        Target::Supporting(kind) => {
            let kinds = match kind {
                Some(kind) => vec![kind],
                None => vec!["not_free", "free"],
            };
            stream::iter(kinds)
                .map(move |kind| {
                    pagination(
                        http,
                        format!("{ROOT}/mypage/users/plans"),
                        Some(kind),
                        "class=\"user-avatar\" href=\"/fanclubs/",
                        Arc::new(move |ids, _| fanclubs(http, ids)),
                    )
                })
                .flatten()
                .boxed()
        }
    }
}

type Each<'a> = Arc<dyn Fn(Vec<String>, String) -> Events<'a> + Send + Sync + 'a>;

/// Upstream `_pagination`: numbered listing pages until one has no IDs;
/// `each` gets a page's IDs and the CSRF token it carries.
fn pagination<'a>(
    http: &'a Http,
    url: String,
    kind: Option<&'static str>,
    needle: &'static str,
    each: Each<'a>,
) -> Events<'a> {
    crate::sites::paginate(Some(1u64), move |page| {
        let (url, each) = (url.clone(), each.clone());
        async move {
            let Some(page) = page else {
                return Ok(None);
            };
            let mut url = http.api(&url)?;
            {
                let mut query = url.query_pairs_mut();
                if let Some(kind) = kind {
                    query.append_pair("type", kind);
                }
                query.append_pair("page", &page.to_string());
            }
            let text = http.text(&url).await?;
            let found = extract_iter(&text, needle, "\"");
            let next = found
                .last()
                .is_some_and(|id| !id.is_empty())
                .then_some(page + 1);
            let ids = found
                .into_iter()
                .filter(|id| digits(id))
                .map(str::to_owned)
                .collect();
            Ok(Some((each(ids, csrf_token(&text)), next)))
        }
    })
}

fn csrf_token(page: &str) -> String {
    extr(page, "name=\"csrf-token\" content=\"", "\"").to_owned()
}

/// Queue each supported fanclub for the creator extractor.
fn fanclubs(http: &Http, ids: Vec<String>) -> Events<'_> {
    stream::iter(ids)
        .filter_map(|id| async move { Url::parse(&format!("{ROOT}/fanclubs/{id}")).ok() })
        .map(move |url| crate::sites::queue(http, "fantia", "supporting", url, &Map::new()))
        .flatten()
        .boxed()
}

/// Fetch each post as it is reached.
fn posts<'a>(http: &'a Http, route: &'static Route, ids: Vec<String>, token: String) -> Events<'a> {
    stream::iter(ids)
        .map(move |id| {
            let token = token.clone();
            crate::sites::once(async move {
                let url = http.api(&format!("{ROOT}/api/v1/posts/{id}"))?;
                let headers = [
                    ("Accept", "application/json, text/plain, */*"),
                    ("X-Requested-With", "XMLHttpRequest"),
                    ("X-CSRF-Token", token.as_str()),
                ];
                let resp = http.json_with(&url, &headers).await?;
                let post = resp
                    .get("post")
                    .ok_or_else(|| error("Fantia returned no post"))?;
                let items = parse_post(post)
                    .into_iter()
                    .map(|item| super::session_cookies(http, item))
                    .collect();
                Ok(crate::sites::events(crate::sites::post(
                    &SITE, http, route, items,
                )))
            })
        })
        .flatten()
        .boxed()
}

fn get<'v>(value: &'v Value, path: &[&str]) -> &'v Value {
    path.iter()
        .try_fold(value, |value, key| value.get(key))
        .unwrap_or(&Value::Null)
}

/// Upstream `_get_post_data`, `_get_post_contents` and `_process_content`:
/// one file per photo, download and blog image, numbered across contents.
pub(crate) fn parse_post(resp: &Value) -> Vec<Item> {
    let id = resp.get("id").cloned().unwrap_or(Value::Null);
    let post_id = crate::sites::scalar(&id);
    let timestamp = super::strptime(str_of(resp, "posted_at"), "%a, %d %b %Y %H:%M:%S %z");
    let fanclub_id = get(resp, &["fanclub", "id"]).clone();
    let mut contents = resp
        .get("post_contents")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if let Some(thumb) = resp.get("thumb").and_then(|t| t.get("original")) {
        contents.insert(
            0,
            json!({"id": "thumb", "title": "thumb", "category": "thumb",
                "download_uri": thumb, "visible_status": "visible", "plan": null}),
        );
    }
    // Upstream inserts the thumbnail into the response's own list.
    let mut raw = resp.clone();
    if let Some(raw) = raw.as_object_mut() {
        raw.insert("post_contents".into(), contents.clone().into());
    }
    let tags: Vec<Value> = resp
        .get("tags")
        .and_then(Value::as_array)
        .map(|tags| tags.iter().map(|t| get(t, &["name"]).clone()).collect())
        .unwrap_or_default();
    let mut post = Map::new();
    post.insert("post_id".into(), id);
    post.insert("post_url".into(), format!("{ROOT}/posts/{post_id}").into());
    for (key, field) in [
        ("post_title", "title"),
        ("comment", "comment"),
        ("rating", "rating"),
        ("posted_at", "posted_at"),
    ] {
        post.insert(key.into(), get(resp, &[field]).clone());
    }
    post.insert("date".into(), date(timestamp));
    post.insert("fanclub_id".into(), fanclub_id.clone());
    post.insert(
        "fanclub_user_id".into(),
        get(resp, &["fanclub", "user", "id"]).clone(),
    );
    post.insert(
        "fanclub_user_name".into(),
        get(resp, &["fanclub", "user", "name"]).clone(),
    );
    post.insert(
        "fanclub_name".into(),
        get(resp, &["fanclub", "name"]).clone(),
    );
    post.insert(
        "fanclub_url".into(),
        format!("{ROOT}/fanclubs/{}", crate::sites::scalar(&fanclub_id)).into(),
    );
    post.insert("tags".into(), tags.into());
    post.insert("_data".into(), raw);
    post.insert("num".into(), 0.into());
    post.insert("content_count".into(), contents.len().into());
    post.insert("content_num".into(), 0.into());
    let user = get(resp, &["fanclub", "user", "name"])
        .as_str()
        .map(str::to_owned);
    let title = str_of(resp, "title").to_owned();
    let mut num = 0u64;
    let mut items = Vec::new();
    for (index, content) in contents.iter().enumerate() {
        let files = process_content(&mut post, content, index + 1);
        for (file_id, file_url) in files {
            num += 1;
            post.insert("file_id".into(), file_id.clone());
            post.insert("file_url".into(), file_url.clone().into());
            post.insert("num".into(), num.into());
            let name = match post.get("content_filename").and_then(Value::as_str) {
                Some(name) if !name.is_empty() => name.to_owned(),
                _ => file_url.clone(),
            };
            super::nameext_into(&name, &mut post);
            let Some(url) = super::file_url(&file_url, ROOT) else {
                continue;
            };
            items.push(
                Item::new(url, post.clone())
                    .ids(post_id.clone(), crate::sites::scalar(&file_id))
                    .user(user.clone())
                    .caption(Some(title.clone()))
                    .timestamp(timestamp),
            );
        }
    }
    items
}

/// One content's keywords, written into `post`, and its `(file_id,
/// file_url)` pairs.
fn process_content(
    post: &mut Map<String, Value>,
    content: &Value,
    num: usize,
) -> Vec<(Value, String)> {
    let field = |key: &str| content.get(key).cloned().unwrap_or(Value::Null);
    let or_empty = |key: &str| match content.get(key) {
        Some(value) if truthy(Some(value)) => value.clone(),
        _ => "".into(),
    };
    post.insert("content_category".into(), field("category"));
    post.insert("content_title".into(), field("title"));
    post.insert("content_filename".into(), or_empty("filename"));
    post.insert("content_id".into(), field("id"));
    post.insert("content_comment".into(), or_empty("comment"));
    post.insert("content_num".into(), num.into());
    let plan = match content.get("plan") {
        Some(plan) if truthy(Some(plan)) => plan.clone(),
        _ => json!({"id": 0, "price": 0, "limit": 0, "name": "", "description": "",
            "thumb": format!("{ROOT}/images/fallback/plan/thumb_default.png")}),
    };
    post.insert("plan".into(), plan);

    let mut files = Vec::new();
    for photo in content
        .get("post_content_photos")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(url) = get(photo, &["url", "original"]).as_str() {
            files.push((field_of(photo, "id"), url.to_owned()));
        }
    }
    if let Some(url) = content.get("download_uri").and_then(Value::as_str) {
        let url = if url.starts_with('/') {
            format!("{ROOT}{url}")
        } else {
            url.to_owned()
        };
        files.push((field("id"), url));
    }
    let mut text = String::new();
    if str_of(content, "category") == "blog"
        && let Some(comment) = content.get("comment").and_then(Value::as_str)
    {
        let ops: Value = serde_json::from_str(comment).unwrap_or(Value::Null);
        for op in ops
            .get("ops")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            match op.get("insert") {
                Some(Value::String(insert)) => text.push_str(insert),
                Some(insert) => {
                    if let Some(image) = insert.get("fantiaImage") {
                        let path = str_of(image, "original_url");
                        files.push((field_of(image, "id"), format!("{ROOT}{path}")));
                    }
                }
                None => {}
            }
        }
    }
    post.insert("blogpost_text".into(), text.into());
    files
}

fn field_of(value: &Value, key: &str) -> Value {
    value.get(key).cloned().unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, json, keywords, media, queued, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://fantia.jp/fanclubs/6939"),
            Some(Target::Creator("6939".into()))
        );
        assert_eq!(
            t("https://www.fantia.jp/posts/508363?x=1"),
            Some(Target::Post("508363".into()))
        );
        assert_eq!(
            t("https://fantia.jp/mypage/users/plans"),
            Some(Target::Supporting(None))
        );
        assert_eq!(
            t("https://fantia.jp/mypage/users/plans?type=free"),
            Some(Target::Supporting(Some("free")))
        );
        assert_eq!(
            t("https://fantia.jp/mypage/users/plans?type=not_free"),
            Some(Target::Supporting(Some("not_free")))
        );
        for raw in [
            "https://fantia.jp/fanclubs/abc",
            "https://fantia.jp/posts/",
            "https://fantia.jp/",
            "https://fantia.jp.evil.com/posts/1",
            "https://evilfantia.jp/posts/1",
            "https://cc.fantia.jp/posts/1",
            "https://fantia.jp:8443/posts/1",
            "https://u:p@fantia.jp/posts/1",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn post_json() -> Value {
        json!({"post": {
        "id": 508363, "title": "Title", "comment": "Hi", "rating": "general",
        "posted_at": "Sat, 23 Jan 2021 12:00:00 +0900",
        "thumb": {"original": "https://cc.fantia.jp/uploads/post/file/508363/thumb.jpg"},
        "fanclub": {"id": 6939, "name": "Club", "user": {"id": 42, "name": "Artist"}},
        "tags": [{"name": "tag1"}, {"name": "tag2"}],
        "post_contents": [
            {"id": 1, "title": "Photos", "category": "photo_gallery", "visible_status": "visible",
             "plan": {"id": 5, "price": 500},
             "post_content_photos": [
                {"id": 11, "url": {"original": "https://cc.fantia.jp/uploads/a.png?Key=1"}},
                {"id": 12, "url": {"original": "https://cc.fantia.jp/uploads/b.jpg"}}]},
            {"id": 2, "title": "File", "category": "file", "visible_status": "visible",
             "filename": "archive.zip", "plan": null,
             "download_uri": "/posts/508363/download/2"},
            {"id": 3, "title": "Blog", "category": "blog", "visible_status": "visible",
             "comment": "{\"ops\":[{\"insert\":\"Hello \"},{\"insert\":{\"fantiaImage\":{\"id\":31,\"original_url\":\"/posts/508363/album_image?query=x\"}}},{\"insert\":\"world\"}]}"}
        ]}})
    }

    #[tokio::test]
    async fn extracts_post_contents() {
        let token = "<meta name=\"csrf-token\" content=\"tok123\" />";
        let (root, server) = serve(vec![html(token), json(post_json())]);
        let http = http("fantia", root.clone());
        http.set_cookie(&root, "_session_id=sess; Path=/");
        let files = media(extract(
            &http,
            Url::parse("https://fantia.jp/posts/508363").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 5);
        let k = keywords(&files[0]);
        assert_eq!(k["file_id"], "thumb");
        assert_eq!(k["content_category"], "thumb");
        assert_eq!(k["plan"]["price"], 0);
        assert_eq!(k["date"], "2021-01-23 03:00:00");
        assert_eq!(k["fanclub_user_name"], "Artist");
        assert_eq!(k["tags"], json!(["tag1", "tag2"]));
        assert_eq!(k["content_count"], 4);
        assert_eq!(k["_data"]["post_contents"][0]["id"], "thumb");
        let k = keywords(&files[1]);
        assert_eq!(k["file_id"], 11);
        assert_eq!(k["num"], 2);
        assert_eq!(k["content_num"], 2);
        assert_eq!(k["plan"]["id"], 5);
        assert_eq!(k["filename"], "a");
        let k = keywords(&files[3]);
        assert_eq!(k["filename"], "archive");
        assert_eq!(k["extension"], "zip");
        assert_eq!(k["file_url"], "https://fantia.jp/posts/508363/download/2");
        let k = keywords(&files[4]);
        assert_eq!(k["file_id"], 31);
        assert_eq!(k["blogpost_text"], "Hello world");
        assert_eq!(k["content_filename"], "");
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "508363_11.png");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("fantia/6939")
        );
        let (_, identity) = crate::sites::archive_key(&files[3]).unwrap();
        assert_eq!(identity, r#"["{post_id}_{file_id}",["508363","2"]]"#);
        assert_eq!(
            files[3].headers.get("Cookie").map(String::as_str),
            Some("_session_id=sess")
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET / "));
        assert!(requests[1].starts_with("GET /api/v1/posts/508363 "));
        assert!(requests[1].contains("x-csrf-token: tok123"));
        assert!(requests[1].contains("x-requested-with: XMLHttpRequest"));
    }

    #[tokio::test]
    async fn creator_pages_are_lazy() {
        let page = "<meta name=\"csrf-token\" content=\"t1\" />\
            <a class=\"link-block\" href=\"/posts/508363\"></a>";
        let (root, server) = serve(vec![html(page), json(post_json()), html("<p>none</p>")]);
        let http = http("fantia", root);
        let files = media(extract(
            &http,
            Url::parse("https://fantia.jp/fanclubs/6939").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 5);
        assert_eq!(keywords(&files[0])["subcategory"], "creator");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /fanclubs/6939/posts?page=1 "));
        assert!(requests[1].contains("x-csrf-token: t1"));
        assert!(requests[2].starts_with("GET /fanclubs/6939/posts?page=2 "));
    }

    #[tokio::test]
    async fn supporting_queues_fanclubs() {
        let page = "<a class=\"user-avatar\" href=\"/fanclubs/7\"></a>";
        let (root, server) = serve(vec![html(page), html(""), html(page), html("")]);
        let http = http("fantia", root);
        let children = queued(extract(
            &http,
            Url::parse("https://fantia.jp/mypage/users/plans").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].subcategory(), "supporting");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /mypage/users/plans?type=not_free&page=1 "));
        assert!(requests[1].starts_with("GET /mypage/users/plans?type=not_free&page=2 "));
        assert!(requests[2].starts_with("GET /mypage/users/plans?type=free&page=1 "));
    }
}
