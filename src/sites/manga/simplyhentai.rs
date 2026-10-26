//! Simply Hentai galleries and listings, mirroring
//! gallery_dl/extractor/simplyhentai.py through the v3 API.
use super::{Gallery, set};
use crate::error;
use crate::sites::gdl::{
    captures, date, group, int_of, language_to_code, parse_iso, parse_query, re,
};
use crate::sites::{Events, Http, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://www.simply-hentai.com";
const API: &str = "https://api-v3.simply-hentai.com";
const BASE: &str = r"^https?://(?:www\.)?simply-hentai\.com";

static GALLERY: Route = Route::new(
    "gallery",
    super::GALLERY_FILENAME,
    &["{category}", "{gallery_id} {title}"],
    "{id}",
);
static SERIES: Route = listing("series");
static MANGA: Route = listing("manga");
static TAG: Route = listing("tag");
static LANGUAGE: Route = listing("language");

const fn listing(subcategory: &'static str) -> Route {
    Route::new(subcategory, "{filename}.{extension}", &["{category}"], "")
}

pub static SITE: Site = Site {
    category: "simplyhentai",
    name: "Simply Hentai",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&GALLERY, &SERIES, &MANGA, &TAG, &LANGUAGE],
    api_key: false,
    cookies: None,
};

#[derive(Debug)]
enum Target {
    Gallery(String),
    /// A listing route, its API endpoint and query parameters.
    Listing(&'static Route, String, Map<String, Value>),
}

/// Listing parameters from the optional `tag-`, `sort-` and `page-` path
/// parts and the query string.
fn params(caps: &regex::Captures<'_>, tag: Option<usize>, first: usize) -> Map<String, Value> {
    let mut params = parse_query(caps.get(first + 3).map_or("", |m| m.as_str()));
    if let Some(m) = tag.and_then(|i| caps.get(i)) {
        set(&mut params, "tag_slug", m.as_str());
    }
    if let Some(m) = caps.get(first + 1) {
        set(&mut params, "sort", m.as_str());
    }
    if let Some(m) = caps.get(first + 2) {
        set(&mut params, "page", m.as_str());
    }
    params
}

fn target(url: &Url) -> Option<Target> {
    const TAIL: &str = r"(?:/sort-([^/?#]+))?(?:/page-(\d+))?(?:\?([^#]+))?";
    let series = re!(&format!(
        r"{BASE}/series/([^/?#]+)(?:/tag-([^/?#]+))?{TAIL}"
    ));
    if let Some(c) = captures(series, url) {
        return Some(Target::Listing(
            &SERIES,
            format!("/series/{}", group(&c, 1)),
            params(&c, Some(2), 2),
        ));
    }
    let manga = re!(&format!(r"{BASE}/2-mangas{TAIL}"));
    if let Some(c) = captures(manga, url) {
        return Some(Target::Listing(
            &MANGA,
            "/mangas".into(),
            params(&c, None, 0),
        ));
    }
    let tag = re!(&format!(
        r"{BASE}/(parody|tag|character|collection|artist|translator)/([^/?#]+)(?:/tag-([^/?#]+))?{TAIL}"
    ));
    if let Some(c) = captures(tag, url) {
        let mut params = params(&c, Some(3), 3);
        let kind = group(&c, 1);
        let endpoint = if kind == "collection" {
            format!("/collection/{}", group(&c, 2))
        } else {
            set(&mut params, "type", kind);
            format!("/tag/{}", group(&c, 2))
        };
        return Some(Target::Listing(&TAG, endpoint, params));
    }
    let language = re!(&format!(r"{BASE}/language/([^/?#]+){TAIL}"));
    if let Some(c) = captures(language, url) {
        let mut params = params(&c, None, 1);
        set(&mut params, "type", "language");
        return Some(Target::Listing(
            &LANGUAGE,
            format!("/tag/{}", group(&c, 1)),
            params,
        ));
    }
    let gallery = re!(&format!(r"{BASE}/[^/?#]+/([^/?#]+)"));
    captures(gallery, url).map(|c| Target::Gallery(group(&c, 1).into()))
}

async fn api(http: &Http, endpoint: &str, params: &Map<String, Value>) -> crate::Result<Value> {
    let mut url = http.api(&format!("{API}/v3{endpoint}"))?;
    if !params.is_empty() {
        url.query_pairs_mut().extend_pairs(
            params
                .iter()
                .map(|(k, v)| (k.as_str(), crate::sites::scalar(v))),
        );
    }
    let referer = format!("{ROOT}/");
    let headers = http.browser_headers(&[("Referer", &referer), ("Origin", ROOT)]);
    http.json_with(&url, &headers).await
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Simply Hentai URL"));
    };
    match target {
        Target::Gallery(slug) => crate::sites::once(gallery(http, slug)),
        Target::Listing(route, endpoint, mut params) => {
            let page = int_of(params.get("page")).max(0);
            set(&mut params, "page", if page == 0 { 1 } else { page });
            crate::sites::paginate(Some(params), move |state| {
                let endpoint = endpoint.clone();
                async move {
                    let Some(mut params) = state else {
                        return Ok(None);
                    };
                    let data = api(http, &endpoint, &params).await?;
                    let galleries = match &data["data"] {
                        Value::Object(map) => map.get("albums").cloned().unwrap_or(Value::Null),
                        other => other.clone(),
                    };
                    let children = galleries
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|gallery| {
                            let series = gallery
                                .get("series")
                                .filter(|s| crate::sites::gdl::truthy(Some(s)))
                                .map_or_else(
                                    || "8-original-work".to_owned(),
                                    |s| crate::sites::scalar(&s["slug"]),
                                );
                            let url = Url::parse(&format!(
                                "{ROOT}/{series}/{}",
                                crate::sites::scalar(&gallery["slug"])
                            ))
                            .ok()?;
                            Some((url, gallery.as_object().cloned().unwrap_or_default()))
                        })
                        .collect();
                    let current = int_of(params.get("page"));
                    let pages = data["pagination"].get("pages").and_then(Value::as_i64);
                    let next = match pages {
                        Some(pages) if current < pages => {
                            set(&mut params, "page", current + 1);
                            Some(params)
                        }
                        _ => None,
                    };
                    Ok(Some((
                        super::queue_urls(http, &SITE, route, children),
                        next,
                    )))
                }
            })
        }
    }
}

