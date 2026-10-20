use futures_util::{
    StreamExt, TryStreamExt,
    stream::{self, BoxStream},
};
use gallery_dl_rs::{
    Result, error,
    event::{self, ChildControl, ChildMetadata, ChildScope, ExtractionEvent, Post},
    filter::FilterExpression,
    job_selection::JobSelection,
    selection::RangeSelection,
};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

fn metadata(id: u64) -> ChildMetadata {
    ChildMetadata::from_raw("fixture", "profile", json!({"id": id}).as_object().unwrap())
}
fn begin(event: Result<ExtractionEvent>) -> ChildControl {
    match event.unwrap() {
        ExtractionEvent::BeginChild(scope) => scope.control,
        other => panic!("expected BeginChild, got {other:?}"),
    }
}
fn end(event: Result<ExtractionEvent>, expected: &ChildControl) {
    match event.unwrap() {
        ExtractionEvent::EndChild(control) => assert!(control.same_scope(expected)),
        other => panic!("expected EndChild, got {other:?}"),
    }
}
struct Dropped(Arc<AtomicBool>);
impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
fn watched_pending(dropped: Arc<AtomicBool>) -> BoxStream<'static, Result<ExtractionEvent>> {
    let guard = Dropped(dropped);
    stream::once(async move {
        let _guard = guard;
        std::future::pending().await
    })
    .boxed()
}

