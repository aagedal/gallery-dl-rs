//! 35photo.pro users, tags, genres and single photos, mirroring
//! gallery_dl/extractor/35photo.py: listings yield photo IDs (HTML pages or
//! the `show_block.php` feed) and each ID is resolved through
//! `api.35photo.pro` `photo.getData`.
use crate::sites::art::{extr, extract_iter, nameext_into, parse_int, truthy};
use crate::sites::{Events, Http, Item, Route, Site, scalar};
use crate::{Result, error};
use futures_util::{StreamExt, TryStreamExt, stream};
use serde_json::{Map, Value};
use url::Url;

pub static SITE: Site = Site {
    category: "35photo",
    name: "35PHOTO",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&USER, &TAG, &GENRE, &IMAGE],
    api_key: false,
};

const ROOT: &str = "https://35photo.pro";
const FILENAME: &str = "{id}{title:?_//}_{num:>02}.{extension}";
static USER: Route = Route::new("user", FILENAME, &["{category}", "{user}"], "{id}_{num}");
static TAG: Route = Route::new(
    "tag",
    FILENAME,
    &["{category}", "Tags", "{search_tag}"],
    "t{search_tag}_{id}_{num}",
);
static GENRE: Route = Route::new(
    "genre",
    FILENAME,
    &["{category}", "Genre", "{genre}"],
    "g{genre_id}_{id}_{num}",
);
static IMAGE: Route = Route::new("image", FILENAME, &["{category}", "{user}"], "{id}_{num}");

#[derive(Debug, PartialEq)]
enum Target {
    User(String),
    Tag(String),
    Genre { id: String, new: bool },
    Image(String),
}

/// Leading ASCII digits, as an unanchored `(\d+)`.
fn leading_digits(text: &str) -> Option<&str> {
    let end = text.bytes().take_while(u8::is_ascii_digit).count();
    (1..=20).contains(&end).then(|| &text[..end])
}

fn target(url: &Url) -> Option<Target> {
    // `(?:[a-z]+\.)?35photo\.pro`
    let host = url.host_str()?;
    let sub_ok = match host.strip_suffix("35photo.pro") {
        Some("") => true,
        Some(prefix) => prefix
            .strip_suffix('.')
            .is_some_and(|l| !l.is_empty() && l.len() <= 63 && l.bytes().all(|b| b.is_ascii_lowercase())),
        None => false,
    };
    if !crate::sites::plain(url) || !sub_ok {
        return None;
    }
    let path = url.path().strip_prefix('/')?;
    let first = path.split('/').next().unwrap_or_default();
    if let Some(rest) = path.strip_prefix("tags/") {
        let tag = rest.split('/').next().unwrap_or_default();
        return crate::sites::art::segment(tag).then(|| Target::Tag(tag.into()));
    }
    if let Some(rest) = first.strip_prefix("photo_") {
        return leading_digits(rest).map(|id| Target::Image(id.into()));
    }
    if let Some(rest) = path.strip_prefix("genre_") {
        let id = leading_digits(rest)?;
        let new = rest[id.len()..].starts_with("/new/");
        return Some(Target::Genre { id: id.into(), new });
    }
    if path.starts_with("rating/") || first.is_empty() {
        return None;
    }
    Some(Target::User(first.into()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Image(id)) => photos(
            http,
            &IMAGE,
            Map::new(),
            stream::iter([Ok(id)]).boxed(),
        ),
        Some(Target::Tag(tag)) => {
            let mut data = Map::new();
            let search = crate::sites::percent_decode(&tag).to_lowercase();
            data.insert("search_tag".into(), search.into());
            photos(http, &TAG, data, tag_ids(http, tag))
        }
        Some(Target::User(user)) => crate::sites::once(async move {
            let page = http
                .text(&http.api(&format!("{ROOT}/{user}/"))?)
                .await?;
            let user_id = parse_int(extr(&page, "/user_", ".xml"), 0);
            let mut data = Map::new();
            data.insert("user".into(), user.into());
            data.insert("user_id".into(), user_id.into());
            let params = vec![
                ("page", "photoUser".to_owned()),
                ("user_id", user_id.to_string()),
                ("type", "getNextPageData".to_owned()),
                ("lastId", "999999999".to_owned()),
            ];
            Ok(photos(http, &USER, data, show_block(http, params, Vec::new())))
        }),
        Some(Target::Genre { id, new }) => crate::sites::once(async move {
            let suffix = if new { "/new/" } else { "/" };
            let page = http
                .text(&http.api(&format!("{ROOT}/genre_{id}{suffix}"))?)
                .await?;
            let ids = photo_ids(extr(&page, " class=\"photo", "\n"));
            let mut data = Map::new();
            data.insert("genre".into(), extr(&page, " genre - ", ". ").into());
            data.insert("genre_id".into(), parse_int(&id, 0).into());
            let Some(last) = ids.last().cloned() else {
                return Ok(crate::sites::events(Vec::new()));
            };
            let params = vec![
                ("page", "genre".to_owned()),
                ("community_id", id),
                ("photo_rating", if new { "0" } else { "50" }.to_owned()),
                ("lastId", last),
                ("type", "getNextPageData".to_owned()),
            ];
            Ok(photos(http, &GENRE, data, show_block(http, params, ids)))
        }),
        None => crate::sites::fail(error("unsupported 35photo URL")),
    }
}

