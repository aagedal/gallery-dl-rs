//! Pexels photos and videos, mirroring gallery_dl/extractor/pexels.py
//! through the web client's `/en-us/api/v3` endpoints.
use super::{date, iso_ts, path_segment, slug_id, str_of, word_char};
use crate::sites::art::{extr, nameext_into, update};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://www.pexels.com";
const API: &str = "https://www.pexels.com/en-us/api";
const FILENAME: &str = "{filename}.{extension}";
static COLLECTION: Route = Route::new(
    "collection",
    FILENAME,
    &["{category}", "Collections", "{collection}"],
    "{id}",
);
static SEARCH: Route = Route::new(
    "search",
    FILENAME,
    &["{category}", "Searches", "{search_tags}"],
    "{id}",
);
static USER: Route = Route::new("user", FILENAME, &["{category}", "@{user[slug]}"], "{id}");
static IMAGE: Route = Route::new("image", FILENAME, &["{category}"], "{id}");

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
    /// `(?:SLUG-)?ID` as the collection name and its ID.
    Collection {
        name: String,
        id: String,
    },
    Search(String),
    /// The `@NAME` segment, or its numeric ID when it has one.
    User(String),
    /// The `(?:SLUG-)?ID` photo path segment.
    Image(String),
}

fn target(url: &Url) -> Option<Target> {
    if !crate::sites::art::on_host(url, &["pexels.com"]) {
        return None;
    }
    let parts: Vec<&str> = url.path()[1..].split('/').collect();
    let digit = |c: char| c.is_ascii_digit();
    match parts.as_slice() {
        ["collections", slug, ..] => {
            let (name, id) = slug_id(slug, word_char)?;
            Some(Target::Collection {
                name: name.into(),
                id: id.into(),
            })
        }
        ["search", query, ..] if path_segment(query) => Some(Target::Search((*query).into())),
        [user, ..] if user.len() > 1 && user.starts_with('@') => {
            let name = &user[1..];
            Some(Target::User(match slug_id(name, digit) {
                Some((_, id)) => id.into(),
                None if path_segment(user) => (*user).into(),
                None => return None,
            }))
        }
        ["photo", slug, ..] => slug_id(slug, digit).map(|(slug, _)| Target::Image(slug.into())),
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Pexels URL"));
    };
    let per_page = |mut params: Vec<(&'static str, String)>| {
        params.push(("page", "1".into()));
        params.push(("per_page", "24".into()));
        params
    };
    match target {
        Target::Collection { name, id } => {
            let mut metadata = Map::new();
            metadata.insert("collection".into(), name.into());
            metadata.insert("collection_id".into(), id.clone().into());
            paginate(
                http,
                &COLLECTION,
                format!("/v3/collections/{id}/media"),
                per_page(Vec::new()),
                metadata,
            )
        }
        Target::Search(query) => {
            let mut metadata = Map::new();
            metadata.insert("search_tags".into(), query.clone().into());
            // Upstream passes the raw segment, which `requests` would encode
            // a second time; the API is sent the decoded search terms.
            let mut params = per_page(vec![("query", crate::sites::percent_decode(&query))]);
            for (name, value) in [
                ("orientation", "all"),
                ("size", "all"),
                ("color", "all"),
                ("sort", "popular"),
            ] {
                params.push((name, value.into()));
            }
            paginate(http, &SEARCH, "/v3/search/photos".into(), params, metadata)
        }
        Target::User(user) => paginate(
            http,
            &USER,
            format!("/v3/users/{user}/media/recent"),
            per_page(Vec::new()),
            Map::new(),
        ),
        Target::Image(slug) => crate::sites::once(async move {
            let page = http
                .text_with(
                    &http.api(&format!("{ROOT}/photo/{slug}/"))?,
                    &http.browser_headers(&[]),
                )
                .await?;
            let medium = next_data(&page)?
                .pointer("/props/pageProps/medium")
                .cloned()
                .ok_or_else(|| error("Pexels page has no photo data"))?;
            Ok(posts_events(http, &IMAGE, vec![medium], &Map::new()))
        }),
    }
}

/// `Extractor._extract_nextdata`.
fn next_data(page: &str) -> Result<Value> {
    let data = Some(extr(
        page,
        " id=\"__NEXT_DATA__\" type=\"application/json\">",
        "</script>",
    ))
    .filter(|data| !data.is_empty())
    .unwrap_or_else(|| {
        extr(
            page,
            " id='__NEXT_DATA__' type='application/json'>",
            "</script>",
        )
    });
    crate::sites::art::json_loads(data, "Pexels")
}

