//! Whyp tracks, user uploads and collections, mirroring
//! gallery_dl/extractor/whyp.py through `api.whyp.it`: lossless files when
//! offered, else the lossy ones.
use super::{date, iso_ts, set_param, str_of};
use crate::sites::art::nameext_into;
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

const API: &str = "https://api.whyp.it";
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

const HEADERS: &[(&str, &str)] = &[
    ("Accept", "application/json"),
    ("Origin", "https://whyp.it"),
    ("Referer", "https://whyp.it/"),
    ("Sec-Fetch-Dest", "empty"),
    ("Sec-Fetch-Mode", "cors"),
    ("Sec-Fetch-Site", "same-site"),
];

#[derive(Debug, PartialEq)]
enum Kind {
    Audio,
    User,
    Collection,
}

/// `(?:www\.)?whyp\.it/(tracks|users|collections)/(\d+)(?:/[^/?#]+)?/?
/// (?:\?([^#]+))?`: the ID and the query, captured only when it directly
/// follows the optional slug.
fn target(url: &Url) -> Option<(Kind, String, Option<String>)> {
    if !crate::sites::art::on_host(url, &["whyp.it"]) {
        return None;
    }
    let path = url.path();
    let (kind, rest) = [
        ("/tracks/", Kind::Audio),
        ("/users/", Kind::User),
        ("/collections/", Kind::Collection),
    ]
    .into_iter()
    .find_map(|(prefix, kind)| Some((kind, path.strip_prefix(prefix)?)))?;
    let id = crate::sites::art::leading_digits(rest)?;
    let mut tail = &rest[id.len()..];
    if let Some(slug) = tail.strip_prefix('/') {
        let len = slug.find('/').unwrap_or(slug.len());
        if len > 0 {
            tail = &slug[len..];
        }
    }
    tail = tail.strip_prefix('/').unwrap_or(tail);
    let query = url
        .query()
        .filter(|q| tail.is_empty() && !q.is_empty())
        .map(str::to_owned);
    Some((kind, id.into(), query))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some((kind, id, query)) = target(&url) else {
        return crate::sites::fail(error("unsupported Whyp URL"));
    };
    let params = query.as_deref().map(|q| super::parse_query(Some(q)));
    match kind {
        Kind::Audio => crate::sites::once(async move {
            let data = request(http, &format!("/api/tracks/{id}"), params.as_deref()).await?;
            let track = data.get("track").cloned().unwrap_or(Value::Null);
            Ok(tracks_events(http, &AUDIO, vec![track], &Map::new()))
        }),
        Kind::User => user_tracks(http, id, params.unwrap_or_default()),
        Kind::Collection => crate::sites::once(async move {
            let endpoint = format!("/api/collections/{id}");
            let data = request(http, &endpoint, params.as_deref()).await?;
            let collection = data.get("collection").cloned().unwrap_or(Value::Null);
            let token = collection
                .get("token")
                .map(crate::sites::scalar)
                .unwrap_or_default();
            let mut kwdict = Map::new();
            kwdict.insert("collection".into(), collection);
            let params = [("token".to_owned(), token)];
            let data = request(http, &format!("{endpoint}/tracks"), Some(&params)).await?;
            let tracks = data
                .get("tracks")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            Ok(tracks_events(http, &COLLECTION, tracks, &kwdict))
        }),
    }
}

async fn request(
    http: &Http,
    endpoint: &str,
    params: Option<&[(String, String)]>,
) -> Result<Value> {
    let mut url = http.api(&format!("{API}{endpoint}"))?;
    if let Some(params) = params.filter(|p| !p.is_empty()) {
        url.query_pairs_mut().extend_pairs(params);
    }
    http.json_with(&url, HEADERS)
        .await
        .map_err(|e| error(format!("Whyp API request failed: {e}")))
}

/// `WhypUserExtractor.tracks`: follow `next_cursor` until it is empty.
fn user_tracks(http: &Http, id: String, params: Vec<(String, String)>) -> Events<'_> {
    crate::sites::paginate(Some((params, 0u32)), move |state| {
        let id = id.clone();
        async move {
            let Some((mut params, pages)) = state else {
                return Ok(None);
            };
            let data = request(http, &format!("/api/users/{id}/tracks"), Some(&params)).await?;
            let tracks = data
                .get("tracks")
                .and_then(Value::as_array)
                .cloned()
                .ok_or_else(|| error("Whyp API response has no tracks"))?;
            let cursor = data
                .get("next_cursor")
                .filter(|c| crate::sites::art::truthy(Some(c)))
                .map(crate::sites::scalar);
            let next = cursor.filter(|_| pages < 100_000).map(|cursor| {
                set_param(&mut params, "cursor", cursor);
                (params, pages + 1)
            });
            Ok(Some((
                tracks_events(http, &USER, tracks, &Map::new()),
                next,
            )))
        }
    })
}

