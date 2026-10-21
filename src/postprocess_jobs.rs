//! Ordered post/job hooks around independently concurrent file downloads.
use crate::{
    Result, error,
    event::{ChildControl, ExtractionEvent, KeywordPolicy, KeywordUpdate, Post},
    extract::{Media, MediaJob},
    naming::{DirectoryTemplate, FilenameTemplate},
    postprocess::{Hook, HookPaths, Postprocessors},
};
use futures_util::{StreamExt, stream::BoxStream};
use serde_json::{Map, Value};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
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
    resume_failed: bool,
    pending: usize,
    next: u64,
    initial: Map<String, Value>,
    original: Map<String, Value>,
    policy: KeywordPolicy,
    active: BTreeSet<u64>,
    last: Option<(u64, Map<String, Value>, Option<PathBuf>)>,
}
struct Shared {
    inner: Mutex<LeaseState>,
    notify: Notify,
    failed: Arc<AtomicBool>,
    processors: Mutex<Option<Arc<Postprocessors>>>,
    objects: crate::postprocess::objects::Bindings,
    directory: Mutex<Option<PathBuf>>,
    source_extension: Mutex<Option<Value>>,
    source_path: Arc<Mutex<HookPaths>>,
    halt: Mutex<Option<Arc<AtomicBool>>>,
}
/// One emitted file's participation in its owning post. Drop on every outcome,
/// including filtering, deduplication, cancellation and failures.
pub struct ScopeLease {
    shared: Arc<Shared>,
    ordinal: u64,
    resume: Mutex<Vec<KeywordUpdate>>,
}
impl std::fmt::Debug for ScopeLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ScopeLease { fields: [redacted] }")
    }
}
impl ScopeLease {
    pub(crate) fn source_paths(&self) -> HookPaths {
        self.shared
            .source_path
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    pub(crate) fn source_begin(&self, paths: &HookPaths) {
        let mut state = self
            .shared
            .source_path
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        state.directory = paths.directory.clone();
        state.extension = paths.extension.clone();
        state.source_extension = paths.source_extension.clone();
        state.filename = None;
        state.temppath = None;
    }
    pub(crate) fn source_built_path(&self, path: &Path) {
        let mut state = self
            .shared
            .source_path
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        state.filename = path.file_name().map(|name| name.to_owned());
        state.realpath = Some(path.to_owned());
        state.temppath.get_or_insert_with(|| path.to_owned());
    }
    pub(crate) fn source_input(&self, path: &Path) {
        self.shared
            .source_path
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .temppath = Some(path.to_owned());
    }
    pub(crate) fn set_source_extension(&self, extension: Option<Value>) {
        *self
            .shared
            .source_extension
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = extension;
    }
    pub(crate) fn directory(&self) -> Option<PathBuf> {
        self.shared
            .directory
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    pub(crate) fn objects(&self) -> crate::postprocess::objects::Bindings {
        self.shared.objects.clone()
    }
    pub(crate) fn processors(&self) -> Option<Arc<Postprocessors>> {
        self.shared
            .processors
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    /// A thrown processor callback aborts its extractor job; download failures
    /// instead only call `fail`, allowing the extractor to continue.
    pub(crate) fn fail_hook(&self) {
        self.fail();
        if let Some(halt) = &*self.shared.halt.lock().unwrap_or_else(|e| e.into_inner()) {
            halt.store(true, Ordering::Release);
        }
    }
    fn halted(&self) -> bool {
        self.shared
            .halt
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|halt| halt.load(Ordering::Acquire))
    }
    /// Snapshot of post-hook keywords, before any file hooks. Use `inherit` to
    /// construct a file context with the producer's ownership semantics.
    pub fn keywords(&self) -> Map<String, Value> {
        self.shared
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .initial
            .clone()
    }
    /// Shared dictionaries observe every preceding file's completed mutations.
    /// Register notifications before checking the turn to avoid cancellation races.
    pub async fn inherit(&self, context: &mut Map<String, Value>) {
        loop {
            let notified = self.shared.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut state = self.shared.inner.lock().unwrap_or_else(|e| e.into_inner());
                let shared = matches!(state.policy, KeywordPolicy::Shared { .. });
                if !shared || state.active.first() == Some(&self.ordinal) {
                    let latest = if shared {
                        state
                            .last
                            .as_ref()
                            .map(|(_, context, _)| context)
                            .unwrap_or(&state.initial)
                    } else {
                        &state.initial
                    };
                    inherit_keywords(&state.policy, &state.original, latest, context);
                    self.shared.objects.reapply_artist();
                    self.shared.objects.refresh(context);
                    if shared {
                        // Remember fields introduced by this emission as well, so
                        // deleting a file-only field survives into the next file.
                        for key in context.keys() {
                            state.original.entry(key.clone()).or_insert(Value::Null);
                        }
                    }
                    return;
                }
            }
            notified.await;
        }
    }
    pub async fn inherit_for(&self, media: &Media, context: &mut Map<String, Value>) -> Result<()> {
        self.inherit(context).await;
        if self.halted() {
            return Err(error("postprocessor job stopped"));
        }
        if self.shared.has_resume_error() {
            return Err(error("producer continuation failed"));
        }
        let updates = media
            .metadata
            .as_ref()
            .and_then(|m| m.keyword_updates.as_deref())
            .unwrap_or_default();
        let result = crate::postprocess::keywords::apply(context, updates);
        self.complete(context);
        if result.is_err() {
            self.shared
                .inner
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .resume_failed = true;
            self.fail();
        }
        if result.is_ok() {
            let mut resume = self.resume.lock().unwrap_or_else(|e| e.into_inner());
            for update in updates {
                if let KeywordUpdate::OnResume(updates) = update {
                    resume.extend(updates.iter().cloned());
                }
            }
        }
        result
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
        let resume = self.resume.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((ordinal, context, _)) = &mut state.last
            && *ordinal == self.ordinal
            && !self.halted()
            && crate::postprocess::keywords::apply(context, &resume).is_err()
        {
            state.resume_failed = true;
            self.shared.failed.store(true, Ordering::Release);
        }
        state.pending -= 1;
        state.active.remove(&self.ordinal);
        self.shared.notify.notify_waiters();
    }
}
impl Shared {
    fn new(
        original: Map<String, Value>,
        context: Map<String, Value>,
        policy: KeywordPolicy,
        failed: Arc<AtomicBool>,
    ) -> Arc<Self> {
        let objects = crate::postprocess::objects::Bindings::new(&policy);
        Self::new_with_objects(original, context, policy, failed, objects)
    }
    fn new_with_objects(
        original: Map<String, Value>,
        context: Map<String, Value>,
        policy: KeywordPolicy,
        failed: Arc<AtomicBool>,
        objects: crate::postprocess::objects::Bindings,
    ) -> Arc<Self> {
        Self::with_source_path(original, context, policy, failed, objects, Arc::default())
    }
    fn with_source_path(
        original: Map<String, Value>,
        mut context: Map<String, Value>,
        policy: KeywordPolicy,
        failed: Arc<AtomicBool>,
        objects: crate::postprocess::objects::Bindings,
        source_path: Arc<Mutex<HookPaths>>,
    ) -> Arc<Self> {
        if let KeywordPolicy::CopiedAfterPost { removed_fields, .. } = &policy {
            for key in removed_fields.iter() {
                context.remove(key);
            }
        }
        Arc::new(Self {
            inner: Mutex::new(LeaseState {
                resume_failed: false,
                pending: 0,
                next: 0,
                initial: context,
                original,
                policy,
                active: BTreeSet::new(),
                last: None,
            }),
            notify: Notify::new(),
            failed,
            processors: Mutex::new(None),
            objects,
            directory: Mutex::new(None),
            source_extension: Mutex::new(None),
            source_path,
            halt: Mutex::new(None),
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
        state.active.insert(next);
        Ok(ScopeLease {
            shared: self.clone(),
            ordinal: state.next,
            resume: Mutex::new(Vec::new()),
        })
    }
    fn has_resume_error(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .resume_failed
    }
    fn checkpoint(&self, context: &Map<String, Value>) {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, latest, _)) = &mut state.last {
            *latest = context.clone();
        } else {
            state.initial = context.clone();
        }
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
                    let mut latest = state.last.as_ref().map_or_else(
                        || (state.initial.clone(), None),
                        |(_, ctx, path)| (ctx.clone(), path.clone()),
                    );
                    self.objects.refresh(&mut latest.0);
                    if let KeywordPolicy::Shared {
                        feedback: Some(feedback),
                        ..
                    } = &state.policy
                    {
                        feedback.capture(&latest.0);
                    }
                    return latest;
                }
            }
            notified.await;
        }
    }
}
fn inherit_keywords(
    policy: &KeywordPolicy,
    original: &Map<String, Value>,
    latest: &Map<String, Value>,
    context: &mut Map<String, Value>,
) {
    let fields = match policy {
        KeywordPolicy::Independent => return,
        KeywordPolicy::Shared { file_fields, .. }
        | KeywordPolicy::CopiedAfterPost { file_fields, .. } => file_fields,
    };
    let file = std::mem::replace(context, latest.clone());
    let removed = if let KeywordPolicy::CopiedAfterPost { removed_fields, .. } = policy {
        for key in removed_fields.iter() {
            context.remove(key);
        }
        removed_fields.as_ref()
    } else {
        &[]
    };
    for (key, value) in file {
        // The producer, rather than the postprocessor adapter, declares which
        // existing fields are assigned anew for each emitted file.
        if (!original.contains_key(&key) && !context.contains_key(&key))
            || fields.contains(&key)
            || removed.contains(&key)
        {
            context.insert(key, value);
        }
    }
}
struct Runtime {
    configuration: JobConfiguration,
    media: Media,
    context: Map<String, Value>,
    paths: HookPaths,
    source_path: Arc<Mutex<HookPaths>>,
    failed: Arc<AtomicBool>,
    halt: Arc<AtomicBool>,
    objects: crate::postprocess::objects::Bindings,
}
impl Runtime {
    async fn hook(&mut self, hook: Hook) -> Result<()> {
        self.hook_observed(hook, self.objects.clone()).await
    }
    async fn hook_independent(&mut self, hook: Hook) -> Result<()> {
        self.hook_observed(hook, Default::default()).await
    }
    async fn hook_observed(
        &mut self,
        hook: Hook,
        objects: crate::postprocess::objects::Bindings,
    ) -> Result<()> {
        let source_paths = self
            .media
            .metadata
            .as_ref()
            .is_some_and(|metadata| metadata.source_defaults.is_some());
        if source_paths {
            let cached = self.source_path.lock().unwrap_or_else(|e| e.into_inner());
            self.paths.filename = cached.filename.clone();
            self.paths.temppath = cached.temppath.clone();
            self.paths.realpath = cached.realpath.clone();
            self.paths.extension = cached.extension.clone();
            self.paths.source_extension = cached.source_extension.clone();
        }
        objects.refresh(&mut self.context);
        let filename =
            crate::naming::effective_filename(&self.media, self.configuration.filename.as_ref());
        let result = objects
            .observe(self.configuration.processors.run_job_hook(
                hook,
                &mut self.context,
                &self.media,
                &mut self.paths,
                &self.configuration.root,
                filename.as_ref(),
            ))
            .await;
        objects.sync_context(&self.context);
        if source_paths {
            *self.source_path.lock().unwrap_or_else(|e| e.into_inner()) = self.paths.clone();
        }
        if result.is_err() {
            self.failed.store(true, Ordering::Release);
            self.halt.store(true, Ordering::Release);
        }
        result
    }
}
#[derive(Default)]
struct Frame {
    control: Option<ChildControl>,
    runtime: Option<Runtime>,
    post: Option<Arc<Shared>>,
    queue: Option<Map<String, Value>>,
    queue_shared: bool,
    eager_only: bool,
    artist_queue: Option<crate::event::KeywordObject>,
    source_class: Option<String>,
    archive_descriptor: Option<crate::source_archive::ArchiveDescriptor>,
    infer_archive_descriptor: bool,
    post_after_pending: bool,
    halt: bool,
    failed: bool,
}
impl Frame {
    fn halted(&self) -> bool {
        self.halt
            || self
                .runtime
                .as_ref()
                .is_some_and(|runtime| runtime.halt.load(Ordering::Acquire))
    }
    fn mark_failed(&mut self) {
        self.failed = true;
        if let Some(runtime) = &self.runtime {
            runtime.failed.store(true, Ordering::Release);
        }
    }
    async fn sync_post(&mut self) -> Result<()> {
        if self.eager_only {
            return Ok(());
        }
        if let Some(post) = &self.post {
            let (context, path) = post.finish().await;
            if let Some(runtime) = &mut self.runtime {
                runtime.context = context;
                let source_paths = runtime
                    .media
                    .metadata
                    .as_ref()
                    .is_some_and(|metadata| metadata.source_defaults.is_some());
                if source_paths {
                    runtime.paths = post
                        .source_path
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone();
                }
                if let Some(path) = path.filter(|_| !source_paths) {
                    runtime.paths.source_extension = post
                        .source_extension
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone();
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
            }
            if post.has_resume_error() {
                return Err(error("producer continuation failed"));
            }
        }
        Ok(())
    }
    async fn end_post(&mut self) -> Result<()> {
        let result = self.sync_post().await;
        self.eager_only = false;
        let pending = std::mem::take(&mut self.post_after_pending);
        if self.post.take().is_some() || pending {
            if result.is_err() {
                self.mark_failed();
            }
            if let Some(runtime) = &mut self.runtime {
                runtime.hook(Hook::PostAfter).await?;
            }
        }
        result
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
            errors.extend(runtime.configuration.processors.close_archives().await);
        }
        errors
    }
}
struct State<'a> {
    input: Option<BoxStream<'a, Result<ExtractionEvent>>>,
    configurations: Arc<BTreeMap<String, JobConfiguration>>,
    frames: Vec<Frame>,
    unopened_children: Vec<ChildControl>,
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
fn post_directory(
    configuration: &JobConfiguration,
    media: &Media,
    keywords: &Map<String, Value>,
) -> Result<PathBuf> {
    crate::download::directory_with_keywords(
        &configuration.root,
        media,
        configuration.directory.as_ref(),
        keywords,
    )
}
fn public_context(media: &Media) -> Map<String, Value> {
    let mut context = crate::postprocess::context(media);
    for key in ["url", "_url", "filename", "name", "extension"] {
        context.remove(key);
    }
    if let Some(inheritance) = media
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.booru_artist_parent.as_ref())
    {
        context.extend(inheritance.keyword_values());
    }
    context
}
impl State<'_> {
    fn configuration(&self, category: &str) -> Option<&JobConfiguration> {
        self.frames
            .last()
            .and_then(|frame| frame.source_class.as_ref())
            .and_then(|class| self.configurations.get(class))
            .or_else(|| self.configurations.get(category))
    }
    async fn initialize_eager(&mut self, category: &str) -> Result<()> {
        let Some(configuration) = self
            .configuration(category)
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
        let mut configuration = configuration;
        configuration.processors = Arc::new(configuration.processors.for_job());
        frame.runtime = Some(Runtime {
            configuration,
            media: fake(Post {
                keywords: Default::default(),
                category: category.into(),
                metadata: None,
                count: None,
            }),
            context: Map::new(),
            paths: HookPaths::default(),
            source_path: Arc::default(),
            failed: Arc::new(AtomicBool::new(frame.failed)),
            halt: Arc::new(AtomicBool::new(false)),
            objects: Default::default(),
        });
        let runtime = frame.runtime.as_mut().unwrap();
        let descriptor = frame.archive_descriptor.clone().or_else(|| {
            frame
                .infer_archive_descriptor
                .then(|| crate::source_archive::for_media(&runtime.media))
                .flatten()
        });
        runtime
            .configuration
            .processors
            .initialize_archives(
                descriptor.as_ref(),
                &runtime.media,
                &runtime.context,
                &runtime.configuration.root,
                &runtime.paths.directory,
            )
            .await;
        let result = runtime.hook(Hook::Init).await;
        frame.eager_only = true;
        frame.post = Some(Shared::new(
            Map::new(),
            runtime.context.clone(),
            KeywordPolicy::Independent,
            runtime.failed.clone(),
        ));
        *frame
            .post
            .as_ref()
            .unwrap()
            .processors
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(runtime.configuration.processors.clone());
        *frame
            .post
            .as_ref()
            .unwrap()
            .halt
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(runtime.halt.clone());
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
                    runtime.context = frame
                        .artist_queue
                        .as_ref()
                        .map(crate::event::KeywordObject::snapshot)
                        .unwrap_or_else(|| frame.queue.take().unwrap_or_default());
                    let result = if frame.artist_queue.is_some() || frame.queue_shared {
                        runtime.hook(Hook::ChildAfter).await
                    } else {
                        runtime.hook_independent(Hook::ChildAfter).await
                    };
                    if frame.queue_shared
                        && let Some(post) = &parent.post
                    {
                        post.checkpoint(&runtime.context);
                    }
                    if let Err(error) = result {
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
                if post
                    .metadata
                    .as_ref()
                    .is_some_and(|metadata| metadata.booru_artist_parent.is_some())
                    && let Some(frame) = self.frames.last_mut()
                    && frame.source_class.is_none()
                {
                    frame.source_class = Some(format!("{}.tag", post.category));
                }
                self.initialize_eager(&post.category).await?;
                let configuration = self.configuration(&post.category).cloned();
                let frame = self
                    .frames
                    .last_mut()
                    .ok_or_else(|| error("missing postprocessor job scope"))?;
                frame.end_post().await?;
                frame.post_after_pending = true;
                let policy = post.keywords.clone();
                let count = post.count;
                let media = fake(*post);
                let mut original = public_context(&media);
                if let Some(count) = count {
                    original.insert("count".into(), count.into());
                }
                if let KeywordPolicy::Shared { post_fields, .. } = &policy {
                    original.extend(
                        post_fields
                            .iter()
                            .map(|(key, value)| (key.clone(), value.clone())),
                    );
                }
                let objects = crate::postprocess::objects::Bindings::for_artist(
                    &policy,
                    media
                        .metadata
                        .as_ref()
                        .and_then(|metadata| metadata.booru_artist_parent.as_ref()),
                );
                objects.refresh(&mut original);
                let mut initialized = false;
                if frame.runtime.is_none()
                    && let Some(mut configuration) = configuration
                {
                    configuration.processors = Arc::new(configuration.processors.for_job());
                    let runtime = Runtime {
                        configuration: configuration.clone(),
                        context: original.clone(),
                        paths: HookPaths {
                            directory: post_directory(&configuration, &media, &original)?,
                            filename: None,
                            realpath: None,
                            temppath: None,
                            extension: String::new(),
                            source_extension: None,
                        },
                        source_path: Arc::default(),
                        media: media.clone(),
                        failed: Arc::new(AtomicBool::new(frame.failed)),
                        halt: Arc::new(AtomicBool::new(false)),
                        objects: objects.clone(),
                    };
                    frame.runtime = Some(runtime);
                    let runtime = frame.runtime.as_mut().unwrap();
                    let descriptor = frame.archive_descriptor.clone().or_else(|| {
                        frame
                            .infer_archive_descriptor
                            .then(|| crate::source_archive::for_media(&media))
                            .flatten()
                    });
                    runtime
                        .configuration
                        .processors
                        .initialize_archives(
                            descriptor.as_ref(),
                            &runtime.media,
                            &runtime.context,
                            &runtime.configuration.root,
                            &runtime.paths.directory,
                        )
                        .await;
                    runtime.hook(Hook::Init).await?;
                    initialized = true;
                }
                if let Some(runtime) = &mut frame.runtime {
                    runtime.objects = objects;
                    if !initialized {
                        runtime.context = original.clone();
                    }
                    if !initialized {
                        runtime.paths.directory =
                            post_directory(&runtime.configuration, &media, &original)?;
                    }
                    runtime.media = media;
                    runtime.hook(Hook::Post).await?;
                    frame.post = Some(Shared::with_source_path(
                        original,
                        runtime.context.clone(),
                        policy,
                        runtime.failed.clone(),
                        runtime.objects.clone(),
                        runtime.source_path.clone(),
                    ));
                    *frame
                        .post
                        .as_ref()
                        .unwrap()
                        .directory
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) = runtime
                        .configuration
                        .directory
                        .as_ref()
                        .map(|_| runtime.paths.directory.clone())
                        .or_else(|| {
                            runtime
                                .media
                                .metadata
                                .as_ref()
                                .and_then(|metadata| metadata.source_defaults.as_ref())
                                .map(|_| runtime.paths.directory.clone())
                        });
                    *frame
                        .post
                        .as_ref()
                        .unwrap()
                        .processors
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) =
                        Some(runtime.configuration.processors.clone());
                    *frame
                        .post
                        .as_ref()
                        .unwrap()
                        .halt
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) = Some(runtime.halt.clone());
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
                let category = scope.metadata.target_category().to_owned();
                let archive_descriptor = crate::source_archive::for_child(&scope.metadata);
                let live = !scope.metadata.keyword_updates().is_empty();
                let artist_queue = scope.metadata.booru_artist_queue().cloned();
                let mut queue = scope.metadata.fields().clone();
                queue.insert("category".into(), scope.metadata.category().into());
                queue.insert("subcategory".into(), scope.metadata.subcategory().into());
                if let Some(object) = &artist_queue {
                    object.replace(&queue);
                }
                let parent = self
                    .frames
                    .last_mut()
                    .ok_or_else(|| error("missing parent scope"))?;
                if live && let Err(error) = parent.sync_post().await {
                    scope.control.cancel();
                    return Err(error);
                }
                if let Some(runtime) = &mut parent.runtime {
                    if live {
                        if let Err(error) = crate::postprocess::keywords::apply(
                            &mut runtime.context,
                            scope.metadata.keyword_updates(),
                        ) {
                            if let Some(post) = &parent.post {
                                post.checkpoint(&runtime.context);
                            }
                            scope.control.cancel();
                            return Err(error);
                        }
                    } else {
                        runtime.context = queue.clone();
                    }
                    let result = if let Some(object) = &artist_queue {
                        runtime.objects =
                            crate::postprocess::objects::Bindings::artist_queue(object.clone());
                        runtime.hook(Hook::Child).await
                    } else if live {
                        runtime.hook(Hook::Child).await
                    } else {
                        runtime.hook_independent(Hook::Child).await
                    };
                    if let Err(error) = result {
                        if live && let Some(post) = &parent.post {
                            post.checkpoint(&runtime.context);
                        }
                        self.unopened_children.push(scope.control.clone());
                        scope.control.cancel();
                        return Err(error);
                    }
                    queue = runtime.context.clone();
                    if live && let Some(post) = &parent.post {
                        post.checkpoint(&queue);
                    }
                }
                if let Some(object) = &artist_queue {
                    object.sync(&queue);
                    scope
                        .metadata
                        .booru_artist_parent()
                        .expect("artist parent")
                        .object()
                        .reset_shallow_from(object);
                }
                self.frames.push(Frame {
                    source_class: scope
                        .metadata
                        .booru_artist_url()
                        .map(|_| format!("{}.tag", scope.metadata.target_category())),
                    artist_queue,
                    control: Some(scope.control),
                    queue: Some(queue),
                    queue_shared: live,
                    archive_descriptor,
                    ..Frame::default()
                });
                self.initialize_eager(&category).await?;
            }
            ExtractionEvent::EndChild(control) => {
                if let Some(index) = self
                    .unopened_children
                    .iter()
                    .position(|child| child.same_scope(&control))
                {
                    self.unopened_children.remove(index);
                    return Ok(None);
                }
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
                let mut errors = child.finalize().await.into_iter();
                if let Some(error) = errors.next() {
                    self.errors.extend(errors);
                    // An exception escaping child finalization belongs to the
                    // parent's queue callback, unlike a child dispatch error.
                    let parent = self.frames.last_mut().unwrap();
                    parent.mark_failed();
                    parent.halt = true;
                    if let Some(runtime) = &parent.runtime {
                        runtime.halt.store(true, Ordering::Release);
                    }
                    return Err(error);
                }
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
                    runtime.context = child
                        .artist_queue
                        .as_ref()
                        .map(crate::event::KeywordObject::snapshot)
                        .unwrap_or_else(|| child.queue.take().unwrap_or_default());
                    let result = if child.artist_queue.is_some() || child.queue_shared {
                        runtime.hook(Hook::ChildAfter).await
                    } else {
                        runtime.hook_independent(Hook::ChildAfter).await
                    };
                    if child.queue_shared
                        && let Some(post) = &parent.post
                    {
                        post.checkpoint(&runtime.context);
                    }
                    result?;
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
    apply_inner(input, configurations, root_category, None, None, true)
}
/// Apply job hooks with the selected source extractor's archive identity format.
pub fn apply_with_descriptor<'a>(
    input: BoxStream<'a, Result<ExtractionEvent>>,
    configurations: Arc<BTreeMap<String, JobConfiguration>>,
    root_category: Option<String>,
    archive_descriptor: Option<crate::source_archive::ArchiveDescriptor>,
) -> BoxStream<'a, Result<JobMedia>> {
    apply_inner(
        input,
        configurations,
        root_category,
        None,
        archive_descriptor,
        false,
    )
}
/// Resolve job configuration using its immutable source extractor class.
pub fn apply_with_source_class<'a>(
    input: BoxStream<'a, Result<ExtractionEvent>>,
    configurations: Arc<BTreeMap<String, JobConfiguration>>,
    root_category: Option<String>,
    root_subcategory: Option<String>,
    archive_descriptor: Option<crate::source_archive::ArchiveDescriptor>,
) -> BoxStream<'a, Result<JobMedia>> {
    apply_inner(
        input,
        configurations,
        root_category,
        root_subcategory,
        archive_descriptor,
        false,
    )
}
fn apply_inner<'a>(
    input: BoxStream<'a, Result<ExtractionEvent>>,
    configurations: Arc<BTreeMap<String, JobConfiguration>>,
    root_category: Option<String>,
    root_subcategory: Option<String>,
    archive_descriptor: Option<crate::source_archive::ArchiveDescriptor>,
    infer_archive_descriptor: bool,
) -> BoxStream<'a, Result<JobMedia>> {
    let state = State {
        input: Some(input),
        configurations,
        frames: vec![Frame {
            source_class: root_category
                .as_ref()
                .zip(root_subcategory.as_ref())
                .map(|(category, subcategory)| format!("{category}.{subcategory}")),
            archive_descriptor,
            infer_archive_descriptor,
            ..Frame::default()
        }],
        unopened_children: Vec::new(),
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
            if let Some(frame) = state.frames.last()
                && frame.halted()
            {
                if let Some(control) = &frame.control {
                    control.cancel();
                } else {
                    state.shutdown().await;
                    continue;
                }
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
                        state.errors.push_back(error);
                        // A callback exception aborts its owning job. The
                        // balanced child wrapper emits EndChild after cancel,
                        // allowing the parent to resume and observe status.
                        if let Some(frame) = state.frames.last_mut()
                            && frame.halted()
                        {
                            frame.mark_failed();
                            if let Some(control) = &frame.control {
                                control.cancel();
                                continue;
                            }
                        }
                        for frame in &mut state.frames {
                            frame.mark_failed();
                            if let Some(control) = &frame.control {
                                control.cancel();
                            }
                        }
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
            keywords: Default::default(),
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

#[cfg(test)]
#[path = "postprocess_keywords_oracle.rs"]
mod keywords_oracle;

#[cfg(test)]
#[path = "postprocess_child_failure_oracle.rs"]
mod child_failure_oracle;

#[cfg(test)]
#[path = "postprocess_hook_failure_tests.rs"]
mod hook_failure_tests;

#[cfg(test)]
mod keyword_order_tests {
    use super::*;
    fn media() -> Media {
        let mut media =
            crate::extract::direct(&url::Url::parse("https://example.org/file.jpg").unwrap())
                .unwrap();
        media.metadata = Some(crate::extract::PostMetadata::default());
        media
    }
    use serde_json::json;

    #[tokio::test]
    async fn shared_files_wait_for_prior_mutations_and_cancelled_leases_do_not_block() {
        let original = json!({"caption":"original","num":0})
            .as_object()
            .unwrap()
            .clone();
        let mut initial = original.clone();
        initial.remove("caption");
        initial.insert("counter".into(), 0.into());
        let shared = Shared::new(
            original,
            initial,
            KeywordPolicy::shared(&["num"]),
            Arc::new(AtomicBool::new(false)),
        );
        let first = shared.lease().unwrap();
        let second = shared.lease().unwrap();
        let cancelled = shared.lease().unwrap();
        let fourth = shared.lease().unwrap();
        drop(cancelled);
        let mut two = json!({"caption":"original","num":2,"filename":"same.jpg"})
            .as_object()
            .unwrap()
            .clone();
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                second.inherit(&mut two)
            )
            .await
            .is_err()
        );
        let mut one = json!({"caption":"original","num":1,"filename":"same.jpg"})
            .as_object()
            .unwrap()
            .clone();
        first.inherit(&mut one).await;
        assert!(!one.contains_key("caption"));
        assert_eq!(one["num"], 1);
        one.insert("counter".into(), 1.into());
        one.remove("filename");
        first.complete(&one);
        drop(first);
        second.inherit(&mut two).await;
        assert_eq!(two["counter"], 1);
        assert_eq!(two["num"], 2);
        assert!(
            !two.contains_key("filename"),
            "a deleted file-only field must stay deleted unless the producer resets it"
        );
        second.complete(&two);
        drop(second);
        let mut four = json!({"caption":"original","num":4,"filename":"same.jpg"})
            .as_object()
            .unwrap()
            .clone();
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            fourth.inherit(&mut four),
        )
        .await
        .unwrap();
        assert_eq!(four["counter"], 1);
        drop(fourth);
        assert_eq!(shared.inner.lock().unwrap().active.len(), 0);
    }

    #[tokio::test]
    async fn copied_post_precedence_and_removed_fields_leave_prepared_file_values_intact() {
        let original = json!({"caption":"original","num":0,"source_id":1})
            .as_object()
            .unwrap()
            .clone();
        let latest = json!({"caption":"post","num":99,"source_id":2,"filename":"post-name"})
            .as_object()
            .unwrap()
            .clone();
        for file_wins in [false, true] {
            let fields = if file_wins {
                vec!["num", "filename"]
            } else {
                vec!["num"]
            };
            let policy =
                KeywordPolicy::copied_after_post(&fields).without_post_fields(&["source_id"]);
            let mut file =
                json!({"caption":"original","num":1,"source_id":3,"filename":"prepared-name"})
                    .as_object()
                    .unwrap()
                    .clone();
            inherit_keywords(&policy, &original, &latest, &mut file);
            let empty = Shared::new(
                original.clone(),
                latest.clone(),
                policy,
                Arc::new(AtomicBool::new(false)),
            );
            let (closed, _) = empty.finish().await;
            assert!(
                !closed.contains_key("source_id"),
                "producer removals also apply when no files are emitted"
            );
            assert_eq!(file,json!({"caption":"post","num":1,"source_id":3,"filename":if file_wins {"prepared-name"} else {"post-name"}}).as_object().unwrap().clone());
        }
    }

    #[tokio::test]
    async fn independent_and_copied_files_do_not_wait_or_inherit_previous_file_changes() {
        for policy in [
            KeywordPolicy::Independent,
            KeywordPolicy::copied_after_post(&["num"]),
        ] {
            let original = json!({"caption":"original","num":0})
                .as_object()
                .unwrap()
                .clone();
            let initial = json!({"counter":0,"num":0}).as_object().unwrap().clone();
            let copied = matches!(policy, KeywordPolicy::CopiedAfterPost { .. });
            let shared = Shared::new(original, initial, policy, Arc::new(AtomicBool::new(false)));
            let first = shared.lease().unwrap();
            let second = shared.lease().unwrap();
            first.complete(json!({"counter":99}).as_object().unwrap());
            let mut context = json!({"caption":"original","num":2})
                .as_object()
                .unwrap()
                .clone();
            tokio::time::timeout(
                std::time::Duration::from_millis(100),
                second.inherit(&mut context),
            )
            .await
            .unwrap();
            assert_eq!(context.get("counter"), copied.then_some(&json!(0)));
            assert_eq!(context.contains_key("caption"), !copied);
            assert_eq!(context["num"], 2);
        }
    }
    #[tokio::test]
    async fn producer_operations_observe_hooks_and_resume_before_next_emission() {
        let original = json!({"num":0,"suffix":""}).as_object().unwrap().clone();
        let initial = json!({"num":42,"suffix":"hook"})
            .as_object()
            .unwrap()
            .clone();
        let shared = Shared::new(
            original,
            initial,
            KeywordPolicy::shared(&["index"]),
            Arc::new(AtomicBool::new(false)),
        );
        let first = shared.lease().unwrap();
        let second = shared.lease().unwrap();
        let mut file = media();
        file.metadata.as_mut().unwrap().keyword_updates = Some(
            vec![
                KeywordUpdate::Increment("num".into()),
                KeywordUpdate::Format {
                    name: "suffix".into(),
                    template: "_p{num:02}".into(),
                },
                KeywordUpdate::Set {
                    name: "_fallback".into(),
                    value: json!(["cover.png"]),
                },
                KeywordUpdate::OnResume(vec![KeywordUpdate::RemoveRequired("_fallback".into())]),
            ]
            .into(),
        );
        let mut context = crate::postprocess::context(&file);
        first.inherit_for(&file, &mut context).await.unwrap();
        assert_eq!(context["num"], 43);
        assert_eq!(context["suffix"], "_p43");
        context.insert("num".into(), json!(900));
        first.complete(&context);
        drop(first);
        let mut next = media();
        next.metadata.as_mut().unwrap().keyword_updates =
            Some(vec![KeywordUpdate::Increment("num".into())].into());
        let mut context = crate::postprocess::context(&next);
        second.inherit_for(&next, &mut context).await.unwrap();
        assert_eq!(context["num"], 901);
        assert!(!context.contains_key("_fallback"));
        drop(second);
        assert_eq!(shared.finish().await.0["num"], 901);
    }
    #[tokio::test]
    async fn required_resume_deletion_failure_prevents_later_writes() {
        let shared = Shared::new(
            Map::new(),
            json!({"num":1}).as_object().unwrap().clone(),
            KeywordPolicy::shared(&[]),
            Arc::new(AtomicBool::new(false)),
        );
        let first = shared.lease().unwrap();
        let mut file = media();
        file.metadata.as_mut().unwrap().keyword_updates = Some(
            vec![KeywordUpdate::OnResume(vec![
                KeywordUpdate::RemoveRequired("_fallback".into()),
            ])]
            .into(),
        );
        first.inherit_for(&file, &mut Map::new()).await.unwrap();
        drop(first);
        assert!(shared.has_resume_error());
        let second = shared.lease().unwrap();
        assert!(second.inherit_for(&file, &mut Map::new()).await.is_err());
        drop(second);
        assert_eq!(shared.finish().await.0["num"], 1);
    }
}

