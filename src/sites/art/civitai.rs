//! Civitai models, images, posts, collections and searches, mirroring
//! gallery_dl/extractor/civitai.py through the public tRPC API (the
//! upstream default) and the public Meilisearch endpoint.
//!
//! Supported options: `quality`, `quality-videos`, `nsfw`, `period`, `sort`,
//! `include` (user dispatch), `api-key` (tRPC) and `token` (search). Not
//! supported: `api` (REST), `metadata` and `files`. Reactions, the generator feed
//! and anything else needing an account are not matched.
use crate::sites::art;
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use futures_util::{StreamExt, stream};
use serde_json::{Map, Value, json};
use url::Url;

const ROOT: &str = "https://civitai.com";
const IMAGE_ROOT: &str = "https://image.civitai.com/xG1nkqKTMzGDvpLrqFT7WA";
const SEARCH_ROOT: &str = "https://search-new.civitai.com";
const SEARCH_TOKEN: &str =
    "Bearer 8c46eb2508e21db1e9828a97968d91ab1ca1caa5f70a00e88a2ba1e286603b61";

pub static SITE: Site = Site {
    category: "civitai",
    name: "Civitai",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[
        &MODEL,
        &IMAGE,
        &COLLECTION,
        &POST,
        &TAG,
        &SEARCH_MODELS,
        &SEARCH_IMAGES,
        &MODELS,
        &IMAGES,
        &VIDEOS,
        &POSTS,
        &USER,
        &USER_MODELS,
        &USER_POSTS,
        &USER_IMAGES,
        &USER_VIDEOS,
        &USER_COLLECTIONS,
    ],
    api_key: true,
};

const FILENAME: &str = "{file[id]}.{extension}";
const ARCHIVE: &str = "{file[uuid]}";
const DIRECTORY: &[&str] = &["{category}", "{user[username]}", "images"];
const POSTS_DIRECTORY: &[&str] = &[
    "{category}",
    "{username|user[username]}",
    "posts",
    "{post[id]}{post[title]:? //}",
];

static MODEL: Route = Route::new(
    "model",
    FILENAME,
    &[
        "{category}",
        "{user[username]}",
        "{model[id]}{model[name]:? //}",
        "{version[id]}{version[name]:? //}",
    ],
    ARCHIVE,
);
static IMAGE: Route = Route::new("image", FILENAME, DIRECTORY, ARCHIVE);
static COLLECTION: Route = Route::new(
    "collection",
    FILENAME,
    &[
        "{category}",
        "{user_collection[username]}",
        "collections",
        "{collection[id]}{collection[name]:? //}",
    ],
    ARCHIVE,
);
static POST: Route = Route::new("post", FILENAME, POSTS_DIRECTORY, ARCHIVE);
static TAG: Route = Route::new("tag", FILENAME, DIRECTORY, ARCHIVE);
static SEARCH_MODELS: Route = Route::new("search-models", FILENAME, DIRECTORY, ARCHIVE);
static SEARCH_IMAGES: Route = Route::new("search-images", FILENAME, DIRECTORY, ARCHIVE);
static MODELS: Route = Route::new("models", FILENAME, DIRECTORY, ARCHIVE);
static IMAGES: Route = Route::new("images", FILENAME, DIRECTORY, ARCHIVE);
static VIDEOS: Route = Route::new("videos", FILENAME, DIRECTORY, ARCHIVE);
static POSTS: Route = Route::new("posts", FILENAME, DIRECTORY, ARCHIVE);
static USER: Route = Route::new("user", FILENAME, DIRECTORY, ARCHIVE);
static USER_MODELS: Route = Route::new("user-models", FILENAME, DIRECTORY, ARCHIVE);
static USER_POSTS: Route = Route::new("user-posts", FILENAME, POSTS_DIRECTORY, ARCHIVE);
static USER_IMAGES: Route = Route::new("user-images", FILENAME, DIRECTORY, ARCHIVE);
static USER_VIDEOS: Route = Route::new(
    "user-videos",
    FILENAME,
    &["{category}", "{username|user[username]}", "videos"],
    ARCHIVE,
);
static USER_COLLECTIONS: Route = Route::new("user-collections", FILENAME, DIRECTORY, ARCHIVE);

#[derive(Debug, PartialEq)]
enum Target {
    Model {
        id: i64,
        version: Option<i64>,
    },
    Image(i64),
    Collection(i64),
    Post(i64),
    Tag(String),
    SearchModels(String),
    SearchImages(String),
    Models(Option<String>),
    Images(Option<String>),
    Videos(Option<String>),
    Posts(Option<String>),
    /// Raw (percent-encoded) user name.
    User(String),
    UserModels(String, Option<String>),
    UserPosts(String, Option<String>),
    UserImages(String, Option<String>),
    UserVideos(String, Option<String>),
    UserCollections(String, Option<String>),
}

fn id(text: &str) -> Option<i64> {
    if art::digits(text) {
        text.parse().ok()
    } else {
        None
    }
}

/// Leading ASCII digits of `text`, as upstream `(\d+)` without an anchor.
fn leading_id(text: &str) -> Option<i64> {
    let len = text.bytes().take_while(u8::is_ascii_digit).count();
    id(&text[..len])
}

fn target(url: &Url) -> Option<Target> {
    if !art::on_host(url, &["civitai.com"]) {
        return None;
    }
    let path = url.path().strip_prefix('/')?;
    let query = url.query().filter(|q| !q.is_empty()).map(str::to_owned);
    let parts: Vec<&str> = path.split('/').collect();
    // `/NAME` or `/NAME/` followed by a query: upstream `/NAME(?:/?\?(...))?(?:$|#)`.
    let listing = |name: &str| match parts.as_slice() {
        [first] if *first == name => Some(query.clone()),
        [first, ""] if *first == name && query.is_some() => Some(query.clone()),
        _ => None,
    };
    match parts.as_slice() {
        ["models", model, rest @ ..] if id(model).is_some() => {
            let version = if rest.is_empty() || rest == [""] {
                url.query()
                    .and_then(|q| q.strip_prefix("modelVersionId="))
                    .and_then(leading_id)
            } else {
                None
            };
            return Some(Target::Model {
                id: id(model)?,
                version,
            });
        }
        ["images", image, ..] if id(image).is_some() => return Some(Target::Image(id(image)?)),
        ["collections", c, ..] if id(c).is_some() => return Some(Target::Collection(id(c)?)),
        ["posts", post, ..] if id(post).is_some() => return Some(Target::Post(id(post)?)),
        ["tag", tag, ..] => {
            let tag = tag.split('&').next().unwrap_or_default();
            return (!tag.is_empty()).then(|| Target::Tag(unquote(tag)));
        }
        ["search", "models"] => return query.map(Target::SearchModels),
        ["search", "images"] => return query.map(Target::SearchImages),
        _ => {}
    }
    if let Some(q) = listing("models") {
        return Some(Target::Models(q));
    }
    if let Some(q) = listing("images") {
        return Some(Target::Images(q));
    }
    if let Some(q) = listing("videos") {
        return Some(Target::Videos(q));
    }
    if let Some(q) = listing("posts") {
        return Some(Target::Posts(q));
    }
    let (user, kind) = match parts.as_slice() {
        ["user", user] | ["user", user, ""] => (*user, None),
        ["user", user, kind] | ["user", user, kind, ""] => (*user, Some(*kind)),
        _ => return None,
    };
    if !art::segment(user) {
        return None;
    }
    let user = user.to_owned();
    let reactions = || parse_query(query.as_deref()).get("section") == Some(&json!("reactions"));
    Some(match kind {
        None => Target::User(user),
        Some("models") => Target::UserModels(user, query),
        Some("posts") => Target::UserPosts(user, query),
        // Reaction feeds need an authenticated account.
        Some("images") if !reactions() => Target::UserImages(user, query),
        Some("videos") if !reactions() => Target::UserVideos(user, query),
        Some("collections") => Target::UserCollections(user, query),
        _ => return None,
    })
}

