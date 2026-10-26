//! Plurk timelines and posts: links in plurks (and optionally their
//! comments) are queued for other extractors, mirroring
//! gallery_dl/extractor/plurk.py.
use crate::error;
use crate::sites::gdl::{Scan, captures, extract_iter, group, re, str_of, strptime, truthy};
use crate::sites::{Events, Http, Route, Site};
use serde_json::Value;

use url::Url;

const ROOT: &str = "https://www.plurk.com";

static TIMELINE: Route = Route::new("timeline", "{filename}.{extension}", &["{category}"], "");
static POST: Route = Route::new("post", "{filename}.{extension}", &["{category}"], "");

pub static SITE: Site = Site {
    category: "plurk",
    name: "Plurk",
    matches: |url| target(url).is_some(),
    extract,
    routes: &[&TIMELINE, &POST],
    api_key: false,
    cookies: Some("plurk.com"),
};

#[derive(Debug, PartialEq)]
enum Target {
    Timeline(String),
    Post(String),
}

fn target(url: &Url) -> Option<Target> {
    if let Some(c) = captures(re!(r"^https?://(?:www\.)?plurk\.com/p/(\w+)"), url) {
        return Some(Target::Post(group(&c, 1).into()));
    }
    captures(
        re!(r"^https?://(?:www\.)?plurk\.com/(\w+)/?(?:$|[?#])"),
        url,
    )
    .filter(|c| &c[1] != "p")
    .map(|c| Target::Timeline(group(&c, 1).into()))
}

/// `_load`: JSON with `new Date(...)` constructors unwrapped.
fn load(data: &str) -> crate::Result<Value> {
    if data.is_empty() {
        return Err(error("Plurk user not found"));
    }
    let data = re!(r"new Date\(([^)]+)\)").replace_all(data, "$1");
    serde_json::from_str(&data).map_err(|_| error("Plurk returned invalid data"))
}

