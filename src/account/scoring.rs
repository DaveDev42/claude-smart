//! Account scoring: choose the best profile to launch under.
//!
//! Logic (spec §2 Account pick + scoring):
//!
//! 1. Fetch [`UsageData`] via `usage::fetch()`.
//! 2. Score each profile: highest `week_all.pct` among viable candidates.
//! 3. Tie-break: soonest reset epoch (via `reset::resets_to_epoch`).
//! 4. Exclusions: `errors{}` entry; `session.pct >= LIMIT_PCT(99)`;
//!    `week_all.pct >= SATURATION_PCT(95)`; absent `session.pct` → encoded `-1`.
//! 5. `--include-current`: if winner == current → return `None` (no-op switch).
//! 6. All saturated/errored → `Err(ScoringError::AllSaturated)` (warn-and-proceed
//!    at call site; NOT a hub-down picker trigger).
//! 7. `FetchError` → caller opens the hub-down picker (NOT handled here).
//!
//! This is a STUB implementation (todo!) per the PHASE 0 contract.
//! Signatures and types are final; bodies are implemented in Phase 6.

use crate::usage::{FetchError, UsageData};

/// The usage percentage at which a session is considered limit-hit and excluded.
pub const LIMIT_PCT: i64 = 99;

/// The usage percentage at which a profile is considered saturated and excluded.
pub const SATURATION_PCT: i64 = 95;

/// Sentinel value for "absent session pct" (profile in usage data but no
/// session section, or section is null).
pub const ABSENT_SESSION_PCT: i64 = -1;

/// Errors that `pick_best` can return.
#[derive(Debug, thiserror::Error)]
pub enum ScoringError {
    /// All profiles are either saturated, session-limited, or errored.
    /// Caller should warn and proceed on the current profile.
    #[error("all profiles are saturated or at session limit")]
    AllSaturated,

    /// The usage fetch failed (hub down / negative-cache cooldown).
    /// Caller should open the hub-down interactive picker.
    #[error("usage fetch failed: {0}")]
    FetchFailed(#[from] FetchError),
}

/// Result of scoring: the profile name to switch to, or `None` if already on
/// the best profile (`--include-current` with winner == current).
pub type ScoringResult = Result<Option<String>, ScoringError>;

/// Pick the best profile given fetched usage data.
///
/// # Parameters
/// - `data`: fetched [`UsageData`] (already validated JSON).
/// - `current_profile`: name of the currently active profile (may be empty string
///   when `CLAUDE_CONFIG_DIR` is unset).
/// - `include_current`: if `true`, return `None` when the winner is the current
///   profile (caller treats this as "no switch needed").
///
/// # Returns
/// - `Ok(Some(name))` — switch to this profile.
/// - `Ok(None)` — winner is current and `include_current` is true (no-op).
/// - `Err(ScoringError::AllSaturated)` — no viable candidate; caller warns and
///   proceeds.
pub fn pick_best(
    _data: &UsageData,
    _current_profile: &str,
    _include_current: bool,
) -> ScoringResult {
    todo!("account::scoring::pick_best — Phase 6 implementation")
}

/// Return the `(session_pct, week_all_pct)` for a given profile from `data`,
/// or `(ABSENT_SESSION_PCT, -1)` when the profile or its sections are absent.
///
/// Used by `cmd_current_usage` in `main.rs` to emit the two-number stdout line.
pub fn current_usage_pcts(
    _data: &UsageData,
    _profile: &str,
) -> (i64, i64) {
    todo!("account::scoring::current_usage_pcts — Phase 6 implementation")
}

/// Helper: is a profile excluded from candidacy?
///
/// Returns `true` when the profile should be skipped in scoring:
/// - present in `errors` map,
/// - `session.pct >= LIMIT_PCT`, or
/// - `week_all.pct >= SATURATION_PCT`.
fn is_excluded(_data: &UsageData, _profile: &str) -> bool {
    todo!("account::scoring::is_excluded — Phase 6 implementation")
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    // Scoring logic tests will live here in Phase 6.
    // Placeholder to confirm the test module compiles.
    #[test]
    fn constants_are_sane() {
        assert!(super::LIMIT_PCT > super::SATURATION_PCT);
        assert!(super::ABSENT_SESSION_PCT < 0);
    }
}
