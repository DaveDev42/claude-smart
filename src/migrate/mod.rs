//! Automatic migration off the legacy per-profile layout onto Orca's
//! account store.
//!
//! The legacy layout: `~/.config/claude-as/profiles.json` maps profile
//! names to config dirs, a machine-wide `CLAUDE_CONFIG_DIR` floor points
//! Orca and every shell at one of them, and `~/.claude.shared/` holds the
//! transcripts every profile links to. The target: Orca owns the accounts,
//! `D` is `~/.claude`, csm keeps no registry.
//!
//! There is no legacy mode and no manual procedure: a new csm migrates the
//! machine on its own. Which invocations may do what is
//! [`probe::trigger_class`]'s table; the cheap [`probe::probe`] decides
//! whether there is anything to do (on a migrated machine one small read
//! and one stat). The work runs in phases, each idempotent and resumable,
//! with every decision recomputed from disk:
//!
//! - adopt ([`adopt`]): every legacy login becomes an Orca account and
//!   Orca's active host account a managed one. Live-safe; it runs before
//!   an interactive launch spawns claude, within [`PRESPAWN_BUDGET`].
//! - carry ([`carry`]): the shared dirs, `~/.claude.json` and the floor
//!   profile's files move into `~/.claude` while the legacy dirs keep
//!   working. It runs after the spawn, on a thread of its own that the
//!   recovery thread starts ([`start_post_spawn`]; claude's exit waits for
//!   it only [`EXIT_GRACE`]), at a terminal FULL word and in `csm migrate`; an Orca
//!   pane runs its renames and config merge before the spawn only when
//!   Orca already runs in `~/.claude` ([`pane_prespawn`]).
//! - cutover, retire: `D` moves to `~/.claude`, the legacy dirs are renamed
//!   `<dir>.retired`. Their pure cores live in [`cutover`] and [`retire`].
//!
//! `<state>/migration.json` ([`state`]) caches progress; `migrate.lock` is a
//! try-lock (busy: another csm is migrating, skip). Credentials are never
//! deleted (the quarantine files them), legacy dirs are renamed, never
//! removed, and unregistered `~/.claude.*` dirs are only listed.

mod adopt;
mod carry;
mod cutover;
mod legacy;
mod plan;
pub(crate) mod probe;
mod retire;
pub(crate) mod state;
#[cfg(test)]
mod tests;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::orca::HostEnv;
use crate::orca::fsx;

use self::legacy::{Legacy, LegacyProfile, Probe};
use self::probe::{ProbeOutcome, TriggerClass};
use self::state::{MigrateLock, MigrationState, Phase, SnapProfile, Snapshot};

pub(crate) use self::legacy::load_legacy;

/// Read by `platform::proc`'s Linux thread test.
#[cfg(all(test, target_os = "linux"))]
pub(crate) use self::cutover::other_csm;
pub(crate) use self::retire::{settle_reason, settle_wanted};

/// How long an interactive launch waits for the probe and stage A before
/// it spawns claude anyway. A run still going then finishes on its thread
/// and is joined after the spawn.
pub(crate) const PRESPAWN_BUDGET: Duration = Duration::from_secs(3);

// ─── the report ───────────────────────────────────────────────────────────────

/// One line about one legacy dir in one stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReportRow {
    pub name: String,
    pub dir: PathBuf,
    pub stage: &'static str,
    pub line: String,
}

/// What a run found and did. Secrets never appear; grants only as
/// fingerprints.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Report {
    pub dry_run: bool,
    /// Anything legacy was found.
    pub legacy: bool,
    /// Another csm holds `migrate.lock`.
    pub busy: bool,
    pub phase: Option<Phase>,
    pub rows: Vec<ReportRow>,
    /// What this run changed, one line each.
    pub changed: Vec<String>,
    /// Why the migration is not finished, one line each.
    pub pending: Vec<String>,
    pub errors: Vec<String>,
    /// `~/.claude.*` dirs the registry never named: never touched.
    pub unregistered: Vec<PathBuf>,
    /// The cutover recorded its time (the floor is cleared or absent).
    pub cutover_recorded: bool,
    /// `--dry-run`: the full preview.
    pub plan: Option<String>,
}

/// The stdout report of `csm migrate`. Pure.
pub(crate) fn render(r: &Report) -> String {
    let mut o = String::new();
    if r.busy {
        o.push_str("another csm is migrating this machine; nothing done\n");
        return o;
    }
    if !r.legacy {
        o.push_str("nothing to migrate: no legacy profile layout on this machine\n");
        return o;
    }
    if let Some(p) = r.phase {
        o.push_str(&format!(
            "phase: {}{}\n",
            p.as_str(),
            if r.dry_run {
                " (dry run: nothing written)"
            } else {
                ""
            }
        ));
    }
    let mut last = "";
    for row in &r.rows {
        if row.name != last {
            o.push_str(&format!("{}  {}\n", row.name, row.dir.display()));
            last = &row.name;
        }
        o.push_str(&format!("  {:<10} {}\n", row.stage, row.line));
    }
    let list = |o: &mut String, title: &str, items: &[String]| {
        if !items.is_empty() {
            o.push_str(&format!("{title}:\n"));
            for i in items {
                o.push_str(&format!("  {i}\n"));
            }
        }
    };
    list(&mut o, "changed", &r.changed);
    list(&mut o, "pending", &r.pending);
    list(&mut o, "errors", &r.errors);
    if !r.unregistered.is_empty() {
        o.push_str("not registered, left alone:\n");
        for d in &r.unregistered {
            o.push_str(&format!("  {}\n", d.display()));
        }
    }
    if let Some(p) = &r.plan {
        o.push('\n');
        o.push_str(p);
    }
    o
}

