//! Relaunch loop, the limit-switch sentinel and the follow file.
//!
//! The relaunch loop (`relaunch_loop`) is platform-agnostic and consumes a
//! `&dyn Launcher` + `ProcCheck`.  The public entry point `run_relaunch_loop`
//! gates it per platform: on unix it runs the full loop; on Windows it falls
//! back to a single launch-without-relaunch (`run_once`) because the Windows
//! console-stop path has two BLOCKING empirical checks that are not yet
//! verified (interactive Ctrl-C forwarding + CTRL_BREAK transcript flush — see
//! `platform/windows.rs` and CLAUDE.md's "Known gaps" section).  Shipping the
//! relaunch loop on Windows before those pass risks losing the supervisor
//! (Ctrl-C kills it) or truncating the session transcript on a limit switch.
//! On Windows a limit hit still switches the account; the user resumes by
//! hand (`run_once` prints the one line that says how).
//!
//! ## Who switches
//!
//! The hook never switches: it writes the sentinel
//! `<state>/sentinel/<sid>.json` and stops its child. The supervisor then
//! runs `crate::account::limit_switch::run_hop`, which takes `switch.lock`,
//! decides leader or follower, and (as leader) switches `D` through
//! `orca::switch`. Every hop runs in the same `D`; only the account in it
//! changes. A leader also drops `<state>/follow/<sid>.json` for each peer
//! session still on the capped account, which that peer's own Stop hook
//! turns into a relaunch at its next turn boundary.
//!
//! ## Recovery
//!
//! An unfinished switch left by a dead csm is repaired here only AFTER the
//! child has spawned (a thread started right after the spawn), never before
//! exec (design S8). A failed repair prints one line pointing at
//! `csm accounts doctor --fix` (to csm's log in an Orca pane).
//!
//! ## Sentinel read-compat
//!
//! `hop` and `born` stay JSON numbers (the legacy zsh writer used
//! `--argjson`); `target_profile` from an older binary reads as
//! `target_account`, and every field this version added defaults.  Compare
//! with `Sidecar`, whose `hop` is a JSON STRING (`sidecar/mod.rs`).

use std::ffi::OsString;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::account::limit_switch::HopOutcome;

// ─── sentinel ─────────────────────────────────────────────────────────────────

/// The sentinel schema version this binary writes.
pub const SENTINEL_V: u32 = 1;

/// `reason` of a same-account model fallback (no switch).
pub const REASON_MODEL_FALLBACK: &str = "model-fallback";
/// `reason` of a peer relaunch after another session's switch (no switch).
pub const REASON_FOLLOW: &str = "follow";

fn sentinel_v() -> u32 {
    SENTINEL_V
}

/// `<state>/sentinel/<sid>.json` — atomic JSON sentinel written by the hook,
/// consumed by the supervisor's post-wait path.
///
/// Design fields: `v`, `target_account`, `from_account`, `from_gen`,
/// `reason`, `at`. The relaunch itself also needs `session_id`, `cwd`,
/// `handoff`, `hop`, `born` and `model_override`, kept from the earlier
/// sentinel.
///
/// `born` is compared against the `born` epoch written into `<sid>.pid` at the
/// start of this loop iteration — the stale-sentinel rejection linchpin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RelaunchSentinel {
    #[serde(default = "sentinel_v")]
    pub v: u32,
    pub session_id: String,
    /// The account the hook picked (an Orca account id). For a model
    /// fallback or a follow, the account the session resumes on.
    #[serde(alias = "target_profile")]
    pub target_account: String,
    /// The session's account when the hook fired (`None` when unknown).
    #[serde(default)]
    pub from_account: Option<String>,
    /// The switch journal's `gen` when the hook fired: a larger `gen` at
    /// consume time means another csm already switched.
    #[serde(default)]
    pub from_gen: u64,
    /// `limit:<dimension>`, [`REASON_MODEL_FALLBACK`] or [`REASON_FOLLOW`].
    #[serde(default)]
    pub reason: String,
    /// Epoch seconds the hook wrote this.
    #[serde(default)]
    pub at: i64,
    pub cwd: String,
    pub handoff: String,
    /// JSON number.  The loop breaks when `hop > MAX_HOPS`.
    pub hop: i64,
    /// Unix epoch (seconds).  Consume only when `sentinel.born >= launch_born`.
    pub born: i64,
    /// `Some(model)` for a same-account model fallback (a `week_fable` cap —
    /// see `crate::hook::detect::fable_fallback_model`): the hop resumes the
    /// SAME session on this model instead of switching accounts.
    #[serde(default)]
    pub model_override: Option<String>,
}

impl RelaunchSentinel {
    /// Does consuming this sentinel ask for an account switch?
    pub fn wants_switch(&self) -> bool {
        self.model_override.is_none() && self.reason != REASON_FOLLOW
    }
}

/// Maximum number of limit-switch hops before the relaunch loop breaks.
/// Matches the legacy zsh `MAX_HOPS=1` constant. A follow does not count.
#[cfg_attr(windows, allow(dead_code))]
pub const MAX_HOPS: i64 = 1;

/// Read a sentinel from `path`.  Returns `None` if the file is absent;
/// propagates I/O or parse errors (the error never quotes the file).
pub fn read_relaunch(path: &Path) -> anyhow::Result<Option<RelaunchSentinel>> {
    read_json(path)
}

/// Write `sentinel` atomically to `path` (parent created 0700).
pub fn write_relaunch(path: &Path, sentinel: &RelaunchSentinel) -> anyhow::Result<()> {
    write_json(path, sentinel)
}

// ─── follow file ──────────────────────────────────────────────────────────────

/// `<state>/follow/<sid>.json`: another session's leader switched `D` away
/// from this session's account. The peer relaunches on its next Stop hook
/// when `at` is not older than its own launch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FollowFile {
    #[serde(default = "sentinel_v")]
    pub v: u32,
    /// The switch journal's `gen` after the leader's switch.
    #[serde(default, rename = "gen")]
    pub generation: u64,
    pub to_account: String,
    /// Epoch seconds the leader wrote this.
    #[serde(default)]
    pub at: i64,
}

pub fn read_follow(path: &Path) -> anyhow::Result<Option<FollowFile>> {
    read_json(path)
}

pub fn write_follow(path: &Path, follow: &FollowFile) -> anyhow::Result<()> {
    write_json(path, follow)
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> anyhow::Result<Option<T>> {
    match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s)
            .map(Some)
            .map_err(|e| anyhow::anyhow!("{}: unparseable ({:?})", path.display(), e.classify())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        crate::orca::fsx::create_dir_all(parent, 0o700)?;
    }
    let json = serde_json::to_vec(value)?;
    crate::orca::fsx::write_atomic(path, &json, crate::orca::fsx::WriteOpts::PRIVATE)?;
    Ok(())
}

// ─── loop ─────────────────────────────────────────────────────────────────────

/// Entry point for the foreground relaunch loop (unix).
///
/// The loop runs until:
/// - no sentinel appears after claude exits, OR
/// - `sentinel.born < launch_born` (stale sentinel — ignore), OR
/// - `sentinel.hop > MAX_HOPS` (a follow is exempt), OR
/// - a recovery of an unfinished switch failed (one line, no relaunch), OR
/// - a sentinel atomic-consume race is detected.
#[cfg(not(windows))]
pub fn run_relaunch_loop(
    launcher: &dyn crate::platform::launcher::Launcher,
    spec: &LaunchSpec,
) -> anyhow::Result<()> {
    relaunch_loop(launcher, spec)
}

/// Entry point on Windows — the console-stop relaunch path is gated OFF until its
/// two BLOCKING empirical checks pass (see module doc + `platform/windows.rs`).
/// Falls back to a single launch: a limit still switches the account after
/// the child exits, then one line tells the user how to resume.
#[cfg(windows)]
pub fn run_relaunch_loop(
    launcher: &dyn crate::platform::launcher::Launcher,
    spec: &LaunchSpec,
) -> anyhow::Result<()> {
    run_once(launcher, spec)
}

