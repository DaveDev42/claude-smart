//! Parse the `resets` string from the usage API into a UTC Unix epoch.
//!
//! Input forms (from `parse-usage.py` / the hub's `/cc-usage/api/data/limits`):
//!
//! - `"9pm (Asia/Seoul)"`            — time-only, implied current or next day
//! - `"8:20pm (Asia/Seoul)"`         — time with minutes, implied day
//! - `"Jun 4 at 9pm (Asia/Seoul)"`   — explicit date + time
//! - `"Jun 18 at 9pm (Asia/Seoul)"`
//! - `"9pm"`                         — bare time, no tz (treated as UTC)
//!
//! All are local times in the given IANA timezone. Seconds are always `:00`
//! (the API never emits sub-minute granularity). Next-year rollover applies
//! when the inferred date is in the past: if the parsed datetime is before
//! `now`, add one year and retry.
//!
//! Shell source faithfully reproduced from:
//!   `shared/claude/claude-smart-helper.sh.j2` lines 831–881
//!
//! Key shell steps this Rust code mirrors:
//!
//! 1. `s="${s%% (*}"` — strip ` (TZ)` suffix (everything from first ` (` onward)
//!    and parse the timezone name from it.
//! 2. `s="${s/ at / }"` — replace literal ` at ` with a space.
//! 3. Trim leading/trailing whitespace.
//! 4. If the remaining string is a bare time `^[0-9]{1,2}(:[0-9]{2})?(am|pm)$`,
//!    prepend today's date (e.g. `"Jun 17"`) — shell: `date '+%b %d'`.
//! 5. Inject `:00` for bare-hour times: `"9pm"` → `"9:00pm"`, `"Jun 4 9pm"` →
//!    `"Jun 4 9:00pm"` (the sed regex in the shell handles both leading and
//!    non-leading positions).
//! 6. Parse as `%b %d %I:%M%p` in the extracted timezone with the current year.
//! 7. If result < now, advance by 1 year (next-year rollover).

use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;

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
/// Thin public wrapper that uses `Utc::now()` as the reference instant.
/// For deterministic tests, use [`resets_to_epoch_at`] instead.
///
/// # Examples
///
/// ```ignore
/// let dt = resets_to_epoch("Jun 4 at 9pm (Asia/Seoul)")?;
/// ```
pub fn resets_to_epoch(resets: &str) -> Result<DateTime<Utc>, ResetParseError> {
    resets_to_epoch_at(resets, Utc::now())
}

