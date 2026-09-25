//! Hook event classification — stdin JSON parsing + limit detection tiers.
//!
//! # Stdin contract (Claude Code hook spec)
//!
//! Claude Code writes a JSON object to the hook's stdin on every Stop/SubagentStop/
//! SessionEnd/StopFailure event. The fields we care about (serde field names are
//! the exact snake_case keys CC sends):
//!
//! ```json
//! {
//!   "session_id":       "01234567-...",
//!   "cwd":              "/Users/example/Projects/...",
//!   "reason":           "stop",
//!   "transcript_path":  "/Users/example/.claude.shared/projects/.../session.jsonl"
//! }
//! ```
//!
//! On a StopFailure event, `reason` is absent and three more fields appear
//! instead (still all `Option` — see [`HookInput`]):
//!
//! ```json
//! {
//!   "session_id":      "01234567-...",
//!   "hook_event_name": "StopFailure",
//!   "error":           "rate_limit",
//!   "error_details":   "free text from the API, optional"
//! }
//! ```
//!
//! # Detection tiers
//!
//! Tier-0: `stop_failure_limit` — `hook_event_name == "StopFailure" && error
//!   == "rate_limit"`. Fires when a 429 ends the turn: Claude Code's query
//!   loop runs the StopFailure hook instead of Stop for that turn —
//!   fire-and-forget, not awaited, stdout/exit code ignored, and skipped for
//!   subagent contexts. Tier-2 never sees that turn's Stop event at all.
//!   CAVEAT (observed on Claude Code 2.1.270, 2026-09): a *subscription* cap
//!   — the 5-hour session, weekly all-model, or model-scoped weekly limit,
//!   whose 429 carries a reset time — does NOT end the turn. Claude Code shows
//!   "Weekly limit reached · Retrying in 6h" and parks the turn in an internal
//!   auto-retry wait; neither StopFailure nor Stop fires, so no hook runs at
//!   all for the duration. The switch for that case comes from the statusline
//!   tick instead (see "Statusline tick" below). Tier-0 stays correct for the
//!   429s that do end the turn.
//!   Requires the hook to be registered on the `StopFailure` event with
//!   matcher `rate_limit` (a companion change outside this crate that adds
//!   `csm hook --owner <dir>` as a StopFailure handler); until that
//!   registration exists, no StopFailure payload ever reaches this process and
//!   tier-2 remains the only detector, as before. A StopFailure with any
//!   OTHER `error` value (`overloaded`, `authentication_failed`,
//!   `invalid_request`, ...) is a real API failure but not a usage limit —
//!   `classify()` does nothing for it and does not fall through to tier-2.
//!   Unlike tier-2, tier-0 is pure over the parsed stdin and does zero I/O, so
//!   it has no freshness bound at all — it is data straight from the failing
//!   request, not a cache.
//!
//! Tier-2: `current-usage` thresholding — session%, week_all%, or the
//!   model-scoped week_fable% at or above limits (all three dimensions, same
//!   `LIMIT_PCT` threshold; `week_fable` absent ⇒ that dimension never fires).
//!   Reproduces the tier-2 block from the legacy shell implementation, extended
//!   with the week_fable dimension the shell source predates.
//!   Freshness: session%/week_all% are also refreshed by the statusline capture,
//!   but the statusline `rate_limits` payload carries no model-scoped window, so
//!   week_fable% is only as fresh as the last per-profile usage-API probe
//!   (`CSM_USAGE_PROFILE_TTL`, default 300 s). A model-scoped cap can therefore
//!   take up to ~5 min to be noticed here — a data-freshness bound, not a
//!   detection gap.
//!
//! # Statusline tick (a second entry point, not a tier)
//!
//! [`crate::hook::run_from_statusline`] runs [`classify_with`] off `csm usage
//! capture` — the statusLine wrapper pipes Claude Code's statusLine stdin into
//! it about once a second, for as long as the session is alive, including
//! while a turn is parked in the auto-retry wait above. The payload carries
//! the session's own `rate_limits` (session + all-model weekly, off its own
//! API responses) plus `session_id`/`cwd`/`transcript_path`; the recorder
//! merges in the store's model-scoped weekly reading and
//! [`statusline_limit_hit_default`] reduces the three to the tier-2 test. A
//! hit is handed to `classify_with`
//! as `live_limit`, which takes it as limited + definitive at step 3 and runs
//! every other step unchanged. Freshness is that of the tick itself for
//! session/week_all and of the last usage-API probe for week_fable (same
//! bound as tier-2). This path must do no usage I/O of its own from here on:
//! the merged reading was already assembled by the capture step before
//! `classify_with` ever runs, so 5b's marker check reads `resets_at` straight
//! off the `LimitHit` this section produced rather than opening the usage
//! cache a second time (see [`LimitHit`]'s doc) — a tick that runs about once
//! a second for every live session cannot afford an extra read on top of the
//! one the capture step already did.
//!
//! # Flow (matches the legacy shell implementation exactly)
//!
//! 1. Kill-switches 1a/1b (env var, file marker) — cheap and
//!    dimension-independent, so they still run first. 1c (the `.switched`
//!    marker) needs the tripped dimension to decide anything, so it moves to
//!    step 4b, right after detection.
//! 2. Reason gate → user_quit flag (doesn't exit yet — detection still runs).
//! 3. Detect (tier-0 / tier-2). Tier-0 short-circuits the other two outcomes:
//!    `NotLimit` → immediate `Skip` (a non-limit StopFailure is never a limit,
//!    whatever tier-2 might say from unrelated stale state); `Limit` →
//!    limited, `definitive = true`; `NotApplicable` (not a StopFailure) →
//!    fall through to tier-2 exactly as before, `definitive = false`.
//! 4. If not limited → exit (with user-quit log if user_quit).
//!
//! 4b. Kill-switch 1c: a `.switched` marker already on disk for this session
//! blocks every dimension EXCEPT `WeekFable` while the fallback is enabled
//! (see [`switched_marker_blocks`]). 1c exists to stop account-switch loops;
//! a `WeekFable` trip that goes on to fall back at 5b never switches an
//! account, spends no hop, and carries its own one-shot marker, so it must
//! not be stopped by a switch an EARLIER, different-dimension trip already
//! made on this same session. With the fallback disabled
//! (`CLAUDE_FABLE_FALLBACK=0`) a `WeekFable` trip is blocked here exactly
//! like any other dimension, matching the pre-fallback behaviour the knob is
//! meant to restore.
//!
//! 5. If user_quit + limited → notify-only (deduped via .detected).
//!
//! 5b. Fable-cap same-account model fallback (sits between steps 5 and 6): a
//! `week_fable` (model-scoped weekly cap) trip does not exclude the account
//! or consume the account-switch hop budget — the account still has headroom
//! on every other model. When `CLAUDE_FABLE_FALLBACK` (default on) is
//! enabled and this session hasn't already used its one-shot fallback on the
//! CURRENT profile (`<sid>.model-fallback`, current for this profile and not
//! stale — see [`model_fallback_marker_is_stale`] and
//! [`model_fallback_marker_is_current`]), `target_profile` is set to
//! `current_profile` and the relaunch carries `model_override` (default
//! fallback model `CLAUDE_FABLE_FALLBACK_MODEL`, default `"opus"`) instead of
//! picking a different account. A marker written on a DIFFERENT profile (an
//! ordinary account switch moved this session off the account that wrote it)
//! does not count either — see [`model_fallback_marker_is_current`] — so a
//! fresh fallback can fire and rewrite it for the account the session is on
//! now.
//!
//! 5c. Suppress a repeat trip within the same fallback window, silently: once
//! the one-shot marker exists, is current for this profile, and is still
//! fresh, a further `week_fable` trip this session is `Decision::Skip`
//! rather than falling through to step 6 or notifying again. `week_fable`
//! stays capped for days, so falling through to the ordinary account-switch
//! path on the very next tick would undo the fallback within seconds of it
//! firing, and could even land the session on another Fable-capped account
//! now that such an account is pickable again — week_fable no longer
//! constrains viability at all, see `scoring::is_viable_pcts`'s doc; and the
//! fallback itself was already logged when it fired, so the repeat trip has
//! nothing new to say — a notify-only here would also consume the session's
//! one-shot `.detected` slot for nothing, silently swallowing a later,
//! genuinely new notify-only (say a `week_all` cap with no viable target) for
//! the rest of this session. This is bounded, not permanent: once the marker
//! goes stale (the weekly window rolled over) or belongs to a profile this
//! session has since left, `already_fell_back` reads false again and 5b's
//! fallback can fire once more. Gated on `CLAUDE_FABLE_FALLBACK` too — with
//! the knob off the whole feature is meant to be inert, so a stale-knob
//! marker from an earlier run must fall through to the ordinary
//! account-switch path, not be silently swallowed. `session` and `week_all`
//! trips never reach this step — `already_fell_back` is never set for them.
//!
//! 6. Pick target profile (skipped when 5b already set `target_profile`; no
//!    viable target → notify-only, deduped via .detected).
//! 7. If CLAUDE_AUTO_SWITCH_RELAUNCH != "1" → notify-only (deduped via .detected).
//! 8. Managed-session gate: check .pid file.
//! 9. Cooldown gate (noclobber .last-switch) — skipped entirely for a 5b
//!    fallback (a same-account model change claims no shared resource and
//!    must not be blocked by, or stamp, another session's cooldown);
//!    otherwise EXCEPT when `definitive` from step 3: a tier-0 or
//!    statusline-tick signal is never blocked by this cooldown (each
//!    csm-supervised session sharing a now-capped account gets its own
//!    independent StopFailure and must be allowed to switch off it, not just
//!    the first one to notice — see the step-9 comment in [`classify`]). The
//!    stamp is still refreshed/claimed best-effort either way for a
//!    non-fallback call, so pct-based (tier-2) detections elsewhere keep
//!    today's throttle.
//! 10. Hop guard (skipped for a 5b fallback — see [`fable_fallback_model`]).
//! 11. Build the handoff prompt (a 5b fallback gets its own wording — see
//!     [`build_fallback_handoff`]) → `Decision::LimitSwitch { target_profile,
//!     handoff, cwd, born, model_override }`.

use std::path::Path;

use serde::Deserialize;

// ─── stdin serde model ────────────────────────────────────────────────────────

/// JSON object Claude Code writes to the hook's stdin on Stop/SubagentStop/
/// SessionEnd/StopFailure.
///
/// Field names match the exact keys Claude Code sends (snake_case, per CC hook spec).
/// All fields are `Option` because the hook must exit 0 cleanly on a missing session_id;
/// absent optional fields should degrade gracefully rather than hard-error.
///
/// The last three fields feed tier-0 (see the module doc's Tier-0 section).
/// They deserialize leniently via [`lenient_opt_string`]: Claude Code's
/// documented shape has them as strings, but a payload where one of them
/// shows up as some other JSON type (an object, a number, ...) must still
/// parse — the field just reads back as `None` rather than aborting the
/// whole hook invocation.
///
/// Claude Code also sends `last_assistant_message` (the text of the model's
/// last reply) on Stop, SubagentStop, and StopFailure. It is deliberately not
/// deserialized, so no conversation text can end up in `limit-switch.log`.
#[derive(Debug, Clone, Deserialize)]
pub struct HookInput {
    /// Session UUID. Required for any action; exit 0 silently when absent.
    pub session_id: Option<String>,

    /// Working directory at the time of the hook event.
    pub cwd: Option<String>,

    /// Reason string from CC: one of "stop", "clear", "logout", "prompt_input_exit",
    /// "exit", etc. The reason gate maps some values to notify-only.
    pub reason: Option<String>,

    /// Path to the `.jsonl` transcript file for this session. Claude Code
    /// still sends this on every event (part of the stdin contract in the
    /// module doc above); nothing in this crate reads it since the tier-1 and
    /// tier-3 transcript scanners that used to were deleted.
    #[allow(dead_code)]
    pub transcript_path: Option<String>,

    /// Event name: "Stop" | "SubagentStop" | "SessionEnd" | "StopFailure" | ...
    /// Absent on hook payload shapes that predate this field (older fixtures,
    /// and any event CC doesn't tag) — defaults to `None`, which tier-0 treats
    /// as "not a StopFailure".
    #[serde(default, deserialize_with = "lenient_opt_string")]
    pub hook_event_name: Option<String>,

    /// StopFailure only: the error-kind enum. Documented values include
    /// "rate_limit", "overloaded", "authentication_failed",
    /// "oauth_org_not_allowed", "account_on_hold", "verification_required",
    /// "billing_error", "invalid_request", "model_not_found", server errors,
    /// and "unknown".
    #[serde(default, deserialize_with = "lenient_opt_string")]
    pub error: Option<String>,

    /// StopFailure only: free-text detail from the API, if any.
    #[serde(default, deserialize_with = "lenient_opt_string")]
    pub error_details: Option<String>,
}

/// Deserialize an optional string field leniently: missing, `null`, or any
/// non-string JSON value (object, number, array, bool) becomes `None` instead
/// of a hard parse error. Used for the StopFailure-only [`HookInput`] fields,
/// whose exact shape is API free text we don't fully control.
fn lenient_opt_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(deserializer)?;
    Ok(v.as_str().map(str::to_string))
}

// ─── limit dimension ──────────────────────────────────────────────────────────

/// Which usage window a limit verdict was tripped by. `Unknown` covers the
/// one signal this crate genuinely cannot attribute to a specific window:
/// tier-0 (`StopFailure`'s payload carries no dimension at all — the API
/// error kind is the same `rate_limit` string whichever window tripped it).
/// Tier-2 ([`usage_threshold_hit_at`]) tags a real dimension directly; the
/// statusline tick carries its own typed [`LimitHit`] straight through
/// (`classify_with`'s `live_limit` parameter — this is the dimension the
/// fable-fallback check at step 5b needs, since the statusline tick is the
/// operative switch path for the model-scoped weekly cap; see the module
/// doc's "Statusline tick" section). The caller already has the typed
/// [`LimitHit`] from
/// [`statusline_limit_hit`]/[`statusline_limit_hit_default`], so
/// `classify_with` takes the dimension directly rather than re-parsing it
/// back out of the reduced message string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LimitDimension {
    Session,
    WeekAll,
    WeekFable,
    Unknown,
}

/// A limit verdict paired with the dimension that tripped it, and — for a
/// `WeekFable` hit only — the reset epoch of that window. Introduced so a
/// future caller can branch on `dimension` without re-parsing `message`'s
/// free text — see the module's `LimitDimension` doc. `resets_at` lets 5b's
/// marker-staleness check (see [`model_fallback_marker_is_stale`]) use the
/// epoch the producer already read off the usage section, instead of a
/// second, separate usage-cache read: `None` for every dimension except
/// `WeekFable`, and `None` even for a `WeekFable` hit whose reading never
/// carried a reset time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LimitHit {
    pub dimension: LimitDimension,
    pub message: String,
    pub resets_at: Option<i64>,
}

