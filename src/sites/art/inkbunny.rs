//! Inkbunny galleries, pools, favorites, searches, watchlists and
//! submissions, mirroring gallery_dl/extractor/inkbunny.py through the
//! `api_*.php` JSON API with a guest session.
//!
//! Only the guest login is supported; upstream's `username`/`password`
//! login is not, so unread submissions and private files stay out of reach.
use super::{date, extr, extract_iter, leading_digits, nameext_into, on_host, parse_int};
use super::{parse_iso, parse_query, str_of};
use crate::event::ExtractionEvent;
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use futures_util::{StreamExt, stream};
use serde_json::{Map, Value};
use std::sync::{Arc, Mutex};
use url::Url;

const ROOT: &str = "https://inkbunny.net";

pub static SITE: Site = Site {
    category: "inkbunny",
    name: "Inkbunny",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[
        &USER, &GALLERY, &SCRAPS, &POOL, &FAVORITE, &UNREAD, &SEARCH, &FOLLOWING, &POST,
    ],
    api_key: false,
    cookies: None,
};

const FILENAME: &str = "{submission_id} {file_id} {title}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{username!l}"];

// `InkbunnyUserExtractor` switches its subcategory to `gallery`/`scraps`.
static USER: Route = Route::new("user", FILENAME, DIRECTORY, "{file_id}");
static GALLERY: Route = Route::new("gallery", FILENAME, DIRECTORY, "{file_id}");
static SCRAPS: Route = Route::new("scraps", FILENAME, DIRECTORY, "{file_id}");
static POOL: Route = Route::new("pool", FILENAME, DIRECTORY, "{file_id}");
static FAVORITE: Route = Route::new(
    "favorite",
    FILENAME,
    &["{category}", "{favs_username!l}", "Favorites"],
    "{file_id}",
);
static UNREAD: Route = Route::new("unread", FILENAME, DIRECTORY, "{file_id}");
static SEARCH: Route = Route::new("search", FILENAME, DIRECTORY, "{file_id}");
static FOLLOWING: Route = Route::new("following", FILENAME, DIRECTORY, "{file_id}");
static POST: Route = Route::new("post", FILENAME, DIRECTORY, "{file_id}");

type Query = Vec<(String, String)>;

#[derive(Debug, PartialEq)]
enum Target {
    /// Subcategory, user name and upstream `scraps` parameter.
    User(&'static str, String, Option<&'static str>),
    /// Pool ID and `orderby`.
    Pool(Option<String>, String),
    /// User ID and `orderby`; `None` reads the `orderby` option.
    Favorite(Option<String>, Option<String>),
    Unread(Query),
    Search(Query),
    Following(Option<String>),
    Post(String),
}

/// Whether a raw query has `mode=NAME…` at its start or after an `&`, as
/// upstream `(?:[^#]+&)?mode=NAME`.
fn has_mode(query: &str, name: &str) -> bool {
    let needle = format!("mode={name}");
    query.starts_with(&needle)
        || query
            .match_indices(&format!("&{needle}"))
            .any(|(i, _)| i > 0)
}

fn query_value(query: &str, key: &str) -> Option<String> {
    parse_query(query)
        .into_iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v)
}

/// Upstream `\w+` followed by the end of the path or a `/`.
fn word(text: &str) -> Option<&str> {
    let len = text
        .bytes()
        .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_')
        .count();
    (len > 0 && matches!(text.as_bytes().get(len), None | Some(b'/'))).then(|| &text[..len])
}

