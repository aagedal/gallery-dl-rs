//! Patreon collections, creators, the followed-creators stream and single
//! posts, mirroring gallery_dl/extractor/patreon.py. Paid posts need a
//! `session_id` cookie.
use crate::error;
use crate::event::ExtractionEvent;
use crate::extract::{ManifestProtocol, MediaJob};
use crate::sites::gdl::{
    captures, date, ensure_http_scheme, ext_from_url, extr, extract_iter,
    filename_from_contentdisposition, group, html_escape, json_after, nameext_into, nested_date,
    next_flight, nextdata, parse_int, parse_iso, parse_query, re, truthy, unescape,
};
use crate::sites::{Events, Http, Item, Route, Site, scalar};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use url::Url;

const ROOT: &str = "https://www.patreon.com";
const USER_AGENT: &str = "Patreon/126.9.0.15 (Android; Android 14; Scale/2.10)";
const FILENAME: &str = "{id}_{title}_{num:>02}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{creator[full_name]}"];

static COLLECTION: Route = Route::new(
    "collection",
    FILENAME,
    &[
        "{category}",
        "{creator[full_name]}",
        "Collections",
        "{collection[title]} ({collection[id]})",
    ],
    "{id}_{num}",
);
static CREATOR: Route = Route::new("creator", FILENAME, DIRECTORY, "{id}_{num}");
static USER: Route = Route::new("user", FILENAME, DIRECTORY, "{id}_{num}");
static POST: Route = Route::new("post", FILENAME, DIRECTORY, "{id}_{num}");

pub static SITE: Site = Site {
    category: "patreon",
    name: "Patreon",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&COLLECTION, &CREATOR, &USER, &POST],
    api_key: false,
    cookies: Some("patreon.com"),
};

#[derive(Debug, PartialEq)]
enum Target {
    Collection(String),
    /// Creator name (if any) and query string.
    Creator(String, String),
    User,
    Post(String),
}

fn target(url: &Url) -> Option<Target> {
    const BASE: &str = r"^https?://(?:www\.)?patreon\.com";
    if let Some(c) = captures(re!(&format!(r"{BASE}/collection/(\d+)")), url) {
        return Some(Target::Collection(group(&c, 1).into()));
    }
    // Upstream's `(?!(?:home|create|...)(?:$|[/?#]))`.
    let first = url
        .path()
        .trim_start_matches('/')
        .split('/')
        .next()
        .unwrap_or_default();
    let reserved = [
        "home", "create", "login", "signup", "search", "posts", "messages",
    ];
    if !reserved.contains(&first)
        && let Some(c) = captures(
            re!(&format!(
                r"{BASE}/(?:profile/creators|(?:cw?/)?([^/?#]+)(?:/posts)?)/?(?:\?([^#]+))?"
            )),
            url,
        )
    {
        return Some(Target::Creator(group(&c, 1).into(), group(&c, 2).into()));
    }
    if captures(re!(&format!(r"{BASE}/home$")), url).is_some() {
        return Some(Target::User);
    }
    captures(re!(&format!(r"{BASE}/posts/([^/?#]+)")), url)
        .map(|c| Target::Post(group(&c, 1).into()))
}

fn logged_in(http: &Http) -> bool {
    Url::parse(ROOT).is_ok_and(|url| http.cookie(&url, "session_id").is_some())
}

