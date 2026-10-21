//! Metadata-derived file and directory modification times.
use super::template::{Template, parse_date};
use crate::{Result, error};
use serde_json::{Map, Value};
use std::{
    path::Path,
    time::{Duration, UNIX_EPOCH},
};
#[derive(Clone)]
pub struct MtimeOptions {
    file: bool,
    key: String,
    value: Option<Template>,
}
impl std::fmt::Debug for MtimeOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MtimeOptions")
            .field("file", &self.file)
            .finish_non_exhaustive()
    }
}

impl MtimeOptions {
    pub fn parse(options: &Map<String, Value>) -> Result<Self> {
        let target = options
            .get("target")
            .filter(|v| truthy(v))
            .or_else(|| options.get("mode"));
        let file = !matches!(target.and_then(Value::as_str), Some("dir" | "directory"));
        let value = options
            .get("value")
            .filter(|v| truthy(v))
            .map(|v| {
                v.as_str()
                    .ok_or_else(|| error("invalid mtime value template"))
                    .and_then(Template::parse)
            })
            .transpose()?;
        let key = options
            .get("key")
            .map_or(Some("date"), Value::as_str)
            .filter(|k| k.len() <= 65536)
            .ok_or_else(|| error("invalid mtime metadata key"))?
            .to_owned();
        Ok(Self { file, key, value })
    }
    pub fn default_events(&self) -> &'static [&'static str] {
        if self.file {
            &["file"]
        } else {
            &["post-after"]
        }
    }
    pub fn is_file(&self) -> bool {
        self.file
    }
    pub fn getter(&self, context: &Map<String, Value>) -> Option<i64> {
        if let Some(timestamp) = self
            .value
            .as_ref()
            .and_then(|template| template.timestamp(context))
        {
            return Some(timestamp);
        }
        let value = match &self.value {
            Some(template) => template.render_value(context).ok()?,
            None => context.get(&self.key)?.clone(),
        };
        if !truthy(&value) {
            return None;
        }
        let date = self.value.is_none()
            && matches!(self.key.as_str(), "date" | "date_updated" | "date_url");
        if date
            && let Some(timestamp) = value
                .as_str()
                .and_then(|value| parse_date(value).ok())
                .map(|date| date.unix_timestamp())
        {
            return Some(timestamp);
        }
        // Python int() truncates floats, recognizes whitespace, signs and underscores.
        Some(match value {
            Value::Bool(value) => i64::from(value),
            Value::Number(value) => value
                .as_i64()
                .or_else(|| {
                    value
                        .as_f64()
                        .filter(|v| v.is_finite() && *v >= i64::MIN as f64 && *v < i64::MAX as f64)
                        .map(|v| v as i64)
                })
                .unwrap_or(0),
            Value::String(value) => parse_int(&value).unwrap_or(0),
            _ => 0,
        })
    }
    pub fn apply(&self, context: &mut Map<String, Value>, path: &Path) -> Result<()> {
        let timestamp = self.getter(context);
        if self.file {
            context.insert(
                "_mtime_meta".into(),
                timestamp.map_or(Value::Null, Value::from),
            );
        }
        if let Some(timestamp) = timestamp.filter(|timestamp| *timestamp != 0) {
            let time = if timestamp >= 0 {
                UNIX_EPOCH.checked_add(Duration::from_secs(timestamp as u64))
            } else {
                UNIX_EPOCH.checked_sub(Duration::from_secs(timestamp.unsigned_abs()))
            }
            .ok_or_else(|| error("mtime timestamp is out of range"))?;
            let file = open_times_target(path).map_err(|_| error("could not open mtime target"))?;
            let metadata = file
                .metadata()
                .map_err(|_| error("could not inspect mtime target"))?;
            if (self.file && !metadata.is_file()) || (!self.file && !metadata.is_dir()) {
                return Err(error("invalid mtime target type"));
            }
            file.set_modified(time)
                .map_err(|_| error("could not set modification time"))?;
        }
        Ok(())
    }
}
/// Opens a file or directory with enough access to inspect and set its times.
/// Windows needs explicit attribute-write access, and directories can only be
/// opened with `FILE_FLAG_BACKUP_SEMANTICS`.
pub(super) fn open_times_target(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_READ_ATTRIBUTES: u32 = 0x0080;
        const FILE_WRITE_ATTRIBUTES: u32 = 0x0100;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        std::fs::OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
    }
    #[cfg(not(windows))]
    {
        std::fs::File::open(path)
    }
}
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(v) => *v,
        Value::Number(v) => v.as_f64() != Some(0.0),
        Value::String(v) => !v.is_empty(),
        Value::Array(v) => !v.is_empty(),
        Value::Object(v) => !v.is_empty(),
    }
}
// Unicode17 decimal blocks, matching Python int() rather than numeric glyphs.
fn decimal(value: char) -> Option<u32> {
    const RANGES: &[(u32, u32)] = &[
        (0x30, 0x39),
        (0x660, 0x669),
        (0x6f0, 0x6f9),
        (0x7c0, 0x7c9),
        (0x966, 0x96f),
        (0x9e6, 0x9ef),
        (0xa66, 0xa6f),
        (0xae6, 0xaef),
        (0xb66, 0xb6f),
        (0xbe6, 0xbef),
        (0xc66, 0xc6f),
        (0xce6, 0xcef),
        (0xd66, 0xd6f),
        (0xde6, 0xdef),
        (0xe50, 0xe59),
        (0xed0, 0xed9),
        (0xf20, 0xf29),
        (0x1040, 0x1049),
        (0x1090, 0x1099),
        (0x17e0, 0x17e9),
        (0x1810, 0x1819),
        (0x1946, 0x194f),
        (0x19d0, 0x19d9),
        (0x1a80, 0x1a89),
        (0x1a90, 0x1a99),
        (0x1b50, 0x1b59),
        (0x1bb0, 0x1bb9),
        (0x1c40, 0x1c49),
        (0x1c50, 0x1c59),
        (0xa620, 0xa629),
        (0xa8d0, 0xa8d9),
        (0xa900, 0xa909),
        (0xa9d0, 0xa9d9),
        (0xa9f0, 0xa9f9),
        (0xaa50, 0xaa59),
        (0xabf0, 0xabf9),
        (0xff10, 0xff19),
        (0x104a0, 0x104a9),
        (0x10d30, 0x10d39),
        (0x10d40, 0x10d49),
        (0x11066, 0x1106f),
        (0x110f0, 0x110f9),
        (0x11136, 0x1113f),
        (0x111d0, 0x111d9),
        (0x112f0, 0x112f9),
        (0x11450, 0x11459),
        (0x114d0, 0x114d9),
        (0x11650, 0x11659),
        (0x116c0, 0x116c9),
        (0x116d0, 0x116e3),
        (0x11730, 0x11739),
        (0x118e0, 0x118e9),
        (0x11950, 0x11959),
        (0x11bf0, 0x11bf9),
        (0x11c50, 0x11c59),
        (0x11d50, 0x11d59),
        (0x11da0, 0x11da9),
        (0x11de0, 0x11de9),
        (0x11f50, 0x11f59),
        (0x16130, 0x16139),
        (0x16a60, 0x16a69),
        (0x16ac0, 0x16ac9),
        (0x16b50, 0x16b59),
        (0x16d70, 0x16d79),
        (0x1ccf0, 0x1ccf9),
        (0x1d7ce, 0x1d7ff),
        (0x1e140, 0x1e149),
        (0x1e2f0, 0x1e2f9),
        (0x1e4f0, 0x1e4f9),
        (0x1e5f1, 0x1e5fa),
        (0x1e950, 0x1e959),
        (0x1fbf0, 0x1fbf9),
    ];
    let code = value as u32;
    RANGES
        .iter()
        .find(|(start, end)| (*start..=*end).contains(&code))
        .map(|(start, _)| (code - start) % 10)
}