/// `items()`: one post per track, extractor keywords applied last.
fn tracks_events<'a>(
    http: &'a Http,
    route: &'static Route,
    tracks: Vec<Value>,
    kwdict: &Map<String, Value>,
) -> Events<'a> {
    let mut events = Vec::new();
    for track in tracks {
        let Value::Object(mut track) = track else {
            continue;
        };
        let lossless = str_of(&Value::Object(track.clone()), "lossless_url").to_owned();
        let raw = if lossless.is_empty() {
            track.insert("original".into(), false.into());
            str_of(&Value::Object(track.clone()), "lossy_url").to_owned()
        } else {
            track.insert("original".into(), true.into());
            lossless
        };
        let mut ts = None;
        if let Some(created) = track.get("created_at") {
            ts = iso_ts(created.as_str().unwrap_or_default());
            track.insert("date".into(), date(ts));
        }
        crate::sites::art::update(&mut track, kwdict);
        nameext_into(&raw, &mut track);
        let Some(url) = crate::sites::media_url(&raw, &["whyp.it"]) else {
            continue;
        };
        let id = track
            .get("id")
            .map(crate::sites::scalar)
            .unwrap_or_default();
        let user = track
            .get("user")
            .map(|user| str_of(user, "username").to_owned());
        let title = track
            .get("title")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let item = Item::new(url, track)
            .ids(id.clone(), id)
            .user(user)
            .caption(title)
            .timestamp(ts);
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
            t("https://whyp.it/tracks/12345/some-song?token=abc"),
            Some((Kind::Audio, "12345".into(), Some("token=abc".into())))
        );
        assert_eq!(
            t("https://www.whyp.it/users/123/"),
            Some((Kind::User, "123".into(), None))
        );
        assert_eq!(
            t("https://whyp.it/collections/9/name/extra?token=x"),
            Some((Kind::Collection, "9".into(), None))
        );
        assert_eq!(
            t("https://whyp.it/tracks/12x"),
            Some((Kind::Audio, "12".into(), None))
        );
        for raw in [
            "https://whyp.it/tracks/",
            "https://whyp.it/tracks/abc",
            "https://api.whyp.it/tracks/1",
            "https://whyp.it.evil.test/tracks/1",
            "https://evilwhyp.it/tracks/1",
            "https://whyp.it:8443/tracks/1",
            "https://u:p@whyp.it/tracks/1",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn track(id: u32, lossless: bool) -> Value {
        json!({"id": id, "title": format!("Track {id}"), "created_at": "2025-02-03T04:05:06.000000Z",
            "user": {"id": 7, "username": "dj"},
            "lossy_url": format!("https://cdn.whyp.it/lossy/{id}.mp3?token=1"),
            "lossless_url": if lossless { Value::from(format!("https://cdn.whyp.it/lossless/{id}.flac")) } else { Value::Null }})
    }

    #[tokio::test]
    async fn audio_prefers_lossless() {
        let (root, server) = serve(vec![json(json!({"track": track(1, true)}))]);
        let http = http("whyp", root);
        let files = media(extract(
            &http,
            Url::parse("https://whyp.it/tracks/1/slug?token=abc").unwrap(),
        ))
        .await;
        assert_eq!(files[0].url.as_str(), "https://cdn.whyp.it/lossless/1.flac");
        let k = keywords(&files[0]);
        assert_eq!(k["original"], true);
        assert_eq!(k["date"], "2025-02-03 04:05:06");
        assert_eq!(k["extension"], "flac");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "1 Track 1.flac");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("whyp/dj (7)")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{id}",["1"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/tracks/1?token=abc "));
        assert!(requests[0].contains("referer: https://whyp.it/"));
    }

    #[tokio::test]
    async fn user_cursor_and_collection() {
        let (root, server) = serve(vec![
            json(json!({"tracks": [track(1, false)], "next_cursor": "c2"})),
            json(json!({"tracks": [track(2, false)], "next_cursor": null})),
            json(json!({"collection": {"id": 9, "title": "Mix", "token": "tok"}})),
            json(json!({"tracks": [track(3, true)]})),
        ]);
        let http = http("whyp", root);
        let files = media(extract(
            &http,
            Url::parse("https://whyp.it/users/7/dj").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[0]);
        assert_eq!(k["original"], false);
        assert_eq!(k["filename"], "1");
        assert_eq!(k["extension"], "mp3");
        let files = media(extract(
            &http,
            Url::parse("https://whyp.it/collections/9/mix").unwrap(),
        ))
        .await;
        assert_eq!(keywords(&files[0])["collection"]["title"], "Mix");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/users/7/tracks "));
        assert!(requests[1].starts_with("GET /api/users/7/tracks?cursor=c2 "));
        assert!(requests[2].starts_with("GET /api/collections/9 "));
        assert!(requests[3].starts_with("GET /api/collections/9/tracks?token=tok "));
    }
}
