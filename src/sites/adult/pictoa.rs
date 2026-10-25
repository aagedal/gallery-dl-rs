//! Pictoa images and albums, mirroring gallery_dl/extractor/pictoa.py: an
//! image page names its file in `og:image`; album pages queue each image
//! page, following `rel="next"`.
//!
//! Requests go to `www.pictoa.com`, where upstream's root redirects.
use crate::error;
use crate::sites::art::{extr, extract_iter, nameext_into, split_html};
use crate::sites::filehosts::rpartition;
use crate::sites::manga::keywords;
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::Value;
use url::Url;

const ROOT: &str = "https://www.pictoa.com";
const DIRECTORY: &[&str] = &["{category}", "{album_id} {album_title}"];
static IMAGE: Route = Route::new("image", "{id}.{extension}", DIRECTORY, "{id}");
static ALBUM: Route = Route::new("album", "{id}.{extension}", DIRECTORY, "{id}");

pub static SITE: Site = Site {
    category: "pictoa",
    name: "Pictoa",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&IMAGE, &ALBUM],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    /// Album and image IDs.
    Image(String, String),
    Album(String),
}

/// `(?:[\w]+\.)?pictoa\.com(?:\.de)?`.
fn pictoa_host(host: &str) -> bool {
    let host = host.strip_suffix(".de").unwrap_or(host);
    let Some(prefix) = host.strip_suffix("pictoa.com") else {
        return false;
    };
    prefix.is_empty()
        || prefix.strip_suffix('.').is_some_and(|label| {
            !label.is_empty()
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        })
}

/// `(?:[\w-]+-)?(\d+)` ending exactly at the end of `text`.
fn trailing_id(text: &str) -> Option<&str> {
    let digits = text.bytes().rev().take_while(u8::is_ascii_digit).count();
    let (prefix, id) = text.split_at(text.len() - digits);
    let prefix_ok = prefix.is_empty()
        || (prefix.len() >= 2
            && prefix.ends_with('-')
            && prefix
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')));
    (digits > 0 && prefix_ok).then_some(id)
}

/// `/albums/(?:[\w-]+-)?(\d+)/(\d+)` and `/albums/(?:[\w-]+-)?(\d+).html`,
/// both unanchored, in class order.
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) || !pictoa_host(url.host_str()?) {
        return None;
    }
    let rest = url.path().strip_prefix("/albums/")?;
    if let Some((album, tail)) = rest.split_once('/')
        && let Some(album) = trailing_id(album)
        && let Some(image) = crate::sites::art::leading_digits(tail)
    {
        return Some(Target::Image(album.to_owned(), image.to_owned()));
    }
    // `(\d+).html`: any one character, then `html`.
    rest.match_indices("html")
        .filter_map(|(i, _)| {
            let head = rest.get(..i)?;
            let head = &head[..head.char_indices().last()?.0];
            trailing_id(head)
        })
        .next()
        .map(|id| Target::Album(id.to_owned()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Image(album, image)) => crate::sites::once(async move {
            let page =
                super::page(http, &format!("{ROOT}/albums/{album}/{image}.html"), &[]).await?;
            let title = extr(&page, "property=\"og:title\" content=\"", "\"");
            let raw = extr(&page, "property=\"og:image\" content=\"", "\"");
            let mut data = keywords([
                ("album_id", album.clone().into()),
                ("album_title", rpartition(title, " #").0.into()),
                ("id", image.clone().into()),
                ("url", raw.into()),
            ]);
            nameext_into(raw, &mut data);
            let items = crate::sites::art::file_url(raw, ROOT)
                .map(|url| Item::new(url, data).ids(album, image))
                .into_iter()
                .collect();
            Ok(crate::sites::events(crate::sites::post(
                &SITE, http, &IMAGE, items,
            )))
        }),
        Some(Target::Album(id)) => album(http, id),
        None => crate::sites::fail(error("unsupported Pictoa URL")),
    }
}

