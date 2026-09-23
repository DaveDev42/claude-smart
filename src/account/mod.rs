//! Account pick + scoring public surface.
//!
//! This module is the entry-point for account-selection logic:
//!
//! - [`pick_account`] — choose the best profile to launch under.
//! - [`current_usage`] — emit `(session_pct, week_all_pct)` for one profile.
//! - [`ProfileMap`] — re-exported profile→dir map loaded from `profiles.json`.
//!
//! Submodules:
//! - `profiles` — load `~/.config/claude-as/profiles.json`.
//! - `scoring`  — scoring/tie-break/exclusions; complete, fixture-tested.
//! - `reset`    — parse `"Jun 4 at 9pm (Asia/Seoul)"` → UTC epoch; complete,
//!   fixture-tested.

pub mod profiles;
pub mod reset;
pub mod scoring;

pub use profiles::ProfileMap;

use crate::usage::{self, UsageData};
use scoring::{PickPolicy, ScoringError, ScoringResult};

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
/// - `Err(ScoringError::FetchFailed(_))` — usage collection failed / negative-cache
///   active; caller opens the stale-usage interactive picker (see
///   [`crate::picker::account`]).
///
/// Applies the staleness gate (proactive / CLI path). For the reactive hook —
/// which must switch off an already-limited profile even on stale data — use
/// [`pick_account_gated`] with `apply_stale_gate=false`.
pub fn pick_account(current_profile: &str, include_current: bool) -> ScoringResult {
    pick_account_gated(current_profile, include_current, true)
}

/// [`pick_account`] with explicit control over the staleness gate.
///
/// `apply_stale_gate=true` is the proactive / CLI behaviour (refuse to score on
/// stale data → caller opens the picker). `apply_stale_gate=false` is the
/// reactive-hook behaviour: the hook fires because the current profile already
/// hit a limit and is non-interactive, so it scores even on stale numbers to
/// pick the freshest-known best rather than strand the user on the limited
/// profile. See [`scoring::pick_best`] for the gate rationale.
pub fn pick_account_gated(
    current_profile: &str,
    include_current: bool,
    apply_stale_gate: bool,
) -> ScoringResult {
    pick_account_with(
        current_profile,
        &PickPolicy {
            include_current,
            apply_stale_gate,
            ..Default::default()
        },
    )
}

/// Full-policy account pick over freshly fetched usage (see
/// [`scoring::pick_best_with`]). The Orca slot, when Orca mode is ON, is
/// always added to `policy.exclude`: it is not an account of its own. With
/// Orca mode OFF the exclusion list is exactly the caller's, so this is the
/// pre-Orca [`pick_account_gated`].
pub fn pick_account_with(current_profile: &str, policy: &PickPolicy<'_>) -> ScoringResult {
    let data: UsageData = usage::fetch().map_err(ScoringError::FetchFailed)?;
    score_excluding_slot(&data, current_profile, policy)
}

/// [`pick_account_with`] over the positive usage cache only: no network, no
/// collector. For `claude -p` / `--print` launches, which must not pay for a
/// usage fetch. A missing or unreadable cache reads as
/// [`ScoringError::NoUsableData`].
pub fn pick_account_cached(current_profile: &str, policy: &PickPolicy<'_>) -> ScoringResult {
    let data = crate::cmd::usage::read_usage_cache().ok_or(ScoringError::NoUsableData)?;
    score_excluding_slot(&data, current_profile, policy)
}

fn score_excluding_slot(
    data: &UsageData,
    current_profile: &str,
    policy: &PickPolicy<'_>,
) -> ScoringResult {
    let slot = crate::orca::slot::current();
    let mut exclude: Vec<&str> = policy.exclude.to_vec();
    if let Some(s) = &slot {
        exclude.push(s.name.as_str());
    }
    let merged = PickPolicy {
        exclude: &exclude,
        ..*policy
    };
    scoring::pick_best_with(data, current_profile, &merged, chrono::Utc::now())
}

/// Return `(session_pct, week_all_pct)` for `profile`, or `None` when the
/// profile is errored, absent from the cache, or the fetch fails.
///
/// `current-usage <profile>` → `<session_pct> <week_all_pct>` on stdout, or
/// empty (errored profile ⇒ empty).
pub fn current_usage(profile: &str) -> Option<(i64, i64)> {
    let data = usage::fetch().ok()?;
    data.current_usage(profile)
}
