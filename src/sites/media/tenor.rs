//! Tenor GIFs, mirroring gallery_dl/extractor/tenor.py: single GIFs come
//! from the page's store cache, searches and users from the public v2
//! search API with the web client's key.
use super::{date, path_segment, slug_id, str_of, ts_value};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
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

/// `\w\w(?:-\w\w)?`, the optional locale prefix.
fn locale(segment: &str) -> bool {
    let chars: Vec<char> = segment.chars().collect();
    let word = |c: &char| super::word_char(*c);
    match chars.as_slice() {
        [a, b] => word(a) && word(b),
        [a, b, '-', c, d] => [a, b, c, d].into_iter().all(word),
        _ => false,
    }
}

/// `tenor\.com/(?:LOCALE/)?` followed by one of the extractor paths.
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) || url.host_str() != Some("tenor.com") {
        return None;
    }
    let parts: Vec<&str> = url.path()[1..].split('/').collect();
    let route = |parts: &[&str]| match parts {
        ["view", slug, ..] => {
            slug_id(slug, |c| c.is_ascii_digit()).map(|(_, id)| Target::Image(id.into()))
        }
        ["search", query, ..] if path_segment(query) => Some(Target::Search((*query).into())),
        ["users" | "official", user, ..] if path_segment(user) => {
            Some(Target::User((*user).into()))
        }
        _ => None,
    };
    match parts.as_slice() {
        [first, rest @ ..] if locale(first) => route(rest).or_else(|| route(&parts)),
        _ => route(&parts),
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Tenor URL"));
    };
    match target {
        Target::Image(id) => crate::sites::once(async move {
            let page = http
                .text_with(
                    &http.api(&format!("{ROOT}/view/{id}"))?,
                    &http.browser_headers(&[]),
                )
                .await?;
            let gif = store_cache(&page)?;
            Ok(gifs_events(http, &IMAGE, vec![gif], &Map::new()))
        }),
        Target::Search(query) => {
            let query = crate::sites::percent_decode(&query);
            let query = match query.rsplit_once('-') {
                Some((rest, "gifs")) => rest.to_owned(),
                _ => query,
            };
            let tags = query.replace('-', " ");
            let mut metadata = Map::new();
            metadata.insert("search_tags".into(), tags.clone().into());
            search(http, &SEARCH, tags, metadata)
        }
        Target::User(user) => search(http, &USER, format!("@{user}"), Map::new()),
    }
}

/// The first GIF of the last entry in the page's `store-cache` JSON.
fn store_cache(page: &str) -> Result<Value> {
    let pos = page
        .find("id=\"store-cache\"")
        .ok_or_else(|| error("Tenor page has no store cache"))?;
    let rest = &page[pos..];
    let data = rest
        .find('>')
        .and_then(|start| {
            let body = &rest[start + 1..];
            body.find("</script>").map(|end| &body[..end])
        })
        .unwrap_or_default();
    let data = crate::sites::art::json_loads(data, "Tenor")?;
    data.pointer("/gifs/byId")
        .and_then(Value::as_object)
        .and_then(|by_id| by_id.values().next_back())
        .and_then(|entry| entry.pointer("/results/0"))
        .cloned()
        .ok_or_else(|| error("Tenor page has no GIF data"))
}

const SEARCH_API: &str = "https://tenor.googleapis.com/v2/search";
const MEDIA_FILTER: &str = "gif,gif_transparent,mediumgif,tinygif,tinygif_transparent,\
webp,webp_transparent,tinywebp,tinywebp_transparent,tinymp4,mp4,webm,originalgif,gifpreview";
const FIELDS: &str = "next,results.id,results.media_formats,results.title,\
results.h1_title,results.long_title,results.itemurl,results.url,results.created,\
results.user,results.shares,results.embed,results.hasaudio,results.policy_status,\
results.source_id,results.flags,results.tags,results.content_rating,results.bg_color,\
results.legacy_info,results.geographic_restriction,results.content_description";

