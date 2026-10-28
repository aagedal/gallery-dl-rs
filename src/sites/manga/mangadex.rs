//! MangaDex chapters, manga, covers, MDLists and authors, mirroring
//! gallery_dl/extractor/mangadex.py through the public API v5.
use super::{Gallery, get, keywords, set, str_of, text};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use futures_util::StreamExt;
use serde_json::{Map, Value};
use std::collections::HashMap;
use url::Url;

pub static SITE: Site = Site {
    category: "mangadex",
    name: "MangaDex",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[
        &CHAPTER, &MANGA, &COVERS, &LIST, &LIST_FEED, &AUTHOR, &FEED, &FOLLOWING,
    ],
    api_key: false,
    cookies: None,
};

const ROOT: &str = "https://mangadex.org";
const FILENAME: &str = "{manga}_c{chapter:>03}{chapter_minor}_{page:>03}.{extension}";
const DIRECTORY: &[&str] = &[
    "{category}",
    "{manga}",
    "{volume:?v/ />02}c{chapter:>03}{chapter_minor}{title:?: //}",
];
const ARCHIVE: &str = "{chapter_id}_{page}";

static CHAPTER: Route = Route::new("chapter", FILENAME, DIRECTORY, ARCHIVE);
static MANGA: Route = Route::new("manga", FILENAME, DIRECTORY, ARCHIVE);
static COVERS: Route = Route::new(
    "covers",
    "{volume:>02}_{lang}.{extension}",
    &["{category}", "{manga}", "Covers"],
    "c_{cover_id}",
);
static LIST: Route = Route::new("list", FILENAME, DIRECTORY, ARCHIVE);
static LIST_FEED: Route = Route::new("list-feed", FILENAME, DIRECTORY, ARCHIVE);
static AUTHOR: Route = Route::new("author", FILENAME, DIRECTORY, ARCHIVE);
static FEED: Route = Route::new("feed", FILENAME, DIRECTORY, ARCHIVE);
static FOLLOWING: Route = Route::new("following", FILENAME, DIRECTORY, ARCHIVE);

#[derive(Debug, PartialEq)]
enum Target {
    Chapter(String),
    Manga(String),
    Covers(String),
    List {
        uuid: String,
        feed: bool,
    },
    Author(String),
    /// `/title/feed`: chapters of followed manga (needs a login).
    Feed,
    /// `/title/follows`: followed manga (needs a login).
    Following,
}

fn uuid(text: &str) -> bool {
    (1..=64).contains(&text.len())
        && text != "feed"
        && text
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f' | b'-'))
}

fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) || !crate::sites::host_is(url, &["mangadex.org", "mangadex.cc"]) {
        return None;
    }
    let parts = crate::sites::segments(url);
    let query = url.query().unwrap_or_default();
    match parts.as_slice() {
        [kind, tab] if matches!(kind.as_str(), "title" | "titles") && tab == "feed" => {
            return Some(Target::Feed);
        }
        [kind, tab] if matches!(kind.as_str(), "title" | "titles") && tab == "follows" => {
            return Some(Target::Following);
        }
        _ => {}
    }
    let (kind, id, rest) = match parts.as_slice() {
        [kind, id, rest @ ..] if uuid(id) => (kind.as_str(), id.clone(), rest),
        _ => return None,
    };
    match kind {
        "chapter" => Some(Target::Chapter(id)),
        "title" | "manga" if rest.len() <= 1 && query.starts_with("tab=art") => {
            Some(Target::Covers(id))
        }
        "title" | "manga" => Some(Target::Manga(id)),
        "list" => {
            let tab = query
                .strip_prefix("tab=")
                .filter(|_| rest.len() <= 1)
                .map(|tab| {
                    let end = tab
                        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
                        .unwrap_or(tab.len());
                    &tab[..end]
                });
            Some(Target::List {
                uuid: id,
                feed: tab == Some("feed"),
            })
        }
        "author" => Some(Target::Author(id)),
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported MangaDex URL"));
    };
    let root = match api_root(http) {
        Ok(root) => root,
        Err(e) => return crate::sites::fail(e),
    };
    match target {
        Target::Chapter(uuid) => chapter_events(http, root, uuid, None),
        Target::Manga(uuid) => {
            let order = if http.option_bool("chapter-reverse", false) {
                "desc"
            } else {
                "asc"
            };
            let params = chapter_params(
                http,
                vec![
                    ("order[volume]".into(), order.into()),
                    ("order[chapter]".into(), order.into()),
                ],
            );
            let endpoint = format!("{root}/manga/{uuid}/feed");
            pages(http, root, endpoint, params, Kind::Chapters(&MANGA))
        }
        Target::Covers(uuid) => {
            let mut params = vec![("manga[]".to_owned(), uuid.clone())];
            for lang in super::option_list(http, "lang").unwrap_or_default() {
                params.push(("locales[]".into(), lang));
            }
            let order = if http.option_bool("chapter-reverse", false) {
                "desc"
            } else {
                "asc"
            };
            params.push(("order[volume]".into(), order.into()));
            // `contentRating: None` suppresses the default rating filter.
            let params = finish_params(http, params, true);
            pages(
                http,
                root.clone(),
                format!("{root}/cover"),
                params,
                Kind::Covers(uuid),
            )
        }
        Target::List { uuid, feed: true } => {
            let params = chapter_params(http, Vec::new());
            let endpoint = format!("{root}/list/{uuid}/feed");
            pages(http, root, endpoint, params, Kind::Chapters(&LIST_FEED))
        }
        Target::List { uuid, feed: false } => crate::sites::once(async move {
            let list = call(http, &format!("{root}/list/{uuid}"), &[]).await?;
            let children = list
                .pointer("/data/relationships")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|item| str_of(item, "type") == "manga")
                .filter_map(|item| title_url(item).map(|url| (url, Map::new())))
                .collect();
            Ok(super::queue_urls(http, &SITE, &LIST, children))
        }),
        Target::Feed => crate::sites::once(async move {
            let auth = bearer(http, &root).await?;
            let params = chapter_params(http, vec![("order[publishAt]".into(), "desc".into())]);
            let endpoint = format!("{root}/user/follows/manga/feed");
            Ok(pages_with(
                http,
                root,
                endpoint,
                params,
                Kind::Chapters(&FEED),
                Some(auth),
            ))
        }),
        Target::Following => crate::sites::once(async move {
            let auth = bearer(http, &root).await?;
            let params = finish_params(http, Vec::new(), true);
            let endpoint = format!("{root}/user/follows/manga");
            Ok(pages_with(
                http,
                root,
                endpoint,
                params,
                Kind::Author,
                Some(auth),
            ))
        }),
        Target::Author(uuid) => {
            let params = finish_params(http, vec![("authorOrArtist".into(), uuid)], false);
            pages(
                http,
                root.clone(),
                format!("{root}/manga"),
                params,
                Kind::Author,
            )
        }
    }
}

fn title_url(manga: &Value) -> Option<Url> {
    let id = str_of(manga, "id");
    uuid(id)
        .then(|| Url::parse(&format!("{ROOT}/title/{id}")).ok())
        .flatten()
}

/// `api-server`, defaulting to `https://api.mangadex.org`.
fn api_root(http: &Http) -> Result<String> {
    let Some(server) = http.option_str("api-server") else {
        return Ok("https://api.mangadex.org".into());
    };
    let root = text::ensure_http_scheme(server)
        .trim_end_matches('/')
        .to_owned();
    Url::parse(&root)
        .ok()
        .filter(crate::sites::plain)
        .map(|_| root)
        .ok_or_else(|| error("invalid MangaDex api-server option"))
}

async fn call(http: &Http, endpoint: &str, params: &[(String, String)]) -> Result<Value> {
    call_with(http, endpoint, params, None).await
}

async fn call_with(
    http: &Http,
    endpoint: &str,
    params: &[(String, String)],
    auth: Option<&str>,
) -> Result<Value> {
    let mut url = http.api(endpoint)?;
    if !params.is_empty() {
        url.query_pairs_mut().extend_pairs(params);
    }
    let mut headers = vec![("Referer", "https://mangadex.org/")];
    if let Some(auth) = auth {
        headers.push(("Authorization", auth));
    }
    http.json_with(&url, &headers).await
}

