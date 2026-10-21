//! Bounded, side-effect-free evaluation of the supported expression AST.
use super::{BinaryOp, CompareOp, Expr, OrderedMap, UnaryOp, Value};
use crate::{Result, error};
use std::{cmp::Ordering, collections::BTreeMap, sync::Arc};

const STEPS: usize = 16_384;
const BYTES: usize = 1024 * 1024;
const ITEMS: usize = 4096;
const DEPTH: usize = 64;
fn invalid() -> crate::Error {
    error("filter expression could not be evaluated")
}

pub(super) fn truthy(value: &Value) -> bool {
    match value {
        Value::None => false,
        Value::SourceDate(value) => value.is_some(),
        Value::Bool(v) => *v,
        Value::Int(v) => *v != 0,
        Value::Float(v) => *v != 0.0,
        Value::String(v) => !v.is_empty(),
        Value::List(v) | Value::Tuple(v) => !v.is_empty(),
        Value::Map(v) => !v.is_empty(),
    }
}
pub(super) fn evaluate(expr: &Expr, context: &BTreeMap<String, Value>) -> Result<Value> {
    Evaluator {
        left: STEPS,
        work_left: 16 * 1024 * 1024,
        allocation_left: BYTES,
        depth: 0,
        context,
    }
    .eval(expr)
}
struct Evaluator<'a> {
    left: usize,
    work_left: usize,
    allocation_left: usize,
    depth: usize,
    context: &'a BTreeMap<String, Value>,
}
impl Evaluator<'_> {
    fn step(&mut self) -> Result<()> {
        self.left = self.left.checked_sub(1).ok_or_else(invalid)?;
        Ok(())
    }
    fn eval(&mut self, expression: &Expr) -> Result<Value> {
        self.step()?;
        if self.depth >= DEPTH {
            return Err(invalid());
        }
        self.depth += 1;
        let result = self.inner(expression);
        self.depth -= 1;
        let value = result?;
        self.check(&value)?;
        Ok(value)
    }
    fn check(&mut self, value: &Value) -> Result<()> {
        self.touch(value)?;
        // Arc-backed context values are cheap to return, but sequences and
        // transformations must not smuggle oversized intermediate values onward.
        match value {
            Value::Float(v) if !v.is_finite() => Err(invalid()),
            Value::String(v) if v.len() > BYTES => Err(invalid()),
            Value::List(v) | Value::Tuple(v) if v.len() > ITEMS => Err(invalid()),
            Value::Map(v) if v.len() > ITEMS => Err(invalid()),
            _ => Ok(()),
        }
    }
    fn inner(&mut self, expression: &Expr) -> Result<Value> {
        Ok(match expression {
            Expr::Literal(value) => value.clone(),
            Expr::Name(name) => self.context.get(name).cloned().ok_or_else(invalid)?,
            Expr::List(values) => {
                self.allocate_sequence(values.len())?;
                if values.len() > ITEMS {
                    return Err(invalid());
                }
                Value::List(
                    values
                        .iter()
                        .map(|v| self.eval(v))
                        .collect::<Result<Vec<_>>>()?
                        .into(),
                )
            }
            Expr::Tuple(values) => {
                self.allocate_sequence(values.len())?;
                if values.len() > ITEMS {
                    return Err(invalid());
                }
                Value::Tuple(
                    values
                        .iter()
                        .map(|v| self.eval(v))
                        .collect::<Result<Vec<_>>>()?
                        .into(),
                )
            }
            Expr::Map(values) => {
                if values.len() > ITEMS {
                    return Err(invalid());
                }
                let mut map = OrderedMap::default();
                for (key, value) in values {
                    self.allocate(
                        key.len()
                            .checked_mul(2)
                            .and_then(|n| n.checked_add(128))
                            .ok_or_else(invalid)?,
                    )?;
                    map.insert(key.clone(), self.eval(value)?);
                }
                Value::Map(Arc::new(map))
            }
            Expr::Index(value, index) => {
                let value = self.eval(value)?;
                let index = self.eval(index)?;
                self.index(&value, &index)?
            }
            Expr::Unary(op, value) => {
                let value = self.eval(value)?;
                match op {
                    UnaryOp::Not => Value::Bool(!truthy(&value)),
                    UnaryOp::Pos => number_value(&value)?,
                    UnaryOp::Neg => match number_value(&value)? {
                        Value::Int(v) => Value::Int(v.checked_neg().ok_or_else(invalid)?),
                        Value::Float(v) => Value::Float(-v),
                        _ => unreachable!(),
                    },
                }
            }
            Expr::Binary(left, BinaryOp::And, right) => {
                let left = self.eval(left)?;
                if truthy(&left) {
                    self.eval(right)?
                } else {
                    left
                }
            }
            Expr::Binary(left, BinaryOp::Or, right) => {
                let left = self.eval(left)?;
                if truthy(&left) {
                    left
                } else {
                    self.eval(right)?
                }
            }
            Expr::Binary(left, op, right) => {
                let left = self.eval(left)?;
                let right = self.eval(right)?;
                self.binary(&left, *op, &right)?
            }
            Expr::Compare(first, comparisons) => {
                let mut left = self.eval(first)?;
                let mut matches = true;
                for (op, next) in comparisons {
                    let right = self.eval(next)?;
                    if !self.compare(&left, *op, &right)? {
                        matches = false;
                        break;
                    }
                    left = right;
                }
                Value::Bool(matches)
            }
            Expr::Call(name, arguments) => {
                self.allocate_sequence(arguments.len())?;
                let arguments = arguments
                    .iter()
                    .map(|v| self.eval(v))
                    .collect::<Result<Vec<_>>>()?;
                self.call(name, &arguments)?
            }
            Expr::Method(value, method, arguments) => {
                let value = self.eval(value)?;
                self.allocate_sequence(arguments.len())?;
                let arguments = arguments
                    .iter()
                    .map(|v| self.eval(v))
                    .collect::<Result<Vec<_>>>()?;
                self.method(&value, method, &arguments)?
            }
            Expr::Conditional { test, yes, no } => {
                if truthy(&self.eval(test)?) {
                    self.eval(yes)?
                } else {
                    self.eval(no)?
                }
            }
        })
    }
    fn index(&mut self, value: &Value, index: &Value) -> Result<Value> {
        match value {
            Value::Map(values) => values.get(string(index)?).cloned().ok_or_else(invalid),
            Value::List(values) | Value::Tuple(values) => values
                .get(position(integer(index)?, values.len())?)
                .cloned()
                .ok_or_else(invalid),
            Value::String(value) => {
                let index = position(integer(index)?, value.chars().count())?;
                let character = value.chars().nth(index).ok_or_else(invalid)?;
                self.allocate(character.len_utf8())?;
                Ok(Value::String(character.to_string().into()))
            }
            _ => Err(invalid()),
        }
    }
    fn equal(&mut self, left: &Value, right: &Value) -> Result<bool> {
        self.step()?;
        self.touch(left)?;
        self.touch(right)?;
        if numeric(left) && numeric(right) {
            return Ok(numeric_cmp(left, right)? == Ordering::Equal);
        }
        Ok(match (left, right) {
            (Value::None, Value::None) => true,
            (Value::SourceDate(a), Value::SourceDate(b)) => {
                a.unwrap_or(-62_135_596_800) == b.unwrap_or(-62_135_596_800)
            }
            (Value::String(a), Value::String(b)) => a == b,
            (Value::List(a), Value::List(b)) | (Value::Tuple(a), Value::Tuple(b)) => {
                if a.len() != b.len() {
                    false
                } else {
                    let mut equal = true;
                    for (a, b) in a.iter().zip(b.iter()) {
                        if !self.equal(a, b)? {
                            equal = false;
                            break;
                        }
                    }
                    equal
                }
            }
            (Value::Map(a), Value::Map(b)) => {
                if a.len() != b.len() {
                    false
                } else {
                    let mut equal = true;
                    for (key, av) in a.iter() {
                        if let Some(bv) = b.get(key)
                            && self.equal(av, bv)?
                        {
                            continue;
                        }
                        equal = false;
                        break;
                    }
                    equal
                }
            }
            _ => false,
        })
    }
    fn order(&mut self, left: &Value, right: &Value) -> Result<Ordering> {
        self.step()?;
        self.touch(left)?;
        self.touch(right)?;
        if numeric(left) && numeric(right) {
            return numeric_cmp(left, right);
        }
        match (left, right) {
            (Value::SourceDate(a), Value::SourceDate(b)) => Ok(a
                .unwrap_or(-62_135_596_800)
                .cmp(&b.unwrap_or(-62_135_596_800))),
            (Value::String(a), Value::String(b)) => Ok(a.cmp(b)),
            (Value::List(a), Value::List(b)) | (Value::Tuple(a), Value::Tuple(b)) => {
                for (a, b) in a.iter().zip(b.iter()) {
                    if !self.equal(a, b)? {
                        return self.order(a, b);
                    }
                }
                Ok(a.len().cmp(&b.len()))
            }
            _ => Err(invalid()),
        }
    }
    fn contains(&mut self, container: &Value, item: &Value) -> Result<bool> {
        self.touch(container)?;
        self.touch(item)?;
        match container {
            Value::String(value) => Ok(value.contains(string(item)?)),
            Value::Map(values) => Ok(values.contains_key(string(item)?)),
            Value::List(values) | Value::Tuple(values) => {
                for value in values.iter() {
                    if self.equal(value, item)? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            _ => Err(invalid()),
        }
    }
    fn compare(&mut self, left: &Value, op: CompareOp, right: &Value) -> Result<bool> {
        Ok(match op {
            CompareOp::Eq => self.equal(left, right)?,
            CompareOp::Ne => !self.equal(left, right)?,
            CompareOp::In => self.contains(right, left)?,
            CompareOp::NotIn => !self.contains(right, left)?,
            CompareOp::Is | CompareOp::IsNot => {
                let same = match (left, right) {
                    (Value::None, Value::None) => true,
                    (Value::Bool(a), Value::Bool(b)) => a == b,
                    (Value::String(a), Value::String(b)) => Arc::ptr_eq(a, b),
                    (Value::List(a), Value::List(b)) | (Value::Tuple(a), Value::Tuple(b)) => {
                        Arc::ptr_eq(a, b)
                    }
                    (Value::Map(a), Value::Map(b)) => Arc::ptr_eq(a, b),
                    _ => false,
                };
                if matches!(op, CompareOp::IsNot) {
                    !same
                } else {
                    same
                }
            }
            _ => {
                let order = self.order(left, right)?;
                match op {
                    CompareOp::Lt => order.is_lt(),
                    CompareOp::Le => !order.is_gt(),
                    CompareOp::Gt => order.is_gt(),
                    CompareOp::Ge => !order.is_lt(),
                    _ => unreachable!(),
                }
            }
        })
    }
    fn binary(&mut self, left: &Value, op: BinaryOp, right: &Value) -> Result<Value> {
        if matches!(op, BinaryOp::Add) {
            match (left, right) {
                (Value::String(a), Value::String(b)) => {
                    if a.len().checked_add(b.len()).is_none_or(|n| n > BYTES) {
                        return Err(invalid());
                    }
                    self.allocate(a.len() + b.len())?;
                    return Ok(Value::String(format!("{a}{b}").into()));
                }
                (Value::List(a), Value::List(b)) => {
                    if a.len().checked_add(b.len()).is_none_or(|n| n > ITEMS) {
                        return Err(invalid());
                    }
                    self.allocate_sequence(a.len() + b.len())?;
                    return Ok(Value::List(a.iter().chain(b.iter()).cloned().collect()));
                }
                (Value::Tuple(a), Value::Tuple(b)) => {
                    if a.len().checked_add(b.len()).is_none_or(|n| n > ITEMS) {
                        return Err(invalid());
                    }
                    self.allocate_sequence(a.len() + b.len())?;
                    return Ok(Value::Tuple(a.iter().chain(b.iter()).cloned().collect()));
                }
                _ => {}
            }
        }
        if matches!(op, BinaryOp::Mul) {
            match (left, right) {
                (Value::String(_) | Value::List(_) | Value::Tuple(_), value) if numeric(value) => {
                    return self.repeat(left, integer(value)?);
                }
                (value, Value::String(_) | Value::List(_) | Value::Tuple(_)) if numeric(value) => {
                    return self.repeat(right, integer(value)?);
                }
                _ => {}
            }
        }
        let a = number_value(left)?;
        let b = number_value(right)?;
        if let (Value::Int(a), Value::Int(b)) = (&a, &b) {
            let value = match op {
                BinaryOp::Add => a.checked_add(*b),
                BinaryOp::Sub => a.checked_sub(*b),
                BinaryOp::Mul => a.checked_mul(*b),
                BinaryOp::FloorDiv => floor_div(*a, *b),
                BinaryOp::Mod => {
                    let rem = if *b == -1 {
                        0
                    } else {
                        a.checked_rem(*b).ok_or_else(invalid)?
                    };
                    Some(if rem != 0 && (rem < 0) != (*b < 0) {
                        rem.checked_add(*b).ok_or_else(invalid)?
                    } else {
                        rem
                    })
                }
                BinaryOp::Pow if *b >= 0 => Some(match (*a, *b) {
                    (_, 0) | (1, _) => 1,
                    (0, _) => 0,
                    (-1, exponent) => {
                        if exponent % 2 == 0 {
                            1
                        } else {
                            -1
                        }
                    }
                    (_, exponent) => a
                        .checked_pow(u32::try_from(exponent).map_err(|_| invalid())?)
                        .ok_or_else(invalid)?,
                }),
                BinaryOp::Div => return Ok(Value::Float(int_ratio(*a, *b)?)),
                BinaryOp::Pow => None,
                _ => return Err(invalid()),
            };
            if !matches!(op, BinaryOp::Div) && !(matches!(op, BinaryOp::Pow) && *b < 0) {
                return value.map(Value::Int).ok_or_else(invalid);
            }
        }
        let a = float(&a)?;
        let b = float(&b)?;
        let value = match op {
            BinaryOp::Add => a + b,
            BinaryOp::Sub => a - b,
            BinaryOp::Mul => a * b,
            BinaryOp::Div if b != 0.0 => a / b,
            BinaryOp::FloorDiv if b != 0.0 => float_divmod(a, b).0,
            BinaryOp::Mod if b != 0.0 => float_divmod(a, b).1,
            BinaryOp::Pow if !(a == 0.0 && b < 0.0) => a.powf(b),
            _ => return Err(invalid()),
        };
        if value.is_finite() {
            Ok(Value::Float(value))
        } else {
            Err(invalid())
        }
    }
    fn repeat(&mut self, value: &Value, count: i128) -> Result<Value> {
        if matches!(value, Value::String(v) if v.is_empty()) {
            return Ok(Value::String(Arc::from("")));
        }
        if matches!(value, Value::List(v) if v.is_empty()) {
            return Ok(Value::List(Arc::from([])));
        }
        if matches!(value, Value::Tuple(v) if v.is_empty()) {
            return Ok(Value::Tuple(Arc::from([])));
        }
        let count = usize::try_from(count.max(0)).map_err(|_| invalid())?;
        match value {
            Value::String(value) => {
                if value.is_empty() {
                    return Ok(Value::String(Arc::from("")));
                }
                if value.len().checked_mul(count).is_none_or(|n| n > BYTES) {
                    return Err(invalid());
                }
                self.allocate(value.len() * count)?;
                Ok(Value::String(value.repeat(count).into()))
            }
            Value::List(value) => {
                if value.is_empty() {
                    return Ok(Value::List(Arc::from([])));
                }
                if value.len().checked_mul(count).is_none_or(|n| n > ITEMS) {
                    return Err(invalid());
                }
                self.allocate_sequence(value.len() * count)?;
                Ok(Value::List(
                    (0..count).flat_map(|_| value.iter().cloned()).collect(),
                ))
            }
            Value::Tuple(values) => {
                if values.len().checked_mul(count).is_none_or(|n| n > ITEMS) {
                    return Err(invalid());
                }
                self.allocate_sequence(values.len() * count)?;
                Ok(Value::Tuple(
                    (0..count).flat_map(|_| values.iter().cloned()).collect(),
                ))
            }
            _ => Err(invalid()),
        }
    }
    fn sequence(&mut self, value: &Value) -> Result<Vec<Value>> {
        self.touch(value)?;
        match value {
            Value::List(values) | Value::Tuple(values) => {
                self.allocate_sequence(values.len())?;
                Ok(values.to_vec())
            }
            Value::Map(values) => {
                self.allocate_sequence(values.len())?;
                let mut output = Vec::with_capacity(values.len());
                for key in values.keys() {
                    output.push(self.copy_string(key)?);
                }
                Ok(output)
            }
            Value::String(value) => {
                let length = value.chars().count();
                self.allocate_sequence(length)?;
                let mut output = Vec::with_capacity(length);
                for c in value.chars() {
                    self.allocate(c.len_utf8())?;
                    output.push(Value::String(c.to_string().into()));
                }
                Ok(output)
            }
            _ => Err(invalid()),
        }
    }
    fn call(&mut self, name: &str, args: &[Value]) -> Result<Value> {
        match (name, args) {
            ("len", [value]) => Ok(Value::Int(match value {
                Value::String(v) => v.chars().count(),
                Value::List(v) | Value::Tuple(v) => v.len(),
                Value::Map(v) => v.len(),
                _ => return Err(invalid()),
            } as i128)),
            ("bool", []) => Ok(Value::Bool(false)),
            ("bool", [value]) => Ok(Value::Bool(truthy(value))),
            ("str", []) => Ok(Value::String(Arc::from(""))),
            ("str", [Value::String(value)]) => Ok(Value::String(value.clone())),
            ("str", [value]) => Ok(Value::String(self.display(value, false, 0)?.into())),
            ("int", []) => Ok(Value::Int(0)),
            ("int", [Value::String(value)]) => Ok(Value::Int(parse_int(value, 10)?)),
            ("int", [Value::String(value), base]) => {
                Ok(Value::Int(parse_int(value, integer(base)?)?))
            }
            ("int", [value]) => Ok(Value::Int(match number_value(value)? {
                Value::Int(v) => v,
                Value::Float(v) => trunc_int(v)?,
                _ => unreachable!(),
            })),
            ("float", []) => Ok(Value::Float(0.0)),
            ("float", [Value::String(value)]) => {
                let cleaned;
                let value = if value.contains('_') {
                    let bytes = value.as_bytes();
                    if bytes.iter().enumerate().any(|(index, byte)| {
                        *byte == b'_'
                            && (index == 0
                                || index + 1 == bytes.len()
                                || !bytes[index - 1].is_ascii_digit()
                                || !bytes[index + 1].is_ascii_digit())
                    }) {
                        return Err(invalid());
                    }
                    self.allocate(value.len())?;
                    cleaned = value.replace('_', "");
                    cleaned.as_str()
                } else {
                    value.as_ref()
                };
                let value = value.trim().parse::<f64>().map_err(|_| invalid())?;
                if !value.is_finite() {
                    return Err(invalid());
                }
                Ok(Value::Float(value))
            }
            ("float", [value]) => Ok(Value::Float(float(value)?)),
            ("abs", [value]) => Ok(match number_value(value)? {
                Value::Int(v) => Value::Int(v.checked_abs().ok_or_else(invalid)?),
                Value::Float(v) => Value::Float(v.abs()),
                _ => unreachable!(),
            }),
            ("round", [value]) | ("round", [value, Value::None]) => self.round(value, None),
            ("round", [value, digits]) => self.round(value, Some(integer(digits)?)),
            ("contains", [container, item]) => self.contains_call(container, item, Some(" ")),
            ("contains", [container, item, Value::None]) => {
                self.contains_call(container, item, None)
            }
            ("contains", [container, item, separator]) => {
                self.contains_call(container, item, Some(string(separator)?))
            }
            ("any" | "all", [value]) => {
                let all = name == "all";
                for item in self.sequence(value)? {
                    self.step()?;
                    if truthy(&item) != all {
                        return Ok(Value::Bool(!all));
                    }
                }
                Ok(Value::Bool(all))
            }
            ("min" | "max", args) if !args.is_empty() => {
                let values = if args.len() == 1 {
                    self.sequence(&args[0])?
                } else {
                    self.allocate_sequence(args.len())?;
                    args.to_vec()
                };
                let mut iter = values.into_iter();
                let mut best = iter.next().ok_or_else(invalid)?;
                for value in iter {
                    let order = self.order(&value, &best)?;
                    if if name == "min" {
                        order.is_lt()
                    } else {
                        order.is_gt()
                    } {
                        best = value;
                    }
                }
                Ok(best)
            }
            _ => Err(invalid()),
        }
    }
    fn round(&mut self, value: &Value, digits: Option<i128>) -> Result<Value> {
        match number_value(value)? {
            Value::Int(value) => {
                let digits = digits.unwrap_or(0);
                if digits >= 0 {
                    return Ok(Value::Int(value));
                }
                if digits < -38 {
                    return Ok(Value::Int(0));
                }
                let divisor = 10i128.checked_pow((-digits) as u32).ok_or_else(invalid)?;
                let quotient = value / divisor;
                let remainder = value % divisor;
                let abs = remainder.checked_abs().ok_or_else(invalid)?;
                let half = divisor / 2;
                let rounded = if abs > half || (abs == half && quotient % 2 != 0) {
                    quotient
                        .checked_add(if value < 0 { -1 } else { 1 })
                        .ok_or_else(invalid)?
                } else {
                    quotient
                };
                Ok(Value::Int(
                    rounded.checked_mul(divisor).ok_or_else(invalid)?,
                ))
            }
            Value::Float(value) => match digits {
                None => Ok(Value::Int(trunc_int(value.round_ties_even())?)),
                Some(digits) if digits > 323 => Ok(Value::Float(value)),
                Some(digits) if digits < -308 => Ok(Value::Float(0.0f64.copysign(value))),
                Some(digits) if digits >= 0 => {
                    let result = format!("{value:.precision$}", precision = digits as usize)
                        .parse::<f64>()
                        .map_err(|_| invalid())?;
                    Ok(Value::Float(result))
                }
                Some(digits) => Ok(Value::Float(round_negative(value, (-digits) as usize)?)),
            },
            _ => unreachable!(),
        }
    }
    fn display(&mut self, value: &Value, nested: bool, depth: usize) -> Result<String> {
        self.step()?;
        if depth >= DEPTH {
            return Err(invalid());
        }
        let result = match value {
            Value::None => "None".into(),
            Value::SourceDate(value) => super::source_date_text(*value),
            Value::Bool(true) => "True".into(),
            Value::Bool(false) => "False".into(),
            Value::Int(value) => value.to_string(),
            Value::Float(value) => {
                let result = float_repr(*value);
                self.allocate(result.len())?;
                result
            }
            Value::String(value) if !nested => {
                self.allocate(value.len())?;
                value.to_string()
            }
            Value::String(value) => {
                let quote = if value.contains('\'') && !value.contains('"') {
                    '"'
                } else {
                    '\''
                };
                let mut result = String::new();
                self.append(&mut result, &quote.to_string())?;
                for c in value.chars() {
                    let piece = match c {
                        '\\' => "\\\\".into(),
                        '\n' => "\\n".into(),
                        '\r' => "\\r".into(),
                        '\t' => "\\t".into(),
                        c if c == quote => format!("\\{c}"),
                        c if nonprintable(c) => {
                            if (c as u32) <= 255 {
                                format!("\\x{:02x}", c as u32)
                            } else if (c as u32) <= 65535 {
                                format!("\\u{:04x}", c as u32)
                            } else {
                                format!("\\U{:08x}", c as u32)
                            }
                        }
                        c => c.to_string(),
                    };
                    self.append(&mut result, &piece)?;
                }
                self.append(&mut result, &quote.to_string())?;
                result
            }
            Value::List(values) => {
                self.allocate(1)?;
                let mut output = String::from("[");
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        self.append(&mut output, ", ")?;
                    }
                    let piece = self.display(value, true, depth + 1)?;
                    self.append(&mut output, &piece)?;
                }
                self.append(&mut output, "]")?;
                output
            }
            Value::Tuple(values) => {
                self.allocate(1)?;
                let mut output = String::from("(");
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        self.append(&mut output, ", ")?;
                    }
                    let piece = self.display(value, true, depth + 1)?;
                    self.append(&mut output, &piece)?;
                }
                if values.len() == 1 {
                    self.append(&mut output, ",")?;
                }
                self.append(&mut output, ")")?;
                output
            }
            Value::Map(values) => {
                self.allocate(1)?;
                let mut output = String::from("{");
                for (index, (key, value)) in values.iter().enumerate() {
                    if index > 0 {
                        self.append(&mut output, ", ")?;
                    }
                    append(
                        &mut output,
                        &self.display(&Value::String(Arc::from(key.as_str())), true, depth + 1)?,
                    )?;
                    self.append(&mut output, ": ")?;
                    let piece = self.display(value, true, depth + 1)?;
                    self.append(&mut output, &piece)?;
                }
                self.append(&mut output, "}")?;
                output
            }
        };
        if result.len() > BYTES {
            return Err(invalid());
        }
        Ok(result)
    }
    fn contains_call(
        &mut self,
        values: &Value,
        elements: &Value,
        separator: Option<&str>,
    ) -> Result<Value> {
        let split;
        let values = if let Value::String(text) = values {
            if separator != Some("") {
                split = self.split(text, separator, None)?;
                &split
            } else {
                values
            }
        } else {
            values
        };
        if let Value::List(elements) | Value::Tuple(elements) = elements {
            for element in elements.iter() {
                self.step()?;
                if self.contains(values, element)? {
                    return Ok(Value::Bool(true));
                }
            }
            Ok(Value::Bool(false))
        } else {
            Ok(Value::Bool(self.contains(values, elements)?))
        }
    }
    fn method(&mut self, value: &Value, name: &str, args: &[Value]) -> Result<Value> {
        self.touch(value)?;
        if let Value::Map(values) = value {
            return match (name, args) {
                ("get", [key]) => Ok(values.get(string(key)?).cloned().unwrap_or(Value::None)),
                ("get", [key, default]) => Ok(values
                    .get(string(key)?)
                    .cloned()
                    .unwrap_or_else(|| default.clone())),
                _ => Err(invalid()),
            };
        }
        if let Value::List(values) | Value::Tuple(values) = value {
            if let ("count", [needle]) = (name, args) {
                let mut count = 0;
                for value in values.iter() {
                    if self.equal(value, needle)? {
                        count += 1;
                    }
                }
                return Ok(Value::Int(count));
            }
            return Err(invalid());
        }
        let text = string(value)?;
        match (name, args) {
            ("lower" | "upper", []) => {
                // Unicode case conversion can expand a scalar up to threefold.
                if text.len() > BYTES / 3 {
                    return Err(invalid());
                }
                self.allocate(text.len() * 3)?;
                bounded_string(if name == "lower" {
                    text.to_lowercase()
                } else {
                    text.to_uppercase()
                })
            }
            ("startswith" | "endswith", [pattern]) => {
                let found = match pattern {
                    Value::String(pattern) => {
                        if name == "startswith" {
                            text.starts_with(pattern.as_ref())
                        } else {
                            text.ends_with(pattern.as_ref())
                        }
                    }
                    Value::Tuple(patterns) => {
                        let mut found = false;
                        for pattern in patterns.iter() {
                            self.step()?;
                            let pattern = string(pattern)?;
                            self.work(pattern.len())?;
                            if if name == "startswith" {
                                text.starts_with(pattern)
                            } else {
                                text.ends_with(pattern)
                            } {
                                found = true;
                                break;
                            }
                        }
                        found
                    }
                    _ => return Err(invalid()),
                };
                Ok(Value::Bool(found))
            }
            ("strip" | "lstrip" | "rstrip", [])
            | ("strip" | "lstrip" | "rstrip", [Value::None]) => self.copy_string(match name {
                "strip" => text.trim_matches(python_space),
                "lstrip" => text.trim_start_matches(python_space),
                _ => text.trim_end_matches(python_space),
            }),
            ("strip" | "lstrip" | "rstrip", [characters]) => {
                let characters = string(characters)?;
                // A lookup set makes trimming linear rather than rescanning a
                // caller-controlled character string for every boundary byte.
                self.allocate(characters.len().checked_mul(8).ok_or_else(invalid)?)?;
                let set: std::collections::HashSet<_> = characters.chars().collect();
                let predicate = |c| set.contains(&c);
                self.copy_string(match name {
                    "strip" => text.trim_matches(predicate),
                    "lstrip" => text.trim_start_matches(predicate),
                    _ => text.trim_end_matches(predicate),
                })
            }
            ("replace", [old, new]) => self.replace(text, string(old)?, string(new)?, None),
            ("replace", [old, new, count]) => {
                self.replace(text, string(old)?, string(new)?, Some(integer(count)?))
            }
            ("count", [pattern]) => {
                let pattern = string(pattern)?;
                Ok(Value::Int(if pattern.is_empty() {
                    text.chars().count() + 1
                } else {
                    text.matches(pattern).count()
                } as i128))
            }
            ("find" | "index", [pattern]) => {
                let found = text
                    .find(string(pattern)?)
                    .map(|offset| text[..offset].chars().count() as i128);
                if name == "index" && found.is_none() {
                    return Err(invalid());
                }
                Ok(Value::Int(found.unwrap_or(-1)))
            }
            ("split", []) | ("split", [Value::None]) => self.split(text, None, None),
            ("split", [separator]) => self.split(text, Some(string(separator)?), None),
            ("split", [Value::None, count]) => self.split(text, None, Some(integer(count)?)),
            ("split", [separator, count]) => {
                self.split(text, Some(string(separator)?), Some(integer(count)?))
            }
            ("join", [values]) => {
                let values = self.sequence(values)?;
                let mut result = String::new();
                for (index, value) in values.iter().enumerate() {
                    self.step()?;
                    self.touch(value)?;
                    if index > 0 {
                        self.append(&mut result, text)?;
                    }
                    self.append(&mut result, string(value)?)?;
                }
                bounded_string(result)
            }
            _ => Err(invalid()),
        }
    }
    fn replace(&mut self, text: &str, old: &str, new: &str, count: Option<i128>) -> Result<Value> {
        let limit = match count {
            Some(count) if count >= 0 => usize::try_from(count).unwrap_or(usize::MAX),
            _ => usize::MAX,
        };
        let mut output = String::new();
        let mut previous = 0;
        for (replaced, (offset, _)) in text.match_indices(old).enumerate() {
            if replaced == limit {
                break;
            }
            self.step()?;
            self.append(&mut output, &text[previous..offset])?;
            self.append(&mut output, new)?;
            previous = offset + old.len();
        }
        self.append(&mut output, &text[previous..])?;
        bounded_string(output)
    }
    fn split(&mut self, text: &str, separator: Option<&str>, count: Option<i128>) -> Result<Value> {
        let count = count
            .filter(|v| *v >= 0)
            .and_then(|v| usize::try_from(v).ok())
            .unwrap_or(usize::MAX);
        let mut values = Vec::new();
        if let Some(separator) = separator {
            if separator.is_empty() {
                return Err(invalid());
            }
            for value in text.splitn(count.saturating_add(1), separator) {
                if values.len() >= ITEMS {
                    return Err(invalid());
                }
                self.step()?;
                self.allocate_sequence(1)?;
                values.push(self.copy_string(value)?);
            }
        } else {
            let mut remaining = text.trim_start_matches(python_space);
            while !remaining.is_empty() {
                if values.len() >= ITEMS {
                    return Err(invalid());
                }
                self.step()?;
                if values.len() == count {
                    self.allocate_sequence(1)?;
                    values.push(self.copy_string(remaining)?);
                    break;
                }
                let offset = remaining.find(python_space).unwrap_or(remaining.len());
                self.allocate_sequence(1)?;
                values.push(self.copy_string(&remaining[..offset])?);
                remaining = remaining[offset..].trim_start_matches(python_space);
            }
        }
        Ok(Value::List(values.into()))
    }
    fn allocate(&mut self, bytes: usize) -> Result<()> {
        self.allocation_left = self
            .allocation_left
            .checked_sub(bytes)
            .ok_or_else(invalid)?;
        Ok(())
    }
    fn allocate_sequence(&mut self, length: usize) -> Result<()> {
        if length > ITEMS {
            return Err(invalid());
        }
        self.allocate(
            length
                .checked_mul(std::mem::size_of::<Value>())
                .ok_or_else(invalid)?,
        )
    }
    fn append(&mut self, output: &mut String, value: &str) -> Result<()> {
        self.allocate(value.len())?;
        append(output, value)
    }
    fn copy_string(&mut self, value: &str) -> Result<Value> {
        self.allocate(value.len())?;
        Ok(Value::String(Arc::from(value)))
    }
    fn work(&mut self, bytes: usize) -> Result<()> {
        self.work_left = self.work_left.checked_sub(bytes).ok_or_else(invalid)?;
        Ok(())
    }
    fn touch(&mut self, value: &Value) -> Result<()> {
        self.work(match value {
            Value::String(v) => v.len(),
            Value::List(v) | Value::Tuple(v) => v.len(),
            Value::Map(v) => v.len(),
            _ => 1,
        })
    }
}
fn append(output: &mut String, value: &str) -> Result<()> {
    if output
        .len()
        .checked_add(value.len())
        .is_none_or(|length| length > BYTES)
    {
        return Err(invalid());
    }
    output.push_str(value);
    Ok(())
}
fn bounded_string(value: String) -> Result<Value> {
    if value.len() > BYTES {
        return Err(invalid());
    }
    Ok(Value::String(value.into()))
}
fn numeric(value: &Value) -> bool {
    matches!(value, Value::Bool(_) | Value::Int(_) | Value::Float(_))
}
fn integer(value: &Value) -> Result<i128> {
    match value {
        Value::Int(v) => Ok(*v),
        Value::Bool(v) => Ok(i128::from(*v)),
        _ => Err(invalid()),
    }
}
fn float(value: &Value) -> Result<f64> {
    match value {
        Value::Float(v) if v.is_finite() => Ok(*v),
        Value::Int(v) => Ok(*v as f64),
        Value::Bool(v) => Ok(if *v { 1.0 } else { 0.0 }),
        _ => Err(invalid()),
    }
}
fn string(value: &Value) -> Result<&str> {
    match value {
        Value::String(v) => Ok(v),
        _ => Err(invalid()),
    }
}
fn number_value(value: &Value) -> Result<Value> {
    match value {
        Value::Int(_) | Value::Float(_) => Ok(value.clone()),
        Value::Bool(v) => Ok(Value::Int(i128::from(*v))),
        _ => Err(invalid()),
    }
}
fn position(index: i128, length: usize) -> Result<usize> {
    let index = if index < 0 {
        (length as i128).checked_add(index).ok_or_else(invalid)?
    } else {
        index
    };
    usize::try_from(index)
        .ok()
        .filter(|index| *index < length)
        .ok_or_else(invalid)
}
fn trunc_int(value: f64) -> Result<i128> {
    if !value.is_finite() || value < i128::MIN as f64 || value >= -(i128::MIN as f64) {
        return Err(invalid());
    }
    Ok(value.trunc() as i128)
}
fn numeric_cmp(left: &Value, right: &Value) -> Result<Ordering> {
    match (left, right) {
        (Value::Float(a), Value::Float(b)) => a.partial_cmp(b).ok_or_else(invalid),
        (Value::Float(a), b) => Ok(int_float_cmp(integer(b)?, *a)?.reverse()),
        (a, Value::Float(b)) => int_float_cmp(integer(a)?, *b),
        (a, b) => Ok(integer(a)?.cmp(&integer(b)?)),
    }
}
fn int_float_cmp(integer: i128, float: f64) -> Result<Ordering> {
    if !float.is_finite() {
        return Err(invalid());
    }
    if float >= -(i128::MIN as f64) {
        return Ok(Ordering::Less);
    }
    if float < i128::MIN as f64 {
        return Ok(Ordering::Greater);
    }
    Ok(match integer.cmp(&(float.trunc() as i128)) {
        Ordering::Equal => {
            if float.fract() > 0.0 {
                Ordering::Less
            } else if float.fract() < 0.0 {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        }
        other => other,
    })
}
fn floor_div(a: i128, b: i128) -> Option<i128> {
    let quotient = a.checked_div(b)?;
    let remainder = a.checked_rem(b)?;
    if remainder != 0 && (remainder < 0) != (b < 0) {
        quotient.checked_sub(1)
    } else {
        Some(quotient)
    }
}
fn parse_int(text: &str, base: i128) -> Result<i128> {
    let mut text = text.trim();
    let negative = text.starts_with('-');
    if text.starts_with(['-', '+']) {
        text = &text[1..];
    }
    let mut base = u32::try_from(base).map_err(|_| invalid())?;
    if base != 0 && !(2..=36).contains(&base) {
        return Err(invalid());
    }
    let prefix = if text.starts_with("0x") || text.starts_with("0X") {
        16
    } else if text.starts_with("0o") || text.starts_with("0O") {
        8
    } else if text.starts_with("0b") || text.starts_with("0B") {
        2
    } else {
        0
    };
    let inferred = base == 0;
    if base == 0 {
        base = if prefix != 0 { prefix } else { 10 };
    }
    if prefix == base {
        text = &text[2..];
    }
    if text.is_empty() {
        return Err(invalid());
    }
    if inferred
        && prefix == 0
        && text.starts_with('0')
        && text.chars().any(|c| c != '0' && c != '_')
    {
        return Err(invalid());
    }
    let mut value = 0i128;
    let mut previous_digit = prefix == base;
    let mut any = false;
    for c in text.chars() {
        if c == '_' {
            if !previous_digit {
                return Err(invalid());
            }
            previous_digit = false;
            continue;
        }
        let digit = c.to_digit(base).ok_or_else(invalid)? as i128;
        value = value
            .checked_mul(i128::from(base))
            .and_then(|v| {
                if negative {
                    v.checked_sub(digit)
                } else {
                    v.checked_add(digit)
                }
            })
            .ok_or_else(invalid)?;
        previous_digit = true;
        any = true;
    }
    if !any || !previous_digit {
        return Err(invalid());
    }
    Ok(value)
}

fn float_divmod(a: f64, b: f64) -> (f64, f64) {
    let mut remainder = a % b;
    let mut quotient = (a - remainder) / b;
    if remainder != 0.0 {
        if remainder.is_sign_negative() != b.is_sign_negative() {
            remainder += b;
            quotient -= 1.0;
        }
    } else {
        remainder = 0.0f64.copysign(b);
    }
    let floor = if quotient != 0.0 {
        let mut floor = quotient.floor();
        if quotient - floor > 0.5 {
            floor += 1.0;
        }
        floor
    } else {
        0.0f64.copysign(a / b)
    };
    (floor, remainder)
}

pub(crate) fn float_repr(value: f64) -> String {
    let absolute = value.abs();
    if absolute != 0.0 && !(1e-4..1e16).contains(&absolute) {
        let scientific = format!("{value:e}");
        let (digits, exponent) = scientific.split_once('e').unwrap();
        let exponent = exponent.parse::<i32>().unwrap();
        format!(
            "{digits}e{}{exponent:02}",
            if exponent >= 0 { "+" } else { "-" },
            exponent = exponent.unsigned_abs()
        )
    } else {
        let mut result = value.to_string();
        if !result.contains('.') {
            result.push_str(".0");
        }
        result
    }
}
fn nonprintable(c: char) -> bool {
    c.is_control()
        || (c.is_whitespace() && c != ' ')
        || matches!(c as u32,
        0x00ad | 0x061c | 0x180e | 0x200b..=0x200f | 0x202a..=0x202e | 0x2060..=0x206f |
        0xfeff | 0xfff9..=0xfffb | 0x1bca0..=0x1bca3 | 0x1d173..=0x1d17a | 0xe0001 | 0xe0020..=0xe007f)
}

fn python_space(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\u{1c}'..='\u{1f}')
}
fn int_ratio(numerator: i128, denominator: i128) -> Result<f64> {
    if denominator == 0 {
        return Err(invalid());
    }
    let negative = (numerator < 0) != (denominator < 0);
    let mut numerator = numerator.unsigned_abs();
    let mut denominator = denominator.unsigned_abs();
    if numerator == 0 {
        return Ok(if negative { -0.0 } else { 0.0 });
    }
    let mut exponent = denominator.leading_zeros() as i32 - numerator.leading_zeros() as i32;
    if exponent >= 0 {
        if numerator < (denominator << exponent) {
            exponent -= 1;
        }
    } else if (numerator << -exponent) < denominator {
        exponent -= 1;
    }
    if exponent >= 0 {
        denominator <<= exponent;
    } else {
        numerator <<= -exponent;
    }
    let mut remainder = numerator - denominator;
    let mut mantissa = 1u64;
    for _ in 0..52 {
        mantissa <<= 1;
        let distance = denominator - remainder;
        if remainder >= distance {
            remainder -= distance;
            mantissa |= 1;
        } else {
            remainder *= 2;
        }
    }
    match remainder.cmp(&(denominator - remainder)) {
        Ordering::Greater => mantissa += 1,
        Ordering::Equal if !mantissa.is_multiple_of(2) => mantissa += 1,
        _ => {}
    }
    let result = (mantissa as f64) * 2f64.powi(exponent - 52);
    Ok(if negative { -result } else { result })
}
fn round_negative(value: f64, places: usize) -> Result<f64> {
    let absolute = value.abs();
    if absolute < 5.0 {
        return Ok(0.0f64.copysign(value));
    }
    if absolute < 4503599627370496.0 {
        if places > 16 {
            return Ok(0.0f64.copysign(value));
        }
        let bits = absolute.to_bits();
        let significand = u128::from((bits & ((1u64 << 52) - 1)) | (1u64 << 52));
        let binary_exponent = ((bits >> 52) & 2047) as i32 - 1023 - 52;
        let factor = 10u128.pow(places as u32);
        let divisor = (1u128 << -binary_exponent) * factor;
        let mut quotient = significand / divisor;
        let remainder = significand % divisor;
        if remainder > divisor - remainder
            || (remainder == divisor - remainder && !quotient.is_multiple_of(2))
        {
            quotient += 1;
        }
        return Ok(((quotient * factor) as f64).copysign(value));
    }
    // Every finite float this large is an exact integer. Formatting that integer
    // first lets decimal rounding avoid reciprocal/multiplication ULP drift.
    let whole = format!("{absolute:.0}");
    if places > whole.len() {
        return Ok(0.0f64.copysign(value));
    }
    let keep = whole.len() - places;
    let tail = &whole.as_bytes()[keep..];
    let mut prefix = whole.as_bytes()[..keep].to_vec();
    let round_up = tail[0] > b'5'
        || (tail[0] == b'5'
            && (tail[1..].iter().any(|b| *b != b'0')
                || prefix.last().is_some_and(|b| !(b - b'0').is_multiple_of(2))));
    if round_up {
        let mut index = prefix.len();
        loop {
            if index == 0 {
                prefix.insert(0, b'1');
                break;
            }
            index -= 1;
            if prefix[index] < b'9' {
                prefix[index] += 1;
                break;
            }
            prefix[index] = b'0';
        }
    }
    if prefix.is_empty() {
        return Ok(0.0f64.copysign(value));
    }
    prefix.resize(prefix.len() + places, b'0');
    let result = std::str::from_utf8(&prefix)
        .map_err(|_| invalid())?
        .parse::<f64>()
        .map_err(|_| invalid())?;
    if result.is_finite() {
        Ok(result.copysign(value))
    } else {
        Err(invalid())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn run(source: &str) -> Value {
        let expression = super::super::parser::parse(source).unwrap();
        evaluate(&expression, &BTreeMap::new()).unwrap()
    }
    #[test]
    fn short_circuit_and_chained_comparison_are_lazy_and_return_operands() {
        assert_eq!(run("False and missing"), Value::Bool(false));
        assert_eq!(run("'kept' or missing"), Value::String(Arc::from("kept")));
        assert_eq!(run("1 > 2 < missing"), Value::Bool(false));
        assert_eq!(
            run("'yes' if True else missing"),
            Value::String(Arc::from("yes"))
        );
        assert_eq!(run("1 < 2 <= 2.0"), Value::Bool(true));
    }
    #[test]
    fn numeric_comparison_preserves_integer_precision_and_python_division() {
        for source in [
            "9007199254740993 != 9007199254740992.0",
            "True == 1",
            "-7 // 3 == -3",
            "7 // -3 == -3",
            "-7 % 3 == 2",
            "7 % -3 == -2",
            "1.0 // 0.1 == 9.0",
            "1 ** 999999999999999999999999 == 1",
        ] {
            assert_eq!(run(source), Value::Bool(true), "{source}");
        }
        assert_eq!(run("round(2.675, 2)"), Value::Float(2.67));
        assert_eq!(run("round(2.5)"), Value::Int(2));
        assert_eq!(run("round(3.5)"), Value::Int(4));
        assert_eq!(run("round(125, -1)"), Value::Int(120));
    }
    #[test]
    fn unicode_indices_methods_and_helpers_preserve_meaning() {
        for source in [
            "len('a🍂ø') == 3",
            "'a🍂ø'[-2] == '🍂'",
            "'ß'.upper() == 'SS'",
            "'ÆØÅ'.lower() == 'æøå'",
            "'  abc  '.strip() == 'abc'",
            "'ab'.replace('', '-') == '-a-b-'",
            "contains('one two', ['missing', 'two'])",
            "not contains('one two', 'ne')",
            "contains('one two', 'ne', '')",
            "contains('one\\ttwo', 'two', None)",
            "{'a': 1}.get('missing', 3) == 3",
            "'a,b,'.split(',') == ['a', 'b', '']",
            "'-'.join(['a', 'b']) == 'a-b'",
            "'a🍂ø'.find('ø') == 2",
        ] {
            assert_eq!(run(source), Value::Bool(true), "{source}");
        }
    }
    #[test]
    fn tuple_values_keep_their_type_through_sequence_operations() {
        for source in [
            "(1, 2) != [1, 2]",
            "(1,) + (2,) == (1, 2)",
            "(1,) * 2 == (1, 1)",
            "(1, 2)[-1] == 2",
            "str((1,)) == '(1,)'",
            "1 in (1, 2)",
            "'abc'.startswith(('x', 'a'))",
        ] {
            assert_eq!(run(source), Value::Bool(true), "{source}");
        }
    }
    #[test]
    fn dictionary_order_and_representations_follow_python() {
        for source in [
            "str({'b': 1, 'a': 2}) == \"{'b': 1, 'a': 2}\"",
            "str({'b': 1, 'a': 2, 'b': 3}) == \"{'b': 3, 'a': 2}\"",
            "{'b': 1, 'a': 2} == {'a': 2, 'b': 1}",
            "''.join({'b': 1, 'a': 2}) == 'ba'",
            "str(1e20) == '1e+20'",
            "str(1e-7) == '1e-07'",
            "str(-0.0) == '-0.0'",
            "str(1e-4) == '0.0001'",
            "str(1e15) == '1000000000000000.0'",
            "round(1.5, None) == 2",
            "float('1_000.5') == 1000.5",
            "'\\x1cabc\\x1f'.strip() == 'abc'",
            "'a\\x1cb'.split() == ['a','b']",
            "round(1e-310, 309) == 0.0",
            "round(5e-324, 309) == 0.0",
            "round(-6.118090425525051e19,-6) == -6.1180904255251e19",
            "round(6.127673492813808e19,-5) == 6.12767349281381e19",
            "round(3.6370517767371194e19,-15) == 3.6371e19",
            "566186495318961628974 / 405290175416300189998 == 1.3969904272596647",
            "int('-170141183460469231731687303715884105728') % -1 == 0",
        ] {
            assert_eq!(run(source), Value::Bool(true), "{source}");
        }
        let expression = Expr::Call(
            "str".into(),
            vec![Expr::List(vec![Expr::Literal(Value::String(Arc::from(
                "a'b",
            )))])],
        );
        assert_eq!(
            evaluate(&expression, &BTreeMap::new()).unwrap(),
            Value::String(Arc::from("[\"a'b\"]"))
        );
    }
    #[test]
    fn derived_sequences_share_a_cumulative_allocation_budget() {
        let child = || {
            Expr::Binary(
                Box::new(Expr::List(vec![Expr::Literal(Value::Int(1))])),
                BinaryOp::Mul,
                Box::new(Expr::Literal(Value::Int(4096))),
            )
        };
        let expression = Expr::List((0..64).map(|_| child()).collect());
        assert!(evaluate(&expression, &BTreeMap::new()).is_err());
        let valid = Expr::List(vec![child(), child()]);
        assert!(evaluate(&valid, &BTreeMap::new()).is_ok());
    }
    #[test]
    fn resource_limits_and_missing_values_fail_privately() {
        for source in [
            "'x' * 100000000000000000000000",
            "[1] * 4097",
            "2 ** 100000000000000000000",
            "int('PRIVATE')",
            "missing",
            "1/0",
            "abs(-170141183460469231731687303715884105728)",
        ] {
            let result = super::super::parser::parse(source)
                .and_then(|expr| evaluate(&expr, &BTreeMap::new()));
            let error = result.unwrap_err().to_string();
            assert!(!error.contains("PRIVATE"));
        }
        let context = BTreeMap::from([("big".into(), Value::String(Arc::from("x".repeat(BYTES))))]);
        let expression = Expr::List(
            (0..32)
                .map(|_| Expr::Call("len".into(), vec![Expr::Name("big".into())]))
                .collect(),
        );
        assert!(
            evaluate(&expression, &context).is_err(),
            "repeated scans share a work budget"
        );
        let huge = Expr::Binary(
            Box::new(Expr::Literal(Value::String(Arc::from("")))),
            BinaryOp::Mul,
            Box::new(Expr::Literal(Value::Int(i128::MAX))),
        );
        assert_eq!(
            evaluate(&huge, &context).unwrap(),
            Value::String(Arc::from(""))
        );
    }
}