// ─── triggers ─────────────────────────────────────────────────────────────────

/// Who asked for a run, which sets what it may print.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Trigger {
    /// `csm migrate [--dry-run]`: every stage, the backoff ignored, the
    /// report on stdout.
    Explicit { dry_run: bool },
    /// A FULL trigger in a terminal (not a launch): every stage, one
    /// stderr line when it changed something, at most one pending line per
    /// reason per day.
    Terminal,
    /// An interactive launch before its spawn: stage A only, printing as
    /// [`Trigger::Terminal`] does.
    PreSpawn,
    /// After a spawn: log only. `after_prespawn`: stage A already ran
    /// before the spawn.
    PostSpawn { after_prespawn: bool },
}

/// The claude a launch spawned, as the run after the spawn must see it.
/// The child starts in its `D` before it registers in `D/sessions`, so no
/// registry scan finds it yet: every stage that writes a dir, or moves a
/// tree, counts it as a live user of its `D` whatever the scan says.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) enum LaunchChild {
    /// No child: `csm migrate`, a terminal word.
    #[default]
    None,
    /// The child runs in `dir`, in session `sid` when known.
    In { dir: PathBuf, sid: Option<String> },
    /// A child runs, its `D` unknown: every dir counts as in use.
    Unknown,
}

impl LaunchChild {
    /// Does a child of this launch run at all?
    pub(crate) fn runs(&self) -> bool {
        !matches!(self, LaunchChild::None)
    }

    /// Does the child (maybe) run in `dir`?
    pub(crate) fn uses(&self, dir: &Path) -> bool {
        match self {
            LaunchChild::None => false,
            LaunchChild::Unknown => true,
            LaunchChild::In { dir: d, .. } => same_path(d, dir),
        }
    }

    /// The child's session id, when known.
    pub(crate) fn sid(&self) -> Option<&str> {
        match self {
            LaunchChild::In { sid, .. } => sid.as_deref(),
            _ => None,
        }
    }
}

/// The same dir: equal once trailing separators go, or once both resolve.
pub(crate) fn same_path(a: &Path, b: &Path) -> bool {
    let trim = |p: &Path| PathBuf::from(p.to_string_lossy().trim_end_matches(['/', '\\']));
    trim(a) == trim(b)
        || std::fs::canonicalize(a)
            .ok()
            .is_some_and(|x| std::fs::canonicalize(b).ok() == Some(x))
}

/// Unix seconds.
fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// The snapshot's `seen_at` when this process records the first one: its
/// own start time (else now). The first new csm to record it is then not
/// "started before the migration" for another csm's stage C, while every
/// csm started before it (the old binary, which cannot record one) is.
fn first_seen_at() -> i64 {
    crate::platform::proc::probe(std::process::id())
        .and_then(|p| i64::try_from(p.start_time).ok())
        .filter(|t| *t > 0)
        .unwrap_or_else(now)
}

// ─── the probe shell ──────────────────────────────────────────────────────────

/// The probe's facts and answer.
struct Probed {
    env: HostEnv,
    state: PathBuf,
    marker: Option<MigrationState>,
    outcome: ProbeOutcome,
}

/// Run the cheap probe: read the marker, stat what [`probe::probe`] asks
/// for, and write a `done` marker on a machine with nothing legacy
/// (unless `dry_run`). `None` when the environment cannot be resolved.
fn probe_now(dry_run: bool) -> Option<Probed> {
    crate::usage::reach::note("migrate-probe");
    let env = HostEnv::current().ok()?;
    let state = fsx::state_dir(&env);
    let marker = state::load(&state);
    let outcome = probe::probe(
        marker.as_ref(),
        &|p: &Path| p.exists(),
        env.claude_config_dir.as_deref(),
        &env.home,
    );
    if outcome == ProbeOutcome::Fresh && !dry_run {
        // Best effort: without it the next run probes again.
        if let Ok(Some(_lock)) = MigrateLock::try_acquire(&state)
            && state::load(&state).is_none()
        {
            let _ = state::save(&state, &MigrationState::done());
        }
    }
    Some(Probed {
        env,
        state,
        marker,
        outcome,
    })
}

// ─── a run ────────────────────────────────────────────────────────────────────

/// The registry as the stages see it: the recorded snapshot grown by
/// whatever the registry names now, the floor from the registry when it
/// names one. A first snapshot records `at()` as `seen_at`; a recorded
/// one keeps its own (or none, which counts every other csm as legacy).
/// Pure over `at`.
fn merge_snapshot(snap: Option<&Snapshot>, now: &Legacy, at: impl FnOnce() -> i64) -> Snapshot {
    let mut out = snap.cloned().unwrap_or_else(|| Snapshot {
        seen_at: Some(at()),
        ..Snapshot::default()
    });
    for p in &now.profiles {
        if !out.profiles.iter().any(|q| q.name == p.name) {
            out.profiles.push(SnapProfile {
                name: p.name.clone(),
                dir: p.dir.clone(),
            });
        }
    }
    out.profiles.sort_by(|a, b| a.name.cmp(&b.name));
    if now.floor.is_some() {
        out.floor = now.floor.clone();
    }
    out
}

