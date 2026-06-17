//! Parse the `resets` string from the usage API into a UTC Unix epoch.
//!
//! Input forms (from `parse-usage.py` / the hub's `/cc-usage/api/data/limits`):
//!
//! - `"9pm (Asia/Seoul)"`            — time-only, implied current or next day
//! - `"8:20pm (Asia/Seoul)"`         — time with minutes, implied day
//! - `"Jun 4 at 9pm (Asia/Seoul)"`   — explicit date + time
//! - `"Jun 18 at 9pm (Asia/Seoul)"`
//!
//! All are local times in the given IANA timezone. Seconds are always `:00`
//! (the API never emits sub-minute granularity). Next-year rollover applies
//! when the inferred date is in the past: if the parsed datetime is before
//! `now`, add one year and retry.
//!
//! This is a STUB implementation (todo!) per the PHASE 0 contract.
//! The signature and types are final; bodies will be implemented in Phase 6.

use chrono::{DateTime, Utc};

/// Error type for reset-epoch parsing failures.
#[derive(Debug, thiserror::Error)]
pub enum ResetParseError {
    #[error("empty resets string")]
    Empty,
    #[error("unrecognised resets format: {0:?}")]
    UnrecognisedFormat(String),
    #[error("unknown timezone: {0:?}")]
    UnknownTimezone(String),
    #[error("date arithmetic overflow")]
    Overflow,
}

/// Parse a `resets` string from the usage API into a UTC [`DateTime`].
///
/// # Examples
///
/// ```ignore
/// let epoch = resets_to_epoch("Jun 4 at 9pm (Asia/Seoul)")?;
/// ```
///
/// Rules:
/// 1. Strip the timezone label `(...)` and parse as IANA name.
/// 2. Inject `:00` seconds if absent.
/// 3. Parse the local datetime in that timezone.
/// 4. If the result is before `now`, advance by one year (next-year rollover).
pub fn resets_to_epoch(resets: &str) -> Result<DateTime<Utc>, ResetParseError> {
    if resets.trim().is_empty() {
        return Err(ResetParseError::Empty);
    }
    todo!("account::reset::resets_to_epoch — Phase 6 implementation (non-empty input)")
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_string_is_err() {
        assert!(matches!(resets_to_epoch(""), Err(ResetParseError::Empty) | Err(_)));
    }
}
