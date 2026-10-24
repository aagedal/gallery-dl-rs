//! CHZZK community posts, mirroring gallery_dl/extractor/naverchzzk.py
//! through Naver's comment API: one comment, or a channel's community
//! comments page by page with their attached images.
use crate::sites::art::{json_loads, nameext_into, parse_int, str_of, strptime};
use crate::sites::media::iso_ts;
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

const API: &str = "https://apis.naver.com/nng_main/nng_comment_api/v1/type/CHANNEL_POST/id";
const FILENAME: &str = "{uid}_{id}_{num}.{extension}";
const DIRECTORY: &[&str] = &["{category}", "{user[userNickname]}"];
const ARCHIVE: &str = "{uid}_{id}_{num}";
static COMMENT: Route = Route::new("comment", FILENAME, DIRECTORY, ARCHIVE);
static COMMUNITY: Route = Route::new("community", FILENAME, DIRECTORY, ARCHIVE);

pub static SITE: Site = Site {
    category: "naver-chzzk",
    name: "CHZZK",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&COMMENT, &COMMUNITY],
    api_key: false,
    cookies: None,
};

#[derive(Debug, PartialEq)]
enum Target {
    Comment(String, String),
    Community(String),
}

/// `chzzk\.naver\.com/(\w+)/community(?:/detail/(\d+))?`.
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) || url.host_str() != Some("chzzk.naver.com") {
        return None;
    }
    let rest = url.path().strip_prefix('/')?;
    let (uid, rest) = rest.split_once('/')?;
    if uid.is_empty() || !uid.chars().all(crate::sites::media::word_char) {
        return None;
    }
    let rest = rest.strip_prefix("community")?;
    if let Some(id) = rest
        .strip_prefix("/detail/")
        .and_then(crate::sites::art::leading_digits)
    {
        return Some(Target::Comment(uid.into(), id.into()));
    }
    Some(Target::Community(uid.into()))
}

/// `request_api`: the response's `content`.
async fn call(http: &Http, uid: &str, id: &str, params: &[(&str, String)]) -> Result<Value> {
    let mut url = http.api(&format!("{API}/{uid}/comments/{id}"))?;
    if !params.is_empty() {
        let mut query = url.query_pairs_mut();
        for (name, value) in params {
            query.append_pair(name, value);
        }
    }
    http.json(&url)
        .await?
        .get("content")
        .cloned()
        .filter(Value::is_object)
        .ok_or_else(|| error("CHZZK API response has no content"))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Comment(uid, id)) => crate::sites::once(async move {
            let content = call(http, &uid, &id, &[]).await?;
            let comment = serde_json::json!({
                "comment": content.get("comment").cloned().unwrap_or(Value::Null),
                "user": content.get("user").cloned().unwrap_or(Value::Null),
            });
            Ok(crate::sites::events(comment_events(
                http, &COMMENT, &comment,
            )))
        }),
        Some(Target::Community(uid)) => {
            let offset = http
                .option("offset")
                .map(|v| parse_int(&crate::sites::scalar(v), 0))
                .unwrap_or(0);
            crate::sites::paginate(Some(offset), move |offset| {
                let uid = uid.clone();
                async move {
                    let Some(offset) = offset else {
                        return Ok(None);
                    };
                    let params = [
                        ("limit", "10".to_owned()),
                        ("offset", offset.to_string()),
                        ("pagingType", "PAGE".to_owned()),
                    ];
                    let content = call(http, &uid, "", &params).await?;
                    let comments = content.get("comments").cloned().unwrap_or(Value::Null);
                    let mut events = Vec::new();
                    for comment in comments
                        .get("data")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                    {
                        events.extend(comment_events(http, &COMMUNITY, comment));
                    }
                    let next = crate::sites::art::truthy(
                        comments.get("page").and_then(|page| page.get("next")),
                    )
                    .then_some(offset + 10);
                    Ok(Some((crate::sites::events(events), next)))
                }
            })
        }
        None => crate::sites::fail(error("unsupported CHZZK URL")),
    }
}

fn comment_events(
    http: &Http,
    route: &'static Route,
    comment: &Value,
) -> Vec<crate::event::ExtractionEvent> {
    crate::sites::post(&SITE, http, route, parse_comment(comment))
}

