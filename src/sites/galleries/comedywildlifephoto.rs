//! Comedy Wildlife Photography Awards galleries, mirroring
//! gallery_dl/extractor/comedywildlifephoto.py.
use crate::error;
use crate::sites::gdl::{
    Scan, captures, extr, extract_iter, group, parse_int, partition, re, unescape,
};
use crate::sites::{Events, Http, Route, Site};
use serde_json::Map;
use url::Url;

const ROOT: &str = "https://www.comedywildlifephoto.com";

static GALLERY: Route = Route::new(
    "gallery",
    "{num:>03} {filename}.{extension}",
    &["{category}", "{section}", "{title}"],
    "{section}/{title}/{num}",
);

pub static SITE: Site = Site {
    category: "comedywildlifephoto",
    name: "Comedy Wildlife Photo",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&GALLERY],
    api_key: false,
    cookies: None,
};

fn target(url: &Url) -> Option<String> {
    captures(
        re!(r"^https?://(?:www\.)?comedywildlifephoto\.com(/gallery/[^/?#]+/[^/?#]+\.php)"),
        url,
    )
    .map(|c| group(&c, 1).into())
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(path) = target(&url) else {
        return crate::sites::fail(error("unsupported Comedy Wildlife Photo URL"));
    };
    crate::sites::once(async move {
        let page = http.text(&http.api(&format!("{ROOT}{path}"))?).await?;
        let mut scan = Scan::new(&page);
        let mut data = Map::new();
        data.insert("section".into(), scan.extr("<h1>", "<").trim().into());
        data.insert("title".into(), scan.extr(">", "<").into());
        data.insert(
            "description".into(),
            unescape(scan.extr("class=\"c1 np\">", "<div")).into(),
        );
        let images = extract_iter(&page, "<figure", "</figure>")
            .into_iter()
            .map(|fig| {
                let (width, _, height) = partition(extr(fig, "data-size=\"", "\""), "x");
                let mut image = Map::new();
                image.insert("width".into(), parse_int(width).into());
                image.insert("height".into(), parse_int(height).into());
                image.insert(
                    "caption".into(),
                    unescape(extr(fig, "<figcaption>", "<")).into(),
                );
                (format!("{ROOT}{}", extr(fig, "href=\"", "\"")), Some(image))
            })
            .collect();
        Ok(crate::sites::events(super::numbered(
            &SITE, http, &GALLERY, &data, images, &path,
        )))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http as client, keywords, media, serve};

    #[tokio::test]
    async fn extracts_figures() {
        let page = r#"<h1> Winners <span>2024</span></h1><p class="c1 np">Desc &amp; more<div>
            <figure data-size="800x600"><a href="/images/a.jpg"></a><figcaption>Caption</figcaption></figure>"#;
        let (root, server) = serve(vec![html(page)]);
        let http = client("comedywildlifephoto", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.comedywildlifephoto.com/gallery/finalists/2024_finalists.php")
                .unwrap(),
        ))
        .await;
        assert_eq!(
            files[0].url.as_str(),
            "https://www.comedywildlifephoto.com/images/a.jpg"
        );
        let k = keywords(&files[0]);
        assert_eq!(
            (
                k["section"].as_str(),
                k["title"].as_str(),
                k["width"].as_i64()
            ),
            (Some("Winners"), Some("2024"), Some(800))
        );
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "001 a.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("comedywildlifephoto/Winners/2024")
        );
        server.join().unwrap();
    }
}