// ─── decision type ────────────────────────────────────────────────────────────

/// Outcome of [`classify`] — what the hook should do.
#[derive(Debug)]
pub enum Decision {
    /// No action — a kill-switch, cooldown, hop guard, or managed-gate check
    /// determined the hook should do nothing.
    Skip,

    /// Notify-only variant. Used for:
    ///   - user-quit + limited (deduped via .detected)
    ///   - no viable target profile (deduped via .detected)
    ///   - detect-only mode CLAUDE_AUTO_SWITCH_RELAUNCH=0 (deduped via .detected)
    ///   - unmanaged session (no pidfile) (deduped via .detected)
    NotifyOnly {
        /// OSC 777 body to emit on stdout.
        message: String,
    },

    /// Full limit-switch: emit notify + write sentinel + stop the supervisor.
    LimitSwitch {
        /// OSC 777 body to emit on stdout.
        message: String,
        /// Profile to switch to.
        target_profile: String,
        /// Handoff prompt for the resumed session.
        handoff: String,
        /// Working directory (from hook input cwd, falling back to empty string).
        cwd: String,
        /// Born epoch from the PID file (carried into the sentinel).
        born: i64,
        /// The dimension that tripped this verdict. Not read by any caller
        /// yet — both `run_with_input` and `run_from_statusline` bind it as
        /// `dimension: _` — this is the plumbing a later commit branches on.
        #[allow(dead_code)]
        dimension: LimitDimension,
        /// `Some(model)` when this is a same-account model fallback (a
        /// `week_fable` cap with the fallback enabled and not already used
        /// this session — see [`fable_fallback_model`]) rather than an
        /// account switch; `target_profile` then equals the current profile.
        /// `None` is the ordinary account-switch path. `commit_and_stop`
        /// branches on this to skip the account-switch bookkeeping (hop bump,
        /// `.switched`, `.last-switch`) and write the one-shot
        /// `.model-fallback` marker instead.
        model_override: Option<String>,
    },
}

// ─── constants ────────────────────────────────────────────────────────────────

/// Maximum number of hops before breaking the relaunch chain.
pub const MAX_HOPS: i64 = 1;

/// Machine-wide cooldown in seconds after a limit-switch.
pub const LAST_SWITCH_COOLDOWN_SECS: i64 = 300;

/// Usage threshold percent (>=99 means "at the limit").
pub const LIMIT_PCT: i64 = 99;

// ─── reason gate ─────────────────────────────────────────────────────────────

/// Returns true if the `reason` field indicates a user-initiated quit.
/// These cases suppress the relaunch but still emit a notify (deduped).
///
/// Shell source: the legacy shell implementation:
///   `case "$reason" in clear|logout|prompt_input_exit|exit) user_quit=1 ;; esac`
pub fn is_user_quit_reason(reason: &str) -> bool {
    matches!(reason, "clear" | "logout" | "prompt_input_exit" | "exit")
}

// ─── public API ───────────────────────────────────────────────────────────────

/// Parse the hook's stdin as a [`HookInput`] JSON object.
///
/// Returns `Ok(HookInput{session_id: None, ..})` on empty stdin (CC may send empty
/// body in some edge cases); returns `Err` only on a structural JSON parse failure
/// that is clearly not a valid hook payload.
pub fn parse_stdin() -> anyhow::Result<HookInput> {
    use std::io::Read as _;
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    parse_input(&buf)
}

/// Parse one hook-shaped JSON document. Shared by [`parse_stdin`] and the
/// statusline tick ([`crate::hook::run_from_statusline`]): Claude Code's
/// statusLine stdin carries the same `session_id`/`cwd`/`transcript_path`
/// keys as a hook event (plus `hook_event_name: "Status"`, `model`,
/// `workspace`, `rate_limits`, ... which are ignored here), so one parser
/// serves both. Whitespace-only input parses to an all-`None` value rather
/// than an error.
pub fn parse_input(raw: &str) -> anyhow::Result<HookInput> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(HookInput {
            session_id: None,
            cwd: None,
            reason: None,
            transcript_path: None,
            hook_event_name: None,
            error: None,
            error_details: None,
        });
    }
    let input: HookInput = serde_json::from_str(trimmed)?;
    Ok(input)
}

/// Classify the hook event given the parsed input and the owner profile dir.
///
/// Reproduces the full legacy shell implementation's flow exactly, plus a tier-0 step
/// (StopFailure) the shell source predates:
///
/// 1. Kill-switches 1a/1b (env, file) — cheap, dimension-independent
/// 2. Reason gate (user_quit flag — note: does NOT short-circuit yet, detection still runs)
/// 3. Detect (tier-0 StopFailure / tier-2) — a tier-0 `NotLimit` verdict
///    exits immediately with `Skip`, bypassing tier-2
/// 4. No limit signal → Skip
///
/// 4b. Kill-switch 1c: a `.switched` marker for this session blocks every
/// dimension except `WeekFable` while the fallback is enabled — see
/// [`switched_marker_blocks`]. Moved here (rather than into step 1) because
/// it needs the dimension `Detect` just produced.
///
/// 5. user_quit + limited → NotifyOnly (deduped via .detected)
///
/// 5b. Fable-cap same-account model fallback check (see [`fable_fallback_model`]):
/// on `Some(model)`, `target_profile` is fixed to the current profile and
/// steps 6, 9, and 10 are all skipped.
///
/// 5c. On `None` from a `week_fable` dimension while the one-shot marker is
/// still current for this profile and fresh, the trip is silently suppressed
/// (`Decision::Skip`) rather than notified or falling through — see the
/// module doc's 5c section, [`model_fallback_marker_is_stale`] and
/// [`model_fallback_marker_is_current`]. `None` from `session`/`week_all`
/// (or from a `week_fable` trip whose marker has gone stale, belongs to a
/// different profile, or with the fallback knob off) runs the existing path
/// below unchanged.
///
/// 6. Pick target profile (exclude current) via account::pick_account —
///    skipped by 5b; no viable target → NotifyOnly (deduped via .detected)
/// 7. CLAUDE_AUTO_SWITCH_RELAUNCH != "1" → NotifyOnly (deduped via .detected)
/// 8. Managed-session gate (.pid file)
/// 9. Machine-wide cooldown (noclobber .last-switch) — skipped entirely by a
///    5b fallback; otherwise skipped when the tier-0 signal was
///    `definitive` (see `cooldown_should_block`)
/// 10. Hop guard — skipped by 5b
/// 11. Build the handoff prompt — 5b gets its own wording (see
///     [`build_fallback_handoff`])
///     → LimitSwitch
pub fn classify(input: &HookInput, owner_dir: &Path) -> anyhow::Result<Decision> {
    classify_with(input, owner_dir, None)
}

