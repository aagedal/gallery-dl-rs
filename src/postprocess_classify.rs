//! Upstream `classify` postprocessor: sort files into subdirectories by extension.
use crate::{Result, error};
use serde_json::{Map, Value};
use std::collections::HashMap;

const DEFAULT_MAPPING: &[(&str, &[&str])] = &[
    (
        "Pictures",
        &[
            "jpg", "jpeg", "png", "gif", "bmp", "svg", "webp", "avif", "heic", "heif", "ico", "psd",
        ],
    ),
    (
        "Video",
        &[
            "flv", "ogv", "avi", "mp4", "mpg", "mpeg", "3gp", "mkv", "webm", "vob", "wmv", "m4v",
            "mov",
        ],
    ),
    ("Music", &["mp3", "aac", "flac", "ogg", "wma", "m4a", "wav"]),
    ("Archives", &["zip", "rar", "7z", "tar", "gz", "bz2"]),
    ("Documents", &["txt", "pdf"]),
];

#[derive(Clone, Debug)]
pub struct ClassifyOptions {
    mapping: HashMap<String, String>,
}

impl ClassifyOptions {
    pub fn parse(options: &Map<String, Value>) -> Result<Self> {
        let mut mapping = HashMap::new();
        match options.get("mapping") {
            None => {
                for (directory, extensions) in DEFAULT_MAPPING {
                    for extension in *extensions {
                        mapping.insert((*extension).to_owned(), (*directory).to_owned());
                    }
                }
            }
            Some(Value::Object(entries)) if entries.len() <= 1024 => {
                for (directory, extensions) in entries {
                    let invalid = || error("invalid classify mapping");
                    if directory.is_empty()
                        || directory
                            .split(['/', '\\'])
                            .any(|part| matches!(part, "" | "." | ".."))
                    {
                        return Err(invalid());
                    }
                    let extensions: Vec<&str> = match extensions {
                        Value::Array(values) => values
                            .iter()
                            .map(|v| v.as_str().ok_or_else(invalid))
                            .collect::<Result<_>>()?,
                        // A Python string iterates as its characters.
                        Value::String(value) => value
                            .char_indices()
                            .map(|(i, c)| &value[i..i + c.len_utf8()])
                            .collect(),
                        _ => return Err(invalid()),
                    };
                    for extension in extensions {
                        mapping.insert(extension.to_owned(), directory.clone());
                    }
                }
            }
            Some(_) => return Err(error("invalid classify mapping")),
        }
        Ok(Self { mapping })
    }

    /// The subdirectory for a file's extension, if any.
    pub fn directory(&self, keywords: &Map<String, Value>) -> Option<&str> {
        keywords
            .get("extension")
            .and_then(Value::as_str)
            .and_then(|extension| self.mapping.get(extension))
            .map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn default_and_custom_mappings() {
        let default = ClassifyOptions::parse(&Map::new()).unwrap();
        let kw = |ext: &str| Map::from_iter([("extension".into(), json!(ext))]);
        assert_eq!(default.directory(&kw("webm")), Some("Video"));
        assert_eq!(default.directory(&kw("JPG")), None);
        let custom = ClassifyOptions::parse(
            json!({"mapping": {"img/sub": ["png"]}})
                .as_object()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(custom.directory(&kw("png")), Some("img/sub"));
        assert_eq!(custom.directory(&kw("jpg")), None);
        assert!(
            ClassifyOptions::parse(json!({"mapping": {"../x": ["png"]}}).as_object().unwrap())
                .is_err()
        );
    }
}
