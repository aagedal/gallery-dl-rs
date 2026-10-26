//! Tenor GIFs, searches and users, mirroring gallery_dl/extractor/tenor.py.
use crate::error;
use crate::sites::gdl::{
    Scan, captures, date, group, nameext_into, parse_ts, re, rpartition, unquote,
};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://tenor.com";
const FILENAME: &str = "{id}{title:? //}.{extension}";

static IMAGE: Route = Route::new("image", FILENAME, &["{category}"], "{id}");
static SEARCH: Route = Route::new("search", FILENAME, &["{category}", "{search_tags}"], "{id}");
static USER: Route = Route::new(
    "user",
    FILENAME,
    &["{category}", "@{user[username]}"],
    "{id}",
);

pub static SITE: Site = Site {
    category: "tenor",
    name: "Tenor",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&IMAGE, &SEARCH, &USER],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Image(String),
    Search(String),
    User(String),
}

fn target(url: &Url) -> Option<Target> {
    const BASE: &str = r"^https?://tenor\.com/(?:\w\w(?:-\w\w)?/)?";
    if let Some(c) = captures(re!(&format!(r"{BASE}view/(?:[^/?#]*-)?(\d+)")), url) {
        return Some(Target::Image(group(&c, 1).into()));
    }
    if let Some(c) = captures(re!(&format!(r"{BASE}search/([^/?#]+)")), url) {
        return Some(Target::Search(group(&c, 1).into()));
    }
    captures(re!(&format!(r"{BASE}(?:users|official)/([^/?#]+)")), url)
        .map(|c| Target::User(group(&c, 1).into()))
}

/// The `format` option: preferred media formats in order.
fn formats(http: &Http) -> Vec<String> {
    match http.option("format") {
        None | Some(Value::Null) => ["gif", "mp4", "webm", "webp"].map(String::from).to_vec(),
        Some(Value::String(s)) => s.split(',').map(str::to_owned).collect(),
        Some(Value::Array(list)) => list.iter().map(crate::sites::scalar).collect(),
        Some(other) => vec![crate::sites::scalar(other)],
    }
}

/// `TenorExtractor.items` for one GIF object.
fn gif_item(http: &Http, gif: &Value, extra: &Map<String, Value>) -> Option<Item> {
    let mut gif = gif.as_object()?.clone();
    let media = gif.get("media_formats")?.clone();
    let (name, fmt) = formats(http)
        .into_iter()
        .find_map(|name| Some((name.clone(), media.get(&name)?.clone())))?;
    let url = crate::sites::scalar(&fmt["url"]);
    let title = gif
        .remove("h1_title")
        .map(|t| crate::sites::scalar(&t))
        .unwrap_or_default();
    let title = title.strip_suffix(" GIF").unwrap_or(&title).to_owned();
    gif.insert("title".into(), title.into());
    let dims = fmt["dims"].as_array().cloned().unwrap_or_default();
    gif.insert(
        "width".into(),
        dims.first().cloned().unwrap_or_else(|| 0.into()),
    );
    gif.insert(
        "height".into(),
        dims.get(1).cloned().unwrap_or_else(|| 0.into()),
    );
    let description = gif
        .remove("content_description")
        .unwrap_or_else(|| "".into());
    gif.insert("description".into(), description);
    gif.insert(
        "id_format".into(),
        url.rsplit('/').nth(1).unwrap_or_default().into(),
    );
    gif.insert("format".into(), name.into());
    gif.insert("duration".into(), fmt["duration"].clone());
    gif.insert("size".into(), fmt["size"].clone());
    gif.insert("date".into(), date(gif.get("created").and_then(parse_ts)));
    gif.extend(extra.clone());
    nameext_into(&url, &mut gif);
    let id = gif.get("id").map(crate::sites::scalar).unwrap_or_default();
    Some(Item::new(Url::parse(&url).ok()?, gif).ids(id.clone(), id))
}

