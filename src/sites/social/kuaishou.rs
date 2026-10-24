//! Kuaishou short videos through the web client's `visionVideoDetail`
//! GraphQL query.
//!
//! gallery-dl and yt-dlp have no Kuaishou extractor; formats follow
//! gallery-dl's conventions. Kuaishou answers anonymous clients with a
//! captcha, so this needs the cookies of a browser session that has opened
//! kuaishou.com (`--cookies` or `--cookies-from-browser`); a captcha answer
//! is reported as such.
use crate::error;
use crate::sites::http::Body;
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value, json};
use url::Url;

pub static SITE: Site = Site {
    category: "kuaishou",
    name: "Kuaishou",
    matches: |url| video_id(url).is_some(),
    extract,
    routes: &[&VIDEO],
    api_key: false,
    cookies: Some("kuaishou.com"),
};

static VIDEO: Route = Route::new(
    "video",
    "{id}.{extension}",
    &["{category}", "{user_name}"],
    "{id}",
);

const ROOT: &str = "https://www.kuaishou.com";
const QUERY: &str = "query visionVideoDetail($photoId: String, $type: String, $page: String, \
    $webPageArea: String) { visionVideoDetail(photoId: $photoId, type: $type, page: $page, \
    webPageArea: $webPageArea) { status type author { id name } photo { id duration caption \
    coverUrl photoUrl timestamp likeCount viewCount } } }";

/// `www.kuaishou.com/short-video/ID`.
fn video_id(url: &Url) -> Option<String> {
    if !crate::sites::plain(url) || !matches!(url.host_str()?, "www.kuaishou.com" | "kuaishou.com")
    {
        return None;
    }
    let id = url
        .path()
        .strip_prefix("/short-video/")?
        .trim_end_matches('/');
    (!id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric()))
        .then(|| id.to_owned())
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(id) = video_id(&url) else {
        return crate::sites::fail(error("unsupported Kuaishou URL"));
    };
    crate::sites::once(async move {
        let endpoint = http.api(&format!("{ROOT}/graphql"))?;
        let referer = format!("{ROOT}/short-video/{id}");
        // The web client's device cookie; without it the API answers
        // `{"result":2}`.
        let page = http.api(&referer)?;
        if http.cookie(&page, "did").is_none() {
            http.get(&page, &http.browser_headers(&[])).await?;
        }
        let query = json!({
            "operationName": "visionVideoDetail",
            "variables": {"photoId": id, "page": "detail"},
            "query": QUERY,
        });
        let headers = http.browser_headers(&[
            ("Referer", referer.as_str()),
            ("Origin", ROOT),
            ("Content-Type", "application/json"),
        ]);
        let body = http.post(&endpoint, Body::Json(&query), &headers).await?;
        let data: Value =
            serde_json::from_slice(&body).map_err(|_| error("Kuaishou returned invalid JSON"))?;
        Ok(crate::sites::events(crate::sites::post(
            &SITE,
            http,
            &VIDEO,
            items(&data, &id)?,
        )))
    })
}

