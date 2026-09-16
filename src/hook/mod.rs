//! `csm hook` — Claude Code Stop/SubagentStop/SessionEnd hook handler.
//!
//! Invoked by Claude Code as a hook process with the event JSON on stdin.
//! This is the **`csm hook` subcommand** — there is no separate `csm-hook` binary
//! (single-binary form, per locked scaffold decision).
//!
//! Commit ordering (matches the legacy shell implementation):
//!   1. merge-sidecar hop
//!   2. write `.relaunch` sentinel (atomic tmp+rename)
//!   3. noclobber-create `.switched` marker
//!   4. re-stamp `.last-switch`
//!   5. write `<sid>.stop` flag (Windows) / `kill(pid, SIGTERM)` (POSIX)
//!      — stop is LAST: supervisor must see a complete sentinel before being asked to stop.
//!
//! `--owner <dir>` is the CLAUDE_CONFIG_DIR of the profile that owns this hook instance.
//! It is baked into the per-profile shim deployed by ansible; the hook uses it to locate
//! the correct profile context.
//!
//! # Second entry point: the statusline tick
//!
//! [`run_from_statusline`] runs the same classification off `csm usage
//! capture` (and the capture inside `csm statusline`). It exists because a
//! subscription cap does not produce a hook event at all: when a request
//! fails with a 429 that carries a reset time, Claude Code (observed on
//! 2.1.270) neither ends the turn with `StopFailure` nor fires `Stop` — it
//! shows "Weekly limit reached · Retrying in 6h" and parks the turn in an
//! internal auto-retry wait, indefinitely. The only csm code that still runs
//! while the turn is parked is the statusLine command, which Claude Code keeps
//! invoking about once a second with the live `rate_limits` for the session's
//! account. So the tick is where the switch has to happen. Its stdout is not a
//! terminal (the statusLine wrapper discards it), so this path never emits the
//! OSC 777 notification — the relaunched session's handoff prompt is the
//! user-visible signal.

pub mod detect;
pub mod notify;
pub mod stop;

use std::path::Path;

use anyhow::Context as _;

/// Read the `hop` field from `<sid>.json` sidecar, tolerating both String and
/// Number forms. Returns 0 on missing/corrupt sidecar (the legacy zsh wrote
/// hop as a JSON string; readers accept both forms). Delegates to the single
/// `Sidecar::hop_int` SSOT so the String/Number tolerance rule lives in
/// exactly one place — this was previously duplicated byte-for-byte in
/// `detect.rs` and `stop.rs`.
pub(crate) fn read_sidecar_hop(sid: &str) -> i64 {
    crate::sidecar::read_sidecar(&crate::paths::sidecar(sid))
        .map(|s| s.hop_int())
        .unwrap_or(0)
}

/// First 8 bytes of a session UUID, for compact log lines and handoff
/// prompts. Panic-free on any input (unlike a raw `&sid[..8]` slice, which
/// panics on a session id shorter than 8 bytes).
pub(crate) fn sid_short(sid: &str) -> &str {
    sid.get(..8).unwrap_or(sid)
}

/// Build one `limit-switch.log` line. `kind` is `"notify-only"` or
/// `"limit-switch"`; `detail` carries the kind-specific fields (`msg=…`, or
/// `to=… cwd=… born=…`); `via` is `Some("statusline")` for the statusline
/// entry point and `None` for the hook entry point (whose lines carry no
/// `via=` suffix, matching the format before this helper existed).
fn decision_log_line(kind: &str, sid_short: &str, detail: &str, via: Option<&str>) -> String {
    match via {
        Some(via) => format!("{kind} sid={sid_short} {detail} via={via}"),
        None => format!("{kind} sid={sid_short} {detail}"),
    }
}

