//! Philomena boorus (Derpibooru, Ponybooru, Furbooru), mirroring
//! gallery_dl/extractor/philomena.py through the `/api/v1/json` API.
use super::{date_value, iso_timestamp};
use crate::error;
use crate::sites::{Events, Http, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const PER_PAGE: usize = 50;
static POST: Route = Route::new("post", "{filename}.{extension}", &["{category}"], "{id}");
static SEARCH: Route = Route::new(
    "search",
    "{filename}.{extension}",
    &["{category}", "{search_tags}"],
    "{id}",
);
static GALLERY: Route = Route::new(
    "gallery",
    "{filename}.{extension}",
    &["{category}", "galleries", "{gallery[id]} {gallery[title]}"],
    "{id}",
);

macro_rules! instance {
    ($ident:ident, $category:literal, $name:literal) => {
        pub static $ident: Site = Site {
            category: $category,
            name: $name,
            matches: |url| target($category, url).is_some(),
            extract: |http, url| extract($category, http, url),
            routes: &[&POST, &SEARCH, &GALLERY],
            api_key: true,
            cookies: None,
        };
    };
}
instance!(DERPIBOORU, "derpibooru", "Derpibooru");
instance!(PONYBOORU, "ponybooru", "Ponybooru");
instance!(FURBOORU, "furbooru", "Furbooru");

struct Instance {
    site: &'static Site,
    root: &'static str,
    hosts: &'static [&'static str],
    filter_id: &'static str,
}

fn instance(category: &str) -> Instance {
    match category {
        "derpibooru" => Instance {
            site: &DERPIBOORU,
            root: "https://derpibooru.org",
            hosts: &["derpibooru.org", "www.derpibooru.org"],
            filter_id: "56027",
        },
        "ponybooru" => Instance {
            site: &PONYBOORU,
            root: "https://ponybooru.org",
            hosts: &["ponybooru.org", "www.ponybooru.org"],
            filter_id: "3",
        },
        _ => Instance {
            site: &FURBOORU,
            root: "https://furbooru.org",
            hosts: &["furbooru.org"],
            filter_id: "2",
        },
    }
}

#[derive(Debug, PartialEq)]
pub(crate) enum Target {
    Post(String),
    Search(Vec<(String, String)>),
    Gallery(String),
}

/// Classify a path below the site root, shared with twibooru, whose post
/// pattern lacks the optional `images/` segment.
pub(crate) fn classify(rest: &str, images: bool) -> Option<Target> {
    let path = rest.strip_prefix('/')?;
    let post = if images {
        path.strip_prefix("images/").unwrap_or(path)
    } else {
        path
    };
    if let Some(id) = super::leading_digits(post) {
        return Some(Target::Post(id.into()));
    }
    if let Some(after) = path.strip_prefix("search") {
        let after = after.strip_prefix('/').unwrap_or(after);
        let query = super::until(after.strip_prefix('?')?, &['#']);
        return (!query.is_empty()).then(|| Target::Search(super::parse_query(query, false)));
    }
    if let Some(tag) = path.strip_prefix("tags/") {
        let tag = super::until(tag, &['/', '?', '#']);
        return (!tag.is_empty()).then(|| Target::Search(vec![("q".into(), tag_query(tag))]));
    }
    let id = super::leading_digits(path.strip_prefix("galleries/")?)?;
    Some(Target::Gallery(id.into()))
}

/// A `/tags/NAME` slug as a search query.
pub(crate) fn tag_query(tag: &str) -> String {
    let mut q = tag.replace('+', " ");
    for (old, new) in [
        ("-colon-", ":"),
        ("-dash-", "-"),
        ("-dot-", "."),
        ("-plus-", "+"),
        ("-fwslash-", "/"),
        ("-bwslash-", "\\"),
    ] {
        q = q.replace(old, new);
    }
    super::unquote(&super::unquote(&q))
}

fn target(category: &str, url: &Url) -> Option<Target> {
    classify(&super::below(url, instance(category).hosts)?, true)
}