fn target(url: &Url) -> Option<Target> {
    if !on_host(url, &["inkbunny.net"]) {
        return None;
    }
    let query = url.query().unwrap_or_default();
    let digits = |prefix: &str| {
        query
            .strip_prefix(prefix)
            .and_then(leading_digits)
            .map(str::to_owned)
    };
    match url.path() {
        "/poolview_process.php" => {
            return Some(Target::Pool(Some(digits("pool_id=")?), "pool_order".into()));
        }
        "/userfavorites_process.php" => {
            return Some(Target::Favorite(Some(digits("favs_user_id=")?), None));
        }
        "/watchlist_process.php" => {
            return Some(Target::Following(Some(digits("mode=watching&user_id=")?)));
        }
        "/usersviewall.php" if has_mode(query, "watching") => {
            return Some(Target::Following(query_value(query, "user_id")));
        }
        "/submissionsviewall.php" => {
            let orderby =
                |default: &str| query_value(query, "orderby").unwrap_or_else(|| default.to_owned());
            return if has_mode(query, "pool") {
                Some(Target::Pool(
                    query_value(query, "pool_id"),
                    orderby("pool_order"),
                ))
            } else if has_mode(query, "userfavs") {
                Some(Target::Favorite(
                    query_value(query, "user_id"),
                    Some(orderby("fav_datetime")),
                ))
            } else if has_mode(query, "unreadsubs") {
                Some(Target::Unread(parse_query(query)))
            } else if has_mode(query, "search") {
                Some(Target::Search(parse_query(query)))
            } else {
                None
            };
        }
        _ => {}
    }
    let path = url.path().strip_prefix('/')?;
    if let Some(id) = path.strip_prefix("s/") {
        return Some(Target::Post(leading_digits(id)?.to_owned()));
    }
    for (kind, scraps) in [("gallery", "no"), ("scraps", "only")] {
        let rest = path.strip_prefix(kind).and_then(|p| p.strip_prefix('/'));
        if let Some(user) = rest.and_then(word) {
            return Some(Target::User(kind, user.to_owned(), Some(scraps)));
        }
    }
    Some(Target::User("user", word(path)?.to_owned(), None))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Inkbunny URL"));
    };
    crate::sites::once(async move {
        let api = Arc::new(Api {
            http,
            sid: Mutex::new(login(http).await?),
        });
        let orderby = http.option_str("orderby").map(str::to_owned);
        Ok(match target {
            Target::User(kind, user, scraps) => {
                let route = match kind {
                    "gallery" => &GALLERY,
                    "scraps" => &SCRAPS,
                    _ => &USER,
                };
                let mut params = vec![
                    param("username", Some(user)),
                    param("scraps", scraps.map(str::to_owned)),
                    param("orderby", orderby.clone()),
                ];
                if orderby.as_deref().is_some_and(|o| o.starts_with("unread_")) {
                    params.push(param("unread_submissions", Some("yes".into())));
                }
                search(api, route, Map::new(), params)
            }
            Target::Pool(pool_id, orderby) => {
                let mut data = Map::new();
                data.insert("pool_id".into(), pool_id.clone().into());
                let params = vec![param("pool_id", pool_id), param("orderby", Some(orderby))];
                search(api, &POOL, data, params)
            }
            Target::Favorite(user_id, query_orderby) => {
                let orderby = query_orderby.or(orderby).unwrap_or("fav_datetime".into());
                let data = favorite_metadata(http, user_id.as_deref()).await?;
                let mut params = vec![
                    param("favs_user_id", user_id),
                    param("orderby", Some(orderby.clone())),
                ];
                if orderby.starts_with("unread_") {
                    params.push(param("unread_submissions", Some("yes".into())));
                }
                search(api, &FAVORITE, data, params)
            }
            Target::Unread(query) => {
                let mut params: Params = query
                    .into_iter()
                    .filter(|(k, _)| k != "rid" && k != "mode")
                    .map(|(k, v)| (k, Some(v)))
                    .collect();
                set(&mut params, "unread_submissions", Some("yes".into()));
                search(api, &UNREAD, Map::new(), params)
            }
            Target::Search(query) => {
                let mut data = Map::new();
                let fields: Map<String, Value> = query
                    .iter()
                    .map(|(k, v)| (k.clone(), Value::from(v.as_str())))
                    .collect();
                data.insert("search".into(), fields.into());
                let params = search_params(http, query).await?;
                search(api, &SEARCH, data, params)
            }
            Target::Following(user_id) => following(http, user_id).await?,
            Target::Post(id) => {
                let posts = api.detail(std::slice::from_ref(&id)).await?;
                if posts.is_empty() {
                    return Err(error("Inkbunny submission not found"));
                }
                let sid = api.sid();
                crate::sites::events(post_events(http, &POST, &posts, &Map::new(), &sid))
            }
        })
    })
}

