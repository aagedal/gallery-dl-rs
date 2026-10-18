//! Post predicates use explicit extractor events, never file ID transitions.
use crate::{
    Result, error,
    event::{ExtractionEvent, Post},
    extract::Media,
    filter::FilterExpression,
    selection::RangeSelection,
};
use futures_util::{StreamExt, stream::BoxStream};

#[derive(Clone, Debug)]
pub struct PostSelection {
    range: Option<RangeSelection>,
    filter: Option<FilterExpression>,
}
impl PostSelection {
    pub fn new(range: Option<RangeSelection>, filter: Option<FilterExpression>) -> Self {
        Self { range, filter }
    }
    /// Post dates precede the filter and range. File predicates run downstream.
    pub fn apply<'a>(
        &self,
        input: BoxStream<'a, Result<ExtractionEvent>>,
        before: Option<i64>,
        after: Option<i64>,
        unordered: bool,
    ) -> BoxStream<'a, Result<Media>> {
        struct State<'a> {
            input: Option<BoxStream<'a, Result<ExtractionEvent>>>,
            selection: PostSelection,
            index: u64,
            active: bool,
            in_post: bool,
            last: bool,
            failed: bool,
        }
        let input = if self
            .range
            .as_ref()
            .is_some_and(|range| range.last_index().is_none())
        {
            None
        } else {
            Some(input)
        };
        futures_util::stream::unfold(
            State {
                input,
                selection: self.clone(),
                index: 0,
                active: false,
                in_post: false,
                last: false,
                failed: false,
            },
            move |mut state| async move {
                let mut skipped = 0u8;
                loop {
                    let event = match state.input.as_mut()?.next().await {
                        Some(event) => event,
                        None => {
                            if state.in_post && !state.failed {
                                state.input = None;
                                return Some((
                                    Err(error("post events ended before the post completed")),
                                    state,
                                ));
                            }
                            return None;
                        }
                    };
                    match event {
                        Err(error) => {
                            state.failed = true;
                            return Some((Err(error), state));
                        }
                        Ok(ExtractionEvent::Post(post)) => {
                            if state.in_post {
                                state.input = None;
                                return Some((
                                    Err(error("post events contain an incomplete boundary")),
                                    state,
                                ));
                            }
                            state.in_post = true;
                            state.failed = false;
                            state.active = false;
                            state.last = false;
                            match date(&post, before, after, unordered) {
                                DateDecision::Stop => {
                                    return None;
                                }
                                DateDecision::Skip => {}
                                DateDecision::Keep => {
                                    if state.selection.filter.as_ref().is_none_or(|filter| {
                                        filter.matches_post(&post).unwrap_or(false)
                                    }) {
                                        let Some(index) = state.index.checked_add(1) else {
                                            state.input = None;
                                            return Some((
                                                Err(error("post selection counter overflow")),
                                                state,
                                            ));
                                        };
                                        state.index = index;
                                        state.active = state
                                            .selection
                                            .range
                                            .as_ref()
                                            .is_none_or(|range| range.contains(index));
                                        state.last =
                                            state.selection.range.as_ref().is_some_and(|range| {
                                                range.last_index() == Some(index)
                                            });
                                    }
                                }
                            }
                        }
                        Ok(ExtractionEvent::Media(media)) => {
                            if !state.in_post {
                                state.input = None;
                                return Some((
                                    Err(error("post selection requires explicit post boundaries")),
                                    state,
                                ));
                            }
                            if state.active {
                                return Some((Ok(*media), state));
                            }
                        }
                        Ok(ExtractionEvent::EndPost) => {
                            if !state.in_post {
                                state.input = None;
                                return Some((
                                    Err(error("post events contain an unexpected end")),
                                    state,
                                ));
                            }
                            state.in_post = false;
                            if state.last {
                                return None;
                            }
                        }
                    }
                    skipped = skipped.wrapping_add(1);
                    if skipped == 0 {
                        tokio::task::yield_now().await;
                    }
                }
            },
        )
        .boxed()
    }
}
enum DateDecision {
    Keep,
    Skip,
    Stop,
}
fn date(post: &Post, before: Option<i64>, after: Option<i64>, unordered: bool) -> DateDecision {
    let Some(timestamp) = post
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.timestamp)
    else {
        return DateDecision::Keep;
    };
    if after.is_some_and(|after| timestamp <= after) {
        return if unordered {
            DateDecision::Skip
        } else {
            DateDecision::Stop
        };
    }
    if before.is_some_and(|before| timestamp >= before) {
        DateDecision::Skip
    } else {
        DateDecision::Keep
    }
}
