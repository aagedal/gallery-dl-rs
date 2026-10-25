//! Limited-work recovery and the source's ordered AJAX/App listing merge.
use super::{Pixiv, enrichment::WorkFailure};
use crate::{Result, error, extract::Media};
use serde_json::{Value, json};

const MAX_IDS: usize = 100_000;
const LIMIT: &str = "https://s.pximg.net/common/images/limit_";
#[derive(Default)]
pub(super) struct SanityListing {
    pub initialized: bool,
    ids: Vec<String>,
}
impl SanityListing {
    pub(super) async fn initialize(&mut self, native: &Pixiv, user: &str) {
        self.initialized = true;
        if !native.sanity {
            return;
        }
        let Some(body) = native
            .request_ajax(&format!("/user/{user}/profile/all"))
            .await
            .filter(super::value_truthy)
        else {
            return;
        };
        let mut ids = Vec::new();
        let mut bytes = 0usize;
        for key in ["illusts", "manga"] {
            let Some(values) = body.get(key) else { return };
            let raw: Box<dyn Iterator<Item = Option<&str>> + Send + '_> = match values {
                Value::Object(values) => Box::new(values.keys().map(|key| Some(key.as_str()))),
                Value::Array(values) => Box::new(values.iter().map(|value| match value {
                    Value::String(raw) => Some(raw.as_str()),
                    Value::Number(raw) => Some(raw.as_str()),
                    _ => None,
                })),
                _ => return,
            };
            for (index, raw) in raw.enumerate() {
                if index % 256 == 0 {
                    tokio::task::yield_now().await;
                }
                let Some(raw) = raw else { return };
                let Ok(id) = raw.trim().parse::<u128>() else {
                    return;
                };
                let id = id.to_string();
                bytes = bytes.saturating_add(id.len());
                if !super::valid_id(&id) || ids.len() >= MAX_IDS || bytes > super::MAX_RESPONSE {
                    return;
                }
                ids.push(id);
            }
        }
        tokio::task::yield_now().await;
        ids.sort_by_cached_key(|id| id.parse::<u128>().expect("validated integer"));
        tokio::task::yield_now().await;
        self.ids = ids;
    }
    pub(super) fn next(
        &mut self,
        works: &mut std::collections::VecDeque<Value>,
        finished: bool,
    ) -> Option<Value> {
        if let Some(work) = works.front() {
            if let Some(id) = super::id_of(work).and_then(|id| id.parse::<u128>().ok())
                && let Some(last) = self.ids.last()
            {
                let ajax = last.parse::<u128>().expect("validated integer");
                if ajax > id {
                    return self.ids.pop().map(make_work);
                }
                if ajax == id {
                    self.ids.pop();
                }
            }
            return works.pop_front();
        }
        if finished {
            return self.ids.pop().map(make_work);
        }
        None
    }
}
fn make_work(id: String) -> Value {
    let id = serde_json::Number::from_u128(id.parse().expect("validated integer"))
        .expect("bounded integer");
    json!({"id":id,"user":{"id":1},"type":id,"title":id,"caption":null,"tags":[],"create_date":null,"width":0,"height":0,"sanity_level":0,"x_restrict":0,"total_comments":0,"is_bookmarked":false,"page_count":1,"image_urls":null,"meta_pages":[],"meta_single_page":{"original_image_url":LIMIT}})
}
impl Pixiv {
    pub fn with_sanity(mut self, enabled: bool) -> Self {
        self.sanity = enabled;
        self
    }
    async fn ajax_image_url(&self, body: &Value) -> Result<Option<String>> {
        if let Some(raw) = body
            .get("urls")
            .and_then(|urls| urls.get("original"))
            .filter(|value| super::value_truthy(value))
        {
            let raw = raw
                .as_str()
                .ok_or_else(|| error("Pixiv returned an invalid AJAX original URL"))?;
            return super::media_url(raw)
                .map(|_| Some(raw.to_owned()))
                .ok_or_else(|| error("Pixiv returned an unsupported AJAX image URL"));
        }
        let Some(square) = body
            .get("id")
            .and_then(Value::as_str)
            .and_then(|id| body.get("userIllusts")?.get(id)?.get("url")?.as_str())
        else {
            return Ok(None);
        };
        let base = square.rsplit_once("_p0").map_or("", |(base, _)| base);
        let mut parts: Vec<_> = base.split('/').collect();
        if parts.len() < 6 {
            return Ok(None);
        }
        parts.drain(3..5);
        parts[3] = "img-original";
        let base = parts.join("/");
        for extension in ["jpg", "png", "gif"] {
            let url = super::media_url(&format!("{base}_p0.{extension}"))
                .ok_or_else(|| error("Pixiv returned an unsupported AJAX image URL"))?;
            if self.ugoira_probe(&url).await {
                return Ok(Some(format!("{base}_p0.{extension}")));
            }
        }
        Ok(None)
    }
    pub(super) async fn recover_ajax_work(
        &self,
        work: &mut Value,
        body: &Value,
    ) -> Result<Vec<Media>> {
        work["_ajax"] = json!(true);
        let Some(url) = self.ajax_image_url(body).await? else {
            return Ok(Vec::new());
        };
        let invalid = || error("Pixiv returned incomplete AJAX illustration metadata");
        for (app, ajax) in [
            ("title", "illustTitle"),
            ("image_urls", "urls"),
            ("create_date", "createDate"),
            ("width", "width"),
            ("height", "height"),
            ("sanity_level", "sl"),
            ("total_view", "viewCount"),
            ("total_comments", "commentCount"),
            ("total_bookmarks", "bookmarkCount"),
            ("restrict", "restrict"),
            ("x_restrict", "xRestrict"),
            ("illust_ai_type", "aiType"),
            ("illust_book_style", "bookStyle"),
        ] {
            work[app] = body.get(ajax).ok_or_else(invalid)?.clone();
        }
        let user_id = body.get("userId").ok_or_else(invalid)?;
        let id = match user_id {
            Value::String(id) => id.parse::<u128>().ok(),
            Value::Number(id) => id.as_u128(),
            _ => None,
        }
        .map(|id| id.to_string())
        .filter(|id| super::valid_id(id))
        .ok_or_else(invalid)?;
        work["user"] = json!({"id":serde_json::Number::from_u128(id.parse().expect("validated integer")).expect("bounded integer"),"account":body.get("userAccount").ok_or_else(invalid)?,"name":body.get("userName").ok_or_else(invalid)?,"is_followed":false,"profile_image_urls":{}});
        if work.get("is_bookmarked").is_none() {
            work["is_bookmarked"] =
                json!(body.get("bookmarkData").is_some_and(super::value_truthy));
        }
        let tags = body
            .get("tags")
            .and_then(|tags| tags.get("tags"))
            .and_then(Value::as_array)
            .ok_or_else(invalid)?;
        if tags.len() > 100_000 {
            return Err(error("Pixiv AJAX tags exceed the count limit"));
        }
        let mut output = Vec::with_capacity(tags.len());
        let mut tag_bytes = 2usize;
        for (index, tag) in tags.iter().enumerate() {
            if index % 256 == 0 {
                tokio::task::yield_now().await;
            }
            let entry = json!({"name":tag.get("tag").ok_or_else(invalid)?,"translated_name":tag.get("translation").and_then(|translation|translation.get("en")).cloned().unwrap_or(Value::Null)});
            tag_bytes = tag_bytes
                .saturating_add(serde_json::to_vec(&entry).map_err(|_| invalid())?.len())
                .saturating_add(usize::from(!output.is_empty()));
            if tag_bytes > super::MAX_RESPONSE {
                return Err(error("Pixiv AJAX tags exceed the size limit"));
            }
            output.push(entry);
        }
        work["tags"] = Value::Array(output);
        work["caption"] = json!(super::web::sanitize_caption(
            body.get("illustComment").ok_or_else(invalid)?
        )?);
        let count = body
            .get("pageCount")
            .and_then(Value::as_i64)
            .ok_or_else(invalid)?;
        if count > 10_000 {
            return Err(error("Pixiv AJAX illustration exceeds the page limit"));
        }
        work["page_count"] = json!(count);
        let urls = if count == 1 {
            vec![super::media_url(&url).ok_or_else(invalid)?]
        } else {
            let (base, extension) = url.rsplit_once("_p0.").unwrap_or(("", ""));
            let count = count.max(0) as usize;
            let size = base.len().saturating_add(extension.len()).saturating_add(9);
            if size.saturating_mul(count) > super::MAX_RESPONSE {
                return Err(error("Pixiv AJAX page URLs exceed the size limit"));
            }
            (0..count)
                .map(|index| {
                    super::media_url(&format!("{base}_p{index}.{extension}"))
                        .ok_or_else(|| error("Pixiv returned an unsupported AJAX page URL"))
                })
                .collect::<Result<Vec<_>>>()?
        };
        let id = super::id_of(work).ok_or_else(invalid)?;
        let mut files = super::parse_files(work, &id, urls, true);
        for file in &mut files {
            if let Some(description) = file
                .metadata
                .as_mut()
                .and_then(|metadata| metadata.file.as_mut())
            {
                description.width = work
                    .get("width")
                    .and_then(Value::as_u64)
                    .filter(|value| *value > 0);
                description.height = work
                    .get("height")
                    .and_then(Value::as_u64)
                    .filter(|value| *value > 0);
            }
        }
        Ok(files)
    }
    pub(super) async fn sanity_files(
        &self,
        work: &mut Value,
    ) -> std::result::Result<Option<Vec<Media>>, WorkFailure> {
        if work
            .get("meta_pages")
            .and_then(Value::as_array)
            .is_some_and(|pages| !pages.is_empty())
        {
            return Ok(None);
        }
        let Some(raw) = work
            .get("meta_single_page")
            .and_then(|page| page.get("original_image_url"))
            .and_then(Value::as_str)
            .filter(|url| url.starts_with(LIMIT))
        else {
            return Ok(None);
        };
        let reason = raw.rsplit('/').next().unwrap_or("").to_owned();
        if !matches!(
            reason.as_str(),
            "limit_" | "limit_unviewable_360.png" | "limit_sanity_level_360.png"
        ) {
            work["_mypixiv"] = json!(true);
            work["__sanity_no_ugoira"] = json!(true);
            return Ok(Some(Vec::new()));
        }
        work["_ajax"] = json!(true);
        work["__sanity_no_ugoira"] = json!(true);
        if !self.sanity {
            return Ok(Some(Vec::new()));
        }
        let id = super::id_of(work)
            .ok_or_else(|| WorkFailure::Enrichment(error("Pixiv limited work has no ID")))?;
        let body = self
            .request_ajax(&format!("/illust/{id}"))
            .await
            .unwrap_or(Value::Null);
        if work.get("type").and_then(Value::as_str) != Some("ugoira") {
            return self
                .recover_ajax_work(work, &body)
                .await
                .map(Some)
                .map_err(WorkFailure::Enrichment);
        }
        if !self.ugoira.enabled() {
            return Ok(Some(Vec::new()));
        }
        self.recover_ajax_work(work, &body)
            .await
            .map_err(WorkFailure::Files)?;
        let meta = self
            .request_ajax(&format!("/illust/{id}/ugoira_meta"))
            .await
            .ok_or_else(|| WorkFailure::Files(error("Pixiv returned no AJAX ugoira metadata")))?;
        let frames = meta
            .get("frames")
            .and_then(super::parse_frames)
            .ok_or_else(|| {
                WorkFailure::Files(error("Pixiv returned invalid AJAX ugoira frames"))
            })?;
        let result = if self.ugoira == super::UgoiraMode::Original {
            let raw = meta
                .get("src")
                .and_then(Value::as_str)
                .ok_or_else(|| WorkFailure::Files(error("Pixiv returned no AJAX ugoira source")))?;
            let mut source = work.clone();
            source["meta_single_page"] = json!({"original_image_url":raw});
            let body = json!({"ugoira_metadata":{"frames":frames}}).to_string();
            self.original_ugoira(&source, body.as_bytes()).await
        } else {
            let raw = meta
                .get("originalSrc")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    WorkFailure::Files(error("Pixiv returned no AJAX ugoira ZIP source"))
                })?;
            let body =
                json!({"ugoira_metadata":{"frames":frames,"zip_urls":{"medium":raw}}}).to_string();
            super::parse_ugoira(work, body.as_bytes())
        };
        let media = result.map_err(WorkFailure::Files)?;
        work["__sanity_no_ugoira"] = json!(false);
        Ok(Some(media))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        event::ExtractionEvent,
        pixiv::tests::{pixiv, response, serve, token},
    };
    use futures_util::StreamExt;
    use std::collections::VecDeque;
    use url::Url;
    fn fixture() -> Value {
        serde_json::from_str(include_str!("../tests/fixtures/pixiv/sanity.json")).unwrap()
    }
    fn reply(value: &Value) -> Vec<u8> {
        response("200 OK", value.to_string().as_bytes())
    }
    #[test]
    fn ordered_merge_matches_all_source_vectors_including_duplicates_and_unsorted_app_rows() {
        for case in fixture()["extend_cases"].as_array().unwrap() {
            let mut state = SanityListing {
                initialized: true,
                ids: case["ajax_ids"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|id| id.to_string())
                    .collect(),
            };
            let mut works: VecDeque<_> = case["input_works"].as_array().unwrap().clone().into();
            let mut actual = Vec::new();
            while let Some(work) = state.next(&mut works, true) {
                actual.push(super::super::id_of(&work).unwrap());
            }
            let expected: Vec<_> = case["works"]
                .as_array()
                .unwrap()
                .iter()
                .map(|work| super::super::id_of(work).unwrap())
                .collect();
            assert_eq!(actual, expected, "{}", case["name"]);
        }
    }
    #[tokio::test]
    async fn profile_ids_are_bounded_sorted_and_discarded_together_on_invalid_or_excess_input() {
        for body in [
            json!({"illusts":["7","bad","8"],"manga":{}}),
            json!({"illusts":vec![1;MAX_IDS+1],"manga":{}}),
            json!({"illusts":["7"],"manga":null}),
            json!({"illusts":["7","0"],"manga":{}}),
        ] {
            let (root, server) = serve(vec![reply(&json!({"body":body}))]);
            let native = pixiv(root.clone()).with_web_root(root).with_sanity(true);
            let mut state = SanityListing::default();
            state.initialize(&native, "5").await;
            assert!(state.initialized);
            assert!(state.ids.is_empty());
            assert_eq!(server.join().unwrap().len(), 1);
        }
        let (root, server) = serve(vec![reply(
            &json!({"body":{"illusts":[" 007 ",9],"manga":{"5":null,"9":null}}}),
        )]);
        let native = pixiv(root.clone()).with_web_root(root).with_sanity(true);
        let mut state = SanityListing::default();
        state.initialize(&native, "5").await;
        assert_eq!(state.ids, ["5", "7", "9", "9"]);
        assert_eq!(server.join().unwrap().len(), 1);
    }
    #[tokio::test]
    async fn ajax_tag_count_and_generated_page_url_bytes_are_bounded_before_large_output_allocation()
     {
        let fixture = fixture();
        let case = &fixture["file_cases"][0];
        let mut data = case["input_work"].clone();
        let original = case["ajax_body"].clone();
        let (root, server) = serve(vec![]);
        let native = pixiv(root);
        let mut body = original.clone();
        body["tags"]["tags"] = json!(vec![json!({"tag":"x"}); 100_001]);
        assert!(
            native
                .recover_ajax_work(&mut data, &body)
                .await
                .unwrap_err()
                .to_string()
                .contains("tag")
        );
        let mut body = original.clone();
        body["pageCount"] = json!(10_000);
        body["urls"]["original"] = json!(format!(
            "https://i.pximg.net/img-original/42_p0.jpg?q={}",
            "x".repeat(4096)
        ));
        assert!(
            native
                .recover_ajax_work(&mut data, &body)
                .await
                .unwrap_err()
                .to_string()
                .contains("size limit")
        );
        let mut body = original;
        body["pageCount"] = json!(10_001);
        assert!(
            native
                .recover_ajax_work(&mut data, &body)
                .await
                .unwrap_err()
                .to_string()
                .contains("page limit")
        );
        assert!(server.join().unwrap().is_empty());
    }
    #[tokio::test]
    async fn profile_preflight_is_lazy_and_source_budget_stops_before_next_inserted_ajax_work() {
        for range in [false, true] {
            let mut api = make_work("48".into());
            api["meta_single_page"] =
                json!({"original_image_url":"https://i.pximg.net/img-original/48_p0.jpg"});
            api["type"] = json!("illust");
            let (root, server) = serve(vec![
                reply(&json!({"body":{"illusts":{"50":null,"49":null},"manga":{}}})),
                token("API_SECRET"),
                reply(&json!({"illusts":[api],"next_url":null})),
                reply(&json!({"body":null})),
            ]);
            let native = pixiv(root.clone())
                .with_web_root(root)
                .with_sanity(true)
                .with_max_posts((!range).then_some(1));
            let url = Url::parse("https://www.pixiv.net/users/5/illustrations").unwrap();
            {
                let untouched = native.events(&url);
                drop(untouched);
            }
            if range {
                let files = crate::job_selection::JobSelection {
                    post_selection: Some(crate::post_selection::PostSelection::new(
                        Some(crate::selection::RangeSelection::parse("1").unwrap()),
                        None,
                    )),
                    ..Default::default()
                }
                .apply(native.events(&url))
                .collect::<Vec<_>>()
                .await;
                assert!(files.is_empty());
            } else {
                let events = native.events(&url).collect::<Vec<_>>().await;
                assert_eq!(events.len(), 2);
                let Some(Ok(ExtractionEvent::Post(post))) = events.first() else {
                    panic!("{events:?}")
                };
                assert_eq!(post.metadata.as_ref().unwrap().post_id, "50");
                assert_eq!(post.count, Some(0));
            }
            let requests = server.join().unwrap();
            assert_eq!(requests.len(), 4);
            assert!(requests[0].starts_with("GET /ajax/user/5/profile/all "));
            assert!(!requests[0].contains("API_SECRET"));
            assert!(requests[2].starts_with("GET /v1/user/illusts?user_id=5 "));
            assert!(requests[3].starts_with("GET /ajax/illust/50 "));
        }
    }
}
