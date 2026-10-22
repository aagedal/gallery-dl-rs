//! 500px photostreams, galleries and photos, mirroring
//! gallery_dl/extractor/500px.py through the GraphQL and `v1/photos` APIs.
use super::{digits, parse_iso, str_of};
use crate::event::ExtractionEvent;
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value, json};
use url::Url;

const ROOT: &str = "https://500px.com";
const GRAPHQL: &str = "https://api.500px.com/graphql";
const PHOTOS: &str = "https://api.500px.com/v1/photos";
const PAGE_SIZE: u64 = 20;

pub static SITE: Site = Site {
    category: "500px",
    name: "500px",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&USER, &GALLERY, &IMAGE],
    api_key: false,
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
    Gallery(String, String),
    Image(String),
}

fn target(url: &Url) -> Option<Target> {
    let host = url.host_str()?;
    if !crate::sites::plain(url) || !matches!(host, "500px.com" | "web.500px.com") {
        return None;
    }
    let path = url.path().strip_prefix('/')?;
    let parts: Vec<&str> = path.split('/').collect();
    if let ["photo", id, ..] = parts.as_slice() {
        let len = id.bytes().take_while(u8::is_ascii_digit).count();
        return digits(&id[..len]).then(|| Target::Image(id[..len].to_owned()));
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

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::User(user)) => photo_pages(
            http,
            &USER,
            Map::new(),
            Kind::User(user),
            Step::Cursor(None),
        ),
        Some(Target::Gallery(user, slug)) => crate::sites::once(async move {
            let (data, user_id, photos) = gallery_metadata(http, &user, &slug).await?;
            Ok(photo_pages(
                http,
                &GALLERY,
                data,
                Kind::Gallery(user_id, slug),
                Step::Photos(photos),
            ))
        }),
        Some(Target::Image(id)) => crate::sites::once(async move {
            let photos = extend(http, &[id]).await?;
            Ok(crate::sites::events(photo_events(
                http,
                &IMAGE,
                photos,
                &Map::new(),
            )))
        }),
        None => crate::sites::fail(error("unsupported 500px URL")),
    }
}

/// Upstream `_request_graphql`; `variables` is sent as a JSON string.
async fn graphql(http: &Http, name: &str, variables: Value) -> Result<Value> {
    let query = QUERIES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, q)| *q)
        .ok_or_else(|| error("unknown 500px query"))?;
    let body = json!({
        "operationName": name,
        "variables": variables.to_string(),
        "query": query,
    });
    let url = http.api(GRAPHQL)?;
    let mut response = http.post_json(&url, &body, &[]).await?;
    match response.get_mut("data").map(Value::take) {
        Some(data @ Value::Object(_)) => Ok(data),
        _ => Err(error("500px GraphQL response contains no data")),
    }
}

/// Upstream `_extend`: full photo objects for `ids`, in order.
async fn extend(http: &Http, ids: &[String]) -> Result<Vec<Value>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut url = http.api(PHOTOS)?;
    url.query_pairs_mut()
        .append_pair("expanded_user_info", "true")
        .append_pair("include_tags", "true")
        .append_pair("include_geo", "true")
        .append_pair("include_equipment_info", "true")
        .append_pair("vendor_photos", "true")
        .append_pair("include_licensing", "true")
        .append_pair("include_releases", "true")
        .append_pair("liked_by", "1")
        .append_pair("following_sample", "100")
        .append_pair("image_size", "4096")
        .append_pair("ids", &ids.join(","));
    let body = http.json_with(&url, &[("Origin", ROOT)]).await?;
    let photos = body
        .get("photos")
        .and_then(Value::as_object)
        .ok_or_else(|| error("500px photos response contains no photos"))?;
    Ok(ids
        .iter()
        .filter_map(|id| photos.get(id).cloned())
        .collect())
}

fn edge_ids(photos: &Value) -> Vec<String> {
    photos
        .get("edges")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|edge| edge.get("node")?.get("legacyId"))
        .filter(|id| id.is_number() || id.is_string())
        .map(crate::sites::scalar)
        .collect()
}

#[derive(Clone)]
enum Kind {
    User(String),
    Gallery(String, String),
}

enum Step {
    Photos(Value),
    Cursor(Option<String>),
}

