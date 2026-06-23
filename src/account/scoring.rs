//! Account scoring: choose the best profile to launch under.
//!
//! Logic (spec §2 Account pick + scoring, shell source pick_account ~lines 883–971):
//!
//! 1. Build candidate rows: profiles NOT in errors{}, with a numeric week_all.pct.
//! 2. Exclusions (in order, per shell lines 947–949):
//!    - `session.pct >= LIMIT_PCT(99)` → skip (absent session.pct = -1, never fires).
//!    - `week_all.pct >= SATURATION_PCT(95)` → skip.
//! 3. Among the survivors, choose the one with the HIGHEST week_all.pct (shell lines
//!    955–961: `pct > best_pct` wins; `pct == best_pct` → soonest reset epoch wins
//!    — a known epoch beats unknown, a smaller epoch beats a larger one).
//! 4. `include_current = false` (reactive / hook): skip the current profile entirely
//!    (shell line 942–944).
//! 5. `include_current = true` (proactive / fresh csm): current competes; if the
//!    winner is current → return `Ok(None)` so the caller keeps it with no switch
//!    (shell lines 967–969).
//! 6. No viable candidate → `Err(ScoringError::AllSaturated)`.
//!
//! Env overrides: `CLAUDE_LIMIT_PCT` / `CLAUDE_PICK_SATURATION_PCT` (spec §2).

use std::collections::HashMap;
use std::env;

use crate::account::reset::resets_to_epoch;
use crate::usage::{FetchError, UsageData};

// ─── constants ────────────────────────────────────────────────────────────────

/// The session usage percentage at which a profile is excluded as session-limited.
/// Override via `CLAUDE_LIMIT_PCT`.
pub const LIMIT_PCT: i64 = 99;

/// The week_all usage percentage at which a profile is considered saturated.
/// Override via `CLAUDE_PICK_SATURATION_PCT`.
pub const SATURATION_PCT: i64 = 95;

/// Sentinel value meaning "session pct absent" — intentionally chosen to be
/// negative so it never triggers the `>= LIMIT_PCT` gate.
pub const ABSENT_SESSION_PCT: i64 = -1;

// ─── helpers ─────────────────────────────────────────────────────────────────

/// Read `CLAUDE_LIMIT_PCT` from the environment, falling back to [`LIMIT_PCT`].
fn limit_pct() -> i64 {
    env::var("CLAUDE_LIMIT_PCT")
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .unwrap_or(LIMIT_PCT)
}

/// Read `CLAUDE_PICK_SATURATION_PCT` from the environment, falling back to
/// [`SATURATION_PCT`].
fn saturation_pct() -> i64 {
    env::var("CLAUDE_PICK_SATURATION_PCT")
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .unwrap_or(SATURATION_PCT)
}

// ─── public error + result types ─────────────────────────────────────────────

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

