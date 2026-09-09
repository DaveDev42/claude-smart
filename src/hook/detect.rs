//! Hook event classification — stdin JSON parsing + limit detection tiers.
//!
//! # Stdin contract (Claude Code hook spec)
//!
//! Claude Code writes a JSON object to the hook's stdin on every Stop/SubagentStop/
//! SessionEnd event. The fields we care about (serde field names are the exact
//! snake_case keys CC sends):
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
//! # Detection tiers
//!
//! Tier-1: `limit-in-tail` — last 12 transcript records contain an
//!   `isApiErrorMessage`-flagged entry matching the limit-banner regex, within 900 s.
//!   Reproduces `limit_in_tail()` from `claude-smart-helper.sh.j2` lines 770–786.
//!   The banner-shape match is structural (no model name is ever compared), so it
//!   would fire the same way for the session limit, the all-model weekly limit,
//!   or a model-scoped weekly limit (the `week_fable` dimension).
//!   STATUS (2026-09): dormant. Claude Code does not write a usage-limit notice
//!   into the transcript JSONL in any record type (checked across ~6.8k
//!   transcripts: zero `isApiErrorMessage` limit banners, zero assistant/system
//!   limit notices — the limit is surfaced only in the TUI and through the
//!   usage API / statusline `rate_limits`). This tier has never been observed
//!   to fire and is kept only as harmless defense-in-depth should a future
//!   Claude Code start writing such records. Tier-2 is the operative reactive
//!   detector; do not rely on tier-1.
//!
//! Tier-2: `current-usage` thresholding — session%, week_all%, or the
//!   model-scoped week_fable% at or above limits (all three dimensions, same
//!   `LIMIT_PCT` threshold; `week_fable` absent ⇒ that dimension never fires).
//!   Reproduces the tier-2 block from `limit-switch.sh.j2` lines 205–221, extended
//!   with the week_fable dimension the shell source predates.
//!   Freshness: session%/week_all% are also refreshed by the statusline capture,
//!   but the statusline `rate_limits` payload carries no model-scoped window, so
//!   week_fable% is only as fresh as the last per-profile usage-API probe
//!   (`CSM_USAGE_PROFILE_TTL`, default 300 s). A model-scoped cap can therefore
//!   take up to ~5 min to be noticed here — a data-freshness bound, not a
//!   detection gap.
//!
//! Tier-3 (malformed-in-tail): 4-conjunct Opus-4.8 tool-call fingerprint within 180 s.
//!   Reproduces `malformed_in_tail()` from `claude-smart-helper.sh.j2` lines 812–829.
//!
//! # Flow (mirrors limit-switch.sh.j2 exactly)
//!
//! 1. Kill-switches (env var, file marker, .switched marker).
//! 2. Reason gate → user_quit flag (doesn't exit yet — detection still runs).
//! 3. Detect (tier-1 / tier-2 / malformed-in-tail).
//! 4. If not limited → exit (with user-quit log if user_quit).
//! 5. If user_quit + limited → notify-only (deduped via .detected).
//! 6. Pick target profile.
//! 7. If no target → notify-only (deduped via .detected).
//! 8. If CLAUDE_AUTO_SWITCH_RELAUNCH != "1" → notify-only (deduped via .detected).
//! 9. Managed-session gate: check .pid file.
//! 10. Cooldown gate (noclobber .last-switch).
//! 11. Hop guard.
//!     → Decision::LimitSwitch { target_profile, handoff, cwd, born }.

use std::path::Path;

use serde::Deserialize;

// ─── stdin serde model ────────────────────────────────────────────────────────

/// JSON object Claude Code writes to the hook's stdin on Stop/SubagentStop/SessionEnd.
///
/// Field names match the exact keys Claude Code sends (snake_case, per CC hook spec).
/// All fields are `Option` because the hook must exit 0 cleanly on a missing session_id;
/// absent optional fields should degrade gracefully rather than hard-error.
#[derive(Debug, Clone, Deserialize)]
pub struct HookInput {
    /// Session UUID. Required for any action; exit 0 silently when absent.
    pub session_id: Option<String>,

    /// Working directory at the time of the hook event.
    pub cwd: Option<String>,

    /// Reason string from CC: one of "stop", "clear", "logout", "prompt_input_exit",
    /// "exit", etc. The reason gate maps some values to notify-only.
    pub reason: Option<String>,

    /// Path to the `.jsonl` transcript file for this session.
    pub transcript_path: Option<String>,
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
    },
}

// ─── constants ────────────────────────────────────────────────────────────────

/// Maximum number of hops before breaking the relaunch chain.
pub const MAX_HOPS: i64 = 1;

/// Machine-wide cooldown in seconds after a limit-switch.
pub const LAST_SWITCH_COOLDOWN_SECS: i64 = 300;

/// Tier-1 tail scan: how many records to look at.
pub const TIER1_TAIL_RECORDS: usize = 12;

/// Tier-1 recency window in seconds.
pub const TIER1_RECENCY_SECS: i64 = 900;

/// Malformed-in-tail tail records to scan.
pub const MALFORMED_TAIL_RECORDS: usize = 8;

/// Malformed-in-tail recency window in seconds.
pub const MALFORMED_RECENCY_SECS: i64 = 180;

/// Usage threshold percent (>=99 means "at the limit").
pub const LIMIT_PCT: i64 = 99;

// ─── reason gate ─────────────────────────────────────────────────────────────

