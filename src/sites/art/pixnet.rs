//! PIXNET albums: images, sets, folders and users, mirroring
//! gallery_dl/extractor/pixnet.py.
use crate::error;
use crate::sites::gdl::{
    Scan, captures, extr, extract_iter, group, parse_int, re, rpartition, unescape,
};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

static IMAGE: Route = Route::new(
    "image",
    "{id}.{extension}",
    &["{category}", "{blog}"],
    "{id}",
);
static SET: Route = Route::new(
    "set",
    "{num:>03}_{id}.{extension}",
    &[
        "{category}",
        "{blog}",
        "{folder_id} {folder_title}",
        "{set_id} {set_title}",
    ],
    "{id}",
);
static FOLDER: Route = Route::new(
    "folder",
    "{num:>03}_{id}.{extension}",
    &["{category}"],
    "{id}",
);
static USER: Route = Route::new(
    "user",
    "{num:>03}_{id}.{extension}",
    &["{category}"],
    "{id}",
);

pub static SITE: Site = Site {
    category: "pixnet",
    name: "PIXNET",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&IMAGE, &SET, &FOLDER, &USER],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Image(String, String),
    Set(String, String),
    Folder(String, String),
    User(String),
}

fn target(url: &Url) -> Option<Target> {
    const BASE: &str = r"^https?://([\w-]+)\.pixnet.net";
    let blog = |c: &regex::Captures| (c[1] != *"www").then(|| group(c, 1).to_owned());
    if let Some(c) = captures(re!(&format!(r"{BASE}/album/photo/(\d+)")), url) {
        return Some(Target::Image(blog(&c)?, group(&c, 2).into()));
    }
    if let Some(c) = captures(re!(&format!(r"{BASE}/album/set/(\d+)")), url) {
        return Some(Target::Set(blog(&c)?, group(&c, 2).into()));
    }
    if let Some(c) = captures(re!(&format!(r"{BASE}/album/folder/(\d+)")), url) {
        return Some(Target::Folder(blog(&c)?, group(&c, 2).into()));
    }
    let c = captures(
        re!(&format!(r"{BASE}(?:/blog|/album(?:/list)?)?/?(?:$|[?#])")),
        url,
    )?;
    Some(Target::User(blog(&c)?))
}

/// `_pagination`: `<li>` entries across the "next" links.
async fn entries(http: &Http, root: &str, first: String) -> crate::Result<Vec<String>> {
    let mut page = first;
    let mut out = Vec::new();
    loop {
        out.extend(
            extract_iter(&page, "<li id=\"", "</li>")
                .into_iter()
                .map(str::to_owned),
        );
        let next = extr(&page, "class=\"nextBtn\"", ">");
        if !next.contains("href") {
            return Ok(out);
        }
        page = http
            .text(&http.api(&format!("{root}{}", extr(next, "href=\"", "\"")))?)
            .await?;
    }
}

fn user_of(page: &str) -> String {
    rpartition(extr(page, "<meta name=\"author\" content=\"", "\";"), " (")
        .0
        .to_owned()
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported PIXNET URL"));
    };
    crate::sites::once(async move {
        match target {
            Target::Image(blog, id) => {
                let mut api = http.api("https://api.pixnet.cc/oembed")?;
                api.query_pairs_mut()
                    .append_pair(
                        "url",
                        &format!("https://{blog}.pixnet.net/album/photo/{id}"),
                    )
                    .append_pair("format", "json");
                let mut data = http
                    .json(&api)
                    .await?
                    .as_object()
                    .cloned()
                    .ok_or_else(|| error("PIXNET returned no image data"))?;
                let file = data
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                data.insert(
                    "id".into(),
                    parse_int(
                        rpartition(&file, "/")
                            .2
                            .split('-')
                            .next()
                            .unwrap_or_default(),
                    )
                    .into(),
                );
                let title = data
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let (stem, _, ext) = rpartition(&title, ".");
                data.insert("filename".into(), stem.into());
                data.insert("extension".into(), ext.into());
                data.insert("blog".into(), blog.into());
                let author = data.remove("author_name").unwrap_or(Value::Null);
                data.insert("user".into(), author);
                let file = Url::parse(&file).map_err(|_| error("PIXNET image has no file"))?;
                Ok(crate::sites::events(crate::sites::post(
                    &SITE,
                    http,
                    &IMAGE,
                    vec![Item::new(file, data).ids(id.clone(), id)],
                )))
            }
            Target::Set(blog, id) => {
                let root = format!("https://{blog}.pixnet.net");
                let page = http
                    .text(&http.api(&format!("{root}/album/set/{id}"))?)
                    .await?;
                let mut scan = Scan::new(&page);
                let user = rpartition(scan.extr("<meta name=\"author\" content=\"", "\";"), " (")
                    .0
                    .to_owned();
                scan.extr("id=\"breadcrumb\"", "");
                let fid = scan.extr("/folder/", "\"").to_owned();
                let fname = scan.extr(">", "<").to_owned();
                let sid = scan.extr("/set/", "\"").to_owned();
                let sname = scan.extr(">", "<").to_owned();
                let mut data = Map::new();
                data.insert("blog".into(), blog.as_str().into());
                data.insert("user".into(), user.into());
                data.insert(
                    "folder_id".into(),
                    fid.parse::<i64>()
                        .map_or_else(|_| Value::from(""), Value::from),
                );
                data.insert("folder_title".into(), unescape(&fname).trim().into());
                data.insert("set_id".into(), parse_int(&sid).into());
                data.insert("set_title".into(), unescape(&sname).into());
                let items = entries(http, &root, page.clone())
                    .await?
                    .iter()
                    .enumerate()
                    .filter_map(|(index, info)| {
                        let mut scan = Scan::new(info);
                        let href = scan.extr(" href=\"", "\"");
                        let src = scan.extr(" src=\"", "\"");
                        let alt = scan.extr(" alt=\"", "\"");
                        let url = src.replace("_s.", ".");
                        let mut photo = Map::new();
                        let photo_id = parse_int(
                            rpartition(href, "/")
                                .2
                                .split('#')
                                .next()
                                .unwrap_or_default(),
                        );
                        photo.insert("id".into(), photo_id.into());
                        photo.insert("url".into(), url.as_str().into());
                        photo.insert("num".into(), (index + 1).into());
                        photo.insert("filename".into(), alt.into());
                        photo.insert("extension".into(), rpartition(src, ".").2.into());
                        photo.extend(data.clone());
                        Some(
                            Item::new(Url::parse(&url).ok()?, photo)
                                .ids(id.clone(), photo_id.to_string()),
                        )
                    })
                    .collect();
                Ok(crate::sites::events(crate::sites::post(
                    &SITE, http, &SET, items,
                )))
            }
            Target::Folder(blog, id) => {
                listing(http, &FOLDER, blog, format!("/album/folder/{id}")).await
            }
            Target::User(blog) => listing(http, &USER, blog, "/album/list".into()).await,
        }
    })
}

