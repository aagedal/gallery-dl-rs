//! Fansly posts, home timelines, lists and creator posts/media, mirroring
//! gallery_dl/extractor/fansly.py. Requests use the `token` option as the
//! `authorization` header.
use crate::error;
use crate::event::ExtractionEvent;
use crate::extract::{ManifestProtocol, MediaJob};
use crate::sites::gdl::{captures, group, int_of, nameext_into, nested_date, parse_ts, re};
use crate::sites::{Events, Http, Item, Route, Site, scalar};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::sync::Arc;
use url::Url;

const ROOT: &str = "https://fansly.com";
const API: &str = "https://apiv3.fansly.com";
const FILENAME: &str = "{id}_{num}_{file[id]}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{account[username]} ({account[id]})"];

macro_rules! routes {
    ($($ident:ident $sub:literal),*) => { $(static $ident: Route = Route::new($sub, FILENAME, DIRECTORY, "{file[id]}");)* };
}
routes!(POST "post", HOME "home", LIST "list", LISTS "lists", CREATOR_POSTS "creator-posts", CREATOR_MEDIA "creator-media");

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
    /// `(subscribed, list id)`.
    Home(bool, Option<String>),
    List(String),
    Lists,
    CreatorPosts(String, Option<String>),
    CreatorMedia(String, Option<String>),
}

fn target(url: &Url) -> Option<Target> {
    const BASE: &str = r"^https?://(?:www\.)?fansly\.com";
    if let Some(c) = captures(re!(&format!(r"{BASE}/post/(\d+)")), url) {
        return Some(Target::Post(group(&c, 1).into()));
    }
    if let Some(c) = captures(
        re!(&format!(r"{BASE}/home(?:/(?:subscribed()|list/(\d+)))?")),
        url,
    ) {
        return Some(Target::Home(
            c.get(1).is_some(),
            c.get(2).map(|m| m.as_str().to_owned()),
        ));
    }
    if let Some(c) = captures(re!(&format!(r"{BASE}/lists/(\d+)")), url) {
        return Some(Target::List(group(&c, 1).into()));
    }
    if captures(re!(&format!(r"{BASE}/lists")), url).is_some() {
        return Some(Target::Lists);
    }
    if let Some(c) = captures(
        re!(&format!(r"{BASE}/([^/?#]+)/posts(?:/wall/(\d+))?")),
        url,
    ) {
        return Some(Target::CreatorPosts(
            group(&c, 1).into(),
            c.get(2).map(|m| m.as_str().to_owned()),
        ));
    }
    captures(
        re!(&format!(r"{BASE}/([^/?#]+)/media(?:/wall/(\d+))?")),
        url,
    )
    .map(|c| Target::CreatorMedia(group(&c, 1).into(), c.get(2).map(|m| m.as_str().to_owned())))
}

