//! Koofr shared links, mirroring gallery_dl/extractor/koofr.py: the public
//! link API describes the shared file or folder, and folders are walked
//! through its `bundle` listing, one directory per post.
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

pub static SITE: Site = Site {
    category: "koofr",
    name: "Koofr",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&SHARED],
    api_key: false,
    cookies: None,
};

static SHARED: Route = Route::new(
    "shared",
    "{num:>03} {filename} ({hash|id:[:8]}).{extension}",
    &["{category}", "{post[title]} ({post[id]})", "{path:I}"],
    "{post[id]}_{path:J/}_{hash|id}",
);

const ROOT: &str = "https://app.koofr.net";

#[derive(Debug, PartialEq)]
enum Target {
    /// `koofr.net/links/UUID`.
    Link(String),
    /// A `k00.fr/CODE` short link.
    Short(String),
}

/// The leading run of `text` matching `accept`.
fn run(text: &str, accept: fn(u8) -> bool) -> &str {
    &text[..text.bytes().position(|b| !accept(b)).unwrap_or(text.len())]
}

fn word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// `(?:app\.)?koofr\.(?:net|eu)/links/([\w-]+)` or `k00\.fr/(\w+)`.
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) {
        return None;
    }
    let host = url.host_str()?;
    let path = url.path();
    if host == "k00.fr" {
        let code = run(&path[1..], word);
        return (!code.is_empty()).then(|| Target::Short(code.to_owned()));
    }
    let domain = host.strip_prefix("app.").unwrap_or(host);
    if !matches!(domain, "koofr.net" | "koofr.eu") {
        return None;
    }
    let uuid = run(path.strip_prefix("/links/")?, |b| word(b) || b == b'-');
    (!uuid.is_empty()).then(|| Target::Link(uuid.to_owned()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Koofr URL"));
    };
    crate::sites::once(async move {
        let uuid = match target {
            Target::Link(uuid) => uuid,
            Target::Short(code) => {
                let short = http.api(&format!("https://k00.fr/{code}"))?;
                let location = http.location(&short, &[]).await?;
                let location = location.as_ref().map_or("", Url::as_str);
                super::rpartition(location, "/").2.to_owned()
            }
        };
        shared(http, uuid).await
    })
}

/// Python `html.escape` (upstream `text.escape`).
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

/// The JSON API headers upstream sends.
fn headers(referer: &str) -> [(&str, &str); 6] {
    [
        ("Referer", referer),
        ("X-Client", "newfrontend"),
        ("X-Koofr-Version", "2.1"),
        ("Sec-Fetch-Dest", "empty"),
        ("Sec-Fetch-Mode", "cors"),
        ("Sec-Fetch-Site", "same-origin"),
    ]
}

/// Shared state for every file of one link.
struct Link {
    uuid: String,
    referer: String,
    api: String,
    password: String,
    base: String,
    post: Value,
    recursive: bool,
}

