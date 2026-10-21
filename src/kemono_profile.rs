//! Creator profile data is descriptive; IDs remain separate from display names.
use super::{id_of, valid_id, valid_service};
use crate::{Result, error};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};

const MAX_TEXT: usize = 64 * 1024;
const MAX_PROFILE: usize = 1024 * 1024;
pub(super) const MAX_CREATORS: usize = 1024;

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct CreatorMetadata {
    pub id: String,
    pub service: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<Arc<str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_id: Option<Arc<str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indexed: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_chats: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub post_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dm_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub share_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat_count: Option<u64>,
}
impl CreatorMetadata {
    pub(super) fn stub(service: &str, user: &str) -> Self {
        Self {
            id: user.to_owned(),
            service: service.to_owned(),
            ..Self::default()
        }
    }
}

pub fn parse_creator_profile(bytes: &[u8], service: &str, user: &str) -> Result<CreatorMetadata> {
    let invalid = || error("Kemono returned invalid creator profile");
    if bytes.len() > MAX_PROFILE || !valid_service(service) || !valid_id(user) {
        return Err(invalid());
    }
    let body: Value = serde_json::from_slice(bytes).map_err(|_| invalid())?;
    parse_profile_value(&body, service, user).ok_or_else(invalid)
}

pub(crate) fn parse_profile_value(
    body: &Value,
    service: &str,
    user: &str,
) -> Option<CreatorMetadata> {
    if !valid_service(service) || !valid_id(user) {
        return None;
    }
    if !body.is_object()
        || id_of(body, "id").as_deref() != Some(user)
        || body.get("service").and_then(Value::as_str) != Some(service)
    {
        return None;
    }
    let text = |key| {
        body.get(key)
            .and_then(Value::as_str)
            .filter(|value| {
                !value.is_empty() && value.len() <= MAX_TEXT && !value.chars().any(char::is_control)
            })
            .map(Arc::from)
    };
    let count = |key| body.get(key).and_then(Value::as_u64);
    let timestamp = |key| {
        body.get(key).and_then(|value| {
            value.as_i64().or_else(|| {
                value
                    .as_str()
                    .filter(|value| value.len() <= 128)
                    .and_then(|value| crate::date::parse_date(value).ok())
            })
        })
    };
    Some(CreatorMetadata {
        id: user.to_owned(),
        service: service.to_owned(),
        name: text("name"),
        public_id: text("public_id"),
        indexed: timestamp("indexed"),
        updated: timestamp("updated"),
        relation_id: body
            .get("relation_id")
            .and_then(Value::as_str)
            .filter(|value| valid_id(value))
            .map(str::to_owned),
        has_chats: body.get("has_chats").and_then(Value::as_bool),
        post_count: count("post_count"),
        dm_count: count("dm_count"),
        share_count: count("share_count"),
        chat_count: count("chat_count"),
    })
}

