//! Bounded content templates using the native, non-executable field formatter.
use crate::{
    Result, error,
    naming::format::{Field, Value as FormatValue},
};
use serde_json::{Map, Value};
use time::format_description::well_known::Rfc3339;
const MAX_SOURCE: usize = 65536;
const MAX_OUTPUT: usize = 16 * 1024 * 1024;
#[derive(Clone, Debug)]
enum Part {
    Text(String),
    Field(Field),
}
#[derive(Clone)]
pub struct Template {
    parts: Vec<Part>,
}
impl std::fmt::Debug for Template {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Template { source: [redacted] }")
    }
}
impl Template {
    pub fn parse(source: &str) -> Result<Self> {
        if source.len() > MAX_SOURCE {
            return Err(error("postprocessor template exceeds the size limit"));
        }
        let source = if let Some(selector) = source.strip_prefix('\u{000c}') {
            let (mode, source) = selector
                .split_once(' ')
                .ok_or_else(|| error("unsupported postprocessor template formatter"))?;
            if mode != "S" {
                return Err(error("unsupported postprocessor template formatter"));
            }
            source
        } else {
            source
        };
        let mut chars = source.chars().peekable();
        let mut parts = Vec::new();
        let mut text = String::new();
        while let Some(c) = chars.next() {
            match c {
                '{' if chars.peek() == Some(&'{') => {
                    chars.next();
                    text.push('{');
                }
                '}' if chars.peek() == Some(&'}') => {
                    chars.next();
                    text.push('}');
                }
                '{' => {
                    if !text.is_empty() {
                        parts.push(Part::Text(std::mem::take(&mut text)));
                    }
                    let mut field = String::new();
                    loop {
                        match chars.next() {
                            Some('}') => break,
                            Some('{') | None => {
                                return Err(error("unclosed postprocessor template field"));
                            }
                            Some(c) => field.push(c),
                        }
                    }
                    parts.push(Part::Field(Field::parse(&field, |_| true)?));
                }
                '}' => return Err(error("unmatched postprocessor template brace")),
                c => text.push(c),
            }
        }
        if !text.is_empty() {
            parts.push(Part::Text(text));
        }
        Ok(Self { parts })
    }
    pub fn render(&self, context: &Map<String, Value>) -> Result<String> {
        let context = convert_context(context)?;
        let mut output = String::new();
        for part in &self.parts {
            let text = match part {
                Part::Text(text) => text.clone(),
                Part::Field(field) => field
                    .render_content(|name| context.get(name).cloned())?
                    .unwrap_or_else(|| "None".into()),
            };
            if text.len() > MAX_OUTPUT - output.len() {
                return Err(error(
                    "postprocessor template output exceeds the size limit",
                ));
            }
            output.push_str(&text);
        }
        Ok(output)
    }
    pub(crate) fn timestamp(&self, context: &Map<String, Value>) -> Option<i64> {
        let [Part::Field(field)] = self.parts.as_slice() else {
            return None;
        };
        let context = convert_context(context).ok()?;
        match field.render_value(|name| context.get(name).cloned()).ok()? {
            FormatValue::Date(value) => Some(value.unix_timestamp()),
            _ => None,
        }
    }
    pub fn render_value(&self, context: &Map<String, Value>) -> Result<Value> {
        if let [Part::Field(field)] = self.parts.as_slice() {
            let context = convert_context(context)?;
            json_value(field.render_value(|name| context.get(name).cloned())?)
        } else {
            self.render(context).map(Value::String)
        }
    }
}
fn convert_context(
    context: &Map<String, Value>,
) -> Result<std::collections::BTreeMap<String, FormatValue>> {
    let mut budget = MAX_OUTPUT;
    let mut nodes = 1_000_000;
    context
        .iter()
        .map(|(key, value)| {
            Ok((
                key.clone(),
                convert(value, key, true, 0, &mut budget, &mut nodes)?,
            ))
        })
        .collect()
}
fn convert(
    value: &Value,
    key: &str,
    dates: bool,
    depth: usize,
    budget: &mut usize,
    nodes: &mut usize,
) -> Result<FormatValue> {
    if depth > 128 || *nodes == 0 {
        return Err(error("postprocessor context exceeds structural limits"));
    }
    *nodes -= 1;
    let bytes = key.len()
        + match value {
            Value::String(s) => s.len(),
            Value::Number(n) => n.to_string().len(),
            _ => 0,
        };
    *budget = budget
        .checked_sub(bytes)
        .ok_or_else(|| error("postprocessor context exceeds the size limit"))?;
    let date = dates && matches!(key, "date" | "date_updated" | "date_url");
    Ok(match value {
        Value::Null => FormatValue::None,
        Value::Bool(v) => FormatValue::Bool(*v),
        Value::Number(n) if date && n.as_i128().is_some() => {
            crate::naming::format::timestamp(n.as_i128().unwrap())
        }
        Value::Number(n) => {
            if let Some(v) = n.as_i128() {
                FormatValue::Int(v)
            } else {
                if !n.to_string().contains(['.', 'e', 'E']) {
                    return Err(error("unsupported postprocessor context integer"));
                }
                FormatValue::Float(
                    n.as_f64()
                        .filter(|v| v.is_finite())
                        .ok_or_else(|| error("unsupported postprocessor context number"))?,
                )
            }
        }
        Value::String(s) if date => {
            parse_date(s).map_or_else(|_| FormatValue::Str(s.clone()), FormatValue::Date)
        }
        Value::String(s) => FormatValue::Str(s.clone()),
        Value::Array(items) => FormatValue::List(
            items
                .iter()
                .map(|v| convert(v, "", false, depth + 1, budget, nodes))
                .collect::<Result<_>>()?,
        ),
        Value::Object(items) => FormatValue::Map(
            items
                .iter()
                .map(|(k, v)| Ok((k.clone(), convert(v, k, false, depth + 1, budget, nodes)?)))
                .collect::<Result<_>>()?,
        ),
    })
}
pub(crate) fn parse_date(value: &str) -> std::result::Result<time::OffsetDateTime, ()> {
    time::OffsetDateTime::parse(value, &Rfc3339)
        .or_else(|_| {
            let format = time::format_description::parse_borrowed::<2>(
                "[year]-[month]-[day] [hour]:[minute]:[second]",
            )
            .expect("constant date format");
            time::PrimitiveDateTime::parse(value, &format).map(time::PrimitiveDateTime::assume_utc)
        })
        .map_err(|_| ())
}

