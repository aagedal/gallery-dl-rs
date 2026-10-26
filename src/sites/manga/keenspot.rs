//! Keenspot webcomics, mirroring gallery_dl/extractor/keenspot.py: the
//! first strip is found from the front page layout, then each page links
//! to the next.
use super::set;
use crate::error;
use crate::extract::MediaJob;
use crate::sites::gdl::{Scan, captures, extr, group, nameext_into, re, rextr};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::Map;
use url::Url;

static COMIC: Route = Route::new(
    "comic",
    "{filename}.{extension}",
    &["{category}", "{comic}"],
    "{comic}_{filename}",
);

pub static SITE: Site = Site {
    category: "keenspot",
    name: "Keenspot",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&COMIC],
    api_key: false,
    cookies: None,
};

/// `(?!www\.|forums\.)([\w-]+)\.keenspot\.com(/.+)?`
fn target(url: &Url) -> Option<(String, Option<String>)> {
    let pattern = re!(r"^https?://([\w-]+)\.keenspot\.com(/.+)?");
    let caps = captures(pattern, url)?;
    let comic = group(&caps, 1);
    if matches!(comic, "www" | "forums") {
        return None;
    }
    Some((
        comic.to_lowercase(),
        caps.get(2).map(|m| m.as_str().to_owned()),
    ))
}

/// How the next page is found.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Next {
    Needle,
    Link,
    Id,
    Lastblood,
    Brawl,
}

#[derive(Clone, Debug)]
struct Layout {
    next: Next,
    image: &'static str,
    needle: &'static str,
}

/// `_first`: the first strip's URL and how to walk from it.
fn first(comic: &str, page: &str) -> (Option<String>, Layout) {
    let mut layout = Layout {
        next: Next::Needle,
        image: "class=\"ksc\"",
        needle: "",
    };
    if comic == "brawlinthefamily" {
        layout.next = Next::Brawl;
        layout.image = "<div id=\"comic\">";
        return (
            Some("http://brawlinthefamily.keenspot.com/comic/theshowdown/".into()),
            layout,
        );
    }
    let url = extr(page, "<link rel=\"first\" href=\"", "\"");
    if !url.is_empty() {
        if comic == "porcelain" {
            layout.needle = "id=\"porArchivetop_\"";
        } else {
            layout.next = Next::Link;
        }
        return (Some(url.into()), layout);
    }
    let rfind = |pos: usize| Some(rextr(page, "href=\"", "\"", Some(pos)).to_owned());
    if let Some(pos) = page.find("id=\"first_day1\"") {
        layout.next = Next::Id;
        return (rfind(pos), layout);
    }
    if let Some(pos) = page.find(">FIRST PAGE<") {
        if comic == "lastblood" {
            layout.next = Next::Lastblood;
            layout.image = "<div id=\"comic\">";
        } else {
            layout.next = Next::Id;
        }
        return (rfind(pos), layout);
    }
    if let Some(pos) = page.find("<div id=\"kscomicpart\"") {
        layout.needle = "<a href=\"/archive.html";
        return (
            Some(Scan::at(page, pos).extr("href=\"", "\"").to_owned()),
            layout,
        );
    }
    if let Some(pos) = page.find(">First Comic<") {
        layout.image = "</header>";
        layout.needle = "class=\"navarchive\"";
        return (rfind(pos), layout);
    }
    if let Some(pos) = page.find("id=\"flip_FirstDay\"") {
        layout.image = "class=\"flip_Pages ksc\"";
        layout.needle = "id=\"flip_ArcButton\"";
        return (rfind(pos), layout);
    }
    (None, layout)
}

fn next(page: &str, layout: &Layout) -> Option<String> {
    let value = match layout.next {
        Next::Needle => {
            let pos = page.find(layout.needle)? + layout.needle.len();
            Scan::at(page, pos).find("href=\"", "\"")?.to_owned()
        }
        Next::Link => extr(page, "<link rel=\"next\" href=\"", "\"").to_owned(),
        Next::Id => {
            let pos = page.find("id=\"next_")?;
            rextr(page, "href=\"", "\"", Some(pos)).to_owned()
        }
        Next::Lastblood => {
            let pos = page.find("link rel='next'")?;
            Scan::at(page, pos).find("href='", "'")?.to_owned()
        }
        Next::Brawl => {
            let pos = page.find("comic-nav-next")?;
            let url = rextr(page, "href=\"", "\"", Some(pos));
            if url.contains("?random") {
                return None;
            }
            url.to_owned()
        }
    };
    (!value.is_empty()).then_some(value)
}

