//! Explicit native configuration: a strict subset of gallery-dl's JSON schema.
//!
//! See <https://gdl-org.github.io/docs/configuration.html>. Extractor defaults
//! are overridden by category settings, then by top-level global settings.
//! Paths are literal, relative to the configuration directory; expansion,
//! conditions, hooks, subcategories, and unsupported options are rejected.
use crate::{
    Result,
    date::parse_date,
    error,
    naming::{DirectoryTemplate, FilenameTemplate},
    selection::RangeSelection,
    size::parse_bytes,
};
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::io::AsyncReadExt;

/// Maximum encoded JSON size, including whitespace.
pub const MAX_CONFIG_BYTES: usize = 1024 * 1024;

/// Sparse settings: absent fields leave the caller's defaults unchanged.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Options {
    pub base_directory: Option<PathBuf>,
    pub filename: Option<String>,
    /// One-based media position ranges, applied separately to each input.
    pub file_range: Option<String>,
    pub date_before: Option<i64>,
    pub date_after: Option<i64>,
    /// Relative directory components; an empty list selects the base directory.
    pub directory: Option<Vec<String>>,
    pub cookies: Option<PathBuf>,
    /// Uses gallery-dl-rs's archive identities, not upstream database IDs.
    pub archive: Option<PathBuf>,
    pub filesize_min: Option<u64>,
    pub filesize_max: Option<u64>,
    pub retries: Option<u8>,
    pub timeout: Option<Duration>,
}

