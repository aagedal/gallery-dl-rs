//! Simply Hentai galleries and listings, mirroring
//! gallery_dl/extractor/simplyhentai.py: everything comes from the
//! `api-v3.simply-hentai.com` JSON API; listings queue each gallery with
//! its API data.
use super::{Gallery, get, set, text};
use crate::sites::art::{on_host, parse_query, segment, truthy};
use crate::sites::{Events, Http, Route, Site, percent_decode};
use crate::{Result, error};
use futures_util::StreamExt;
use serde_json::Value;
use url::Url;

pub static SITE: Site = Site {
    category: "simplyhentai",
    name: "Simply Hentai",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&GALLERY, &SERIES, &MANGA, &TAG, &LANGUAGE],
    api_key: false,
    cookies: None,
};

const ROOT: &str = "https://www.simply-hentai.com";
const API: &str = "https://api-v3.simply-hentai.com";
/// Listing pages one URL may claim.
const MAX_PAGES: i64 = 10_000;

static GALLERY: Route = Route::new(
    "gallery",
    "{category}_{gallery_id}_{num:>03}.{extension}",
    &["{category}", "{gallery_id} {title}"],
    "{id}",
);
static SERIES: Route = Route::new("series", "{filename}.{extension}", &["{category}"], "");
static MANGA: Route = Route::new("manga", "{filename}.{extension}", &["{category}"], "");
static TAG: Route = Route::new("tag", "{filename}.{extension}", &["{category}"], "");
static LANGUAGE: Route = Route::new("language", "{filename}.{extension}", &["{category}"], "");

#[derive(Debug, PartialEq)]
enum Target {
    /// A listing: its route, API endpoint and request parameters.
    Listing {
        route: &'static str,
        endpoint: String,
        params: Vec<(String, String)>,
    },
    /// Gallery slug, raw.
    Gallery(String),
}

/// One optional `/PREFIX([^/?#]+)` group at the start of `rest`.
fn group<'r>(rest: &mut &'r str, prefix: &str) -> Option<&'r str> {
    let value = rest.strip_prefix(prefix)?;
    let end = value.find('/').unwrap_or(value.len());
    if end == 0 {
        return None;
    }
    *rest = &value[end..];
    Some(&value[..end])
}

/// `(?:/tag-([^/?#]+))?(?:/sort-([^/?#]+))?(?:/page-(\d+))?(?:\?([^#]+))?`
/// after a listing's slug: the URL's query (only if nothing else follows)
/// with the path's values set in upstream order.
fn listing_params(
    mut rest: &str,
    query: Option<&str>,
    tag: bool,
    mut params: Vec<(String, String)>,
) -> Vec<(String, String)> {
    let tag = if tag { group(&mut rest, "/tag-") } else { None };
    let sort = group(&mut rest, "/sort-");
    let page = rest.strip_prefix("/page-").and_then(|value| {
        let len = value.bytes().take_while(u8::is_ascii_digit).count();
        (len > 0).then(|| {
            rest = &value[len..];
            &value[..len]
        })
    });
    let qs = query.filter(|q| !q.is_empty() && rest.is_empty());
    let mut all = parse_query(qs.unwrap_or_default());
    for (key, value) in params.drain(..) {
        put(&mut all, &key, value);
    }
    for (key, value) in [("tag_slug", tag), ("sort", sort), ("page", page)] {
        if let Some(value) = value {
            put(&mut all, key, percent_decode(value));
        }
    }
    all
}

/// `params[key] = value`, keeping an existing key's position.
fn put(params: &mut Vec<(String, String)>, key: &str, value: String) {
    match params.iter_mut().find(|(k, _)| k == key) {
        Some(entry) => entry.1 = value,
        None => params.push((key.to_owned(), value)),
    }
}

