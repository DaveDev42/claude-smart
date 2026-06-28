//! Interactive orphan-process reaper — I/O shell.
//!
//! `claude` is launched by `csm` as a foreground child in its own process group
//! (`platform/posix.rs`). When claude (or `csm` itself) dies abnormally it can
//! leave residual processes behind: MCP servers, sandbox helpers, Bash-tool
//! background commands, or — if the supervisor died — a `claude` that outlived
//! it. This module discovers those candidates and (in later phases) lets the
//! user pick which to kill. **Phase 1 implements discovery + `--dry-run` only**:
//! it lists candidates and exits without an interactive picker or any kill.
//!
//! The decision logic lives in the pure `scan` submodule; this file is the thin
//! I/O shell that captures the live process table (sysinfo + `getpgid`) and the
//! clock, then calls in.

pub mod scan;

use std::ffi::OsString;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context as _;

use crate::paths;
use crate::platform::pid::read_pid_file;
use scan::{candidate_row, select_candidates, Candidate, ProcRow, Session};

/// Which sessions a reap run is scoped to.
#[derive(Debug, Clone)]
pub enum ReapScope {
    /// One explicit session id (`--session <sid>`).
    One(String),
    /// Every `csm`-managed session on this machine (`<smart_dir>/*.pid`).
    /// This is the Phase-1 default — pidfiles are the authoritative list of
    /// sessions `csm` is supervising (or was, before a crash).
    All,
}

impl ReapScope {
    /// Resolve the CLI flags into a scope. `--session` wins; otherwise `All`.
    pub fn resolve(session: Option<String>) -> Self {
        match session {
            Some(sid) => ReapScope::One(sid),
            None => ReapScope::All,
        }
    }
}

/// Current wall-clock as Unix epoch seconds (the `now` the pure core needs for
/// age formatting). Captured here so `scan` stays clock-free.
fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Read a `Session` anchor from `<sid>.pid`. Returns `None` when the pidfile is
/// absent or unparseable (no managed pid to anchor on → nothing to reap).
fn session_for(sid: &str) -> Option<Session> {
    match read_pid_file(&paths::pid_file(sid)) {
        Ok(Some((pid, born))) => Some(Session {
            sid: sid.to_string(),
            claude_pid: pid,
            // `born` is stored i64 (epoch seconds, always >= 0 in practice); the
            // pure core compares it against u64 start_times.
            born: born.max(0) as u64,
        }),
        _ => None,
    }
}

/// Enumerate the session ids `csm` is (or was) supervising: every `<sid>.pid`
/// under `smart_dir`. Best-effort — an unreadable dir yields an empty list.
fn all_session_ids() -> Vec<String> {
    let dir = paths::smart_dir_no_create();
    let mut ids = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("pid") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    ids.push(stem.to_string());
                }
            }
        }
    }
    ids.sort();
    ids
}

/// The sessions to inspect for the given scope.
fn sessions_for(scope: &ReapScope) -> Vec<Session> {
    let ids = match scope {
        ReapScope::One(sid) => vec![sid.clone()],
        ReapScope::All => all_session_ids(),
    };
    ids.iter().filter_map(|sid| session_for(sid)).collect()
}

// ─── live process-table snapshot ────────────────────────────────────────────

/// Capture the full live process table as `ProcRow`s.
///
/// One `System::new_all()` sweep (off the hot path — the reaper is never on the
/// latency-sensitive Stop path that `proc_check` warns about), then a per-pid
/// `getpgid` on POSIX. On Windows `getpgid` has no analogue, so `pgid` is left
/// `None` and only the ppid-walk net applies (a documented gap).
fn snapshot_proc_table() -> Vec<ProcRow> {
    use sysinfo::System;

    let sys = System::new_all();
    let mut rows = Vec::with_capacity(sys.processes().len());
    for (pid, proc_) in sys.processes() {
        let pid_u32 = pid.as_u32();
        let exe_base = proc_
            .exe()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .trim_end_matches(".exe")
            .to_ascii_lowercase();
        let cmd_snippet = cmd_snippet(proc_.cmd());
        rows.push(ProcRow {
            pid: pid_u32,
            ppid: proc_.parent().map(|p| p.as_u32()),
            pgid: pgid_of(pid_u32),
            start_time: proc_.start_time(),
            exe_base,
            cmd_snippet,
        });
    }
    rows
}

/// Process-group id of `pid` on POSIX (`getpgid`); `None` on Windows or error.
#[cfg(unix)]
fn pgid_of(pid: u32) -> Option<u32> {
    use nix::unistd::{getpgid, Pid};
    getpgid(Some(Pid::from_raw(pid as i32)))
        .ok()
        .map(|p| p.as_raw() as u32)
}

#[cfg(not(unix))]
fn pgid_of(_pid: u32) -> Option<u32> {
    None // no process-group concept; ppid-walk is the only net on Windows.
}

/// Join a command line into a single-line snippet, truncated for display.
fn cmd_snippet(cmd: &[String]) -> String {
    const MAX: usize = 60;
    let joined = cmd.join(" ");
    let one_line = joined.replace(['\n', '\t'], " ");
    if one_line.chars().count() > MAX {
        let kept: String = one_line.chars().take(MAX.saturating_sub(1)).collect();
        format!("{kept}…")
    } else {
        one_line
    }
}

// ─── public entry point ─────────────────────────────────────────────────────

