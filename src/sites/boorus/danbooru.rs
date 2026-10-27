//! Danbooru and e621 instances the dedicated `crate::booru` module does not
//! serve (ATFBooru, AIBooru, Booruvar, e6ai), mirroring
//! gallery_dl/extractor/danbooru.py and e621.py.
use crate::error;
use crate::event::ExtractionEvent;
use crate::sites::gdl::{
    auth_info, basic_auth, captures, date, int_of, nameext_into, parse_iso, parse_query, quote, re,
    unquote,
};
use crate::sites::{Events, Http, Item, Route, Site, scalar};
use serde_json::{Map, Value};
use std::time::Duration;
use url::Url;

const USER_AGENT: &str = concat!("gallery-dl-rs/", env!("CARGO_PKG_VERSION"));
const FILENAME: &str = "{category}_{id}_{filename}.{extension}";
const POOL_FILENAME: &str = "{num:>04}_{id}_{filename}.{extension}";
const DIR: &[&str] = &["{category}"];

static TAG: Route = Route::new(
    "tag",
    FILENAME,
    &["{category}", "{search_tags}"],
    "t_{search_tags}_{id}",
);
static RANDOM: Route = Route::new(
    "random",
    FILENAME,
    &["{category}", "{search_tags}"],
    "t_{search_tags}_{id}",
);
static POOL: Route = Route::new(
    "pool",
    POOL_FILENAME,
    &["{category}", "pool", "{pool[id]} {pool[name]}"],
    "p_{pool[id]}_{id}",
);
static FAVGROUP: Route = Route::new(
    "favgroup",
    POOL_FILENAME,
    &[
        "{category}",
        "Favorite Groups",
        "{favgroup[id]} {favgroup[name]}",
    ],
    "fg_{favgroup[id]}_{id}",
);
static POST: Route = Route::new("post", FILENAME, DIR, "{id}");
static MEDIA_ASSET: Route = Route::new(
    "media-asset",
    "{category}_ma{id}_{filename}.{extension}",
    DIR,
    "m{id}",
);
static POPULAR: Route = Route::new(
    "popular",
    FILENAME,
    &["{category}", "popular", "{scale}", "{date}"],
    "P_{scale[0]}_{date}_{id}",
);
static ARTIST: Route = Route::new("artist", FILENAME, DIR, "{id}");
static ARTIST_SEARCH: Route = Route::new("artist-search", FILENAME, DIR, "{id}");
static FAVORITE: Route = Route::new(
    "favorite",
    FILENAME,
    &["{category}", "Favorites", "{user_id}"],
    "f_{user_id}_{id}",
);
static ROUTES: &[&Route] = &[
    &TAG,
    &RANDOM,
    &POOL,
    &FAVGROUP,
    &POST,
    &MEDIA_ASSET,
    &POPULAR,
    &ARTIST,
    &ARTIST_SEARCH,
    &FAVORITE,
];

struct Instance {
    root: Option<&'static str>,
    hosts: &'static str,
    /// e621 dialect (`E621Extractor`) instead of Danbooru's.
    e621: bool,
}

macro_rules! instance {
    ($ident:ident, $instance:ident, $category:literal, $name:literal, $root:expr, $hosts:literal, $e621:literal) => {
        static $instance: Instance = Instance {
            root: $root,
            hosts: $hosts,
            e621: $e621,
        };
        pub(super) static $ident: Site = Site {
            category: $category,
            name: $name,
            matches: |url| target(&$instance, url).is_some(),
            extract: |http, url| extract(&$ident, &$instance, http, url),
            routes: ROUTES,
            api_key: false,
            cookies: None,
        };
    };
}

instance!(
    ATFBOORU,
    ATFBOORU_I,
    "atfbooru",
    "ATFBooru",
    Some("https://booru.allthefallen.moe"),
    r"booru\.allthefallen\.moe",
    false
);
instance!(
    AIBOORU,
    AIBOORU_I,
    "aibooru",
    "AIBooru",
    None,
    r"(?:safe\.|general\.)?aibooru\.(?:online|download)",
    false
);
instance!(
    BOORUVAR,
    BOORUVAR_I,
    "booruvar",
    "Booruvar",
    Some("https://booru.borvar.art"),
    r"booru\.borvar\.art",
    false
);
instance!(
    E6AI,
    E6AI_I,
    "e6ai",
    "e6ai",
    Some("https://e6ai.net"),
    r"e6ai\.net",
    true
);

