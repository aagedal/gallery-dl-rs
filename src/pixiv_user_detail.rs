//! Bounded public App API user-detail snapshots and an extraction-local cache.
use super::Pixiv;
use crate::{Result, error};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::Arc,
};

const MAX_BYTES: usize = 32 * 1024 * 1024;
const MAX_NODES: usize = 1_000_000;
const MAX_DEPTH: usize = 128;
const CACHE_BYTES: usize = 32 * 1024 * 1024;
const CACHE_ENTRIES: usize = 256;
pub(super) const WORK_HINT: &str = "__gallery_dl_pixiv_user_detail";

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct UserDetail(Map<String, Value>);
impl<'de> Deserialize<'de> for UserDetail {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        Self::parse(&value)
            .ok_or_else(|| serde::de::Error::custom("invalid or oversized Pixiv user detail"))
    }
}
impl UserDetail {
    pub fn fields(&self) -> &Map<String, Value> {
        &self.0
    }
    pub fn parse(value: &Value) -> Option<Self> {
        let fields = value.as_object()?;
        valid(fields).then(|| Self(fields.clone()))
    }
    /// Capture the API's four documented user-detail roots from merged metadata.
    pub fn parse_compat(fields: &Map<String, Value>) -> Option<Self> {
        let fields = fields
            .iter()
            .filter(|(key, _)| {
                matches!(
                    key.as_str(),
                    "user" | "profile" | "profile_publicity" | "workspace"
                )
            })
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        valid(&fields).then_some(Self(fields))
    }
    pub fn valid(&self) -> bool {
        valid(&self.0)
    }
    pub fn user_id(&self) -> Option<String> {
        super::id_of(self.0.get("user")?)
    }
}
fn valid(fields: &Map<String, Value>) -> bool {
    fn visit(value: &Value, depth: usize, nodes: &mut usize, bytes: &mut usize) -> bool {
        *nodes = nodes.saturating_add(1);
        if depth > MAX_DEPTH || *nodes > MAX_NODES {
            return false;
        }
        let mut text = |s: &str| {
            *bytes = bytes.saturating_add(s.len());
            s.len() <= MAX_BYTES && *bytes <= MAX_BYTES
        };
        match value {
            Value::String(s) => text(s),
            Value::Number(n) => {
                text(n.as_str())
                    && if n.as_str().contains(['.', 'e', 'E']) {
                        n.as_f64().is_some_and(f64::is_finite)
                    } else {
                        n.as_i128().is_some()
                    }
            }
            Value::Array(values) => values.iter().all(|v| visit(v, depth + 1, nodes, bytes)),
            Value::Object(values) => values.iter().all(|(k, v)| {
                *bytes = bytes.saturating_add(k.len());
                k.len() <= MAX_BYTES && *bytes <= MAX_BYTES && visit(v, depth + 1, nodes, bytes)
            }),
            _ => true,
        }
    }
    if fields.keys().any(|key| {
        matches!(
            key.as_str(),
            "id" | "post_id"
                | "media_id"
                | "shortcode"
                | "username"
                | "caption"
                | "timestamp"
                | "index"
                | "tags"
                | "comments"
                | "tags_bookmark"
                | "frames"
                | "file"
                | "profile_asset"
                | "ranking"
                | "search"
                | "series"
                | "num_series"
                | "user_follow"
                | "user_bookmark"
                | "pixivision"
                | "sketch"
                | "ugoira_original"
                | "ugoira_frame_index"
                | "type"
                | "category"
                | "subcategory"
                | "rating"
                | "score"
                | "community"
                | "discord"
                | "creator"
                | "revision"
                | "retweet_id"
                | "quoted_by_id"
                | "reposted_by_id"
                | "article"
                | "instance"
                | "url"
                | "filename"
                | "name"
                | "extension"
                | "date"
                | "num"
                | "count"
                | "suffix"
                | "hash"
                | "date_url"
                | "width"
                | "height"
                | "width_original"
                | "height_original"
                | "duration"
                | "bitrate"
                | "filesize"
                | "file_size"
                | "media_type"
                | "description"
                | "sensitive_flags"
                | "source_id"
                | "source_username"
                | "exts_image"
                | "exts_video"
                | "exts_archive"
        ) || key.starts_with('_') && !matches!(key.as_str(), "_ajax" | "_mypixiv")
    }) {
        return false;
    }
    let Some(user) = fields.get("user") else {
        return false;
    };
    if super::id_of(user).is_none() {
        return false;
    }
    let mut nodes = 0usize;
    let mut bytes = 0usize;
    fields.iter().all(|(k, v)| {
        bytes = bytes.saturating_add(k.len());
        k.len() <= MAX_BYTES && bytes <= MAX_BYTES && visit(v, 0, &mut nodes, &mut bytes)
    }) && serde_json::to_vec(fields).is_ok_and(|raw| raw.len() <= MAX_BYTES)
}
#[derive(Default)]
pub(super) struct UserDetailCache {
    entries: BTreeMap<String, (Arc<UserDetail>, usize)>,
    order: VecDeque<String>,
    bytes: usize,
}
impl Pixiv {
    pub fn with_metadata(mut self, enabled: bool) -> Self {
        self.metadata = enabled;
        self
    }
    pub(super) async fn user_detail(&self, id: &str) -> Result<Arc<UserDetail>> {
        // Serializing cache misses avoids duplicate lookups; cancellation drops
        // the guard and publishes no incomplete response or cached failure.
        let mut cache = self.user_detail_cache.lock().await;
        if let Some((detail, _)) = cache.entries.get(id) {
            return Ok(detail.clone());
        }
        let mut endpoint = self
            .api_root
            .join("v1/user/detail")
            .expect("static endpoint");
        endpoint.query_pairs_mut().append_pair("user_id", id);
        let value = super::json(&self.get(&endpoint).await?)?;
        let detail = UserDetail::parse(&value)
            .filter(|detail| detail.user_id().as_deref() == Some(id))
            .ok_or_else(|| error("Pixiv returned invalid user detail"))?;
        let bytes = serde_json::to_vec(detail.fields())
            .map_err(|_| error("Pixiv returned invalid user detail"))?
            .len();
        while cache.entries.len() >= CACHE_ENTRIES
            || cache.bytes.saturating_add(bytes) > CACHE_BYTES
        {
            let Some(old) = cache.order.pop_front() else {
                break;
            };
            if let Some((_, size)) = cache.entries.remove(&old) {
                cache.bytes = cache.bytes.saturating_sub(size);
            }
        }
        let detail = Arc::new(detail);
        cache.bytes += bytes;
        cache.order.push_back(id.to_owned());
        cache.entries.insert(id.to_owned(), (detail.clone(), bytes));
        Ok(detail)
    }
    pub(super) async fn enrich_user_work(
        &self,
        work: &mut Value,
    ) -> Result<Option<Arc<UserDetail>>> {
        if !self.metadata {
            return Ok(None);
        }
        let id = work
            .get("user")
            .and_then(super::id_of)
            .ok_or_else(|| error("Pixiv work contains no valid user ID"))?;
        let detail = self.user_detail(&id).await?;
        let fields = work
            .as_object_mut()
            .ok_or_else(|| error("Pixiv returned an invalid work"))?;
        fields.extend(detail.fields().clone());
        // Empty-media Posts are constructed after the work pipeline returns.
        // Keep their exact response snapshot independent of cache eviction.
        fields.insert(WORK_HINT.into(), Value::Object(detail.fields().clone()));
        Ok(Some(detail))
    }
    pub(super) fn work_user_detail(&self, work: &Value) -> Option<Arc<UserDetail>> {
        work.get(WORK_HINT)
            .and_then(UserDetail::parse)
            .or_else(|| work.as_object().and_then(UserDetail::parse_compat))
            .map(Arc::new)
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
    use url::Url;
    fn reply(value: &Value) -> Vec<u8> {
        response("200 OK", value.to_string().as_bytes())
    }
    fn detail(id: u64) -> Value {
        json!({"user":{"id":id,"account":"enriched","name":"猫","extra":{"nested":[1,false,"raw"]}},"profile":{"comment":"<b>raw</b>","date":"raw"},"profile_publicity":{"gender":"public"},"workspace":{"pc":"Linux"},"extra_root":{"data":[null,"value"]}})
    }
    fn work(id: u64) -> Value {
        json!({"id":id,"type":"illust","user":{"id":5,"account":"original","source_extra":true},"title":"Title","total_comments":1,"is_bookmarked":true,"meta_single_page":{"original_image_url":format!("https://i.pximg.net/img-original/{id}_p0.jpg")},"meta_pages":[]})
    }
    #[test]
    fn user_detail_preserves_order_values_and_bounds_deserialization_and_collisions() {
        let raw = detail(5);
        let parsed = UserDetail::parse(&raw).unwrap();
        assert_eq!(serde_json::to_value(&parsed).unwrap(), raw);
        assert_eq!(
            parsed.fields().keys().collect::<Vec<_>>(),
            raw.as_object().unwrap().keys().collect::<Vec<_>>()
        );
        assert_eq!(
            serde_json::from_value::<UserDetail>(raw.clone()).unwrap(),
            parsed
        );
        let baseline = UserDetail::parse_compat(raw.as_object().unwrap()).unwrap();
        assert!(!baseline.fields().contains_key("extra_root"));
        let mut invalid = raw.clone();
        invalid["post_id"] = json!("other");
        assert!(UserDetail::parse(&invalid).is_none());
        assert!(serde_json::from_value::<UserDetail>(invalid).is_err());
        for value in [json!(null), json!({"user":{"id":0}}), json!({"profile":{}})] {
            assert!(UserDetail::parse(&value).is_none());
        }
        let mut large = raw.clone();
        large["profile"]["large"] = json!("x".repeat(128 * 1024));
        assert!(UserDetail::parse(&large).is_some());
        large["profile"]["large"] = json!("x".repeat(MAX_BYTES));
        assert!(UserDetail::parse(&large).is_none());
    }
    #[tokio::test]
    async fn files_then_user_detail_then_comments_bookmark_and_cached_snapshots_are_shared() {
        let (root, server) = serve(vec![
            token("SECRET"),
            reply(&detail(5)),
            reply(&json!({"comments":[{"id":1}],"next_url":null})),
            reply(&json!({"bookmark_detail":{"tags":[{"name":"a","is_registered":true}]}})),
        ]);
        let native = pixiv(root)
            .with_metadata(true)
            .with_comments(true)
            .with_metadata_bookmark(true);
        let mut data = work(42);
        data["meta_pages"] = json!([{"image_urls":{"original":"https://i.pximg.net/img-original/42_p0.jpg"}},{"image_urls":{"original":"https://i.pximg.net/img-original/42_p1.jpg"}}]);
        let files = native.work(&mut data, true).await.unwrap();
        let left = files[0].metadata.as_ref().unwrap();
        let right = files[1].metadata.as_ref().unwrap();
        assert_eq!(left.username.as_deref(), Some("enriched"));
        assert!(Arc::ptr_eq(
            left.pixiv_user_detail.as_ref().unwrap(),
            right.pixiv_user_detail.as_ref().unwrap()
        ));
        assert_eq!(
            left.pixiv_user_detail.as_ref().unwrap().fields(),
            detail(5).as_object().unwrap()
        );
        assert_eq!(data["user"]["extra"]["nested"], json!([1, false, "raw"]));
        let cached = native.user_detail("5").await.unwrap();
        assert!(Arc::ptr_eq(
            &cached,
            left.pixiv_user_detail.as_ref().unwrap()
        ));
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 4);
        assert!(requests[1].starts_with("GET /v1/user/detail?user_id=5 "));
        assert!(requests[2].starts_with("GET /v3/illust/comments?illust_id=42 "));
        assert!(requests[3].starts_with("GET /v2/illust/bookmark/detail?illust_id=42 "));
        // File errors precede all optional metadata work.
        let (root, server) = serve(vec![]);
        let mut bad = work(42);
        bad["meta_single_page"] = json!({"original_image_url":"https://elsewhere.invalid/42.jpg"});
        assert!(
            pixiv(root)
                .with_metadata(true)
                .work(&mut bad, true)
                .await
                .is_err()
        );
        assert!(server.join().unwrap().is_empty());
    }
    #[tokio::test]
    async fn listing_preloads_owner_once_and_source_cap_prevents_later_artist_detail() {
        let (root, server) = serve(vec![
            token("t"),
            reply(&detail(9)),
            reply(&json!({"illusts":[work(42),work(43)],"next_url":null})),
            reply(&detail(5)),
        ]);
        let native = pixiv(root).with_metadata(true).with_max_posts(Some(1));
        let files = native
            .stream(&Url::parse("https://www.pixiv.net/users/9/illustrations").unwrap())
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0]
                .metadata
                .as_ref()
                .unwrap()
                .pixiv_user_detail
                .as_ref()
                .unwrap()
                .user_id()
                .as_deref(),
            Some("5")
        );
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 4);
        assert!(requests[1].starts_with("GET /v1/user/detail?user_id=9 "));
        assert!(requests[2].starts_with("GET /v1/user/illusts?user_id=9&type=illust "));
        assert!(requests[3].starts_with("GET /v1/user/detail?user_id=5 "));
    }
    #[tokio::test]
    async fn detail_failures_abort_all_buffered_feeds_before_post_and_comments() {
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
            let listing = input.contains("/illustrations");
            let owner = input.contains("bookmarks/artworks");
            let mut replies = Vec::new();
            if article {
                replies.push(response("200 OK",b"<title>Title</title><a href=\"https://www.pixiv.net/en/artworks/42\"><a href=\"https://www.pixiv.net/en/artworks/43\">"));
            }
            replies.push(reply(&json!({"access_token":"t","user":{"id":17}})));
            if listing || owner {
                replies.push(reply(&detail(9)));
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
            replies.push(response("404 Not Found", b"PRIVATE"));
            let (root, server) = serve(replies);
            let events = pixiv(root.clone())
                .with_pixivision_root(root)
                .with_metadata(true)
                .with_comments(true)
                .with_metadata_bookmark(true)
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
            let requests = server.join().unwrap();
            assert_eq!(
                requests.len(),
                3 + usize::from(article || listing || owner),
                "{input}"
            );
            assert!(
                requests
                    .iter()
                    .all(|r| !r.contains("comments") && !r.contains("bookmark/detail"))
            );
        }
    }
    #[tokio::test]
    async fn empty_animation_keeps_snapshot_and_metadata_disabled_keeps_complete_basic_user() {
        let mut animation = work(42);
        animation["type"] = json!("ugoira");
        let (root, server) = serve(vec![
            token("t"),
            reply(&json!({"illust":animation})),
            reply(&detail(5)),
        ]);
        let events = pixiv(root)
            .with_metadata(true)
            .with_ugoira(false)
            .events(&Url::parse("https://www.pixiv.net/artworks/42").unwrap())
            .collect::<Vec<_>>()
            .await;
        let Some(Ok(ExtractionEvent::Post(post))) = events.first() else {
            panic!("{events:?}")
        };
        assert_eq!(post.count, Some(0));
        assert_eq!(
            post.metadata
                .as_ref()
                .unwrap()
                .pixiv_user_detail
                .as_ref()
                .unwrap()
                .fields(),
            detail(5).as_object().unwrap()
        );
        assert_eq!(server.join().unwrap().len(), 3);
        let (root, server) = serve(vec![]);
        let mut basic = work(42);
        let files = pixiv(root).work(&mut basic, true).await.unwrap();
        assert_eq!(
            files[0]
                .metadata
                .as_ref()
                .unwrap()
                .pixiv_user_detail
                .as_ref()
                .unwrap()
                .fields()["user"],
            basic["user"]
        );
        assert!(server.join().unwrap().is_empty());
    }
    #[tokio::test]
    async fn failed_details_are_not_cached_and_wrong_requested_identity_is_rejected() {
        let (root, server) = serve(vec![
            token("t"),
            response("404 Not Found", b"PRIVATE"),
            reply(&detail(99)),
            reply(&detail(5)),
        ]);
        let native = pixiv(root);
        assert!(native.user_detail("5").await.is_err());
        assert!(native.user_detail("5").await.is_err());
        let valid = native.user_detail("5").await.unwrap();
        assert!(Arc::ptr_eq(&valid, &native.user_detail("5").await.unwrap()));
        assert_eq!(server.join().unwrap().len(), 4);
    }
    #[tokio::test]
    async fn cancelled_detail_miss_releases_cache_and_does_not_publish_partial_response() {
        use std::time::Duration;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let root = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let (started, waiting) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut connection, _) = listener.accept().await.unwrap();
            let mut buffer = [0; 8192];
            assert_ne!(connection.read(&mut buffer).await.unwrap(), 0);
            connection.write_all(&token("SECRET")).await.unwrap();
            let (mut connection, _) = listener.accept().await.unwrap();
            let count = connection.read(&mut buffer).await.unwrap();
            assert!(
                String::from_utf8_lossy(&buffer[..count])
                    .starts_with("GET /v1/user/detail?user_id=5 ")
            );
            connection
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n{\"user\":")
                .await
                .unwrap();
            started.send(()).unwrap();
            match tokio::time::timeout(Duration::from_secs(2), connection.read(&mut buffer))
                .await
                .unwrap()
            {
                Ok(0) => {}
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
                other => panic!("cancelled detail remained active: {other:?}"),
            }
            let (mut connection, _) =
                tokio::time::timeout(Duration::from_secs(2), listener.accept())
                    .await
                    .unwrap()
                    .unwrap();
            let count = connection.read(&mut buffer).await.unwrap();
            assert!(
                String::from_utf8_lossy(&buffer[..count])
                    .starts_with("GET /v1/user/detail?user_id=5 ")
            );
            connection.write_all(&reply(&detail(5))).await.unwrap();
            assert!(
                tokio::time::timeout(Duration::from_millis(100), listener.accept())
                    .await
                    .is_err()
            );
        });
        let native = Arc::new(pixiv(root));
        let task_native = native.clone();
        let task = tokio::spawn(async move { task_native.user_detail("5").await });
        tokio::time::timeout(Duration::from_secs(2), waiting)
            .await
            .unwrap()
            .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(native.user_detail_cache.lock().await.entries.is_empty());
        let detail = tokio::time::timeout(Duration::from_secs(2), native.user_detail("5"))
            .await
            .unwrap()
            .unwrap();
        assert!(Arc::ptr_eq(
            &detail,
            &native.user_detail("5").await.unwrap()
        ));
        server.await.unwrap();
    }
    #[tokio::test]
    async fn recorded_source_illustration_user_detail_cases_match_http_order_and_public_snapshots()
    {
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/pixiv/user_detail.json")).unwrap();
        for case in fixture["cases"].as_array().unwrap().iter().filter(|case| {
            case["kind"] == "illustration"
                && case["name"] != "merge_canonical_collision_source_behavior"
        }) {
            let mut works = case["input_works"].as_array().unwrap().clone();
            for work in &mut works {
                let id = super::super::id_of(work).unwrap();
                work["type"] = json!(if case["empty_files"] == true {
                    "ugoira"
                } else {
                    "illust"
                });
                work["meta_single_page"] = json!({"original_image_url":format!("https://i.pximg.net/img-original/{id}_p0.jpg")});
                work["meta_pages"] = json!([]);
            }
            let expected_events = case["events"].as_array().unwrap();
            let expected_meta = expected_events.first().map(|event| &event["metadata"]);
            let mut replies = vec![token("SECRET")];
            let mut expected_paths = Vec::new();
            for call in case["calls"].as_array().unwrap() {
                match call["method"].as_str().unwrap() {
                    "works" => {
                        replies.push(reply(&json!({"illusts":works,"next_url":null})));
                        expected_paths.push(if case["bookmark_owner"] == true {
                            "/v1/user/bookmarks/illust"
                        } else if case["preload"] == true {
                            "/v1/user/illusts"
                        } else {
                            "/v1/illust/ranking"
                        });
                    }
                    "_call" => {
                        if case["detail_fail"] == true {
                            replies.push(response("404 Not Found", b"PRIVATE"));
                        } else {
                            let mut detail = case["user_detail"].clone();
                            let id = call["params"]["user_id"]
                                .as_str()
                                .unwrap()
                                .parse::<u64>()
                                .unwrap();
                            detail["user"]["id"] = json!(id);
                            if case["options"]
                                .get("metadata")
                                .is_some_and(super::super::value_truthy)
                                && let Some(expected) = expected_events
                                    .iter()
                                    .find(|event| event["metadata"]["user"]["id"] == json!(id))
                            {
                                detail["user"] = expected["metadata"]["user"].clone();
                            }
                            replies.push(reply(&detail));
                        }
                        expected_paths.push("/v1/user/detail");
                    }
                    "illust_comments" => {
                        replies.push(reply(
                            &json!({"comments":expected_meta.unwrap()["comments"],"next_url":null}),
                        ));
                        expected_paths.push("/v3/illust/comments");
                    }
                    "illust_bookmark_detail" => {
                        let tags: Vec<_> = expected_meta.unwrap()["tags_bookmark"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|name| json!({"name":name,"is_registered":true}))
                            .collect();
                        replies.push(reply(&json!({"bookmark_detail":{"tags":tags}})));
                        expected_paths.push("/v2/illust/bookmark/detail");
                    }
                    "_extract_files" => {}
                    other => panic!("unexpected source call: {other}"),
                }
            }
            let (root, server) = serve(replies);
            let native = pixiv(root)
                .with_metadata(
                    case["options"]
                        .get("metadata")
                        .is_some_and(super::super::value_truthy),
                )
                .with_ugoira(false)
                .with_comments(case["options"]["comments"] == true)
                .with_metadata_bookmark(case["options"]["metadata-bookmark"] == true);
            let input = if case["bookmark_owner"] == true {
                "https://www.pixiv.net/users/5/bookmarks/artworks"
            } else if case["preload"] == true {
                "https://www.pixiv.net/users/5/illustrations"
            } else {
                "https://www.pixiv.net/ranking.php?date=20240101"
            };
            let events = native
                .events(&Url::parse(input).unwrap())
                .collect::<Vec<_>>()
                .await;
            if !case["error"].is_null() {
                assert_eq!(events.len(), 1, "{}: {events:?}", case["name"]);
                assert!(events[0].is_err());
            } else {
                assert!(
                    events.iter().all(Result::is_ok),
                    "{}: {events:?}",
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
                assert_eq!(actual.len(), expected_events.len(), "{}", case["name"]);
                for (actual, expected) in actual.into_iter().zip(expected_events) {
                    let snapshot = actual.pixiv_user_detail.as_ref().unwrap();
                    for (key, value) in snapshot.fields() {
                        assert_eq!(
                            value, &expected["metadata"][key],
                            "{} field {key}",
                            case["name"]
                        );
                    }
                    assert_eq!(
                        actual
                            .comments
                            .as_deref()
                            .map(|values| Value::Array(values.to_vec())),
                        expected["metadata"].get("comments").cloned(),
                        "{}",
                        case["name"]
                    );
                    assert_eq!(
                        actual.tags_bookmark.as_deref().map(|values| json!(values)),
                        expected["metadata"].get("tags_bookmark").cloned(),
                        "{}",
                        case["name"]
                    );
                }
            }
            let requests = server.join().unwrap();
            assert_eq!(requests.len(), expected_paths.len() + 1, "{}", case["name"]);
            for (request, expected_path) in requests[1..].iter().zip(expected_paths) {
                assert!(
                    request.starts_with(&format!("GET {expected_path}?")),
                    "{}: {request}",
                    case["name"]
                );
            }
        }
    }
    #[tokio::test]
    async fn avatar_and_background_share_owner_cache_and_only_enabled_metadata_exposes_profile_fields()
     {
        for enabled in [false, true] {
            let mut raw = detail(9);
            raw["user"]["profile_image_urls"] = json!({"medium":"https://i.pximg.net/user-profile/img/2024/01/02/03/04/05/9_170.jpg"});
            raw["profile"]["background_image_url"] =
                json!("https://i.pximg.net/background/img/2024/01/02/03/04/05/9.jpg");
            let (root, server) = serve(vec![token("t"), reply(&raw)]);
            let native = pixiv(root).with_metadata(enabled).with_include(Some(
                crate::dispatch::Include::Views(vec!["avatar".into(), "background".into()]),
            ));
            let files = crate::job_selection::JobSelection::default()
                .apply(native.events(&Url::parse("https://www.pixiv.net/users/9").unwrap()))
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .collect::<Result<Vec<_>>>()
                .unwrap();
            assert_eq!(files.len(), 2);
            let left = files[0]
                .metadata
                .as_ref()
                .unwrap()
                .pixiv_user_detail
                .as_ref()
                .unwrap();
            let right = files[1]
                .metadata
                .as_ref()
                .unwrap()
                .pixiv_user_detail
                .as_ref()
                .unwrap();
            assert_eq!(left.fields().get("profile").is_some(), enabled);
            assert_eq!(left.fields()["user"], raw["user"]);
            assert_eq!(right.fields()["user"], raw["user"]);
            if enabled {
                assert!(Arc::ptr_eq(left, right));
                assert_eq!(left.fields(), raw.as_object().unwrap());
            }
            assert_eq!(server.join().unwrap().len(), 2);
        }
    }
    #[tokio::test]
    async fn bounded_cache_evicts_old_entries_without_changing_active_snapshots() {
        let mut replies = vec![token("t")];
        for id in 1..=CACHE_ENTRIES + 1 {
            replies.push(reply(&detail(id as u64)));
        }
        replies.push(reply(&detail(1)));
        let (root, server) = serve(replies);
        let native = pixiv(root);
        let first = native.user_detail("1").await.unwrap();
        for id in 2..=CACHE_ENTRIES + 1 {
            native.user_detail(&id.to_string()).await.unwrap();
        }
        {
            let cache = native.user_detail_cache.lock().await;
            assert_eq!(cache.entries.len(), CACHE_ENTRIES);
            assert!(cache.bytes <= CACHE_BYTES);
            assert!(!cache.entries.contains_key("1"));
        }
        assert_eq!(first.fields(), detail(1).as_object().unwrap());
        let refreshed = native.user_detail("1").await.unwrap();
        assert_eq!(first, refreshed);
        assert!(!Arc::ptr_eq(&first, &refreshed));
        assert_eq!(server.join().unwrap().len(), CACHE_ENTRIES + 3);
    }
    #[tokio::test]
    async fn attached_novel_client_shares_same_api_cache_but_root_changes_isolate_it() {
        let novel = json!({"id":99,"user":{"id":5,"account":"basic"},"title":"Novel","series":{}});
        let (root, server) = serve(vec![
            token("ART"),
            reply(&detail(9)),
            reply(&json!({"illusts":[work(42)],"next_url":null})),
            reply(&detail(5)),
            token("NOVEL"),
            reply(&json!({"novels":[novel],"next_url":null})),
            response("200 OK", b"<script>novel: {\"text\":\"raw\"},\n</script>"),
        ]);
        let novel = Arc::new(pixiv(root.clone()).with_metadata(true));
        let native = pixiv(root)
            .with_metadata(true)
            .with_max_posts(Some(1))
            .with_novel_extractor(novel.clone())
            .with_include(Some(crate::dispatch::Include::Views(vec![
                "artworks".into(),
                "user".into(),
            ])));
        assert!(Arc::ptr_eq(
            &native.user_detail_cache,
            &novel.user_detail_cache
        ));
        let files = crate::job_selection::JobSelection::default()
            .apply(native.events(&Url::parse("https://www.pixiv.net/users/9").unwrap()))
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(files.len(), 2);
        assert!(Arc::ptr_eq(
            files[0]
                .metadata
                .as_ref()
                .unwrap()
                .pixiv_user_detail
                .as_ref()
                .unwrap(),
            files[1]
                .metadata
                .as_ref()
                .unwrap()
                .pixiv_user_detail
                .as_ref()
                .unwrap()
        ));
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 7);
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.contains("/v1/user/detail?user_id=5 "))
                .count(),
            1
        );
        let native = native.with_api_root(Url::parse("http://127.0.0.1:9/").unwrap());
        assert!(!Arc::ptr_eq(
            &native.user_detail_cache,
            &novel.user_detail_cache
        ));
        assert!(native.user_detail_cache.lock().await.entries.is_empty());
    }
    #[test]
    fn detail_rejects_transport_file_context_and_typed_metadata_aliases() {
        for key in [
            "url",
            "filename",
            "name",
            "extension",
            "date",
            "num",
            "count",
            "suffix",
            "hash",
            "date_url",
            "width",
            "height",
            "width_original",
            "height_original",
            "duration",
            "bitrate",
            "filesize",
            "file_size",
            "media_type",
            "description",
            "sensitive_flags",
            "source_id",
            "source_username",
            "exts_image",
            "exts_video",
            "exts_archive",
            "post_id",
            "media_id",
            "shortcode",
            "username",
            "caption",
            "timestamp",
            "index",
            "tags",
            "comments",
            "tags_bookmark",
            "frames",
            "file",
            "profile_asset",
            "ranking",
            "search",
            "series",
            "num_series",
            "user_follow",
            "user_bookmark",
            "pixivision",
            "sketch",
            "ugoira_original",
            "ugoira_frame_index",
            "type",
            "category",
            "subcategory",
            "rating",
            "score",
            "community",
            "discord",
            "creator",
            "revision",
            "retweet_id",
            "quoted_by_id",
            "reposted_by_id",
            "article",
            "instance",
        ] {
            let mut raw = detail(5);
            raw[key] = json!("spoofed");
            assert!(UserDetail::parse(&raw).is_none(), "{key}");
            assert!(serde_json::from_value::<UserDetail>(raw).is_err(), "{key}");
        }
        let mut raw = detail(5);
        raw["_ajax"] = json!(true);
        raw["_mypixiv"] = json!(true);
        raw["total_comments"] = json!(9);
        raw["is_bookmarked"] = json!(true);
        assert!(UserDetail::parse(&raw).is_some());
        // Native route validation excludes leading-zero IDs, so requested and
        // returned identities have one canonical decimal spelling.
        assert!(!super::super::valid_id("005"));
    }
}
