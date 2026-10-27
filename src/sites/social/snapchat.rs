//! Public Snapchat profiles (`snapchat.com/@USER`, `/add/USER`).
//!
//! gallery-dl has no Snapchat extractor; formats follow its conventions. The
//! profile page's `__NEXT_DATA__` lists the current public story, curated
//! highlights and Spotlight highlights with direct media URLs, so no login is
//! needed. Each story or highlight is one post; images are JPEG and videos
//! MP4. Single Spotlight URLs stay with the yt-dlp site.
use crate::error;
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

pub static SITE: Site = Site {
    category: "snapchat",
    name: "Snapchat",
    matches: |url| username(url).is_some(),
    extract,
    routes: &[&PROFILE],
    api_key: false,
    cookies: None,
};

static PROFILE: Route = Route::new(
    "profile",
    "{id}.{extension}",
    &["{category}", "{username}", "{type}"],
    "{id}",
);

const ROOT: &str = "https://www.snapchat.com";

/// The profile name of `@USER` and `add/USER` URLs.
fn username(url: &Url) -> Option<String> {
    if !crate::sites::plain(url) || !matches!(url.host_str()?, "www.snapchat.com" | "snapchat.com")
    {
        return None;
    }
    let mut segments = url.path().split('/').filter(|s| !s.is_empty());
    let name = match (segments.next()?, segments.next()) {
        (first, None) => first.strip_prefix('@')?,
        ("add", Some(name)) => name,
        _ => return None,
    };
    if segments.next().is_some() {
        return None;
    }
    (!name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')))
    .then(|| name.to_ascii_lowercase())
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(name) = username(&url) else {
        return crate::sites::fail(error("unsupported Snapchat URL"));
    };
    crate::sites::once(async move {
        let page = http.api(&format!("{ROOT}/@{name}"))?;
        let html = http.text_with(&page, &http.browser_headers(&[])).await?;
        let data = crate::sites::gdl::nextdata(&html)
            .map_err(|_| error("Snapchat profile page has no profile data"))?;
        let props = data
            .pointer("/props/pageProps")
            .ok_or_else(|| error("Snapchat profile page has no profile data"))?;
        let profile = props
            .pointer("/userProfile/publicProfileInfo")
            .filter(|p| p.is_object())
            .ok_or_else(|| error("Snapchat profile not found or not public"))?;
        let mut events = Vec::new();
        for (kind, stories) in [
            ("story", props.get("story").map(std::slice::from_ref)),
            (
                "highlight",
                props
                    .get("curatedHighlights")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice),
            ),
            (
                "spotlight",
                props
                    .get("spotlightHighlights")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice),
            ),
        ] {
            for story in stories.into_iter().flatten() {
                let items = story_items(story, kind, profile, &name);
                events.extend(crate::sites::post(&SITE, http, &PROFILE, items));
            }
        }
        Ok(crate::sites::events(events))
    })
}

