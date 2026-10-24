//! Luscious albums and album searches, mirroring
//! gallery_dl/extractor/luscious.py: both go through the site's GraphQL
//! endpoint, page by page while `has_next_page` is set.
//!
//! Session cookies from `--cookies` are sent with every request.
use crate::sites::art::{date, parse_query, parse_ts, str_of};
use crate::sites::manga::text::{parse_int, parse_int_value, partition};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Value, json};
use url::Url;

const ROOT: &str = "https://members.luscious.net";
const ALBUM_GET: &str = include_str!("luscious/AlbumGet.graphql");
const ALBUM_PICTURES: &str = include_str!("luscious/AlbumListOwnPictures.graphql");
const ALBUM_LIST: &str = include_str!("luscious/AlbumListWithPeek.graphql");

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
    /// The raw query string, if any.
    Search(Option<String>),
}

/// `(?:www\.|members\.)?luscious\.net` followed by
/// `/(?:albums|pictures/c/[^/?#]+/album)/[^/?#]+_(\d+)` or
/// `/albums/list/?(?:\?([^#]+))?`, both unanchored.
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url)
        || !matches!(
            url.host_str(),
            Some("luscious.net" | "www.luscious.net" | "members.luscious.net")
        )
    {
        return None;
    }
    let raw: Vec<&str> = url.path().split('/').skip(1).collect();
    let slug = match raw.as_slice() {
        ["albums", slug, ..] => Some(*slug),
        ["pictures", "c", name, "album", slug, ..] if !name.is_empty() => Some(*slug),
        _ => None,
    };
    if let Some(id) = slug.and_then(album_id) {
        return Some(Target::Album(id.to_owned()));
    }
    let path = url.path();
    if path.starts_with("/albums/list") {
        let query = matches!(path, "/albums/list" | "/albums/list/")
            .then(|| url.query().filter(|q| !q.is_empty()).map(str::to_owned))
            .flatten();
        return Some(Target::Search(query));
    }
    None
}

/// `[^/?#]+_(\d+)`: the digits after the last `_` that follows at least
/// one character and precedes a digit.
fn album_id(slug: &str) -> Option<&str> {
    slug.match_indices('_')
        .rev()
        .filter(|(i, _)| *i > 0)
        .find_map(|(i, _)| crate::sites::art::leading_digits(&slug[i + 1..]))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Album(id)) => album(http, id),
        Some(Target::Search(query)) => search(http, query.unwrap_or_default()),
        None => crate::sites::fail(error("unsupported Luscious URL")),
    }
}

/// `_request_graphql`: the response's `data`.
async fn graphql(http: &Http, operation: &str, query: &str, variables: Value) -> Result<Value> {
    let mut url = http.api(&format!("{ROOT}/graphql/nobatch/"))?;
    url.query_pairs_mut()
        .append_pair("operationName", operation);
    let body = json!({
        "id": 1,
        "operationName": operation,
        "query": query,
        "variables": variables,
    });
    let mut response = http
        .post_json(&url, &body, &http.browser_headers(&[]))
        .await?;
    match response.get_mut("data").map(Value::take) {
        Some(data) if data.is_object() => Ok(data),
        _ => Err(error("Luscious GraphQL query failed")),
    }
}

/// Texts or titles of a list of objects.
fn names(value: &Value, key: &str) -> Value {
    value
        .as_array()
        .map(|items| items.iter().map(|item| item[key].clone()).collect())
        .unwrap_or_else(|| Value::Array(Vec::new()))
}

/// `LusciousAlbumExtractor.metadata`.
fn album_metadata(mut album: Value) -> Result<Value> {
    if album.get("errors").is_some() || !album.is_object() {
        return Err(error("Luscious album not found"));
    }
    album["audiences"] = names(&album["audiences"], "title");
    album["genres"] = names(&album["genres"], "title");
    album["tags"] = names(&album["tags"], "text");
    album["cover"] = album["cover"]["url"].clone();
    album["content"] = album["content"]["title"].clone();
    let language = partition(str_of(&album["language"], "title"), " ")
        .0
        .to_owned();
    album["language"] = language.into();
    album["created_by"] = album["created_by"]["display_name"].clone();
    album["id"] = parse_int_value(album.get("id")).into();
    album["date"] = date(parse_ts(&album["created"]));
    Ok(album)
}

