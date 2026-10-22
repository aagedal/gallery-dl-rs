//! Hatena Blog, mirroring gallery_dl/extractor/hatenablog.py.
//!
//! Only the canonical `BLOG.hatenablog.com`, `.hatenablog.jp`,
//! `.hatenadiary.com` and `.hateblo.jp` hosts are supported; the
//! `hatenablog:https://custom.domain/...` prefix form is not.
use super::{Scan, extr};
use crate::sites::{Events, Http, Item, Route, Site, unescape};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

pub static SITE: Site = Site {
    category: "hatenablog",
    name: "HatenaBlog",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&ENTRY, &HOME, &ARCHIVE, &SEARCH],
    api_key: false,
};

const FILENAME: &str = "{category}_{domain}_{entry}_{num:>02}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{domain}"];
static ENTRY: Route = Route::new("entry", FILENAME, DIRECTORY, "{filename}");
static HOME: Route = Route::new("home", FILENAME, DIRECTORY, "{filename}");
static ARCHIVE: Route = Route::new("archive", FILENAME, DIRECTORY, "{filename}");
static SEARCH: Route = Route::new("search", FILENAME, DIRECTORY, "{filename}");

const DOMAINS: &[&str] = &[
    "hatenablog.com",
    "hatenablog.jp",
    "hatenadiary.com",
    "hateblo.jp",
];

#[derive(Debug, PartialEq)]
enum Target {
    Entry {
        domain: String,
        path: String,
    },
    /// Home, archive and search pages: route, path and accepted query.
    Entries {
        route: &'static str,
        domain: String,
        path: String,
        query: Vec<(String, String)>,
    },
}

fn domain(url: &Url) -> Option<String> {
    DOMAINS
        .iter()
        .find(|d| super::subdomain(url, d).is_some())
        .map(|_| url.host_str().unwrap_or_default().to_owned())
}

fn archive_path(path: &str) -> bool {
    let Some(rest) = path.strip_prefix("/archive") else {
        return false;
    };
    if rest.is_empty() {
        return true;
    }
    if let Some(category) = rest.strip_prefix("/category/") {
        return !category.is_empty();
    }
    let parts: Vec<&str> = rest[1..].split('/').collect();
    rest.starts_with('/')
        && parts.len() <= 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

fn target(url: &Url) -> Option<Target> {
    let domain = domain(url)?;
    let path = url.path();
    if let Some(entry) = path.strip_prefix("/entry/") {
        return (!entry.is_empty()).then(|| Target::Entry {
            domain,
            path: entry.to_owned(),
        });
    }
    let (route, allowed): (&str, &[&str]) = if path == "/" || path.is_empty() {
        ("home", &[])
    } else if archive_path(path) {
        ("archive", &[])
    } else if path == "/search" {
        ("search", &["q"])
    } else {
        return None;
    };
    let query = url
        .query_pairs()
        .filter(|(k, _)| k == "page" || allowed.contains(&k.as_ref()))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    Some(Target::Entries {
        route,
        domain,
        path: if route == "home" {
            String::new()
        } else {
            path.to_owned()
        },
        query,
    })
}

fn route(name: &str) -> &'static Route {
    match name {
        "home" => &HOME,
        "archive" => &ARCHIVE,
        "search" => &SEARCH,
        _ => &ENTRY,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Entry { domain, path }) => crate::sites::once(async move {
            let url = http.api(&format!("https://{domain}/entry/{path}"))?;
            let page = http.text(&url).await?;
            let mut scan = Scan::new(&page);
            let mut events = Vec::new();
            while let Some(attributes) = scan.find("<article ", ">") {
                if attributes.contains("no-entry") {
                    continue;
                }
                let article = scan.extr("", "</article>");
                events = crate::sites::post(&SITE, http, &ENTRY, article_items(&domain, article));
                break;
            }
            Ok(crate::sites::events(events))
        }),
        Some(Target::Entries {
            route: name,
            domain,
            path,
            query,
        }) => entries(http, route(name), domain, path, query),
        None => crate::sites::fail(error("unsupported HatenaBlog URL")),
    }
}

fn entries<'a>(
    http: &'a Http,
    route: &'static Route,
    domain: String,
    path: String,
    query: Vec<(String, String)>,
) -> Events<'a> {
    let first = format!("https://{domain}{path}");
    crate::sites::paginate(Some((first, query)), move |state| {
        let domain = domain.clone();
        async move {
            let Some((page_url, query)) = state else {
                return Ok(None);
            };
            let mut url = http.api(&page_url)?;
            if !query.is_empty() {
                url.query_pairs_mut().extend_pairs(&query);
            }
            let page = http.text(&url).await?;
            let events = page_events(http, route, &domain, &page)?;
            let next = next_page(&page, &page_url, &domain).map(|url| (url, Vec::new()));
            Ok(Some((events, next)))
        }
    })
}