/// Request parameters in upstream dict order; `None` values are omitted.
type Params = Vec<(String, Option<String>)>;

fn param(key: &str, value: Option<String>) -> (String, Option<String>) {
    (key.to_owned(), value)
}

/// Python `params[key] = value`: replace in place, else append.
fn set(params: &mut Params, key: &str, value: Option<String>) {
    match params.iter_mut().find(|(k, _)| k == key) {
        Some(slot) => slot.1 = value,
        None => params.push(param(key, value)),
    }
}

/// Python `params.pop(key, None)`.
fn pop(params: &mut Params, key: &str) -> Option<String> {
    let index = params.iter().position(|(k, _)| k == key)?;
    params.remove(index).1
}

/// Upstream `InkbunnySearchExtractor.posts` parameter renames; a `favsby`
/// user name is resolved to its ID through the profile page.
async fn search_params(http: &Http, query: Query) -> Result<Params> {
    let mut params: Params = query.into_iter().map(|(k, v)| (k, Some(v))).collect();
    pop(&mut params, "rid");
    let join = pop(&mut params, "stringtype");
    set(&mut params, "string_join_type", join);
    let days = pop(&mut params, "days");
    set(&mut params, "dayslimit", days);
    let artist = pop(&mut params, "artist");
    set(&mut params, "username", artist);
    if let Some(favsby) = pop(&mut params, "favsby").filter(|s| !s.is_empty()) {
        let mut url = http.api(ROOT)?;
        url.path_segments_mut()
            .map_err(|_| error("invalid Inkbunny URL"))?
            .clear()
            .push(&favsby);
        let page = http.text(&url).await?;
        let user_id = extr(&page, "?user_id=", "'");
        let user_id = user_id.split('&').next().unwrap_or_default();
        set(&mut params, "favs_user_id", Some(user_id.to_owned()));
    }
    Ok(params)
}

/// Upstream `InkbunnyFavoriteExtractor.metadata`: the owner's user name
/// from the favorites page.
async fn favorite_metadata(http: &Http, user_id: Option<&str>) -> Result<Map<String, Value>> {
    // Python formats a missing ID as `None`.
    let id = user_id.unwrap_or("None");
    let mut url = http.api(&format!("{ROOT}/userfavorites_process.php"))?;
    url.query_pairs_mut().append_pair("favs_user_id", id);
    let page = http.text(&url).await?;
    let link = extr(&page, "<a rel=\"author\"", "</a>");
    let mut data = Map::new();
    data.insert("favs_user_id".into(), user_id.into());
    data.insert("favs_username".into(), extr(link, "href=\"/", "\"").into());
    Ok(data)
}

/// Upstream `_authenticate_impl` and `set_allowed_ratings` for the guest
/// account: log in, then allow every rating.
async fn login(http: &Http) -> Result<String> {
    let url = http.api(&format!("{ROOT}/api_login.php"))?;
    let data = http
        .post_form(&url, &[("username", "guest"), ("password", "")], &[])
        .await?;
    let sid = str_of(&data, "sid");
    if sid.is_empty() || sid.len() > 256 {
        return Err(error("Inkbunny guest login failed"));
    }
    let mut url = http.api(&format!("{ROOT}/api_userrating.php"))?;
    url.query_pairs_mut()
        .append_pair("tag[2]", "yes")
        .append_pair("tag[3]", "yes")
        .append_pair("tag[4]", "yes")
        .append_pair("tag[5]", "yes")
        .append_pair("sid", sid);
    let rating = http.json(&url).await?;
    if rating.get("error_code").is_some() {
        return Err(error("Inkbunny rejected the guest rating settings"));
    }
    Ok(sid.to_owned())
}

/// Upstream `InkbunnyAPI` with its session ID.
struct Api<'a> {
    http: &'a Http,
    sid: Mutex<String>,
}