/// Single launch with no relaunch — the Windows fall-back while the
/// console-stop relaunch loop is gated off. A sentinel the hook wrote still
/// runs the switch (leader or follower), then says how to resume by hand.
#[cfg(windows)]
fn run_once(
    launcher: &dyn crate::platform::launcher::Launcher,
    spec: &LaunchSpec,
) -> anyhow::Result<()> {
    use crate::paths;

    let sid = spec.session_id.clone();
    let pid_path = paths::pid_file(&sid);
    let sentinel_path = paths::sentinel(&sid);
    let _ = std::fs::remove_file(&sentinel_path);

    let mut recovery = None;
    let mut on_spawn = || recovery = start_recovery(&spec.pin);
    let (status, handle) = launcher.run_foreground(&sid, &spec.cli, &spec.env, &mut on_spawn)?;
    let mut repair = finish_recovery(spec, &sid, recovery);
    let _ = std::fs::remove_file(&pid_path);

    if let Ok(Some(sentinel)) = read_relaunch(&sentinel_path) {
        let _ = std::fs::remove_file(&sentinel_path);
        if repair == Repair::Busy && sentinel.born >= handle.born {
            repair = retry_recovery(spec, &sid);
        }
        if !repair.blocks_relaunch() && sentinel.born >= handle.born && sentinel.wants_switch() {
            let outcome =
                crate::account::limit_switch::run_hop(&sentinel, &sid, &spec.pin, &mut |line| {
                    say(spec, &sid, line, false)
                });
            let account = hop_account(
                &outcome,
                || d_account_now(&spec.pin),
                sentinel.from_account.clone(),
            );
            // Nothing is resumed here: the line says where the account is
            // now and the next one says how to resume.
            let line = hop_line(&sentinel, &outcome, account.as_deref(), false);
            say(spec, &sid, &line, true);
            let short = crate::hook::sid_short(&sid);
            say(
                spec,
                &sid,
                &format!("csm: resume with `csm --resume {short}`"),
                true,
            );
        } else if sentinel.born >= handle.born && !sentinel.wants_switch() {
            // A model fallback (or a follow) stopped claude too: never
            // leave the session stopped without a word.
            let short = crate::hook::sid_short(&sid);
            say(spec, &sid, &windows_resume_hint(&sentinel, short), true);
        }
    }
    exit_with(status)
}

/// The line a Windows launch prints for a consumed sentinel that switched
/// no account (a model fallback or a follow): how to resume by hand, with
/// the fallback model when there is one. Pure.
#[cfg(any(windows, test))]
fn windows_resume_hint(sentinel: &RelaunchSentinel, short: &str) -> String {
    match sentinel.model_override.as_deref() {
        Some(m) => format!(
            "csm: this model's weekly limit is reached; resume on {m} with `csm --resume {short} --model {m}`"
        ),
        None => format!("csm: resume with `csm --resume {short}`"),
    }
}

/// The full platform-agnostic relaunch loop (used on unix; gated off on Windows,
/// where `run_relaunch_loop` dispatches to `run_once` instead, so this is not
/// compiled there).
#[cfg(not(windows))]
fn relaunch_loop(
    launcher: &dyn crate::platform::launcher::Launcher,
    spec: &LaunchSpec,
) -> anyhow::Result<()> {
    relaunch_loop_with(launcher, spec, &start_recovery, &repair_before_respawn)
}

/// Starts the repair of an unfinished switch right after a spawn
/// ([`start_recovery`] outside tests).
#[cfg(not(windows))]
type RecoveryStart<'a> =
    dyn Fn(&crate::launch_context::ConfigDirPin) -> Option<std::thread::JoinHandle<Repair>> + 'a;

/// Repairs an unfinished switch before a relaunch hop spawns; `None` when
/// nothing is pending ([`repair_before_respawn`] outside tests).
#[cfg(not(windows))]
type RecoveryNow<'a> = dyn Fn(&crate::launch_context::ConfigDirPin) -> Option<Repair> + 'a;

/// [`relaunch_loop`] with the recovery starter and the pre-relaunch repair
/// injected.
#[cfg(not(windows))]
fn relaunch_loop_with(
    launcher: &dyn crate::platform::launcher::Launcher,
    spec: &LaunchSpec,
    start: &RecoveryStart<'_>,
    now: &RecoveryNow<'_>,
) -> anyhow::Result<()> {
    use crate::paths;
    use crate::platform::pid;

    let sid = spec.session_id.clone();
    let sentinel_path = paths::sentinel(&sid);
    let pid_path = paths::pid_file(&sid);

    // The CLI mutates across hops (resume + handoff prompt); start from the
    // cold-launch CLI the caller built. Every hop runs in the same `D`.
    let mut cli: Vec<OsString> = spec.cli.clone();

    loop {
        // Clobber guard: if another live csm already owns this sid's pidfile,
        // do not stomp it — abort this loop (the other supervisor is in charge).
        if let Ok(Some((other_pid, _born))) = pid::read_pid_file(&pid_path) {
            use crate::platform::proc_check::ProcCheck;
            if other_pid != 0
                && crate::platform::PlatformProcCheck::is_live_claude_or_node(other_pid)
            {
                anyhow::bail!(
                    "session {sid} is already managed by a live process (pid {other_pid})"
                );
            }
        }

        // A stale sentinel from a prior chain must never be consumed by this
        // launch. (Defensive: the born-check below is the real guard.)
        let _ = std::fs::remove_file(&sentinel_path);

        // Recovery of an unfinished switch runs on a thread started right
        // after the child spawns, never before the first exec (an Orca pane
        // must not wait on it). It runs on every spawn whose journal is
        // pending, not just the first; a relaunch hop also repairs before
        // its spawn (below), so this thread then finds nothing to do.
        let mut recovery = None;
        let mut on_spawn = || {
            recovery = start(&spec.pin);
        };
        let (status, handle) = launcher.run_foreground(&sid, &cli, &spec.env, &mut on_spawn)?;
        let mut repair = finish_recovery(spec, &sid, recovery);

        // Did the hook drop a sentinel for THIS incarnation?
        let sentinel = match read_relaunch(&sentinel_path) {
            Ok(Some(s)) => s,
            Ok(None) => {
                let _ = std::fs::remove_file(&pid_path);
                return exit_with(status);
            }
            Err(e) => {
                let _ = std::fs::remove_file(&sentinel_path);
                let _ = std::fs::remove_file(&pid_path);
                return Err(e);
            }
        };

        // Born-check: reject a sentinel written for a PRIOR launch.
        if sentinel.born < handle.born {
            let _ = std::fs::remove_file(&sentinel_path);
            let _ = std::fs::remove_file(&pid_path);
            return exit_with(status);
        }

        // Atomic consume: a crash mid-relaunch cannot replay it.
        if std::fs::remove_file(&sentinel_path).is_err() {
            let _ = std::fs::remove_file(&pid_path);
            return exit_with(status);
        }

        // The recovery thread found switch.lock held (a switch or a login in
        // progress): nothing was repaired and nothing failed. The child is
        // stopped now, so try once more before the hop.
        if repair == Repair::Busy {
            repair = retry_recovery(spec, &sid);
        }
        if repair.blocks_relaunch() {
            let _ = std::fs::remove_file(&pid_path);
            return exit_with(status);
        }

        // Hop cap: bound the automatic switches per chain. A follow spends
        // no hop, so it is never capped here.
        if sentinel.reason != REASON_FOLLOW && sentinel.hop > MAX_HOPS {
            say(
                spec,
                &sid,
                &format!("csm: limit-switch hop cap ({MAX_HOPS}) reached — not relaunching again"),
                true,
            );
            let _ = std::fs::remove_file(&pid_path);
            return exit_with(status);
        }

        // Lead or follow the switch (a model fallback and a follow switch
        // nothing).
        let outcome = if sentinel.wants_switch() {
            let outcome =
                crate::account::limit_switch::run_hop(&sentinel, &sid, &spec.pin, &mut |line| {
                    say(spec, &sid, line, false)
                });
            if outcome == HopOutcome::LockBusy {
                // Another csm still holds switch.lock and may be switching
                // `D`: starting claude there now would race it.
                say(
                    spec,
                    &sid,
                    &hop_line(&sentinel, &outcome, None, false),
                    true,
                );
                let short = crate::hook::sid_short(&sid);
                say(
                    spec,
                    &sid,
                    &format!("csm: resume with `csm --resume {short}`"),
                    true,
                );
                let _ = std::fs::remove_file(&pid_path);
                return exit_with(status);
            }
            Some(outcome)
        } else {
            None
        };

        // A hop whose switch failed with `D` half written (a failed restore,
        // a failed verify) leaves the journal pending. Repair it now, while
        // no child of this chain runs, instead of relaunching into that `D`
        // and leaving the repair to a thread racing the new child, which
        // could load (and refresh) a mismatched grant/identity pair first.
        // It runs before the account is read and the resume line printed:
        // the repair may move `D`, and a failed one keeps the session down,
        // which is then the one line an Orca pane shows.
        if let Some(r) = now(&spec.pin) {
            if let Repair::Failed(why) = &r {
                let short = crate::hook::sid_short(&sid);
                say(spec, &sid, &not_resumed_line(why, short), true);
                let _ = std::fs::remove_file(&pid_path);
                return exit_with(status);
            }
            report_repair(spec, &sid, r);
        }

        // Say in one line where the session resumes.
        let account = match &outcome {
            Some(outcome) => {
                let account = hop_account(
                    outcome,
                    || d_account_now(&spec.pin),
                    sentinel.from_account.clone(),
                );
                say(
                    spec,
                    &sid,
                    &hop_line(&sentinel, outcome, account.as_deref(), true),
                    true,
                );
                account
            }
            None => {
                let account = no_switch_account(&sentinel, || d_account_now(&spec.pin));
                let line = match &sentinel.model_override {
                    Some(model) => format!(
                        "csm: model-scoped cap on {}; resumed on model {model}",
                        label(&sentinel.target_account)
                    ),
                    None => format!(
                        "csm: another session switched the account; resumed on {}",
                        label(account.as_deref().unwrap_or_default())
                    ),
                };
                say(spec, &sid, &line, true);
                account
            }
        };

        // Record this incarnation's account and launch time: usage captures
        // and the follow check key on them. `D`'s identity is noted first
        // (a follow or a Stay may find `D` moved by another supervisor, and
        // no tick has seen it yet), so this incarnation's captures count.
        let born =
            crate::usage::local::launch_born(&crate::account::AccountSet::load_pinned(&spec.pin));
        let _ = crate::sidecar::merge_sidecar(
            &paths::sidecar(&sid),
            &crate::sidecar::Sidecar {
                account_id: account,
                born: Some(born),
                ..Default::default()
            },
        );

        // Build the next iteration's CLI: same sid, resume the session, re-apply
        // the launch flags the sidecar remembers, and inject the handoff prompt
        // (unless suppressed).
        let remembered = crate::sidecar::read_sidecar(&paths::sidecar(&sid)).unwrap_or_default();
        let (next_cli, dropped) = build_next_cli(&sid, &sentinel, &remembered);
        if !dropped.is_empty() {
            let _ = crate::hook::notify::append_log(
                &sid,
                &format!(
                    "relaunch sid={} dropped passthru: {}",
                    crate::hook::sid_short(&sid),
                    crate::cli::carry::describe_dropped(&dropped)
                ),
            );
        }
        cli = next_cli;
    }
}

