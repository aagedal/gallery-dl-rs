//! Ugoira ZIP and original-frame selection mirrors the source extractor.
use super::{Pixiv, UgoiraFrame};
use crate::{
    Result, error,
    extract::{Media, PostMetadata},
};
use serde_json::Value;
use std::{collections::BTreeSet, sync::Arc, time::SystemTime};
use url::Url;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UgoiraMode {
    Skip,
    #[default]
    Zip,
    Original,
}
impl UgoiraMode {
    pub fn parse(value: &Value) -> Self {
        if value.as_str() == Some("original") {
            Self::Original
        } else if super::value_truthy(value) {
            Self::Zip
        } else {
            Self::Skip
        }
    }
    pub fn value(self) -> Value {
        match self {
            Self::Skip => Value::Bool(false),
            Self::Zip => Value::Bool(true),
            Self::Original => Value::String("original".into()),
        }
    }
    pub fn enabled(self) -> bool {
        self != Self::Skip
    }
}

// Source nameext_from_url preserves JPEG and extensions up to 16 characters.
fn original_extension(url: &Url) -> String {
    let raw = url.path().rsplit('/').next().unwrap_or("");
    let mut bytes = Vec::with_capacity(raw.len());
    let mut index = 0;
    let raw = raw.as_bytes();
    while index < raw.len() {
        if raw[index] == b'%'
            && let Some(hex) = raw.get(index + 1..index + 3)
            && let Ok(hex) = std::str::from_utf8(hex)
            && let Ok(value) = u8::from_str_radix(hex, 16)
        {
            bytes.push(value);
            index += 3;
            continue;
        }
        bytes.push(raw[index]);
        index += 1;
    }
    let filename = String::from_utf8_lossy(&bytes);
    filename
        .rsplit_once('.')
        .filter(|(name, extension)| !name.is_empty() && extension.chars().count() <= 16)
        .map(|(_, extension)| extension.to_lowercase())
        .unwrap_or_default()
}

