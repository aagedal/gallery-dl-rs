//! rule34.xyz and rule34.world, mirroring gallery_dl/extractor/rule34xyz.py.
//!
//! The JSON API shape (`POST /api/v2/post/search/...` pagination and
//! `GET /api/v2/post/{id}`) is shared with rule34vault, furry34 and
//! yiffverse; their modules reuse the [`Api`] engine below with their own
//! file URL, pagination and tag-type rules.
use super::{below, booru_fields, date_value, iso_timestamp, leading_digits, rpartition, unquote};
use crate::event::ExtractionEvent;
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use std::collections::VecDeque;
use url::Url;

const FILENAME: &str = "{category}_{id}.{extension}";
static POST: Route = Route::new("post", FILENAME, &["{category}"], "{id}");
static PLAYLIST: Route = Route::new(
    "playlist",
    FILENAME,
    &["{category}", "{playlist_id}"],
    "p_{playlist_id}_{id}",
);
static TAG: Route = Route::new(
    "tag",
    FILENAME,
    &["{category}", "{search_tags}"],
    "t_{search_tags}_{id}",
);

pub static RULE34XYZ: Site = Site {
    category: "rule34xyz",
    name: "rule34.xyz",
    matches: |url| target(&XYZ, url).is_some(),
    extract: |http, url| extract(&XYZ, http, url),
    routes: &[&POST, &PLAYLIST, &TAG],
    api_key: false,
    cookies: None,
};

pub static RULE34WORLD: Site = Site {
    category: "rule34world",
    name: "rule34.world",
    matches: |url| target(&WORLD, url).is_some(),
    extract: |http, url| extract(&WORLD, http, url),
    routes: &[&POST, &PLAYLIST, &TAG],
    api_key: false,
    cookies: None,
};

const TAG_TYPES: &[(Option<i64>, &str)] = &[
    (None, "general"),
    (Some(0), "general"),
    (Some(1), "general"),
    (Some(2), "copyright"),
    (Some(4), "character"),
    (Some(8), "artist"),
    (Some(16), "system"),
    (Some(32), "meta"),
];

static XYZ: Api = Api {
    site: &RULE34XYZ,
    hosts: &["rule34.xyz", "www.rule34.xyz"],
    root: "https://rule34.xyz",
    root_cdn: "https://rule34xyz.b-cdn.net",
    per_page: 60,
    file_url,
    split_filename: true,
    tag_types: TAG_TYPES,
    falsy_type: None,
    start,
    advance,
};

static WORLD: Api = Api {
    site: &RULE34WORLD,
    hosts: &["rule34.world", "www.rule34.world"],
    root: "https://rule34.world",
    root_cdn: "https://rule34storage.b-cdn.net",
    per_page: 60,
    file_url,
    split_filename: true,
    tag_types: TAG_TYPES,
    falsy_type: None,
    start,
    advance,
};

/// `Rule34xyzExtractor._pagination` initial parameters.
fn start(params: &mut Map<String, Value>, per_page: usize) {
    params.insert("Skip".into(), 0.into());
    params.insert("take".into(), per_page.into());
    params.insert("CountTotal".into(), false.into());
    params.insert("IncludeLinks".into(), true.into());
    params.insert("OrderBy".into(), 0.into());
}

/// Next page: `Skip += per_page; cursor = data["cursor"]`.
fn advance(params: &mut Map<String, Value>, cursor: Option<&Value>, per_page: usize) -> Result<()> {
    let skip = params.get("Skip").and_then(Value::as_u64).unwrap_or(0);
    params.insert("Skip".into(), (skip + per_page as u64).into());
    let cursor = cursor.ok_or_else(|| error("rule34.xyz response lacks a page cursor"))?;
    params.insert("cursor".into(), cursor.clone());
    Ok(())
}

