//! A bounded, non-executable subset of gallery-dl's replacement fields.
//!
//! Fields follow `{alternatives[!conversion][:specifiers]}`. Alternatives are
//! `|`-separated keys with `.key`, `[key]`, `[index]`, and `[start:stop]`
//! accessors, or single-quoted literals. Specifiers chain upstream's `?`, `L`,
//! `J`, `R`, `A`, `C`, `S`, and `O` forms before a final Python format spec or,
//! for dates, a `strftime` pattern. Nothing evaluates arbitrary code.
use crate::{Result, error};
use time::{OffsetDateTime, UtcOffset};

/// Intermediate text cannot grow past this many bytes during rendering.
const MAX_TEXT: usize = 65536;
const MAX_WIDTH: usize = 240;

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Value {
    None,
    Bool(bool),
    Int(i128),
    Float(f64),
    Str(String),
    List(Vec<Value>),
    Map(Vec<(String, Value)>),
    Date(OffsetDateTime),
}

impl Value {
    fn truthy(&self) -> bool {
        match self {
            Value::None => false,
            Value::Bool(value) => *value,
            Value::Int(value) => *value != 0,
            Value::Float(value) => *value != 0.0,
            Value::Str(value) => !value.is_empty(),
            Value::List(value) => !value.is_empty(),
            Value::Map(value) => !value.is_empty(),
            Value::Date(_) => true,
        }
    }

    /// Python's `str()` for the supported value types.
    fn text(&self) -> String {
        match self {
            Value::Str(value) => value.clone(),
            Value::Date(value) => strftime(value, "%Y-%m-%d %H:%M:%S").unwrap_or_default(),
            value => repr(value),
        }
    }
}

pub(crate) fn timestamp(seconds: i128) -> Value {
    i64::try_from(seconds)
        .ok()
        .and_then(|seconds| OffsetDateTime::from_unix_timestamp(seconds).ok())
        .map_or(Value::None, Value::Date)
}