/// [`classify`] with a limit the caller has already established from evidence
/// the hook stdin does not carry.
///
/// `live_limit = Some(hit)` is the statusline tick's case
/// ([`crate::hook::run_from_statusline`]): the owning profile's live
/// `rate_limits` reading crossed `CLAUDE_LIMIT_PCT`, and `hit` carries the
/// reduced message (`"week_all 100%"`, ...) already tagged with the typed
/// dimension that tripped it — the caller already has this from
/// [`statusline_limit_hit_default`], so `classify_with` takes it directly
/// instead of re-parsing the message string. It short-circuits step 3
/// exactly like a tier-0 hit — limited, `definitive`, no transcript or
/// usage-cache read — and is definitive for the same reason
/// tier-0 is: it is that session's own account, read off its own API
/// responses seconds ago, not a shared cache. Every other step
/// (kill-switches, reason gate, target pick, relaunch switch, managed gate,
/// cooldown, hop guard) applies unchanged. `None` is the ordinary hook path.
pub fn classify_with(
    input: &HookInput,
    owner_dir: &Path,
    live_limit: Option<&LimitHit>,
) -> anyhow::Result<Decision> {
    use crate::paths;

    let sid = match &input.session_id {
        Some(s) if !s.is_empty() => s.as_str(),
        _ => return Ok(Decision::Skip),
    };

    // ── 1. Kill-switches 1a/1b (cheapest checks first) ────────────────────────
    // Shell: the legacy shell implementation
    // 1c (the `.switched` marker) is deferred to step 4b below — it needs the
    // tripped dimension, which isn't known yet at this point.
    if kill_switches_engaged() {
        return Ok(Decision::Skip);
    }

    // ── 2. Reason gate (set flag, DON'T exit yet) ─────────────────────────────
    // Shell: the legacy shell implementation
    // NOTE: the shell (post-2026-06-10 fix) does NOT exit here — it sets user_quit
    // and continues so that detection still runs; detection + user_quit → notify-only.
    let user_quit = input
        .reason
        .as_deref()
        .map(is_user_quit_reason)
        .unwrap_or(false);

    // ── 3. Detect (tier-0 StopFailure / tier-2) ──────────────────────────────
    // Shell: the legacy shell implementation (tier-0 has no shell analogue —
    // StopFailure is a hook event class the shell implementation predates).
    // Tier-0 is pure over the already-parsed input (no I/O at all); tier-2
    // uses the local usage cache.
    //
    // `definitive` tracks whether the signal came from tier-0: it feeds the
    // cooldown-exception decision at step 9 (a definitive signal is never
    // throttled by the machine-wide cooldown — see `cooldown_should_block`).

    // ── 4. No limit → exit (no side effects) ─────────────────────────────────
    // Shell: the legacy shell implementation
    let (limited_msg, definitive, dimension, week_fable_resets_at) =
        match detect_limit(input, owner_dir, live_limit) {
            Detection::NotLimit => {
                // StopFailure for a non-limit API error (overloaded,
                // authentication_failed, invalid_request, ...). Not something to
                // switch accounts over — do nothing, and do NOT fall through to
                // tier-2: that reads usage-cache state that has nothing to do with
                // this failure and could produce an unrelated false positive on
                // the same turn.
                return Ok(Decision::Skip);
            }
            Detection::NoSignal => {
                if user_quit {
                    // Shell: `_log "user-quit-skip" "reason=${reason}"`
                    // No notification — this is just a log entry when no limit detected.
                }
                return Ok(Decision::Skip);
            }
            Detection::Limited {
                message,
                definitive,
                dimension,
                resets_at,
            } => (message, definitive, dimension, resets_at),
        };

    // ──────────────── Limit detected from this point on ──────────────────────

    // ── 4b. Kill-switch 1c: `.switched` (deferred until the dimension is known) ──
    // Shell: the legacy shell implementation blocked on `.switched` at step 1
    // unconditionally; this crate defers that block to here so a `WeekFable`
    // trip — which never switches an account itself — isn't stopped by a
    // switch an earlier, different-dimension trip already made on this same
    // session. See `switched_marker_blocks`'s doc and the module doc's 4b
    // section.
    if paths::switched(sid).exists() && switched_marker_blocks(dimension, fable_fallback_enabled())
    {
        return Ok(Decision::Skip);
    }

    // ── 5. User-quit + limited → one-shot notify (deduped via .detected) ─────
    // Shell: the legacy shell implementation
    // NEVER kill/relaunch on a session the user explicitly closed.
    if user_quit {
        let profile_name = owner_dir_to_profile_name(owner_dir);
        let body = format!(
            "[{profile_name}] hit {limited_msg} — you quit, so not relaunching; next csm will pick a healthy account"
        );
        return Ok(notify_once(sid, body));
    }

    let current_profile = owner_dir_to_profile_name(owner_dir);

    // ── 5b. Fable-cap same-account model fallback ─────────────────────────────
    // A model-scoped weekly cap (week_fable) does not mean the account is out
    // of budget — every OTHER model on it is still fine, so switching accounts
    // would waste a healthy account's hop budget for no reason. Stay on this
    // account and relaunch the same session on the fallback model instead,
    // unless the knob is off or this session already used its one-shot
    // fallback. Checked before the target pick (step 6) so a Fable-only cap
    // never excludes an otherwise-healthy account or consumes the
    // account-switch hop guard (step 10). See `fable_fallback_model`'s own doc
    // for the full loop-safety reasoning.
    // Only a `week_fable` trip can ever set `already_fell_back` — reading the
    // marker, let alone judging its staleness or which profile it belongs to,
    // cannot change the outcome for a `session`/`week_all` trip, so skip the
    // I/O entirely for those: no marker file read on this profile's
    // smart_dir, and (since `week_fable_resets_at` below came from the same
    // `LimitHit` that already told us we were limited — see `LimitHit`'s doc)
    // no separate usage-cache read either, even on a genuine `week_fable`
    // trip. That matters most on the statusline tick, which must stay free
    // of any usage I/O beyond what detecting the trip itself already
    // required.
    //
    // The marker belongs to the account it was written on (see
    // `model_fallback_marker_is_current`'s doc): a marker written while this
    // session was on a DIFFERENT profile does not count,
    // exactly like a stale one, because an ordinary account-switch cap
    // (`session`/`week_all`) can move this session off the account the
    // marker was written for without ever touching `.model-fallback` itself.
    let already_fell_back = if dimension == LimitDimension::WeekFable {
        match model_fallback_marker_read(sid) {
            Some((marker_epoch, marker_profile)) => {
                let current = model_fallback_marker_is_current(
                    &marker_profile,
                    &current_profile,
                    marker_epoch,
                    week_fable_resets_at,
                );
                if !current {
                    // Belongs to an earlier week_fable window, or to a
                    // profile this session has since left. Remove it now, at
                    // the moment it's judged not current, so a fresh
                    // fallback's exclusive claim
                    // (`stop::claim_model_fallback`) lands on an empty slot
                    // instead of the noclobber claim needlessly failing
                    // closed against a leftover file from a window or
                    // account that no longer applies.
                    let _ = std::fs::remove_file(paths::model_fallback(sid));
                }
                current
            }
            // No marker at all, OR one present that doesn't parse (garbage —
            // a hand edit, or an older marker format; see
            // `model_fallback_marker_read`'s doc). Both read as "no fallback
            // recorded for the current window" rather than an
            // `.exists()`-only check, which would read a corrupt marker as
            // "definitely already fell back" and, under 5c, bar the session
            // from ever falling back again — the only consumer of "marker
            // present" is the staleness-bounded suppression below, and an
            // unreadable marker proves nothing about the current window.
            // Best-effort remove any corrupt leftover too, so the self-heal
            // is real: a plain existence check on this same path a moment
            // from now (in `stop::claim_model_fallback`'s exclusive claim)
            // must not fail closed against garbage it can never parse away.
            None => {
                let _ = std::fs::remove_file(paths::model_fallback(sid));
                false
            }
        }
    } else {
        false
    };
    let fallback_model = fable_fallback_model(
        dimension,
        fable_fallback_enabled(),
        already_fell_back,
        &fable_fallback_model_name(),
    );

    // ── 5c. Suppress a repeat WeekFable trip within the same fallback window ──
    // Silent — `Decision::Skip`, not a notify-only — because the fallback
    // itself was already logged when it fired, so a repeat trip has nothing
    // new to say, and a notify-only here would consume the session's one-shot
    // `.detected` slot for nothing, silently swallowing a later, genuinely
    // new notify-only for the rest of this session (see the module doc's 5c
    // section). Only a `week_fable` trip whose one-shot marker is still fresh
    // is suppressed here — `session`/`week_all` never set `already_fell_back`,
    // a stale marker already reads as `already_fell_back == false` above (so
    // `fallback_model` is `Some` and this branch is never reached), and the
    // knob-off case is excluded by `fable_fallback_enabled()` so a
    // knob-disabled fleet still falls through to the ordinary account-switch
    // path exactly as if this fallback feature didn't exist.
    if dimension == LimitDimension::WeekFable
        && fallback_model.is_none()
        && already_fell_back
        && fable_fallback_enabled()
    {
        return Ok(Decision::Skip);
    }

    // ── 6. Pick target profile (exclude current, reactive hook mode) ──────────
    // Shell: the legacy shell implementation
    //
    // Gate OFF (apply_stale_gate=false): the proactive launch path refuses to
    // auto-pick on stale usage (it has a picker fallback), but this hook fires
    // ONLY because the current profile already hit a limit and is
    // non-interactive. The tier-2 limit *detection* already bypasses the gate
    // (it reads current_usage directly), so blocking the *target-pick* on the
    // same stale data would leave the user stranded on the limited profile —
    // detected-but-not-switched. Score on the freshest-known numbers instead.
    //
    // Skipped entirely for a 5b fallback: the target IS the current profile,
    // by construction, never a pick result.
    let target_profile = if fallback_model.is_some() {
        current_profile.clone()
    } else {
        match pick_target(&current_profile) {
            Some(name) => name,
            None => {
                // No viable target (all saturated/errored, or fetch miss)
                // Shell: the legacy shell implementation
                let body = format!(
                    "[{current_profile}] hit {limited_msg} — no account with headroom to switch to"
                );
                return Ok(notify_once(sid, body));
            }
        }
    };

    // ── 7. Detect-only mode (CLAUDE_AUTO_SWITCH_RELAUNCH != "1") ─────────────
    // Shell: the legacy shell implementation
    // Default is "1" (relaunch enabled). Explicit =0 → notify-only.
    // MUST run before any state mutation — does NOT claim .switched or cooldown.
    if !relaunch_enabled() {
        let sid_short = crate::hook::sid_short(sid);
        // A 5b fallback never switches accounts (`target_profile ==
        // current_profile`), so the notify text must say so and name the
        // `--model` flag a manual resume actually needs — "switch to
        // [x]" naming the account the user is already on would be both
        // pointless and, missing `--model`, resume the session back onto
        // the capped model.
        let body = match &fallback_model {
            Some(model) => format!(
                "[{current_profile}] hit {limited_msg} → resume on model [{model}], same account (auto-relaunch OFF; csm --profile {current_profile} --resume {sid_short} --model {model})"
            ),
            None => format!(
                "[{current_profile}] hit {limited_msg} → switch to [{target_profile}] (auto-relaunch OFF; csm --profile {target_profile} --resume {sid_short})"
            ),
        };
        return Ok(notify_once(sid, body));
    }

    // ── 8. Managed-session gate: .pid file must exist and match claude/node ───
    // Shell: the legacy shell implementation
    let born_epoch = match managed_session(sid) {
        ManagedGate::NotManaged => {
            let sid_short = crate::hook::sid_short(sid);
            // Same reasoning as step 7's notify text above: a fallback names
            // the model to resume on, not an account switch to itself.
            let body = match &fallback_model {
                Some(model) => format!(
                    "[{current_profile}] hit {limited_msg} → resume on model [{model}], same account (csm --profile {current_profile} --resume {sid_short} --model {model})"
                ),
                None => format!(
                    "[{current_profile}] hit {limited_msg} → switch to [{target_profile}] by hand (csm --profile {target_profile} --resume {sid_short})"
                ),
            };
            return Ok(notify_once(sid, body));
        }
        // Bad PID file, or PID not a live claude/node process — log and skip.
        ManagedGate::Dead => return Ok(Decision::Skip),
        ManagedGate::Live { born } => born,
    };

    // ── 9. Machine-wide cooldown (atomic noclobber claim) ─────────────────────
    // Shell: the legacy shell implementation
    // Claim with noclobber; if fails, check if within cooldown window.
    //
    // Skipped entirely for a 5b fallback (`fallback_model.is_some()`): a
    // same-account model change consumes no shared resource, so it must
    // neither claim this machine-wide cooldown nor be blocked by another
    // session's recent switch. `.last-switch` is a cross-session throttle on
    // real account switches; stamping it here would needlessly block an
    // unrelated session's own switch for up to `CLAUDE_SWITCH_COOLDOWN`
    // seconds, and being blocked by it would silently drop a fallback that
    // should always happen (see `fable_fallback_model`'s loop-safety doc —
    // `.last-switch` is explicitly named as a mechanism the fallback must
    // NOT use).
    //
    // Exception for a non-fallback call (Rust-side addition, no shell
    // analogue): a definitive tier-0 signal (StopFailure rate_limit) is
    // never blocked by this cooldown. The cooldown exists to stop one
    // runaway session from hammering the switch machinery on
    // stale/borderline pct data; it was never meant to gate a SECOND
    // session's own, independent, fresh confirmation that its account just
    // hit a hard limit. Several csm-supervised sessions can share one
    // account — when its all-model weekly cap saturates, every one of them
    // gets its own StopFailure(rate_limit) on its next turn, and each must
    // be allowed to switch off it rather than have all but the first be
    // silently Skip'd and stranded in the auto-continue wait for days. We
    // still call `check_and_claim_cooldown` unconditionally (for a
    // non-fallback call) so the stamp gets refreshed/claimed best-effort —
    // pct-based (tier-2) detections elsewhere keep today's throttle exactly.
    if fallback_model.is_none() {
        paths::smart_dir()?; // ensure dir exists before cooldown_blocks touches it
        if cooldown_blocks(definitive) {
            return Ok(Decision::Skip);
        }
    }

    // ── 10. Hop guard ─────────────────────────────────────────────────────────
    // Shell: the legacy shell implementation
    //
    // Skipped for a 5b fallback: it doesn't touch the account-switch hop
    // budget (see `fable_fallback_model`'s doc) — the hop carried into the
    // handoff/sentinel stays at whatever this session already recorded.
    let next_hop = if fallback_model.is_some() {
        crate::hook::read_sidecar_hop(sid)
    } else {
        match next_hop_within_cap(sid) {
            Some(n) => n,
            None => return Ok(Decision::Skip),
        }
    };

    // ── 11. Build the handoff prompt ──────────────────────────────────────────
    // Shell: the legacy shell implementation
    //
    // A 5b fallback gets its own handoff string: `build_handoff` phrases
    // "switched from [current] to [target] account" and a hop number, which
    // is untrue and stale when `target_profile == current_profile` (see
    // `build_fallback_handoff`'s own doc).
    let sid_short = crate::hook::sid_short(sid);
    let handoff = match &fallback_model {
        Some(model) => build_fallback_handoff(sid_short, &current_profile, model),
        None => build_handoff(sid_short, &current_profile, &target_profile, next_hop),
    };

    let message = if let Some(model) = &fallback_model {
        format!(
            "[{current_profile}] hit {limited_msg} → falling back to model [{model}] (same account)"
        )
    } else {
        format!(
            "[{current_profile}] hit {limited_msg} → switching to [{target_profile}] (hop {next_hop})"
        )
    };
    let cwd_str = input
        .cwd
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| ".".to_string());

    Ok(Decision::LimitSwitch {
        message,
        target_profile,
        handoff,
        cwd: cwd_str,
        born: born_epoch,
        dimension,
        model_override: fallback_model,
    })
}

// ─── `classify_with` gate steps ───────────────────────────────────────────────
//
// Each fn below covers exactly the banner(s) named in its doc comment, split
// with no new seams beyond `classify_with`'s own pre-existing section
// boundaries.

/// Step 1 (banners 1a/1b): the two kill-switches that do not depend on the
/// tripped dimension, cheapest first. Kill-switch 1c runs at step 4b.
fn kill_switches_engaged() -> bool {
    use crate::paths;

    // 1a. Env var kill-switch: CLAUDE_AUTO_SWITCH=0
    if std::env::var("CLAUDE_AUTO_SWITCH").as_deref() == Ok("0") {
        return true;
    }

    // 1b. File-based kill-switch: .auto-switch-disabled
    if paths::smart_dir_no_create()
        .join(".auto-switch-disabled")
        .exists()
    {
        return true;
    }

    // 1c (the `.switched` marker) is checked separately, at step 4b, once the
    // tripped dimension is known — see `switched_marker_blocks`.
    false
}

/// Step 4b (kill-switch 1c, deferred): does an existing `.switched` marker
/// block this trip?
///
/// `true` for every dimension except `WeekFable` while the fallback is
/// enabled. 1c exists to cap how often ONE session can switch accounts; a
/// `WeekFable` trip that goes on to fall back at 5b never switches an
/// account, spends no hop, and is guarded by its own one-shot
/// `.model-fallback` marker instead, so an EARLIER, different-dimension
/// switch's `.switched` marker must not strand it — that is the stranding
/// bug this predicate fixes (see the module doc's 4b section). With the
/// fallback disabled (`enabled == false`), a `WeekFable` trip is blocked here
/// exactly like any other dimension: the whole fallback feature is meant to
/// be inert with the knob off, so `.switched` must keep doing what it always
/// did.
pub(crate) fn switched_marker_blocks(dimension: LimitDimension, enabled: bool) -> bool {
    !(dimension == LimitDimension::WeekFable && enabled)
}

/// Outcome of the tier-0/tier-2 limit check (banners 3-4).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Detection {
    /// A StopFailure event whose `error` is not `"rate_limit"` — a real API
    /// failure, but not a usage limit. `classify_with` must Skip, never fall
    /// through to tier-2.
    NotLimit,
    /// Neither tier-0 nor tier-2 saw a limit.
    NoSignal,
    /// A limit was detected. `definitive` is true for tier-0 (StopFailure or
    /// the statusline tick's `live_limit`), false for tier-2 (usage-cache
    /// threshold). `dimension` is `LimitDimension::Unknown` for the true
    /// StopFailure case (its payload carries no dimension at all); the
    /// `live_limit` case carries its real dimension straight through from the
    /// caller's own [`LimitHit`] (no re-parsing), and tier-2 tags its real
    /// dimension via `detect_usage_threshold_hit`. `resets_at` is the
    /// `WeekFable` window's reset epoch when `dimension` is `WeekFable` and
    /// the producer had one on hand, `None` otherwise — see [`LimitHit`]'s
    /// doc.
    Limited {
        message: String,
        definitive: bool,
        dimension: LimitDimension,
        resets_at: Option<i64>,
    },
}

/// Steps 3-4: tier-0 StopFailure / tier-2 usage-cache check.
fn detect_limit(input: &HookInput, owner_dir: &Path, live_limit: Option<&LimitHit>) -> Detection {
    match (live_limit, stop_failure_limit(input)) {
        (Some(hit), _) => Detection::Limited {
            message: hit.message.clone(),
            definitive: true,
            dimension: hit.dimension,
            resets_at: hit.resets_at,
        },
        (None, StopFailureVerdict::Limit(msg)) => Detection::Limited {
            message: msg,
            definitive: true,
            dimension: LimitDimension::Unknown,
            resets_at: None,
        },
        (None, StopFailureVerdict::NotLimit) => Detection::NotLimit,
        (None, StopFailureVerdict::NotApplicable) => match detect_usage_threshold_hit(owner_dir) {
            Some(hit) => Detection::Limited {
                message: hit.message,
                definitive: false,
                dimension: hit.dimension,
                resets_at: hit.resets_at,
            },
            None => Detection::NoSignal,
        },
    }
}

/// Banners 5/6/7/8 tails: the one-shot notify deduped via the `.detected`
/// noclobber marker, shared by the four exits that used to repeat it inline.
fn notify_once(sid: &str, body: String) -> Decision {
    use crate::paths;

    let detect_path = paths::detected(sid);
    if !detect_path.exists() {
        let _ = paths::smart_dir(); // ensure dir exists
        let _ = write_noclobber_epoch(&detect_path);
        prune_detected_markers();
        return Decision::NotifyOnly { message: body };
    }
    Decision::Skip
}

/// Step 6: pick a target profile, excluding `current_profile`, with the
/// stale-usage gate off (reactive hook mode — see the caller's comment on
/// why this call site must not apply that gate).
fn pick_target(current_profile: &str) -> Option<String> {
    let target_result = crate::account::pick_account_gated(
        current_profile,
        /*include_current=*/ false,
        /*apply_stale_gate=*/ false,
    );
    // Single viability authority: `resolve_target_from_pick` is a pure pass-through
    // over `account::pick_account_gated`'s verdict — the hook never recomputes or
    // second-guesses which profile is viable. Whatever `scoring::pick_best`
    // excludes (session-limited or week_all-saturated — `week_fable` no
    // longer constrains viability at all, see `scoring::is_viable_pcts`'s
    // doc) can therefore never come back as a relaunch target.
    resolve_target_from_pick(target_result)
}

/// Step 7: `CLAUDE_AUTO_SWITCH_RELAUNCH` defaults to `"1"` (relaunch
/// enabled); any other value means detect-only (notify, don't relaunch).
fn relaunch_enabled() -> bool {
    std::env::var("CLAUDE_AUTO_SWITCH_RELAUNCH").unwrap_or_else(|_| "1".to_string()) == "1"
}

// ─── fable fallback (model-scoped weekly cap, same-account) ─────────────────

/// `CLAUDE_FABLE_FALLBACK` defaults to `"1"` (fallback enabled); any other
/// value disables it, so a `week_fable` trip falls through to the ordinary
/// account-switch path, whose target pick no longer excludes a
/// Fable-saturated account either.
fn fable_fallback_enabled() -> bool {
    std::env::var("CLAUDE_FABLE_FALLBACK").unwrap_or_else(|_| "1".into()) == "1"
}

