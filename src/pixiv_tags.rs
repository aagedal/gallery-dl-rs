//! Bounded Pixiv API tag arrays and the source extractor's tag transforms.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeSet, sync::Arc};

const MAX_COUNT: usize = 4096;
const MAX_NODES: usize = 16384;
const MAX_BYTES: usize = 1024 * 1024;
const MAX_STRING: usize = 64 * 1024;
const MAX_DEPTH: usize = 16;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TagMode {
    #[default]
    Japanese,
    Original,
    Translated,
}
impl TagMode {
    pub fn parse(value: &str) -> Self {
        match value {
            "original" => Self::Original,
            "translated" => Self::Translated,
            _ => Self::Japanese,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Japanese => "japanese",
            Self::Original => "original",
            Self::Translated => "translated",
        }
    }
    pub(super) fn transform(self, value: &Value) -> Option<Tags> {
        let raw = value.as_array()?;
        if !bounded(raw) {
            return None;
        }
        if self == Self::Original {
            return Some(Tags(Arc::from(raw.clone())));
        }
        let mut seen = BTreeSet::new();
        let mut output = Vec::with_capacity(raw.len());
        for tag in raw {
            let name = if self == Self::Translated {
                let translated = tag.get("translated_name")?;
                if super::value_truthy(translated) {
                    translated
                } else {
                    tag.get("name")?
                }
            } else {
                tag.get("name")?
            };
            let name = name.as_str()?;
            if self != Self::Translated || seen.insert(name) {
                output.push(Value::String(name.to_owned()));
            }
        }
        Some(Tags(output.into()))
    }
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct Tags(Arc<[Value]>);
impl<'de> Deserialize<'de> for Tags {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        Self::parse_compat(&value)
            .ok_or_else(|| serde::de::Error::custom("invalid or oversized Pixiv tags"))
    }
}
impl Tags {
    pub fn values(&self) -> &[Value] {
        &self.0
    }
    /// Compatibility data is already transformed by upstream; retain it exactly.
    pub fn parse_compat(value: &Value) -> Option<Self> {
        let values = value.as_array()?;
        bounded(values).then(|| Self(Arc::from(values.clone())))
    }
}
fn bounded(values: &[Value]) -> bool {
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
    values.len() <= MAX_COUNT
        && values
            .iter()
            .all(|value| visit(value, 0, &mut nodes, &mut bytes))
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
    #[test]
    fn recorded_upstream_tag_modes_match_artwork_and_novel_oracles() {
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/pixiv/tags.json")).unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            let mode = TagMode::parse(case["mode"].as_str().unwrap_or("japanese"));
            let result = mode.transform(&case["input_tags"]);
            for kind in ["illustration", "novel"] {
                let expected = &case["expected"][kind];
                if expected["error"].is_null() {
                    assert_eq!(
                        serde_json::to_value(result.as_ref().unwrap()).unwrap(),
                        expected["tags"],
                        "{} {kind}",
                        case["name"]
                    );
                } else {
                    assert!(result.is_none(), "{} {kind}", case["name"]);
                }
            }
        }
    }
    #[test]
    fn numeric_values_fit_filter_representation_without_precision_loss() {
        let max = i128::MAX.to_string();
        let exact: Value = serde_json::from_str(&format!("[{max}]")).unwrap();
        let tags = Tags::parse_compat(&exact).unwrap();
        assert_eq!(serde_json::to_string(&tags).unwrap(), format!("[{max}]"));
        assert!(serde_json::from_value::<Tags>(exact).is_ok());
        for numeric in [
            "170141183460469231731687303715884105728",
            "-170141183460469231731687303715884105729",
            "1e999",
            "-1.5e999",
        ] {
            let value: Value = serde_json::from_str(&format!("[{numeric}]")).unwrap();
            assert!(Tags::parse_compat(&value).is_none(), "{numeric}");
            assert!(TagMode::Original.transform(&value).is_none(), "{numeric}");
            assert!(serde_json::from_value::<Tags>(value).is_err(), "{numeric}");
        }
        let normal = json!([
            1.25,
            -2e30,
            "170141183460469231731687303715884105728",
            "1e999"
        ]);
        assert_eq!(
            serde_json::to_value(Tags::parse_compat(&normal).unwrap()).unwrap(),
            normal
        );
    }
    fn raw() -> Value {
        json!([{"name":"猫","translated_name":"cat","extra":{"nested":[null,true,17]}},{"name":"猫","translated_name":""},{"name":"cat","translated_name":null},{"name":"猫","translated_name":"cat"}])
    }
    #[test]
    fn mode_transforms_preserve_original_and_bound_all_compatibility_json_values() {
        assert_eq!(TagMode::default(), TagMode::Japanese);
        for value in ["unknown", "Original", "TRANSLATED", ""] {
            assert_eq!(TagMode::parse(value), TagMode::Japanese);
        }
        for mode in [TagMode::Japanese, TagMode::Original, TagMode::Translated] {
            assert_eq!(TagMode::parse(mode.as_str()), mode);
        }
        assert_eq!(
            serde_json::to_value(TagMode::Japanese.transform(&raw()).unwrap()).unwrap(),
            json!(["猫", "猫", "cat", "猫"])
        );
        assert_eq!(
            serde_json::to_value(TagMode::Translated.transform(&raw()).unwrap()).unwrap(),
            json!(["cat", "猫"])
        );
        let original = TagMode::Original.transform(&raw()).unwrap();
        assert_eq!(serde_json::to_value(original.clone()).unwrap(), raw());
        assert_eq!(Tags::parse_compat(&raw()), Some(original));
        let arbitrary = json!([null,true,4,["nested"],{"arbitrary":"retained"}]);
        assert_eq!(
            serde_json::to_value(TagMode::Original.transform(&arbitrary).unwrap()).unwrap(),
            arbitrary
        );
        assert!(
            TagMode::Translated
                .transform(&json!([{"name":"name"}]))
                .is_none()
        );
        assert_eq!(
            TagMode::Translated
                .transform(&json!([{"translated_name":"translated"}]))
                .unwrap()
                .values(),
            &[json!("translated")]
        );
        for fallback in [
            Value::Null,
            json!(""),
            json!(false),
            json!(0),
            json!([]),
            json!({}),
        ] {
            assert_eq!(
                TagMode::Translated
                    .transform(&json!([{"name":"fallback","translated_name":fallback}]))
                    .unwrap()
                    .values(),
                &[json!("fallback")]
            );
        }
        assert!(
            TagMode::Japanese
                .transform(&json!([]))
                .unwrap()
                .values()
                .is_empty()
        );
        assert!(serde_json::from_value::<Tags>(json!(["x".repeat(MAX_STRING + 1)])).is_err());
        let numeric: Value =
            serde_json::from_str(&format!("[{}]", "9".repeat(MAX_STRING + 1))).unwrap();
        assert!(Tags::parse_compat(&numeric).is_none());
        assert!(serde_json::from_value::<Tags>(numeric).is_err());
        assert!(Tags::parse_compat(&json!(null)).is_none());
        assert!(Tags::parse_compat(&json!(["x".repeat(MAX_STRING + 1)])).is_none());
        assert!(Tags::parse_compat(&Value::Array(vec![Value::Null; MAX_COUNT + 1])).is_none());
        assert!(Tags::parse_compat(&json!(vec![vec![Value::Null; 4]; 4000])).is_none());
        let mut deep = json!(null);
        for _ in 0..=MAX_DEPTH {
            deep = json!([deep]);
        }
        assert!(Tags::parse_compat(&json!([deep])).is_none());
        assert!(
            Tags::parse_compat(&json!(vec![
                "x".repeat(MAX_STRING);
                MAX_BYTES / MAX_STRING + 1
            ]))
            .is_none()
        );
    }
    fn work(kind: &str) -> Value {
        json!({"id":42,"type":kind,"user":{"id":5,"account":"artist"},"tags":raw(),"meta_single_page":{"original_image_url":"https://i.pximg.net/img-original/42_p0.jpg"},"meta_pages":[]})
    }

    #[tokio::test]
    async fn native_detail_modes_and_absent_versus_empty_tags_are_visible_on_posts_and_files() {
        for mode in [TagMode::Japanese, TagMode::Translated, TagMode::Original] {
            for input in [None, Some(json!([])), Some(raw())] {
                let mut data = work("illust");
                data.as_object_mut().unwrap().remove("tags");
                if let Some(value) = input.clone() {
                    data["tags"] = value;
                }
                let (root, server) = serve(vec![
                    token("t"),
                    response("200 OK", json!({"illust":data}).to_string().as_bytes()),
                ]);
                let native = pixiv(root).with_tags(mode);
                let events = native
                    .events(&Url::parse("https://www.pixiv.net/artworks/42").unwrap())
                    .collect::<Vec<_>>()
                    .await
                    .into_iter()
                    .collect::<crate::Result<Vec<_>>>()
                    .unwrap();
                let expected = input.as_ref().and_then(|value| mode.transform(value));
                assert_eq!(events.len(), 3);
                for event in events {
                    match event {
                        ExtractionEvent::Post(post) => {
                            assert_eq!(post.metadata.unwrap().tags, expected)
                        }
                        ExtractionEvent::Media(file) => {
                            assert_eq!(file.metadata.unwrap().tags, expected)
                        }
                        _ => {}
                    }
                }
                assert_eq!(server.join().unwrap().len(), 2);
            }
        }
    }

    #[tokio::test]
    async fn configured_translated_tags_survive_empty_animation_posts_in_every_feed_context() {
        for (route, series, article, owner) in [
            (
                "https://www.pixiv.net/ranking.php?date=20240101",
                false,
                false,
                false,
            ),
            ("https://www.pixiv.net/tags/cat", false, false, false),
            (
                "https://www.pixiv.net/user/999/series/7",
                true,
                false,
                false,
            ),
            (
                "https://www.pixiv.net/bookmark_new_illust.php",
                false,
                false,
                true,
            ),
            ("https://www.pixivision.net/a/77", false, true, false),
        ] {
            let mut replies = Vec::new();
            if article {
                replies.push(response(
                    "200 OK",
                    b"<title>Article</title><a href=\"https://www.pixiv.net/en/artworks/42\">",
                ));
            }
            replies.push(if owner {
                response("200 OK", br#"{"access_token":"t","user":{"id":17}}"#)
            } else {
                token("t")
            });
            let mut data = if article {
                json!({"illust":work("ugoira")})
            } else {
                json!({"illusts":[work("ugoira")],"next_url":null})
            };
            if series {
                data["illust_series_detail"] = json!({"id":7,"series_work_count":1});
            }
            replies.push(response("200 OK", data.to_string().as_bytes()));
            let (root, server) = serve(replies);
            let native = pixiv(root.clone())
                .with_pixivision_root(root)
                .with_ugoira(false)
                .with_tags(TagMode::Translated);
            let events = native
                .events(&Url::parse(route).unwrap())
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .collect::<crate::Result<Vec<_>>>()
                .unwrap();
            assert_eq!(events.len(), 2, "{route}");
            let ExtractionEvent::Post(post) = &events[0] else {
                panic!("missing empty post {route}")
            };
            assert_eq!(post.count, Some(0));
            assert_eq!(
                post.metadata
                    .as_ref()
                    .unwrap()
                    .tags
                    .as_ref()
                    .unwrap()
                    .values(),
                &[json!("cat"), json!("猫")]
            );
            assert_eq!(server.join().unwrap().len(), if article { 3 } else { 2 });
        }
    }
    #[tokio::test]
    async fn translated_tags_survive_failed_animation_enrichment_without_losing_post_frame() {
        let (root, server) = serve(vec![
            token("t"),
            response(
                "200 OK",
                json!({"illusts":[work("ugoira")],"next_url":null})
                    .to_string()
                    .as_bytes(),
            ),
            response("404 Not Found", b""),
        ]);
        let native = pixiv(root).with_tags(TagMode::Translated);
        let events = native
            .events(&Url::parse("https://www.pixiv.net/ranking.php?date=20240101").unwrap())
            .collect::<Vec<_>>()
            .await;
        assert_eq!(events.len(), 3);
        assert!(events[0].is_err());
        let ExtractionEvent::Post(post) = events[1].as_ref().unwrap() else {
            panic!("expected empty post")
        };
        assert_eq!(post.count, Some(0));
        assert_eq!(
            post.metadata
                .as_ref()
                .unwrap()
                .tags
                .as_ref()
                .unwrap()
                .values(),
            &[json!("cat"), json!("猫")]
        );
        assert!(matches!(events[2], Ok(ExtractionEvent::EndPost)));
        assert_eq!(server.join().unwrap().len(), 3);
    }
}
