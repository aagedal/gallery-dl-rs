//! Audiochan audio, users, collections and searches, mirroring
//! gallery_dl/extractor/audiochan.py through `api.audiochan.com`.
use crate::error;
use crate::sites::gdl::{
    captures, date, group, nameext_from_name, parse_iso, parse_query, re, str_of,
};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const API: &str = "https://api.audiochan.com";
const API_HEADERS: &[(&str, &str)] = &[
    ("content-type", "application/json"),
    ("Origin", "https://audiochan.com"),
    ("Sec-Fetch-Dest", "empty"),
    ("Sec-Fetch-Mode", "cors"),
    ("Sec-Fetch-Site", "same-site"),
];
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

#[derive(Debug, PartialEq)]
enum Target {
    Audio(String),
    User(String),
    Collection(String),
    Search(String),
}

fn target(url: &Url) -> Option<Target> {
    const BASE: &str = r"^https?://(?:www\.)?audiochan\.com";
    if let Some(c) = captures(re!(&format!(r"{BASE}/a/([^/?#]+)")), url) {
        return Some(Target::Audio(group(&c, 1).into()));
    }
    if let Some(c) = captures(re!(&format!(r"{BASE}/u/([^/?#]+)")), url) {
        return Some(Target::User(group(&c, 1).into()));
    }
    if let Some(c) = captures(re!(&format!(r"{BASE}/c/([^/?#]+)")), url) {
        return Some(Target::Collection(group(&c, 1).into()));
    }
    captures(re!(&format!(r"{BASE}/search/?\?([^#]+)")), url)
        .map(|c| Target::Search(group(&c, 1).into()))
}

async fn api(http: &Http, endpoint: &str, params: &[(String, String)]) -> crate::Result<Value> {
    let mut url = http.api(&format!("{API}{endpoint}"))?;
    if !params.is_empty() {
        url.query_pairs_mut().extend_pairs(params.iter());
    }
    http.json_with(&url, API_HEADERS).await
}

/// `_extract_description`: the text nodes of a rich-text document.
fn description(node: &Value, texts: &mut Vec<Value>) {
    if let Some(text) = node.get("text") {
        texts.push(text.clone());
    } else if let Some(Value::Array(content)) = node.get("content") {
        for child in content {
            description(child, texts);
        }
    }
}

fn audio_item(post: &Value, with_user: bool, extra: &Map<String, Value>) -> Option<Item> {
    let mut post = post.as_object()?.clone();
    let file = post.get("audioFile")?.clone();
    post.insert("date".into(), date(parse_iso(str_of(&file, "created_at"))));
    post.insert(
        "date_updated".into(),
        date(parse_iso(str_of(&file, "updated_at"))),
    );
    let mut texts = Vec::new();
    description(post.get("description").unwrap_or(&Value::Null), &mut texts);
    post.insert("description".into(), texts.into());
    let tags: Vec<String> = post
        .get("tags")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|tag| {
            let tag = tag.get("tag").unwrap_or(tag);
            format!(
                "{}:{}",
                crate::sites::scalar(&tag["category"]),
                crate::sites::scalar(&tag["name"])
            )
        })
        .collect();
    post.insert("tags".into(), tags.into());
    if with_user {
        let user = post
            .get("credits")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("user"))
            .cloned()
            .unwrap_or(Value::Null);
        post.insert("user".into(), user);
    }
    post.extend(extra.clone());
    let url = match str_of(&file, "url") {
        "" => str_of(&file, "stream_url"),
        url => url,
    };
    let (name, ext) = nameext_from_name(str_of(&file, "filename"));
    post.insert("filename".into(), name.into());
    post.insert("extension".into(), ext.into());
    let id = crate::sites::scalar(&file["id"]);
    let mut item = Item::new(Url::parse(url).ok()?, post).ids(id.clone(), id);
    for (name, value) in [
        (
            "Accept",
            "audio/webm,audio/ogg,audio/wav,audio/*;q=0.9,application/ogg;q=0.7,video/*;q=0.6,*/*;q=0.5",
        ),
        ("Sec-Fetch-Dest", "audio"),
        ("Sec-Fetch-Mode", "no-cors"),
        ("Sec-Fetch-Site", "same-site"),
        ("Accept-Encoding", "identity"),
    ] {
        item = item.header(name, value);
    }
    Some(item)
}

