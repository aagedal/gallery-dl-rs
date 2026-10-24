//! Iwara images, videos, playlists, profiles, follow lists, searches and
//! tag listings, mirroring gallery_dl/extractor/iwara.py through the
//! `apiq.iwara.tv` API.
//!
//! Videos resolve to direct download URLs like upstream: a file's source
//! list is requested with the `X-Version` SHA-1 of its id and expiry, and
//! the best (or the `format` option's) source is emitted. Upstream's
//! `username`/`password` login is not supported, so favorites are
//! unavailable.
use super::{parse_int, parse_query, str_of};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use futures_util::{StreamExt, TryStreamExt, stream};
use serde_json::{Map, Value};
use sha1::{Digest, Sha1};
use url::Url;

const API: &str = "https://apiq.iwara.tv";
const FILENAME: &str = "{date} {id} {title[:200]} {filename}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{user[name]}"];
const ARCHIVE: &str = "{type} {user[name]} {id} {file_id}";
static USER: Route = Route::new("user", FILENAME, DIRECTORY, ARCHIVE);
static USER_IMAGES: Route = Route::new("user-images", FILENAME, DIRECTORY, ARCHIVE);
static USER_VIDEOS: Route = Route::new("user-videos", FILENAME, DIRECTORY, ARCHIVE);
static USER_PLAYLISTS: Route = Route::new("user-playlists", FILENAME, DIRECTORY, ARCHIVE);
static FOLLOWING: Route = Route::new("following", FILENAME, DIRECTORY, ARCHIVE);
static FOLLOWERS: Route = Route::new("followers", FILENAME, DIRECTORY, ARCHIVE);
static IMAGE: Route = Route::new("image", FILENAME, DIRECTORY, ARCHIVE);
static VIDEO: Route = Route::new("video", FILENAME, DIRECTORY, ARCHIVE);
static PLAYLIST: Route = Route::new("playlist", FILENAME, DIRECTORY, ARCHIVE);
static FAVORITE: Route = Route::new("favorite", FILENAME, DIRECTORY, ARCHIVE);
static SEARCH: Route = Route::new("search", FILENAME, DIRECTORY, ARCHIVE);
static TAG: Route = Route::new("tag", FILENAME, DIRECTORY, ARCHIVE);