/// Is there nothing legacy to migrate after all: no profile registered now
/// or recorded before, no `~/.claude.shared` and no cutover on record? The
/// probe also counts an inherited `CLAUDE_CONFIG_DIR` spelled
/// `~/.claude.<x>` (design section 1), which alone is only a pin: with
/// nothing else, no stage has a dir to adopt or retire, and the cutover
/// must not neutralise or switch `~/.claude` on a machine that never had
/// the legacy layout. Pure.
fn nothing_recorded(snap: &Snapshot, shared_exists: bool, cutover: bool) -> bool {
    snap.profiles.is_empty() && !shared_exists && !cutover
}

fn as_legacy(s: &Snapshot) -> Legacy {
    Legacy {
        profiles: s
            .profiles
            .iter()
            .map(|p| LegacyProfile {
                name: p.name.clone(),
                dir: p.dir.clone(),
            })
            .collect(),
        floor: s.floor.clone(),
    }
}

/// `~/.claude.*` dirs the registry does not name (and that are not the
/// shared dir or a retired one). Pure over the listing.
fn unregistered(names: &[String], home: &Path, known: &[PathBuf]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = names
        .iter()
        .filter(|n| n.starts_with(".claude.") && *n != ".claude.json")
        .filter(|n| !n.starts_with(".claude.shared") && !n.ends_with(".retired"))
        .map(|n| home.join(n))
        .filter(|p| !known.iter().any(|k| k == p))
        .collect();
    out.sort();
    out
}

fn list_unregistered(home: &Path, known: &[PathBuf]) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(home) else {
        return Vec::new();
    };
    let names: Vec<String> = rd
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| e.file_name().to_str().map(str::to_owned))
        .collect();
    unregistered(&names, home, known)
}

/// Run the migration once. Takes `migrate.lock` (not on a dry run).
pub(crate) fn run_with(trigger: Trigger) -> Report {
    run_with_child(trigger, &LaunchChild::None)
}

/// [`run_with`] beside a launch's claude `child` (the run after a spawn, or
/// the pre-spawn run whose child may start before it ends).
pub(crate) fn run_with_child(trigger: Trigger, child: &LaunchChild) -> Report {
    let dry_run = matches!(trigger, Trigger::Explicit { dry_run: true });
    let mut report = Report {
        dry_run,
        ..Report::default()
    };
    let Some(p) = probe_now(dry_run) else {
        report
            .errors
            .push("cannot resolve the home directory".to_owned());
        return report;
    };
    match p.outcome {
        ProbeOutcome::Done | ProbeOutcome::Fresh => {
            report.phase = Some(Phase::Done);
            report.cutover_recorded = p.marker.as_ref().is_some_and(|m| m.cutover.is_some());
            return report;
        }
        ProbeOutcome::Pending => report.legacy = true,
    }
    let _lock = if dry_run {
        None
    } else {
        match MigrateLock::try_acquire(&p.state) {
            Ok(Some(l)) => Some(l),
            Ok(None) => {
                report.busy = true;
                report
                    .pending
                    .push("another csm is migrating this machine".to_owned());
                return report;
            }
            Err(e) => {
                report.errors.push(format!("cannot take migrate.lock: {e}"));
                return report;
            }
        }
    };
    // Read again under the lock: another csm may have moved on.
    let mut st = state::load(&p.state).unwrap_or_default();
    let home = &p.env.home;
    let registry = match load_legacy(home) {
        Ok(l) => l,
        Err(e) => {
            report.errors.push(format!("{e:#}"));
            return report;
        }
    };
    let snap = merge_snapshot(st.legacy.as_ref(), &registry, first_seen_at);
    if nothing_recorded(
        &snap,
        std::fs::symlink_metadata(probe::shared_path(home)).is_ok(),
        st.cutover.is_some(),
    ) {
        // Only an inherited `CLAUDE_CONFIG_DIR` spelled `~/.claude.<x>`
        // made the probe look: no registry profile, no recorded one and no
        // `~/.claude.shared` back it, so there is nothing to adopt, and a
        // cutover would rewrite a `~/.claude` login this machine never
        // moved out of. Done; the dir it names is left alone.
        report.legacy = false;
        report.phase = Some(Phase::Done);
        if !dry_run {
            st.phase = Phase::Done;
            st.legacy = None;
            if let Err(e) = state::save(&p.state, &st) {
                report
                    .errors
                    .push(format!("cannot write {}: {e}", state::MARKER));
            }
        }
        return report;
    }
    let legacy = as_legacy(&snap);
    st.legacy = Some(snap.clone());
    if st.phase == Phase::Done {
        // The registry came back: adopt what it names.
        st.phase = Phase::Adopt;
    }
    report.unregistered = list_unregistered(home, &snap.dirs());

    let now = now();
    let explicit = matches!(trigger, Trigger::Explicit { .. });
    let skip_adopt = match trigger {
        Trigger::PostSpawn { after_prespawn } => after_prespawn,
        // A store-less host re-runs adopt until an Orca store appears.
        Trigger::Terminal | Trigger::PreSpawn => {
            st.phase > Phase::Adopt && !st.steps.contains_key(adopt::STORE_LESS_STEP)
        }
        Trigger::Explicit { .. } => false,
    };
    if !skip_adopt {
        let end = adopt::adopt(
            &legacy,
            adopt::AdoptOpts {
                dry_run,
                now,
                explicit,
                // Before a spawn the launch's child may start in csm's D
                // while this run still writes it (a run that outlives its
                // budget).
                child_in_d: trigger == Trigger::PreSpawn,
                child,
            },
            &mut st,
            &mut report,
        );
        if end.settled && st.phase == Phase::Adopt && !dry_run {
            st.phase = Phase::Carry;
        }
    }
    // Stage B: after the spawn, at a terminal FULL word and in `csm
    // migrate`, never before a launch's spawn. It keeps running after the
    // carry phase: B2 repeats until the floor dir is retired.
    // A launch-bound run whose claude has exited starts no further stage
    // (`child_exited`): the next launch resumes from the marker.
    let go_on = || may_continue(trigger, STOP.load(Ordering::SeqCst));
    let run_b = trigger != Trigger::PreSpawn
        && st.phase >= Phase::Carry
        && st.phase < Phase::Done
        && go_on();
    if run_b || dry_run {
        let end = run_carry(
            &p.env,
            &p.state,
            &legacy,
            dry_run,
            false,
            child,
            &mut st,
            &mut report,
        );
        if end.settled && st.phase == Phase::Carry && !dry_run {
            st.phase = Phase::Cutover;
        }
    }
    // The cutover and stage C: never before a launch's spawn.
    let run_c = trigger != Trigger::PreSpawn
        && st.phase >= Phase::Cutover
        && st.phase < Phase::Done
        && go_on();
    if run_c {
        if st.cutover.is_none() {
            let recorded =
                cutover::cutover(&p.env, &legacy, dry_run, now, child, &mut st, &mut report);
            if recorded && st.phase == Phase::Cutover && !dry_run {
                st.phase = Phase::Retire;
            }
        } else {
            cutover::recheck_floor(&p.env, &legacy, dry_run, &mut st, &mut report);
            if st.phase == Phase::Cutover && !dry_run {
                st.phase = Phase::Retire;
            }
        }
    }
    if run_c && st.phase == Phase::Retire && go_on() {
        let end = retire::retire_stage(
            &p.env,
            &p.state,
            &legacy,
            &report.unregistered.clone(),
            retire::RetireOpts {
                dry_run,
                now,
                explicit,
                child,
                launch_bound: matches!(trigger, Trigger::PreSpawn | Trigger::PostSpawn { .. }),
            },
            &mut st,
            &mut report,
        );
        if end.done && !dry_run {
            st.phase = Phase::Done;
        }
    }
    if dry_run {
        report.plan = dry_run_plan(&legacy);
    }
    report.phase = Some(st.phase);
    report.cutover_recorded = st.cutover.is_some();
    if matches!(trigger, Trigger::PostSpawn { .. }) && !report.changed.is_empty() {
        st.summary = Some(changed_line(&report));
    }
    if !dry_run && let Err(e) = state::save(&p.state, &st) {
        report
            .errors
            .push(format!("cannot write {}: {e}", state::MARKER));
        // Recorded means on disk: a cutover this run could not write down
        // is redone by the next run, so the floor setters stay until then.
        report.cutover_recorded = state::load(&p.state).is_some_and(|m| m.cutover.is_some());
    }
    report
}

