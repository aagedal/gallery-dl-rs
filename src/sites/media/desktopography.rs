//! Desktopography exhibitions and wallpapers, mirroring
//! gallery_dl/extractor/desktopography.py: the front page lists yearly
//! exhibitions, each lists entries, and each entry links its resolutions.
use super::word_char;
use crate::error;
use crate::sites::art::{extract_iter, nameext_into};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://desktopography.net";
const FILENAME: &str = "{filename}.{extension}";
const DIRECTORY: &[&str] = &["{category}"];
static SITE_ROUTE: Route = Route::new("site", FILENAME, DIRECTORY, "{filename}");
static EXHIBITION: Route = Route::new("exhibition", FILENAME, DIRECTORY, "{filename}");
static ENTRY: Route = Route::new("entry", FILENAME, DIRECTORY, "{filename}");

pub static SITE: Site = Site {
    category: "desktopography",
    name: "Desktopography",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&SITE_ROUTE, &EXHIBITION, &ENTRY],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Site,
    Exhibition(String),
    Entry(String),
}

/// `desktopography\.net/$`, `/exhibition-([^/?#]+)/` and
/// `/portfolios/([\w-]+)`; upstream accepts no `www.`.
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) || url.host_str() != Some("desktopography.net") {
        return None;
    }
    let path = url.path();
    if path == "/" && url.query().is_none() && url.fragment().is_none() {
        return Some(Target::Site);
    }
    if let Some(rest) = path.strip_prefix("/exhibition-") {
        let (year, _) = rest.split_once('/')?;
        return (!year.is_empty()).then(|| Target::Exhibition(year.into()));
    }
    let rest = path.strip_prefix("/portfolios/")?;
    let len = rest
        .find(|c: char| !(word_char(c) || c == '-'))
        .unwrap_or(rest.len());
    (len > 0).then(|| Target::Entry(rest[..len].into()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Desktopography URL"));
    };
    crate::sites::once(async move {
        match target {
            Target::Site => {
                let page = http.text(&http.api(ROOT)?).await?;
                let begin = "<a href=\"https://desktopography.net/exhibition-";
                let children = extract_iter(&page, begin, "/\">")
                    .into_iter()
                    .map(|year| (format!("{ROOT}/exhibition-{year}/"), Map::new()))
                    .collect();
                Ok(queue_all(http, &SITE_ROUTE, children))
            }
            Target::Exhibition(year) => {
                let page = http
                    .text(&http.api(&format!("{ROOT}/exhibition-{year}/"))?)
                    .await?;
                let base = "https://desktopography.net/portfolios/";
                let begin = format!("<a class=\"overlay-background\" href=\"{base}");
                let mut data = Map::new();
                data.insert("year".into(), year.as_str().into());
                let children = extract_iter(&page, &begin, "\">")
                    .into_iter()
                    .map(|entry| (format!("{base}{entry}"), data.clone()))
                    .collect();
                Ok(queue_all(http, &EXHIBITION, children))
            }
            Target::Entry(entry) => {
                let page = http
                    .text(&http.api(&format!("{ROOT}/portfolios/{entry}"))?)
                    .await?;
                let items = entry_items(&page, &entry);
                Ok(crate::sites::events(crate::sites::post(
                    &SITE, http, &ENTRY, items,
                )))
            }
        }
    })
}

/// `DesktopographyEntryExtractor.items`: every wallpaper button, named by
/// its `download` attribute.
fn entry_items(page: &str, entry: &str) -> Vec<Item> {
    let begin = "<a target=\"_blank\" href=\"https://desktopography.net";
    let mut items = Vec::new();
    for link in extract_iter(page, begin, "\">") {
        let (path, filename) = link
            .split_once("\" class=\"wallpaper-button\" download=\"")
            .unwrap_or((link, ""));
        let mut data = Map::new();
        data.insert("entry".into(), entry.into());
        nameext_into(filename, &mut data);
        let Some(file) = super::web_url(&format!("{ROOT}{path}"))
            .filter(|url| url.host_str() == Some("desktopography.net"))
        else {
            continue;
        };
        let name = crate::sites::scalar(&data["filename"]);
        items.push(Item::new(file, data).ids(entry, name));
    }
    items
}

/// Queue the children that stay on this site's extractors.
fn queue_all<'a>(
    http: &'a Http,
    route: &'static Route,
    children: Vec<(String, Map<String, Value>)>,
) -> Events<'a> {
    let children: Vec<(Url, Map<String, Value>)> = children
        .into_iter()
        .filter_map(|(raw, data)| {
            let url = Url::parse(&raw).ok()?;
            target(&url).is_some().then_some((url, data))
        })
        .collect();
    futures_util::StreamExt::boxed(futures_util::StreamExt::flatten(
        futures_util::stream::iter(children.into_iter().map(move |(child, data)| {
            crate::sites::queue(http, "desktopography", route.subcategory, child, &data)
        })),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, queued, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(t("https://desktopography.net/"), Some(Target::Site));
        assert_eq!(
            t("https://desktopography.net/exhibition-2020/x"),
            Some(Target::Exhibition("2020".into()))
        );
        assert_eq!(
            t("http://desktopography.net/portfolios/new-dawn_2/"),
            Some(Target::Entry("new-dawn_2".into()))
        );
        for raw in [
            "https://desktopography.net/?x",
            "https://desktopography.net/exhibition-2020",
            "https://desktopography.net/exhibition-/",
            "https://desktopography.net/portfolios/",
            "https://www.desktopography.net/",
            "https://desktopography.net.evil.test/",
            "https://desktopography.net:8443/",
            "https://u:p@desktopography.net/",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[tokio::test]
    async fn site_and_exhibition_queue_children() {
        let (root, server) = serve(vec![
            html(
                "<a href=\"https://desktopography.net/exhibition-2020/\">\
                 <a href=\"https://desktopography.net/exhibition-2019/\">",
            ),
            html(
                "<a class=\"overlay-background\" href=\"https://desktopography.net/portfolios/dawn/\">\
                 <a class=\"overlay-background\" href=\"https://desktopography.net/portfolios/%2F\">",
            ),
        ]);
        let http = http("desktopography", root);
        let children = queued(extract(
            &http,
            Url::parse("https://desktopography.net/").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 2);
        let children = queued(extract(
            &http,
            Url::parse("https://desktopography.net/exhibition-2020/").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].subcategory(), "exhibition");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET / "));
        assert!(requests[1].starts_with("GET /exhibition-2020/ "));
    }

    #[tokio::test]
    async fn entry_lists_resolutions() {
        let page = "<a target=\"_blank\" href=\"https://desktopography.net/wp-content/uploads/dawn-1920x1080.jpg\" class=\"wallpaper-button\" download=\"dawn-1920x1080.jpg\">\
            <a target=\"_blank\" href=\"https://desktopography.net/wp-content/uploads/dawn-2560x1440.jpg\" class=\"wallpaper-button\" download=\"dawn-2560x1440.jpg\">";
        let (root, server) = serve(vec![html(page)]);
        let http = http("desktopography", root);
        let files = media(extract(
            &http,
            Url::parse("https://desktopography.net/portfolios/dawn/").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[1].url.as_str(),
            "https://desktopography.net/wp-content/uploads/dawn-2560x1440.jpg"
        );
        let k = keywords(&files[1]);
        assert_eq!(k["entry"], "dawn");
        assert_eq!(k["filename"], "dawn-2560x1440");
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "dawn-2560x1440.jpg");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("desktopography")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{filename}",["dawn-1920x1080"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /portfolios/dawn "));
    }
}