/// `_extract_bootstrap`: Next.js data, else one of the inline variants.
fn bootstrap(page: &str) -> crate::Result<Value> {
    if let Ok(data) = nextdata(page) {
        let env = &data["props"]["pageProps"]["bootstrapEnvelope"];
        let found = Some(&env["pageBootstrap"])
            .filter(|b| truthy(Some(b)))
            .or_else(|| env.get("bootstrap"));
        if let Some(found) = found.filter(|f| !f.is_null()) {
            return Ok(found.clone());
        }
    }
    for begin in [
        "window.patreon = {\"bootstrap\":",
        "window.patreon = wrapInProxy({\"bootstrap\":",
    ] {
        let raw = extr(page, begin, "},\"apiServer\"");
        if !raw.is_empty() {
            return serde_json::from_str(&format!("{raw}}}"))
                .map_err(|_| error("Unable to extract bootstrap data"));
        }
    }
    let raw = extr(page, "window.patreon.bootstrap,", "});");
    if !raw.is_empty() {
        return serde_json::from_str(&format!("{raw}}}"))
            .map_err(|_| error("Unable to extract bootstrap data"));
    }
    let raw = extr(page, "window.patreon = {", "};\n");
    if let Ok(mut data) = serde_json::from_str::<Value>(&format!("{{{raw}}}"))
        && !raw.is_empty()
    {
        return Ok(data["bootstrap"].take());
    }
    // Newer App Router pages only carry it in their flight data.
    let flight = next_flight(page);
    if let Some(env) = json_after(&flight, "{\"bootstrapEnvelope\":") {
        let found = Some(&env["pageBootstrap"])
            .filter(|b| truthy(Some(b)))
            .unwrap_or(&env["bootstrap"]);
        if !found.is_null() {
            return Ok(found.clone());
        }
    }
    Err(error("Unable to extract bootstrap data"))
}

/// `utils/patreon_tiptap.to_html`.
fn tiptap(markup: &str) -> Option<String> {
    let data: Value = serde_json::from_str(markup).ok()?;
    let mut html = String::new();
    for block in data["content"].as_array()? {
        tiptap_content(&mut html, block);
    }
    Some(html)
}

fn tiptap_children(html: &mut String, content: &Value) {
    for block in content["content"].as_array().into_iter().flatten() {
        tiptap_content(html, block);
    }
}

fn tiptap_content(html: &mut String, content: &Value) {
    let attrs = &content["attrs"];
    let attr = |key: &str| Some(&attrs[key]).filter(|v| truthy(Some(v))).map(scalar);
    match content["type"].as_str().unwrap_or_default() {
        "paragraph" => {
            html.push_str("<p>");
            tiptap_children(html, content);
            html.push_str("</p>");
        }
        "text" => tiptap_text(html, content),
        "heading" => {
            let level = attr("level").unwrap_or_else(|| "3".into());
            html.push_str(&format!("<h{level}>"));
            tiptap_children(html, content);
            html.push_str(&format!("</h{level}>"));
        }
        kind @ ("listItem" | "bulletList" | "orderedList" | "blockquote") => {
            let tag = match kind {
                "listItem" => "li",
                "bulletList" => "ul",
                "orderedList" => "ol",
                _ => "blockquote",
            };
            html.push_str(&format!("<{tag}>"));
            tiptap_children(html, content);
            html.push_str(&format!("</{tag}>"));
        }
        "image" => {
            if let Some(src) = attr("src") {
                html.push_str("<div data-image-container=\"true\"");
                if let Some(align) = attr("alignment") {
                    html.push_str(&format!(" data-alignment=\"{}\"", html_escape(&align)));
                }
                html.push_str(&format!("><figure><img src=\"{}\"", html_escape(&src)));
                if let Some(mid) = attr("media_id") {
                    html.push_str(&format!(" data-media-id=\"{}\"", html_escape(&mid)));
                }
                html.push_str("/></figure></div>");
            }
        }
        "link" => {
            if let Some(href) = attr("href") {
                html.push_str(&format!("<a href=\"{}\">", html_escape(&href)));
                tiptap_children(html, content);
                html.push_str("</a>");
            }
        }
        "hardBreak" => html.push_str("<br/>"),
        "horizontalRule" => html.push_str("<hr/>"),
        _ => {}
    }
}

