//! imagepond files, albums and users, mirroring
//! gallery_dl/extractor/imagepond.py: file pages carry Open Graph metadata,
//! and album and user listings queue file pages across `rel="next"` pages.
use super::Extr;
use crate::sites::art::extr;
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

pub static SITE: Site = Site {
    category: "imagepond",
    name: "imagepond",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&FILE, &ALBUM, &USER],
    api_key: false,
    cookies: None,
};

const DIRECTORY: &[&str] = &["{category}", "{user}", "{album}"];
static FILE: Route = Route::new("file", "{filename}.{extension}", DIRECTORY, "{id}");
static ALBUM: Route = Route::new("album", "{filename}.{extension}", DIRECTORY, "{id}");
static USER: Route = Route::new("user", "{filename}.{extension}", DIRECTORY, "{id}");

const ROOT: &str = "https://www.imagepond.net";
const HOSTS: &[&str] = &["imagepond.net"];

#[derive(Debug, PartialEq)]
enum Target {
    /// `/(?:i|img|image|video)/ID`.
    File(String),
    /// Everything after `/a/` or `/album/`.
    Album(String),
    /// Everything after `/` or `/user/`.
    User(String),
}

/// The upstream classes in order; album and user patterns end in `(.+)`
/// and so keep the query and fragment.
fn target(url: &Url) -> Option<Target> {
    if !super::on_host(url, HOSTS) {
        return None;
    }
    let path = url.path();
    for kind in ["/i/", "/img/", "/image/", "/video/"] {
        if let Some(id) = path
            .strip_prefix(kind)
            .and_then(|rest| rest.split('/').next())
            && !id.is_empty()
        {
            return Some(Target::File(format!("{kind}{id}")));
        }
    }
    let mut rest = path[1..].to_owned();
    if let Some(query) = url.query() {
        rest = format!("{rest}?{query}");
    }
    if let Some(fragment) = url.fragment() {
        rest = format!("{rest}#{fragment}");
    }
    for prefix in ["a/", "album/"] {
        if let Some(album) = rest.strip_prefix(prefix)
            && !album.is_empty()
        {
            return Some(Target::Album(album.to_owned()));
        }
    }
    match rest.strip_prefix("user/") {
        Some(user) if !user.is_empty() => Some(Target::User(user.to_owned())),
        _ => (!rest.is_empty()).then_some(Target::User(rest)),
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::File(path)) => crate::sites::once(async move {
            let (location, page) = http.page(&http.api(&format!("{ROOT}{path}"))?, &[]).await?;
            let item = parse_file(&page, location.as_str())?;
            Ok(crate::sites::events(crate::sites::post(
                &SITE,
                http,
                &FILE,
                vec![item],
            )))
        }),
        Some(Target::Album(album)) => listing(http, &ALBUM, format!("{ROOT}/a/{album}")),
        Some(Target::User(user)) => listing(http, &USER, format!("{ROOT}/user/{user}")),
        None => crate::sites::fail(error("unsupported imagepond URL")),
    }
}

/// `ImagepondFileExtractor.items`; `location` is the page's final URL,
/// whose last component is the file ID.
fn parse_file(page: &str, location: &str) -> Result<Item> {
    let mut scan = Extr::new(page);
    let mut og = |name: &str| {
        scan.next(&format!("property=\"og:{name}\" content=\""), "\"")
            .to_owned()
    };
    let title = crate::sites::unescape(&og("title"));
    let kind = og("type");
    let mut file = Map::new();
    if kind == "image" {
        file.insert("type".into(), "image".into());
        file.insert("url".into(), og("image").into());
        file.insert("width".into(), super::parse_int(&og("image:width")).into());
        file.insert(
            "height".into(),
            super::parse_int(&og("image:height")).into(),
        );
    } else {
        file.insert("type".into(), "video".into());
        file.insert("url".into(), og("video").into());
        file.insert("mime".into(), og("video:type").into());
        file.insert("width".into(), super::parse_int(&og("video:width")).into());
        file.insert(
            "height".into(),
            super::parse_int(&og("video:height")).into(),
        );
        file.insert("thumbnail".into(), og("image").into());
        scan.next("<span class=\"uppercase", "</span>");
        let (m, _, s) = super::partition(scan.next("<span>", "</span>"), ":");
        let duration = super::parse_int(m) * 60 + super::parse_int(s);
        file.insert("duration".into(), duration.into());
    }
    let id = super::rpartition(location, "/").2.to_owned();
    file.insert("id".into(), id.clone().into());
    file.insert("title".into(), title.clone().into());
    let date = super::strptime(
        scan.next("<span class=\"hidden sm:inline\">", "<"),
        "%b %d, %Y",
    );
    file.insert("date".into(), super::date_value(date));
    let user = scan.next("/user/", "\"").to_owned();
    file.insert("user".into(), user.clone().into());
    let album_id = scan.next("/a/", "\"");
    if album_id.is_empty() {
        file.insert("album".into(), "".into());
        file.insert("album_id".into(), "".into());
    } else {
        let album = super::partition(scan.next("<span ", "<"), ">").2;
        file.insert("album".into(), crate::sites::unescape(album).into());
        file.insert("album_id".into(), album_id.into());
    }
    let url = file
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    super::nameext_from_url(&url, &mut file);
    if !file.contains_key("mime") {
        let mime = format!(
            "{}/{}",
            crate::sites::scalar(&file["type"]),
            crate::sites::scalar(&file["extension"])
        );
        file.insert("mime".into(), mime.into());
    }
    let url = super::web_url(&url).ok_or_else(|| error("imagepond file has no URL"))?;
    Ok(Item::new(url, file)
        .ids(id.clone(), id)
        .user(Some(user))
        .caption(Some(title))
        .timestamp(date))
}