#[derive(Debug, PartialEq)]
enum Kind {
    Tag(String),
    Random(String),
    Pool(String),
    Favgroup(String),
    Post(String),
    MediaAsset(String),
    Popular(String),
    Artist(String),
    ArtistSearch(String),
    Favorite(String),
}

/// The instance root and target of a URL.
fn target(instance: &Instance, url: &Url) -> Option<(String, Kind)> {
    let base = format!(r"^https?://({})", instance.hosts);
    let c = captures(re!(&format!("{base}(/.*)?$")), url)?;
    let root = instance
        .root
        .map_or_else(|| format!("https://{}", &c[1]), str::to_owned);
    let m = |pattern: &str| captures(re!(&format!("{base}{pattern}")), url);
    let last = |c: regex::Captures| c.get(c.len() - 1).map_or("", |g| g.as_str()).to_owned();
    let kind = if instance.e621 {
        if let Some(c) = m(r"/posts?(?:\?[^#]*?tags=|/index/\d+/)([^&#]*)") {
            Kind::Tag(last(c))
        } else if let Some(c) = m(r"/pool(?:s|/show)/(\d+)") {
            Kind::Pool(last(c))
        } else if let Some(c) = m(r"/p(?:ost(?:s|/show)/(\d+)|/(\w+))") {
            let id = match (c.get(2), c.get(3)) {
                (Some(id), _) => id.as_str().to_owned(),
                (None, Some(b32)) => i64::from_str_radix(b32.as_str(), 32).ok()?.to_string(),
                _ => return None,
            };
            Kind::Post(id)
        } else if let Some(c) = m(r"/explore/posts/popular(?:\?([^#]*))?") {
            Kind::Popular(last(c))
        } else if let Some(c) = m(r"/artists/(\d+)") {
            Kind::Artist(last(c))
        } else if let Some(c) = m(r"/artists/?\?([^#]+)") {
            Kind::ArtistSearch(last(c))
        } else {
            Kind::Favorite(last(m(r"/favorites(?:\?([^#]*))?")?))
        }
    } else if let Some(c) = m(r"/posts\?(?:[^&#]*&)*tags=([^&#]*)") {
        Kind::Tag(last(c))
    } else if let Some(c) = m(r"/posts/random(?:\?(?:[^&#]*&)*tags=([^&#]*))?") {
        Kind::Random(last(c))
    } else if let Some(c) = m(r"/pool(?:s|/show)/(\d+)") {
        Kind::Pool(last(c))
    } else if let Some(c) = m(r"/favorite_group(?:s|/show)/(\d+)") {
        Kind::Favgroup(last(c))
    } else if let Some(c) = m(r"/post(?:s|/show)/(\d+)") {
        Kind::Post(last(c))
    } else if let Some(c) = m(r"/media_assets/(\d+)") {
        Kind::MediaAsset(last(c))
    } else if let Some(c) = m(r"/(?:explore/posts/)?popular(?:\?([^#]*))?") {
        Kind::Popular(last(c))
    } else if let Some(c) = m(r"/artists/(\d+)") {
        Kind::Artist(last(c))
    } else {
        Kind::ArtistSearch(last(m(r"/artists/?\?([^#]+)")?))
    };
    Some((root, kind))
}

/// One API request with the gallery-dl agent and optional Basic auth.
async fn api(http: &Http, url: &str, params: &[(String, String)]) -> crate::Result<Value> {
    let mut url = http.api(url)?;
    if !params.is_empty() {
        url.query_pairs_mut().extend_pairs(params);
    }
    let auth = auth_info(http).map(|(user, key)| basic_auth(&user, &key));
    let mut headers = vec![("User-Agent", USER_AGENT)];
    if let Some(auth) = &auth {
        headers.push(("Authorization", auth.as_str()));
    }
    http.json_with(&url, &headers).await
}