/// gallery-dl `text.unquote` after `+` to space, as in `parse_query_list`.
fn unquote(text: &str) -> String {
    crate::sites::percent_decode(text)
}

/// gallery-dl `text.parse_query_list` with the upstream list keys.
fn parse_query(qs: Option<&str>) -> Map<String, Value> {
    const LISTS: &[&str] = &[
        "tags",
        "reactions",
        "baseModels",
        "tools",
        "techniques",
        "types",
        "fileFormats",
    ];
    let mut result = Map::new();
    for pair in qs.unwrap_or_default().split('&') {
        let Some((name, value)) = pair.split_once('=') else {
            continue;
        };
        let name = unquote(&name.replace('+', " "));
        let value = unquote(&value.replace('+', " "));
        if LISTS.contains(&name.as_str()) {
            match result.get_mut(&name) {
                Some(Value::Array(list)) => list.push(value.into()),
                _ => {
                    result.insert(name, json!([value]));
                }
            }
        } else if !result.contains_key(&name) {
            result.insert(name, value.into());
        }
    }
    result
}

/// Upstream `CivitaiTrpcAPI._type_params`.
fn type_params(params: &mut Map<String, Value>) -> Result<()> {
    const INTS: &[&str] = &["tags", "tools", "techniques", "modelId", "modelVersionId"];
    const BOOLS: &[&str] = &[
        "remixesOnly",
        "nonRemixesOnly",
        "withMeta",
        "fromPlatform",
        "supportsGeneration",
    ];
    let convert = |name: &str, value: &Value| -> Result<Value> {
        let Some(text) = value.as_str() else {
            return Ok(value.clone());
        };
        if INTS.contains(&name) {
            text.trim()
                .parse::<i64>()
                .map(Value::from)
                .map_err(|_| error("civitai query parameter is not an integer"))
        } else {
            Ok((text == "true").into())
        }
    };
    for (name, value) in params.iter_mut() {
        if !INTS.contains(&name.as_str()) && !BOOLS.contains(&name.as_str()) {
            continue;
        }
        *value = match value {
            Value::String(_) => convert(name, value)?,
            Value::Array(items) => Value::Array(
                items
                    .iter()
                    .map(|item| convert(name, item))
                    .collect::<Result<_>>()?,
            ),
            _ => continue,
        };
    }
    Ok(())
}

/// Upstream defaults merged with user parameters (`_merge_params`).
fn merge(mut defaults: Map<String, Value>, user: Map<String, Value>) -> Map<String, Value> {
    art::update(&mut defaults, &user);
    defaults
}

fn object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

/// Resolved upstream `_init` and API settings.
#[derive(Clone, Debug)]
struct Cfg {
    image_quality: String,
    image_ext: &'static str,
    video_quality: String,
    nsfw: Value,
    period: Value,
    sort: Value,
}

fn joined(value: &Value) -> String {
    match value {
        Value::Array(items) => items
            .iter()
            .map(crate::sites::scalar)
            .collect::<Vec<_>>()
            .join(","),
        other => crate::sites::scalar(other),
    }
}

impl Cfg {
    fn new(http: &Http) -> Self {
        let option = |key| http.option(key).filter(|v| !v.is_null());
        let quality = option("quality").filter(|v| art::truthy(Some(v)));
        let (image_quality, image_ext) = match quality {
            Some(value) => {
                let q = joined(value);
                let ext = if q == "original=true" { "png" } else { "jpg" };
                (q, ext)
            }
            None => ("original=true".to_owned(), "png"),
        };
        let video_quality = match option("quality-videos") {
            Some(value) if art::truthy(Some(value)) => {
                let q = joined(value);
                match q.strip_prefix('+') {
                    Some(rest) => {
                        format!("{image_quality},{}", rest.trim_start_matches(['+', ',']))
                    }
                    None => q,
                }
            }
            Some(_) if quality.is_some() => image_quality.clone(),
            _ => "original=true,quality=100".to_owned(),
        };
        let nsfw = match option("nsfw") {
            None | Some(Value::Bool(true)) => json!(31),
            Some(value) if !art::truthy(Some(value)) => json!(1),
            Some(value) => value.clone(),
        };
        let period = option("period")
            .filter(|v| art::truthy(Some(v)))
            .cloned()
            .unwrap_or_else(|| json!("AllTime"));
        let sort = match option("sort")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            Some(sort) => {
                let first = sort.chars().next().unwrap_or_default().to_ascii_lowercase();
                if "drn".contains(first) {
                    json!("Newest")
                } else if "ao".contains(first) {
                    json!("Oldest")
                } else {
                    json!(sort)
                }
            }
            None => json!("Newest"),
        };
        Self {
            image_quality,
            image_ext,
            video_quality,
            nsfw,
            period,
            sort,
        }
    }

    fn ext(&self, file: &Map<String, Value>) -> &'static str {
        if is_video(file) {
            "webm"
        } else {
            self.image_ext
        }
    }

    /// Upstream `_url`: the download URL; sets `file["uuid"]`.
    fn url(&self, image: &mut Map<String, Value>) -> Option<String> {
        let url = image.get("url")?.as_str()?.to_owned();
        let quality = if is_video(image) {
            &self.video_quality
        } else {
            &self.image_quality
        };
        if url.contains('/') {
            let mut parts: Vec<&str> = url.rsplitn(4, '/').collect();
            parts.reverse();
            if parts.len() < 3 {
                return None;
            }
            image.insert("uuid".into(), parts[1].into());
            parts[2] = quality;
            return Some(parts.join("/"));
        }
        image.insert("uuid".into(), url.clone().into());
        let id = image
            .get("id")
            .map(crate::sites::scalar)
            .unwrap_or_else(|| "None".into());
        let name = match image.get("name").and_then(Value::as_str) {
            Some(name) if !name.is_empty() => name.to_owned(),
            _ => match image.get("mimeType").and_then(Value::as_str) {
                Some(mime) if !mime.is_empty() => {
                    format!("{id}.{}", mime.rsplit('/').next().unwrap_or_default())
                }
                _ => format!("{id}.{}", self.ext(image)),
            },
        };
        Some(format!("{IMAGE_ROOT}/{url}/{quality}/{name}"))
    }

    fn images(&self, user: Map<String, Value>) -> Result<Map<String, Value>> {
        let mut params = merge(
            object(json!({
                "useIndex": true,
                "period": self.period,
                "sort": self.sort,
                "withMeta": false,
                "fromPlatform": false,
                "browsingLevel": self.nsfw,
                "include": ["cosmetics"],
            })),
            user,
        );
        type_params(&mut params)?;
        Ok(params)
    }

    fn models(&self, user: Map<String, Value>) -> Map<String, Value> {
        merge(
            object(json!({
                "period": self.period,
                "periodMode": "published",
                "sort": self.sort,
                "pending": false,
                "hidden": false,
                "followed": false,
                "earlyAccess": false,
                "fromPlatform": false,
                "supportsGeneration": false,
                "browsingLevel": self.nsfw,
            })),
            user,
        )
    }

    fn posts(&self, user: Map<String, Value>) -> Result<Map<String, Value>> {
        let mut params = merge(
            object(json!({
                "browsingLevel": self.nsfw,
                "period": self.period,
                "periodMode": "published",
                "sort": self.sort,
                "followed": false,
                "draftOnly": false,
                "pending": true,
                "include": ["cosmetics"],
            })),
            user,
        );
        type_params(&mut params)?;
        Ok(params)
    }

    fn collections(&self, user: Map<String, Value>) -> Result<Map<String, Value>> {
        let mut params = merge(
            object(json!({"browsingLevel": self.nsfw, "sort": self.sort})),
            user,
        );
        type_params(&mut params)?;
        Ok(params)
    }
}

