//! tumblrgallery.xyz blogs, posts and searches, mirroring
//! gallery_dl/extractor/tumblrgallery.py by scraping the `xx-co-me` image
//! links of each page.
//!
//! A blog gallery ends at the first page that fails or redirects (upstream
//! stops at any status of 300 or above). The search's unused first page
//! request is not repeated before the result pages are read.
use super::set_nameext;
use crate::sites::art::{digits, extr, extract_iter};
use crate::sites::nameext;
use crate::sites::{Events, Http, Item, Route, Site, remove_html, unescape};
use crate::{Result, error};
use futures_util::{StreamExt, stream};
use serde_json::{Map, Value};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use url::Url;

const ROOT: &str = "https://tumblrgallery.xyz";
const FILENAME: &str = "{category}_{gallery_id}_{num:>03}_{id}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{gallery_id} {title}"];
const ARCHIVE: &str = "{gallery_id}_{num}";
static TUMBLRBLOG: Route = Route::new("tumblrblog", FILENAME, DIRECTORY, ARCHIVE);
static POST: Route = Route::new("post", FILENAME, DIRECTORY, ARCHIVE);
static SEARCH: Route = Route::new(
    "search",
    "{category}_{num:>03}_{gallery_id}_{id}_{title}.{extension}",
    &["{category}", "{search_term}"],
    ARCHIVE,
);

pub static SITE: Site = Site {
    category: "tumblrgallery",
    name: "tumblrgallery",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&TUMBLRBLOG, &POST, &SEARCH],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Tumblrblog(String),
    Post(String),
    Search(String),
}

/// `/tumblrblog/gallery/(\d+)\.html`, `/post/(\d+)\.html` or
/// `/s\.php\?q=([^&#]+)` on the bare host.
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) || url.host_str() != Some("tumblrgallery.xyz") {
        return None;
    }
    let path = url.path();
    let id = |rest: &str| {
        let id = rest.split_once(".html")?.0;
        digits(id).then(|| id.to_owned())
    };
    if let Some(rest) = path.strip_prefix("/tumblrblog/gallery/") {
        return id(rest).map(Target::Tumblrblog);
    }
    if let Some(rest) = path.strip_prefix("/post/") {
        return id(rest).map(Target::Post);
    }
    if path != "/s.php" {
        return None;
    }
    let term = url.query()?.strip_prefix("q=")?.split('&').next()?;
    (!term.is_empty()).then(|| Target::Search(term.into()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Tumblrblog(id)) => crate::sites::once(async move {
            let base = format!("{ROOT}/tumblrblog/gallery/{id}");
            let page = http.text(&http.api(&format!("{base}.html"))?).await?;
            let mut data = Map::new();
            data.insert(
                "title".into(),
                unescape(extr(&page, "<h1>", "</h1>")).into(),
            );
            data.insert("gallery_id".into(), id.parse::<u64>().unwrap_or(0).into());
            let num = Arc::new(AtomicUsize::new(0));
            Ok(crate::sites::paginate(Some(1u64), move |pnum| {
                let (base, data, num) = (base.clone(), data.clone(), num.clone());
                async move {
                    let Some(pnum) = pnum else {
                        return Ok(None);
                    };
                    let url = http.api(&format!("{base}/{pnum}.html"))?;
                    let page = match http.page(&url, &[]).await {
                        Ok((location, page)) if location == url => page,
                        _ => return Ok(None),
                    };
                    let items = images(&page, &data, &num);
                    Ok(Some((
                        crate::sites::events(crate::sites::post(&SITE, http, &TUMBLRBLOG, items)),
                        Some(pnum + 1),
                    )))
                }
            }))
        }),
        Some(Target::Post(id)) => crate::sites::once(async move {
            let page = http
                .text(&http.api(&format!("{ROOT}/post/{id}.html"))?)
                .await?;
            let mut data = Map::new();
            data.insert("title".into(), post_title(&page).into());
            data.insert("gallery_id".into(), id.parse::<u64>().unwrap_or(0).into());
            let items = images(&page, &data, &AtomicUsize::new(0));
            Ok(crate::sites::events(crate::sites::post(
                &SITE, http, &POST, items,
            )))
        }),
        Some(Target::Search(term)) => search(http, term),
        None => crate::sites::fail(error("unsupported tumblrgallery URL")),
    }
}

