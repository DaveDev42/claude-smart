//! Account scoring: choose the best profile to launch under.
//!
//! Logic (account pick + scoring):
//!
//! 1. Build candidate rows: profiles NOT in errors{}, with a numeric week_all.pct.
//! 2. Exclusions (in order) — see [`is_viable_pcts`], the SINGLE viability
//!    authority every caller (this module's own ranking, the stale-usage
//!    picker's `cmd::run::account_row_rank` — see [`crate::picker::account`])
//!    routes through:
//!    - `session.pct >= LIMIT_PCT(99)` → skip (absent session.pct = -1, never fires).
//!    - `week_all.pct >= SATURATION_PCT(95)` → skip.
//!    - `week_fable.pct` (the model-scoped weekly cap, whichever tier the API
//!      reports under `kind=weekly_scoped`, labelled by `week_model_label`)
//!      does NOT constrain viability, however high it reads — an account
//!      whose only exhausted window is the model-scoped weekly cap is still
//!      fully usable on another model. That case is handled by the
//!      Stop hook falling back to the latest Opus on the SAME account instead
//!      of excluding it from picking (see `src/hook/detect.rs`'s
//!      `fable_fallback_model`); this predicate never sees `week_fable` at
//!      all. (Behaviour change from the earlier rule, where a saturated
//!      `week_fable` excluded the profile exactly like `week_all`.)
//! 3. Among the survivors, choose the one whose weekly reset returns SOONEST:
//!    a known reset epoch beats an unknown one, and a smaller (sooner) epoch
//!    beats a larger one. The epoch compared is the LATER of `week_all`'s and
//!    `week_fable`'s reset (when both are known) — a viable candidate is, by
//!    construction, under neither cap, but it is only genuinely "fresh" again
//!    once BOTH weekly windows have rolled over, so the binding constraint on
//!    ranking is whichever of the two resets later. Ties (equal epoch, or all
//!    epochs unknown) break to the HIGHER week_all.pct, then to the first
//!    candidate in name order. Rationale: budget spent on the account that
//!    refills first is the cheapest budget — drain that account, keep the
//!    later-resetting ones in reserve.
//!    (Policy changed post-0.2.11: the retired shell source's `pick_account`
//!    drained highest-pct-first with a soonest-reset tie-break; the primary
//!    and secondary keys are now swapped.)
//! 4. `include_current = false` (reactive / hook): skip the current profile entirely.
//! 5. `include_current = true` (proactive / fresh csm): current competes; if the
//!    winner is current → return `Ok(None)` so the caller keeps it with no switch.
//! 6. No viable candidate → `Err(ScoringError::AllSaturated)`.
//!
//! Env overrides: `CLAUDE_LIMIT_PCT` (session) / `CLAUDE_PICK_SATURATION_PCT`
//! (week_all). Neither applies to `week_fable` here any more — see item 2
//! above. `week_fable`'s own limit-DETECTION threshold (whether a reading
//! counts as "capped" in the first place, as opposed to whether a capped
//! reading excludes the profile from picking) is a separate check in
//! `src/hook/detect.rs`, still `CLAUDE_LIMIT_PCT`, unchanged by this commit.

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use crate::usage::model::UsageSection;
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
    crate::envvar::i64_or("CLAUDE_LIMIT_PCT", LIMIT_PCT)
}

/// Read `CLAUDE_PICK_SATURATION_PCT` from the environment, falling back to
/// [`SATURATION_PCT`].
fn saturation_pct() -> i64 {
    crate::envvar::i64_or("CLAUDE_PICK_SATURATION_PCT", SATURATION_PCT)
}

/// Default max age, in seconds, of the usage data that account auto-pick will
/// still trust. Each profile is captured locally on this machine — a live
/// OAuth API probe (per-profile TTL `CSM_USAGE_PROFILE_TTL`, default 300s) or
/// a statusline-stdin merge — so data this old means local collection itself
/// has stalled for a while (no successful probe, and no statusline capture
/// either) across the whole registry, not just one profile. Data older than
/// this means the percentages no longer reflect reality and auto-picking on
/// them can route into an account that is actually over its limit. Override
/// via `CLAUDE_USAGE_MAX_AGE` (alias `CSM_USAGE_MAX_AGE_SECS`). `0` disables
/// the gate entirely (trust any age).
pub const USAGE_MAX_AGE_SECS: u64 = 1800;

/// Read the usage max-age gate (seconds) from the environment.
///
/// `CLAUDE_USAGE_MAX_AGE` wins; `CSM_USAGE_MAX_AGE_SECS` is the namespaced
/// alias (mirrors the `CLAUDE_USAGE_TTL` / `CSM_USAGE_TTL_SECS` pair in the
/// transport layer). An unparseable `CLAUDE_USAGE_MAX_AGE` now falls through
/// to a valid `CSM_USAGE_MAX_AGE_SECS` rather than straight to the default
/// (see [`crate::envvar::u64_with_alias`] — this used to disagree with the
/// transport layer's more forgiving rule; it no longer does). Unparseable /
/// unset both → [`USAGE_MAX_AGE_SECS`]. `0` = gate off.
fn usage_max_age_secs() -> u64 {
    crate::envvar::u64_with_alias(
        "CLAUDE_USAGE_MAX_AGE",
        "CSM_USAGE_MAX_AGE_SECS",
        USAGE_MAX_AGE_SECS,
    )
}

/// The freshest `captured_at` instant in `data`: the top-level field if
/// present, else the newest per-profile `captured_at`. `None` when no timestamp
/// anywhere parses (old cache files predate the field, or a `CSM_USAGE_CMD`
/// source omits it). RFC-3339 / ISO-8601 with a `Z` or offset (e.g.
/// `2026-06-17T07:13:19Z`).
fn newest_captured_at(data: &UsageData) -> Option<DateTime<Utc>> {
    fn parse(s: &str) -> Option<DateTime<Utc>> {
        DateTime::parse_from_rfc3339(s.trim())
            .ok()
            .map(|dt| dt.with_timezone(&Utc))
    }
    let mut newest: Option<DateTime<Utc>> = data.captured_at.as_deref().and_then(parse);
    for pu in data.profiles.values() {
        if let Some(ts) = pu.captured_at.as_deref().and_then(parse) {
            newest = Some(match newest {
                Some(cur) if cur >= ts => cur,
                _ => ts,
            });
        }
    }
    newest
}