/// One picture as `LusciousAlbumExtractor.items` emits it.
fn picture(mut image: Value, album: &Value, num: usize, gif: bool) -> Option<Item> {
    image["num"] = num.into();
    image["album"] = album.clone();
    let thumbnail = image["thumbnails"][0]["url"].clone();
    if let Some(object) = image.as_object_mut() {
        object.remove("thumbnails");
    }
    image["thumbnail"] = match thumbnail {
        Value::Null => "".into(),
        other => other,
    };
    image["tags"] = names(&image["tags"], "text");
    image["date"] = date(parse_ts(&image["created"]));
    image["id"] = parse_int_value(image.get("id")).into();
    let text = |key: &str| str_of(&image, key).to_owned();
    let (first, second) = match gif {
        true => (text("url_to_original"), text("url_to_video")),
        false => (text("url_to_video"), text("url_to_original")),
    };
    let raw = [first, second, text("thumbnail")]
        .into_iter()
        .find(|url| !url.is_empty())?;
    let url = crate::sites::art::file_url(&raw, ROOT)?;
    let mut data = match image {
        Value::Object(map) => map,
        _ => return None,
    };
    crate::sites::art::nameext_into(&raw, &mut data);
    let id = crate::sites::scalar(&data["id"]);
    let album_id = crate::sites::scalar(&album["id"]);
    let title = album["title"].as_str().map(str::to_owned);
    Some(Item::new(url, data).ids(album_id, id).caption(title))
}

/// `LusciousAlbumExtractor`: the album, then its pictures page by page.
fn album(http: &Http, id: String) -> Events<'_> {
    crate::sites::once(async move {
        let data = graphql(http, "AlbumGet", ALBUM_GET, json!({"id": id})).await?;
        let album = album_metadata(data["album"]["get"].clone())?;
        let gif = http.option_bool("gif", false);
        Ok(crate::sites::paginate(Some((1, 0)), move |state| {
            let album = album.clone();
            let id = id.clone();
            async move {
                let Some((page, num)) = state else {
                    return Ok(None);
                };
                let variables = json!({"input": {
                    "filters": [{"name": "album_id", "value": id}],
                    "display": "position",
                    "page": page,
                }});
                let data = graphql(http, "AlbumListOwnPictures", ALBUM_PICTURES, variables).await?;
                let list = &data["picture"]["list"];
                let images = list["items"].as_array().cloned().unwrap_or_default();
                let count = images.len();
                let items = images
                    .into_iter()
                    .enumerate()
                    .filter_map(|(i, image)| picture(image, &album, num + i + 1, gif))
                    .collect();
                let next = (list["info"]["has_next_page"].as_bool() == Some(true)
                    && page < super::MAX_PAGES)
                    .then_some((page + 1, num + count));
                Ok(Some((
                    crate::sites::events(crate::sites::post(&SITE, http, &ALBUM, items)),
                    next,
                )))
            }
        }))
    })
}