/// `PexelsAPI` request headers, including the web client's static key.
const HEADERS: &[(&str, &str)] = &[
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

/// `PexelsAPI._pagination`: follow `pagination.current_page` until it
/// reaches `total_pages`.
fn paginate<'a>(
    http: &'a Http,
    route: &'static Route,
    endpoint: String,
    params: Vec<(&'static str, String)>,
    metadata: Map<String, Value>,
) -> Events<'a> {
    crate::sites::paginate(Some(1i64), move |page| {
        let endpoint = endpoint.clone();
        let params = params.clone();
        let metadata = metadata.clone();
        async move {
            let Some(page) = page else {
                return Ok(None);
            };
            let mut url = http.api(&format!("{API}{endpoint}"))?;
            url.query_pairs_mut()
                .extend_pairs(params.iter().map(|(k, v)| {
                    let v = if *k == "page" {
                        page.to_string()
                    } else {
                        v.clone()
                    };
                    (*k, v)
                }));
            let body = http
                .json_with(&url, &http.browser_headers(HEADERS))
                .await
                .map_err(|e| error(format!("Pexels API request failed: {e}")))?;
            let posts = body
                .get("data")
                .and_then(Value::as_array)
                .cloned()
                .ok_or_else(|| error("Pexels API response has no data"))?;
            let pagination = body.get("pagination");
            let number = |key| pagination.and_then(|p| p.get(key)).and_then(Value::as_i64);
            let current = number("current_page").unwrap_or(page);
            let next = match number("total_pages") {
                Some(total) if current < total && current < 100_000 => Some(current + 1),
                _ => None,
            };
            Ok(Some((posts_events(http, route, posts, &metadata), next)))
        }
    })
}

