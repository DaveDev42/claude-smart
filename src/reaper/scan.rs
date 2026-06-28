//! Pure candidate-selection core for the orphan reaper.
//!
//! Everything here is **pure**: given a snapshot of the live process table
//! (`&[ProcRow]`) plus a session anchor (`Session`), it decides which processes
//! are reap candidates. No syscalls, no I/O, no clock — the I/O shell in
//! `reaper/mod.rs` captures the snapshot (sysinfo + `getpgid`) and the current
//! time, then calls in here. This split is the project's "pure core + thin I/O"
//! invariant (CLAUDE.md §4) and is what makes the selection logic unit-testable
//! against a synthetic `Vec<ProcRow>`.
//!
//! ## What counts as a candidate
//!
//! A `csm`-supervised `claude` is launched as its own process-group leader
//! (`setpgid(0,0)` in `platform/posix.rs`), so on POSIX `pgid == claude_pid`.
//! Its children (MCP servers, sandbox helpers, Bash-tool background procs)
//! inherit that pgid — and crucially the **pgid survives** the leader's death
//! and re-parenting to init, unlike the `ppid` link which is severed. So pgid is
//! the durable correlation signal; the `ppid` walk is a secondary net for
//! grandchildren that escaped the group via `setsid` while claude was still
//! alive. Every candidate is additionally gated by `start_time >= born` so a
//! recycled PID from before the session can never be selected (the PID-recycle
//! guard; `born` is the nonce stamped into `<sid>.pid`).

use crate::platform::proc_check::is_claude_or_node_name;

/// One row of the live process table, captured once by the I/O shell so the
/// decision functions stay pure.
///
/// `start_time` and the session's `born` are both Unix epoch **seconds** (the
/// resolution sysinfo's `Process::start_time()` and the `<sid>.pid` nonce share),
/// so the `>=` comparison is apples-to-apples.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcRow {
    pub pid: u32,
    /// Parent pid (`Process::parent()`); `None` for pid 1 / kernel threads / a
    /// process whose parent the table did not capture.
    pub ppid: Option<u32>,
    /// Process-group id (POSIX `getpgid`); `None` on Windows or on a `getpgid`
    /// error (`ESRCH`/`EPERM`). A `None` pgid is never a pgid match — fail-closed.
    pub pgid: Option<u32>,
    /// Unix epoch seconds the process started (`Process::start_time()`).
    pub start_time: u64,
    /// Lowercased exe basename with any `.exe` suffix stripped (matches the
    /// normalization `proc_check::is_claude_or_node_name` expects).
    pub exe_base: String,
    /// A short, already-truncated command-line snippet for display only.
    pub cmd_snippet: String,
}

/// The anchor a reap run is scoped to: one `csm`-managed claude session.
///
/// `claude_pid` + `born` come straight from `<sid>.pid` (`platform/pid.rs`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub sid: String,
    pub claude_pid: u32,
    pub born: u64,
}

/// Why a process was flagged — drives display annotation and (later) kill
/// semantics. Carried as a hidden picker field so the handler can tell a stray
/// child apart from "the live claude whose supervisor died".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateKind {
    /// A descendant of claude (pgid match and/or ppid-walk reachable).
    Child,
    /// `claude` itself, still alive after its `csm` supervisor went away.
    LiveClaude,
}

impl CandidateKind {
    /// Stable machine token used as a hidden picker column (col2).
    pub fn tag(self) -> &'static str {
        match self {
            CandidateKind::Child => "child",
            CandidateKind::LiveClaude => "live-claude",
        }
    }
}

/// A selected reap candidate: the process plus why it was flagged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub pid: u32,
    pub kind: CandidateKind,
    pub exe_base: String,
    pub start_time: u64,
    pub cmd_snippet: String,
}

impl Candidate {
    fn child(p: &ProcRow) -> Self {
        Candidate {
            pid: p.pid,
            kind: CandidateKind::Child,
            exe_base: p.exe_base.clone(),
            start_time: p.start_time,
            cmd_snippet: p.cmd_snippet.clone(),
        }
    }

