//! Fansly posts, home timelines, lists and creator walls, mirroring
//! gallery_dl/extractor/fansly.py through the `apiv3.fansly.com` API.
//!
//! Access is the account owner's own: the `token` option is sent as the
//! API's `authorization` header, as upstream does; without it only
//! public content is listed. Each attachment's widest variant (filtered
//! by the `formats` option) is downloaded, or its preview when `previews`
//! is on (the default). HLS/DASH variants go to yt-dlp with their
//! CloudFront cookies; upstream's `_fallback` URLs are not emitted.
use super::manifest_item;
use crate::extract::ManifestProtocol;
use crate::sites::art::{date, json_loads, on_host, parse_ts, segment, str_of};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use futures_util::{StreamExt, stream};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::sync::Arc;
use url::Url;

const ROOT: &str = "https://fansly.com";
const API: &str = "https://apiv3.fansly.com";
const FILENAME: &str = "{id}_{num}_{file[id]}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{account[username]} ({account[id]})"];
const ARCHIVE: &str = "{file[id]}";
/// API pages read at most per listing.
const MAX_PAGES: usize = 10_000;

static POST: Route = Route::new("post", FILENAME, DIRECTORY, ARCHIVE);
static HOME: Route = Route::new("home", FILENAME, DIRECTORY, ARCHIVE);
static LIST: Route = Route::new("list", FILENAME, DIRECTORY, ARCHIVE);
static LISTS: Route = Route::new("lists", FILENAME, DIRECTORY, ARCHIVE);
static CREATOR_POSTS: Route = Route::new("creator-posts", FILENAME, DIRECTORY, ARCHIVE);
static CREATOR_MEDIA: Route = Route::new("creator-media", FILENAME, DIRECTORY, ARCHIVE);

pub static SITE: Site = Site {
    category: "fansly",
    name: "Fansly",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&POST, &HOME, &LIST, &LISTS, &CREATOR_POSTS, &CREATOR_MEDIA],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Post(String),
    /// `subscribed` or a list ID; neither for the main timeline.
    Home {
        subscribed: bool,
        list: Option<String>,
    },
    List(String),
    Lists,
    /// Creator (name or `id:ID`), wall and whether it is the media tab.
    Creator(String, Option<String>, bool),
}

fn target(url: &Url) -> Option<Target> {
    if !on_host(url, &["fansly.com"]) {
        return None;
    }
    let path = url.path();
    let digits_after = |prefix: &str| {
        crate::sites::art::leading_digits(path.strip_prefix(prefix)?).map(str::to_owned)
    };
    if let Some(id) = digits_after("/post/") {
        return Some(Target::Post(id));
    }
    if let Some(rest) = path.strip_prefix("/home") {
        let subscribed = rest.starts_with("/subscribed");
        let list = digits_after("/home/list/");
        return Some(Target::Home { subscribed, list });
    }
    if let Some(id) = digits_after("/lists/") {
        return Some(Target::List(id));
    }
    if path.starts_with("/lists") {
        return Some(Target::Lists);
    }
    let parts: Vec<&str> = path[1..].splitn(3, '/').collect();
    let [creator, tab, rest @ ..] = parts.as_slice() else {
        return None;
    };
    let media = match *tab {
        "posts" => false,
        "media" => true,
        _ => return None,
    };
    if !segment(creator) {
        return None;
    }
    let wall = rest
        .first()
        .and_then(|rest| rest.strip_prefix("wall/"))
        .and_then(crate::sites::art::leading_digits)
        .map(str::to_owned);
    Some(Target::Creator(
        crate::sites::percent_decode(creator),
        wall,
        media,
    ))
}

/// API access and the file options.
struct Api<'a> {
    http: &'a Http,
    token: Option<String>,
    /// The `formats` option's variant types.
    formats: Option<Vec<i64>>,
    previews: bool,
}

