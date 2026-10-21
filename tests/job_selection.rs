use futures_util::{
    StreamExt, TryStreamExt,
    stream::{self, BoxStream},
};
use gallery_dl_rs::{
    Result, error,
    event::{self, ChildMetadata, ChildScope, ExtractionEvent, Post},
    extract::{self, Media, PostMetadata},
    filter::FilterExpression,
    job_selection::JobSelection,
    post_selection::PostSelection,
    selection::RangeSelection,
};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

fn range(value: &str) -> RangeSelection {
    RangeSelection::parse(value).unwrap()
}
fn file(id: u64, time: Option<i64>) -> Media {
    let mut media =
        extract::direct(&extract::http_url(&format!("https://fixture.test/{id}.jpg")).unwrap())
            .unwrap();
    media.metadata = Some(PostMetadata {
        post_id: id.to_string(),
        timestamp: time,
        ..Default::default()
    });
    media
}
fn post(ids: &[u64], time: Option<i64>) -> BoxStream<'static, Result<ExtractionEvent>> {
    stream::iter(
        event::group(ids.iter().map(|id| file(*id, time)).collect(), "direct")
            .into_iter()
            .map(Ok),
    )
    .boxed()
}
fn scope(id: u64) -> ChildScope {
    ChildScope::new(ChildMetadata::from_raw(
        "kemono",
        "user",
        json!({"id":id,"name":format!("child{id}")})
            .as_object()
            .unwrap(),
    ))
}
/// A producer checks the child's control before every poll, including startup.
fn child(
    scope: ChildScope,
    input: BoxStream<'static, Result<ExtractionEvent>>,
    requests: Arc<AtomicUsize>,
) -> BoxStream<'static, Result<ExtractionEvent>> {
    let control = scope.control.clone();
    let end = control.clone();
    stream::once(async move { Ok(ExtractionEvent::BeginChild(scope)) })
        .chain(stream::unfold(
            (input, control, false),
            move |(mut input, control, mut started)| {
                let requests = requests.clone();
                async move {
                    if control.is_cancelled() {
                        return None;
                    }
                    if !started {
                        requests.fetch_add(1, Ordering::SeqCst);
                        started = true;
                    }
                    input
                        .next()
                        .await
                        .map(|event| (event, (input, control, started)))
                }
            },
        ))
        .chain(stream::once(
            async move { Ok(ExtractionEvent::EndChild(end)) },
        ))
        .boxed()
}
async fn ids(
    selection: JobSelection,
    input: BoxStream<'static, Result<ExtractionEvent>>,
) -> Vec<u64> {
    selection
        .apply(input)
        .map_ok(|media| media.metadata.unwrap().post_id.parse::<u64>().unwrap())
        .try_collect()
        .await
        .unwrap()
}
fn count() -> Arc<AtomicUsize> {
    Arc::new(AtomicUsize::new(0))
}