/// The series, manga, tag, language and gallery patterns, in upstream
/// order; all are unanchored at the end.
fn target(url: &Url) -> Option<Target> {
    if !on_host(url, &["simply-hentai.com"]) {
        return None;
    }
    let path = url.path();
    let query = url.query();
    let slug_of = |rest: &'_ str| -> Option<(String, usize)> {
        let end = rest.find('/').unwrap_or(rest.len());
        (end > 0).then(|| (rest[..end].to_owned(), end))
    };
    if let Some(rest) = path.strip_prefix("/series/")
        && let Some((slug, end)) = slug_of(rest)
    {
        return Some(Target::Listing {
            route: "series",
            endpoint: format!("/series/{slug}"),
            params: listing_params(&rest[end..], query, true, Vec::new()),
        });
    }
    if let Some(rest) = path.strip_prefix("/2-mangas") {
        return Some(Target::Listing {
            route: "manga",
            endpoint: "/mangas".into(),
            params: listing_params(rest, query, false, Vec::new()),
        });
    }
    for kind in [
        "parody",
        "tag",
        "character",
        "collection",
        "artist",
        "translator",
    ] {
        let Some((slug, end)) = path
            .strip_prefix(&format!("/{kind}/"))
            .and_then(|rest| slug_of(rest).map(|(s, e)| (s, e + kind.len() + 2)))
        else {
            continue;
        };
        let (endpoint, params) = match kind {
            "collection" => (format!("/collection/{slug}"), Vec::new()),
            _ => (
                format!("/tag/{slug}"),
                vec![("type".to_owned(), kind.to_owned())],
            ),
        };
        return Some(Target::Listing {
            route: "tag",
            endpoint,
            params: listing_params(&path[end..], query, true, params),
        });
    }
    if let Some(rest) = path.strip_prefix("/language/")
        && let Some((language, end)) = slug_of(rest)
    {
        let params = vec![("type".to_owned(), "language".to_owned())];
        return Some(Target::Listing {
            route: "language",
            endpoint: format!("/tag/{language}"),
            params: listing_params(&rest[end..], query, false, params),
        });
    }
    let raw: Vec<&str> = path.split('/').skip(1).collect();
    match raw.as_slice() {
        [first, slug, ..] if segment(first) && segment(slug) => {
            Some(Target::Gallery((*slug).to_owned()))
        }
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Listing {
            route,
            endpoint,
            params,
        }) => {
            let route = SITE
                .routes
                .iter()
                .copied()
                .find(|r| r.subcategory == route)
                .unwrap_or(&SERIES);
            listing(http, route, endpoint, params)
        }
        Some(Target::Gallery(slug)) => gallery(http, slug),
        None => crate::sites::fail(error("unsupported Simply Hentai URL")),
    }
}

/// `SimplyhentaiExtractor.request_api`.
async fn api(http: &Http, endpoint: &str, params: &[(String, String)]) -> Result<Value> {
    let mut url = http.api(&format!("{API}/v3{endpoint}"))?;
    if !params.is_empty() {
        url.query_pairs_mut().extend_pairs(params);
    }
    let referer = format!("{ROOT}/");
    http.json_with(
        &url,
        &http.browser_headers(&[("Referer", &referer), ("Origin", ROOT)]),
    )
    .await
}

/// `SimplyhentaiExtractor._pagination` and `items`: each listed gallery,
/// queued as `ROOT/SERIES/SLUG` with its data, until the last page.
fn listing<'a>(
    http: &'a Http,
    route: &'static Route,
    endpoint: String,
    mut params: Vec<(String, String)>,
) -> Events<'a> {
    let first = params
        .iter()
        .find(|(k, _)| k == "page")
        .map_or(1, |(_, v)| crate::sites::art::parse_int(v, 1));
    put(&mut params, "page", first.to_string());
    crate::sites::paginate(Some((first, 0)), move |state| {
        let endpoint = endpoint.clone();
        let mut params = params.clone();
        async move {
            let Some((page, count)) = state else {
                return Ok(None);
            };
            put(&mut params, "page", page.to_string());
            let data = api(http, &endpoint, &params).await?;
            let galleries = match data.get("data") {
                Some(Value::Object(data)) => data.get("albums").cloned().unwrap_or(Value::Null),
                Some(list) => list.clone(),
                None => Value::Null,
            };
            let streams: Vec<Events<'a>> = galleries
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .filter_map(|gallery| {
                    let data = gallery.as_object()?.clone();
                    let slug = crate::sites::scalar(data.get("slug")?);
                    Some(super::queue_with(&SITE, route, &data, move || {
                        self::gallery(http, slug)
                    }))
                })
                .collect();
            let pages = data.pointer("/pagination/pages").and_then(Value::as_i64);
            let next = pages
                .filter(|&pages| page < pages && count + 1 < MAX_PAGES)
                .map(|_| (page + 1, count + 1));
            Ok(Some((
                futures_util::stream::iter(streams).flatten().boxed(),
                next,
            )))
        }
    })
}

