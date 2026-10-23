//! RedGIFs, mirroring gallery_dl/extractor/redgifs.py through the v2 API
//! with a temporary guest token.
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use std::time::{Duration, Instant};
use url::Url;

pub static SITE: Site = Site {
    category: "redgifs",
    name: "RedGIFs",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&IMAGE, &USER, &COLLECTION, &COLLECTIONS, &NICHES, &SEARCH],
    api_key: false,
    cookies: None,
};

/// Upstream `{gallery:?//[:11]}`; the template engine has no `[:N]` format
/// specifier, so the equivalent accessor slice is used.
const FILENAME: &str = "{category}_{gallery[:11]:?//}{num:?_/_/>02}{id}.{extension}";
static USER: Route = Route::new("user", FILENAME, &["{category}", "{userName}"], "{id}");
static COLLECTION: Route = Route::new(
    "collection",
    FILENAME,
    &[
        "{category}",
        "{collection[userName]}",
        "{collection[folderName]}",
    ],
    "{collection[folderId]}_{id}",
);
static COLLECTIONS: Route = Route::new("collections", FILENAME, &["{category}"], "{id}");
static NICHES: Route = Route::new("niches", FILENAME, &["{category}"], "{id}");
static SEARCH: Route = Route::new(
    "search",
    FILENAME,
    &["{category}", "Search", "{search}"],
    "{id}",
);
static IMAGE: Route = Route::new("image", FILENAME, &["{category}"], "{id}");

const API: &str = "https://api.redgifs.com";
const ROOT: &str = "https://www.redgifs.com";
const PER_PAGE: u64 = 100;
const TOKEN_LIFETIME: Duration = Duration::from_secs(600);

/// The temporary token, keyed by API origin and cached for ten minutes like
/// upstream's `cache(_exp=600)`. It is only sent as a request header.
static TOKEN: tokio::sync::Mutex<Vec<(String, String, Instant)>> =
    tokio::sync::Mutex::const_new(Vec::new());

#[derive(Debug, PartialEq)]
enum Target {
    User {
        user: String,
        query: Option<String>,
    },
    Collection {
        user: String,
        id: String,
    },
    Collections {
        user: String,
    },
    Niches {
        niche: String,
        query: Option<String>,
    },
    /// `tag` for `/gifs/TAG`; `search` is upstream's empty group for
    /// `/search[/gifs]`, which selects the `/v2/search/gifs` endpoint.
    Search {
        tag: Option<String>,
        search: bool,
        query: Option<String>,
    },
    Image(String),
}

fn word(label: &str) -> bool {
    !label.is_empty() && label.chars().all(|c| c.is_alphanumeric() || c == '_')
}

/// `redgifs.com` with at most one `\w+` label in front.
fn host_in(url: &Url, domain: &str, any_label: bool) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    host == domain
        || host
            .strip_suffix(domain)
            .and_then(|h| h.strip_suffix('.'))
            .is_some_and(|label| {
                if any_label {
                    word(label)
                } else {
                    label == "www"
                }
            })
}

/// The leading `[A-Za-z0-9]+` of a segment, as the image pattern captures.
fn alnum(id: &str) -> Option<&str> {
    let end = id
        .find(|c: char| !c.is_ascii_alphanumeric())
        .unwrap_or(id.len());
    (end > 0).then(|| &id[..end])
}

fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) {
        return None;
    }
    let raw = url.path();
    let parts = crate::sites::segments(url);
    let p: Vec<&str> = parts.iter().map(String::as_str).collect();
    let query = url.query().map(str::to_owned);
    let end_slash_ok = |n: usize| {
        // `/?` before the optional query and `$` after it.
        let trimmed = raw.strip_suffix('/').unwrap_or(raw);
        trimmed.matches('/').count() == n && url.fragment().is_none()
    };
    if host_in(url, "gfycat.com", true) {
        // `gfycat.com(?:/gifs/detail|/\w+)?/ID`
        let id = match p.as_slice() {
            ["gifs", "detail", id, ..] => alnum(id),
            [w, id, ..] if word(w) && alnum(id).is_some() && raw[1..].contains('/') => alnum(id),
            [id, ..] => alnum(id),
            _ => None,
        }?;
        return Some(Target::Image(id.into()));
    }
    if host_in(url, "gifdeliverynetwork.com", false) {
        return p
            .first()
            .and_then(|id| alnum(id))
            .map(|id| Target::Image(id.into()));
    }
    if url.host_str() == Some("i.redgifs.com")
        && let ["i", id, ..] = p.as_slice()
        && let Some(id) = alnum(id)
    {
        return Some(Target::Image(id.into()));
    }
    let any = host_in(url, "redgifs.com", true);
    let www = host_in(url, "redgifs.com", false);
    if !any {
        return None;
    }
    match p.as_slice() {
        ["users", user, "collections", id, ..] if www => Some(Target::Collection {
            user: (*user).into(),
            id: (*id).into(),
        }),
        ["users", user, "collections"] if www && end_slash_ok(3) && query.is_none() => {
            Some(Target::Collections {
                user: (*user).into(),
            })
        }
        ["users", user] if end_slash_ok(2) => Some(Target::User {
            user: (*user).into(),
            query,
        }),
        ["niches", niche] if www && end_slash_ok(2) => Some(Target::Niches {
            niche: (*niche).into(),
            query,
        }),
        ["gifs", tag, rest @ ..] => Some(Target::Search {
            tag: Some((*tag).into()),
            search: false,
            query: query.filter(|_| rest.is_empty()),
        }),
        ["search"] | ["search", "gifs"] => Some(Target::Search {
            tag: None,
            search: true,
            query,
        }),
        ["browse"] => Some(Target::Search {
            tag: None,
            search: false,
            query,
        }),
        ["watch" | "ifr", id, ..] => alnum(id).map(|id| Target::Image(id.into())),
        _ => None,
    }
}

fn headers(token: Option<&str>) -> Vec<(&'static str, String)> {
    let mut headers = vec![
        ("Accept", "application/json, text/plain, */*".to_owned()),
        ("Referer", format!("{ROOT}/")),
        ("Origin", ROOT.to_owned()),
    ];
    if let Some(token) = token {
        headers.push(("Authorization", token.to_owned()));
    }
    headers
}

async fn token(http: &Http) -> Result<String> {
    let url = http.api(&format!("{API}/v2/auth/temporary"))?;
    #[cfg(not(test))]
    let origin = url.origin().ascii_serialization();
    // Test servers reuse ports across tests; keep each test's token apart.
    #[cfg(test)]
    let origin = format!(
        "{} {:?}",
        url.origin().ascii_serialization(),
        std::thread::current().id()
    );
    let mut cached = TOKEN.lock().await;
    cached.retain(|(_, _, at)| at.elapsed() < TOKEN_LIFETIME);
    if let Some((_, token, _)) = cached.iter().find(|(o, _, _)| *o == origin) {
        return Ok(token.clone());
    }
    let h = headers(None);
    let refs: Vec<(&str, &str)> = h.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let body = http.json_with(&url, &refs).await?;
    let value = body
        .get("token")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty() && t.len() <= 8192 && !t.contains(['\r', '\n']))
        .ok_or_else(|| error("RedGIFs did not issue a temporary token"))?;
    let token = format!("Bearer {value}");
    cached.push((origin, token.clone(), Instant::now()));
    Ok(token)
}

