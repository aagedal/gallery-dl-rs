//! xHamster photo galleries and user gallery listings, mirroring
//! gallery_dl/extractor/xhamster.py: each gallery page embeds its data as
//! `window.initials`, whose pagination names the following pages. Files
//! keep the host the URL used; one post per gallery page.
use crate::Result;
use crate::error;
use crate::sites::art::{Scan, date, extr, json_loads, parse_ts, segment, str_of, valid_label};
use crate::sites::manga::text::parse_int_value;
use crate::sites::{Events, Http, Item, Route, Site};
use futures_util::{StreamExt, stream};
use serde_json::{Map, Value, json};
use url::Url;

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

/// Gallery pages read at most.
const MAX_PAGES: i64 = 1000;

#[derive(Debug, PartialEq)]
enum Target {
    /// Root and gallery path.
    Gallery(String, String),
    /// Root and user name.
    User(String, String),
}

/// `(?:[\w-]+\.)?xhamster(?:\d?\.(?:com|one|desi)|\.porncache\.net)`.
fn on_site(url: &Url) -> bool {
    let Some(host) = url.host_str().filter(|_| crate::sites::plain(url)) else {
        return false;
    };
    let base = match host.split_once('.') {
        Some((label, rest)) if valid_label(label) && rest.starts_with("xhamster") => rest,
        _ => host,
    };
    let Some(rest) = base.strip_prefix("xhamster") else {
        return false;
    };
    let rest = rest
        .strip_prefix(|c: char| c.is_ascii_digit())
        .unwrap_or(rest);
    matches!(rest, ".com" | ".one" | ".desi") || base == "xhamster.porncache.net"
}

fn target(url: &Url) -> Option<Target> {
    if !on_site(url) {
        return None;
    }
    let root = format!("https://{}", url.host_str()?);
    let path = url.path();
    if let Some(rest) = path.strip_prefix("/photos/gallery/") {
        let name = &rest[..rest.find('/').unwrap_or(rest.len())];
        return segment(name).then(|| Target::Gallery(root, format!("/photos/gallery/{name}")));
    }
    let rest = path.strip_prefix("/users/")?;
    let (user, tail) = rest.split_once('/').unwrap_or((rest, ""));
    let tail = tail.strip_prefix("photos").unwrap_or(tail);
    (segment(user) && matches!(tail, "" | "/")).then(|| Target::User(root, user.to_owned()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Gallery(root, path)) => gallery(http, root, path),
        Some(Target::User(root, user)) => user_galleries(http, root, user),
        None => crate::sites::fail(error("unsupported xHamster URL")),
    }
}

/// `_extract_data`: the page's `window.initials` JSON.
async fn initials(http: &Http, url: &str) -> Result<Value> {
    let page = http.text(&http.api(url)?).await?;
    let data = extr(&page, "window.initials=", "</script>").trim_end_matches(['\n', '\r', ';']);
    json_loads(data, "xHamster")
}

fn gallery(http: &Http, root: String, path: String) -> Events<'_> {
    crate::sites::once(async move {
        let first = initials(http, &format!("{root}{path}")).await?;
        let data = metadata(&first)?;
        Ok(crate::sites::paginate(
            Some((first, 0usize, 1i64)),
            move |state| {
                let data = data.clone();
                async move {
                    let Some((page, num, count)) = state else {
                        return Ok(None);
                    };
                    let photos = page
                        .pointer("/photosGalleryModel/photos")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    let mut next_num = num;
                    let items: Vec<Item> = photos
                        .into_iter()
                        .filter_map(|photo| {
                            next_num += 1;
                            item(photo, &data, next_num)
                        })
                        .collect();
                    let events = crate::sites::post(&SITE, http, &GALLERY, items);
                    let pagination = page.pointer("/galleryPage/paginationProps");
                    let current =
                        parse_int_value(pagination.and_then(|p| p.get("currentPageNumber")));
                    let last = parse_int_value(pagination.and_then(|p| p.get("lastPageNumber")));
                    let template = pagination
                        .map(|p| str_of(p, "pageLinkTemplate"))
                        .unwrap_or_default();
                    if current >= last || count >= MAX_PAGES || template.chars().count() < 3 {
                        return Ok(Some((crate::sites::events(events), None)));
                    }
                    let cut = template.char_indices().rev().nth(2).map_or(0, |(i, _)| i);
                    let next = format!("{}{}", &template[..cut], current + 1);
                    // Later pages must stay on the gallery's own site.
                    if !Url::parse(&next).is_ok_and(|url| on_site(&url)) {
                        return Err(error("xHamster pagination left the site"));
                    }
                    let page = initials(http, &next).await?;
                    Ok(Some((
                        crate::sites::events(events),
                        Some((page, next_num, count + 1)),
                    )))
                }
            },
        ))
    })
}

