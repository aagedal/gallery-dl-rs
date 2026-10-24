//! vanilla-rock.com blog posts and tag or category listings, mirroring
//! gallery_dl/extractor/vanillarock.py: a post links each full-size image
//! from its `main-img` blocks; listings follow `next page-numbers`.
use crate::error;
use crate::sites::art::{Scan, date, extr, nameext_into, on_host, parse_iso, split_html};
use crate::sites::{Events, Http, Item, Route, Site, unescape};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://vanilla-rock.com";
static POST: Route = Route::new(
    "post",
    "{num:>02}.{extension}",
    &["{category}", "{path}"],
    "{filename}",
);
static TAG: Route = Route::new("tag", "{filename}.{extension}", &["{category}"], "");

pub static SITE: Site = Site {
    category: "vanillarock",
    name: "vanilla-rock",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&POST, &TAG],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    /// `/SLUG`, raw.
    Post(String),
    /// `/tag/…` or `/category/…`, raw.
    Tag(String),
}

/// `(/(?!category/|tag/)[^/?#]+)/?$` and `(/(?:tag|category)/[^?#]+)`.
fn target(url: &Url) -> Option<Target> {
    if !on_host(url, &["vanilla-rock.com"]) {
        return None;
    }
    let path = url.path();
    let bare = url.query().is_none() && url.fragment().is_none();
    let slug = path[1..].strip_suffix('/').unwrap_or(&path[1..]);
    let excluded = path.starts_with("/category/") || path.starts_with("/tag/");
    if bare && !slug.is_empty() && !slug.contains('/') && !excluded {
        return Some(Target::Post(format!("/{slug}")));
    }
    ["/tag/", "/category/"]
        .iter()
        .any(|prefix| path.len() > prefix.len() && path.starts_with(prefix))
        .then(|| Target::Tag(path.to_owned()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Post(path)) => crate::sites::once(async move {
            let page = super::page(http, &format!("{ROOT}{path}"), &[]).await?;
            let items = post_items(&page, &path);
            Ok(crate::sites::events(crate::sites::post(
                &SITE, http, &POST, items,
            )))
        }),
        Some(Target::Tag(path)) => tag(http, format!("{ROOT}{path}")),
        None => crate::sites::fail(error("unsupported vanilla-rock URL")),
    }
}

/// `VanillarockPostExtractor.items`.
fn post_items(page: &str, path: &str) -> Vec<Item> {
    let mut e = Scan::new(page);
    let name = e.extr("<h1 class=\"entry-title\">", "<");
    let mut images = Vec::new();
    while let Some(img) = e
        .find("<div class=\"main-img\">", "</div>")
        .filter(|i| !i.is_empty())
    {
        images.push(extr(img, "href=\"", "\""));
    }
    let mut data = Map::new();
    data.insert("count".into(), images.len().into());
    data.insert("title".into(), unescape(name).into());
    data.insert("path".into(), path.trim_matches('/').into());
    data.insert(
        "date".into(),
        date(parse_iso(e.extr("<div class=\"date\">", "</div>"))),
    );
    let tags: Vec<String> = split_html(e.extr("<div class=\"cat-tag\">", "</div>"))
        .into_iter()
        .step_by(2)
        .collect();
    data.insert("tags".into(), tags.into());
    let title = data["title"].as_str().map(str::to_owned);
    images
        .into_iter()
        .enumerate()
        .filter_map(|(index, raw)| {
            let url = crate::sites::art::file_url(raw, ROOT)?;
            let mut fields = data.clone();
            fields.insert("num".into(), (index + 1).into());
            nameext_into(raw, &mut fields);
            Some(
                Item::new(url, fields)
                    .ids(path.trim_matches('/'), (index + 1).to_string())
                    .caption(title.clone()),
            )
        })
        .collect()
}

