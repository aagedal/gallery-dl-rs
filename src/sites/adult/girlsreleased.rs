//! Girlsreleased sets, models and sites, mirroring
//! gallery_dl/extractor/girlsreleased.py: the site's JSON API describes a
//! set, whose images are hosted elsewhere and queued for their host's
//! extractor; model and site listings queue each set, 80 per page.
use super::Interval;
use crate::sites::art::{date, nameext_into, on_host, parse_ts};
use crate::sites::{Events, Http, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://girlsreleased.com";
static SET: Route = Route::new("set", "{filename}.{extension}", &["{category}"], "");
static MODEL: Route = Route::new("model", "{filename}.{extension}", &["{category}"], "");
static SITE_ROUTE: Route = Route::new("site", "{filename}.{extension}", &["{category}"], "");

/// Upstream `request_interval = 0.5`.
static INTERVAL: Interval = Interval::new(500);

pub static SITE: Site = Site {
    category: "girlsreleased",
    name: "Girlsreleased",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&SET, &MODEL, &SITE_ROUTE],
    api_key: false,
    cookies: None,
};

#[derive(Debug)]
enum Target {
    Set(String),
    /// A listing's route and its path below `/model/` or `/site/`, raw.
    Listing(&'static Route, String),
}

/// `/set/(\d+)`, `/model/(\d+(?:/.+)?)` and
/// `/site/([^/?#]+(?:/model/\d+/?.*)?)`, all unanchored. Listing paths keep
/// only the URL's path.
fn target(url: &Url) -> Option<Target> {
    if !on_host(url, &["girlsreleased.com"]) {
        return None;
    }
    let path = url.path();
    let digits = crate::sites::art::leading_digits;
    if let Some(id) = path.strip_prefix("/set/").and_then(digits) {
        return Some(Target::Set(id.to_owned()));
    }
    if let Some(rest) = path.strip_prefix("/model/") {
        let id = digits(rest)?;
        let tail = &rest[id.len()..];
        let path = match tail.len() > 1 && tail.starts_with('/') {
            true => rest,
            false => id,
        };
        return Some(Target::Listing(&MODEL, path.to_owned()));
    }
    let rest = path.strip_prefix("/site/")?;
    let name = &rest[..rest.find('/').unwrap_or(rest.len())];
    if name.is_empty() {
        return None;
    }
    let tail = &rest[name.len()..];
    let path = match tail.strip_prefix("/model/").and_then(digits) {
        Some(_) => rest,
        None => name,
    };
    Some(Target::Listing(&SITE_ROUTE, path.to_owned()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Set(id)) => crate::sites::once(set(http, id)),
        Some(Target::Listing(route, path)) => listing(http, route, path),
        None => crate::sites::fail(error("unsupported Girlsreleased URL")),
    }
}

/// `GirlsreleasedSetExtractor.items`: each image's host page, queued with
/// the set's metadata.
async fn set(http: &Http, id: String) -> Result<Events<'_>> {
    INTERVAL.wait().await;
    let url = http.api(&format!("{ROOT}/api/0.2/set/{id}"))?;
    let json = http.json_with(&url, &http.browser_headers(&[])).await?;
    let set = &json["set"];
    let images = set["images"].as_array().cloned().unwrap_or_default();
    let set_id = crate::sites::scalar(&set["id"]);
    let title = match &set["name"] {
        name if crate::sites::art::truthy(Some(name)) => name.clone(),
        _ => set["id"].clone(),
    };
    let models: Vec<Value> = set["models"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|model| model[1].clone())
        .collect();
    let mut data = Map::new();
    data.insert("title".into(), title);
    data.insert("id".into(), set["id"].clone());
    data.insert("site".into(), set["site"].clone());
    data.insert("model".into(), models.into());
    data.insert("date".into(), date(parse_ts(&set["date"])));
    data.insert("count".into(), images.len().into());
    data.insert("url".into(), format!("{ROOT}/set/{set_id}").into());
    let children = images
        .iter()
        .enumerate()
        .filter_map(|(index, image)| {
            let host_page = crate::sites::filehosts::web_url(image[3].as_str()?)?;
            let mut data = data.clone();
            data.insert("num".into(), (index + 1).into());
            let name = image[5].as_str().unwrap_or_default();
            nameext_into(name, &mut data);
            Some((host_page, data))
        })
        .collect();
    Ok(crate::sites::filehosts::queue_all(
        http,
        SITE.category,
        SET.subcategory,
        children,
    ))
}

