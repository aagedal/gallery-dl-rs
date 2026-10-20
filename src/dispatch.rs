//! Ordered upstream-style profile dispatcher selections.
use crate::{Result, error};
use serde_json::Value;

const MAX_VIEWS: usize = 128;
const MAX_VIEW_BYTES: usize = 128;

/// Only the exact string `all` expands the catalog. An explicit empty list
/// disables dispatch and remains distinct from an absent setting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Include {
    All,
    Views(Vec<String>),
}

impl Include {
    pub fn parse(value: &Value) -> Result<Self> {
        let invalid =
            || error("include must be all, a bounded string/list of view names, false or null");
        let views = match value {
            Value::String(value) if value == "all" => return Ok(Self::All),
            Value::String(value) if value.len() <= MAX_VIEWS * MAX_VIEW_BYTES => {
                if value.is_empty() {
                    Vec::new()
                } else {
                    // Dispatch removes ASCII spaces, not arbitrary whitespace.
                    value
                        .replace(' ', "")
                        .split(',')
                        .map(str::to_owned)
                        .collect()
                }
            }
            Value::Array(values) if values.len() <= MAX_VIEWS => values
                .iter()
                .map(|value| value.as_str().map(str::to_owned).ok_or_else(invalid))
                .collect::<Result<Vec<_>>>()?,
            Value::Bool(false) | Value::Null => Vec::new(),
            _ => return Err(invalid()),
        };
        if views.len() > MAX_VIEWS
            || views
                .iter()
                .any(|view| view.len() > MAX_VIEW_BYTES || view.contains('\0'))
        {
            return Err(invalid());
        }
        Ok(Self::Views(views))
    }

    pub fn value(&self) -> Value {
        match self {
            Self::All => Value::String("all".into()),
            Self::Views(views) => serde_json::json!(views),
        }
    }
}

/// Canonical view names in configured order. Unknown names are omitted like
/// upstream's invalid-include entries; known unsupported views stay in the
/// catalog so their producer can report an error inside the selected scope.
/// Duplicate queued views retain their child positions.
pub fn select(
    include: Option<&Include>,
    default: &[&str],
    available: &[&str],
    aliases: &[(&str, &str)],
) -> Vec<String> {
    let views = match include {
        Some(Include::All) => return available.iter().map(|view| (*view).to_owned()).collect(),
        Some(Include::Views(views)) => views.iter().map(String::as_str).collect(),
        None => default.to_vec(),
    };
    views
        .into_iter()
        .filter_map(|view| {
            let canonical = aliases
                .iter()
                .find(|(alias, _)| *alias == view)
                .map_or(view, |(_, canonical)| *canonical);
            available.contains(&canonical).then(|| canonical.to_owned())
        })
        .collect()
}
