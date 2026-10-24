//! Size Booru (sizebooru.com), mirroring gallery_dl/extractor/sizebooru.py:
//! listing pages link each post's `/Details/ID` page, and every file is
//! served from `/Picture/ID`. The `metadata` option reads each post's
//! details page for dates, users, tags and favorites.
//!
//! Upstream's `request_interval` of 0.5 to 1.5 seconds is not applied.
use super::{
    Extr, booru_fields, date_value, extract_iter, item, leading_digits, parse_int, per_post,
};
use crate::sites::{Events, Http, Item, Route, Site, remove_html, unescape};
use crate::{Result, error};
use futures_util::{StreamExt, stream};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://sizebooru.com";
const FILENAME: &str = "{id}.{extension}";
const ARCHIVE: &str = "{id}";
/// `SizebooruExtractor.page_start` and `BooruExtractor.per_page`.
const PAGE_START: i64 = 1;
const PER_PAGE: i64 = 100;

static POST: Route = Route::new("post", FILENAME, &["{category}"], ARCHIVE);
static TAG: Route = Route::new("tag", FILENAME, &["{category}", "{search_tags}"], ARCHIVE);
static GALLERY: Route = Route::new(
    "gallery",
    FILENAME,
    &["{category}", "{gallery_name} ({gallery_id})"],
    ARCHIVE,
);
static USER: Route = Route::new("user", FILENAME, &["{category}", "Uploads {user}"], ARCHIVE);
static FAVORITE: Route = Route::new(
    "favorite",
    FILENAME,
    &["{category}", "Favorites {user}"],
    ARCHIVE,
);

pub static SITE: Site = Site {
    category: "sizebooru",
    name: "Size Booru",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&POST, &TAG, &GALLERY, &USER, &FAVORITE],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Post(String),
    Tag(String),
    Gallery(String),
    User(String),
    Favorite(String),
}

/// `(?:www\.)?sizebooru\.com` paths, raw as upstream sees them.
fn target(url: &Url) -> Option<Target> {
    let rest = super::below(url, &["sizebooru.com", "www.sizebooru.com"])?;
    let name = |prefix: &str| {
        let name = super::until(rest.strip_prefix(prefix)?, &['/', '?', '#']);
        (!name.is_empty()).then(|| name.to_owned())
    };
    let id = |prefix: &str| leading_digits(rest.strip_prefix(prefix)?).map(str::to_owned);
    id("/Details/")
        .map(Target::Post)
        .or_else(|| name("/Search/").map(Target::Tag))
        .or_else(|| id("/Galleries/List/").map(Target::Gallery))
        .or_else(|| name("/Profile/Uploads/").map(Target::User))
        .or_else(|| name("/Profile/Favorites/").map(Target::Favorite))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Size Booru URL"));
    };
    let mut data = Map::new();
    let (route, path) = match target {
        Target::Post(id) => {
            return crate::sites::once(async move {
                let item = prepare(http, &id, None, &Map::new()).await?;
                Ok(crate::sites::events(per_post(
                    &SITE,
                    http,
                    &POST,
                    vec![item],
                )))
            });
        }
        Target::Tag(tag) => {
            data.insert("search_tags".into(), super::unquote(&tag).into());
            (&TAG, format!("/Search/{tag}"))
        }
        Target::Gallery(id) => {
            data.insert("gallery_id".into(), parse_int(&id).into());
            (&GALLERY, format!("/Galleries/List/{id}"))
        }
        Target::User(user) => {
            data.insert("user".into(), super::unquote(&user).into());
            (&USER, format!("/Profile/Uploads/{user}"))
        }
        Target::Favorite(user) => {
            data.insert("user".into(), super::unquote(&user).into());
            (&FAVORITE, format!("/Profile/Favorites/{user}"))
        }
    };
    paginate(http, route, path, data)
}

