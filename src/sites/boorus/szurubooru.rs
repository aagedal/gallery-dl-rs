//! szurubooru instances (bcbnsfw, snootbooru, visuabusters), mirroring
//! gallery_dl/extractor/szurubooru.py through the `/api` JSON endpoints.
use crate::error;
use crate::sites::boorus::{booru_fields, date_value, iso_timestamp, leading_digits, unquote_plus};
use crate::sites::{Events, Http, Item, Route, Site};
use base64::Engine;
use serde_json::{Map, Value};
use url::Url;

const PER_PAGE: usize = 100;
const FILENAME: &str = "{id}_{version}_{checksumMD5}.{extension}";
static TAG: Route = Route::new(
    "tag",
    FILENAME,
    &["{category}", "{search_tags}"],
    "t_{search_tags}_{id}_{version}",
);
static POST: Route = Route::new("post", FILENAME, &["{category}"], "{id}_{version}");

macro_rules! instance {
    ($ident:ident, $category:literal, $name:literal) => {
        pub static $ident: Site = Site {
            category: $category,
            name: $name,
            matches: |url| target($category, url).is_some(),
            extract: |http, url| extract($category, http, url),
            routes: &[&TAG, &POST],
            api_key: false,
        };
    };
}
instance!(BCBNSFW, "bcbnsfw", "bcbnsfw");
instance!(SNOOTBOORU, "snootbooru", "Snootbooru");
instance!(VISUABUSTERS, "visuabusters", "Visuabusters");

fn site(category: &str) -> &'static Site {
    match category {
        "bcbnsfw" => &BCBNSFW,
        "snootbooru" => &SNOOTBOORU,
        _ => &VISUABUSTERS,
    }
}

fn root(category: &str) -> &'static str {
    match category {
        "bcbnsfw" => "https://booru.bcbnsfw.space",
        "snootbooru" => "https://snootbooru.com",
        _ => "https://www.visuabusters.com/booru",
    }
}

/// Upstream `pattern`s as `host[/prefix]` roots.
fn roots(category: &str) -> &'static [&'static str] {
    match category {
        "bcbnsfw" => &["booru.bcbnsfw.space"],
        "snootbooru" => &["snootbooru.com"],
        _ => &["visuabusters.com/booru", "www.visuabusters.com/booru"],
    }
}

/// Instance `query-all` setting.
fn query_all(category: &str) -> Option<&'static str> {
    (category == "bcbnsfw").then_some("*")
}

#[derive(Debug, PartialEq)]
enum Target {
    /// Raw (still quoted) `query=` path component, if any.
    Tag(Option<String>),
    Post(String),
}

fn target(category: &str, url: &Url) -> Option<Target> {
    let rest = crate::sites::boorus::below(url, roots(category))?;
    if let Some(tail) = rest.strip_prefix("/posts") {
        let query = tail
            .strip_prefix("/query=")
            .map(|q| crate::sites::boorus::until(q, &['/', '?', '#']).to_owned());
        return Some(Target::Tag(query));
    }
    let id = leading_digits(rest.strip_prefix("/post/")?)?;
    Some(Target::Post(id.to_owned()))
}

fn headers(http: &Http) -> Vec<(&'static str, String)> {
    let mut headers = vec![
        ("Accept", "application/json".to_owned()),
        ("Content-Type", "application/json".to_owned()),
    ];
    if let Some(username) = http.option_str("username").filter(|u| !u.is_empty()) {
        let token = http
            .option("token")
            .map(crate::sites::scalar)
            .unwrap_or_else(|| "None".into());
        let encoded =
            base64::engine::general_purpose::STANDARD.encode(format!("{username}:{token}"));
        headers.push(("Authorization", format!("Token {encoded}")));
    }
    headers
}

async fn api(http: &Http, url: &Url) -> crate::Result<Value> {
    let headers = headers(http);
    let refs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    http.json_with(url, &refs).await
}

