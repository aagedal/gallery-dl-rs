//! LeakGallery profiles, trending, most-liked and posts, mirroring
//! gallery_dl/extractor/leakgallery.py.
use crate::error;
use crate::sites::gdl::{captures, group, nameext_into, re, str_of};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use std::collections::HashSet;
use url::Url;

const FILENAME: &str = "{id}_{filename}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{creator}"];

static USER: Route = Route::new("user", FILENAME, DIRECTORY, "{creator}_{id}");
static TRENDING: Route = Route::new("trending", FILENAME, DIRECTORY, "{creator}_{id}");
static MOSTLIKED: Route = Route::new("mostliked", FILENAME, DIRECTORY, "{creator}_{id}");
static POST: Route = Route::new("post", FILENAME, DIRECTORY, "{creator}_{id}");

pub static SITE: Site = Site {
    category: "leakgallery",
    name: "LeakGallery",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&USER, &TRENDING, &MOSTLIKED, &POST],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    User(String, String, String),
    Trending(String),
    MostLiked,
    Post(String, String),
}

fn target(url: &Url) -> Option<Target> {
    const BASE: &str = r"^https?://(?:www\.)?leakgallery\.com";
    if let Some(c) = captures(
        re!(&format!(
            r"{BASE}/([^/?#]+)(?:/(Photos|Videos|All))?(?:/(MostRecent|MostViewed|MostLiked))?/?$"
        )),
        url,
    ) {
        let creator = group(&c, 1);
        if !["trending-medias", "most-liked", "random"]
            .iter()
            .any(|p| creator.starts_with(p))
        {
            let kind = c.get(2).map_or("All", |m| m.as_str());
            let sort = c.get(3).map_or("MostRecent", |m| m.as_str());
            return Some(Target::User(creator.into(), kind.into(), sort.into()));
        }
    }
    if let Some(c) = captures(re!(&format!(r"{BASE}/trending-medias(?:/([\w-]+))?")), url) {
        return Some(Target::Trending(
            c.get(1).map_or("Last-Hour", |m| m.as_str()).into(),
        ));
    }
    if captures(re!(&format!(r"{BASE}/most-liked")), url).is_some() {
        return Some(Target::MostLiked);
    }
    captures(re!(&format!(r"{BASE}/([^/?#]+)/(\d+)")), url)
        .map(|c| Target::Post(group(&c, 1).into(), group(&c, 2).into()))
}

/// `_yield_media_items`: unique files, named after their creator.
fn media_items(medias: &[Value], creator: Option<&str>, seen: &mut HashSet<String>) -> Vec<Item> {
    medias
        .iter()
        .filter_map(|media| {
            let path = str_of(media, "file_path").to_owned();
            if !seen.insert(path.clone()) {
                return None;
            }
            let mut fields = media.as_object()?.clone();
            let creator = match creator {
                Some(creator) => creator.to_owned(),
                None => media["profile"]["username"]
                    .as_str()
                    .filter(|u| !u.is_empty())
                    .unwrap_or("unknown")
                    .to_owned(),
            };
            fields.insert("creator".into(), creator.into());
            let url = format!("https://cdn.leakgallery.com/{path}");
            fields.insert("url".into(), url.as_str().into());
            nameext_into(&url, &mut fields);
            let id = fields
                .get("id")
                .map(crate::sites::scalar)
                .unwrap_or_default();
            Some(Item::new(Url::parse(&url).ok()?, fields).ids(id.clone(), id))
        })
        .collect()
}