fn is_video(file: &Map<String, Value>) -> bool {
    file.get("type").and_then(Value::as_str) == Some("video")
}

fn iso(value: Option<&Value>) -> Option<i64> {
    value.and_then(Value::as_str).and_then(art::parse_iso)
}

fn date_value(ts: Option<i64>) -> Value {
    ts.map_or(Value::Null, |ts| art::date(Some(ts)))
}

fn username(user: &Value) -> Option<String> {
    user.get("username")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn scalar_of(map: &Map<String, Value>, key: &str) -> String {
    map.get(key)
        .filter(|v| !v.is_null())
        .map(crate::sites::scalar)
        .unwrap_or_default()
}

/// One file of the image-based extractors (`images()` in upstream `items`).
fn image_item(cfg: &Cfg, file: Value, extra: &Map<String, Value>) -> Option<Item> {
    let mut file = object(file);
    let user = file.shift_remove("user").unwrap_or(Value::Null);
    let ts = iso(file.get("createdAt"));
    file.insert("date".into(), date_value(ts));
    let raw = cfg.url(&mut file)?;
    let url = crate::sites::media_url(&raw, &["civitai.com"])?;
    let ext = cfg.ext(&file);
    let post_id = scalar_of(&file, "postId");
    let media_id = scalar_of(&file, "id");
    let caption = file
        .get("postTitle")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let name = username(&user);
    let mut data = Map::new();
    data.insert("file".into(), file.into());
    data.insert("user".into(), user);
    data.insert("url".into(), raw.clone().into());
    art::nameext_into(&raw, &mut data);
    if data["extension"] == "" {
        data.insert("extension".into(), ext.into());
    }
    art::update(&mut data, extra);
    Some(
        Item::new(url, data)
            .ids(post_id, media_id)
            .user(name)
            .caption(caption)
            .timestamp(ts),
    )
}

/// One file of upstream `_image_results`, merged with post/model `data`.
fn result_item(cfg: &Cfg, num: usize, file: Value, data: &Map<String, Value>) -> Option<Item> {
    let mut file = object(file);
    let raw_file = file.get("url")?.as_str()?.to_owned();
    let raw = cfg.url(&mut file)?;
    let url = crate::sites::media_url(&raw, &["civitai.com"])?;
    let mut names = Map::new();
    art::nameext_into(&raw_file, &mut names);
    let filename = names["filename"].as_str().unwrap_or_default().to_owned();
    if names["extension"] == "" {
        names.insert("extension".into(), cfg.ext(&file).into());
    }
    if !file.contains_key("id")
        && !filename.is_empty()
        && filename.bytes().all(|b| b.is_ascii_digit())
    {
        file.insert("id".into(), art::parse_int(&filename, 0).into());
    }
    let ts = iso(file.get("createdAt"));
    if !file.contains_key("date") {
        file.insert("date".into(), date_value(ts));
    }
    let post_id = data
        .get("post")
        .and_then(|p| p.get("id"))
        .filter(|v| !v.is_null())
        .map(crate::sites::scalar)
        .unwrap_or_else(|| scalar_of(&file, "postId"));
    let media_id = scalar_of(&file, "id");
    let user = data.get("user").and_then(username);
    let caption = data
        .get("post")
        .and_then(|p| p.get("title"))
        .or_else(|| data.get("model").and_then(|m| m.get("name")))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let mut fields = Map::new();
    fields.insert("num".into(), num.into());
    fields.insert("file".into(), file.into());
    fields.insert("url".into(), raw.into());
    art::update(&mut fields, &names);
    art::update(&mut fields, data);
    Some(
        Item::new(url, fields)
            .ids(post_id, media_id)
            .user(user)
            .caption(caption)
            .timestamp(ts),
    )
}

fn now_ms() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default()
        .to_string()
}

/// Upstream `CivitaiTrpcAPI._call`: `result.data.json` of one tRPC query.
async fn trpc(
    http: &Http,
    endpoint: &str,
    params: &Map<String, Value>,
    meta: Option<&Value>,
) -> Result<Value> {
    let mut input = json!({"json": params});
    if let Some(meta) = meta {
        input["meta"] = json!({"values": meta});
    }
    let mut url = http.api(&format!("{ROOT}/api/trpc/{endpoint}"))?;
    url.query_pairs_mut()
        .append_pair("input", &input.to_string());
    let date = now_ms();
    let mut headers = vec![
        ("content-type", "application/json"),
        ("x-client-version", "5.0.1386"),
        ("x-client-date", date.as_str()),
        ("x-client", "web"),
        ("x-fingerprint", "undefined"),
    ];
    let authorization = http.api_key().map(|key| format!("Bearer {key}"));
    if let Some(authorization) = &authorization {
        headers.push(("Authorization", authorization));
    }
    let mut body = http.json_with(&url, &headers).await?;
    body.pointer_mut("/result/data/json")
        .map(Value::take)
        .ok_or_else(|| error("civitai API response contains no data"))
}

