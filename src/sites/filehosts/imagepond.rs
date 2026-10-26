//! ImagePond files, albums and users, mirroring
//! gallery_dl/extractor/imagepond.py.
use crate::error;
use crate::sites::gdl::{
    Scan, captures, date, extr, extract_iter, group, nameext_into, parse_int, partition, re,
    strptime, unescape,
};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://www.imagepond.net";
const DIRECTORY: &[&str] = &["{category}", "{user}", "{album}"];

static FILE: Route = Route::new("file", "{filename}.{extension}", DIRECTORY, "{id}");
static ALBUM: Route = Route::new("album", "{filename}.{extension}", DIRECTORY, "{id}");
static USER: Route = Route::new("user", "{filename}.{extension}", DIRECTORY, "{id}");

pub static SITE: Site = Site {
    category: "imagepond",
    name: "ImagePond",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&FILE, &ALBUM, &USER],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    File(String),
    Album(String),
    User(String),
}

fn target(url: &Url) -> Option<Target> {
    const BASE: &str = r"^https?://(?:www\.)?imagepond\.net";
    if let Some(c) = captures(
        re!(&format!(r"{BASE}(/(?:i(?:mg|mage)?|video)/[^/?#]+)")),
        url,
    ) {
        return Some(Target::File(group(&c, 1).into()));
    }
    if let Some(c) = captures(re!(&format!(r"{BASE}/a(?:lbum)?/(.+)")), url) {
        return Some(Target::Album(group(&c, 1).into()));
    }
    captures(re!(&format!(r"{BASE}/(?:user/)?(.+)")), url)
        .map(|c| Target::User(group(&c, 1).into()))
}

/// `_pagination`: items on each page, following `rel="next"` links.
async fn paginate(
    http: &Http,
    url: &str,
    pattern: Option<&regex::Regex>,
) -> crate::Result<(String, Vec<String>)> {
    let first = http.text(&http.api(url)?).await?;
    let mut page = first.clone();
    let mut items = Vec::new();
    loop {
        match pattern {
            None => items.extend(
                extract_iter(&page, "'javascript:void(0)' : '", "'")
                    .into_iter()
                    .map(str::to_owned),
            ),
            Some(pattern) => items.extend(
                pattern
                    .captures_iter(&page)
                    .map(|c| group(&c, 1).to_owned()),
            ),
        }
        let Some(pos) = page.find(" rel=\"next\"") else {
            break;
        };
        let start = page[..pos].rfind('<').unwrap_or(0);
        let end = page[pos..].find('>').map_or(page.len(), |i| pos + i);
        let next = extr(&page[start..end], "href=\"", "\"");
        if next.is_empty() {
            break;
        }
        page = http.text(&http.api(&unescape(next))?).await?;
    }
    Ok((first, items))
}

async fn file(http: &Http, path: &str) -> crate::Result<Item> {
    let (page, final_url) = http
        .text_final(&http.api(&format!("{ROOT}{path}"))?, &[])
        .await?;
    let mut scan = Scan::new(&page);
    let title = unescape(scan.extr("property=\"og:title\" content=\"", "\""));
    let kind = scan.extr("property=\"og:type\" content=\"", "\"");
    let mut data = Map::new();
    if kind == "image" {
        data.insert("type".into(), "image".into());
        data.insert(
            "url".into(),
            scan.extr("property=\"og:image\" content=\"", "\"").into(),
        );
        data.insert(
            "width".into(),
            parse_int(scan.extr("property=\"og:image:width\" content=\"", "\"")).into(),
        );
        data.insert(
            "height".into(),
            parse_int(scan.extr("property=\"og:image:height\" content=\"", "\"")).into(),
        );
    } else {
        data.insert("type".into(), "video".into());
        data.insert(
            "url".into(),
            scan.extr("property=\"og:video\" content=\"", "\"").into(),
        );
        data.insert(
            "mime".into(),
            scan.extr("property=\"og:video:type\" content=\"", "\"")
                .into(),
        );
        data.insert(
            "width".into(),
            parse_int(scan.extr("property=\"og:video:width\" content=\"", "\"")).into(),
        );
        data.insert(
            "height".into(),
            parse_int(scan.extr("property=\"og:video:height\" content=\"", "\"")).into(),
        );
        data.insert(
            "thumbnail".into(),
            scan.extr("property=\"og:image\" content=\"", "\"").into(),
        );
        scan.extr("<span class=\"uppercase", "</span>");
        let (m, _, s) = partition(scan.extr("<span>", "</span>"), ":");
        data.insert("duration".into(), (parse_int(m) * 60 + parse_int(s)).into());
    }
    data.insert(
        "id".into(),
        final_url
            .path()
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .into(),
    );
    data.insert("title".into(), title.into());
    data.insert(
        "date".into(),
        date(strptime(
            scan.extr("<span class=\"hidden sm:inline\">", "<"),
            "%b %d, %Y",
        )),
    );
    data.insert("user".into(), scan.extr("/user/", "\"").into());
    match scan.find("/a/", "\"").filter(|a| !a.is_empty()) {
        Some(album_id) => {
            let album_id = album_id.to_owned();
            data.insert(
                "album".into(),
                unescape(partition(scan.extr("<span ", "<"), ">").2).into(),
            );
            data.insert("album_id".into(), album_id.into());
        }
        None => {
            data.insert("album".into(), "".into());
            data.insert("album_id".into(), "".into());
        }
    }
    let raw = crate::sites::scalar(&data["url"]);
    nameext_into(&raw, &mut data);
    if !data.contains_key("mime") {
        let mime = format!(
            "{}/{}",
            crate::sites::scalar(&data["type"]),
            crate::sites::scalar(&data["extension"])
        );
        data.insert("mime".into(), mime.into());
    }
    let id = crate::sites::scalar(&data["id"]);
    let url = Url::parse(&raw).map_err(|_| error("ImagePond file has no URL"))?;
    Ok(Item::new(url, data).ids(id.clone(), id))
}

