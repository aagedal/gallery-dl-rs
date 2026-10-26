//! NSFWalbum albums, mirroring gallery_dl/extractor/nsfwalbum.py.
use crate::error;
use crate::sites::gdl::{
    Scan, captures, extr, extract_iter, group, parse_int, re, remove_html, split_html, unescape,
};
use crate::sites::{Events, Http, Route, Site};
use serde_json::{Map, Value};
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

fn target(url: &Url) -> Option<(String, String)> {
    captures(
        re!(r"^https?://(?:www\.)?nsfwalbum\.com(/album/(\d+))"),
        url,
    )
    .map(|c| (group(&c, 1).into(), group(&c, 2).into()))
}

/// `_annihilate`: XOR each character with 6.
fn annihilate(value: &str) -> String {
    value
        .chars()
        .map(|c| char::from_u32(u32::from(c) ^ 6).unwrap_or(c))
        .collect()
}

/// One image's backend data: URL, width, height and the proxy fallback.
async fn image(http: &Http, image_id: &str) -> crate::Result<(String, Map<String, Value>)> {
    let frame = http
        .text(&http.api(&format!("{ROOT}/iframe_image.php?id={image_id}"))?)
        .await?;
    let spirit = annihilate(extr(&frame, "giraffe.annihilate(\"", "\""));
    let mut backend = http.api(&format!("{ROOT}/backend.php"))?;
    backend
        .query_pairs_mut()
        .append_pair("spirit", &spirit)
        .append_pair("photo", image_id);
    let data = http.json(&backend).await?;
    let mut file = Map::new();
    file.insert("id".into(), parse_int(image_id).into());
    file.insert(
        "width".into(),
        parse_int(&crate::sites::scalar(&data[1])).into(),
    );
    file.insert(
        "height".into(),
        parse_int(&crate::sites::scalar(&data[2])).into(),
    );
    let proxy = format!("{ROOT}/imageProxy.php?photoId={image_id}&spirit={spirit}");
    let url = crate::sites::scalar(&data[0]);
    // A relative URL fails upstream, which then downloads the proxy fallback.
    if !url.starts_with("http") {
        return Ok((proxy, file));
    }
    file.insert("_fallback".into(), vec![proxy].into());
    Ok((url, file))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some((path, album_id)) = target(&url) else {
        return crate::sites::fail(error("unsupported NSFWalbum URL"));
    };
    crate::sites::once(async move {
        let page = http.text(&http.api(&format!("{ROOT}{path}"))?).await?;
        let mut scan = Scan::new(&page);
        let mut data = Map::new();
        data.insert("album_id".into(), parse_int(&album_id).into());
        data.insert("title".into(), unescape(scan.extr("<h6>", "</h6>")).into());
        data.insert(
            "models".into(),
            split_html(scan.extr("\"models\"> Models:", "</div>")).into(),
        );
        data.insert(
            "studio".into(),
            remove_html(scan.extr("\"models\"> Studio:", "</div>")).into(),
        );
        let mut images = Vec::new();
        for image_id in extract_iter(&page, "data-img-id=\"", "\"") {
            // Upstream retries each image a few times before skipping it.
            let mut found = None;
            for _ in 0..3 {
                if let Ok(result) = image(http, image_id).await {
                    found = Some(result);
                    break;
                }
            }
            if let Some((url, file)) = found {
                images.push((url, Some(file)));
            }
        }
        Ok(crate::sites::events(super::numbered(
            &SITE, http, &ALBUM, &data, images, &album_id,
        )))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http as client, json, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn annihilates_spirit() {
        assert_eq!(annihilate(&annihilate("abc123")), "abc123");
        assert_eq!(annihilate("gdc"), "abe");
    }

    #[tokio::test]
    async fn resolves_images_through_backend() {
        let page =
            r#"<h6>Album</h6><div class="models"> Models: <a>Jane</a></div><img data-img-id="5">"#;
        let (root, server) = serve(vec![
            html(page),
            html(r#"giraffe.annihilate("gdc", 6)"#),
            json(json!(["https://img.nsfwalbum.com/5.jpg", "800", "600"])),
        ]);
        let http = client("nsfwalbum", root);
        let files = media(extract(
            &http,
            Url::parse("https://nsfwalbum.com/album/401611").unwrap(),
        ))
        .await;
        assert_eq!(files[0].url.as_str(), "https://img.nsfwalbum.com/5.jpg");
        assert_eq!(
            files[0].handoff["_fallback"][0],
            "https://nsfwalbum.com/imageProxy.php?photoId=5&spirit=abe"
        );
        assert_eq!(keywords(&files[0])["models"], json!(["Jane"]));
        let (file, _) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "401611_001_5.jpg");
        let requests = server.join().unwrap();
        assert!(requests[2].starts_with("GET /backend.php?spirit=abe&photo=5 "));
    }
}