/// Decide whether `data` is too stale to auto-pick on, relative to `now`.
///
/// **fail-open**: returns `false` (trust the data) when the gate is disabled
/// (`max_age == 0`) OR no `captured_at` parses — we never *block* on an unknown
/// age, only on a known-and-too-old one. This preserves the pre-gate behaviour
/// for cache files / sources that carry no timestamp, while a collector whose
/// data froze (its `captured_at` stops advancing) is correctly caught.
fn data_too_stale_at(data: &UsageData, max_age_secs: u64, now: DateTime<Utc>) -> bool {
    if max_age_secs == 0 {
        return false; // gate disabled
    }
    let Some(captured) = newest_captured_at(data) else {
        return false; // unknown age → fail-open
    };
    let age = now.signed_duration_since(captured);
    // Negative age (captured_at in the future — clock skew) is not "stale".
    age.num_seconds() > max_age_secs as i64
}

// ─── the single viability authority ────────────────────────────────────────

/// The ONE viability predicate: `true` when a profile with these three raw
/// percentages is a legitimate pick candidate.
///
/// This is the single authority for "is this profile viable" — [`pick_best_with`]
/// (below) and `cmd::run::account_row_rank` (the stale-usage picker rows)
/// both route through it rather than each hand-rolling the same three checks,
/// so a profile whose model-scoped weekly cap is exhausted is skipped
/// everywhere a pick or a recommendation is made, not just in one of the two
/// code paths.
///
/// - `session_pct >= LIMIT_PCT` → not viable. Callers encode "no session
///   data" as [`ABSENT_SESSION_PCT`] (-1), which is always `< LIMIT_PCT` and
///   so never trips this gate.
/// - `week_all_pct` present and `>= SATURATION_PCT` → not viable.
/// - `week_fable_pct` (the model-scoped weekly cap) never constrains
///   viability, however high it reads. **User-visible behaviour change:**
///   this predicate used to treat a saturated `week_fable_pct` exactly
///   like `week_all_pct` and exclude the profile; the user chose to keep
///   such an account pickable instead,
///   since a model-scoped-only cap still leaves the account fully usable on
///   another model — the Stop hook now handles that case with a same-account
///   model fallback instead of excluding the profile (see
///   `src/hook/detect.rs`'s `fable_fallback_model`). The parameter stays in
///   this function's signature so every existing call site keeps compiling
///   unchanged; it is simply never read.
///
/// The session/week_all threshold is read from the environment on every call
/// (`limit_pct()` / `saturation_pct()`), so a
/// `CLAUDE_LIMIT_PCT`/`CLAUDE_PICK_SATURATION_PCT` override set mid-process
/// (as the tests do) is honoured immediately — no caller needs to re-read the
/// env itself.
pub fn is_viable_pcts(
    session_pct: i64,
    week_all_pct: Option<i64>,
    _week_fable_pct: Option<i64>,
) -> bool {
    let lim = limit_pct();
    let sat = saturation_pct();

    if session_pct >= lim {
        return false;
    }
    if let Some(w) = week_all_pct
        && w >= sat
    {
        return false;
    }
    true
}

/// The binding weekly reset epoch: the LATER of `week_all`'s and `week_fable`'s
/// (a viable account is under neither cap but is only fully fresh once BOTH
/// windows roll over). `i64::MAX` when neither is known, so a known reset
/// always beats an unknown one.
///
/// Shared by [`pick_best_with`]'s ranking and `cmd::run::account_row_rank` (the
/// stale-usage picker's rank), which resolve their `week_all`/`week_fable`
/// epochs from different sources (`UsageSection::reset_instant` vs. the
/// picker's cached `resets_at`/`resets` fields) but must apply the same
/// later-of rule once resolved.
pub fn effective_reset_epoch(week_all: Option<i64>, week_fable: Option<i64>) -> i64 {
    match (week_all, week_fable) {
        (Some(a), Some(f)) => a.max(f),
        (Some(a), None) => a,
        (None, Some(f)) => f,
        (None, None) => i64::MAX,
    }
}

// ─── public error + result types ─────────────────────────────────────────────

/// Errors that `pick_best` can return.
#[derive(Debug, thiserror::Error)]
pub enum ScoringError {
    /// All profiles are either saturated, session-limited, or errored.
    /// Caller should warn and proceed on the current profile.
    #[error("all profiles are saturated or at session limit")]
    AllSaturated,

    /// No profile carried any usable usage data — every profile was errored or
    /// had no `week_all` section (the fetch returned an empty/degenerate blob,
    /// not a confident "all limited" verdict). Distinct from [`AllSaturated`],
    /// where we *did* read real percentages and they were all over the line.
    ///
    /// "We couldn't tell" must not be silently treated as "stay put": the caller
    /// opens the interactive picker so the user chooses deliberately, exactly as
    /// it does for [`FetchFailed`].
    #[error("no usable usage data for any profile")]
    NoUsableData,

    /// The usage fetch failed (unreachable / negative-cache cooldown).
    /// Caller should open the stale-usage interactive picker.
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
/// # Staleness gate
/// When `apply_stale_gate` is `true`, the data's freshness is checked against
/// the usage max-age (`usage_max_age_secs`) BEFORE scoring. If the newest
/// `captured_at` is older than the gate, the percentages are no longer
/// trustworthy and we return [`ScoringError::NoUsableData`] WITHOUT scoring —
/// exactly as if no profile had usable data. The caller then opens the
/// interactive picker (or, in a non-interactive context, fail-safes to the
/// current profile), rather than auto-routing into an account whose real usage
/// we cannot see. The gate fail-opens on a missing/unparseable timestamp (see
/// `data_too_stale_at`).
///
/// Proactive launch and the explicit `csm pick-account` CLI pass `true` (they
/// have a picker fallback, so "we can't tell → ask the user" is correct). The
/// reactive **hook** passes `false`: it fires only because the current profile
/// already hit a limit, and the hook is non-interactive (no picker). Refusing
/// to score on stale data there would strand the user ON the limited profile;
/// leaving for the freshest-known best, even on slightly stale numbers, is the
/// safer choice. See [`pick_best`] / [`pick_best_with`].
///
/// # Ranking
/// Soonest known weekly reset first; ties break to higher week_all.pct, then
/// name order (see the module doc — this diverges from the retired shell
/// source, which ranked highest-pct-first).
///
/// Production callers go through [`pick_account`](crate::account::pick_account)
/// → [`pick_best_with`]; this gate-on convenience wrapper exists for the
/// scoring tests only.
#[cfg(test)]
pub fn pick_best(data: &UsageData, current_profile: &str, include_current: bool) -> ScoringResult {
    pick_best_at(data, current_profile, include_current, true, Utc::now())
}

/// `now`-injected core of [`pick_best`], for deterministic staleness tests.
/// Mirrors the `resets_to_epoch` / `resets_to_epoch_at` split in `reset.rs`.
///
/// `apply_stale_gate` toggles the freshness gate (see [`pick_best`] docs).
/// The pre-Orca policy: no current preference, no excluded names. See
/// [`pick_best_with`] for the full policy. Test-only: production callers go
/// through [`pick_best_with`].
#[cfg(test)]
pub fn pick_best_at(
    data: &UsageData,
    current_profile: &str,
    include_current: bool,
    apply_stale_gate: bool,
    now: DateTime<Utc>,
) -> ScoringResult {
    pick_best_with(
        data,
        current_profile,
        &PickPolicy {
            include_current,
            apply_stale_gate,
            prefer_current: false,
            exclude: &[],
        },
        now,
    )
}

/// How [`pick_best_with`] treats the current profile and which names it
/// never considers.
#[derive(Debug, Clone, Copy, Default)]
pub struct PickPolicy<'a> {
    /// Whether the current profile competes as a candidate (see [`pick_best_with`]).
    pub include_current: bool,
    /// Whether stale usage data blocks the pick (see [`pick_best_with`]).
    pub apply_stale_gate: bool,
    /// With `include_current`: when the current profile is itself a viable
    /// candidate, stay on it (`Ok(None)`) instead of ranking. Used when the
    /// current account came from Orca's live selection, so a launch does not
    /// hop off an account the user just picked in Orca while it still has
    /// headroom. The staleness gate still runs first.
    pub prefer_current: bool,
    /// Profile names that are never candidates (the Orca slot, which is not
    /// an account of its own, plus any caller-specific exclusions).
    pub exclude: &'a [&'a str],
}

