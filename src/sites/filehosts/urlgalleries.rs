//! urlgalleries galleries, mirroring gallery_dl/extractor/urlgalleries.py:
//! each gallery entry's page names an image host URL (`IMAGE_URL`), which
//! is queued for that host's extractor.
use super::Extr;
use crate::sites::art::{extr, split_html};
use crate::sites::{Events, Http, Route, Site};
use crate::{Result, error};
use futures_util::StreamExt;
use serde_json::{Map, Value};
use url::Url;

pub static SITE: Site = Site {
    category: "urlgalleries",
    name: "urlgalleries",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&GALLERY],
    api_key: false,
    cookies: None,
};

static GALLERY: Route = Route::new(
    "gallery",
    "{category}_{gallery_id}_{num:>03}.{extension}",
    &["{category}", "{gallery_id} {title}"],
    "{gallery_id}_{num}",
);

const ROOT: &str = "https://urlgalleries.com";

/// Blog, gallery ID and the optional `/SLUG` of
/// `(?:\w+\.)?urlgalleries\.com/([^/?#]+)/(\d+)(/[^/?#]+)?`.
#[derive(Debug, PartialEq)]
struct Target {
    blog: String,
    id: String,
    slug: String,
}

fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) {
        return None;
    }
    let sub = url.host_str()?.strip_suffix("urlgalleries.com")?;
    if !sub.is_empty() {
        let label = sub.strip_suffix('.')?;
        if label.is_empty()
            || !label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return None;
        }
    }
    let mut parts = url.path()[1..].split('/');
    let blog = parts.next().filter(|blog| !blog.is_empty())?;
    let second = parts.next()?;
    let id_len = second
        .bytes()
        .position(|b| !b.is_ascii_digit())
        .unwrap_or(second.len());
    if id_len == 0 || id_len > 20 {
        return None;
    }
    let slug = match (id_len == second.len(), parts.next()) {
        (true, Some(slug)) if !slug.is_empty() => format!("/{slug}"),
        _ => String::new(),
    };
    Some(Target {
        blog: blog.to_owned(),
        id: second[..id_len].to_owned(),
        slug,
    })
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported urlgalleries URL"));
    };
    crate::sites::once(async move { gallery(http, target).await })
}

async fn gallery(http: &Http, target: Target) -> Result<Events<'_>> {
    let Target { blog, id, slug } = target;
    let url = http.api(&format!("{ROOT}/{blog}/{id}{slug}/?a=10000"))?;
    let page = http.text(&url).await?;
    let section = extr(&page, "<section ", "</section>");
    let images: Vec<String> = super::extract_iter(section, " href=\"", "\"")
        .into_iter()
        .map(str::to_owned)
        .collect();
    let mut data = metadata(&page, &id);
    data.insert("count".into(), images.len().into());
    // Each entry's page is requested as the consumer reaches it.
    Ok(crate::sites::pages(
        futures_util::stream::iter(images.into_iter().enumerate()).then(move |(index, path)| {
            let mut data = data.clone();
            data.insert("num".into(), (index + 1).into());
            async move {
                let page = http.text(&http.api(&format!("{ROOT}{path}"))?).await?;
                let raw = extr(&page, " IMAGE_URL = \"", "\"");
                let Some(url) = super::web_url(raw) else {
                    return Ok(crate::sites::events(Vec::new()));
                };
                Ok(crate::sites::queue(
                    http,
                    SITE.category,
                    GALLERY.subcategory,
                    url,
                    &data,
                ))
            }
        }),
    ))
}

fn metadata(page: &str, id: &str) -> Map<String, Value> {
    let mut scan = Extr::new(page);
    let mut data = Map::new();
    data.insert("gallery_id".into(), id.into());
    let desc = scan
        .next("property=\"og:description\" content=\"", "\"")
        .to_owned();
    let date = crate::sites::art::parse_iso(scan.next("\"datePublished\":\"", "\""));
    data.insert("date".into(), super::date_value(date));
    let blog = scan.next("\"@type\":\"ListItem\",\"position\":2,\"name\":\"", "\"");
    data.insert("blog".into(), crate::sites::unescape(blog).into());
    let title = scan.next("\"@type\":\"ListItem\",\"position\":3,\"name\":\"", "\"");
    data.insert("title".into(), crate::sites::unescape(title.trim()).into());
    let tags = split_html(scan.next("<div class=\"badgeCatsInner\"", "</div>"));
    data.insert(
        "tags".into(),
        tags.into_iter().skip(1).collect::<Vec<_>>().into(),
    );
    if desc.contains(". Published by ") {
        data.insert("blog".into(), extr(&desc, ". Published by ", ".").into());
    }
    data
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, queued, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        let gallery = |blog: &str, id: &str, slug: &str| {
            Some(Target {
                blog: blog.into(),
                id: id.into(),
                slug: slug.into(),
            })
        };
        assert_eq!(
            t("https://urlgalleries.com/photos2q/2368085/sweet-girl"),
            gallery("photos2q", "2368085", "/sweet-girl")
        );
        assert_eq!(
            t("https://photos2q.urlgalleries.com/blog/123x/y"),
            gallery("blog", "123", "")
        );
        assert_eq!(t("http://urlgalleries.com/b/9"), gallery("b", "9", ""));
        for raw in [
            "https://urlgalleries.com/blog",
            "https://urlgalleries.com/blog/x",
            "https://a.b.urlgalleries.com/blog/1",
            "https://evilurlgalleries.com/blog/1",
            "https://urlgalleries.com:81/blog/1",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[tokio::test]
    async fn queues_each_entry_image() {
        let page = "<meta property=\"og:description\" content=\"Pics. Published by Somebody.\">\
            {\"datePublished\":\"2024-06-05T10:00:00+00:00\"}\
            {\"@type\":\"ListItem\",\"position\":2,\"name\":\"blog &amp; co\"}\
            {\"@type\":\"ListItem\",\"position\":3,\"name\":\" Sweet &amp; Girl \"}\
            <div class=\"badgeCatsInner\"><a>one</a><a>two</a></div>\
            <section class=\"g\"><a href=\"/photos2q/2368085/1\">1</a><a href=\"/photos2q/2368085/2\">2</a></section>";
        let entry = "<script>var IMAGE_URL = \"https://imx.to/i/1qdeva\";</script>";
        let (root, server) = serve(vec![html(page), html(entry), html("<p>none</p>")]);
        let http = http("urlgalleries", root);
        let children = queued(extract(
            &http,
            Url::parse("https://urlgalleries.com/photos2q/2368085/sweet-girl").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].category(), "urlgalleries");
        assert_eq!(children[0].subcategory(), "gallery");
        let data = metadata(page, "2368085");
        assert_eq!(data["blog"], "Somebody");
        assert_eq!(data["title"], "Sweet & Girl");
        assert_eq!(data["tags"], serde_json::json!(["one", "two"]));
        assert_eq!(data["date"], "2024-06-05 10:00:00");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /photos2q/2368085/sweet-girl/?a=10000 "));
        assert!(requests[1].starts_with("GET /photos2q/2368085/1 "));
        assert!(requests[2].starts_with("GET /photos2q/2368085/2 "));
    }
}