// ─── scoring core ─────────────────────────────────────────────────────────────

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
///
/// # Shell source
/// `pick_account` in `claude-smart-helper.sh.j2` lines 883–971.
pub fn pick_best(data: &UsageData, current_profile: &str, include_current: bool) -> ScoringResult {
    let lim = limit_pct();
    let sat = saturation_pct();

    // Candidates: profiles present in data.profiles, NOT in errors{}.
    // Build (name, week_all_pct, session_pct, resets_str) tuples.
    // Shell lines 924–934: jq emits profile name, week_all.pct, session.pct(-1 absent),
    // week_all.resets.
    // Borrow the errors map, or use an empty sentinel for the "no errors" case.
    let empty_errors: HashMap<String, String> = HashMap::new();
    let errors: &HashMap<String, String> = data
        .errors
        .as_ref()
        .map(|m| m as &HashMap<String, String>)
        .unwrap_or(&empty_errors);

    // We keep this borrowed ref around for the loop.
    struct Candidate<'a> {
        name: &'a str,
        week_pct: i64,
        session_pct: i64,
        resets: Option<&'a str>,
    }

    let mut candidates: Vec<Candidate<'_>> = data
        .profiles
        .iter()
        .filter_map(|(name, pu)| {
            // skip errored profiles (shell line 928: select(($e[.key] // null) == null))
            if errors.contains_key(name.as_str()) {
                return None;
            }
            // skip profiles with no week_all section (shell line 929:
            // select((.value.week_all.pct // null) != null))
            let week_pct = pu.week_all.as_ref()?.pct;
            let session_pct = pu
                .session
                .as_ref()
                .map(|s| s.pct)
                .unwrap_or(ABSENT_SESSION_PCT);
            let resets = pu.week_all.as_ref().and_then(|wa| wa.resets.as_deref());
            Some(Candidate {
                name: name.as_str(),
                week_pct,
                session_pct,
                resets,
            })
        })
        .collect();

    // Shell lines 939–962: iterate rows, apply exclusion gates, track best.
    let mut best_name: Option<&str> = None;
    let mut best_pct: i64 = i64::MIN;
    let mut best_epoch: Option<i64> = None;

    // Sort by name for deterministic tie-break behavior in tests (HashMap order is
    // non-deterministic; the shell source reads jq output which may also vary).
    // The tie-break logic is epoch-based (from the data), so stable naming order
    // ensures tests with equal pcts and equal/absent epochs are reproducible.
    candidates.sort_by(|a, b| a.name.cmp(b.name));

    for c in &candidates {
        // Reactive (hook) mode: never target the current profile (shell lines 941–943).
        if !include_current && !current_profile.is_empty() && c.name == current_profile {
            continue;
        }

        // Session-limit gate: session.pct >= LIMIT_PCT → skip (shell line 947).
        // -1 (absent) is < 99 so it intentionally passes.
        if c.session_pct >= lim {
            continue;
        }

        // Saturation gate: week_all.pct >= SATURATION_PCT → skip (shell line 949).
        if c.week_pct >= sat {
            continue;
        }

        // Compute reset epoch for tie-breaking (shell line 950).
        // resets_to_epoch failures are treated as "unknown" (None), matching shell
        // behavior where `resets_to_epoch` prints nothing on parse failure.
        let epoch: Option<i64> = c
            .resets
            .and_then(|r| resets_to_epoch(r).ok())
            .map(|dt| dt.timestamp());

        // First viable candidate (shell lines 951–953).
        if best_name.is_none() {
            best_name = Some(c.name);
            best_pct = c.week_pct;
            best_epoch = epoch;
            continue;
        }

        // Higher pct wins (shell lines 955–956).
        if c.week_pct > best_pct {
            best_name = Some(c.name);
            best_pct = c.week_pct;
            best_epoch = epoch;
        } else if c.week_pct == best_pct {
            // Equal pct → soonest reset epoch wins (shell lines 957–960).
            // A known epoch beats unknown; a smaller epoch (sooner) beats a larger.
            let new_wins = match (epoch, best_epoch) {
                (Some(_), None) => true,       // known beats unknown
                (Some(e), Some(be)) => e < be, // smaller (sooner) wins
                _ => false,                    // unknown doesn't beat known or equal unknown
            };
            if new_wins {
                best_name = Some(c.name);
                best_pct = c.week_pct;
                best_epoch = epoch;
            }
        }
    }

    match best_name {
        None => Err(ScoringError::AllSaturated),
        Some(name) => {
            // include_current=true: winner == current → no-op (shell lines 967–969).
            if include_current && !current_profile.is_empty() && name == current_profile {
                Ok(None)
            } else {
                Ok(Some(name.to_owned()))
            }
        }
    }
}

/// Return the `(session_pct, week_all_pct)` for a given profile from `data`,
/// or `None` when the profile is errored, absent, or has no week_all section.
///
/// Absent `session.pct` is encoded as [`ABSENT_SESSION_PCT`] (-1) (spec §2,
/// shell `current-usage` lines 730–753).
///
/// Test-isolation helper: the production scorer inlines `data.current_usage`;
/// this named wrapper lets unit tests assert the §2 encoding independently.
#[allow(dead_code)]
pub fn current_usage_pcts(data: &UsageData, profile: &str) -> Option<(i64, i64)> {
    data.current_usage(profile)
}