async fn fetch_photos(http: &Http, kind: &Kind, cursor: Option<String>) -> Result<Value> {
    let (name, variables, path): (&str, Value, &[&str]) = match (kind, cursor) {
        (Kind::User(user), None) => (
            "OtherPhotosQuery",
            json!({"username": user, "pageSize": PAGE_SIZE}),
            &["user", "photos"],
        ),
        (Kind::User(user), Some(cursor)) => (
            "OtherPhotosPaginationContainerQuery",
            json!({"username": user, "pageSize": PAGE_SIZE, "cursor": cursor}),
            &["userByUsername", "photos"],
        ),
        (Kind::Gallery(owner, slug), cursor) => (
            "GalleriesDetailPaginationContainerQuery",
            json!({"ownerLegacyId": owner, "slug": slug, "token": null,
                "pageSize": PAGE_SIZE, "cursor": cursor}),
            &["galleryByOwnerIdAndSlugOrToken", "photos"],
        ),
    };
    let mut data = graphql(http, name, variables).await?;
    for key in path {
        data = data
            .get_mut(*key)
            .map(Value::take)
            .filter(|v| !v.is_null())
            .ok_or_else(|| error("500px user or gallery not found"))?;
    }
    Ok(data)
}

/// Pages of 20 GraphQL edges, each extended through `v1/photos`.
fn photo_pages<'a>(
    http: &'a Http,
    route: &'static Route,
    data: Map<String, Value>,
    kind: Kind,
    start: Step,
) -> Events<'a> {
    crate::sites::paginate(Some(start), move |step| {
        let kind = kind.clone();
        let data = data.clone();
        async move {
            let Some(step) = step else {
                return Ok(None);
            };
            let photos = match step {
                Step::Photos(photos) => photos,
                Step::Cursor(cursor) => fetch_photos(http, &kind, cursor).await?,
            };
            let list = extend(http, &edge_ids(&photos)).await?;
            let events = photo_events(http, route, list, &data);
            let info = photos.get("pageInfo");
            let next = info
                .and_then(|i| i.get("hasNextPage"))
                .and_then(Value::as_bool)
                .unwrap_or(false)
                .then(|| {
                    let cursor = info.map(|i| str_of(i, "endCursor").to_owned());
                    Step::Cursor(cursor)
                });
            Ok(Some((crate::sites::events(events), next)))
        }
    })
}

/// Upstream gallery `metadata`: the profile and the gallery's first page.
async fn gallery_metadata(
    http: &Http,
    user: &str,
    slug: &str,
) -> Result<(Map<String, Value>, String, Value)> {
    let mut profile = graphql(http, "ProfileRendererQuery", json!({"username": user})).await?;
    let user = profile
        .get_mut("profile")
        .map(Value::take)
        .filter(Value::is_object)
        .ok_or_else(|| error("500px user not found"))?;
    let user_id = user
        .get("legacyId")
        .map(crate::sites::scalar)
        .ok_or_else(|| error("500px profile has no ID"))?;
    let variables = json!({
        "galleryOwnerLegacyId": user_id,
        "ownerLegacyId": user_id,
        "slug": slug,
        "token": null,
        "pageSize": PAGE_SIZE,
    });
    let mut data = graphql(http, "GalleriesDetailQueryRendererQuery", variables).await?;
    let mut gallery = data
        .get_mut("gallery")
        .map(Value::take)
        .filter(Value::is_object)
        .ok_or_else(|| error("500px gallery not found"))?;
    let photos = gallery
        .as_object_mut()
        .and_then(|g| g.remove("photos"))
        .unwrap_or(Value::Null);
    let mut meta = Map::new();
    meta.insert("gallery".into(), gallery);
    meta.insert("user".into(), user);
    Ok((meta, user_id, photos))
}

/// One Directory per photo, as upstream.
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

pub(crate) fn parse_photo(photo: &Value, data: &Map<String, Value>) -> Option<Item> {
    let mut fields = photo.as_object()?.clone();
    let raw = photo
        .get("images")?
        .as_array()?
        .last()?
        .get("url")?
        .as_str()?;
    let url = Url::parse(raw).ok().filter(crate::sites::plain)?;
    let (filename, _) = crate::sites::nameext(&url);
    fields.insert("filename".into(), filename.into());
    let extension = fields.get("image_format").cloned().unwrap_or(Value::Null);
    fields.insert("extension".into(), extension);
    super::update(&mut fields, data);
    let id = fields
        .get("id")
        .map(crate::sites::scalar)
        .unwrap_or_default();
    let user = fields.get("user").map(|u| str_of(u, "username").to_owned());
    let created = parse_iso(str_of(photo, "created_at"));
    Some(
        Item::new(url, fields)
            .ids(id.clone(), id)
            .user(user)
            .caption(Some(str_of(photo, "name").to_owned()))
            .timestamp(created),
    )
}