async fn gallery(http: &Http, slug: String) -> crate::Result<Events<'_>> {
    let endpoint = format!("/manga/{slug}");
    let mut data = api(http, &endpoint, &Map::new()).await?["data"]
        .as_object()
        .cloned()
        .ok_or_else(|| error("Simply Hentai gallery not found"))?;
    let id = data.remove("id").unwrap_or(Value::Null);
    set(&mut data, "gallery_id", id.clone());
    let series = data
        .get("series")
        .filter(|s| crate::sites::gdl::truthy(Some(s)))
        .map_or_else(
            || data.get("series").cloned().unwrap_or(Value::Null),
            |s| s.get("title").cloned().unwrap_or(Value::Null),
        );
    set(&mut data, "series", series);
    let created = data
        .get("created_at")
        .map(crate::sites::scalar)
        .unwrap_or_default();
    set(&mut data, "date", date(parse_iso(&created)));
    for key in ["artists", "characters", "parodies", "tags", "translators"] {
        let titles: Vec<Value> = data
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|t| t["title"].clone())
            .collect();
        set(&mut data, key, titles);
    }
    data.remove("related");
    let mut images = data.remove("images").unwrap_or(Value::Null);
    if data.get("image_count").map_or(32, |c| int_of(Some(c))) > 12 {
        images =
            api(http, &format!("{endpoint}/pages"), &Map::new()).await?["data"]["pages"].clone();
    }
    if let Some(language) = data
        .get("language")
        .and_then(|l| l.get("name"))
        .and_then(Value::as_str)
    {
        let language = language.to_owned();
        set(
            &mut data,
            "lang",
            language_to_code(&language).map_or(Value::Null, Value::from),
        );
        set(&mut data, "language", language);
    }
    if let Some(full) = data
        .remove("preview")
        .and_then(|p| p.get("sizes").and_then(|s| s.get("full")).cloned())
    {
        set(&mut data, "cover", full);
    }
    let pages = images
        .as_array()
        .into_iter()
        .flatten()
        .map(|img| {
            let mut image = Map::new();
            set(&mut image, "id", img["id"].clone());
            (crate::sites::scalar(&img["sizes"]["full"]), Some(image))
        })
        .collect();
    Ok(crate::sites::events(
        Gallery::numbered(&GALLERY, data, pages)
            .id(crate::sites::scalar(&id))
            .events(&SITE, http),
    ))
}