#[cfg(test)]
mod keyword_feedback_tests {
    use super::*;
    use crate::{event::KeywordFeedback, extract::PostMetadata, postprocess::Specification};
    use futures_util::FutureExt;
    use serde_json::json;

    #[test]
    fn feedback_distinguishes_missing_null_and_unobserved_and_redacts_debug() {
        let feedback = KeywordFeedback::new("PRIVATE_KEY");
        let clone = feedback.clone();
        assert_eq!(feedback.take(), None);
        feedback.capture(&json!({"PRIVATE_KEY": null}).as_object().unwrap().clone());
        assert_eq!(clone.take(), Some(Some(Value::Null)));
        assert_eq!(feedback.take(), None);
        feedback.capture(&Map::new());
        assert_eq!(feedback.take(), Some(None));
        let mut context = json!({"PRIVATE_KEY":{"nested":"PRIVATE_VALUE"}})
            .as_object()
            .unwrap()
            .clone();
        feedback.capture(&context);
        context["PRIVATE_KEY"]["nested"] = json!("changed");
        assert!(!format!("{feedback:?}").contains("PRIVATE"));
        assert_eq!(
            feedback.take(),
            Some(Some(json!({"nested":"PRIVATE_VALUE"})))
        );
    }

    #[tokio::test]
    async fn feedback_waits_for_all_file_leases_and_repeated_finish_observes_latest() {
        let feedback = KeywordFeedback::new("id");
        let context = json!({"id":100}).as_object().unwrap().clone();
        let shared = Shared::new(
            context.clone(),
            context,
            KeywordPolicy::shared(&[]).with_feedback(feedback.clone()),
            Arc::new(AtomicBool::new(false)),
        );
        let first = shared.lease().unwrap();
        let second = shared.lease().unwrap();
        assert!(shared.finish().now_or_never().is_none());
        assert_eq!(feedback.take(), None);
        first.complete(json!({"id":200}).as_object().unwrap());
        drop(first);
        assert!(shared.finish().now_or_never().is_none());
        assert_eq!(feedback.take(), None);
        let mut second_context = json!({"id":100}).as_object().unwrap().clone();
        second.inherit(&mut second_context).await;
        assert_eq!(second_context["id"], 200);
        second_context.insert("id".into(), json!(300));
        second.complete(&second_context);
        drop(second);
        assert_eq!(shared.finish().await.0["id"], 300);
        assert_eq!(feedback.take(), Some(Some(json!(300))));
        shared.checkpoint(json!({"id":400}).as_object().unwrap());
        shared.finish().await;
        assert_eq!(feedback.take(), Some(Some(json!(400))));
    }