/// `_search_results`: 50 results per page, following `next` until empty.
fn search<'a>(
    http: &'a Http,
    route: &'static Route,
    query: String,
    metadata: Map<String, Value>,
) -> Events<'a> {
    crate::sites::paginate((Some(None::<String>), 0u32), move |(pos, pages)| {
        let query = query.clone();
        let metadata = metadata.clone();
        async move {
            let Some(pos) = pos else {
                return Ok(None);
            };
            let mut url = http.api(SEARCH_API)?;
            {
                let mut params = url.query_pairs_mut();
                params.extend_pairs([
                    ("appversion", "browser-r20250225-1"),
                    ("prettyPrint", "false"),
                    ("key", "AIzaSyC-P6_qz3FzCoXGLk6tgitZo4jEJ5mLzD8"),
                    ("client_key", "tenor_web"),
                    ("locale", "en"),
                    ("anon_id", ""),
                    ("q", &query),
                    ("limit", "50"),
                    ("contentfilter", "low"),
                    ("media_filter", MEDIA_FILTER),
                    ("fields", FIELDS),
                ]);
                // `requests` drops the initial `None` position.
                if let Some(pos) = &pos {
                    params.append_pair("pos", pos);
                }
                params.append_pair("component", "web_desktop");
            }
            let origin = format!("{ROOT}/");
            let headers = http.browser_headers(&[("Referer", &origin), ("Origin", ROOT)]);
            let body = http.json_with(&url, &headers).await?;
            let gifs = body
                .get("results")
                .and_then(Value::as_array)
                .cloned()
                .ok_or_else(|| error("Tenor search response has no results"))?;
            let next = body
                .get("next")
                .and_then(Value::as_str)
                .filter(|next| !next.is_empty() && pages < 100_000)
                .map(|next| Some(next.to_owned()));
            Ok(Some((
                gifs_events(http, route, gifs, &metadata),
                (next, pages + 1),
            )))
        }
    })
}

/// The `format` option: a list or comma-separated string of format names.
fn formats(http: &Http) -> Vec<String> {
    match http.option("format") {
        None | Some(Value::Null) => ["gif", "mp4", "webm", "webp"].map(String::from).to_vec(),
        Some(Value::String(list)) => list.split(',').map(str::to_owned).collect(),
        Some(Value::Array(list)) => list
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        Some(_) => Vec::new(),
    }
}