/// The `--dry-run` preview over every legacy dir (presence probes only,
/// no secret read).
fn dry_run_plan(legacy: &Legacy) -> Option<String> {
    use crate::orca::context::Context;
    use crate::orca::live::SystemProcs;
    let ctx = Context::current(&SystemProcs).ok()?;
    let view = crate::orca::snapshot(&crate::orca::SnapshotOptions::default()).ok()?;
    Some(plan::render_plan(&plan::build_plan(
        &ctx,
        &view,
        legacy,
        Probe::Presence,
    )))
}

/// `csm migrate [--dry-run]`.
pub(crate) fn run(dry_run: bool) -> Report {
    run_with(Trigger::Explicit { dry_run })
}

// ─── terminal output ──────────────────────────────────────────────────────────

/// The one stderr line for what a run changed. Pure.
fn changed_line(r: &Report) -> String {
    format!("csm: migration: {}", r.changed.join("; "))
}

/// The note key for a pending or error line: its text with every run of
/// digits folded to `#`, so a line naming a pid, a count or a time keeps
/// one key across runs (one line per reason per day). Pure.
fn note_key(line: &str) -> String {
    let mut key = String::with_capacity(line.len() + 5);
    key.push_str("line:");
    let mut in_digits = false;
    for c in line.chars() {
        if c.is_ascii_digit() {
            if !in_digits {
                key.push('#');
            }
            in_digits = true;
        } else {
            key.push(c);
            in_digits = false;
        }
    }
    key
}

/// What a terminal run prints: the changed line, and the first pending or
/// error line whose note is due. Returns the lines and the note keys they
/// use. Pure.
fn terminal_lines(
    r: &Report,
    notes: &std::collections::BTreeMap<String, i64>,
    now: i64,
) -> (Vec<String>, Vec<String>) {
    let mut lines = Vec::new();
    let mut keys = Vec::new();
    if !r.changed.is_empty() {
        lines.push(changed_line(r));
    }
    let first_due = r
        .errors
        .iter()
        .chain(r.pending.iter())
        .find(|l| state::note_due(notes, &note_key(l), now));
    if let Some(l) = first_due {
        lines.push(format!(
            "csm: migration to Orca's accounts is not finished: {l} (`csm migrate` shows where it stands)"
        ));
        keys.push(note_key(l));
    }
    (lines, keys)
}