fn items(data: &Value, id: &str) -> crate::Result<Vec<Item>> {
    if data.pointer("/data/captcha").is_some()
        || data
            .pointer("/data/url")
            .and_then(Value::as_str)
            .is_some_and(|url| url.contains("captcha"))
    {
        return Err(error(
            "Kuaishou requires a captcha-verified session; open kuaishou.com in a browser \
             and pass its cookies with --cookies or --cookies-from-browser",
        ));
    }
    if data
        .get("result")
        .and_then(Value::as_i64)
        .is_some_and(|result| result != 1)
    {
        return Err(error(
            "Kuaishou refused the session; open kuaishou.com in a browser and pass its \
             cookies with --cookies or --cookies-from-browser",
        ));
    }
    let detail = data
        .pointer("/data/visionVideoDetail")
        .filter(|detail| detail.is_object())
        .ok_or_else(|| error("Kuaishou returned no video details"))?;
    let photo = detail
        .get("photo")
        .filter(|photo| photo.is_object())
        .ok_or_else(|| error("Kuaishou video not found or not public"))?;
    let raw = photo
        .get("photoUrl")
        .and_then(Value::as_str)
        .ok_or_else(|| error("Kuaishou video has no playable URL"))?;
    let url = Url::parse(raw)
        .ok()
        .filter(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some())
        .ok_or_else(|| error("Kuaishou returned an invalid video URL"))?;
    let author = detail.get("author").cloned().unwrap_or(Value::Null);
    let name = author
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_owned);
    // Timestamps are milliseconds.
    let timestamp = photo
        .get("timestamp")
        .and_then(Value::as_i64)
        .map(|ms| ms / 1000);
    let caption = photo
        .get("caption")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let mut keywords = Map::new();
    keywords.insert("id".into(), id.into());
    keywords.insert("caption".into(), caption.clone().into());
    keywords.insert(
        "user_id".into(),
        author.get("id").cloned().unwrap_or(Value::Null),
    );
    keywords.insert("user_name".into(), name.clone().into());
    for key in ["duration", "likeCount", "viewCount", "coverUrl"] {
        keywords.insert(key.into(), photo.get(key).cloned().unwrap_or(Value::Null));
    }
    keywords.insert("date".into(), timestamp.into());
    keywords.insert("filename".into(), id.into());
    keywords.insert("extension".into(), "mp4".into());
    Ok(vec![
        Item::new(url, keywords)
            .ids(id, format!("{id}_1"))
            .user(name)
            .caption(caption)
            .timestamp(timestamp)
            .header("Referer", &format!("{ROOT}/")),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http, json as reply, keywords, media, serve};

    #[test]
    fn matches_short_video_urls() {
        let id = |raw: &str| video_id(&Url::parse(raw).unwrap());
        assert_eq!(
            id("https://www.kuaishou.com/short-video/3xf8enef8rg4hkc?x=1").as_deref(),
            Some("3xf8enef8rg4hkc")
        );
        for raw in [
            "https://www.kuaishou.com/",
            "https://www.kuaishou.com/short-video/",
            "https://www.kuaishou.com/short-video/a-b",
            "https://www.kuaishou.com:8443/short-video/abc",
            "https://evil.test/short-video/abc",
        ] {
            assert_eq!(id(raw), None, "{raw}");
        }
    }

    #[tokio::test]
    async fn extracts_a_video_and_reports_captchas() {
        let detail = json!({"data": {"visionVideoDetail": {"status": 1,
            "author": {"id": "u1", "name": "Alice"},
            "photo": {"id": "3xabc", "caption": "hi", "duration": 9000,
                "photoUrl": "https://v2.kwaicdn.com/upic/a.mp4?tag=1",
                "timestamp": 1700000000123_i64}}}});
        let captcha = json!({"data": {"result": 400002,
            "url": "https://captcha.zt.kuaishou.com/iframe/index.html?captchaSession=x"}});
        let page = crate::sites::testing::response(
            "200 OK",
            "text/html\r\nSet-Cookie: did=web_1; Path=/",
            b"<html></html>",
        );
        let (root, server) = serve(vec![page, reply(detail), reply(captcha)]);
        let http = http("kuaishou", root);
        let url = Url::parse("https://www.kuaishou.com/short-video/3xabc").unwrap();
        let files = media(extract(&http, url.clone())).await;
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].url.as_str(),
            "https://v2.kwaicdn.com/upic/a.mp4?tag=1"
        );
        let k = keywords(&files[0]);
        assert_eq!(
            (k["user_name"].clone(), k["date"].clone()),
            (json!("Alice"), json!(1700000000))
        );
        let (name, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(name.render(&files[0]).unwrap(), "3xabc.mp4");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("kuaishou/Alice")
        );
        let events: Vec<_> = futures_util::StreamExt::collect(extract(&http, url)).await;
        let message = events[0].as_ref().err().unwrap().to_string();
        assert!(message.contains("captcha-verified session"), "{message}");
        assert!(!message.contains("captchaSession"));
        let requests = server.join().unwrap();
        // The device cookie is fetched once and then sent with the query.
        assert!(
            requests[0].starts_with("GET /short-video/3xabc "),
            "{}",
            requests[0]
        );
        assert!(requests[1].starts_with("POST /graphql "), "{}", requests[1]);
        assert!(requests[1].contains("did=web_1"));
        assert!(requests[1].contains("\"photoId\":\"3xabc\""));
        assert!(requests[2].starts_with("POST /graphql "));
    }
}
