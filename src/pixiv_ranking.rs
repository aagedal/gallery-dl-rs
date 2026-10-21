//! Pixiv rankings retain the ranking-day namespace and content-filtered ranks.
use super::Pixiv;
use crate::{Result, error, event::ExtractionEvent};
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

const MODES: &[(&str, &str)] = &[
    ("daily", "day"),
    ("daily_r18", "day_r18"),
    ("daily_ai", "day_ai"),
    ("daily_r18_ai", "day_r18_ai"),
    ("weekly", "week"),
    ("weekly_r18", "week_r18"),
    ("monthly", "month"),
    ("male", "day_male"),
    ("male_r18", "day_male_r18"),
    ("female", "day_female"),
    ("female_r18", "day_female_r18"),
    ("original", "week_original"),
    ("rookie", "week_rookie"),
    ("r18g", "week_r18g"),
];

fn decimal_digit(value: char) -> bool {
    crate::python_unicode::decimal_digit(value).is_some()
}

fn valid_date(date: &str) -> bool {
    let mut chars = date.chars();
    for index in 0..10 {
        let Some(value) = chars.next() else {
            return false;
        };
        if matches!(index, 4 | 7) {
            if value != '-' {
                return false;
            }
        } else if !decimal_digit(value) {
            return false;
        }
    }
    chars.next().is_none()
}

