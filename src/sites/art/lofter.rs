//! LOFTER, mirroring gallery_dl/extractor/lofter.py.
//!
//! Upstream POSTs a form body to the Android API; the same parameters are
//! accepted in the query string of a POST, which is what is sent here.
use super::extract_iter;
use crate::sites::{Events, Http, Item, Route, Site, unescape};
use crate::{Result, error};
use serde_json::{Map, Value, json};
use url::Url;

pub static SITE: Site = Site {
    category: "lofter",
    name: "LOFTER",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&POST, &BLOG_POSTS],
    api_key: false,
};

const FILENAME: &str = "{id}_{num}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{blog_name}"];
static POST: Route = Route::new("post", FILENAME, DIRECTORY, "{id}_{num}");
static BLOG_POSTS: Route = Route::new("blog-posts", FILENAME, DIRECTORY, "{id}_{num}");

const REFERER: &str = "https://www.lofter.com/";

#[derive(Debug, PartialEq)]
enum Target {
    Post { blog: u64, post: u64 },
    Blog(String),
}

fn hex(text: &str) -> Option<u64> {
    let len = text
        .bytes()
        .take_while(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
        .count();
    if len == 0 || len > 16 {
        return None;
    }
    u64::from_str_radix(&text[..len], 16).ok()
}

/// `[\w-]+` limited to DNS-safe ASCII.
fn name(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 64
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn target(url: &Url) -> Option<Target> {
    let user = super::subdomain(url, "lofter.com")?;
    let path = url.path();
    if let Some(ids) = path.strip_prefix("/post/") {
        let (blog, post) = ids.split_once('_')?;
        if !blog.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) {
            return None;
        }
        return Some(Target::Post {
            blog: hex(blog).filter(|_| !blog.is_empty())?,
            post: hex(post)?,
        });
    }
    if user == "www" {
        let blog = path.strip_prefix("/front/blog/home-page/")?;
        let blog = blog.strip_suffix('/').unwrap_or(blog);
        return name(blog).then(|| Target::Blog(blog.into()));
    }
    matches!(path, "" | "/").then(|| Target::Blog(user.into()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Post { blog, post }) => crate::sites::once(async move {
            let response = call(
                http,
                "/oldapi/post/detail.api",
                &[
                    ("targetblogid", blog.to_string()),
                    ("postid", post.to_string()),
                ],
            )
            .await?;
            let post = response
                .get("posts")
                .and_then(|p| p.get(0))
                .ok_or_else(|| error("LOFTER post response contains no post"))?;
            Ok(crate::sites::events(post_events(http, &POST, post)))
        }),
        Some(Target::Blog(blog)) => blog_posts(http, blog),
        None => crate::sites::fail(error("unsupported LOFTER URL")),
    }
}

async fn call(http: &Http, endpoint: &str, params: &[(&str, String)]) -> Result<Value> {
    let mut url = http.api(&format!("https://api.lofter.com{endpoint}"))?;
    url.query_pairs_mut()
        .append_pair("product", "lofter-android-7.9.10")
        .extend_pairs(params);
    let info = http
        .post_json(&url, &json!({}), &[("Referer", REFERER)])
        .await?;
    match info.get("meta").and_then(|m| m.get("status")).and_then(Value::as_i64) {
        Some(200) => Ok(info.get("response").cloned().unwrap_or(Value::Null)),
        Some(4200) => Err(error("LOFTER blog not found")),
        _ => Err(error("LOFTER API request failed")),
    }
}

fn blog_posts(http: &Http, blog: String) -> Events<'_> {
    crate::sites::paginate(Some(0i64), move |offset| {
        let blog = blog.clone();
        async move {
            let Some(offset) = offset else {
                return Ok(None);
            };
            let data = call(
                http,
                "/v2.0/blogHomePage.api",
                &[
                    ("method", "getPostLists".into()),
                    ("offset", offset.to_string()),
                    ("limit", "200".into()),
                    ("blogdomain", format!("{blog}.lofter.com")),
                ],
            )
            .await?;
            let posts = data
                .get("posts")
                .and_then(Value::as_array)
                .ok_or_else(|| error("LOFTER blog response contains no posts"))?;
            let mut events = Vec::new();
            for post in posts {
                events.extend(post_events(http, &BLOG_POSTS, post));
            }
            let next_offset = data.get("offset").and_then(Value::as_i64).unwrap_or(-1);
            let next = (next_offset >= 0 && offset + posts.len() as i64 >= next_offset)
                .then_some(next_offset);
            Ok(Some((crate::sites::events(events), next)))
        }
    })
}