type Ids<'a> = futures_util::stream::BoxStream<'a, Result<String>>;

/// Upstream `_photo_ids`: unique `/photo_ID/` values, numerically descending.
fn photo_ids(page: &str) -> Vec<String> {
    let mut ids: Vec<String> = extract_iter(page, "/photo_", "/")
        .into_iter()
        .map(str::to_owned)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    ids.sort_by_key(|id| std::cmp::Reverse(parse_int(id, 0)));
    ids
}

/// Upstream `_pagination` over `show_block.php`, after `extra` IDs.
fn show_block<'a>(http: &'a Http, params: Vec<(&'static str, String)>, extra: Vec<String>) -> Ids<'a> {
    let first = stream::iter(extra.into_iter().map(Ok));
    let rest = stream::try_unfold(Some(params), move |params| async move {
        let Some(mut params) = params else {
            return Ok::<_, crate::Error>(None);
        };
        let mut url = http.api(&format!("{ROOT}/show_block.php"))?;
        url.query_pairs_mut().extend_pairs(params.iter().map(|(k, v)| (*k, v.as_str())));
        let headers = [("Referer", ROOT), ("X-Requested-With", "XMLHttpRequest")];
        let data = http.json_with(&url, &headers).await?;
        let ids = photo_ids(data.get("data").and_then(Value::as_str).unwrap_or_default());
        let last = data.get("lastId");
        let next = truthy(last).then(|| {
            let value = last.map(scalar).unwrap_or_default();
            for (key, v) in &mut params {
                if *key == "lastId" {
                    *v = value.clone();
                }
            }
            params
        });
        Ok(Some((stream::iter(ids.into_iter().map(Ok)), next)))
    })
    .try_flatten();
    first.chain(rest).boxed()
}

/// Upstream tag `photos()`: `list_N` pages until one has no photo links.
fn tag_ids(http: &Http, tag: String) -> Ids<'_> {
    stream::try_unfold(Some(1u64), move |num| {
        let tag = tag.clone();
        async move {
            let Some(num) = num else {
                return Ok::<_, crate::Error>(None);
            };
            let url = http.api(&format!("{ROOT}/tags/{tag}/list_{num}/"))?;
            let page = http.text(&url).await?;
            let mut ids: Vec<String> = Vec::new();
            for id in extract_iter(&page, "35photo.pro/photo_", "/") {
                if ids.last().is_none_or(|prev| prev != id) {
                    ids.push(id.to_owned());
                }
            }
            let next = (!ids.is_empty() && num < 100_000).then_some(num + 1);
            Ok(Some((stream::iter(ids.into_iter().map(Ok)), next)))
        }
    })
    .try_flatten()
    .boxed()
}

/// Resolve each photo ID lazily into one post of its files.
fn photos<'a>(
    http: &'a Http,
    route: &'static Route,
    data: Map<String, Value>,
    ids: Ids<'a>,
) -> Events<'a> {
    ids.map(move |id| {
        let data = data.clone();
        match id {
            Ok(id) => crate::sites::once(async move {
                let mut url = http.api("https://api.35photo.pro/")?;
                url.query_pairs_mut()
                    .append_pair("method", "photo.getData")
                    .append_pair("photoId", &id);
                let body = http.json(&url).await?;
                let photo = body
                    .get("data")
                    .and_then(|d| d.get(&id))
                    .ok_or_else(|| error("35photo photo data is missing"))?;
                let items = parse_photo(photo, &data)?;
                Ok(crate::sites::events(crate::sites::post(
                    &SITE, http, route, items,
                )))
            }),
            Err(e) => crate::sites::fail(e),
        }
    })
    .flatten()
    .boxed()
}

