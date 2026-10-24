//! BBC programme galleries, mirroring gallery_dl/extractor/bbc.py: a
//! gallery page lists its thumbnails, rewritten to the `width` option's
//! size (default 1920); a programme's gallery listing queues each gallery.
//!
//! Upstream's smaller-size fallback URLs are not tried.
use crate::sites::art::{
    extr, extract_iter, json_loads, nameext_into, on_host, parse_int, segment,
};
use crate::sites::{Events, Http, Item, Route, Site, unescape};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://www.bbc.co.uk";
static GALLERY: Route = Route::new(
    "gallery",
    "{num:>02}.{extension}",
    &["{category}", "{path:I}"],
    "{programme}_{num}",
);
static PROGRAMME: Route = Route::new("programme", "{filename}.{extension}", &["{category}"], "");

pub static SITE: Site = Site {
    category: "bbc",
    name: "BBC",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&GALLERY, &PROGRAMME],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    /// The `/programmes/...` path.
    Gallery(String),
    /// The `/programmes/ID/galleries` path and the start page.
    Programme(String, i64),
}

/// `/programmes/ID(/NAME)?` ending the URL, unless `NAME` starts with
/// `galleries`; or `/programmes/ID/galleries(/?\?page=N)?`.
fn target(url: &Url) -> Option<Target> {
    if !on_host(url, &["bbc.co.uk"]) {
        return None;
    }
    let rest = url.path().strip_prefix("/programmes/")?;
    let parts: Vec<&str> = rest.split('/').collect();
    let id = parts.first().copied().filter(|id| segment(id))?;
    // The programme pattern has no end anchor: `galleriesX` matches too.
    if parts
        .get(1)
        .is_some_and(|name| name.starts_with("galleries"))
    {
        let page = match (parts[1] == "galleries", parts.get(2), url.query()) {
            (true, None, Some(query)) | (true, Some(&""), Some(query)) if parts.len() <= 3 => query
                .strip_prefix("page=")
                .and_then(crate::sites::art::leading_digits)
                .map_or(1, |n| parse_int(n, 1)),
            _ => 1,
        };
        return Some(Target::Programme(
            format!("/programmes/{id}/galleries"),
            page,
        ));
    }
    let whole = url.query().is_none() && url.fragment().is_none();
    match parts.as_slice() {
        [_] | [_, _] if whole && parts.iter().all(|p| segment(p)) => {
            Some(Target::Gallery(format!("/programmes/{rest}")))
        }
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Gallery(path)) => crate::sites::once(async move {
            let page_url = format!("{ROOT}{path}");
            let page = http.text(&http.api(&page_url)?).await?;
            let width = http
                .option("width")
                .map(|v| parse_int(&crate::sites::scalar(v), 0))
                .filter(|w| *w != 0)
                .map_or(1920, |w| w - w.rem_euclid(16));
            let items = parse_gallery(&page, &page_url, width)?;
            Ok(crate::sites::events(crate::sites::post(
                &SITE, http, &GALLERY, items,
            )))
        }),
        Some(Target::Programme(path, page)) => crate::sites::paginate(Some(page), move |page| {
            let path = path.clone();
            async move {
                let Some(page) = page else {
                    return Ok(None);
                };
                let mut url = http.api(&format!("{ROOT}{path}"))?;
                url.query_pairs_mut().append_pair("page", &page.to_string());
                let html = http.text(&url).await?;
                let children: Vec<Url> =
                    extract_iter(&html, "<a href=\"https://www.bbc.co.uk/programmes/", "\"")
                        .into_iter()
                        .filter_map(|id| Url::parse(&format!("{ROOT}/programmes/{id}")).ok())
                        .filter(|url| matches!(target(url), Some(Target::Gallery(_))))
                        .collect();
                let next = html.contains("rel=\"next\"").then_some(page + 1);
                let events = futures_util::StreamExt::boxed(futures_util::StreamExt::flatten(
                    futures_util::stream::iter(children.into_iter().map(move |child| {
                        crate::sites::queue(http, "bbc", "programme", child, &Map::new())
                    })),
                ));
                Ok(Some((events, next)))
            }
        }),
        None => crate::sites::fail(error("unsupported BBC URL")),
    }
}

