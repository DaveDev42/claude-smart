//! Render a unix epoch reset time into the hub-compatible display string —
//! `"Sep 3 at 8:59pm (Asia/Seoul)"` — so `UsageSection.resets` keeps the exact
//! shape [`crate::account::reset::resets_to_epoch_at`] already parses. Nothing
//! downstream (scoring's stale-age math, the picker's row rendering) needs to
//! change because of the switch to local collection: the local collector still
//! emits a `resets` string, just derived from `resets_at` instead of scraped
//! from `claude`'s own `/usage` screen.

use chrono::{DateTime, Timelike, Utc};
use chrono_tz::Tz;

/// Format `epoch_secs` in this machine's local IANA timezone (resolved via
/// `iana_time_zone::get_timezone()`).
///
/// Falls back to UTC — rendered as `"… (UTC)"` — when the OS can't name a
/// zone (e.g. a minimal container with no `/etc/localtime`), matching the
/// hub scraper's own UTC fallback for the equivalent case.
pub fn format_resets(epoch_secs: i64) -> String {
    format_resets_in(epoch_secs, local_tz())
}

/// Pure variant of [`format_resets`] that takes an explicit zone — the
/// testable core. The OS's local zone is not fixed across dev machines/CI, so
/// tests always call this directly with a fixed [`Tz`] rather than relying on
/// whatever zone the test runner's host happens to be in.
pub fn format_resets_in(epoch_secs: i64, tz: Tz) -> String {
    let utc = DateTime::<Utc>::from_timestamp(epoch_secs, 0).unwrap_or_else(|| {
        DateTime::<Utc>::from_timestamp(0, 0).expect("epoch 0 is always a valid Utc instant")
    });
    let local = utc.with_timezone(&tz);

    // Manual 12-hour conversion (rather than `Timelike::hour12`) so the exact
    // "0 o'clock becomes 12" mapping is spelled out and doesn't depend on
    // memorizing chrono's own boundary convention.
    let hour24 = local.hour();
    let is_pm = hour24 >= 12;
    let hour12 = match hour24 % 12 {
        0 => 12,
        h => h,
    };
    let ampm = if is_pm { "pm" } else { "am" };

    // "%-d" = day-of-month with no zero-padding (chrono implements this
    // itself, not delegated to libc strftime, so it's portable). Matches the
    // hub-scraper shape exactly: "Sep 3 at 8:59pm (Asia/Seoul)".
    format!(
        "{} at {}:{:02}{} ({})",
        local.format("%b %-d"),
        hour12,
        local.minute(),
        ampm,
        tz.name(),
    )
}

/// Resolve this machine's local IANA timezone, falling back to UTC.
fn local_tz() -> Tz {
    iana_time_zone::get_timezone()
        .ok()
        .and_then(|name| name.parse::<Tz>().ok())
        .unwrap_or(chrono_tz::UTC)
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::reset::resets_to_epoch_at;
    use chrono::TimeZone;

    /// 2026-09-03T11:59:00Z = 2026-09-03 20:59:00 in Asia/Seoul (UTC+9) — the
    /// exact example string from the design spec.
    const FIXED_EPOCH: i64 = 1_788_436_740;

    #[test]
    fn format_resets_in_matches_spec_example() {
        let tz: Tz = "Asia/Seoul".parse().unwrap();
        assert_eq!(
            format_resets_in(FIXED_EPOCH, tz),
            "Sep 3 at 8:59pm (Asia/Seoul)"
        );
    }

    #[test]
    fn format_resets_in_utc_zone() {
        // 2026-09-02T08:59:59Z rounds to 08:59 UTC (seconds are dropped —
        // the API's own resolution is truncated to whole seconds, and this
        // function itself only formats minutes; the fixture's epoch is the
        // full-precision resets_at truncated by DateTime::from_timestamp's
        // integer-seconds contract).
        let epoch = 1_788_339_599; // 2026-09-02T08:59:59Z
        assert_eq!(
            format_resets_in(epoch, chrono_tz::UTC),
            "Sep 2 at 8:59am (UTC)"
        );
    }

    #[test]
    fn format_resets_in_noon_and_midnight_boundaries() {
        // 2026-01-01T00:00:00Z — midnight UTC → 12am.
        let midnight = Utc
            .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
            .unwrap()
            .timestamp();
        assert_eq!(
            format_resets_in(midnight, chrono_tz::UTC),
            "Jan 1 at 12:00am (UTC)"
        );

        // 2026-01-01T12:00:00Z — noon UTC → 12pm.
        let noon = Utc
            .with_ymd_and_hms(2026, 1, 1, 12, 0, 0)
            .unwrap()
            .timestamp();
        assert_eq!(
            format_resets_in(noon, chrono_tz::UTC),
            "Jan 1 at 12:00pm (UTC)"
        );
    }

    /// Round-trip: format an epoch, then parse the resulting string back with
    /// `resets_to_epoch_at` (giving it a `now` just before the target date so
    /// the year-rollover branch doesn't fire) — must recover the same epoch.
    #[test]
    fn round_trips_through_resets_to_epoch_at() {
        let tz: Tz = "Asia/Seoul".parse().unwrap();
        let rendered = format_resets_in(FIXED_EPOCH, tz);
        assert_eq!(rendered, "Sep 3 at 8:59pm (Asia/Seoul)");

        let now = Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap();
        let parsed = resets_to_epoch_at(&rendered, now).expect("must re-parse");
        assert_eq!(parsed.timestamp(), FIXED_EPOCH);
    }
}
