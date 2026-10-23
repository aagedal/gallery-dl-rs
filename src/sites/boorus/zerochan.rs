//! Zerochan (www.zerochan.net), mirroring gallery_dl/extractor/zerochan.py
//! for logged-out access: tag listings through the JSON listing API (or the
//! HTML thumbnails with `pagination: "html"`) and single entries through
//! their page's JSON-LD.
use super::{Extr, booru_fields, date_value, item, parse_query, per_post};
use crate::sites::{Events, Http, Route, Site, scalar};
use crate::{Result, error};
use futures_util::StreamExt;
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://www.zerochan.net";
const STATIC: &str = "https://static.zerochan.net/.full.";
/// `ZerochanExtractor.page_start` and `per_page`.
const PAGE_START: i64 = 1;
const PER_PAGE: i64 = 200;
const DEFAULT_EXTENSIONS: [&str; 4] = ["jpg", "png", "webp", "gif"];
/// Internal carrier for the parsed `date`, removed before emitting.
const TIMESTAMP: &str = "\u{0}timestamp";

static TAG: Route = Route::new(
    "tag",
    "{id}.{extension}",
    &["{category}", "{search_tags}"],
    "{id}",
);
static IMAGE: Route = Route::new("image", "{id}.{extension}", &["{category}"], "{id}");

pub static ZEROCHAN: Site = Site {
    category: "zerochan",
    name: "Zerochan",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&TAG, &IMAGE],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Tag { tag: String, query: Option<String> },
    Image(String),
}

/// `BASE/(?!\d+$)([^/?#]+)/?(?:\?([^#]+))?` (tag, tried first) and
/// `BASE/(\d+)` (image), matched against path, query and fragment.
fn target(url: &Url) -> Option<Target> {
    let rest = super::below(url, &["zerochan.net"])
        .or_else(|| super::below(url, &["www.zerochan.net"]))?;
    let rest = match url.fragment() {
        Some(fragment) => format!("{rest}#{fragment}"),
        None => rest,
    };
    let rest = rest.strip_prefix('/')?;
    if !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()) {
        return Some(Target::Image(rest.into()));
    }
    let tag = super::until(rest, &['/', '?', '#']);
    if tag.is_empty() {
        return None;
    }
    let after = &rest[tag.len()..];
    let after = after.strip_prefix('/').unwrap_or(after);
    let query = after
        .strip_prefix('?')
        .map(|q| super::until(q, &['#']))
        .filter(|q| !q.is_empty())
        .map(str::to_owned);
    Some(Target::Tag {
        tag: tag.into(),
        query,
    })
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Tag { tag, query }) => {
            let mut data = Map::new();
            data.insert("search_tags".into(), super::unquote_plus(&tag).into());
            let mut params = parse_query(query.as_deref().unwrap_or_default(), true);
            let page = parse_int_or(super::query_get(&params, "p"), PAGE_START);
            set_param(&mut params, "p", page.to_string());
            if http.option_str("pagination") == Some("html") {
                tag_html(http, tag, params, data)
            } else {
                if super::query_get(&params, "l").is_none() {
                    params.push(("l".into(), PER_PAGE.to_string()));
                }
                set_param(&mut params, "json", "1".into());
                tag_api(http, tag, params, data)
            }
        }
        Some(Target::Image(id)) => crate::sites::once(async move {
            let mut post = match entry_html(http, &id).await {
                Ok(post) => post,
                // Upstream logs the page's message and yields nothing.
                Err(e) if not_found(&e) => return Ok(crate::sites::events(Vec::new())),
                Err(e) => return Err(e),
            };
            if http.option_bool("metadata", false) {
                post.extend(entry_api(http, &id).await?);
            }
            Ok(crate::sites::events(finish(
                http,
                &IMAGE,
                post,
                &Map::new(),
            )))
        }),
        None => crate::sites::fail(error("unsupported Zerochan URL")),
    }
}

/// The HTTP layer's wording for 404/410 responses.
fn not_found(e: &crate::Error) -> bool {
    e.to_string().ends_with("resource not found")
}

/// `text.parse_int(value, default)`.
fn parse_int_or(value: Option<&str>, default: i64) -> i64 {
    match value {
        None | Some("") => default,
        Some(v) => v.trim().replace('_', "").parse().unwrap_or(default),
    }
}

