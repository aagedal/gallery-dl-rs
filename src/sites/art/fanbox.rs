//! pixivFANBOX creators, tags, posts, home and supporting feeds, and
//! pixiv redirects, mirroring gallery_dl/extractor/fanbox.py. Paid posts
//! need a `FANBOXSESSID` cookie.
use crate::error;
use crate::sites::gdl::{
    captures, date, extract_iter, group, int_of, nameext_into, parse_iso, re, truthy, unquote,
    ytdl_item,
};
use crate::sites::{Events, Http, Item, Route, Site};
use futures_util::StreamExt;
use serde_json::{Map, Value, json};
use url::Url;

const API: &str = "https://api.fanbox.cc";
const FILENAME: &str = "{id}_{num}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{creatorId}"];

macro_rules! routes {
    ($($ident:ident $sub:literal),*) => { $(static $ident: Route = Route::new($sub, FILENAME, DIRECTORY, "{id}_{num}");)* };
}
routes!(CREATOR "creator", TAG "tag", POST "post", HOME "home", SUPPORTING "supporting", REDIRECT "redirect");

pub static SITE: Site = Site {
    category: "fanbox",
    name: "pixivFANBOX",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&CREATOR, &TAG, &POST, &HOME, &SUPPORTING, &REDIRECT],
    api_key: false,
    cookies: Some("fanbox.cc"),
};

#[derive(Debug, PartialEq)]
enum Target {
    Creator(String),
    Tag(String, String),
    Post(String),
    Home,
    Supporting,
    Redirect(String),
}

fn target(url: &Url) -> Option<Target> {
    const USER: &str = r"^https?://(?:([\w-]+)\.fanbox\.cc|(?:www\.)?fanbox\.cc/@([\w-]+))";
    let creator = |c: &regex::Captures| {
        c.get(1)
            .or(c.get(2))
            .map_or(String::new(), |m| m.as_str().to_owned())
    };
    let www = |c: &regex::Captures| c.get(1).is_some_and(|m| m.as_str() == "www");
    if let Some(c) = captures(re!(&format!(r"{USER}(?:/posts)?/?$")), url).filter(|c| !www(c)) {
        return Some(Target::Creator(creator(&c)));
    }
    if let Some(c) = captures(re!(&format!(r"{USER}/tags/([^/?#]+)")), url).filter(|c| !www(c)) {
        return Some(Target::Tag(creator(&c), group(&c, 3).into()));
    }
    if let Some(c) = captures(re!(&format!(r"{USER}/posts/(\d+)")), url).filter(|c| !www(c)) {
        return Some(Target::Post(group(&c, 3).into()));
    }
    const BASE: &str = r"^https?://(?:www\.)?fanbox\.cc";
    if captures(re!(&format!(r"{BASE}/?$")), url).is_some() {
        return Some(Target::Home);
    }
    if captures(re!(&format!(r"{BASE}/home/supporting")), url).is_some() {
        return Some(Target::Supporting);
    }
    captures(
        re!(r"^https?://(?:www\.)?pixiv\.net/fanbox/creator/(\d+)"),
        url,
    )
    .map(|c| Target::Redirect(group(&c, 1).into()))
}

async fn api(http: &Http, url: &str) -> crate::Result<Value> {
    let headers = [
        ("Accept", "application/json, text/plain, */*"),
        ("Origin", "https://www.fanbox.cc"),
        ("Referer", "https://www.fanbox.cc/"),
        ("Sec-Fetch-Dest", "empty"),
        ("Sec-Fetch-Mode", "cors"),
        ("Sec-Fetch-Site", "same-site"),
    ];
    let url = if url.starts_with("//") {
        format!("https:{url}")
    } else {
        url.to_owned()
    };
    Ok(http.json_with(&http.api(&url)?, &headers).await?["body"].clone())
}

/// The list or object inside a response body, for both the shapes upstream
/// knows and FANBOX's newer wrapped ones (`pageUrls`, `posts`, `post`).
fn unwrap_body(body: Value, key: &str) -> Value {
    match body {
        Value::Object(mut map) if map.contains_key(key) && !map.contains_key("id") => {
            map.remove(key).unwrap_or(Value::Null)
        }
        other => other,
    }
}