/// `SimplyhentaiGalleryExtractor`: the gallery's API data, with its page
/// list fetched separately for more than 12 images.
fn gallery(http: &Http, slug: String) -> Events<'_> {
    crate::sites::once(async move {
        let endpoint = format!("/manga/{slug}");
        let mut data = match api(http, &endpoint, &[]).await?.get("data") {
            Some(Value::Object(data)) => data.clone(),
            _ => return Err(error("Simply Hentai returned no gallery")),
        };
        let id = data.remove("id").unwrap_or(Value::Null);
        set(&mut data, "gallery_id", id.clone());
        let series = match data.get("series") {
            Some(series) if truthy(Some(series)) => get(series, "title"),
            other => other.cloned().unwrap_or(Value::Null),
        };
        set(&mut data, "series", series);
        let created = text::iso(
            data.get("created_at")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        );
        set(&mut data, "date", text::date(created));
        for key in ["artists", "characters", "parodies", "tags", "translators"] {
            let titles: Vec<Value> = data
                .get(key)
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .map(|tag| get(tag, "title"))
                .collect();
            set(&mut data, key, titles);
        }
        data.remove("related");
        let mut images = data.remove("images").unwrap_or(Value::Null);
        let count = data
            .get("image_count")
            .map_or(Some(32), Value::as_i64)
            .unwrap_or(0);
        if count > 12 {
            images = api(http, &format!("{endpoint}/pages"), &[])
                .await?
                .pointer("/data/pages")
                .cloned()
                .unwrap_or(Value::Null);
        }
        if let Some(name) = data.get("language").and_then(|l| l.get("name")).cloned() {
            let language = crate::sites::scalar(&name);
            set(&mut data, "lang", text::language_to_code(&language));
            set(&mut data, "language", name);
        }
        // Upstream pops `preview` even when it has no full size.
        if let Some(full) = data
            .remove("preview")
            .and_then(|preview| preview.pointer("/sizes/full").cloned())
        {
            set(&mut data, "cover", full);
        }
        let pages = images
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .map(|image| {
                let url = image
                    .pointer("/sizes/full")
                    .map(crate::sites::scalar)
                    .unwrap_or_default();
                let id = get(image, "id");
                (url, Some(super::keywords([("id", id)])))
            })
            .collect();
        let gallery = Gallery {
            enum_key: "num",
            ..Gallery::chapter(&GALLERY, data, pages)
                .id(crate::sites::scalar(&id))
                .timestamp(created)
        };
        Ok(crate::sites::events(gallery.events(&SITE, http)))
    })
}

#[cfg(test)]
mod tests {
    use super::super::tests::{archive, path};
    use super::*;
    use crate::sites::testing::{http, json, keywords as kw, media, queued, serve};
    use serde_json::json;