    fn post(feedback: KeywordFeedback) -> ExtractionEvent {
        ExtractionEvent::Post(Box::new(Post {
            category: "fixture".into(),
            count: Some(0),
            metadata: Some(PostMetadata {
                post_id: "42".into(),
                ..Default::default()
            }),
            keywords: KeywordPolicy::shared(&[]).with_feedback(feedback),
        }))
    }

    #[tokio::test]
    async fn post_only_feedback_captures_hook_mutations_and_deletion_before_post_after() {
        for (hook, expected) in [
            (
                json!({"name":"metadata","mode":"modify","event":"post","fields":{"id":"{num}"}}),
                Some(json!(220)),
            ),
            (
                json!({"name":"metadata","mode":"delete","event":"post","fields":["id"]}),
                None,
            ),
        ] {
            let root = tempfile::tempdir().unwrap();
            let processors=Arc::new(Postprocessors::new(vec![
                Specification::parse(&hook,root.path()).unwrap(),
                Specification::parse(&json!({"name":"metadata","mode":"modify","event":"post-after","fields":{"id":"AFTER"}}),root.path()).unwrap(),
            ]));
            let feedback = KeywordFeedback::new("id");
            let mut event = post(feedback.clone());
            if let ExtractionEvent::Post(post) = &mut event
                && let KeywordPolicy::Shared { post_fields, .. } = &mut post.keywords
            {
                *post_fields = Arc::new(json!({"id":42,"num":220}).as_object().unwrap().clone());
            }
            let input =
                futures_util::stream::iter(vec![Ok(event), Ok(ExtractionEvent::EndPost)]).boxed();
            let result = apply(
                input,
                Arc::new(BTreeMap::from([(
                    "fixture".into(),
                    JobConfiguration {
                        processors,
                        root: root.path().to_owned(),
                        filename: None,
                        directory: None,
                        init: false,
                    },
                )])),
            )
            .collect::<Vec<_>>()
            .await;
            assert!(result.is_empty());
            assert_eq!(feedback.take(), Some(expected));
        }
    }

