//! FikFap posts, user profiles and hashtags, mirroring
//! gallery_dl/extractor/fikfap.py through `api.fikfap.com` with its
//! anonymous authorization. A post's original video is downloaded
//! directly; otherwise its HLS stream goes to yt-dlp.
use super::manifest_item;
use crate::extract::ManifestProtocol;
use crate::sites::art::{date, ext_from_url, leading_digits, on_host, parse_iso, segment, str_of};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://fikfap.com";
const API: &str = "https://api.fikfap.com";
const FILENAME: &str = "{postId} {label[:240]}.{extension}";
const ARCHIVE: &str = "{postId}";
const PER_PAGE: usize = 21;

static POST: Route = Route::new(
    "post",
    FILENAME,
    &["{category}", "{author[username]}"],
    ARCHIVE,
);
static USER: Route = Route::new(
    "user",
    FILENAME,
    &["{category}", "{author[username]}"],
    ARCHIVE,
);
static HASHTAG: Route = Route::new("hashtag", FILENAME, &["{category}", "{hashtag}"], ARCHIVE);

pub static SITE: Site = Site {
    category: "fikfap",
    name: "FikFap",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&POST, &USER, &HASHTAG],
    api_key: false,
    cookies: None,
};

/// Headers yt-dlp sends for the HLS stream.
const STREAM_HEADERS: &[(&str, &str)] = &[
    ("Referer", "https://fikfap.com/"),
    ("Origin", ROOT),
    ("Sec-Fetch-Dest", "empty"),
    ("Sec-Fetch-Mode", "cors"),
    ("Sec-Fetch-Site", "cross-site"),
];

#[derive(Debug, PartialEq)]
enum Target {
    Post(String),
    User(String),
    Hashtag(String),
}

fn target(url: &Url) -> Option<Target> {
    if !on_host(url, &["fikfap.com"]) {
        return None;
    }
    let path = url.path();
    let post = path.strip_prefix("/post/").or_else(|| {
        let rest = path.strip_prefix("/user/")?;
        let (user, rest) = rest.split_once('/')?;
        rest.strip_prefix("post/").filter(|_| segment(user))
    });
    if let Some(id) = post.and_then(leading_digits) {
        return Some(Target::Post(id.to_owned()));
    }
    let first = |prefix: &str| {
        let rest = path.strip_prefix(prefix)?;
        let name = &rest[..rest.find('/').unwrap_or(rest.len())];
        segment(name).then(|| name.to_owned())
    };
    if let Some(user) = first("/user/") {
        return Some(Target::User(user));
    }
    first("/hash/").map(Target::Hashtag)
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Post(id)) => crate::sites::once(async move {
            let post = call(http, &format!("/posts/{id}"), &[]).await?;
            if post.get("postId").map(crate::sites::scalar).as_deref() != Some(id.as_str()) {
                return Err(error("FikFap post not found"));
            }
            Ok(crate::sites::events(events(http, &POST, post, None)))
        }),
        Some(Target::User(user)) => {
            let endpoint = format!("/profile/username/{user}/posts");
            pagination(http, &USER, endpoint, None)
        }
        Some(Target::Hashtag(hashtag)) => {
            let endpoint = format!("/hashtags/label/{hashtag}/posts");
            pagination(http, &HASHTAG, endpoint, Some(hashtag))
        }
        None => crate::sites::fail(error("unsupported FikFap URL")),
    }
}

/// Upstream `request_api`.
async fn call(http: &Http, endpoint: &str, params: &[(&str, &str)]) -> Result<Value> {
    let mut url = http.api(&format!("{API}{endpoint}"))?;
    if !params.is_empty() {
        url.query_pairs_mut().extend_pairs(params);
    }
    http.json_with(
        &url,
        &[
            ("Referer", "https://fikfap.com/"),
            (
                "Authorization-Anonymous",
                "2527cc30-c3c5-41be-b8bb-104b6ea7a206",
            ),
            ("IsLoggedIn", "false"),
            ("IsPWA", "false"),
            ("Origin", ROOT),
            ("Sec-Fetch-Dest", "empty"),
            ("Sec-Fetch-Mode", "cors"),
            ("Sec-Fetch-Site", "same-site"),
        ],
    )
    .await
}

/// Pages of 21 posts, continued `afterId` the last one.
fn pagination<'a>(
    http: &'a Http,
    route: &'static Route,
    endpoint: String,
    hashtag: Option<String>,
) -> Events<'a> {
    crate::sites::paginate(Some(None::<String>), move |after| {
        let endpoint = endpoint.clone();
        let hashtag = hashtag.clone();
        async move {
            let Some(after) = after else {
                return Ok(None);
            };
            let mut params = vec![("amount", PER_PAGE.to_string())];
            if let Some(after) = after {
                params.push(("afterId", after));
            }
            let params: Vec<(&str, &str)> = params.iter().map(|(k, v)| (*k, v.as_str())).collect();
            let data = call(http, &endpoint, &params).await?;
            let posts = data.as_array().cloned().unwrap_or_default();
            let next = (posts.len() >= PER_PAGE)
                .then(|| {
                    posts
                        .last()
                        .and_then(|p| p.get("postId"))
                        .map(crate::sites::scalar)
                })
                .flatten()
                .map(Some);
            let events = posts
                .into_iter()
                .flat_map(|post| events(http, route, post, hashtag.as_deref()))
                .collect();
            Ok(Some((crate::sites::events(events), next)))
        }
    })
}

/// One post boundary and its video; posts without one are skipped.
fn events(
    http: &Http,
    route: &'static Route,
    post: Value,
    hashtag: Option<&str>,
) -> Vec<crate::event::ExtractionEvent> {
    let Value::Object(mut fields) = post else {
        return Vec::new();
    };
    let item = item(&mut fields, hashtag);
    crate::sites::post(&SITE, http, route, item.into_iter().collect())
}

