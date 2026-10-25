//! Fuskator galleries and searches, mirroring
//! gallery_dl/extractor/fuskator.py: a gallery's image list comes from the
//! `ajax/gal.aspx` endpoint after an `ajax/auth.aspx` token; searches queue
//! each gallery page by page.
use crate::sites::art::{extr, extract_iter, on_host};
use crate::sites::filehosts::rextr;
use crate::sites::http::Body;
use crate::sites::manga::{Gallery, keywords, text};
use crate::sites::{Events, Http, Route, Site, unescape};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://fuskator.com";
static GALLERY: Route = Route::new(
    "gallery",
    "{category}_{gallery_id}_{num:>03}.{extension}",
    &["{category}", "{gallery_id} {title}"],
    "{gallery_id}_{num}",
);
static SEARCH: Route = Route::new("search", "{filename}.{extension}", &["{category}"], "");

pub static SITE: Site = Site {
    category: "fuskator",
    name: "Fuskator",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&GALLERY, &SEARCH],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Gallery(String),
    /// The search path and query, raw.
    Search(String),
}

/// `fuskator\.com/(?:thumbs|expanded)/([^/?#]+)` and
/// `fuskator\.com(/(?:search|page)/.+)`.
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) || url.host_str() != Some("fuskator.com") {
        return None;
    }
    let raw: Vec<&str> = url.path().split('/').skip(1).collect();
    match raw.as_slice() {
        ["thumbs" | "expanded", hash, ..] if !hash.is_empty() => {
            Some(Target::Gallery((*hash).to_owned()))
        }
        ["search" | "page", ..] => {
            let rest = &url[url::Position::BeforePath..url::Position::AfterQuery];
            let kind = raw[0].len() + 2;
            (rest.len() > kind).then(|| Target::Search(rest.to_owned()))
        }
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Gallery(hash)) => crate::sites::once(gallery(http, hash)),
        Some(Target::Search(path)) => search(http, format!("{ROOT}{path}")),
        None => crate::sites::fail(error("unsupported Fuskator URL")),
    }
}

/// `FuskatorGalleryExtractor`: the gallery page's title, then the image
/// list behind the auth token.
async fn gallery(http: &Http, hash: String) -> Result<Events<'_>> {
    let page_url = format!("{ROOT}/thumbs/{hash}/index.html");
    let page = super::page(http, &page_url, &[]).await?;
    let headers = http.browser_headers(&[
        ("Referer", &page_url),
        ("X-Requested-With", "XMLHttpRequest"),
    ]);
    let auth = http
        .post(
            &http.api(&format!("{ROOT}/ajax/auth.aspx"))?,
            Body::Form(&[]),
            &headers,
        )
        .await?;
    let auth = String::from_utf8_lossy(&auth).into_owned();
    let mut url = http.api(&format!("{ROOT}/ajax/gal.aspx"))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    url.query_pairs_mut()
        .append_pair("X-Auth", &auth)
        .append_pair("hash", &hash)
        .append_pair("_", &now.to_string());
    let data = http.json_with(&url, &headers).await?;
    let pages = data["images"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|image| {
            let url = format!("https:{}", image["imageUrl"].as_str()?);
            Some((url, image.as_object().cloned()))
        })
        .collect();
    let metadata = metadata(&page, &hash, &data);
    let id = crate::sites::scalar(&metadata["gallery_id"]);
    Ok(crate::sites::events(
        Gallery {
            enum_key: "num",
            ..Gallery::chapter(&GALLERY, metadata, pages).id(id)
        }
        .events(&SITE, http),
    ))
}

/// `FuskatorGalleryExtractor.metadata`.
fn metadata(page: &str, hash: &str, data: &Value) -> Map<String, Value> {
    let title = extr(page, "<title>", "</title>").trim();
    let (title, _, gallery_id) = crate::sites::filehosts::rpartition(title, "#");
    // `title[:-15]` drops the site's title suffix.
    let keep = title.chars().count().saturating_sub(15);
    let title: String = title.chars().take(keep).collect();
    let tags: Vec<&str> = data["tags"]
        .as_str()
        .unwrap_or_default()
        .split(',')
        .collect();
    keywords([
        ("gallery_id", text::parse_int(gallery_id).into()),
        ("gallery_hash", hash.into()),
        ("title", unescape(&title).into()),
        ("views", data.get("hits").cloned().unwrap_or(Value::Null)),
        ("score", data.get("rating").cloned().unwrap_or(Value::Null)),
        ("tags", tags.into()),
    ])
}

