//! ImageBam galleries and images, mirroring gallery_dl/extractor/imagebam.py:
//! gallery pages list image pages, each naming its full-size file.
use super::word_char;
use crate::sites::art::{Scan, extr, nameext_from_name, update};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use futures_util::StreamExt;
use serde_json::{Map, Value};
use url::Url;

/// Upstream's root is `www.imagebam.com`, which now redirects here; site
/// requests follow same-origin redirects only.
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
    /// The `/gallery/ID` or `/view/GID` path.
    Gallery(String),
    /// The image page path, with `/xx/xx/xx/ID` file paths rewritten.
    Image(String),
}

/// The leading `[a-zA-Z0-9]+` run of `text`.
fn alnum(text: &str) -> &str {
    let len = text.bytes().take_while(u8::is_ascii_alphanumeric).count();
    &text[..len]
}

/// `(?:www\.)?imagebam\.com(/(?:gallery/|view/G)ID)` and
/// `(?:\w+\.)?imagebam\.com(/(?:image/|view/M|(?:[0-9a-f]{2}/){3})ID)`,
/// unanchored after the ID.
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) {
        return None;
    }
    let label = match url.host_str()?.strip_suffix("imagebam.com")? {
        "" => None,
        sub => Some(sub.strip_suffix('.')?),
    };
    if label.is_some_and(|label| label.is_empty() || !label.chars().all(word_char)) {
        return None;
    }
    let path = url.path();
    if label.is_none_or(|label| label == "www") {
        for prefix in ["/gallery/", "/view/G"] {
            if let Some(id) = path.strip_prefix(prefix).map(alnum)
                && !id.is_empty()
            {
                return Some(Target::Gallery(format!("{prefix}{id}")));
            }
        }
    }
    for prefix in ["/image/", "/view/M"] {
        if let Some(id) = path.strip_prefix(prefix).map(alnum)
            && !id.is_empty()
        {
            return Some(Target::Image(format!("{prefix}{id}")));
        }
    }
    // `(?:[0-9a-f]{2}/){3}ID`: a file path names its page by the ID's
    // first letter.
    let dirs = path.get(1..10)?.as_bytes();
    let hex = |b: &u8| b.is_ascii_digit() || (b'a'..=b'f').contains(b);
    if !dirs
        .chunks(3)
        .all(|c| hex(&c[0]) && hex(&c[1]) && c[2] == b'/')
    {
        return None;
    }
    let id = alnum(&path[10..]);
    match id.chars().next()? {
        'M' => Some(Target::Image(format!("/view/{id}"))),
        _ => Some(Target::Image(format!("/image/{id}"))),
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported ImageBam URL"));
    };
    crate::sites::once(async move {
        // `_init`: skip the content warning interstitials.
        let root = http.api(ROOT)?;
        http.set_cookie(&root, "nsfw_inter=1; Path=/");
        http.set_cookie(&root, "sfw_inter=1; Path=/");
        match target {
            Target::Image(path) => {
                let item = image(http, &path, Map::new()).await?;
                Ok(crate::sites::events(crate::sites::post(
                    &SITE,
                    http,
                    &IMAGE,
                    vec![item],
                )))
            }
            Target::Gallery(path) => gallery(http, path).await,
        }
    })
}

/// `ImagebamGalleryExtractor.items`: every listing page first, then one
/// image page per file, requested as the consumer reaches it.
async fn gallery(http: &Http, path: String) -> Result<Events<'_>> {
    let mut page = http.text(&http.api(&format!("{ROOT}{path}"))?).await?;
    let mut data = Map::new();
    let title = crate::sites::unescape(extr(&page, "id=\"gallery-name\">", "<").trim());
    data.insert("title".into(), title.into());
    let mut paths = Vec::new();
    let mut seen = std::collections::HashSet::new();
    loop {
        paths.extend(image_paths(&page));
        match next_page(&page) {
            Some(next) if seen.insert(next.clone()) => {
                page = http.text(&http.api(next.as_str())?).await?;
            }
            _ => break,
        }
    }
    paths.reverse();
    data.insert("count".into(), paths.len().into());
    let key = path.rsplit('/').next().unwrap_or_default();
    data.insert("gallery_key".into(), key.into());
    Ok(crate::sites::pages(
        futures_util::stream::iter(paths.into_iter().enumerate()).then(move |(index, path)| {
            let mut data = data.clone();
            data.insert("num".into(), (index + 1).into());
            async move {
                let item = image(http, &path, data).await?;
                Ok(crate::sites::events(crate::sites::post(
                    &SITE,
                    http,
                    &GALLERY,
                    vec![item],
                )))
            }
        }),
    ))
}

/// `images()` on one listing page: `/image/ID` and `/view/MID` links.
/// Links without `www.` count too, as the site now writes them.
fn image_paths(page: &str) -> Vec<String> {
    let prefix = "<a href=\"https://";
    page.match_indices(prefix)
        .filter_map(|(start, _)| {
            let rest = &page[start + prefix.len()..];
            let rest = rest.strip_prefix("www.").unwrap_or(rest);
            let rest = rest.strip_prefix("imagebam.com/")?;
            ["image/", "view/M"].into_iter().find_map(|kind| {
                let id = alnum(rest.strip_prefix(kind)?);
                (!id.is_empty()).then(|| format!("/{kind}{id}"))
            })
        })
        .collect()
}

/// The `rel="next"` link of a listing page, kept only on ImageBam.
fn next_page(page: &str) -> Option<Url> {
    let pos = page.find("rel=\"next\" aria-label=\"Next")?;
    if pos == 0 {
        return None;
    }
    let raw = crate::sites::filehosts::rextr(page, "href=\"", "\"", Some(pos));
    Url::parse(raw)
        .ok()
        .filter(|url| crate::sites::art::on_host(url, &["imagebam.com"]))
}

