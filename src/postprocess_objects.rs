//! Explicit nested object identity for hook dictionaries. The observer carries
//! mutation intents outside JSON, including equal root replacements.
use crate::event::{KeywordObject, KeywordPolicy};
use serde_json::{Map, Value};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[derive(Clone, Default)]
pub(crate) struct Bindings(Arc<[Binding]>);
struct Binding {
    name: String,
    object: KeywordObject,
    attached: AtomicBool,
}
tokio::task_local! { static OBSERVER: Bindings; }
impl Bindings {
    pub(crate) fn new(policy: &KeywordPolicy) -> Self {
        let KeywordPolicy::Shared { objects, .. } = policy else {
            return Self::default();
        };
        Self(
            objects
                .iter()
                .map(|(name, object)| Binding {
                    name: name.clone(),
                    object: object.clone(),
                    attached: AtomicBool::new(true),
                })
                .collect(),
        )
    }
    pub(crate) fn refresh(&self, context: &mut Map<String, Value>) {
        for binding in self.0.iter() {
            if binding.attached.load(Ordering::Acquire) {
                context.insert(
                    binding.name.clone(),
                    Value::Object(binding.object.snapshot()),
                );
            }
        }
    }
    pub(crate) async fn observe<F: std::future::Future>(&self, future: F) -> F::Output {
        OBSERVER.scope(self.clone(), future).await
    }
    fn record(&self, keys: &[&str], context: &Map<String, Value>) {
        let Some(root) = keys.first() else { return };
        for binding in self.0.iter().filter(|binding| binding.name == *root) {
            if keys.len() == 1 {
                // Assignment/deletion is an identity change even if its new
                // value compares equal to the producer dictionary.
                binding.attached.store(false, Ordering::Release);
            } else if binding.attached.load(Ordering::Acquire)
                && let Some(Value::Object(value)) = context.get(*root)
            {
                binding.object.replace(value);
            }
        }
    }
}
pub(crate) fn record(keys: &[&str], context: &Map<String, Value>) {
    let _ = OBSERVER.try_with(|observer| observer.record(keys, context));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        extract::{Media, MediaJob},
        postprocess::metadata::MetadataOptions,
    };
    use serde_json::json;
    use std::path::Path;
    fn object() -> KeywordObject {
        KeywordObject::new(
            json!({"id":7,"name":"ORIGINAL"})
                .as_object()
                .unwrap()
                .clone(),
        )
    }
    fn media() -> Media {
        Media {
            url: "https://example.invalid/file.txt".parse().unwrap(),
            job: MediaJob::DirectHttp,
            category: "fixture".into(),
            filename: "file.txt".into(),
            metadata: None,
            headers: Default::default(),
            handoff: Default::default(),
        }
    }
    async fn run(bindings: &Bindings, context: &mut Map<String, Value>, options: Value) {
        let options = MetadataOptions::parse(options.as_object().unwrap(), Path::new(".")).unwrap();
        bindings
            .observe(options.run(context, &media(), Path::new("file.txt"), Path::new(".")))
            .await
            .unwrap();
    }
    #[tokio::test]
    async fn nested_mutations_share_identity_but_equal_root_replacement_detaches() {
        for detach in [false, true] {
            let object = object();
            let policy = KeywordPolicy::shared(&[]).with_shared_object("pool", object.clone());
            let bindings = Bindings::new(&policy);
            let mut context = Map::new();
            bindings.refresh(&mut context);
            if detach {
                run(
                    &bindings,
                    &mut context,
                    json!({"mode":"modify","fields":{"pool":"{pool}"}}),
                )
                .await;
            }
            run(
                &bindings,
                &mut context,
                json!({"mode":"modify","fields":{"pool[name]":"LOCAL","pool[extra]":"NEW"}}),
            )
            .await;
            assert_eq!(context["pool"]["name"], "LOCAL");
            assert_eq!(
                object.snapshot()["name"],
                if detach { "ORIGINAL" } else { "LOCAL" }
            );
            let next = Bindings::new(&policy);
            let mut next_context = Map::new();
            next.refresh(&mut next_context);
            assert_eq!(
                next_context["pool"]["name"],
                if detach { "ORIGINAL" } else { "LOCAL" }
            );
            assert!(!format!("{object:?}").contains("ORIGINAL"));
        }
    }
    #[tokio::test]
    async fn root_delete_detaches_only_current_post_and_failed_nested_write_keeps_attachment() {
        let object = object();
        let policy = KeywordPolicy::shared(&[]).with_shared_object("pool", object.clone());
        let bindings = Bindings::new(&policy);
        let mut context = Map::new();
        bindings.refresh(&mut context);
        run(&bindings,&mut context,json!({"mode":"modify","fields":{"pool[missing][name]":"IGNORED","pool[name]":"PUBLISHED"}})).await;
        assert_eq!(object.snapshot()["name"], "PUBLISHED");
        run(
            &bindings,
            &mut context,
            json!({"mode":"delete","fields":["pool"]}),
        )
        .await;
        bindings.refresh(&mut context);
        assert!(!context.contains_key("pool"));
        let mut next = Map::new();
        Bindings::new(&policy).refresh(&mut next);
        assert_eq!(next["pool"]["name"], "PUBLISHED");
    }
    #[tokio::test]
    async fn task_scoped_observers_do_not_cross_concurrent_jobs_or_survive_cancellation() {
        let first = object();
        let second = object();
        let a =
            Bindings::new(&KeywordPolicy::shared(&[]).with_shared_object("pool", first.clone()));
        let b =
            Bindings::new(&KeywordPolicy::shared(&[]).with_shared_object("pool", second.clone()));
        let modify = |bindings: Bindings, value: &'static str| async move {
            let mut context = Map::new();
            bindings.refresh(&mut context);
            bindings
                .observe(async {
                    tokio::task::yield_now().await;
                    let options = MetadataOptions::parse(
                        json!({"mode":"modify","fields":{"pool[name]":value}})
                            .as_object()
                            .unwrap(),
                        Path::new("."),
                    )
                    .unwrap();
                    options
                        .run(
                            &mut context,
                            &media(),
                            Path::new("file.txt"),
                            Path::new("."),
                        )
                        .await
                        .unwrap();
                })
                .await;
        };
        tokio::join!(modify(a.clone(), "FIRST"), modify(b, "SECOND"));
        assert_eq!(first.snapshot()["name"], "FIRST");
        assert_eq!(second.snapshot()["name"], "SECOND");
        use futures_util::FutureExt;
        assert!(
            a.observe(std::future::pending::<()>())
                .now_or_never()
                .is_none()
        );
        let mut context = json!({"pool":{"name":"OUTSIDE"}})
            .as_object()
            .unwrap()
            .clone();
        record(&["pool", "name"], &context);
        context.clear();
        assert_eq!(first.snapshot()["name"], "FIRST");
    }
}
