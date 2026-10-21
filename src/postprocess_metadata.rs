//! Metadata sidecars and ordered metadata mutations.
use crate::{Result, error, extract::Media, postprocess::template::Template};
use serde_json::{Map, Value};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::io::AsyncWriteExt;

const LIMIT: usize = 32 * 1024 * 1024;
fn fail() -> crate::Error {
    error("invalid metadata postprocessor configuration")
}
fn truthy(v: &Value) -> bool {
    if let Some(value) = crate::filter::source_primitive_from_json(v) {
        return truthy(value);
    }
    if let Some(value) = crate::filter::source_date_from_json(v) {
        return value.is_some();
    }
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(m) => !m.is_empty(),
        Value::Number(n) => n.as_f64() != Some(0.0),
    }
}
#[derive(Clone, Debug)]
enum Mode {
    Json,
    Tags,
    Custom(Template),
    Modify(Vec<(String, Template)>),
    Delete(Vec<String>),
}
#[derive(Clone, Debug)]
enum Directory {
    Default,
    Literal(PathBuf),
    Formatted(Vec<Template>),
}
#[derive(Clone)]
pub struct MetadataOptions {
    mode: Mode,
    archive_config: Option<Arc<super::archive::ArchiveConfig>>,
    archive_state: Arc<tokio::sync::Mutex<ArchiveState>>,
    base: Option<PathBuf>,
    root_base: bool,
    directory: Directory,
    filename: Option<Template>,
    extension: String,
    extension_format: Option<Template>,
    filename_template: Option<crate::naming::FilenameTemplate>,
    stdout: bool,
    append: bool,
    exclusive: bool,
    empty: bool,
    skip: bool,
    mtime: bool,
    meta_path: Option<String>,
    include: Option<Vec<String>>,
    exclude: Vec<String>,
    private: bool,
    ascii: bool,
    encoding_ascii: bool,
    sort: bool,
    indent: Option<String>,
    separators: (String, String),
    newline: Option<String>,
}
enum ArchiveState {
    Uninitialized,
    Disabled,
    Open(super::archive::ArchiveSession),
    Closed,
}
impl std::fmt::Debug for MetadataOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MetadataOptions { [redacted] }")
    }
}
struct DestinationState<'a> {
    directory: &'a Path,
    filename: Option<&'a std::ffi::OsStr>,
    extension: &'a str,
    source_extension: Option<&'a Value>,
}
impl MetadataOptions {
    pub fn default_events() -> Vec<String> {
        vec!["file".into()]
    }
    pub fn parse(options: &Map<String, Value>, base: &Path) -> Result<Self> {
        let archive_config = super::archive::ArchiveConfig::parse(options, base)?.map(Arc::new);
        let flag = |k: &str| options.get(k).is_some_and(truthy);
        if let Some(encoding) = options.get("encoding") {
            match encoding.as_str() {
                Some("utf-8" | "utf8" | "UTF-8" | "ascii" | "us-ascii") => (),
                _ => return Err(error("unsupported metadata postprocessor encoding")),
            }
        }
        let ascii = flag("ascii");
        let ascii_encoding = options
            .get("encoding")
            .and_then(Value::as_str)
            .is_some_and(|s| matches!(s, "ascii" | "us-ascii"));
        // ASCII encoding is checked after rendering; JSON ensure_ascii is a separate setting.
        let content = options
            .get("content-format")
            .filter(|v| truthy(v))
            .or_else(|| options.get("format"));
        let format_content = |v: &Value| -> Result<String> {
            match v {
                Value::String(s) if s.len() <= LIMIT => Ok(s.clone()),
                Value::Array(a) if a.len() <= 4096 => {
                    let mut s = String::new();
                    for v in a {
                        s.push_str(v.as_str().ok_or_else(fail)?);
                        s.push('\n');
                        if s.len() > LIMIT {
                            return Err(fail());
                        }
                    }
                    Ok(s)
                }
                _ => Err(fail()),
            }
        };
        let mode_name = options.get("mode").and_then(Value::as_str).unwrap_or("");
        let mut stdout = mode_name == "print";
        let mode = match mode_name {
            "tags" => Mode::Tags,
            "modify" => {
                let m = options
                    .get("fields")
                    .and_then(Value::as_object)
                    .ok_or_else(fail)?;
                if m.len() > 4096 {
                    return Err(fail());
                }
                Mode::Modify(
                    m.iter()
                        .map(|(k, v)| {
                            Ok((
                                bounded_key(k)?,
                                Template::parse(v.as_str().ok_or_else(fail)?)?,
                            ))
                        })
                        .collect::<Result<_>>()?,
                )
            }
            "delete" => Mode::Delete(match options.get("fields") {
                Some(Value::String(s)) if s.len() <= 4096 => {
                    s.chars().map(|c| c.to_string()).collect()
                }
                Some(Value::Object(m)) if m.len() <= 4096 => {
                    m.keys().map(|k| bounded_key(k)).collect::<Result<_>>()?
                }
                other => list(other)?.ok_or_else(fail)?,
            }),
            "custom" | "print" => {
                let mut s = format_content(content.ok_or_else(fail)?)?;
                if stdout
                    && !s.ends_with('\n')
                    && (!s.starts_with('\u{c}') || s.as_bytes().get(1) == Some(&b'F'))
                {
                    s.push('\n');
                }
                Mode::Custom(Template::parse(&s)?)
            }
            "" if content.is_some_and(truthy) => {
                Mode::Custom(Template::parse(&format_content(content.unwrap())?)?)
            }
            _ => Mode::Json,
        };
        let jsonl = mode_name == "jsonl";
        let default_ext = if matches!(mode, Mode::Json) {
            "json"
        } else {
            "txt"
        };
        let filename_value = options.get("filename").cloned().unwrap_or_else(|| {
            if stdout {
                Value::String("-".into())
            } else if jsonl {
                Value::String("data.jsonl".into())
            } else {
                Value::Null
            }
        });
        let filename = match filename_value {
            Value::String(s) if !s.is_empty() => {
                if s == "-" {
                    stdout = true;
                    None
                } else {
                    Some(Template::parse(&s)?)
                }
            }
            Value::Null | Value::Bool(false) => None,
            Value::String(_) => None,
            _ => return Err(fail()),
        };
        let extension = match options.get("extension") {
            None => default_ext.to_owned(),
            Some(Value::String(s))
                if !s.is_empty() && s.len() <= 128 && !s.contains(['/', '\\', '\0']) =>
            {
                s.clone()
            }
            _ => return Err(fail()),
        };
        let extension_format = options
            .get("extension-format")
            .filter(|v| truthy(v))
            .map(|v| Template::parse(v.as_str().ok_or_else(fail)?))
            .transpose()?;
        let mut root_base = false;
        let base = match options.get("base-directory").filter(|v| truthy(v)) {
            None => None,
            Some(Value::Bool(true)) => {
                root_base = true;
                None
            }
            Some(Value::String(s)) => Some(resolve(base, s)?),
            _ => return Err(fail()),
        };
        let directory = match options.get("directory").filter(|v| truthy(v)) {
            None => Directory::Default,
            Some(Value::String(s)) => Directory::Literal(expand(s)?),
            Some(Value::Array(a)) if a.len() <= 128 => Directory::Formatted(
                a.iter()
                    .map(|v| Template::parse(v.as_str().ok_or_else(fail)?))
                    .collect::<Result<_>>()?,
            ),
            _ => return Err(fail()),
        };
        let open = options
            .get("open")
            .and_then(Value::as_str)
            .unwrap_or(if jsonl { "a" } else { "w" });
        if !matches!(open, "w" | "a" | "x" | "wt" | "at" | "xt") {
            return Err(error("unsupported metadata postprocessor open mode"));
        }
        let indent = match options.get("indent") {
            None if !jsonl => Some("    ".into()),
            None | Some(Value::Null) => None,
            Some(Value::Number(n)) => {
                Some(" ".repeat(n.as_u64().filter(|v| *v <= 128).ok_or_else(fail)? as usize))
            }
            Some(Value::String(s)) if s.len() <= 128 => Some(s.clone()),
            _ => return Err(fail()),
        };
        let separators = match options.get("separators") {
            None | Some(Value::Null) => (
                if indent.is_some() { "," } else { ", " }.into(),
                ": ".into(),
            ),
            Some(Value::Array(v)) if v.len() == 2 => (
                v[0].as_str()
                    .filter(|s| s.len() <= 128)
                    .ok_or_else(fail)?
                    .into(),
                v[1].as_str()
                    .filter(|s| s.len() <= 128)
                    .ok_or_else(fail)?
                    .into(),
            ),
            _ => return Err(fail()),
        };
        let newline = match options.get("newline") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) if matches!(s.as_str(), "" | "\n" | "\r" | "\r\n") => {
                Some(s.clone())
            }
            _ => return Err(fail()),
        };
        let out = Self {
            mode,
            archive_config,
            archive_state: Arc::new(tokio::sync::Mutex::new(ArchiveState::Uninitialized)),
            base,
            root_base,
            directory,
            filename,
            extension,
            extension_format,
            filename_template: None,
            stdout,
            append: open.starts_with('a'),
            exclusive: open.starts_with('x'),
            empty: flag("empty"),
            skip: flag("skip"),
            mtime: flag("mtime"),
            meta_path: options
                .get("metadata-path")
                .map(|v| bounded_key(v.as_str().ok_or_else(fail)?))
                .transpose()?,
            include: list(options.get("include").filter(|v| truthy(v)))?.filter(|v| !v.is_empty()),
            exclude: list(options.get("exclude").filter(|v| truthy(v)))?.unwrap_or_default(),
            private: flag("private"),
            ascii,
            encoding_ascii: ascii_encoding,
            sort: flag("sort"),
            indent,
            separators,
            newline,
        };
        // Retain encoding validation without storing private configuration in errors.
        Ok(out)
    }
    pub(crate) fn fresh_job(&self) -> Self {
        let mut options = self.clone();
        options.archive_state = Arc::new(tokio::sync::Mutex::new(ArchiveState::Uninitialized));
        options
    }
    pub(crate) async fn initialize_archive(
        &self,
        descriptor: Option<&crate::source_archive::ArchiveDescriptor>,
        media: &Media,
        context: &Map<String, Value>,
        root: &Path,
        directory: &Path,
    ) {
        let Some(config) = &self.archive_config else {
            return;
        };
        let mut state = self.archive_state.lock().await;
        if !matches!(*state, ArchiveState::Uninitialized) {
            return;
        }
        if config.uses_source_format()
            && !self.stdout
            && !matches!(self.mode, Mode::Modify(_) | Mode::Delete(_))
            && descriptor.is_some_and(|d| !crate::source_archive::default_keywords_supported(d))
        {
            eprintln!(
                "metadata archive initialization failed: source archive keywords are unavailable; configure archive-format explicitly"
            );
            *state = ArchiveState::Disabled;
            return;
        }
        let category = descriptor.map_or(media.category.as_str(), |d| d.category.as_str());
        let format = descriptor.map(|d| d.format.as_str());
        *state = match config
            .open(category, format, context, root, directory)
            .await
        {
            Ok(session) => ArchiveState::Open(session),
            Err(failure) => {
                eprintln!("metadata archive initialization failed: {failure}");
                ArchiveState::Disabled
            }
        };
    }
    pub(crate) async fn close_archive(&self) -> Result<()> {
        let mut state = self.archive_state.lock().await;
        let previous = std::mem::replace(&mut *state, ArchiveState::Closed);
        if let ArchiveState::Open(session) = previous {
            session.close().await?;
        }
        Ok(())
    }
    async fn archive_session(&self) -> Option<super::archive::ArchiveSession> {
        match &*self.archive_state.lock().await {
            ArchiveState::Open(session) => Some(session.clone()),
            _ => None,
        }
    }
    pub fn with_filename_template(
        mut self,
        template: Option<crate::naming::FilenameTemplate>,
    ) -> Self {
        self.filename_template = template;
        self
    }
    pub async fn run(
        &self,
        context: &mut Map<String, Value>,
        media: &Media,
        path: &Path,
        root: &Path,
    ) -> Result<()> {
        self.run_with_source_extension(context, media, path, root, None)
            .await
    }
    pub(crate) async fn run_with_source_extension(
        &self,
        context: &mut Map<String, Value>,
        media: &Media,
        path: &Path,
        root: &Path,
        source_extension: Option<&Value>,
    ) -> Result<()> {
        let extension = path.extension().unwrap_or_default().to_string_lossy();
        self.run_destination(
            context,
            media,
            DestinationState {
                directory: path.parent().unwrap_or(root),
                filename: path.file_name(),
                extension: &extension,
                source_extension,
            },
            root,
        )
        .await
    }
    pub(crate) async fn run_job(
        &self,
        context: &mut Map<String, Value>,
        media: &Media,
        paths: &crate::postprocess::HookPaths,
        root: &Path,
    ) -> Result<()> {
        self.run_destination(
            context,
            media,
            DestinationState {
                directory: &paths.directory,
                filename: paths.filename.as_deref(),
                extension: &paths.extension,
                source_extension: paths.source_extension.as_ref(),
            },
            root,
        )
        .await
    }
    async fn run_destination(
        &self,
        context: &mut Map<String, Value>,
        media: &Media,
        paths: DestinationState<'_>,
        root: &Path,
    ) -> Result<()> {
        let descriptor = crate::source_archive::for_media(media);
        self.initialize_archive(descriptor.as_ref(), media, context, root, paths.directory)
            .await;
        match &self.mode {
            Mode::Modify(fields) => {
                for (key, template) in fields {
                    let popular = media
                        .metadata
                        .as_ref()
                        .is_some_and(|metadata| metadata.booru_popular.is_some());
                    let alias = template
                        .identity_path()
                        .and_then(|path| super::objects::resolve_alias(&path));
                    // Source modify retains the value's type, including an
                    // existing producer datetime copied to an arbitrary key.
                    let result = template.render_source_value(context);
                    if let Ok(mut value) = result {
                        if key.split('[').next().is_some_and(|root| {
                            root == "_parent" || super::objects::is_artist_root(root)
                        }) && let Some(raw) = crate::filter::source_primitive_from_json(&value)
                        {
                            value = raw.clone();
                        }
                        if matches!(key.as_str(), "date" | "date_updated" | "date_url")
                            || popular && key == "scale"
                        {
                            value = crate::filter::source_primitive_json(value);
                        }
                        if mutate(context, key, Some(value)) {
                            if let Some(alias) = alias {
                                super::objects::assign_alias(key, alias);
                            }
                            super::objects::refresh_artist(context);
                        }
                    }
                }
                return Ok(());
            }
            Mode::Delete(fields) => {
                for key in fields {
                    if mutate(context, key, None) {
                        super::objects::refresh_artist(context);
                    }
                }
                return Ok(());
            }
            _ => (),
        }
        if self.stdout {
            let content = self.content(context)?;
            let mut out = tokio::io::stdout();
            out.write_all(content.as_bytes())
                .await
                .map_err(|_| error("could not write metadata output"))?;
            return Ok(());
        }
        let archive = self.archive_session().await;
        let check = if let Some(archive) = &archive {
            let check = archive.check(context).await?;
            if check.hit {
                return Ok(());
            }
            Some(check)
        } else {
            None
        };
        // Retain the per-key execution lease until the write and add finish.
        let _archive_guard = check.map(|check| check.guard);
        let destination = self.destination(context, media, &paths, root)?;
        // Source restores kwdict.get("extension") after formatting, inserting
        // None even when the key was initially absent.
        if self.filename.is_none() && self.extension_format.is_some() {
            context.entry("extension".to_owned()).or_insert(Value::Null);
        }
        if let Some(key) = &self.meta_path {
            context.insert(
                key.clone(),
                Value::String(destination.to_string_lossy().into_owned()),
            );
            super::objects::record(&[key], context);
        }
        if self.skip
            && tokio::fs::try_exists(&destination)
                .await
                .map_err(|_| error("could not inspect metadata path"))?
        {
            return Ok(());
        }
        let mut content = self.content(context)?;
        if content.is_empty() && !self.empty {
            if let Some(archive) = &archive {
                archive.add(context).await?;
            }
            return Ok(());
        }
        if self.encoding_ascii && !content.is_ascii() {
            return Err(error("metadata output is not ASCII"));
        }
        if let Some(newline) = &self.newline
            && !matches!(newline.as_str(), "" | "\n")
        {
            let extra = if newline.len() > 1 {
                content.bytes().filter(|b| *b == b'\n').count()
            } else {
                0
            };
            if content.len().saturating_add(extra) > LIMIT {
                return Err(error("metadata output exceeds supported limit"));
            }
            content = content.replace('\n', newline);
        }
        let directory = destination
            .parent()
            .ok_or_else(|| error("invalid metadata output path"))?;
        tokio::fs::create_dir_all(directory)
            .await
            .map_err(|_| error("could not create metadata directory"))?;
        if self.append {
            // Keep record writes serialized even if a caller drops its future.
            static LOCK: OnceLock<Arc<Mutex<()>>> = OnceLock::new();
            append_record(
                destination.clone(),
                content,
                LOCK.get_or_init(|| Arc::new(Mutex::new(()))).clone(),
            )
            .await?;
        } else {
            let temporary = tempfile::NamedTempFile::new_in(directory)
                .map_err(|_| error("could not stage metadata output"))?;
            // Open the handle before yielding: a cancelled write cannot recreate a
            // staging path after NamedTempFile's drop removes it.
            let handle = temporary
                .reopen()
                .map_err(|_| error("could not open metadata staging file"))?;
            let mut handle = tokio::fs::File::from_std(handle);
            handle
                .write_all(content.as_bytes())
                .await
                .map_err(|_| error("could not write metadata output"))?;
            handle
                .flush()
                .await
                .map_err(|_| error("could not flush metadata output"))?;
            if self.exclusive {
                temporary
                    .persist_noclobber(&destination)
                    .map_err(|_| error("could not publish metadata output"))?;
            } else {
                temporary
                    .persist(&destination)
                    .map_err(|_| error("could not publish metadata output"))?;
            }
        }
        if self.mtime
            && let Some(timestamp) = context
                .get("_mtime_meta")
                .filter(|v| truthy(v))
                .or_else(|| context.get("_mtime_http").filter(|v| truthy(v)))
                .and_then(Value::as_f64)
        {
            let file = open_regular_output(&destination, false, false)?;
            let duration = std::time::Duration::try_from_secs_f64(timestamp.abs())
                .map_err(|_| error("invalid metadata time"))?;
            let time = if timestamp >= 0.0 {
                std::time::UNIX_EPOCH.checked_add(duration)
            } else {
                std::time::UNIX_EPOCH.checked_sub(duration)
            };
            file.set_times(
                std::fs::FileTimes::new()
                    .set_modified(time.ok_or_else(|| error("invalid metadata time"))?),
            )
            .map_err(|_| error("could not set metadata time"))?;
        }
        if let Some(archive) = &archive {
            archive.add(context).await?;
        }
        Ok(())
    }
    fn destination(
        &self,
        context: &Map<String, Value>,
        media: &Media,
        paths: &DestinationState<'_>,
        root: &Path,
    ) -> Result<PathBuf> {
        let base = self.base.as_deref().unwrap_or(if self.root_base {
            root
        } else {
            paths.directory
        });
        let dir = match &self.directory {
            Directory::Default => base.to_owned(),
            Directory::Literal(p) => base.join(p),
            Directory::Formatted(parts) => {
                let mut p = base.to_owned();
                for part in parts {
                    let s = clean(&part.render(context)?);
                    if !s.is_empty() {
                        p.push(s);
                    }
                }
                p
            }
        };
        let filename = if let Some(t) = &self.filename {
            clean(&t.render(context)?)
        } else if let Some(t) = &self.extension_format {
            let mut keywords = context.clone();
            // Source substitutes the current path extension while formatting,
            // then rebuilds the extractor's configured filename.
            keywords.insert(
                "extension".into(),
                paths
                    .source_extension
                    .cloned()
                    .unwrap_or_else(|| Value::String(paths.extension.into())),
            );
            let rendered = t.render(&keywords)?;
            keywords.insert("extension".into(), Value::String(rendered.clone()));
            if let Some(template) = &self.filename_template {
                if let Some(filename) = template.render_keywords(media, &keywords)? {
                    clean(&filename)
                } else {
                    format!(
                        "{}.{}",
                        paths
                            .filename
                            .and_then(|f| Path::new(f).file_stem())
                            .unwrap_or_else(|| std::ffi::OsStr::new("metadata"))
                            .to_string_lossy(),
                        clean(&rendered)
                    )
                }
            } else {
                format!(
                    "{}.{}",
                    paths
                        .filename
                        .and_then(|f| Path::new(f).file_stem())
                        .unwrap_or_else(|| std::ffi::OsStr::new("metadata"))
                        .to_string_lossy(),
                    clean(&rendered)
                )
            }
        } else {
            format!(
                "{}.{}",
                paths
                    .filename
                    .filter(|f| !f.is_empty())
                    .unwrap_or_else(|| std::ffi::OsStr::new("metadata"))
                    .to_string_lossy(),
                self.extension
            )
        };
        if filename.is_empty() || filename.len() > 4096 {
            return Err(error("invalid metadata filename"));
        }
        Ok(dir.join(filename))
    }
    fn content(&self, context: &Map<String, Value>) -> Result<String> {
        validate_context(context)?;
        let out = match &self.mode {
            Mode::Custom(t) => t.render(context)?,
            Mode::Tags => tags(context)?,
            Mode::Json => {
                let mut m = Map::new();
                if let Some(include) = &self.include {
                    for key in include {
                        if let Some(v) = context.get(key) {
                            m.insert(key.clone(), v.clone());
                        }
                    }
                } else {
                    for (k, v) in context {
                        if (self.private || !k.starts_with('_')) && !self.exclude.contains(k) {
                            m.insert(k.clone(), v.clone());
                        }
                    }
                }
                if m.is_empty() {
                    String::new()
                } else {
                    let mut out = String::new();
                    self.json(
                        &Value::Object(crate::postprocess::public_keywords(&m)),
                        0,
                        &mut out,
                    )?;
                    out.push('\n');
                    out
                }
            }
            _ => String::new(),
        };
        if out.len() > LIMIT {
            return Err(error("metadata output exceeds supported limit"));
        }
        Ok(out)
    }
    fn json(&self, v: &Value, depth: usize, out: &mut String) -> Result<()> {
        if depth > 128 || out.len() > LIMIT {
            return Err(error("metadata output exceeds supported limit"));
        }
        match v {
            Value::Object(m) => {
                out.push('{');
                let mut pairs = m.iter().collect::<Vec<_>>();
                if self.sort {
                    pairs.sort_by(|a, b| a.0.cmp(b.0));
                }
                for (i, (k, v)) in pairs.iter().enumerate() {
                    if i > 0 {
                        out.push_str(&self.separators.0);
                    }
                    self.line(depth + 1, out);
                    out.push_str(&quoted(k, self.ascii)?);
                    out.push_str(&self.separators.1);
                    self.json(v, depth + 1, out)?;
                }
                if !m.is_empty() {
                    self.line(depth, out);
                }
                out.push('}');
            }
            Value::Array(a) => {
                out.push('[');
                for (i, v) in a.iter().enumerate() {
                    if i > 0 {
                        out.push_str(&self.separators.0);
                    }
                    self.line(depth + 1, out);
                    self.json(v, depth + 1, out)?;
                }
                if !a.is_empty() {
                    self.line(depth, out);
                }
                out.push(']');
            }
            Value::String(s) => out.push_str(&quoted(s, self.ascii)?),
            _ => out.push_str(&v.to_string()),
        }
        if out.len() > LIMIT {
            return Err(error("metadata output exceeds supported limit"));
        }
        Ok(())
    }
    fn line(&self, depth: usize, out: &mut String) {
        if let Some(indent) = &self.indent {
            out.push('\n');
            for _ in 0..depth {
                out.push_str(indent);
            }
        }
    }
}
struct CancelQueuedAppend(Arc<AtomicBool>);
impl Drop for CancelQueuedAppend {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}
fn open_regular_output(path: &Path, append: bool, create: bool) -> Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).append(append).create(create);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .mode(0o600);
    }
    let file = options
        .open(path)
        .map_err(|_| error("could not open metadata output"))?;
    if !file
        .metadata()
        .map_err(|_| error("could not inspect metadata output"))?
        .is_file()
    {
        return Err(error("metadata output is not a regular file"));
    }
    Ok(file)
}
async fn append_record(destination: PathBuf, content: String, lock: Arc<Mutex<()>>) -> Result<()> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let _cancel = CancelQueuedAppend(cancelled.clone());
    tokio::task::spawn_blocking(move || -> Result<()> {
        use std::io::Write;
        let _guard = lock
            .lock()
            .map_err(|_| error("metadata append lock failed"))?;
        // A dropped future must not create or append a file after waiting for
        // another writer. Once this commit starts, finish the complete record
        // under the same lock to prevent partial/interleaved JSONL records.
        if cancelled.load(Ordering::Acquire) {
            return Err(error("metadata append cancelled"));
        }
        let mut file = open_regular_output(&destination, true, true)?;
        file.write_all(content.as_bytes())
            .map_err(|_| error("could not write metadata output"))?;
        file.flush()
            .map_err(|_| error("could not flush metadata output"))
    })
    .await
    .map_err(|_| error("metadata append worker failed"))?
}
fn validate_context(context: &Map<String, Value>) -> Result<()> {
    if context.len() > 1_000_000 {
        return Err(error("metadata context exceeds supported limits"));
    }
    let mut stack = context
        .iter()
        .map(|(key, value)| (key.as_str(), value, 0usize))
        .collect::<Vec<_>>();
    let mut bytes = 0usize;
    let mut nodes = 0usize;
    while let Some((key, value, depth)) = stack.pop() {
        nodes += 1;
        bytes = bytes.saturating_add(key.len()).saturating_add(match value {
            Value::String(s) => s.len(),
            Value::Number(n) => n.to_string().len(),
            _ => 0,
        });
        if bytes > LIMIT || nodes > 1_000_000 || depth > 128 {
            return Err(error("metadata context exceeds supported limits"));
        }
        let children = match value {
            Value::Object(m) => m.len(),
            Value::Array(a) => a.len(),
            _ => 0,
        };
        if children.saturating_add(stack.len()).saturating_add(nodes) > 1_000_000 {
            return Err(error("metadata context exceeds supported limits"));
        }
        match value {
            Value::Object(m) => stack.extend(m.iter().map(|(k, v)| (k.as_str(), v, depth + 1))),
            Value::Array(a) => stack.extend(a.iter().map(|v| ("", v, depth + 1))),
            _ => (),
        }
    }
    Ok(())
}
fn bounded_key(s: &str) -> Result<String> {
    if s.len() > 4096 || s.contains('\0') {
        Err(fail())
    } else {
        Ok(s.into())
    }
}
fn list(v: Option<&Value>) -> Result<Option<Vec<String>>> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => {
            let v = s.split(',').map(bounded_key).collect::<Result<Vec<_>>>()?;
            if v.len() > 4096 {
                return Err(fail());
            }
            Ok(Some(v))
        }
        Some(Value::Array(v)) if v.len() <= 4096 => Ok(Some(
            v.iter()
                .map(|v| bounded_key(v.as_str().ok_or_else(fail)?))
                .collect::<Result<_>>()?,
        )),
        _ => Err(fail()),
    }
}
/// Python's `os.path.expanduser`: `HOME` on POSIX; `USERPROFILE`, then
/// `HOMEDRIVE` + `HOMEPATH` on Windows.
fn home_directory() -> Option<String> {
    if cfg!(windows) {
        std::env::var("USERPROFILE")
            .ok()
            .or_else(|| Some(std::env::var("HOMEDRIVE").ok()? + &std::env::var("HOMEPATH").ok()?))
    } else {
        std::env::var("HOME").ok()
    }
}
pub(crate) fn expand(s: &str) -> Result<PathBuf> {
    if s.len() > 4096 || s.contains('\0') {
        return Err(fail());
    }
    let s = if s == "~" || s.starts_with("~/") || (cfg!(windows) && s.starts_with("~\\")) {
        let home = home_directory().ok_or_else(fail)?;
        if s == "~" {
            home
        } else {
            format!("{home}/{}", &s[2..])
        }
    } else {
        s.to_owned()
    };
    let mut result = String::new();
    let mut rest = s.as_str();
    while let Some(at) = rest.find('$') {
        result.push_str(&rest[..at]);
        rest = &rest[at..];
        let end = if rest.starts_with("${") {
            rest.find('}').map(|n| n + 1)
        } else {
            let n = rest[1..]
                .bytes()
                .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_')
                .count();
            (n > 0).then_some(n + 1)
        };
        if let Some(end) = end {
            let token = &rest[..end];
            let key = token
                .strip_prefix("${")
                .and_then(|s| s.strip_suffix('}'))
                .unwrap_or(&token[1..]);
            result.push_str(&std::env::var(key).unwrap_or_else(|_| token.to_owned()));
            rest = &rest[end..];
        } else {
            result.push('$');
            rest = &rest[1..];
        }
        if result.len() > 4096 {
            return Err(fail());
        }
    }
    result.push_str(rest);
    if result.len() > 4096 || result.contains('\0') {
        return Err(fail());
    }
    Ok(result.into())
}
fn resolve(base: &Path, s: &str) -> Result<PathBuf> {
    let p = expand(s)?;
    Ok(if p.is_absolute() { p } else { base.join(p) })
}
fn clean(s: &str) -> String {
    let value = s
        .chars()
        .map(|c| {
            if c == '/' || c == '\\' || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect::<String>();
    if matches!(value.as_str(), "." | "..") {
        "_".into()
    } else {
        value
    }
}
fn quoted(s: &str, ascii: bool) -> Result<String> {
    use std::fmt::Write;
    let mut out = String::from("\"");
    for c in s.chars() {
        if out.len() > LIMIT.saturating_sub(12) {
            return Err(error("metadata output exceeds supported limit"));
        }
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if c < ' ' => write!(out, "\\u{:04x}", c as u32).unwrap(),
            c if ascii && !c.is_ascii() => {
                let mut buffer = [0; 2];
                for unit in c.encode_utf16(&mut buffer) {
                    write!(out, "\\u{unit:04x}").unwrap();
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
    Ok(out)
}
fn tags(context: &Map<String, Value>) -> Result<String> {
    let Some(value) = context
        .get("tags")
        .filter(|v| truthy(v))
        .or_else(|| context.get("tag_string").filter(|v| truthy(v)))
    else {
        return Ok(String::new());
    };
    let value = crate::filter::source_primitive_from_json(value).unwrap_or(value);
    let tags = match value {
        Value::String(s) => {
            let comma = s.split(", ").collect::<Vec<_>>();
            if (comma.len() as f64) < s.chars().count() as f64 / 16.0 {
                s.split(' ').map(str::to_owned).collect()
            } else {
                comma.into_iter().map(str::to_owned).collect()
            }
        }
        Value::Array(a) if a.first().is_some_and(Value::is_object) => {
            let mut out = Vec::new();
            for v in a {
                let m = v
                    .as_object()
                    .ok_or_else(|| error("invalid metadata tags"))?;
                out.extend(m.values().filter_map(Value::as_str).map(str::to_owned));
            }
            out.sort();
            out
        }
        Value::Array(a) => a
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| error("invalid metadata tags"))
            })
            .collect::<Result<_>>()?,
        _ => return Err(error("invalid metadata tags")),
    };
    if tags.is_empty() {
        Ok(String::new())
    } else {
        Ok(tags.join("\n") + "\n")
    }
}
fn mutate(context: &mut Map<String, Value>, path: &str, value: Option<Value>) -> bool {
    let mut keys = path
        .split('[')
        .map(|s| s.trim_end_matches(']').trim_matches(['\'', '"']));
    let Some(first) = keys.next() else {
        return false;
    };
    let keys = std::iter::once(first).chain(keys).collect::<Vec<_>>();
    fn apply(map: &mut Map<String, Value>, keys: &[&str], value: Option<Value>) -> bool {
        if keys.len() == 1 {
            if let Some(v) = value {
                map.insert(keys[0].into(), v);
                true
            } else {
                map.remove(keys[0]).is_some()
            }
        } else if let Some(inner) = map.get_mut(keys[0]) {
            let inner = if crate::filter::source_primitive_from_json(inner).is_some() {
                inner
                    .as_object_mut()
                    .and_then(|map| map.values_mut().next())
                    .and_then(Value::as_object_mut)
            } else {
                inner.as_object_mut()
            };
            inner.is_some_and(|inner| apply(inner, &keys[1..], value))
        } else {
            false
        }
    }
    if keys.len() <= 128 && apply(context, &keys, value) {
        super::objects::record(&keys, context);
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn options(v: Value) -> MetadataOptions {
        MetadataOptions::parse(v.as_object().unwrap(), Path::new(".")).unwrap()
    }
    fn context(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }
    fn media() -> Media {
        Media {
            url: "https://example.org/file.jpg".parse().unwrap(),
            category: "test".into(),
            filename: "file.jpg".into(),
            metadata: None,
            job: crate::extract::MediaJob::DirectHttp,
            headers: Default::default(),
            handoff: Default::default(),
        }
    }
    #[test]
    fn json_filtering_ascii_and_separators_match_python_encoder() {
        let c = context(json!({"z":"é😀","_secret":1,"nested":{"_kept":2},"tags":["a","b"]}));
        let out = options(json!({})).content(&c).unwrap();
        assert!(!out.contains("_secret"));
        assert!(out.contains("_kept"));
        assert!(out.ends_with("\n"));
        assert_eq!(options(json!({"mode":"jsonl","include":["_secret","z"],"ascii":true,"sort":true,"separators":[",",":"]})).content(&c).unwrap(),"{\"_secret\":1,\"z\":\"\\u00e9\\ud83d\\ude00\"}\n");
        assert_eq!(
            options(json!({"include":"","exclude":"z,tags"}))
                .content(&c)
                .unwrap(),
            "{\n    \"nested\": {\n        \"_kept\": 2\n    }\n}\n"
        );
        assert_eq!(
            options(json!({"include":["absent"]})).content(&c).unwrap(),
            ""
        );
    }
    #[test]
    fn tags_original_dictionary_sort_and_string_heuristic() {
        assert_eq!(options(json!({"mode":"tags"})).content(&context(json!({"tags":[{"name":"猫","translated_name":"cat"},{"name":"cat","extra":false}]}))).unwrap(),"cat\ncat\n猫\n");
        assert_eq!(
            options(json!({"mode":"tags"}))
                .content(&context(json!({"tag_string":"one, two"})))
                .unwrap(),
            "one\ntwo\n"
        );
        assert_eq!(
            options(json!({"mode":"tags"}))
                .content(&context(
                    json!({"tags":"long_tag_one long_tag_two long_tag_three"})
                ))
                .unwrap(),
            "long_tag_one\nlong_tag_two\nlong_tag_three\n"
        );
    }
    #[tokio::test]
    async fn sequential_modify_delete_typed_values_and_missing_nested_paths() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("file.jpg");
        let mut c = context(json!({"id":7,"nested":{"x":2},"array":[1,2]}));
        options(json!({"mode":"modify","fields":{"copy":"{array}","id":"{id}","nested[x]":"{id}","later":"{nested[x]}"}})).run(&mut c,&media(),&path,temp.path()).await.unwrap();
        assert_eq!(c["copy"], json!([1, 2]));
        assert_eq!(c["later"], 7);
        assert_eq!(c["nested"]["x"], 7);
        options(json!({"mode":"delete","fields":["nested['x']","missing[x]","copy"]}))
            .run(&mut c, &media(), &path, temp.path())
            .await
            .unwrap();
        assert!(c["nested"].as_object().unwrap().is_empty());
        assert!(!c.contains_key("copy"));
        assert!(!path.exists());
    }
    #[tokio::test]
    async fn files_paths_skip_empty_newlines_and_metadata_path_order() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("file.jpg");
        let mut c = context(json!({"id":7,"name":"a/b"}));
        let o = options(
            json!({"mode":"custom","format":["{id}","{name}"],"directory":["sidecars","{name}"],"filename":"{id}.txt","newline":"\r\n","metadata-path":"sidecar"}),
        );
        o.run(&mut c, &media(), &path, temp.path()).await.unwrap();
        let output = temp.path().join("sidecars").join("a_b").join("7.txt");
        assert_eq!(std::fs::read_to_string(&output).unwrap(), "7\r\na/b\r\n");
        assert_eq!(c["sidecar"], output.to_string_lossy().as_ref());
        let o = options(
            json!({"mode":"custom","format":"new","directory":"sidecars/a_b","filename":"7.txt","skip":true}),
        );
        o.run(&mut c, &media(), &path, temp.path()).await.unwrap();
        assert_eq!(std::fs::read_to_string(output).unwrap(), "7\r\na/b\r\n");
        options(json!({"mode":"tags"}))
            .run(&mut c, &media(), &path, temp.path())
            .await
            .unwrap();
        assert!(!temp.path().join("file.jpg.txt").exists());
        options(json!({"mode":"tags","empty":true}))
            .run(&mut c, &media(), &path, temp.path())
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(temp.path().join("file.jpg.txt")).unwrap(),
            b""
        );
        options(json!({"extension-format":"info.json"}))
            .run(&mut c, &media(), &path, temp.path())
            .await
            .unwrap();
        assert!(temp.path().join("file.info.json").exists());
        options(json!({"extension-format":"{extension}.info"}))
            .with_filename_template(Some(
                crate::naming::FilenameTemplate::parse("{name}.{extension}").unwrap(),
            ))
            .run(&mut c, &media(), &path, temp.path())
            .await
            .unwrap();
        assert!(temp.path().join("a_b.jpg.info").exists());
    }
    #[tokio::test]
    async fn concurrent_jsonl_appends_are_complete_records_and_exclusive_output_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let mut tasks = tokio::task::JoinSet::new();
        for id in 0..32 {
            let root = temp.path().to_owned();
            tasks.spawn(async move {
                let mut c = context(json!({"id":id,"data":"x".repeat(4096)}));
                options(json!({"mode":"jsonl"}))
                    .run(&mut c, &media(), &root.join("file.jpg"), &root)
                    .await
                    .unwrap();
            });
        }
        while let Some(r) = tasks.join_next().await {
            r.unwrap();
        }
        let data = std::fs::read_to_string(temp.path().join("data.jsonl")).unwrap();
        assert_eq!(data.lines().count(), 32);
        for line in data.lines() {
            assert!(serde_json::from_str::<Value>(line).unwrap()["id"].is_number());
        }
        let mut c = context(json!({"id":1}));
        let p = temp.path().join("file.jpg");
        let o = options(json!({"open":"x"}));
        o.run(&mut c, &media(), &p, temp.path()).await.unwrap();
        let original = std::fs::read(temp.path().join("file.jpg.json")).unwrap();
        c.insert("id".into(), json!(2));
        assert!(o.run(&mut c, &media(), &p, temp.path()).await.is_err());
        assert_eq!(
            std::fs::read(temp.path().join("file.jpg.json")).unwrap(),
            original
        );
    }
    #[tokio::test]
    async fn metadata_mtime_uses_private_hook_timestamp_precedence() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("file.jpg");
        let o = options(json!({"mtime":true}));
        let mut c = context(json!({"id":1,"_mtime_meta":1700000000.25,"_mtime_http":1600000000}));
        o.run(&mut c, &media(), &path, temp.path()).await.unwrap();
        let timestamp = std::fs::metadata(temp.path().join("file.jpg.json"))
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();
        assert_eq!(timestamp.as_secs(), 1700000000);
        assert_eq!(timestamp.subsec_nanos(), 250000000);
        c.insert("_mtime_meta".into(), json!(0));
        o.run(&mut c, &media(), &path, temp.path()).await.unwrap();
        assert_eq!(
            std::fs::metadata(temp.path().join("file.jpg.json"))
                .unwrap()
                .modified()
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            1600000000
        );
    }
    #[tokio::test]
    async fn cancelled_queued_append_never_creates_or_modifies_output_after_release() {
        for existing in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let destination = temp.path().join("data.jsonl");
            if existing {
                std::fs::write(&destination, b"existing\n").unwrap();
            }
            let lock = Arc::new(Mutex::new(()));
            let held = lock.clone();
            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let holder = std::thread::spawn(move || {
                let _guard = held.lock().unwrap();
                ready_tx.send(()).unwrap();
                release_rx
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .unwrap();
            });
            ready_rx
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap();
            let mut append = Box::pin(append_record(
                destination.clone(),
                "late record\n".into(),
                lock.clone(),
            ));
            assert!(futures_util::poll!(append.as_mut()).is_pending());
            drop(append);
            release_tx.send(()).unwrap();
            holder.join().unwrap();
            // The worker's lock reference outlives its whole write attempt.
            // Waiting for it to disappear proves this assertion cannot race a
            // worker that has not reached the canceled-state check yet.
            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                while Arc::strong_count(&lock) != 1 {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("cancelled append worker remained queued");
            if existing {
                assert_eq!(std::fs::read(destination).unwrap(), b"existing\n");
            } else {
                assert!(!destination.exists());
            }
        }
    }
    #[test]
    fn configured_paths_expand_home_and_environment_without_shell_execution() {
        assert_eq!(
            expand("~").unwrap(),
            PathBuf::from(home_directory().unwrap())
        );
        // Windows runners do not necessarily define HOME.
        if let Ok(home) = std::env::var("HOME") {
            assert_eq!(
                expand("${HOME}/sidecars").unwrap(),
                PathBuf::from(&home).join("sidecars")
            );
            assert_eq!(
                expand("$HOME/sidecars").unwrap(),
                PathBuf::from(&home).join("sidecars")
            );
        } else if !cfg!(windows) {
            panic!("HOME is not set");
        }
        assert_eq!(
            expand("$(uname)/literal").unwrap(),
            PathBuf::from("$(uname)/literal")
        );
        assert_eq!(
            expand("${unfinished").unwrap(),
            PathBuf::from("${unfinished")
        );
    }
    #[test]
    fn unsupported_settings_fail_without_private_values_and_debug_is_redacted() {
        for v in [
            json!({"archive":"postgresql://private-path"}),
            json!({"encoding":"private-encoding"}),
            json!({"open":"private-mode"}),
            json!({"filename":"private-{bad"}),
        ] {
            let e = MetadataOptions::parse(v.as_object().unwrap(), Path::new(".")).unwrap_err();
            assert!(!e.to_string().contains("private-"));
        }
        assert!(
            !format!("{:?}", options(json!({"format":"private-format"})))
                .contains("private-format")
        );
    }
}