fn set(params: &mut Vec<(String, String)>, key: &str, value: String) {
    match params.iter_mut().find(|(k, _)| k == key) {
        Some(entry) => entry.1 = value,
        None => params.push((key.to_owned(), value)),
    }
}

/// `_pagination`: page numbers, or `a<id>`/`b<id>` cursors with `prefix`.
fn pagination<'a, F>(
    http: &'a Http,
    url: String,
    mut params: Vec<(String, String)>,
    prefix: Option<&'static str>,
    per_page: i64,
    render: F,
) -> Events<'a>
where
    F: Fn(Vec<Value>) -> Vec<ExtractionEvent> + Send + Sync + 'a,
{
    let threshold = match http.option("threshold") {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(1).max(1),
        _ => per_page - 20,
    };
    set(&mut params, "limit", per_page.to_string());
    let render = std::sync::Arc::new(render);
    crate::sites::paginate(Some((params, true)), move |state| {
        let (url, render) = (url.clone(), render.clone());
        async move {
            let Some((mut params, first)) = state else {
                return Ok(None);
            };
            if !first {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            let mut data = api(http, &url, &params).await?;
            if data.is_object() {
                data = data["posts"].take();
            }
            let mut posts = data.as_array().cloned().unwrap_or_default();
            let count = posts.len() as i64;
            let last_id = posts.last().map(|p| scalar(&p["id"]));
            if prefix == Some("a") && !first {
                posts.reverse();
            }
            let events = render(posts);
            let next = (count >= threshold).then(|| {
                let page = match (prefix, last_id) {
                    (Some(prefix), Some(id)) => format!("{prefix}{id}"),
                    _ => {
                        let current = params
                            .iter()
                            .find(|(k, _)| k == "page")
                            .map(|(_, v)| v.as_str());
                        (current.map_or(1, |p| p.parse::<i64>().unwrap_or(1)) + 1).to_string()
                    }
                };
                set(&mut params, "page", page);
                (params, false)
            });
            Ok(Some((crate::sites::events(events), next)))
        }
    })
}

fn tag_list(value: Option<&Value>) -> Value {
    match value.and_then(Value::as_str).filter(|s| !s.is_empty()) {
        Some(tags) => tags.split(' ').map(Value::from).collect::<Vec<_>>().into(),
        None => Value::Array(Vec::new()),
    }
}

/// `DanbooruExtractor.items` / `E621Extractor.items` for one post.
fn post_item(root: &str, e621: bool, post: &Value, extra: &Map<String, Value>) -> Option<Item> {
    let mut post = post.as_object()?.clone();
    let raw = if e621 {
        let file = post.get("file")?.clone();
        let md5 = scalar(&file["md5"]);
        let ext = scalar(&file["ext"]);
        let url = match file
            .get("url")
            .and_then(Value::as_str)
            .filter(|u| !u.is_empty())
        {
            Some(url) => url.to_owned(),
            None => format!(
                "https://static1.{}/data/{}/{}/{md5}.{ext}",
                root.trim_start_matches("https://"),
                md5.get(0..2)?,
                md5.get(2..4)?
            ),
        };
        post.insert("filename".into(), md5.into());
        post.insert("extension".into(), ext.into());
        let mut tags: Vec<Value> = Vec::new();
        if let Some(groups) = post.get("tags").and_then(Value::as_object).cloned() {
            for (category, list) in groups {
                tags.extend(list.as_array().cloned().unwrap_or_default());
                post.insert(format!("tags_{category}"), list);
            }
        }
        tags.sort_by_key(scalar);
        post.insert("tags".into(), tags.into());
        url
    } else {
        // Posts without `file_url` are restricted (upstream skips them).
        let mut url = post.get("file_url")?.as_str()?.to_owned();
        nameext_into(&url, &mut post);
        post.insert("tags".into(), tag_list(post.get("tag_string")));
        for kind in ["artist", "character", "copyright", "general", "meta"] {
            let tags = tag_list(post.get(&format!("tag_string_{kind}")));
            post.insert(format!("tags_{kind}"), tags);
        }
        if post.get("extension").and_then(Value::as_str) == Some("zip") {
            url = post.get("large_file_url")?.as_str()?.to_owned();
            post.insert("extension".into(), "webm".into());
        }
        if url.starts_with("//") {
            url = format!("https:{url}");
        } else if url.starts_with('/') {
            url = format!("{root}{url}");
        }
        url
    };
    let created = post
        .get("created_at")
        .and_then(Value::as_str)
        .and_then(parse_iso);
    post.insert("date".into(), date(created));
    post.extend(extra.clone());
    let id = scalar(&post["id"]);
    Some(Item::new(Url::parse(&raw).ok()?, post).ids(id.as_str(), id.as_str()))
}

/// Each post as its own directory, as upstream's per-post `Directory`.
fn render_posts(
    site: &'static Site,
    http: &Http,
    route: &'static Route,
    root: &str,
    e621: bool,
    posts: Vec<Value>,
    extra: &Map<String, Value>,
) -> Vec<ExtractionEvent> {
    posts
        .iter()
        .filter_map(|post| post_item(root, e621, post, extra))
        .flat_map(|item| crate::sites::post(site, http, route, vec![item]))
        .collect()
}

/// `_collection_metadata`: the pool/favgroup with its post order.
async fn collection(
    http: &Http,
    root: &str,
    path: &str,
    id: &str,
) -> crate::Result<(Value, Vec<String>)> {
    let mut data = api(http, &format!("{root}/{path}s/{id}.json"), &[]).await?;
    let ids = data["post_ids"]
        .as_array()
        .map(|ids| ids.iter().map(scalar).collect())
        .unwrap_or_default();
    if let Some(map) = data.as_object_mut() {
        map.remove("post_ids");
        let name = map
            .get("name")
            .map(scalar)
            .unwrap_or_default()
            .replace('_', " ");
        map.insert("name".into(), name.into());
    }
    Ok((data, ids))
}

/// `DanbooruPopularExtractor.metadata`: `date` and `scale` keywords.
fn popular_metadata(params: &Map<String, Value>) -> Map<String, Value> {
    let scale = params
        .get("scale")
        .map(scalar)
        .unwrap_or_else(|| "day".into());
    let today = time::OffsetDateTime::now_utc().date();
    let mut day = params
        .get("date")
        .map(scalar)
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| today.to_string());
    if scale == "week" {
        let format = time::macros::format_description!("[year]-[month]-[day]");
        if let Ok(parsed) = time::Date::parse(&day, &format) {
            let monday = parsed
                - time::Duration::days(i64::from(parsed.weekday().number_days_from_monday()));
            day = monday.to_string();
        }
    } else if scale == "month" {
        day.truncate(day.len().saturating_sub(3));
    }
    let mut data = Map::new();
    data.insert("date".into(), day.into());
    data.insert("scale".into(), scale.into());
    data
}