/// Upstream `CivitaiTrpcAPI._pagination`: `handle` turns each page of items
/// into events; with `user`, a first page by another user yields nothing.
fn trpc_paginate<'a, F>(
    http: &'a Http,
    endpoint: &'static str,
    mut params: Map<String, Value>,
    user: Option<String>,
    handle: F,
) -> Events<'a>
where
    F: FnMut(Vec<Value>) -> Result<Events<'a>> + Send + 'a,
{
    let meta = (!params.contains_key("cursor")).then(|| {
        params.insert("cursor".into(), Value::Null);
        json!({"cursor": ["undefined"]})
    });
    crate::sites::paginate(Some((params, meta, handle, true)), move |state| {
        let user = user.clone();
        async move {
            let Some((mut params, meta, mut handle, first)) = state else {
                return Ok(None);
            };
            let mut data = trpc(http, endpoint, &params, meta.as_ref()).await?;
            let items = match data.get_mut("items").map(Value::take) {
                Some(Value::Array(items)) => items,
                _ => return Err(error("civitai API response contains no items")),
            };
            if first
                && let (Some(name), Some(item)) = (&user, items.first())
                && item.pointer("/user/username").and_then(Value::as_str) != Some(name.as_str())
            {
                return Ok(None);
            }
            let events = handle(items)?;
            let next = data
                .get("nextCursor")
                .filter(|c| art::truthy(Some(c)))
                .cloned()
                .map(|cursor| {
                    params.insert("cursor".into(), cursor);
                    (params, None, handle, false)
                });
            Ok(Some((events, next)))
        }
    })
}

fn chain<'a>(parts: Vec<Events<'a>>) -> Events<'a> {
    stream::iter(parts).flatten().boxed()
}

fn file_events<'a>(http: &'a Http, route: &'static Route, items: Vec<Item>) -> Events<'a> {
    crate::sites::events(crate::sites::post(&SITE, http, route, items))
}

/// Image-based extractors: one directory per file.
fn images_flow<'a>(
    http: &'a Http,
    route: &'static Route,
    cfg: Cfg,
    params: Map<String, Value>,
    extra: Map<String, Value>,
) -> Events<'a> {
    trpc_paginate(http, "image.getInfinite", params, None, move |items| {
        Ok(chain(
            items
                .into_iter()
                .filter_map(|file| image_item(&cfg, file, &extra))
                .map(|item| file_events(http, route, vec![item]))
                .collect(),
        ))
    })
}

/// A paginated `_image_results` handler numbering files across pages.
fn results_handler<'a>(
    http: &'a Http,
    route: &'static Route,
    cfg: Cfg,
    data: Map<String, Value>,
) -> impl FnMut(Vec<Value>) -> Result<Events<'a>> + Send + 'a {
    let mut num = 0;
    move |images| {
        let items = images
            .into_iter()
            .filter_map(|file| {
                num += 1;
                result_item(&cfg, num, file, &data)
            })
            .collect();
        Ok(file_events(http, route, items))
    }
}

fn post_events<'a>(http: &'a Http, route: &'static Route, cfg: Cfg, stub: Value) -> Events<'a> {
    crate::sites::once(async move {
        let id = stub
            .get("id")
            .and_then(Value::as_i64)
            .ok_or_else(|| error("civitai post has no ID"))?;
        let mut post = object(trpc(http, "post.get", &object(json!({"id": id})), None).await?);
        let ts = iso(post.get("publishedAt"));
        post.insert("date".into(), date_value(ts));
        let user = post.shift_remove("user").unwrap_or(Value::Null);
        let mut data = Map::new();
        data.insert("post".into(), post.into());
        data.insert("user".into(), user);
        let mut handle = results_handler(http, route, cfg.clone(), data);
        match stub.get("images") {
            Some(images) => handle(images.as_array().cloned().unwrap_or_default()),
            None => {
                let params = cfg.images(object(json!({"postId": id, "pending": true})))?;
                Ok(trpc_paginate(
                    http,
                    "image.getInfinite",
                    params,
                    None,
                    handle,
                ))
            }
        }
    })
}

fn posts_flow<'a>(
    http: &'a Http,
    route: &'static Route,
    cfg: Cfg,
    params: Map<String, Value>,
) -> Result<Events<'a>> {
    let user = params
        .get("username")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let params = cfg.posts(params)?;
    Ok(trpc_paginate(
        http,
        "post.getInfinite",
        params,
        user,
        move |posts| {
            Ok(chain(
                posts
                    .into_iter()
                    .map(|post| post_events(http, route, cfg.clone(), post))
                    .collect(),
            ))
        },
    ))
}

fn version_events<'a>(
    http: &'a Http,
    cfg: Cfg,
    model: &Map<String, Value>,
    user: &Value,
    version: Value,
) -> Result<Events<'a>> {
    let mut version = object(version);
    let ts = iso(version.get("createdAt"));
    version.insert("date".into(), date_value(ts));
    let version_id = version.get("id").cloned().unwrap_or(Value::Null);
    let images = version.get("images").cloned();
    let mut data = Map::new();
    data.insert("model".into(), model.clone().into());
    data.insert("version".into(), version.into());
    data.insert("user".into(), user.clone());
    let mut handle = results_handler(http, &MODEL, cfg.clone(), data);
    if let Some(images) = images {
        return handle(images.as_array().cloned().unwrap_or_default());
    }
    let mut params = object(json!({
        "modelVersionId": version_id,
        "prioritizedUserIds": [user.get("id").cloned().unwrap_or(Value::Null)],
        "period": cfg.period,
        "sort": cfg.sort,
        "limit": 20,
        "pending": true,
    }));
    type_params(&mut params)?;
    Ok(trpc_paginate(
        http,
        "image.getInfinite",
        params,
        None,
        handle,
    ))
}

fn model_flow(http: &Http, cfg: Cfg, id: i64, version_id: Option<i64>) -> Events<'_> {
    crate::sites::once(async move {
        let mut model =
            object(trpc(http, "model.getById", &object(json!({"id": id})), None).await?);
        let user = match model.shift_remove("user") {
            Some(user) => user,
            None => model
                .shift_remove("creator")
                .ok_or_else(|| error("civitai model has no creator"))?,
        };
        let mut versions = match model.shift_remove("modelVersions") {
            Some(Value::Array(versions)) => versions,
            _ => return Err(error("civitai model has no versions")),
        };
        if let Some(version_id) = version_id {
            let found = versions
                .into_iter()
                .find(|v| v.get("id").and_then(Value::as_i64) == Some(version_id));
            let version = match found {
                Some(version) => version,
                None => {
                    trpc(
                        http,
                        "modelVersion.getById",
                        &object(json!({"id": version_id})),
                        None,
                    )
                    .await?
                }
            };
            versions = vec![version];
        }
        let parts = versions
            .into_iter()
            .map(|version| version_events(http, cfg.clone(), &model, &user, version))
            .collect::<Result<Vec<_>>>()?;
        Ok(chain(parts))
    })
}

fn queue_models<'a>(
    http: &'a Http,
    route: &'static Route,
    items: impl IntoIterator<Item = Value>,
) -> Result<Events<'a>> {
    let mut children = Vec::new();
    for model in items {
        let Some(id) = model.get("id").filter(|v| !v.is_null()) else {
            continue;
        };
        let url = art::url(&format!("{ROOT}/models/{}", crate::sites::scalar(id)))?;
        children.push(crate::sites::queue(
            http,
            SITE.category,
            route.subcategory,
            url,
            &Map::new(),
        ));
    }
    Ok(chain(children))
}

