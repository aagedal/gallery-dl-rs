//! Pixivision's public article request is isolated from authenticated App API calls.
use super::{Pixiv, Reply};
use crate::{
    Result, error,
    event::{ExtractionEvent, Post},
};
use futures_util::{
    StreamExt,
    stream::{self, BoxStream},
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, OnceLock},
};
use url::Url;

const MAX_TEXT: usize = 64 * 1024;
const ANCHOR: &str = "<a href=\"https://www.pixiv.net/en/artworks/";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PixivisionMetadata {
    pub id: String,
    pub title: Arc<str>,
}
impl PixivisionMetadata {
    pub fn valid(&self) -> bool {
        super::valid_id(&self.id) && self.title.len() <= MAX_TEXT
    }
    fn parse(id: &Value, title: &Value) -> Option<Self> {
        let id = match id {
            Value::String(id) => {
                let id = id.trim();
                super::valid_id(id).then(|| id.to_owned())?
            }
            Value::Number(id) => {
                let id = id.to_string();
                super::valid_id(&id).then_some(id)?
            }
            _ => return None,
        };
        let title = title.as_str().filter(|title| title.len() <= MAX_TEXT)?;
        Some(Self {
            id,
            title: Arc::from(title),
        })
    }
    pub fn parse_compat(value: &Value) -> Option<Self> {
        Self::parse(value.get("id")?, value.get("title")?)
    }
    pub fn parse_flat(value: &Map<String, Value>) -> Option<Self> {
        Self::parse(value.get("pixivision_id")?, value.get("pixivision_title")?)
    }
}

pub fn target(url: &Url) -> Result<String> {
    let invalid = || error("unsupported Pixivision article URL");
    if !matches!(url.scheme(), "http" | "https")
        || !matches!(
            url.host_str(),
            Some("pixivision.net" | "www.pixivision.net")
        )
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(invalid());
    }
    let path = super::segments(url);
    let id = match path.as_slice() {
        ["a", id] | ["en", "a", id] => *id,
        _ => return Err(invalid()),
    };
    super::valid_id(id)
        .then(|| id.to_owned())
        .ok_or_else(invalid)
}

// Generated from Python 3.15 stdlib html.entities.html5 (WHATWG HTML5 named
// character references). Keys include the legacy semicolonless spellings.
fn entities() -> &'static BTreeMap<String, String> {
    static TABLE: OnceLock<BTreeMap<String, String>> = OnceLock::new();
    TABLE.get_or_init(|| {
        serde_json::from_str(include_str!("pixivision_entities.json"))
            .expect("generated HTML5 entity table")
    })
}
fn numeric(value: u32) -> String {
    const C1: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž',
        '\u{8f}', '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9d}',
        'ž', 'Ÿ',
    ];
    match value {
        0 | 0xd800..=0xdfff | 0x110000..=u32::MAX => "�".into(),
        0x80..=0x9f => C1[(value - 0x80) as usize].to_string(),
        1..=8 | 11 | 14..=31 | 127 | 0xfdd0..=0xfdef => String::new(),
        value if value & 0xffff == 0xfffe || value & 0xffff == 0xffff => String::new(),
        value => char::from_u32(value)
            .expect("validated Unicode scalar")
            .to_string(),
    }
}

