//! Pictoa albums and images, mirroring gallery_dl/extractor/pictoa.py.
use crate::error;
use crate::sites::gdl::{
    captures, extr, extract_iter, group, nameext_into, re, rpartition, split_html,
};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://pictoa.com";
const BASE: &str = r"^https?://(?:[\w]+\.)?pictoa\.com(?:\.de)?";
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
    Image(String, String),
    Album(String),
}

fn target(url: &Url) -> Option<Target> {
    if let Some(c) = captures(re!(&format!(r"{BASE}/albums/(?:[\w-]+-)?(\d+)/(\d+)")), url) {
        return Some(Target::Image(group(&c, 1).into(), group(&c, 2).into()));
    }
    captures(re!(&format!(r"{BASE}/albums/(?:[\w-]+-)?(\d+).html")), url)
        .map(|c| Target::Album(group(&c, 1).into()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Pictoa URL"));
    };
    match target {
        Target::Image(album_id, image_id) => crate::sites::once(async move {
            let page = http
                .text(&http.api(&format!("{ROOT}/albums/{album_id}/{image_id}.html"))?)
                .await?;
            let title = extr(&page, "property=\"og:title\" content=\"", "\"");
            let file = extr(&page, "property=\"og:image\" content=\"", "\"").to_owned();
            let mut data = Map::new();
            data.insert("album_id".into(), album_id.as_str().into());
            data.insert("album_title".into(), rpartition(title, " #").0.into());
            data.insert("id".into(), image_id.as_str().into());
            data.insert("url".into(), file.as_str().into());
            nameext_into(&file, &mut data);
            let file = Url::parse(&file).map_err(|_| error("Pictoa image has no file"))?;
            Ok(crate::sites::events(crate::sites::post(
                &SITE,
                http,
                &IMAGE,
                vec![Item::new(file, data).ids(album_id, image_id)],
            )))
        }),
        Target::Album(album_id) => crate::sites::paginate(
            Some((
                format!("{ROOT}/albums/{album_id}.html"),
                None::<Map<String, Value>>,
            )),
            move |state| {
                let album_id = album_id.clone();
                async move {
                    let Some((url, data)) = state else {
                        return Ok(None);
                    };
                    let page = http.text(&http.api(&url)?).await?;
                    let data = data.unwrap_or_else(|| {
                        let mut data = Map::new();
                        data.insert("album_id".into(), album_id.as_str().into());
                        data.insert("album_title".into(), extr(&page, "<h1>", "<").into());
                        let tags: Vec<Value> =
                            split_html(extr(&page, "<ol class=\"related-categories", "</ol>"))
                                .into_iter()
                                .skip(1)
                                .map(Value::from)
                                .collect();
                        data.insert("tags".into(), tags.into());
                        data
                    });
                    let container = extr(&page, "<main>", "<span id=\"flag\" >");
                    let children = extract_iter(container, "<a rel=\"nofollow\" href=\"", "\"")
                        .into_iter()
                        .filter_map(|link| Some((Url::parse(link).ok()?, data.clone())))
                        .collect();
                    let next = extr(&page, "<link rel=\"next\" href=\"", "\"");
                    let next = (!next.is_empty()).then(|| (next.to_owned(), Some(data)));
                    Ok(Some((super::queue(http, &SITE, &ALBUM, children), next)))
                }
            },
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{children, html, http as client, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.pictoa.com/albums/name-12345/678.html"),
            Some(Target::Image("12345".into(), "678".into()))
        );
        assert_eq!(
            t("https://www.pictoa.com.de/albums/name-12345.html"),
            Some(Target::Album("12345".into()))
        );
    }

    #[tokio::test]
    async fn album_follows_next_links() {
        let first = r#"<h1>Album</h1><main><a rel="nofollow" href="https://www.pictoa.com/albums/a-1/2.html"></a><span id="flag" ><link rel="next" href="https://www.pictoa.com/albums/a-1-p2.html">"#;
        let second = r#"<main><a rel="nofollow" href="https://www.pictoa.com/albums/a-1/3.html"></a><span id="flag" >"#;
        let (root, server) = serve(vec![html(first), html(second)]);
        let http = client("pictoa", root);
        let queued = children(extract(
            &http,
            Url::parse("https://www.pictoa.com/albums/a-1.html").unwrap(),
        ))
        .await;
        assert_eq!(queued.len(), 2);
        server.join().unwrap();
    }
}