fn queue<'a>(
    http: &'a Http,
    route: &'static Route,
    urls: Vec<(Url, Map<String, Value>)>,
) -> Events<'a> {
    use futures_util::StreamExt;
    futures_util::stream::iter(urls.into_iter().map(move |(url, data)| {
        crate::sites::queue(http, SITE.category, route.subcategory, url, &data)
    }))
    .flatten()
    .boxed()
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported ImagePond URL"));
    };
    crate::sites::once(async move {
        Ok(match target {
            Target::File(path) => crate::sites::events(crate::sites::post(
                &SITE,
                http,
                &FILE,
                vec![file(http, &path).await?],
            )),
            Target::Album(id) => {
                let (page, items) = paginate(http, &format!("{ROOT}/a/{id}"), None).await?;
                let mut scan = Scan::new(&page);
                let album = scan.find("<h1", "<").map_or(String::new(), |t| {
                    unescape(&t[t.find('>').map_or(0, |i| i + 1)..])
                });
                let count = parse_int(scan.extr("<span>", " "));
                let urls = items
                    .into_iter()
                    .enumerate()
                    .filter_map(|(index, raw)| {
                        let mut data = Map::new();
                        data.insert("album".into(), album.as_str().into());
                        data.insert("count".into(), count.into());
                        data.insert("num".into(), (index + 1).into());
                        Some((Url::parse(&raw).ok()?, data))
                    })
                    .collect();
                queue(http, &ALBUM, urls)
            }
            Target::User(user) => {
                let pattern = re!(r#"/i/([^/?#"]+)""#);
                let (_, paths) =
                    paginate(http, &format!("{ROOT}/user/{user}"), Some(pattern)).await?;
                let urls = paths
                    .into_iter()
                    .filter_map(|p| Url::parse(&format!("{ROOT}/i/{p}")).ok())
                    .map(|u| (u, Map::new()))
                    .collect();
                queue(http, &USER, urls)
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
            t("https://www.imagepond.net/i/chv_317512"),
            Some(Target::File("/i/chv_317512".into()))
        );
        assert_eq!(
            t("https://imagepond.net/video/1000423939.zb8Fxy"),
            Some(Target::File("/video/1000423939.zb8Fxy".into()))
        );
        assert_eq!(
            t("https://imagepond.net/album/CDilP/?sort=date_desc&page=1"),
            Some(Target::Album("CDilP/?sort=date_desc&page=1".into()))
        );
        assert_eq!(
            t("https://imagepond.net/dariusbbb24"),
            Some(Target::User("dariusbbb24".into()))
        );
    }

    #[tokio::test]
    async fn extracts_video_file() {
        let page = r#"<meta property="og:title" content="Clip &amp; More"><meta property="og:type" content="video">
            <meta property="og:video" content="https://cdn.imagepond.net/v/clip.mp4"><meta property="og:video:type" content="video/mp4">
            <meta property="og:video:width" content="1280"><meta property="og:video:height" content="720"><meta property="og:image" content="https://cdn.imagepond.net/t.jpg">
            <span class="uppercase">mp4</span><span>2:05</span><span class="hidden sm:inline">Feb 17, 2025<a href="/user/darius"><a href="/a/AbC"><span class="x">Holiday</span>"#;
        let (root, server) = serve(vec![html(page)]);
        let http = client("imagepond", root);
        let files = media(extract(
            &http,
            Url::parse("https://imagepond.net/video/1000423939.zb8Fxy").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let k = keywords(&files[0]);
        assert_eq!(k["duration"], 125);
        assert_eq!(k["user"], "darius");
        assert_eq!(k["album"], "Holiday");
        assert_eq!(k["album_id"], "AbC");
        assert_eq!(k["id"], "1000423939.zb8Fxy");
        assert_eq!(k["date"], "2025-02-17 00:00:00");
        let (_, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("imagepond/darius/Holiday")
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn album_and_user_pages() {
        let first = r#"<h1 class="t">Holiday</h1><span>2 files</span><a href='javascript:void(0)' : 'https://www.imagepond.net/i/a1'>
            <a data-pagination="next" href="https://www.imagepond.net/a/AbC/?page=2" rel="next">"#;
        let second = r#"<a href='javascript:void(0)' : 'https://www.imagepond.net/i/a2'>"#;
        let (root, server) = serve(vec![html(first), html(second)]);
        let http = client("imagepond", root);
        assert_eq!(
            children(extract(
                &http,
                Url::parse("https://www.imagepond.net/a/AbC").unwrap()
            ))
            .await
            .len(),
            2
        );
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("GET /a/AbC/?page=2 "));
        let (root, server) = serve(vec![html(r#"<a href="/i/x1"></a><a href="/i/x2"></a>"#)]);
        let http = client("imagepond", root);
        assert_eq!(
            children(extract(
                &http,
                Url::parse("https://www.imagepond.net/user/darius").unwrap()
            ))
            .await
            .len(),
            2
        );
        server.join().unwrap();
    }
}
