//! Source formatter duration offsets, including Python integer arithmetic.
use super::{Value, python_integer};
use crate::{Result, error};
use time::OffsetDateTime;

#[derive(Clone, Copy, Debug)]
pub(super) enum Offset {
    Seconds(i64),
    Local,
}

// Decimal magnitudes allow cancellation before checking timedelta's bounds.
struct Integer {
    negative: bool,
    digits: Vec<u8>, // least significant first
}
impl Integer {
    fn parse(text: &str) -> Result<Self> {
        let text = match python_integer(text) {
            Some(Value::Int(value)) => value.to_string(),
            Some(Value::BigInt(value)) => value,
            _ => return Err(error("invalid template offset integer")),
        };
        Ok(Self {
            negative: text.starts_with('-'),
            digits: text
                .trim_start_matches('-')
                .bytes()
                .rev()
                .map(|b| b - b'0')
                .collect(),
        })
    }
    fn positive(&self) -> bool {
        !self.negative && self.digits.iter().any(|&d| d != 0)
    }
    fn multiply(&mut self, factor: u16) {
        let mut carry = 0;
        for digit in &mut self.digits {
            let value = u16::from(*digit) * factor + carry;
            *digit = (value % 10) as u8;
            carry = value / 10;
        }
        while carry != 0 {
            self.digits.push((carry % 10) as u8);
            carry /= 10;
        }
    }
    fn add(mut self, mut other: Self) -> Self {
        if self.negative == other.negative {
            self.digits
                .resize(self.digits.len().max(other.digits.len()), 0);
            let mut carry = 0;
            for (i, digit) in self.digits.iter_mut().enumerate() {
                let value = *digit + other.digits.get(i).copied().unwrap_or(0) + carry;
                *digit = value % 10;
                carry = value / 10;
            }
            if carry != 0 {
                self.digits.push(carry);
            }
        } else {
            let order = self
                .digits
                .len()
                .cmp(&other.digits.len())
                .then_with(|| self.digits.iter().rev().cmp(other.digits.iter().rev()));
            if order.is_lt() {
                std::mem::swap(&mut self, &mut other);
            }
            let mut borrow = 0i16;
            for (i, digit) in self.digits.iter_mut().enumerate() {
                let value = i16::from(*digit)
                    - i16::from(other.digits.get(i).copied().unwrap_or(0))
                    - borrow;
                *digit = value.rem_euclid(10) as u8;
                borrow = i16::from(value < 0);
            }
            while self.digits.len() > 1 && self.digits.last() == Some(&0) {
                self.digits.pop();
            }
        }
        self
    }
    fn seconds(self) -> Result<i64> {
        let mut value = 0i64;
        for digit in self.digits.iter().rev() {
            value = value
                .checked_mul(10)
                .and_then(|v| v.checked_add(i64::from(*digit)))
                .ok_or_else(|| error("template offset duration overflowed"))?;
        }
        if self.negative {
            value = -value;
        }
        let seconds = value
            .checked_mul(60)
            .ok_or_else(|| error("template offset duration overflowed"))?;
        if !(-86_399_999_913_600..=86_399_999_999_999).contains(&seconds) {
            return Err(error("template offset duration overflowed"));
        }
        Ok(seconds)
    }
}

impl Offset {
    pub(super) fn parse(argument: &str) -> Result<Self> {
        if argument.is_empty() || argument == "local" {
            return Ok(Self::Local);
        }
        let (hours, minutes) = argument.split_once(':').unwrap_or((argument, ""));
        let mut hours = Integer::parse(hours)?;
        let positive = hours.positive();
        hours.multiply(60);
        if !minutes.is_empty() {
            let mut minutes = Integer::parse(minutes)?;
            if !positive {
                minutes.negative = !minutes.negative;
            }
            hours = hours.add(minutes);
        }
        Ok(Self::Seconds(hours.seconds()?))
    }
    pub(super) fn seconds(self, date: OffsetDateTime) -> Result<i64> {
        match self {
            Self::Seconds(seconds) => Ok(seconds),
            Self::Local => local_offset(date),
        }
    }
}

fn local_offset(date: OffsetDateTime) -> Result<i64> {
    // Source subtracts a naive epoch before calling localtime.
    if date.offset() != time::UtcOffset::UTC {
        return Err(error("local template offsets require a naive date"));
    }
    #[cfg(any(unix, windows))]
    {
        // Python timedelta division produces a float before localtime. At large
        // dates this can round a subsecond value across a timezone transition.
        let seconds = (date.unix_timestamp() as f64 + f64::from(date.microsecond()) / 1_000_000.0)
            .floor() as i64;
        // time_t is narrower on some supported targets.
        #[allow(clippy::useless_conversion)]
        let timestamp: libc::time_t = seconds
            .try_into()
            .map_err(|_| error("local template offset timestamp overflowed"))?;
        // SAFETY: localtime writes to a valid, exclusively owned tm value.
        let mut local: libc::tm = unsafe { std::mem::zeroed() };
        #[cfg(unix)]
        let valid = !unsafe { libc::localtime_r(&timestamp, &mut local) }.is_null();
        #[cfg(windows)]
        let valid = unsafe { libc::localtime_s(&mut local, &timestamp) } == 0;
        if !valid {
            return Err(error("local template offset timestamp overflowed"));
        }
        #[cfg(unix)]
        {
            #[allow(clippy::useless_conversion)]
            Ok(local.tm_gmtoff.into())
        }
        #[cfg(windows)]
        {
            let calendar = time::Date::from_calendar_date(
                local.tm_year + 1900,
                (local.tm_mon as u8 + 1)
                    .try_into()
                    .map_err(|_| error("invalid local month"))?,
                local.tm_mday as u8,
            )
            .map_err(|_| error("invalid local date"))?;
            let clock =
                time::Time::from_hms(local.tm_hour as u8, local.tm_min as u8, local.tm_sec as u8)
                    .map_err(|_| error("invalid local time"))?;
            Ok(calendar.with_time(clock).assume_utc().unix_timestamp() - seconds)
        }
    }
    #[cfg(not(any(unix, windows)))]
    Err(error(
        "local template offsets are unsupported on this platform",
    ))
}