fn json_value(value: FormatValue) -> Result<Value> {
    Ok(match value {
        FormatValue::None => Value::Null,
        FormatValue::Bool(v) => v.into(),
        FormatValue::Int(v) => Value::Number(
            serde_json::Number::from_i128(v)
                .ok_or_else(|| error("unsupported template integer"))?,
        ),
        FormatValue::Float(v) => Value::Number(
            serde_json::Number::from_f64(v).ok_or_else(|| error("unsupported template number"))?,
        ),
        FormatValue::Str(v) => Value::String(v),
        FormatValue::Date(v) => Value::String(
            v.format(&Rfc3339)
                .map_err(|_| error("unsupported template date"))?,
        ),
        FormatValue::List(v) => Value::Array(v.into_iter().map(json_value).collect::<Result<_>>()?),
        FormatValue::Map(v) => Value::Object(
            v.into_iter()
                .map(|(k, v)| Ok((k, json_value(v)?)))
                .collect::<Result<_>>()?,
        ),
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn source_identity_mixed_missing_and_arbitrary_fields() {
        let context = json!({"x":[1,2],"custom":{"n":7},"empty":false});
        let context = context.as_object().unwrap();
        for (source, expected) in [
            ("{x}", json!([1, 2])),
            ("{missing}", Value::Null),
            ("a{missing}b", json!("aNoneb")),
            ("{custom[n]:03}", json!("007")),
            ("{{x}}\n/path/{custom[n]}", json!("{x}\n/path/7")),
            ("{empty}", json!(false)),
        ] {
            assert_eq!(
                Template::parse(source)
                    .unwrap()
                    .render_value(context)
                    .unwrap(),
                expected
            );
        }
    }
    #[test]
    fn present_empty_values_remain_empty_in_content_and_typed_output() {
        let context = serde_json::json!({"extension":"","nested":{"empty":""}});
        let context = context.as_object().unwrap();
        for (source, expected) in [
            ("{extension}.json", ".json"),
            ("{nested[empty]}", ""),
            ("{extension!s}", ""),
            ("{extension:>0}", ""),
            ("a{missing}b", "aNoneb"),
        ] {
            let template = Template::parse(source).unwrap();
            assert_eq!(template.render(context).unwrap(), expected, "{source}");
            assert_eq!(
                template.render_value(context).unwrap(),
                serde_json::json!(expected),
                "{source}"
            );
        }
    }
    #[test]
    fn formatter_selector_accepts_string_and_rejects_other_dialects_privately() {
        let context = serde_json::json!({"x":7});
        let context = context.as_object().unwrap();
        assert_eq!(
            Template::parse("\u{000c}S {x}")
                .unwrap()
                .render_value(context)
                .unwrap(),
            serde_json::json!(7)
        );
        for selector in ["E", "F", "J", "M", "T", "PRIVATE"] {
            let error = Template::parse(&format!("\u{000c}{selector} PRIVATE"))
                .unwrap_err()
                .to_string();
            assert!(!error.contains("PRIVATE"));
        }
    }
    #[test]
    fn dates_conversion_and_nested_source_dates() {
        let context = json!({"date":"2024-01-02T03:04:05Z","date_updated":1704164645,"tags":[{"date":"raw"}]});
        let context = context.as_object().unwrap();
        assert_eq!(
            Template::parse("{date:%Y-%m-%d}_{date_updated:%H%M%S}")
                .unwrap()
                .render(context)
                .unwrap(),
            "2024-01-02_030405"
        );
        assert_eq!(
            Template::parse("{tags[0][date]}")
                .unwrap()
                .render_value(context)
                .unwrap(),
            json!("raw")
        );
    }
    #[test]
    fn malformed_and_excess_output_are_bounded_and_debug_is_private() {
        assert!(Template::parse("{").is_err());
        assert!(Template::parse(&"x".repeat(MAX_SOURCE + 1)).is_err());
        let template = Template::parse(&"{x}".repeat(300)).unwrap();
        let context = json!({"x":"x".repeat(60000)});
        assert!(template.render(context.as_object().unwrap()).is_err());
        assert!(!format!("{:?}", Template::parse("PRIVATE").unwrap()).contains("PRIVATE"));
    }
}