pub static SITE: Site = Site {
    category: "iwara",
    name: "Iwara",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[
        &USER,
        &USER_IMAGES,
        &USER_VIDEOS,
        &USER_PLAYLISTS,
        &FOLLOWING,
        &FOLLOWERS,
        &IMAGE,
        &VIDEO,
        &PLAYLIST,
        &FAVORITE,
        &SEARCH,
        &TAG,
    ],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    User(String),
    /// A profile listing's route, the user and its query string.
    UserListing(&'static str, String, String),
    Following(String),
    Followers(String),
    Image(String),
    Video(String),
    Playlist(String),
    Favorite,
    Search(String),
    Tag(&'static str, String),
}

/// `iwara\.(tv|ai)` paths, raw as upstream sees them, and the TLD.
fn target(url: &Url) -> Option<(String, Target)> {
    let tld = ["tv", "ai"]
        .into_iter()
        .find(|tld| super::on_host(url, &[&format!("iwara.{tld}")]))?;
    let path = url.path();
    let query = url.query().unwrap_or_default().to_owned();
    let segments = super::raw_segments(url);
    let first = |text: &str| {
        text.split(['/', '?', '#'])
            .next()
            .unwrap_or_default()
            .to_owned()
    };
    let target = match segments.as_slice() {
        ["profile", user, rest @ ..] if path.starts_with(&format!("/profile/{user}")) => {
            let user = (*user).to_owned();
            match rest.first().copied() {
                None if url.query().is_none() && url.fragment().is_none() => Target::User(user),
                Some("images") => Target::UserListing("user-images", user, query),
                Some("videos") => Target::UserListing("user-videos", user, query),
                Some("playlists") => Target::UserListing("user-playlists", user, query),
                Some(next) if next.starts_with("following") => Target::Following(user),
                Some(next) if next.starts_with("followers") => Target::Followers(user),
                _ => return None,
            }
        }
        ["image", id, ..] => Target::Image(first(id)),
        ["video", id, ..] => Target::Video(first(id)),
        ["playlist", id, ..] => Target::Playlist(first(id)),
        [favorites, ..] if favorites.starts_with("favorites") => Target::Favorite,
        ["search"] if !query.is_empty() => Target::Search(query),
        ["images", ..] => Target::Tag("image", query),
        ["videos", ..] => Target::Tag("video", query),
        _ => return None,
    };
    Some((tld.to_owned(), target))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some((tld, target)) = target(&url) else {
        return crate::sites::fail(error("unsupported Iwara URL"));
    };
    let api = Api {
        http,
        root: format!("https://www.iwara.{tld}"),
    };
    match target {
        Target::User(user) => dispatch(http, &api.root, &user),
        Target::UserListing(subcategory, user, query) => crate::sites::once(async move {
            let (info, params) = api.user_params(&user, &query).await?;
            Ok(match subcategory {
                "user-images" => {
                    let results = api.paginate("/images", rating(params));
                    images(api, &USER_IMAGES, results, Some(info), Map::new())
                }
                "user-videos" => {
                    let results = api.paginate("/videos", rating(params));
                    videos(api, &USER_VIDEOS, results, Some(info), Map::new())
                }
                _ => {
                    let results = api.paginate("/playlists", params);
                    playlists(api, results)
                }
            })
        }),
        Target::Following(user) => follows(api, FOLLOWING.subcategory, user, "following", "user"),
        Target::Followers(user) => {
            follows(api, FOLLOWERS.subcategory, user, "followers", "follower")
        }
        Target::Image(id) => {
            let call = api.clone();
            let image = stream::once(async move { call.call(&format!("/image/{id}"), &[]).await });
            images(api, &IMAGE, image.boxed(), None, Map::new())
        }
        Target::Video(id) => {
            let call = api.clone();
            let video = stream::once(async move { call.call(&format!("/video/{id}"), &[]).await });
            videos(api, &VIDEO, video.boxed(), None, Map::new())
        }
        Target::Playlist(id) => {
            let results = api.paginate(&format!("/playlist/{id}"), Vec::new());
            videos(api, &PLAYLIST, results, None, Map::new())
        }
        Target::Favorite => crate::sites::fail(error(
            "Iwara favorites need a login, which is not supported",
        )),
        Target::Search(query) => {
            let params = parse_query(&query);
            let get = |name: &str| {
                params
                    .iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, v)| v.clone())
            };
            let mut kind = get("type")
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| "videos".into());
            if !kind.ends_with('s') {
                kind.push('s');
            }
            let search = get("query");
            let mut data = Map::new();
            data.insert(
                "search_tags".into(),
                search.clone().map_or(Value::Null, Value::from),
            );
            let mut params = vec![("type".to_owned(), kind.clone())];
            params.extend(search.map(|q| ("query".to_owned(), q)));
            let results = api.paginate("/search", params);
            by_type(api, &SEARCH, &kind[..kind.len() - 1], results, data)
        }
        Target::Tag(kind, query) => {
            let params = parse_query(&query);
            let mut data = Map::new();
            let tags = params
                .iter()
                .find(|(n, _)| n == "tags")
                .map(|(_, v)| v.as_str());
            data.insert("search_tags".into(), tags.map_or(Value::Null, Value::from));
            let results = api.paginate(&format!("/{kind}s"), rating(params));
            by_type(api, &TAG, kind, results, data)
        }
    }
}

/// `params.setdefault("rating", "all")`.
fn rating(mut params: Vec<(String, String)>) -> Vec<(String, String)> {
    if !params.iter().any(|(name, _)| name == "rating") {
        params.push(("rating".into(), "all".into()));
    }
    params
}

/// Upstream `IwaraUserExtractor`: queue the `include` listings.
fn dispatch<'a>(http: &'a Http, root: &str, user: &str) -> Events<'a> {
    let base = format!("{root}/profile/{user}/");
    let children: Vec<Url> = super::include(
        http,
        &["user-images", "user-videos", "user-playlists"],
        &["user-images", "user-videos"],
    )
    .into_iter()
    .filter_map(|sub| Url::parse(&format!("{base}{}", &sub["user-".len()..])).ok())
    .collect();
    stream::iter(children)
        .map(move |child| crate::sites::queue(http, "iwara", "user", child, &Map::new()))
        .flatten()
        .boxed()
}

type Results<'a> = futures_util::stream::BoxStream<'a, Result<Value>>;

