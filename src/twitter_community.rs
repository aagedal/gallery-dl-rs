//! Public community metadata shared by native GraphQL and compatibility jobs.
use super::*;
use std::sync::Arc;

#[derive(Clone, Debug, serde::Serialize)]
pub struct CommunityMetadata {
    pub id: String,
    pub name: Option<Arc<str>>,
    pub description: Option<Arc<str>>,
    pub date: Option<i64>,
    pub nsfw: Option<bool>,
    pub role: Option<Arc<str>>,
    pub member_count: Option<u64>,
    pub rules: Arc<[Arc<str>]>,
    pub admin: Option<Arc<str>>,
    pub creator: Option<Arc<str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub banner: Option<Arc<str>>,
}

fn id(value: &Value) -> Option<String> {
    let id = match value {
        Value::String(value) => value.clone(),
        Value::Number(value) if value.is_u64() => value.to_string(),
        _ => return None,
    };
    valid_id(&id).then_some(id)
}
fn string(value: Option<&Value>) -> Option<Arc<str>> {
    value.and_then(Value::as_str).map(Arc::from)
}
fn banner(value: Option<&Value>) -> Option<Arc<str>> {
    let value = value?.as_str()?;
    crate::extract::http_url(value).ok()?;
    Some(Arc::from(value))
}

pub(super) fn native(tweet: &Value) -> Option<CommunityMetadata> {
    let data = tweet.pointer("/author_community_relationship/community_results/result")?;
    let identity = data
        .get("id_str")
        .or_else(|| data.get("rest_id"))
        .and_then(id)?;
    for key in ["id_str", "rest_id"] {
        if let Some(value) = data.get(key)
            && id(value).as_deref() != Some(identity.as_str())
        {
            return None;
        }
    }
    Some(CommunityMetadata {
        id: identity,
        name: string(data.get("name")),
        description: string(data.get("description")),
        date: data
            .get("created_at")
            .and_then(Value::as_u64)
            .and_then(|ms| i64::try_from(ms / 1000).ok()),
        nsfw: data.get("is_nsfw").and_then(Value::as_bool),
        role: string(data.get("role")),
        member_count: data.get("member_count").and_then(Value::as_u64),
        rules: data
            .get("rules")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|rule| string(rule.get("name")))
            .collect(),
        admin: string(data.pointer("/admin_results/result/core/screen_name")),
        creator: string(data.pointer("/creator_results/result/core/screen_name")),
        banner: banner(data.pointer("/custom_banner_media/media_info/original_img_url")),
    })
}

pub(crate) fn compatibility(data: &Value) -> Option<CommunityMetadata> {
    Some(CommunityMetadata {
        id: id(data.get("id")?)?,
        name: string(data.get("name")),
        description: string(data.get("description")),
        date: data.get("date").and_then(|date| match date {
            Value::String(value) => crate::date::parse_date(value).ok(),
            value => value.as_i64(),
        }),
        nsfw: data.get("nsfw").and_then(Value::as_bool),
        role: string(data.get("role")),
        member_count: data.get("member_count").and_then(Value::as_u64),
        rules: data
            .get("rules")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|rule| string(Some(rule)))
            .collect(),
        admin: string(data.get("admin")),
        creator: string(data.get("creator")),
        banner: banner(data.get("banner")),
    })
}
