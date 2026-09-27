//! Stage A (adopt), live-safe: every legacy login becomes an Orca account
//! (A1), Orca's active host account becomes a managed one when it has
//! none (A2), and with Orca stopped a dir's fresher grant is read back
//! into its stash (A3).
//!
//! A1 imports over RPC while Orca runs and through the offline store
//! protocol otherwise ([`import_action`]); A2 selects the floor profile's
//! account only while Orca has no active host account, never null; A3
//! writes a stash, so it runs only while Orca is stopped. The pure
//! per-row cores ([`row_action`], [`unreadable_row`], [`import_line`], …)
//! came over from the former `csm migrate import`.

use std::path::{Path, PathBuf};

use crate::orca::add::{self, SystemClaude};
use crate::orca::context::{Context, LOCK_WAIT};
use crate::orca::http::{OauthHttp, SystemHttp};
use crate::orca::live::{ProcFacts, SystemProcs, registry_users};
use crate::orca::quarantine::Quarantine;
use crate::orca::readback::{self, ReadBack};
use crate::orca::record::AccountRecord;
use crate::orca::runtime::runtime_paths;
use crate::orca::switch::{self, Outcome};
use crate::orca::{HostOs, OrcaError, OrcaView, SnapshotOptions, fsx};

use super::legacy::*;
use super::state::{MigrationState, NETWORK_BACKOFF_SECS, StepStatus};
use super::{LaunchChild, Report, ReportRow};

use anyhow::bail;

// ─── store-writing steps on a SQLite profile (pure) ───────────────────────────

/// What step 4 does with one plan row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RowAction {
    /// `accounts import` offline (patches the store).
    Import,
    /// Offline read-back into the account's stash (no store write).
    ReadBack(String),
    /// The import patches a SQLite-backed store, which csm changes only
    /// through a running Orca: left for `csm accounts import <dir>` then.
    Defer,
    Skip,
}

/// Step 4's action for a row; `sqlite`: the profile keeps its state in
/// SQLite. Pure.
pub(crate) fn row_action(status: &Status, sqlite: bool) -> RowAction {
    match status {
        Status::ToImport if sqlite => RowAction::Defer,
        Status::ToImport => RowAction::Import,
        Status::InOrca {
            id,
            fresher: Some(true),
        } => RowAction::ReadBack(id.clone()),
        _ => RowAction::Skip,
    }
}

/// Why step 4 must not act on a row: a Keychain item of its dir could not
/// be read (a locked Keychain, a `security` timeout), so its freshest
/// grant is unknown. Importing, reading back or keeping the stash on that
/// partial read could leave the dir's newer grant behind; the row counts
/// as failed instead, which also keeps step 7's floor switch (whose
/// refresh rotates the stash's refresh token) from running. Pure.
pub(crate) fn unreadable_row(facts: &ProfileFacts) -> Option<&'static str> {
    facts
        .grant_sources
        .contains(&KEYCHAIN_UNREADABLE)
        .then_some("a Keychain item of the dir could not be read (is the Keychain locked?); nothing done, rerun when it reads")
}

/// Step 4's import: `previousLegacyCredentialsSha256` is the unscoped
/// item's digest, read under `switch.lock` inside the import so a
/// concurrent switch cannot slip in between.
pub(crate) fn import_one(
    ctx: &Context,
    procs: &dyn ProcFacts,
    dir: &Path,
) -> anyhow::Result<String> {
    let cli = SystemClaude::configured()?;
    let c = ctx.with_accounts_env(procs, |env| add::import_current_legacy(env, &cli, dir))?;
    import_line(&c).map_err(anyhow::Error::msg)
}

/// Step 4's row for one import: `Err` when Orca refused the redo (nothing
/// was imported) or did not confirm it, so the row counts as failed and
/// the floor switch does not run on top of it. Pure.
pub(crate) fn import_line(c: &add::AccountChange) -> Result<String, String> {
    use crate::orca::store::RedoOutcome;
    let who = c
        .email
        .clone()
        .or_else(|| c.id.clone())
        .unwrap_or_else(|| "the account".into());
    let leftover = c
        .leftover
        .as_ref()
        .map(|l| format!("; left for `csm accounts doctor`: {l}"))
        .unwrap_or_default();
    match &c.redo {
        Some(RedoOutcome::Failed(why)) => Err(format!(
            "{who} was not imported: Orca refused it ({why}){leftover}"
        )),
        Some(RedoOutcome::Uncertain(why)) => Err(format!(
            "{who} may not be imported: it was handed to Orca, which did not confirm it ({why}); \
             check `csm accounts doctor`{leftover}"
        )),
        _ => Ok(format!("imported {who}{leftover}")),
    }
}