/// The Iwara API for one site root (`www.iwara.tv` or `www.iwara.ai`).
#[derive(Clone)]
struct Api<'a> {
    http: &'a Http,
    root: String,
}

impl<'a> Api<'a> {
    async fn call(&self, endpoint: &str, params: &[(String, String)]) -> Result<Value> {
        let mut url = self.http.api(&format!("{API}{endpoint}"))?;
        if !params.is_empty() {
            url.query_pairs_mut().extend_pairs(params);
        }
        let referer = format!("{}/", self.root);
        let site = &self.root["https://".len()..];
        self.http
            .json_with(
                &url,
                &[
                    ("Referer", referer.as_str()),
                    ("Content-Type", "application/json"),
                    ("Origin", self.root.as_str()),
                    ("X-Site", site),
                ],
            )
            .await
    }

    /// Upstream `_pagination`: pages of 50 `results`.
    fn paginate(&self, endpoint: &str, params: Vec<(String, String)>) -> Results<'a> {
        let api = self.clone();
        let endpoint = endpoint.to_owned();
        stream::try_unfold(Some(0u64), move |page| {
            let (api, endpoint, mut params) = (api.clone(), endpoint.clone(), params.clone());
            async move {
                let Some(page) = page else {
                    return Ok(None);
                };
                params.push(("page".into(), page.to_string()));
                params.push(("limit".into(), "50".into()));
                let data = api.call(&endpoint, &params).await?;
                let results = data
                    .get("results")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let next = (results.len() >= 50).then_some(page + 1);
                Ok::<_, crate::Error>(Some((stream::iter(results.into_iter().map(Ok)), next)))
            }
        })
        .try_flatten()
        .boxed()
    }

    /// Upstream `_user_params`: the profile's user info and the listing's
    /// query with its `user` id.
    async fn user_params(
        &self,
        user: &str,
        query: &str,
    ) -> Result<(Map<String, Value>, Vec<(String, String)>)> {
        let profile = self.call(&format!("/profile/{user}"), &[]).await?;
        let id = profile
            .pointer("/user/id")
            .map(crate::sites::scalar)
            .ok_or_else(|| error("Iwara profile has no user id"))?;
        let mut params: Vec<(String, String)> = parse_query(query)
            .into_iter()
            .filter(|(name, _)| name != "user")
            .collect();
        params.push(("user".into(), id));
        Ok((
            user_info(&profile).ok_or_else(|| error("invalid Iwara profile"))?,
            params,
        ))
    }
}

/// Upstream `extract_user_info`.
fn user_info(profile: &Value) -> Option<Map<String, Value>> {
    let empty = Value::Object(Map::new());
    let user = profile
        .get("user")
        .filter(|u| !u.is_null())
        .unwrap_or(&empty);
    let get = |key: &str| user.get(key).cloned().unwrap_or(Value::Null);
    let mut info = Map::new();
    info.insert("id".into(), get("id"));
    info.insert("name".into(), get("username"));
    info.insert("nick".into(), user.get("name")?.as_str()?.trim().into());
    info.insert("status".into(), get("status"));
    info.insert("role".into(), get("role"));
    info.insert("premium".into(), get("premium"));
    info.insert("date".into(), iso_date(user.get("createdAt")));
    info.insert(
        "description".into(),
        profile.get("body").cloned().unwrap_or(Value::Null),
    );
    Some(info)
}

fn iso_date(value: Option<&Value>) -> Value {
    super::date(value.and_then(Value::as_str).and_then(super::parse_iso))
}