/// `_parse_image_page`: the full-size URL and its `alt` file name, merged
/// with the gallery data.
async fn image(http: &Http, path: &str, data: Map<String, Value>) -> Result<Item> {
    let page = http.text(&http.api(&format!("{ROOT}{path}"))?).await?;
    let mut scan = Scan::new(&page);
    let src = scan.extr("<img src=\"https://images", "\"");
    let file = match src {
        "" => None,
        src => crate::sites::media_url(&format!("https://images{src}"), &["imagebam.com"]),
    };
    let Some(file) = file else {
        return Err(error("ImageBam image not found"));
    };
    let filename = crate::sites::unescape(scan.extr("alt=\"", "\""));
    let image_key = path.rsplit('/').next().unwrap_or_default().to_owned();
    let mut image = Map::new();
    image.insert("url".into(), file.as_str().into());
    image.insert("image_key".into(), image_key.clone().into());
    super::set_nameext(&mut image, nameext_from_name(&filename));
    update(&mut image, &data);
    let gallery = data
        .get("gallery_key")
        .map(crate::sites::scalar)
        .unwrap_or_else(|| image_key.clone());
    let title = image
        .get("title")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Ok(Item::new(file, image)
        .ids(gallery, image_key)
        .caption(title))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.imagebam.com/view/GA3MT1"),
            Some(Target::Gallery("/view/GA3MT1".into()))
        );
        assert_eq!(
            t("http://imagebam.com/gallery/adz2y0f9574bjpmonaismyrhtjgvey4o/x"),
            Some(Target::Gallery(
                "/gallery/adz2y0f9574bjpmonaismyrhtjgvey4o".into()
            ))
        );
        assert_eq!(
            t("https://www.imagebam.com/view/MEQSIJ6"),
            Some(Target::Image("/view/MEQSIJ6".into()))
        );
        assert_eq!(
            t("https://www.imagebam.com/image/94d56c502511890"),
            Some(Target::Image("/image/94d56c502511890".into()))
        );
        assert_eq!(
            t("https://images3.imagebam.com/1d/8c/44/94d56c502511890.png"),
            Some(Target::Image("/image/94d56c502511890".into()))
        );
        assert_eq!(
            t("https://images4.imagebam.com/43/bb/02/MEQSIJ6_o.jpg"),
            Some(Target::Image("/view/MEQSIJ6".into()))
        );
        for raw in [
            "https://www.imagebam.com/",
            "https://www.imagebam.com/view/G",
            "https://images3.imagebam.com/view/GA3MT1",
            "https://images3.imagebam.com/1D/8c/44/x.png",
            "https://a.b.imagebam.com/view/MEQSIJ6",
            "https://my-host.imagebam.com/view/MEQSIJ6",
            "https://evilimagebam.com/view/MEQSIJ6",
            "https://www.imagebam.com.evil.test/view/MEQSIJ6",
            "https://www.imagebam.com:8080/view/MEQSIJ6",
            "https://u:p@www.imagebam.com/view/MEQSIJ6",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn image_page(n: u32) -> String {
        format!(
            "<img src=\"https://images4.imagebam.com/aa/bb/cc/M{n}_o.jpg\" alt=\"Pic &amp; {n}.JPG\">"
        )
    }

    #[tokio::test]
    async fn gallery_follows_next_pages_and_reverses() {
        let first = "<h1 id=\"gallery-name\"> Holiday &amp; Sun </h1>\
            <a href=\"https://imagebam.com/view/M1\">1</a>\
            <a href=\"https://www.imagebam.com/view/G9\">other gallery</a>\
            <a href=\"https://www.imagebam.com/view/GX?page=2\" rel=\"next\" aria-label=\"Next &raquo;\">";
        let second = "<a href=\"https://www.imagebam.com/image/2\">2</a>";
        let (root, server) = serve(vec![
            html(first),
            html(second),
            html(&image_page(2)),
            html(&image_page(1)),
        ]);
        let http = http("imagebam", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.imagebam.com/view/GX").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].url.as_str(),
            "https://images4.imagebam.com/aa/bb/cc/M2_o.jpg"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["image_key"], "2");
        assert_eq!(k["title"], "Holiday & Sun");
        assert_eq!(k["gallery_key"], "GX");
        assert_eq!(k["count"], 2);
        assert_eq!(k["num"], 1);
        assert_eq!(k["filename"], "Pic & 2");
        assert_eq!(k["extension"], "jpg");
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "002 Pic & 1.jpg");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("imagebam/Holiday & Sun GX")
        );
        let (_, identity) = crate::sites::archive_key(&files[1]).unwrap();
        assert_eq!(identity, r#"["{gallery_key}_{image_key}",["GX","M1"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /view/GX "));
        assert!(requests[0].contains("nsfw_inter=1; sfw_inter=1"));
        assert!(requests[1].starts_with("GET /view/GX?page=2 "));
        assert!(requests[2].starts_with("GET /image/2 "));
        assert!(requests[3].starts_with("GET /view/M1 "));
    }

    #[tokio::test]
    async fn image_from_file_url() {
        let (root, server) = serve(vec![html(&image_page(7)), html("<p>gone</p>")]);
        let http = http("imagebam", root);
        let files = media(extract(
            &http,
            Url::parse("https://images4.imagebam.com/43/bb/02/M7_o.jpg").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(keywords(&files[0])["subcategory"], "image");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "Pic & 7.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("imagebam")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{image_key}",["M7"]]"#);
        let missing = extract(
            &http,
            Url::parse("https://www.imagebam.com/view/M8").unwrap(),
        );
        assert!(crate::sites::testing::fails(missing).await);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /view/M7 "));
    }
}
