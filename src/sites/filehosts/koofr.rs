//! Koofr shared links, mirroring gallery_dl/extractor/koofr.py: public link
//! metadata and (recursively) its directory listing.
use crate::error;
use crate::sites::gdl::{captures, date, group, html_escape, nameext_from_name, re, str_of};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::{Map, Value};
use url::Url;

const ROOT: &str = "https://app.koofr.net";

static SHARED: Route = Route::new(
    "shared",
    "{num:>03} {filename} ({hash|id:[:8]}).{extension}",
    &["{category}", "{post[title]} ({post[id]})", "{path:I}"],
    "{post[id]}_{path:J/}_{hash|id}",
);

pub static SITE: Site = Site {
    category: "koofr",
    name: "Koofr",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&SHARED],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Link(String),
    Short(String),
}

fn target(url: &Url) -> Option<Target> {
    let pattern = re!(r"^https?://(?:(?:app\.)?koofr\.(?:net|eu)/links/([\w-]+)|k00\.fr/(\w+))");
    let c = captures(pattern, url)?;
    Some(match c.get(1) {
        Some(uuid) => Target::Link(uuid.as_str().into()),
        None => Target::Short(group(&c, 2).into()),
    })
}

fn modified(file: &Value) -> Value {
    date(
        file["modified"]
            .as_f64()
            .map(|ms| (ms / 1000.0).floor() as i64),
    )
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Koofr URL"));
    };
    crate::sites::once(async move {
        let uuid = match target {
            Target::Link(uuid) => uuid,
            Target::Short(code) => {
                let location = http
                    .location(&http.api(&format!("https://k00.fr/{code}"))?, &[])
                    .await?
                    .ok_or_else(|| error("Koofr short link did not redirect"))?;
                location
                    .path()
                    .rsplit('/')
                    .next()
                    .unwrap_or_default()
                    .to_owned()
            }
        };
        let api = format!("{ROOT}/api/v2/public/links/{uuid}");
        let referer = format!("{ROOT}/links/{uuid}");
        let password = http.option_str("password").unwrap_or_default().to_owned();
        let headers = [
            ("Referer", referer.as_str()),
            ("X-Client", "newfrontend"),
            ("X-Koofr-Version", "2.1"),
            ("Sec-Fetch-Dest", "empty"),
            ("Sec-Fetch-Mode", "cors"),
            ("Sec-Fetch-Site", "same-origin"),
        ];
        let mut url = http.api(&api)?;
        url.query_pairs_mut().append_pair("password", &password);
        let data = http.json_with(&url, &headers).await?;
        let file = data["file"].clone();
        let recursive = str_of(&file, "type") == "dir" && http.option_bool("recursive", true);
        let mut post = Map::new();
        post.insert("id".into(), data["id"].clone());
        post.insert("title".into(), data["name"].clone());
        post.insert("date".into(), modified(&file));
        let post = Value::Object(post);
        // (path, files) per directory, depth-first like `_extract_files`.
        let mut listings: Vec<(Vec<String>, Vec<Value>)> = Vec::new();
        if recursive {
            let mut stack = vec![Vec::<String>::new()];
            while let Some(path) = stack.pop() {
                let mut url = http.api(&format!("{api}/bundle"))?;
                url.query_pairs_mut()
                    .append_pair("password", &password)
                    .append_pair("path", &format!("/{}", path.join("/")));
                let listing = http.json_with(&url, &headers).await?;
                let mut files = Vec::new();
                let mut dirs = Vec::new();
                for item in listing["files"].as_array().into_iter().flatten() {
                    if str_of(item, "type") == "dir" {
                        let mut sub = path.clone();
                        sub.push(str_of(item, "name").to_owned());
                        dirs.push(sub);
                    } else {
                        files.push(item.clone());
                    }
                }
                listings.push((path, files));
                stack.extend(dirs.into_iter().rev());
            }
        } else {
            listings.push((Vec::new(), vec![file]));
        }
        let base = format!(
            "{}/content/links/{uuid}/files/get/",
            data.get("publicUrlBase")
                .and_then(Value::as_str)
                .filter(|b| !b.is_empty())
                .unwrap_or(ROOT)
        );
        let mut password_param = if password.is_empty() {
            String::new()
        } else {
            format!("&password={}", html_escape(&password))
        };
        let mut events = Vec::new();
        for (path, files) in listings {
            let count = files.len();
            let mut items = Vec::new();
            for (index, file) in files.into_iter().enumerate() {
                let name = str_of(&file, "name").to_owned();
                let mut fields = file.as_object().cloned().unwrap_or_default();
                fields.insert("path".into(), path.clone().into());
                fields.insert("num".into(), (index + 1).into());
                fields.insert("count".into(), count.into());
                fields.insert("post".into(), post.clone());
                fields.insert("date".into(), modified(&file));
                let (stem, ext) = nameext_from_name(&name);
                fields.insert("filename".into(), stem.into());
                fields.insert("extension".into(), ext.into());
                let file_path = if recursive {
                    if path.is_empty() {
                        name.clone()
                    } else {
                        format!("{}/{name}", path.join("/"))
                    }
                } else {
                    password_param.push_str("&force");
                    String::new()
                };
                let raw = format!(
                    "{base}{}?path=/{}{password_param}",
                    html_escape(&name),
                    html_escape(&file_path)
                );
                let Ok(url) = Url::parse(&raw) else { continue };
                let id = fields
                    .get("hash")
                    .or(fields.get("id"))
                    .map(crate::sites::scalar)
                    .unwrap_or_default();
                items.push(
                    Item::new(url, fields)
                        .ids(crate::sites::scalar(&post["id"]), id)
                        .header("Referer", &referer),
                );
            }
            events.extend(crate::sites::post(&SITE, http, &SHARED, items));
        }
        Ok(crate::sites::events(events))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http as client, json, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://app.koofr.eu/links/923b4f56-3aaf"),
            Some(Target::Link("923b4f56-3aaf".into()))
        );
        assert_eq!(
            t("https://k00.fr/cltf71jr"),
            Some(Target::Short("cltf71jr".into()))
        );
        assert_eq!(t("https://koofr.net/about"), None);
    }

    #[tokio::test]
    async fn walks_shared_directories() {
        let (root, server) = serve(vec![
            json(
                json!({"id": "L1", "name": "Share", "file": {"type": "dir", "name": "Share", "modified": 1600000000000i64}}),
            ),
            json(
                json!({"files": [{"type": "file", "name": "a b.jpg", "hash": "0123456789abcdef", "modified": 1600000000000i64},
                                  {"type": "dir", "name": "Sub"}]}),
            ),
            json(
                json!({"files": [{"type": "file", "name": "c.png", "hash": "fedcba9876543210", "modified": 1600000000000i64}]}),
            ),
        ]);
        let http = client("koofr", root);
        let files = media(extract(
            &http,
            Url::parse("https://app.koofr.net/links/U1").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].url.as_str(),
            "https://app.koofr.net/content/links/U1/files/get/a%20b.jpg?path=/a%20b.jpg"
        );
        assert_eq!(
            files[1].url.as_str(),
            "https://app.koofr.net/content/links/U1/files/get/c.png?path=/Sub/c.png"
        );
        assert_eq!(
            files[1].headers["Referer"],
            "https://app.koofr.net/links/U1"
        );
        let k = keywords(&files[1]);
        assert_eq!(k["path"], json!(["Sub"]));
        assert_eq!(k["post"]["title"], "Share");
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "001 c (fedcba98).png");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("koofr/Share (L1)/Sub")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert!(identity.contains("0123456789abcdef"), "{identity}");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /api/v2/public/links/U1?password= "));
        assert!(
            requests[2].starts_with("GET /api/v2/public/links/U1/bundle?password=&path=%2FSub ")
        );
    }
}