#[cfg(test)]
mod source_oracle {
    use super::*;
    // Generated from actual gallery-dl 1.32.17 MetadataPP methods, no network.
    // metadata.py SHA256: db7481267bbdbca3ed95ace30dcb05f3cec5511a84f75f1769a0362ae0fb53a5
    #[tokio::test]
    async fn actual_upstream_mode_vectors_match() {
        let cases: Value=serde_json::from_str(r###"[{"options": {}, "context": {"id": 7, "name": "猫😀", "_hidden": "private", "nested": {"_kept": true}, "tags": ["a", "b"]}, "expected": "{\n    \"id\": 7,\n    \"name\": \"猫😀\",\n    \"nested\": {\n        \"_kept\": true\n    },\n    \"tags\": [\n        \"a\",\n        \"b\"\n    ]\n}\n"}, {"options": {"mode": "jsonl"}, "context": {"id": 7, "name": "猫😀", "_hidden": "private", "nested": {"_kept": true}, "tags": ["a", "b"]}, "expected": "{\"id\": 7, \"name\": \"猫😀\", \"nested\": {\"_kept\": true}, \"tags\": [\"a\", \"b\"]}\n"}, {"options": {"ascii": true, "sort": true}, "context": {"id": 7, "name": "猫😀", "_hidden": "private", "nested": {"_kept": true}, "tags": ["a", "b"]}, "expected": "{\n    \"id\": 7,\n    \"name\": \"\\u732b\\ud83d\\ude00\",\n    \"nested\": {\n        \"_kept\": true\n    },\n    \"tags\": [\n        \"a\",\n        \"b\"\n    ]\n}\n"}, {"options": {"indent": 0}, "context": {"id": 7, "name": "猫😀", "_hidden": "private", "nested": {"_kept": true}, "tags": ["a", "b"]}, "expected": "{\n\"id\": 7,\n\"name\": \"猫😀\",\n\"nested\": {\n\"_kept\": true\n},\n\"tags\": [\n\"a\",\n\"b\"\n]\n}\n"}, {"options": {"indent": "--", "separators": ["|", "="]}, "context": {"id": 7, "name": "猫😀", "_hidden": "private", "nested": {"_kept": true}, "tags": ["a", "b"]}, "expected": "{\n--\"id\"=7|\n--\"name\"=\"猫😀\"|\n--\"nested\"={\n----\"_kept\"=true\n--}|\n--\"tags\"=[\n----\"a\"|\n----\"b\"\n--]\n}\n"}, {"options": {"include": ["name", "_hidden"]}, "context": {"id": 7, "name": "猫😀", "_hidden": "private", "nested": {"_kept": true}, "tags": ["a", "b"]}, "expected": "{\n    \"name\": \"猫😀\",\n    \"_hidden\": \"private\"\n}\n"}, {"options": {"include": "", "exclude": "name,tags"}, "context": {"id": 7, "name": "猫😀", "_hidden": "private", "nested": {"_kept": true}, "tags": ["a", "b"]}, "expected": "{\n    \"id\": 7,\n    \"nested\": {\n        \"_kept\": true\n    }\n}\n"}, {"options": {"exclude": ["_hidden"], "private": true}, "context": {"id": 7, "name": "猫😀", "_hidden": "private", "nested": {"_kept": true}, "tags": ["a", "b"]}, "expected": "{\n    \"id\": 7,\n    \"name\": \"猫😀\",\n    \"nested\": {\n        \"_kept\": true\n    },\n    \"tags\": [\n        \"a\",\n        \"b\"\n    ]\n}\n"}, {"options": {"include": ["missing"]}, "context": {"id": 7, "name": "猫😀", "_hidden": "private", "nested": {"_kept": true}, "tags": ["a", "b"]}, "expected": ""}, {"options": {"private": true}, "context": {"id": 7, "name": "猫😀", "_hidden": "private", "nested": {"_kept": true}, "tags": ["a", "b"]}, "expected": "{\n    \"id\": 7,\n    \"name\": \"猫😀\",\n    \"_hidden\": \"private\",\n    \"nested\": {\n        \"_kept\": true\n    },\n    \"tags\": [\n        \"a\",\n        \"b\"\n    ]\n}\n"}, {"options": {"mode": "custom", "format": ["{id}", "{name}"]}, "context": {"id": 7, "name": "猫😀", "_hidden": "private", "nested": {"_kept": true}, "tags": ["a", "b"]}, "expected": "7\n猫😀\n"}, {"options": {"format": "{id} {name}"}, "context": {"id": 7, "name": "猫😀", "_hidden": "private", "nested": {"_kept": true}, "tags": ["a", "b"]}, "expected": "7 猫😀"}, {"options": {"mode": "print", "format": "{id}"}, "context": {"id": 7, "name": "猫😀", "_hidden": "private", "nested": {"_kept": true}, "tags": ["a", "b"]}, "expected": "7\n"}, {"options": {"mode": "print", "format": "\fX{id}"}, "context": {"id": 7, "name": "猫😀", "_hidden": "private", "nested": {"_kept": true}, "tags": ["a", "b"]}, "expected": ""}, {"options": {"mode": "print", "format": "\fF{id}"}, "context": {"id": 7, "name": "猫😀", "_hidden": "private", "nested": {"_kept": true}, "tags": ["a", "b"]}, "expected": ""}, {"options": {"mode": "modify", "fields": {"copy": "{tags}", "nested[x]": "{id}", "later": "{nested[x]}"}}, "context": {"id": 7, "name": "猫😀", "_hidden": "private", "nested": {"_kept": true}, "tags": ["a", "b"]}, "expected": {"id": 7, "name": "猫😀", "_hidden": "private", "nested": {"_kept": true, "x": 7}, "tags": ["a", "b"], "copy": ["a", "b"], "later": 7}}, {"options": {"mode": "delete", "fields": ["nested[_kept]", "missing[x]", "id"]}, "context": {"id": 7, "name": "猫😀", "_hidden": "private", "nested": {"_kept": true}, "tags": ["a", "b"]}, "expected": {"name": "猫😀", "_hidden": "private", "nested": {}, "tags": ["a", "b"]}}, {"options": {"mode": "tags"}, "context": {"tags": [{"name": "猫", "translated_name": "cat"}, {"name": "cat", "extra": false}]}, "expected": "cat\ncat\n猫\n"}, {"options": {"mode": "tags"}, "context": {"tag_string": "one, two"}, "expected": "one\ntwo\n"}, {"options": {"mode": "tags"}, "context": {"tags": "long_tag_one long_tag_two long_tag_three"}, "expected": "long_tag_one\nlong_tag_two\nlong_tag_three\n"}, {"options": {"mode": "tags"}, "context": {"tags": []}, "expected": ""}, {"options": {"mode": "tags"}, "context": {"tags": "a b"}, "expected": "a b\n"}, {"options": {"mode": "tags"}, "context": {"tags": ["x", "x"]}, "expected": "x\nx\n"}, {"options": {"mode": "tags"}, "context": {}, "expected": ""}]"###).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let media = Media {
            url: "https://example.org/file.jpg".parse().unwrap(),
            category: "test".into(),
            filename: "file.jpg".into(),
            metadata: None,
            job: crate::extract::MediaJob::DirectHttp,
            headers: Default::default(),
            handoff: Default::default(),
        };
        for case in cases.as_array().unwrap() {
            // Native templates deliberately reject executable/unknown formatter
            // selectors instead of treating them as literal sidecar content.
            if case["options"]["format"]
                .as_str()
                .is_some_and(|s| s.starts_with('\u{c}'))
            {
                assert!(
                    MetadataOptions::parse(case["options"].as_object().unwrap(), temp.path())
                        .is_err()
                );
                continue;
            }
            let options =
                MetadataOptions::parse(case["options"].as_object().unwrap(), temp.path()).unwrap();
            let mut context = case["context"].as_object().unwrap().clone();
            if matches!(options.mode, Mode::Modify(_) | Mode::Delete(_)) {
                options
                    .run(
                        &mut context,
                        &media,
                        &temp.path().join("file.jpg"),
                        temp.path(),
                    )
                    .await
                    .unwrap();
                assert_eq!(
                    Value::Object(context),
                    case["expected"],
                    "{:?}",
                    case["options"]
                );
            } else {
                assert_eq!(
                    options.content(&context).unwrap(),
                    case["expected"].as_str().unwrap(),
                    "{:?}",
                    case["options"]
                );
            }
        }
    }
    #[cfg(unix)]
    #[test]
    fn append_and_mtime_handles_reject_symlinks_without_changing_the_target() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target");
        std::fs::write(&target, b"sentinel").unwrap();
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(open_regular_output(&link, true, true).is_err());
        assert!(open_regular_output(&link, false, false).is_err());
        assert_eq!(std::fs::read(target).unwrap(), b"sentinel");
    }
}

