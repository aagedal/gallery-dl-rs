//! Comic Vine image galleries, mirroring gallery_dl/extractor/comicvine.py:
//! a `BooruExtractor` over `/js/image-data.json`, 1000 images per page.
//!
//! The `url` option may name another post key for file URLs; a list of
//! keys and the `tags`/`notes` options (which need per-post HTML that
//! upstream's class does not define) are not supported.
use super::keywords;
use crate::sites::art::{segment, strptime};
use crate::sites::boorus::{booru_fields, file_url, item, per_post, unquote};
use crate::sites::{Events, Http, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

pub static SITE: Site = Site {
    category: "comicvine",
    name: "Comic Vine",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&TAG],
    api_key: false,
    cookies: None,
};

const ROOT: &str = "https://comicvine.gamespot.com";
const PER_PAGE: usize = 1000;
/// Image pages fetched for one gallery.
const MAX_PAGES: usize = 1000;

static TAG: Route = Route::new(
    "tag",
    "{filename}.{extension}",
    &["{category}", "{tag}"],
    "{id}",
);

#[derive(Debug, PartialEq)]
struct Target {
    /// `/NAME/ID/images/…`, raw, query included.
    path: String,
    name: String,
    object: String,
}

/// `comicvine\.gamespot\.com(/([^/?#]+)/(\d+-\d+)/images/.*)`.
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) || url.host_str() != Some("comicvine.gamespot.com") {
        return None;
    }
    let raw: Vec<&str> = url.path().splitn(5, '/').skip(1).collect();
    let [name, object, "images", ..] = raw.as_slice() else {
        return None;
    };
    let (a, b) = object.split_once('-')?;
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !segment(name) || !digits(a) || !digits(b) || raw.len() < 4 {
        return None;
    }
    let path = match url.query() {
        Some(query) => format!("{}?{query}", url.path()),
        None => url.path().to_owned(),
    };
    Some(Target {
        path,
        name: (*name).to_owned(),
        object: (*object).to_owned(),
    })
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(Target { path, name, object }) = target(&url) else {
        return crate::sites::fail(error("unsupported Comic Vine URL"));
    };
    crate::sites::once(async move {
        let page = http
            .text_with(
                &http.api(&format!("{ROOT}{path}"))?,
                &http.browser_headers(&[]),
            )
            .await?;
        let gallery = super::text::extract(&page, "data-gallery-id=\"", "\"", 0)
            .0
            .map(str::to_owned);
        let data = keywords([("tag", unquote(&name).into())]);
        Ok(crate::sites::paginate(Some(0usize), move |start| {
            let (gallery, object, data) = (gallery.clone(), object.clone(), data.clone());
            async move {
                let Some(start) = start else {
                    return Ok(None);
                };
                let images = image_page(http, gallery.as_deref(), &object, start).await?;
                let next = (images.len() >= PER_PAGE && start / PER_PAGE + 1 < MAX_PAGES)
                    .then_some(start + PER_PAGE);
                let items = images
                    .into_iter()
                    .filter_map(|post| post_item(http, post, &data))
                    .collect();
                let events = per_post(&SITE, http, &TAG, items);
                Ok(Some((crate::sites::events(events), next)))
            }
        }))
    })
}

/// One `image-data.json` page.
async fn image_page(
    http: &Http,
    gallery: Option<&str>,
    object: &str,
    start: usize,
) -> Result<Vec<Map<String, Value>>> {
    let mut url = http.api(&format!("{ROOT}/js/image-data.json"))?;
    {
        let mut query = url.query_pairs_mut();
        if let Some(gallery) = gallery {
            query.append_pair("images", gallery);
        }
        query
            .append_pair("start", &start.to_string())
            .append_pair("count", &PER_PAGE.to_string())
            .append_pair("object", object);
    }
    let body = http.json(&url).await?;
    Ok(body
        .get("images")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|image| image.as_object().cloned())
        .collect())
}