pub(crate) fn read_back_one(
    ctx: &Context,
    procs: &dyn ProcFacts,
    view: &OrcaView,
    http: &dyn OauthHttp,
    dir: &Path,
    id: &str,
) -> anyhow::Result<String> {
    let _lock = fsx::SwitchLock::acquire(&ctx.state, crate::orca::context::LOCK_WAIT)?;
    if ctx.orca_running(procs) {
        bail!("Orca started; read-back skipped");
    }
    let paths = runtime_paths(Some(&dir.to_string_lossy()), &ctx.env.home, |p| p.exists());
    let records: Vec<AccountRecord> = view.host_accounts().cloned().collect();
    let q = Quarantine::new(ctx.os(), &ctx.state);
    let rep = readback::read_back(&ReadBack {
        os: ctx.os(),
        user_data: &ctx.user_data.dir,
        paths: &paths,
        keychain_user: &ctx.keychain_user,
        records: &records,
        exclude: None,
        // Never refresh here: the dir keeps its grant (and its refresh
        // token) until retire, and claude may run in it again before then.
        // A 401 leaves the grant quarantined under its account; retire
        // files it as `Retired` and settle refreshes it once the dir is
        // retired and nothing else holds it.
        live_claude: true,
        http,
        quarantine: &q,
        now_ms: chrono::Utc::now().timestamp_millis(),
        migration: true,
    })?;
    let mut s = match &rep.persisted {
        Some(p) if p == id => format!("stash {id} now holds the dir's grant"),
        Some(p) => format!("the dir's grant went to stash {p}"),
        None => format!("stash {id} kept"),
    };
    if !rep.quarantined.is_empty() {
        s.push_str(&format!("; {} grant(s) quarantined", rep.quarantined.len()));
    }
    Ok(s)
}

// ─── A1: import (pure) ────────────────────────────────────────────────────────

/// How A1 adds a `ToImport` login.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImportAction {
    /// `accounts.addClaudeFromConfigDir` on the running Orca.
    Rpc,
    /// The offline store protocol (`add::import_current_legacy`).
    Offline,
    /// Not now; the reason is the pending line.
    Defer(&'static str),
}

/// A1's route. A running Orca takes the import over RPC whatever its
/// store; a stopped one is written only through the offline protocol's
/// gates (the tested version, a JSON store, store access), never on Linux,
/// which has no Orca version source yet, and never on Windows, whose Orca
/// detection is inferred. The offline import re-checks each
/// gate under its lock; this is the decision the report shows. Pure.
pub(crate) fn import_action(
    orca_running: bool,
    os: HostOs,
    sqlite: bool,
    version_ok: bool,
    store_access_allowed: bool,
) -> ImportAction {
    if orca_running {
        ImportAction::Rpc
    } else if os == HostOs::Linux {
        ImportAction::Defer("Orca is stopped and csm changes its store on Linux only through it")
    } else if os == HostOs::Windows {
        // Orca detection on Windows is inferred (design section 6): an
        // automatic store write behind an Orca csm failed to see would
        // break Invariant 6.
        ImportAction::Defer("Orca is stopped and csm changes its store on Windows only through it")
    } else if sqlite {
        ImportAction::Defer("Orca keeps its state in SQLite, so the import waits for Orca to run")
    } else if !version_ok {
        ImportAction::Defer(
            "the installed Orca version is not one csm was tested with; the import waits for Orca to run",
        )
    } else if !store_access_allowed {
        ImportAction::Defer(
            "this Orca userData is not csm's to write; the import waits for Orca to run",
        )
    } else {
        ImportAction::Offline
    }
}

// ─── A2: the active account (pure) ────────────────────────────────────────────

/// What A2 does about Orca's active host account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ActiveAction {
    /// Orca already has an active account (it wins), or there is no floor
    /// profile to name one.
    Keep,
    /// Select the floor profile's account.
    Select(String),
    /// The floor profile holds no login csm could adopt: nothing to select.
    NoFloorAccount,
    /// Not now; the reason is the pending line.
    Defer(&'static str),
}

