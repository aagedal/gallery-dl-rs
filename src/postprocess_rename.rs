//! Upstream `rename` postprocessor: rename previously downloaded files
//! between an old and a new filename format.
//!
//! With `from`, an existing file named by `from` is renamed at `prepare` to
//! `to` (or the current filename). With only `to`, an existing file with the
//! current filename is renamed on `skip`, and new downloads are saved under
//! `to` (`prepare-after`).
use super::truthy;
use crate::{Result, error, extract::Media, naming::FilenameTemplate};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub struct RenameOptions {
    from: Option<FilenameTemplate>,
    to: Option<FilenameTemplate>,
    skip: bool,
}
impl std::fmt::Debug for RenameOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RenameOptions { formats: [redacted] }")
    }
}

fn template(options: &Map<String, Value>, key: &str) -> Result<Option<FilenameTemplate>> {
    match options.get(key).filter(|v| truthy(v)) {
        None => Ok(None),
        Some(Value::String(value)) => FilenameTemplate::parse_source(value).map(Some),
        Some(_) => Err(error(format!("rename '{key}' must be a format string"))),
    }
}

impl RenameOptions {
    pub fn parse(options: &Map<String, Value>) -> Result<Self> {
        let from = template(options, "from")?;
        let to = template(options, "to")?;
        if from.is_none() && to.is_none() {
            return Err(error("Option 'from' or 'to' is required"));
        }
        Ok(Self {
            from,
            to,
            skip: options.get("skip").is_none_or(truthy),
        })
    }

    /// Hooks upstream registers for this configuration.
    pub fn events(&self) -> Vec<super::Hook> {
        if self.from.is_some() {
            vec![super::Hook::Prepare]
        } else {
            vec![super::Hook::Skip, super::Hook::PrepareAfter]
        }
    }

    fn render(
        template: &FilenameTemplate,
        media: &Media,
        keywords: &Map<String, Value>,
    ) -> Result<String> {
        template
            .render_keywords(media, keywords)?
            .ok_or_else(|| error("rename format produced no filename"))
    }

    fn rename(&self, old: &Path, new: &Path) -> Result<()> {
        let name = |p: &Path| {
            p.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        };
        if self.skip && new.exists() {
            eprintln!(
                "warning: Not renaming '{}' to '{}' since another file with the same name exists",
                name(old),
                name(new)
            );
            return Ok(());
        }
        eprintln!("'{}' -> '{}'", name(old), name(new));
        std::fs::rename(old, new)?;
        Ok(())
    }

    /// Run for `hook`; returns the new destination path if it changed.
    pub fn run(
        &self,
        hook: super::Hook,
        media: &Media,
        keywords: &Map<String, Value>,
        path: &Path,
    ) -> Result<Option<PathBuf>> {
        let directory = path
            .parent()
            .ok_or_else(|| error("invalid postprocessor directory"))?;
        match (hook, &self.from, &self.to) {
            (super::Hook::Prepare, Some(from), to) => {
                let old = directory.join(Self::render(from, media, keywords)?);
                if old.exists() {
                    let new = match to {
                        Some(to) => directory.join(Self::render(to, media, keywords)?),
                        None => path.to_owned(),
                    };
                    self.rename(&old, &new)?;
                }
                Ok(None)
            }
            (super::Hook::Skip, None, Some(to)) => {
                let new = directory.join(Self::render(to, media, keywords)?);
                if path.exists() && new != path {
                    self.rename(path, &new)?;
                    return Ok(Some(new));
                }
                Ok(None)
            }
            (super::Hook::PrepareAfter, None, Some(to)) => {
                Ok(Some(directory.join(Self::render(to, media, keywords)?)))
            }
            _ => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::postprocess::Hook;
    use serde_json::json;

    fn media() -> Media {
        Media {
            url: "https://fixture.invalid/a.jpg".parse().unwrap(),
            category: "fixture".into(),
            filename: "a.jpg".into(),
            metadata: None,
            job: crate::extract::MediaJob::DirectHttp,
            headers: Default::default(),
            handoff: Default::default(),
        }
    }

    #[test]
    fn renames_from_old_format_and_to_new_format() {
        let root = tempfile::tempdir().unwrap();
        let keywords = Map::from_iter([
            ("id".into(), json!(7)),
            ("title".into(), json!("t")),
            ("extension".into(), json!("jpg")),
        ]);
        let current = root.path().join("7.jpg");
        std::fs::write(root.path().join("t.jpg"), b"x").unwrap();
        let from =
            RenameOptions::parse(json!({"from": "{title}.{extension}"}).as_object().unwrap())
                .unwrap();
        assert_eq!(from.events(), [Hook::Prepare]);
        from.run(Hook::Prepare, &media(), &keywords, &current)
            .unwrap();
        assert!(current.exists() && !root.path().join("t.jpg").exists());

        let to = RenameOptions::parse(json!({"to": "{title}.{extension}"}).as_object().unwrap())
            .unwrap();
        let renamed = to.run(Hook::Skip, &media(), &keywords, &current).unwrap();
        assert_eq!(renamed, Some(root.path().join("t.jpg")));
        assert!(!current.exists() && root.path().join("t.jpg").exists());
        assert_eq!(
            to.run(Hook::PrepareAfter, &media(), &keywords, &current)
                .unwrap(),
            Some(root.path().join("t.jpg"))
        );
        // Existing targets are kept unless `skip` is disabled.
        std::fs::write(&current, b"y").unwrap();
        to.run(Hook::Skip, &media(), &keywords, &current).unwrap();
        assert!(current.exists());
        assert!(RenameOptions::parse(&Map::new()).is_err());
    }
}