/// GalleryExtractor `metadata()` and `images()`.
pub(crate) fn parse_gallery(page: &str, page_url: &str, width: i64) -> Result<Vec<Item>> {
    let jsonld = match extr(page, "<script type=\"application/ld+json\">", "</script>") {
        "" => extr(page, "<script type='application/ld+json'>", "</script>"),
        data => data,
    };
    let jsonld = json_loads(jsonld, "BBC")?;
    let mut path: Vec<Value> = Vec::new();
    for element in jsonld
        .get("itemListElement")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let name = element.get("name").cloned().unwrap_or(Value::Null);
        if !path.contains(&name) {
            path.push(name);
        }
    }
    let h1 = extr(page, "<h1>", "</h1>");
    let title = h1.rsplit_once("</span>").map_or(h1, |(_, title)| title);
    let mut data = Map::new();
    data.insert("title".into(), unescape(title).into());
    data.insert(
        "description".into(),
        unescape(extr(page, "property=\"og:description\" content=\"", "\"")).into(),
    );
    let programme = page_url.split('/').nth(4).unwrap_or_default();
    data.insert("programme".into(), programme.into());
    data.insert("path".into(), Value::Array(path));
    let dimensions = format!("/{width}xn/");
    let images: Vec<(String, Map<String, Value>)> =
        extract_iter(page, "class=\"gallery__thumbnail", ">")
            .into_iter()
            .map(|img| {
                let src = extr(img, "data-image-src=\"", "\"");
                let mut extra = Map::new();
                extra.insert(
                    "title_image".into(),
                    unescape(extr(img, "data-gallery-title=\"", "\"")).into(),
                );
                extra.insert(
                    "synopsis".into(),
                    unescape(extr(img, "data-gallery-synopsis=\"", "\"")).into(),
                );
                (src.replace("/320x180_b/", &dimensions), extra)
            })
            .collect();
    let count = images.len();
    data.insert("count".into(), count.into());
    let mut items = Vec::new();
    for (index, (raw, extra)) in images.into_iter().enumerate() {
        let Some(url) = crate::sites::art::file_url(&raw, ROOT) else {
            continue;
        };
        let mut fields = data.clone();
        crate::sites::art::update(&mut fields, &extra);
        fields.insert("num".into(), (index + 1).into());
        nameext_into(&raw, &mut fields);
        let caption = extra
            .get("title_image")
            .and_then(Value::as_str)
            .map(str::to_owned);
        items.push(
            Item::new(url, fields)
                .ids(programme, (index + 1).to_string())
                .caption(caption),
        );
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, queued, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.bbc.co.uk/programmes/p084qtzs/p085g9kg"),
            Some(Target::Gallery("/programmes/p084qtzs/p085g9kg".into()))
        );
        assert_eq!(
            t("https://bbc.co.uk/programmes/p084qtzs"),
            Some(Target::Gallery("/programmes/p084qtzs".into()))
        );
        assert_eq!(
            t("https://www.bbc.co.uk/programmes/b006q2x0/galleries?page=40"),
            Some(Target::Programme(
                "/programmes/b006q2x0/galleries".into(),
                40
            ))
        );
        assert_eq!(
            t("https://www.bbc.co.uk/programmes/b006q2x0/galleries"),
            Some(Target::Programme(
                "/programmes/b006q2x0/galleries".into(),
                1
            ))
        );
        assert_eq!(
            t("https://www.bbc.co.uk/programmes/b0/galleriesx?page=3"),
            Some(Target::Programme("/programmes/b0/galleries".into(), 1))
        );
        for raw in [
            "https://www.bbc.co.uk/programmes/a/b/c",
            "https://www.bbc.co.uk/programmes/a/b?x=1",
            "https://www.bbc.co.uk/programmes/",
            "https://www.bbc.co.uk.evil/programmes/a",
            "https://www.bbc.co.uk:8080/programmes/a",
            "https://u@www.bbc.co.uk/programmes/a",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[tokio::test]
    async fn extracts_gallery() {
        let page = r#"<script type="application/ld+json">{"itemListElement": [
            {"name": "Show"}, {"name": "Gallery"}, {"name": "Show"}]}</script>
            <h1><span>Show</span>Pictures &amp; more</h1>
            <meta property="og:description" content="Desc">
            <div class="gallery__thumbnail" data-image-src="https://ichef.bbci.co.uk/images/ic/320x180_b/p1.jpg"
              data-gallery-title="One" data-gallery-synopsis="S1">
            <div class="gallery__thumbnail" data-image-src="https://ichef.bbci.co.uk/images/ic/320x180_b/p2.jpg"
              data-gallery-title="Two" data-gallery-synopsis="S2">"#;
        let (root, server) = serve(vec![html(page)]);
        let options = serde_json::json!({"width": 1000}).as_object().cloned();
        let http = http("bbc", root).with_options(options.map(std::sync::Arc::new));
        let files = media(extract(
            &http,
            Url::parse("https://www.bbc.co.uk/programmes/p1/p2").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[1]);
        assert_eq!(k["title"], "Pictures & more");
        assert_eq!(k["programme"], "p1");
        assert_eq!(k["title_image"], "Two");
        assert_eq!(k["count"], 2);
        assert_eq!(
            files[1].url.as_str(),
            "https://ichef.bbci.co.uk/images/ic/992xn/p2.jpg"
        );
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "02.jpg");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("bbc/Show/Gallery")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{programme}_{num}",["p1","1"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /programmes/p1/p2 "));
    }

    #[tokio::test]
    async fn queues_programme_galleries() {
        let first = r#"<a href="https://www.bbc.co.uk/programmes/p1">x</a>
            <a href="https://www.bbc.co.uk/programmes/p2">y</a><link rel="next">"#;
        let second = r#"<a href="https://www.bbc.co.uk/programmes/p3">z</a>"#;
        let (root, server) = serve(vec![html(first), html(second)]);
        let http = http("bbc", root);
        let children = queued(extract(
            &http,
            Url::parse("https://www.bbc.co.uk/programmes/b0/galleries?page=2").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 3);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /programmes/b0/galleries?page=2 "));
        assert!(requests[1].starts_with("GET /programmes/b0/galleries?page=3 "));
    }
}
