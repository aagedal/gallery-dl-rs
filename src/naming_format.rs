//! A bounded, non-executable subset of gallery-dl's replacement fields.
//!
//! Fields follow `{alternatives[!conversion][:specifiers]}`. Alternatives are
//! `|`-separated keys with `.key`, `[key]`, `[index]`, and `[start:stop]`
//! accessors, or single-quoted literals. Specifiers chain upstream's `?`, `L`,
//! `J`, `R`, `A`, `C`, `S`, and `O` forms before a final Python format spec or,
//! for dates, a `strftime` pattern. Nothing evaluates arbitrary code.
use crate::{Result, error};
use time::{OffsetDateTime, UtcOffset};

#[path = "source_offset.rs"]
mod source_offset;

/// Intermediate text cannot grow past this many bytes during rendering.
const MAX_TEXT: usize = 65536;
const MAX_WIDTH: usize = 240;

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Value {
    None,
    Missing,
    BigInt(String),
    Bool(bool),
    Int(i128),
    Float(f64),
    Str(String),
    List(Vec<Value>),
    Map(Vec<(String, Value)>),
    Date(OffsetDateTime),
    InvalidDate,
    NullDate(OffsetDateTime),
    SourceDate(OffsetDateTime),
    SourcePrimitive(Box<Value>),
}

impl Value {
    fn truthy(&self) -> bool {
        match self {
            Value::None | Value::Missing | Value::InvalidDate | Value::NullDate(_) => false,
            Value::BigInt(value) => value.bytes().any(|byte| matches!(byte, b'1'..=b'9')),
            Value::SourcePrimitive(value) => value.truthy(),
            Value::Bool(value) => *value,
            Value::Int(value) => *value != 0,
            Value::Float(value) => *value != 0.0,
            Value::Str(value) => !value.is_empty(),
            Value::List(value) => !value.is_empty(),
            Value::Map(value) => !value.is_empty(),
            Value::Date(_) | Value::SourceDate(_) => true,
        }
    }

