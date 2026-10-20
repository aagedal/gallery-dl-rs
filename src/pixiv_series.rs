//! Illustration series preserve first-page series metadata and raw-work order.
use super::Pixiv;
use crate::{
    Result, error,
    event::{ExtractionEvent, Post},
};
use futures_util::{
    StreamExt,
    stream::{self, BoxStream},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeSet, VecDeque},
    sync::Arc,
};
use url::Url;

const MAX_TEXT: usize = 64 * 1024;
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct SeriesMetadata {
    pub id: String,
    pub title: Option<Arc<str>>,
    pub caption: Option<Arc<str>>,
    pub total: u64,
}
impl SeriesMetadata {
    pub fn valid(&self) -> bool {
        super::valid_id(&self.id)
            && self.total <= i64::MAX as u64
            && self
                .title
                .as_ref()
                .is_none_or(|title| title.len() <= MAX_TEXT)
            && self
                .caption
                .as_ref()
                .is_none_or(|caption| caption.len() <= MAX_TEXT)
    }
    fn parse(value: &Value, total_key: &str) -> Option<Self> {
        let id = super::id_of(value)?;
        let total = value.get(total_key)?.as_u64()?;
        if total > i64::MAX as u64 {
            return None;
        }
        let text = |key| match value.get(key) {
            None | Some(Value::Null) => Some(None),
            Some(Value::String(text)) if text.len() <= MAX_TEXT => {
                Some(Some(Arc::from(text.as_str())))
            }
            _ => None,
        };
        let result = Self {
            id,
            title: text("title")?,
            caption: text("caption")?,
            total,
        };
        result.valid().then_some(result)
    }
    pub fn parse_compat(value: &Value) -> Option<Self> {
        Self::parse(value, "total")
    }
}