fn models_flow<'a>(
    http: &'a Http,
    route: &'static Route,
    cfg: &Cfg,
    params: Map<String, Value>,
) -> Events<'a> {
    trpc_paginate(
        http,
        "model.getAll",
        cfg.models(params),
        None,
        move |items| queue_models(http, route, items),
    )
}

fn collection_flow(http: &Http, cfg: Cfg, cid: i64) -> Events<'_> {
    crate::sites::once(async move {
        let mut body = trpc(
            http,
            "collection.getById",
            &object(json!({"id": cid})),
            None,
        )
        .await?;
        let mut collection = object(
            body.get_mut("collection")
                .map(Value::take)
                .unwrap_or_default(),
        );
        let user = collection.shift_remove("user").unwrap_or(Value::Null);
        let mut extra = Map::new();
        extra.insert("collection".into(), collection.into());
        extra.insert("user_collection".into(), user);
        let mut params = object(json!({
            "collectionId": cid,
            "period": cfg.period,
            "sort": cfg.sort,
            "browsingLevel": cfg.nsfw,
            "include": ["cosmetics"],
        }));
        type_params(&mut params)?;
        Ok(images_flow(http, &COLLECTION, cfg, params, extra))
    })
}

fn user_collections_flow(http: &Http, cfg: Cfg, user: String, query: Option<String>) -> Events<'_> {
    crate::sites::once(async move {
        let creator = trpc(
            http,
            "user.getCreator",
            &object(json!({"username": unquote(&user)})),
            None,
        )
        .await?;
        let user_id = creator
            .get("id")
            .filter(|v| !v.is_null())
            .cloned()
            .ok_or_else(|| error("civitai user not found"))?;
        let mut params = parse_query(query.as_deref());
        params.insert("userId".into(), user_id);
        let params = cfg.collections(params)?;
        Ok(trpc_paginate(
            http,
            "collection.getInfinite",
            params,
            None,
            move |items| {
                let mut children = Vec::new();
                for collection in items {
                    let Some(id) = collection.get("id").filter(|v| !v.is_null()) else {
                        continue;
                    };
                    let url =
                        art::url(&format!("{ROOT}/collections/{}", crate::sites::scalar(id)))?;
                    children.push(crate::sites::queue(
                        http,
                        SITE.category,
                        USER_COLLECTIONS.subcategory,
                        url,
                        &object(collection),
                    ));
                }
                Ok(chain(children))
            },
        ))
    })
}

/// Upstream `Dispatch._dispatch_extractors` with `include`.
fn user_flow(http: &Http, user: String) -> Result<Events<'_>> {
    const ALL: &[&str] = &[
        "user-models",
        "user-posts",
        "user-images",
        "user-videos",
        "user-collections",
    ];
    let include: Vec<String> = match http.option("include") {
        None | Some(Value::Null) => vec!["user-images".into(), "user-videos".into()],
        Some(Value::String(s)) if s == "all" => ALL.iter().map(|s| (*s).to_owned()).collect(),
        Some(Value::String(s)) => s
            .replace(' ', "")
            .split(',')
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        Some(_) => Vec::new(),
    };
    let mut children = Vec::new();
    for sub in include {
        let Some(kind) = ALL
            .iter()
            .find(|s| **s == sub)
            .and_then(|s| s.strip_prefix("user-"))
        else {
            continue;
        };
        let url = art::url(&format!("{ROOT}/user/{user}/{kind}"))?;
        children.push(crate::sites::queue(
            http,
            SITE.category,
            USER.subcategory,
            url,
            &Map::new(),
        ));
    }
    Ok(chain(children))
}

#[derive(Clone, Copy, PartialEq)]
enum Search {
    Models,
    Images,
}

/// Upstream `CivitaiSearchAPI._generate_filter`.
fn search_filter(level: i64) -> String {
    let levels: Vec<String> = [1, 2, 4, 8, 16]
        .into_iter()
        .filter(|bit| level & bit != 0)
        .map(|bit| format!("nsfwLevel={bit}"))
        .collect();
    if levels.is_empty() {
        "()".into()
    } else {
        format!("({})", levels.join(" OR "))
    }
}

fn search_flow(http: &Http, cfg: Cfg, kind: Search, query: String) -> Events<'_> {
    const LIMIT: i64 = 51;
    let params = parse_query(Some(&query));
    let (index, facets, route) = match kind {
        Search::Models => (
            "models_v9",
            json!([
                "category.name",
                "checkpointType",
                "fileFormats",
                "lastVersionAtUnix",
                "tags.name",
                "type",
                "user.username",
                "version.baseModel"
            ]),
            &SEARCH_MODELS,
        ),
        Search::Images => (
            "images_v6",
            json!([
                "aspectRatio",
                "baseModel",
                "createdAtUnix",
                "tagNames",
                "techniqueNames",
                "toolNames",
                "type",
                "user.username"
            ]),
            &SEARCH_IMAGES,
        ),
    };
    let index = params
        .get("sortBy")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(index)
        .to_owned();
    let auth = match http.option_str("token").filter(|s| !s.is_empty()) {
        Some(token) if token.contains(' ') => token.to_owned(),
        Some(token) => format!("Bearer {token}"),
        None => SEARCH_TOKEN.to_owned(),
    };
    let body = json!({
        "q": params.get("query").cloned().unwrap_or(Value::Null),
        "indexUid": index,
        "facets": facets,
        "attributesToHighlight": [],
        "highlightPreTag": "__ais-highlight__",
        "highlightPostTag": "__/ais-highlight__",
        "limit": LIMIT,
        "offset": 0,
        "filter": [search_filter(cfg.nsfw.as_i64().unwrap_or(31))],
    });
    crate::sites::paginate(Some(0i64), move |offset| {
        let mut query = body.clone();
        let auth = auth.clone();
        let cfg = cfg.clone();
        async move {
            let Some(offset) = offset else {
                return Ok(None);
            };
            query["offset"] = offset.into();
            let url = http.api(&format!("{SEARCH_ROOT}/multi-search"))?;
            let headers = [
                ("Authorization", auth.as_str()),
                (
                    "X-Meilisearch-Client",
                    "Meilisearch instant-meilisearch (v0.13.5) ; Meilisearch JavaScript (v0.34.0)",
                ),
                ("Origin", ROOT),
                ("Sec-Fetch-Dest", "empty"),
                ("Sec-Fetch-Mode", "cors"),
                ("Sec-Fetch-Site", "same-site"),
                ("Priority", "u=4"),
            ];
            let mut data = http
                .post_json(&url, &json!({"queries": [query]}), &headers)
                .await?;
            let hits = match data.pointer_mut("/results/0/hits").map(Value::take) {
                Some(Value::Array(hits)) => hits,
                _ => return Err(error("civitai search response contains no hits")),
            };
            let count = hits.len() as i64;
            let events = match kind {
                Search::Models => queue_models(http, route, hits)?,
                Search::Images => chain(
                    hits.into_iter()
                        .filter_map(|hit| image_item(&cfg, hit, &Map::new()))
                        .map(|item| file_events(http, route, vec![item]))
                        .collect(),
                ),
            };
            let limit = LIMIT - 1;
            // Upstream advances by `limit - 1` and stops below half a page.
            let next = (count >= limit / 2 && offset < 100_000).then_some(offset + limit);
            Ok(Some((events, next)))
        }
    })
}