/// `_pagination`: numbered API pages until an empty page (errors end it).
fn pages<'a>(
    http: &'a Http,
    route: &'static Route,
    base: String,
    params: Vec<(String, String)>,
    creator: Option<String>,
) -> Events<'a> {
    crate::sites::paginate(Some((1u64, HashSet::<String>::new())), move |state| {
        let base = base.clone();
        let params = params.clone();
        let creator = creator.clone();
        async move {
            let Some((number, mut seen)) = state else {
                return Ok(None);
            };
            let mut url = http.api(&format!("{base}{number}"))?;
            if !params.is_empty() {
                url.query_pairs_mut().extend_pairs(params.iter());
            }
            let Ok(data) = http.json(&url).await else {
                return Ok(None);
            };
            let medias = match &data {
                Value::Object(map) if map.contains_key("medias") => {
                    map["medias"].as_array().cloned().unwrap_or_default()
                }
                Value::Array(list) => list.clone(),
                _ => Vec::new(),
            };
            if medias.is_empty() {
                return Ok(None);
            }
            let items = media_items(&medias, creator.as_deref(), &mut seen);
            let events = items
                .into_iter()
                .flat_map(|item| crate::sites::post(&SITE, http, route, vec![item]))
                .collect();
            Ok(Some((
                crate::sites::events(events),
                Some((number + 1, seen)),
            )))
        }
    })
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported LeakGallery URL"));
    };
    match target {
        Target::User(creator, kind, sort) => pages(
            http,
            &USER,
            format!("https://api.leakgallery.com/profile/{creator}/"),
            vec![("type".into(), kind), ("sort".into(), sort)],
            Some(creator),
        ),
        Target::Trending(period) => pages(
            http,
            &TRENDING,
            format!("https://api.leakgallery.com/popular/media/{period}/"),
            Vec::new(),
            None,
        ),
        Target::MostLiked => pages(
            http,
            &MOSTLIKED,
            "https://api.leakgallery.com/most-liked/".into(),
            Vec::new(),
            None,
        ),
        Target::Post(creator, id) => crate::sites::once(async move {
            let page = http
                .text(&http.api(&format!("https://leakgallery.com/{creator}/{id}"))?)
                .await?;
            // Files may sit in per-creator directories below `content*/`.
            let videos = re!(
                r#"https://cdn\.leakgallery\.com/content[^/?#]*/(?:[\w.-]+/)*(?:compressed_)?watermark_[^"]+\.(?:mp4|mov|m4a|webm)"#
            );
            let images = re!(
                r#"https://cdn\.leakgallery\.com/content[^/?#]*/(?:[\w.-]+/)*watermark_[^"]+\.(?:jpe?g|png)"#
            );
            let mut seen = HashSet::new();
            let items: Vec<Item> = videos
                .find_iter(&page)
                .chain(images.find_iter(&page))
                .map(|m| m.as_str().to_owned())
                .filter(|url| seen.insert(url.clone()))
                .filter_map(|url| {
                    let mut data = Map::new();
                    data.insert("id".into(), id.as_str().into());
                    data.insert("creator".into(), creator.as_str().into());
                    data.insert("url".into(), url.as_str().into());
                    nameext_into(&url, &mut data);
                    Some(Item::new(Url::parse(&url).ok()?, data).ids(id.clone(), id.clone()))
                })
                .collect();
            let events = items
                .into_iter()
                .flat_map(|item| crate::sites::post(&SITE, http, &POST, vec![item]))
                .collect();
            Ok(crate::sites::events(events))
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http as client, json, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://leakgallery.com/sophieraiin"),
            Some(Target::User(
                "sophieraiin".into(),
                "All".into(),
                "MostRecent".into()
            ))
        );
        assert_eq!(
            t("https://leakgallery.com/sophieraiin/Videos/MostLiked"),
            Some(Target::User(
                "sophieraiin".into(),
                "Videos".into(),
                "MostLiked".into()
            ))
        );
        assert_eq!(
            t("https://leakgallery.com/sophieraiin/12240"),
            Some(Target::Post("sophieraiin".into(), "12240".into()))
        );
        assert_eq!(
            t("https://leakgallery.com/trending-medias/Week"),
            Some(Target::Trending("Week".into()))
        );
        assert_eq!(
            t("https://leakgallery.com/most-liked"),
            Some(Target::MostLiked)
        );
    }

    #[tokio::test]
    async fn post_media_in_creator_directories() {
        let page = r#"<video src="https://cdn.leakgallery.com/content-videos/sophieraiin/watermark_745_sophieraiin_241.mp4"></video>
            <img src="https://cdn.leakgallery.com/content/profile_pic/alysa_profile_pic.webp">"#;
        let (root, _server) = serve(vec![html(page)]);
        let http = client("leakgallery", root);
        let files = media(extract(
            &http,
            Url::parse("https://leakgallery.com/sophieraiin/12240").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert!(
            files[0]
                .url
                .as_str()
                .ends_with("/sophieraiin/watermark_745_sophieraiin_241.mp4")
        );
    }

    #[tokio::test]
    async fn profile_pages_until_empty() {
        let (root, server) = serve(vec![
            json(
                json!({"medias": [{"id": 1, "file_path": "content/a.jpg"}, {"id": 2, "file_path": "content/a.jpg"}]}),
            ),
            json(json!({"medias": []})),
        ]);
        let http = client("leakgallery", root);
        let files = media(extract(
            &http,
            Url::parse("https://leakgallery.com/sophieraiin").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].url.as_str(),
            "https://cdn.leakgallery.com/content/a.jpg"
        );
        assert_eq!(keywords(&files[0])["creator"], "sophieraiin");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /profile/sophieraiin/1?type=All&sort=MostRecent "));
    }

    #[tokio::test]
    async fn post_page_lists_videos_first() {
        let page = r#""https://cdn.leakgallery.com/content1/watermark_b.jpg" "https://cdn.leakgallery.com/content1/compressed_watermark_a.mp4""#;
        let (root, server) = serve(vec![html(page)]);
        let http = client("leakgallery", root);
        let files = media(extract(
            &http,
            Url::parse("https://leakgallery.com/sophieraiin/12240").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert!(files[0].url.as_str().ends_with(".mp4"));
        server.join().unwrap();
    }
}
