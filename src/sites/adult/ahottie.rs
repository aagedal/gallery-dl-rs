//! AHottie albums, tags and searches, mirroring
//! gallery_dl/extractor/ahottie.py: an album's images span its pages,
//! followed by `rel="next"`; tag and search listings queue each album.
//!
//! Files are requested without a Referer, as upstream does. Upstream's
//! check that skips the site's placeholder image is not ported.
use super::{set_param, with_query};
use crate::sites::art::{Scan, date, extr, on_host, parse_iso, parse_query, split_html};
use crate::sites::filehosts::{extract as find_between, rextr, rpartition};
use crate::sites::manga::{Gallery, keywords};
use crate::sites::{Events, Http, Route, Site, percent_decode, unescape};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://ahottie.top";
static GALLERY: Route = Route::new(
    "gallery",
    "{num:>03}.{extension}",
    &["{category}", "{date:%Y-%m-%d} {title} ({gallery_id})"],
    "{gallery_id}_{num}_{filename}",
);
static TAG: Route = Route::new("tag", "{filename}.{extension}", &["{category}"], "");
static SEARCH: Route = Route::new("search", "{filename}.{extension}", &["{category}"], "");

/// Album pages one URL may follow.
const MAX_ALBUM_PAGES: usize = 1_000;

pub static SITE: Site = Site {
    category: "ahottie",
    name: "AHottie",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&GALLERY, &TAG, &SEARCH],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Gallery(String),
    Tag(String),
    /// The raw query string.
    Search(String),
}

/// `(/albums/(\w+))` and `/tags/([^/?#]+)` (unanchored) and
/// `/search/?\?([^#]+)`, in class order.
fn target(url: &Url) -> Option<Target> {
    if !on_host(url, &["ahottie.top"]) {
        return None;
    }
    let path = url.path();
    if let Some(rest) = path.strip_prefix("/albums/") {
        let len = rest
            .bytes()
            .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_')
            .count();
        if len > 0 {
            return Some(Target::Gallery(rest[..len].to_owned()));
        }
    }
    if let Some(tag) = path.strip_prefix("/tags/") {
        let tag = &tag[..tag.find('/').unwrap_or(tag.len())];
        if !tag.is_empty() {
            return Some(Target::Tag(tag.to_owned()));
        }
    }
    let query = url.query().filter(|q| !q.is_empty())?;
    matches!(path, "/search" | "/search/").then(|| Target::Search(query.to_owned()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Gallery(id)) => crate::sites::once(gallery(http, id)),
        Some(Target::Tag(tag)) => {
            let search_tags = Value::from(percent_decode(&tag));
            listing(
                http,
                &TAG,
                format!("{ROOT}/tags/{tag}"),
                Vec::new(),
                search_tags,
            )
        }
        Some(Target::Search(query)) => {
            let params = parse_query(&query);
            let search_tags = params
                .iter()
                .find(|(key, _)| key == "kw")
                .map_or(Value::Null, |(_, kw)| kw.clone().into());
            listing(http, &SEARCH, format!("{ROOT}/search"), params, search_tags)
        }
        None => crate::sites::fail(error("unsupported AHottie URL")),
    }
}

/// `AhottieGalleryExtractor`: metadata from the first page, images from
/// every page.
async fn gallery(http: &Http, id: String) -> Result<Events<'_>> {
    let mut page = super::page(http, &format!("{ROOT}/albums/{id}"), &[]).await?;
    let mut e = Scan::new(&page);
    let title = rpartition(e.extr("<title>", "<"), " | ").0;
    let data = keywords([
        ("gallery_id", id.clone().into()),
        ("title", unescape(title).into()),
        ("date", date(parse_iso(e.extr("datetime=\"", "\"")))),
        (
            "tags",
            split_html(e.extr("<i ", "</div>"))
                .into_iter()
                .skip(1)
                .collect::<Vec<_>>()
                .into(),
        ),
    ]);
    let mut pages = Vec::new();
    for _ in 0..MAX_ALBUM_PAGES {
        let start = page.find("<time ").map_or(0, |pos| pos + 1);
        let mut pos = start;
        while let Some((url, next)) = find_between(&page, "\" src=\"", "\"", pos) {
            pages.push((url.to_owned(), None));
            pos = next;
        }
        let Some(next) = page[start..].find("rel=\"next\"").map(|i| start + i) else {
            break;
        };
        let next = rextr(&page, "href=\"", "\"", Some(next)).to_owned();
        match Url::parse(&next) {
            Ok(url) if on_host(&url, &["ahottie.top"]) => {}
            _ => break,
        }
        page = super::page(http, &next, &[]).await?;
    }
    Ok(crate::sites::events(
        Gallery {
            enum_key: "num",
            ..Gallery::chapter(&GALLERY, data, pages).id(id)
        }
        .events(&SITE, http),
    ))
}