impl Api<'_> {
    /// `FanslyAPI._call`: the `response` of one API request. `None` values
    /// are not sent.
    async fn call(&self, endpoint: &str, params: &[(&str, Option<&str>)]) -> Result<Value> {
        let mut url = self.http.api(&format!("{API}/api{endpoint}"))?;
        {
            let mut query = url.query_pairs_mut();
            for (key, value) in params {
                if let Some(value) = value {
                    query.append_pair(key, value);
                }
            }
            query.append_pair("ngsw-bypass", "true");
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis())
            .to_string();
        let mut headers = vec![("fansly-client-ts", now.as_str()), ("Origin", ROOT)];
        if let Some(token) = &self.token {
            headers.push(("authorization", token.as_str()));
        }
        let data = self.http.json_with(&url, &headers).await?;
        data.get("response")
            .cloned()
            .ok_or_else(|| error("Fansly returned no response"))
    }

    async fn account(&self, creator: &str) -> Result<Value> {
        let response = match creator.strip_prefix("id:") {
            Some(id) => self.call("/v1/account", &[("ids", Some(id))]).await?,
            None => {
                self.call("/v1/account", &[("usernames", Some(creator))])
                    .await?
            }
        };
        response
            .get(0)
            .cloned()
            .ok_or_else(|| error("Fansly account not found"))
    }

    /// `_extract_files`: the post's attachments as file keywords and URLs.
    async fn files(&self, post: &mut Map<String, Value>) -> Result<Vec<File>> {
        let Some(Value::Array(mut attachments)) = post.remove("attachments") else {
            return Ok(Vec::new());
        };
        if let Some(Value::Array(extra)) = post.remove("_extra") {
            let ids: Vec<String> = extra.iter().map(crate::sites::scalar).collect();
            let response = self
                .call("/v1/account/media", &[("ids", Some(&ids.join(",")))])
                .await?;
            let media = by_id(&response);
            attachments.extend(ids.iter().filter_map(|id| media.get(id).cloned()));
        }
        let mut files = Vec::new();
        for attachment in attachments {
            if let Some(file) = self.attachment(&attachment, false) {
                files.push(file);
            }
        }
        Ok(files)
    }

    /// `_extract_attachment`: the widest allowed variant with locations,
    /// else the preview.
    fn attachment(&self, attachment: &Value, preview: bool) -> Option<File> {
        let Some(Value::Object(mut media)) = attachment
            .get(if preview { "preview" } else { "media" })
            .cloned()
        else {
            return None;
        };
        let mut variants: Vec<Value> = match media.remove("variants") {
            Some(Value::Array(variants)) => variants,
            _ => Vec::new(),
        };
        if crate::sites::art::truthy(media.get("locations")) {
            variants.push(Value::Object(media.clone()));
        }
        let best = variants
            .into_iter()
            .filter(|variant| crate::sites::art::truthy(variant.get("locations")))
            .filter_map(|variant| {
                let kind = variant.get("type").and_then(Value::as_i64)?;
                if kind == 0 || self.formats.as_ref().is_some_and(|f| !f.contains(&kind)) {
                    return None;
                }
                let rank = if kind > 256 { kind - 500 } else { kind };
                let width = variant.get("width").and_then(Value::as_i64).unwrap_or(0);
                Some(((width, rank), variant))
            })
            .fold(
                None::<((i64, i64), Value)>,
                |best, (key, variant)| match best {
                    Some((best_key, _)) if best_key >= key => best,
                    _ => Some((key, variant)),
                },
            );
        let Some((_, Value::Object(mut variant))) = best else {
            if self.previews && !preview && attachment.get("preview").is_some() {
                return self.attachment(attachment, true);
            }
            return None;
        };
        let mime = str_of(&Value::Object(variant.clone()), "mimetype").to_owned();
        let location = match variant.remove("locations") {
            Some(Value::Array(locations)) => locations.into_iter().next()?,
            _ => return None,
        };
        if let Some(Value::String(metadata)) = variant.remove("metadata")
            && let Ok(Value::Object(extra)) = json_loads(&metadata, "Fansly")
        {
            crate::sites::art::update(&mut variant, &extra);
        }
        let format = variant.get("type").cloned().unwrap_or(Value::Null);
        let mut file = variant;
        file.insert("preview".into(), preview.into());
        file.insert("format".into(), format);
        file.insert(
            "date".into(),
            date(media.get("createdAt").and_then(parse_ts)),
        );
        file.insert(
            "date_updated".into(),
            date(media.get("updatedAt").and_then(parse_ts)),
        );
        let raw = str_of(&location, "location").to_owned();
        if let Some(meta) = location.get("metadata") {
            file.insert("type".into(), "video".into());
            let protocol = if mime == "application/dash+xml" {
                ManifestProtocol::Dash
            } else {
                ManifestProtocol::Hls
            };
            let cookies = [
                ("CloudFront-Key-Pair-Id", "Key-Pair-Id"),
                ("CloudFront-Signature", "Signature"),
                ("CloudFront-Policy", "Policy"),
            ]
            .map(|(name, key)| format!("{name}={}", str_of(meta, key)))
            .join("; ");
            Some(File {
                file,
                url: raw,
                manifest: Some((protocol, cookies)),
            })
        } else {
            let kind = if mime.starts_with("image/") {
                "image"
            } else {
                "video"
            };
            file.insert("type".into(), kind.into());
            Some(File {
                file,
                url: raw,
                manifest: None,
            })
        }
    }

    /// `_update_posts`: accounts and attachments resolved from the
    /// response's media and bundles; bundle media missing from it are left
    /// in `_extra` for a later lookup.
    fn update_posts(&self, response: &Value) -> Vec<Map<String, Value>> {
        let accounts = by_id(&response["accounts"]);
        let media = by_id(&response["accountMedia"]);
        let bundles = by_id(&response["accountMediaBundles"]);
        let posts = response["posts"].as_array().cloned().unwrap_or_default();
        posts
            .into_iter()
            .filter_map(|post| {
                let Value::Object(mut post) = post else {
                    return None;
                };
                if let Some(account) = post
                    .get("accountId")
                    .map(crate::sites::scalar)
                    .and_then(|id| accounts.get(&id).cloned())
                {
                    post.remove("accountId");
                    post.insert("account".into(), account);
                }
                let mut extra = Vec::new();
                let mut attachments = Vec::new();
                for attachment in post["attachments"].as_array().cloned().unwrap_or_default() {
                    let Some(cid) = attachment.get("contentId").map(crate::sites::scalar) else {
                        attachments.push(attachment);
                        continue;
                    };
                    if let Some(item) = media.get(&cid) {
                        attachments.push(item.clone());
                    } else if let Some(bundle) = bundles.get(&cid) {
                        let mut content = bundle["bundleContent"]
                            .as_array()
                            .cloned()
                            .unwrap_or_default();
                        content.sort_by_key(|c| c["pos"].as_i64().unwrap_or(0));
                        for c in content {
                            let id = crate::sites::scalar(&c["accountMediaId"]);
                            match media.get(&id) {
                                Some(item) => attachments.push(item.clone()),
                                None => extra.push(c["accountMediaId"].clone()),
                            }
                        }
                    }
                }
                if !extra.is_empty() {
                    post.insert("_extra".into(), extra.into());
                }
                post.insert("attachments".into(), attachments.into());
                Some(post)
            })
            .collect()
    }
}