/// After lines were shown: mark their notes and clear a shown summary.
/// Skipped when another csm holds the lock (the note then shows again).
fn acknowledge(state_dir: &Path, keys: &[String], summary: Option<&str>) {
    if keys.is_empty() && summary.is_none() {
        return;
    }
    let Ok(Some(_lock)) = MigrateLock::try_acquire(state_dir) else {
        return;
    };
    let Some(mut st) = state::load(state_dir) else {
        return;
    };
    let t = now();
    state::prune_notes(&mut st.notes, t);
    for k in keys {
        st.notes.insert(k.clone(), t);
    }
    if summary.is_some() && st.summary.as_deref() == summary {
        st.summary = None;
    }
    let _ = state::save(state_dir, &st);
}

/// Print a terminal run's lines on stderr and acknowledge them.
fn say_terminal(state_dir: &Path, r: &Report, summary: Option<&str>) {
    let notes = state::load(state_dir).map(|m| m.notes).unwrap_or_default();
    let (lines, keys) = terminal_lines(r, &notes, now());
    for l in &lines {
        eprintln!("{l}");
    }
    acknowledge(state_dir, &keys, summary);
}

/// Log a post-spawn run's lines (never the terminal).
fn log_lines(r: &Report) {
    let mut lines = Vec::new();
    if !r.changed.is_empty() {
        lines.push(changed_line(r));
    }
    lines.extend(
        r.errors
            .iter()
            .map(|l| format!("csm: migration error: {l}")),
    );
    lines.extend(
        r.pending
            .iter()
            .map(|l| format!("csm: migration pending: {l}")),
    );
    for l in lines {
        let _ = crate::hook::notify::append_log("migrate", &l);
    }
}

// ─── dispatch ─────────────────────────────────────────────────────────────────

/// FULL and NOTE triggers at dispatch. `run` and `migrate` decide for
/// themselves; every NONE word returns before the probe.
pub(crate) fn at_dispatch(word: &str, rest: &[OsString]) {
    if matches!(word, "run" | "migrate") {
        return;
    }
    match probe::trigger_class(word, rest, None) {
        TriggerClass::Full => {
            let r = run_with(Trigger::Terminal);
            if let Ok(env) = HostEnv::current() {
                say_terminal(&fsx::state_dir(&env), &r, None);
            }
        }
        TriggerClass::Note => {
            let Some(p) = probe_now(false) else {
                return;
            };
            note(&p);
            if word == "orca" {
                // `orca status` prints a migration row from this probe.
                DISPATCH_PROBE.with(|d| *d.borrow_mut() = Some(p));
            }
        }
        TriggerClass::Pane | TriggerClass::None => {}
    }
}

thread_local! {
    /// The NOTE probe [`at_dispatch`] ran for `orca`, which
    /// [`status_line`] reuses so one `csm orca status` probes once. Per
    /// thread: dispatch and the command run on the main thread.
    static DISPATCH_PROBE: std::cell::RefCell<Option<Probed>> =
        const { std::cell::RefCell::new(None) };
}

/// NOTE: the probe only, and at most one line a day. The day is kept in
/// the marker when one exists; otherwise in [`state::NOTE_FILE`], since a
/// new marker would read as a migration under way (the probe then stops
/// looking at the disk).
fn note(p: &Probed) {
    if p.outcome != ProbeOutcome::Pending {
        return;
    }
    const KEY: &str = "note:pending";
    let notes = match p.marker.as_ref() {
        Some(m) => m.notes.clone(),
        None => state::load_note_file(&p.state)
            .map(|t| std::collections::BTreeMap::from([(KEY.to_owned(), t)]))
            .unwrap_or_default(),
    };
    if !state::note_due(&notes, KEY, now()) {
        return;
    }
    let Ok(Some(_lock)) = MigrateLock::try_acquire(&p.state) else {
        return;
    };
    eprintln!(
        "csm: this machine still has the legacy profile layout; the next `csm` launch moves it to \
         Orca's accounts (`csm migrate` shows where it stands)"
    );
    match state::load(&p.state) {
        Some(mut st) => {
            st.notes.insert(KEY.to_owned(), now());
            let _ = state::save(&p.state, &st);
        }
        None => {
            let _ = state::save_note_file(&p.state, now());
        }
    }
}

/// The migration row of `csm orca status`.
pub(crate) fn status_line() -> String {
    let Some(p) = DISPATCH_PROBE
        .with(|d| d.borrow_mut().take())
        .or_else(|| probe_now(false))
    else {
        return "unknown".to_owned();
    };
    match (p.outcome, p.marker.as_ref()) {
        (ProbeOutcome::Done, _) | (ProbeOutcome::Fresh, _) => "done".to_owned(),
        (ProbeOutcome::Pending, Some(m)) if m.legacy.is_some() => {
            format!("in progress ({})", m.phase.as_str())
        }
        (ProbeOutcome::Pending, _) => "pending (legacy profiles found)".to_owned(),
    }
}

/// The dirs an inherited `CLAUDE_CONFIG_DIR` may name as a stale pin (the
/// recorded legacy dirs, the registry's while no snapshot exists, and
/// `~/.claude`), whether the cutover was recorded, and the recorded floor
/// profile's dir. Reads the marker and at most the registry.
pub(crate) fn stale_dirs(env: &HostEnv) -> (Vec<PathBuf>, bool, Option<PathBuf>) {
    let marker = state::load(&fsx::state_dir(env));
    let snap = marker.as_ref().and_then(|m| m.legacy.as_ref());
    let mut dirs = match snap {
        Some(s) => s.dirs(),
        None => load_legacy(&env.home)
            .map(|l| l.profiles.into_iter().map(|p| p.dir).collect())
            .unwrap_or_default(),
    };
    dirs.push(env.home.join(".claude"));
    let floor = snap.and_then(|s| {
        let name = s.floor.as_deref()?;
        s.profiles
            .iter()
            .find(|p| p.name == name)
            .map(|p| p.dir.clone())
    });
    (dirs, marker.is_some_and(|m| m.cutover.is_some()), floor)
}

