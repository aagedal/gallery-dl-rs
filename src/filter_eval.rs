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
        Value::SourceNullDate(_) => false,
        Value::SourcePrimitive(value) => truthy(value),
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
        locals: Vec::new(),
    }
    .eval(expr)
}
struct Evaluator<'a> {
    left: usize,
    work_left: usize,
    allocation_left: usize,
    depth: usize,
    context: &'a BTreeMap<String, Value>,
    /// Comprehension variables, innermost last.
    locals: Vec<(String, Value)>,
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
            Expr::Name(name) => {
                let value = match self.locals.iter().rev().find(|(local, _)| local == name) {
                    Some((_, value)) => value.clone(),
                    None => self.context.get(name).cloned().ok_or_else(invalid)?,
                };
                if let Value::SourcePrimitive(value) = value {
                    (*value).clone()
                } else {
                    value
                }
            }
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
            Expr::Slice(value, bounds) => {
                let value = self.eval(value)?;
                let mut evaluated = [None, None, None];
                for (slot, bound) in evaluated.iter_mut().zip(bounds.iter()) {
                    *slot = match self.eval(bound)? {
                        Value::None => None,
                        bound => Some(i64::try_from(integer(&bound)?).map_err(|_| invalid())?),
                    };
                }
                self.slice(&value, evaluated)?
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
                let mut positional = Vec::new();
                let mut keywords = Vec::new();
                for argument in arguments {
                    match argument {
                        Expr::Keyword(key, value) => {
                            keywords.push((key.as_str(), self.eval(value)?))
                        }
                        value if keywords.is_empty() => positional.push(self.eval(value)?),
                        _ => return Err(invalid()),
                    }
                }
                if keywords.is_empty() {
                    self.call(name, &positional)?
                } else {
                    self.call_keywords(name, &positional, &keywords)?
                }
            }
            // Keyword arguments only appear inside calls.
            Expr::Keyword(..) => return Err(invalid()),
            Expr::Comprehension {
                key,
                element,
                targets,
                iterable,
                conditions,
            } => {
                let items = self.eval(iterable)?;
                let items = self.sequence(&items)?;
                let mut values = Vec::new();
                let mut map = super::OrderedMap::default();
                for item in items {
                    self.step()?;
                    let bound = targets.len();
                    if bound == 1 {
                        self.locals.push((targets[0].clone(), item));
                    } else {
                        let parts = match item {
                            Value::List(parts) | Value::Tuple(parts) if parts.len() == bound => {
                                parts
                            }
                            _ => return Err(invalid()),
                        };
                        for (target, part) in targets.iter().zip(parts.iter()) {
                            self.locals.push((target.clone(), part.clone()));
                        }
                    }
                    let result = (|| -> Result<()> {
                        for condition in conditions {
                            if !truthy(&self.eval(condition)?) {
                                return Ok(());
                            }
                        }
                        if values.len() >= ITEMS {
                            return Err(invalid());
                        }
                        self.allocate_sequence(1)?;
                        match key {
                            Some(key) => {
                                let key = self.eval(key)?;
                                let value = self.eval(element)?;
                                map.insert(string(&key)?.to_owned(), value);
                            }
                            None => values.push(self.eval(element)?),
                        }
                        Ok(())
                    })();
                    self.locals.truncate(self.locals.len() - bound);
                    result?;
                }
                if key.is_some() {
                    Value::Map(Arc::new(map))
                } else {
                    Value::List(values.into())
                }
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
    /// Python slicing of strings, lists and tuples.
    fn slice(&mut self, value: &Value, [start, stop, step]: [Option<i64>; 3]) -> Result<Value> {
        let step = step.unwrap_or(1);
        if step == 0 {
            return Err(invalid());
        }
        let indices = |len: usize| -> Vec<usize> {
            let len = len as i64;
            let clamp = |bound: i64, low: i64, high: i64| {
                let bound = if bound < 0 { bound + len } else { bound };
                bound.clamp(low, high)
            };
            let (start, stop) = if step > 0 {
                (
                    start.map_or(0, |b| clamp(b, 0, len)),
                    stop.map_or(len, |b| clamp(b, 0, len)),
                )
            } else {
                (
                    start.map_or(len - 1, |b| clamp(b, -1, len - 1)),
                    stop.map_or(-1, |b| clamp(b, -1, len - 1)),
                )
            };
            let mut out = Vec::new();
            let mut i = start;
            while (step > 0 && i < stop) || (step < 0 && i > stop) {
                out.push(i as usize);
                i += step;
            }
            out
        };
        match value {
            Value::String(text) => {
                let chars: Vec<char> = text.chars().collect();
                let sliced: String = indices(chars.len()).into_iter().map(|i| chars[i]).collect();
                self.allocate(sliced.len())?;
                Ok(Value::String(sliced.into()))
            }
            Value::List(values) | Value::Tuple(values) => {
                let sliced: Vec<Value> = indices(values.len())
                    .into_iter()
                    .map(|i| values[i].clone())
                    .collect();
                Ok(if matches!(value, Value::List(_)) {
                    Value::List(sliced.into())
                } else {
                    Value::Tuple(sliced.into())
                })
            }
            _ => Err(invalid()),
        }
    }
    fn index(&mut self, value: &Value, index: &Value) -> Result<Value> {
        match value {
            Value::Map(values) => values
                .get(string(index)?)
                .map(|value| {
                    if let Value::SourcePrimitive(value) = value {
                        (**value).clone()
                    } else {
                        value.clone()
                    }
                })
                .ok_or_else(invalid),
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
            (Value::SourceNullDate(a), Value::SourceNullDate(b)) => a == b,
            (Value::SourceDate(a), Value::SourceNullDate(b))
            | (Value::SourceNullDate(b), Value::SourceDate(a)) => {
                a.unwrap_or(-62_135_596_800) == *b
            }
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
            (Value::SourceNullDate(a), Value::SourceNullDate(b)) => Ok(a.cmp(b)),
            (Value::SourceDate(a), Value::SourceNullDate(b)) => {
                Ok(a.unwrap_or(-62_135_596_800).cmp(b))
            }
            (Value::SourceNullDate(a), Value::SourceDate(b)) => {
                Ok(a.cmp(&b.unwrap_or(-62_135_596_800)))
            }
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
        // Datetimes are seconds; `timedelta` values are seconds as well.
        match (left, op, right) {
            (Value::SourceDate(Some(date)), BinaryOp::Add | BinaryOp::Sub, delta)
                if numeric(delta) =>
            {
                let delta = float(delta)?;
                let delta = if matches!(op, BinaryOp::Sub) {
                    -delta
                } else {
                    delta
                };
                return Ok(Value::SourceDate(Some(*date + delta.floor() as i64)));
            }
            (delta, BinaryOp::Add, Value::SourceDate(Some(date))) if numeric(delta) => {
                return Ok(Value::SourceDate(Some(
                    *date + float(delta)?.floor() as i64,
                )));
            }
            (Value::SourceDate(Some(a)), BinaryOp::Sub, Value::SourceDate(Some(b))) => {
                return Ok(Value::Float((*a - *b) as f64));
            }
            _ => {}
        }
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
        if let (BinaryOp::Mod, Value::String(format)) = (op, left) {
            return self.printf(format, right);
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
            ("sum", [values]) | ("sum", [values, _]) => {
                let mut total = args.get(1).cloned().unwrap_or(Value::Int(0));
                for value in self.sequence(values)? {
                    self.step()?;
                    total = self.binary(&total, BinaryOp::Add, &value)?;
                }
                Ok(total)
            }
            ("sorted", [values]) => self.sorted(values, false),
            ("reversed", [values]) => {
                let mut values = self.sequence(values)?;
                values.reverse();
                Ok(Value::List(values.into()))
            }
            ("list", []) => Ok(Value::List(Arc::from([]))),
            ("tuple", []) => Ok(Value::Tuple(Arc::from([]))),
            ("list", [values]) => Ok(Value::List(self.sequence(values)?.into())),
            ("tuple", [values]) => Ok(Value::Tuple(self.sequence(values)?.into())),
            ("range", bounds) if (1..=3).contains(&bounds.len()) => {
                let numbers = bounds.iter().map(integer).collect::<Result<Vec<_>>>()?;
                let (start, stop, step) = match numbers[..] {
                    [stop] => (0, stop, 1),
                    [start, stop] => (start, stop, 1),
                    [start, stop, step] => (start, stop, step),
                    _ => unreachable!(),
                };
                if step == 0 {
                    return Err(invalid());
                }
                let mut values = Vec::new();
                let mut current = start;
                while (step > 0 && current < stop) || (step < 0 && current > stop) {
                    if values.len() >= ITEMS {
                        return Err(invalid());
                    }
                    self.step()?;
                    values.push(Value::Int(current));
                    current = current.checked_add(step).ok_or_else(invalid)?;
                }
                self.allocate_sequence(values.len())?;
                Ok(Value::List(values.into()))
            }
            ("enumerate", [values]) | ("enumerate", [values, _]) => {
                let start = args.get(1).map(integer).transpose()?.unwrap_or(0);
                let values = self.sequence(values)?;
                self.allocate_sequence(values.len() * 3)?;
                Ok(Value::List(
                    values
                        .into_iter()
                        .enumerate()
                        .map(|(index, value)| {
                            Value::Tuple(Arc::from([Value::Int(start + index as i128), value]))
                        })
                        .collect(),
                ))
            }
            ("zip", sequences) if !sequences.is_empty() => {
                let sequences = sequences
                    .iter()
                    .map(|values| self.sequence(values))
                    .collect::<Result<Vec<_>>>()?;
                let length = sequences.iter().map(Vec::len).min().unwrap_or(0);
                self.allocate_sequence(length * (sequences.len() + 1))?;
                Ok(Value::List(
                    (0..length)
                        .map(|index| {
                            Value::Tuple(sequences.iter().map(|s| s[index].clone()).collect())
                        })
                        .collect(),
                ))
            }
            ("divmod", [a, b]) => Ok(Value::Tuple(Arc::from([
                self.binary(a, BinaryOp::FloorDiv, b)?,
                self.binary(a, BinaryOp::Mod, b)?,
            ]))),
            ("pow", [a, b]) => self.binary(a, BinaryOp::Pow, b),
            ("hex" | "oct" | "bin", [value]) => {
                let value = integer(value)?;
                let magnitude = value.unsigned_abs();
                let digits = match name {
                    "hex" => format!("0x{magnitude:x}"),
                    "oct" => format!("0o{magnitude:o}"),
                    _ => format!("0b{magnitude:b}"),
                };
                self.copy_string(&format!("{}{digits}", if value < 0 { "-" } else { "" }))
            }
            ("chr", [value]) => u32::try_from(integer(value)?)
                .ok()
                .and_then(char::from_u32)
                .ok_or_else(invalid)
                .and_then(|c| self.copy_string(&c.to_string())),
            ("ord", [value]) => {
                let mut chars = string(value)?.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) => Ok(Value::Int(u32::from(c).into())),
                    _ => Err(invalid()),
                }
            }
            ("print", values) => {
                let mut line = Vec::new();
                for value in values {
                    line.push(match value {
                        Value::String(text) => text.to_string(),
                        value => self.display(value, false, 0)?,
                    });
                }
                println!("{}", line.join(" "));
                Ok(Value::None)
            }
            ("parse_int", [value]) | ("parse_int", [value, _]) => {
                let default = args.get(1).cloned().unwrap_or(Value::Int(0));
                Ok(match value {
                    Value::Int(_) => value.clone(),
                    Value::String(text) => text.trim().parse::<i128>().map_or(default, Value::Int),
                    _ => default,
                })
            }
            ("datetime", parts) if (3..=7).contains(&parts.len()) => {
                let parts = parts.iter().map(integer).collect::<Result<Vec<_>>>()?;
                let part = |index: usize| parts.get(index).copied().unwrap_or(0);
                let date = time::Date::from_calendar_date(
                    i32::try_from(part(0)).map_err(|_| invalid())?,
                    time::Month::try_from(u8::try_from(part(1)).map_err(|_| invalid())?)
                        .map_err(|_| invalid())?,
                    u8::try_from(part(2)).map_err(|_| invalid())?,
                )
                .map_err(|_| invalid())?;
                let time = time::Time::from_hms(
                    u8::try_from(part(3)).map_err(|_| invalid())?,
                    u8::try_from(part(4)).map_err(|_| invalid())?,
                    u8::try_from(part(5)).map_err(|_| invalid())?,
                )
                .map_err(|_| invalid())?;
                Ok(Value::SourceDate(Some(
                    time::PrimitiveDateTime::new(date, time)
                        .assume_utc()
                        .unix_timestamp(),
                )))
            }
            ("datetime.now" | "datetime.utcnow", []) => Ok(Value::SourceDate(Some(
                time::OffsetDateTime::now_utc().unix_timestamp(),
            ))),
            ("datetime.fromtimestamp", [value]) => {
                Ok(Value::SourceDate(Some(float(value)?.floor() as i64)))
            }
            ("timedelta", parts) if parts.len() <= 3 => {
                let mut seconds = 0.0;
                for (part, scale) in parts.iter().zip([86400.0, 1.0, 1e-6]) {
                    seconds += float(part)? * scale;
                }
                Ok(Value::Float(seconds))
            }
            (
                "re.search" | "re.match" | "re.fullmatch" | "re.findall" | "re.split",
                [pattern, text],
            ) => self.regex(name, string(pattern)?, string(text)?, None),
            ("re.sub", [pattern, replacement, text]) => self.regex(
                name,
                string(pattern)?,
                string(text)?,
                Some(string(replacement)?),
            ),
            _ => Err(invalid()),
        }
    }
    fn sorted(&mut self, values: &Value, reverse: bool) -> Result<Value> {
        let mut values = self.sequence(values)?;
        let mut failure = None;
        values.sort_by(|a, b| match self.order(a, b) {
            Ok(order) => order,
            Err(error) => {
                failure.get_or_insert(error);
                Ordering::Equal
            }
        });
        if let Some(error) = failure {
            return Err(error);
        }
        if reverse {
            values.reverse();
        }
        Ok(Value::List(values.into()))
    }
    /// Calls with keyword arguments: `sorted(reverse=)`, `timedelta(...)`,
    /// `round(ndigits=)`.
    fn call_keywords(
        &mut self,
        name: &str,
        args: &[Value],
        keywords: &[(&str, Value)],
    ) -> Result<Value> {
        match (name, args) {
            ("sorted", [values]) => match keywords {
                [("reverse", reverse)] => self.sorted(values, truthy(reverse)),
                _ => Err(invalid()),
            },
            ("round", [value]) => match keywords {
                [("ndigits", Value::None)] => self.round(value, None),
                [("ndigits", digits)] => self.round(value, Some(integer(digits)?)),
                _ => Err(invalid()),
            },
            ("timedelta", positional) if positional.len() <= 3 => {
                let mut seconds = match self.call("timedelta", positional)? {
                    Value::Float(seconds) => seconds,
                    _ => unreachable!(),
                };
                for (key, value) in keywords {
                    seconds += float(value)?
                        * match *key {
                            "weeks" => 604800.0,
                            "days" => 86400.0,
                            "hours" => 3600.0,
                            "minutes" => 60.0,
                            "seconds" => 1.0,
                            "milliseconds" => 1e-3,
                            "microseconds" => 1e-6,
                            _ => return Err(invalid()),
                        };
                }
                Ok(Value::Float(seconds))
            }
            _ => Err(invalid()),
        }
    }
    /// Upstream's `re` functions; a match is the tuple of its groups (truthy,
    /// and `m[N]` selects group N like a `re.Match`).
    fn regex(
        &mut self,
        function: &str,
        pattern: &str,
        text: &str,
        replacement: Option<&str>,
    ) -> Result<Value> {
        self.work(pattern.len().saturating_add(text.len()))?;
        let regex = regex::RegexBuilder::new(pattern)
            .size_limit(1 << 20)
            .build()
            .map_err(|_| invalid())?;
        let groups = |captures: regex::Captures<'_>| -> Vec<Value> {
            captures
                .iter()
                .map(|group| {
                    group.map_or(Value::None, |group| {
                        Value::String(Arc::from(group.as_str()))
                    })
                })
                .collect()
        };
        match function {
            "re.search" | "re.match" | "re.fullmatch" => {
                let found = regex.captures(text).filter(|captures| {
                    let whole = captures.get(0).expect("group 0");
                    match function {
                        "re.match" => whole.start() == 0,
                        "re.fullmatch" => whole.start() == 0 && whole.end() == text.len(),
                        _ => true,
                    }
                });
                // `match`/`fullmatch` anchor at the start: retry anchored when
                // the leftmost match starts later.
                let found = match (found, function) {
                    (None, "re.match" | "re.fullmatch") => {
                        let anchored = if function == "re.match" {
                            format!("^(?:{pattern})")
                        } else {
                            format!("^(?:{pattern})$")
                        };
                        regex::RegexBuilder::new(&anchored)
                            .size_limit(1 << 20)
                            .build()
                            .map_err(|_| invalid())?
                            .captures(text)
                            .map(groups)
                    }
                    (found, _) => found.map(groups),
                };
                Ok(found.map_or(Value::None, |groups| {
                    self.allocate_sequence(groups.len()).ok();
                    Value::Tuple(groups.into())
                }))
            }
            "re.findall" => {
                let mut values = Vec::new();
                for captures in regex.captures_iter(text) {
                    if values.len() >= ITEMS {
                        return Err(invalid());
                    }
                    self.step()?;
                    let mut groups = groups(captures);
                    values.push(match groups.len() {
                        1 => groups.remove(0),
                        2 => groups.remove(1),
                        _ => Value::Tuple(groups[1..].iter().cloned().collect()),
                    });
                }
                self.allocate_sequence(values.len())?;
                Ok(Value::List(values.into()))
            }
            "re.split" => {
                let mut values = Vec::new();
                for part in regex.split(text) {
                    if values.len() >= ITEMS {
                        return Err(invalid());
                    }
                    values.push(self.copy_string(part)?);
                }
                Ok(Value::List(values.into()))
            }
            _ => {
                // Python `\1`/`\g<name>` references become `${1}`/`${name}`.
                let replacement = replacement.unwrap_or_default();
                let mut converted = String::new();
                let mut chars = replacement.chars().peekable();
                while let Some(c) = chars.next() {
                    match c {
                        '$' => converted.push_str("$$"),
                        '\\' => match chars.peek().copied() {
                            Some(d) if d.is_ascii_digit() => {
                                let mut number = String::new();
                                while let Some(d) =
                                    chars.peek().copied().filter(char::is_ascii_digit)
                                {
                                    number.push(d);
                                    chars.next();
                                }
                                converted.push_str(&format!("${{{number}}}"));
                            }
                            Some('g') => {
                                chars.next();
                                if chars.next() != Some('<') {
                                    return Err(invalid());
                                }
                                let name: String =
                                    chars.by_ref().take_while(|c| *c != '>').collect();
                                converted.push_str(&format!("${{{name}}}"));
                            }
                            Some('n') => {
                                chars.next();
                                converted.push('\n');
                            }
                            Some('t') => {
                                chars.next();
                                converted.push('\t');
                            }
                            Some('\\') => {
                                chars.next();
                                converted.push('\\');
                            }
                            _ => converted.push('\\'),
                        },
                        c => converted.push(c),
                    }
                }
                let output = regex.replace_all(text, converted.as_str()).into_owned();
                self.allocate(output.len())?;
                bounded_string(output)
            }
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
    /// Python's printf-style `format % values`: `%s %r %d %i %f %F %e %E %g
    /// %G %x %X %o %c %%` with flags, width, precision and `%(key)s`.
    fn printf(&mut self, format: &str, values: &Value) -> Result<Value> {
        let positional: Vec<Value> = match values {
            Value::Tuple(items) => items.to_vec(),
            Value::Map(_) => Vec::new(),
            value => vec![value.clone()],
        };
        let mut next = positional.into_iter();
        let mut output = String::new();
        let mut chars = format.chars().peekable();
        while let Some(c) = chars.next() {
            self.step()?;
            if c != '%' {
                output.push(c);
                continue;
            }
            let mut key = None;
            if chars.peek() == Some(&'(') {
                chars.next();
                let name: String = chars.by_ref().take_while(|c| *c != ')').collect();
                key = Some(name);
            }
            let mut flags = String::new();
            while let Some(flag) = chars.peek().copied().filter(|c| "-+ #0".contains(*c)) {
                flags.push(flag);
                chars.next();
            }
            let digits = |chars: &mut std::iter::Peekable<std::str::Chars<'_>>| {
                let mut text = String::new();
                while let Some(d) = chars.peek().copied().filter(char::is_ascii_digit) {
                    text.push(d);
                    chars.next();
                }
                text.parse::<usize>().ok()
            };
            let width = digits(&mut chars);
            let precision = if chars.peek() == Some(&'.') {
                chars.next();
                Some(digits(&mut chars).unwrap_or(0))
            } else {
                None
            };
            let kind = chars.next().ok_or_else(invalid)?;
            if kind == '%' {
                output.push('%');
                continue;
            }
            let value = match &key {
                Some(key) => match values {
                    Value::Map(map) => map.get(key.as_str()).cloned().ok_or_else(invalid)?,
                    _ => return Err(invalid()),
                },
                None => next.next().ok_or_else(invalid)?,
            };
            let (sign, body) = match kind {
                's' | 'r' | 'a' => {
                    let text = match (&value, kind) {
                        (Value::String(text), 's') => text.to_string(),
                        (value, 's') => self.display(value, false, 0)?,
                        (value, _) => self.display(value, true, 0)?,
                    };
                    let text = match precision {
                        Some(precision) => text.chars().take(precision).collect(),
                        None => text,
                    };
                    (String::new(), text)
                }
                'c' => (
                    String::new(),
                    match &value {
                        Value::String(text) if text.chars().count() == 1 => text.to_string(),
                        value => {
                            char::from_u32(u32::try_from(integer(value)?).map_err(|_| invalid())?)
                                .ok_or_else(invalid)?
                                .to_string()
                        }
                    },
                ),
                'd' | 'i' | 'u' | 'x' | 'X' | 'o' => {
                    let number = match number_value(&value)? {
                        Value::Int(number) => number,
                        Value::Float(number) => trunc_int(number)?,
                        _ => unreachable!(),
                    };
                    let magnitude = number.unsigned_abs();
                    let mut digits = match kind {
                        'x' => format!("{magnitude:x}"),
                        'X' => format!("{magnitude:X}"),
                        'o' => format!("{magnitude:o}"),
                        _ => magnitude.to_string(),
                    };
                    if flags.contains('#') {
                        digits = match kind {
                            'x' => format!("0x{digits}"),
                            'X' => format!("0X{digits}"),
                            'o' => format!("0o{digits}"),
                            _ => digits,
                        };
                    }
                    if let Some(precision) = precision
                        && digits.len() < precision
                    {
                        digits = format!("{}{digits}", "0".repeat(precision - digits.len()));
                    }
                    let sign = if number < 0 {
                        "-"
                    } else if flags.contains('+') {
                        "+"
                    } else if flags.contains(' ') {
                        " "
                    } else {
                        ""
                    };
                    (sign.to_owned(), digits)
                }
                'f' | 'F' | 'e' | 'E' | 'g' | 'G' => {
                    let number = float(&value)?;
                    let precision = precision.unwrap_or(6);
                    let magnitude = number.abs();
                    let body = match kind {
                        'f' | 'F' => format!("{magnitude:.precision$}"),
                        'e' | 'E' => {
                            let text = format!("{magnitude:.precision$e}");
                            let (mantissa, exponent) = text.split_once('e').unwrap_or((&text, "0"));
                            let exponent: i32 = exponent.parse().unwrap_or(0);
                            let text = format!(
                                "{mantissa}e{}{:02}",
                                if exponent < 0 { '-' } else { '+' },
                                exponent.unsigned_abs()
                            );
                            if kind == 'E' {
                                text.to_uppercase()
                            } else {
                                text
                            }
                        }
                        _ => {
                            // `%g` via the same rules as `format(x, "g")`.
                            let text = crate::naming::format::general_float(magnitude, precision);
                            if kind == 'G' {
                                text.to_uppercase()
                            } else {
                                text
                            }
                        }
                    };
                    let sign = if number.is_sign_negative() && number != 0.0 {
                        "-"
                    } else if flags.contains('+') {
                        "+"
                    } else if flags.contains(' ') {
                        " "
                    } else {
                        ""
                    };
                    (sign.to_owned(), body)
                }
                _ => return Err(invalid()),
            };
            let length = sign.chars().count() + body.chars().count();
            let pad = width.unwrap_or(0).saturating_sub(length);
            if pad > BYTES {
                return Err(invalid());
            }
            let numeric = !matches!(kind, 's' | 'r' | 'a' | 'c');
            if flags.contains('-') {
                output.push_str(&sign);
                output.push_str(&body);
                output.push_str(&" ".repeat(pad));
            } else if flags.contains('0') && numeric {
                output.push_str(&sign);
                output.push_str(&"0".repeat(pad));
                output.push_str(&body);
            } else {
                output.push_str(&" ".repeat(pad));
                output.push_str(&sign);
                output.push_str(&body);
            }
            if output.len() > BYTES {
                return Err(invalid());
            }
        }
        // Like Python, unused positional arguments are an error.
        if next.next().is_some() {
            return Err(invalid());
        }
        self.allocate(output.len())?;
        bounded_string(output)
    }

    fn display(&mut self, value: &Value, nested: bool, depth: usize) -> Result<String> {
        self.step()?;
        if depth >= DEPTH {
            return Err(invalid());
        }
        let result = match value {
            Value::None => "None".into(),
            Value::SourceDate(value) => super::source_date_text(*value),
            Value::SourceNullDate(_) => super::source_date_text(None),
            Value::SourcePrimitive(value) => self.display(value, nested, depth + 1)?,
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
        match (value, name, args) {
            (Value::SourceDate(Some(date)), "timestamp", []) => {
                return Ok(Value::Float(*date as f64));
            }
            // `timedelta` values are seconds.
            (Value::Float(_) | Value::Int(_), "total_seconds", []) => {
                return Ok(Value::Float(float(value)?));
            }
            _ => {}
        }
        if let Value::Map(values) = value {
            return match (name, args) {
                ("keys", []) => {
                    self.allocate_sequence(values.len())?;
                    Ok(Value::List(
                        values
                            .keys()
                            .map(|key| Value::String(Arc::from(key.as_str())))
                            .collect(),
                    ))
                }
                ("values", []) => {
                    self.allocate_sequence(values.len())?;
                    Ok(Value::List(values.values().cloned().collect()))
                }
                ("items", []) => {
                    self.allocate_sequence(values.len() * 3)?;
                    Ok(Value::List(
                        values
                            .iter()
                            .map(|(key, value)| {
                                Value::Tuple(Arc::from([
                                    Value::String(Arc::from(key.as_str())),
                                    value.clone(),
                                ]))
                            })
                            .collect(),
                    ))
                }
                ("get", [key]) => Ok(values.get(string(key)?).cloned().unwrap_or(Value::None)),
                ("get", [key, default]) => Ok(values
                    .get(string(key)?)
                    .cloned()
                    .unwrap_or_else(|| default.clone())),
                _ => Err(invalid()),
            };
        }
        if let Value::List(values) | Value::Tuple(values) = value {
            if let ("index", [needle]) = (name, args) {
                for (index, value) in values.iter().enumerate() {
                    if self.equal(value, needle)? {
                        return Ok(Value::Int(index as i128));
                    }
                }
                return Err(invalid());
            }
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
        let predicate = |test: fn(char) -> bool, cased: bool| {
            Value::Bool(
                !text.is_empty()
                    && text
                        .chars()
                        .all(|c| test(c) || (cased && !c.is_alphabetic()))
                    && (!cased || text.chars().any(char::is_alphabetic)),
            )
        };
        let width = |value: &Value| -> Result<usize> {
            usize::try_from(integer(value)?.max(0))
                .ok()
                .filter(|width| *width <= BYTES)
                .ok_or_else(invalid)
        };
        let fill = |value: Option<&Value>| -> Result<char> {
            match value {
                None => Ok(' '),
                Some(value) => {
                    let mut chars = string(value)?.chars();
                    match (chars.next(), chars.next()) {
                        (Some(c), None) => Ok(c),
                        _ => Err(invalid()),
                    }
                }
            }
        };
        match (name, args) {
            ("isdigit" | "isdecimal" | "isnumeric", []) => {
                return Ok(predicate(|c| c.is_numeric(), false));
            }
            ("isalpha", []) => return Ok(predicate(char::is_alphabetic, false)),
            ("isalnum", []) => return Ok(predicate(char::is_alphanumeric, false)),
            ("isspace", []) => return Ok(predicate(python_space, false)),
            ("islower", []) => return Ok(predicate(|c| !c.is_uppercase(), true)),
            ("isupper", []) => return Ok(predicate(|c| !c.is_lowercase(), true)),
            ("title" | "capitalize" | "swapcase", []) => {
                if text.len() > BYTES / 3 {
                    return Err(invalid());
                }
                self.allocate(text.len() * 3)?;
                let mut output = String::new();
                let mut previous_cased = false;
                for (index, c) in text.chars().enumerate() {
                    match name {
                        "title" if previous_cased => output.extend(c.to_lowercase()),
                        "title" => output.extend(c.to_uppercase()),
                        "capitalize" if index == 0 => output.extend(c.to_uppercase()),
                        "capitalize" => output.extend(c.to_lowercase()),
                        _ if c.is_uppercase() => output.extend(c.to_lowercase()),
                        _ => output.extend(c.to_uppercase()),
                    }
                    previous_cased = c.is_alphabetic();
                }
                return bounded_string(output);
            }
            ("zfill", [size]) => {
                let size = width(size)?;
                let length = text.chars().count();
                if length >= size {
                    return self.copy_string(text);
                }
                let (sign, digits) = match text.chars().next() {
                    Some(c @ ('+' | '-')) => (c.to_string(), &text[1..]),
                    _ => (String::new(), text),
                };
                self.allocate(size)?;
                return bounded_string(format!("{sign}{}{digits}", "0".repeat(size - length)));
            }
            ("center" | "ljust" | "rjust", [size, rest @ ..]) if rest.len() <= 1 => {
                let size = width(size)?;
                let fill = fill(rest.first())?;
                let length = text.chars().count();
                if length >= size {
                    return self.copy_string(text);
                }
                self.allocate(size * 4)?;
                let total = size - length;
                let (left, right) = match name {
                    "ljust" => (0, total),
                    "rjust" => (total, 0),
                    // CPython puts the odd fill character on the left when
                    // both the padding and the width are odd.
                    _ => {
                        let left = total / 2 + (total & size & 1);
                        (left, total - left)
                    }
                };
                let pad = |count: usize| fill.to_string().repeat(count);
                return bounded_string(format!("{}{text}{}", pad(left), pad(right)));
            }
            ("partition" | "rpartition", [separator]) => {
                let separator = string(separator)?;
                if separator.is_empty() {
                    return Err(invalid());
                }
                let found = if name == "partition" {
                    text.split_once(separator)
                } else {
                    text.rsplit_once(separator)
                };
                let parts = match found {
                    Some((head, tail)) => [head, separator, tail],
                    None if name == "partition" => [text, "", ""],
                    None => ["", "", text],
                };
                self.allocate(text.len() + separator.len())?;
                return Ok(Value::Tuple(
                    parts
                        .iter()
                        .map(|part| Value::String(Arc::from(*part)))
                        .collect(),
                ));
            }
            ("rsplit", [separator, count]) if !matches!(separator, Value::None) => {
                let separator = string(separator)?;
                let count = integer(count)?;
                if separator.is_empty() {
                    return Err(invalid());
                }
                let parts: Vec<&str> = if count < 0 {
                    text.split(separator).collect()
                } else {
                    let mut parts: Vec<&str> = text
                        .rsplitn(
                            usize::try_from(count)
                                .unwrap_or(usize::MAX)
                                .saturating_add(1),
                            separator,
                        )
                        .collect();
                    parts.reverse();
                    parts
                };
                if parts.len() > ITEMS {
                    return Err(invalid());
                }
                self.allocate_sequence(parts.len())?;
                let mut values = Vec::new();
                for part in parts {
                    values.push(self.copy_string(part)?);
                }
                return Ok(Value::List(values.into()));
            }
            ("rsplit", rest) if rest.len() <= 1 => {
                return self.method(value, "split", rest);
            }
            ("removeprefix", [prefix]) => {
                let prefix = string(prefix)?;
                return self.copy_string(text.strip_prefix(prefix).unwrap_or(text));
            }
            ("removesuffix", [suffix]) => {
                let suffix = string(suffix)?;
                return self.copy_string(text.strip_suffix(suffix).unwrap_or(text));
            }
            _ => {}
        }
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
    fn upstream_globals_builtins_and_methods_follow_python() {
        let text = |source: &str| match run(source) {
            Value::String(value) => value.to_string(),
            other => panic!("{source}: {other:?}"),
        };
        let int = |source: &str| match run(source) {
            Value::Int(value) => value,
            other => panic!("{source}: {other:?}"),
        };
        // Documented upstream filters.
        assert_eq!(
            run("datetime(2025, 6, 1) >= datetime(2025, 5, 1)"),
            Value::Bool(true)
        );
        assert_eq!(
            run("datetime(2025, 5, 1) + timedelta(days=1) == datetime(2025, 5, 2)"),
            Value::Bool(true)
        );
        assert_eq!(
            run("(datetime(2025, 5, 2) - datetime(2025, 5, 1)) == timedelta(hours=24)"),
            Value::Bool(true)
        );
        assert!(truthy(&run("re.search(r'foo(bar)+', 'x foobarbar y')")));
        assert_eq!(
            run("re.search(r'foo(bar)+', 'x foobarbar y')[1]"),
            run("'bar'")
        );
        assert_eq!(run("re.match(r'x', 'yx')"), Value::None);
        assert!(truthy(&run("re.match(r'y', 'yx')")));
        assert_eq!(run("re.fullmatch(r'a|ab', 'ab')[0]"), run("'ab'"));
        assert_eq!(text("re.sub(r'(b)a', r'\\1-', 'xbay')"), "xb-y");
        assert_eq!(run("re.findall(r'b(a)', 'baba')"), run("['a', 'a']"));
        assert_eq!(run("print('x')"), Value::None);
        // Builtins.
        assert_eq!(int("sum([1, 2, 3])"), 6);
        assert_eq!(
            run("sorted(['b', 'c', 'a'], reverse=True)"),
            run("['c', 'b', 'a']")
        );
        assert_eq!(run("list(reversed((1, 2)))"), run("[2, 1]"));
        assert_eq!(run("list(range(1, 7, 2))"), run("[1, 3, 5]"));
        assert_eq!(run("list(enumerate(['a']))"), run("[(0, 'a')]"));
        assert_eq!(run("list(zip([1, 2], 'ab'))"), run("[(1, 'a'), (2, 'b')]"));
        assert_eq!(run("divmod(-7, 2)"), run("(-4, 1)"));
        assert_eq!(int("pow(2, 10)"), 1024);
        assert_eq!(text("hex(-255)"), "-0xff");
        assert_eq!(text("chr(65)"), "A");
        assert_eq!(int("ord('é')"), 233);
        assert_eq!(int("parse_int('12')"), 12);
        // Methods.
        assert_eq!(text("'hello wORLD'.title()"), "Hello World");
        assert_eq!(text("'hello World'.capitalize()"), "Hello world");
        assert_eq!(text("'Hello'.swapcase()"), "hELLO");
        assert_eq!(
            run("'123'.isdigit() and not 'a1'.isdigit()"),
            Value::Bool(true)
        );
        assert_eq!(run("'abc'.islower() and 'A1'.isupper()"), Value::Bool(true));
        assert_eq!(text("'-5'.zfill(4)"), "-005");
        assert_eq!(text("'Hello World'.center(15, '*')"), "**Hello World**");
        assert_eq!(text("'ab'.center(5)"), "  ab ");
        assert_eq!(text("'a'.rjust(3, '0')"), "00a");
        assert_eq!(run("'a b c'.partition(' ')"), run("('a', ' ', 'b c')"));
        assert_eq!(run("'a b c'.rpartition('x')"), run("('', '', 'a b c')"));
        assert_eq!(run("'a b c'.rsplit(' ', 1)"), run("['a b', 'c']"));
        assert_eq!(text("'file.jpg'.removesuffix('.jpg')"), "file");
        assert_eq!(run("{'a': 1}.keys()"), run("['a']"));
        assert_eq!(run("{'a': 1}.items()"), run("[('a', 1)]"));
        assert_eq!(int("['a', 'b'].index('b')"), 1);
        // printf-style formatting (expected values from CPython).
        for (source, expected) in [
            ("'%s-%d' % ('a', 5)", "a-5"),
            ("'%05.1f' % 2.5", "002.5"),
            ("'%x' % 255", "ff"),
            ("'%#X' % 255", "0XFF"),
            ("'%-5s|' % 'ab'", "ab   |"),
            ("'%5s|' % 'ab'", "   ab|"),
            ("'%+d' % 3", "+3"),
            ("'% d' % 3", " 3"),
            ("'%03d' % -7", "-07"),
            ("'%.2s' % 'abcdef'", "ab"),
            ("'%r' % 'x'", "'x'"),
            ("'%c' % 65", "A"),
            ("'%c' % 'z'", "z"),
            ("'%e' % 12345.678", "1.234568e+04"),
            ("'%.2E' % 0.000123", "1.23E-04"),
            ("'%g' % 0.0001234", "0.0001234"),
            ("'%g' % 123456789.0", "1.23457e+08"),
            ("'%(a)s=%(b)d' % {'a': 'k', 'b': 2}", "k=2"),
            ("'%%%d%%' % 5", "%5%"),
            ("'%s' % [1, 2]", "[1, 2]"),
            ("'%i' % 3.9", "3"),
            ("'%o' % 8", "10"),
            ("'%10.3f|' % 3.14159", "     3.142|"),
            ("'%-8.2f|' % 2.0", "2.00    |"),
            ("'%s %s' % (1, None)", "1 None"),
            ("'%s' % True", "True"),
        ] {
            assert_eq!(text(source), expected, "{source}");
        }
        assert!(super::super::parser::parse("'%s %s' % ('a',)").is_ok());
        // Comprehensions and generator expressions.
        assert_eq!(run("[t.upper() for t in ['a', 'b']]"), run("['A', 'B']"));
        assert_eq!(run("[x for x in [1, 2, 3] if x > 1 if x < 3]"), run("[2]"));
        assert_eq!(run("{t: 1 for t in 'ab'}"), run("{'a': 1, 'b': 1}"));
        assert_eq!(
            run("[k for k, v in {'a': 1, 'b': 2}.items() if v > 1]"),
            run("['b']")
        );
        assert_eq!(
            run("any(t in 'abc' for t in ('x', 'b'))"),
            Value::Bool(true)
        );
        assert_eq!(run("all(x > 0 for x in [1, 2])"), Value::Bool(true));
        assert_eq!(int("sum(x * 2 for x in range(3))"), 6);
        assert_eq!(run("(x for x in [1])"), run("[1]"));
        assert_eq!(run("[[1, 2], [3]]"), run("[[1, 2], [3]]"));
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