/// An account's label (email local part, else id prefix).
fn label(id: &str) -> String {
    if id.is_empty() {
        return "the current account".into();
    }
    crate::account::AccountSet::load().label(id)
}

/// The account `D` holds now (its `oauthAccount` mapped to one stash), `D`
/// being the child's (the launch's pin applied). Reads files only.
fn d_account_now(pin: &crate::launch_context::ConfigDirPin) -> Option<String> {
    crate::account::AccountSet::load_pinned(pin).current
}

/// The account the next incarnation runs on. After a switch or a follow it
/// is the target. After a Stay it is whatever `D` holds now, read after the
/// hop: another supervisor may have switched `D` while this one waited for
/// `switch.lock` and gave up, and the sidecar must not keep naming the
/// capped account then. The sentinel's `from` is the fallback when `D`'s
/// identity maps to no single account. Pure apart from `d_now`.
fn hop_account(
    outcome: &HopOutcome,
    d_now: impl FnOnce() -> Option<String>,
    from: Option<String>,
) -> Option<String> {
    match outcome {
        HopOutcome::Followed { to } | HopOutcome::Switched { to, .. } => Some(to.clone()),
        HopOutcome::Stay { .. } | HopOutcome::LockBusy => d_now().or(from),
    }
}

/// The account a relaunch that switched nothing runs on. A model fallback
/// stays on the sentinel's own account. A follow runs on whatever `D` holds
/// now: its file names the target of the switch it was written for, and a
/// later switch (another leader, or the user in Orca) may have moved `D`
/// again since, so recording the follow's target would file every later
/// usage capture under the wrong account. The follow's target is the
/// fallback when `D`'s identity maps to no single account. Pure apart from
/// `d_now`. Unix only: Windows does not relaunch a follow.
#[cfg(any(not(windows), test))]
fn no_switch_account(
    sentinel: &RelaunchSentinel,
    d_now: impl FnOnce() -> Option<String>,
) -> Option<String> {
    let target = Some(sentinel.target_account.clone()).filter(|a| !a.is_empty());
    if sentinel.model_override.is_some() {
        return target;
    }
    d_now().or(target)
}

/// The one relaunch line for a hop outcome; `now_on` is [`hop_account`]'s
/// answer. `resumed` is false on Windows, where the session is not relaunched
/// and the caller prints how to resume it by hand. Pure over its inputs
/// apart from the label lookup.
fn hop_line(
    sentinel: &RelaunchSentinel,
    outcome: &HopOutcome,
    now_on: Option<&str>,
    resumed: bool,
) -> String {
    let from = sentinel
        .from_account
        .as_deref()
        .map(label)
        .unwrap_or_else(|| "the current account".into());
    let now_on = now_on.map(label).unwrap_or_else(|| from.clone());
    hop_line_with(&from, &now_on, outcome, resumed, label)
}

/// [`hop_line`] with the labels injected. Pure.
fn hop_line_with(
    from: &str,
    now_on: &str,
    outcome: &HopOutcome,
    resumed: bool,
    label: impl Fn(&str) -> String,
) -> String {
    let on = if resumed { "resumed on" } else { "now on" };
    match outcome {
        HopOutcome::Switched { to, .. } | HopOutcome::Followed { to } => {
            format!("csm: account {from} capped; {on} {}", label(to))
        }
        HopOutcome::Stay { reason } => {
            format!("csm: account {from} capped; no switch ({reason}); {on} {now_on}")
        }
        HopOutcome::LockBusy => format!(
            "csm: account {from} capped; another csm still holds switch.lock, so the session was not relaunched"
        ),
    }
}

/// Print `line` on stderr unless the launch is quiet (an Orca pane), where
/// only `always` lines (the one relaunch line, fatal errors) print. Every
/// line also goes to csm's log.
fn say(spec: &LaunchSpec, sid: &str, line: &str, always: bool) {
    if shown_on_stderr(spec.quiet, always) {
        eprintln!("{line}");
    }
    let _ = crate::hook::notify::append_log(sid, line);
}

/// Whether a [`say`] line reaches stderr. Pure.
fn shown_on_stderr(quiet: bool, always: bool) -> bool {
    always || !quiet
}

/// How the repair of an unfinished switch went, as the relaunch loop sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Repair {
    /// Nothing pending, or repaired.
    Done,
    /// Another csm held `switch.lock` for the whole wait: nothing was
    /// attempted and `D` was not touched. Not a failure.
    Busy,
    /// The repair ran and failed; `D` was left neutral. The text says why.
    Failed(String),
    /// Orca runs and the repair waits for it to stop (see
    /// [`crate::orca::switch::Recovery::Deferred`]): nothing was written.
    /// Not a failure, and not worth a retry while Orca runs.
    Deferred(String),
}

impl Repair {
    /// Pure: map [`crate::orca::switch::recover`]'s answer.
    fn from_recovery(r: Result<crate::orca::switch::Recovery, crate::orca::OrcaError>) -> Repair {
        use crate::orca::switch::Recovery;
        match r {
            Ok(Recovery::Busy) => Repair::Busy,
            Ok(Recovery::Failed(why)) => Repair::Failed(why),
            Ok(Recovery::Deferred(why)) => Repair::Deferred(why),
            // Orca started during the repair and now owns `D`, as with a
            // repair handed to it on purpose: not a reason to keep the
            // session down, but worth the log line.
            Ok(Recovery::Uncertain(why)) => Repair::Deferred(format!(
                "the repair was handed to Orca and did not verify: {why}"
            )),
            Err(e) => Repair::Failed(e.to_string()),
            Ok(_) => Repair::Done,
        }
    }

    /// Design §3 Recovery: no session is relaunched only when a repair ran
    /// and failed. A busy lock holder is not that. Pure.
    fn blocks_relaunch(&self) -> bool {
        matches!(self, Repair::Failed(_))
    }
}

