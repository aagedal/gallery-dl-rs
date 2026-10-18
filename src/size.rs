//! File size selection, independent of HTTP framing and range validation.
use crate::{Result, error};
use std::fmt;

/// Parse gallery-dl's binary byte units (`500k`, `2.5M`, `3G`). Fractional
/// bytes round to the nearest even integer. Bare integer bytes retain u64 precision.
pub fn parse_bytes(value: &str) -> Result<u64> {
    let invalid = || {
        error("file size must be nonnegative bytes or a binary b/k/m/g/t/p amount within u64 range")
    };
    if value.len() > 128 {
        return Err(invalid());
    }
    let value = value.trim();
    let (amount, power) = match value.as_bytes().last().map(u8::to_ascii_lowercase) {
        Some(unit) if b"bkmgtp".contains(&unit) => (
            &value[..value.len() - 1],
            b"bkmgtp".iter().position(|&v| v == unit).unwrap() as u32,
        ),
        _ => (value, 0),
    };
    let amount = amount.trim();
    let integer = amount.strip_prefix('+').unwrap_or(amount);
    let multiplier = 1024u64.pow(power);
    if !integer.is_empty() && integer.bytes().all(|c| c.is_ascii_digit()) {
        return integer
            .parse::<u64>()
            .ok()
            .and_then(|n| n.checked_mul(multiplier))
            .ok_or_else(invalid);
    }
    let amount = amount.parse::<f64>().map_err(|_| invalid())?;
    let bytes = (amount * multiplier as f64).round_ties_even();
    if !amount.is_finite()
        || amount < 0.0
        || !bytes.is_finite()
        || bytes < 0.0
        || bytes >= 18446744073709551616.0
    {
        return Err(invalid());
    }
    Ok(bytes as u64)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SizeRejection {
    TooSmall { size: u64, minimum: u64 },
    TooLarge { size: u64, maximum: u64 },
}
impl fmt::Display for SizeRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooSmall { size, minimum } => {
                write!(f, "file size {size} is below minimum {minimum} bytes")
            }
            Self::TooLarge { size, maximum } => {
                write!(f, "file size {size} exceeds maximum {maximum} bytes")
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SizeLimits {
    minimum: Option<u64>,
    maximum: Option<u64>,
}
impl SizeLimits {
    /// A zero limit disables that bound. Both limits are inclusive.
    pub fn new(minimum: Option<u64>, maximum: Option<u64>) -> Result<Self> {
        let minimum = minimum.filter(|&n| n > 0);
        let maximum = maximum.filter(|&n| n > 0);
        if minimum.zip(maximum).is_some_and(|(min, max)| min > max) {
            return Err(error("minimum file size exceeds maximum file size"));
        }
        Ok(Self { minimum, maximum })
    }
    pub fn check(&self, size: u64) -> Option<SizeRejection> {
        if let Some(minimum) = self.minimum
            && size < minimum
        {
            return Some(SizeRejection::TooSmall { size, minimum });
        }
        self.check_max(size)
    }
    /// Apply while streaming, when the complete size may still be unknown.
    pub fn check_max(&self, size: u64) -> Option<SizeRejection> {
        self.maximum
            .filter(|&maximum| size > maximum)
            .map(|maximum| SizeRejection::TooLarge { size, maximum })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn binary_sizes_and_rounding_match_documented_units() {
        for (spec, expected) in [
            ("500k", 512000),
            ("2.5M", 2621440),
            (" 1 K ", 1024),
            ("1b", 1),
            ("0", 0),
            ("0.5", 0),
            ("1.5", 2),
            ("2.5", 2),
            ("3G", 3221225472),
            ("1e3", 1000),
        ] {
            assert_eq!(parse_bytes(spec).unwrap(), expected, "{spec}");
        }
        assert_eq!(parse_bytes(&u64::MAX.to_string()).unwrap(), u64::MAX);
        assert_eq!(parse_bytes("18014398509481983k").unwrap(), u64::MAX - 1023);
    }
    #[test]
    fn malformed_overflow_and_nonfinite_sizes_fail_privately() {
        for spec in [
            "",
            "secret",
            "-1",
            "NaN",
            "inf",
            "1e999",
            "18446744073709551616",
            "18446744073709551615k",
            "1MB",
            "1.0e20",
            "1e-9999k",
        ] {
            // Subnormal underflow is valid rounding to zero, as in the upstream parser.
            if spec == "1e-9999k" {
                assert_eq!(parse_bytes(spec).unwrap(), 0);
                continue;
            }
            let message = parse_bytes(spec).unwrap_err().to_string();
            assert!(!message.contains("secret"));
        }
        assert!(parse_bytes(&"1".repeat(129)).is_err());
    }
    #[test]
    fn inclusive_limits_and_explicit_zero() {
        let limits = SizeLimits::new(Some(2), Some(4)).unwrap();
        assert!(matches!(
            limits.check(1),
            Some(SizeRejection::TooSmall { .. })
        ));
        assert_eq!(limits.check(2), None);
        assert_eq!(limits.check(4), None);
        assert!(matches!(
            limits.check(5),
            Some(SizeRejection::TooLarge { .. })
        ));
        assert_eq!(limits.check_max(1), None);
        assert!(SizeLimits::new(Some(4), Some(2)).is_err());
        assert_eq!(
            SizeLimits::new(Some(0), Some(0)).unwrap(),
            SizeLimits::default()
        );
    }
}