type Params = Vec<(&'static str, String)>;

fn set(params: &mut Params, name: &'static str, value: String) {
    match params.iter_mut().find(|(k, _)| *k == name) {
        Some(entry) => entry.1 = value,
        None => params.push((name, value)),
    }
}

/// `FanslyAPI._call`: the `response` member of one API reply.
async fn call(http: &Http, endpoint: &str, params: &Params) -> crate::Result<Value> {
    let mut url = http.api(&format!("{API}/api{endpoint}"))?;
    url.query_pairs_mut()
        .extend_pairs(params.iter().map(|(k, v)| (*k, v.as_str())))
        .append_pair("ngsw-bypass", "true");
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
        .to_string();
    let mut headers = vec![("fansly-client-ts", ts.as_str()), ("Origin", ROOT)];
    if let Some(token) = http.option_str("token").filter(|t| !t.is_empty()) {
        headers.push(("authorization", token));
    }
    let mut data = http.json_with(&url, &headers).await?;
    Ok(data["response"].take())
}

fn ids(values: &[Value]) -> String {
    values.iter().map(scalar).collect::<Vec<_>>().join(",")
}

fn by_id(list: &Value) -> HashMap<String, Value> {
    list.as_array()
        .into_iter()
        .flatten()
        .map(|item| (scalar(&item["id"]), item.clone()))
        .collect()
}

/// `_update_posts`: resolve accounts and attachment content ids.
fn update_posts(response: &Value) -> Vec<Map<String, Value>> {
    let accounts = by_id(&response["accounts"]);
    let media = by_id(&response["accountMedia"]);
    let bundles = by_id(&response["accountMediaBundles"]);
    let mut posts = Vec::new();
    for post in response["posts"].as_array().into_iter().flatten() {
        let Some(mut post) = post.as_object().cloned() else {
            continue;
        };
        if let Some(account) = post
            .get("accountId")
            .and_then(|id| accounts.get(&scalar(id)))
        {
            post.remove("accountId");
            post.insert("account".into(), account.clone());
        }
        let mut extra = Vec::new();
        let mut attachments = Vec::new();
        for attachment in post
            .get("attachments")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(cid) = attachment.get("contentId").map(scalar) else {
                attachments.push(attachment.clone());
                continue;
            };
            if let Some(media) = media.get(&cid) {
                attachments.push(media.clone());
            } else if let Some(bundle) = bundles.get(&cid) {
                let mut content = bundle["bundleContent"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                content.sort_by_key(|c| int_of(c.get("pos")));
                for c in content {
                    let mid = scalar(&c["accountMediaId"]);
                    match media.get(&mid) {
                        Some(media) => attachments.push(media.clone()),
                        None => extra.push(c["accountMediaId"].clone()),
                    }
                }
            }
        }
        if !extra.is_empty() {
            post.insert("_extra".into(), extra.into());
        }
        post.insert("attachments".into(), attachments.into());
        posts.push(post);
    }
    posts
}

/// Extraction options shared by every post.
struct Options {
    route: &'static Route,
    previews: bool,
    formats: Option<Vec<i64>>,
    wall: Option<Value>,
}

impl Options {
    fn new(http: &Http, route: &'static Route) -> Self {
        let formats = match http.option("formats") {
            Some(Value::Array(list)) if !list.is_empty() => {
                Some(list.iter().map(|v| int_of(Some(v))).collect())
            }
            _ => None,
        };
        Self {
            route,
            previews: http.option_bool("previews", true),
            formats,
            wall: None,
        }
    }
}

/// `_extract_attachment`: the widest allowed variant, or the preview.
fn extract_attachment(
    files: &mut Vec<Map<String, Value>>,
    options: &Options,
    attachment: &Value,
    preview: bool,
) {
    let mut media = attachment[if preview { "preview" } else { "media" }].clone();
    let Some(map) = media.as_object_mut() else {
        return;
    };
    let mut variants = map
        .remove("variants")
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();
    if crate::sites::gdl::truthy(map.get("locations")) {
        variants.push(Value::Object(map.clone()));
    }
    let best = variants
        .into_iter()
        .filter(|variant| crate::sites::gdl::truthy(variant.get("locations")))
        .filter_map(|variant| {
            let kind = int_of(variant.get("type"));
            if kind == 0 || options.formats.as_ref().is_some_and(|f| !f.contains(&kind)) {
                return None;
            }
            let rank = if kind > 256 { kind - 500 } else { kind };
            Some(((int_of(variant.get("width")), rank), variant))
        })
        .max_by_key(|(key, _)| *key)
        .map(|(_, variant)| variant);
    let Some(Value::Object(mut variant)) = best else {
        if options.previews && !preview && attachment.get("preview").is_some() {
            extract_attachment(files, options, attachment, true);
        }
        return;
    };
    let mime = variant.get("mimetype").map(scalar).unwrap_or_default();
    let location = variant
        .remove("locations")
        .and_then(|l| l.get(0).cloned())
        .unwrap_or(Value::Null);
    if let Some(meta) = variant.remove("metadata")
        && let Some(Value::Object(meta)) = meta.as_str().and_then(|m| serde_json::from_str(m).ok())
    {
        variant.extend(meta);
    }
    let mut file = variant.clone();
    file.insert("preview".into(), preview.into());
    file.insert(
        "format".into(),
        variant.get("type").cloned().unwrap_or(Value::Null),
    );
    file.insert(
        "date".into(),
        nested_date(map.get("createdAt").and_then(parse_ts)),
    );
    file.insert(
        "date_updated".into(),
        nested_date(map.get("updatedAt").and_then(parse_ts)),
    );
    let mut entry = Map::new();
    let target = location.get("location").map(scalar).unwrap_or_default();
    if let Some(meta) = location.get("metadata").filter(|m| m.is_object()) {
        file.insert("type".into(), "video".into());
        let fallback: Vec<Value> = map
            .get("locations")
            .and_then(|l| l.get(0))
            .and_then(|l| l.get("location"))
            .cloned()
            .into_iter()
            .collect();
        let manifest = if mime == "application/dash+xml" {
            "dash"
        } else {
            "hls"
        };
        entry.insert("url".into(), format!("ytdl:{target}").into());
        entry.insert("_fallback".into(), fallback.into());
        entry.insert("_ytdl_manifest".into(), manifest.into());
        entry.insert(
            "_cookies".into(),
            format!(
                "CloudFront-Key-Pair-Id={}; CloudFront-Signature={}; CloudFront-Policy={}",
                scalar(&meta["Key-Pair-Id"]),
                scalar(&meta["Signature"]),
                scalar(&meta["Policy"]),
            )
            .into(),
        );
        entry.insert("_manifest".into(), target.into());
    } else {
        let kind = if mime.starts_with("image/") {
            "image"
        } else {
            "video"
        };
        file.insert("type".into(), kind.into());
        entry.insert("url".into(), target.into());
    }
    entry.insert("file".into(), file.into());
    files.push(entry);
}

/// `_extract_files`, fetching bundle media missing from the response.
async fn extract_files(
    http: &Http,
    options: &Options,
    post: &mut Map<String, Value>,
) -> crate::Result<Vec<Map<String, Value>>> {
    let Some(Value::Array(mut attachments)) = post.remove("attachments") else {
        return Ok(Vec::new());
    };
    if let Some(Value::Array(extra)) = post.remove("_extra") {
        let fetched = call(http, "/v1/account/media", &vec![("ids", ids(&extra))]).await?;
        let media = by_id(&fetched);
        attachments.extend(
            extra
                .iter()
                .filter_map(|id| media.get(&scalar(id)).cloned()),
        );
    }
    let mut files = Vec::new();
    for attachment in &attachments {
        extract_attachment(&mut files, options, attachment, false);
    }
    Ok(files)
}

/// One post's files, as `items` yields them.
async fn post_events(
    http: &Http,
    options: &Options,
    mut post: Map<String, Value>,
) -> crate::Result<Vec<ExtractionEvent>> {
    let files = extract_files(http, options, &mut post).await?;
    post.insert("count".into(), files.len().into());
    post.insert(
        "date".into(),
        nested_date(post.get("createdAt").and_then(parse_ts)),
    );
    if let Some(wall) = &options.wall {
        post.insert("wall".into(), wall.clone());
    }
    let post_id = post.get("id").map(scalar).unwrap_or_default();
    let mut items = Vec::new();
    for (index, mut file) in files.into_iter().enumerate() {
        let cookies = file.remove("_cookies");
        let manifest = file.remove("_manifest");
        let mut data = post.clone();
        data.insert("num".into(), (index + 1).into());
        let file_id = file["file"].get("id").map(scalar).unwrap_or_default();
        let raw = file.get("url").map(scalar).unwrap_or_default();
        data.extend(file);
        let item = if let Some(base) = manifest.as_ref().and_then(Value::as_str) {
            nameext_into(base, &mut data);
            data.insert("extension".into(), "mp4".into());
            let protocol = if data["_ytdl_manifest"] == "dash" {
                ManifestProtocol::Dash
            } else {
                ManifestProtocol::Hls
            };
            let (Ok(url), Ok(base_url)) = (Url::parse(&raw), Url::parse(base)) else {
                continue;
            };
            let mut item = Item::new(url, data).job(MediaJob::Manifest {
                protocol,
                base_url,
                inline_data: None,
            });
            // yt-dlp receives the CloudFront signature as a cookie header.
            if let Some(cookies) = cookies.as_ref().and_then(Value::as_str) {
                item = item.header("Cookie", cookies);
            }
            item
        } else {
            nameext_into(&raw, &mut data);
            let Ok(url) = Url::parse(&raw) else { continue };
            Item::new(url, data)
        };
        items.push(item.ids(post_id.as_str(), file_id));
    }
    Ok(crate::sites::post(&SITE, http, options.route, items))
}

/// Posts of one page, extracted lazily in order.
fn posts_events<'a>(
    http: &'a Http,
    options: Arc<Options>,
    posts: Vec<Map<String, Value>>,
) -> Events<'a> {
    crate::sites::each(posts, move |post| {
        let options = options.clone();
        async move { post_events(http, &options, post).await }
    })
}