/// `items()` for one `{"comment", "user"}` pair.
pub(crate) fn parse_comment(comment: &Value) -> Vec<Item> {
    let Some(mut data) = comment.get("comment").and_then(Value::as_object).cloned() else {
        return Vec::new();
    };
    let files = match data.remove("attaches") {
        Some(Value::Array(files)) => files,
        _ => Vec::new(),
    };
    data.insert(
        "id".into(),
        data.get("commentId").cloned().unwrap_or(Value::Null),
    );
    data.insert(
        "uid".into(),
        data.get("objectId").cloned().unwrap_or(Value::Null),
    );
    data.insert(
        "user".into(),
        comment.get("user").cloned().unwrap_or(Value::Null),
    );
    data.insert("count".into(), files.len().into());
    let ts = strptime(
        str_of(&Value::Object(data.clone()), "createdDate"),
        "%Y%m%d%H%M%S",
    );
    data.insert("date".into(), crate::filter::source_date_json(ts));
    let id = data.get("id").map(crate::sites::scalar).unwrap_or_default();
    let nick = comment
        .get("user")
        .and_then(|user| user.get("userNickname"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let content = data
        .get("content")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let mut items = Vec::new();
    for (index, file) in files.into_iter().enumerate() {
        let Value::Object(mut file) = file else {
            continue;
        };
        if let Some(extra) = file.get("extraJson").and_then(Value::as_str)
            && !extra.is_empty()
            && let Ok(Value::Object(extra)) = json_loads(extra, "CHZZK")
        {
            crate::sites::art::update(&mut file, &extra);
        }
        let created = iso_ts(str_of(&Value::Object(file.clone()), "createdDate"));
        let updated = iso_ts(str_of(&Value::Object(file.clone()), "updatedDate"));
        file.insert("date".into(), crate::filter::source_date_json(created));
        file.insert(
            "date_updated".into(),
            crate::filter::source_date_json(updated),
        );
        let raw = str_of(&Value::Object(file.clone()), "attachValue").to_owned();
        let Some(url) = crate::sites::media::web_url(&raw) else {
            continue;
        };
        let mut fields: Map<String, Value> = data.clone();
        fields.insert("num".into(), (index + 1).into());
        fields.insert("file".into(), Value::Object(file));
        nameext_into(&raw, &mut fields);
        items.push(
            Item::new(url, fields)
                .ids(id.clone(), (index + 1).to_string())
                .user(nick.clone())
                .caption(content.clone())
                .timestamp(ts),
        );
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http, json, keywords, media, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://chzzk.naver.com/abc123/community/detail/45"),
            Some(Target::Comment("abc123".into(), "45".into()))
        );
        assert_eq!(
            t("https://chzzk.naver.com/abc123/community"),
            Some(Target::Community("abc123".into()))
        );
        for raw in [
            "https://chzzk.naver.com/abc123",
            "https://chzzk.naver.com/a-b/community",
            "https://www.chzzk.naver.com/abc/community",
            "https://chzzk.naver.com.evil/abc/community",
            "https://chzzk.naver.com:444/abc/community",
            "https://u@chzzk.naver.com/abc/community",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn comment(id: u64, files: usize) -> Value {
        let attaches: Vec<Value> = (0..files)
            .map(|i| {
                json!({"attachValue": format!("https://nng-phinf.pstatic.net/a/{id}_{i}.png"),
                    "extraJson": "{\"width\": 10}",
                    "createdDate": "2024-01-02T03:04:05", "updatedDate": "2024-01-02T03:04:06"})
            })
            .collect();
        json!({"comment": {"commentId": id, "objectId": "abc", "content": "hi",
                "createdDate": "20240102030405", "attaches": attaches},
            "user": {"userNickname": "Nick"}})
    }

    #[tokio::test]
    async fn extracts_comment() {
        let c = comment(45, 2);
        let (root, server) = serve(vec![json(json!({"content": c}))]);
        let http = http("naver-chzzk", root);
        let files = media(extract(
            &http,
            Url::parse("https://chzzk.naver.com/abc/community/detail/45").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        let k = keywords(&files[1]);
        assert_eq!(k["id"], 45);
        assert_eq!(k["uid"], "abc");
        assert_eq!(k["count"], 2);
        assert_eq!(k["file"]["width"], 10);
        assert!(!k.contains_key("attaches"));
        let (file, dir) = crate::sites::default_templates(&files[1]).unwrap();
        assert_eq!(file.render(&files[1]).unwrap(), "abc_45_2.png");
        assert_eq!(
            dir.render(&files[1]).unwrap().unwrap(),
            std::path::PathBuf::from("naver-chzzk/Nick")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{uid}_{id}_{num}",["abc","45","1"]]"#);
        let requests = server.join().unwrap();
        assert!(
            requests[0].starts_with(
                "GET /nng_main/nng_comment_api/v1/type/CHANNEL_POST/id/abc/comments/45 "
            )
        );
    }

    #[tokio::test]
    async fn pages_through_community() {
        let page = |c: Value, next: bool| {
            json(json!({"content": {"comments": {"data": [c], "page": {"next": next}}}}))
        };
        let (root, server) = serve(vec![page(comment(1, 1), true), page(comment(2, 1), false)]);
        let options = json!({"offset": 20}).as_object().cloned();
        let http = http("naver-chzzk", root).with_options(options.map(std::sync::Arc::new));
        let files = media(extract(
            &http,
            Url::parse("https://chzzk.naver.com/abc/community").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 2);
        assert_eq!(keywords(&files[1])["subcategory"], "community");
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with(
            "GET /nng_main/nng_comment_api/v1/type/CHANNEL_POST/id/abc/comments/?limit=10&offset=20&pagingType=PAGE "
        ));
        assert!(requests[1].contains("offset=30"));
    }
}
