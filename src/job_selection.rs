//! Per-job predicates for nested extractor streams with explicit child ownership.
use crate::{
    Result, error,
    event::{ChildControl, ExtractionEvent},
    extract::Media,
    filter::FilterExpression,
    post_selection::PostSelection,
    selection::RangeSelection,
};
use futures_util::{StreamExt, stream::BoxStream};
use std::collections::{HashSet, VecDeque};

const MAX_FRAMES: usize = 128;

#[derive(Clone, Debug, Default)]
pub struct JobSelection {
    pub post_selection: Option<PostSelection>,
    pub file_range: Option<RangeSelection>,
    pub file_filter: Option<FilterExpression>,
    pub child_range: Option<RangeSelection>,
    pub child_filter: Option<FilterExpression>,
    pub before: Option<i64>,
    pub after: Option<i64>,
    pub unordered: bool,
}

#[derive(Default)]
struct Frame {
    control: Option<ChildControl>,
    posts: u64,
    files: u64,
    children: u64,
    in_post: bool,
    active: bool,
    last_post: bool,
    last_for_parent: bool,
    stopped: bool,
    failed: bool,
    shown_post: bool,
    shown_child: bool,
}
struct State<'a> {
    input: Option<BoxStream<'a, Result<ExtractionEvent>>>,
    selection: JobSelection,
    frames: Vec<Frame>,
    pending: VecDeque<ExtractionEvent>,
    visited: HashSet<String>,
}
impl State<'_> {
    fn sync_cancelled(&mut self) {
        let mut stopped = false;
        for frame in &mut self.frames {
            stopped |= frame.stopped
                || frame
                    .control
                    .as_ref()
                    .is_some_and(ChildControl::is_cancelled);
            if stopped {
                frame.stopped = true;
                if let Some(control) = &frame.control {
                    control.cancel();
                }
            }
        }
    }
    fn stop(&mut self) {
        if self.frames.len() == 1 {
            self.input = None;
        } else {
            let frame = self.frames.last_mut().expect("current job");
            frame.stopped = true;
            frame.control.as_ref().expect("child control").cancel();
        }
    }
    fn fail(&mut self, message: &str) -> crate::Error {
        for frame in self.frames.iter().rev() {
            if let Some(control) = &frame.control {
                control.cancel();
            }
            if frame.shown_post {
                self.pending.push_back(ExtractionEvent::EndPost);
            }
            if frame.shown_child
                && let Some(control) = &frame.control
            {
                self.pending
                    .push_back(ExtractionEvent::EndChild(control.clone()));
            }
        }
        self.frames = vec![Frame::default()];
        self.input = None;
        error(message)
    }
}
impl Drop for State<'_> {
    fn drop(&mut self) {
        for frame in &self.frames {
            if let Some(control) = &frame.control {
                control.cancel();
            }
        }
    }
}

#[derive(PartialEq)]
enum DateDecision {
    Keep,
    Skip,
    Stop,
}
fn date(timestamp: Option<i64>, selection: &JobSelection) -> DateDecision {
    let Some(timestamp) = timestamp else {
        return DateDecision::Keep;
    };
    if selection.after.is_some_and(|after| timestamp <= after) {
        return if selection.unordered {
            DateDecision::Skip
        } else {
            DateDecision::Stop
        };
    }
    if selection.before.is_some_and(|before| timestamp >= before) {
        DateDecision::Skip
    } else {
        DateDecision::Keep
    }
}