/// `FuskatorSearchExtractor.items`: each listed gallery, following the
/// last link before `>>`.
fn search(http: &Http, url: String) -> Events<'_> {
    crate::sites::paginate(Some((url, 0)), move |state| async move {
        let Some((url, count)) = state else {
            return Ok(None);
        };
        let page = super::page(http, &url, &[]).await?;
        let children = extract_iter(&page, "class=\"pic_pad\"><a href=\"", "\"")
            .into_iter()
            .filter_map(|path| Url::parse(&format!("{ROOT}{path}")).ok())
            .map(|url| (url, Map::new()))
            .collect();
        let pages = extr(&page, "class=\"pages\"><span>", ">&gt;&gt;<");
        let next = (!pages.is_empty() && count + 1 < super::MAX_PAGES)
            .then(|| {
                (
                    format!("{ROOT}{}", rextr(pages, "href=\"", "\"", None)),
                    count + 1,
                )
            })
            .filter(|(next, _)| {
                Url::parse(next).is_ok_and(|next| on_host(&next, &["fuskator.com"]))
            });
        Ok(Some((
            crate::sites::manga::queue_urls(http, &SITE, &SEARCH, children),
            next,
        )))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::manga::tests::{archive, path};
    use crate::sites::testing::{html, http, json, keywords as kw, media, queued, response, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://fuskator.com/thumbs/d0GnIzXrSKU/"),
            Some(Target::Gallery("d0GnIzXrSKU".into()))
        );
        assert_eq!(
            t("http://fuskator.com/expanded/x/index.html"),
            Some(Target::Gallery("x".into()))
        );
        assert_eq!(
            t("https://fuskator.com/search/red_hair/?x=1"),
            Some(Target::Search("/search/red_hair/?x=1".into()))
        );
        assert_eq!(
            t("https://fuskator.com/page/3/"),
            Some(Target::Search("/page/3/".into()))
        );
        for raw in [
            "https://fuskator.com/search/",
            "https://fuskator.com/thumbs/",
            "https://www.fuskator.com/thumbs/x/",
            "https://fuskator.com.evil.test/thumbs/x/",
            "https://fuskator.com:8443/thumbs/x/",
            "https://u:p@fuskator.com/thumbs/x/",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[tokio::test]
    async fn extracts_gallery() {
        let page = "<title> Red &amp; Blue - Fuskator.com#12345</title>";
        let data = json!({"hits": 77, "rating": 4.2, "tags": "a,b",
            "images": [{"imageUrl": "//i.fuskator.com/large/x/01.jpg", "width": 800},
                       {"imageUrl": "//i.fuskator.com/large/x/02.jpg", "width": 900}]});
        let (root, server) = serve(vec![
            html(page),
            response("200 OK", "text/plain", b"TOKEN"),
            json(data),
        ]);
        let http = http("fuskator", root);
        let files = media(extract(
            &http,
            Url::parse("https://fuskator.com/thumbs/hashX/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[1].url.as_str(),
            "https://i.fuskator.com/large/x/02.jpg"
        );
        let k = kw(&files[1]);
        assert_eq!(k["gallery_id"], 12345);
        assert_eq!(k["gallery_hash"], "hashX");
        assert_eq!(k["title"], "Red & Blue");
        assert_eq!(k["views"], 77);
        assert_eq!(k["score"], 4.2);
        assert_eq!(k["tags"], json!(["a", "b"]));
        assert_eq!(k["width"], 900);
        assert_eq!(k["imageUrl"], "//i.fuskator.com/large/x/02.jpg");
        assert_eq!(
            path(&files[1]),
            "fuskator/12345 Red & Blue/fuskator_12345_002.jpg"
        );
        assert_eq!(
            archive(&files[1]),
            r#"["{gallery_id}_{num}",["12345","2"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /thumbs/hashX/index.html "));
        assert!(requests[1].starts_with("POST /ajax/auth.aspx "));
        assert!(requests[1].contains("x-requested-with: XMLHttpRequest"));
        assert!(requests[2].starts_with("GET /ajax/gal.aspx?X-Auth=TOKEN&hash=hashX&_="));
    }

    #[tokio::test]
    async fn search_follows_pages() {
        let first = r#"<div class="pic_pad"><a href="/thumbs/a/">a</a></div>
<div class="pages"><span>1</span><a href="/search/x/2/">2</a><a href="/search/x/2/">&gt;&gt;</a></div>"#;
        let second = r#"<div class="pic_pad"><a href="/thumbs/b/">b</a></div>"#;
        let (root, server) = serve(vec![html(first), html(second)]);
        let http = http("fuskator", root);
        let children = queued(extract(
            &http,
            Url::parse("https://fuskator.com/search/x/").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].subcategory(), "search");
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /search/x/2/ "));
    }
}