/// Set `name` in an ordered parameter list, keeping its position.
fn set_param(params: &mut Vec<(String, String)>, name: &str, value: String) {
    match params.iter_mut().find(|(n, _)| n == name) {
        Some(pair) => pair.1 = value,
        None => params.push((name.into(), value)),
    }
}

fn next_page(params: &[(String, String)]) -> Vec<(String, String)> {
    let mut params = params.to_vec();
    let page = parse_int_or(super::query_get(&params, "p"), PAGE_START);
    set_param(&mut params, "p", (page + 1).to_string());
    params
}

fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
        Some(Value::Bool(true)) => true,
    }
}

/// The `extensions` option: a list or a comma-separated string.
fn extensions(http: &Http) -> Vec<String> {
    let configured: Vec<String> = match http.option("extensions") {
        Some(Value::String(s)) if !s.is_empty() => s.split(',').map(str::to_owned).collect(),
        Some(Value::Array(a)) => a.iter().map(scalar).collect(),
        _ => Vec::new(),
    };
    if configured.is_empty() {
        DEFAULT_EXTENSIONS.iter().map(|s| (*s).to_owned()).collect()
    } else {
        configured
    }
}

/// A listed post and what remains to be fetched for it.
enum Entry {
    /// A listing API item; with `metadata`, merged with the entry page and
    /// entry API, otherwise given the first fallback-extension URL.
    Api(Map<String, Value>),
    /// An HTML thumbnail id to fetch with `metadata`.
    Fetch(String),
    /// A complete HTML thumbnail post.
    Ready(Map<String, Value>),
}

/// `posts_api`: `/TAG?p=N&l=200&json=1` until `next` is unset.
fn tag_api<'a>(
    http: &'a Http,
    tag: String,
    params: Vec<(String, String)>,
    data: Map<String, Value>,
) -> Events<'a> {
    crate::sites::paginate(Some(params), move |state| {
        let tag = tag.clone();
        let data = data.clone();
        async move {
            let Some(params) = state else {
                return Ok(None);
            };
            let mut url = http.api(&format!("{ROOT}/{tag}"))?;
            url.query_pairs_mut().extend_pairs(&params);
            let body = match http.json(&url).await {
                Ok(body) => body,
                Err(e) if not_found(&e) => return Ok(None),
                Err(e) => return Err(e),
            };
            let Some(posts) = body.get("items").and_then(Value::as_array) else {
                return Ok(None);
            };
            let entries = posts
                .iter()
                .filter_map(|post| post.as_object().cloned().map(Entry::Api))
                .collect();
            let next = truthy(body.get("next")).then(|| next_page(&params));
            Ok(Some((page_events(http, entries, data), next)))
        }
    })
}

/// `posts_html`: the thumbnail list of `/TAG?p=N` until there is no
/// `rel="next"` link.
fn tag_html<'a>(
    http: &'a Http,
    tag: String,
    params: Vec<(String, String)>,
    data: Map<String, Value>,
) -> Events<'a> {
    let metadata = http.option_bool("metadata", false);
    crate::sites::paginate(Some(params), move |state| {
        let tag = tag.clone();
        let data = data.clone();
        async move {
            let Some(params) = state else {
                return Ok(None);
            };
            let mut url = http.api(&format!("{ROOT}/{tag}"))?;
            url.query_pairs_mut().extend_pairs(&params);
            let page = match http.text(&url).await {
                Ok(page) => page,
                Err(e) if not_found(&e) => return Ok(None),
                Err(e) => return Err(e),
            };
            let entries = parse_thumbs(&page, metadata);
            let next = page.contains("rel=\"next\"").then(|| next_page(&params));
            Ok(Some((page_events(http, entries, data), next)))
        }
    })
}