pub(super) type CacheKey = (String, String, String);
#[derive(Default)]
pub(super) struct Cache {
    entries: HashMap<CacheKey, Arc<CreatorMetadata>>,
    order: VecDeque<CacheKey>,
}
impl Cache {
    pub fn get(&self, key: &CacheKey) -> Option<Arc<CreatorMetadata>> {
        self.entries.get(key).cloned()
    }
    pub fn insert(&mut self, key: CacheKey, value: Arc<CreatorMetadata>) {
        if !self.entries.contains_key(&key) {
            if self.entries.len() >= MAX_CREATORS
                && let Some(oldest) = self.order.pop_front()
            {
                self.entries.remove(&oldest);
            }
            self.order.push_back(key.clone());
        }
        self.entries.insert(key, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn direct_profile_keeps_typed_recognized_fields_and_immutable_ids() {
        let body = json!({"id":"42","service":"patreon","name":"Display 猫","public_id":"public-handle",
            "indexed":1600000000,"updated":1700000000,"relation_id":"relation-1","has_chats":false,
            "post_count":4,"dm_count":0,"share_count":5,"chat_count":6,"unknown_secret":"private-value"});
        let profile = parse_creator_profile(body.to_string().as_bytes(), "patreon", "42").unwrap();
        assert_eq!(profile.id, "42");
        assert_eq!(profile.service, "patreon");
        assert_eq!(profile.name.as_deref(), Some("Display 猫"));
        assert_eq!(profile.public_id.as_deref(), Some("public-handle"));
        assert_eq!(
            (profile.indexed, profile.updated),
            (Some(1600000000), Some(1700000000))
        );
        assert_eq!(profile.relation_id.as_deref(), Some("relation-1"));
        assert_eq!(profile.has_chats, Some(false));
        assert_eq!(
            (
                profile.post_count,
                profile.dm_count,
                profile.share_count,
                profile.chat_count
            ),
            (Some(4), Some(0), Some(5), Some(6))
        );
        let public = serde_json::to_string(&profile).unwrap();
        assert!(!public.contains("private"));
        assert!(!public.contains("unknown_secret"));
    }

    #[test]
    fn profile_timestamps_accept_actual_string_dates_and_preserve_display_name_spaces() {
        let body = json!({"id":"42","service":"patreon","name":"  Display Name  ",
            "indexed":"2024-01-02T03:04:05","updated":"2024-01-02T04:04:05+01:00"});
        let profile = parse_creator_profile(body.to_string().as_bytes(), "patreon", "42").unwrap();
        assert_eq!(profile.name.as_deref(), Some("  Display Name  "));
        assert_eq!(profile.indexed, Some(1704164645));
        assert_eq!(profile.updated, Some(1704164645));
    }

    #[test]
    fn malformed_optional_fields_are_omitted_without_losing_identity() {
        let body = json!({"id":"42","service":"patreon","name":"private\nname","public_id":"p".repeat(MAX_TEXT+1),
            "indexed":"private-invalid-date","updated":1.5,"relation_id":null,"has_chats":"true",
            "post_count":-1,"dm_count":"4","share_count":1.5,"chat_count":null});
        let profile = parse_creator_profile(body.to_string().as_bytes(), "patreon", "42").unwrap();
        assert_eq!(profile, CreatorMetadata::stub("patreon", "42"));
        assert_eq!(
            serde_json::to_value(&profile).unwrap(),
            json!({"id":"42","service":"patreon"})
        );
        let name = "n".repeat(MAX_TEXT);
        let body = json!({"id":42,"service":"patreon","name":name});
        assert_eq!(
            parse_creator_profile(body.to_string().as_bytes(), "patreon", "42")
                .unwrap()
                .name
                .as_deref(),
            Some(name.as_str())
        );
    }

    #[test]
    fn profiles_require_direct_matching_identity_and_private_bounded_input_errors() {
        for body in [
            json!({"id":"private-other","service":"patreon","name":"wrong"}),
            json!({"id":"42","service":"private-service"}),
            json!({"id":"42"}),
            json!({"service":"patreon"}),
            json!({"profile":{"id":"42","service":"patreon"}}),
            json!([]),
            json!({"id":42.0,"service":"patreon"}),
        ] {
            assert!(parse_profile_value(&body, "patreon", "42").is_none());
            assert_eq!(
                parse_creator_profile(body.to_string().as_bytes(), "patreon", "42")
                    .unwrap_err()
                    .to_string(),
                "Kemono returned invalid creator profile"
            );
        }
        let big =
            json!({"id":"42","service":"patreon","private":"x".repeat(MAX_PROFILE)}).to_string();
        assert_eq!(
            parse_creator_profile(big.as_bytes(), "patreon", "42")
                .unwrap_err()
                .to_string(),
            "Kemono returned invalid creator profile"
        );
    }

    #[test]
    fn bounded_cache_evicts_oldest_creator_and_shares_success_or_failure_values() {
        let mut cache = Cache::default();
        for index in 0..=MAX_CREATORS {
            let id = index.to_string();
            let key = ("kemono.cr".into(), "patreon".into(), id.clone());
            cache.insert(key, Arc::new(CreatorMetadata::stub("patreon", &id)));
        }
        assert_eq!(cache.entries.len(), MAX_CREATORS);
        assert_eq!(cache.order.len(), MAX_CREATORS);
        assert!(
            cache
                .get(&("kemono.cr".into(), "patreon".into(), "0".into()))
                .is_none()
        );
        let key = (
            "kemono.cr".into(),
            "patreon".into(),
            MAX_CREATORS.to_string(),
        );
        let first = cache.get(&key).unwrap();
        let second = cache.get(&key).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        cache.insert(key, first);
        assert_eq!(cache.entries.len(), MAX_CREATORS);
        assert_eq!(cache.order.len(), MAX_CREATORS);
    }
}
