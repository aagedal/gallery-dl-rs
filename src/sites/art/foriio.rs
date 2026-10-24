//! foriio portfolio works and user listings, mirroring
//! gallery_dl/extractor/foriio.py through `api.foriio.com`.
//!
//! Image works resolve to their original files on `foriio.imgix.net`;
//! video and sound works are handed to yt-dlp as upstream's `ytdl:` URLs,
//! and web articles are queued when a native extractor supports them.
use super::{digits, on_host, raw_segments, str_of, truthy};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use futures_util::{StreamExt, stream};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://foriio.com";
const API: &str = "https://api.foriio.com/api";
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

/// `fori(?:io\.com|\.io)/works/(\d+)`, else `/(?!works/)([^/?#]+)`.
fn target(url: &Url) -> Option<Target> {
    if !on_host(url, &["foriio.com", "fori.io"]) {
        return None;
    }
    let path = url.path();
    if let Some(rest) = path.strip_prefix("/works/") {
        return super::leading_digits(rest).map(|id| Target::Work(id.to_owned()));
    }
    match raw_segments(url).first() {
        Some(user) if path.starts_with(&format!("/{user}")) => Some(Target::User((*user).into())),
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Work(id)) => crate::sites::once(async move {
            let data = http
                .json(&http.api(&format!("{API}/v1/works/{id}"))?)
                .await?;
            let work = data
                .get("work")
                .and_then(Value::as_object)
                .ok_or_else(|| error("foriio returned no work"))?;
            work_events(http, work.clone())
        }),
        Some(Target::User(user)) => user_works(http, user),
        None => crate::sites::fail(error("unsupported foriio URL")),
    }
}

/// `_orig`: the original file for an image name.
fn orig(raw: &str) -> (String, Option<Url>) {
    let raw = raw.split('?').next().unwrap_or_default();
    let name = raw.rsplit('/').next().unwrap_or_default();
    let url = format!("https://foriio.imgix.net/store/{name}");
    (name.to_owned(), Url::parse(&url).ok())
}

/// An original image file with its name's `filename`/`extension`.
fn image(raw: &str, data: &Map<String, Value>) -> Option<Item> {
    let (name, url) = orig(raw);
    let mut fields = data.clone();
    let (stem, ext) = super::nameext_from_name(&crate::sites::percent_decode(&name));
    fields.insert("filename".into(), stem.into());
    fields.insert("extension".into(), ext.into());
    Some(Item::new(url?, fields))
}