fn extract<'a>(category: &'static str, http: &'a Http, url: Url) -> Events<'a> {
    let Some(target) = target(category, &url) else {
        return crate::sites::fail(error("unsupported Philomena URL"));
    };
    let inst = instance(category);
    let api = format!("{}/api/v1/json", inst.root);
    match target {
        Target::Post(id) => crate::sites::once(async move {
            let url = http.api(&format!("{api}/images/{id}"))?;
            let body = http.json(&url).await?;
            let image = body
                .get("image")
                .ok_or_else(|| error("Philomena image response lacks an image"))?;
            Ok(crate::sites::events(images_events(
                inst.site,
                http,
                &POST,
                std::slice::from_ref(image),
                &Map::new(),
            )))
        }),
        Target::Search(params) => {
            let mut data = Map::new();
            data.insert(
                "search_tags".into(),
                super::query_get(&params, "q").unwrap_or_default().into(),
            );
            search(http, category, &SEARCH, params, data)
        }
        Target::Gallery(id) => crate::sites::once(async move {
            let mut url = http.api(&format!("{api}/search/galleries"))?;
            url.query_pairs_mut().append_pair("q", &format!("id:{id}"));
            let body = http.json(&url).await?;
            let gallery = body
                .get("galleries")
                .and_then(|g| g.get(0))
                .cloned()
                .ok_or_else(|| error("Philomena gallery not found"))?;
            let mut data = Map::new();
            data.insert("gallery".into(), gallery);
            let filter = format!("gallery_id:{id}");
            let params = vec![
                ("sd".into(), "desc".into()),
                ("sf".into(), filter.clone()),
                ("q".into(), filter),
            ];
            Ok(search(http, category, &GALLERY, params, data))
        }),
    }
}

fn search<'a>(
    http: &'a Http,
    category: &'static str,
    route: &'static Route,
    mut params: Vec<(String, String)>,
    data: Map<String, Value>,
) -> Events<'a> {
    let inst = instance(category);
    if let Some(key) = http.api_key() {
        params.push(("key".into(), key.into()));
    }
    if let Some(filter) = filter_option(http) {
        params.push(("filter_id".into(), filter));
    } else if http.api_key().is_none() {
        params.push(("filter_id".into(), inst.filter_id.into()));
    }
    let endpoint = format!("{}/api/v1/json/search/images", inst.root);
    crate::sites::paginate(Some(1u64), move |page| {
        let params = params.clone();
        let data = data.clone();
        let endpoint = endpoint.clone();
        async move {
            let Some(page) = page else {
                return Ok(None);
            };
            let mut url = http.api(&endpoint)?;
            url.query_pairs_mut()
                .extend_pairs(&params)
                .append_pair("page", &page.to_string())
                .append_pair("per_page", &PER_PAGE.to_string());
            let body = http.json(&url).await?;
            let images = body
                .get("images")
                .and_then(Value::as_array)
                .ok_or_else(|| error("Philomena search response lacks images"))?;
            let events = images_events(inst.site, http, route, images, &data);
            let next = (images.len() >= PER_PAGE).then_some(page + 1);
            Ok(Some((crate::sites::events(events), next)))
        }
    })
}