/// The facts A2 decides on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ActiveFacts<'a> {
    /// Orca's effective active host account.
    pub active_id: Option<&'a str>,
    /// `None`: no floor profile.
    pub floor: Option<&'a Status>,
    pub orca_running: bool,
    pub os: HostOs,
    pub sqlite: bool,
    pub version_ok: bool,
    pub store_access_allowed: bool,
    /// csm's `D` is the floor profile's dir, where Orca runs once started
    /// with the floor set: an offline switch materializes there.
    pub d_is_floor: bool,
    /// A dry run: the floor's grant was probed for presence only, so its
    /// freshness is unknown and not held against the select.
    pub dry_run: bool,
}

/// A2. Never selects null: only a floor account Orca has, and only while
/// Orca's copy of its login is not older than the floor dir's. A select
/// from no active account does no read-back, in Orca
/// (runtime-auth-sync.ts: the read-back runs only for the last-synced
/// account, which is null here) or in csm's offline switch: it
/// materializes the stash into `D`, so a stash whose refresh token the
/// floor's claude has rotated away would log `D` out. Such a login is read
/// back first (A3, Orca stopped); until then the select waits. Pure.
pub(crate) fn active_action(f: &ActiveFacts<'_>) -> ActiveAction {
    if f.active_id.is_some() {
        return ActiveAction::Keep;
    }
    let (id, fresher) = match f.floor {
        None => return ActiveAction::Keep,
        Some(Status::InOrca { id, fresher }) => (id.clone(), *fresher),
        Some(Status::ToImport) => {
            return ActiveAction::Defer("the floor profile's login is not in Orca yet");
        }
        Some(Status::NoCredentials) => return ActiveAction::NoFloorAccount,
    };
    match fresher {
        Some(true) => {
            return ActiveAction::Defer(
                "the floor profile's login is fresher than Orca's copy of it; csm selects it once \
                 that copy is read back, with Orca stopped",
            );
        }
        None if !f.dry_run => {
            return ActiveAction::Defer(
                "the floor profile's login could not be compared with Orca's copy (is the \
                 Keychain locked?); csm selects it once it can",
            );
        }
        _ => {}
    }
    if f.orca_running {
        ActiveAction::Select(id)
    } else if f.os == HostOs::Linux {
        ActiveAction::Defer(
            "Orca has no active account; csm selects one on Linux only through a running Orca",
        )
    } else if f.os == HostOs::Windows {
        ActiveAction::Defer(
            "Orca has no active account; csm selects one on Windows only through a running Orca",
        )
    } else if f.sqlite {
        ActiveAction::Defer(
            "Orca has no active account and keeps its state in SQLite; csm selects one once Orca runs",
        )
    } else if !f.version_ok || !f.store_access_allowed {
        ActiveAction::Defer(
            "Orca has no active account and its store is not csm's to write; csm selects one once Orca runs",
        )
    } else if !f.d_is_floor {
        ActiveAction::Defer(
            "Orca has no active account and csm's D is not the floor profile's dir; csm selects one once Orca runs",
        )
    } else {
        ActiveAction::Select(id)
    }
}

// ─── A3: read-back (pure) ─────────────────────────────────────────────────────

/// A3's gate for a row: `Ok(id)` reads the dir's fresher grant back into
/// stash `id`, `Err(reason)` leaves it (to settle, which runs once Orca
/// stops). It writes a stash, so never behind a running Orca, never on
/// Windows (whose Orca detection is unverified), only for a tested Orca
/// version and store access, and never under a live claude in the dir.
/// Pure.
pub(crate) fn readback_gate(
    status: &Status,
    orca_running: bool,
    os: HostOs,
    version_ok: bool,
    store_access_allowed: bool,
    live_claude: bool,
) -> Result<String, &'static str> {
    let RowAction::ReadBack(id) = row_action(status, false) else {
        return Err("nothing to read back");
    };
    if orca_running {
        Err("the dir's grant is fresher than the stash; it is read back once Orca is stopped")
    } else if os == HostOs::Windows {
        Err("the dir's grant is fresher than the stash; csm does not read it back on Windows")
    } else if !version_ok || !store_access_allowed {
        Err("the dir's grant is fresher than the stash; the stash is not csm's to write")
    } else if live_claude {
        Err("the dir's grant is fresher than the stash; a claude runs in the dir")
    } else {
        Ok(id)
    }
}

// ─── store-less hosts (pure) ──────────────────────────────────────────────────

/// The marker step that flags a store-less host: adopt runs again on every
/// FULL trigger while it is set, so a store that appears later is adopted.
pub(crate) const STORE_LESS_STEP: &str = "store-less";

