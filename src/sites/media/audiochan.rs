//! Audiochan audio posts, users, collections and searches, mirroring
//! gallery_dl/extractor/audiochan.py through `api.audiochan.com`.
//!
//! Files without a direct `url` use their `stream_url`; upstream's segmented
//! download of those is not ported.
use super::{date, iso_ts, path_segment, set_param, str_of};
use crate::sites::art::{nameext_from_name, update};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

const API: &str = "https://api.audiochan.com";
const FILENAME: &str = "{title} ({slug}).{extension}";
const DIRECTORY: &[&str] = &["{category}", "{user[display_name]}"];
const ARCHIVE: &str = "{audioFile[id]}";
static AUDIO: Route = Route::new("audio", FILENAME, DIRECTORY, ARCHIVE);
static USER: Route = Route::new("user", FILENAME, DIRECTORY, ARCHIVE);
static COLLECTION: Route = Route::new("collection", FILENAME, DIRECTORY, ARCHIVE);
static SEARCH: Route = Route::new("search", FILENAME, DIRECTORY, ARCHIVE);

pub static SITE: Site = Site {
    category: "audiochan",
    name: "Audiochan",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&AUDIO, &USER, &COLLECTION, &SEARCH],
    api_key: false,
    cookies: None,
};

const API_HEADERS: &[(&str, &str)] = &[
    ("content-type", "application/json"),
    ("Origin", "https://audiochan.com"),
    ("Sec-Fetch-Dest", "empty"),
    ("Sec-Fetch-Mode", "cors"),
    ("Sec-Fetch-Site", "same-site"),
];

const DOWNLOAD_HEADERS: &[(&str, &str)] = &[
    (
        "Accept",
        "audio/webm,audio/ogg,audio/wav,audio/*;q=0.9,application/ogg;q=0.7,video/*;q=0.6,*/*;q=0.5",
    ),
    ("Sec-Fetch-Dest", "audio"),
    ("Sec-Fetch-Mode", "no-cors"),
    ("Sec-Fetch-Site", "same-site"),
    ("Accept-Encoding", "identity"),
];

#[derive(Debug, PartialEq)]
enum Target {
    Audio(String),
    User(String),
    Collection(String),
    Search(String),
}

/// `(?:www\.)?audiochan\.com` with `/(a|u|c)/([^/?#]+)` or
/// `/search/?\?([^#]+)`.
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::art::on_host(url, &["audiochan.com"]) {
        return None;
    }
    let path = url.path();
    if matches!(path, "/search" | "/search/") {
        let query = url.query().filter(|q| !q.is_empty())?;
        return Some(Target::Search(query.into()));
    }
    let parts: Vec<&str> = path[1..].split('/').collect();
    match parts.as_slice() {
        ["a", slug, ..] if path_segment(slug) => Some(Target::Audio((*slug).into())),
        ["u", user, ..] if path_segment(user) => Some(Target::User((*user).into())),
        ["c", slug, ..] if path_segment(slug) => Some(Target::Collection((*slug).into())),
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Audiochan URL"));
    };
    match target {
        Target::Audio(slug) => crate::sites::once(async move {
            let audio = request_api(http, &format!("/audios/slug/{slug}"), &[]).await?;
            Ok(posts_events(http, &AUDIO, vec![audio], true, &Map::new()))
        }),
        Target::User(user) => crate::sites::once(async move {
            let endpoint = format!("/users/{user}");
            let profile = request_api(http, &endpoint, &[]).await?;
            let mut kwdict = Map::new();
            kwdict.insert(
                "user".into(),
                profile.get("data").cloned().unwrap_or(Value::Null),
            );
            let params = vec![
                ("sfw_only".into(), "false".into()),
                ("sort".into(), "new".into()),
            ];
            Ok(paginate(
                http,
                &USER,
                format!("{endpoint}/audios"),
                params,
                None,
                false,
                kwdict,
            ))
        }),
        Target::Collection(slug) => crate::sites::once(async move {
            let mut collection = request_api(http, &format!("/collections/{slug}"), &[]).await?;
            if let Value::Object(collection) = &mut collection {
                collection.remove("audios");
                collection.remove("items");
            }
            let mut kwdict = Map::new();
            kwdict.insert("collection".into(), collection);
            let endpoint = format!("/collections/slug/{slug}/items");
            Ok(paginate(
                http,
                &COLLECTION,
                endpoint,
                Vec::new(),
                None,
                false,
                kwdict,
            ))
        }),
        Target::Search(query) => {
            let mut params = super::parse_query(Some(&query));
            set_param(&mut params, "sfw_only", "false".into());
            let mut kwdict = Map::new();
            let tags = super::query_get(&params, "q").map_or(Value::Null, Value::from);
            kwdict.insert("search_tags".into(), tags);
            paginate(
                http,
                &SEARCH,
                "/search".into(),
                params,
                Some("audios"),
                true,
                kwdict,
            )
        }
    }
}