/// `_pagination` with `page`/`limit` parameters until `has_more` is false.
fn paginate<'a>(
    http: &'a Http,
    route: &'static Route,
    endpoint: String,
    params: Vec<(String, String)>,
    key: Option<&'static str>,
    with_user: bool,
    extra: Map<String, Value>,
) -> Events<'a> {
    crate::sites::paginate(Some(1i64), move |state| {
        let endpoint = endpoint.clone();
        let mut params = params.clone();
        let extra = extra.clone();
        async move {
            let Some(page) = state else { return Ok(None) };
            params.push(("page".into(), page.to_string()));
            params.push(("limit".into(), "12".into()));
            let mut data = api(http, &endpoint, &params).await?;
            if let Some(key) = key {
                data = data[key].clone();
            }
            let events = data["data"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|post| audio_item(post, with_user, &extra))
                .flat_map(|item| crate::sites::post(&SITE, http, route, vec![item]))
                .collect();
            let next = crate::sites::gdl::truthy(data.get("has_more")).then_some(page + 1);
            Ok(Some((crate::sites::events(events), next)))
        }
    })
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Audiochan URL"));
    };
    match target {
        Target::Audio(slug) => crate::sites::once(async move {
            let audio = api(http, &format!("/audios/slug/{slug}"), &[]).await?;
            let items = audio_item(&audio, true, &Map::new()).into_iter().collect();
            Ok(crate::sites::events(crate::sites::post(
                &SITE, http, &AUDIO, items,
            )))
        }),
        Target::User(user) => crate::sites::once(async move {
            let endpoint = format!("/users/{user}");
            let mut extra = Map::new();
            extra.insert(
                "user".into(),
                api(http, &endpoint, &[]).await?["data"].clone(),
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
                extra,
            ))
        }),
        Target::Collection(slug) => crate::sites::once(async move {
            let mut collection = api(http, &format!("/collections/{slug}"), &[]).await?;
            if let Some(map) = collection.as_object_mut() {
                map.remove("audios");
                map.remove("items");
            }
            let mut extra = Map::new();
            extra.insert("collection".into(), collection);
            Ok(paginate(
                http,
                &COLLECTION,
                format!("/collections/slug/{slug}/items"),
                Vec::new(),
                None,
                false,
                extra,
            ))
        }),
        Target::Search(query) => {
            let mut params: Vec<(String, String)> = parse_query(&query)
                .into_iter()
                .map(|(k, v)| (k, crate::sites::scalar(&v)))
                .collect();
            let q = params
                .iter()
                .find(|(k, _)| k == "q")
                .map(|(_, v)| Value::from(v.as_str()))
                .unwrap_or(Value::Null);
            params.retain(|(k, _)| k != "sfw_only");
            params.push(("sfw_only".into(), "false".into()));
            let mut extra = Map::new();
            extra.insert("search_tags".into(), q);
            paginate(
                http,
                &SEARCH,
                "/search".into(),
                params,
                Some("audios"),
                true,
                extra,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http as client, json, keywords, media, serve};
    use serde_json::json;

    fn post(id: i64) -> Value {
        json!({"title": "Cozy", "slug": format!("s{id}"),
            "description": {"content": [{"content": [{"text": "line one"}, {"text": "two"}]}]},
            "tags": [{"tag": {"category": "genre", "name": "asmr"}}, {"category": "mood", "name": "calm"}],
            "credits": [{"user": {"display_name": "Lil"}}],
            "audioFile": {"id": id, "url": "", "stream_url": format!("https://cdn.audiochan.com/{id}.m3u8"), "filename": "cozy.mp3",
                "created_at": "2025-01-02T03:04:05Z", "updated_at": "2025-01-03T03:04:05Z"}})
    }

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://audiochan.com/a/pBP1V1ODEV2od9CjLu"),
            Some(Target::Audio("pBP1V1ODEV2od9CjLu".into()))
        );
        assert_eq!(
            t("https://audiochan.com/search?q=Cozy&sort=trending"),
            Some(Target::Search("q=Cozy&sort=trending".into()))
        );
        assert_eq!(
            t("https://audiochan.com/c/qzr"),
            Some(Target::Collection("qzr".into()))
        );
    }

    #[tokio::test]
    async fn searches_audio() {
        let (root, server) = serve(vec![
            json(json!({"audios": {"data": [post(1)], "has_more": true}})),
            json(json!({"audios": {"data": [post(2)], "has_more": false}})),
        ]);
        let http = client("audiochan", root);
        let files = media(extract(
            &http,
            Url::parse("https://audiochan.com/search?q=Cozy").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].url.as_str(), "https://cdn.audiochan.com/1.m3u8");
        assert_eq!(files[0].headers["Sec-Fetch-Dest"], "audio");
        let k = keywords(&files[0]);
        assert_eq!(k["tags"], json!(["genre:asmr", "mood:calm"]));
        assert_eq!(k["description"], json!(["line one", "two"]));
        assert_eq!(k["search_tags"], "Cozy");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "Cozy (s1).mp3");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("audiochan/Lil")
        );
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with("GET /search?q=Cozy&sfw_only=false&page=1&limit=12 "),
            "{}",
            requests[0]
        );
        assert!(requests[1].contains("page=2"));
    }
}