/// Upstream `extract_media_info`; `file` is the object holding the file
/// fields, if they are included.
fn media_info(item: &Value, file: Option<&Value>) -> Map<String, Value> {
    let get = |key: &str| item.get(key).cloned().unwrap_or(Value::Null);
    let text = |key: &str| {
        item.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_owned()
    };
    let mut info = Map::new();
    info.insert("id".into(), get("id"));
    info.insert("slug".into(), get("slug"));
    info.insert("rating".into(), get("rating"));
    info.insert("likes".into(), get("numLikes"));
    info.insert("views".into(), get("numViews"));
    info.insert("comments".into(), get("numComments"));
    let tags: Vec<Value> = item
        .get("tags")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|tag| tag.get("id").cloned().unwrap_or(Value::Null))
        .collect();
    info.insert("tags".into(), tags.into());
    info.insert("title".into(), text("title").into());
    info.insert("description".into(), text("body").into());
    if let Some(file) = file {
        let get = |key: &str| file.get(key).cloned().unwrap_or(Value::Null);
        let name = str_of(file, "name");
        let (filename, extension) = name.rsplit_once('.').unwrap_or(("", name));
        info.insert("file_id".into(), get("id"));
        info.insert("filename".into(), filename.into());
        info.insert("extension".into(), extension.into());
        info.insert("date".into(), iso_date(file.get("createdAt")));
        info.insert("date_updated".into(), iso_date(file.get("updatedAt")));
        info.insert("mime".into(), get("mime"));
        info.insert("size".into(), get("size"));
        info.insert("width".into(), get("width"));
        info.insert("height".into(), get("height"));
        info.insert("duration".into(), get("duration"));
        info.insert("type".into(), get("type"));
    }
    info
}

/// Upstream `items_by_type`.
fn by_type<'a>(
    api: Api<'a>,
    route: &'static Route,
    kind: &str,
    results: Results<'a>,
    data: Map<String, Value>,
) -> Events<'a> {
    match kind {
        "image" => images(api, route, results, None, data),
        "video" => videos(api, route, results, None, data),
        "user" => users(api.http, &api.root, route.subcategory, results, None),
        _ => crate::sites::fail(error(format!("Iwara: unsupported result type '{kind}'"))),
    }
}

/// A listing entry's wrapped object (`{"image": {...}}`), else the entry.
fn unwrap<'v>(item: &'v Value, key: &str) -> &'v Value {
    item.get(key)
        .filter(|inner| inner.is_object())
        .unwrap_or(item)
}

/// Upstream `items_image`: one post per image with its files; images that
/// fail to load are skipped.
fn images<'a>(
    api: Api<'a>,
    route: &'static Route,
    results: Results<'a>,
    user: Option<Map<String, Value>>,
    data: Map<String, Value>,
) -> Events<'a> {
    results
        .map(move |image| {
            let (api, user, data) = (api.clone(), user.clone(), data.clone());
            crate::sites::once(async move {
                let image = image?;
                let mut image = unwrap(&image, "image").clone();
                if !super::truthy(image.get("files")) {
                    let id = image
                        .get("id")
                        .map(crate::sites::scalar)
                        .unwrap_or_default();
                    image = match api.call(&format!("/image/{id}"), &[]).await {
                        Ok(image) => image,
                        Err(_) => return Ok(crate::sites::events(Vec::new())),
                    };
                }
                let items = image_items(&image, user, &data).unwrap_or_default();
                Ok(crate::sites::events(crate::sites::post(
                    &SITE, api.http, route, items,
                )))
            })
        })
        .flatten()
        .boxed()
}

/// One image's files with upstream keywords.
pub(crate) fn image_items(
    image: &Value,
    user: Option<Map<String, Value>>,
    data: &Map<String, Value>,
) -> Option<Vec<Item>> {
    let files = image.get("files")?.as_array()?;
    let mut group = media_info(image, None);
    let user = match user {
        Some(user) => user,
        None => user_info(image)?,
    };
    let user_name = user.get("name").and_then(Value::as_str).map(str::to_owned);
    group.insert("user".into(), user.into());
    group.insert("type".into(), "image".into());
    group.insert("count".into(), files.len().into());
    let id = group
        .get("id")
        .map(crate::sites::scalar)
        .unwrap_or_default();
    let mut items = Vec::new();
    for (index, file) in files.iter().enumerate() {
        let mut fields = media_info(file, Some(file));
        super::update(&mut fields, &group);
        fields.insert("num".into(), (index + 1).into());
        super::update(&mut fields, data);
        let file_id = file.get("id").map(crate::sites::scalar).unwrap_or_default();
        let extension = fields
            .get("extension")
            .map(crate::sites::scalar)
            .unwrap_or_default();
        let raw = format!("https://i.iwara.tv/image/original/{file_id}/{file_id}.{extension}");
        let Ok(url) = Url::parse(&raw) else {
            continue;
        };
        let timestamp = file
            .get("createdAt")
            .and_then(Value::as_str)
            .and_then(super::parse_iso);
        let title = fields
            .get("title")
            .and_then(Value::as_str)
            .map(str::to_owned);
        items.push(
            Item::new(url, fields)
                .ids(id.clone(), file_id)
                .user(user_name.clone())
                .caption(title)
                .timestamp(timestamp),
        );
    }
    Some(items)
}