fn tiptap_text(html: &mut String, content: &Value) {
    let mut close = Vec::new();
    for mark in content["marks"].as_array().into_iter().flatten() {
        let (open, end) = match mark["type"].as_str().unwrap_or_default() {
            "link" => {
                let attrs = &mark["attrs"];
                let mut open = format!(
                    "<a href=\"{}\"",
                    html_escape(
                        &attrs
                            .get("href")
                            .filter(|h| truthy(Some(h)))
                            .map(scalar)
                            .unwrap_or_default()
                    )
                );
                for key in ["target", "rel"] {
                    if let Some(value) = attrs.get(key) {
                        open.push_str(&format!(" {key}=\"{}\"", scalar(value)));
                    }
                }
                open.push('>');
                (open, "</a>")
            }
            "bold" => ("<strong>".into(), "</strong>"),
            "italic" => ("<em>".into(), "</em>"),
            "underline" => ("<u>".into(), "</u>"),
            "strike" => ("<s>".into(), "</s>"),
            _ => continue,
        };
        html.push_str(&open);
        close.push(end);
    }
    html.push_str(&html_escape(content["text"].as_str().unwrap_or_default()));
    for end in close.into_iter().rev() {
        html.push_str(end);
    }
}

/// `_transform`: included objects by type and id.
type Included = HashMap<String, HashMap<String, Value>>;

fn transform(included: &Value) -> Included {
    let mut result: Included = HashMap::new();
    for inc in included.as_array().into_iter().flatten() {
        result
            .entry(scalar(&inc["type"]))
            .or_default()
            .insert(scalar(&inc["id"]), inc["attributes"].clone());
    }
    result
}

fn files(post: &Value, included: &Included, key: &str) -> Value {
    post["relationships"][key]["data"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|file| {
            included
                .get(&scalar(&file["type"]))
                .and_then(|kind| kind.get(&scalar(&file["id"])))
                .cloned()
                .unwrap_or(Value::Null)
        })
        .collect::<Vec<_>>()
        .into()
}

/// `self.cache(self._user, ...)`: creator lookups by API URL.
static USERS: Mutex<Option<HashMap<String, Option<Value>>>> = Mutex::new(None);

/// `_user`: creator details, or `None` for error responses.
async fn fetch_user(http: &Http, url: &str) -> Option<Value> {
    if let Some(hit) = USERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_default()
        .get(url)
    {
        return hit.clone();
    }
    let result = async {
        let (status, body) = http
            .get_status(&http.api(url).ok()?, &[("User-Agent", USER_AGENT)])
            .await
            .ok()?;
        if status >= 400 {
            return None;
        }
        let mut user: Value = serde_json::from_str(&body).ok()?;
        let user = user["data"].take();
        let mut attr = user["attributes"].as_object()?.clone();
        attr.insert("id".into(), user["id"].clone());
        let created = attr
            .get("created")
            .and_then(Value::as_str)
            .and_then(parse_iso);
        attr.insert("date".into(), nested_date(created));
        Some(Value::Object(attr))
    }
    .await;
    USERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_default()
        .insert(url.to_owned(), result.clone());
    result
}

