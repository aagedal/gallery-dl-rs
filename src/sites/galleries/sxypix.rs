//! sxypix.com galleries, mirroring gallery_dl/extractor/sxypix.py.
use crate::error;
use crate::sites::gdl::{captures, extr, group, re, unescape};
use crate::sites::manga::{GALLERY_ARCHIVE, GALLERY_FILENAME};
use crate::sites::{Events, Http, Route, Site};
use serde_json::Map;
use url::Url;

static GALLERY: Route = Route::new(
    "gallery",
    GALLERY_FILENAME,
    &["{category}", "{gallery_id} {title}"],
    GALLERY_ARCHIVE,
);

pub static SITE: Site = Site {
    category: "sxypix",
    name: "sxypix",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&GALLERY],
    api_key: false,
    cookies: None,
};

fn target(url: &Url) -> Option<(String, String)> {
    captures(re!(r"^https?://(?:www\.)?sxypix\.com(/w/(\w+))"), url)
        .map(|c| (group(&c, 1).into(), group(&c, 2).into()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some((path, id)) = target(&url) else {
        return crate::sites::fail(error("unsupported sxypix URL"));
    };
    crate::sites::once(async move {
        let page = http
            .text(&http.api(&format!("https://sxypix.com{path}"))?)
            .await?;
        let mut data = Map::new();
        data.insert("gallery_id".into(), id.as_str().into());
        data.insert(
            "title".into(),
            unescape(extr(&page, "<meta name=\"keywords\" content=\"", "\"")).into(),
        );
        let form = [
            ("aid", extr(&page, "data-aid='", "'")),
            ("ghash", extr(&page, "data-ghash='", "'")),
        ];
        let gallery = http
            .post_form(&http.api("https://sxypix.com/php/gall.php")?, &form, &[])
            .await?;
        let images = gallery["r"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|entry| {
                (
                    format!(
                        "https://x.{}",
                        extr(entry.as_str().unwrap_or_default(), "data-src='//.", "'")
                    ),
                    None,
                )
            })
            .collect();
        Ok(crate::sites::events(super::numbered(
            &SITE, http, &GALLERY, &data, images, &id,
        )))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http as client, json, media, serve};
    use serde_json::json;

    #[tokio::test]
    async fn posts_for_gallery_listing() {
        let page = r#"<meta name="keywords" content="Title"><div data-aid='1' data-ghash='abc'>"#;
        let listing = json!({"r": ["<img data-src='//.sxypix.com/a/1.jpg'>", "<img data-src='//.sxypix.com/a/2.jpg'>"]});
        let (root, server) = serve(vec![html(page), json(listing)]);
        let http = client("sxypix", root);
        let files = media(extract(
            &http,
            Url::parse("https://sxypix.com/w/2bbaf1b24a5863d0e73436619bbaa7ee").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].url.as_str(), "https://x.sxypix.com/a/1.jpg");
        let requests = server.join().unwrap();
        assert!(
            requests[1].starts_with("POST /php/gall.php ")
                && requests[1].ends_with("aid=1&ghash=abc")
        );
    }
}
