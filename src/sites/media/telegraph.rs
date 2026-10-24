//! Telegraph articles, mirroring gallery_dl/extractor/telegraph.py: the
//! article page's Open Graph data and its figure images.
use super::{date, iso_ts, path_segment};
use crate::sites::art::{Scan, extract_iter, file_url, nameext_into, update};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
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

/// `(?:www\.)??telegra\.ph(/[^/?#]+)`: the article path.
fn target(url: &Url) -> Option<String> {
    if !crate::sites::art::on_host(url, &["telegra.ph"]) {
        return None;
    }
    let first = url.path()[1..].split('/').next()?;
    path_segment(first).then(|| format!("/{first}"))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(path) = target(&url) else {
        return crate::sites::fail(error("unsupported Telegraph URL"));
    };
    crate::sites::once(async move {
        let page = http.text(&http.api(&format!("{ROOT}{path}"))?).await?;
        let items = parse_article(&page)?;
        Ok(crate::sites::events(crate::sites::post(
            &SITE, http, &GALLERY, items,
        )))
    })
}

/// `metadata(page)`.
fn metadata(page: &str) -> (Map<String, Value>, Option<i64>) {
    let mut scan = Scan::new(page);
    let mut data = Map::new();
    let unescape = crate::sites::unescape;
    data.insert(
        "title".into(),
        unescape(scan.extr("property=\"og:title\" content=\"", "\"")).into(),
    );
    data.insert(
        "description".into(),
        unescape(scan.extr("property=\"og:description\" content=\"", "\"")).into(),
    );
    let ts = iso_ts(scan.extr("property=\"article:published_time\" content=\"", "\""));
    data.insert("date".into(), date(ts));
    data.insert(
        "author".into(),
        unescape(scan.extr("property=\"article:author\" content=\"", "\"")).into(),
    );
    let post_url = unescape(scan.extr("rel=\"canonical\" href=\"", "\""));
    let slug: String = post_url.chars().skip(19).collect();
    data.insert("post_url".into(), post_url.into());
    data.insert("slug".into(), slug.into());
    (data, ts)
}

/// `images(page)` with `GalleryExtractor` numbering: `<figure>` images, or
/// every `<img>` when there are none; embeds are skipped.
fn parse_article(page: &str) -> Result<Vec<Item>> {
    let (mut data, ts) = metadata(page);
    let mut figures = extract_iter(page, "<figure>", "</figure>");
    if figures.is_empty() {
        figures = extract_iter(page, "<img", ">");
    }
    let width = figures.len().to_string().len();
    let mut images = Vec::new();
    for figure in figures {
        let mut scan = Scan::new(figure);
        let Some(src) = scan.find("src=\"", "\"") else {
            continue;
        };
        if src.starts_with("/embed/") {
            continue;
        }
        let caption = scan
            .find("<figcaption>", "<")
            .filter(|c| !c.is_empty())
            .map(crate::sites::unescape)
            .unwrap_or_default();
        let raw = if src.starts_with('/') {
            format!("{ROOT}{src}")
        } else {
            src.to_owned()
        };
        images.push((raw, caption));
    }
    data.insert("count".into(), images.len().into());
    let slug = data
        .get("slug")
        .map(crate::sites::scalar)
        .unwrap_or_default();
    let author = data
        .get("author")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let title = data.get("title").and_then(Value::as_str).map(str::to_owned);
    let mut items = Vec::new();
    for (index, (raw, caption)) in images.into_iter().enumerate() {
        let num = index + 1;
        let mut image = Map::new();
        image.insert("url".into(), raw.clone().into());
        image.insert("caption".into(), caption.into());
        image.insert("num".into(), num.into());
        image.insert("num_formatted".into(), format!("{num:0width$}").into());
        update(&mut data, &image);
        nameext_into(&raw, &mut data);
        let Some(url) = file_url(&raw, ROOT) else {
            continue;
        };
        items.push(
            Item::new(url, data.clone())
                .ids(slug.clone(), num.to_string())
                .user(author.clone())
                .caption(title.clone())
                .timestamp(ts),
        );
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://telegra.ph/Telegraph-Test-03-28"),
            Some("/Telegraph-Test-03-28".into())
        );
        assert_eq!(t("http://www.telegra.ph/x/y?z"), Some("/x".into()));
        for raw in [
            "https://telegra.ph/",
            "https://telegra.ph/%2e%2e",
            "https://api.telegra.ph/x",
            "https://telegra.ph.evil.test/x",
            "https://eviltelegra.ph/x",
            "https://telegra.ph:8443/x",
            "https://u:p@telegra.ph/x",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[tokio::test]
    async fn extracts_figures() {
        let mut page = String::from(
            "<meta property=\"og:title\" content=\"Test &amp; Title\">\
             <meta property=\"og:description\" content=\"Desc\">\
             <meta property=\"article:published_time\" content=\"2022-03-28T12:00:00+0000\">\
             <meta property=\"article:author\" content=\"Writer\">\
             <link rel=\"canonical\" href=\"https://telegra.ph/Test-Title-03-28\">\
             <figure><iframe src=\"/embed/youtube?url=x\"></iframe></figure>",
        );
        for i in 1..=10 {
            page.push_str(&format!(
                "<figure><img src=\"/file/img{i}.jpg\"><figcaption>Cap &lt;{i}&gt;</figcaption></figure>"
            ));
        }
        page.push_str("<figure><img src=\"https://cdn.example.test/ext.png\"></figure>");
        let (root, server) = serve(vec![html(&page)]);
        let http = http("telegraph", root);
        let files = media(extract(
            &http,
            Url::parse("https://telegra.ph/Test-Title-03-28").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 11);
        assert_eq!(files[0].url.as_str(), "https://telegra.ph/file/img1.jpg");
        let k = keywords(&files[0]);
        assert_eq!(k["title"], "Test & Title");
        assert_eq!(k["slug"], "Test-Title-03-28");
        assert_eq!(k["date"], "2022-03-28 12:00:00");
        assert_eq!(k["count"], 11);
        assert_eq!(k["num_formatted"], "01");
        assert_eq!(k["caption"], "Cap <1>");
        let k = keywords(&files[10]);
        assert_eq!(k["caption"], "");
        assert_eq!(k["num"], 11);
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "01_img1.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("telegraph/Test-Title-03-28")
        );
        let (_, identity) = crate::sites::archive_key(&files[10]).unwrap();
        assert_eq!(identity, r#"["{slug}_{num}",["Test-Title-03-28","11"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /Test-Title-03-28 "));
    }
}