fn item(fields: &mut Map<String, Value>, hashtag: Option<&str>) -> Option<Item> {
    let post = Value::Object(fields.clone());
    let original = str_of(&post, "videoFileOriginalUrl");
    let stream = str_of(&post, "videoStreamUrl");
    let created = parse_iso(str_of(&post, "createdAt"));
    fields.insert("date".into(), date(created));
    fields.insert(
        "date_updated".into(),
        date(parse_iso(str_of(&post, "updatedAt"))),
    );
    let tags: Vec<Value> = post
        .get("hashtags")
        .and_then(Value::as_array)
        .map(|tags| {
            tags.iter()
                .filter_map(|t| t.get("label").cloned())
                .collect()
        })
        .unwrap_or_default();
    fields.insert("tags".into(), tags.into());
    fields.insert(
        "filename".into(),
        post.get("label").cloned().unwrap_or(Value::Null),
    );
    if let Some(hashtag) = hashtag {
        fields.insert("hashtag".into(), hashtag.into());
    }
    let item = if !original.is_empty() {
        fields.insert("extension".into(), ext_from_url(original).into());
        Item::new(crate::sites::media::web_url(original)?, fields.clone())
    } else if !stream.is_empty() {
        fields.insert("extension".into(), "mp4".into());
        let mut item = manifest_item(stream, ManifestProtocol::Hls, fields.clone())?;
        for (name, value) in STREAM_HEADERS {
            item = item.header(name, value);
        }
        item
    } else {
        return None;
    };
    let id = post
        .get("postId")
        .map(crate::sites::scalar)
        .unwrap_or_default();
    let author = post
        .get("author")
        .map(|author| str_of(author, "username").to_owned());
    Some(
        item.ids(id.clone(), id)
            .user(author)
            .caption(Some(str_of(&post, "label").to_owned()))
            .timestamp(created),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::MediaJob;
    use crate::sites::manga::tests::{archive, path};
    use crate::sites::testing::{http, json, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://fikfap.com/user/someone/post/12345"),
            Some(Target::Post("12345".into()))
        );
        assert_eq!(
            t("https://www.fikfap.com/post/7"),
            Some(Target::Post("7".into()))
        );
        assert_eq!(
            t("https://fikfap.com/user/someone"),
            Some(Target::User("someone".into()))
        );
        assert_eq!(
            t("https://fikfap.com/hash/sunset"),
            Some(Target::Hashtag("sunset".into()))
        );
        for raw in [
            "https://fikfap.com/",
            "https://fikfap.com/user/",
            "https://fikfap.com.evil.test/post/1",
            "https://fikfap.com:8443/post/1",
            "https://u:p@fikfap.com/post/1",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn post(id: u64, stream: bool) -> Value {
        let mut post = json!({
            "postId": id, "label": format!("Clip {id}"),
            "createdAt": "2025-01-02T03:04:05.000Z", "updatedAt": "2025-01-03T03:04:05.000Z",
            "hashtags": [{"label": "sunset"}, {"label": "sea"}],
            "author": {"username": "maker"},
        });
        if stream {
            post["videoStreamUrl"] = json!(format!("https://vz.fikfap.test/{id}/playlist.m3u8"));
        } else {
            post["videoFileOriginalUrl"] =
                json!(format!("https://cdn.fikfap.test/{id}/orig.MOV?t=1"));
        }
        post
    }

    #[tokio::test]
    async fn extracts_post() {
        let (root, server) = serve(vec![json(post(12345, false))]);
        let http = http("fikfap", root);
        let files = media(extract(
            &http,
            Url::parse("https://fikfap.com/user/maker/post/12345").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let k = keywords(&files[0]);
        assert_eq!(k["extension"], "mov");
        assert_eq!(k["filename"], "Clip 12345");
        assert_eq!(k["tags"], json!(["sunset", "sea"]));
        assert_eq!(k["date"], "2025-01-02 03:04:05");
        assert_eq!(path(&files[0]), "fikfap/maker/12345 Clip 12345.mov");
        assert_eq!(archive(&files[0]), r#"["{postId}",["12345"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /posts/12345 "));
        assert!(requests[0].contains("authorization-anonymous: 2527cc30-"));
    }

    #[tokio::test]
    async fn mismatched_post_is_an_error() {
        let (root, server) = serve(vec![json(post(1, false))]);
        let http = http("fikfap", root);
        assert!(
            crate::sites::testing::fails(extract(
                &http,
                Url::parse("https://fikfap.com/post/2").unwrap(),
            ))
            .await
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn paginates_hashtag_streams() {
        let first: Vec<Value> = (1..=21).map(|i| post(i, true)).collect();
        let (root, server) = serve(vec![
            json(Value::Array(first)),
            json(json!([post(22, true), {"postId": 23, "label": "none"}])),
        ]);
        let http = http("fikfap", root);
        let files = media(extract(
            &http,
            Url::parse("https://fikfap.com/hash/sunset").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 22);
        assert_eq!(
            files[21].url.as_str(),
            "ytdl:https://vz.fikfap.test/22/playlist.m3u8"
        );
        assert!(matches!(files[21].job, MediaJob::Manifest { .. }));
        assert_eq!(files[21].headers["Referer"], "https://fikfap.com/");
        let k = keywords(&files[21]);
        assert_eq!(k["hashtag"], "sunset");
        assert_eq!(k["extension"], "mp4");
        assert_eq!(path(&files[21]), "fikfap/sunset/22 Clip 22.mp4");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /hashtags/label/sunset/posts?amount=21 "));
        assert!(requests[1].starts_with("GET /hashtags/label/sunset/posts?amount=21&afterId=21 "));
    }
}