/// Returns true if the `reason` field indicates a user-initiated quit.
/// These cases suppress the relaunch but still emit a notify (deduped).
///
/// Shell source: limit-switch.sh.j2 lines 159-161:
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
    let trimmed = buf.trim();
    if trimmed.is_empty() {
        return Ok(HookInput {
            session_id: None,
            cwd: None,
            reason: None,
            transcript_path: None,
        });
    }
    let input: HookInput = serde_json::from_str(trimmed)?;
    Ok(input)
}

/// Classify the hook event given the parsed input and the owner profile dir.
///
/// Reproduces the full `limit-switch.sh.j2` flow exactly:
///
/// 1. Kill-switches (env, file, .switched marker)
/// 2. Reason gate (user_quit flag — note: does NOT short-circuit yet, detection still runs)
/// 3. Detect (tier-1 / tier-2 / malformed)
/// 4. No limit signal → Skip
/// 5. user_quit + limited → NotifyOnly (deduped via .detected)
/// 6. Pick target profile (exclude current) via account::pick_account
/// 7. No viable target → NotifyOnly (deduped via .detected)
/// 8. CLAUDE_AUTO_SWITCH_RELAUNCH != "1" → NotifyOnly (deduped via .detected)
/// 9. Managed-session gate (.pid file)
/// 10. Machine-wide cooldown (noclobber .last-switch)
/// 11. Hop guard
///     → LimitSwitch
pub fn classify(input: &HookInput, owner_dir: &Path) -> anyhow::Result<Decision> {
    use crate::paths;

    let sid = match &input.session_id {
        Some(s) if !s.is_empty() => s.as_str(),
        _ => return Ok(Decision::Skip),
    };

    // ── 1. Kill-switches (cheapest checks first) ──────────────────────────────
    // Shell: limit-switch.sh.j2 lines 136-141

    // 1a. Env var kill-switch: CLAUDE_AUTO_SWITCH=0
    if std::env::var("CLAUDE_AUTO_SWITCH").as_deref() == Ok("0") {
        return Ok(Decision::Skip);
    }

    // 1b. File-based kill-switch: .auto-switch-disabled
    if paths::smart_dir_no_create()
        .join(".auto-switch-disabled")
        .exists()
    {
        return Ok(Decision::Skip);
    }

    // 1c. Already switched this session: .switched marker (fast path)
    // Shell: limit-switch.sh.j2 lines 140-141
    if paths::switched(sid).exists() {
        return Ok(Decision::Skip);
    }

    // ── 2. Reason gate (set flag, DON'T exit yet) ─────────────────────────────
    // Shell: limit-switch.sh.j2 lines 158-161
    // NOTE: the shell (post-2026-06-10 fix) does NOT exit here — it sets user_quit
    // and continues so that detection still runs; detection + user_quit → notify-only.
    let user_quit = input
        .reason
        .as_deref()
        .map(is_user_quit_reason)
        .unwrap_or(false);

    // ── 3. Detect (two-tier + malformed) ─────────────────────────────────────
    // Shell: limit-switch.sh.j2 lines 184-226
    // Tier-1 is local (no network), tier-2 uses the local usage cache.

    let (limited_msg, limited) = {
        // Tier-1 — transcript tail (local, instant)
        // Shell: limit-switch.sh.j2 lines 195-203
        let t1 = detect_limit_in_tail(input);
        if let Some(msg) = t1 {
            (msg, true)
        } else {
            // Tier-2 — usage pct from local cache
            // Shell: limit-switch.sh.j2 lines 205-221
            let t2 = detect_usage_threshold(owner_dir);
            if let Some(msg) = t2 {
                (msg, true)
            } else {
                // Malformed-in-tail (only if no limit signal from tier-1 or tier-2)
                // Note: in the shell this is in a separate hook (malformed-recover),
                // but the helper has it in `malformed-in-tail`. We reproduce the check.
                let m = detect_malformed_in_tail(input);
                if let Some(msg) = m {
                    (msg, true)
                } else {
                    (String::new(), false)
                }
            }
        }
    };

    // ── 4. No limit → exit (no side effects) ─────────────────────────────────
    // Shell: limit-switch.sh.j2 lines 223-227
    if !limited {
        if user_quit {
            // Shell: `_log "user-quit-skip" "reason=${reason}"`
            // No notification — this is just a log entry when no limit detected.
        }
        return Ok(Decision::Skip);
    }

    // ──────────────── Limit detected from this point on ──────────────────────

    // ── 5. User-quit + limited → one-shot notify (deduped via .detected) ─────
    // Shell: limit-switch.sh.j2 lines 246-258
    // NEVER kill/relaunch on a session the user explicitly closed.
    if user_quit {
        let detect_path = paths::detected(sid);
        if !detect_path.exists() {
            let _ = paths::smart_dir(); // ensure dir exists
            let _ = write_noclobber_epoch(&detect_path);
            prune_detected_markers();
            let profile_name = owner_dir_to_profile_name(owner_dir);
            let body = format!(
                "[{profile_name}] hit {limited_msg} — you quit, so not relaunching; next csm will pick a healthy account"
            );
            return Ok(Decision::NotifyOnly { message: body });
        }
        return Ok(Decision::Skip);
    }

    // ── 6. Pick target profile (exclude current, reactive hook mode) ──────────
    // Shell: limit-switch.sh.j2 lines 267-285
    //
    // Gate OFF (apply_stale_gate=false): the proactive launch path refuses to
    // auto-pick on stale usage (it has a picker fallback), but this hook fires
    // ONLY because the current profile already hit a limit and is
    // non-interactive. The tier-2 limit *detection* already bypasses the gate
    // (it reads current_usage directly), so blocking the *target-pick* on the
    // same stale data would leave the user stranded on the limited profile —
    // detected-but-not-switched. Score on the freshest-known numbers instead.
    let current_profile = owner_dir_to_profile_name(owner_dir);
    let target_result = crate::account::pick_account_gated(
        &current_profile,
        /*include_current=*/ false,
        /*apply_stale_gate=*/ false,
    );
    // Single viability authority: `resolve_target_from_pick` is a pure pass-through
    // over `account::pick_account_gated`'s verdict — the hook never recomputes or
    // second-guesses which profile is viable. Whatever `scoring::pick_best`
    // excludes (session-limited, week_all-saturated, or — once fable-aware —
    // week_fable-saturated) can therefore never come back as a relaunch target.
    let target_profile = match resolve_target_from_pick(target_result) {
        Some(name) => name,
        None => {
            // No viable target (all saturated/errored, or fetch miss)
            // Shell: limit-switch.sh.j2 lines 274-286
            let detect_path = paths::detected(sid);
            if !detect_path.exists() {
                let _ = paths::smart_dir();
                let _ = write_noclobber_epoch(&detect_path);
                prune_detected_markers();
                let body = format!(
                    "[{current_profile}] hit {limited_msg} — no account with headroom to switch to"
                );
                return Ok(Decision::NotifyOnly { message: body });
            }
            return Ok(Decision::Skip);
        }
    };

    // ── 7. Detect-only mode (CLAUDE_AUTO_SWITCH_RELAUNCH != "1") ─────────────
    // Shell: limit-switch.sh.j2 lines 298-308
    // Default is "1" (relaunch enabled). Explicit =0 → notify-only.
    // MUST run before any state mutation — does NOT claim .switched or cooldown.
    let relaunch_env =
        std::env::var("CLAUDE_AUTO_SWITCH_RELAUNCH").unwrap_or_else(|_| "1".to_string());
    if relaunch_env != "1" {
        let detect_path = paths::detected(sid);
        if !detect_path.exists() {
            let _ = paths::smart_dir();
            let _ = write_noclobber_epoch(&detect_path);
            prune_detected_markers();
            let sid_short = sid.get(..8).unwrap_or(sid);
            let body = format!(
                "[{current_profile}] hit {limited_msg} → switch to [{target_profile}] (auto-relaunch OFF; csm --profile {target_profile} --resume {sid_short})"
            );
            return Ok(Decision::NotifyOnly { message: body });
        }
        return Ok(Decision::Skip);
    }

    // ── 8. Managed-session gate: .pid file must exist and match claude/node ───
    // Shell: limit-switch.sh.j2 lines 318-348
    let pid_path = paths::pid_file(sid);
    if !pid_path.exists() {
        let detect_path = paths::detected(sid);
        if !detect_path.exists() {
            let _ = paths::smart_dir();
            let _ = write_noclobber_epoch(&detect_path);
            prune_detected_markers();
            let sid_short = sid.get(..8).unwrap_or(sid);
            let body = format!(
                "[{current_profile}] hit {limited_msg} → switch to [{target_profile}] by hand (csm --profile {target_profile} --resume {sid_short})"
            );
            return Ok(Decision::NotifyOnly { message: body });
        }
        return Ok(Decision::Skip);
    }

    // Read the pid file: "<pid> <born_epoch>"
    // Shell: limit-switch.sh.j2 lines 332-348
    let pid_content = std::fs::read_to_string(&pid_path).unwrap_or_default();
    let (claude_pid, born_epoch) = match parse_pid_file(&pid_content) {
        Some(v) => v,
        None => {
            // Bad PID file — log and skip
            return Ok(Decision::Skip);
        }
    };

    // Confirm the PID is a live claude/node process
    // Shell: limit-switch.sh.j2 lines 339-348
    if !is_live_claude_or_node(claude_pid) {
        return Ok(Decision::Skip);
    }

    // ── 9. Machine-wide cooldown (atomic noclobber claim) ─────────────────────
    // Shell: limit-switch.sh.j2 lines 355-367
    // Claim with noclobber; if fails, check if within cooldown window.
    let smart_dir = paths::smart_dir()?;
    let last_switch_path = paths::last_switch();
    let cooldown_secs = std::env::var("CLAUDE_SWITCH_COOLDOWN")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(LAST_SWITCH_COOLDOWN_SECS);

    let cooldown_blocked = check_and_claim_cooldown(&last_switch_path, cooldown_secs);
    let _ = smart_dir; // ensure we called smart_dir for side effect
    if cooldown_blocked {
        return Ok(Decision::Skip);
    }

    // ── 10. Hop guard ─────────────────────────────────────────────────────────
    // Shell: limit-switch.sh.j2 lines 376-381
    let max_hops = std::env::var("CLAUDE_MAX_HOPS")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(MAX_HOPS);
    let current_hop = read_sidecar_hop(sid);
    if current_hop >= max_hops {
        return Ok(Decision::Skip);
    }

    // ── 11. Build the handoff prompt ──────────────────────────────────────────
    // Shell: limit-switch.sh.j2 lines 402-406
    let next_hop = current_hop + 1;
    let sid_short = sid.get(..8).unwrap_or(sid);
    let handoff = build_handoff(sid_short, &current_profile, &target_profile, next_hop);

    let message = format!(
        "[{current_profile}] hit {limited_msg} → switching to [{target_profile}] (hop {next_hop})"
    );
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
    })
}