/// `_process`: a post's attributes extended with its relationships.
async fn process(http: &Http, post: &Value, included: &Included) -> Map<String, Value> {
    let mut attr = post["attributes"].as_object().cloned().unwrap_or_default();
    attr.insert("id".into(), parse_int(&scalar(&post["id"])).into());
    let relationships = &post["relationships"];
    for key in ["images", "attachments", "attachments_media"] {
        attr.insert(key.into(), files(post, included, key));
    }
    let published = attr
        .get("published_at")
        .and_then(Value::as_str)
        .and_then(parse_iso);
    attr.insert("date".into(), date(published));
    let campaign = included
        .get("campaign")
        .and_then(|c| c.get(&scalar(&relationships["campaign"]["data"]["id"])))
        .cloned()
        .unwrap_or(Value::Null);
    attr.insert("campaign".into(), campaign);
    let tags: Vec<Value> = relationships["user_defined_tags"]["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|tag| tag["type"] == "post_tag")
        .map(|tag| scalar(&tag["id"]).replace("user_defined;", "").into())
        .collect();
    attr.insert("tags".into(), tags.into());
    let user = &relationships["user"];
    let related = scalar(&user["links"]["related"]);
    let creator = match fetch_user(http, &related).await {
        Some(creator) => creator,
        None => included
            .get("user")
            .and_then(|u| u.get(&scalar(&user["data"]["id"])))
            .cloned()
            .unwrap_or(Value::Null),
    };
    attr.insert("creator".into(), creator);
    if !truthy(attr.get("content"))
        && let Some(cjs) = attr.remove("content_json_string")
        && let Some(html) = cjs.as_str().and_then(tiptap)
    {
        attr.insert("content".into(), html.into());
    }
    attr
}

/// `_filename`: the name from a HEAD request's Content-Disposition.
async fn head_filename(http: &Http, url: &str) -> String {
    let Ok(url) = http.api(url) else {
        return String::new();
    };
    match http.head(&url, &[("User-Agent", USER_AGENT)]).await {
        Ok((_, headers)) => headers
            .get("content-disposition")
            .and_then(|v| v.to_str().ok())
            .map(filename_from_contentdisposition)
            .unwrap_or_default(),
        Err(_) => String::new(),
    }
}

/// `name or self._filename(url) or url`.
async fn name_for(http: &Http, name: Option<&Value>, url: &str) -> String {
    if let Some(name) = name.filter(|n| truthy(Some(n))) {
        return scalar(name);
    }
    let name = head_filename(http, url).await;
    if name.is_empty() {
        url.to_owned()
    } else {
        name
    }
}

/// One file candidate: `(kind, file, url, name)`.
type File = (&'static str, Value, String, String);

/// The `files` generators, in their configured order.
async fn post_files(http: &Http, post: &Map<String, Value>) -> Vec<File> {
    let kinds: Vec<String> = match http.option("files") {
        Some(Value::String(s)) => s.split(',').map(str::to_owned).collect(),
        Some(Value::Array(list)) => list.iter().map(scalar).collect(),
        _ => [
            "images",
            "image_large",
            "attachments",
            "postfile",
            "content",
        ]
        .map(str::to_owned)
        .to_vec(),
    };
    let images_fmt = http.option_str("format-images").filter(|f| !f.is_empty());
    let mut out = Vec::new();
    for kind in kinds {
        match kind.as_str() {
            "images" => {
                for image in post
                    .get("images")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let url = images_fmt
                        .and_then(|fmt| image["image_urls"].get(fmt))
                        .or_else(|| image.get("download_url"))
                        .filter(|u| truthy(Some(u)))
                        .map(scalar);
                    if let Some(url) = url {
                        let name = name_for(http, image.get("file_name"), &url).await;
                        out.push(("image", image.clone(), url, name));
                    }
                }
            }
            "image_large" => {
                if let Some(image) = post.get("image").filter(|i| truthy(Some(i)))
                    && let Some(url) = image.get("large_url").filter(|u| truthy(Some(u)))
                {
                    let url = scalar(url);
                    let name = name_for(http, image.get("file_name"), &url).await;
                    out.push(("image_large", image.clone(), url, name));
                }
            }
            "attachments" => {
                for attachment in post
                    .get("attachments")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let Ok(source) = http.api(&scalar(&attachment["url"])) else {
                        continue;
                    };
                    if let Ok(Some(url)) =
                        http.location(&source, &[("User-Agent", USER_AGENT)]).await
                    {
                        out.push((
                            "attachment",
                            attachment.clone(),
                            url.to_string(),
                            scalar(&attachment["name"]),
                        ));
                    }
                }
                for attachment in post
                    .get("attachments_media")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if let Some(url) = attachment.get("download_url").filter(|u| truthy(Some(u))) {
                        out.push((
                            "attachment",
                            attachment.clone(),
                            scalar(url),
                            scalar(&attachment["file_name"]),
                        ));
                    }
                }
            }
            "postfile" => {
                if let Some(postfile) = post.get("post_file").filter(|p| truthy(Some(p))) {
                    let url = scalar(&postfile["url"]);
                    let name = match postfile.get("name").filter(|n| truthy(Some(n))) {
                        Some(name) => scalar(name),
                        None if url.starts_with("https://stream.mux.com/") => url.clone(),
                        None => name_for(http, None, &url).await,
                    };
                    out.push(("postfile", postfile.clone(), url, name));
                }
            }
            "content" => {
                let content = post
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                for img in extract_iter(content, "><figure><img src=\"", ">") {
                    let url = unescape(&img[..img.find('"').unwrap_or(img.len())]);
                    let data = serde_json::json!({"media_id": extr(img, "media_id=\"", "\"")});
                    let name = name_for(http, None, &url).await;
                    out.push(("content", data, url, name));
                }
            }
            _ => {}
        }
    }
    out
}