#[cfg(test)]
mod tests {
    use super::super::tests::{archive, path};
    use super::*;
    use crate::sites::testing::{children, http, json, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert!(matches!(
            t("https://www.simply-hentai.com/puella-magi-madoka-magica/hangyaku-english"),
            Some(Target::Gallery(slug)) if slug == "hangyaku-english"
        ));
        let Some(Target::Listing(route, endpoint, params)) = t(
            "https://www.simply-hentai.com/series/mysterious-girlfriend-x/tag-nakadashi/sort-popularity",
        ) else {
            panic!()
        };
        assert_eq!(route.subcategory, "series");
        assert_eq!(endpoint, "/series/mysterious-girlfriend-x");
        assert_eq!(
            Value::Object(params),
            json!({"tag_slug": "nakadashi", "sort": "popularity"})
        );
        let Some(Target::Listing(_, endpoint, params)) =
            t("https://www.simply-hentai.com/language/german/sort-newest/page-12")
        else {
            panic!()
        };
        assert_eq!(endpoint, "/tag/german");
        assert_eq!(
            Value::Object(params),
            json!({"sort": "newest", "page": "12", "type": "language"})
        );
        let Some(Target::Listing(_, endpoint, params)) =
            t("https://www.simply-hentai.com/collection/english-b5e31")
        else {
            panic!()
        };
        assert_eq!(endpoint, "/collection/english-b5e31");
        assert!(params.is_empty());
        let Some(Target::Listing(_, endpoint, params)) =
            t("https://www.simply-hentai.com/artist/crimson/tag-color/sort-alphabetical")
        else {
            panic!()
        };
        assert_eq!(endpoint, "/tag/crimson");
        assert_eq!(params["type"], "artist");
        assert_eq!(params["tag_slug"], "color");
        assert!(
            matches!(t("https://www.simply-hentai.com/2-mangas/sort-most-viewed"), Some(Target::Listing(r, _, _)) if r.subcategory == "manga")
        );
        assert!(t("https://www.simply-hentai.com/x").is_none());
    }

    #[tokio::test]
    async fn extracts_gallery() {
        let (root, server) = serve(vec![json(json!({"data": {
            "id": 99, "title": "Hangyaku", "slug": "h", "created_at": "2019-01-02T03:04:05Z",
            "series": {"title": "Madoka", "slug": "madoka"}, "artists": [{"title": "A"}], "tags": null,
            "language": {"name": "english"}, "preview": {"sizes": {"full": "https://c.test/p.jpg"}},
            "image_count": 2, "related": [1],
            "images": [{"id": 1, "sizes": {"full": "https://cdn.simply-hentai.com/1.jpg"}},
                       {"id": 2, "sizes": {"full": "https://cdn.simply-hentai.com/2.jpg"}}]}}))]);
        let http = http("simplyhentai", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.simply-hentai.com/madoka/h").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[1]);
        assert_eq!(k["gallery_id"], 99);
        assert_eq!(k["series"], "Madoka");
        assert_eq!(k["artists"], json!(["A"]));
        assert_eq!(k["tags"], json!([]));
        assert_eq!(k["lang"], "en");
        assert_eq!(k["cover"], "https://c.test/p.jpg");
        assert_eq!(k["id"], 2);
        assert!(k.get("related").is_none());
        assert_eq!(
            path(&files[1]),
            "simplyhentai/99 Hangyaku/simplyhentai_99_002.jpg"
        );
        assert_eq!(archive(&files[0]), r#"["{id}",["1"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /v3/manga/h "));
        assert!(
            requests[0]
                .to_ascii_lowercase()
                .contains("origin: https://www.simply-hentai.com")
        );
    }

    #[tokio::test]
    async fn listings_page_through_albums() {
        let (root, server) = serve(vec![
            json(
                json!({"data": {"albums": [{"slug": "a", "series": {"slug": "s"}}]}, "pagination": {"pages": 2}}),
            ),
            json(json!({"data": [{"slug": "b", "series": null}], "pagination": {"pages": 2}})),
        ]);
        let http = http("simplyhentai", root);
        assert_eq!(
            children(extract(
                &http,
                Url::parse("https://www.simply-hentai.com/series/s").unwrap()
            ))
            .await
            .len(),
            2
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /v3/series/s?page=1 "));
        assert!(requests[1].starts_with("GET /v3/series/s?page=2 "));
    }
}