    fn live_claude(p: &ProcRow) -> Self {
        Candidate {
            pid: p.pid,
            kind: CandidateKind::LiveClaude,
            exe_base: p.exe_base.clone(),
            start_time: p.start_time,
            cmd_snippet: p.cmd_snippet.clone(),
        }
    }
}

/// Pure BFS: the set of pids reachable from `root` by following `ppid` edges in
/// `table`. Used to catch grandchildren whose pgid escaped claude's group (via
/// `setsid`) *while claude is still alive* — once claude dies the parent link is
/// severed, so this net only helps in the live-tree case (pgid covers the rest).
///
/// `root` itself is **not** included. Robust against cycles (a malformed table
/// where a ppid forms a loop) via the `seen` set.
pub fn ppid_descendants(table: &[ProcRow], root: u32) -> std::collections::BTreeSet<u32> {
    use std::collections::BTreeSet;
    let mut reached: BTreeSet<u32> = BTreeSet::new();
    let mut frontier = vec![root];
    while let Some(parent) = frontier.pop() {
        for p in table {
            if p.ppid == Some(parent) && p.pid != root && reached.insert(p.pid) {
                frontier.push(p.pid);
            }
        }
    }
    reached
}

/// PURE: is this session's claude **still alive and supervised** in the snapshot?
///
/// True iff the recorded `claude_pid` is present in `table`, its `start_time`
/// matches the session's `born` exactly (so it is THE claude we recorded, not a
/// recycled pid), and its exe is `claude`/`node`. When true, the session is a
/// *live* one — its children are legitimate working processes, NOT orphans — so
/// the reaper must skip the whole session (the live-session guard). When false,
/// the recorded claude has died (the pidfile is stale) and its descendants are
/// real orphan candidates.
///
/// This is the reaper's analogue of the relaunch loop's clobber guard
/// (`platform/relaunch.rs`): both ask "is the recorded pid a live claude that
/// genuinely belongs to this session?" — here against a one-shot snapshot so the
/// answer is consistent with the candidate scan that uses the same table.
pub fn session_claude_is_live(table: &[ProcRow], session: &Session) -> bool {
    table.iter().any(|p| {
        p.pid == session.claude_pid
            && p.start_time == session.born
            && is_claude_or_node_name(&p.exe_base)
    })
}

/// PURE: select reap candidates for `session` from a process-table snapshot.
///
/// `self_pid` is the reaper's own pid (never a candidate). `include_live_claude`
/// is true only for the startup stale-sweep (class 3); in the post-exit trigger
/// claude is already dead, so it is false and the live-claude branch is skipped.
///
/// The candidate predicate (per process `P`):
/// ```text
/// P.pid != self_pid                       (never kill ourselves)
/// AND P.pid != claude_pid                 (the live claude is handled separately)
/// AND P.start_time >= born                (PID-recycle guard)
/// AND ( P.pgid == Some(claude_pid)        (durable pgrp membership)
///    OR P.pid ∈ ppid_descendants(claude_pid) )
/// ```
/// Results are deduped by pid and sorted ascending for a stable display order.
pub fn select_candidates(
    table: &[ProcRow],
    session: &Session,
    self_pid: u32,
    include_live_claude: bool,
) -> Vec<Candidate> {
    let c = session.claude_pid;
    let born = session.born;

    let descendants = ppid_descendants(table, c);

    let mut out: Vec<Candidate> = Vec::new();
    for p in table {
        if p.pid == self_pid || p.pid == c {
            continue;
        }
        if p.start_time < born {
            continue; // PID-recycle guard: older than the session → impostor.
        }
        let pgid_match = p.pgid == Some(c);
        if pgid_match || descendants.contains(&p.pid) {
            out.push(Candidate::child(p));
        }
    }

    if include_live_claude {
        if let Some(cp) = table.iter().find(|p| p.pid == c) {
            // Exact born match (not `>=`): this must be THE claude we recorded,
            // not a same-pid recycle. The exe gate rejects a same-second impostor
            // that happens to be non-claude.
            if cp.start_time == born && is_claude_or_node_name(&cp.exe_base) {
                out.push(Candidate::live_claude(cp));
            }
        }
    }

    out.sort_by_key(|cand| cand.pid);
    out.dedup_by_key(|cand| cand.pid);
    out
}