fn extract<'a>(
    site: &'static Site,
    instance: &'static Instance,
    http: &'a Http,
    url: Url,
) -> Events<'a> {
    let Some((root, kind)) = target(instance, &url) else {
        return crate::sites::fail(error("unsupported Danbooru URL"));
    };
    let e621 = instance.e621;
    let per_page = if e621 { 320 } else { 200 };
    let posts_url = format!("{root}/posts.json");
    let list_root = root.clone();
    let list =
        move |route: &'static Route, url: String, params, prefix, extra: Map<String, Value>| {
            let root = list_root.clone();
            pagination(http, url, params, prefix, per_page, move |posts| {
                render_posts(site, http, route, &root, e621, posts, &extra)
            })
        };
    match kind {
        Kind::Tag(raw) => {
            let tags = unquote(&raw.replace('+', " "));
            let mut prefix = Some("b");
            for tag in tags.split_whitespace() {
                if let Some(order) = tag.strip_prefix("order:") {
                    prefix = match order {
                        "id" | "id_asc" => Some("a"),
                        "id_desc" => Some("b"),
                        _ => None,
                    };
                } else if ["id:", "md5:", "ordfav:", "ordfavgroup:", "ordpool:"]
                    .iter()
                    .any(|p| tag.starts_with(p))
                {
                    prefix = None;
                    break;
                }
            }
            let mut extra = Map::new();
            extra.insert("search_tags".into(), tags.as_str().into());
            list(&TAG, posts_url, vec![("tags".into(), tags)], prefix, extra)
        }
        Kind::Random(raw) => crate::sites::once(async move {
            let tags = unquote(&raw.replace('+', " "));
            let params: Vec<(String, String)> = if tags.is_empty() {
                Vec::new()
            } else {
                vec![("tags".into(), tags.clone())]
            };
            let data = api(http, &format!("{root}/posts/random.json"), &params).await?;
            let posts = if data.is_object() {
                vec![data]
            } else {
                data.as_array().cloned().unwrap_or_default()
            };
            let mut extra = Map::new();
            extra.insert("search_tags".into(), tags.into());
            Ok(crate::sites::events(render_posts(
                site, http, &RANDOM, &root, e621, posts, &extra,
            )))
        }),
        Kind::Pool(id) | Kind::Favgroup(id) => {
            let (route, key, path) = if matches!(route_kind(&url, e621), Some(true)) {
                (&FAVGROUP, "favgroup", "favorite_group")
            } else {
                (&POOL, "pool", "pool")
            };
            crate::sites::once(async move {
                let (meta, ids) = collection(http, &root, path, &id).await?;
                let mut extra = Map::new();
                extra.insert(key.into(), meta);
                let numbered = move |posts: Vec<Value>| -> Vec<Value> {
                    posts
                        .into_iter()
                        .filter_map(|mut post| {
                            let pid = scalar(&post["id"]);
                            let num = ids.iter().position(|i| *i == pid)? + 1;
                            post.as_object_mut()?.insert("num".into(), num.into());
                            Some(post)
                        })
                        .collect()
                };
                if e621 {
                    // `E621PoolExtractor.posts`: everything, in pool order.
                    let mut all = Vec::new();
                    let mut params = vec![
                        ("tags".to_owned(), format!("pool:{id}")),
                        ("limit".into(), "320".into()),
                    ];
                    loop {
                        let data = api(http, &format!("{root}/posts.json"), &params).await?;
                        let page = data["posts"].as_array().cloned().unwrap_or_default();
                        let Some(last) = page.last().map(|p| scalar(&p["id"])) else {
                            break;
                        };
                        let done = (page.len() as i64) < 300;
                        all.extend(page);
                        if done {
                            break;
                        }
                        set(&mut params, "page", format!("b{last}"));
                    }
                    let mut posts = numbered(all);
                    posts.sort_by_key(|p| int_of(p.get("num")));
                    return Ok(crate::sites::events(render_posts(
                        site, http, route, &root, e621, posts, &extra,
                    )));
                }
                // `_collection_posts` with the `order-posts` option.
                let order = http.option_str("order-posts").unwrap_or_default();
                let (ordered, prefix, reverse) = match order {
                    "id" | "desc_id" | "id_desc" => (false, Some("b"), false),
                    "desc" | "desc_pool" | "pool_desc" => (true, None, true),
                    "asc_id" | "id_asc" => (false, None, true),
                    _ => (true, None, false),
                };
                let tag = if ordered {
                    format!("ord{key}")
                } else {
                    key.to_owned()
                };
                let params = vec![("tags".to_owned(), format!("{tag}:{id}"))];
                if !reverse {
                    return Ok(list(
                        route,
                        format!("{root}/posts.json"),
                        params,
                        prefix,
                        extra,
                    ));
                }
                let mut all = Vec::new();
                let mut params = params;
                set(&mut params, "limit", "200".into());
                for page in 1..=1000 {
                    set(&mut params, "page", page.to_string());
                    let data = api(http, &format!("{root}/posts.json"), &params).await?;
                    let batch = data.as_array().cloned().unwrap_or_default();
                    let done = batch.len() < 180;
                    all.extend(batch);
                    if done {
                        break;
                    }
                }
                all.reverse();
                Ok(crate::sites::events(render_posts(
                    site,
                    http,
                    route,
                    &root,
                    e621,
                    numbered(all),
                    &extra,
                )))
            })
        }
        Kind::Post(id) => crate::sites::once(async move {
            let mut post = api(http, &format!("{root}/posts/{id}.json"), &[]).await?;
            if e621 {
                post = post["post"].take();
            }
            Ok(crate::sites::events(render_posts(
                site,
                http,
                &POST,
                &root,
                e621,
                vec![post],
                &Map::new(),
            )))
        }),
        Kind::MediaAsset(id) => crate::sites::once(async move {
            let mut asset = api(http, &format!("{root}/media_assets/{id}.json"), &[]).await?;
            let url = asset["variants"]
                .as_array()
                .and_then(|v| v.last())
                .map(|v| v["url"].clone());
            if let Some(map) = asset.as_object_mut() {
                map.insert("file_url".into(), url.unwrap_or(Value::Null));
                for key in [
                    "tag_string",
                    "tag_string_artist",
                    "tag_string_character",
                    "tag_string_copyright",
                    "tag_string_general",
                    "tag_string_meta",
                ] {
                    map.insert(key.into(), "".into());
                }
            }
            Ok(crate::sites::events(render_posts(
                site,
                http,
                &MEDIA_ASSET,
                &root,
                e621,
                vec![asset],
                &Map::new(),
            )))
        }),
        Kind::Popular(query) => {
            let params = parse_query(&query);
            let extra = popular_metadata(&params);
            let endpoint = if e621 {
                "/popular.json"
            } else {
                "/explore/posts/popular.json"
            };
            let params = params.into_iter().map(|(k, v)| (k, scalar(&v))).collect();
            list(&POPULAR, format!("{root}{endpoint}"), params, None, extra)
        }
        Kind::Favorite(query) => {
            let params = parse_query(&query);
            let mut extra = Map::new();
            extra.insert(
                "user_id".into(),
                params.get("user_id").cloned().unwrap_or_else(|| "".into()),
            );
            let params = params.into_iter().map(|(k, v)| (k, scalar(&v))).collect();
            list(
                &FAVORITE,
                format!("{root}/favorites.json"),
                params,
                None,
                extra,
            )
        }
        Kind::Artist(id) => crate::sites::once(async move {
            let artist = api(http, &format!("{root}/artists/{id}.json"), &[]).await?;
            Ok(queue_artists(http, site, &ARTIST, &root, vec![artist]))
        }),
        Kind::ArtistSearch(query) => {
            let mut params: Vec<(String, String)> = parse_query(&query)
                .into_iter()
                .map(|(k, v)| (k, scalar(&v)))
                .collect();
            let page = params
                .iter()
                .find(|(k, _)| k == "page")
                .map_or(1, |(_, v)| v.parse().unwrap_or(1).max(1));
            crate::sites::paginate(Some(page), move |state| {
                let root = root.clone();
                set(&mut params, "page", state.unwrap_or(1).to_string());
                let params = params.clone();
                async move {
                    let Some(page) = state else { return Ok(None) };
                    let data = api(http, &format!("{root}/artists.json"), &params).await?;
                    let artists = data.as_array().cloned().unwrap_or_default();
                    let next = (artists.len() >= 20).then_some(page + 1);
                    Ok(Some((
                        queue_artists(http, site, &ARTIST_SEARCH, &root, artists),
                        next,
                    )))
                }
            })
        }
    }
}