fn repr(value: &Value) -> String {
    match value {
        Value::None => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Int(value) => value.to_string(),
        Value::Float(value) => crate::filter::float_repr(*value),
        Value::Str(value) => {
            let quote = if value.contains('\'') && !value.contains('"') {
                '"'
            } else {
                '\''
            };
            let mut output = String::from(quote);
            for c in value.chars() {
                match c {
                    '\\' => output.push_str("\\\\"),
                    '\n' => output.push_str("\\n"),
                    '\r' => output.push_str("\\r"),
                    '\t' => output.push_str("\\t"),
                    c if c == quote => {
                        output.push('\\');
                        output.push(c);
                    }
                    c => output.push(c),
                }
            }
            output.push(quote);
            output
        }
        Value::List(values) => format!(
            "[{}]",
            values.iter().map(repr).collect::<Vec<_>>().join(", ")
        ),
        Value::Map(values) => format!(
            "{{{}}}",
            values
                .iter()
                .map(|(key, value)| format!("{}: {}", repr(&Value::Str(key.clone())), repr(value)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Date(value) => value.unix_timestamp().to_string(),
    }
}

#[derive(Clone, Debug)]
enum Accessor {
    Key(String),
    Index(i64),
    Slice(Option<i64>, Option<i64>),
}

#[derive(Clone, Debug)]
enum Source {
    Literal(String),
    Path {
        name: String,
        accessors: Vec<Accessor>,
    },
}

#[derive(Clone, Copy, Debug)]
enum Arithmetic {
    Add,
    Sub,
    Mul,
}

#[derive(Clone, Debug)]
enum Op {
    Optional { before: String, after: String },
    Limit { max: usize, replacement: String },
    Join(String),
    Replace { old: String, new: String },
    Arithmetic(Arithmetic, i128),
    Convert(Vec<char>),
    Sort { reverse: bool },
    Offset(UtcOffset),
}

#[derive(Clone, Debug)]
pub(crate) struct Field {
    alternatives: Vec<Source>,
    conversion: Option<char>,
    ops: Vec<Op>,
    spec: String,
}

const CONVERSIONS: &str = "lucCtsSrjdDTifgUHW";

impl Field {
    /// Parse the text between braces. `known` validates top-level names.
    pub(crate) fn parse(text: &str, known: impl Fn(&str) -> bool) -> Result<Self> {
        let (head, conversion, spec) = split_field(text)?;
        let mut alternatives = Vec::new();
        for alternative in split_outside(head, '|') {
            alternatives.push(parse_source(alternative, &known)?);
        }
        if let Some(c) = conversion
            && !CONVERSIONS.contains(c)
        {
            return Err(error(format!("unknown template conversion: !{c}")));
        }
        let (ops, spec) = parse_ops(spec)?;
        if !spec.contains('%') {
            // Validate eagerly; date patterns are checked against each value.
            PythonSpec::parse(&spec)?;
        }
        Ok(Self {
            alternatives,
            conversion,
            ops,
            spec,
        })
    }

    pub(crate) fn names(&self) -> impl Iterator<Item = &str> {
        self.alternatives.iter().filter_map(|source| match source {
            Source::Path { name, .. } => Some(name.as_str()),
            Source::Literal(_) => None,
        })
    }

    /// Preserve the source value for a single unformatted replacement field.
    pub(crate) fn render_value(&self, lookup: impl Fn(&str) -> Option<Value>) -> Result<Value> {
        // Python's outer formatter parser treats these as field delimiters
        // even inside its single-quoted literal shorthand. Keep the existing
        // content parser permissiveness separate from metadata identity values.
        if self
            .alternatives
            .iter()
            .any(|source| matches!(source,Source::Literal(value) if value.contains([':', '!'])))
        {
            return Err(error("unsupported identity literal field syntax"));
        }
        let mut value = self.select(lookup);
        if let Some(conversion) = self.conversion {
            value = convert_identity(value, conversion)?;
        }
        apply_identity(&self.ops, &self.spec, value)
    }

    fn select(&self, lookup: impl Fn(&str) -> Option<Value>) -> Value {
        let mut selected = Value::None;
        for source in &self.alternatives {
            let value = match source {
                Source::Literal(value) => Value::Str(value.clone()),
                Source::Path { name, accessors } => {
                    let mut value = lookup(name).unwrap_or(Value::None);
                    for accessor in accessors {
                        value = access(value, accessor);
                    }
                    value
                }
            };
            let found = value.truthy();
            selected = value;
            if found {
                break;
            }
        }
        selected
    }
    /// Render the field. `Ok(None)` reports a missing or empty required value.
    pub(crate) fn render(&self, lookup: impl Fn(&str) -> Option<Value>) -> Result<Option<String>> {
        let optional = self.ops.iter().any(|op| matches!(op, Op::Optional { .. }));
        let output = self.render_content(lookup)?;
        Ok(match output {
            Some(output) if output.is_empty() && !optional => None,
            output => output,
        })
    }
    /// Content formatting retains present empty strings; filename rendering
    /// separately rejects an empty required field.
    pub(crate) fn render_content(
        &self,
        lookup: impl Fn(&str) -> Option<Value>,
    ) -> Result<Option<String>> {
        let mut selected = self.select(lookup);
        if let Some(conversion) = self.conversion {
            selected = convert(selected, conversion)?;
        }
        apply(&self.ops, &self.spec, selected)
    }
}

fn bounded(value: String) -> Result<String> {
    if value.len() > MAX_TEXT {
        return Err(error("template value exceeds supported limits"));
    }
    Ok(value)
}

fn apply(ops: &[Op], spec: &str, value: Value) -> Result<Option<String>> {
    let Some((op, rest)) = ops.split_first() else {
        if value == Value::None {
            return Ok(None);
        }
        return format_value(&value, spec).and_then(bounded).map(Some);
    };
    let value = match op {
        Op::Optional { before, after } => {
            if !value.truthy() {
                return Ok(Some(String::new()));
            }
            return apply(rest, spec, value)?
                .map(|text| bounded(format!("{before}{text}{after}")))
                .transpose();
        }
        Op::Limit { max, replacement } => {
            let Some(text) = apply(rest, spec, value)? else {
                return Ok(None);
            };
            // Upstream replaces, rather than truncates, overlong output.
            if text.chars().count() <= *max {
                return Ok(Some(text));
            }
            let text = replacement.clone();
            return Ok(Some(text));
        }
        _ if value == Value::None => return Ok(None),
        Op::Join(separator) => match value {
            Value::List(values) => Value::Str(bounded(
                values
                    .iter()
                    .map(Value::text)
                    .collect::<Vec<_>>()
                    .join(separator),
            )?),
            value => value,
        },
        Op::Replace { old, new } => match value {
            Value::Str(text) if !old.is_empty() => {
                Value::Str(bounded(text.replace(old.as_str(), new))?)
            }
            Value::Str(text) => Value::Str(text),
            _ => return Err(error("template replacement requires text")),
        },
        Op::Arithmetic(operation, operand) => {
            let overflow = || error("template arithmetic overflowed");
            match value {
                Value::Int(value) => Value::Int(
                    match operation {
                        Arithmetic::Add => value.checked_add(*operand),
                        Arithmetic::Sub => value.checked_sub(*operand),
                        Arithmetic::Mul => value.checked_mul(*operand),
                    }
                    .ok_or_else(overflow)?,
                ),
                Value::Float(value) => Value::Float(match operation {
                    Arithmetic::Add => value + *operand as f64,
                    Arithmetic::Sub => value - *operand as f64,
                    Arithmetic::Mul => value * *operand as f64,
                }),
                _ => return Err(error("template arithmetic requires a number")),
            }
        }
        Op::Convert(conversions) => {
            let mut value = value;
            for conversion in conversions {
                value = convert(value, *conversion)?;
            }
            value
        }
        Op::Sort { reverse } => match value {
            Value::List(mut values) => {
                values.sort_by(|a, b| {
                    a.partial_cmp_value(b)
                        .unwrap_or_else(|| a.text().cmp(&b.text()))
                });
                if *reverse {
                    values.reverse();
                }
                Value::List(values)
            }
            value => value,
        },
        Op::Offset(offset) => match value {
            Value::Date(date) => Value::Date(date.to_offset(*offset)),
            _ => return Err(error("template offsets require a date")),
        },
    };
    apply(rest, spec, value)
}

/// Identity formatting uses typed operations until a specifier actually formats
/// text. Filename/content formatting above deliberately retains its own policy.
fn apply_identity(ops: &[Op], spec: &str, value: Value) -> Result<Value> {
    let Some((op, rest)) = ops.split_first() else {
        return if spec.is_empty() {
            Ok(value)
        } else {
            format_value(&value, spec).and_then(bounded).map(Value::Str)
        };
    };
    let value = match op {
        Op::Optional { before, after } => {
            if !value.truthy() {
                return Ok(Value::Str(String::new()));
            }
            let value = apply_identity(rest, spec, value)?;
            return bounded(format!("{before}{}{after}", identity_text(&value))).map(Value::Str);
        }
        Op::Limit { max, replacement } => {
            let value = apply_identity(rest, spec, value)?;
            let length = match &value {
                Value::Str(v) => v.chars().count(),
                Value::List(v) => v.len(),
                Value::Map(v) => v.len(),
                _ => return Err(error("template length limit requires a sized value")),
            };
            return Ok(if length <= *max {
                value
            } else {
                Value::Str(replacement.clone())
            });
        }
        Op::Join(separator) => match value {
            Value::Str(value) => Value::Str(value),
            Value::List(values) => {
                let mut output = String::new();
                for (index, value) in values.into_iter().enumerate() {
                    let Value::Str(value) = value else {
                        return Err(error("template join requires string elements"));
                    };
                    let extra = value
                        .len()
                        .checked_add(if index == 0 { 0 } else { separator.len() })
                        .ok_or_else(|| error("template join exceeds supported limits"))?;
                    if extra > MAX_TEXT - output.len() {
                        return Err(error("template join exceeds supported limits"));
                    }
                    if index != 0 {
                        output.push_str(separator);
                    }
                    output.push_str(&value);
                }
                Value::Str(output)
            }
            Value::Map(values) => {
                let values = values.into_iter().map(|(key, _)| Value::Str(key)).collect();
                return apply_identity(ops, spec, Value::List(values));
            }
            _ => return Err(error("template join requires an iterable value")),
        },
        Op::Replace { old, new } => {
            let Value::Str(value) = value else {
                return Err(error("template replacement requires text"));
            };
            let count = if old.is_empty() {
                value.chars().count() + 1
            } else {
                value.matches(old.as_str()).count()
            };
            let length = value
                .len()
                .checked_sub(count.saturating_mul(old.len()))
                .and_then(|n| n.checked_add(count.checked_mul(new.len())?))
                .filter(|n| *n <= MAX_TEXT)
                .ok_or_else(|| error("template replacement exceeds supported limits"))?;
            let output = value.replace(old.as_str(), new);
            debug_assert_eq!(length, output.len());
            Value::Str(output)
        }
        Op::Arithmetic(operation, operand) => {
            let value = if let Value::Bool(value) = value {
                Value::Int(value.into())
            } else {
                value
            };
            match value {
                Value::Int(value) => Value::Int(
                    match operation {
                        Arithmetic::Add => value.checked_add(*operand),
                        Arithmetic::Sub => value.checked_sub(*operand),
                        Arithmetic::Mul => value.checked_mul(*operand),
                    }
                    .ok_or_else(|| error("template arithmetic overflowed"))?,
                ),
                Value::Float(value) => {
                    let output = match operation {
                        Arithmetic::Add => value + *operand as f64,
                        Arithmetic::Sub => value - *operand as f64,
                        Arithmetic::Mul => value * *operand as f64,
                    };
                    if !output.is_finite() {
                        return Err(error("template arithmetic exceeds supported limits"));
                    }
                    Value::Float(output)
                }
                Value::Str(value) if matches!(operation, Arithmetic::Mul) => {
                    let count = usize::try_from((*operand).max(0))
                        .map_err(|_| error("template repetition exceeds supported limits"))?;
                    if value.len().checked_mul(count).is_none_or(|n| n > MAX_TEXT) {
                        return Err(error("template repetition exceeds supported limits"));
                    }
                    Value::Str(value.repeat(count))
                }
                Value::List(values) if matches!(operation, Arithmetic::Mul) => {
                    if values.is_empty() {
                        return apply_identity(rest, spec, Value::List(values));
                    }
                    let count = usize::try_from((*operand).max(0))
                        .map_err(|_| error("template repetition exceeds supported limits"))?;
                    let bytes = values
                        .iter()
                        .try_fold(0usize, |total, value| {
                            identity_size(value).and_then(|size| total.checked_add(size))
                        })
                        .ok_or_else(|| error("template repetition exceeds supported limits"))?;
                    if bytes
                        .checked_mul(count)
                        .is_none_or(|n| n > 16 * 1024 * 1024)
                        || values.len().checked_mul(count).is_none_or(|n| n > 100_000)
                    {
                        return Err(error("template repetition exceeds supported limits"));
                    }
                    Value::List((0..count).flat_map(|_| values.iter().cloned()).collect())
                }
                _ => return Err(error("template arithmetic requires compatible values")),
            }
        }
        Op::Convert(conversions) => {
            let mut value = value;
            for conversion in conversions {
                value = convert_identity(value, *conversion)?;
            }
            value
        }
        Op::Sort { reverse } => {
            let mut values = match value {
                Value::List(values) => values,
                Value::Str(value) => value.chars().map(|c| Value::Str(c.to_string())).collect(),
                Value::Map(values) => values.into_iter().map(|(key, _)| Value::Str(key)).collect(),
                _ => return Err(error("template sorting requires an iterable value")),
            };
            if let Some(first) = values.first()
                && values
                    .iter()
                    .skip(1)
                    .any(|value| identity_cmp(first, value).is_none())
            {
                return Err(error("template sorting requires comparable values"));
            }
            values.sort_by(|a, b| {
                let order = identity_cmp(a, b).expect("validated comparable values");
                if *reverse { order.reverse() } else { order }
            });
            Value::List(values)
        }
        Op::Offset(offset) => {
            let Value::Date(value) = value else {
                return Err(error("template offsets require a date"));
            };
            Value::Date(
                value
                    .checked_add(time::Duration::seconds(offset.whole_seconds().into()))
                    .ok_or_else(|| error("template date offset overflowed"))?,
            )
        }
    };
    apply_identity(rest, spec, value)
}
fn identity_size(value: &Value) -> Option<usize> {
    match value {
        Value::Str(value) => Some(value.len() + 1),
        Value::List(values) => values.iter().try_fold(1usize, |total, value| {
            total.checked_add(identity_size(value)?)
        }),
        Value::Map(values) => values.iter().try_fold(1usize, |total, (key, value)| {
            total
                .checked_add(key.len())?
                .checked_add(identity_size(value)?)
        }),
        _ => Some(32),
    }
}
fn identity_cmp(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (Value::Bool(a), Value::Bool(b)) => Some(a.cmp(b)),
        (Value::Bool(a), b) => identity_cmp(&Value::Int((*a).into()), b),
        (a, Value::Bool(b)) => identity_cmp(a, &Value::Int((*b).into())),
        (Value::Int(integer), Value::Float(float)) => identity_int_float_cmp(*integer, *float),
        (Value::Float(float), Value::Int(integer)) => {
            identity_int_float_cmp(*integer, *float).map(std::cmp::Ordering::reverse)
        }
        _ => a.partial_cmp_value(b),
    }
}
fn identity_int_float_cmp(integer: i128, float: f64) -> Option<std::cmp::Ordering> {
    use std::cmp::Ordering;
    if !float.is_finite() {
        return None;
    }
    if float >= -(i128::MIN as f64) {
        return Some(Ordering::Less);
    }
    if float < i128::MIN as f64 {
        return Some(Ordering::Greater);
    }
    Some(match integer.cmp(&(float.trunc() as i128)) {
        Ordering::Equal if float.fract() > 0.0 => Ordering::Less,
        Ordering::Equal if float.fract() < 0.0 => Ordering::Greater,
        order => order,
    })
}

fn identity_repr(value: &Value) -> String {
    match value {
        Value::Date(value) => format!(
            "datetime.datetime({}, {}, {}, {}, {}, {})",
            value.year(),
            u8::from(value.month()),
            value.day(),
            value.hour(),
            value.minute(),
            value.second()
        ),
        Value::List(values) => format!(
            "[{}]",
            values
                .iter()
                .map(identity_repr)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Map(values) => format!(
            "{{{}}}",
            values
                .iter()
                .map(|(key, value)| format!(
                    "{}: {}",
                    repr(&Value::Str(key.clone())),
                    identity_repr(value)
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        value => repr(value),
    }
}
fn identity_text(value: &Value) -> String {
    match value {
        Value::List(_) | Value::Map(_) => identity_repr(value),
        value => value.text(),
    }
}
fn identity_json(value: &Value) -> serde_json::Value {
    match value {
        Value::Date(_) => serde_json::Value::String(identity_text(value)),
        Value::List(values) => values.iter().map(identity_json).collect(),
        Value::Map(values) => values
            .iter()
            .map(|(key, value)| (key.clone(), identity_json(value)))
            .collect(),
        value => json(value),
    }
}
impl Value {
    pub(crate) fn identity_text(&self) -> Result<String> {
        bounded(identity_text(self))
    }
}
fn parse_identity_int(text: &str) -> Option<i128> {
    let text = text.trim();
    let digits = text.strip_prefix(['+', '-']).unwrap_or(text);
    if digits.is_empty()
        || digits.starts_with('_')
        || digits.ends_with('_')
        || digits.contains("__")
        || !digits.bytes().all(|c| c.is_ascii_digit() || c == b'_')
    {
        return None;
    }
    text.replace('_', "").parse().ok()
}
fn identity_int(value: &Value) -> Option<i128> {
    match value {
        Value::Int(value) => Some(*value),
        Value::Bool(value) => Some((*value).into()),
        Value::Float(value)
            if value.is_finite() && *value >= i128::MIN as f64 && *value < i128::MAX as f64 =>
        {
            Some(value.trunc() as i128)
        }
        Value::Str(value) => parse_identity_int(value),
        _ => None,
    }
}
fn convert_identity(value: Value, conversion: char) -> Result<Value> {
    let invalid = || error("template conversion does not support this value");
    Ok(match conversion {
        'i' => Value::Int(identity_int(&value).ok_or_else(invalid)?),
        'f' => {
            let number = match value {
                Value::Int(value) => Some(value as f64),
                Value::Float(value) => Some(value),
                Value::Bool(value) => Some(f64::from(value)),
                Value::Str(value) => value.trim().parse().ok(),
                _ => None,
            };
            Value::Float(number.filter(|n| n.is_finite()).ok_or_else(invalid)?)
        }
        's' => Value::Str(bounded(identity_text(&value))?),
        'r' => Value::Str(bounded(identity_repr(&value))?),
        'S' => {
            let text = if !value.truthy() {
                String::new()
            } else if let Value::List(values) = value {
                values
                    .iter()
                    .map(identity_text)
                    .collect::<Vec<_>>()
                    .join(", ")
            } else {
                identity_text(&value)
            };
            Value::Str(bounded(text)?)
        }
        'j' => {
            let raw = serde_json::to_string(&identity_json(&value))?;
            let mut output = String::new();
            for c in raw.chars() {
                if c as u32 > 0x7e {
                    for code in c.encode_utf16(&mut [0; 2]) {
                        output.push_str(&format!("\\u{code:04x}"));
                    }
                } else {
                    output.push(c);
                }
                if output.len() > MAX_TEXT {
                    return Err(error("template JSON exceeds supported limits"));
                }
            }
            Value::Str(output)
        }
        'd' => identity_int(&value).map_or(Value::None, timestamp),
        'D' => {
            if !value.truthy() {
                Value::None
            } else {
                match value {
                    Value::Date(value) => Value::Date(value),
                    Value::Str(value) => crate::date::parse_date(&value)
                        .map_or(Value::None, |seconds| timestamp(seconds.into())),
                    value => identity_int(&value).map_or(Value::None, timestamp),
                }
            }
        }
        'T' => Value::Str(match value {
            Value::Date(value) => value.unix_timestamp().to_string(),
            _ => String::new(),
        }),
        'l' | 'u' | 'c' | 'C' | 't' | 'g' | 'U' | 'H' | 'W' => {
            if !matches!(value, Value::Str(_)) {
                return Err(invalid());
            }
            convert(value, conversion)?
        }
        _ => convert(value, conversion)?,
    })
}

impl Value {
    fn partial_cmp_value(&self, other: &Value) -> Option<std::cmp::Ordering> {
        match (self, other) {
            (Value::Int(a), Value::Int(b)) => Some(a.cmp(b)),
            (Value::Float(a), Value::Float(b)) => a.partial_cmp(b),
            (Value::Int(a), Value::Float(b)) => (*a as f64).partial_cmp(b),
            (Value::Float(a), Value::Int(b)) => a.partial_cmp(&(*b as f64)),
            (Value::Str(a), Value::Str(b)) => Some(a.cmp(b)),
            (Value::Date(a), Value::Date(b)) => Some(a.cmp(b)),
            _ => None,
        }
    }
}

fn access(value: Value, accessor: &Accessor) -> Value {
    let position = |index: i64, length: usize| -> Option<usize> {
        let index = if index < 0 {
            length as i64 + index
        } else {
            index
        };
        usize::try_from(index).ok().filter(|index| *index < length)
    };
    let bounds = |start: Option<i64>, stop: Option<i64>, length: usize| {
        let clamp = |index: i64| {
            let index = if index < 0 {
                length as i64 + index
            } else {
                index
            };
            index.clamp(0, length as i64) as usize
        };
        let start = start.map_or(0, clamp);
        let stop = stop.map_or(length, clamp);
        (start, stop.max(start))
    };
    match (value, accessor) {
        (Value::Map(values), Accessor::Key(key)) => values
            .into_iter()
            .find(|(name, _)| name == key)
            .map_or(Value::None, |(_, value)| value),
        (Value::Map(values), Accessor::Index(index)) => values
            .into_iter()
            .find(|(name, _)| name == &index.to_string())
            .map_or(Value::None, |(_, value)| value),
        (Value::List(values), Accessor::Index(index)) => {
            position(*index, values.len()).map_or(Value::None, |index| values[index].clone())
        }
        (Value::Str(text), Accessor::Index(index)) => {
            let chars: Vec<char> = text.chars().collect();
            position(*index, chars.len())
                .map_or(Value::None, |index| Value::Str(chars[index].to_string()))
        }
        (Value::List(values), Accessor::Slice(start, stop)) => {
            let (start, stop) = bounds(*start, *stop, values.len());
            Value::List(values[start..stop].to_vec())
        }
        (Value::Str(text), Accessor::Slice(start, stop)) => {
            let chars: Vec<char> = text.chars().collect();
            let (start, stop) = bounds(*start, *stop, chars.len());
            Value::Str(chars[start..stop].iter().collect())
        }
        _ => Value::None,
    }
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        first
            .to_uppercase()
            .chain(chars.flat_map(char::to_lowercase))
            .collect()
    })
}

fn unescape_html(text: &str) -> String {
    let mut output = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('&') {
        output.push_str(&rest[..start]);
        rest = &rest[start..];
        let Some(end) = rest[..rest.len().min(12)].find(';') else {
            output.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some('\u{a0}'),
            _ => entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
                .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                .or_else(|| entity.strip_prefix('#').and_then(|n| n.parse().ok()))
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                output.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                output.push('&');
                rest = &rest[1..];
            }
        }
    }
    output.push_str(rest);
    output
}

fn convert(value: Value, conversion: char) -> Result<Value> {
    if value == Value::None {
        return Ok(match conversion {
            'S' => Value::Str(String::new()),
            _ => Value::None,
        });
    }
    let text = |value: &Value| value.text();
    Ok(match conversion {
        'l' => Value::Str(text(&value).to_lowercase()),
        'u' => Value::Str(text(&value).to_uppercase()),
        'c' => Value::Str(capitalize(&text(&value))),
        'C' => Value::Str(
            text(&value)
                .split_whitespace()
                .map(capitalize)
                .collect::<Vec<_>>()
                .join(" "),
        ),
        't' => Value::Str(text(&value).trim().to_owned()),
        's' => Value::Str(text(&value)),
        'S' => Value::Str(match &value {
            Value::List(values) => values
                .iter()
                .map(Value::text)
                .collect::<Vec<_>>()
                .join(", "),
            value => value.text(),
        }),
        'r' => Value::Str(repr(&value)),
        'j' => Value::Str(serde_json::to_string(&json(&value))?),
        'd' | 'D' => match value {
            Value::Date(date) => Value::Date(date),
            Value::Int(seconds) => timestamp(seconds),
            Value::Float(seconds) if seconds.is_finite() => timestamp(seconds.floor() as i128),
            Value::Str(text) => crate::date::parse_date(&text)
                .map_or(Value::None, |seconds| timestamp(seconds.into())),
            _ => Value::None,
        },
        'T' => match value {
            Value::Date(date) => Value::Str(date.unix_timestamp().to_string()),
            _ => Value::None,
        },
        'i' => match value {
            Value::Int(value) => Value::Int(value),
            Value::Bool(value) => Value::Int(value.into()),
            Value::Float(value) if value.is_finite() => Value::Int(value.trunc() as i128),
            Value::Str(text) => text.trim().parse().map_or(Value::None, Value::Int),
            _ => Value::None,
        },
        'f' => match value {
            Value::Int(value) => Value::Float(value as f64),
            Value::Float(value) => Value::Float(value),
            Value::Str(text) => text.trim().parse().map_or(Value::None, Value::Float),
            _ => Value::None,
        },
        'g' => {
            let mut slug = String::new();
            for c in text(&value).to_lowercase().chars() {
                if c.is_alphanumeric() || c == '_' {
                    slug.push(c);
                } else if (c == '-' || c.is_whitespace()) && !slug.ends_with('-') {
                    slug.push('-');
                }
            }
            Value::Str(slug.trim_matches(['-', '_']).to_owned())
        }
        'U' => Value::Str(unescape_html(&text(&value))),
        'H' => {
            let mut stripped = String::new();
            let mut tag = false;
            for c in text(&value).chars() {
                match c {
                    '<' => tag = true,
                    '>' if tag => tag = false,
                    c if !tag => stripped.push(c),
                    _ => {}
                }
            }
            Value::Str(
                unescape_html(&stripped)
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" "),
            )
        }
        'W' => Value::Str(
            text(&value)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" "),
        ),
        c => return Err(error(format!("unknown template conversion: !{c}"))),
    })
}

fn json(value: &Value) -> serde_json::Value {
    use serde_json::Value as Json;
    match value {
        Value::None => Json::Null,
        Value::Bool(value) => Json::Bool(*value),
        Value::Int(value) => serde_json::Number::from_i128(*value)
            .map_or_else(|| Json::String(value.to_string()), Json::Number),
        Value::Float(value) => {
            serde_json::Number::from_f64(*value).map_or(Json::Null, Json::Number)
        }
        Value::Str(value) => Json::String(value.clone()),
        Value::List(values) => Json::Array(values.iter().map(json).collect()),
        Value::Map(values) => Json::Object(
            values
                .iter()
                .map(|(key, value)| (key.clone(), json(value)))
                .collect(),
        ),
        Value::Date(_) => Json::String(value.text()),
    }
}

/// Split `head[!c][:spec]` at the first unbracketed, unquoted `!` or `:`.
fn split_field(text: &str) -> Result<(&str, Option<char>, &str)> {
    let mut depth = 0usize;
    let mut quoted = false;
    for (offset, c) in text.char_indices() {
        match c {
            '\'' if depth == 0 => quoted = !quoted,
            '[' if !quoted => depth += 1,
            ']' if !quoted => {
                depth = depth
                    .checked_sub(1)
                    .ok_or_else(|| error("unmatched ] in template field"))?;
            }
            '!' | ':' if depth == 0 && !quoted => {
                let head = &text[..offset];
                if c == ':' {
                    return Ok((head, None, &text[offset + 1..]));
                }
                let mut rest = text[offset + 1..].chars();
                let conversion = rest
                    .next()
                    .ok_or_else(|| error("template conversion is missing"))?;
                let rest = rest.as_str();
                return match rest.strip_prefix(':') {
                    Some(spec) => Ok((head, Some(conversion), spec)),
                    None if rest.is_empty() => Ok((head, Some(conversion), "")),
                    None => Err(error("template conversion must be one character")),
                };
            }
            _ => {}
        }
    }
    if depth != 0 || quoted {
        return Err(error("unclosed template field accessor"));
    }
    Ok((text, None, ""))
}

fn split_outside(text: &str, separator: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut quoted = false;
    let mut start = 0;
    for (offset, c) in text.char_indices() {
        if c == '\'' {
            quoted = !quoted;
        } else if c == separator && !quoted {
            parts.push(&text[start..offset]);
            start = offset + c.len_utf8();
        }
    }
    parts.push(&text[start..]);
    parts
}

fn parse_source(text: &str, known: &impl Fn(&str) -> bool) -> Result<Source> {
    if let Some(literal) = text.strip_prefix('\'') {
        let literal = literal
            .strip_suffix('\'')
            .filter(|literal| !literal.contains('\''))
            .ok_or_else(|| error("invalid template string literal"))?;
        return Ok(Source::Literal(literal.into()));
    }
    let identifier = |text: &str| {
        !text.is_empty()
            && text
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    };
    let end = text.find(['.', '[']).unwrap_or(text.len());
    let name = &text[..end];
    if !identifier(name) {
        return Err(error("invalid template field name"));
    }
    if !known(name) {
        return Err(error(format!("unknown filename template field: {name}")));
    }
    let mut accessors = Vec::new();
    let mut rest = &text[end..];
    while !rest.is_empty() {
        if accessors.len() >= 16 {
            return Err(error("template field has too many accessors"));
        }
        if let Some(after) = rest.strip_prefix('.') {
            let end = after.find(['.', '[']).unwrap_or(after.len());
            if !identifier(&after[..end]) {
                return Err(error("invalid template attribute"));
            }
            accessors.push(Accessor::Key(after[..end].into()));
            rest = &after[end..];
        } else if let Some(after) = rest.strip_prefix('[') {
            let end = after
                .find(']')
                .ok_or_else(|| error("unclosed template field accessor"))?;
            let inner = &after[..end];
            let integer = |text: &str| -> Result<Option<i64>> {
                if text.is_empty() {
                    return Ok(None);
                }
                text.parse()
                    .map(Some)
                    .map_err(|_| error("invalid template slice"))
            };
            accessors.push(match inner.split_once(':') {
                Some((start, stop)) => Accessor::Slice(integer(start)?, integer(stop)?),
                None if inner.parse::<i64>().is_ok() => Accessor::Index(inner.parse()?),
                None if identifier(inner) => Accessor::Key(inner.into()),
                None => return Err(error("invalid template field key")),
            });
            rest = &after[end + 1..];
        } else {
            return Err(error("invalid template field accessor"));
        }
    }
    Ok(Source::Path {
        name: name.into(),
        accessors,
    })
}

fn parse_ops(mut spec: &str) -> Result<(Vec<Op>, String)> {
    let mut ops = Vec::new();
    // Each specifier consumes `/`-terminated arguments, like upstream.
    let take = |spec: &mut &str| -> Result<String> {
        let (argument, rest) = spec
            .split_once('/')
            .ok_or_else(|| error("template specifier is missing a / terminator"))?;
        *spec = rest;
        Ok(argument.into())
    };
    while let Some(kind) = spec.chars().next().filter(|c| "?LJRACSO".contains(*c)) {
        if ops.len() >= 16 {
            return Err(error("template field has too many specifiers"));
        }
        spec = &spec[1..];
        ops.push(match kind {
            '?' => Op::Optional {
                before: take(&mut spec)?,
                after: take(&mut spec)?,
            },
            'L' => Op::Limit {
                max: take(&mut spec)?
                    .parse()
                    .ok()
                    .filter(|max| *max <= MAX_TEXT)
                    .ok_or_else(|| error("invalid template length limit"))?,
                replacement: take(&mut spec)?,
            },
            'J' => Op::Join(take(&mut spec)?),
            'R' => Op::Replace {
                old: take(&mut spec)?,
                new: take(&mut spec)?,
            },
            'A' => {
                let argument = take(&mut spec)?;
                let mut chars = argument.chars();
                let operation = match chars.next() {
                    Some('+') => Arithmetic::Add,
                    Some('-') => Arithmetic::Sub,
                    Some('*') => Arithmetic::Mul,
                    _ => return Err(error("template arithmetic must use +, -, or *")),
                };
                let operand = chars
                    .as_str()
                    .parse()
                    .map_err(|_| error("template arithmetic requires an integer"))?;
                Op::Arithmetic(operation, operand)
            }
            'C' => {
                let conversions: Vec<char> = take(&mut spec)?.chars().collect();
                if conversions.is_empty() || conversions.iter().any(|c| !CONVERSIONS.contains(*c)) {
                    return Err(error("unknown template conversion"));
                }
                Op::Convert(conversions)
            }
            'S' => Op::Sort {
                reverse: match take(&mut spec)?.as_str() {
                    "" | "a" => false,
                    "r" | "d" => true,
                    _ => return Err(error("template sort order must be a, r, or d")),
                },
            },
            'O' => {
                let argument = take(&mut spec)?;
                let offset = crate::date::parse_date(&format!("2000-01-01T00:00{argument}"))
                    .ok()
                    .filter(|_| argument.starts_with(['+', '-']))
                    .ok_or_else(|| error("template offsets use +HH:MM or -HH:MM"))?;
                UtcOffset::from_whole_seconds((946684800 - offset) as i32)
                    .map(Op::Offset)
                    .map_err(|_| error("invalid template offset"))?
            }
            _ => unreachable!(),
        });
    }
    Ok((ops, spec.into()))
}

#[derive(Debug, Default)]
struct PythonSpec {
    fill: Option<char>,
    align: Option<char>,
    sign: Option<char>,
    alternate: bool,
    zero: bool,
    width: usize,
    grouping: Option<char>,
    precision: Option<usize>,
    kind: Option<char>,
}

impl PythonSpec {
    fn parse(spec: &str) -> Result<Self> {
        let invalid = || error("unsupported template format specification");
        let mut result = Self::default();
        let chars: Vec<char> = spec.chars().collect();
        let mut i = 0;
        let is_align = |c: char| matches!(c, '<' | '>' | '^' | '=');
        if chars.len() >= 2 && is_align(chars[1]) {
            result.fill = Some(chars[0]);
            result.align = Some(chars[1]);
            i = 2;
        } else if chars.first().copied().is_some_and(is_align) {
            result.align = Some(chars[0]);
            i = 1;
        }
        if let Some(&c) = chars.get(i).filter(|c| matches!(c, '+' | '-' | ' ')) {
            result.sign = Some(c);
            i += 1;
        }
        if chars.get(i) == Some(&'#') {
            result.alternate = true;
            i += 1;
        }
        if chars.get(i) == Some(&'0') {
            result.zero = true;
            i += 1;
        }
        let number = |i: &mut usize| -> Result<Option<usize>> {
            let start = *i;
            while chars.get(*i).is_some_and(char::is_ascii_digit) {
                *i += 1;
            }
            if start == *i {
                return Ok(None);
            }
            let value: usize = chars[start..*i]
                .iter()
                .collect::<String>()
                .parse()
                .map_err(|_| invalid())?;
            if value > MAX_WIDTH {
                return Err(error("template width exceeds 240"));
            }
            Ok(Some(value))
        };
        result.width = number(&mut i)?.unwrap_or(0);
        if let Some(&c) = chars.get(i).filter(|c| matches!(c, ',' | '_')) {
            result.grouping = Some(c);
            i += 1;
        }
        if chars.get(i) == Some(&'.') {
            i += 1;
            result.precision = Some(number(&mut i)?.ok_or_else(invalid)?);
        }
        if let Some(&c) = chars.get(i) {
            if !"sdxXobfFeE%".contains(c) {
                return Err(invalid());
            }
            result.kind = Some(c);
            i += 1;
        }
        if i != chars.len() {
            return Err(invalid());
        }
        Ok(result)
    }

    fn pad(&self, sign: &str, body: &str, default_align: char) -> String {
        let length = sign.chars().count() + body.chars().count();
        let fill = self.fill.unwrap_or(if self.zero { '0' } else { ' ' });
        let align = self.align.unwrap_or(if self.zero && default_align == '>' {
            '='
        } else {
            default_align
        });
        let padding = self.width.saturating_sub(length);
        let repeat = |count: usize| std::iter::repeat_n(fill, count).collect::<String>();
        match align {
            '<' => format!("{sign}{body}{}", repeat(padding)),
            '^' => format!(
                "{}{sign}{body}{}",
                repeat(padding / 2),
                repeat(padding - padding / 2)
            ),
            '=' => format!("{sign}{}{body}", repeat(padding)),
            _ => format!("{}{sign}{body}", repeat(padding)),
        }
    }
}

fn group(digits: &str, separator: char, size: usize) -> String {
    let (integer, fraction) = digits
        .split_once('.')
        .map_or((digits, None), |(a, b)| (a, Some(b)));
    let mut grouped = String::new();
    for (index, c) in integer.chars().enumerate() {
        if index > 0 && (integer.len() - index) % size == 0 {
            grouped.push(separator);
        }
        grouped.push(c);
    }
    if let Some(fraction) = fraction {
        grouped.push('.');
        grouped.push_str(fraction);
    }
    grouped
}

fn format_float(value: f64, kind: char, precision: usize) -> String {
    if value.is_nan() {
        return if kind.is_uppercase() { "NAN" } else { "nan" }.into();
    }
    if value.is_infinite() {
        return if kind.is_uppercase() { "INF" } else { "inf" }.into();
    }
    match kind {
        'e' | 'E' => {
            let text = format!("{:.*e}", precision, value.abs());
            let (digits, exponent) = text.split_once('e').unwrap_or((&text, "0"));
            let exponent: i32 = exponent.parse().unwrap_or(0);
            let text = format!(
                "{digits}e{}{:02}",
                if exponent < 0 { '-' } else { '+' },
                exponent.unsigned_abs()
            );
            if kind == 'E' {
                text.to_uppercase()
            } else {
                text
            }
        }
        '%' => format!("{:.*}%", precision, value.abs() * 100.0),
        _ => format!("{:.*}", precision, value.abs()),
    }
}

fn format_value(value: &Value, spec: &str) -> Result<String> {
    if let Value::Date(date) = value {
        return if spec.is_empty() {
            Ok(value.text())
        } else {
            strftime(date, spec)
        };
    }
    if spec.is_empty() {
        return Ok(value.text());
    }
    let spec = PythonSpec::parse(spec)?;
    let incompatible = || error("template format specification does not match the value");
    match value {
        Value::Str(text) => {
            if spec.sign.is_some()
                || spec.alternate
                || spec.grouping.is_some()
                || spec.align == Some('=')
                || !matches!(spec.kind, None | Some('s'))
            {
                return Err(incompatible());
            }
            let text: String = match spec.precision {
                Some(precision) => text.chars().take(precision).collect(),
                None => text.clone(),
            };
            Ok(spec.pad("", &text, '<'))
        }
        Value::Int(_) | Value::Bool(_) | Value::Float(_) => {
            let number = match value {
                Value::Bool(value) => Value::Int((*value).into()),
                value => value.clone(),
            };
            let (negative, prefix, body) = match (&number, spec.kind) {
                (Value::Int(value), None | Some('d' | 'x' | 'X' | 'o' | 'b')) => {
                    if spec.precision.is_some() {
                        return Err(incompatible());
                    }
                    let magnitude = value.unsigned_abs();
                    let (body, prefix, size) = match spec.kind {
                        Some('x') => (format!("{magnitude:x}"), "0x", 4),
                        Some('X') => (format!("{magnitude:X}"), "0X", 4),
                        Some('o') => (format!("{magnitude:o}"), "0o", 4),
                        Some('b') => (format!("{magnitude:b}"), "0b", 4),
                        _ => (magnitude.to_string(), "", 3),
                    };
                    let body = match spec.grouping {
                        Some(',') if size == 3 => group(&body, ',', 3),
                        Some(',') => return Err(incompatible()),
                        Some(separator) => group(&body, separator, size),
                        None => body,
                    };
                    let prefix = if spec.alternate { prefix } else { "" };
                    (*value < 0, prefix, body)
                }
                (Value::Int(_), _) | (Value::Float(_), _) => {
                    let value = match number {
                        Value::Int(value) => value as f64,
                        Value::Float(value) => value,
                        _ => unreachable!(),
                    };
                    let body = match spec.kind {
                        None if spec.precision.is_none() => crate::filter::float_repr(value.abs()),
                        Some(kind @ ('f' | 'F' | 'e' | 'E' | '%')) => {
                            format_float(value, kind, spec.precision.unwrap_or(6))
                        }
                        _ => return Err(incompatible()),
                    };
                    let body = match spec.grouping {
                        Some(separator) if !body.contains(['e', 'E']) => {
                            let (digits, suffix) = body
                                .strip_suffix('%')
                                .map_or((body.as_str(), ""), |digits| (digits, "%"));
                            format!("{}{suffix}", group(digits, separator, 3))
                        }
                        _ => body,
                    };
                    (
                        value.is_sign_negative() && !value.is_nan() && value != 0.0,
                        "",
                        body,
                    )
                }
                _ => unreachable!(),
            };
            let sign = match (negative, spec.sign) {
                (true, _) => "-",
                (false, Some('+')) => "+",
                (false, Some(' ')) => " ",
                _ => "",
            };
            Ok(spec.pad(&format!("{sign}{prefix}"), &body, '>'))
        }
        _ => Err(incompatible()),
    }
}

const WEEKDAYS: [&str; 7] = [
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
    "Sunday",
];
const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// The portable C89 `strftime` directives plus `%f`, `%z`, and `%:z`.
fn strftime(date: &OffsetDateTime, pattern: &str) -> Result<String> {
    let mut output = String::new();
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            output.push(c);
            continue;
        }
        let weekday = date.weekday().number_days_from_monday() as usize;
        let month = u8::from(date.month()) as usize - 1;
        let hour12 = match date.hour() % 12 {
            0 => 12,
            hour => hour,
        };
        let offset = date.offset();
        let offset_text = |colon: bool| {
            let seconds = offset.whole_seconds();
            let sign = if seconds < 0 { '-' } else { '+' };
            let seconds = seconds.unsigned_abs();
            let separator = if colon { ":" } else { "" };
            format!(
                "{sign}{:02}{separator}{:02}",
                seconds / 3600,
                seconds / 60 % 60
            )
        };
        match chars.next() {
            Some('a') => output.push_str(&WEEKDAYS[weekday][..3]),
            Some('A') => output.push_str(WEEKDAYS[weekday]),
            Some('b') => output.push_str(&MONTHS[month][..3]),
            Some('B') => output.push_str(MONTHS[month]),
            Some('d') => output.push_str(&format!("{:02}", date.day())),
            Some('f') => output.push_str(&format!("{:06}", date.microsecond())),
            Some('H') => output.push_str(&format!("{:02}", date.hour())),
            Some('I') => output.push_str(&format!("{hour12:02}")),
            Some('j') => output.push_str(&format!("{:03}", date.ordinal())),
            Some('m') => output.push_str(&format!("{:02}", month + 1)),
            Some('M') => output.push_str(&format!("{:02}", date.minute())),
            Some('p') => output.push_str(if date.hour() < 12 { "AM" } else { "PM" }),
            Some('S') => output.push_str(&format!("{:02}", date.second())),
            Some('w') => output.push_str(&((weekday + 1) % 7).to_string()),
            Some('y') => output.push_str(&format!("{:02}", date.year().rem_euclid(100))),
            Some('Y') => output.push_str(&format!("{:04}", date.year())),
            Some('z') => output.push_str(&offset_text(false)),
            Some(':') if chars.next() == Some('z') => output.push_str(&offset_text(true)),
            Some('Z') if offset.is_utc() => output.push_str("UTC"),
            Some('Z') => {}
            Some('%') => output.push('%'),
            _ => return Err(error("unsupported template date directive")),
        }
        if output.len() > MAX_TEXT {
            return Err(error("template value exceeds supported limits"));
        }
    }
    Ok(output)
}