/// Pure, deterministic core: parse `resets` relative to the given `now`.
///
/// `now` is used for two purposes:
/// 1. Supplying today's date when the input is a bare time (no date part).
/// 2. The "already past?" test that triggers next-year rollover.
///
/// This function is `pub` so that calling modules (e.g. `scoring`) can
/// call it with a real `Utc::now()` without going through the thin wrapper.
/// Tests inject a fixed instant for determinism.
pub fn resets_to_epoch_at(
    resets: &str,
    now: DateTime<Utc>,
) -> Result<DateTime<Utc>, ResetParseError> {
    let s = resets.trim();
    if s.is_empty() {
        return Err(ResetParseError::Empty);
    }

    // ── Step 1: extract the IANA timezone from the " (TZ)" suffix ─────────────
    //
    // Shell: `s="${s%% (*}"` removes everything from the first ` (` onward.
    // We capture what was inside the parens so we know which zone to parse in.
    let (body, tz): (&str, Tz) = extract_tz(s)?;

    // ── Step 2: strip " at " → " " ────────────────────────────────────────────
    //
    // Shell: `s="${s/ at / }"` (replaces first occurrence only, which suffices
    // since the format never has two " at " tokens).
    let body = body.replace(" at ", " ");
    let body = body.trim().to_string();

    if body.is_empty() {
        return Err(ResetParseError::UnrecognisedFormat(resets.to_string()));
    }

    // ── Step 3: determine today's date in the target timezone ─────────────────
    //
    // "today" is the wall-clock date in `tz` at the moment `now` was captured.
    let now_in_tz = now.with_timezone(&tz);
    let today = now_in_tz.date_naive();

    // ── Step 4: if the body is a bare time, prepend today's date ──────────────
    //
    // Shell: `if printf '%s' "$s" | grep -qiE '^[0-9]{1,2}(:[0-9]{2})?(am|pm)$'`
    // then prepend `$(date '+%b %d')`.  We reproduce this by detecting the pattern
    // and building a prefixed body identical to what `date '+%b %d'` would produce.
    let body = if is_bare_time(&body) {
        // Format as "Mon DD" matching `date '+%b %d'` (abbreviated month + zero-padded day)
        let prefix = format_month_day(today);
        format!("{} {}", prefix, body)
    } else {
        body
    };

    // ── Step 5: inject ":00" for bare-hour times ──────────────────────────────
    //
    // Shell sed: `s/([^0-9:])([0-9]{1,2})(am|pm|AM|PM)/\1\2:00\3/; s/^([0-9]{1,2})(am|pm|AM|PM)/\1:00\2/`
    // Both anchored (leading position) and non-anchored (preceded by non-digit/non-colon).
    let body = inject_minutes(&body);

    // ── Step 6: parse ──────────────────────────────────────────────────────────
    //
    // Shell parses as `'%Y %b %d %I:%M%p'` with the current year prepended.
    // After our normalisation the body is `"<Mon> <DD> <H>:<MM>(am|pm)"`.
    // We try the current year first, then the next year if the result is in the past.
    let year = now_in_tz.year();
    let dt = parse_body_with_year(&body, year, tz).ok_or_else(|| {
        ResetParseError::UnrecognisedFormat(format!("{} (from {:?})", body, resets))
    })?;

    // ── Step 7: next-year rollover ────────────────────────────────────────────
    //
    // Shell: `[ "$epoch" -lt "$now" ] && epoch="$(date -j -f '%Y %b %d %I:%M%p' "$((y+1)) $s" ...)"`
    let dt_utc = dt.with_timezone(&Utc);
    if dt_utc < now {
        let dt_next = parse_body_with_year(&body, year + 1, tz).ok_or(ResetParseError::Overflow)?;
        return Ok(dt_next.with_timezone(&Utc));
    }

    Ok(dt_utc)
}

// ─── helpers ─────────────────────────────────────────────────────────────────

/// Extract the IANA timezone from the " (TZ)" suffix.
///
/// If no parenthesised suffix is present, default to UTC (bare strings like
/// `"9pm"` have no zone; the shell also falls back to the system tz, but UTC
/// is a safe cross-machine default for tests and production alike).
fn extract_tz(s: &str) -> Result<(&str, Tz), ResetParseError> {
    if let Some(paren_start) = s.find(" (") {
        let body = s[..paren_start].trim();
        let rest = &s[paren_start + 2..]; // skip " ("
        let tz_str = rest.trim_end_matches(')').trim();
        let tz: Tz = tz_str
            .parse()
            .map_err(|_| ResetParseError::UnknownTimezone(tz_str.to_string()))?;
        Ok((body, tz))
    } else {
        // No parenthesised suffix — treat as UTC.
        Ok((s, chrono_tz::UTC))
    }
}

/// Return true if `s` looks like a bare time `^[0-9]{1,2}(:[0-9]{2})?(am|pm)$` (case-insensitive).
///
/// Mirrors the shell grep: `grep -qiE '^[0-9]{1,2}(:[0-9]{2})?(am|pm)$'`.
fn is_bare_time(s: &str) -> bool {
    let s_lower = s.to_lowercase();
    // Must end with "am" or "pm"
    let (digits_part, _ampm) = if let Some(stem) = s_lower
        .strip_suffix("am")
        .or_else(|| s_lower.strip_suffix("pm"))
    {
        (stem, ())
    } else {
        return false;
    };
    // digits_part is either "H", "HH", "H:MM", or "HH:MM"
    if let Some(colon_pos) = digits_part.find(':') {
        let hour = &digits_part[..colon_pos];
        let min = &digits_part[colon_pos + 1..];
        !hour.is_empty()
            && hour.len() <= 2
            && hour.chars().all(|c| c.is_ascii_digit())
            && min.len() == 2
            && min.chars().all(|c| c.is_ascii_digit())
    } else {
        // No colon — must be 1 or 2 digits
        !digits_part.is_empty()
            && digits_part.len() <= 2
            && digits_part.chars().all(|c| c.is_ascii_digit())
    }
}

