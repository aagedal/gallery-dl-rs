//! Ordered post/job hooks around independently concurrent file downloads.
use crate::{
    Result, error,
    event::{ChildControl, ExtractionEvent, Post},
    extract::{Media, MediaJob},
    naming::{DirectoryTemplate, FilenameTemplate},
    postprocess::{Hook, HookPaths, Postprocessors},
};
use futures_util::{StreamExt, stream::BoxStream};
use serde_json::{Map, Value};
use std::{
    collections::{BTreeMap, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::Notify;

#[derive(Clone, Debug)]
pub struct JobConfiguration {
    pub processors: Arc<Postprocessors>,
    pub root: PathBuf,
    pub filename: Option<FilenameTemplate>,
    pub directory: Option<DirectoryTemplate>,
    pub init: bool,
}
pub struct JobMedia {
    pub media: Media,
    pub scope: Option<ScopeLease>,
}
impl std::fmt::Debug for JobMedia {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("JobMedia { fields: [redacted] }")
    }
}
struct LeaseState {
    pending: usize,
    next: u64,
    initial: Map<String, Value>,
    last: Option<(u64, Map<String, Value>, Option<PathBuf>)>,
}
struct Shared {
    inner: Mutex<LeaseState>,
    notify: Notify,
    failed: Arc<AtomicBool>,
}
/// One emitted file's participation in its owning post. Drop on every outcome,
/// including filtering, deduplication, cancellation and failures.
pub struct ScopeLease {
    shared: Arc<Shared>,
    ordinal: u64,
}
impl std::fmt::Debug for ScopeLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ScopeLease { fields: [redacted] }")
    }
}
impl ScopeLease {
    pub fn keywords(&self) -> Map<String, Value> {
        self.shared
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .initial
            .clone()
    }
    /// Preserve source file order even when concurrent downloads finish out of order.
    pub fn complete(&self, context: &Map<String, Value>) {
        let mut state = self.shared.inner.lock().unwrap_or_else(|e| e.into_inner());
        if state
            .last
            .as_ref()
            .is_none_or(|(ordinal, _, _)| self.ordinal >= *ordinal)
        {
            let path = state
                .last
                .as_ref()
                .filter(|(ordinal, _, _)| *ordinal == self.ordinal)
                .and_then(|(_, _, path)| path.clone());
            state.last = Some((self.ordinal, context.clone(), path));
        }
    }
    pub fn complete_at(&self, context: &Map<String, Value>, path: &Path) {
        self.complete(context);
        let mut state = self.shared.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((ordinal, _, saved)) = &mut state.last
            && *ordinal == self.ordinal
        {
            *saved = Some(path.to_owned());
        }
    }
    pub fn fail(&self) {
        self.shared.failed.store(true, Ordering::Release);
    }
}
impl Drop for ScopeLease {
    fn drop(&mut self) {
        let mut state = self.shared.inner.lock().unwrap_or_else(|e| e.into_inner());
        state.pending -= 1;
        if state.pending == 0 {
            self.shared.notify.notify_waiters();
        }
    }
}
impl Shared {
    fn new(context: Map<String, Value>, failed: Arc<AtomicBool>) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(LeaseState {
                pending: 0,
                next: 0,
                initial: context,
                last: None,
            }),
            notify: Notify::new(),
            failed,
        })
    }
    fn lease(self: &Arc<Self>) -> Result<ScopeLease> {
        let mut state = self
            .inner
            .lock()
            .map_err(|_| error("postprocessor scope lock failed"))?;
        let pending = state
            .pending
            .checked_add(1)
            .ok_or_else(|| error("too many active postprocessor files"))?;
        let next = state
            .next
            .checked_add(1)
            .ok_or_else(|| error("too many postprocessor files"))?;
        state.pending = pending;
        state.next = next;
        Ok(ScopeLease {
            shared: self.clone(),
            ordinal: state.next,
        })
    }
    async fn finish(&self) -> (Map<String, Value>, Option<PathBuf>) {
        loop {
            // Register before testing the counter so the final Drop cannot race
            // with notification registration and leave the boundary waiting.
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                if state.pending == 0 {
                    return state.last.as_ref().map_or_else(
                        || (state.initial.clone(), None),
                        |(_, ctx, path)| (ctx.clone(), path.clone()),
                    );
                }
            }
            notified.await;
        }
    }
}
struct Runtime {
    configuration: JobConfiguration,
    media: Media,
    context: Map<String, Value>,
    paths: HookPaths,
    failed: Arc<AtomicBool>,
}
impl Runtime {
    async fn hook(&mut self, hook: Hook) -> Result<()> {
        self.configuration
            .processors
            .run_job_hook(
                hook,
                &mut self.context,
                &self.media,
                &mut self.paths,
                &self.configuration.root,
                self.configuration.filename.as_ref(),
            )
            .await?;
        Ok(())
    }
}
#[derive(Default)]
struct Frame {
    control: Option<ChildControl>,
    runtime: Option<Runtime>,
    post: Option<Arc<Shared>>,
    queue: Option<Map<String, Value>>,
    failed: bool,
}
impl Frame {
    fn mark_failed(&mut self) {
        self.failed = true;
        if let Some(runtime) = &self.runtime {
            runtime.failed.store(true, Ordering::Release);
        }
    }
    async fn end_post(&mut self) -> Result<()> {
        if let Some(post) = self.post.take() {
            let (context, path) = post.finish().await;
            if let Some(runtime) = &mut self.runtime {
                runtime.context = context;
                if let Some(path) = path {
                    runtime.paths.filename = path.file_name().map(|name| name.to_os_string());
                    runtime.paths.extension = runtime
                        .context
                        .get("extension")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .unwrap_or_else(|| {
                            path.extension()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .into_owned()
                        });
                    runtime.paths.realpath = Some(path);
                }
                runtime.hook(Hook::PostAfter).await?;
            }
        }
        Ok(())
    }
    async fn finalize(&mut self) -> Vec<crate::Error> {
        let mut errors = Vec::new();
        if let Err(error) = self.end_post().await {
            self.mark_failed();
            errors.push(error);
        }
        if let Some(runtime) = &mut self.runtime {
            let hook = if self.failed || runtime.failed.load(Ordering::Acquire) {
                Hook::FinalizeError
            } else {
                Hook::FinalizeSuccess
            };
            if let Err(error) = runtime.hook(hook).await {
                runtime.failed.store(true, Ordering::Release);
                errors.push(error);
            }
            if let Err(error) = runtime.hook(Hook::Finalize).await {
                errors.push(error);
            }
        }
        errors
    }
}
struct State<'a> {
    input: Option<BoxStream<'a, Result<ExtractionEvent>>>,
    configurations: Arc<BTreeMap<String, JobConfiguration>>,
    frames: Vec<Frame>,
    errors: VecDeque<crate::Error>,
    finished: bool,
    started: bool,
    root_category: Option<String>,
}
impl Drop for State<'_> {
    fn drop(&mut self) {
        for frame in &self.frames {
            if let Some(control) = &frame.control {
                control.cancel();
            }
        }
    }
}
fn fake(post: Post) -> Media {
    Media {
        url: url::Url::parse("https://invalid.invalid/").unwrap(),
        job: MediaJob::DirectHttp,
        category: post.category,
        filename: "post".into(),
        metadata: post.metadata,
        headers: BTreeMap::new(),
        handoff: BTreeMap::new(),
    }
}
fn post_directory(configuration: &JobConfiguration, media: &Media) -> Result<PathBuf> {
    crate::download::destination_with_templates(
        &configuration.root,
        media,
        None,
        configuration.directory.as_ref(),
    )?
    .parent()
    .map(Path::to_owned)
    .ok_or_else(|| error("invalid postprocessor directory"))
}
fn public_context(media: &Media) -> Map<String, Value> {
    let mut context = crate::postprocess::context(media);
    for key in ["url", "_url", "filename", "name", "extension"] {
        context.remove(key);
    }
    context
}
impl State<'_> {
    async fn initialize_eager(&mut self, category: &str) -> Result<()> {
        let Some(configuration) = self
            .configurations
            .get(category)
            .filter(|configuration| configuration.init)
            .cloned()
        else {
            return Ok(());
        };
        let frame = self
            .frames
            .last_mut()
            .ok_or_else(|| error("missing postprocessor job scope"))?;
        if frame.runtime.is_some() {
            return Ok(());
        }
        frame.runtime = Some(Runtime {
            configuration,
            media: fake(Post {
                category: category.into(),
                metadata: None,
                count: None,
            }),
            context: Map::new(),
            paths: HookPaths::default(),
            failed: Arc::new(AtomicBool::new(frame.failed)),
        });
        let runtime = frame.runtime.as_mut().unwrap();
        let result = runtime.hook(Hook::Init).await;
        frame.post = Some(Shared::new(runtime.context.clone(), runtime.failed.clone()));
        result
    }
    async fn shutdown(&mut self) {
        self.input = None;
        while let Some(mut frame) = self.frames.pop() {
            self.errors.extend(frame.finalize().await);
            if let Some(control) = &frame.control {
                control.cancel();
            }
            if let Some(parent) = self.frames.last_mut() {
                if frame.failed
                    || frame
                        .runtime
                        .as_ref()
                        .is_some_and(|r| r.failed.load(Ordering::Acquire))
                {
                    parent.mark_failed();
                }
                if let Some(runtime) = &mut parent.runtime {
                    runtime.context = frame.queue.take().unwrap_or_default();
                    if let Err(error) = runtime.hook(Hook::ChildAfter).await {
                        parent.mark_failed();
                        self.errors.push_back(error);
                    }
                }
            }
        }
        self.finished = true;
    }
    async fn event(&mut self, event: ExtractionEvent) -> Result<Option<JobMedia>> {
        match event {
            ExtractionEvent::Media(media) => {
                let scope = self
                    .frames
                    .last()
                    .and_then(|frame| frame.post.as_ref())
                    .map(|post| post.lease())
                    .transpose()?;
                return Ok(Some(JobMedia {
                    media: *media,
                    scope,
                }));
            }
            ExtractionEvent::Post(post) => {
                self.initialize_eager(&post.category).await?;
                let frame = self
                    .frames
                    .last_mut()
                    .ok_or_else(|| error("missing postprocessor job scope"))?;
                frame.end_post().await?;
                let media = fake(*post);
                let mut initialized = false;
                if frame.runtime.is_none()
                    && let Some(configuration) = self.configurations.get(&media.category)
                {
                    let runtime = Runtime {
                        configuration: configuration.clone(),
                        context: public_context(&media),
                        paths: HookPaths {
                            directory: post_directory(configuration, &media)?,
                            filename: None,
                            realpath: None,
                            extension: String::new(),
                        },
                        media: media.clone(),
                        failed: Arc::new(AtomicBool::new(frame.failed)),
                    };
                    frame.runtime = Some(runtime);
                    frame.runtime.as_mut().unwrap().hook(Hook::Init).await?;
                    initialized = true;
                }
                if let Some(runtime) = &mut frame.runtime {
                    if !initialized {
                        runtime.context = public_context(&media);
                    }
                    runtime.paths.directory = post_directory(&runtime.configuration, &media)?;
                    runtime.media = media;
                    runtime.hook(Hook::Post).await?;
                    frame.post = Some(Shared::new(runtime.context.clone(), runtime.failed.clone()));
                }
            }
            ExtractionEvent::EndPost => {
                self.frames
                    .last_mut()
                    .ok_or_else(|| error("missing postprocessor post scope"))?
                    .end_post()
                    .await?
            }
            ExtractionEvent::BeginChild(scope) => {
                if self.frames.len() >= 128
                    || self.frames.iter().any(|frame| {
                        frame
                            .control
                            .as_ref()
                            .is_some_and(|c| c.same_scope(&scope.control))
                    })
                {
                    scope.control.cancel();
                    return Err(error("invalid postprocessor child nesting"));
                }
                let category = scope.metadata.category().to_owned();
                let mut queue = scope.metadata.fields().clone();
                queue.insert("category".into(), scope.metadata.category().into());
                queue.insert("subcategory".into(), scope.metadata.subcategory().into());
                if let Some(runtime) = self.frames.last_mut().and_then(|f| f.runtime.as_mut()) {
                    runtime.context = queue.clone();
                    if let Err(error) = runtime.hook(Hook::Child).await {
                        scope.control.cancel();
                        return Err(error);
                    }
                    queue = runtime.context.clone();
                }
                self.frames.push(Frame {
                    control: Some(scope.control),
                    queue: Some(queue),
                    ..Frame::default()
                });
                self.initialize_eager(&category).await?;
            }
            ExtractionEvent::EndChild(control) => {
                if self.frames.len() <= 1
                    || !self
                        .frames
                        .last()
                        .and_then(|f| f.control.as_ref())
                        .is_some_and(|c| c.same_scope(&control))
                {
                    return Err(error("invalid postprocessor child closure"));
                }
                let mut child = self.frames.pop().unwrap();
                self.errors.extend(child.finalize().await);
                let parent = self.frames.last_mut().unwrap();
                if child.failed
                    || child
                        .runtime
                        .as_ref()
                        .is_some_and(|r| r.failed.load(Ordering::Acquire))
                {
                    parent.mark_failed();
                }
                if let Some(runtime) = &mut parent.runtime {
                    runtime.context = child.queue.take().unwrap_or_default();
                    runtime.hook(Hook::ChildAfter).await?;
                }
            }
        }
        Ok(None)
    }
}
pub fn apply<'a>(
    input: BoxStream<'a, Result<ExtractionEvent>>,
    configurations: Arc<BTreeMap<String, JobConfiguration>>,
) -> BoxStream<'a, Result<JobMedia>> {
    apply_with_category(input, configurations, None)
}
pub fn apply_with_category<'a>(
    input: BoxStream<'a, Result<ExtractionEvent>>,
    configurations: Arc<BTreeMap<String, JobConfiguration>>,
    root_category: Option<String>,
) -> BoxStream<'a, Result<JobMedia>> {
    let state = State {
        input: Some(input),
        configurations,
        frames: vec![Frame::default()],
        errors: VecDeque::new(),
        finished: false,
        started: false,
        root_category,
    };
    futures_util::stream::unfold(state, |mut state| async move {
        let mut skipped = 0u8;
        loop {
            if let Some(error) = state.errors.pop_front() {
                return Some((Err(error), state));
            }
            if state.finished {
                return None;
            }
            if !state.started {
                state.started = true;
                if let Some(category) = state.root_category.take()
                    && let Err(error) = state.initialize_eager(&category).await
                {
                    for frame in &mut state.frames {
                        frame.mark_failed();
                    }
                    state.errors.push_back(error);
                    state.shutdown().await;
                    continue;
                }
            }
            let event = match state.input.as_mut() {
                Some(input) => input.next().await,
                None => None,
            };
            match event {
                None => state.shutdown().await,
                Some(Err(error)) => {
                    for frame in &mut state.frames {
                        frame.mark_failed();
                    }
                    return Some((Err(error), state));
                }
                Some(Ok(event)) => match state.event(event).await {
                    Ok(Some(media)) => return Some((Ok(media), state)),
                    Ok(None) => {}
                    Err(error) => {
                        for frame in &mut state.frames {
                            frame.mark_failed();
                            if let Some(control) = &frame.control {
                                control.cancel();
                            }
                        }
                        state.errors.push_back(error);
                        state.shutdown().await;
                    }
                },
            }
            skipped = skipped.wrapping_add(1);
            if skipped == 0 {
                tokio::task::yield_now().await;
            }
        }
    })
    .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        event::{ChildMetadata, ChildScope},
        extract::PostMetadata,
        postprocess::Specification,
    };
    use serde_json::json;
    use std::time::Duration;
    fn media() -> Media {
        let mut media =
            crate::extract::direct(&url::Url::parse("https://example.org/file.jpg").unwrap())
                .unwrap();
        media.category = "pixiv".into();
        media.metadata = Some(PostMetadata {
            post_id: "42".into(),
            media_id: "42_p0".into(),
            ..Default::default()
        });
        media
    }
    fn post(category: &str) -> ExtractionEvent {
        ExtractionEvent::Post(Box::new(Post {
            category: category.into(),
            metadata: media().metadata,
            count: Some(0),
        }))
    }
    fn configurations(
        root: &Path,
        category: &str,
        values: Vec<Value>,
    ) -> Arc<BTreeMap<String, JobConfiguration>> {
        Arc::new(
            [(
                category.into(),
                JobConfiguration {
                    processors: Arc::new(Postprocessors::new(
                        values
                            .iter()
                            .map(|value| Specification::parse(value, root).unwrap())
                            .collect(),
                    )),
                    root: root.to_owned(),
                    filename: None,
                    directory: None,
                    init: false,
                },
            )]
            .into_iter()
            .collect(),
        )
    }
    fn trace(root: &Path, label: &str, event: &str) -> Value {
        json!({"name":"metadata","mode":"custom","event":event,"base-directory":root,"filename":"trace.log","format":format!("{label}:{event}|"),"open":"a"})
    }
    #[tokio::test]
    async fn boundary_waits_for_every_lease_and_uses_source_order_context_and_actual_last_path() {
        let root = tempfile::tempdir().unwrap();
        let configurations = configurations(
            root.path(),
            "pixiv",
            vec![
                json!({"name":"metadata","mode":"modify","event":"init","fields":{"initialized":"yes"}}),
                json!({"name":"hash","event":"post-after","mode":"sha256"}),
                json!({"name":"metadata","mode":"custom","event":"finalize","base-directory":root.path(),"filename":"trace.log","format":"{initialized}:{winner}:{sha256}"}),
            ],
        );
        let input = futures_util::stream::iter(vec![
            Ok(post("pixiv")),
            Ok(ExtractionEvent::Media(Box::new(media()))),
            Ok(ExtractionEvent::Media(Box::new(media()))),
            Ok(ExtractionEvent::EndPost),
        ])
        .boxed();
        let mut input = apply(input, configurations);
        let first = input.next().await.unwrap().unwrap().scope.unwrap();
        let second = input.next().await.unwrap().unwrap().scope.unwrap();
        assert_eq!(first.keywords()["initialized"], "yes");
        assert!(!first.keywords().contains_key("url"));
        assert!(!first.keywords().contains_key("filename"));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), input.next())
                .await
                .is_err()
        );
        let physical = root.path().join("actual-last.data");
        std::fs::write(&physical, b"abc").unwrap();
        let mut second_context = second.keywords();
        second_context.insert("winner".into(), "last".into());
        second.complete_at(&second_context, &physical);
        drop(second);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), input.next())
                .await
                .is_err()
        );
        let mut first_context = first.keywords();
        first_context.insert("winner".into(), "first".into());
        first.complete(&first_context);
        drop(first);
        assert!(input.next().await.is_none());
        assert_eq!(
            std::fs::read_to_string(root.path().join("trace.log")).unwrap(),
            "yes:last:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
    #[tokio::test]
    async fn empty_posts_and_child_jobs_run_ordered_independent_lifecycle_hooks_once() {
        let root = tempfile::tempdir().unwrap();
        let events = [
            "init",
            "post",
            "post-after",
            "child",
            "child-after",
            "finalize-success",
            "finalize",
        ];
        let mut configurations = BTreeMap::new();
        for category in ["pixiv", "pixiv-novel"] {
            let config = configurations_for_trace(root.path(), category, &events);
            configurations.extend(config.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        let child = ChildScope::new(ChildMetadata::from_raw("pixiv-novel", "novel", &Map::new()));
        let input = futures_util::stream::iter(vec![
            Ok(post("pixiv")),
            Ok(ExtractionEvent::EndPost),
            Ok(ExtractionEvent::BeginChild(child.clone())),
            Ok(post("pixiv-novel")),
            Ok(ExtractionEvent::EndPost),
            Ok(ExtractionEvent::EndChild(child.control)),
            Ok(post("pixiv")),
            Ok(ExtractionEvent::EndPost),
        ])
        .boxed();
        let results = apply(input, Arc::new(configurations))
            .collect::<Vec<_>>()
            .await;
        assert!(results.is_empty());
        assert_eq!(
            std::fs::read_to_string(root.path().join("trace.log")).unwrap(),
            "pixiv:init|pixiv:post|pixiv:post-after|pixiv:child|pixiv-novel:init|pixiv-novel:post|pixiv-novel:post-after|pixiv-novel:finalize-success|pixiv-novel:finalize|pixiv:child-after|pixiv:post|pixiv:post-after|pixiv:finalize-success|pixiv:finalize|"
        );
    }
    fn configurations_for_trace(
        root: &Path,
        category: &str,
        events: &[&str],
    ) -> Arc<BTreeMap<String, JobConfiguration>> {
        configurations(
            root,
            category,
            events
                .iter()
                .map(|event| trace(root, category, event))
                .collect(),
        )
    }
    #[tokio::test]
    async fn extraction_or_download_failure_selects_finalize_error_then_finalize() {
        for extraction_failure in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let configurations = configurations_for_trace(
                root.path(),
                "pixiv",
                &["finalize-success", "finalize-error", "finalize"],
            );
            let mut events = vec![
                Ok(post("pixiv")),
                Ok(ExtractionEvent::Media(Box::new(media()))),
                Ok(ExtractionEvent::EndPost),
            ];
            if extraction_failure {
                events.push(Err(error("expected extraction failure")));
            }
            let mut input = apply(futures_util::stream::iter(events).boxed(), configurations);
            let item = input.next().await.unwrap().unwrap();
            if !extraction_failure {
                item.scope.as_ref().unwrap().fail();
            }
            drop(item);
            let errors = input.collect::<Vec<_>>().await;
            assert_eq!(errors.len(), usize::from(extraction_failure));
            assert_eq!(
                std::fs::read_to_string(root.path().join("trace.log")).unwrap(),
                "pixiv:finalize-error|pixiv:finalize|"
            );
        }
    }
    #[tokio::test]
    async fn hook_failure_finalizes_and_aborts_before_any_file_is_emitted() {
        let root = tempfile::tempdir().unwrap();
        let configurations = configurations(
            root.path(),
            "pixiv",
            vec![
                json!({"name":"hash","event":"init"}),
                trace(root.path(), "pixiv", "finalize-error"),
                trace(root.path(), "pixiv", "finalize"),
            ],
        );
        let input = futures_util::stream::iter(vec![
            Ok(post("pixiv")),
            Ok(ExtractionEvent::Media(Box::new(media()))),
        ])
        .boxed();
        let results = apply(input, configurations).collect::<Vec<_>>().await;
        assert_eq!(results.len(), 1);
        assert!(results[0].is_err());
        assert_eq!(
            std::fs::read_to_string(root.path().join("trace.log")).unwrap(),
            "pixiv:finalize-error|pixiv:finalize|"
        );
    }
    #[tokio::test]
    async fn consumer_drop_cancels_child_and_unconfigured_media_has_no_lease() {
        let child = ChildScope::new(ChildMetadata::from_raw("pixiv", "user", &Map::new()));
        let input = futures_util::stream::iter(vec![
            Ok(ExtractionEvent::BeginChild(child.clone())),
            Ok(post("pixiv")),
            Ok(ExtractionEvent::Media(Box::new(media()))),
        ])
        .boxed();
        let mut input = apply(input, Arc::new(BTreeMap::new()));
        assert!(input.next().await.unwrap().unwrap().scope.is_none());
        drop(input);
        assert!(child.control.is_cancelled());
    }
    #[tokio::test]
    async fn premature_end_drains_child_finalization_and_parent_child_after() {
        let root = tempfile::tempdir().unwrap();
        let events = ["child-after", "finalize-success", "finalize"];
        let mut configs = BTreeMap::new();
        for category in ["pixiv", "pixiv-novel"] {
            configs.extend(
                configurations_for_trace(root.path(), category, &events)
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone())),
            );
        }
        let child = ChildScope::new(ChildMetadata::from_raw("pixiv-novel", "novel", &Map::new()));
        let input = futures_util::stream::iter(vec![
            Ok(post("pixiv")),
            Ok(ExtractionEvent::EndPost),
            Ok(ExtractionEvent::BeginChild(child.clone())),
            Ok(post("pixiv-novel")),
            Ok(ExtractionEvent::EndPost),
        ])
        .boxed();
        assert!(
            apply(input, Arc::new(configs))
                .collect::<Vec<_>>()
                .await
                .is_empty()
        );
        assert!(child.control.is_cancelled());
        assert_eq!(
            std::fs::read_to_string(root.path().join("trace.log")).unwrap(),
            "pixiv-novel:finalize-success|pixiv-novel:finalize|pixiv:child-after|pixiv:finalize-success|pixiv:finalize|"
        );
    }
    #[tokio::test]
    async fn eager_empty_job_runs_init_before_input_and_empty_post_after_then_finalize() {
        let root = tempfile::tempdir().unwrap();
        let mut configurations = (*configurations_for_trace(
            root.path(),
            "pixiv",
            &["init", "post-after", "finalize-success", "finalize"],
        ))
        .clone();
        configurations.get_mut("pixiv").unwrap().init = true;
        let trace = root.path().join("trace.log");
        let input = futures_util::stream::once(async move {
            assert_eq!(std::fs::read_to_string(trace).unwrap(), "pixiv:init|");
            None::<Result<ExtractionEvent>>
        })
        .filter_map(std::future::ready)
        .boxed();
        assert!(
            apply_with_category(input, Arc::new(configurations), Some("pixiv".into()))
                .collect::<Vec<_>>()
                .await
                .is_empty()
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("trace.log")).unwrap(),
            "pixiv:init|pixiv:post-after|pixiv:finalize-success|pixiv:finalize|"
        );
    }
    #[tokio::test]
    async fn eager_init_uses_empty_context_while_lazy_init_mutations_belong_to_first_post() {
        for eager in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let mut configurations=(*configurations(root.path(),"pixiv",vec![
                json!({"name":"metadata","mode":"modify","event":"init","fields":{"init_only":"yes","observed_post":"{post_id}"}}),
            ])).clone();
            configurations.get_mut("pixiv").unwrap().init = eager;
            let input = futures_util::stream::iter(vec![
                Ok(post("pixiv")),
                Ok(ExtractionEvent::Media(Box::new(media()))),
                Ok(ExtractionEvent::EndPost),
            ])
            .boxed();
            let mut input =
                apply_with_category(input, Arc::new(configurations), Some("pixiv".into()));
            let item = input.next().await.unwrap().unwrap();
            let context = item.scope.as_ref().unwrap().keywords();
            assert_eq!(context.contains_key("init_only"), !eager);
            assert_eq!(context.contains_key("observed_post"), !eager);
            drop(item);
            assert!(input.next().await.is_none());
        }
    }
    #[tokio::test]
    async fn eager_child_initializes_and_finalizes_even_without_child_posts() {
        let root = tempfile::tempdir().unwrap();
        let mut configurations = (*configurations_for_trace(
            root.path(),
            "pixiv-novel",
            &["init", "post-after", "finalize-success", "finalize"],
        ))
        .clone();
        configurations.get_mut("pixiv-novel").unwrap().init = true;
        let child = ChildScope::new(ChildMetadata::from_raw("pixiv-novel", "novel", &Map::new()));
        let input = futures_util::stream::iter(vec![
            Ok(ExtractionEvent::BeginChild(child.clone())),
            Ok(ExtractionEvent::EndChild(child.control)),
        ])
        .boxed();
        assert!(
            apply(input, Arc::new(configurations))
                .collect::<Vec<_>>()
                .await
                .is_empty()
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("trace.log")).unwrap(),
            "pixiv-novel:init|pixiv-novel:post-after|pixiv-novel:finalize-success|pixiv-novel:finalize|"
        );
    }
}
