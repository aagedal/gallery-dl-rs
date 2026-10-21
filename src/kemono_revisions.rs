//! Bounded revision normalization and Python-compatible revision identities.
use super::{Identity, MAX_POST_FILES, MAX_RESPONSE, post_identity};
use crate::{Result, error};
use serde::Serialize;
use serde_json::Value;
use sha1::{Digest, Sha1};
use std::{collections::HashSet, sync::Arc};

pub(super) const MAX_REVISIONS: usize = 1000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RevisionMode {
    #[default]
    None,
    All,
    Unique,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RevisionOrder {
    #[default]
    Newest,
    Oldest,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct RevisionMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hash: Option<Arc<str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<usize>,
}

pub(super) struct RevisionPost {
    pub post: Value,
    pub metadata: Option<RevisionMetadata>,
}

fn invalid() -> crate::Error {
    error("Kemono returned invalid revision data")
}

pub(super) fn embedded(body: &Value) -> Result<Option<Vec<Value>>> {
    let Some(values) = body.get("props").and_then(|props| props.get("revisions")) else {
        return Ok(None);
    };
    let values = values.as_array().ok_or_else(invalid)?;
    if values.len() > MAX_REVISIONS {
        return Err(error("Kemono revision count exceeds limit"));
    }
    let mut revisions = Vec::new();
    for entry in values {
        let revision = entry
            .as_array()
            .and_then(|entry| entry.get(1))
            .ok_or_else(invalid)?;
        if revision.get("revision_id").is_some() {
            revisions.push(revision.clone());
        }
    }
    Ok(Some(revisions))
}

pub(super) fn response(bytes: &[u8]) -> Result<Vec<Value>> {
    if bytes.len() > MAX_RESPONSE {
        return Err(error("Kemono revision response exceeds limit"));
    }
    let body: Value = serde_json::from_slice(bytes).map_err(|_| invalid())?;
    let Value::Array(revisions) = body else {
        return Err(invalid());
    };
    if revisions.len() > MAX_REVISIONS {
        return Err(error("Kemono revision count exceeds limit"));
    }
    Ok(revisions)
}

pub(super) fn normalize(mut post: Value) -> Result<Value> {
    let object = post.as_object_mut().ok_or_else(invalid)?;
    if let Some(file) = object.get_mut("file") {
        if let Value::String(encoded) = file {
            if encoded.len() > MAX_RESPONSE {
                return Err(invalid());
            }
            *file = serde_json::from_str(encoded).map_err(|_| invalid())?;
        }
        if !file.is_object() && !file.is_null() {
            return Err(invalid());
        }
    }
    if let Some(attachments) = object.get_mut("attachments")
        && !attachments.is_null()
    {
        let attachments = attachments.as_array_mut().ok_or_else(invalid)?;
        if attachments.len() > MAX_POST_FILES {
            return Err(error("Kemono revision file count exceeds limit"));
        }
        for attachment in attachments {
            if let Value::String(encoded) = attachment {
                if encoded.len() > MAX_RESPONSE {
                    return Err(invalid());
                }
                *attachment = serde_json::from_str(encoded).map_err(|_| invalid())?;
            }
            if !attachment.is_object() {
                return Err(invalid());
            }
        }
    }
    Ok(post)
}

fn revision_id(post: &Value) -> Result<u64> {
    match post.get("revision_id") {
        Some(Value::Number(number)) => number.as_u64().filter(|id| *id > 0).ok_or_else(invalid),
        Some(Value::String(id))
            if !id.is_empty() && id.len() <= 20 && id.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            id.parse::<u64>()
                .ok()
                .filter(|id| *id > 0)
                .ok_or_else(invalid)
        }
        _ => Err(invalid()),
    }
}

/// Canonical JSON uses Python's float spelling and Unicode key ordering.
fn canonical(value: &Value, output: &mut String, depth: usize) -> Result<()> {
    if depth > 128 || output.len() > MAX_RESPONSE * 2 {
        return Err(invalid());
    }
    match value {
        Value::Null => output.push_str("null"),
        Value::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
        Value::Number(value) => {
            let lexical = value.to_string();
            if lexical.contains(['.', 'e', 'E']) {
                let number = value
                    .as_f64()
                    .filter(|number| number.is_finite())
                    .ok_or_else(invalid)?;
                output.push_str(&crate::filter::float_repr(number));
            } else {
                output.push_str(if lexical == "-0" { "0" } else { &lexical });
            }
        }
        Value::String(value) => {
            output.push_str(&serde_json::to_string(value).map_err(|_| invalid())?)
        }
        Value::Array(values) => {
            output.push('[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                canonical(value, output, depth + 1)?;
            }
            output.push(']');
        }
        Value::Object(values) => {
            output.push('{');
            let mut keys: Vec<_> = values.keys().collect();
            keys.sort_unstable();
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                output.push_str(&serde_json::to_string(key).map_err(|_| invalid())?);
                output.push(':');
                canonical(&values[key], output, depth + 1)?;
            }
            output.push('}');
        }
    }
    if output.len() > MAX_RESPONSE * 2 {
        return Err(invalid());
    }
    Ok(())
}

