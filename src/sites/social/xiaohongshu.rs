//! Xiaohongshu (RED) notes: `xiaohongshu.com/explore/ID` and
//! `discovery/item/ID`, read from the page's `window.__INITIAL_STATE__` as
//! yt-dlp's extractor does, but with image notes as well as videos.
//!
//! gallery-dl has no Xiaohongshu extractor; formats follow its conventions.
//! Xiaohongshu serves notes only to links carrying their `xsec_token` (as
//! shared from the app or site) or to sessions with cookies; other requests
//! end on an error page, which is reported as such.
use crate::error;
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

pub static SITE: Site = Site {
    category: "xiaohongshu",
    name: "Xiaohongshu",
    matches: |url| note_id(url).is_some(),
    extract,
    routes: &[&NOTE],
    api_key: false,
    cookies: Some("xiaohongshu.com"),
};

static NOTE: Route = Route::new(
    "note",
    "{id}_{num}.{extension}",
    &["{category}", "{user[nickname]}"],
    "{id}_{num}",
);

const ROOT: &str = "https://www.xiaohongshu.com";
const VIDEO_ROOT: &str = "https://sns-video-bd.xhscdn.com";

/// The hexadecimal note ID of `explore/ID` and `discovery/item/ID` URLs.
fn note_id(url: &Url) -> Option<String> {
    if !crate::sites::plain(url) || url.host_str()? != "www.xiaohongshu.com" {
        return None;
    }
    let path = url.path().trim_end_matches('/');
    let id = path
        .strip_prefix("/explore/")
        .or_else(|| path.strip_prefix("/discovery/item/"))?;
    (!id.is_empty() && id.len() <= 32 && id.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| id.to_ascii_lowercase())
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(id) = note_id(&url) else {
        return crate::sites::fail(error("unsupported Xiaohongshu URL"));
    };
    crate::sites::once(async move {
        // Keep the link's own query: its xsec_token grants access.
        let mut page = http.api(&format!("{ROOT}/explore/{id}"))?;
        page.set_query(url.query());
        let html = http.text_with(&page, &http.browser_headers(&[])).await?;
        let state = initial_state(&html).ok_or_else(unavailable)?;
        let note = state
            .pointer("/note/noteDetailMap")
            .and_then(|map| map.get(&id))
            .and_then(|detail| detail.get("note"))
            .filter(|note| note.get("noteId").is_some() || note.get("type").is_some())
            .ok_or_else(unavailable)?;
        Ok(crate::sites::events(crate::sites::post(
            &SITE,
            http,
            &NOTE,
            items(note, &id),
        )))
    })
}

fn unavailable() -> crate::Error {
    error(
        "Xiaohongshu note unavailable; use a shared link with its xsec_token, or pass \
         xiaohongshu.com cookies with --cookies or --cookies-from-browser",
    )
}

/// `window.__INITIAL_STATE__`, a JavaScript object literal that is JSON
/// except for bare `undefined` values.
fn initial_state(html: &str) -> Option<Value> {
    let start = html.find("window.__INITIAL_STATE__=")? + "window.__INITIAL_STATE__=".len();
    let end = html[start..].find("</script>")? + start;
    let source = html[start..end].trim().trim_end_matches(';');
    // Replace `undefined` outside string literals only.
    let mut json = String::with_capacity(source.len());
    let (mut in_string, mut escaped) = (false, false);
    let mut rest = source;
    while let Some(c) = rest.chars().next() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
        } else if c == '"' {
            in_string = true;
        } else if let Some(tail) = rest.strip_prefix("undefined") {
            json.push_str("null");
            rest = tail;
            continue;
        }
        json.push(c);
        rest = &rest[c.len_utf8()..];
    }
    serde_json::from_str(&json).ok()
}

fn https(raw: &str) -> Option<Url> {
    let raw = if let Some(rest) = raw.strip_prefix("http://") {
        format!("https://{rest}")
    } else {
        raw.to_owned()
    };
    Url::parse(&raw)
        .ok()
        .filter(|url| url.scheme() == "https" && url.host_str().is_some())
}