fn with(mut params: Map<String, Value>, key: &str, value: Value) -> Map<String, Value> {
    params.insert(key.into(), value);
    params
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Civitai URL"));
    };
    let cfg = Cfg::new(http);
    let result = (|| -> Result<Events<'_>> {
        Ok(match target {
            Target::Model { id, version } => model_flow(http, cfg, id, version),
            Target::Image(id) => crate::sites::once(async move {
                let file = trpc(http, "image.get", &object(json!({"id": id})), None).await?;
                Ok(match image_item(&cfg, file, &Map::new()) {
                    Some(item) => file_events(http, &IMAGE, vec![item]),
                    None => crate::sites::events(Vec::new()),
                })
            }),
            Target::Collection(id) => collection_flow(http, cfg, id),
            Target::Post(id) => post_events(http, &POST, cfg, json!({"id": id})),
            Target::Tag(tag) => {
                let params = object(json!({"tagname": tag}));
                models_flow(http, &TAG, &cfg, params)
            }
            Target::SearchModels(q) => search_flow(http, cfg, Search::Models, q),
            Target::SearchImages(q) => search_flow(http, cfg, Search::Images, q),
            Target::Models(q) => models_flow(http, &MODELS, &cfg, parse_query(q.as_deref())),
            Target::Images(q) => {
                let params = with(parse_query(q.as_deref()), "types", json!(["image"]));
                images_flow(http, &IMAGES, cfg.clone(), cfg.images(params)?, Map::new())
            }
            Target::Videos(q) => {
                let params = with(parse_query(q.as_deref()), "types", json!(["video"]));
                images_flow(http, &VIDEOS, cfg.clone(), cfg.images(params)?, Map::new())
            }
            Target::Posts(q) => posts_flow(http, &POSTS, cfg, parse_query(q.as_deref()))?,
            Target::User(user) => user_flow(http, user)?,
            Target::UserModels(user, q) => {
                let params = with(parse_query(q.as_deref()), "username", unquote(&user).into());
                models_flow(http, &USER_MODELS, &cfg, params)
            }
            Target::UserPosts(user, q) => {
                let params = with(parse_query(q.as_deref()), "username", unquote(&user).into());
                posts_flow(http, &USER_POSTS, cfg, params)?
            }
            Target::UserImages(user, q) => user_images(http, &USER_IMAGES, cfg, "image", &user, q)?,
            Target::UserVideos(user, q) => user_images(http, &USER_VIDEOS, cfg, "video", &user, q)?,
            Target::UserCollections(user, q) => user_collections_flow(http, cfg, user, q),
        })
    })();
    result.unwrap_or_else(crate::sites::fail)
}

