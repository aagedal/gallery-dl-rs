//! Explicit nested object identity for hook dictionaries. The observer carries
//! mutation intents outside JSON, including equal root replacements.
use crate::event::{KeywordObject, KeywordPolicy};
use serde_json::{Map, Value};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[derive(Clone, Default)]
pub(crate) struct Bindings(
    Arc<std::collections::BTreeMap<String, Binding>>,
    Arc<std::collections::BTreeSet<String>>,
    Option<KeywordObject>,
);
struct Binding {
    name: String,
    object: KeywordObject,
    attached: AtomicBool,
    alias: std::sync::Mutex<Option<KeywordObject>>,
}
impl Binding {
    fn current(&self) -> KeywordObject {
        self.alias
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap_or_else(|| self.object.clone())
    }
}
tokio::task_local! { static OBSERVER: Bindings; }
impl Bindings {
    pub(crate) fn new(policy: &KeywordPolicy) -> Self {
        let KeywordPolicy::Shared { objects, .. } = policy else {
            return Self::default();
        };
        let roots = objects
            .iter()
            .filter(|(name, _)| name == "_parent")
            .map(|(name, _)| name.clone())
            .collect::<std::collections::BTreeSet<_>>();
        Self(
            Arc::new(
                objects
                    .iter()
                    .map(|(name, object)| {
                        (
                            name.clone(),
                            Binding {
                                name: name.clone(),
                                object: object.clone(),
                                attached: AtomicBool::new(true),
                                alias: Default::default(),
                            },
                        )
                    })
                    .collect(),
            ),
            Arc::new(roots),
            None,
        )
    }
    pub(crate) fn artist_queue(root: KeywordObject) -> Self {
        Self(Default::default(), Default::default(), Some(root))
    }
    pub(crate) fn for_artist(
        policy: &KeywordPolicy,
        inheritance: Option<&crate::event::ArtistInheritance>,
    ) -> Self {
        let mut bindings = Self::new(policy);
        if let Some(inheritance) = inheritance {
            bindings.1 = Arc::new(inheritance.keyword_names().into_iter().collect());
        }
        bindings
    }
    pub(crate) fn reapply_artist(&self) {
        for binding in self
            .0
            .values()
            .filter(|binding| self.1.contains(&binding.name))
        {
            binding.attached.store(true, Ordering::Release);
            *binding.alias.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
    }
    #[cfg(test)]
    pub(crate) fn reattach(&self, name: &str) {
        if let Some(binding) = self.0.get(name) {
            binding.attached.store(true, Ordering::Release);
            *binding.alias.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
    }
    pub(crate) fn refresh(&self, context: &mut Map<String, Value>) {
        if let Some(root) = &self.2 {
            *context = root.snapshot();
            return;
        }
        for binding in self.0.values() {
            if binding.attached.load(Ordering::Acquire) {
                context.insert(
                    binding.name.clone(),
                    Value::Object(binding.current().snapshot()),
                );
            }
        }
    }
    pub(crate) fn sync_context(&self, context: &Map<String, Value>) {
        if let Some(root) = &self.2 {
            root.sync(context);
        }
    }
    pub(crate) async fn observe<F: std::future::Future>(&self, future: F) -> F::Output {
        OBSERVER.scope(self.clone(), future).await
    }
    fn record(&self, keys: &[&str], context: &Map<String, Value>) {
        if let Some(root) = &self.2 {
            root.record(keys, context);
            return;
        }
        let Some(root) = keys.first() else { return };
        if let Some(binding) = self.0.get(*root) {
            if keys.len() == 1 {
                // Assignment/deletion is an identity change even if its new
                // value compares equal to the producer dictionary.
                binding.attached.store(false, Ordering::Release);
            } else if binding.attached.load(Ordering::Acquire)
                && let Some(Value::Object(value)) = context.get(*root)
            {
                binding.current().record(&keys[1..], value);
            }
        }
    }
}
pub(crate) fn is_artist_root(key: &str) -> bool {
    OBSERVER
        .try_with(|observer| {
            observer
                .2
                .as_ref()
                .map_or_else(|| observer.1.contains(key), |root| root.contains_key(key))
        })
        .unwrap_or(false)
}
pub(crate) fn is_artist_context() -> bool {
    OBSERVER
        .try_with(|observer| observer.2.is_some() || !observer.1.is_empty())
        .unwrap_or(false)
}
pub(crate) fn refresh_artist(context: &mut Map<String, Value>) {
    let _ = OBSERVER.try_with(|observer| {
        if let Some(root) = &observer.2 {
            *context = root.snapshot();
            return;
        }
        for binding in observer.0.values().filter(|binding| {
            observer.1.contains(&binding.name) && binding.attached.load(Ordering::Acquire)
        }) {
            context.insert(
                binding.name.clone(),
                Value::Object(binding.current().snapshot()),
            );
        }
    });
}
pub(crate) fn resolve_alias(path: &[String]) -> Option<KeywordObject> {
    OBSERVER
        .try_with(|observer| {
            if let Some(root) = &observer.2 {
                return root.object_at(path);
            }
            let name = path.first()?;
            if !observer.1.contains(name) {
                return None;
            }
            let binding = observer.0.get(name)?;
            if !binding.attached.load(Ordering::Acquire) {
                return None;
            }
            binding.current().object_at(&path[1..])
        })
        .ok()
        .flatten()
}
pub(crate) fn assign_alias(path: &str, alias: KeywordObject) {
    let keys = path
        .split('[')
        .map(|s| s.trim_end_matches(']').trim_matches(['\'', '"']))
        .collect::<Vec<_>>();
    let _ = OBSERVER.try_with(|observer| {
        if let Some(root) = &observer.2 {
            root.alias_at(&keys, &alias);
            return;
        }
        let Some(name) = keys.first() else {
            return;
        };
        if !observer.1.contains(*name) {
            return;
        }
        let Some(binding) = observer.0.get(*name) else {
            return;
        };
        if keys.len() == 1 {
            *binding.alias.lock().unwrap_or_else(|e| e.into_inner()) = Some(alias.clone());
            binding.attached.store(true, Ordering::Release);
        } else if binding.attached.load(Ordering::Acquire) {
            binding.current().alias_at(&keys[1..], &alias);
        }
    });
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
    async fn shallow_parent_copy_keeps_roots_separate_and_nested_identity_until_assignment() {
        for detach in [false, true] {
            let queue = KeywordObject::new(
                json!({"name":"QUEUE","nested":{"value":"OLD","deeper":{"x":1}}})
                    .as_object()
                    .unwrap()
                    .clone(),
            );
            let parent = queue.shallow_copy();
            let bindings = Bindings::new(
                &KeywordPolicy::shared(&[]).with_shared_object("_parent", parent.clone()),
            );
            let mut context = Map::new();
            bindings.refresh(&mut context);
            run(
                &bindings,
                &mut context,
                json!({"mode":"modify","fields":{"_parent[name]":"CHILD"}}),
            )
            .await;
            assert_eq!(queue.snapshot()["name"], "QUEUE");
            assert_eq!(parent.snapshot()["name"], "CHILD");
            if detach {
                context.insert("replacement".into(), context["_parent"]["nested"].clone());
                run(
                    &bindings,
                    &mut context,
                    json!({"mode":"modify","fields":{"_parent[nested]":"{replacement}"}}),
                )
                .await;
            }
            context.insert("two".into(), json!(2));
            run(&bindings,&mut context,json!({"mode":"modify","fields":{"_parent[nested][value]":"NEW","_parent[nested][deeper][x]":"{two}"}})).await;
            assert_eq!(parent.snapshot()["nested"]["value"], "NEW");
            assert_eq!(
                queue.snapshot()["nested"]["value"],
                if detach { "OLD" } else { "NEW" }
            );
            assert_eq!(
                queue.snapshot()["nested"]["deeper"]["x"],
                if detach { 1 } else { 2 }
            );
            context.insert("rootreplacement".into(), context["_parent"].clone());
            run(
                &bindings,
                &mut context,
                json!({"mode":"modify","fields":{"_parent":"{rootreplacement}"}}),
            )
            .await;
            run(
                &bindings,
                &mut context,
                json!({"mode":"modify","fields":{"_parent[name]":"DETACHED"}}),
            )
            .await;
            assert_eq!(parent.snapshot()["name"], "CHILD");
            bindings.reattach("_parent");
            bindings.refresh(&mut context);
            assert_eq!(context["_parent"]["name"], "CHILD");
        }
    }
    #[tokio::test]
    async fn artist_identity_assignments_alias_existing_nested_objects_until_next_file() {
        let queue = KeywordObject::new(
            json!({"name":"Q","nested":{"x":1},"replacement":{"nested":{"x":7}}})
                .as_object()
                .unwrap()
                .clone(),
        );
        let parent = queue.shallow_copy();
        let bindings = Bindings::new(
            &KeywordPolicy::shared(&[]).with_shared_object("_parent", parent.clone()),
        );
        let mut context = Map::new();
        bindings.refresh(&mut context);
        run(&bindings,&mut context,json!({"mode":"modify","fields":{"_parent[nested]":"{_parent[replacement][nested]}","_parent[nested][x]":"{_parent[nested][x]!i:A+1/}"}})).await;
        assert_eq!(queue.snapshot()["nested"]["x"], 1);
        assert_eq!(queue.snapshot()["replacement"]["nested"]["x"], 8);
        run(&bindings,&mut context,json!({"mode":"modify","fields":{"_parent":"{_parent[replacement]}","_parent[nested][x]":"{_parent[nested][x]!i:A+1/}"}})).await;
        assert_eq!(queue.snapshot()["replacement"]["nested"]["x"], 9);
        bindings.reattach("_parent");
        bindings.refresh(&mut context);
        assert_eq!(context["_parent"]["name"], "Q");
        assert_eq!(context["_parent"]["nested"]["x"], 9);
    }
    #[tokio::test]
    async fn artist_queue_hooks_preserve_raw_scalar_types_and_aliases_before_shallow_inheritance() {
        let queue = KeywordObject::new(
            json!({"date":"2024-01-01T00:04:05+05:00","left":{"x":1},"right":{"x":7}})
                .as_object()
                .unwrap()
                .clone(),
        );
        let bindings = Bindings::artist_queue(queue.clone());
        let mut context = queue.snapshot();
        run(&bindings,&mut context,json!({"mode":"modify","fields":{"copied":"{date}","converted":"{date!D}","year":"{converted:%Y}","left":"{right}","left[x]":"{left[x]!i:A+1/}"}})).await;
        assert_eq!(
            crate::postprocess::public_keywords(&context)["copied"],
            "2024-01-01T00:04:05+05:00"
        );
        assert_eq!(context["year"], "2023");
        assert_eq!(context["left"]["x"], 8);
        assert_eq!(context["right"]["x"], 8);
        let child = queue.shallow_copy();
        let mut copied = child.snapshot();
        copied["left"]["x"] = json!(9);
        child.record(&["left", "x"], &copied);
        assert_eq!(queue.snapshot()["right"]["x"], 9);
        run(
            &bindings,
            &mut context,
            json!({"mode":"modify","fields":{"date":"2024-01-01T00:00:00Z"}}),
        )
        .await;
        assert!(
            crate::postprocess::template::Template::parse("{date:%Y}")
                .unwrap()
                .render(&context)
                .is_err()
        );
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