/// The note's images, or its video: the original upload when its key is
/// given, else the highest-resolution stream.
fn items(note: &Value, id: &str) -> Vec<Item> {
    let user = note.get("user").cloned().unwrap_or(Value::Null);
    let nickname = user
        .get("nickname")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let title = note
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let desc = note.get("desc").and_then(Value::as_str).unwrap_or_default();
    let caption = [title, desc]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    // Milliseconds.
    let timestamp = note.get("time").and_then(Value::as_i64).map(|ms| ms / 1000);
    let mut files: Vec<(Url, &str, Option<&Value>)> = Vec::new();
    if note.get("type").and_then(Value::as_str) == Some("video") {
        let original = note
            .pointer("/video/consumer/originVideoKey")
            .and_then(Value::as_str)
            .filter(|key| !key.is_empty())
            .and_then(|key| https(&format!("{VIDEO_ROOT}/{key}")));
        let stream = note
            .pointer("/video/media/stream")
            .and_then(Value::as_object)
            .into_iter()
            .flat_map(|codecs| codecs.values())
            .filter_map(Value::as_array)
            .flatten()
            .filter(|s| s.get("masterUrl").and_then(Value::as_str).is_some())
            .max_by_key(|s| {
                s.get("width").and_then(Value::as_u64).unwrap_or(0)
                    * s.get("height").and_then(Value::as_u64).unwrap_or(0)
            })
            .and_then(|s| https(s["masterUrl"].as_str()?));
        if let Some(url) = original.or(stream) {
            files.push((url, "mp4", None));
        }
    } else {
        for image in note
            .get("imageList")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(url) = image
                .get("urlDefault")
                .or_else(|| image.get("url"))
                .and_then(Value::as_str)
                .and_then(https)
            {
                // Image URLs carry no extension; their default format is WebP.
                let ext = if url.path().contains("jpg") {
                    "jpg"
                } else {
                    "webp"
                };
                files.push((url, ext, Some(image)));
            }
        }
    }
    files
        .into_iter()
        .enumerate()
        .map(|(index, (url, ext, image))| {
            let num = index + 1;
            let mut keywords = Map::new();
            keywords.insert("id".into(), id.into());
            keywords.insert(
                "type".into(),
                note.get("type").cloned().unwrap_or(Value::Null),
            );
            keywords.insert("title".into(), title.into());
            keywords.insert("desc".into(), desc.into());
            keywords.insert("user".into(), user.clone());
            keywords.insert("date".into(), timestamp.into());
            keywords.insert("num".into(), num.into());
            for key in ["width", "height"] {
                if let Some(value) = image.and_then(|image| image.get(key)) {
                    keywords.insert(key.into(), value.clone());
                }
            }
            keywords.insert("filename".into(), format!("{id}_{num}").into());
            keywords.insert("extension".into(), ext.into());
            Item::new(url, keywords)
                .ids(id, format!("{id}_{num}"))
                .user(nickname.clone())
                .caption(Some(caption.clone()))
                .timestamp(timestamp)
                .header("Referer", &format!("{ROOT}/"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn matches_note_urls() {
        let id = |raw: &str| note_id(&Url::parse(raw).unwrap());
        assert_eq!(
            id("https://www.xiaohongshu.com/explore/6411CF99000000001300b6d9?xsec_token=a")
                .as_deref(),
            Some("6411cf99000000001300b6d9")
        );
        assert!(
            id("https://www.xiaohongshu.com/discovery/item/674051740000000007027a15").is_some()
        );
        for raw in [
            "https://www.xiaohongshu.com/explore",
            "https://www.xiaohongshu.com/explore/xyz",
            "https://www.xiaohongshu.com/user/profile/abc",
            "https://evil.test/explore/abc",
        ] {
            assert_eq!(id(raw), None, "{raw}");
        }
    }

    #[test]
    fn parses_javascript_state() {
        let state = initial_state(
            r#"<script>window.__INITIAL_STATE__={"a":undefined,"b":"undefined","c":[undefined]};</script>"#,
        )
        .unwrap();
        assert_eq!(state, json!({"a": null, "b": "undefined", "c": [null]}));
    }

    fn page(note: Value) -> Vec<u8> {
        let state = json!({"note": {"noteDetailMap": {"abc1": {"note": note}}}})
            .to_string()
            .replace("\"UNDEF\"", "undefined");
        html(&format!(
            "<script>window.__INITIAL_STATE__={state}</script>"
        ))
    }

    #[tokio::test]
    async fn extracts_image_and_video_notes() {
        let images = json!({"noteId": "abc1", "type": "normal", "title": "T", "desc": "D",
            "time": 1700000000000_i64, "user": {"nickname": "Neko", "userId": "u"},
            "lastUpdate": "UNDEF",
            "imageList": [
                {"urlDefault": "http://sns-webpic-qc.xhscdn.com/a/b!nd_dft_wlteh_webp_3",
                 "width": 1080, "height": 1440},
                {"urlDefault": "https://sns-webpic-qc.xhscdn.com/c/d!nd_dft_wlteh_jpg_3"}]});
        let video = json!({"noteId": "abc1", "type": "video", "user": {"nickname": "Neko"},
            "video": {"consumer": {"originVideoKey": "pre_post/xyz"},
                "media": {"stream": {"h264": [{"masterUrl": "https://v.example/low.mp4",
                    "width": 1, "height": 1}]}}}});
        let (root, server) = serve(vec![page(images), page(video), html("<html>404</html>")]);
        let http = http("xiaohongshu", root);
        let url =
            Url::parse("https://www.xiaohongshu.com/explore/abc1?xsec_token=T0K&xsec_source=pc")
                .unwrap();
        let files = media(extract(&http, url.clone())).await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].url.as_str(),
            "https://sns-webpic-qc.xhscdn.com/a/b!nd_dft_wlteh_webp_3"
        );
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
        assert_eq!(names, ["abc1_1.webp", "abc1_2.jpg"]);
        let k = keywords(&files[0]);
        assert_eq!(
            (k["width"].clone(), k["date"].clone()),
            (json!(1080), json!(1700000000))
        );
        let (_, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("xiaohongshu/Neko")
        );
        let files = media(extract(&http, url.clone())).await;
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].url.as_str(),
            "https://sns-video-bd.xhscdn.com/pre_post/xyz"
        );
        let events: Vec<_> = futures_util::StreamExt::collect(extract(&http, url)).await;
        let message = events[0].as_ref().err().unwrap().to_string();
        assert!(message.contains("xsec_token"), "{message}");
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with("GET /explore/abc1?xsec_token=T0K&xsec_source=pc "),
            "{}",
            requests[0]
        );
    }
}