/// Upstream `ForiioWorkExtractor.items` for one API work.
fn work_events(http: &Http, mut work: Map<String, Value>) -> Result<Events<'_>> {
    for key in [
        "related_works",
        "single_work_og",
        "og_image_url_for_instagram",
    ] {
        work.remove(key);
    }
    let work_id = work.remove("id").unwrap_or(Value::Null);
    work.insert("work_id".into(), work_id.clone());
    let timestamp = super::parse_iso(
        work.get("published_at")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    work.insert("date".into(), super::date(timestamp));
    let previews = http.option_bool("previews", false);
    let mut items = Vec::new();
    let mut children = Vec::new();
    if let Some(files) = work.remove("images") {
        let files = files.as_array().cloned().unwrap_or_default();
        work.insert("count".into(), files.len().into());
        for (index, file) in files.iter().enumerate() {
            work.insert("num".into(), (index + 1).into());
            work.insert(
                "width".into(),
                file.get("width").cloned().unwrap_or(Value::Null),
            );
            work.insert(
                "height".into(),
                file.get("height").cloned().unwrap_or(Value::Null),
            );
            work.insert(
                "file_id".into(),
                file.get("id").cloned().unwrap_or(Value::Null),
            );
            let raw = file
                .pointer("/urls/list")
                .and_then(Value::as_str)
                .unwrap_or_default();
            items.extend(image(raw, &work));
        }
    } else if let Some((key, option, extension)) = [
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
        let enabled = http.option_bool(option, true);
        for (index, file) in files.iter().enumerate() {
            work.insert("num".into(), (index + 1).into());
            let file_id = match file.get("video_id") {
                Some(id) if key == "videos" && truthy(Some(id)) => id.clone(),
                _ => file.get("title").cloned().unwrap_or(Value::Null),
            };
            work.insert("file_id".into(), file_id);
            if let Some(file) = file.as_object() {
                super::update(&mut work, file);
            }
            let page = str_of(file, "url");
            if enabled && key == "web_articles" {
                if let Ok(child) = Url::parse(page)
                    && crate::sites::plain(&child)
                    && crate::sites::find(&child).is_some()
                {
                    children.push((child, work.clone()));
                }
            } else if enabled {
                let mut fields = work.clone();
                fields.insert("extension".into(), extension.into());
                items.extend(crate::sites::social::ytdl_item(page, fields));
            }
            if previews {
                let picture = if key == "web_articles" {
                    "image"
                } else {
                    "picture_url"
                };
                items.extend(image(str_of(file, picture), &work));
            }
        }
    } else if let Some(file) = work.get("copy_writing").cloned() {
        work.insert("count".into(), 1.into());
        work.insert("num".into(), 1.into());
        work.insert(
            "file_id".into(),
            file.get("id").cloned().unwrap_or(Value::Null),
        );
        items.extend(image(str_of(&file, "image"), &work));
    } else {
        return Err(error(format!(
            "foriio work {}: unsupported type {}",
            crate::sites::scalar(&work_id),
            work.get("type")
                .map(crate::sites::scalar)
                .unwrap_or_default()
        )));
    }
    let id = crate::sites::scalar(&work_id);
    let user = work
        .get("author")
        .and_then(|a| a.get("screen_name"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let title = work.get("title").and_then(Value::as_str).map(str::to_owned);
    let items = items
        .into_iter()
        .map(|item| {
            let file_id = item
                .keywords
                .get("file_id")
                .map(crate::sites::scalar)
                .unwrap_or_default();
            item.ids(id.clone(), file_id)
                .user(user.clone())
                .caption(title.clone())
                .timestamp(timestamp)
        })
        .collect();
    let files = crate::sites::events(crate::sites::post(&SITE, http, &WORK, items));
    let queued = stream::iter(children)
        .map(move |(child, data)| crate::sites::queue(http, "foriio", "work", child, &data))
        .flatten();
    Ok(files.chain(queued).boxed())
}

/// Upstream `ForiioUserExtractor`: queue each work, optionally only those
/// whose `type` is listed in the `posts` option.
fn user_works(http: &Http, user: String) -> Events<'_> {
    let posts: Option<Vec<String>> = match http.option("posts") {
        Some(Value::String(s)) if !s.is_empty() => Some(s.split(',').map(str::to_owned).collect()),
        Some(Value::Array(items)) if !items.is_empty() => {
            Some(items.iter().map(crate::sites::scalar).collect())
        }
        _ => None,
    };
    crate::sites::paginate(Some(1u64), move |page| {
        let (user, posts) = (user.clone(), posts.clone());
        async move {
            let Some(page) = page else {
                return Ok(None);
            };
            let mut url = http.api(&format!("{API}/v1/users/{user}/works"))?;
            url.query_pairs_mut()
                .append_pair("page", &page.to_string())
                .append_pair("per_page", "20");
            let referer = format!("{ROOT}/");
            let data = http
                .json_with(&url, &[("Referer", referer.as_str()), ("Origin", ROOT)])
                .await?;
            let mut children = Vec::new();
            for work in data
                .get("works")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let kind = work.get("type").map(crate::sites::scalar);
                if let (Some(posts), Some(kind)) = (&posts, &kind)
                    && !posts.contains(kind)
                {
                    continue;
                }
                let id = work.get("id").map(crate::sites::scalar).unwrap_or_default();
                if !digits(&id) {
                    continue;
                }
                if let (Ok(child), Some(fields)) =
                    (Url::parse(&format!("{ROOT}/works/{id}")), work.as_object())
                {
                    children.push((child, fields.clone()));
                }
            }
            let current = data.pointer("/meta/current_page").and_then(Value::as_i64);
            let total = data.pointer("/meta/total_pages").and_then(Value::as_i64);
            let next = match (current, total) {
                (Some(current), Some(total)) if current < total => Some(page + 1),
                _ => None,
            };
            let events = stream::iter(children)
                .map(move |(child, data)| crate::sites::queue(http, "foriio", "user", child, &data))
                .flatten()
                .boxed();
            Ok(Some((events, next)))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http, json, keywords, media, queued, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.foriio.com/works/12345"),
            Some(Target::Work("12345".into()))
        );
        assert_eq!(
            t("https://fori.io/works/9x"),
            Some(Target::Work("9".into()))
        );
        assert_eq!(
            t("https://foriio.com/user_a"),
            Some(Target::User("user_a".into()))
        );
        assert_eq!(
            t("https://fori.io/works"),
            Some(Target::User("works".into()))
        );
        for raw in [
            "https://foriio.com/",
            "https://foriio.com/works/x",
            "https://api.foriio.com/user",
            "https://foriio.com.evil.com/user",
            "https://foriio.com:8080/user",
            "https://u:p@foriio.com/user",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn work(extra: Value) -> Value {
        let mut work = json!({"id": 77, "title": "My Work", "type": "image",
            "published_at": "2020-01-02T03:04:05.000+09:00",
            "author": {"id": 5, "screen_name": "artist"},
            "related_works": [], "single_work_og": "x"});
        super::super::update(work.as_object_mut().unwrap(), extra.as_object().unwrap());
        json!({"work": work})
    }

    #[tokio::test]
    async fn extracts_image_work() {
        let body = work(json!({"images": [
            {"id": 1, "width": 10, "height": 20,
             "urls": {"list": "https://foriio.imgix.net/store/list/abc.png?w=300"}},
            {"id": 2, "width": 1, "height": 2,
             "urls": {"list": "https://foriio.imgix.net/store/list/def.JPG"}}]}));
        let (root, server) = serve(vec![json(body)]);
        let http = http("foriio", root);
        let files = media(extract(
            &http,
            Url::parse("https://foriio.com/works/77").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].url.as_str(),
            "https://foriio.imgix.net/store/abc.png"
        );
        let k = keywords(&files[1]);
        assert_eq!(k["work_id"], 77);
        assert!(!k.contains_key("id") && !k.contains_key("related_works"));
        assert_eq!(k["date"], "2020-01-01 18:04:05");
        assert_eq!(k["count"], 2);
        assert_eq!(k["num"], 2);
        assert_eq!(k["file_id"], 2);
        assert_eq!(k["width"], 1);
        assert_eq!(k["extension"], "jpg");
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "02 2.jpg");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("foriio/artist (5)/2020-01-01 My Work (77)")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{work_id}/{file_id}/{num}",["77","1","1"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/v1/works/77 "));
    }

    #[tokio::test]
    async fn video_works_use_ytdl_and_previews() {
        let body = work(json!({"type": "video", "videos": [
            {"video_id": "", "title": "Clip", "url": "https://www.youtube.com/watch?v=abc",
             "picture_url": "https://foriio.imgix.net/store/x/pic.jpg"}]}));
        let (root, server) = serve(vec![json(body)]);
        let http = http("foriio", root).with_options(Some(std::sync::Arc::new(
            json!({"previews": true}).as_object().unwrap().clone(),
        )));
        let files = media(extract(
            &http,
            Url::parse("https://foriio.com/works/77").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].url.as_str(),
            "ytdl:https://www.youtube.com/watch?v=abc"
        );
        assert!(matches!(
            files[0].job,
            crate::extract::MediaJob::VideoPage { .. }
        ));
        let k = keywords(&files[0]);
        assert_eq!(k["file_id"], "Clip");
        assert_eq!(k["extension"], "mp4");
        assert_eq!(
            files[1].url.as_str(),
            "https://foriio.imgix.net/store/pic.jpg"
        );
        assert_eq!(keywords(&files[1])["extension"], "jpg");
        server.join().unwrap();
    }

    #[tokio::test]
    async fn queues_user_works() {
        let page = |current: i64, ids: &[i64]| {
            json!({"works": ids.iter().map(|id| json!({"id": id, "type": if *id == 2 {"video"} else {"image"}})).collect::<Vec<_>>(),
                "meta": {"current_page": current, "total_pages": 2}})
        };
        let (root, server) = serve(vec![json(page(1, &[1, 2])), json(page(2, &[3]))]);
        let http = http("foriio", root).with_options(Some(std::sync::Arc::new(
            json!({"posts": "image"}).as_object().unwrap().clone(),
        )));
        let children = queued(extract(
            &http,
            Url::parse("https://foriio.com/someone").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].subcategory(), "user");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/v1/users/someone/works?page=1&per_page=20 "));
        assert!(requests[0].contains("origin: https://foriio.com"));
        assert!(requests[1].starts_with("GET /api/v1/users/someone/works?page=2&per_page=20 "));
    }
}
