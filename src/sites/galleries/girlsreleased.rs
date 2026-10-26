//! GirlsReleased sets, models and sites, mirroring
//! gallery_dl/extractor/girlsreleased.py: set images are queued for their
//! image hosts' extractors.
use crate::error;
use crate::sites::gdl::{captures, date, group, nameext_into, parse_ts, re};
use crate::sites::{Events, Http, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://girlsreleased.com";

static SET: Route = Route::new("set", "{filename}.{extension}", &["{category}"], "");
static MODEL: Route = Route::new("model", "{filename}.{extension}", &["{category}"], "");
static SITE_ROUTE: Route = Route::new("site", "{filename}.{extension}", &["{category}"], "");

pub static SITE: Site = Site {
    category: "girlsreleased",
    name: "GirlsReleased",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&SET, &MODEL, &SITE_ROUTE],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Set(String),
    /// `model` or `site`, and the path below it.
    List(&'static str, String),
}

fn target(url: &Url) -> Option<Target> {
    const BASE: &str = r"^https?://(?:www\.)?girlsreleased\.com";
    if let Some(c) = captures(re!(&format!(r"{BASE}/set/(\d+)")), url) {
        return Some(Target::Set(group(&c, 1).into()));
    }
    if let Some(c) = captures(re!(&format!(r"{BASE}/model/(\d+(?:/.+)?)")), url) {
        return Some(Target::List("model", group(&c, 1).into()));
    }
    captures(
        re!(&format!(r"{BASE}/site/([^/?#]+(?:/model/\d+/?.*)?)")),
        url,
    )
    .map(|c| Target::List("site", group(&c, 1).into()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported GirlsReleased URL"));
    };
    match target {
        Target::Set(id) => crate::sites::once(async move {
            let set = http
                .json(&http.api(&format!("{ROOT}/api/0.2/set/{id}"))?)
                .await?["set"]
                .clone();
            let images = set["images"].as_array().cloned().unwrap_or_default();
            let mut data = Map::new();
            let title = if crate::sites::gdl::truthy(set.get("name")) {
                set["name"].clone()
            } else {
                set["id"].clone()
            };
            data.insert("title".into(), title);
            data.insert("id".into(), set["id"].clone());
            data.insert("site".into(), set["site"].clone());
            let models: Vec<Value> = set["models"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|m| m[1].clone())
                .collect();
            data.insert("model".into(), models.into());
            data.insert("date".into(), date(set.get("date").and_then(parse_ts)));
            data.insert("count".into(), images.len().into());
            data.insert(
                "url".into(),
                format!("{ROOT}/set/{}", crate::sites::scalar(&set["id"])).into(),
            );
            let children = images
                .iter()
                .enumerate()
                .filter_map(|(index, image)| {
                    let mut fields = data.clone();
                    fields.insert("num".into(), (index + 1).into());
                    nameext_into(&crate::sites::scalar(&image[5]), &mut fields);
                    Some((Url::parse(image[3].as_str()?).ok()?, fields))
                })
                .collect();
            Ok(super::queue(http, &SITE, &SET, children))
        }),
        Target::List(kind, path) => {
            let route: &'static Route = if kind == "model" { &MODEL } else { &SITE_ROUTE };
            crate::sites::paginate(Some(0u64), move |state| {
                let path = path.clone();
                async move {
                    let Some(page) = state else { return Ok(None) };
                    let data = http
                        .json(&http.api(&format!("{ROOT}/api/0.2/sets/{kind}/{path}/page/{page}"))?)
                        .await?;
                    let sets = data["sets"].as_array().cloned().unwrap_or_default();
                    if sets.is_empty() {
                        return Ok(None);
                    }
                    let more = sets.len() >= 80;
                    let children = sets
                        .into_iter()
                        .skip(usize::from(page > 0))
                        .filter_map(|set| {
                            Some((
                                Url::parse(&format!(
                                    "{ROOT}/set/{}",
                                    crate::sites::scalar(&set[0])
                                ))
                                .ok()?,
                                Map::new(),
                            ))
                        })
                        .collect();
                    Ok(Some((
                        super::queue(http, &SITE, route, children),
                        more.then_some(page + 1),
                    )))
                }
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{children, http as client, json, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://girlsreleased.com/set/32332"),
            Some(Target::Set("32332".into()))
        );
        assert_eq!(
            t("https://girlsreleased.com/model/11545/Ariana%20Regent"),
            Some(Target::List("model", "11545/Ariana%20Regent".into()))
        );
        assert_eq!(
            t("https://girlsreleased.com/site/femjoy.com/model/854/Gabi"),
            Some(Target::List("site", "femjoy.com/model/854/Gabi".into()))
        );
    }

    #[tokio::test]
    async fn set_queues_image_host_links() {
        let (root, server) = serve(vec![json(
            json!({"set": {"id": "32332", "name": "", "site": "femjoy.com", "models": [[1, "Gabi"]], "date": 1500000000,
            "images": [[0, 0, 0, "https://imx.to/i/abc", 0, "https://imx.to/u/t/a.jpg"], [0, 0, 0, "https://pixhost.to/show/1/2_b.jpg", 0, "b.jpg"]]}}),
        )]);
        let http = client("girlsreleased", root);
        assert_eq!(
            children(extract(
                &http,
                Url::parse("https://girlsreleased.com/set/32332").unwrap()
            ))
            .await
            .len(),
            2
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn model_pages_skip_repeated_first_set() {
        let first: Vec<Value> = (0..80).map(|i| json!([format!("{i}")])).collect();
        let (root, server) = serve(vec![
            json(json!({"sets": first})),
            json(json!({"sets": [["79"], ["80"]]})),
        ]);
        let http = client("girlsreleased", root);
        assert_eq!(
            children(extract(
                &http,
                Url::parse("https://girlsreleased.com/model/11545/A").unwrap()
            ))
            .await
            .len(),
            81
        );
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /api/0.2/sets/model/11545/A/page/1 "));
    }
}
