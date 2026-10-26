//! Image Chest galleries and users, mirroring
//! gallery_dl/extractor/imagechest.py. With the `access-token` option,
//! galleries are read through the `api.imgchest.com` v1 API.
use crate::error;
use crate::sites::gdl::{
    captures, date, extr, group, nameext_into, parse_iso, re, str_of, unescape, unquote,
};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://imgchest.com";

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
    if let Some(c) = captures(
        re!(r"^https?://(?:www\.)?imgchest\.com/p/([A-Za-z0-9]{11})"),
        url,
    ) {
        return Some(Target::Gallery(group(&c, 1).into()));
    }
    captures(re!(r"^https?://(?:www\.)?imgchest\.com/u/([^/?#]+)"), url)
        .map(|c| Target::User(group(&c, 1).into()))
}

/// The post and its files, from the page or (with a token) the API.
async fn post(http: &Http, id: &str) -> crate::Result<(Map<String, Value>, Vec<Value>)> {
    if let Some(token) = http.option_str("access-token").filter(|t| !t.is_empty()) {
        let bearer = format!("Bearer {token}");
        let url = http.api(&format!("https://api.imgchest.com/v1/post/{id}"))?;
        let mut post = http
            .json_with(&url, &[("Authorization", &bearer)])
            .await
            .map_err(|e| {
                if crate::sites::http::refused_status(&e).is_some() {
                    error("Invalid API access token")
                } else {
                    e
                }
            })?["data"]
            .as_object()
            .cloned()
            .ok_or_else(|| error("Image Chest API returned no post"))?;
        let created = post
            .get("created")
            .map(crate::sites::scalar)
            .unwrap_or_default();
        post.insert("date".into(), date(parse_iso(&created)));
        post.insert("gallery_id".into(), id.into());
        post.remove("image_count");
        let mut files = match post.remove("images") {
            Some(Value::Array(files)) => files,
            _ => Vec::new(),
        };
        for file in &mut files {
            let created = file
                .get("created")
                .map(crate::sites::scalar)
                .unwrap_or_default();
            if let Some(file) = file.as_object_mut() {
                file.insert("date".into(), date(parse_iso(&created)));
            }
        }
        return Ok((post, files));
    }
    let page = http.text(&http.api(&format!("{ROOT}/p/{id}"))?).await?;
    let data: Option<Value> =
        serde_json::from_str(&unescape(extr(&page, "data-page=\"", "\""))).ok();
    let Some(mut post) = data.and_then(|d| d["props"]["post"].as_object().cloned()) else {
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
    let tags: Vec<Value> = post
        .get("tags")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|t| t["name"].clone())
        .collect();
    post.insert("tags".into(), tags.into());
    Ok((post, files))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Image Chest URL"));
    };
    match target {
        Target::Gallery(id) => crate::sites::once(async move {
            let (data, files) = post(http, &id).await?;
            let count = files.len();
            let items = files
                .into_iter()
                .enumerate()
                .filter_map(|(index, file)| {
                    let link = str_of(&file, "link").to_owned();
                    let url = Url::parse(&link).ok()?;
                    let mut fields = data.clone();
                    fields.insert("count".into(), count.into());
                    if let Value::Object(file) = file {
                        fields.extend(file);
                    }
                    fields.insert("num".into(), (index + 1).into());
                    if !fields.contains_key("extension") {
                        nameext_into(&link, &mut fields);
                    }
                    Some(Item::new(url, fields).ids(id.clone(), (index + 1).to_string()))
                })
                .collect();
            Ok(crate::sites::events(crate::sites::post(
                &SITE, http, &GALLERY, items,
            )))
        }),
        Target::User(user) => crate::sites::paginate(Some(1u64), move |state| {
            let user = user.clone();
            async move {
                let Some(page) = state else { return Ok(None) };
                let mut url = http.api(&format!("{ROOT}/api/posts"))?;
                url.query_pairs_mut()
                    .append_pair("page", &page.to_string())
                    .append_pair("sort", "new")
                    .append_pair("tag", "")
                    .append_pair("q", "")
                    .append_pair("username", &unquote(&user))
                    .append_pair("nsfw", "true");
                let data = http.json(&url).await?;
                let galleries = data["data"].as_array().cloned().unwrap_or_default();
                if galleries.is_empty() {
                    return Ok(None);
                }
                let children = galleries
                    .into_iter()
                    .filter_map(|g| {
                        Some((
                            Url::parse(str_of(&g, "link")).ok()?,
                            g.as_object().cloned()?,
                        ))
                    })
                    .collect::<Vec<_>>();
                let events = futures_util::StreamExt::boxed(futures_util::StreamExt::flatten(
                    futures_util::stream::iter(children.into_iter().map(move |(url, data)| {
                        crate::sites::queue(http, SITE.category, USER.subcategory, url, &data)
                    })),
                ));
                Ok(Some((events, Some(page + 1))))
            }
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{children, html, http as client, json, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://imgchest.com/p/3na7kr3by8d"),
            Some(Target::Gallery("3na7kr3by8d".into()))
        );
        assert_eq!(
            t("https://imgchest.com/u/LunarLandr"),
            Some(Target::User("LunarLandr".into()))
        );
        assert_eq!(t("https://imgchest.com/p/short"), None);
    }

    #[tokio::test]
    async fn extracts_gallery_from_page_data() {
        let data = json!({"props": {"post": {"title": "Cats & Dogs", "tags": [{"name": "cat"}],
            "files": [{"id": "a", "link": "https://cdn.imgchest.com/files/a.jpg"}, {"id": "b", "link": "https://cdn.imgchest.com/files/b.png"}]}}});
        let attr = data
            .to_string()
            .replace('&', "&amp;")
            .replace('"', "&quot;");
        let (root, server) = serve(vec![html(&format!(
            r#"<div id="app" data-page="{attr}"></div>"#
        ))]);
        let http = client("imagechest", root);
        let files = media(extract(
            &http,
            Url::parse("https://imgchest.com/p/3na7kr3by8d").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[1]);
        assert_eq!(k["tags"], json!(["cat"]));
        assert_eq!(k["id"], "b");
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(
            file.render(&files[1]).unwrap(),
            "imagechest_3na7kr3by8d_002.png"
        );
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("imagechest/3na7kr3by8d Cats & Dogs")
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn uses_api_with_access_token() {
        let (root, server) = serve(vec![json(
            json!({"data": {"title": "T", "created": "2023-01-02T03:04:05Z", "image_count": 1,
            "images": [{"id": "x", "link": "https://cdn.imgchest.com/files/x.gif", "created": "2023-01-02T03:04:06Z"}]}}),
        )]);
        let http = client("imagechest", root).with_options(Some(std::sync::Arc::new(
            json!({"access-token": "tok"}).as_object().unwrap().clone(),
        )));
        let files = media(extract(
            &http,
            Url::parse("https://imgchest.com/p/3na7kr3by8d").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(keywords(&files[0])["date"], "2023-01-02 03:04:06");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /v1/post/3na7kr3by8d "));
        assert!(
            requests[0].contains("authorization: Bearer tok")
                || requests[0].contains("Authorization: Bearer tok")
        );
    }

    #[tokio::test]
    async fn user_pages_until_empty() {
        let (root, server) = serve(vec![
            json(json!({"data": [{"link": "https://imgchest.com/p/3na7kr3by8d"}]})),
            json(json!({"data": []})),
        ]);
        let http = client("imagechest", root);
        assert_eq!(
            children(extract(
                &http,
                Url::parse("https://imgchest.com/u/Lunar").unwrap()
            ))
            .await
            .len(),
            1
        );
        let requests = server.join().unwrap();
        assert!(
            requests[0]
                .starts_with("GET /api/posts?page=1&sort=new&tag=&q=&username=Lunar&nsfw=true ")
        );
    }
}
