//! Explicit extractor post boundaries, kept separate from file identities.
use crate::{
    Result,
    extract::{Media, PostMetadata},
};
use futures_util::{StreamExt, stream::BoxStream};
use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

/// Cooperative cancellation shared by one parent/child extraction scope.
/// This handle contains no URL, credentials, or producer-owned metadata.
#[derive(Clone, Default)]
pub struct ChildControl(Arc<AtomicBool>);

impl ChildControl {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    pub fn same_scope(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl std::fmt::Debug for ChildControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ChildControl { scope: [redacted] }")
    }
}

/// Bounded public child-selection metadata, never the raw queue payload.
/// IDs intentionally retain their original scalar representation: a filter
/// may reject an invalid identity before the producer validates or uses it.
#[derive(Clone)]
pub struct ChildMetadata {
    category: String,
    subcategory: String,
    fields: serde_json::Map<String, serde_json::Value>,
}

impl ChildMetadata {
    pub fn from_raw(
        category: &str,
        subcategory: &str,
        data: &serde_json::Map<String, serde_json::Value>,
    ) -> Self {
        fn label(value: &str) -> String {
            if value.len() <= 64
                && value
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
            {
                value.to_owned()
            } else {
                String::new()
            }
        }
        fn scalar(value: &serde_json::Value) -> Option<serde_json::Value> {
            match value {
                serde_json::Value::String(text) => {
                    // Public names/topics and invalid IDs must retain their
                    // exact meaning for filters, including URLs and newlines.
                    (text.len() <= 64 * 1024 && !text.contains('\0')).then(|| value.clone())
                }
                serde_json::Value::Number(number) => {
                    (number.as_str().len() <= 128).then(|| value.clone())
                }
                serde_json::Value::Bool(_) | serde_json::Value::Null => Some(value.clone()),
                serde_json::Value::Array(_) | serde_json::Value::Object(_) => None,
            }
        }
        let mut fields = serde_json::Map::new();
        for key in [
            "id",
            "name",
            "account",
            "service",
            "user",
            "indexed",
            "updated",
            "favorited",
            "faved_seq",
            "server_id",
            "channel_id",
            "server",
            "channel",
            "channel_topic",
            "channel_nsfw",
            "channel_type",
            "parent_id",
            "post_count",
        ] {
            let Some(value) = data.get(key) else { continue };
            let value = if matches!(key, "server" | "channel") {
                if let Some(object) = value.as_object() {
                    // Only the public one-level queue shape is retained. Do
                    // not descend into arbitrary API response objects.
                    let keys: &[&str] = if key == "server" {
                        &["id", "name", "indexed", "updated"]
                    } else {
                        &[
                            "id",
                            "server_id",
                            "name",
                            "parent_channel_id",
                            "topic",
                            "is_nsfw",
                            "type",
                            "post_count",
                            "position",
                            "theme_color",
                            "icon_emoji",
                        ]
                    };
                    let public: serde_json::Map<_, _> = keys
                        .iter()
                        .filter_map(|key| {
                            scalar(object.get(*key)?).map(|value| ((*key).to_owned(), value))
                        })
                        .collect();
                    (!public.is_empty()).then_some(serde_json::Value::Object(public))
                } else {
                    scalar(value)
                }
            } else {
                scalar(value)
            };
            if let Some(value) = value {
                fields.insert(key.into(), value);
            }
        }
        Self {
            category: label(category),
            subcategory: label(subcategory),
            fields,
        }
    }

    pub fn category(&self) -> &str {
        &self.category
    }

    pub fn subcategory(&self) -> &str {
        &self.subcategory
    }

    pub fn fields(&self) -> &serde_json::Map<String, serde_json::Value> {
        &self.fields
    }
}

impl std::fmt::Debug for ChildMetadata {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ChildMetadata { fields: [redacted] }")
    }
}

#[derive(Clone, Debug)]
pub struct ChildScope {
    pub control: ChildControl,
    pub metadata: ChildMetadata,
}