/// Upstream `request_interval = (0.5, 1.5)`.
async fn pace() {
    if !cfg!(test) {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

/// `_comments`: all responses to a plurk.
async fn comments(http: &Http, plurk: &Value) -> crate::Result<Vec<Value>> {
    let url = http.api(&format!("{ROOT}/Responses/get"))?;
    let headers = [
        ("Origin", ROOT),
        ("Referer", ROOT),
        ("X-Requested-With", "XMLHttpRequest"),
    ];
    let id = crate::sites::scalar(&plurk["id"]);
    let mut count = Some("200");
    let mut from = None::<String>;
    let mut out = Vec::new();
    loop {
        let mut form = vec![("plurk_id", id.as_str())];
        if let Some(count) = count {
            form.push(("count", count));
        }
        if let Some(from) = &from {
            form.push(("from_response_id", from.as_str()));
        }
        pace().await;
        let info = http.post_form(&url, &form, &headers).await?;
        let responses = info["responses"].as_array().cloned().unwrap_or_default();
        let last = responses.last().and_then(|r| r["id"].as_i64());
        out.extend(responses);
        let newer = crate::sites::gdl::int_of(info.get("has_newer"));
        if !truthy(info.get("has_newer")) {
            return Ok(out);
        }
        if newer < 200 {
            count = None;
        }
        let Some(last) = last else { return Ok(out) };
        from = Some((last + 1).to_string());
    }
}

/// Queued links of plurks, plus their comments when `comments` is set.
async fn links<'a>(
    http: &'a Http,
    route: &'static Route,
    plurks: Vec<Value>,
) -> crate::Result<Events<'a>> {
    let with_comments = truthy(http.option("comments"));
    let mut children = Vec::new();
    for plurk in plurks {
        let data = plurk.as_object().cloned().unwrap_or_default();
        let mut sources = vec![plurk.clone()];
        if with_comments {
            sources.extend(comments(http, &plurk).await?);
        }
        for source in &sources {
            for link in extract_iter(str_of(source, "content"), " href=\"", "\"") {
                if let Ok(url) = Url::parse(link) {
                    children.push((url, data.clone()));
                }
            }
        }
    }
    Ok(crate::sites::galleries::queue(http, &SITE, route, children))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    let Some(target) = target(&url) else {
        return crate::sites::fail(error("unsupported Plurk URL"));
    };
    match target {
        Target::Post(id) => crate::sites::once(async move {
            let page = http.text(&http.api(&format!("{ROOT}/p/{id}"))?).await?;
            let mut scan = Scan::new(&page);
            let user = scan.extr(" GLOBAL=", "\n").to_owned();
            let mut data = load(scan.extr("plurk =", ";\n"))?;
            if let (Ok(user), Some(map)) = (load(&user), data.as_object_mut()) {
                map.insert("user".into(), user["page_user"].clone());
            }
            links(http, &POST, vec![data]).await
        }),
        Target::Timeline(user) => {
            crate::sites::paginate(Some((None::<String>, None::<Vec<Value>>)), move |state| {
                let user = user.clone();
                async move {
                    let Some((user_id, plurks)) = state else {
                        return Ok(None);
                    };
                    let referer = format!("{ROOT}/{user}");
                    let (user_id, plurks) = match (user_id, plurks) {
                        (Some(id), Some(plurks)) => (id, plurks),
                        _ => {
                            let page = http.text(&http.api(&referer)?).await?;
                            let mut scan = Scan::new(&page);
                            let id = scan.extr("\"page_user\": {\"id\":", ",").trim().to_owned();
                            let plurks = load(scan.extr("_PLURKS = ", ";\n"))?
                                .as_array()
                                .cloned()
                                .unwrap_or_default();
                            (id, plurks)
                        }
                    };
                    if plurks.is_empty() {
                        return Ok(None);
                    }
                    let offset = strptime(
                        str_of(plurks.last().unwrap_or(&Value::Null), "posted")
                            .trim_end_matches(" GMT"),
                        "%a, %d %b %Y %H:%M:%S",
                    );
                    let events = links(http, &TIMELINE, plurks).await?;
                    let Some(offset) =
                        offset.and_then(|ts| time::OffsetDateTime::from_unix_timestamp(ts).ok())
                    else {
                        return Ok(Some((events, None)));
                    };
                    let offset = format!(
                        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.000Z",
                        offset.year(),
                        offset.month() as u8,
                        offset.day(),
                        offset.hour(),
                        offset.minute(),
                        offset.second()
                    );
                    pace().await;
                    let next = http
                        .post_form(
                            &http.api(&format!("{ROOT}/TimeLine/getPlurks"))?,
                            &[("user_id", user_id.as_str()), ("offset", offset.as_str())],
                            &[
                                ("Referer", referer.as_str()),
                                ("X-Requested-With", "XMLHttpRequest"),
                            ],
                        )
                        .await?;
                    let next = next["plurks"].as_array().cloned().unwrap_or_default();
                    Ok(Some((events, Some((Some(user_id), Some(next))))))
                }
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::testing::{children, html, http as client, json, serve};
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.plurk.com/plurkapi"),
            Some(Target::Timeline("plurkapi".into()))
        );
        assert_eq!(
            t("https://www.plurk.com/p/i701j1"),
            Some(Target::Post("i701j1".into()))
        );
        assert_eq!(t("https://www.plurk.com/p"), None);
    }

    #[test]
    fn unwraps_dates() {
        let value =
            load(r#"{"posted": new Date("Fri, 05 Apr 2024 01:02:03 GMT"), "id": 1}"#).unwrap();
        assert_eq!(value["posted"], "Fri, 05 Apr 2024 01:02:03 GMT");
    }

    #[tokio::test]
    async fn timeline_queues_links_and_pages_by_offset() {
        let page = "var GLOBAL = {\"page_user\": {\"id\": 42, \"x\": 1}};\nvar _PLURKS = [{\"id\": 1, \"posted\": new Date(\"Fri, 05 Apr 2024 01:02:03 GMT\"), \"content\": \"<a href=\\\"https://imgur.com/a\\\">x</a>\"}];\n";
        let (root, server) = serve(vec![html(page), json(json!({"plurks": []}))]);
        let http = client("plurk", root);
        let queued = children(extract(
            &http,
            Url::parse("https://www.plurk.com/plurkapi").unwrap(),
        ))
        .await;
        assert_eq!(queued.len(), 1);
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("POST /TimeLine/getPlurks "));
        assert!(requests[1].ends_with("user_id=42&offset=2024-04-05T01%3A02%3A03.000Z"));
    }
}
