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
    let decision = detect::classify(&input, owner_dir)?;

    match decision {
        detect::Decision::Skip => {
            // Nothing to do (kill-switch, cooldown, reason gate, etc.)
        }
        detect::Decision::NotifyOnly { ref message } => {
            // Reason gate: user-quit events → notify only, no relaunch.
            notify::emit_osc777(message)?;
            notify::append_log(&sid, message, owner_dir)?;
        }
        detect::Decision::LimitSwitch {
            ref message,
            ref target_profile,
            ref handoff,
        } => {
            // Full limit-switch commit sequence (§4b ordering):
            // 1–5 handled by stop::commit_and_stop (sentinel write precedes stop signal)
            notify::emit_osc777(message)?;
            notify::append_log(&sid, message, owner_dir)?;
            stop::commit_and_stop(&sid, target_profile, handoff, owner_dir)?;
        }
    }

    Ok(())
}
