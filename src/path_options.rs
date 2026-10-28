//! Upstream `path-restrict`, `path-replace`, `path-remove` and `path-strip`.
//!
//! These are process-wide, like upstream's global configuration. When none is
//! configured, filenames keep gallery-dl-rs's default cleaning. Separators
//! and NUL are always replaced, whatever the configured character set.
use crate::{Result, error};
use regex::Regex;
use serde_json::{Map, Value};
use std::{collections::HashMap, sync::OnceLock};

#[derive(Debug)]
enum Clean {
    None,
    Class(Regex, String),
    Map(HashMap<char, String>),
}

impl Clean {
    fn build(chars: &str, replace: &str) -> Result<Self> {
        if chars.is_empty() {
            return Ok(Self::None);
        }
        Regex::new(&format!("[{chars}]"))
            .map(|regex| Self::Class(regex, replace.to_owned()))
            .map_err(|_| error("invalid path character set"))
    }
    fn apply(&self, value: &str) -> String {
        match self {
            Self::None => value.to_owned(),
            Self::Class(regex, replace) => regex.replace_all(value, replace.as_str()).into_owned(),
            Self::Map(map) => value
                .chars()
                .map(|c| map.get(&c).cloned().unwrap_or_else(|| c.to_string()))
                .collect(),
        }
    }
}

#[derive(Debug)]
pub struct PathOptions {
    restrict: Clean,
    remove: Clean,
    strip: String,
}

static OPTIONS: OnceLock<PathOptions> = OnceLock::new();

const WINDOWS_CHARS: &str = "\\\\|/<>:\"?*";

impl PathOptions {
    /// Build from upstream option values, keyed by option name.
    pub fn parse(values: &Map<String, Value>) -> Result<Option<Self>> {
        if values.is_empty() {
            return Ok(None);
        }
        let replace = match values.get("path-replace") {
            None => "_",
            Some(Value::String(value)) => value.as_str(),
            Some(_) => return Err(error("path-replace must be a string")),
        };
        let restrict = match values.get("path-restrict") {
            None => Clean::build(if cfg!(windows) { WINDOWS_CHARS } else { "/" }, replace)?,
            Some(Value::String(value)) => match value.as_str() {
                "auto" => Clean::build(if cfg!(windows) { WINDOWS_CHARS } else { "/" }, replace)?,
                "unix" => Clean::build("/", replace)?,
                "windows" => Clean::build(WINDOWS_CHARS, replace)?,
                "windows+" => Clean::Map(
                    [
                        ('\\', "⧹"),
                        ('|', "｜"),
                        ('/', "⧸"),
                        ('<', "＜"),
                        ('>', "＞"),
                        (':', "："),
                        ('"', "＂"),
                        ('?', "？"),
                        ('*', "＊"),
                    ]
                    .into_iter()
                    .map(|(c, r)| (c, r.to_owned()))
                    .collect(),
                ),
                "ascii" => Clean::build("^0-9A-Za-z_.", replace)?,
                "ascii+" => Clean::build("^0-9@-\\[\\]-{ #-)+-.;=!}~", replace)?,
                chars => Clean::build(&regex_class(chars), replace)?,
            },
            Some(Value::Object(map)) => Clean::Map(
                map.iter()
                    .map(|(key, value)| {
                        let mut chars = key.chars();
                        match (chars.next(), chars.next(), value.as_str()) {
                            (Some(c), None, Some(value)) => Ok((c, value.to_owned())),
                            _ => Err(error("path-restrict mappings map characters to strings")),
                        }
                    })
                    .collect::<Result<_>>()?,
            ),
            Some(_) => return Err(error("path-restrict must be a string or mapping")),
        };
        let remove = match values.get("path-remove") {
            None => Clean::build("\\x00-\\x1f\\x7f", "")?,
            Some(Value::String(value)) => Clean::build(&regex_class(value), "")?,
            Some(_) => return Err(error("path-remove must be a string")),
        };
        let strip = match values.get("path-strip").and_then(Value::as_str) {
            None | Some("auto") => if cfg!(windows) { ". " } else { "" }.to_owned(),
            Some("unix") => String::new(),
            Some("windows") => ". ".to_owned(),
            Some(chars) => chars.to_owned(),
        };
        Ok(Some(Self {
            restrict,
            remove,
            strip,
        }))
    }

