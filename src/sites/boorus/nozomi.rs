//! nozomi.la posts, index pages, tags and searches, mirroring
//! gallery_dl/extractor/nozomi.py: `.nozomi` files are big-endian lists of
//! post ids (read in 256-byte ranges for listings), and each post's JSON
//! names its images on `gold-usergeneratedcontent.net`.
use super::{date_value, iso_timestamp, leading_digits};
use crate::sites::art;
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use futures_util::{StreamExt, stream};
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use url::Url;

const ROOT: &str = "https://nozomi.la";
const DOMAIN: &str = "gold-usergeneratedcontent.net";
const FILENAME: &str = "{postid} {dataid}.{extension}";
static POST: Route = Route::new("post", FILENAME, &["{category}"], "{dataid}");
static INDEX: Route = Route::new("index", FILENAME, &["{category}"], "{dataid}");
static TAG: Route = Route::new(
    "tag",
    FILENAME,
    &["{category}", "{search_tags}"],
    "t_{search_tags}_{dataid}",
);
static SEARCH: Route = Route::new(
    "search",
    FILENAME,
    &["{category}", "{search_tags:J }"],
    "t_{search_tags}_{dataid}",
);

pub static SITE: Site = Site {
    category: "nozomi",
    name: "nozomi.la",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&POST, &INDEX, &TAG, &SEARCH],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Post(String),
    /// A `.nozomi` path and the page number.
    Index(String, String),
    Tag(String, String),
    Search(String),
}

/// `nozomi\.la` paths (no `www.`), raw as upstream sees them.
fn target(url: &Url) -> Option<Target> {
    let rest = super::below(url, &["nozomi.la"])?;
    let path = url.path();
    if let Some(id) = rest.strip_prefix("/post/").and_then(leading_digits) {
        return Some(Target::Post(id.into()));
    }
    if path == "/" {
        return Some(Target::Index("/index.nozomi".into(), String::new()));
    }
    for index in ["index-Popular", "index"] {
        if let Some(page) = path
            .strip_prefix(&format!("/{index}-"))
            .and_then(|p| p.strip_suffix(".html"))
            .filter(|p| art::digits(p))
        {
            return Some(Target::Index(format!("/{index}.nozomi"), page.into()));
        }
    }
    if let Some(tail) = path.strip_prefix("/tag/") {
        // `([^/?#]+)-(\d+)\.`: the last `-DIGITS.` in the segment.
        let segment = super::until(tail, &['/']);
        let (tag, page) = segment.match_indices('-').rev().find_map(|(dash, _)| {
            let digits = leading_digits(&segment[dash + 1..])?;
            segment[dash + 1 + digits.len()..]
                .starts_with('.')
                .then(|| (&segment[..dash], digits))
        })?;
        return (!tag.is_empty()).then(|| Target::Tag(tag.into(), page.into()));
    }
    if path == "/search.html" {
        let query = url.query()?.strip_prefix("q=")?;
        let query = super::until(query, &['&']);
        return (!query.is_empty()).then(|| Target::Search(query.into()));
    }
    None
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported nozomi.la URL"));
    };
    match target {
        Target::Post(id) => posts(http, &POST, stream::iter([Ok(id)]).boxed(), Map::new()),
        Target::Index(nozomi, page) => posts(http, &INDEX, ranges(http, nozomi, &page), Map::new()),
        Target::Tag(tag, page) => {
            let tag = super::unquote(&tag);
            let mut data = Map::new();
            data.insert("search_tags".into(), tag.clone().into());
            let ids = ranges(http, format!("/nozomi/{tag}.nozomi"), &page);
            posts(http, &TAG, ids, data)
        }
        Target::Search(query) => {
            let tags: Vec<String> = super::unquote(&query)
                .split_whitespace()
                .map(str::to_owned)
                .collect();
            let mut data = Map::new();
            data.insert(
                "search_tags".into(),
                tags.iter().map(|t| Value::from(t.as_str())).collect(),
            );
            let ids = stream::once(search(http, tags))
                .map(|ids| match ids {
                    Ok(ids) => stream::iter(ids.into_iter().map(Ok)).boxed(),
                    Err(e) => stream::iter([Err(e)]).boxed(),
                })
                .flatten()
                .boxed();
            posts(http, &SEARCH, ids, data)
        }
    }
}

