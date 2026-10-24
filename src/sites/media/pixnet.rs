//! PIXNET album photos, sets, folders and users, mirroring
//! gallery_dl/extractor/pixnet.py: listing pages hold `<li>` entries and a
//! `nextBtn` link; single photos come from the oEmbed endpoint.
use super::word_char;
use crate::sites::art::{Scan, extr, extract_iter, file_url, parse_int, update};
use crate::sites::filehosts::rpartition;
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

const FILENAME: &str = "{num:>03}_{id}.{extension}";
const MEDIA_HOSTS: &[&str] = &["pixnet.net", "pimg.tw", "pixnet.cc"];
static IMAGE: Route = Route::new(
    "image",
    "{id}.{extension}",
    &["{category}", "{blog}"],
    "{id}",
);
static SET: Route = Route::new(
    "set",
    FILENAME,
    &[
        "{category}",
        "{blog}",
        "{folder_id} {folder_title}",
        "{set_id} {set_title}",
    ],
    "{id}",
);
static FOLDER: Route = Route::new("folder", FILENAME, &["{category}"], "{id}");
static USER: Route = Route::new("user", FILENAME, &["{category}"], "{id}");

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
enum Kind {
    Image,
    Set,
    Folder,
    User,
}

/// `(?!www\.)([\w-]+)\.pixnet.net` with `/album/(photo|set|folder)/(\d+)`
/// or a user's `(?:/blog|/album(?:/list)?)?/?(?:$|[?#])` path.
fn target(url: &Url) -> Option<(Kind, String, String)> {
    if !crate::sites::plain(url) {
        return None;
    }
    let blog = url.host_str()?.strip_suffix(".pixnet.net")?;
    if blog.is_empty() || blog == "www" || !blog.chars().all(|c| word_char(c) || c == '-') {
        return None;
    }
    let path = url.path();
    for (prefix, kind) in [
        ("/album/photo/", Kind::Image),
        ("/album/set/", Kind::Set),
        ("/album/folder/", Kind::Folder),
    ] {
        if let Some(rest) = path.strip_prefix(prefix) {
            let id = crate::sites::art::leading_digits(rest)?;
            return Some((kind, blog.into(), id.into()));
        }
    }
    matches!(
        path,
        "/" | "/blog" | "/blog/" | "/album" | "/album/" | "/album/list" | "/album/list/"
    )
    .then(|| (Kind::User, blog.into(), String::new()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some((kind, blog, id)) = target(&url) else {
        return crate::sites::fail(error("unsupported PIXNET URL"));
    };
    let root = format!("https://{blog}.pixnet.net");
    match kind {
        Kind::Image => crate::sites::once(async move {
            let mut url = http.api("https://api.pixnet.cc/oembed")?;
            url.query_pairs_mut()
                .append_pair("url", &format!("{root}/album/photo/{id}"))
                .append_pair("format", "json");
            let data = http.json(&url).await?;
            let item = image(data, &blog)?;
            Ok(crate::sites::events(crate::sites::post(
                &SITE,
                http,
                &IMAGE,
                vec![item],
            )))
        }),
        Kind::Set => crate::sites::once(async move {
            let page = http
                .text(&http.api(&format!("{root}/album/set/{id}"))?)
                .await?;
            let data = set_metadata(&page, &blog);
            Ok(listing(http, root, page, move |http, entries, num| {
                set_events(http, entries, num, &data)
            }))
        }),
        Kind::Folder | Kind::User => crate::sites::once(async move {
            let (route, url) = match kind {
                Kind::Folder => (&FOLDER, format!("{root}/album/folder/{id}")),
                _ => (&USER, format!("{root}/album/list")),
            };
            let page = http.text(&http.api(&url)?).await?;
            let author = extr(&page, "<meta name=\"author\" content=\"", "\";");
            let mut data = Map::new();
            data.insert("blog".into(), blog.as_str().into());
            data.insert("user".into(), rpartition(author, " (").0.into());
            let base = root.clone();
            Ok(listing(http, root, page, move |http, entries, _| {
                queue_entries(http, route, &base, entries, &data)
            }))
        }),
    }
}

/// `_pagination`: the `<li>` entries of each page, following `nextBtn`
/// links. `handle` turns one page's entries into events, given the number
/// of entries before them.
fn listing<'a, F>(http: &'a Http, root: String, page: String, handle: F) -> Events<'a>
where
    F: Fn(&'a Http, Vec<String>, usize) -> Events<'a> + Send + Sync + 'a,
{
    let handle = std::sync::Arc::new(handle);
    // The state holds the fetched first page, then the URL of the next one.
    let first: std::result::Result<String, String> = Ok(page);
    crate::sites::paginate(Some((first, 0usize, 0u32)), move |state| {
        let root = root.clone();
        let handle = handle.clone();
        async move {
            let Some((page, seen, pages)) = state else {
                return Ok(None);
            };
            let page = match page {
                Ok(page) => page,
                Err(url) => http.text(&http.api(&url)?).await?,
            };
            let entries: Vec<String> = extract_iter(&page, "<li id=\"", "</li>")
                .into_iter()
                .map(str::to_owned)
                .collect();
            let count = entries.len();
            let next = extr(&page, "class=\"nextBtn\"", ">");
            let next = (next.contains("href") && pages < 10_000).then(|| {
                let url = format!("{root}{}", extr(next, "href=\"", "\""));
                (Err(url), seen + count, pages + 1)
            });
            Ok(Some((handle(http, entries, seen), next)))
        }
    })
}

/// `PixnetExtractor.items`: queue each set or folder entry.
fn queue_entries<'a>(
    http: &'a Http,
    route: &'static Route,
    root: &str,
    entries: Vec<String>,
    data: &Map<String, Value>,
) -> Events<'a> {
    let mut children = Vec::new();
    for info in entries {
        let mut scan = Scan::new(&info);
        let raw = scan.extr(" href=\"", "\"");
        let alt = scan.extr(" alt=\"", "\"");
        let mut item = Map::new();
        item.insert("id".into(), parse_int(rpartition(raw, "/").2, 0).into());
        item.insert("title".into(), crate::sites::unescape(alt).into());
        update(&mut item, data);
        if let Some(url) = file_url(raw, root)
            .filter(|url| matches!(target(url), Some((Kind::Set | Kind::Folder, ..))))
        {
            children.push((url, item));
        }
    }
    futures_util::StreamExt::boxed(futures_util::StreamExt::flatten(
        futures_util::stream::iter(children.into_iter().map(move |(child, data)| {
            crate::sites::queue(http, "pixnet", route.subcategory, child, &data)
        })),
    ))
}