/// Upstream `items_video`: one file per video, from its best source;
/// videos that fail to resolve are skipped.
fn videos<'a>(
    api: Api<'a>,
    route: &'static Route,
    results: Results<'a>,
    user: Option<Map<String, Value>>,
    data: Map<String, Value>,
) -> Events<'a> {
    let formats = formats(api.http);
    results
        .map(move |video| {
            let (api, user, data, formats) =
                (api.clone(), user.clone(), data.clone(), formats.clone());
            crate::sites::once(async move {
                let video = video?;
                let mut video = unwrap(&video, "video").clone();
                if video.get("fileUrl").is_none() {
                    let id = video
                        .get("id")
                        .map(crate::sites::scalar)
                        .unwrap_or_default();
                    video = match api.call(&format!("/video/{id}"), &[]).await {
                        Ok(video) => video,
                        Err(_) => return Ok(crate::sites::events(Vec::new())),
                    };
                }
                let sources = match source(&api, str_of(&video, "fileUrl")).await {
                    Ok(sources) => sources,
                    Err(_) => return Ok(crate::sites::events(Vec::new())),
                };
                let items: Vec<Item> =
                    video_item(&video, &sources, formats.as_deref(), user, &data)
                        .into_iter()
                        .collect();
                Ok(crate::sites::events(crate::sites::post(
                    &SITE, api.http, route, items,
                )))
            })
        })
        .flatten()
        .boxed()
}

/// The `format` option as lowercase names.
fn formats(http: &Http) -> Option<Vec<String>> {
    match http.option("format")? {
        Value::Null | Value::Bool(false) => None,
        Value::String(s) if s.is_empty() => None,
        Value::String(s) => Some(
            s.replace(' ', "")
                .to_lowercase()
                .split(',')
                .map(str::to_owned)
                .collect(),
        ),
        Value::Array(items) if items.is_empty() => None,
        Value::Array(items) => Some(items.iter().map(crate::sites::scalar).collect()),
        other => Some(vec![crate::sites::scalar(other)]),
    }
}

/// The SHA-1 `X-Version` header for a file URL, or `None` without an
/// `expires=` parameter followed by another one.
pub(crate) fn x_version(file_url: &str) -> Option<String> {
    let (base, query) = file_url.split_once('?').unwrap_or((file_url, ""));
    let expires = super::extr(query, "expires=", "&");
    if expires.is_empty() {
        return None;
    }
    let file_id = base.rsplit('/').next().unwrap_or_default();
    let key = format!("{file_id}_{expires}_mSvL05GfEmeEmsEYfGCnVpEjYgTJraJN");
    Some(format!("{:x}", Sha1::digest(key.as_bytes())))
}

/// Upstream `IwaraAPI.source`: the file's source list.
async fn source(api: &Api<'_>, file_url: &str) -> Result<Vec<Value>> {
    let Some(hash) = x_version(file_url) else {
        return Ok(Vec::new());
    };
    let url = super::url(file_url)?;
    if !crate::sites::plain(&url) {
        return Err(error("invalid Iwara file URL"));
    }
    let referer = format!("{}/", api.root);
    let site = &api.root["https://".len()..];
    let sources = api
        .http
        .json_with(
            &api.http.api(url.as_str())?,
            &[
                ("X-Version", hash.as_str()),
                ("Referer", referer.as_str()),
                ("Content-Type", "application/json"),
                ("Origin", api.root.as_str()),
                ("X-Site", site),
            ],
        )
        .await?;
    Ok(sources.as_array().cloned().unwrap_or_default())
}

/// Upstream `extract_video_source`: the highest quality source, `Source`
/// first; or the first available of the `format` option's names.
pub(crate) fn pick_source<'s>(
    sources: &'s [Value],
    formats: Option<&[String]>,
) -> Option<&'s Value> {
    let has_src = |source: &Value| super::truthy(source.get("src"));
    fn name(source: &Value) -> Option<&str> {
        source.get("name").and_then(Value::as_str)
    }
    match formats {
        Some(formats) => formats.iter().find_map(|format| {
            sources
                .iter()
                .rev()
                .filter(|s| has_src(s))
                .find(|s| name(s).is_some_and(|n| !n.is_empty() && n.to_lowercase() == *format))
        }),
        None => {
            let rank = |source: &Value| match name(source) {
                _ if !has_src(source) => 0,
                Some("Source") => 99999,
                Some(name) => parse_int(name, 0),
                None => 0,
            };
            // The first of the best, as Python's stable reverse sort keeps.
            let best = sources.iter().map(rank).max()?;
            sources.iter().find(|s| rank(s) == best)
        }
    }
}

