//! Public Pixiv Sketch walls use browser headers, independently of the App API.
use super::{Pixiv, Reply};
use crate::{
    Result, error,
    event::{ExtractionEvent, Post},
    extract::{Media, MediaJob, PostMetadata},
    file_metadata::FileMetadata,
};
use futures_util::{
    StreamExt,
    stream::{self, BoxStream},
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::Arc,
};
use url::Url;

const MAX_STRING: usize = 64 * 1024;
const MAX_BYTES: usize = 1024 * 1024;
const MAX_NODES: usize = 16384;
const MAX_DEPTH: usize = 16;

/// Sketch identifiers are opaque rather than illustration/page identifiers.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && !id.chars().any(char::is_control)
}
pub fn id_from_value(value: &Value) -> Option<String> {
    match value {
        Value::String(id) if valid_id(id) => Some(id.clone()),
        Value::Number(id) if id.as_i128().is_some() && valid_id(id.as_str()) => {
            Some(id.as_str().to_owned())
        }
        _ => None,
    }
}
fn username_valid(username: &str) -> bool {
    !username.is_empty()
        && username.len() <= 1024
        && !username
            .chars()
            .any(|value| value.is_control() || matches!(value, '/' | '?' | '#' | '\\'))
}
pub fn target(url: &Url) -> Result<String> {
    let invalid = || error("unsupported Pixiv Sketch wall URL");
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str() != Some("sketch.pixiv.net")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(invalid());
    }
    let username = url
        .path()
        .strip_prefix("/@")
        .and_then(|path| path.split('/').next())
        .filter(|username| username_valid(username))
        .ok_or_else(invalid)?;
    Ok(username.to_owned())
}
fn omitted(key: &str) -> bool {
    matches!(
        key,
        "id" | "media"
            | "_links"
            | "date"
            | "width"
            | "height"
            | "filename"
            | "extension"
            | "url"
            | "num"
            | "category"
            | "subcategory"
    ) || matches!(
        key,
        "_http_headers"
            | "_http_headers_download"
            | "_extractor"
            | "_fallback"
            | "_cookies"
            | "_cookiejar"
    ) || matches!(
        key.to_ascii_lowercase().as_str(),
        "headers" | "cookies" | "authorization" | "access_token" | "refresh_token"
    )
}
fn bounded(fields: &Map<String, Value>, filter: bool) -> bool {
    fn text(value: &str, bytes: &mut usize) -> bool {
        *bytes = bytes.saturating_add(value.len());
        value.len() <= MAX_STRING && *bytes <= MAX_BYTES
    }
    fn visit(value: &Value, depth: usize, nodes: &mut usize, bytes: &mut usize) -> bool {
        *nodes = nodes.saturating_add(1);
        if depth > MAX_DEPTH || *nodes > MAX_NODES {
            return false;
        }
        match value {
            Value::String(value) => text(value, bytes),
            Value::Number(value) => {
                let raw = value.as_str();
                text(raw, bytes)
                    && if raw.contains(['.', 'e', 'E']) {
                        value.as_f64().is_some_and(f64::is_finite)
                    } else {
                        value.as_i128().is_some()
                    }
            }
            Value::Array(values) => values
                .iter()
                .all(|value| visit(value, depth + 1, nodes, bytes)),
            Value::Object(values) => values
                .iter()
                .all(|(key, value)| text(key, bytes) && visit(value, depth + 1, nodes, bytes)),
            _ => true,
        }
    }
    let mut nodes = 0;
    let mut bytes = 0;
    fields.len() <= 4096
        && fields
            .iter()
            .filter(|(key, _)| !filter || !omitted(key))
            .all(|(key, value)| text(key, &mut bytes) && visit(value, 0, &mut nodes, &mut bytes))
}
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct SketchMetadata(Map<String, Value>);
impl<'de> Deserialize<'de> for SketchMetadata {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let value = Map::<String, Value>::deserialize(deserializer)?;
        Self::parse_compat(&value)
            .ok_or_else(|| serde::de::Error::custom("invalid or oversized Pixiv Sketch metadata"))
    }
}
impl SketchMetadata {
    pub fn fields(&self) -> &Map<String, Value> {
        &self.0
    }
    pub fn owner_id(&self) -> Option<String> {
        id_from_value(self.0.get("user")?.get("id")?)
    }
    pub fn valid(&self) -> bool {
        self.0.get("post_id").and_then(id_from_value).is_some()
            && self.owner_id().is_some()
            && !self.0.keys().any(|key| omitted(key))
            && bounded(&self.0, false)
    }
    pub fn parse_compat(fields: &Map<String, Value>) -> Option<Self> {
        fields.get("post_id").and_then(id_from_value)?;
        fields
            .get("user")
            .and_then(|user| user.get("id"))
            .and_then(id_from_value)?;
        if !bounded(fields, true) {
            return None;
        }
        Some(Self(
            fields
                .iter()
                .filter(|(key, _)| !omitted(key))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        ))
    }
}
struct Current {
    metadata: PostMetadata,
    photos: VecDeque<Value>,
}
struct State {
    username: String,
    next: Option<Value>,
    pages: usize,
    seen: BTreeSet<String>,
    posts: VecDeque<Value>,
    current: Option<Current>,
    end: bool,
    abort: bool,
    done: bool,
}
fn unquote(raw: &str) -> String {
    let mut bytes = Vec::with_capacity(raw.len());
    let mut index = 0;
    let raw = raw.as_bytes();
    while index < raw.len() {
        if raw[index] == b'%'
            && let Some(hex) = raw.get(index + 1..index + 3)
            && let Ok(hex) = std::str::from_utf8(hex)
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            bytes.push(byte);
            index += 3;
            continue;
        }
        bytes.push(raw[index]);
        index += 1;
    }
    String::from_utf8_lossy(&bytes).into_owned()
}
fn media_url(raw: &str) -> Option<Url> {
    if raw.len() > MAX_STRING || raw.chars().any(char::is_control) {
        return None;
    }
    let url = Url::parse(raw).ok()?;
    (matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none())
    .then_some(url)
}
fn photo(photo: Value, current: &Current, profile: &Url) -> Result<Media> {
    let invalid = || error("Pixiv Sketch post contains an invalid photo");
    let id = photo
        .get("id")
        .and_then(id_from_value)
        .ok_or_else(invalid)?;
    let original = photo
        .get("photo")
        .and_then(|photo| photo.get("original"))
        .ok_or_else(invalid)?;
    let raw = original
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(invalid)?;
    let url = media_url(raw).ok_or_else(invalid)?;
    let width = original
        .get("width")
        .ok_or_else(invalid)?
        .as_u64()
        .filter(|value| *value > 0);
    let height = original
        .get("height")
        .ok_or_else(invalid)?
        .as_u64()
        .filter(|value| *value > 0);
    let filename = unquote(
        raw.split('?')
            .next()
            .unwrap_or("")
            .rsplit('/')
            .next()
            .unwrap_or(""),
    );
    let extension = filename
        .rsplit_once('.')
        .filter(|(name, ext)| !name.is_empty() && ext.chars().count() <= 16)
        .map(|(_, ext)| ext.to_lowercase())
        .unwrap_or_default();
    let mut metadata = current.metadata.clone();
    metadata.media_id = id.clone();
    metadata.index = 0;
    metadata.file = Some(FileMetadata {
        width,
        height,
        ..FileMetadata::default()
    });
    Ok(Media {
        filename: format!(
            "{} {}.{}",
            crate::download::safe_component(&metadata.post_id),
            crate::download::safe_component(&id),
            if extension.is_empty() {
                String::new()
            } else {
                crate::download::safe_component(&extension)
            }
        ),
        url,
        job: MediaJob::DirectHttp,
        category: "pixiv".into(),
        metadata: Some(metadata),
        headers: BTreeMap::from([("Referer".into(), profile.as_str().into())]),
        handoff: BTreeMap::new(),
    })
}
impl Pixiv {
    fn sketch_profile(&self, username: &str) -> Result<Url> {
        self.sketch_root
            .join(&format!("@{username}"))
            .map_err(|_| error("invalid Pixiv Sketch wall origin"))
    }
    fn sketch_first(&self, username: &str) -> Result<Url> {
        self.sketch_root
            .join(&format!("api/walls/@{username}/posts/public.json"))
            .map_err(|_| error("invalid Pixiv Sketch wall origin"))
    }
    fn sketch_next(&self, username: &str, raw: &str) -> Result<Url> {
        let invalid = || error("Pixiv Sketch returned an unexpected next page");
        // Source concatenates root + href, rather than joining a relative URL.
        let url = Url::parse(&format!(
            "{}{}",
            self.sketch_root.as_str().trim_end_matches('/'),
            raw
        ))
        .map_err(|_| invalid())?;
        let first = self.sketch_first(username)?;
        if url.origin() != self.sketch_root.origin()
            || url.path() != first.path()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(invalid());
        }
        Ok(url)
    }
    async fn sketch_page(&self, state: &mut State) -> Result<()> {
        if state.pages >= super::MAX_PAGES {
            state.done = true;
            return Ok(());
        }
        let endpoint = if state.pages == 0 {
            self.sketch_first(&state.username)?
        } else {
            let links = state
                .next
                .take()
                .ok_or_else(|| error("Pixiv Sketch returned invalid pagination metadata"))?;
            let links = links
                .as_object()
                .ok_or_else(|| error("Pixiv Sketch returned invalid pagination metadata"))?;
            let Some(next) = links.get("next").filter(|value| super::value_truthy(value)) else {
                state.done = true;
                return Ok(());
            };
            let raw = next
                .get("href")
                .and_then(Value::as_str)
                .filter(|raw| raw.len() <= MAX_STRING)
                .ok_or_else(|| error("Pixiv Sketch returned invalid pagination metadata"))?;
            self.sketch_next(&state.username, raw)?
        };
        let mut canonical = endpoint.clone();
        let mut query: Vec<_> = endpoint
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        query.sort();
        canonical.set_query(None);
        canonical.query_pairs_mut().extend_pairs(query);
        if !state.seen.insert(canonical.to_string()) {
            return Err(error("Pixiv Sketch pagination repeated a page"));
        }
        let profile = self.sketch_profile(&state.username)?;
        let reply = tokio::time::timeout(
            self.timeout,
            self.request(&endpoint, |_, _| {
                self.client
                    .get(endpoint.clone())
                    .header(
                        "User-Agent",
                        self.network.user_agent().unwrap_or("Mozilla/5.0"),
                    )
                    .header("Accept", "application/vnd.sketch-v4+json")
                    .header("Referer", self.sketch_root.as_str())
                    .header("X-Requested-With", profile.as_str())
                    .timeout(self.request_timeout)
            }),
        )
        .await
        .map_err(|_| error("native Pixiv Sketch extraction timed out"))??;
        let bytes = match reply {
            Reply::Ok(bytes) => bytes,
            Reply::Status(status, _) => {
                return Err(error(format!("Pixiv Sketch wall returned HTTP {status}")));
            }
        };
        let mut body = super::json(&bytes)?;
        let posts = body
            .get_mut("data")
            .and_then(|data| data.get_mut("items"))
            .and_then(Value::as_array_mut)
            .ok_or_else(|| error("Pixiv Sketch response contains no post list"))?;
        state.posts = std::mem::take(posts).into();
        // Keep even malformed links until the current posts have been consumed.
        state.next = Some(
            body.get_mut("_links")
                .map(Value::take)
                .unwrap_or(Value::Null),
        );
        state.pages += 1;
        Ok(())
    }
    pub(super) fn sketch_user_events(&self, id: String) -> BoxStream<'_, Result<ExtractionEvent>> {
        stream::once(async move {
            let detail = self.user_detail(&id).await?;
            let user = detail
                .fields()
                .get("user")
                .ok_or_else(|| error("Pixiv response contains no Sketch account"))?;
            if super::id_of(user).as_deref() != Some(&id) {
                return Err(error("Pixiv returned an unexpected Sketch account"));
            }
            let account = user
                .get("account")
                .and_then(Value::as_str)
                .filter(|account| username_valid(account) && !account.contains('%'))
                .ok_or_else(|| error("Pixiv response contains an invalid Sketch account"))?;
            Ok(self.sketch_events(account.to_owned()))
        })
        .map(|result| match result {
            Ok(events) => events,
            Err(error) => stream::once(async move { Err(error) }).boxed(),
        })
        .flatten()
        .boxed()
    }
    pub(super) fn sketch_events(&self, username: String) -> BoxStream<'_, Result<ExtractionEvent>> {
        let state = State {
            username,
            next: None,
            pages: 0,
            seen: BTreeSet::new(),
            posts: VecDeque::new(),
            current: None,
            end: false,
            abort: false,
            done: false,
        };
        stream::unfold(state, move |mut state| async move {
            let mut skipped = 0u8;
            loop {
                if state.end {
                    state.end = false;
                    state.current = None;
                    if state.abort {
                        state.done = true;
                    }
                    return Some((Ok(ExtractionEvent::EndPost), state));
                }
                if state.done {
                    return None;
                }
                if let Some(current) = &mut state.current {
                    if let Some(raw) = current.photos.pop_front() {
                        let profile = match self.sketch_profile(&state.username) {
                            Ok(profile) => profile,
                            Err(error) => {
                                state.abort = true;
                                state.end = true;
                                return Some((Err(error), state));
                            }
                        };
                        match photo(raw, current, &profile) {
                            Ok(mut media) => {
                                self.network.apply_media(&mut media);
                                return Some((Ok(ExtractionEvent::Media(Box::new(media))), state));
                            }
                            Err(error) => {
                                state.abort = true;
                                state.end = true;
                                return Some((Err(error), state));
                            }
                        }
                    }
                    state.end = true;
                    continue;
                }
                if let Some(raw) = state.posts.pop_front() {
                    skipped = skipped.wrapping_add(1);
                    if skipped == 0 {
                        tokio::task::yield_now().await;
                    }
                    let prepared = (|| {
                        let mut fields = if let Value::Object(fields) = raw {
                            fields
                        } else {
                            return Err(error("Pixiv Sketch returned an invalid post"));
                        };
                        let photos = fields
                            .remove("media")
                            .and_then(|value| {
                                if let Value::Array(values) = value {
                                    Some(values)
                                } else {
                                    None
                                }
                            })
                            .ok_or_else(|| error("Pixiv Sketch post contains no media list"))?;
                        let id = fields
                            .remove("id")
                            .ok_or_else(|| error("Pixiv Sketch post contains no ID"))?;
                        let post_id = id_from_value(&id)
                            .ok_or_else(|| error("Pixiv Sketch post contains an invalid ID"))?;
                        fields.insert("post_id".into(), id);
                        let timestamp = fields
                            .get("created_at")
                            .and_then(Value::as_str)
                            .and_then(|date| crate::date::parse_date(date).ok())
                            .ok_or_else(|| error("Pixiv Sketch post contains an invalid date"))?;
                        let sketch = SketchMetadata::parse_compat(&fields)
                            .ok_or_else(|| error("Pixiv Sketch post contains invalid metadata"))?;
                        let username = sketch
                            .fields()
                            .get("user")
                            .and_then(|user| user.get("unique_name"))
                            .and_then(Value::as_str)
                            .map(Arc::from);
                        let caption = sketch
                            .fields()
                            .get("text")
                            .and_then(Value::as_str)
                            .map(Arc::from);
                        let metadata = PostMetadata {
                            post_id,
                            username,
                            caption,
                            timestamp: Some(timestamp),
                            sketch: Some(Arc::new(sketch)),
                            sketch_archive: true,
                            ..PostMetadata::default()
                        };
                        Ok::<_, crate::Error>((metadata, photos))
                    })();
                    match prepared {
                        Ok((metadata, photos)) => {
                            let count = photos.len();
                            state.current = Some(Current {
                                metadata: metadata.clone(),
                                photos: photos.into(),
                            });
                            return Some((
                                Ok(ExtractionEvent::Post(Box::new(Post {
                                    category: "pixiv".into(),
                                    metadata: Some(metadata),
                                    count: Some(count),
                                }))),
                                state,
                            ));
                        }
                        Err(error) => {
                            state.done = true;
                            return Some((Err(error), state));
                        }
                    }
                }
                if let Err(error) = self.sketch_page(&mut state).await {
                    state.done = true;
                    return Some((Err(error), state));
                }
            }
        })
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        filter::FilterExpression,
        job_selection::JobSelection,
        pixiv::tests::{pixiv, response, serve, serve_with, token},
        post_selection::PostSelection,
        selection::RangeSelection,
    };
    use serde_json::json;
    fn fixture() -> Value {
        serde_json::from_str(include_str!("../tests/fixtures/pixiv/sketch.json")).unwrap()
    }
    fn post(id: &str, owner: &str, photos: Vec<Value>) -> Value {
        json!({"id":id,"user":{"id":owner,"unique_name":"actual-owner","name":"Actual Artist"},"text":"raw <b>text</b>","created_at":"2024-01-01T09:00:00+09:00","media":photos,"favorite_count":3})
    }
    fn image(id: &str) -> Value {
        json!({"id":id,"photo":{"original":{"url":format!("https://arbitrary-cdn.example/{id}.JPEG?key=public"),"width":640,"height":480}}})
    }
    fn page(posts: Vec<Value>, next: Option<&str>) -> Vec<u8> {
        response(
            "200 OK",
            json!({"data":{"items":posts},"_links":{"next":next.map(|href|json!({"href":href}))}})
                .to_string()
                .as_bytes(),
        )
    }
    fn url() -> Url {
        Url::parse("https://sketch.pixiv.net/@fixture.user?ignored=1").unwrap()
    }

    #[test]
    fn routes_metadata_bounds_and_opaque_identities_are_source_scoped() {
        for suffix in ["", "/", "/following/extra?ignored=yes"] {
            assert_eq!(
                super::super::target(
                    &Url::parse(&format!("https://sketch.pixiv.net/@fixture.user{suffix}"))
                        .unwrap()
                )
                .unwrap(),
                super::super::Target::Sketch("fixture.user".into())
            );
        }
        for raw in [
            "https://sketch.pixiv.net/fixture.user",
            "https://www.sketch.pixiv.net/@x",
            "https://sketch.pixiv.net/@",
            "https://private:secret@sketch.pixiv.net/@x",
            "https://sketch.pixiv.net:444/@x",
        ] {
            assert!(super::super::target(&Url::parse(raw).unwrap()).is_err());
        }
        for id in ["post-A", "owner/string", "photo with spaces", "0", "-1"] {
            assert!(valid_id(id));
        }
        for id in ["", "\n", "x\0y"] {
            assert!(!valid_id(id));
        }
        assert!(id_from_value(&json!({})).is_none());
        assert!(id_from_value(&json!(1.5)).is_none());
        let raw = json!({"post_id":"post-A","user":{"id":"owner","unique_name":"artist","extras":[true,null]},"text":"rawtext","_http_headers":{"Authorization":"PRIVATE"},"media":["PRIVATE"],"id":"file","date":"ignored","width":900});
        let metadata = SketchMetadata::parse_compat(raw.as_object().unwrap()).unwrap();
        assert!(metadata.valid());
        assert_eq!(metadata.owner_id().as_deref(), Some("owner"));
        assert_eq!(
            metadata
                .fields()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["post_id", "user", "text"]
        );
        assert!(
            !serde_json::to_string(&metadata)
                .unwrap()
                .contains("PRIVATE")
        );
        assert_eq!(
            serde_json::from_value::<SketchMetadata>(serde_json::to_value(&metadata).unwrap())
                .unwrap(),
            metadata
        );
        for bad in [
            json!({"post_id":{},"user":{"id":1}}),
            json!({"post_id":"p","user":{"id":null}}),
            json!({"post_id":"p","user":{"id":"o"},"text":"x".repeat(MAX_STRING+1)}),
        ] {
            assert!(SketchMetadata::parse_compat(bad.as_object().unwrap()).is_none());
            assert!(serde_json::from_value::<SketchMetadata>(bad).is_err());
        }
        let huge:Value=serde_json::from_str("{\"post_id\":\"p\",\"user\":{\"id\":\"o\"},\"number\":170141183460469231731687303715884105728}").unwrap();
        assert!(SketchMetadata::parse_compat(huge.as_object().unwrap()).is_none());
        let mut deep = Value::Null;
        for _ in 0..18 {
            deep = json!([deep]);
        }
        assert!(
            SketchMetadata::parse_compat(
                json!({"post_id":"p","user":{"id":"o"},"deep":deep})
                    .as_object()
                    .unwrap()
            )
            .is_none()
        );
    }

    #[tokio::test]
    async fn recorded_upstream_wall_oracles_match_lazy_frames_metadata_media_and_public_headers() {
        let fixture = fixture();
        for case in fixture["cases"].as_array().unwrap() {
            let calls = case["calls"].as_array().unwrap();
            let replies = case["responses"]
                .as_array()
                .unwrap()
                .iter()
                .take(calls.len())
                .map(|body| response("200 OK", body.to_string().as_bytes()))
                .collect();
            let (root, server) = serve(replies);
            let native = pixiv(root.clone())
                .with_sketch_root(root.clone())
                .with_refresh_token(None)
                .with_max_posts(Some(1))
                .with_tags(super::super::TagMode::Translated)
                .with_ugoira(false);
            let input = Url::parse(case["url"].as_str().unwrap()).unwrap();
            let events = native.events(&input).collect::<Vec<_>>().await;
            let mut source = case["events"].as_array().unwrap().iter();
            let mut posts = 0;
            let mut ends = 0;
            let mut errors = 0;
            let mut files = Vec::new();
            for event in events {
                match event {
                    Err(error) => {
                        errors += 1;
                        assert!(!error.to_string().contains("PRIVATE"));
                    }
                    Ok(ExtractionEvent::EndPost) => ends += 1,
                    Ok(ExtractionEvent::Post(post)) => {
                        posts += 1;
                        let expected = source.next().unwrap();
                        assert_eq!(expected["kind"], "post");
                        let metadata = post.metadata.unwrap();
                        assert_eq!(metadata.timestamp, expected["timestamp"].as_i64());
                        assert_eq!(
                            metadata.sketch.as_deref(),
                            SketchMetadata::parse_compat(expected["metadata"].as_object().unwrap())
                                .as_ref()
                        );
                        assert!(metadata.sketch_archive);
                        assert!(metadata.media_id.is_empty());
                        assert!(metadata.tags.is_none());
                    }
                    Ok(ExtractionEvent::Media(file)) => {
                        let expected = source.next().unwrap();
                        assert_eq!(expected["kind"], "media");
                        assert_eq!(
                            file.filename,
                            expected["default_filename"].as_str().unwrap()
                        );
                        assert_eq!(file.url.as_str(), expected["url"].as_str().unwrap());
                        let metadata = file.metadata.as_ref().unwrap();
                        assert_eq!(metadata.timestamp, expected["timestamp"].as_i64());
                        assert_eq!(
                            metadata.media_id,
                            id_from_value(&expected["metadata"]["id"]).unwrap()
                        );
                        assert_eq!(
                            metadata.sketch.as_deref(),
                            SketchMetadata::parse_compat(expected["metadata"].as_object().unwrap())
                                .as_ref()
                        );
                        assert_eq!(
                            metadata.file.as_ref().unwrap().width,
                            expected["metadata"]["width"].as_u64()
                        );
                        assert_eq!(
                            metadata.file.as_ref().unwrap().height,
                            expected["metadata"]["height"].as_u64()
                        );
                        assert_eq!(metadata.index, 0);
                        assert!(file.archive_key().is_some());
                        assert_eq!(
                            file.headers,
                            BTreeMap::from([(
                                "Referer".into(),
                                root.join("@fixture.user").unwrap().to_string()
                            )])
                        );
                        files.push(file);
                    }
                    _ => panic!("unexpected Sketch child event"),
                }
            }
            assert!(source.next().is_none());
            assert_eq!(posts, ends);
            assert_eq!(errors, usize::from(!case["error"].is_null()));
            if files.len() == 4 {
                assert_eq!(files[0].archive_key(), files[2].archive_key());
                assert_ne!(files[1].archive_key(), files[3].archive_key());
            }
            let requests = server.join().unwrap();
            assert_eq!(requests.len(), calls.len());
            for (request, call) in requests.iter().zip(calls) {
                let expected = Url::parse(call["url"].as_str().unwrap()).unwrap();
                let target = expected.path().to_owned()
                    + &expected
                        .query()
                        .map(|query| format!("?{query}"))
                        .unwrap_or_default();
                assert!(request.starts_with(&format!("GET {target} HTTP/1.1")));
                let lower = request.to_ascii_lowercase();
                assert!(lower.contains("accept: application/vnd.sketch-v4+json\r\n"));
                assert!(lower.contains("user-agent: mozilla/5.0\r\n"));
                assert!(
                    lower.contains(&format!("referer: {}\r\n", root.as_str()).to_ascii_lowercase())
                );
                assert!(
                    lower.contains(
                        &format!(
                            "x-requested-with: {}\r\n",
                            root.join("@fixture.user").unwrap()
                        )
                        .to_ascii_lowercase()
                    )
                );
                for forbidden in [
                    "authorization:",
                    "cookie:",
                    "app-os:",
                    "x-client-hash:",
                    "x-client-time:",
                    "refresh-secret",
                ] {
                    assert!(!lower.contains(forbidden), "{forbidden}");
                }
            }
        }
    }

    #[tokio::test]
    async fn post_boundary_and_file_or_post_range_drop_leave_later_invalid_photos_and_pages_unpolled()
     {
        for selection in [0, 1, 2] {
            let malformed = json!({"id":"bad","photo":{"original":{"url":"PRIVATE"}}});
            let (root, server) = serve(if selection == 2 {
                Vec::new()
            } else {
                vec![page(
                    vec![
                        post("p", "o", vec![image("first"), malformed]),
                        post("later", "o", vec![image("later")]),
                    ],
                    (selection != 2).then_some("/api/walls/@other/posts/public.json"),
                )]
            });
            let native = pixiv(root.clone())
                .with_sketch_root(root)
                .with_refresh_token(None);
            let target = url();
            if selection == 0 {
                let mut events = native.events(&target);
                assert!(matches!(
                    events.next().await.unwrap().unwrap(),
                    ExtractionEvent::Post(_)
                ));
                drop(events);
            } else {
                let job = if selection == 1 {
                    JobSelection {
                        file_range: Some(RangeSelection::parse("1").unwrap()),
                        ..Default::default()
                    }
                } else {
                    JobSelection {
                        post_selection: Some(PostSelection::new(
                            Some(RangeSelection::parse("0").unwrap()),
                            None,
                        )),
                        ..Default::default()
                    }
                };
                let files = job.apply(native.events(&target)).collect::<Vec<_>>().await;
                if selection == 1 {
                    assert_eq!(files.len(), 1);
                    assert!(files[0].is_ok());
                } else {
                    assert!(files.is_empty());
                }
            }
            assert_eq!(server.join().unwrap().len(), usize::from(selection != 2));
        }
    }

    #[tokio::test]
    async fn pagination_scope_and_cycles_are_checked_only_after_current_posts() {
        for next in [
            "/api/walls/@other/posts/public.json",
            "/api/walls/@fixture.user/posts/private.json",
            "//evil.test/wall",
            "https://evil.test/wall",
            "/api/walls/@fixture.user/posts/public.json#secret",
            "/api/walls/@fixture.user/posts/public.json",
        ] {
            let (root, server) = serve(vec![page(vec![post("p", "o", vec![])], Some(next))]);
            let native = pixiv(root.clone())
                .with_sketch_root(root)
                .with_refresh_token(None);
            let events = native.events(&url()).collect::<Vec<_>>().await;
            assert_eq!(events.len(), 3, "{next}");
            assert!(matches!(events[0], Ok(ExtractionEvent::Post(_))));
            assert!(matches!(events[1], Ok(ExtractionEvent::EndPost)));
            assert!(events[2].is_err());
            assert_eq!(server.join().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn profile_include_resolves_account_only_after_selection_and_direct_sketch_needs_no_oauth()
     {
        let profile = response("200 OK", br#"{"user":{"id":9,"account":"fixture.user"}}"#);
        let (root, server) = serve(vec![
            token("t"),
            profile,
            page(vec![post("p", "o", vec![image("photo")])], None),
        ]);
        let native = pixiv(root.clone())
            .with_sketch_root(root)
            .with_include(Some(crate::dispatch::Include::Views(vec!["sketch".into()])));
        let events = native
            .events(&Url::parse("https://www.pixiv.net/users/9").unwrap())
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert!(matches!(events[0], ExtractionEvent::BeginChild(_)));
        assert!(matches!(
            events.last().unwrap(),
            ExtractionEvent::EndChild(_)
        ));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, ExtractionEvent::Media(_)))
                .count(),
            1
        );
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[1].starts_with("GET /v1/user/detail?user_id=9 "));
        assert!(requests[2].starts_with("GET /api/walls/@fixture.user/posts/public.json "));
        let (root, server) = serve(vec![page(vec![], None)]);
        let native = pixiv(root.clone())
            .with_sketch_root(root)
            .with_refresh_token(None)
            .with_include(Some(crate::dispatch::Include::Views(vec!["sketch".into()])));
        let input = Url::parse("https://www.pixiv.net/users/9").unwrap();
        let rejected = JobSelection {
            child_filter: Some(FilterExpression::parse("False").unwrap()),
            ..Default::default()
        }
        .apply(native.events(&input))
        .collect::<Vec<_>>()
        .await;
        assert!(rejected.is_empty());
        assert!(native.events(&url()).collect::<Vec<_>>().await.is_empty());
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn bounded_pagination_and_custom_user_agent_apply_to_public_wall_requests() {
        let (root, server) = serve_with(|_| {
            (0..super::super::MAX_PAGES)
                .map(|index| {
                    page(
                        vec![],
                        Some(&format!(
                            "/api/walls/@fixture.user/posts/public.json?offset={}",
                            index + 1
                        )),
                    )
                })
                .collect()
        });
        let native = pixiv(root.clone())
            .with_sketch_root(root)
            .with_refresh_token(None)
            .with_network(
                &crate::network::NetworkOptions::new(
                    Some(String::new()),
                    Some("Sketch fixture agent".into()),
                )
                .unwrap(),
            )
            .unwrap();
        assert!(native.events(&url()).collect::<Vec<_>>().await.is_empty());
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), super::super::MAX_PAGES);
        assert!(
            requests
                .iter()
                .all(|request| request.contains("user-agent: Sketch fixture agent\r\n"))
        );
    }
    #[tokio::test]
    async fn malformed_truthy_pagination_fails_after_current_media_but_falsy_next_ends_and_range_skips_it()
     {
        for next in [
            json!(true),
            json!(1),
            json!({"href":null}),
            json!(["PRIVATE"]),
            json!({"href":{}}),
        ] {
            for stop in [false, true] {
                let (root,server)=serve(vec![response("200 OK",json!({"data":{"items":[post("p","o",vec![image("first")])]},"_links":{"next":next}}).to_string().as_bytes())]);
                let native = pixiv(root.clone())
                    .with_sketch_root(root)
                    .with_refresh_token(None);
                if stop {
                    let files = JobSelection {
                        file_range: Some(RangeSelection::parse("1").unwrap()),
                        ..Default::default()
                    }
                    .apply(native.events(&url()))
                    .collect::<Vec<_>>()
                    .await;
                    assert_eq!(files.len(), 1);
                    assert!(files[0].is_ok());
                } else {
                    let events = native.events(&url()).collect::<Vec<_>>().await;
                    assert_eq!(events.len(), 4);
                    assert!(matches!(events[0], Ok(ExtractionEvent::Post(_))));
                    assert!(matches!(events[1], Ok(ExtractionEvent::Media(_))));
                    assert!(matches!(events[2], Ok(ExtractionEvent::EndPost)));
                    assert!(events[3].is_err());
                }
                assert_eq!(server.join().unwrap().len(), 1);
            }
        }
        for next in [
            Value::Null,
            json!(false),
            json!(0),
            json!([]),
            json!({}),
            json!(""),
        ] {
            let (root, server) = serve(vec![response(
                "200 OK",
                json!({"data":{"items":[]},"_links":{"next":next}})
                    .to_string()
                    .as_bytes(),
            )]);
            assert!(
                pixiv(root.clone())
                    .with_sketch_root(root)
                    .events(&url())
                    .collect::<Vec<_>>()
                    .await
                    .is_empty()
            );
            assert_eq!(server.join().unwrap().len(), 1);
        }
    }
    #[tokio::test]
    async fn opaque_ids_are_retained_for_metadata_archive_and_sanitized_in_filenames() {
        let (root, server) = serve(vec![page(
            vec![post("../..", "owner", vec![image("photo/name")])],
            None,
        )]);
        let files = pixiv(root.clone())
            .with_sketch_root(root)
            .with_refresh_token(None)
            .stream(&url())
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(files.len(), 1);
        assert!(!files[0].filename.contains('/'));
        assert!(!files[0].filename.contains('\\'));
        let metadata = files[0].metadata.as_ref().unwrap();
        assert_eq!(metadata.post_id, "../..");
        assert_eq!(metadata.media_id, "photo/name");
        assert!(files[0].archive_key().is_some());
        assert_eq!(server.join().unwrap().len(), 1);
    }
}