fn search<'a>(
    http: &'a Http,
    route: &'static Route,
    query: String,
    extra: Map<String, Value>,
) -> Events<'a> {
    crate::sites::paginate(Some(String::new()), move |state| {
        let query = query.clone();
        let extra = extra.clone();
        async move {
            let Some(pos) = state else { return Ok(None) };
            let mut url = http.api("https://tenor.googleapis.com/v2/search")?;
            {
                let mut pairs = url.query_pairs_mut();
                pairs.append_pair("appversion", "browser-r20250225-1")
                    .append_pair("prettyPrint", "false")
                    .append_pair("key", "AIzaSyC-P6_qz3FzCoXGLk6tgitZo4jEJ5mLzD8")
                    .append_pair("client_key", "tenor_web")
                    .append_pair("locale", "en")
                    .append_pair("anon_id", "")
                    .append_pair("q", &query)
                    .append_pair("limit", "50")
                    .append_pair("contentfilter", "low")
                    .append_pair("media_filter", "gif,gif_transparent,mediumgif,tinygif,tinygif_transparent,webp,webp_transparent,tinywebp,tinywebp_transparent,tinymp4,mp4,webm,originalgif,gifpreview")
                    .append_pair("fields", "next,results.id,results.media_formats,results.title,results.h1_title,results.long_title,results.itemurl,results.url,results.created,results.user,results.shares,results.embed,results.hasaudio,results.policy_status,results.source_id,results.flags,results.tags,results.content_rating,results.bg_color,results.legacy_info,results.geographic_restriction,results.content_description");
                if !pos.is_empty() {
                    pairs.append_pair("pos", &pos);
                }
                pairs.append_pair("component", "web_desktop");
            }
            let referer = format!("{ROOT}/");
            let data = http
                .json_with(&url, &[("Referer", &referer), ("Origin", ROOT)])
                .await?;
            let events = data["results"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|gif| gif_item(http, gif, &extra))
                .flat_map(|item| crate::sites::post(&SITE, http, route, vec![item]))
                .collect();
            let next = data
                .get("next")
                .and_then(Value::as_str)
                .filter(|n| !n.is_empty())
                .map(str::to_owned);
            Ok(Some((crate::sites::events(events), next)))
        }
    })
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Tenor URL"));
    };
    match target {
        Target::Image(id) => crate::sites::once(async move {
            let page = http.text(&http.api(&format!("{ROOT}/view/{id}"))?).await?;
            let pos = page
                .find("id=\"store-cache\"")
                .ok_or_else(|| error("Tenor page has no GIF data"))?;
            let data: Value = serde_json::from_str(Scan::at(&page, pos).extr(">", "</script>"))
                .map_err(|_| error("Tenor page has invalid GIF data"))?;
            let gif = data["gifs"]["byId"]
                .as_object()
                .and_then(|by_id| by_id.values().next_back())
                .map(|entry| entry["results"][0].clone())
                .ok_or_else(|| error("Tenor page has no GIF data"))?;
            let items = gif_item(http, &gif, &Map::new()).into_iter().collect();
            Ok(crate::sites::events(crate::sites::post(
                &SITE, http, &IMAGE, items,
            )))
        }),
        Target::Search(raw) => {
            let query = unquote(&raw);
            let (rest, _, last) = rpartition(&query, "-");
            let query = if last == "gifs" {
                rest.to_owned()
            } else {
                query.clone()
            };
            let tags = query.replace('-', " ");
            let mut extra = Map::new();
            extra.insert("search_tags".into(), tags.as_str().into());
            search(http, &SEARCH, tags, extra)
        }
        Target::User(user) => search(http, &USER, format!("@{user}"), Map::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http as client, json, keywords, media, serve};
    use serde_json::json;

    fn gif() -> Value {
        json!({"id": "8525772382434057283", "h1_title": "Moving GIF", "created": 1700000000.5,
            "content_description": "A moving thing", "user": {"username": "annetv"},
            "media_formats": {"mp4": {"url": "https://media.tenor.com/AbC/moving.mp4", "dims": [498, 280], "duration": 1.2, "size": 1000},
                              "gif": {"url": "https://media.tenor.com/XyZ/moving.gif", "dims": [498, 280], "duration": 1.2, "size": 5000}}})
    }

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://tenor.com/ja/view/moving-gif-8525772382434057283"),
            Some(Target::Image("8525772382434057283".into()))
        );
        assert_eq!(
            t("https://tenor.com/en-GB/search/trees-gifs"),
            Some(Target::Search("trees-gifs".into()))
        );
        assert_eq!(
            t("https://tenor.com/official/rwrbonprime"),
            Some(Target::User("rwrbonprime".into()))
        );
    }

    #[tokio::test]
    async fn extracts_view_page() {
        let store = json!({"gifs": {"byId": {"8525772382434057283": {"results": [gif()]}}}});
        let page = format!(r#"<script id="store-cache" type="text/x-cache">{store}</script>"#);
        let (root, server) = serve(vec![html(&page)]);
        let http = client("tenor", root);
        let files = media(extract(
            &http,
            Url::parse("https://tenor.com/view/moving-gif-8525772382434057283").unwrap(),
        ))
        .await;
        assert_eq!(
            files[0].url.as_str(),
            "https://media.tenor.com/XyZ/moving.gif"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["title"], "Moving");
        assert_eq!(k["id_format"], "XyZ");
        assert_eq!(k["width"], 498);
        assert_eq!(k["format"], "gif");
        let (file, _) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            file.render(&files[0]).unwrap(),
            "8525772382434057283 Moving.gif"
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn searches_with_format_preference() {
        let (root, server) = serve(vec![
            json(json!({"results": [gif()], "next": "50"})),
            json(json!({"results": [], "next": ""})),
        ]);
        let http = client("tenor", root).with_options(Some(std::sync::Arc::new(
            json!({"format": "webm,mp4"}).as_object().unwrap().clone(),
        )));
        let files = media(extract(
            &http,
            Url::parse("https://tenor.com/search/trees-water-gifs").unwrap(),
        ))
        .await;
        assert_eq!(
            files[0].url.as_str(),
            "https://media.tenor.com/AbC/moving.mp4"
        );
        assert_eq!(keywords(&files[0])["search_tags"], "trees water");
        let requests = server.join().unwrap();
        assert!(requests[0].contains("&q=trees+water&"));
        assert!(requests[1].contains("&pos=50&"));
    }
}