/// The `format` option: an ordered list of `files` keys. Non-string list
/// entries never match a key; `None` means iteration fails upstream.
fn formats(http: &Http) -> Option<Vec<Option<String>>> {
    let default = || {
        ["100", "101", "102", "10"]
            .map(|f| Some(f.to_owned()))
            .to_vec()
    };
    Some(match http.option("format") {
        None | Some(Value::Null | Value::Bool(false)) => default(),
        Some(Value::String(s)) if s.is_empty() => default(),
        Some(Value::String(s)) => s.split(',').map(|f| Some(f.to_owned())).collect(),
        Some(Value::Array(a)) if a.is_empty() => default(),
        Some(Value::Array(a)) => a.iter().map(|f| f.as_str().map(str::to_owned)).collect(),
        Some(Value::Object(o)) if o.is_empty() => default(),
        Some(Value::Object(o)) => o.keys().map(|k| Some(k.clone())).collect(),
        Some(Value::Number(n)) if n.as_f64() == Some(0.0) => default(),
        Some(_) => return None,
    })
}

/// `Rule34xyzExtractor._file_url`; `None` where upstream raises and skips.
fn file_url(api: &Api, http: &Http, post: &mut Map<String, Value>) -> Option<String> {
    const FORMATS: &[(&str, &str)] = &[
        ("10", "pic.jpg"),
        ("100", "mov.mp4"),
        ("101", "mov720.mp4"),
        ("102", "mov480.mp4"),
    ];
    let files = post.get("files")?.as_object()?;
    // Without a match upstream logs a warning and then fails on the unbound
    // `extension`, so the post is skipped.
    let fmt = formats(http)?
        .into_iter()
        .flatten()
        .find(|fmt| files.contains_key(fmt))?;
    // A configured format outside FORMATS leaves `extension` as None, which
    // fails on `extension.partition`.
    let extension = FORMATS.iter().find(|(f, _)| *f == fmt)?.1;
    api.media_url(post, fmt, extension)
}

// ---------------------------------------------------------------------------
// Shared engine for the rule34xyz-style JSON API.

/// `_pagination` parameters for the next page, given `data["cursor"]`.
pub(super) type Advance = fn(&mut Map<String, Value>, Option<&Value>, usize) -> Result<()>;

pub(super) struct Api {
    pub site: &'static Site,
    /// Accepted hosts (exact).
    pub hosts: &'static [&'static str],
    pub root: &'static str,
    pub root_cdn: &'static str,
    pub per_page: usize,
    /// `_file_url`: sets `file_url` (and format keys); `None` skips the post.
    pub file_url: fn(&Api, &Http, &mut Map<String, Value>) -> Option<String>,
    /// `_prepare` splits `filename` into `filename` and `format`.
    pub split_filename: bool,
    /// `TAG_TYPES` for the `tags` option.
    pub tag_types: &'static [(Option<i64>, &'static str)],
    /// furry34's `tag["type"] or 1` substitute for falsy types.
    pub falsy_type: Option<i64>,
    /// `_pagination` initial parameters.
    pub start: fn(&mut Map<String, Value>, usize),
    /// `_pagination` parameters for the next page, given `data["cursor"]`.
    pub advance: Advance,
}

/// What an URL asks for.
#[derive(Debug)]
pub(super) enum Target {
    Post(String),
    Search {
        route: &'static Route,
        endpoint: String,
        params: Map<String, Value>,
        data: Map<String, Value>,
    },
}

impl Api {
    /// The URL's `path?query` when it is on one of this site's hosts.
    pub fn below(&self, url: &Url) -> Option<String> {
        below(url, self.hosts)
    }

    /// `/post/ID` (prefix match), mirroring `BASE_PATTERN + r"/post/(\d+)"`.
    pub fn post(&self, rest: &str) -> Option<Target> {
        let id = leading_digits(rest.strip_prefix("/post/")?)?;
        Some(Target::Post(id.to_owned()))
    }

