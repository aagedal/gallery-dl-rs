//! hentai-cosplay-xxx.com, hentai-img-xxx.com and porn-image.com galleries,
//! mirroring gallery_dl/extractor/hentaicosplays.py.
use crate::error;
use crate::sites::gdl::{extr, extract_iter, rpartition, unescape};
use crate::sites::{Events, Http, Route, Site};
use serde_json::Map;
use url::Url;

static GALLERY: Route = Route::new(
    "gallery",
    "{filename}.{extension}",
    &["{site}", "{title}"],
    "{title}_{filename}",
);

macro_rules! instance {
    ($ident:ident, $category:literal, $name:literal, $root:literal, $pattern:literal) => {
        pub static $ident: Site = Site {
            category: $category,
            name: $name,
            matches: |url| target($pattern, url).is_some(),
            extract: |http, url| extract(&$ident, $root, $pattern, http, url),
            routes: &[&GALLERY],
            api_key: false,
            cookies: None,
        };
    };
}
instance!(
    HENTAICOSPLAY,
    "hentaicosplay",
    "Hentai Cosplay",
    "https://hentai-cosplay-xxx.com",
    r"^https?://(?:\w\w\.)?hentai-cosplays?(?:-xxx)?\.com/(?:image|story)/([\w-]+)"
);
instance!(
    HENTAIIMG,
    "hentaiimg",
    "Hentai Image",
    "https://hentai-img-xxx.com",
    r"^https?://(?:\w\w\.)?hentai-img(?:-xxx)?\.com/(?:image|story)/([\w-]+)"
);
instance!(
    PORNIMAGE,
    "pornimage",
    "Porn Image",
    "https://porn-image.com",
    r"^https?://(?:\w\w\.)?porn-images?(?:-xxx)?\.com/(?:image|story)/([\w-]+)"
);

fn compiled(pattern: &'static str) -> regex::Regex {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<&'static str, regex::Regex>>> = OnceLock::new();
    let mut cache = CACHE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    cache
        .entry(pattern)
        .or_insert_with(|| regex::Regex::new(pattern).expect("static pattern"))
        .clone()
}

fn target(pattern: &'static str, url: &Url) -> Option<String> {
    crate::sites::gdl::captures(&compiled(pattern), url).map(|c| c[1].to_owned())
}

fn extract<'a>(
    site: &'static Site,
    root: &'static str,
    pattern: &'static str,
    http: &'a Http,
    url: Url,
) -> Events<'a> {
    let Some(slug) = target(pattern, &url) else {
        return crate::sites::fail(error("unsupported gallery URL"));
    };
    crate::sites::once(async move {
        let page_url = format!("{root}/story/{slug}/");
        let page = http
            .text_with(&http.api(&page_url)?, &[("Referer", &page_url)])
            .await?;
        let mut data = Map::new();
        data.insert(
            "title".into(),
            unescape(rpartition(extr(&page, "<title>", "</title>"), " Story Viewer - ").0).into(),
        );
        data.insert("slug".into(), slug.as_str().into());
        data.insert(
            "site".into(),
            rpartition(crate::sites::gdl::partition(root, "://").2, ".")
                .0
                .into(),
        );
        let images = extract_iter(&page, "<amp-img class=\"auto-style\" src=\"", "\"")
            .into_iter()
            .map(|u| (u.replacen("http:", "https:", 1), None))
            .collect();
        let mut events = super::numbered(site, http, &GALLERY, &data, images, &slug);
        for event in &mut events {
            if let crate::event::ExtractionEvent::Media(media) = event {
                media.headers.insert("Referer".into(), page_url.clone());
            }
        }
        Ok(crate::sites::events(events))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http as client, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        assert!((HENTAICOSPLAY.matches)(
            &Url::parse("https://hentai-cosplays.com/image/---devilism--tide-kurihara-/").unwrap()
        ));
        assert!((PORNIMAGE.matches)(
            &Url::parse("https://fr.porn-images-xxx.com/image/enako-enako-24/").unwrap()
        ));
        assert!((HENTAIIMG.matches)(
            &Url::parse("https://ja.hentai-img.com/image/hollow-cora-502/").unwrap()
        ));
        assert!(!(HENTAIIMG.matches)(
            &Url::parse("https://porn-image.com/image/x/").unwrap()
        ));
    }

    #[tokio::test]
    async fn extracts_story_images() {
        let page = r#"<title>Enako &amp; Cosplay Story Viewer - Porn Image</title>
            <amp-img class="auto-style" src="http://static.porn-image.com/upload/1.jpg"></amp-img>"#;
        let (root, server) = serve(vec![html(page)]);
        let http = client("pornimage", root);
        let files = media((PORNIMAGE.extract)(
            &http,
            Url::parse("https://fr.porn-image.com/image/enako-enako-24/").unwrap(),
        ))
        .await;
        assert_eq!(
            files[0].url.as_str(),
            "https://static.porn-image.com/upload/1.jpg"
        );
        assert_eq!(
            files[0].headers["Referer"],
            "https://porn-image.com/story/enako-enako-24/"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["site"], "porn-image");
        assert_eq!(k["title"], "Enako & Cosplay");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /story/enako-enako-24/ "));
    }
}