fn metadata(data: &Value) -> Result<Map<String, Value>> {
    let gallery = data
        .get("galleryPage")
        .ok_or_else(|| error("xHamster page has no gallery"))?;
    let info = &gallery["infoProps"];
    let model = &gallery["galleryModel"];
    let author = &info["authorInfoProps"];
    let tags: Vec<Value> = info["categoriesTags"]
        .as_array()
        .map(|tags| tags.iter().map(|t| t["label"].clone()).collect())
        .unwrap_or_default();
    let mut out = Map::new();
    out.insert(
        "user".into(),
        json!({
            "id": parse_int_value(model.get("userId")),
            "url": author["authorLink"],
            "name": author["authorName"],
            "verified": crate::sites::art::truthy(author.get("verified")),
            "subscribers": info["subscribeButtonProps"]["subscribers"],
        }),
    );
    out.insert(
        "gallery".into(),
        json!({
            "id": parse_int_value(gallery.get("id")),
            "tags": tags,
            "date": date(parse_ts(&model["created"])),
            "views": parse_int_value(model.get("views")),
            "likes": parse_int_value(model["rating"].get("likes")),
            "dislikes": parse_int_value(model["rating"].get("dislikes")),
            "title": model["title"],
            "description": model["description"],
            "thumbnail": model["thumbURL"],
        }),
    );
    out.insert(
        "count".into(),
        parse_int_value(gallery.get("photosCount")).into(),
    );
    Ok(out)
}

