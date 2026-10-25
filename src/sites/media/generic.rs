//! Images in any web page, mirroring gallery_dl/extractor/generic.py for
//! `generic:URL` (or `g:URL`) inputs: page metadata, then every `src`/
//! `srcset` of `<img>`, `<video>` and `<source>` and every URL-like run
//! ending in a media extension, resolved against the page and deduplicated.
//!
//! With upstream's `enabled` option, the program routes URLs no other
//! extractor matches here without the prefix.
use crate::sites::art::{extr, nameext_into};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

/// The subcategory is the page's domain, set per item as upstream does.
static GENERIC: Route = Route::new(
    "",
    "{filename}.{extension}",
    &["{category}", "{subcategory}", "{path}"],
    "{imageurl}",
);

pub static SITE: Site = Site {
    category: "generic",
    name: "generic",
    matches: |url| url.scheme() == "generic" && crate::extract::prefixed_target(url).is_some(),
    extract,
    routes: &[&GENERIC],
    api_key: false,
    cookies: None,
};

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(page_url) = crate::extract::prefixed_target(&url) else {
        return crate::sites::fail(error("unsupported generic URL"));
    };
    crate::sites::once(async move {
        let page = http.text(&http.api(page_url.as_str())?).await?;
        let items = parse(&page, &page_url)?;
        Ok(crate::sites::events(crate::sites::post(
            &SITE, http, &GENERIC, items,
        )))
    })
}

/// `metadata()` plus `images()` for one fetched page.
pub(crate) fn parse(page: &str, page_url: &Url) -> Result<Vec<Item>> {
    let mut data = metadata(page);
    let path = page_url.path();
    // Upstream's path group is absent for a bare domain.
    let path = if path == "/" && !page_url.as_str().ends_with('/') {
        ""
    } else {
        path
    };
    data.insert("path".into(), path.replace('/', "").into());
    data.insert("pageurl".into(), page_url.as_str().into());
    let domain = page_url.host_str().unwrap_or_default();
    data.insert("subcategory".into(), domain.into());
    let urls = images(page, page_url);
    data.insert("count".into(), urls.len().into());
    Ok(urls
        .into_iter()
        .enumerate()
        .filter_map(|(index, raw)| {
            let url = Url::parse(&raw)
                .ok()
                .filter(|url| matches!(url.scheme(), "http" | "https"))?;
            let mut fields = data.clone();
            fields.insert("num".into(), (index + 1).into());
            fields.insert("imageurl".into(), raw.as_str().into());
            nameext_into(&raw, &mut fields);
            Some(Item::new(url, fields))
        })
        .collect())
}

fn metadata(page: &str) -> Map<String, Value> {
    let meta = |name: &str| extr(page, &format!("<meta {name} content=\""), "\"");
    let fields = [
        ("title", extr(page, "<title>", "</title>")),
        ("description", meta("name=\"description\"")),
        ("keywords", meta("name=\"keywords\"")),
        ("language", meta("name=\"language\"")),
        ("name", meta("itemprop=\"name\"")),
        ("copyright", meta("name=\"copyright\"")),
        ("og_site", meta("property=\"og:site\"")),
        ("og_site_name", meta("property=\"og:site_name\"")),
        ("og_title", meta("property=\"og:title\"")),
        ("og_description", meta("property=\"og:description\"")),
    ];
    fields
        .into_iter()
        .filter(|(_, value)| !value.is_empty())
        .map(|(key, value)| (key.to_owned(), crate::sites::unescape(value).into()))
        .collect()
}

/// Absolute, deduplicated image URLs in source order: tag sources first,
/// then extension matches.
fn images(page: &str, page_url: &Url) -> Vec<String> {
    let url = page_url.as_str();
    let scheme = format!("{}://", page_url.scheme());
    let root = match page_url.port() {
        Some(port) => format!("{scheme}{}:{port}", page_url.host_str().unwrap_or_default()),
        None => format!("{scheme}{}", page_url.host_str().unwrap_or_default()),
    };
    let base = match base_href(page) {
        Some(base) => base.trim_end_matches('/').to_owned(),
        None if url.ends_with('/') => url.trim_end_matches('/').to_owned(),
        // `os.path.dirname`
        None => url.rsplit_once('/').map_or("", |(dir, _)| dir).to_owned(),
    };
    let mut seen = std::collections::HashSet::new();
    let mut found = Vec::new();
    for raw in tag_sources(page).into_iter().chain(extension_urls(page)) {
        let absolute = if raw.starts_with("http") {
            raw.to_owned()
        } else if raw.starts_with("//") {
            format!("{scheme}{}", raw.trim_start_matches('/'))
        } else if raw.starts_with('/') {
            format!("{root}{raw}")
        } else {
            format!("{base}/{raw}")
        };
        if seen.insert(absolute.clone()) {
            found.push(absolute);
        }
    }
    found
}