/// `_pagination` (timelines) and `_pagination_media` (media offers).
fn pagination<'a>(
    http: &'a Http,
    options: Arc<Options>,
    endpoint: String,
    params: Params,
    media: bool,
) -> Events<'a> {
    crate::sites::paginate(Some(params), move |state| {
        let (endpoint, options) = (endpoint.clone(), options.clone());
        async move {
            let Some(mut params) = state else {
                return Ok(None);
            };
            let response = call(http, &endpoint, &params).await?;
            let posts = if media {
                let Some(data) = response["data"].as_array().filter(|d| !d.is_empty()) else {
                    return Ok(None);
                };
                // `_update_media`: posts in the order of their media items.
                let mut aggregation = response["aggregationData"].clone();
                let posts = by_id(&aggregation["posts"]);
                let ordered: Vec<Value> = data
                    .iter()
                    .filter_map(|item| posts.get(&scalar(&item["correlationId"])).cloned())
                    .collect();
                aggregation["posts"] = ordered.into();
                set(&mut params, "before", scalar(&data[data.len() - 1]["id"]));
                update_posts(&aggregation)
            } else {
                if !crate::sites::gdl::truthy(response.get("posts")) {
                    return Ok(None);
                }
                let posts = update_posts(&response);
                let Some(min) = posts
                    .iter()
                    .filter_map(|p| p.get("id").map(scalar))
                    .min_by_key(|id| (id.len(), id.clone()))
                else {
                    return Ok(None);
                };
                set(&mut params, "before", min);
                posts
            };
            Ok(Some((posts_events(http, options, posts), Some(params))))
        }
    })
}

