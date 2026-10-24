//! Unsplash photos, mirroring gallery_dl/extractor/unsplash.py through the
//! site's `napi` JSON endpoints.
use super::{date, iso_ts, path_segment, set_param, str_of, word_char};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://unsplash.com";
const PER_PAGE: usize = 20;
const FILENAME: &str = "{id}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{user[username]}"];
static IMAGE: Route = Route::new("image", FILENAME, DIRECTORY, "{id}");
static USER: Route = Route::new("user", FILENAME, DIRECTORY, "{id}");
static FAVORITE: Route = Route::new("favorite", FILENAME, DIRECTORY, "{id}");
static COLLECTION: Route = Route::new("collection", FILENAME, DIRECTORY, "{id}");
static SEARCH: Route = Route::new("search", FILENAME, DIRECTORY, "{id}");

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
    Collection {
        id: String,
        title: String,
    },
    /// The `/s/photos/QUERY` segment and the URL's own query string.
    Search {
        item: String,
        query: Option<String>,
    },
}

/// `@(\w+)`.
fn user(segment: &str) -> Option<&str> {
    let name = segment.strip_prefix('@')?;
    (!name.is_empty() && name.chars().all(word_char)).then_some(name)
}

/// `unsplash\.com` URLs as upstream sees them, raw segments included.
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) || url.host_str() != Some("unsplash.com") {
        return None;
    }
    let parts: Vec<&str> = url.path()[1..].split('/').collect();
    let last = |index: usize| parts.len() == index + 1;
    match parts.as_slice() {
        ["photos", id, ..] if path_segment(id) => Some(Target::Image((*id).into())),
        // `/@(\w+)/?$`: nothing may follow, not even an empty query.
        [name] | [name, ""] if url.query().is_none() && url.fragment().is_none() => {
            user(name).map(|name| Target::User(name.into()))
        }
        [name, likes, ..] if likes.starts_with("likes") && user(name).is_some() => {
            user(name).map(|name| Target::Favorite(name.into()))
        }
        ["collections", id, rest @ ..] if path_segment(id) => Some(Target::Collection {
            id: (*id).into(),
            title: rest.first().copied().unwrap_or_default().into(),
        }),
        ["s", "photos", item, ..] if path_segment(item) => Some(Target::Search {
            item: (*item).into(),
            query: url
                .query()
                .filter(|q| !q.is_empty() && last(2))
                .map(str::to_owned),
        }),
        _ => None,
    }
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
            photos_events(http, &IMAGE, vec![photo], &Map::new())
        }),
        Target::User(name) => paginate(
            http,
            &USER,
            format!("{ROOT}/napi/users/{name}/photos"),
            latest(),
            false,
            Map::new(),
        ),
        Target::Favorite(name) => paginate(
            http,
            &FAVORITE,
            format!("{ROOT}/napi/users/{name}/likes"),
            latest(),
            false,
            Map::new(),
        ),
        Target::Collection { id, title } => {
            let mut metadata = Map::new();
            metadata.insert("collection_id".into(), id.clone().into());
            metadata.insert("collection_title".into(), title.into());
            paginate(
                http,
                &COLLECTION,
                format!("{ROOT}/napi/collections/{id}/photos"),
                latest(),
                false,
                metadata,
            )
        }
        Target::Search { item, query } => {
            let mut params = vec![(
                "query".to_owned(),
                crate::sites::percent_decode(&item.replace('-', " ")),
            )];
            for (name, value) in super::parse_query(query.as_deref()) {
                set_param(&mut params, &name, value);
            }
            paginate(
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

/// `_pagination`: pages of 20 from page one until a short page; search
/// results are wrapped in `results`.
fn paginate<'a>(
    http: &'a Http,
    route: &'static Route,
    endpoint: String,
    mut params: Vec<(String, String)>,
    results: bool,
    metadata: Map<String, Value>,
) -> Events<'a> {
    set_param(&mut params, "per_page", PER_PAGE.to_string());
    crate::sites::paginate(Some(1u64), move |page| {
        let endpoint = endpoint.clone();
        let mut params = params.clone();
        let metadata = metadata.clone();
        async move {
            let Some(page) = page else {
                return Ok(None);
            };
            set_param(&mut params, "page", page.to_string());
            let mut url = http.api(&endpoint)?;
            url.query_pairs_mut().extend_pairs(&params);
            let body = http.json(&url).await?;
            let body = if results {
                body.get("results")
            } else {
                Some(&body)
            };
            let photos = body
                .and_then(Value::as_array)
                .cloned()
                .ok_or_else(|| error("Unsplash page has no photos"))?;
            let next = (photos.len() >= PER_PAGE && page < 100_000).then_some(page + 1);
            Ok(Some((photos_events(http, route, photos, &metadata)?, next)))
        }
    })
}