/// A host with no Orca store of its own (design section 2): not macOS,
/// Orca not running here, and no store csm may treat as this host's (none
/// at all, or a WSL view of the Windows Orca's userData). Such a host gets
/// the files stages only: its logins stay in their dirs until a store
/// exists. macOS without a store is not one: Orca's first start creates
/// it, and the migration waits for that. Pure.
pub(crate) fn store_less(
    os: HostOs,
    orca_running: bool,
    has_store: bool,
    store_access_allowed: bool,
) -> bool {
    os != HostOs::MacOs && !orca_running && (!has_store || !store_access_allowed)
}

// ─── errors ───────────────────────────────────────────────────────────────────

/// An error's class for the marker, and whether it only waits (a network,
/// RPC, Keychain or refusal that a rerun may pass) rather than failing.
pub(crate) fn error_class(e: &anyhow::Error) -> (&'static str, bool) {
    if let Some(o) = e.downcast_ref::<OrcaError>() {
        return match o {
            OrcaError::Network(_) => ("network", true),
            OrcaError::Rpc(_) => ("rpc", true),
            OrcaError::Refused(_) => ("refused", true),
            OrcaError::Keychain(_) => ("keychain", true),
            OrcaError::Io { source, .. } if source.kind() == std::io::ErrorKind::WouldBlock => {
                ("busy", true)
            }
            OrcaError::Io { .. } => ("io", false),
            OrcaError::Invalid(_) => ("invalid", false),
        };
    }
    if let Some(io) = e.downcast_ref::<std::io::Error>() {
        return if io.kind() == std::io::ErrorKind::WouldBlock {
            ("busy", true)
        } else {
            ("io", false)
        };
    }
    ("other", true)
}

// ─── the stage (I/O shell) ────────────────────────────────────────────────────

/// How stage A runs.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AdoptOpts<'a> {
    pub dry_run: bool,
    /// Unix seconds, for the network backoff.
    pub now: i64,
    /// An explicit `csm migrate`: the network backoff does not apply.
    pub explicit: bool,
    /// The launch's claude may start in csm's `D` while this run writes it
    /// (the pre-spawn run, which can outlive its budget): A2's switch then
    /// treats it as live (no refresh, Orca's materialize order).
    pub child_in_d: bool,
    /// The launch's claude after a spawn.
    pub child: &'a LaunchChild,
}

/// Stage A's end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AdoptEnd {
    /// Every login is in Orca (or holds none) and Orca has a managed active
    /// account (or no floor names one): the phase may move on.
    pub settled: bool,
}

fn same_dir(a: &Path, b: &Path) -> bool {
    let trim = |p: &Path| PathBuf::from(p.to_string_lossy().trim_end_matches(['/', '\\']));
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| trim(p));
    trim(a) == trim(b) || canon(a) == canon(b)
}

fn snapshot() -> anyhow::Result<OrcaView> {
    Ok(crate::orca::snapshot(&SnapshotOptions::default())?)
}

fn row(report: &mut Report, p: &LegacyProfile, stage: &'static str, line: impl Into<String>) {
    report.rows.push(ReportRow {
        name: p.name.clone(),
        dir: p.dir.clone(),
        stage,
        line: line.into(),
    });
}

/// Record a failed step: pending (with the network backoff) or an error.
pub(super) fn failed(
    report: &mut Report,
    st: &mut MigrationState,
    key: &str,
    what: String,
    e: &anyhow::Error,
    now: i64,
) {
    let (class, waits) = error_class(e);
    let line = format!("{what}: {e}");
    if class == "network" {
        st.next_attempt_at = Some(now + NETWORK_BACKOFF_SECS);
    }
    if waits {
        st.set_step(key, StepStatus::Pending, Some(class));
        report.pending.push(line);
    } else {
        st.set_step(key, StepStatus::Error, Some(class));
        report.errors.push(line);
    }
}