/// The strip images of one page.
fn images(page: &str, root: &str, layout: &Layout, comic: &str) -> Vec<Item> {
    let mut items = Vec::new();
    let mut pos = 0;
    while let Some(found) = page[pos..].find(layout.image) {
        let mut scan = Scan::at(page, pos + found + layout.image.len());
        let Some(img) = scan.find("src=\"", "\"") else {
            break;
        };
        pos = scan.pos();
        if img.ends_with(".js") || img.is_empty() {
            continue;
        }
        let (raw, job) = if img.starts_with('/') {
            (format!("{root}{img}"), None)
        } else if img.contains("youtube.com/") {
            (
                format!("ytdl:{img}"),
                Url::parse(img)
                    .ok()
                    .map(|page_url| MediaJob::VideoPage { page_url }),
            )
        } else {
            (img.to_owned(), None)
        };
        let Ok(url) = Url::parse(&raw) else { continue };
        let mut data = Map::new();
        set(&mut data, "comic", comic);
        nameext_into(&raw, &mut data);
        let mut item = Item::new(url, data);
        if let Some(job) = job {
            item = item.job(job);
        }
        items.push(item);
    }
    items
}

struct State {
    comic: String,
    root: String,
    layout: Layout,
    url: Option<String>,
    prev: Option<String>,
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some((comic, path)) = target(&url) else {
        return crate::sites::fail(error("unsupported Keenspot URL"));
    };
    crate::sites::once(async move {
        let mut root = format!("http://{comic}.keenspot.com");
        let front = http.api(&format!("{root}/"))?;
        // Upstream takes the root from the final URL after redirects.
        if let Some(moved) = http.location(&front, &[]).await? {
            root = crate::sites::gdl::root_of(&moved);
        }
        let page = super::fetch(http, &format!("{root}/")).await?;
        let (first, layout) = first(&comic, &page);
        let first = first.ok_or_else(|| error("Keenspot page layout is not recognized"))?;
        let url = match path {
            Some(path) => format!("{root}{path}"),
            None => first,
        };
        let state = State {
            comic,
            root,
            layout,
            url: Some(url),
            prev: None,
        };
        Ok(crate::sites::paginate(state, move |mut state| async move {
            let Some(url) = state.url.take().filter(|u| state.prev.as_ref() != Some(u)) else {
                return Ok(None);
            };
            let page_url = Url::parse(&format!("{}/", state.root))
                .and_then(|base| base.join(&url))
                .map_err(|_| error("invalid Keenspot page URL"))?;
            let page = http.text(&http.api(page_url.as_str())?).await?;
            let items = images(&page, &state.root, &state.layout, &state.comic);
            let events = crate::sites::post(&SITE, http, &COMIC, items);
            state.url = next(&page, &state.layout);
            state.prev = Some(url);
            Ok(Some((crate::sites::events(events), state)))
        }))
    })
}

#[cfg(test)]
mod tests {
    use super::super::tests::{archive, path};
    use super::*;
    use crate::sites::testing::{html, http, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("http://marksmen.keenspot.com/"),
            Some(("marksmen".into(), None))
        );
        assert_eq!(
            t("http://TwoKinds.keenspot.com/comic/1066/"),
            Some(("twokinds".into(), Some("/comic/1066/".into())))
        );
        assert_eq!(
            t("http://crowscare.keenspot.com"),
            Some(("crowscare".into(), None))
        );
        assert_eq!(t("http://www.keenspot.com/"), None);
        assert_eq!(t("http://forums.keenspot.com/x"), None);
    }

    #[test]
    fn recognizes_layouts() {
        let (url, layout) = first(
            "x",
            r#"<link rel="first" href="http://x.keenspot.com/d/1.html">"#,
        );
        assert_eq!(url.as_deref(), Some("http://x.keenspot.com/d/1.html"));
        assert_eq!(layout.next, Next::Link);
        let (url, layout) = first("x", r#"<a href="/d/first.html"><img id="first_day1"></a>"#);
        assert_eq!(url.as_deref(), Some("/d/first.html"));
        assert_eq!(layout.next, Next::Id);
        let (url, layout) = first("twokinds", r#"<a href="/comic/1/">>First Comic<</a>"#);
        assert_eq!(url.as_deref(), Some("/comic/1/"));
        assert_eq!(layout.image, "</header>");
        assert_eq!(first("x", "<html></html>").0, None);
    }

    #[tokio::test]
    async fn walks_strips_until_the_last_page() {
        let front = r#"<link rel="first" href="/d/1.html">"#;
        let one = r#"<img class="ksc" src="/comics/1.png"><script class="ksc" src="/x.js"></script>
            <link rel="next" href="/d/2.html">"#;
        let two = r#"<img class="ksc" src="https://cdn.keenspot.com/2.jpg"><link rel="next" href="/d/2.html">"#;
        let (root, server) = serve(vec![
            html("<html></html>"),
            html(front),
            html(one),
            html(two),
        ]);
        let http = http("keenspot", root);
        let files = media(extract(
            &http,
            Url::parse("http://marksmen.keenspot.com").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].url.as_str(),
            "http://marksmen.keenspot.com/comics/1.png"
        );
        assert_eq!(path(&files[1]), "keenspot/marksmen/2.jpg");
        assert_eq!(
            archive(&files[0]),
            r#"["{comic}_{filename}",["marksmen","1"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[2].starts_with("GET /d/1.html "));
        assert!(requests[3].starts_with("GET /d/2.html "));
    }
}
