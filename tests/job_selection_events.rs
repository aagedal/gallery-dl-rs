use futures_util::{
    StreamExt, TryStreamExt,
    stream::{self, BoxStream},
};
use gallery_dl_rs::{
    Result,
    event::{self, ChildMetadata, ExtractionEvent, Post},
    extract::{self, Media, PostMetadata},
    filter::FilterExpression,
    job_selection::JobSelection,
    post_selection::PostSelection,
    selection::RangeSelection,
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
fn post(id: &str) -> ExtractionEvent {
    ExtractionEvent::Post(Box::new(Post {
        keywords: Default::default(),
        category: "direct".into(),
        metadata: Some(PostMetadata {
            post_id: id.into(),
            ..Default::default()
        }),
        count: Some(0),
    }))
}
fn media() -> ExtractionEvent {
    let media: Media =
        extract::direct(&extract::http_url("https://fixture.test/a.jpg").unwrap()).unwrap();
    ExtractionEvent::Media(Box::new(media))
}
fn input(events: Vec<ExtractionEvent>) -> BoxStream<'static, Result<ExtractionEvent>> {
    stream::iter(events.into_iter().map(Ok)).boxed()
}
fn labels(events: &[ExtractionEvent]) -> Vec<&'static str> {
    events
        .iter()
        .map(|e| match e {
            ExtractionEvent::Post(_) => "post",
            ExtractionEvent::EndPost => "end",
            ExtractionEvent::Media(_) => "media",
            ExtractionEvent::BeginChild(_) => "beginchild",
            ExtractionEvent::EndChild(_) => "endchild",
        })
        .collect()
}
#[tokio::test]
async fn selected_empty_posts_retain_balanced_boundaries_and_rejected_posts_have_none() {
    let selection = JobSelection {
        post_selection: Some(PostSelection::new(
            None,
            Some(FilterExpression::parse("post_id != 'reject'").unwrap()),
        )),
        ..Default::default()
    };
    let output = selection
        .apply_events(input(vec![
            post("reject"),
            ExtractionEvent::EndPost,
            post("keep"),
            ExtractionEvent::EndPost,
        ]))
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    assert_eq!(labels(&output), ["post", "end"]);
    let ExtractionEvent::Post(post) = &output[0] else {
        panic!()
    };
    assert_eq!(post.metadata.as_ref().unwrap().post_id, "keep");
    assert_eq!(post.count, Some(0));
}
#[tokio::test]
async fn child_boundaries_preserve_scopes_and_rejected_factory_is_never_started() {
    let calls = Arc::new(AtomicUsize::new(0));
    let children = stream::iter([1, 2])
        .flat_map({
            let calls = calls.clone();
            move |id| {
                let calls = calls.clone();
                event::child_job(
                    ChildMetadata::from_raw(
                        "pixiv",
                        "user",
                        serde_json::json!({"id":id}).as_object().unwrap(),
                    ),
                    move || {
                        calls.fetch_add(1, Ordering::SeqCst);
                        input(vec![post("child"), ExtractionEvent::EndPost])
                    },
                )
            }
        })
        .boxed();
    let selection = JobSelection {
        child_filter: Some(FilterExpression::parse("id == 2").unwrap()),
        ..Default::default()
    };
    let events = selection
        .apply_events(children)
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    assert_eq!(labels(&events), ["beginchild", "post", "end", "endchild"]);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let (ExtractionEvent::BeginChild(begin), ExtractionEvent::EndChild(end)) =
        (&events[0], &events[3])
    else {
        panic!()
    };
    assert!(begin.control.same_scope(end));
}
#[tokio::test]
async fn end_post_is_not_polled_until_previous_media_has_been_consumed() {
    let ended = Arc::new(AtomicBool::new(false));
    let flag = ended.clone();
    let source = input(vec![post("one"), media()])
        .chain(stream::once(async move {
            flag.store(true, Ordering::SeqCst);
            Ok(ExtractionEvent::EndPost)
        }))
        .boxed();
    let mut selected = JobSelection::default().apply_events(source);
    assert!(matches!(
        selected.next().await.unwrap().unwrap(),
        ExtractionEvent::Post(_)
    ));
    assert!(!ended.load(Ordering::SeqCst));
    assert!(matches!(
        selected.next().await.unwrap().unwrap(),
        ExtractionEvent::Media(_)
    ));
    assert!(!ended.load(Ordering::SeqCst));
    assert!(matches!(
        selected.next().await.unwrap().unwrap(),
        ExtractionEvent::EndPost
    ));
    assert!(ended.load(Ordering::SeqCst));
}
#[tokio::test]
async fn finite_file_cut_synthesizes_end_without_polling_unused_tail_in_root_and_child() {
    for child in [false, true] {
        let polled = Arc::new(AtomicBool::new(false));
        let flag = polled.clone();
        let source = input(vec![post("one"), media()])
            .chain(stream::once(async move {
                flag.store(true, Ordering::SeqCst);
                Ok(media())
            }))
            .boxed();
        let source = if child {
            event::child_job(
                ChildMetadata::from_raw("pixiv", "user", &Default::default()),
                move || source,
            )
        } else {
            source
        };
        let events = JobSelection {
            file_range: Some(RangeSelection::parse("1").unwrap()),
            ..Default::default()
        }
        .apply_events(source)
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
        assert_eq!(
            labels(&events),
            if child {
                vec!["beginchild", "post", "media", "end", "endchild"]
            } else {
                vec!["post", "media", "end"]
            }
        );
        assert!(!polled.load(Ordering::SeqCst));
    }
}