impl Api<'_> {
    fn sid(&self) -> String {
        self.sid.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Upstream `_call`: an expired session (`error_code` 2) logs in again
    /// once; other API errors end extraction.
    async fn call(&self, endpoint: &str, params: &mut Params) -> Result<Value> {
        for attempt in 0..2 {
            set(params, "sid", Some(self.sid()));
            let mut url = self.http.api(&format!("{ROOT}/api_{endpoint}.php"))?;
            {
                let mut query = url.query_pairs_mut();
                for (key, value) in params.iter() {
                    if let Some(value) = value {
                        query.append_pair(key, value);
                    }
                }
            }
            let data = self.http.json(&url).await?;
            match data.get("error_code") {
                None => return Ok(data),
                Some(code) if crate::sites::scalar(code) == "2" && attempt == 0 => {
                    let sid = login(self.http).await?;
                    *self.sid.lock().unwrap_or_else(|e| e.into_inner()) = sid;
                }
                Some(_) => {
                    let message: String =
                        str_of(&data, "error_message").chars().take(200).collect();
                    return Err(error(format!("Inkbunny API error: {message}")));
                }
            }
        }
        Err(error("Inkbunny session expired"))
    }

    /// Upstream `detail`: full submissions in the order of `ids`; unknown
    /// IDs are left out.
    async fn detail(&self, ids: &[String]) -> Result<Vec<Value>> {
        let mut params = vec![
            param("submission_ids", Some(ids.join(","))),
            param("show_description", Some("yes".into())),
            param("show_pools", Some("yes".into())),
        ];
        let data = self.call("submissions", &mut params).await?;
        let found = data
            .get("submissions")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(ids
            .iter()
            .filter_map(|id| {
                found
                    .iter()
                    .find(|sub| {
                        sub.get("submission_id").map(crate::sites::scalar).as_ref() == Some(id)
                    })
                    .cloned()
            })
            .collect())
    }
}

/// A JSON number or numeric string as an integer.
fn int_of(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Number(n)) => n.as_i64().unwrap_or_default(),
        Some(Value::String(s)) => parse_int(s, 0),
        _ => 0,
    }
}

/// Upstream `_pagination_search`: submission IDs one page at a time, each
/// page resolved through `detail`; `rid` keeps later pages consistent.
fn search<'a>(
    api: Arc<Api<'a>>,
    route: &'static Route,
    metadata: Map<String, Value>,
    mut params: Params,
) -> Events<'a> {
    set(&mut params, "page", Some("1".into()));
    set(&mut params, "get_rid", Some("yes".into()));
    set(&mut params, "submission_ids_only", Some("yes".into()));
    crate::sites::paginate(Some((params, 1i64)), move |state| {
        let api = api.clone();
        let metadata = metadata.clone();
        async move {
            let Some((mut params, page)) = state else {
                return Ok(None);
            };
            let data = api.call("search", &mut params).await?;
            let ids: Vec<String> = data
                .get("submissions")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|sub| sub.get("submission_id"))
                .map(crate::sites::scalar)
                .collect();
            if ids.is_empty() {
                return Ok(None);
            }
            let posts = api.detail(&ids).await?;
            let events = post_events(api.http, route, &posts, &metadata, &api.sid());
            let next = if int_of(data.get("page")) >= int_of(data.get("pages_count")) {
                None
            } else {
                if pop(&mut params, "get_rid").is_some() {
                    set(&mut params, "rid", Some(str_of(&data, "rid").to_owned()));
                }
                set(&mut params, "page", Some((page + 1).to_string()));
                Some((params, page + 1))
            };
            Ok(Some((crate::sites::events(events), next)))
        }
    })
}

fn post_events(
    http: &Http,
    route: &'static Route,
    posts: &[Value],
    metadata: &Map<String, Value>,
    sid: &str,
) -> Vec<ExtractionEvent> {
    posts
        .iter()
        .flat_map(|post| crate::sites::post(&SITE, http, route, parse_post(post, metadata, sid)))
        .collect()
}

