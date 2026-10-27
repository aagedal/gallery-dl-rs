//! xHamster photo galleries and users, mirroring
//! gallery_dl/extractor/xhamster.py (`window.initials` page data).
use crate::error;
use crate::sites::gdl::{Scan, captures, date, extr, group, int_of, nameext_into, parse_ts, re};
use crate::sites::{Events, Http, Item, Route, Site};
use futures_util::StreamExt;
use serde_json::{Map, Value};
use url::Url;

const BASE: &str = r"^https?://((?:[\w-]+\.)?xhamster(?:\d?\.(?:com|one|desi)|\.porncache\.net))";

static GALLERY: Route = Route::new(
    "gallery",
    "{num:>03}_{id}.{extension}",
    &[
        "{category}",
        "{user[name]}",
        "{gallery[id]} {gallery[title]}",
    ],
    "{id}",
);
static USER: Route = Route::new("user", "{filename}.{extension}", &["{category}"], "");

pub static SITE: Site = Site {
    category: "xhamster",
    name: "xHamster",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&GALLERY, &USER],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Gallery(String, String),
    User(String, String),
}

fn target(url: &Url) -> Option<Target> {
    if let Some(c) = captures(re!(&format!(r"{BASE}(/photos/gallery/[^/?#]+)")), url) {
        return Some(Target::Gallery(group(&c, 1).into(), group(&c, 2).into()));
    }
    let c = captures(
        re!(&format!(r"{BASE}/users/([^/?#]+)(?:/photos)?/?(?:$|[?#])")),
        url,
    )?;
    Some(Target::User(group(&c, 1).into(), group(&c, 2).into()))
}

async fn initials(http: &Http, url: &str) -> crate::Result<Value> {
    let page = http.text(&http.api(url)?).await?;
    serde_json::from_str(
        extr(&page, "window.initials=", "</script>").trim_end_matches(['\n', '\r', ';']),
    )
    .map_err(|_| error("xHamster page has no initial data"))
}