/// Which optional metadata (`user`, `plan`, `comments`) to fetch.
fn includes(http: &Http) -> (bool, bool, bool) {
    let list: Vec<String> = match http.option("metadata") {
        None | Some(Value::Null) | Some(Value::Bool(false)) => Vec::new(),
        Some(Value::String(s)) => s.split(',').map(str::to_owned).collect(),
        Some(Value::Array(list)) => list.iter().map(crate::sites::scalar).collect(),
        Some(_) => vec!["user".into(), "plan".into()],
    };
    let has = |key: &str| list.iter().any(|k| k == key);
    (
        has("user"),
        has("plan"),
        has("comments") || truthy(http.option("comments")),
    )
}

/// Reorder an id-keyed map by `ids` (`_sort_map`).
fn sort_map(body: &mut Map<String, Value>, key: &str, ids: &[String]) -> Map<String, Value> {
    let Some(orig) = body.get(key).and_then(Value::as_object).cloned() else {
        return Map::new();
    };
    if orig.is_empty() {
        return orig;
    }
    let sorted: Map<String, Value> = ids
        .iter()
        .filter_map(|id| Some((id.clone(), orig.get(id)?.clone())))
        .collect();
    body.insert(key.into(), Value::Object(sorted.clone()));
    sorted
}

/// `_extract_post`: post fields and its content body.
async fn extract_post(
    http: &Http,
    mut post: Map<String, Value>,
) -> crate::Result<(Option<Map<String, Value>>, Map<String, Value>)> {
    post.insert("archives".into(), json!([]));
    let mut body = post.remove("body").and_then(|b| b.as_object().cloned());
    if let Some(body) = body.as_mut() {
        if let Some(html) = body.get("html") {
            post.insert("html".into(), html.clone());
        }
        if post.get("type").and_then(Value::as_str) == Some("article") {
            post.insert("articleBody".into(), Value::Object(body.clone()));
        }
        if let Some(blocks) = body.get("blocks").and_then(Value::as_array).cloned() {
            let (mut content, mut images, mut files) = (Vec::new(), Vec::new(), Vec::new());
            for block in &blocks {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    content.push(text.to_owned());
                }
                for link in block
                    .get("links")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    content.push(crate::sites::scalar(&link["url"]));
                }
                if let Some(id) = block.get("imageId") {
                    images.push(crate::sites::scalar(id));
                }
                if let Some(id) = block.get("fileId") {
                    files.push(crate::sites::scalar(id));
                }
            }
            post.insert("content".into(), content.join("\n").into());
            sort_map(body, "imageMap", &images);
            let file_map = sort_map(body, "fileMap", &files);
            let archives: Vec<Value> = file_map
                .values()
                .filter(|f| {
                    ["zip", "rar", "7z", "tar", "gz", "bz2", "lzma", "xz"].contains(
                        &f["extension"]
                            .as_str()
                            .unwrap_or_default()
                            .to_lowercase()
                            .as_str(),
                    )
                })
                .cloned()
                .collect();
            if !file_map.is_empty() {
                post.insert("archives".into(), archives.into());
            }
        }
    }
    let published = parse_iso(
        post.get("publishedDatetime")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    post.insert(
        "date".into(),
        published.map_or(Value::Null, |ts| date(Some(ts))),
    );
    post.insert(
        "text".into(),
        body.as_ref()
            .and_then(|b| b.get("text").cloned())
            .unwrap_or(Value::Null),
    );
    post.insert("isCoverImage".into(), false.into());
    let (meta_user, meta_plan, meta_comments) = includes(http);
    if let Some(cid) = post.get("creatorId").map(crate::sites::scalar) {
        if meta_user {
            let mut user = api(http, &format!("{API}/creator.get?creatorId={cid}")).await?;
            if let Some(map) = user.as_object_mut() {
                let inner = map
                    .remove("user")
                    .and_then(|u| u.as_object().cloned())
                    .unwrap_or_default();
                map.extend(inner);
            }
            post.insert("user".into(), user);
        }
        if meta_plan {
            let mut plans: Vec<(i64, Value)> = vec![(
                0,
                json!({"id": "", "title": "", "fee": 0, "description": "", "coverImageUrl": "", "creatorId": cid, "hasAdultContent": null, "paymentMethod": null}),
            )];
            for mut plan in api(http, &format!("{API}/plan.listCreator?creatorId={cid}"))
                .await?
                .as_array()
                .cloned()
                .unwrap_or_default()
            {
                if let Some(map) = plan.as_object_mut() {
                    map.remove("user");
                }
                plans.push((int_of(plan.get("fee")), plan));
            }
            let fee = int_of(post.get("feeRequired"));
            let plan = match plans.iter().rev().find(|(f, _)| *f == fee) {
                Some((_, plan)) => plan.clone(),
                None => match plans
                    .iter()
                    .filter(|(f, _)| *f >= fee)
                    .min_by_key(|(f, _)| *f)
                {
                    Some((_, plan)) => plan.clone(),
                    None => {
                        let mut plan = plans[0].1.clone();
                        plan["fee"] = fee.into();
                        plan
                    }
                },
            };
            post.insert("plan".into(), plan);
        }
    }
    if meta_comments {
        let mut comments = Vec::new();
        if int_of(post.get("commentCount")) > 0 {
            let mut url = Some(format!(
                "{API}/post.getComments?limit=10&postId={}",
                crate::sites::scalar(&post["id"])
            ));
            while let Some(next) = url {
                let Ok(list) = api(http, &next).await else {
                    break;
                };
                comments.extend(
                    list["commentList"]["items"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default(),
                );
                url = list["commentList"]["nextUrl"].as_str().map(str::to_owned);
            }
        }
        post.insert("comments".into(), comments.into());
    }
    Ok((body, post))
}

/// One file of a post (`fileUrl`, name, numbering); fanbox downloads carry
/// the session cookie.
fn file_item(
    http: &Http,
    post: &Map<String, Value>,
    url: &str,
    num: usize,
    extra: Map<String, Value>,
) -> Option<Item> {
    let mut fields = post.clone();
    fields.insert("fileUrl".into(), url.into());
    nameext_into(url, &mut fields);
    fields.extend(extra);
    fields.insert("num".into(), num.into());
    let file = Url::parse(url).ok()?;
    let cookie = file
        .host_str()
        .is_some_and(|h| h.ends_with("fanbox.cc"))
        .then(|| http.cookie_header(&file))
        .flatten();
    let id = crate::sites::scalar(&post["id"]);
    let mut item = Item::new(file, fields).ids(id, num.to_string());
    if let Some(cookie) = cookie {
        item = item.header("Cookie", &cookie);
    }
    Some(item)
}

/// `_get_urls_from_post` and `_process_embed`.
async fn post_events<'a>(
    http: &'a Http,
    route: &'static Route,
    item: Value,
) -> crate::Result<Events<'a>> {
    let fee_max = http.option("fee-max").map(|v| int_of(Some(v)));
    let id = crate::sites::scalar(&item["id"]);
    let search_tags = item.get("search_tags").cloned();
    let mut item = match fee_max {
        Some(max) if max < int_of(item.get("feeRequired")) => item,
        _ => api(http, &format!("{API}/post.info?postId={id}"))
            .await
            .map(|body| unwrap_body(body, "post"))
            .unwrap_or(item),
    };
    if let Some(map) = item.as_object_mut() {
        if let Some(tags) = search_tags {
            map.insert("search_tags".into(), tags);
        }
        // Newer listings carry `cover` instead of `coverImageUrl`.
        if !map.contains_key("coverImageUrl")
            && let Some(url) = map
                .get("cover")
                .filter(|c| c["type"] == "cover_image")
                .and_then(|c| c["url"].as_str())
                .map(str::to_owned)
        {
            map.insert("coverImageUrl".into(), url.into());
        }
    }
    let (body, post) = extract_post(http, item.as_object().cloned().unwrap_or_default()).await?;
    let mut items = Vec::new();
    let mut num = 0;
    if let Some(cover) = post
        .get("coverImageUrl")
        .and_then(Value::as_str)
        .filter(|c| !c.is_empty())
    {
        let cover = re!(r"/c/[0-9a-z_]+").replace(cover, "").into_owned();
        let mut extra = Map::new();
        extra.insert("isCoverImage".into(), true.into());
        items.extend(file_item(http, &post, &cover, num, extra));
        num += 1;
    }
    let mut children = Vec::new();
    if let Some(body) = body {
        if let Some(html) = body.get("html").and_then(Value::as_str) {
            let mut urls: Vec<&str> = extract_iter(html, "href=\"", "\"")
                .into_iter()
                .filter(|h| {
                    h.contains("fanbox.pixiv.net/images/entry") || h.contains("downloads.fanbox.cc")
                })
                .collect();
            urls.extend(extract_iter(html, "data-src-original=\"", "\""));
            for url in urls {
                items.extend(file_item(http, &post, url, num, Map::new()));
                num += 1;
            }
        }
        for group in ["images", "imageMap"] {
            let entries: Vec<Value> = match body.get(group) {
                Some(Value::Array(list)) => list.clone(),
                Some(Value::Object(map)) => map.values().cloned().collect(),
                _ => continue,
            };
            for image in entries {
                let url = crate::sites::scalar(&image["originalUrl"]);
                let mut extra = Map::new();
                if let Some(ext) = image.get("extension") {
                    extra.insert("extension".into(), ext.clone());
                }
                for (key, source) in [("fileId", "id"), ("width", "width"), ("height", "height")] {
                    extra.insert(
                        key.into(),
                        image.get(source).cloned().unwrap_or(Value::Null),
                    );
                }
                items.extend(file_item(http, &post, &url, num, extra));
                num += 1;
            }
        }
        for group in ["files", "fileMap"] {
            let entries: Vec<Value> = match body.get(group) {
                Some(Value::Array(list)) => list.clone(),
                Some(Value::Object(map)) => map.values().cloned().collect(),
                _ => continue,
            };
            for file in entries {
                let url = crate::sites::scalar(&file["url"]);
                let mut extra = Map::new();
                if let Some(ext) = file.get("extension") {
                    extra.insert("extension".into(), ext.clone());
                }
                if let Some(name) = file.get("name") {
                    extra.insert("filename".into(), name.clone());
                }
                extra.insert(
                    "fileId".into(),
                    file.get("id").cloned().unwrap_or(Value::Null),
                );
                items.extend(file_item(http, &post, &url, num, extra));
                num += 1;
            }
        }
        let embeds = http.option("embeds").cloned().unwrap_or(Value::Bool(true));
        if truthy(Some(&embeds)) {
            let ytdl = embeds.as_str() == Some("ytdl");
            let mut found: Vec<Value> = body.get("video").cloned().into_iter().collect();
            found.extend(
                body.get("embedMap")
                    .and_then(Value::as_object)
                    .into_iter()
                    .flat_map(|m| m.values().cloned()),
            );
            for embed in found {
                let content_id = crate::sites::scalar(
                    embed
                        .get("videoId")
                        .filter(|v| !v.is_null())
                        .or(embed.get("contentId"))
                        .unwrap_or(&Value::Null),
                );
                let prefix = if ytdl { "ytdl:" } else { "" };
                let (url, video) = match embed["serviceProvider"].as_str().unwrap_or_default() {
                    "soundcloud" => (
                        Some(format!("{prefix}https://soundcloud.com/{content_id}")),
                        true,
                    ),
                    "youtube" => (
                        Some(format!("{prefix}https://youtube.com/watch?v={content_id}")),
                        true,
                    ),
                    "vimeo" => (
                        Some(format!("{prefix}https://vimeo.com/{content_id}")),
                        true,
                    ),
                    "fanbox" => (
                        http.location(
                            &http.api(&format!("https://www.pixiv.net/fanbox/{content_id}"))?,
                            &[],
                        )
                        .await
                        .ok()
                        .flatten()
                        .map(|u| u.to_string()),
                        false,
                    ),
                    "twitter" => (
                        Some(format!("https://twitter.com/_/status/{content_id}")),
                        false,
                    ),
                    "google_forms" => (
                        Some(format!(
                            "https://docs.google.com/forms/d/e/{content_id}/viewform?usp=sf_link"
                        )),
                        false,
                    ),
                    _ => (None, false),
                };
                let Some(url) = url else { continue };
                let mut fields = post.clone();
                fields.insert("embed".into(), embed.clone());
                fields.insert("embedUrl".into(), url.as_str().into());
                nameext_into(&url, &mut fields);
                fields.insert("num".into(), num.into());
                num += 1;
                if video && ytdl {
                    let page = url.trim_start_matches("ytdl:");
                    items.extend(
                        ytdl_item(page, fields).map(|i| i.ids(id.clone(), (num - 1).to_string())),
                    );
                } else if let Ok(url) = Url::parse(&url) {
                    children.push((url, fields));
                }
            }
        }
    }
    let media = crate::sites::events(crate::sites::post(&SITE, http, route, items));
    Ok(media
        .chain(crate::sites::galleries::queue(http, &SITE, route, children))
        .boxed())
}

