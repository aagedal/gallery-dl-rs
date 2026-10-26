//! urlgalleries galleries, mirroring gallery_dl/extractor/urlgalleries.py:
//! each thumbnail page names its image URL, which is queued for the image
//! host's extractor.
use crate::error;
use crate::sites::gdl::{
    Scan, captures, date, extr, extract_iter, group, parse_iso, re, split_html, unescape,
};
use crate::sites::{Events, Http, Route, Site};
use futures_util::StreamExt;
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://urlgalleries.com";

static GALLERY: Route = Route::new(
    "gallery",
    "{category}_{gallery_id}_{num:>03}.{extension}",
    &["{category}", "{gallery_id} {title}"],
    "{gallery_id}_{num}",
);

pub static SITE: Site = Site {
    category: "urlgalleries",
    name: "urlgalleries",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&GALLERY],
    api_key: false,
    cookies: None,
};

fn target(url: &Url) -> Option<(String, String, String)> {
    let pattern = re!(r"^https?://(?:\w+\.)?urlgalleries\.com/([^/?#]+)/(\d+)(/[^/?#]+)?");
    let c = captures(pattern, url)?;
    Some((
        group(&c, 1).into(),
        group(&c, 2).into(),
        group(&c, 3).into(),
    ))
}

fn metadata(page: &str, gallery_id: &str) -> Map<String, Value> {
    let mut scan = Scan::new(page);
    let desc = scan
        .extr("property=\"og:description\" content=\"", "\"")
        .to_owned();
    let mut data = Map::new();
    data.insert("gallery_id".into(), gallery_id.into());
    data.insert(
        "date".into(),
        date(parse_iso(scan.extr("\"datePublished\":\"", "\""))),
    );
    data.insert(
        "blog".into(),
        unescape(scan.extr("\"@type\":\"ListItem\",\"position\":2,\"name\":\"", "\"")).into(),
    );
    let title = unescape(
        scan.extr("\"@type\":\"ListItem\",\"position\":3,\"name\":\"", "\"")
            .trim(),
    );
    data.insert("title".into(), title.into());
    let tags: Vec<String> = split_html(scan.extr("<div class=\"badgeCatsInner\"", "</div>"))
        .into_iter()
        .skip(1)
        .collect();
    data.insert("tags".into(), tags.into());
    if desc.contains(". Published by ") {
        data.insert("blog".into(), extr(&desc, ". Published by ", ".").into());
    }
    data
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some((blog, gallery_id, slug)) = target(&url) else {
        return crate::sites::fail(error("unsupported urlgalleries URL"));
    };
    crate::sites::once(async move {
        let page = http
            .text(&http.api(&format!("{ROOT}/{blog}/{gallery_id}{slug}/?a=10000"))?)
            .await?;
        let images: Vec<String> =
            extract_iter(extr(&page, "<section ", "</section>"), " href=\"", "\"")
                .into_iter()
                .map(str::to_owned)
                .collect();
        let mut data = metadata(&page, &gallery_id);
        data.insert("count".into(), images.len().into());
        let stream = futures_util::stream::iter(images.into_iter().enumerate())
            .then(move |(index, path)| {
                let mut data = data.clone();
                async move {
                    let page = http.text(&http.api(&format!("{ROOT}{path}"))?).await?;
                    let image = Url::parse(extr(&page, " IMAGE_URL = \"", "\""))
                        .map_err(|_| error("urlgalleries image page has no image URL"))?;
                    data.insert("num".into(), (index + 1).into());
                    Ok::<_, crate::Error>(crate::sites::queue(
                        http,
                        SITE.category,
                        GALLERY.subcategory,
                        image,
                        &data,
                    ))
                }
            })
            .map(|result| result.unwrap_or_else(crate::sites::fail));
        Ok(stream.flatten().boxed())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{children, html, http as client, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://urlgalleries.com/dreamer/7645840"),
            Some(("dreamer".into(), "7645840".into(), String::new()))
        );
        assert_eq!(
            t("https://xarchivesx.urlgalleries.com/xarchivesx/6722560/caroline/"),
            Some(("xarchivesx".into(), "6722560".into(), "/caroline".into()))
        );
        assert_eq!(t("https://urlgalleries.com/dreamer"), None);
    }

    #[tokio::test]
    async fn queues_image_host_urls() {
        let gallery = r#"<meta property="og:description" content="Gallery. Published by dreamer. More">
            "datePublished":"2024-01-02T03:04:05+00:00" "@type":"ListItem","position":2,"name":"x" "@type":"ListItem","position":3,"name":" Title "
            <div class="badgeCatsInner"><a>a</a></div>
            <section class="g"><a href="/porn-pic-1"></a><a href="/porn-pic-2"></a></section>"#;
        let (root, server) = serve(vec![
            html(gallery),
            html(r#"var IMAGE_URL = "https://imx.to/i/abc";"#),
            html(r#"var IMAGE_URL = "https://example.org/b.jpg";"#),
        ]);
        let http = client("urlgalleries", root);
        let queued = children(extract(
            &http,
            Url::parse("https://urlgalleries.com/dreamer/7645840").unwrap(),
        ))
        .await;
        assert_eq!(queued.len(), 2);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /dreamer/7645840/?a=10000 "));
        assert!(requests[2].starts_with("GET /porn-pic-2 "));
        let data = metadata(gallery, "7645840");
        assert_eq!(data["blog"], "dreamer");
        assert_eq!(data["title"], "Title");
        assert_eq!(data["tags"], serde_json::json!(["a"]));
    }
}