    /// A playlist URL `{prefix}ID`.
    pub fn playlist(&self, rest: &str, prefix: &str, route: &'static Route) -> Option<Target> {
        let id = leading_digits(rest.strip_prefix(prefix)?)?;
        let mut data = Map::new();
        data.insert("playlist_id".into(), id.into());
        Some(Target::Search {
            route,
            endpoint: format!("/v2/post/search/playlist/{id}"),
            params: Map::new(),
            data,
        })
    }

    /// A tag search for `tags`, with `includeTags` first.
    pub fn search(
        &self,
        route: &'static Route,
        search_tags: String,
        include: Vec<String>,
        extra: Option<(&str, Value)>,
    ) -> Target {
        let mut params = Map::new();
        params.insert("includeTags".into(), include.into());
        if let Some((key, value)) = extra {
            params.insert(key.into(), value);
        }
        let mut data = Map::new();
        data.insert("search_tags".into(), search_tags.into());
        Target::Search {
            route,
            endpoint: "/v2/post/search/root".into(),
            params,
            data,
        }
    }

    /// `{root}/posts/{id // 1000}/{id}/{id}.{extension}` with the CDN root
    /// when `files[fmt][0]` is truthy; also sets `file_url`, `format_id`
    /// and `format`. `None` where upstream raises.
    pub fn media_url(
        &self,
        post: &mut Map<String, Value>,
        fmt: String,
        extension: &str,
    ) -> Option<String> {
        let first = match post.get("files")?.as_object()?.get(&fmt)? {
            Value::Array(items) => truthy(items.first()?),
            // A string's first character is always truthy.
            Value::String(s) if !s.is_empty() => true,
            _ => return None,
        };
        let id = post.get("id")?.as_i64()?;
        let root = if first { self.root_cdn } else { self.root };
        let url = format!("{root}/posts/{}/{id}/{id}.{extension}", id.div_euclid(1000));
        post.insert("file_url".into(), url.clone().into());
        post.insert("format_id".into(), fmt.into());
        post.insert(
            "format".into(),
            super::before(extension, ".").to_owned().into(),
        );
        Some(url)
    }
}

/// Python truthiness of a JSON value.
pub(super) fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Stream a target's posts.
pub(super) fn run<'a>(api: &'static Api, http: &'a Http, target: Target) -> Events<'a> {
    match target {
        Target::Post(id) => crate::sites::once(async move {
            let post = fetch_post(api, http, &id).await?;
            // The site's `post` route is registered first.
            let events = emit(api, http, api.site.routes[0], post, &Map::new()).await?;
            Ok(crate::sites::events(events))
        }),
        Target::Search {
            route,
            endpoint,
            mut params,
            data,
        } => {
            (api.start)(&mut params, api.per_page);
            let state = Pages {
                params,
                pending: VecDeque::new(),
                cursor: None,
                fetched: false,
                more: true,
            };
            crate::sites::paginate(state, move |mut state| {
                let endpoint = endpoint.clone();
                let data = data.clone();
                async move {
                    if let Some(post) = state.pending.pop_front() {
                        let events = emit(api, http, route, post, &data).await?;
                        return Ok(Some((crate::sites::events(events), state)));
                    }
                    if !state.more {
                        return Ok(None);
                    }
                    if state.fetched {
                        (api.advance)(&mut state.params, state.cursor.as_ref(), api.per_page)?;
                    }
                    let url = http.api(&format!("{}/api{endpoint}", api.root))?;
                    let page = http
                        .post_json(&url, &Value::Object(state.params.clone()), &[])
                        .await?;
                    let items = page
                        .get("items")
                        .and_then(Value::as_array)
                        .ok_or_else(|| error(format!("{} returned no post list", api.site.name)))?;
                    state.fetched = true;
                    state.more = items.len() >= api.per_page;
                    state.cursor = page.get("cursor").cloned();
                    state.pending = items
                        .iter()
                        .map(|post| {
                            post.as_object().cloned().ok_or_else(|| {
                                error(format!("{} returned an invalid post", api.site.name))
                            })
                        })
                        .collect::<Result<_>>()?;
                    Ok(Some((crate::sites::events(Vec::new()), state)))
                }
            })
        }
    }
}

