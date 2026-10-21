//! Bounded public novel fields, including persistent uploaded-image mutations.
use serde::Serialize;
use serde_json::{Map, Value};

const MAX_BYTES: usize = 32 * 1024 * 1024;
const MAX_NODES: usize = 1_000_000;
const MAX_DEPTH: usize = 128;

#[derive(Clone, Debug, Serialize)]
#[serde(transparent)]
pub struct NovelMetadata(Map<String, Value>);
impl NovelMetadata {
    pub fn fields(&self) -> &Map<String, Value> {
        &self.0
    }
    /// Capture descriptive source fields without duplicating typed native fields.
    pub fn parse(data: &Map<String, Value>) -> Option<Self> {
        capture(data, false, None).map(Self)
    }
    pub(crate) fn parse_work(
        data: &Map<String, Value>,
        detail: Option<&super::UserDetail>,
    ) -> Option<Self> {
        capture(data, false, detail.map(super::UserDetail::fields)).map(Self)
    }
    pub(crate) fn queue_fields(data: &Map<String, Value>) -> Option<Map<String, Value>> {
        capture(data, true, None)
    }
}
fn capture(
    data: &Map<String, Value>,
    queue: bool,
    excluded: Option<&Map<String, Value>>,
) -> Option<Map<String, Value>> {
    let include = |key: &str| {
        !excluded.is_some_and(|fields| fields.contains_key(key))
            && (matches!(
                key,
                "series"
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
            ) || !reserved(key)
                || queue
                    && matches!(
                        key,
                        "user"
                            | "profile"
                            | "profile_publicity"
                            | "workspace"
                            | "tags"
                            | "comments"
                            | "tags_bookmark"
                            | "caption"
                            | "rating"
                            | "width"
                            | "height"
                    ))
    };
    let mut nodes = 0usize;
    let mut bytes = 0usize;
    fn visit(value: &Value, depth: usize, nodes: &mut usize, bytes: &mut usize) -> bool {
        *nodes = nodes.saturating_add(1);
        if depth > MAX_DEPTH || *nodes > MAX_NODES {
            return false;
        }
        match value {
            Value::String(value) => {
                *bytes = bytes.saturating_add(value.len());
                *bytes <= MAX_BYTES
            }
            Value::Array(values) => values
                .iter()
                .all(|value| visit(value, depth + 1, nodes, bytes)),
            Value::Object(values) => values.iter().all(|(key, value)| {
                *bytes = bytes.saturating_add(key.len());
                *bytes <= MAX_BYTES && visit(value, depth + 1, nodes, bytes)
            }),
            Value::Number(value) => {
                *bytes = bytes.saturating_add(value.as_str().len());
                *bytes <= MAX_BYTES
                    && (value.as_i128().is_some() || value.as_f64().is_some_and(f64::is_finite))
            }
            _ => true,
        }
    }
    if !data
        .iter()
        .filter(|(key, _)| include(key))
        .all(|(key, value)| {
            bytes = bytes.saturating_add(key.len());
            bytes <= MAX_BYTES && visit(value, 0, &mut nodes, &mut bytes)
        })
    {
        return None;
    }
    let fields: Map<_, _> = data
        .iter()
        .filter(|(key, _)| include(key))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len());
            if self.0 > MAX_BYTES {
                return Err(std::io::Error::other(
                    "Pixiv novel metadata exceeds encoded limit",
                ));
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Counter(0), &fields).ok()?;
    Some(fields)
}
fn reserved(key: &str) -> bool {
    key.starts_with('_')
        || matches!(
            key,
            "post_id"
                | "media_id"
                | "shortcode"
                | "username"
                | "caption"
                | "caption_html"
                | "timestamp"
                | "index"
                | "ajax"
                | "mypixiv"
                | "user"
                | "profile"
                | "profile_publicity"
                | "workspace"
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
                | "category"
                | "subcategory"
                | "rating"
                | "score"
                | "pool"
                | "favgroup"
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
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn collection_roots_are_filtered_before_flattened_post_serialization() {
        let raw = json!({"title":"Novel","custom":{"public":true},"pool":{"id":99},"favgroup":{"id":100}});
        let captured = NovelMetadata::parse(raw.as_object().unwrap()).unwrap();
        for key in ["pool", "favgroup"] {
            assert!(!captured.fields().contains_key(key));
            assert!(
                !NovelMetadata::queue_fields(raw.as_object().unwrap())
                    .unwrap()
                    .contains_key(key)
            );
        }
        let metadata = crate::extract::PostMetadata {
            pixiv_novel_metadata: Some(std::sync::Arc::new(captured)),
            pool: Some(std::sync::Arc::new(json!({"id":7}))),
            favgroup: Some(std::sync::Arc::new(json!({"id":8}))),
            ..Default::default()
        };
        // JSON-to-Value would hide duplicate object keys; inspect actual bytes.
        let encoded = serde_json::to_string(&metadata).unwrap();
        for key in ["pool", "favgroup"] {
            assert_eq!(
                encoded.matches(&format!("\"{key}\":")).count(),
                1,
                "{encoded}"
            );
        }
        let decoded: Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded["pool"]["id"], 7);
        assert_eq!(decoded["favgroup"]["id"], 8);
        assert_eq!(decoded["custom"]["public"], true);
    }
}