/// `FanslyListExtractor`: queue each list member's posts.
fn list(http: &Http, list_id: String) -> Events<'_> {
    let params: Params = vec![
        ("listId", list_id),
        ("limit", "50".into()),
        ("sortMode", "3".into()),
    ];
    crate::sites::paginate(Some(params), move |state| async move {
        let Some(mut params) = state else {
            return Ok(None);
        };
        let response = call(http, "/v1/lists/itemsnew", &params).await?;
        let Some(items) = response.as_array().filter(|r| !r.is_empty()) else {
            return Ok(None);
        };
        // `_update_items`: full accounts, in list order.
        let item_ids: Vec<Value> = items.iter().map(|i| i["id"].clone()).collect();
        let accounts = by_id(&call(http, "/v1/account", &vec![("ids", ids(&item_ids))]).await?);
        let children = item_ids
            .iter()
            .filter_map(|id| {
                let account = accounts.get(&scalar(id))?;
                let url = format!("{ROOT}/{}/posts", scalar(&account["username"]));
                Some((Url::parse(&url).ok()?, account.as_object()?.clone()))
            })
            .collect();
        set(
            &mut params,
            "after",
            scalar(&items[items.len() - 1]["sortId"]),
        );
        Ok(Some((
            crate::sites::galleries::queue(http, &SITE, &LIST, children),
            Some(params),
        )))
    })
}

