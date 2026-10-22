//! Adobe Portfolio galleries, mirroring gallery_dl/extractor/myportfolio.py:
//! a project page lists its images, while a portfolio index queues each
//! project cover as a gallery child.
use crate::sites::art::{Scan, extr, extract_iter, nameext_into};
use crate::sites::{Events, Http, Item, Route, Site, unescape};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

pub static SITE: Site = Site {
    category: "myportfolio",
    name: "Adobe Portfolio",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&GALLERY],
    api_key: false,
};

static GALLERY: Route = Route::new(
    "gallery",
    "{num:>02}.{extension}",
    &["{category}", "{user}", "{title}"],
    "{user}_{filename}",
);

#[derive(Debug, PartialEq)]
struct Target {
    /// `myportfolio:` for custom domains, else empty.
    prefix: &'static str,
    domain: String,
    /// The first path segment with its leading slash, or empty.
    path: String,
}

/// The upstream `(/[^/?#]+)?` group of a raw path.
fn first_segment(path: &str) -> String {
    match path.strip_prefix('/') {
        Some(rest) => {
            let end = rest.find('/').unwrap_or(rest.len());
            if end == 0 {
                String::new()
            } else {
                format!("/{}", &rest[..end])
            }
        }
        None => String::new(),
    }
}

fn target(url: &Url) -> Option<Target> {
    if url.scheme() == "myportfolio" {
        // `myportfolio:(?:https?://)?([^/]+)`: an explicit custom domain.
        let rest = url.as_str().strip_prefix("myportfolio:")?;
        let rest = rest
            .strip_prefix("https://")
            .or_else(|| rest.strip_prefix("http://"))
            .unwrap_or(rest);
        let inner = Url::parse(&format!("https://{rest}")).ok()?;
        if !crate::sites::plain(&inner) {
            return None;
        }
        let domain = inner.host_str()?.to_owned();
        return Some(Target {
            prefix: "myportfolio:",
            domain,
            path: first_segment(inner.path()),
        });
    }
    let user = crate::sites::art::subdomain(url, "myportfolio.com")?;
    if user == "cdn" {
        return None;
    }
    Some(Target {
        prefix: "",
        domain: url.host_str()?.to_owned(),
        path: first_segment(url.path()),
    })
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Adobe Portfolio URL"));
    };
    crate::sites::once(async move {
        let page_url = http.api(&format!("https://{}{}", target.domain, target.path))?;
        let page = http.text(&page_url).await?;
        let projects = extr(&page, "<section class=\"project-covers", "</section>");
        if !projects.is_empty() {
            let base = format!("{}https://{}", target.prefix, target.domain);
            let children: Vec<Url> = extract_iter(projects, " href=\"", "\"")
                .into_iter()
                .filter_map(|path| Url::parse(&format!("{base}{path}")).ok())
                .filter(|child| self::target(child).is_some())
                .collect();
            let data = Map::new();
            return Ok(futures_util::StreamExt::boxed(
                futures_util::StreamExt::flatten(futures_util::stream::iter(
                    children.into_iter().map(move |child| {
                        crate::sites::queue(http, "myportfolio", "gallery", child, &data)
                    }),
                )),
            ));
        }
        let base = format!("https://{}/", target.domain);
        let items = gallery(&page, &base)?;
        Ok(crate::sites::events(crate::sites::post(
            &SITE, http, &GALLERY, items,
        )))
    })
}

/// Upstream `metadata()`: user and title from `og:title` and `<h1>`.
fn metadata(page: &str) -> Result<Map<String, Value>> {
    let mut scan = Scan::new(page);
    let mut user = scan
        .extr("property=\"og:title\" content=\"", "\"")
        .to_owned();
    if user.is_empty() {
        user = scan.extr("property=og:title content=\"", "\"").to_owned();
    }
    let mut descr = scan.extr("property=\"og:description\" content=\"", "\"");
    if descr.is_empty() {
        descr = scan.extr("property=og:description content=\"", "\"");
    }
    let title = scan.extr("<h1 ", "</h1>");
    let title = if !title.is_empty() {
        let title = title.split_once('>').map_or("", |(_, t)| t).to_owned();
        // Python `user[:-len(title)-3]` with character counts.
        let keep = user
            .chars()
            .count()
            .saturating_sub(title.chars().count() + 3);
        user = user.chars().take(keep).collect();
        title
    } else if !user.is_empty() {
        let (u, t) = match user.split_once(" - ") {
            Some((u, t)) => (u.to_owned(), t.to_owned()),
            None => (user.clone(), String::new()),
        };
        user = u;
        t
    } else {
        return Err(error("Adobe Portfolio gallery not found"));
    };
    let mut data = Map::new();
    data.insert("user".into(), unescape(&user).into());
    data.insert("title".into(), unescape(&title).into());
    data.insert("description".into(), unescape(descr).into());
    Ok(data)
}