#[tokio::test]
async fn factory_is_lazy_and_rejection_cancels_without_starting() {
    let started = Arc::new(AtomicUsize::new(0));
    let count = started.clone();
    let mut input = event::child_job(metadata(1), move || {
        count.fetch_add(1, Ordering::SeqCst);
        stream::empty().boxed()
    });
    assert_eq!(started.load(Ordering::SeqCst), 0);
    let control = begin(input.next().await.unwrap());
    assert_eq!(started.load(Ordering::SeqCst), 0);
    control.cancel();
    end(input.next().await.unwrap(), &control);
    assert!(input.next().await.is_none());
    assert_eq!(started.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn selection_rejections_and_empty_ranges_never_build_child() {
    for selection in [
        JobSelection {
            child_filter: Some(FilterExpression::parse("id == 2").unwrap()),
            ..Default::default()
        },
        JobSelection {
            child_range: Some(RangeSelection::parse("0").unwrap()),
            ..Default::default()
        },
        JobSelection {
            file_range: Some(RangeSelection::parse("0").unwrap()),
            ..Default::default()
        },
    ] {
        let started = Arc::new(AtomicUsize::new(0));
        let count = started.clone();
        let input = event::child_job(metadata(1), move || {
            count.fetch_add(1, Ordering::SeqCst);
            stream::empty().boxed()
        });
        let output: Vec<_> = selection.apply(input).try_collect().await.unwrap();
        assert!(output.is_empty());
        assert_eq!(started.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn natural_eof_and_recoverable_errors_keep_balanced_outer_scope() {
    let mut input = event::child_job(metadata(1), || {
        stream::iter([
            Err(error("fixture recoverable error")),
            Ok(ExtractionEvent::Post(Box::new(Post {
                category: "fixture".into(),
                metadata: None,
                count: Some(0),
            }))),
            Ok(ExtractionEvent::EndPost),
        ])
        .boxed()
    });
    let control = begin(input.next().await.unwrap());
    assert!(input.next().await.unwrap().is_err());
    assert!(matches!(
        input.next().await.unwrap().unwrap(),
        ExtractionEvent::Post(_)
    ));
    assert!(matches!(
        input.next().await.unwrap().unwrap(),
        ExtractionEvent::EndPost
    ));
    end(input.next().await.unwrap(), &control);
    assert!(input.next().await.is_none());
    assert!(!control.is_cancelled());
}

#[tokio::test]
async fn dropping_pending_nested_adapter_cancels_descendants_and_drops_input() {
    let dropped = Arc::new(AtomicBool::new(false));
    let leaf_dropped = dropped.clone();
    let mut input = event::child_job(metadata(1), move || {
        event::child_job(metadata(2), move || {
            event::child_job(metadata(3), move || watched_pending(leaf_dropped))
        })
    });
    let outer = begin(input.next().await.unwrap());
    let middle = begin(input.next().await.unwrap());
    let inner = begin(input.next().await.unwrap());
    // Start the innermost input before cancellation, retaining the pending
    // next future in the adapter, as a consumer would between polls.
    assert!(futures_util::poll!(input.next()).is_pending());
    outer.cancel();
    // Cancellation is cooperative; dropping the adapter interrupts the
    // pending input immediately without requiring a producer wakeup.
    drop(input);
    assert!(dropped.load(Ordering::SeqCst));
    assert!(middle.is_cancelled());
    assert!(inner.is_cancelled());
}

#[tokio::test]
async fn canceled_outer_emits_visible_descendant_ends_in_reverse_order() {
    let dropped = Arc::new(AtomicBool::new(false));
    let guard = Dropped(dropped.clone());
    let middle = ChildScope::new(metadata(2));
    let inner = ChildScope::new(metadata(3));
    let middle_control = middle.control.clone();
    let inner_control = inner.control.clone();
    let mut input = event::child_job(metadata(1), move || {
        let tail = stream::once(async move {
            let _guard = guard;
            std::future::pending().await
        });
        stream::iter([
            Ok(ExtractionEvent::BeginChild(middle)),
            Ok(ExtractionEvent::BeginChild(inner)),
        ])
        .chain(tail)
        .boxed()
    });
    let outer = begin(input.next().await.unwrap());
    begin(input.next().await.unwrap());
    begin(input.next().await.unwrap());
    outer.cancel();
    end(input.next().await.unwrap(), &inner_control);
    assert!(dropped.load(Ordering::SeqCst));
    end(input.next().await.unwrap(), &middle_control);
    end(input.next().await.unwrap(), &outer);
    assert!(input.next().await.is_none());
    assert!(inner_control.is_cancelled());
    assert!(middle_control.is_cancelled());
}

#[tokio::test]
async fn cancellation_during_pending_poll_hides_and_cancels_new_scope() {
    let (send, receive) = tokio::sync::oneshot::channel();
    let unseen = ChildScope::new(metadata(3));
    let unseen_control = unseen.control.clone();
    let visible = ChildScope::new(metadata(2));
    let visible_control = visible.control.clone();
    let dropped = Arc::new(AtomicBool::new(false));
    let guard = Dropped(dropped.clone());
    let mut input = event::child_job(metadata(1), move || {
        stream::once(async move { Ok(ExtractionEvent::BeginChild(visible)) })
            .chain(stream::once(async move {
                let _guard = guard;
                receive.await.unwrap();
                Ok(ExtractionEvent::BeginChild(unseen))
            }))
            .boxed()
    });
    let outer = begin(input.next().await.unwrap());
    begin(input.next().await.unwrap());
    assert!(futures_util::poll!(input.next()).is_pending());
    outer.cancel();
    send.send(()).unwrap();
    end(input.next().await.unwrap(), &visible_control);
    assert!(unseen_control.is_cancelled());
    assert!(visible_control.is_cancelled());
    assert!(dropped.load(Ordering::SeqCst));
    end(input.next().await.unwrap(), &outer);
    assert!(input.next().await.is_none());
}

#[tokio::test]
async fn dropping_adapter_cancels_all_exposed_scopes_and_drops_factory_or_input() {
    for start_input in [false, true] {
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = Dropped(dropped.clone());
        let child = ChildScope::new(metadata(2));
        let child_control = child.control.clone();
        let mut input = event::child_job(metadata(1), move || {
            stream::unfold((Some(child), guard), |(child, guard)| async move {
                if let Some(child) = child {
                    Some((Ok(ExtractionEvent::BeginChild(child)), (None, guard)))
                } else {
                    std::future::pending().await
                }
            })
            .boxed()
        });
        let outer = begin(input.next().await.unwrap());
        if start_input {
            begin(input.next().await.unwrap());
        }
        assert!(!dropped.load(Ordering::SeqCst));
        drop(input);
        assert!(outer.is_cancelled());
        assert!(dropped.load(Ordering::SeqCst));
        if start_input {
            assert!(child_control.is_cancelled());
        }
    }
}

#[tokio::test]
async fn malformed_source_boundaries_are_not_repaired_or_hidden() {
    for malformed in [
        vec![Ok(ExtractionEvent::BeginChild(ChildScope::new(metadata(
            2,
        ))))],
        vec![Ok(ExtractionEvent::EndChild(ChildControl::default()))],
        vec![Ok(ExtractionEvent::Post(Box::new(Post {
            category: "fixture".into(),
            metadata: None,
            count: None,
        })))],
    ] {
        let output: Vec<_> = JobSelection::default()
            .apply(event::child_job(metadata(1), move || {
                stream::iter(malformed).boxed()
            }))
            .collect()
            .await;
        assert!(
            output.iter().any(Result::is_err),
            "framing violation was hidden"
        );
    }
}

#[tokio::test]
async fn natural_nested_completion_emits_each_end_once_and_restores_siblings() {
    let first = event::child_job(metadata(2), || {
        stream::iter([Err(error("recoverable child failure"))]).boxed()
    });
    let second = event::child_job(metadata(3), || stream::empty().boxed());
    let mut input = event::child_job(metadata(1), move || first.chain(second).boxed());
    let outer = begin(input.next().await.unwrap());
    let first = begin(input.next().await.unwrap());
    assert!(input.next().await.unwrap().is_err());
    end(input.next().await.unwrap(), &first);
    let second = begin(input.next().await.unwrap());
    end(input.next().await.unwrap(), &second);
    end(input.next().await.unwrap(), &outer);
    assert!(input.next().await.is_none());
    assert!(!outer.is_cancelled());
    assert!(!first.is_cancelled());
    assert!(!second.is_cancelled());
}
