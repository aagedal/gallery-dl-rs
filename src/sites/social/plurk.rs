//! Plurk timelines and posts, mirroring gallery_dl/extractor/plurk.py: the
//! links in each plurk (and, with the `comments` option, its responses) are
//! queued as child jobs with the plurk as their metadata.
//!
//! Upstream queues every link and lets unsupported ones fail; here links no
//! registry site handles are skipped.
use crate::sites::art::{Scan, extract_iter, strptime};
use crate::sites::{Events, Http, Route, Site};
use crate::{Result, error};
use serde_json::{Map, Value};
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

/// Python `\w` restricted to ASCII.
fn word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// `plurk\.com/p/(\w+)` and `plurk\.com/(?!p/)(\w+)/?(?:$|[?#])`.
fn target(url: &Url) -> Option<Target> {
    if !crate::sites::plain(url) || !crate::sites::host_is(url, &["plurk.com"]) {
        return None;
    }
    let path = url.path().strip_prefix('/')?;
    if let Some(id) = path.strip_prefix("p/") {
        let len = id.bytes().take_while(|b| word(*b)).count();
        return (len > 0).then(|| Target::Post(id[..len].to_owned()));
    }
    let user = path.strip_suffix('/').unwrap_or(path);
    (!user.is_empty() && user.bytes().all(word)).then(|| Target::Timeline(user.to_owned()))
}

fn extract(http: &Http, url: Url) -> Events<'_> {
    match target(&url) {
        Some(Target::Timeline(user)) => timeline(http, user),
        Some(Target::Post(id)) => crate::sites::once(async move {
            let page = http.text(&http.api(&format!("{ROOT}/p/{id}"))?).await?;
            let plurk = parse_post(&page)?;
            Ok(queue_links(http, &POST, vec![plurk]))
        }),
        None => crate::sites::fail(error("unsupported Plurk URL")),
    }
}

/// `_load`: page JSON with `new Date(...)` wrappers removed.
pub(crate) fn load(data: Option<&str>) -> Result<Value> {
    let data = data
        .filter(|d| !d.is_empty())
        .ok_or_else(|| error("Plurk user not found"))?;
    let mut out = String::with_capacity(data.len());
    let mut rest = data;
    while let Some(start) = rest.find("new Date(") {
        let inner = &rest[start + 9..];
        match inner.find(')').filter(|end| *end > 0) {
            Some(end) => {
                out.push_str(&rest[..start]);
                out.push_str(&inner[..end]);
                rest = &inner[end + 1..];
            }
            None => {
                out.push_str(&rest[..start + 9]);
                rest = inner;
            }
        }
    }
    out.push_str(rest);
    crate::sites::art::json_loads(&out, "Plurk")
}

/// The plurk of a post page, with the page's `user` when available.
pub(crate) fn parse_post(page: &str) -> Result<Value> {
    let mut scan = Scan::new(page);
    let user = scan.find(" GLOBAL=", "\n");
    let mut plurk = load(scan.find("plurk =", ";\n"))?;
    // Upstream only warns when the user data is unavailable.
    if let Ok(user) = load(user)
        && let (Some(page_user), Some(plurk)) = (user.get("page_user"), plurk.as_object_mut())
    {
        plurk.insert("user".into(), page_user.clone());
    }
    Ok(plurk)
}

/// `_PLURKS` of a timeline page and the page user's id.
pub(crate) fn parse_timeline(page: &str) -> Result<(String, Vec<Value>)> {
    let mut scan = Scan::new(page);
    let user_id = scan
        .find("\"page_user\": {\"id\":", ",")
        .map(|id| id.trim().to_owned())
        .ok_or_else(|| error("Plurk page has no user id"))?;
    let plurks = list(load(scan.find("_PLURKS = ", ";\n"))?);
    Ok((user_id, plurks))
}

fn list(value: Value) -> Vec<Value> {
    match value {
        Value::Array(items) => items,
        _ => Vec::new(),
    }
}

/// The `offset` for the page after a plurk posted at
/// `%a, %d %b %Y %H:%M:%S %Z`.
pub(crate) fn offset(posted: &str) -> Option<String> {
    let (_, rest) = posted.split_once(", ")?;
    let (stamp, _zone) = rest.rsplit_once(' ')?;
    let seconds = strptime(stamp, "%d %b %Y %H:%M:%S")?;
    let dt = time::OffsetDateTime::from_unix_timestamp(seconds).ok()?;
    Some(format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.000Z",
        dt.year(),
        u8::from(dt.month()),
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second()
    ))
}

enum Page {
    First(String),
    Next { user_id: String, offset: String },
}

