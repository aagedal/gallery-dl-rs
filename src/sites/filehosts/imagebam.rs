//! ImageBam galleries and images, mirroring gallery_dl/extractor/imagebam.py.
use crate::error;
use crate::sites::gdl::{Scan, captures, extr, group, nameext_from_name, re, rextr, unescape};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://imagebam.com";

static GALLERY: Route = Route::new(
    "gallery",
    "{num:>03} {filename}.{extension}",
    &["{category}", "{title} {gallery_key}"],
    "{gallery_key}_{image_key}",
);
static IMAGE: Route = Route::new(
    "image",
    "{filename}.{extension}",
    &["{category}"],
    "{image_key}",
);

pub static SITE: Site = Site {
    category: "imagebam",
    name: "ImageBam",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&GALLERY, &IMAGE],
    api_key: false,
    cookies: Some("imagebam.com"),
};

#[derive(Debug, PartialEq)]
enum Target {
    Gallery(String),
    Image(String),
}

fn target(url: &Url) -> Option<Target> {
    let gallery = re!(r"^https?://(?:www\.)?imagebam\.com(/(?:gallery/|view/G)[a-zA-Z0-9]+)");
    if let Some(c) = captures(gallery, url) {
        return Some(Target::Gallery(group(&c, 1).into()));
    }
    let image = re!(
        r"^https?://(?:\w+\.)?imagebam\.com(/(?:image/|view/M|(?:[0-9a-f]{2}/){3})[a-zA-Z0-9]+)"
    );
    let path = group(&captures(image, url)?, 1).to_owned();
    // Direct file URLs map to their image page.
    Some(Target::Image(if path.as_bytes().get(3) == Some(&b'/') {
        let key = &path[10..];
        format!(
            "{}{key}",
            if key.starts_with('M') {
                "/view/"
            } else {
                "/image/"
            }
        )
    } else {
        path
    }))
}

/// `_parse_image_page`.
async fn image_page(http: &Http, path: &str) -> crate::Result<(Url, Map<String, Value>)> {
    let page = http.text(&http.api(&format!("{ROOT}{path}"))?).await?;
    let mut scan = Scan::new(&page);
    let url = scan
        .find("<img src=\"https://images", "\"")
        .filter(|u| !u.is_empty())
        .ok_or_else(|| error("ImageBam image not found"))?;
    let url = format!("https://images{url}");
    let (name, ext) = nameext_from_name(&unescape(scan.extr("alt=\"", "\"")));
    let mut data = Map::new();
    data.insert("filename".into(), name.into());
    data.insert("extension".into(), ext.into());
    data.insert("url".into(), url.as_str().into());
    data.insert(
        "image_key".into(),
        path.rsplit('/').next().unwrap_or_default().into(),
    );
    Ok((
        Url::parse(&url).map_err(|_| error("invalid ImageBam image URL"))?,
        data,
    ))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported ImageBam URL"));
    };
    crate::sites::once(async move {
        let root = http.api(ROOT)?;
        for name in ["nsfw_inter", "sfw_inter"] {
            http.set_cookie(&root, &format!("{name}=1; Domain=imagebam.com; Path=/"));
        }
        Ok(match target {
            Target::Image(path) => {
                let (file, data) = image_page(http, &path).await?;
                let key = crate::sites::scalar(&data["image_key"]);
                crate::sites::events(crate::sites::post(
                    &SITE,
                    http,
                    &IMAGE,
                    vec![Item::new(file, data).ids(key.clone(), key)],
                ))
            }
            Target::Gallery(path) => {
                let mut page = http.text(&http.api(&format!("{ROOT}{path}"))?).await?;
                let title = unescape(extr(&page, "id=\"gallery-name\">", "<").trim());
                let pattern =
                    // The site moved from `www.imagebam.com` to `imagebam.com`.
                    re!(r#"<a href="https://(?:www\.)?imagebam\.com(/(?:image/|view/M)[a-zA-Z0-9]+)"#);
                let mut paths: Vec<String> = Vec::new();
                loop {
                    paths.extend(
                        pattern
                            .captures_iter(&page)
                            .map(|c| group(&c, 1).to_owned()),
                    );
                    let next = page
                        .find("rel=\"next\" aria-label=\"Next")
                        .filter(|pos| *pos > 0)
                        .map(|pos| rextr(&page, "href=\"", "\"", Some(pos)).to_owned())
                        .filter(|url| !url.is_empty());
                    match next {
                        Some(url) => page = http.text(&http.api(&url)?).await?,
                        None => break,
                    }
                }
                paths.reverse();
                let key = path.rsplit('/').next().unwrap_or_default().to_owned();
                let count = paths.len();
                let mut items = Vec::new();
                for (index, image) in paths.iter().enumerate() {
                    let (file, mut data) = image_page(http, image).await?;
                    data.insert("title".into(), title.as_str().into());
                    data.insert("count".into(), count.into());
                    data.insert("gallery_key".into(), key.as_str().into());
                    data.insert("num".into(), (index + 1).into());
                    items.push(Item::new(file, data).ids(key.clone(), (index + 1).to_string()));
                }
                crate::sites::events(crate::sites::post(&SITE, http, &GALLERY, items))
            }
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http as client, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.imagebam.com/view/GA3MT1"),
            Some(Target::Gallery("/view/GA3MT1".into()))
        );
        assert_eq!(
            t("http://www.imagebam.com/gallery/op9dwck"),
            Some(Target::Gallery("/gallery/op9dwck".into()))
        );
        assert_eq!(
            t("https://www.imagebam.com/view/ME8JOQP"),
            Some(Target::Image("/view/ME8JOQP".into()))
        );
        assert_eq!(
            t("http://images3.imagebam.com/1d/8c/44/94d56c502511890.png"),
            Some(Target::Image("/image/94d56c502511890".into()))
        );
        assert_eq!(t("https://www.imagebam.com/user/x"), None);
    }

    #[tokio::test]
    async fn extracts_gallery_oldest_first() {
        let gallery = r#"<div id="gallery-name"> Holiday &amp; More </div>
            <a href="https://www.imagebam.com/view/MB"><a href="https://www.imagebam.com/view/MA">"#;
        let image = |n: &str| {
            format!(r#"<img src="https://images4.imagebam.com/x/{n}.jpg" alt="{n}.jpg">"#)
        };
        let (root, server) = serve(vec![html(gallery), html(&image("a")), html(&image("b"))]);
        let http = client("imagebam", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.imagebam.com/view/GA3MT1").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].url.as_str(),
            "https://images4.imagebam.com/x/a.jpg"
        );
        let k = keywords(&files[1]);
        assert_eq!(k["image_key"], "MB");
        assert_eq!(k["title"], "Holiday & More");
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "002 b.jpg");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("imagebam/Holiday & More GA3MT1")
        );
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /view/MA "));
    }
}
