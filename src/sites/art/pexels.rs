//! Pexels images, collections, searches and users, mirroring
//! gallery_dl/extractor/pexels.py.
use crate::error;
use crate::sites::gdl::{
    captures, date, extr, group, int_of, nameext_into, nextdata, parse_iso, partition, re, str_of,
};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use std::time::Duration;
use url::Url;

const ROOT: &str = "https://www.pexels.com";
const API: &str = "https://www.pexels.com/en-us/api";
const BASE: &str = r"^https?://(?:www\.)?pexels\.com";

static COLLECTION: Route = Route::new(
    "collection",
    "{filename}.{extension}",
    &["{category}", "Collections", "{collection}"],
    "{id}",
);
static SEARCH: Route = Route::new(
    "search",
    "{filename}.{extension}",
    &["{category}", "Searches", "{search_tags}"],
    "{id}",
);
static USER: Route = Route::new(
    "user",
    "{filename}.{extension}",
    &["{category}", "@{user[slug]}"],
    "{id}",
);
static IMAGE: Route = Route::new("image", "{filename}.{extension}", &["{category}"], "{id}");

pub static SITE: Site = Site {
    category: "pexels",
    name: "Pexels",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&COLLECTION, &SEARCH, &USER, &IMAGE],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Collection(String, String),
    Search(String),
    User(String),
    Image(String),
}

fn target(url: &Url) -> Option<Target> {
    if let Some(c) = captures(
        re!(&format!(r"{BASE}/collections/((?:[^/?#]*-)?(\w+))")),
        url,
    ) {
        return Some(Target::Collection(group(&c, 1).into(), group(&c, 2).into()));
    }
    if let Some(c) = captures(re!(&format!(r"{BASE}/search/([^/?#]+)")), url) {
        return Some(Target::Search(group(&c, 1).into()));
    }
    if let Some(c) = captures(
        re!(&format!(r"{BASE}/(@(?:(?:[^/?#]*-)?(\d+)|[^/?#]+))")),
        url,
    ) {
        return Some(Target::User(
            c.get(2).map_or_else(|| group(&c, 1), |m| m.as_str()).into(),
        ));
    }
    captures(re!(&format!(r"{BASE}/photo/((?:[^/?#]*-)?\d+)")), url)
        .map(|c| Target::Image(group(&c, 1).into()))
}

/// `PexelsExtractor.items` for one post.
fn item(post: &Value, metadata: &Map<String, Value>) -> Option<Item> {
    let mut post = match post.get("attributes") {
        Some(attributes) => {
            let mut fields = attributes.as_object()?.clone();
            fields.insert("type".into(), post["type"].clone());
            fields
        }
        None => post.as_object()?.clone(),
    };
    post.extend(metadata.clone());
    let created = str_of(&Value::Object(post.clone()), "created_at").to_owned();
    post.insert(
        "date".into(),
        date(parse_iso(
            created
                .get(..created.len().saturating_sub(5))
                .unwrap_or_default(),
        )),
    );
    let (url, name) = if let Some(image) = post.get("image") {
        let (url, _, query) = partition(str_of(image, "download_link"), "?");
        (url.to_owned(), extr(query, "&dl=", "&").to_owned())
    } else {
        let video = post.get("video")?;
        (
            str_of(video, "download_link").to_owned(),
            str_of(video, "src").to_owned(),
        )
    };
    nameext_into(&name, &mut post);
    let id = post.get("id").map(crate::sites::scalar).unwrap_or_default();
    Some(Item::new(Url::parse(&url).ok()?, post).ids(id.clone(), id))
}

