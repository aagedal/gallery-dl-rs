//! Tungsten posts, models and users, mirroring gallery_dl/extractor/tungsten.py.
use crate::error;
use crate::sites::gdl::{
    captures, date, extr, group, nextdata, parse_iso, parse_query, re, str_of,
};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://tungsten.run";
const BASE: &str = r"^https?://(?:www\.)?tungsten\.run";
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
    User(String, String),
}

fn target(url: &Url) -> Option<Target> {
    if let Some(c) = captures(re!(&format!(r"{BASE}/post/(\w+)")), url) {
        return Some(Target::Post(group(&c, 1).into()));
    }
    if let Some(c) = captures(
        re!(&format!(r"{BASE}/model/(\w+)(?:/?\?model_version=(\w+))?")),
        url,
    ) {
        return Some(Target::Model(
            group(&c, 1).into(),
            c.get(2).map(|m| m.as_str().to_owned()),
        ));
    }
    captures(
        re!(&format!(
            r"{BASE}/user/([^/?#]+)(?:/posts)?/?(?:\?([^#]+))?"
        )),
        url,
    )
    .map(|c| Target::User(group(&c, 1).into(), group(&c, 2).into()))
}

/// `TungstenExtractor.items` for one post.
fn item(post: &Value, extra: &Map<String, Value>) -> Option<Item> {
    let mut fields = post.as_object()?.clone();
    let url = str_of(post, "original_url").to_owned();
    fields.insert("date".into(), date(parse_iso(str_of(post, "created_at"))));
    fields.insert(
        "filename".into(),
        url[url.rfind('/').map_or(0, |i| i + 1)..].into(),
    );
    fields.insert("extension".into(), "webp".into());
    fields.extend(extra.clone());
    let uuid = fields
        .get("uuid")
        .map(crate::sites::scalar)
        .unwrap_or_default();
    Some(Item::new(Url::parse(&url).ok()?, fields).ids(uuid.clone(), uuid))
}

fn pagination<'a>(
    http: &'a Http,
    route: &'static Route,
    url: String,
    params: Vec<(String, String)>,
    extra: Map<String, Value>,
) -> Events<'a> {
    crate::sites::paginate(Some(1u64), move |state| {
        let url = url.clone();
        let params = params.clone();
        let extra = extra.clone();
        async move {
            let Some(page) = state else { return Ok(None) };
            let mut request = http.api(&url)?;
            request
                .query_pairs_mut()
                .extend_pairs(params.iter())
                .append_pair("page", &page.to_string())
                .append_pair("per_page", "40");
            let headers = [
                ("Origin", ROOT),
                ("Sec-Fetch-Dest", "empty"),
                ("Sec-Fetch-Mode", "cors"),
                ("Sec-Fetch-Site", "same-site"),
            ];
            let data = http.json_with(&request, &headers).await?;
            let posts = data.as_array().cloned().unwrap_or_default();
            let next = (posts.len() >= 40).then_some(page + 1);
            let events = posts
                .iter()
                .filter_map(|post| item(post, &extra))
                .flat_map(|item| crate::sites::post(&SITE, http, route, vec![item]))
                .collect();
            Ok(Some((crate::sites::events(events), next)))
        }
    })
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Tungsten URL"));
    };
    match target {
        Target::Post(id) => crate::sites::once(async move {
            let page = http.text(&http.api(&format!("{ROOT}/post/{id}"))?).await?;
            let data = nextdata(&page)?;
            let items = item(&data["props"]["pageProps"]["post"], &Map::new())
                .into_iter()
                .collect();
            Ok(crate::sites::events(crate::sites::post(
                &SITE, http, &POST, items,
            )))
        }),
        Target::Model(model, version) => crate::sites::once(async move {
            let version = match version {
                Some(version) => version,
                None => {
                    let page = http
                        .text(&http.api(&format!("{ROOT}/model/{model}/"))?)
                        .await?;
                    extr(&page, "\"modelVersionUUID\":\"", "\"").to_owned()
                }
            };
            let params = [
                ("sort", "top_all_time"),
                ("tweakable_only", "false"),
                ("following", "false"),
                ("model_version_uuid", version.as_str()),
            ]
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .to_vec();
            Ok(pagination(
                http,
                &MODEL,
                "https://api.tungsten.run/v1/posts".into(),
                params,
                Map::new(),
            ))
        }),
        Target::User(user, query) => crate::sites::once(async move {
            let page = http
                .text(&http.api(&format!("{ROOT}/user/{user}"))?)
                .await?;
            let uuid = extr(&page, "\"user\":{\"uuid\":\"", "\"").to_owned();
            let mut query = parse_query(&query);
            query.entry("sort").or_insert("top_all_time".into());
            let mut extra = Map::new();
            extra.insert(
                "search_tags".into(),
                query.get("tag").cloned().unwrap_or_else(|| "".into()),
            );
            let params = query
                .iter()
                .map(|(k, v)| (k.clone(), crate::sites::scalar(v)))
                .collect();
            Ok(pagination(
                http,
                &USER,
                format!("https://api.tungsten.run/v1/users/{uuid}/posts"),
                params,
                extra,
            ))
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http as client, json, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://tungsten.run/post/AbCd"),
            Some(Target::Post("AbCd".into()))
        );
        assert_eq!(
            t("https://tungsten.run/model/AbCd?model_version=Ef"),
            Some(Target::Model("AbCd".into(), Some("Ef".into())))
        );
        assert_eq!(
            t("https://tungsten.run/user/jane/posts?tag=cat"),
            Some(Target::User("jane".into(), "tag=cat".into()))
        );
    }

    #[tokio::test]
    async fn user_posts_page_until_short() {
        let post = json!({"uuid": "u1", "title": "Cat", "created_at": "2024-01-02T03:04:05Z", "original_url": "https://cdn.tungsten.run/o/abc", "user": {"username": "jane"}});
        let (root, server) = serve(vec![html(r#"{"user":{"uuid":"U9"}}"#), json(json!([post]))]);
        let http = client("tungsten", root);
        let files = media(extract(
            &http,
            Url::parse("https://tungsten.run/user/jane?tag=cat").unwrap(),
        ))
        .await;
        let k = keywords(&files[0]);
        assert_eq!(k["search_tags"], "cat");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            file.render(&files[0]).unwrap(),
            "2024-01-02 03:04:05 Cat u1.webp"
        );
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("tungsten/jane")
        );
        let requests = server.join().unwrap();
        assert!(
            requests[1].starts_with(
                "GET /v1/users/U9/posts?tag=cat&sort=top_all_time&page=1&per_page=40 "
            )
        );
    }
}
