//! Explicit extractor post boundaries, kept separate from file identities.
use crate::{
    Result,
    extract::{Media, PostMetadata},
};
use futures_util::{StreamExt, stream::BoxStream};
use std::collections::VecDeque;

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
    Post(Box<Post>),
    Media(Box<Media>),
    EndPost,
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