/// Upstream `utils/500px_graphql.py` query texts.
static QUERIES: &[(&str, &str)] = &[
    (
        "OtherPhotosQuery",
        r#"query OtherPhotosQuery($username: String!, $pageSize: Int) {
  user: userByUsername(username: $username) {
    ...OtherPhotosPaginationContainer_user_RlXb8
    id
  }
}

fragment OtherPhotosPaginationContainer_user_RlXb8 on User {
  photos(first: $pageSize, privacy: PROFILE, sort: ID_DESC) {
    edges {
      node {
        id
        legacyId
        canonicalPath
        width
        height
        name
        isLikedByMe
        notSafeForWork
        photographer: uploader {
          id
          legacyId
          username
          displayName
          canonicalPath
          followedByUsers {
            isFollowedByMe
          }
        }
        images(sizes: [33, 35]) {
          size
          url
          jpegUrl
          webpUrl
          id
        }
        __typename
      }
      cursor
    }
    totalCount
    pageInfo {
      endCursor
      hasNextPage
    }
  }
}
"#,
    ),
    (
        "OtherPhotosPaginationContainerQuery",
        r#"query OtherPhotosPaginationContainerQuery($username: String!, $pageSize: Int, $cursor: String) {
  userByUsername(username: $username) {
    ...OtherPhotosPaginationContainer_user_3e6UuE
    id
  }
}

fragment OtherPhotosPaginationContainer_user_3e6UuE on User {
  photos(first: $pageSize, after: $cursor, privacy: PROFILE, sort: ID_DESC) {
    edges {
      node {
        id
        legacyId
        canonicalPath
        width
        height
        name
        isLikedByMe
        notSafeForWork
        photographer: uploader {
          id
          legacyId
          username
          displayName
          canonicalPath
          followedByUsers {
            isFollowedByMe
          }
        }
        images(sizes: [33, 35]) {
          size
          url
          jpegUrl
          webpUrl
          id
        }
        __typename
      }
      cursor
    }
    totalCount
    pageInfo {
      endCursor
      hasNextPage
    }
  }
}
"#,
    ),
    (
        "ProfileRendererQuery",
        r#"query ProfileRendererQuery($username: String!) {
  profile: userByUsername(username: $username) {
    id
    legacyId
    userType: type
    username
    firstName
    displayName
    registeredAt
    canonicalPath
    avatar {
      ...ProfileAvatar_avatar
      id
    }
    userProfile {
      firstname
      lastname
      state
      country
      city
      about
      id
    }
    socialMedia {
      website
      twitter
      instagram
      facebook
      id
    }
    coverPhotoUrl
    followedByUsers {
      totalCount
      isFollowedByMe
    }
    followingUsers {
      totalCount
    }
    membership {
      expiryDate
      membershipTier: tier
      photoUploadQuota
      refreshPhotoUploadQuotaAt
      paymentStatus
      id
    }
    profileTabs {
      tabs {
        name
        visible
      }
    }
    ...EditCover_cover
    photoStats {
      likeCount
      viewCount
    }
    photos(privacy: PROFILE) {
      totalCount
    }
    licensingPhotos(status: ACCEPTED) {
      totalCount
    }
    portfolio {
      id
      status
      userDisabled
    }
  }
}

fragment EditCover_cover on User {
  coverPhotoUrl
}

fragment ProfileAvatar_avatar on UserAvatar {
  images(sizes: [MEDIUM, LARGE]) {
    size
    url
    id
  }
}
"#,
    ),
    (
        "GalleriesDetailQueryRendererQuery",
        r#"query GalleriesDetailQueryRendererQuery($galleryOwnerLegacyId: ID!, $ownerLegacyId: String, $slug: String, $token: String, $pageSize: Int, $gallerySize: Int) {
  galleries(galleryOwnerLegacyId: $galleryOwnerLegacyId, first: $gallerySize) {
    edges {
      node {
        legacyId
        description
        name
        privacy
        canonicalPath
        notSafeForWork
        buttonName
        externalUrl
        cover {
          images(sizes: [35, 33]) {
            size
            webpUrl
            jpegUrl
            id
          }
          id
        }
        photos {
          totalCount
        }
        id
      }
    }
  }
  gallery: galleryByOwnerIdAndSlugOrToken(ownerLegacyId: $ownerLegacyId, slug: $slug, token: $token) {
    ...GalleriesDetailPaginationContainer_gallery_RlXb8
    id
  }
}