    #[tokio::test]
    async fn posts_without_hook_scope_leave_feedback_unobserved() {
        let feedback = KeywordFeedback::new("id");
        let input = futures_util::stream::iter(vec![
            Ok(post(feedback.clone())),
            Ok(ExtractionEvent::EndPost),
        ])
        .boxed();
        assert!(
            apply(input, Arc::new(BTreeMap::new()))
                .collect::<Vec<_>>()
                .await
                .is_empty()
        );
        assert_eq!(feedback.take(), None);
    }
}

#[cfg(test)]
mod keyword_object_tests {
    use super::*;
    use crate::{event::KeywordObject, extract::PostMetadata, postprocess::Specification};
    use serde_json::json;

    #[tokio::test]
    async fn file_mutations_and_post_after_update_prepared_next_post_unless_root_replaced() {
        for detach in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let object = KeywordObject::new(
                json!({"id":7,"name":"ORIGINAL"})
                    .as_object()
                    .unwrap()
                    .clone(),
            );
            let mut specs = vec![
                json!({"name":"metadata","mode":"custom","event":"post","base-directory":root.path(),"filename":"posts.txt","format":"{pool[id]}:{pool[name]}|","open":"a"}),
            ];
            specs.push(json!({"name":"metadata","mode":"modify","event":"post","filter":"id == 1","fields":{"pool[name]":"POST"}}));
            if detach {
                specs.push(json!({"name":"metadata","mode":"modify","event":"file","filter":"id == 1","fields":{"pool":"{pool}"}}));
            }
            specs.extend([
                json!({"name":"metadata","mode":"modify","event":"file","filter":"id == 1","fields":{"pool[id]":"{next_pool_id}"}}),
                json!({"name":"metadata","mode":"modify","event":"post-after","filter":"id == 1","fields":{"pool[name]":"AFTER"}}),
            ]);
            let processors = Arc::new(Postprocessors::new(
                specs
                    .iter()
                    .map(|spec| Specification::parse(spec, root.path()).unwrap())
                    .collect(),
            ));
            // Prepare both Directory snapshots before any hook runs, as source
            // generators can do. Binding identity must outlive their copies.
            let mut events = Vec::new();
            for id in [1, 2] {
                let metadata = PostMetadata {
                    post_id: id.to_string(),
                    ..Default::default()
                };
                let mut policy = KeywordPolicy::shared(&[
                    "num",
                    "index",
                    "media_id",
                    "filename",
                    "name",
                    "extension",
                ])
                .with_shared_object("pool", object.clone());
                if let KeywordPolicy::Shared { post_fields, .. } = &mut policy {
                    *post_fields = Arc::new(
                        json!({"id":id,"pool":object.snapshot(),"next_pool_id":900})
                            .as_object()
                            .unwrap()
                            .clone(),
                    );
                }
                events.push(Ok(ExtractionEvent::Post(Box::new(Post {
                    keywords: policy,
                    category: "fixture".into(),
                    metadata: Some(metadata.clone()),
                    count: Some(1),
                }))));
                let (url, job) = crate::extract::inline_text("file bytes".into()).unwrap();
                events.push(Ok(ExtractionEvent::Media(Box::new(Media {
                    url,
                    job,
                    category: "fixture".into(),
                    filename: format!("{id}.txt"),
                    metadata: Some(metadata),
                    headers: Default::default(),
                    handoff: Default::default(),
                }))));
                events.push(Ok(ExtractionEvent::EndPost));
            }
            let mut jobs = apply(
                futures_util::stream::iter(events).boxed(),
                Arc::new(BTreeMap::from([(
                    "fixture".into(),
                    JobConfiguration {
                        processors: processors.clone(),
                        root: root.path().to_owned(),
                        filename: None,
                        directory: Some(
                            DirectoryTemplate::parse(&["{pool[name]}".into()]).unwrap(),
                        ),
                        init: false,
                    },
                )])),
            );
            let client = crate::download::client(std::time::Duration::from_secs(2)).unwrap();
            let mut count = 0;
            while let Some(item) = jobs.next().await {
                let item = item.unwrap();
                let outcome = crate::download::download_with_options(
                    &client,
                    root.path(),
                    &item.media,
                    crate::download::DownloadOptions {
                        postprocessors: Some(&processors),
                        postprocessor_scope: item.scope.as_ref(),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
                let expected_directory = if count == 0 {
                    "ORIGINAL"
                } else if detach {
                    "POST"
                } else {
                    "AFTER"
                };
                assert_eq!(
                    outcome.saved_path().unwrap().parent().unwrap(),
                    root.path().join(expected_directory)
                );
                count += 1;
            }
            assert_eq!(count, 2);
            assert_eq!(
                std::fs::read_to_string(root.path().join("posts.txt")).unwrap(),
                if detach {
                    "7:ORIGINAL|7:POST|"
                } else {
                    "7:ORIGINAL|900:AFTER|"
                }
            );
            assert_eq!(
                object.snapshot(),
                if detach {
                    json!({"id":7,"name":"POST"})
                } else {
                    json!({"id":900,"name":"AFTER"})
                }
                .as_object()
                .unwrap()
                .clone()
            );
        }
    }
}

#[cfg(test)]
mod independent_object_queue_tests {
    use super::*;
    use crate::{
        event::{ChildMetadata, ChildScope, KeywordObject},
        postprocess::Specification,
    };
    use serde_json::json;
    #[tokio::test]
    async fn independent_queue_callbacks_neither_inject_nor_detach_parent_objects() {
        let root = tempfile::tempdir().unwrap();
        let object = KeywordObject::new(
            json!({"id":7,"name":"ORIGINAL"})
                .as_object()
                .unwrap()
                .clone(),
        );
        let specs = [
            json!({"name":"metadata","mode":"custom","event":"child","base-directory":root.path(),"filename":"child.txt","format":"{pool}|"}),
            json!({"name":"metadata","mode":"modify","event":"child","fields":{"pool":"QUEUE"}}),
            json!({"name":"metadata","mode":"custom","event":"child-after","base-directory":root.path(),"filename":"child-after.txt","format":"{pool}|"}),
            json!({"name":"metadata","mode":"modify","event":"post-after","fields":{"pool[name]":"PARENT_AFTER"}}),
        ];
        let configuration = JobConfiguration {
            processors: Arc::new(Postprocessors::new(
                specs
                    .iter()
                    .map(|v| Specification::parse(v, root.path()).unwrap())
                    .collect(),
            )),
            root: root.path().to_owned(),
            filename: None,
            directory: None,
            init: false,
        };
        let child = ChildScope::new(ChildMetadata::from_raw("unused", "empty", &Map::new()));
        let events = vec![
            Ok(ExtractionEvent::Post(Box::new(Post {
                category: "fixture".into(),
                metadata: None,
                count: Some(0),
                keywords: KeywordPolicy::shared(&[]).with_shared_object("pool", object.clone()),
            }))),
            Ok(ExtractionEvent::BeginChild(child.clone())),
            Ok(ExtractionEvent::EndChild(child.control)),
            Ok(ExtractionEvent::EndPost),
        ];
        assert!(
            apply(
                futures_util::stream::iter(events).boxed(),
                Arc::new(BTreeMap::from([("fixture".into(), configuration)]))
            )
            .collect::<Vec<_>>()
            .await
            .is_empty()
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("child.txt")).unwrap(),
            "None|"
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("child-after.txt")).unwrap(),
            "QUEUE|"
        );
        assert_eq!(object.snapshot()["name"], "PARENT_AFTER");
    }
}