/// `_filehash`: the 32-character path component of a download URL.
fn filehash(url: &str) -> String {
    url.split('?')
        .next()
        .unwrap_or_default()
        .rsplit('/')
        .find(|part| part.len() == 32)
        .unwrap_or_default()
        .to_owned()
}

/// `items` for one processed post.
async fn post_events(
    http: &Http,
    route: &'static Route,
    collection: Option<&Value>,
    mut post: Map<String, Value>,
) -> Vec<ExtractionEvent> {
    // Upstream warns and skips posts the user cannot view.
    if post
        .get("current_user_can_view")
        .is_some_and(|v| !truthy(Some(v)))
    {
        return Vec::new();
    }
    if let Some(collection) = collection {
        post.insert("collection".into(), collection.clone());
    }
    let post_id = post.get("id").map(scalar).unwrap_or_default();
    let mut hashes = HashSet::new();
    let mut items = Vec::new();
    for (kind, file, url, name) in post_files(http, &post).await {
        let hash = filehash(&url);
        if !hash.is_empty() && !hashes.insert(hash.clone()) {
            continue;
        }
        let mut data = post.clone();
        data.insert("hash".into(), hash.into());
        data.insert("type".into(), kind.into());
        data.insert("file".into(), file);
        data.insert("num".into(), (items.len() + 1).into());
        nameext_into(&name, &mut data);
        let item = if ext_from_url(&url) == "m3u8" {
            data.insert("_ytdl_manifest".into(), "hls".into());
            data.insert("extension".into(), "mp4".into());
            let (Ok(link), Ok(base_url)) = (Url::parse(&format!("ytdl:{url}")), Url::parse(&url))
            else {
                continue;
            };
            Item::new(link, data)
                .job(MediaJob::Manifest {
                    protocol: ManifestProtocol::Hls,
                    base_url,
                    inline_data: None,
                })
                .header("Referer", &format!("{ROOT}/"))
        } else {
            let Ok(link) = Url::parse(&url) else { continue };
            Item::new(link, data)
        };
        let num = items.len() + 1;
        items.push(item.ids(post_id.as_str(), num.to_string()));
    }
    crate::sites::post(&SITE, http, route, items)
}

/// `_init_cursor`: the `cursor` option's starting position.
fn init_cursor(http: &Http) -> String {
    match http.option("cursor") {
        None | Some(Value::Bool(true)) => String::new(),
        Some(value) if truthy(Some(value)) => scalar(value),
        Some(_) => String::new(),
    }
}

/// `_order`: the `order-posts` option, else `sort`.
fn order(http: &Http, sort: &str, collection: bool) -> String {
    if let Some(order) = http.option_str("order-posts").filter(|o| !o.is_empty()) {
        let order = match (collection, order) {
            (false, "d" | "desc") => "-published_at",
            (false, "a" | "asc" | "r" | "reverse") => "published_at",
            (true, "a" | "asc") => "collection_order",
            (true, "d" | "desc" | "r" | "reverse") => "-published_at",
            (_, other) => other,
        };
        return format!("&sort={order}");
    }
    if sort.is_empty() {
        String::new()
    } else {
        format!("&sort={sort}")
    }
}