/// `decode_nozomi`: big-endian 32-bit post ids.
pub(crate) fn decode(bytes: &[u8]) -> Vec<u32> {
    bytes
        .chunks_exact(4)
        .map(|c| u32::from_be_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

type Ids<'a> = futures_util::stream::BoxStream<'a, Result<String>>;

/// Upstream `posts`: 256-byte ranges of a listing's `.nozomi` file while
/// its `Content-Range` total extends past them.
fn ranges<'a>(http: &'a Http, nozomi: String, page: &str) -> Ids<'a> {
    let offset = (art::parse_int(page, 1) - 1) * 256;
    futures_util::TryStreamExt::try_flatten(stream::try_unfold(Some(offset), move |offset| {
        let nozomi = nozomi.clone();
        async move {
            let Some(offset) = offset else {
                return Ok(None);
            };
            let url = http.api(&format!("https://n.nozomi.la{nozomi}"))?;
            let range = format!("bytes={offset}-{}", offset + 255);
            let (headers, body) = http
                .get_with_headers(&url, &[("Range", range.as_str()), ("Origin", ROOT)])
                .await?;
            let ids: Vec<Result<String>> = decode(&body)
                .into_iter()
                .map(|id| Ok(id.to_string()))
                .collect();
            let next = offset + 256;
            // `text.parse_int(cr, offset)`: a missing total ends the listing.
            let total = headers
                .get("content-range")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.rsplit('/').next())
                .map_or(next, |total| art::parse_int(total, next));
            let next = (total > next).then_some(next);
            Ok::<_, crate::Error>(Some((stream::iter(ids), next)))
        }
    }))
    .boxed()
}

/// Upstream `NozomiSearchExtractor.posts`: intersect the positive tags'
/// ids, subtract the negative ones, newest first.
async fn search(http: &Http, tags: Vec<String>) -> Result<Vec<String>> {
    let nozomi = |path: String| async move {
        let url = http.api(&format!("https://j.{DOMAIN}/{path}.nozomi"))?;
        Ok::<_, crate::Error>(decode(&http.get(&url, &[("Origin", ROOT)]).await?))
    };
    let (mut positive, mut negative) = (Vec::new(), Vec::new());
    for tag in &tags {
        let quoted = super::quote(&tag.replace('/', ""));
        if tag.starts_with('-') {
            negative.push(quoted);
        } else {
            positive.push(quoted);
        }
    }
    let mut result: Option<BTreeSet<u32>> = None;
    for tag in positive {
        let ids: BTreeSet<u32> = nozomi(format!("nozomi/{tag}")).await?.into_iter().collect();
        result = Some(match result {
            None => ids,
            Some(result) => result.intersection(&ids).copied().collect(),
        });
    }
    let mut result = match result {
        Some(result) => result,
        None => nozomi("index".into()).await?.into_iter().collect(),
    };
    for tag in negative {
        for id in nozomi(format!("nozomi/{}", &tag[1..])).await? {
            result.remove(&id);
        }
    }
    Ok(result.into_iter().rev().map(|id| id.to_string()).collect())
}

/// `s[-1]` and `s[-3:-1]` of an id, the directory parts of its URLs.
fn parts(id: &str) -> (&str, &str) {
    let len = id.len();
    (
        &id[len.saturating_sub(1)..],
        &id[len.saturating_sub(3)..len.saturating_sub(1)],
    )
}

/// Fetch each post's JSON as it is reached; unavailable posts are skipped.
fn posts<'a>(
    http: &'a Http,
    route: &'static Route,
    ids: Ids<'a>,
    data: Map<String, Value>,
) -> Events<'a> {
    ids.map(move |id| {
        let data = data.clone();
        crate::sites::once(async move {
            let id = id?;
            if !id.bytes().all(|b| b.is_ascii_digit()) {
                return Ok(crate::sites::events(Vec::new()));
            }
            let (last, middle) = parts(&id);
            let url = http.api(&format!(
                "https://j.{DOMAIN}/post/{last}/{middle}/{id}.json"
            ))?;
            let post = match http.json_with(&url, &[("Origin", ROOT)]).await {
                Ok(post) => post,
                Err(e)
                    if crate::sites::http::refused_status(&e).is_some()
                        || e.to_string().ends_with("resource not found") =>
                {
                    return Ok(crate::sites::events(Vec::new()));
                }
                Err(e) => return Err(e),
            };
            Ok(crate::sites::events(crate::sites::post(
                &SITE,
                http,
                route,
                parse_post(&post, &data),
            )))
        })
    })
    .flatten()
    .boxed()
}