/// The daily hint for a stale pin a launch replaced.
pub(crate) fn stale_pin_hint() {
    const KEY: &str = "note:stale-pin";
    let Ok(env) = HostEnv::current() else {
        return;
    };
    let state_dir = fsx::state_dir(&env);
    let notes = state::load(&state_dir).map(|m| m.notes).unwrap_or_default();
    if !state::note_due(&notes, KEY, now()) {
        return;
    }
    eprintln!(
        "csm: CLAUDE_CONFIG_DIR in this shell names an old profile dir; claude runs in Orca's dir \
         instead. Remove the export from your shell startup files."
    );
    // Only onto a marker the launch's probe left: a new one would read
    // as a migration under way.
    if let Ok(Some(_lock)) = MigrateLock::try_acquire(&state_dir)
        && let Some(mut st) = state::load(&state_dir)
    {
        st.notes.insert(KEY.to_owned(), now());
        let _ = state::save(&state_dir, &st);
    }
}

// ─── around the spawn ─────────────────────────────────────────────────────────

/// A run armed for after the spawn ([`start_post_spawn`]).
struct Armed {
    /// A pre-spawn run that outlived [`PRESPAWN_BUDGET`].
    worker: Option<(JoinHandle<()>, Receiver<Report>)>,
    after_prespawn: bool,
    /// The claude the launch spawns ([`note_child`]); unknown until noted.
    child: LaunchChild,
}

static ARMED: Mutex<Option<Armed>> = Mutex::new(None);

fn arm(a: Armed) {
    *ARMED.lock().unwrap_or_else(|e| e.into_inner()) = Some(a);
}

/// Is a post-spawn run armed? The recovery starter spawns its thread for
/// it even when no switch is pending.
pub(crate) fn post_spawn_armed() -> bool {
    ARMED.lock().unwrap_or_else(|e| e.into_inner()).is_some()
}

/// Tell the armed post-spawn run where the launch's claude runs: its `D`
/// (`None`: unknown) and session id. Called right before the spawn; a
/// launch with nothing armed ignores it.
pub(crate) fn note_child(dir: Option<PathBuf>, sid: Option<String>) {
    if let Some(a) = ARMED.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        a.child = match dir {
            Some(dir) => LaunchChild::In { dir, sid },
            None => LaunchChild::Unknown,
        };
    }
}

/// Is a pre-spawn run that outlived [`PRESPAWN_BUDGET`] still going? It may
/// hold `switch.lock` (an import, the floor select), so the launch skips
/// its own lock-taking repair and switch rather than wait up to
/// [`crate::orca::context::LOCK_WAIT`] for it: the recovery after the
/// spawn repairs what is pending.
pub(crate) fn prespawn_still_running() -> bool {
    worker_running(ARMED.lock().unwrap_or_else(|e| e.into_inner()).as_ref())
}

/// Does `armed` carry a pre-spawn worker that has not finished?
fn worker_running(armed: Option<&Armed>) -> bool {
    armed
        .and_then(|a| a.worker.as_ref())
        .is_some_and(|(h, _)| !h.is_finished())
}

/// An interactive launch, before the spawn: the probe, then stage A on a
/// worker the launch waits for at most [`PRESPAWN_BUDGET`]. A summary an
/// unwatched run left prints first. Never blocks the launch on a failure.
pub(crate) fn prespawn() {
    let Some(p) = probe_now(false) else {
        return;
    };
    let summary = p.marker.as_ref().and_then(|m| m.summary.clone());
    if let Some(s) = &summary {
        eprintln!("{s}");
    }
    if p.outcome != ProbeOutcome::Pending {
        acknowledge(&p.state, &[], summary.as_deref());
        return;
    }
    match bounded(PRESPAWN_BUDGET, || run_with(Trigger::PreSpawn)) {
        Bounded::Done(r) => {
            say_terminal(&p.state, &r, summary.as_deref());
            arm(Armed {
                worker: None,
                after_prespawn: true,
                child: LaunchChild::Unknown,
            });
        }
        Bounded::Running(handle, rx) => {
            acknowledge(&p.state, &[], summary.as_deref());
            arm(Armed {
                worker: Some((handle, rx)),
                after_prespawn: true,
                child: LaunchChild::Unknown,
            });
        }
        Bounded::Failed => acknowledge(&p.state, &[], summary.as_deref()),
    }
}

/// What [`bounded`] saw when its budget ran out or its worker finished.
pub(crate) enum Bounded<T> {
    /// The worker finished within the budget (and was joined).
    Done(T),
    /// The budget ran out first: the worker still runs, its result comes
    /// on the receiver.
    Running(JoinHandle<()>, Receiver<T>),
    /// No thread could be started, or the worker died without a result.
    Failed,
}

/// Run `work` on a `csm-migrate` thread and wait for it at most `budget`.
/// The launch's pre-spawn wait: the caller spawns claude as soon as this
/// returns, whatever the worker is doing.
pub(crate) fn bounded<T: Send + 'static>(
    budget: Duration,
    work: impl FnOnce() -> T + Send + 'static,
) -> Bounded<T> {
    let (tx, rx) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("csm-migrate".into())
        .spawn(move || {
            let _ = tx.send(work());
        });
    let Ok(handle) = spawned else {
        return Bounded::Failed;
    };
    match rx.recv_timeout(budget) {
        Ok(r) => {
            let _ = handle.join();
            Bounded::Done(r)
        }
        Err(RecvTimeoutError::Timeout) => Bounded::Running(handle, rx),
        Err(RecvTimeoutError::Disconnected) => {
            let _ = handle.join();
            Bounded::Failed
        }
    }
}