/// One video's file with upstream keywords.
pub(crate) fn video_item(
    video: &Value,
    sources: &[Value],
    formats: Option<&[String]>,
    user: Option<Map<String, Value>>,
    data: &Map<String, Value>,
) -> Option<Item> {
    let source = pick_source(sources, formats)?;
    let download = source.get("src")?.get("download")?.as_str()?;
    // Upstream `item.get(key) or {}`: a missing file has no file fields.
    let none = Value::Null;
    let file = video.get("file").unwrap_or(&none);
    let mut info = media_info(video, Some(file));
    info.insert(
        "format".into(),
        source.get("name").cloned().unwrap_or(Value::Null),
    );
    info.insert("count".into(), 1.into());
    info.insert("num".into(), 1.into());
    let user = match user {
        Some(user) => user,
        None => user_info(video)?,
    };
    let user_name = user.get("name").and_then(Value::as_str).map(str::to_owned);
    info.insert("user".into(), user.into());
    super::update(&mut info, data);
    let url = Url::parse(&format!("https:{download}"))
        .ok()
        .filter(crate::sites::plain)?;
    let id = info.get("id").map(crate::sites::scalar).unwrap_or_default();
    let file_id = info
        .get("file_id")
        .map(crate::sites::scalar)
        .unwrap_or_default();
    let title = info.get("title").and_then(Value::as_str).map(str::to_owned);
    let timestamp = video
        .pointer("/file/createdAt")
        .and_then(Value::as_str)
        .and_then(super::parse_iso);
    Some(
        Item::new(url, info)
            .ids(id, file_id)
            .user(user_name)
            .caption(title)
            .timestamp(timestamp),
    )
}

/// Upstream `items_user`: queue each user's profile.
fn users<'a>(
    http: &'a Http,
    root: &str,
    subcategory: &'static str,
    results: Results<'a>,
    key: Option<&'static str>,
) -> Events<'a> {
    let base = format!("{root}/profile/");
    results
        .map(move |user| match user {
            Ok(user) => {
                let user = match key {
                    Some(key) => user.get(key).cloned().unwrap_or(Value::Null),
                    None => user,
                };
                let (Some(mut data), Some(name)) = (
                    user.as_object().cloned(),
                    user.get("username").and_then(Value::as_str),
                ) else {
                    return crate::sites::events(Vec::new());
                };
                data.insert("type".into(), "user".into());
                match Url::parse(&format!("{base}{name}")) {
                    Ok(child) if target(&child).is_some() => {
                        crate::sites::queue(http, "iwara", subcategory, child, &data)
                    }
                    _ => crate::sites::events(Vec::new()),
                }
            }
            Err(e) => crate::sites::fail(e),
        })
        .flatten()
        .boxed()
}

/// Upstream `IwaraFollowingExtractor`/`IwaraFollowersExtractor`.
fn follows<'a>(
    api: Api<'a>,
    subcategory: &'static str,
    user: String,
    endpoint: &'static str,
    key: &'static str,
) -> Events<'a> {
    crate::sites::once(async move {
        let profile = api.call(&format!("/profile/{user}"), &[]).await?;
        let id = profile
            .pointer("/user/id")
            .map(crate::sites::scalar)
            .ok_or_else(|| error("Iwara profile has no user id"))?;
        let results = api.paginate(&format!("/user/{id}/{endpoint}"), Vec::new());
        Ok(users(api.http, &api.root, subcategory, results, Some(key)))
    })
}

