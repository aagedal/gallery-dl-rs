use futures_util::{StreamExt, TryStreamExt, stream};
use gallery_dl_rs::{
    event::{self, ChildMetadata, ExtractionEvent},
    filter::FilterExpression,
    job_selection::JobSelection,
    selection::RangeSelection,
};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

fn child(
    name: &str,
    accepted: bool,
    calls: Arc<AtomicUsize>,
) -> futures_util::stream::BoxStream<'static, gallery_dl_rs::Result<ExtractionEvent>> {
    let raw = json!({"name":name,"accepted":accepted,"nested":{"rows":[{"value":"deep"}]},"date":"2024-01-01T00:00:00Z"});
    let metadata = ChildMetadata::from_booru_artist(
        "e926",
        "artist-search",
        raw.as_object().unwrap(),
        &format!("https://e926.net/posts?tags={name}")
            .parse()
            .unwrap(),
    )
    .unwrap();
    event::child_job(metadata, move || {
        calls.fetch_add(1, Ordering::SeqCst);
        stream::empty().boxed()
    })
}

#[tokio::test]
async fn artist_dedup_runs_after_predicates_and_consumes_range_positions() {
    let calls = Arc::new(AtomicUsize::new(0));
    let input = stream::iter(vec![
        child("same", false, calls.clone()),
        child("same", true, calls.clone()),
        child("same", true, calls.clone()),
        child("tail", true, calls.clone()),
    ])
    .flatten()
    .boxed();
    let selection = JobSelection {
        child_filter: Some(
            FilterExpression::parse("accepted and nested['rows'][0]['value'] == 'deep'").unwrap(),
        ),
        child_range: Some(RangeSelection::parse("1-2").unwrap()),
        ..Default::default()
    };
    let events: Vec<_> = selection.apply_events(input).try_collect().await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, ExtractionEvent::BeginChild(_)))
            .count(),
        1
    );
    let ExtractionEvent::BeginChild(scope) = &events[0] else {
        panic!()
    };
    assert_eq!(scope.metadata.target_category(), "e621");
    assert_eq!(scope.metadata.fields()["_extractor"], "E621TagExtractor");
    assert_eq!(
        scope.metadata.fields()["_url"],
        "https://e926.net/posts?tags=same"
    );
}

#[tokio::test]
async fn raw_artist_events_keep_duplicate_queues_and_full_payload() {
    let calls = Arc::new(AtomicUsize::new(0));
    let events: Vec<_> = child("same", true, calls.clone())
        .chain(child("same", true, calls.clone()))
        .try_collect()
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(events.len(), 4);
    let ExtractionEvent::BeginChild(scope) = &events[0] else {
        panic!()
    };
    assert_eq!(
        scope.metadata.fields()["nested"]["rows"][0]["value"],
        "deep"
    );
    assert!(
        FilterExpression::parse("date == '2024-01-01T00:00:00Z'")
            .unwrap()
            .matches_child(&scope.metadata)
            .unwrap()
    );
}