/// `_pagination`: queue file pages of each listing page, following the
/// anchor marked `rel="next"`.
fn listing<'a>(http: &'a Http, route: &'static Route, start: String) -> Events<'a> {
    let state = (Some(start), None::<Map<String, Value>>, 0usize, 0u32);
    crate::sites::paginate(state, move |(next, album, mut num, count)| async move {
        let Some(url) = next.filter(|_| count < 10_000) else {
            return Ok(None);
        };
        let page = http.text(&http.api(&url)?).await?;
        let album = match album {
            Some(album) => album,
            None => album_metadata(route, &page),
        };
        let links: Vec<String> = if std::ptr::eq(route, &USER) {
            user_files(&page)
                .into_iter()
                .map(|path| format!("{ROOT}/i/{path}"))
                .collect()
        } else {
            super::extract_iter(&page, "'javascript:void(0)' : '", "'")
                .into_iter()
                .map(str::to_owned)
                .collect()
        };
        let mut children = Vec::new();
        for link in links {
            let Some(url) = Url::parse(&link)
                .ok()
                .filter(|url| matches!(target(url), Some(Target::File(_))))
            else {
                continue;
            };
            let mut data = album.clone();
            if std::ptr::eq(route, &ALBUM) {
                // `self.kwdict["num"]` counts every queued item.
                num += 1;
                data.insert("num".into(), num.into());
            }
            children.push((url, data));
        }
        let events = super::queue_all(http, SITE.category, route.subcategory, children);
        Ok(Some((
            events,
            (next_page(&page), Some(album), num, count + 1),
        )))
    })
}

/// `_extract_metadata_album` for albums; nothing for users.
fn album_metadata(route: &Route, page: &str) -> Map<String, Value> {
    let mut data = Map::new();
    if !std::ptr::eq(route, &ALBUM) {
        return data;
    }
    let (album, pos) = match super::extract(page, "<h1", "<", 0) {
        Some((title, pos)) => {
            let title = title.find('>').map_or(title, |i| &title[i + 1..]);
            (crate::sites::unescape(title), pos)
        }
        None => (String::new(), 0),
    };
    data.insert("album".into(), album.into());
    let count = super::extract(page, "<span>", " ", pos).map_or("", |(count, _)| count);
    data.insert("count".into(), super::parse_int(count).into());
    data
}

/// `/i/([^/?#"]+)"` matches of a user page.
fn user_files(page: &str) -> Vec<String> {
    page.match_indices("/i/")
        .filter_map(|(i, _)| {
            let rest = &page[i + 3..];
            let len = rest.find(['/', '?', '#', '"']).unwrap_or(rest.len());
            (len > 0 && rest[len..].starts_with('"')).then(|| rest[..len].to_owned())
        })
        .collect()
}