#[tokio::test]
async fn file_counters_reset_for_children_and_restore_suspended_parent_and_grandparent() {
    let requests = count();
    let grand = child(scope(3), post(&[31, 32, 33], None), requests.clone());
    let middle = post(&[11], None)
        .chain(grand)
        .chain(post(&[12, 13], None))
        .boxed();
    let nested = child(scope(1), middle, requests.clone());
    let sibling = child(scope(2), post(&[21, 22, 23], None), requests.clone());
    let root = post(&[1], None)
        .chain(nested)
        .chain(sibling)
        .chain(post(&[2, 3], None))
        .boxed();
    assert_eq!(
        ids(
            JobSelection {
                file_range: Some(range("2")),
                ..Default::default()
            },
            root
        )
        .await,
        [32, 12, 22, 2]
    );
    assert_eq!(requests.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn child_filter_precedes_range_and_rejected_children_never_start() {
    let requests = [count(), count(), count(), count()];
    let mut input = stream::empty().boxed();
    for (index, request) in requests.iter().enumerate() {
        input = input
            .chain(child(
                scope(index as u64 + 1),
                post(&[index as u64 + 1], None),
                request.clone(),
            ))
            .boxed();
    }
    let selection = JobSelection {
        child_filter: Some(FilterExpression::parse("id >= 2").unwrap()),
        child_range: Some(range("2")),
        ..Default::default()
    };
    assert_eq!(ids(selection, input).await, [3]);
    assert_eq!(
        requests
            .iter()
            .map(|v| v.load(Ordering::SeqCst))
            .collect::<Vec<_>>(),
        [0, 0, 1, 0]
    );
}

#[tokio::test]
async fn finite_child_range_stops_only_its_parent_and_allows_root_siblings() {
    let requests = [count(), count(), count(), count(), count()];
    let first = child(scope(10), post(&[10], None), requests[0].clone());
    let second = child(scope(11), post(&[11], None), requests[1].clone());
    let third = child(scope(12), post(&[12], None), requests[2].clone());
    let outer = child(
        scope(1),
        first.chain(second).chain(third).boxed(),
        requests[3].clone(),
    );
    let sibling = child(scope(2), post(&[20], None), requests[4].clone());
    assert_eq!(
        ids(
            JobSelection {
                child_range: Some(range("1-2")),
                ..Default::default()
            },
            outer.chain(sibling).boxed()
        )
        .await,
        [10, 11, 20]
    );
    assert_eq!(
        requests
            .iter()
            .map(|v| v.load(Ordering::SeqCst))
            .collect::<Vec<_>>(),
        [1, 1, 0, 1, 1]
    );
}

#[tokio::test]
async fn post_range_resets_per_child_and_file_filter_precedes_file_counter() {
    let a = child(
        scope(1),
        post(&[10], None)
            .chain(post(&[11, 12], None))
            .chain(post(&[13], None))
            .boxed(),
        count(),
    );
    let b = child(
        scope(2),
        post(&[20], None).chain(post(&[21, 22], None)).boxed(),
        count(),
    );
    let selection = JobSelection {
        post_selection: Some(PostSelection::new(Some(range("2")), None)),
        file_filter: Some(FilterExpression::parse("post_id in [12, 22]").unwrap()),
        file_range: Some(range("1")),
        ..Default::default()
    };
    assert_eq!(ids(selection, a.chain(b).boxed()).await, [12, 22]);
}

#[tokio::test]
async fn ordered_dates_cancel_only_current_child_and_directory_dates_control_all_files() {
    let a = scope(1);
    let control = a.control.clone();
    let input = child(
        a,
        post(&[10], Some(100)).chain(post(&[11], Some(300))).boxed(),
        count(),
    )
    .chain(child(scope(2), post(&[20], Some(200)), count()))
    .boxed();
    assert_eq!(
        ids(
            JobSelection {
                after: Some(150),
                ..Default::default()
            },
            input
        )
        .await,
        [20]
    );
    assert!(control.is_cancelled());
    let mut late = event::group(vec![file(1, Some(200)), file(2, Some(100))], "direct");
    if let ExtractionEvent::Post(post) = late.front_mut().unwrap() {
        post.metadata = None;
    }
    assert_eq!(
        ids(
            JobSelection {
                after: Some(150),
                ..Default::default()
            },
            stream::iter(late.into_iter().map(Ok)).boxed()
        )
        .await,
        [1, 2]
    );
    let mut late = event::group(vec![file(3, Some(100))], "direct");
    if let ExtractionEvent::Post(post) = late.front_mut().unwrap() {
        post.metadata.as_mut().unwrap().timestamp = Some(200);
    }
    assert_eq!(
        ids(
            JobSelection {
                post_selection: Some(PostSelection::new(None, None)),
                after: Some(150),
                ..Default::default()
            },
            stream::iter(late.into_iter().map(Ok)).boxed()
        )
        .await,
        [3]
    );
}

struct Dropped(Arc<AtomicBool>);
impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
#[tokio::test]
async fn last_root_file_drops_input_before_yield_and_child_last_file_sets_control_before_yield() {
    let dropped = Arc::new(AtomicBool::new(false));
    let guard = Dropped(dropped.clone());
    let input = post(&[1, 2], None)
        .map(move |event| {
            let _ = &guard;
            event
        })
        .boxed();
    let mut selected = JobSelection {
        file_range: Some(range("1")),
        ..Default::default()
    }
    .apply(input);
    assert!(selected.next().await.unwrap().is_ok());
    assert!(dropped.load(Ordering::SeqCst));
    assert!(selected.next().await.is_none());
    let s = scope(1);
    let control = s.control.clone();
    let mut selected = JobSelection {
        file_range: Some(range("1")),
        ..Default::default()
    }
    .apply(child(s, post(&[1, 2], None), count()));
    assert!(selected.next().await.unwrap().is_ok());
    assert!(control.is_cancelled());
    assert!(selected.next().await.is_none());
}

#[tokio::test]
async fn empty_ranges_do_not_poll_root_and_empty_child_range_stops_at_first_eligible_child() {
    for selection in [
        JobSelection {
            file_range: Some(range("0")),
            ..Default::default()
        },
        JobSelection {
            post_selection: Some(PostSelection::new(Some(range("0")), None)),
            ..Default::default()
        },
    ] {
        let polled = count();
        let p = polled.clone();
        let input = stream::once(async move {
            p.fetch_add(1, Ordering::SeqCst);
            Ok(ExtractionEvent::EndPost)
        })
        .boxed();
        assert!(selection.apply(input).next().await.is_none());
        assert_eq!(polled.load(Ordering::SeqCst), 0);
    }
    let request = count();
    let input = post(&[1], None)
        .chain(child(scope(1), post(&[2], None), request.clone()))
        .chain(post(&[3], None))
        .boxed();
    assert_eq!(
        ids(
            JobSelection {
                child_range: Some(range("0")),
                ..Default::default()
            },
            input
        )
        .await,
        [1]
    );
    assert_eq!(request.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn empty_child_range_waits_for_a_child_that_passes_filter_then_stops_its_parent() {
    let requests = count();
    let later = count();
    let polled = later.clone();
    let input = post(&[1], None)
        .chain(child(scope(1), post(&[2], None), requests.clone()))
        .chain(post(&[3], None))
        .chain(child(scope(2), post(&[4], None), requests.clone()))
        .chain(stream::once(async move {
            polled.fetch_add(1, Ordering::SeqCst);
            Err(error("later parent page must not start"))
        }))
        .boxed();
    assert_eq!(
        ids(
            JobSelection {
                child_filter: Some(FilterExpression::parse("id >= 2").unwrap()),
                child_range: Some(range("0")),
                ..Default::default()
            },
            input
        )
        .await,
        [1, 3]
    );
    assert_eq!(requests.load(Ordering::SeqCst), 0);
    assert_eq!(later.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn malformed_boundaries_and_unclosed_child_scopes_fail_privately() {
    for case in 0..5 {
        let a = scope(1);
        let b = scope(2);
        let events = match case {
            0 => vec![ExtractionEvent::EndChild(a.control.clone())],
            1 => vec![
                ExtractionEvent::BeginChild(a.clone()),
                ExtractionEvent::EndChild(b.control.clone()),
            ],
            2 => vec![ExtractionEvent::BeginChild(a.clone())],
            3 => vec![
                ExtractionEvent::BeginChild(a.clone()),
                ExtractionEvent::BeginChild(a.clone()),
            ],
            _ => vec![
                ExtractionEvent::BeginChild(a.clone()),
                ExtractionEvent::Post(Box::new(Post {
                    keywords: Default::default(),
                    category: "direct".into(),
                    metadata: None,
                    count: None,
                })),
                ExtractionEvent::EndChild(a.control.clone()),
            ],
        };
        let result = JobSelection::default()
            .apply(stream::iter(events.into_iter().map(Ok)).boxed())
            .collect::<Vec<_>>()
            .await;
        assert!(
            result
                .last()
                .unwrap()
                .as_ref()
                .unwrap_err()
                .to_string()
                .contains("child")
        );
    }
}

#[tokio::test]
async fn errors_do_not_advance_positions_and_child_failure_does_not_poison_parent_framing() {
    let s = scope(1);
    let control = s.control.clone();
    let bad = stream::iter([
        Ok(ExtractionEvent::BeginChild(s)),
        Ok(ExtractionEvent::Post(Box::new(Post {
            keywords: Default::default(),
            category: "direct".into(),
            metadata: None,
            count: None,
        }))),
        Err(error("fixture child failed")),
        Ok(ExtractionEvent::EndChild(control)),
    ])
    .boxed();
    let good = child(scope(2), post(&[20], None), count());
    let selection = JobSelection {
        post_selection: Some(PostSelection::new(Some(range("1")), None)),
        ..Default::default()
    };
    let result = selection
        .apply(bad.chain(good).boxed())
        .collect::<Vec<_>>()
        .await;
    assert_eq!(result.len(), 2);
    assert_eq!(
        result[0].as_ref().unwrap_err().to_string(),
        "fixture child failed"
    );
    assert_eq!(
        result[1]
            .as_ref()
            .unwrap()
            .metadata
            .as_ref()
            .unwrap()
            .post_id,
        "20"
    );
}

#[tokio::test]
async fn nesting_depth_is_bounded_and_dropping_consumer_cancels_open_scopes() {
    let active_scope = scope(1);
    let control = active_scope.control.clone();
    let mut input = JobSelection::default().apply(child(active_scope, post(&[1], None), count()));
    assert!(input.next().await.unwrap().is_ok());
    drop(input);
    assert!(control.is_cancelled());
    let scopes = (0..128).map(scope).collect::<Vec<_>>();
    let input = stream::iter(
        scopes
            .iter()
            .cloned()
            .map(|scope| Ok(ExtractionEvent::BeginChild(scope)))
            .collect::<Vec<_>>(),
    )
    .boxed();
    let result = JobSelection::default()
        .apply(input)
        .collect::<Vec<_>>()
        .await;
    assert!(
        result[0]
            .as_ref()
            .unwrap_err()
            .to_string()
            .contains("nesting")
    );
    assert!(scopes.iter().all(|scope| scope.control.is_cancelled()));
}

#[tokio::test]
async fn cancelling_outer_job_propagates_before_poll_and_unwinds_open_inner_posts() {
    let outer = scope(1);
    let outer_control = outer.control.clone();
    let inner = scope(2);
    let inner_control = inner.control.clone();
    let opening = stream::iter(vec![
        Ok(ExtractionEvent::BeginChild(outer)),
        Ok(ExtractionEvent::Post(Box::new(Post {
            keywords: Default::default(),
            category: "direct".into(),
            metadata: None,
            count: None,
        }))),
        Ok(ExtractionEvent::BeginChild(inner)),
        Ok(ExtractionEvent::Post(Box::new(Post {
            keywords: Default::default(),
            category: "direct".into(),
            metadata: None,
            count: None,
        }))),
        Ok(ExtractionEvent::Media(Box::new(file(1, None)))),
    ]);
    let closing = stream::once(async move {
        assert!(
            inner_control.is_cancelled(),
            "descendant must be canceled before producer's next poll"
        );
        Ok(ExtractionEvent::EndChild(inner_control))
    })
    .chain(stream::once({
        let control = outer_control.clone();
        async move { Ok(ExtractionEvent::EndChild(control)) }
    }));
    let input = opening
        .chain(closing)
        .chain(child(scope(3), post(&[20], None), count()))
        .boxed();
    let mut selected = JobSelection::default().apply(input);
    assert_eq!(
        selected
            .next()
            .await
            .unwrap()
            .unwrap()
            .metadata
            .unwrap()
            .post_id,
        "1"
    );
    outer_control.cancel();
    assert_eq!(
        selected
            .next()
            .await
            .unwrap()
            .unwrap()
            .metadata
            .unwrap()
            .post_id,
        "20"
    );
    assert!(selected.next().await.is_none());
}
