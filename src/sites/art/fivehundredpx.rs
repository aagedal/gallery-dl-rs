//! 500px photostreams, galleries and photos.
//!
//! Upstream gallery_dl/extractor/500px.py targets the retired
//! `api.500px.com` (GraphQL and `v1/photos`). The site now runs on the
//! `api-neo.500px.com` GraphQL API (operations as named in the web app's
//! bundle), keyed by opaque node IDs; usernames and gallery slugs are
//! resolved to those first. Numeric `id`s are still taken from the CDN
//! URLs (`/photo/<id>/`), so archive entries stay compatible.
use crate::event::ExtractionEvent;
use crate::sites::gdl::truthy;
use crate::sites::{Events, Http, Item, Route, Site, scalar};
use crate::{Result, error};
use serde_json::{Map, Value, json};
use url::Url;

const API: &str = "https://api-neo.500px.com/graphql";
const PAGE_SIZE: u64 = 50;

pub static SITE: Site = Site {
    category: "500px",
    name: "500px",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&USER, &GALLERY, &IMAGE],
    api_key: false,
    cookies: None,
};

const FILENAME: &str = "{id}_{name}.{extension}";
static USER: Route = Route::new(
    "user",
    FILENAME,
    &["{category}", "{user[username]}"],
    "{id}",
);
static GALLERY: Route = Route::new(
    "gallery",
    FILENAME,
    &["{category}", "{user[username]}", "{gallery[name]}"],
    "{id}",
);
static IMAGE: Route = Route::new(
    "image",
    FILENAME,
    &["{category}", "{user[username]}"],
    "{id}",
);

#[derive(Debug, PartialEq)]
enum Target {
    User(String),
    /// Username and gallery slug (`/p/USER/galleries/SLUG`).
    Gallery(String, String),
    /// A gallery node ID (`/gallery/ID`).
    GalleryId(String),
    /// A photo node ID, or an old numeric ID (`/photo/ID/slug`).
    Image(String),
}

fn target(url: &Url) -> Option<Target> {
    let host = url.host_str()?;
    if !crate::sites::plain(url) || !matches!(host, "500px.com" | "web.500px.com") {
        return None;
    }
    let path = url.path().strip_prefix('/')?;
    let parts: Vec<&str> = path.split('/').collect();
    let id = |id: &str| {
        (!id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric())).then(|| id.to_owned())
    };
    match parts.as_slice() {
        ["photo", photo, ..] => return id(photo).map(Target::Image),
        ["gallery", gallery, ..] => return id(gallery).map(Target::GalleryId),
        _ => {}
    }
    // `(?!photo/|liked)(?:p/)?([^/?#]+)/?$`
    if !path.starts_with("liked") {
        let user = match parts.as_slice() {
            ["p", user] | ["p", user, ""] if !user.is_empty() => Some(*user),
            [user] | [user, ""] if !user.is_empty() => Some(*user),
            _ => None,
        };
        if let Some(user) = user {
            return Some(Target::User(user.to_owned()));
        }
    }
    // `(?!photo/)(?:p/)?([^/?#]+)/galleries/([^/?#]+)`
    let gallery = |parts: &[&str]| match parts {
        [user, "galleries", gallery, ..] if !user.is_empty() && !gallery.is_empty() => {
            Some(Target::Gallery((*user).to_owned(), (*gallery).to_owned()))
        }
        _ => None,
    };
    match parts.as_slice() {
        ["p", rest @ ..] => gallery(rest).or_else(|| gallery(&parts)),
        _ => gallery(&parts),
    }
}

/// Photo fields requested from every listing.
const PHOTO_FIELDS: &str = "__typename ... on Photo { id title description width height \
    isNsfw likeCount viewCount urls { size_4k } uploader { id username displayName } }";

