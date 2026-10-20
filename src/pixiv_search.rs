//! Pixiv illustration searches, including source-compatible date-window replay.
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
const SORTS: &[(&str, &str)] = &[
    ("date", "date_asc"),
    ("date_d", "date_desc"),
    ("popular_d", "popular_desc"),
    ("popular_male_d", "popular_male_desc"),
    ("popular_female_d", "popular_female_desc"),
];
const TARGETS: &[(&str, &str)] = &[
    ("tag", "partial_match_for_tags"),
    ("s_tag", "partial_match_for_tags"),
    ("tag_full", "exact_match_for_tags"),
    ("s_tag_full", "exact_match_for_tags"),
    ("tc", "title_and_caption"),
    ("s_tc", "title_and_caption"),
];

/// Immutable search context. Request date bounds may change during deep-search
/// rollover, while these original metadata fields and archive word stay fixed.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct SearchOptions {
    pub word: String,
    pub sort: String,
    pub target: String,
    pub date_start: Option<String>,
    pub date_end: Option<String>,
}
pub type SearchMetadata = SearchOptions;

pub(super) fn is_route(url: &Url) -> bool {
    matches!(url.path(), "/search" | "/search.php")
        || url.path().starts_with("/tags/")
        || url.path().starts_with("/en/tags/")
}

// urllib.parse.unquote leaves malformed escapes literal and replaces invalid
// decoded UTF-8, without treating a path's literal '+' as a space.
fn unquote(raw: &str) -> String {
    let mut bytes = Vec::with_capacity(raw.len());
    let raw = raw.as_bytes();
    let mut index = 0;
    while index < raw.len() {
        if raw[index] == b'%'
            && let Some(hex) = raw.get(index + 1..index + 3)
            && let Ok(hex) = std::str::from_utf8(hex)
            && let Ok(value) = u8::from_str_radix(hex, 16)
        {
            bytes.push(value);
            index += 3;
            continue;
        }
        bytes.push(raw[index]);
        index += 1;
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

impl SearchOptions {
    pub fn parse(url: &Url) -> Result<Self> {
        let invalid = || error("unsupported Pixiv search URL or options");
        if !matches!(url.scheme(), "http" | "https")
            || !matches!(url.host_str(), Some("pixiv.net" | "www.pixiv.net"))
            || url.port().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(invalid());
        }
        let query = |name: &str| {
            url.query()
                .unwrap_or("")
                .split('&')
                .filter(|pair| pair.contains('='))
                .filter_map(|pair| url::form_urlencoded::parse(pair.as_bytes()).next())
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.into_owned())
        };
        let path = super::segments(url);
        let word = match path.as_slice() {
            ["tags", word] | ["tags", word, _] | ["en", "tags", word] | ["en", "tags", word, _] => {
                unquote(word)
            }
            ["search"] | ["search.php"] => query("q")
                .filter(|word| !word.is_empty())
                .or_else(|| query("word"))
                .ok_or_else(|| error("Pixiv search requires a search term"))?,
            _ => return Err(invalid()),
        };
        let order = query("order").unwrap_or_else(|| "date_d".into());
        let sort = SORTS
            .iter()
            .find(|(alias, _)| *alias == order)
            .map(|(_, sort)| (*sort).to_owned())
            .ok_or_else(|| error("invalid Pixiv search order"))?;
        let mode = query("s_mode").unwrap_or_else(|| "s_tag_full".into());
        let target = TARGETS
            .iter()
            .find(|(alias, _)| *alias == mode)
            .map(|(_, target)| (*target).to_owned())
            .ok_or_else(|| error("invalid Pixiv search mode"))?;
        let options = Self {
            word,
            sort,
            target,
            date_start: query("scd"),
            date_end: query("ecd"),
        };
        if !options.valid() {
            return Err(invalid());
        }
        Ok(options)
    }

    pub fn valid(&self) -> bool {
        self.word.len() <= MAX_TEXT
            && SORTS.iter().any(|(_, sort)| *sort == self.sort)
            && TARGETS.iter().any(|(_, target)| *target == self.target)
            && self
                .date_start
                .as_ref()
                .is_none_or(|date| date.len() <= MAX_TEXT)
            && self
                .date_end
                .as_ref()
                .is_none_or(|date| date.len() <= MAX_TEXT)
    }

    pub fn parse_compat(value: &Value) -> Option<Self> {
        let word = value.get("word")?.as_str()?;
        let sort = value.get("sort")?.as_str()?;
        let target = value.get("target")?.as_str()?;
        if word.len() > MAX_TEXT
            || !SORTS.iter().any(|(_, canonical)| *canonical == sort)
            || !TARGETS.iter().any(|(_, canonical)| *canonical == target)
        {
            return None;
        }
        let date = |key| match value.get(key) {
            None | Some(Value::Null) => Some(None),
            Some(Value::String(value)) if value.len() <= MAX_TEXT => Some(Some(value.clone())),
            _ => None,
        };
        let result = Self {
            word: word.to_owned(),
            sort: sort.to_owned(),
            target: target.to_owned(),
            date_start: date("date_start")?,
            date_end: date("date_end")?,
        };
        result.valid().then_some(result)
    }

    fn first(&self, native: &Pixiv) -> Url {
        let mut url = native
            .api_root
            .join("v1/search/illust")
            .expect("static search endpoint");
        url.query_pairs_mut()
            .append_pair("word", &self.word)
            .append_pair("search_target", &self.target)
            .append_pair("sort", &self.sort);
        if let Some(date) = &self.date_start {
            url.query_pairs_mut().append_pair("start_date", date);
        }
        if let Some(date) = &self.date_end {
            url.query_pairs_mut().append_pair("end_date", date);
        }
        url
    }

    fn checked(&self, native: &Pixiv, raw: &str) -> Result<Url> {
        let invalid = || error("Pixiv returned an unexpected search next page");
        let url = Url::parse(raw).map_err(|_| invalid())?;
        let expected = self.first(native);
        let pairs: Vec<_> = url.query_pairs().collect();
        let scope = expected.query_pairs().all(|(key, value)| {
            let mut entries = pairs.iter().filter(|(actual, _)| *actual == key);
            entries.next().is_some_and(|(_, actual)| *actual == value) && entries.next().is_none()
        });
        let dates = (self.date_start.is_some()
            || !pairs.iter().any(|(key, _)| key == "start_date"))
            && (self.date_end.is_some() || !pairs.iter().any(|(key, _)| key == "end_date"));
        if url.origin() != native.api_root.origin()
            || url.path() != expected.path()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || !scope
            || !dates
        {
            return Err(invalid());
        }
        Ok(url)
    }
}