/// Upstream `MangadexAPI.authenticate`: a personal API client
/// (`client-id`/`client-secret`) with the password grant, or the legacy
/// `auth/login` endpoint. Tokens are not cached between runs.
async fn bearer(http: &Http, root: &str) -> Result<String> {
    static TOKEN: tokio::sync::OnceCell<String> = tokio::sync::OnceCell::const_new();
    TOKEN
        .get_or_try_init(|| async {
            let (username, password) = crate::sites::gdl::auth_info(http)
                .ok_or_else(|| error("MangaDex feeds and follows need a username and password"))?;
            match http.option_str("client-id") {
                Some(client_id) => {
                    let secret = http.option_str("client-secret").unwrap_or_default();
                    let url = Url::parse(
                        "https://auth.mangadex.org/realms/mangadex/protocol/openid-connect/token",
                    )
                    .expect("static URL");
                    let body = http
                        .post(
                            &url,
                            crate::sites::http::Body::Form(&[
                                ("grant_type", "password"),
                                ("username", &username),
                                ("password", &password),
                                ("client_id", client_id),
                                ("client_secret", secret),
                            ]),
                            &[],
                        )
                        .await?;
                    let value: Value = serde_json::from_slice(&body)
                        .map_err(|_| error("MangaDex login failed"))?;
                    value
                        .get("access_token")
                        .and_then(Value::as_str)
                        .map(|token| format!("Bearer {token}"))
                        .ok_or_else(|| error("MangaDex login failed"))
                }
                None => {
                    let url = Url::parse(&format!("{root}/auth/login"))
                        .map_err(|_| error("invalid MangaDex API root"))?;
                    let credentials =
                        serde_json::json!({"username": username, "password": password});
                    let body = http
                        .post(&url, crate::sites::http::Body::Json(&credentials), &[])
                        .await?;
                    let value: Value = serde_json::from_slice(&body)
                        .map_err(|_| error("MangaDex login failed"))?;
                    if value.get("result").and_then(Value::as_str) != Some("ok") {
                        return Err(error("MangaDex login failed"));
                    }
                    value
                        .pointer("/token/session")
                        .and_then(Value::as_str)
                        .map(|token| format!("Bearer {token}"))
                        .ok_or_else(|| error("MangaDex login failed"))
                }
            }
        })
        .await
        .cloned()
}

/// `_pagination_chapters`: language filter and scanlation group includes.
fn chapter_params(http: &Http, mut params: Vec<(String, String)>) -> Vec<(String, String)> {
    for lang in super::option_list(http, "lang").unwrap_or_default() {
        params.push(("translatedLanguage[]".into(), lang));
    }
    params.push(("includes[]".into(), "scanlation_group".into()));
    finish_params(http, params, false)
}

/// `_pagination`: content ratings unless suppressed, then `api-parameters`.
fn finish_params(
    http: &Http,
    mut params: Vec<(String, String)>,
    no_ratings: bool,
) -> Vec<(String, String)> {
    if !no_ratings {
        let ratings = super::option_list(http, "ratings").unwrap_or_else(|| {
            ["safe", "suggestive", "erotica", "pornographic"]
                .map(String::from)
                .to_vec()
        });
        for rating in ratings {
            params.push(("contentRating[]".into(), rating));
        }
    }
    if let Some(Value::Object(extra)) = http.option("api-parameters") {
        params.retain(|(key, _)| !extra.contains_key(key));
        for (key, value) in extra {
            match value {
                Value::Array(values) => {
                    for value in values {
                        params.push((key.clone(), crate::sites::scalar(value)));
                    }
                }
                Value::Null => {}
                value => params.push((key.clone(), crate::sites::scalar(value))),
            }
        }
    }
    params
}

#[derive(Clone)]
enum Kind {
    Chapters(&'static Route),
    Covers(String),
    Author,
}

type Cache = HashMap<String, Map<String, Value>>;

/// `_pagination`: `offset` advances by `limit` until it reaches `total`.
fn pages<'a>(
    http: &'a Http,
    root: String,
    endpoint: String,
    params: Vec<(String, String)>,
    kind: Kind,
) -> Events<'a> {
    pages_with(http, root, endpoint, params, kind, None)
}

fn pages_with<'a>(
    http: &'a Http,
    root: String,
    endpoint: String,
    params: Vec<(String, String)>,
    kind: Kind,
    auth: Option<String>,
) -> Events<'a> {
    crate::sites::paginate(Some((0u64, Cache::new())), move |state| {
        let (root, endpoint, kind) = (root.clone(), endpoint.clone(), kind.clone());
        let auth = auth.clone();
        let mut params = params.clone();
        async move {
            let Some((offset, mut cache)) = state else {
                return Ok(None);
            };
            params.push(("offset".into(), offset.to_string()));
            let data = call_with(http, &endpoint, &params, auth.as_deref()).await?;
            let items = data
                .get("data")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let number = |key| data.get(key).and_then(Value::as_u64).unwrap_or(0);
            let next = number("offset") + number("limit");
            let events = page_events(http, &root, &kind, items, &mut cache).await?;
            let more = next > offset && next < number("total");
            Ok(Some((events, more.then_some((next, cache)))))
        }
    })
}