fn page_events<'a>(
    http: &'a Http,
    route: &'static Route,
    domain: &str,
    page: &str,
) -> Result<Events<'a>> {
    let mut scan = Scan::new(page);
    let attributes = scan.extr("<body ", ">");
    if attributes.contains("page-archive") {
        let mut children = Vec::new();
        while let Some(section) = scan.find("<section class=\"archive-entry", "</section>") {
            if section.is_empty() {
                break;
            }
            let link = unescape(extr(
                section,
                "<a class=\"entry-title-link\" href=\"",
                "\"",
            ));
            // Upstream queues `hatenablog:URL`; only canonical hosts are
            // dispatchable here.
            if let Ok(child) = Url::parse(&link)
                && matches!(target(&child), Some(Target::Entry { .. }))
            {
                children.push(child);
            }
        }
        let subcategory = route.subcategory;
        return Ok(futures_util::StreamExt::boxed(
            futures_util::StreamExt::flatten(futures_util::stream::iter(
                children.into_iter().map(move |child| {
                    crate::sites::queue(http, "hatenablog", subcategory, child, &Map::new())
                }),
            )),
        ));
    }
    let mut events = Vec::new();
    while let Some(attributes) = scan.find("<article ", ">") {
        if attributes.contains("no-entry") {
            continue;
        }
        let article = scan.extr("", "</article>");
        events.extend(crate::sites::post(
            &SITE,
            http,
            route,
            article_items(domain, article),
        ));
    }
    Ok(crate::sites::events(events))
}

/// Upstream `' class="pager-next">\s*<a href="([^"]+)'`, on the same blog.
fn next_page(page: &str, base: &str, domain: &str) -> Option<String> {
    let marker = " class=\"pager-next\">";
    let mut rest = page;
    while let Some(start) = rest.find(marker) {
        rest = &rest[start + marker.len()..];
        let after = rest.trim_start();
        if let Some(link) = after.strip_prefix("<a href=\"")
            && let Some(end) = link.find('"')
            && end > 0
        {
            let url = Url::parse(base).ok()?.join(&unescape(&link[..end])).ok()?;
            return (crate::sites::plain(&url) && url.host_str() == Some(domain))
                .then(|| url.to_string());
        }
    }
    None
}

/// Upstream `<img +([^>]+)` attribute strings.
fn img_attributes(content: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = content;
    while let Some(start) = rest.find("<img") {
        rest = &rest[start + 4..];
        if !rest.starts_with(' ') {
            continue;
        }
        let attrs = rest.trim_start_matches(' ');
        let end = attrs.find('>').unwrap_or(attrs.len());
        if end > 0 {
            out.push(&attrs[..end]);
        }
    }
    out
}

