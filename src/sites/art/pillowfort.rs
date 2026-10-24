//! Pillowfort posts and user blogs, mirroring
//! gallery_dl/extractor/pillowfort.py through the `/json/` post endpoints.
//!
//! Upstream's `username`/`password` login is not supported; a
//! `_Pf_new_session` or `remember_user_token` cookie from `--cookies` is
//! sent instead.
use super::{date, parse_iso, str_of, truthy};
use crate::error;
use crate::event::ExtractionEvent;
use crate::sites::{Events, Http, Item, Route, Site};
use futures_util::{StreamExt, stream};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://www.pillowfort.social";

pub static SITE: Site = Site {
    category: "pillowfort",
    name: "Pillowfort",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&POST, &USER],
    api_key: false,
    cookies: Some("www.pillowfort.social"),
};

const FILENAME: &str = "{post_id} {title|original_post[title]:?/ /}{num:>02}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{username}"];

static POST: Route = Route::new("post", FILENAME, DIRECTORY, "{id}");
static USER: Route = Route::new("user", FILENAME, DIRECTORY, "{id}");

#[derive(Debug, PartialEq)]
enum Target {
    Post(String),
    /// `USER` or `USER/tagged/TAG`, raw.
    User(String),
}

fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) || url.host_str() != Some("www.pillowfort.social") {
        return None;
    }
    let path = url.path().strip_prefix('/')?;
    if let Some(id) = path.strip_prefix("posts/") {
        return Some(Target::Post(super::leading_digits(id)?.to_owned()));
    }
    let mut parts = path.split('/');
    let user = parts.next().filter(|s| !s.is_empty())?;
    match (parts.next(), parts.next()) {
        (Some("tagged"), Some(tag)) if !tag.is_empty() => {
            Some(Target::User(format!("{user}/tagged/{tag}")))
        }
        _ => Some(Target::User(user.to_owned())),
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Pillowfort URL"));
    };
    match target {
        Target::Post(id) => crate::sites::once(async move {
            let post = http
                .json(&http.api(&format!("{ROOT}/posts/{id}/json/"))?)
                .await?;
            Ok(post_events(http, &POST, post))
        }),
        Target::User(item) => crate::sites::paginate(Some(1u64), move |page| {
            let item = item.clone();
            async move {
                let Some(page) = page else {
                    return Ok(None);
                };
                let mut url = http.api(&format!("{ROOT}/{item}/json/"))?;
                url.query_pairs_mut().append_pair("p", &page.to_string());
                let data = http.json(&url).await?;
                let posts = data
                    .get("posts")
                    .and_then(Value::as_array)
                    .cloned()
                    .ok_or_else(|| error("Pillowfort returned no posts"))?;
                let next = (posts.len() >= 20).then_some(page + 1);
                let events = stream::iter(posts)
                    .map(move |post| post_events(http, &USER, post))
                    .flatten()
                    .boxed();
                Ok(Some((events, next)))
            }
        }),
    }
}

/// One file or queued embed of a post.
pub(crate) enum Entry {
    File(Box<Item>),
    Queue(Url, Map<String, Value>),
}

fn post_events<'a>(http: &'a Http, route: &'static Route, post: Value) -> Events<'a> {
    let options = Options {
        inline: http.option_bool("inline", true),
        reblogs: http.option_bool("reblogs", false),
        external: http.option_bool("external", false),
    };
    let mut files = Vec::new();
    let mut queued = Vec::new();
    for entry in parse_post(&post, &options) {
        match entry {
            Entry::File(item) => files.push(*item),
            Entry::Queue(url, data) => queued.push((url, data)),
        }
    }
    let events: Vec<ExtractionEvent> = crate::sites::post(&SITE, http, route, files);
    let subcategory = route.subcategory;
    crate::sites::events(events)
        .chain(
            stream::iter(queued)
                .map(move |(url, data)| {
                    crate::sites::queue(http, "pillowfort", subcategory, url, &data)
                })
                .flatten(),
        )
        .boxed()
}

pub(crate) struct Options {
    pub inline: bool,
    pub reblogs: bool,
    pub external: bool,
}

/// `src="(https://img\d+\.pillowfort\.social/posts/[^"]+)` matches.
fn inline_images(content: &str) -> Vec<String> {
    let mut urls = Vec::new();
    for (index, _) in content.match_indices("src=\"https://img") {
        let rest = &content[index + "src=\"https://img".len()..];
        let shard = rest.bytes().take_while(u8::is_ascii_digit).count();
        if shard == 0 || !rest[shard..].starts_with(".pillowfort.social/posts/") {
            continue;
        }
        let end = rest.find('"').unwrap_or(rest.len());
        if end > shard + ".pillowfort.social/posts/".len() && rest[end..].starts_with('"') {
            urls.push(format!("https://img{}", &rest[..end]));
        }
    }
    urls
}