// ─── detection tier implementations ──────────────────────────────────────────

/// Tier-1: scan the last [`TIER1_TAIL_RECORDS`] lines of the transcript for an
/// entry with `isApiErrorMessage: true` matching the limit-banner regex, within
/// [`TIER1_RECENCY_SECS`] of now.
///
/// Returns `Some(description)` if a qualifying limit record is found, `None` otherwise.
///
/// Reproduces `limit_in_tail()` from `claude-smart-helper.sh.j2` lines 770–786.
/// Sanitizes the tail_hit text (remove `"` and `\`, collapse newlines/tabs) and
/// truncates to 80 chars — matching the shell sanitization at limit-switch.sh.j2 lines 198-200.
fn detect_limit_in_tail(input: &HookInput) -> Option<String> {
    let tp = input.transcript_path.as_deref().filter(|s| !s.is_empty())?;
    let path = std::path::Path::new(tp);
    if !path.exists() {
        return None;
    }

    let now_secs = now_epoch();
    let hit = limit_in_tail_impl(path, TIER1_TAIL_RECORDS, TIER1_RECENCY_SECS, now_secs)?;

    // Sanitize for embedding in notify: remove `"` and `\`, collapse newlines/tabs.
    // Shell: limit-switch.sh.j2 lines 198-200:
    //   `tail_hit="$(printf '%s' "$tail_hit" | tr -d '"\\' | tr '\n\t' '  ')"`
    //   `limited="api-error: ${tail_hit:0:80}"`
    let sanitized: String = hit
        .chars()
        .filter(|c| *c != '"' && *c != '\\')
        .map(|c| if c == '\n' || c == '\t' { ' ' } else { c })
        .take(80)
        .collect();
    Some(format!("api-error: {sanitized}"))
}

