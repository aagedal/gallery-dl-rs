//! ImageShack images, albums and user uploads, mirroring
//! gallery_dl/extractor/imageshack.py through the site's `rest_api`.
//!
//! Not ported: upstream `skip_posts` fast-forwarding of the API offset.
use super::{date, path_segment, set_param, str_of, ts_value};
use crate::sites::art::{ext_from_url, nameext_from_name, nameext_into, update};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://imageshack.com";
const FILENAME: &str = "{date:%Y-%m-%d} {id}{title:? - //[:230]}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{owner[username]}"];
static IMAGE: Route = Route::new("image", FILENAME, DIRECTORY, "{id}");
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
static USER: Route = Route::new("user", FILENAME, DIRECTORY, "{id}");

pub static SITE: Site = Site {
    category: "imageshack",
    name: "ImageShack",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&IMAGE, &GALLERY, &USER],
    api_key: false,
    cookies: None,
};

const HEADERS: &[(&str, &str)] = &[
    ("Accept", "application/json, text/javascript, */*; q=0.01"),
    ("Content-Type", "application/json"),
    ("X-Requested-With", "XMLHttpRequest"),
    ("Sec-Fetch-Dest", "empty"),
    ("Sec-Fetch-Mode", "cors"),
    ("Sec-Fetch-Site", "same-origin"),
    ("Priority", "u=0"),
];

#[derive(Debug, PartialEq)]
enum Target {
    Image(String),
    Gallery(String),
    User(String),
}

/// `(?:www\.)?imageshack\.com/(i|a|user)/([^/?#]+)`.
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::art::on_host(url, &["imageshack.com"]) {
        return None;
    }
    let parts: Vec<&str> = url.path()[1..].split('/').collect();
    match parts.as_slice() {
        ["i", id, ..] if path_segment(id) => Some(Target::Image((*id).into())),
        ["a", id, ..] if path_segment(id) => Some(Target::Gallery((*id).into())),
        ["user", name, ..] if path_segment(name) => Some(Target::User((*name).into())),
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported ImageShack URL"));
    };
    match target {
        Target::Image(id) => crate::sites::once(async move {
            let image = request_api(http, &format!("/v2/images/{id}"), Vec::new()).await?;
            Ok(images_events(http, &IMAGE, vec![image], &Map::new()))
        }),
        Target::Gallery(id) => paginate(
            http,
            &GALLERY,
            format!("/v2/albums/{id}"),
            vec![
                ("limit".into(), "40".into()),
                ("offset".into(), "0".into()),
                ("hide_empty".into(), "true".into()),
                ("show_private".into(), "false".into()),
            ],
        ),
        Target::User(user) => paginate(
            http,
            &USER,
            "/v2/images".into(),
            vec![
                ("username".into(), crate::sites::percent_decode(&user)),
                ("limit".into(), "40".into()),
                ("offset".into(), "0".into()),
                ("hide_empty".into(), "true".into()),
            ],
        ),
    }
}

/// `request_api`: the `result` of `/rest_api/ENDPOINT` with a `ts` stamp.
/// Endpoints start with `/`, so the path has upstream's double slash.
async fn request_api(
    http: &Http,
    endpoint: &str,
    mut params: Vec<(String, String)>,
) -> Result<Value> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    set_param(&mut params, "ts", ts.to_string());
    let mut url = http.api(&format!("{ROOT}/rest_api/{endpoint}"))?;
    url.query_pairs_mut().extend_pairs(&params);
    let body = http
        .json_with(&url, HEADERS)
        .await
        .map_err(|e| error(format!("ImageShack API request failed: {e}")))?;
    body.get("result")
        .cloned()
        .ok_or_else(|| error("ImageShack API response has no result"))
}

