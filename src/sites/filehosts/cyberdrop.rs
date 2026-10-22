//! Cyberdrop albums and files, mirroring gallery_dl/extractor/cyberdrop.py
//! (a lolisafe derivative) through the `api.cyberdrop.cr` file API.
use super::Extr;
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

pub static SITE: Site = Site {
    category: "cyberdrop",
    name: "Cyberdrop",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&ALBUM, &MEDIA],
    api_key: false,
};

static ALBUM: Route = Route::new(
    "album",
    "{filename}.{extension}",
    &["{category}", "{album_name} ({album_id})"],
    "{album_id}_{id}",
);
static MEDIA: Route = Route::new(
    "media",
    "{filename}.{extension}",
    &["{category}"],
    "{album_id}_{id}",
);

const ROOT: &str = "https://cyberdrop.cr";
const ROOT_API: &str = "https://api.cyberdrop.cr";
const HOSTS: &[&str] = &["cyberdrop.cr", "cyberdrop.me", "cyberdrop.to"];

#[derive(Debug, PartialEq)]
enum Target {
    Album(String),
    Media(String),
}

fn target(url: &Url) -> Option<Target> {
    if !super::on_host(url, HOSTS) {
        return None;
    }
    match crate::sites::segments(url).as_slice() {
        [a, id, ..] if a == "a" => Some(Target::Album(id.clone())),
        [kind, id, ..] if kind == "e" || kind == "f" => Some(Target::Media(id.clone())),
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Album(id)) => crate::sites::once(async move {
            let page = http.text(&http.api(&format!("{ROOT}/a/{id}"))?).await?;
            let (ids, data) = parse_album(&page, &id);
            Ok(files(http, &ALBUM, ids, data))
        }),
        Some(Target::Media(id)) => {
            let mut data = Map::new();
            data.insert("album_id".into(), "".into());
            data.insert("album_name".into(), "".into());
            data.insert("album_size".into(), (-1).into());
            data.insert("description".into(), "".into());
            data.insert("count".into(), 1.into());
            files(http, &MEDIA, vec![id], data)
        }
        None => crate::sites::fail(error("unsupported cyberdrop URL")),
    }
}

fn parse_album(page: &str, album_id: &str) -> (Vec<String>, Map<String, Value>) {
    let mut extr = Extr::new(page);
    let mut desc = extr.next("property=\"og:description\" content=\"", "\"");
    if desc.starts_with(
        "A privacy-focused censorship-resistant file sharing platform free for everyone.",
    ) {
        desc = "";
    }
    extr.next("id=\"title\"", "");
    let mut album = Map::new();
    album.insert("album_id".into(), album_id.into());
    album.insert(
        "album_name".into(),
        crate::sites::unescape(extr.next("title=\"", "\"")).into(),
    );
    album.insert(
        "album_size".into(),
        super::parse_bytes(extr.next("<p class=\"title\">", "B")).into(),
    );
    album.insert(
        "date".into(),
        super::date_value(super::strptime(
            extr.next("<p class=\"title\">", "<"),
            "%d.%m.%Y",
        )),
    );
    album.insert(
        "description".into(),
        crate::sites::unescape(&crate::sites::unescape(super::rpartition(desc, " [R").0)).into(),
    );
    let ids: Vec<String> = super::extract_iter(page, "id=\"file\" href=\"/f/", "\"")
        .into_iter()
        .map(str::to_owned)
        .collect();
    album.insert("count".into(), ids.len().into());
    (ids, album)
}

fn files<'a>(
    http: &'a Http,
    route: &'static Route,
    ids: Vec<String>,
    album: Map<String, Value>,
) -> Events<'a> {
    let inputs: Vec<(usize, String)> = ids.into_iter().enumerate().collect();
    super::each(&SITE, http, route, inputs, move |(index, id)| {
        let album = album.clone();
        async move { file(http, &id, &album, index + 1).await.map(Some) }
    })
}

async fn file(http: &Http, id: &str, album: &Map<String, Value>, num: usize) -> Result<Item> {
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        return Err(error("invalid cyberdrop file ID"));
    }
    let info = http
        .json(&http.api(&format!("{ROOT_API}/api/file/info/{id}"))?)
        .await?;
    let auth = info
        .get("auth_url")
        .and_then(Value::as_str)
        .and_then(|raw| crate::sites::media_url(raw, HOSTS))
        .ok_or_else(|| error("cyberdrop file info has no valid auth URL"))?;
    let auth = http.json(&http.api(auth.as_str())?).await?;
    let url = auth
        .get("url")
        .and_then(Value::as_str)
        .and_then(super::web_url)
        .ok_or_else(|| error("cyberdrop returned an invalid file URL"))?;
    Ok(file_item(info, url, album, num))
}