async fn request_api(http: &Http, endpoint: &str, params: &[(String, String)]) -> Result<Value> {
    let mut url = http.api(&format!("{API}{endpoint}"))?;
    if !params.is_empty() {
        url.query_pairs_mut().extend_pairs(params);
    }
    http.json_with(&url, API_HEADERS)
        .await
        .map_err(|e| error(format!("Audiochan API request failed: {e}")))
}

/// `_pagination`: pages of 12 until `has_more` is false; search results
/// are nested under `key`.
fn paginate<'a>(
    http: &'a Http,
    route: &'static Route,
    endpoint: String,
    mut params: Vec<(String, String)>,
    key: Option<&'static str>,
    credits: bool,
    kwdict: Map<String, Value>,
) -> Events<'a> {
    set_param(&mut params, "page", "1".into());
    set_param(&mut params, "limit", "12".into());
    crate::sites::paginate(Some(1u64), move |page| {
        let endpoint = endpoint.clone();
        let mut params = params.clone();
        let kwdict = kwdict.clone();
        async move {
            let Some(page) = page else {
                return Ok(None);
            };
            set_param(&mut params, "page", page.to_string());
            let mut data = request_api(http, &endpoint, &params).await?;
            if let Some(key) = key {
                data = data.get(key).cloned().unwrap_or(Value::Null);
            }
            let posts = data
                .get("data")
                .and_then(Value::as_array)
                .cloned()
                .ok_or_else(|| error("Audiochan API response has no data"))?;
            let more = crate::sites::art::truthy(data.get("has_more"));
            let next = (more && !posts.is_empty() && page < 100_000).then_some(page + 1);
            Ok(Some((
                posts_events(http, route, posts, credits, &kwdict),
                next,
            )))
        }
    })
}

/// `_extract_description`: the `text` leaves of a rich-text document.
fn description(node: &Value, texts: &mut Vec<Value>) {
    if let Some(text) = node.get("text") {
        texts.push(text.clone());
    } else if let Some(Value::Array(content)) = node.get("content") {
        for child in content {
            description(child, texts);
        }
    }
}

