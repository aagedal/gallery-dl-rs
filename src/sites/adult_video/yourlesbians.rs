//! YourLesbians albums, mirroring gallery_dl/extractor/yourlesbians.py:
//! Open Graph metadata and the album's linked images, named after their
//! second-to-last path segment.
use crate::error;
use crate::sites::art::{Scan, extract_iter, on_host, segment, split_html};
use crate::sites::manga::{Gallery, Page, keywords};
use crate::sites::{Events, Http, Route, Site, unescape};
use serde_json::Map;
use url::Url;

const ROOT: &str = "https://yourlesbians.com";

static ALBUM: Route = Route::new(
    "album",
    "{num:>03} {filename}.{extension}",
    &["{category}", "{title}"],
    "{title}/{num}",
);

pub static SITE: Site = Site {
    category: "yourlesbians",
    name: "YourLesbians",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&ALBUM],
    api_key: false,
    cookies: None,
};

/// `yourlesbians\.com(/album/([^/?#]+)/?)`: the page path.
fn target(url: &Url) -> Option<String> {
    if !on_host(url, &["yourlesbians.com"]) {
        return None;
    }
    let rest = url.path().strip_prefix("/album/")?;
    let name = &rest[..rest.find('/').unwrap_or(rest.len())];
    if !segment(name) {
        return None;
    }
    let slash = if rest[name.len()..].starts_with('/') {
        "/"
    } else {
        ""
    };
    Some(format!("/album/{name}{slash}"))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(path) = target(&url) else {
        return crate::sites::fail(error("unsupported YourLesbians URL"));
    };
    crate::sites::once(async move {
        let page = http.text(&http.api(&format!("{ROOT}{path}"))?).await?;
        let mut scan = Scan::new(&page);
        let album_url = scan.extr("property=\"og:url\" content=\"", "\"");
        let title = scan.extr("property=\"og:title\" content=\"", "\"");
        // Python `title[:-8]`: drop the site name suffix.
        let cut = title.char_indices().rev().nth(7).map_or(0, |(i, _)| i);
        let title = unescape(title[..cut].trim_end());
        let thumbnail = scan.extr("property=\"og:image\" content=\"", "\"");
        let description = scan.extr("property=\"og:description\" content=\"", "\"");
        let description = if description.ends_with(", right after.") {
            ""
        } else {
            description
        };
        let tags: Vec<String> = split_html(scan.extr("tags-row", "</div>"))
            .into_iter()
            .skip(1)
            .collect();
        let album = scan.extr("class=\"album-inner", "</div>");
        let data = keywords([
            ("album_url", album_url.into()),
            ("title", title.into()),
            ("album_thumbnail", thumbnail.into()),
            ("description", description.into()),
            ("tags", tags.into()),
        ]);
        let gallery = Gallery {
            enum_key: "num",
            ..Gallery::chapter(&ALBUM, data, images(album)).id(path.clone())
        };
        Ok(crate::sites::events(gallery.events(&SITE, http)))
    })
}

/// Each link's name comes from `url.rsplit("/", 2)[1]`; links with fewer
/// path separators are skipped.
fn images(album: &str) -> Vec<Page> {
    extract_iter(album, "<a href=\"", "\"")
        .into_iter()
        .filter_map(|url| {
            let mut parts = url.rsplitn(3, '/');
            parts.next()?;
            let name = parts.next()?;
            parts.next()?;
            let (stem, ext) = name.rsplit_once('.').unwrap_or(("", name));
            let mut image = Map::new();
            image.insert("filename".into(), stem.into());
            image.insert("extension".into(), ext.into());
            Some((url.to_owned(), Some(image)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::manga::tests::{archive, path};
    use crate::sites::testing::{html, http, keywords as kw, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://yourlesbians.com/album/sunny-day/"),
            Some("/album/sunny-day/".into())
        );
        assert_eq!(
            t("https://www.yourlesbians.com/album/sunny-day"),
            Some("/album/sunny-day".into())
        );
        for raw in [
            "https://yourlesbians.com/album/",
            "https://yourlesbians.com/albums/x/",
            "https://yourlesbians.com.evil.test/album/x/",
            "https://yourlesbians.com:444/album/x/",
            "https://u:p@yourlesbians.com/album/x/",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[tokio::test]
    async fn extracts_album() {
        let page = r#"<meta property="og:url" content="https://yourlesbians.com/album/sunny-day/">
<meta property="og:title" content="Sunny &amp; Day  - Site X">
<meta property="og:image" content="https://cdn.yl.test/thumb.jpg">
<meta property="og:description" content="They met, right after.">
<div class="tags-row"><a>outdoor</a><a>sun</a></div>
<div class="album-inner"><a href="https://cdn.yl.test/sources/1/first.jpg/"></a>
<a href="https://cdn.yl.test/sources/1/second.png/?x=1"></a><a href="nolinks"></a></div>"#;
        let (root, server) = serve(vec![html(page)]);
        let http = http("yourlesbians", root);
        let files = media(extract(
            &http,
            Url::parse("https://yourlesbians.com/album/sunny-day/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = kw(&files[1]);
        assert_eq!(k["title"], "Sunny & Day");
        assert_eq!(k["description"], "");
        assert_eq!(k["tags"], json!(["outdoor", "sun"]));
        assert_eq!(k["album_url"], "https://yourlesbians.com/album/sunny-day/");
        assert_eq!(k["count"], 2);
        assert_eq!(k["filename"], "second");
        assert_eq!(k["extension"], "png");
        assert_eq!(path(&files[1]), "yourlesbians/Sunny & Day/002 second.png");
        assert_eq!(
            archive(&files[1]),
            r#"["{title}/{num}",["Sunny & Day","2"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /album/sunny-day/ "));
    }
}