/// `PexelsAPI._pagination`, waiting out rate limits like upstream.
fn pagination<'a>(
    http: &'a Http,
    route: &'static Route,
    endpoint: String,
    params: Vec<(&'static str, String)>,
    metadata: Map<String, Value>,
) -> Events<'a> {
    crate::sites::paginate(Some(1i64), move |state| {
        let endpoint = endpoint.clone();
        let params = params.clone();
        let metadata = metadata.clone();
        async move {
            let Some(page) = state else { return Ok(None) };
            if page > 1 && !cfg!(test) {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            let mut url = http.api(&format!("{API}{endpoint}"))?;
            url.query_pairs_mut()
                .extend_pairs(params.iter().map(|(k, v)| (*k, v.as_str())))
                .append_pair("page", &page.to_string());
            let headers = [
                ("Accept", "*/*"),
                ("Content-Type", "application/json"),
                ("secret-key", "H2jk9uKnhRmL6WPwh89zBezWvr"),
                ("Authorization", ""),
                ("X-Forwarded-CF-Connecting-IP", ""),
                ("X-Forwarded-HTTP_CF_IPCOUNTRY", ""),
                ("X-Forwarded-CF-IPRegionCode", ""),
                ("X-Client-Type", "react"),
                ("Sec-Fetch-Dest", "empty"),
                ("Sec-Fetch-Mode", "cors"),
                ("Sec-Fetch-Site", "same-origin"),
                ("Priority", "u=4"),
            ];
            let data = http
                .json_with(&url, &headers)
                .await
                .map_err(|e| error(format!("API request failed ({e})")))?;
            let current = int_of(data["pagination"].get("current_page"));
            let next =
                (current < int_of(data["pagination"].get("total_pages"))).then_some(current + 1);
            let events = data["data"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|post| item(post, &metadata))
                .flat_map(|item| crate::sites::post(&SITE, http, route, vec![item]))
                .collect();
            Ok(Some((crate::sites::events(events), next)))
        }
    })
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Pexels URL"));
    };
    let per_page = || ("per_page", "24".to_owned());
    match target {
        Target::Collection(name, id) => {
            let mut metadata = Map::new();
            metadata.insert("collection".into(), name.into());
            metadata.insert("collection_id".into(), id.as_str().into());
            pagination(
                http,
                &COLLECTION,
                format!("/v3/collections/{id}/media"),
                vec![per_page()],
                metadata,
            )
        }
        Target::Search(query) => {
            let mut metadata = Map::new();
            metadata.insert("search_tags".into(), query.as_str().into());
            let params = vec![
                ("query", query),
                per_page(),
                ("orientation", "all".into()),
                ("size", "all".into()),
                ("color", "all".into()),
                ("sort", "popular".into()),
            ];
            pagination(http, &SEARCH, "/v3/search/photos".into(), params, metadata)
        }
        Target::User(user) => pagination(
            http,
            &USER,
            format!("/v3/users/{user}/media/recent"),
            vec![per_page()],
            Map::new(),
        ),
        Target::Image(slug) => crate::sites::once(async move {
            let page = http
                .text(&http.api(&format!("{ROOT}/photo/{slug}/"))?)
                .await?;
            let data = nextdata(&page)?;
            let items = item(&data["props"]["pageProps"]["medium"], &Map::new())
                .into_iter()
                .collect();
            Ok(crate::sites::events(crate::sites::post(
                &SITE, http, &IMAGE, items,
            )))
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http as client, json, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.pexels.com/collections/cherry-blossoms-abc123/"),
            Some(Target::Collection(
                "cherry-blossoms-abc123".into(),
                "abc123".into()
            ))
        );
        assert_eq!(
            t("https://www.pexels.com/@jane-doe-12345/"),
            Some(Target::User("12345".into()))
        );
        assert_eq!(
            t("https://www.pexels.com/@janedoe/"),
            Some(Target::User("@janedoe".into()))
        );
        assert_eq!(
            t("https://www.pexels.com/photo/slug-678/"),
            Some(Target::Image("slug-678".into()))
        );
    }

    #[tokio::test]
    async fn search_reads_attributes() {
        let post = json!({"type": "photo", "attributes": {"id": 5, "created_at": "2020-01-02T03:04:05.000Z",
            "image": {"download_link": "https://images.pexels.com/photos/5/a.jpeg?cs=srgb&dl=nice-photo-5.jpg&fm=jpg"}, "user": {"slug": "jane"}}});
        let (root, server) = serve(vec![json(
            json!({"data": [post], "pagination": {"current_page": 1, "total_pages": 1}}),
        )]);
        let http = client("pexels", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.pexels.com/search/sky/").unwrap(),
        ))
        .await;
        assert_eq!(
            files[0].url.as_str(),
            "https://images.pexels.com/photos/5/a.jpeg"
        );
        let k = keywords(&files[0]);
        assert_eq!(
            (
                k["filename"].as_str(),
                k["type"].as_str(),
                k["date"].as_str()
            ),
            (
                Some("nice-photo-5"),
                Some("photo"),
                Some("2020-01-02 03:04:05")
            )
        );
        let (_, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("pexels/Searches/sky")
        );
        let requests = server.join().unwrap();
        assert!(requests[0].contains("secret-key: H2jk9uKnhRmL6WPwh89zBezWvr"));
    }
}
