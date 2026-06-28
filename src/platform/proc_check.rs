/// "Is PID a live claude or node process?"
///
/// Used by:
///   1. Clobber guard in the relaunch loop (skip pidfile write if someone else owns it).
///   2. Hook kill-gate (only stop a session whose PID is a live claude/node).
///   3. Picker live-annotation (`_sid-live`).
///
/// TOCTOU note: the caller records `born` separately and compares *after* this
/// returns `true` (born-match guard in the relaunch loop).  This trait does NOT
/// protect against PID recycling on its own.
pub trait ProcCheck {
    /// Returns `true` iff `pid` is running AND its exe basename (case-insensitive,
    /// `.exe` stripped on Windows) ends with "claude" or "node".
    fn is_live_claude_or_node(pid: u32) -> bool;
}

/// Shared name-matching logic used by every `ProcCheck` implementation.
///
/// Tolerates:
/// - Renamed Node builds (`claude`, `claude-3`, etc.)
/// - Full path components (`/usr/bin/node`, `node.exe`)
/// - Linux `ps` comm truncation at 15 chars — "claude" (6) and "node" (4) both fit
/// - A configured drop-in launch binary (`happy`/`tp`) whose own basename is
///   neither `claude` nor `node` — so a renamed launcher's child is still
///   recognized as "ours" (see [`is_name_for`]).
///
/// `base` must be the **basename only** (no path separators) with `.exe` already
/// stripped on Windows.
pub fn is_claude_or_node_name(base: &str) -> bool {
    is_name_for(base, &crate::config::resolve_launch_command())
}

/// Pure name-match seam (no env/file I/O): `base` is "ours" iff it is a
/// canonical claude/node basename OR the exact basename of the configured
/// `launch` command's binary.
///
/// The configured-binary branch is purely additive: with the default
/// `["claude"]` (or anything ending in `claude`/`node`) it never adds a match,
/// so behavior is unchanged when nothing is configured.
fn is_name_for(base: &str, launch: &[std::ffi::OsString]) -> bool {
    let lower = base.to_ascii_lowercase();
    if lower.ends_with("claude") || lower.ends_with("node") {
        return true;
    }
    // Exact basename of the configured launch binary (e.g. "happy"/"tp"), with a
    // trailing ".exe" stripped on Windows. Skipped when it is itself claude/node
    // (already covered above).
    let name = launch
        .first()
        .map(std::path::Path::new)
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .trim_end_matches(".exe")
        .to_ascii_lowercase();
    !name.is_empty() && name != "claude" && name != "node" && lower == name
}

/// `SysinfoProcCheck` uses the `sysinfo` crate for a **targeted** single-process
/// refresh (never `refresh_all()` — a full sweep stalls the hot Stop path on a
/// busy Windows box).
///
/// `platform/mod.rs` wires this as `PlatformProcCheck` on every target — sysinfo
/// gives a targeted exe()-basename lookup on macOS, Linux/WSL, and Windows alike,
/// so there is no external `ps` spawn and no per-OS proc-check code to maintain.
pub struct SysinfoProcCheck;

impl ProcCheck for SysinfoProcCheck {
    fn is_live_claude_or_node(pid: u32) -> bool {
        use sysinfo::{Pid, ProcessRefreshKind, RefreshKind, System};

        let pid = Pid::from_u32(pid);
        let mut sys = System::new_with_specifics(
            RefreshKind::new().with_processes(ProcessRefreshKind::new()),
        );
        sys.refresh_processes_specifics(ProcessRefreshKind::new());

        if let Some(proc) = sys.process(pid) {
            if let Some(exe) = proc.exe() {
                let stem = exe
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    // strip .exe for Windows paths
                    .trim_end_matches(".exe");
                return is_claude_or_node_name(stem);
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::{is_claude_or_node_name, is_name_for};
    use std::ffi::OsString;

    /// Build a launch token vec for the pure-seam tests.
    fn launch(toks: &[&str]) -> Vec<OsString> {
        toks.iter().map(OsString::from).collect()
    }

    #[test]
    fn test_claude_variants() {
        assert!(is_claude_or_node_name("claude"));
        assert!(is_claude_or_node_name("Claude"));
        assert!(is_claude_or_node_name("CLAUDE"));
        // Stripped .exe (Windows): "claude.exe" → strip → "claude"
        assert!(is_claude_or_node_name("claude"));
        // Note: "claude-3" ends with "-3", NOT "claude" — correctly NOT matched.
        // The comm/exe basename for real claude binaries is always "claude" or "node".
        assert!(!is_claude_or_node_name("claude-3"));
    }

    #[test]
    fn test_node_variants() {
        assert!(is_claude_or_node_name("node"));
        assert!(is_claude_or_node_name("Node"));
        assert!(is_claude_or_node_name("NODE"));
    }

    #[test]
    fn test_non_matches() {
        assert!(!is_claude_or_node_name("bash"));
        assert!(!is_claude_or_node_name("zsh"));
        assert!(!is_claude_or_node_name("python3"));
        assert!(!is_claude_or_node_name(""));
        // Partial prefix — does NOT end with "claude"
        assert!(!is_claude_or_node_name("not-claud"));
        // "claude-3" ends with "-3", not "claude" — correctly rejected
        assert!(!is_claude_or_node_name("claude-3"));
        // "nodemon" ends with "mon", not "node" — correctly rejected
        assert!(!is_claude_or_node_name("nodemon"));
    }

    #[test]
    fn test_wsl_interop_paths() {
        // Simulate a basename extracted from a WSL interop path like
        // /mnt/c/…/claude.  The caller strips the path and .exe before calling.
        assert!(is_claude_or_node_name("claude"));
        assert!(is_claude_or_node_name("node"));
    }

    // ─── configured drop-in launch binary (pure seam) ──────────────────────────

    #[test]
    fn launch_binary_matches_exact_basename() {
        // A renamed launcher (`happy`/`tp`) is recognized as ours.
        assert!(is_name_for("happy", &launch(&["happy"])));
        assert!(is_name_for("tp", &launch(&["tp"])));
        // Case-insensitive, like the claude/node branch.
        assert!(is_name_for("Happy", &launch(&["happy"])));
        // First token is the binary even for a multi-token command (`npx happy`).
        assert!(is_name_for("npx", &launch(&["npx", "happy"])));
    }

    #[test]
    fn launch_binary_path_and_exe_stripped() {
        // A full path / .exe suffix on the configured binary still matches by
        // basename.
        assert!(is_name_for("happy", &launch(&["/usr/local/bin/happy"])));
        assert!(is_name_for("happy", &launch(&["happy.exe"])));
    }

    #[test]
    fn launch_binary_no_false_match() {
        // An unrelated process is NOT ours just because a launcher is configured.
        assert!(!is_name_for("bash", &launch(&["happy"])));
        assert!(!is_name_for("zsh", &launch(&["happy"])));
    }

    #[test]
    fn default_claude_adds_no_extra_match() {
        // With the default `["claude"]`, the additive branch is inert: only the
        // canonical claude/node names match, exactly as before.
        assert!(is_name_for("claude", &launch(&["claude"])));
        assert!(is_name_for("node", &launch(&["claude"])));
        assert!(!is_name_for("happy", &launch(&["claude"])));
        assert!(!is_name_for("bash", &launch(&["claude"])));
        // Empty launch (defensive): falls back to claude/node-only matching.
        assert!(!is_name_for("happy", &launch(&[])));
        assert!(is_name_for("claude", &launch(&[])));
    }
}
