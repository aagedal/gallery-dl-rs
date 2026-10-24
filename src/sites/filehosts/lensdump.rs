//! Lensdump albums, album lists and images, mirroring
//! gallery_dl/extractor/lensdump.py: listing pages carry each image's
//! `data-object` JSON and link the next page via `data-pagination="next"`.
use super::Extr;
use crate::sites::art::extr;
use crate::sites::manga::{Gallery, Page};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

pub static SITE: Site = Site {
    category: "lensdump",
    name: "Lensdump",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&IMAGE, &ALBUM, &ALBUMS],
    api_key: false,
    cookies: None,
};

static ALBUM: Route = Route::new(
    "album",
    "{category}_{gallery_id}_{num:>03}.{extension}",
    &["{category}", "{gallery_id} {title}"],
    "{gallery_id}_{num}",
);
static ALBUMS: Route = Route::new("albums", "{filename}.{extension}", &["{category}"], "");
static IMAGE: Route = Route::new(
    "image",
    "{category}_{id}{title:?_//}.{extension}",
    &["{category}"],
    "{id}",
);

const ROOT: &str = "https://lensdump.com";

#[derive(Debug, PartialEq)]
enum Target {
    /// Album ID and the query following it.
    Album(String, Option<String>),
    /// User name and the query following it.
    Albums(String, Option<String>),
    Image(String),
}

/// The leading `\w` run of `text`.
fn word(text: &str) -> &str {
    let len = text
        .bytes()
        .position(|b| !(b.is_ascii_alphanumeric() || b == b'_'))
        .unwrap_or(text.len());
    &text[..len]
}

/// The upstream classes in order: `lensdump.com/a/ID`, `lensdump.com/USER`
/// (not `a/` or `i/`), each with `(?:/?\?([^#]+))?`, then images on
/// `(?:i\d?\.)?lensdump\.com` or `\w\.l3n\.co` at `/(?:i/)?(\w+)`.
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) {
        return None;
    }
    let host = url.host_str()?;
    let path = url.path();
    // The query only counts when it directly follows the name.
    let query = |rest: &str| {
        matches!(rest, "" | "/")
            .then(|| url.query().filter(|q| !q.is_empty()).map(str::to_owned))
            .flatten()
    };
    if host == "lensdump.com" {
        if let Some(rest) = path.strip_prefix("/a/") {
            let id = word(rest);
            if !id.is_empty() {
                return Some(Target::Album(id.to_owned(), query(&rest[id.len()..])));
            }
        }
        let rest = &path[1..];
        if !rest.starts_with("a/") && !rest.starts_with("i/") {
            let user = rest.split('/').next().unwrap_or_default();
            if !user.is_empty() {
                return Some(Target::Albums(user.to_owned(), query(&rest[user.len()..])));
            }
        }
    }
    let image_host = match host.strip_suffix("lensdump.com") {
        Some(sub) => {
            let label = sub.strip_suffix('.').unwrap_or(sub);
            sub.is_empty()
                || (sub.ends_with('.')
                    && label
                        .strip_prefix('i')
                        .is_some_and(|n| n.len() <= 1 && n.bytes().all(|b| b.is_ascii_digit())))
        }
        None => host
            .strip_suffix(".l3n.co")
            .is_some_and(|label| label.len() == 1 && word(label) == label),
    };
    if !image_host {
        return None;
    }
    let rest = &path[1..];
    let key = word(rest.strip_prefix("i/").unwrap_or(rest));
    let key = if key.is_empty() { word(rest) } else { key };
    (!key.is_empty()).then(|| Target::Image(key.to_owned()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Album(id, query)) => crate::sites::once(async move {
            let url = match query {
                Some(query) => format!("{ROOT}/a/{id}/?{query}"),
                None => format!("{ROOT}/a/{id}"),
            };
            let page = http.text(&http.api(&url)?).await?;
            let mut data = Map::new();
            data.insert("gallery_id".into(), id.clone().into());
            let title = extr(&page, "property=\"og:title\" content=\"", "\"").trim();
            data.insert("title".into(), crate::sites::unescape(title).into());
            let mut pages = Vec::new();
            for item in pagination(http, page, " class=\"list-item ", ">").await? {
                pages.push(image_entry(&item));
            }
            let gallery = Gallery {
                enum_key: "num",
                ..Gallery::chapter(&ALBUM, data, pages).id(id).uncounted()
            };
            Ok(crate::sites::events(gallery.events(&SITE, http)))
        }),
        Some(Target::Albums(user, query)) => crate::sites::once(async move {
            let mut url = http.api(&format!("{ROOT}/{user}/"))?;
            match query {
                Some(query) => {
                    let pairs = crate::sites::art::parse_query(&query);
                    url.query_pairs_mut().extend_pairs(pairs);
                }
                None => {
                    url.query_pairs_mut()
                        .append_pair("sort", "date_asc")
                        .append_pair("page", "1");
                }
            }
            let page = http.text(&url).await?;
            let base = Url::parse(ROOT).map_err(|_| error("invalid Lensdump root"))?;
            let children = pagination(http, page, "data-url-short=\"", "\"")
                .await?
                .into_iter()
                .filter_map(|path| base.join(&path).ok())
                .filter(|url| matches!(target(url), Some(Target::Album(..))))
                .map(|url| (url, Map::new()))
                .collect();
            Ok(super::queue_all(
                http,
                SITE.category,
                ALBUMS.subcategory,
                children,
            ))
        }),
        Some(Target::Image(key)) => crate::sites::once(async move {
            let page = http.text(&http.api(&format!("{ROOT}/i/{key}"))?).await?;
            let item = parse_image(&page, &key)?;
            Ok(crate::sites::events(crate::sites::post(
                &SITE,
                http,
                &IMAGE,
                vec![item],
            )))
        }),
        None => crate::sites::fail(error("unsupported Lensdump URL")),
    }
}