/// Upstream `items` for one submission: boolean flags, tag and rating
/// names, then one file per entry of `files` with its own date.
pub(crate) fn parse_post(post: &Value, metadata: &Map<String, Value>, sid: &str) -> Vec<Item> {
    const TO_BOOL: [&str; 7] = [
        "deleted",
        "favorite",
        "friends_only",
        "guest_block",
        "hidden",
        "public",
        "scraps",
    ];
    let Some(fields) = post.as_object() else {
        return Vec::new();
    };
    let mut data = fields.clone();
    super::update(&mut data, metadata);
    let created = |value: &Value| {
        let text: String = str_of(value, "create_datetime").chars().take(19).collect();
        parse_iso(&text)
    };
    data.insert("date".into(), date(created(post)));
    let names = |key: &str, name: &str| -> Value {
        post.get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|v| v.get(name).cloned().unwrap_or(Value::Null))
            .collect::<Vec<_>>()
            .into()
    };
    data.insert("tags".into(), names("keywords", "keyword_name"));
    data.insert("ratings".into(), names("ratings", "name"));
    for key in TO_BOOL {
        if let Some(value) = data.get_mut(key) {
            *value = (value.as_str() == Some("t")).into();
        }
    }
    data.remove("keywords");
    let files = match data.remove("files") {
        Some(Value::Array(files)) => files,
        _ => Vec::new(),
    };
    let id = str_of(post, "submission_id").to_owned();
    let user = str_of(post, "username").to_owned();
    let title = str_of(post, "title").to_owned();
    let mut items = Vec::new();
    for (num, file) in files.iter().enumerate() {
        let Some(entries) = file.as_object() else {
            continue;
        };
        super::update(&mut data, entries);
        data.insert("num".into(), (num + 1).into());
        data.insert(
            "deleted".into(),
            (file.get("deleted").and_then(Value::as_str) == Some("t")).into(),
        );
        let timestamp = created(file);
        data.insert("date".into(), date(timestamp));
        nameext_into(str_of(file, "file_name"), &mut data);
        let mut raw = str_of(file, "file_url_full").to_owned();
        if raw.contains("/private_files/") {
            raw = format!("{raw}?sid={sid}");
        }
        let Some(url) = crate::sites::media_url(&raw, &["inkbunny.net", "ib.metapix.net"]) else {
            continue;
        };
        let file_id = file
            .get("file_id")
            .map(crate::sites::scalar)
            .unwrap_or_default();
        items.push(
            Item::new(url, data.clone())
                .ids(id.clone(), file_id)
                .user(Some(user.clone()))
                .caption(Some(title.clone()))
                .timestamp(timestamp),
        );
    }
    items
}