/// `TumblrgallerySearchExtractor.images`: each result post's images,
/// following the result pages' next link.
fn search(http: &Http, term: String) -> Events<'_> {
    let num = Arc::new(AtomicUsize::new(0));
    let first = format!("s.php?q={term}");
    crate::sites::paginate(Some(first), move |page_url| {
        let (term, num) = (term.clone(), num.clone());
        async move {
            let Some(page_url) = page_url else {
                return Ok(None);
            };
            let page = http.text(&page_link(http, &page_url)?).await?;
            let ids: Vec<String> =
                extract_iter(&page, "<div class=\"title\"><a href=\"post/", ".html")
                    .into_iter()
                    .filter(|id| digits(id))
                    .map(str::to_owned)
                    .collect();
            let events = crate::sites::pages(stream::iter(ids).then(move |id| {
                let (term, num) = (term.clone(), num.clone());
                async move {
                    let post = http
                        .text(&http.api(&format!("{ROOT}/post/{id}.html"))?)
                        .await?;
                    let mut data = Map::new();
                    data.insert("search_term".into(), term.into());
                    data.insert("gallery_id".into(), id.into());
                    data.insert("title".into(), post_title(&post).into());
                    let items = images(&post, &data, &num);
                    Ok(crate::sites::events(crate::sites::post(
                        &SITE, http, &SEARCH, items,
                    )))
                }
            }));
            let next = extr(&page, "</span> <a class=\"btn btn-primary\" href=\"", "\"");
            let next = (!next.is_empty() && next != page_url).then(|| next.to_owned());
            Ok(Some((events, next)))
        }
    })
}

/// `root + "/" + page_url`, which must stay on the site.
fn page_link(http: &Http, page_url: &str) -> Result<Url> {
    let url = Url::parse(&format!("{ROOT}/{page_url}"))
        .ok()
        .filter(|url| url.host_str() == Some("tumblrgallery.xyz"))
        .ok_or_else(|| error("tumblrgallery next page leaves the site"))?;
    http.api(url.as_str())
}

/// The post page title, with tags removed and `_` replaced by `-`.
fn post_title(page: &str) -> String {
    remove_html(&unescape(extr(page, "<title>", "</title>"))).replace('_', "-")
}

