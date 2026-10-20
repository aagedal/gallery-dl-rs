//! Optional illustration comments and registered bookmark-tag enrichment.
use super::Pixiv;
use crate::{Result, error};
use serde_json::Value;
use std::collections::BTreeSet;
use url::Url;

#[derive(Debug)]
pub(super) enum WorkFailure {
    Files(crate::Error),
    Enrichment(crate::Error),
}
impl WorkFailure {
    pub(super) fn into_error(self) -> crate::Error {
        match self {
            Self::Files(error) | Self::Enrichment(error) => error,
        }
    }
}

const MAX_COMMENTS: usize = 100_000;
const MAX_BYTES: usize = 32 * 1024 * 1024;
const MAX_QUERY: usize = 64 * 1024;

impl Pixiv {
    pub fn with_comments(mut self, enabled: bool) -> Self {
        self.comments = enabled;
        self
    }
    pub fn with_metadata_bookmark(mut self, enabled: bool) -> Self {
        self.metadata_bookmark = enabled;
        self
    }

    fn comments_first(&self, id: &str) -> Url {
        let mut endpoint = self
            .api_root
            .join("v3/illust/comments")
            .expect("static comments endpoint");
        endpoint.query_pairs_mut().append_pair("illust_id", id);
        endpoint
    }
    fn comments_next(&self, id: &str, raw: &str) -> Result<Url> {
        let invalid = || error("Pixiv returned unexpected comment pagination");
        if raw.len() > MAX_QUERY {
            return Err(invalid());
        }
        // App API pagination keeps the endpoint and only forwards the last
        // question mark's query. Never request a response-supplied origin/path.
        let query = raw.rsplit_once('?').map_or(raw, |(_, query)| query);
        // Normalize source first-value semantics before binding the retained ID.
        let mut names = BTreeSet::new();
        let mut pairs = Vec::new();
        for part in query.split('&').filter(|part| part.contains('=')) {
            if let Some((key, value)) = url::form_urlencoded::parse(part.as_bytes()).next()
                && names.insert(key.to_string())
            {
                pairs.push((key.into_owned(), value.into_owned()));
            }
        }
        if !pairs
            .iter()
            .any(|(key, value)| key == "illust_id" && value == id)
        {
            return Err(invalid());
        }
        let mut endpoint = self.comments_first(id);
        endpoint.set_query(None);
        endpoint.query_pairs_mut().extend_pairs(pairs);
        Ok(endpoint)
    }
    async fn illust_comments(&self, id: &str) -> Result<Vec<Value>> {
        let mut endpoint = self.comments_first(id);
        let mut seen = BTreeSet::new();
        let mut output = Vec::new();
        let mut bytes = 2usize;
        for _ in 0..super::MAX_PAGES {
            let mut canonical = endpoint.clone();
            let mut pairs: Vec<_> = endpoint
                .query_pairs()
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect();
            pairs.sort();
            canonical.set_query(None);
            canonical.query_pairs_mut().extend_pairs(pairs);
            if !seen.insert(canonical.to_string()) {
                return Err(error("Pixiv comment pagination repeated a page"));
            }
            let mut body = super::json(&self.get(&endpoint).await?)?;
            let comments = body
                .get_mut("comments")
                .and_then(Value::as_array_mut)
                .ok_or_else(|| error("Pixiv response contains no comment list"))?;
            if output.len().saturating_add(comments.len()) > MAX_COMMENTS {
                return Err(error("Pixiv comments exceeded the count limit"));
            }
            for (index, comment) in std::mem::take(comments).into_iter().enumerate() {
                if index % 256 == 0 {
                    tokio::task::yield_now().await;
                }
                let size = serde_json::to_vec(&comment)
                    .map_err(|_| error("Pixiv returned invalid comments"))?
                    .len();
                bytes = bytes
                    .saturating_add(size)
                    .saturating_add(usize::from(!output.is_empty()));
                if bytes > MAX_BYTES {
                    return Err(error("Pixiv comments exceeded the size limit"));
                }
                output.push(comment);
            }
            let next = body
                .get("next_url")
                .ok_or_else(|| error("Pixiv comments contain no pagination state"))?;
            if !super::value_truthy(next) {
                return Ok(output);
            }
            endpoint = self.comments_next(
                id,
                next.as_str()
                    .ok_or_else(|| error("Pixiv returned invalid comment pagination"))?,
            )?;
        }
        Err(error("Pixiv comments exceeded the page limit"))
    }
    async fn bookmark_tags(&self, id: &str) -> Result<Vec<Value>> {
        let invalid = || error("Pixiv returned invalid registered bookmark tags");
        let mut endpoint = self
            .api_root
            .join("v2/illust/bookmark/detail")
            .expect("static bookmark detail endpoint");
        endpoint.query_pairs_mut().append_pair("illust_id", id);
        let mut body = super::json(&self.get(&endpoint).await?)?;
        let tags = body
            .get_mut("bookmark_detail")
            .and_then(|detail| detail.get_mut("tags"))
            .and_then(Value::as_array_mut)
            .ok_or_else(invalid)?;
        if tags.len() > MAX_COMMENTS {
            return Err(invalid());
        }
        let mut output = Vec::new();
        let mut bytes = 0usize;
        for (index, tag) in std::mem::take(tags).into_iter().enumerate() {
            if index % 256 == 0 {
                tokio::task::yield_now().await;
            }
            if super::value_truthy(tag.get("is_registered").ok_or_else(invalid)?) {
                let name = tag
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(invalid)?;
                bytes = bytes.saturating_add(name.len());
                if bytes > MAX_BYTES {
                    return Err(invalid());
                }
                output.push(Value::String(name.to_owned()));
            }
        }
        Ok(output)
    }
    pub(super) async fn enrich_work(&self, work: &mut Value) -> Result<()> {
        if !self.comments && !self.metadata_bookmark {
            return Ok(());
        }
        let id = super::id_of(work)
            .ok_or_else(|| error("Pixiv enrichment requires a valid illustration ID"))?;
        if self.comments {
            let comments = if work.get("total_comments").is_some_and(super::value_truthy)
                && !work.get("_ajax").is_some_and(super::value_truthy)
            {
                self.illust_comments(&id).await.unwrap_or_default()
            } else {
                Vec::new()
            };
            work["comments"] = Value::Array(comments);
        }
        if self.metadata_bookmark
            && super::value_truthy(
                work.get("is_bookmarked")
                    .ok_or_else(|| error("Pixiv illustration contains no bookmark state"))?,
            )
        {
            work["tags_bookmark"] = Value::Array(self.bookmark_tags(&id).await?);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        event::ExtractionEvent,
        job_selection::JobSelection,
        pixiv::tests::{bookmark_owner_reply, pixiv, response, serve, serve_with, token},
        selection::RangeSelection,
    };
    use futures_util::StreamExt;
    use serde_json::json;
    use std::sync::Arc;
    fn fixture() -> Value {
        serde_json::from_str(include_str!("../tests/fixtures/pixiv/enrichment.json")).unwrap()
    }
    fn work(id: u64) -> Value {
        json!({"id":id,"type":"illust","user":{"id":5,"account":"artist"},"tags":[{"name":"猫","translated_name":"cat"}],"total_comments":2,"is_bookmarked":true,"meta_single_page":{"original_image_url":format!("https://i.pximg.net/img-original/{id}_p0.jpg")},"meta_pages":[]})
    }
    fn reply(value: &Value) -> Vec<u8> {
        response("200 OK", value.to_string().as_bytes())
    }
    fn comments(values: Vec<Value>, next: Value) -> Vec<u8> {
        reply(&json!({"comments":values,"next_url":next}))
    }
    fn bookmark() -> Vec<u8> {
        reply(
            &json!({"bookmark_detail":{"tags":[{"name":"same","is_registered":true},{"name":"omit","is_registered":false},{"name":"same","is_registered":1},{"is_registered":false},{"name":" 猫 ","is_registered":"yes"}]}}),
        )
    }

    #[tokio::test]
    async fn recorded_source_option_gates_comments_failures_and_registered_tags_match_http() {
        for case in fixture()["cases"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|case| {
                case["file_fail"] != true && case["name"] != "malformed_tags_error_after_enrichment"
            })
        {
            let calls = case["calls"].as_array().unwrap();
            let get_comments = calls.iter().any(|call| call["method"] == "illust_comments");
            let get_bookmark = calls
                .iter()
                .any(|call| call["method"] == "illust_bookmark_detail");
            let mut data = case["input_work"].clone();
            let count = case["file_count"].as_u64().unwrap() as usize;
            data["meta_single_page"] = json!({"original_image_url":"https://i.pximg.net/img-original/img/2024/01/02/03/04/05/42_p0.jpg"});
            data["meta_pages"]=json!((0..if count>1{count}else{0}).map(|index|json!({"image_urls":{"original":format!("https://i.pximg.net/img-original/img/2024/01/02/03/04/05/42_p{index}.jpg")}})).collect::<Vec<_>>());
            if count == 0 {
                data["type"] = json!("ugoira");
            }
            if !case["file_ajax"].is_null() {
                data["_ajax"] = case["file_ajax"].clone();
            }
            let mut replies = vec![token("ACCESS_SECRET"), reply(&json!({"illust":data}))];
            let mut comment_calls = 0;
            if get_comments {
                let values = case["comments_script"]["values"].as_array().unwrap();
                if let Some(before) = case["comments_script"]["fail_before_index"].as_u64() {
                    if before > 0 {
                        replies.push(comments(
                            values.iter().take(before as usize).cloned().collect(),
                            json!("https://unrequested.invalid/ignored?illust_id=42&offset=30"),
                        ));
                        comment_calls += 1;
                    }
                    replies.push(response("404 Not Found", b"PRIVATE_COMMENT_ERROR"));
                    comment_calls += 1;
                } else {
                    replies.push(comments(values.clone(), Value::Null));
                    comment_calls += 1;
                }
            }
            if get_bookmark {
                replies.push(if case["bookmark_fail"] == true {
                    response("404 Not Found", b"PRIVATE_BOOKMARK_ERROR")
                } else {
                    reply(&json!({"bookmark_detail":case["bookmark_detail"]}))
                });
            }
            let (root, server) = serve(replies);
            let native = pixiv(root)
                .with_ugoira(false)
                .with_comments(
                    case["options"]
                        .get("comments")
                        .is_some_and(super::super::value_truthy),
                )
                .with_metadata_bookmark(
                    case["options"]
                        .get("metadata-bookmark")
                        .is_some_and(super::super::value_truthy),
                )
                .with_tags(super::super::TagMode::parse(
                    case["options"]["tags"].as_str().unwrap_or("japanese"),
                ));
            let events = native
                .events(&Url::parse("https://www.pixiv.net/artworks/42").unwrap())
                .collect::<Vec<_>>()
                .await;
            if !case["error"].is_null() {
                assert_eq!(events.len(), 1, "{}", case["name"]);
                assert!(events[0].is_err());
            } else {
                let mut source = case["events"].as_array().unwrap().iter();
                let mut current_comments = None;
                let mut current_tags = None;
                let mut ends = 0;
                for event in events {
                    match event.unwrap() {
                        ExtractionEvent::Post(post) => {
                            let expected = source.next().unwrap();
                            assert_eq!(expected["kind"], "post");
                            assert_eq!(post.count, Some(count));
                            let metadata = post.metadata.unwrap();
                            assert_eq!(
                                metadata
                                    .comments
                                    .as_ref()
                                    .map(|comments| serde_json::to_value(comments).unwrap()),
                                expected["metadata"].get("comments").cloned(),
                                "{}",
                                case["name"]
                            );
                            assert_eq!(
                                metadata
                                    .tags_bookmark
                                    .as_ref()
                                    .map(|tags| serde_json::to_value(tags).unwrap()),
                                expected["metadata"].get("tags_bookmark").cloned(),
                                "{}",
                                case["name"]
                            );
                            current_comments = metadata.comments;
                            current_tags = metadata.tags_bookmark;
                        }
                        ExtractionEvent::Media(file) => {
                            let expected = source.next().unwrap();
                            assert_eq!(expected["kind"], "media");
                            let metadata = file.metadata.as_ref().unwrap();
                            assert_eq!(
                                metadata
                                    .comments
                                    .as_ref()
                                    .map(|comments| serde_json::to_value(comments).unwrap()),
                                expected["metadata"].get("comments").cloned()
                            );
                            assert_eq!(
                                metadata
                                    .tags_bookmark
                                    .as_ref()
                                    .map(|tags| serde_json::to_value(tags).unwrap()),
                                expected["metadata"].get("tags_bookmark").cloned()
                            );
                            if let (Some(file), Some(post)) =
                                (&metadata.comments, &current_comments)
                            {
                                assert!(Arc::ptr_eq(file, post));
                            }
                            if let (Some(file), Some(post)) =
                                (&metadata.tags_bookmark, &current_tags)
                            {
                                assert!(Arc::ptr_eq(file, post));
                            }
                            assert_eq!(file.headers.len(), 1);
                            assert!(!serde_json::to_string(&file).unwrap().contains("PRIVATE_"));
                        }
                        ExtractionEvent::EndPost => ends += 1,
                        _ => panic!("unexpected child"),
                    }
                }
                assert!(source.next().is_none());
                assert_eq!(ends, 1);
            }
            let requests = server.join().unwrap();
            assert_eq!(
                requests.len(),
                2 + comment_calls + usize::from(get_bookmark),
                "{}",
                case["name"]
            );
            for request in requests.iter().skip(2) {
                assert!(
                    request
                        .to_ascii_lowercase()
                        .contains("authorization: bearer access_secret")
                );
                assert!(!request.contains("unrequested.invalid"));
            }
            for request in requests.iter().skip(2).take(comment_calls) {
                assert!(request.starts_with("GET /v3/illust/comments?illust_id=42"));
            }
            if get_bookmark {
                assert!(
                    requests
                        .last()
                        .unwrap()
                        .starts_with("GET /v2/illust/bookmark/detail?illust_id=42 ")
                );
            }
        }
    }

    #[tokio::test]
    async fn comments_rebuild_only_fixed_endpoint_query_with_first_values_and_discard_partial_on_failure()
     {
        for next in [
            "illust_id=42&offset=30",
            "https://unrequested.invalid/evil?illust_id=42&illust_id=other&offset=30",
            "?illust_id&illust_id=42&offset=30",
            "?illust_id=42&offset=30&offset=999",
        ] {
            let (root, server) = serve(vec![
                token("t"),
                reply(&json!({"illust":work(42)})),
                comments(
                    vec![json!({"id":1,"full":{"nested":[true,null,"raw <text>"]}})],
                    json!(next),
                ),
                comments(vec![json!({"id":2})], Value::Null),
                bookmark(),
            ]);
            let files = pixiv(root)
                .with_comments(true)
                .with_metadata_bookmark(true)
                .stream(&Url::parse("https://www.pixiv.net/artworks/42").unwrap())
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .collect::<Result<Vec<_>>>()
                .unwrap();
            assert_eq!(
                files[0]
                    .metadata
                    .as_ref()
                    .unwrap()
                    .comments
                    .as_ref()
                    .unwrap()
                    .len(),
                2
            );
            assert_eq!(
                files[0]
                    .metadata
                    .as_ref()
                    .unwrap()
                    .tags_bookmark
                    .as_ref()
                    .unwrap()
                    .as_ref(),
                ["same", "same", " 猫 "]
            );
            let requests = server.join().unwrap();
            assert_eq!(requests.len(), 5);
            assert!(requests[3].starts_with("GET /v3/illust/comments?illust_id=42&offset=30 "));
        }
        for failure in [
            response("404 Not Found", b"PRIVATE"),
            reply(&json!({"comments":[{"id":2}]})),
            comments(
                vec![json!({"id":2})],
                json!("?illust_id=other&illust_id=42&offset=60"),
            ),
            comments(vec![json!({"id":2})], json!("?illust_id=42")),
        ] {
            let (root, server) = serve(vec![
                token("t"),
                reply(&json!({"illust":work(42)})),
                comments(vec![json!({"id":1})], json!("?illust_id=42&offset=30")),
                failure,
                bookmark(),
            ]);
            let files = pixiv(root)
                .with_comments(true)
                .with_metadata_bookmark(true)
                .stream(&Url::parse("https://www.pixiv.net/artworks/42").unwrap())
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .collect::<Result<Vec<_>>>()
                .unwrap();
            let metadata = files[0].metadata.as_ref().unwrap();
            assert!(metadata.comments.as_ref().unwrap().is_empty());
            assert_eq!(metadata.tags_bookmark.as_ref().unwrap().len(), 3);
            assert_eq!(server.join().unwrap().len(), 5);
        }
    }

    #[tokio::test]
    async fn fatal_bookmark_errors_abort_all_feed_buffers_before_posts_and_later_work_http() {
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
                replies.push(response("200 OK",b"<title>Article</title><a href=\"https://www.pixiv.net/en/artworks/42\"><a href=\"https://www.pixiv.net/en/artworks/43\">"));
            }
            replies.push(reply(&json!({"access_token":"t","user":{"id":17}})));
            if owner {
                replies.push(bookmark_owner_reply(9));
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
            replies.push(response("404 Not Found", b"PRIVATE_BOOKMARK_ERROR"));
            let (root, server) = serve(replies);
            let native = pixiv(root.clone())
                .with_pixivision_root(root)
                .with_metadata_bookmark(true);
            let events = native
                .events(&Url::parse(input).unwrap())
                .collect::<Vec<_>>()
                .await;
            assert_eq!(events.len(), 1, "{input}");
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
    async fn empty_animations_are_enriched_but_file_errors_precede_enrichment_and_selection_stops_future_work()
     {
        let mut animation = work(42);
        animation["type"] = json!("ugoira");
        let (root, server) = serve(vec![
            token("t"),
            reply(&json!({"illust":animation})),
            comments(vec![json!({"id":1})], Value::Null),
            bookmark(),
        ]);
        let events = pixiv(root)
            .with_ugoira(false)
            .with_comments(true)
            .with_metadata_bookmark(true)
            .events(&Url::parse("https://www.pixiv.net/artworks/42").unwrap())
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(events.len(), 2);
        let ExtractionEvent::Post(post) = &events[0] else {
            panic!("expected empty post")
        };
        assert_eq!(post.count, Some(0));
        assert_eq!(
            post.metadata
                .as_ref()
                .unwrap()
                .comments
                .as_ref()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            post.metadata
                .as_ref()
                .unwrap()
                .tags_bookmark
                .as_ref()
                .unwrap()
                .len(),
            3
        );
        assert_eq!(server.join().unwrap().len(), 4);
        let mut invalid = work(42);
        invalid["meta_single_page"]["original_image_url"] =
            json!("https://unrequested.invalid/unsupported.jpg");
        let (root, server) = serve(vec![token("t"), reply(&json!({"illust":invalid}))]);
        let events = pixiv(root)
            .with_comments(true)
            .with_metadata_bookmark(true)
            .events(&Url::parse("https://www.pixiv.net/artworks/42").unwrap())
            .collect::<Vec<_>>()
            .await;
        assert_eq!(events.len(), 1);
        assert!(
            events[0]
                .as_ref()
                .unwrap_err()
                .to_string()
                .contains("no supported media")
        );
        assert_eq!(server.join().unwrap().len(), 2);
        let (root, server) = serve(vec![
            token("t"),
            reply(&json!({"illusts":[work(42),work(43)],"next_url":null})),
            comments(vec![], Value::Null),
            bookmark(),
        ]);
        let native = pixiv(root).with_comments(true).with_metadata_bookmark(true);
        let files = JobSelection {
            file_range: Some(RangeSelection::parse("1").unwrap()),
            ..Default::default()
        }
        .apply(native.events(&Url::parse("https://www.pixiv.net/users/9/illustrations").unwrap()))
        .collect::<Vec<_>>()
        .await;
        assert_eq!(files.len(), 1);
        assert!(files[0].is_ok());
        assert_eq!(server.join().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn comment_count_page_and_aggregate_limits_softly_discard_all_collected_data() {
        let (root, server) = serve_with(|root| {
            let mut replies = vec![token("t"), reply(&json!({"illust":work(42)}))];
            for page in 0..super::super::MAX_PAGES {
                replies.push(comments(
                    vec![json!({"id":page})],
                    json!(
                        root.join(&format!(
                            "v3/illust/comments?illust_id=42&offset={}",
                            page + 1
                        ))
                        .unwrap()
                    ),
                ));
            }
            replies
        });
        let files = pixiv(root)
            .with_comments(true)
            .stream(&Url::parse("https://www.pixiv.net/artworks/42").unwrap())
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert!(
            files[0]
                .metadata
                .as_ref()
                .unwrap()
                .comments
                .as_ref()
                .unwrap()
                .is_empty()
        );
        assert_eq!(server.join().unwrap().len(), 2 + super::super::MAX_PAGES);
        let (root, server) = serve(vec![
            token("t"),
            reply(&json!({"illust":work(42)})),
            comments(
                vec![Value::Null; MAX_COMMENTS],
                json!("?illust_id=42&offset=30"),
            ),
            comments(vec![Value::Null], Value::Null),
        ]);
        let files = pixiv(root)
            .with_comments(true)
            .stream(&Url::parse("https://www.pixiv.net/artworks/42").unwrap())
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert!(
            files[0]
                .metadata
                .as_ref()
                .unwrap()
                .comments
                .as_ref()
                .unwrap()
                .is_empty()
        );
        assert_eq!(server.join().unwrap().len(), 4);
        let large = json!({"comment":"x".repeat(17*1024*1024)});
        let (root, server) = serve(vec![
            token("t"),
            reply(&json!({"illust":work(42)})),
            comments(vec![large.clone()], json!("?illust_id=42&offset=30")),
            comments(vec![large], Value::Null),
        ]);
        let files = pixiv(root)
            .with_comments(true)
            .stream(&Url::parse("https://www.pixiv.net/artworks/42").unwrap())
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert!(
            files[0]
                .metadata
                .as_ref()
                .unwrap()
                .comments
                .as_ref()
                .unwrap()
                .is_empty()
        );
        assert_eq!(server.join().unwrap().len(), 4);
    }
    #[tokio::test]
    async fn cancelling_incomplete_comments_closes_http_without_bookmark_or_successor_requests() {
        use std::time::Duration;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let root = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let (started, waiting) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            for reply in [token("ACCESS_SECRET"), reply(&json!({"illust":work(42)}))] {
                let (mut connection, _) = listener.accept().await.unwrap();
                let mut buffer = [0; 8192];
                let count = connection.read(&mut buffer).await.unwrap();
                assert_ne!(count, 0);
                connection.write_all(&reply).await.unwrap();
            }
            let (mut connection, _) = listener.accept().await.unwrap();
            let mut buffer = [0; 8192];
            let count = connection.read(&mut buffer).await.unwrap();
            let request = String::from_utf8_lossy(&buffer[..count]);
            assert!(request.starts_with("GET /v3/illust/comments?illust_id=42 "));
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer access_secret")
            );
            connection
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n{\"comments\":[")
                .await
                .unwrap();
            started.send(()).unwrap();
            match tokio::time::timeout(Duration::from_secs(2), connection.read(&mut buffer))
                .await
                .unwrap()
            {
                Ok(0) => {}
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
                other => panic!("cancelled comment connection stayed active: {other:?}"),
            }
            assert!(
                tokio::time::timeout(Duration::from_millis(100), listener.accept())
                    .await
                    .is_err()
            );
        });
        let extraction = tokio::spawn(async move {
            pixiv(root)
                .with_comments(true)
                .with_metadata_bookmark(true)
                .events(&Url::parse("https://www.pixiv.net/artworks/42").unwrap())
                .collect::<Vec<_>>()
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), waiting)
            .await
            .unwrap()
            .unwrap();
        extraction.abort();
        assert!(extraction.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(3), server)
            .await
            .unwrap()
            .unwrap();
    }
}
