//! Images and videos linked from any web page, for `g:`/`generic:`
//! prefixed inputs, mirroring gallery_dl/extractor/generic.py.
use crate::error;
use crate::sites::gdl::{ensure_http_scheme, extr, nameext_into, re, unescape};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

static GENERIC: Route = Route::new(
    "",
    "{filename}.{extension}",
    &["{category}", "{subcategory}", "{path}"],
    "{imageurl}",
);

pub static SITE: Site = Site {
    category: "generic",
    name: "generic",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&GENERIC],
    api_key: false,
    cookies: None,
};

/// The page URL, its scheme and domain, and its path.
#[derive(Debug, PartialEq)]
struct Target {
    url: String,
    scheme: String,
    domain: String,
    path: String,
}

fn target(url: &Url) -> Option<Target> {
    // `(?i)g(?:eneric)?:`; URL parsing already lowercased the scheme.
    let rest = url
        .as_str()
        .strip_prefix("g:")
        .or_else(|| url.as_str().strip_prefix("generic:"))?;
    // Opaque URLs percent-encode non-ASCII hosts such as `räksmörgås.se`.
    let decoded;
    let rest = match rest.find("://").map(|at| at + 3) {
        Some(start) if rest.contains('%') => {
            let end = rest[start..]
                .find(['/', '?', '#'])
                .map_or(rest.len(), |at| start + at);
            decoded = format!(
                "{}{}{}",
                &rest[..start],
                crate::sites::percent_decode(&rest[start..end]),
                &rest[end..]
            );
            decoded.as_str()
        }
        _ => rest,
    };
    let c = re!(r"^(?i)(https?://)?([-\w\.]+)(/[^?#]*)?(?:\?[^#]*)?(?:#.*)?$").captures(rest)?;
    let scheme = c.get(1).map_or("https://", |m| m.as_str());
    let url = if c.get(1).is_some() {
        rest.to_owned()
    } else {
        ensure_http_scheme(rest)
    };
    Some(Target {
        url,
        scheme: scheme.to_owned(),
        domain: c[2].to_owned(),
        path: c.get(3).map_or("", |m| m.as_str()).to_owned(),
    })
}

/// `metadata`: common `<title>`/`<meta>` values of the page.
fn metadata(page: &str, target: &Target) -> Map<String, Value> {
    let fields = [
        ("title", "<title>", "</title>"),
        ("description", "<meta name=\"description\" content=\"", "\""),
        ("keywords", "<meta name=\"keywords\" content=\"", "\""),
        ("language", "<meta name=\"language\" content=\"", "\""),
        ("name", "<meta itemprop=\"name\" content=\"", "\""),
        ("copyright", "<meta name=\"copyright\" content=\"", "\""),
        ("og_site", "<meta property=\"og:site\" content=\"", "\""),
        (
            "og_site_name",
            "<meta property=\"og:site_name\" content=\"",
            "\"",
        ),
        ("og_title", "<meta property=\"og:title\" content=\"", "\""),
        (
            "og_description",
            "<meta property=\"og:description\" content=\"",
            "\"",
        ),
    ];
    let mut data: Map<String, Value> = fields
        .iter()
        .filter_map(|(key, begin, end)| {
            let value = extr(page, begin, end);
            (!value.is_empty()).then(|| ((*key).to_owned(), unescape(value).into()))
        })
        .collect();
    data.insert("path".into(), target.path.replace('/', "").into());
    data.insert("pageurl".into(), target.url.as_str().into());
    data
}