/// One GraphQL call; `field` is the response's root field.
async fn graphql(http: &Http, query: &str, field: &str, variables: Value) -> Result<Value> {
    let body = json!({"query": query, "variables": variables});
    // The web app sends a random per-device UUID; ID lookups require one.
    let hex = crate::sites::gdl::generate_token(16);
    let device = format!(
        "{}-{}-4{}-a{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[13..16],
        &hex[17..20],
        &hex[20..32]
    );
    let mut data = http
        .post_json(
            &http.api(API)?,
            &body,
            &[("x-500px-device-id", device.as_str())],
        )
        .await?;
    if let Some(message) = data["errors"][0]["message"].as_str() {
        return Err(error(format!("500px API error: {message}")));
    }
    Ok(data["data"][field].take())
}

/// `getUser(username)`: the user node.
async fn user_node(http: &Http, username: &str) -> Result<Value> {
    let user = graphql(
        http,
        "query($u: String!) { getUser(username: $u) { id username displayName } }",
        "getUser",
        json!({"u": username}),
    )
    .await?;
    if user.is_null() {
        return Err(error("500px user not found"));
    }
    Ok(user)
}

/// The user and gallery of a `/galleries/SLUG` URL: galleries are matched
/// by their name's lowercase, dash-separated form.
async fn gallery_by_slug(http: &Http, username: &str, slug: &str) -> Result<(Value, Value)> {
    let user = user_node(http, username).await?;
    let want = slug.to_lowercase();
    let mut after = Value::Null;
    loop {
        let page = graphql(
            http,
            "query($id: ID!, $first: Int, $after: String) { pageGalleries(userId: $id, first: $first, after: $after) \
             { edges { node { id name description itemCount } } pageInfo { hasNextPage endCursor } } }",
            "pageGalleries",
            json!({"id": user["id"], "first": PAGE_SIZE, "after": after}),
        )
        .await?;
        for edge in page["edges"].as_array().into_iter().flatten() {
            let node = &edge["node"];
            let name = node["name"].as_str().unwrap_or_default();
            if node["id"] == want.as_str() || slugify(name) == want {
                return Ok((user, node.clone()));
            }
        }
        if !truthy(page["pageInfo"].get("hasNextPage")) {
            return Err(error("500px gallery not found"));
        }
        after = page["pageInfo"]["endCursor"].clone();
    }
}

fn slugify(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_owned()
}

#[derive(Clone)]
enum Listing {
    /// `pageResources(userId)`.
    User(String),
    /// `pageGalleryItems(galleryId)`.
    Gallery(String),
}

/// Cursor-paginated photo listings.
fn photo_pages<'a>(
    http: &'a Http,
    route: &'static Route,
    data: Map<String, Value>,
    listing: Listing,
) -> Events<'a> {
    crate::sites::paginate(Some(Value::Null), move |state| {
        let (data, listing) = (data.clone(), listing.clone());
        async move {
            let Some(after) = state else { return Ok(None) };
            let (query, field, id) = match &listing {
                Listing::User(id) => (
                    format!(
                        "query($id: ID!, $first: Int, $after: String) {{ pageResources(userId: $id, first: $first, \
                         after: $after, resourceTypes: [PHOTO], sort: CREATED_AT_DESC) {{ edges {{ node {{ {PHOTO_FIELDS} }} }} \
                         pageInfo {{ hasNextPage endCursor }} }} }}"
                    ),
                    "pageResources",
                    id,
                ),
                Listing::Gallery(id) => (
                    format!(
                        "query($id: ID!, $first: Int, $after: String) {{ pageGalleryItems(galleryId: $id, first: $first, \
                         after: $after) {{ edges {{ node {{ {PHOTO_FIELDS} }} }} pageInfo {{ hasNextPage endCursor }} }} }}"
                    ),
                    "pageGalleryItems",
                    id,
                ),
            };
            let page = graphql(
                http,
                &query,
                field,
                json!({"id": id, "first": PAGE_SIZE, "after": after}),
            )
            .await?;
            let photos: Vec<Value> = page["edges"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|edge| edge["node"].clone())
                .collect();
            let events = photo_events(http, route, photos, &data);
            let info = &page["pageInfo"];
            let next = truthy(info.get("hasNextPage"))
                .then(|| info["endCursor"].clone())
                .filter(|cursor| !cursor.is_null());
            Ok(Some((crate::sites::events(events), next)))
        }
    })
}