/// `_pagination`: items across `nextUrl` links.
fn pagination<'a>(http: &'a Http, route: &'static Route, first: String) -> Events<'a> {
    crate::sites::paginate(Some(first), move |state| async move {
        let Some(url) = state else { return Ok(None) };
        let body = api(http, &url).await?;
        let items = body["items"].as_array().cloned().unwrap_or_default();
        let next = body["nextUrl"]
            .as_str()
            .filter(|u| !u.is_empty())
            .map(str::to_owned);
        Ok(Some((
            crate::sites::each_stream(items, move |item| post_events(http, route, item)),
            next,
        )))
    })
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported FANBOX URL"));
    };
    match target {
        Target::Post(id) => crate::sites::once(async move {
            post_events(http, &POST, json!({"id": id, "feeRequired": 0})).await
        }),
        Target::Creator(creator) => crate::sites::once(async move {
            let pages = api(
                http,
                &format!("{API}/post.paginateCreator?creatorId={creator}"),
            )
            .await?;
            let mut urls: Vec<String> = unwrap_body(pages, "pageUrls")
                .as_array()
                .into_iter()
                .flatten()
                .map(crate::sites::scalar)
                .collect();
            let offset = int_of(http.option("offset")).max(0) as usize;
            let (quotient, remainder) = (offset / 10, offset % 10);
            urls.drain(..quotient.min(urls.len()));
            Ok(crate::sites::paginate(
                Some((urls, remainder)),
                move |state| async move {
                    let Some((mut urls, remainder)) = state.filter(|(u, _)| !u.is_empty()) else {
                        return Ok(None);
                    };
                    let url = urls.remove(0);
                    let mut posts = unwrap_body(api(http, &url).await?, "posts")
                        .as_array()
                        .cloned()
                        .unwrap_or_default();
                    posts.drain(..remainder.min(posts.len()));
                    Ok(Some((
                        crate::sites::each_stream(posts, move |item| {
                            post_events(http, &CREATOR, item)
                        }),
                        Some((urls, 0)),
                    )))
                },
            ))
        }),
        Target::Tag(creator, tag) => {
            // Upstream sets `search_tags` for the whole job.
            let tags = unquote(&tag);
            crate::sites::paginate(
                Some(format!(
                    "{API}/post.listTagged?creatorId={creator}&tag={tag}"
                )),
                move |state| {
                    let tags = tags.clone();
                    async move {
                        let Some(url) = state else { return Ok(None) };
                        let body = api(http, &url).await?;
                        let items: Vec<Value> = body["items"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .map(|item| {
                                let mut item = item.clone();
                                item["search_tags"] = tags.as_str().into();
                                item
                            })
                            .collect();
                        let next = body["nextUrl"]
                            .as_str()
                            .filter(|u| !u.is_empty())
                            .map(str::to_owned);
                        Ok(Some((
                            crate::sites::each_stream(items, move |item| {
                                post_events(http, &TAG, item)
                            }),
                            next,
                        )))
                    }
                },
            )
        }
        Target::Home => pagination(http, &HOME, format!("{API}/post.listHome?limit=10")),
        Target::Supporting => pagination(
            http,
            &SUPPORTING,
            format!("{API}/post.listSupporting?limit=10"),
        ),
        Target::Redirect(id) => crate::sites::once(async move {
            let location = http
                .location(
                    &http.api(&format!("https://www.pixiv.net/fanbox/creator/{id}"))?,
                    &[],
                )
                .await?
                .ok_or_else(|| error("FANBOX user not found"))?;
            Ok(crate::sites::galleries::queue(
                http,
                &SITE,
                &REDIRECT,
                vec![(location, Map::new())],
            ))
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http as client, json as body, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://xub.fanbox.cc"),
            Some(Target::Creator("xub".into()))
        );
        assert_eq!(
            t("https://www.fanbox.cc/@xub/posts"),
            Some(Target::Creator("xub".into()))
        );
        assert_eq!(
            t("https://mochirong.fanbox.cc/tags/%E6%9C%88%E4%BE%8B"),
            Some(Target::Tag("mochirong".into(), "%E6%9C%88%E4%BE%8B".into()))
        );
        assert_eq!(
            t("https://www.fanbox.cc/@xub/posts/1910054"),
            Some(Target::Post("1910054".into()))
        );
        assert_eq!(t("https://www.fanbox.cc/"), Some(Target::Home));
        assert_eq!(
            t("https://www.fanbox.cc/home/supporting"),
            Some(Target::Supporting)
        );
        assert_eq!(
            t("https://www.pixiv.net/fanbox/creator/52336352"),
            Some(Target::Redirect("52336352".into()))
        );
    }

    #[tokio::test]
    async fn article_blocks_in_body_order() {
        let post = json!({"body": {"id": "1910054", "type": "article", "creatorId": "xub", "feeRequired": 0, "publishedDatetime": "2021-03-05T13:40:57+09:00",
            "coverImageUrl": "https://pixiv.pximg.net/c/1200x630_90_a2_g5/fanbox/public/images/post/1910054/cover/abc.jpeg",
            "body": {"blocks": [{"type": "p", "text": "Hi"}, {"type": "image", "imageId": "b"}, {"type": "image", "imageId": "a"}, {"type": "file", "fileId": "f"}],
                "imageMap": {"a": {"id": "a", "originalUrl": "https://downloads.fanbox.cc/images/post/1910054/a.png", "extension": "png", "width": 1, "height": 2},
                             "b": {"id": "b", "originalUrl": "https://downloads.fanbox.cc/images/post/1910054/b.jpeg", "extension": "jpeg", "width": 3, "height": 4}},
                "fileMap": {"f": {"id": "f", "name": "pack", "extension": "zip", "url": "https://downloads.fanbox.cc/files/post/1910054/f.zip"}}}}});
        let (root, server) = serve(vec![body(post)]);
        let http = client("fanbox", root);
        let files = media(extract(
            &http,
            Url::parse("https://xub.fanbox.cc/posts/1910054").unwrap(),
        ))
        .await;
        let urls: Vec<&str> = files.iter().map(|f| f.url.as_str()).collect();
        assert_eq!(
            urls,
            [
                "https://pixiv.pximg.net/fanbox/public/images/post/1910054/cover/abc.jpeg",
                "https://downloads.fanbox.cc/images/post/1910054/b.jpeg",
                "https://downloads.fanbox.cc/images/post/1910054/a.png",
                "https://downloads.fanbox.cc/files/post/1910054/f.zip"
            ]
        );
        let k = keywords(&files[3]);
        assert_eq!(
            (
                k["content"].as_str(),
                k["filename"].as_str(),
                k["archives"][0]["id"].as_str()
            ),
            (Some("Hi"), Some("pack"), Some("f"))
        );
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "1910054_0.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("fanbox/xub")
        );
        server.join().unwrap();
    }
}