fn extract<'a>(category: &'static str, http: &'a Http, url: Url) -> Events<'a> {
    let Some(target) = target(category, &url) else {
        return crate::sites::fail(error("unsupported szurubooru URL"));
    };
    let base = root(category);
    match target {
        Target::Tag(query) => {
            let query = query.map(|q| unquote_plus(&q)).unwrap_or_default();
            let mut data = Map::new();
            data.insert("search_tags".into(), query.clone().into());
            let search = if query.trim().is_empty() {
                query_all(category).map(str::to_owned)
            } else {
                Some(query)
            };
            crate::sites::paginate(Some(0usize), move |offset| {
                let search = search.clone();
                let data = data.clone();
                async move {
                    let Some(offset) = offset else {
                        return Ok(None);
                    };
                    let mut url = http.api(&format!("{base}/api/posts/"))?;
                    {
                        let mut pairs = url.query_pairs_mut();
                        if let Some(search) = &search {
                            pairs.append_pair("query", search);
                        }
                        pairs
                            .append_pair("offset", &offset.to_string())
                            .append_pair("limit", &PER_PAGE.to_string());
                    }
                    let body = api(http, &url).await?;
                    let results = body
                        .get("results")
                        .and_then(Value::as_array)
                        .ok_or_else(|| error("szurubooru returned no results"))?;
                    let count = results.len();
                    let events = posts_events(category, http, &TAG, results, &data);
                    let next = (count >= PER_PAGE).then_some(offset + count);
                    Ok(Some((crate::sites::events(events), next)))
                }
            })
        }
        Target::Post(id) => crate::sites::once(async move {
            let url = http.api(&format!("{base}/api/post/{id}"))?;
            let post = api(http, &url).await?;
            Ok(crate::sites::events(posts_events(
                category,
                http,
                &POST,
                std::slice::from_ref(&post),
                &Map::new(),
            )))
        }),
    }
}

fn posts_events(
    category: &str,
    http: &Http,
    route: &'static Route,
    posts: &[Value],
    data: &Map<String, Value>,
) -> Vec<crate::event::ExtractionEvent> {
    let items = posts
        .iter()
        .filter_map(|post| parse_post(category, post, data))
        .collect();
    crate::sites::boorus::per_post(site(category), http, route, items)
}

