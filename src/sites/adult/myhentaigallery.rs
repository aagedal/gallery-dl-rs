//! MyHentaiGallery galleries and tag listings, mirroring
//! gallery_dl/extractor/myhentaigallery.py: gallery pages list thumbnails
//! whose `/original/` counterparts are the files; listings follow `Next`.
use crate::sites::art::{Scan, extr, extract_iter, split_html};
use crate::sites::filehosts::rextr;
use crate::sites::manga::{Gallery, keywords, text};
use crate::sites::{Events, Http, Route, Site, unescape};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://myhentaigallery.com";
static GALLERY: Route = Route::new(
    "gallery",
    "{category}_{gallery_id}_{num:>03}.{extension}",
    &["{category}", "{gallery_id} {artist:?[/] /J, }{title}"],
    "{gallery_id}_{num}",
);
static TAG: Route = Route::new("tag", "{filename}.{extension}", &["{category}"], "");

pub static SITE: Site = Site {
    category: "myhentaigallery",
    name: "MyHentaiGallery",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&GALLERY, &TAG],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Gallery(String),
    /// The listing's path and query.
    Tag(String),
}

/// `myhentaigallery\.com` followed by
/// `/g(?:allery/(?:thumbnails|show))?/(\d+)` or
/// `(/g/(artist|category|group|parody)/(\d+).*)`.
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) || url.host_str() != Some("myhentaigallery.com") {
        return None;
    }
    let path = url.path();
    let gallery = ["/g/", "/gallery/thumbnails/", "/gallery/show/"]
        .iter()
        .find_map(|prefix| path.strip_prefix(prefix))
        .and_then(crate::sites::art::leading_digits);
    if let Some(id) = gallery {
        return Some(Target::Gallery(id.to_owned()));
    }
    let raw: Vec<&str> = path.split('/').skip(1).collect();
    match raw.as_slice() {
        ["g", "artist" | "category" | "group" | "parody", id, ..]
            if crate::sites::art::leading_digits(id).is_some() =>
        {
            let query = url.query().map(|q| format!("?{q}")).unwrap_or_default();
            Some(Target::Tag(format!("{path}{query}")))
        }
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Gallery(id)) => crate::sites::once(async move {
            let page_url = format!("{ROOT}/g/{id}");
            let page = super::page(http, &page_url, &[("Referer", &page_url)]).await?;
            let data = metadata(&page, &id)?;
            let gallery = Gallery {
                enum_key: "num",
                ..Gallery::chapter(&GALLERY, data, images(&page))
                    .id(id)
                    .header("Referer", page_url)
            };
            Ok(crate::sites::events(gallery.events(&SITE, http)))
        }),
        Some(Target::Tag(path)) => tag(http, format!("{ROOT}{path}")),
        None => crate::sites::fail(error("unsupported MyHentaiGallery URL")),
    }
}

/// `MyhentaigalleryGalleryExtractor.metadata`.
fn metadata(page: &str, id: &str) -> Result<Map<String, Value>> {
    let mut e = Scan::new(page);
    let title = e
        .extr("<div class=\"comic-description\">\n", "</h1>")
        .trim_start();
    let title = title.strip_prefix("<h1>").unwrap_or(title);
    if title.is_empty() {
        return Err(error("MyHentaiGallery gallery not found"));
    }
    let title = unescape(title);
    Ok(keywords([
        ("title", title.into()),
        ("gallery_id", text::parse_int(id).into()),
        (
            "tags",
            split_html(e.extr("        Categories:", "</div>")).into(),
        ),
        (
            "artist",
            split_html(e.extr("        Artists:", "</div>")).into(),
        ),
        (
            "group",
            split_html(e.extr("        Groups:", "</div>")).into(),
        ),
        (
            "parodies",
            split_html(e.extr("        Parodies:", "</div>")).into(),
        ),
    ]))
}

/// Each thumbnail's `/original/` image.
fn images(page: &str) -> Vec<crate::sites::manga::Page> {
    extract_iter(page, "class=\"comic-thumb\"", "</div>")
        .into_iter()
        .map(|thumb| {
            let src = unescape(extr(thumb, "src=\"", "\""));
            (src.replace("/thumbnail/", "/original/"), None)
        })
        .collect()
}