impl ChildScope {
    pub fn new(metadata: ChildMetadata) -> Self {
        Self {
            control: ChildControl::default(),
            metadata,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Post {
    pub category: String,
    pub metadata: Option<PostMetadata>,
    /// Declared file count; this is metadata, not a framing delimiter.
    pub count: Option<usize>,
}
impl Post {
    pub fn from_media(media: &[Media], category: &str) -> Self {
        let mut metadata = media.first().and_then(|media| media.metadata.clone());
        if let Some(metadata) = &mut metadata {
            metadata.file = None;
            metadata.media_id.clear();
            metadata.index = 0;
        }
        Self {
            category: category.into(),
            metadata,
            count: Some(media.len()),
        }
    }
}
#[derive(Clone, Debug)]
pub enum ExtractionEvent {
    BeginChild(ChildScope),
    EndChild(ChildControl),
    Post(Box<Post>),
    Media(Box<Media>),
    EndPost,
}

/// Queue one independent child job without starting its extractor until the
/// consumer has seen its selection metadata. Canceling the scope drops active
/// work and closes any visible descendant scopes before closing this child.
pub fn child_job<'a>(
    metadata: ChildMetadata,
    build: impl FnOnce() -> BoxStream<'a, Result<ExtractionEvent>> + Send + 'a,
) -> BoxStream<'a, Result<ExtractionEvent>> {
    struct State<'a, F> {
        metadata: Option<ChildMetadata>,
        control: ChildControl,
        build: Option<F>,
        input: Option<BoxStream<'a, Result<ExtractionEvent>>>,
        descendants: Vec<ChildControl>,
        ended: bool,
    }
    impl<F> Drop for State<'_, F> {
        fn drop(&mut self) {
            if !self.ended {
                self.control.cancel();
            }
            for control in &self.descendants {
                control.cancel();
            }
        }
    }
    let state = State {
        metadata: Some(metadata),
        control: ChildControl::default(),
        build: Some(build),
        input: None,
        descendants: Vec::new(),
        ended: false,
    };
    futures_util::stream::unfold(state, |mut state| async move {
        if let Some(metadata) = state.metadata.take() {
            let scope = ChildScope {
                control: state.control.clone(),
                metadata,
            };
            return Some((Ok(ExtractionEvent::BeginChild(scope)), state));
        }
        loop {
            if state.ended {
                return None;
            }
            if state.control.is_cancelled() {
                state.input = None;
                state.build = None;
                for control in &state.descendants {
                    control.cancel();
                }
                if let Some(control) = state.descendants.pop() {
                    return Some((Ok(ExtractionEvent::EndChild(control)), state));
                }
                state.ended = true;
                return Some((Ok(ExtractionEvent::EndChild(state.control.clone())), state));
            }
            if let Some(build) = state.build.take() {
                state.input = Some(build());
            }
            let event = state.input.as_mut().expect("active child job").next().await;
            if state.control.is_cancelled() {
                // A cancellation can arrive while the producer was pending.
                // A newly returned scope has not been shown to the consumer.
                if let Some(Ok(ExtractionEvent::BeginChild(scope))) = event {
                    scope.control.cancel();
                }
                continue;
            }
            match event {
                Some(event) => {
                    match &event {
                        Ok(ExtractionEvent::BeginChild(scope)) => {
                            state.descendants.push(scope.control.clone());
                        }
                        Ok(ExtractionEvent::EndChild(control))
                            if state
                                .descendants
                                .last()
                                .is_some_and(|current| current.same_scope(control)) =>
                        {
                            state.descendants.pop();
                        }
                        _ => {}
                    }
                    return Some((event, state));
                }
                None => {
                    state.input = None;
                    state.ended = true;
                    return Some((Ok(ExtractionEvent::EndChild(state.control.clone())), state));
                }
            }
        }
    })
    .boxed()
}
pub fn group(media: Vec<Media>, category: &str) -> VecDeque<ExtractionEvent> {
    let mut events = VecDeque::with_capacity(media.len() + 2);
    events.push_back(ExtractionEvent::Post(Box::new(Post::from_media(
        &media, category,
    ))));
    events.extend(
        media
            .into_iter()
            .map(|media| ExtractionEvent::Media(Box::new(media))),
    );
    events.push_back(ExtractionEvent::EndPost);
    events
}
/// Preserve the original file-only API without guessing boundaries from IDs.
pub fn media_only<'a>(
    input: BoxStream<'a, Result<ExtractionEvent>>,
) -> BoxStream<'a, Result<Media>> {
    futures_util::stream::unfold(input, |mut input| async move {
        let mut skipped = 0u8;
        while let Some(event) = input.next().await {
            match event {
                Ok(ExtractionEvent::Media(media)) => return Some((Ok(*media), input)),
                Err(error) => return Some((Err(error), input)),
                _ => {
                    skipped = skipped.wrapping_add(1);
                    if skipped == 0 {
                        tokio::task::yield_now().await;
                    }
                }
            }
        }
        None
    })
    .boxed()
}