/// `FanslyListsExtractor`: queue each of the account's lists.
fn lists(http: &Http) -> Events<'_> {
    crate::sites::once(async move {
        let response = call(http, "/v1/lists/account", &vec![("itemId", String::new())]).await?;
        let children = response
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|list| {
                let url = format!(
                    "{ROOT}/lists/{}#{}",
                    scalar(&list["id"]),
                    scalar(&list["label"])
                );
                Some((Url::parse(&url).ok()?, list.as_object()?.clone()))
            })
            .collect();
        Ok(crate::sites::galleries::queue(
            http, &SITE, &LISTS, children,
        ))
    })
}

/// `FanslyExtractor.posts` for creators: every (or one) wall's posts.
fn creator<'a>(
    http: &'a Http,
    route: &'static Route,
    name: String,
    wall_id: Option<String>,
) -> Events<'a> {
    crate::sites::once(async move {
        let params = match name.strip_prefix("id:") {
            Some(id) => vec![("ids", id.to_owned())],
            None => vec![("usernames", name.clone())],
        };
        let account = call(http, "/v1/account", &params).await?[0].take();
        let mut walls = account["walls"].as_array().cloned().unwrap_or_default();
        if let Some(wall_id) = wall_id {
            walls.retain(|wall| scalar(&wall["id"]) == wall_id);
            if walls.is_empty() {
                return Err(error("Requested wall could not be found"));
            }
        }
        let account_id = scalar(&account["id"]);
        let streams = walls.into_iter().map(move |wall| {
            let wall_id = scalar(&wall["id"]);
            let mut options = Options::new(http, route);
            options.wall = Some(wall);
            let options = Arc::new(options);
            if std::ptr::eq(route, &CREATOR_MEDIA) {
                let params: Params = vec![
                    ("locationId", wall_id),
                    ("locationType", "1002".into()),
                    ("accountId", account_id.clone()),
                    ("mediaType", String::new()),
                    ("before", String::new()),
                    ("after", "0".into()),
                    ("limit", "30".into()),
                    ("offset", "0".into()),
                ];
                pagination(
                    http,
                    options,
                    "/v1/mediaoffers/location".into(),
                    params,
                    true,
                )
            } else {
                let params: Params = vec![
                    ("before", "0".into()),
                    ("after", "0".into()),
                    ("wallId", wall_id),
                    ("contentSearch", String::new()),
                ];
                pagination(
                    http,
                    options,
                    format!("/v1/timelinenew/{account_id}"),
                    params,
                    false,
                )
            }
        });
        use futures_util::StreamExt;
        Ok(futures_util::stream::iter(streams).flatten().boxed())
    })
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Fansly URL"));
    };
    match target {
        Target::Post(id) => crate::sites::once(async move {
            let response = call(http, "/v1/post", &vec![("ids", id)]).await?;
            let options = Arc::new(Options::new(http, &POST));
            Ok(posts_events(http, options, update_posts(&response)))
        }),
        Target::Home(subscribed, list_id) => {
            let mut params: Params = vec![("before", "0".into()), ("after", "0".into())];
            match list_id {
                Some(id) => params.push(("listId", id)),
                None => params.push(("mode", if subscribed { "1" } else { "0" }.into())),
            }
            let options = Arc::new(Options::new(http, &HOME));
            pagination(http, options, "/v1/timeline/home".into(), params, false)
        }
        Target::List(id) => list(http, id),
        Target::Lists => lists(http),
        Target::CreatorPosts(name, wall) => creator(http, &CREATOR_POSTS, name, wall),
        Target::CreatorMedia(name, wall) => creator(http, &CREATOR_MEDIA, name, wall),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{children, http as client, json as body, keywords, media, serve};
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
            Some(Target::Home(false, None))
        );
        assert_eq!(
            t("https://fansly.com/home/subscribed"),
            Some(Target::Home(true, None))
        );
        assert_eq!(
            t("https://fansly.com/home/list/42"),
            Some(Target::Home(false, Some("42".into())))
        );
        assert_eq!(
            t("https://fansly.com/lists/42"),
            Some(Target::List("42".into()))
        );
        assert_eq!(t("https://fansly.com/lists"), Some(Target::Lists));
        assert_eq!(
            t("https://fansly.com/someone/posts/wall/77"),
            Some(Target::CreatorPosts("someone".into(), Some("77".into())))
        );
        assert_eq!(
            t("https://fansly.com/someone/media"),
            Some(Target::CreatorMedia("someone".into(), None))
        );
    }

    fn variant(id: &str, kind: i64, width: i64, url: &str) -> Value {
        json!({"id": id, "type": kind, "width": width, "mimetype": "image/jpeg",
               "locations": [{"location": url}]})
    }

    #[tokio::test]
    async fn post_variants_bundles_and_manifests() {
        let response = json!({"response": {
            "accounts": [{"id": "9", "username": "someone"}],
            "posts": [{"id": "100", "accountId": "9", "createdAt": 1_700_000_000,
                "attachments": [{"contentId": "m1"}, {"contentId": "b1"}]}],
            "accountMedia": [
                {"id": "m1", "media": {"id": "f1", "createdAt": 1, "updatedAt": 2, "mimetype": "image/jpeg",
                    "width": 100, "type": 1, "locations": [{"location": "https://cdn.fansly.com/f1.jpeg?x=1"}],
                    "variants": [variant("f1a", 1, 1920, "https://cdn.fansly.com/f1a.jpeg"),
                                 variant("f1b", 1, 480, "https://cdn.fansly.com/f1b.jpeg")]}},
                {"id": "m2", "media": {"id": "f2", "createdAt": 1, "updatedAt": 2, "mimetype": "video/mp4",
                    "width": 1280, "type": 2, "locations": [{"location": "https://cdn.fansly.com/f2.mp4"}],
                    "variants": [{"id": "f2h", "type": 302, "width": 1920, "mimetype": "application/vnd.apple.mpegurl",
                        "locations": [{"location": "https://cdn.fansly.com/f2.m3u8",
                            "metadata": {"Key-Pair-Id": "K", "Signature": "S", "Policy": "P"}}]}]}}],
            "accountMediaBundles": [{"id": "b1", "bundleContent": [
                {"accountMediaId": "m3", "pos": 1}, {"accountMediaId": "m2", "pos": 0}]}]}});
        let extra = json!({"response": [{"id": "m3", "media": {"id": "f3", "createdAt": 1, "updatedAt": 2,
            "mimetype": "image/png", "type": 1, "width": 10, "variants": [],
            "locations": [{"location": "https://cdn.fansly.com/f3.png"}]}}]});
        let (root, server) = serve(vec![body(response), body(extra)]);
        let http = client("fansly", root);
        let files = media(extract(
            &http,
            Url::parse("https://fansly.com/post/100").unwrap(),
        ))
        .await;
        let urls: Vec<&str> = files.iter().map(|f| f.url.as_str()).collect();
        assert_eq!(
            urls,
            [
                "https://cdn.fansly.com/f1a.jpeg",
                "ytdl:https://cdn.fansly.com/f2.m3u8",
                "https://cdn.fansly.com/f3.png"
            ]
        );
        assert_eq!(
            files[1].headers["Cookie"],
            "CloudFront-Key-Pair-Id=K; CloudFront-Signature=S; CloudFront-Policy=P"
        );
        assert_eq!(
            files[1].handoff["_fallback"],
            json!(["https://cdn.fansly.com/f2.mp4"])
        );
        let k = keywords(&files[0]);
        assert_eq!(
            (k["count"].as_i64(), k["account"]["username"].as_str()),
            (Some(3), Some("someone"))
        );
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "100_1_f1a.jpeg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            crate::sites::testing::os_dir("fansly/someone (9)")
        );
        let requests = server.join().unwrap();
        assert!(requests[1].contains("/api/v1/account/media?ids=m3"));
    }

    #[tokio::test]
    async fn lists_queue_accounts() {
        let (root, _server) = serve(vec![body(json!({"response": [
            {"id": "1", "label": "Faves"}, {"id": "2", "label": "Other"}]}))]);
        let http = client("fansly", root);
        let queued = children(extract(
            &http,
            Url::parse("https://fansly.com/lists").unwrap(),
        ))
        .await;
        assert_eq!(queued.len(), 2);
    }
}