/// Entry point for `csm hook [--owner <profile_dir>]`.
///
/// `owner_dir` is the profile directory (value of CLAUDE_CONFIG_DIR for the hook's
/// owning profile). It is used to resolve profile context when needed. The hook
/// reads event JSON from stdin and, depending on the detected limit state,
/// writes the relaunch sentinel and signals the supervisor to stop.
pub fn run(owner_dir: &Path) -> anyhow::Result<()> {
    // Parse hook input from stdin.
    let input = detect::parse_stdin().context("failed to parse hook stdin JSON")?;

    // session_id is required — exit 0 silently if missing (hook contract).
    let sid = match &input.session_id {
        Some(s) if !s.is_empty() => s.clone(),
        _ => {
            // No session_id — exit cleanly; hook contract says exit 0.
            return Ok(());
        }
    };

    // Classify the hook event and determine whether a limit-switch is warranted.
    // classify() reproduces the full legacy shell implementation's flow including
    // kill-switches, reason gate, detection tiers, managed-session gate, cooldown,
    // and hop guard.
    let decision = detect::classify(&input, owner_dir)?;

    match decision {
        detect::Decision::Skip => {
            // Nothing to do — a kill-switch, cooldown, marker, or no-limit result.
        }

        detect::Decision::NotifyOnly { ref message } => {
            // Notify-only: user-quit + limited, no-target, detect-only mode, or
            // unmanaged session. Emit OSC 777 notify on stdout.
            // Log goes to the smart_dir limit-switch.log.
            let log_msg = decision_log_line(
                "notify-only",
                sid_short(&sid),
                &format!("msg={message}"),
                None,
            );
            notify::emit_osc777(message).unwrap_or(()); // best-effort stdout
            let _ = notify::append_log(&sid, &log_msg); // best-effort log
        }

        detect::Decision::LimitSwitch {
            ref message,
            ref target_profile,
            ref handoff,
            ref cwd,
            born,
        } => {
            // Full limit-switch commit sequence (matches the legacy shell
            // implementation's ordering): notify first (stdout before any
            // mutation), then commit_and_stop.
            notify::emit_osc777(message).unwrap_or(());

            let log_msg = decision_log_line(
                "limit-switch",
                sid_short(&sid),
                &format!("to={target_profile} cwd={cwd} born={born}"),
                None,
            );
            let _ = notify::append_log(&sid, &log_msg);

            stop::commit_and_stop(sid.as_str(), target_profile, handoff, cwd, born)
                .with_context(|| format!("commit_and_stop failed for session {sid}"))?;
        }
    }

    Ok(())
}

/// Entry point for the statusline tick. `raw` is the statusLine stdin JSON
/// exactly as `csm usage capture` read it; `capture` is what
/// [`crate::usage::local::record_statusline_payload`] just made of it.
///
/// Order of checks, cheapest first, because this runs about once a second
/// for every live session:
///
/// 1. [`detect::statusline_limit`] over the merged reading — pure, no I/O.
///    Almost every tick ends here.
/// 2. Parse `raw` as a [`detect::HookInput`] (statusLine stdin carries the
///    same `session_id`/`cwd`/`transcript_path` keys a hook event does).
/// 3. [`detect::classify_with`] with the reading as a definitive live limit.
///    Kill-switches, `.switched`, target pick, relaunch switch, managed gate,
///    cooldown exception and hop guard all apply exactly as for the hook.
/// 4. On `LimitSwitch`, claim `.switched` first ([`stop::claim_switched`] —
///    ticks overlap; only one may commit), then [`stop::commit_and_stop`].
///    If the commit fails the claim is released so the next tick retries.
///
/// Never returns an error and never writes to stdout/stderr: the capture
/// this rides on is fire-and-forget and must stay that way. Outcomes are
/// logged to `limit-switch.log` with `via=statusline`.
pub fn run_from_statusline(raw: &str, capture: &crate::usage::local::StatuslineCapture) {
    let Some(limit_msg) = detect::statusline_limit(&capture.usage) else {
        return;
    };
    let Ok(input) = detect::parse_input(raw) else {
        return;
    };
    let Some(sid) = input.session_id.clone().filter(|s| !s.is_empty()) else {
        return;
    };
    let owner_dir = Path::new(&capture.profile_dir);
    let Ok(decision) = detect::classify_with(&input, owner_dir, Some(&limit_msg)) else {
        return;
    };
    let sid_short = sid_short(&sid);

    match decision {
        detect::Decision::Skip => {}

        detect::Decision::NotifyOnly { ref message } => {
            // Deduped by `.detected` inside classify, so this lands once per
            // session, not once per second.
            let log_msg = decision_log_line(
                "notify-only",
                sid_short,
                &format!("msg={message}"),
                Some("statusline"),
            );
            let _ = notify::append_log(&sid, &log_msg);
        }

        detect::Decision::LimitSwitch {
            message: _,
            ref target_profile,
            ref handoff,
            ref cwd,
            born,
        } => {
            if !stop::claim_switched(&sid) {
                return;
            }
            let log_msg = decision_log_line(
                "limit-switch",
                sid_short,
                &format!("to={target_profile} cwd={cwd} born={born}"),
                Some("statusline"),
            );
            let _ = notify::append_log(&sid, &log_msg);

            if let Err(e) = stop::commit_and_stop(sid.as_str(), target_profile, handoff, cwd, born)
            {
                let _ = notify::append_log(
                    &sid,
                    &decision_log_line(
                        "limit-switch",
                        sid_short,
                        &format!("commit failed: {e:#}"),
                        Some("statusline"),
                    ),
                );
                let _ = std::fs::remove_file(crate::paths::switched(&sid));
            }
        }
    }
}