/// Python html.unescape's HTML5 replacement rules, including longest named
/// prefix matching, two-codepoint values and legacy numeric replacements.
pub(super) fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at + 1..];
        if let Some(raw) = rest.strip_prefix('#') {
            let (hex, raw) =
                if let Some(raw) = raw.strip_prefix('x').or_else(|| raw.strip_prefix('X')) {
                    (true, raw)
                } else {
                    (false, raw)
                };
            let count = raw
                .bytes()
                .take_while(|byte| {
                    if hex {
                        byte.is_ascii_hexdigit()
                    } else {
                        byte.is_ascii_digit()
                    }
                })
                .count();
            if count > 0 {
                let value = u32::from_str_radix(&raw[..count], if hex { 16 } else { 10 })
                    .unwrap_or(u32::MAX);
                out.push_str(&numeric(value));
                let consumed = 1
                    + usize::from(hex)
                    + count
                    + usize::from(raw.as_bytes().get(count) == Some(&b';'));
                rest = &rest[consumed..];
                continue;
            }
        }
        let mut end = 0;
        let mut count = 0;
        for (index, value) in rest.char_indices() {
            if count == 32 || matches!(value, '\t' | '\n' | '\u{c}' | ' ' | '<' | '&' | '#' | ';') {
                break;
            }
            end = index + value.len_utf8();
            count += 1;
        }
        if count == 0 {
            out.push('&');
            continue;
        }
        if rest.as_bytes().get(end) == Some(&b';') {
            end += 1;
        }
        let candidate = &rest[..end];
        if let Some(value) = entities().get(candidate) {
            out.push_str(value);
        } else {
            let boundaries: Vec<_> = candidate.char_indices().map(|(index, _)| index).collect();
            let matched = boundaries
                .into_iter()
                .enumerate()
                .rev()
                .find_map(|(chars, index)| {
                    (chars >= 2)
                        .then(|| {
                            entities()
                                .get(&candidate[..index])
                                .map(|value| (index, value))
                        })
                        .flatten()
                });
            if let Some((index, value)) = matched {
                out.push_str(value);
                out.push_str(&candidate[index..]);
            } else {
                out.push('&');
                out.push_str(candidate);
            }
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

struct Article {
    html: String,
    cursor: usize,
    previous: Option<(usize, usize)>,
}
impl Article {
    fn next(&mut self) -> Option<Option<&str>> {
        let start = self.cursor + self.html[self.cursor..].find(ANCHOR)? + ANCHOR.len();
        let end = start + self.html[start..].find('"')?;
        self.cursor = end + 1;
        if self
            .previous
            .is_some_and(|(first, last)| self.html[first..last] == self.html[start..end])
        {
            return Some(None);
        }
        self.previous = Some((start, end));
        let raw = &self.html[start..end];
        Some(Some(raw.split_once('?').map_or(raw, |(id, _)| id)))
    }
}
struct State {
    budget: super::PostBudget,
    id: String,
    article: Option<Article>,
    metadata: Option<Arc<PixivisionMetadata>>,
    pending: VecDeque<ExtractionEvent>,
    done: bool,
}
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64() != Some(0.),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

impl Pixiv {
    async fn pixivision_article(&self, id: &str) -> Result<(Article, Arc<PixivisionMetadata>)> {
        let endpoint = self
            .pixivision_root
            .join(&format!("en/a/{id}"))
            .map_err(|_| error("invalid Pixivision article origin"))?;
        let reply = tokio::time::timeout(
            self.timeout,
            self.request(&endpoint, |_, _| {
                // Never use app_headers, bearer_auth, OAuth hash headers or cookies.
                self.client
                    .get(endpoint.clone())
                    .header(
                        "User-Agent",
                        self.network.user_agent().unwrap_or("Mozilla/5.0"),
                    )
                    .timeout(self.request_timeout)
            }),
        )
        .await
        .map_err(|_| error("native Pixivision article extraction timed out"))??;
        let bytes = match reply {
            Reply::Ok(bytes) => bytes,
            Reply::Status(status, _) => {
                return Err(error(format!("Pixivision article returned HTTP {status}")));
            }
        };
        let html = String::from_utf8_lossy(&bytes).into_owned();
        let title = html
            .split_once("<title>")
            .and_then(|(_, rest)| rest.split_once('<').map(|(title, _)| title))
            .unwrap_or("");
        if title.len() > MAX_TEXT {
            return Err(error(
                "Pixivision article title exceeds the metadata size limit",
            ));
        }
        let metadata = Arc::new(PixivisionMetadata {
            id: id.to_owned(),
            title: Arc::from(unescape(title)),
        });
        if !metadata.valid() {
            return Err(error(
                "Pixivision article title exceeds the metadata size limit",
            ));
        }
        Ok((
            Article {
                html,
                cursor: 0,
                previous: None,
            },
            metadata,
        ))
    }
    pub(super) fn pixivision_events(&self, id: String) -> BoxStream<'_, Result<ExtractionEvent>> {
        let state = State {
            budget: super::PostBudget::new(self.max_posts),
            id,
            article: None,
            metadata: None,
            pending: VecDeque::new(),
            done: false,
        };
        stream::unfold(state, move |mut state| async move {
            let mut skipped = 0u8;
            loop {
                if let Some(event) = state.pending.pop_front() {
                    return Some((Ok(event), state));
                }
                if state.budget.exhausted() {
                    return None;
                }
                if state.done {
                    return None;
                }
                if state.article.is_none() {
                    match self.pixivision_article(&state.id).await {
                        Ok((article, metadata)) => {
                            state.article = Some(article);
                            state.metadata = Some(metadata);
                        }
                        Err(error) => {
                            state.done = true;
                            return Some((Err(error), state));
                        }
                    }
                }
                let id = state.article.as_mut().expect("loaded article").next()?;
                skipped = skipped.wrapping_add(1);
                if skipped == 0 {
                    tokio::task::yield_now().await;
                }
                let Some(id) = id else {
                    continue;
                };
                if !super::valid_id(id) {
                    state.done = true;
                    return Some((
                        Err(error(
                            "Pixivision article contains an invalid illustration ID",
                        )),
                        state,
                    ));
                }
                let id = id.to_owned();
                let mut endpoint = self
                    .api_root
                    .join("v1/illust/detail")
                    .expect("static detail endpoint");
                endpoint.query_pairs_mut().append_pair("illust_id", &id);
                let mut body = match self
                    .get(&endpoint)
                    .await
                    .and_then(|bytes| super::json(&bytes))
                {
                    Ok(body) => body,
                    Err(error) => {
                        state.done = true;
                        return Some((Err(error), state));
                    }
                };
                let Some(work) = body.get_mut("illust") else {
                    state.done = true;
                    return Some((Err(error("Pixiv response contains no illustration")), state));
                };
                if super::id_of(work).as_deref() != Some(&id) {
                    state.done = true;
                    return Some((
                        Err(error("Pixiv returned an unexpected illustration")),
                        state,
                    ));
                }
                state.budget.consume();
                if !work
                    .get("user")
                    .and_then(|user| user.get("id"))
                    .is_some_and(truthy)
                {
                    continue;
                }
                let context = state.metadata.as_ref().expect("article metadata").clone();
                let (mut media, failure) = match self.work(work, true).await {
                    Ok(media) => (media, None),
                    Err(super::WorkFailure::Enrichment(error)) => {
                        state.done = true;
                        return Some((Err(error), state));
                    }
                    Err(super::WorkFailure::Files(error)) => (Vec::new(), Some(error)),
                };
                if media.is_empty() {
                    let mut metadata = self.empty_work_metadata(work, &id, failure.is_none());
                    metadata.pixivision = Some(context);
                    metadata.pixivision_archive = true;
                    state.pending = VecDeque::from([
                        ExtractionEvent::Post(Box::new(Post {
                            keywords: Default::default(),
                            category: "pixiv".into(),
                            metadata: Some(metadata),
                            count: Some(0),
                        })),
                        ExtractionEvent::EndPost,
                    ]);
                    if let Some(error) = failure {
                        return Some((Err(error), state));
                    }
                    continue;
                }
                for item in &mut media {
                    let metadata = item.metadata.as_mut().expect("Pixiv illustration metadata");
                    metadata.pixivision = Some(context.clone());
                    metadata.pixivision_archive = true;
                }
                state.pending = self.finish(media);
            }
        })
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        pixiv::tests::{pixiv, response, serve, token},
        post_selection::PostSelection,
        selection::RangeSelection,
    };
    use serde_json::json;

    fn oracle() -> Value {
        serde_json::from_str(include_str!("../tests/fixtures/pixiv/pixivision.json")).unwrap()
    }
    fn url() -> Url {
        Url::parse("http://pixivision.net/a/77?ignored=1").unwrap()
    }
    fn work(id: u64, kind: &str) -> Value {
        json!({"id":id,"type":kind,"user":{"id":5,"account":"artist"},"title":"Title","create_date":"2024-01-01T00:00:00Z","meta_single_page":{"original_image_url":format!("https://i.pximg.net/img-original/{id}_p0.jpg")},"meta_pages":[]})
    }
    fn detail(id: u64, kind: &str) -> Vec<u8> {
        response(
            "200 OK",
            json!({"illust":work(id,kind)}).to_string().as_bytes(),
        )
    }
    fn html(ids: &[&str]) -> Vec<u8> {
        response(
            "200 OK",
            format!(
                "<title>Title</title>{}",
                ids.iter()
                    .map(|id| format!("{ANCHOR}{id}\">x</a>"))
                    .collect::<String>()
            )
            .as_bytes(),
        )
    }

    #[test]
    fn html5_decoder_matches_all_source_vectors_and_metadata_is_bounded() {
        let fixture = oracle();
        for case in fixture["entity_cases"].as_array().unwrap() {
            assert_eq!(
                unescape(case["input"].as_str().unwrap()),
                case["output"].as_str().unwrap(),
                "{}",
                case["input"]
            );
        }
        let expected =
            PixivisionMetadata::parse_flat(fixture["metadata"].as_object().unwrap()).unwrap();
        assert_eq!(expected.id, "77");
        assert!(expected.valid());
        assert_eq!(
            PixivisionMetadata::parse_compat(&json!({"id":77,"title":expected.title})),
            Some(expected)
        );
        for value in [
            json!({"id":[],"title":"x"}),
            json!({"id":"0","title":"x"}),
            json!({"id":"77","title":"x".repeat(MAX_TEXT+1)}),
            json!({"id":"77","title":{}}),
        ] {
            assert!(PixivisionMetadata::parse_compat(&value).is_none());
        }
        for input in [
            "https://www.pixivision.net/en/a/77",
            "http://pixivision.net/a/77?x=1",
        ] {
            assert_eq!(
                super::super::target(&Url::parse(input).unwrap()).unwrap(),
                super::super::Target::Pixivision("77".into())
            );
        }
        for input in [
            "https://pixivision.net/ja/a/77",
            "https://evil.test/en/a/77",
            "https://pixivision.net/en/a/0",
            "https://pixivision.net/a/77/extra",
        ] {
            assert!(super::super::target(&Url::parse(input).unwrap()).is_err());
        }
    }

    #[tokio::test]
    async fn source_article_oracle_preserves_adjacent_raw_dedup_and_public_request_privacy() {
        let fixture = oracle();
        let mut replies = vec![
            response("200 OK", fixture["html"].as_str().unwrap().as_bytes()),
            token("access-secret"),
        ];
        for id in fixture["detail_ids"].as_array().unwrap() {
            replies.push(detail(id.as_str().unwrap().parse().unwrap(), "illust"));
        }
        let (root, server) = serve(replies);
        let native = pixiv(root.clone()).with_pixivision_root(root);
        let events = native
            .events(&url())
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        let expected =
            PixivisionMetadata::parse_flat(fixture["metadata"].as_object().unwrap()).unwrap();
        let mut ids = Vec::new();
        for event in events {
            match event {
                ExtractionEvent::Post(post) => {
                    let metadata = post.metadata.unwrap();
                    assert_eq!(metadata.pixivision.as_deref(), Some(&expected));
                    assert!(metadata.pixivision_archive);
                }
                ExtractionEvent::Media(media) => {
                    let metadata = media.metadata.as_ref().unwrap();
                    ids.push(metadata.post_id.clone());
                    assert_eq!(metadata.pixivision.as_deref(), Some(&expected));
                    assert!(media.archive_key().is_some());
                    let mut ordinary = media.clone();
                    ordinary.metadata.as_mut().unwrap().pixivision_archive = false;
                    ordinary.metadata.as_mut().unwrap().pixivision = None;
                    assert_ne!(media.archive_key(), ordinary.archive_key());
                }
                _ => {}
            }
        }
        assert_eq!(serde_json::to_value(ids).unwrap(), fixture["detail_ids"]);
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /en/a/77 HTTP/1.1"));
        let public = requests[0].to_ascii_lowercase();
        assert!(public.contains("user-agent: mozilla/5.0\r\n"));
        for forbidden in [
            "authorization:",
            "cookie:",
            "x-client-time:",
            "x-client-hash:",
            "refresh-secret",
            "access-secret",
        ] {
            assert!(!public.contains(forbidden), "{forbidden}");
        }
        assert!(requests[1].starts_with("POST /auth/token "));
        for (request, id) in requests[2..]
            .iter()
            .zip(fixture["detail_ids"].as_array().unwrap())
        {
            assert!(request.starts_with(&format!(
                "GET /v1/illust/detail?illust_id={} ",
                id.as_str().unwrap()
            )));
            assert!(request.contains("Bearer access-secret"));
        }
    }

    #[tokio::test]
    async fn public_article_empty_and_http_failure_precede_missing_authentication() {
        for (reply, expected) in [
            (html(&[]), None),
            (response("404 Not Found", b""), Some("404")),
            (html(&["42"]), Some("refresh-token")),
        ] {
            let (root, server) = serve(vec![reply]);
            let native = pixiv(root.clone())
                .with_pixivision_root(root)
                .with_refresh_token(None);
            let events = native.events(&url()).collect::<Vec<_>>().await;
            match expected {
                None => assert!(events.is_empty()),
                Some(text) => {
                    assert_eq!(events.len(), 1);
                    let message = events[0].as_ref().unwrap_err().to_string();
                    assert!(message.contains(text), "{message}");
                }
            }
            assert_eq!(server.join().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn selected_detail_failures_abort_without_fetching_later_anchors() {
        for reply in [response("404 Not Found", b""), detail(99, "ugoira")] {
            let (root, server) = serve(vec![html(&["42", "43"]), token("access"), reply]);
            let native = pixiv(root.clone()).with_pixivision_root(root);
            let events = native.events(&url()).collect::<Vec<_>>().await;
            assert_eq!(events.len(), 1);
            assert!(events[0].is_err());
            assert_eq!(server.join().unwrap().len(), 3);
        }
        let (root, server) = serve(vec![html(&["42/path", "43"])]);
        let native = pixiv(root.clone()).with_pixivision_root(root);
        assert!(native.events(&url()).next().await.unwrap().is_err());
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn empty_animation_retains_post_position_and_range_drop_avoids_later_detail() {
        for (range, expected, details) in [
            ("1", Vec::<String>::new(), 1),
            ("2", vec!["43_p0".into()], 2),
        ] {
            let mut replies = vec![
                html(&["42", "43", "44"]),
                token("access"),
                detail(42, "ugoira"),
            ];
            if details == 2 {
                replies.push(detail(43, "illust"));
            }
            let (root, server) = serve(replies);
            let native = pixiv(root.clone())
                .with_pixivision_root(root)
                .with_ugoira(false);
            let selection = PostSelection::new(Some(RangeSelection::parse(range).unwrap()), None);
            let events = selection
                .apply(native.events(&url()), None, None, true)
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .collect::<Result<Vec<_>>>()
                .unwrap();
            let ids: Vec<_> = events
                .into_iter()
                .map(|media| media.metadata.unwrap().media_id)
                .collect();
            assert_eq!(ids, expected);
            assert_eq!(server.join().unwrap().len(), 2 + details);
        }
    }
    #[tokio::test]
    async fn failed_animation_is_balanced_and_later_illustrations_continue() {
        let (root, server) = serve(vec![
            html(&["42", "43"]),
            token("access"),
            detail(42, "ugoira"),
            response("404 Not Found", b""),
            detail(43, "illust"),
        ]);
        let native = pixiv(root.clone()).with_pixivision_root(root);
        let events = native.events(&url()).collect::<Vec<_>>().await;
        assert!(events[0].is_err());
        let mut posts = Vec::new();
        let mut ends = 0;
        let mut media = Vec::new();
        for event in events.into_iter().skip(1) {
            match event.unwrap() {
                ExtractionEvent::Post(post) => {
                    let metadata = post.metadata.unwrap();
                    assert!(metadata.pixivision_archive);
                    posts.push((metadata.post_id, post.count));
                }
                ExtractionEvent::EndPost => ends += 1,
                ExtractionEvent::Media(file) => media.push(file.filename),
                _ => {}
            }
        }
        assert_eq!(posts, [("42".into(), Some(0)), ("43".into(), Some(1))]);
        assert_eq!(ends, 2);
        assert_eq!(media, ["43_p0.jpg"]);
        let requests = server.join().unwrap();
        assert!(requests[3].starts_with("GET /v1/ugoira/metadata?illust_id=42 "));
    }

    #[tokio::test]
    async fn dropping_after_first_post_leaves_later_details_unrequested_and_honors_configured_ua() {
        let (root, server) = serve(vec![
            html(&["42", "43"]),
            token("access"),
            detail(42, "illust"),
        ]);
        let native = pixiv(root.clone())
            .with_pixivision_root(root)
            .with_network(
                &crate::network::NetworkOptions::new(
                    Some(String::new()),
                    Some("Custom article agent".into()),
                )
                .unwrap(),
            )
            .unwrap();
        let article = url();
        let mut events = native.events(&article);
        assert!(matches!(
            events.next().await.unwrap().unwrap(),
            ExtractionEvent::Post(_)
        ));
        drop(events);
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(
            requests[0]
                .to_ascii_lowercase()
                .contains("user-agent: custom article agent\r\n")
        );
    }
    #[tokio::test]
    async fn max_posts_counts_query_variants_but_not_adjacent_raw_duplicates_before_next_detail() {
        let (root, server) = serve(vec![
            html(&["42?x", "42?x", "42?y", "43"]),
            token("t"),
            detail(42, "illust"),
            detail(42, "illust"),
        ]);
        let native = pixiv(root.clone())
            .with_pixivision_root(root)
            .with_max_posts(Some(2));
        let files = native
            .stream(&url())
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(files.len(), 2);
        assert!(files.iter().all(|file| file.filename == "42_p0.jpg"));
        assert_eq!(server.join().unwrap().len(), 4);
    }
}
