//! TMOHentai galleries, mirroring gallery_dl/extractor/tmohentai.py: the
//! images are numbered WebP files, one per `lanzador` link on the gallery
//! page.
use super::{Gallery, keywords};
use crate::error;
use crate::sites::art::{Scan, split_html};
use crate::sites::{Events, Http, Route, Site, remove_html, unescape};
use url::Url;

pub static SITE: Site = Site {
    category: "tmohentai",
    name: "TMOHentai",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&GALLERY],
    api_key: false,
    cookies: None,
};

/// Upstream's root is plain HTTP.
const ROOT: &str = "http://tmohentai.com";
const IMAGES: &str = "https://imgrojo.tmohentai.com/contents";
/// Images a gallery may claim.
const MAX_COUNT: usize = 10_000;

static GALLERY: Route = Route::new(
    "gallery",
    "{category}_{gallery_id}_{num:>03}.{extension}",
    &["{category}", "{title} ({gallery_id})"],
    "{gallery_id}_{num}",
);

/// `tmohentai\.com/(?:contents|reader)/(\w+)`, unanchored at the end.
fn target(url: &Url) -> Option<String> {
    if !crate::sites::plain(url) || url.host_str() != Some("tmohentai.com") {
        return None;
    }
    let raw: Vec<&str> = url.path().split('/').skip(1).collect();
    let ["contents" | "reader", id, ..] = raw.as_slice() else {
        return None;
    };
    let end = id
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(id.len());
    (end > 0).then(|| id[..end].to_owned())
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(id) = target(&url) else {
        return crate::sites::fail(error("unsupported TMOHentai URL"));
    };
    crate::sites::once(async move {
        let page = http
            .text_with(
                &http.api(&format!("{ROOT}/contents/{id}"))?,
                &http.browser_headers(&[]),
            )
            .await?;
        let mut scan = Scan::new(&page);
        let data = keywords([
            ("gallery_id", id.clone().into()),
            ("title", unescape(scan.extr("<h3>", "<").trim()).into()),
            (
                "artists",
                split_html(scan.extr("<label>Artists and Artists Groups</label>", "</ul>")).into(),
            ),
            (
                "genres",
                split_html(scan.extr("<label>Genders</label>", "</ul>")).into(),
            ),
            (
                "tags",
                split_html(scan.extr("<label>Tags</label>", "</ul>")).into(),
            ),
            (
                "uploader",
                remove_html(scan.extr("<label>Uploaded By</label>", "</ul>")).into(),
            ),
            ("language", scan.extr("&nbsp;", "\n").into()),
        ]);
        let count = page.matches("class=\"lanzador").count().min(MAX_COUNT);
        let pages = (0..count)
            .map(|i| (format!("{IMAGES}/{id}/{i:03}.webp"), None))
            .collect();
        let gallery = Gallery {
            enum_key: "num",
            ..Gallery::chapter(&GALLERY, data, pages).id(id)
        };
        Ok(crate::sites::events(gallery.events(&SITE, http)))
    })
}

#[cfg(test)]
mod tests {
    use super::super::tests::{archive, path};
    use super::*;
    use crate::sites::testing::{html, http, keywords as kw, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://tmohentai.com/contents/12345a67b89c0"),
            Some("12345a67b89c0".into())
        );
        assert_eq!(
            t("http://tmohentai.com/reader/abc_1/paginated/2"),
            Some("abc_1".into())
        );
        for raw in [
            "https://tmohentai.com/contents/",
            "https://tmohentai.com/section/all",
            "https://www.tmohentai.com/contents/abc",
            "https://tmohentai.com.evil.test/contents/abc",
            "https://tmohentai.com:8443/contents/abc",
            "https://u:p@tmohentai.com/contents/abc",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[tokio::test]
    async fn extracts_gallery() {
        let page = "<h3> Love &amp; Peace </h3>
<label>Artists and Artists Groups</label><ul><li><a>Artist One</a></li></ul>
<label>Genders</label><ul><li><a>Romance</a></li><li><a>Comedy</a></li></ul>
<label>Tags</label><ul><li><a>Tag</a></li></ul>
<label>Uploaded By</label><ul><li><a> Up <b>Loader</b></a></li></ul>
<span>Language:&nbsp;Español\n</span>
<a class=\"lanzador btn\"></a><a class=\"lanzador\"></a>";
        let (root, requests) = serve(vec![html(page)]);
        let http = http("TMOHentai", root);
        let files = media(extract(
            &http,
            Url::parse("https://tmohentai.com/reader/12345a67b89c0/cascade").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[1].url.as_str(),
            "https://imgrojo.tmohentai.com/contents/12345a67b89c0/001.webp"
        );
        let k = kw(&files[1]);
        assert_eq!(k["gallery_id"], "12345a67b89c0");
        assert_eq!(k["title"], "Love & Peace");
        assert_eq!(k["artists"], json!(["Artist One"]));
        assert_eq!(k["genres"], json!(["Romance", "Comedy"]));
        assert_eq!(k["tags"], json!(["Tag"]));
        assert_eq!(k["uploader"], "Up Loader");
        assert_eq!(k["language"], "Español");
        assert_eq!(k["num"], 2);
        assert_eq!(k["count"], 2);
        assert_eq!(
            path(&files[1]),
            "tmohentai/Love & Peace (12345a67b89c0)/tmohentai_12345a67b89c0_002.webp"
        );
        assert_eq!(
            archive(&files[1]),
            r#"["{gallery_id}_{num}",["12345a67b89c0","2"]]"#
        );
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /contents/12345a67b89c0 "));
    }
}