pub(super) fn hash(post: &Value) -> Result<Arc<str>> {
    let mut post = post.clone();
    let object = post.as_object_mut().ok_or_else(invalid)?;
    for key in ["revision_id", "added", "next", "prev"] {
        object.remove(key);
    }
    if let Some(file) = object.get_mut("file").and_then(Value::as_object_mut) {
        file.remove("name");
    }
    if let Some(attachments) = object.get_mut("attachments").and_then(Value::as_array_mut) {
        for file in attachments {
            if let Some(file) = file.as_object_mut() {
                file.remove("name");
            }
        }
    }
    let mut encoded = String::new();
    canonical(&post, &mut encoded, 0)?;
    Ok(Arc::from(format!("{:x}", Sha1::digest(encoded.as_bytes()))))
}

pub(super) fn prepare(
    current: Value,
    historical: Vec<Value>,
    identity: &Identity,
    mode: RevisionMode,
    order: RevisionOrder,
    explicit: Option<Option<u64>>,
) -> Result<Vec<RevisionPost>> {
    if historical.len() > MAX_REVISIONS {
        return Err(error("Kemono revision count exceeds limit"));
    }
    let mut posts = Vec::with_capacity(historical.len() + usize::from(explicit.is_none()));
    if explicit.is_none() {
        posts.push((0, normalize(current)?));
    }
    let mut seen_ids = HashSet::new();
    for post in historical {
        post_identity(
            &post,
            Some((&identity.service, &identity.user)),
            Some(&identity.id),
        )?;
        let id = revision_id(&post)?;
        if !seen_ids.insert(id) {
            return Err(error("Kemono returned duplicate revision IDs"));
        }
        posts.push((id, normalize(post)?));
    }
    let mut result = Vec::with_capacity(posts.len());
    let mut last: Option<Arc<str>> = None;
    for (id, post) in posts {
        let hash = hash(&post)?;
        if explicit.is_none() && mode == RevisionMode::Unique && last.as_ref() == Some(&hash) {
            continue;
        }
        last = Some(hash.clone());
        result.push(RevisionPost {
            post,
            metadata: Some(RevisionMetadata {
                id: Some(id),
                hash: Some(hash),
                ..RevisionMetadata::default()
            }),
        });
    }
    let count = result.len();
    for (position, post) in result.iter_mut().enumerate() {
        let metadata = post.metadata.as_mut().expect("revision metadata");
        metadata.index = Some(count - position);
        metadata.count = Some(count);
    }
    if order == RevisionOrder::Oldest {
        result.reverse();
    }
    if let Some(Some(requested)) = explicit {
        result.retain(|post| {
            post.metadata.as_ref().and_then(|metadata| metadata.id) == Some(requested)
        });
        if result.is_empty() {
            return Err(error("Kemono revision not found"));
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn identity() -> Identity {
        Identity {
            service: "patreon".into(),
            user: "42".into(),
            id: "7".into(),
        }
    }
    fn current() -> Value {
        json!({"id":"7","service":"patreon","user":"42","title":"same",
            "file":{"path":"/aa/11/a.png","name":"a.png"},"attachments":[],"added":"today"})
    }
    fn historical(id: u64, title: &str) -> Value {
        let mut post = current();
        post["revision_id"] = json!(id);
        post["title"] = json!(title);
        post["file"]["name"] = json!("renamed.png");
        post["added"] = json!("yesterday");
        post
    }

    #[test]
    fn canonical_hash_matches_python_unicode_and_float_reference_vector() {
        // Python json.dumps(ensure_ascii=False, check_circular=False,
        // sort_keys=True, separators=(',', ':')), then hashlib.sha1(...).
        let post = json!({"id":"7","service":"patreon","user":"42","title":"猫 café\n☃",
            "published":"2024-01-01T00:00:00","file":{"path":"/aa/11/a.png","name":"drop.jpg"},
            "attachments":[{"path":"/bb/22/b.jpg","name":"drop.png"}],"revision_id":27,
            "added":"ignore","next":{"secret":1},"prev":"ignore",
            "z":{"β":-0.0,"a":1e-7,"b":1e20,"c":1.0,"d":1e16}});
        assert_eq!(
            hash(&post).unwrap().as_ref(),
            "cedb78717fcee90ce582ecaa48bc6fc915dd28fb"
        );
        let mut renamed = post.clone();
        renamed["file"]["name"] = json!("different");
        renamed["attachments"][0]["name"] = json!("different");
        renamed["revision_id"] = json!(99);
        renamed["added"] = Value::Null;
        assert_eq!(hash(&post).unwrap(), hash(&renamed).unwrap());
        renamed["file"]["path"] = json!("/changed.png");
        assert_ne!(hash(&post).unwrap(), hash(&renamed).unwrap());
    }

    #[test]
    fn canonical_hash_preserves_python_big_integers_and_extreme_exponents() {
        let post: Value = serde_json::from_str(r#"{"id":"7","file":{},"attachments":[],"metrics":{"huge":1234567890123456789012345678901234567890,"negative":-1234567890123456789012345678901234567890,"tiny":1e-300,"large":1e300,"unicode":"𝄞\u2028"}}"#).unwrap();
        assert_eq!(
            hash(&post).unwrap().as_ref(),
            "4dcf503a1c830516af5f35656c6e461e0ee6342f"
        );
    }

    #[test]
    fn canonical_numbers_normalize_negative_integer_zero_and_reject_float_overflow() {
        let post: Value =
            serde_json::from_str(r#"{"id":"7","file":{},"attachments":[],"n":-0,"f":1e-7}"#)
                .unwrap();
        assert_eq!(
            hash(&post).unwrap().as_ref(),
            "4257c754907b4ddfc9413d3b9dd994f1cd6bb2de"
        );
        for exponent in ["1e400", "-1e400"] {
            let post: Value = serde_json::from_str(&format!(
                r#"{{"id":"7","file":{{}},"attachments":[],"private_metric":{exponent}}}"#
            ))
            .unwrap();
            assert_eq!(
                hash(&post).unwrap_err().to_string(),
                "Kemono returned invalid revision data"
            );
        }
    }

    #[test]
    fn encoded_file_and_attachment_objects_normalize_before_hashing() {
        let mut post = current();
        post["attachments"] = json!([{"path":"/bb/22/b.png","name":"b.png"}]);
        let mut encoded = post.clone();
        encoded["file"] = json!(post["file"].to_string());
        encoded["attachments"] = json!([post["attachments"][0].to_string()]);
        let decoded = normalize(encoded).unwrap();
        assert_eq!(hash(&decoded).unwrap(), hash(&post).unwrap());
        assert_eq!(decoded, post);
        for bad in [
            json!({"file":"secret-invalid-json"}),
            json!({"file":"[]"}),
            json!({"attachments":["secret-invalid-json"]}),
        ] {
            let error = normalize(bad).unwrap_err().to_string();
            assert!(!error.contains("secret"));
            assert_eq!(error, "Kemono returned invalid revision data");
        }
    }

    #[test]
    fn unique_collapses_adjacent_hashes_before_indices_and_reverse() {
        let history = || {
            vec![
                historical(4, "same"),
                historical(3, "older"),
                historical(2, "older"),
                historical(1, "same"),
            ]
        };
        let revisions = prepare(
            current(),
            history(),
            &identity(),
            RevisionMode::Unique,
            RevisionOrder::Newest,
            None,
        )
        .unwrap();
        let fields: Vec<_> = revisions
            .iter()
            .map(|post| {
                let metadata = post.metadata.as_ref().unwrap();
                (metadata.id, metadata.index, metadata.count)
            })
            .collect();
        assert_eq!(
            fields,
            [
                (Some(0), Some(3), Some(3)),
                (Some(3), Some(2), Some(3)),
                (Some(1), Some(1), Some(3))
            ]
        );
        let revisions = prepare(
            current(),
            history(),
            &identity(),
            RevisionMode::Unique,
            RevisionOrder::Oldest,
            None,
        )
        .unwrap();
        assert_eq!(
            revisions
                .iter()
                .map(|post| post.metadata.as_ref().unwrap().id.unwrap())
                .collect::<Vec<_>>(),
            [1, 3, 0]
        );
        assert_eq!(revisions[0].metadata.as_ref().unwrap().index, Some(1));
        let explicit = prepare(
            current(),
            history(),
            &identity(),
            RevisionMode::Unique,
            RevisionOrder::Oldest,
            Some(Some(2)),
        )
        .unwrap();
        assert_eq!(explicit.len(), 1);
        let metadata = explicit[0].metadata.as_ref().unwrap();
        assert_eq!(
            (metadata.id, metadata.index, metadata.count),
            (Some(2), Some(2), Some(4))
        );
    }

    #[test]
    fn embedded_revisions_use_tuple_payloads_and_bounded_strict_identities() {
        let body = json!({"post":current(),"props":{"revisions":[["ignored", historical(1,"old")],[0,{"id":"without-revision-id"}]]}});
        let revisions = embedded(&body).unwrap().unwrap();
        assert_eq!(revisions.len(), 1);
        assert_eq!(revisions[0]["revision_id"], 1);
        assert!(
            embedded(&json!({"props":{"revisions":[]}}))
                .unwrap()
                .unwrap()
                .is_empty()
        );
        assert!(embedded(&json!({"props":{}})).unwrap().is_none());
        assert!(
            response(
                &json!(vec![historical(1, "old"); MAX_REVISIONS + 1])
                    .to_string()
                    .into_bytes()
            )
            .is_err()
        );
        for field in ["service", "user", "id"] {
            let mut post = historical(1, "old");
            post[field] = json!("private-invalid");
            let error = prepare(
                current(),
                vec![post],
                &identity(),
                RevisionMode::All,
                RevisionOrder::Newest,
                None,
            )
            .err()
            .unwrap()
            .to_string();
            assert!(!error.contains("private"));
            assert!(error.contains("identity"));
        }
        assert!(
            prepare(
                current(),
                vec![historical(1, "a"), historical(1, "b")],
                &identity(),
                RevisionMode::All,
                RevisionOrder::Newest,
                None
            )
            .is_err()
        );
    }
}
