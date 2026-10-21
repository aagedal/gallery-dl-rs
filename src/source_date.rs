//! Source `dt.convert` bounds: ISO first, then Python decimal integer coercion.
use serde_json::Value;
use time::{Date, Month, Weekday};

const MICROS: i64 = 1_000_000;

fn checked_timestamp(value: i64) -> Option<i64> {
    time::OffsetDateTime::from_unix_timestamp(value)
        .ok()
        .filter(|date| (1..=9999).contains(&date.year()))
        .map(|_| value)
}

pub(crate) fn convert(value: &Value) -> Option<i64> {
    if matches!(value, Value::Bool(false)) || value.as_f64() == Some(0.0) {
        return None;
    }
    match value {
        Value::String(value) => parse_string(value),
        value => parse_timestamp(value),
    }
}

/// Source `dt.parse_ts`: int-only coercion, including falsy numeric zero.
pub(crate) fn parse_timestamp(value: &Value) -> Option<i64> {
    match value {
        Value::String(value) => integer(value),
        Value::Bool(value) => Some(i64::from(*value)),
        Value::Number(value) => {
            if let Some(value) = value.as_i64() {
                checked_timestamp(value)
            } else {
                let value = value.as_f64()?;
                if !value.is_finite() || value < i64::MIN as f64 || value >= i64::MAX as f64 {
                    return None;
                }
                checked_timestamp(value.trunc() as i64)
            }
        }
        _ => None,
    }
}

fn integer(input: &str) -> Option<i64> {
    // Python int() uses Unicode whitespace, excluding the extra C0 characters
    // accepted by str.isspace(). Decimal Nd digits may be freely mixed.
    let input = input.trim_matches(char::is_whitespace);
    let (negative, input) = match input.as_bytes().first() {
        Some(b'-') => (true, &input[1..]),
        Some(b'+') => (false, &input[1..]),
        _ => (false, input),
    };
    let mut result = 0i64;
    let mut digits = 0usize;
    let mut previous_digit = false;
    for character in input.chars() {
        if character == '_' {
            if !previous_digit {
                return None;
            }
            previous_digit = false;
        } else {
            let digit = crate::python_unicode::decimal_digit(character)?;
            digits += 1;
            if digits > 4300 {
                return None;
            }
            result = result.checked_mul(10)?.checked_add(i64::from(digit))?;
            previous_digit = true;
        }
    }
    if !previous_digit {
        return None;
    }
    checked_timestamp(if negative { -result } else { result })
}

pub(crate) fn parse_string(input: &str) -> Option<i64> {
    iso(input).or_else(|| integer(input))
}

fn decimal(input: &[u8]) -> Option<u32> {
    if input.is_empty() {
        return None;
    }
    input.iter().try_fold(0u32, |value, digit| {
        digit
            .is_ascii_digit()
            .then(|| value * 10 + u32::from(digit - b'0'))
    })
}

fn separator(input: &[u8]) -> Option<usize> {
    if input.len() < 7 || !input[..4].iter().all(u8::is_ascii_digit) {
        return None;
    }
    if input.len() == 7 {
        return Some(7);
    }
    Some(if input[4] == b'-' {
        if input[5] == b'W' {
            if input.len() > 8 && input[8] == b'-' {
                if input.len() == 9 {
                    return None;
                }
                if input.get(10).is_some_and(u8::is_ascii_digit) {
                    8
                } else {
                    10
                }
            } else {
                8
            }
        } else {
            10
        }
    } else if input[4] == b'W' {
        let end = input[7..]
            .iter()
            .position(|c| !c.is_ascii_digit())
            .map_or(input.len(), |n| n + 7);
        if end < 9 {
            end
        } else if end % 2 == 0 {
            7
        } else {
            8
        }
    } else {
        8
    })
}