fn user_images<'a>(
    http: &'a Http,
    route: &'static Route,
    cfg: Cfg,
    kind: &str,
    user: &str,
    query: Option<String>,
) -> Result<Events<'a>> {
    let params = with(parse_query(query.as_deref()), "types", json!([kind]));
    let params = with(params, "username", unquote(user).into());
    let params = cfg.images(params)?;
    Ok(images_flow(http, route, cfg, params, Map::new()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http, json as reply, keywords, media, serve};
    use std::path::PathBuf;
    use std::sync::Arc;

    fn t(raw: &str) -> Option<Target> {
        target(&Url::parse(raw).unwrap())
    }

    /// The decoded tRPC `input` parameter of a recorded request.
    fn input(request: &str) -> Value {
        let line = request.lines().next().unwrap();
        let path = line.split(' ').nth(1).unwrap();
        let url = Url::parse(&format!("http://x{path}")).unwrap();
        let raw = art::query(&url, "input").unwrap();
        serde_json::from_str(&raw).unwrap()
    }

    fn trpc_reply(value: Value) -> Vec<u8> {
        reply(json!({"result": {"data": {"json": value}}}))
    }

    #[test]
    fn classifies_urls() {
        assert_eq!(
            t("https://civitai.com/models/703211/maid-classic"),
            Some(Target::Model {
                id: 703211,
                version: None
            })
        );
        assert_eq!(
            t("https://civitai.com/models/703211?modelVersionId=786644"),
            Some(Target::Model {
                id: 703211,
                version: Some(786644)
            })
        );
        assert_eq!(
            t("https://www.civitai.com/images/26962948"),
            Some(Target::Image(26962948))
        );
        assert_eq!(
            t("https://civitai.com/collections/11035869"),
            Some(Target::Collection(11035869))
        );
        assert_eq!(
            t("https://civitai.com/posts/6877551"),
            Some(Target::Post(6877551))
        );
        assert_eq!(
            t("https://civitai.com/tag/mecha%20suit"),
            Some(Target::Tag("mecha suit".into()))
        );
        assert_eq!(
            t("https://civitai.com/search/models?sortBy=models_v9&query=Voynich"),
            Some(Target::SearchModels(
                "sortBy=models_v9&query=Voynich".into()
            ))
        );
        assert!(matches!(
            t("https://civitai.com/search/images?query=x"),
            Some(Target::SearchImages(_))
        ));
        assert_eq!(t("https://civitai.com/models"), Some(Target::Models(None)));
        assert_eq!(
            t("https://civitai.com/images/?tags=482"),
            Some(Target::Images(Some("tags=482".into())))
        );
        assert_eq!(t("https://civitai.com/videos"), Some(Target::Videos(None)));
        assert_eq!(t("https://civitai.com/posts"), Some(Target::Posts(None)));
        assert_eq!(
            t("https://civitai.com/user/waomodder"),
            Some(Target::User("waomodder".into()))
        );
        assert_eq!(
            t("https://civitai.com/user/waomodder/models?tag=x"),
            Some(Target::UserModels("waomodder".into(), Some("tag=x".into())))
        );
        assert!(matches!(
            t("https://civitai.com/user/w/posts/"),
            Some(Target::UserPosts(..))
        ));
        assert!(matches!(
            t("https://civitai.com/user/w/images"),
            Some(Target::UserImages(..))
        ));
        assert!(matches!(
            t("https://civitai.com/user/w/videos"),
            Some(Target::UserVideos(..))
        ));
        assert!(matches!(
            t("https://civitai.com/user/w/collections"),
            Some(Target::UserCollections(..))
        ));
        for raw in [
            "https://civitai.com/",
            "https://civitai.com/models/",
            "https://civitai.com/search/models",
            "https://civitai.com/user/w/images?section=reactions",
            "https://civitai.com/user/w/videos?section=reactions",
            "https://civitai.com/generate",
            "https://civitai.com/user/w/unknown",
            "https://civitai.com.evil.com/models/1",
            "https://evilcivitai.com/models/1",
            "https://image.civitai.com/models/1",
            "https://civitai.com:8443/models/1",
            "https://u:p@civitai.com/models/1",
            "ftp://civitai.com/models/1",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
        assert!(
            crate::sites::find(&Url::parse("https://civitai.com/models/1").unwrap())
                .is_some_and(|site| site.category == "civitai")
        );
    }

    #[test]
    fn parses_and_types_query_parameters() {
        let mut params = parse_query(Some(
            "sort=Most+Collected&tags=6594&baseModels=PixArt+a&baseModels=Pony&remixesOnly=false&tags=7&sort=x",
        ));
        assert_eq!(params["sort"], "Most Collected");
        assert_eq!(params["baseModels"], json!(["PixArt a", "Pony"]));
        type_params(&mut params).unwrap();
        assert_eq!(params["tags"], json!([6594, 7]));
        assert_eq!(params["remixesOnly"], false);
        let mut bad = parse_query(Some("modelId=abc"));
        assert!(type_params(&mut bad).is_err());
        assert_eq!(
            search_filter(31),
            "(nsfwLevel=1 OR nsfwLevel=2 OR nsfwLevel=4 OR nsfwLevel=8 OR nsfwLevel=16)"
        );
        assert_eq!(search_filter(1), "(nsfwLevel=1)");
        assert_eq!(search_filter(0), "()");
        assert_eq!(art::parse_iso("2024-08-30T15:28:47.661Z"), Some(1725031727));
    }

    fn cfg_with(options: Value) -> Cfg {
        let http = Http::new("civitai", std::time::Duration::from_secs(1))
            .unwrap()
            .with_options(Some(Arc::new(object(options))));
        Cfg::new(&http)
    }

    #[test]
    fn builds_download_urls_like_upstream() {
        let cfg = cfg_with(json!({}));
        assert_eq!(cfg.nsfw, 31);
        assert_eq!(cfg.sort, "Newest");
        assert_eq!(cfg.period, "AllTime");
        let mut image = object(json!({"id": 5, "url": "uuid-1", "name": "a b.png"}));
        assert_eq!(
            cfg.url(&mut image).unwrap(),
            format!("{IMAGE_ROOT}/uuid-1/original=true/a b.png")
        );
        assert_eq!(image["uuid"], "uuid-1");
        let mut video =
            object(json!({"id": 6, "url": "uuid-2", "type": "video", "mimeType": "video/mp4"}));
        assert_eq!(
            cfg.url(&mut video).unwrap(),
            format!("{IMAGE_ROOT}/uuid-2/original=true,quality=100/6.mp4")
        );
        let mut bare = object(json!({"id": 7, "url": "uuid-3", "type": "video"}));
        assert!(cfg.url(&mut bare).unwrap().ends_with("/7.webm"));
        let mut full = object(json!({"url": format!("{IMAGE_ROOT}/uuid-4/width=450/x.jpeg")}));
        assert_eq!(
            cfg.url(&mut full).unwrap(),
            format!("{IMAGE_ROOT}/uuid-4/original=true/x.jpeg")
        );
        assert_eq!(full["uuid"], "uuid-4");

        let cfg = cfg_with(
            json!({"quality": ["width=1024", "q=1"], "quality-videos": "+quality=50",
            "nsfw": false, "sort": "oldest", "period": "Week"}),
        );
        assert_eq!(cfg.image_quality, "width=1024,q=1");
        assert_eq!(cfg.image_ext, "jpg");
        assert_eq!(cfg.video_quality, "width=1024,q=1,quality=50");
        assert_eq!(cfg.nsfw, 1);
        assert_eq!(cfg.sort, "Oldest");
        assert_eq!(cfg.period, "Week");
        let cfg = cfg_with(json!({"quality": "w", "quality-videos": ""}));
        assert_eq!(cfg.video_quality, "w");
    }

    fn image(id: u64, post: u64) -> Value {
        json!({"id": id, "url": format!("uuid-{id}"), "name": format!("{id:05}-seed.png"),
            "createdAt": "2024-08-31T01:11:47.021Z", "postId": post, "type": "image",
            "user": {"id": 9, "username": "bolero537"}})
    }

    #[tokio::test]
    async fn extracts_single_image() {
        let (root, server) = serve(vec![trpc_reply(image(26962948, 6030721))]);
        let http = http("civitai", root);
        let files = media(extract(
            &http,
            Url::parse("https://civitai.com/images/26962948").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].url.as_str(),
            format!("{IMAGE_ROOT}/uuid-26962948/original=true/26962948-seed.png")
        );
        let k = keywords(&files[0]);
        assert_eq!(k["filename"], "26962948-seed");
        assert_eq!(k["extension"], "png");
        assert_eq!(k["user"]["username"], "bolero537");
        assert_eq!(k["file"]["date"], "2024-08-31 01:11:47");
        assert_eq!(k["file"]["uuid"], "uuid-26962948");
        assert!(k["file"].get("user").is_none());
        assert_eq!(k["subcategory"], "image");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "26962948.png");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            PathBuf::from("civitai/bolero537/images")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{file[uuid]}",["uuid-26962948"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/trpc/image.get?input="));
        assert!(requests[0].contains("x-client: web"));
        assert_eq!(input(&requests[0]), json!({"json": {"id": 26962948}}));
    }

    #[tokio::test]
    async fn api_key_is_sent_as_bearer_token() {
        let (root, server) = serve(vec![trpc_reply(image(1, 2))]);
        let http = http("civitai", root).with_api_key(Some("secret"));
        let files = media(extract(
            &http,
            Url::parse("https://civitai.com/images/1").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let requests = server.join().unwrap();
        assert!(
            requests[0]
                .to_ascii_lowercase()
                .contains("authorization: bearer secret")
        );
    }

    #[tokio::test]
    async fn response_without_result_is_an_error() {
        let (root, server) = serve(vec![reply(json!({"error": {"message": "nope"}}))]);
        let http = http("civitai", root);
        let events = extract(&http, Url::parse("https://civitai.com/images/1").unwrap());
        let results: Vec<_> = crate::event::media_only(events).collect().await;
        assert!(results.iter().any(Result::is_err));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn extracts_post_images_across_pages() {
        let post = json!({"id": 6877551, "title": "Showcase", "publishedAt": "2024-09-22T12:54:15.000Z",
            "user": {"id": 1, "username": "author"}});
        let mut uuid_only = image(3, 6877551);
        uuid_only.as_object_mut().unwrap().remove("name");
        uuid_only.as_object_mut().unwrap().remove("id");
        let (root, server) = serve(vec![
            trpc_reply(post),
            trpc_reply(
                json!({"items": [image(1, 6877551), image(2, 6877551)], "nextCursor": "c2"}),
            ),
            trpc_reply(json!({"items": [uuid_only], "nextCursor": null})),
        ]);
        let http = http("civitai", root);
        let files = media(extract(
            &http,
            Url::parse("https://civitai.com/posts/6877551").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 3);
        let k = keywords(&files[1]);
        assert_eq!(k["num"], 2);
        assert_eq!(k["post"]["date"], "2024-09-22 12:54:15");
        assert_eq!(k["user"]["username"], "author");
        assert_eq!(k["file"]["date"], "2024-08-31 01:11:47");
        // `_image_results` names files after the raw `url` value.
        assert_eq!(k["filename"], "uuid-2");
        assert_eq!(k["extension"], "png");
        assert_eq!(keywords(&files[2])["num"], 3);
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "1.png");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            PathBuf::from("civitai/author/posts/6877551 Showcase")
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/trpc/post.get?"));
        assert!(requests[1].starts_with("GET /api/trpc/image.getInfinite?"));
        let first = input(&requests[1]);
        assert_eq!(first["json"]["postId"], 6877551);
        assert_eq!(first["json"]["pending"], true);
        assert_eq!(first["json"]["browsingLevel"], 31);
        assert_eq!(first["json"]["cursor"], Value::Null);
        assert_eq!(first["meta"], json!({"values": {"cursor": ["undefined"]}}));
        let second = input(&requests[2]);
        assert_eq!(second["json"]["cursor"], "c2");
        assert!(second.get("meta").is_none());
    }

    #[tokio::test]
    async fn extracts_model_version() {
        let model = json!({"id": 703211, "name": "maid classic", "type": "LORA",
        "user": {"id": 9, "username": "bolero537"},
        "modelVersions": [
            {"id": 1, "name": "other", "createdAt": "2024-08-30T15:28:47.661Z"},
            {"id": 786644, "name": "v1.0 pony", "createdAt": "2024-08-30T15:28:47.661Z"}
        ]});
        let (root, server) = serve(vec![
            trpc_reply(model),
            trpc_reply(json!({"items": [image(26887852, 1)], "nextCursor": null})),
        ]);
        let http = http("civitai", root);
        let files = media(extract(
            &http,
            Url::parse("https://civitai.com/models/703211?modelVersionId=786644").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let k = keywords(&files[0]);
        assert_eq!(k["version"]["date"], "2024-08-30 15:28:47");
        assert_eq!(k["model"]["name"], "maid classic");
        assert!(k["model"].get("modelVersions").is_none());
        assert_eq!(k["num"], 1);
        let (_, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            PathBuf::from("civitai/bolero537/703211 maid classic/786644 v1.0 pony")
        );
        let requests = server.join().unwrap();
        let params = input(&requests[1]);
        assert_eq!(params["json"]["modelVersionId"], 786644);
        assert_eq!(params["json"]["prioritizedUserIds"], json!([9]));
        assert_eq!(params["json"]["limit"], 20);
    }

    #[tokio::test]
    async fn tag_queues_models_lazily() {
        let model = json!({"id": 5, "name": "m", "creator": {"id": 2, "username": "c"},
            "modelVersions": [{"id": 6, "name": "v", "createdAt": "2024-01-01T00:00:00Z",
                "images": [image(10, 1)]}]});
        let (root, server) = serve(vec![
            trpc_reply(json!({"items": [{"id": 5}], "nextCursor": null})),
            trpc_reply(model),
        ]);
        let http = http("civitai", root);
        let files = media(extract(
            &http,
            Url::parse("https://civitai.com/tag/mecha").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(keywords(&files[0])["user"]["username"], "c");
        assert_eq!(keywords(&files[0])["subcategory"], "model");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/trpc/model.getAll?"));
        let params = input(&requests[0]);
        assert_eq!(params["json"]["tagname"], "mecha");
        assert_eq!(params["json"]["periodMode"], "published");
        assert!(requests[1].starts_with("GET /api/trpc/model.getById?"));
    }

    #[tokio::test]
    async fn collection_adds_collection_keywords() {
        let (root, server) = serve(vec![
            trpc_reply(json!({"collection": {"id": 11, "name": "Trees",
                "user": {"id": 3, "username": "TettyCo"}}})),
            trpc_reply(json!({"items": [image(20, 2)]})),
        ]);
        let http = http("civitai", root);
        let files = media(extract(
            &http,
            Url::parse("https://civitai.com/collections/11").unwrap(),
        ))
        .await;
        let k = keywords(&files[0]);
        assert_eq!(k["collection"]["name"], "Trees");
        assert!(k["collection"].get("user").is_none());
        let (_, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            PathBuf::from("civitai/TettyCo/collections/11 Trees")
        );
        let requests = server.join().unwrap();
        let params = input(&requests[1]);
        assert_eq!(params["json"]["collectionId"], 11);
        assert!(params["json"].get("useIndex").is_none());
    }

    #[tokio::test]
    async fn user_dispatches_images_and_videos() {
        let mut video = image(31, 3);
        video["type"] = json!("video");
        video["name"] = json!("clip.mp4");
        let (root, server) = serve(vec![
            trpc_reply(json!({"items": [image(30, 3)]})),
            trpc_reply(json!({"items": [video]})),
        ]);
        let http = http("civitai", root);
        let files = media(extract(
            &http,
            Url::parse("https://civitai.com/user/bolero537").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(keywords(&files[0])["subcategory"], "user-images");
        assert_eq!(keywords(&files[1])["subcategory"], "user-videos");
        assert!(
            files[1]
                .url
                .as_str()
                .contains("/original=true,quality=100/clip.mp4")
        );
        let (_, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            PathBuf::from("civitai/bolero537/videos")
        );
        let requests = server.join().unwrap();
        let images = input(&requests[0]);
        assert_eq!(images["json"]["types"], json!(["image"]));
        assert_eq!(images["json"]["username"], "bolero537");
        assert_eq!(input(&requests[1])["json"]["types"], json!(["video"]));
    }

    #[tokio::test]
    async fn user_posts_skip_foreign_first_page() {
        let (root, server) = serve(vec![trpc_reply(json!({"items": [
            {"id": 1, "user": {"username": "someone-else"}}], "nextCursor": "x"}))]);
        let http = http("civitai", root);
        let files = media(extract(
            &http,
            Url::parse("https://civitai.com/user/deleted/posts").unwrap(),
        ))
        .await;
        assert!(files.is_empty());
        let requests = server.join().unwrap();
        assert_eq!(input(&requests[0])["json"]["username"], "deleted");
    }

    #[tokio::test]
    async fn search_images_posts_meilisearch_queries() {
        let mut hit = image(40, 4);
        hit["url"] = json!("uuid-40");
        let (root, server) = serve(vec![reply(json!({"results": [{"hits": [hit]}]}))]);
        let http =
            http("civitai", root).with_options(Some(Arc::new(object(json!({"nsfw": false})))));
        let files = media(extract(
            &http,
            Url::parse("https://civitai.com/search/images?sortBy=images_v6&query=Voynich").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(keywords(&files[0])["subcategory"], "search-images");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("POST /multi-search "));
        assert!(
            requests[0]
                .to_ascii_lowercase()
                .contains("authorization: bearer 8c46")
        );
        let body: Value =
            serde_json::from_str(requests[0].split("\r\n\r\n").nth(1).unwrap()).unwrap();
        let query = &body["queries"][0];
        assert_eq!(query["q"], "Voynich");
        assert_eq!(query["indexUid"], "images_v6");
        assert_eq!(query["filter"], json!(["(nsfwLevel=1)"]));
        assert_eq!(query["limit"], 51);
    }
}
