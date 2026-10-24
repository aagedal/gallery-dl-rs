//! Are.na channels, mirroring gallery_dl/extractor/arena.py through the
//! public `api.are.na/v2` channel API.
use super::{on_host, raw_segments, str_of};
use crate::sites::{Events, Http, Item, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
use url::Url;

const API: &str = "https://api.are.na/v2/channels";
const LIMIT: usize = 100;
static CHANNEL: Route = Route::new(
    "channel",
    "{num:>03}{block[id]:? //}.{extension}",
    &[
        "{category}",
        "{user[full_name]} ({user[id]})",
        "{channel[title]} ({channel[id]})",
    ],
    "{channel[id]}/{block[id]}",
);

pub static SITE: Site = Site {
    category: "arena",
    name: "Are.na",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&CHANNEL],
    api_key: false,
    cookies: None,
};

/// `are\.na/[^/?#]+/([^/?#]+)`: the channel slug.
fn target(url: &Url) -> Option<String> {
    if !on_host(url, &["are.na"]) {
        return None;
    }
    match raw_segments(url).as_slice() {
        [user, slug, ..] if url.path().starts_with(&format!("/{user}/{slug}")) => {
            Some((*slug).to_owned())
        }
        _ => None,
    }
}

/// Nested datetimes in the internal source-date form, so templates can
/// format them; public JSON output renders them as date text.
fn dates(object: &mut Map<String, Value>) -> Option<i64> {
    let created = super::parse_iso(
        object
            .get("created_at")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    let updated = super::parse_iso(
        object
            .get("updated_at")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    object.insert("date".into(), crate::filter::source_date_json(created));
    object.insert(
        "date_updated".into(),
        crate::filter::source_date_json(updated),
    );
    created
}

/// GalleryExtractor metadata: `count`, `user`, `owner` and `channel`.
pub(crate) fn metadata(channel: Value) -> Result<Map<String, Value>> {
    let Value::Object(mut channel) = channel else {
        return Err(error("Are.na channel is not an object"));
    };
    dates(&mut channel);
    channel.remove("contents");
    let mut data = Map::new();
    data.insert(
        "count".into(),
        channel.get("length").cloned().unwrap_or(Value::Null),
    );
    data.insert("user".into(), channel.remove("user").unwrap_or(Value::Null));
    data.insert(
        "owner".into(),
        channel.remove("owner").unwrap_or(Value::Null),
    );
    data.insert("channel".into(), Value::Object(channel));
    Ok(data)
}

fn block_url(block: &Value) -> Option<&str> {
    fn pick(value: Option<&Value>) -> Option<&str> {
        value.and_then(|v| v.get("url")).and_then(Value::as_str)
    }
    if let Some(attachment) = block.get("attachment").filter(|a| super::truthy(Some(a))) {
        return pick(Some(attachment));
    }
    let image = block.get("image").filter(|i| super::truthy(Some(i)))?;
    for key in ["original", "display", "large"] {
        if let Some(value) = image.get(key).filter(|v| super::truthy(Some(v))) {
            return pick(Some(value));
        }
    }
    None
}

struct State {
    page: u64,
    num: u64,
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(slug) = target(&url) else {
        return crate::sites::fail(error("unsupported Are.na URL"));
    };
    crate::sites::once(async move {
        let channel = http.json(&http.api(&format!("{API}/{slug}"))?).await?;
        let data = metadata(channel)?;
        // `zip(range(1, count + 1), images)` stops after `count` files.
        let count = data.get("count").and_then(Value::as_u64);
        Ok(crate::sites::paginate(
            Some(State { page: 1, num: 1 }),
            move |state| {
                let slug = slug.clone();
                let data = data.clone();
                async move {
                    let Some(state) = state else {
                        return Ok(None);
                    };
                    let mut url = http.api(&format!("{API}/{slug}/contents"))?;
                    url.query_pairs_mut()
                        .append_pair("page", &state.page.to_string())
                        .append_pair("per", &LIMIT.to_string());
                    let body = http.json(&url).await?;
                    let Some(contents) = body
                        .get("contents")
                        .and_then(Value::as_array)
                        .filter(|c| !c.is_empty())
                    else {
                        return Ok(None);
                    };
                    let (items, num) = parse_contents(contents, &data, state.num, count);
                    let events = crate::sites::post(&SITE, http, &CHANNEL, items);
                    let more = contents.len() >= LIMIT
                        && count.is_none_or(|count| num <= count)
                        && state.page < 10_000;
                    let next = more.then_some(State {
                        page: state.page + 1,
                        num,
                    });
                    Ok(Some((crate::sites::events(events), next)))
                }
            },
        ))
    })
}

/// The downloadable blocks of one contents page, numbered from `num`.
pub(crate) fn parse_contents(
    contents: &[Value],
    data: &Map<String, Value>,
    mut num: u64,
    count: Option<u64>,
) -> (Vec<Item>, u64) {
    let channel_id = data
        .get("channel")
        .and_then(|c| c.get("id"))
        .map(crate::sites::scalar)
        .unwrap_or_default();
    let user = data
        .get("user")
        .map(|user| str_of(user, "full_name").to_owned());
    let mut items = Vec::new();
    for block in contents {
        if count.is_some_and(|count| num > count) {
            break;
        }
        let Some(raw) = block_url(block) else {
            continue;
        };
        let Some(url) = super::file_url(raw, "https://are.na/") else {
            continue;
        };
        let Value::Object(mut block) = block.clone() else {
            continue;
        };
        let created = dates(&mut block);
        let source = block.remove("source").unwrap_or(Value::Null);
        let id = block
            .get("id")
            .map(crate::sites::scalar)
            .unwrap_or_default();
        let title = block
            .get("title")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let mut fields = data.clone();
        fields.insert("block".into(), Value::Object(block));
        fields.insert("source".into(), source);
        fields.insert("num".into(), num.into());
        super::nameext_into(url.as_str(), &mut fields);
        items.push(
            Item::new(url, fields)
                .ids(channel_id.clone(), id)
                .user(user.clone())
                .caption(title)
                .timestamp(created),
        );
        num += 1;
    }
    (items, num)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{http, json, keywords, media, serve};
    use futures_util::StreamExt;
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://are.na/evan-collins-1522646491/cassette-futurism"),
            Some("cassette-futurism".into())
        );
        assert_eq!(t("https://www.are.na/u/c/x"), Some("c".into()));
        for raw in [
            "https://are.na/u",
            "https://are.na.evil.com/u/c",
            "https://evilare.na/u/c",
            "https://api.are.na/u/c",
            "https://are.na:8080/u/c",
            "https://u:p@are.na/u/c",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    fn block(id: u64, kind: &str) -> Value {
        let url = format!("https://d2w9rnfcy7mm78.cloudfront.net/{id}/original_x.png?1");
        let mut block = json!({"id": id, "title": "B", "created_at": "2020-01-02T03:04:05.000Z",
            "updated_at": "2020-01-03T03:04:05.000Z", "source": {"url": "https://src"}});
        match kind {
            "image" => block["image"] = json!({"original": {"url": url}}),
            "attachment" => {
                block["attachment"] =
                    json!({"url": format!("https://attachments.are.na/{id}/doc.pdf")})
            }
            _ => {}
        }
        block
    }

    #[tokio::test]
    async fn paginates_channel_contents() {
        let channel = json!({"id": 7, "title": "Cassette", "length": 102,
            "created_at": "2019-01-01T00:00:00.000Z", "updated_at": "2019-01-02T00:00:00.000Z",
            "user": {"id": 1, "full_name": "Evan"}, "owner": {"id": 1}, "contents": []});
        let mut first: Vec<Value> = (1..=99).map(|id| block(id, "image")).collect();
        first.push(block(100, "text"));
        let (root, server) = serve(vec![
            json(channel),
            json(json!({"contents": first})),
            json(json!({"contents": [block(101, "attachment"), block(102, "image")]})),
        ]);
        let http = http("arena", root);
        let files = media(extract(
            &http,
            Url::parse("https://www.are.na/evan/cassette").unwrap(),
        ))
        .await;
        assert_eq!(files.len(), 101);
        assert_eq!(
            files[99].url.as_str(),
            "https://attachments.are.na/101/doc.pdf"
        );
        let k = keywords(&files[99]);
        assert_eq!(k["num"], 100);
        assert_eq!(k["count"], 102);
        assert_eq!(k["source"]["url"], "https://src");
        assert!(!k["channel"].as_object().unwrap().contains_key("user"));
        let public = crate::postprocess::public_keywords(&crate::postprocess::context(&files[0]));
        assert_eq!(public["block"]["date"], "2020-01-02 03:04:05");
        assert_eq!(public["channel"]["date_updated"], "2019-01-02 00:00:00");
        let (file, dir) = crate::sites::default_templates(&files[0]).unwrap();
        assert_eq!(file.render(&files[0]).unwrap(), "001 1.png");
        assert_eq!(
            dir.render(&files[0]).unwrap().unwrap(),
            std::path::PathBuf::from("arena/Evan (1)/Cassette (7)")
        );
        let (_, identity) = crate::sites::archive_key(&files[0]).unwrap();
        assert_eq!(identity, r#"["{channel[id]}/{block[id]}",["7","1"]]"#);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /v2/channels/cassette "));
        assert!(requests[1].starts_with("GET /v2/channels/cassette/contents?page=1&per=100 "));
        assert!(requests[2].starts_with("GET /v2/channels/cassette/contents?page=2&per=100 "));
    }

    #[tokio::test]
    async fn missing_channel_is_an_error() {
        let (root, server) = serve(vec![crate::sites::testing::response(
            "404 Not Found",
            "application/json",
            b"{}",
        )]);
        let http = http("arena", root);
        let events = extract(&http, Url::parse("https://www.are.na/u/gone").unwrap());
        let results: Vec<_> = crate::event::media_only(events).collect().await;
        assert!(results.iter().any(Result::is_err));
        server.join().unwrap();
    }
}
