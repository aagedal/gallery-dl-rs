//! Native post-download processing and source-frame collection.
use crate::{Result, error, extract::Media, pixiv::UgoiraFrame};
use serde_json::{Map, Value};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
#[path = "postprocess_ugoira.rs"]
pub mod ugoira;
#[path = "postprocess_zip.rs"]
pub(crate) mod zip;
pub use ugoira::UgoiraOptions;

#[derive(Clone, Debug)]
pub struct SourceFrame {
    pub frame: UgoiraFrame,
    pub path: PathBuf,
    pub extension: String,
}
#[derive(Clone, Debug)]
pub enum FrameInput {
    Zip {
        path: PathBuf,
        frames: Arc<[UgoiraFrame]>,
    },
    Files(Vec<SourceFrame>),
}
impl FrameInput {
    pub fn frames(&self) -> Vec<UgoiraFrame> {
        match self {
            Self::Zip { frames, .. } => frames.to_vec(),
            Self::Files(frames) => frames.iter().map(|f| f.frame.clone()).collect(),
        }
    }
}

#[path = "postprocess_hash.rs"]
pub mod hash;
#[path = "postprocess_jobs.rs"]
pub mod jobs;
#[path = "postprocess_keywords.rs"]
pub(crate) mod keywords;
#[path = "postprocess_metadata.rs"]
pub mod metadata;
#[path = "postprocess_mtime.rs"]
pub mod mtime;
#[path = "postprocess_template.rs"]
pub mod template;

#[derive(Clone, Debug)]
pub enum Processor {
    Ugoira(UgoiraOptions),
    Metadata(metadata::MetadataOptions),
    Mtime(mtime::MtimeOptions),
    Hash(hash::HashOptions),
}
impl Processor {
    pub fn ugoira(&self) -> Option<&UgoiraOptions> {
        if let Self::Ugoira(options) = self {
            Some(options)
        } else {
            None
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hook {
    Init,
    Prepare,
    PrepareAfter,
    File,
    After,
    Skip,
    Error,
    Post,
    PostAfter,
    Child,
    ChildAfter,
    Finalize,
    FinalizeSuccess,
    FinalizeError,
}
impl Hook {
    fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "init" => Self::Init,
            "prepare" => Self::Prepare,
            "prepare-after" => Self::PrepareAfter,
            "file" => Self::File,
            "after" => Self::After,
            "skip" => Self::Skip,
            "error" => Self::Error,
            "post" => Self::Post,
            "post-after" => Self::PostAfter,
            "child" => Self::Child,
            "child-after" => Self::ChildAfter,
            "finalize" => Self::Finalize,
            "finalize-success" => Self::FinalizeSuccess,
            "finalize-error" => Self::FinalizeError,
            _ => return Err(error("unsupported native postprocessor event")),
        })
    }
}