impl Options {
    /// Return a copy with every present field in `overrides` taking precedence.
    /// This can apply explicit CLI values after configuration resolution.
    pub fn overlay(&self, overrides: &Self) -> Self {
        Self {
            base_directory: overrides
                .base_directory
                .clone()
                .or_else(|| self.base_directory.clone()),
            filename: overrides.filename.clone().or_else(|| self.filename.clone()),
            file_range: overrides
                .file_range
                .clone()
                .or_else(|| self.file_range.clone()),
            date_before: overrides.date_before.or(self.date_before),
            date_after: overrides.date_after.or(self.date_after),
            directory: overrides
                .directory
                .clone()
                .or_else(|| self.directory.clone()),
            cookies: overrides.cookies.clone().or_else(|| self.cookies.clone()),
            archive: overrides.archive.clone().or_else(|| self.archive.clone()),
            filesize_min: overrides.filesize_min.or(self.filesize_min),
            filesize_max: overrides.filesize_max.or(self.filesize_max),
            retries: overrides.retries.or(self.retries),
            timeout: overrides.timeout.or(self.timeout),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Config {
    global: Options,
    extractor: Options,
    categories: BTreeMap<String, Options>,
}

// Separate structs deliberately avoid serde flatten: duplicate and unknown
// keys must fail at every level, including global versus extractor scopes.
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
struct RawRoot {
    base_directory: Field,
    filename: Field,
    file_range: Field,
    date_before: Field,
    date_after: Field,
    directory: Field,
    cookies: Field,
    archive: Field,
    filesize_min: Field,
    filesize_max: Field,
    retries: Field,
    timeout: Field,
    extractor: RawExtractor,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
struct RawExtractor {
    base_directory: Field,
    filename: Field,
    file_range: Field,
    date_before: Field,
    date_after: Field,
    directory: Field,
    cookies: Field,
    archive: Field,
    filesize_min: Field,
    filesize_max: Field,
    retries: Field,
    timeout: Field,
    instagram: RawOptions,
    twitter: RawOptions,
    facebook: RawOptions,
    direct: RawOptions,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
struct RawOptions {
    base_directory: Field,
    filename: Field,
    file_range: Field,
    date_before: Field,
    date_after: Field,
    directory: Field,
    cookies: Field,
    archive: Field,
    filesize_min: Field,
    filesize_max: Field,
    retries: Field,
    timeout: Field,
}

/// Unlike Option<Value>, preserve explicit null so it is rejected rather than
/// silently inheriting a potentially surprising parent value.
#[derive(Default)]
struct Field(Option<Value>);
impl<'de> Deserialize<'de> for Field {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        Value::deserialize(deserializer).map(|value| Self(Some(value)))
    }
}

impl Config {
    /// Read at most MAX_CONFIG_BYTES + 1 bytes. Diagnostics never include file
    /// paths, JSON source, unknown key names, or credential values.
    pub async fn load(path: &Path) -> Result<Self> {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(|_| error("cannot resolve native configuration directory"))?
                .join(path)
        };
        let parent = absolute
            .parent()
            .ok_or_else(|| error("native configuration has no parent directory"))?;
        let parent = tokio::fs::canonicalize(parent)
            .await
            .map_err(|_| error("cannot resolve native configuration directory"))?;
        let file = tokio::fs::File::open(&absolute)
            .await
            .map_err(|_| error("cannot open native configuration"))?;
        let mut bytes = Vec::new();
        file.take((MAX_CONFIG_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| error("cannot read native configuration"))?;
        Self::parse(&bytes, &parent)
    }

    /// Parse using an absolute directory for relative paths. This never reads
    /// cookies or archives and never evaluates shell/environment expressions.
    pub fn parse(bytes: &[u8], directory: &Path) -> Result<Self> {
        if bytes.len() > MAX_CONFIG_BYTES {
            return Err(error("native configuration exceeds 1 MiB"));
        }
        if !directory.is_absolute() {
            return Err(error("native configuration directory must be absolute"));
        }
        // Do not forward serde's diagnostics: they can quote private values or
        // a malicious unknown key. Known setting failures are reported below.
        // serde's struct deserializer also accepts positional arrays. Restrict
        // every configuration scope to JSON objects before typed decoding.
        let shape: Value = serde_json::from_slice(bytes)
            .map_err(|_| error("invalid native configuration JSON or unsupported option"))?;
        let object = shape
            .as_object()
            .ok_or_else(|| error("native configuration must be a JSON object"))?;
        if let Some(extractor) = object.get("extractor") {
            let extractor = extractor
                .as_object()
                .ok_or_else(|| error("native configuration extractor must be a JSON object"))?;
            for name in ["instagram", "twitter", "facebook", "direct"] {
                if extractor.get(name).is_some_and(|value| !value.is_object()) {
                    return Err(error("native configuration category must be a JSON object"));
                }
            }
        }
        drop(shape);
        let root: RawRoot = serde_json::from_slice(bytes)
            .map_err(|_| error("invalid native configuration JSON or unsupported option"))?;
        let global = RawOptions {
            base_directory: root.base_directory,
            filename: root.filename,
            file_range: root.file_range,
            date_before: root.date_before,
            date_after: root.date_after,
            directory: root.directory,
            cookies: root.cookies,
            archive: root.archive,
            filesize_min: root.filesize_min,
            filesize_max: root.filesize_max,
            retries: root.retries,
            timeout: root.timeout,
        }
        .resolve(directory)?;
        let extractor = root.extractor;
        let defaults = RawOptions {
            base_directory: extractor.base_directory,
            filename: extractor.filename,
            file_range: extractor.file_range,
            date_before: extractor.date_before,
            date_after: extractor.date_after,
            directory: extractor.directory,
            cookies: extractor.cookies,
            archive: extractor.archive,
            filesize_min: extractor.filesize_min,
            filesize_max: extractor.filesize_max,
            retries: extractor.retries,
            timeout: extractor.timeout,
        }
        .resolve(directory)?;
        let mut categories = BTreeMap::new();
        for (name, settings) in [
            ("instagram", extractor.instagram),
            ("twitter", extractor.twitter),
            ("facebook", extractor.facebook),
            ("direct", extractor.direct),
        ] {
            categories.insert(name.into(), settings.resolve(directory)?);
        }
        Ok(Self {
            global,
            extractor: defaults,
            categories,
        })
    }

    pub fn resolve(&self, category: &str) -> Result<Options> {
        if !matches!(category, "instagram" | "twitter" | "facebook" | "direct") {
            return Err(error("unsupported native configuration category"));
        }
        let settings = self.categories.get(category).map_or_else(
            || self.extractor.clone(),
            |specific| self.extractor.overlay(specific),
        );
        Ok(settings.overlay(&self.global))
    }
}

impl RawOptions {
    fn resolve(self, directory: &Path) -> Result<Options> {
        let file_range = match self.file_range.0 {
            None => None,
            Some(value) => {
                let invalid = || {
                    error(
                        "native configuration file-range must be a supported range string or list of strings",
                    )
                };
                let spec = match value {
                    Value::String(spec) if spec.is_empty() => ":".into(),
                    Value::String(spec) => spec,
                    Value::Array(values) if values.is_empty() => ":".into(),
                    Value::Array(values) if !values.is_empty() && values.len() <= 256 => values
                        .iter()
                        .map(|v| v.as_str().ok_or_else(invalid))
                        .collect::<Result<Vec<_>>>()?
                        .join(","),
                    _ => return Err(invalid()),
                };
                RangeSelection::parse(&spec).map_err(|_| invalid())?;
                Some(spec)
            }
        };
        let filename = match self.filename.0 {
            None => None,
            Some(Value::String(value)) => {
                FilenameTemplate::parse(&value)
                    .map_err(|_| error("unsupported native configuration filename template"))?;
                Some(value)
            }
            Some(_) => {
                return Err(error(
                    "native configuration filename must be a supported string template",
                ));
            }
        };
        let templates = match self.directory.0 {
            None => None,
            Some(Value::Array(values)) => {
                let components: Vec<String> = values
                    .into_iter()
                    .map(|value| {
                        value.as_str().map(str::to_owned).ok_or_else(|| {
                            error("native configuration directory must be an array of string templates")
                        })
                    })
                    .collect::<Result<_>>()?;
                DirectoryTemplate::parse(&components)
                    .map_err(|_| error("unsupported native configuration directory template"))?;
                Some(components)
            }
            Some(_) => {
                return Err(error(
                    "native configuration directory must be an array of string templates",
                ));
            }
        };
        let retries = match self.retries.0 {
            None => None,
            Some(value) => Some(value.as_u64().filter(|value| *value <= 10).ok_or_else(|| {
                error("native configuration retries must be an integer from 0 to 10")
            })? as u8),
        };
        let timeout = match self.timeout.0 {
            None => None,
            Some(value) => {
                let seconds = value
                    .as_f64()
                    .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
                    .ok_or_else(|| {
                        error("native configuration timeout must be positive finite seconds")
                    })?;
                let timeout = Duration::try_from_secs_f64(seconds)
                    .map_err(|_| error("native configuration timeout is out of range"))?;
                if timeout.is_zero() || std::time::Instant::now().checked_add(timeout).is_none() {
                    return Err(error(
                        "native configuration timeout is outside the clock range or precision",
                    ));
                }
                Some(timeout)
            }
        };
        Ok(Options {
            base_directory: path(self.base_directory, directory, "base-directory")?,
            cookies: path(self.cookies, directory, "cookies")?,
            archive: path(self.archive, directory, "archive")?,
            filename,
            file_range,
            date_before: date(self.date_before)?,
            date_after: date(self.date_after)?,
            directory: templates,
            filesize_min: size(self.filesize_min)?,
            filesize_max: size(self.filesize_max)?,
            retries,
            timeout,
        })
    }
}

fn path(field: Field, directory: &Path, name: &str) -> Result<Option<PathBuf>> {
    let Some(value) = field.0 else {
        return Ok(None);
    };
    let invalid = || {
        error(format!(
            "native configuration {name} must be a nonempty literal path without expansion"
        ))
    };
    let value = value.as_str().ok_or_else(invalid)?;
    if value.is_empty()
        || value.chars().any(char::is_control)
        || value.starts_with('~')
        || value.contains('$')
        || value.contains(['{', '}'])
        || value.contains("://")
    {
        return Err(invalid());
    }
    let value = Path::new(value);
    Ok(Some(if value.is_absolute() {
        value.into()
    } else {
        directory.join(value)
    }))
}

fn size(field: Field) -> Result<Option<u64>> {
    let Some(value) = field.0 else {
        return Ok(None);
    };
    let invalid = || {
        error(
            "native configuration file size limit must be nonnegative bytes or a supported binary size string",
        )
    };
    match value {
        Value::Number(number) => number
            .as_u64()
            .map(Ok)
            .unwrap_or_else(|| parse_bytes(&number.to_string()))
            .map(Some)
            .map_err(|_| invalid()),
        Value::String(value) => parse_bytes(&value).map(Some).map_err(|_| invalid()),
        _ => Err(invalid()),
    }
}

fn date(field: Field) -> Result<Option<i64>> {
    let Some(value) = field.0 else {
        return Ok(None);
    };
    let invalid = || {
        error(
            "native configuration date limit must be a supported ISO date or integer Unix timestamp",
        )
    };
    match value {
        Value::String(value) => parse_date(&value).map(Some).map_err(|_| invalid()),
        Value::Number(value) => value.as_i64().map(Some).ok_or_else(invalid),
        _ => Err(invalid()),
    }
}