#[cfg(test)]
mod job_path_oracle {
    use super::*;
    #[tokio::test]
    async fn explicit_job_paths_match_actual_source_metadata_destinations() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/postprocessors/metadata_paths.json"
        ))
        .unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            let temp = tempfile::tempdir().unwrap();
            let expand = |s: &str| PathBuf::from(s.replace("$ROOT", temp.path().to_str().unwrap()));
            let paths = crate::postprocess::HookPaths {
                directory: expand(case["paths"]["directory"].as_str().unwrap()),
                filename: case["paths"]["filename"]
                    .as_str()
                    .map(std::ffi::OsString::from),
                realpath: case["paths"]["realpath"].as_str().map(expand),
                temppath: None,
                extension: case["paths"]["extension"].as_str().unwrap().into(),
                source_extension: None,
            };
            let mut context = case["keywords"].as_object().unwrap().clone();
            let options = MetadataOptions::parse(case["options"].as_object().unwrap(), temp.path())
                .unwrap()
                .with_filename_template(Some(
                    crate::naming::FilenameTemplate::parse(
                        case["filename_template"].as_str().unwrap(),
                    )
                    .unwrap(),
                ));
            let media = Media {
                url: "https://fixture.invalid/".parse().unwrap(),
                category: "test".into(),
                filename: "post".into(),
                metadata: None,
                job: crate::extract::MediaJob::DirectHttp,
                headers: Default::default(),
                handoff: Default::default(),
            };
            options
                .run_job(&mut context, &media, &paths, temp.path())
                .await
                .unwrap();
            let expected = expand(case["expected_path"].as_str().unwrap());
            assert!(
                expected.is_file(),
                "{}: expected {}",
                case["name"],
                expected.display()
            );
            assert_eq!(
                Value::Object(context),
                case["keywords_after"],
                "{}",
                case["name"]
            );
            assert!(!temp.path().join("artist1/post.json").exists());
        }
    }
}

