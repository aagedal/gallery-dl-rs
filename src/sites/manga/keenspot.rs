//! Keenspot webcomics, mirroring gallery_dl/extractor/keenspot.py: the
//! front page's layout picks the first comic page and how to find the
//! next one; each page's images are yielded until the next link repeats.
//!
//! The front page may redirect to another origin (HTTPS or a comic's own
//! domain); like upstream, that origin becomes the root for later pages.
//! Because the HTTP layer follows same-origin redirects only, the front
//! page is first probed for a redirect, which costs one extra request.
//! Later pages must stay on the root's host. For `brawlinthefamily` the
//! hard-coded first page is requested below the (redirected) root. A
//! layout upstream does not recognize ends extraction with an error
//! (upstream logs it and yields nothing).
use super::keywords;
use crate::extract::MediaJob;
use crate::sites::art::{extr, nameext_into, subdomain};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

pub static SITE: Site = Site {
    category: "keenspot",
    name: "Keenspot",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&COMIC],
    api_key: false,
    cookies: None,
};

/// Comic pages fetched for one comic.
const MAX_PAGES: usize = 100_000;
/// Redirects followed for the front page.
const MAX_REDIRECTS: usize = 5;

static COMIC: Route = Route::new(
    "comic",
    "{filename}.{extension}",
    &["{category}", "{comic}"],
    "{comic}_{filename}",
);

#[derive(Debug, PartialEq)]
struct Target {
    /// Lower-cased subdomain.
    comic: String,
    /// `(/.+)?`: the raw path and query, if any.
    path: Option<String>,
}

/// `(?!www\.|forums\.)([\w-]+)\.keenspot\.com(/.+)?`.
fn target(url: &Url) -> Option<Target> {
    let comic = subdomain(url, "keenspot.com")?.to_lowercase();
    if matches!(comic.as_str(), "www" | "forums") {
        return None;
    }
    let rest = match url.query() {
        Some(query) => format!("{}?{query}", url.path()),
        None => url.path().to_owned(),
    };
    let path = (rest.len() > 1).then_some(rest);
    Some(Target { comic, path })
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target { comic, path }) => crate::sites::once(async move {
            let (root, page) = front_page(http, &comic).await?;
            let (layout, first) = Layout::first(&comic, &root, &page)?;
            let url = match path {
                Some(path) => format!("{root}{path}"),
                None => first,
            };
            Ok(pages(http, comic, root, layout, url))
        }),
        None => crate::sites::fail(error("unsupported Keenspot URL")),
    }
}

/// The front page and the root it was served from.
async fn front_page(http: &Http, comic: &str) -> Result<(String, String)> {
    let mut url = Url::parse(&format!("http://{comic}.keenspot.com/"))
        .map_err(|_| error("invalid Keenspot URL"))?;
    for _ in 0..MAX_REDIRECTS {
        match http
            .location(&http.api(url.as_str())?, &http.browser_headers(&[]))
            .await?
        {
            Some(next) if crate::sites::plain(&next) => url = next,
            Some(_) => return Err(error("Keenspot redirected to an unusable URL")),
            None => break,
        }
    }
    let root = url.origin().ascii_serialization();
    let page = http
        .text_with(&http.api(url.as_str())?, &http.browser_headers(&[]))
        .await?;
    Ok((root, page))
}

/// How the next page is found.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Next {
    /// `_next_needle`: the first `href` after the needle.
    Needle,
    /// `_next_link`: `<link rel="next">`.
    Link,
    /// `_next_id`: the last `href` before `id="next_`.
    Id,
    /// `_next_lastblood`: the `href` after `link rel='next'`.
    Lastblood,
    /// `_next_brawl`: the last `href` before `comic-nav-next`.
    Brawl,
}

#[derive(Clone, Debug, PartialEq)]
struct Layout {
    /// Marker preceding each image's `src`.
    image: &'static str,
    needle: &'static str,
    next: Next,
}

/// `text.rextr(page, 'href="', '"', pos)`: the last `href` that ends
/// before `pos`.
fn rhref(page: &str, pos: usize) -> String {
    let begin = "href=\"";
    let Some(first) = page.get(..pos).and_then(|head| head.rfind(begin)) else {
        return String::new();
    };
    let first = first + begin.len();
    page[first..]
        .find('"')
        .map_or_else(String::new, |len| page[first..first + len].to_owned())
}

/// `text.extract(page, begin, end, pos)[0]`.
fn after(page: &str, begin: &str, end: &str, pos: usize) -> Option<String> {
    super::text::extract(page, begin, end, pos)
        .0
        .map(str::to_owned)
}