fn images(page: &str) -> Vec<&str> {
    let lightbox = extract_iter(page, "js-lightbox\" data-src=\"", "\"");
    if lightbox.is_empty() {
        extract_iter(page, "data-src=\"", "\"")
    } else {
        lightbox
    }
}

fn gallery(page: &str, base: &str) -> Result<Vec<Item>> {
    let mut data = metadata(page)?;
    let images = images(page);
    data.insert("count".into(), images.len().into());
    let user = data["user"].as_str().map(str::to_owned);
    let title = data["title"].as_str().map(str::to_owned);
    let mut items = Vec::new();
    for (index, raw) in images.into_iter().enumerate() {
        let Some(url) = crate::sites::art::file_url(raw, base) else {
            continue;
        };
        let mut fields = data.clone();
        fields.insert("num".into(), (index + 1).into());
        nameext_into(raw, &mut fields);
        let name = fields["filename"].as_str().unwrap_or_default().to_owned();
        items.push(
            Item::new(url, fields)
                .ids(title.clone().unwrap_or_default(), name)
                .user(user.clone())
                .caption(title.clone()),
        );
    }
    Ok(items)
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
            t("https://andrewling.myportfolio.com/volvo-xc-90-hybrid/x"),
            Some(Target {
                prefix: "",
                domain: "andrewling.myportfolio.com".into(),
                path: "/volvo-xc-90-hybrid".into(),
            })
        );
        assert_eq!(t("https://andrewling.myportfolio.com/").unwrap().path, "");
        let custom = t("myportfolio:https://tooco.com.ar/6-of-diamonds").unwrap();
        assert_eq!(custom.prefix, "myportfolio:");
        assert_eq!(custom.domain, "tooco.com.ar");
        assert_eq!(custom.path, "/6-of-diamonds");
        assert_eq!(t("myportfolio:tooco.com.ar").unwrap().path, "");
        for raw in [
            "https://cdn.myportfolio.com/f33/74.jpg",
            "https://a.b.myportfolio.com/x",
            "https://evilmyportfolio.com/x",
            "https://x.myportfolio.com.evil.com/x",
            "https://x.myportfolio.com:8080/x",
            "https://u:p@x.myportfolio.com/x",
            "myportfolio:https://u:p@tooco.com.ar/x",
            "https://myportfolio.com/x",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
        assert!(
            crate::sites::find(&Url::parse("https://abc.myportfolio.com/x").unwrap())
                .is_some_and(|s| s.category == "myportfolio")
        );
    }

    const GALLERY_PAGE: &str = r#"<meta property="og:title" content="Andrew Ling - Volvo - XC &amp; 90" />
<meta property="og:description" content="A &amp; B" />
<h1 class="title preserve-whitespace">Volvo - XC &amp; 90</h1>
<div class="js-lightbox" data-src="https://cdn.myportfolio.com/abc/one.jpg?h=1"></div>
<div class="js-lightbox" data-src="https://cdn.myportfolio.com/abc/two.png?h=2"></div>"#;

    #[test]
    fn parses_metadata() {
        let data = metadata(GALLERY_PAGE).unwrap();
        assert_eq!(data["user"], "Andrew Ling");
        assert_eq!(data["title"], "Volvo - XC & 90");
        assert_eq!(data["description"], "A & B");
        let data = metadata(r#"<meta property=og:title content="U - T">"#).unwrap();
        assert_eq!((&data["user"], &data["title"]), (&"U".into(), &"T".into()));
        assert!(metadata("<html></html>").is_err());
        assert_eq!(
            images(r#"<img data-src="a.jpg"><img data-src="b.jpg">"#).len(),
            2
        );
    }

    #[tokio::test]
    async fn extracts_gallery() {
        let (root, server) = serve(vec![html(GALLERY_PAGE)]);
        let http = http("myportfolio", root);
        let files = media(extract(
            &http,
            Url::parse("https://andrewling.myportfolio.com/volvo-xc-90-hybrid").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[1]);
        assert_eq!(k["num"], 2);
        assert_eq!(k["count"], 2);
        assert_eq!(k["filename"], "two");
        assert_eq!(k["extension"], "png");
        assert_eq!(
            files[1].url.as_str(),
            "https://cdn.myportfolio.com/abc/two.png?h=2"
        );
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "02.png");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("myportfolio/Andrew Ling/Volvo - XC & 90")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{user}_{filename}",["Andrew Ling","one"]]"#);
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with("GET /volvo-xc-90-hybrid "),
            "{}",
            requests[0]
        );
    }

    #[tokio::test]
    async fn queues_project_covers() {
        let index = r#"<section class="project-covers"><a href="/one" class="x"></a><a href="/two"></a></section>"#;
        let (root, server) = serve(vec![html(index), html(GALLERY_PAGE), html(GALLERY_PAGE)]);
        let http = http("myportfolio", root);
        let files = media(extract(
            &http,
            Url::parse("https://andrewling.myportfolio.com/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 4);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET / "));
        assert!(requests[1].starts_with("GET /one "));
        assert!(requests[2].starts_with("GET /two "));
    }
}