/// Upstream `_photo_data` with the listing metadata merged in.
fn parse_photo(data: &Value, extra: &Map<String, Value>) -> Result<Vec<Item>> {
    let get = |key: &str| {
        data.get(key)
            .cloned()
            .ok_or_else(|| error("35photo photo data is incomplete"))
    };
    let mut info = Map::new();
    info.insert("url".into(), get("src")?);
    info.insert("id".into(), get("photo_id")?);
    info.insert("title".into(), get("photo_name")?);
    info.insert("description".into(), get("photo_desc")?);
    let tags = get("tags")?;
    info.insert(
        "tags".into(),
        if truthy(Some(&tags)) { tags } else { Value::Array(Vec::new()) },
    );
    info.insert("views".into(), get("photo_see")?);
    info.insert("favorites".into(), get("photo_fav")?);
    info.insert("score".into(), get("photo_rating")?);
    info.insert("type".into(), get("photo_type")?);
    info.insert("date".into(), get("timeAdd")?);
    info.insert("user".into(), get("user_login")?);
    info.insert("user_id".into(), get("user_id")?);
    info.insert("user_name".into(), get("user_name")?);
    let mut files = Vec::new();
    match data.get("series").and_then(Value::as_array) {
        Some(series) => {
            for (index, photo) in series.iter().enumerate() {
                let mut file = info.clone();
                file.insert("num".into(), (index + 1).into());
                file.insert("url".into(), photo.get("src").cloned().unwrap_or(Value::Null));
                let id = photo.get("id").map(scalar).unwrap_or_default();
                file.insert("id_series".into(), parse_int(&id, 0).into());
                let title = photo.get("title").filter(|t| truthy(Some(t)));
                file.insert("title_series".into(), title.cloned().unwrap_or_else(|| "".into()));
                files.push(file);
            }
        }
        None => {
            info.insert("num".into(), 1.into());
            files.push(info);
        }
    }
    let mut items = Vec::new();
    for mut file in files {
        crate::sites::art::update(&mut file, extra);
        let raw = file.get("url").and_then(Value::as_str).unwrap_or_default().to_owned();
        let Some(url) = crate::sites::art::file_url(&raw, ROOT) else {
            continue;
        };
        nameext_into(&raw, &mut file);
        let id = file.get("id").map(scalar).unwrap_or_default();
        let num = file.get("num").map(scalar).unwrap_or_default();
        let user = file.get("user").map(scalar);
        let title = file.get("title").and_then(Value::as_str).map(str::to_owned);
        items.push(
            Item::new(url, file)
                .ids(id.clone(), format!("{id}_{num}"))
                .user(user)
                .caption(title),
        );
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, json, keywords, media, serve};
    use serde_json::json;

    fn t(raw: &str) -> Option<Target> {
        target(&Url::parse(raw).unwrap())
    }

    #[test]
    fn classifies_urls() {
        assert_eq!(t("https://35photo.pro/liya"), Some(Target::User("liya".into())));
        assert_eq!(t("https://en.35photo.pro/liya/x"), Some(Target::User("liya".into())));
        assert_eq!(
            t("https://35photo.pro/tags/landscape/"),
            Some(Target::Tag("landscape".into()))
        );
        assert_eq!(
            t("https://35photo.pro/genre_109/"),
            Some(Target::Genre { id: "109".into(), new: false })
        );
        assert_eq!(
            t("https://35photo.pro/genre_109/new/"),
            Some(Target::Genre { id: "109".into(), new: true })
        );
        assert_eq!(
            t("https://35photo.pro/photo_753340/"),
            Some(Target::Image("753340".into()))
        );
        for raw in [
            "https://35photo.pro/",
            "https://35photo.pro/rating/x",
            "https://35photo.pro/photo_x/",
            "https://35photo.pro/genre_/",
            "https://EN1.35photo.pro/liya",
            "https://a.b.35photo.pro/liya",
            "https://evil35photo.pro/liya",
            "https://35photo.pro.evil.com/liya",
            "https://35photo.pro:8080/liya",
            "https://u:p@35photo.pro/liya",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[test]
    fn photo_ids_are_unique_and_descending() {
        let page = r#"<a href="/photo_5/"><a href="/photo_12/"><a href="/photo_5/">"#;
        assert_eq!(photo_ids(page), ["12", "5"]);
        assert!(photo_ids("").is_empty());
    }

    fn photo(id: u64, series: bool) -> Value {
        let mut data = json!({
            "src": format!("https://35photo.pro/photos_main/1/{id}.jpg"),
            "photo_id": id, "photo_name": "Winter walk", "photo_desc": "d",
            "tags": null, "photo_see": 10, "photo_fav": 2, "photo_rating": 3,
            "photo_type": 0, "timeAdd": "15 авг, 2014", "user_login": "liya",
            "user_id": 20415, "user_name": "Liya Mirzaeva",
        });
        if series {
            data["series"] = json!([
                {"src": "https://35photo.pro/photos_series/1/a.jpg", "id": "7", "title": null},
                {"src": "https://35photo.pro/photos_series/1/b.jpg", "id": 8, "title": "B"},
            ]);
        }
        json!({"data": {id.to_string(): data}})
    }

    #[tokio::test]
    async fn extracts_image_and_series() {
        let (root, server) = serve(vec![json(photo(753340, true))]);
        let http = http("35photo", root);
        let files = media(extract(
            &http,
            Url::parse("https://35photo.pro/photo_753340/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[1]);
        assert_eq!(k["id"], 753340);
        assert_eq!(k["id_series"], 8);
        assert_eq!(k["title_series"], "B");
        assert_eq!(keywords(&files[0])["title_series"], "");
        assert_eq!(k["tags"], json!([]));
        assert_eq!(k["date"], "15 авг, 2014");
        assert_eq!(k["num"], 2);
        assert_eq!(k["filename"], "b");
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "753340_Winter walk_02.jpg");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("35photo/liya")
        );
        let (_, identity) = crate::sites::archive_key(&files[1]).unwrap();
        assert_eq!(identity, r#"["{id}_{num}",["753340","2"]]"#);
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with("GET /?method=photo.getData&photoId=753340 "),
            "{}",
            requests[0]
        );
    }

    #[tokio::test]
    async fn paginates_user_feed() {
        let (root, server) = serve(vec![
            html(r#"<link href="https://35photo.pro/rss/user_20415.xml">"#),
            json(json!({"data": "<a href=\"/photo_2/\"><a href=\"/photo_3/\">", "lastId": 2})),
            json(photo(3, false)),
            json(photo(2, false)),
            json(json!({"data": "", "lastId": ""})),
        ]);
        let http = http("35photo", root);
        let files = media(extract(&http, Url::parse("https://35photo.pro/liya").unwrap())).await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[0]);
        assert_eq!(k["user_id"], 20415);
        assert_eq!(k["id"], 3);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /liya/ "));
        assert!(
            requests[1].starts_with(
                "GET /show_block.php?page=photoUser&user_id=20415&type=getNextPageData&lastId=999999999 "
            ),
            "{}",
            requests[1]
        );
        assert!(requests[1].to_ascii_lowercase().contains("x-requested-with: xmlhttprequest"));
        assert!(requests[4].contains("lastId=2 "), "{}", requests[4]);
    }

    #[tokio::test]
    async fn tag_pages_until_empty() {
        let (root, server) = serve(vec![
            html(r#"<a href="https://35photo.pro/photo_9/"><a href="https://35photo.pro/photo_9/">"#),
            json(photo(9, false)),
            html("<html></html>"),
        ]);
        let http = http("35photo", root);
        let files = media(extract(
            &http,
            Url::parse("https://35photo.pro/tags/Land%20Scape/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(keywords(&files[0])["search_tag"], "land scape");
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["t{search_tag}_{id}_{num}",["land scape","9","1"]]"#);
        let (_, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("35photo/Tags/land scape")
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /tags/Land%20Scape/list_1/ "));
        assert!(requests[2].starts_with("GET /tags/Land%20Scape/list_2/ "));
    }

    #[tokio::test]
    async fn genre_uses_page_ids_first() {
        let (root, server) = serve(vec![
            html("<title>Photo genre - Nature. x</title><div class=\"photo\"><a href=\"/photo_4/\">\n"),
            json(photo(4, false)),
            json(json!({"data": "", "lastId": null})),
        ]);
        let http = http("35photo", root);
        let files = media(extract(&http, Url::parse("https://35photo.pro/genre_109/").unwrap())).await;
        assert_eq!(files.len(), 1);
        let k = keywords(&files[0]);
        assert_eq!(k["genre"], "Nature");
        assert_eq!(k["genre_id"], 109);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /genre_109/ "));
        assert!(
            requests[2].starts_with(
                "GET /show_block.php?page=genre&community_id=109&photo_rating=50&lastId=4&type=getNextPageData "
            ),
            "{}",
            requests[2]
        );
    }
}
