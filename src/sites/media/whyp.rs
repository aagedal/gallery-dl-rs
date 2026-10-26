//! Whyp tracks, users and collections, mirroring
//! gallery_dl/extractor/whyp.py through `api.whyp.it`.
use crate::error;
use crate::sites::gdl::{captures, date, group, nameext_into, parse_iso, parse_query, re, str_of};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const API: &str = "https://api.whyp.it";
const HEADERS: &[(&str, &str)] = &[
    ("Accept", "application/json"),
    ("Origin", "https://whyp.it"),
    ("Referer", "https://whyp.it/"),
    ("Sec-Fetch-Dest", "empty"),
    ("Sec-Fetch-Mode", "cors"),
    ("Sec-Fetch-Site", "same-site"),
];
const FILENAME: &str = "{id} {title}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{user[username]} ({user[id]})"];

static AUDIO: Route = Route::new("audio", FILENAME, DIRECTORY, "{id}");
static USER: Route = Route::new("user", FILENAME, DIRECTORY, "{id}");
static COLLECTION: Route = Route::new("collection", FILENAME, DIRECTORY, "{id}");

pub static SITE: Site = Site {
    category: "whyp",
    name: "Whyp",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&AUDIO, &USER, &COLLECTION],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Audio(String, Option<String>),
    User(String, Option<String>),
    Collection(String, Option<String>),
}

fn target(url: &Url) -> Option<Target> {
    let suffix = r"/(\d+)(?:/[^/?#]+)?/?(?:\?([^#]+))?";
    let opt = |c: &regex::Captures<'_>| c.get(2).map(|m| m.as_str().to_owned());
    if let Some(c) = captures(
        re!(&format!(r"^https?://(?:www\.)?whyp\.it/tracks{suffix}")),
        url,
    ) {
        return Some(Target::Audio(group(&c, 1).into(), opt(&c)));
    }
    if let Some(c) = captures(
        re!(&format!(r"^https?://(?:www\.)?whyp\.it/users{suffix}")),
        url,
    ) {
        return Some(Target::User(group(&c, 1).into(), opt(&c)));
    }
    let c = captures(
        re!(&format!(
            r"^https?://(?:www\.)?whyp\.it/collections{suffix}"
        )),
        url,
    )?;
    Some(Target::Collection(group(&c, 1).into(), opt(&c)))
}

async fn api(http: &Http, path: &str, params: &[(String, String)]) -> crate::Result<Value> {
    let mut url = http.api(&format!("{API}{path}"))?;
    if !params.is_empty() {
        url.query_pairs_mut().extend_pairs(params.iter());
    }
    http.json_with(&url, HEADERS).await
}

fn query(qs: Option<&str>) -> Vec<(String, String)> {
    qs.map(|q| {
        parse_query(q)
            .into_iter()
            .map(|(k, v)| (k, crate::sites::scalar(&v)))
            .collect()
    })
    .unwrap_or_default()
}

fn track_item(track: &Value, extra: &Map<String, Value>) -> Option<Item> {
    let mut track = track.as_object()?.clone();
    let (url, original) = match track
        .get("lossless_url")
        .and_then(Value::as_str)
        .filter(|u| !u.is_empty())
    {
        Some(url) => (url.to_owned(), true),
        None => (
            str_of(&Value::Object(track.clone()), "lossy_url").to_owned(),
            false,
        ),
    };
    track.insert("original".into(), original.into());
    if let Some(created) = track.get("created_at").map(crate::sites::scalar) {
        track.insert("date".into(), date(parse_iso(&created)));
    }
    track.extend(extra.clone());
    nameext_into(&url, &mut track);
    let id = track
        .get("id")
        .map(crate::sites::scalar)
        .unwrap_or_default();
    Some(Item::new(Url::parse(&url).ok()?, track).ids(id.clone(), id))
}

fn events<'a>(
    http: &'a Http,
    route: &'static Route,
    tracks: &[Value],
    extra: &Map<String, Value>,
) -> Events<'a> {
    let events = tracks
        .iter()
        .filter_map(|t| track_item(t, extra))
        .flat_map(|item| crate::sites::post(&SITE, http, route, vec![item]))
        .collect();
    crate::sites::events(events)
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Whyp URL"));
    };
    match target {
        Target::Audio(id, qs) => crate::sites::once(async move {
            let data = api(http, &format!("/api/tracks/{id}"), &query(qs.as_deref())).await?;
            Ok(events(http, &AUDIO, &[data["track"].clone()], &Map::new()))
        }),
        Target::Collection(id, qs) => crate::sites::once(async move {
            let collection = api(
                http,
                &format!("/api/collections/{id}"),
                &query(qs.as_deref()),
            )
            .await?["collection"]
                .clone();
            let token = crate::sites::scalar(&collection["token"]);
            let data = api(
                http,
                &format!("/api/collections/{id}/tracks"),
                &[("token".into(), token)],
            )
            .await?;
            let mut extra = Map::new();
            extra.insert("collection".into(), collection);
            Ok(events(
                http,
                &COLLECTION,
                data["tracks"].as_array().map_or(&[][..], Vec::as_slice),
                &extra,
            ))
        }),
        Target::User(id, qs) => crate::sites::paginate(Some(query(qs.as_deref())), move |state| {
            let id = id.clone();
            async move {
                let Some(mut params) = state else {
                    return Ok(None);
                };
                let data = api(http, &format!("/api/users/{id}/tracks"), &params).await?;
                let page = events(
                    http,
                    &USER,
                    data["tracks"].as_array().map_or(&[][..], Vec::as_slice),
                    &Map::new(),
                );
                let next = data
                    .get("next_cursor")
                    .filter(|c| crate::sites::gdl::truthy(Some(c)))
                    .map(|cursor| {
                        params.retain(|(k, _)| k != "cursor");
                        params.push(("cursor".into(), crate::sites::scalar(cursor)));
                        params
                    });
                Ok(Some((page, next)))
            }
        }),
    }
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
            t("https://whyp.it/tracks/12345/slug?token=tokenstring"),
            Some(Target::Audio(
                "12345".into(),
                Some("token=tokenstring".into())
            ))
        );
        assert_eq!(
            t("https://whyp.it/users/1/brad"),
            Some(Target::User("1".into(), None))
        );
        assert_eq!(
            t("https://whyp.it/collections/1/example"),
            Some(Target::Collection("1".into(), None))
        );
    }

    #[tokio::test]
    async fn user_follows_cursors() {
        let track = |id: i64, lossless: bool| {
            json!({"id": id, "title": "T", "created_at": "2024-01-02T03:04:05Z",
            "user": {"id": 1, "username": "brad"},
            "lossless_url": if lossless { json!("https://cdn.whyp.it/a.flac") } else { Value::Null },
            "lossy_url": "https://cdn.whyp.it/a.mp3"})
        };
        let (root, server) = serve(vec![
            json(json!({"tracks": [track(1, true)], "next_cursor": "C2"})),
            json(json!({"tracks": [track(2, false)], "next_cursor": null})),
        ]);
        let http = client("whyp", root);
        let files = media(extract(
            &http,
            Url::parse("https://whyp.it/users/1/brad").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].url.as_str(), "https://cdn.whyp.it/a.flac");
        assert_eq!(keywords(&files[1])["original"], false);
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "1 T.flac");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("whyp/brad (1)")
        );
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /api/users/1/tracks?cursor=C2 "));
    }
}
