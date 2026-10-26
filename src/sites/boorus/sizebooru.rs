//! Size Booru posts, tags, galleries, uploads and favorites, mirroring
//! gallery_dl/extractor/sizebooru.py. Option: `metadata` (fetch details).
use crate::error;
use crate::sites::boorus::per_post;
use crate::sites::gdl::{
    Scan, captures, date, extr, extract_iter, group, parse_int, re, remove_html, rpartition,
    split_html, strptime, unescape, unquote,
};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://sizebooru.com";
const PER_PAGE: usize = 100;

static POST: Route = Route::new("post", "{id}.{extension}", &["{category}"], "{id}");
static TAG: Route = Route::new(
    "tag",
    "{id}.{extension}",
    &["{category}", "{search_tags}"],
    "{id}",
);
static GALLERY: Route = Route::new(
    "gallery",
    "{id}.{extension}",
    &["{category}", "{gallery_name} ({gallery_id})"],
    "{id}",
);
static USER: Route = Route::new(
    "user",
    "{id}.{extension}",
    &["{category}", "Uploads {user}"],
    "{id}",
);
static FAVORITE: Route = Route::new(
    "favorite",
    "{id}.{extension}",
    &["{category}", "Favorites {user}"],
    "{id}",
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
    /// Route, listing path and its keyword.
    List(&'static str, String, String),
}

fn target(url: &Url) -> Option<Target> {
    const BASE: &str = r"^https?://(?:www\.)?sizebooru\.com";
    if let Some(c) = captures(re!(&format!(r"{BASE}/Details/(\d+)")), url) {
        return Some(Target::Post(group(&c, 1).into()));
    }
    for (kind, pattern) in [
        ("tag", re!(&format!(r"{BASE}/Search/([^/?#]+)"))),
        ("gallery", re!(&format!(r"{BASE}/Galleries/List/(\d+)"))),
        ("user", re!(&format!(r"{BASE}/Profile/Uploads/([^/?#]+)"))),
        (
            "favorite",
            re!(&format!(r"{BASE}/Profile/Favorites/([^/?#]+)")),
        ),
    ] {
        if let Some(c) = captures(pattern, url) {
            let value = group(&c, 1).to_owned();
            let path = match kind {
                "tag" => format!("/Search/{value}"),
                "gallery" => format!("/Galleries/List/{value}"),
                "user" => format!("/Profile/Uploads/{value}"),
                _ => format!("/Profile/Favorites/{value}"),
            };
            return Some(Target::List(kind, path, value));
        }
    }
    None
}

/// `BooruExtractor.items` with `_prepare` or `_prepare_metadata`.
async fn post_item(
    http: &Http,
    id: &str,
    extension: Option<&str>,
    extra: &Map<String, Value>,
) -> crate::Result<Item> {
    let raw = format!("{ROOT}/Picture/{id}");
    let mut post = Map::new();
    post.insert("file_url".into(), raw.as_str().into());
    post.insert("id".into(), parse_int(id).into());
    post.insert("filename".into(), id.into());
    let mut ext = extension.unwrap_or_default().to_owned();
    if http.option_bool("metadata", false) {
        let page = http
            .text(&http.api(&format!("{ROOT}/Details/{id}"))?)
            .await?;
        let mut scan = Scan::new(&page);
        post.insert(
            "date".into(),
            date(strptime(scan.extr("<b>Posted Date:</b> ", "<"), "%m/%d/%Y")),
        );
        post.insert(
            "date_approved".into(),
            date(strptime(
                scan.extr("<b>Approved Date:</b> ", "<"),
                "%m/%d/%Y",
            )),
        );
        post.insert(
            "approver".into(),
            remove_html(scan.extr("<b>Approved By:</b>", "</")).into(),
        );
        post.insert(
            "uploader".into(),
            remove_html(scan.extr("<b>Posted By:</b>", "</")).into(),
        );
        let artist = scan.extr("<b>Artist:</b> ", "</");
        post.insert(
            "artist".into(),
            if artist == "N/A" {
                Value::Null
            } else {
                remove_html(artist).into()
            },
        );
        post.insert(
            "views".into(),
            parse_int(scan.extr("<b>Views:</b>", "<")).into(),
        );
        let source = extr(scan.extr("<b>Source Link:</b>", "</"), " href=\"", "\"");
        post.insert(
            "source".into(),
            if source.is_empty() {
                Value::Null
            } else {
                source.into()
            },
        );
        post.insert(
            "tags".into(),
            split_html(scan.extr("<h6>Related Tags</h6>", "</ul>")).into(),
        );
        post.insert(
            "favorite".into(),
            split_html(scan.extr("<h6>Favorited By</h6>", "</ul>")).into(),
        );
        let (name, _, file_ext) = rpartition(scan.extr("\" alt=\"", "\""), ".");
        post.insert("filename".into(), name.into());
        if ext.is_empty() {
            ext = file_ext.to_lowercase();
        }
    } else if ext.is_empty() {
        ext = "jpg".into();
    }
    post.insert("extension".into(), ext.into());
    post.extend(extra.clone());
    let url = Url::parse(&raw).map_err(|_| error("invalid Size Booru file URL"))?;
    Ok(Item::new(url, post).ids(id, id))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Size Booru URL"));
    };
    match target {
        Target::Post(id) => crate::sites::once(async move {
            let item = post_item(http, &id, None, &Map::new()).await?;
            Ok(crate::sites::events(per_post(
                &SITE,
                http,
                &POST,
                vec![item],
            )))
        }),
        Target::List(kind, path, value) => {
            let route: &'static Route = match kind {
                "tag" => &TAG,
                "gallery" => &GALLERY,
                "user" => &USER,
                _ => &FAVORITE,
            };
            let mut extra = Map::new();
            match kind {
                "tag" => {
                    extra.insert("search_tags".into(), unquote(&value).into());
                }
                "gallery" => {
                    extra.insert("gallery_id".into(), parse_int(&value).into());
                }
                _ => {
                    extra.insert("user".into(), unquote(&value).into());
                }
            }
            crate::sites::paginate(Some((1i64, extra)), move |state| {
                let path = path.clone();
                async move {
                    let Some((number, mut extra)) = state else {
                        return Ok(None);
                    };
                    let mut url = http.api(&format!("{ROOT}{path}"))?;
                    url.query_pairs_mut()
                        .append_pair("pageNo", &number.to_string())
                        .append_pair("pageSize", &PER_PAGE.to_string());
                    let page = http.text(&url).await?;
                    if kind == "gallery" && number == 1 {
                        extra.insert(
                            "gallery_name".into(),
                            unescape(extr(&page, "<title>Gallery: ", " - Size Booru<")).into(),
                        );
                    }
                    let thumbs = extract_iter(&page, "<a href=\"/Details/", ";base64");
                    let mut events = Vec::new();
                    for thumb in &thumbs {
                        let id = &thumb[..thumb.find('"').unwrap_or(thumb.len())];
                        let ext = &thumb[thumb.rfind('/').map_or(0, |i| i + 1)..];
                        let item = post_item(http, id, Some(ext), &extra).await?;
                        events.extend(per_post(&SITE, http, route, vec![item]));
                    }
                    let done = thumbs.is_empty()
                        || extr(&page, "area-label=\"Next\"", ">").contains("disabled");
                    Ok(Some((
                        crate::sites::events(events),
                        (!done).then_some((number + 1, extra)),
                    )))
                }
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http as client, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://sizebooru.com/Details/283342"),
            Some(Target::Post("283342".into()))
        );
        assert_eq!(
            t("https://sizebooru.com/Galleries/List/7"),
            Some(Target::List(
                "gallery",
                "/Galleries/List/7".into(),
                "7".into()
            ))
        );
        assert_eq!(
            t("https://sizebooru.com/Profile/Favorites/GtsXxx"),
            Some(Target::List(
                "favorite",
                "/Profile/Favorites/GtsXxx".into(),
                "GtsXxx".into()
            ))
        );
    }

    #[tokio::test]
    async fn pages_gallery_thumbnails() {
        let first = r#"<title>Gallery: Giant &amp; Tiny - Size Booru</title>
            <a href="/Details/11"><img src="data:image/png;base64,xx"></a><a href="/Details/12"><img src="data:image/gif;base64,yy"></a>
            <li><a area-label="Next" class="page-link" href="?pageNo=2">"#;
        let second = r#"<a href="/Details/13"><img src="data:image/jpeg;base64,zz"></a><a area-label="Next" class="disabled">"#;
        let (root, server) = serve(vec![html(first), html(second)]);
        let http = client("sizebooru", root);
        let files = media(extract(
            &http,
            Url::parse("https://sizebooru.com/Galleries/List/7").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].url.as_str(), "https://sizebooru.com/Picture/11");
        let k = keywords(&files[1]);
        assert_eq!(k["extension"], "gif");
        assert_eq!(k["gallery_name"], "Giant & Tiny");
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "12.gif");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("sizebooru/Giant & Tiny (7)")
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /Galleries/List/7?pageNo=1&pageSize=100 "));
        assert!(requests[1].contains("pageNo=2"));
    }
}