fn parse_int(value: &str) -> Option<i64> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let digits = value.strip_prefix(['+', '-']).unwrap_or(value);
    if digits.is_empty()
        || digits.starts_with('_')
        || digits.ends_with('_')
        || digits.contains("__")
    {
        return None;
    }
    let mut normalized = String::new();
    for c in value.chars() {
        if c == '_' {
            continue;
        }
        if c == '+' || c == '-' {
            normalized.push(c);
        } else {
            normalized.push(char::from_digit(decimal(c)?, 10)?);
        }
    }
    normalized.parse().ok()
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn options(value: Value) -> MtimeOptions {
        MtimeOptions::parse(value.as_object().unwrap()).unwrap()
    }
    #[test]
    fn actual_source_mtime_oracle_matches_context_mutation_and_directory_target() {
        let oracle: Value =
            serde_json::from_str(include_str!("../tests/fixtures/postprocessors/source.json"))
                .unwrap();
        for case in oracle["mtime_cases"].as_array().unwrap() {
            let options = MtimeOptions::parse(case["options"].as_object().unwrap()).unwrap();
            let directory = tempfile::tempdir().unwrap();
            let file = directory.path().join("file");
            std::fs::write(&file, b"data").unwrap();
            fn native_dates(value: &Value) -> Value {
                match value {
                    Value::Object(map)
                        if map.get("$datetime").and_then(Value::as_str).is_some() =>
                    {
                        Value::String(map["$datetime"].as_str().unwrap().replace('T', " "))
                    }
                    Value::Object(map) => Value::Object(
                        map.iter()
                            .map(|(key, value)| (key.clone(), native_dates(value)))
                            .collect(),
                    ),
                    value => value.clone(),
                }
            }
            let mut context = native_dates(&case["context"]).as_object().unwrap().clone();
            let target = if options.is_file() {
                file.as_path()
            } else {
                directory.path()
            };
            open_times_target(directory.path())
                .unwrap()
                .set_modified(UNIX_EPOCH + Duration::from_secs(1_000_000_000))
                .unwrap();
            options.apply(&mut context, target).unwrap();
            let mut expected = native_dates(&case["result"]);
            if let Some(timestamp) = expected.get("_mtime_meta").and_then(Value::as_f64) {
                expected["_mtime_meta"] = Value::from(timestamp as i64);
            }
            assert_eq!(Value::Object(context), expected, "{}", case["name"]);
            if case["options"].get("event").is_none() {
                assert_eq!(
                    serde_json::to_value(options.default_events()).unwrap(),
                    case["hooks"],
                    "{}",
                    case["name"]
                );
            }
            assert_eq!(
                std::fs::metadata(directory.path())
                    .unwrap()
                    .modified()
                    .unwrap()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
                case["directory_mtime"].as_u64().unwrap(),
                "{}",
                case["name"]
            );
        }
    }
    #[test]
    fn source_getters_dates_truthiness_and_integer_coercion() {
        let context = json!({"date":"2024-01-02 03:04:05","num":" 1_234 ","negative":-3.9,"zero":0,"bad":"oops"});
        let context = context.as_object().unwrap();
        for (config, expected) in [
            (json!({}), Some(1704164645)),
            (json!({"key":"num"}), Some(1234)),
            (json!({"key":"negative"}), Some(-3)),
            (json!({"key":"zero"}), None),
            (json!({"key":"bad"}), Some(0)),
            (json!({"value":"{num}"}), Some(1234)),
            (json!({"value":"{date}"}), Some(1704164645)),
            (json!({"key":"missing"}), None),
        ] {
            assert_eq!(options(config).getter(context), expected);
        }
    }
    #[test]
    fn source_file_directory_defaults_and_real_modification_times() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("file");
        std::fs::write(&path, b"data").unwrap();
        let mut context = json!({"date":1704164645}).as_object().unwrap().clone();
        let file = options(json!({}));
        assert_eq!(file.default_events(), ["file"]);
        file.apply(&mut context, &path).unwrap();
        assert_eq!(context["_mtime_meta"], 1704164645);
        assert_eq!(
            std::fs::metadata(&path)
                .unwrap()
                .modified()
                .unwrap()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            1704164645
        );
        let dir = options(json!({"target":"directory"}));
        assert_eq!(dir.default_events(), ["post-after"]);
        context.remove("_mtime_meta");
        dir.apply(&mut context, directory.path()).unwrap();
        assert!(!context.contains_key("_mtime_meta"));
        assert!(file.apply(&mut context, directory.path()).is_err());
        context.insert("date".into(), Value::Null);
        file.apply(&mut context, &path).unwrap();
        assert!(context["_mtime_meta"].is_null());
    }
}