/// `BooruExtractor.items` for one API post, with `SzurubooruExtractor._prepare`.
fn parse_post(category: &str, post: &Value, data: &Map<String, Value>) -> Option<Item> {
    let mut fields = post.as_object()?.clone();
    let raw = fields.get("contentUrl")?.as_str()?;
    let raw = if raw.starts_with("http") {
        raw.to_owned()
    } else {
        format!("{}/{raw}", root(category))
    };
    let url = Url::parse(&raw).ok().filter(crate::sites::plain)?;
    booru_fields(&url, &mut fields, data);

    let timestamp = fields
        .get("creationTime")
        .and_then(Value::as_str)
        .and_then(iso_timestamp);
    fields.insert("date".into(), date_value(timestamp));

    let mut tags = Vec::new();
    let mut categories: Vec<(String, Vec<Value>)> = Vec::new();
    for tag in fields
        .get("tags")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let kind = tag
            .get("category")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let kind = crate::sites::boorus::rpartition(kind, "_").1.to_owned();
        let Some(name) = tag.get("names").and_then(|n| n.get(0)).cloned() else {
            continue;
        };
        match categories.iter_mut().find(|(k, _)| *k == kind) {
            Some((_, list)) => list.push(name.clone()),
            None => categories.push((kind, vec![name.clone()])),
        }
        tags.push(name);
    }
    let caption = tags
        .iter()
        .map(crate::sites::scalar)
        .collect::<Vec<_>>()
        .join(" ");
    fields.insert("tags".into(), Value::Array(tags));
    for (kind, list) in categories {
        fields.insert(format!("tags_{kind}"), Value::Array(list));
    }

    let id = fields
        .get("id")
        .map(crate::sites::scalar)
        .unwrap_or_default();
    let user = fields
        .get("user")
        .and_then(|u| u.get("name"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    Some(
        Item::new(url, fields)
            .ids(id.clone(), id)
            .user(user)
            .caption(Some(caption))
            .timestamp(timestamp),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http, json, keywords, media, serve};
    use serde_json::json;

    fn t(category: &str, raw: &str) -> Option<Target> {
        target(category, &Url::parse(raw).unwrap())
    }

    #[test]
    fn classifies_urls() {
        assert_eq!(
            t("bcbnsfw", "https://booru.bcbnsfw.space/posts/query=a%20b+c"),
            Some(Target::Tag(Some("a%20b+c".into())))
        );
        assert_eq!(
            t("bcbnsfw", "https://booru.bcbnsfw.space/posts"),
            Some(Target::Tag(None))
        );
        assert_eq!(
            t("snootbooru", "https://snootbooru.com/post/14511/extra"),
            Some(Target::Post("14511".into()))
        );
        assert_eq!(
            t("visuabusters", "https://www.visuabusters.com/booru/post/7"),
            Some(Target::Post("7".into()))
        );
        assert_eq!(
            t(
                "visuabusters",
                "https://visuabusters.com/booru/posts/query=x/y"
            ),
            Some(Target::Tag(Some("x".into())))
        );
        assert_eq!(t("visuabusters", "https://visuabusters.com/post/7"), None);
        assert_eq!(t("snootbooru", "https://www.snootbooru.com/post/1"), None);
        assert_eq!(
            t("snootbooru", "https://snootbooru.com.evil.test/post/1"),
            None
        );
        assert_eq!(t("snootbooru", "https://snootbooru.com:8080/post/1"), None);
        assert_eq!(t("snootbooru", "https://snootbooru.com/post/x"), None);
        assert_eq!(t("snootbooru", "https://snootbooru.com/users"), None);
        assert!(
            crate::sites::find(&Url::parse("https://snootbooru.com/post/1").unwrap())
                .is_some_and(|site| site.category == "snootbooru")
        );
    }

    fn post(id: u64) -> Value {
        json!({
            "id": id, "version": 3, "checksumMD5": format!("md5{id}"),
            "creationTime": "2017-07-14T02:40:00.123Z",
            "contentUrl": format!("data/posts/{id}_abc.png"),
            "user": {"name": "uploader"},
            "tags": [
                {"names": ["alice", "a"], "category": "character"},
                {"names": ["sky"], "category": "default"},
                {"names": ["bob"], "category": "x_character"},
            ],
        })
    }

    #[tokio::test]
    async fn paginates_tag_search() {
        let first: Vec<_> = (1..=100).map(post).collect();
        let (root, server) = serve(vec![
            json(json!({"results": first})),
            json(json!({"results": [post(101)]})),
        ]);
        let http = http("visuabusters", root);
        let files = media(extract(
            "visuabusters",
            &http,
            Url::parse("https://visuabusters.com/booru/posts/query=sky+blue").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 101);
        assert_eq!(
            files[0].url.as_str(),
            "https://www.visuabusters.com/booru/data/posts/1_abc.png"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["search_tags"], "sky blue");
        assert_eq!(k["tags"], json!(["alice", "sky", "bob"]));
        assert_eq!(k["tags_character"], json!(["alice", "bob"]));
        assert_eq!(k["tags_default"], json!(["sky"]));
        assert_eq!(k["date"], "2017-07-14 02:40:00");
        assert_eq!(k["filename"], "1_abc");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "1_3_md51.png");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("visuabusters/sky blue")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(
            identity,
            r#"["t_{search_tags}_{id}_{version}",["sky blue","1","3"]]"#
        );
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with("GET /booru/api/posts/?query=sky+blue&offset=0&limit=100 "),
            "{}",
            requests[0]
        );
        assert!(
            requests[0]
                .to_ascii_lowercase()
                .contains("accept: application/json")
        );
        assert!(
            requests[1].starts_with("GET /booru/api/posts/?query=sky+blue&offset=100&limit=100 ")
        );
    }

    #[tokio::test]
    async fn empty_query_uses_query_all() {
        let (root, server) = serve(vec![json(json!({"results": [post(1)]}))]);
        let http = http("bcbnsfw", root);
        let files = media(extract(
            "bcbnsfw",
            &http,
            Url::parse("https://booru.bcbnsfw.space/posts").unwrap(),
        ))
        .await;
        assert_eq!(keywords(&files[0])["search_tags"], "");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/posts/?query=*&offset=0&limit=100 "));

        let (root, server) = serve(vec![json(json!({"results": []}))]);
        let http = crate::sites::testing::http("snootbooru", root);
        let files = media(extract(
            "snootbooru",
            &http,
            Url::parse("https://snootbooru.com/posts/query=").unwrap(),
        ))
        .await;
        assert!(files.is_empty());
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/posts/?offset=0&limit=100 "));
    }

    #[tokio::test]
    async fn single_post() {
        let mut p = post(9);
        p["contentUrl"] = "https://cdn.snootbooru.com/9.webm".into();
        let (root, server) = serve(vec![json(p)]);
        let http = http("snootbooru", root);
        let files = media(extract(
            "snootbooru",
            &http,
            Url::parse("https://snootbooru.com/post/9").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].url.as_str(), "https://cdn.snootbooru.com/9.webm");
        let meta = files[0].metadata.as_ref().unwrap();
        assert_eq!(meta.username.as_deref(), Some("uploader"));
        assert_eq!(meta.timestamp, Some(1_500_000_000));
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "9_3_md59.webm");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("snootbooru")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{id}_{version}",["9","3"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/post/9 "));
    }
}
