//! FikFap posts, users and hashtags, mirroring gallery_dl/extractor/fikfap.py.
//! Posts without an original file are downloaded from their HLS stream.
use crate::error;
use crate::sites::gdl::{captures, date, ext_from_url, group, hls_item, parse_iso, re, str_of};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://fikfap.com";
const API: &str = "https://api.fikfap.com";
const FILENAME: &str = "{postId} {label[:240]}.{extension}";

static POST: Route = Route::new(
    "post",
    FILENAME,
    &["{category}", "{author[username]}"],
    "{postId}",
);
static USER: Route = Route::new(
    "user",
    FILENAME,
    &["{category}", "{author[username]}"],
    "{postId}",
);
static HASHTAG: Route = Route::new(
    "hashtag",
    FILENAME,
    &["{category}", "{hashtag}"],
    "{postId}",
);

pub static SITE: Site = Site {
    category: "fikfap",
    name: "FikFap",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&POST, &USER, &HASHTAG],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Post(String),
    User(String),
    Hashtag(String),
}

fn target(url: &Url) -> Option<Target> {
    const BASE: &str = r"^https?://(?:www\.)?fikfap\.com";
    if let Some(c) = captures(re!(&format!(r"{BASE}/(?:user/[^/?#]+/)?post/(\d+)")), url) {
        return Some(Target::Post(group(&c, 1).into()));
    }
    if let Some(c) = captures(re!(&format!(r"{BASE}/user/([^/?#]+)")), url) {
        return Some(Target::User(group(&c, 1).into()));
    }
    captures(re!(&format!(r"{BASE}/hash/([^/?#]+)")), url)
        .map(|c| Target::Hashtag(group(&c, 1).into()))
}

async fn api(http: &Http, url: &str, params: &[(&str, String)]) -> crate::Result<Value> {
    let mut url = http.api(url)?;
    if !params.is_empty() {
        url.query_pairs_mut()
            .extend_pairs(params.iter().map(|(k, v)| (*k, v.as_str())));
    }
    let referer = format!("{ROOT}/");
    http.json_with(
        &url,
        &[
            ("Referer", referer.as_str()),
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

/// `FikfapExtractor.items` for one post.
fn post_item(post: &Value, extra: &Map<String, Value>) -> Option<Item> {
    let mut fields = post.as_object()?.clone();
    fields.insert("date".into(), date(parse_iso(str_of(post, "createdAt"))));
    fields.insert(
        "date_updated".into(),
        date(parse_iso(str_of(post, "updatedAt"))),
    );
    let tags: Vec<Value> = post["hashtags"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|t| t["label"].clone())
        .collect();
    fields.insert("tags".into(), tags.into());
    fields.insert(
        "filename".into(),
        post.get("label").cloned().unwrap_or(Value::Null),
    );
    fields.extend(extra.clone());
    let id = crate::sites::scalar(&post["postId"]);
    let item = if let Some(original) = post
        .get("videoFileOriginalUrl")
        .and_then(Value::as_str)
        .filter(|u| !u.is_empty())
    {
        fields.insert("extension".into(), ext_from_url(original).into());
        Item::new(Url::parse(original).ok()?, fields)
    } else {
        let stream = post
            .get("videoStreamUrl")
            .and_then(Value::as_str)
            .filter(|u| !u.is_empty())?;
        fields.insert("extension".into(), "mp4".into());
        let referer = format!("{ROOT}/");
        hls_item(stream, fields)?
            .header("Referer", &referer)
            .header("Origin", ROOT)
            .header("Sec-Fetch-Dest", "empty")
            .header("Sec-Fetch-Mode", "cors")
            .header("Sec-Fetch-Site", "cross-site")
    };
    Some(item.ids(id.clone(), id))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported FikFap URL"));
    };
    let (route, endpoint, extra): (&'static Route, String, Map<String, Value>) = match target {
        Target::Post(id) => {
            return crate::sites::once(async move {
                let post = api(http, &format!("{API}/posts/{id}"), &[]).await?;
                if crate::sites::scalar(&post["postId"]) != id {
                    return Err(error("FikFap post not found"));
                }
                let items = post_item(&post, &Map::new()).into_iter().collect();
                Ok(crate::sites::events(crate::sites::post(
                    &SITE, http, &POST, items,
                )))
            });
        }
        Target::User(user) => (
            &USER,
            format!("{API}/profile/username/{user}/posts"),
            Map::new(),
        ),
        Target::Hashtag(tag) => {
            let mut extra = Map::new();
            extra.insert("hashtag".into(), tag.as_str().into());
            (&HASHTAG, format!("{API}/hashtags/label/{tag}/posts"), extra)
        }
    };
    crate::sites::paginate(Some(None::<String>), move |state| {
        let endpoint = endpoint.clone();
        let extra = extra.clone();
        async move {
            let Some(after) = state else { return Ok(None) };
            let mut params = vec![("amount", "21".to_owned())];
            if let Some(after) = after {
                params.push(("afterId", after));
            }
            let posts = api(http, &endpoint, &params)
                .await?
                .as_array()
                .cloned()
                .unwrap_or_default();
            let next = (posts.len() >= 21)
                .then(|| {
                    posts
                        .last()
                        .map(|p| Some(crate::sites::scalar(&p["postId"])))
                })
                .flatten();
            let events = posts
                .iter()
                .filter_map(|post| post_item(post, &extra))
                .flat_map(|item| crate::sites::post(&SITE, http, route, vec![item]))
                .collect();
            Ok(Some((crate::sites::events(events), next)))
        }
    })
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
            t("https://fikfap.com/user/alinevs/post/1429486"),
            Some(Target::Post("1429486".into()))
        );
        assert_eq!(
            t("https://fikfap.com/user/Hot-sauce-34"),
            Some(Target::User("Hot-sauce-34".into()))
        );
        assert_eq!(
            t("https://fikfap.com/hash/foo-bar"),
            Some(Target::Hashtag("foo-bar".into()))
        );
    }

    #[tokio::test]
    async fn extracts_original_and_stream_posts() {
        let post = |id: i64, original: Option<&str>| {
            json!({"postId": id, "label": "Clip", "createdAt": "2024-01-02T03:04:05Z",
            "updatedAt": "2024-01-02T03:04:05Z", "hashtags": [{"label": "tag"}], "author": {"username": "alinevs"},
            "videoFileOriginalUrl": original, "videoStreamUrl": "https://vz.fikfap.com/a/playlist.m3u8"})
        };
        let mut first: Vec<Value> = (0..20)
            .map(|i| post(i, Some("https://cdn.fikfap.com/v/a.mov")))
            .collect();
        first.push(post(20, None));
        let (root, server) = serve(vec![json(Value::Array(first)), json(json!([]))]);
        let http = client("fikfap", root);
        let files = media(extract(
            &http,
            Url::parse("https://fikfap.com/user/alinevs").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 21);
        assert_eq!(keywords(&files[0])["extension"], "mov");
        assert_eq!(
            files[20].url.as_str(),
            "ytdl:https://vz.fikfap.com/a/playlist.m3u8"
        );
        assert_eq!(files[20].headers["Origin"], "https://fikfap.com");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "0 Clip.mov");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("fikfap/alinevs")
        );
        let requests = server.join().unwrap();
        assert!(
            requests[1].starts_with("GET /profile/username/alinevs/posts?amount=21&afterId=20 ")
        );
    }
}
