//! Desktopography exhibitions and wallpaper entries, mirroring
//! gallery_dl/extractor/desktopography.py.
use crate::error;
use crate::sites::gdl::{captures, extract_iter, group, nameext_into, partition, re};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::Map;
use url::Url;

const ROOT: &str = "https://desktopography.net";
const BASE: &str = r"^https?://desktopography\.net";

static SITE_ROUTE: Route = Route::new(
    "site",
    "{filename}.{extension}",
    &["{category}"],
    "{filename}",
);
static EXHIBITION: Route = Route::new(
    "exhibition",
    "{filename}.{extension}",
    &["{category}"],
    "{filename}",
);
static ENTRY: Route = Route::new(
    "entry",
    "{filename}.{extension}",
    &["{category}"],
    "{filename}",
);

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

fn target(url: &Url) -> Option<Target> {
    if captures(re!(&format!(r"{BASE}/$")), url).is_some() {
        return Some(Target::Site);
    }
    if let Some(c) = captures(re!(&format!(r"{BASE}/exhibition-([^/?#]+)/")), url) {
        return Some(Target::Exhibition(group(&c, 1).into()));
    }
    captures(re!(&format!(r"{BASE}/portfolios/([\w-]+)")), url)
        .map(|c| Target::Entry(group(&c, 1).into()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Desktopography URL"));
    };
    crate::sites::once(async move {
        match target {
            Target::Site => {
                let page = http.text(&http.api(ROOT)?).await?;
                let children = extract_iter(
                    &page,
                    "<a href=\"https://desktopography.net/exhibition-",
                    "/\">",
                )
                .into_iter()
                .filter_map(|year| {
                    Some((
                        Url::parse(&format!("{ROOT}/exhibition-{year}/")).ok()?,
                        Map::new(),
                    ))
                })
                .collect();
                Ok(crate::sites::galleries::queue(
                    http,
                    &SITE,
                    &SITE_ROUTE,
                    children,
                ))
            }
            Target::Exhibition(year) => {
                let page = http
                    .text(&http.api(&format!("{ROOT}/exhibition-{year}/"))?)
                    .await?;
                let base = "https://desktopography.net/portfolios/";
                let mut data = Map::new();
                data.insert("year".into(), year.as_str().into());
                let children = extract_iter(
                    &page,
                    &format!("<a class=\"overlay-background\" href=\"{base}"),
                    "\">",
                )
                .into_iter()
                .filter_map(|entry| {
                    Some((Url::parse(&format!("{base}{entry}")).ok()?, data.clone()))
                })
                .collect();
                Ok(crate::sites::galleries::queue(
                    http,
                    &SITE,
                    &EXHIBITION,
                    children,
                ))
            }
            Target::Entry(entry) => {
                let page = http
                    .text(&http.api(&format!("{ROOT}/portfolios/{entry}"))?)
                    .await?;
                let items = extract_iter(
                    &page,
                    "<a target=\"_blank\" href=\"https://desktopography.net",
                    "\">",
                )
                .into_iter()
                .filter_map(|image| {
                    let (path, _, filename) =
                        partition(image, "\" class=\"wallpaper-button\" download=\"");
                    let mut data = Map::new();
                    data.insert("entry".into(), entry.as_str().into());
                    nameext_into(filename, &mut data);
                    let name = crate::sites::scalar(&data["filename"]);
                    Some(
                        Item::new(Url::parse(&format!("{ROOT}{path}")).ok()?, data)
                            .ids(entry.clone(), name),
                    )
                })
                .collect();
                Ok(crate::sites::events(crate::sites::post(
                    &SITE, http, &ENTRY, items,
                )))
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http as client, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(t("https://desktopography.net/"), Some(Target::Site));
        assert_eq!(
            t("https://desktopography.net/exhibition-2020/"),
            Some(Target::Exhibition("2020".into()))
        );
        assert_eq!(
            t("https://desktopography.net/portfolios/new-era/"),
            Some(Target::Entry("new-era".into()))
        );
    }

    #[tokio::test]
    async fn entry_lists_resolutions() {
        let page = r#"<a target="_blank" href="https://desktopography.net/wp-content/uploads/a-1920x1080.jpg" class="wallpaper-button" download="new-era-1920x1080.jpg">"#;
        let (root, server) = serve(vec![html(page)]);
        let http = client("desktopography", root);
        let files = media(extract(
            &http,
            Url::parse("https://desktopography.net/portfolios/new-era/").unwrap(),
        ))
        .await;
        assert_eq!(
            files[0].url.as_str(),
            "https://desktopography.net/wp-content/uploads/a-1920x1080.jpg"
        );
        assert_eq!(keywords(&files[0])["filename"], "new-era-1920x1080");
        server.join().unwrap();
    }
}