/// `PixnetSetExtractor.metadata`: author and breadcrumb folder and set.
fn set_metadata(page: &str, blog: &str) -> Map<String, Value> {
    let mut scan = Scan::new(page);
    let user = scan.extr("<meta name=\"author\" content=\"", "\";");
    scan.extr("id=\"breadcrumb\"", "");
    let fid = scan.extr("/folder/", "\"");
    let fname = scan.extr(">", "<");
    let sid = scan.extr("/set/", "\"");
    let sname = scan.extr(">", "<");
    let mut data = Map::new();
    data.insert("blog".into(), blog.into());
    data.insert("user".into(), rpartition(user, " (").0.into());
    // `text.parse_int(fid, "")`.
    let folder_id = match parse_int(fid, i64::MIN) {
        i64::MIN => Value::from(""),
        id => id.into(),
    };
    data.insert("folder_id".into(), folder_id);
    data.insert(
        "folder_title".into(),
        crate::sites::unescape(fname).trim().into(),
    );
    data.insert("set_id".into(), parse_int(sid, 0).into());
    data.insert("set_title".into(), crate::sites::unescape(sname).into());
    data
}

/// `PixnetSetExtractor.items` for one listing page: one post per page.
fn set_events<'a>(
    http: &'a Http,
    entries: Vec<String>,
    seen: usize,
    data: &Map<String, Value>,
) -> Events<'a> {
    let mut items = Vec::new();
    for (index, info) in entries.into_iter().enumerate() {
        let mut scan = Scan::new(&info);
        let link = scan.extr(" href=\"", "\"");
        let src = scan.extr(" src=\"", "\"");
        let alt = scan.extr(" alt=\"", "\"");
        let raw = src.replace("_s.", ".");
        let id = rpartition(link, "/")
            .2
            .split('#')
            .next()
            .unwrap_or_default();
        let mut photo = Map::new();
        photo.insert("id".into(), parse_int(id, 0).into());
        photo.insert("url".into(), raw.as_str().into());
        photo.insert("num".into(), (seen + index + 1).into());
        photo.insert("filename".into(), alt.into());
        photo.insert("extension".into(), rpartition(src, ".").2.into());
        update(&mut photo, data);
        if let Some(url) = crate::sites::media_url(&raw, MEDIA_HOSTS) {
            items.push(item(url, photo));
        }
    }
    crate::sites::events(crate::sites::post(&SITE, http, &SET, items))
}

