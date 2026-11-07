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

/// Upstream `build_duration_func_ex` (`sleep-retries`, `sleep-429`): a
/// duration, or `exp[:BASE[:START[:MAX]]]=DURATION` /
/// `lin[:START[:MAX]]=DURATION` growing with the attempt number.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RetrySleep {
    range: SleepRange,
    growth: Growth,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Growth {
    Fixed,
    Exponential { base: f64, start: f64, max: f64 },
    Linear { start: f64, max: f64 },
}

impl RetrySleep {
    pub fn from_value(value: &serde_json::Value) -> Result<Option<Self>> {
        let fixed = |range: Option<SleepRange>| {
            range.map(|range| Self {
                range,
                growth: Growth::Fixed,
            })
        };
        let Some(text) = value.as_str().filter(|text| text.contains('=')) else {
            return SleepRange::from_value(value).map(fixed);
        };
        let invalid = || error("invalid retry sleep duration");
        let (head, duration) = text.split_once('=').ok_or_else(invalid)?;
        let range = SleepRange::parse(duration)?;
        let (kind, args) = head.split_once(':').unwrap_or((head, ""));
        let number = |text: &str, default: f64| -> Result<f64> {
            if text.is_empty() {
                Ok(default)
            } else {
                text.parse::<f64>()
                    .ok()
                    .filter(|value| value.is_finite())
                    .ok_or_else(invalid)
            }
        };
        let mut parts = args.split(':');
        let growth = if !kind.is_empty() && "exponential".starts_with(kind) {
            let base = number(parts.next().unwrap_or_default(), 2.0)?;
            let start = number(parts.next().unwrap_or_default(), 0.0)?;
            let max = number(parts.next().unwrap_or_default(), 3600.0)?;
            Growth::Exponential { base, start, max }
        } else if !kind.is_empty() && "linear".starts_with(kind) {
            let start = number(parts.next().unwrap_or_default(), 0.0)?;
            let max = number(parts.next().unwrap_or_default(), 3600.0)?;
            Growth::Linear { start, max }
        } else {
            return Err(invalid());
        };
        Ok(Some(Self { range, growth }))
    }

    /// The wait before retry `attempt` (1 for the first retry).
    pub fn delay(&self, attempt: u32) -> Duration {
        let value = self.range.sample().as_secs_f64();
        let n = f64::from(attempt.max(1));
        let seconds = match self.growth {
            Growth::Fixed => value,
            // Without arguments upstream applies no start or maximum.
            Growth::Exponential { base, start, max } => {
                (start + value * base.powf(n - 1.0)).min(max)
            }
            Growth::Linear { start, max } => (start + value * n).min(max),
        };
        Duration::from_secs_f64(seconds.clamp(0.0, 86400.0))
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

    #[test]
    fn retry_sleeps_grow_like_upstream() {
        let parse = |text: &str| {
            RetrySleep::from_value(&serde_json::json!(text))
                .unwrap()
                .unwrap()
        };
        let seconds = |sleep: RetrySleep, n| sleep.delay(n).as_secs_f64();
        assert_eq!(seconds(parse("lin=1"), 3), 3.0);
        assert_eq!(seconds(parse("l:10:12=1"), 5), 12.0);
        assert_eq!(seconds(parse("exp=2"), 3), 8.0);
        assert_eq!(seconds(parse("e:3:1=1"), 3), 10.0);
        assert_eq!(seconds(parse("5"), 9), 5.0);
        assert!(RetrySleep::from_value(&serde_json::json!("quad=1")).is_err());
    }
}