fn calendar(input: &[u8]) -> Option<Date> {
    let year = decimal(input.get(..4)?)? as i32;
    if !(1..=9999).contains(&year) {
        return None;
    }
    let separated = *input.get(4)? == b'-';
    let mut position = 4 + usize::from(separated);
    let date = if *input.get(position)? == b'W' {
        position += 1;
        let week = decimal(input.get(position..position + 2)?)? as u8;
        position += 2;
        let day = if position == input.len() {
            1
        } else {
            if separated {
                if *input.get(position)? != b'-' {
                    return None;
                }
                position += 1;
            }
            let day = decimal(input.get(position..position + 1)?)?;
            position += 1;
            day
        };
        if position != input.len() {
            return None;
        }
        let weekday = match day {
            1 => Weekday::Monday,
            2 => Weekday::Tuesday,
            3 => Weekday::Wednesday,
            4 => Weekday::Thursday,
            5 => Weekday::Friday,
            6 => Weekday::Saturday,
            7 => Weekday::Sunday,
            _ => return None,
        };
        Date::from_iso_week_date(year, week, weekday).ok()?
    } else {
        let month = decimal(input.get(position..position + 2)?)? as u8;
        position += 2;
        if separated {
            if *input.get(position)? != b'-' {
                return None;
            }
            position += 1;
        }
        let day = decimal(input.get(position..position + 2)?)? as u8;
        if position + 2 != input.len() {
            return None;
        }
        Date::from_calendar_date(year, Month::try_from(month).ok()?, day).ok()?
    };
    (1..=9999).contains(&date.year()).then_some(date)
}

fn clock(input: &[u8]) -> Option<[u32; 4]> {
    let mut values = [0; 4];
    let mut position = 0;
    let mut separated = false;
    for (part, value) in values[..3].iter_mut().enumerate() {
        *value = decimal(input.get(position..position + 2)?)?;
        position += 2;
        if position == input.len() {
            return Some(values);
        }
        if part == 0 {
            separated = input[position] == b':';
        }
        if part == 2 {
            break;
        }
        if separated {
            if input[position] != b':' {
                return None;
            }
            position += 1;
        }
    }
    let fraction = if matches!(input.get(position), Some(b'.' | b',')) {
        &input[position + 1..]
    } else if !separated && input.len() - position >= 2 {
        // Python's C parser accepts an implicit fraction after basic HHMMSS.
        // A single extra digit, or this spelling with colon separators, fails.
        &input[position..]
    } else {
        return None;
    };
    if fraction.is_empty() || !fraction.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let count = fraction.len().min(6);
    values[3] = decimal(&fraction[..count])? * 10u32.pow(6 - count as u32);
    Some(values)
}

fn clock_micros(values: [u32; 4], midnight: bool) -> Option<i64> {
    let [hour, minute, second, micros] = values;
    if minute > 59
        || second > 59
        || hour > 23 && !(midnight && hour == 24 && minute == 0 && second == 0 && micros == 0)
    {
        return None;
    }
    Some(i64::from(hour * 3600 + minute * 60 + second) * MICROS + i64::from(micros))
}

fn iso(input: &str) -> Option<i64> {
    let split = separator(input.as_bytes())?;
    let date = calendar(input.get(..split)?.as_bytes())?;
    if split == input.len() {
        return Some(date.midnight().assume_utc().unix_timestamp());
    }
    // Only the single separator is arbitrary Unicode; all date/time digits and
    // punctuation are ASCII. Its UTF-8 width does not affect component parsing.
    let rest = input.get(split..)?;
    let time = rest.get(rest.chars().next()?.len_utf8()..)?.as_bytes();
    let zone = time
        .iter()
        .position(|byte| matches!(byte, b'+' | b'-' | b'Z'));
    let local = clock_micros(clock(&time[..zone.unwrap_or(time.len())])?, true)?;
    let local = date.midnight().assume_utc().unix_timestamp() * MICROS + local;
    checked_timestamp(local.div_euclid(MICROS))?;
    let offset = match zone {
        None => 0,
        Some(index) if time[index] == b'Z' => {
            if index + 1 != time.len() {
                return None;
            }
            0
        }
        Some(index) => {
            let offset = clock_micros(clock(&time[index + 1..])?, false)?;
            if time[index] == b'-' { -offset } else { offset }
        }
    };
    checked_timestamp((local - offset).div_euclid(MICROS))
}