fn metadata(data: &Value) -> Map<String, Value> {
    let gallery = &data["galleryPage"];
    let info = &gallery["infoProps"];
    let model = &gallery["galleryModel"];
    let author = &info["authorInfoProps"];
    let user = serde_json::json!({
        "id": int_of(model.get("userId")),
        "url": author["authorLink"],
        "name": author["authorName"],
        "verified": crate::sites::gdl::truthy(author.get("verified")),
        "subscribers": info["subscribeButtonProps"]["subscribers"],
    });
    let tags: Vec<Value> = info["categoriesTags"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|t| t["label"].clone())
        .collect();
    let details = serde_json::json!({
        "id": int_of(gallery.get("id")),
        "tags": tags,
        // Newer pages drop `galleryModel`; `infoProps` has these too.
        "date": date(
            model
                .get("created")
                .or_else(|| author.get("createdDate"))
                .and_then(parse_ts)
        ),
        "views": int_of(
            model
                .get("views")
                .or_else(|| info["viewsInfoProps"].get("views"))
        ),
        "likes": int_of(model["rating"].get("likes")),
        "dislikes": int_of(model["rating"].get("dislikes")),
        "title": model
            .get("title")
            .cloned()
            .unwrap_or_else(|| gallery["photoItems"][0]["alt"].clone()),
        "description": model["description"],
        "thumbnail": model["thumbURL"],
    });
    let mut result = Map::new();
    result.insert("user".into(), user);
    result.insert("gallery".into(), details);
    result.insert("count".into(), int_of(gallery.get("photosCount")).into());
    result
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported xHamster URL"));
    };
    crate::sites::once(async move {
        Ok(match target {
            Target::Gallery(host, path) => {
                let mut data = initials(http, &format!("https://{host}{path}")).await?;
                let meta = metadata(&data);
                let mut items = Vec::new();
                loop {
                    // Newer pages list `galleryPage.photoItems` instead.
                    let (photos, newer) = match data["photosGalleryModel"]["photos"].as_array() {
                        Some(photos) => (photos.clone(), false),
                        None => (
                            data["galleryPage"]["photoItems"]
                                .as_array()
                                .cloned()
                                .unwrap_or_default(),
                            true,
                        ),
                    };
                    for photo in &photos {
                        let Some(mut image) = photo.as_object().cloned() else {
                            continue;
                        };
                        let raw = image
                            .get(if newer { "imgSrc" } else { "imageURL" })
                            .map(crate::sites::scalar)
                            .unwrap_or_default();
                        let Ok(file) = Url::parse(&raw) else { continue };
                        image.extend(meta.clone());
                        nameext_into(&raw, &mut image);
                        image.insert("num".into(), (items.len() + 1).into());
                        if !newer {
                            image.insert("extension".into(), "webp".into());
                        }
                        image.remove("modelName");
                        let id = image
                            .get("id")
                            .map(crate::sites::scalar)
                            .unwrap_or_default();
                        items.push(
                            Item::new(file, image)
                                .ids(crate::sites::scalar(&meta["gallery"]["id"]), id),
                        );
                    }
                    let pagination = &data["galleryPage"]["paginationProps"];
                    let current = int_of(pagination.get("currentPageNumber"));
                    if current >= int_of(pagination.get("lastPageNumber")) {
                        break;
                    }
                    let template = crate::sites::scalar(&pagination["pageLinkTemplate"]);
                    let next = format!(
                        "{}{}",
                        &template[..template.len().saturating_sub(3)],
                        current + 1
                    );
                    data = initials(http, &next).await?;
                }
                crate::sites::events(crate::sites::post(&SITE, http, &GALLERY, items))
            }
            Target::User(host, user) => {
                let mut url = format!("https://{host}/users/{user}/photos");
                let mut galleries = Vec::new();
                loop {
                    let page = http.text(&http.api(&url)?).await?;
                    let mut scan = Scan::new(&page);
                    while let Some(link) = scan
                        .find(" role-pop\" href=\"", "\"")
                        .filter(|l| !l.is_empty())
                    {
                        if let Ok(link) = Url::parse(link) {
                            galleries.push(link);
                        }
                    }
                    match scan
                        .find("data-page=\"next\" href=\"", "\"")
                        .filter(|n| !n.is_empty())
                    {
                        Some(next) => url = next.to_owned(),
                        None => break,
                    }
                }
                futures_util::stream::iter(galleries)
                    .map(move |url| {
                        crate::sites::queue(http, SITE.category, USER.subcategory, url, &Map::new())
                    })
                    .flatten()
                    .boxed()
            }
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http as client, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn newer_gallery_metadata() {
        let data = serde_json::json!({"galleryPage": {"id": 15860946, "photosCount": 19,
            "photoItems": [{"id": 1, "alt": "Carwash", "imgSrc": "https://ic-ph-nss.xhcdn.com/a/x/jpeg/000/1_1000.jpg"}],
            "infoProps": {"authorInfoProps": {"createdDate": 1643783409, "authorName": "A"},
                "viewsInfoProps": {"views": 114282}}}});
        let meta = metadata(&data);
        assert_eq!(meta["gallery"]["title"], "Carwash");
        assert_eq!(meta["gallery"]["views"], 114282);
        assert_eq!(meta["gallery"]["date"], "2022-02-02 06:30:09");
    }

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://jp.xhamster2.com/photos/gallery/take-me-15860946"),
            Some(Target::Gallery(
                "jp.xhamster2.com".into(),
                "/photos/gallery/take-me-15860946".into()
            ))
        );
        assert!(t("https://xhamster.porncache.net/photos/gallery/11748968").is_some());
        assert_eq!(
            t("https://xhamster.com/users/daringsex/photos"),
            Some(Target::User("xhamster.com".into(), "daringsex".into()))
        );
        assert_eq!(t("https://xhamster.com/users/nickname68/videos"), None);
    }

    #[tokio::test]
    async fn pages_gallery_photos() {
        let page = |n: i64, id: i64| {
            let data = json!({"galleryPage": {"id": "11748968", "photosCount": 2,
                "infoProps": {"authorInfoProps": {"authorLink": "https://xhamster.com/users/x", "authorName": "x"},
                    "subscribeButtonProps": {"subscribers": 5}, "categoriesTags": [{"label": "Tag"}]},
                "galleryModel": {"userId": "7", "created": 1500000000, "views": "10", "rating": {"likes": 1, "dislikes": 0},
                    "title": "World", "description": "", "thumbURL": "t"},
                "paginationProps": {"currentPageNumber": n, "lastPageNumber": 2, "pageLinkTemplate": "https://xhamster.com/photos/gallery/x-11748968/{#}"}},
                "photosGalleryModel": {"photos": [{"id": id, "imageURL": format!("https://ic-ph-ny.xhcdn.com/a/{id}.jpg"), "modelName": "m"}]}});
            format!("<script>window.initials={data};\n</script>")
        };
        let (root, server) = serve(vec![html(&page(1, 1)), html(&page(2, 2))]);
        let http = client("xhamster", root);
        let files = media(extract(
            &http,
            Url::parse("https://xhamster.com/photos/gallery/x-11748968").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[1]);
        assert_eq!(k["num"], 2);
        assert_eq!(k["extension"], "webp");
        assert!(k.get("modelName").is_none());
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "002_2.webp");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("xhamster/x/11748968 World")
        );
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /photos/gallery/x-11748968/2 "));
    }
}