fn item(photo: Value, data: &Map<String, Value>, num: usize) -> Option<Item> {
    let Value::Object(mut image) = photo else {
        return None;
    };
    let raw = image.get("imageURL").and_then(Value::as_str)?.to_owned();
    let url = crate::sites::media::web_url(&raw)?;
    crate::sites::art::update(&mut image, data);
    crate::sites::art::nameext_into(&raw, &mut image);
    image.insert("num".into(), num.into());
    image.insert("extension".into(), "webp".into());
    image.remove("modelName");
    let id = image
        .get("id")
        .map(crate::sites::scalar)
        .unwrap_or_default();
    let gallery = data
        .get("gallery")
        .and_then(|g| g.get("id"))
        .map(crate::sites::scalar)
        .unwrap_or_default();
    let user = data
        .get("user")
        .and_then(|u| u.get("name"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    Some(Item::new(url, image).ids(gallery, id).user(user))
}

/// `XhamsterUserExtractor.items`: gallery links page by page.
fn user_galleries(http: &Http, root: String, user: String) -> Events<'_> {
    let first = format!("{root}/users/{user}/photos");
    crate::sites::paginate(Some(first), move |url| async move {
        let Some(url) = url else {
            return Ok(None);
        };
        let page = http.text(&http.api(&url)?).await?;
        let mut scan = Scan::new(&page);
        let mut children = Vec::new();
        loop {
            let link = scan.extr(" role-pop\" href=\"", "\"");
            if link.is_empty() {
                break;
            }
            if let Ok(child) = Url::parse(link)
                && matches!(target(&child), Some(Target::Gallery(..)))
            {
                children.push(child);
            }
        }
        let next = scan.extr("data-page=\"next\" href=\"", "\"");
        let next = Url::parse(next).ok().filter(on_site).map(String::from);
        let events = stream::iter(children)
            .map(move |child| {
                crate::sites::queue(http, SITE.category, USER.subcategory, child, &Map::new())
            })
            .flatten()
            .boxed();
        Ok(Some((events, next)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::manga::tests::{archive, path};
    use crate::sites::testing::{html, http, keywords, media, queued, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://xhamster.com/photos/gallery/trip-12345"),
            Some(Target::Gallery(
                "https://xhamster.com".into(),
                "/photos/gallery/trip-12345".into()
            ))
        );
        assert_eq!(
            t("https://de.xhamster2.com/photos/gallery/x/2"),
            Some(Target::Gallery(
                "https://de.xhamster2.com".into(),
                "/photos/gallery/x".into()
            ))
        );
        assert_eq!(
            t("https://xhamster.porncache.net/users/someone/photos?page=2"),
            Some(Target::User(
                "https://xhamster.porncache.net".into(),
                "someone".into()
            ))
        );
        assert_eq!(
            t("https://xhamster.desi/users/someone"),
            Some(Target::User(
                "https://xhamster.desi".into(),
                "someone".into()
            ))
        );
        for raw in [
            "https://xhamster.com/users/someone/videos",
            "https://xhamster.com/videos/x",
            "https://xhamster12.com/photos/gallery/x",
            "https://a.b.xhamster.com/photos/gallery/x",
            "https://xhamster.com.evil.test/photos/gallery/x",
            "https://xhamster.com:8443/photos/gallery/x",
            "https://u:p@xhamster.com/photos/gallery/x",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn page(current: i64, last: i64, ids: &[u64]) -> String {
        let photos: Vec<Value> = ids
            .iter()
            .map(|id| {
                json!({"id": id, "imageURL": format!("https://ic.xh.test/a/{id}/orig.jpg"),
                    "modelName": "photo", "pageURL": format!("https://xhamster.com/photos/view/{id}")})
            })
            .collect();
        let data = json!({
            "galleryPage": {
                "id": "12345", "photosCount": "3",
                "infoProps": {
                    "authorInfoProps": {"authorLink": "https://xhamster.com/users/someone",
                        "authorName": "someone", "verified": true},
                    "subscribeButtonProps": {"subscribers": 10},
                    "categoriesTags": [{"label": "Travel"}],
                },
                "galleryModel": {"userId": "77", "created": 1577934245, "views": 5,
                    "rating": {"likes": 2, "dislikes": 1}, "title": "Trip", "description": "",
                    "thumbURL": "https://ic.xh.test/t.jpg"},
                "paginationProps": {"currentPageNumber": current, "lastPageNumber": last,
                    "pageLinkTemplate": "https://xhamster.com/photos/gallery/trip-12345/{#}"},
            },
            "photosGalleryModel": {"photos": photos},
        });
        format!("<script>window.initials={data};\n</script>")
    }

    #[tokio::test]
    async fn extracts_gallery_pages() {
        let (root, server) = serve(vec![html(&page(1, 2, &[1, 2])), html(&page(2, 2, &[3]))]);
        let http = http("xhamster", root);
        let files = media(extract(
            &http,
            Url::parse("https://xhamster.com/photos/gallery/trip-12345").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 3);
        let k = keywords(&files[2]);
        assert_eq!(k["num"], 3);
        assert_eq!(k["extension"], "webp");
        assert!(!k.contains_key("modelName"));
        assert_eq!(k["user"]["id"], 77);
        assert_eq!(k["user"]["verified"], true);
        assert_eq!(k["gallery"]["tags"], json!(["Travel"]));
        assert_eq!(k["gallery"]["date"], "2020-01-02 03:04:05");
        assert_eq!(k["count"], 3);
        assert_eq!(path(&files[2]), "xhamster/someone/12345 Trip/003_3.webp");
        assert_eq!(archive(&files[2]), r#"["{id}",["3"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /photos/gallery/trip-12345 "));
        assert!(requests[1].starts_with("GET /photos/gallery/trip-12345/2 "));
    }

    #[tokio::test]
    async fn queues_user_galleries() {
        let first = "<a class=\"x role-pop\" href=\"https://xhamster.com/photos/gallery/a-1\"></a>\
<a class=\"y role-pop\" href=\"https://elsewhere.test/photos/gallery/b-2\"></a>\
<a data-page=\"next\" href=\"https://xhamster.com/users/someone/photos/2\">";
        let last = "<a class=\"x role-pop\" href=\"https://xhamster.com/photos/gallery/c-3\"></a>";
        let (root, server) = serve(vec![html(first), html(last)]);
        let http = http("xhamster", root);
        let children = queued(extract(
            &http,
            Url::parse("https://xhamster.com/users/someone").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 2);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /users/someone/photos "));
        assert!(requests[1].starts_with("GET /users/someone/photos/2 "));
    }
}