fn article_items(domain: &str, article: &str) -> Vec<Item> {
    let mut scan = Scan::new(article);
    let timestamp = super::parse_iso(scan.extr("<time datetime=\"", "\""));
    let entry_link = unescape(scan.extr("<a href=\"", "\""));
    let entry = entry_link
        .split_once("/entry/")
        .map(|(_, e)| e.to_owned())
        .unwrap_or_default();
    let title = unescape(scan.extr(">", "<"));
    let content = scan.extr("<div class=\"entry-content hatenablog-entry\">", "</div>");
    let images: Vec<String> = img_attributes(content)
        .into_iter()
        .filter(|attrs| attrs.contains("class=\"hatena-fotolife\""))
        .map(|attrs| unescape(extr(attrs, "src=\"", "\"")))
        .collect();
    let mut data = Map::new();
    data.insert("domain".into(), domain.into());
    data.insert("date".into(), super::date(timestamp));
    data.insert("entry".into(), entry.clone().into());
    data.insert("title".into(), title.clone().into());
    data.insert("count".into(), images.len().into());
    images
        .iter()
        .enumerate()
        .filter_map(|(index, raw)| {
            let url = super::file_url(raw, &format!("https://{domain}/"))?;
            let mut fields = data.clone();
            fields.insert("num".into(), Value::from(index + 1));
            super::nameext_into(raw, &mut fields);
            let media_id = fields["filename"].as_str().unwrap_or_default().to_owned();
            Some(
                Item::new(url, fields)
                    .ids(entry.clone(), media_id)
                    .user(Some(domain.to_owned()))
                    .caption(Some(title.clone()))
                    .timestamp(timestamp),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, serve};

    fn t(raw: &str) -> Option<Target> {
        target(&Url::parse(raw).unwrap())
    }

    #[test]
    fn classifies_urls() {
        assert_eq!(
            t("https://cosmiclatte.hatenablog.com/entry/2020/05/28/003227"),
            Some(Target::Entry {
                domain: "cosmiclatte.hatenablog.com".into(),
                path: "2020/05/28/003227".into()
            })
        );
        assert_eq!(
            t("https://moko0908.hatenablog.jp/?page=1&x=2"),
            Some(Target::Entries {
                route: "home",
                domain: "moko0908.hatenablog.jp".into(),
                path: String::new(),
                query: vec![("page".into(), "1".into())]
            })
        );
        for raw in [
            "https://x.hateblo.jp/archive",
            "https://x.hatenadiary.com/archive/2024/01/02",
            "https://x.hatenablog.com/archive/category/A%20B",
        ] {
            assert!(
                matches!(t(raw), Some(Target::Entries { route: "archive", .. })),
                "{raw}"
            );
        }
        assert_eq!(
            t("https://x.hatenablog.com/search?q=a&page=2&y=1"),
            Some(Target::Entries {
                route: "search",
                domain: "x.hatenablog.com".into(),
                path: "/search".into(),
                query: vec![("q".into(), "a".into()), ("page".into(), "2".into())]
            })
        );
        for raw in [
            "https://hatenablog.com/entry/1",
            "https://a.b.hatenablog.com/entry/1",
            "https://x.hatenablog.com.evil.com/entry/1",
            "https://evilhatenablog.com/entry/1",
            "https://x.hatenablog.com:8080/entry/1",
            "https://u:p@x.hatenablog.com/entry/1",
            "https://x.hatenablog.com/entry/",
            "https://x.hatenablog.com/archive/2024/01/02/03",
            "https://x.hatenablog.com/archive/abc",
            "https://x.hatenablog.com/about",
            "ftp://x.hatenablog.com/",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    const ARTICLE: &str = r#"<article class="entry">
<time datetime="2020-05-28T00:32:27+09:00">
<h1><a href="https://x.hatenablog.com/entry/2020/05/28/003227">Title &amp; more</a></h1>
<div class="entry-content hatenablog-entry">
<img src="https://cdn.test/skip.png" alt="">
<img  class="hatena-fotolife" src="https://cdn-ak.f.st-hatena.com/images/fotolife/x/x/20200528/20200528003227.png?a&amp;b" >
<img class="hatena-fotolife" title="t" src="https://cdn-ak.f.st-hatena.com/images/fotolife/x/x/20200528/second.JPG">
</div></article>"#;

    #[tokio::test]
    async fn extracts_entry() {
        let page = format!(
            "<html><article class=\"no-entry\"></article>{ARTICLE}<article>other</article>"
        );
        let (root, server) = serve(vec![html(&page)]);
        let http = http("hatenablog", root);
        let files = media(extract(
            &http,
            Url::parse("https://x.hatenablog.com/entry/2020/05/28/003227?x=1").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].url.as_str(),
            "https://cdn-ak.f.st-hatena.com/images/fotolife/x/x/20200528/20200528003227.png?a&b"
        );
        let k = keywords(&files[1]);
        assert_eq!(k["domain"], "x.hatenablog.com");
        assert_eq!(k["entry"], "2020/05/28/003227");
        assert_eq!(k["title"], "Title & more");
        assert_eq!(k["count"], 2);
        assert_eq!(k["num"], 2);
        assert_eq!(k["date"], "2020-05-27 15:32:27");
        assert_eq!(k["filename"], "second");
        assert_eq!(k["extension"], "jpg");
        assert_eq!(k["subcategory"], "entry");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            file.render(&files[0]).unwrap(),
            "hatenablog_x.hatenablog.com_2020_05_28_003227_01.png"
        );
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("hatenablog/x.hatenablog.com")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{filename}",["20200528003227"]]"#);
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with("GET /entry/2020/05/28/003227 "),
            "{}",
            requests[0]
        );
    }

    #[tokio::test]
    async fn paginates_home_and_queues_archive_entries() {
        let first = format!(
            "<body class=\"page-index\">{ARTICLE}<div class=\"pager\"><span class=\"pager-next\">\n  <a href=\"https://x.hatenablog.com/?page=1590000000\">next</a>"
        );
        let second = "<body class=\"page-archive\"><section class=\"archive-entry x\">\
            <a class=\"entry-title-link\" href=\"https://x.hatenablog.com/entry/2020/05/28/003227\">t</a></section>\
            <section class=\"archive-entry x\"><a class=\"entry-title-link\" href=\"https://custom.example/entry/1\">t</a></section>";
        let entry = format!("<body>{ARTICLE}");
        let (root, server) = serve(vec![html(&first), html(second), html(&entry)]);
        let http = http("hatenablog", root);
        let files = media(extract(
            &http,
            Url::parse("https://x.hatenablog.com/?page=5").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 4);
        assert_eq!(keywords(&files[0])["subcategory"], "home");
        assert_eq!(keywords(&files[2])["subcategory"], "entry");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /?page=5 "), "{}", requests[0]);
        assert!(requests[1].starts_with("GET /?page=1590000000 "));
        assert!(requests[2].starts_with("GET /entry/2020/05/28/003227 "));
    }

    #[test]
    fn helpers() {
        assert_eq!(img_attributes("<img a><imgx b><img  c>"), ["a", "c"]);
        assert_eq!(
            next_page(
                " class=\"pager-next\">x</a> class=\"pager-next\"> <a href=\"/?page=2\">",
                "https://x.hatenablog.com/",
                "x.hatenablog.com"
            ),
            Some("https://x.hatenablog.com/?page=2".into())
        );
        assert_eq!(
            next_page(
                " class=\"pager-next\"><a href=\"https://evil.test/\">",
                "https://x.hatenablog.com/",
                "x.hatenablog.com"
            ),
            None
        );
    }
}