/// `items()`: one post per photo in the configured `format` (`raw`).
fn photos_events<'a>(
    http: &'a Http,
    route: &'static Route,
    photos: Vec<Value>,
    metadata: &Map<String, Value>,
) -> Result<Events<'a>> {
    let format = match http.option("format") {
        Some(Value::String(f)) if !f.is_empty() => f.clone(),
        _ => "raw".to_owned(),
    };
    let mut events = Vec::new();
    for photo in photos {
        let Value::Object(mut photo) = photo else {
            continue;
        };
        photo.remove("current_user_collections");
        photo.remove("related_collections");
        let raw = photo
            .get("urls")
            .map(|urls| str_of(urls, &format))
            .unwrap_or_default()
            .to_owned();
        let url = crate::sites::media_url(&raw, &["unsplash.com"])
            .ok_or_else(|| error(format!("Unsplash photo has no '{format}' URL")))?;
        crate::sites::art::nameext_into(&raw, &mut photo);
        crate::sites::art::update(&mut photo, metadata);
        photo.insert("extension".into(), "jpg".into());
        let ts = iso_ts(str_of(&Value::Object(photo.clone()), "created_at"));
        photo.insert("date".into(), date(ts));
        if let Some(Value::Array(tags)) = photo.get("tags") {
            let titles: Vec<Value> = tags
                .iter()
                .map(|tag| tag.get("title").cloned().unwrap_or(Value::Null))
                .collect();
            photo.insert("tags".into(), titles.into());
        }
        let id = photo
            .get("id")
            .map(crate::sites::scalar)
            .unwrap_or_default();
        let username = photo
            .get("user")
            .map(|user| str_of(user, "username").to_owned());
        let caption = ["description", "alt_description"]
            .iter()
            .find_map(|key| photo.get(*key).and_then(Value::as_str))
            .map(str::to_owned);
        let item = Item::new(url, photo)
            .ids(id.clone(), id)
            .user(username)
            .caption(caption)
            .timestamp(ts);
        events.extend(crate::sites::post(&SITE, http, route, vec![item]));
    }
    Ok(crate::sites::events(events))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http, json, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://unsplash.com/photos/red-wall-hwLAI5lRhdM"),
            Some(Target::Image("red-wall-hwLAI5lRhdM".into()))
        );
        assert_eq!(
            t("https://unsplash.com/@davehoefler/"),
            Some(Target::User("davehoefler".into()))
        );
        assert_eq!(
            t("https://unsplash.com/@davehoefler/likes"),
            Some(Target::Favorite("davehoefler".into()))
        );
        assert_eq!(
            t("https://unsplash.com/collections/3178572/winter"),
            Some(Target::Collection {
                id: "3178572".into(),
                title: "winter".into()
            })
        );
        assert_eq!(
            t("https://unsplash.com/collections/3178572/"),
            Some(Target::Collection {
                id: "3178572".into(),
                title: String::new()
            })
        );
        assert_eq!(
            t("https://unsplash.com/s/photos/hair-style?orientation=portrait"),
            Some(Target::Search {
                item: "hair-style".into(),
                query: Some("orientation=portrait".into())
            })
        );
        for raw in [
            "https://unsplash.com/",
            "https://unsplash.com/@user?x=1",
            "https://unsplash.com/@user/collections",
            "https://unsplash.com/@us-er",
            "https://unsplash.com/photos/",
            "https://unsplash.com/photos/%2e%2e",
            "https://www.unsplash.com/photos/abc",
            "https://unsplash.com.evil.test/photos/abc",
            "https://evilunsplash.com/photos/abc",
            "https://unsplash.com:8443/photos/abc",
            "https://u:p@unsplash.com/photos/abc",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn photo(id: &str) -> Value {
        json!({"id": id, "created_at": "2021-03-04T05:06:07Z",
            "description": null, "alt_description": "a red wall",
            "user": {"username": "dave", "name": "Dave"},
            "tags": [{"title": "red", "type": "search"}, {"title": "wall"}],
            "current_user_collections": [], "related_collections": {"total": 3},
            "urls": {"raw": format!("https://images.unsplash.com/photo-{id}?ixid=x&ixlib=rb-4"),
                     "full": "https://images.unsplash.com/full.jpg"}})
    }

    #[tokio::test]
    async fn extracts_one_photo() {
        let (root, server) = serve(vec![json(photo("abc"))]);
        let http = http("unsplash", root);
        let files = media(extract(
            &http,
            Url::parse("https://unsplash.com/photos/abc").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].url.as_str(),
            "https://images.unsplash.com/photo-abc?ixid=x&ixlib=rb-4"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["filename"], "photo-abc");
        assert_eq!(k["extension"], "jpg");
        assert_eq!(k["date"], "2021-03-04 05:06:07");
        assert_eq!(k["tags"], json!(["red", "wall"]));
        assert_eq!(k["subcategory"], "image");
        assert!(!k.contains_key("related_collections"));
        assert!(!k.contains_key("current_user_collections"));
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "abc.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("unsplash/dave")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{id}",["abc"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /napi/photos/abc "));
    }

    #[tokio::test]
    async fn collection_paginates_until_a_short_page() {
        let full: Vec<Value> = (0..20).map(|i| photo(&format!("p{i}"))).collect();
        let (root, server) = serve(vec![json(Value::Array(full)), json(json!([photo("last")]))]);
        let http = http("unsplash", root);
        let files = media(extract(
            &http,
            Url::parse("https://unsplash.com/collections/123/winter").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 21);
        let k = keywords(&files[20]);
        assert_eq!(k["collection_id"], "123");
        assert_eq!(k["collection_title"], "winter");
        assert_eq!(k["subcategory"], "collection");
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with(
                "GET /napi/collections/123/photos?order_by=latest&per_page=20&page=1 "
            ),
            "{}",
            requests[0]
        );
        assert!(requests[1].contains("&page=2 "));
    }

    #[tokio::test]
    async fn search_merges_url_query() {
        let (root, server) = serve(vec![json(json!({"total": 1, "results": [photo("s")]}))]);
        let http = http("unsplash", root);
        let files = media(extract(
            &http,
            Url::parse("https://unsplash.com/s/photos/hair-style?orientation=portrait&page=3")
                .unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with(
                "GET /napi/search/photos?query=hair+style&orientation=portrait&page=1&per_page=20 "
            ),
            "{}",
            requests[0]
        );
    }
}
