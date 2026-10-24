//! Image Chest galleries and user profiles, mirroring
//! gallery_dl/extractor/imagechest.py: gallery pages embed their post as
//! JSON, or the official API supplies it when `access-token` is set.
use super::{date, iso_ts, path_segment, str_of};
use crate::sites::art::{extr, nameext_into, update};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://imgchest.com";
const API: &str = "https://api.imgchest.com";
static GALLERY: Route = Route::new(
    "gallery",
    "{category}_{gallery_id}_{num:>03}.{extension}",
    &["{category}", "{gallery_id} {title}"],
    "{gallery_id}_{num}",
);
static USER: Route = Route::new("user", "{filename}.{extension}", &["{category}"], "");

pub static SITE: Site = Site {
    category: "imagechest",
    name: "Image Chest",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&GALLERY, &USER],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Gallery(String),
    User(String),
}

fn target(url: &Url) -> Option<Target> {
    if !crate::sites::art::on_host(url, &["imgchest.com"]) {
        return None;
    }
    let parts: Vec<&str> = url.path()[1..].split('/').collect();
    match parts.as_slice() {
        // `p/([A-Za-z0-9]{11})`, unanchored: longer IDs match their prefix.
        ["p", id, ..] => id
            .get(..11)
            .filter(|id| id.bytes().all(|b| b.is_ascii_alphanumeric()))
            .map(|id| Target::Gallery(id.into())),
        ["u", user, ..] if path_segment(user) => Some(Target::User((*user).into())),
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Image Chest URL"));
    };
    match target {
        Target::Gallery(id) => crate::sites::once(async move {
            let (data, files) = match http.option_str("access-token").filter(|t| !t.is_empty()) {
                Some(token) => api_post(http, &id, token).await?,
                None => {
                    let page = http.text(&http.api(&format!("{ROOT}/p/{id}"))?).await?;
                    page_post(&page, &id)?
                }
            };
            Ok(crate::sites::events(crate::sites::post(
                &SITE,
                http,
                &GALLERY,
                gallery_items(data, files),
            )))
        }),
        Target::User(user) => users_posts(http, crate::sites::percent_decode(&user)),
    }
}

/// `metadata(page)`: the post embedded in the page's `data-page` JSON.
fn page_post(page: &str, id: &str) -> Result<(Map<String, Value>, Vec<Value>)> {
    let data =
        serde_json::from_str::<Value>(&crate::sites::unescape(extr(page, "data-page=\"", "\"")))
            .ok()
            .and_then(|data| data.pointer("/props/post").cloned());
    let Some(Value::Object(mut post)) = data else {
        if page.contains("<title>Not Found</title>") {
            return Err(error("Image Chest gallery not found"));
        }
        return Ok((Map::new(), Vec::new()));
    };
    let files = match post.remove("files") {
        Some(Value::Array(files)) => files,
        _ => Vec::new(),
    };
    post.insert("gallery_id".into(), id.into());
    if let Some(Value::Array(tags)) = post.get("tags") {
        let names: Vec<Value> = tags
            .iter()
            .map(|tag| tag.get("name").cloned().unwrap_or(Value::Null))
            .collect();
        post.insert("tags".into(), names.into());
    }
    Ok((post, files))
}

/// `_metadata_api`: `/v1/post/ID` with the configured access token.
async fn api_post(http: &Http, id: &str, token: &str) -> Result<(Map<String, Value>, Vec<Value>)> {
    let auth = format!("Bearer {token}");
    let body = http
        .json_with(
            &http.api(&format!("{API}/v1/post/{id}"))?,
            &[("Authorization", &auth)],
        )
        .await
        .map_err(|e| error(format!("Image Chest API request failed: {e}")))?;
    let Some(Value::Object(mut post)) = body.get("data").cloned() else {
        return Err(error("Image Chest API response has no post"));
    };
    let created = iso_ts(str_of(&Value::Object(post.clone()), "created"));
    post.insert("date".into(), date(created));
    let mut images = match post.remove("images") {
        Some(Value::Array(images)) => images,
        _ => return Err(error("Image Chest API post has no images")),
    };
    for image in &mut images {
        if let Value::Object(image) = image {
            let created = iso_ts(image.get("created").and_then(Value::as_str).unwrap_or(""));
            image.insert("date".into(), date(created));
        }
    }
    post.insert("gallery_id".into(), id.into());
    post.remove("image_count");
    Ok((post, images))
}

