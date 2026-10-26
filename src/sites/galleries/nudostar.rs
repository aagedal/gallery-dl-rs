//! NudoStar models and images, mirroring gallery_dl/extractor/nudostar.py.
use crate::error;
use crate::sites::gdl::{captures, extr, group, nameext_into, parse_int, re, rpartition};
use crate::sites::manga::{GALLERY_ARCHIVE, GALLERY_FILENAME};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://nudostar.tv";
const BASE: &str = r"^https?://(?:[a-z]{2}.)?nudostar\.tv";
const DIRECTORY: &[&str] = &["{category}", "{gallery_id} {title}"];

static MODEL: Route = Route::new("model", GALLERY_FILENAME, DIRECTORY, GALLERY_ARCHIVE);
static IMAGE: Route = Route::new("image", GALLERY_FILENAME, DIRECTORY, GALLERY_ARCHIVE);

pub static SITE: Site = Site {
    category: "nudostar",
    name: "NudoStar",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&MODEL, &IMAGE],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Model(String, String),
    Image(String, String, String),
}

fn target(url: &Url) -> Option<Target> {
    if let Some(c) = captures(re!(&format!(r"{BASE}(/models/([^/?#]+)/?)$")), url) {
        return Some(Target::Model(group(&c, 1).into(), group(&c, 2).into()));
    }
    captures(re!(&format!(r"{BASE}(/models/([^/?#]+)/(\d+)/)")), url).map(|c| {
        Target::Image(
            group(&c, 1).into(),
            group(&c, 2).into(),
            group(&c, 3).into(),
        )
    })
}

/// `NudostarModelExtractor.metadata`.
fn metadata(page: &str, slug: &str) -> Map<String, Value> {
    let names: Vec<Value> = rpartition(extr(page, "<title>", "<"), " Nude ")
        .0
        .split(" / ")
        .map(Value::from)
        .collect();
    let mut data = Map::new();
    data.insert("gallery_id".into(), slug.into());
    data.insert("model_slug".into(), slug.into());
    data.insert("model".into(), names[0].clone());
    data.insert("model_names".into(), names.into());
    data.insert("title".into(), "".into());
    data
}

/// `NudostarModelExtractor.images`: numbered from the first thumbnail.
fn images(page: &str) -> Vec<(String, Option<Map<String, Value>>)> {
    let path = extr(page, "\" src=\"https://nudostar.tv", "\"");
    let mut parts = path.rsplitn(3, '/');
    let (Some(rhs), Some(_), Some(lhs)) = (parts.next(), parts.next(), parts.next()) else {
        return Vec::new();
    };
    let mut pieces = rhs.rsplitn(3, '_');
    let (Some(end), Some(count), Some(name)) = (pieces.next(), pieces.next(), pieces.next()) else {
        return Vec::new();
    };
    let ext = format!(".{}", rpartition(end, ".").2);
    (1..=parse_int(count))
        .map(|i| {
            (
                format!("{ROOT}{lhs}/{}000/{name}_{i:04}{ext}", i / 1000 + 1),
                None,
            )
        })
        .collect()
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported NudoStar URL"));
    };
    crate::sites::once(async move {
        match target {
            Target::Model(path, slug) => {
                let page = http.text(&http.api(&format!("{ROOT}{path}"))?).await?;
                let data = metadata(&page, &slug);
                Ok(crate::sites::events(super::numbered(
                    &SITE,
                    http,
                    &MODEL,
                    &data,
                    images(&page),
                    &slug,
                )))
            }
            Target::Image(path, slug, num) => {
                let page = http.text(&http.api(&format!("{ROOT}{path}"))?).await?;
                let start = page
                    .find("class=\"headline\"")
                    .ok_or_else(|| error("NudoStar image not found"))?;
                let file = extr(&page[start..], "src=\"", "\"").to_owned();
                let mut data = metadata(&page, &slug);
                nameext_into(&file, &mut data);
                data.insert("num".into(), parse_int(&num).into());
                data.insert("url".into(), file.as_str().into());
                let file = Url::parse(&file).map_err(|_| error("NudoStar image has no file"))?;
                Ok(crate::sites::events(crate::sites::post(
                    &SITE,
                    http,
                    &IMAGE,
                    vec![Item::new(file, data).ids(slug, num)],
                )))
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://nudostar.tv/models/jane/"),
            Some(Target::Model("/models/jane/".into(), "jane".into()))
        );
        assert_eq!(
            t("https://es.nudostar.tv/models/jane/12/"),
            Some(Target::Image(
                "/models/jane/12/".into(),
                "jane".into(),
                "12".into()
            ))
        );
    }

    #[test]
    fn numbers_images_from_thumbnail() {
        let page =
            r#"<img alt="x" src="https://nudostar.tv/contents/j/jane/1000/jane_1001_thumb.jpg">"#;
        let found = images(page);
        assert_eq!(found.len(), 1001);
        assert_eq!(
            found[0].0,
            "https://nudostar.tv/contents/j/jane/1000/jane_0001.jpg"
        );
        assert_eq!(
            found[1000].0,
            "https://nudostar.tv/contents/j/jane/2000/jane_1001.jpg"
        );
        let data = metadata("<title>Jane / Janie Nude OnlyFans</title>", "jane");
        assert_eq!(data["model"], "Jane");
        assert_eq!(data["model_names"], serde_json::json!(["Jane", "Janie"]));
    }
}
