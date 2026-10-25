//! hentai-cosplay-xxx.com and its sister sites hentai-img-xxx.com and
//! porn-image.com, mirroring gallery_dl/extractor/hentaicosplays.py: a
//! gallery's story-viewer page lists every image.
//!
//! Configured `extractor.hentaicosplays.<category>` instances and
//! `hentaicosplays:URL` inputs go through `crate::sites::instances`.
use crate::error;
use crate::sites::art::{extr, extract_iter};
use crate::sites::filehosts::rpartition;
use crate::sites::manga::{Gallery, keywords};
use crate::sites::{Events, Http, Route, Site, unescape};
use url::Url;

static GALLERY: Route = Route::new(
    "gallery",
    "{filename}.{extension}",
    &["{site}", "{title}"],
    "{title}_{filename}",
);

macro_rules! instance {
    ($ident:ident, $category:literal, $name:literal, $root:literal, $stem:literal, $plural:literal) => {
        pub static $ident: Site = Site {
            category: $category,
            name: $name,
            matches: |url| target($stem, $plural, url).is_some(),
            extract: |http, url| extract(&$ident, $root, $stem, $plural, http, url),
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
    "hentai-cosplay",
    "hentai-cosplays"
);
instance!(
    HENTAIIMG,
    "hentaiimg",
    "Hentai Img",
    "https://hentai-img-xxx.com",
    "hentai-img",
    "hentai-img"
);
instance!(
    PORNIMAGE,
    "pornimage",
    "Porn Image",
    "https://porn-image.com",
    "porn-image",
    "porn-images"
);

/// `(?:\w\w\.)?STEM(?:-xxx)?\.com` (with an optional plural `s` for two of
/// the sites), then `/(?:image|story)/([\w-]+)`, unanchored.
fn target(stem: &str, plural: &str, url: &Url) -> Option<String> {
    if !crate::sites::plain(url) {
        return None;
    }
    if !stem_host(stem, plural, url) {
        return None;
    }
    slug(url.path())
}

/// Configured `extractor.hentaicosplays.<category>` instances.
pub(crate) static FAMILY: crate::sites::instances::Family = crate::sites::instances::Family {
    basecategory: "hentaicosplays",
    builtin: &[&HENTAICOSPLAY, &HENTAIIMG, &PORNIMAGE],
    related: &[],
    template: &HENTAICOSPLAY,
    matches: |category, url| configured_target(category, url).is_some(),
    extract: |category, http, url| match crate::sites::instances::configured(&FAMILY, category) {
        Some(instance) => match configured_target(category, &url) {
            Some(slug) => gallery(&instance.site, instance.root, slug, http),
            None => crate::sites::fail(error("unsupported hentaicosplays URL")),
        },
        None => {
            let site = FAMILY
                .builtin
                .iter()
                .copied()
                .find(|site| site.category == category)
                .unwrap_or(&HENTAICOSPLAY);
            (site.extract)(http, url)
        }
    },
};

/// The gallery slug of a URL below a configured instance's root.
fn configured_target(category: &str, url: &Url) -> Option<String> {
    match crate::sites::instances::configured(&FAMILY, category) {
        Some(instance) => {
            let rest = crate::sites::instances::below(url, instance.spec)?;
            slug(rest.split('?').next().unwrap_or_default())
        }
        None => None,
    }
}

/// `/(?:image|story)/([\w-]+)`.
fn slug(path: &str) -> Option<String> {
    let raw: Vec<&str> = path.split('/').skip(1).collect();
    let ["image" | "story", slug, ..] = raw.as_slice() else {
        return None;
    };
    let len = slug
        .bytes()
        .take_while(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
        .count();
    (len > 0).then(|| slug[..len].to_owned())
}

/// `(?:\w\w\.)?STEM(?:-xxx)?\.com`.
fn stem_host(stem: &str, plural: &str, url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = match host.split_once('.') {
        Some((label, rest))
            if label.len() == 2
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_')
                && rest.ends_with(".com")
                && rest.matches('.').count() == 1 =>
        {
            rest
        }
        _ => host,
    };
    let Some(name) = host.strip_suffix(".com") else {
        return false;
    };
    let name = name.strip_suffix("-xxx").unwrap_or(name);
    name == stem || name == plural
}

fn extract<'a>(
    site: &'static Site,
    root: &'static str,
    stem: &'static str,
    plural: &'static str,
    http: &'a Http,
    url: Url,
) -> Events<'a> {
    let Some(slug) = target(stem, plural, &url) else {
        return crate::sites::fail(error(format!("unsupported {} URL", site.name)));
    };
    gallery(site, root, slug, http)
}

/// The gallery's story-viewer page below `root`.
fn gallery<'a>(
    site: &'static Site,
    root: &'static str,
    slug: String,
    http: &'a Http,
) -> Events<'a> {
    crate::sites::once(async move {
        let page_url = format!("{root}/story/{slug}/");
        let page = super::page(http, &page_url, &[("Referer", &page_url)]).await?;
        let title = rpartition(extr(&page, "<title>", "</title>"), " Story Viewer - ").0;
        let site_name = rpartition(root.split_once("://").map_or("", |(_, host)| host), ".").0;
        let data = keywords([
            ("title", unescape(title).into()),
            ("slug", slug.clone().into()),
            ("site", site_name.into()),
        ]);
        let pages = extract_iter(&page, "<amp-img class=\"auto-style\" src=\"", "\"")
            .into_iter()
            .map(|url| (url.replacen("http:", "https:", 1), None))
            .collect();
        Ok(crate::sites::events(
            Gallery {
                enum_key: "num",
                ..Gallery::chapter(&GALLERY, data, pages)
                    .id(slug)
                    .header("Referer", page_url)
            }
            .events(site, http),
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::manga::tests::{archive, path};
    use crate::sites::testing::{html, http, keywords as kw, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| crate::sites::find(&Url::parse(raw).unwrap()).map(|s| s.category);
        assert_eq!(
            t("https://hentai-cosplay-xxx.com/image/some-title/"),
            Some("hentaicosplay")
        );
        assert_eq!(
            t("https://jp.hentai-cosplays.com/story/some-title/"),
            Some("hentaicosplay")
        );
        assert_eq!(t("https://hentai-img-xxx.com/image/x/"), Some("hentaiimg"));
        assert_eq!(t("https://porn-images-xxx.com/image/x/"), Some("pornimage"));
        assert_eq!(t("http://porn-image.com/story/x"), Some("pornimage"));
        let slug = |raw: &str| target("porn-image", "porn-images", &Url::parse(raw).unwrap());
        assert_eq!(
            slug("https://porn-image.com/image/a_b-c.d/"),
            Some("a_b-c".into())
        );
        for raw in [
            "https://porn-image.com/image/",
            "https://porn-image.com/tag/x/",
            "https://www.porn-image.com/image/x/",
            "https://a.b.porn-image.com/image/x/",
            "https://porn-image.com.evil.test/image/x/",
            "https://porn-image.com:8443/image/x/",
            "https://u:p@porn-image.com/image/x/",
            "https://porn-imagess.com/image/x/",
        ] {
            assert_eq!(slug(raw), None, "{raw}");
        }
    }

    #[tokio::test]
    async fn configured_instance_uses_its_root() {
        crate::sites::instances::define(
            "hentaicosplays",
            "hentaicosplays-configured",
            serde_json::json!({"root": "https://cos.test", "pattern": r"(?:\w\w\.)?cos\.test"}),
        );
        let url = Url::parse("https://jp.cos.test/image/garden-park/").unwrap();
        let site = crate::sites::find(&url).unwrap();
        assert_eq!(site.category, "hentaicosplays-configured");
        let page = "<title>Garden Story Viewer - X</title>\
            <amp-img class=\"auto-style\" src=\"https://static.test/a/01.jpg\"></amp-img>";
        let (root, server) = serve(vec![html(page)]);
        let http = http("hentaicosplays-configured", root);
        let files = media((site.extract)(&http, url)).await;
        let k = kw(&files[0]);
        assert_eq!(k["category"], "hentaicosplays-configured");
        assert_eq!(k["site"], "cos");
        assert_eq!(
            files[0].headers["Referer"],
            "https://cos.test/story/garden-park/"
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /story/garden-park/ "));
    }

    #[tokio::test]
    async fn extracts_gallery() {
        let page = "<title>Garden &amp; Park Story Viewer - Hentai Image</title>\
            <amp-img class=\"auto-style\" src=\"http://static.test/a/01.jpg\"></amp-img>\
            <amp-img class=\"auto-style\" src=\"https://static.test/a/02.jpg\"></amp-img>";
        let (root, server) = serve(vec![html(page)]);
        let http = http("hentaiimg", root);
        let files = media(extract(
            &HENTAIIMG,
            "https://hentai-img-xxx.com",
            "hentai-img",
            "hentai-img",
            &http,
            Url::parse("https://hentai-img-xxx.com/image/garden-park/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].url.as_str(), "https://static.test/a/01.jpg");
        assert_eq!(
            files[0].headers["Referer"],
            "https://hentai-img-xxx.com/story/garden-park/"
        );
        let k = kw(&files[1]);
        assert_eq!(k["category"], "hentaiimg");
        assert_eq!(k["title"], "Garden & Park");
        assert_eq!(k["slug"], "garden-park");
        assert_eq!(k["site"], "hentai-img-xxx");
        assert_eq!(k["num"], 2);
        assert_eq!(path(&files[1]), "hentai-img-xxx/Garden & Park/02.jpg");
        assert_eq!(
            archive(&files[1]),
            r#"["{title}_{filename}",["Garden & Park","02"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /story/garden-park/ "));
    }
}
