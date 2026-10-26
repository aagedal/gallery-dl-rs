//! MyHentaiGallery galleries and tag listings, mirroring
//! gallery_dl/extractor/myhentaigallery.py.
use crate::error;
use crate::sites::gdl::{
    Scan, captures, extr, extract_iter, group, parse_int, re, rextr, split_html, unescape,
};
use crate::sites::manga::{GALLERY_ARCHIVE, GALLERY_FILENAME};
use crate::sites::{Events, Http, Route, Site};
use serde_json::Map;
use url::Url;

const ROOT: &str = "https://myhentaigallery.com";
const BASE: &str = r"^https?://myhentaigallery\.com";

static GALLERY: Route = Route::new(
    "gallery",
    GALLERY_FILENAME,
    &["{category}", "{gallery_id} {artist:?[/] /J, }{title}"],
    GALLERY_ARCHIVE,
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
    Tag(String),
}

fn target(url: &Url) -> Option<Target> {
    if let Some(c) = captures(
        re!(&format!(r"{BASE}/g(?:allery/(?:thumbnails|show))?/(\d+)")),
        url,
    ) {
        return Some(Target::Gallery(group(&c, 1).into()));
    }
    captures(
        re!(&format!(
            r"{BASE}(/g/(artist|category|group|parody)/(\d+).*)"
        )),
        url,
    )
    .map(|c| Target::Tag(group(&c, 1).into()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported MyHentaiGallery URL"));
    };
    match target {
        Target::Gallery(id) => crate::sites::once(async move {
            let page_url = format!("{ROOT}/g/{id}");
            let page = http
                .text_with(&http.api(&page_url)?, &[("Referer", &page_url)])
                .await?;
            let mut scan = Scan::new(&page);
            let title = scan
                .extr("<div class=\"comic-description\">\n", "</h1>")
                .trim_start();
            let title = title.strip_prefix("<h1>").unwrap_or(title);
            if title.is_empty() {
                return Err(error("MyHentaiGallery gallery not found"));
            }
            let mut data = Map::new();
            data.insert("title".into(), unescape(title).into());
            data.insert("gallery_id".into(), parse_int(&id).into());
            for (key, label) in [
                ("tags", "        Categories:"),
                ("artist", "        Artists:"),
                ("group", "        Groups:"),
                ("parodies", "        Parodies:"),
            ] {
                data.insert(key.into(), split_html(scan.extr(label, "</div>")).into());
            }
            let images = extract_iter(&page, "class=\"comic-thumb\"", "</div>")
                .into_iter()
                .map(|thumb| {
                    (
                        unescape(extr(thumb, "src=\"", "\"")).replace("/thumbnail/", "/original/"),
                        None,
                    )
                })
                .collect();
            let events = super::numbered(&SITE, http, &GALLERY, &data, images, &id)
                .into_iter()
                .map(|event| with_referer(event, &page_url))
                .collect();
            Ok(crate::sites::events(events))
        }),
        Target::Tag(path) => {
            crate::sites::paginate(Some(format!("{ROOT}{path}")), move |state| async move {
                let Some(url) = state else { return Ok(None) };
                let page = http.text(&http.api(&url)?).await?;
                let children = extract_iter(&page, "<div class=\"comic-inner\">", "<div")
                    .into_iter()
                    .filter_map(|inner| {
                        Some((
                            Url::parse(&format!("{ROOT}{}", extr(inner, "href=\"", "\""))).ok()?,
                            Map::new(),
                        ))
                    })
                    .collect();
                let next = page
                    .find(">Next<")
                    .map(|pos| format!("{ROOT}{}", rextr(&page, "href=\"", "\"", Some(pos))));
                Ok(Some((super::queue(http, &SITE, &TAG, children), next)))
            })
        }
    }
}

/// Upstream sets the gallery page as the session `Referer`.
fn with_referer(
    event: crate::event::ExtractionEvent,
    referer: &str,
) -> crate::event::ExtractionEvent {
    match event {
        crate::event::ExtractionEvent::Media(mut media) => {
            media.headers.insert("Referer".into(), referer.into());
            crate::event::ExtractionEvent::Media(media)
        }
        event => event,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{children, html, http as client, keywords, media, serve};

    #[tokio::test]
    async fn extracts_gallery() {
        let page = "<div class=\"comic-description\">\n  <h1>Title &amp; More</h1>\n        Categories: <a>Tag</a></div>\n        Artists: <a>Artist</a></div>
            <div class=\"comic-thumb\"><img src=\"https://images.myhentaicomics.com/m/thumbnail/1.jpg\"></div>";
        let (root, server) = serve(vec![html(page)]);
        let http = client("myhentaigallery", root);
        let files = media(extract(
            &http,
            Url::parse("https://myhentaigallery.com/g/16247").unwrap(),
        ))
        .await;
        assert_eq!(
            files[0].url.as_str(),
            "https://images.myhentaicomics.com/m/original/1.jpg"
        );
        assert_eq!(
            files[0].headers["Referer"],
            "https://myhentaigallery.com/g/16247"
        );
        assert_eq!(keywords(&files[0])["artist"], serde_json::json!(["Artist"]));
        let (_, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("myhentaigallery/16247 [Artist] Title & More")
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn tag_follows_next() {
        let first = r#"<div class="comic-inner"><a href="/g/1"><div><a href="/g/category/2?page=2">Next</a>"#;
        let second = r#"<div class="comic-inner"><a href="/g/2"><div>"#;
        let (root, server) = serve(vec![html(first), html(second)]);
        let http = client("myhentaigallery", root);
        assert_eq!(
            children(extract(
                &http,
                Url::parse("https://myhentaigallery.com/g/category/2").unwrap()
            ))
            .await
            .len(),
            2
        );
        server.join().unwrap();
    }
}