/// `VanillarockTagExtractor.items`: each listed post, following the next
/// page link.
fn tag(http: &Http, url: String) -> Events<'_> {
    crate::sites::paginate(Some((url, 0)), move |state| async move {
        let Some((url, count)) = state else {
            return Ok(None);
        };
        let page = super::page(http, &url, &[]).await?;
        let mut e = Scan::new(&page);
        let mut children = Vec::new();
        while let Some(post) = e
            .find("<h2 class=\"entry-title\">", "</h2>")
            .filter(|p| !p.is_empty())
        {
            if let Ok(url) = Url::parse(extr(post, "href=\"", "\"")) {
                children.push((url, Map::<String, Value>::new()));
            }
        }
        let next = unescape(e.extr("class=\"next page-numbers\" href=\"", "\""));
        let next = Url::parse(&next)
            .ok()
            .filter(|next| on_host(next, &["vanilla-rock.com"]))
            .filter(|_| count + 1 < super::MAX_PAGES)
            .map(|_| (next, count + 1));
        Ok(Some((
            crate::sites::manga::queue_urls(http, &SITE, &TAG, children),
            next,
        )))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::manga::tests::{archive, path};
    use crate::sites::testing::{html, http, keywords, media, queued, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://vanilla-rock.com/mizuhashi_parsee-5"),
            Some(Target::Post("/mizuhashi_parsee-5".into()))
        );
        assert_eq!(
            t("https://www.vanilla-rock.com/post/"),
            Some(Target::Post("/post".into()))
        );
        assert_eq!(
            t("https://vanilla-rock.com/tag/%E7%8B%90/page/2"),
            Some(Target::Tag("/tag/%E7%8B%90/page/2".into()))
        );
        assert_eq!(
            t("https://vanilla-rock.com/category/x?y"),
            Some(Target::Tag("/category/x".into()))
        );
        for raw in [
            "https://vanilla-rock.com/",
            "https://vanilla-rock.com/post?x=1",
            "https://vanilla-rock.com/a/b",
            "https://vanilla-rock.com/tag/",
            "https://m.vanilla-rock.com/post",
            "https://vanilla-rock.com.evil.test/post",
            "https://vanilla-rock.com:8443/post",
            "https://u:p@vanilla-rock.com/post",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[tokio::test]
    async fn extracts_post() {
        let page = r#"<h1 class="entry-title">Spring &amp; Snow</h1>
<div class="main-img"><a href="https://vanilla-rock.com/wp-content/uploads/a.jpg"><img></a></div>
<div class="main-img"><a href="https://vanilla-rock.com/wp-content/uploads/b.png"><img></a></div>
<div class="date">2019-03-04T05:06:07+09:00</div>
<div class="cat-tag"><a>Original</a>, <a>Scenery</a></div>"#;
        let (root, server) = serve(vec![html(page)]);
        let http = http("vanillarock", root);
        let files = media(extract(
            &http,
            Url::parse("https://vanilla-rock.com/spring-snow/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[1]);
        assert_eq!(k["title"], "Spring & Snow");
        assert_eq!(k["path"], "spring-snow");
        assert_eq!(k["count"], 2);
        assert_eq!(k["date"], "2019-03-03 20:06:07");
        assert_eq!(k["tags"], json!(["Original", "Scenery"]));
        assert_eq!(path(&files[1]), "vanillarock/spring-snow/02.png");
        assert_eq!(archive(&files[1]), r#"["{filename}",["b"]]"#);
        assert!(server.join().unwrap()[0].starts_with("GET /spring-snow "));
    }

    #[tokio::test]
    async fn tag_follows_next_pages() {
        let first = r#"<h2 class="entry-title"><a href="https://vanilla-rock.com/a">a</a></h2>
<h2 class="entry-title"><a href="https://vanilla-rock.com/b">b</a></h2>
<a class="next page-numbers" href="https://vanilla-rock.com/tag/x/page/2?a=1&amp;b=2">"#;
        let second = r#"<h2 class="entry-title"><a href="https://vanilla-rock.com/c">c</a></h2>"#;
        let (root, server) = serve(vec![html(first), html(second)]);
        let http = http("vanillarock", root);
        let children = queued(extract(
            &http,
            Url::parse("https://vanilla-rock.com/tag/x").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 3);
        assert_eq!(children[0].subcategory(), "tag");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /tag/x "));
        assert!(requests[1].starts_with("GET /tag/x/page/2?a=1&b=2 "));
    }
}
