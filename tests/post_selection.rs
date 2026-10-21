use futures_util::{StreamExt, TryStreamExt, stream};
use gallery_dl_rs::{
    error,
    event::{self, ExtractionEvent, Post},
    extract::{self, Media, PostMetadata},
    filter::FilterExpression,
    post_selection::PostSelection,
    selection::RangeSelection,
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
fn file(id: &str, index: usize, time: i64) -> Media {
    let mut media = extract::direct(
        &extract::http_url(&format!("https://cdn.example/{id}_{index}.jpg")).unwrap(),
    )
    .unwrap();
    media.category = "twitter".into();
    media.metadata = Some(PostMetadata {
        post_id: id.into(),
        media_id: index.to_string(),
        index,
        timestamp: Some(time),
        caption: Some(format!("Post {id}").into()),
        ..Default::default()
    });
    media
}
fn group(id: &str, count: usize, time: i64) -> Vec<ExtractionEvent> {
    event::group(
        (1..=count).map(|index| file(id, index, time)).collect(),
        "twitter",
    )
    .into_iter()
    .collect()
}
fn select(range: Option<&str>, filter: Option<&str>) -> PostSelection {
    PostSelection::new(
        range.map(|value| RangeSelection::parse(value).unwrap()),
        filter.map(|value| FilterExpression::parse(value).unwrap()),
    )
}
async fn selected(events: Vec<ExtractionEvent>, selection: PostSelection) -> Vec<Media> {
    selection
        .apply(
            stream::iter(events.into_iter().map(Ok)).boxed(),
            None,
            None,
            false,
        )
        .try_collect()
        .await
        .unwrap()
}
#[tokio::test]
async fn explicit_posts_keep_all_siblings_and_do_not_infer_identity_transitions() {
    let mut events = group("10", 2, 200);
    events.extend(group("10", 3, 200));
    let items = selected(events, select(Some("2"), None)).await;
    assert_eq!(items.len(), 3);
    assert_eq!(
        items
            .iter()
            .map(|item| item.metadata.as_ref().unwrap().index)
            .collect::<Vec<_>>(),
        [1, 2, 3]
    );
}
#[tokio::test]
async fn empty_posts_count_but_declared_count_never_truncates_files() {
    let mut events: Vec<_> = event::group(Vec::new(), "twitter").into_iter().collect();
    let mut second = group("20", 3, 200);
    if let ExtractionEvent::Post(post) = &mut second[0] {
        post.count = Some(0);
    }
    events.extend(second);
    assert_eq!(selected(events, select(Some("2"), None)).await.len(), 3);
}
#[tokio::test]
async fn post_filters_precede_post_ranges_without_file_specific_context() {
    let mut events = group("10", 1, 200);
    events.extend(group("20", 2, 200));
    events.extend(group("30", 3, 200));
    let items=selected(events,select(Some("1"),Some("count >= 2 and metadata.get('file') is None and metadata.get('media_id') is None and metadata.get('index') is None"))).await;
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].metadata.as_ref().unwrap().post_id, "20");
    assert!(
        selected(
            group("10", 2, 200),
            select(None, Some("extension == 'jpg'"))
        )
        .await
        .is_empty()
    );
    assert!(
        selected(group("10", 2, 200), select(None, Some("1 / 0")))
            .await
            .is_empty()
    );
}
#[tokio::test]
async fn post_filter_precedes_date_selection_and_range_with_ordered_or_unordered_cutoff() {
    let mut events = group("10", 1, 50);
    events.extend(group("20", 2, 200));
    let input = stream::iter(events.clone().into_iter().map(Ok)).boxed();
    assert!(
        select(Some("1"), None)
            .apply(input, None, Some(100), false)
            .try_collect::<Vec<_>>()
            .await
            .unwrap()
            .is_empty()
    );
    let input = stream::iter(events.into_iter().map(Ok)).boxed();
    let items = select(Some("1"), Some("count >= 2"))
        .apply(input, Some(300), Some(100), true)
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].metadata.as_ref().unwrap().post_id, "20");
}
#[tokio::test]
async fn finite_and_empty_post_ranges_drop_upstream_without_polling_later_work() {
    for empty in [false, true] {
        let touched = Arc::new(AtomicBool::new(false));
        let future_flag = touched.clone();
        let first = stream::iter(group("10", 2, 200).into_iter().map(Ok));
        let later = stream::once(async move {
            future_flag.store(true, Ordering::SeqCst);
            Ok(ExtractionEvent::EndPost)
        });
        let input = first.chain(later).boxed();
        let items = select(Some(if empty { "0" } else { "1" }), None)
            .apply(input, None, None, false)
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        assert_eq!(items.len(), if empty { 0 } else { 2 });
        assert!(!touched.load(Ordering::SeqCst));
    }
}
#[tokio::test]
async fn extraction_errors_preserve_positions_and_bad_framing_is_private() {
    let input = stream::iter(
        std::iter::once(Err(error("fixture error"))).chain(group("10", 2, 200).into_iter().map(Ok)),
    )
    .boxed();
    let mut result = select(Some("1"), None).apply(input, None, None, false);
    assert_eq!(
        result.next().await.unwrap().unwrap_err().to_string(),
        "fixture error"
    );
    assert_eq!(result.try_collect::<Vec<_>>().await.unwrap().len(), 2);
    let broken = stream::iter(
        std::iter::once(Err(error("earlier source error")))
            .chain(group("20", 1, 200).into_iter().take(2).map(Ok)),
    )
    .boxed();
    let mut broken = select(None, None).apply(broken, None, None, false);
    assert!(broken.next().await.unwrap().is_err());
    assert!(broken.next().await.unwrap().is_ok());
    assert!(
        broken
            .next()
            .await
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("completed")
    );
    for events in [
        vec![ExtractionEvent::Media(Box::new(file("10", 1, 200)))],
        vec![ExtractionEvent::EndPost],
        vec![ExtractionEvent::Post(Box::new(Post::from_media(
            &[file("10", 1, 200)],
            "twitter",
        )))],
    ] {
        let error = select(None, None)
            .apply(
                stream::iter(events.into_iter().map(Ok)).boxed(),
                None,
                None,
                false,
            )
            .try_collect::<Vec<_>>()
            .await
            .unwrap_err()
            .to_string();
        assert!(!error.contains("cdn.example"));
        assert!(error.contains("post"));
    }
}
#[tokio::test]
async fn unbounded_empty_posts_yield_for_cancellation() {
    let input = stream::repeat(ExtractionEvent::Post(Box::new(Post {
        keywords: Default::default(),
        category: "twitter".into(),
        metadata: None,
        count: Some(0),
    })))
    .flat_map(|post| stream::iter([Ok(post), Ok(ExtractionEvent::EndPost)]))
    .boxed();
    let mut input = select(None, None).apply(input, None, None, false);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(20), input.next())
            .await
            .is_err()
    );
}