/// Folders and users queue their sets and folders.
async fn listing<'a>(
    http: &'a Http,
    route: &'static Route,
    blog: String,
    path: String,
) -> crate::Result<Events<'a>> {
    let root = format!("https://{blog}.pixnet.net");
    let page = http.text(&http.api(&format!("{root}{path}"))?).await?;
    let user = user_of(&page);
    let children = entries(http, &root, page)
        .await?
        .iter()
        .filter_map(|info| {
            let mut scan = Scan::new(info);
            let href = scan.extr(" href=\"", "\"").to_owned();
            let alt = scan.extr(" alt=\"", "\"");
            let mut item = Map::new();
            item.insert("id".into(), parse_int(rpartition(&href, "/").2).into());
            item.insert("title".into(), unescape(alt).into());
            item.insert("blog".into(), blog.as_str().into());
            item.insert("user".into(), user.as_str().into());
            let href = if href.starts_with('/') {
                format!("{root}{href}")
            } else {
                href
            };
            Some((Url::parse(&href).ok()?, item))
        })
        .collect();
    Ok(crate::sites::galleries::queue(http, &SITE, route, children))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{children, html, http as client, keywords, media, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://albertayu773.pixnet.net/album/photo/159443828"),
            Some(Target::Image("albertayu773".into(), "159443828".into()))
        );
        assert_eq!(
            t("https://anrine910070.pixnet.net/album/set/5917493"),
            Some(Target::Set("anrine910070".into(), "5917493".into()))
        );
        assert_eq!(
            t("https://ginkoyo.pixnet.net/album/folder/1052271"),
            Some(Target::Folder("ginkoyo".into(), "1052271".into()))
        );
        assert_eq!(
            t("https://albertayu773.pixnet.net/album/list"),
            Some(Target::User("albertayu773".into()))
        );
        assert_eq!(t("https://www.pixnet.net/"), None);
    }

    #[tokio::test]
    async fn set_reads_breadcrumb_and_photos() {
        let page = r#"<meta name="author" content="Jane (jane)";><div id="breadcrumb"><a href="/album/folder/12">Folder </a><a href="/album/set/34">Set</a>
            <li id="p1"><a href="https://jane.pixnet.net/album/photo/56#x"><img src="https://pic.pimg.tw/jane/a_s.jpg" alt="Alpha"></a></li>"#;
        let (root, server) = serve(vec![html(page)]);
        let http = client("pixnet", root);
        let files = media(extract(
            &http,
            Url::parse("https://jane.pixnet.net/album/set/34").unwrap(),
        ))
        .await;
        assert_eq!(files[0].url.as_str(), "https://pic.pimg.tw/jane/a.jpg");
        let k = keywords(&files[0]);
        assert_eq!(
            (
                k["folder_id"].as_i64(),
                k["set_title"].as_str(),
                k["user"].as_str(),
                k["id"].as_i64()
            ),
            (Some(12), Some("Set"), Some("Jane"), Some(56))
        );
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "001_56.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("pixnet/jane/12 Folder/34 Set")
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn user_follows_next_button() {
        let first = r#"<meta name="author" content="Jane (jane)";><li id="a"><a href="https://jane.pixnet.net/album/set/1"><img alt="One"></a></li><a class="nextBtn" href="/album/list?page=2">"#;
        let second = r#"<li id="b"><a href="https://jane.pixnet.net/album/folder/2"><img alt="Two"></a></li><span class="nextBtn">"#;
        let (root, server) = serve(vec![html(first), html(second)]);
        let http = client("pixnet", root);
        assert_eq!(
            children(extract(
                &http,
                Url::parse("https://jane.pixnet.net/").unwrap()
            ))
            .await
            .len(),
            2
        );
        server.join().unwrap();
    }
}