/// `CLAUDE_FABLE_FALLBACK_MODEL` defaults to `"opus"` — the alias for the
/// latest Opus, never a pinned model id, so nothing here needs updating when
/// Anthropic ships a new Opus.
fn fable_fallback_model_name() -> String {
    std::env::var("CLAUDE_FABLE_FALLBACK_MODEL").unwrap_or_else(|_| "opus".into())
}

/// Step 5b's pure decision core: should this trip fall back to a model on the
/// SAME account instead of switching accounts?
///
/// `Some(fallback_model)` only when ALL of:
/// - `dimension == LimitDimension::WeekFable` — the model-scoped weekly cap.
///   A `Session` or `WeekAll` trip is never model-scoped, so Opus on the same
///   account would free nothing; those always fall through to `None` and the
///   ordinary account-switch path.
/// - `enabled` — `CLAUDE_FABLE_FALLBACK` (see [`fable_fallback_enabled`]).
/// - `!already_fell_back` — this session hasn't used its one-shot fallback
///   yet (`<sid>.model-fallback`, see [`crate::paths::model_fallback`]).
///
/// # Loop safety
///
/// `week_fable` is carried forward from the last usage-API probe
/// (`src/usage/local/statusline.rs`), so after a fallback relaunch the SAME
/// capped reading is still on record and would trip the next statusline tick
/// again. Two existing anti-loop mechanisms do not help here: `.switched`
/// would burn the session's account-switch budget for good if the fallback
/// wrote it (it's for the wrong kind of relaunch), and `.last-switch` is a
/// machine-wide cooldown that a same-account model change has no business
/// stamping (it would needlessly throttle other sessions' real account
/// switches) — `commit_and_stop` and `classify_with`'s cooldown check both
/// skip it for a fallback. So the fallback gets its own one-shot marker,
/// checked here via `already_fell_back`: once it is set (and still fresh —
/// see [`model_fallback_marker_is_stale`]), this function returns `None` for
/// any further `WeekFable` trip.
///
/// That `None` IS special-cased by the caller (`classify_with`'s step 5c —
/// see the module doc's 5c section): a further `WeekFable` trip while the
/// marker is still fresh is silently suppressed (`Decision::Skip`), not
/// escalated to the ordinary account-switch path and not notified again,
/// because `week_fable` stays capped for days and falling through would undo
/// the fallback on the very next tick. It is bounded, not permanent — once
/// the marker goes stale, `already_fell_back` is `false` again and this
/// function resumes returning `Some`. `Session` and `WeekAll` never set
/// `already_fell_back` in the first place, so they are unaffected either way
/// and keep falling through to the ordinary account-switch path.
pub(crate) fn fable_fallback_model(
    dimension: LimitDimension,
    enabled: bool,
    already_fell_back: bool,
    fallback_model: &str,
) -> Option<String> {
    if dimension == LimitDimension::WeekFable && enabled && !already_fell_back {
        Some(fallback_model.to_string())
    } else {
        None
    }
}

/// Is a `<sid>.model-fallback` marker written at `marker_epoch` stale — from
/// an earlier `week_fable` weekly window rather than the current one? Without
/// this, 5c's suppression (see the module doc) would be permanent: a
/// `--resume`d session keeps its session id indefinitely, so the marker
/// (keyed by sid, never deleted) would bar that session from ever falling
/// back again, even after the capped window has long since reset.
///
/// The weekly window is 7 days and `week_fable_resets_at` is the END of the
/// CURRENT window, so a marker written before `resets_at - 7*86400` was
/// written during an EARLIER window and must be treated as absent — that
/// earlier fallback spent its one shot against a cap that has since reset,
/// not this one. `saturating_sub` keeps that subtraction in range for an
/// extreme `resets_at` near `i64::MIN` instead of overflowing (a debug build
/// panics on overflow).
///
/// `week_fable_resets_at: None` (no reset epoch known — the profile's
/// `week_fable` reading has never carried one) never expires the marker;
/// current behaviour, unchanged. There is no window boundary to compare
/// against, and guessing one would risk expiring a marker that is genuinely
/// still within its window.
pub(crate) fn model_fallback_marker_is_stale(
    marker_epoch: i64,
    week_fable_resets_at: Option<i64>,
) -> bool {
    match week_fable_resets_at {
        Some(resets_at) => marker_epoch < resets_at.saturating_sub(7 * 86_400),
        None => false,
    }
}

/// Does a `<sid>.model-fallback` marker written on `marker_profile` at
/// `marker_epoch` still count as "already fell back" for `current_profile`?
///
/// Sibling to [`model_fallback_marker_is_stale`], which handles the OTHER
/// staleness axis (the weekly window elapsing). Both must hold for the
/// marker to suppress a further fallback: it must be for the SAME account
/// the session is on now, and not stale.
///
/// Without the profile check, a marker survives an ordinary account switch
/// it never recorded: session S falls back to a model on account A, writing
/// the marker; a later `week_all` cap on A switches S to B; the marker is
/// still on disk (the fallback path never touches it, and an account switch
/// never clears it) and still fresh, so if B's own `week_fable` later trips,
/// A's marker would wrongly suppress the fallback S needs on B — 5c would
/// fire silently and S would be stuck on a capped model with the actual
/// fallback path never having run for its current account. Treating a
/// foreign-profile marker exactly like a stale one — it doesn't count, it's
/// removed, a fresh one may be written — closes that gap.
pub(crate) fn model_fallback_marker_is_current(
    marker_profile: &str,
    current_profile: &str,
    marker_epoch: i64,
    week_fable_resets_at: Option<i64>,
) -> bool {
    marker_profile == current_profile
        && !model_fallback_marker_is_stale(marker_epoch, week_fable_resets_at)
}

/// I/O shell for [`model_fallback_marker_is_current`]: read
/// `<sid>.model-fallback`'s stored `<epoch> <profile>`, if the marker exists
/// and parses. `None` covers "no marker" and "marker present but
/// unparseable" alike — both the initial claim
/// ([`crate::hook::stop::claim_model_fallback`]) and every later refresh
/// ([`crate::hook::stop::commit_and_stop`]) write the exact same
/// `"<epoch> <profile>"` shape, atomically (private tmp file hard-linked or
/// renamed into place), so a reader never observes a half-written file, or
/// one with an epoch but no profile, from either path; an unparseable marker
/// here means something else wrote or edited the file directly.
/// `classify_with` treats all of those cases the same way, as "no fallback
/// recorded for the current window on this profile", rather than trying to
/// tell them apart.
fn model_fallback_marker_read(sid: &str) -> Option<(i64, String)> {
    use crate::paths;
    let content = std::fs::read_to_string(paths::model_fallback(sid)).ok()?;
    let mut parts = content.trim().splitn(2, ' ');
    let epoch: i64 = parts.next()?.parse().ok()?;
    let profile = parts.next()?.trim();
    if profile.is_empty() {
        return None;
    }
    Some((epoch, profile.to_string()))
}

/// Outcome of the managed-session gate (banner 8).
#[derive(Debug, Clone, PartialEq, Eq)]
enum ManagedGate {
    /// No `.pid` file — nothing csm is supervising for this session.
    NotManaged,
    /// A `.pid` file exists but is malformed, or names a PID that is no
    /// longer a live claude/node process.
    Dead,
    /// A `.pid` file names a live claude/node process; carries its recorded
    /// birth epoch.
    Live { born: i64 },
}

/// Step 8: `.pid` file must exist and name a live claude/node process.
fn managed_session(sid: &str) -> ManagedGate {
    use crate::paths;

    let pid_path = paths::pid_file(sid);
    if !pid_path.exists() {
        return ManagedGate::NotManaged;
    }

    // Read the pid file: "<pid> <born_epoch>"
    let pid_content = std::fs::read_to_string(&pid_path).unwrap_or_default();
    let (claude_pid, born_epoch) = match parse_pid_file(&pid_content) {
        Some(v) => v,
        None => return ManagedGate::Dead,
    };

    // Confirm the PID is a live claude/node process
    if !is_live_claude_or_node(claude_pid) {
        return ManagedGate::Dead;
    }

    ManagedGate::Live { born: born_epoch }
}

/// Step 9: machine-wide cooldown (atomic noclobber claim), with the
/// tier-0-definitive exception. Always claims/refreshes the stamp via
/// `check_and_claim_cooldown`, even when the result is "don't block" —
/// see `classify_with`'s call site comment for why.
fn cooldown_blocks(definitive: bool) -> bool {
    use crate::paths;

    let last_switch_path = paths::last_switch();
    let cooldown_secs = crate::envvar::i64_or("CLAUDE_SWITCH_COOLDOWN", LAST_SWITCH_COOLDOWN_SECS);
    let window_blocked = check_and_claim_cooldown(&last_switch_path, cooldown_secs);
    cooldown_should_block(definitive, window_blocked)
}

/// Step 10: hop guard. `None` means the cap is reached; `Some(n)` is the next
/// hop number to record.
fn next_hop_within_cap(sid: &str) -> Option<i64> {
    let max_hops = crate::envvar::i64_or("CLAUDE_MAX_HOPS", MAX_HOPS);
    let current_hop = crate::hook::read_sidecar_hop(sid);
    if current_hop >= max_hops {
        None
    } else {
        Some(current_hop + 1)
    }
}

// ─── detection tier implementations ──────────────────────────────────────────

/// Maximum length, in characters, of the `error_details` snippet embedded in
/// a tier-0 message. Keeps a possibly-large free-text API string out of logs
/// and notify bodies.
pub const STOP_FAILURE_DETAIL_MAX_CHARS: usize = 120;

/// Outcome of the tier-0 StopFailure check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StopFailureVerdict {
    /// `hook_event_name == "StopFailure" && error == "rate_limit"` — the
    /// definitive, zero-latency usage-limit signal. Carries the
    /// human-readable message to surface to the user/notify body.
    Limit(String),
    /// A StopFailure event with any OTHER `error` value. A real API failure,
    /// but not a usage limit — classify() must do nothing (`Decision::Skip`)
    /// rather than fall through to tier-2, which looks at usage-cache
    /// state unrelated to this specific failure.
    NotLimit,
    /// Not a StopFailure event at all (or `hook_event_name` absent, as on
    /// every ordinary Stop/SubagentStop/SessionEnd payload) — tier-0 has no
    /// opinion; classify() falls through to the tier-2 check.
    NotApplicable,
}

/// Tier-0: does this event report a StopFailure whose `error` is
/// `"rate_limit"`?
///
/// Pure over `&HookInput` — no I/O, no env reads. This is the operative
/// detector at the exact moment an account hits a usage limit: Claude Code's
/// query loop runs the StopFailure hook (fire-and-forget, not awaited) instead
/// of Stop for that turn, before parking the prompt in its "Usage limit
/// reached" auto-continue wait — see the module doc's Tier-0 section. Requires
/// the hook to be registered on the `StopFailure` event with matcher
/// `rate_limit` (a companion change outside this crate); if it isn't, this fn
/// is simply never reached with a StopFailure payload and tier-2 remains the
/// detector, as today.
pub(crate) fn stop_failure_limit(input: &HookInput) -> StopFailureVerdict {
    if input.hook_event_name.as_deref() != Some("StopFailure") {
        return StopFailureVerdict::NotApplicable;
    }
    if input.error.as_deref() != Some("rate_limit") {
        return StopFailureVerdict::NotLimit;
    }
    let detail = input.error_details.as_deref().unwrap_or("");
    let detail = single_line_truncate(detail, STOP_FAILURE_DETAIL_MAX_CHARS);
    let msg = if detail.is_empty() {
        "usage limit (StopFailure rate_limit)".to_string()
    } else {
        format!("usage limit (StopFailure rate_limit: {detail})")
    };
    StopFailureVerdict::Limit(msg)
}

/// Collapse newlines/tabs to spaces and truncate to at most `max_chars`
/// **characters** (not bytes) — char-boundary safe, so multibyte UTF-8 input
/// (Korean error text, emoji, ...) is never split mid-codepoint and can never
/// panic, unlike a byte-index slice.
fn single_line_truncate(s: &str, max_chars: usize) -> String {
    s.chars()
        .map(|c| {
            if c == '\n' || c == '\r' || c == '\t' {
                ' '
            } else {
                c
            }
        })
        .take(max_chars)
        .collect()
}

/// Pure decision: should the machine-wide cooldown block this switch?
///
/// A `definitive` (tier-0) signal is never blocked — see the step-9 comment
/// in [`classify`] for why. Every other detection tier keeps today's
/// behavior exactly: blocked iff the raw window check (`window_blocked`,
/// from [`check_and_claim_cooldown`]) says so.
pub(crate) fn cooldown_should_block(definitive: bool, window_blocked: bool) -> bool {
    if definitive {
        return false;
    }
    window_blocked
}

/// Tier-2: query the usage cache for the owning profile and check whether
/// session%, week_all%, or the model-scoped week_fable% is at or above the
/// configured threshold.
///
/// Returns `Some(hit)` if any dimension is exceeded, `None` otherwise.
///
/// I/O shell: fetches the fleet's [`crate::usage::UsageData`] once and hands the
/// three raw pcts to the pure [`usage_threshold_hit_at`] decision core (invariant:
/// pure core + thin I/O shell). `week_fable_pct` is `None` when the profile
/// carries no model-scoped weekly cap (e.g. `week_fable` is absent) — that
/// dimension then never constrains the verdict.
///
/// Reproduces the tier-2 block from the legacy shell implementation, extended
/// with the week_fable dimension (never a hardcoded model name — the fleet's
/// `week_fable` field name is what's fixed, the model it measures is data,
/// carried separately in `week_model_label`). Named `_hit` (rather than kept
/// as a bare `detect_usage_threshold`) because [`detect_limit`] needs the
/// tagged [`LimitHit`], not just its message — there is no remaining caller
/// that wants the String-only form, so no message-returning twin exists here
/// (contrast [`usage_threshold_hit`], whose twin is kept for its own
/// directly-tested `Option<String>` contract). For a `WeekFable` hit,
/// `resets_at` is patched in afterward from the same profile lookup — see
/// [`LimitHit`]'s doc.
fn detect_usage_threshold_hit(owner_dir: &Path) -> Option<LimitHit> {
    let profile = owner_dir_to_profile_name(owner_dir);
    let data = crate::usage::fetch().ok()?;
    let (session_pct, week_pct) = data.current_usage(&profile)?;
    let week_fable = data
        .profiles
        .get(&profile)
        .and_then(|pu| pu.week_fable.as_ref());
    let week_fable_pct = week_fable.map(|s| s.pct);

    let limit_pct = crate::envvar::i64_or("CLAUDE_LIMIT_PCT", LIMIT_PCT);

    let mut hit = usage_threshold_hit_at(session_pct, week_pct, week_fable_pct, limit_pct)?;
    if hit.dimension == LimitDimension::WeekFable {
        hit.resets_at = week_fable.and_then(|s| s.resets_at);
    }
    Some(hit)
}

