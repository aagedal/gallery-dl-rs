//! Unsplash photos, users, likes, collections and searches, mirroring
//! gallery_dl/extractor/unsplash.py.
use crate::error;
use crate::sites::gdl::{captures, date, group, nameext_into, parse_iso, parse_query, re, unquote};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://unsplash.com";
const BASE: &str = r"^https?://unsplash\.com";
const PER_PAGE: usize = 20;
const DIRECTORY: &[&str] = &["{category}", "{user[username]}"];

static IMAGE: Route = Route::new("image", "{id}.{extension}", DIRECTORY, "{id}");
static USER: Route = Route::new("user", "{id}.{extension}", DIRECTORY, "{id}");
static FAVORITE: Route = Route::new("favorite", "{id}.{extension}", DIRECTORY, "{id}");
static COLLECTION: Route = Route::new("collection", "{id}.{extension}", DIRECTORY, "{id}");
static SEARCH: Route = Route::new("search", "{id}.{extension}", DIRECTORY, "{id}");

pub static SITE: Site = Site {
    category: "unsplash",
    name: "Unsplash",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&IMAGE, &USER, &FAVORITE, &COLLECTION, &SEARCH],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Image(String),
    User(String),
    Favorite(String),
    Collection(String, String),
    Search(String, String),
}

fn target(url: &Url) -> Option<Target> {
    if let Some(c) = captures(re!(&format!(r"{BASE}/photos/([^/?#]+)")), url) {
        return Some(Target::Image(group(&c, 1).into()));
    }
    if let Some(c) = captures(re!(&format!(r"{BASE}/@(\w+)/?$")), url) {
        return Some(Target::User(group(&c, 1).into()));
    }
    if let Some(c) = captures(re!(&format!(r"{BASE}/@(\w+)/likes")), url) {
        return Some(Target::Favorite(group(&c, 1).into()));
    }
    if let Some(c) = captures(
        re!(&format!(r"{BASE}/collections/([^/?#]+)(?:/([^/?#]+))?")),
        url,
    ) {
        return Some(Target::Collection(group(&c, 1).into(), group(&c, 2).into()));
    }
    captures(
        re!(&format!(r"{BASE}/s/photos/([^/?#]+)(?:\?([^#]+))?")),
        url,
    )
    .map(|c| Target::Search(group(&c, 1).into(), group(&c, 2).into()))
}

/// `UnsplashExtractor.items` for one photo.
fn item(http: &Http, photo: &Value, metadata: &Map<String, Value>) -> Option<Item> {
    let format = http
        .option("format")
        .and_then(Value::as_str)
        .unwrap_or("raw");
    let mut photo = photo.as_object()?.clone();
    photo.remove("current_user_collections");
    photo.remove("related_collections");
    let url = photo.get("urls")?.get(format)?.as_str()?.to_owned();
    nameext_into(&url, &mut photo);
    photo.extend(metadata.clone());
    photo.insert("extension".into(), "jpg".into());
    let created = date(parse_iso(
        photo
            .get("created_at")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    ));
    photo.insert("date".into(), created);
    if let Some(tags) = photo.get("tags").and_then(Value::as_array) {
        let titles: Vec<Value> = tags.iter().map(|t| t["title"].clone()).collect();
        photo.insert("tags".into(), titles.into());
    }
    let id = photo
        .get("id")
        .map(crate::sites::scalar)
        .unwrap_or_default();
    Some(Item::new(Url::parse(&url).ok()?, photo).ids(id.clone(), id))
}

fn pagination<'a>(
    http: &'a Http,
    route: &'static Route,
    url: String,
    params: Vec<(String, String)>,
    results: bool,
    metadata: Map<String, Value>,
) -> Events<'a> {
    crate::sites::paginate(Some(1u64), move |state| {
        let url = url.clone();
        let params = params.clone();
        let metadata = metadata.clone();
        async move {
            let Some(page) = state else { return Ok(None) };
            let mut request = http.api(&url)?;
            request
                .query_pairs_mut()
                .extend_pairs(params.iter())
                .append_pair("per_page", &PER_PAGE.to_string())
                .append_pair("page", &page.to_string());
            let data = http.json(&request).await?;
            let photos = if results {
                data["results"].clone()
            } else {
                data
            };
            let photos = photos.as_array().cloned().unwrap_or_default();
            let next = (photos.len() >= PER_PAGE).then_some(page + 1);
            let events = photos
                .iter()
                .filter_map(|photo| item(http, photo, &metadata))
                .flat_map(|item| crate::sites::post(&SITE, http, route, vec![item]))
                .collect();
            Ok(Some((crate::sites::events(events), next)))
        }
    })
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Unsplash URL"));
    };
    let latest = || vec![("order_by".to_owned(), "latest".to_owned())];
    match target {
        Target::Image(id) => crate::sites::once(async move {
            let photo = http
                .json(&http.api(&format!("{ROOT}/napi/photos/{id}"))?)
                .await?;
            let items = item(http, &photo, &Map::new()).into_iter().collect();
            Ok(crate::sites::events(crate::sites::post(
                &SITE, http, &IMAGE, items,
            )))
        }),
        Target::User(user) => pagination(
            http,
            &USER,
            format!("{ROOT}/napi/users/{user}/photos"),
            latest(),
            false,
            Map::new(),
        ),
        Target::Favorite(user) => pagination(
            http,
            &FAVORITE,
            format!("{ROOT}/napi/users/{user}/likes"),
            latest(),
            false,
            Map::new(),
        ),
        Target::Collection(id, title) => {
            let mut metadata = Map::new();
            metadata.insert("collection_id".into(), id.as_str().into());
            metadata.insert("collection_title".into(), title.into());
            pagination(
                http,
                &COLLECTION,
                format!("{ROOT}/napi/collections/{id}/photos"),
                latest(),
                false,
                metadata,
            )
        }
        Target::Search(term, query) => {
            let mut params = vec![("query".to_owned(), unquote(&term.replace('-', " ")))];
            for (key, value) in parse_query(&query) {
                params.retain(|(k, _)| *k != key);
                params.push((key, crate::sites::scalar(&value)));
            }
            pagination(
                http,
                &SEARCH,
                format!("{ROOT}/napi/search/photos"),
                params,
                true,
                Map::new(),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http as client, json, keywords, media, serve};
    use serde_json::json;

    #[tokio::test]
    async fn search_reads_results() {
        let photo = json!({"id": "abc", "created_at": "2020-01-02T03:04:05Z", "urls": {"raw": "https://images.unsplash.com/photo-1?ixid=x"}, "tags": [{"title": "sky"}],
            "user": {"username": "jane"}, "related_collections": {}});
        let (root, server) = serve(vec![json(json!({"results": [photo]}))]);
        let http = client("unsplash", root);
        let files = media(extract(
            &http,
            Url::parse("https://unsplash.com/s/photos/blue-sky?orientation=landscape").unwrap(),
        ))
        .await;
        let k = keywords(&files[0]);
        assert_eq!(k["tags"], json!(["sky"]));
        assert!(!k.contains_key("related_collections"));
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "abc.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("unsplash/jane")
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with(
            "GET /napi/search/photos?query=blue+sky&orientation=landscape&per_page=20&page=1 "
        ));
    }
}