/// `PictoaAlbumExtractor.items`: each image page of the album, queued with
/// the album's title and tags.
fn album(http: &Http, id: String) -> Events<'_> {
    let first = format!("{ROOT}/albums/{id}.html");
    crate::sites::paginate(
        Some((first, None::<serde_json::Map<String, Value>>, 0)),
        move |state| {
            let id = id.clone();
            async move {
                let Some((url, data, count)) = state else {
                    return Ok(None);
                };
                let page = super::page(http, &url, &[]).await?;
                let data = data.unwrap_or_else(|| {
                    let tags: Vec<String> =
                        split_html(extr(&page, "<ol class=\"related-categories", "</ol>"))
                            .into_iter()
                            .skip(1)
                            .collect();
                    keywords([
                        ("album_id", id.into()),
                        ("album_title", extr(&page, "<h1>", "<").into()),
                        ("tags", tags.into()),
                    ])
                });
                let container = extr(&page, "<main>", "<span id=\"flag\" >");
                let children = extract_iter(container, "<a rel=\"nofollow\" href=\"", "\"")
                    .into_iter()
                    .filter_map(|href| Url::parse(href).ok())
                    .map(|url| (url, data.clone()))
                    .collect();
                let next = extr(&page, "<link rel=\"next\" href=\"", "\"");
                let next = Url::parse(next)
                    .ok()
                    .filter(|next| {
                        crate::sites::plain(next) && next.host_str().is_some_and(pictoa_host)
                    })
                    .filter(|_| count + 1 < super::MAX_PAGES)
                    .map(|next| (next.to_string(), Some(data), count + 1));
                Ok(Some((
                    crate::sites::manga::queue_urls(http, &SITE, &ALBUM, children),
                    next,
                )))
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::manga::tests::{archive, path};
    use crate::sites::testing::{html, http, keywords as kw, media, queued, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.pictoa.com/albums/some-name-12345/678.html"),
            Some(Target::Image("12345".into(), "678".into()))
        );
        assert_eq!(
            t("https://pictoa.com.de/albums/12/34"),
            Some(Target::Image("12".into(), "34".into()))
        );
        assert_eq!(
            t("https://de.pictoa.com/albums/some-name-12345.html"),
            Some(Target::Album("12345".into()))
        );
        assert_eq!(
            t("https://pictoa.com/albums/9xhtml"),
            Some(Target::Album("9".into()))
        );
        for raw in [
            "https://pictoa.com/albums/name.html",
            "https://pictoa.com/albums/-1.html",
            "https://pictoa.com/albums/a-1/x.html",
            "https://a-b.pictoa.com/albums/1.html",
            "https://pictoa.com.evil.test/albums/1.html",
            "https://pictoa.com:8443/albums/1.html",
            "https://u:p@pictoa.com/albums/1.html",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[tokio::test]
    async fn extracts_image() {
        let page = "<meta property=\"og:title\" content=\"Blue Sky #2\">\
            <meta property=\"og:image\" content=\"https://s1.pictoa.com/media/a/b/678.jpg\">";
        let (root, server) = serve(vec![html(page)]);
        let http = http("pictoa", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.pictoa.com/albums/blue-sky-12345/678.html").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let k = kw(&files[0]);
        assert_eq!(k["album_id"], "12345");
        assert_eq!(k["album_title"], "Blue Sky");
        assert_eq!(k["id"], "678");
        assert_eq!(k["url"], "https://s1.pictoa.com/media/a/b/678.jpg");
        assert_eq!(path(&files[0]), "pictoa/12345 Blue Sky/678.jpg");
        assert_eq!(archive(&files[0]), r#"["{id}",["678"]]"#);
        assert!(server.join().unwrap()[0].starts_with("GET /albums/12345/678.html "));
    }

    #[tokio::test]
    async fn album_queues_image_pages() {
        let first = "<h1>Blue Sky</h1><ol class=\"related-categories\"><li>Tags</li><li><a>Sky</a></li></ol>\
            <main><a rel=\"nofollow\" href=\"https://www.pictoa.com/albums/blue-sky-12345/1.html\"></a>\
            <span id=\"flag\" ></main><link rel=\"next\" href=\"https://www.pictoa.com/albums/blue-sky-12345-2.html\">";
        let second = "<main><a rel=\"nofollow\" href=\"https://www.pictoa.com/albums/blue-sky-12345/2.html\"></a><span id=\"flag\" >";
        let (root, server) = serve(vec![html(first), html(second)]);
        let http = http("pictoa", root);
        let children = queued(extract(
            &http,
            Url::parse("https://www.pictoa.com/albums/blue-sky-12345.html").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 2);
        assert_eq!(children[1].subcategory(), "album");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /albums/12345.html "));
        assert!(requests[1].starts_with("GET /albums/blue-sky-12345-2.html "));
    }
}