/// `GalleryExtractor.items`: files numbered from one, each merged into the
/// shared post data.
fn gallery_items(mut data: Map<String, Value>, files: Vec<Value>) -> Vec<Item> {
    let limit = match data.get("count").and_then(Value::as_u64) {
        Some(count) => count as usize,
        None => {
            data.insert("count".into(), files.len().into());
            files.len()
        }
    };
    let id = data.get("gallery_id").map(crate::sites::scalar);
    let title = data.get("title").and_then(Value::as_str).map(str::to_owned);
    let mut items = Vec::new();
    for (index, file) in files.into_iter().take(limit).enumerate() {
        data.insert("num".into(), (index + 1).into());
        let raw = str_of(&file, "link").to_owned();
        let Value::Object(file) = file else {
            continue;
        };
        update(&mut data, &file);
        if !file.contains_key("extension") {
            nameext_into(&raw, &mut data);
        }
        let Some(url) = crate::sites::media_url(&raw, &["imgchest.com"]) else {
            continue;
        };
        let username = data
            .get("username")
            .and_then(Value::as_str)
            .or_else(|| data.get("user").and_then(|u| u.get("username")?.as_str()))
            .map(str::to_owned);
        let ts = iso_ts(str_of(&Value::Object(data.clone()), "created"));
        items.push(
            Item::new(url, data.clone())
                .ids(id.clone().unwrap_or_default(), (index + 1).to_string())
                .user(username)
                .caption(title.clone())
                .timestamp(ts),
        );
    }
    items
}