fn strip_query(url: &str) -> String {
    url.split('?').next().unwrap_or_default().to_owned()
}

fn json_list(post: &Map<String, Value>, key: &str) -> Vec<Value> {
    post.get(key)
        .and_then(Value::as_str)
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
}

fn origins(list: Vec<Value>) -> Vec<String> {
    list.iter()
        .filter_map(|x| x.get("orign").and_then(Value::as_str))
        .map(strip_query)
        .collect()
}

pub(crate) fn post_items(post: &Value) -> Vec<Item> {
    if post.is_null() {
        return Vec::new();
    }
    let post = post.get("post").unwrap_or(post);
    let Some(mut post) = post.as_object().cloned() else {
        return Vec::new();
    };
    let blog_name = post
        .get("blogInfo")
        .and_then(|b| b.get("blogName"))
        .cloned()
        .unwrap_or(Value::Null);
    post.insert("blog_name".into(), blog_name.clone());
    let timestamp = post
        .get("publishTime")
        .and_then(Value::as_f64)
        .map(|ms| (ms / 1000.0).trunc() as i64);
    post.insert("date".into(), super::date(timestamp));
    let urls: Vec<String> = match post.get("type").and_then(Value::as_i64) {
        Some(1) => extract_iter(
            post.get("content").and_then(Value::as_str).unwrap_or_default(),
            "<img src=\"",
            "\"",
        )
        .into_iter()
        .map(|x| strip_query(&unescape(x)))
        .collect(),
        Some(2) => origins(json_list(&post, "photoLinks")),
        Some(4) => post
            .get("embed")
            .and_then(Value::as_str)
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
            .and_then(|e| e.get("originUrl")?.as_str().map(str::to_owned))
            .into_iter()
            .collect(),
        Some(5) => origins(json_list(&post, "images")),
        // Upstream warns about unsupported post types.
        _ => Vec::new(),
    };
    post.insert("count".into(), urls.len().into());
    let id = post.get("id").map(crate::sites::scalar).unwrap_or_default();
    let title = post.get("title").and_then(Value::as_str).map(str::to_owned);
    let user = blog_name.as_str().map(str::to_owned);
    urls.iter()
        .enumerate()
        .filter_map(|(index, raw)| {
            let url = super::file_url(raw, REFERER)?;
            let mut fields = post.clone();
            fields.insert("num".into(), (index + 1).into());
            super::nameext_into(raw, &mut fields);
            Some(
                Item::new(url, fields)
                    .ids(id.clone(), (index + 1).to_string())
                    .user(user.clone())
                    .caption(title.clone())
                    .timestamp(timestamp)
                    .header("Referer", REFERER),
            )
        })
        .collect()
}