/// Format a `NaiveDate` as `"Mon DD"` (e.g. `"Jun  4"` → matches `date '+%b %d'`).
///
/// `date '+%b %d'` on macOS zero-pads the day to 2 digits with a leading space
/// for single-digit days (e.g. `"Jun  4"`), BUT our subsequent parse is flexible
/// enough to handle both `"Jun 4"` and `"Jun  4"` because we normalise
/// whitespace.  We use zero-padding to exactly match `%b %d` shell output.
fn format_month_day(date: NaiveDate) -> String {
    // chrono's %b is the abbreviated month name; %d is zero-padded day.
    // The shell's `date '+%b %d'` uses space-padded day on macOS (e.g. "Jun  4"),
    // but our `inject_minutes` and `parse_body_with_year` are whitespace-tolerant.
    date.format("%b %d").to_string()
}

/// Inject `:00` minutes into bare-hour time tokens in `s`.
///
/// Mirrors the shell sed:
/// ```sh
/// s="$(printf '%s' "$s" | sed -E \
///   's/([^0-9:])([0-9]{1,2})(am|pm|AM|PM)/\1\2:00\3/;
///    s/^([0-9]{1,2})(am|pm|AM|PM)/\1:00\2/')"
/// ```
///
/// Two passes are required:
/// 1. Leading position: `"9pm"` → `"9:00pm"`.
/// 2. Non-leading preceded by a non-digit/non-colon character: `"Jun 4 9pm"` →
///    `"Jun 4 9:00pm"`.
///
/// A time that already has minutes (`"5:59pm"`) does NOT match because the digit
/// before `am/pm` is preceded by a colon, which the character class `[^0-9:]`
/// excludes.
fn inject_minutes(s: &str) -> String {
    // We operate on bytes for simplicity; all characters here are ASCII.
    let lower = s.to_lowercase();
    let bytes = lower.as_bytes();
    let mut result = String::with_capacity(s.len() + 4);

    let mut i = 0usize;
    while i < bytes.len() {
        // Try to match a bare-hour token starting at `i`.
        // Conditions for a match at position `i`:
        //   - preceded by a non-digit, non-colon character (or at position 0),
        //   - followed by 1 or 2 ASCII digit bytes,
        //   - followed immediately by "am" or "pm" (no colon in between).
        if let Some((end, ampm)) = try_bare_hour_at(bytes, i) {
            // Check the preceding character guard.
            let preceded_by_safe = if i == 0 {
                true
            } else {
                let prev = bytes[i - 1];
                prev != b':' && !prev.is_ascii_digit()
            };

            if preceded_by_safe {
                // Emit the digit(s) + ":00" + ampm.
                let digits = &lower[i..end - 2]; // exclude the trailing "am"/"pm"
                result.push_str(digits);
                result.push_str(":00");
                result.push_str(ampm);
                i = end;
                continue;
            }
        }

        result.push(lower.chars().nth(i).unwrap_or(bytes[i] as char));
        i += 1;
    }

    result
}

/// At byte position `start` in `bytes`, attempt to match `[0-9]{1,2}(am|pm)`.
/// Returns `(end_byte_idx, ampm_str)` on success, `None` on failure.
fn try_bare_hour_at(bytes: &[u8], start: usize) -> Option<(usize, &'static str)> {
    let mut pos = start;
    // Consume 1 or 2 digits.
    if pos >= bytes.len() || !bytes[pos].is_ascii_digit() {
        return None;
    }
    pos += 1;
    if pos < bytes.len() && bytes[pos].is_ascii_digit() {
        pos += 1;
    }
    // Must be followed by "am" or "pm" (lowercase; we already lowercased).
    if bytes.get(pos..pos + 2) == Some(b"am") {
        Some((pos + 2, "am"))
    } else if bytes.get(pos..pos + 2) == Some(b"pm") {
        Some((pos + 2, "pm"))
    } else {
        None
    }
}