fn parse_thumbs(page: &str, metadata: bool) -> Vec<Entry> {
    let thumbs = crate::sites::extr(page, "<ul id=\"thumbs", "</ul>").unwrap_or_default();
    let mut extr = Extr::new(thumbs);
    let mut entries = Vec::new();
    while !extr.next("<li class=\"", ">").is_empty() {
        if metadata {
            entries.push(Entry::Fetch(extr.next("href=\"/", "\"").to_owned()));
            continue;
        }
        let mut post = Map::new();
        post.insert("id".into(), extr.next("href=\"/", "\"").into());
        post.insert("name".into(), extr.next("alt=\"", "\"").into());
        post.insert("width".into(), extr.next("title=\"", "&#10005;").into());
        post.insert("height".into(), extr.next("", " ").into());
        post.insert("size".into(), extr.next("", "b").into());
        let url = format!(
            "https://static.{}",
            extr.next("<a href=\"https://static.", "\"")
        );
        post.insert("file_url".into(), url.into());
        entries.push(Entry::Ready(post));
    }
    entries
}

/// One listing page, fetching per-post metadata only as it is consumed.
fn page_events<'a>(http: &'a Http, entries: Vec<Entry>, data: Map<String, Value>) -> Events<'a> {
    let metadata = http.option_bool("metadata", false);
    let first_extension = extensions(http).swap_remove(0);
    crate::sites::pages(futures_util::stream::iter(entries).then(move |entry| {
        let data = data.clone();
        let extension = first_extension.clone();
        async move {
            let post = match entry {
                Entry::Api(mut post) => {
                    let id = post.get("id").map(scalar).unwrap_or_default();
                    if metadata {
                        post.extend(entry_html(http, &id).await?);
                        post.extend(entry_api(http, &id).await?);
                    } else {
                        // Upstream also queues the other extensions as
                        // `_fallback` URLs, which registry media lack.
                        post.insert(
                            "file_url".into(),
                            format!("{STATIC}{id}.{extension}").into(),
                        );
                    }
                    post
                }
                Entry::Fetch(id) => {
                    let mut post = entry_html(http, &id).await?;
                    post.extend(entry_api(http, &id).await?);
                    post
                }
                Entry::Ready(post) => post,
            };
            Ok(crate::sites::events(finish(http, &TAG, post, &data)))
        }
    }))
}

/// `BooruExtractor.items` for one post, with the `tags` option's `_tags`.
fn finish(
    http: &Http,
    route: &'static Route,
    mut post: Map<String, Value>,
    data: &Map<String, Value>,
) -> Vec<crate::event::ExtractionEvent> {
    let timestamp = post.remove(TIMESTAMP).and_then(|v| v.as_i64());
    let Some(url) = post
        .get("file_url")
        .and_then(Value::as_str)
        .and_then(|raw| super::file_url(ROOT, raw))
    else {
        return Vec::new();
    };
    if http.option_bool("tags", false) {
        split_tags(&mut post);
    }
    booru_fields(&url, &mut post, data);
    per_post(
        &ZEROCHAN,
        http,
        route,
        vec![item(url, post, "uploader", "name", timestamp)],
    )
}

/// `ZerochanExtractor._tags`: `tags_<category>` lists from `Category:Name`.
fn split_tags(post: &mut Map<String, Value>) {
    let Some(tags) = post.get("tags").and_then(Value::as_array) else {
        return;
    };
    let mut groups: Vec<(String, Vec<Value>)> = Vec::new();
    for tag in tags {
        let tag = scalar(tag);
        let (category, name) = tag.split_once(':').unwrap_or((&tag, ""));
        match groups.iter_mut().find(|(c, _)| c == category) {
            Some((_, list)) => list.push(name.into()),
            None => groups.push((category.to_owned(), vec![name.into()])),
        }
    }
    for (category, names) in groups {
        post.insert(format!("tags_{}", category.to_lowercase()), names.into());
    }
}

/// `text` without its last `n` characters (Python `text[:-n]`).
fn drop_last(text: &str, n: usize) -> &str {
    let keep = text.chars().count().saturating_sub(n);
    text.char_indices()
        .nth(keep)
        .map_or(text, |(at, _)| &text[..at])
}

/// Python `str.capitalize`.
fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first
            .to_uppercase()
            .chain(chars.flat_map(char::to_lowercase))
            .collect(),
        None => String::new(),
    }
}

/// `Extractor._extract_jsonld`.
fn jsonld(page: &str) -> Option<Map<String, Value>> {
    let raw = crate::sites::extr(page, "<script type=\"application/ld+json\">", "</script>")
        .filter(|s| !s.is_empty())
        .or_else(|| crate::sites::extr(page, "<script type='application/ld+json'>", "</script>"))?;
    serde_json::from_str::<Value>(raw)
        .ok()?
        .as_object()
        .cloned()
}

