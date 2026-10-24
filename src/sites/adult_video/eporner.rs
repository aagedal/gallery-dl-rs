//! EPORNER photo galleries, mirroring gallery_dl/extractor/eporner.py: the
//! gallery grid's previews with their resolution suffix removed.
use super::slug;
use crate::error;
use crate::sites::art::{extr, extract_iter, on_host};
use crate::sites::manga::{Gallery, Page, keywords};
use crate::sites::{Events, Http, Route, Site, unescape};
use serde_json::Map;
use url::Url;

const ROOT: &str = "https://www.eporner.com";

static GALLERY: Route = Route::new(
    "gallery",
    "{category}_{gallery_id}_{num:>03}.{extension}",
    &["{category}", "{gallery_id} {title}"],
    "{gallery_id}_{num}",
);

pub static SITE: Site = Site {
    category: "eporner",
    name: "EPORNER",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&GALLERY],
    api_key: false,
    cookies: None,
};

/// `eporner\.com/gallery/(\w+)(?:/([\w-]+))?`: the ID and slug.
fn target(url: &Url) -> Option<(String, Option<String>)> {
    if !on_host(url, &["eporner.com"]) {
        return None;
    }
    let rest = url.path().strip_prefix("/gallery/")?;
    let id = rest
        .find(|c: char| !crate::sites::media::word_char(c))
        .map_or(rest, |end| &rest[..end]);
    if id.is_empty() {
        return None;
    }
    let slug = rest[id.len()..]
        .strip_prefix('/')
        .map(slug)
        .filter(|slug| !slug.is_empty());
    Some((id.to_owned(), slug.map(str::to_owned)))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some((id, slug)) = target(&url) else {
        return crate::sites::fail(error("unsupported EPORNER URL"));
    };
    crate::sites::once(async move {
        // Upstream formats a missing slug as Python's `None`.
        let slug = slug.as_deref().unwrap_or("None");
        let page = http
            .text(&http.api(&format!("{ROOT}/gallery/{id}/{slug}/"))?)
            .await?;
        let head: String = page.chars().take(240).collect();
        if head.contains("Age Verification<") {
            return Err(error(
                "EPORNER requires age verification for this gallery; pass cookies",
            ));
        }
        let title = extr(&page, "<title>", " - EPORNER</title>");
        let title = title.strip_suffix(" Photo Gallery").unwrap_or(title);
        let data = keywords([
            ("gallery_id", id.clone().into()),
            ("title", unescape(title).into()),
            (
                "slug",
                extr(&page, "/gallery/", "/\"")
                    .rsplit('/')
                    .next()
                    .unwrap_or_default()
                    .into(),
            ),
            (
                "description",
                unescape(extr(&page, "name=\"description\" content=\"", "\"")).into(),
            ),
            (
                "tags",
                extr(&page, "EP.ads.keywords = \"", "\"")
                    .split(',')
                    .collect::<Vec<_>>()
                    .into(),
            ),
        ]);
        let gallery = Gallery {
            enum_key: "num",
            ..Gallery::chapter(&GALLERY, data, images(&page)).id(id)
        };
        Ok(crate::sites::events(gallery.events(&SITE, http)))
    })
}

/// Full images: `NAME_WxH.EXT` previews become `NAME.EXT`, whose `id` is
/// the name up to its first `-`.
fn images(page: &str) -> Vec<Page> {
    let album = extr(
        page,
        "class=\"photosgrid gallerygrid\"",
        "id='gallerySlideBox'",
    );
    extract_iter(album, " src=\"", "\"")
        .into_iter()
        .map(|src| {
            let (url, ext) = src.rsplit_once('.').unwrap_or(("", src));
            let url = cut(url, url.rfind('_'));
            let name = &url[url.rfind('/').map_or(0, |i| i + 1)..];
            let id = cut(name, name.find('-'));
            let mut image = Map::new();
            image.insert("id".into(), id.into());
            (format!("{url}.{ext}"), Some(image))
        })
        .collect()
}

/// Python `text[:index]`, where a missing index (`-1`) drops the last
/// character.
fn cut(text: &str, index: Option<usize>) -> &str {
    match index {
        Some(index) => &text[..index],
        None => &text[..text.char_indices().last().map_or(0, |(i, _)| i)],
    }
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
            t("https://www.eporner.com/gallery/GID_1/the-slug/"),
            Some(("GID_1".into(), Some("the-slug".into())))
        );
        assert_eq!(
            t("https://eporner.com/gallery/abc"),
            Some(("abc".into(), None))
        );
        for raw in [
            "https://www.eporner.com/gallery/",
            "https://www.eporner.com/video-abc/",
            "https://www.eporner.com.evil.test/gallery/abc/",
            "https://www.eporner.com:81/gallery/abc/",
            "https://u:p@www.eporner.com/gallery/abc/",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
        assert_eq!(cut("abc", None), "ab");
    }

    #[tokio::test]
    async fn extracts_gallery() {
        let page = "<html><title>Blue Sky Photo Gallery - EPORNER</title>
<meta name=\"description\" content=\"Clouds &amp; sky\">
<a href=\"/gallery/GID1/blue-sky/\">x</a><script>EP.ads.keywords = \"sky,blue\";</script>
<div class=\"photosgrid gallerygrid\"><img src=\"https://static.ep.test/gallery/1/11208293-image-3_296x1000.jpg\">
<img src=\"https://static.ep.test/gallery/1/11208294-image-4_296x1000.png\"><div id='gallerySlideBox'>";
        let (root, server) = serve(vec![html(page)]);
        let http = http("eporner", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.eporner.com/gallery/GID1/blue-sky/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].url.as_str(),
            "https://static.ep.test/gallery/1/11208293-image-3.jpg"
        );
        let k = kw(&files[1]);
        assert_eq!(k["id"], "11208294");
        assert_eq!(k["title"], "Blue Sky");
        assert_eq!(k["slug"], "blue-sky");
        assert_eq!(k["description"], "Clouds & sky");
        assert_eq!(k["tags"], json!(["sky", "blue"]));
        assert_eq!(k["filename"], "11208294-image-4");
        assert_eq!(
            path(&files[1]),
            "eporner/GID1 Blue Sky/eporner_GID1_002.png"
        );
        assert_eq!(archive(&files[1]), r#"["{gallery_id}_{num}",["GID1","2"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /gallery/GID1/blue-sky/ "));
    }

    #[tokio::test]
    async fn age_verification_is_an_error() {
        let (root, server) = serve(vec![html("<title>Age Verification</title>")]);
        let http = http("eporner", root);
        assert!(
            crate::sites::testing::fails(extract(
                &http,
                Url::parse("https://www.eporner.com/gallery/abc").unwrap(),
            ))
            .await
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /gallery/abc/None/ "));
    }
}