/// Favorite groups share `Kind::Pool`'s handling; tell them apart by path.
fn route_kind(url: &Url, e621: bool) -> Option<bool> {
    (!e621).then(|| url.path().starts_with("/favorite_group"))
}

/// `items_artists`: queue each artist's tag search.
fn queue_artists<'a>(
    http: &'a Http,
    site: &'static Site,
    route: &'static Route,
    root: &str,
    artists: Vec<Value>,
) -> Events<'a> {
    let children = artists
        .into_iter()
        .filter_map(|artist| {
            let url = format!("{root}/posts?tags={}", quote(&scalar(&artist["name"])));
            Some((Url::parse(&url).ok()?, artist.as_object()?.clone()))
        })
        .collect();
    crate::sites::galleries::queue(http, site, route, children)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{children, http as client, json as body, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |i: &Instance, raw: &str| target(i, &Url::parse(raw).unwrap());
        assert_eq!(
            t(
                &AIBOORU_I,
                "https://safe.aibooru.online/posts?tags=center_frills"
            ),
            Some((
                "https://safe.aibooru.online".into(),
                Kind::Tag("center_frills".into())
            ))
        );
        assert_eq!(
            t(&ATFBOORU_I, "https://booru.allthefallen.moe/pools/9"),
            Some((
                "https://booru.allthefallen.moe".into(),
                Kind::Pool("9".into())
            ))
        );
        assert_eq!(
            t(&BOORUVAR_I, "https://booru.borvar.art/favorite_groups/2").map(|t| t.1),
            Some(Kind::Favgroup("2".into()))
        );
        assert_eq!(
            t(&E6AI_I, "https://e6ai.net/p/a").map(|t| t.1),
            Some(Kind::Post("10".into()))
        );
        assert_eq!(
            t(&E6AI_I, "https://e6ai.net/posts?tags=anry").map(|t| t.1),
            Some(Kind::Tag("anry".into()))
        );
        assert_eq!(t(&E6AI_I, "https://e621.net/posts/1"), None);
    }

    #[tokio::test]
    async fn danbooru_tag_search_paginates_with_cursor() {
        let post = |id: i64| {
            json!({"id": id, "file_url": format!("https://cdn.aibooru.download/original/{id}.png"),
            "created_at": "2023-01-01T00:00:00.000-05:00", "tag_string": "a b", "tag_string_artist": "a",
            "tag_string_character": "", "tag_string_copyright": "", "tag_string_general": "b", "tag_string_meta": ""})
        };
        let page: Vec<Value> = (0..180).map(|i| post(1000 - i)).collect();
        let (root, server) = serve(vec![body(Value::Array(page)), body(json!([post(5)]))]);
        let http = client("aibooru", root);
        let files = media(extract(
            &AIBOORU,
            &AIBOORU_I,
            &http,
            Url::parse("https://aibooru.online/posts?tags=a+b").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 181);
        let k = keywords(&files[0]);
        assert_eq!(
            (k["search_tags"].as_str(), k["tags_artist"][0].as_str()),
            (Some("a b"), Some("a"))
        );
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "aibooru_1000_1000.png");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            crate::sites::testing::os_dir("aibooru/a b")
        );
        let requests = server.join().unwrap();
        assert!(requests[1].contains("page=b821"));
    }

    #[tokio::test]
    async fn e621_post_and_artist() {
        let post = json!({"post": {"id": 10, "created_at": "2024-01-01T00:00:00.000-05:00",
            "file": {"url": null, "md5": "0123456789abcdef0123456789abcdef", "ext": "png"},
            "tags": {"general": ["z", "a"], "artist": ["m"]}}});
        let (root, _server) = serve(vec![body(post)]);
        let http = client("e6ai", root);
        let files = media(extract(
            &E6AI,
            &E6AI_I,
            &http,
            Url::parse("https://e6ai.net/posts/10").unwrap(),
        ))
        .await;
        assert_eq!(
            files[0].url.as_str(),
            "https://static1.e6ai.net/data/01/23/0123456789abcdef0123456789abcdef.png"
        );
        assert_eq!(keywords(&files[0])["tags"], json!(["a", "m", "z"]));

        let (root, _server) = serve(vec![body(json!({"id": 3, "name": "some artist"}))]);
        let http = client("e6ai", root);
        let queued = children(extract(
            &E6AI,
            &E6AI_I,
            &http,
            Url::parse("https://e6ai.net/artists/3").unwrap(),
        ))
        .await;
        assert_eq!(queued.len(), 1);
    }
}