/// Stage B's shell: Orca's `D` and the login floor for I3, then
/// [`carry::carry`]. `pane`: the pre-spawn exception.
#[allow(clippy::too_many_arguments, reason = "one call per trigger")]
fn run_carry(
    env: &HostEnv,
    state_dir: &Path,
    legacy: &Legacy,
    dry_run: bool,
    pane: bool,
    child: &LaunchChild,
    st: &mut MigrationState,
    report: &mut Report,
) -> carry::CarryEnd {
    let orca = crate::launch_context::orca_main_dir(env);
    let floor = if pane {
        // No launchctl before a pane's spawn: Orca runs, so its D decides.
        None
    } else {
        cutover::session_floor().ok().flatten()
    };
    let i3 = carry::i3_applies(&orca, floor.as_deref(), &env.home);
    carry::carry(
        env,
        state_dir,
        legacy,
        &crate::orca::live::SystemProcs,
        carry::CarryOpts {
            dry_run,
            pane,
            i3,
            now: std::time::SystemTime::now(),
            child_live: child.runs(),
            child_sid: child.sid(),
        },
        st,
        report,
    )
}

/// The pane pre-spawn exception: when Orca's live `D` is already
/// `~/.claude` (the fleet broke the contract, or a reboot beat the
/// cutover) and the migration is pending, B1 and B2 run before the spawn,
/// so a resumed pane finds its transcripts. Renames and the config merge
/// only: no Keychain, network or RPC, no copy across filesystems, a 1 s
/// lock wait, nothing printed (the log only). Anything else waits for the
/// run after the spawn.
pub(crate) fn pane_prespawn() {
    let Some(p) = probe_now(true) else {
        return;
    };
    if p.outcome != ProbeOutcome::Pending {
        return;
    }
    let crate::launch_context::OrcaMain::Dir(o) = crate::launch_context::orca_main_dir(&p.env)
    else {
        return;
    };
    if carry::lexical(&o.dir) != carry::lexical(&p.env.home.join(".claude")) {
        return;
    }
    let Ok(Some(_lock)) = MigrateLock::try_acquire(&p.state) else {
        return;
    };
    let mut st = state::load(&p.state).unwrap_or_default();
    if st.phase == Phase::Done {
        return;
    }
    let Ok(registry) = load_legacy(&p.env.home) else {
        return;
    };
    let snap = merge_snapshot(st.legacy.as_ref(), &registry, first_seen_at);
    if nothing_recorded(
        &snap,
        std::fs::symlink_metadata(probe::shared_path(&p.env.home)).is_ok(),
        st.cutover.is_some(),
    ) {
        return;
    }
    let legacy = as_legacy(&snap);
    st.legacy = Some(snap);
    let mut report = Report {
        legacy: true,
        ..Report::default()
    };
    run_carry(
        &p.env,
        &p.state,
        &legacy,
        false,
        true,
        &LaunchChild::None,
        &mut st,
        &mut report,
    );
    let _ = state::save(&p.state, &st);
    log_lines(&report);
}

/// A pane or structured launch: nothing before the spawn but
/// [`pane_prespawn`]; the run happens after it, log only.
pub(crate) fn arm_pane() {
    arm(Armed {
        worker: None,
        after_prespawn: false,
        child: LaunchChild::Unknown,
    });
}

/// After the spawn, on the recovery thread: hand the armed run to a thread
/// of its own and return at once, so the supervisor joins only the
/// recovery when claude exits ([`finish_on_exit`] waits for this run for at
/// most [`EXIT_GRACE`]). Runs once per process: it takes what was armed.
pub(crate) fn start_post_spawn() {
    let Some(a) = ARMED.lock().unwrap_or_else(|e| e.into_inner()).take() else {
        return;
    };
    if let Ok(h) = spawn_post_spawn(a, post_spawn) {
        *POST_SPAWN.lock().unwrap_or_else(|e| e.into_inner()) = Some(h);
    }
}

/// Start `run` over `a` on the `csm-migrate-post` thread. The seam
/// [`start_post_spawn`] uses.
fn spawn_post_spawn(a: Armed, run: fn(Armed)) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("csm-migrate-post".into())
        .spawn(move || run(a))
}

/// The run after the spawn: join a pre-spawn run that outlived its budget
/// (its lines go to the log and, when it changed something, to the
/// summary shown at the next launch), then run what is left. Log only:
/// the terminal belongs to claude now.
fn post_spawn(a: Armed) {
    if let Some((handle, rx)) = a.worker {
        let _ = handle.join();
        if let Ok(r) = rx.recv() {
            log_lines(&r);
            if !r.changed.is_empty()
                && let Ok(env) = HostEnv::current()
            {
                keep_summary(&fsx::state_dir(&env), &changed_line(&r));
            }
        }
    }
    let r = run_with_child(
        Trigger::PostSpawn {
            after_prespawn: a.after_prespawn,
        },
        &a.child,
    );
    log_lines(&r);
}

// ─── stopping at exit ─────────────────────────────────────────────────────────

