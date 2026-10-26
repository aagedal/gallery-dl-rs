//! BBC programme galleries, mirroring gallery_dl/extractor/bbc.py.
use crate::error;
use crate::sites::gdl::{
    captures, extr, extract_iter, group, int_of, jsonld, partition, re, rpartition, unescape,
};
use crate::sites::{Events, Http, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://www.bbc.co.uk";
const BASE: &str = r"^https?://(?:www\.)?bbc\.co\.uk";

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
    Gallery(String),
    Programme(String, i64),
}

fn target(url: &Url) -> Option<Target> {
    if let Some(c) = captures(
        re!(&format!(r"{BASE}(/programmes/[^/?#]+(?:/([^/?#]+))?)$")),
        url,
    ) {
        // Upstream's `(?!/galleries)`.
        if c.get(2).is_none_or(|m| m.as_str() != "galleries") {
            return Some(Target::Gallery(group(&c, 1).into()));
        }
    }
    captures(
        re!(&format!(
            r"{BASE}(/programmes/[^/?#]+/galleries)(?:/?\?page=(\d+))?"
        )),
        url,
    )
    .map(|c| {
        Target::Programme(
            group(&c, 1).into(),
            c.get(2)
                .map_or(1, |m| crate::sites::gdl::parse_int(m.as_str())),
        )
    })
}

/// Unique names in order (`util.unique_sequence`).
fn path_names(data: &Value) -> Vec<Value> {
    let mut names: Vec<Value> = Vec::new();
    for element in data["itemListElement"].as_array().into_iter().flatten() {
        let name = element["name"].clone();
        if names.last() != Some(&name) {
            names.push(name);
        }
    }
    names
}

fn images(page: &str, width: i64) -> Vec<(String, Option<Map<String, Value>>)> {
    let dimensions = format!("/{width}xn/");
    extract_iter(page, "class=\"gallery__thumbnail", ">")
        .into_iter()
        .map(|img| {
            let src = extr(img, "data-image-src=\"", "\"");
            let (front, _, back) = partition(src, "/320x180_b/");
            let fallback: Vec<Value> = [1920, 1600, 1280, 976]
                .into_iter()
                .filter(|w| *w < width)
                .map(|w| format!("{front}/{w}xn/{back}").into())
                .collect();
            let mut file = Map::new();
            file.insert(
                "title_image".into(),
                unescape(extr(img, "data-gallery-title=\"", "\"")).into(),
            );
            file.insert(
                "synopsis".into(),
                unescape(extr(img, "data-gallery-synopsis=\"", "\"")).into(),
            );
            file.insert("_fallback".into(), fallback.into());
            (src.replace("/320x180_b/", &dimensions), Some(file))
        })
        .collect()
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported BBC URL"));
    };
    match target {
        Target::Gallery(path) => crate::sites::once(async move {
            let page_url = format!("{ROOT}{path}");
            let page = http.text(&http.api(&page_url)?).await?;
            let data = jsonld(&page)?;
            let programme = page_url.split('/').nth(4).unwrap_or_default().to_owned();
            let mut fields = Map::new();
            fields.insert(
                "title".into(),
                unescape(rpartition(extr(&page, "<h1>", "</h1>"), "</span>").2).into(),
            );
            fields.insert(
                "description".into(),
                unescape(extr(&page, "property=\"og:description\" content=\"", "\"")).into(),
            );
            fields.insert("programme".into(), programme.as_str().into());
            fields.insert("path".into(), path_names(&data).into());
            let width = match int_of(http.option("width")) {
                0 => 1920,
                width => width - width % 16,
            };
            Ok(crate::sites::events(crate::sites::galleries::numbered(
                &SITE,
                http,
                &GALLERY,
                &fields,
                images(&page, width),
                &programme,
            )))
        }),
        Target::Programme(path, first) => crate::sites::paginate(Some(first), move |state| {
            let path = path.clone();
            async move {
                let Some(number) = state else { return Ok(None) };
                let mut url = http.api(&format!("{ROOT}{path}"))?;
                url.query_pairs_mut()
                    .append_pair("page", &number.to_string());
                let page = http.text(&url).await?;
                let children =
                    extract_iter(&page, "<a href=\"https://www.bbc.co.uk/programmes/", "\"")
                        .into_iter()
                        .filter_map(|id| {
                            Some((
                                Url::parse(&format!("https://www.bbc.co.uk/programmes/{id}"))
                                    .ok()?,
                                Map::new(),
                            ))
                        })
                        .collect();
                let next = page.contains("rel=\"next\"").then_some(number + 1);
                Ok(Some((
                    crate::sites::galleries::queue(http, &SITE, &PROGRAMME, children),
                    next,
                )))
            }
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http as client, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.bbc.co.uk/programmes/p084qtzs/p085g9kg"),
            Some(Target::Gallery("/programmes/p084qtzs/p085g9kg".into()))
        );
        assert_eq!(
            t("https://www.bbc.co.uk/programmes/b006q2x0/galleries"),
            Some(Target::Programme(
                "/programmes/b006q2x0/galleries".into(),
                1
            ))
        );
        assert_eq!(
            t("https://www.bbc.co.uk/programmes/b006q2x0/galleries?page=40"),
            Some(Target::Programme(
                "/programmes/b006q2x0/galleries".into(),
                40
            ))
        );
    }

    #[tokio::test]
    async fn extracts_gallery_with_fallbacks() {
        let page = r#"<script type="application/ld+json">{"itemListElement": [{"name": "Show"}, {"name": "Show"}, {"name": "Gallery"}]}</script>
            <h1><span>Show</span>Gallery &amp; More</h1><div class="gallery__thumbnail" data-image-src="https://ichef.bbci.co.uk/images/ic/320x180_b/p1.jpg" data-gallery-title="One">"#;
        let (root, server) = serve(vec![html(page)]);
        let http = client("bbc", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.bbc.co.uk/programmes/p084qtzs/p085g9kg").unwrap(),
        ))
        .await;
        assert_eq!(
            files[0].url.as_str(),
            "https://ichef.bbci.co.uk/images/ic/1920xn/p1.jpg"
        );
        assert_eq!(
            files[0].handoff["_fallback"],
            serde_json::json!([
                "https://ichef.bbci.co.uk/images/ic/1600xn/p1.jpg",
                "https://ichef.bbci.co.uk/images/ic/1280xn/p1.jpg",
                "https://ichef.bbci.co.uk/images/ic/976xn/p1.jpg"
            ])
        );
        assert_eq!(keywords(&files[0])["title"], "Gallery & More");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "01.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("bbc/Show/Gallery")
        );
        server.join().unwrap();
    }
}