/// The statusline tick's limit test: reduce the profile's merged reading
/// (this tick's `session`/`week_all` plus the store's carried-forward
/// `week_fable`) to the same three-dimension check tier-2 uses, returning the
/// typed [`LimitHit`] — what [`crate::hook::run_from_statusline`] passes
/// straight through to `classify_with`'s `live_limit` parameter, instead of
/// re-parsing the dimension back out of a message string. For a `WeekFable`
/// hit, `resets_at` is filled straight from `usage.week_fable`'s own reading
/// — no extra I/O, since that section is already in memory (see [`LimitHit`]'s
/// doc, and the module doc's "Statusline tick" section on why this path must
/// stay I/O-free). Absent `session`/`week_all` map to `-1` so they never
/// fire, matching the shell contract [`usage_threshold_hit`] documents. Pure,
/// so the tick's whole decision about *whether to even run `classify`* is
/// unit-tested here.
pub(crate) fn statusline_limit_hit_default(
    usage: &crate::usage::model::ProfileUsage,
) -> Option<LimitHit> {
    let limit_pct = crate::envvar::i64_or("CLAUDE_LIMIT_PCT", LIMIT_PCT);
    statusline_limit_hit(usage, limit_pct)
}

/// [`statusline_limit_hit_default`] with the threshold passed in, for
/// tests — the typed reduction, same rule as tier-2, with the threshold as a
/// parameter instead of read from `CLAUDE_LIMIT_PCT`.
pub(crate) fn statusline_limit_hit(
    usage: &crate::usage::model::ProfileUsage,
    limit_pct: i64,
) -> Option<LimitHit> {
    let pct = |s: &Option<crate::usage::model::UsageSection>| s.as_ref().map_or(-1, |s| s.pct);
    let mut hit = usage_threshold_hit_at(
        pct(&usage.session),
        pct(&usage.week_all),
        usage.week_fable.as_ref().map(|s| s.pct),
        limit_pct,
    )?;
    if hit.dimension == LimitDimension::WeekFable {
        hit.resets_at = usage.week_fable.as_ref().and_then(|s| s.resets_at);
    }
    Some(hit)
}

/// Pure decision core for tier-2: given the three usage-pct dimensions and the
/// threshold, decide whether a limit was hit.
///
/// Shell: the legacy shell implementation (session/week_all only — week_fable
/// is a Rust-side addition, same rule: threshold only on non-negative integers,
/// so absent-as-`-1` never fires, and `None` — no model-scoped cap at all for
/// this profile — never fires either).
///
/// Checked in order: session, then week_all, then week_fable (first hit wins;
/// order only affects which description string comes back, not whether one
/// does).
///
/// No production call site uses this `Option<String>` form
/// directly ([`detect_usage_threshold_hit`] and [`statusline_limit_hit`] both
/// call [`usage_threshold_hit_at`] instead so they can keep the dimension
/// tag) — this wrapper is kept solely so the older tests below stay
/// byte-for-byte unmodified.
#[allow(dead_code)]
pub(crate) fn usage_threshold_hit(
    session_pct: i64,
    week_pct: i64,
    week_fable_pct: Option<i64>,
    limit_pct: i64,
) -> Option<String> {
    usage_threshold_hit_at(session_pct, week_pct, week_fable_pct, limit_pct).map(|hit| hit.message)
}

/// [`usage_threshold_hit`]'s pure core, extended to tag which dimension
/// tripped: same three-dimension check (session, then week_all, then
/// week_fable — first hit wins), but returns a [`LimitHit`] instead of
/// collapsing straight to a message string.
pub(crate) fn usage_threshold_hit_at(
    session_pct: i64,
    week_pct: i64,
    week_fable_pct: Option<i64>,
    limit_pct: i64,
) -> Option<LimitHit> {
    if session_pct >= 0 && session_pct >= limit_pct {
        return Some(LimitHit {
            dimension: LimitDimension::Session,
            message: format!("session {session_pct}%"),
            resets_at: None,
        });
    }
    if week_pct >= 0 && week_pct >= limit_pct {
        return Some(LimitHit {
            dimension: LimitDimension::WeekAll,
            message: format!("week_all {week_pct}%"),
            resets_at: None,
        });
    }
    if let Some(fable_pct) = week_fable_pct
        && fable_pct >= 0
        && fable_pct >= limit_pct
    {
        return Some(LimitHit {
            dimension: LimitDimension::WeekFable,
            message: format!("week_fable {fable_pct}%"),
            resets_at: None,
        });
    }
    None
}

// ─── helpers ─────────────────────────────────────────────────────────────────

/// Resolve the relaunch target from an `account::pick_account`-family scoring
/// result — the single point where classify() decides "who do we switch to".
///
/// `Ok(Some(name))` → `name` IS the target; this function never recomputes,
/// re-ranks, or filters it — whatever viability rule `scoring::pick_best`
/// applies (session/week_all — `week_fable` no longer constrains viability,
/// see `scoring::is_viable_pcts`'s doc) is the ONLY rule that ever runs.
/// `Ok(None)` (no-op winner, not expected with `include_current=false` but
/// handled defensively) or `Err(_)` (all saturated / fetch failed) → `None`,
/// meaning no viable target: the caller must fall back to notify-only rather
/// than writing a sentinel that would relaunch into a capped or unknown
/// profile.
pub(crate) fn resolve_target_from_pick(
    result: crate::account::scoring::ScoringResult,
) -> Option<String> {
    match result {
        Ok(Some(name)) => Some(name),
        Ok(None) | Err(_) => None,
    }
}

/// Derive the profile name from the owner dir by taking the last path segment
/// and stripping the `.claude.` prefix.
///
/// e.g. `/Users/example/.claude.home` → `"home"`
///      `/Users/example/.claude.work` → `"work"`
///      (unknown dir) → use the last segment as-is
pub(crate) fn owner_dir_to_profile_name(owner_dir: &Path) -> String {
    let seg = owner_dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
    // Strip leading `.claude.` prefix if present
    if let Some(stripped) = seg.strip_prefix(".claude.") {
        stripped.to_string()
    } else {
        seg.to_string()
    }
}

/// Parse `"<pid> <born>"` from a pid file. Returns `None` on any parse failure.
/// Re-exports the liveness module's implementation.
fn parse_pid_file(content: &str) -> Option<(u32, i64)> {
    let mut tokens = content.split_whitespace();
    let pid: u32 = tokens.next()?.parse().ok()?;
    let born: i64 = tokens.next()?.parse().ok()?;
    Some((pid, born))
}

/// Check if pid is a live claude/node process. Delegates to platform hook (stop.rs).
fn is_live_claude_or_node(pid: u32) -> bool {
    // Reuse the platform implementation from stop.rs via the public helper.
    crate::hook::stop::check_is_live_claude_or_node(pid)
}

/// Build the handoff prompt string.
///
/// Shell: the legacy shell implementation:
///   `HANDOFF="이전 세션(${session_id:0:8})이 사용량 한도에 걸려 [${current_profile}]에서 [${target_profile}] 계정으로 자동 전환됐어. (hop ${next_hop}) 직전까지 하던 작업을 그대로 이어서 진행해줘."`
/// Unless overridden by CLAUDE_SMART_RESUME_PROMPT or suppressed (empty string).
///
/// `${CLAUDE_SMART_RESUME_PROMPT-resume}` semantics:
///   - unset → "resume"
///   - empty ("") → disabled (return empty string)
///   - set to a value → that value
///     BUT the Korean handoff from the shell is the DEFAULT WHEN TRIGGERING A SWITCH.
///     We use the Korean string as the contextual handoff and CLAUDE_SMART_RESUME_PROMPT
///     as an override (empty = suppress).
pub(crate) fn build_handoff(
    sid_short: &str,
    current_profile: &str,
    target_profile: &str,
    next_hop: i64,
) -> String {
    // Check if CLAUDE_SMART_RESUME_PROMPT explicitly suppresses
    match std::env::var("CLAUDE_SMART_RESUME_PROMPT") {
        Ok(v) if v.is_empty() => {
            // Explicit empty = suppress handoff
            String::new()
        }
        Ok(v) => {
            // Explicit non-empty override
            v
        }
        Err(_) => {
            // Unset = use the Korean default handoff string
            format!(
                "이전 세션({sid_short})이 사용량 한도에 걸려 [{current_profile}]에서 [{target_profile}] 계정으로 자동 전환됐어. (hop {next_hop}) 직전까지 하던 작업을 그대로 이어서 진행해줘."
            )
        }
    }
}

/// Build the handoff prompt string for a same-account model fallback (5b —
/// see [`fable_fallback_model`]), where `target_profile == current_profile`
/// by construction and there is no hop to report.
///
/// [`build_handoff`]'s default template says "switched from [current] to
/// [target] account (hop N)"; fed the same profile on both sides it would
/// read as an account switch that never happened and repeat a hop number
/// nothing actually bumped. This is its own default string instead, still
/// honouring `CLAUDE_SMART_RESUME_PROMPT`'s empty-string-suppresses /
/// explicit-override semantics exactly like `build_handoff`.
pub(crate) fn build_fallback_handoff(
    sid_short: &str,
    current_profile: &str,
    model: &str,
) -> String {
    match std::env::var("CLAUDE_SMART_RESUME_PROMPT") {
        Ok(v) if v.is_empty() => {
            // Explicit empty = suppress handoff
            String::new()
        }
        Ok(v) => {
            // Explicit non-empty override
            v
        }
        Err(_) => {
            // Unset = use the Korean default fallback-handoff string
            format!(
                "이전 세션({sid_short})이 [{current_profile}] 계정의 모델별 주간 한도에 걸려 {model} 모델로 자동 전환됐어 (계정은 그대로 유지). 직전까지 하던 작업을 그대로 이어서 진행해줘."
            )
        }
    }
}

/// Write the current epoch to `path` with noclobber semantics (first write wins).
/// Returns Ok(()) regardless of whether the write happened.
fn write_noclobber_epoch(path: &Path) -> anyhow::Result<()> {
    use std::fs::OpenOptions;
    use std::io::Write as _;

    let epoch = now_epoch().to_string();
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut f) => {
            let _ = f.write_all(epoch.as_bytes());
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // First write wins, silently skip.
        }
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

