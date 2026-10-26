//! foriio works and users, mirroring gallery_dl/extractor/foriio.py.
use crate::error;
use crate::sites::gdl::{
    captures, date, group, int_of, nameext_into, parse_iso, partition, re, str_of, truthy,
    ytdl_item,
};
use crate::sites::{Events, Http, Item, Route, Site};
use futures_util::StreamExt;
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://foriio.com";
const API: &str = "https://api.foriio.com/api";
const BASE: &str = r"^https?://(?:www\.)?fori(?:io\.com|\.io)";

static WORK: Route = Route::new(
    "work",
    "{num:>02} {file_id}.{extension}",
    &[
        "{category}",
        "{author[screen_name]} ({author[id]})",
        "{date:%Y-%m-%d} {title[b:230]} ({work_id})",
    ],
    "{work_id}/{file_id}/{num}",
);
static USER: Route = Route::new("user", "{filename}.{extension}", &["{category}"], "");

pub static SITE: Site = Site {
    category: "foriio",
    name: "foriio",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&WORK, &USER],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Work(String),
    User(String),
}

fn target(url: &Url) -> Option<Target> {
    if let Some(c) = captures(re!(&format!(r"{BASE}/works/(\d+)")), url) {
        return Some(Target::Work(group(&c, 1).into()));
    }
    let c = captures(re!(&format!(r"{BASE}/([^/?#]+)")), url)?;
    // Upstream's `(?!works/)`.
    (!url.path().starts_with("/works/")).then(|| Target::User(group(&c, 1).into()))
}

/// `_orig`: the full-size store URL for a file name.
fn orig(url: &str) -> (String, String) {
    let name = url[url.rfind('/').map_or(0, |i| i + 1)..].to_owned();
    (format!("https://foriio.imgix.net/store/{name}"), name)
}

fn option(http: &Http, key: &str, default: bool) -> bool {
    http.option(key).map_or(default, |v| truthy(Some(v)))
}