/// `_build_url`.
fn build_url(http: &Http, endpoint: &str, query: &str, order: &str) -> String {
    format!(
        "{ROOT}/api/{endpoint}\
         ?include=campaign,access_rules,attachments,attachments_media,\
         audio,images,media,native_video_insights,poll.choices,\
         poll.current_user_responses.user,\
         poll.current_user_responses.choice,\
         poll.current_user_responses.poll,\
         user,user_defined_tags,ti_checks\
         &fields[campaign]=currency,show_audio_post_download_links,\
         avatar_photo_url,avatar_photo_image_urls,earnings_visibility,\
         is_nsfw,is_monthly,name,url\
         &fields[post]=change_visibility_at,comment_count,commenter_count,\
         content,content_json_string,current_user_can_comment,\
         current_user_can_delete,current_user_can_view,\
         current_user_has_liked,embed,image,insights_last_updated_at,\
         is_paid,like_count,meta_image_url,min_cents_pledged_to_view,\
         post_file,post_metadata,published_at,patreon_url,post_type,\
         pledge_url,preview_asset_type,thumbnail,thumbnail_url,\
         teaser_text,title,upgrade_url,url,was_posted_by_campaign_owner,\
         has_ti_violation,moderation_status,\
         post_level_suspension_removal_date,pls_one_liners_by_category,\
         video_preview,view_count\
         &fields[post_tag]=tag_type,value\
         &fields[user]=image_url,full_name,url\
         &fields[access_rule]=access_rule_type,amount_cents\
         &fields[media]=id,image_urls,download_url,metadata,file_name\
         &fields[native_video_insights]=average_view_duration,\
         average_view_pct,has_preview,id,last_updated_at,num_views,\
         preview_views,video_duration\
         &page[cursor]={}{query}{order}&json-api-version=1.0",
        init_cursor(http)
    )
}