/// Upstream `items` for one post: media plus inline images, numbered
/// files, and embeds queued only with `external`.
pub(crate) fn parse_post(post: &Value, options: &Options) -> Vec<Entry> {
    let Some(fields) = post.as_object() else {
        return Vec::new();
    };
    if fields.contains_key("original_post") && !options.reblogs {
        return Vec::new();
    }
    let mut data = fields.clone();
    let mut files = match data.remove("media") {
        Some(Value::Array(media)) => media,
        _ => Vec::new(),
    };
    if options.inline {
        for url in inline_images(str_of(post, "content")) {
            let mut file = Map::new();
            file.insert("url".into(), url.into());
            files.push(file.into());
        }
    }
    let created = parse_iso(str_of(post, "created_at"));
    data.insert("date".into(), date(created));
    let post_id = data.remove("id").unwrap_or(Value::Null);
    data.insert("post_id".into(), post_id.clone());
    data.insert("count".into(), files.len().into());
    data.insert("num".into(), 0.into());
    let post_id = crate::sites::scalar(&post_id);
    let user = str_of(post, "username").to_owned();
    let mut num = 0u64;
    let mut timestamp = created;
    let mut entries = Vec::new();
    for file in files {
        let Value::Object(file) = file else {
            continue;
        };
        let raw = match file.get("url") {
            Some(url) if truthy(Some(url)) => url,
            _ => match file.get("b2_lg_url") {
                Some(url) if truthy(Some(url)) => url,
                _ => continue,
            },
        };
        let raw = raw.as_str().unwrap_or_default().to_owned();
        let embed = truthy(file.get("embed_code"));
        if embed && !options.external {
            continue;
        }
        if !embed {
            num += 1;
            data.insert("num".into(), num.into());
        }
        super::update(&mut data, &file);
        super::nameext_into(&raw, &mut data);
        let filename = data["filename"].as_str().unwrap_or_default().to_owned();
        let (hash, name) = filename.split_once('_').unwrap_or((&filename, ""));
        data.insert("hash".into(), hash.into());
        data.insert("filename".into(), name.into());
        if !file.contains_key("id") {
            data.insert("id".into(), hash.into());
        }
        if let Some(stamp) = file.get("created_at") {
            timestamp = parse_iso(stamp.as_str().unwrap_or_default());
            data.insert("date".into(), date(timestamp));
        }
        let Ok(url) = Url::parse(&raw) else {
            continue;
        };
        if !crate::sites::plain(&url) {
            continue;
        }
        if embed {
            // Only embeds of natively supported sites can be followed.
            if crate::sites::find(&url).is_some() {
                entries.push(Entry::Queue(url, data.clone()));
            }
            continue;
        }
        let media_id = data.get("id").map(crate::sites::scalar).unwrap_or_default();
        entries.push(Entry::File(Box::new(
            Item::new(url, data.clone())
                .ids(post_id.clone(), media_id)
                .user(Some(user.clone()))
                .caption(Some(str_of(post, "title").to_owned()))
                .timestamp(timestamp),
        )));
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http, json, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.pillowfort.social/posts/27510"),
            Some(Target::Post("27510".into()))
        );
        assert_eq!(
            t("https://www.pillowfort.social/Pome"),
            Some(Target::User("Pome".into()))
        );
        assert_eq!(
            t("https://www.pillowfort.social/Pome/tagged/art%20stuff"),
            Some(Target::User("Pome/tagged/art%20stuff".into()))
        );
        assert_eq!(
            t("https://www.pillowfort.social/Pome/other"),
            Some(Target::User("Pome".into()))
        );
        for raw in [
            "https://www.pillowfort.social/",
            "https://www.pillowfort.social/posts/abc",
            "https://pillowfort.social/Pome",
            "https://www.pillowfort.social.evil.com/Pome",
            "https://evilwww.pillowfort.social/Pome",
            "https://www.pillowfort.social:8443/Pome",
            "https://u:p@www.pillowfort.social/Pome",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn post(id: u64) -> Value {
        json!({"id": id, "username": "Pome", "title": "", "created_at": "2020-01-02T03:04:05.000Z",
            "content": "<p><img src=\"https://img2.pillowfort.social/posts/abc_inline.png\"></p>\
                        <img src=\"https://evil.test/posts/x.png\">",
            "original_post": {"title": "Original"},
            "media": [
                {"id": 5, "url": "https://img3.pillowfort.social/posts/f00_pic.jpg",
                 "created_at": "2020-01-03T00:00:00Z", "embed_code": null},
                {"id": 6, "url": "", "b2_lg_url": null},
                {"id": 7, "url": "https://www.youtube.com/watch?v=x", "embed_code": "<iframe>"}]})
    }

    #[tokio::test]
    async fn extracts_reblogged_posts_with_inline_images() {
        let (root, server) = serve(vec![json(post(27510))]);
        let http = http("pillowfort", root).with_options(Some(std::sync::Arc::new(
            serde_json::from_value(json!({"reblogs": true})).unwrap(),
        )));
        let files = media(extract(
            &http,
            Url::parse("https://www.pillowfort.social/posts/27510").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[0]);
        assert_eq!(k["post_id"], 27510);
        assert_eq!(k["id"], 5);
        assert_eq!(k["hash"], "f00");
        assert_eq!(k["filename"], "pic");
        assert_eq!(k["date"], "2020-01-03 00:00:00");
        assert_eq!(k["count"], 4);
        assert!(!k.contains_key("media"));
        let k = keywords(&files[1]);
        assert_eq!(k["id"], "abc");
        assert_eq!(k["num"], 2);
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "27510 Original 02.png");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("pillowfort/Pome")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{id}",["5"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /posts/27510/json/ "));
    }

    #[tokio::test]
    async fn user_pages_skip_reblogs() {
        let mut own = post(2);
        own.as_object_mut().unwrap().remove("original_post");
        own["title"] = "Mine".into();
        let mut first: Vec<Value> = (0..19).map(post).collect();
        first.push(own);
        let (root, server) = serve(vec![
            json(json!({"posts": first})),
            json(json!({"posts": []})),
        ]);
        let http = http("pillowfort", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.pillowfort.social/Pome/tagged/art").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let (file, _) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "2 Mine 01.jpg");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /Pome/tagged/art/json/?p=1 "));
        assert!(requests[1].starts_with("GET /Pome/tagged/art/json/?p=2 "));
    }
}
