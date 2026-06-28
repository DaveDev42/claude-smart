//! Kill mechanics for the reaper — the thin syscall shell + a pure result
//! summary.
//!
//! The user selects exactly which pids to kill in the picker, so we kill the
//! **selected set, pid by pid** — never a blanket `kill(-pgid)` that could reach
//! processes the user did not pick (or, worse, ourselves). Default signal is
//! SIGKILL (immediate, unambiguous — the supervisor relationship is already
//! gone); `--term` opts into SIGTERM-first with no escalation wait.
//!
//! On Windows there is no signal model: every kill is `TerminateProcess`, and
//! `--term` is accepted but ignored (documented). A pid that is already gone is
//! treated as **success** (the goal — it being dead — is met), not an error.

/// What signal to deliver to each selected pid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillSignal {
    /// SIGKILL (POSIX) / `TerminateProcess` (Windows) — immediate, default.
    Kill,
    /// SIGTERM (POSIX) — let the process clean up; no escalation. On Windows this
    /// maps to `TerminateProcess` (no SIGTERM analogue) — the kill is forceful
    /// regardless, which the help text documents.
    Term,
}

/// Outcome of a single pid's kill attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KillOutcome {
    /// The signal was delivered.
    Signalled,
    /// The pid was already gone (POSIX `ESRCH`) — the goal is met, so success.
    AlreadyGone,
    /// The kill failed (e.g. `EPERM`); carries a short reason for reporting.
    Failed(String),
}

/// PURE: summarize a batch of `(pid, outcome)` results into a one-line report.
///
/// Separated from the syscall loop so the reporting is unit-testable without
/// actually killing anything.
pub fn summarize(results: &[(u32, KillOutcome)]) -> String {
    let mut signalled = 0usize;
    let mut already = 0usize;
    let mut failed: Vec<&u32> = Vec::new();
    for (pid, o) in results {
        match o {
            KillOutcome::Signalled => signalled += 1,
            KillOutcome::AlreadyGone => already += 1,
            KillOutcome::Failed(_) => failed.push(pid),
        }
    }
    let mut parts = Vec::new();
    if signalled > 0 {
        parts.push(format!("{signalled} killed"));
    }
    if already > 0 {
        parts.push(format!("{already} already gone"));
    }
    if !failed.is_empty() {
        let pids: Vec<String> = failed.iter().map(|p| p.to_string()).collect();
        parts.push(format!("{} failed (pid {})", failed.len(), pids.join(", ")));
    }
    if parts.is_empty() {
        "csm reap: nothing to kill".to_string()
    } else {
        format!("csm reap: {}", parts.join(", "))
    }
}

/// Deliver `signal` to each pid, collecting per-pid outcomes. Thin I/O shell over
/// the platform `kill_one`.
pub fn kill_all(pids: &[u32], signal: KillSignal) -> Vec<(u32, KillOutcome)> {
    pids.iter()
        .map(|&pid| (pid, kill_one(pid, signal)))
        .collect()
}

/// Deliver `signal` to one pid. POSIX implementation.
#[cfg(unix)]
fn kill_one(pid: u32, signal: KillSignal) -> KillOutcome {
    use nix::errno::Errno;
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;

    let sig = match signal {
        KillSignal::Kill => Signal::SIGKILL,
        KillSignal::Term => Signal::SIGTERM,
    };
    match kill(Pid::from_raw(pid as i32), sig) {
        Ok(()) => KillOutcome::Signalled,
        // The process is already gone — the goal is met.
        Err(Errno::ESRCH) => KillOutcome::AlreadyGone,
        Err(e) => KillOutcome::Failed(e.to_string()),
    }
}

/// Deliver `signal` to one pid. Windows implementation: `TerminateProcess`.
/// `signal` is ignored (no SIGTERM analogue) — the kill is always forceful.
#[cfg(windows)]
fn kill_one(pid: u32, _signal: KillSignal) -> KillOutcome {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};

    // SAFETY: documented Win32 process APIs. We open with TERMINATE rights,
    // terminate, then close the handle. A null handle means the process is gone
    // (or not ours) — treat "gone" as success since the goal is met.
    //
    // `HANDLE` is `isize` in windows-sys 0.52 (not a pointer), so the failure
    // sentinel is `0`, not `is_null()`.
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if handle == 0 {
            // Could not open — most commonly the pid no longer exists.
            return KillOutcome::AlreadyGone;
        }
        let ok = TerminateProcess(handle, 1);
        CloseHandle(handle);
        if ok != 0 {
            KillOutcome::Signalled
        } else {
            KillOutcome::Failed("TerminateProcess failed".to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarize_mixed_batch() {
        let results = vec![
            (100, KillOutcome::Signalled),
            (200, KillOutcome::Signalled),
            (300, KillOutcome::AlreadyGone),
            (400, KillOutcome::Failed("EPERM".into())),
        ];
        let s = summarize(&results);
        assert!(s.contains("2 killed"), "{s}");
        assert!(s.contains("1 already gone"), "{s}");
        assert!(s.contains("1 failed (pid 400)"), "{s}");
    }

    #[test]
    fn summarize_all_success_omits_failed_clause() {
        let results = vec![(1, KillOutcome::Signalled), (2, KillOutcome::AlreadyGone)];
        let s = summarize(&results);
        assert!(s.contains("1 killed") && s.contains("1 already gone"));
        assert!(!s.contains("failed"), "no failures → no failed clause: {s}");
    }

    #[test]
    fn summarize_empty_is_nothing_to_kill() {
        assert_eq!(summarize(&[]), "csm reap: nothing to kill");
    }

    #[cfg(unix)]
    #[test]
    fn kill_nonexistent_pid_is_already_gone() {
        // A very high pid unlikely to be live. ESRCH → AlreadyGone. We assert only
        // the not-Signalled shape so the test is robust even in the cosmic case
        // that the pid is somehow live (then EPERM → Failed, still not asserted).
        let outcome = kill_one(4_000_000_000, KillSignal::Term);
        assert!(
            matches!(outcome, KillOutcome::AlreadyGone | KillOutcome::Failed(_)),
            "high nonexistent pid should be AlreadyGone (or Failed on EPERM), got {outcome:?}"
        );
    }
}
