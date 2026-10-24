//! WallpaperCave wallpapers, mirroring gallery_dl/extractor/wallpapercave.py:
//! any page's download links, else its `tdownload` link, else the wallpaper
//! images it shows.
use crate::error;
use crate::sites::art::{extract_iter, nameext_into};
use crate::sites::filehosts::rextr;
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::Map;
use url::Url;

const ROOT: &str = "https://wallpapercave.com";
static IMAGE: Route = Route::new("image", "{filename}.{extension}", &["{category}"], "");

pub static SITE: Site = Site {
    category: "wallpapercave",
    name: "WallpaperCave",
    matches: |url| crate::sites::art::on_host(url, &["wallpapercave.com"]),
    extract,
    routes: &[&IMAGE],
    api_key: false,
    cookies: None,
};

fn extract(http: &Http, mut url: Url) -> Events<'_> {
    if !(SITE.matches)(&url) {
        return crate::sites::fail(error("unsupported WallpaperCave URL"));
    }
    url.set_fragment(None);
    crate::sites::once(async move {
        let page = http.text(&http.api(url.as_str())?).await?;
        let mut events = Vec::new();
        for path in paths(&page) {
            let Some(file) = super::web_url(&format!("{ROOT}{path}")) else {
                continue;
            };
            let mut image = Map::new();
            nameext_into(path, &mut image);
            let name = crate::sites::scalar(&image["filename"]);
            let item = Item::new(file, image).ids(name.clone(), name);
            events.extend(crate::sites::post(&SITE, http, &IMAGE, vec![item]));
        }
        Ok(crate::sites::events(events))
    })
}

/// The file paths of a page, in upstream's order of preference.
fn paths(page: &str) -> Vec<&str> {
    let downloads = extract_iter(page, "class=\"download\" href=\"", "\"");
    if !downloads.is_empty() {
        return downloads;
    }
    if let Some(pos) = page.find("id=\"tdownload\"") {
        // `text.rextr` with `default=None`: an empty link still counts.
        if page[..pos].contains("href=\"") {
            return vec![rextr(page, "href=\"", "\"", Some(pos))];
        }
    }
    extract_iter(page, "class=\"wallpaper\" id=\"wp", "</picture>")
        .into_iter()
        .map(|wp| rextr(wp, " src=\"", "\"", None))
        .filter(|path| !path.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, serve};

    #[test]
    fn matches_urls() {
        let m = |raw: &str| (SITE.matches)(&Url::parse(raw).unwrap());
        assert!(m("https://wallpapercave.com/w/wp10270355"));
        assert!(m(
            "https://www.wallpapercave.com/apple-wwdc-2024-wallpapers"
        ));
        for raw in [
            "https://mobile.wallpapercave.com/w/wp1",
            "https://wallpapercave.com.evil.test/w/wp1",
            "https://evilwallpapercave.com/w/wp1",
            "https://wallpapercave.com:8080/w/wp1",
            "https://u:p@wallpapercave.com/w/wp1",
        ] {
            assert!(!m(raw), "{raw}");
        }
    }

    #[test]
    fn falls_back_to_tdownload_and_pictures() {
        let page = "<a href=\"/x\"></a><a href=\"/download/a-wp1\" id=\"tdownload\">";
        assert_eq!(paths(page), ["/download/a-wp1"]);
        let page = "<div class=\"wallpaper\" id=\"wp1\"><picture><img src=\"/a.webp\">\
            <img src=\"/wp/wp1.jpg\"></picture><div class=\"wallpaper\" id=\"wp2\"></picture>";
        assert_eq!(paths(page), ["/wp/wp1.jpg"]);
    }

    #[tokio::test]
    async fn extracts_download_links() {
        let page = "<a class=\"download\" href=\"/download/sunset-wp1\">\
            <a class=\"download\" href=\"/download/moon-wp2.png\">";
        let (root, server) = serve(vec![html(page)]);
        let http = http("wallpapercave", root);
        let files = media(extract(
            &http,
            Url::parse("https://wallpapercave.com/space-wallpapers?x=1#top").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].url.as_str(),
            "https://wallpapercave.com/download/sunset-wp1"
        );
        let k = keywords(&files[1]);
        assert_eq!(k["filename"], "moon-wp2");
        assert_eq!(k["extension"], "png");
        assert_eq!(k["subcategory"], "image");
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "moon-wp2.png");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("wallpapercave")
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /space-wallpapers?x=1 "));
    }
}
