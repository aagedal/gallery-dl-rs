//! Hotleak posts, creators, categories and searches, mirroring
//! gallery_dl/extractor/hotleak.py. Videos are HLS streams for yt-dlp.
use crate::error;
use crate::sites::gdl::{
    captures, extr, extract_iter, group, nameext_into, parse_int, parse_query, re, unescape,
    ytdl_item,
};
use crate::sites::{Events, Http, Item, Route, Site};
use base64::Engine;
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://hotleak.vip";
const BASE: &str = r"^https?://(?:www\.)?hotleak\.vip";
const FILENAME: &str = "{creator}_{id}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{creator}"];
const ARCHIVE: &str = "{type}_{creator}_{id}";

static POST: Route = Route::new("post", FILENAME, DIRECTORY, ARCHIVE);
static CREATOR: Route = Route::new("creator", FILENAME, DIRECTORY, ARCHIVE);
static CATEGORY: Route = Route::new("category", FILENAME, DIRECTORY, ARCHIVE);
static SEARCH: Route = Route::new("search", FILENAME, DIRECTORY, ARCHIVE);

pub static SITE: Site = Site {
    category: "hotleak",
    name: "Hotleak",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&POST, &CREATOR, &CATEGORY, &SEARCH],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Post(String, String, String),
    Creator(String),
    Category(String, String),
    Search(String),
}

fn reserved(name: &str) -> bool {
    matches!(name, "hot" | "creators" | "videos" | "photos")
}

fn target(url: &Url) -> Option<Target> {
    if let Some(c) = captures(re!(&format!(r"{BASE}/([^/]+)/(photo|video)/(\d+)")), url) {
        return (!reserved(&c[1])).then(|| {
            Target::Post(
                group(&c, 1).into(),
                group(&c, 2).into(),
                group(&c, 3).into(),
            )
        });
    }
    if let Some(c) = captures(
        re!(&format!(
            r"{BASE}/(hot|creators|videos|photos)(?:/?\?([^#]+))?"
        )),
        url,
    ) {
        return Some(Target::Category(
            group(&c, 1).into(),
            c.get(2).map_or("", |m| m.as_str()).into(),
        ));
    }
    if let Some(c) = captures(re!(&format!(r"{BASE}/search/?\?([^#]+)")), url) {
        return Some(Target::Search(group(&c, 1).into()));
    }
    let c = captures(re!(&format!(r"{BASE}/([^/?#]+)/?$")), url)?;
    (!reserved(&c[1])).then(|| Target::Creator(group(&c, 1).into()))
}

/// `decode_video_url`: drop 16 characters at each end, reverse, base64.
fn decode_video_url(raw: &str) -> Option<String> {
    let chars: Vec<char> = raw.chars().collect();
    let inner: String = chars
        .get(16..chars.len().checked_sub(16)?)?
        .iter()
        .rev()
        .collect();
    String::from_utf8(
        base64::engine::general_purpose::STANDARD
            .decode(inner)
            .ok()?,
    )
    .ok()
}

/// `HotleakExtractor.items` for one post.
fn item(mut data: Map<String, Value>) -> Option<Item> {
    let raw = crate::sites::scalar(&data["url"]);
    let id = crate::sites::scalar(&data["id"]);
    data.insert("_http_expected_status".into(), vec![404].into());
    let item = if let Some(stream) = raw.strip_prefix("ytdl:") {
        ytdl_item(stream, data)?
    } else {
        let raw = raw
            .replace("/storage/storage/", "/storage/")
            .replace("_thumb.", ".");
        data.insert("url".into(), raw.as_str().into());
        Item::new(Url::parse(&raw).ok()?, data)
    };
    Some(item.ids(id.clone(), id))
}

fn video(data: &mut Map<String, Value>, encoded: &str) {
    let url = format!("ytdl:{}", decode_video_url(encoded).unwrap_or_default());
    nameext_into(&url, data);
    data.insert("url".into(), url.into());
    data.insert("extension".into(), "mp4".into());
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Hotleak URL"));
    };
    match target {
        Target::Post(creator, kind, id) => crate::sites::once(async move {
            let page = http
                .text(&http.api(&format!("{ROOT}/{creator}/{kind}/{id}"))?)
                .await?;
            let page = extr(&page, "<div class=\"movie-image thumb\">", "</article>");
            let mut data = Map::new();
            data.insert("id".into(), parse_int(&id).into());
            data.insert("creator".into(), creator.into());
            data.insert("type".into(), kind.as_str().into());
            if kind == "photo" {
                let src = extr(page, "data-src=\"", "\"");
                data.insert("url".into(), src.into());
                nameext_into(src, &mut data);
            } else {
                video(&mut data, extr(&unescape(page), "\"src\":\"", "\""));
            }
            let items = item(data).into_iter().collect();
            Ok(crate::sites::events(crate::sites::post(
                &SITE, http, &POST, items,
            )))
        }),
        Target::Creator(creator) => crate::sites::paginate(Some(1u64), move |state| {
            let creator = creator.clone();
            async move {
                let Some(number) = state else { return Ok(None) };
                let mut request = http.api(&format!("{ROOT}/{creator}"))?;
                request
                    .query_pairs_mut()
                    .append_pair("page", &number.to_string());
                let posts = http
                    .json_with(&request, &[("X-Requested-With", "XMLHttpRequest")])
                    .await?;
                let posts = posts.as_array().cloned().unwrap_or_default();
                if posts.is_empty() {
                    return Ok(None);
                }
                let items = posts
                    .iter()
                    .filter_map(|post| {
                        let mut data = Map::new();
                        data.insert("creator".into(), creator.as_str().into());
                        data.insert(
                            "id".into(),
                            parse_int(&crate::sites::scalar(&post["id"])).into(),
                        );
                        match post["type"].as_i64() {
                            Some(0) => {
                                let url = format!(
                                    "{ROOT}/storage/{}",
                                    crate::sites::scalar(&post["image"])
                                );
                                data.insert("type".into(), "photo".into());
                                nameext_into(&url, &mut data);
                                data.insert("url".into(), url.into());
                            }
                            Some(1) => {
                                data.insert("type".into(), "video".into());
                                video(
                                    &mut data,
                                    post["stream_url_play"].as_str().unwrap_or_default(),
                                );
                            }
                            _ => return None,
                        }
                        item(data)
                    })
                    .flat_map(|item| crate::sites::post(&SITE, http, &CREATOR, vec![item]))
                    .collect();
                Ok(Some((crate::sites::events(items), Some(number + 1))))
            }
        }),
        Target::Category(category, params) => {
            listing(http, &CATEGORY, format!("{ROOT}/{category}"), params)
        }
        Target::Search(params) => listing(http, &SEARCH, format!("{ROOT}/search"), params),
    }
}