/// Whether the journal says a switch is unfinished.
fn journal_pending() -> bool {
    let state = crate::paths::smart_dir_no_create();
    crate::orca::switch::read_journal(&state).is_some_and(|j| j.pending())
}

/// Start the repair of an unfinished switch on a thread, when the journal
/// says one is pending, and after it hand the automatic migration's
/// post-spawn run, when the launch armed one, to a thread of its own
/// ([`crate::migrate::start_post_spawn`], once per process). The handle
/// returned covers the repair only: when claude exits the supervisor joins
/// the repair, never the migration run, which stops after its current
/// stage and gets a short grace at the process exit
/// ([`crate::migrate::finish_on_exit`]). Called right after the child
/// spawns.
fn start_recovery(
    pin: &crate::launch_context::ConfigDirPin,
) -> Option<std::thread::JoinHandle<Repair>> {
    let recover = journal_pending();
    let migrate = crate::migrate::post_spawn_armed();
    if !recover && !migrate {
        return None;
    }
    let pin = pin.clone();
    std::thread::Builder::new()
        .name("csm-recover".into())
        .spawn(move || {
            let r = if recover {
                recover_now(&pin, true)
            } else {
                Repair::Done
            };
            if migrate {
                crate::migrate::start_post_spawn();
            }
            r
        })
        .ok()
}

/// Before a relaunch hop spawns: repair an unfinished switch in the
/// foreground, `None` when the journal has nothing pending. No child of
/// this chain runs, so the live-claude scan decides the materialize order.
#[cfg(not(windows))]
fn repair_before_respawn(pin: &crate::launch_context::ConfigDirPin) -> Option<Repair> {
    journal_pending().then(|| recover_now(pin, false))
}

/// Run [`crate::orca::switch::recover`].
/// With `child_live` it runs while this supervisor's claude child starts in
/// `D`, usually before that child has registered in `D/sessions`, so the
/// switch env counts a live claude regardless of the scan: the repair then
/// uses Orca's materialize order (one `.claude.json` rewrite beside a live
/// writer) and skips the refresh the child may be racing.
fn recover_now(pin: &crate::launch_context::ConfigDirPin, child_live: bool) -> Repair {
    use crate::orca::switch::recover;
    let procs = crate::orca::live::SystemProcs;
    let ctx = match crate::orca::context::Context::current_pinned(&procs, pin) {
        Ok(c) => c,
        Err(e) => return Repair::Failed(e.to_string()),
    };
    let http = crate::orca::http::SystemHttp::from_env();
    Repair::from_recovery(ctx.with_switch_env_child(&procs, &http, child_live, recover))
}

/// Join the recovery thread and [`report_repair`] its answer.
fn finish_recovery(
    spec: &LaunchSpec,
    sid: &str,
    recovery: Option<std::thread::JoinHandle<Repair>>,
) -> Repair {
    // claude has exited: a migration run bound to this launch starts no
    // further stage, so a hop does not queue behind it.
    crate::migrate::child_exited();
    let r = recovery.and_then(|h| h.join().ok()).unwrap_or(Repair::Done);
    report_repair(spec, sid, r)
}

/// A second try after the recovery thread found `switch.lock` busy, once
/// the child has exited: skipped when the holder already settled the
/// journal. Still busy is not a failure either; the hop then waits for the
/// lock itself.
fn retry_recovery(spec: &LaunchSpec, sid: &str) -> Repair {
    if !journal_pending() {
        return Repair::Done;
    }
    report_repair(spec, sid, recover_now(&spec.pin, true))
}

/// Say once how a repair went: a failure gets the doctor line, a busy lock
/// a log line.
fn report_repair(spec: &LaunchSpec, sid: &str, r: Repair) -> Repair {
    match &r {
        Repair::Failed(why) => say(
            spec,
            sid,
            &recovery_failed_line(why),
            RECOVERY_FAILED_ALWAYS,
        ),
        Repair::Busy => {
            let _ = crate::hook::notify::append_log(
                sid,
                "csm: an unfinished account switch was not repaired: another csm holds switch.lock",
            );
        }
        Repair::Deferred(why) => {
            let _ = crate::hook::notify::append_log(
                sid,
                &format!("csm: an unfinished account switch was not repaired: {why}"),
            );
        }
        Repair::Done => {}
    }
    r
}

/// A failed repair is not fatal (claude already ran and its status is
/// returned), so in an Orca pane its line goes to csm's log only (design
/// section 3, Recovery).
const RECOVERY_FAILED_ALWAYS: bool = false;

/// The one line of a relaunch that a failed pre-relaunch repair kept
/// down. Pure.
#[cfg(not(windows))]
fn not_resumed_line(why: &str, short: &str) -> String {
    format!(
        "csm: an unfinished account switch could not be repaired ({why}), so the session was \
         not resumed; run `csm accounts doctor --fix`, then `csm --resume {short}`"
    )
}

/// Pure.
fn recovery_failed_line(why: &str) -> String {
    format!(
        "csm: an unfinished account switch could not be repaired ({why}); run `csm accounts doctor --fix`"
    )
}

/// Build the claude CLI for the next relaunch hop: resume the same session,
/// re-apply the `--permission-mode`/`--effort`/`--model` the sidecar remembers
/// for it, replay the session-shaping flags of the original launch, and pass
/// the handoff prompt (if any). Returns the argv and the remembered passthru
/// tokens it could not replay, which the caller logs.
///
/// The remembered flags matter because a switch is meant to continue the
/// same work: a session launched as `csm --model <m>` that moves to another
/// profile must come back up on `<m>`, not on that profile's default model.
/// The same holds for the flags `csm run` never consumed and forwarded to
/// claude — a session launched `--dangerously-skip-permissions --add-dir /x`
/// that comes back without them stops on the first permission prompt with
/// nobody there to answer it. `csm run` persists both into the sidecar at
/// launch precisely so this hop can read them back. What is *not* replayed is
/// the initial prompt (`--resume` already carries that conversation) and every
/// flag that would fight this argv or the resume itself; `cli::carry` owns
/// that decision.
///
/// Only the unix relaunch loop calls this; on Windows the loop is gated off
/// (`run_relaunch_loop` → `run_once`, which relaunches nothing and so has no
/// second argv to build), but the builder still compiles there so both
/// platforms would carry identically the day that gate lifts.
#[cfg_attr(windows, allow(dead_code))]
fn build_next_cli(
    sid: &str,
    sentinel: &RelaunchSentinel,
    remembered: &crate::sidecar::Sidecar,
) -> (Vec<OsString>, Vec<String>) {
    // A same-account model fallback (`sentinel.model_override`, see
    // `crate::hook::detect::fable_fallback_model`) overrides the remembered
    // model for this hop only — clone-and-replace, never a sidecar rewrite,
    // so a LATER hop that carries no override still replays whatever model
    // the session actually launched with. `sidecar_flags()` (below) is the
    // FIRST thing appended after the session id, ahead of all carried
    // passthru, so `--model` always lands in the same argv position whether
    // it came from the sidecar or from this override.
    let remembered = match &sentinel.model_override {
        Some(m) => {
            let mut c = remembered.clone();
            c.model = Some(m.clone());
            c
        }
        None => remembered.clone(),
    };

    let mut cli: Vec<OsString> = Vec::new();
    cli.push(OsString::from("--resume"));
    cli.push(OsString::from(sid));
    cli.extend(remembered.sidecar_flags());

    let passthru = remembered.passthru.as_deref().unwrap_or(&[]);
    let carried = crate::cli::carry::carry_passthru(passthru);
    let open_variadic = carried.trailing_variadic;
    cli.extend(carried.carried);

    // The handoff prompt is the first turn after resume (e.g. "resume"). Empty =
    // suppressed (user already had a pending tail); pass nothing then.
    if !sentinel.handoff.is_empty() {
        // A carried run that ends inside a variadic flag would swallow the
        // prompt as one more of its values, and the session would come back
        // with nothing to do. `--` closes the run; it is emitted only in that
        // case, so an argv that was already correct keeps its exact shape.
        if open_variadic {
            cli.push(OsString::from("--"));
        }
        cli.push(OsString::from(&sentinel.handoff));
    }
    (cli, carried.dropped)
}