async fn call(http: &Http, endpoint: &str, params: &[(String, String)]) -> Result<Value> {
    let token = token(http).await?;
    let mut url = http.api(&format!("{API}{endpoint}"))?;
    if !params.is_empty() {
        url.query_pairs_mut().extend_pairs(params);
    }
    let h = headers(Some(&token));
    let refs: Vec<(&str, &str)> = h.iter().map(|(k, v)| (*k, v.as_str())).collect();
    http.json_with(&url, &refs)
        .await
        .map_err(|e| error(format!("RedGIFs API request failed: {e}")))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported RedGIFs URL"));
    };
    match target {
        Target::Image(id) => crate::sites::once(async move {
            let body = call(http, &format!("/v2/gifs/{}", id.to_lowercase()), &[]).await?;
            let gif = body
                .get("gif")
                .cloned()
                .ok_or_else(|| error("RedGIFs response contains no gif"))?;
            Ok(gifs_events(http, &IMAGE, vec![gif], Map::new()))
        }),
        Target::User { user, query } => {
            let mut params = crate::sites::media::parse_query(query.as_deref());
            for (key, value) in &mut params {
                if key == "type"
                    && let Some(c) = value.chars().next()
                {
                    *value = c.to_lowercase().to_string();
                }
            }
            if !params.iter().any(|(k, _)| k == "order") {
                params.push(("order".into(), "new".into()));
            }
            let mut data = Map::new();
            data.insert("userName".into(), user.clone().into());
            paginate(
                http,
                &USER,
                format!("/v2/users/{}/search", user.to_lowercase()),
                params,
                "gifs",
                data,
            )
        }
        Target::Collection { user, id } => crate::sites::once(async move {
            let mut info = call(http, &format!("/v2/users/{user}/collections/{id}"), &[])
                .await?
                .as_object()
                .cloned()
                .ok_or_else(|| error("RedGIFs collection response is not an object"))?;
            info.insert("userName".into(), user.clone().into());
            let mut data = Map::new();
            data.insert("collection".into(), info.into());
            Ok(paginate(
                http,
                &COLLECTION,
                format!("/v2/users/{user}/collections/{id}/gifs"),
                Vec::new(),
                "gifs",
                data,
            ))
        }),
        Target::Collections { user } => {
            // Not file metadata: the URL user that child URLs are built from.
            let mut data = Map::new();
            data.insert("_user".into(), user.clone().into());
            paginate(
                http,
                &COLLECTIONS,
                format!("/v2/users/{user}/collections"),
                Vec::new(),
                "collections",
                data,
            )
        }
        Target::Niches { niche, query } => {
            let query = crate::sites::media::parse_query(query.as_deref());
            let order = crate::sites::media::query_get(&query, "order")
                .filter(|o| !o.is_empty())
                .unwrap_or("new")
                .to_owned();
            paginate(
                http,
                &NICHES,
                format!("/v2/niches/{niche}/gifs"),
                vec![("count".into(), "30".into()), ("order".into(), order)],
                "gifs",
                Map::new(),
            )
        }
        Target::Search { tag, search, query } => {
            let mut params = crate::sites::media::parse_query(query.as_deref());
            if let Some(tag) = &tag {
                let tag = tag.clone();
                match params.iter_mut().find(|(k, _)| k == "tags") {
                    Some((_, value)) => *value = tag,
                    None => params.push(("tags".into(), tag)),
                }
            }
            let get = |key| {
                crate::sites::media::query_get(&params, key)
                    .filter(|v| !v.is_empty())
                    .map(str::to_owned)
            };
            let label = get("query")
                .or_else(|| get("tags"))
                .or_else(|| get("order"))
                .unwrap_or_else(|| "trending".into());
            let mut data = Map::new();
            data.insert("search".into(), label.into());
            let endpoint = if search {
                "/v2/search/gifs"
            } else {
                // `params["search_text"] = params.pop("tags", None)`; requests
                // drops a None value.
                if let Some(pos) = params.iter().position(|(k, _)| k == "tags") {
                    let (_, tags) = params.remove(pos);
                    params.push(("search_text".into(), tags));
                }
                "/v2/gifs/search"
            };
            paginate(http, &SEARCH, endpoint.into(), params, "gifs", data)
        }
    }
}

