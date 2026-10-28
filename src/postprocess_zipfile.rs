//! Upstream `zip` postprocessor: store downloaded files in a ZIP archive named
//! after the job's download directory (`DIRECTORY.zip`).
//!
//! Each job gets fresh state (see `Postprocessors::for_job`). Unless
//! `keep-files` is set, stored files are not published and the then-empty
//! directory is removed when the job finalizes.
use super::truthy;
use crate::{Result, error};
use serde_json::{Map, Value};
use std::{
    collections::HashSet,
    fs::{File, OpenOptions},
    io::{BufReader, Seek},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use zip::{CompressionMethod, ZipArchive, ZipWriter, write::SimpleFileOptions};

#[derive(Default)]
struct State {
    /// Archive path, fixed by the first stored file's directory.
    path: Option<PathBuf>,
    writer: Option<ZipWriter<File>>,
    names: HashSet<String>,
    extra_files: bool,
}

#[derive(Clone)]
pub struct ZipOptions {
    delete: bool,
    extension: String,
    compression: CompressionMethod,
    safe: bool,
    files: Vec<String>,
    state: Arc<Mutex<State>>,
}
impl std::fmt::Debug for ZipOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZipOptions")
            .field("extension", &self.extension)
            .finish_non_exhaustive()
    }
}