    fn listing(route: &'static str, endpoint: &str, params: &[(&str, &str)]) -> Option<Target> {
        Some(Target::Listing {
            route,
            endpoint: endpoint.into(),
            params: params
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        })
    }

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.simply-hentai.com/series/naruto"),
            listing("series", "/series/naruto", &[])
        );
        assert_eq!(
            t("https://www.simply-hentai.com/series/naruto/tag-x/sort-new/page-3?a=1&sort=old"),
            listing(
                "series",
                "/series/naruto",
                &[
                    ("a", "1"),
                    ("sort", "new"),
                    ("tag_slug", "x"),
                    ("page", "3")
                ]
            )
        );
        assert_eq!(
            t("https://simply-hentai.com/series/naruto/other?a=1"),
            listing("series", "/series/naruto", &[])
        );
        assert_eq!(
            t("https://www.simply-hentai.com/2-mangas/page-2"),
            listing("manga", "/mangas", &[("page", "2")])
        );
        assert_eq!(
            t("https://www.simply-hentai.com/artist/someone/sort-new"),
            listing(
                "tag",
                "/tag/someone",
                &[("type", "artist"), ("sort", "new")]
            )
        );
        assert_eq!(
            t("https://www.simply-hentai.com/collection/best?x=y"),
            listing("tag", "/collection/best", &[("x", "y")])
        );
        assert_eq!(
            t("https://www.simply-hentai.com/language/english/page-2"),
            listing(
                "language",
                "/tag/english",
                &[("type", "language"), ("page", "2")]
            )
        );
        assert_eq!(
            t("https://www.simply-hentai.com/naruto/some-gallery/all-pages"),
            Some(Target::Gallery("some-gallery".into()))
        );
        for raw in [
            "https://www.simply-hentai.com/",
            "https://www.simply-hentai.com/naruto",
            "https://api.simply-hentai.com/naruto/x",
            "https://www.simply-hentai.com.evil.test/naruto/x",
            "https://www.simply-hentai.com:8443/naruto/x",
            "https://u:p@www.simply-hentai.com/naruto/x",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn gallery_json(images: Value) -> Value {
        json!({"data": {"id": 777, "slug": "some-gallery", "title": "Some Gallery",
            "series": {"title": "Naruto", "slug": "naruto"}, "created_at": "2024-03-04T05:06:07+00:00",
            "artists": [{"title": "A"}], "tags": [{"title": "T1"}, {"title": "T2"}],
            "related": [1], "image_count": 2, "language": {"name": "english"},
            "preview": {"sizes": {"full": "https://cdn.test/cover.jpg"}}, "images": images}})
    }

    #[tokio::test]
    async fn extracts_gallery() {
        let images = json!([
            {"id": 11, "sizes": {"full": "https://cdn.test/a/1.jpg"}},
            {"id": 12, "sizes": {"full": "https://cdn.test/a/2.png"}},
        ]);
        let (root, requests) = serve(vec![json(gallery_json(images))]);
        let http = http("Simply Hentai", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.simply-hentai.com/naruto/some-gallery").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = kw(&files[1]);
        assert_eq!(k["gallery_id"], 777);
        assert_eq!(k["id"], 12);
        assert_eq!(k["series"], "Naruto");
        assert_eq!(k["date"], "2024-03-04 05:06:07");
        assert_eq!(k["artists"], json!(["A"]));
        assert_eq!(k["tags"], json!(["T1", "T2"]));
        assert_eq!(k["characters"], json!([]));
        assert_eq!(k["language"], "english");
        assert_eq!(k["lang"], "en");
        assert_eq!(k["cover"], "https://cdn.test/cover.jpg");
        assert_eq!(k["num"], 2);
        assert!(!k.contains_key("related") && !k.contains_key("preview"));
        assert_eq!(
            path(&files[1]),
            "simplyhentai/777 Some Gallery/simplyhentai_777_002.png"
        );
        assert_eq!(archive(&files[1]), r#"["{id}",["12"]]"#);
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /v3/manga/some-gallery "));
        assert!(
            requests[0]
                .to_ascii_lowercase()
                .contains("origin: https://www.simply-hentai.com\r\n")
        );
    }

    #[tokio::test]
    async fn large_gallery_requests_its_pages() {
        let mut data = gallery_json(json!([]));
        data["data"]["image_count"] = json!(20);
        data["data"]["series"] = Value::Null;
        let pages =
            json!({"data": {"pages": [{"id": 1, "sizes": {"full": "https://cdn.test/1.jpg"}}]}});
        let (root, requests) = serve(vec![json(data), json(pages)]);
        let http = http("Simply Hentai", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.simply-hentai.com/8-original-work/some-gallery").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        assert_eq!(kw(&files[0])["series"], Value::Null);
        let requests = requests.join().unwrap();
        assert!(requests[1].starts_with("GET /v3/manga/some-gallery/pages "));
    }

    #[tokio::test]
    async fn listing_pages_and_queues_galleries() {
        let first = json!({"data": {"albums": [
            {"slug": "g1", "series": {"slug": "naruto"}},
            {"slug": "g2", "series": null},
        ]}, "pagination": {"pages": 2}});
        let second = json!({"data": [{"slug": "g3"}], "pagination": {"pages": 2}});
        let (root, requests) = serve(vec![json(first), json(second)]);
        let http = http("Simply Hentai", root);
        let children = queued(extract(
            &http,
            Url::parse("https://www.simply-hentai.com/tag/big/sort-new").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 3);
        assert_eq!(children[0].subcategory(), "tag");
        let requests = requests.join().unwrap();
        assert!(requests[0].starts_with("GET /v3/tag/big?type=tag&sort=new&page=1 "));
        assert!(requests[1].starts_with("GET /v3/tag/big?type=tag&sort=new&page=2 "));
    }
}