/// `_pagination`: `page` from the query or one, `count` of 100, until
/// `page >= pages`.
fn paginate<'a>(
    http: &'a Http,
    route: &'static Route,
    endpoint: String,
    mut params: Vec<(String, String)>,
    key: &'static str,
    data: Map<String, Value>,
) -> Events<'a> {
    let start = match params.iter().position(|(k, _)| k == "page") {
        Some(pos) => crate::sites::media::parse_int(&params.remove(pos).1),
        None => 1,
    };
    params.retain(|(k, _)| k != "count");
    crate::sites::paginate(Some(start), move |page| {
        let endpoint = endpoint.clone();
        let params = params.clone();
        let data = data.clone();
        async move {
            let Some(page) = page else {
                return Ok(None);
            };
            let mut query = params;
            // Python dict order: existing keys keep their position.
            query.push(("page".into(), page.to_string()));
            query.push(("count".into(), PER_PAGE.to_string()));
            let body = call(http, &endpoint, &query).await?;
            let entries: Vec<Value> = body
                .get(key)
                .and_then(Value::as_array)
                .cloned()
                .ok_or_else(|| error("RedGIFs page has no results"))?;
            let pages = body.get("pages").and_then(Value::as_i64).unwrap_or(0);
            let next = (page < pages && page < 100_000).then_some(page + 1);
            let events = if key == "collections" {
                collections_events(
                    http,
                    entries,
                    crate::sites::media::str_of(&Value::Object(data), "_user"),
                )
            } else {
                gifs_events(http, route, entries, data)
            };
            Ok(Some((events, next)))
        }
    })
}

fn collections_events<'a>(http: &'a Http, collections: Vec<Value>, user: &str) -> Events<'a> {
    let mut children = Vec::new();
    for collection in collections {
        let Some(data) = collection.as_object().cloned() else {
            continue;
        };
        let folder = crate::sites::media::str_of(&collection, "folderId");
        if folder.is_empty() || folder.contains(['/', '?', '#']) {
            continue;
        }
        if user.is_empty() || user.contains(['/', '?', '#']) {
            continue;
        }
        if let Ok(url) = Url::parse(&format!("{ROOT}/users/{user}/collections/{folder}")) {
            children.push((url, data));
        }
    }
    futures_util::StreamExt::boxed(futures_util::StreamExt::flatten(
        futures_util::stream::iter(children.into_iter().map(move |(url, data)| {
            crate::sites::queue(http, "redgifs", "collections", url, &data)
        })),
    ))
}

fn formats(http: &Http) -> Vec<String> {
    let defaults = ["hd", "sd", "gif"].map(String::from);
    match http.option("format") {
        None | Some(Value::Null) => defaults.to_vec(),
        Some(Value::String(s)) => std::iter::once(s.clone()).chain(defaults).collect(),
        Some(Value::Array(list)) => list
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        Some(_) => defaults.to_vec(),
    }
}

/// `items()` for a page of gifs; galleries fetch their member gifs lazily.
fn gifs_events<'a>(
    http: &'a Http,
    route: &'static Route,
    gifs: Vec<Value>,
    metadata: Map<String, Value>,
) -> Events<'a> {
    let formats = formats(http);
    crate::sites::pages(futures_util::stream::iter(gifs).then(move |gif| {
        let metadata = metadata.clone();
        let formats = formats.clone();
        async move {
            let Some(mut gif) = gif.as_object().cloned() else {
                return Ok(crate::sites::events(Vec::new()));
            };
            let gallery = gif
                .get("gallery")
                .and_then(Value::as_str)
                .filter(|g| !g.is_empty())
                .map(str::to_owned);
            let (members, enumerate_from) = match &gallery {
                Some(id) if id.bytes().all(|b| b.is_ascii_alphanumeric()) => {
                    let body = call(http, &format!("/v2/gallery/{id}"), &[]).await?;
                    let members: Vec<Map<String, Value>> = body
                        .get("gifs")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|g| g.as_object().cloned())
                        .collect();
                    (Some(members), 1)
                }
                _ => (None, 0),
            };
            gif.extend(metadata.clone());
            let count = members.as_ref().map_or(1, Vec::len);
            gif.insert("count".into(), count.into());
            let created = crate::sites::media::ts_value(gif.get("createDate"));
            gif.insert("date".into(), crate::sites::media::date(created));
            let files = members.unwrap_or_else(|| vec![gif.clone()]);
            let mut items = Vec::new();
            for (offset, mut file) in files.into_iter().enumerate() {
                if enumerate_from == 1 {
                    // Directory paths come from the post (outer gif)
                    // metadata upstream; carry it into gallery members.
                    for (key, value) in &metadata {
                        file.entry(key.clone()).or_insert(value.clone());
                    }
                }
                let Some(url) = select(&file, &formats) else {
                    continue;
                };
                crate::sites::media::set_nameext(&mut file, crate::sites::nameext(&url));
                file.insert("num".into(), (offset + enumerate_from).into());
                file.insert("count".into(), count.into());
                let id = file.get("id").map(crate::sites::scalar).unwrap_or_default();
                let user = file
                    .get("userName")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let ts = crate::sites::media::ts_value(file.get("createDate")).or(created);
                let post_id = gallery.clone().unwrap_or_else(|| id.clone());
                items.push(
                    Item::new(url, file)
                        .ids(post_id, id)
                        .user(user)
                        .timestamp(ts),
                );
            }
            Ok(crate::sites::events(crate::sites::post(
                &SITE, http, route, items,
            )))
        }
    }))
}