/// `items()`: one post per photo or video; other media types are skipped.
fn posts_events<'a>(
    http: &'a Http,
    route: &'static Route,
    posts: Vec<Value>,
    metadata: &Map<String, Value>,
) -> Events<'a> {
    let mut events = Vec::new();
    for post in posts {
        let Value::Object(mut post) = post else {
            continue;
        };
        if let Some(Value::Object(attributes)) = post.get("attributes") {
            let kind = post.get("type").cloned().unwrap_or(Value::Null);
            post = attributes.clone();
            post.insert("type".into(), kind);
        }
        update(&mut post, metadata);
        let created = str_of(&Value::Object(post.clone()), "created_at").to_owned();
        let trimmed: String = {
            let count = created.chars().count();
            created.chars().take(count.saturating_sub(5)).collect()
        };
        let ts = iso_ts(&trimmed);
        post.insert("date".into(), date(ts));
        let (raw, name) = if let Some(image) = post.get("image") {
            let (url, query) = str_of(image, "download_link")
                .split_once('?')
                .unwrap_or((str_of(image, "download_link"), ""));
            (url.to_owned(), extr(query, "&dl=", "&").to_owned())
        } else if let Some(video) = post.get("video") {
            (
                str_of(video, "download_link").to_owned(),
                str_of(video, "src").to_owned(),
            )
        } else {
            continue;
        };
        let Some(url) = crate::sites::media_url(&raw, &["pexels.com"]) else {
            continue;
        };
        nameext_into(&name, &mut post);
        let id = post.get("id").map(crate::sites::scalar).unwrap_or_default();
        let username = post
            .get("user")
            .map(|user| str_of(user, "username").to_owned());
        let caption = post.get("title").and_then(Value::as_str).map(str::to_owned);
        let item = Item::new(url, post)
            .ids(id.clone(), id)
            .user(username)
            .caption(caption)
            .timestamp(ts);
        events.extend(crate::sites::post(&SITE, http, route, vec![item]));
    }
    crate::sites::events(events)
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
            t("https://www.pexels.com/collections/cute-cats-a1b2c3/"),
            Some(Target::Collection {
                name: "cute-cats-a1b2c3".into(),
                id: "a1b2c3".into()
            })
        );
        assert_eq!(
            t("https://pexels.com/collections/abc"),
            Some(Target::Collection {
                name: "abc".into(),
                id: "abc".into()
            })
        );
        assert_eq!(
            t("https://www.pexels.com/search/cute%20cats/"),
            Some(Target::Search("cute%20cats".into()))
        );
        assert_eq!(
            t("https://www.pexels.com/@jane-doe-12345/"),
            Some(Target::User("12345".into()))
        );
        assert_eq!(
            t("https://www.pexels.com/@jane"),
            Some(Target::User("@jane".into()))
        );
        assert_eq!(
            t("https://www.pexels.com/photo/a-cat-on-a-mat-1234567/"),
            Some(Target::Image("a-cat-on-a-mat-1234567".into()))
        );
        for raw in [
            "https://www.pexels.com/",
            "https://www.pexels.com/@",
            "https://www.pexels.com/photo/no-id/",
            "https://www.pexels.com/collections/-/",
            "https://images.pexels.com/photo/x-1/",
            "https://www.pexels.com.evil.test/photo/x-1/",
            "https://evilpexels.com/photo/x-1/",
            "https://www.pexels.com:8080/photo/x-1/",
            "https://u@www.pexels.com/photo/x-1/",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn photo(id: u64) -> Value {
        json!({"id": id.to_string(), "type": "photo", "attributes": {
            "id": id, "slug": "a-cat", "title": "A Cat",
            "created_at": "2020-05-01T10:41:11.000Z",
            "user": {"id": 7, "username": "jane", "slug": "jane-doe"},
            "image": {"download_link": format!(
                "https://images.pexels.com/photos/{id}/pexels-photo-{id}.jpeg?cs=srgb&dl=jane-doe-{id}.jpg&fm=jpg")}}})
    }

    #[tokio::test]
    async fn user_paginates_api() {
        let video = json!({"id": "9", "type": "video", "attributes": {
            "id": 9, "created_at": "2021-01-01T00:00:00.000Z",
            "user": {"id": 7, "username": "jane", "slug": "jane-doe"},
            "video": {"src": "https://videos.pexels.com/video-files/9/clip.mp4",
                      "download_link": "https://www.pexels.com/download/video/9/"}}});
        let (root, server) = serve(vec![
            json(json!({"data": [photo(1)],
                "pagination": {"current_page": 1, "total_pages": 2}})),
            json(json!({"data": [video],
                "pagination": {"current_page": 2, "total_pages": 2}})),
        ]);
        let http = http("pexels", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.pexels.com/@jane-doe-7/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].url.as_str(),
            "https://images.pexels.com/photos/1/pexels-photo-1.jpeg"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["type"], "photo");
        assert_eq!(k["filename"], "jane-doe-1");
        assert_eq!(k["extension"], "jpg");
        assert_eq!(k["date"], "2020-05-01 10:41:11");
        let k = keywords(&files[1]);
        assert_eq!(k["type"], "video");
        assert_eq!(k["filename"], "clip");
        assert_eq!(
            files[1].url.as_str(),
            "https://www.pexels.com/download/video/9/"
        );
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "jane-doe-1.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("pexels/@jane-doe")
        );
        let (_, identity) = crate::sites::archive_key(&files[1]).unwrap();
        assert_eq!(identity, r#"["{id}",["9"]]"#);
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with("GET /en-us/api/v3/users/7/media/recent?page=1&per_page=24 "),
            "{}",
            requests[0]
        );
        assert!(requests[0].contains("secret-key: H2jk9uKnhRmL6WPwh89zBezWvr"));
        assert!(requests[1].contains("?page=2&per_page=24 "));
    }

    #[tokio::test]
    async fn search_and_collection_metadata() {
        let (root, server) = serve(vec![json(json!({"data": [photo(2)],
            "pagination": {"current_page": 1, "total_pages": 1}}))]);
        let http = http("pexels", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.pexels.com/search/cute%20cats/").unwrap(),
        ))
        .await;
        let (_, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("pexels/Searches/cute%20cats")
        );
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with(
                "GET /en-us/api/v3/search/photos?query=cute+cats&page=1&per_page=24&orientation=all&size=all&color=all&sort=popular "
            ),
            "{}",
            requests[0]
        );

        let (root, server) = serve(vec![json(json!({"data": [photo(3)],
            "pagination": {"current_page": 1, "total_pages": 1}}))]);
        let http = crate::sites::testing::http("pexels", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.pexels.com/collections/cats-x1y2/").unwrap(),
        ))
        .await;
        let k = keywords(&files[0]);
        assert_eq!(k["collection"], "cats-x1y2");
        assert_eq!(k["collection_id"], "x1y2");
        let (_, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("pexels/Collections/cats-x1y2")
        );
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with("GET /en-us/api/v3/collections/x1y2/media?page=1&per_page=24 ")
        );
    }

    #[tokio::test]
    async fn image_reads_next_data() {
        let data = json!({"props": {"pageProps": {"medium": photo(5)}}});
        let page =
            format!("<script id=\"__NEXT_DATA__\" type=\"application/json\">{data}</script>");
        let (root, server) = serve(vec![html(&page)]);
        let http = http("pexels", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.pexels.com/photo/a-cat-5/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(keywords(&files[0])["subcategory"], "image");
        let (_, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("pexels")
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /photo/a-cat-5/ "));
    }
}
