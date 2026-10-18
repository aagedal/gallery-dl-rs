//! Descriptive file properties, independent of private request and archive identity.
use serde::Serialize;
use serde_json::{Map, Value};
use std::sync::Arc;

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct FileMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width_original: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height_original: Option<u64>,
    /// Seconds, as declared by the extractor response, without probing the file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bitrate: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_type: Option<Arc<str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<Arc<str>>,
    #[serde(skip_serializing_if = "no_flags")]
    pub sensitive_flags: Arc<[Arc<str>]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_username: Option<Arc<str>>,
}

fn no_flags(flags: &Arc<[Arc<str>]>) -> bool {
    flags.is_empty()
}

pub(crate) fn compatibility(data: &Map<String, Value>) -> Option<FileMetadata> {
    let positive = |key: &str| data.get(key).and_then(Value::as_u64).filter(|v| *v > 0);
    let media_type = data
        .get("type")
        .and_then(Value::as_str)
        .and_then(|kind| match kind {
            "photo" | "image" | "video" | "animated_gif" | "preview" | "audio" | "document"
            | "article:cover" | "article:image" => Some(kind),
            "GraphImage" => Some("image"),
            "GraphVideo" => Some("video"),
            _ => None,
        })
        .map(Arc::from);
    let source_id = data
        .get("source_id")
        .and_then(|value| match value {
            Value::String(value) => Some(value.clone()),
            Value::Number(value) if value.is_u64() => Some(value.to_string()),
            _ => None,
        })
        .filter(|id| {
            !id.is_empty()
                && id.len() <= 32
                && !id.starts_with('0')
                && id.bytes().all(|b| b.is_ascii_digit())
        });
    let metadata = FileMetadata {
        width: positive("width").or_else(|| positive("image_width")),
        height: positive("height").or_else(|| positive("image_height")),
        width_original: positive("width_original"),
        height_original: positive("height_original"),
        duration: data
            .get("duration")
            .and_then(Value::as_f64)
            .filter(|value| value.is_finite() && *value >= 0.0),
        bitrate: data.get("bitrate").and_then(Value::as_u64),
        media_type,
        description: data
            .get("description")
            .and_then(Value::as_str)
            .map(Arc::from),
        sensitive_flags: data
            .get("sensitive_flags")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|flag| matches!(*flag, "Nudity" | "Sensitive" | "Violence"))
            .map(Arc::from)
            .collect(),
        source_id,
        source_username: data
            .get("source_user")
            .and_then(|user| user.get("name"))
            .and_then(Value::as_str)
            .map(Arc::from),
    };
    (metadata != FileMetadata::default()).then_some(metadata)
}
