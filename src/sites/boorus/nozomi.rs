//! nozomi.la posts, index pages, tags and searches, mirroring
//! gallery_dl/extractor/nozomi.py (big-endian `.nozomi` post ID lists).
use crate::error;
use crate::sites::gdl::{captures, date, group, parse_iso, quote, re, unquote};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use std::collections::{BTreeSet, VecDeque};
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
    /// `.nozomi` path and page number.
    Index(String, Option<String>),
    Tag(String, String),
    Search(String),
}

fn target(url: &Url) -> Option<Target> {
    if let Some(c) = captures(re!(r"^https?://nozomi\.la/post/(\d+)"), url) {
        return Some(Target::Post(group(&c, 1).into()));
    }
    if let Some(c) = captures(
        re!(r"^https?://nozomi\.la/(?:(index(?:-Popular)?)-(\d+)\.html)?(?:$|#|\?)"),
        url,
    ) {
        let index = c.get(1).map_or("index", |m| m.as_str());
        return Some(Target::Index(
            format!("/{index}.nozomi"),
            c.get(2).map(|m| m.as_str().to_owned()),
        ));
    }
    if let Some(c) = captures(re!(r"^https?://nozomi\.la/tag/([^/?#]+)-(\d+)\."), url) {
        return Some(Target::Tag(unquote(group(&c, 1)), group(&c, 2).into()));
    }
    captures(re!(r"^https?://nozomi\.la/search\.html\?q=([^&#]+)"), url)
        .map(|c| Target::Search(group(&c, 1).into()))
}

/// `decode_nozomi`: big-endian 32-bit post IDs.
fn decode(bytes: &[u8]) -> Vec<u32> {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| u32::from_be_bytes(*b))
        .collect()
}

fn names(value: Option<&Value>) -> Value {
    match value.and_then(Value::as_array) {
        Some(list) if !list.is_empty() => list
            .iter()
            .map(|x| x["tagname_display"].clone())
            .collect::<Vec<_>>()
            .into(),
        _ => Value::Array(Vec::new()),
    }
}

/// One post's files; missing posts (HTTP errors) are skipped like upstream.
async fn post_items(http: &Http, post_id: &str, extra: &Map<String, Value>) -> Vec<Item> {
    let n = post_id.len();
    let url = format!(
        "https://j.{DOMAIN}/post/{}/{}/{post_id}.json",
        &post_id[n - 1..],
        &post_id[n.saturating_sub(3)..n - 1]
    );
    let Ok(url) = http.api(&url) else {
        return Vec::new();
    };
    let Ok(post) = http.json_with(&url, &[("Origin", ROOT)]).await else {
        return Vec::new();
    };
    let Some(mut post) = post.as_object().cloned() else {
        return Vec::new();
    };
    post.insert("tags".into(), names(post.get("general")));
    for key in ["artist", "copyright", "character"] {
        let value = names(post.get(key));
        post.insert(key.into(), value);
    }
    let posted = post
        .get("date")
        .and_then(Value::as_str)
        .map(|d| format!("{d}:00"));
    post.insert("date".into(), date(posted.as_deref().and_then(parse_iso)));
    post.extend(extra.clone());
    let images = post
        .get("imageurls")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for key in ["general", "imageurl", "imageurls"] {
        post.remove(key);
    }
    images
        .into_iter()
        .enumerate()
        .filter_map(|(index, image)| {
            let did = crate::sites::scalar(&image["dataid"]);
            let video = crate::sites::gdl::truthy(image.get("is_video"));
            let kind = crate::sites::scalar(&image["type"]);
            let (subdomain, ext) = if video {
                ("v", kind)
            } else if kind == "gif" {
                ("g", kind)
            } else {
                ("w", "webp".to_owned())
            };
            let n = did.len();
            let url = format!(
                "https://{subdomain}.{DOMAIN}/{}/{}/{did}.{ext}",
                &did[n.checked_sub(1)?..],
                &did[n.saturating_sub(3)..n - 1]
            );
            let mut fields = post.clone();
            fields.insert("num".into(), (index + 1).into());
            fields.insert("filename".into(), did.as_str().into());
            fields.insert("dataid".into(), did.as_str().into());
            fields.insert("is_video".into(), video.into());
            fields.insert("extension".into(), ext.into());
            fields.insert("url".into(), url.as_str().into());
            Some(Item::new(Url::parse(&url).ok()?, fields).ids(post_id, did))
        })
        .collect()
}

/// Lazily walk post IDs, extracting each post as it is reached.
fn walk<'a>(
    http: &'a Http,
    route: &'static Route,
    source: Source,
    extra: Map<String, Value>,
) -> Events<'a> {
    crate::sites::paginate(
        (VecDeque::<String>::new(), Some(source)),
        move |(mut ids, source)| {
            let extra = extra.clone();
            async move {
                if let Some(id) = ids.pop_front() {
                    let items = post_items(http, &id, &extra).await;
                    return Ok(Some((
                        crate::sites::events(crate::sites::post(&SITE, http, route, items)),
                        (ids, source),
                    )));
                }
                let Some(source) = source else {
                    return Ok(None);
                };
                let (batch, next) = source.next(http).await?;
                Ok(Some((
                    crate::sites::events(Vec::new()),
                    (batch.into_iter().map(|id| id.to_string()).collect(), next),
                )))
            }
        },
    )
}

/// Where post IDs come from.
enum Source {
    /// Ranged reads of a `.nozomi` file, 256 bytes (64 IDs) at a time.
    Ranged {
        url: String,
        offset: u64,
    },
    Fixed(Vec<u32>),
}

