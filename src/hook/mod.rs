//! `csm hook` — Claude Code Stop/SubagentStop/SessionEnd hook handler.
//!
//! Invoked by Claude Code as a hook process with the event JSON on stdin.
//! This is the **`csm hook` subcommand** — there is no separate `csm-hook` binary
//! (single-binary form, per locked scaffold decision).
//!
//! Commit ordering (mirrors `limit-switch.sh.j2` §4b):
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

pub mod detect;
pub mod notify;
pub mod stop;

use std::path::Path;

use anyhow::Context as _;

pub use detect::HookInput;

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
    // classify() reproduces the full limit-switch.sh.j2 flow including kill-switches,
    // reason gate, detection tiers, managed-session gate, cooldown, and hop guard.
    let decision = detect::classify(&input, owner_dir)?;

    match decision {
        detect::Decision::Skip => {
            // Nothing to do — a kill-switch, cooldown, marker, or no-limit result.
        }

        detect::Decision::NotifyOnly { ref message } => {
            // Notify-only: user-quit + limited, no-target, detect-only mode, or
            // unmanaged session. Emit OSC 777 notify on stdout.
            // Log goes to the smart_dir limit-switch.log.
            let log_msg = format!("notify-only sid={} msg={}", &sid[..sid.len().min(8)], message);
            notify::emit_osc777(message).unwrap_or(()); // best-effort stdout
            let _ = notify::append_log(&sid, &log_msg, owner_dir); // best-effort log
        }

        detect::Decision::LimitSwitch {
            ref message,
            ref target_profile,
            ref handoff,
            ref cwd,
            born,
        } => {
            // Full limit-switch commit sequence (§4b ordering):
            // notify first (stdout before any mutation), then commit_and_stop.
            notify::emit_osc777(message).unwrap_or(());

            let log_msg = format!(
                "limit-switch sid={} to={} cwd={} hop={}",
                &sid[..sid.len().min(8)],
                target_profile,
                cwd,
                born,
            );
            let _ = notify::append_log(&sid, &log_msg, owner_dir);

            stop::commit_and_stop(sid.as_str(), target_profile, handoff, cwd, born, owner_dir)
                .with_context(|| format!("commit_and_stop failed for session {sid}"))?;
        }
    }

    Ok(())
}