/// `AhottieExtractor._pagination`: each album of each page with its title
/// and date, while the page links a next one.
fn listing<'a>(
    http: &'a Http,
    route: &'static Route,
    url: String,
    params: Vec<(String, String)>,
    search_tags: Value,
) -> Events<'a> {
    let start = crate::sites::art::parse_int(
        params
            .iter()
            .find(|(key, _)| key == "page")
            .map_or("", |(_, value)| value),
        1,
    );
    crate::sites::paginate(Some((start, 0usize)), move |state| {
        let url = url.clone();
        let mut params = params.clone();
        let search_tags = search_tags.clone();
        async move {
            let Some((pnum, count)) = state else {
                return Ok(None);
            };
            set_param(&mut params, "page", pnum.to_string());
            let page = http
                .text_with(
                    &with_query(http, &url, &params)?,
                    &http.browser_headers(&[]),
                )
                .await?;
            let children =
                crate::sites::art::extract_iter(&page, "<div class=\"relative\">", "</div>")
                    .into_iter()
                    .filter_map(|album| {
                        let url = Url::parse(extr(album, " href=\"", "\"")).ok()?;
                        let mut data = Map::new();
                        data.insert("url".into(), url.as_str().into());
                        data.insert(
                            "title".into(),
                            percent_decode(extr(album, " alt=\"", "\"")).into(),
                        );
                        data.insert(
                            "date".into(),
                            date(parse_iso(extr(album, " datetime=\"", "\""))),
                        );
                        data.insert("search_tags".into(), search_tags.clone());
                        Some((url, data))
                    })
                    .collect();
            let next = (page.contains("rel=\"next\"") && count + 1 < super::MAX_PAGES)
                .then_some((pnum + 1, count + 1));
            Ok(Some((
                crate::sites::manga::queue_urls(http, &SITE, route, children),
                next,
            )))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::manga::tests::{archive, path};
    use crate::sites::testing::{html, http, keywords as kw, media, queued, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://ahottie.top/albums/1234567890"),
            Some(Target::Gallery("1234567890".into()))
        );
        assert_eq!(
            t("https://www.ahottie.top/albums/ab_c-d?page=2"),
            Some(Target::Gallery("ab_c".into()))
        );
        assert_eq!(
            t("https://ahottie.top/tags/Some%20Tag/x"),
            Some(Target::Tag("Some%20Tag".into()))
        );
        assert_eq!(
            t("https://ahottie.top/search/?kw=a+b&page=2"),
            Some(Target::Search("kw=a+b&page=2".into()))
        );
        for raw in [
            "https://ahottie.top/albums/",
            "https://ahottie.top/search",
            "https://ahottie.top/searches?kw=a",
            "https://m.ahottie.top/albums/1",
            "https://ahottie.top.evil.test/albums/1",
            "https://ahottie.top:8443/albums/1",
            "https://u:p@ahottie.top/albums/1",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[tokio::test]
    async fn extracts_album_pages() {
        let first = r#"<title>Garden &amp; Rain | AHottie</title>
<time datetime="2024-04-05T06:07:08+00:00">x</time>
<div><i class="tag"></i><a>Garden</a><a>Rain</a></div>
<img alt="x" src="https://img.test/a/1.jpg"><img alt="y" src="https://img.test/a/2.jpg">
<a href="https://ahottie.top/albums/abc?page=2" rel="next">"#;
        let second = r#"<img alt="z" src="https://img.test/a/3.webp">"#;
        let (root, server) = serve(vec![html(first), html(second)]);
        let http = http("ahottie", root);
        let files = media(extract(
            &http,
            Url::parse("https://ahottie.top/albums/abc").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 3);
        assert!(!files[0].headers.contains_key("Referer"));
        let k = kw(&files[2]);
        assert_eq!(k["gallery_id"], "abc");
        assert_eq!(k["title"], "Garden & Rain");
        assert_eq!(k["date"], "2024-04-05 06:07:08");
        assert_eq!(k["tags"], json!(["Garden", "Rain"]));
        assert_eq!(k["count"], 3);
        assert_eq!(
            path(&files[2]),
            "ahottie/2024-04-05 Garden & Rain (abc)/003.webp"
        );
        assert_eq!(
            archive(&files[2]),
            r#"["{gallery_id}_{num}_{filename}",["abc","3","3"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /albums/abc "));
        assert!(requests[1].starts_with("GET /albums/abc?page=2 "));
    }

    #[tokio::test]
    async fn search_queues_albums() {
        let page = r#"<div class="relative"><a href="https://ahottie.top/albums/x1"><img alt="A%20B"><time datetime="2024-01-01T00:00:00+00:00"></time></a></div>
<a rel="next">"#;
        let last = r#"<div class="relative"><a href="https://ahottie.top/albums/x2"></a></div>"#;
        let (root, server) = serve(vec![html(page), html(last)]);
        let http = http("ahottie", root);
        let children = queued(extract(
            &http,
            Url::parse("https://ahottie.top/search?kw=two+words").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].subcategory(), "search");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /search?kw=two+words&page=1 "));
        assert!(requests[1].starts_with("GET /search?kw=two+words&page=2 "));
    }
}