/// `_pagination`: the `begin`/`end` matches of every page, following the
/// `data-pagination="next"` link.
async fn pagination(http: &Http, mut page: String, begin: &str, end: &str) -> Result<Vec<String>> {
    let base = Url::parse(ROOT).map_err(|_| error("invalid Lensdump root"))?;
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    loop {
        out.extend(
            super::extract_iter(&page, begin, end)
                .into_iter()
                .map(str::to_owned),
        );
        let next = extr(&page, " data-pagination=\"next\"", ">");
        if next.is_empty() {
            break;
        }
        let Some(url) = base
            .join(extr(next, "href=\"", "\""))
            .ok()
            .filter(|url| super::on_host(url, &["lensdump.com"]))
        else {
            break;
        };
        if !seen.insert(url.clone()) || seen.len() > 10_000 {
            break;
        }
        page = http.text(&http.api(url.as_str())?).await?;
    }
    Ok(out)
}

/// One `list-item` tag's `data-object` JSON as an album page.
fn image_entry(item: &str) -> Page {
    let raw = match extr(item, "data-object='", "'") {
        "" => extr(item, "data-object=\"", "\""),
        raw => raw,
    };
    let data: Value =
        serde_json::from_str(&crate::sites::percent_decode(raw)).unwrap_or(Value::Null);
    let get = |key: &str| data.get(key).cloned().unwrap_or(Value::Null);
    let url = get("url");
    let mut image = Map::new();
    image.insert("id".into(), get("name"));
    image.insert("url".into(), url.clone());
    let title = match get("title") {
        Value::String(title) => crate::sites::unescape(&title).into(),
        other => other,
    };
    image.insert("title".into(), title);
    image.insert("name".into(), get("filename"));
    image.insert("filename".into(), get("name"));
    image.insert("extension".into(), get("extension"));
    for key in ["width", "height"] {
        let value = crate::sites::manga::text::parse_int_value(data.get(key));
        image.insert(key.into(), value.into());
    }
    (url.as_str().unwrap_or_default().to_owned(), Some(image))
}