fragment GalleriesDetailPaginationContainer_gallery_RlXb8 on Gallery {
  id
  legacyId
  name
  privacy
  notSafeForWork
  ownPhotosOnly
  canonicalPath
  publicSlug
  lastPublishedAt
  photosAddedSinceLastPublished
  reportStatus
  creator {
    legacyId
    id
  }
  cover {
    images(sizes: [33, 32, 36, 2048]) {
      url
      size
      webpUrl
      id
    }
    id
  }
  description
  externalUrl
  buttonName
  photos(first: $pageSize) {
    totalCount
    edges {
      cursor
      node {
        id
        legacyId
        canonicalPath
        name
        description
        category
        uploadedAt
        location
        width
        height
        isLikedByMe
        photographer: uploader {
          id
          legacyId
          username
          displayName
          canonicalPath
          avatar {
            images(sizes: SMALL) {
              url
              id
            }
            id
          }
          followedByUsers {
            totalCount
            isFollowedByMe
          }
        }
        images(sizes: [33, 32]) {
          size
          url
          webpUrl
          id
        }
        __typename
      }
    }
    pageInfo {
      endCursor
      hasNextPage
    }
  }
}
"#,
    ),
    (
        "GalleriesDetailPaginationContainerQuery",
        r#"query GalleriesDetailPaginationContainerQuery($ownerLegacyId: String, $slug: String, $token: String, $pageSize: Int, $cursor: String) {
  galleryByOwnerIdAndSlugOrToken(ownerLegacyId: $ownerLegacyId, slug: $slug, token: $token) {
    ...GalleriesDetailPaginationContainer_gallery_3e6UuE
    id
  }
}

