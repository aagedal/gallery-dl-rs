//! Actual source job fixture: a successful mutation followed by an ASCII write failure.
use super::*;
use crate::{
    event::{ChildMetadata, ChildScope, KeywordUpdate},
    postprocess::Specification,
};
use serde_json::json;

#[tokio::test]
async fn failed_child_after_mutations_survive_explicit_and_eof_finalization() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../tests/fixtures/postprocessors/child_failure.json"
    ))
    .unwrap();
    assert_eq!(fixture["status"], 1);
    assert!(
        fixture["records"]
            .as_array()
            .unwrap()
            .iter()
            .any(|record| record["phase"] == "UnicodeEncodeError")
    );
    let expected = fixture["records"].as_array().unwrap().last().unwrap()["keywords"].clone();
    let keys = expected
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    for explicit_end in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let mut options = fixture["options"].as_array().unwrap().clone();
        // Both jobs resolve the failing sidecar in their configured base directory.
        options[1]["base-directory"] = json!(root.path());
        options.push(json!({"name":"metadata", "mode":"json", "event":"finalize", "base-directory":root.path(), "filename":"final.json", "include":keys}));
        let processors = Postprocessors::new(
            options
                .iter()
                .map(|value| Specification::parse(value, root.path()).unwrap())
                .collect(),
        );
        let configurations = Arc::new(BTreeMap::from([(
            "pixiv".into(),
            JobConfiguration {
                processors: Arc::new(processors),
                root: root.path().to_owned(),
                filename: None,
                directory: None,
                init: false,
            },
        )]));
        let child = ChildScope::new(
            ChildMetadata::from_raw("pixiv", "work", &Map::new())
                .with_keyword_updates(vec![KeywordUpdate::Increment("num".into())]),
        );
        let mut events = vec![
            Ok(ExtractionEvent::Post(Box::new(Post {
                category: "pixiv".into(),
                metadata: None,
                count: None,
                keywords: KeywordPolicy::Shared {
                    feedback: None,
                    objects: Arc::from([]),
                    file_fields: Arc::from([]),
                    post_fields: Arc::new(
                        json!({"id":42,"num":0,"seed":42,"suffix":"original"})
                            .as_object()
                            .unwrap()
                            .clone(),
                    ),
                },
            }))),
            Ok(ExtractionEvent::BeginChild(child.clone())),
        ];
        if explicit_end {
            events.push(Ok(ExtractionEvent::EndChild(child.control)));
            events.push(Ok(ExtractionEvent::EndPost));
        }
        let results = apply(futures_util::stream::iter(events).boxed(), configurations)
            .collect::<Vec<_>>()
            .await;
        assert_eq!(results.len(), 1, "explicit_end={explicit_end}");
        assert!(results[0].is_err());
        let mut actual: Value =
            serde_json::from_slice(&std::fs::read(root.path().join("final.json")).unwrap())
                .unwrap();
        let attempt = Path::new(actual["attempt"].as_str().unwrap())
            .strip_prefix(root.path())
            .unwrap();
        actual["attempt"] =
            format!("$WORK/{}", attempt.to_string_lossy().replace('\\', "/")).into();
        assert_eq!(actual, expected, "explicit_end={explicit_end}");
    }
}
