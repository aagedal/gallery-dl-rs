//! s3nd.pics posts, users and searches, mirroring
//! gallery_dl/extractor/s3ndpics.py.
use crate::error;
use crate::sites::gdl::{
    captures, date, group, int_of, nameext_into, parse_iso, parse_query, re, str_of,
};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const API: &str = "https://s3nd.pics/api";
const FILES: &str = "https://s3.s3nd.pics/s3nd-pics/";
const BASE: &str = r"^https?://(?:www\.)?s3nd\.pics";
const FILENAME: &str = "{num:>02}.{extension}";
const DIRECTORY: &[&str] = &[
    "{category}",
    "{user[username]}",
    "{date} {title:?/ /}({id})",
];

static POST: Route = Route::new("post", FILENAME, DIRECTORY, "{id}_{num}");
static USER: Route = Route::new("user", FILENAME, DIRECTORY, "{id}_{num}");
static SEARCH: Route = Route::new("search", FILENAME, DIRECTORY, "{id}_{num}");

pub static SITE: Site = Site {
    category: "s3ndpics",
    name: "s3nd.pics",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&POST, &USER, &SEARCH],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Post(String),
    User(String),
    Search(String),
}

fn target(url: &Url) -> Option<Target> {
    if let Some(c) = captures(re!(&format!(r"{BASE}/post/([0-9a-f]+)")), url) {
        return Some(Target::Post(group(&c, 1).into()));
    }
    if let Some(c) = captures(re!(&format!(r"{BASE}/user/(\w+)")), url) {
        return Some(Target::User(group(&c, 1).into()));
    }
    captures(re!(&format!(r"{BASE}/search/?\?([^#]+)")), url)
        .map(|c| Target::Search(group(&c, 1).into()))
}

/// `S3ndpicsExtractor.items` for one post; `extra` holds job-wide keywords.
fn post_items(post: &Value, extra: &Map<String, Value>) -> Vec<Item> {
    let Some(mut post) = post.as_object().cloned() else {
        return Vec::new();
    };
    let id = post.remove("_id").unwrap_or(Value::Null);
    let user = post.remove("userId").unwrap_or(Value::Null);
    post.insert("id".into(), id);
    post.insert("user".into(), user);
    let created = date(parse_iso(
        post.get("createdAt")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    ));
    let updated = date(parse_iso(
        post.get("updatedAt")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    ));
    post.insert("date".into(), created);
    post.insert("date_updated".into(), updated);
    let files = post
        .remove("files")
        .and_then(|f| f.as_array().cloned())
        .unwrap_or_default();
    post.insert("count".into(), files.len().into());
    post.extend(extra.clone());
    let post_id = post.get("id").map(crate::sites::scalar).unwrap_or_default();
    files
        .iter()
        .enumerate()
        .filter_map(|(index, file)| {
            let mut fields = post.clone();
            fields.insert("num".into(), (index + 1).into());
            fields.insert("type".into(), file["type"].clone());
            let path = str_of(file, "url");
            nameext_into(path, &mut fields);
            Some(
                Item::new(Url::parse(&format!("{FILES}{path}")).ok()?, fields)
                    .ids(post_id.clone(), (index + 1).to_string()),
            )
        })
        .collect()
}

fn pagination<'a>(
    http: &'a Http,
    route: &'static Route,
    params: Vec<(String, String)>,
    extra: Map<String, Value>,
) -> Events<'a> {
    crate::sites::paginate(Some(1i64), move |state| {
        let params = params.clone();
        let mut extra = extra.clone();
        async move {
            let Some(page) = state else { return Ok(None) };
            let mut url = http.api(&format!("{API}/posts"))?;
            url.query_pairs_mut()
                .extend_pairs(params.iter())
                .append_pair("page", &page.to_string());
            let data = http.json(&url).await?;
            extra.insert("total".into(), data["pagination"]["total"].clone());
            let next = (page < int_of(data["pagination"].get("pages"))).then_some(page + 1);
            let events = data["posts"]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|post| crate::sites::post(&SITE, http, route, post_items(post, &extra)))
                .collect();
            Ok(Some((crate::sites::events(events), next)))
        }
    })
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported s3nd.pics URL"));
    };
    match target {
        Target::Post(id) => crate::sites::once(async move {
            let data = http.json(&http.api(&format!("{API}/posts/{id}"))?).await?;
            Ok(crate::sites::events(crate::sites::post(
                &SITE,
                http,
                &POST,
                post_items(&data["post"], &Map::new()),
            )))
        }),
        Target::User(name) => crate::sites::once(async move {
            let user = http
                .json(&http.api(&format!("{API}/users/username/{name}"))?)
                .await?["user"]
                .clone();
            let params = vec![
                ("userId".into(), crate::sites::scalar(&user["_id"])),
                ("limit".into(), "12".into()),
                ("sortBy".into(), "newest".into()),
            ];
            let mut extra = Map::new();
            extra.insert("user".into(), user);
            Ok(pagination(http, &USER, params, extra))
        }),
        Target::Search(query) => {
            let mut query = parse_query(&query);
            query.entry("limit").or_insert("20".into());
            let tags = ["tag", "tags", "q"]
                .iter()
                .find_map(|k| {
                    query
                        .get(*k)
                        .filter(|v| crate::sites::gdl::truthy(Some(v)))
                        .cloned()
                })
                .unwrap_or(Value::Null);
            let mut extra = Map::new();
            extra.insert("search_tags".into(), tags);
            let params = query
                .iter()
                .map(|(k, v)| (k.clone(), crate::sites::scalar(v)))
                .collect();
            pagination(http, &SEARCH, params, extra)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http as client, json, keywords, media, serve};
    use serde_json::json;

    #[tokio::test]
    async fn search_pages_posts() {
        let post = |id: &str| {
            json!({"_id": id, "userId": {"username": "jane"}, "title": "Hi", "createdAt": "2025-01-02T03:04:05.000Z", "updatedAt": "2025-01-02T03:04:05.000Z",
            "files": [{"type": "image", "url": format!("{id}/a.jpg")}, {"type": "video", "url": format!("{id}/b.mp4")}]})
        };
        let (root, server) = serve(vec![
            json(json!({"posts": [post("aa")], "pagination": {"total": 2, "pages": 2}})),
            json(json!({"posts": [post("bb")], "pagination": {"total": 2, "pages": 2}})),
        ]);
        let http = client("s3ndpics", root);
        let files = media(extract(
            &http,
            Url::parse("https://s3nd.pics/search?tag=cat").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 4);
        assert_eq!(
            files[1].url.as_str(),
            "https://s3.s3nd.pics/s3nd-pics/aa/b.mp4"
        );
        let k = keywords(&files[1]);
        assert_eq!(
            (
                k["search_tags"].as_str(),
                k["total"].as_i64(),
                k["count"].as_i64()
            ),
            (Some("cat"), Some(2), Some(2))
        );
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "02.mp4");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            crate::sites::testing::os_dir("s3ndpics/jane/2025-01-02 03:04:05 Hi (aa)")
        );
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /api/posts?tag=cat&limit=20&page=2 "));
    }
}