fn timeline(http: &Http, user: String) -> Events<'_> {
    crate::sites::paginate(Some(Page::First(user.clone())), move |state| {
        let referer = format!("{ROOT}/{user}");
        async move {
            let (user_id, plurks) = match state {
                None => return Ok(None),
                Some(Page::First(user)) => {
                    parse_timeline(&http.text(&http.api(&format!("{ROOT}/{user}"))?).await?)?
                }
                Some(Page::Next { user_id, offset }) => {
                    let url = http.api(&format!("{ROOT}/TimeLine/getPlurks"))?;
                    let form = [("user_id", user_id.as_str()), ("offset", offset.as_str())];
                    let headers = [
                        ("Referer", referer.as_str()),
                        ("X-Requested-With", "XMLHttpRequest"),
                    ];
                    let body = http.post_form(&url, &form, &headers).await?;
                    let plurks = list(body.get("plurks").cloned().unwrap_or(Value::Null));
                    (user_id, plurks)
                }
            };
            if plurks.is_empty() {
                return Ok(None);
            }
            let next = plurks
                .last()
                .and_then(|plurk| plurk.get("posted"))
                .and_then(Value::as_str)
                .and_then(offset)
                .map(|offset| Page::Next { user_id, offset });
            Ok(Some((queue_links(http, &TIMELINE, plurks), next)))
        }
    })
}

/// Queue the links of each plurk; comments are fetched lazily per plurk.
fn queue_links<'a>(http: &'a Http, route: &'static Route, plurks: Vec<Value>) -> Events<'a> {
    let comments = http.option_bool("comments", false);
    crate::sites::pages(futures_util::StreamExt::then(
        futures_util::stream::iter(plurks),
        move |plurk| async move {
            let mut links = urls(&plurk);
            if comments {
                for comment in responses(http, &plurk).await? {
                    links.extend(urls(&comment));
                }
            }
            let data = plurk.as_object().cloned().unwrap_or_default();
            let children = links
                .into_iter()
                .filter_map(|link| Url::parse(&link).ok())
                .filter(|link| crate::sites::find(link).is_some())
                .map(|link| (link, data.clone()))
                .collect::<Vec<(Url, Map<String, Value>)>>();
            Ok(futures_util::StreamExt::boxed(
                futures_util::StreamExt::flatten(futures_util::stream::iter(
                    children.into_iter().map(move |(child, data)| {
                        crate::sites::queue(http, "plurk", route.subcategory, child, &data)
                    }),
                )),
            ))
        },
    ))
}

/// `_urls`: the `href` values in a plurk's or response's content.
fn urls(object: &Value) -> Vec<String> {
    let content = object
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    extract_iter(content, " href=\"", "\"")
        .into_iter()
        .map(str::to_owned)
        .collect()
}