#[cfg(test)]
mod source_date_exports {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn artist_parent_raw_dates_stay_strings_but_explicit_conversion_retains_datetime() {
        let parent = crate::event::KeywordObject::new(
            json!({"date":"2024-01-01T00:04:05+05:00"})
                .as_object()
                .unwrap()
                .clone(),
        );
        let mut media =
            crate::extract::direct(&"https://fixture.invalid/image.jpg".parse().unwrap()).unwrap();
        media.metadata = Some(crate::extract::PostMetadata {
            booru_artist_parent: Some(parent.into()),
            ..Default::default()
        });
        let mut context = crate::postprocess::context(&media);
        let options = MetadataOptions::parse(
            json!({"mode":"modify","fields":{
                "date":"{_parent[date]}",
                "raw_copy":"{_parent[date]}",
                "converted":"{_parent[date]!D}",
                "year":"{converted:%Y}"
            }})
            .as_object()
            .unwrap(),
            Path::new("."),
        )
        .unwrap();
        options
            .run(&mut context, &media, Path::new("image.jpg"), Path::new("."))
            .await
            .unwrap();
        assert_eq!(
            crate::filter::source_date_from_json(&context["converted"]),
            Some(Some(1704049445))
        );
        assert_eq!(context["year"], "2023");
        let template = crate::postprocess::template::Template::parse("{date:%Y}").unwrap();
        assert!(template.render(&context).is_err());
        let public = crate::postprocess::public_keywords(&context);
        assert_eq!(public["date"], "2024-01-01T00:04:05+05:00");
        assert_eq!(public["raw_copy"], public["date"]);
        assert_eq!(public["converted"], "2023-12-31 19:04:05");
        let json_options = MetadataOptions::parse(
            json!({"mode":"json","include":["date","raw_copy","converted","year"]})
                .as_object()
                .unwrap(),
            Path::new("."),
        )
        .unwrap();
        let encoded = json_options.content(&context).unwrap();
        assert!(!encoded.contains("$gallery_dl_source"));
        let exported: Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(exported["converted"], "2023-12-31 19:04:05");
        assert_eq!(exported["raw_copy"], "2024-01-01T00:04:05+05:00");
    }