    /// Python's `str()` for the supported value types.
    fn text(&self) -> String {
        match self {
            Value::Str(value) => value.clone(),
            Value::SourcePrimitive(value) => value.text(),
            Value::InvalidDate | Value::NullDate(_) => "[Invalid DateTime]".into(),
            Value::Date(value) | Value::SourceDate(value) => {
                let mut text = strftime(value, "%Y-%m-%d %H:%M:%S").unwrap_or_default();
                if value.microsecond() != 0 {
                    text.push_str(&format!(".{:06}", value.microsecond()));
                }
                if value.offset() != UtcOffset::UTC {
                    let seconds = value.offset().whole_seconds();
                    text.push(if seconds < 0 { '-' } else { '+' });
                    let seconds = seconds.unsigned_abs();
                    text.push_str(&format!("{:02}:{:02}", seconds / 3600, seconds / 60 % 60));
                    if seconds % 60 != 0 {
                        text.push_str(&format!(":{:02}", seconds % 60));
                    }
                }
                text
            }
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

pub(crate) fn source_timestamp(seconds: i64) -> Value {
    OffsetDateTime::from_unix_timestamp(seconds).map_or(Value::InvalidDate, Value::SourceDate)
}

fn repr(value: &Value) -> String {
    match value {
        Value::None | Value::Missing => "None".into(),
        Value::BigInt(value) => value.clone(),
        Value::SourcePrimitive(value) => repr(value),
        Value::InvalidDate => "NullDatetime(1, 1, 1, 0, 0)".into(),
        Value::NullDate(date) => datetime_repr(date, "NullDatetime"),
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
        Value::Date(_) | Value::SourceDate(_) => identity_repr(value),
    }
}

#[derive(Clone, Debug)]
enum Accessor {
    Attribute(String),
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
    Identity,
    Optional { before: String, after: String },
    Limit { max: usize, replacement: String },
    Join(String),
    Replace { old: String, new: String },
    Arithmetic(Arithmetic, i128),
    Convert(Vec<char>),
    Sort { reverse: bool },
    Offset(source_offset::Offset),
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
        Self::parse_mode(text, known, false)
    }
    pub(crate) fn parse_source(text: &str, known: impl Fn(&str) -> bool) -> Result<Self> {
        Self::parse_mode(text, known, true)
    }
    fn parse_mode(text: &str, known: impl Fn(&str) -> bool, source: bool) -> Result<Self> {
        let (head, conversion, spec) = split_field(text)?;
        if source && head.is_empty() {
            return Ok(Self {
                alternatives: vec![Source::Literal(String::new())],
                conversion: None,
                ops: Vec::new(),
                spec: String::new(),
            });
        }
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
        if !source
            && !spec.contains('%')
            && !alternatives.iter().any(
                |source| matches!(source, Source::Path { name, .. } if name == "date_favorited"),
            )
        {
            // Source datetimes permit literal strftime text; primitive values
            // still validate their final specification at rendering time.
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

    pub(crate) fn identity_path(&self) -> Option<Vec<String>> {
        if self.conversion.is_some() || !self.ops.is_empty() || !self.spec.is_empty() {
            return None;
        }
        let [Source::Path { name, accessors }] = self.alternatives.as_slice() else {
            return None;
        };
        let mut path = vec![name.clone()];
        for accessor in accessors {
            match accessor {
                Accessor::Key(key) | Accessor::Attribute(key) => path.push(key.clone()),
                _ => return None,
            }
        }
        Some(path)
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
        if matches!(value, Value::SourcePrimitive(_))
            && self.conversion.is_none()
            && self.ops.is_empty()
            && self.spec.is_empty()
        {
            return Ok(value);
        }
        while let Value::SourcePrimitive(inner) = value {
            value = *inner;
        }
        if let Some(conversion) = self.conversion {
            value = convert_identity(value, conversion)?;
        }
        apply_identity(&self.ops, &self.spec, value)
    }

    fn select(&self, lookup: impl Fn(&str) -> Option<Value>) -> Value {
        self.select_with_default(lookup, Value::None)
    }
    fn select_with_default(&self, lookup: impl Fn(&str) -> Option<Value>, default: Value) -> Value {
        let mut selected = default.clone();
        for source in &self.alternatives {
            let value = match source {
                Source::Literal(value) => Value::Str(value.clone()),
                Source::Path { name, accessors } => {
                    let mut value = lookup(name).unwrap_or_else(|| default.clone());
                    for accessor in accessors {
                        value = access(value, accessor);
                    }
                    if default == Value::None {
                        fn normalize_missing(value: Value) -> Value {
                            match value {
                                Value::Missing => Value::None,
                                Value::SourcePrimitive(value) => {
                                    Value::SourcePrimitive(Box::new(normalize_missing(*value)))
                                }
                                value => value,
                            }
                        }
                        normalize_missing(value)
                    } else {
                        value
                    }
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
        let output = self.render_content_with_default(lookup, Value::None)?;
        Ok(match output {
            Some(output) if output.is_empty() && !optional => None,
            output => output,
        })
    }
    pub(crate) fn render_default_value(
        &self,
        lookup: impl Fn(&str) -> Option<Value>,
    ) -> Result<Value> {
        if self.ops.iter().any(|op| matches!(op, Op::Identity)) {
            let mut value = self.select_with_default(lookup, Value::Missing);
            while let Value::SourcePrimitive(inner) = value {
                value = *inner;
            }
            if let Some(conversion) = self.conversion {
                value = convert_source(value, conversion)?;
            }
            return apply_identity(&self.ops, &self.spec, value);
        }
        if let Some(conversion) = self.conversion
            && self.ops.is_empty()
            && self.spec.is_empty()
        {
            let mut value = self.select_with_default(lookup, Value::Missing);
            while let Value::SourcePrimitive(inner) = value {
                value = *inner;
            }
            return convert_source(value, conversion);
        }
        Ok(Value::Str(
            self.render_content(lookup)?
                .unwrap_or_else(|| "None".into()),
        ))
    }
    /// Content formatting retains present empty strings; filename rendering
    /// separately rejects an empty required field.
    pub(crate) fn render_content(
        &self,
        lookup: impl Fn(&str) -> Option<Value>,
    ) -> Result<Option<String>> {
        self.render_content_with_default(lookup, Value::Missing)
    }
    fn render_content_with_default(
        &self,
        lookup: impl Fn(&str) -> Option<Value>,
        default: Value,
    ) -> Result<Option<String>> {
        let source = default == Value::Missing;
        let mut selected = self.select_with_default(lookup, default);
        while let Value::SourcePrimitive(inner) = selected {
            selected = *inner;
        }
        if let Some(conversion) = self.conversion {
            selected = if source {
                convert_source(selected, conversion)?
            } else {
                convert(selected, conversion)?
            };
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
        Op::Identity => return bounded(value.text()).map(Some),
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
        _ if value == Value::None && !matches!(op, Op::Convert(_)) => return Ok(None),
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
                value = convert_source(value, *conversion)?;
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
        Op::Offset(offset) => offset_date(value, *offset)?,
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
        Op::Identity => return Ok(value),
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
        Op::Offset(offset) => offset_date(value, *offset)?,
    };
    apply_identity(rest, spec, value)
}
fn identity_size(value: &Value) -> Option<usize> {
    match value {
        Value::SourcePrimitive(value) => identity_size(value),
        Value::Str(value) | Value::BigInt(value) => Some(value.len() + 1),
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

fn datetime_repr(value: &OffsetDateTime, class: &str) -> String {
    let mut output = format!(
        "{class}({}, {}, {}, {}, {}",
        value.year(),
        u8::from(value.month()),
        value.day(),
        value.hour(),
        value.minute()
    );
    if value.second() != 0 || value.microsecond() != 0 {
        output.push_str(&format!(", {}", value.second()));
    }
    if value.microsecond() != 0 {
        output.push_str(&format!(", {}", value.microsecond()));
    }
    if value.offset() != UtcOffset::UTC {
        output.push_str(&format!(
            ", tzinfo=datetime.timezone(datetime.timedelta(seconds={}))",
            value.offset().whole_seconds()
        ));
    }
    output.push(')');
    output
}
fn offset_date(value: Value, offset: source_offset::Offset) -> Result<Value> {
    let (date, invalid) = match value {
        Value::Date(date) | Value::SourceDate(date) => (date, false),
        Value::InvalidDate => (
            OffsetDateTime::from_unix_timestamp(-62_135_596_800).expect("year one"),
            true,
        ),
        Value::NullDate(date) => (date, true),
        _ => return Err(error("template offsets require a date")),
    };
    let seconds = offset.seconds(date)?;
    let date = date
        .checked_add(time::Duration::seconds(seconds))
        .filter(|date| (1..=9999).contains(&date.year()))
        .ok_or_else(|| error("template date offset overflowed"))?;
    Ok(if invalid {
        Value::NullDate(date)
    } else {
        Value::SourceDate(date)
    })
}
fn identity_repr(value: &Value) -> String {
    match value {
        Value::SourcePrimitive(value) => identity_repr(value),
        Value::Date(value) | Value::SourceDate(value) => datetime_repr(value, "datetime.datetime"),
        Value::NullDate(value) => datetime_repr(value, "NullDatetime"),
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
        Value::Date(_) | Value::SourceDate(_) | Value::InvalidDate | Value::NullDate(_) => {
            serde_json::Value::String(identity_text(value))
        }
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
    Ok(match conversion {
        'i' | 'f' => convert_source(value, conversion)?,
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
        'd' | 'D' => convert_date(value, conversion == 'D'),
        'T' => Value::Str(match value {
            Value::Date(value) | Value::SourceDate(value) => value.unix_timestamp().to_string(),
            Value::InvalidDate => "-62135596800".into(),
            Value::NullDate(date) => date.unix_timestamp().to_string(),
            _ => String::new(),
        }),
        'l' | 'u' | 'c' | 'C' | 't' | 'g' | 'U' | 'H' | 'W' => convert_source(value, conversion)?,
        _ => convert(value, conversion)?,
    })
}

fn convert_date(value: Value, iso: bool) -> Value {
    use serde_json::Value as Json;
    let value = match value {
        Value::SourcePrimitive(value) => return convert_date(*value, iso),
        Value::Date(value) | Value::SourceDate(value) if iso => {
            return crate::source_date::parse_timestamp(&Json::from(value.unix_timestamp()))
                .map_or(Value::InvalidDate, source_timestamp);
        }
        Value::Str(value) => Json::String(value),
        Value::Int(value) => match serde_json::Number::from_i128(value) {
            Some(value) => Json::Number(value),
            None => return Value::InvalidDate,
        },
        Value::Float(value) => match serde_json::Number::from_f64(value) {
            Some(value) => Json::Number(value),
            None => return Value::InvalidDate,
        },
        Value::Bool(value) => Json::Bool(value),
        _ => return Value::InvalidDate,
    };
    let seconds = if iso {
        crate::source_date::convert(&value)
    } else {
        crate::source_date::parse_timestamp(&value)
    };
    seconds.map_or(Value::InvalidDate, source_timestamp)
}

impl Value {
    fn partial_cmp_value(&self, other: &Value) -> Option<std::cmp::Ordering> {
        let datetime = |value: &Value| match value {
            Value::Date(value) | Value::SourceDate(value) | Value::NullDate(value) => {
                Some(value.unix_timestamp_nanos())
            }
            Value::InvalidDate => Some(-62_135_596_800i128 * 1_000_000_000),
            _ => None,
        };
        if let (Some(a), Some(b)) = (datetime(self), datetime(other)) {
            return Some(a.cmp(&b));
        }
        match (self, other) {
            (Value::Int(a), Value::Int(b)) => Some(a.cmp(b)),
            (Value::Float(a), Value::Float(b)) => a.partial_cmp(b),
            (Value::Int(a), Value::Float(b)) => (*a as f64).partial_cmp(b),
            (Value::Float(a), Value::Int(b)) => a.partial_cmp(&(*b as f64)),
            (Value::Str(a), Value::Str(b)) => Some(a.cmp(b)),
            (Value::Date(a) | Value::SourceDate(a), Value::Date(b) | Value::SourceDate(b)) => {
                Some(a.cmp(b))
            }
            _ => None,
        }
    }
}

fn access(value: Value, accessor: &Accessor) -> Value {
    if let Value::SourcePrimitive(inner) = value {
        return Value::SourcePrimitive(Box::new(access(*inner, accessor)));
    }
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
        (
            Value::Date(date) | Value::SourceDate(date) | Value::NullDate(date),
            Accessor::Attribute(name),
        ) => match name.as_str() {
            "year" => Value::Int(date.year().into()),
            "month" => Value::Int(u8::from(date.month()).into()),
            "day" => Value::Int(date.day().into()),
            "hour" => Value::Int(date.hour().into()),
            "minute" => Value::Int(date.minute().into()),
            "second" => Value::Int(date.second().into()),
            "microsecond" => Value::Int(date.microsecond().into()),
            "fold" => Value::Int(0),
            "tzinfo" => Value::None,
            _ => Value::Missing,
        },
        (Value::InvalidDate, Accessor::Attribute(name)) => match name.as_str() {
            "year" | "month" | "day" => Value::Int(1),
            "hour" | "minute" | "second" | "microsecond" | "fold" => Value::Int(0),
            "tzinfo" => Value::None,
            _ => Value::Missing,
        },
        (Value::Map(values), Accessor::Key(key) | Accessor::Attribute(key)) => values
            .into_iter()
            .find(|(name, _)| name == key)
            .map_or(Value::Missing, |(_, value)| value),
        (Value::Map(values), Accessor::Index(index)) => values
            .into_iter()
            .find(|(name, _)| name == &index.to_string())
            .map_or(Value::Missing, |(_, value)| value),
        (Value::List(values), Accessor::Index(index)) => {
            position(*index, values.len()).map_or(Value::Missing, |index| values[index].clone())
        }
        (Value::Str(text), Accessor::Index(index)) => {
            let chars: Vec<char> = text.chars().collect();
            position(*index, chars.len())
                .map_or(Value::Missing, |index| Value::Str(chars[index].to_string()))
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
        _ => Value::Missing,
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

fn convert_source(value: Value, conversion: char) -> Result<Value> {
    let invalid = || error("template conversion does not support this value");
    if matches!(value, Value::Missing) {
        return Ok(match conversion {
            'i' => Value::Int(0),
            'f' => Value::Float(0.0),
            'd' => source_timestamp(0),
            'D' => Value::InvalidDate,
            's' | 'r' | 'T' => Value::Str("None".into()),
            'S' | 'C' | 'H' => Value::Str(String::new()),
            'g' => Value::Str("none".into()),
            'j' => Value::Str("null".into()),
            'U' => Value::Missing,
            _ => return Err(invalid()),
        });
    }
    match conversion {
        'i' => match &value {
            Value::BigInt(_) => Ok(value),
            Value::Str(text) => python_integer(text).ok_or_else(invalid),
            _ => identity_int(&value).map(Value::Int).ok_or_else(invalid),
        },
        'f' => {
            let number = match &value {
                Value::Str(text) => {
                    let text = python_float_text(text).ok_or_else(invalid)?;
                    text.parse::<f64>().ok()
                }
                Value::Int(number) => Some(*number as f64),
                Value::BigInt(number) => number
                    .parse::<f64>()
                    .ok()
                    .filter(|number| number.is_finite()),
                Value::Float(number) => Some(*number),
                Value::Bool(number) => Some(f64::from(*number)),
                _ => None,
            }
            .ok_or_else(invalid)?;
            Ok(Value::Float(number))
        }
        'l' | 'u' | 'c' | 'C' | 't' | 'U' | 'W' => {
            if !matches!(value, Value::Str(_)) {
                return Err(invalid());
            }
            convert(value, conversion)
        }
        'g' => convert(Value::Str(value.text()), conversion),
        'H' if !value.truthy() => Ok(Value::Str(String::new())),
        'H' => {
            if !matches!(value, Value::Str(_)) {
                return Err(invalid());
            }
            convert(value, conversion)
        }
        's' | 'S' | 'r' | 'j' | 'd' | 'D' | 'T' => convert_identity(value, conversion),
        _ => Err(invalid()),
    }
}
fn python_integer(text: &str) -> Option<Value> {
    let text = text.trim_matches(char::is_whitespace);
    let (sign, text) = match text.as_bytes().first() {
        Some(b'+') => ("", &text[1..]),
        Some(b'-') => ("-", &text[1..]),
        _ => ("", text),
    };
    let mut digits = String::new();
    let mut previous = false;
    for character in text.chars() {
        if character == '_' {
            if !previous {
                return None;
            }
            previous = false;
        } else {
            let digit = crate::python_unicode::decimal_digit(character)?;
            digits.push(char::from(b'0' + digit));
            if digits.len() > 4300 {
                return None;
            }
            previous = true;
        }
    }
    if !previous {
        return None;
    }
    let digits = digits.trim_start_matches('0');
    let number = if digits.is_empty() {
        "0".into()
    } else {
        format!("{sign}{digits}")
    };
    Some(
        number
            .parse::<i128>()
            .map_or_else(|_| Value::BigInt(number), Value::Int),
    )
}
fn python_float_text(text: &str) -> Option<String> {
    let text = text.trim_matches(char::is_whitespace);
    let characters: Vec<_> = text.chars().collect();
    let mut result = String::new();
    for (index, character) in characters.iter().copied().enumerate() {
        if character == '_' {
            if index == 0
                || index + 1 == characters.len()
                || crate::python_unicode::decimal_digit(characters[index - 1]).is_none()
                || crate::python_unicode::decimal_digit(characters[index + 1]).is_none()
            {
                return None;
            }
        } else if let Some(digit) = crate::python_unicode::decimal_digit(character) {
            result.push(char::from(b'0' + digit));
        } else {
            result.push(character);
        }
    }
    if result.is_empty() {
        None
    } else {
        Some(result)
    }
}
fn convert(value: Value, conversion: char) -> Result<Value> {
    if value == Value::Missing {
        return Ok(match conversion {
            'd' => source_timestamp(0),
            'D' => Value::InvalidDate,
            'T' => Value::Str("None".into()),
            'S' => Value::Str(String::new()),
            _ => Value::None,
        });
    }
    if value == Value::None {
        return Ok(match conversion {
            'S' => Value::Str(String::new()),
            'd' | 'D' => Value::InvalidDate,
            'T' => Value::Str(String::new()),
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
        'd' | 'D' => convert_date(value, conversion == 'D'),
        'T' => match value {
            Value::Date(date) | Value::SourceDate(date) => {
                Value::Str(date.unix_timestamp().to_string())
            }
            Value::InvalidDate => Value::Str("-62135596800".into()),
            Value::NullDate(date) => Value::Str(date.unix_timestamp().to_string()),
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
        Value::None | Value::Missing => Json::Null,
        Value::BigInt(value) => serde_json::from_str(value).unwrap_or(Json::Null),
        Value::SourcePrimitive(value) => json(value),
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
        Value::Date(_) | Value::SourceDate(_) | Value::InvalidDate | Value::NullDate(_) => {
            Json::String(value.text())
        }
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
    if !identifier(name) && !known(name) {
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
            if !identifier(&after[..end]) && after[..end].parse::<i64>().is_err() {
                return Err(error("invalid template attribute"));
            }
            accessors.push(match after[..end].parse::<i64>() {
                Ok(index) => Accessor::Index(index),
                Err(_) => Accessor::Attribute(after[..end].into()),
            });
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
    while let Some(kind) = spec.chars().next().filter(|c| "?LJRACSOI".contains(*c)) {
        if ops.len() >= 16 {
            return Err(error("template field has too many specifiers"));
        }
        spec = &spec[1..];
        if kind == 'I' {
            ops.push(Op::Identity);
            return Ok((ops, String::new()));
        }
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
                let (argument, rest) = spec.split_once('/').unwrap_or((spec, ""));
                spec = rest;
                Op::Offset(source_offset::Offset::parse(argument)?)
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
    if matches!(value, Value::InvalidDate | Value::NullDate(_)) {
        return Ok("[Invalid DateTime]".into());
    }
    if let Value::Date(date) | Value::SourceDate(date) = value {
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
            Some('z') if !offset.is_utc() => output.push_str(&offset_text(false)),
            Some('z') => {}
            Some(':') if chars.next() == Some('z') => {
                if !offset.is_utc() {
                    output.push_str(&offset_text(true));
                }
            }
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

#[cfg(test)]
mod strict_missing_tests {
    use super::{Field, Value};
    #[test]
    fn strict_required_fields_keep_missing_errors_while_source_content_uses_default() {
        let field = Field::parse("user[name]", |_| true).unwrap();
        assert_eq!(field.render(|_| None).unwrap(), None);
        assert_eq!(field.render_content(|_| None).unwrap(), Some("None".into()));
        let date = Field::parse_source("missing!d", |_| true).unwrap();
        assert!(
            matches!(date.render_default_value(|_| None).unwrap(), Value::SourceDate(value) if value.unix_timestamp() == 0)
        );
        let present_null = Field::parse_source("value!d", |_| true).unwrap();
        assert_eq!(
            present_null
                .render_default_value(|_| Some(Value::None))
                .unwrap(),
            Value::InvalidDate
        );
    }
    #[test]
    fn source_bare_conversions_retain_types_and_reject_private_invalid_values() {
        assert_eq!(
            Field::parse_source("value!i", |_| true)
                .unwrap()
                .render_default_value(|_| Some(Value::Str("١_２".into())))
                .unwrap(),
            Value::Int(12)
        );
        assert_eq!(
            Field::parse_source("value!f", |_| true)
                .unwrap()
                .render_default_value(|_| Some(Value::Str("-１.２_５".into())))
                .unwrap(),
            Value::Float(-1.25)
        );
        for conversion in ['i', 'f'] {
            let field = Field::parse_source(&format!("value!{conversion}"), |_| true).unwrap();
            assert!(field.render_default_value(|_| Some(Value::None)).is_err());
            let error = field
                .render_default_value(|_| Some(Value::Str("PRIVATE_BAD_NUMBER".into())))
                .unwrap_err();
            assert!(!error.to_string().contains("PRIVATE_BAD_NUMBER"));
        }
        assert_eq!(
            Field::parse_source("missing!i", |_| true)
                .unwrap()
                .render_default_value(|_| None)
                .unwrap(),
            Value::Int(0)
        );
        assert_eq!(
            Field::parse_source("missing!f", |_| true)
                .unwrap()
                .render_default_value(|_| None)
                .unwrap(),
            Value::Float(0.0)
        );
        assert_eq!(
            Field::parse_source("none!g", |_| true)
                .unwrap()
                .render_default_value(|_| Some(Value::None))
                .unwrap(),
            Value::Str("none".into())
        );
        assert_eq!(
            Field::parse_source("none!j", |_| true)
                .unwrap()
                .render_default_value(|_| Some(Value::None))
                .unwrap(),
            Value::Str("null".into())
        );
        assert!(
            Field::parse_source("none!l", |_| true)
                .unwrap()
                .render_default_value(|_| Some(Value::None))
                .is_err()
        );
        assert_eq!(
            Field::parse_source("value:Ci/", |_| true)
                .unwrap()
                .render_default_value(|_| Some(Value::Str("12".into())))
                .unwrap(),
            Value::Str("12".into())
        );
    }
    #[test]
    fn source_empty_fields_ignore_empty_key_and_conversions_but_strict_fields_reject() {
        for field in ["", "!D", "!d", "!x", ":invalid"] {
            assert_eq!(
                Field::parse_source(field, |_| true)
                    .unwrap()
                    .render_default_value(|_| Some(Value::Int(99)))
                    .unwrap(),
                Value::Str(String::new())
            );
        }
        assert!(Field::parse("", |_| false).is_err());
    }
    #[test]
    fn source_formatted_conversions_and_conversion_chains_propagate_private_errors() {
        for (text, value) in [
            ("raw!i:03", Value::Str("PRIVATE_BAD_INT".into())),
            ("raw!f:.2f", Value::Str("PRIVATE_BAD_FLOAT".into())),
            ("raw!l:>8", Value::Int(1)),
            ("raw:Ci/03", Value::Str("PRIVATE_BAD_INT".into())),
            ("raw:Cf/.2f", Value::Str("PRIVATE_BAD_FLOAT".into())),
        ] {
            let field = Field::parse_source(text, |_| true).unwrap();
            let error = field
                .render_default_value(|_| Some(value.clone()))
                .unwrap_err();
            assert!(!error.to_string().contains("PRIVATE_"));
        }
        let strict = Field::parse("raw!i:03", |_| true).unwrap();
        assert_eq!(
            strict.render(|_| Some(Value::Str("bad".into()))).unwrap(),
            None
        );
    }
}