impl JobSelection {
    /// Each child job inherits predicates, with independent counters and framing.
    /// A canceled child is drained only through its matching EndChild event;
    /// producers must check its control before polling further extraction work.
    pub fn apply<'a>(
        &self,
        input: BoxStream<'a, Result<ExtractionEvent>>,
    ) -> BoxStream<'a, Result<Media>> {
        crate::event::media_only(self.apply_events(input))
    }
    /// Retain selected job boundaries for consumers that run postprocessor hooks.
    /// Finite file cuts close visible posts without polling their unused tails.
    pub fn apply_events<'a>(
        &self,
        input: BoxStream<'a, Result<ExtractionEvent>>,
    ) -> BoxStream<'a, Result<ExtractionEvent>> {
        let empty = self
            .file_range
            .as_ref()
            .is_some_and(|range| range.last_index().is_none())
            || self
                .post_selection
                .as_ref()
                .and_then(|selection| selection.range.as_ref())
                .is_some_and(|range| range.last_index().is_none());
        let input = if empty {
            drop(input);
            None
        } else {
            Some(input)
        };
        let state = State {
            input,
            selection: self.clone(),
            frames: vec![Frame::default()],
            pending: VecDeque::new(),
            visited: HashSet::new(),
        };
        futures_util::stream::unfold(state, |mut state| async move {
            let mut skipped = 0u8;
            loop {
                if let Some(event) = state.pending.pop_front() {
                    return Some((Ok(event), state));
                }
                state.sync_cancelled();
                if state.input.is_none() {
                    if state.frames[0].shown_post {
                        state.frames[0].shown_post = false;
                        state.frames[0].in_post = false;
                        return Some((Ok(ExtractionEvent::EndPost), state));
                    }
                    return None;
                }
                let next = state.input.as_mut().expect("active input").next().await;
                let Some(event) = next else {
                    if state.frames.len() != 1 {
                        let error = state.fail("child events ended before the child completed");
                        return Some((Err(error), state));
                    }
                    let frame = &state.frames[0];
                    if frame.in_post && !frame.failed {
                        let error = state.fail("post events ended before the post completed");
                        return Some((Err(error), state));
                    }
                    if state.frames[0].shown_post {
                        state.frames[0].shown_post = false;
                        state.frames[0].in_post = false;
                        state.input = None;
                        return Some((Ok(ExtractionEvent::EndPost), state));
                    }
                    return None;
                };
                // Cancellation may also arrive while the producer was pending.
                state.sync_cancelled();
                match event {
                    Err(error) => {
                        state.frames.last_mut().expect("current job").failed = true;
                        return Some((Err(error), state));
                    }
                    Ok(ExtractionEvent::BeginChild(scope)) => {
                        if state.frames.len() >= MAX_FRAMES
                            || state.frames.iter().any(|frame| {
                                frame
                                    .control
                                    .as_ref()
                                    .is_some_and(|control| control.same_scope(&scope.control))
                            })
                        {
                            scope.control.cancel();
                            let error = state.fail("child events contain invalid nesting");
                            return Some((Err(error), state));
                        }
                        let parent = state.frames.last().expect("parent job");
                        let rejected = parent.stopped
                            || parent.in_post && !parent.active
                            || scope.control.is_cancelled()
                            || state.selection.child_filter.as_ref().is_some_and(|filter| {
                                !filter.matches_child(&scope.metadata).unwrap_or(false)
                            });
                        let (mut selected, last_for_parent) = if rejected {
                            (false, false)
                        } else {
                            let parent = state.frames.last_mut().expect("parent job");
                            let Some(index) = parent.children.checked_add(1) else {
                                scope.control.cancel();
                                let error = state.fail("child selection counter overflow");
                                return Some((Err(error), state));
                            };
                            parent.children = index;
                            (
                                state
                                    .selection
                                    .child_range
                                    .as_ref()
                                    .is_none_or(|range| range.contains(index)),
                                state.selection.child_range.as_ref().is_some_and(|range| {
                                    range.last_index().is_none_or(|last| index >= last)
                                }),
                            )
                        };
                        if selected && let Some(url) = scope.metadata.booru_artist_url() {
                            selected = state.visited.insert(url.to_owned());
                        }
                        if !selected {
                            scope.control.cancel();
                        }
                        state.frames.push(Frame {
                            control: Some(scope.control.clone()),
                            shown_child: selected,
                            stopped: !selected,
                            last_for_parent,
                            ..Frame::default()
                        });
                        if selected {
                            return Some((Ok(ExtractionEvent::BeginChild(scope)), state));
                        }
                    }
                    Ok(ExtractionEvent::EndChild(control)) => {
                        let frame = state.frames.last().expect("current job");
                        if state.frames.len() == 1
                            || !frame
                                .control
                                .as_ref()
                                .is_some_and(|current| current.same_scope(&control))
                            || frame.in_post && !frame.stopped && !frame.failed
                        {
                            let error = state
                                .fail("child events contain an incomplete or unexpected boundary");
                            return Some((Err(error), state));
                        }
                        let frame = state.frames.pop().expect("completed child");
                        if frame.last_for_parent {
                            state.stop();
                        }
                        if frame.shown_child {
                            if frame.shown_post {
                                state.pending.push_back(ExtractionEvent::EndChild(control));
                                return Some((Ok(ExtractionEvent::EndPost), state));
                            }
                            return Some((Ok(ExtractionEvent::EndChild(control)), state));
                        }
                    }
                    Ok(ExtractionEvent::Post(post)) => {
                        let frame = state.frames.last().expect("current job");
                        if frame.in_post {
                            let error = state.fail("post events contain an incomplete boundary");
                            return Some((Err(error), state));
                        }
                        let stopped = frame.stopped;
                        let frame = state.frames.last_mut().expect("current job");
                        frame.in_post = true;
                        frame.failed = false;
                        frame.active = false;
                        frame.last_post = false;
                        if !stopped {
                            if let Some(selection) = &state.selection.post_selection {
                                match date(
                                    post.metadata.as_ref().and_then(|post| post.timestamp),
                                    &state.selection,
                                ) {
                                    DateDecision::Stop => state.stop(),
                                    DateDecision::Skip => {}
                                    DateDecision::Keep => {
                                        if selection.filter.as_ref().is_none_or(|filter| {
                                            filter.matches_post(&post).unwrap_or(false)
                                        }) {
                                            let frame =
                                                state.frames.last_mut().expect("current job");
                                            let Some(index) = frame.posts.checked_add(1) else {
                                                let error =
                                                    state.fail("post selection counter overflow");
                                                return Some((Err(error), state));
                                            };
                                            frame.posts = index;
                                            frame.active = selection
                                                .range
                                                .as_ref()
                                                .is_none_or(|range| range.contains(index));
                                            frame.last_post =
                                                selection.range.as_ref().is_some_and(|range| {
                                                    range.last_index() == Some(index)
                                                });
                                        }
                                    }
                                }
                            } else {
                                frame.active = true;
                            }
                        }
                        let frame = state.frames.last_mut().expect("current job");
                        if frame.active && !frame.stopped {
                            frame.shown_post = true;
                            return Some((Ok(ExtractionEvent::Post(post)), state));
                        }
                    }
                    Ok(ExtractionEvent::EndPost) => {
                        let frame = state.frames.last_mut().expect("current job");
                        if !frame.in_post {
                            let error = state.fail("post events contain an unexpected end");
                            return Some((Err(error), state));
                        }
                        frame.in_post = false;
                        let shown = std::mem::take(&mut frame.shown_post);
                        if frame.last_post && !frame.stopped {
                            state.stop();
                        }
                        if shown {
                            return Some((Ok(ExtractionEvent::EndPost), state));
                        }
                    }
                    Ok(ExtractionEvent::Media(media)) => {
                        let frame = state.frames.last().expect("current job");
                        if state.selection.post_selection.is_some()
                            && !frame.in_post
                            && !frame.stopped
                        {
                            let error =
                                state.fail("post selection requires explicit post boundaries");
                            return Some((Err(error), state));
                        }
                        if !frame.stopped && (!frame.in_post || frame.active) {
                            let date = if state.selection.post_selection.is_none() {
                                date(
                                    media.metadata.as_ref().and_then(|post| post.timestamp),
                                    &state.selection,
                                )
                            } else {
                                DateDecision::Keep
                            };
                            if date == DateDecision::Stop {
                                state.stop();
                            } else if date == DateDecision::Keep
                                && state
                                    .selection
                                    .file_filter
                                    .as_ref()
                                    .is_none_or(|filter| filter.matches(&media).unwrap_or(false))
                            {
                                let frame = state.frames.last_mut().expect("current job");
                                let Some(index) = frame.files.checked_add(1) else {
                                    let error = state.fail("file selection counter overflow");
                                    return Some((Err(error), state));
                                };
                                frame.files = index;
                                let selected = state
                                    .selection
                                    .file_range
                                    .as_ref()
                                    .is_none_or(|range| range.contains(index));
                                let last = state
                                    .selection
                                    .file_range
                                    .as_ref()
                                    .is_some_and(|range| range.last_index() == Some(index));
                                if last {
                                    state.stop();
                                }
                                if selected {
                                    return Some((Ok(ExtractionEvent::Media(media)), state));
                                }
                            }
                        }
                    }
                }
                skipped = skipped.wrapping_add(1);
                if skipped == 0 {
                    tokio::task::yield_now().await;
                }
            }
        })
        .boxed()
    }
}
