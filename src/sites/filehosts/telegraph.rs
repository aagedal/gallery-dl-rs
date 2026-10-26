//! Telegraph articles, mirroring gallery_dl/extractor/telegraph.py.
use crate::error;
use crate::sites::gdl::{
    Scan, captures, date, extract_iter, group, nameext_into, parse_iso, re, unescape,
};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://telegra.ph";

static GALLERY: Route = Route::new(
    "gallery",
    "{num_formatted}_{filename}.{extension}",
    &["{category}", "{slug}"],
    "{slug}_{num}",
);

pub static SITE: Site = Site {
    category: "telegraph",
    name: "Telegraph",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&GALLERY],
    api_key: false,
    cookies: None,
};

fn target(url: &Url) -> Option<String> {
    let pattern = re!(r"^https?://(?:www\.)?telegra\.ph(/[^/?#]+)");
    captures(pattern, url).map(|c| group(&c, 1).to_owned())
}

fn metadata(page: &str) -> Map<String, Value> {
    let mut scan = Scan::new(page);
    let mut data = Map::new();
    data.insert(
        "title".into(),
        unescape(scan.extr("property=\"og:title\" content=\"", "\"")).into(),
    );
    data.insert(
        "description".into(),
        unescape(scan.extr("property=\"og:description\" content=\"", "\"")).into(),
    );
    data.insert(
        "date".into(),
        date(parse_iso(scan.extr(
            "property=\"article:published_time\" content=\"",
            "\"",
        ))),
    );
    data.insert(
        "author".into(),
        unescape(scan.extr("property=\"article:author\" content=\"", "\"")).into(),
    );
    let post_url = unescape(scan.extr("rel=\"canonical\" href=\"", "\""));
    let slug: String = post_url.chars().skip(19).collect();
    data.insert("post_url".into(), post_url.into());
    data.insert("slug".into(), slug.into());
    data
}

fn images(page: &str, data: &Map<String, Value>) -> Vec<Item> {
    let mut figures = extract_iter(page, "<figure>", "</figure>");
    if figures.is_empty() {
        figures = extract_iter(page, "<img", ">");
    }
    let zeroes = figures.len().to_string().len();
    let mut items = Vec::new();
    let mut num = 0;
    for figure in figures {
        let mut scan = Scan::new(figure);
        let src = scan.extr("src=\"", "\"");
        if src.starts_with("/embed/") || src.is_empty() {
            continue;
        }
        let url = if src.starts_with('/') {
            format!("{ROOT}{src}")
        } else {
            src.to_owned()
        };
        let caption = scan
            .find("<figcaption>", "<")
            .map(unescape)
            .unwrap_or_default();
        num += 1;
        let Ok(file) = Url::parse(&url) else { continue };
        let mut fields = data.clone();
        fields.insert("url".into(), url.as_str().into());
        fields.insert("caption".into(), caption.into());
        fields.insert("num".into(), num.into());
        fields.insert("num_formatted".into(), format!("{num:0zeroes$}").into());
        fields.insert("count".into(), 0.into());
        nameext_into(&url, &mut fields);
        items.push(Item::new(file, fields).ids(String::new(), num.to_string()));
    }
    let count = items.len();
    for item in &mut items {
        item.keywords.insert("count".into(), count.into());
    }
    items
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(path) = target(&url) else {
        return crate::sites::fail(error("unsupported Telegraph URL"));
    };
    crate::sites::once(async move {
        let page = http.text(&http.api(&format!("{ROOT}{path}"))?).await?;
        let data = metadata(&page);
        let items = images(&page, &data);
        Ok(crate::sites::events(crate::sites::post(
            &SITE, http, &GALLERY, items,
        )))
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
            t("https://telegra.ph/Telegraph-Test-03-28").as_deref(),
            Some("/Telegraph-Test-03-28")
        );
        assert!(t("https://telegra.ph/森-03-28").is_some());
        assert_eq!(t("https://telegram.org/x"), None);
    }

    #[tokio::test]
    async fn extracts_figures() {
        let page = r#"<meta property="og:title" content="Test &amp; Title"><meta property="og:description" content="D">
            <meta property="article:published_time" content="2022-03-28T16:59:55+0000"><meta property="article:author" content="me">
            <link rel="canonical" href="https://telegra.ph/Telegraph-Test-03-28">
            <figure><img src="/file/abc.jpg"><figcaption>One &amp; two</figcaption></figure>
            <figure><iframe src="/embed/youtube?x"></iframe></figure>
            <figure><img src="https://i.test/x.png"></figure>"#;
        let (root, server) = serve(vec![html(page)]);
        let http = client("telegraph", root);
        let files = media(extract(
            &http,
            Url::parse("https://telegra.ph/Telegraph-Test-03-28").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].url.as_str(), "https://telegra.ph/file/abc.jpg");
        let k = keywords(&files[0]);
        assert_eq!(k["slug"], "Telegraph-Test-03-28");
        assert_eq!(k["caption"], "One & two");
        assert_eq!(k["num_formatted"], "1");
        assert_eq!(k["date"], "2022-03-28 16:59:55");
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "2_x.png");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("telegraph/Telegraph-Test-03-28")
        );
        server.join().unwrap();
    }
}