/// `ImagechestUserExtractor.items`: queue each gallery of `/api/posts`,
/// page by page until an empty page.
fn users_posts(http: &Http, username: String) -> Events<'_> {
    crate::sites::paginate(Some(1u64), move |page| {
        let username = username.clone();
        async move {
            let Some(page) = page else {
                return Ok(None);
            };
            let mut url = http.api(&format!("{ROOT}/api/posts"))?;
            url.query_pairs_mut().extend_pairs([
                ("page", page.to_string().as_str()),
                ("sort", "new"),
                ("tag", ""),
                ("q", ""),
                ("username", &username),
                ("nsfw", "true"),
            ]);
            let body = http.json(&url).await?;
            let galleries = match body.get("data") {
                Some(Value::Array(data)) if !data.is_empty() => data.clone(),
                _ => return Ok(None),
            };
            let mut children = Vec::new();
            for gallery in galleries {
                let Value::Object(data) = gallery else {
                    continue;
                };
                let link = data.get("link").and_then(Value::as_str).unwrap_or_default();
                if let Some(url) = super::web_url(link)
                    .filter(|url| matches!(target(url), Some(Target::Gallery(_))))
                {
                    children.push((url, data));
                }
            }
            let events = futures_util::StreamExt::boxed(futures_util::StreamExt::flatten(
                futures_util::stream::iter(children.into_iter().map(move |(child, data)| {
                    crate::sites::queue(http, "imagechest", "user", child, &data)
                })),
            ));
            Ok(Some((events, (page < 100_000).then_some(page + 1))))
        }
    })
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
            t("https://imgchest.com/p/3na7kr3by8d"),
            Some(Target::Gallery("3na7kr3by8d".into()))
        );
        assert_eq!(
            t("https://www.imgchest.com/p/3na7kr3by8dXYZ"),
            Some(Target::Gallery("3na7kr3by8d".into()))
        );
        assert_eq!(
            t("https://imgchest.com/u/Some%20One"),
            Some(Target::User("Some%20One".into()))
        );
        for raw in [
            "https://imgchest.com/p/short",
            "https://imgchest.com/p/3na7kr3by8-x",
            "https://imgchest.com/u/",
            "https://cdn.imgchest.com/p/3na7kr3by8d",
            "https://imgchest.com.evil.test/p/3na7kr3by8d",
            "https://evilimgchest.com/p/3na7kr3by8d",
            "https://imgchest.com:8080/p/3na7kr3by8d",
            "https://u:p@imgchest.com/p/3na7kr3by8d",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn page(files: Value) -> String {
        let data = json!({"props": {"post": {"id": "3na7kr3by8d", "title": "Cats & Dogs",
            "username": "chester", "tags": [{"id": 1, "name": "cats"}], "files": files}}});
        let escaped = data
            .to_string()
            .replace('&', "&amp;")
            .replace('"', "&quot;");
        format!("<div id=\"app\" data-page=\"{escaped}\"></div>")
    }

    #[tokio::test]
    async fn gallery_from_page_data() {
        let files = json!([
            {"id": "a", "description": null, "link": "https://cdn.imgchest.com/files/a1.jpg"},
            {"id": "b", "description": "two", "link": "https://cdn.imgchest.com/files/b2.png"}]);
        let (root, server) = serve(vec![html(&page(files))]);
        let http = http("imagechest", root);
        let files = media(extract(
            &http,
            Url::parse("https://imgchest.com/p/3na7kr3by8d").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[1]);
        assert_eq!(k["gallery_id"], "3na7kr3by8d");
        assert_eq!(k["tags"], json!(["cats"]));
        assert_eq!(k["count"], 2);
        assert_eq!(k["num"], 2);
        assert_eq!(k["filename"], "b2");
        assert!(!k.contains_key("files"));
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(
            file.render(&files[1]).unwrap(),
            "imagechest_3na7kr3by8d_002.png"
        );
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("imagechest/3na7kr3by8d Cats & Dogs")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{gallery_id}_{num}",["3na7kr3by8d","1"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /p/3na7kr3by8d "));
    }

    #[tokio::test]
    async fn gallery_from_api_with_access_token() {
        let (root, server) = serve(vec![json(json!({"data": {
            "id": "3na7kr3by8d", "title": "T", "created": "2023-01-02T03:04:05.000000Z",
            "image_count": 1, "images": [{"id": "x", "link": "https://cdn.imgchest.com/files/x.gif",
                "created": "2023-01-02T03:04:06.000000Z"}]}}))]);
        let http = http("imagechest", root).with_options(Some(std::sync::Arc::new(
            json!({"access-token": "secret"})
                .as_object()
                .unwrap()
                .clone(),
        )));
        let files = media(extract(
            &http,
            Url::parse("https://imgchest.com/p/3na7kr3by8d").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let k = keywords(&files[0]);
        assert_eq!(k["date"], "2023-01-02 03:04:06");
        assert!(!k.contains_key("image_count") && !k.contains_key("images"));
        assert!(!k.values().any(|v| v.to_string().contains("secret")));
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /v1/post/3na7kr3by8d "));
        assert!(requests[0].contains("authorization: Bearer secret"));
    }

    #[tokio::test]
    async fn user_queues_galleries_until_empty_page() {
        let files = json!([{"id": "a", "link": "https://cdn.imgchest.com/files/a1.jpg"}]);
        let (root, server) = serve(vec![
            json(json!({"data": [
                {"id": "3na7kr3by8d", "link": "https://imgchest.com/p/3na7kr3by8d"},
                {"id": "evil", "link": "https://evil.test/p/3na7kr3by8d"}]})),
            html(&page(files)),
            json(json!({"data": []})),
        ]);
        let http = http("imagechest", root);
        let files = media(extract(
            &http,
            Url::parse("https://imgchest.com/u/Some%20One").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(keywords(&files[0])["subcategory"], "gallery");
        let requests = server.join().unwrap();
        assert!(
            requests[0]
                .starts_with("GET /api/posts?page=1&sort=new&tag=&q=&username=Some+One&nsfw=true "),
            "{}",
            requests[0]
        );
        assert!(requests[1].starts_with("GET /p/3na7kr3by8d "));
        assert!(requests[2].starts_with("GET /api/posts?page=2&"));
    }
}