/// Helper: is a profile excluded from candidacy?
///
/// Returns `true` when the profile should be skipped in scoring:
/// - present in `errors` map,
/// - `session.pct >= LIMIT_PCT` (env-overridable), or
/// - `week_all.pct >= SATURATION_PCT` (env-overridable).
///
/// Used by tests to assert individual exclusion conditions independently.
/// Test-isolation helper: the production scorer inlines the same three checks.
#[allow(dead_code)]
pub fn is_excluded(data: &UsageData, profile: &str) -> bool {
    let lim = limit_pct();
    let sat = saturation_pct();

    if let Some(errors) = &data.errors {
        if errors.contains_key(profile) {
            return true;
        }
    }

    if let Some(pu) = data.profiles.get(profile) {
        let session_pct = pu
            .session
            .as_ref()
            .map(|s| s.pct)
            .unwrap_or(ABSENT_SESSION_PCT);
        if session_pct >= lim {
            return true;
        }
        if let Some(wa) = &pu.week_all {
            if wa.pct >= sat {
                return true;
            }
        }
    }

    false
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crate::usage::model::{ProfileUsage, UsageData, UsageSection};

    use super::*;

    // ─── fixture builders ─────────────────────────────────────────────────────

    fn make_section(pct: i64, resets: Option<&str>) -> UsageSection {
        UsageSection {
            pct,
            resets: resets.map(String::from),
        }
    }

    fn make_profile(session_pct: Option<i64>, week_pct: i64, resets: Option<&str>) -> ProfileUsage {
        ProfileUsage {
            captured_at: None,
            session: session_pct.map(|p| make_section(p, None)),
            week_all: Some(make_section(week_pct, resets)),
            week_sonnet: None,
            session_stats: vec![],
        }
    }

    fn make_data(profiles: HashMap<String, ProfileUsage>) -> UsageData {
        UsageData {
            captured_at: None,
            profiles,
            errors: None,
        }
    }

    fn make_data_with_errors(
        profiles: HashMap<String, ProfileUsage>,
        errors: HashMap<String, String>,
    ) -> UsageData {
        UsageData {
            captured_at: None,
            profiles,
            errors: Some(errors),
        }
    }

    // ─── constants sanity ─────────────────────────────────────────────────────

    #[test]
    fn constants_are_sane() {
        const {
            assert!(
                LIMIT_PCT > SATURATION_PCT,
                "LIMIT_PCT must be > SATURATION_PCT"
            )
        };
        const { assert!(ABSENT_SESSION_PCT < 0, "absent sentinel must be negative") };
    }

    // ─── basic pick ──────────────────────────────────────────────────────────

    /// Shell behavior: among two healthy profiles, pick the one with the
    /// HIGHER week_all.pct (drain the account nearest its ceiling first).
    #[test]
    fn one_saturated_one_healthy_picks_healthy() {
        let mut profiles = HashMap::new();
        // "saturated" has week_pct = 96 (>= SATURATION_PCT=95) → excluded
        profiles.insert("saturated".to_string(), make_profile(Some(10), 96, None));
        // "healthy" has week_pct = 60 → viable
        profiles.insert("healthy".to_string(), make_profile(Some(5), 60, None));
        let data = make_data(profiles);
        let result = pick_best(&data, "other", false).unwrap();
        assert_eq!(result.as_deref(), Some("healthy"));
    }

    /// Higher week_pct profile is picked even when both are healthy.
    #[test]
    fn picks_highest_week_pct() {
        let mut profiles = HashMap::new();
        // "low" at 30%, "high" at 70%
        profiles.insert("low".to_string(), make_profile(Some(5), 30, None));
        profiles.insert("high".to_string(), make_profile(Some(5), 70, None));
        let data = make_data(profiles);
        let result = pick_best(&data, "", false).unwrap();
        assert_eq!(result.as_deref(), Some("high"));
    }

    // ─── tie-break by reset epoch ─────────────────────────────────────────────

    /// Equal week_pct, both with no resets string → both epochs are None → tie
    /// is broken by alphabetical candidate order (first alphabetically wins).
    /// This validates the tie-break code path without calling resets_to_epoch.
    #[test]
    fn tiebreak_no_resets_alphabetical_first_wins() {
        let mut profiles = HashMap::new();
        // Both at 50% with no resets string → epoch = None for both.
        // Candidates are sorted alphabetically so "alpha" is first, wins by
        // virtue of being first to set best_name when both epochs are None.
        profiles.insert("alpha".to_string(), make_profile(Some(5), 50, None));
        profiles.insert("beta".to_string(), make_profile(Some(5), 50, None));
        let data = make_data(profiles);
        let result = pick_best(&data, "", false).unwrap();
        assert!(result.is_some(), "tie-break must return Some");
        // "alpha" is first alphabetically; "beta" has no known epoch advantage
        // (both None → new_wins=false → alpha keeps best).
        assert_eq!(
            result.as_deref(),
            Some("alpha"),
            "with equal pct and no epochs, first alphabetical candidate wins"
        );
    }

    /// Equal week_pct, "early" has no resets (epoch=None), "zeta" has no
    /// resets either.  With equal pcts and both epochs unknown, first
    /// alphabetical wins.
    #[test]
    fn tiebreak_equal_pct_and_no_epoch_first_alphabetical_wins() {
        let mut profiles = HashMap::new();
        profiles.insert("early".to_string(), make_profile(Some(5), 50, None));
        profiles.insert("zeta".to_string(), make_profile(Some(5), 50, None));
        let data = make_data(profiles);
        let result = pick_best(&data, "", false).unwrap();
        // "early" < "zeta" alphabetically → "early" is first, wins the tie.
        assert_eq!(result.as_deref(), Some("early"));
    }

    // ─── include_current flag ─────────────────────────────────────────────────

    /// include_current=true: if winner IS current → return Ok(None) (no-op switch).
    #[test]
    fn include_current_no_op_when_winner_is_current() {
        let mut profiles = HashMap::new();
        // "current" is the only healthy profile — it should win but trigger the no-op.
        profiles.insert("current".to_string(), make_profile(Some(10), 70, None));
        let data = make_data(profiles);
        let result = pick_best(&data, "current", true).unwrap();
        assert_eq!(
            result, None,
            "winner == current with include_current=true must be None"
        );
    }

    /// include_current=true: if winner is NOT current, return that winner.
    #[test]
    fn include_current_returns_better_profile() {
        let mut profiles = HashMap::new();
        profiles.insert("current".to_string(), make_profile(Some(10), 30, None));
        profiles.insert("better".to_string(), make_profile(Some(5), 70, None));
        let data = make_data(profiles);
        let result = pick_best(&data, "current", true).unwrap();
        assert_eq!(result.as_deref(), Some("better"));
    }

    /// include_current=true with the current profile as the ONLY candidate, but
    /// saturated: the saturation gate drops it, leaving zero candidates, so the
    /// result is AllSaturated — NOT Ok(None). The distinction matters: Ok(None)
    /// means "stay, you're already best"; AllSaturated means "nothing viable,
    /// caller must warn". A saturated sole-current must take the second path so
    /// proactive launch surfaces the saturation rather than silently proceeding
    /// as if the current profile were a healthy pick.
    #[test]
    fn include_current_sole_saturated_current_is_all_saturated_not_noop() {
        let mut profiles = HashMap::new();
        profiles.insert("current".to_string(), make_profile(Some(10), 97, None)); // >= SATURATION_PCT
        let data = make_data(profiles);
        let err = pick_best(&data, "current", true).unwrap_err();
        assert!(
            matches!(err, ScoringError::AllSaturated),
            "a saturated sole current must be AllSaturated, not a no-op Ok(None)"
        );
    }

    /// include_current=false: exclude current from candidates.
    #[test]
    fn exclude_current_in_reactive_mode() {
        let mut profiles = HashMap::new();
        // "current" has the highest pct but must be excluded.
        profiles.insert("current".to_string(), make_profile(Some(10), 80, None));
        profiles.insert("alt".to_string(), make_profile(Some(5), 40, None));
        let data = make_data(profiles);
        let result = pick_best(&data, "current", false).unwrap();
        assert_eq!(
            result.as_deref(),
            Some("alt"),
            "reactive mode must not return current"
        );
    }

    // ─── all-saturated ────────────────────────────────────────────────────────

    /// When all profiles are saturated (week_pct >= SATURATION_PCT), return AllSaturated.
    #[test]
    fn all_saturated_returns_error() {
        let mut profiles = HashMap::new();
        profiles.insert("p1".to_string(), make_profile(Some(10), 95, None)); // exactly SATURATION_PCT
        profiles.insert("p2".to_string(), make_profile(Some(10), 98, None));
        let data = make_data(profiles);
        let err = pick_best(&data, "other", false).unwrap_err();
        assert!(
            matches!(err, ScoringError::AllSaturated),
            "all saturated must return AllSaturated"
        );
    }

    /// Empty profiles → AllSaturated.
    #[test]
    fn empty_profiles_is_all_saturated() {
        let data = make_data(HashMap::new());
        let err = pick_best(&data, "", false).unwrap_err();
        assert!(matches!(err, ScoringError::AllSaturated));
    }

    // ─── errored profiles excluded ────────────────────────────────────────────

    /// Profiles in the errors map must never be candidates.
    #[test]
    fn errored_profile_excluded() {
        let mut profiles = HashMap::new();
        // "errored" has a healthy week_pct but is in the errors map → must be excluded.
        profiles.insert("errored".to_string(), make_profile(Some(5), 80, None));
        profiles.insert("healthy".to_string(), make_profile(Some(5), 50, None));
        let mut errors = HashMap::new();
        errors.insert(
            "errored".to_string(),
            "HTTP 401: no credentials".to_string(),
        );
        let data = make_data_with_errors(profiles, errors);
        let result = pick_best(&data, "", false).unwrap();
        assert_eq!(
            result.as_deref(),
            Some("healthy"),
            "errored profile must be excluded"
        );
    }

    /// All profiles errored → AllSaturated.
    #[test]
    fn all_errored_is_all_saturated() {
        let mut profiles = HashMap::new();
        profiles.insert("p1".to_string(), make_profile(Some(5), 50, None));
        let mut errors = HashMap::new();
        errors.insert("p1".to_string(), "error".to_string());
        let data = make_data_with_errors(profiles, errors);
        let err = pick_best(&data, "", false).unwrap_err();
        assert!(matches!(err, ScoringError::AllSaturated));
    }

    // ─── session-limit gate ───────────────────────────────────────────────────

    /// A profile with session.pct >= LIMIT_PCT must be excluded even if its
    /// week_all is healthy.
    #[test]
    fn session_limited_excluded() {
        let mut profiles = HashMap::new();
        // "session_hit" has session_pct=99 (== LIMIT_PCT) → excluded
        profiles.insert("session_hit".to_string(), make_profile(Some(99), 20, None));
        profiles.insert("healthy".to_string(), make_profile(Some(5), 10, None));
        let data = make_data(profiles);
        let result = pick_best(&data, "other", false).unwrap();
        assert_eq!(result.as_deref(), Some("healthy"));
    }

    /// A profile with absent session.pct (encoded as -1) must NOT be excluded
    /// by the session gate (shell comment: "Unknown session pct... encoded as -1
    /// and never excludes — only a POSITIVE limit reading disqualifies").
    #[test]
    fn absent_session_pct_not_excluded() {
        let mut profiles = HashMap::new();
        // session=None → ABSENT_SESSION_PCT (-1) → must not be excluded
        profiles.insert("no_session".to_string(), make_profile(None, 50, None));
        let data = make_data(profiles);
        let result = pick_best(&data, "", false).unwrap();
        assert_eq!(result.as_deref(), Some("no_session"));
    }

    // ─── is_excluded helper ───────────────────────────────────────────────────

    #[test]
    fn is_excluded_for_error_profile() {
        let mut profiles = HashMap::new();
        profiles.insert("p".to_string(), make_profile(Some(5), 50, None));
        let mut errors = HashMap::new();
        errors.insert("p".to_string(), "err".to_string());
        let data = make_data_with_errors(profiles, errors);
        assert!(is_excluded(&data, "p"));
    }

    #[test]
    fn is_excluded_for_session_limited() {
        let mut profiles = HashMap::new();
        profiles.insert("p".to_string(), make_profile(Some(LIMIT_PCT), 50, None));
        let data = make_data(profiles);
        assert!(is_excluded(&data, "p"));
    }

    #[test]
    fn is_excluded_for_saturated() {
        let mut profiles = HashMap::new();
        profiles.insert("p".to_string(), make_profile(Some(5), SATURATION_PCT, None));
        let data = make_data(profiles);
        assert!(is_excluded(&data, "p"));
    }

    #[test]
    fn is_excluded_healthy_is_false() {
        let mut profiles = HashMap::new();
        profiles.insert("p".to_string(), make_profile(Some(5), 50, None));
        let data = make_data(profiles);
        assert!(!is_excluded(&data, "p"));
    }

    // ─── current_usage_pcts ───────────────────────────────────────────────────

    #[test]
    fn current_usage_pcts_present_profile() {
        let mut profiles = HashMap::new();
        profiles.insert("p".to_string(), make_profile(Some(42), 31, None));
        let data = make_data(profiles);
        let (sess, week) = current_usage_pcts(&data, "p").unwrap();
        assert_eq!(sess, 42);
        assert_eq!(week, 31);
    }

    #[test]
    fn current_usage_pcts_absent_session_encodes_minus_one() {
        let mut profiles = HashMap::new();
        profiles.insert("p".to_string(), make_profile(None, 55, None));
        let data = make_data(profiles);
        let (sess, week) = current_usage_pcts(&data, "p").unwrap();
        assert_eq!(sess, ABSENT_SESSION_PCT);
        assert_eq!(week, 55);
    }

    #[test]
    fn current_usage_pcts_errored_is_none() {
        let mut profiles = HashMap::new();
        profiles.insert("p".to_string(), make_profile(Some(5), 50, None));
        let mut errors = HashMap::new();
        errors.insert("p".to_string(), "err".to_string());
        let data = make_data_with_errors(profiles, errors);
        assert!(current_usage_pcts(&data, "p").is_none());
    }

    #[test]
    fn current_usage_pcts_absent_is_none() {
        let data = make_data(HashMap::new());
        assert!(current_usage_pcts(&data, "nonexistent").is_none());
    }

    // ─── no_week_all_section ──────────────────────────────────────────────────

    /// A profile with no week_all section must be excluded from candidacy
    /// (shell line 929: select((.value.week_all.pct // null) != null)).
    #[test]
    fn profile_without_week_all_is_excluded() {
        let mut profiles = HashMap::new();
        // Profile with no week_all section
        let pu = ProfileUsage {
            captured_at: None,
            session: Some(make_section(5, None)),
            week_all: None,
            week_sonnet: None,
            session_stats: vec![],
        };
        profiles.insert("no_week_all".to_string(), pu);
        profiles.insert("has_week_all".to_string(), make_profile(Some(5), 40, None));
        let data = make_data(profiles);
        let result = pick_best(&data, "", false).unwrap();
        assert_eq!(result.as_deref(), Some("has_week_all"));
    }

    // ─── reactive-mode empty current ─────────────────────────────────────────

    /// include_current=false with empty current string must not filter anything.
    #[test]
    fn reactive_mode_empty_current_includes_all() {
        let mut profiles = HashMap::new();
        profiles.insert("p".to_string(), make_profile(Some(5), 60, None));
        let data = make_data(profiles);
        let result = pick_best(&data, "", false).unwrap();
        assert_eq!(result.as_deref(), Some("p"));
    }

    // ─── single profile include_current=false, current != profile ────────────

    #[test]
    fn reactive_mode_picks_non_current_profile() {
        let mut profiles = HashMap::new();
        profiles.insert("alt".to_string(), make_profile(Some(10), 50, None));
        let data = make_data(profiles);
        // current is "main" but only "alt" exists; alt must win
        let result = pick_best(&data, "main", false).unwrap();
        assert_eq!(result.as_deref(), Some("alt"));
    }
}
