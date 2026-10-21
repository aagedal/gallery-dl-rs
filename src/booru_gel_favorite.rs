//! Gelbooru favorites use entry pages followed by lazy post hydration.
use super::*;

enum Page {
    Int(i128),
    Float(f64),
}
impl Page {
    fn text(&self) -> String {
        match self {
            Self::Int(value) => value.to_string(),
            Self::Float(value) => crate::filter::float_repr(*value),
        }
    }
    fn advance(&mut self, reverse: bool) -> Result<bool> {
        match self {
            Self::Int(value) => {
                *value = value
                    .checked_add(if reverse { -1 } else { 1 })
                    .ok_or_else(invalid)?;
                Ok(reverse && *value < 0)
            }
            Self::Float(value) => {
                *value += if reverse { -1.0 } else { 1.0 };
                Ok(reverse && *value < 0.0)
            }
        }
    }
}
fn invalid() -> crate::Error {
    error("invalid Gelbooru favorite response or ordering")
}
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64() != Some(0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}
fn flip(order: Option<&Value>) -> Result<bool> {
    let Some(value) = order.filter(|value| truthy(value)) else {
        return Ok(false);
    };
    match value {
        Value::String(value) => Ok(matches!(value.chars().next(), Some('r' | 'a'))),
        Value::Array(values) => match &values[0] {
            Value::String(value) => Ok(matches!(value.as_str(), "r" | "a")),
            Value::Array(_) | Value::Object(_) => Err(invalid()),
            _ => Ok(false),
        },
        _ => Err(invalid()),
    }
}
fn ascending(favorites: &Value) -> Result<bool> {
    let values = match favorites {
        Value::Array(values) => values,
        Value::Object(_) => return Ok(false), // LookupError for integer key 0.
        Value::String(value) if value.is_empty() => return Ok(false),
        _ => return Err(invalid()),
    };
    let mut ids = Vec::new();
    for index in 0..2 {
        let Some(value) = values.get(index) else {
            return Ok(false);
        };
        let object = value.as_object().ok_or_else(invalid)?;
        let Some(id) = object.get("id") else {
            return Ok(false);
        };
        ids.push(id.clone());
    }
    crate::filter::FilterExpression::parse("left < right")?
        .matches_keywords(&serde_json::Map::from_iter([
            ("left".into(), ids.remove(0)),
            ("right".into(), ids.remove(0)),
        ]))
        .map_err(|_| invalid())
}
fn last_page(count: &Value) -> Result<Page> {
    match count {
        Value::Bool(value) => Ok(Page::Int((i128::from(*value) - 1).div_euclid(100))),
        Value::Number(value) => {
            if let Some(value) = value.as_i128() {
                Ok(Page::Int(
                    value.checked_sub(1).ok_or_else(invalid)?.div_euclid(100),
                ))
            } else {
                let value = value
                    .as_f64()
                    .filter(|value| value.is_finite())
                    .ok_or_else(invalid)?;
                Ok(Page::Float(((value - 1.0) / 100.0).floor()))
            }
        }
        _ => Err(invalid()),
    }
}
fn string(value: &Value) -> Result<String> {
    use crate::naming::format::{Field, Value as F};
    fn convert(value: &Value) -> F {
        match value {
            Value::Null => F::None,
            Value::Bool(value) => F::Bool(*value),
            Value::Number(value) => value.as_i128().map(F::Int).unwrap_or_else(|| {
                if value.to_string().contains(['.', 'e', 'E']) {
                    F::Float(value.as_f64().unwrap_or_default())
                } else {
                    F::Str(value.to_string())
                }
            }),
            Value::String(value) => F::Str(value.clone()),
            Value::Array(values) => F::List(values.iter().map(convert).collect()),
            Value::Object(values) => F::Map(
                values
                    .iter()
                    .map(|(k, v)| (k.clone(), convert(v)))
                    .collect(),
            ),
        }
    }
    Field::parse("value!s", |_| true)?
        .render(|_| Some(convert(value)))?
        .ok_or_else(invalid)
}
fn member_ids(value: &Value) -> Result<Vec<String>> {
    match value {
        Value::Null => Ok(Vec::new()),
        Value::Array(values) => values
            .iter()
            .filter(|value| !value.is_null())
            .map(string)
            .collect(),
        Value::Object(values) => Ok(values.keys().cloned().collect()),
        value => Ok(vec![string(value)?]),
    }
}
fn integer_text(value: &str) -> Option<i64> {
    let value = value.trim_matches(char::is_whitespace);
    let mut chars = value.chars().peekable();
    let mut text = String::new();
    if chars.peek().is_some_and(|c| matches!(c, '+' | '-')) {
        text.push(chars.next()?);
    }
    let mut digit = false;
    for c in chars {
        if c == '_' {
            if !digit {
                return None;
            }
            digit = false;
        } else {
            text.push(char::from(b'0' + crate::python_unicode::decimal_digit(c)?));
            digit = true;
        }
    }
    digit.then(|| text.parse().ok()).flatten()
}
fn favorite_date(value: &Value) -> Value {
    let timestamp = match value {
        Value::Bool(value) => Some(i64::from(*value)),
        Value::Number(value) => value.as_i64().or_else(|| {
            value
                .as_f64()
                .map(f64::trunc)
                .filter(|value| {
                    value.is_finite() && (-62135596800.0..253402300800.0).contains(value)
                })
                .map(|value| value as i64)
        }),
        Value::String(value) => integer_text(value),
        _ => None,
    };
    timestamp
        .and_then(|value| time::OffsetDateTime::from_unix_timestamp(value).ok())
        .filter(|date| (1..=9999).contains(&date.year()))
        .map(|date| {
            format!(
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                date.year(),
                date.month() as u8,
                date.day(),
                date.hour(),
                date.minute(),
                date.second()
            )
            .into()
        })
        .unwrap_or_else(|| "[Invalid DateTime]".into())
}
async fn request(client: &Booru, root: &Url, query: &[(&str, &str)]) -> Result<Value> {
    let mut params = vec![("page", "dapi"), ("q", "index"), ("json", "1")];
    params.extend_from_slice(query);
    let bytes = client
        .call_kind(root, "index.php", &params, RequestKind::Api)
        .await?;
    serde_json::from_slice(&bytes).map_err(|_| invalid())
}
pub(super) struct Favorites {
    id: String,
    page: Option<Page>,
    reverse: bool,
    entries: VecDeque<Value>,
    done: bool,
}
impl Favorites {
    pub(super) fn new(id: String) -> Self {
        Self {
            id,
            page: None,
            reverse: false,
            entries: VecDeque::new(),
            done: false,
        }
    }
    pub(super) async fn next(&mut self, client: &Booru, root: &Url) -> Result<Option<Vec<Value>>> {
        if self.page.is_none() {
            let data = request(
                client,
                root,
                &[("s", "favorite"), ("id", &self.id), ("limit", "2")],
            )
            .await?;
            let count = data
                .get("@attributes")
                .and_then(|value| value.get("count"))
                .ok_or_else(invalid)?;
            let values = data.get("favorite").ok_or_else(invalid)?;
            self.reverse = ascending(values)? ^ flip(client.favorite_order.as_ref())?;
            self.page = Some(if self.reverse {
                last_page(count)?
            } else {
                Page::Int(0)
            });
        }
        loop {
            if let Some(entry) = self.entries.pop_front() {
                let id = entry.get("favorite").ok_or_else(invalid)?;
                let ids = member_ids(id)?;
                let mut params = ids.iter().map(|id| ("id", id.as_str())).collect::<Vec<_>>();
                params.push(("s", "post"));
                let body = request(client, root, &params).await?;
                let mut posts = match body.get("post") {
                    Some(Value::Array(values)) => values.clone(),
                    Some(value) => vec![value.clone()],
                    None if body.is_object() => Vec::new(),
                    _ => return Err(invalid()),
                };
                if !posts.is_empty() {
                    let date = favorite_date(entry.get("added").ok_or_else(invalid)?);
                    for post in &mut posts {
                        post.as_object_mut()
                            .ok_or_else(invalid)?
                            .insert("date_favorited".into(), date.clone());
                    }
                }
                return Ok(Some(posts));
            }
            if self.done {
                return Ok(None);
            }
            let page = self.page.as_mut().expect("favorite probe completed");
            let pid = page.text();
            let body = request(
                client,
                root,
                &[
                    ("s", "favorite"),
                    ("id", &self.id),
                    ("limit", "100"),
                    ("pid", &pid),
                ],
            )
            .await?;
            let mut entries = match body.get("favorite") {
                Some(Value::Array(values)) => values.clone(),
                None if body.is_object() => Vec::new(),
                Some(value) if !self.reverse => vec![value.clone()],
                _ => return Err(invalid()),
            };
            if self.reverse {
                entries.reverse();
            }
            self.done = (!self.reverse && entries.is_empty()) || page.advance(self.reverse)?;
            self.entries = entries.into();
        }
    }
}