/// `HotleakExtractor._pagination`: article links until a page has none.
fn listing<'a>(http: &'a Http, route: &'static Route, base: String, params: String) -> Events<'a> {
    let mut query = parse_query(&params);
    let first = crate::sites::gdl::int_of(query.get("page")).max(1);
    query.remove("page");
    crate::sites::paginate(Some(first), move |state| {
        let base = base.clone();
        let query = query.clone();
        async move {
            let Some(number) = state else { return Ok(None) };
            let mut url = http.api(&base)?;
            {
                let mut pairs = url.query_pairs_mut();
                for (key, value) in &query {
                    pairs.append_pair(key, &crate::sites::scalar(value));
                }
                pairs.append_pair("page", &number.to_string());
            }
            let page = http.text(&url).await?;
            if !page.contains("</article>") {
                return Ok(None);
            }
            let children = extract_iter(&page, "<article class=\"movie-item", "</article>")
                .into_iter()
                .filter_map(|item| {
                    Some((Url::parse(extr(item, "<a href=\"", "\"")).ok()?, Map::new()))
                })
                .collect();
            Ok(Some((
                super::queue(http, &SITE, route, children),
                Some(number + 1),
            )))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{children, html, http as client, json, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://hotleak.vip/jane/photo/12345"),
            Some(Target::Post("jane".into(), "photo".into(), "12345".into()))
        );
        assert_eq!(
            t("https://hotleak.vip/jane"),
            Some(Target::Creator("jane".into()))
        );
        assert_eq!(
            t("https://hotleak.vip/photos"),
            Some(Target::Category("photos".into(), String::new()))
        );
        assert_eq!(
            t("https://hotleak.vip/hot?page=2"),
            Some(Target::Category("hot".into(), "page=2".into()))
        );
        assert_eq!(
            t("https://hotleak.vip/search?search=jane"),
            Some(Target::Search("search=jane".into()))
        );
    }

    fn encode(url: &str) -> String {
        let b64: String = base64::engine::general_purpose::STANDARD
            .encode(url)
            .chars()
            .rev()
            .collect();
        format!("{0}{b64}{0}", "x".repeat(16))
    }

    #[test]
    fn decodes_video_urls() {
        assert_eq!(
            decode_video_url(&encode("https://cdn/a.m3u8")).unwrap(),
            "https://cdn/a.m3u8"
        );
    }

    #[tokio::test]
    async fn creator_posts_photos_and_videos() {
        let (root, server) = serve(vec![
            json(
                json!([{"id": 1, "type": 0, "image": "storage/a_thumb.jpg"}, {"id": "2", "type": 1, "stream_url_play": encode("https://cdn.hotleak.vip/v/2.m3u8")}]),
            ),
            json(json!([])),
        ]);
        let http = client("hotleak", root);
        let files = media(extract(
            &http,
            Url::parse("https://hotleak.vip/jane").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].url.as_str(), "https://hotleak.vip/storage/a.jpg");
        assert_eq!(
            files[1].url.as_str(),
            "ytdl:https://cdn.hotleak.vip/v/2.m3u8"
        );
        assert_eq!(keywords(&files[1])["extension"], "mp4");
        assert_eq!(
            files[0].handoff["_http_expected_status"],
            serde_json::json!([404])
        );
        let (file, _) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "jane_2.mp4");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /jane?page=1 "));
    }

    #[tokio::test]
    async fn category_queues_articles() {
        let page = r#"<article class="movie-item"><a href="https://hotleak.vip/jane/photo/1">x</a></article>"#;
        let (root, server) = serve(vec![html(page), html("none")]);
        let http = client("hotleak", root);
        assert_eq!(
            children(extract(
                &http,
                Url::parse("https://hotleak.vip/photos").unwrap()
            ))
            .await
            .len(),
            1
        );
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /photos?page=2 "));
    }
}