struct State {
    budget: super::PostBudget,
    id: String,
    next: Option<String>,
    pages: usize,
    seen: BTreeSet<String>,
    works: VecDeque<Value>,
    series: Option<Arc<SeriesMetadata>>,
    number: Option<i64>,
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
fn prefix(number: i64) -> String {
    let number = number.to_string();
    format!(
        "{}{}",
        "0".repeat(3usize.saturating_sub(number.len())),
        number
    )
}

impl Pixiv {
    fn series_first(&self, id: &str) -> Url {
        let mut endpoint = self
            .api_root
            .join("v1/illust/series")
            .expect("static series endpoint");
        endpoint
            .query_pairs_mut()
            .append_pair("illust_series_id", id)
            .append_pair("offset", "0");
        endpoint
    }
    fn series_next(&self, id: &str, raw: &str) -> Result<Url> {
        let invalid = || error("Pixiv returned an unexpected illustration series next page");
        let endpoint = Url::parse(raw).map_err(|_| invalid())?;
        let first = self.series_first(id);
        let pairs: Vec<_> = endpoint.query_pairs().collect();
        let mut ids = pairs.iter().filter(|(key, _)| key == "illust_series_id");
        let scope = ids.next().is_some_and(|(_, actual)| actual == id) && ids.next().is_none();
        if endpoint.origin() != self.api_root.origin()
            || endpoint.path() != first.path()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.fragment().is_some()
            || !scope
        {
            return Err(invalid());
        }
        Ok(endpoint)
    }
    async fn series_page(&self, state: &mut State) -> Result<()> {
        if state.pages >= super::MAX_PAGES {
            state.done = true;
            return Ok(());
        }
        let Some(raw) = state.next.take() else {
            state.done = true;
            return Ok(());
        };
        let endpoint = self.series_next(&state.id, &raw)?;
        let mut canonical = endpoint.clone();
        let mut pairs: Vec<_> = endpoint
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        pairs.sort();
        canonical.set_query(None);
        canonical.query_pairs_mut().extend_pairs(pairs);
        if !state.seen.insert(canonical.to_string()) {
            return Err(error(
                "Pixiv illustration series pagination repeated a page",
            ));
        }
        let mut body = super::json(&self.get(&endpoint).await?)?;
        if state.pages == 0 {
            state.series = body
                .get("illust_series_detail")
                .and_then(|series| SeriesMetadata::parse(series, "series_work_count"))
                .filter(|series| series.id == state.id)
                .map(Arc::new);
        }
        let works = body
            .get_mut("illusts")
            .and_then(Value::as_array_mut)
            .ok_or_else(|| error("Pixiv response contains no series illustration list"))?;
        state.works = std::mem::take(works).into();
        state.next = super::string(&body, "next_url").map(str::to_owned);
        state.pages += 1;
        Ok(())
    }
    pub(super) fn series_events(&self, id: String) -> BoxStream<'_, Result<ExtractionEvent>> {
        if self.refresh_token.is_none() {
            return stream::once(async { Err(super::missing_token()) }).boxed();
        }
        let next = Some(self.series_first(&id).to_string());
        let state = State {
            budget: super::PostBudget::new(self.max_posts),
            id,
            next,
            pages: 0,
            seen: BTreeSet::new(),
            works: VecDeque::new(),
            series: None,
            number: None,
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
                if let Some(work) = state.works.pop_front() {
                    state.budget.consume();
                    skipped = skipped.wrapping_add(1);
                    if skipped == 0 {
                        tokio::task::yield_now().await;
                    }
                    let Some(series) = state.series.clone() else {
                        state.done = true;
                        return Some((
                            Err(error("Pixiv returned invalid illustration series metadata")),
                            state,
                        ));
                    };
                    let number = match state.number {
                        None => series.total as i64,
                        Some(number) => match number.checked_sub(1) {
                            Some(number) => number,
                            None => {
                                state.done = true;
                                return Some((
                                    Err(error("Pixiv illustration series counter overflow")),
                                    state,
                                ));
                            }
                        },
                    };
                    state.number = Some(number);
                    if !work
                        .get("user")
                        .and_then(|user| user.get("id"))
                        .is_some_and(truthy)
                    {
                        continue;
                    }
                    let (mut media, failure) = match self.work(&work, true).await {
                        Ok(media) => (media, None),
                        Err(error) => (Vec::new(), Some(error)),
                    };
                    if media.is_empty() {
                        let Some(id) = super::id_of(&work) else {
                            return Some((
                                Err(error("Pixiv series illustration contains an invalid ID")),
                                state,
                            ));
                        };
                        let mut metadata = self.empty_work_metadata(&work, &id, failure.is_none());
                        metadata.series = Some(series);
                        metadata.num_series = Some(number);
                        state.pending = VecDeque::from([
                            ExtractionEvent::Post(Box::new(Post {
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
                        item.filename = format!("{}_{}", prefix(number), item.filename);
                        let metadata = item.metadata.as_mut().expect("Pixiv illustration metadata");
                        metadata.series = Some(series.clone());
                        metadata.num_series = Some(number);
                    }
                    state.pending = self.finish(media);
                    continue;
                }
                if let Err(error) = self.series_page(&mut state).await {
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
        event::ExtractionEvent,
        job_selection::JobSelection,
        pixiv::tests::{pixiv, response, serve, serve_with, token},
        post_selection::PostSelection,
        selection::RangeSelection,
    };
    use serde_json::json;
    fn work(id: u64, kind: &str, pages: usize) -> Value {
        json!({"id":id,"type":kind,"title":"Title","caption":"Artwork caption","user":{"id":5,"account":"original-artist"},"create_date":"2024-01-01T09:00:00+09:00",
            "meta_single_page":{"original_image_url":format!("https://i.pximg.net/img-original/{id}_p0.jpg")},
            "meta_pages":if pages<=1{Vec::new()}else{(0..pages).map(|index|json!({"image_urls":{"original":format!("https://i.pximg.net/img-original/{id}_p{index}.png")}})).collect::<Vec<_>>()}})
    }
    fn detail(total: u64) -> Value {
        json!({"id":7,"title":"First series","caption":"<p>Raw series caption</p>","series_work_count":total,"private":"SECRET"})
    }
    fn page(works: Vec<Value>, detail: Value, next: Option<Url>) -> Vec<u8> {
        response(
            "200 OK",
            json!({"illusts":works,"illust_series_detail":detail,"next_url":next})
                .to_string()
                .as_bytes(),
        )
    }
    fn url() -> Url {
        Url::parse("https://www.pixiv.net/user/999/series/7").unwrap()
    }

    #[test]
    fn route_and_public_metadata_preserve_counters_raw_caption_and_signed_zero_padding() {
        assert_eq!(
            super::super::target(&url()).unwrap(),
            super::super::Target::Series {
                owner: "999".into(),
                id: "7".into()
            }
        );
        for raw in [
            "https://www.pixiv.net/en/user/999/series/7",
            "https://www.pixiv.net/users/999/series/7",
            "https://www.pixiv.net/user/bad/series/7",
            "https://www.pixiv.net/user/999/series/0",
        ] {
            assert!(super::super::target(&Url::parse(raw).unwrap()).is_err());
        }
        let metadata = SeriesMetadata::parse_compat(
            &json!({"id":7,"title":"Title","caption":"<b>raw</b>","total":0}),
        )
        .unwrap();
        assert!(metadata.valid());
        assert_eq!(metadata.caption.as_deref(), Some("<b>raw</b>"));
        for (number, expected) in [
            (4, "004"),
            (0, "000"),
            (-1, "0-1"),
            (-12, "-12"),
            (1234, "1234"),
        ] {
            assert_eq!(prefix(number), expected);
        }
        for (key, value) in [
            ("id", json!("bad")),
            ("total", json!(-1)),
            ("total", json!(u64::MAX)),
            ("title", json!(false)),
            ("caption", json!("x".repeat(MAX_TEXT + 1))),
        ] {
            let mut data = json!({"id":7,"total":4});
            data[key] = value;
            assert!(SeriesMetadata::parse_compat(&data).is_none());
        }
    }

    #[tokio::test]
    async fn recorded_upstream_series_oracle_matches_http_metadata_counters_and_filenames() {
        let oracle: Value =
            serde_json::from_slice(include_bytes!("../tests/fixtures/pixiv/series.json")).unwrap();
        let (root, server) = serve_with(|root| {
            let mut replies = vec![token("series-access")];
            for original in oracle["responses"].as_array().unwrap() {
                let mut body = original.clone();
                for raw in body["illusts"].as_array_mut().unwrap() {
                    let id = raw["id"].as_u64().unwrap();
                    let user = raw["user"]["id"].clone();
                    let mut enriched = work(id, raw["type"].as_str().unwrap(), 1);
                    enriched["user"]["id"] = user;
                    *raw = enriched;
                }
                if let Some(raw) = body["next_url"].as_str() {
                    let source = Url::parse(raw).unwrap();
                    let mut next = root.join(source.path().trim_start_matches('/')).unwrap();
                    next.set_query(source.query());
                    body["next_url"] = json!(next);
                }
                replies.push(response("200 OK", body.to_string().as_bytes()));
            }
            replies
        });
        let events = pixiv(root)
            .with_ugoira(false)
            .events(&Url::parse(oracle["url"].as_str().unwrap()).unwrap())
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        let expected = oracle["works"].as_array().unwrap();
        let mut posts = 0;
        let mut files = 0;
        for event in events {
            match event {
                ExtractionEvent::Post(post) => {
                    posts += 1;
                    let metadata = post.metadata.as_ref().unwrap();
                    assert_ne!(metadata.post_id, "43");
                    let source = expected
                        .iter()
                        .find(|work| work["id"].as_u64().unwrap().to_string() == metadata.post_id)
                        .unwrap();
                    assert_eq!(metadata.num_series, source["num_series"].as_i64());
                    assert_eq!(
                        metadata.series.as_deref(),
                        Some(&SeriesMetadata::parse_compat(&source["series"]).unwrap())
                    );
                    if metadata.post_id == "44" {
                        assert_eq!(post.count, Some(0));
                    }
                }
                ExtractionEvent::Media(media) => {
                    files += 1;
                    let metadata = media.metadata.as_ref().unwrap();
                    let source = expected
                        .iter()
                        .find(|work| work["id"].as_u64().unwrap().to_string() == metadata.post_id)
                        .unwrap();
                    assert_eq!(
                        media.filename,
                        format!("{}.jpg", source["filename_stem"].as_str().unwrap())
                    );
                    assert_eq!(metadata.num_series, source["num_series"].as_i64());
                    assert_eq!(metadata.username.as_deref(), Some("original-artist"));
                    assert_eq!(
                        media.archive_key(),
                        super::super::parse_illust(&work(
                            metadata.post_id.parse().unwrap(),
                            "illust",
                            1
                        ))
                        .unwrap()[0]
                            .archive_key()
                    );
                }
                _ => {}
            }
        }
        assert_eq!(posts, 5);
        assert_eq!(files, 4);
        let requests = server.join().unwrap();
        let calls = oracle["calls"].as_array().unwrap();
        assert_eq!(requests.len(), calls.len() + 1);
        for (request, call) in requests[1..].iter().zip(calls) {
            let actual = Url::parse(&format!(
                "http://localhost{}",
                request.split_whitespace().nth(1).unwrap()
            ))
            .unwrap();
            assert_eq!(actual.path(), call["endpoint"].as_str().unwrap());
            let actual: std::collections::BTreeMap<_, _> = actual
                .query_pairs()
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect();
            let expected: std::collections::BTreeMap<_, _> = call["params"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(key, value)| {
                    (
                        key.clone(),
                        value
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| value.to_string()),
                    )
                })
                .collect();
            assert_eq!(actual, expected);
            assert!(!request.contains("user_id=999"));
        }
    }

    #[tokio::test]
    async fn multi_page_work_shares_series_position_and_preserves_original_archive_identity() {
        let (root, server) = serve(vec![
            token("t"),
            page(vec![work(41, "illust", 2)], detail(4), None),
        ]);
        let files = pixiv(root)
            .stream(&url())
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            files
                .iter()
                .map(|file| file.filename.as_str())
                .collect::<Vec<_>>(),
            ["004_41_p0.png", "004_41_p1.png"]
        );
        let original = super::super::parse_illust(&work(41, "illust", 2)).unwrap();
        for (file, original) in files.iter().zip(original) {
            let metadata = file.metadata.as_ref().unwrap();
            assert_eq!(metadata.num_series, Some(4));
            assert_eq!(file.archive_key(), original.archive_key());
            assert_eq!(
                metadata.media_id,
                original.metadata.as_ref().unwrap().media_id
            );
            assert!(!serde_json::to_string(file).unwrap().contains("SECRET"));
        }
        assert_eq!(server.join().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn raw_missing_users_and_empty_animations_decrement_signed_positions_before_post_selection()
     {
        for range in ["1", "2"] {
            let mut missing = work(40, "illust", 1);
            missing["user"]["id"] = json!(0);
            let (root, server) = serve(vec![
                token("t"),
                page(
                    vec![missing, work(41, "ugoira", 1), work(42, "illust", 1)],
                    detail(1),
                    None,
                ),
            ]);
            let native = pixiv(root).with_ugoira(false);
            let files = JobSelection {
                post_selection: Some(PostSelection::new(
                    Some(RangeSelection::parse(range).unwrap()),
                    None,
                )),
                ..Default::default()
            }
            .apply(native.events(&url()))
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
            assert_eq!(files.len(), usize::from(range == "2"));
            if let Some(file) = files.first() {
                assert_eq!(file.filename, "0-1_42_p0.jpg");
                assert_eq!(file.metadata.as_ref().unwrap().num_series, Some(-1));
            }
            assert_eq!(server.join().unwrap().len(), 2);
        }
    }

    #[tokio::test]
    async fn failed_animation_closes_empty_post_and_preserves_later_number_and_selection() {
        for range in ["1", "2"] {
            let (root, server) = serve(vec![
                token("t"),
                page(
                    vec![work(41, "ugoira", 1), work(42, "illust", 1)],
                    detail(1),
                    None,
                ),
                response("503 Service Unavailable", b"PRIVATE"),
            ]);
            let native = pixiv(root);
            let results = JobSelection {
                post_selection: Some(PostSelection::new(
                    Some(RangeSelection::parse(range).unwrap()),
                    None,
                )),
                ..Default::default()
            }
            .apply(native.events(&url()))
            .collect::<Vec<_>>()
            .await;
            assert!(results[0].is_err());
            let files = results
                .iter()
                .filter_map(|result| result.as_ref().ok())
                .collect::<Vec<_>>();
            assert_eq!(files.len(), usize::from(range == "2"));
            if let Some(file) = files.first() {
                assert_eq!(file.filename, "000_42_p0.jpg");
            }
            assert_eq!(server.join().unwrap().len(), 3);
        }
    }

    #[tokio::test]
    async fn first_page_detail_is_required_only_when_a_work_exists_and_later_detail_does_not_replace_it()
     {
        let (root, server) = serve(vec![token("t"), page(Vec::new(), Value::Null, None)]);
        assert!(
            pixiv(root)
                .stream(&url())
                .collect::<Vec<_>>()
                .await
                .is_empty()
        );
        assert_eq!(server.join().unwrap().len(), 2);
        let (root, server) = serve_with(|root| {
            vec![
                token("t"),
                page(
                    Vec::new(),
                    Value::Null,
                    Some(
                        root.join("v1/illust/series?illust_series_id=7&offset=30")
                            .unwrap(),
                    ),
                ),
                page(vec![work(41, "illust", 1)], detail(4), None),
            ]
        });
        let results = pixiv(root).stream(&url()).collect::<Vec<_>>().await;
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].as_ref().unwrap_err().to_string(),
            "Pixiv returned invalid illustration series metadata"
        );
        assert_eq!(server.join().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn dropping_first_series_file_keeps_later_ugoira_and_page_requests_lazy() {
        let (root, server) = serve_with(|root| {
            vec![
                token("t"),
                page(
                    vec![work(41, "illust", 1), work(42, "ugoira", 1)],
                    detail(4),
                    Some(
                        root.join("v1/illust/series?illust_series_id=7&offset=30")
                            .unwrap(),
                    ),
                ),
            ]
        });
        let native = pixiv(root);
        let mut files = native.stream(&url());
        assert_eq!(
            files.next().await.unwrap().unwrap().filename,
            "004_41_p0.jpg"
        );
        drop(files);
        assert_eq!(server.join().unwrap().len(), 2);
    }

    #[test]
    fn next_page_pins_series_origin_endpoint_and_unique_id_while_allowing_offset_change() {
        let native = Pixiv::new(std::time::Duration::from_secs(1)).unwrap();
        assert!(
            native
                .series_next(
                    "7",
                    "https://app-api.pixiv.net/v1/illust/series?illust_series_id=7&offset=30"
                )
                .is_ok()
        );
        for raw in [
            "https://evil.example/v1/illust/series?illust_series_id=7",
            "https://app-api.pixiv.net/v1/novel/series?illust_series_id=7",
            "https://u:p@app-api.pixiv.net/v1/illust/series?illust_series_id=7",
            "https://app-api.pixiv.net/v1/illust/series?illust_series_id=8",
            "https://app-api.pixiv.net/v1/illust/series?illust_series_id=7&illust_series_id=7",
        ] {
            assert!(native.series_next("7", raw).is_err(), "{raw}");
        }
    }

    #[tokio::test]
    async fn canonical_cycle_guard_page_bound_and_private_http_errors_are_bounded() {
        let (root, server) = serve_with(|root| {
            vec![
                token("t"),
                page(
                    Vec::new(),
                    detail(4),
                    Some(
                        root.join("v1/illust/series?offset=0&illust_series_id=7")
                            .unwrap(),
                    ),
                ),
            ]
        });
        let results = pixiv(root).stream(&url()).collect::<Vec<_>>().await;
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].as_ref().unwrap_err().to_string(),
            "Pixiv illustration series pagination repeated a page"
        );
        assert_eq!(server.join().unwrap().len(), 2);
        let (root, server) = serve_with(|root| {
            let mut replies = vec![token("t")];
            replies.extend((1..=super::super::MAX_PAGES).map(|offset| {
                page(
                    Vec::new(),
                    detail(4),
                    Some(
                        root.join(&format!(
                            "v1/illust/series?illust_series_id=7&offset={offset}"
                        ))
                        .unwrap(),
                    ),
                )
            }));
            replies
        });
        let results = pixiv(root).stream(&url()).collect::<Vec<_>>().await;
        assert!(results.is_empty(), "{results:?}");
        assert_eq!(server.join().unwrap().len(), super::super::MAX_PAGES + 1);
        let (root, server) = serve(vec![token("t"), response("403 Forbidden", b"PRIVATE")]);
        let results = pixiv(root).stream(&url()).collect::<Vec<_>>().await;
        assert_eq!(results.len(), 1);
        assert!(results[0].is_err());
        assert!(
            !results[0]
                .as_ref()
                .unwrap_err()
                .to_string()
                .contains("PRIVATE")
        );
        assert_eq!(server.join().unwrap().len(), 2);
    }
    #[tokio::test]
    async fn max_posts_counts_raw_series_works_including_missing_users_and_empty_animation() {
        let mut missing = work(40, "illust", 1);
        missing["user"]["id"] = json!(0);
        let (root, server) = serve(vec![
            token("t"),
            page(
                vec![missing, work(41, "ugoira", 1), work(42, "illust", 1)],
                detail(4),
                Some(Url::parse("https://unexpected.invalid/page").unwrap()),
            ),
        ]);
        let events = pixiv(root)
            .with_ugoira(false)
            .with_max_posts(Some(2))
            .events(&url())
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(events.len(), 2);
        let ExtractionEvent::Post(post) = &events[0] else {
            panic!("expected empty post")
        };
        assert_eq!(post.count, Some(0));
        assert_eq!(post.metadata.as_ref().unwrap().num_series, Some(3));
        assert!(matches!(events[1], ExtractionEvent::EndPost));
        assert_eq!(server.join().unwrap().len(), 2);
    }
}
