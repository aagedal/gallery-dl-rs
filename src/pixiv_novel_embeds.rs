//! Lazy novel marker traversal and persistent uploaded-image metadata.
use super::Novel;
use crate::{
    Result, error,
    event::KeywordUpdate,
    extract::{Media, MediaJob},
    file_metadata::FileMetadata,
};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeSet, VecDeque},
    sync::Arc,
};
use url::Url;
const MAX_ENTRIES: usize = 100_000;
/// Forced work extractors match a decimal prefix rather than a whole URL path.
pub(super) fn artwork_id(raw: &str) -> Option<String> {
    let end = raw.bytes().take_while(u8::is_ascii_digit).count();
    let id = &raw[..end];
    (id.len() <= 20 && super::super::valid_id(id.trim_start_matches('0'))).then(|| id.to_owned())
}
#[derive(Default)]
pub(super) struct Plan {
    pub uploads: bool,
    pub ids: VecDeque<String>,
}
#[derive(Default)]
pub(super) struct Visited {
    ids: BTreeSet<String>,
    bytes: usize,
}
impl Visited {
    pub(super) fn insert(&mut self, id: &str) -> Result<bool> {
        if self.ids.contains(id) {
            return Ok(false);
        }
        let bytes = self.bytes.saturating_add(id.len()).saturating_add(40);
        if self.ids.len() >= MAX_ENTRIES || bytes > super::super::MAX_RESPONSE {
            return Err(error("Pixiv novel artwork queues exceed traversal limits"));
        }
        self.bytes = bytes;
        self.ids.insert(id.to_owned());
        Ok(true)
    }
}
pub(super) async fn scan(text: &str) -> Result<Plan> {
    let mut rest = text;
    let mut seen = BTreeSet::new();
    let mut plan = Plan::default();
    let mut count = 0usize;
    let mut bytes = 0usize;
    while let Some((_, tail)) = rest.split_once('[') {
        let Some((marker, tail)) = tail.split_once(']') else {
            break;
        };
        rest = tail;
        count += 1;
        if count.is_multiple_of(256) {
            tokio::task::yield_now().await;
        }
        if count > MAX_ENTRIES {
            return Err(error("Pixiv novel embeds exceed the marker limit"));
        }
        if marker.starts_with("uploadedimage:") {
            plan.uploads = true;
        } else if let Some(raw) = marker.strip_prefix("pixivimage:") {
            let raw = raw.split_once('-').map_or(raw, |(id, _)| id);
            bytes = bytes.saturating_add(raw.len());
            if bytes > super::super::MAX_RESPONSE {
                return Err(error("Pixiv novel artwork targets exceed the size limit"));
            }
            if seen.insert(raw.to_owned()) {
                plan.ids.push_back(raw.to_owned());
            }
        }
    }
    Ok(plan)
}
pub(super) fn images(mut body: Value) -> Option<VecDeque<Value>> {
    let values = body.get_mut("textEmbeddedImages")?.as_object_mut()?;
    (values.len() <= MAX_ENTRIES).then(|| {
        std::mem::take(values)
            .into_iter()
            .map(|(_, value)| value)
            .collect()
    })
}
fn date(timestamp: i64) -> Option<String> {
    let date = time::OffsetDateTime::from_unix_timestamp(timestamp).ok()?;
    Some(format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        date.year(),
        u8::from(date.month()),
        date.day(),
        date.hour(),
        date.minute(),
        date.second()
    ))
}
pub(super) fn url_fields(url: &Url) -> (String, String, Value) {
    let raw = super::super::web::unquote(url.path().rsplit('/').next().unwrap_or(""));
    let (filename, extension) = raw
        .rsplit_once('.')
        .filter(|(stem, extension)| !stem.is_empty() && extension.chars().count() <= 16)
        .map(|(stem, extension)| (stem.to_owned(), extension.to_lowercase()))
        .unwrap_or((raw, String::new()));
    let parts: Vec<_> = url.as_str().split('/').collect();
    let timestamp = (parts.len() == 12)
        .then(|| {
            format!(
                "{}-{}-{}T{}:{}:{}+09:00",
                parts[5], parts[6], parts[7], parts[8], parts[9], parts[10]
            )
        })
        .and_then(|raw| crate::date::parse_date(&raw).ok());
    (
        filename,
        extension,
        timestamp
            .and_then(date)
            .map(Value::String)
            .unwrap_or(Value::Null),
    )
}
impl Novel {
    pub(super) fn refresh_snapshot(&mut self) -> Result<()> {
        let fields = self
            .data
            .as_object()
            .ok_or_else(|| error("Pixiv returned invalid novel metadata"))?;
        self.metadata.pixiv_novel_metadata = super::super::NovelMetadata::parse_work(
            fields,
            self.metadata.pixiv_user_detail.as_deref(),
        )
        .map(Arc::new);
        if self.metadata.pixiv_novel_metadata.is_none() {
            return Err(error("Pixiv novel metadata exceeds public metadata limits"));
        }
        Ok(())
    }
    pub(super) fn prepare_source(&mut self) -> Result<()> {
        self.data["num"] = json!(0);
        self.data["suffix"] = json!("");
        self.data["date"] = self
            .metadata
            .timestamp
            .and_then(date)
            .map(Value::String)
            .unwrap_or(Value::Null);
        self.data["rating"] = match self.data.get("x_restrict").and_then(Value::as_u64) {
            Some(0) => json!("General"),
            Some(1) => json!("R-18"),
            Some(2) => json!("R-18G"),
            _ => Value::Null,
        };
        if let Some(tags) = &self.metadata.tags {
            self.data["tags"] = Value::Array(tags.values().to_vec());
        }
        self.metadata.rating = self
            .data
            .get("rating")
            .and_then(Value::as_str)
            .map(Arc::from);
        self.refresh_snapshot()
    }
    pub(super) fn next_number(&mut self) -> Result<usize> {
        let index = self
            .data
            .get("num")
            .and_then(Value::as_u64)
            .and_then(|value| value.checked_add(1))
            .filter(|value| *value < MAX_ENTRIES as u64)
            .ok_or_else(|| error("Pixiv novel embeds exceed the file limit"))?
            as usize;
        self.data["num"] = json!(index);
        self.data["suffix"] = json!(format!("_p{index:02}"));
        Ok(index)
    }
    pub(super) fn queue_updates(&mut self) -> Result<Vec<KeywordUpdate>> {
        self.next_number()?;
        self.data["date_url"] = Value::Null;
        let mut updates = Vec::new();
        if !self.queue_started {
            updates.push(KeywordUpdate::Set {
                name: "date_url".into(),
                value: Value::Null,
            });
            self.queue_started = true;
        }
        updates.push(KeywordUpdate::Increment("num".into()));
        updates.push(KeywordUpdate::Format {
            name: "suffix".into(),
            template: "_p{num:02}".into(),
        });
        Ok(updates)
    }
    pub(super) fn uploaded(&mut self, image: Value, mode: super::super::TagMode) -> Result<Media> {
        let invalid = || error("Pixiv returned an invalid embedded novel image");
        let mut image = image.as_object().cloned().ok_or_else(invalid)?;
        let urls = image.shift_remove("urls").ok_or_else(invalid)?;
        let raw = urls
            .get("original")
            .and_then(Value::as_str)
            .ok_or_else(invalid)?;
        let url = crate::extract::http_url(raw).map_err(|_| invalid())?;
        if image.keys().any(|key| {
            key.starts_with('_')
                || matches!(
                    key.as_str(),
                    "post_id"
                        | "media_id"
                        | "shortcode"
                        | "username"
                        | "caption_html"
                        | "timestamp"
                        | "index"
                        | "category"
                        | "subcategory"
                        | "headers"
                        | "cookies"
                        | "authorization"
                        | "access_token"
                        | "refresh_token"
                )
        }) {
            return Err(invalid());
        }
        let public_image = super::super::NovelMetadata::queue_fields(&image).ok_or_else(invalid)?;
        let mut updates: Vec<_> = public_image
            .into_iter()
            .map(|(name, value)| KeywordUpdate::Set { name, value })
            .collect();
        let previous_detail = self.metadata.pixiv_user_detail.clone();
        let user_changed = previous_detail
            .as_ref()
            .is_some_and(|detail| image.keys().any(|key| detail.fields().contains_key(key)));
        let date_changed = image.contains_key("date");
        self.data.as_object_mut().ok_or_else(invalid)?.extend(image);
        let number = self.next_number()?;
        let (filename, extension, date_url) = url_fields(&url);
        if !extension.bytes().all(|value| value.is_ascii_alphanumeric()) {
            return Err(error(
                "Pixiv novel image contains an unsafe filename extension",
            ));
        }
        self.data["filename"] = json!(filename);
        self.data["extension"] = json!(extension);
        self.data["date_url"] = date_url.clone();
        updates.extend([
            KeywordUpdate::Set {
                name: "date_url".into(),
                value: date_url,
            },
            KeywordUpdate::Increment("num".into()),
            KeywordUpdate::Format {
                name: "suffix".into(),
                template: "_p{num:02}".into(),
            },
            KeywordUpdate::Set {
                name: "filename".into(),
                value: self.data["filename"].clone(),
            },
            KeywordUpdate::Set {
                name: "extension".into(),
                value: self.data["extension"].clone(),
            },
        ]);
        let mut metadata = Self::metadata(&self.data, mode)?;
        metadata.pixiv_user_detail = if user_changed {
            let fields: Map<_, _> = previous_detail
                .as_ref()
                .expect("changed detail")
                .fields()
                .keys()
                .filter_map(|key| self.data.get(key).map(|value| (key.clone(), value.clone())))
                .collect();
            Some(Arc::new(
                super::super::UserDetail::parse(&Value::Object(fields)).ok_or_else(invalid)?,
            ))
        } else {
            previous_detail
        };
        metadata.rating = self
            .data
            .get("rating")
            .and_then(Value::as_str)
            .map(Arc::from);
        metadata.tags = self
            .data
            .get("tags")
            .and_then(super::super::Tags::parse_compat);
        metadata.timestamp = if date_changed {
            self.data
                .get("date")
                .and_then(Value::as_str)
                .and_then(|date| crate::date::parse_date(date).ok())
        } else {
            self.metadata.timestamp
        };
        metadata.index = number + 1;
        metadata.media_id = format!("{}_p{number}", metadata.post_id);
        metadata.file = Some(FileMetadata {
            width: self
                .data
                .get("width")
                .and_then(Value::as_u64)
                .filter(|value| *value > 0),
            height: self
                .data
                .get("height")
                .and_then(Value::as_u64)
                .filter(|value| *value > 0),
            ..FileMetadata::default()
        });
        self.metadata = metadata;
        self.refresh_snapshot()?;
        let mut metadata = self.metadata.clone();
        metadata.keyword_updates = Some(updates.into());
        Ok(Media {
            url,
            job: MediaJob::DirectHttp,
            category: "pixiv-novel".into(),
            filename: format!("{}_p{number}.{extension}", self.metadata.post_id),
            metadata: Some(metadata),
            headers: std::collections::BTreeMap::from([(
                "Referer".into(),
                super::super::REFERER.into(),
            )]),
            handoff: std::collections::BTreeMap::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture_case(name: &str) -> Value {
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/pixiv/novel_embeds.json"))
                .unwrap();
        fixture["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|case| case["name"] == name)
            .unwrap()
            .clone()
    }
    fn signature(updates: &[KeywordUpdate]) -> Vec<Value> {
        updates
            .iter()
            .map(|update| match update {
                KeywordUpdate::Set { name, value } => json!({"set":name,"value":value}),
                KeywordUpdate::Increment(name) => json!({"increment":name}),
                KeywordUpdate::Format { name, template } => {
                    json!({"format":name,"template":template})
                }
                KeywordUpdate::Remove(name) => json!({"remove":name}),
                KeywordUpdate::RemoveRequired(name) => json!({"remove_required":name}),
                KeywordUpdate::OnResume(updates) => json!({"on_resume":signature(updates)}),
            })
            .collect()
    }
    fn apply(updates: &[KeywordUpdate], context: &mut Map<String, Value>) -> Vec<KeywordUpdate> {
        let mut resume = Vec::new();
        for update in updates {
            match update {
                KeywordUpdate::Set { name, value } => {
                    context.insert(name.clone(), value.clone());
                }
                KeywordUpdate::Remove(name) => {
                    context.remove(name);
                }
                KeywordUpdate::RemoveRequired(name) => {
                    assert!(context.remove(name).is_some());
                }
                KeywordUpdate::Increment(name) => {
                    let value = context[name].as_i64().unwrap() + 1;
                    context.insert(name.clone(), json!(value));
                }
                KeywordUpdate::Format { name, template } => {
                    let value = crate::postprocess::template::Template::parse(template)
                        .unwrap()
                        .render_value(context)
                        .unwrap();
                    context.insert(name.clone(), value);
                }
                KeywordUpdate::OnResume(updates) => resume.extend(updates.iter().cloned()),
            }
        }
        resume
    }
    #[test]
    fn recorded_cover_upload_and_queue_operations_reproduce_source_snapshots_in_order() {
        let case = fixture_case("cover_offsets_upload_and_child_numbers");
        let mut novel = Novel::parse(
            case["input_works"][0].clone(),
            super::super::super::TagMode::Japanese,
        )
        .unwrap();
        let post = novel.post(true, true);
        let crate::event::KeywordPolicy::Shared { file_fields, .. } = post.keywords else {
            panic!()
        };
        assert_eq!(
            file_fields.as_ref(),
            ["index", "media_id", "url", "_url", "_filesize"]
        );
        let mut context = novel.data.as_object().unwrap().clone();
        assert_eq!(
            Value::Object(context.clone()),
            case["events"][0]["metadata"]
        );
        let text = novel
            .text(case["webview_text"].as_str().unwrap().into())
            .unwrap();
        let text_updates = text
            .metadata
            .as_ref()
            .unwrap()
            .keyword_updates
            .as_deref()
            .unwrap();
        assert_eq!(
            signature(text_updates),
            [json!({"set":"extension","value":"txt"})]
        );
        assert!(apply(text_updates, &mut context).is_empty());
        assert_eq!(
            Value::Object(context.clone()),
            case["events"][1]["metadata"]
        );
        let cover = novel.cover().unwrap();
        let updates = cover
            .metadata
            .as_ref()
            .unwrap()
            .keyword_updates
            .as_deref()
            .unwrap();
        let source = &case["events"][2]["metadata"];
        assert_eq!(
            signature(updates),
            [
                json!({"set":"date_url","value":source["date_url"]}),
                json!({"increment":"num"}),
                json!({"format":"suffix","template":"_p{num:02}"}),
                json!({"set":"_fallback","value":source["_fallback"]}),
                json!({"set":"filename","value":source["filename"]}),
                json!({"set":"extension","value":"jpg"}),
                json!({"on_resume":[{"remove_required":"_fallback"}]}),
            ]
        );
        let resume = apply(updates, &mut context);
        assert_eq!(Value::Object(context.clone()), *source);
        assert!(apply(&resume, &mut context).is_empty());
        for (index, image) in case["ajax_response"]["body"]["textEmbeddedImages"]
            .as_object()
            .unwrap()
            .values()
            .enumerate()
        {
            let uploaded = novel
                .uploaded(image.clone(), super::super::super::TagMode::Japanese)
                .unwrap();
            let updates = uploaded
                .metadata
                .as_ref()
                .unwrap()
                .keyword_updates
                .as_deref()
                .unwrap();
            let mut expected = image
                .as_object()
                .unwrap()
                .iter()
                .filter(|(key, _)| key.as_str() != "urls")
                .map(|(name, value)| json!({"set":name,"value":value}))
                .collect::<Vec<_>>();
            let source = &case["events"][index + 3]["metadata"];
            expected.extend([
                json!({"set":"date_url","value":source["date_url"]}),
                json!({"increment":"num"}),
                json!({"format":"suffix","template":"_p{num:02}"}),
                json!({"set":"filename","value":source["filename"]}),
                json!({"set":"extension","value":source["extension"]}),
            ]);
            assert_eq!(signature(updates), expected);
            assert!(apply(updates, &mut context).is_empty());
            assert_eq!(Value::Object(context.clone()), *source);
            assert!(novel.metadata.keyword_updates.is_none());
        }
        for index in 0..2 {
            let updates = novel.queue_updates().unwrap();
            let mut expected = Vec::new();
            if index == 0 {
                expected.push(json!({"set":"date_url","value":null}));
            }
            expected.extend([
                json!({"increment":"num"}),
                json!({"format":"suffix","template":"_p{num:02}"}),
            ]);
            assert_eq!(signature(&updates), expected);
            assert!(apply(&updates, &mut context).is_empty());
            let mut source = case["events"][index + 5]["metadata"]
                .as_object()
                .unwrap()
                .clone();
            source.remove("_extractor");
            assert_eq!(context, source);
        }
        assert!(novel.metadata.keyword_updates.is_none());
        assert!(post.metadata.unwrap().keyword_updates.is_none());
    }
    #[test]
    fn producer_operations_preserve_hook_mutations_and_apply_explicit_image_num_override() {
        let case = fixture_case("cover_offsets_upload_and_child_numbers");
        let mut novel = Novel::parse(
            case["input_works"][0].clone(),
            super::super::super::TagMode::Japanese,
        )
        .unwrap();
        let mut context = novel.data.as_object().unwrap().clone();
        context.insert("num".into(), json!(40));
        context.insert("suffix".into(), json!("post-hook"));
        context.remove("caption");
        let text = novel.text("text".into()).unwrap();
        apply(
            text.metadata
                .as_ref()
                .unwrap()
                .keyword_updates
                .as_deref()
                .unwrap(),
            &mut context,
        );
        assert_eq!(context["num"], 40);
        assert_eq!(context["suffix"], "post-hook");
        assert!(!context.contains_key("caption"));
        let cover = novel.cover().unwrap();
        let resume = apply(
            cover
                .metadata
                .as_ref()
                .unwrap()
                .keyword_updates
                .as_deref()
                .unwrap(),
            &mut context,
        );
        assert_eq!(context["num"], 41);
        assert_eq!(context["suffix"], "_p41");
        assert_eq!(cover.metadata.as_ref().unwrap().media_id, "42_p1");
        apply(&resume, &mut context);
        let mut image = case["ajax_response"]["body"]["textEmbeddedImages"]["a"].clone();
        image["num"] = json!(7);
        let uploaded = novel
            .uploaded(image, super::super::super::TagMode::Japanese)
            .unwrap();
        let updates = uploaded
            .metadata
            .as_ref()
            .unwrap()
            .keyword_updates
            .as_deref()
            .unwrap();
        assert!(signature(updates).contains(&json!({"set":"num","value":7})));
        apply(updates, &mut context);
        assert_eq!(context["num"], 8);
        assert_eq!(context["suffix"], "_p08");
        apply(&novel.queue_updates().unwrap(), &mut context);
        assert!(context["date_url"].is_null());
        context.insert("num".into(), json!(100));
        context.insert("date_url".into(), json!("child-hook"));
        apply(&novel.queue_updates().unwrap(), &mut context);
        assert_eq!(context["num"], 101);
        assert_eq!(context["suffix"], "_p101");
        assert_eq!(context["date_url"], "child-hook");
    }
    #[tokio::test]
    async fn marker_scan_matches_literal_source_brackets_pages_and_first_occurrence_order() {
        let plan=scan("[Uploadedimage:a][outer[pixivimage:99]][pixivimage:70-3][uploadedimage:b][pixivimage:71][pixivimage:70][pixivimage:][pixivimage:-2][pixivimage: 72 ][pixivimage:unfinished").await.unwrap();
        assert!(plan.uploads);
        assert_eq!(plan.ids, ["70", "71", "", " 72 "]);
        assert!(scan(&"[x]".repeat(MAX_ENTRIES + 1)).await.is_err());
        assert!(scan("[uploadedimage:").await.unwrap().ids.is_empty());
    }
    #[test]
    fn uploaded_collection_and_selected_queue_memory_are_bounded_without_losing_duplicate_identity()
    {
        assert!(images(json!({"textEmbeddedImages":[]})).is_none());
        assert!(
            images(json!({"textEmbeddedImages":{"a":{"value":1},"b":{"value":2}}}))
                .unwrap()
                .pop_front()
                .unwrap()["value"]
                == 1
        );
        let mut visited = Visited::default();
        assert!(visited.insert("70").unwrap());
        assert!(!visited.insert("70").unwrap());
        visited.bytes = super::super::super::MAX_RESPONSE;
        assert!(!visited.insert("70").unwrap());
        assert!(visited.insert("71").is_err());
        let mut visited = Visited {
            ids: (0..MAX_ENTRIES).map(|id| id.to_string()).collect(),
            ..Default::default()
        };
        assert!(visited.insert("extra").is_err());
    }
    #[test]
    fn uploaded_mutations_retain_prepared_tags_refresh_overlapping_detail_and_snapshot_each_file() {
        let raw = json!({"id":42,"title":"Novel","caption":"<b>Raw</b>","user":{"id":5,"account":"before"},"profile":{"comment":"profile"},"extra_top":{"old":true},"tags":[{"name":"猫","translated_name":"cat"}],"create_date":"2024-01-02T12:04:05+09:00"});
        let mut novel = Novel::parse(raw, super::super::super::TagMode::Japanese).unwrap();
        novel.metadata.pixiv_user_detail=Some(Arc::new(super::super::super::UserDetail::parse(&json!({"user":{"id":5,"account":"before"},"profile":{"comment":"profile"},"extra_top":{"old":true}})).unwrap()));
        novel.refresh_snapshot().unwrap();
        let first=novel.uploaded(json!({"urls":{"original":"https://other.invalid/novel-cover-original/img/2024/01/03/04/05/06/upload_a.JPEG"},"width":640,"height":480,"extra_upload":{"first":true},"extra_top":{"new":true},"user":{"id":6,"account":"after"}}),super::super::super::TagMode::Japanese).unwrap();
        let first_metadata = first.metadata.as_ref().unwrap();
        assert_eq!(first.filename, "42_p1.jpeg");
        assert_eq!(
            first_metadata.tags.as_ref().unwrap().values(),
            [json!("猫")]
        );
        assert_eq!(
            first_metadata.pixiv_user_detail.as_ref().unwrap().fields()["user"]["account"],
            "after"
        );
        assert_eq!(
            first_metadata.pixiv_user_detail.as_ref().unwrap().fields()["extra_top"],
            json!({"new":true})
        );
        assert!(
            !first_metadata
                .pixiv_novel_metadata
                .as_ref()
                .unwrap()
                .fields()
                .contains_key("extra_top")
        );
        let second=novel.uploaded(json!({"urls":{"original":"https://other.invalid/novel-cover-original/img/2024/01/04/05/06/07/upload_b.png"},"later_extra":2}),super::super::super::TagMode::Japanese).unwrap();
        assert_eq!(second.filename, "42_p2.png");
        let metadata = second.metadata.as_ref().unwrap();
        assert_eq!(metadata.file.as_ref().unwrap().width, Some(640));
        assert_eq!(
            metadata.pixiv_novel_metadata.as_ref().unwrap().fields()["extra_upload"],
            json!({"first":true})
        );
        assert_eq!(
            first_metadata
                .pixiv_novel_metadata
                .as_ref()
                .unwrap()
                .fields()["num"],
            1
        );
        assert_eq!(
            metadata.pixiv_novel_metadata.as_ref().unwrap().fields()["num"],
            2
        );
        assert!(Arc::ptr_eq(
            first_metadata.pixiv_user_detail.as_ref().unwrap(),
            metadata.pixiv_user_detail.as_ref().unwrap()
        ));
        assert_eq!(
            serde_json::to_string(metadata)
                .unwrap()
                .matches("\"extra_top\"")
                .count(),
            1
        );
    }
    #[test]
    fn generated_file_numbers_and_decoded_extensions_cannot_escape_archive_or_filename_bounds() {
        let mut novel = Novel::parse(
            json!({"id":42,"user":{"id":5},"x_restrict":0}),
            super::super::super::TagMode::Japanese,
        )
        .unwrap();
        assert_eq!(novel.metadata.rating.as_deref(), Some("General"));
        novel.data["num"] = json!(99_998);
        assert_eq!(novel.next_number().unwrap(), 99_999);
        assert!(novel.next_number().is_err());
        novel.data["num"] = json!(0);
        assert!(
            novel
                .uploaded(
                    json!({"urls":{"original":"https://i.pximg.net/upload.j%2Fpg"}}),
                    super::super::super::TagMode::Japanese
                )
                .unwrap_err()
                .to_string()
                .contains("unsafe filename")
        );
    }
    fn novel(id: u64) -> Value {
        json!({"id":id,"title":"Novel","user":{"id":5,"account":"artist"},"x_restrict":0,"tags":[]})
    }
    fn reply(value: &Value) -> Vec<u8> {
        super::super::super::tests::response("200 OK", value.to_string().as_bytes())
    }
    fn webview(text: &str) -> Vec<u8> {
        super::super::super::tests::response(
            "200 OK",
            format!("<script>novel: {},\n</script>", json!({"text":text})).as_bytes(),
        )
    }
    #[tokio::test]
    async fn rejected_duplicate_queue_does_not_visit_artwork_before_later_novel_accepts_it() {
        use futures_util::StreamExt;
        let (root, server) = super::super::super::tests::serve(vec![
            super::super::super::tests::token("NOVEL"),
            reply(&json!({"novels":[novel(42),novel(43)],"next_url":null})),
            webview("[pixivimage:70]"),
            webview("[pixivimage:70]"),
            super::super::super::tests::token("ART"),
            reply(
                &json!({"illust":{"id":70,"user":{"id":7},"type":"illust","meta_pages":[],"meta_single_page":{"original_image_url":"https://i.pximg.net/img-original/70_p0.jpg"}}}),
            ),
        ]);
        let leaf = Arc::new(super::super::super::tests::pixiv(root.clone()));
        let native = super::super::super::tests::pixiv(root)
            .with_novel_options(super::super::NovelOptions {
                embeds: true,
                ..Default::default()
            })
            .with_artwork_extractor(leaf);
        let url = Url::parse("https://www.pixiv.net/users/9/novels").unwrap();
        let output = crate::job_selection::JobSelection {
            child_filter: Some(crate::filter::FilterExpression::parse("id == 43").unwrap()),
            ..Default::default()
        }
        .apply(native.events(&url))
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()
        .unwrap();
        assert_eq!(
            output
                .iter()
                .map(|file| file.filename.as_str())
                .collect::<Vec<_>>(),
            ["42_p0.txt", "43_p0.txt", "70_p0.jpg"]
        );
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 6);
        assert!(
            requests[5]
                .to_ascii_lowercase()
                .contains("authorization: bearer art")
        );
    }
    #[tokio::test]
    async fn invalid_child_is_selected_before_validation_and_errors_balance_without_stopping_later_novels()
     {
        use futures_util::StreamExt;
        for selection in [0, 1, 2] {
            let mut replies = vec![
                super::super::super::tests::token("NOVEL"),
                reply(&json!({"novels":[novel(42),novel(43)],"next_url":null})),
                webview("[pixivimage:abc]"),
            ];
            if selection != 2 {
                replies.push(webview("later novel"));
            }
            let (root, server) = super::super::super::tests::serve(replies);
            let native = super::super::super::tests::pixiv(root.clone())
                .with_novel_options(super::super::NovelOptions {
                    embeds: true,
                    ..Default::default()
                })
                .with_artwork_extractor(Arc::new(
                    super::super::super::tests::pixiv(root).with_refresh_token(None),
                ));
            let url = Url::parse("https://www.pixiv.net/users/9/novels").unwrap();
            if selection == 0 {
                let events = native.events(&url).collect::<Vec<_>>().await;
                assert_eq!(
                    events
                        .iter()
                        .filter(|event| matches!(
                            event,
                            Ok(crate::event::ExtractionEvent::BeginChild(_))
                        ))
                        .count(),
                    1
                );
                assert_eq!(
                    events
                        .iter()
                        .filter(|event| matches!(
                            event,
                            Ok(crate::event::ExtractionEvent::EndChild(_))
                        ))
                        .count(),
                    1
                );
                assert_eq!(
                    events
                        .iter()
                        .filter(|event| matches!(event, Ok(crate::event::ExtractionEvent::EndPost)))
                        .count(),
                    2
                );
                assert_eq!(events.iter().filter(|event| event.is_err()).count(), 1);
                assert!(
                    events
                        .iter()
                        .find_map(|event| event.as_ref().err())
                        .unwrap()
                        .to_string()
                        .contains("linked artwork ID")
                );
                assert_eq!(
                    events
                        .iter()
                        .filter(|event| matches!(
                            event,
                            Ok(crate::event::ExtractionEvent::Media(_))
                        ))
                        .count(),
                    2
                );
            } else {
                let selector = if selection == 1 {
                    crate::job_selection::JobSelection {
                        child_filter: Some(
                            crate::filter::FilterExpression::parse("False").unwrap(),
                        ),
                        ..Default::default()
                    }
                } else {
                    crate::job_selection::JobSelection {
                        child_range: Some(crate::selection::RangeSelection::parse("0").unwrap()),
                        ..Default::default()
                    }
                };
                let output = selector
                    .apply(native.events(&url))
                    .collect::<Vec<_>>()
                    .await
                    .into_iter()
                    .collect::<Result<Vec<_>>>()
                    .unwrap();
                assert_eq!(output.len(), if selection == 1 { 2 } else { 1 });
            }
            assert_eq!(
                server.join().unwrap().len(),
                if selection == 2 { 3 } else { 4 }
            );
        }
    }
}