/// Upstream `IwaraUserPlaylistsExtractor`: queue each playlist.
fn playlists<'a>(api: Api<'a>, results: Results<'a>) -> Events<'a> {
    let base = format!("{}/playlist/", api.root);
    let http = api.http;
    results
        .map(move |playlist| match playlist {
            Ok(Value::Object(mut data)) => {
                data.insert("type".into(), "playlist".into());
                let id = data.get("id").map(crate::sites::scalar).unwrap_or_default();
                match Url::parse(&format!("{base}{id}")) {
                    Ok(child) if target(&child).is_some() => {
                        crate::sites::queue(http, "iwara", "user-playlists", child, &data)
                    }
                    _ => crate::sites::events(Vec::new()),
                }
            }
            Ok(_) => crate::sites::events(Vec::new()),
            Err(e) => crate::sites::fail(e),
        })
        .flatten()
        .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http, json, keywords, media, queued, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap()).map(|(_, t)| t);
        assert_eq!(
            t("https://www.iwara.tv/profile/user"),
            Some(Target::User("user".into()))
        );
        assert_eq!(
            t("https://iwara.ai/profile/user/images?sort=likes"),
            Some(Target::UserListing(
                "user-images",
                "user".into(),
                "sort=likes".into()
            ))
        );
        assert_eq!(
            t("https://www.iwara.tv/profile/user/videos"),
            Some(Target::UserListing(
                "user-videos",
                "user".into(),
                String::new()
            ))
        );
        assert_eq!(
            t("https://www.iwara.tv/profile/user/playlists"),
            Some(Target::UserListing(
                "user-playlists",
                "user".into(),
                String::new()
            ))
        );
        assert_eq!(
            t("https://www.iwara.tv/profile/u/following"),
            Some(Target::Following("u".into()))
        );
        assert_eq!(
            t("https://www.iwara.tv/profile/u/followers"),
            Some(Target::Followers("u".into()))
        );
        assert_eq!(
            t("https://www.iwara.tv/image/abc/slug"),
            Some(Target::Image("abc".into()))
        );
        assert_eq!(
            t("https://www.iwara.tv/video/abc"),
            Some(Target::Video("abc".into()))
        );
        assert_eq!(
            t("https://www.iwara.tv/playlist/abc"),
            Some(Target::Playlist("abc".into()))
        );
        assert_eq!(
            t("https://www.iwara.tv/favorites/videos"),
            Some(Target::Favorite)
        );
        assert_eq!(
            t("https://www.iwara.tv/search?query=a&type=image"),
            Some(Target::Search("query=a&type=image".into()))
        );
        assert_eq!(
            t("https://www.iwara.tv/videos?tags=a"),
            Some(Target::Tag("video", "tags=a".into()))
        );
        assert_eq!(
            target(&Url::parse("https://iwara.ai/video/x").unwrap()).map(|(tld, _)| tld),
            Some("ai".into())
        );
        for raw in [
            "https://www.iwara.tv/profile/user?x=1",
            "https://www.iwara.tv/profile/user/other",
            "https://www.iwara.tv/search",
            "https://www.iwara.tv/",
            "https://apiq.iwara.tv/video/x",
            "https://www.iwara.tv.evil.com/video/x",
            "https://www.iwara.tv:8080/video/x",
            "https://u:p@www.iwara.tv/video/x",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[test]
    fn versions_and_sources() {
        let hash = x_version("https://files.iwara.tv/file/abc?expires=123&hash=x").unwrap();
        let expected = format!(
            "{:x}",
            Sha1::digest(b"abc_123_mSvL05GfEmeEmsEYfGCnVpEjYgTJraJN")
        );
        assert_eq!(hash, expected);
        assert_eq!(
            x_version("https://files.iwara.tv/file/abc?expires=123"),
            None
        );
        let sources = vec![
            json!({"name": "360", "src": {"download": "//a"}}),
            json!({"name": "Source", "src": null}),
            json!({"name": "540", "src": {"download": "//b"}}),
            json!({"name": "preview", "src": {"download": "//c"}}),
        ];
        assert_eq!(pick_source(&sources, None).unwrap()["name"], "540");
        let wanted = ["source".to_owned(), "360".to_owned()];
        assert_eq!(pick_source(&sources, Some(&wanted)).unwrap()["name"], "360");
        assert!(pick_source(&sources, Some(&["720".to_owned()])).is_none());
    }

    fn user() -> Value {
        json!({"id": "u1", "username": "maker", "name": " Maker ", "status": "active",
            "role": "user", "premium": false, "createdAt": "2020-01-01T00:00:00.000Z"})
    }

    #[tokio::test]
    async fn extracts_video_with_direct_url() {
        let video = json!({"id": "v1", "slug": "s", "title": " Clip ", "body": null,
            "numLikes": 3, "tags": [{"id": "t1"}], "user": user(),
            "fileUrl": "https://files.iwara.tv/file/f1?expires=99&hash=h",
            "file": {"id": "f1", "name": "clip.mp4", "createdAt": "2020-01-02T03:04:05.000Z",
                     "mime": "video/mp4", "size": 10, "type": "video"}});
        let sources = json!([
            {"name": "360", "src": {"download": "//files.iwara.tv/download?f=360"}},
            {"name": "Source", "src": {"download": "//files.iwara.tv/download?f=src"}}]);
        let (root, server) = serve(vec![json(video), json(sources)]);
        let http = http("Iwara", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.iwara.tv/video/v1/slug").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].url.as_str(),
            "https://files.iwara.tv/download?f=src"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["id"], "v1");
        assert_eq!(k["title"], "Clip");
        assert_eq!(k["description"], "");
        assert_eq!(k["tags"], json!(["t1"]));
        assert_eq!(k["file_id"], "f1");
        assert_eq!(k["filename"], "clip");
        assert_eq!(k["extension"], "mp4");
        assert_eq!(k["format"], "Source");
        assert_eq!(k["type"], "video");
        assert_eq!(k["user"]["nick"], "Maker");
        assert_eq!(k["user"]["date"], "2020-01-01 00:00:00");
        let colon = if cfg!(windows) { "_" } else { ":" };
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            file.render(&files[0]).unwrap(),
            format!("2020-01-02 03{colon}04{colon}05 v1 Clip clip.mp4")
        );
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("iwara/maker")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(
            identity,
            r#"["{type} {user[name]} {id} {file_id}",["video","maker","v1","f1"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /video/v1 "));
        assert!(requests[0].contains("x-site: www.iwara.tv"));
        assert!(requests[1].starts_with("GET /file/f1?expires=99&hash=h "));
        let hash = x_version("https://files.iwara.tv/file/f1?expires=99&hash=h").unwrap();
        assert!(requests[1].contains(&format!("x-version: {hash}")));
    }

    #[tokio::test]
    async fn user_images_use_the_profile_and_fetch_missing_files() {
        let profile = json!({"user": user(), "body": "About"});
        let listing = json!({"results": [{"id": "i1", "title": "Pic", "user": user()}]});
        let image = json!({"id": "i1", "title": "Pic", "user": user(), "files": [
            {"id": "f1", "name": "a.png", "createdAt": "2020-01-02T03:04:05.000Z", "type": "image"},
            {"id": "f2", "name": "b.jpg", "type": "image"}]});
        let (root, server) = serve(vec![json(profile), json(listing), json(image)]);
        let http = http("Iwara", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.iwara.tv/profile/maker/images?sort=date").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[1].url.as_str(),
            "https://i.iwara.tv/image/original/f2/f2.jpg"
        );
        let k = keywords(&files[1]);
        assert_eq!(k["id"], "i1");
        assert_eq!(k["file_id"], "f2");
        assert_eq!(k["type"], "image");
        assert_eq!(k["count"], 2);
        assert_eq!(k["num"], 2);
        assert_eq!(k["user"]["description"], "About");
        assert_eq!(k["subcategory"], "user-images");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /profile/maker "));
        assert!(
            requests[1].starts_with("GET /images?sort=date&user=u1&rating=all&page=0&limit=50 "),
            "{}",
            requests[1]
        );
        assert!(requests[2].starts_with("GET /image/i1 "));
    }

    #[tokio::test]
    async fn queues_profiles_and_followers() {
        let http = http("Iwara", Url::parse("http://127.0.0.1:9/").unwrap());
        let children = queued(extract(
            &http,
            Url::parse("https://www.iwara.tv/profile/maker").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 2);

        let profile = json!({"user": user(), "body": null});
        let followers = json!({"results": [{"follower": {"username": "fan"}}, {"follower": {"username": null}}]});
        let (root, server) = serve(vec![json(profile), json(followers)]);
        let http = crate::sites::testing::http("Iwara", root);
        let children = queued(extract(
            &http,
            Url::parse("https://www.iwara.tv/profile/maker/followers").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].subcategory(), "followers");
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /user/u1/followers?page=0&limit=50 "));
    }
}