impl Source {
    async fn next(self, http: &Http) -> crate::Result<(Vec<u32>, Option<Source>)> {
        match self {
            Source::Fixed(ids) => Ok((ids, None)),
            Source::Ranged { url, offset } => {
                let range = format!("bytes={offset}-{}", offset + 255);
                let (body, headers) = http
                    .get_with_headers(&http.api(&url)?, &[("Range", &range), ("Origin", ROOT)])
                    .await?;
                let next_offset = offset + 256;
                let total = headers
                    .get(reqwest::header::CONTENT_RANGE)
                    .and_then(|v| v.to_str().ok())
                    .map(|v| v.rsplit('/').next().unwrap_or_default().to_owned())
                    .map_or(next_offset as i64, |t| {
                        crate::sites::art::parse_int(&t, next_offset as i64)
                    });
                let next = (total > next_offset as i64).then_some(Source::Ranged {
                    url,
                    offset: next_offset,
                });
                Ok((decode(&body), next))
            }
        }
    }
}

async fn nozomi_ids(http: &Http, path: &str) -> crate::Result<Vec<u32>> {
    let body = http
        .get(
            &http.api(&format!("https://j.{DOMAIN}/{path}.nozomi"))?,
            &[("Origin", ROOT)],
        )
        .await?;
    Ok(decode(&body))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported nozomi.la URL"));
    };
    let ranged = |path: &str, page: Option<&str>| Source::Ranged {
        url: format!("https://n.nozomi.la{path}"),
        offset: (crate::sites::art::parse_int(page.unwrap_or_default(), 1) - 1).max(0) as u64 * 256,
    };
    match target {
        Target::Post(id) => walk(
            http,
            &POST,
            Source::Fixed(vec![id.parse().unwrap_or(0)]),
            Map::new(),
        ),
        Target::Index(path, page) => walk(http, &INDEX, ranged(&path, page.as_deref()), Map::new()),
        Target::Tag(tags, page) => {
            let mut extra = Map::new();
            extra.insert("search_tags".into(), tags.as_str().into());
            walk(
                http,
                &TAG,
                ranged(&format!("/nozomi/{tags}.nozomi"), Some(&page)),
                extra,
            )
        }
        Target::Search(query) => crate::sites::once(async move {
            let tags: Vec<String> = unquote(&query)
                .split_whitespace()
                .map(str::to_owned)
                .collect();
            let (mut positive, mut negative) = (Vec::new(), Vec::new());
            for tag in &tags {
                let quoted = quote(&tag.replace('/', ""));
                if tag.starts_with('-') {
                    negative.push(quoted)
                } else {
                    positive.push(quoted)
                }
            }
            let mut result: Option<BTreeSet<u32>> = None;
            for tag in &positive {
                let ids: BTreeSet<u32> = nozomi_ids(http, &format!("nozomi/{tag}"))
                    .await?
                    .into_iter()
                    .collect();
                result = Some(match result {
                    None => ids,
                    Some(current) => current.intersection(&ids).copied().collect(),
                });
            }
            let mut result = match result {
                Some(result) => result,
                None => nozomi_ids(http, "index").await?.into_iter().collect(),
            };
            for tag in &negative {
                for id in nozomi_ids(http, &format!("nozomi/{}", &tag[1..])).await? {
                    result.remove(&id);
                }
            }
            let ids: Vec<u32> = result.into_iter().rev().collect();
            let mut extra = Map::new();
            extra.insert("search_tags".into(), tags.into());
            Ok(walk(http, &SEARCH, Source::Fixed(ids), extra))
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http as client, json, keywords, media, serve};
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
            Some(Target::Index("/index.nozomi".into(), None))
        );
        assert_eq!(
            t("https://nozomi.la/index-Popular-33.html"),
            Some(Target::Index(
                "/index-Popular.nozomi".into(),
                Some("33".into())
            ))
        );
        assert_eq!(
            t("https://nozomi.la/tag/3:1_aspect_ratio-1.html"),
            Some(Target::Tag("3:1_aspect_ratio".into(), "1".into()))
        );
        assert_eq!(
            t("https://nozomi.la/search.html?q=hibiscus%203:4_ratio#1"),
            Some(Target::Search("hibiscus%203:4_ratio".into()))
        );
    }

    #[test]
    fn decodes_ids() {
        assert_eq!(
            decode(&[0, 0, 1, 0, 0, 0x37, 0xae, 0x6e]),
            vec![256, 3_649_134]
        );
    }

    #[tokio::test]
    async fn extracts_post_files() {
        let (root, server) = serve(vec![json(
            json!({"postid": 3649262, "date": "2016-07-26 02:32:03-05",
            "general": [{"tagname_display": "solo"}], "artist": null,
            "imageurls": [{"dataid": "2c1b2c4", "type": "jpg"}, {"dataid": "abcdef9", "type": "gif"}, {"dataid": "1234567", "type": "mp4", "is_video": true}]}),
        )]);
        let http = client("nozomi", root);
        let files = media(extract(
            &http,
            Url::parse("https://nozomi.la/post/3649262.html").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 3);
        assert_eq!(
            files[0].url.as_str(),
            "https://w.gold-usergeneratedcontent.net/4/2c/2c1b2c4.webp"
        );
        assert_eq!(
            files[1].url.as_str(),
            "https://g.gold-usergeneratedcontent.net/9/ef/abcdef9.gif"
        );
        assert_eq!(
            files[2].url.as_str(),
            "https://v.gold-usergeneratedcontent.net/7/56/1234567.mp4"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["tags"], json!(["solo"]));
        assert_eq!(k["date"], "2016-07-26 07:32:03");
        let (file, _) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "3649262 2c1b2c4.webp");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /post/2/26/3649262.json "));
    }
}