/// How long a launch that ends waits for a migration run still going (the
/// pre-spawn worker of a launch that never spawned, the run after a
/// spawn) before the process exits anyway. Every stage resumes after a
/// crash, so a run cut here costs only a rerun at the next launch; a
/// [`Critical`] section is waited out.
pub(crate) const EXIT_GRACE: Duration = Duration::from_secs(1);

/// The run after the spawn, started by [`start_post_spawn`].
static POST_SPAWN: Mutex<Option<JoinHandle<()>>> = Mutex::new(None);

/// Set once the launch's claude has exited: a launch-bound run starts no
/// further stage.
static STOP: AtomicBool = AtomicBool::new(false);

/// How many [`Critical`] sections run now.
static CRITICAL: AtomicUsize = AtomicUsize::new(0);

/// A step that must not be cut by the process exiting: a copy across
/// filesystems (a half-copied tree would be drained as a collision next
/// time) and a hold of Claude Code's `<config>.lock` (left behind, it
/// blocks every Claude Code's config save until it goes stale). Held, it
/// makes [`finish_on_exit`] wait past [`EXIT_GRACE`].
pub(crate) struct Critical(());

impl Critical {
    pub(crate) fn enter() -> Critical {
        CRITICAL.fetch_add(1, Ordering::SeqCst);
        Critical(())
    }
}

impl Drop for Critical {
    fn drop(&mut self) {
        CRITICAL.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Claude Code's `<config>.lock` ([`fsx::ClaudeConfigLocks`]) held
/// inside a [`Critical`] section, entered once the lock is taken (the wait
/// for it is not critical). The lock goes before the section ends.
pub(crate) struct HeldConfigLock {
    pub(crate) locks: crate::orca::fsx::ClaudeConfigLocks,
    _critical: Critical,
}

/// Take Claude Code's lock on `config` (the path as Claude Code builds
/// it, not resolved) as a [`HeldConfigLock`].
pub(crate) fn hold_config_lock(
    config: &std::path::Path,
    wait: std::time::Duration,
) -> std::io::Result<HeldConfigLock> {
    let locks = crate::orca::fsx::ClaudeConfigLocks::acquire(config, wait)?;
    Ok(HeldConfigLock {
        locks,
        _critical: Critical::enter(),
    })
}

fn in_critical() -> bool {
    CRITICAL.load(Ordering::SeqCst) > 0
}

/// The launch's claude has exited: runs bound to the launch finish the
/// stage they are in and start no other. The limit-switch hop that may
/// follow then does not queue behind a cutover or a retire for
/// `switch.lock`.
pub(crate) fn child_exited() {
    STOP.store(true, Ordering::SeqCst);
}

/// May a run for `trigger` start another stage? Only the launch-bound
/// runs stop once `stopped`. Pure.
fn may_continue(trigger: Trigger, stopped: bool) -> bool {
    !(stopped && matches!(trigger, Trigger::PreSpawn | Trigger::PostSpawn { .. }))
}

/// Wait until `finished`, for at most `grace`, and past it while
/// `critical` holds. `true` when it finished. Pure over its probes.
fn wait_bounded(finished: &dyn Fn() -> bool, grace: Duration, critical: &dyn Fn() -> bool) -> bool {
    let deadline = std::time::Instant::now() + grace;
    loop {
        if finished() {
            return true;
        }
        if std::time::Instant::now() >= deadline && !critical() {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A pre-spawn worker of a launch that never spawned (the picker was
/// cancelled, a step before the spawn failed): its report, when it ends
/// within `grace`, and `None` when it is still going (the process exit
/// then cuts it; the next launch resumes).
fn settle_unspawned(
    worker: (JoinHandle<()>, Receiver<Report>),
    grace: Duration,
    critical: &dyn Fn() -> bool,
) -> Option<Report> {
    let (handle, rx) = worker;
    if !wait_bounded(&|| handle.is_finished(), grace, critical) {
        return None;
    }
    let _ = handle.join();
    rx.try_recv().ok()
}

/// A launch ends (claude exited, or it never spawned): no launch-bound run
/// starts another stage, a pre-spawn run that never saw a spawn gets
/// [`EXIT_GRACE`] to report on the terminal (which is csm's again), and the
/// run after the spawn gets the same to finish. Called on every return of
/// `csm run` after [`prespawn`] or [`arm_pane`], and before a process exit
/// that ends the launch; idempotent.
pub(crate) fn finish_on_exit() {
    child_exited();
    let armed = ARMED.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(Armed {
        worker: Some(worker),
        ..
    }) = armed
        && let Some(r) = settle_unspawned(worker, EXIT_GRACE, &in_critical)
        && let Ok(env) = HostEnv::current()
    {
        say_terminal(&fsx::state_dir(&env), &r, None);
    }
    let post = POST_SPAWN.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(h) = post
        && wait_bounded(&|| h.is_finished(), EXIT_GRACE, &in_critical)
    {
        let _ = h.join();
    }
}

/// Runs [`finish_on_exit`] when a launch returns, on every path.
pub(crate) struct LaunchGuard(());

impl LaunchGuard {
    pub(crate) fn new() -> LaunchGuard {
        LaunchGuard(())
    }
}

impl Drop for LaunchGuard {
    fn drop(&mut self) {
        finish_on_exit();
    }
}

/// Keep `line` for the next terminal launch.
fn keep_summary(state_dir: &Path, line: &str) {
    let Ok(Some(_lock)) = MigrateLock::try_acquire(state_dir) else {
        return;
    };
    let Some(mut st) = state::load(state_dir) else {
        return;
    };
    st.summary = Some(line.to_owned());
    let _ = state::save(state_dir, &st);
}