/// `BooruExtractor.items` for one image, with `ComicvineTagExtractor._prepare`.
fn post_item(
    http: &Http,
    mut post: Map<String, Value>,
    data: &Map<String, Value>,
) -> Option<crate::sites::Item> {
    let key = http.option_str("url").unwrap_or("original");
    let url = file_url(ROOT, post.get(key)?.as_str()?)?;
    booru_fields(&url, &mut post, data);
    let timestamp = post
        .get("dateCreated")
        .and_then(Value::as_str)
        .and_then(|date| strptime(date, "%a, %b %d %Y"));
    post.insert("date".into(), crate::sites::art::date(timestamp));
    let tags: Vec<Value> = post
        .get("tags")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|tag| tag.get("name"))
        .filter(|name| crate::sites::art::truthy(Some(name)))
        .cloned()
        .collect();
    post.insert("tags".into(), tags.into());
    Some(item(url, post, "", "caption", timestamp))
}

#[cfg(test)]
mod tests {
    use super::super::tests::{archive, path};
    use super::*;
    use crate::sites::testing::{html, http, json, keywords as kw, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://comicvine.gamespot.com/jock/4040-5653/images/"),
            Some(Target {
                path: "/jock/4040-5653/images/".into(),
                name: "jock".into(),
                object: "4040-5653".into(),
            })
        );
        assert_eq!(
            t("http://comicvine.gamespot.com/batman/4005-1699/images/x?page=2"),
            Some(Target {
                path: "/batman/4005-1699/images/x?page=2".into(),
                name: "batman".into(),
                object: "4005-1699".into(),
            })
        );
        for raw in [
            "https://comicvine.gamespot.com/jock/4040-5653/",
            "https://comicvine.gamespot.com/jock/4040/images/",
            "https://comicvine.gamespot.com/jock/4040-x/images/",
            "https://gamespot.com/jock/4040-5653/images/",
            "https://comicvine.gamespot.com.evil.test/jock/4040-5653/images/",
            "https://comicvine.gamespot.com:8443/jock/4040-5653/images/",
            "https://u:p@comicvine.gamespot.com/jock/4040-5653/images/",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[tokio::test]
    async fn extracts_gallery_images() {
        let page = r#"<div class="gallery" data-gallery-id="2915-5653" data-object-id="x">"#;
        let images = json!({"images": [
            {"id": 7148, "original": "https://comicvine.gamespot.com/a/uploads/original/1/17/71482-jock.jpg",
             "dateCreated": "Wed, Sep 17 2008", "caption": "Jock",
             "tags": [{"name": "Jock"}, {"name": ""}]},
            {"id": 7149, "dateCreated": "x", "tags": []}
        ]});
        let (root, requests) = serve(vec![html(page), json(images)]);
        let http = http("Comic Vine", root);
        let files = media(extract(
            &http,
            Url::parse("https://comicvine.gamespot.com/jock/4040-5653/images/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let k = kw(&files[0]);
        assert_eq!(k["id"], 7148);
        assert_eq!(k["tag"], "jock");
        assert_eq!(k["date"], "2008-09-17 00:00:00");
        assert_eq!(k["tags"], json!(["Jock"]));
        assert_eq!(k["filename"], "71482-jock");
        assert_eq!(path(&files[0]), "comicvine/jock/71482-jock.jpg");
        assert_eq!(archive(&files[0]), r#"["{id}",["7148"]]"#);
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /jock/4040-5653/images/ "));
        assert!(requests[1].starts_with(
            "GET /js/image-data.json?images=2915-5653&start=0&count=1000&object=4040-5653 "
        ));
    }

    #[tokio::test]
    async fn pages_through_full_responses() {
        let full: Vec<Value> = (0..1000)
            .map(|i| json!({"id": i, "original": format!("/a/{i}.jpg"), "tags": []}))
            .collect();
        let (root, requests) = serve(vec![
            html(""),
            json(json!({"images": full})),
            json(json!({"images": []})),
        ]);
        let http = http("Comic Vine", root);
        let files = media(extract(
            &http,
            Url::parse("https://comicvine.gamespot.com/x/1-2/images/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1000);
        assert_eq!(
            files[0].url.as_str(),
            "https://comicvine.gamespot.com/a/0.jpg"
        );
        let requests = requests.join().unwrap();
        assert!(requests[1].starts_with("GET /js/image-data.json?start=0&count=1000&object=1-2 "));
        assert!(
            requests[2].starts_with("GET /js/image-data.json?start=1000&count=1000&object=1-2 ")
        );
    }
}