/// Format an age (now − start_time, in seconds) as `h:mm:ss` for display.
///
/// `now < start_time` (clock skew / a process that started "in the future")
/// clamps to `0:00:00` rather than underflowing.
pub fn format_age(now: u64, start_time: u64) -> String {
    let secs = now.saturating_sub(start_time);
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    format!("{h}:{m:02}:{s:02}")
}

/// Build the TSV row a candidate is rendered as in the picker / dry-run output.
///
/// Layout (delimiter `\t`):
/// ```text
/// <pid>\t<kind-tag>\t<pid_pad> <exe_base>  age=<h:mm:ss>  <cmd_snippet>
/// └col1┘ └─ col2 ─┘ └──────────────── displayed (from field 3) ───────────────┘
/// ```
/// col1 is the **hidden recovery key** (the pid the caller kills); col2 is the
/// hidden `CandidateKind` tag; field 3+ is what the picker shows. This mirrors
/// the picker's "field 1 is a hidden recovery key" contract (`picker/engine.rs`).
pub fn candidate_row(c: &Candidate, now: u64) -> String {
    let age = format_age(now, c.start_time);
    format!(
        "{pid}\t{tag}\t{pid:>7}  {exe}  age={age}  {cmd}",
        pid = c.pid,
        tag = c.kind.tag(),
        exe = c.exe_base,
        age = age,
        cmd = c.cmd_snippet,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic process-table row builder (keeps each test's noise down).
    fn row(pid: u32, ppid: Option<u32>, pgid: Option<u32>, start: u64, exe: &str) -> ProcRow {
        ProcRow {
            pid,
            ppid,
            pgid,
            start_time: start,
            exe_base: exe.to_string(),
            cmd_snippet: format!("{exe} --flag"),
        }
    }

    fn session(claude_pid: u32, born: u64) -> Session {
        Session {
            sid: "00000000-0000-0000-0000-000000000000".to_string(),
            claude_pid,
            born,
        }
    }

    // claude pid 100, born 1000. self (the reaper) is pid 9.
    const CLAUDE: u32 = 100;
    const BORN: u64 = 1000;
    const SELF: u32 = 9;

    #[test]
    fn pgid_match_selects_child() {
        // An MCP server in claude's process group, started after born.
        let table = vec![
            row(CLAUDE, Some(1), Some(CLAUDE), BORN, "node"),
            row(200, Some(CLAUDE), Some(CLAUDE), BORN + 5, "node"), // MCP server
        ];
        let got = select_candidates(&table, &session(CLAUDE, BORN), SELF, false);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].pid, 200);
        assert_eq!(got[0].kind, CandidateKind::Child);
    }

    #[test]
    fn different_pgid_not_selected() {
        // A wholly unrelated process: different pgid, not in claude's tree.
        let table = vec![
            row(CLAUDE, Some(1), Some(CLAUDE), BORN, "node"),
            row(300, Some(1), Some(300), BORN + 5, "firefox"),
        ];
        let got = select_candidates(&table, &session(CLAUDE, BORN), SELF, false);
        assert!(
            got.is_empty(),
            "unrelated proc must not be a candidate: {got:?}"
        );
    }

    #[test]
    fn born_filter_rejects_recycled_pid_even_with_matching_pgid() {
        // The impostor case: a process that LOOKS like it's in claude's group but
        // started BEFORE the session began — a recycled pid. Must be rejected
        // despite the pgid match (the recycle guard is unconditional).
        let table = vec![
            row(CLAUDE, Some(1), Some(CLAUDE), BORN, "node"),
            row(200, Some(CLAUDE), Some(CLAUDE), BORN - 1, "node"), // older than session
        ];
        let got = select_candidates(&table, &session(CLAUDE, BORN), SELF, false);
        assert!(got.is_empty(), "pre-born pid must be rejected: {got:?}");
    }

    #[test]
    fn never_selects_self_or_claude() {
        // Even if self and claude technically sit in claude's group, neither is a
        // child candidate (self = never-kill-self; claude = handled separately).
        let table = vec![
            row(CLAUDE, Some(1), Some(CLAUDE), BORN, "node"),
            row(SELF, Some(1), Some(CLAUDE), BORN, "csm"),
        ];
        let got = select_candidates(&table, &session(CLAUDE, BORN), SELF, false);
        assert!(
            got.iter().all(|c| c.pid != SELF && c.pid != CLAUDE),
            "self/claude must never be child candidates: {got:?}"
        );
    }

    #[test]
    fn ppid_walk_catches_setsid_escaped_grandchild() {
        // A grandchild that escaped the process group (its own pgid) but is still
        // reachable via parent edges while claude lives.
        let table = vec![
            row(CLAUDE, Some(1), Some(CLAUDE), BORN, "node"),
            row(200, Some(CLAUDE), Some(CLAUDE), BORN + 1, "bash"), // child, in group
            row(201, Some(200), Some(201), BORN + 2, "python3"),    // grandchild, escaped pgid
        ];
        let got = select_candidates(&table, &session(CLAUDE, BORN), SELF, false);
        let pids: Vec<u32> = got.iter().map(|c| c.pid).collect();
        assert!(pids.contains(&200), "in-group child missing: {pids:?}");
        assert!(
            pids.contains(&201),
            "setsid-escaped grandchild must be caught by ppid walk: {pids:?}"
        );
    }

    #[test]
    fn pgid_and_ppid_overlap_dedups_to_one() {
        // A process matched by BOTH signals appears exactly once.
        let table = vec![
            row(CLAUDE, Some(1), Some(CLAUDE), BORN, "node"),
            row(200, Some(CLAUDE), Some(CLAUDE), BORN + 1, "bash"), // pgid match AND ppid-reachable
        ];
        let got = select_candidates(&table, &session(CLAUDE, BORN), SELF, false);
        assert_eq!(got.len(), 1, "must dedup to a single candidate: {got:?}");
        assert_eq!(got[0].pid, 200);
    }

    #[test]
    fn none_pgid_only_reachable_via_ppid() {
        // pgid == None (Windows / getpgid error) is never a pgid match, but the
        // process can still be caught by the ppid walk. Fail-closed on pgid.
        let table = vec![
            row(CLAUDE, Some(1), Some(CLAUDE), BORN, "node"),
            row(200, Some(CLAUDE), None, BORN + 1, "helper"), // ppid-reachable only
            row(300, Some(1), None, BORN + 1, "unrelated"),   // neither signal
        ];
        let got = select_candidates(&table, &session(CLAUDE, BORN), SELF, false);
        let pids: Vec<u32> = got.iter().map(|c| c.pid).collect();
        assert_eq!(
            pids,
            vec![200],
            "only the ppid-reachable None-pgid proc: {pids:?}"
        );
    }

    #[test]
    fn live_claude_included_only_when_requested_and_matching() {
        let table = vec![row(CLAUDE, Some(1), Some(CLAUDE), BORN, "claude")];

        // include_live_claude = false → claude itself never appears.
        let off = select_candidates(&table, &session(CLAUDE, BORN), SELF, false);
        assert!(
            off.is_empty(),
            "claude must be absent when not requested: {off:?}"
        );

        // include_live_claude = true, exact born + claude exe → appears as LiveClaude.
        let on = select_candidates(&table, &session(CLAUDE, BORN), SELF, true);
        assert_eq!(on.len(), 1);
        assert_eq!(on[0].pid, CLAUDE);
        assert_eq!(on[0].kind, CandidateKind::LiveClaude);
    }

    #[test]
    fn live_claude_rejected_on_born_mismatch_or_wrong_exe() {
        // Same pid but a DIFFERENT start_time → a recycled pid, not our claude.
        let recycled = vec![row(CLAUDE, Some(1), Some(CLAUDE), BORN + 50, "claude")];
        let got = select_candidates(&recycled, &session(CLAUDE, BORN), SELF, true);
        assert!(
            got.is_empty(),
            "born mismatch must reject live-claude: {got:?}"
        );

        // Exact born but the exe is not claude/node → a same-second impostor.
        let impostor = vec![row(CLAUDE, Some(1), Some(CLAUDE), BORN, "python3")];
        let got = select_candidates(&impostor, &session(CLAUDE, BORN), SELF, true);
        assert!(
            got.is_empty(),
            "non-claude exe must reject live-claude: {got:?}"
        );
    }

    #[test]
    fn ppid_descendants_handles_cycles() {
        // A malformed table where ppid edges form a loop must not hang.
        let table = vec![
            row(CLAUDE, Some(1), Some(CLAUDE), BORN, "node"),
            row(200, Some(CLAUDE), Some(CLAUDE), BORN + 1, "a"),
            row(201, Some(200), Some(CLAUDE), BORN + 1, "b"),
            row(200, Some(201), Some(CLAUDE), BORN + 1, "a-dup"), // duplicate pid forming a cycle
        ];
        let reached = ppid_descendants(&table, CLAUDE);
        assert!(reached.contains(&200) && reached.contains(&201));
    }

    #[test]
    fn format_age_basic_and_clamps() {
        assert_eq!(format_age(1000 + 3661, 1000), "1:01:01");
        assert_eq!(format_age(1000, 1000), "0:00:00");
        // now < start_time → clamp, no underflow panic.
        assert_eq!(format_age(900, 1000), "0:00:00");
    }

    #[test]
    fn candidate_row_col1_is_recoverable_pid() {
        // The TSV round-trip the picker relies on: col1 must be the pid string.
        let c = Candidate {
            pid: 4242,
            kind: CandidateKind::Child,
            exe_base: "node".to_string(),
            start_time: 1000,
            cmd_snippet: "node mcp-server.js".to_string(),
        };
        let r = candidate_row(&c, 1000 + 65);
        let col1 = r.split('\t').next().unwrap();
        assert_eq!(col1, "4242", "col1 must be the recoverable pid");
        let col2 = r.split('\t').nth(1).unwrap();
        assert_eq!(col2, "child", "col2 must be the kind tag");
        // Display portion carries the human bits.
        assert!(r.contains("age=0:01:05"), "row: {r}");
        assert!(r.contains("node mcp-server.js"), "row: {r}");
    }

    #[test]
    fn session_live_when_claude_present_born_matches_and_exe_is_claude() {
        let table = vec![
            row(CLAUDE, Some(1), Some(CLAUDE), BORN, "claude"),
            row(200, Some(CLAUDE), Some(CLAUDE), BORN + 5, "node"),
        ];
        assert!(
            session_claude_is_live(&table, &session(CLAUDE, BORN)),
            "a present claude with matching born + claude exe must read as live"
        );
    }

    #[test]
    fn session_not_live_when_claude_pid_absent() {
        // The recorded claude died (pidfile is stale); only its orphaned child
        // remains. Not live → its children are real candidates.
        let table = vec![row(200, Some(1), Some(CLAUDE), BORN + 5, "node")];
        assert!(
            !session_claude_is_live(&table, &session(CLAUDE, BORN)),
            "absent claude pid must read as not-live"
        );
    }

    #[test]
    fn session_not_live_on_born_mismatch_or_wrong_exe() {
        // Same pid, different start_time → a recycled pid, not our claude.
        let recycled = vec![row(CLAUDE, Some(1), Some(CLAUDE), BORN + 99, "claude")];
        assert!(!session_claude_is_live(&recycled, &session(CLAUDE, BORN)));
        // Exact born but a non-claude exe → a same-second impostor on the pid.
        let impostor = vec![row(CLAUDE, Some(1), Some(CLAUDE), BORN, "firefox")];
        assert!(!session_claude_is_live(&impostor, &session(CLAUDE, BORN)));
    }
}
