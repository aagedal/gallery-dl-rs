//! Luscious albums and album searches over its GraphQL API, mirroring
//! gallery_dl/extractor/luscious.py.
use crate::error;
use crate::sites::gdl::{captures, date, group, nameext_into, parse_int, parse_query, re, truthy};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value, json};
use url::Url;

const ROOT: &str = "https://members.luscious.net";

static ALBUM: Route = Route::new(
    "album",
    "{category}_{album[id]}_{num:>03}.{extension}",
    &["{category}", "{album[id]} {album[title]}"],
    "{album[id]}_{id}",
);
static SEARCH: Route = Route::new("search", "{filename}.{extension}", &["{category}"], "");

pub static SITE: Site = Site {
    category: "luscious",
    name: "Luscious",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&ALBUM, &SEARCH],
    api_key: false,
    cookies: Some("luscious.net"),
};

#[derive(Debug, PartialEq)]
enum Target {
    Album(String),
    Search(String),
}

fn target(url: &Url) -> Option<Target> {
    const BASE: &str = r"^https?://(?:www\.|members\.)?luscious\.net";
    if let Some(c) = captures(
        re!(&format!(
            r"{BASE}/(?:albums|pictures/c/[^/?#]+/album)/[^/?#]+_(\d+)"
        )),
        url,
    ) {
        return Some(Target::Album(group(&c, 1).into()));
    }
    captures(re!(&format!(r"{BASE}/albums/list/?(?:\?([^#]+))?")), url)
        .map(|c| Target::Search(group(&c, 1).into()))
}

/// `_request_graphql`: the response's `data`.
async fn graphql(
    http: &Http,
    operation: &str,
    query: &str,
    variables: Value,
) -> crate::Result<Value> {
    let mut url = http.api(&format!("{ROOT}/graphql/nobatch/"))?;
    url.query_pairs_mut()
        .append_pair("operationName", operation);
    let body = json!({"id": 1, "operationName": operation, "query": query, "variables": variables});
    let data = http
        .post_json(&url, &body, &[])
        .await
        .map_err(|e| error(format!("GraphQL query failed ({e})")))?;
    Ok(data["data"].clone())
}

fn titles(value: &Value, key: &str) -> Value {
    value
        .as_array()
        .into_iter()
        .flatten()
        .map(|item| item[key].clone())
        .collect::<Vec<_>>()
        .into()
}

/// `LusciousAlbumExtractor.metadata`.
fn album_fields(mut album: Value) -> Map<String, Value> {
    let audiences = titles(&album["audiences"], "title");
    let genres = titles(&album["genres"], "title");
    let tags = titles(&album["tags"], "text");
    let cover = album["cover"]["url"].clone();
    let content = album["content"]["title"].clone();
    let language = album["language"]["title"]
        .as_str()
        .map_or(Value::Null, |l| {
            l.split(' ').next().unwrap_or_default().into()
        });
    let created_by = album["created_by"]["display_name"].clone();
    let id = parse_int(&crate::sites::scalar(&album["id"]));
    let created = date(album["created"].as_i64());
    let Some(map) = album.as_object_mut() else {
        return Map::new();
    };
    for (key, value) in [
        ("audiences", audiences),
        ("genres", genres),
        ("tags", tags),
        ("cover", cover),
        ("content", content),
        ("language", language),
        ("created_by", created_by),
        ("id", id.into()),
        ("date", created),
    ] {
        map.insert(key.into(), value);
    }
    map.clone()
}