/// Core logic for tier-1 transcript tail scanning.
/// Extracted for testability (injected `now_secs`).
///
/// Reads the last `n` JSONL lines of the transcript file, parses each as JSON,
/// and looks for:
///   - `isApiErrorMessage: true`
///   - text content matching the limit-banner pattern (case-insensitive)
///   - timestamp within `window_secs` of `now_secs`
///
/// Returns the matched text on first (last-in-file) match, `None` otherwise.
pub(crate) fn limit_in_tail_impl(
    path: &Path,
    n: usize,
    window_secs: i64,
    now_secs: i64,
) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    // Shell: `tail -n "$n" "$tp"` — take last n non-empty lines.
    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail: &[&str] = if lines.len() > n {
        &lines[lines.len() - n..]
    } else {
        &lines
    };

    let mut result: Option<String> = None;
    for line in tail {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        // Must have isApiErrorMessage == true
        if v.get("isApiErrorMessage").and_then(|b| b.as_bool()) != Some(true) {
            continue;
        }
        // Extract text from message.content (array or string)
        let text = extract_message_text(&v);
        if text.is_empty() {
            continue;
        }
        // Must match limit-banner pattern (case-insensitive).
        // Base shell jq: `test("hit your .*limit|usage limit|limit reached"; "i")`.
        // Generalized here (structurally, never on a model name) to also catch a
        // model-scoped weekly banner phrased with "reached" instead of "hit", or
        // as a bare "weekly limit" mention — the same shapes Anthropic uses for
        // the session/all-model banners, just with a different noun in between.
        let tl = text.to_ascii_lowercase();
        let has_hit_phrase =
            (tl.contains("hit your") || tl.contains("reached your")) && tl.contains("limit");
        if !has_hit_phrase
            && !tl.contains("usage limit")
            && !tl.contains("limit reached")
            && !tl.contains("weekly limit")
        {
            continue;
        }
        // Recency check: timestamp within window_secs of now
        // Shell: `((.timestamp // "") | sub("\\.[0-9]+Z$"; "Z")) | try fromdateiso8601 catch 0`
        let ts = extract_timestamp_epoch(&v);
        if ts <= 0 || (now_secs - ts) > window_secs {
            continue;
        }
        // Last match wins (shell: `| tail -1` on jq output)
        result = Some(text);
    }
    result
}

/// Tier-2: query the usage cache for the owning profile and check whether
/// session%, week_all%, or the model-scoped week_fable% is at or above the
/// configured threshold.
///
/// Returns `Some(description)` if any dimension is exceeded, `None` otherwise.
///
/// I/O shell: fetches the fleet's [`crate::usage::UsageData`] once and hands the
/// three raw pcts to the pure [`usage_threshold_hit`] decision core (invariant:
/// pure core + thin I/O shell). `week_fable_pct` is `None` when the profile
/// carries no model-scoped weekly cap (e.g. `week_fable` is absent) — that
/// dimension then never constrains the verdict.
///
/// Reproduces the tier-2 block from `limit-switch.sh.j2` lines 205-221, extended
/// with the week_fable dimension (never a hardcoded model name — the fleet's
/// `week_fable` field name is what's fixed, the model it measures is data,
/// carried separately in `week_model_label`).
fn detect_usage_threshold(owner_dir: &Path) -> Option<String> {
    let profile = owner_dir_to_profile_name(owner_dir);
    let data = crate::usage::fetch().ok()?;
    let (session_pct, week_pct) = data.current_usage(&profile)?;
    let week_fable_pct = data
        .profiles
        .get(&profile)
        .and_then(|pu| pu.week_fable.as_ref())
        .map(|s| s.pct);

    let limit_pct = std::env::var("CLAUDE_LIMIT_PCT")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(LIMIT_PCT);

    usage_threshold_hit(session_pct, week_pct, week_fable_pct, limit_pct)
}