impl Layout {
    /// `KeenspotComicExtractor._first`: the layout and the first page.
    fn first(comic: &str, root: &str, page: &str) -> Result<(Self, String)> {
        let mut layout = Layout {
            image: "class=\"ksc\"",
            needle: "",
            next: Next::Needle,
        };
        if comic == "brawlinthefamily" {
            layout.next = Next::Brawl;
            layout.image = "<div id=\"comic\">";
            return Ok((layout, format!("{root}/comic/theshowdown/")));
        }
        let first = extr(page, "<link rel=\"first\" href=\"", "\"");
        if !first.is_empty() {
            if comic == "porcelain" {
                layout.needle = "id=\"porArchivetop_\"";
            } else {
                layout.next = Next::Link;
            }
            return Ok((layout, first.to_owned()));
        }
        if let Some(pos) = page.find("id=\"first_day1\"") {
            layout.next = Next::Id;
            return Ok((layout, rhref(page, pos)));
        }
        if let Some(pos) = page.find(">FIRST PAGE<") {
            if comic == "lastblood" {
                layout.next = Next::Lastblood;
                layout.image = "<div id=\"comic\">";
            } else {
                layout.next = Next::Id;
            }
            return Ok((layout, rhref(page, pos)));
        }
        if let Some(pos) = page.find("<div id=\"kscomicpart\"") {
            layout.needle = "<a href=\"/archive.html";
            let first = after(page, "href=\"", "\"", pos).unwrap_or_default();
            return Ok((layout, first));
        }
        // twokinds
        if let Some(pos) = page.find(">First Comic<") {
            layout.image = "</header>";
            layout.needle = "class=\"navarchive\"";
            return Ok((layout, rhref(page, pos)));
        }
        // flipside
        if let Some(pos) = page.find("id=\"flip_FirstDay\"") {
            layout.image = "class=\"flip_Pages ksc\"";
            layout.needle = "id=\"flip_ArcButton\"";
            return Ok((layout, rhref(page, pos)));
        }
        Err(error("Unrecognized Keenspot page layout"))
    }

    /// The next page's URL, `None` at the end.
    fn next(&self, page: &str) -> Result<Option<String>> {
        let missing = || error("Keenspot page lacks its next-page marker");
        Ok(match self.next {
            Next::Needle => {
                let pos = page.find(self.needle).ok_or_else(missing)? + self.needle.len();
                after(page, "href=\"", "\"", pos)
            }
            Next::Link => Some(extr(page, "<link rel=\"next\" href=\"", "\"").to_owned()),
            Next::Id => page.find("id=\"next_").map(|pos| rhref(page, pos)),
            Next::Lastblood => {
                let pos = page.find("link rel='next'").ok_or_else(missing)?;
                after(page, "href='", "'", pos)
            }
            Next::Brawl => {
                let pos = page.find("comic-nav-next").ok_or_else(missing)?;
                Some(rhref(page, pos)).filter(|url| !url.contains("?random"))
            }
        })
    }

    /// The images on one page, after each `image` marker.
    fn images(&self, page: &str, root: &str) -> Vec<String> {
        let mut found = Vec::new();
        let mut pos = 0;
        while let Some(at) = page[pos..].find(self.image).map(|i| pos + i) {
            let (img, next) = super::text::extract(page, "src=\"", "\"", at + self.image.len());
            let Some(img) = img else { break };
            pos = next;
            if img.ends_with(".js") {
                continue;
            }
            found.push(if img.starts_with('/') {
                format!("{root}{img}")
            } else if img.contains("youtube.com/") {
                format!("ytdl:{img}")
            } else {
                img.to_owned()
            });
        }
        found
    }
}

/// One file; `ytdl:` URLs are handed to the video downloader.
fn item(raw: &str, comic: &str) -> Option<Item> {
    let mut data: Map<String, Value> = keywords([("comic", comic.into())]);
    nameext_into(raw, &mut data);
    let filename = data
        .get("filename")
        .map(crate::sites::scalar)
        .unwrap_or_default();
    let url = Url::parse(raw).ok()?;
    let item = match raw.strip_prefix("ytdl:") {
        Some(page) => {
            let page_url = Url::parse(page).ok().filter(crate::sites::plain)?;
            Item::new(url, data).job(MediaJob::VideoPage { page_url })
        }
        None if crate::sites::plain(&url) => Item::new(url, data),
        None => return None,
    };
    Some(item.ids(comic, filename))
}