/// `MyhentaigalleryTagExtractor.galleries`: each listed gallery, following
/// the link before `>Next<`.
fn tag(http: &Http, url: String) -> Events<'_> {
    crate::sites::paginate(Some((url, 0)), move |state| async move {
        let Some((url, count)) = state else {
            return Ok(None);
        };
        let page = super::page(http, &url, &[]).await?;
        let children = extract_iter(&page, "<div class=\"comic-inner\">", "<div")
            .into_iter()
            .filter_map(|inner| Url::parse(&format!("{ROOT}{}", extr(inner, "href=\"", "\""))).ok())
            .map(|url| (url, Map::new()))
            .collect();
        let next = page
            .find(">Next<")
            .map(|pos| format!("{ROOT}{}", rextr(&page, "href=\"", "\"", Some(pos))))
            .filter(|_| count + 1 < super::MAX_PAGES)
            .map(|next| (next, count + 1));
        Ok(Some((
            crate::sites::manga::queue_urls(http, &SITE, &TAG, children),
            next,
        )))
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
        let gallery = |id: &str| Some(Target::Gallery(id.into()));
        assert_eq!(t("https://myhentaigallery.com/g/16247"), gallery("16247"));
        assert_eq!(
            t("https://myhentaigallery.com/gallery/show/16247/1"),
            gallery("16247")
        );
        assert_eq!(
            t("http://myhentaigallery.com/gallery/thumbnails/16247"),
            gallery("16247")
        );
        assert_eq!(
            t("https://myhentaigallery.com/g/category/8?page=2"),
            Some(Target::Tag("/g/category/8?page=2".into()))
        );
        for raw in [
            "https://myhentaigallery.com/g/tag/8",
            "https://myhentaigallery.com/gallery/12",
            "https://www.myhentaigallery.com/g/1",
            "https://myhentaigallery.com.evil.test/g/1",
            "https://myhentaigallery.com:8443/g/1",
            "https://u:p@myhentaigallery.com/g/1",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    const PAGE: &str = "<div class=\"comic-description\">\n  <h1>Tea &amp; Cake</h1>\n\
        <div>        Categories: <a>Comedy</a> <a>Slice</a></div>\
        <div>        Artists: <a>Artist A</a></div>\
        <div>        Groups: </div><div>        Parodies: <a>Original</a></div>\
        <div class=\"comic-thumb\"><img src=\"https://images.test/thumbnail/1.jpg?a=1&amp;b=2\"></div>\
        <div class=\"comic-thumb\"><img src=\"https://images.test/thumbnail/2.png\"></div>";

    #[tokio::test]
    async fn extracts_gallery() {
        let (root, server) = serve(vec![html(PAGE)]);
        let http = http("myhentaigallery", root);
        let files = media(extract(
            &http,
            Url::parse("https://myhentaigallery.com/gallery/show/16247/1").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].url.as_str(),
            "https://images.test/original/1.jpg?a=1&b=2"
        );
        assert_eq!(
            files[0].headers["Referer"],
            "https://myhentaigallery.com/g/16247"
        );
        let k = kw(&files[1]);
        assert_eq!(k["title"], "Tea & Cake");
        assert_eq!(k["gallery_id"], 16247);
        assert_eq!(k["tags"], json!(["Comedy", "Slice"]));
        assert_eq!(k["artist"], json!(["Artist A"]));
        assert_eq!(k["group"], json!([]));
        assert_eq!(k["parodies"], json!(["Original"]));
        assert_eq!(k["count"], 2);
        assert_eq!(
            path(&files[1]),
            "myhentaigallery/16247 [Artist A] Tea & Cake/myhentaigallery_16247_002.png"
        );
        assert_eq!(
            archive(&files[1]),
            r#"["{gallery_id}_{num}",["16247","2"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /g/16247 "));
        assert!(requests[0].contains("referer: https://myhentaigallery.com/g/16247"));
    }

    #[tokio::test]
    async fn missing_title_is_an_error() {
        let (root, server) = serve(vec![html("<p>gone</p>")]);
        let http = http("myhentaigallery", root);
        let url = Url::parse("https://myhentaigallery.com/g/1").unwrap();
        assert!(crate::sites::testing::fails(extract(&http, url)).await);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn tag_follows_next() {
        let first = r#"<div class="comic-inner"><a href="/g/1">a</a><div></div>
<a href="/g/category/8?page=2">&gt;</a><a>>Next<</a>"#;
        let second = r#"<div class="comic-inner"><a href="/g/2">b</a><div>"#;
        let (root, server) = serve(vec![html(first), html(second)]);
        let http = http("myhentaigallery", root);
        let children = queued(extract(
            &http,
            Url::parse("https://myhentaigallery.com/g/category/8").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].subcategory(), "tag");
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /g/category/8?page=2 "));
    }
}