#[derive(Clone)]
pub struct Specification {
    pub options: Processor,
    pub events: Arc<[Hook]>,
    pub filter: Option<crate::filter::FilterExpression>,
    pub whitelist: Option<Vec<String>>,
    pub blacklist: Option<Vec<String>>,
}
impl std::fmt::Debug for Specification {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Postprocessor { options: [redacted] }")
    }
}
impl Specification {
    pub fn parse(value: &Value, base: &Path) -> Result<Self> {
        let mut options = match value {
            Value::String(name) => Map::from_iter([("name".into(), Value::String(name.clone()))]),
            Value::Object(options) => options.clone(),
            _ => return Err(error("invalid native postprocessor configuration")),
        };
        let name = options
            .get("name")
            .or_else(|| options.get("type"))
            .and_then(Value::as_str)
            .ok_or_else(|| error("native postprocessor requires a name"))?
            .to_owned();
        let (name, event) = name
            .rsplit_once('@')
            .map_or((name.as_str(), None), |(name, event)| (name, Some(event)));
        if !options.contains_key("event")
            && let Some(event) = event
        {
            options.insert("event".into(), event.into());
        }
        let (name, mode) = name
            .rsplit_once('/')
            .map_or((name, None), |(name, mode)| (name, Some(mode)));

        if !options.contains_key("mode")
            && let Some(mode) = mode
        {
            options.insert("mode".into(), mode.into());
        }
        let filter = match options.get("filter").filter(|v| truthy(v)) {
            None => None,
            Some(Value::String(value)) => Some(crate::filter::FilterExpression::parse(value)?),
            _ => return Err(error("invalid native postprocessor filter")),
        };
        let list = |key: &str| -> Result<Option<Vec<String>>> {
            let values = match options.get(key) {
                None | Some(Value::Null) => return Ok(None),
                Some(Value::String(value)) if value.is_empty() => Vec::new(),
                Some(Value::String(value)) => value.split(',').take(129).collect::<Vec<_>>(),
                Some(Value::Array(values)) if values.len() <= 128 => values
                    .iter()
                    .map(|value| {
                        value
                            .as_str()
                            .ok_or_else(|| error("invalid postprocessor category filter"))
                    })
                    .collect::<Result<Vec<_>>>()?,
                _ => return Err(error("invalid postprocessor category filter")),
            };
            if values.len() > 128 || values.iter().any(|value| value.len() > 256) {
                return Err(error(
                    "postprocessor category filter exceeds supported limits",
                ));
            }
            if values.iter().any(|value| {
                value
                    .split_once(':')
                    .is_some_and(|(_, suffix)| !matches!(suffix, "" | "*"))
            }) {
                return Err(error("unsupported postprocessor category filter"));
            }
            Ok(Some(values.into_iter().map(str::to_owned).collect()))
        };
        let processor = match name {
            "ugoira" => Processor::Ugoira(UgoiraOptions::parse(&options, base)?),
            "metadata" => Processor::Metadata(metadata::MetadataOptions::parse(&options, base)?),
            "mtime" => Processor::Mtime(mtime::MtimeOptions::parse(&options)?),
            "hash" => Processor::Hash(hash::HashOptions::parse(&options)?),
            _ => {
                return Err(error(
                    "unsupported native postprocessor; supported: ugoira, metadata, mtime, hash",
                ));
            }
        };
        let defaults = match &processor {
            Processor::Mtime(options) => options
                .default_events()
                .iter()
                .map(|s| s.to_string())
                .collect(),
            _ => vec!["file".to_owned()],
        };
        let events = if matches!(processor, Processor::Ugoira(_)) {
            Vec::new()
        } else {
            let events = match options.get("event") {
                None | Some(Value::Null) => defaults,
                Some(Value::String(value)) => value.split(',').map(str::to_owned).collect(),
                Some(Value::Array(values)) if values.len() <= 32 => values
                    .iter()
                    .map(|v| {
                        v.as_str()
                            .map(str::to_owned)
                            .ok_or_else(|| error("invalid postprocessor event"))
                    })
                    .collect::<Result<Vec<_>>>()?,
                _ => return Err(error("invalid postprocessor event")),
            };
            if events.len() > 32 {
                return Err(error("too many postprocessor events"));
            }
            events
                .iter()
                .map(|value| Hook::parse(value))
                .collect::<Result<Vec<_>>>()?
        };
        Ok(Self {
            options: processor,
            events: events.into(),
            filter,
            whitelist: list("whitelist")?,
            blacklist: list("blacklist")?,
        })
    }
    fn matches_category(&self, media: &Media) -> bool {
        let matches = |name: &str| {
            name == media.category
                || name.strip_suffix(":*") == Some(media.category.as_str())
                || name.strip_suffix(':') == Some(media.category.as_str())
        };
        self.whitelist.as_ref().map_or_else(
            || {
                !self
                    .blacklist
                    .as_ref()
                    .is_some_and(|v| v.iter().any(|name| matches(name)))
            },
            |v| v.is_empty() || v.iter().any(|name| matches(name)),
        )
    }
    pub(crate) fn matches_keywords(&self, media: &Media, keywords: &Map<String, Value>) -> bool {
        self.matches_category(media)
            && self
                .filter
                .as_ref()
                .is_none_or(|filter| filter.matches_keywords(keywords).unwrap_or(false))
    }
    pub fn applies(&self, media: &Media) -> bool {
        self.matches_category(media)
            && self
                .filter
                .as_ref()
                .is_none_or(|filter| filter.matches(media).unwrap_or(false))
            && (self.options.ugoira().is_none()
                || media.metadata.as_ref().is_some_and(|m| m.frames.is_some()))
    }
}
pub(crate) fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(v) => *v,
        Value::Number(v) => v.as_f64() != Some(0.0),
        Value::String(v) => !v.is_empty(),
        Value::Array(v) => !v.is_empty(),
        Value::Object(v) => !v.is_empty(),
    }
}