/// `_pagination`: follow `links.next` through the JSON:API pages.
fn pagination<'a>(
    http: &'a Http,
    route: &'static Route,
    collection: Option<Value>,
    url: String,
) -> Events<'a> {
    crate::sites::paginate(Some(url), move |state| {
        let collection = collection.clone();
        async move {
            let Some(url) = state.filter(|u| !u.is_empty()) else {
                return Ok(None);
            };
            let url = http.api(&ensure_http_scheme(&url))?;
            let page = http
                .json_with(
                    &url,
                    &[
                        ("Content-Type", "application/vnd.api+json"),
                        ("User-Agent", USER_AGENT),
                    ],
                )
                .await?;
            let mut posts = Vec::new();
            if let Some(included) = page.get("included") {
                let included = transform(included);
                for post in page["data"].as_array().into_iter().flatten() {
                    posts.push(process(http, post, &included).await);
                }
            }
            let next = page
                .get("links")
                .and_then(|links| links.get("next"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            let events = crate::sites::each(posts, move |post| {
                let collection = collection.clone();
                async move { Ok(post_events(http, route, collection.as_ref(), post).await) }
            });
            Ok(Some((events, Some(next.unwrap_or_default()))))
        }
    })
}

/// `PatreonCreatorExtractor._get_campaign_id`.
async fn campaign_id(
    http: &Http,
    creator: &str,
    params: &Map<String, Value>,
) -> crate::Result<String> {
    if let Some(id) = creator.strip_prefix("id:") {
        return Ok(id.to_owned());
    }
    if let Some(id) = params
        .get("c")
        .or_else(|| params.get("campaign_id"))
        .filter(|v| truthy(Some(v)))
    {
        return Ok(scalar(id));
    }
    let url = match params.get("u").filter(|v| truthy(Some(v))) {
        Some(user) => format!("{ROOT}/user?u={}", scalar(user)),
        None => format!("{ROOT}/{creator}"),
    };
    let page = http
        .text_with(&http.api(&url)?, &[("User-Agent", USER_AGENT)])
        .await?;
    if let Ok(data) = bootstrap(&page) {
        let id = &data["campaign"]["data"]["id"];
        if !id.is_null() {
            return Ok(scalar(id));
        }
        return Err(error("Unable to extract campaign ID"));
    }
    match extr(
        &page,
        r#"{\"value\":{\"campaign\":{\"data\":{\"id\":\""#,
        "\\\"",
    ) {
        "" => Err(error("Failed to extract campaign ID")),
        id => Ok(id.to_owned()),
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Patreon URL"));
    };
    match target {
        Target::Collection(id) => crate::sites::once(async move {
            let data = http
                .json_with(
                    &http.api(&format!("{ROOT}/api/collection/{id}"))?,
                    &[("User-Agent", USER_AGENT)],
                )
                .await?;
            let coll = &data["data"];
            let mut attr = coll["attributes"].as_object().cloned().unwrap_or_default();
            attr.insert("id".into(), coll["id"].clone());
            let created = attr
                .get("created_at")
                .and_then(Value::as_str)
                .and_then(parse_iso);
            attr.insert("date".into(), nested_date(created));
            let campaign = extr(
                attr.get("thumbnail")
                    .and_then(|t| t.get("url"))
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                "/campaign/",
                "/",
            )
            .to_owned();
            // Patreon answers 400 without the campaign filter.
            let query = format!(
                "&filter[campaign_id]={campaign}&filter[contains_exclusive_posts]=true\
                 &filter[is_draft]=false&filter[collection_id]={id}&filter[include_drops]=true"
            );
            let url = build_url(
                http,
                "posts",
                &query,
                &order(http, "collection_order", true),
            );
            Ok(pagination(
                http,
                &COLLECTION,
                Some(Value::Object(attr)),
                url,
            ))
        }),
        Target::Creator(creator, query) => crate::sites::once(async move {
            let params = parse_query(&query);
            let campaign = campaign_id(http, &creator, &params).await?;
            let filters: String = params
                .iter()
                .filter(|(key, _)| key.starts_with("filters["))
                .map(|(key, value)| {
                    format!("&filter[{}={}", &key[8..], html_escape(&scalar(value)))
                })
                .collect();
            let sort = params
                .get("sort")
                .map(scalar)
                .unwrap_or_else(|| "-published_at".into());
            let query = format!(
                "&filter[campaign_id]={campaign}&filter[contains_exclusive_posts]=true\
                 &filter[is_draft]=false{filters}"
            );
            let url = build_url(http, "posts", &query, &order(http, &sort, false));
            Ok(pagination(http, &CREATOR, None, url))
        }),
        Target::User => {
            let query = "&filter[is_following]=true&json-api-use-default-includes=false";
            let url = build_url(http, "stream", query, &order(http, "", false));
            pagination(http, &USER, None, url)
        }
        Target::Post(id) => crate::sites::once(async move {
            // Logged-out requests with this agent get `.m3u8` manifests.
            let agent = if logged_in(http) {
                USER_AGENT
            } else {
                "Patreon/14.2.1 (Android; Android 11; Scale/2.10)"
            };
            let page = http
                .text_with(
                    &http.api(&format!("{ROOT}/posts/{id}"))?,
                    &[("User-Agent", agent)],
                )
                .await?;
            let data = bootstrap(&page)?;
            let Some(post) = data.get("post") else {
                return Ok(crate::sites::events(Vec::new()));
            };
            let included = transform(&post["included"]);
            let post = process(http, &post["data"], &included).await;
            Ok(crate::sites::events(
                post_events(http, &POST, None, post).await,
            ))
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http as client, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.patreon.com/collection/12345"),
            Some(Target::Collection("12345".into()))
        );
        assert_eq!(
            t("https://www.patreon.com/c/koveliana/posts"),
            Some(Target::Creator("koveliana".into(), String::new()))
        );
        assert_eq!(
            t("https://www.patreon.com/profile/creators?u=123"),
            Some(Target::Creator(String::new(), "u=123".into()))
        );
        assert_eq!(t("https://www.patreon.com/home"), Some(Target::User));
        assert_eq!(
            t("https://www.patreon.com/posts/free-post-12497641"),
            Some(Target::Post("free-post-12497641".into()))
        );
        assert_eq!(t("https://www.patreon.com/search?q=x"), None);
    }

    #[test]
    fn tiptap_html() {
        let markup = json!({"content": [
            {"type": "paragraph", "content": [
                {"type": "text", "text": "a<b", "marks": [{"type": "bold"}, {"type": "link", "attrs": {"href": "https://x/"}}]}]},
            {"type": "image", "attrs": {"src": "https://c10.patreonusercontent.com/i.png", "media_id": 7}},
            {"type": "hardBreak"}]});
        assert_eq!(
            tiptap(&markup.to_string()).unwrap(),
            "<p><strong><a href=\"https://x/\">a&lt;b</a></strong></p>\
             <div data-image-container=\"true\"><figure><img src=\"https://c10.patreonusercontent.com/i.png\" data-media-id=\"7\"/></figure></div><br/>"
        );
    }

    #[test]
    fn bootstrap_from_flight_data() {
        let payload = r#"5b:["$","$L7c",null,{"bootstrapEnvelope":{"pageBootstrap":{"post":{"data":{"id":"1"}}}}}]"#;
        let page = format!(
            r#"<script nonce="n">self.__next_f.push([1,{}])</script>"#,
            serde_json::to_string(payload).unwrap()
        );
        assert_eq!(bootstrap(&page).unwrap()["post"]["data"]["id"], "1");
    }

    #[test]
    fn filehashes() {
        assert_eq!(
            filehash(
                "https://c10.patreonusercontent.com/4/patreon-media/p/post/1/0123456789abcdef0123456789abcdef/1.png?token=x"
            ),
            "0123456789abcdef0123456789abcdef"
        );
    }

    #[tokio::test]
    async fn post_page_files() {
        let hash = "0123456789abcdef0123456789abcdef";
        let image = format!("https://c10.patreonusercontent.com/p/{hash}/eyJ/1.png");
        let bootstrap = json!({"post": {
            "data": {"id": "12497641", "attributes": {"title": "Free Post", "published_at": "2017-07-10T12:00:00.000+00:00",
                "current_user_can_view": true, "image": {"large_url": image.clone(), "file_name": "big.png"},
                "content_json_string": "{\"content\":[{\"type\":\"image\",\"attrs\":{\"src\":\"https://c10.patreonusercontent.com/content.jpg\"}}]}",
                "post_file": {"url": "https://stream.mux.com/abc.m3u8?token=t", "name": null}},
              "relationships": {"images": {"data": [{"type": "media", "id": "1"}]},
                  "user": {"data": {"id": "5"}, "links": {"related": "https://www.patreon.com/api/user/5"}},
                  "campaign": {"data": {"id": "9"}},
                  "user_defined_tags": {"data": [{"type": "post_tag", "id": "user_defined;art"}]}}},
            "included": [
                {"type": "media", "id": "1", "attributes": {"download_url": image, "file_name": "one.png"}},
                {"type": "user", "id": "5", "attributes": {"full_name": "Creator"}},
                {"type": "campaign", "id": "9", "attributes": {"name": "Camp"}}]}});
        let page = format!(
            r#"<script id="__NEXT_DATA__" type="application/json">{}</script>"#,
            json!({"props": {"pageProps": {"bootstrapEnvelope": {"pageBootstrap": bootstrap}}}})
        );
        let (root, _server) = serve(vec![
            html(&page),
            crate::sites::testing::response("404 Not Found", "text/plain", b""),
            crate::sites::testing::response("200 OK", "image/jpeg", b""),
        ]);
        let http = client("patreon", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.patreon.com/posts/free-post-12497641").unwrap(),
        ))
        .await;
        let urls: Vec<&str> = files.iter().map(|f| f.url.as_str()).collect();
        assert_eq!(
            urls,
            [
                "https://c10.patreonusercontent.com/p/0123456789abcdef0123456789abcdef/eyJ/1.png",
                "ytdl:https://stream.mux.com/abc.m3u8?token=t",
                "https://c10.patreonusercontent.com/content.jpg"
            ]
        );
        let k = keywords(&files[0]);
        assert_eq!(
            (
                k["creator"]["full_name"].as_str(),
                k["tags"][0].as_str(),
                k["type"].as_str()
            ),
            (Some("Creator"), Some("art"), Some("image"))
        );
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "12497641_Free Post_01.png");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            crate::sites::testing::os_dir("patreon/Creator")
        );
        assert_eq!(keywords(&files[1])["extension"], "mp4");
    }
}
