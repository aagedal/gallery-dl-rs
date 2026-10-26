//! WallpaperCave images, mirroring gallery_dl/extractor/wallpapercave.py.
use crate::error;
use crate::sites::gdl::{extract_iter, nameext_into, rextr};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::Map;
use url::Url;

const ROOT: &str = "https://wallpapercave.com";

static IMAGE: Route = Route::new("image", "{filename}.{extension}", &["{category}"], "");

pub static SITE: Site = Site {
    category: "wallpapercave",
    name: "WallpaperCave",
    matches: |url| crate::sites::gdl::on(url, &["wallpapercave.com"], true),
    extract,
    routes: &[&IMAGE],
    api_key: false,
    cookies: None,
};

/// Download paths: download buttons, else the `tdownload` link, else
/// the wallpaper pictures.
fn paths(page: &str) -> Vec<String> {
    let found: Vec<String> = extract_iter(page, "class=\"download\" href=\"", "\"")
        .into_iter()
        .map(str::to_owned)
        .collect();
    if !found.is_empty() {
        return found;
    }
    if let Some(pos) = page.find("id=\"tdownload\"") {
        return vec![rextr(page, "href=\"", "\"", Some(pos)).to_owned()];
    }
    extract_iter(page, "class=\"wallpaper\" id=\"wp", "</picture>")
        .into_iter()
        .map(|wp| rextr(wp, " src=\"", "\"", None))
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect()
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    crate::sites::once(async move {
        let page = http.text(&http.api(url.as_str())?).await?;
        let items = paths(&page)
            .into_iter()
            .filter_map(|path| {
                let mut image = Map::new();
                nameext_into(&path, &mut image);
                let file = Url::parse(&format!("{ROOT}{path}")).ok()?;
                let id = crate::sites::scalar(&image["filename"]);
                Some(crate::sites::Item::new(file, image).ids(id.clone(), id))
            })
            .collect::<Vec<Item>>();
        if items.is_empty() {
            return Err(error("WallpaperCave page has no images"));
        }
        Ok(crate::sites::events(
            items
                .into_iter()
                .flat_map(|item| crate::sites::post(&SITE, http, &IMAGE, vec![item]))
                .collect(),
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_download_paths() {
        assert_eq!(
            paths(
                r#"<a class="download" href="/download/a-wp1"><a class="download" href="/download/b-wp2">"#
            ),
            ["/download/a-wp1", "/download/b-wp2"]
        );
        assert_eq!(
            paths(r#"<a href="/wp/wp1.jpg" id="tdownload">"#),
            ["/wp/wp1.jpg"]
        );
        assert_eq!(
            paths(
                r#"<div class="wallpaper" id="wp1"><img src="/a.jpg"><img src="/b.jpg"></picture>"#
            ),
            ["/b.jpg"]
        );
    }
}