/// Scoped source configuration. More specific lists run before inherited lists.
#[derive(Clone, PartialEq)]
pub struct Configuration {
    entries: Vec<Value>,
    overrides: Option<Map<String, Value>>,
    definitions: Option<Map<String, Value>>,
    base: PathBuf,
}
impl std::fmt::Debug for Configuration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Postprocessors { configuration: [redacted] }")
    }
}
impl Configuration {
    pub fn parse(
        entries: Option<Value>,
        overrides: Option<Value>,
        definitions: Option<Value>,
        base: &Path,
    ) -> Result<Option<Self>> {
        if entries.is_none() && overrides.is_none() && definitions.is_none() {
            return Ok(None);
        }
        let entries = match entries.filter(truthy) {
            None => Vec::new(),
            Some(Value::Array(values)) if values.len() <= 64 => values,
            Some(value @ (Value::String(_) | Value::Object(_))) => vec![value],
            _ => return Err(error("invalid native postprocessors configuration")),
        };
        if entries
            .iter()
            .any(|v| !matches!(v, Value::String(_) | Value::Object(_)))
        {
            return Err(error("invalid native postprocessor entry"));
        }
        let overrides = match overrides {
            None => None,
            Some(value) if !truthy(&value) => Some(Map::new()),
            Some(Value::Object(value)) => Some(value),
            _ => return Err(error("invalid native postprocessor options")),
        };
        let definitions = match definitions {
            None | Some(Value::Null) => None,
            Some(Value::Object(value)) if value.len() <= 128 => Some(value),
            _ => return Err(error("invalid native postprocessor definitions")),
        };
        Ok(Some(Self {
            entries,
            overrides,
            definitions,
            base: base.to_owned(),
        }))
    }
    pub fn overlaid(&self, specific: &Self) -> Self {
        let mut entries = specific.entries.clone();
        entries.extend(self.entries.iter().cloned());
        Self {
            entries,
            overrides: specific
                .overrides
                .clone()
                .or_else(|| self.overrides.clone()),
            definitions: specific
                .definitions
                .clone()
                .or_else(|| self.definitions.clone()),
            base: specific.base.clone(),
        }
    }
    /// Root options take scalar precedence while lists retain specific-first order.
    pub(crate) fn with_global_options(mut self, global: &Self) -> Self {
        if global.overrides.is_some() {
            self.overrides.clone_from(&global.overrides);
        }
        if global.definitions.is_some() {
            self.definitions.clone_from(&global.definitions);
        }
        self
    }
    pub fn specifications(&self) -> Result<Vec<Specification>> {
        if self.entries.len() > 128 {
            return Err(error("too many native postprocessors"));
        }
        let mut result = Vec::new();
        for value in &self.entries {
            let definitions = self.definitions.as_ref();
            let mut options = match value {
                Value::String(name) => definitions
                    .and_then(|d| d.get(name))
                    .filter(|v| truthy(v))
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({"name":name})),
                Value::Object(options) => {
                    let mut merged = options
                        .get("type")
                        .and_then(Value::as_str)
                        .and_then(|name| definitions.and_then(|d| d.get(name)))
                        .and_then(Value::as_object)
                        .cloned()
                        .unwrap_or_default();
                    merged.extend(options.clone());
                    if !merged.contains_key("name")
                        && let Some(name) = merged.get("type").cloned()
                    {
                        merged.insert("name".into(), name);
                    }
                    Value::Object(merged)
                }
                _ => return Err(error("invalid native postprocessor entry")),
            };
            if let Some(overrides) = &self.overrides {
                options
                    .as_object_mut()
                    .ok_or_else(|| error("invalid native postprocessor definition"))?
                    .extend(overrides.clone());
            }
            result.push(Specification::parse(&options, &self.base)?);
        }
        Ok(result)
    }
}
/// Match Pixiv's URL date before falling back to the artwork creation date.
pub fn source_timestamp(media: &Media) -> Option<i64> {
    let parts: Vec<_> = media.url.as_str().split('/').collect();
    let timestamp = (parts.len() == 12)
        .then(|| {
            format!(
                "{}-{}-{}T{}:{}:{}+09:00",
                parts[5], parts[6], parts[7], parts[8], parts[9], parts[10]
            )
        })
        .and_then(|raw| crate::date::parse_date(&raw).ok());
    timestamp.or_else(|| media.metadata.as_ref().and_then(|m| m.timestamp))
}

#[path = "postprocess_frames.rs"]
pub mod frames;