/// `_pagination`: advance `offset` by `limit` until the reported offset
/// reaches `total`. Album pages also set the `gallery` keyword.
fn paginate<'a>(
    http: &'a Http,
    route: &'static Route,
    endpoint: String,
    params: Vec<(String, String)>,
) -> Events<'a> {
    crate::sites::paginate(Some((params, Map::new())), move |state| {
        let endpoint = endpoint.clone();
        async move {
            let Some((mut params, mut kwdict)) = state else {
                return Ok(None);
            };
            let data = request_api(http, &endpoint, params.clone()).await?;
            if route.subcategory == "gallery" && !kwdict.contains_key("gallery") {
                kwdict.insert("gallery".into(), gallery(&data).into());
            }
            kwdict.insert(
                "total".into(),
                data.get("total").cloned().unwrap_or(Value::Null),
            );
            let images = data
                .get("images")
                .and_then(Value::as_array)
                .cloned()
                .ok_or_else(|| error("ImageShack API response has no images"))?;
            let offset = data.get("offset").and_then(Value::as_i64);
            let total = data.get("total").and_then(Value::as_i64);
            // An empty page would request the same window forever.
            let next = match (offset, total) {
                (Some(offset), Some(total)) if offset < total && !images.is_empty() => {
                    let number = |key| {
                        params
                            .iter()
                            .find(|(k, _)| k == key)
                            .and_then(|(_, v)| v.parse::<i64>().ok())
                            .unwrap_or(0)
                    };
                    let offset = number("offset") + number("limit");
                    set_param(&mut params, "offset", offset.to_string());
                    Some((params, kwdict.clone()))
                }
                _ => None,
            };
            Ok(Some((images_events(http, route, images, &kwdict), next)))
        }
    })
}

/// `_parse_gallery`: the album fields of the first page.
fn gallery(data: &Value) -> Map<String, Value> {
    let mut gallery = Map::new();
    for key in ["id", "title", "description", "owner"] {
        gallery.insert(key.into(), data.get(key).cloned().unwrap_or(Value::Null));
    }
    let created = ts_value(data.get("creation_date"));
    gallery.insert("date".into(), date(created));
    gallery
}

/// `items()`: one post per image, with extractor keywords applied last.
fn images_events<'a>(
    http: &'a Http,
    route: &'static Route,
    images: Vec<Value>,
    kwdict: &Map<String, Value>,
) -> Events<'a> {
    let mut events = Vec::new();
    for image in images {
        let Value::Object(mut img) = image else {
            continue;
        };
        let ts = ts_value(img.get("creation_date"));
        img.insert("date".into(), date(ts));
        let raw = ensure_http_scheme(str_of(&Value::Object(img.clone()), "direct_link"));
        let name = str_of(&Value::Object(img.clone()), "original_filename").to_owned();
        if name.is_empty() {
            nameext_into(&raw, &mut img);
        } else {
            super::set_nameext(&mut img, nameext_from_name(&name));
            if str_of(&Value::Object(img.clone()), "extension").is_empty() {
                img.insert("extension".into(), ext_from_url(&raw).into());
            }
        }
        update(&mut img, kwdict);
        let Some(url) = crate::sites::media_url(&raw, &["imageshack.com", "imageshack.us"]) else {
            continue;
        };
        let id = img.get("id").map(crate::sites::scalar).unwrap_or_default();
        let owner = img
            .get("owner")
            .map(|owner| str_of(owner, "username").to_owned());
        let title = img.get("title").and_then(Value::as_str).map(str::to_owned);
        let item = Item::new(url, img)
            .ids(id.clone(), id)
            .user(owner)
            .caption(title)
            .timestamp(ts);
        events.extend(crate::sites::post(&SITE, http, route, vec![item]));
    }
    crate::sites::events(events)
}