/// Upstream `_pagination`: `pageNo` pages of `/Details/` thumbnails until
/// the `Next` link is disabled. The first gallery page names the gallery.
fn paginate<'a>(
    http: &'a Http,
    route: &'static Route,
    path: String,
    data: Map<String, Value>,
) -> Events<'a> {
    crate::sites::paginate(Some((PAGE_START, data)), move |state| {
        let path = path.clone();
        async move {
            let Some((page, mut data)) = state else {
                return Ok(None);
            };
            let mut url = http.api(&format!("{ROOT}{path}"))?;
            url.query_pairs_mut()
                .append_pair("pageNo", &page.to_string())
                .append_pair("pageSize", &PER_PAGE.to_string());
            let text = http.text(&url).await?;
            if route.subcategory == "gallery" && page == PAGE_START {
                let name = crate::sites::extr(&text, "<title>Gallery: ", " - Size Booru<");
                data.insert(
                    "gallery_name".into(),
                    unescape(name.unwrap_or_default()).into(),
                );
            }
            let thumbs = thumbnails(&text);
            let next_disabled = crate::sites::extr(&text, "area-label=\"Next\"", ">")
                .unwrap_or_default()
                .contains("disabled");
            let next = (!next_disabled && !thumbs.is_empty()).then(|| (page + 1, data.clone()));
            // Posts are prepared as they are reached, as `metadata` fetches
            // each one's details page.
            let events = stream::iter(thumbs)
                .map(move |(id, extension)| {
                    let data = data.clone();
                    crate::sites::once(async move {
                        let item = prepare(http, &id, Some(extension), &data).await?;
                        Ok(crate::sites::events(per_post(
                            &SITE,
                            http,
                            route,
                            vec![item],
                        )))
                    })
                })
                .flatten()
                .boxed();
            Ok(Some((events, next)))
        }
    })
}

/// Each thumbnail's post id and the image type of its inline preview.
fn thumbnails(page: &str) -> Vec<(String, String)> {
    extract_iter(page, "<a href=\"/Details/", ";base64")
        .into_iter()
        .map(|thumb| {
            let id = thumb.split('"').next().unwrap_or_default();
            let extension = thumb.rsplit('/').next().unwrap_or_default();
            (id.to_owned(), extension.to_owned())
        })
        .collect()
}

/// `BooruExtractor.items` for one post: the `/Picture/` URL and `_prepare`
/// (or `_prepare_metadata` with the `metadata` option).
async fn prepare(
    http: &Http,
    id: &str,
    extension: Option<String>,
    data: &Map<String, Value>,
) -> Result<Item> {
    let details = if http.option_bool("metadata", false) {
        Some(
            http.text(&http.api(&format!("{ROOT}/Details/{id}"))?)
                .await?,
        )
    } else {
        None
    };
    parse_post(id, extension, data, details.as_deref())
}

