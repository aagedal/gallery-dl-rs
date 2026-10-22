//! gofile.io folders, mirroring gallery_dl/extractor/gofile.py: a guest
//! account token is created once per run and sent as the `accountToken`
//! cookie for downloads.
use super::{Body, request, sha256_hex, user_agent};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use std::collections::HashMap;
use url::Url;

pub static SITE: Site = Site {
    category: "gofile",
    name: "Gofile",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&FOLDER],
    api_key: false,
};

/// Upstream renders `{name} ({code})` from the folder's Directory message;
/// registry files only carry their own keywords, so the same fields are read
/// from the nested `folder` dictionary each file includes.
static FOLDER: Route = Route::new(
    "folder",
    "{filename}.{extension}",
    &["{category}", "{folder[name]} ({folder[code]})"],
    "{id}",
);

const ROOT: &str = "https://gofile.io";
const API: &str = "https://api.gofile.io";

/// Guest tokens, per API origin.
static TOKENS: tokio::sync::Mutex<Option<HashMap<String, String>>> =
    tokio::sync::Mutex::const_new(None);

fn target(url: &Url) -> Option<String> {
    if !super::on_host(url, &["gofile.io"]) {
        return None;
    }
    match crate::sites::segments(url).as_slice() {
        [d, id, ..] if d == "d" && !id.is_empty() => Some(id.clone()),
        _ => None,
    }
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(id) => crate::sites::once(async move { folder(http, &id).await }),
        None => crate::sites::fail(error("unsupported gofile URL")),
    }
}

fn check(response: Value) -> Result<Value> {
    match response.get("status").and_then(Value::as_str) {
        Some("ok") => Ok(response.get("data").cloned().unwrap_or(Value::Null)),
        Some("error-notFound") => Err(error("gofile content not found")),
        Some("error-passwordRequired") => Err(error("gofile folder requires a password")),
        Some(status) if status.len() < 64 => Err(error(format!("gofile API failed ({status})"))),
        _ => Err(error("gofile API failed")),
    }
}

async fn token(http: &Http) -> Result<String> {
    if let Some(token) = http.option_str("api-token").filter(|t| !t.is_empty()) {
        return Ok(token.to_owned());
    }
    let url = http.api(&format!("{API}/accounts"))?;
    let key = url.origin().ascii_serialization();
    let mut cache = TOKENS.lock().await;
    if let Some(token) = cache.as_ref().and_then(|c| c.get(&key)) {
        return Ok(token.clone());
    }
    let referer = format!("{ROOT}/");
    let response = request(
        http,
        reqwest::Method::POST,
        &url,
        &[("Referer", &referer), ("Origin", ROOT)],
        Body::Empty,
    )
    .await?;
    let data = check(response.json("gofile")?)?;
    let token = data
        .get("token")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty() && t.bytes().all(|b| b.is_ascii_alphanumeric()))
        .ok_or_else(|| error("gofile did not create a guest account"))?
        .to_owned();
    cache
        .get_or_insert_with(HashMap::new)
        .insert(key, token.clone());
    Ok(token)
}

/// `_generate_website_token` from gofile's `wt.obf.js`.
fn website_token(agent: &str, token: &str, now: u64) -> String {
    sha256_hex(format!("{agent}::en-US::{token}::{}::5d4f7g8sd45fsd", now / 14400).as_bytes())
}

async fn folder<'a>(http: &'a Http, id: &str) -> Result<Events<'a>> {
    let token = token(http).await?;
    let mut url = http.api(&format!("{API}/contents/{id}"))?;
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("contentFilter", "")
            .append_pair("page", "1")
            .append_pair("pageSize", "1000")
            .append_pair("sortField", "name")
            .append_pair("sortDirection", "1");
        if let Some(password) = http.option_str("password") {
            query.append_pair("password", &sha256_hex(password.as_bytes()));
        }
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let bearer = format!("Bearer {token}");
    let website = website_token(&user_agent(http), &token, now);
    let referer = format!("{ROOT}/");
    let body = http
        .json_with(
            &url,
            &[
                ("Referer", &referer),
                ("Origin", ROOT),
                ("Authorization", &bearer),
                ("X-Website-Token", &website),
                ("X-BL", "en-US"),
            ],
        )
        .await?;
    let data = check(body)?;
    let (items, children) = parse_folder(data, &token, http.option_bool("recursive", true))?;
    let mut events = crate::sites::events(crate::sites::post(&SITE, http, &FOLDER, items));
    if !children.is_empty() {
        events = futures_util::StreamExt::boxed(futures_util::StreamExt::chain(
            events,
            super::queue_all(http, "gofile", "folder", children),
        ));
    }
    Ok(events)
}

type Children = Vec<(Url, Map<String, Value>)>;