async fn entry_html(http: &Http, id: &str) -> Result<Map<String, Value>> {
    let url = http.api(&format!("{ROOT}/{id}"))?;
    let page = http.text(&url).await?;
    Ok(parse_entry_html(&page, id))
}

/// `_parse_entry_html`: JSON-LD media fields plus breadcrumbs, uploader,
/// tags and source from the entry page.
fn parse_entry_html(page: &str, id: &str) -> Map<String, Value> {
    let mut data = Map::new();
    let Some(ld) = jsonld(page) else {
        data.insert("id".into(), id.into());
        return data;
    };
    let text = |key: &str| ld.get(key).map(scalar).unwrap_or_default();
    let timestamp = super::iso_timestamp(&text("datePublished"));
    let mut extr = Extr::new(page);
    data.insert("id".into(), super::int_value(id));
    if let Some(url) = ld.get("contentUrl") {
        data.insert("file_url".into(), url.clone());
    }
    data.insert("date".into(), date_value(timestamp));
    data.insert(TIMESTAMP.into(), timestamp.into());
    data.insert(
        "width".into(),
        super::int_value(drop_last(&text("width"), 3)),
    );
    data.insert(
        "height".into(),
        super::int_value(drop_last(&text("height"), 3)),
    );
    data.insert(
        "size".into(),
        super::parse_bytes(drop_last(&text("contentSize"), 1)).into(),
    );
    let path: Vec<Value> = super::split_html(extr.next("class=\"breadcrumbs", "</nav>"))
        .into_iter()
        .skip(2)
        .map(Value::from)
        .collect();
    data.insert("path".into(), path.into());
    data.insert("uploader".into(), extr.next("href=\"/user/", "\"").into());
    let tags_html = extr.next("<ul id=\"tags\"", "</ul>");
    let source = super::rpartition(extr.next("id=\"source-url\"", "</p>"), "</s>").1;
    data.insert(
        "source".into(),
        crate::sites::unescape(&crate::sites::remove_html(source)).into(),
    );
    let author = ld
        .get("author")
        .and_then(|a| a.get("name"))
        .cloned()
        .unwrap_or_else(|| "".into());
    data.insert("author".into(), author);
    let tags: Vec<Value> = tags_html
        .split("<li class=")
        .skip(1)
        .map(|tag| {
            let category = crate::sites::extr(tag, "\"", "\"").unwrap_or_default();
            let name = crate::sites::unescape(
                crate::sites::extr(tag, "data-tag=\"", "\"").unwrap_or_default(),
            );
            format!("{}:{name}", capitalize(super::before(category, " "))).into()
        })
        .collect();
    data.insert("tags".into(), tags.into());
    data
}

async fn entry_api(http: &Http, id: &str) -> Result<Map<String, Value>> {
    let url = http.api(&format!("{ROOT}/{id}?json"))?;
    let body = http.text(&url).await?;
    Ok(parse_entry_api(&body, id))
}

/// `_parse_entry_api` for logged-out sessions (tags included).
fn parse_entry_api(body: &str, id: &str) -> Map<String, Value> {
    let item = match serde_json::from_str::<Value>(body) {
        Ok(Value::Object(item)) => item,
        _ => {
            let mut item = parse_loose_json(body);
            item.insert("id".into(), super::int_value(id));
            item
        }
    };
    let get = |key: &str| item.get(key).cloned().unwrap_or(Value::Null);
    let mut data = Map::new();
    for (to, from) in [
        ("id", "id"),
        ("file_url", "full"),
        ("width", "width"),
        ("height", "height"),
        ("size", "size"),
        ("name", "primary"),
        ("md5", "hash"),
        ("source", "source"),
        ("tags", "tags"),
    ] {
        data.insert(to.into(), get(from));
    }
    data
}