    #[tokio::test]
    async fn typed_dates_export_public_strings_and_hook_literal_strings_keep_their_type() {
        for (date, text, truthy) in [
            (Some(0), "1970-01-01 00:00:00", true),
            (None, "[Invalid DateTime]", false),
        ] {
            let source = crate::filter::source_date_json(date);
            let mut context = Map::from_iter([
                ("date_favorited".into(), source.clone()),
                ("nested".into(), json!([{"date":source}])),
            ]);
            let json_options =
                MetadataOptions::parse(json!({"mode":"json"}).as_object().unwrap(), Path::new("."))
                    .unwrap();
            let encoded = json_options.content(&context).unwrap();
            let exported: Value = serde_json::from_str(&encoded).unwrap();
            assert_eq!(exported["date_favorited"], text);
            assert_eq!(exported["nested"][0]["date"], text);
            assert!(!encoded.contains("$gallery_dl_source_date"));
            let modify=MetadataOptions::parse(json!({"mode":"modify","fields":{"copied":"{date_favorited}","date_favorited":"{'[Invalid DateTime]'}"}}).as_object().unwrap(),Path::new(".")).unwrap();
            let media = crate::extract::direct(
                &url::Url::parse("https://example.invalid/file.jpg").unwrap(),
            )
            .unwrap();
            modify
                .run(&mut context, &media, Path::new("file.jpg"), Path::new("."))
                .await
                .unwrap();
            let copied = crate::filter::FilterExpression::parse("bool(copied)").unwrap();
            assert_eq!(copied.matches_keywords(&context).unwrap(), truthy);
            assert!(
                crate::filter::FilterExpression::parse("bool(date_favorited)")
                    .unwrap()
                    .matches_keywords(&context)
                    .unwrap()
            );
            assert_eq!(context["date_favorited"], "[Invalid DateTime]");
            assert_eq!(
                crate::postprocess::public_keywords(&context)["copied"],
                text
            );
            assert_eq!(
                crate::postprocess::template::Template::parse("{copied:%Y}")
                    .unwrap()
                    .render(&context)
                    .unwrap(),
                if truthy { "1970" } else { "[Invalid DateTime]" }
            );
        }
    }
}