/// Pure decision core for tier-2: given the three usage-pct dimensions and the
/// threshold, decide whether a limit was hit.
///
/// Shell: limit-switch.sh.j2 lines 215-219 (session/week_all only — week_fable
/// is a Rust-side addition, same rule: threshold only on non-negative integers,
/// so absent-as-`-1` never fires, and `None` — no model-scoped cap at all for
/// this profile — never fires either).
///
/// Checked in order: session, then week_all, then week_fable (first hit wins;
/// order only affects which description string comes back, not whether one
/// does).
pub(crate) fn usage_threshold_hit(
    session_pct: i64,
    week_pct: i64,
    week_fable_pct: Option<i64>,
    limit_pct: i64,
) -> Option<String> {
    if session_pct >= 0 && session_pct >= limit_pct {
        return Some(format!("session {session_pct}%"));
    }
    if week_pct >= 0 && week_pct >= limit_pct {
        return Some(format!("week_all {week_pct}%"));
    }
    if let Some(fable_pct) = week_fable_pct {
        if fable_pct >= 0 && fable_pct >= limit_pct {
            return Some(format!("week_fable {fable_pct}%"));
        }
    }
    None
}

/// Malformed-in-tail: scan the last tail for the 4-conjunct Opus-4.8 tool-call
/// fingerprint within [`MALFORMED_RECENCY_SECS`].
///
/// Returns `Some("MALFORMED_TOOL_USE")` on a hit, `None` otherwise.
///
/// Reproduces `malformed_in_tail()` from `claude-smart-helper.sh.j2` lines 812–829.
/// Four conjuncts (validated zero-FP on the corpus):
///   1. `.type == "assistant"` AND `.message.stop_reason == "tool_use"`
///   2. content has NO block of type "tool_use" (the drop)
///   3. joined text content ENDS with `</invoke>` (pattern: `</invoke>\s*$`)
///   4. text CONTAINS `<invoke name=` or `antml:invoke name=`
fn detect_malformed_in_tail(input: &HookInput) -> Option<String> {
    let tp = input.transcript_path.as_deref().filter(|s| !s.is_empty())?;
    let path = std::path::Path::new(tp);
    if !path.exists() {
        return None;
    }
    let now_secs = now_epoch();
    malformed_in_tail_impl(
        path,
        MALFORMED_TAIL_RECORDS,
        MALFORMED_RECENCY_SECS,
        now_secs,
    )
}