/// One post's images with upstream keywords.
pub(crate) fn parse_post(post: &Value, data: &Map<String, Value>) -> Vec<Item> {
    let Some(mut post) = post.as_object().cloned() else {
        return Vec::new();
    };
    let list = |value: Option<&Value>| -> Value {
        value
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|tag| tag.get("tagname_display").cloned().unwrap_or(Value::Null))
            .collect()
    };
    let tags = list(post.get("general"));
    post.insert("tags".into(), tags);
    for key in ["artist", "copyright", "character"] {
        let value = list(post.get(key));
        post.insert(key.into(), value);
    }
    let timestamp = post
        .get("date")
        .and_then(Value::as_str)
        .and_then(|date| iso_timestamp(&format!("{date}:00")));
    post.insert("date".into(), date_value(timestamp));
    art::update(&mut post, data);
    let images = post
        .get("imageurls")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for key in ["general", "imageurl", "imageurls"] {
        post.remove(key);
    }
    let post_id = post
        .get("postid")
        .map(crate::sites::scalar)
        .unwrap_or_default();
    let mut items = Vec::new();
    for (index, image) in images.iter().enumerate() {
        let dataid = image
            .get("dataid")
            .map(crate::sites::scalar)
            .unwrap_or_default();
        if dataid.is_empty() || !dataid.bytes().all(|b| b.is_ascii_alphanumeric()) {
            continue;
        }
        let video = art::truthy(image.get("is_video"));
        let mut ext = image
            .get("type")
            .map(crate::sites::scalar)
            .unwrap_or_default();
        let subdomain = if video {
            "v"
        } else if ext == "gif" {
            "g"
        } else {
            ext = "webp".into();
            "w"
        };
        let (last, middle) = parts(&dataid);
        let raw = format!("https://{subdomain}.{DOMAIN}/{last}/{middle}/{dataid}.{ext}");
        let Ok(url) = Url::parse(&raw) else {
            continue;
        };
        let mut fields = post.clone();
        fields.insert("num".into(), (index + 1).into());
        fields.insert("filename".into(), dataid.clone().into());
        fields.insert("dataid".into(), dataid.clone().into());
        fields.insert("is_video".into(), video.into());
        fields.insert("extension".into(), ext.into());
        fields.insert("url".into(), raw.into());
        items.push(
            Item::new(url, fields)
                .ids(post_id.clone(), dataid)
                .timestamp(timestamp),
        );
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http, json, keywords, media, response, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://nozomi.la/post/3649262.html"),
            Some(Target::Post("3649262".into()))
        );
        assert_eq!(
            t("https://nozomi.la/"),
            Some(Target::Index("/index.nozomi".into(), String::new()))
        );
        assert_eq!(
            t("https://nozomi.la/index-Popular-33.html"),
            Some(Target::Index("/index-Popular.nozomi".into(), "33".into()))
        );
        assert_eq!(
            t("https://nozomi.la/tag/3:1_aspect_ratio-1.html"),
            Some(Target::Tag("3:1_aspect_ratio".into(), "1".into()))
        );
        assert_eq!(
            t("https://nozomi.la/tag/a-b-2.html"),
            Some(Target::Tag("a-b".into(), "2".into()))
        );
        assert_eq!(
            t("https://nozomi.la/search.html?q=hibiscus%203:4_ratio#1"),
            Some(Target::Search("hibiscus%203:4_ratio".into()))
        );
        for raw in [
            "https://nozomi.la/tag/a.html",
            "https://nozomi.la/index-x.html",
            "https://nozomi.la/search.html",
            "https://www.nozomi.la/post/1.html",
            "https://nozomi.la.evil.com/post/1.html",
            "https://nozomi.la:8080/post/1.html",
            "https://u:p@nozomi.la/post/1.html",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[test]
    fn decodes_ids() {
        assert_eq!(decode(&[0, 0, 1, 0, 0, 0, 0, 5, 9]), [256, 5]);
        assert_eq!(parts("3649262"), ("2", "26"));
        assert_eq!(parts("5"), ("5", ""));
    }

    fn post() -> Value {
        json!({"postid": 3649262, "date": "2020-01-02 03:04:05-05",
            "general": [{"tagname_display": "flower"}], "artist": [{"tagname_display": "someone"}],
            "copyright": null, "imageurl": "x",
            "imageurls": [{"dataid": "abc123", "type": "jpg"},
                          {"dataid": "def456", "type": "gif"},
                          {"dataid": "ghi789", "type": "mp4", "is_video": 1}]})
    }

    fn ranged(body: &[u8], total: usize) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-{}/{total}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len().saturating_sub(1),
            body.len()
        )
        .into_bytes();
        out.extend_from_slice(body);
        out
    }

    #[tokio::test]
    async fn extracts_post_images() {
        let (root, server) = serve(vec![json(post())]);
        let http = http("nozomi.la", root);
        let files = media(extract(
            &http,
            Url::parse("https://nozomi.la/post/3649262.html").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 3);
        assert_eq!(
            files[0].url.as_str(),
            "https://w.gold-usergeneratedcontent.net/3/12/abc123.webp"
        );
        assert_eq!(
            files[1].url.as_str(),
            "https://g.gold-usergeneratedcontent.net/6/45/def456.gif"
        );
        assert_eq!(
            files[2].url.as_str(),
            "https://v.gold-usergeneratedcontent.net/9/78/ghi789.mp4"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["tags"], json!(["flower"]));
        assert_eq!(k["artist"], json!(["someone"]));
        assert_eq!(k["copyright"], json!([]));
        assert_eq!(k["date"], "2020-01-02 08:04:05");
        assert_eq!(k["extension"], "webp");
        assert_eq!(k["is_video"], false);
        assert!(!k.contains_key("imageurls") && !k.contains_key("general"));
        assert_eq!(keywords(&files[2])["is_video"], true);
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "3649262 abc123.webp");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("nozomi")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{dataid}",["abc123"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /post/2/26/3649262.json "));
        assert!(requests[0].contains("origin: https://nozomi.la"));
    }

    #[tokio::test]
    async fn reads_tag_ranges_and_skips_missing_posts() {
        let first: Vec<u8> = (1..=64u32).flat_map(|id| id.to_be_bytes()).collect();
        let mut responses = vec![ranged(&first, 260)];
        responses.extend((1..=64).map(|_| response("404 Not Found", "text/plain", b"")));
        responses.push(ranged(&7u32.to_be_bytes(), 260));
        responses.push(json(post()));
        let (root, server) = serve(responses);
        let http = http("nozomi.la", root);
        let files = media(extract(
            &http,
            Url::parse("https://nozomi.la/tag/a%20b-1.html").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 3);
        let k = keywords(&files[0]);
        assert_eq!(k["search_tags"], "a b");
        let (_, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("nozomi/a b")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["t_{search_tags}_{dataid}",["a b","abc123"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /nozomi/a%20b.nozomi "));
        assert!(requests[0].contains("range: bytes=0-255"));
        assert!(requests[1].starts_with("GET /post/1//1.json "));
        assert!(requests[65].contains("range: bytes=256-511"));
        assert!(requests[66].starts_with("GET /post/7//7.json "));
    }

    #[tokio::test]
    async fn search_intersects_and_subtracts() {
        let ids = |ids: &[u32]| {
            response(
                "200 OK",
                "application/octet-stream",
                &ids.iter()
                    .flat_map(|id| id.to_be_bytes())
                    .collect::<Vec<_>>(),
            )
        };
        let (root, server) = serve(vec![
            ids(&[1, 2, 3, 4]),
            ids(&[2, 3, 4, 5]),
            ids(&[3]),
            response("404 Not Found", "text/plain", b""),
            response("404 Not Found", "text/plain", b""),
        ]);
        let http = http("nozomi.la", root);
        let files = media(extract(
            &http,
            Url::parse("https://nozomi.la/search.html?q=a%20b/c%20-d").unwrap(),
        ))
        .await;
        assert!(files.is_empty());
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /nozomi/a.nozomi "));
        assert!(requests[1].starts_with("GET /nozomi/bc.nozomi "));
        assert!(requests[2].starts_with("GET /nozomi/d.nozomi "));
        assert!(requests[3].starts_with("GET /post/4//4.json "));
        assert!(requests[4].starts_with("GET /post/2//2.json "));
    }
}
