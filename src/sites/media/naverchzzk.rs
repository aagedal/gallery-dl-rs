//! CHZZK community posts, mirroring gallery_dl/extractor/naverchzzk.py
//! through Naver's comment API. Option: `offset`.
use crate::error;
use crate::sites::gdl::{
    captures, date, group, int_of, nameext_into, parse_iso, re, str_of, strptime,
};
use crate::sites::{Events, Http, Item, Route, Site};
use serde_json::Value;
use url::Url;

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

fn target(url: &Url) -> Option<Target> {
    if let Some(c) = captures(
        re!(r"^https?://chzzk\.naver\.com/(\w+)/community/detail/(\d+)"),
        url,
    ) {
        return Some(Target::Comment(group(&c, 1).into(), group(&c, 2).into()));
    }
    captures(re!(r"^https?://chzzk\.naver\.com/(\w+)/community"), url)
        .map(|c| Target::Community(group(&c, 1).into()))
}

async fn api(http: &Http, uid: &str, id: &str, params: &[(&str, String)]) -> crate::Result<Value> {
    let mut url = http.api(&format!("https://apis.naver.com/nng_main/nng_comment_api/v1/type/CHANNEL_POST/id/{uid}/comments/{id}"))?;
    if !params.is_empty() {
        url.query_pairs_mut()
            .extend_pairs(params.iter().map(|(k, v)| (*k, v.as_str())));
    }
    Ok(http.json(&url).await?["content"].clone())
}

/// `NaverChzzkExtractor.items` for one comment.
fn comment_items(comment: &Value) -> Vec<Item> {
    let Some(mut data) = comment["comment"].as_object().cloned() else {
        return Vec::new();
    };
    let files = data
        .remove("attaches")
        .and_then(|a| a.as_array().cloned())
        .unwrap_or_default();
    data.insert(
        "id".into(),
        data.get("commentId").cloned().unwrap_or(Value::Null),
    );
    data.insert(
        "uid".into(),
        data.get("objectId").cloned().unwrap_or(Value::Null),
    );
    data.insert("user".into(), comment["user"].clone());
    data.insert("count".into(), files.len().into());
    let created = data
        .get("createdDate")
        .map(crate::sites::scalar)
        .unwrap_or_default();
    data.insert("date".into(), date(strptime(&created, "%Y%m%d%H%M%S")));
    let id = data.get("id").map(crate::sites::scalar).unwrap_or_default();
    files
        .into_iter()
        .enumerate()
        .filter_map(|(index, file)| {
            let mut file = file.as_object().cloned()?;
            if let Some(Value::String(extra)) = file.get("extraJson")
                && let Ok(Value::Object(extra)) = serde_json::from_str::<Value>(extra)
            {
                file.extend(extra);
            }
            let created = file
                .get("createdDate")
                .map(crate::sites::scalar)
                .unwrap_or_default();
            let updated = file
                .get("updatedDate")
                .map(crate::sites::scalar)
                .unwrap_or_default();
            file.insert("date".into(), date(parse_iso(&created)));
            file.insert("date_updated".into(), date(parse_iso(&updated)));
            let raw = str_of(&Value::Object(file.clone()), "attachValue").to_owned();
            let mut fields = data.clone();
            fields.insert("num".into(), (index + 1).into());
            fields.insert("file".into(), Value::Object(file));
            nameext_into(&raw, &mut fields);
            Some(Item::new(Url::parse(&raw).ok()?, fields).ids(id.clone(), (index + 1).to_string()))
        })
        .collect()
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported CHZZK URL"));
    };
    match target {
        Target::Comment(uid, id) => crate::sites::once(async move {
            let res = api(http, &uid, &id, &[]).await?;
            let comment = serde_json::json!({"comment": res["comment"], "user": res["user"]});
            Ok(crate::sites::events(crate::sites::post(
                &SITE,
                http,
                &COMMENT,
                comment_items(&comment),
            )))
        }),
        Target::Community(uid) => {
            let offset = http.option("offset").map_or(0, |v| int_of(Some(v)));
            crate::sites::paginate(Some(offset), move |state| {
                let uid = uid.clone();
                async move {
                    let Some(offset) = state else { return Ok(None) };
                    let params = [
                        ("limit", "10".to_owned()),
                        ("offset", offset.to_string()),
                        ("pagingType", "PAGE".to_owned()),
                    ];
                    let comments = api(http, &uid, "", &params).await?["comments"].clone();
                    let events = comments["data"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .flat_map(|comment| {
                            crate::sites::post(&SITE, http, &COMMUNITY, comment_items(comment))
                        })
                        .collect();
                    let next = crate::sites::gdl::truthy(comments["page"].get("next"))
                        .then_some(offset + 10);
                    Ok(Some((crate::sites::events(events), next)))
                }
            })
        }
    }
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
            t("https://chzzk.naver.com/f30b95/community/detail/13393754"),
            Some(Target::Comment("f30b95".into(), "13393754".into()))
        );
        assert_eq!(
            t("https://chzzk.naver.com/f30b95/community"),
            Some(Target::Community("f30b95".into()))
        );
    }

    #[tokio::test]
    async fn extracts_comment_attachments() {
        let (root, server) = serve(vec![json(json!({"content": {
            "comment": {"commentId": 77, "objectId": "f30b95", "createdDate": "20240102030405",
                "attaches": [{"attachValue": "https://nng-phinf.pstatic.net/a/1.png", "extraJson": "{\"width\": 100}",
                    "createdDate": "2024-01-02T03:04:05", "updatedDate": "2024-01-02T03:04:05"}]},
            "user": {"userNickname": "Nick"}}}))]);
        let http = client("naver-chzzk", root);
        let files = media(extract(
            &http,
            Url::parse("https://chzzk.naver.com/f30b95/community/detail/77").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 1);
        let k = keywords(&files[0]);
        assert_eq!(k["file"]["width"], 100);
        assert_eq!(k["date"], "2024-01-02 03:04:05");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "f30b95_77_1.png");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("naver-chzzk/Nick")
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with(
            "GET /nng_main/nng_comment_api/v1/type/CHANNEL_POST/id/f30b95/comments/77 "
        ));
    }
}