fn post_events(
    http: &Http,
    route: &'static Route,
    post: &Value,
) -> Vec<crate::event::ExtractionEvent> {
    crate::sites::post(&SITE, http, route, post_items(post))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http, json, keywords, media, serve};

    fn t(raw: &str) -> Option<Target> {
        target(&Url::parse(raw).unwrap())
    }

    #[test]
    fn classifies_urls() {
        assert_eq!(
            t("https://gengar563.lofter.com/post/1e82da8c_2ba27e056"),
            Some(Target::Post {
                blog: 0x1e82da8c,
                post: 0x2ba27e056
            })
        );
        assert_eq!(
            t("https://gengar563.lofter.com/"),
            Some(Target::Blog("gengar563".into()))
        );
        assert_eq!(
            t("https://www.lofter.com/front/blog/home-page/gengar563"),
            Some(Target::Blog("gengar563".into()))
        );
        for raw in [
            "https://www.lofter.com/",
            "https://lofter.com/",
            "https://gengar563.lofter.com/view",
            "https://gengar563.lofter.com/post/xyz_1",
            "https://gengar563.lofter.com/post/1e82da8c",
            "https://gengar563.lofter.com.evil.com/",
            "https://evillofter.com/",
            "https://a.b.lofter.com/",
            "https://gengar563.lofter.com:8080/",
            "https://u:p@gengar563.lofter.com/",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn photo(id: u64) -> Value {
        json!({"post": {"id": id, "type": 2, "title": "t", "publishTime": 1694962338843u64,
            "blogInfo": {"blogName": "gengar563", "blogId": 1},
            "photoLinks": "[{\"orign\":\"https://imglf5.lf127.net/img/a/B.jpg?imageView\"},{\"orign\":\"https://imglf5.lf127.net/img/a/c.png\"}]"}})
    }

    #[tokio::test]
    async fn extracts_post() {
        let (root, server) = serve(vec![json(
            json!({"meta": {"status": 200}, "response": {"posts": [photo(11713110102)]}}),
        )]);
        let http = http("lofter", root);
        let files = media(extract(
            &http,
            Url::parse("https://gengar563.lofter.com/post/1e82da8c_2ba27e056").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].url.as_str(), "https://imglf5.lf127.net/img/a/B.jpg");
        let k = keywords(&files[0]);
        assert_eq!(k["blog_name"], "gengar563");
        assert_eq!(k["date"], "2023-09-17 14:52:18");
        assert_eq!(k["count"], 2);
        assert_eq!(k["filename"], "B");
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "11713110102_2.png");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("lofter/gengar563")
        );
        let (_, identity) = crate::sites::archive_key(&files[1]).unwrap();
        assert_eq!(identity, r#"["{id}_{num}",["11713110102","2"]]"#);
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with(
                "POST /oldapi/post/detail.api?product=lofter-android-7.9.10&targetblogid=512940684&postid=11713110102 "
            ),
            "{}",
            requests[0]
        );
    }

    #[tokio::test]
    async fn paginates_blog_posts() {
        let article = json!({"post": {"id": 5, "type": 1, "publishTime": 0,
            "blogInfo": {"blogName": "b"},
            "content": "<p><img src=\"https://imglf3.lf127.net/x.gif?a&amp;b\"></p>"}});
        let (root, server) = serve(vec![
            json(json!({"meta": {"status": 200}, "response": {"offset": 2,
                "posts": [photo(1), article]}})),
            json(json!({"meta": {"status": 200}, "response": {"offset": -1,
                "posts": [{"post": {"id": 6, "type": 9, "publishTime": 0, "blogInfo": {}}}]}})),
        ]);
        let http = http("lofter", root);
        let files = media(extract(&http, Url::parse("https://b.lofter.com").unwrap())).await;
        assert_eq!(files.len(), 3);
        assert_eq!(files[2].url.as_str(), "https://imglf3.lf127.net/x.gif");
        assert_eq!(keywords(&files[2])["subcategory"], "blog-posts");
        let requests = server.join().unwrap();
        assert!(requests[0].contains(
            "/v2.0/blogHomePage.api?product=lofter-android-7.9.10&method=getPostLists&offset=0&limit=200&blogdomain=b.lofter.com "
        ));
        assert!(requests[1].contains("&offset=2&"));
    }

    #[tokio::test]
    async fn reports_missing_blog() {
        let (root, _server) = serve(vec![json(json!({"meta": {"status": 4200}}))]);
        let http = http("lofter", root);
        let events: Vec<_> = futures_util::StreamExt::collect(extract(
            &http,
            Url::parse("https://b.lofter.com/").unwrap(),
        ))
        .await;
        let message = events[0].as_ref().err().unwrap().to_string();
        assert!(message.contains("not found") && !message.contains("lofter.com"));
    }
}