/// Bound active extraction across every event, page, retry, and child lookup.
/// Time between consumer polls is excluded; exhaustion drops the input before
/// yielding the terminal error, releasing active requests and owned children.
pub fn with_extraction_budget<'a, T: Send + 'a>(
    input: BoxStream<'a, Result<T>>,
    budget: std::time::Duration,
) -> BoxStream<'a, Result<T>> {
    futures_util::stream::unfold(Some((input, budget)), |state| async move {
        let (mut input, remaining) = state?;
        if remaining.is_zero() {
            return Some((Err(crate::error("extraction timed out")), None));
        }
        let started = tokio::time::Instant::now();
        let next = tokio::time::timeout(remaining, input.next()).await;
        let elapsed = started.elapsed();
        match next {
            Ok(item) if elapsed <= remaining => {
                item.map(|item| (item, Some((input, remaining.saturating_sub(elapsed)))))
            }
            _ => Some((Err(crate::error("extraction timed out")), None)),
        }
    })
    .boxed()
}

#[cfg(test)]
mod budget_tests {
    use super::*;
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::Duration,
    };

    struct Dropped(Arc<AtomicBool>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn one_budget_spans_items_but_excludes_consumer_pauses() {
        let dropped = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let input =
            futures_util::stream::unfold((0, Dropped(dropped.clone())), move |(n, guard)| {
                let seen = seen.clone();
                async move {
                    seen.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_secs(6)).await;
                    Some((Ok(n), (n + 1, guard)))
                }
            })
            .boxed();
        let mut input = with_extraction_budget(input, Duration::from_secs(11));
        assert_eq!(input.next().await.unwrap().unwrap(), 0);
        tokio::time::sleep(Duration::from_secs(100)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(
            input
                .next()
                .await
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("timed out")
        );
        assert!(dropped.load(Ordering::SeqCst));
        assert!(input.next().await.is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn rejected_events_and_recoverable_errors_do_not_reset_the_budget() {
        let input =
            futures_util::stream::iter([Ok(1), Err(crate::error("recoverable")), Ok(3), Ok(4)])
                .then(|item| async move {
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    item
                })
                .boxed();
        let input = with_extraction_budget(input, Duration::from_secs(10));
        let events = input
            .filter(|item| std::future::ready(!matches!(item, Ok(1))))
            .collect::<Vec<_>>()
            .await;
        assert_eq!(events.len(), 3);
        assert!(
            events[0]
                .as_ref()
                .unwrap_err()
                .to_string()
                .contains("recoverable")
        );
        assert_eq!(*events[1].as_ref().unwrap(), 3);
        assert!(
            events[2]
                .as_ref()
                .unwrap_err()
                .to_string()
                .contains("timed out")
        );
    }

    #[tokio::test]
    async fn zero_budget_and_consumer_drop_do_not_start_later_work() {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let input = futures_util::stream::once(async move {
            seen.fetch_add(1, Ordering::SeqCst);
            Ok(1)
        })
        .boxed();
        let mut input = with_extraction_budget(input, Duration::ZERO);
        assert!(input.next().await.unwrap().is_err());
        assert!(input.next().await.is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let dropped = Arc::new(AtomicBool::new(false));
        let input = futures_util::stream::unfold(Dropped(dropped.clone()), |guard| async {
            Some((Ok(1), guard))
        })
        .boxed();
        let mut input = with_extraction_budget(input, Duration::from_secs(1));
        assert_eq!(input.next().await.unwrap().unwrap(), 1);
        drop(input);
        assert!(dropped.load(Ordering::SeqCst));
    }
}