/// `KeenspotComicExtractor.items`: page after page until the next URL is
/// empty or repeats.
fn pages(http: &Http, comic: String, root: String, layout: Layout, first: String) -> Events<'_> {
    let host = Url::parse(&root)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned));
    let state = (Some(first), String::new(), 0usize);
    crate::sites::paginate(state, move |(url, prev, count)| {
        let (comic, root, layout, host) =
            (comic.clone(), root.clone(), layout.clone(), host.clone());
        async move {
            let Some(url) = url.filter(|url| !url.is_empty() && *url != prev) else {
                return Ok(None);
            };
            if count >= MAX_PAGES {
                return Ok(None);
            }
            let Some(target) = Url::parse(&root)
                .and_then(|root| root.join(&url))
                .ok()
                .filter(|target| {
                    crate::sites::plain(target) && target.host_str() == host.as_deref()
                })
            else {
                return Ok(None);
            };
            let page = http
                .text_with(&http.api(target.as_str())?, &http.browser_headers(&[]))
                .await?;
            let items: Vec<Item> = layout
                .images(&page, &root)
                .iter()
                .filter_map(|raw| item(raw, &comic))
                .collect();
            let next = layout.next(&page)?;
            let events = crate::sites::post(&SITE, http, &COMIC, items);
            Ok(Some((crate::sites::events(events), (next, url, count + 1))))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::super::tests::{archive, path};
    use super::*;
    use crate::sites::testing::{html, http, keywords as kw, media, response, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("http://marksmen.keenspot.com/"),
            Some(Target {
                comic: "marksmen".into(),
                path: None,
            })
        );
        assert_eq!(
            t("https://TwoKinds.keenspot.com/comic/1083/?x=1"),
            Some(Target {
                comic: "twokinds".into(),
                path: Some("/comic/1083/?x=1".into()),
            })
        );
        for raw in [
            "http://www.keenspot.com/",
            "http://forums.keenspot.com/",
            "http://keenspot.com/",
            "http://a.b.keenspot.com/",
            "http://marksmen.keenspot.com.evil.test/",
            "http://marksmen.keenspot.com:8080/",
            "http://u:p@marksmen.keenspot.com/",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[test]
    fn picks_layouts() {
        let first = |comic: &str, page: &str| Layout::first(comic, "http://x", page).unwrap();
        let (layout, url) = first("a", r#"<link rel="first" href="/d/1.html">"#);
        assert_eq!((layout.next, url.as_str()), (Next::Link, "/d/1.html"));
        let (layout, _) = first("porcelain", r#"<link rel="first" href="/d/1.html">"#);
        assert_eq!(
            (layout.next, layout.needle),
            (Next::Needle, "id=\"porArchivetop_\"")
        );
        let (layout, url) = first(
            "a",
            r#"<a href="/d/0.html"></a><a href="/d/1.html" id="first_day1">"#,
        );
        assert_eq!((layout.next, url.as_str()), (Next::Id, "/d/1.html"));
        let (layout, url) = first("twokinds", r#"<a href="/comic/1/">x</a>>First Comic<"#);
        assert_eq!((layout.image, url.as_str()), ("</header>", "/comic/1/"));
        let (layout, _) = first("brawlinthefamily", "");
        assert_eq!(layout.next, Next::Brawl);
        assert!(Layout::first("a", "http://x", "<p>").is_err());
    }

    #[tokio::test]
    async fn walks_pages_from_the_redirected_front_page() {
        let redirect = String::from_utf8(response("301 Moved Permanently", "text/html", b""))
            .unwrap()
            .replace(
                "\r\n\r\n",
                "\r\nLocation: https://twokinds.keenspot.com/\r\n\r\n",
            )
            .into_bytes();
        let front = r#"<a href="/comic/1/">First</a>>First Comic<"#;
        let one = r#"<header>x</header><img src="/comics/20031022.jpg">
<a href="/x" class="navarchive"></a><a href="/comic/2/">next</a>"#;
        let two = r#"<header>x</header><iframe src="https://www.youtube.com/embed/abc"></iframe>
<a class="navarchive"></a><a href="/comic/2/">same</a>"#;
        let (root, requests) = serve(vec![
            redirect,
            html(front),
            html(front),
            html(one),
            html(two),
        ]);
        let http = http("Keenspot", root);
        let files = media(extract(
            &http,
            Url::parse("http://twokinds.keenspot.com/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].url.as_str(),
            "https://twokinds.keenspot.com/comics/20031022.jpg"
        );
        let k = kw(&files[0]);
        assert_eq!(k["comic"], "twokinds");
        assert_eq!(k["filename"], "20031022");
        assert_eq!(path(&files[0]), "keenspot/twokinds/20031022.jpg");
        assert_eq!(
            archive(&files[0]),
            r#"["{comic}_{filename}",["twokinds","20031022"]]"#
        );
        assert_eq!(
            files[1].url.as_str(),
            "ytdl:https://www.youtube.com/embed/abc"
        );
        assert!(matches!(files[1].job, MediaJob::VideoPage { .. }));
        assert_eq!(kw(&files[1])["filename"], "abc");
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET / "));
        assert!(requests[3].starts_with("GET /comic/1/ "));
        assert!(requests[4].starts_with("GET /comic/2/ "));
    }

    #[tokio::test]
    async fn path_overrides_the_first_page_and_link_layout_stops() {
        let front = r#"<link rel="first" href="/d/1.html">"#;
        let page = r#"<img class="ksc" src="/scripts/x.js"><img class="ksc" src="http://cdn.keenspot.com/a.png">"#;
        let (root, requests) = serve(vec![html(""), html(front), html(page)]);
        let http = http("Keenspot", root);
        let files = media(extract(
            &http,
            Url::parse("http://marksmen.keenspot.com/d/20.html").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].url.as_str(), "http://cdn.keenspot.com/a.png");
        let requests = requests.join().unwrap();
        assert!(requests[2].starts_with("GET /d/20.html "));
    }
}