fn parse_folder(data: Value, token: &str, recursive: bool) -> Result<(Vec<Item>, Children)> {
    let Value::Object(mut folder) = data else {
        return Err(error("gofile folder response is not an object"));
    };
    let Some(Value::Object(contents)) = folder.remove("children") else {
        return Err(error("gofile folder requires a password"));
    };
    let folder_value = Value::Object(folder.clone());
    let mut items = Vec::new();
    let mut children = Vec::new();
    let mut num = 0u64;
    for content in contents.into_values() {
        let Value::Object(mut content) = content else {
            continue;
        };
        content.insert("folder".into(), folder_value.clone());
        match content.get("type").and_then(Value::as_str) {
            Some("file") => {
                let Some(url) = content
                    .get("link")
                    .and_then(Value::as_str)
                    .and_then(|link| crate::sites::media_url(link, &["gofile.io"]))
                else {
                    continue;
                };
                num += 1;
                content.insert("num".into(), num.into());
                let name = content
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let (stem, _, ext) = super::rpartition(&name, ".");
                content.insert("filename".into(), stem.into());
                content.insert("extension".into(), ext.into());
                let id = content
                    .get("id")
                    .map(crate::sites::scalar)
                    .unwrap_or_default();
                let created = content.get("createTime").and_then(Value::as_i64);
                let code = folder
                    .get("code")
                    .map(crate::sites::scalar)
                    .unwrap_or_default();
                items.push(
                    Item::new(url, content)
                        .ids(code, id)
                        .timestamp(created)
                        .header("Cookie", &format!("accountToken={token}")),
                );
            }
            Some("folder") if recursive => {
                let id = content
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| {
                        !id.is_empty()
                            && id
                                .bytes()
                                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                    })
                    .map(str::to_owned);
                if let Some(child) = id.and_then(|id| Url::parse(&format!("{ROOT}/d/{id}")).ok()) {
                    children.push((child, content));
                }
            }
            _ => {}
        }
    }
    Ok((items, children))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http, json, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(t("https://gofile.io/d/AbC123"), Some("AbC123".into()));
        assert_eq!(t("https://www.gofile.io/d/AbC123"), Some("AbC123".into()));
        for raw in [
            "https://gofile.io/",
            "https://gofile.io/x/AbC",
            "https://gofile.io.evil.test/d/AbC",
            "https://evilgofile.io/d/AbC",
            "https://gofile.io:8443/d/AbC",
            "https://user@gofile.io/d/AbC",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[test]
    fn website_token_matches_upstream_formula() {
        let expected = sha256_hex(b"UA::en-US::tok::2::5d4f7g8sd45fsd");
        assert_eq!(website_token("UA", "tok", 2 * 14400 + 5), expected);
    }

    #[test]
    fn missing_children_means_password() {
        let err = parse_folder(json!({"id": "x"}), "t", true).err().unwrap();
        assert!(err.to_string().contains("password"));
    }

    fn folder_body(code: &str, children: Value) -> Value {
        json!({"status": "ok", "data": {"id": format!("id-{code}"), "type": "folder",
            "name": format!("Folder {code}"), "code": code, "createTime": 1700000000,
            "children": children}})
    }

    #[tokio::test]
    async fn extracts_folder_with_guest_token_and_subfolders() {
        let (root, server) = serve(vec![
            json(json!({"status": "ok", "data": {"token": "GuestTok1"}})),
            json(folder_body(
                "abc",
                json!({
                    "f1": {"id": "f1", "type": "file", "name": "photo.one.JPG",
                           "createTime": 1700000100, "size": 10, "md5": "m",
                           "link": "https://store1.gofile.io/download/web/f1/photo.one.JPG"},
                    "sub": {"id": "sub", "type": "folder", "name": "Inner", "code": "sub"},
                }),
            )),
            json(folder_body(
                "sub",
                json!({"f2": {"id": "f2", "type": "file", "name": "clip.mp4",
                    "link": "https://store2.gofile.io/download/web/f2/clip.mp4"}}),
            )),
        ]);
        let http = http("Gofile", root);
        let files = media(extract(
            &http,
            Url::parse("https://gofile.io/d/abc").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[0]);
        assert_eq!(k["filename"], "photo.one");
        assert_eq!(k["extension"], "JPG");
        assert_eq!(k["num"], 1);
        assert_eq!(k["folder"]["code"], "abc");
        assert!(k["folder"].get("children").is_none());
        assert!(!serde_json::to_string(k).unwrap().contains("GuestTok1"));
        assert_eq!(
            files[0].headers.get("Cookie").map(String::as_str),
            Some("accountToken=GuestTok1")
        );
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "photo.one.JPG");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("gofile/Folder abc (abc)")
        );
        assert_eq!(
            crate::sites::archive_key(&files[0]).unwrap().1,
            r#"["{id}",["f1"]]"#
        );
        assert_eq!(keywords(&files[1])["folder"]["code"], "sub");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("POST /accounts "));
        assert!(
            requests[1].starts_with(
                "GET /contents/abc?contentFilter=&page=1&pageSize=1000&sortField=name&sortDirection=1 "
            ),
            "{}",
            requests[1]
        );
        assert!(
            requests[1]
                .to_ascii_lowercase()
                .contains("authorization: bearer guesttok1")
        );
        assert!(
            requests[1]
                .to_ascii_lowercase()
                .contains("x-website-token: ")
        );
        // The cached guest token is reused for the queued subfolder.
        assert!(requests[2].starts_with("GET /contents/sub?"));
    }

    #[tokio::test]
    async fn reports_api_status() {
        let (root, _server) = serve(vec![json(json!({"status": "error-notFound"}))]);
        let http = http("Gofile", root).with_options(Some(std::sync::Arc::new(
            json!({"api-token": "Configured"})
                .as_object()
                .unwrap()
                .clone(),
        )));
        let events: Vec<_> = futures_util::StreamExt::collect(extract(
            &http,
            Url::parse("https://gofile.io/d/zzz").unwrap(),
        ))
        .await;
        assert!(
            events[0]
                .as_ref()
                .err()
                .unwrap()
                .to_string()
                .contains("not found")
        );
    }
}