/// Run the reaper for `scope`.
///
/// **Phase 1**: only `dry_run = true` is supported — discover candidates and
/// print them, killing nothing. A non-dry-run call returns an error directing
/// the user to the (not-yet-built) interactive path so the command can never
/// silently kill before the picker + kill mechanics land in Phase 2.
pub fn run(scope: ReapScope, dry_run: bool) -> anyhow::Result<()> {
    let sessions = sessions_for(&scope);
    if sessions.is_empty() {
        match &scope {
            ReapScope::One(sid) => {
                println!("csm reap: no pidfile for session {sid} — nothing to inspect");
            }
            ReapScope::All => {
                println!("csm reap: no csm-managed sessions found — nothing to inspect");
            }
        }
        return Ok(());
    }

    let table = snapshot_proc_table();
    let self_pid = std::process::id();
    let now = now_epoch();

    // Per-session candidates. `include_live_claude = false` in Phase 1: the
    // startup class-3 ("claude outlived a dead csm") path is a later phase, and
    // surfacing a still-supervised claude as a kill target here would be unsafe.
    let mut total: Vec<(String, Candidate)> = Vec::new();
    for session in &sessions {
        let cands = select_candidates(&table, session, self_pid, false);
        for c in cands {
            total.push((session.sid.clone(), c));
        }
    }

    if total.is_empty() {
        println!(
            "csm reap: {} session(s) inspected, no orphan candidates found",
            sessions.len()
        );
        return Ok(());
    }

    if dry_run {
        println!(
            "csm reap (dry-run): {} candidate(s) across {} session(s):",
            total.len(),
            sessions.len()
        );
        for (sid, c) in &total {
            // Re-use the picker TSV layout, but show col1/col2 too in dry-run so
            // the human sees the pid + kind plainly.
            let row = candidate_row(c, now);
            let display = row.splitn(3, '\t').nth(2).unwrap_or(&row);
            println!(
                "  [{}] {display}  (session {})",
                c.kind.tag(),
                short_sid(sid)
            );
        }
        return Ok(());
    }

    // Phase 2 will replace this with the multi-select picker + SIGKILL.
    anyhow::bail!(
        "csm reap: interactive kill is not implemented yet (Phase 2). \
         Re-run with --dry-run to list the {} candidate(s).",
        total.len()
    )
}

/// First 8 chars of a session id, for compact dry-run lines.
fn short_sid(sid: &str) -> &str {
    sid.get(..8).unwrap_or(sid)
}

/// `csm reap` flag parsing + dispatch into [`run`].
///
/// Flags: `--dry-run`, `--session <sid>`, `--all` (explicit form of the default
/// scope), `-h`/`--help`. Unknown flags are a hard error (house convention).
pub fn cmd(args: &[OsString]) -> anyhow::Result<()> {
    let mut session: Option<String> = None;
    let mut dry_run = false;
    let mut explicit_all = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.to_string_lossy().as_ref() {
            "--dry-run" => dry_run = true,
            "--all" => explicit_all = true,
            "--session" => {
                let v = it
                    .next()
                    .context("csm reap: --session requires a <sid> argument")?;
                session = Some(v.to_string_lossy().into_owned());
            }
            "-h" | "--help" => {
                print_help();
                return Ok(());
            }
            other => anyhow::bail!("csm reap: unknown flag '{other}' (see `csm reap --help`)"),
        }
    }
    if explicit_all && session.is_some() {
        anyhow::bail!("csm reap: --all and --session are mutually exclusive");
    }
    run(ReapScope::resolve(session), dry_run)
}

fn print_help() {
    println!(
        "csm reap — discover orphan processes left by a csm-managed claude session\n\
         \n\
         USAGE:\n\
         \x20   csm reap [--dry-run] [--all | --session <sid>]\n\
         \n\
         FLAGS:\n\
         \x20   --dry-run          List candidates and exit (Phase 1: the only mode that runs)\n\
         \x20   --all              Inspect every csm-managed session (default scope)\n\
         \x20   --session <sid>    Inspect one session\n\
         \x20   -h, --help         Show this help\n\
         \n\
         A candidate is a live process correlated to a session's claude by process\n\
         group (durable across re-parenting) or parent chain, started after the\n\
         session began. Interactive kill lands in a later phase."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmd_snippet_truncates_long_lines() {
        let long = vec!["node".to_string(), "x".repeat(100)];
        let s = cmd_snippet(&long);
        assert!(
            s.chars().count() <= 60,
            "snippet too long: {}",
            s.chars().count()
        );
        assert!(
            s.ends_with('…'),
            "truncated snippet should end with ellipsis: {s}"
        );
    }

    #[test]
    fn cmd_snippet_collapses_newlines() {
        let multi = vec!["a\nb".to_string(), "c\td".to_string()];
        let s = cmd_snippet(&multi);
        assert!(
            !s.contains('\n') && !s.contains('\t'),
            "snippet must be one line: {s:?}"
        );
    }

    #[test]
    fn scope_resolve_session_wins() {
        match ReapScope::resolve(Some("sid-1".to_string())) {
            ReapScope::One(s) => assert_eq!(s, "sid-1"),
            _ => panic!("explicit --session must resolve to One"),
        }
        match ReapScope::resolve(None) {
            ReapScope::All => {}
            _ => panic!("no --session must resolve to All"),
        }
    }

    #[test]
    fn short_sid_truncates_or_passes_through() {
        assert_eq!(short_sid("0123456789abcdef"), "01234567");
        assert_eq!(short_sid("abc"), "abc");
    }
}