/// Prune `.detected` markers older than 7 days across the smart dir.
/// Best-effort: any error is silently ignored.
/// Shell: `find "$SMART_DIR" -maxdepth 1 -name '*.detected' -mtime +7 -delete`
fn prune_detected_markers() {
    use crate::paths;
    let smart_dir = paths::smart_dir_no_create();
    let seven_days_secs = 7 * 24 * 3600u64;
    let now = std::time::SystemTime::now();
    let Ok(entries) = std::fs::read_dir(&smart_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if !name_str.ends_with(".detected") {
            continue;
        }
        if let Ok(meta) = entry.metadata()
            && let Ok(modified) = meta.modified()
            && let Ok(age) = now.duration_since(modified)
            && age.as_secs() > seven_days_secs
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Check the machine-wide cooldown and atomically claim the slot.
///
/// Returns `true` (blocked) if within the cooldown window.
/// Returns `false` (proceed) if the window has expired or we claimed the slot.
///
/// Shell: the legacy shell implementation:
///   - Try noclobber create `.last-switch` → if succeeds, we own it (proceed).
///   - If fails (already exists): read timestamp, check window.
///     - Within window → skip (return true).
///     - Outside window → overwrite (proceed, return false).
fn check_and_claim_cooldown(last_switch_path: &Path, cooldown_secs: i64) -> bool {
    use std::fs::OpenOptions;
    use std::io::Write as _;

    let now = now_epoch();
    let epoch_str = now.to_string();

    // Try noclobber create — if we win the race, proceed immediately.
    let created = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(last_switch_path)
    {
        Ok(mut f) => {
            let _ = f.write_all(epoch_str.as_bytes());
            true
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(_) => {
            // Unexpected error on create: proceed (fail open, same as shell behavior).
            return false;
        }
    };

    if created {
        // Atomically claimed the slot — proceed.
        return false;
    }

    // File exists — check timestamp.
    let last_ts = std::fs::read_to_string(last_switch_path)
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(0);

    if last_ts > 0 && (now - last_ts) < cooldown_secs {
        // Within cooldown — blocked.
        return true;
    }

    // Outside the window: overwrite to re-claim.
    // Shell: `date +%s > "$LAST_SWITCH"` (no noclobber, overwrites)
    let _ = std::fs::write(last_switch_path, &epoch_str);
    false
}

/// Return the current epoch in seconds (UNIX_EPOCH).
/// In tests, use injected time to avoid SystemTime::now().
#[cfg(not(test))]
pub(crate) fn now_epoch() -> i64 {
    crate::epoch::now_secs() as i64
}

/// In tests, we use a thread-local override for deterministic time.
#[cfg(test)]
pub(crate) fn now_epoch() -> i64 {
    TEST_NOW.with(|n| *n.borrow())
}

#[cfg(test)]
thread_local! {
    static TEST_NOW: std::cell::RefCell<i64> = const { std::cell::RefCell::new(1_718_000_000) };
}

#[cfg(test)]
pub(crate) fn set_test_now(epoch: i64) {
    TEST_NOW.with(|n| *n.borrow_mut() = epoch);
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    // ── HookInput serde tests ──────────────────────────────────────────────────

    /// HookInput deserializes from a typical CC hook payload.
    #[test]
    fn hook_input_full_payload() {
        let json = r#"{
            "session_id": "01234567-89ab-cdef-0123-456789abcdef",
            "cwd": "/Users/example/Projects/github.com/foo",
            "reason": "stop",
            "transcript_path": "/Users/example/.claude.shared/projects/-Users-example-Projects-github-com-foo/01234567-89ab-cdef-0123-456789abcdef.jsonl"
        }"#;
        let input: HookInput = serde_json::from_str(json).expect("deserialize full payload");
        assert_eq!(
            input.session_id.as_deref(),
            Some("01234567-89ab-cdef-0123-456789abcdef")
        );
        assert_eq!(
            input.cwd.as_deref(),
            Some("/Users/example/Projects/github.com/foo")
        );
        assert_eq!(input.reason.as_deref(), Some("stop"));
        assert!(input.transcript_path.is_some());
    }

    /// HookInput gracefully accepts missing optional fields.
    #[test]
    fn hook_input_minimal_payload() {
        let json = r#"{"session_id": "aaaabbbb-cccc-dddd-eeee-ffffaaaabbbb"}"#;
        let input: HookInput = serde_json::from_str(json).expect("deserialize minimal");
        assert_eq!(
            input.session_id.as_deref(),
            Some("aaaabbbb-cccc-dddd-eeee-ffffaaaabbbb")
        );
        assert!(input.cwd.is_none());
        assert!(input.reason.is_none());
        assert!(input.transcript_path.is_none());
    }

    /// HookInput with no session_id (empty object).
    #[test]
    fn hook_input_no_session_id() {
        let json = r#"{"cwd": "/tmp", "reason": "stop"}"#;
        let input: HookInput = serde_json::from_str(json).expect("deserialize no sid");
        assert!(input.session_id.is_none());
    }

    /// Empty stdin produces a HookInput with all None fields (no panic).
    #[test]
    fn hook_input_empty_string_round_trip() {
        let json = r#"{}"#;
        let input: HookInput = serde_json::from_str(json).expect("empty object");
        assert!(input.session_id.is_none());
        assert!(input.cwd.is_none());
        assert!(input.reason.is_none());
        assert!(input.transcript_path.is_none());
    }

    /// HookInput serde: extra unknown fields are ignored (forward-compat).
    #[test]
    fn hook_input_ignores_extra_fields() {
        let json = r#"{
            "session_id": "cafecafe-cafe-cafe-cafe-cafecafecafe",
            "cwd": "/tmp",
            "reason": "stop",
            "transcript_path": null,
            "future_field": "some_value",
            "another_extra": 42
        }"#;
        let input: HookInput = serde_json::from_str(json).expect("extra fields tolerated");
        assert_eq!(
            input.session_id.as_deref(),
            Some("cafecafe-cafe-cafe-cafe-cafecafecafe")
        );
        // transcript_path: null -> None
        assert!(input.transcript_path.is_none());
    }

    /// The statusline tick feeds `parse_input` Claude Code's statusLine stdin,
    /// which is a superset of the hook shape: the three keys the switch needs
    /// come through, `hook_event_name` reads "Status" (so tier-0 stays
    /// NotApplicable), and the statusLine-only keys are ignored.
    #[test]
    fn parse_input_statusline_payload_shape() {
        let json = r#"{
            "hook_event_name": "Status",
            "session_id": "01234567-89ab-cdef-0123-456789abcdef",
            "transcript_path": "/Users/example/.claude.shared/projects/foo/01234567.jsonl",
            "cwd": "/Users/example/Projects/foo",
            "model": {"id": "claude-fable-5-1", "display_name": "Fable 5.1"},
            "workspace": {"current_dir": "/Users/example/Projects/foo", "project_dir": "/Users/example/Projects/foo"},
            "version": "2.1.270",
            "output_style": {"name": "default"},
            "cost": {"total_cost_usd": 0.0},
            "context_window": {"total_input_tokens": 1, "context_window_size": 200000},
            "rate_limits": {
                "five_hour": {"used_percentage": 21.0, "resets_at": 1789398600},
                "seven_day": {"used_percentage": 100.0, "resets_at": 1789646400}
            }
        }"#;
        let input = parse_input(json).expect("statusline stdin parses as HookInput");
        assert_eq!(
            input.session_id.as_deref(),
            Some("01234567-89ab-cdef-0123-456789abcdef")
        );
        assert_eq!(input.cwd.as_deref(), Some("/Users/example/Projects/foo"));
        assert!(input.transcript_path.is_some());
        assert_eq!(input.hook_event_name.as_deref(), Some("Status"));
        assert!(input.reason.is_none());
        assert!(matches!(
            stop_failure_limit(&input),
            StopFailureVerdict::NotApplicable
        ));
    }

    #[test]
    fn parse_input_blank_is_all_none() {
        let input = parse_input("  \n").expect("blank input is not an error");
        assert!(input.session_id.is_none());
        assert!(input.hook_event_name.is_none());
    }

    #[test]
    fn parse_input_garbage_is_error() {
        assert!(parse_input("{not json").is_err());
    }

    /// `classify_with(.., Some(_))` still runs the session_id gate first: a
    /// live limit with no session to act on is a Skip, not a switch attempt.
    #[test]
    fn classify_with_live_limit_but_no_session_id_is_skip() {
        let input = parse_input(r#"{"cwd": "/Users/example/Projects/foo"}"#).unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let hit = LimitHit {
            dimension: LimitDimension::WeekAll,
            message: "week_all 100%".to_string(),
            resets_at: None,
        };
        let decision = classify_with(&input, dir.path(), Some(&hit)).unwrap();
        assert!(matches!(decision, Decision::Skip));
    }

    /// A realistic StopFailure(rate_limit) payload deserializes cleanly: the
    /// tier-0 fields are populated and the unread `last_assistant_message` key
    /// is ignored.
    #[test]
    fn hook_input_stop_failure_rate_limit_payload() {
        let json = r#"{
            "session_id": "01234567-89ab-cdef-0123-456789abcdef",
            "transcript_path": "/Users/example/.claude.shared/projects/foo/01234567-89ab-cdef-0123-456789abcdef.jsonl",
            "cwd": "/Users/example/Projects/github.com/foo",
            "permission_mode": "default",
            "hook_event_name": "StopFailure",
            "error": "rate_limit",
            "error_details": "5-hour limit reached, resets 21:00 Asia/Seoul",
            "last_assistant_message": "Sure, let me check that for you."
        }"#;
        let input: HookInput =
            serde_json::from_str(json).expect("deserialize StopFailure rate_limit payload");
        assert_eq!(input.hook_event_name.as_deref(), Some("StopFailure"));
        assert_eq!(input.error.as_deref(), Some("rate_limit"));
        assert_eq!(
            input.error_details.as_deref(),
            Some("5-hour limit reached, resets 21:00 Asia/Seoul")
        );
        // A StopFailure payload carries no "reason" field.
        assert!(input.reason.is_none());
    }

    /// An ordinary Stop payload (no StopFailure fields at all) still
    /// deserializes exactly as before — the new fields default to `None`.
    #[test]
    fn hook_input_stop_payload_without_new_fields() {
        let json = r#"{
            "session_id": "01234567-89ab-cdef-0123-456789abcdef",
            "cwd": "/Users/example/Projects/github.com/foo",
            "reason": "stop",
            "transcript_path": "/Users/example/.claude.shared/projects/foo/01234567-89ab-cdef-0123-456789abcdef.jsonl"
        }"#;
        let input: HookInput = serde_json::from_str(json).expect("deserialize plain Stop payload");
        assert_eq!(input.reason.as_deref(), Some("stop"));
        assert!(input.hook_event_name.is_none());
        assert!(input.error.is_none());
        assert!(input.error_details.is_none());
    }

    /// A malformed/unexpected StopFailure payload where `error` comes back as
    /// a JSON object (not a string) must still deserialize — the lenient
    /// deserializer treats the field as absent rather than erroring the whole
    /// hook invocation.
    #[test]
    fn hook_input_stop_failure_error_as_object_is_lenient() {
        let json = r#"{
            "session_id": "01234567-89ab-cdef-0123-456789abcdef",
            "hook_event_name": "StopFailure",
            "error": {"code": "rate_limit", "nested": true},
            "error_details": 42
        }"#;
        let input: HookInput =
            serde_json::from_str(json).expect("lenient deserialize with object-shaped error");
        assert_eq!(input.hook_event_name.as_deref(), Some("StopFailure"));
        assert!(
            input.error.is_none(),
            "object-shaped error must deserialize to None, not error out"
        );
        assert!(
            input.error_details.is_none(),
            "number-shaped error_details must deserialize to None"
        );
    }

    // ── reason gate tests ──────────────────────────────────────────────────────

    /// Reason gate: user-quit reasons map to notify-only.
    #[test]
    fn reason_gate_user_quit_reasons() {
        for reason in &["clear", "logout", "prompt_input_exit", "exit"] {
            assert!(
                is_user_quit_reason(reason),
                "expected {reason:?} to be a user-quit reason"
            );
        }
    }

    /// Reason gate: "stop" is NOT a user-quit reason (it triggers detection).
    #[test]
    fn reason_gate_stop_is_not_user_quit() {
        assert!(
            !is_user_quit_reason("stop"),
            "\"stop\" should not be a user-quit reason"
        );
    }

    /// Reason gate: unknown reasons are not user-quit (they go through detection).
    #[test]
    fn reason_gate_unknown_is_not_user_quit() {
        assert!(!is_user_quit_reason("unknown_event"));
        assert!(!is_user_quit_reason(""));
        assert!(!is_user_quit_reason("SubagentStop"));
    }

    // ── constant tests ─────────────────────────────────────────────────────────

    /// MAX_HOPS constant is 1 (the hop guard).
    #[test]
    fn max_hops_is_one() {
        assert_eq!(MAX_HOPS, 1);
    }

    /// LAST_SWITCH_COOLDOWN_SECS is 300 (the machine-wide cooldown).
    #[test]
    fn last_switch_cooldown_is_300() {
        assert_eq!(LAST_SWITCH_COOLDOWN_SECS, 300);
    }

    // ── owner_dir_to_profile_name tests ───────────────────────────────────────

    #[test]
    fn owner_dir_profile_name_home() {
        let p = Path::new("/Users/example/.claude.home");
        assert_eq!(owner_dir_to_profile_name(p), "home");
    }

    #[test]
    fn owner_dir_profile_name_work() {
        let p = Path::new("/home/you/.claude.work");
        assert_eq!(owner_dir_to_profile_name(p), "work");
    }

    #[test]
    fn owner_dir_profile_name_no_prefix() {
        // If no ".claude." prefix, use the last segment verbatim
        let p = Path::new("/home/you/mydir");
        assert_eq!(owner_dir_to_profile_name(p), "mydir");
    }

    // ── build_handoff tests ────────────────────────────────────────────────────

    #[test]
    fn build_handoff_default_korean() {
        crate::testenv::with_env_var("CLAUDE_SMART_RESUME_PROMPT", None, || {
            let h = build_handoff("01234567", "home", "work", 1);
            // Should contain Korean text
            assert!(h.contains("01234567"), "should contain sid_short: {h}");
            assert!(h.contains("home"), "should contain current profile: {h}");
            assert!(h.contains("work"), "should contain target profile: {h}");
            assert!(h.contains("hop 1"), "should contain hop: {h}");
        });
    }

    #[test]
    fn build_handoff_empty_suppresses() {
        crate::testenv::with_env_var("CLAUDE_SMART_RESUME_PROMPT", Some(""), || {
            let h = build_handoff("01234567", "home", "work", 1);
            assert!(h.is_empty(), "empty env var should suppress handoff");
        });
    }

    #[test]
    fn build_handoff_custom_override() {
        crate::testenv::with_env_var(
            "CLAUDE_SMART_RESUME_PROMPT",
            Some("custom prompt here"),
            || {
                let h = build_handoff("01234567", "home", "work", 1);
                assert_eq!(h, "custom prompt here");
            },
        );
    }

    // ── build_fallback_handoff (same-account model fallback, 5b) ─────────────

    /// The default fallback handoff must name the CURRENT profile once (not
    /// as a "from [x] to [y]" pair — there is only one account here), name
    /// the fallback model, and must NOT claim any account was switched or
    /// report a hop number.
    #[test]
    fn build_fallback_handoff_default_korean() {
        crate::testenv::with_env_var("CLAUDE_SMART_RESUME_PROMPT", None, || {
            let h = build_fallback_handoff("01234567", "home", "opus");
            assert!(h.contains("01234567"), "should contain sid_short: {h}");
            assert!(h.contains("home"), "should contain current profile: {h}");
            assert!(h.contains("opus"), "should contain fallback model: {h}");
            assert!(!h.contains("hop"), "a fallback never reports a hop: {h}");
        });
    }

    #[test]
    fn build_fallback_handoff_empty_suppresses() {
        crate::testenv::with_env_var("CLAUDE_SMART_RESUME_PROMPT", Some(""), || {
            let h = build_fallback_handoff("01234567", "home", "opus");
            assert!(h.is_empty(), "empty env var should suppress handoff");
        });
    }

    #[test]
    fn build_fallback_handoff_custom_override() {
        crate::testenv::with_env_var(
            "CLAUDE_SMART_RESUME_PROMPT",
            Some("custom prompt here"),
            || {
                let h = build_fallback_handoff("01234567", "home", "opus");
                assert_eq!(h, "custom prompt here");
            },
        );
    }

    // ── stop_failure_limit (pure tier-0 detector) ──────────────────────────────

    fn hook_input_stop_failure(error: Option<&str>, error_details: Option<&str>) -> HookInput {
        HookInput {
            session_id: Some("test-sid".to_string()),
            cwd: None,
            reason: None,
            transcript_path: None,
            hook_event_name: Some("StopFailure".to_string()),
            error: error.map(str::to_string),
            error_details: error_details.map(str::to_string),
        }
    }

    /// StopFailure(rate_limit) with error_details → a definitive Limit verdict
    /// whose message embeds the detail text.
    #[test]
    fn stop_failure_limit_rate_limit_hit() {
        let input = hook_input_stop_failure(Some("rate_limit"), Some("resets 21:00 Asia/Seoul"));
        let verdict = stop_failure_limit(&input);
        match verdict {
            StopFailureVerdict::Limit(msg) => {
                assert!(msg.contains("rate_limit"), "message: {msg}");
                assert!(msg.contains("resets 21:00 Asia/Seoul"), "message: {msg}");
            }
            other => panic!("expected Limit, got {other:?}"),
        }
    }

    /// StopFailure(rate_limit) with no error_details → still a definitive
    /// Limit verdict, just without a detail clause.
    #[test]
    fn stop_failure_limit_rate_limit_hit_no_details() {
        let input = hook_input_stop_failure(Some("rate_limit"), None);
        let verdict = stop_failure_limit(&input);
        match verdict {
            StopFailureVerdict::Limit(msg) => {
                assert!(msg.contains("rate_limit"), "message: {msg}");
            }
            other => panic!("expected Limit, got {other:?}"),
        }
    }

    /// StopFailure with any other error → NotLimit (do nothing; never falls
    /// through to tier-2).
    #[test]
    fn stop_failure_limit_other_error_is_not_limit() {
        for err in [
            "overloaded",
            "authentication_failed",
            "oauth_org_not_allowed",
            "account_on_hold",
            "verification_required",
            "billing_error",
            "invalid_request",
            "model_not_found",
            "unknown",
        ] {
            let input = hook_input_stop_failure(Some(err), None);
            assert_eq!(
                stop_failure_limit(&input),
                StopFailureVerdict::NotLimit,
                "error={err} should be NotLimit"
            );
        }
    }

    /// StopFailure with `error` entirely absent (malformed/unexpected shape)
    /// → NotLimit, not a panic and not a Limit.
    #[test]
    fn stop_failure_limit_missing_error_is_not_limit() {
        let input = hook_input_stop_failure(None, None);
        assert_eq!(stop_failure_limit(&input), StopFailureVerdict::NotLimit);
    }

    /// A non-StopFailure event (ordinary Stop) → NotApplicable, regardless of
    /// what the error-shaped fields happen to hold.
    #[test]
    fn stop_failure_limit_non_stop_failure_is_not_applicable() {
        let input = HookInput {
            session_id: Some("test-sid".to_string()),
            cwd: None,
            reason: Some("stop".to_string()),
            transcript_path: None,
            hook_event_name: None,
            error: None,
            error_details: None,
        };
        assert_eq!(
            stop_failure_limit(&input),
            StopFailureVerdict::NotApplicable
        );

        // Even a Stop event where `error` happens to be "rate_limit" (should
        // never happen, but tier-0 must key off hook_event_name, not error
        // alone) is NotApplicable.
        let input2 = hook_input_stop_failure(Some("rate_limit"), None);
        let input2 = HookInput {
            hook_event_name: Some("Stop".to_string()),
            ..input2
        };
        assert_eq!(
            stop_failure_limit(&input2),
            StopFailureVerdict::NotApplicable
        );
    }

    /// A long error_details string is truncated to
    /// [`STOP_FAILURE_DETAIL_MAX_CHARS`] characters in the resulting message.
    #[test]
    fn stop_failure_limit_truncates_long_details() {
        let long_detail = "x".repeat(500);
        let input = hook_input_stop_failure(Some("rate_limit"), Some(&long_detail));
        let StopFailureVerdict::Limit(msg) = stop_failure_limit(&input) else {
            panic!("expected Limit verdict");
        };
        // The embedded detail is capped at STOP_FAILURE_DETAIL_MAX_CHARS chars.
        let embedded_xs = msg.chars().filter(|c| *c == 'x').count();
        assert_eq!(embedded_xs, STOP_FAILURE_DETAIL_MAX_CHARS);
    }

    /// Multibyte UTF-8 (Korean) error_details truncates on a char boundary
    /// without panicking, and does not exceed the char cap.
    #[test]
    fn stop_failure_limit_truncates_multibyte_details_without_panic() {
        // Each character here is a multibyte Korean syllable; repeat well
        // past the truncation cap.
        let long_detail = "사용량 한도에 도달했습니다 ".repeat(20);
        let input = hook_input_stop_failure(Some("rate_limit"), Some(&long_detail));
        let StopFailureVerdict::Limit(msg) = stop_failure_limit(&input) else {
            panic!("expected Limit verdict");
        };
        // No panic getting here is the primary assertion; also sanity-check
        // the message is non-empty and well-formed UTF-8 (guaranteed by
        // `String`, but assert something meaningful was produced).
        assert!(msg.contains("rate_limit"));
        assert!(msg.chars().count() < long_detail.chars().count());
    }

    // ── cooldown gate tests ────────────────────────────────────────────────────

    #[test]
    fn cooldown_first_claimant_proceeds() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(".last-switch");
        // File doesn't exist — first claimant should proceed (returns false = not blocked)
        let blocked = check_and_claim_cooldown(&path, 300);
        assert!(!blocked, "first claimant should not be blocked");
        assert!(path.exists(), "last-switch file should be created");
    }

    #[test]
    fn cooldown_blocks_within_window() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(".last-switch");
        // Write a recent epoch (now - 60 s → well within 300 s cooldown)
        let base_now: i64 = 1_718_000_000;
        set_test_now(base_now);
        std::fs::write(&path, (base_now - 60).to_string()).unwrap();

        let blocked = check_and_claim_cooldown(&path, 300);
        assert!(blocked, "should be blocked within cooldown window");
    }

    #[test]
    fn cooldown_allows_after_window() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(".last-switch");
        // Write a stale epoch (now - 400 s → outside 300 s cooldown)
        let base_now: i64 = 1_718_000_000;
        set_test_now(base_now);
        std::fs::write(&path, (base_now - 400).to_string()).unwrap();

        let blocked = check_and_claim_cooldown(&path, 300);
        assert!(!blocked, "should not be blocked outside cooldown window");
    }

    // ── cooldown_should_block (pure tier-0 cooldown-exception decision) ───────

    /// A definitive (tier-0) signal is never blocked, even while the raw
    /// window check says blocked — this is the whole point of the exception:
    /// a second session sharing the same now-capped account must still be
    /// allowed to switch, not stranded behind the first session's cooldown.
    #[test]
    fn cooldown_should_block_definitive_overrides_window_blocked() {
        assert!(!cooldown_should_block(true, true));
    }

    /// A definitive signal is (trivially) also not blocked when the window
    /// itself was not blocked.
    #[test]
    fn cooldown_should_block_definitive_and_window_open() {
        assert!(!cooldown_should_block(true, false));
    }

    /// A non-definitive (tier-2) signal keeps today's behavior
    /// exactly: blocked iff the window says blocked.
    #[test]
    fn cooldown_should_block_non_definitive_follows_window() {
        assert!(cooldown_should_block(false, true));
        assert!(!cooldown_should_block(false, false));
    }

    // ── sentinel hop increment test ────────────────────────────────────────────

    /// Verify that the hop counter increments correctly when read from a sidecar.
    #[test]
    fn hop_increments_from_sidecar() {
        // A sidecar with hop="1" (string form from old zsh merge_sidecar)
        let sidecar_json = r#"{"sessionId":"test","hop":"1","permissionMode":"default"}"#;
        let val: serde_json::Value = serde_json::from_str(sidecar_json).unwrap();
        let hop = match val.get("hop") {
            Some(serde_json::Value::String(s)) => s.parse::<i64>().unwrap_or(0),
            Some(serde_json::Value::Number(n)) => n.as_i64().unwrap_or(0),
            _ => 0,
        };
        assert_eq!(hop, 1);
        assert_eq!(hop + 1, 2, "next_hop should be 2");
    }

    /// born from PID file is passed through correctly.
    #[test]
    fn born_passthrough_from_pidfile() {
        let content = "12345 1718000000\n";
        let (pid, born) = parse_pid_file(content).unwrap();
        assert_eq!(pid, 12345);
        assert_eq!(born, 1_718_000_000);
    }

    // ── statusline_limit_hit (the tick's pre-gate) ──────────────────────────

    fn reading(
        session: Option<i64>,
        week_all: Option<i64>,
        week_fable: Option<i64>,
    ) -> crate::usage::model::ProfileUsage {
        use crate::usage::model::UsageSection;
        let sec = |pct: i64| UsageSection {
            pct,
            resets: None,
            resets_at: None,
        };
        crate::usage::model::ProfileUsage {
            session: session.map(sec),
            week_all: week_all.map(sec),
            week_fable: week_fable.map(sec),
            ..Default::default()
        }
    }

    #[test]
    fn statusline_limit_hit_all_healthy_is_none() {
        assert_eq!(
            statusline_limit_hit(&reading(Some(21), Some(80), Some(90)), 99),
            None
        );
    }

    #[test]
    fn statusline_limit_hit_week_all_capped() {
        // The live case: session fine, all-model weekly at 100.
        assert_eq!(
            statusline_limit_hit(&reading(Some(21), Some(100), Some(100)), 99),
            Some(LimitHit {
                dimension: LimitDimension::WeekAll,
                message: "week_all 100%".to_string(),
                resets_at: None,
            })
        );
    }

    #[test]
    fn statusline_limit_hit_session_capped_wins_over_week() {
        assert_eq!(
            statusline_limit_hit(&reading(Some(99), Some(100), None), 99),
            Some(LimitHit {
                dimension: LimitDimension::Session,
                message: "session 99%".to_string(),
                resets_at: None,
            })
        );
    }

    /// statusLine stdin never carries the model-scoped window; it rides in
    /// from the store's carried-forward record and must still fire, tagged
    /// with `LimitDimension::WeekFable`, not just a reduced message string.
    #[test]
    fn statusline_limit_hit_tags_week_fable() {
        assert_eq!(
            statusline_limit_hit(&reading(Some(10), Some(60), Some(100)), 99),
            Some(LimitHit {
                dimension: LimitDimension::WeekFable,
                message: "week_fable 100%".to_string(),
                resets_at: None,
            })
        );
    }

    /// A `WeekFable` hit's `resets_at` is threaded straight from
    /// `usage.week_fable`'s own reading, with no extra I/O — the whole point
    /// of carrying it on `LimitHit` (see the module's `LimitHit` doc).
    #[test]
    fn statusline_limit_hit_carries_week_fable_resets_at() {
        use crate::usage::model::UsageSection;
        let usage = crate::usage::model::ProfileUsage {
            week_fable: Some(UsageSection {
                pct: 100,
                resets: None,
                resets_at: Some(1_789_646_400),
            }),
            ..Default::default()
        };
        assert_eq!(
            statusline_limit_hit(&usage, 99),
            Some(LimitHit {
                dimension: LimitDimension::WeekFable,
                message: "week_fable 100%".to_string(),
                resets_at: Some(1_789_646_400),
            })
        );
    }

    /// A `Session` or `WeekAll` hit never carries `resets_at`, even when the
    /// tripped dimension's own section has one — only a `WeekFable` hit's
    /// epoch is ever meaningful to the marker-staleness check that reads it.
    #[test]
    fn statusline_limit_hit_session_hit_never_carries_resets_at() {
        use crate::usage::model::UsageSection;
        let usage = crate::usage::model::ProfileUsage {
            session: Some(UsageSection {
                pct: 99,
                resets: None,
                resets_at: Some(1_789_646_400),
            }),
            ..Default::default()
        };
        assert_eq!(
            statusline_limit_hit(&usage, 99),
            Some(LimitHit {
                dimension: LimitDimension::Session,
                message: "session 99%".to_string(),
                resets_at: None,
            })
        );
    }

    #[test]
    fn statusline_limit_hit_absent_sections_never_fire() {
        assert_eq!(statusline_limit_hit(&reading(None, None, None), 99), None);
        assert_eq!(
            statusline_limit_hit(&reading(None, None, Some(50)), 99),
            None
        );
    }

    #[test]
    fn statusline_limit_hit_honours_threshold() {
        assert_eq!(
            statusline_limit_hit(&reading(Some(50), Some(10), None), 50),
            Some(LimitHit {
                dimension: LimitDimension::Session,
                message: "session 50%".to_string(),
                resets_at: None,
            })
        );
    }

    // ── usage_threshold_hit (pure tier-2 core, R1/R3 week_fable dimension) ────

    /// session at/over the threshold fires, regardless of week_all/week_fable.
    #[test]
    fn usage_threshold_hit_session_capped() {
        let hit = usage_threshold_hit(99, 10, Some(5), 99);
        assert_eq!(hit.as_deref(), Some("session 99%"));
    }

    /// week_all at/over the threshold fires when session is healthy.
    #[test]
    fn usage_threshold_hit_week_all_capped() {
        let hit = usage_threshold_hit(10, 99, None, 99);
        assert_eq!(hit.as_deref(), Some("week_all 99%"));
    }

    /// week_fable (the model-scoped weekly cap) at/over the threshold fires
    /// even when session and week_all are both healthy — this is the R1/R2 gap:
    /// a profile whose model-scoped weekly cap alone is exhausted must still be
    /// detected as limited.
    #[test]
    fn usage_threshold_hit_week_fable_capped() {
        let hit = usage_threshold_hit(10, 20, Some(99), 99);
        assert_eq!(hit.as_deref(), Some("week_fable 99%"));
    }

    /// week_fable == None (profile has no model-scoped cap at all) must never
    /// be treated as "limited" — the dimension simply does not constrain the
    /// verdict when the API returned no such limit for this profile.
    #[test]
    fn usage_threshold_hit_week_fable_none_does_not_fire() {
        let hit = usage_threshold_hit(10, 20, None, 99);
        assert!(
            hit.is_none(),
            "None week_fable must not be treated as capped"
        );
    }

    /// week_fable present but under the threshold (e.g. 94% against a 99%
    /// limit) must not fire — only None is exempt, not "close but under".
    #[test]
    fn usage_threshold_hit_week_fable_under_threshold_does_not_fire() {
        let hit = usage_threshold_hit(10, 20, Some(94), 99);
        assert!(
            hit.is_none(),
            "week_fable under the threshold must not fire"
        );
    }

    /// All three dimensions healthy → no hit.
    #[test]
    fn usage_threshold_hit_all_healthy_no_hit() {
        let hit = usage_threshold_hit(10, 20, Some(30), 99);
        assert!(hit.is_none());
    }

    /// Absent session (-1 sentinel) never fires on its own, even at a
    /// nominally "over threshold" negative value — mirrors the pre-existing
    /// session/week_all absent-encoding rule, now also proven for week_fable
    /// interacting with the other two dimensions in the same call.
    #[test]
    fn usage_threshold_hit_absent_session_and_fable_skip_to_week_all() {
        let hit = usage_threshold_hit(-1, 99, Some(-1), 99);
        assert_eq!(
            hit.as_deref(),
            Some("week_all 99%"),
            "absent session must not mask a real week_all hit"
        );
    }

    /// week_fable is checked last: a capped session or week_all is reported
    /// first even when week_fable is ALSO capped (description reflects the
    /// first true dimension, viability is the same either way).
    #[test]
    fn usage_threshold_hit_session_reported_before_fable() {
        let hit = usage_threshold_hit(99, 10, Some(99), 99);
        assert_eq!(hit.as_deref(), Some("session 99%"));
    }

    // ── detect_usage_threshold_hit (the Stop-hook tier-2 producer) ────────────

    /// The Stop-hook tier-2 path must carry `week_fable`'s own `resets_at`
    /// onto the `LimitHit` it produces, exactly like `statusline_limit_hit`
    /// does for the statusline path — 5b's staleness check and 2's
    /// per-account currency check both key off it, so a hit with `resets_at:
    /// None` can never expire the one-shot fallback marker.
    #[test]
    fn detect_usage_threshold_hit_week_fable_carries_resets_at() {
        let _guard = crate::testenv::lock_for("CSM_USAGE_CMD");
        let home = tempfile::TempDir::new().unwrap();

        let mut profiles = std::collections::HashMap::new();
        profiles.insert(
            "limited".to_string(),
            crate::usage::model::ProfileUsage {
                week_fable: Some(crate::usage::model::UsageSection {
                    pct: 100,
                    resets: None,
                    resets_at: Some(1_700_000_000),
                }),
                ..Default::default()
            },
        );
        let usage = crate::usage::model::UsageData {
            captured_at: None,
            profiles,
            errors: None,
            ..Default::default()
        };
        let usage_file = home.path().join("usage-cmd.json");
        std::fs::write(&usage_file, serde_json::to_string(&usage).unwrap()).unwrap();

        let prior_cmd = std::env::var_os("CSM_USAGE_CMD");
        crate::testenv::set_var("CSM_USAGE_CMD", &format!("cat {}", usage_file.display()));

        let hit = crate::testenv::with_test_home(home.path(), || {
            detect_usage_threshold_hit(Path::new("/Users/example/.claude.limited"))
        });

        match prior_cmd {
            Some(v) => crate::testenv::set_var("CSM_USAGE_CMD", &v.to_string_lossy()),
            None => crate::testenv::remove_var("CSM_USAGE_CMD"),
        }

        let hit = hit.expect("week_fable at 100% must trip the tier-2 check");
        assert_eq!(hit.dimension, LimitDimension::WeekFable);
        assert_eq!(
            hit.resets_at,
            Some(1_700_000_000),
            "the hit must carry week_fable's own resets_at, not None"
        );
    }

    // ── usage_threshold_hit_at (the LimitHit-tagged pure core) ────────────────

    /// The typed core tags each of the three dimensions correctly, and still
    /// returns `None` when nothing crosses the threshold.
    #[test]
    fn usage_threshold_hit_at_tags_each_dimension() {
        assert_eq!(
            usage_threshold_hit_at(99, 10, Some(5), 99),
            Some(LimitHit {
                dimension: LimitDimension::Session,
                message: "session 99%".to_string(),
                resets_at: None,
            })
        );
        assert_eq!(
            usage_threshold_hit_at(10, 99, None, 99),
            Some(LimitHit {
                dimension: LimitDimension::WeekAll,
                message: "week_all 99%".to_string(),
                resets_at: None,
            })
        );
        assert_eq!(
            usage_threshold_hit_at(10, 20, Some(99), 99),
            Some(LimitHit {
                dimension: LimitDimension::WeekFable,
                message: "week_fable 99%".to_string(),
                resets_at: None,
            })
        );
        assert_eq!(usage_threshold_hit_at(10, 20, Some(30), 99), None);
    }

    // ── detect_limit's live_limit branch (the caller hands over a typed
    //    LimitHit directly, so detect_limit never re-parses the dimension
    //    out of the message text) ───────────────────────────────────────────

    #[test]
    fn detect_limit_live_limit_tags_week_fable_not_unknown() {
        let input = HookInput {
            session_id: Some("s1".to_string()),
            cwd: None,
            reason: None,
            transcript_path: None,
            hook_event_name: Some("Stop".to_string()),
            error: None,
            error_details: None,
        };
        let hit = LimitHit {
            dimension: LimitDimension::WeekFable,
            message: "week_fable 100%".to_string(),
            resets_at: Some(1_789_646_400),
        };
        // owner_dir is unread on the live_limit branch (it's the tier-2 file
        // path), so any path works — Path::new(".") avoids a tempfile dep.
        let detection = detect_limit(&input, Path::new("."), Some(&hit));
        assert_eq!(
            detection,
            Detection::Limited {
                message: "week_fable 100%".to_string(),
                definitive: true,
                dimension: LimitDimension::WeekFable,
                resets_at: Some(1_789_646_400),
            }
        );
    }

    /// `usage_threshold_hit` (the pre-existing `Option<String>` API) must
    /// still return byte-identical messages after being rewritten as a thin
    /// wrapper over `usage_threshold_hit_at`.
    #[test]
    fn usage_threshold_hit_still_returns_identical_strings() {
        assert_eq!(
            usage_threshold_hit(99, 10, Some(5), 99).as_deref(),
            Some("session 99%")
        );
        assert_eq!(
            usage_threshold_hit(10, 99, None, 99).as_deref(),
            Some("week_all 99%")
        );
        assert_eq!(
            usage_threshold_hit(10, 20, Some(99), 99).as_deref(),
            Some("week_fable 99%")
        );
        assert_eq!(usage_threshold_hit(10, 20, Some(30), 99), None);
    }

    // ── fable_fallback_model (5b, same-account model fallback) ───────────────

    /// `session` and `week_all` trips never fall back to a model — neither is
    /// model-scoped, so a different model on the same account frees nothing;
    /// both keep switching accounts exactly as before.
    #[test]
    fn fable_fallback_model_returns_none_for_week_all_and_session() {
        assert_eq!(
            fable_fallback_model(LimitDimension::Session, true, false, "opus"),
            None
        );
        assert_eq!(
            fable_fallback_model(LimitDimension::WeekAll, true, false, "opus"),
            None
        );
    }

    /// A second `week_fable` trip after the fallback already fired returns
    /// `None`, even though everything else about the call still qualifies —
    /// what the caller does with that `None` (fall through to an account
    /// switch, or suppress it under 5c when the marker is still fresh) is
    /// `classify_with`'s decision, not this pure core's.
    #[test]
    fn fable_fallback_model_none_when_marker_present() {
        assert_eq!(
            fable_fallback_model(LimitDimension::WeekFable, true, true, "opus"),
            None
        );
    }

    /// `CLAUDE_FABLE_FALLBACK` off (the `enabled` flag false) disables the
    /// fallback regardless of dimension or marker state.
    #[test]
    fn fable_fallback_model_none_when_knob_disabled() {
        assert_eq!(
            fable_fallback_model(LimitDimension::WeekFable, false, false, "opus"),
            None
        );
    }

    /// A `week_fable` trip, enabled, with no prior fallback this session,
    /// returns `Some(fallback_model)` with whatever model name the caller
    /// passed — never a hardcoded `"opus"` inside the pure core itself.
    #[test]
    fn fable_fallback_model_honours_custom_model_name() {
        assert_eq!(
            fable_fallback_model(LimitDimension::WeekFable, true, false, "opus"),
            Some("opus".to_string())
        );
        assert_eq!(
            fable_fallback_model(LimitDimension::WeekFable, true, false, "claude-opus-4-5"),
            Some("claude-opus-4-5".to_string())
        );
    }

    // ── model_fallback_marker_is_stale ────────────────────────────────────────

    /// A marker written more than 7 days before the current window's reset is
    /// from an earlier window — stale, must be treated as absent.
    #[test]
    fn marker_is_stale_when_written_before_the_prior_window() {
        let resets_at = 1_718_000_000;
        let marker_epoch = resets_at - 8 * 86_400;
        assert!(model_fallback_marker_is_stale(
            marker_epoch,
            Some(resets_at)
        ));
    }

    /// A marker written within the current 7-day window is fresh.
    #[test]
    fn marker_is_not_stale_within_the_current_window() {
        let resets_at = 1_718_000_000;
        let marker_epoch = resets_at - 3 * 86_400;
        assert!(!model_fallback_marker_is_stale(
            marker_epoch,
            Some(resets_at)
        ));
    }

    /// Exactly at the boundary (`resets_at - 7*86400`) the marker still
    /// counts as belonging to the current window — the comparison is a
    /// strict `<`, not `<=`.
    #[test]
    fn marker_is_not_stale_exactly_at_the_boundary() {
        let resets_at = 1_718_000_000;
        let marker_epoch = resets_at - 7 * 86_400;
        assert!(!model_fallback_marker_is_stale(
            marker_epoch,
            Some(resets_at)
        ));
    }

    /// One second past the boundary, it is stale.
    #[test]
    fn marker_is_stale_one_second_past_the_boundary() {
        let resets_at = 1_718_000_000;
        let marker_epoch = resets_at - 7 * 86_400 - 1;
        assert!(model_fallback_marker_is_stale(
            marker_epoch,
            Some(resets_at)
        ));
    }

    /// No known `resets_at` — never expire the marker; there is no window
    /// boundary to compare against.
    #[test]
    fn marker_never_stale_when_resets_at_unknown() {
        assert!(!model_fallback_marker_is_stale(0, None));
        assert!(!model_fallback_marker_is_stale(i64::MAX, None));
    }

    /// An extreme `resets_at` near `i64::MIN` must not overflow the
    /// `resets_at - 7*86400` subtraction (a debug build panics on overflow) —
    /// `saturating_sub` clamps it instead, so this must return cleanly rather
    /// than panicking.
    #[test]
    fn marker_never_stale_with_extreme_negative_resets_at_no_overflow() {
        let resets_at = i64::MIN + 10;
        assert!(!model_fallback_marker_is_stale(i64::MIN, Some(resets_at)));
    }

    // ── model_fallback_marker_is_current (per-account marker currency) ──────

    /// Same profile, fresh epoch: counts.
    #[test]
    fn marker_is_current_when_same_profile_and_fresh() {
        let resets_at = 1_718_000_000;
        let marker_epoch = resets_at - 3 * 86_400;
        assert!(model_fallback_marker_is_current(
            "work",
            "work",
            marker_epoch,
            Some(resets_at)
        ));
    }

    /// A marker written on a DIFFERENT profile never counts, even with a
    /// fresh epoch — the account it was written for is not the one the
    /// session is on now.
    #[test]
    fn marker_is_not_current_for_a_different_profile() {
        let resets_at = 1_718_000_000;
        let marker_epoch = resets_at - 3 * 86_400;
        assert!(!model_fallback_marker_is_current(
            "work",
            "home",
            marker_epoch,
            Some(resets_at)
        ));
    }

    /// Same profile but a stale epoch never counts — the other axis
    /// (`model_fallback_marker_is_stale`) still applies.
    #[test]
    fn marker_is_not_current_when_stale() {
        let resets_at = 1_718_000_000;
        let marker_epoch = resets_at - 8 * 86_400;
        assert!(!model_fallback_marker_is_current(
            "work",
            "work",
            marker_epoch,
            Some(resets_at)
        ));
    }

    // ── model_fallback_marker_read (parses "<epoch> <profile>") ──────────────

    /// A well-formed marker parses to (epoch, profile) — driven through the
    /// public path via a real file, since the fn itself is private.
    #[test]
    fn marker_read_parses_epoch_and_profile() {
        let home = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(home.path().join(".claude.shared").join("smart")).unwrap();
        crate::testenv::with_test_home(home.path(), || {
            let sid = "sid-marker-read-0001";
            std::fs::write(crate::paths::model_fallback(sid), "1700000000 work").unwrap();
            assert_eq!(
                model_fallback_marker_read(sid),
                Some((1_700_000_000, "work".to_string()))
            );
        });
    }

    /// An old-format, epoch-only marker (no profile field) does not parse —
    /// the on-disk shape changed to carry the profile, and since this feature
    /// has never shipped in a release there is no back-compat obligation for
    /// the old shape; it must self-heal like any other unparseable marker.
    #[test]
    fn marker_read_rejects_epoch_only_old_shape() {
        let home = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(home.path().join(".claude.shared").join("smart")).unwrap();
        crate::testenv::with_test_home(home.path(), || {
            let sid = "sid-marker-read-old-shape-0001";
            std::fs::write(crate::paths::model_fallback(sid), "1700000000").unwrap();
            assert_eq!(model_fallback_marker_read(sid), None);
        });
    }

    /// Missing marker file: no panic, reads as `None`.
    #[test]
    fn marker_read_missing_file_is_none() {
        let home = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(home.path().join(".claude.shared").join("smart")).unwrap();
        crate::testenv::with_test_home(home.path(), || {
            assert_eq!(
                model_fallback_marker_read("sid-marker-read-missing-0001"),
                None
            );
        });
    }

    // ── switched_marker_blocks (1c passes a WeekFable trip through) ─────────

    /// A `WeekFable` trip with the fallback enabled passes through — it goes
    /// on to 5b, which never switches an account.
    #[test]
    fn switched_marker_does_not_block_week_fable_when_fallback_enabled() {
        assert!(!switched_marker_blocks(LimitDimension::WeekFable, true));
    }

    /// A `WeekFable` trip with the fallback DISABLED is blocked exactly like
    /// any other dimension — the knob-off case must restore pre-fallback
    /// behaviour.
    #[test]
    fn switched_marker_blocks_week_fable_when_fallback_disabled() {
        assert!(switched_marker_blocks(LimitDimension::WeekFable, false));
    }

    /// `Session` and `WeekAll` trips are always blocked, regardless of the
    /// fallback knob — only a fallback-bound `WeekFable` trip is ever let
    /// through.
    #[test]
    fn switched_marker_blocks_every_other_dimension_regardless_of_knob() {
        assert!(switched_marker_blocks(LimitDimension::Session, true));
        assert!(switched_marker_blocks(LimitDimension::WeekAll, true));
        assert!(switched_marker_blocks(LimitDimension::Unknown, true));
        assert!(switched_marker_blocks(LimitDimension::Session, false));
        assert!(switched_marker_blocks(LimitDimension::WeekAll, false));
    }

    // ── resolve_target_from_pick (single viability authority pass-through) ────

    /// A viable winner from pick_account passes straight through as the target.
    #[test]
    fn resolve_target_passes_through_viable_winner() {
        let result: crate::account::scoring::ScoringResult = Ok(Some("work".to_string()));
        assert_eq!(resolve_target_from_pick(result).as_deref(), Some("work"));
    }

    /// `Ok(None)` (no-op winner) resolves to no target — classify() must fall
    /// back to notify-only, never fabricate a target of its own.
    #[test]
    fn resolve_target_none_on_no_op_winner() {
        let result: crate::account::scoring::ScoringResult = Ok(None);
        assert!(resolve_target_from_pick(result).is_none());
    }

    /// `Err(AllSaturated)` — every profile is session- or week_all-saturated
    /// (`week_fable` no longer constrains viability, see
    /// `scoring::is_viable_pcts`'s doc) — resolves to no target: classify()
    /// must NOT write a relaunch sentinel pointing at a capped profile — it
    /// must fall back to notify-only.
    #[test]
    fn resolve_target_none_on_all_saturated_err() {
        let result: crate::account::scoring::ScoringResult =
            Err(crate::account::scoring::ScoringError::AllSaturated);
        assert!(
            resolve_target_from_pick(result).is_none(),
            "an all-saturated verdict must never produce a relaunch target"
        );
    }

    /// `Err(FetchFailed)` also resolves to no target (fetch miss, not a
    /// verdict at all — same fallback as AllSaturated).
    #[test]
    fn resolve_target_none_on_fetch_failed_err() {
        let result: crate::account::scoring::ScoringResult =
            Err(crate::account::scoring::ScoringError::FetchFailed(
                crate::usage::FetchError::NegativeCacheActive,
            ));
        assert!(resolve_target_from_pick(result).is_none());
    }
}
