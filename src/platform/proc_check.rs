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
    /// Returns `true` iff `pid` is running AND its process name, exe basename,
    /// or argv[0] basename (case-insensitive, `.exe` stripped) ends with
    /// "claude" or "node". See `identity_matches` for why all three are checked.
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
pub(crate) fn is_name_for(base: &str, launch: &[std::ffi::OsString]) -> bool {
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

/// Reduce a process name, exe path, or argv[0] to the bare basename
/// [`is_name_for`] expects: the directory part dropped (either separator, so a
/// Windows path reduces the same way on every host) and a trailing `.exe`
/// stripped in any case.
pub(crate) fn bare_basename(id: &str) -> &str {
    let base = id.rsplit(['/', '\\']).next().unwrap_or(id);
    let cut = base.len().saturating_sub(4);
    match base.get(cut..) {
        Some(ext) if ext.eq_ignore_ascii_case(".exe") => &base[..cut],
        _ => base,
    }
}

/// Pure identity seam: a process is "ours" iff ANY of its identifiers (process
/// name, exe path, argv[0]), reduced to a bare basename, passes [`is_name_for`].
///
/// No single identifier is enough on its own. The native installer runs
/// `claude` through a symlink to a versioned file
/// (`~/.local/share/claude/versions/2.1.266`), so wherever the OS resolves that
/// link for the exe path (Linux `/proc/<pid>/exe`, macOS `proc_pidpath`) the
/// exe basename is a version string. The process name (`p_comm` on macOS,
/// `/proc/<pid>/stat` comm on Linux, the image name on Windows) and argv[0]
/// keep the `claude` it was launched as.
fn identity_matches(ids: &[Option<&str>], launch: &[std::ffi::OsString]) -> bool {
    ids.iter()
        .flatten()
        .any(|id| is_name_for(bare_basename(id), launch))
}

/// `SysinfoProcCheck` uses the `sysinfo` crate for a **targeted** single-process
/// refresh (never `refresh_all()` — a full sweep stalls the hot Stop path on a
/// busy Windows box).
///
/// `platform/mod.rs` wires this as `PlatformProcCheck` on every target — sysinfo
/// gives a targeted name/exe/argv lookup on macOS, Linux/WSL, and Windows alike,
/// so there is no external `ps` spawn and no per-OS proc-check code to maintain.
pub struct SysinfoProcCheck;

impl ProcCheck for SysinfoProcCheck {
    fn is_live_claude_or_node(pid: u32) -> bool {
        use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

        // `System::new()` starts empty and `refresh_processes_specifics` loads
        // exactly this one pid, so the process table is never swept.
        // `ProcessRefreshKind::nothing()` leaves exe and cmd unset (the name is
        // always filled), so both are requested explicitly.
        let pid = Pid::from_u32(pid);
        let mut sys = System::new();
        let kind = ProcessRefreshKind::nothing()
            .with_exe(UpdateKind::OnlyIfNotSet)
            .with_cmd(UpdateKind::OnlyIfNotSet);
        if sys.refresh_processes_specifics(ProcessesToUpdate::Some(&[pid]), true, kind) == 0 {
            return false;
        }
        let Some(proc_) = sys.process(pid) else {
            return false;
        };
        let name = proc_.name().to_str();
        let exe = proc_.exe().and_then(|p| p.to_str());
        let argv0 = proc_.cmd().first().and_then(|s| s.to_str());
        identity_matches(
            &[name, exe, argv0],
            &crate::config::resolve_launch_command(),
        )
    }
}

/// Poll `is_live_claude_or_node` until it returns true or `timeout` elapses,
/// returning false in that case. On Linux with glibc, `Command::spawn`
/// resumes the parent as soon as the child has *started* its `execve`,
/// before the kernel has published the child's `comm` or
/// `/proc/<pid>/cmdline` — so a freshly spawned pid can briefly fail every
/// identity check even though it is about to become a live `claude` process.
/// Production is unaffected: the check there runs against sessions that have
/// been alive for seconds to hours, never against a pid spawned microseconds
/// earlier.
#[cfg(test)]
pub(crate) fn wait_until_live_claude_or_node(pid: u32, timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if SysinfoProcCheck::is_live_claude_or_node(pid) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::wait_until_live_claude_or_node;
    use super::{bare_basename, identity_matches, is_claude_or_node_name, is_name_for};
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
        // The process name of a real claude is always "claude" or "node".
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

    // ─── identity seam: process name / exe path / argv[0] ──────────────────────

    #[test]
    fn bare_basename_strips_dirs_and_exe_suffix() {
        assert_eq!(bare_basename("claude"), "claude");
        assert_eq!(bare_basename("/Users/example/.local/bin/claude"), "claude");
        assert_eq!(
            bare_basename(r"C:\Users\example\.local\bin\claude.exe"),
            "claude"
        );
        assert_eq!(bare_basename("CLAUDE.EXE"), "CLAUDE");
        assert_eq!(bare_basename("node.exe"), "node");
        assert_eq!(bare_basename(""), "");
        assert_eq!(bare_basename(".exe"), "");
        // The 4-byte suffix window would split a multibyte char: left unchanged.
        assert_eq!(bare_basename("ééx"), "ééx");
    }

    #[test]
    fn native_installer_matches_despite_versioned_exe() {
        // Where the OS resolves the installer's symlink, the exe basename is the
        // version; the name and argv[0] still say claude.
        let l = launch(&["claude"]);
        let versioned = "/Users/example/.local/share/claude/versions/2.1.266";
        assert!(identity_matches(
            &[Some("claude"), Some(versioned), Some("claude")],
            &l
        ));
        // The version-string exe on its own is never enough.
        assert!(!identity_matches(&[None, Some(versioned), None], &l));
        assert!(!identity_matches(
            &[Some("2.1.266"), Some(versioned), None],
            &l
        ));
    }

    #[test]
    fn any_single_identifier_is_enough() {
        let l = launch(&["claude"]);
        assert!(identity_matches(&[Some("claude"), None, None], &l));
        assert!(identity_matches(
            &[Some(""), Some("/usr/local/bin/node"), None],
            &l
        ));
        assert!(identity_matches(
            &[
                Some("2.1.266"),
                None,
                Some("/home/example/.local/bin/claude")
            ],
            &l
        ));
        // Windows image names and paths, in any case.
        assert!(identity_matches(&[Some("CLAUDE.EXE"), None, None], &l));
        assert!(identity_matches(
            &[Some(""), Some(r"C:\Program Files\nodejs\node.exe"), None],
            &l
        ));
    }

    #[test]
    fn unrelated_process_never_matches() {
        let l = launch(&["claude"]);
        assert!(!identity_matches(
            &[Some("zsh"), Some("/bin/zsh"), Some("-zsh")],
            &l
        ));
        assert!(!identity_matches(&[None, None, None], &l));
        assert!(!identity_matches(&[], &l));
        // A directory named claude does not make what runs from it claude.
        assert!(!identity_matches(
            &[Some("python3"), Some("/opt/claude/bin/python3"), None],
            &l
        ));
    }

    #[test]
    fn configured_launcher_matches_through_any_identifier() {
        let l = launch(&["happy"]);
        assert!(identity_matches(&[Some("happy"), None, None], &l));
        assert!(identity_matches(
            &[Some(""), None, Some("/usr/local/bin/happy")],
            &l
        ));
        assert!(!identity_matches(
            &[Some("bash"), Some("/bin/bash"), Some("bash")],
            &l
        ));
    }

    // ─── live sysinfo lookup ───────────────────────────────────────────────────

    /// Regression test for the kill-gate: a process launched as `claude` through
    /// a symlink (the native installer's layout) must be recognized. The refresh
    /// used to leave `exe` unset and nothing else was checked, so this was
    /// always `false` and no limit switch could stop a session. The lookup is
    /// polled via `wait_until_live_claude_or_node` rather than checked once,
    /// to ride out the exec window described on that helper's doc comment.
    #[cfg(unix)]
    #[test]
    fn live_symlinked_claude_is_recognized() {
        use super::{ProcCheck, SysinfoProcCheck};

        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("claude");
        std::os::unix::fs::symlink("/bin/sleep", &link).unwrap();
        let mut child = std::process::Command::new(&link).arg("30").spawn().unwrap();
        let pid = child.id();
        let live = wait_until_live_claude_or_node(pid, std::time::Duration::from_secs(5));
        let _ = child.kill();
        let _ = child.wait();
        assert!(live, "a live process launched as `claude` must pass");
        assert!(
            !SysinfoProcCheck::is_live_claude_or_node(pid),
            "a reaped pid must not pass"
        );
    }

    #[test]
    fn live_unrelated_process_is_rejected() {
        use super::{ProcCheck, SysinfoProcCheck};
        // The test binary is live but named after the crate, not claude/node.
        assert!(!SysinfoProcCheck::is_live_claude_or_node(std::process::id()));
    }
}