async fn page_events<'a>(
    http: &'a Http,
    root: &str,
    kind: &Kind,
    items: Vec<Value>,
    cache: &mut Cache,
) -> Result<Events<'a>> {
    let mut streams: Vec<Events<'a>> = Vec::new();
    match kind {
        Kind::Chapters(route) => {
            for chapter in items {
                let (uuid, data) = transform(http, root, &chapter, cache).await?;
                let child_root = root.to_owned();
                let child = data.clone();
                streams.push(super::queue_with(&SITE, route, &data, move || {
                    chapter_events(http, child_root, uuid, Some(child))
                }));
            }
        }
        Kind::Author => {
            let children = items
                .iter()
                .filter_map(|manga| {
                    Some((
                        title_url(manga)?,
                        manga.as_object().cloned().unwrap_or_default(),
                    ))
                })
                .collect();
            streams.push(super::queue_urls(http, &SITE, &AUTHOR, children));
        }
        Kind::Covers(uuid) => {
            for cover in items {
                let manga_id = relationships(&cover, "manga")
                    .first()
                    .map(|m| str_of(m, "id").to_owned())
                    .unwrap_or_default();
                let info = manga_info_cached(http, root, &manga_id, cache).await?;
                if let Some(item) = cover_item(uuid, &cover, info) {
                    streams.push(crate::sites::events(crate::sites::post(
                        &SITE,
                        http,
                        &COVERS,
                        vec![item],
                    )));
                }
            }
        }
    }
    Ok(futures_util::stream::iter(streams).flatten().boxed())
}

fn relationships<'v>(value: &'v Value, kind: &str) -> Vec<&'v Value> {
    value
        .get("relationships")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| str_of(item, "type") == kind)
        .collect()
}

/// A localized string object's `en` value, else its first value.
fn localized(value: &Value) -> Value {
    let Some(map) = value.as_object() else {
        return "".into();
    };
    match map.get("en") {
        Some(en) if !en.is_null() && en.as_str() != Some("") => en.clone(),
        _ => map.values().next().cloned().unwrap_or_else(|| "".into()),
    }
}

async fn manga_info_cached<'c>(
    http: &Http,
    root: &str,
    uuid: &str,
    cache: &'c mut Cache,
) -> Result<&'c Map<String, Value>> {
    if !cache.contains_key(uuid) {
        let manga = call(
            http,
            &format!("{root}/manga/{uuid}"),
            &[
                ("includes[]".into(), "artist".into()),
                ("includes[]".into(), "author".into()),
            ],
        )
        .await?;
        let info = manga_info(manga.get("data").unwrap_or(&Value::Null));
        cache.insert(uuid.to_owned(), info);
    }
    Ok(&cache[uuid])
}

/// `_manga_info`.
pub(crate) fn manga_info(manga: &Value) -> Map<String, Value> {
    let attr = manga.get("attributes").unwrap_or(&Value::Null);
    let names = |kind| {
        Value::Array(
            relationships(manga, kind)
                .into_iter()
                .map(|a| {
                    a.pointer("/attributes/name")
                        .cloned()
                        .unwrap_or(Value::Null)
                })
                .collect(),
        )
    };
    let alt_titles = attr
        .get("altTitles")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|t| t.as_object()?.values().next_back().cloned())
        .collect();
    let tags = attr
        .get("tags")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|t| {
            t.pointer("/attributes/name/en")
                .cloned()
                .unwrap_or(Value::Null)
        })
        .collect();
    keywords([
        (
            "manga",
            localized(attr.get("title").unwrap_or(&Value::Null)),
        ),
        ("manga_id", get(manga, "id")),
        ("manga_titles", Value::Array(alt_titles)),
        (
            "manga_date",
            text::date(
                attr.get("createdAt")
                    .and_then(Value::as_str)
                    .and_then(text::iso),
            ),
        ),
        (
            "description",
            localized(attr.get("description").unwrap_or(&Value::Null)),
        ),
        ("demographic", get(attr, "publicationDemographic")),
        ("origin", get(attr, "originalLanguage")),
        ("status", get(attr, "status")),
        ("year", get(attr, "year")),
        ("rating", get(attr, "contentRating")),
        ("links", get(attr, "links")),
        ("tags", Value::Array(tags)),
        ("artist", names("artist")),
        ("author", names("author")),
    ])
}

