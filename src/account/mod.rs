//! Account pick + scoring public surface, keyed by Orca account id.
//!
//! - [`pick_account_gated`] — choose the best account to switch to.
//! - [`AccountSet`] — Orca's host accounts keyed by account id, plus `D`
//!   (csm keeps no registry of its own; see [`crate::orca`]).
//! - [`limit_switch`] — the supervisor's leader/follower decision.
//!
//! Submodules:
//! - `accounts` — the read-only adapter over Orca's account list.
//! - `scoring`  — scoring/tie-break/exclusions; complete, fixture-tested.
//! - `reset`    — parse `"Jun 4 at 9pm (Asia/Seoul)"` → UTC epoch; complete,
//!   fixture-tested.

pub mod accounts;
pub mod limit_switch;
pub mod reset;
pub mod scoring;

pub use accounts::AccountSet;

use crate::usage::{self, UsageData};
use scoring::{ScoringError, ScoringResult};

/// Choose the best account to switch to, from csm's cached usage only
/// ([`usage::fetch_cached`]: no usage command, network, Orca socket or
/// Keychain, since the hook calls this; design decision 8).
///
/// - `current`: the account id the session is on (empty when unknown).
/// - `include_current`: when `true`, return `Ok(None)` if the winner is
///   `current` (no-op switch).
/// - `apply_stale_gate`: `true` refuses to score on stale data; the reactive
///   hook passes `false` because it must move off an already-limited
///   account even on stale numbers. See [`scoring::pick_best`] for the gate.
///
/// `Ok(Some(id))` names the account; `Err(_)` means none is viable or the
/// fetch failed.
pub fn pick_account_gated(
    current: &str,
    include_current: bool,
    apply_stale_gate: bool,
) -> ScoringResult {
    let data: UsageData = usage::fetch_cached().map_err(ScoringError::FetchFailed)?;
    scoring::pick_best_gated(&data, current, include_current, apply_stale_gate)
}