fragment GalleriesDetailPaginationContainer_gallery_3e6UuE on Gallery {
  id
  legacyId
  name
  privacy
  notSafeForWork
  ownPhotosOnly
  canonicalPath
  publicSlug
  lastPublishedAt
  photosAddedSinceLastPublished
  reportStatus
  creator {
    legacyId
    id
  }
  cover {
    images(sizes: [33, 32, 36, 2048]) {
      url
      size
      webpUrl
      id
    }
    id
  }
  description
  externalUrl
  buttonName
  photos(first: $pageSize, after: $cursor) {
    totalCount
    edges {
      cursor
      node {
        id
        legacyId
        canonicalPath
        name
        description
        category
        uploadedAt
        location
        width
        height
        isLikedByMe
        photographer: uploader {
          id
          legacyId
          username
          displayName
          canonicalPath
          avatar {
            images(sizes: SMALL) {
              url
              id
            }
            id
          }
          followedByUsers {
            totalCount
            isFollowedByMe
          }
        }
        images(sizes: [33, 32]) {
          size
          url
          webpUrl
          id
        }
        __typename
      }
    }
    pageInfo {
      endCursor
      hasNextPage
    }
  }
}
"#,
    ),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http, json, keywords, media, serve};
    use futures_util::StreamExt;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://500px.com/alice"),
            Some(Target::User("alice".into()))
        );
        assert_eq!(
            t("https://web.500px.com/p/alice/?view=photos"),
            Some(Target::User("alice".into()))
        );
        assert_eq!(t("https://500px.com/p"), Some(Target::User("p".into())));
        assert_eq!(
            t("https://500px.com/p/alice/galleries/trips"),
            Some(Target::Gallery("alice".into(), "trips".into()))
        );
        assert_eq!(
            t("https://500px.com/alice/galleries/trips/x"),
            Some(Target::Gallery("alice".into(), "trips".into()))
        );
        assert_eq!(
            t("https://500px.com/photo/12345/Title"),
            Some(Target::Image("12345".into()))
        );
        for raw in [
            "https://500px.com/",
            "https://500px.com/liked",
            "https://500px.com/likedbob",
            "https://500px.com/alice/photos",
            "https://500px.com/photo/abc",
            "https://www.500px.com/alice",
            "https://500px.com.evil.com/alice",
            "https://evil500px.com/alice",
            "https://500px.com:444/alice",
            "https://u:p@500px.com/alice",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn photo(id: u64) -> Value {
        json!({"id": id, "name": format!("Shot {id}"), "image_format": "jpeg",
            "created_at": "2020-01-02T03:04:05-05:00",
            "user": {"username": "alice", "id": 9},
            "images": [{"size": 1, "url": "https://drscdn.500px.org/photo/1/m%3D900/small"},
                       {"size": 4096, "url": format!("https://drscdn.500px.org/photo/{id}/m%3D4096/v2")}]})
    }

    fn photos(ids: &[u64]) -> Value {
        let map: Map<String, Value> = ids.iter().map(|id| (id.to_string(), photo(*id))).collect();
        json!({"photos": map})
    }

    fn edges(ids: &[u64], next: Option<&str>) -> Value {
        json!({"edges": ids.iter().map(|id| json!({"node": {"legacyId": id}})).collect::<Vec<_>>(),
            "pageInfo": {"hasNextPage": next.is_some(), "endCursor": next}})
    }

    #[tokio::test]
    async fn user_photostream_paginates_through_graphql() {
        let (root, server) = serve(vec![
            json(json!({"data": {"user": {"photos": edges(&[1, 2], Some("C1"))}}})),
            json(photos(&[2, 1])),
            json(json!({"data": {"userByUsername": {"photos": edges(&[3], None)}}})),
            json(photos(&[3])),
        ]);
        let http = http("500px", root);
        let files = media(extract(
            &http,
            Url::parse("https://500px.com/p/alice").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 3);
        assert_eq!(keywords(&files[0])["id"], 1);
        let k = keywords(&files[0]);
        assert_eq!(k["extension"], "jpeg");
        assert_eq!(
            files[0].metadata.as_ref().unwrap().timestamp,
            Some(1577952245)
        );
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "1_Shot 1.jpeg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("500px/alice")
        );
        let (_, identity) = crate::sites::archive_key(&files[2]).unwrap();
        assert_eq!(identity, r#"["{id}",["3"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("POST /graphql "));
        assert!(requests[0].contains(r#""operationName":"OtherPhotosQuery""#));
        assert!(requests[0].contains(r#""variables":"{\"username\":\"alice\",\"pageSize\":20}""#));
        assert!(requests[1].starts_with("GET /v1/photos?expanded_user_info=true&"));
        assert!(requests[1].contains("&image_size=4096&ids=1%2C2 "));
        assert!(requests[1].contains("origin: https://500px.com"));
        assert!(requests[2].contains(r#"\"cursor\":\"C1\""#));
        assert!(requests[2].contains("OtherPhotosPaginationContainerQuery"));
    }

    #[tokio::test]
    async fn gallery_merges_user_and_gallery() {
        let (root, server) = serve(vec![
            json(json!({"data": {"profile": {"legacyId": 9, "username": "Alice"}}})),
            json(json!({"data": {"gallery": {"name": "Trips", "legacyId": 4,
                "photos": edges(&[5], None)}}})),
            json(photos(&[5])),
        ]);
        let http = http("500px", root);
        let files = media(extract(
            &http,
            Url::parse("https://500px.com/alice/galleries/trips").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let k = keywords(&files[0]);
        assert_eq!(k["gallery"]["name"], "Trips");
        assert!(k["gallery"].get("photos").is_none());
        assert_eq!(k["user"]["username"], "Alice");
        let (_, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("500px/Alice/Trips")
        );
        let requests = server.join().unwrap();
        assert!(requests[1].contains(r#"\"galleryOwnerLegacyId\":\"9\""#));
        assert!(requests[1].contains(r#"\"slug\":\"trips\""#));
    }

    #[tokio::test]
    async fn image_requests_single_photo() {
        let (root, server) = serve(vec![json(photos(&[7]))]);
        let http = http("500px", root);
        let files = media(extract(
            &http,
            Url::parse("https://500px.com/photo/7/title").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].url.as_str(),
            "https://drscdn.500px.org/photo/7/m%3D4096/v2"
        );
        assert_eq!(keywords(&files[0])["subcategory"], "image");
        let requests = server.join().unwrap();
        assert!(requests[0].contains("&ids=7 "));
    }

    #[tokio::test]
    async fn unknown_user_is_an_error() {
        let (root, server) = serve(vec![json(json!({"data": {"user": null}}))]);
        let http = http("500px", root);
        let events = extract(&http, Url::parse("https://500px.com/nobody").unwrap());
        let results: Vec<_> = crate::event::media_only(events).collect().await;
        assert!(results.iter().any(Result::is_err));
        server.join().unwrap();
    }
}