/// Map a child `ExitStatus` to the loop's `Result`, preserving the exit code by
/// setting our own process exit code to match (so `csm run` is transparent).
fn exit_with(status: std::process::ExitStatus) -> anyhow::Result<()> {
    // The launch ends here, maybe through `process::exit`, which skips the
    // guard in `cmd::run`: give a migration run its short grace first.
    crate::migrate::finish_on_exit();
    if let Some(code) = status.code() {
        if code != 0 {
            std::process::exit(code);
        }
        return Ok(());
    }
    // No exit code → killed by a signal (unix). Mirror the shell convention
    // 128 + signo. On Windows, ExitStatus always has a code, so this is unix-only.
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            std::process::exit(128 + sig);
        }
    }
    Ok(())
}

/// Parameters for a single `csm run` invocation, threaded through the relaunch loop.
pub struct LaunchSpec {
    /// The session id (`--session-id`). A fresh UUID on cold launch; the same
    /// sid across all hops in one relaunch chain.
    pub session_id: String,
    /// The child's `CLAUDE_CONFIG_DIR` pin (also inside `env`). The
    /// supervisor applies it to every read of `D` it makes itself (the limit
    /// switch, the recovery, the sidecar's account), since its own process
    /// environment keeps the inherited value.
    pub pin: crate::launch_context::ConfigDirPin,
    /// The child's env changes: the `CLAUDE_CONFIG_DIR` pin (only when the
    /// inherited value differs from `D`) and, when the active account is
    /// Orca-managed, the credential variables to strip.
    pub env: crate::platform::launcher::ChildEnv,
    /// An Orca pane or structured launch: only fatal errors and the one
    /// relaunch line reach stderr; everything else goes to csm's log.
    pub quiet: bool,
    /// The cold-launch working directory. Carried for diagnostics; every hop
    /// runs inside the same supervisor process, so claude inherits it.
    #[allow(dead_code)]
    pub cwd: std::path::PathBuf,
    /// Full CLI to pass to claude (everything after `csm run [csm-flags]`).
    pub cli: Vec<OsString>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sentinel(handoff: &str) -> RelaunchSentinel {
        RelaunchSentinel {
            session_id: "abc123".to_string(),
            v: SENTINEL_V,
            target_account: "home".to_string(),
            from_account: None,
            from_gen: 0,
            reason: String::new(),
            at: 0,
            cwd: "/home/you/projects".to_string(),
            handoff: handoff.to_string(),
            hop: 1,
            born: 1,
            model_override: None,
        }
    }

    #[test]
    fn a_windows_stop_that_switched_nothing_still_says_how_to_resume() {
        let mut s = sentinel("");
        s.reason = REASON_MODEL_FALLBACK.into();
        s.model_override = Some("opus".into());
        assert!(!s.wants_switch());
        let line = windows_resume_hint(&s, "abc1");
        assert!(line.contains("csm --resume abc1 --model opus"), "{line}");
        s.reason = REASON_FOLLOW.into();
        s.model_override = None;
        assert!(!s.wants_switch());
        assert_eq!(
            windows_resume_hint(&s, "abc1"),
            "csm: resume with `csm --resume abc1`"
        );
        // Peers follow by relaunching, which only the unix loop does.
        assert_eq!(crate::hook::detect::follows_supported(), !cfg!(windows));
    }

    fn strs(v: &[OsString]) -> Vec<String> {
        v.iter().map(|s| s.to_string_lossy().into_owned()).collect()
    }

    /// The hop re-applies the sidecar's remembered launch flags between the
    /// resume verb and the handoff prompt, so the switched session runs the
    /// same model/effort/permission mode the user launched with.
    #[test]
    fn build_next_cli_carries_remembered_flags() {
        let remembered = crate::sidecar::Sidecar {
            model: Some("some-model".to_string()),
            effort: Some("high".to_string()),
            permission_mode: Some("plan".to_string()),
            ..Default::default()
        };
        let (cli, dropped) = build_next_cli("abc123", &sentinel("carry on"), &remembered);
        assert_eq!(
            strs(&cli),
            [
                "--resume",
                "abc123",
                "--permission-mode",
                "plan",
                "--effort",
                "high",
                "--model",
                "some-model",
                "carry on",
            ]
        );
        assert!(dropped.is_empty());
    }

    #[test]
    fn build_next_cli_without_flags_is_resume_and_handoff() {
        let (cli, dropped) = build_next_cli("abc123", &sentinel("carry on"), &Default::default());
        assert_eq!(strs(&cli), ["--resume", "abc123", "carry on"]);
        assert!(dropped.is_empty());
    }

    #[test]
    fn build_next_cli_empty_handoff_passes_no_prompt() {
        let remembered = crate::sidecar::Sidecar {
            model: Some("some-model".to_string()),
            ..Default::default()
        };
        let (cli, _) = build_next_cli("abc123", &sentinel(""), &remembered);
        assert_eq!(strs(&cli), ["--resume", "abc123", "--model", "some-model"]);
    }

    /// The whole point of remembering the passthru: the switched session comes
    /// back with the permission bypass and the extra directory it was launched
    /// with, ordered after the sidecar flags and before the handoff prompt.
    #[test]
    fn build_next_cli_carries_remembered_passthru_before_the_handoff() {
        let remembered = crate::sidecar::Sidecar {
            model: Some("some-model".to_string()),
            passthru: Some(vec![
                "--dangerously-skip-permissions".to_string(),
                "--add-dir".to_string(),
                "/Users/example/x".to_string(),
                "--settings".to_string(),
                "/Users/example/s.json".to_string(),
                "do the thing".to_string(),
            ]),
            ..Default::default()
        };
        let (cli, dropped) = build_next_cli("abc123", &sentinel("carry on"), &remembered);
        assert_eq!(
            strs(&cli),
            [
                "--resume",
                "abc123",
                "--model",
                "some-model",
                "--dangerously-skip-permissions",
                "--add-dir",
                "/Users/example/x",
                "--settings",
                "/Users/example/s.json",
                "carry on",
            ]
        );
        assert_eq!(
            dropped,
            ["do the thing"],
            "the initial prompt is not replayed; the caller logs that it went"
        );
    }

    #[test]
    fn build_next_cli_with_a_prompt_only_passthru_carries_nothing() {
        let remembered = crate::sidecar::Sidecar {
            passthru: Some(vec!["do the thing".to_string()]),
            ..Default::default()
        };
        let (cli, dropped) = build_next_cli("abc123", &sentinel("carry on"), &remembered);
        assert_eq!(strs(&cli), ["--resume", "abc123", "carry on"]);
        assert_eq!(dropped, ["do the thing"]);
    }

    /// The regression this guards: a launch whose last carried flag is
    /// variadic (`--add-dir /x`) used to hand claude a prompt it would read as
    /// one more directory, so the switched session came back with the extra
    /// directory wrong AND no first turn. `--` closes the run.
    #[test]
    fn build_next_cli_closes_a_trailing_variadic_before_the_handoff() {
        let remembered = crate::sidecar::Sidecar {
            passthru: Some(vec![
                "--add-dir".to_string(),
                "/Users/example/x".to_string(),
            ]),
            ..Default::default()
        };
        let (cli, _) = build_next_cli("abc123", &sentinel("carry on"), &remembered);
        assert_eq!(
            strs(&cli),
            [
                "--resume",
                "abc123",
                "--add-dir",
                "/Users/example/x",
                "--",
                "carry on",
            ]
        );
    }

    /// The separator is not free: it changes how claude reads everything after
    /// it, so it appears only when a variadic run is actually open. A launch
    /// whose carried tail is a boolean (or a one-value flag with its value)
    /// keeps the argv it always had.
    #[test]
    fn build_next_cli_omits_the_separator_when_nothing_can_absorb() {
        let remembered = crate::sidecar::Sidecar {
            passthru: Some(vec![
                "--add-dir".to_string(),
                "/Users/example/x".to_string(),
                "--dangerously-skip-permissions".to_string(),
            ]),
            ..Default::default()
        };
        let (cli, _) = build_next_cli("abc123", &sentinel("carry on"), &remembered);
        assert_eq!(
            strs(&cli),
            [
                "--resume",
                "abc123",
                "--add-dir",
                "/Users/example/x",
                "--dangerously-skip-permissions",
                "carry on",
            ],
            "the boolean closed the variadic run already"
        );
    }

    #[test]
    fn build_next_cli_with_an_open_variadic_and_no_handoff_adds_no_separator() {
        let remembered = crate::sidecar::Sidecar {
            passthru: Some(vec![
                "--add-dir".to_string(),
                "/Users/example/x".to_string(),
            ]),
            ..Default::default()
        };
        let (cli, _) = build_next_cli("abc123", &sentinel(""), &remembered);
        assert_eq!(
            strs(&cli),
            ["--resume", "abc123", "--add-dir", "/Users/example/x"],
            "nothing follows the run, so there is nothing to separate"
        );
    }