fn query_date(raw: &str) -> Option<String> {
    let chars: Vec<_> = raw.chars().take(9).collect();
    if chars.len() != 8 || !chars.iter().copied().all(decimal_digit) {
        return None;
    }
    let mut date = String::new();
    for (index, value) in chars.into_iter().enumerate() {
        if matches!(index, 4 | 6) {
            date.push('-');
        }
        date.push(value);
    }
    Some(date)
}
fn valid_kind(kind: &str) -> bool {
    !kind.is_empty() && kind.len() <= 1024 && !kind.contains('\0')
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RankingOptions {
    /// Canonical App API mode, after translating the ranking.php alias.
    pub mode: String,
    pub date: String,
    /// Exact case-sensitive content filter; None/empty means no filtering.
    pub content: Option<String>,
}

impl RankingOptions {
    pub fn parse(url: &Url) -> Result<Self> {
        Self::parse_at(url, time::OffsetDateTime::now_utc().unix_timestamp())
    }

    pub fn parse_at(url: &Url, unix_seconds: i64) -> Result<Self> {
        let invalid = || error("unsupported Pixiv ranking URL or options");
        if !matches!(url.scheme(), "http" | "https")
            || !matches!(url.host_str(), Some("pixiv.net" | "www.pixiv.net"))
            || url.path() != "/ranking.php"
            || url.port().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(invalid());
        }
        // text.parse_query ignores bare tokens, then keeps the first decoded key.
        let query = |name: &str| {
            url.query()
                .unwrap_or("")
                .split('&')
                .filter(|pair| pair.contains('='))
                .filter_map(|pair| url::form_urlencoded::parse(pair.as_bytes()).next())
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.into_owned())
        };
        let mode = query("mode")
            .unwrap_or_else(|| "daily".into())
            .to_lowercase();
        let mode = MODES
            .iter()
            .find(|(alias, _)| *alias == mode)
            .map(|(_, mode)| (*mode).to_owned())
            .ok_or_else(|| error("invalid Pixiv ranking mode"))?;
        let date = query("date").as_deref().and_then(query_date);
        let date = match date {
            Some(date) => date,
            None => {
                let date = time::OffsetDateTime::from_unix_timestamp(unix_seconds)
                    .ok()
                    .and_then(|now| now.checked_sub(time::Duration::days(1)))
                    .ok_or_else(invalid)?;
                format!(
                    "{:04}-{:02}-{:02}",
                    date.year(),
                    u8::from(date.month()),
                    date.day()
                )
            }
        };
        let content = query("content");
        let options = Self {
            mode,
            date,
            content,
        };
        if !options.valid() {
            return Err(invalid());
        }
        Ok(options)
    }

    fn valid(&self) -> bool {
        MODES.iter().any(|(_, mode)| *mode == self.mode)
            && valid_date(&self.date)
            && self
                .content
                .as_ref()
                .is_none_or(|kind| kind.is_empty() || valid_kind(kind))
    }

    fn first(&self, native: &Pixiv) -> Url {
        let mut url = native
            .api_root
            .join("v1/illust/ranking")
            .expect("static ranking endpoint");
        url.query_pairs_mut()
            .append_pair("mode", &self.mode)
            .append_pair("date", &self.date);
        url
    }

    fn checked(&self, native: &Pixiv, raw: &str) -> Result<Url> {
        let invalid = || error("Pixiv returned an unexpected ranking next page");
        let url = Url::parse(raw).map_err(|_| invalid())?;
        let expected = self.first(native);
        let pairs: Vec<_> = url.query_pairs().collect();
        let scope = expected.query_pairs().all(|(key, value)| {
            let mut entries = pairs.iter().filter(|(actual, _)| *actual == key);
            entries.next().is_some_and(|(_, actual)| *actual == value) && entries.next().is_none()
        });
        if url.origin() != native.api_root.origin()
            || url.path() != expected.path()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || !scope
        {
            return Err(invalid());
        }
        Ok(url)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RankingMetadata {
    pub mode: String,
    pub date: String,
    pub rank: u64,
    #[serde(rename = "type")]
    pub kind: String,
}
impl RankingMetadata {
    pub fn valid(&self) -> bool {
        self.rank > 0
            && MODES.iter().any(|(_, mode)| *mode == self.mode)
            && valid_date(&self.date)
            && valid_kind(&self.kind)
    }
    pub fn parse_compat(value: &Value) -> Option<Self> {
        let mode = value.get("mode")?.as_str()?;
        let date = value.get("date")?.as_str()?;
        let rank = value.get("rank")?.as_u64()?;
        let kind = value.get("type")?.as_str()?;
        if rank == 0
            || !MODES.iter().any(|(_, canonical)| *canonical == mode)
            || !valid_date(date)
            || !valid_kind(kind)
        {
            return None;
        }
        let metadata = Self {
            mode: mode.to_owned(),
            date: date.to_owned(),
            rank,
            kind: kind.to_owned(),
        };
        metadata.valid().then_some(metadata)
    }
}

struct State {
    budget: super::PostBudget,
    options: RankingOptions,
    next: Option<String>,
    pages: usize,
    seen: BTreeSet<String>,
    works: VecDeque<Value>,
    pending: VecDeque<ExtractionEvent>,
    rank: u64,
    done: bool,
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64() != Some(0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

impl Pixiv {
    async fn ranking_page(&self, state: &mut State) -> Result<()> {
        if state.pages >= super::MAX_PAGES {
            state.done = true;
            return Ok(());
        }
        let Some(raw) = state.next.take() else {
            state.done = true;
            return Ok(());
        };
        let endpoint = state.options.checked(self, &raw)?;
        let mut canonical = endpoint.clone();
        let mut pairs: Vec<_> = endpoint
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        pairs.sort();
        canonical.set_query(None);
        canonical.query_pairs_mut().extend_pairs(pairs);
        if !state.seen.insert(canonical.to_string()) {
            return Err(error("Pixiv ranking pagination repeated a page"));
        }
        let mut body = super::json(&self.get(&endpoint).await?)?;
        let works = body
            .get_mut("illusts")
            .and_then(Value::as_array_mut)
            .ok_or_else(|| error("Pixiv response contains no ranking illustration list"))?;
        state.works = std::mem::take(works).into();
        state.next = super::string(&body, "next_url").map(str::to_owned);
        state.pages += 1;
        Ok(())
    }

    pub(super) fn ranking_events(
        &self,
        options: RankingOptions,
    ) -> BoxStream<'_, Result<ExtractionEvent>> {
        if !options.valid() {
            return stream::once(async { Err(error("invalid Pixiv ranking options")) }).boxed();
        }
        if self.refresh_token.is_none() {
            return stream::once(async { Err(super::missing_token()) }).boxed();
        }
        let next = Some(options.first(self).to_string());
        let state = State {
            budget: super::PostBudget::new(self.max_posts),
            options,
            next,
            pages: 0,
            seen: BTreeSet::new(),
            works: VecDeque::new(),
            pending: VecDeque::new(),
            rank: 0,
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
                if let Some(mut work) = state.works.pop_front() {
                    skipped = skipped.wrapping_add(1);
                    if skipped == 0 {
                        tokio::task::yield_now().await;
                    }
                    if let Some(content) = state
                        .options
                        .content
                        .as_deref()
                        .filter(|content| !content.is_empty())
                        && work.get("type").and_then(Value::as_str) != Some(content)
                    {
                        continue;
                    }
                    state.budget.consume();
                    let Some(rank) = state.rank.checked_add(1) else {
                        state.done = true;
                        return Some((Err(error("Pixiv ranking counter overflow")), state));
                    };
                    state.rank = rank;
                    // The source enumerates before common missing-user/media
                    // checks, so those skipped works leave visible rank gaps.
                    if !work
                        .get("user")
                        .and_then(|user| user.get("id"))
                        .is_some_and(truthy)
                    {
                        continue;
                    }
                    let (mut media, failure) = match self.work(&mut work, true).await {
                        Ok(media) => (media, None),
                        Err(super::WorkFailure::Enrichment(error)) => {
                            state.done = true;
                            state.works.clear();
                            return Some((Err(error), state));
                        }
                        // Preserve animation errors and empty-post positions.
                        Err(super::WorkFailure::Files(error)) => (Vec::new(), Some(error)),
                    };
                    let ranking = Arc::new(RankingMetadata {
                        mode: state.options.mode.clone(),
                        date: state.options.date.clone(),
                        rank,
                        kind: state
                            .options
                            .content
                            .as_deref()
                            .filter(|content| !content.is_empty())
                            .unwrap_or("all")
                            .to_owned(),
                    });
                    if media.is_empty() {
                        let Some(id) = super::id_of(&work) else {
                            return Some((
                                Err(error("Pixiv ranking illustration contains an invalid ID")),
                                state,
                            ));
                        };
                        let mut metadata = self.empty_work_metadata(&work, &id, failure.is_none());
                        metadata.ranking = Some(ranking);
                        metadata.ranking_archive = true;
                        state.pending = VecDeque::from([
                            ExtractionEvent::Post(Box::new(crate::event::Post {
                                keywords: Default::default(),
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
                        let metadata = item.metadata.as_mut().expect("Pixiv illustration metadata");
                        metadata.ranking = Some(ranking.clone());
                        metadata.ranking_archive = true;
                    }
                    state.pending = self.finish(media);
                    continue;
                }
                if state.done {
                    return None;
                }
                if let Err(error) = self.ranking_page(&mut state).await {
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
        pixiv::tests::{pixiv, response, serve, serve_with, token},
    };
    use serde_json::json;

    fn work(id: u64, kind: &str, pages: usize) -> Value {
        json!({"id":id,"type":kind,"title":"Title","user":{"id":5,"account":"original-artist"},
            "create_date":"2024-01-01T09:00:00+09:00",
            "meta_single_page":{"original_image_url":format!("https://i.pximg.net/img-original/{id}_p0.jpg")},
            "meta_pages":if pages<=1{Vec::new()}else{(0..pages).map(|index|json!({"image_urls":{"original":format!("https://i.pximg.net/img-original/{id}_p{index}.png")}})).collect::<Vec<_>>()}})
    }
    fn page(works: Vec<Value>, next: Option<Url>) -> Vec<u8> {
        response(
            "200 OK",
            json!({"illusts":works,"next_url":next})
                .to_string()
                .as_bytes(),
        )
    }
    fn url(content: &str) -> Url {
        Url::parse(&format!(
            "https://www.pixiv.net/ranking.php?date=20240101{content}"
        ))
        .unwrap()
    }

    #[test]
    fn query_aliases_dates_first_values_and_literal_content_match_upstream() {
        for (alias, mode) in MODES {
            let url = Url::parse(&format!(
                "https://www.pixiv.net/ranking.php?mode={}&date=20240231",
                alias.to_uppercase()
            ))
            .unwrap();
            let options = RankingOptions::parse_at(&url, 1704067200).unwrap();
            assert_eq!(&options.mode, mode);
            assert_eq!(options.date, "2024-02-31");
        }
        for suffix in [
            "",
            "?date=",
            "?date=2024-01-01",
            "?date=bad",
            "?date=²²²²²²²²",
        ] {
            let raw = Url::parse(&format!("https://www.pixiv.net/ranking.php{suffix}")).unwrap();
            assert_eq!(
                RankingOptions::parse_at(&raw, 1704067200).unwrap().date,
                "2023-12-31"
            );
        }
        let raw=Url::parse("https://www.pixiv.net/ranking.php?mode&mode=weekly&mode=daily&date=２０２６１０２０&content=ILLUST&content=illust").unwrap();
        let options = RankingOptions::parse_at(&raw, 1704067200).unwrap();
        assert_eq!(options.mode, "week");
        assert_eq!(options.date, "２０２６-１０-２０");
        assert_eq!(options.content.as_deref(), Some("ILLUST"));
        assert_eq!(
            RankingOptions::parse_at(
                &Url::parse("https://www.pixiv.net/ranking.php?MODE=weekly").unwrap(),
                1704067200
            )
            .unwrap()
            .mode,
            "day"
        );
        for raw in [
            "https://www.pixiv.net/ranking.php?mode=day",
            "https://www.pixiv.net/ranking.php?mode=",
            "https://www.pixiv.net/en/ranking.php",
            "https://u:p@www.pixiv.net/ranking.php",
        ] {
            assert!(
                RankingOptions::parse_at(&Url::parse(raw).unwrap(), 1704067200).is_err(),
                "{raw}"
            );
        }
        assert!(matches!(
            super::super::target(&url("")).unwrap(),
            super::super::Target::Ranking(_)
        ));
    }

    #[test]
    fn compatibility_metadata_is_bounded_validated_and_preserves_unicode_decimal_dates() {
        let value = json!({"mode":"day","date":"２０２６-１０-２０","rank":7,"type":"all","secret":"omitted"});
        let ranking = RankingMetadata::parse_compat(&value).unwrap();
        assert!(ranking.valid());
        assert_eq!(
            serde_json::to_value(ranking).unwrap(),
            json!({"mode":"day","date":"２０２６-１０-２０","rank":7,"type":"all"})
        );
        for (key, value) in [
            ("mode", json!("daily")),
            ("mode", json!("x".repeat(1000))),
            ("date", json!("20240101")),
            ("date", json!("2024-1-01")),
            ("date", json!("²²²²-²²-²²")),
            ("rank", json!(0)),
            ("rank", json!(-1)),
            ("rank", json!(1.5)),
            ("rank", json!("1")),
            ("type", json!("")),
            ("type", json!("x".repeat(1025))),
            ("type", json!("nul\0")),
        ] {
            let mut invalid = json!({"mode":"day","date":"2024-01-01","rank":1,"type":"all"});
            invalid[key] = value;
            assert!(RankingMetadata::parse_compat(&invalid).is_none(), "{key}");
        }
    }

    #[tokio::test]
    async fn content_filter_precedes_rank_but_missing_users_and_media_leave_rank_gaps() {
        let mut no_user = work(11, "illust", 1);
        no_user["user"]["id"] = json!(0);
        let mut no_media = work(12, "illust", 1);
        no_media["meta_single_page"] = json!({});
        let (root, server) = serve_with(|root| {
            vec![
                token("ranking-access"),
                page(
                    vec![
                        work(10, "manga", 1),
                        no_user,
                        no_media,
                        work(13, "illust", 2),
                    ],
                    Some(
                        root.join("v1/illust/ranking?mode=day&date=2024-01-01&offset=30")
                            .unwrap(),
                    ),
                ),
                page(vec![work(14, "illust", 1)], None),
            ]
        });
        let native = pixiv(root);
        let events = native
            .events(&url("&content=illust"))
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        let mut ranks = Vec::new();
        let mut posts = Vec::new();
        for event in events {
            match event {
                ExtractionEvent::Post(post) => {
                    let metadata = post.metadata.unwrap();
                    assert!(metadata.ranking_archive);
                    posts.push(metadata.ranking.unwrap().rank);
                }
                ExtractionEvent::Media(media) => {
                    let metadata = media.metadata.as_ref().unwrap();
                    let ranking = metadata.ranking.as_ref().unwrap();
                    ranks.push(ranking.rank);
                    assert_eq!(ranking.kind, "illust");
                    assert_eq!(ranking.date, "2024-01-01");
                    assert_eq!(metadata.timestamp, Some(1704067200));
                    assert_eq!(metadata.username.as_deref(), Some("original-artist"));
                    assert!(metadata.ranking_archive);
                    assert!(media.archive_key().is_some());
                }
                _ => {}
            }
        }
        assert_eq!(posts, [2, 3, 4]);
        assert_eq!(ranks, [3, 3, 4]);
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[1].starts_with("GET /v1/illust/ranking?mode=day&date=2024-01-01 "));
        assert!(!requests[1].contains("content="));
    }

    #[tokio::test]
    async fn disabled_animation_keeps_empty_post_and_post_range_positions() {
        use crate::{
            job_selection::JobSelection, post_selection::PostSelection, selection::RangeSelection,
        };
        for range in ["1", "2"] {
            let (root, server) = serve(vec![
                token("t"),
                page(vec![work(41, "ugoira", 1), work(42, "illust", 1)], None),
            ]);
            let native = pixiv(root).with_ugoira(false);
            let files = JobSelection {
                post_selection: Some(PostSelection::new(
                    Some(RangeSelection::parse(range).unwrap()),
                    None,
                )),
                ..Default::default()
            }
            .apply(native.events(&url("")))
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
            assert_eq!(files.len(), usize::from(range == "2"));
            if let Some(file) = files.first() {
                assert_eq!(file.filename, "42_p0.jpg");
                assert_eq!(
                    file.metadata
                        .as_ref()
                        .unwrap()
                        .ranking
                        .as_ref()
                        .unwrap()
                        .rank,
                    2
                );
            }
            assert_eq!(server.join().unwrap().len(), 2);
        }
        let (root, server) = serve(vec![token("t"), page(vec![work(41, "ugoira", 1)], None)]);
        let events = pixiv(root)
            .with_ugoira(false)
            .events(&url(""))
            .collect::<Vec<_>>()
            .await;
        let Ok(ExtractionEvent::Post(post)) = &events[0] else {
            panic!("missing empty animation post")
        };
        assert_eq!(post.count, Some(0));
        let metadata = post.metadata.as_ref().unwrap();
        assert_eq!(metadata.post_id, "41");
        assert_eq!(metadata.username.as_deref(), Some("original-artist"));
        assert_eq!(metadata.ranking.as_ref().unwrap().rank, 1);
        assert!(metadata.ranking_archive);
        assert!(matches!(events[1], Ok(ExtractionEvent::EndPost)));
        assert_eq!(events.len(), 2);
        assert_eq!(server.join().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn failed_animation_retains_empty_post_range_position_after_reported_error() {
        use crate::{
            job_selection::JobSelection, post_selection::PostSelection, selection::RangeSelection,
        };
        for range in ["1", "2"] {
            let (root, server) = serve(vec![
                token("t"),
                page(vec![work(41, "ugoira", 1), work(42, "illust", 1)], None),
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
            .apply(native.events(&url("")))
            .collect::<Vec<_>>()
            .await;
            assert!(results[0].is_err());
            let files = results
                .iter()
                .filter_map(|result| result.as_ref().ok())
                .collect::<Vec<_>>();
            assert_eq!(files.len(), usize::from(range == "2"));
            if let Some(file) = files.first() {
                assert_eq!(file.filename, "42_p0.jpg");
                assert_eq!(
                    file.metadata
                        .as_ref()
                        .unwrap()
                        .ranking
                        .as_ref()
                        .unwrap()
                        .rank,
                    2
                );
            }
            assert_eq!(server.join().unwrap().len(), 3);
        }
    }

    #[tokio::test]
    async fn empty_content_means_all_but_unknown_literal_all_and_case_mismatch_keep_paging_without_files()
     {
        for content in ["", "all", "unknown", "ILLUST"] {
            let (root, server) = serve_with(|root| {
                vec![
                    token("t"),
                    page(
                        vec![work(41, "illust", 1)],
                        Some(
                            root.join("v1/illust/ranking?mode=day&date=2024-01-01&offset=30")
                                .unwrap(),
                        ),
                    ),
                    page(vec![work(42, "manga", 1)], None),
                ]
            });
            let files = pixiv(root)
                .stream(&url(&format!("&content={content}")))
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .collect::<Result<Vec<_>>>()
                .unwrap();
            assert_eq!(
                files.len(),
                if content.is_empty() { 2 } else { 0 },
                "{content}"
            );
            if let Some(file) = files.first() {
                assert_eq!(
                    file.metadata
                        .as_ref()
                        .unwrap()
                        .ranking
                        .as_ref()
                        .unwrap()
                        .kind,
                    "all"
                );
            }
            assert_eq!(server.join().unwrap().len(), 3);
        }
    }

    #[tokio::test]
    async fn ranking_archive_identity_uses_mode_date_and_artwork_not_rank_or_content() {
        let (root, server) = serve(vec![token("t"), page(vec![work(41, "illust", 1)], None)]);
        let media = pixiv(root)
            .stream(&url("&content=illust"))
            .collect::<Vec<_>>()
            .await
            .remove(0)
            .unwrap();
        let ordinary = super::super::parse_illust(&work(41, "illust", 1))
            .unwrap()
            .remove(0);
        assert_ne!(media.archive_key(), ordinary.archive_key());
        let mut changed = media.clone();
        let mut ranking = (**changed.metadata.as_ref().unwrap().ranking.as_ref().unwrap()).clone();
        ranking.rank = 99;
        ranking.kind = "all".into();
        changed.metadata.as_mut().unwrap().ranking = Some(Arc::new(ranking.clone()));
        assert_eq!(changed.archive_key(), media.archive_key());
        ranking.date = "2024-01-02".into();
        changed.metadata.as_mut().unwrap().ranking = Some(Arc::new(ranking.clone()));
        assert_ne!(changed.archive_key(), media.archive_key());
        ranking.date = "2024-01-01".into();
        ranking.mode = "week".into();
        changed.metadata.as_mut().unwrap().ranking = Some(Arc::new(ranking));
        assert_ne!(changed.archive_key(), media.archive_key());
        assert_eq!(server.join().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn ranking_ugoira_and_next_pages_are_fetched_only_when_consumed() {
        let (root, server) = serve_with(|root| {
            vec![
                token("t"),
                page(
                    vec![work(41, "illust", 1), work(42, "ugoira", 1)],
                    Some(
                        root.join("v1/illust/ranking?mode=day&date=2024-01-01&offset=30")
                            .unwrap(),
                    ),
                ),
            ]
        });
        let native = pixiv(root);
        let mut files = native.stream(&url(""));
        assert_eq!(files.next().await.unwrap().unwrap().filename, "41_p0.jpg");
        drop(files);
        assert_eq!(server.join().unwrap().len(), 2);
        let(root,server)=serve(vec![token("t"),page(vec![work(42,"ugoira",1)],None),response("200 OK",json!({"ugoira_metadata":{"zip_urls":{"medium":"https://i.pximg.net/img-zip-ugoira/42_ugoira600x600.zip"},"frames":[{"file":"000000.jpg","delay":100}]}}).to_string().as_bytes())]);
        let files = pixiv(root)
            .stream(&url("&content=ugoira"))
            .collect::<Vec<_>>()
            .await;
        assert_eq!(files.len(), 1);
        let media = files[0].as_ref().unwrap();
        assert_eq!(media.filename, "42_p0.zip");
        assert_eq!(
            media
                .metadata
                .as_ref()
                .unwrap()
                .ranking
                .as_ref()
                .unwrap()
                .rank,
            1
        );
        assert!(server.join().unwrap()[2].starts_with("GET /v1/ugoira/metadata?illust_id=42 "));
    }

    #[test]
    fn next_url_cannot_change_ranking_origin_endpoint_mode_date_or_duplicate_scope() {
        let native = Pixiv::new(std::time::Duration::from_secs(1)).unwrap();
        let options = RankingOptions::parse(&url("")).unwrap();
        for raw in [
            "https://evil.example/v1/illust/ranking?mode=day&date=2024-01-01",
            "https://app-api.pixiv.net/v1/user/illusts?mode=day&date=2024-01-01",
            "https://u:p@app-api.pixiv.net/v1/illust/ranking?mode=day&date=2024-01-01",
            "https://app-api.pixiv.net/v1/illust/ranking?mode=week&date=2024-01-01",
            "https://app-api.pixiv.net/v1/illust/ranking?mode=day&date=2024-01-02",
            "https://app-api.pixiv.net/v1/illust/ranking?mode=day&mode=day&date=2024-01-01",
            "https://app-api.pixiv.net/v1/illust/ranking?mode=day&date=2024-01-01&date=2024-01-01",
        ] {
            assert!(options.checked(&native, raw).is_err(), "{raw}");
        }
    }

    #[tokio::test]
    async fn ranking_cycle_guard_and_page_bound_stop_before_repeated_or_extra_http() {
        let (root, server) = serve_with(|root| {
            vec![
                token("t"),
                page(
                    Vec::new(),
                    Some(
                        root.join("v1/illust/ranking?date=2024-01-01&mode=day")
                            .unwrap(),
                    ),
                ),
            ]
        });
        let files = pixiv(root).stream(&url("")).collect::<Vec<_>>().await;
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].as_ref().unwrap_err().to_string(),
            "Pixiv ranking pagination repeated a page"
        );
        assert_eq!(server.join().unwrap().len(), 2);
        let (root, server) = serve_with(|root| {
            let mut replies = vec![token("t")];
            replies.extend((1..=super::super::MAX_PAGES).map(|offset| {
                page(
                    Vec::new(),
                    Some(
                        root.join(&format!(
                            "v1/illust/ranking?mode=day&date=2024-01-01&offset={offset}"
                        ))
                        .unwrap(),
                    ),
                )
            }));
            replies
        });
        let files = pixiv(root).stream(&url("")).collect::<Vec<_>>().await;
        assert!(files.is_empty(), "{files:?}");
        assert_eq!(server.join().unwrap().len(), super::super::MAX_PAGES + 1);
    }

    #[tokio::test]
    async fn ugoira_failure_recovers_later_rank_and_page_http_failure_is_private() {
        let (root, server) = serve(vec![
            token("t"),
            page(vec![work(41, "ugoira", 1), work(42, "illust", 1)], None),
            response("503 Service Unavailable", b"SECRET"),
        ]);
        let files = pixiv(root).stream(&url("")).collect::<Vec<_>>().await;
        assert_eq!(files.len(), 2);
        assert!(files[0].is_err());
        assert_eq!(
            files[1]
                .as_ref()
                .unwrap()
                .metadata
                .as_ref()
                .unwrap()
                .ranking
                .as_ref()
                .unwrap()
                .rank,
            2
        );
        assert!(
            !files[0]
                .as_ref()
                .unwrap_err()
                .to_string()
                .contains("SECRET")
        );
        assert_eq!(server.join().unwrap().len(), 3);
        let (root, server) = serve(vec![token("t"), response("403 Forbidden", b"PRIVATE")]);
        let files = pixiv(root).stream(&url("")).collect::<Vec<_>>().await;
        assert_eq!(files.len(), 1);
        assert!(files[0].is_err());
        assert!(
            !files[0]
                .as_ref()
                .unwrap_err()
                .to_string()
                .contains("PRIVATE")
        );
        assert_eq!(server.join().unwrap().len(), 2);
    }
    #[tokio::test]
    async fn max_posts_counts_matching_content_before_missing_user_and_enrichment() {
        for missing in [true, false] {
            let mut selected = work(41, "illust", 2);
            if missing {
                selected["user"]["id"] = json!(0);
            }
            let (root, server) = serve(vec![
                token("t"),
                page(
                    vec![work(40, "manga", 1), selected, work(42, "ugoira", 1)],
                    Some(Url::parse("https://unexpected.invalid/page").unwrap()),
                ),
            ]);
            let native = pixiv(root).with_max_posts(Some(1));
            let events = native
                .events(&url("&content=illust"))
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .collect::<Result<Vec<_>>>()
                .unwrap();
            let files: Vec<_> = events
                .into_iter()
                .filter_map(|event| {
                    if let ExtractionEvent::Media(file) = event {
                        Some(file)
                    } else {
                        None
                    }
                })
                .collect();
            assert_eq!(files.len(), if missing { 0 } else { 2 });
            for file in files {
                assert_eq!(file.metadata.unwrap().ranking.unwrap().rank, 1);
            }
            assert_eq!(server.join().unwrap().len(), 2);
        }
    }
}
