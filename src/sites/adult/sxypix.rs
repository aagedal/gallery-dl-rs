//! SxyPix galleries, mirroring gallery_dl/extractor/sxypix.py: the gallery
//! page names its album IDs, and `php/gall.php` returns the image list.
use crate::sites::art::{extr, on_host};
use crate::sites::http::Body;
use crate::sites::manga::{Gallery, keywords};
use crate::sites::{Events, Http, Route, Site, unescape};
use crate::{Result, error};
use serde_json::Value;
use url::Url;

const ROOT: &str = "https://sxypix.com";
static GALLERY: Route = Route::new(
    "gallery",
    "{category}_{gallery_id}_{num:>03}.{extension}",
    &["{category}", "{gallery_id} {title}"],
    "{gallery_id}_{num}",
);

pub static SITE: Site = Site {
    category: "sxypix",
    name: "SxyPix",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&GALLERY],
    api_key: false,
    cookies: None,
};

/// `(?:www\.)?sxypix\.com(/w/(\w+))`, unanchored.
fn target(url: &Url) -> Option<String> {
    if !on_host(url, &["sxypix.com"]) {
        return None;
    }
    let rest = url.path().strip_prefix("/w/")?;
    let len = rest
        .bytes()
        .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_')
        .count();
    (len > 0).then(|| rest[..len].to_owned())
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(id) = target(&url) else {
        return crate::sites::fail(error("unsupported SxyPix URL"));
    };
    crate::sites::once(gallery(http, id))
}

async fn gallery(http: &Http, id: String) -> Result<Events<'_>> {
    let page = super::page(http, &format!("{ROOT}/w/{id}"), &[]).await?;
    let data = keywords([
        ("gallery_id", id.clone().into()),
        (
            "title",
            unescape(extr(&page, "<meta name=\"keywords\" content=\"", "\"")).into(),
        ),
    ]);
    let form = [
        ("aid", extr(&page, "data-aid='", "'")),
        ("ghash", extr(&page, "data-ghash='", "'")),
    ];
    let body = http
        .post(
            &http.api(&format!("{ROOT}/php/gall.php"))?,
            Body::Form(&form),
            &http.browser_headers(&[]),
        )
        .await?;
    let gallery: Value =
        serde_json::from_slice(&body).map_err(|_| error("SxyPix returned invalid JSON"))?;
    let pages = gallery["r"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|entry| {
            (
                format!("https://x.{}", extr(entry, "data-src='//.", "'")),
                None,
            )
        })
        .collect();
    Ok(crate::sites::events(
        Gallery {
            enum_key: "num",
            ..Gallery::chapter(&GALLERY, data, pages).id(id)
        }
        .events(&SITE, http),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::manga::tests::{archive, path};
    use crate::sites::testing::{html, http, json, keywords as kw, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://sxypix.com/w/2bbaf1b24a5863d0e73436619bbaa7ee"),
            Some("2bbaf1b24a5863d0e73436619bbaa7ee".into())
        );
        assert_eq!(t("http://www.sxypix.com/w/ab_c-d"), Some("ab_c".into()));
        for raw in [
            "https://sxypix.com/w/",
            "https://sxypix.com/x/abc",
            "https://m.sxypix.com/w/abc",
            "https://sxypix.com.evil.test/w/abc",
            "https://sxypix.com:8443/w/abc",
            "https://u:p@sxypix.com/w/abc",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[tokio::test]
    async fn extracts_gallery() {
        let page = "<meta name=\"keywords\" content=\"Night &amp; City\">\
            <div data-aid='77' data-ghash='gh1'></div>";
        let list = json!({"r": [
            "<div data-src='//.sxypix.com/i/1.jpg'></div>",
            "<div data-src='//.sxypix.com/i/2.jpg'></div>"]});
        let (root, server) = serve(vec![html(page), json(list)]);
        let http = http("sxypix", root);
        let files = media(extract(
            &http,
            Url::parse("https://sxypix.com/w/abc123").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(files[1].url.as_str(), "https://x.sxypix.com/i/2.jpg");
        let k = kw(&files[1]);
        assert_eq!(k["gallery_id"], "abc123");
        assert_eq!(k["title"], "Night & City");
        assert_eq!(k["num"], 2);
        assert_eq!(
            path(&files[1]),
            "sxypix/abc123 Night & City/sxypix_abc123_002.jpg"
        );
        assert_eq!(
            archive(&files[1]),
            r#"["{gallery_id}_{num}",["abc123","2"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /w/abc123 "));
        assert!(requests[1].starts_with("POST /php/gall.php "));
        assert!(requests[1].ends_with("aid=77&ghash=gh1"));
    }
}