/// `images`: `src`/`srcset` values of media elements and anything with a
/// media extension, made absolute and deduplicated in order.
fn images(page: &str, target: &Target) -> Vec<String> {
    let src = re!(r#"(?i)<(?:img|video|source)\s[^>]*src(?:set)?=["']?([^"'\s>]+)"#);
    let ext = re!(
        r#"(?i)(?:[^?&#"'>\s]+)\.(?:jpe?g|jpe|png|gif|web[mp]|mp4|mkv|og[gmv]|opus)(?:[^"'<>\s]*)?"#
    );
    let found = src
        .captures_iter(page)
        .map(|c| c.get(1).map_or("", |m| m.as_str()))
        .chain(ext.find_iter(page).map(|m| m.as_str()));
    let base = match re!(r#"(?i)(?:<base\s.*?href=["']?)([^"' >]+)"#).captures(page) {
        Some(c) => c[1].trim_end_matches('/').to_owned(),
        None if target.url.ends_with('/') => target.url.trim_end_matches('/').to_owned(),
        // `os.path.dirname`.
        None => target
            .url
            .rsplit_once('/')
            .map_or("", |(dir, _)| dir)
            .to_owned(),
    };
    let root = format!("{}{}", target.scheme, target.domain);
    let mut seen = std::collections::HashSet::new();
    found
        .map(|u| {
            if u.starts_with("http") {
                u.to_owned()
            } else if u.starts_with("//") {
                format!("{}{}", target.scheme, u.trim_start_matches('/'))
            } else if u.starts_with('/') {
                format!("{root}{u}")
            } else {
                format!("{base}/{u}")
            }
        })
        .filter(|u| seen.insert(u.clone()))
        .collect()
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported generic URL"));
    };
    crate::sites::once(async move {
        let page = http.text(&http.api(&target.url)?).await?;
        let mut data = metadata(&page, &target);
        let urls = images(&page, &target);
        data.insert("count".into(), urls.len().into());
        data.insert("_subcategory".into(), target.domain.as_str().into());
        let items = urls
            .into_iter()
            .enumerate()
            .filter_map(|(index, raw)| {
                let url = Url::parse(&raw).ok()?;
                let mut keywords = data.clone();
                keywords.insert("num".into(), (index + 1).into());
                keywords.insert("imageurl".into(), raw.as_str().into());
                nameext_into(&raw, &mut keywords);
                Some(Item::new(url, keywords))
            })
            .collect();
        Ok(crate::sites::events(crate::sites::post(
            &SITE, http, &GENERIC, items,
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
        let page = t("generic:https://www.nongnu.org/lzip/").unwrap();
        assert_eq!(
            (page.url.as_str(), page.domain.as_str(), page.path.as_str()),
            ("https://www.nongnu.org/lzip/", "www.nongnu.org", "/lzip/")
        );
        let idn = t("generic:https://räksmörgås.josefsson.org/").unwrap();
        assert_eq!(idn.domain, "räksmörgås.josefsson.org");
        let bare = t("g:example.org/a/b.html").unwrap();
        assert_eq!(
            (bare.url.as_str(), bare.scheme.as_str()),
            ("https://example.org/a/b.html", "https://")
        );
        assert_eq!(t("https://example.org/"), None);
    }

    #[test]
    fn resolves_image_urls() {
        let target =
            target(&Url::parse("generic:https://example.org/dir/page.html").unwrap()).unwrap();
        let page = r#"<img src="a.png"><img srcset="/b.jpg 1x, //cdn.example.org/c.webp 2x">
            <video src="https://v.example.org/d.mp4"></video><a href="a.png">dup</a>"#;
        assert_eq!(
            images(page, &target),
            [
                "https://example.org/dir/a.png",
                "https://example.org/b.jpg",
                "https://v.example.org/d.mp4",
                "https://cdn.example.org/c.webp"
            ]
        );
    }

    #[tokio::test]
    async fn page_metadata_and_paths() {
        let (root, _server) = serve(vec![html(
            r#"<title>Lzip &amp; more</title><meta name="description" content="Compressor"><img src="/img/logo.png">"#,
        )]);
        let http = client("generic", root);
        let files = media(extract(
            &http,
            Url::parse("generic:https://www.nongnu.org/lzip/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let k = keywords(&files[0]);
        assert_eq!(
            (
                k["title"].as_str(),
                k["subcategory"].as_str(),
                k["path"].as_str()
            ),
            (Some("Lzip & more"), Some("www.nongnu.org"), Some("lzip"))
        );
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "logo.png");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            crate::sites::testing::os_dir("generic/www.nongnu.org/lzip")
        );
    }
}