pub(crate) fn parse_post(
    id: &str,
    extension: Option<String>,
    data: &Map<String, Value>,
    details: Option<&str>,
) -> Result<Item> {
    let file_url = format!("{ROOT}/Picture/{id}");
    let url = Url::parse(&file_url).map_err(|_| error("invalid Size Booru post id"))?;
    let mut fields = Map::new();
    fields.insert("id".into(), id.into());
    fields.insert(
        "extension".into(),
        extension.map_or(Value::Null, Value::from),
    );
    fields.insert("file_url".into(), file_url.into());
    booru_fields(&url, &mut fields, data);
    fields.insert("id".into(), parse_int(id).into());
    let mut timestamp = None;
    match details {
        None => {
            fields.insert("filename".into(), id.into());
        }
        Some(page) => {
            let mut extr = Extr::new(page);
            let date = |text: &str| super::strptime(text, "%m/%d/%Y");
            timestamp = date(extr.next("<b>Posted Date:</b> ", "<"));
            fields.insert("date".into(), date_value(timestamp));
            let approved = date(extr.next("<b>Approved Date:</b> ", "<"));
            fields.insert("date_approved".into(), date_value(approved));
            fields.insert(
                "approver".into(),
                remove_html(extr.next("<b>Approved By:</b>", "</")).into(),
            );
            fields.insert(
                "uploader".into(),
                remove_html(extr.next("<b>Posted By:</b>", "</")).into(),
            );
            let artist = extr.next("<b>Artist:</b> ", "</");
            fields.insert(
                "artist".into(),
                if artist == "N/A" {
                    Value::Null
                } else {
                    remove_html(artist).into()
                },
            );
            fields.insert(
                "views".into(),
                parse_int(extr.next("<b>Views:</b>", "<")).into(),
            );
            let source =
                crate::sites::extr(extr.next("<b>Source Link:</b>", "</"), " href=\"", "\"")
                    .filter(|s| !s.is_empty());
            fields.insert("source".into(), source.map_or(Value::Null, Value::from));
            let list = |text: &str| -> Value {
                super::split_html(text)
                    .into_iter()
                    .map(Value::from)
                    .collect()
            };
            fields.insert(
                "tags".into(),
                list(extr.next("<h6>Related Tags</h6>", "</ul>")),
            );
            fields.insert(
                "favorite".into(),
                list(extr.next("<h6>Favorited By</h6>", "</ul>")),
            );
            let (name, ext) = super::rpartition(extr.next("\" alt=\"", "\""), ".");
            fields.insert("filename".into(), name.into());
            if !crate::sites::art::truthy(fields.get("extension")) {
                fields.insert("extension".into(), ext.to_lowercase().into());
            }
        }
    }
    if !crate::sites::art::truthy(fields.get("extension")) {
        fields.insert("extension".into(), "jpg".into());
    }
    Ok(item(url, fields, "uploader", "tags", timestamp))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://sizebooru.com/Details/12345"),
            Some(Target::Post("12345".into()))
        );
        assert_eq!(
            t("https://www.sizebooru.com/Search/giantess"),
            Some(Target::Tag("giantess".into()))
        );
        assert_eq!(
            t("https://sizebooru.com/Galleries/List/123"),
            Some(Target::Gallery("123".into()))
        );
        assert_eq!(
            t("https://sizebooru.com/Profile/Uploads/User%20A"),
            Some(Target::User("User%20A".into()))
        );
        assert_eq!(
            t("https://sizebooru.com/Profile/Favorites/B"),
            Some(Target::Favorite("B".into()))
        );
        for raw in [
            "https://sizebooru.com/Details/x",
            "https://sizebooru.com/Search/",
            "https://sizebooru.com/",
            "https://img.sizebooru.com/Details/1",
            "https://sizebooru.com.evil.com/Details/1",
            "https://sizebooru.com:8080/Details/1",
            "https://u:p@sizebooru.com/Details/1",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[tokio::test]
    async fn paginates_galleries() {
        let first = r#"<title>Gallery: Big &amp; Small - Size Booru</title>
            <a href="/Details/11"><img src="data:image/png;base64,x"></a>
            <a href="/Details/12"><img src="data:image/;base64,x"></a>
            <a area-label="Next" class="page-link">"#;
        let second = r#"<a href="/Details/13"><img src="data:image/gif;base64,x"></a>
            <a area-label="Next" class="page-link disabled">"#;
        let (root, server) = serve(vec![html(first), html(second)]);
        let http = http("Size Booru", root);
        let files = media(extract(
            &http,
            Url::parse("https://sizebooru.com/Galleries/List/7").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].url.as_str(), "https://sizebooru.com/Picture/11");
        let k = keywords(&files[1]);
        assert_eq!(k["id"], 12);
        assert_eq!(k["filename"], "12");
        assert_eq!(k["extension"], "jpg");
        assert_eq!(k["gallery_id"], 7);
        assert_eq!(k["gallery_name"], "Big & Small");
        assert_eq!(keywords(&files[2])["gallery_name"], "Big & Small");
        assert_eq!(keywords(&files[2])["extension"], "gif");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "11.png");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("sizebooru/Big & Small (7)")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{id}",["11"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /Galleries/List/7?pageNo=1&pageSize=100 "));
        assert!(requests[1].starts_with("GET /Galleries/List/7?pageNo=2&pageSize=100 "));
    }

    #[tokio::test]
    async fn post_metadata() {
        let details = r#"<b>Posted Date:</b> 01/02/2020<br>
            <b>Approved Date:</b> 01/03/2020<br>
            <b>Approved By:</b> <a>Mod</a></li>
            <b>Posted By:</b> <a>Poster</a></li>
            <b>Artist:</b> N/A</li>
            <b>Views:</b> 42<br>
            <b>Source Link:</b> <a href="https://example.org/x">x</a></li>
            <h6>Related Tags</h6><ul><li>tag a</li><li>tag b</li></ul>
            <h6>Favorited By</h6><ul><li>fan</li></ul>
            <img src="/Picture/5" alt="My.Pic.PNG">"#;
        let (root, server) = serve(vec![html(details)]);
        let http = http("Size Booru", root).with_options(Some(std::sync::Arc::new(
            serde_json::json!({"metadata": true})
                .as_object()
                .unwrap()
                .clone(),
        )));
        let files = media(extract(
            &http,
            Url::parse("https://sizebooru.com/Details/5").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let k = keywords(&files[0]);
        assert_eq!(k["id"], 5);
        assert_eq!(k["date"], "2020-01-02 00:00:00");
        assert_eq!(k["date_approved"], "2020-01-03 00:00:00");
        assert_eq!(k["approver"], "Mod");
        assert_eq!(k["uploader"], "Poster");
        assert_eq!(k["artist"], Value::Null);
        assert_eq!(k["views"], 42);
        assert_eq!(k["source"], "https://example.org/x");
        assert_eq!(k["tags"], serde_json::json!(["tag a", "tag b"]));
        assert_eq!(k["favorite"], serde_json::json!(["fan"]));
        assert_eq!(k["filename"], "My.Pic");
        assert_eq!(k["extension"], "png");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "5.png");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("sizebooru")
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /Details/5 "));
    }
}
