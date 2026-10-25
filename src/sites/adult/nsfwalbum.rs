//! NSFWalbum albums, mirroring gallery_dl/extractor/nsfwalbum.py: each
//! image of an album page is resolved through its iframe page's
//! `giraffe.annihilate` token and the `backend.php` endpoint, one image at
//! a time as the consumer reaches it.
//!
//! An image that still fails after `retries` (default 4) further attempts
//! is skipped, as upstream warns and continues. When the backend names no
//! absolute file URL (it now answers with a relative placeholder path),
//! the file is fetched from upstream's `imageProxy.php` fallback instead;
//! upstream's placeholder-image check is not ported.
use crate::sites::art::{Scan, extract_iter, nameext_into, on_host, split_html};
use crate::sites::manga::{keywords, text};
use crate::sites::{Events, Http, Item, Route, Site, remove_html, unescape};
use crate::{Result, error};
use serde_json::{Map, Value};
use std::collections::VecDeque;
use url::Url;

const ROOT: &str = "https://nsfwalbum.com";
static ALBUM: Route = Route::new(
    "album",
    "{album_id}_{num:>03}_{id}.{extension}",
    &["{category}", "{album_id} {title}"],
    "{id}",
);

pub static SITE: Site = Site {
    category: "nsfwalbum",
    name: "NSFWalbum",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&ALBUM],
    api_key: false,
    cookies: None,
};

/// `(?:www\.)?nsfwalbum\.com(/album/(\d+))`, unanchored.
fn target(url: &Url) -> Option<String> {
    if !on_host(url, &["nsfwalbum.com"]) {
        return None;
    }
    let id = url
        .path()
        .strip_prefix("/album/")
        .and_then(crate::sites::art::leading_digits)?;
    Some(id.to_owned())
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(id) = target(&url) else {
        return crate::sites::fail(error("unsupported NSFWalbum URL"));
    };
    crate::sites::once(async move {
        let page = super::page(http, &format!("{ROOT}/album/{id}"), &[]).await?;
        let data = metadata(&page, &id);
        let images: VecDeque<String> = extract_iter(&page, "data-img-id=\"", "\"")
            .into_iter()
            .map(str::to_owned)
            .collect();
        let retries = http
            .option("retries")
            .and_then(Value::as_i64)
            .unwrap_or(4)
            .clamp(0, 10);
        Ok(crate::sites::paginate(
            Some((images, 0usize)),
            move |state| {
                let data = data.clone();
                async move {
                    let Some((mut images, num)) = state else {
                        return Ok(None);
                    };
                    let Some(image_id) = images.pop_front() else {
                        return Ok(None);
                    };
                    let (events, num) = match resolve(http, &image_id, retries).await {
                        Some((info, spirit)) => {
                            let item = item(&data, &image_id, &spirit, num + 1, &info);
                            let items = item.into_iter().collect();
                            (crate::sites::post(&SITE, http, &ALBUM, items), num + 1)
                        }
                        None => (Vec::new(), num),
                    };
                    Ok(Some((crate::sites::events(events), Some((images, num)))))
                }
            },
        ))
    })
}

/// `NsfwalbumAlbumExtractor.metadata`.
fn metadata(page: &str, id: &str) -> Map<String, Value> {
    let mut e = Scan::new(page);
    keywords([
        ("album_id", text::parse_int(id).into()),
        ("title", unescape(e.extr("<h6>", "</h6>")).into()),
        (
            "models",
            split_html(e.extr("\"models\"> Models:", "</div>")).into(),
        ),
        (
            "studio",
            remove_html(e.extr("\"models\"> Studio:", "</div>")).into(),
        ),
    ])
}

/// The `backend.php` answer for one image and its token, retrying like
/// upstream; the token is fetched again only while it is missing.
async fn resolve(http: &Http, image_id: &str, retries: i64) -> Option<(Value, String)> {
    let mut spirit: Option<String> = None;
    for _ in 0..=retries {
        let attempt: Result<(Value, String)> = async {
            let token = match &spirit {
                Some(token) => token.clone(),
                None => {
                    let url = http.api(&format!("{ROOT}/iframe_image.php"))?;
                    let mut url = url;
                    url.query_pairs_mut().append_pair("id", image_id);
                    let page = http.text_with(&url, &http.browser_headers(&[])).await?;
                    let raw = crate::sites::extr(&page, "giraffe.annihilate(\"", "\"")
                        .ok_or_else(|| error("NSFWalbum image has no token"))?;
                    let token = annihilate(raw);
                    spirit = Some(token.clone());
                    token
                }
            };
            let mut url = http.api(&format!("{ROOT}/backend.php"))?;
            url.query_pairs_mut()
                .append_pair("spirit", &token)
                .append_pair("photo", image_id);
            let info = http.json_with(&url, &http.browser_headers(&[])).await?;
            Ok((info, token))
        }
        .await;
        if let Ok(found) = attempt {
            return Some(found);
        }
    }
    None
}