    /// Install process-wide options; later calls are ignored.
    pub fn install(self) {
        let _ = OPTIONS.set(self);
    }

    /// The configured options, if any.
    pub fn get() -> Option<&'static Self> {
        OPTIONS.get()
    }

    /// Upstream `clean_path(clean_segment(value))`, plus mandatory separator
    /// and NUL replacement. Directory components are also `strip`ped.
    pub fn clean(&self, value: &str, directory: bool) -> String {
        let value = self.remove.apply(&self.restrict.apply(value));
        let mut value: String = value
            .chars()
            .map(|c| {
                if c == '/' || c == '\0' || cfg!(windows) && c == '\\' {
                    '_'
                } else {
                    c
                }
            })
            .collect();
        if directory && !self.strip.is_empty() {
            value = value.trim_matches(|c| self.strip.contains(c)).to_owned();
        }
        value
    }
}

static EXTENSION_MAP: OnceLock<HashMap<String, String>> = OnceLock::new();

/// Install upstream's `extension-map` (an object); later calls are ignored.
pub fn set_extension_map(value: &Value) -> Result<()> {
    let map = value
        .as_object()
        .ok_or_else(|| error("extension-map must be an object"))?
        .iter()
        .map(|(key, value)| {
            value
                .as_str()
                .map(|value| (key.clone(), value.to_owned()))
                .ok_or_else(|| error("extension-map values must be strings"))
        })
        .collect::<Result<_>>()?;
    let _ = EXTENSION_MAP.set(map);
    Ok(())
}

/// Upstream `PathFormat.extension_map`: by default `jpeg`, `jpe`, `jfif`,
/// `jif` and `jfi` become `jpg`.
pub fn map_extension(extension: &str) -> Option<String> {
    match EXTENSION_MAP.get() {
        Some(map) => map.get(extension).cloned(),
        None => matches!(extension, "jpeg" | "jpe" | "jfif" | "jif" | "jfi").then(|| "jpg".into()),
    }
}

/// Upstream inserts custom character sets into a regex class verbatim;
/// escape only what Rust's class syntax treats differently from Python's.
fn regex_class(chars: &str) -> String {
    chars
        .replace('[', "\\[")
        .replace("&&", "\\&\\&")
        .replace("~~", "\\~\\~")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn options(value: Value) -> PathOptions {
        PathOptions::parse(value.as_object().unwrap())
            .unwrap()
            .unwrap()
    }

    #[test]
    fn upstream_restrict_modes() {
        let unix = options(json!({"path-restrict": "unix"}));
        assert_eq!(unix.clean("a b/c:d\u{1}é", false), "a b_c:dé");
        let windows = options(json!({"path-restrict": "windows", "path-replace": "-"}));
        assert_eq!(windows.clean("a<b>c?d", false), "a-b-c-d");
        let ascii = options(json!({"path-restrict": "ascii"}));
        assert_eq!(ascii.clean("a b-é.jpg", false), "a_b__.jpg");
        let plus = options(json!({"path-restrict": "windows+"}));
        assert_eq!(plus.clean("a:b", false), "a：b");
        let custom = options(json!({"path-restrict": {"a": "A"}}));
        assert_eq!(custom.clean("a/a", false), "A_A");
        let strip = options(json!({"path-strip": "windows"}));
        assert_eq!(strip.clean(" dir. ", true), "dir");
        assert_eq!(strip.clean(" name. ", false), " name. ");
        assert!(PathOptions::parse(&Map::new()).unwrap().is_none());
    }
}