/// Run stage A over the legacy profiles. Every decision is taken again
/// from Orca's list and the dirs as they are now.
pub(crate) fn adopt(
    legacy: &Legacy,
    opts: AdoptOpts<'_>,
    st: &mut MigrationState,
    report: &mut Report,
) -> AdoptEnd {
    crate::usage::reach::note("migrate-adopt");
    let procs = SystemProcs;
    let unsettled = AdoptEnd { settled: false };
    // R1: while Orca runs, csm acts in Orca's live D (a legacy floor may
    // have started it in `<floor>`); A2's select over RPC is refused when
    // csm's D differs from Orca's.
    let pin = crate::launch_context::launch_dir(false)
        .map_or(crate::launch_context::ConfigDirPin::Leave, |d| d.pin);
    let ctx = match Context::current_pinned(&procs, &pin) {
        Ok(c) => c,
        Err(e) => {
            report.errors.push(format!("adopt: {e}"));
            return unsettled;
        }
    };
    let mut view = match snapshot() {
        Ok(v) => v,
        Err(e) => {
            report.errors.push(format!("adopt: {e}"));
            return unsettled;
        }
    };
    if view.running
        && let Some(why) = &view.rpc_error
    {
        // Orca's store lags its memory: classifying against it could
        // import an account Orca already has.
        report.pending.push(format!(
            "adopt: Orca runs but did not list its accounts ({why})"
        ));
        return unsettled;
    }
    let store_access = ctx.user_data.store_access_allowed();
    if store_less(ctx.os(), view.running, view.store.is_some(), store_access) {
        for p in &legacy.profiles {
            row(
                report,
                p,
                "adopt",
                "no Orca store on this host: the login stays in its dir until one exists",
            );
        }
        st.set_step(STORE_LESS_STEP, StepStatus::Pending, None);
        return AdoptEnd { settled: true };
    }
    st.steps.remove(STORE_LESS_STEP);
    let network_due = opts.explicit || st.network_due(opts.now);
    // Grants are read (a secret) only when A3 may act on them.
    let probe = if !view.running && !opts.dry_run {
        Probe::Read
    } else {
        Probe::Presence
    };
    let mut settled = true;
    let mut facts_of: Vec<(LegacyProfile, ProfileFacts)> = Vec::new();

    // A1.
    for p in &legacy.profiles {
        let key = format!("A1:{}", p.name);
        if !p.dir.is_dir() {
            row(report, p, "adopt", "the dir is gone; nothing to import");
            st.set_step(&key, StepStatus::Done, None);
            continue;
        }
        let facts = profile_facts(&ctx, p, probe);
        facts_of.push((p.clone(), facts.clone()));
        if let Some(why) = unreadable_row(&facts) {
            row(report, p, "adopt", why);
            report.pending.push(format!("{}: {why}", p.name));
            st.set_step(&key, StepStatus::Pending, Some("keychain"));
            settled = false;
            continue;
        }
        let host: Vec<AccountRecord> = view.host_accounts().cloned().collect();
        let sg = |id: &str| match probe {
            Probe::Read => stash_grant(&ctx, &view, id),
            Probe::Presence => None,
        };
        match classify(&facts, &host, &sg) {
            Status::NoCredentials => {
                row(report, p, "adopt", "holds no login");
                st.set_step(&key, StepStatus::Done, None);
            }
            Status::InOrca { id, .. } => {
                row(report, p, "adopt", format!("in Orca ({id})"));
                st.set_step(&key, StepStatus::Done, None);
            }
            Status::ToImport => {
                let action = import_action(
                    view.running,
                    ctx.os(),
                    view.sqlite_state,
                    view.version_ok,
                    store_access,
                );
                let route = match action {
                    ImportAction::Defer(why) => {
                        row(report, p, "adopt", format!("to import: {why}"));
                        report.pending.push(format!("{}: {why}", p.name));
                        st.set_step(&key, StepStatus::Pending, Some("deferred"));
                        settled = false;
                        continue;
                    }
                    ImportAction::Rpc => "over Orca's RPC",
                    ImportAction::Offline => "into Orca's store",
                };
                if opts.dry_run {
                    row(report, p, "adopt", format!("would import {route}"));
                    settled = false;
                    continue;
                }
                if !network_due {
                    row(report, p, "adopt", "to import: waits for the network");
                    report.pending.push(format!(
                        "{}: the import waits after a network failure",
                        p.name
                    ));
                    settled = false;
                    continue;
                }
                match import_one(&ctx, &procs, &p.dir) {
                    Ok(line) => {
                        row(report, p, "adopt", line.clone());
                        report.changed.push(format!("{}: {line}", p.name));
                        st.set_step(&key, StepStatus::Done, None);
                        // A later profile may share this identity: classify
                        // it against Orca's list as it is now.
                        match snapshot() {
                            Ok(v) => view = v,
                            Err(e) => {
                                report.errors.push(format!("adopt: {e}"));
                                return unsettled;
                            }
                        }
                    }
                    Err(e) => {
                        row(report, p, "adopt", format!("not imported: {e}"));
                        failed(
                            report,
                            st,
                            &key,
                            format!("{}: import", p.name),
                            &e,
                            opts.now,
                        );
                        settled = false;
                    }
                }
            }
        }
    }

    // A3, before A2: a floor login fresher than its stash is read back
    // first, so A2 never selects a stale copy. It never holds the phase;
    // settle catches what it leaves.
    let host: Vec<AccountRecord> = view.host_accounts().cloned().collect();
    let http = SystemHttp::from_env();
    for (p, f) in &facts_of {
        let status = classify(f, &host, &|id| match probe {
            Probe::Read => stash_grant(&ctx, &view, id),
            Probe::Presence => None,
        });
        if !matches!(
            status,
            Status::InOrca {
                fresher: Some(true),
                ..
            }
        ) {
            continue;
        }
        let live = opts.child.uses(&p.dir)
            || registry_users(ctx.os(), &p.dir, &ctx.env.home, &procs).is_some();
        match readback_gate(
            &status,
            ctx.orca_running(&procs),
            ctx.os(),
            ctx.version_ok,
            store_access,
            live,
        ) {
            Err(why) => row(report, p, "read-back", why),
            Ok(id) if opts.dry_run => {
                row(
                    report,
                    p,
                    "read-back",
                    format!("would read the dir's grant back into stash {id}"),
                );
            }
            Ok(id) => match read_back_one(&ctx, &procs, &view, &http, &p.dir, &id) {
                Ok(line) => {
                    row(report, p, "read-back", line.clone());
                    report.changed.push(format!("{}: {line}", p.name));
                }
                Err(e) => row(report, p, "read-back", format!("left for later: {e}")),
            },
        }
    }

    // A2.
    let floor = legacy
        .floor
        .as_deref()
        .and_then(|n| legacy.profiles.iter().find(|p| p.name == n));
    // The floor's grant against its stash, read now (after A3): with Orca
    // running the probe above was presence only, and a select must not
    // materialize a stash older than the floor's login.
    let floor_facts = floor.and_then(|p| {
        let f = facts_of.iter().find(|(q, _)| q.name == p.name)?.1.clone();
        let wants_read = !opts.dry_run
            && view.active_id.is_none()
            && f.grant.is_none()
            && !f.grant_sources.is_empty();
        Some(if wants_read {
            profile_facts(&ctx, p, Probe::Read)
        } else {
            f
        })
    });
    let floor_status = floor.map(|_| match &floor_facts {
        // Not every grant could be read: freshness unknown (fail closed).
        Some(f) if !opts.dry_run && unreadable_row(f).is_some() => {
            match classify(f, &host, &|_| None) {
                Status::InOrca { id, .. } => Status::InOrca { id, fresher: None },
                other => other,
            }
        }
        Some(f) => classify(f, &host, &|id| {
            if opts.dry_run {
                None
            } else {
                stash_grant(&ctx, &view, id)
            }
        }),
        None => Status::NoCredentials,
    });
    let d_is_floor = floor.is_some_and(|p| same_dir(&ctx.paths.config_dir, &p.dir));
    let facts = ActiveFacts {
        active_id: view.active_id.as_deref(),
        floor: floor_status.as_ref(),
        orca_running: view.running,
        os: ctx.os(),
        sqlite: view.sqlite_state,
        version_ok: view.version_ok,
        store_access_allowed: store_access,
        d_is_floor,
        dry_run: opts.dry_run,
    };
    match (active_action(&facts), floor) {
        (ActiveAction::Keep, _) => st.set_step("A2", StepStatus::Done, None),
        (ActiveAction::NoFloorAccount, Some(p)) => {
            row(
                report,
                p,
                "active",
                "Orca has no active account and the floor profile holds no login to select",
            );
            st.set_step("A2", StepStatus::Done, None);
        }
        (ActiveAction::Defer(why), _) => {
            report.pending.push(format!("active account: {why}"));
            st.set_step("A2", StepStatus::Pending, Some("deferred"));
            settled = false;
        }
        (ActiveAction::Select(id), Some(p)) if opts.dry_run => {
            row(report, p, "active", format!("would select {id} in Orca"));
            settled = false;
        }
        (ActiveAction::Select(id), Some(p)) => match select_floor(
            &ctx,
            &procs,
            &id,
            opts.child_in_d || opts.child.uses(&ctx.paths.config_dir),
        ) {
            Ok(Some(line)) => {
                row(report, p, "active", line.clone());
                report.changed.push(line);
                st.set_step("A2", StepStatus::Done, None);
            }
            Ok(None) => st.set_step("A2", StepStatus::Done, None),
            Err(e) => {
                failed(report, st, "A2", "active account".into(), &e, opts.now);
                settled = false;
            }
        },
        (ActiveAction::NoFloorAccount | ActiveAction::Select(_), None) => {
            st.set_step("A2", StepStatus::Done, None);
        }
    }

    AdoptEnd { settled }
}