/// `PixnetImageExtractor.items`: the oEmbed fields of one photo.
fn image(data: Value, blog: &str) -> Result<Item> {
    let Value::Object(mut data) = data else {
        return Err(error("PIXNET oEmbed response is not an object"));
    };
    let raw = super::str_of(&Value::Object(data.clone()), "url").to_owned();
    let tail = rpartition(&raw, "/").2;
    let id = tail.split('-').next().unwrap_or_default();
    data.insert("id".into(), parse_int(id, 0).into());
    let title = super::str_of(&Value::Object(data.clone()), "title").to_owned();
    let (name, _, ext) = rpartition(&title, ".");
    data.insert("filename".into(), name.into());
    data.insert("extension".into(), ext.into());
    data.insert("blog".into(), blog.into());
    let user = data.remove("author_name").unwrap_or(Value::Null);
    data.insert("user".into(), user);
    let Some(url) = crate::sites::media_url(&raw, MEDIA_HOSTS) else {
        return Err(error("PIXNET photo has no file URL"));
    };
    Ok(item(url, data))
}

fn item(url: Url, data: Map<String, Value>) -> Item {
    let id = data.get("id").map(crate::sites::scalar).unwrap_or_default();
    let set = data
        .get("set_id")
        .map(crate::sites::scalar)
        .unwrap_or_else(|| id.clone());
    let user = data.get("user").and_then(Value::as_str).map(str::to_owned);
    Item::new(url, data).ids(set, id).user(user)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, json, keywords, media, queued, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://albertayu773.pixnet.net/album/photo/159443828"),
            Some((Kind::Image, "albertayu773".into(), "159443828".into()))
        );
        assert_eq!(
            t("https://anrine910070.pixnet.net/album/set/5917493x"),
            Some((Kind::Set, "anrine910070".into(), "5917493".into()))
        );
        assert_eq!(
            t("https://sky92100.pixnet.net/album/folder/1234"),
            Some((Kind::Folder, "sky92100".into(), "1234".into()))
        );
        for raw in [
            "https://my_blog.pixnet.net/",
            "https://a-b.pixnet.net/blog",
            "https://a.pixnet.net/album/list/?x",
            "https://a.pixnet.net/album#x",
        ] {
            assert_eq!(t(raw).map(|t| t.0), Some(Kind::User), "{raw}");
        }
        for raw in [
            "https://www.pixnet.net/",
            "https://pixnet.net/",
            "https://a.b.pixnet.net/",
            "https://a.pixnet.net/blog/post/1",
            "https://a.pixnet.net/album/set/x",
            "https://a.pixnet.net.evil.test/",
            "https://a.pixnet.net:8080/",
            "https://u:p@a.pixnet.net/",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[tokio::test]
    async fn set_pages_through_listing() {
        let first = "<meta name=\"author\" content=\"Anne (anne)\";>\
            <div id=\"breadcrumb\"><a href=\"/album/folder/77\"> Trips &amp; more </a>\
            <a href=\"/album/set/55\">Set &amp; one</a></div>\
            <li id=\"a\"><a href=\"https://anne.pixnet.net/album/photo/101#x\"><img src=\"https://pic.pimg.tw/anne/1_s.jpg\" alt=\"one\"></a></li>\
            <a class=\"nextBtn\" href=\"/album/set/55?page=2\">";
        let second = "<li id=\"b\"><a href=\"https://anne.pixnet.net/album/photo/102\"><img src=\"https://pic.pimg.tw/anne/2_s.png\" alt=\"two\"></a></li>\
            <span class=\"nextBtn\">";
        let (root, server) = serve(vec![html(first), html(second)]);
        let http = http("pixnet", root);
        let files = media(extract(
            &http,
            Url::parse("https://anne.pixnet.net/album/set/55").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].url.as_str(), "https://pic.pimg.tw/anne/1.jpg");
        let k = keywords(&files[1]);
        assert_eq!(k["id"], 102);
        assert_eq!(k["num"], 2);
        assert_eq!(k["filename"], "two");
        assert_eq!(k["extension"], "png");
        assert_eq!(k["user"], "Anne");
        assert_eq!(k["folder_id"], 77);
        assert_eq!(k["folder_title"], "Trips & more");
        assert_eq!(k["set_title"], "Set & one");
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "002_102.png");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("pixnet/anne/77 Trips & more/55 Set & one")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{id}",["101"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /album/set/55 "));
        assert!(requests[1].starts_with("GET /album/set/55?page=2 "));
    }

    #[tokio::test]
    async fn user_queues_sets_and_folders() {
        let page = "<meta name=\"author\" content=\"Anne (anne)\";>\
            <li id=\"s\"><a href=\"https://anne.pixnet.net/album/set/55\"><img alt=\"Set &amp; one\"></a></li>\
            <li id=\"f\"><a href=\"/album/folder/77\"><img alt=\"Folder\"></a></li>\
            <li id=\"e\"><a href=\"https://evil.test/album/set/1\"><img alt=\"x\"></a></li>";
        let (root, server) = serve(vec![html(page)]);
        let http = http("pixnet", root);
        let children = queued(extract(
            &http,
            Url::parse("https://anne.pixnet.net/").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].subcategory(), "user");
        assert_eq!(children[0].fields()["id"], 55);
        assert_eq!(children[1].fields()["user"], "Anne");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /album/list "));
    }

    #[tokio::test]
    async fn image_from_oembed() {
        let (root, server) = serve(vec![json(json!({
            "url": "https://pic.pimg.tw/anne/159443828-123.jpg",
            "title": "beach.photo.JPG", "author_name": "Anne", "type": "photo"}))]);
        let http = http("pixnet", root);
        let files = media(extract(
            &http,
            Url::parse("https://anne.pixnet.net/album/photo/159443828").unwrap(),
        ))
        .await;
        let k = keywords(&files[0]);
        assert_eq!(k["id"], 159443828);
        assert_eq!(k["filename"], "beach.photo");
        assert_eq!(k["extension"], "JPG");
        assert_eq!(k["user"], "Anne");
        assert!(!k.contains_key("author_name"));
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "159443828.JPG");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("pixnet/anne")
        );
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with(
                "GET /oembed?url=https%3A%2F%2Fanne.pixnet.net%2Falbum%2Fphoto%2F159443828&format=json "
            ),
            "{}",
            requests[0]
        );
    }
}
