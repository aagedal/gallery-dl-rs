//! ahottie albums, tags and searches, mirroring gallery_dl/extractor/ahottie.py.
use crate::error;
use crate::sites::gdl::{
    Scan, captures, date, extr, extract_iter, group, parse_iso, parse_query, re, rextr, rpartition,
    split_html, unescape, unquote,
};
use crate::sites::{Events, Http, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://ahottie.top";
const BASE: &str = r"^https?://(?:www\.)?ahottie\.top";

static GALLERY: Route = Route::new(
    "gallery",
    "{num:>03}.{extension}",
    &["{category}", "{date:%Y-%m-%d} {title} ({gallery_id})"],
    "{gallery_id}_{num}_{filename}",
);
static TAG: Route = Route::new("tag", "{filename}.{extension}", &["{category}"], "");
static SEARCH: Route = Route::new("search", "{filename}.{extension}", &["{category}"], "");

pub static SITE: Site = Site {
    category: "ahottie",
    name: "ahottie",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&GALLERY, &TAG, &SEARCH],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Gallery(String, String),
    Tag(String),
    Search(String),
}

fn target(url: &Url) -> Option<Target> {
    if let Some(c) = captures(re!(&format!(r"{BASE}(/albums/(\w+))")), url) {
        return Some(Target::Gallery(group(&c, 1).into(), group(&c, 2).into()));
    }
    if let Some(c) = captures(re!(&format!(r"{BASE}/tags/([^/?#]+)")), url) {
        return Some(Target::Tag(group(&c, 1).into()));
    }
    captures(re!(&format!(r"{BASE}/search/?\?([^#]+)")), url)
        .map(|c| Target::Search(group(&c, 1).into()))
}

/// `_pagination`: album links of listing pages, queued for the gallery.
fn listing<'a>(
    http: &'a Http,
    route: &'static Route,
    url: String,
    mut params: Vec<(String, String)>,
    extra: Map<String, Value>,
) -> Events<'a> {
    let first = params
        .iter()
        .position(|(k, _)| k == "page")
        .map_or(1, |i| crate::sites::art::parse_int(&params.remove(i).1, 1));
    crate::sites::paginate(Some(first), move |state| {
        let url = url.clone();
        let params = params.clone();
        let extra = extra.clone();
        async move {
            let Some(number) = state else { return Ok(None) };
            let mut request = http.api(&url)?;
            request
                .query_pairs_mut()
                .extend_pairs(params.iter())
                .append_pair("page", &number.to_string());
            let page = http.text(&request).await?;
            let albums = extract_iter(&page, "<div class=\"relative\">", "</div>")
                .into_iter()
                .filter_map(|album| {
                    let mut data = extra.clone();
                    data.insert("title".into(), unquote(extr(album, " alt=\"", "\"")).into());
                    data.insert(
                        "date".into(),
                        date(parse_iso(extr(album, " datetime=\"", "\""))),
                    );
                    Some((Url::parse(extr(album, " href=\"", "\"")).ok()?, data))
                })
                .collect();
            let next = page.contains("rel=\"next\"").then_some(number + 1);
            Ok(Some((super::queue(http, &SITE, route, albums), next)))
        }
    })
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported ahottie URL"));
    };
    match target {
        Target::Gallery(path, id) => crate::sites::once(async move {
            let mut page = http.text(&http.api(&format!("{ROOT}{path}"))?).await?;
            let mut scan = Scan::new(&page);
            let mut data = Map::new();
            data.insert("gallery_id".into(), id.as_str().into());
            data.insert(
                "title".into(),
                unescape(rpartition(scan.extr("<title>", "<"), " | ").0).into(),
            );
            data.insert(
                "date".into(),
                date(parse_iso(scan.extr("datetime=\"", "\""))),
            );
            let tags: Vec<String> = split_html(scan.extr("<i ", "</div>"))
                .into_iter()
                .skip(1)
                .collect();
            data.insert("tags".into(), tags.into());
            let mut images = Vec::new();
            loop {
                let start = page.find("<time ").map_or(0, |i| i + 1);
                images.extend(
                    extract_iter(&page[start..], "\" src=\"", "\"")
                        .into_iter()
                        .map(|u| (u.to_owned(), None)),
                );
                let Some(pos) = page[start..].find("rel=\"next\"").map(|i| start + i) else {
                    break;
                };
                let next = rextr(&page, "href=\"", "\"", Some(pos)).to_owned();
                page = http.text(&http.api(&next)?).await?;
            }
            Ok(crate::sites::events(super::numbered(
                &SITE, http, &GALLERY, &data, images, &id,
            )))
        }),
        Target::Tag(tag) => {
            let mut extra = Map::new();
            extra.insert("search_tags".into(), unquote(&tag).into());
            listing(http, &TAG, format!("{ROOT}/tags/{tag}"), Vec::new(), extra)
        }
        Target::Search(query) => {
            let params: Vec<(String, String)> = parse_query(&query)
                .into_iter()
                .map(|(k, v)| (k, crate::sites::scalar(&v)))
                .collect();
            let mut extra = Map::new();
            extra.insert(
                "search_tags".into(),
                params
                    .iter()
                    .find(|(k, _)| k == "kw")
                    .map_or(Value::Null, |(_, v)| v.as_str().into()),
            );
            listing(http, &SEARCH, format!("{ROOT}/search"), params, extra)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{children, html, http as client, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://ahottie.top/albums/1234567890"),
            Some(Target::Gallery(
                "/albums/1234567890".into(),
                "1234567890".into()
            ))
        );
        assert_eq!(
            t("https://ahottie.top/search?kw=cosplay"),
            Some(Target::Search("kw=cosplay".into()))
        );
        assert_eq!(
            t("https://ahottie.top/tags/cosplay"),
            Some(Target::Tag("cosplay".into()))
        );
    }

    #[tokio::test]
    async fn pages_gallery_images() {
        let first = r#"<title>Pretty Album | ahottie</title><time datetime="2024-01-02T03:04:05Z"></time><div><i class="t"></i><a>Tag A</a></div>
            <img alt="x" src="https://img.ahottie.top/1.jpg"><a href="https://ahottie.top/albums/123?page=2" rel="next">"#;
        let second = r#"<time></time><img alt="x" src="https://img.ahottie.top/2.jpg">"#;
        let (root, server) = serve(vec![html(first), html(second)]);
        let http = client("ahottie", root);
        let files = media(extract(
            &http,
            Url::parse("https://ahottie.top/albums/123").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[1]);
        assert_eq!(k["title"], "Pretty Album");
        assert_eq!(k["tags"], serde_json::json!(["Tag A"]));
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "002.jpg");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("ahottie/2024-01-02 Pretty Album (123)")
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn tag_listing_queues_albums() {
        let page = r#"<div class="relative"><a href="https://ahottie.top/albums/1"><img alt="One" src="t"><time datetime="2024-01-01T00:00:00Z"></time></div>"#;
        let (root, server) = serve(vec![html(page)]);
        let http = client("ahottie", root);
        assert_eq!(
            children(extract(
                &http,
                Url::parse("https://ahottie.top/tags/cosplay").unwrap()
            ))
            .await
            .len(),
            1
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /tags/cosplay?page=1 "));
    }
}