/// Parse a normalised body string (`"<Mon> <DD> <H>:<MM>(am|pm)"`) with the
/// given year and IANA timezone, returning a timezone-aware `DateTime<Tz>`.
///
/// Accepts both space-separated tokens and extra whitespace.
fn parse_body_with_year(body: &str, year: i32, tz: Tz) -> Option<DateTime<Tz>> {
    // body after normalisation has the form: "Mon DD H:MMam" or "Mon DD H:MMpm"
    // (all lowercase after inject_minutes).
    // We need to reconstruct a NaiveDateTime and localise it.
    //
    // Strategy: split on whitespace, collect tokens, then parse:
    //   tokens[0] = abbreviated month (e.g. "jun")
    //   tokens[1] = day (e.g. "04" or "4")
    //   tokens[2] = time with am/pm (e.g. "9:00pm")
    let tokens: Vec<&str> = body.split_whitespace().collect();
    if tokens.len() != 3 {
        return None;
    }
    let month_str = tokens[0];
    let day_str = tokens[1];
    let time_str = tokens[2]; // e.g. "9:00pm"

    // Parse the month name (case-insensitive abbreviated).
    let month = parse_month(month_str)?;
    let day: u32 = day_str.parse().ok()?;

    // Parse the time component: H:MMam or H:MMpm.
    let naive_time = parse_12h_time(time_str)?;

    let naive_date = NaiveDate::from_ymd_opt(year, month, day)?;
    let naive_dt = NaiveDateTime::new(naive_date, naive_time);

    // Localise to the target timezone.  `from_local_datetime` handles DST gaps/folds
    // by choosing the earlier (fold) or skipping (gap) as chrono-tz docs specify.
    tz.from_local_datetime(&naive_dt).single()
}

/// Parse an abbreviated English month name (case-insensitive) into a 1-based month number.
fn parse_month(s: &str) -> Option<u32> {
    match s.to_lowercase().as_str() {
        "jan" => Some(1),
        "feb" => Some(2),
        "mar" => Some(3),
        "apr" => Some(4),
        "may" => Some(5),
        "jun" => Some(6),
        "jul" => Some(7),
        "aug" => Some(8),
        "sep" => Some(9),
        "oct" => Some(10),
        "nov" => Some(11),
        "dec" => Some(12),
        _ => None,
    }
}

/// Parse a 12-hour time string like `"9:00pm"`, `"8:20am"`, `"12:00pm"`.
///
/// The shell parses with `%I:%M%p` (strptime); we reproduce that here.
fn parse_12h_time(s: &str) -> Option<NaiveTime> {
    // Must end with "am" or "pm" (already lowercased by inject_minutes).
    let (stem, is_pm) = if let Some(stem) = s.strip_suffix("pm") {
        (stem, true)
    } else if let Some(stem) = s.strip_suffix("am") {
        (stem, false)
    } else {
        return None;
    };

    let colon = stem.find(':')?;
    let hour_str = &stem[..colon];
    let min_str = &stem[colon + 1..];

    let hour: u32 = hour_str.parse().ok()?;
    let min: u32 = min_str.parse().ok()?;

    // Convert 12-hour to 24-hour:
    // 12am → 0, 1am → 1, …, 11am → 11
    // 12pm → 12, 1pm → 13, …, 11pm → 23
    let hour_24 = match (is_pm, hour) {
        (false, 12) => 0, // 12am = midnight
        (false, h) => h,
        (true, 12) => 12, // 12pm = noon
        (true, h) => h + 12,
    };

    NaiveTime::from_hms_opt(hour_24, min, 0)
}