/// `LusciousAlbumExtractor.items` for one picture.
fn picture(
    mut image: Map<String, Value>,
    album: &Map<String, Value>,
    num: usize,
    gif: bool,
) -> Option<Item> {
    image.insert("num".into(), num.into());
    image.insert("album".into(), album.clone().into());
    let thumbnail = image
        .remove("thumbnails")
        .and_then(|t| t.get(0).map(|t| t["url"].clone()))
        .unwrap_or_else(|| "".into());
    image.insert("thumbnail".into(), thumbnail);
    let tags = titles(&image["tags"], "text");
    image.insert("tags".into(), tags);
    let created = date(image.get("created").and_then(Value::as_i64));
    image.insert("date".into(), created);
    let id = parse_int(
        &image
            .get("id")
            .map(crate::sites::scalar)
            .unwrap_or_default(),
    );
    image.insert("id".into(), id.into());
    let pick = |key: &str| {
        image
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    let (first, second) = if gif {
        ("url_to_original", "url_to_video")
    } else {
        ("url_to_video", "url_to_original")
    };
    let url = pick(first)
        .or_else(|| pick(second))
        .or_else(|| pick("thumbnail"))?;
    nameext_into(&url, &mut image);
    let album_id = crate::sites::scalar(&album["id"]);
    Some(Item::new(Url::parse(&url).ok()?, image).ids(album_id, id.to_string()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Luscious URL"));
    };
    match target {
        Target::Album(id) => album(http, id),
        Target::Search(query) => search(http, query),
    }
}

fn album(http: &Http, album_id: String) -> Events<'_> {
    let gif = truthy(http.option("gif"));
    crate::sites::paginate(
        Some((None::<Map<String, Value>>, 1u64, 0usize)),
        move |state| {
            let album_id = album_id.clone();
            async move {
                let Some((album, page, num)) = state else {
                    return Ok(None);
                };
                let album = match album {
                    Some(album) => album,
                    None => {
                        let data =
                            graphql(http, "AlbumGet", ALBUM_GET, json!({"id": album_id})).await?;
                        let album = data["album"]["get"].clone();
                        if album.get("errors").is_some() || !album.is_object() {
                            return Err(error("Luscious album not found"));
                        }
                        album_fields(album)
                    }
                };
                let variables = json!({"input": {"filters": [{"name": "album_id", "value": album_id}], "display": "position", "page": page}});
                let data = graphql(
                    http,
                    "AlbumListOwnPictures",
                    ALBUM_LIST_OWN_PICTURES,
                    variables,
                )
                .await?;
                let list = &data["picture"]["list"];
                let mut number = num;
                let items: Vec<Item> = list["items"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|image| {
                        number += 1;
                        picture(image.as_object()?.clone(), &album, number, gif)
                    })
                    .collect();
                let next = truthy(list["info"].get("has_next_page"))
                    .then(|| (Some(album), page + 1, number));
                let events = items
                    .into_iter()
                    .flat_map(|item| crate::sites::post(&SITE, http, &ALBUM, vec![item]))
                    .collect();
                Ok(Some((crate::sites::events(events), next)))
            }
        },
    )
}

fn search(http: &Http, query: String) -> Events<'_> {
    let mut query = parse_query(&query);
    let display = query
        .remove("display")
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| "date_newest".into());
    let page = query
        .remove("page")
        .map_or(1, |p| crate::sites::gdl::int_of(Some(&p)).max(1));
    let filters: Vec<Value> = query
        .into_iter()
        .map(|(name, value)| json!({"name": name, "value": value}))
        .collect();
    crate::sites::paginate(Some(page), move |state| {
        let display = display.clone();
        let filters = filters.clone();
        async move {
            let Some(page) = state else { return Ok(None) };
            let variables =
                json!({"input": {"display": display, "filters": filters, "page": page}});
            let data = graphql(http, "AlbumListWithPeek", ALBUM_LIST_WITH_PEEK, variables).await?;
            let list = &data["album"]["list"];
            let children = list["items"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|album| {
                    let mut album = album.as_object()?.clone();
                    let url = format!("{ROOT}{}", crate::sites::scalar(album.get("url")?));
                    album.insert("url".into(), url.as_str().into());
                    Some((Url::parse(&url).ok()?, album))
                })
                .collect();
            let next = truthy(list["info"].get("has_next_page")).then_some(page + 1);
            Ok(Some((super::queue(http, &SITE, &SEARCH, children), next)))
        }
    })
}

const ALBUM_GET: &str = r#"
query AlbumGet($id: ID!) {
    album {
        get(id: $id) {
            ... on Album {
                ...AlbumStandard
            }
            ... on MutationError {
                errors {
                    code
                    message
                }
            }
        }
    }
}

fragment AlbumStandard on Album {
    __typename
    id
    title
    labels
    description
    created
    modified
    like_status
    number_of_favorites
    rating
    status
    marked_for_deletion
    marked_for_processing
    number_of_pictures
    number_of_animated_pictures
    slug
    is_manga
    url
    download_url
    permissions
    cover {
        width
        height
        size
        url
    }
    created_by {
        id
        name
        display_name
        user_title
        avatar {
            url
            size
        }
        url
    }
    content {
        id
        title
        url
    }
    language {
        id
        title
        url
    }
    tags {
        id
        category
        text
        url
        count
    }
    genres {
        id
        title
        slug
        url
    }
    audiences {
        id
        title
        url
        url
    }
    last_viewed_picture {
        id
        position
        url
    }
}
"#;

const ALBUM_LIST_OWN_PICTURES: &str = r#"
query AlbumListOwnPictures($input: PictureListInput!) {
    picture {
        list(input: $input) {
            info {
                ...FacetCollectionInfo
            }
            items {
                ...PictureStandardWithoutAlbum
            }
        }
    }
}

