use futures_util::{StreamExt, TryStreamExt, stream};
use gallery_dl_rs::{
    event::{self, ChildMetadata, ExtractionEvent, KeywordPolicy},
    extract::{Media, MediaJob, PostMetadata},
    filter::FilterExpression,
    naming::FilenameTemplate,
    postprocess::{
        self, Postprocessors, Specification,
        jobs::{self, JobConfiguration},
    },
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path, sync::Arc};

fn file() -> Media {
    Media {
        url: "https://fixture.invalid/42.jpg".parse().unwrap(),
        job: MediaJob::InlineText {
            data: "fixture".into(),
        },
        category: "danbooru".into(),
        filename: "42.jpg".into(),
        metadata: Some(PostMetadata {
            post_id: "42".into(),
            media_id: "42".into(),
            index: 1,
            timestamp: Some(1700000000),
            booru_keywords: Some(Arc::new(
                json!({"filename":"ORIGINAL","extension":"jpg"})
                    .as_object()
                    .unwrap()
                    .clone(),
            )),
            ..Default::default()
        }),
        headers: Default::default(),
        handoff: Default::default(),
    }
}
fn metadata(mode: &Value) -> ChildMetadata {
    ChildMetadata::from_booru_artist("danbooru","artist",json!({"id":7,"name":"ARTIST","date":"2024-01-01T00:00:00Z","filename":"INHERITED","extension":"jpeg","search_tags":"inherited","nested":{"x":1}}).as_object().unwrap(),&"https://danbooru.donmai.us/posts?tags=ARTIST".parse().unwrap()).unwrap().with_booru_artist_inheritance(mode).unwrap()
}
fn events(
    mode: &Value,
) -> futures_util::stream::BoxStream<'static, gallery_dl_rs::Result<ExtractionEvent>> {
    event::child_job(metadata(mode), || {
        let media = file();
        let policy = KeywordPolicy::shared(&["num", "index", "media_id"])
            .with_post_fields(&media, &["filename", "name", "extension"]);
        stream::iter(
            event::group_with_keywords(vec![media], "danbooru", policy)
                .into_iter()
                .map(Ok),
        )
        .boxed()
    })
}
#[tokio::test]
async fn artist_inheritance_modes_keep_provenance_and_project_only_selected_raw_keywords() {
    for mode in [
        Value::Null,
        json!(false),
        json!(0),
        json!(""),
        json!([]),
        json!({}),
        json!("artist_meta"),
        json!(true),
        json!([1]),
    ] {
        let output: Vec<_> = events(&mode).try_collect().await.unwrap();
        let media = output
            .iter()
            .find_map(|event| {
                if let ExtractionEvent::Media(media) = event {
                    Some(media)
                } else {
                    None
                }
            })
            .unwrap();
        let proof = media
            .metadata
            .as_ref()
            .unwrap()
            .booru_artist_parent
            .as_ref()
            .unwrap();
        let context = postprocess::public_keywords(&postprocess::context(media));
        assert!(!context.contains_key("metadata"));
        assert!(!context.contains_key("num"));
        assert!(!context.contains_key("_extractor"));
        if mode == json!("artist_meta") {
            assert_eq!(proof.key(), Some("artist_meta"));
            assert_eq!(context["artist_meta"]["id"], 7);
            assert!(!context.contains_key("name"));
            assert!(
                FilterExpression::parse("artist_meta['nested']['x'] == 1")
                    .unwrap()
                    .matches(media)
                    .unwrap()
            );
            let template = FilenameTemplate::parse_with_keywords(
                "{artist_meta[name]}_{post_id}.bin",
                &["artist_meta".into()],
            )
            .unwrap();
            assert_eq!(template.render(media).unwrap(), "ARTIST_42.bin");
        } else if proof.is_flat() {
            assert_eq!(context["id"], 7);
            assert_eq!(context["name"], "ARTIST");
            assert_eq!(context["date"], "2024-01-01T00:00:00Z");
            assert_eq!(context["subcategory"], "artist");
            assert_eq!(context["filename"], "INHERITED");
            assert_eq!(media.metadata.as_ref().unwrap().post_id, "42");
        } else {
            assert!(!context.contains_key("name"));
            assert!(!context.contains_key("_parent"));
            assert_eq!(context["id"], 42);
        }
        let serialized = serde_json::to_string(media.metadata.as_ref().unwrap()).unwrap();
        assert!(!serialized.contains("booru_artist_parent"));
    }
}

fn configuration(root: &Path, specs: &[Value], init: bool) -> JobConfiguration {
    JobConfiguration {
        processors: Arc::new(Postprocessors::new(
            specs
                .iter()
                .map(|value| Specification::parse(value, root).unwrap())
                .collect(),
        )),
        root: root.into(),
        filename: None,
        directory: None,
        init,
    }
}
#[tokio::test]
async fn artist_flat_reapplies_job_fields_at_url_and_uses_immutable_tag_configuration() {
    let root = tempfile::tempdir().unwrap();
    let configs = Arc::new(BTreeMap::from([
        (
            "danbooru.artist".into(),
            configuration(
                root.path(),
                &[
                    json!({"name":"metadata","event":"child","mode":"modify","fields":{"name":"UPDATED","category":"HOOK_LABEL"}}),
                ],
                true,
            ),
        ),
        (
            "danbooru.tag".into(),
            configuration(
                root.path(),
                &[
                    json!({"name":"metadata","event":"post","mode":"modify","fields":{"marker":"TAG_CONFIG","id":"{'999'}","name":"POST_NAME"}}),
                ],
                false,
            ),
        ),
    ]));
    let mut input = jobs::apply_with_source_class(
        events(&json!(true)),
        configs,
        Some("danbooru".into()),
        Some("artist".into()),
        None,
    );
    let job = input.next().await.unwrap().unwrap();
    let scope = job.scope.as_ref().unwrap();
    let post = postprocess::public_keywords(&scope.keywords());
    assert_eq!(post["id"], "999");
    assert_eq!(post["name"], "POST_NAME");
    assert_eq!(post["marker"], "TAG_CONFIG");
    let mut file_context = postprocess::context(&job.media);
    scope.inherit(&mut file_context).await;
    let public = postprocess::public_keywords(&file_context);
    assert_eq!(public["id"], 7);
    assert_eq!(public["name"], "UPDATED");
    assert_eq!(public["category"], "HOOK_LABEL");
    assert_eq!(public["marker"], "TAG_CONFIG");
    scope.complete(&file_context);
    drop(job);
    assert!(input.next().await.is_none());
}
