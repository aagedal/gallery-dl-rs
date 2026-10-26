//! ok.porn albums, mirroring gallery_dl/extractor/okporn.py.
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
    category: "okporn",
    name: "ok.porn",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&GALLERY],
    api_key: false,
    cookies: None,
};

fn target(url: &Url) -> Option<String> {
    captures(re!(r"^https?://(?:www\.)?ok\.porn/albums/(\d+)"), url).map(|c| group(&c, 1).into())
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(id) = target(&url) else {
        return crate::sites::fail(error("unsupported ok.porn URL"));
    };
    crate::sites::once(async move {
        let page = http
            .text(&http.api(&format!("https://ok.porn/albums/{id}/"))?)
            .await?;
        let mut data = Map::new();
        data.insert("gallery_id".into(), parse_int(&id).into());
        data.insert(
            "title".into(),
            unescape(extr(&page, "h1 class=title>", "</h1>")).into(),
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
        let images = extract_iter(&page, "data-original=\"", "\"")
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
    async fn extracts_album() {
        let page = r#"<meta name="description" content="Desc"><meta name="keywords" content="a, b"><h1 class=title>Title</h1>
            <img data-original="https://ok.porn/contents/1.jpg"><img data-original="https://ok.porn/contents/2.jpg">"#;
        let (root, server) = serve(vec![html(page)]);
        let http = client("okporn", root);
        let files = media(extract(
            &http,
            Url::parse("https://ok.porn/albums/7/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(keywords(&files[1])["tags"], serde_json::json!(["a", "b"]));
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "okporn_7_002.jpg");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("okporn/7 Title")
        );
        server.join().unwrap();
    }
}