#[cfg(test)]
mod directory_scope_tests {
    use super::*;
    use crate::postprocess::Specification;
    use serde_json::json;
    #[tokio::test]
    async fn absent_job_directory_preserves_caller_download_root_and_directory() {
        let configuration_root = tempfile::tempdir().unwrap();
        let download_root = tempfile::tempdir().unwrap();
        let processors=Arc::new(Postprocessors::new(vec![Specification::parse(&json!({"name":"metadata","mode":"modify","event":"post","fields":{"flag":"present"}}),configuration_root.path()).unwrap()]));
        let (url, job) = crate::extract::inline_text("bytes".into()).unwrap();
        let media = Media {
            url,
            job,
            category: "fixture".into(),
            filename: "file.txt".into(),
            metadata: None,
            headers: Default::default(),
            handoff: Default::default(),
        };
        let events = vec![
            Ok(ExtractionEvent::Post(Box::new(Post {
                keywords: KeywordPolicy::shared(&[]),
                category: "fixture".into(),
                metadata: None,
                count: Some(1),
            }))),
            Ok(ExtractionEvent::Media(Box::new(media))),
            Ok(ExtractionEvent::EndPost),
        ];
        let mut jobs = apply(
            futures_util::stream::iter(events).boxed(),
            Arc::new(BTreeMap::from([(
                "fixture".into(),
                JobConfiguration {
                    processors: processors.clone(),
                    root: configuration_root.path().to_owned(),
                    filename: None,
                    directory: None,
                    init: false,
                },
            )])),
        );
        let item = jobs.next().await.unwrap().unwrap();
        assert_eq!(item.scope.as_ref().unwrap().directory(), None);
        let directory = DirectoryTemplate::parse(&["CALLER".into()]).unwrap();
        let client = crate::download::client(std::time::Duration::from_secs(2)).unwrap();
        let outcome = crate::download::download_with_options(
            &client,
            download_root.path(),
            &item.media,
            crate::download::DownloadOptions {
                postprocessors: Some(&processors),
                postprocessor_scope: item.scope.as_ref(),
                directory: Some(&directory),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            outcome.saved_path().unwrap().parent().unwrap(),
            download_root.path().join("CALLER")
        );
        drop(item);
        assert!(jobs.next().await.is_none());
    }
}