fn file_item(info: Value, url: Url, album: &Map<String, Value>, num: usize) -> Item {
    let mut file = match info {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    file.insert("url".into(), url.as_str().into());
    for (key, value) in album {
        file.insert(key.clone(), value.clone());
    }
    file.insert("num".into(), num.into());
    let name = file
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    super::nameext_from_url(&name, &mut file);
    let stem = file
        .get("filename")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let (base, _, id) = super::rpartition(&stem, "-");
    file.insert("name".into(), base.into());
    file.insert("id".into(), id.into());
    let album_id = album
        .get("album_id")
        .map(crate::sites::scalar)
        .unwrap_or_default();
    Item::new(url, file)
        .ids(album_id, id)
        .caption(Some(base.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{html, http, json, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(t("https://cyberdrop.me/a/8uE0wQiK"), Some(Target::Album("8uE0wQiK".into())));
        assert_eq!(t("https://www.cyberdrop.cr/f/abc"), Some(Target::Media("abc".into())));
        assert_eq!(t("https://cyberdrop.to/e/abc"), Some(Target::Media("abc".into())));
        for raw in [
            "https://cyberdrop.com/a/x",
            "https://cyberdrop.me.evil.test/a/x",
            "https://cyberdrop.me:81/a/x",
            "https://cyberdrop.me/x/y",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    const PAGE: &str = "<meta property=\"og:description\" content=\"Holiday &amp;amp; fun [R1]\">\
<h1 id=\"title\" class=\"title\" title=\"Trip &amp; Co\">Trip</h1>\
<p class=\"title\">1.5 MB</p><p class=\"title\">05.06.2024</p>\
<a id=\"file\" href=\"/f/AAA\"></a><a id=\"file\" href=\"/f/BBB\"></a>";

    #[test]
    fn parses_album_page() {
        let (ids, data) = parse_album(PAGE, "X1");
        assert_eq!(ids, vec!["AAA", "BBB"]);
        assert_eq!(data["album_name"], "Trip & Co");
        assert_eq!(data["album_size"], 1_572_864);
        assert_eq!(data["date"], "2024-06-05 00:00:00");
        assert_eq!(data["description"], "Holiday & fun");
        assert_eq!(data["count"], 2);
    }

    #[tokio::test]
    async fn extracts_album_files() {
        let (root, server) = serve(vec![
            html(PAGE),
            json(json!({"name": "beach-AbC123.jpg", "size": 5, "type": "image/jpeg",
                "auth_url": "https://api.cyberdrop.cr/api/file/auth/AAA"})),
            json(json!({"url": "https://sun.cyberdrop.cr/api/file/d/AAA?token=x"})),
            json(json!({"name": "other.png",
                "auth_url": "https://api.cyberdrop.cr/api/file/auth/BBB"})),
            json(json!({"url": "https://sun.cyberdrop.cr/api/file/d/BBB"})),
        ]);
        let http = http("Cyberdrop", root);
        let files = media(extract(&http, Url::parse("https://cyberdrop.me/a/X1").unwrap())).await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[0]);
        assert_eq!(k["filename"], "beach-AbC123");
        assert_eq!(k["extension"], "jpg");
        assert_eq!(k["name"], "beach");
        assert_eq!(k["id"], "AbC123");
        assert_eq!(k["album_name"], "Trip & Co");
        assert_eq!(k["num"], 1);
        assert_eq!(keywords(&files[1])["name"], "");
        assert_eq!(keywords(&files[1])["id"], "other");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "beach-AbC123.jpg");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("cyberdrop/Trip & Co (X1)")
        );
        assert_eq!(
            crate::sites::archive_key(&files[0]).unwrap().1,
            r#"["{album_id}_{id}",["X1","AbC123"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /a/X1 "));
        assert!(requests[1].starts_with("GET /api/file/info/AAA "));
        assert!(requests[2].starts_with("GET /api/file/auth/AAA "));
    }

    #[tokio::test]
    async fn media_route_uses_empty_album() {
        let (root, _server) = serve(vec![
            json(json!({"name": "a-b.gif", "auth_url": "https://api.cyberdrop.cr/api/file/auth/Q"})),
            json(json!({"url": "https://x.cyberdrop.cr/d/Q"})),
        ]);
        let http = http("Cyberdrop", root);
        let files = media(extract(&http, Url::parse("https://cyberdrop.cr/f/Q").unwrap())).await;
        let k = keywords(&files[0]);
        assert_eq!(k["subcategory"], "media");
        assert_eq!(k["album_size"], -1);
        let (_, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(dir.render(&files[0]).unwrap().unwrap(), std::path::PathBuf::from("cyberdrop"));
    }
}