async fn shared(http: &Http, uuid: String) -> Result<Events<'_>> {
    let api = format!("{ROOT}/api/v2/public/links/{uuid}");
    let referer = format!("{ROOT}/links/{uuid}");
    let password = http.option_str("password").unwrap_or_default().to_owned();
    let mut url = http.api(&api)?;
    url.query_pairs_mut().append_pair("password", &password);
    let data = http.json_with(&url, &headers(&referer)).await?;
    let mut file = data
        .get("file")
        .and_then(Value::as_object)
        .cloned()
        .ok_or_else(|| error("Koofr link has no file"))?;
    file.insert("path".into(), Value::Array(Vec::new()));
    let recursive = file.get("type").and_then(Value::as_str) == Some("dir")
        && http.option_bool("recursive", true);
    let modified = timestamp(&file);
    let mut post = Map::new();
    post.insert("id".into(), data.get("id").cloned().unwrap_or(Value::Null));
    post.insert(
        "title".into(),
        data.get("name").cloned().unwrap_or(Value::Null),
    );
    post.insert("date".into(), super::date_value(modified));
    let root = data
        .get("publicUrlBase")
        .and_then(Value::as_str)
        .filter(|base| !base.is_empty())
        .unwrap_or(ROOT);
    let link = std::sync::Arc::new(Link {
        base: format!("{root}/content/links/{uuid}/files/get/"),
        password: if password.is_empty() {
            String::new()
        } else {
            format!("&password={}", escape(&password))
        },
        uuid,
        referer,
        api,
        post: Value::Object(post),
        recursive,
    });
    if !recursive {
        let items = files(&link, vec![file])?;
        return Ok(crate::sites::events(crate::sites::post(
            &SITE, http, &SHARED, items,
        )));
    }
    // `_extract_files`: each directory's files, then its subdirectories
    // depth-first, requested as the consumer reaches them.
    Ok(crate::sites::paginate(vec![file], move |mut stack| {
        let link = link.clone();
        async move {
            let Some(dir) = stack.pop() else {
                return Ok(None);
            };
            let path: Vec<String> = dir
                .get("path")
                .and_then(Value::as_array)
                .map(|path| path.iter().map(crate::sites::scalar).collect())
                .unwrap_or_default();
            let mut url = http.api(&format!("{}/bundle", link.api))?;
            url.query_pairs_mut()
                .append_pair("password", http.option_str("password").unwrap_or_default())
                .append_pair("path", &format!("/{}", path.join("/")));
            let listing = http.json_with(&url, &headers(&link.referer)).await?;
            let mut dirs = Vec::new();
            let mut entries = Vec::new();
            for item in listing
                .get("files")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let Some(mut item) = item.as_object().cloned() else {
                    continue;
                };
                let mut item_path = path.clone();
                if item.get("type").and_then(Value::as_str) == Some("dir") {
                    item_path.push(
                        item.get("name")
                            .map(crate::sites::scalar)
                            .unwrap_or_default(),
                    );
                    item.insert("path".into(), item_path.into());
                    dirs.push(item);
                } else {
                    item.insert("path".into(), item_path.into());
                    entries.push(item);
                }
            }
            stack.extend(dirs.into_iter().rev());
            let items = files(&link, entries)?;
            Ok(Some((
                crate::sites::events(crate::sites::post(&SITE, http, &SHARED, items)),
                stack,
            )))
        }
    }))
}

/// `file["modified"] / 1000` as Unix seconds.
fn timestamp(file: &Map<String, Value>) -> Option<i64> {
    let ms = file.get("modified")?.as_f64()?;
    ms.is_finite().then(|| (ms / 1000.0).floor() as i64)
}

