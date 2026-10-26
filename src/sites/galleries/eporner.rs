//! EPORNER photo galleries, mirroring gallery_dl/extractor/eporner.py.
use crate::error;
use crate::sites::gdl::{captures, extr, extract_iter, group, re, unescape};
use crate::sites::{Events, Http, Route, Site};
use serde_json::{Map, Value};
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
    cookies: Some("eporner.com"),
};

fn target(url: &Url) -> Option<(String, String)> {
    let c = captures(
        re!(r"^https?://(?:www\.)?eporner\.com/gallery/(\w+)(?:/([\w-]+))?"),
        url,
    )?;
    Some((group(&c, 1).into(), group(&c, 2).into()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some((id, slug)) = target(&url) else {
        return crate::sites::fail(error("unsupported EPORNER URL"));
    };
    crate::sites::once(async move {
        let slug_part = if slug.is_empty() {
            "None".to_owned()
        } else {
            slug
        };
        let page = http
            .text(&http.api(&format!("{ROOT}/gallery/{id}/{slug_part}/"))?)
            .await?;
        if page
            .get(..240.min(page.len()))
            .is_some_and(|head| head.contains("Age Verification<"))
        {
            return Err(error("EPORNER gallery requires age verification"));
        }
        let title = extr(&page, "<title>", " - EPORNER</title>");
        let title = title.strip_suffix(" Photo Gallery").unwrap_or(title);
        let mut data = Map::new();
        data.insert("gallery_id".into(), id.as_str().into());
        data.insert("title".into(), unescape(title).into());
        data.insert(
            "slug".into(),
            extr(&page, "/gallery/", "/\"")
                .rsplit('/')
                .next()
                .unwrap_or_default()
                .into(),
        );
        data.insert(
            "description".into(),
            unescape(extr(&page, "name=\"description\" content=\"", "\"")).into(),
        );
        let tags: Vec<String> = extr(&page, "EP.ads.keywords = \"", "\"")
            .split(',')
            .map(str::to_owned)
            .collect();
        data.insert("tags".into(), tags.into());
        let album = extr(
            &page,
            "class=\"photosgrid gallerygrid\"",
            "id='gallerySlideBox'",
        );
        let images = extract_iter(album, " src=\"", "\"")
            .into_iter()
            .map(|src| {
                // Previews carry a `_WxH` suffix; without it is the full image.
                let (url, _, ext) = crate::sites::gdl::rpartition(src, ".");
                let url = &url[..url.rfind('_').unwrap_or(url.len())];
                let name = &url[url.rfind('/').map_or(0, |i| i + 1)..];
                let mut image = Map::new();
                image.insert(
                    "id".into(),
                    Value::from(&name[..name.find('-').unwrap_or(name.len())]),
                );
                (format!("{url}.{ext}"), Some(image))
            })
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

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.eporner.com/gallery/WgK6O5Fa6Fz/slug-x/"),
            Some(("WgK6O5Fa6Fz".into(), "slug-x".into()))
        );
        assert_eq!(t("https://www.eporner.com/video-x/"), None);
    }

    #[tokio::test]
    async fn extracts_full_images() {
        let page = r#"<title>Nice &amp; Cool Photo Gallery - EPORNER</title><meta name="description" content="D">
            <a href="/gallery/WgK/nice-cool/">x</a> EP.ads.keywords = "a,b";
            <div class="photosgrid gallerygrid"><img src="https://static-eu-cdn.eporner.com/gallery/Wg/K6/WgK6/11208293-image-3_296x1000.jpg">
            </div><div id='gallerySlideBox'>"#;
        let (root, server) = serve(vec![html(page)]);
        let http = client("eporner", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.eporner.com/gallery/WgK/nice-cool/").unwrap(),
        ))
        .await;
        assert_eq!(
            files[0].url.as_str(),
            "https://static-eu-cdn.eporner.com/gallery/Wg/K6/WgK6/11208293-image-3.jpg"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["id"], "11208293");
        assert_eq!(k["title"], "Nice & Cool");
        assert_eq!(k["slug"], "nice-cool");
        let (file, _) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "eporner_WgK_001.jpg");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /gallery/WgK/nice-cool/ "));
    }
}