/// `_transform`: a chapter's keywords, with its manga info.
async fn transform(
    http: &Http,
    root: &str,
    chapter: &Value,
    cache: &mut Cache,
) -> Result<(String, Map<String, Value>)> {
    let uuid = str_of(chapter, "id").to_owned();
    if !self::uuid(&uuid) {
        return Err(error("MangaDex returned an invalid chapter ID"));
    }
    let manga_id = relationships(chapter, "manga")
        .first()
        .map(|m| str_of(m, "id").to_owned())
        .ok_or_else(|| error("MangaDex chapter has no manga"))?;
    let mut data = manga_info_cached(http, root, &manga_id, cache)
        .await?
        .clone();
    data.extend(chapter_data(chapter));
    Ok((uuid, data))
}

fn chapter_data(chapter: &Value) -> Map<String, Value> {
    let attr = chapter.get("attributes").unwrap_or(&Value::Null);
    let lang = match attr.get("translatedLanguage") {
        Some(Value::String(lang)) if !lang.is_empty() => text::partition(lang, "-").0.into(),
        Some(other) => other.clone(),
        None => Value::Null,
    };
    let (number, minor) = match attr.get("chapter").and_then(Value::as_str) {
        Some(ch) if !ch.is_empty() => text::chapter(ch),
        _ => (0, String::new()),
    };
    let groups = relationships(chapter, "scanlation_group")
        .into_iter()
        .map(|g| {
            g.pointer("/attributes/name")
                .cloned()
                .unwrap_or(Value::Null)
        })
        .collect();
    let published = attr
        .get("publishAt")
        .and_then(Value::as_str)
        .and_then(text::iso);
    keywords([
        ("title", get(attr, "title")),
        ("volume", text::parse_int_value(attr.get("volume")).into()),
        ("chapter", number.into()),
        ("chapter_minor", minor.into()),
        ("chapter_id", get(chapter, "id")),
        ("date", text::date(published)),
        ("group", Value::Array(groups)),
        ("lang", lang),
        ("count", get(attr, "pages")),
        ("_external_url", get(attr, "externalUrl")),
    ])
}

/// `MangadexChapterExtractor.items`.
fn chapter_events<'a>(
    http: &'a Http,
    root: String,
    uuid: String,
    data: Option<Map<String, Value>>,
) -> Events<'a> {
    crate::sites::once(async move {
        let data = match data {
            Some(data) => data,
            None => {
                let chapter = call(
                    http,
                    &format!("{root}/chapter/{uuid}"),
                    &[("includes[]".into(), "scanlation_group".into())],
                )
                .await?;
                let chapter = chapter.get("data").unwrap_or(&Value::Null);
                transform(http, &root, chapter, &mut Cache::new()).await?.1
            }
        };
        let external = data
            .get("_external_url")
            .is_some_and(|u| u.as_str().is_some_and(|u| !u.is_empty()));
        if external && data.get("count").and_then(Value::as_i64).unwrap_or(0) == 0 {
            return Err(error(format!(
                "Chapter {}{} is not available on MangaDex and can instead be read \
                 on the official publisher's website",
                data.get("chapter")
                    .map(crate::sites::scalar)
                    .unwrap_or_default(),
                data.get("chapter_minor")
                    .map(crate::sites::scalar)
                    .unwrap_or_default(),
            )));
        }
        let (path, key) = if http.option_bool("data-saver", false) {
            ("data-saver", "dataSaver")
        } else {
            ("data", "data")
        };
        let server = call(http, &format!("{root}/at-home/server/{uuid}"), &[]).await?;
        let base = str_of(&server, "baseUrl").trim_end_matches('/');
        let hash = server
            .pointer("/chapter/hash")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if base.is_empty() || hash.is_empty() || hash.contains(['/', '?', '#']) {
            return Err(error("MangaDex returned no image server"));
        }
        let pages = server
            .pointer(&format!("/chapter/{key}"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|file| !file.contains(['/', '?', '#']))
            .map(|file| (format!("{base}/{path}/{hash}/{file}"), None))
            .collect();
        let timestamp = data.get("date").and_then(Value::as_str).and_then(text::iso);
        Ok(crate::sites::events(
            Gallery::chapter(&CHAPTER, data, pages)
                .uncounted()
                .id(uuid)
                .timestamp(timestamp)
                .header("Referer", "https://mangadex.org/")
                .events(&SITE, http),
        ))
    })
}

