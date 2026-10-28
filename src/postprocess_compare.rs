//! Upstream `compare` postprocessor: with `skip: false`, compare a new
//! download against the existing file and keep the old one when equal
//! (`replace`) or save under `NAME.N.EXT` when different (`enumerate`).
use super::truthy;
use crate::{Result, Stop};
use serde_json::{Map, Value};
use std::{
    io::Read,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug)]
pub struct CompareOptions {
    enumerate: bool,
    shallow: bool,
    /// `equal`: stop after this many consecutive equal files.
    equal: Option<(Stop, u64)>,
    equal_count: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

/// What to do with the new download.
#[derive(Debug, PartialEq, Eq)]
pub enum Comparison {
    /// The existing file is identical; discard the download.
    Equal,
    /// Publish the download at this path.
    Publish(PathBuf),
}

impl CompareOptions {
    pub fn parse(options: &Map<String, Value>) -> Result<Self> {
        let equal = options
            .get("equal")
            .filter(|v| truthy(v))
            .and_then(Value::as_str)
            .and_then(|equal| {
                let (mode, limit) = equal.split_once(':').unwrap_or((equal, ""));
                let stop = match mode {
                    "abort" => Stop::Abort,
                    "terminate" => Stop::Terminate,
                    "exit" => Stop::Exit(0),
                    _ => return None,
                };
                Some((stop, crate::sites::gdl::parse_int(limit) as u64))
            });
        Ok(Self {
            equal,
            equal_count: Default::default(),
            enumerate: options.get("action").and_then(Value::as_str) == Some("enumerate"),
            shallow: options.get("shallow").is_some_and(truthy),
        })
    }

    fn same(&self, existing: &Path, new: &Path) -> std::io::Result<bool> {
        if std::fs::metadata(existing)?.len() != std::fs::metadata(new)?.len() {
            return Ok(false);
        }
        if self.shallow {
            return Ok(true);
        }
        let (mut a, mut b) = (std::fs::File::open(existing)?, std::fs::File::open(new)?);
        let (mut x, mut y) = (vec![0; 16384], vec![0; 16384]);
        loop {
            let n = a.read(&mut x)?;
            let m = b.read(&mut y)?;
            if x[..n] != y[..m] {
                return Ok(false);
            }
            if n == 0 {
                return Ok(true);
            }
        }
    }

    /// Count an equal file for `equal`; fails with the configured stop
    /// once the limit of consecutive equal files is reached.
    pub fn count(&self, comparison: &Comparison) -> Result<()> {
        use std::sync::atomic::Ordering;
        if *comparison != Comparison::Equal {
            self.equal_count.store(0, Ordering::Relaxed);
            return Ok(());
        }
        if let Some((stop, limit)) = self.equal {
            let count = self.equal_count.fetch_add(1, Ordering::Relaxed) + 1;
            if count >= limit {
                return Err(stop.into());
            }
        }
        Ok(())
    }

    /// Compare the download at `new` with the destination `path`.
    pub fn run(&self, path: &Path, new: &Path) -> Comparison {
        if !self.enumerate {
            return match self.same(path, new) {
                Ok(true) => Comparison::Equal,
                _ => Comparison::Publish(path.to_owned()),
            };
        }
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let (stem, extension) = name.rsplit_once('.').unwrap_or((&name, ""));
        let mut candidate = path.to_owned();
        for number in 1.. {
            match self.same(&candidate, new) {
                Ok(true) => return Comparison::Equal,
                Ok(false) => {
                    let name = if extension.is_empty() {
                        format!("{stem}.{number}")
                    } else {
                        format!("{stem}.{number}.{extension}")
                    };
                    candidate = path.with_file_name(name);
                }
                // Upstream stops at the first candidate that cannot be read.
                Err(_) => break,
            }
        }
        Comparison::Publish(candidate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn replace_and_enumerate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.jpg");
        let new = dir.path().join("new");
        std::fs::write(&path, b"old").unwrap();
        std::fs::write(&new, b"old").unwrap();
        let replace = CompareOptions::parse(&Map::new()).unwrap();
        assert_eq!(replace.run(&path, &new), Comparison::Equal);
        std::fs::write(&new, b"new").unwrap();
        assert_eq!(replace.run(&path, &new), Comparison::Publish(path.clone()));
        let shallow = CompareOptions::parse(json!({"shallow": true}).as_object().unwrap()).unwrap();
        assert_eq!(shallow.run(&path, &new), Comparison::Equal);

        let enumerate =
            CompareOptions::parse(json!({"action": "enumerate"}).as_object().unwrap()).unwrap();
        assert_eq!(
            enumerate.run(&path, &new),
            Comparison::Publish(dir.path().join("a.1.jpg"))
        );
        std::fs::write(dir.path().join("a.1.jpg"), b"new").unwrap();
        assert_eq!(enumerate.run(&path, &new), Comparison::Equal);

        let abort =
            CompareOptions::parse(json!({"equal": "abort:2"}).as_object().unwrap()).unwrap();
        assert!(abort.count(&Comparison::Equal).is_ok());
        assert!(abort.count(&Comparison::Publish(path.clone())).is_ok());
        assert!(abort.count(&Comparison::Equal).is_ok());
        let stop = abort.count(&Comparison::Equal).unwrap_err();
        assert_eq!(Stop::of(&stop), Some(Stop::Abort));
        let exit = CompareOptions::parse(json!({"equal": "exit"}).as_object().unwrap()).unwrap();
        assert_eq!(
            Stop::of(&exit.count(&Comparison::Equal).unwrap_err()),
            Some(Stop::Exit(0))
        );
    }
}