/// Full-policy scoring core. `pick_best_at` (test-only) is this with the pre-Orca
/// policy; the viability predicate is still [`is_viable_pcts`] alone.
pub fn pick_best_with(
    data: &UsageData,
    current_profile: &str,
    policy: &PickPolicy<'_>,
    now: DateTime<Utc>,
) -> ScoringResult {
    let PickPolicy {
        include_current,
        apply_stale_gate,
        prefer_current,
        exclude,
    } = *policy;
    // Staleness gate (spec: proactive auto-pick must not fly on stale usage). A
    // frozen data source keeps serving the same `captured_at`; once that ages
    // past the gate we refuse to score and let the caller fall back to the
    // picker/current. The reactive hook opts OUT (apply_stale_gate=false): it
    // must move off an already-limited profile even on stale numbers.
    if apply_stale_gate && data_too_stale_at(data, usage_max_age_secs(), now) {
        return Err(ScoringError::NoUsableData);
    }

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
        week_fable_pct: Option<i64>,
        week_all: Option<&'a UsageSection>,
        week_fable: Option<&'a UsageSection>,
    }

    let mut candidates: Vec<Candidate<'_>> = data
        .profiles
        .iter()
        .filter_map(|(name, pu)| {
            // skip errored profiles (shell line 928: select(($e[.key] // null) == null))
            if errors.contains_key(name.as_str()) {
                return None;
            }
            // excluded names (the Orca slot) are never candidates
            if exclude.contains(&name.as_str()) {
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
            Some(Candidate {
                name: name.as_str(),
                week_pct,
                session_pct,
                week_fable_pct: pu.week_fable.as_ref().map(|s| s.pct),
                week_all: pu.week_all.as_ref(),
                week_fable: pu.week_fable.as_ref(),
            })
        })
        .collect();

    // Iterate rows, apply exclusion gates, track best.
    let mut best_name: Option<&str> = None;
    let mut best_key: (i64, i64) = (i64::MAX, i64::MAX);

    // Sort by name for deterministic tie-break behavior (HashMap order is
    // non-deterministic). The rank key is data-derived (epoch, pct), so stable
    // naming order ensures full ties are reproducible: the strictly-smaller
    // comparison below keeps the first name among fully-tied candidates.
    candidates.sort_by(|a, b| a.name.cmp(b.name));

    // Prefer-current: a viable current profile is kept as-is (no switch).
    if include_current
        && prefer_current
        && !current_profile.is_empty()
        && candidates.iter().any(|c| {
            c.name == current_profile
                && is_viable_pcts(c.session_pct, Some(c.week_pct), c.week_fable_pct)
        })
    {
        return Ok(None);
    }

    for c in &candidates {
        // Reactive (hook) mode: never target the current profile.
        if !include_current && !current_profile.is_empty() && c.name == current_profile {
            continue;
        }

        // Viability gate: session/week_all/week_fable, all through the single
        // authority — see `is_viable_pcts`'s doc.
        if !is_viable_pcts(c.session_pct, Some(c.week_pct), c.week_fable_pct) {
            continue;
        }

        // Rank key, smaller wins: (weekly reset epoch, negated week pct).
        // Primary: SOONEST weekly reset — parse failures / absent resets become
        // i64::MAX so a known epoch always beats an unknown one. The epoch
        // compared is the LATER of week_all's and week_fable's reset (when
        // both are known): a viable candidate is under neither cap, but it is
        // only fully "fresh" once BOTH weekly windows have rolled over, so
        // that later reset is the binding constraint on ranking. Secondary:
        // higher week_all.pct (drain the fuller of two same-reset accounts).
        // `reset_instant` prefers the machine-native `resets_at` epoch and only
        // falls back to re-parsing the `resets` display string when absent —
        // resolved against the same `now` as the staleness gate for determinism.
        let week_all_epoch = c
            .week_all
            .and_then(|s| s.reset_instant(now))
            .map(|dt| dt.timestamp());
        let week_fable_epoch = c
            .week_fable
            .and_then(|s| s.reset_instant(now))
            .map(|dt| dt.timestamp());
        let epoch = effective_reset_epoch(week_all_epoch, week_fable_epoch);
        let key = (epoch, -c.week_pct);

        if best_name.is_none() || key < best_key {
            best_name = Some(c.name);
            best_key = key;
        }
    }

    match best_name {
        // No winner. Distinguish "we read real numbers and they were all over
        // the limit" (AllSaturated → keep current) from "no profile had any
        // usable usage at all" (NoUsableData → open the picker). `candidates`
        // already excluded errored / no-week_all profiles, so an empty
        // candidate set means we never had data to score on.
        None if candidates.is_empty() => Err(ScoringError::NoUsableData),
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
            resets_at: None,
        }
    }

    fn make_profile(session_pct: Option<i64>, week_pct: i64, resets: Option<&str>) -> ProfileUsage {
        ProfileUsage {
            captured_at: None,
            session: session_pct.map(|p| make_section(p, None)),
            week_all: Some(make_section(week_pct, resets)),
            week_fable: None,
            week_model_label: None,
            session_stats: vec![],
            source: None,
            attention: None,
        }
    }

    /// Like [`make_profile`] but also carries a `week_fable` section —
    /// `fable_pct: None` means "no model-scoped weekly cap for this profile"
    /// (must not constrain viability); `Some(pct)` sets its reading.
    fn make_profile_with_fable(
        session_pct: Option<i64>,
        week_pct: i64,
        fable_pct: Option<i64>,
        fable_resets: Option<&str>,
    ) -> ProfileUsage {
        ProfileUsage {
            week_fable: fable_pct.map(|p| make_section(p, fable_resets)),
            week_model_label: fable_pct.map(|_| "Fable".to_string()),
            ..make_profile(session_pct, week_pct, None)
        }
    }

    fn make_data(profiles: HashMap<String, ProfileUsage>) -> UsageData {
        UsageData {
            captured_at: None,
            profiles,
            errors: None,
            ..Default::default()
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
            ..Default::default()
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

    /// A saturated profile is excluded; the remaining healthy one wins.
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

    /// Fixed reference instant for reset-epoch ranking tests: noon UTC on
    /// Jun 17 2026 (= 9pm KST), matching the `reset.rs` test convention.
    fn ranking_now() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 17, 12, 0, 0).unwrap()
    }

    /// PRIMARY key: the account whose weekly reset returns soonest wins, even
    /// against a much higher week_all.pct. Budget on the account that refills
    /// first is the cheapest to spend.
    #[test]
    fn sooner_reset_beats_higher_pct() {
        let mut profiles = HashMap::new();
        // "fresh" barely used but resets later (Jun 20).
        profiles.insert(
            "fresh".to_string(),
            make_profile(Some(0), 0, Some("Jun 20 at 9pm (Asia/Seoul)")),
        );
        // "burning" heavily used but resets sooner (Jun 18).
        profiles.insert(
            "burning".to_string(),
            make_profile(Some(5), 70, Some("Jun 18 at 9pm (Asia/Seoul)")),
        );
        let data = make_data(profiles);
        let result = pick_best_at(&data, "", false, true, ranking_now()).unwrap();
        assert_eq!(result.as_deref(), Some("burning"));
    }

    /// Regression for the two-account shape that motivated the policy flip:
    /// `heavy` is the more-used account (week 31%) but resets LATER (Jul 9);
    /// `light` is barely used (week 0%) but resets SOONER (Jul 8). The old
    /// highest-pct-first rule kept draining `heavy`; the new rule drains
    /// `light` (soonest refill = cheapest budget). `now` sits before both
    /// resets so no year-rollover perturbs the ordering. Names are neutral
    /// (the no_private_names guard forbids real profile literals here).
    #[test]
    fn sooner_resetting_account_wins_over_more_used_one() {
        use chrono::TimeZone;
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let mut profiles = HashMap::new();
        profiles.insert(
            "heavy".to_string(),
            make_profile(Some(27), 31, Some("Jul 9 at 8:59pm (Asia/Seoul)")),
        );
        profiles.insert(
            "light".to_string(),
            make_profile(Some(0), 0, Some("Jul 8 at 6pm (Asia/Seoul)")),
        );
        let data = make_data(profiles);
        // Proactive launch already on `heavy`: the sooner-resetting `light`
        // must be recommended as a switch, not silently kept.
        let result = pick_best_at(&data, "heavy", true, true, now).unwrap();
        assert_eq!(result.as_deref(), Some("light"));
    }

    /// A known weekly reset epoch beats an unknown one regardless of pct.
    #[test]
    fn known_reset_beats_unknown_regardless_of_pct() {
        let mut profiles = HashMap::new();
        profiles.insert("noreset".to_string(), make_profile(Some(5), 70, None));
        profiles.insert(
            "hasreset".to_string(),
            make_profile(Some(5), 10, Some("Jun 20 at 9pm (Asia/Seoul)")),
        );
        let data = make_data(profiles);
        let result = pick_best_at(&data, "", false, true, ranking_now()).unwrap();
        assert_eq!(result.as_deref(), Some("hasreset"));
    }

    /// `resets_at` (the machine-native epoch the local collector computed)
    /// wins even when the display string `resets` is garbage that would fail
    /// to parse — mirrors `UsageSection::reset_instant`'s own precedence, and
    /// pins that scoring reads the epoch through that helper rather than
    /// re-parsing `resets` itself.
    #[test]
    fn resets_at_epoch_wins_over_unparseable_resets_string() {
        let mut profiles = HashMap::new();
        // "epoch_wins" has an unparseable resets string but a resets_at epoch
        // that is SOONER than "string_only"'s correctly-parsed reset date —
        // if scoring fell back to parsing `resets`, this candidate would sink
        // to i64::MAX and lose; reading resets_at correctly makes it win.
        let sooner_epoch = ranking_now().timestamp() + 1_000; // well before Jun 20
        profiles.insert(
            "epoch_wins".to_string(),
            ProfileUsage {
                captured_at: None,
                session: Some(make_section(5, None)),
                week_all: Some(UsageSection {
                    pct: 70,
                    resets: Some("not a valid reset string".to_string()),
                    resets_at: Some(sooner_epoch),
                }),
                week_fable: None,
                week_model_label: None,
                session_stats: vec![],
                source: None,
                attention: None,
            },
        );
        profiles.insert(
            "string_only".to_string(),
            make_profile(Some(5), 10, Some("Jun 20 at 9pm (Asia/Seoul)")),
        );
        let data = make_data(profiles);
        let result = pick_best_at(&data, "", false, true, ranking_now()).unwrap();
        assert_eq!(result.as_deref(), Some("epoch_wins"));
    }

    /// Equal reset epoch → the HIGHER week_all.pct wins (drain the fuller of
    /// two accounts whose budgets refill at the same instant).
    #[test]
    fn equal_reset_higher_pct_wins() {
        let mut profiles = HashMap::new();
        let resets = Some("Jun 18 at 9pm (Asia/Seoul)");
        profiles.insert("low".to_string(), make_profile(Some(5), 30, resets));
        profiles.insert("high".to_string(), make_profile(Some(5), 70, resets));
        let data = make_data(profiles);
        let result = pick_best_at(&data, "", false, true, ranking_now()).unwrap();
        assert_eq!(result.as_deref(), Some("high"));
    }

    /// SECONDARY key fallback: when no candidate has a parseable reset, the
    /// higher week_pct profile is picked (the pre-0.3 primary key).
    #[test]
    fn no_resets_picks_highest_week_pct() {
        let mut profiles = HashMap::new();
        // "low" at 30%, "high" at 70%
        profiles.insert("low".to_string(), make_profile(Some(5), 30, None));
        profiles.insert("high".to_string(), make_profile(Some(5), 70, None));
        let data = make_data(profiles);
        let result = pick_best(&data, "", false).unwrap();
        assert_eq!(result.as_deref(), Some("high"));
    }

    // ─── full-tie fallback (equal epoch + equal pct) ──────────────────────────

    /// Equal week_pct, both with no resets string → both epochs are unknown →
    /// the full tie is broken by alphabetical candidate order (first wins).
    /// This validates the tie-break code path without calling the reset parser.
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

    // ─── no-usable-data vs all-saturated ──────────────────────────────────────

    /// A profile with NO week_all section (the usage blob arrived empty /
    /// degenerate for it). `make_profile` always fills week_all, so build it by
    /// hand.
    fn make_profile_no_week(session_pct: Option<i64>) -> ProfileUsage {
        ProfileUsage {
            captured_at: None,
            session: session_pct.map(|p| make_section(p, None)),
            week_all: None,
            week_fable: None,
            week_model_label: None,
            session_stats: vec![],
            source: None,
            attention: None,
        }
    }

    /// Every profile lacks week_all → zero candidates ever formed. This is
    /// "we couldn't tell" (NoUsableData → caller opens the picker), NOT
    /// AllSaturated ("we read real numbers and they were all over the line").
    #[test]
    fn no_week_data_anywhere_is_no_usable_data_not_saturated() {
        let mut profiles = HashMap::new();
        profiles.insert("a".to_string(), make_profile_no_week(Some(10)));
        profiles.insert("b".to_string(), make_profile_no_week(None));
        let data = make_data(profiles);
        let err = pick_best(&data, "", true).unwrap_err();
        assert!(
            matches!(err, ScoringError::NoUsableData),
            "all-no-week must be NoUsableData (open picker), got {err:?}"
        );
    }

    // (empty-profiles and all-errored NoUsableData cases live next to their
    // former AllSaturated counterparts below, updated to the new verdict.)

    /// Contrast: profiles DID have week_all and they were all saturated →
    /// candidates formed then gated out → AllSaturated (keep current), NOT
    /// NoUsableData. Guards the boundary between the two verdicts.
    #[test]
    fn real_saturation_stays_all_saturated_not_no_usable_data() {
        let mut profiles = HashMap::new();
        profiles.insert("a".to_string(), make_profile(Some(10), 96, None)); // >= SATURATION
        profiles.insert("b".to_string(), make_profile(Some(10), 98, None));
        let data = make_data(profiles);
        let err = pick_best(&data, "", true).unwrap_err();
        assert!(
            matches!(err, ScoringError::AllSaturated),
            "real all-saturated must stay AllSaturated, got {err:?}"
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

    /// Empty profiles → NoUsableData (we never had a number to score on — the
    /// caller opens the picker, it must not be mistaken for "all at the limit").
    #[test]
    fn empty_profiles_is_no_usable_data() {
        let data = make_data(HashMap::new());
        let err = pick_best(&data, "", false).unwrap_err();
        assert!(matches!(err, ScoringError::NoUsableData));
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

    /// A NeedsLogin profile (design spec "맛이 간 프로필은 로그인하라고 경고") —
    /// dead credentials, recorded in `errors` per `local::mod`'s exclusion
    /// mechanism — must never be picked even when it carries the best numbers
    /// in the whole registry. Launching under it would just land the user on
    /// Claude Code's `/login` screen.
    #[test]
    fn needs_login_profile_with_best_numbers_is_not_picked() {
        let mut profiles = HashMap::new();
        // "dead" has by far the best (lowest) week_pct, but its credentials
        // are dead — it still carries an `attention` + stale numbers (the
        // report/footer surfaces still need something to render), yet must
        // be excluded from scoring via `errors`.
        let mut dead = make_profile(Some(1), 2, None);
        dead.attention = Some(crate::usage::model::Attention {
            kind: crate::usage::model::AttentionKind::NeedsLogin,
            message: "credentials expired".to_string(),
            action: "CLAUDE_CONFIG_DIR=/Users/example/.claude.dead claude auth login".to_string(),
            since_epoch: Some(1_756_000_000),
        });
        profiles.insert("dead".to_string(), dead);
        // "healthy" has worse numbers but usable credentials.
        profiles.insert("healthy".to_string(), make_profile(Some(50), 60, None));
        let mut errors = HashMap::new();
        errors.insert(
            "dead".to_string(),
            "credentials expired — login required".to_string(),
        );
        let data = make_data_with_errors(profiles, errors);
        let result = pick_best(&data, "", false).unwrap();
        assert_eq!(
            result.as_deref(),
            Some("healthy"),
            "a needs_login profile must never be auto-picked, regardless of its numbers"
        );
    }

    /// All profiles errored → NoUsableData (transport/scrape failures, not real
    /// limits — the caller opens the picker rather than silently keeping current).
    #[test]
    fn all_errored_is_no_usable_data() {
        let mut profiles = HashMap::new();
        profiles.insert("p1".to_string(), make_profile(Some(5), 50, None));
        let mut errors = HashMap::new();
        errors.insert("p1".to_string(), "error".to_string());
        let data = make_data_with_errors(profiles, errors);
        let err = pick_best(&data, "", false).unwrap_err();
        assert!(matches!(err, ScoringError::NoUsableData));
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

    // ─── model-scoped weekly (week_fable) gate ────────────────────────────────
    // week_fable no longer constrains viability at all (a Fable-only
    // cap is handled by the Stop hook's same-account model fallback instead —
    // see `src/hook/detect.rs`). These tests pin that rule: a saturated
    // week_fable reading must NOT exclude a profile.

    /// `is_viable_pcts` sanity: the three dimensions in isolation and combined.
    #[test]
    fn is_viable_pcts_dimensions() {
        // Healthy across the board.
        assert!(is_viable_pcts(10, Some(50), Some(50)));
        // week_fable absent → does not constrain viability.
        assert!(is_viable_pcts(10, Some(50), None));
        // week_fable saturated no longer constrains viability — still
        // viable even at 100%, as long as session/week_all are fine.
        assert!(is_viable_pcts(10, Some(50), Some(SATURATION_PCT)));
        assert!(is_viable_pcts(10, Some(50), Some(100)));
        // session limited → not viable regardless of the weekly dimensions.
        assert!(!is_viable_pcts(LIMIT_PCT, Some(0), None));
    }

    /// A profile whose model-scoped weekly cap (week_fable) is fully
    /// exhausted (100%) stays pickable as long as its
    /// session and week_all readings are healthy — dropping the week_fable
    /// branch from `is_viable_pcts` leaves it viable. Proven independent of
    /// any tie-break: the OTHER profile is passed as `current_profile` with
    /// `include_current=false`, so it is excluded from the candidate pool
    /// entirely and the fable-saturated one is the only candidate left — if
    /// `is_viable_pcts` still excluded it, this would resolve to `None`, not
    /// a name-order win.
    #[test]
    fn fable_saturated_no_longer_excludes_with_healthy_session_and_week_all() {
        let mut profiles = HashMap::new();
        profiles.insert(
            "zzz_fable_capped".to_string(),
            make_profile_with_fable(Some(5), 10, Some(100), None),
        );
        profiles.insert(
            "avail".to_string(),
            make_profile_with_fable(Some(5), 10, None, None),
        );
        let data = make_data(profiles);
        let result = pick_best(&data, "avail", false).unwrap();
        assert_eq!(
            result.as_deref(),
            Some("zzz_fable_capped"),
            "a fable-saturated profile must stay pickable — a model-scoped-only cap \
             leaves the account usable on another model"
        );
    }

    /// `week_fable: None` (this profile carries no model-scoped weekly limit
    /// at all) must NOT be treated as limited — it stays a normal candidate.
    #[test]
    fn fable_none_is_viable() {
        let mut profiles = HashMap::new();
        profiles.insert(
            "no_fable_cap".to_string(),
            make_profile_with_fable(Some(5), 10, None, None),
        );
        let data = make_data(profiles);
        let result = pick_best(&data, "", false).unwrap();
        assert_eq!(result.as_deref(), Some("no_fable_cap"));
    }

    /// `week_fable.pct` just under `SATURATION_PCT` (94%) must still be viable
    /// — the threshold is `>=`, not `>`.
    #[test]
    fn fable_just_under_saturation_is_viable() {
        let mut profiles = HashMap::new();
        profiles.insert(
            "almost_capped".to_string(),
            make_profile_with_fable(Some(5), 10, Some(SATURATION_PCT - 1), None),
        );
        let data = make_data(profiles);
        let result = pick_best(&data, "", false).unwrap();
        assert_eq!(result.as_deref(), Some("almost_capped"));
    }

    /// `week_fable.pct == SATURATION_PCT` (95%) no longer excludes —
    /// same `>=` rule as `week_all` used to apply, but this dimension is out
    /// of `is_viable_pcts` entirely now. Same exclusion-based proof as
    /// `fable_saturated_no_longer_excludes_with_healthy_session_and_week_all`:
    /// the healthy profile is `current_profile` and excluded, so the capped
    /// one winning is a viability result, not a tie-break coincidence.
    #[test]
    fn fable_at_saturation_is_no_longer_excluded() {
        let mut profiles = HashMap::new();
        profiles.insert(
            "zzz_capped".to_string(),
            make_profile_with_fable(Some(5), 10, Some(SATURATION_PCT), None),
        );
        profiles.insert(
            "avail".to_string(),
            make_profile_with_fable(Some(5), 10, None, None),
        );
        let data = make_data(profiles);
        let result = pick_best(&data, "avail", false).unwrap();
        assert_eq!(result.as_deref(), Some("zzz_capped"));
    }

    /// Two profiles differ ONLY in their fable saturation. When a
    /// model-scoped-only cap used to exclude a profile outright, the
    /// uncapped one always won; now both are viable and rank identically
    /// (same reset, same week_pct), so the winner is whichever tie-break —
    /// name order — favours.
    /// RANKING ITSELF IS UNCHANGED by this commit: this expectation flips only
    /// because dropping the viability branch puts both candidates in the race.
    ///
    /// Names are deliberately picked so the alphabetically-first one ("avail")
    /// carries the WORSE (higher) fable pct: if the winner tracked fable pct
    /// instead of name — the exact regression this test exists to catch — it
    /// would pick "fable_ok" (the lower pct) instead, not silently agree.
    #[test]
    fn only_fable_difference_no_longer_affects_viability() {
        let mut profiles = HashMap::new();
        let resets = Some("Jun 20 at 9pm (Asia/Seoul)");
        profiles.insert(
            "avail".to_string(),
            make_profile_with_fable(Some(5), 30, Some(99), resets),
        );
        profiles.insert(
            "fable_ok".to_string(),
            make_profile_with_fable(Some(5), 30, Some(20), resets),
        );
        let data = make_data(profiles);
        let result = pick_best_at(&data, "", false, true, ranking_now()).unwrap();
        assert_eq!(
            result.as_deref(),
            Some("avail"),
            "both are viable now and tie on rank; name order breaks the tie, not fable pct"
        );
    }

    /// The CURRENT profile is excluded in reactive mode
    /// (`include_current=false`, mirroring the hook) regardless of its own
    /// viability — this is the reactive-current exclusion, not the
    /// `is_viable_pcts` gate (fable saturation no longer excludes anything;
    /// this test just confirms that unrelated gate still holds
    /// when the current profile happens to be fable-capped).
    #[test]
    fn fable_capped_current_is_skipped_in_reactive_mode() {
        let mut profiles = HashMap::new();
        profiles.insert(
            "current".to_string(),
            make_profile_with_fable(Some(2), 5, Some(100), None),
        );
        profiles.insert(
            "alt".to_string(),
            make_profile_with_fable(Some(5), 10, None, None),
        );
        let data = make_data(profiles);
        let result = pick_best(&data, "current", false).unwrap();
        assert_eq!(result.as_deref(), Some("alt"));
    }

    /// Ranking: when both `week_all` and `week_fable` resets are known for a
    /// viable candidate, the LATER of the two is the binding constraint —
    /// mirrors `cmd::run::account_row_rank`'s identical `max()` rule.
    #[test]
    fn ranking_uses_later_of_week_all_and_fable_reset() {
        let mut profiles = HashMap::new();
        // "early_all_late_fable": week_all resets Jun 18, but its (healthy)
        // fable cap resets later, Jun 25 — the effective epoch is Jun 25.
        profiles.insert(
            "early_all_late_fable".to_string(),
            make_profile_with_fable(Some(5), 10, Some(20), Some("Jun 25 at 9pm (Asia/Seoul)")),
        );
        {
            let p = profiles.get_mut("early_all_late_fable").unwrap();
            p.week_all = Some(make_section(10, Some("Jun 18 at 9pm (Asia/Seoul)")));
        }
        // "flat_jun20": both dimensions reset Jun 20 — effective epoch Jun 20,
        // sooner than the other candidate's effective Jun 25 → this one wins.
        profiles.insert(
            "flat_jun20".to_string(),
            make_profile_with_fable(Some(5), 10, Some(20), Some("Jun 20 at 9pm (Asia/Seoul)")),
        );
        {
            let p = profiles.get_mut("flat_jun20").unwrap();
            p.week_all = Some(make_section(10, Some("Jun 20 at 9pm (Asia/Seoul)")));
        }
        let data = make_data(profiles);
        let result = pick_best_at(&data, "", false, true, ranking_now()).unwrap();
        assert_eq!(
            result.as_deref(),
            Some("flat_jun20"),
            "the candidate whose LATER (binding) dimension resets sooner must win"
        );
    }

    // ─── exclusion check (errors map / is_viable_pcts, inlined as pick_best_at
    // applies it) ───────────────────────────────────────────────────────────

    /// Mirrors the exclusion gate `pick_best_at` applies inline: a profile is
    /// excluded when it is present in `errors`, or when its pcts fail
    /// `is_viable_pcts`.
    fn is_excluded(data: &UsageData, profile: &str) -> bool {
        if let Some(errors) = &data.errors
            && errors.contains_key(profile)
        {
            return true;
        }

        if let Some(pu) = data.profiles.get(profile) {
            let session_pct = pu
                .session
                .as_ref()
                .map(|s| s.pct)
                .unwrap_or(ABSENT_SESSION_PCT);
            let week_all_pct = pu.week_all.as_ref().map(|s| s.pct);
            let week_fable_pct = pu.week_fable.as_ref().map(|s| s.pct);
            if !is_viable_pcts(session_pct, week_all_pct, week_fable_pct) {
                return true;
            }
        }

        false
    }

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

    #[test]
    fn fable_saturated_is_not_excluded() {
        let mut profiles = HashMap::new();
        profiles.insert(
            "p".to_string(),
            make_profile_with_fable(Some(5), 10, Some(SATURATION_PCT), None),
        );
        let data = make_data(profiles);
        assert!(
            !is_excluded(&data, "p"),
            "week_fable saturation no longer excludes a profile"
        );
    }

    #[test]
    fn is_excluded_fable_none_is_false() {
        let mut profiles = HashMap::new();
        profiles.insert(
            "p".to_string(),
            make_profile_with_fable(Some(5), 10, None, None),
        );
        let data = make_data(profiles);
        assert!(!is_excluded(&data, "p"));
    }

    // ─── data.current_usage ────────────────────────────────────────────────

    #[test]
    fn current_usage_pcts_present_profile() {
        let mut profiles = HashMap::new();
        profiles.insert("p".to_string(), make_profile(Some(42), 31, None));
        let data = make_data(profiles);
        let (sess, week) = data.current_usage("p").unwrap();
        assert_eq!(sess, 42);
        assert_eq!(week, 31);
    }

    #[test]
    fn current_usage_pcts_absent_session_encodes_minus_one() {
        let mut profiles = HashMap::new();
        profiles.insert("p".to_string(), make_profile(None, 55, None));
        let data = make_data(profiles);
        let (sess, week) = data.current_usage("p").unwrap();
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
        assert!(data.current_usage("p").is_none());
    }

    #[test]
    fn current_usage_pcts_absent_is_none() {
        let data = make_data(HashMap::new());
        assert!(data.current_usage("nonexistent").is_none());
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
            week_fable: None,
            week_model_label: None,
            session_stats: vec![],
            source: None,
            attention: None,
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

    // ─── staleness gate (max-age) ────────────────────────────────────────────

    use chrono::TimeZone;

    /// A clearly-pickable single-profile dataset with a top-level `captured_at`.
    fn make_data_captured(captured_at: &str) -> UsageData {
        let mut profiles = HashMap::new();
        profiles.insert("alt".to_string(), make_profile(Some(10), 50, None));
        UsageData {
            captured_at: Some(captured_at.to_string()),
            profiles,
            errors: None,
            ..Default::default()
        }
    }

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn fresh_data_scores_normally() {
        // captured 5 min before `now` — well inside the 1800s default gate.
        let data = make_data_captured("2026-06-29T12:00:00Z");
        let now = at("2026-06-29T12:05:00Z");
        let result = pick_best_at(&data, "main", false, true, now).unwrap();
        assert_eq!(result.as_deref(), Some("alt"), "fresh data must score");
    }

    #[test]
    fn stale_data_degrades_to_no_usable_data() {
        // captured 40 min before `now` — past the 1800s (30 min) default gate.
        // Even though "alt" is trivially pickable, the gate must refuse to score
        // and hand the caller NoUsableData (→ picker / fail-safe-to-current),
        // NOT a confident auto-pick on percentages we can no longer trust.
        let data = make_data_captured("2026-06-29T12:00:00Z");
        let now = at("2026-06-29T12:40:00Z");
        let err = pick_best_at(&data, "main", false, true, now).unwrap_err();
        assert!(
            matches!(err, ScoringError::NoUsableData),
            "stale data must be NoUsableData (open picker), got {err:?}"
        );
    }

    #[test]
    fn stale_data_scores_when_gate_off() {
        // Same 40-min-stale dataset as `stale_data_degrades_to_no_usable_data`,
        // but with the gate OFF (apply_stale_gate=false) — the reactive-hook
        // path. The hook fires only because the current profile already hit a
        // limit and is non-interactive, so it must move to the freshest-known
        // best even on stale numbers rather than strand the user on the limited
        // profile. This is the auto-switch bug fix: gate-on returned
        // NoUsableData → NotifyOnly (no switch); gate-off scores → LimitSwitch.
        let data = make_data_captured("2026-06-29T12:00:00Z");
        let now = at("2026-06-29T12:40:00Z");
        let result = pick_best_at(&data, "main", false, false, now).unwrap();
        assert_eq!(
            result.as_deref(),
            Some("alt"),
            "gate off must score stale data so the hook can switch off a limited profile"
        );
    }

    #[test]
    fn missing_captured_at_fails_open() {
        // No captured_at anywhere (legacy cache / non-standard source). The gate must
        // NOT block — unknown age is trusted, preserving pre-gate behaviour.
        let mut profiles = HashMap::new();
        profiles.insert("alt".to_string(), make_profile(Some(10), 50, None));
        let data = make_data(profiles); // captured_at: None
        let now = at("2026-06-29T12:40:00Z");
        let result = pick_best_at(&data, "main", false, true, now).unwrap();
        assert_eq!(
            result.as_deref(),
            Some("alt"),
            "missing captured_at must fail-open (trust the data)"
        );
    }

    #[test]
    fn per_profile_captured_at_is_used_when_top_level_absent() {
        // top-level captured_at absent, but the profile carries one that is
        // fresh → score normally (newest_captured_at falls back to per-profile).
        let mut profiles = HashMap::new();
        let mut p = make_profile(Some(10), 50, None);
        p.captured_at = Some("2026-06-29T12:04:00Z".to_string());
        profiles.insert("alt".to_string(), p);
        let data = UsageData {
            captured_at: None,
            profiles,
            errors: None,
            ..Default::default()
        };
        let now = at("2026-06-29T12:05:00Z");
        let result = pick_best_at(&data, "main", false, true, now).unwrap();
        assert_eq!(result.as_deref(), Some("alt"));
    }

    #[test]
    fn gate_disabled_with_zero_trusts_any_age() {
        // CLAUDE_USAGE_MAX_AGE=0 disables the gate: even ancient data scores.
        crate::testenv::with_env_var("CLAUDE_USAGE_MAX_AGE", Some("0"), || {
            let data = make_data_captured("2020-01-01T00:00:00Z"); // years old
            let now = at("2026-06-29T12:00:00Z");
            let result = pick_best_at(&data, "main", false, true, now);
            assert_eq!(
                result.unwrap().as_deref(),
                Some("alt"),
                "max-age 0 must disable the gate (trust any age)"
            );
        });
    }

    #[test]
    fn usage_max_age_unparseable_legacy_falls_through_to_alias() {
        crate::testenv::with_env_vars(
            &[
                ("CLAUDE_USAGE_MAX_AGE", Some("not-a-number")),
                ("CSM_USAGE_MAX_AGE_SECS", Some("90")),
            ],
            || {
                let result = usage_max_age_secs();
                assert_eq!(
                    result, 90,
                    "present-but-unparseable legacy var should fall through to a valid alias"
                );
            },
        );
    }

    #[test]
    fn future_captured_at_is_not_stale() {
        // Clock skew: captured_at slightly in the future. A negative age must
        // not be read as "stale" — score normally.
        let data = make_data_captured("2026-06-29T12:10:00Z");
        let now = at("2026-06-29T12:05:00Z");
        let result = pick_best_at(&data, "main", false, true, now).unwrap();
        assert_eq!(result.as_deref(), Some("alt"));
    }

    #[test]
    fn newest_captured_at_prefers_top_level() {
        // top-level present and fresh, a profile's per-slice stale → top-level
        // wins (newest), so the dataset is fresh and scores.
        let mut profiles = HashMap::new();
        let mut p = make_profile(Some(10), 50, None);
        p.captured_at = Some("2026-06-29T11:00:00Z".to_string()); // old slice
        profiles.insert("alt".to_string(), p);
        let data = UsageData {
            captured_at: Some("2026-06-29T12:04:00Z".to_string()), // fresh top-level
            profiles,
            errors: None,
            ..Default::default()
        };
        let now = at("2026-06-29T12:05:00Z");
        let result = pick_best_at(&data, "main", false, true, now).unwrap();
        assert_eq!(result.as_deref(), Some("alt"));
        // sanity: the helper picks the newer of the two
        let newest = newest_captured_at(&data).unwrap();
        assert_eq!(newest, Utc.with_ymd_and_hms(2026, 6, 29, 12, 4, 0).unwrap());
    }

    // ─── pick_best_with: prefer_current + exclude ────────────────────────────

    fn heavy_light() -> UsageData {
        let mut profiles = HashMap::new();
        profiles.insert(
            "heavy".to_string(),
            make_profile(Some(27), 31, Some("Jul 9 at 8:59pm (Asia/Seoul)")),
        );
        profiles.insert(
            "light".to_string(),
            make_profile(Some(0), 0, Some("Jul 8 at 6pm (Asia/Seoul)")),
        );
        make_data(profiles)
    }

    fn july_now() -> DateTime<Utc> {
        use chrono::TimeZone;
        Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap()
    }

    #[test]
    fn prefer_current_keeps_a_viable_current_profile() {
        let policy = PickPolicy {
            include_current: true,
            apply_stale_gate: true,
            prefer_current: true,
            exclude: &[],
        };
        // Without the preference `light` would win (see the test above).
        let result = pick_best_with(&heavy_light(), "heavy", &policy, july_now()).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn prefer_current_still_moves_off_a_non_viable_current_profile() {
        let mut data = heavy_light();
        data.profiles.insert(
            "heavy".to_string(),
            make_profile(Some(100), 31, Some("Jul 9 at 8:59pm (Asia/Seoul)")),
        );
        let policy = PickPolicy {
            include_current: true,
            apply_stale_gate: true,
            prefer_current: true,
            exclude: &[],
        };
        let result = pick_best_with(&data, "heavy", &policy, july_now()).unwrap();
        assert_eq!(result.as_deref(), Some("light"));
    }

    #[test]
    fn prefer_current_is_ignored_in_reactive_mode() {
        let policy = PickPolicy {
            include_current: false,
            apply_stale_gate: false,
            prefer_current: true,
            exclude: &[],
        };
        let result = pick_best_with(&heavy_light(), "heavy", &policy, july_now()).unwrap();
        assert_eq!(result.as_deref(), Some("light"));
    }

    #[test]
    fn excluded_profile_is_never_a_candidate() {
        let policy = PickPolicy {
            include_current: true,
            apply_stale_gate: true,
            prefer_current: false,
            exclude: &["light"],
        };
        let result = pick_best_with(&heavy_light(), "heavy", &policy, july_now()).unwrap();
        assert_eq!(result, None, "light excluded, heavy is the only winner");
    }

    #[test]
    fn excluded_current_profile_is_not_preferred() {
        // The Orca slot as `current` must never be kept by prefer_current.
        let policy = PickPolicy {
            include_current: true,
            apply_stale_gate: true,
            prefer_current: true,
            exclude: &["heavy"],
        };
        let result = pick_best_with(&heavy_light(), "heavy", &policy, july_now()).unwrap();
        assert_eq!(result.as_deref(), Some("light"));
    }

    #[test]
    fn only_excluded_data_reads_as_no_usable_data() {
        let policy = PickPolicy {
            include_current: true,
            apply_stale_gate: true,
            prefer_current: false,
            exclude: &["heavy", "light"],
        };
        let err = pick_best_with(&heavy_light(), "", &policy, july_now()).unwrap_err();
        assert!(matches!(err, ScoringError::NoUsableData));
    }

    #[test]
    fn pick_best_at_equals_default_policy() {
        let at = pick_best_at(&heavy_light(), "heavy", true, true, july_now()).unwrap();
        let with = pick_best_with(
            &heavy_light(),
            "heavy",
            &PickPolicy {
                include_current: true,
                apply_stale_gate: true,
                ..Default::default()
            },
            july_now(),
        )
        .unwrap();
        assert_eq!(at, with);
    }
}