/// An object list keyed by each object's `id`.
fn by_id(list: &Value) -> HashMap<String, Value> {
    list.as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| Some((item.get("id").map(crate::sites::scalar)?, item.clone())))
        .collect()
}

/// One attachment file.
struct File {
    file: Map<String, Value>,
    url: String,
    /// Manifest protocol and the CloudFront `Cookie` header.
    manifest: Option<(ManifestProtocol, String)>,
}

/// `FanslyExtractor.items` for one post: one post boundary of its files.
async fn post_events<'a>(
    api: &Api<'a>,
    route: &'static Route,
    mut post: Map<String, Value>,
    extra: &Map<String, Value>,
) -> Result<Vec<crate::event::ExtractionEvent>> {
    let files = api.files(&mut post).await?;
    let created = post.get("createdAt").and_then(parse_ts);
    post.insert("count".into(), files.len().into());
    post.insert("date".into(), date(created));
    crate::sites::art::update(&mut post, extra);
    let id = post.get("id").map(crate::sites::scalar).unwrap_or_default();
    let user = post
        .get("account")
        .map(|account| str_of(account, "username").to_owned());
    let caption = post
        .get("content")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let mut items = Vec::new();
    for (index, file) in files.into_iter().enumerate() {
        let mut fields = post.clone();
        fields.insert("num".into(), (index + 1).into());
        let file_id = file
            .file
            .get("id")
            .map(crate::sites::scalar)
            .unwrap_or_default();
        fields.insert("file".into(), Value::Object(file.file));
        let url = match &file.manifest {
            Some(_) => format!("ytdl:{}", file.url),
            None => file.url.clone(),
        };
        fields.insert("url".into(), url.clone().into());
        crate::sites::art::nameext_into(&url, &mut fields);
        let item = match file.manifest {
            Some((protocol, cookies)) => manifest_item(&file.url, protocol, fields)
                .map(|item| item.header("Cookie", &cookies)),
            None => crate::sites::media::web_url(&file.url).map(|url| Item::new(url, fields)),
        };
        if let Some(item) = item {
            items.push(
                item.ids(id.clone(), file_id)
                    .user(user.clone())
                    .caption(caption.clone())
                    .timestamp(created),
            );
        }
    }
    Ok(crate::sites::post(&SITE, api.http, route, items))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Fansly URL"));
    };
    let formats = match http.option("formats") {
        Some(Value::Array(items)) if !items.is_empty() => {
            Some(items.iter().filter_map(Value::as_i64).collect())
        }
        _ => None,
    };
    let api = Arc::new(Api {
        http,
        token: http
            .option_str("token")
            .filter(|t| !t.is_empty())
            .map(str::to_owned),
        formats,
        previews: http.option_bool("previews", true),
    });
    match target {
        Target::Post(id) => crate::sites::once(async move {
            let response = api.call("/v1/post", &[("ids", Some(&id))]).await?;
            let mut events = Vec::new();
            for post in api.update_posts(&response) {
                events.extend(post_events(&api, &POST, post, &Map::new()).await?);
            }
            Ok(crate::sites::events(events))
        }),
        Target::Home { subscribed, list } => {
            let mode = match (&list, subscribed) {
                (Some(_), _) => None,
                (None, true) => Some("1".to_owned()),
                (None, false) => Some("0".to_owned()),
            };
            let mut params = vec![
                ("before".to_owned(), Some("0".to_owned())),
                ("after".to_owned(), Some("0".to_owned())),
            ];
            match list {
                Some(list) => params.push(("listId".into(), Some(list))),
                None => params.push(("mode".into(), mode)),
            }
            timeline(api, &HOME, "/v1/timeline/home".into(), params, Map::new())
        }
        Target::List(id) => list_accounts(api, id),
        Target::Lists => crate::sites::once(async move {
            let response = api
                .call("/v1/lists/account", &[("itemId", Some(""))])
                .await?;
            let children: Vec<(Url, Map<String, Value>)> = response
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|list| {
                    let id = list.get("id").map(crate::sites::scalar)?;
                    let label = str_of(list, "label");
                    let mut url = Url::parse(&format!("{ROOT}/lists/{id}")).ok()?;
                    url.set_fragment(Some(label));
                    Some((url, list.as_object()?.clone()))
                })
                .collect();
            Ok(queue(http, &LISTS, children))
        }),
        Target::Creator(creator, wall, media) => crate::sites::once(async move {
            let account = api.account(&creator).await?;
            let walls: Vec<Value> = account["walls"].as_array().cloned().unwrap_or_default();
            let walls = match &wall {
                Some(id) => {
                    let wall = walls
                        .into_iter()
                        .find(|w| w.get("id").map(crate::sites::scalar).as_deref() == Some(id))
                        .ok_or_else(|| error("Fansly wall not found"))?;
                    vec![wall]
                }
                None => walls,
            };
            let account_id = account
                .get("id")
                .map(crate::sites::scalar)
                .unwrap_or_default();
            let pages: Vec<Events<'_>> = walls
                .into_iter()
                .map(|wall| {
                    let wall_id = wall.get("id").map(crate::sites::scalar).unwrap_or_default();
                    let mut extra = Map::new();
                    extra.insert("wall".into(), wall);
                    if media {
                        media_offers(api.clone(), account_id.clone(), wall_id, extra)
                    } else {
                        let params = vec![
                            ("before".to_owned(), Some("0".to_owned())),
                            ("after".to_owned(), Some("0".to_owned())),
                            ("wallId".to_owned(), Some(wall_id)),
                            ("contentSearch".to_owned(), Some(String::new())),
                        ];
                        let endpoint = format!("/v1/timelinenew/{account_id}");
                        timeline(api.clone(), &CREATOR_POSTS, endpoint, params, extra)
                    }
                })
                .collect();
            Ok(stream::iter(pages).flatten().boxed())
        }),
    }
}