/// `MangadexCoversExtractor.items` for one cover.
fn cover_item(uuid: &str, cover: &Value, info: &Map<String, Value>) -> Option<Item> {
    let attr = cover.get("attributes")?;
    let file = attr.get("fileName")?.as_str()?;
    if file.contains(['/', '?', '#']) {
        return None;
    }
    let url = Url::parse(&format!("{ROOT}/covers/{uuid}/{file}")).ok()?;
    let mut data = info.clone();
    let created = attr
        .get("createdAt")
        .and_then(Value::as_str)
        .and_then(text::iso);
    let updated = attr
        .get("updatedAt")
        .and_then(Value::as_str)
        .and_then(text::iso);
    set(&mut data, "cover", file);
    set(&mut data, "lang", get(attr, "locale"));
    set(
        &mut data,
        "volume",
        text::parse_int_value(attr.get("volume")),
    );
    set(&mut data, "date", text::date(created));
    set(&mut data, "date_updated", text::date(updated));
    // text.nameext_from_url on the bare file name.
    let (name, ext) = crate::sites::nameext(&url);
    set(&mut data, "filename", name.clone());
    set(&mut data, "extension", ext);
    set(&mut data, "cover_id", name);
    Some(
        Item::new(url, data)
            .ids(str_of(cover, "id"), str_of(cover, "id"))
            .timestamp(created)
            .header("Referer", "https://mangadex.org/"),
    )
}

#[cfg(test)]
mod tests {
    use super::super::tests::{archive, path};
    use super::*;
    use crate::sites::testing::{http, json, keywords as kw, media, serve};
    use serde_json::json;

