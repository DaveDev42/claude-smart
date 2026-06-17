//! Session liveness check.
//!
//! `sid_live(sid)` answers: "Is there a live Claude Code process managing
//! session `sid` right now?"
//!
//! ## Algorithm (spec §2 "_sid-live")
//!
//! 1. Read `<smart_dir>/<sid>.pid` — one line `"<pid> <born>"`.
//!    If the file is absent or unparseable → `false` (no managed process).
//! 2. Verify that `pid` is a running process whose executable basename
//!    (case-insensitive, `.exe` stripped on Windows) ends with `"claude"` or
//!    `"node"`.
//!
//! The PID→comm check is delegated to the platform abstraction:
//! - **macOS / Linux:** `PosixProcCheck` (`ps -o comm= -p <pid>` — never
//!   `-o args=`, which would leak `CLAUDE_CONFIG_DIR` into logs).
//! - **Windows / Linux (alternative):** `SysinfoProcCheck` (targeted
//!   `sysinfo::refresh_process(Pid)` — never a full sweep on the hot Stop
//!   path).
//!
//! TOCTOU note: the comm check is best-effort.  The caller may record `born`
//! separately and perform a born-match guard for stricter safety (the relaunch
//! loop does this).  `sid_live` intentionally does not do the born-match itself
//! — it is a quick annotation helper for the picker and the auto-resume path.
//!
//! ## Phase 0
//!
//! The body is `unimplemented!()` — the platform proc-check trait and its
//! impls live in `platform/proc_check.rs` (Phase 7 in the scaffold order).
//! The signature and types are final.

use crate::paths;

/// Return `true` if session `sid` has a live managed `claude`/`node` process.
///
/// Reads `<smart_dir>/<sid>.pid`, extracts the PID, and delegates the
/// process-comm check to the platform implementation.
///
/// Returns `false` on any of:
/// - `<sid>.pid` absent,
/// - `<sid>.pid` unparseable,
/// - the recorded PID is not running,
/// - the running process's comm does not end with `"claude"` or `"node"`.
///
/// Never panics.
pub fn sid_live(sid: &str) -> bool {
    // Phase 0: types and wiring are final; implementation is Phase 7.
    // The `_pid_path` binding exists so callers can see the paths usage pattern.
    let _pid_path = paths::pid_file(sid);
    unimplemented!(
        "sid_live: read <sid>.pid → parse (pid, born) → platform proc-check \
         (Phase 7 — see platform/proc_check.rs PosixProcCheck / SysinfoProcCheck)"
    )
}

/// Parse the two-token PID-file content `"<pid> <born>"`.
///
/// Returns `None` on any parse failure (absent, non-UTF-8, wrong token count,
/// non-numeric tokens).  This matches the spec §6 read-compat contract:
/// "parse failure = absent".
///
/// This is a pure function — always fully implemented (trivial, no platform
/// dependency).
pub(crate) fn parse_pid_file(content: &str) -> Option<(u32, i64)> {
    let mut tokens = content.split_whitespace();
    let pid: u32 = tokens.next()?.parse().ok()?;
    let born: i64 = tokens.next()?.parse().ok()?;
    // Any extra tokens are silently ignored for forward-compatibility.
    Some((pid, born))
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // parse_pid_file is REAL (pure, no platform deps) — full test coverage here.

    #[test]
    fn parse_pid_file_basic() {
        let (pid, born) = parse_pid_file("12345 1718000000\n")
            .expect("should parse pid+born");
        assert_eq!(pid, 12345);
        assert_eq!(born, 1_718_000_000);
    }

    #[test]
    fn parse_pid_file_no_trailing_newline() {
        let (pid, born) = parse_pid_file("99 42")
            .expect("no newline is fine");
        assert_eq!(pid, 99);
        assert_eq!(born, 42);
    }

    #[test]
    fn parse_pid_file_extra_whitespace() {
        let (pid, born) = parse_pid_file("  1000  9999999999  ")
            .expect("leading/trailing whitespace is fine");
        assert_eq!(pid, 1000);
        assert_eq!(born, 9_999_999_999);
    }

    #[test]
    fn parse_pid_file_extra_tokens_ignored() {
        // Forward-compat: future versions may append more fields.
        let (pid, born) = parse_pid_file("1 2 extra tokens here")
            .expect("extra tokens are ignored");
        assert_eq!(pid, 1);
        assert_eq!(born, 2);
    }

    #[test]
    fn parse_pid_file_empty_returns_none() {
        assert!(parse_pid_file("").is_none());
        assert!(parse_pid_file("   ").is_none());
    }

    #[test]
    fn parse_pid_file_only_pid_returns_none() {
        // born epoch is required — one token is not enough.
        assert!(parse_pid_file("12345").is_none());
    }

    #[test]
    fn parse_pid_file_non_numeric_pid_returns_none() {
        assert!(parse_pid_file("abc 1718000000").is_none());
    }

    #[test]
    fn parse_pid_file_non_numeric_born_returns_none() {
        assert!(parse_pid_file("12345 notanumber").is_none());
    }

    #[test]
    fn parse_pid_file_negative_born_is_valid() {
        // born is i64; a negative epoch is unusual but must not panic.
        let (pid, born) = parse_pid_file("1 -1").expect("negative born is valid i64");
        assert_eq!(pid, 1);
        assert_eq!(born, -1);
    }

    #[test]
    fn parse_pid_file_pid_overflow_returns_none() {
        // u32::MAX + 1 cannot fit in u32 → None.
        let too_large = "4294967296 1718000000"; // 2^32
        assert!(parse_pid_file(too_large).is_none());
    }
}