/// A2's write: under `switch.lock`, check again that Orca has no active
/// host account (a GUI select may have won the race), then switch to `id`
/// (RPC while Orca runs, the offline switch otherwise). `child_live`: the
/// launch's claude may be starting in `D`, so the offline switch neither
/// refreshes nor opens the neutral window. `Ok(None)`: Orca had an active
/// account by then.
fn select_floor(
    ctx: &Context,
    procs: &dyn ProcFacts,
    id: &str,
    child_live: bool,
) -> anyhow::Result<Option<String>> {
    let lock = fsx::SwitchLock::acquire(&ctx.state, LOCK_WAIT)?;
    let view = snapshot()?;
    if view.active_id.is_some() {
        return Ok(None);
    }
    if view.running && view.rpc_error.is_some() {
        bail!("Orca runs but did not list its accounts");
    }
    let http = SystemHttp::from_env();
    // Checked again inside the switch, on the state it acts on: a GUI
    // select since the snapshot above wins too.
    let r = ctx.with_switch_env_child(procs, &http, child_live, |env| {
        switch::select_if_none_active(env, &lock, id)
    })?;
    let Some(r) = r else {
        return Ok(None);
    };
    match r.outcome {
        Outcome::Switched | Outcome::AlreadyActive => Ok(Some(format!(
            "Orca had no active account; selected the floor profile's account ({id})"
        ))),
        Outcome::Uncertain(why) => Err(anyhow::Error::from(OrcaError::Refused(format!(
            "the select of {id} did not verify ({why}); `csm accounts doctor` reconciles"
        )))),
    }
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_action_over_orca_up_or_down() {
        use ImportAction::*;
        // Orca up: RPC, whatever the store, version or OS.
        for os in [HostOs::MacOs, HostOs::Linux, HostOs::Windows] {
            assert_eq!(import_action(true, os, true, false, false), Rpc);
            assert_eq!(import_action(true, os, false, true, true), Rpc);
        }
        // Orca down, JSON store, tested version: offline on macOS only.
        // Linux has no version source; Windows Orca detection is inferred,
        // so an automatic store write there waits for a running Orca.
        assert_eq!(
            import_action(false, HostOs::MacOs, false, true, true),
            Offline
        );
        assert!(
            matches!(import_action(false, HostOs::Windows, false, true, true), Defer(w) if w.contains("Windows"))
        );
        assert!(matches!(
            import_action(false, HostOs::Linux, false, true, true),
            Defer(_)
        ));
        // SQLite, untested version, no store access: deferred.
        assert!(
            matches!(import_action(false, HostOs::MacOs, true, true, true), Defer(w) if w.contains("SQLite"))
        );
        assert!(
            matches!(import_action(false, HostOs::MacOs, false, false, true), Defer(w) if w.contains("version"))
        );
        assert!(matches!(
            import_action(false, HostOs::MacOs, false, true, false),
            Defer(_)
        ));
    }

    fn facts<'a>(active: Option<&'a str>, floor: Option<&'a Status>) -> ActiveFacts<'a> {
        ActiveFacts {
            active_id: active,
            floor,
            orca_running: false,
            os: HostOs::MacOs,
            sqlite: false,
            version_ok: true,
            store_access_allowed: true,
            d_is_floor: true,
            dry_run: false,
        }
    }

    #[test]
    fn active_is_selected_only_when_orca_has_none() {
        let in_orca = Status::InOrca {
            id: "acct-work".into(),
            fresher: Some(false),
        };
        // An active account Orca already has wins.
        assert_eq!(
            active_action(&facts(Some("acct-home"), Some(&in_orca))),
            ActiveAction::Keep
        );
        // No floor profile: nothing to name.
        assert_eq!(active_action(&facts(None, None)), ActiveAction::Keep);
        assert_eq!(
            active_action(&facts(None, Some(&in_orca))),
            ActiveAction::Select("acct-work".into())
        );
        assert!(matches!(
            active_action(&facts(None, Some(&Status::ToImport))),
            ActiveAction::Defer(_)
        ));
        assert_eq!(
            active_action(&facts(None, Some(&Status::NoCredentials))),
            ActiveAction::NoFloorAccount
        );
        // Orca up: RPC, even on SQLite.
        let up = ActiveFacts {
            orca_running: true,
            sqlite: true,
            d_is_floor: false,
            ..facts(None, Some(&in_orca))
        };
        assert_eq!(active_action(&up), ActiveAction::Select("acct-work".into()));
        // Orca down: SQLite, Linux, untested, or a D the floor does not name defer.
        for f in [
            ActiveFacts {
                sqlite: true,
                ..facts(None, Some(&in_orca))
            },
            ActiveFacts {
                os: HostOs::Linux,
                ..facts(None, Some(&in_orca))
            },
            ActiveFacts {
                version_ok: false,
                ..facts(None, Some(&in_orca))
            },
            ActiveFacts {
                d_is_floor: false,
                ..facts(None, Some(&in_orca))
            },
            ActiveFacts {
                os: HostOs::Windows,
                ..facts(None, Some(&in_orca))
            },
        ] {
            assert!(matches!(active_action(&f), ActiveAction::Defer(_)), "{f:?}");
        }
    }

    /// Review round 1: a select from no active account materializes the
    /// stash into D without reading D back (Orca's runtime-auth-sync.ts
    /// reads back only the last-synced account). A floor login fresher
    /// than its stash, or one that could not be compared, holds the select
    /// back whether Orca runs or not; a dry run, which probes presence
    /// only, still previews it.
    #[test]
    fn a_fresher_floor_login_holds_the_select_back() {
        let fresher = Status::InOrca {
            id: "acct-work".into(),
            fresher: Some(true),
        };
        let unknown = Status::InOrca {
            id: "acct-work".into(),
            fresher: None,
        };
        for running in [true, false] {
            for st in [&fresher, &unknown] {
                let f = ActiveFacts {
                    orca_running: running,
                    ..facts(None, Some(st))
                };
                assert!(matches!(active_action(&f), ActiveAction::Defer(_)), "{f:?}");
            }
        }
        let w = |st| match active_action(&ActiveFacts {
            orca_running: true,
            ..facts(None, Some(st))
        }) {
            ActiveAction::Defer(w) => w,
            other => panic!("{other:?}"),
        };
        assert!(w(&fresher).contains("fresher"));
        assert!(w(&unknown).contains("compared"));
        let dry = ActiveFacts {
            orca_running: true,
            dry_run: true,
            ..facts(None, Some(&unknown))
        };
        assert_eq!(
            active_action(&dry),
            ActiveAction::Select("acct-work".into())
        );
        // An active account still wins over a fresher floor.
        assert_eq!(
            active_action(&facts(Some("acct-home"), Some(&fresher))),
            ActiveAction::Keep
        );
    }

    #[test]
    fn read_back_runs_only_with_orca_stopped() {
        let fresher = Status::InOrca {
            id: "acct-work".into(),
            fresher: Some(true),
        };
        assert_eq!(
            readback_gate(&fresher, false, HostOs::MacOs, true, true, false),
            Ok("acct-work".into())
        );
        assert!(readback_gate(&fresher, true, HostOs::MacOs, true, true, false).is_err());
        assert!(readback_gate(&fresher, false, HostOs::Windows, true, true, false).is_err());
        assert!(readback_gate(&fresher, false, HostOs::MacOs, false, true, false).is_err());
        assert!(readback_gate(&fresher, false, HostOs::MacOs, true, true, true).is_err());
        let kept = Status::InOrca {
            id: "acct-work".into(),
            fresher: Some(false),
        };
        assert!(readback_gate(&kept, false, HostOs::MacOs, true, true, false).is_err());
    }

    #[test]
    fn network_and_rpc_errors_wait_io_errors_fail() {
        let e = anyhow::Error::from(OrcaError::Network("dns".into()));
        assert_eq!(error_class(&e), ("network", true));
        let e = anyhow::Error::from(OrcaError::Invalid("bad".into()));
        assert_eq!(error_class(&e), ("invalid", false));
        let e = anyhow::Error::from(OrcaError::Refused("x".into())).context("csm migrate");
        assert_eq!(error_class(&e), ("refused", true));
        let e = anyhow::Error::from(std::io::Error::new(std::io::ErrorKind::WouldBlock, "held"));
        assert_eq!(error_class(&e), ("busy", true));
    }
}
