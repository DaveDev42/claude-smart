//! Account pick + scoring public surface.
//!
//! This module is the entry-point for account-selection logic:
//!
//! - [`pick_account`] — choose the best profile to launch under (spec §2).
//! - [`current_usage`] — emit `(session_pct, week_all_pct)` for one profile.
//! - [`ProfileMap`] — re-exported profile→dir map loaded from `profiles.json`.
//!
//! Submodules:
//! - `profiles` — load `~/.config/claude-as/profiles.json` (REAL impl).
//! - `scoring`  — scoring/tie-break/exclusions (stub, Phase 6).
//! - `reset`    — parse `"Jun 4 at 9pm (Asia/Seoul)"` → UTC epoch (stub, Phase 6).

pub mod profiles;
pub mod reset;
pub mod scoring;

pub use profiles::ProfileMap;

use crate::usage::{self, FetchError, UsageData};
use scoring::{ScoringError, ScoringResult};

/// Choose the best profile to switch to.
///
/// # Parameters
/// - `current_profile`: name of the currently active profile (from the leaf of
///   `CLAUDE_CONFIG_DIR`, or empty string when unset).
/// - `include_current`: when `true`, return `Ok(None)` if the winner equals
///   `current_profile` (no-op switch).
///
/// # Returns
/// - `Ok(Some(name))` — caller should switch `CLAUDE_CONFIG_DIR` to this profile.
/// - `Ok(None)` — winner is already current (`include_current` was `true`).
/// - `Err(ScoringError::AllSaturated)` — no viable candidate; caller warns and
///   keeps the current profile.
/// - `Err(ScoringError::FetchFailed(_))` — hub down / negative-cache active;
///   caller opens the hub-down interactive picker (spec §4a).
pub fn pick_account(current_profile: &str, include_current: bool) -> ScoringResult {
    let data: UsageData = usage::fetch().map_err(ScoringError::FetchFailed)?;
    scoring::pick_best(&data, current_profile, include_current)
}

/// Return `(session_pct, week_all_pct)` for `profile`, or `None` when the
/// profile is errored, absent from the cache, or the fetch fails.
///
/// Spec §2: `current-usage <profile>` → `<session_pct> <week_all_pct>` on
/// stdout, or empty (errored profile ⇒ empty).
pub fn current_usage(profile: &str) -> Option<(i64, i64)> {
    let data = usage::fetch().ok()?;
    data.current_usage(profile)
}