/// `items()`: one post per audio file, extractor keywords applied last.
fn posts_events<'a>(
    http: &'a Http,
    route: &'static Route,
    posts: Vec<Value>,
    credits: bool,
    kwdict: &Map<String, Value>,
) -> Events<'a> {
    let mut events = Vec::new();
    for post in posts {
        let Value::Object(mut post) = post else {
            continue;
        };
        let file = post.get("audioFile").cloned().unwrap_or(Value::Null);
        let ts = iso_ts(str_of(&file, "created_at"));
        post.insert("date".into(), date(ts));
        let updated = iso_ts(str_of(&file, "updated_at"));
        post.insert("date_updated".into(), date(updated));
        let mut texts = Vec::new();
        if let Some(node) = post.get("description") {
            description(node, &mut texts);
        }
        post.insert("description".into(), texts.into());
        let tags: Vec<Value> = post
            .get("tags")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|tag| {
                let tag = tag.get("tag").unwrap_or(tag);
                let text = |key| tag.get(key).map(crate::sites::scalar).unwrap_or_default();
                format!("{}:{}", text("category"), text("name")).into()
            })
            .collect();
        post.insert("tags".into(), tags.into());
        if credits {
            let user = post
                .get("credits")
                .and_then(|credits| credits.get(0)?.get("user"))
                .cloned();
            post.insert("user".into(), user.unwrap_or(Value::Null));
        }
        let raw = match str_of(&file, "url") {
            "" => str_of(&file, "stream_url"),
            url => url,
        };
        let Some(url) = crate::sites::media_url(raw, &["audiochan.com"]) else {
            continue;
        };
        super::set_nameext(&mut post, nameext_from_name(str_of(&file, "filename")));
        update(&mut post, kwdict);
        let id = file.get("id").map(crate::sites::scalar).unwrap_or_default();
        let slug = post
            .get("slug")
            .map(crate::sites::scalar)
            .unwrap_or_default();
        let user = post
            .get("user")
            .map(|user| str_of(user, "display_name").to_owned());
        let title = post.get("title").and_then(Value::as_str).map(str::to_owned);
        let mut item = Item::new(url, post)
            .ids(slug, id)
            .user(user)
            .caption(title)
            .timestamp(ts);
        for (name, value) in DOWNLOAD_HEADERS {
            item = item.header(name, value);
        }
        events.extend(crate::sites::post(&SITE, http, route, vec![item]));
    }
    crate::sites::events(events)
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
            t("https://audiochan.com/a/my-song"),
            Some(Target::Audio("my-song".into()))
        );
        assert_eq!(
            t("https://www.audiochan.com/u/someone/x"),
            Some(Target::User("someone".into()))
        );
        assert_eq!(
            t("https://audiochan.com/c/best-of"),
            Some(Target::Collection("best-of".into()))
        );
        assert_eq!(
            t("https://audiochan.com/search/?q=rain&sort=new"),
            Some(Target::Search("q=rain&sort=new".into()))
        );
        for raw in [
            "https://audiochan.com/",
            "https://audiochan.com/a/",
            "https://audiochan.com/search",
            "https://audiochan.com/search/x?q=1",
            "https://api.audiochan.com/a/x",
            "https://audiochan.com.evil.test/a/x",
            "https://evilaudiochan.com/a/x",
            "https://audiochan.com:8443/a/x",
            "https://u:p@audiochan.com/a/x",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn audio(n: u32, url: &str) -> Value {
        json!({"id": format!("a{n}"), "slug": format!("song-{n}"), "title": format!("Song {n}"),
            "description": {"type": "doc", "content": [
                {"type": "paragraph", "content": [{"type": "text", "text": "Hello"}]},
                {"type": "paragraph", "content": [{"type": "text", "text": "World"}]}]},
            "tags": [{"tag": {"category": "genre", "name": "rain"}}, {"category": "mood", "name": "calm"}],
            "credits": [{"user": {"display_name": "Credit", "id": 1}}],
            "audioFile": {"id": 100 + n, "filename": format!("Song {n}.MP3"), "url": url,
                "stream_url": format!("https://cdn.audiochan.com/stream/{n}.m3u8"),
                "created_at": "2025-01-02T03:04:05.000Z", "updated_at": "2025-01-03T00:00:00Z"}})
    }

    #[tokio::test]
    async fn audio_post() {
        let (root, server) = serve(vec![json(audio(1, "https://cdn.audiochan.com/a/1.mp3"))]);
        let http = http("audiochan", root);
        let files = media(extract(
            &http,
            Url::parse("https://audiochan.com/a/song-1").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].url.as_str(), "https://cdn.audiochan.com/a/1.mp3");
        assert_eq!(files[0].headers["Sec-Fetch-Dest"], "audio");
        assert_eq!(files[0].headers["Accept-Encoding"], "identity");
        let k = keywords(&files[0]);
        assert_eq!(k["description"], json!(["Hello", "World"]));
        assert_eq!(k["tags"], json!(["genre:rain", "mood:calm"]));
        assert_eq!(k["user"]["display_name"], "Credit");
        assert_eq!(k["date"], "2025-01-02 03:04:05");
        assert_eq!(k["date_updated"], "2025-01-03 00:00:00");
        assert_eq!(k["filename"], "Song 1");
        assert_eq!(k["extension"], "mp3");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "Song 1 (song-1).mp3");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("audiochan/Credit")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{audioFile[id]}",["101"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /audios/slug/song-1 "));
        assert!(requests[0].contains("origin: https://audiochan.com"));
    }

    #[tokio::test]
    async fn user_pages_and_overrides_user() {
        let (root, server) = serve(vec![
            json(json!({"data": {"display_name": "Owner", "id": 9}})),
            json(json!({"data": [audio(1, "")], "has_more": true})),
            json(
                json!({"data": [audio(2, "https://cdn.audiochan.com/a/2.mp3")], "has_more": false}),
            ),
        ]);
        let http = http("audiochan", root);
        let files = media(extract(
            &http,
            Url::parse("https://audiochan.com/u/owner").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].url.as_str(),
            "https://cdn.audiochan.com/stream/1.m3u8"
        );
        let (_, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("audiochan/Owner")
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /users/owner "));
        assert!(
            requests[1]
                .starts_with("GET /users/owner/audios?sfw_only=false&sort=new&page=1&limit=12 "),
            "{}",
            requests[1]
        );
        assert!(requests[2].contains("&page=2&"));
    }

    #[tokio::test]
    async fn collection_and_search() {
        let (root, server) = serve(vec![
            json(json!({"id": 5, "title": "Best", "audios": [], "items": []})),
            json(
                json!({"data": [audio(3, "https://cdn.audiochan.com/a/3.mp3")], "has_more": false}),
            ),
            json(
                json!({"audios": {"data": [audio(4, "https://cdn.audiochan.com/a/4.mp3")],
                "has_more": false}}),
            ),
        ]);
        let http = http("audiochan", root);
        let files = media(extract(
            &http,
            Url::parse("https://audiochan.com/c/best").unwrap(),
        ))
        .await;
        let k = keywords(&files[0]);
        assert_eq!(k["collection"], json!({"id": 5, "title": "Best"}));
        assert!(!k.contains_key("user") || k["user"].is_object());
        let files = media(extract(
            &http,
            Url::parse("https://audiochan.com/search?q=rain+day").unwrap(),
        ))
        .await;
        let k = keywords(&files[0]);
        assert_eq!(k["search_tags"], "rain day");
        assert_eq!(k["subcategory"], "search");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /collections/best "));
        assert!(requests[1].starts_with("GET /collections/slug/best/items?page=1&limit=12 "));
        assert!(
            requests[2].starts_with("GET /search?q=rain+day&sfw_only=false&page=1&limit=12 "),
            "{}",
            requests[2]
        );
    }
}