fn value_of(value: Option<&Value>) -> Option<String> {
    let value = value?;
    value
        .get("value")
        .unwrap_or(value)
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// One story's or highlight's snaps.
fn story_items(story: &Value, kind: &str, profile: &Value, name: &str) -> Vec<Item> {
    let story_id = value_of(story.get("storyId"))
        .or_else(|| value_of(story.get("highlightId")))
        .unwrap_or_else(|| format!("{name}_{kind}"));
    let title = value_of(story.get("storyTitle"));
    let mut items = Vec::new();
    for snap in story
        .get("snapList")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(url) = snap
            .pointer("/snapUrls/mediaUrl")
            .and_then(Value::as_str)
            .and_then(|raw| Url::parse(raw).ok())
            .filter(|url| url.scheme() == "https" && url.host_str().is_some())
        else {
            continue;
        };
        let index = snap.get("snapIndex").and_then(Value::as_u64).unwrap_or(0);
        let id = value_of(snap.get("snapId")).unwrap_or_else(|| format!("{story_id}_{index}"));
        let video = snap.get("snapMediaType").and_then(Value::as_u64) == Some(1);
        let timestamp = value_of(snap.get("timestampInSec")).and_then(|t| t.parse().ok());
        let mut keywords = Map::new();
        keywords.insert("id".into(), id.clone().into());
        keywords.insert("username".into(), name.into());
        keywords.insert(
            "title".into(),
            profile.get("title").cloned().unwrap_or(Value::Null),
        );
        keywords.insert("type".into(), kind.into());
        keywords.insert("story_id".into(), story_id.clone().into());
        keywords.insert("story_title".into(), title.clone().into());
        keywords.insert("snap_index".into(), index.into());
        keywords.insert("date".into(), timestamp.into());
        keywords.insert("filename".into(), id.clone().into());
        keywords.insert("extension".into(), if video { "mp4" } else { "jpg" }.into());
        items.push(
            Item::new(url, keywords)
                .ids(story_id.clone(), id)
                .user(Some(name.to_owned()))
                .caption(title.clone())
                .timestamp(timestamp),
        );
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn matches_public_profile_urls() {
        let name = |raw: &str| username(&Url::parse(raw).unwrap());
        assert_eq!(
            name("https://www.snapchat.com/@NASA").as_deref(),
            Some("nasa")
        );
        assert_eq!(
            name("https://snapchat.com/add/nasa?share=1").as_deref(),
            Some("nasa")
        );
        for raw in [
            "https://www.snapchat.com/",
            "https://www.snapchat.com/spotlight/W7_EDlXWTBi",
            "https://www.snapchat.com/@nasa/spotlight/x",
            "https://www.snapchat.com/add/",
            "https://evil.test/@nasa",
        ] {
            assert_eq!(name(raw), None, "{raw}");
        }
    }

    #[tokio::test]
    async fn extracts_story_highlights_and_spotlight() {
        let snap = |id: &str, kind: u64, url: &str| {
            json!({"snapIndex": 0, "snapId": {"value": id}, "snapMediaType": kind,
                "timestampInSec": {"value": "1700000000"},
                "snapUrls": {"mediaUrl": url}})
        };
        let props = json!({"props": {"pageProps": {
            "userProfile": {"publicProfileInfo": {"username": "nasa", "title": "NASA"}},
            "story": {"storyId": {"value": "s1"}, "snapList": [
                snap("", 0, "https://cf-st.sc-cdn.net/d/a.1")]},
            "curatedHighlights": [{"highlightId": {"value": "h1"},
                "storyTitle": {"value": "Moon"}, "snapList": [
                    snap("", 1, "https://cf-st.sc-cdn.net/d/b.1"),
                    snap("", 1, "http://insecure.example/c")]}],
            "spotlightHighlights": [{"storyId": {"value": "sp"}, "snapList": [
                snap("W7_x", 1, "https://cf-st.sc-cdn.net/d/c.1")]}]}}});
        let page = format!(
            "<html><script id=\"__NEXT_DATA__\" type=\"application/json\">{props}</script></html>"
        );
        let (root, server) = serve(vec![html(&page)]);
        let http = http("snapchat", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.snapchat.com/add/NASA").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 3);
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
        assert_eq!(names, ["s1_0.jpg", "h1_0.mp4", "W7_x.mp4"]);
        let k = keywords(&files[1]);
        assert_eq!(
            (k["type"].clone(), k["story_title"].clone()),
            (json!("highlight"), json!("Moon"))
        );
        assert_eq!(k["date"], 1700000000);
        let (_, dir) = crate::sites::default_templates(&files[2]).unwrap();
        assert_eq!(
            dir.render(&files[2]).unwrap().unwrap(),
            std::path::PathBuf::from("snapchat/nasa/spotlight")
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /@nasa "), "{}", requests[0]);
    }

    #[tokio::test]
    async fn missing_profiles_are_errors() {
        let (root, _server) = serve(vec![html("<html>nothing</html>")]);
        let http = http("snapchat", root);
        let events: Vec<_> = futures_util::StreamExt::collect(extract(
            &http,
            Url::parse("https://www.snapchat.com/@nobody").unwrap(),
        ))
        .await;
        let message = events[0].as_ref().err().unwrap().to_string();
        assert!(message.contains("no profile data"), "{message}");
    }
}
