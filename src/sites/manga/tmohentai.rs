//! TMOHentai galleries, mirroring gallery_dl/extractor/tmohentai.py.
use super::{Gallery, set};
use crate::error;
use crate::sites::gdl::{Scan, captures, group, re, remove_html, split_html, unescape};
use crate::sites::{Events, Http, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "http://tmohentai.com";

static GALLERY: Route = Route::new(
    "gallery",
    super::GALLERY_FILENAME,
    &["{category}", "{title} ({gallery_id})"],
    super::GALLERY_ARCHIVE,
);

pub static SITE: Site = Site {
    category: "tmohentai",
    name: "TMOHentai",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&GALLERY],
    api_key: false,
    cookies: None,
};

fn target(url: &Url) -> Option<String> {
    let pattern = re!(r"^https?://tmohentai\.com/(?:contents|reader)/(\w+)");
    captures(pattern, url).map(|caps| group(&caps, 1).to_owned())
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(id) = target(&url) else {
        return crate::sites::fail(error("unsupported TMOHentai URL"));
    };
    crate::sites::once(async move {
        let page = super::fetch(http, &format!("{ROOT}/contents/{id}")).await?;
        let base = format!("https://imgrojo.tmohentai.com/contents/{id}/");
        let pages = (0..page.matches("class=\"lanzador").count())
            .map(|i| (format!("{base}{i:03}.webp"), None))
            .collect();
        Ok(crate::sites::events(
            Gallery::numbered(&GALLERY, metadata(&page, &id), pages)
                .id(id)
                .events(&SITE, http),
        ))
    })
}

fn metadata(page: &str, id: &str) -> Map<String, Value> {
    let mut scan = Scan::new(page);
    let mut data = Map::new();
    set(&mut data, "gallery_id", id);
    set(&mut data, "title", unescape(scan.extr("<h3>", "<").trim()));
    set(
        &mut data,
        "artists",
        split_html(scan.extr("<label>Artists and Artists Groups</label>", "</ul>")),
    );
    set(
        &mut data,
        "genres",
        split_html(scan.extr("<label>Genders</label>", "</ul>")),
    );
    set(
        &mut data,
        "tags",
        split_html(scan.extr("<label>Tags</label>", "</ul>")),
    );
    set(
        &mut data,
        "uploader",
        remove_html(scan.extr("<label>Uploaded By</label>", "</ul>")),
    );
    set(&mut data, "language", scan.extr("&nbsp;", "\n"));
    data
}

#[cfg(test)]
mod tests {
    use super::super::tests::{archive, path};
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://tmohentai.com/contents/653c2aeaa693c").as_deref(),
            Some("653c2aeaa693c")
        );
        assert_eq!(
            t("https://tmohentai.com/reader/653c2aeaa693c/paginated/1").as_deref(),
            Some("653c2aeaa693c")
        );
        assert_eq!(t("https://www.tmohentai.com/contents/653c2aeaa693c"), None);
    }

    #[tokio::test]
    async fn extracts_gallery() {
        let page = "<h3> Title &amp; Co </h3><label>Artists and Artists Groups</label><ul><li><a>Art</a></li></ul>\
            <label>Genders</label><ul><li>Romance</li></ul><label>Tags</label><ul><li>a</li><li>b</li></ul>\
            <label>Uploaded By</label><ul><li><a>Up</a></li></ul><span>&nbsp;Español\n</span>\
            <div class=\"lanzador\"></div><div class=\"lanzador x\"></div>";
        let (root, server) = serve(vec![html(page)]);
        let http = http("tmohentai", root);
        let files = media(extract(
            &http,
            Url::parse("https://tmohentai.com/contents/653c2aeaa693c").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[1].url.as_str(),
            "https://imgrojo.tmohentai.com/contents/653c2aeaa693c/001.webp"
        );
        let k = keywords(&files[1]);
        assert_eq!(k["title"], "Title & Co");
        assert_eq!(k["tags"], serde_json::json!(["a", "b"]));
        assert_eq!(k["uploader"], "Up");
        assert_eq!(k["language"], "Español");
        assert_eq!(k["num"], 2);
        assert_eq!(
            path(&files[1]),
            "tmohentai/Title & Co (653c2aeaa693c)/tmohentai_653c2aeaa693c_002.webp"
        );
        assert_eq!(
            archive(&files[0]),
            r#"["{gallery_id}_{num}",["653c2aeaa693c","1"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /contents/653c2aeaa693c "));
    }
}