/// `_comments`: all responses of a plurk, 200 per request.
async fn responses(http: &Http, plurk: &Value) -> Result<Vec<Value>> {
    let url = http.api(&format!("{ROOT}/Responses/get"))?;
    let id = plurk
        .get("id")
        .map(crate::sites::scalar)
        .unwrap_or_default();
    let headers = [
        ("Origin", ROOT),
        ("Referer", ROOT),
        ("X-Requested-With", "XMLHttpRequest"),
    ];
    let mut count = true;
    let mut from = None::<String>;
    let mut all = Vec::new();
    loop {
        let mut form = vec![("plurk_id", id.as_str())];
        if count {
            form.push(("count", "200"));
        }
        if let Some(from) = &from {
            form.push(("from_response_id", from.as_str()));
        }
        let info = http.post_form(&url, &form, &headers).await?;
        let responses = list(info.get("responses").cloned().unwrap_or(Value::Null));
        let last = responses
            .last()
            .and_then(|r| r.get("id"))
            .and_then(Value::as_i64);
        all.extend(responses);
        let newer = info.get("has_newer");
        if !crate::sites::art::truthy(newer) {
            return Ok(all);
        }
        // Python compares `True` as 1.
        if matches!(newer, Some(Value::Bool(true)))
            || newer.and_then(Value::as_f64).is_some_and(|n| n < 200.0)
        {
            count = false;
        }
        let Some(last) = last else {
            return Err(error("Plurk responses page is empty"));
        };
        from = Some((last + 1).to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::ExtractionEvent;
    use crate::sites::testing::{html, http, json, serve};
    use futures_util::StreamExt;
    use serde_json::json;

    #[test]
    fn classifies_urls() {
        let t = |raw: &str| target(&Url::parse(raw).unwrap());
        assert_eq!(
            t("https://www.plurk.com/plurkapi"),
            Some(Target::Timeline("plurkapi".into()))
        );
        assert_eq!(
            t("https://plurk.com/plurk_api/?x=1"),
            Some(Target::Timeline("plurk_api".into()))
        );
        assert_eq!(
            t("https://www.plurk.com/p/i701j1"),
            Some(Target::Post("i701j1".into()))
        );
        assert_eq!(
            t("https://www.plurk.com/p/i701j1-x"),
            Some(Target::Post("i701j1".into()))
        );
        for raw in [
            "https://www.plurk.com/",
            "https://www.plurk.com/p/",
            "https://www.plurk.com/user/extra",
            "https://www.plurk.com/us-er",
            "https://m.plurk.com/user",
            "https://www.plurk.com.evil.test/user",
            "https://evilplurk.com/user",
            "https://www.plurk.com:8443/user",
            "https://u@www.plurk.com/user",
        ] {
            assert_eq!(t(raw), None, "{raw}");
        }
    }

    #[test]
    fn helpers_match_upstream() {
        assert_eq!(
            load(Some(
                r#"[{"posted": new Date("Fri, 05 Mar 2021 12:34:56 GMT")}]"#
            ))
            .unwrap(),
            json!([{"posted": "Fri, 05 Mar 2021 12:34:56 GMT"}])
        );
        assert!(load(Some("")).is_err() && load(None).is_err());
        assert_eq!(
            offset("Fri, 05 Mar 2021 12:34:56 GMT").as_deref(),
            Some("2021-03-05T12:34:56.000Z")
        );
        assert_eq!(offset("garbage"), None);
    }

    /// Queued children as (subcategory, plurk id), canceled unextracted.
    async fn children(mut events: Events<'_>) -> Vec<(String, String)> {
        let mut out = Vec::new();
        while let Some(event) = events.next().await {
            if let ExtractionEvent::BeginChild(scope) = event.unwrap() {
                let meta = &scope.metadata;
                out.push((
                    meta.subcategory().to_owned(),
                    meta.fields()
                        .get("id")
                        .map(crate::sites::scalar)
                        .unwrap_or_default(),
                ));
                scope.control.cancel();
            }
        }
        out
    }

    #[tokio::test]
    async fn timeline_queues_links_and_paginates() {
        let page = r#"<script>GLOBAL = {"page_user": {"id": 42, "nick_name": "x"}};
var _PLURKS = [{"id": 1, "posted": new Date("Fri, 05 Mar 2021 12:34:56 GMT"), "content": "<a href=\"https://speakerdeck.com/u/deck\">x</a> <a href=\"https://unsupported.test/x\">y</a>"}];
</script>"#;
        let (root, server) = serve(vec![
            html(page),
            json(
                json!({"plurks": [{"id": 2, "posted": "Thu, 04 Mar 2021 00:00:00 GMT",
                "content": "<a href=\"https://speakerdeck.com/u/two\">z</a>"}]}),
            ),
            json(json!({"plurks": []})),
        ]);
        let http = http("plurk", root);
        let found = children(extract(
            &http,
            Url::parse("https://www.plurk.com/someone").unwrap(),
        ))
        .await;
        assert_eq!(
            found,
            vec![
                ("timeline".to_owned(), "1".to_owned()),
                ("timeline".to_owned(), "2".to_owned())
            ]
        );
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /someone "));
        let second = requests[1].to_ascii_lowercase();
        assert!(second.starts_with("post /timeline/getplurks "));
        assert!(second.contains("referer: https://www.plurk.com/someone"));
        assert!(second.contains("x-requested-with: xmlhttprequest"));
        assert!(requests[1].ends_with("user_id=42&offset=2021-03-05T12%3A34%3A56.000Z"));
        assert!(requests[2].ends_with("user_id=42&offset=2021-03-04T00%3A00%3A00.000Z"));
    }

    #[test]
    fn post_page_carries_user() {
        let page = "var GLOBAL={\"page_user\": {\"id\": 7}}\nvar plurk = {\"id\": 5, \"posted\": new Date(\"Fri, 05 Mar 2021 12:34:56 GMT\"), \"content\": \"\"};\n";
        let plurk = parse_post(page).unwrap();
        assert_eq!(plurk["user"], json!({"id": 7}));
        assert_eq!(plurk["posted"], "Fri, 05 Mar 2021 12:34:56 GMT");
    }

    #[tokio::test]
    async fn comments_are_paginated() {
        let (root, server) = serve(vec![
            json(
                json!({"responses": [{"id": 10, "content": "<a href=\"https://speakerdeck.com/a/b\">"}],
                "has_newer": 5}),
            ),
            json(json!({"responses": [{"id": 20, "content": ""}], "has_newer": 0})),
        ]);
        let http = http("plurk", root);
        let all = responses(&http, &json!({"id": 99})).await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(urls(&all[0]), vec!["https://speakerdeck.com/a/b"]);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("POST /Responses/get "));
        assert!(
            requests[0]
                .to_ascii_lowercase()
                .contains("origin: https://www.plurk.com")
        );
        assert!(requests[0].ends_with("plurk_id=99&count=200"));
        assert!(requests[1].ends_with("plurk_id=99&from_response_id=11"));
    }
}
