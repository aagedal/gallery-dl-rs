//! Cookie-scoped browser AJAX requests, separate from App API authorization.
use super::Pixiv;
use crate::{Result, cookies::CookieJar, error};
use serde_json::Value;
use std::{
    collections::BTreeSet,
    time::{SystemTime, UNIX_EPOCH},
};
use url::Url;

fn browser_agent() -> String {
    // gallery_dl.util.USERAGENT_FIREFOX derives the release from the UTC date.
    let ordinal = 719_163
        + SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            / 86_400;
    let version = (ordinal.saturating_sub(735_534)) / 28;
    format!(
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:{version}.0) Gecko/20100101 Firefox/{version}.0"
    )
}
fn unquote(raw: &str) -> String {
    let mut bytes = Vec::with_capacity(raw.len());
    let raw = raw.as_bytes();
    let mut index = 0;
    while index < raw.len() {
        if raw[index] == b'%'
            && let Some(hex) = raw.get(index + 1..index + 3)
            && hex.iter().all(u8::is_ascii_hexdigit)
            && let Ok(hex) = std::str::from_utf8(hex)
            && let Ok(value) = u8::from_str_radix(hex, 16)
        {
            bytes.push(value);
            index += 3;
        } else {
            bytes.push(raw[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}
/// Match the source's literal opening-anchor regex before HTML5 unescaping.
pub(super) fn sanitize_caption(value: &Value) -> Result<String> {
    if !super::value_truthy(value) {
        return Ok(String::new());
    }
    let caption = value
        .as_str()
        .ok_or_else(|| error("Pixiv returned an invalid AJAX caption"))?;
    const PREFIX: &str = "<a href=\"/jump.php?";
    let mut rest = caption;
    let mut output = String::with_capacity(caption.len());
    while let Some((before, tail)) = rest.split_once(PREFIX) {
        output.push_str(before);
        let length = tail.find('"').unwrap_or(tail.len());
        if length == 0 {
            output.push_str(PREFIX);
            rest = tail;
            continue;
        }
        output.push_str("<a href=\"");
        output.push_str(&unquote(&tail[..length]));
        rest = &tail[length..];
    }
    output.push_str(rest);
    Ok(super::pixivision::unescape(&output))
}
impl Pixiv {
    pub fn with_captions(mut self, enabled: bool) -> Self {
        self.captions = enabled;
        self
    }
    pub fn with_cookies(mut self, cookies: CookieJar) -> Self {
        self.web_cookies = tokio::sync::Mutex::new(cookies);
        self
    }
    pub fn with_web_root(mut self, root: Url) -> Self {
        self.web_root = root;
        self
    }
    pub(super) async fn request_ajax(&self, endpoint: &str) -> Option<Value> {
        tokio::time::timeout(self.timeout, self.ajax_request(endpoint))
            .await
            .ok()?
            .ok()
    }
    async fn ajax_request(&self, endpoint: &str) -> Result<Value> {
        for attempt in 0..=self.retries {
            match self.ajax_attempt(endpoint, attempt).await {
                Ok(value) => return Ok(value),
                Err(failure) if failure.retry && attempt < self.retries => {
                    tokio::time::sleep(super::backoff(attempt)).await
                }
                Err(failure) => return Err(failure.error),
            }
        }
        unreachable!()
    }
    async fn ajax_attempt(
        &self,
        endpoint: &str,
        attempt: u8,
    ) -> std::result::Result<Value, super::Failure> {
        let invalid = || error("Pixiv web AJAX request failed");
        if !endpoint.starts_with('/')
            || endpoint.starts_with("//")
            || endpoint.len() > 64 * 1024
            || endpoint.chars().any(char::is_control)
        {
            return Err(invalid().into());
        }
        let mut current = self
            .web_root
            .join(&format!("ajax{endpoint}"))
            .map_err(|_| invalid())?;
        let prefix = self.web_root.join("ajax/").map_err(|_| invalid())?;
        if current.origin() != self.web_root.origin()
            || !current.path().starts_with(prefix.path())
            || !current.username().is_empty()
            || current.password().is_some()
            || current.fragment().is_some()
        {
            return Err(invalid().into());
        }
        let origin = current.origin();
        let mut seen = BTreeSet::new();
        for _ in 0..=10 {
            if !seen.insert(current.to_string()) {
                return Err(invalid().into());
            }
            self.rate_limits
                .wait(&current, self.timeout)
                .await
                .map_err(|_| invalid())?;
            let mut request = self
                .client
                .get(current.clone())
                .timeout(self.request_timeout)
                .header(
                    "User-Agent",
                    self.network
                        .user_agent()
                        .map(str::to_owned)
                        .unwrap_or_else(browser_agent),
                )
                .header("Referer", self.web_root.as_str())
                .header("Accept", "*/*")
                .header("Accept-Language", "en-US,en;q=0.5")
                .header("Accept-Encoding", "gzip")
                .header("Origin", self.web_root.origin().ascii_serialization())
                .header("Sec-Fetch-Dest", "empty")
                .header("Sec-Fetch-Mode", "cors")
                .header("Sec-Fetch-Site", "same-site");
            if let Some(cookie) = self.web_cookies.lock().await.header(&current) {
                request = request.header("Cookie", cookie);
            }
            let mut response = request
                .send()
                .await
                .map_err(|_| super::Failure::transient("Pixiv web request failed"))?;
            self.web_cookies
                .lock()
                .await
                .store_response(&current, response.headers());
            let status = response.status();
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
                let delay = super::retry_after(response.headers(), SystemTime::now())
                    .unwrap_or_else(|| super::backoff(attempt));
                self.rate_limits.defer(&current, delay);
                return Err(super::Failure::transient("Pixiv web request failed"));
            }
            if matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308) {
                let mut locations = response.headers().get_all(reqwest::header::LOCATION).iter();
                if let Some(location) = locations.next() {
                    if locations.next().is_some() {
                        return Err(invalid().into());
                    }
                    let location = location.to_str().map_err(|_| invalid())?;
                    if !location.is_empty() {
                        if location.len() > 64 * 1024 || location.chars().any(char::is_control) {
                            return Err(invalid().into());
                        }
                        let mut next = current.join(location).map_err(|_| invalid())?;
                        if next.origin() != origin
                            || !next.username().is_empty()
                            || next.password().is_some()
                        {
                            return Err(invalid().into());
                        }
                        next.set_fragment(None);
                        current = next;
                        continue;
                    }
                }
            }
            let headers = response.headers().clone();
            let mut bytes = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| super::Failure::transient("could not read Pixiv web response"))?
            {
                if chunk.len() > super::MAX_RESPONSE.saturating_sub(bytes.len()) {
                    return Err(invalid().into());
                }
                bytes.extend_from_slice(&chunk);
            }
            let bytes = crate::transport::decode_api_body(bytes, &headers, super::MAX_RESPONSE)
                .await
                .map_err(|error| {
                    let retry = error.is::<crate::transport::ApiContentDecodeError>();
                    super::Failure { error, retry }
                })?;
            let mut data: Value = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
            if data.get("error").is_some_and(super::value_truthy) {
                return Err(invalid().into());
            }
            return data
                .get_mut("body")
                .map(Value::take)
                .ok_or_else(|| invalid().into());
        }
        Err(invalid().into())
    }
    pub(super) async fn enrich_caption(&self, work: &mut Value) -> Result<()> {
        if self.captions
            && !work.get("caption").is_some_and(super::value_truthy)
            && !work.get("_mypixiv").is_some_and(super::value_truthy)
            && !work.get("_ajax").is_some_and(super::value_truthy)
        {
            let id = super::id_of(work)
                .ok_or_else(|| error("Pixiv caption requires a valid illustration ID"))?;
            if let Some(body) = self
                .request_ajax(&format!("/illust/{id}"))
                .await
                .filter(super::value_truthy)
            {
                let raw = body
                    .get("illustComment")
                    .ok_or_else(|| error("Pixiv AJAX response contains no caption"))?;
                work["caption"] = Value::String(sanitize_caption(raw)?);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        event::ExtractionEvent,
        pixiv::tests::{pixiv, response, serve, token},
    };
    use futures_util::StreamExt;
    use serde_json::json;
    fn reply(value: &Value) -> Vec<u8> {
        response("200 OK", value.to_string().as_bytes())
    }
    fn fixture() -> Value {
        serde_json::from_str(include_str!("../tests/fixtures/pixiv/captions.json")).unwrap()
    }
    fn work(id: u64) -> Value {
        json!({"id":id,"user":{"id":5,"account":"artist"},"type":"illust","title":"Title","caption":"","total_comments":1,"is_bookmarked":true,"meta_single_page":{"original_image_url":format!("https://i.pximg.net/img-original/{id}_p0.jpg")},"meta_pages":[]})
    }
    #[test]
    fn sanitizer_matches_source_vectors_and_rejects_truthy_nonstrings() {
        for vector in fixture()["sanitizer_vectors"].as_array().unwrap() {
            assert_eq!(
                sanitize_caption(&vector["input"]).unwrap(),
                vector["output"].as_str().unwrap()
            );
        }
        for malformed in [json!(1), json!(true), json!([0]), json!({"x":0})] {
            assert!(sanitize_caption(&malformed).is_err());
        }
        assert_eq!(
            sanitize_caption(&json!("<a href=\"/jump.php?%+1%ZZ%2b+%FF\">link</a>")).unwrap(),
            "<a href=\"%+1%ZZ++�\">link</a>"
        );
        assert!(browser_agent().starts_with("Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:"));
    }
    #[tokio::test]
    async fn source_caption_cases_match_native_http_order_gates_and_metadata() {
        for case in fixture()["cases"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|case| {
                case["novel"] == false && case["name"] != "user_merge_supplies_caption_blocks_fetch"
            })
        {
            let mut data = case["input_work"].clone();
            let id = super::super::id_of(&data).unwrap();
            data["type"] = json!(if case["file_count"] == 0 {
                "ugoira"
            } else {
                "illust"
            });
            data["meta_single_page"] = json!({"original_image_url":format!("https://i.pximg.net/img-original/{id}_p0.jpg")});
            let count = case["file_count"].as_u64().unwrap() as usize;
            data["meta_pages"] = json!((0..if count>1 {count}else{0}).map(|page|json!({"image_urls":{"original":format!("https://i.pximg.net/img-original/{id}_p{page}.jpg")}})).collect::<Vec<_>>());
            let mut replies = vec![token("API_SECRET"), reply(&json!({"illust":data}))];
            let mut expected_paths = vec!["/auth/token", "/v1/illust/detail"];
            for call in case["calls"].as_array().unwrap() {
                match call["method"].as_str().unwrap() {
                    "user_detail" => {
                        let detail = if case["user_detail"].is_null() {
                            json!({"user":case["input_work"]["user"]})
                        } else {
                            case["user_detail"].clone()
                        };
                        replies.push(reply(&detail));
                        expected_paths.push("/v1/user/detail");
                    }
                    "illust_comments" => {
                        replies.push(reply(&json!({"comments":[{"id":7,"comment":"Fixture comment"}],"next_url":null})));
                        expected_paths.push("/v3/illust/comments");
                    }
                    "illust_bookmark_detail" => {
                        replies.push(reply(&json!({"bookmark_detail":{"tags":[{"name":"saved","is_registered":true}]}})));
                        expected_paths.push("/v2/illust/bookmark/detail");
                    }
                    "request_json" => {
                        replies.push(if case["request_fail"] == true {
                            response("404 Not Found", b"PRIVATE")
                        } else {
                            reply(&case["ajax_response"])
                        });
                        expected_paths.push("/ajax/illust/42");
                    }
                    "_extract_files" => {}
                    other => panic!("unknown source call {other}"),
                }
            }
            let (root, server) = serve(replies);
            let options = &case["options"];
            let native = pixiv(root.clone())
                .with_web_root(root.clone())
                .with_ugoira(false)
                .with_captions(
                    options
                        .get("captions")
                        .is_some_and(super::super::value_truthy),
                )
                .with_metadata(
                    options
                        .get("metadata")
                        .is_some_and(super::super::value_truthy),
                )
                .with_comments(
                    options
                        .get("comments")
                        .is_some_and(super::super::value_truthy),
                )
                .with_metadata_bookmark(
                    options
                        .get("metadata-bookmark")
                        .is_some_and(super::super::value_truthy),
                );
            let events = native
                .events(&Url::parse("https://www.pixiv.net/artworks/42").unwrap())
                .collect::<Vec<_>>()
                .await;
            if !case["error"].is_null() {
                assert_eq!(events.len(), 1, "{} {events:?}", case["name"]);
                assert!(events[0].is_err());
            } else {
                assert!(
                    events.iter().all(Result::is_ok),
                    "{} {events:?}",
                    case["name"]
                );
                let actual: Vec<_> = events
                    .iter()
                    .filter_map(|event| match event {
                        Ok(ExtractionEvent::Post(post)) => post.metadata.as_ref(),
                        Ok(ExtractionEvent::Media(media)) => media.metadata.as_ref(),
                        _ => None,
                    })
                    .collect();
                let expected = case["events"].as_array().unwrap();
                assert_eq!(actual.len(), expected.len(), "{}", case["name"]);
                for (actual, expected) in actual.into_iter().zip(expected) {
                    assert_eq!(
                        actual.caption_html.as_deref(),
                        expected["metadata"]["caption"].as_str(),
                        "{}",
                        case["name"]
                    );
                    if let Some(html) = actual
                        .caption_html
                        .as_deref()
                        .filter(|html| !html.is_empty())
                    {
                        assert!(
                            actual
                                .caption
                                .as_deref()
                                .unwrap()
                                .contains(&crate::mastodon::strip_html(html)),
                            "{}",
                            case["name"]
                        );
                    }
                }
            }
            let requests = server.join().unwrap();
            assert_eq!(requests.len(), expected_paths.len(), "{}", case["name"]);
            for (request, path) in requests.iter().zip(expected_paths) {
                assert!(
                    request.starts_with(&format!(
                        "{} {path}",
                        if path == "/auth/token" { "POST" } else { "GET" }
                    )),
                    "{} {request}",
                    case["name"]
                );
                if path.starts_with("/ajax/") {
                    let lower = request.to_ascii_lowercase();
                    for private in [
                        "authorization:",
                        "api_secret",
                        "app-os:",
                        "x-client-hash:",
                        "x-client-time:",
                    ] {
                        assert!(!lower.contains(private), "{lower}");
                    }
                    assert!(lower.contains("user-agent: mozilla/5.0"));
                    assert!(lower.contains(&format!("referer: {root}")));
                }
            }
        }
    }
    #[tokio::test]
    async fn scoped_cookies_rotate_on_retry_and_redirect_without_api_credentials() {
        let jar=CookieJar::parse("127.0.0.1\tFALSE\t/\tFALSE\t0\tPHPSESSID\tPRIVATE_INITIAL\nelsewhere.invalid\tFALSE\t/\tFALSE\t0\tother\tPRIVATE_OTHER").unwrap();
        let (root,server)=serve(vec![b"HTTP/1.1 503 Service Unavailable\r\nSet-Cookie: PHPSESSID=PRIVATE_RETRY; Path=/\r\nRetry-After: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),b"HTTP/1.1 302 Found\r\nLocation: /redirected\r\nSet-Cookie: PHPSESSID=PRIVATE_REDIRECT; Path=/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),reply(&json!({"body":{"illustComment":"<b>HTML</b>"}})),token("API_SECRET"),reply(&json!({"illust":work(42)}))]);
        let network = crate::network::NetworkOptions::new(
            Some(String::new()),
            Some("CustomBrowser/1".into()),
        )
        .unwrap();
        let native = pixiv(root.clone())
            .with_web_root(root.clone())
            .with_cookies(jar)
            .with_retries(1)
            .with_network(&network)
            .unwrap();
        assert!(native.request_ajax("/illust/42").await.is_some());
        let files = native
            .stream(&Url::parse("https://www.pixiv.net/artworks/42").unwrap())
            .collect::<Vec<_>>()
            .await;
        assert_eq!(files.len(), 1);
        assert!(files[0].is_ok());
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 5);
        for (index, cookie) in ["PRIVATE_INITIAL", "PRIVATE_RETRY", "PRIVATE_REDIRECT"]
            .into_iter()
            .enumerate()
        {
            assert!(requests[index].contains(cookie));
            let lower = requests[index].to_ascii_lowercase();
            assert!(lower.contains("user-agent: custombrowser/1"));
            assert!(!lower.contains("authorization:"));
            assert!(!lower.contains("app-os:"));
            assert!(!lower.contains("x-client-"));
            assert!(!requests[index].contains("PRIVATE_OTHER"));
        }
        assert!(requests[2].starts_with("GET /redirected "));
        for request in &requests[3..] {
            assert!(!request.to_ascii_lowercase().contains("cookie:"));
            assert!(!request.contains("PRIVATE_"));
        }
    }
    #[tokio::test]
    async fn ajax_prefix_redirect_origin_cycles_and_response_bounds_are_soft_failures() {
        for endpoint in [
            "https://elsewhere.invalid/",
            "//elsewhere.invalid/x",
            "/../auth/token",
            "/illust/42#fragment",
            "/illust/42\n",
        ] {
            let (root, server) = serve(vec![]);
            assert!(
                pixiv(root.clone())
                    .with_web_root(root)
                    .request_ajax(endpoint)
                    .await
                    .is_none()
            );
            assert!(server.join().unwrap().is_empty());
        }
        for location in [
            "http://elsewhere.invalid/private",
            "http://user:PRIVATE@127.0.0.1/",
            "/ajax/illust/42",
        ] {
            let (root,server)=serve(vec![format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").into_bytes()]);
            assert!(
                pixiv(root.clone())
                    .with_web_root(root)
                    .request_ajax("/illust/42")
                    .await
                    .is_none()
            );
            assert_eq!(server.join().unwrap().len(), 1);
        }
        let replies=(0..=10).map(|index|format!("HTTP/1.1 302 Found\r\nLocation: /redirect/{index}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").into_bytes()).collect();
        let (root, server) = serve(replies);
        assert!(
            pixiv(root.clone())
                .with_web_root(root)
                .request_ajax("/illust/42")
                .await
                .is_none()
        );
        assert_eq!(server.join().unwrap().len(), 11);
        for body in [
            b"not JSON".to_vec(),
            json!({"body":{"large":"x".repeat(super::super::MAX_RESPONSE)}})
                .to_string()
                .into_bytes(),
        ] {
            let (root, server) = serve(vec![response("200 OK", &body)]);
            assert!(
                pixiv(root.clone())
                    .with_web_root(root)
                    .request_ajax("/illust/42")
                    .await
                    .is_none()
            );
            assert_eq!(server.join().unwrap().len(), 1);
        }
    }
    #[tokio::test]
    async fn partial_body_transport_failure_retries_with_rotated_cookie_and_gzip_is_bounded() {
        use std::io::Write;
        let (root,server)=serve(vec![b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\nSet-Cookie: session=ROTATED; Path=/\r\nConnection: close\r\n\r\n{\"body\":".to_vec(),reply(&json!({"body":{"ok":true}}))]);
        assert_eq!(
            pixiv(root.clone())
                .with_web_root(root)
                .with_retries(1)
                .request_ajax("/illust/42")
                .await,
            Some(json!({"ok":true}))
        );
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].contains("session=ROTATED"));
        for oversized in [false, true] {
            let body = if oversized {
                json!({"body":{"large":"x".repeat(super::super::MAX_RESPONSE)}})
            } else {
                json!({"body":{"ok":"猫"}})
            };
            let mut gzip =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            gzip.write_all(body.to_string().as_bytes()).unwrap();
            let gzip = gzip.finish().unwrap();
            let mut raw=format!("HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",gzip.len()).into_bytes();
            raw.extend(gzip);
            let (root, server) = serve(vec![raw]);
            let result = pixiv(root.clone())
                .with_web_root(root)
                .request_ajax("/illust/42")
                .await;
            assert_eq!(result.is_none(), oversized);
            assert_eq!(server.join().unwrap().len(), 1);
        }
    }
    #[tokio::test]
    async fn malformed_truthy_ajax_body_aborts_all_feed_buffers_before_post_and_later_http() {
        for input in [
            "https://www.pixiv.net/users/9/illustrations",
            "https://www.pixiv.net/ranking.php?date=20240101",
            "https://www.pixiv.net/tags/cat",
            "https://www.pixiv.net/user/999/series/7",
            "https://www.pixiv.net/bookmark_new_illust.php",
            "https://www.pixivision.net/a/77",
            "https://www.pixiv.net/users/9/bookmarks/artworks",
        ] {
            let article = input.contains("pixivision");
            let owner = input.contains("bookmarks/artworks");
            let mut replies = Vec::new();
            if article {
                replies.push(response("200 OK",b"<title>Title</title><a href=\"https://www.pixiv.net/en/artworks/42\"><a href=\"https://www.pixiv.net/en/artworks/43\">"));
            }
            replies.push(reply(&json!({"access_token":"t","user":{"id":17}})));
            if owner {
                replies.push(crate::pixiv::tests::bookmark_owner_reply(9));
            }
            let mut body = if article {
                json!({"illust":work(42)})
            } else {
                json!({"illusts":[work(42),work(43)],"next_url":null})
            };
            if input.contains("/series/") {
                body["illust_series_detail"] = json!({"id":7,"series_work_count":2});
            }
            replies.push(reply(&body));
            replies.push(reply(&json!({"body":{"not_caption":"PRIVATE"}})));
            let (root, server) = serve(replies);
            let events = pixiv(root.clone())
                .with_web_root(root.clone())
                .with_pixivision_root(root)
                .with_captions(true)
                .events(&Url::parse(input).unwrap())
                .collect::<Vec<_>>()
                .await;
            assert_eq!(events.len(), 1, "{input}: {events:?}");
            assert!(events[0].is_err());
            assert!(
                !events[0]
                    .as_ref()
                    .unwrap_err()
                    .to_string()
                    .contains("PRIVATE")
            );
            assert_eq!(
                server.join().unwrap().len(),
                3 + usize::from(article || owner)
            );
        }
    }
    #[tokio::test]
    async fn post_range_budget_and_drop_avoid_later_work_caption_requests() {
        for budget in [false, true] {
            let (root, server) = serve(vec![
                token("t"),
                reply(
                    &json!({"illusts":[work(42),work(43)],"next_url":"https://unrequested.invalid/next"}),
                ),
                reply(&json!({"body":{"illustComment":"<b>First</b>"}})),
            ]);
            let native = pixiv(root.clone())
                .with_web_root(root)
                .with_captions(true)
                .with_max_posts(budget.then_some(1));
            let input = Url::parse("https://www.pixiv.net/users/9/illustrations").unwrap();
            if budget {
                let files = native
                    .stream(&input)
                    .collect::<Vec<_>>()
                    .await
                    .into_iter()
                    .collect::<Result<Vec<_>>>()
                    .unwrap();
                assert_eq!(files.len(), 1);
            } else {
                let files = crate::job_selection::JobSelection {
                    post_selection: Some(crate::post_selection::PostSelection::new(
                        Some(crate::selection::RangeSelection::parse("1").unwrap()),
                        None,
                    )),
                    ..Default::default()
                }
                .apply(native.events(&input))
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .collect::<Result<Vec<_>>>()
                .unwrap();
                assert_eq!(files.len(), 1);
            }
            assert_eq!(server.join().unwrap().len(), 3);
        }
        let (root, server) = serve(vec![
            token("t"),
            reply(&json!({"illusts":[work(42),work(43)],"next_url":null})),
            reply(&json!({"body":{"illustComment":"First"}})),
        ]);
        let native = pixiv(root.clone()).with_web_root(root).with_captions(true);
        let input = Url::parse("https://www.pixiv.net/users/9/illustrations").unwrap();
        let mut events = native.events(&input);
        assert!(matches!(
            events.next().await,
            Some(Ok(ExtractionEvent::Post(_)))
        ));
        drop(events);
        assert_eq!(server.join().unwrap().len(), 3);
    }
    #[tokio::test]
    async fn cancelling_caption_body_closes_connection_without_later_work_or_requests() {
        use std::time::Duration;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let root = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let (started, waiting) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            for response in [
                token("API_SECRET"),
                reply(&json!({"illusts":[work(42),work(43)],"next_url":null})),
            ] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = [0; 8192];
                assert_ne!(stream.read(&mut bytes).await.unwrap(), 0);
                stream.write_all(&response).await.unwrap();
            }
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = [0; 8192];
            let count = stream.read(&mut bytes).await.unwrap();
            let request = String::from_utf8_lossy(&bytes[..count]);
            assert!(request.starts_with("GET /ajax/illust/42 "));
            assert!(!request.contains("API_SECRET"));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n{\"body\":")
                .await
                .unwrap();
            started.send(()).unwrap();
            match tokio::time::timeout(Duration::from_secs(2), stream.read(&mut bytes))
                .await
                .unwrap()
            {
                Ok(0) => {}
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
                other => panic!("caption remained active: {other:?}"),
            };
            assert!(
                tokio::time::timeout(Duration::from_millis(100), listener.accept())
                    .await
                    .is_err()
            );
        });
        let extraction = tokio::spawn(async move {
            pixiv(root.clone())
                .with_web_root(root)
                .with_captions(true)
                .events(&Url::parse("https://www.pixiv.net/users/9/illustrations").unwrap())
                .collect::<Vec<_>>()
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), waiting)
            .await
            .unwrap()
            .unwrap();
        extraction.abort();
        assert!(extraction.await.unwrap_err().is_cancelled());
        server.await.unwrap();
    }
    #[tokio::test]
    async fn actual_source_http_status_vectors_keep_fatal_false_json_and_browser_headers() {
        for vector in fixture()["http_status_vectors"].as_array().unwrap() {
            let status = vector["status"].as_u64().unwrap();
            let raw = response(
                &format!("{status} Fixture"),
                vector["response_json"].to_string().as_bytes(),
            );
            let (root, server) = serve(vec![raw]);
            let result = pixiv(root.clone())
                .with_web_root(root.clone())
                .request_ajax("/illust/42")
                .await;
            assert_eq!(
                result,
                (!vector["body"].is_null()).then(|| vector["body"].clone()),
                "HTTP {status}"
            );
            let requests = server.join().unwrap();
            assert_eq!(requests.len(), 1);
            let lower = requests[0].to_ascii_lowercase();
            for (name, value) in [
                ("origin", root.origin().ascii_serialization()),
                ("referer", root.to_string()),
                ("sec-fetch-dest", "empty".into()),
                ("sec-fetch-mode", "cors".into()),
                ("sec-fetch-site", "same-site".into()),
                ("accept-language", "en-US,en;q=0.5".into()),
            ] {
                assert!(
                    lower.contains(&format!("{name}: {}", value.to_ascii_lowercase())),
                    "{lower}"
                );
            }
            assert!(!lower.contains("authorization:"));
            assert!(!lower.contains("app-os:"));
            assert!(!lower.contains("x-client-"));
        }
    }
    #[tokio::test]
    async fn corrupt_gzip_retries_with_response_cookie_but_size_failures_do_not_retry() {
        let (root,server)=serve(vec![b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nContent-Encoding: gzip\r\nSet-Cookie: session=DECODE_ROTATION; Path=/\r\nConnection: close\r\n\r\ncorrupt".to_vec(),reply(&json!({"body":{"ok":true}}))]);
        assert_eq!(
            pixiv(root.clone())
                .with_web_root(root)
                .with_retries(1)
                .request_ajax("/illust/42")
                .await,
            Some(json!({"ok":true}))
        );
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].contains("session=DECODE_ROTATION"));
        use std::io::Write;
        let body = json!({"body":{"large":"x".repeat(super::super::MAX_RESPONSE)}}).to_string();
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gzip.write_all(body.as_bytes()).unwrap();
        let gzip = gzip.finish().unwrap();
        let mut raw=format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Encoding: gzip\r\nConnection: close\r\n\r\n",gzip.len()).into_bytes();
        raw.extend(gzip);
        let (root, server) = serve(vec![raw]);
        assert!(
            pixiv(root.clone())
                .with_web_root(root)
                .with_retries(1)
                .request_ajax("/illust/42")
                .await
                .is_none()
        );
        assert_eq!(server.join().unwrap().len(), 1);
    }
}
