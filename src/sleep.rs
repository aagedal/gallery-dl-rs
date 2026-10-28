//! Upstream-style `SECONDS` or `MIN-MAX` waits, e.g. `--sleep 1.5-3`.
use crate::{Result, error};
use std::time::Duration;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SleepRange {
    min: Duration,
    max: Duration,
}

impl SleepRange {
    pub fn parse(value: &str) -> Result<Self> {
        let invalid = || error("sleep must be SECONDS or MIN-MAX non-negative seconds up to 3600");
        let seconds = |value: &str| -> Result<Duration> {
            let value: f64 = value.trim().parse().map_err(|_| invalid())?;
            if !(0.0..=3600.0).contains(&value) {
                return Err(invalid());
            }
            Ok(Duration::from_secs_f64(value))
        };
        let (min, max) = match value.split_once('-') {
            Some((min, max)) => (seconds(min)?, seconds(max)?),
            None => {
                let value = seconds(value)?;
                (value, value)
            }
        };
        if min > max {
            return Err(invalid());
        }
        Ok(Self { min, max })
    }

    /// Upstream duration values: a number, a `MIN-MAX` string, or a
    /// `[MIN, MAX]` list. `None` for null/absent; `Err` for invalid values.
    pub fn from_value(value: &serde_json::Value) -> Result<Option<Self>> {
        use serde_json::Value;
        match value {
            Value::Null => Ok(None),
            Value::Number(n) => Self::parse(&n.to_string()).map(Some),
            Value::String(s) => Self::parse(s).map(Some),
            Value::Array(values) if (1..=2).contains(&values.len()) => {
                let text: Vec<String> = values
                    .iter()
                    .map(|v| match v {
                        Value::Number(n) => Ok(n.to_string()),
                        Value::String(s) => Ok(s.clone()),
                        _ => Err(error("invalid sleep duration")),
                    })
                    .collect::<Result<_>>()?;
                Self::parse(&text.join("-")).map(Some)
            }
            _ => Err(error("invalid sleep duration")),
        }
    }

    /// A uniformly chosen duration within the range. Jitter needs no
    /// cryptographic quality, so the clock's sub-second bits suffice.
    pub fn sample(&self) -> Duration {
        let span = self.max - self.min;
        if span.is_zero() {
            return self.min;
        }
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        self.min + span.mul_f64(f64::from(nanos % 1_000_000) / 1_000_000.0)
    }

    pub async fn wait(&self) {
        let duration = self.sample();
        if !duration.is_zero() {
            tokio::time::sleep(duration).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fixed_and_ranged_waits() {
        let fixed = SleepRange::parse("1.5").unwrap();
        assert_eq!(fixed.sample(), Duration::from_millis(1500));
        let range = SleepRange::parse("1-2").unwrap();
        for _ in 0..100 {
            let sample = range.sample();
            assert!(sample >= Duration::from_secs(1) && sample <= Duration::from_secs(2));
        }
        assert_eq!(SleepRange::parse("0").unwrap().sample(), Duration::ZERO);
        for bad in ["", "-1", "2-1", "x", "1-", "inf", "NaN", "3601", "1-2-3"] {
            assert!(SleepRange::parse(bad).is_err(), "{bad}");
        }
    }
}
