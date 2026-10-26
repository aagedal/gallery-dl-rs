//! ImageShack images, galleries and users, mirroring
//! gallery_dl/extractor/imageshack.py through `/rest_api/v2`.
use crate::error;
use crate::sites::gdl::{
    captures, date, ensure_http_scheme, ext_from_url, group, int_of, nameext_from_name,
    nameext_into, parse_ts, re, str_of,
};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://imageshack.com";
const FILENAME: &str = "{date:%Y-%m-%d} {id}{title:? - //[:230]}.{extension}";

static IMAGE: Route = Route::new(
    "image",
    FILENAME,
    &["{category}", "{owner[username]}"],
    "{id}",
);
static GALLERY: Route = Route::new(
    "gallery",
    FILENAME,
    &[
        "{category}",
        "{owner[username]}",
        "{gallery[title]} ({gallery[id]})",
    ],
    "{gallery[id]}/{id}",
);
static USER: Route = Route::new(
    "user",
    FILENAME,
    &["{category}", "{owner[username]}"],
    "{id}",
);

pub static SITE: Site = Site {
    category: "imageshack",
    name: "ImageShack",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&IMAGE, &GALLERY, &USER],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Image(String),
    Gallery(String),
    User(String),
}

fn target(url: &Url) -> Option<Target> {
    const BASE: &str = r"^https?://(?:www\.)?imageshack\.com";
    if let Some(c) = captures(re!(&format!(r"{BASE}/i/([^/?#]+)")), url) {
        return Some(Target::Image(group(&c, 1).into()));
    }
    if let Some(c) = captures(re!(&format!(r"{BASE}/a/([^/?#]+)")), url) {
        return Some(Target::Gallery(group(&c, 1).into()));
    }
    captures(re!(&format!(r"{BASE}/user/([^/?#]+)")), url)
        .map(|c| Target::User(group(&c, 1).into()))
}

async fn api(http: &Http, endpoint: &str, params: &[(&str, String)]) -> crate::Result<Value> {
    let mut url = http.api(&format!("{ROOT}/rest_api/{endpoint}"))?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    url.query_pairs_mut()
        .extend_pairs(params.iter().map(|(k, v)| (*k, v.as_str())))
        .append_pair("ts", &ts.to_string());
    let data = http
        .json_with(
            &url,
            &[
                ("Accept", "application/json, text/javascript, */*; q=0.01"),
                ("Content-Type", "application/json"),
                ("X-Requested-With", "XMLHttpRequest"),
                ("Sec-Fetch-Dest", "empty"),
                ("Sec-Fetch-Mode", "cors"),
                ("Sec-Fetch-Site", "same-origin"),
                ("Priority", "u=0"),
            ],
        )
        .await?;
    Ok(data["result"].clone())
}

