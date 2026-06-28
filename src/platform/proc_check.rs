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
///
/// `base` must be the **basename only** (no path separators) with `.exe` already
/// stripped on Windows.
pub fn is_claude_or_node_name(base: &str) -> bool {
    let lower = base.to_ascii_lowercase();
    lower.ends_with("claude") || lower.ends_with("node")
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
    use super::is_claude_or_node_name;

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
}
