//! Comedy Wildlife Photography Awards galleries, mirroring
//! gallery_dl/extractor/comedywildlifephoto.py: each `<figure>` on a gallery
//! page links one full-size photo.
use super::{Scan, extr, extract_iter, on_host, parse_int, raw_segments};
use crate::error;
use crate::sites::{Events, Http, Item, Route, Site, unescape};
use serde_json::{Map, Value};
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
    name: "Comedy Wildlife Photography Awards",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&GALLERY],
    api_key: false,
    cookies: None,
};

/// `(/gallery/[^/?#]+/[^/?#]+\.php)`: the page path, raw as upstream sees
/// it; the greedy name ends at the segment's last `.php`.
fn target(url: &Url) -> Option<String> {
    if !on_host(url, &["comedywildlifephoto.com"]) {
        return None;
    }
    match raw_segments(url).as_slice() {
        ["gallery", section, name, ..]
            if url.path().starts_with(&format!("/gallery/{section}/")) =>
        {
            let end = name.rfind(".php").filter(|end| *end > 0)?;
            Some(format!("/gallery/{section}/{}", &name[..end + 4]))
        }
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(path) = target(&url) else {
        return crate::sites::fail(error("unsupported Comedy Wildlife Photography URL"));
    };
    crate::sites::once(async move {
        let page = http.text(&http.api(&format!("{ROOT}{path}"))?).await?;
        Ok(crate::sites::events(crate::sites::post(
            &SITE,
            http,
            &GALLERY,
            parse_gallery(&page),
        )))
    })
}

/// GalleryExtractor `metadata` and `images` over one gallery page.
pub(crate) fn parse_gallery(page: &str) -> Vec<Item> {
    let mut scan = Scan::new(page);
    let mut data = Map::new();
    data.insert("section".into(), scan.extr("<h1>", "<").trim().into());
    let title = scan.extr(">", "<").to_owned();
    data.insert("title".into(), title.clone().into());
    data.insert(
        "description".into(),
        unescape(scan.extr("class=\"c1 np\">", "<div")).into(),
    );
    let figures = extract_iter(page, "<figure", "</figure>");
    data.insert("count".into(), figures.len().into());
    let mut items = Vec::new();
    for (index, figure) in figures.into_iter().enumerate() {
        let href = extr(figure, "href=\"", "\"");
        let size = extr(figure, "data-size=\"", "\"");
        let (width, height) = size.split_once('x').unwrap_or((size, ""));
        let mut fields = data.clone();
        fields.insert("num".into(), (index + 1).into());
        fields.insert("width".into(), parse_int(width, 0).into());
        fields.insert("height".into(), parse_int(height, 0).into());
        fields.insert(
            "caption".into(),
            unescape(extr(figure, "<figcaption>", "<")).into(),
        );
        let raw = format!("{ROOT}{href}");
        super::nameext_into(&raw, &mut fields);
        let Some(url) = Url::parse(&raw).ok().filter(crate::sites::plain) else {
            continue;
        };
        let caption = fields
            .get("caption")
            .and_then(Value::as_str)
            .map(str::to_owned);
        items.push(
            Item::new(url, fields)
                .ids(title.clone(), (index + 1).to_string())
                .caption(caption),
        );
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.comedywildlifephoto.com/gallery/finalists/2024_finalists.php"),
            Some("/gallery/finalists/2024_finalists.php".into())
        );
        assert_eq!(
            t("http://comedywildlifephoto.com/gallery/s/a.php.phpx?x"),
            Some("/gallery/s/a.php.php".into())
        );
        for raw in [
            "https://www.comedywildlifephoto.com/gallery/s/.php",
            "https://www.comedywildlifephoto.com/gallery/s/a.html",
            "https://www.comedywildlifephoto.com/gallery/a.php",
            "https://www.comedywildlifephoto.com.evil.com/gallery/s/a.php",
            "https://cdn.comedywildlifephoto.com/gallery/s/a.php",
            "https://www.comedywildlifephoto.com:81/gallery/s/a.php",
            "https://u@www.comedywildlifephoto.com/gallery/s/a.php",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[tokio::test]
    async fn extracts_gallery() {
        let page = r#"<h1> Winners <span>2024 &amp; more</span></h1>
            <p class="c1 np">About &amp; this<div>
            <figure><a href="/images/gallery/1/a_b.jpg" data-size="1200x800"><figcaption>Owl &amp; cat</figcaption></a></figure>
            <figure><a href="/images/gallery/1/c.JPG" data-size="x"></a></figure>"#;
        let (root, server) = serve(vec![html(page)]);
        let http = http("Comedy Wildlife", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.comedywildlifephoto.com/gallery/winners/2024.php").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[0]);
        assert_eq!(k["section"], "Winners");
        assert_eq!(k["title"], "2024 &amp; more");
        assert_eq!(k["description"], "About & this");
        assert_eq!(k["count"], 2);
        assert_eq!(k["width"], 1200);
        assert_eq!(k["height"], 800);
        assert_eq!(k["caption"], "Owl & cat");
        assert_eq!(keywords(&files[1])["width"], 0);
        assert_eq!(keywords(&files[1])["extension"], "jpg");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "001 a_b.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("comedywildlifephoto/Winners/2024 &amp; more")
        );
        let (_, identity) = crate::sites::archive_key(&files[1]).unwrap();
        assert_eq!(
            identity,
            r#"["{section}/{title}/{num}",["Winners","2024 &amp; more","2"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /gallery/winners/2024.php "));
    }
}