fn lower_starts(text: &str, prefix: &str) -> bool {
    text.len() >= prefix.len()
        && text.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

/// `(?i)<(?:img|video|source)\s[^>]*src(?:set)?=["']?([^"'\s>]+)`: one
/// match per tag, at the last `src`/`srcset` (greedy `[^>]*`) whose value
/// is non-empty.
fn tag_sources(page: &str) -> Vec<&str> {
    let mut found = Vec::new();
    let mut rest = page;
    while let Some(open) = rest.find('<') {
        let after = &rest[open + 1..];
        let name = ["img", "video", "source"].into_iter().find(|name| {
            lower_starts(after, name) && after[name.len()..].starts_with(char::is_whitespace)
        });
        let Some(name) = name else {
            rest = after;
            continue;
        };
        let body = &after[name.len()..];
        let tag = &body[..body.find('>').unwrap_or(body.len())];
        let value = (0..tag.len())
            .rev()
            .filter(|&i| tag.is_char_boundary(i) && lower_starts(&tag[i..], "src"))
            .find_map(|i| {
                let mut value = &tag[i + 3..];
                if lower_starts(value, "set=") {
                    value = &value[3..];
                }
                let value = value.strip_prefix('=')?;
                let value = value.strip_prefix(['"', '\'']).unwrap_or(value);
                let end = value
                    .find(|c: char| matches!(c, '"' | '\'' | '>') || c.is_whitespace())
                    .unwrap_or(value.len());
                (end > 0).then(|| &value[..end])
            });
        if let Some(value) = value {
            found.push(value);
        }
        rest = &body[tag.len()..];
    }
    found
}

const EXTENSIONS: &[&str] = &[
    "jpeg", "jpg", "jpe", "png", "gif", "webm", "webp", "mp4", "mkv", "ogg", "ogm", "ogv", "opus",
];

/// `(?i)[^?&#"'>\s]+\.(?:jpe?g|jpe|png|gif|web[mp]|mp4|mkv|og[gmv]|opus)
/// [^"'<>\s]*`: from the start of each run, up to the run's last dot before
/// a media extension, then on until a quote, bracket or space.
fn extension_urls(page: &str) -> Vec<&str> {
    let run = |c: char| !matches!(c, '?' | '&' | '#' | '"' | '\'' | '>') && !c.is_whitespace();
    let tail = |c: char| !matches!(c, '"' | '\'' | '<' | '>') && !c.is_whitespace();
    let mut found = Vec::new();
    let mut start = 0;
    while start < page.len() {
        let rest = &page[start..];
        let Some(offset) = rest.find(run) else {
            break;
        };
        let begin = start + offset;
        let run_len = page[begin..]
            .find(|c| !run(c))
            .unwrap_or(page.len() - begin);
        let span = &page[begin..begin + run_len];
        let dot = span
            .char_indices()
            .rev()
            .filter(|&(i, c)| c == '.' && i > 0)
            .find_map(|(i, _)| {
                EXTENSIONS
                    .iter()
                    .find(|ext| lower_starts(&span[i + 1..], ext))
                    .map(|ext| i + 1 + ext.len())
            });
        match dot {
            Some(ext_end) => {
                let after = begin + ext_end;
                let end = page[after..]
                    .find(|c| !tail(c))
                    .map_or(page.len(), |n| after + n);
                found.push(&page[begin..end]);
                start = end;
            }
            None => start = begin + run_len,
        }
    }
    found
}

/// `(?i)<base\s.*?href=["']?([^"' >]+)`, within one line.
fn base_href(page: &str) -> Option<&str> {
    let mut rest = page;
    while let Some(open) = rest.find('<') {
        let after = &rest[open + 1..];
        if lower_starts(after, "base") && after[4..].starts_with(char::is_whitespace) {
            let line = &after[4..after.find('\n').unwrap_or(after.len())];
            let lower = line.to_ascii_lowercase();
            if let Some(at) = lower.find("href=") {
                let value = &line[at + 5..];
                let value = value.strip_prefix(['"', '\'']).unwrap_or(value);
                let end = value.find(['"', '\'', ' ', '>']).unwrap_or(value.len());
                if end > 0 {
                    return Some(&value[..end]);
                }
            }
        }
        rest = after;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| crate::extract::input_url(raw).map(|url| (SITE.matches)(&url));
        assert!(t("generic:https://www.nongnu.org/lzip/").unwrap());
        assert!(t("G:example.org/page").unwrap());
        assert!(t("Generic:HTTP://example.org").unwrap());
        assert!(!t("https://example.org/").unwrap());
        assert!(t("generic:https://u:p@example.org/").is_err());
        assert_eq!(
            crate::extract::input_url("g:example.org/a")
                .unwrap()
                .as_str(),
            "generic:https://example.org/a"
        );
    }

    #[test]
    fn finds_tag_sources_and_extension_urls() {
        let page = r#"<html><head><base href="https://cdn.example.org/assets/"></head>
<IMG class="x" src="a.jpg" data-src="/lazy/b.png">
<img src=""><video poster=x SRCSET='//media.example.org/c.webm 2x'>
<source src="d.mp4" type="video/mp4">
<a href="https://example.org/gallery/e.JPEG?size=large">e</a> text f.gif, more
<img alt="no source"> <p>photo.jpgx</p>"#;
        assert_eq!(
            tag_sources(page),
            vec!["/lazy/b.png", "//media.example.org/c.webm", "d.mp4"]
        );
        assert_eq!(
            extension_urls(page),
            vec![
                "a.jpg",
                "/lazy/b.png",
                "//media.example.org/c.webm",
                "d.mp4",
                "https://example.org/gallery/e.JPEG?size=large",
                "f.gif,",
                "photo.jpgx",
            ]
        );
        assert_eq!(base_href(page), Some("https://cdn.example.org/assets/"));
    }

    #[tokio::test]
    async fn extracts_page_images_with_metadata() {
        let page = r#"<html><head><title>My &amp; Page</title>
<meta name="description" content="About">
<meta property="og:title" content="OG"></head>
<body><img src="/img/one.jpg"><img src="two.png">
<a href="https://other.example.net/three.webp">x</a>
<img src="/img/one.jpg"></body></html>"#;
        let (root, server) = serve(vec![html(page)]);
        let http = http("generic", root);
        let url = crate::extract::input_url("generic:https://example.org/gallery/view").unwrap();
        let files = media(extract(&http, url)).await;
        let urls: Vec<&str> = files.iter().map(|f| f.url.as_str()).collect();
        assert_eq!(
            urls,
            vec![
                "https://example.org/img/one.jpg",
                "https://example.org/gallery/two.png",
                "https://other.example.net/three.webp",
            ]
        );
        let k = keywords(&files[1]);
        assert_eq!(k["category"], "generic");
        assert_eq!(k["subcategory"], "example.org");
        assert_eq!(k["title"], "My & Page");
        assert_eq!(k["description"], "About");
        assert_eq!(k["og_title"], "OG");
        assert!(!k.contains_key("keywords"));
        assert_eq!(k["path"], "galleryview");
        assert_eq!(k["pageurl"], "https://example.org/gallery/view");
        assert_eq!(k["count"], 3);
        assert_eq!(k["num"], 2);
        assert_eq!(k["filename"], "two");
        assert_eq!(k["imageurl"], "https://example.org/gallery/two.png");
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "two.png");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("generic/example.org/galleryview")
        );
        let (_, identity) = crate::sites::archive_key(&files[1]).unwrap();
        assert!(
            identity.contains("https://example.org/gallery/two.png"),
            "{identity}"
        );
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with("GET /gallery/view "),
            "{}",
            requests[0]
        );
    }
}
