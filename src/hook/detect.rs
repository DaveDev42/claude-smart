//! Hook event classification — stdin JSON parsing + limit detection tiers.
//!
//! # Stdin contract (Claude Code hook spec)
//!
//! Claude Code writes a JSON object to the hook's stdin on every Stop/SubagentStop/
//! SessionEnd event. The fields we care about (serde field names are the exact
//! camelCase keys CC sends):
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
//!
//! Tier-2: `current-usage` thresholding — session% or week_all% at or above limits.
//!
//! Malformed-in-tail: 4-conjunct Opus-4.8 tool-call fingerprint within 180 s.
//!
//! Kill-switches (checked first, return [`Decision::Skip`]):
//!   - `CLAUDE_AUTO_SWITCH=0` env var
//!   - `<smart_dir>/.auto-switch-disabled` marker file
//!   - `<smart_dir>/<sid>.switched` marker (already switched this session)
//!
//! Reason gate: `clear|logout|prompt_input_exit|exit` reasons → [`Decision::NotifyOnly`].
//!
//! Machine-wide cooldown: `.last-switch` noclobber 300 s.
//!
//! Hop guard: MAX_HOPS = 1; hop from sidecar must be <= MAX_HOPS.

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

    /// User-quit reason gate: emit an OSC 777 notify but do NOT write a relaunch
    /// sentinel or stop the process.
    NotifyOnly { message: String },

    /// Full limit-switch: emit notify + write sentinel + stop the supervisor.
    LimitSwitch {
        message: String,
        target_profile: String,
        handoff: String,
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

/// Malformed-in-tail recency window in seconds.
pub const MALFORMED_RECENCY_SECS: i64 = 180;

// ─── reason gate ─────────────────────────────────────────────────────────────

/// Returns true if the `reason` field indicates a user-initiated quit.
/// These map to [`Decision::NotifyOnly`] (inform but do not relaunch).
pub fn is_user_quit_reason(reason: &str) -> bool {
    matches!(
        reason,
        "clear" | "logout" | "prompt_input_exit" | "exit"
    )
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
/// Checks kill-switches, reason gate, cooldowns, hop guard, and the three detection
/// tiers (tier-1 limit-in-tail, tier-2 usage threshold, malformed-in-tail).
/// Bodies for the detection tiers are `unimplemented!()` placeholders — see
/// [`detect_limit_in_tail`], [`detect_usage_threshold`], [`detect_malformed_in_tail`].
pub fn classify(input: &HookInput, owner_dir: &Path) -> anyhow::Result<Decision> {
    use crate::paths;

    let sid = match &input.session_id {
        Some(s) if !s.is_empty() => s.as_str(),
        _ => return Ok(Decision::Skip),
    };

    // ── kill-switches ──────────────────────────────────────────────────────────

    // 1. Env var kill-switch: CLAUDE_AUTO_SWITCH=0
    if std::env::var("CLAUDE_AUTO_SWITCH").as_deref() == Ok("0") {
        return Ok(Decision::Skip);
    }

    // 2. File-based kill-switch: .auto-switch-disabled
    if paths::smart_dir_no_create()
        .join(".auto-switch-disabled")
        .exists()
    {
        return Ok(Decision::Skip);
    }

    // 3. Already switched this session: .switched marker
    if paths::switched(sid).exists() {
        return Ok(Decision::Skip);
    }

    // ── reason gate ───────────────────────────────────────────────────────────

    if let Some(reason) = &input.reason {
        if is_user_quit_reason(reason) {
            let message = format!(
                "csm: session {} ended (reason: {})",
                sid,
                reason
            );
            return Ok(Decision::NotifyOnly { message });
        }
    }

    // ── machine-wide cooldown ─────────────────────────────────────────────────

    if let Ok(content) = std::fs::read_to_string(paths::last_switch()) {
        if let Ok(epoch) = content.trim().parse::<i64>() {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            if now - epoch < LAST_SWITCH_COOLDOWN_SECS {
                return Ok(Decision::Skip);
            }
        }
    }

    // ── hop guard ─────────────────────────────────────────────────────────────

    // Read the current hop count from the sidecar. If hop >= MAX_HOPS, skip.
    let current_hop = read_sidecar_hop(sid);
    if current_hop >= MAX_HOPS {
        return Ok(Decision::Skip);
    }

    // ── detection tiers ───────────────────────────────────────────────────────

    // Tier-1: limit-in-tail
    let tier1 = detect_limit_in_tail(input, owner_dir);

    // Tier-2: usage threshold (only if tier-1 didn't fire)
    let tier2 = if !tier1 {
        detect_usage_threshold(owner_dir)
    } else {
        false
    };

    // Malformed-in-tail (only if neither tier fired)
    let malformed = if !tier1 && !tier2 {
        detect_malformed_in_tail(input, owner_dir)
    } else {
        false
    };

    let limit_detected = tier1 || tier2 || malformed;

    if !limit_detected {
        return Ok(Decision::Skip);
    }

    // ── prune stale .detected / .switched markers (7-day window) ─────────────
    prune_old_markers(sid);

    // ── build the limit-switch decision ───────────────────────────────────────
    // target_profile and handoff are resolved by the account scorer at relaunch time;
    // the hook writes a sentinel with these values — unimplemented for PHASE 0.
    let target_profile = resolve_target_profile(owner_dir);
    let handoff = default_handoff();

    let message = format!("csm: limit detected for session {sid}, switching to {target_profile}");

    Ok(Decision::LimitSwitch {
        message,
        target_profile,
        handoff,
    })
}

// ─── detection tier implementations (PHASE 0: stubs) ─────────────────────────

/// Tier-1: scan the last [`TIER1_TAIL_RECORDS`] records of the transcript for an
/// entry with `isApiErrorMessage: true` matching the limit-banner regex, within
/// [`TIER1_RECENCY_SECS`] of now.
///
/// Returns `true` if a qualifying limit record is found.
fn detect_limit_in_tail(_input: &HookInput, _owner_dir: &Path) -> bool {
    // PHASE 0 stub — full implementation in a later phase.
    false
}

/// Tier-2: query the usage cache for the owning profile and check whether
/// session% or week_all% is at or above the configured thresholds.
///
/// Returns `true` if thresholds are exceeded.
fn detect_usage_threshold(_owner_dir: &Path) -> bool {
    // PHASE 0 stub — full implementation in a later phase.
    false
}

/// Malformed-in-tail: scan the last tail for the 4-conjunct Opus-4.8 tool-call
/// fingerprint within [`MALFORMED_RECENCY_SECS`].
///
/// Returns `true` if the fingerprint is found.
fn detect_malformed_in_tail(_input: &HookInput, _owner_dir: &Path) -> bool {
    // PHASE 0 stub — full implementation in a later phase.
    false
}

// ─── helpers (PHASE 0: stubs) ─────────────────────────────────────────────────

/// Read the current hop count from the sidecar (`<sid>.json`).
/// Returns 0 on missing/corrupt sidecar.
fn read_sidecar_hop(sid: &str) -> i64 {
    use crate::paths;
    let path = paths::sidecar(sid);
    let Ok(content) = std::fs::read_to_string(&path) else {
        return 0;
    };
    let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) else {
        return 0;
    };
    match val.get("hop") {
        Some(serde_json::Value::String(s)) => s.parse::<i64>().unwrap_or(0),
        Some(serde_json::Value::Number(n)) => n.as_i64().unwrap_or(0),
        _ => 0,
    }
}

/// Resolve the target profile to switch to. PHASE 0 stub — real implementation
/// will call the account scorer.
fn resolve_target_profile(_owner_dir: &Path) -> String {
    // PHASE 0 stub.
    String::from("personal")
}

/// Return the default handoff prompt string (localized).
fn default_handoff() -> String {
    // The spec calls for Korean "resume" prompt; the exact string is sourced from
    // CLAUDE_SMART_RESUME_PROMPT env or the default "resume".
    std::env::var("CLAUDE_SMART_RESUME_PROMPT").unwrap_or_else(|_| "resume".to_string())
}

/// Prune `.detected` and `.switched` markers older than 7 days.
fn prune_old_markers(sid: &str) {
    use crate::paths;
    let seven_days_secs = 7 * 24 * 3600u64;
    let now = std::time::SystemTime::now();

    for path in [paths::detected(sid), paths::switched(sid)] {
        if let Ok(metadata) = std::fs::metadata(&path) {
            if let Ok(modified) = metadata.modified() {
                if let Ok(age) = now.duration_since(modified) {
                    if age.as_secs() > seven_days_secs {
                        let _ = std::fs::remove_file(&path);
                    }
                }
            }
        }
    }
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// HookInput deserializes from a typical CC hook payload.
    #[test]
    fn hook_input_full_payload() {
        let json = r#"{
            "session_id": "01234567-89ab-cdef-0123-456789abcdef",
            "cwd": "/Users/example/Projects/github.com/foo",
            "reason": "stop",
            "transcript_path": "/Users/example/.claude.shared/projects/-Users-dave-Projects-github-com-foo/01234567-89ab-cdef-0123-456789abcdef.jsonl"
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
        // parse_stdin() reads stdin; test the serde layer directly.
        let json = r#"{}"#;
        let input: HookInput = serde_json::from_str(json).expect("empty object");
        assert!(input.session_id.is_none());
        assert!(input.cwd.is_none());
        assert!(input.reason.is_none());
        assert!(input.transcript_path.is_none());
    }

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
}