#[derive(Debug)]
pub struct Postprocessors {
    pub(crate) specifications: Vec<Specification>,
    pub(crate) frames: frames::FrameCollector,
}
impl Postprocessors {
    pub fn new(specifications: Vec<Specification>) -> Self {
        Self {
            specifications,
            frames: Default::default(),
        }
    }
    pub(crate) fn has_generic(&self) -> bool {
        self.specifications
            .iter()
            .any(|spec| !matches!(spec.options, Processor::Ugoira(_)))
    }
    pub fn selected<'a>(&'a self, media: &Media) -> Vec<&'a UgoiraOptions> {
        self.plan(media).1
    }
    pub fn prepared_media(&self, media: &Media) -> Media {
        self.plan(media).0
    }
    fn plan<'a>(&'a self, media: &Media) -> (Media, Vec<&'a UgoiraOptions>) {
        let mut prepared = media.clone();
        let zip = media
            .metadata
            .as_ref()
            .is_some_and(|m| m.ugoira_frame_index.is_none());
        let mut selected = Vec::new();
        for spec in &self.specifications {
            if let Some(options) = spec.options.ugoira()
                && spec.applies(&prepared)
            {
                selected.push(options);
                if zip && !options.keep_files() {
                    prepared = with_extension(&prepared, options.extension());
                }
            }
        }
        (prepared, selected)
    }
}
pub fn with_extension(media: &Media, extension: &str) -> Media {
    let mut result = media.clone();
    let stem = result
        .filename
        .rsplit_once('.')
        .map_or(result.filename.as_str(), |(stem, _)| stem);
    result.filename = format!("{stem}.{extension}");
    result
}

pub(crate) fn regular_file(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => Ok(true),
        Ok(_) => Err(error("postprocessor destination is not a regular file")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}
/// Conversion takes place on the destination filesystem and only complete files
/// become visible. Encoder output and arguments are never included in errors.
pub(crate) async fn convert(
    options: &UgoiraOptions,
    input: &FrameInput,
    media: &Media,
    path: &Path,
    timeout: std::time::Duration,
) -> Result<()> {
    if regular_file(path)? && options.skip() {
        return Ok(());
    }
    let parent = path
        .parent()
        .ok_or_else(|| error("invalid postprocessor destination"))?;
    tokio::fs::create_dir_all(parent).await?;
    let temporary = tempfile::TempDir::new_in(parent)?;
    let output = temporary
        .path()
        .join(format!("converted.{}", options.extension()));
    options.convert(input, &output, media, timeout).await?;
    if !regular_file(&output)? {
        return Err(error("postprocessor produced no output"));
    }
    std::fs::File::open(&output)?.sync_all()?;
    if options.skip() {
        match std::fs::hard_link(&output, path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                regular_file(path)?;
            }
            Err(e) => return Err(e.into()),
        }
    } else {
        regular_file(path)?;
        std::fs::rename(&output, path)?;
    }
    Ok(())
}