fn image_item(image: &Value, extra: &Map<String, Value>) -> Option<Item> {
    let mut img = image.as_object()?.clone();
    img.insert(
        "date".into(),
        date(img.get("creation_date").and_then(parse_ts)),
    );
    let raw = ensure_http_scheme(str_of(image, "direct_link"));
    match image
        .get("original_filename")
        .and_then(Value::as_str)
        .filter(|n| !n.is_empty())
    {
        Some(name) => {
            let (stem, mut ext) = nameext_from_name(name);
            if ext.is_empty() {
                ext = ext_from_url(&raw);
            }
            img.insert("filename".into(), stem.into());
            img.insert("extension".into(), ext.into());
        }
        None => nameext_into(&raw, &mut img),
    }
    img.extend(extra.clone());
    let id = img.get("id").map(crate::sites::scalar).unwrap_or_default();
    Some(Item::new(Url::parse(&raw).ok()?, img).ids(id.clone(), id))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported ImageShack URL"));
    };
    let (endpoint, route, base): (String, &'static Route, Vec<(&'static str, String)>) =
        match target {
            Target::Image(id) => {
                return crate::sites::once(async move {
                    let image = api(http, &format!("/v2/images/{id}"), &[]).await?;
                    let items = image_item(&image, &Map::new()).into_iter().collect();
                    Ok(crate::sites::events(crate::sites::post(
                        &SITE, http, &IMAGE, items,
                    )))
                });
            }
            Target::Gallery(id) => (
                format!("/v2/albums/{id}"),
                &GALLERY,
                vec![
                    ("limit", "40".into()),
                    ("hide_empty", "true".into()),
                    ("show_private", "false".into()),
                ],
            ),
            Target::User(user) => (
                "/v2/images".into(),
                &USER,
                vec![
                    ("username", user),
                    ("limit", "40".into()),
                    ("hide_empty", "true".into()),
                ],
            ),
        };
    crate::sites::paginate(Some((0i64, None::<Value>)), move |state| {
        let endpoint = endpoint.clone();
        let base = base.clone();
        async move {
            let Some((offset, gallery)) = state else {
                return Ok(None);
            };
            let mut params = base.clone();
            let at = params
                .iter()
                .position(|(k, _)| *k == "limit")
                .map_or(params.len(), |i| i + 1);
            params.insert(at, ("offset", offset.to_string()));
            let data = api(http, &endpoint, &params).await?;
            let gallery = gallery.or_else(|| {
                (route.subcategory == "gallery").then(|| {
                    let mut info = Map::new();
                    for key in ["id", "title", "description", "owner"] {
                        info.insert(key.into(), data.get(key).cloned().unwrap_or(Value::Null));
                    }
                    info.insert(
                        "date".into(),
                        date(data.get("creation_date").and_then(parse_ts)),
                    );
                    Value::Object(info)
                })
            });
            let total = data.get("total").cloned().unwrap_or(Value::Null);
            let mut extra = Map::new();
            extra.insert("total".into(), total.clone());
            if let Some(gallery) = &gallery {
                extra.insert("gallery".into(), gallery.clone());
            }
            let events = data["images"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|image| image_item(image, &extra))
                .flat_map(|item| crate::sites::post(&SITE, http, route, vec![item]))
                .collect();
            let current = int_of(data.get("offset"));
            let next = (total.is_number() && current < int_of(Some(&total)))
                .then_some((offset + 40, gallery));
            Ok(Some((crate::sites::events(events), next)))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http as client, json, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://imageshack.com/i/iq8ac04fj"),
            Some(Target::Image("iq8ac04fj".into()))
        );
        assert_eq!(
            t("https://imageshack.com/a/OGtz/1"),
            Some(Target::Gallery("OGtz".into()))
        );
        assert_eq!(
            t("https://imageshack.com/user/Ezekiel.Gonzalez"),
            Some(Target::User("Ezekiel.Gonzalez".into()))
        );
    }

    #[tokio::test]
    async fn extracts_image() {
        let (root, server) = serve(vec![json(
            json!({"result": {"id": "iq8ac04fj", "title": "Sunset",
            "creation_date": 1500000000, "direct_link": "imagizer.imageshack.com/img922/1/ac04f.jpg",
            "original_filename": "sunset", "owner": {"username": "zeke"}}}),
        )]);
        let http = client("imageshack", root);
        let files = media(extract(
            &http,
            Url::parse("https://imageshack.com/i/iq8ac04fj").unwrap(),
        ))
        .await;
        assert_eq!(
            files[0].url.as_str(),
            "https://imagizer.imageshack.com/img922/1/ac04f.jpg"
        );
        assert_eq!(keywords(&files[0])["extension"], "jpg");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            file.render(&files[0]).unwrap(),
            "2017-07-14 iq8ac04fj - Sunset.jpg"
        );
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("imageshack/zeke")
        );
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with("GET /rest_api//v2/images/iq8ac04fj?ts="),
            "{}",
            requests[0]
        );
    }

    #[tokio::test]
    async fn pages_gallery() {
        let image = |id: &str| json!({"id": id, "creation_date": 1500000000, "direct_link": format!("https://imagizer.imageshack.com/{id}.png"), "original_filename": ""});
        let (root, server) = serve(vec![
            json(
                json!({"result": {"id": "G1", "title": "Album", "owner": {"username": "zeke"}, "total": 41, "offset": 0, "images": [image("a")]}}),
            ),
            json(json!({"result": {"total": 41, "offset": 40, "images": [image("b")]}})),
            json(json!({"result": {"total": 41, "offset": 80, "images": []}})),
        ]);
        let http = client("imageshack", root);
        let files = media(extract(
            &http,
            Url::parse("https://imageshack.com/a/G1").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(keywords(&files[1])["gallery"]["title"], "Album");
        let (_, identity) = crate::sites::archive_key(&files[1]).unwrap();
        assert_eq!(identity, r#"["{gallery[id]}/{id}",["G1","b"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /rest_api//v2/albums/G1?limit=40&offset=0&hide_empty=true&show_private=false&ts="), "{}", requests[0]);
        assert!(requests[1].contains("offset=40"));
    }
}
