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

#[derive(Clone)]
pub struct Specification {
    pub options: UgoiraOptions,
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
        let _ = event; // Ugoira registers fixed hooks; event suffixes do not change them.
        let (name, mode) = name
            .rsplit_once('/')
            .map_or((name, None), |(name, mode)| (name, Some(mode)));
        if name != "ugoira" {
            return Err(error("unsupported native postprocessor; supported: ugoira"));
        }
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
        Ok(Self {
            options: UgoiraOptions::parse(&options, base)?,
            filter,
            whitelist: list("whitelist")?,
            blacklist: list("blacklist")?,
        })
    }
    pub fn applies(&self, media: &Media) -> bool {
        let matches = |name: &str| {
            name == media.category
                || name.strip_suffix(":*") == Some(media.category.as_str())
                || name.strip_suffix(':') == Some(media.category.as_str())
        };
        let category = self.whitelist.as_ref().map_or_else(
            || {
                !self
                    .blacklist
                    .as_ref()
                    .is_some_and(|v| v.iter().any(|name| matches(name)))
            },
            |v| v.is_empty() || v.iter().any(|name| matches(name)),
        );
        category
            && self
                .filter
                .as_ref()
                .is_none_or(|filter| filter.matches(media).unwrap_or(false))
            && media.metadata.as_ref().is_some_and(|m| m.frames.is_some())
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
    specifications: Vec<Specification>,
    pub(crate) frames: frames::FrameCollector,
}
impl Postprocessors {
    pub fn new(specifications: Vec<Specification>) -> Self {
        Self {
            specifications,
            frames: Default::default(),
        }
    }
    pub fn selected<'a>(&'a self, media: &Media) -> Vec<&'a Specification> {
        self.plan(media).1
    }
    pub fn prepared_media(&self, media: &Media) -> Media {
        self.plan(media).0
    }
    fn plan<'a>(&'a self, media: &Media) -> (Media, Vec<&'a Specification>) {
        let mut prepared = media.clone();
        let zip = media
            .metadata
            .as_ref()
            .is_some_and(|m| m.ugoira_frame_index.is_none());
        let mut selected = Vec::new();
        for spec in &self.specifications {
            if spec.applies(&prepared) {
                selected.push(spec);
                if zip && !spec.options.keep_files() {
                    prepared = with_extension(&prepared, spec.options.extension());
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
    spec: &Specification,
    input: &FrameInput,
    media: &Media,
    path: &Path,
    timeout: std::time::Duration,
) -> Result<()> {
    if regular_file(path)? && spec.options.skip() {
        return Ok(());
    }
    let parent = path
        .parent()
        .ok_or_else(|| error("invalid postprocessor destination"))?;
    tokio::fs::create_dir_all(parent).await?;
    let temporary = tempfile::TempDir::new_in(parent)?;
    let output = temporary
        .path()
        .join(format!("converted.{}", spec.options.extension()));
    spec.options.convert(input, &output, media, timeout).await?;
    if !regular_file(&output)? {
        return Err(error("postprocessor produced no output"));
    }
    std::fs::File::open(&output)?.sync_all()?;
    if spec.options.skip() {
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