struct Source {
    base: String,
    extension: Option<String>,
}
impl Source {
    fn parse(illust: &Value) -> Result<Self> {
        let invalid = || error("Pixiv returned an invalid original ugoira URL");
        let raw = illust
            .get("meta_single_page")
            .and_then(|page| page.get("original_image_url"))
            .and_then(Value::as_str)
            .filter(|raw| raw.len() <= 64 * 1024)
            .ok_or_else(invalid)?;
        super::media_url(raw).ok_or_else(invalid)?;
        if let Some((base, extension)) = raw.rsplit_once("_ugoira0.") {
            Ok(Self {
                base: format!("{base}_ugoira"),
                extension: Some(extension.to_owned()),
            })
        } else {
            let (base, separator) = raw
                .rsplit_once("_ugoira")
                .map_or(("", ""), |(base, _)| (base, "_ugoira"));
            let base = format!(
                "{}{separator}",
                base.replacen("/img-zip-ugoira/", "/img-original/", 1)
            );
            // A source without the marker would generate relative frame URLs;
            // reject it before probing or publishing any unrelated URL.
            if base.is_empty() {
                return Err(invalid());
            }
            Ok(Self {
                base,
                extension: None,
            })
        }
    }
    fn url(&self, index: usize, extension: &str) -> Result<Url> {
        super::media_url(&format!("{}{index}.{extension}", self.base))
            .ok_or_else(|| error("Pixiv returned an invalid original ugoira frame URL"))
    }
    fn media(
        &self,
        illust: &Value,
        frames: Arc<[UgoiraFrame]>,
        extension: &str,
    ) -> Result<Vec<Media>> {
        let id = super::id_of(illust)
            .ok_or_else(|| error("Pixiv original ugoira contains no illustration ID"))?;
        // Account for URL canonicalization (notably UTF-8 percent expansion)
        // before multiplying a source URL across up to 100,000 frames.
        let per_url = self
            .url(frames.len().saturating_sub(1), extension)?
            .as_str()
            .len();
        if per_url.saturating_mul(frames.len()) > super::MAX_RESPONSE {
            return Err(error(
                "Pixiv original ugoira frame URLs exceed the size limit",
            ));
        }
        let urls = (0..frames.len())
            .map(|index| self.url(index, extension))
            .collect::<Result<Vec<_>>>()?;
        let mut media = super::parse_files(illust, &id, urls);
        for (index, file) in media.iter_mut().enumerate() {
            file.filename = format!("{id}_p{index}.{}", original_extension(&file.url));
            let metadata = file.metadata.as_mut().expect("Pixiv frame metadata");
            if let Some(file) = &mut metadata.file {
                file.width = illust
                    .get("width")
                    .and_then(Value::as_u64)
                    .filter(|value| *value > 0);
                file.height = illust
                    .get("height")
                    .and_then(Value::as_u64)
                    .filter(|value| *value > 0);
            }
            metadata.frames = Some(frames.clone());
            metadata.ugoira_original = Some(true);
            metadata.ugoira_frame_index = Some(index);
        }
        Ok(media)
    }
}
impl Pixiv {
    /// Redirect probes to a bounded fixture server while preserving actual
    /// i.pximg.net URLs in emitted media and all production requests by default.
    pub fn with_ugoira_probe_root(mut self, root: Url) -> Self {
        self.ugoira_probe_root = Some(root);
        self
    }
    pub(super) async fn ugoira_probe(&self, url: &Url) -> bool {
        let endpoint = if let Some(root) = &self.ugoira_probe_root {
            let mut endpoint = root.clone();
            endpoint.set_path(url.path());
            endpoint.set_query(url.query());
            endpoint.set_fragment(None);
            endpoint
        } else {
            url.clone()
        };
        // Source catches exhausted HTTP/connection/timeout errors and advances
        // to the next format. Public probes never use authenticated API headers.
        tokio::time::timeout(self.timeout, self.ugoira_probe_redirects(endpoint))
            .await
            .unwrap_or(false)
    }
    async fn ugoira_probe_redirects(&self, mut endpoint: Url) -> bool {
        let origin = endpoint.origin();
        let mut seen = BTreeSet::new();
        for _ in 0..=10 {
            if !endpoint.username().is_empty()
                || endpoint.password().is_some()
                || !seen.insert(endpoint.to_string())
            {
                return false;
            }
            let mut attempt = 0;
            let next = loop {
                if self
                    .rate_limits
                    .wait(&endpoint, self.timeout)
                    .await
                    .is_err()
                {
                    return false;
                }
                let response = self
                    .client
                    .head(endpoint.clone())
                    .header("Referer", super::REFERER)
                    .timeout(self.request_timeout)
                    .send()
                    .await;
                let response = match response {
                    Ok(response) => response,
                    Err(_) if attempt < self.retries => {
                        tokio::time::sleep(super::backoff(attempt)).await;
                        attempt += 1;
                        continue;
                    }
                    Err(_) => return false,
                };
                let status = response.status();
                if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
                    let delay = super::retry_after(response.headers(), SystemTime::now());
                    self.rate_limits
                        .defer(&endpoint, delay.unwrap_or_else(|| super::backoff(attempt)));
                    if attempt < self.retries {
                        tokio::time::sleep(super::backoff(attempt)).await;
                        attempt += 1;
                        continue;
                    }
                    return false;
                }
                if status.as_u16() >= 400 {
                    return false;
                }
                if matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308)
                    && let Some(location) = response.headers().get(reqwest::header::LOCATION)
                {
                    let Ok(location) = location.to_str() else {
                        return false;
                    };
                    if location.is_empty() {
                        return true;
                    }
                    if location.len() > 64 * 1024 || location.chars().any(char::is_control) {
                        return false;
                    }
                    let Ok(mut next) = endpoint.join(location) else {
                        return false;
                    };
                    if next.origin() != origin
                        || !next.username().is_empty()
                        || next.password().is_some()
                    {
                        return false;
                    }
                    next.set_fragment(None);
                    break next;
                }
                // requests accepts locationless 3xx responses as successful.
                return true;
            };
            endpoint = next;
        }
        false
    }
    pub(super) async fn original_ugoira(&self, illust: &Value, bytes: &[u8]) -> Result<Vec<Media>> {
        let body = super::json(bytes)?;
        let frames = body
            .get("ugoira_metadata")
            .and_then(|data| data.get("frames"))
            .and_then(super::parse_frames)
            .ok_or_else(|| error("Pixiv returned invalid ugoira frame metadata"))?;
        let source = Source::parse(illust)?;
        let extension = if let Some(extension) = &source.extension {
            extension.as_str()
        } else {
            let mut extension = "gif";
            for candidate in ["jpg", "png", "gif"] {
                let url = source.url(0, candidate)?;
                if self.ugoira_probe(&url).await {
                    extension = candidate;
                    break;
                }
            }
            extension
        };
        source.media(illust, frames, extension)
    }
    pub(super) fn empty_work_metadata(
        &self,
        work: &Value,
        id: &str,
        successful: bool,
    ) -> PostMetadata {
        let mut metadata = self.illustration_metadata(work, id);
        if successful
            && self.ugoira == UgoiraMode::Original
            && work.get("type").and_then(Value::as_str) == Some("ugoira")
            && work.get("__sanity_no_ugoira") != Some(&Value::Bool(true))
        {
            metadata.frames = Some(Arc::from([]));
            metadata.ugoira_original = Some(true);
        }
        metadata
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        event::ExtractionEvent,
        job_selection::JobSelection,
        pixiv::tests::{bookmark_owner_reply, pixiv, response, serve, serve_with, token},
        post_selection::PostSelection,
        selection::RangeSelection,
    };
    use futures_util::StreamExt;
    use serde_json::json;
    fn fixture() -> Value {
        serde_json::from_str(include_str!("../tests/fixtures/pixiv/ugoira_original.json")).unwrap()
    }
    fn work(raw: &str) -> Value {
        json!({"id":42,"type":"ugoira","title":"Title","user":{"id":5,"account":"artist"},"tags":[{"name":"猫","translated_name":"cat"}],"width":640,"height":480,"create_date":"2024-01-02T12:04:05+09:00","meta_single_page":{"original_image_url":raw},"meta_pages":[]})
    }
    fn direct() -> &'static str {
        "https://i.pximg.net/img-original/img/2024/01/02/03/04/05/42_ugoira0.jpg"
    }
    fn zipped() -> &'static str {
        "https://i.pximg.net/img-zip-ugoira/img/2024/01/02/03/04/05/42_ugoira600x600.zip"
    }
    fn metadata(count: usize) -> Value {
        json!({"ugoira_metadata":{"frames":(0..count).map(|index|json!({"file":format!("{:06}.jpg",count-index),"delay":index})).collect::<Vec<_>>(),"zip_urls":{"medium":zipped()}}})
    }
    fn reply(value: &Value) -> Vec<u8> {
        response("200 OK", value.to_string().as_bytes())
    }
    #[test]
    fn exact_mode_truthiness_and_media_id_frame_bounds() {
        assert_eq!(UgoiraMode::default(), UgoiraMode::Zip);
        for value in [
            Value::Null,
            json!(false),
            json!(0),
            json!(""),
            json!([]),
            json!({}),
        ] {
            assert_eq!(UgoiraMode::parse(&value), UgoiraMode::Skip);
        }
        for value in [
            json!(true),
            json!(-1),
            json!("Original"),
            json!("false"),
            json!([0]),
            json!({"x":false}),
        ] {
            assert_eq!(UgoiraMode::parse(&value), UgoiraMode::Zip);
        }
        assert_eq!(UgoiraMode::parse(&json!("original")), UgoiraMode::Original);
        for mode in [UgoiraMode::Skip, UgoiraMode::Zip, UgoiraMode::Original] {
            assert_eq!(UgoiraMode::parse(&mode.value()), mode);
        }
        assert!(super::super::valid_media_id("42", "42_p99999"));
        assert!(!super::super::valid_media_id("42", "42_p100000"));
        assert!(!super::super::valid_media_id("42", "42_p00001"));
        assert!(super::super::parse_frames(&json!([])).unwrap().is_empty());
    }

    #[test]
    fn original_filename_extensions_preserve_jpeg_and_source_nameext_rules() {
        for (raw, extension) in [
            ("https://i.pximg.net/42_ugoira0.JPEG", "jpeg"),
            ("https://i.pximg.net/42_ugoira0.webp?query=ignored", "webp"),
            ("https://i.pximg.net/42_ugoira0.tiff", "tiff"),
            ("https://i.pximg.net/42_ugoira0.%6A%70%67", "jpg"),
            ("https://i.pximg.net/42_ugoira0.ABCDEFGHIJK", "abcdefghijk"),
        ] {
            let source = Source::parse(&work(raw)).unwrap();
            let frames = Arc::from([UgoiraFrame {
                file: "ignored.name".into(),
                delay: 1,
            }]);
            let files = source
                .media(&work(raw), frames, source.extension.as_deref().unwrap())
                .unwrap();
            assert_eq!(files[0].filename, format!("42_p0.{extension}"));
        }
    }

    #[tokio::test]
    async fn recorded_upstream_modes_original_derivation_empty_frames_and_probe_order_match_http() {
        for case in fixture()["cases"].as_array().unwrap() {
            let mode = if case["mode_set"] == true {
                UgoiraMode::parse(&case["mode"])
            } else {
                UgoiraMode::default()
            };
            let mut replies = vec![
                token("ACCESS_SECRET"),
                reply(&json!({"illust":work(case["img_url"].as_str().unwrap())})),
            ];
            if mode.enabled() {
                replies.push(reply(&json!({"ugoira_metadata":case["ugoira_metadata"]})));
            }
            for outcome in case["head_outcomes"].as_array().unwrap() {
                replies.push(match outcome.as_str().unwrap() {
                    "success" => response("200 OK", b""),
                    "http_error" => response("404 Not Found", b""),
                    "network_exhausted" => Vec::new(),
                    other => panic!("{other}"),
                });
            }
            let (root, server) = serve(replies);
            let native = pixiv(root.clone())
                .with_ugoira_probe_root(root)
                .with_ugoira_mode(mode);
            let events = native
                .events(&Url::parse("https://www.pixiv.net/artworks/42").unwrap())
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .collect::<Result<Vec<_>>>()
                .unwrap();
            let mut expected = case["events"].as_array().unwrap().iter();
            let mut posts = 0;
            let mut ends = 0;
            let mut shared: Option<Arc<[UgoiraFrame]>> = None;
            for event in events {
                match event {
                    ExtractionEvent::EndPost => ends += 1,
                    ExtractionEvent::Post(post) => {
                        posts += 1;
                        let source = expected.next().unwrap();
                        assert_eq!(source["kind"], "post");
                        assert_eq!(
                            post.count,
                            source["metadata"]["count"]
                                .as_u64()
                                .map(|count| count as usize)
                        );
                        let metadata = post.metadata.unwrap();
                        assert_eq!(
                            metadata.source_num.as_ref(),
                            Some(&source["metadata"]["num"])
                        );
                        assert_eq!(
                            metadata.ugoira_original,
                            source["metadata"]["_ugoira_original"].as_bool()
                        );
                        assert_eq!(metadata.ugoira_frame_index, None);
                        if mode.enabled() {
                            assert_eq!(
                                serde_json::to_value(metadata.frames.as_ref().unwrap()).unwrap(),
                                case["frames"]
                            );
                            shared = metadata.frames;
                        } else {
                            assert!(metadata.frames.is_none());
                        }
                    }
                    ExtractionEvent::Media(file) => {
                        let source = expected.next().unwrap();
                        assert_eq!(source["kind"], "media");
                        assert_eq!(
                            file.url.as_str(),
                            source["url"].as_str().unwrap(),
                            "{}",
                            case["name"]
                        );
                        assert_eq!(file.filename, source["default_filename"].as_str().unwrap());
                        let metadata = file.metadata.as_ref().unwrap();
                        assert_eq!(
                            crate::postprocess::context(&file)["num"],
                            source["metadata"]["num"]
                        );
                        assert_eq!(
                            metadata.ugoira_original,
                            source["metadata"]["_ugoira_original"].as_bool()
                        );
                        if mode == UgoiraMode::Original {
                            assert_eq!(metadata.file.as_ref().unwrap().width, Some(640));
                            assert_eq!(metadata.file.as_ref().unwrap().height, Some(480));
                            assert_eq!(
                                metadata.file.as_ref().unwrap().width,
                                source["metadata"]["width"].as_u64()
                            );
                            assert_eq!(
                                metadata.file.as_ref().unwrap().height,
                                source["metadata"]["height"].as_u64()
                            );
                        }
                        assert_eq!(
                            metadata.ugoira_frame_index,
                            source["metadata"]["_ugoira_frame_index"]
                                .as_u64()
                                .map(|index| index as usize)
                        );
                        assert!(Arc::ptr_eq(
                            metadata.frames.as_ref().unwrap(),
                            shared.as_ref().unwrap()
                        ));
                        assert!(file.archive_key().is_some());
                        assert_eq!(
                            file.headers,
                            std::collections::BTreeMap::from([(
                                "Referer".into(),
                                super::super::REFERER.into()
                            )])
                        );
                    }
                    _ => panic!("unexpected child event"),
                }
            }
            assert!(expected.next().is_none(), "{}", case["name"]);
            assert_eq!(posts, 1);
            assert_eq!(ends, 1);
            let requests = server.join().unwrap();
            let calls = case["calls"].as_array().unwrap();
            assert_eq!(requests.len(), 1 + calls.len(), "{}", case["name"]);
            assert!(requests[1].starts_with("GET /v1/illust/detail?illust_id=42 "));
            if mode.enabled() {
                assert!(requests[2].starts_with("GET /v1/ugoira/metadata?illust_id=42 "));
            }
            for (request, call) in requests.iter().skip(3).zip(calls.iter().skip(2)) {
                let url = Url::parse(call["url"].as_str().unwrap()).unwrap();
                assert!(request.starts_with(&format!("HEAD {} HTTP/1.1", url.path())));
                let lower = request.to_ascii_lowercase();
                assert!(lower.contains("referer: https://app-api.pixiv.net/\r\n"));
                assert!(lower.contains("user-agent: pixiviosapp/"));
                for forbidden in [
                    "authorization:",
                    "cookie:",
                    "app-os:",
                    "x-client-hash:",
                    "x-client-time:",
                    "access_secret",
                    "refresh-secret",
                ] {
                    assert!(!lower.contains(forbidden), "{forbidden}");
                }
            }
        }
    }

    #[tokio::test]
    async fn exhausted_status_retries_advance_probe_formats_and_redirects_are_not_followed() {
        let (root, server) = serve(vec![
            token("t"),
            reply(&json!({"illust":work(zipped())})),
            reply(&metadata(2)),
            response("503 Service Unavailable", b""),
            response("503 Service Unavailable", b""),
            b"HTTP/1.1 302 Found\r\nLocation: https://evil.example/PRIVATE\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
            response("200 OK", b""),
        ]);
        let native = pixiv(root.clone())
            .with_ugoira_probe_root(root)
            .with_ugoira_mode(UgoiraMode::Original)
            .with_retries(1);
        let files = native
            .stream(&Url::parse("https://www.pixiv.net/artworks/42").unwrap())
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(files.len(), 2);
        assert!(files.iter().all(|file| file.url.path().ends_with(".gif")));
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 7);
        assert!(requests[3].starts_with("HEAD ") && requests[3].contains("0.jpg "));
        assert!(requests[4].contains("0.jpg "));
        assert!(requests[5].contains("0.png "));
        assert!(requests[6].contains("0.gif "));
    }

    #[test]
    fn original_frames_share_metadata_and_bound_count_and_aggregate_url_allocation() {
        let source = Source::parse(&work(direct())).unwrap();
        let frames: Arc<[UgoiraFrame]> = (0..super::super::MAX_UGOIRA_FRAMES)
            .map(|index| UgoiraFrame {
                file: format!("{index}.jpg"),
                delay: 1,
            })
            .collect::<Vec<_>>()
            .into();
        let files = source
            .media(&work(direct()), frames.clone(), "jpg")
            .unwrap();
        assert_eq!(files.len(), 100000);
        assert_eq!(files.last().unwrap().filename, "42_p99999.jpg");
        assert_eq!(
            files
                .last()
                .unwrap()
                .metadata
                .as_ref()
                .unwrap()
                .ugoira_frame_index,
            Some(99999)
        );
        assert!(Arc::ptr_eq(
            files[99999]
                .metadata
                .as_ref()
                .unwrap()
                .frames
                .as_ref()
                .unwrap(),
            &frames
        ));
        assert!(super::super::valid_media_id(
            "42",
            &files[99999].metadata.as_ref().unwrap().media_id
        ));
        let huge = Source {
            base: format!("https://i.pximg.net/{}", "a".repeat(64000)),
            extension: Some("jpg".into()),
        };
        assert!(huge.media(&work(direct()), frames, "jpg").is_err());
        let expanded = Source {
            base: format!("https://i.pximg.net/{}_ugoira", "猫".repeat(10000)),
            extension: None,
        };
        let frames = Arc::from(vec![
            UgoiraFrame {
                file: "x.jpg".into(),
                delay: 1
            };
            500
        ]);
        assert!(expanded.media(&work(direct()), frames, "jpg").is_err());
    }

    #[tokio::test]
    async fn original_metadata_failure_closes_empty_post_and_leaves_later_source_works_available() {
        let mut image = work(direct());
        image["id"] = json!(43);
        image["type"] = json!("illust");
        image["meta_single_page"]["original_image_url"] =
            json!("https://i.pximg.net/img-original/43_p0.jpg");
        for bookmark in [false, true] {
            let mut replies = vec![token("t")];
            if bookmark {
                replies.push(bookmark_owner_reply(9));
            }
            replies.extend([
                reply(&json!({"illusts":[work(direct()),image],"next_url":null})),
                response("404 Not Found", b""),
            ]);
            let (root, server) = serve(replies);
            let native = pixiv(root).with_ugoira_mode(UgoiraMode::Original);
            let input = Url::parse(if bookmark {
                "https://www.pixiv.net/users/9/bookmarks/artworks"
            } else {
                "https://www.pixiv.net/users/9/illustrations"
            })
            .unwrap();
            let events = native.events(&input).collect::<Vec<_>>().await;
            assert!(events[0].is_err());
            let mut posts = 0;
            let mut ends = 0;
            let mut files = 0;
            for event in events.into_iter().skip(1) {
                match event.unwrap() {
                    ExtractionEvent::Post(post) => {
                        posts += 1;
                        let metadata = post.metadata.unwrap();
                        assert_eq!(metadata.user_bookmark.is_some(), bookmark);
                        if posts == 1 {
                            assert_eq!(post.count, Some(0));
                            assert!(metadata.frames.is_none());
                            assert!(metadata.ugoira_original.is_none());
                        }
                    }
                    ExtractionEvent::EndPost => ends += 1,
                    ExtractionEvent::Media(_) => files += 1,
                    _ => {}
                }
            }
            assert_eq!((posts, ends, files), (2, 2, 1));
            assert_eq!(server.join().unwrap().len(), if bookmark { 4 } else { 3 });
        }
    }

    #[tokio::test]
    async fn selection_drop_never_fetches_future_work_metadata_and_zero_frames_keep_post_positions()
    {
        for (range, expected) in [("1", 0), ("2", 1)] {
            let mut image = work(direct());
            image["id"] = json!(43);
            image["type"] = json!("illust");
            image["meta_single_page"]["original_image_url"] =
                json!("https://i.pximg.net/img-original/43_p0.jpg");
            let (root, server) = serve(vec![
                token("t"),
                reply(&json!({"illusts":[work(direct()),image,work(zipped())],"next_url":null})),
                reply(&metadata(0)),
            ]);
            let native = pixiv(root).with_ugoira_mode(UgoiraMode::Original);
            let files = JobSelection {
                post_selection: Some(PostSelection::new(
                    Some(RangeSelection::parse(range).unwrap()),
                    None,
                )),
                ..Default::default()
            }
            .apply(
                native.events(&Url::parse("https://www.pixiv.net/users/9/illustrations").unwrap()),
            )
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
            assert_eq!(files.len(), expected);
            assert_eq!(server.join().unwrap().len(), 3);
        }
        let (root, server) = serve_with(|root| {
            vec![
                token("t"),
                reply(
                    &json!({"illusts":[work(direct()),work(zipped())],"next_url":root.join("v1/user/illusts?user_id=9&type=illust&offset=30").unwrap()}),
                ),
                reply(&metadata(3)),
            ]
        });
        let native = pixiv(root).with_ugoira_mode(UgoiraMode::Original);
        let files = JobSelection {
            file_range: Some(RangeSelection::parse("1").unwrap()),
            ..Default::default()
        }
        .apply(native.events(&Url::parse("https://www.pixiv.net/users/9/illustrations").unwrap()))
        .collect::<Vec<_>>()
        .await;
        assert_eq!(files.len(), 1);
        assert!(files[0].is_ok());
        assert_eq!(server.join().unwrap().len(), 3);
    }
    #[tokio::test]
    async fn successful_empty_original_frames_retain_tags_and_every_feed_context() {
        for (input, kind) in [
            ("https://www.pixiv.net/users/9/illustrations", "works"),
            (
                "https://www.pixiv.net/users/9/bookmarks/artworks",
                "bookmark",
            ),
            ("https://www.pixiv.net/ranking.php?date=20240101", "ranking"),
            ("https://www.pixiv.net/tags/cat", "search"),
            ("https://www.pixiv.net/user/999/series/7", "series"),
            ("https://www.pixiv.net/bookmark_new_illust.php", "followed"),
            ("https://www.pixivision.net/a/77", "article"),
        ] {
            let mut replies = Vec::new();
            if kind == "article" {
                replies.push(response(
                    "200 OK",
                    b"<title>Article</title><a href=\"https://www.pixiv.net/en/artworks/42\">",
                ));
            }
            replies.push(reply(
                &json!({"access_token":"t","user":{"id":17,"account":"owner"}}),
            ));
            if kind == "bookmark" {
                replies.push(bookmark_owner_reply(9));
            }
            let mut body = if kind == "article" {
                json!({"illust":work(direct())})
            } else {
                json!({"illusts":[work(direct())],"next_url":null})
            };
            if kind == "series" {
                body["illust_series_detail"] = json!({"id":7,"series_work_count":1});
            }
            replies.push(reply(&body));
            replies.push(reply(&metadata(0)));
            let (root, server) = serve(replies);
            let native = pixiv(root.clone())
                .with_pixivision_root(root)
                .with_ugoira_mode(UgoiraMode::Original)
                .with_tags(super::super::TagMode::Translated);
            let events = native
                .events(&Url::parse(input).unwrap())
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .collect::<Result<Vec<_>>>()
                .unwrap();
            assert_eq!(events.len(), 2, "{kind}");
            let ExtractionEvent::Post(post) = &events[0] else {
                panic!("expected empty Post")
            };
            assert_eq!(post.count, Some(0));
            let metadata = post.metadata.as_ref().unwrap();
            assert_eq!(metadata.ugoira_original, Some(true));
            assert!(metadata.frames.as_ref().unwrap().is_empty());
            assert_eq!(metadata.ugoira_frame_index, None);
            assert_eq!(metadata.tags.as_ref().unwrap().values(), &[json!("cat")]);
            assert_eq!(metadata.user_bookmark.is_some(), kind == "bookmark");
            assert_eq!(metadata.ranking.is_some(), kind == "ranking");
            assert_eq!(metadata.search.is_some(), kind == "search");
            assert_eq!(metadata.series.is_some(), kind == "series");
            assert_eq!(metadata.user_follow.is_some(), kind == "followed");
            assert_eq!(metadata.pixivision.is_some(), kind == "article");
            assert_eq!(
                server.join().unwrap().len(),
                3 + usize::from(matches!(kind, "bookmark" | "article"))
            );
        }
    }

    #[tokio::test]
    async fn cancellation_during_head_closes_connection_and_never_probes_later_formats() {
        use std::time::Duration;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let root = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let (started, waiting) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            for reply in [
                token("ACCESS_SECRET"),
                reply(&json!({"illust":work(zipped())})),
                reply(&metadata(2)),
            ] {
                let (mut connection, _) = listener.accept().await.unwrap();
                let mut buffer = [0; 8192];
                let count = connection.read(&mut buffer).await.unwrap();
                assert_ne!(count, 0);
                connection.write_all(&reply).await.unwrap();
            }
            let (mut connection, _) = listener.accept().await.unwrap();
            let mut buffer = [0; 8192];
            let count = connection.read(&mut buffer).await.unwrap();
            assert_ne!(count, 0);
            let request = String::from_utf8_lossy(&buffer[..count]);
            assert!(request.starts_with("HEAD ") && request.contains("0.jpg "));
            assert!(request.contains("user-agent: Original fixture agent\r\n"));
            assert!(!request.to_ascii_lowercase().contains("authorization:"));
            started.send(()).unwrap();
            match tokio::time::timeout(Duration::from_secs(2), connection.read(&mut buffer))
                .await
                .unwrap()
            {
                Ok(0) => {}
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
                other => panic!("cancelled probe connection remained active: {other:?}"),
            }
            assert!(
                tokio::time::timeout(Duration::from_millis(100), listener.accept())
                    .await
                    .is_err()
            );
        });
        let extraction = tokio::spawn(async move {
            pixiv(root.clone())
                .with_ugoira_probe_root(root)
                .with_ugoira_mode(UgoiraMode::Original)
                .with_network(
                    &crate::network::NetworkOptions::new(
                        Some(String::new()),
                        Some("Original fixture agent".into()),
                    )
                    .unwrap(),
                )
                .unwrap()
                .stream(&Url::parse("https://www.pixiv.net/artworks/42").unwrap())
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
    #[tokio::test]
    async fn public_probes_follow_only_bounded_same_origin_redirects_and_accept_locationless_redirects()
     {
        for location in [None, Some(""), Some("/redirected.jpg")] {
            let mut replies = vec![
                token("t"),
                reply(&json!({"illust":work(zipped())})),
                reply(&metadata(1)),
            ];
            replies.push(match location {None=>response("302 Found",b""),Some(location)=>format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").into_bytes()});
            if location.is_some_and(|location| !location.is_empty()) {
                replies.push(response("200 OK", b""));
            }
            let (root, server) = serve(replies);
            let files = pixiv(root.clone())
                .with_ugoira_probe_root(root)
                .with_ugoira_mode(UgoiraMode::Original)
                .stream(&Url::parse("https://www.pixiv.net/artworks/42").unwrap())
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .collect::<Result<Vec<_>>>()
                .unwrap();
            assert_eq!(files[0].filename, "42_p0.jpg");
            let requests = server.join().unwrap();
            assert_eq!(
                requests.len(),
                4 + usize::from(location.is_some_and(|location| !location.is_empty()))
            );
            if location.is_some_and(|location| !location.is_empty()) {
                assert!(requests[4].starts_with("HEAD /redirected.jpg "));
            }
            for request in &requests[3..] {
                let lower = request.to_ascii_lowercase();
                assert!(
                    !lower.contains("authorization:")
                        && !lower.contains("app-os:")
                        && !lower.contains("x-client-hash:")
                        && !lower.contains("cookie:")
                );
            }
        }
        let (root,server)=serve(vec![token("t"),reply(&json!({"illust":work(zipped())})),reply(&metadata(1)),b"HTTP/1.1 302 Found\r\nLocation: /img-original/img/2024/01/02/03/04/05/42_ugoira0.jpg\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),response("200 OK",b"")]);
        let files = pixiv(root.clone())
            .with_ugoira_probe_root(root)
            .with_ugoira_mode(UgoiraMode::Original)
            .stream(&Url::parse("https://www.pixiv.net/artworks/42").unwrap())
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(files[0].filename, "42_p0.png");
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 5);
        assert!(requests[4].contains("0.png "));
    }
    #[tokio::test]
    async fn only_requests_redirect_statuses_follow_location_and_redirect_count_is_bounded() {
        for status in [
            "300 Multiple Choices",
            "304 Not Modified",
            "305 Use Proxy",
            "306 Unused",
        ] {
            let (root,server)=serve(vec![token("t"),reply(&json!({"illust":work(zipped())})),reply(&metadata(1)),format!("HTTP/1.1 {status}\r\nLocation: https://evil.example/PRIVATE\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").into_bytes()]);
            let files = pixiv(root.clone())
                .with_ugoira_probe_root(root)
                .with_ugoira_mode(UgoiraMode::Original)
                .stream(&Url::parse("https://www.pixiv.net/artworks/42").unwrap())
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .collect::<Result<Vec<_>>>()
                .unwrap();
            assert_eq!(files[0].filename, "42_p0.jpg");
            assert_eq!(server.join().unwrap().len(), 4);
        }
        let mut replies = vec![
            token("t"),
            reply(&json!({"illust":work(zipped())})),
            reply(&metadata(1)),
        ];
        for index in 1..=11 {
            replies.push(format!("HTTP/1.1 302 Found\r\nLocation: /redirect/{index}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").into_bytes());
        }
        replies.push(response("200 OK", b""));
        let (root, server) = serve(replies);
        let files = pixiv(root.clone())
            .with_ugoira_probe_root(root)
            .with_ugoira_mode(UgoiraMode::Original)
            .stream(&Url::parse("https://www.pixiv.net/artworks/42").unwrap())
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(files[0].filename, "42_p0.png");
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 15);
        assert!(requests.last().unwrap().contains("0.png "));
        assert!(
            !requests
                .iter()
                .any(|request| request.starts_with("HEAD /redirect/11 "))
        );
    }
}