    /// A same-account model fallback (`sentinel.model_override`) overrides
    /// the remembered model for this hop — the resumed session gets exactly
    /// one `--model` flag, the fallback model, not the sidecar's original one.
    #[test]
    fn sentinel_model_override_emits_exactly_one_model_flag() {
        let remembered = crate::sidecar::Sidecar {
            model: Some("some-model".to_string()),
            effort: Some("high".to_string()),
            ..Default::default()
        };
        let mut s = sentinel("carry on");
        s.model_override = Some("opus".to_string());
        let (cli, dropped) = build_next_cli("abc123", &s, &remembered);
        assert_eq!(
            strs(&cli),
            [
                "--resume", "abc123", "--effort", "high", "--model", "opus", "carry on",
            ]
        );
        assert_eq!(
            cli.iter().filter(|a| *a == "--model").count(),
            1,
            "exactly one --model flag, never both the sidecar's and the override's"
        );
        assert!(dropped.is_empty());
    }

    /// No `model_override` (the ordinary account-switch sentinel, `None`) —
    /// the hop replays whatever model the sidecar remembers, exactly as
    /// before this field existed.
    #[test]
    fn sentinel_without_model_override_replays_sidecar_model() {
        let remembered = crate::sidecar::Sidecar {
            model: Some("some-model".to_string()),
            ..Default::default()
        };
        let (cli, _) = build_next_cli("abc123", &sentinel("carry on"), &remembered);
        assert_eq!(
            strs(&cli),
            ["--resume", "abc123", "--model", "some-model", "carry on"]
        );
    }