fn photo_events(
    http: &Http,
    route: &'static Route,
    photos: Vec<Value>,
    data: &Map<String, Value>,
) -> Vec<ExtractionEvent> {
    photos
        .iter()
        .filter_map(|photo| parse_photo(photo, data))
        .flat_map(|item| crate::sites::post(&SITE, http, route, vec![item]))
        .collect()
}

/// A photo node's keywords, with upstream's `id`/`name`/`user` names.
fn parse_photo(photo: &Value, data: &Map<String, Value>) -> Option<Item> {
    if photo.get("__typename").is_some_and(|t| t != "Photo") {
        return None;
    }
    let raw = photo["urls"]["size_4k"].as_str()?;
    let url = Url::parse(raw).ok().filter(crate::sites::plain)?;
    // `…/photo/<legacy id>/<n>.jpg`
    let segments: Vec<&str> = url.path_segments()?.collect();
    let legacy = match segments.as_slice() {
        ["photo", id, ..] if id.bytes().all(|b| b.is_ascii_digit()) => (*id).to_owned(),
        _ => scalar(&photo["id"]),
    };
    let mut fields = Map::new();
    fields.insert(
        "id".into(),
        legacy
            .parse::<i64>()
            .map_or_else(|_| legacy.clone().into(), Value::from),
    );
    fields.insert("node_id".into(), photo["id"].clone());
    fields.insert("name".into(), photo["title"].clone());
    for key in [
        "description",
        "width",
        "height",
        "isNsfw",
        "likeCount",
        "viewCount",
    ] {
        fields.insert(key.into(), photo.get(key).cloned().unwrap_or(Value::Null));
    }
    let uploader = &photo["uploader"];
    fields.insert(
        "user".into(),
        json!({"id": uploader["id"], "username": uploader["username"], "fullname": uploader["displayName"]}),
    );
    let (filename, extension) = crate::sites::nameext(&url);
    fields.insert("filename".into(), filename.into());
    fields.insert(
        "extension".into(),
        if extension.is_empty() {
            "jpg".into()
        } else {
            extension.into()
        },
    );
    fields.insert("url".into(), raw.into());
    super::update(&mut fields, data);
    let user = uploader["username"].as_str().map(str::to_owned);
    Some(
        Item::new(url, fields)
            .ids(legacy.clone(), legacy)
            .user(user)
            .caption(photo["title"].as_str().map(str::to_owned)),
    )
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::User(username)) => crate::sites::once(async move {
            let user = user_node(http, &username).await?;
            let id = scalar(&user["id"]);
            Ok(photo_pages(http, &USER, Map::new(), Listing::User(id)))
        }),
        Some(Target::Gallery(username, slug)) => crate::sites::once(async move {
            let (_, gallery) = gallery_by_slug(http, &username, &slug).await?;
            let mut data = Map::new();
            let id = scalar(&gallery["id"]);
            data.insert("gallery".into(), gallery);
            Ok(photo_pages(http, &GALLERY, data, Listing::Gallery(id)))
        }),
        Some(Target::GalleryId(id)) => crate::sites::once(async move {
            let gallery = graphql(
                http,
                "query($id: ID!) { getGalleryById(id: $id) { id name description itemCount } }",
                "getGalleryById",
                json!({"id": id}),
            )
            .await?;
            if gallery.is_null() {
                return Err(error("500px gallery not found"));
            }
            let mut data = Map::new();
            data.insert("gallery".into(), gallery);
            Ok(photo_pages(http, &GALLERY, data, Listing::Gallery(id)))
        }),
        Some(Target::Image(id)) => crate::sites::once(async move {
            if id.bytes().all(|b| b.is_ascii_digit()) {
                return Err(error(
                    "old numeric 500px photo URLs cannot be resolved; use the photo's current /photo/ URL",
                ));
            }
            let photo = graphql(
                http,
                &format!("query($id: ID!) {{ getPhotoById(id: $id) {{ {PHOTO_FIELDS} }} }}"),
                "getPhotoById",
                json!({"id": id}),
            )
            .await?;
            Ok(crate::sites::events(photo_events(
                http,
                &IMAGE,
                vec![photo],
                &Map::new(),
            )))
        }),
        None => crate::sites::fail(error("unsupported 500px URL")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http as client, json as reply, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://500px.com/p/fashvamp"),
            Some(Target::User("fashvamp".into()))
        );
        assert_eq!(
            t("https://web.500px.com/fashvamp"),
            Some(Target::User("fashvamp".into()))
        );
        assert_eq!(
            t("https://500px.com/p/fashvamp/galleries/lera"),
            Some(Target::Gallery("fashvamp".into(), "lera".into()))
        );
        assert_eq!(
            t("https://500px.com/gallery/l2L5QYAEkO"),
            Some(Target::GalleryId("l2L5QYAEkO".into()))
        );
        assert_eq!(
            t("https://500px.com/photo/SDjsz9u8wrn"),
            Some(Target::Image("SDjsz9u8wrn".into()))
        );
        assert_eq!(t("https://500px.com/liked"), None);
        assert_eq!(slugify("Nature&landscapes"), "nature-landscapes");
    }

    fn photo(id: &str, legacy: u64) -> Value {
        json!({"__typename": "Photo", "id": id, "title": "Queen Of Coasts", "width": 5184, "height": 3456,
            "urls": {"size_4k": format!("https://cdn-resize-prod-com.500px.cloud/photo/{legacy}/0.jpg?s=2&sig=x")},
            "uploader": {"id": "YB9lC9vLfM", "username": "fashvamp", "displayName": "Alice"}})
    }

    #[tokio::test]
    async fn gallery_slug_and_pages() {
        let (root, server) = serve(vec![
            reply(json!({"data": {"getUser": {"id": "YB9lC9vLfM", "username": "fashvamp"}}})),
            reply(json!({"data": {"pageGalleries": {"edges": [
                {"node": {"id": "8KRRib2M1j", "name": "LIFESTYLE"}},
                {"node": {"id": "l2L5QYAEkO", "name": "Lera", "itemCount": 2}}],
                "pageInfo": {"hasNextPage": false}}}})),
            reply(
                json!({"data": {"pageGalleryItems": {"edges": [{"node": photo("zwygs0OXuy", 249720625)}],
                "pageInfo": {"hasNextPage": true, "endCursor": "c1"}}}}),
            ),
            reply(
                json!({"data": {"pageGalleryItems": {"edges": [{"node": photo("cjNOfdAuiF", 234428959)}],
                "pageInfo": {"hasNextPage": false, "endCursor": null}}}}),
            ),
        ]);
        let http = client("500px", root);
        let files = media(extract(
            &http,
            Url::parse("https://500px.com/p/fashvamp/galleries/lera").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[0]);
        assert_eq!(
            (k["id"].as_i64(), k["gallery"]["name"].as_str()),
            (Some(249720625), Some("Lera"))
        );
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            file.render(&files[0]).unwrap(),
            "249720625_Queen Of Coasts.jpg"
        );
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            crate::sites::testing::os_dir("500px/fashvamp/Lera")
        );
        let requests = server.join().unwrap();
        assert!(requests[2].contains("\"id\":\"l2L5QYAEkO\""));
        assert!(requests[3].contains("\"after\":\"c1\""));
    }

    #[tokio::test]
    async fn api_errors_surface() {
        let (root, _server) = serve(vec![reply(
            json!({"errors": [{"message": "Incorrect GraphQL Node ID"}]}),
        )]);
        let http = client("500px", root);
        assert!(
            crate::sites::testing::fails(extract(
                &http,
                Url::parse("https://500px.com/photo/abcDEF123").unwrap()
            ))
            .await
        );
    }
}