/// The `href` of the tag holding ` rel="next"`, kept only on imagepond.
fn next_page(page: &str) -> Option<String> {
    let pos = page.find(" rel=\"next\"")?;
    let start = page[..pos].rfind('<').unwrap_or(0);
    let end = page[pos..].find('>').map_or(page.len(), |i| pos + i);
    let href = extr(&page[start..end], "href=\"", "\"");
    if href.is_empty() {
        return None;
    }
    let url = Url::parse(ROOT)
        .ok()?
        .join(&crate::sites::unescape(href))
        .ok()?;
    super::on_host(&url, HOSTS).then(|| url.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, keywords, media, queued, serve};

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.imagepond.net/i/AbC1/x"),
            Some(Target::File("/i/AbC1".into()))
        );
        assert_eq!(
            t("https://imagepond.net/video/v1"),
            Some(Target::File("/video/v1".into()))
        );
        assert_eq!(
            t("https://www.imagepond.net/a/Alb?page=2"),
            Some(Target::Album("Alb?page=2".into()))
        );
        assert_eq!(
            t("https://www.imagepond.net/album/Alb"),
            Some(Target::Album("Alb".into()))
        );
        assert_eq!(
            t("https://www.imagepond.net/user/bob"),
            Some(Target::User("bob".into()))
        );
        assert_eq!(
            t("https://www.imagepond.net/bob/x"),
            Some(Target::User("bob/x".into()))
        );
        for raw in [
            "https://www.imagepond.net/",
            "https://cdn.imagepond.net/i/x",
            "https://imagepond.net.evil.test/i/x",
            "https://www.imagepond.net:8080/i/x",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[tokio::test]
    async fn extracts_image_and_video_pages() {
        let image = "<meta property=\"og:title\" content=\"Sun &amp; Sea\">\
            <meta property=\"og:type\" content=\"image\">\
            <meta property=\"og:image\" content=\"https://cdn.imagepond.net/i/AbC1.JPG\">\
            <meta property=\"og:image:width\" content=\"800\">\
            <meta property=\"og:image:height\" content=\"600\">\
            <span class=\"hidden sm:inline\">Jun 05, 2024</span>\
            <a href=\"/user/bob\">bob</a><a href=\"/a/Al1\"><span class=\"x\">Trip &amp; Co</span></a>";
        let video = "<meta property=\"og:title\" content=\"Clip\">\
            <meta property=\"og:type\" content=\"video.other\">\
            <meta property=\"og:video\" content=\"https://cdn.imagepond.net/v/clip.mp4\">\
            <meta property=\"og:video:type\" content=\"video/mp4\">\
            <meta property=\"og:video:width\" content=\"1280\">\
            <meta property=\"og:video:height\" content=\"720\">\
            <meta property=\"og:image\" content=\"https://cdn.imagepond.net/v/clip.jpg\">\
            <span class=\"uppercase\">mp4</span><span>2:05</span>\
            <a href=\"/user/eve\">eve</a>";
        let (root, server) = serve(vec![html(image), html(video)]);
        let http = http("imagepond", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.imagepond.net/i/AbC1").unwrap(),
        ))
        .await;
        let k = keywords(&files[0]);
        assert_eq!(k["id"], "AbC1");
        assert_eq!(k["title"], "Sun & Sea");
        assert_eq!(k["width"], 800);
        assert_eq!(k["date"], "2024-06-05 00:00:00");
        assert_eq!(k["user"], "bob");
        assert_eq!(k["album"], "Trip & Co");
        assert_eq!(k["album_id"], "Al1");
        assert_eq!(k["mime"], "image/jpg");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "AbC1.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("imagepond/bob/Trip & Co")
        );
        assert_eq!(
            crate::sites::archive_key(&files[0]).unwrap().1,
            r#"["{id}",["AbC1"]]"#
        );
        let files = media(extract(
            &http,
            Url::parse("https://www.imagepond.net/video/v1").unwrap(),
        ))
        .await;
        let k = keywords(&files[0]);
        assert_eq!(k["type"], "video");
        assert_eq!(k["mime"], "video/mp4");
        assert_eq!(k["duration"], 125);
        assert_eq!(k["album"], "");
        assert_eq!(k["thumbnail"], "https://cdn.imagepond.net/v/clip.jpg");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /i/AbC1 "));
        assert!(requests[1].starts_with("GET /video/v1 "));
    }

    #[tokio::test]
    async fn albums_and_users_follow_next_pages() {
        let first = "<h1 class=\"t\">Trip &amp; Co</h1><span>3 files</span>\
            <a href=\"x\" onclick=\"a ? 'javascript:void(0)' : 'https://www.imagepond.net/i/one'\">\
            <a class=\"n\" href=\"/a/Al1?page=2&amp;x=1\" rel=\"next\">";
        let second = "<a onclick=\"a ? 'javascript:void(0)' : 'https://evil.test/i/two'\">\
            <a onclick=\"a ? 'javascript:void(0)' : 'https://imagepond.net/i/three'\">";
        let user = "<a href=\"/i/u1\"><a href=\"/i/u2/\">";
        let (root, server) = serve(vec![html(first), html(second), html(user)]);
        let http = http("imagepond", root);
        let children = queued(extract(
            &http,
            Url::parse("https://www.imagepond.net/a/Al1").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 2);
        assert_eq!(children[1].subcategory(), "album");
        let children = queued(extract(
            &http,
            Url::parse("https://www.imagepond.net/user/bob").unwrap(),
        ))
        .await;
        assert_eq!(children.len(), 1);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /a/Al1 "));
        assert!(requests[1].starts_with("GET /a/Al1?page=2&x=1 "));
        assert!(requests[2].starts_with("GET /user/bob "));
    }
}
