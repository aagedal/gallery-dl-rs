//! Public LinkedIn posts: `linkedin.com/posts/SLUG` and
//! `linkedin.com/feed/update/urn:li:activity|ugcPost|share:ID`.
//!
//! gallery-dl has no LinkedIn extractor, and yt-dlp reads videos only. The
//! logged-out post page lists a post's images in order (at the 800px size
//! offered to visitors; the signed URLs cannot be resized) and describes
//! the post, or its video, in JSON-LD. The file extension comes from a
//! 16-byte probe of each image.
use crate::error;
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

pub static SITE: Site = Site {
    category: "linkedin",
    name: "LinkedIn",
    matches: |url| post_path(url).is_some(),
    extract,
    routes: &[&POST],
    api_key: false,
    cookies: Some("linkedin.com"),
};

static POST: Route = Route::new(
    "post",
    "{post_id}_{num}.{extension}",
    &["{category}", "{author}"],
    "{post_id}_{num}",
);

const ROOT: &str = "https://www.linkedin.com";

/// The page path of a post URL, and its identifier.
fn post_path(url: &Url) -> Option<(String, String)> {
    if !crate::sites::plain(url) {
        return None;
    }
    let host = url.host_str()?;
    if host != "linkedin.com" && !host.ends_with(".linkedin.com") {
        return None;
    }
    let segments: Vec<&str> = url.path().split('/').filter(|s| !s.is_empty()).collect();
    match segments.as_slice() {
        ["posts", slug] => {
            let id = slug
                .rsplit(['-', '_'])
                .find(|part| part.len() >= 15 && part.bytes().all(|b| b.is_ascii_digit()))?;
            let safe = slug.len() <= 400
                && slug
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'%'));
            safe.then(|| (format!("/posts/{slug}"), id.to_owned()))
        }
        ["feed", "update", urn] => {
            let id = urn
                .strip_prefix("urn:li:activity:")
                .or_else(|| urn.strip_prefix("urn:li:ugcPost:"))
                .or_else(|| urn.strip_prefix("urn:li:share:"))?;
            (!id.is_empty() && id.len() <= 32 && id.bytes().all(|b| b.is_ascii_digit()))
                .then(|| (format!("/feed/update/{urn}"), id.to_owned()))
        }
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some((path, id)) = post_path(&url) else {
        return crate::sites::fail(error("unsupported LinkedIn URL"));
    };
    crate::sites::once(async move {
        let page = http.api(&format!("{ROOT}{path}"))?;
        let html = http
            .text_with(
                &page,
                &http.browser_headers(&[("Accept-Language", "en-US,en")]),
            )
            .await?;
        let ld = json_ld(&html)
            .ok_or_else(|| error("LinkedIn post unavailable without login; pass --cookies"))?;
        let author = ld
            .pointer("/author/name")
            .or_else(|| ld.pointer("/creator/name"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let text = ld
            .get("articleBody")
            .or_else(|| ld.get("description"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let date = ld
            .get("datePublished")
            .or_else(|| ld.get("uploadDate"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let mut files: Vec<(Url, Option<&'static str>)> = Vec::new();
        if ld.get("@type").and_then(Value::as_str) == Some("VideoObject") {
            if let Some(video) = https(ld.get("contentUrl").and_then(Value::as_str)) {
                files.push((video, Some("mp4")));
            }
        } else {
            files.extend(images(&html).into_iter().map(|url| (url, None)));
            if files.is_empty()
                && let Some(image) = https(ld.pointer("/image/url").and_then(Value::as_str))
            {
                files.push((image, None));
            }
        }
        let mut items = Vec::new();
        for (url, extension) in files {
            let num = items.len() + 1;
            let extension = match extension {
                Some(extension) => extension,
                None => match http.api(url.as_str()) {
                    Ok(probe) => http
                        .get(&probe, &http.browser_headers(&[("Range", "bytes=0-15")]))
                        .await
                        .map(|bytes| sniff(&bytes))
                        .unwrap_or("jpg"),
                    Err(_) => "jpg",
                },
            };
            let mut keywords = Map::new();
            keywords.insert("post_id".into(), id.clone().into());
            keywords.insert("author".into(), author.clone().into());
            keywords.insert("text".into(), text.clone().into());
            keywords.insert("date".into(), date.clone().into());
            keywords.insert("num".into(), num.into());
            keywords.insert("filename".into(), format!("{id}_{num}").into());
            keywords.insert("extension".into(), extension.into());
            items.push(
                Item::new(url, keywords)
                    .ids(id.clone(), format!("{id}_{num}"))
                    .user(author.clone())
                    .caption(Some(text.clone())),
            );
        }
        Ok(crate::sites::events(crate::sites::post(
            &SITE, http, &POST, items,
        )))
    })
}

fn https(raw: Option<&str>) -> Option<Url> {
    Url::parse(raw?)
        .ok()
        .filter(|url| url.scheme() == "https" && url.host_str().is_some())
}

/// The post's JSON-LD object (`SocialMediaPosting` or `VideoObject`).
fn json_ld(html: &str) -> Option<Value> {
    let marker = r#"<script type="application/ld+json">"#;
    let mut rest = html;
    while let Some(start) = rest.find(marker) {
        let body = &rest[start + marker.len()..];
        let end = body.find("</script>")?;
        if let Ok(value) = serde_json::from_str::<Value>(body[..end].trim())
            && matches!(
                value.get("@type").and_then(Value::as_str),
                Some("SocialMediaPosting" | "VideoObject" | "Article")
            )
        {
            return Some(value);
        }
        rest = &body[end..];
    }
    None
}

/// The image URLs of the post's `feed-images-content` list, in order.
fn images(html: &str) -> Vec<Url> {
    let Some(start) = html.find(r#"data-test-id="feed-images-content""#) else {
        return Vec::new();
    };
    let list = &html[start..];
    let list = &list[..list.find("</ul>").unwrap_or(list.len())];
    let mut urls: Vec<Url> = Vec::new();
    let marker = r#"data-delayed-url=""#;
    let mut rest = list;
    while let Some(pos) = rest.find(marker) {
        rest = &rest[pos + marker.len()..];
        let raw = &rest[..rest.find('"').unwrap_or(0)];
        let raw = raw.replace("&amp;", "&");
        if let Some(url) = https(Some(&raw)).filter(|url| url.host_str() == Some("media.licdn.com"))
            && !urls.contains(&url)
        {
            urls.push(url);
        }
    }
    urls
}

fn sniff(bytes: &[u8]) -> &'static str {
    match bytes {
        [0x89, b'P', b'N', b'G', ..] => "png",
        [b'G', b'I', b'F', ..] => "gif",
        [
            b'R',
            b'I',
            b'F',
            b'F',
            _,
            _,
            _,
            _,
            b'W',
            b'E',
            b'B',
            b'P',
            ..,
        ] => "webp",
        _ => "jpg",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, response, serve};
    use serde_json::json;

    #[test]
    fn matches_post_urls() {
        let p = |raw: &str| post_path(&Url::parse(raw).unwrap()).map(|(_, id)| id);
        assert_eq!(
            p("https://www.linkedin.com/posts/someone_topic-activity-7501212373572050945-8YjQ")
                .as_deref(),
            Some("7501212373572050945")
        );
        assert_eq!(
            p("https://www.linkedin.com/feed/update/urn:li:ugcPost:7513004942043824128/")
                .as_deref(),
            Some("7513004942043824128")
        );
        for raw in [
            "https://www.linkedin.com/posts/invalid",
            "https://www.linkedin.com/in/someone",
            "https://www.linkedin.com/feed/update/urn:li:job:1",
            "https://evil.test/posts/a-activity-7501212373572050945-x",
        ] {
            assert_eq!(p(raw), None, "{raw}");
        }
    }

    #[tokio::test]
    async fn extracts_image_lists_and_videos() {
        let ld = json!({"@type": "SocialMediaPosting", "author": {"name": "Ada"},
            "articleBody": "Hello", "datePublished": "2026-08-24T11:30:23.540Z",
            "image": {"url": "https://media.licdn.com/dms/image/v2/cover"}});
        let page = format!(
            r#"<script type="application/ld+json">{ld}</script>
            <ul data-test-id="feed-images-content">
              <li data-position="0"><img data-delayed-url="https://media.licdn.com/dms/image/v2/A/feedshare-shrink_800/x?e=1&amp;t=a"></li>
              <li data-position="1"><img data-delayed-url="https://media.licdn.com/dms/image/v2/B/feedshare-shrink_800/y?e=1&amp;t=b"></li>
            </ul><img data-delayed-url="https://media.licdn.com/elsewhere">"#
        );
        let video = json!({"@type": "VideoObject", "creator": {"name": "Ada"},
            "contentUrl": "https://dms.licdn.com/playlist/vid/v2/v.mp4"});
        let (root, server) = serve(vec![
            html(&page),
            response("206 Partial Content", "image/png", b"\x89PNG\r\n"),
            response("206 Partial Content", "image/jpeg", b"\xff\xd8\xff"),
            html(&format!(
                r#"<script type="application/ld+json">{video}</script>"#
            )),
            html("<html>authwall</html>"),
        ]);
        let http = http("linkedin", root);
        let url = Url::parse(
            "https://www.linkedin.com/posts/ada_hello-activity-7501212373572050945-8YjQ",
        )
        .unwrap();
        let files = media(extract(&http, url.clone())).await;
        let names: Vec<String> = files
            .iter()
            .map(|f| {
                crate::sites::default_templates(f)
                    .unwrap()
                    .0
                    .render(f)
                    .unwrap()
            })
            .collect();
        assert_eq!(
            names,
            ["7501212373572050945_1.png", "7501212373572050945_2.jpg"]
        );
        assert_eq!(
            files[0].url.as_str(),
            "https://media.licdn.com/dms/image/v2/A/feedshare-shrink_800/x?e=1&t=a"
        );
        assert_eq!(keywords(&files[0])["author"], "Ada");
        let videos = media(extract(&http, url.clone())).await;
        assert_eq!(
            videos[0].url.as_str(),
            "https://dms.licdn.com/playlist/vid/v2/v.mp4"
        );
        let events: Vec<_> = futures_util::StreamExt::collect(extract(&http, url)).await;
        let message = events[0].as_ref().err().unwrap().to_string();
        assert!(message.contains("--cookies"), "{message}");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /posts/ada_hello-activity-7501212373572050945-8YjQ "));
    }
}