type Params = Vec<(String, Option<String>)>;

fn borrowed(params: &Params) -> Vec<(&str, Option<&str>)> {
    params
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_deref()))
        .collect()
}

/// `_pagination`: timeline pages continued `before` the lowest post ID.
fn timeline<'a>(
    api: Arc<Api<'a>>,
    route: &'static Route,
    endpoint: String,
    params: Params,
    extra: Map<String, Value>,
) -> Events<'a> {
    crate::sites::paginate(Some((params, 0usize)), move |state| {
        let api = api.clone();
        let endpoint = endpoint.clone();
        let extra = extra.clone();
        async move {
            let Some((mut params, count)) = state else {
                return Ok(None);
            };
            let response = api.call(&endpoint, &borrowed(&params)).await?;
            if !crate::sites::art::truthy(response.get("posts")) {
                return Ok(None);
            }
            let posts = api.update_posts(&response);
            let before = posts
                .iter()
                .filter_map(|p| p.get("id").map(crate::sites::scalar))
                .min();
            let mut events = Vec::new();
            for post in posts {
                events.extend(post_events(&api, route, post, &extra).await?);
            }
            let next = before.filter(|_| count + 1 < MAX_PAGES).map(|before| {
                set(&mut params, "before", before);
                (params, count + 1)
            });
            Ok(Some((crate::sites::events(events), next)))
        }
    })
}

