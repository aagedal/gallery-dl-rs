//! pornstars.tube image albums, mirroring
//! gallery_dl/extractor/pornstarstube.py: the links inside the album
//! block.
use super::slug;
use crate::error;
use crate::sites::art::{extr, extract_iter, leading_digits, on_host, parse_int};
use crate::sites::manga::{Gallery, keywords};
use crate::sites::{Events, Http, Route, Site, unescape};
use url::Url;

const ROOT: &str = "https://pornstars.tube";

static GALLERY: Route = Route::new(
    "gallery",
    "{category}_{gallery_id}_{num:>03}.{extension}",
    &["{category}", "{gallery_id} {title}"],
    "{gallery_id}_{num}",
);

pub static SITE: Site = Site {
    category: "pornstarstube",
    name: "pornstars.tube",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&GALLERY],
    api_key: false,
    cookies: None,
};

/// `pornstars\.tube/albums/(\d+)(?:/([\w-]+))?`: the ID and slug.
fn target(url: &Url) -> Option<(String, Option<String>)> {
    if !on_host(url, &["pornstars.tube"]) {
        return None;
    }
    let rest = url.path().strip_prefix("/albums/")?;
    let id = leading_digits(rest)?;
    let slug = rest[id.len()..]
        .strip_prefix('/')
        .map(slug)
        .filter(|slug| !slug.is_empty());
    Some((id.to_owned(), slug.map(str::to_owned)))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some((id, slug)) = target(&url) else {
        return crate::sites::fail(error("unsupported pornstars.tube URL"));
    };
    crate::sites::once(async move {
        let path = format!("/albums/{id}/{}/", slug.as_deref().unwrap_or("a"));
        let page = http.text(&http.api(&format!("{ROOT}{path}"))?).await?;
        let data = keywords([
            ("gallery_id", parse_int(&id, 0).into()),
            ("slug", slug.unwrap_or_default().into()),
            (
                "title",
                unescape(extr(&page, "<title>", " - PORNSTARS.TUBE</title>")).into(),
            ),
            (
                "description",
                unescape(extr(&page, "name=\"description\" content=\"", "\"")).into(),
            ),
            (
                "tags",
                extr(&page, "name=\"keywords\" content=\"", "\"")
                    .split(", ")
                    .collect::<Vec<_>>()
                    .into(),
            ),
        ]);
        let album = extr(&page, "class=\"block-album\"", "\n</div>");
        let pages = extract_iter(album, " href=\"", "\"")
            .into_iter()
            .map(|url| (url.to_owned(), None))
            .collect();
        let gallery = Gallery {
            enum_key: "num",
            ..Gallery::chapter(&GALLERY, data, pages).id(id)
        };
        Ok(crate::sites::events(gallery.events(&SITE, http)))
    })
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
            t("https://pornstars.tube/albums/12345/some-slug/"),
            Some(("12345".into(), Some("some-slug".into())))
        );
        assert_eq!(
            t("https://www.pornstars.tube/albums/12345"),
            Some(("12345".into(), None))
        );
        for raw in [
            "https://pornstars.tube/albums/slug/",
            "https://pornstars.tube.evil.test/albums/1/",
            "https://pornstars.tube:8080/albums/1/",
            "https://u@pornstars.tube/albums/1/",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[tokio::test]
    async fn extracts_album() {
        let page = "<title>Garden Walk - PORNSTARS.TUBE</title>
<meta name=\"description\" content=\"A walk\"><meta name=\"keywords\" content=\"garden, walk\">
<div class=\"block-album\"><a href=\"https://cdn.pt.test/1/a.jpg\"></a>
<a href=\"https://cdn.pt.test/1/b.jpg\"></a>\n</div><a href=\"https://pornstars.tube/other\">";
        let (root, server) = serve(vec![html(page)]);
        let http = http("pornstars.tube", root);
        let files = media(extract(
            &http,
            Url::parse("https://pornstars.tube/albums/12345/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = kw(&files[0]);
        assert_eq!(k["gallery_id"], 12345);
        assert_eq!(k["slug"], "");
        assert_eq!(k["title"], "Garden Walk");
        assert_eq!(k["tags"], json!(["garden", "walk"]));
        assert_eq!(k["count"], 2);
        assert_eq!(
            path(&files[0]),
            "pornstarstube/12345 Garden Walk/pornstarstube_12345_001.jpg"
        );
        assert_eq!(
            archive(&files[1]),
            r#"["{gallery_id}_{num}",["12345","2"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /albums/12345/a/ "));
    }
}