/// One work's files (and queued web articles).
fn work_events<'a>(http: &'a Http, work: Value) -> Events<'a> {
    let Some(mut work) = work.as_object().cloned() else {
        return crate::sites::fail(error("foriio returned no work"));
    };
    for key in [
        "related_works",
        "single_work_og",
        "og_image_url_for_instagram",
    ] {
        work.remove(key);
    }
    let work_id = work.remove("id").unwrap_or(Value::Null);
    work.insert("work_id".into(), work_id.clone());
    let work_id = crate::sites::scalar(&work_id);
    work.insert(
        "date".into(),
        date(parse_iso(
            work.get("published_at")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        )),
    );
    let previews = option(http, "previews", false);
    let mut items: Vec<Item> = Vec::new();
    let mut children = Vec::new();
    let item = |fields: &Map<String, Value>, url: &str, name: Option<&str>| -> Option<Item> {
        let mut fields = fields.clone();
        if let Some(name) = name {
            nameext_into(name, &mut fields);
        }
        let file_id = fields
            .get("file_id")
            .map(crate::sites::scalar)
            .unwrap_or_default();
        Some(Item::new(Url::parse(url).ok()?, fields).ids(work_id.clone(), file_id))
    };
    if let Some(files) = work.remove("images").and_then(|f| f.as_array().cloned()) {
        work.insert("count".into(), files.len().into());
        for (index, file) in files.iter().enumerate() {
            work.insert("num".into(), (index + 1).into());
            work.insert("width".into(), file["width"].clone());
            work.insert("height".into(), file["height"].clone());
            work.insert("file_id".into(), file["id"].clone());
            let (url, name) = orig(partition(str_of(&file["urls"], "list"), "?").0);
            items.extend(item(&work, &url, Some(&name)));
        }
    } else if let Some((key, flag, ext)) = [
        ("videos", "videos", "mp4"),
        ("sounds", "audio", "mp3"),
        ("web_articles", "external", ""),
    ]
    .into_iter()
    .find(|(key, ..)| work.contains_key(*key))
    {
        let files = work
            .remove(key)
            .and_then(|f| f.as_array().cloned())
            .unwrap_or_default();
        work.insert("count".into(), files.len().into());
        let enabled = option(http, flag, true);
        for (index, file) in files.iter().enumerate() {
            work.insert("num".into(), (index + 1).into());
            let file_id = if key == "videos" {
                file.get("video_id").filter(|v| truthy(Some(v))).cloned()
            } else {
                None
            };
            work.insert(
                "file_id".into(),
                file_id.unwrap_or_else(|| file["title"].clone()),
            );
            if let Some(file) = file.as_object() {
                work.extend(file.clone());
            }
            let url = str_of(file, "url");
            if enabled {
                if key == "web_articles" {
                    if let Ok(url) = Url::parse(url) {
                        children.push((url, work.clone()));
                    }
                } else {
                    let mut fields = work.clone();
                    fields.insert("extension".into(), ext.into());
                    let file_id = crate::sites::scalar(&fields["file_id"]);
                    items.extend(ytdl_item(url, fields).map(|i| i.ids(work_id.clone(), file_id)));
                }
            }
            if previews {
                let preview = str_of(
                    file,
                    if key == "web_articles" {
                        "image"
                    } else {
                        "picture_url"
                    },
                );
                let (url, name) = orig(preview);
                items.extend(item(&work, &url, Some(&name)));
            }
        }
    } else if let Some(file) = work.get("copy_writing").cloned() {
        work.insert("count".into(), 1.into());
        work.insert("num".into(), 1.into());
        work.insert("file_id".into(), file["id"].clone());
        let (url, name) = orig(str_of(&file, "image"));
        items.extend(item(&work, &url, Some(&name)));
    } else {
        return crate::sites::fail(error(format!(
            "{work_id}: Unsupported type {:?}",
            str_of(&Value::Object(work), "type")
        )));
    }
    let media = crate::sites::events(
        items
            .into_iter()
            .flat_map(|item| crate::sites::post(&SITE, http, &WORK, vec![item]))
            .collect(),
    );
    media
        .chain(crate::sites::galleries::queue(http, &SITE, &WORK, children))
        .boxed()
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported foriio URL"));
    };
    match target {
        Target::Work(id) => crate::sites::once(async move {
            let data = http
                .json(&http.api(&format!("{API}/v1/works/{id}"))?)
                .await?;
            Ok(work_events(http, data["work"].clone()))
        }),
        Target::User(user) => {
            let posts: Option<Vec<String>> = http.option("posts").map(|value| match value {
                Value::String(s) => s.split(',').map(str::to_owned).collect(),
                Value::Array(list) => list.iter().map(crate::sites::scalar).collect(),
                other => vec![crate::sites::scalar(other)],
            });
            crate::sites::paginate(Some(1i64), move |state| {
                let user = user.clone();
                let posts = posts.clone();
                async move {
                    let Some(page) = state else { return Ok(None) };
                    let mut url = http.api(&format!("{API}/v1/users/{user}/works"))?;
                    url.query_pairs_mut()
                        .append_pair("page", &page.to_string())
                        .append_pair("per_page", "20");
                    let referer = format!("{ROOT}/");
                    let data = http
                        .json_with(&url, &[("Referer", referer.as_str()), ("Origin", ROOT)])
                        .await?;
                    let children = data["works"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|work| {
                            posts
                                .as_ref()
                                .is_none_or(|posts| posts.iter().any(|p| p == str_of(work, "type")))
                        })
                        .filter_map(|work| {
                            Some((
                                Url::parse(&format!(
                                    "{ROOT}/works/{}",
                                    crate::sites::scalar(&work["id"])
                                ))
                                .ok()?,
                                work.as_object()?.clone(),
                            ))
                        })
                        .collect();
                    let meta = &data["meta"];
                    let next = (meta.is_object()
                        && int_of(meta.get("current_page")) < int_of(meta.get("total_pages")))
                    .then_some(page + 1);
                    Ok(Some((
                        crate::sites::galleries::queue(http, &SITE, &USER, children),
                        next,
                    )))
                }
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http as client, json, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.foriio.com/works/12345"),
            Some(Target::Work("12345".into()))
        );
        assert_eq!(t("https://fori.io/jane"), Some(Target::User("jane".into())));
    }

    #[tokio::test]
    async fn image_work_uses_store_urls() {
        let work = json!({"work": {"id": 7, "title": "Art", "published_at": "2023-04-05T06:07:08.000+09:00", "author": {"id": 3, "screen_name": "jane"},
            "related_works": [], "images": [{"id": 11, "width": 10, "height": 20, "urls": {"list": "https://foriio.imgix.net/store/list/abc.png?w=400"}}]}});
        let (root, server) = serve(vec![json(work)]);
        let http = client("foriio", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.foriio.com/works/7").unwrap(),
        ))
        .await;
        assert_eq!(
            files[0].url.as_str(),
            "https://foriio.imgix.net/store/abc.png"
        );
        assert!(!keywords(&files[0]).contains_key("related_works"));
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "01 11.png");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("foriio/jane (3)/2023-04-04 Art (7)")
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn video_work_hands_off_to_ytdl() {
        let work = json!({"work": {"id": 8, "title": "Clip", "published_at": "2023-04-05T06:07:08Z", "author": {"id": 3, "screen_name": "jane"},
            "videos": [{"video_id": "yt1", "title": "Clip", "url": "https://www.youtube.com/watch?v=yt1", "picture_url": "https://x/p.jpg"}]}});
        let (root, server) = serve(vec![json(work)]);
        let http = client("foriio", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.foriio.com/works/8").unwrap(),
        ))
        .await;
        assert_eq!(
            files[0].url.as_str(),
            "ytdl:https://www.youtube.com/watch?v=yt1"
        );
        assert_eq!(keywords(&files[0])["extension"], "mp4");
        server.join().unwrap();
    }
}
