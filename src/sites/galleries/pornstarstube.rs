//! pornstars.tube albums, mirroring gallery_dl/extractor/pornstarstube.py.
use crate::error;
use crate::sites::gdl::{captures, extr, extract_iter, group, parse_int, re, unescape};
use crate::sites::manga::{GALLERY_ARCHIVE, GALLERY_FILENAME};
use crate::sites::{Events, Http, Route, Site};
use serde_json::{Map, Value};
use url::Url;

static GALLERY: Route = Route::new(
    "gallery",
    GALLERY_FILENAME,
    &["{category}", "{gallery_id} {title}"],
    GALLERY_ARCHIVE,
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

fn target(url: &Url) -> Option<(String, String)> {
    captures(
        re!(r"^https?://(?:www\.)?pornstars\.tube/albums/(\d+)(?:/([\w-]+))?"),
        url,
    )
    .map(|c| (group(&c, 1).into(), group(&c, 2).into()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some((id, slug)) = target(&url) else {
        return crate::sites::fail(error("unsupported pornstars.tube URL"));
    };
    crate::sites::once(async move {
        let path_slug = if slug.is_empty() { "a" } else { &slug };
        let page = http
            .text(&http.api(&format!("https://pornstars.tube/albums/{id}/{path_slug}/"))?)
            .await?;
        let mut data = Map::new();
        data.insert("gallery_id".into(), parse_int(&id).into());
        data.insert("slug".into(), slug.as_str().into());
        data.insert(
            "title".into(),
            unescape(extr(&page, "<title>", " - PORNSTARS.TUBE</title>")).into(),
        );
        data.insert(
            "description".into(),
            unescape(extr(&page, "name=\"description\" content=\"", "\"")).into(),
        );
        let tags: Vec<Value> = extr(&page, "name=\"keywords\" content=\"", "\"")
            .split(", ")
            .map(Value::from)
            .collect();
        data.insert("tags".into(), tags.into());
        let album = extr(&page, "class=\"block-album\"", "\n</div>");
        let images = extract_iter(album, " href=\"", "\"")
            .into_iter()
            .map(|u| (u.to_owned(), None))
            .collect();
        Ok(crate::sites::events(super::numbered(
            &SITE, http, &GALLERY, &data, images, &id,
        )))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http as client, keywords, media, serve};

    #[tokio::test]
    async fn extracts_album_links() {
        let page = "<title>Title - PORNSTARS.TUBE</title><div class=\"block-album\">\n<a href=\"https://pornstars.tube/get/1.jpg\"></a>\n</div><a href=\"https://x/other\">";
        let (root, server) = serve(vec![html(page)]);
        let http = client("pornstarstube", root);
        let files = media(extract(
            &http,
            Url::parse("https://pornstars.tube/albums/12345/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(keywords(&files[0])["title"], "Title");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /albums/12345/a/ "));
    }
}
