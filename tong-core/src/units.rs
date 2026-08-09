//! Human-readable duration and size parsing.
//!
//! Used for store policy configuration (`[store] retention`, `[store]
//! max_size`, and their `TONG_STORE_*` environment overrides). Formats are
//! Cargo-style: durations are `<number><unit>` with units `s`, `m`, `h`,
//! `d`; sizes are `<number><unit>` with units `B`, `K`/`KB`, `M`/`MB`,
//! `G`/`GB`, `T`/`TB` (decimal) and `KiB`, `MiB`, `GiB`, `TiB` (binary).
//! A bare number is accepted for both and means seconds / bytes.

use std::time::Duration;

/// Failure parsing a duration or size string.
#[derive(Debug, PartialEq, Eq)]
pub enum UnitError {
    /// The string was not `<number><unit>`.
    InvalidFormat(String),
    /// The number did not parse.
    InvalidNumber(String),
}

impl std::fmt::Display for UnitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidFormat(text) => {
                write!(
                    f,
                    "{text:?} is not a valid duration or size (expected <number><unit>)"
                )
            }
            Self::InvalidNumber(text) => write!(f, "{text:?} is not a valid number"),
        }
    }
}

impl std::error::Error for UnitError {}

/// Parses a human duration such as `7d`, `24h`, `90m`, `30s` (or a bare
/// number of seconds).
pub fn parse_duration(text: &str) -> Result<Duration, UnitError> {
    let text = text.trim();
    let split = text
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let value: f64 = number
        .parse()
        .map_err(|_| UnitError::InvalidNumber(text.to_owned()))?;
    let multiplier = match unit.to_ascii_lowercase().as_str() {
        "" | "s" => 1.0,
        "m" => 60.0,
        "h" => 3600.0,
        "d" => 86_400.0,
        "w" => 604_800.0,
        other => {
            return Err(UnitError::InvalidFormat(format!(
                "{text} (unknown unit {other:?}; expected s, m, h, d, or w)"
            )));
        }
    };
    Ok(Duration::from_secs_f64(value * multiplier))
}

/// Parses a human size such as `10G`, `500M`, `2GiB` (or a bare byte
/// count). `K`/`M`/`G`/`T` are decimal (powers of 1000), like Cargo's GC
/// configuration; `KiB`/`MiB`/`GiB`/`TiB` are binary (powers of 1024).
pub fn parse_size(text: &str) -> Result<u64, UnitError> {
    let text = text.trim();
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let value: u64 = number
        .parse()
        .map_err(|_| UnitError::InvalidNumber(text.to_owned()))?;
    let multiplier = match unit.to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" => 1_000,
        "m" | "mb" => 1_000_000,
        "g" | "gb" => 1_000_000_000,
        "t" | "tb" => 1_000_000_000_000,
        "kib" => 1 << 10,
        "mib" => 1 << 20,
        "gib" => 1 << 30,
        "tib" => 1 << 40,
        other => {
            return Err(UnitError::InvalidFormat(format!(
                "{text} (unknown unit {other:?}; expected B, K, M, G, T, KiB, MiB, GiB, or TiB)"
            )));
        }
    };
    value
        .checked_mul(multiplier)
        .ok_or_else(|| UnitError::InvalidNumber(text.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_durations() {
        assert_eq!(
            parse_duration("7d").unwrap(),
            Duration::from_secs(7 * 86_400)
        );
        assert_eq!(
            parse_duration("24h").unwrap(),
            Duration::from_secs(24 * 3600)
        );
        assert_eq!(parse_duration("90m").unwrap(), Duration::from_secs(90 * 60));
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("42").unwrap(), Duration::from_secs(42));
        assert!(parse_duration("1w").is_ok());
        assert!(parse_duration("7x").is_err());
        assert!(parse_duration("").is_err());
    }

    #[test]
    fn parses_sizes() {
        assert_eq!(parse_size("10G").unwrap(), 10_000_000_000);
        assert_eq!(parse_size("500M").unwrap(), 500_000_000);
        assert_eq!(parse_size("2GiB").unwrap(), 2 * (1 << 30));
        assert_eq!(parse_size("1024").unwrap(), 1024);
        assert_eq!(parse_size("1KiB").unwrap(), 1024);
        assert!(parse_size("1X").is_err());
    }
}