/// `_pagination_media`: a wall's media offers, 30 a page, each resolved
/// to its post.
fn media_offers<'a>(
    api: Arc<Api<'a>>,
    account: String,
    wall: String,
    extra: Map<String, Value>,
) -> Events<'a> {
    let params: Params = [
        ("locationId", wall.as_str()),
        ("locationType", "1002"),
        ("accountId", account.as_str()),
        ("mediaType", ""),
        ("before", ""),
        ("after", "0"),
        ("limit", "30"),
        ("offset", "0"),
    ]
    .iter()
    .map(|(k, v)| ((*k).to_owned(), Some((*v).to_owned())))
    .collect();
    crate::sites::paginate(Some((params, 0usize)), move |state| {
        let api = api.clone();
        let extra = extra.clone();
        async move {
            let Some((mut params, count)) = state else {
                return Ok(None);
            };
            let response = api
                .call("/v1/mediaoffers/location", &borrowed(&params))
                .await?;
            let data = response["data"].as_array().cloned().unwrap_or_default();
            if data.is_empty() {
                return Ok(None);
            }
            // `_update_media`: the posts in the order of the offers.
            let mut aggregation = response["aggregationData"].clone();
            let posts = by_id(&aggregation["posts"]);
            let ordered: Vec<Value> = data
                .iter()
                .filter_map(|item| {
                    posts
                        .get(&crate::sites::scalar(&item["correlationId"]))
                        .cloned()
                })
                .collect();
            aggregation["posts"] = ordered.into();
            let mut events = Vec::new();
            for post in api.update_posts(&aggregation) {
                events.extend(post_events(&api, &CREATOR_MEDIA, post, &extra).await?);
            }
            let before = data
                .last()
                .and_then(|item| item.get("id"))
                .map(crate::sites::scalar);
            let next = before.filter(|_| count + 1 < MAX_PAGES).map(|before| {
                set(&mut params, "before", before);
                (params, count + 1)
            });
            Ok(Some((crate::sites::events(events), next)))
        }
    })
}

/// `FanslyListExtractor`: each listed account's posts queued, 50 a page.
fn list_accounts<'a>(api: Arc<Api<'a>>, list: String) -> Events<'a> {
    let params: Params = vec![
        ("listId".into(), Some(list)),
        ("limit".into(), Some("50".into())),
        ("after".into(), None),
        ("sortMode".into(), Some("3".into())),
    ];
    crate::sites::paginate(Some((params, 0usize)), move |state| {
        let api = api.clone();
        async move {
            let Some((mut params, count)) = state else {
                return Ok(None);
            };
            let response = api.call("/v1/lists/itemsnew", &borrowed(&params)).await?;
            let items = response.as_array().cloned().unwrap_or_default();
            if items.is_empty() {
                return Ok(None);
            }
            // `_update_items`: the accounts behind the list items.
            let ids: Vec<String> = items
                .iter()
                .filter_map(|item| item.get("id").map(crate::sites::scalar))
                .collect();
            let accounts = by_id(
                &api.call("/v1/account", &[("ids", Some(&ids.join(",")))])
                    .await?,
            );
            let children: Vec<(Url, Map<String, Value>)> = ids
                .iter()
                .filter_map(|id| {
                    let account = accounts.get(id)?;
                    let name = str_of(account, "username");
                    let url = Url::parse(&format!("{ROOT}/{name}/posts")).ok()?;
                    Some((url, account.as_object()?.clone()))
                })
                .collect();
            let after = items
                .last()
                .and_then(|item| item.get("sortId"))
                .map(crate::sites::scalar);
            let next = after.filter(|_| count + 1 < MAX_PAGES).map(|after| {
                set(&mut params, "after", after);
                (params, count + 1)
            });
            Ok(Some((queue(api.http, &LIST, children), next)))
        }
    })
}