/// `LusciousSearchExtractor`: albums matching the URL's filters, queued.
fn search(http: &Http, query: String) -> Events<'_> {
    let mut query = parse_query(&query);
    let mut take = |name: &str| {
        let index = query.iter().position(|(key, _)| key == name)?;
        Some(query.remove(index).1)
    };
    let display = take("display").unwrap_or_else(|| "date_newest".into());
    let page = take("page").map_or(1, |page| match parse_int(&page) {
        0 if page.trim() != "0" => 1,
        page => page,
    });
    let filters: Vec<Value> = query
        .into_iter()
        .map(|(name, value)| json!({"name": name, "value": value}))
        .collect();
    crate::sites::paginate(Some((page, 0)), move |state| {
        let display = display.clone();
        let filters = filters.clone();
        async move {
            let Some((page, count)) = state else {
                return Ok(None);
            };
            let variables = json!({"input": {
                "display": display,
                "filters": filters,
                "page": page,
            }});
            let data = graphql(http, "AlbumListWithPeek", ALBUM_LIST, variables).await?;
            let list = &data["album"]["list"];
            let mut children = Vec::new();
            for album in list["items"].as_array().into_iter().flatten() {
                let Value::Object(mut album) = album.clone() else {
                    continue;
                };
                let url = format!("{ROOT}{}", str_of(&Value::Object(album.clone()), "url"));
                album.insert("url".into(), url.clone().into());
                if let Ok(url) = Url::parse(&url) {
                    children.push((url, album));
                }
            }
            let next = (list["info"]["has_next_page"].as_bool() == Some(true)
                && count + 1 < super::MAX_PAGES)
                .then_some((page + 1, count + 1));
            Ok(Some((
                crate::sites::manga::queue_urls(http, &SITE, &SEARCH, children),
                next,
            )))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::manga::tests::{archive, path};
    use crate::sites::testing::{http, json as reply, keywords, media, queued, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://luscious.net/albums/some_title_12345/"),
            Some(Target::Album("12345".into()))
        );
        assert_eq!(
            t("https://members.luscious.net/pictures/c/name/album/a_b_7x/"),
            Some(Target::Album("7".into()))
        );
        assert_eq!(
            t("https://www.luscious.net/albums/list/?tagged=x&display=rating"),
            Some(Target::Search(Some("tagged=x&display=rating".into())))
        );
        assert_eq!(
            t("https://luscious.net/albums/list"),
            Some(Target::Search(None))
        );
        for raw in [
            "https://luscious.net/albums/_123/",
            "https://luscious.net/albums/title/",
            "https://luscious.net/pictures/c//album/a_1/",
            "https://old.luscious.net/albums/a_1/",
            "https://luscious.net.evil.test/albums/a_1/",
            "https://luscious.net:8443/albums/a_1/",
            "https://u:p@luscious.net/albums/a_1/",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[tokio::test]
    async fn extracts_album_pages() {
        let album = json!({"data": {"album": {"get": {
            "id": "12345", "title": "Sketches", "created": 1700000000,
            "audiences": [{"title": "Everyone"}], "genres": [{"title": "Art"}],
            "tags": [{"text": "Pencil"}], "cover": {"url": "https://cdn.test/c.jpg"},
            "content": {"title": "Original"}, "language": {"title": "English Language"},
            "created_by": {"display_name": "Uploader"}}}}});
        let pictures = |items: Value, next: bool| {
            json!({"data": {"picture": {"list": {
                "info": {"has_next_page": next}, "items": items}}}})
        };
        let first = pictures(
            json!([{"id": "1", "created": 1700000001, "tags": [{"text": "A"}],
                "thumbnails": [{"url": "https://cdn.test/t1.jpg"}],
                "url_to_original": "https://cdn.test/one.png", "url_to_video": null}]),
            true,
        );
        let second = pictures(
            json!([{"id": "2", "created": 1700000002, "tags": [], "thumbnails": [],
                "url_to_original": "https://cdn.test/two.gif",
                "url_to_video": "https://cdn.test/two.mp4"}]),
            false,
        );
        let (root, server) = serve(vec![reply(album), reply(first), reply(second)]);
        let http = http("luscious", root);
        let files = media(extract(
            &http,
            Url::parse("https://luscious.net/albums/sketches_12345/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(files[1].url.as_str(), "https://cdn.test/two.mp4");
        let k = keywords(&files[0]);
        assert_eq!(k["id"], 1);
        assert_eq!(k["num"], 1);
        assert_eq!(k["thumbnail"], "https://cdn.test/t1.jpg");
        assert_eq!(k["tags"], json!(["A"]));
        assert_eq!(k["date"], "2023-11-14 22:13:21");
        assert_eq!(k["album"]["id"], 12345);
        assert_eq!(k["album"]["language"], "English");
        assert_eq!(k["album"]["created_by"], "Uploader");
        assert_eq!(k["album"]["audiences"], json!(["Everyone"]));
        assert_eq!(k["album"]["cover"], "https://cdn.test/c.jpg");
        assert_eq!(keywords(&files[1])["num"], 2);
        assert_eq!(keywords(&files[1])["thumbnail"], "");
        assert_eq!(
            path(&files[1]),
            "luscious/12345 Sketches/luscious_12345_002.mp4"
        );
        assert_eq!(archive(&files[1]), r#"["{album[id]}_{id}",["12345","2"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("POST /graphql/nobatch/?operationName=AlbumGet "));
        assert!(requests[0].contains(r#""variables":{"id":"12345"}"#));
        assert!(requests[2].contains(r#""page":2"#));
        assert!(requests[2].contains(r#"{"name":"album_id","value":"12345"}"#));
    }

    #[tokio::test]
    async fn search_queues_albums() {
        let reply_page = json!({"data": {"album": {"list": {"info": {"has_next_page": false},
            "items": [{"id": "5", "title": "A", "url": "/albums/a_5/"}]}}}});
        let (root, server) = serve(vec![reply(reply_page)]);
        let http = http("luscious", root);
        let children = queued(extract(
            &http,
            Url::parse("https://luscious.net/albums/list/?tagged=x&page=3&display=rating").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].subcategory(), "search");
        assert_eq!(children[0].fields()["id"], "5");
        let requests = server.join().unwrap();
        assert!(requests[0].contains(
            r#""input":{"display":"rating","filters":[{"name":"tagged","value":"x"}],"page":3}"#
        ));
    }
}