#[cfg(test)]
mod source_primitive_tests {
    use super::*;
    use crate::{filter::FilterExpression, postprocess::template::Template};
    use serde_json::json;
    use std::sync::Arc;

    #[tokio::test]
    async fn popular_strings_survive_copy_delete_and_iso_literal_replacement_until_explicit_conversion()
     {
        let root = tempfile::tempdir().unwrap();
        let mut media=crate::booru::parse_post(crate::booru::Site::Danbooru,&json!({"id":42,"file_url":"https://cdn.donmai.us/original/raw.jpg","md5":"0123456789abcdef0123456789abcdef"})).unwrap();
        let date = "2024-02-03T04:05:06+00:00";
        media.metadata.as_mut().unwrap().timestamp = Some(0);
        media.metadata.as_mut().unwrap().booru_popular = Some(Arc::new(Map::from_iter([
            ("date".into(), json!(date)),
            ("scale".into(), json!("day")),
        ])));
        let mut context = crate::postprocess::context(&media);
        assert_eq!(crate::postprocess::public_keywords(&context)["date"], date);
        assert!(!context.contains_key("num"));
        assert!(
            FilterExpression::parse("date.startswith('2024') and date[0] == '2'")
                .unwrap()
                .matches(&media)
                .unwrap()
        );
        assert_eq!(
            Template::parse("{date!s}")
                .unwrap()
                .render(&context)
                .unwrap(),
            date
        );
        assert_eq!(
            Template::parse("{date!r}")
                .unwrap()
                .render(&context)
                .unwrap(),
            format!("'{date}'")
        );
        assert_eq!(
            Template::parse("{scale[0]}")
                .unwrap()
                .render(&context)
                .unwrap(),
            "d"
        );
        assert!(
            Template::parse("{date:%Y}")
                .unwrap()
                .render(&context)
                .is_err()
        );
        assert!(
            crate::naming::FilenameTemplate::parse("{date:%Y}")
                .unwrap()
                .render(&media)
                .is_err()
        );
        let path = root.path().join("file.jpg");
        for options in [
            json!({"mode":"delete","fields":["date"]}),
            json!({"mode":"modify","fields":{"date":"1970-01-01T00:00:00+00:00","copy":"{date}","date_updated":"{date!s}"}}),
        ] {
            MetadataOptions::parse(options.as_object().unwrap(), root.path())
                .unwrap()
                .run(&mut context, &media, &path, root.path())
                .await
                .unwrap();
        }
        assert_eq!(
            crate::postprocess::public_keywords(&context)["date"],
            "1970-01-01T00:00:00+00:00"
        );
        for field in ["date", "copy", "date_updated"] {
            assert!(
                Template::parse(&format!("{{{field}:%Y}}"))
                    .unwrap()
                    .render(&context)
                    .is_err()
            );
        }
        let encoder =
            MetadataOptions::parse(json!({"mode":"json"}).as_object().unwrap(), root.path())
                .unwrap();
        assert!(
            !encoder
                .content(&context)
                .unwrap()
                .contains("$gallery_dl_source_")
        );
        let explicit = MetadataOptions::parse(
            json!({"mode":"modify","fields":{"date":"{date!D}"}})
                .as_object()
                .unwrap(),
            root.path(),
        )
        .unwrap();
        explicit
            .run(&mut context, &media, &path, root.path())
            .await
            .unwrap();
        assert_eq!(
            Template::parse("{date:%Y}")
                .unwrap()
                .render(&context)
                .unwrap(),
            "1970"
        );
        assert!(crate::filter::source_date_from_json(&context["date"]).is_some());
        context.insert("replacement".into(), json!({"keep":1,"remove":2}));
        context.insert("three".into(), json!(3));
        for options in [
            json!({"mode":"modify","fields":{"date":"{replacement}"}}),
            json!({"mode":"modify","fields":{"date[keep]":"{three}"}}),
            json!({"mode":"delete","fields":["date[remove]"]}),
        ] {
            MetadataOptions::parse(options.as_object().unwrap(), root.path())
                .unwrap()
                .run(&mut context, &media, &path, root.path())
                .await
                .unwrap();
        }
        assert_eq!(
            crate::postprocess::public_keywords(&context)["date"],
            json!({"keep":3})
        );
    }
}
