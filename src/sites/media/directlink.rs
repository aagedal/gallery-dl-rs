//! Direct links to media files, mirroring gallery_dl/extractor/directlink.py.
//!
//! Top-level direct links keep the `direct` download path, so this site never
//! claims a URL in [`crate::sites::find`]. It extracts queued child URLs that
//! no other site handles, as upstream's lowest-priority extractor does.
use crate::error;
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

/// Upstream names the subcategory after the link's domain; registry routes
/// are static, so files carry an empty subcategory. Upstream's filename
/// `{domain}/{path}/{filename}.{extension}` is cleaned as one segment, so
/// its slashes become underscores; `R` does that replacement here.
static DIRECTLINK: Route = Route::new(
    "",
    "{domain}_{path:R/_/}_{filename}.{extension}",
    &["{category}"],
    "{domain}/{path}/{filename}.{extension}",
);

pub static SITE: Site = Site {
    category: "directlink",
    name: "direct link",
    matches: |_| false,
    extract,
    routes: &[&DIRECTLINK],
    api_key: false,
    cookies: None,
};

const EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "jpe", "png", "gif", "bmp", "svg", "webm", "webp", "avif", "heic", "psd", "mp4",
    "m4v", "mov", "mkv", "ogg", "ogm", "ogv", "wav", "mp3", "opus", "zip", "rar", "7z", "pdf",
    "swf",
];

/// Upstream's pattern: an HTTP(S) URL whose path ends in a media extension.
/// Credentials in the authority are rejected.
pub(crate) fn matches(url: &Url) -> bool {
    target(url).is_some()
}

/// `(domain, path)` as the pattern's raw groups.
fn target(url: &Url) -> Option<(String, String)> {
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    let host = url.host_str()?;
    let domain = match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    };
    let path = url.path().strip_prefix('/')?;
    let (stem, ext) = path.rsplit_once('.')?;
    if stem.is_empty() || !EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()) {
        return None;
    }
    Some((domain, path.to_owned()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some((domain, path)) = target(&url) else {
        return crate::sites::fail(error("unsupported direct link"));
    };
    let item = item(url, &domain, &path);
    crate::sites::events(crate::sites::post(&SITE, http, &DIRECTLINK, vec![item]))
}

/// `DirectlinkExtractor.items`: unquoted groups, with the path split into
/// its directory, filename and lowercase extension.
fn item(url: Url, domain: &str, path: &str) -> Item {
    let unquote = |raw: &str| crate::sites::percent_decode(raw);
    let path = unquote(path);
    let (dir, name) = path.rsplit_once('/').unwrap_or(("", &path));
    let (filename, ext) = name.rsplit_once('.').unwrap_or(("", name));
    let mut data = Map::new();
    data.insert("domain".into(), unquote(domain).into());
    data.insert("path".into(), dir.into());
    let optional = |value: Option<&str>| match value.filter(|v| !v.is_empty()) {
        Some(value) => Value::from(unquote(value)),
        None => Value::Null,
    };
    data.insert("query".into(), optional(url.query()));
    data.insert("fragment".into(), optional(url.fragment()));
    data.insert("filename".into(), filename.into());
    data.insert("extension".into(), ext.to_lowercase().into());
    let referer = url.as_str().to_owned();
    Item::new(url, data)
        .ids(domain, path.clone())
        .header("Referer", &referer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http, keywords, media};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| matches(&Url::parse(raw).unwrap());
        for raw in [
            "https://en.wikipedia.org/static/images/project-logos/enwiki.png",
            "http://example.org/a/b/clip.MP4?x=1#frag",
            "https://example.org:8443/file.7z",
            "https://example.org/album/photo.jpe",
        ] {
            assert!(t(raw), "{raw}");
        }
        for raw in [
            "https://example.org/page.html",
            "https://example.org/",
            "https://example.org/.png",
            "https://example.org/image.png/",
            "https://u:p@example.org/a.png",
            "ftp://example.org/a.png",
        ] {
            assert!(!t(raw), "{raw}");
        }
        // Never claims top-level URLs, which keep the `direct` path.
        assert!(crate::sites::find(&Url::parse("https://example.org/a.png").unwrap()).is_none());
    }

    #[tokio::test]
    async fn extracts_one_file_with_upstream_keywords() {
        let http = http("directlink", Url::parse("http://127.0.0.1:9/").unwrap());
        let url = Url::parse("https://img.example.org/a%20b/c/My%20Pic.JPG?s=1#top").unwrap();
        let files = media(extract(&http, url)).await;
        assert_eq!(files.len(), 1);
        let k = keywords(&files[0]);
        assert_eq!(k["category"], "directlink");
        assert_eq!(k["domain"], "img.example.org");
        assert_eq!(k["path"], "a b/c");
        assert_eq!(k["filename"], "My Pic");
        assert_eq!(k["extension"], "jpg");
        assert_eq!(k["query"], "s=1");
        assert_eq!(k["fragment"], "top");
        assert_eq!(
            files[0].headers["Referer"],
            "https://img.example.org/a%20b/c/My%20Pic.JPG?s=1#top"
        );
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            file.render(&files[0]).unwrap(),
            "img.example.org_a b_c_My Pic.jpg"
        );
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("directlink")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert!(identity.contains("img.example.org"), "{identity}");
    }

    #[tokio::test]
    async fn queued_children_fall_back_to_direct_links() {
        let http = http("directlink", Url::parse("http://127.0.0.1:9/").unwrap());
        let url = Url::parse("https://cdn.example.net/x/y.webp").unwrap();
        assert!(crate::sites::supported(&url));
        let files = media(crate::sites::extract(&http, url)).await;
        assert_eq!(files.len(), 1);
        assert_eq!(keywords(&files[0])["path"], "x");
        let root = Url::parse("https://cdn.example.net/top.png").unwrap();
        let files = media(crate::sites::extract(&http, root)).await;
        assert_eq!(keywords(&files[0])["path"], "");
        // An empty path leaves upstream's doubled separator.
        let (file, _) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "cdn.example.net__top.png");
    }
}