/// `_urls_from_page` with `_data_from_url`, numbered after `num`.
pub(crate) fn images(page: &str, data: &Map<String, Value>, num: &AtomicUsize) -> Vec<Item> {
    extract_iter(
        page,
        "<div class=\"report\"> <a class=\"xx-co-me\" href=\"",
        "\"",
    )
    .into_iter()
    .filter_map(|raw| {
        let url = crate::sites::art::file_url(raw, ROOT)?;
        let (filename, ext) = nameext(&url);
        let parts: Vec<&str> = filename.split('_').collect();
        let id = match parts.get(1) {
            Some(&"inline") => parts.get(2).copied(),
            other => other.copied(),
        }
        .unwrap_or(&filename)
        .to_owned();
        let index = num.fetch_add(1, Ordering::Relaxed) + 1;
        let mut fields = data.clone();
        fields.insert("id".into(), id.clone().into());
        fields.insert("num".into(), index.into());
        set_nameext(&mut fields, (filename, ext));
        let gallery = fields.get("gallery_id").map(crate::sites::scalar);
        let title = fields
            .get("title")
            .and_then(Value::as_str)
            .map(str::to_owned);
        Some(
            Item::new(url, fields)
                .ids(gallery.unwrap_or_default(), id)
                .caption(title),
        )
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, response, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://tumblrgallery.xyz/tumblrblog/gallery/103975.html"),
            Some(Target::Tumblrblog("103975".into()))
        );
        assert_eq!(
            t("https://tumblrgallery.xyz/post/405674.html"),
            Some(Target::Post("405674".into()))
        );
        assert_eq!(
            t("https://tumblrgallery.xyz/s.php?q=everyday-life&x=1"),
            Some(Target::Search("everyday-life".into()))
        );
        for raw in [
            "https://www.tumblrgallery.xyz/post/1.html",
            "https://tumblrgallery.xyz/post/x.html",
            "https://tumblrgallery.xyz/post/1",
            "https://tumblrgallery.xyz/s.php?x=1&q=a",
            "https://tumblrgallery.xyz.evil/post/1.html",
            "https://tumblrgallery.xyz:81/post/1.html",
            "https://u@tumblrgallery.xyz/post/1.html",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn links(names: &[&str]) -> String {
        names
            .iter()
            .map(|n| {
                format!(
                    r#"<div class="report"> <a class="xx-co-me" href="https://data.tumblrgallery.xyz/{n}">x</a>"#
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn blog_pages_until_failure() {
        let first = "<h1>Blog &amp; co</h1>";
        let (root, server) = serve(vec![
            html(first),
            html(&links(&[
                "tumblr_abc_1280.jpg",
                "tumblr_inline_xyz_540.png",
            ])),
            html(&links(&["single.gif"])),
            response("404 Not Found", "text/html", b""),
        ]);
        let http = http("tumblrgallery", root);
        let files = media(extract(
            &http,
            Url::parse("https://tumblrgallery.xyz/tumblrblog/gallery/7.html").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 3);
        let ids: Vec<_> = files.iter().map(|f| keywords(f)["id"].clone()).collect();
        assert_eq!(ids, ["abc", "xyz", "single"]);
        assert_eq!(keywords(&files[2])["num"], 3);
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            file.render(&files[0]).unwrap(),
            "tumblrgallery_7_001_abc.jpg"
        );
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("tumblrgallery/7 Blog & co")
        );
        let (_, identity) = crate::sites::archive_key(&files[2]).unwrap();
        assert_eq!(identity, r#"["{gallery_id}_{num}",["7","3"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /tumblrblog/gallery/7.html "));
        assert!(requests[1].starts_with("GET /tumblrblog/gallery/7/1.html "));
        assert!(requests[3].starts_with("GET /tumblrblog/gallery/7/3.html "));
    }

    #[tokio::test]
    async fn search_follows_results() {
        let results = r#"<div class="title"><a href="post/11.html">a</a>
            </span> <a class="btn btn-primary" href="s.php?q=cat&amp;page=2">"#;
        let last = r#"<div class="title"><a href="post/12.html">b</a>
            </span> <a class="btn btn-primary" href="s.php?q=cat&amp;page=2">"#;
        let post = |title: &str| format!("<title>{title}</title>{}", links(&["tumblr_p_1.jpg"]));
        let (root, server) = serve(vec![
            html(results),
            html(&post("A_b")),
            html(last),
            html(&post("C")),
        ]);
        let http = http("tumblrgallery", root);
        let files = media(extract(
            &http,
            Url::parse("https://tumblrgallery.xyz/s.php?q=cat").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[0]);
        assert_eq!(k["title"], "A-b");
        assert_eq!(k["gallery_id"], "11");
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(
            file.render(&files[1]).unwrap(),
            "tumblrgallery_002_12_p_C.jpg"
        );
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("tumblrgallery/cat")
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /s.php?q=cat "));
        assert!(requests[1].starts_with("GET /post/11.html "));
        assert!(requests[2].starts_with("GET /s.php?q=cat&amp;page=2 "));
    }
}