impl ZipOptions {
    pub fn parse(options: &Map<String, Value>) -> Result<Self> {
        let extension = match options.get("extension") {
            None => "zip".to_owned(),
            Some(Value::String(value))
                if !value.is_empty() && value.len() <= 64 && !value.contains(['/', '\\', '\0']) =>
            {
                value.clone()
            }
            Some(_) => return Err(error("invalid zip extension")),
        };
        let compression = match options.get("compression").and_then(Value::as_str) {
            None | Some("store") => CompressionMethod::Stored,
            Some("zip") => CompressionMethod::Deflated,
            Some(other) => {
                eprintln!(
                    "warning: unsupported zip compression algorithm '{other}'; \
                     falling back to 'store'"
                );
                CompressionMethod::Stored
            }
        };
        let files = match options.get("files") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(values)) if values.len() <= 1024 => values
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| error("zip files must be a list of paths"))
                })
                .collect::<Result<_>>()?,
            Some(_) => return Err(error("zip files must be a list of paths")),
        };
        Ok(Self {
            delete: !options.get("keep-files").is_some_and(truthy),
            extension,
            compression,
            safe: options.get("mode").and_then(Value::as_str) == Some("safe"),
            files,
            state: Default::default(),
        })
    }

    /// A copy with independent per-job archive state.
    pub(crate) fn fresh_job(&self) -> Self {
        Self {
            state: Default::default(),
            ..self.clone()
        }
    }

    fn open(&self, state: &mut State) -> Result<ZipWriter<File>> {
        let path = state.path.as_ref().expect("archive path");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        if file.metadata()?.len() == 0 {
            return Ok(ZipWriter::new(file));
        }
        state.names = ZipArchive::new(BufReader::new(&mut file))
            .map_err(|_| error("existing zip archive is invalid"))?
            .file_names()
            .filter_map(|name| name.ok().map(|name| name.into_owned()))
            .collect();
        file.rewind()?;
        ZipWriter::new_append(file).map_err(|_| error("existing zip archive is invalid"))
    }

    fn add(&self, writer: &mut ZipWriter<File>, source: &Path, name: &str) -> Result<()> {
        let options = SimpleFileOptions::default()
            .compression_method(self.compression)
            .large_file(source.metadata()?.len() >= u32::MAX as u64);
        writer
            .start_file(name, options)
            .map_err(|e| error(format!("zip write failed: {e}")))?;
        std::io::copy(&mut File::open(source)?, writer)?;
        Ok(())
    }

    /// Store `source` as `name`. Returns whether the downloaded file should be
    /// deleted instead of published.
    pub fn write(&self, source: &Path, directory: &Path, name: &str) -> Result<bool> {
        let mut state = self.state.lock().map_err(|_| error("zip state poisoned"))?;
        if state.path.is_none() {
            let mut path = directory.as_os_str().to_owned();
            path.push(format!(".{}", self.extension));
            state.path = Some(path.into());
        }
        let mut writer = match state.writer.take() {
            Some(writer) => writer,
            None => self.open(&mut state)?,
        };
        let result = (|| {
            if !state.extra_files {
                state.extra_files = true;
                for extra in &self.files {
                    let path = crate::postprocess::expand_home(extra);
                    let path = if path.is_absolute() {
                        path
                    } else {
                        directory.join(path)
                    };
                    let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned())
                    else {
                        continue;
                    };
                    match self.add(&mut writer, &path, &name) {
                        Ok(()) => {
                            state.names.insert(name);
                            if self.delete {
                                let _ = std::fs::remove_file(&path);
                            }
                        }
                        Err(_) => {
                            eprintln!("warning: unable to write {} to zip archive", path.display())
                        }
                    }
                }
            }
            if state.names.contains(name) {
                return Ok(false);
            }
            self.add(&mut writer, source, name)?;
            state.names.insert(name.to_owned());
            Ok(self.delete)
        })();
        if self.safe {
            writer
                .finish()
                .map_err(|e| error(format!("zip write failed: {e}")))?;
        } else {
            state.writer = Some(writer);
        }
        result
    }

    /// Close the archive; without `keep-files`, remove the (empty) download
    /// directory and an archive that received no entries.
    pub fn finalize(&self) -> Result<()> {
        let mut state = self.state.lock().map_err(|_| error("zip state poisoned"))?;
        if let Some(writer) = state.writer.take() {
            writer
                .finish()
                .map_err(|e| error(format!("zip write failed: {e}")))?;
        }
        if self.delete
            && let Some(path) = &state.path
        {
            if let Some(directory) = path
                .to_str()
                .and_then(|p| p.strip_suffix(&format!(".{}", self.extension)))
            {
                let _ = std::fs::remove_dir(directory);
            }
            if state.names.is_empty() {
                let _ = std::fs::remove_file(path);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn names(path: &Path) -> Vec<String> {
        let mut names: Vec<_> = ZipArchive::new(File::open(path).unwrap())
            .unwrap()
            .file_names()
            .filter_map(|name| name.ok().map(|name| name.into_owned()))
            .collect();
        names.sort();
        names
    }

    #[test]
    fn stores_files_skips_duplicates_appends_and_cleans_up() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("gallery");
        std::fs::create_dir_all(&directory).unwrap();
        let source = root.path().join("source.part");
        std::fs::write(&source, b"data").unwrap();
        for mode in ["fast", "safe"] {
            let zip = ZipOptions::parse(json!({"mode": mode}).as_object().unwrap()).unwrap();
            assert!(zip.write(&source, &directory, "a.jpg").unwrap());
            assert!(!zip.write(&source, &directory, "a.jpg").unwrap());
            zip.finalize().unwrap();
            let fresh = zip.fresh_job();
            assert!(fresh.write(&source, &directory, "b.jpg").unwrap());
            fresh.finalize().unwrap();
            assert_eq!(names(&root.path().join("gallery.zip")), ["a.jpg", "b.jpg"]);
            assert!(!directory.exists());
            std::fs::remove_file(root.path().join("gallery.zip")).unwrap();
            std::fs::create_dir_all(&directory).unwrap();
        }
        let cbz = ZipOptions::parse(
            json!({"extension": "cbz", "keep-files": true, "compression": "zip"})
                .as_object()
                .unwrap(),
        )
        .unwrap();
        assert!(!cbz.write(&source, &directory, "c.jpg").unwrap());
        cbz.finalize().unwrap();
        assert_eq!(names(&root.path().join("gallery.cbz")), ["c.jpg"]);
        assert!(directory.exists());
    }
}