fragment FacetCollectionInfo on FacetCollectionInfo {
    page
    has_next_page
    has_previous_page
    total_items
    total_pages
    items_per_page
    url_complete
    url_filters_only
}

fragment PictureStandardWithoutAlbum on Picture {
    __typename
    id
    title
    created
    like_status
    number_of_comments
    number_of_favorites
    status
    width
    height
    resolution
    aspect_ratio
    url_to_original
    url_to_video
    is_animated
    position
    tags {
        id
        category
        text
        url
    }
    permissions
    url
    thumbnails {
        width
        height
        size
        url
    }
}
"#;

const ALBUM_LIST_WITH_PEEK: &str = r#"
query AlbumListWithPeek($input: AlbumListInput!) {
    album {
        list(input: $input) {
            info {
                ...FacetCollectionInfo
            }
            items {
                ...AlbumMinimal
                peek_thumbnails {
                    width
                    height
                    size
                    url
                }
            }
        }
    }
}

fragment FacetCollectionInfo on FacetCollectionInfo {
    page
    has_next_page
    has_previous_page
    total_items
    total_pages
    items_per_page
    url_complete
    url_filters_only
}

fragment AlbumMinimal on Album {
    __typename
    id
    title
    labels
    description
    created
    modified
    number_of_favorites
    number_of_pictures
    slug
    is_manga
    url
    download_url
    cover {
        width
        height
        size
        url
    }
    content {
        id
        title
        url
    }
    language {
        id
        title
        url
    }
    tags {
        id
        category
        text
        url
        count
    }
    genres {
        id
        title
        slug
        url
    }
    audiences {
        id
        title
        url
    }
}
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{children, http as client, json as body, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://luscious.net/albums/okinami-no-koigokoro_277031/"),
            Some(Target::Album("277031".into()))
        );
        assert_eq!(
            t(
                "https://members.luscious.net/pictures/c/video_game_manga/album/okinami_277031/sorted/position/id/16528978/"
            ),
            Some(Target::Album("277031".into()))
        );
        assert_eq!(
            t("https://luscious.net/albums/list/?tagged=+full_color&display=rating"),
            Some(Target::Search("tagged=+full_color&display=rating".into()))
        );
    }

    #[tokio::test]
    async fn album_pages_pictures() {
        let album = json!({"data": {"album": {"get": {"id": "277031", "title": "Okinami", "audiences": [{"title": "Straight"}], "genres": [], "tags": [{"text": "tag"}],
            "cover": {"url": "https://c/cover.jpg"}, "content": {"title": "Hentai"}, "language": {"title": "English Language"},
            "created_by": {"display_name": "user"}, "created": 1500000000}}}});
        let pictures = |page: bool, id: i64| {
            json!({"data": {"picture": {"list": {"info": {"has_next_page": page}, "items": [{"id": id.to_string(), "tags": [],
            "created": 1500000000, "thumbnails": [{"url": "https://c/t.jpg"}], "url_to_original": format!("https://c/{id}.gif"), "url_to_video": format!("https://c/{id}.mp4")}]}}}})
        };
        let (root, server) = serve(vec![
            body(album),
            body(pictures(true, 1)),
            body(pictures(false, 2)),
        ]);
        let http = client("luscious", root);
        let files = media(extract(
            &http,
            Url::parse("https://luscious.net/albums/x_277031/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(files[1].url.as_str(), "https://c/2.mp4");
        let k = keywords(&files[1]);
        assert_eq!(k["num"], 2);
        assert_eq!(k["album"]["language"], "English");
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "luscious_277031_002.mp4");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("luscious/277031 Okinami")
        );
        let requests = server.join().unwrap();
        assert!(
            requests[1].starts_with("POST /graphql/nobatch/?operationName=AlbumListOwnPictures ")
        );
    }

    #[tokio::test]
    async fn search_queues_albums() {
        let (root, server) = serve(vec![body(
            json!({"data": {"album": {"list": {"info": {"has_next_page": false}, "items": [{"url": "/albums/a_1/"}]}}}}),
        )]);
        let http = client("luscious", root);
        let queued = children(extract(
            &http,
            Url::parse("https://luscious.net/albums/list/?tagged=x").unwrap(),
        ))
        .await;
        assert_eq!(queued.len(), 1);
        assert!(
            server.join().unwrap()[0]
                .contains("\"filters\":[{\"name\":\"tagged\",\"value\":\"x\"}]")
        );
    }
}