/// `_annihilate`: XOR each character with 6.
fn annihilate(value: &str) -> String {
    value
        .chars()
        .map(|c| char::from_u32(c as u32 ^ 6).unwrap_or(c))
        .collect()
}

fn item(
    data: &Map<String, Value>,
    image_id: &str,
    spirit: &str,
    num: usize,
    info: &Value,
) -> Option<Item> {
    let raw = info[0].as_str()?;
    let url = match raw.starts_with("https://") || raw.starts_with("http://") {
        true => crate::sites::art::file_url(raw, ROOT)?,
        false => {
            let mut url = Url::parse(&format!("{ROOT}/imageProxy.php")).ok()?;
            url.query_pairs_mut()
                .append_pair("photoId", image_id)
                .append_pair("spirit", spirit);
            url
        }
    };
    let mut fields = data.clone();
    fields.insert("num".into(), num.into());
    fields.insert("id".into(), text::parse_int(image_id).into());
    fields.insert("width".into(), text::parse_int_value(info.get(1)).into());
    fields.insert("height".into(), text::parse_int_value(info.get(2)).into());
    nameext_into(raw, &mut fields);
    let album_id = crate::sites::scalar(&fields["album_id"]);
    Some(Item::new(url, fields).ids(album_id, image_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::manga::tests::{archive, path};
    use crate::sites::testing::{html, http, json, keywords as kw, media, response, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://nsfwalbum.com/album/401611"),
            Some("401611".into())
        );
        assert_eq!(t("http://www.nsfwalbum.com/album/7x"), Some("7".into()));
        for raw in [
            "https://nsfwalbum.com/album/",
            "https://nsfwalbum.com/photo/1",
            "https://m.nsfwalbum.com/album/1",
            "https://nsfwalbum.com.evil.test/album/1",
            "https://nsfwalbum.com:8443/album/1",
            "https://u:p@nsfwalbum.com/album/1",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
        assert_eq!(annihilate(&annihilate("tok3n")), "tok3n");
    }

    #[tokio::test]
    async fn resolves_each_image() {
        let page = "<h6>Field &amp; Flowers</h6><div class=\"models\"> Models: <a>Model A</a> <a>Model B</a></div>\
            <div class=\"models\"> Studio: <a>Studio X</a></div>\
            <img data-img-id=\"11\"><img data-img-id=\"12\">";
        let iframe = |token: &str| {
            html(&format!(
                "<script>giraffe.annihilate(\"{}\", 6)</script>",
                annihilate(token)
            ))
        };
        let (root, server) = serve(vec![
            html(page),
            iframe("abc"),
            json(json!(["https://img.test/a/11.jpg", "800", "600"])),
            iframe("def"),
            response("500 Internal Server Error", "text/plain", b""),
            json(json!(["/show_img.jpg", 640, 480])),
        ]);
        let http = http("nsfwalbum", root);
        let files = media(extract(
            &http,
            Url::parse("https://nsfwalbum.com/album/401611").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = kw(&files[1]);
        assert_eq!(k["album_id"], 401611);
        assert_eq!(k["title"], "Field & Flowers");
        assert_eq!(k["models"], json!(["Model A", "Model B"]));
        assert_eq!(k["studio"], "Studio X");
        assert_eq!(k["id"], 12);
        assert_eq!(k["num"], 2);
        assert_eq!(k["width"], 640);
        assert_eq!(
            files[1].url.as_str(),
            "https://nsfwalbum.com/imageProxy.php?photoId=12&spirit=def"
        );
        assert_eq!(kw(&files[0])["height"], 600);
        assert_eq!(
            path(&files[1]),
            "nsfwalbum/401611 Field & Flowers/401611_002_12.jpg"
        );
        assert_eq!(archive(&files[1]), r#"["{id}",["12"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /album/401611 "));
        assert!(requests[1].starts_with("GET /iframe_image.php?id=11 "));
        assert!(requests[2].starts_with("GET /backend.php?spirit=abc&photo=11 "));
        assert!(requests[4].starts_with("GET /backend.php?spirit=def&photo=12 "));
        assert!(requests[5].starts_with("GET /backend.php?spirit=def&photo=12 "));
    }
}