    const C1: &str = "11111111-aaaa-bbbb-cccc-000000000001";
    const C2: &str = "11111111-aaaa-bbbb-cccc-000000000002";
    const M: &str = "22222222-aaaa-bbbb-cccc-000000000000";

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t(&format!("https://mangadex.org/chapter/{C1}/2")),
            Some(Target::Chapter(C1.into()))
        );
        assert_eq!(
            t(&format!("https://www.mangadex.cc/title/{M}/some-name")),
            Some(Target::Manga(M.into()))
        );
        assert_eq!(
            t(&format!("https://mangadex.org/manga/{M}")),
            Some(Target::Manga(M.into()))
        );
        assert_eq!(
            t(&format!("https://mangadex.org/title/{M}/name?tab=art")),
            Some(Target::Covers(M.into()))
        );
        assert_eq!(
            t(&format!("https://mangadex.org/list/{M}/name?tab=feed")),
            Some(Target::List {
                uuid: M.into(),
                feed: true
            })
        );
        assert_eq!(
            t(&format!("https://mangadex.org/list/{M}")),
            Some(Target::List {
                uuid: M.into(),
                feed: false
            })
        );
        assert_eq!(
            t(&format!("https://mangadex.org/author/{M}/name")),
            Some(Target::Author(M.into()))
        );
        assert_eq!(t("https://mangadex.org/title/feed"), Some(Target::Feed));
        assert_eq!(t("https://mangadex.org/titles/feed"), Some(Target::Feed));
        assert_eq!(
            t("https://mangadex.org/title/follows"),
            Some(Target::Following)
        );
        for raw in [
            "https://mangadex.org/chapter/XYZ",
            "https://mangadex.org.evil.test/chapter/abc",
            "https://evilmangadex.org/chapter/abc",
            "https://mangadex.org:8080/chapter/abc",
            "https://user@mangadex.org/chapter/abc",
            "https://api.mangadex.org/chapter/abc",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn manga() -> Value {
        json!({"data": {"id": M, "type": "manga", "attributes": {
            "title": {"ja-ro": "Romaji", "en": "English Title"},
            "altTitles": [{"ja": "日本"}, {"en": "Alt"}],
            "description": {"en": ""},
            "createdAt": "2018-11-21T05:37:56+00:00",
            "publicationDemographic": "seinen", "originalLanguage": "ja",
            "status": "ongoing", "year": 2018, "contentRating": "safe",
            "links": {"al": "1"},
            "tags": [{"attributes": {"name": {"en": "Comedy"}}}]},
            "relationships": [
                {"type": "author", "attributes": {"name": "Writer"}},
                {"type": "artist", "attributes": {"name": "Painter"}}]}})
    }

    fn chapter(id: &str, number: &str) -> Value {
        json!({"id": id, "type": "chapter", "attributes": {
            "title": "Start", "volume": "2", "chapter": number,
            "translatedLanguage": "pt-br", "publishAt": "2017-07-14T02:40:00+00:00",
            "pages": 2, "externalUrl": null},
            "relationships": [
                {"type": "scanlation_group", "attributes": {"name": "Group"}},
                {"type": "manga", "id": M}]})
    }

    fn server(hash: &str) -> Value {
        json!({"baseUrl": "https://node.mangadex.network",
            "chapter": {"hash": hash, "data": ["1-a.png", "2-b.jpg"],
            "dataSaver": ["1-a.jpg"]}})
    }

    #[tokio::test]
    async fn extracts_chapter_with_manga_info() {
        let (root, requests) = serve(vec![
            json(json!({"data": chapter(C1, "12.5")})),
            json(manga()),
            json(server("h1")),
        ]);
        let http = http("MangaDex", root);
        let files = media(extract(
            &http,
            Url::parse(&format!("https://mangadex.org/chapter/{C1}")).unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[1].url.as_str(),
            "https://node.mangadex.network/data/h1/2-b.jpg"
        );
        let k = kw(&files[0]);
        assert_eq!(k["manga"], "English Title");
        assert_eq!(k["manga_titles"], json!(["日本", "Alt"]));
        assert_eq!(k["description"], "");
        assert_eq!(k["manga_date"], "2018-11-21 05:37:56");
        assert_eq!(k["tags"], json!(["Comedy"]));
        assert_eq!(k["author"], json!(["Writer"]));
        assert_eq!(k["artist"], json!(["Painter"]));
        assert_eq!(k["chapter"], 12);
        assert_eq!(k["chapter_minor"], ".5");
        assert_eq!(k["volume"], 2);
        assert_eq!(k["lang"], "pt");
        assert_eq!(k["group"], json!(["Group"]));
        assert_eq!(k["date"], "2017-07-14 02:40:00");
        assert_eq!(k["count"], 2);
        assert_eq!(k["page"], 1);
        assert_eq!(
            files[0].metadata.as_ref().unwrap().timestamp,
            Some(1500000000)
        );
        assert_eq!(files[0].headers["Referer"], "https://mangadex.org/");
        // Windows path sanitization replaces the colon in the chapter title.
        let colon = if cfg!(windows) { "_" } else { ":" };
        assert_eq!(
            path(&files[0]),
            format!("mangadex/English Title/v02 c012.5{colon} Start/English Title_c012.5_001.png")
        );
        assert_eq!(
            archive(&files[1]),
            format!(r#"["{{chapter_id}}_{{page}}",["{C1}","2"]]"#)
        );
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with(&format!(
            "GET /chapter/{C1}?includes%5B%5D=scanlation_group "
        )));
        assert!(requests[1].starts_with(&format!(
            "GET /manga/{M}?includes%5B%5D=artist&includes%5B%5D=author "
        )));
        assert!(requests[2].starts_with(&format!("GET /at-home/server/{C1} ")));
    }

    #[tokio::test]
    async fn external_chapters_fail_without_leaking_urls() {
        let mut ch = chapter(C1, "3");
        ch["attributes"]["pages"] = 0.into();
        ch["attributes"]["externalUrl"] = "https://secret.test/x".into();
        let (root, _requests) = serve(vec![json(json!({"data": ch})), json(manga())]);
        let http = http("MangaDex", root);
        let mut events = extract(
            &http,
            Url::parse(&format!("https://mangadex.org/chapter/{C1}")).unwrap(),
        );
        let error = loop {
            match events.next().await {
                Some(Err(error)) => break error.to_string(),
                Some(Ok(_)) => continue,
                None => panic!("expected an error"),
            }
        };
        assert!(error.contains("Chapter 3 is not available"), "{error}");
        assert!(!error.contains("secret"));
    }

    #[tokio::test]
    async fn manga_feed_paginates_and_reuses_queued_data() {
        let page = |id, number, offset| json!({"data": [chapter(id, number)], "offset": offset, "limit": 1, "total": 2});
        let (root, requests) = serve(vec![
            json(page(C1, "1", 0)),
            json(manga()),
            json(server("h1")),
            json(page(C2, "2", 1)),
            json(server("h2")),
        ]);
        let http = http("MangaDex", root);
        let files = media(extract(
            &http,
            Url::parse(&format!("https://mangadex.org/title/{M}/slug")).unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 4);
        assert_eq!(kw(&files[2])["chapter"], 2);
        assert_eq!(kw(&files[2])["subcategory"], "chapter");
        let requests = requests.join().unwrap();
        assert!(
            requests[0].starts_with(&format!(
                "GET /manga/{M}/feed?order%5Bvolume%5D=asc&order%5Bchapter%5D=asc\
                 &includes%5B%5D=scanlation_group&contentRating%5B%5D=safe\
                 &contentRating%5B%5D=suggestive&contentRating%5B%5D=erotica\
                 &contentRating%5B%5D=pornographic&offset=0 "
            )),
            "{}",
            requests[0]
        );
        assert!(requests[2].starts_with(&format!("GET /at-home/server/{C1} ")));
        assert!(requests[3].contains("&offset=1 "));
        assert!(requests[4].starts_with(&format!("GET /at-home/server/{C2} ")));
    }

    #[tokio::test]
    async fn options_select_language_ratings_and_data_saver() {
        let (root, requests) = serve(vec![
            json(json!({"data": [chapter(C1, "1")], "offset": 0, "limit": 100, "total": 1})),
            json(manga()),
            json(server("h1")),
        ]);
        let options = json!({"lang": "en,fr", "ratings": ["safe"], "data-saver": true,
            "chapter-reverse": true});
        let http = http("MangaDex", root).with_options(Some(std::sync::Arc::new(
            options.as_object().unwrap().clone(),
        )));
        let files = media(extract(
            &http,
            Url::parse(&format!("https://mangadex.org/title/{M}")).unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].url.as_str(),
            "https://node.mangadex.network/data-saver/h1/1-a.jpg"
        );
        let requests = requests.join().unwrap();
        assert!(
            requests[0].starts_with(&format!(
                "GET /manga/{M}/feed?order%5Bvolume%5D=desc&order%5Bchapter%5D=desc\
                 &translatedLanguage%5B%5D=en&translatedLanguage%5B%5D=fr\
                 &includes%5B%5D=scanlation_group&contentRating%5B%5D=safe&offset=0 "
            )),
            "{}",
            requests[0]
        );
    }

    #[tokio::test]
    async fn extracts_covers() {
        let cover = json!({"id": "cv1", "attributes": {"fileName": "abc.png", "locale": "ja",
            "volume": "3", "createdAt": "2017-07-14T02:40:00+00:00",
            "updatedAt": "2017-07-14T02:40:00+00:00"},
            "relationships": [{"type": "manga", "id": M}]});
        let (root, requests) = serve(vec![
            json(json!({"data": [cover], "offset": 0, "limit": 10, "total": 1})),
            json(manga()),
        ]);
        let http = http("MangaDex", root);
        let files = media(extract(
            &http,
            Url::parse(&format!("https://mangadex.org/title/{M}?tab=art")).unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].url.as_str(),
            format!("https://mangadex.org/covers/{M}/abc.png")
        );
        let k = kw(&files[0]);
        assert_eq!(k["cover_id"], "abc");
        assert_eq!(k["volume"], 3);
        assert_eq!(path(&files[0]), "mangadex/English Title/Covers/03_ja.png");
        assert_eq!(archive(&files[0]), r#"["c_{cover_id}",["abc"]]"#);
        let requests = requests.join().unwrap();
        assert!(
            requests[0].starts_with(&format!(
                "GET /cover?manga%5B%5D={M}&order%5Bvolume%5D=asc&offset=0 "
            )),
            "{}",
            requests[0]
        );
    }

    #[tokio::test]
    async fn list_and_author_queue_titles() {
        let (root, requests) = serve(vec![
            json(json!({"data": {"relationships": [
                {"type": "user", "id": "aaaa"}, {"type": "manga", "id": M}]}})),
            json(json!({"data": [], "offset": 0, "limit": 100, "total": 0})),
        ]);
        let http = http("MangaDex", root);
        let files = media(extract(
            &http,
            Url::parse(&format!("https://mangadex.org/list/{C1}/name")).unwrap(),
        ))
        .await;
        assert!(files.is_empty());
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with(&format!("GET /list/{C1} ")));
        assert!(requests[1].starts_with(&format!("GET /manga/{M}/feed?")));
    }
}