    /// Rollback safety: a `.relaunch` written by an OLDER binary (no
    /// `model_override` key at all) must still parse, with the field reading
    /// back as `None` — the same `#[serde(default)]` contract as every other
    /// forward-compat field on this type. Mirrors
    /// `read_compat_unknown_future_field_is_ignored_not_fatal`, but for a
    /// field THIS version knows and an OLDER file lacks, not the reverse.
    #[test]
    fn read_compat_old_file_without_model_override_defaults_to_none() {
        let json = r#"{
            "session_id":"sid-9","target_profile":"work","cwd":"/tmp",
            "handoff":"resume","hop":1,"born":1700000000
        }"#;
        let s: RelaunchSentinel =
            serde_json::from_str(json).expect("a sentinel without model_override must still parse");
        assert_eq!(s.model_override, None);
    }

    #[test]
    fn roundtrip_sentinel() {
        let sentinel = RelaunchSentinel {
            session_id: "abc123".to_string(),
            v: SENTINEL_V,
            target_account: "home".to_string(),
            from_account: None,
            from_gen: 0,
            reason: String::new(),
            at: 0,
            cwd: "/home/you/projects".to_string(),
            handoff: "resume".to_string(),
            hop: 1,
            born: 1_718_000_000,
            model_override: None,
        };
        let json = serde_json::to_string(&sentinel).unwrap();
        let back: RelaunchSentinel = serde_json::from_str(&json).unwrap();
        assert_eq!(back.session_id, sentinel.session_id);
        assert_eq!(back.target_account, sentinel.target_account);
        assert_eq!(back.hop, sentinel.hop);
        assert_eq!(back.born, sentinel.born);
    }

    /// The earlier sentinel spelled the target `target_profile`; the alias
    /// keeps such a file readable across an upgrade.
    #[test]
    fn field_names_match_legacy_zsh() {
        // The legacy zsh write_relaunch used these exact JSON key names.
        // Verify the serde rename is absent (snake_case == wire name).
        let json = r#"{
            "session_id": "sid-1",
            "target_profile": "work",
            "cwd": "/tmp",
            "handoff": "resume",
            "hop": 0,
            "born": 1700000000
        }"#;
        let s: RelaunchSentinel = serde_json::from_str(json).unwrap();
        assert_eq!(s.session_id, "sid-1");
        assert_eq!(s.target_account, "work");
        assert_eq!(s.hop, 0_i64);
        assert_eq!(s.born, 1_700_000_000_i64);
    }

    #[test]
    fn read_compat_unknown_future_field_is_ignored_not_fatal() {
        // Rollback safety: a NEWER binary may write an extra field this version
        // doesn't know. Reading it must succeed (the sentinel is consume-and-
        // delete, so dropping the unknown field loses nothing) and the known
        // fields must still be correct — never a parse abort.
        let json = r#"{
            "session_id":"sid-9","target_profile":"work","cwd":"/tmp",
            "handoff":"resume","hop":1,"born":1700000000,
            "futureField":"ignored","another":{"x":1}
        }"#;
        let s: RelaunchSentinel = serde_json::from_str(json).expect("unknown field must not abort");
        assert_eq!(s.session_id, "sid-9");
        assert_eq!(s.target_account, "work");
        assert_eq!(s.hop, 1_i64);
    }

    #[test]
    fn read_compat_corrupt_relaunch_errs_without_panic() {
        // A truncated/garbled .relaunch must surface as Err (the loop aborts the
        // relaunch and cleans up) — never a panic. Locks the failure mode.
        let tmp_dir = tempfile::tempdir().unwrap();
        let path = tmp_dir.path().join("corrupt.relaunch");
        std::fs::write(&path, "not json at all{{{").unwrap();
        let result = read_relaunch(&path);
        assert!(
            result.is_err(),
            "corrupt .relaunch must be Err, got {result:?}"
        );
    }

    #[test]
    fn hop_is_number_not_string() {
        // Contrast with sidecar where hop is a JSON STRING.
        // Here hop must deserialize from a JSON number.
        let json =
            r#"{"session_id":"x","target_profile":"p","cwd":"/","handoff":"","hop":1,"born":0}"#;
        let s: RelaunchSentinel = serde_json::from_str(json).unwrap();
        assert_eq!(s.hop, 1_i64);
    }

    #[test]
    fn read_absent_returns_none() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let path = tmp_dir.path().join("nonexistent.relaunch");
        let result = read_relaunch(&path).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn write_then_read_roundtrip() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let path = tmp_dir.path().join("test.relaunch");
        let sentinel = RelaunchSentinel {
            session_id: "test-sid".to_string(),
            v: SENTINEL_V,
            target_account: "home".to_string(),
            from_account: None,
            from_gen: 0,
            reason: String::new(),
            at: 0,
            cwd: "/tmp".to_string(),
            handoff: "resume".to_string(),
            hop: 0,
            born: 1_718_100_000,
            model_override: None,
        };
        write_relaunch(&path, &sentinel).unwrap();
        let back = read_relaunch(&path)
            .unwrap()
            .expect("should exist after write");
        assert_eq!(back.session_id, sentinel.session_id);
        assert_eq!(back.born, sentinel.born);
        assert_eq!(back.hop, sentinel.hop);
    }

    #[test]
    fn stale_sentinel_born_check() {
        // The consumer MUST reject sentinels where born < launch_born.
        let launch_born: i64 = 1_718_200_000;
        let stale_born: i64 = 1_718_100_000; // older than launch
        assert!(
            stale_born < launch_born,
            "stale sentinel should have born < launch_born"
        );
        // A fresh sentinel should have born >= launch_born.
        let fresh_born: i64 = 1_718_200_001;
        assert!(fresh_born >= launch_born);
    }

    #[test]
    fn hop_cap_constant() {
        assert_eq!(MAX_HOPS, 1, "MAX_HOPS must match legacy zsh MAX_HOPS=1");
    }

    /// Cross-format contract: `Sidecar.hop` is written as a JSON STRING (source
    /// site: `merge_sidecar_hop` in `hook/stop.rs`, mirroring the legacy zsh
    /// `jq --arg` sidecar writer) while `RelaunchSentinel.hop` is written as a
    /// JSON NUMBER (source site: this struct's `pub hop: i64` field, matching
    /// the legacy zsh `write_relaunch` `jq --argjson` writer). Owner decision:
    /// the `.relaunch` number-vs-string read tolerance stays — this test pins
    /// what is *written*, not what is accepted on read.
    #[test]
    fn sidecar_hop_is_string_relaunch_hop_is_number() {
        let sidecar = crate::sidecar::Sidecar {
            hop: Some(serde_json::Value::String("2".to_string())),
            ..Default::default()
        };
        let sidecar_json = serde_json::to_value(&sidecar).unwrap();
        assert!(
            sidecar_json["hop"].is_string(),
            "sidecar hop must serialize as a JSON string, got {sidecar_json}"
        );

        let sentinel = RelaunchSentinel {
            session_id: "abc123".to_string(),
            v: SENTINEL_V,
            target_account: "home".to_string(),
            from_account: None,
            from_gen: 0,
            reason: String::new(),
            at: 0,
            cwd: "/home/you/projects".to_string(),
            handoff: "resume".to_string(),
            hop: 2,
            born: 1,
            model_override: None,
        };
        let sentinel_json = serde_json::to_value(&sentinel).unwrap();
        assert!(
            sentinel_json["hop"].is_number(),
            "relaunch sentinel hop must serialize as a JSON number, got {sentinel_json}"
        );
    }

    #[test]
    fn hop_line_names_the_outcome() {
        let label = |id: &str| format!("L-{id}");
        let switched = HopOutcome::Switched {
            from: Some("a1".into()),
            to: "b2".into(),
            generation: 3,
        };
        assert_eq!(
            hop_line_with("alice", "x", &switched, true, label),
            "csm: account alice capped; resumed on L-b2"
        );
        let followed = HopOutcome::Followed { to: "c3".into() };
        assert_eq!(
            hop_line_with("alice", "x", &followed, true, label),
            "csm: account alice capped; resumed on L-c3"
        );
        let stay = HopOutcome::Stay {
            reason: "no other account has headroom".into(),
        };
        assert_eq!(
            hop_line_with("alice", "alice", &stay, true, label),
            "csm: account alice capped; no switch (no other account has headroom); resumed on alice"
        );
        assert_eq!(
            hop_line_with("alice", "bob", &stay, true, label),
            "csm: account alice capped; no switch (no other account has headroom); resumed on bob"
        );
        // Windows (no relaunch): never claims the session was resumed.
        assert_eq!(
            hop_line_with("alice", "x", &switched, false, label),
            "csm: account alice capped; now on L-b2"
        );
        assert_eq!(
            hop_line_with("alice", "bob", &stay, false, label),
            "csm: account alice capped; no switch (no other account has headroom); now on bob"
        );
    }

    #[test]
    fn a_stay_records_the_account_d_holds_now() {
        let stay = HopOutcome::Stay {
            reason: "cannot take switch.lock".into(),
        };
        // A peer supervisor moved D to b while this one waited for the lock.
        assert_eq!(
            hop_account(&stay, || Some("b".into()), Some("a".into())),
            Some("b".into())
        );
        // D's identity maps to no single account: keep the sentinel's.
        assert_eq!(
            hop_account(&stay, || None, Some("a".into())),
            Some("a".into())
        );
        // A switch or a follow never reads D.
        let switched = HopOutcome::Switched {
            from: Some("a".into()),
            to: "c".into(),
            generation: 1,
        };
        assert_eq!(
            hop_account(&switched, || panic!("D read"), Some("a".into())),
            Some("c".into())
        );
    }

    #[test]
    fn a_follow_records_the_account_d_holds_now() {
        // A follow written for an earlier switch (to "home"); a later switch
        // moved D on to c before this session reached its turn boundary.
        let mut s = sentinel("");
        s.reason = REASON_FOLLOW.into();
        assert_eq!(no_switch_account(&s, || Some("c".into())), Some("c".into()));
        // D's identity maps to no single account: the follow's target.
        assert_eq!(no_switch_account(&s, || None), Some("home".into()));
        // A model fallback stays on its own account and never reads D.
        s.model_override = Some("sonnet".into());
        assert_eq!(
            no_switch_account(&s, || panic!("D read")),
            Some("home".into())
        );
    }

    #[test]
    fn the_pin_moves_the_context_to_the_childs_d() {
        use crate::launch_context::ConfigDirPin;
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let orca_d = tmp.path().join("orca-d");
        let mut env = crate::orca::HostEnv::for_test(&home, crate::orca::HostOs::Linux);
        // A login dotfile exported another dir into the pane.
        env.claude_config_dir = Some(tmp.path().join("dotfile-d").to_string_lossy().into_owned());
        let procs = crate::orca::live::SystemProcs;

        let mut set = env.clone();
        ConfigDirPin::Set(orca_d.clone()).apply_to(&mut set);
        let ctx = crate::orca::context::Context::from_env(set, &procs);
        assert_eq!(ctx.paths.config_dir, orca_d);
        assert_eq!(ctx.paths.config_path, orca_d.join(".claude.json"));

        // Orca main runs without the variable: claude's default layout,
        // with the identity in ~/.claude.json.
        let mut unset = env.clone();
        ConfigDirPin::Unset.apply_to(&mut unset);
        let ctx = crate::orca::context::Context::from_env(unset, &procs);
        assert_eq!(ctx.paths.config_dir, home.join(".claude"));
        assert_eq!(ctx.paths.config_path, home.join(".claude.json"));

        let mut leave = env.clone();
        ConfigDirPin::Leave.apply_to(&mut leave);
        assert_eq!(leave.claude_config_dir, env.claude_config_dir);
    }

    /// `d_account_now` reads the pinned `D`, not the inherited one: the
    /// inherited `~/.claude.json` names alice, the child's `D` names bob,
    /// and only the pin decides which account the next incarnation is on.
    #[test]
    fn d_account_now_reads_the_pinned_d() {
        use crate::launch_context::ConfigDirPin;
        use crate::orca::testsupport::{make_stash, record_json, write_store};
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        crate::testenv::with_test_home(&home, || {
            let env = crate::orca::HostEnv::current().unwrap();
            let ud = crate::orca::userdata::resolve(&env, |_| false).dir;
            make_stash(&ud, "id-a", Some(br#"{"accountUuid":"u-a"}"#), None);
            make_stash(&ud, "id-b", Some(br#"{"accountUuid":"u-b"}"#), None);
            let recs = [
                record_json(&ud, "id-a", "alice@example.com", None),
                record_json(&ud, "id-b", "bob@example.com", None),
            ];
            write_store(&ud, &recs, Some("id-a"));
            // The inherited layout: no CLAUDE_CONFIG_DIR, identity in
            // ~/.claude.json.
            std::fs::create_dir_all(home.join(".claude")).unwrap();
            std::fs::write(
                home.join(".claude.json"),
                r#"{"oauthAccount":{"accountUuid":"u-a"}}"#,
            )
            .unwrap();
            let orca_d = tmp.path().join("orca-d");
            std::fs::create_dir_all(&orca_d).unwrap();
            std::fs::write(
                orca_d.join(".claude.json"),
                r#"{"oauthAccount":{"accountUuid":"u-b"}}"#,
            )
            .unwrap();

            assert_eq!(d_account_now(&ConfigDirPin::Leave).as_deref(), Some("id-a"));
            assert_eq!(
                d_account_now(&ConfigDirPin::Set(orca_d.clone())).as_deref(),
                Some("id-b")
            );
        });
    }

    /// A fake launcher: spawn `n` calls `on_spawn`, and while `n` is below
    /// `follows` its child "exits" leaving a follow sentinel for itself.
    #[cfg(unix)]
    struct FollowingLauncher {
        follows: usize,
        spawns: std::cell::Cell<usize>,
    }

    #[cfg(unix)]
    impl crate::platform::launcher::Launcher for FollowingLauncher {
        fn run_foreground(
            &self,
            sid: &str,
            _cli: &[OsString],
            _env: &crate::platform::launcher::ChildEnv,
            on_spawn: &mut dyn FnMut(),
        ) -> std::io::Result<(
            std::process::ExitStatus,
            crate::platform::launcher::ChildHandle,
        )> {
            use std::os::unix::process::ExitStatusExt;
            let n = self.spawns.get();
            self.spawns.set(n + 1);
            on_spawn();
            let born = 100 + n as i64;
            if n < self.follows {
                let mut s = sentinel("");
                s.session_id = sid.to_owned();
                s.reason = REASON_FOLLOW.into();
                s.target_account = "id-b".into();
                s.born = born;
                write_relaunch(&crate::paths::sentinel(sid), &s).unwrap();
            }
            Ok((
                std::process::ExitStatus::from_raw(0),
                crate::platform::launcher::ChildHandle { pid: 0, born },
            ))
        }
    }

    /// Every spawn gets its own recovery, not only the first: a hop whose
    /// switch failed with `D` half written leaves the journal pending and
    /// relaunches into that `D`, and the next child must not run in it
    /// unrepaired.
    #[cfg(unix)]
    #[test]
    fn recovery_starts_on_every_spawn_of_a_relaunch_chain() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let launcher = FollowingLauncher {
            follows: 2,
            spawns: std::cell::Cell::new(0),
        };
        let spec = LaunchSpec {
            session_id: "11111111-2222-3333-4444-555555555555".into(),
            pin: crate::launch_context::ConfigDirPin::Leave,
            env: Default::default(),
            quiet: true,
            cwd: tmp.path().to_path_buf(),
            cli: Vec::new(),
        };
        let starts = std::cell::Cell::new(0usize);
        let start = |_: &crate::launch_context::ConfigDirPin| {
            starts.set(starts.get() + 1);
            Some(std::thread::spawn(|| Repair::Done))
        };
        let nows = std::cell::Cell::new(0usize);
        let now = |_: &crate::launch_context::ConfigDirPin| {
            nows.set(nows.get() + 1);
            None
        };
        crate::testenv::with_test_home(&home, || {
            relaunch_loop_with(&launcher, &spec, &start, &now).unwrap();
        });
        assert_eq!(launcher.spawns.get(), 3, "two follows, then a plain exit");
        assert_eq!(starts.get(), 3, "one recovery start per spawn");
        assert_eq!(nows.get(), 2, "one foreground repair check per relaunch");
    }

    /// A relaunch hop repairs a pending switch before it spawns, with no
    /// child of the chain running; a repair that fails keeps the session
    /// down instead of starting claude in a half-written `D`.
    #[cfg(unix)]
    #[test]
    fn a_hop_repairs_before_the_relaunch_and_a_failed_repair_blocks_it() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let launcher = FollowingLauncher {
            follows: 2,
            spawns: std::cell::Cell::new(0),
        };
        let spec = LaunchSpec {
            session_id: "11111111-2222-3333-4444-666666666666".into(),
            pin: crate::launch_context::ConfigDirPin::Leave,
            env: Default::default(),
            quiet: true,
            cwd: tmp.path().to_path_buf(),
            cli: Vec::new(),
        };
        let start = |_: &crate::launch_context::ConfigDirPin| None;
        let spawns_at_repair = std::cell::Cell::new(None);
        let now = |_: &crate::launch_context::ConfigDirPin| {
            spawns_at_repair.set(Some(launcher.spawns.get()));
            Some(Repair::Failed("restore failed".into()))
        };
        let log = crate::testenv::with_test_home(&home, || {
            relaunch_loop_with(&launcher, &spec, &start, &now).unwrap();
            std::fs::read_to_string(crate::paths::smart_dir().unwrap().join("limit-switch.log"))
                .unwrap_or_default()
        });
        // The pane's one line says the session is down, and nothing
        // claimed a resume first.
        assert!(log.contains("was not resumed"), "{log}");
        assert!(log.contains("csm --resume 11111111"), "{log}");
        assert!(!log.contains("resumed on"), "{log}");
        assert_eq!(
            spawns_at_repair.get(),
            Some(1),
            "the repair ran after the first child and before any relaunch"
        );
        assert_eq!(
            launcher.spawns.get(),
            1,
            "no relaunch after a failed repair"
        );
    }

    /// A repair before the relaunch that moves `D` decides the account the
    /// next incarnation is recorded on: the sidecar names the account `D`
    /// holds after the repair, not the one it held when the child exited
    /// (usage captures are filed under the sidecar's account).
    #[cfg(unix)]
    #[test]
    fn the_recorded_account_is_read_after_the_repair() {
        use crate::orca::testsupport::{make_stash, record_json, write_store};
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let sid = "11111111-2222-3333-4444-777777777777";
        let launcher = FollowingLauncher {
            follows: 1,
            spawns: std::cell::Cell::new(0),
        };
        let spec = LaunchSpec {
            session_id: sid.into(),
            pin: crate::launch_context::ConfigDirPin::Leave,
            env: Default::default(),
            quiet: true,
            cwd: tmp.path().to_path_buf(),
            cli: Vec::new(),
        };
        let identity = |uuid: &str| format!(r#"{{"oauthAccount":{{"accountUuid":"{uuid}"}}}}"#);
        let start = |_: &crate::launch_context::ConfigDirPin| None;
        // The follow names id-b and `D` holds id-b when the child exits;
        // the repair then puts id-a back.
        let now = |_: &crate::launch_context::ConfigDirPin| {
            std::fs::write(home.join(".claude.json"), identity("u-a")).unwrap();
            Some(Repair::Done)
        };
        let recorded = crate::testenv::with_test_home(&home, || {
            let env = crate::orca::HostEnv::current().unwrap();
            let ud = crate::orca::userdata::resolve(&env, |_| false).dir;
            make_stash(&ud, "id-a", Some(br#"{"accountUuid":"u-a"}"#), None);
            make_stash(&ud, "id-b", Some(br#"{"accountUuid":"u-b"}"#), None);
            let recs = [
                record_json(&ud, "id-a", "alice@example.com", None),
                record_json(&ud, "id-b", "bob@example.com", None),
            ];
            write_store(&ud, &recs, Some("id-b"));
            std::fs::create_dir_all(home.join(".claude")).unwrap();
            std::fs::write(home.join(".claude.json"), identity("u-b")).unwrap();
            relaunch_loop_with(&launcher, &spec, &start, &now).unwrap();
            crate::sidecar::read_sidecar(&crate::paths::sidecar(sid)).unwrap_or_default()
        });
        assert_eq!(launcher.spawns.get(), 2, "one follow, then a plain exit");
        assert_eq!(
            recorded.account_id.as_deref(),
            Some("id-a"),
            "the account `D` holds after the repair"
        );
    }

    /// A recovery that only found `switch.lock` held is not a failed
    /// repair: it neither blocks the relaunch nor prints the doctor line.
    /// Only a repair that ran and failed (or errored) blocks it.
    #[test]
    fn a_busy_switch_lock_is_not_a_failed_repair() {
        use crate::orca::switch::Recovery;
        let busy = Repair::from_recovery(Ok(Recovery::Busy));
        assert_eq!(busy, Repair::Busy);
        assert!(!busy.blocks_relaunch());
        assert_eq!(Repair::from_recovery(Ok(Recovery::Nothing)), Repair::Done);
        assert!(!Repair::Done.blocks_relaunch());
        let deferred = Repair::from_recovery(Ok(Recovery::Deferred("Orca runs".into())));
        assert_eq!(deferred, Repair::Deferred("Orca runs".into()));
        assert!(!deferred.blocks_relaunch());
        let uncertain = Repair::from_recovery(Ok(Recovery::Uncertain("no answer".into())));
        assert!(
            matches!(&uncertain, Repair::Deferred(why) if why.contains("no answer")),
            "{uncertain:?}"
        );
        assert!(!uncertain.blocks_relaunch());
        let failed = Repair::from_recovery(Ok(Recovery::Failed("x".into())));
        assert_eq!(failed, Repair::Failed("x".into()));
        assert!(failed.blocks_relaunch());
        let err = Repair::from_recovery(Err(crate::orca::OrcaError::Refused("y".into())));
        assert!(err.blocks_relaunch());
    }

    #[test]
    fn a_lock_busy_hop_says_it_did_not_relaunch() {
        let line = hop_line_with("alice", "alice", &HopOutcome::LockBusy, false, |s| s.into());
        assert!(line.contains("alice capped"), "{line}");
        assert!(line.contains("not relaunched"), "{line}");
    }

    #[test]
    fn recovery_failed_line_points_at_doctor() {
        let line = recovery_failed_line("journal unreadable");
        assert!(line.contains("journal unreadable"));
        assert!(line.contains("csm accounts doctor --fix"));
    }

    #[test]
    fn a_failed_repair_stays_off_an_orca_panes_stderr() {
        // Quiet (an Orca pane): log only. Otherwise stderr too.
        assert!(!shown_on_stderr(true, RECOVERY_FAILED_ALWAYS));
        assert!(shown_on_stderr(false, RECOVERY_FAILED_ALWAYS));
        // The relaunch line and fatal errors still reach a pane.
        assert!(shown_on_stderr(true, true));
    }

    #[test]
    fn follow_and_fallback_sentinels_do_not_switch() {
        let mut s = sentinel("");
        assert!(s.wants_switch());
        s.reason = REASON_FOLLOW.into();
        assert!(!s.wants_switch());
        let mut s = sentinel("");
        s.model_override = Some("opus".into());
        assert!(!s.wants_switch());
    }
}