struct State {
    budget: super::PostBudget,
    metadata: Arc<SearchMetadata>,
    request: SearchOptions,
    next: Option<String>,
    pages: usize,
    seen: BTreeSet<String>,
    works: VecDeque<Value>,
    pending: VecDeque<ExtractionEvent>,
    last_date: Option<String>,
    anchor: Option<String>,
    done: bool,
}
fn direction(sort: &str) -> Option<(&'static str, i64)> {
    match sort {
        "date_desc" => Some(("end_date", 1)),
        "date_asc" => Some(("start_date", -1)),
        _ => None,
    }
}
fn crossed(sort: &str, anchor: &str, date: &str) -> bool {
    if sort == "date_desc" {
        anchor >= date
    } else {
        anchor <= date
    }
}
fn work_date(work: &Value) -> Option<&str> {
    work.get("create_date")
        .and_then(Value::as_str)
        .filter(|date| date.len() <= MAX_TEXT)
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

impl Pixiv {
    async fn search_page(&self, state: &mut State) -> Result<()> {
        if state.pages >= super::MAX_PAGES {
            state.done = true;
            return Ok(());
        }
        let Some(raw) = state.next.take() else {
            state.done = true;
            return Ok(());
        };
        // API-provided bounds are pinned before any locally owned rollover.
        let mut endpoint = state.request.checked(self, &raw)?;
        let offset = endpoint
            .query_pairs()
            .find(|(key, _)| key == "offset")
            .and_then(|(_, value)| value.parse::<i64>().ok())
            .unwrap_or(0);
        if offset >= 5000
            && let Some((key, days)) = direction(&state.request.sort)
            && let Some(last) = &state.last_date
        {
            let timestamp = crate::date::parse_date(last)
                .map_err(|_| error("Pixiv search contains an invalid rollover date"))?;
            let date = time::OffsetDateTime::from_unix_timestamp(timestamp)
                .ok()
                .and_then(|date| date.checked_add(time::Duration::days(days)))
                .ok_or_else(|| error("Pixiv search contains an invalid rollover date"))?;
            let date = format!(
                "{:04}-{:02}-{:02}",
                date.year(),
                u8::from(date.month()),
                date.day()
            );
            let pairs: Vec<_> = endpoint
                .query_pairs()
                .filter(|(name, _)| name != "offset" && name != key)
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect();
            endpoint.set_query(None);
            endpoint
                .query_pairs_mut()
                .extend_pairs(pairs)
                .append_pair(key, &date);
            if key == "start_date" {
                state.request.date_start = Some(date);
            } else {
                state.request.date_end = Some(date);
            }
            state.anchor = Some(last.clone());
        }
        let mut canonical = endpoint.clone();
        let mut pairs: Vec<_> = endpoint
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        pairs.sort();
        canonical.set_query(None);
        canonical.query_pairs_mut().extend_pairs(pairs);
        if !state.seen.insert(canonical.to_string()) {
            return Err(error("Pixiv search pagination repeated a page"));
        }
        let mut body = super::json(&self.get(&endpoint).await?)?;
        let works = body
            .get_mut("illusts")
            .and_then(Value::as_array_mut)
            .ok_or_else(|| error("Pixiv response contains no search illustration list"))?;
        state.last_date = works.last().and_then(work_date).map(str::to_owned);
        // Source checks the replay page's final raw create_date before scanning
        // its rows. Empty replay pages safely retain their anchor and paginate.
        let replay = if let Some(anchor) = &state.anchor {
            works.is_empty()
                || state
                    .last_date
                    .as_deref()
                    .is_some_and(|last| crossed(&state.request.sort, anchor, last))
        } else {
            true
        };
        state.works = if replay {
            std::mem::take(works).into()
        } else {
            VecDeque::new()
        };
        state.next = super::string(&body, "next_url").map(str::to_owned);
        state.pages += 1;
        Ok(())
    }

    pub(super) fn search_events(
        &self,
        options: SearchOptions,
    ) -> BoxStream<'_, Result<ExtractionEvent>> {
        if !options.valid() {
            return stream::once(async { Err(error("invalid Pixiv search options")) }).boxed();
        }
        if self.refresh_token.is_none() {
            return stream::once(async { Err(super::missing_token()) }).boxed();
        }
        let next = Some(options.first(self).to_string());
        let metadata = Arc::new(options.clone());
        let state = State {
            budget: super::PostBudget::new(self.max_posts),
            metadata,
            request: options,
            next,
            pages: 0,
            seen: BTreeSet::new(),
            works: VecDeque::new(),
            pending: VecDeque::new(),
            last_date: None,
            anchor: None,
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
                    if let Some(anchor) = &state.anchor {
                        let Some(date) = work_date(&work) else {
                            return Some((
                                Err(error("Pixiv search replay work contains no date")),
                                state,
                            ));
                        };
                        if crossed(&state.request.sort, anchor, date) {
                            state.anchor = None;
                        }
                        continue;
                    }
                    state.budget.consume();
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
                        Err(super::WorkFailure::Files(error)) => (Vec::new(), Some(error)),
                    };
                    if media.is_empty() {
                        let Some(id) = super::id_of(&work) else {
                            return Some((
                                Err(error("Pixiv search illustration contains an invalid ID")),
                                state,
                            ));
                        };
                        let mut metadata = self.empty_work_metadata(&work, &id, failure.is_none());
                        metadata.search = Some(state.metadata.clone());
                        metadata.search_archive = true;
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
                        let metadata = item.metadata.as_mut().expect("Pixiv illustration metadata");
                        metadata.search = Some(state.metadata.clone());
                        metadata.search_archive = true;
                    }
                    state.pending = self.finish(media);
                    continue;
                }
                if state.done {
                    return None;
                }
                if let Err(error) = self.search_page(&mut state).await {
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

    fn work(id: u64, kind: &str, date: &str) -> Value {
        json!({"id":id,"type":kind,"user":{"id":5,"account":"original-artist"},"title":"Title","create_date":date,
        "meta_single_page":{"original_image_url":format!("https://i.pximg.net/img-original/{id}_p0.jpg")},"meta_pages":[]})
    }
    fn page(works: Vec<Value>, next: Option<Url>) -> Vec<u8> {
        response(
            "200 OK",
            json!({"illusts":works,"next_url":next})
                .to_string()
                .as_bytes(),
        )
    }
    fn url() -> Url {
        Url::parse("https://www.pixiv.net/tags/cat").unwrap()
    }
    const DATE: &str = "2024-01-10T00:00:00+09:00";

    #[test]
    fn path_query_precedence_lossy_unicode_bare_keys_and_empty_word_match_source() {
        for (raw, word) in [
            (
                "https://www.pixiv.net/tags/%E7%8C%AB/novels?q=ignored",
                "猫",
            ),
            ("https://www.pixiv.net/en/tags/C++?word=ignored", "C++"),
            ("https://www.pixiv.net/tags/%FF", "�"),
            ("https://www.pixiv.net/tags/%GG", "%GG"),
            (
                "https://www.pixiv.net/search?q&word=fallback&q=chosen&q=later",
                "chosen",
            ),
            (
                "https://www.pixiv.net/search.php?q=&word=fallback",
                "fallback",
            ),
            ("https://www.pixiv.net/search.php?word=", ""),
            ("https://www.pixiv.net/search.php?q=&word=", ""),
            (
                "https://www.pixiv.net/search.php?q=C%2B%2B+%26+%E7%8C%AB",
                "C++ & 猫",
            ),
            ("https://www.pixiv.net/tags/%00", "\0"),
        ] {
            let options = SearchOptions::parse(&Url::parse(raw).unwrap()).unwrap();
            assert_eq!(options.word, word, "{raw}");
            assert_eq!(options.sort, "date_desc");
            assert_eq!(options.target, "exact_match_for_tags");
        }
        for (raw, error) in [
            ("https://www.pixiv.net/search", "term"),
            ("https://www.pixiv.net/search?q=", "term"),
            ("https://www.pixiv.net/en/search?q=x", "unsupported"),
            ("https://www.pixiv.net/tags/cat?order=DATE", "order"),
            ("https://www.pixiv.net/tags/cat?s_mode=TAG", "mode"),
            ("https://www.pixiv.net/tags/cat?order=", "order"),
        ] {
            assert!(
                SearchOptions::parse(&Url::parse(raw).unwrap())
                    .unwrap_err()
                    .to_string()
                    .contains(error)
            );
        }
        assert!(matches!(
            super::super::target(&Url::parse("https://www.pixiv.net/tags/cat/novels").unwrap())
                .unwrap(),
            super::super::Target::Search(_)
        ));
    }

    #[test]
    fn exact_sort_and_target_aliases_preserve_raw_date_options_and_metadata_bounds() {
        for (alias, canonical) in SORTS {
            let options = SearchOptions::parse(
                &Url::parse(&format!("https://www.pixiv.net/tags/cat?order={alias}")).unwrap(),
            )
            .unwrap();
            assert_eq!(&options.sort, canonical);
        }
        for (alias, canonical) in TARGETS {
            let options = SearchOptions::parse(
                &Url::parse(&format!("https://www.pixiv.net/tags/cat?s_mode={alias}")).unwrap(),
            )
            .unwrap();
            assert_eq!(&options.target, canonical);
        }
        let options = SearchOptions::parse(
            &Url::parse("https://www.pixiv.net/tags/cat?scd=&ecd=arbitrary+date&order&order=date")
                .unwrap(),
        )
        .unwrap();
        assert_eq!(options.date_start.as_deref(), Some(""));
        assert_eq!(options.date_end.as_deref(), Some("arbitrary date"));
        assert_eq!(options.sort, "date_asc");
        let data = serde_json::to_value(&options).unwrap();
        assert_eq!(SearchMetadata::parse_compat(&data).unwrap(), options);
        let mut empty = data.clone();
        empty["word"] = json!("");
        assert!(SearchMetadata::parse_compat(&empty).is_some());
        for (key, value) in [
            ("word", json!("x".repeat(MAX_TEXT + 1))),
            ("sort", json!("date_d")),
            ("target", json!("s_tag")),
            ("date_start", json!(false)),
            ("date_end", json!("x".repeat(MAX_TEXT + 1))),
        ] {
            let mut invalid = data.clone();
            invalid[key] = value;
            assert!(SearchMetadata::parse_compat(&invalid).is_none(), "{key}");
        }
        let term = "x".repeat(MAX_TEXT);
        let raw = Url::parse(&format!("https://www.pixiv.net/search?word={term}")).unwrap();
        assert!(SearchOptions::parse(&raw).is_ok());
    }

    #[tokio::test]
    async fn term_encoding_raw_dates_empty_pages_and_metadata_are_source_compatible() {
        let raw = Url::parse(
            "https://www.pixiv.net/search.php?q=%E7%8C%AB+%26+C%2B%2B&scd=&ecd=raw+date",
        )
        .unwrap();
        let (root, server) = serve_with(|root| {
            vec![token("search-access"),page(Vec::new(),Some(root.join("v1/search/illust?word=%E7%8C%AB+%26+C%2B%2B&search_target=exact_match_for_tags&sort=date_desc&start_date=&end_date=raw+date&offset=30").unwrap())),page(vec![work(41,"illust",DATE)],None)]
        });
        let events = pixiv(root)
            .events(&raw)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        let ExtractionEvent::Post(post) = &events[0] else {
            panic!("missing search post")
        };
        let metadata = post.metadata.as_ref().unwrap();
        assert!(metadata.search_archive);
        assert_eq!(metadata.search.as_ref().unwrap().word, "猫 & C++");
        let ExtractionEvent::Media(media) = &events[1] else {
            panic!("missing search media")
        };
        let metadata = media.metadata.as_ref().unwrap();
        assert_eq!(metadata.username.as_deref(), Some("original-artist"));
        assert_eq!(
            metadata.search.as_ref().unwrap().date_start.as_deref(),
            Some("")
        );
        assert_eq!(
            metadata.search.as_ref().unwrap().date_end.as_deref(),
            Some("raw date")
        );
        assert_eq!(
            media.headers,
            std::collections::BTreeMap::from([("Referer".into(), super::super::REFERER.into())])
        );
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[1].starts_with("GET /v1/search/illust?word=%E7%8C%AB+%26+C%2B%2B&search_target=exact_match_for_tags&sort=date_desc&start_date=&end_date=raw+date "));
        assert!(
            requests[1]
                .to_ascii_lowercase()
                .contains("authorization: bearer search-access")
        );
    }

    #[tokio::test]
    async fn search_archive_namespace_depends_on_word_not_sort_target_dates_or_rank() {
        let (root, server) = serve(vec![token("t"), page(vec![work(41, "illust", DATE)], None)]);
        let media = pixiv(root)
            .stream(&url())
            .collect::<Vec<_>>()
            .await
            .remove(0)
            .unwrap();
        assert!(media.archive_key().is_some());
        let ordinary = super::super::parse_illust(&work(41, "illust", DATE))
            .unwrap()
            .remove(0);
        assert_ne!(ordinary.archive_key(), media.archive_key());
        let mut changed = media.clone();
        let mut context = (**media.metadata.as_ref().unwrap().search.as_ref().unwrap()).clone();
        context.sort = "date_asc".into();
        context.target = "title_and_caption".into();
        context.date_end = Some("other bound".into());
        changed.metadata.as_mut().unwrap().search = Some(Arc::new(context.clone()));
        assert_eq!(changed.archive_key(), media.archive_key());
        context.word = "other term".into();
        changed.metadata.as_mut().unwrap().search = Some(Arc::new(context));
        assert_ne!(changed.archive_key(), media.archive_key());
        assert_eq!(server.join().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn empty_and_failed_animations_keep_post_selection_positions() {
        for failed in [false, true] {
            for range in ["1", "2"] {
                let mut replies = vec![
                    token("t"),
                    page(
                        vec![work(41, "ugoira", DATE), work(42, "illust", DATE)],
                        None,
                    ),
                ];
                if failed {
                    replies.push(response("503 Service Unavailable", b"PRIVATE"));
                }
                let (root, server) = serve(replies);
                let native = pixiv(root).with_ugoira(failed);
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
                let files = results
                    .iter()
                    .filter_map(|result| result.as_ref().ok())
                    .collect::<Vec<_>>();
                assert_eq!(files.len(), usize::from(range == "2"));
                assert_eq!(
                    results.iter().filter(|result| result.is_err()).count(),
                    usize::from(failed)
                );
                if let Some(file) = files.first() {
                    assert_eq!(file.filename, "42_p0.jpg");
                    assert_eq!(
                        file.metadata
                            .as_ref()
                            .unwrap()
                            .search
                            .as_ref()
                            .unwrap()
                            .word,
                        "cat"
                    );
                }
                assert_eq!(server.join().unwrap().len(), if failed { 3 } else { 2 });
            }
        }
    }

    #[tokio::test]
    async fn dropped_or_finite_file_range_does_not_fetch_later_animation_or_page() {
        for range in [false, true] {
            let (root, server) = serve_with(|root| {
                vec![token("t"),page(vec![work(41,"illust",DATE),work(42,"ugoira",DATE)],Some(root.join("v1/search/illust?word=cat&search_target=exact_match_for_tags&sort=date_desc&offset=30").unwrap()))]
            });
            let native = pixiv(root);
            if range {
                let files = JobSelection {
                    file_range: Some(RangeSelection::parse("1").unwrap()),
                    ..Default::default()
                }
                .apply(native.events(&url()))
                .collect::<Vec<_>>()
                .await;
                assert_eq!(files.len(), 1);
                assert!(files[0].is_ok());
            } else {
                let mut files = native.stream(&url());
                assert!(files.next().await.unwrap().is_ok());
                drop(files);
            }
            assert_eq!(server.join().unwrap().len(), 2);
        }
    }

    #[test]
    fn next_url_pins_term_sort_target_optional_bounds_origin_path_and_unique_scope() {
        let native = Pixiv::new(std::time::Duration::from_secs(1)).unwrap();
        let options = SearchOptions::parse(&url()).unwrap();
        for raw in [
            "https://evil.example/v1/search/illust?word=cat&search_target=exact_match_for_tags&sort=date_desc",
            "https://app-api.pixiv.net/v1/illust/ranking?word=cat&search_target=exact_match_for_tags&sort=date_desc",
            "https://u:p@app-api.pixiv.net/v1/search/illust?word=cat&search_target=exact_match_for_tags&sort=date_desc",
            "https://app-api.pixiv.net/v1/search/illust?word=other&search_target=exact_match_for_tags&sort=date_desc",
            "https://app-api.pixiv.net/v1/search/illust?word=cat&search_target=title_and_caption&sort=date_desc",
            "https://app-api.pixiv.net/v1/search/illust?word=cat&search_target=exact_match_for_tags&sort=date_asc",
            "https://app-api.pixiv.net/v1/search/illust?word=cat&word=cat&search_target=exact_match_for_tags&sort=date_desc",
            "https://app-api.pixiv.net/v1/search/illust?word=cat&search_target=exact_match_for_tags&sort=date_desc&end_date=added",
        ] {
            assert!(options.checked(&native, raw).is_err(), "{raw}");
        }
    }

    #[tokio::test]
    async fn canonical_cycle_and_total_page_bound_stop_before_duplicate_or_extra_requests() {
        let (root, server) = serve_with(|root| {
            vec![token("t"),page(Vec::new(),Some(root.join("v1/search/illust?sort=date_desc&search_target=exact_match_for_tags&word=cat").unwrap()))]
        });
        let results = pixiv(root).stream(&url()).collect::<Vec<_>>().await;
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].as_ref().unwrap_err().to_string(),
            "Pixiv search pagination repeated a page"
        );
        assert_eq!(server.join().unwrap().len(), 2);
        let (root, server) = serve_with(|root| {
            let mut replies = vec![token("t")];
            replies.extend((1..=super::super::MAX_PAGES).map(|offset|page(Vec::new(),Some(root.join(&format!("v1/search/illust?word=cat&search_target=exact_match_for_tags&sort=date_desc&offset={offset}")).unwrap()))));
            replies
        });
        let results = pixiv(root).stream(&url()).collect::<Vec<_>>().await;
        assert!(results.is_empty(), "{results:?}");
        assert_eq!(server.join().unwrap().len(), super::super::MAX_PAGES + 1);
    }

    #[tokio::test]
    async fn recorded_upstream_pagination_oracle_matches_http_calls_and_selected_ids() {
        let oracle: Value = serde_json::from_slice(include_bytes!(
            "../tests/fixtures/pixiv/search-pagination.json"
        ))
        .unwrap();
        assert_eq!(
            oracle["source"],
            "gallery_dl.extractor.pixiv.PixivAppAPI._pagination_search"
        );
        for case in oracle["cases"].as_array().unwrap() {
            let options = SearchOptions {
                word: case["initial"]["word"].as_str().unwrap().to_owned(),
                sort: case["initial"]["sort"].as_str().unwrap().to_owned(),
                target: case["initial"]["search_target"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
                date_start: case["initial"]["start_date"].as_str().map(str::to_owned),
                date_end: case["initial"]["end_date"].as_str().map(str::to_owned),
            };
            let (root, server) = serve_with(|root| {
                let mut replies = vec![token("t")];
                for source in case["responses"].as_array().unwrap() {
                    let works = source["illusts"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|source| {
                            work(
                                source["id"].as_u64().unwrap(),
                                "illust",
                                source["create_date"].as_str().unwrap(),
                            )
                        })
                        .collect();
                    let next = source["next_url"].as_str().map(|raw| {
                        let source = Url::parse(raw).unwrap();
                        let mut local = root.join(source.path().trim_start_matches('/')).unwrap();
                        local.set_query(source.query());
                        local
                    });
                    replies.push(page(works, next));
                }
                replies
            });
            let native = pixiv(root);
            let files = crate::event::media_only(native.search_events(options.clone()))
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .collect::<Result<Vec<_>>>()
                .unwrap();
            let actual = files
                .iter()
                .map(|file| {
                    file.metadata
                        .as_ref()
                        .unwrap()
                        .post_id
                        .parse::<u64>()
                        .unwrap()
                })
                .collect::<Vec<_>>();
            let expected = case["selected_ids"]
                .as_array()
                .unwrap()
                .iter()
                .map(|id| id.as_u64().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(actual, expected, "{}", options.sort);
            for file in &files {
                assert_eq!(
                    file.metadata.as_ref().unwrap().search.as_deref(),
                    Some(&options)
                );
            }
            let requests = server.join().unwrap();
            let calls = case["calls"].as_array().unwrap();
            assert_eq!(requests.len(), calls.len() + 1);
            for (request, call) in requests[1..].iter().zip(calls) {
                let path = request.split_whitespace().nth(1).unwrap();
                let actual = Url::parse(&format!("http://localhost{path}")).unwrap();
                assert_eq!(actual.path(), call["endpoint"].as_str().unwrap());
                let actual: std::collections::BTreeMap<_, _> = actual
                    .query_pairs()
                    .map(|(key, value)| (key.into_owned(), value.into_owned()))
                    .collect();
                let expected: std::collections::BTreeMap<_, _> = call["params"]
                    .as_object()
                    .unwrap()
                    .iter()
                    .filter_map(|(key, value)| {
                        value.as_str().map(|value| (key.clone(), value.to_owned()))
                    })
                    .collect();
                assert_eq!(actual, expected, "{}", options.sort);
            }
        }
    }

    #[tokio::test]
    async fn descending_rollover_uses_utc_day_and_suppresses_replay_crossing_row() {
        // Last first-page work is Jan10 local, Jan9 UTC: source end_date becomes Jan10.
        let (root, server) = serve_with(|root| {
            vec![token("t"),page(vec![work(41,"illust","2024-01-10T00:30:00+09:00")],Some(root.join("v1/search/illust?word=cat&search_target=exact_match_for_tags&sort=date_desc&offset=5000").unwrap())),
            page(vec![work(40,"illust","2024-01-10T01:00:00+09:00"),work(41,"illust","2024-01-10T00:30:00+09:00"),work(42,"illust","2024-01-09T23:59:00+09:00")],None)]
        });
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
            ["41_p0.jpg", "42_p0.jpg"]
        );
        for file in &files {
            assert_eq!(
                file.metadata
                    .as_ref()
                    .unwrap()
                    .search
                    .as_ref()
                    .unwrap()
                    .date_end,
                None
            );
        }
        let requests = server.join().unwrap();
        let query = Url::parse(&format!(
            "http://localhost{}",
            requests[2].split_whitespace().nth(1).unwrap()
        ))
        .unwrap();
        assert!(
            query
                .query_pairs()
                .any(|(key, value)| key == "end_date" && value == "2024-01-10")
        );
        assert!(!query.query_pairs().any(|(key, _)| key == "offset"));
    }

    #[tokio::test]
    async fn ascending_rollover_pins_original_opposite_bound_and_survives_empty_replay_pages() {
        let raw = Url::parse("https://www.pixiv.net/tags/cat?order=date&ecd=original-end").unwrap();
        let (root, server) = serve_with(|root| {
            vec![token("t"),page(vec![work(41,"illust","2024-01-10T00:30:00+09:00")],Some(root.join("v1/search/illust?word=cat&search_target=exact_match_for_tags&sort=date_asc&end_date=original-end&offset=5000").unwrap())),
            page(Vec::new(),Some(root.join("v1/search/illust?word=cat&search_target=exact_match_for_tags&sort=date_asc&end_date=original-end&start_date=2024-01-08&offset=30").unwrap())),
            page(vec![work(40,"illust","2024-01-09T23:00:00+09:00"),work(42,"illust","2024-01-10T01:00:00+09:00"),work(43,"illust","2024-01-10T02:00:00+09:00")],None)]
        });
        let files = pixiv(root)
            .stream(&raw)
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
            ["41_p0.jpg", "43_p0.jpg"]
        );
        for file in &files {
            let metadata = file.metadata.as_ref().unwrap().search.as_ref().unwrap();
            assert_eq!(metadata.date_start, None);
            assert_eq!(metadata.date_end.as_deref(), Some("original-end"));
        }
        let requests = server.join().unwrap();
        assert!(requests[2].contains("start_date=2024-01-08"));
        assert!(!requests[2].contains("offset="));
        assert!(requests[3].contains("offset=30"));
    }

    #[tokio::test]
    async fn popular_sorts_do_not_roll_dates_at_offset_five_thousand() {
        let raw = Url::parse("https://www.pixiv.net/tags/cat?order=popular_d").unwrap();
        let (root, server) = serve_with(|root| {
            vec![token("t"),page(vec![work(41,"illust",DATE)],Some(root.join("v1/search/illust?word=cat&search_target=exact_match_for_tags&sort=popular_desc&offset=5000").unwrap())),page(vec![work(42,"illust",DATE)],None)]
        });
        let files = pixiv(root).stream(&raw).collect::<Vec<_>>().await;
        assert_eq!(files.len(), 2);
        let requests = server.join().unwrap();
        assert!(requests[2].contains("offset=5000"));
        assert!(!requests[2].contains("end_date="));
    }

    #[tokio::test]
    async fn hostile_next_bound_is_rejected_before_rollover_or_followup_auth_send() {
        let (root, server) = serve_with(|root| {
            vec![token("t"),page(vec![work(41,"illust",DATE)],Some(root.join("v1/search/illust?word=cat&search_target=exact_match_for_tags&sort=date_desc&end_date=hostile&offset=5000").unwrap()))]
        });
        let results = pixiv(root).stream(&url()).collect::<Vec<_>>().await;
        assert_eq!(results.len(), 2);
        assert!(results[0].is_ok());
        assert_eq!(
            results[1].as_ref().unwrap_err().to_string(),
            "Pixiv returned an unexpected search next page"
        );
        assert_eq!(server.join().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn search_page_http_errors_are_private_and_missing_users_do_not_create_posts() {
        let mut missing = work(40, "illust", DATE);
        missing["user"]["id"] = json!(0);
        let (root, server) = serve_with(|root| {
            vec![token("t"),page(vec![missing,work(41,"illust",DATE)],Some(root.join("v1/search/illust?word=cat&search_target=exact_match_for_tags&sort=date_desc&offset=30").unwrap())),response("403 Forbidden",b"PRIVATE")]
        });
        let results = pixiv(root).stream(&url()).collect::<Vec<_>>().await;
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].as_ref().unwrap().filename, "41_p0.jpg");
        assert!(results[1].is_err());
        assert!(
            !results[1]
                .as_ref()
                .unwrap_err()
                .to_string()
                .contains("PRIVATE")
        );
        assert_eq!(server.join().unwrap().len(), 3);
    }
    #[tokio::test]
    async fn max_posts_counts_after_replay_suppression_and_stops_before_future_ugoira() {
        let (root, server) = serve_with(|root| {
            vec![token("t"),page(vec![work(41,"illust","2024-01-10T00:30:00+09:00")],Some(root.join("v1/search/illust?word=cat&search_target=exact_match_for_tags&sort=date_desc&offset=5000").unwrap())),page(vec![work(40,"illust","2024-01-10T01:00:00+09:00"),work(41,"illust","2024-01-10T00:30:00+09:00"),work(42,"illust","2024-01-09T23:59:00+09:00"),work(43,"ugoira",DATE)],Some(Url::parse("https://unexpected.invalid/page").unwrap()))]
        });
        let files = pixiv(root)
            .with_max_posts(Some(2))
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
            ["41_p0.jpg", "42_p0.jpg"]
        );
        assert_eq!(server.join().unwrap().len(), 3);
    }
}