/// The files of one directory, numbered from one, with their download URLs.
fn files(link: &Link, files: Vec<Map<String, Value>>) -> Result<Vec<Item>> {
    let count = files.len();
    let mut password = link.password.clone();
    let mut items = Vec::with_capacity(count);
    for (index, mut file) in files.into_iter().enumerate() {
        let date = timestamp(&file);
        file.insert("num".into(), (index + 1).into());
        file.insert("count".into(), count.into());
        file.insert("post".into(), link.post.clone());
        file.insert("date".into(), super::date_value(date));
        let name = file
            .get("name")
            .map(crate::sites::scalar)
            .unwrap_or_default();
        super::nameext_from_name(&name, &mut file);
        let path = if link.recursive {
            let dirs: Vec<String> = file
                .get("path")
                .and_then(Value::as_array)
                .map(|path| path.iter().map(crate::sites::scalar).collect())
                .unwrap_or_default();
            if dirs.is_empty() {
                name.clone()
            } else {
                format!("{}/{name}", dirs.join("/"))
            }
        } else {
            password.push_str("&force");
            String::new()
        };
        let raw = format!(
            "{}{}?path=/{}{password}",
            link.base,
            escape(&name),
            escape(&path)
        );
        let url = super::web_url(&raw).ok_or_else(|| error("Koofr returned an invalid URL"))?;
        let id = file.get("id").map(crate::sites::scalar).unwrap_or_default();
        items.push(
            Item::new(url, file)
                .ids(link.uuid.clone(), id)
                .timestamp(date)
                .header("Referer", &link.referer),
        );
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http, json, keywords, media, response, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://app.koofr.net/links/0a1b-2c3d?x"),
            Some(Target::Link("0a1b-2c3d".into()))
        );
        assert_eq!(
            t("https://koofr.eu/links/abc"),
            Some(Target::Link("abc".into()))
        );
        assert_eq!(
            t("https://k00.fr/xyz12"),
            Some(Target::Short("xyz12".into()))
        );
        for raw in [
            "https://www.koofr.net/links/abc",
            "https://app.koofr.net/link/abc",
            "https://app.koofr.net.evil.test/links/abc",
            "https://app.koofr.net:444/links/abc",
            "https://www.k00.fr/xyz",
            "https://k00.fr/",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn link(kind: &str) -> Value {
        json!({"id": "L1", "name": "Share", "publicUrlBase": "https://app.koofr.net",
            "file": {"name": "Share", "type": kind, "modified": 1_717_545_600_000_i64,
                "hash": "abcdef0123456789", "size": 3}})
    }

    #[tokio::test]
    async fn walks_folders_depth_first() {
        let root = json!({"files": [
            {"name": "a.jpg", "type": "file", "modified": 1_717_545_600_000_i64, "hash": "1234567890ab"},
            {"name": "Sub", "type": "dir", "modified": 0},
        ]});
        let sub = json!({"files": [
            {"name": "b c.png", "type": "file", "modified": 0, "hash": "", "id": "IDIDIDIDID"},
        ]});
        let (root_url, server) = serve(vec![json(link("dir")), json(root), json(sub)]);
        let http = http("koofr", root_url);
        let files = media(extract(
            &http,
            Url::parse("https://app.koofr.net/links/U-1").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].url.as_str(),
            "https://app.koofr.net/content/links/U-1/files/get/a.jpg?path=/a.jpg"
        );
        assert_eq!(
            files[1].url.as_str(),
            "https://app.koofr.net/content/links/U-1/files/get/b%20c.png?path=/Sub/b%20c.png"
        );
        assert_eq!(
            files[1].headers.get("Referer").map(String::as_str),
            Some("https://app.koofr.net/links/U-1")
        );
        let k = keywords(&files[1]);
        assert_eq!(k["path"], json!(["Sub"]));
        assert_eq!(k["post"]["title"], "Share");
        assert_eq!(k["num"], 1);
        assert_eq!(keywords(&files[0])["date"], "2024-06-05 00:00:00");
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "001 b c (IDIDIDID).png");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("koofr/Share (L1)/Sub")
        );
        assert_eq!(file.render(&files[0]).unwrap(), "001 a (12345678).jpg");
        assert_eq!(
            crate::sites::archive_key(&files[1]).unwrap().1,
            r#"["{post[id]}_{path:J/}_{hash|id}",["L1","[\"Sub\"]","IDIDIDIDID"]]"#
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/v2/public/links/U-1?password= "));
        assert!(requests[0].contains("x-koofr-version: 2.1"));
        assert!(requests[1].starts_with("GET /api/v2/public/links/U-1/bundle?password=&path=%2F "));
        assert!(requests[2].contains("bundle?password=&path=%2FSub "));
    }

    #[tokio::test]
    async fn single_files_and_short_links() {
        let redirect = response(
            "302 Found\r\nLocation: https://app.koofr.net/links/U2",
            "text/html",
            b"",
        );
        let (root, server) = serve(vec![redirect, json(link("file"))]);
        let http = http("koofr", root);
        let files = media(extract(&http, Url::parse("https://k00.fr/abc").unwrap())).await;
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].url.as_str(),
            "https://app.koofr.net/content/links/U2/files/get/Share?path=/&force"
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /abc "));
        assert!(requests[1].starts_with("GET /api/v2/public/links/U2?password= "));
    }
}