/// `GirlsreleasedExtractor._pagination`: the sets of each API page (the
/// first entry of later pages repeats the previous page's last), until a
/// page has fewer than 80.
fn listing<'a>(http: &'a Http, route: &'static Route, path: String) -> Events<'a> {
    let base = format!("{ROOT}/api/0.2/sets/{}/{path}/page/", route.subcategory);
    crate::sites::paginate(Some(0usize), move |state| {
        let base = base.clone();
        async move {
            let Some(pnum) = state else { return Ok(None) };
            INTERVAL.wait().await;
            let url = http.api(&format!("{base}{pnum}"))?;
            let json = http.json_with(&url, &http.browser_headers(&[])).await?;
            let sets = json["sets"].as_array().cloned().unwrap_or_default();
            let next = (sets.len() >= 80 && pnum < super::MAX_PAGES).then_some(pnum + 1);
            let children = sets
                .iter()
                .skip(usize::from(pnum > 0))
                .filter_map(|set| {
                    let id = crate::sites::scalar(set.get(0)?);
                    Some((Url::parse(&format!("{ROOT}/set/{id}")).ok()?, Map::new()))
                })
                .collect();
            Ok(Some((
                crate::sites::manga::queue_urls(http, &SITE, route, children),
                next,
            )))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http, json, queued, serve};
    use serde_json::json;

    impl PartialEq for Target {
        fn eq(&self, other: &Self) -> bool {
            match (self, other) {
                (Self::Set(a), Self::Set(b)) => a == b,
                (Self::Listing(r, p), Self::Listing(s, o)) => std::ptr::eq(*r, *s) && p == o,
                _ => false,
            }
        }
    }

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://girlsreleased.com/set/12345"),
            Some(Target::Set("12345".into()))
        );
        assert_eq!(
            t("https://www.girlsreleased.com/model/123/Some-Name?x"),
            Some(Target::Listing(&MODEL, "123/Some-Name".into()))
        );
        assert_eq!(
            t("https://girlsreleased.com/model/123/"),
            Some(Target::Listing(&MODEL, "123".into()))
        );
        assert_eq!(
            t("https://girlsreleased.com/site/some.site/model/5/name"),
            Some(Target::Listing(
                &SITE_ROUTE,
                "some.site/model/5/name".into()
            ))
        );
        assert_eq!(
            t("https://girlsreleased.com/site/some.site/other"),
            Some(Target::Listing(&SITE_ROUTE, "some.site".into()))
        );
        for raw in [
            "https://girlsreleased.com/set/x",
            "https://girlsreleased.com/model/x",
            "https://girlsreleased.com/site/",
            "https://m.girlsreleased.com/set/1",
            "https://girlsreleased.com.evil.test/set/1",
            "https://girlsreleased.com:8443/set/1",
            "https://u:p@girlsreleased.com/set/1",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[tokio::test]
    async fn set_queues_hosted_images() {
        let set = json!({"set": {"id": "12345", "name": "", "site": "studio.test",
            "models": [[1, "Model A"], [2, "Model B"]], "date": 1700000000,
            "images": [[0, 0, 0, "https://imx.to/i/abc", 0, "https://i.imx.to/i/2024/01/01/abc.jpg"],
                       [0, 0, 0, "https://imx.to/i/def", 0, "https://i.imx.to/i/2024/01/01/def.jpg"]]}});
        let (root, server) = serve(vec![json(set)]);
        let http = http("girlsreleased", root);
        let children = queued(extract(
            &http,
            Url::parse("https://girlsreleased.com/set/12345").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 2);
        assert_eq!(children[1].category(), "girlsreleased");
        assert_eq!(children[1].subcategory(), "set");
        assert_eq!(children[1].fields()["id"], "12345");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/0.2/set/12345 "));
    }

    #[tokio::test]
    async fn model_pages_skip_repeated_sets() {
        let first: Vec<Value> = (0..80).map(|i| json!([i.to_string(), "x"])).collect();
        let second = json!({"sets": [["79", "x"], ["80", "y"]]});
        let (root, server) = serve(vec![json(json!({"sets": first})), json(second)]);
        let http = http("girlsreleased", root);
        let children = queued(extract(
            &http,
            Url::parse("https://girlsreleased.com/model/123/Some-Name").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 81);
        assert_eq!(children[80].subcategory(), "model");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/0.2/sets/model/123/Some-Name/page/0 "));
        assert!(requests[1].starts_with("GET /api/0.2/sets/model/123/Some-Name/page/1 "));
    }
}