/// `items()`: one post per GIF in the first available format; GIFs
/// without one are skipped.
fn gifs_events<'a>(
    http: &'a Http,
    route: &'static Route,
    gifs: Vec<Value>,
    metadata: &Map<String, Value>,
) -> Events<'a> {
    let formats = formats(http);
    let mut events = Vec::new();
    for gif in gifs {
        let Value::Object(mut gif) = gif else {
            continue;
        };
        let Some((name, mut format)) = formats.iter().find_map(|name| {
            let format = gif.get("media_formats")?.get(name)?.as_object()?;
            Some((name.clone(), format.clone()))
        }) else {
            continue;
        };
        format.insert("name".into(), name.clone().into());
        let dims = format.remove("dims");
        if let Some(Value::Object(formats)) = gif.get_mut("media_formats") {
            formats.insert(name.clone(), format.clone().into());
        }
        let raw = str_of(&Value::Object(format.clone()), "url").to_owned();
        let Some(url) = crate::sites::media_url(&raw, &["tenor.com"]) else {
            continue;
        };
        let title = match gif.remove("h1_title") {
            Some(Value::String(title)) => title.strip_suffix(" GIF").unwrap_or(&title).to_owned(),
            _ => String::new(),
        };
        gif.insert("title".into(), title.clone().into());
        let (width, height) = match dims.as_ref().and_then(Value::as_array) {
            Some(dims) if !dims.is_empty() => (
                dims.first().cloned().unwrap_or(Value::Null),
                dims.get(1).cloned().unwrap_or(Value::Null),
            ),
            _ => (0.into(), 0.into()),
        };
        gif.insert("width".into(), width);
        gif.insert("height".into(), height);
        let description = gif.remove("content_description").unwrap_or("".into());
        gif.insert("description".into(), description);
        let id_format = raw.rsplit('/').nth(1).unwrap_or_default().to_owned();
        gif.insert("id_format".into(), id_format.into());
        gif.insert("format".into(), name.into());
        for key in ["duration", "size"] {
            gif.insert(key.into(), format.get(key).cloned().unwrap_or(Value::Null));
        }
        let ts = ts_value(gif.get("created"));
        gif.insert("date".into(), date(ts));
        crate::sites::art::update(&mut gif, metadata);
        crate::sites::art::nameext_into(&raw, &mut gif);
        let id = gif.get("id").map(crate::sites::scalar).unwrap_or_default();
        let username = gif
            .get("user")
            .map(|user| str_of(user, "username").to_owned());
        let item = Item::new(url, gif)
            .ids(id.clone(), id)
            .user(username)
            .caption(Some(title))
            .timestamp(ts);
        events.extend(crate::sites::post(&SITE, http, route, vec![item]));
    }
    crate::sites::events(events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, json, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://tenor.com/view/dog-dance-gif-12345678"),
            Some(Target::Image("12345678".into()))
        );
        assert_eq!(
            t("https://tenor.com/de-DE/view/12345"),
            Some(Target::Image("12345".into()))
        );
        assert_eq!(
            t("https://tenor.com/en/search/happy-dog-gifs"),
            Some(Target::Search("happy-dog-gifs".into()))
        );
        assert_eq!(
            t("https://tenor.com/official/Tenor"),
            Some(Target::User("Tenor".into()))
        );
        assert_eq!(
            t("https://tenor.com/users/someone"),
            Some(Target::User("someone".into()))
        );
        for raw in [
            "https://tenor.com/",
            "https://tenor.com/view/no-id",
            "https://tenor.com/eng/view/123",
            "https://www.tenor.com/view/123",
            "https://media.tenor.com/view/123",
            "https://tenor.com.evil.test/view/123",
            "https://eviltenor.com/view/123",
            "https://tenor.com:8080/view/123",
            "https://u:p@tenor.com/view/123",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn gif(id: &str) -> Value {
        json!({"id": id, "h1_title": "Happy Dog GIF", "created": 1700000000.5,
            "content_description": "a dog", "user": {"username": "doglover"},
            "media_formats": {
                "mp4": {"url": format!("https://media.tenor.com/{id}AAAPo/happy-dog.mp4"),
                        "dims": [498, 280], "duration": 1.2, "size": 1000},
                "gif": {"url": format!("https://media.tenor.com/{id}AAAAC/happy-dog.gif"),
                        "dims": [498, 280], "duration": 0, "size": 5000}}})
    }

    #[tokio::test]
    async fn image_reads_store_cache() {
        let cache = json!({"gifs": {"byId": {"x": {"results": [gif("abc")]}}}});
        let page = format!("<script id=\"store-cache\" type=\"text/x-cache\">{cache}</script>");
        let (root, server) = serve(vec![html(&page)]);
        let http = http("tenor", root);
        let files = media(extract(
            &http,
            Url::parse("https://tenor.com/view/happy-dog-gif-123").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].url.as_str(),
            "https://media.tenor.com/abcAAAAC/happy-dog.gif"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["title"], "Happy Dog");
        assert_eq!(k["description"], "a dog");
        assert_eq!(k["format"], "gif");
        assert_eq!(k["id_format"], "abcAAAAC");
        assert_eq!(k["width"], 498);
        assert_eq!(k["size"], 5000);
        assert_eq!(k["date"], "2023-11-14 22:13:20");
        assert!(!k.contains_key("h1_title"));
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "abc Happy Dog.gif");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("tenor")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{id}",["abc"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /view/123 "));
    }

    #[tokio::test]
    async fn search_follows_next_position() {
        let (root, server) = serve(vec![
            json(json!({"results": [gif("a")], "next": "CAo"})),
            json(json!({"results": [gif("b")], "next": ""})),
        ]);
        let http = http("tenor", root).with_options(Some(std::sync::Arc::new(
            json!({"format": "mp4,gif"}).as_object().unwrap().clone(),
        )));
        let files = media(extract(
            &http,
            Url::parse("https://tenor.com/search/happy-dog-gifs").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[1]);
        assert_eq!(k["format"], "mp4");
        assert_eq!(k["search_tags"], "happy dog");
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "b Happy Dog.mp4");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("tenor/happy dog")
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /v2/search?appversion=browser-r20250225-1&"));
        assert!(requests[0].contains("&q=happy+dog&limit=50&"));
        assert!(!requests[0].contains("&pos="));
        assert!(requests[1].contains("&pos=CAo&component=web_desktop "));
        assert!(
            requests[1]
                .to_ascii_lowercase()
                .contains("referer: https://tenor.com/")
        );
    }

    #[tokio::test]
    async fn user_searches_by_handle() {
        let (root, server) = serve(vec![json(json!({"results": [gif("c")]}))]);
        let http = http("tenor", root);
        let files = media(extract(
            &http,
            Url::parse("https://tenor.com/users/doglover").unwrap(),
        ))
        .await;
        let (_, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("tenor/@doglover")
        );
        let requests = server.join().unwrap();
        assert!(requests[0].contains("&q=%40doglover&"));
    }
}