struct Pages {
    params: Map<String, Value>,
    pending: VecDeque<Map<String, Value>>,
    cursor: Option<Value>,
    fetched: bool,
    more: bool,
}

/// `_fetch_post`: `GET {root}/api/v2/post/{id}`.
async fn fetch_post(api: &Api, http: &Http, id: &str) -> Result<Map<String, Value>> {
    let url = http.api(&format!("{}/api/v2/post/{id}", api.root))?;
    match http.json(&url).await? {
        Value::Object(post) => Ok(post),
        _ => Err(error(format!("{} returned an invalid post", api.site.name))),
    }
}

/// One `BooruExtractor.items` iteration.
async fn emit(
    api: &Api,
    http: &Http,
    route: &'static Route,
    mut post: Map<String, Value>,
    data: &Map<String, Value>,
) -> Result<Vec<ExtractionEvent>> {
    let Some(url) = (api.file_url)(api, http, &mut post).and_then(|u| Url::parse(&u).ok()) else {
        // Upstream: "Unable to fetch download URL for post ..." and continue.
        return Ok(Vec::new());
    };
    if http.option_bool("tags", false) {
        tags(api, http, &mut post).await?;
    }
    booru_fields(&url, &mut post, data);
    // _prepare
    post.shift_remove("files");
    let timestamp = post
        .get("created")
        .and_then(Value::as_str)
        .and_then(iso_timestamp);
    post.insert("date".into(), date_value(timestamp));
    if api.split_filename
        && let Some(Value::String(name)) = post.get("filename")
    {
        let (head, tail) = rpartition(name, ".");
        let (head, tail) = (head.to_owned(), tail.to_owned());
        post.insert("filename".into(), head.into());
        post.insert("format".into(), tail.into());
    }
    if let Some(tags) = post.get("tags") {
        let values = tag_list(api, tags)?
            .into_iter()
            .map(|(_, value)| value)
            .collect::<Vec<_>>();
        post.insert("tags".into(), values.into());
    }
    let id = post.get("id").map(crate::sites::scalar).unwrap_or_default();
    let user = post
        .get("uploader")
        .and_then(|u| u.get("userName"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let item = Item::new(url, post)
        .ids(id.clone(), id)
        .user(user)
        .timestamp(timestamp);
    Ok(crate::sites::post(api.site, http, route, vec![item]))
}

/// Tag objects as `(type, value)`; `type` is `None` when absent.
fn tag_list(api: &Api, tags: &Value) -> Result<Vec<(Option<Value>, Value)>> {
    let invalid = || error(format!("{} returned invalid tag data", api.site.name));
    tags.as_array()
        .ok_or_else(invalid)?
        .iter()
        .map(|tag| {
            let tag = tag.as_object().ok_or_else(invalid)?;
            Ok((
                tag.get("type").cloned(),
                tag.get("value").cloned().ok_or_else(invalid)?,
            ))
        })
        .collect()
}

/// `_tags`: fetch the full post when it lacks `tags`, then set
/// `tags_<type>` lists. Types mapping to the same name overwrite each other
/// in first-seen order, as upstream's dict assignment does.
async fn tags(api: &Api, http: &Http, post: &mut Map<String, Value>) -> Result<()> {
    if !post.contains_key("tags") {
        let id = post.get("id").map(crate::sites::scalar).unwrap_or_default();
        for (key, value) in fetch_post(api, http, &id).await? {
            post.insert(key, value);
        }
    }
    let unknown = || error(format!("{} returned an unknown tag type", api.site.name));
    let mut groups: Vec<(Option<i64>, Vec<Value>)> = Vec::new();
    for (kind, value) in tag_list(api, post.get("tags").unwrap_or(&Value::Null))? {
        let kind = kind.ok_or_else(unknown)?;
        let kind = match api.falsy_type {
            Some(default) if !truthy(&kind) => Some(default),
            _ => match kind {
                Value::Null => None,
                other => Some(other.as_i64().ok_or_else(unknown)?),
            },
        };
        match groups.iter_mut().find(|(k, _)| *k == kind) {
            Some((_, values)) => values.push(value),
            None => groups.push((kind, vec![value])),
        }
    }
    for (kind, values) in groups {
        let name = api
            .tag_types
            .iter()
            .find(|(k, _)| *k == kind)
            .map(|(_, name)| *name)
            .ok_or_else(unknown)?;
        post.insert(format!("tags_{name}"), values.into());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// rule34xyz URL patterns

fn target(api: &Api, url: &Url) -> Option<Target> {
    let rest = api.below(url)?;
    if let Some(target) = api.post(&rest) {
        return Some(target);
    }
    if let Some(target) = api.playlist(&rest, "/playlists/view/", &PLAYLIST) {
        return Some(target);
    }
    // r"/([^/?#]+)$": one segment ending the URL.
    let tag = rest.strip_prefix('/')?;
    if tag.is_empty() || tag.contains(['/', '?']) || url.fragment().is_some() {
        return None;
    }
    let tags: Vec<String> = unquote(&unquote(tag).replace('_', " "))
        .split('|')
        .map(str::to_owned)
        .collect();
    Some(api.search(&TAG, tags.join(", "), tags, None))
}

fn extract<'a>(api: &'static Api, http: &'a Http, url: Url) -> Events<'a> {
    match target(api, &url) {
        Some(target) => run(api, http, target),
        None => crate::sites::fail(error(format!("unsupported {} URL", api.site.name))),
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::sites::testing::{fails, http, json, keywords, media, serve};
    use serde_json::json;
    use std::sync::Arc;

    pub fn with_options(http: Http, options: Value) -> Http {
        http.with_options(Some(Arc::new(options.as_object().unwrap().clone())))
    }

    /// The JSON body of a recorded request.
    pub fn body(request: &str) -> Value {
        serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap()
    }

    fn t(raw: &str) -> Option<(&'static str, Target)> {
        let url = Url::parse(raw).unwrap();
        let site = crate::sites::find(&url)?;
        let api = match site.category {
            "rule34xyz" => &XYZ,
            "rule34world" => &WORLD,
            _ => return None,
        };
        Some((site.category, target(api, &url)?))
    }

    #[test]
    fn classifies_urls() {
        assert!(matches!(
            t("https://rule34.xyz/post/3613851"),
            Some(("rule34xyz", Target::Post(id))) if id == "3613851"
        ));
        assert!(matches!(
            t("https://www.rule34.world/post/12x"),
            Some(("rule34world", Target::Post(id))) if id == "12"
        ));
        let Some((
            "rule34xyz",
            Target::Search {
                route,
                endpoint,
                data,
                ..
            },
        )) = t("https://rule34.xyz/playlists/view/119")
        else {
            panic!()
        };
        assert_eq!(route.subcategory, "playlist");
        assert_eq!(endpoint, "/v2/post/search/playlist/119");
        assert_eq!(data["playlist_id"], "119");
        let Some((
            "rule34world",
            Target::Search {
                route,
                params,
                data,
                ..
            },
        )) = t("https://rule34.world/preview_%2528preview0%2529%7Cb")
        else {
            panic!()
        };
        assert_eq!(route.subcategory, "tag");
        assert_eq!(data["search_tags"], "preview (preview0), b");
        assert_eq!(params["includeTags"], json!(["preview (preview0)", "b"]));
        // `/post` without an id is a tag, like the upstream pattern order.
        assert!(matches!(
            t("https://rule34.xyz/post"),
            Some((_, Target::Search { .. }))
        ));
        for bad in [
            "https://rule34.xyz/",
            "https://rule34.xyz",
            "https://rule34.xyz/sfw?x=1",
            "https://rule34.xyz/sfw/",
            "https://rule34.xyz/sfw#top",
            "https://rule34.xyz/post/abc",
            "https://rule34.xyz.evil.test/post/1",
            "https://evilrule34.xyz/post/1",
            "https://m.rule34.xyz/post/1",
            "https://rule34.xyz:8080/post/1",
            "https://user@rule34.xyz/post/1",
            "ftp://rule34.xyz/post/1",
            "https://rule34.com/post/1",
        ] {
            assert!(t(bad).is_none(), "{bad}");
        }
    }

    fn post(id: i64, files: Value) -> Value {
        json!({"id": id, "created": "2023-03-29T03:00:59.136819Z", "type": 0,
            "uploaderId": 9741, "files": files,
            "uploader": {"userName": "someone"}})
    }

    #[tokio::test]
    async fn post_selects_format_and_tags() {
        let mut full = post(3613851, json!({"10": [true]}))
            .as_object()
            .unwrap()
            .clone();
        full.insert(
            "tags".into(),
            json!([{"type": 8, "value": "artist a"}, {"type": 1, "value": "g1"},
                   {"type": 0, "value": "g0"}, {"type": null, "value": "gn"},
                   {"type": 2, "value": "copy"}]),
        );
        let (root, server) = serve(vec![json(Value::Object(full))]);
        let http = with_options(http("rule34.xyz", root), json!({"tags": true}));
        let files = media(extract(
            &XYZ,
            &http,
            Url::parse("https://rule34.xyz/post/3613851").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].url.as_str(),
            "https://rule34xyz.b-cdn.net/posts/3613/3613851/3613851.pic.jpg"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["id"], 3613851);
        assert_eq!(k["filename"], "3613851");
        assert_eq!(k["extension"], "jpg");
        assert_eq!(k["format"], "pic");
        assert_eq!(k["format_id"], "10");
        assert_eq!(k["date"], "2023-03-29 03:00:59");
        assert!(!k.contains_key("files"));
        assert_eq!(k["tags"], json!(["artist a", "g1", "g0", "gn", "copy"]));
        assert_eq!(k["tags_artist"], json!(["artist a"]));
        // Types 1, 0 and None all map to "general"; the last group wins.
        assert_eq!(k["tags_general"], json!(["gn"]));
        assert_eq!(k["tags_copyright"], json!(["copy"]));
        assert_eq!(k["subcategory"], "post");
        let meta = files[0].metadata.as_ref().unwrap();
        assert_eq!(meta.username.as_deref(), Some("someone"));
        assert_eq!(meta.timestamp, Some(1680058859));
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "rule34xyz_3613851.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("rule34xyz")
        );
        let (category, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(category, "rule34xyz");
        assert_eq!(identity, r#"["{id}",["3613851"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/v2/post/3613851 "));
    }

    #[test]
    fn file_urls_follow_format_option() {
        let (root, _) = (Url::parse("http://127.0.0.1:1/").unwrap(), ());
        let plain = http("rule34.world", root.clone());
        let mut p = post(3571567, json!({"10": [true], "101": [false], "100": []}))
            .as_object()
            .unwrap()
            .clone();
        // "100" is listed first but its empty list fails `files[fmt][0]`.
        assert_eq!(file_url(&WORLD, &plain, &mut p.clone()), None);
        p.insert("files".into(), json!({"10": [true], "101": [0]}));
        assert_eq!(
            file_url(&WORLD, &plain, &mut p).as_deref(),
            Some("https://rule34.world/posts/3571/3571567/3571567.mov720.mp4")
        );
        assert_eq!(p["format"], "mov720");
        assert_eq!(p["format_id"], "101");
        let custom = with_options(
            http("rule34.world", root.clone()),
            json!({"format": "10,4,5"}),
        );
        assert_eq!(
            file_url(&WORLD, &custom, &mut p).as_deref(),
            Some("https://rule34storage.b-cdn.net/posts/3571/3571567/3571567.pic.jpg")
        );
        let unknown = with_options(http("rule34.world", root.clone()), json!({"format": ["7"]}));
        p.insert("files".into(), json!({"7": [true]}));
        assert_eq!(file_url(&WORLD, &unknown, &mut p), None);
        // No requested format available: skipped upstream.
        assert_eq!(file_url(&WORLD, &plain, &mut p), None);
    }

    #[tokio::test]
    async fn paginates_tag_search() {
        let first: Vec<_> = (1..=60).map(|i| post(i, json!({"10": [false]}))).collect();
        let (root, server) = serve(vec![
            json(json!({"items": first, "cursor": "c1"})),
            json(json!({"items": [post(1061, json!({"100": [true]}))], "cursor": "c2"})),
        ]);
        let http = http("rule34.xyz", root);
        let files = media(extract(
            &XYZ,
            &http,
            Url::parse("https://rule34.xyz/a_b%7Cc").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 61);
        assert_eq!(
            files[0].url.as_str(),
            "https://rule34.xyz/posts/0/1/1.pic.jpg"
        );
        assert_eq!(
            files[60].url.as_str(),
            "https://rule34xyz.b-cdn.net/posts/1/1061/1061.mov.mp4"
        );
        let k = keywords(&files[60]);
        assert_eq!(k["search_tags"], "a b, c");
        assert_eq!(k["filename"], "1061");
        assert_eq!(k["format"], "mov");
        assert_eq!(k["subcategory"], "tag");
        let (file, dir) = crate::sites::default_templates(&files[60]).unwrap();
        assert_eq!(file.render(&files[60]).unwrap(), "rule34xyz_1061.mp4");
        assert_eq!(
            dir.render(&files[60]).unwrap().unwrap(),
            std::path::PathBuf::from("rule34xyz/a b, c")
        );
        let (_, identity) = crate::sites::archive_key(&files[60]).unwrap();
        assert_eq!(identity, r#"["t_{search_tags}_{id}",["a b, c","1061"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("POST /api/v2/post/search/root "));
        assert_eq!(
            body(&requests[0]).to_string(),
            r#"{"includeTags":["a b","c"],"Skip":0,"take":60,"CountTotal":false,"IncludeLinks":true,"OrderBy":0}"#
        );
        assert_eq!(
            body(&requests[1]).to_string(),
            r#"{"includeTags":["a b","c"],"Skip":60,"take":60,"CountTotal":false,"IncludeLinks":true,"OrderBy":0,"cursor":"c1"}"#
        );
    }

    #[tokio::test]
    async fn playlist_keywords_and_archive() {
        let (root, server) = serve(vec![json(json!({"items": [post(5, json!({"10": [1]}))]}))]);
        let http = http("rule34.world", root);
        let files = media(extract(
            &WORLD,
            &http,
            Url::parse("https://rule34.world/playlists/view/259015").unwrap(),
        ))
        .await;
        let k = keywords(&files[0]);
        assert_eq!(k["category"], "rule34world");
        assert_eq!(k["playlist_id"], "259015");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "rule34world_5.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("rule34world/259015")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["p_{playlist_id}_{id}",["259015","5"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("POST /api/v2/post/search/playlist/259015 "));
        assert_eq!(
            body(&requests[0]).to_string(),
            r#"{"Skip":0,"take":60,"CountTotal":false,"IncludeLinks":true,"OrderBy":0}"#
        );
    }

    #[tokio::test]
    async fn search_without_items_is_an_error() {
        let (root, server) = serve(vec![json(json!({"cursor": "c"}))]);
        let http = http("rule34.xyz", root);
        assert!(
            fails(extract(
                &XYZ,
                &http,
                Url::parse("https://rule34.xyz/sfw").unwrap()
            ))
            .await
        );
        server.join().unwrap();
    }
}