/// `_parse_json`: recover fields from the entry API's malformed JSON.
fn parse_loose_json(body: &str) -> Map<String, Value> {
    let text: String = body
        .chars()
        .filter(|c| !matches!(*c as u32, 0..=0x1f | 0x7f))
        .collect();
    let (main, tags) = text.split_once("tags\": [").unwrap_or((&text, ""));
    let mut item = Map::new();
    for line in main.split(",  \"").skip(1) {
        let (key, value) = line.split_once("\": ").unwrap_or((line, ""));
        let value = if value.starts_with('"') {
            let inner: String = value.chars().skip(1).collect();
            Value::from(drop_last(&inner, 1))
        } else if value.is_empty() {
            Value::from("")
        } else {
            super::int_value(value)
        };
        if !key.is_empty() {
            item.insert(key.into(), value);
        }
    }
    let tags: String = tags.chars().skip(5).collect();
    let mut tags: Vec<String> = tags.split("\",    \"").map(str::to_owned).collect();
    if let Some(last) = tags.last_mut() {
        *last = drop_last(last, 5).to_owned();
    }
    item.insert("tags".into(), tags.into());
    item
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, json, keywords, media, response, serve};
    use serde_json::json;

    fn t(raw: &str) -> Option<Target> {
        target(&Url::parse(raw).unwrap())
    }

    fn tag(tag: &str, query: Option<&str>) -> Option<Target> {
        Some(Target::Tag {
            tag: tag.into(),
            query: query.map(str::to_owned),
        })
    }

    #[test]
    fn classifies_urls() {
        assert_eq!(
            t("https://www.zerochan.net/Perth+%28Kantai+Collection%29"),
            tag("Perth+%28Kantai+Collection%29", None)
        );
        assert_eq!(
            t("https://zerochan.net/Touhou/?p=3&s=fav"),
            tag("Touhou", Some("p=3&s=fav"))
        );
        assert_eq!(
            t("https://www.zerochan.net/Touhou/x?p=3"),
            tag("Touhou", None)
        );
        assert_eq!(
            t("https://www.zerochan.net/2920445"),
            Some(Target::Image("2920445".into()))
        );
        // `(?!\d+$)` only rejects ids running to the end of the URL.
        assert_eq!(t("https://www.zerochan.net/2920445/"), tag("2920445", None));
        assert_eq!(
            t("https://www.zerochan.net/2920445?x=1"),
            tag("2920445", Some("x=1"))
        );
        assert_eq!(t("https://www.zerochan.net/123abc"), tag("123abc", None));
        for raw in [
            "https://www.zerochan.net/",
            "https://www.zerochan.net/?q=1",
            "https://static.zerochan.net/Touhou",
            "https://www.zerochan.net.evil.test/Touhou",
            "https://evilzerochan.net/Touhou",
            "https://www.zerochan.net:444/Touhou",
            "https://u:p@www.zerochan.net/Touhou",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
        assert!(
            crate::sites::find(&Url::parse("https://www.zerochan.net/1").unwrap())
                .is_some_and(|site| site.category == "zerochan")
        );
    }

    #[tokio::test]
    async fn api_pagination_uses_first_extension() {
        let (root, server) = serve(vec![
            json(json!({"items": [
                {"id": 11, "width": 100, "height": 200, "tag": "Perth", "tags": ["Perth", "Female"]},
                {"id": 12, "width": 1, "height": 2, "tag": "Perth", "tags": []}
            ], "next": true})),
            json(json!({"items": [{"id": 13, "width": 3, "height": 4, "tags": []}]})),
        ]);
        let http = http("zerochan", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.zerochan.net/Perth+%28Kantai+Collection%29").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 3);
        let file = &files[0];
        assert_eq!(
            file.url.as_str(),
            "https://static.zerochan.net/.full.11.jpg"
        );
        let k = keywords(file);
        assert_eq!(k["id"], 11);
        assert_eq!(k["width"], 100);
        assert_eq!(k["search_tags"], "Perth (Kantai Collection)");
        assert_eq!(k["filename"], ".full.11");
        assert_eq!(k["extension"], "jpg");
        assert_eq!(k["file_url"], "https://static.zerochan.net/.full.11.jpg");
        assert!(!k.contains_key("date"));
        let (name, dir) = crate::sites::default_templates(file).unwrap();
        assert_eq!(name.render(file).unwrap(), "11.jpg");
        assert_eq!(
            dir.render(file).unwrap().unwrap(),
            std::path::PathBuf::from("zerochan/Perth (Kantai Collection)")
        );
        assert_eq!(
            crate::sites::archive_key(file).unwrap(),
            ("zerochan", r#"["{id}",["11"]]"#.into())
        );
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with("GET /Perth+%28Kantai+Collection%29?p=1&l=200&json=1 "),
            "{}",
            requests[0]
        );
        assert!(requests[1].starts_with("GET /Perth+%28Kantai+Collection%29?p=2&l=200&json=1 "));
    }

    #[tokio::test]
    async fn api_query_options_and_missing_tag() {
        let (root, server) = serve(vec![
            json(json!({"items": [{"id": 5, "tags": ["Theme:Coat", "Character:Perth"]}]})),
            response("404 Not Found", "text/html", b"<h2>No such tag</h2>"),
        ]);
        let options = json!({"extensions": "png,jpg", "tags": true});
        let http = http("zerochan", root).with_options(Some(std::sync::Arc::new(
            options.as_object().unwrap().clone(),
        )));
        let files = media(extract(
            &http,
            Url::parse("https://www.zerochan.net/Touhou?p=4&l=10").unwrap(),
        ))
        .await;
        assert_eq!(
            files[0].url.as_str(),
            "https://static.zerochan.net/.full.5.png"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["tags_theme"], json!(["Coat"]));
        assert_eq!(k["tags_character"], json!(["Perth"]));
        let none = media(extract(
            &http,
            Url::parse("https://www.zerochan.net/non_existant_tag").unwrap(),
        ))
        .await;
        assert!(none.is_empty());
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with("GET /Touhou?p=4&l=10&json=1 "),
            "{}",
            requests[0]
        );
    }

    const ENTRY: &str = r#"<html><head>
<script type="application/ld+json">{"@context":"https://schema.org","@type":"ImageObject",
"contentUrl":"https://static.zerochan.net/Perth.%28Kantai.Collection%29.full.2920445.jpg",
"datePublished":"2020-04-24T21:33:44+00:00","width":"1920 px","height":"1366 px",
"contentSize":"1.9 MB","author":{"@type":"Person","name":"YeFan 葉凡"}}</script></head><body>
<nav class="breadcrumbs"><a href="/">Zerochan</a> <a href="/Game">Game</a>
<a href="/Kantai+Collection">Kantai Collection</a> <a>Perth (Kantai Collection)</a></nav>
<p>Uploaded by <a href="/user/YukinoTokisaki">YukinoTokisaki</a></p>
<ul id="tags"><li class="mangaka" data-tag="YeFan 葉凡"></li>
<li class="theme hidden" data-tag="Firefighter Outfit"></li><li class="CHARACTER" data-tag="Perth &amp; Co"></li></ul>
<p id="source-url"><s>Source</s></s> <a href="x">https://x.test/?a=1&amp;b=2</a></p>
</body></html>"#;

    #[test]
    fn parses_entry_page() {
        let data = parse_entry_html(ENTRY, "2920445");
        assert_eq!(data["id"], 2920445);
        assert_eq!(
            data["file_url"],
            "https://static.zerochan.net/Perth.%28Kantai.Collection%29.full.2920445.jpg"
        );
        assert_eq!(data["date"], "2020-04-24 21:33:44");
        assert_eq!(data["width"], 1920);
        assert_eq!(data["height"], 1366);
        assert_eq!(data["size"], 1_992_294);
        assert_eq!(
            data["path"],
            json!(["Game", "Kantai Collection", "Perth (Kantai Collection)"])
        );
        assert_eq!(data["uploader"], "YukinoTokisaki");
        assert_eq!(data["author"], "YeFan 葉凡");
        assert_eq!(data["source"], "https://x.test/?a=1&b=2");
        assert_eq!(
            data["tags"],
            json!([
                "Mangaka:YeFan 葉凡",
                "Theme:Firefighter Outfit",
                "Character:Perth & Co"
            ])
        );
        let fallback = parse_entry_html("<html>no json-ld</html>", "7");
        assert_eq!(fallback, json!({"id": "7"}).as_object().unwrap().clone());
    }

    #[test]
    fn parses_loose_entry_api() {
        let body = "{\n  \"id\": 1,\n  \"primary\": \"Perth\",\n  \"width\": 1920,\n  \"full\": \"https://static.zerochan.net/a.full.1.jpg\",\n  \"hash\": \"abc\",\n  \"tags\": [\n    \"Perth\",\n    \"Bad \"quote\"\"\n  ]\n}";
        let data = parse_entry_api(body, "1");
        assert_eq!(data["id"], 1);
        assert_eq!(data["name"], "Perth");
        assert_eq!(data["width"], 1920);
        assert_eq!(data["md5"], "abc");
        assert_eq!(data["file_url"], "https://static.zerochan.net/a.full.1.jpg");
        assert_eq!(data["tags"], json!(["Perth", "Bad \"quote\""]));
        assert_eq!(data["source"], Value::Null);
        let strict = parse_entry_api(
            r#"{"id": 2, "full": "u", "tags": ["x"], "source": "s"}"#,
            "2",
        );
        assert_eq!(strict["tags"], json!(["x"]));
        assert_eq!(strict["source"], "s");
    }

    #[tokio::test]
    async fn image_entry_and_metadata_option() {
        let api = json!({"id": 2920445, "full": "https://static.zerochan.net/Perth.%28Kantai.Collection%29.full.2920445.jpg",
            "width": 1920, "height": 1366, "size": 1975296, "primary": "Perth (Kantai Collection)",
            "hash": "0123", "source": "", "tags": ["Perth (Kantai Collection)"]});
        let (root, server) = serve(vec![
            html(ENTRY),
            json(api),
            response("404 Not Found", "text/html", b"<h2>Gone</h2>"),
        ]);
        let options = json!({"metadata": true});
        let http = http("zerochan", root).with_options(Some(std::sync::Arc::new(
            options.as_object().unwrap().clone(),
        )));
        let files = media(extract(
            &http,
            Url::parse("https://www.zerochan.net/2920445").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let file = &files[0];
        let k = keywords(file);
        assert_eq!(k["subcategory"], "image");
        assert_eq!(k["filename"], "Perth.(Kantai.Collection).full.2920445");
        assert_eq!(k["size"], 1975296);
        assert_eq!(k["md5"], "0123");
        assert_eq!(k["name"], "Perth (Kantai Collection)");
        assert_eq!(k["date"], "2020-04-24 21:33:44");
        assert!(!k.contains_key(TIMESTAMP));
        let meta = file.metadata.as_ref().unwrap();
        assert_eq!(meta.timestamp, Some(1_587_764_024));
        assert_eq!(meta.username.as_deref(), Some("YukinoTokisaki"));
        let (name, dir) = crate::sites::default_templates(file).unwrap();
        assert_eq!(name.render(file).unwrap(), "2920445.jpg");
        assert_eq!(
            dir.render(file).unwrap().unwrap(),
            std::path::PathBuf::from("zerochan")
        );
        let gone = media(extract(
            &http,
            Url::parse("https://www.zerochan.net/1").unwrap(),
        ))
        .await;
        assert!(gone.is_empty());
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /2920445 "));
        assert!(requests[1].starts_with("GET /2920445?json "));
        assert!(requests[2].starts_with("GET /1 "));
    }

    #[tokio::test]
    async fn html_pagination() {
        let page = r#"<ul id="thumbs2"><li class="a"><a href="/11" class="thumb"><img alt="Perth" title="1920&#10005;1080 123kb"></a>
<a href="https://static.zerochan.net/Perth.full.11.png">x</a></li></ul> <a rel="next" href="?p=2">"#;
        let (root, server) = serve(vec![html(page), html("<ul id=\"thumbs2\"></ul>")]);
        let options = json!({"pagination": "html"});
        let http = http("zerochan", root).with_options(Some(std::sync::Arc::new(
            options.as_object().unwrap().clone(),
        )));
        let files = media(extract(
            &http,
            Url::parse("https://www.zerochan.net/Perth").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let k = keywords(&files[0]);
        assert_eq!(k["id"], "11");
        assert_eq!(k["name"], "Perth");
        assert_eq!(k["width"], "1920");
        assert_eq!(k["height"], "1080");
        assert_eq!(k["size"], "123k");
        assert_eq!(k["extension"], "png");
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with("GET /Perth?p=1 "),
            "{}",
            requests[0]
        );
        assert!(requests[1].starts_with("GET /Perth?p=2 "));
    }
}