// ─── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    // Fixed "now" used across all deterministic tests.
    // 2026-06-17T12:00:00Z — noon UTC, which is 21:00 KST (UTC+9).
    fn fixed_now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 17, 12, 0, 0).unwrap()
    }

    // Helper: assert that the parsed epoch equals an expected UTC datetime.
    fn assert_epoch(input: &str, expected_utc: DateTime<Utc>) {
        let result = resets_to_epoch_at(input, fixed_now())
            .unwrap_or_else(|e| panic!("parse failed for {:?}: {e}", input));
        assert_eq!(
            result, expected_utc,
            "input={:?}: expected {} got {}",
            input, expected_utc, result
        );
    }

    // ── empty / whitespace ──────────────────────────────────────────────────────

    #[test]
    fn empty_string_is_err() {
        let err = resets_to_epoch_at("", fixed_now());
        assert!(matches!(err, Err(ResetParseError::Empty)), "{:?}", err);
    }

    #[test]
    fn whitespace_only_is_err() {
        let err = resets_to_epoch_at("   ", fixed_now());
        assert!(matches!(err, Err(ResetParseError::Empty)), "{:?}", err);
    }

    // ── bare-time forms ─────────────────────────────────────────────────────────

    /// `"9pm (Asia/Seoul)"` — bare hour, tz present.
    /// Asia/Seoul = UTC+9.  9pm KST on Jun 17 = 12:00 UTC on Jun 17.
    /// fixed_now is 12:00 UTC, so this is NOT in the past — should give Jun 17.
    #[test]
    fn bare_hour_pm_with_tz_same_day() {
        // 9pm Asia/Seoul = UTC+9 → 21:00 - 09:00 = 12:00 UTC same day
        let expected = Utc.with_ymd_and_hms(2026, 6, 17, 12, 0, 0).unwrap();
        assert_epoch("9pm (Asia/Seoul)", expected);
    }

    /// `"8pm (Asia/Seoul)"` — 8pm KST = 11:00 UTC.
    /// Shell: prepend today's date → "Jun 17 8pm"; parse → Jun 17 2026 11:00 UTC.
    /// fixed_now is 12:00 UTC Jun 17 → 11:00 < 12:00 → past → try year+1 → Jun 17 2027.
    ///
    /// NOTE: the shell only has YEAR rollover for bare-time forms, not day rollover.
    /// "Jun 17 8pm 2026" past → "Jun 17 8pm 2027", not "Jun 18 8pm 2026".
    #[test]
    fn bare_hour_already_past_rolls_to_next_year() {
        // 8pm KST = 20:00 KST - 9h = 11:00 UTC; prepended with today (Jun 17)
        // Jun 17 11:00 UTC 2026 < Jun 17 12:00 UTC (fixed_now) → rollover to 2027
        let expected = Utc.with_ymd_and_hms(2027, 6, 17, 11, 0, 0).unwrap();
        assert_epoch("8pm (Asia/Seoul)", expected);
    }

    /// `"8:20pm (Asia/Seoul)"` — time with minutes, tz.
    /// Shell: prepend today (Jun 17) → "Jun 17 8:20pm"; parse → Jun 17 11:20 UTC 2026.
    /// fixed_now=12:00 UTC Jun 17 → past → next year → Jun 17 11:20 UTC 2027.
    #[test]
    fn time_with_minutes_rolls_to_next_year() {
        let expected = Utc.with_ymd_and_hms(2027, 6, 17, 11, 20, 0).unwrap();
        assert_epoch("8:20pm (Asia/Seoul)", expected);
    }

    /// `"6:50pm (Asia/Seoul)"` — 6:50pm KST = 09:50 UTC.
    /// Shell: prepend today (Jun 17) → "Jun 17 6:50pm"; parse → Jun 17 09:50 UTC 2026.
    /// fixed_now=12:00 UTC Jun 17 → past → next year → Jun 17 09:50 UTC 2027.
    #[test]
    fn bare_time_with_minutes_past_rolls_to_next_year() {
        let expected = Utc.with_ymd_and_hms(2027, 6, 17, 9, 50, 0).unwrap();
        assert_epoch("6:50pm (Asia/Seoul)", expected);
    }

    /// `"11pm (Asia/Seoul)"` — 11pm KST = 14:00 UTC.  fixed_now=12:00 UTC → future.
    #[test]
    fn late_evening_kst_is_future() {
        // 11pm KST = 23:00 KST - 9h = 14:00 UTC Jun 17
        let expected = Utc.with_ymd_and_hms(2026, 6, 17, 14, 0, 0).unwrap();
        assert_epoch("11pm (Asia/Seoul)", expected);
    }

    // ── date + time forms ────────────────────────────────────────────────────────

    /// `"Jun 18 at 9pm (Asia/Seoul)"` — explicit date in the future.
    #[test]
    fn explicit_date_future() {
        // Jun 18 9pm KST = Jun 18 21:00 KST = Jun 18 12:00 UTC
        let expected = Utc.with_ymd_and_hms(2026, 6, 18, 12, 0, 0).unwrap();
        assert_epoch("Jun 18 at 9pm (Asia/Seoul)", expected);
    }

    /// `"Jun 4 at 9pm (Asia/Seoul)"` — date in the past (Jun 4 < Jun 17) → next year.
    #[test]
    fn explicit_date_past_rolls_next_year() {
        // Jun 4 9pm KST = Jun 4 12:00 UTC (same UTC as above but Jun 4)
        // Jun 4 2026 < Jun 17 2026 (fixed_now) → rollover to 2027
        let expected = Utc.with_ymd_and_hms(2027, 6, 4, 12, 0, 0).unwrap();
        assert_epoch("Jun 4 at 9pm (Asia/Seoul)", expected);
    }

    /// `"Jun 3 at 5:59pm (Asia/Seoul)"` — date + minutes, past → next year.
    #[test]
    fn explicit_date_with_minutes_past_rolls_next_year() {
        // Jun 3 5:59pm KST = Jun 3 17:59 KST - 9h = Jun 3 08:59 UTC 2026
        // Jun 3 2026 < Jun 17 2026 → next year → Jun 3 2027 08:59 UTC
        let expected = Utc.with_ymd_and_hms(2027, 6, 3, 8, 59, 0).unwrap();
        assert_epoch("Jun 3 at 5:59pm (Asia/Seoul)", expected);
    }

    /// `"Dec 31 at 9pm (Asia/Seoul)"` parsed in June → future within the year.
    #[test]
    fn explicit_date_december_future_same_year() {
        // Dec 31 9pm KST = Dec 31 12:00 UTC 2026 (still in future from Jun 17)
        let expected = Utc.with_ymd_and_hms(2026, 12, 31, 12, 0, 0).unwrap();
        assert_epoch("Dec 31 at 9pm (Asia/Seoul)", expected);
    }

    /// `"Jan 1 at 9pm (Asia/Seoul)"` parsed in June → past → next year.
    #[test]
    fn explicit_date_january_past_rolls_next_year() {
        // Jan 1 9pm KST = Jan 1 12:00 UTC 2026 < Jun 17 2026 → 2027
        let expected = Utc.with_ymd_and_hms(2027, 1, 1, 12, 0, 0).unwrap();
        assert_epoch("Jan 1 at 9pm (Asia/Seoul)", expected);
    }

    // ── am/pm boundary ───────────────────────────────────────────────────────────

    /// `"12am"` = midnight = 0:00 in 24h (UTC, no tz).
    /// Shell: prepend today (Jun 17) → "Jun 17 12am"; parse → Jun 17 00:00 UTC 2026.
    /// fixed_now=12:00 UTC Jun 17 → past → next year → Jun 17 00:00 UTC 2027.
    #[test]
    fn midnight_12am() {
        // 12am UTC Jun 17 2026 = 00:00 UTC < 12:00 UTC (fixed_now) → rollover to 2027
        let expected = Utc.with_ymd_and_hms(2027, 6, 17, 0, 0, 0).unwrap();
        assert_epoch("12am", expected);
    }

    /// `"12pm"` = noon = 12:00 in 24h (UTC since no TZ given).
    /// fixed_now is exactly 12:00 UTC Jun 17 → NOT strictly past (== now, not <) → same day.
    #[test]
    fn noon_12pm_utc_not_past() {
        // 12pm UTC Jun 17 == fixed_now (12:00 UTC) → not strictly past → same day
        let expected = Utc.with_ymd_and_hms(2026, 6, 17, 12, 0, 0).unwrap();
        assert_epoch("12pm", expected);
    }

    /// `"1am (Asia/Seoul)"` = 01:00 KST.
    /// Shell: prepend today (Jun 17) → "Jun 17 1am KST"; parse → Jun 16 16:00 UTC 2026.
    /// fixed_now=12:00 UTC Jun 17 → past → next year → Jun 16 16:00 UTC 2027.
    ///
    /// NOTE: 1am KST Jun 17 = Jun 17 01:00 KST - 9h = Jun 16 16:00 UTC.
    /// The UTC date is Jun 16 (day before), not Jun 17. Still past → year rollover.
    #[test]
    fn early_morning_kst_rolls_to_next_year() {
        // 1am KST Jun 17 = Jun 16 16:00 UTC 2026 < Jun 17 12:00 UTC → rollover to 2027
        let expected = Utc.with_ymd_and_hms(2027, 6, 16, 16, 0, 0).unwrap();
        assert_epoch("1am (Asia/Seoul)", expected);
    }

    // ── timezone offset ──────────────────────────────────────────────────────────

    /// Test that a known non-Seoul timezone (America/New_York, UTC-4 in summer) is
    /// handled correctly.  `"9pm (America/New_York)"` = 21:00 - 4h = 01:00 UTC next day.
    #[test]
    fn new_york_timezone() {
        // fixed_now = Jun 17 12:00 UTC → Jun 17 9pm ET is still in the future
        // 9pm EDT = UTC-4 → 21:00 + 4 = 01:00 UTC Jun 18
        let expected = Utc.with_ymd_and_hms(2026, 6, 18, 1, 0, 0).unwrap();
        assert_epoch("9pm (America/New_York)", expected);
    }

    /// UTC timezone explicitly in parens.
    #[test]
    fn explicit_utc_timezone() {
        // Jun 20 9pm UTC = Jun 20 21:00 UTC — future
        let expected = Utc.with_ymd_and_hms(2026, 6, 20, 21, 0, 0).unwrap();
        assert_epoch("Jun 20 at 9pm (UTC)", expected);
    }

    // ── no-timezone (bare) forms ──────────────────────────────────────────────────

    /// `"9pm"` with no timezone defaults to UTC.  Same as the Seoul 9pm test but UTC.
    #[test]
    fn bare_time_no_tz_defaults_utc() {
        // 9pm UTC Jun 17 = 21:00 UTC Jun 17 → future (12:00 UTC is before 21:00 UTC)
        let expected = Utc.with_ymd_and_hms(2026, 6, 17, 21, 0, 0).unwrap();
        assert_epoch("9pm", expected);
    }

    // ── unknown timezone ─────────────────────────────────────────────────────────

    #[test]
    fn unknown_timezone_is_err() {
        let err = resets_to_epoch_at("9pm (Fake/Timezone)", fixed_now());
        assert!(
            matches!(err, Err(ResetParseError::UnknownTimezone(_))),
            "{:?}",
            err
        );
    }

    // ── internal helper unit tests ────────────────────────────────────────────────

    #[test]
    fn is_bare_time_recognises_forms() {
        assert!(is_bare_time("9pm"), "9pm");
        assert!(is_bare_time("9am"), "9am");
        assert!(is_bare_time("12pm"), "12pm");
        assert!(is_bare_time("8:20pm"), "8:20pm");
        assert!(is_bare_time("11:59am"), "11:59am");
        assert!(!is_bare_time("Jun 4 9pm"), "should not match date+time");
        assert!(!is_bare_time(""), "empty");
        assert!(!is_bare_time("9"), "no ampm");
        assert!(!is_bare_time("9:00"), "no ampm after colon");
    }

    #[test]
    fn inject_minutes_bare_hour_leading() {
        assert_eq!(inject_minutes("9pm"), "9:00pm");
        assert_eq!(inject_minutes("12am"), "12:00am");
    }

    #[test]
    fn inject_minutes_bare_hour_after_date() {
        // "jun 17 9pm" → "jun 17 9:00pm"
        let result = inject_minutes("jun 17 9pm");
        assert_eq!(result, "jun 17 9:00pm");
    }

    #[test]
    fn inject_minutes_already_has_minutes() {
        // "5:59pm" — has a colon before "pm" → no injection
        assert_eq!(inject_minutes("5:59pm"), "5:59pm");
        // "jun 17 8:20pm" — already has minutes
        assert_eq!(inject_minutes("jun 17 8:20pm"), "jun 17 8:20pm");
    }

    #[test]
    fn parse_12h_time_variants() {
        use chrono::Timelike;
        let t = parse_12h_time("9:00pm").unwrap();
        assert_eq!(t.hour(), 21);
        assert_eq!(t.minute(), 0);

        let t = parse_12h_time("12:00am").unwrap();
        assert_eq!(t.hour(), 0);

        let t = parse_12h_time("12:00pm").unwrap();
        assert_eq!(t.hour(), 12);

        let t = parse_12h_time("8:20am").unwrap();
        assert_eq!(t.hour(), 8);
        assert_eq!(t.minute(), 20);

        let t = parse_12h_time("11:59pm").unwrap();
        assert_eq!(t.hour(), 23);
        assert_eq!(t.minute(), 59);
    }

    #[test]
    fn parse_month_all_names() {
        let expected = [
            ("jan", 1u32),
            ("feb", 2),
            ("mar", 3),
            ("apr", 4),
            ("may", 5),
            ("jun", 6),
            ("jul", 7),
            ("aug", 8),
            ("sep", 9),
            ("oct", 10),
            ("nov", 11),
            ("dec", 12),
        ];
        for (name, num) in expected {
            assert_eq!(parse_month(name), Some(num), "month={name}");
            // Also test title case.
            let title = format!("{}{}", &name[..1].to_uppercase(), &name[1..]);
            assert_eq!(parse_month(&title), Some(num), "month={title}");
        }
        assert_eq!(parse_month("xyz"), None);
    }

    // ── real-world sample strings from the spec ──────────────────────────────────

    /// From spec §4a rendered examples and usage model test fixture.
    #[test]
    fn spec_sample_jun18_9pm_seoul() {
        // From SAMPLE_CACHE_JSON: "Jun 18 at 9pm (Asia/Seoul)"
        // Jun 18 9pm KST = Jun 18 12:00 UTC → future from Jun 17 12:00 UTC
        let expected = Utc.with_ymd_and_hms(2026, 6, 18, 12, 0, 0).unwrap();
        assert_epoch("Jun 18 at 9pm (Asia/Seoul)", expected);
    }

    /// From SAMPLE_CACHE_JSON work.week_all: "Jun 20 at 8:20pm (Asia/Seoul)"
    #[test]
    fn spec_sample_jun20_820pm_seoul() {
        // Jun 20 8:20pm KST = Jun 20 20:20 KST - 9h = Jun 20 11:20 UTC → future
        let expected = Utc.with_ymd_and_hms(2026, 6, 20, 11, 20, 0).unwrap();
        assert_epoch("Jun 20 at 8:20pm (Asia/Seoul)", expected);
    }

    /// `"9pm (Asia/Seoul)"` from personal.session in the spec.
    #[test]
    fn spec_sample_9pm_seoul_not_past() {
        // 9pm KST = 12:00 UTC Jun 17; fixed_now = 12:00 UTC Jun 17 → NOT past
        let expected = Utc.with_ymd_and_hms(2026, 6, 17, 12, 0, 0).unwrap();
        assert_epoch("9pm (Asia/Seoul)", expected);
    }

    // ── rollover year boundary ───────────────────────────────────────────────────

    /// A "now" in January sees a June reset as far in the future — no rollover.
    #[test]
    fn january_now_june_reset_no_rollover() {
        let now_jan = Utc.with_ymd_and_hms(2026, 1, 5, 12, 0, 0).unwrap();
        let result = resets_to_epoch_at("Jun 4 at 9pm (Asia/Seoul)", now_jan).unwrap();
        // Jun 4 9pm KST 2026 = Jun 4 12:00 UTC 2026 → future from Jan 5 → no rollover
        let expected = Utc.with_ymd_and_hms(2026, 6, 4, 12, 0, 0).unwrap();
        assert_eq!(result, expected);
    }

    /// A "now" in December sees a June reset in the past → rolls to next year.
    #[test]
    fn december_now_june_reset_rolls_next_year() {
        let now_dec = Utc.with_ymd_and_hms(2026, 12, 1, 12, 0, 0).unwrap();
        let result = resets_to_epoch_at("Jun 4 at 9pm (Asia/Seoul)", now_dec).unwrap();
        // Jun 4 9pm KST 2026 = Jun 4 12:00 UTC 2026 < Dec 1 2026 → rollover to 2027
        let expected = Utc.with_ymd_and_hms(2027, 6, 4, 12, 0, 0).unwrap();
        assert_eq!(result, expected);
    }
}