/// Keyword values exposed by native extractors. Transport headers, handoffs and
/// cookie stores are deliberately absent, just as they are in expression contexts.
pub fn context(media: &Media) -> Map<String, Value> {
    fn json(value: &crate::filter::Value) -> Value {
        use crate::filter::Value as V;
        match value {
            V::None => Value::Null,
            V::Bool(v) => (*v).into(),
            V::Int(v) => serde_json::Number::from_i128(*v).map_or(Value::Null, Value::Number),
            V::Float(v) => serde_json::Number::from_f64(*v).map_or(Value::Null, Value::Number),
            V::String(v) => v.as_ref().into(),
            V::List(v) | V::Tuple(v) => Value::Array(v.iter().map(json).collect()),
            V::Map(v) => Value::Object(v.iter().map(|(k, v)| (k.clone(), json(v))).collect()),
        }
    }
    let mut context: Map<String, Value> = crate::filter::context(media)
        .into_iter()
        .map(|(k, v)| (k, json(&v)))
        .collect();
    if let Some(timestamp) = media.metadata.as_ref().and_then(|m| m.timestamp) {
        context.entry("date").or_insert(timestamp.into());
    }
    for key in ["date", "date_updated", "date_url"] {
        if let Some(timestamp) = context.get(key).and_then(Value::as_i64)
            && let Ok(date) = time::OffsetDateTime::from_unix_timestamp(timestamp)
        {
            context.insert(
                key.into(),
                format!(
                    "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                    date.year(),
                    date.month() as u8,
                    date.day(),
                    date.hour(),
                    date.minute(),
                    date.second()
                )
                .into(),
            );
        }
    }
    context
}
#[derive(Clone, Debug, Default)]
pub(crate) struct HookPaths {
    pub directory: PathBuf,
    pub filename: Option<std::ffi::OsString>,
    pub realpath: Option<PathBuf>,
    pub extension: String,
}
impl Postprocessors {
    pub(crate) async fn run_job_hook(
        &self,
        hook: Hook,
        context: &mut Map<String, Value>,
        media: &Media,
        paths: &mut HookPaths,
        root: &Path,
        filename: Option<&crate::naming::FilenameTemplate>,
    ) -> Result<()> {
        for spec in &self.specifications {
            if !spec.events.contains(&hook) || !spec.matches_keywords(media, context) {
                continue;
            }
            match &spec.options {
                Processor::Ugoira(_) => {}
                Processor::Metadata(options) => {
                    options
                        .clone()
                        .with_filename_template(filename.cloned())
                        .run_job(context, media, paths, root)
                        .await?
                }
                Processor::Mtime(options) if options.is_file() => {
                    let timestamp = options.getter(context);
                    context.insert(
                        "_mtime_meta".into(),
                        timestamp.map_or(Value::Null, Value::from),
                    );
                }
                Processor::Mtime(options) => {
                    // Upstream set_mtime ignores OS errors, including absent directories.
                    let _ = options.apply(context, &paths.directory);
                }
                Processor::Hash(options) => {
                    let path = paths
                        .realpath
                        .as_deref()
                        .ok_or_else(|| error("missing postprocessor hash input"))?;
                    if options.run(context, path).await? {
                        let name = match filename {
                            Some(template) => template.render_keywords(media, context)?,
                            None => None,
                        }
                        .unwrap_or_else(|| {
                            paths
                                .filename
                                .as_ref()
                                .map(|name| name.to_string_lossy().into_owned())
                                .unwrap_or_else(|| media.filename.clone())
                        });
                        paths.filename = Some(name.clone().into());
                        paths.realpath = Some(paths.directory.join(name));
                    }
                }
            }
        }
        Ok(())
    }
}
impl Postprocessors {
    pub async fn run_hook(
        &self,
        hook: Hook,
        context: &mut Map<String, Value>,
        media: &Media,
        path: &Path,
        root: &Path,
    ) -> Result<bool> {
        let mut rebuild = false;
        for spec in &self.specifications {
            if !spec.events.contains(&hook)
                || !spec.matches_category(media)
                || spec
                    .filter
                    .as_ref()
                    .is_some_and(|filter| !filter.matches_keywords(context).unwrap_or(false))
            {
                continue;
            }
            match &spec.options {
                Processor::Ugoira(_) => {}
                Processor::Metadata(options) => options.run(context, media, path, root).await?,
                Processor::Mtime(options) => options.apply(
                    context,
                    if options.is_file() {
                        path
                    } else {
                        path.parent()
                            .ok_or_else(|| error("invalid postprocessor directory"))?
                    },
                )?,
                Processor::Hash(options) => rebuild |= options.run(context, path).await?,
            }
        }
        Ok(rebuild)
    }
}

#[cfg(test)]
mod job_mtime_path_tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn job_file_mtime_records_timestamp_without_touching_missing_paths() {
        let root = tempfile::tempdir().unwrap();
        let processors = Postprocessors::new(vec![
            Specification::parse(&json!({"name":"mtime","event":"post"}), root.path()).unwrap(),
            Specification::parse(
                &json!({"name":"mtime","target":"directory","event":"post-after"}),
                root.path(),
            )
            .unwrap(),
        ]);
        let media = Media {
            url: "https://fixture.invalid/file.jpg".parse().unwrap(),
            category: "fixture".into(),
            filename: "post".into(),
            metadata: None,
            job: crate::extract::MediaJob::DirectHttp,
            headers: Default::default(),
            handoff: Default::default(),
        };
        let directory = root.path().join("not-created");
        let missing_file = root.path().join("missing.jpg");
        let mut paths = HookPaths {
            directory: directory.clone(),
            filename: None,
            realpath: Some(missing_file.clone()),
            extension: String::new(),
        };
        let mut context = Map::from_iter([("date".into(), json!(1704164646))]);
        processors
            .run_job_hook(
                Hook::Post,
                &mut context,
                &media,
                &mut paths,
                root.path(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(context["_mtime_meta"], 1704164646);
        assert!(!missing_file.exists());
        assert!(!directory.exists());
        processors
            .run_job_hook(
                Hook::PostAfter,
                &mut context,
                &media,
                &mut paths,
                root.path(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(context["_mtime_meta"], 1704164646);
        assert!(!missing_file.exists());
        assert!(!directory.exists());
    }
}