/// Upstream `InkbunnyFollowingExtractor`: the watchlist redirects to a
/// paged user listing whose entries are queued as user profiles.
async fn following(http: &Http, user_id: Option<String>) -> Result<Events<'_>> {
    let mut url = http.api(&format!("{ROOT}/watchlist_process.php"))?;
    url.query_pairs_mut()
        .append_pair("mode", "watching")
        .append_pair("user_id", user_id.as_deref().unwrap_or("None"));
    let listing = match http.location(&url, &[]).await? {
        Some(next) if on_host(&next, &["inkbunny.net"]) || next.origin() == url.origin() => {
            http.api(next.as_str())?
        }
        Some(_) => return Err(error("Inkbunny watchlist redirected off-site")),
        None => url,
    };
    let page = http.text(&listing).await?;
    let mut params: Params = parse_query(listing.query().unwrap_or_default())
        .into_iter()
        .map(|(k, v)| (k, Some(v)))
        .collect();
    let number = params
        .iter()
        .find(|(k, _)| k == "page")
        .and_then(|(_, v)| v.as_deref())
        .map_or(1, |v| parse_int(v, 1));
    set(&mut params, "page", Some(number.to_string()));
    let mut base = listing;
    base.set_query(None);
    Ok(crate::sites::paginate(
        Some((Some(page), params, number)),
        move |state| {
            let base = base.clone();
            async move {
                let Some((page, mut params, number)) = state else {
                    return Ok(None);
                };
                let page = match page {
                    Some(page) => page,
                    None => {
                        let mut url = base;
                        {
                            let mut query = url.query_pairs_mut();
                            for (key, value) in &params {
                                query.append_pair(key, value.as_deref().unwrap_or_default());
                            }
                        }
                        http.text(&url).await?
                    }
                };
                let start = page
                    .find("id=\"changethumboriginal_form\"")
                    .ok_or_else(|| error("Inkbunny watchlist page has no user list"))?;
                let children: Vec<Url> = extract_iter(
                    &page[start..],
                    "<a class=\"widget_userNameSmall\" href=\"",
                    "\"",
                )
                .into_iter()
                .filter_map(|path| Url::parse(&format!("{ROOT}{path}")).ok())
                .filter(|child| matches!(target(child), Some(Target::User(..))))
                .collect();
                let events = stream::iter(children)
                    .map(move |child| {
                        crate::sites::queue(http, "inkbunny", "following", child, &Map::new())
                    })
                    .flatten()
                    .boxed();
                let next = page.contains("<a title='next page' ").then(|| {
                    set(&mut params, "page", Some((number + 1).to_string()));
                    (None, params, number + 1)
                });
                Ok(Some((events, next)))
            }
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, json, keywords, media, queued, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://inkbunny.net/soothe"),
            Some(Target::User("user", "soothe".into(), None))
        );
        assert_eq!(
            t("https://inkbunny.net/gallery/soothe"),
            Some(Target::User("gallery", "soothe".into(), Some("no")))
        );
        assert_eq!(
            t("https://www.inkbunny.net/scraps/soothe/"),
            Some(Target::User("scraps", "soothe".into(), Some("only")))
        );
        assert_eq!(
            t("https://inkbunny.net/gallery"),
            Some(Target::User("user", "gallery".into(), None))
        );
        assert_eq!(
            t("https://inkbunny.net/s/1829715-p2"),
            Some(Target::Post("1829715".into()))
        );
        assert_eq!(
            t("https://inkbunny.net/poolview_process.php?pool_id=28985"),
            Some(Target::Pool(Some("28985".into()), "pool_order".into()))
        );
        assert_eq!(
            t(
                "https://inkbunny.net/submissionsviewall.php?rid=ffffffffff&mode=pool&pool_id=28985&page=1&orderby=pool_order&random=no"
            ),
            Some(Target::Pool(Some("28985".into()), "pool_order".into()))
        );
        assert_eq!(
            t("https://inkbunny.net/userfavorites_process.php?favs_user_id=20969"),
            Some(Target::Favorite(Some("20969".into()), None))
        );
        assert_eq!(
            t("https://inkbunny.net/submissionsviewall.php?mode=userfavs&user_id=20969&orderby=x"),
            Some(Target::Favorite(Some("20969".into()), Some("x".into())))
        );
        assert!(matches!(
            t("https://inkbunny.net/submissionsviewall.php?text=&mode=unreadsubs&type="),
            Some(Target::Unread(_))
        ));
        assert!(matches!(
            t("https://inkbunny.net/submissionsviewall.php?text=cute&mode=search&type="),
            Some(Target::Search(_))
        ));
        assert_eq!(
            t("https://inkbunny.net/watchlist_process.php?mode=watching&user_id=20969"),
            Some(Target::Following(Some("20969".into())))
        );
        assert_eq!(
            t("https://inkbunny.net/usersviewall.php?rid=x&mode=watching&page=1&user_id=7"),
            Some(Target::Following(Some("7".into())))
        );
        for raw in [
            "https://inkbunny.net/",
            "https://inkbunny.net/s/abc",
            "https://inkbunny.net/user.php",
            "https://inkbunny.net/poolview_process.php?x=1",
            "https://inkbunny.net/submissionsviewall.php?mode=other",
            "https://inkbunny.net/submissionsviewall.php?xmode=pool",
            "https://inkbunny.net.evil.com/soothe",
            "https://evilinkbunny.net/soothe",
            "https://a.inkbunny.net/soothe",
            "https://inkbunny.net:8443/soothe",
            "https://u:p@inkbunny.net/soothe",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn submission(id: u64) -> Value {
        json!({"submission_id": id.to_string(), "title": format!("Pic {id}"),
            "username": "Artist", "user_id": "9",
            "create_datetime": "2020-01-02 03:04:05.123456+00",
            "public": "t", "scraps": "f", "guest_block": "f",
            "keywords": [{"keyword_id": "1", "keyword_name": "fox"}],
            "ratings": [{"name": "Nudity"}],
            "files": [
                {"file_id": format!("{id}1"), "file_name": format!("{id}_artist_a.png"),
                 "file_url_full": format!("https://tx.ib.metapix.net/files/full/{id}_a.png"),
                 "create_datetime": "2020-01-03 00:00:00.5+00", "deleted": "f"},
                {"file_id": format!("{id}2"), "file_name": format!("{id}_artist_b.JPG"),
                 "file_url_full": format!("https://private.ib.metapix.net/private_files/full/{id}_b.jpg"),
                 "create_datetime": "2020-01-04 00:00:00.5+00", "deleted": "t"}]})
    }

    fn login_replies() -> Vec<Vec<u8>> {
        vec![
            json(json!({"sid": "SID", "user_id": 0})),
            json(json!({"sid": "SID"})),
        ]
    }

    #[tokio::test]
    async fn extracts_a_submission() {
        let mut replies = login_replies();
        replies.push(json(json!({"submissions": [submission(5)]})));
        let (root, server) = serve(replies);
        let http = http("inkbunny", root);
        let files = media(extract(
            &http,
            Url::parse("https://inkbunny.net/s/5").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[1]);
        assert_eq!(k["tags"], json!(["fox"]));
        assert_eq!(k["ratings"], json!(["Nudity"]));
        assert_eq!(k["public"], true);
        assert_eq!(k["scraps"], false);
        assert_eq!(k["deleted"], true);
        assert_eq!(k["num"], 2);
        assert_eq!(k["date"], "2020-01-04 00:00:00");
        assert_eq!(k["filename"], "5_artist_b");
        assert_eq!(k["extension"], "jpg");
        assert!(!k.contains_key("files") && !k.contains_key("keywords"));
        assert_eq!(
            files[1].url.as_str(),
            "https://private.ib.metapix.net/private_files/full/5_b.jpg?sid=SID"
        );
        assert_eq!(keywords(&files[0])["deleted"], false);
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "5 51 Pic 5.png");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("inkbunny/artist")
        );
        let (_, identity) = crate::sites::archive_key(&files[1]).unwrap();
        assert_eq!(identity, r#"["{file_id}",["52"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("POST /api_login.php "));
        assert!(requests[0].ends_with("username=guest&password="));
        assert!(
            requests[1].starts_with(
                "GET /api_userrating.php?tag%5B2%5D=yes&tag%5B3%5D=yes&tag%5B4%5D=yes\
                 &tag%5B5%5D=yes&sid=SID "
            ),
            "{}",
            requests[1]
        );
        assert!(
            requests[2].starts_with(
                "GET /api_submissions.php?submission_ids=5&show_description=yes\
                 &show_pools=yes&sid=SID "
            ),
            "{}",
            requests[2]
        );
    }

    #[tokio::test]
    async fn gallery_searches_page_by_page() {
        let mut replies = login_replies();
        replies.extend([
            json(json!({"submissions": [{"submission_id": "1"}], "page": 1,
                "pages_count": 2, "rid": "abc"})),
            json(json!({"submissions": [submission(1)]})),
            json(json!({"error_code": 2, "error_message": "Invalid Session ID"})),
            json(json!({"sid": "SID2"})),
            json(json!({"sid": "SID2"})),
            json(json!({"submissions": [{"submission_id": "2"}], "page": 2,
                "pages_count": 2})),
            json(json!({"submissions": [submission(2)]})),
        ]);
        let (root, server) = serve(replies);
        let http = http("inkbunny", root);
        let files = media(extract(
            &http,
            Url::parse("https://inkbunny.net/gallery/Artist").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 4);
        assert_eq!(keywords(&files[0])["subcategory"], "gallery");
        assert_eq!(
            files[3].url.as_str(),
            "https://private.ib.metapix.net/private_files/full/2_b.jpg?sid=SID2"
        );
        let requests = server.join().unwrap();
        assert!(
            requests[2].starts_with(
                "GET /api_search.php?username=Artist&scraps=no&page=1&get_rid=yes\
                 &submission_ids_only=yes&sid=SID "
            ),
            "{}",
            requests[2]
        );
        assert!(
            requests[4].starts_with(
                "GET /api_search.php?username=Artist&scraps=no&page=2\
                 &submission_ids_only=yes&sid=SID&rid=abc "
            ),
            "{}",
            requests[4]
        );
        assert!(requests[5].starts_with("POST /api_login.php "));
        assert!(
            requests[7].contains("&sid=SID2&rid=abc "),
            "{}",
            requests[7]
        );
    }

    #[tokio::test]
    async fn favorites_resolve_the_owner_name() {
        let mut replies = login_replies();
        replies.extend([
            html("<div><a rel=\"author\" href=\"/Owner\">Owner</a></div>"),
            json(json!({"submissions": [{"submission_id": "3"}], "page": "1",
                "pages_count": "1", "rid": "r"})),
            json(json!({"submissions": [submission(3)]})),
        ]);
        let (root, server) = serve(replies);
        let http = http("inkbunny", root);
        let files = media(extract(
            &http,
            Url::parse("https://inkbunny.net/userfavorites_process.php?favs_user_id=20").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[0]);
        assert_eq!(k["favs_username"], "Owner");
        assert_eq!(k["favs_user_id"], "20");
        let (_, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("inkbunny/owner/Favorites")
        );
        let requests = server.join().unwrap();
        assert!(requests[2].starts_with("GET /userfavorites_process.php?favs_user_id=20 "));
        assert!(
            requests[3]
                .starts_with("GET /api_search.php?favs_user_id=20&orderby=fav_datetime&page=1"),
            "{}",
            requests[3]
        );
    }

    #[tokio::test]
    async fn search_renames_parameters() {
        let mut replies = login_replies();
        replies.push(json(json!({"submissions": []})));
        let (root, server) = serve(replies);
        let http = http("inkbunny", root);
        let files = media(extract(
            &http,
            Url::parse(
                "https://inkbunny.net/submissionsviewall.php?rid=1&mode=search&text=red+fox&stringtype=and&days=7&artist=Bob",
            )
            .unwrap(),
        ))
        .await;
        assert!(files.is_empty());
        let requests = server.join().unwrap();
        assert!(
            requests[2].starts_with(
                "GET /api_search.php?mode=search&text=red+fox&string_join_type=and\
                 &dayslimit=7&username=Bob&page=1&get_rid=yes&submission_ids_only=yes&sid=SID "
            ),
            "{}",
            requests[2]
        );
    }

    #[tokio::test]
    async fn following_queues_watched_users() {
        let mut replies = login_replies();
        let redirect = b"HTTP/1.1 302 Found\r\nLocation: https://inkbunny.net/usersviewall.php?rid=r1&mode=watching&page=1&user_id=7\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec();
        replies.extend([
            redirect,
            html(
                "<a class=\"widget_userNameSmall\" href=\"/Before\"></a>\
                <form id=\"changethumboriginal_form\"></form>\
                <a class=\"widget_userNameSmall\" href=\"/Alice\"></a>\
                <a title='next page' href='x'>",
            ),
            html(
                "<form id=\"changethumboriginal_form\"></form>\
                <a class=\"widget_userNameSmall\" href=\"/Bob\"></a>",
            ),
        ]);
        let (root, server) = serve(replies);
        let http = http("inkbunny", root);
        let children = queued(extract(
            &http,
            Url::parse("https://inkbunny.net/watchlist_process.php?mode=watching&user_id=7")
                .unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].subcategory(), "following");
        let requests = server.join().unwrap();
        assert!(requests[2].starts_with("GET /watchlist_process.php?mode=watching&user_id=7 "));
        assert!(
            requests[3].starts_with("GET /usersviewall.php?rid=r1&mode=watching&page=1&user_id=7 ")
        );
        assert!(
            requests[4].starts_with("GET /usersviewall.php?rid=r1&mode=watching&page=2&user_id=7 "),
            "{}",
            requests[4]
        );
    }
}