/// `text.ensure_http_scheme`.
fn ensure_http_scheme(url: &str) -> String {
    if !url.is_empty() && !url.starts_with("https://") && !url.starts_with("http://") {
        format!("https://{}", url.trim_start_matches(['/', ':']))
    } else {
        url.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http, json, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://imageshack.com/i/pmdpUGMNj"),
            Some(Target::Image("pmdpUGMNj".into()))
        );
        assert_eq!(
            t("https://www.imageshack.com/a/img922/x?y"),
            Some(Target::Gallery("img922".into()))
        );
        assert_eq!(
            t("http://imageshack.com/user/Jane"),
            Some(Target::User("Jane".into()))
        );
        for raw in [
            "https://imageshack.com/",
            "https://imageshack.com/i/",
            "https://imageshack.com/users/Jane",
            "https://imagizer.imageshack.com/i/abc",
            "https://imageshack.com.evil.test/i/abc",
            "https://evilimageshack.com/i/abc",
            "https://imageshack.com:8080/i/abc",
            "https://u:p@imageshack.com/i/abc",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn image(id: u64, name: &str) -> Value {
        json!({"id": format!("i{id}"), "title": "Sunset", "creation_date": 1_600_000_000 + id,
            "original_filename": name, "owner": {"username": "jane"},
            "direct_link": format!("imagizer.imageshack.com/img922/{id}/abc{id}.png")})
    }

    #[tokio::test]
    async fn gallery_paginates_by_offset() {
        let (root, server) = serve(vec![
            json(
                json!({"success": true, "result": {"id": "A1", "title": "Trip",
                "description": "d", "owner": {"username": "jane"}, "creation_date": 1_500_000_000,
                "offset": 0, "total": 2, "images": [image(1, "first.JPG")]}}),
            ),
            json(json!({"result": {"id": "A1", "offset": 40, "total": 2,
                "images": [image(2, "")]}})),
        ]);
        let http = http("imageshack", root);
        let files = media(extract(
            &http,
            Url::parse("https://imageshack.com/a/A1").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].url.as_str(),
            "https://imagizer.imageshack.com/img922/1/abc1.png"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["filename"], "first");
        assert_eq!(k["extension"], "jpg");
        assert_eq!(k["total"], 2);
        assert_eq!(k["gallery"]["title"], "Trip");
        assert_eq!(k["gallery"]["date"], "2017-07-14 02:40:00");
        let k = keywords(&files[1]);
        assert_eq!(k["filename"], "abc2");
        assert_eq!(k["gallery"]["id"], "A1");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            file.render(&files[0]).unwrap(),
            "2020-09-13 i1 - Sunset.jpg"
        );
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("imageshack/jane/Trip (A1)")
        );
        let (_, identity) = crate::sites::archive_key(&files[1]).unwrap();
        assert_eq!(identity, r#"["{gallery[id]}/{id}",["A1","i2"]]"#);
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with(
                "GET /rest_api//v2/albums/A1?limit=40&offset=0&hide_empty=true&show_private=false&ts="
            ),
            "{}",
            requests[0]
        );
        assert!(requests[0].contains("x-requested-with: XMLHttpRequest"));
        assert!(requests[1].contains("&offset=40&"));
    }

    #[tokio::test]
    async fn image_and_user() {
        let (root, server) = serve(vec![
            json(json!({"result": image(3, "noext")})),
            json(json!({"result": {"offset": 0, "total": 1, "images": [image(4, "x.gif")]}})),
            // Upstream stops only once the reported offset reaches the total.
            json(json!({"result": {"offset": 40, "total": 1, "images": []}})),
        ]);
        let http = http("imageshack", root);
        let files = media(extract(
            &http,
            Url::parse("https://imageshack.com/i/i3").unwrap(),
        ))
        .await;
        let k = keywords(&files[0]);
        assert_eq!(k["filename"], "noext");
        assert_eq!(k["extension"], "png");
        assert!(!k.contains_key("total"));
        let (dir_file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            dir_file.render(&files[0]).unwrap(),
            "2020-09-13 i3 - Sunset.png"
        );
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("imageshack/jane")
        );
        let files = media(extract(
            &http,
            Url::parse("https://imageshack.com/user/jane").unwrap(),
        ))
        .await;
        assert_eq!(keywords(&files[0])["subcategory"], "user");
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{id}",["i4"]]"#);
        // `{title:? - //[:230]}` cuts long titles.
        let mut long = files[0].clone();
        let metadata = long.metadata.as_mut().unwrap();
        let mut keywords = (**metadata.site_keywords.as_ref().unwrap()).clone();
        keywords
            .fields
            .insert("title".into(), "x".repeat(300).into());
        metadata.site_keywords = Some(std::sync::Arc::new(keywords));
        let name = dir_file.render(&long).unwrap();
        assert_eq!(name, format!("2020-09-13 i4 - {}.gif", "x".repeat(230)));
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /rest_api//v2/images/i3?ts="));
        assert!(
            requests[1].starts_with(
                "GET /rest_api//v2/images?username=jane&limit=40&offset=0&hide_empty=true&ts="
            ),
            "{}",
            requests[1]
        );
    }
}