/// `LensdumpImageExtractor.items`.
fn parse_image(page: &str, key: &str) -> Result<Item> {
    let mut scan = Extr::new(page);
    let mut data = Map::new();
    data.insert("id".into(), key.into());
    let title = crate::sites::unescape(scan.next("property=\"og:title\" content=\"", "\""));
    data.insert("title".into(), title.clone().into());
    let url = scan
        .next("property=\"og:image\" content=\"", "\"")
        .to_owned();
    data.insert("url".into(), url.clone().into());
    for key in ["width", "height"] {
        let value = scan.next(&format!("property=\"image:{key}\" content=\""), "\"");
        data.insert(key.into(), super::parse_int(value).into());
    }
    let date = crate::sites::art::parse_iso(scan.next("<span title=\"", "\""));
    data.insert("date".into(), super::date_value(date));
    super::nameext_from_url(&url, &mut data);
    let file = super::web_url(&url).ok_or_else(|| error("Lensdump image has no file URL"))?;
    Ok(Item::new(file, data)
        .ids(key, key)
        .caption(Some(title))
        .timestamp(date))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, queued, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://lensdump.com/a/1IhJr"),
            Some(Target::Album("1IhJr".into(), None))
        );
        assert_eq!(
            t("https://lensdump.com/a/1IhJr/?page=2"),
            Some(Target::Album("1IhJr".into(), Some("page=2".into())))
        );
        assert_eq!(
            t("https://lensdump.com/vstar925/albums?x=1"),
            Some(Target::Albums("vstar925".into(), None))
        );
        assert_eq!(
            t("https://lensdump.com/vstar925?sort=date_desc"),
            Some(Target::Albums(
                "vstar925".into(),
                Some("sort=date_desc".into())
            ))
        );
        assert_eq!(
            t("https://lensdump.com/i/tyoAyM"),
            Some(Target::Image("tyoAyM".into()))
        );
        assert_eq!(
            t("https://i3.lensdump.com/i/tyoAyM.webp"),
            Some(Target::Image("tyoAyM".into()))
        );
        assert_eq!(
            t("https://c.l3n.co/i/tyoAyM.webp"),
            Some(Target::Image("tyoAyM".into()))
        );
        assert_eq!(
            t("https://i.lensdump.com/tyoAyM"),
            Some(Target::Image("tyoAyM".into()))
        );
        for raw in [
            "https://www.lensdump.com/a/x",
            "https://i12.lensdump.com/i/x",
            "https://ab.l3n.co/i/x",
            "https://lensdump.com.evil.test/i/x",
            "https://lensdump.com:8443/i/x",
            "https://lensdump.com/",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[tokio::test]
    async fn album_pages_carry_data_objects() {
        let first = "<meta property=\"og:title\" content=\" Trip &amp; Co \">\
            <div class=\"list-item c\" data-object='%7B%22name%22%3A%22AAA%22%2C%22url%22%3A%22https%3A%2F%2Fi.lensdump.com%2Fi%2FAAA.jpg%22%2C%22title%22%3A%22a%26amp%3Bb%22%2C%22filename%22%3A%22AAA.jpg%22%2C%22extension%22%3A%22jpg%22%2C%22width%22%3A%22640%22%2C%22height%22%3A480%7D'>\
            <a data-pagination=\"next\" href=\"/a/X1/?page=2\">";
        let second = "<div class=\"list-item c\" data-object=\"%7B%22name%22%3A%22BBB%22%2C%22url%22%3A%22https%3A%2F%2Fi.lensdump.com%2Fi%2FBBB.png%22%2C%22extension%22%3A%22png%22%7D\">";
        let (root, server) = serve(vec![html(first), html(second)]);
        let http = http("lensdump", root);
        let files = media(extract(
            &http,
            Url::parse("https://lensdump.com/a/X1").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[0]);
        assert_eq!(k["gallery_id"], "X1");
        assert_eq!(k["title"], "a&b");
        assert_eq!(k["filename"], "AAA");
        assert_eq!(k["name"], "AAA.jpg");
        assert_eq!(k["width"], 640);
        assert_eq!(k["height"], 480);
        assert!(!k.contains_key("count"));
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "lensdump_X1_002.png");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("lensdump/X1 a&b")
        );
        assert_eq!(
            crate::sites::archive_key(&files[1]).unwrap().1,
            r#"["{gallery_id}_{num}",["X1","2"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /a/X1 "));
        assert!(requests[1].starts_with("GET /a/X1/?page=2 "));
    }

    #[tokio::test]
    async fn albums_and_images() {
        let albums = "<a data-url-short=\"https://lensdump.com/a/A1\"><a data-url-short=\"/a/A2\">\
            <a data-url-short=\"https://evil.test/a/A3\">";
        let image = "<meta property=\"og:title\" content=\"Sun &amp; Sea\">\
            <meta property=\"og:image\" content=\"https://i3.lensdump.com/i/tyoAyM.webp\">\
            <meta property=\"image:width\" content=\"800\"><meta property=\"image:height\" content=\"600\">\
            <span title=\"2024-06-05 12:00:00\">";
        let (root, server) = serve(vec![html(albums), html(image)]);
        let http = http("lensdump", root);
        let children = queued(extract(
            &http,
            Url::parse("https://lensdump.com/vstar925").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].subcategory(), "albums");
        let files = media(extract(
            &http,
            Url::parse("https://lensdump.com/i/tyoAyM").unwrap(),
        ))
        .await;
        let k = keywords(&files[0]);
        assert_eq!(k["title"], "Sun & Sea");
        assert_eq!(k["width"], 800);
        assert_eq!(k["date"], "2024-06-05 12:00:00");
        let (file, _) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            file.render(&files[0]).unwrap(),
            "lensdump_tyoAyM_Sun & Sea.webp"
        );
        assert_eq!(
            crate::sites::archive_key(&files[0]).unwrap().1,
            r#"["{id}",["tyoAyM"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /vstar925/?sort=date_asc&page=1 "));
        assert!(requests[1].starts_with("GET /i/tyoAyM "));
    }
}