fn set(params: &mut Params, key: &str, value: String) {
    match params.iter_mut().find(|(k, _)| k == key) {
        Some((_, old)) => *old = Some(value),
        None => params.push((key.into(), Some(value))),
    }
}

fn queue<'a>(
    http: &'a Http,
    route: &'static Route,
    children: Vec<(Url, Map<String, Value>)>,
) -> Events<'a> {
    stream::iter(
        children
            .into_iter()
            .filter(|(url, _)| target(url).is_some()),
    )
    .map(move |(child, data)| {
        crate::sites::queue(http, SITE.category, route.subcategory, child, &data)
    })
    .flatten()
    .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::MediaJob;
    use crate::sites::manga::tests::{archive, path};
    use crate::sites::testing::{http, json, keywords, media, queued, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://fansly.com/post/1234567890"),
            Some(Target::Post("1234567890".into()))
        );
        assert_eq!(
            t("https://fansly.com/home"),
            Some(Target::Home {
                subscribed: false,
                list: None
            })
        );
        assert_eq!(
            t("https://fansly.com/home/subscribed"),
            Some(Target::Home {
                subscribed: true,
                list: None
            })
        );
        assert_eq!(
            t("https://fansly.com/home/list/12"),
            Some(Target::Home {
                subscribed: false,
                list: Some("12".into())
            })
        );
        assert_eq!(
            t("https://fansly.com/lists/12"),
            Some(Target::List("12".into()))
        );
        assert_eq!(t("https://fansly.com/lists"), Some(Target::Lists));
        assert_eq!(
            t("https://www.fansly.com/creator/posts/wall/34"),
            Some(Target::Creator("creator".into(), Some("34".into()), false))
        );
        assert_eq!(
            t("https://fansly.com/creator/media"),
            Some(Target::Creator("creator".into(), None, true))
        );
        for raw in [
            "https://fansly.com/creator",
            "https://fansly.com/creator/likes",
            "https://fansly.com.evil.test/post/1",
            "https://fansly.com:8443/post/1",
            "https://u:p@fansly.com/post/1",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn post_response() -> Value {
        json!({"response": {
            "posts": [{"id": "500", "accountId": "7", "content": "Hello", "createdAt": 1577934245,
                "attachments": [{"contentId": "m1"}, {"contentId": "b1"}]}],
            "accounts": [{"id": "7", "username": "creator"}],
            "accountMedia": [
                {"id": "m1", "media": {"id": "f1", "createdAt": 1577934245, "updatedAt": 1577934245,
                    "type": 1, "width": 100, "mimetype": "image/jpeg",
                    "locations": [{"location": "https://cdn3.fansly.test/f1.jpeg?sig=x"}],
                    "variants": [{"id": "f1s", "type": 1, "width": 50, "mimetype": "image/jpeg",
                        "locations": [{"location": "https://cdn3.fansly.test/f1s.jpeg"}]}]}},
                {"id": "m2", "media": {"id": "f2", "createdAt": 1577934245, "updatedAt": 1577934245,
                    "type": 2, "width": 0, "mimetype": "video/mp4", "locations": [],
                    "variants": [{"id": "f2h", "type": 302, "width": 1920,
                        "mimetype": "application/vnd.apple.mpegurl",
                        "metadata": "{\"duration\": 12}",
                        "locations": [{"location": "https://cdn3.fansly.test/f2.m3u8",
                            "metadata": {"Key-Pair-Id": "K", "Signature": "S", "Policy": "P"}}]}]}},
            ],
            "accountMediaBundles": [{"id": "b1", "bundleContent": [
                {"accountMediaId": "m3", "pos": 1}, {"accountMediaId": "m2", "pos": 0}]}],
        }})
    }

    #[tokio::test]
    async fn extracts_post_with_bundles() {
        let extra = json!({"response": [
            {"id": "m3", "media": {"id": "f3", "createdAt": 1, "updatedAt": 1, "type": 1,
                "width": 10, "mimetype": "image/png",
                "locations": [{"location": "https://cdn3.fansly.test/f3.png"}]}}]});
        let (root, server) = serve(vec![json(post_response()), json(extra)]);
        let http = http("fansly", root).with_options(Some(Arc::new(
            json!({"token": "SECRET"}).as_object().unwrap().clone(),
        )));
        let files = media(extract(
            &http,
            Url::parse("https://fansly.com/post/500").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 3);
        assert_eq!(
            files[0].url.as_str(),
            "https://cdn3.fansly.test/f1.jpeg?sig=x"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["file"]["id"], "f1");
        assert_eq!(k["file"]["type"], "image");
        assert_eq!(k["count"], 3);
        assert_eq!(k["account"]["username"], "creator");
        assert_eq!(k["date"], "2020-01-02 03:04:05");
        assert_eq!(path(&files[0]), "fansly/creator (7)/500_1_f1.jpeg");
        assert_eq!(archive(&files[0]), r#"["{file[id]}",["f1"]]"#);
        assert_eq!(
            files[1].url.as_str(),
            "ytdl:https://cdn3.fansly.test/f2.m3u8"
        );
        assert!(matches!(
            files[1].job,
            MediaJob::Manifest {
                protocol: ManifestProtocol::Hls,
                ..
            }
        ));
        assert_eq!(
            files[1].headers["Cookie"],
            "CloudFront-Key-Pair-Id=K; CloudFront-Signature=S; CloudFront-Policy=P"
        );
        let k = keywords(&files[1]);
        assert_eq!(k["file"]["type"], "video");
        assert_eq!(k["file"]["duration"], 12);
        assert_eq!(k["file"]["format"], 302);
        assert_eq!(keywords(&files[2])["file"]["id"], "f3");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/v1/post?ids=500&ngsw-bypass=true "));
        assert!(requests[0].contains("authorization: SECRET"));
        assert!(requests[0].contains("origin: https://fansly.com"));
        assert!(requests[1].starts_with("GET /api/v1/account/media?ids=m3&ngsw-bypass=true "));
    }

    #[tokio::test]
    async fn paginates_creator_wall() {
        let account = json!({"response": [{"id": "7", "username": "creator",
            "walls": [{"id": "30", "name": "Main"}, {"id": "34", "name": "Other"}]}]});
        let page = |id: &str| {
            json!({"response": {"posts": [{"id": id, "accountId": "7", "createdAt": 1,
                "attachments": [{"contentId": "m1"}]}],
                "accounts": [{"id": "7", "username": "creator"}],
                "accountMedia": [{"id": "m1", "media": {"id": format!("f{id}"), "createdAt": 1,
                    "updatedAt": 1, "type": 1, "width": 1, "mimetype": "image/jpeg",
                    "locations": [{"location": format!("https://cdn3.fansly.test/{id}.jpg")}]}}],
                "accountMediaBundles": []}})
        };
        let (root, server) = serve(vec![
            json(account),
            json(page("90")),
            json(json!({"response": {"posts": []}})),
        ]);
        let http = http("fansly", root);
        let files = media(extract(
            &http,
            Url::parse("https://fansly.com/creator/posts/wall/34").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(keywords(&files[0])["wall"]["name"], "Other");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/v1/account?usernames=creator&ngsw-bypass=true "));
        assert!(requests[1].starts_with(
            "GET /api/v1/timelinenew/7?before=0&after=0&wallId=34&contentSearch=&ngsw-bypass=true "
        ));
        assert!(requests[2].starts_with("GET /api/v1/timelinenew/7?before=90&"));
    }

    #[tokio::test]
    async fn queues_list_accounts() {
        let (root, server) = serve(vec![
            json(json!({"response": [{"id": "7", "sortId": "s1"}]})),
            json(json!({"response": [{"id": "7", "username": "creator"}]})),
            json(json!({"response": []})),
        ]);
        let http = http("fansly", root);
        let children = queued(extract(
            &http,
            Url::parse("https://fansly.com/lists/12").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].subcategory(), "list");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with(
            "GET /api/v1/lists/itemsnew?listId=12&limit=50&sortMode=3&ngsw-bypass=true "
        ));
        assert!(requests[1].starts_with("GET /api/v1/account?ids=7&"));
        assert!(
            requests[2]
                .starts_with("GET /api/v1/lists/itemsnew?listId=12&limit=50&after=s1&sortMode=3&")
        );
    }
}