/// The `filter` option as an API `filter_id`.
pub(crate) fn filter_option(http: &Http) -> Option<String> {
    match http.option("filter")? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn images_events(
    site: &'static Site,
    http: &Http,
    route: &'static Route,
    images: &[Value],
    data: &Map<String, Value>,
) -> Vec<crate::event::ExtractionEvent> {
    let svg = http.option_bool("svg", true);
    let items = images
        .iter()
        .filter_map(|image| parse_image(image, data, svg))
        .collect();
    super::per_post(site, http, route, items)
}

/// One API image as a file with its upstream keywords.
fn parse_image(image: &Value, data: &Map<String, Value>, svg: bool) -> Option<crate::sites::Item> {
    let mut fields = image.as_object()?.clone();
    let mut raw = fields
        .get("representations")
        .and_then(|r| r.get("full"))
        .or_else(|| fields.get("view_url"))?
        .as_str()?
        .to_owned();
    if svg && fields.get("format").and_then(Value::as_str) == Some("svg") {
        raw = format!("{}.svg", super::rpartition(&raw, ".").0);
    }
    let url = super::file_url("", &raw).filter(|u| u.host_str().is_some())?;
    super::booru_fields(&url, &mut fields, data);
    let created = fields
        .get("created_at")
        .and_then(Value::as_str)
        .and_then(|s| iso_timestamp(s.get(..19).unwrap_or(s)));
    fields.insert("date".into(), date_value(created));
    Some(super::item(url, fields, "uploader", "tags", created))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http, json, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |c, raw: &str| target(c, &Url::parse(raw).unwrap());
        assert_eq!(
            t("derpibooru", "https://derpibooru.org/images/1"),
            Some(Target::Post("1".into()))
        );
        assert_eq!(
            t("derpibooru", "https://www.derpibooru.org/1/comments"),
            Some(Target::Post("1".into()))
        );
        assert_eq!(
            t("ponybooru", "https://ponybooru.org/search?q=cute&sf=score"),
            Some(Target::Search(vec![
                ("q".into(), "cute".into()),
                ("sf".into(), "score".into())
            ]))
        );
        assert_eq!(
            t(
                "furbooru",
                "https://furbooru.org/tags/artist-colon-somebody+name"
            ),
            Some(Target::Search(vec![(
                "q".into(),
                "artist:somebody name".into()
            )]))
        );
        assert_eq!(
            t("derpibooru", "https://derpibooru.org/galleries/42"),
            Some(Target::Gallery("42".into()))
        );
        for (category, raw) in [
            ("furbooru", "https://www.furbooru.org/images/1"),
            ("derpibooru", "https://derpibooru.org.evil.test/images/1"),
            ("derpibooru", "https://derpibooru.org:8443/images/1"),
            ("derpibooru", "https://derpibooru.org/search"),
            ("derpibooru", "https://derpibooru.org/tags/"),
            ("derpibooru", "https://derpibooru.org/profiles/x"),
            ("derpibooru", "https://ponybooru.org/images/1"),
        ] {
            assert_eq!(t(category, raw), None, "{raw}");
        }
        assert!(
            crate::sites::find(&Url::parse("https://ponybooru.org/images/5").unwrap())
                .is_some_and(|site| site.category == "ponybooru")
        );
    }

    fn image(id: u64, format: &str) -> Value {
        json!({"id": id, "format": format, "created_at": "2017-07-14T02:40:00Z",
            "uploader": "someone", "tags": ["safe", "cute"],
            "view_url": format!("https://derpicdn.net/img/view/2017/7/14/{id}.png"),
            "representations": {"full": format!("https://derpicdn.net/img/view/2017/7/14/{id}.png")}})
    }

    #[tokio::test]
    async fn extracts_single_post() {
        let (root, server) = serve(vec![json(json!({"image": image(7, "svg")}))]);
        let http = http("Derpibooru", root);
        let files = media(extract(
            "derpibooru",
            &http,
            Url::parse("https://derpibooru.org/images/7").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].url.as_str(),
            "https://derpicdn.net/img/view/2017/7/14/7.svg"
        );
        let k = keywords(&files[0]);
        assert_eq!(k["filename"], "7");
        assert_eq!(k["extension"], "svg");
        assert_eq!(k["date"], "2017-07-14 02:40:00");
        assert_eq!(k["subcategory"], "post");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "7.svg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("derpibooru")
        );
        let (namespace, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(namespace, "derpibooru");
        assert_eq!(identity, r#"["{id}",["7"]]"#);
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with("GET /api/v1/json/images/7 "),
            "{}",
            requests[0]
        );
    }

    #[tokio::test]
    async fn paginates_search_with_instance_filter() {
        let first: Vec<_> = (1..=50).map(|id| image(id, "png")).collect();
        let (root, server) = serve(vec![
            json(json!({"images": first})),
            json(json!({"images": [image(51, "png")]})),
        ]);
        let http = http("Derpibooru", root);
        let files = media(extract(
            "derpibooru",
            &http,
            Url::parse("https://derpibooru.org/search?q=safe%2C+cute").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 51);
        let k = keywords(&files[50]);
        assert_eq!(k["search_tags"], "safe, cute");
        let (_, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("derpibooru/safe, cute")
        );
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with(
                "GET /api/v1/json/search/images?q=safe%2C+cute&filter_id=56027&page=1&per_page=50 "
            ),
            "{}",
            requests[0]
        );
        assert!(requests[1].contains("&page=2&per_page=50 "));
    }

    #[tokio::test]
    async fn gallery_metadata_and_search() {
        let (root, server) = serve(vec![
            json(json!({"galleries": [{"id": 42, "title": "Best"}]})),
            json(json!({"images": [image(3, "png")]})),
        ]);
        let http = http("Furbooru", root);
        let files = media(extract(
            "furbooru",
            &http,
            Url::parse("https://furbooru.org/galleries/42").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(keywords(&files[0])["gallery"]["title"], "Best");
        let (_, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("furbooru/galleries/42 Best")
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/v1/json/search/galleries?q=id%3A42 "));
        assert!(
            requests[1].starts_with(
                "GET /api/v1/json/search/images?sd=desc&sf=gallery_id%3A42&q=gallery_id%3A42&filter_id=2&page=1&per_page=50 "
            ),
            "{}",
            requests[1]
        );
    }
}