use futures_util::StreamExt as _;

/// The first configured format with a URL, on a RedGIFs media host.
fn select(gif: &Map<String, Value>, formats: &[String]) -> Option<Url> {
    let urls = gif.get("urls")?;
    formats.iter().find_map(|format| {
        let raw = urls.get(format)?.as_str().filter(|u| !u.is_empty())?;
        let raw = raw.replacen("//thumbs2.", "//thumbs3.", 1);
        crate::sites::media_url(&raw, &["redgifs.com"])
    })
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
            t("https://www.redgifs.com/watch/Squeakyfoolishbird"),
            Some(Target::Image("Squeakyfoolishbird".into()))
        );
        assert_eq!(
            t("https://v3.redgifs.com/ifr/abc"),
            Some(Target::Image("abc".into()))
        );
        assert_eq!(
            t("https://i.redgifs.com/i/abc.jpg"),
            Some(Target::Image("abc".into()))
        );
        assert_eq!(
            t("https://gfycat.com/gifs/detail/abc"),
            Some(Target::Image("abc".into()))
        );
        assert_eq!(
            t("https://www.redgifs.com/users/Me/?order=top&type=G"),
            Some(Target::User {
                user: "Me".into(),
                query: Some("order=top&type=G".into())
            })
        );
        assert_eq!(
            t("https://www.redgifs.com/users/Me/collections/abc"),
            Some(Target::Collection {
                user: "Me".into(),
                id: "abc".into()
            })
        );
        assert_eq!(
            t("https://redgifs.com/users/Me/collections/"),
            Some(Target::Collections { user: "Me".into() })
        );
        assert!(matches!(
            t("https://www.redgifs.com/niches/pets?order=top"),
            Some(Target::Niches { .. })
        ));
        assert_eq!(
            t("https://www.redgifs.com/search?query=cats"),
            Some(Target::Search {
                tag: None,
                search: true,
                query: Some("query=cats".into())
            })
        );
        assert!(matches!(
            t("https://www.redgifs.com/gifs/cat%20dog"),
            Some(Target::Search {
                tag: Some(_),
                search: false,
                ..
            })
        ));
        for raw in [
            "https://www.redgifs.com/",
            "https://redgifs.com.evil.test/watch/abc",
            "https://evilredgifs.com/watch/abc",
            "https://a.b.redgifs.com/watch/abc",
            "https://www.redgifs.com:444/watch/abc",
            "https://user@www.redgifs.com/watch/abc",
            "https://www.redgifs.com/users/Me/likes",
            "https://v3.redgifs.com/niches/pets",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn gif(id: &str, gallery: Option<&str>) -> Value {
        json!({"id": id, "createDate": 1500000000, "userName": "someone",
            "gallery": gallery, "tags": ["a"],
            "urls": {"sd": format!("https://media.redgifs.com/{id}-mobile.mp4"),
                     "hd": format!("https://thumbs2.redgifs.com/{id}.mp4")}})
    }

    #[tokio::test]
    async fn image_uses_temporary_token_and_hd_format() {
        let (root, server) = serve(vec![
            json(json!({"token": "tok123"})),
            json(json!({"gif": gif("squeakyfoolishbird", None)})),
        ]);
        let http = http("redgifs", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.redgifs.com/watch/SqueakyFoolishBird").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].url.as_str(),
            "https://thumbs3.redgifs.com/squeakyfoolishbird.mp4"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["num"], 0);
        assert_eq!(k["count"], 1);
        assert_eq!(k["date"], "2017-07-14 02:40:00");
        assert_eq!(k["extension"], "mp4");
        assert!(!k.values().any(|v| v.to_string().contains("tok123")));
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            file.render(&files[0]).unwrap(),
            "redgifs_squeakyfoolishbird.mp4"
        );
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("redgifs")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{id}",["squeakyfoolishbird"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /v2/auth/temporary "));
        assert!(!requests[0].contains("authorization:"));
        assert!(requests[1].starts_with("GET /v2/gifs/squeakyfoolishbird "));
        assert!(
            requests[1]
                .to_ascii_lowercase()
                .contains("authorization: bearer tok123")
        );
    }

    #[tokio::test]
    async fn user_paginates_and_expands_galleries() {
        let (root, server) = serve(vec![
            json(json!({"token": "t"})),
            json(json!({"page": 1, "pages": 2, "gifs": [gif("one", None)]})),
            json(json!({"page": 2, "pages": 2, "gifs": [gif("two", Some("abcdefghijklmn"))]})),
            json(
                json!({"gifs": [gif("m1", Some("abcdefghijklmn")), gif("m2", Some("abcdefghijklmn"))]}),
            ),
        ]);
        let http = http("redgifs", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.redgifs.com/users/SomeOne?type=g").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 3);
        let k = keywords(&files[2]);
        assert_eq!(k["num"], 2);
        assert_eq!(k["count"], 2);
        assert_eq!(k["userName"], "someone");
        let (file, dir) = crate::sites::default_templates(&files[2]).unwrap();
        assert_eq!(
            file.render(&files[2]).unwrap(),
            "redgifs_abcdefghijk_02_m2.mp4"
        );
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("redgifs/SomeOne")
        );
        let requests = server.join().unwrap();
        assert!(
            requests[1]
                .starts_with("GET /v2/users/someone/search?type=g&order=new&page=1&count=100 "),
            "{}",
            requests[1]
        );
        assert!(requests[2].contains("page=2&count=100 "));
        assert!(requests[3].starts_with("GET /v2/gallery/abcdefghijklmn "));
    }

    #[tokio::test]
    async fn collections_queue_collection_pages() {
        let (root, server) = serve(vec![
            json(json!({"token": "t"})),
            json(json!({"page": 1, "pages": 1, "collections": [
                {"folderId": "f1", "folderName": "Faves", "userName": "Me"}]})),
            json(json!({"folderId": "f1", "folderName": "Faves"})),
            json(json!({"page": 1, "pages": 1, "gifs": [gif("x", None)]})),
        ]);
        let http = http("redgifs", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.redgifs.com/users/Me/collections").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let (_, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("redgifs/Me/Faves")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{collection[folderId]}_{id}",["f1","x"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /v2/users/Me/collections?page=1&count=100 "));
        assert!(requests[2].starts_with("GET /v2/users/Me/collections/f1 "));
        assert!(requests[3].starts_with("GET /v2/users/Me/collections/f1/gifs?page=1"));
    }

    #[tokio::test]
    async fn search_uses_tags_as_search_text() {
        let (root, server) = serve(vec![
            json(json!({"token": "t"})),
            json(json!({"page": 1, "pages": 1, "gifs": [gif("x", None)]})),
        ]);
        let http = http("redgifs", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.redgifs.com/gifs/cute%20cats?order=top").unwrap(),
        ))
        .await;
        let (_, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("redgifs/Search/cute cats")
        );
        let requests = server.join().unwrap();
        assert!(
            requests[1].starts_with(
                "GET /v2/gifs/search?order=top&search_text=cute+cats&page=1&count=100 "
            ),
            "{}",
            requests[1]
        );
    }
}