/// Core logic for malformed-in-tail scanning.
/// Extracted for testability (injected `now_secs`).
pub(crate) fn malformed_in_tail_impl(
    path: &Path,
    n: usize,
    window_secs: i64,
    now_secs: i64,
) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail: &[&str] = if lines.len() > n {
        &lines[lines.len() - n..]
    } else {
        &lines
    };

    let mut found = false;
    for line in tail {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        // Conjunct 1: type=="assistant" AND message.stop_reason=="tool_use"
        if v.get("type").and_then(|t| t.as_str()) != Some("assistant") {
            continue;
        }
        if v.get("message")
            .and_then(|m| m.get("stop_reason"))
            .and_then(|r| r.as_str())
            != Some("tool_use")
        {
            continue;
        }
        // Conjunct 2: content has NO block of type "tool_use"
        let content_arr = v
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
            .map(|a| a.as_slice())
            .unwrap_or(&[]);
        let has_tool_use_block = content_arr
            .iter()
            .any(|block| block.get("type").and_then(|t| t.as_str()) == Some("tool_use"));
        if has_tool_use_block {
            continue;
        }
        // Build joined text content
        let text: String = content_arr
            .iter()
            .filter_map(|block| {
                if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                    block.get("text").and_then(|t| t.as_str())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("");
        // Conjunct 3: text ends with </invoke> (ignoring trailing whitespace)
        // Shell: `test("</invoke>\\s*$"; "")`
        if !text.trim_end().ends_with("</invoke>") {
            continue;
        }
        // Conjunct 4: text contains <invoke name= or antml:invoke name=
        // Shell: `test("<invoke name=|antml:invoke name="; "")`
        if !text.contains("<invoke name=") && !text.contains("antml:invoke name=") {
            continue;
        }
        // Recency check
        let ts = extract_timestamp_epoch(&v);
        if ts <= 0 || (now_secs - ts) > window_secs {
            continue;
        }
        found = true;
        // Last match wins (shell: `| tail -1`)
    }
    if found {
        Some("MALFORMED_TOOL_USE".to_string())
    } else {
        None
    }
}

// ─── helpers ─────────────────────────────────────────────────────────────────

/// Extract the text content from a transcript record's message.
/// Handles both array-of-blocks and raw-string forms.
///
/// Shell jq: `(.message.content // empty) | if type=="array" then (map(select(.type=="text") | .text) | join(" "))
///            elif type=="string" then . else "" end`
fn extract_message_text(v: &serde_json::Value) -> String {
    let content = match v.get("message").and_then(|m| m.get("content")) {
        Some(c) => c,
        None => return String::new(),
    };
    if let Some(arr) = content.as_array() {
        arr.iter()
            .filter_map(|block| {
                if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                    block.get("text").and_then(|t| t.as_str())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    } else if let Some(s) = content.as_str() {
        s.to_string()
    } else {
        String::new()
    }
}

/// Extract the timestamp epoch from a transcript record, matching the shell's
/// `((.timestamp // "") | sub("\\.[0-9]+Z$"; "Z")) | try fromdateiso8601 catch 0`
///
/// Returns 0 on any parse failure.
fn extract_timestamp_epoch(v: &serde_json::Value) -> i64 {
    let ts_str = match v.get("timestamp").and_then(|t| t.as_str()) {
        Some(s) if !s.is_empty() => s,
        _ => return 0,
    };
    // Strip subsecond digits and trailing Z: "2026-06-10T12:34:56.789Z" → "2026-06-10T12:34:56Z"
    // Shell: `sub("\\.[0-9]+Z$"; "Z")`
    let normalized = if let Some(dot_pos) = ts_str.find('.') {
        // Remove from '.' to end, then append 'Z'
        let base = &ts_str[..dot_pos];
        // Ensure it looks like it ends at 'Z' context
        if ts_str.ends_with('Z') {
            format!("{base}Z")
        } else {
            ts_str.to_string()
        }
    } else {
        ts_str.to_string()
    };
    // Parse ISO-8601 UTC datetime
    use chrono::DateTime;
    if let Ok(dt) = DateTime::parse_from_rfc3339(&normalized) {
        dt.timestamp()
    } else {
        0
    }
}

/// Resolve the relaunch target from an `account::pick_account`-family scoring
/// result — the single point where classify() decides "who do we switch to".
///
/// `Ok(Some(name))` → `name` IS the target; this function never recomputes,
/// re-ranks, or filters it — whatever viability rule `scoring::pick_best`
/// applies (session/week_all/week_fable) is the ONLY rule that ever runs.
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

/// Read the `hop` field from `<sid>.json` sidecar.
/// Returns 0 on missing/corrupt sidecar. Tolerates both String and Number (§6 compat).
/// Delegates to the single `Sidecar::hop_int` SSOT (was triplicated).
fn read_sidecar_hop(sid: &str) -> i64 {
    crate::sidecar::read_sidecar(&crate::paths::sidecar(sid))
        .map(|s| s.hop_int())
        .unwrap_or(0)
}

/// Build the handoff prompt string.
///
/// Shell: limit-switch.sh.j2 lines 402-406:
///   `HANDOFF="이전 세션(${session_id:0:8})이 사용량 한도에 걸려 [${current_profile}]에서 [${target_profile}] 계정으로 자동 전환됐어. (hop ${next_hop}) 직전까지 하던 작업을 그대로 이어서 진행해줘."`
/// Unless overridden by CLAUDE_SMART_RESUME_PROMPT or suppressed (empty string).
///
/// The spec (§2 relaunch): `${CLAUDE_SMART_RESUME_PROMPT-resume}` semantics:
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
        if let Ok(meta) = entry.metadata() {
            if let Ok(modified) = meta.modified() {
                if let Ok(age) = now.duration_since(modified) {
                    if age.as_secs() > seven_days_secs {
                        let _ = std::fs::remove_file(entry.path());
                    }
                }
            }
        }
    }
}

/// Check the machine-wide cooldown and atomically claim the slot.
///
/// Returns `true` (blocked) if within the cooldown window.
/// Returns `false` (proceed) if the window has expired or we claimed the slot.
///
/// Shell: limit-switch.sh.j2 lines 355-367:
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
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
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
    use std::sync::Mutex;

    /// Serializes tests that mutate the process-global `CLAUDE_SMART_RESUME_PROMPT`
    /// env var, so they don't clobber each other under the default parallel runner.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

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

    /// MAX_HOPS constant is 1 (spec §2 hop guard).
    #[test]
    fn max_hops_is_one() {
        assert_eq!(MAX_HOPS, 1);
    }

    /// LAST_SWITCH_COOLDOWN_SECS is 300 (spec §2 machine-wide cooldown).
    #[test]
    fn last_switch_cooldown_is_300() {
        assert_eq!(LAST_SWITCH_COOLDOWN_SECS, 300);
    }

    // ── limit_in_tail_impl tests ───────────────────────────────────────────────

    fn make_transcript_line(
        is_api_error: bool,
        text: &str,
        ts_offset_secs: i64,
        base_epoch: i64,
    ) -> String {
        let epoch = base_epoch + ts_offset_secs;
        let dt = chrono::DateTime::<chrono::Utc>::from_timestamp(epoch, 0)
            .unwrap()
            .format("%Y-%m-%dT%H:%M:%S%.3fZ")
            .to_string();
        serde_json::json!({
            "type": "assistant",
            "isApiErrorMessage": is_api_error,
            "timestamp": dt,
            "message": {
                "content": [
                    {"type": "text", "text": text}
                ]
            }
        })
        .to_string()
    }

    /// Tier-1 detects a fresh limit banner in the transcript tail.
    #[test]
    fn limit_in_tail_detects_fresh_banner() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let base_now: i64 = 1_718_000_000;
        set_test_now(base_now);
        let banner = "You've hit your session limit · resets 9pm (Asia/Seoul)";
        let line = make_transcript_line(true, banner, -100, base_now); // 100 s ago

        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "{line}").unwrap();

        let result = limit_in_tail_impl(f.path(), 12, 900, base_now);
        assert!(result.is_some(), "should detect fresh limit banner");
        assert!(result.unwrap().contains("hit your"));
    }

    /// Tier-1 does NOT trigger on a stale limit banner (outside the window).
    #[test]
    fn limit_in_tail_skips_stale_banner() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let base_now: i64 = 1_718_000_000;
        let banner = "You've hit your session limit · resets 9pm (Asia/Seoul)";
        // Banner timestamp is 1000 s ago (outside the 900 s window)
        let line = make_transcript_line(true, banner, -1000, base_now);

        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "{line}").unwrap();

        let result = limit_in_tail_impl(f.path(), 12, 900, base_now);
        assert!(result.is_none(), "stale banner should not trigger");
    }

    /// Tier-1 does NOT trigger on a non-error record.
    #[test]
    fn limit_in_tail_skips_non_error_record() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let base_now: i64 = 1_718_000_000;
        let banner = "You've hit your session limit · resets 9pm (Asia/Seoul)";
        // isApiErrorMessage is false
        let line = make_transcript_line(false, banner, -100, base_now);

        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "{line}").unwrap();

        let result = limit_in_tail_impl(f.path(), 12, 900, base_now);
        assert!(result.is_none(), "non-error record should not trigger");
    }

    /// Tier-1 does NOT trigger when text doesn't match the limit pattern.
    #[test]
    fn limit_in_tail_skips_wrong_text() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let base_now: i64 = 1_718_000_000;
        let line = make_transcript_line(true, "Server overloaded, try again later", -100, base_now);

        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "{line}").unwrap();

        let result = limit_in_tail_impl(f.path(), 12, 900, base_now);
        assert!(result.is_none(), "wrong text should not trigger");
    }

    /// Tier-1: only the last N lines are checked.
    #[test]
    fn limit_in_tail_only_last_n_records() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let base_now: i64 = 1_718_000_000;
        let mut f = NamedTempFile::new().unwrap();

        // Write 15 normal records, then the limit banner at position 13 (inside last 12).
        for i in 0..12 {
            let line = make_transcript_line(false, &format!("normal message {i}"), -50, base_now);
            writeln!(f, "{line}").unwrap();
        }
        // This banner is within the window of the last 12 records
        let banner_line = make_transcript_line(
            true,
            "You've hit your session limit · resets 9pm",
            -60,
            base_now,
        );
        writeln!(f, "{banner_line}").unwrap();

        // Now add 3 more normal records — banner is at position -4 from end (within 12)
        for i in 0..3 {
            let line = make_transcript_line(false, &format!("after {i}"), -10, base_now);
            writeln!(f, "{line}").unwrap();
        }

        // Total: 16 lines. Banner is at index 12 (0-based), within the last 4+1=4 records
        // of the tail-12 scan. Should be detected.
        let result = limit_in_tail_impl(f.path(), 12, 900, base_now);
        assert!(
            result.is_some(),
            "banner within last 12 should be detected: {result:?}"
        );
    }

    // ── malformed_in_tail_impl tests ───────────────────────────────────────────

    fn make_malformed_record(ts_offset: i64, base_now: i64) -> String {
        let epoch = base_now + ts_offset;
        let dt = chrono::DateTime::<chrono::Utc>::from_timestamp(epoch, 0)
            .unwrap()
            .format("%Y-%m-%dT%H:%M:%S%.3fZ")
            .to_string();
        serde_json::json!({
            "type": "assistant",
            "timestamp": dt,
            "message": {
                "stop_reason": "tool_use",
                "content": [
                    {
                        "type": "text",
                        "text": "I'll use the tool <invoke name=\"bash\"><parameter>ls</parameter></invoke>"
                    }
                ]
            }
        })
        .to_string()
    }

    /// malformed-in-tail detects a fresh malformed tool-call record.
    #[test]
    fn malformed_in_tail_detects_fresh_hit() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let base_now: i64 = 1_718_000_000;
        let line = make_malformed_record(-60, base_now); // 60 s ago, within 180 s

        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "{line}").unwrap();

        let result = malformed_in_tail_impl(f.path(), 8, 180, base_now);
        assert_eq!(result.as_deref(), Some("MALFORMED_TOOL_USE"));
    }

    /// malformed-in-tail does NOT trigger when the record has a tool_use block.
    #[test]
    fn malformed_in_tail_skips_when_has_tool_use_block() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let base_now: i64 = 1_718_000_000;
        let epoch = base_now - 60;
        let dt = chrono::DateTime::<chrono::Utc>::from_timestamp(epoch, 0)
            .unwrap()
            .format("%Y-%m-%dT%H:%M:%S%.3fZ")
            .to_string();
        // Has a real tool_use block → conjunct 2 fails
        let line = serde_json::json!({
            "type": "assistant",
            "timestamp": dt,
            "message": {
                "stop_reason": "tool_use",
                "content": [
                    {"type": "tool_use", "id": "t1", "name": "bash", "input": {"cmd": "ls"}},
                    {"type": "text", "text": "running <invoke name=\"bash\"></invoke>"}
                ]
            }
        })
        .to_string();

        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "{line}").unwrap();

        let result = malformed_in_tail_impl(f.path(), 8, 180, base_now);
        assert!(
            result.is_none(),
            "should not trigger when tool_use block present"
        );
    }

    /// malformed-in-tail does NOT trigger when text doesn't end with </invoke>.
    #[test]
    fn malformed_in_tail_skips_without_close_invoke() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let base_now: i64 = 1_718_000_000;
        let epoch = base_now - 60;
        let dt = chrono::DateTime::<chrono::Utc>::from_timestamp(epoch, 0)
            .unwrap()
            .format("%Y-%m-%dT%H:%M:%S%.3fZ")
            .to_string();
        // Text ends with something else — conjunct 3 fails
        let line = serde_json::json!({
            "type": "assistant",
            "timestamp": dt,
            "message": {
                "stop_reason": "tool_use",
                "content": [
                    {"type": "text", "text": "<invoke name=\"bash\">ls"}
                ]
            }
        })
        .to_string();

        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "{line}").unwrap();

        let result = malformed_in_tail_impl(f.path(), 8, 180, base_now);
        assert!(
            result.is_none(),
            "should not trigger without </invoke> at end"
        );
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
        let _g = ENV_LOCK.lock().unwrap();
        // Remove any env override
        let prev = std::env::var("CLAUDE_SMART_RESUME_PROMPT");
        std::env::remove_var("CLAUDE_SMART_RESUME_PROMPT");
        let h = build_handoff("01234567", "home", "work", 1);
        // Should contain Korean text
        assert!(h.contains("01234567"), "should contain sid_short: {h}");
        assert!(h.contains("home"), "should contain current profile: {h}");
        assert!(h.contains("work"), "should contain target profile: {h}");
        assert!(h.contains("hop 1"), "should contain hop: {h}");
        // Restore
        if let Ok(v) = prev {
            std::env::set_var("CLAUDE_SMART_RESUME_PROMPT", v);
        }
    }

    #[test]
    fn build_handoff_empty_suppresses() {
        let _g = ENV_LOCK.lock().unwrap();
        let prev = std::env::var("CLAUDE_SMART_RESUME_PROMPT");
        std::env::set_var("CLAUDE_SMART_RESUME_PROMPT", "");
        let h = build_handoff("01234567", "home", "work", 1);
        assert!(h.is_empty(), "empty env var should suppress handoff");
        if let Ok(v) = prev {
            std::env::set_var("CLAUDE_SMART_RESUME_PROMPT", v);
        } else {
            std::env::remove_var("CLAUDE_SMART_RESUME_PROMPT");
        }
    }

    #[test]
    fn build_handoff_custom_override() {
        let _g = ENV_LOCK.lock().unwrap();
        let prev = std::env::var("CLAUDE_SMART_RESUME_PROMPT");
        std::env::set_var("CLAUDE_SMART_RESUME_PROMPT", "custom prompt here");
        let h = build_handoff("01234567", "home", "work", 1);
        assert_eq!(h, "custom prompt here");
        if let Ok(v) = prev {
            std::env::set_var("CLAUDE_SMART_RESUME_PROMPT", v);
        } else {
            std::env::remove_var("CLAUDE_SMART_RESUME_PROMPT");
        }
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

    // ── extract_timestamp_epoch tests ─────────────────────────────────────────

    #[test]
    fn timestamp_epoch_parsed_correctly() {
        let v = serde_json::json!({"timestamp": "2024-01-10T12:00:00.000Z"});
        let epoch = extract_timestamp_epoch(&v);
        assert!(epoch > 0, "should parse ISO-8601 timestamp");
        // 2024-01-10T12:00:00Z = 1704888000 (approximately)
        assert!(
            epoch > 1_700_000_000 && epoch < 1_800_000_000,
            "epoch out of expected range: {epoch}"
        );
    }

    #[test]
    fn timestamp_epoch_missing_returns_zero() {
        let v = serde_json::json!({"type": "assistant"});
        assert_eq!(extract_timestamp_epoch(&v), 0);
    }

    #[test]
    fn timestamp_epoch_empty_returns_zero() {
        let v = serde_json::json!({"timestamp": ""});
        assert_eq!(extract_timestamp_epoch(&v), 0);
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

    /// `Err(AllSaturated)` — every profile is session/week_all/week_fable
    /// capped — resolves to no target. This is the case a fable-only-capped
    /// fleet (every account healthy on session/week_all but exhausted on the
    /// model-scoped weekly cap) must land in once `scoring::pick_best`
    /// excludes on week_fable: classify() must NOT write a relaunch sentinel
    /// pointing at a capped profile — it must fall back to notify-only.
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

    // ── tier-1 pattern: model-scoped weekly banner shapes (generic, no model name) ─

    /// A weekly-scoped banner phrased with "reached your ... limit" (rather
    /// than "hit your") must still be detected — the model-scoped weekly cap
    /// message may not share the session banner's exact wording, but the
    /// pattern match is structural (never keyed to a model name).
    #[test]
    fn limit_in_tail_detects_reached_your_phrasing() {
        let base_now: i64 = 1_718_000_000;
        let banner =
            "You've reached your weekly limit for this session's model tier · resets Mon 9am";
        let line = make_transcript_line(true, banner, -100, base_now);

        let mut f = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            writeln!(f, "{line}").unwrap();
        }

        let result = limit_in_tail_impl(f.path(), 12, 900, base_now);
        assert!(
            result.is_some(),
            "'reached your ... limit' phrasing should be detected"
        );
    }

    /// A bare "weekly limit" mention — with none of "hit your"/"reached
    /// your"/"usage limit"/"limit reached" present — is also detected: covers
    /// a terser model-scoped weekly banner shape. The model tier name (data,
    /// e.g. "Fable") never has to appear in the pattern itself.
    #[test]
    fn limit_in_tail_detects_bare_weekly_limit_phrasing() {
        let base_now: i64 = 1_718_000_000;
        let banner = "Your weekly limit for Fable is used up for this week. Resets Monday.";
        let line = make_transcript_line(true, banner, -100, base_now);

        let mut f = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            writeln!(f, "{line}").unwrap();
        }

        let result = limit_in_tail_impl(f.path(), 12, 900, base_now);
        assert!(
            result.is_some(),
            "bare 'weekly limit' phrasing should be detected"
        );
    }

    /// Sanity: the generalized pattern still does NOT fire on unrelated text
    /// that merely mentions "weekly" or "limit" separately without any of the
    /// recognized banner shapes.
    #[test]
    fn limit_in_tail_still_skips_unrelated_weekly_mention() {
        let base_now: i64 = 1_718_000_000;
        let banner = "Here's your weekly summary. No caps were exceeded.";
        let line = make_transcript_line(true, banner, -100, base_now);

        let mut f = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            writeln!(f, "{line}").unwrap();
        }

        let result = limit_in_tail_impl(f.path(), 12, 900, base_now);
        assert!(
            result.is_none(),
            "unrelated weekly summary text must not trigger a false positive"
        );
    }
}
