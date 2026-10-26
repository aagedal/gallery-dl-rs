//! XVIDEOS profile photo galleries, mirroring gallery_dl/extractor/xvideos.py.
use crate::error;
use crate::sites::gdl::{
    Scan, captures, extr, extract_iter, group, parse_int, re, rextr, unescape,
};
use crate::sites::{Events, Http, Route, Site};
use futures_util::StreamExt;
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://www.xvideos.com";
const BASE: &str = r"^https?://(?:www\.)?xvideos\.com/(?:profiles|(?:amateur-|model-)?channels)";

static GALLERY: Route = Route::new(
    "gallery",
    "{category}_{gallery[id]}_{num:>03}.{extension}",
    &[
        "{category}",
        "{user[name]}",
        "{gallery[id]} {gallery[title]}",
    ],
    "{gallery[id]}_{num}",
);
static USER: Route = Route::new("user", "{filename}.{extension}", &["{category}"], "");

pub static SITE: Site = Site {
    category: "xvideos",
    name: "XVIDEOS",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&GALLERY, &USER],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Gallery(String, String),
    User(String),
}

fn target(url: &Url) -> Option<Target> {
    if let Some(c) = captures(re!(&format!(r"{BASE}/([^/?#]+)/photos/(\d+)")), url) {
        return Some(Target::Gallery(group(&c, 1).into(), group(&c, 2).into()));
    }
    captures(re!(&format!(r"{BASE}/([^/?#]+)/?(?:#.*)?$")), url)
        .map(|c| Target::User(group(&c, 1).into()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported XVIDEOS URL"));
    };
    crate::sites::once(async move {
        Ok(match target {
            Target::Gallery(user, id) => {
                let mut page = http
                    .text(&http.api(&format!("{ROOT}/profiles/{user}/photos/{id}"))?)
                    .await?;
                let mut scan = Scan::new(&page);
                let mut info = Map::new();
                info.insert(
                    "id".into(),
                    parse_int(scan.extr("\"id_user\":", ",")).into(),
                );
                info.insert("display".into(), scan.extr("\"display\":\"", "\"").into());
                info.insert("sex".into(), scan.extr("\"sex\":\"", "\"").into());
                info.insert("name".into(), user.as_str().into());
                let title = scan.extr("\"title\":\"", "\"").to_owned();
                info.insert(
                    "description".into(),
                    scan.extr("<small class=\"mobile-hide\">", "</small>")
                        .trim()
                        .into(),
                );
                let tags = scan.extr("<em>Tagged:</em>", "<").trim().to_owned();
                let tags: Vec<String> = if tags.is_empty() {
                    Vec::new()
                } else {
                    unescape(&tags).split(", ").map(str::to_owned).collect()
                };
                let mut gallery = Map::new();
                gallery.insert("id".into(), parse_int(&id).into());
                gallery.insert("title".into(), unescape(&title).into());
                gallery.insert("tags".into(), tags.into());
                let mut data = Map::new();
                data.insert("user".into(), Value::Object(info));
                data.insert("gallery".into(), Value::Object(gallery));
                let needle = "<a class=\"embed-responsive-item\" href=\"";
                let mut images: Vec<String> = extract_iter(&page, needle, "\"")
                    .into_iter()
                    .map(str::to_owned)
                    .collect();
                while !images.is_empty() && images.len().is_multiple_of(500) {
                    let path = rextr(&page, " href=\"", "\"", page.find(">Next</"));
                    if path.is_empty() {
                        break;
                    }
                    page = http.text(&http.api(&format!("{ROOT}{path}"))?).await?;
                    images.extend(
                        extract_iter(&page, needle, "\"")
                            .into_iter()
                            .map(str::to_owned),
                    );
                }
                let images = images.into_iter().map(|u| (u, None)).collect();
                crate::sites::events(super::numbered(&SITE, http, &GALLERY, &data, images, &id))
            }
            Target::User(user) => {
                let page = http
                    .text(&http.api(&format!("{ROOT}/profiles/{user}"))?)
                    .await?;
                let conf: Value = serde_json::from_str(extr(&page, "xv.conf=", ";</script>"))
                    .map_err(|_| error("XVIDEOS profile has no configuration"))?;
                let mut galleries: Vec<(i64, Map<String, Value>)> = conf["data"]["galleries"]
                    .as_object()
                    .into_iter()
                    .flatten()
                    .filter(|(id, _)| id.as_str() != "0")
                    .map(|(id, gallery)| {
                        let mut data = Map::new();
                        data.insert("id".into(), parse_int(id).into());
                        data.insert(
                            "title".into(),
                            unescape(crate::sites::gdl::str_of(gallery, "title")).into(),
                        );
                        data.insert("count".into(), gallery["nb_pics"].clone());
                        (parse_int(id), data)
                    })
                    .collect();
                galleries.sort_by_key(|(id, _)| *id);
                let children: Vec<(Url, Map<String, Value>)> = galleries
                    .into_iter()
                    .filter_map(|(id, data)| {
                        Some((
                            Url::parse(&format!("{ROOT}/profiles/{user}/photos/{id}")).ok()?,
                            data,
                        ))
                    })
                    .collect();
                futures_util::stream::iter(children)
                    .map(move |(url, data)| {
                        crate::sites::queue(http, SITE.category, USER.subcategory, url, &data)
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
    use crate::sites::testing::{children, html, http as client, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.xvideos.com/profiles/pervertedcouple/photos/751031"),
            Some(Target::Gallery("pervertedcouple".into(), "751031".into()))
        );
        assert_eq!(
            t("https://www.xvideos.com/amateur-channels/pervertedcouple/photos/751031"),
            Some(Target::Gallery("pervertedcouple".into(), "751031".into()))
        );
        assert_eq!(
            t("https://www.xvideos.com/profiles/pervertedcouple#_tabPhotos"),
            Some(Target::User("pervertedcouple".into()))
        );
    }

    #[tokio::test]
    async fn extracts_gallery() {
        let page = r#""id_user":123,"display":"Couple","sex":"Couple","title":"Pics &amp; More"
            <small class="mobile-hide"> About </small><em>Tagged:</em> a, b <
            <a class="embed-responsive-item" href="https://profile-pics-cdn.xvideos-cdn.com/1.jpg">"#;
        let (root, server) = serve(vec![html(page)]);
        let http = client("xvideos", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.xvideos.com/profiles/pc/photos/751031").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let k = keywords(&files[0]);
        assert_eq!(k["gallery"]["tags"], serde_json::json!(["a", "b"]));
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "xvideos_751031_001.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("xvideos/pc/751031 Pics & More")
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn user_queues_galleries_by_id() {
        let page = r#"<script>xv.conf={"data":{"galleries":{"0":{"title":"all","nb_pics":9},"20":{"title":"B","nb_pics":1},"3":{"title":"A","nb_pics":2}}}};</script>"#;
        let (root, server) = serve(vec![html(page)]);
        let http = client("xvideos", root);
        assert_eq!(
            children(extract(
                &http,
                Url::parse("https://www.xvideos.com/profiles/pc").unwrap()
            ))
            .await
            .len(),
            2
        );
        server.join().unwrap();
    }
}
