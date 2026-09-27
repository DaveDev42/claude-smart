//! The supervisor's side of a limit switch (design §4 "Who switches").
//!
//! The hook never switches. It writes the sentinel and stops its child; the
//! supervisor that consumes the sentinel calls [`run_hop`], which takes
//! `switch.lock` and decides:
//!
//! - **Follow** when the switch journal's generation moved past the
//!   sentinel's `from_gen` (another leader switched after this hook fired),
//!   or when the active account already differs from the capped one and is
//!   viable (someone, possibly the user in Orca's GUI, switched). No switch;
//!   the session relaunches on the active account.
//! - **Lead** otherwise: re-check the hook's target with a limit-pick fetch
//!   (Orca's `accounts.list{refreshUsage:true}` when it runs, 30 s, then the
//!   cached values), re-pick when it is no longer viable, switch `D` to it
//!   through [`crate::orca::switch`] without releasing the lock, and write a
//!   follow file for every csm-supervised peer still on the capped account.
//!
//! A bare claude in `D` that csm does not supervise moves with the switch,
//! as it would on an Orca GUI switch, but only when `claude --version` is at
//! or above the configured floor (`csm config set min-claude-version`);
//! below it or unknown, the leader does not switch.
//!
//! Any failure is a [`HopOutcome::Stay`]: the caller relaunches once on the
//! same account (the hook already claimed `.switched`). The one exception is
//! a `switch.lock` another csm holds past [`HOP_LOCK_WAIT`]: that is
//! [`HopOutcome::LockBusy`], and the caller does not relaunch, because a
//! claude started in `D` then would race a switch whose plan assumed no
//! live claude (design §3 step 7).
//!
//! The decisions ([`hop_role`], [`choose_target`], [`unsupervised`]) are
//! pure and unit-tested; [`run_hop`] is the I/O shell.

use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use crate::account::AccountSet;
use crate::account::scoring;
use crate::launch_context::ConfigDirPin;
use crate::orca::context::{Context, LOCK_WAIT};
use crate::orca::fsx::SwitchLock;
use crate::orca::http::SystemHttp;
use crate::orca::live::{ProcFacts, SystemProcs};
use crate::orca::{runtime, switch};
use crate::platform::relaunch::{FollowFile, RelaunchSentinel, SENTINEL_V, write_follow};
use crate::usage::UsageData;

// ─── pure core ────────────────────────────────────────────────────────────────

/// Whether this supervisor leads the switch or follows one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HopRole {
    /// Relaunch on this (already active) account; no switch.
    Follow(String),
    Lead,
}

/// Pure: see the module doc. `active` is Orca's active account (RPC) or
/// `D`'s; `active_viable` is its viability (unknown counts as viable).
pub fn hop_role(
    from_gen: u64,
    cur_gen: u64,
    from_account: Option<&str>,
    active: Option<&str>,
    active_viable: bool,
) -> HopRole {
    let Some(active) = active.filter(|a| !a.is_empty()) else {
        return HopRole::Lead;
    };
    let moved = from_account != Some(active);
    if (cur_gen > from_gen && (moved || active_viable)) || (moved && active_viable) {
        return HopRole::Follow(active.to_owned());
    }
    HopRole::Lead
}

/// Is `id` viable per `data` ([`scoring::is_viable_pcts`], the one
/// predicate)? `None` when `data` has no usable reading for it.
pub fn viable_in(data: &UsageData, id: &str) -> Option<bool> {
    if data.errors.as_ref().is_some_and(|e| e.contains_key(id)) {
        return None;
    }
    let pu = data.profiles.get(id)?;
    let week = pu.week_all.as_ref().map(|s| s.pct);
    let session = pu
        .session
        .as_ref()
        .map(|s| s.pct)
        .unwrap_or(scoring::ABSENT_SESSION_PCT);
    if week.is_none() && pu.session.is_none() {
        return None;
    }
    Some(scoring::is_viable_pcts(
        session,
        week,
        pu.week_fable.as_ref().map(|s| s.pct),
    ))
}

/// Pure: the leader's target. The hook's pick stands when it is a known
/// account other than `from` and is not known to be capped; otherwise the
/// best viable account other than `from` ([`scoring::pick_best_gated`],
/// stale gate off: the session is already capped). `Err(why)` when there is
/// none.
pub fn choose_target(
    accounts: &AccountSet,
    data: Option<&UsageData>,
    hook_target: &str,
    from: Option<&str>,
) -> Result<String, String> {
    let known = |id: &str| accounts.contains(id) && Some(id) != from;
    if known(hook_target) && data.and_then(|d| viable_in(d, hook_target)) != Some(false) {
        return Ok(hook_target.to_owned());
    }
    let Some(data) = data else {
        return Err("no usage data to pick another account".into());
    };
    match scoring::pick_best_gated(data, from.unwrap_or(""), false, false) {
        Ok(Some(id)) if known(&id) => Ok(id),
        Ok(_) => Err("no other account with headroom".into()),
        Err(e) => Err(e.to_string()),
    }
}

/// Pure: the live claude pids in `D` that no csm supervisor owns.
pub fn unsupervised(live: &[u32], supervised: &HashSet<u32>) -> Vec<u32> {
    live.iter()
        .copied()
        .filter(|p| *p != 0 && !supervised.contains(p))
        .collect()
}

/// Pure: may the leader switch with an unsupervised claude live in `D`?
pub fn version_gate(version_output: Option<&str>, floor: &str) -> Result<(), String> {
    let Some(v) = version_output.and_then(crate::config::version_from_output) else {
        return Err("an unsupervised claude is live in D and its version is unknown".into());
    };
    match crate::config::version_at_least(&v, floor) {
        Some(true) => Ok(()),
        _ => Err(format!(
            "an unsupervised claude is live in D and claude {v} is below {floor}"
        )),
    }
}

// ─── I/O shell ────────────────────────────────────────────────────────────────

/// How long a hop waits for `switch.lock` in all. Its claude is already
/// stopped, so it has nothing better to do than wait; the bound covers the
/// longest holder csm has, an interactive `accounts add` login
/// ([`crate::orca::add::LOGIN_TIMEOUT`]), plus a limit leader's worst-case
/// fetch and offline switch.
pub const HOP_LOCK_WAIT: Duration = Duration::from_secs(300);

/// Take `switch.lock` for a hop: wait `first`, call `on_wait` once when that
/// runs out, then wait until `total` has passed. A `WouldBlock` error means
/// another process held the lock for all of `total`.
pub fn acquire_hop_lock(
    state: &Path,
    first: Duration,
    total: Duration,
    on_wait: &mut dyn FnMut(),
) -> std::io::Result<SwitchLock> {
    let first = first.min(total);
    match SwitchLock::acquire(state, first) {
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && total > first => {
            on_wait();
            SwitchLock::acquire(state, total - first)
        }
        r => r,
    }
}

/// What [`run_hop`] decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HopOutcome {
    /// Another switch already happened; relaunch on `to`.
    Followed { to: String },
    /// This supervisor switched `D` from `from` to `to`.
    Switched {
        from: Option<String>,
        to: String,
        generation: u64,
    },
    /// No switch; relaunch once on the same account.
    Stay { reason: String },
    /// Another csm held `switch.lock` for all of [`HOP_LOCK_WAIT`]: no
    /// switch, and no relaunch either while it may still be switching `D`.
    LockBusy,
}

/// The line [`run_hop`] hands its `notice` when its first wait for
/// `switch.lock` runs out and it keeps waiting.
pub const LOCK_WAIT_NOTICE: &str = "csm: waiting for another csm's account switch to finish";

/// Lead or follow the switch a sentinel asks for. Blocks up to
/// [`HOP_LOCK_WAIT`] for `switch.lock`, plus the limit-pick fetch. `pin` is
/// the launch's `CLAUDE_CONFIG_DIR` pin: the switch acts on the `D` the
/// child runs in, not the one this process inherited. Secondary lines (the
/// [`LOCK_WAIT_NOTICE`], the usage collector's warnings) go to `notice`,
/// never straight to stderr: the caller routes them (csm's log in an Orca
/// pane).
pub fn run_hop(
    sentinel: &RelaunchSentinel,
    own_sid: &str,
    pin: &ConfigDirPin,
    notice: &mut dyn FnMut(&str),
) -> HopOutcome {
    let procs = SystemProcs;
    let ctx = match Context::current_pinned(&procs, pin) {
        Ok(c) => c,
        Err(e) => return stay(format!("cannot resolve Orca's context: {e}")),
    };
    let lock = match acquire_hop_lock(&ctx.state, LOCK_WAIT, HOP_LOCK_WAIT, &mut || {
        notice(LOCK_WAIT_NOTICE)
    }) {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return HopOutcome::LockBusy,
        Err(e) => return stay(format!("cannot take switch.lock ({:?})", e.kind())),
    };
    let cur_gen = switch::read_journal(&ctx.state)
        .map(|j| j.generation)
        .unwrap_or(0);
    let running = ctx.orca_running(&procs);
    // Orca's live list when it runs: on Orca 1.4.214+ the store is an
    // export written at quit, so it lacks accounts added in this Orca
    // session and still names removed ones (which the RPC switch would then
    // refuse as missing, turning the hop into a Stay).
    let accounts = AccountSet::load_live_with(&ctx.env);
    let active = if running && accounts.from_orca {
        accounts.active.clone().or_else(|| accounts.current.clone())
    } else {
        accounts.current.clone()
    };
    let from = sentinel
        .from_account
        .clone()
        .filter(|a| !a.is_empty())
        .or_else(|| active.clone());
    // Cached readings only (no network, no Keychain, no CSM_USAGE_CMD):
    // this runs under switch.lock before the follower check, and peers wait
    // on that lock. The leader's real pick reads fresh values below.
    let cached = crate::usage::fetch_cached_with(Some(&ctx.env)).ok();
    let active_viable = active
        .as_deref()
        .and_then(|a| cached.as_ref().and_then(|d| viable_in(d, a)))
        .unwrap_or(true);
    if let HopRole::Follow(to) = hop_role(
        sentinel.from_gen,
        cur_gen,
        from.as_deref(),
        active.as_deref(),
        active_viable,
    ) {
        return HopOutcome::Followed { to };
    }

    // Leader.
    let (fresh, warnings) =
        crate::usage::capture_warnings(|| crate::usage::fetch_for_limit_pick(&lock, &ctx.env));
    warnings.iter().for_each(|w| notice(w));
    let fresh = fresh.ok().or(cached);
    let target = match choose_target(
        &accounts,
        fresh.as_ref(),
        &sentinel.target_account,
        from.as_deref(),
    ) {
        Ok(t) => t,
        Err(why) => return stay(why),
    };
    if let Err(why) = unsupervised_gate(&ctx, &procs) {
        return stay(why);
    }
    let http = SystemHttp::from_env();
    let report = ctx.with_switch_env(&procs, &http, |env| {
        switch::switch_held(env, &lock, &target)
    });
    let report = match report {
        Ok(r) => r,
        Err(e) => return stay(format!("the switch failed: {e}")),
    };
    if let switch::Outcome::Uncertain(why) = &report.outcome {
        return stay(format!(
            "the switch ended uncertain ({why}); run `csm accounts doctor --fix`"
        ));
    }
    let now = crate::epoch::now_secs() as i64;
    if let Some(from) = from.as_deref()
        && crate::hook::detect::follows_supported()
    {
        write_peer_follows(own_sid, from, &target, report.generation, now);
    }
    let _ = crate::hook::stop::stamp_last_switch();
    let after = AccountSet::load_with(&ctx.env);
    let _ = crate::usage::local::note_identity(after.current_uuid.as_deref(), now);
    drop(lock);
    HopOutcome::Switched {
        from,
        to: target,
        generation: report.generation,
    }
}

fn stay(reason: String) -> HopOutcome {
    HopOutcome::Stay { reason }
}

/// The pids csm supervises: every `<state>/<sid>.pid`.
fn supervised_pids(state: &Path) -> HashSet<u32> {
    let mut out = HashSet::new();
    let Ok(entries) = std::fs::read_dir(state) else {
        return out;
    };
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("pid") {
            continue;
        }
        if let Ok(Some((pid, _))) = crate::platform::pid::read_pid_file(&path) {
            out.insert(pid);
        }
    }
    out
}

/// The unsupervised-session precondition (design §4).
fn unsupervised_gate(ctx: &Context, facts: &dyn ProcFacts) -> Result<(), String> {
    let domain = runtime::this_pid_domain(ctx.os());
    let scan = runtime::scan_sessions(&ctx.paths.config_dir.join("sessions"), &domain, facts)
        .map_err(|e| format!("cannot scan D's sessions: {e}"))?;
    let mut live: Vec<u32> = scan.live.iter().map(|r| r.pid).collect();
    live.extend(scan.unverifiable.iter().map(|r| r.pid));
    if unsupervised(&live, &supervised_pids(&ctx.state)).is_empty() {
        return Ok(());
    }
    let floor = crate::config::Config::load()
        .unwrap_or_default()
        .min_claude_version()
        .to_owned();
    version_gate(claude_version().as_deref(), &floor)
}

/// `claude --version`'s stdout, with a 10 s cap. `None` on any failure.
/// The binary is the real `claude` ([`crate::config::real_claude_program`]:
/// never csm itself, never a configured drop-in launcher), unless
/// `CLAUDE_SMART_CLAUDE_BIN` names one.
fn claude_version() -> Option<String> {
    use std::io::Read as _;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let bin: std::ffi::OsString =
        match std::env::var_os("CLAUDE_SMART_CLAUDE_BIN").filter(|b| !b.is_empty()) {
            Some(b) => b,
            None => crate::config::real_claude_program().ok()?.into_os_string(),
        };
    let mut cmd = Command::new(bin);
    cmd.arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = crate::platform::child::own_group(&mut cmd).spawn().ok()?;
    let (tx, rx) = std::sync::mpsc::channel();
    let pipe = child.stdout.take();
    std::thread::spawn(move || {
        let mut out = String::new();
        let ok = pipe.is_some_and(|mut p| p.read_to_string(&mut out).is_ok());
        let _ = tx.send(ok.then_some(out));
    });
    let timeout = Duration::from_secs(10);
    let started = Instant::now();
    crate::platform::child::wait_deadline(&mut child, timeout, Duration::from_millis(50), true)
        .ok()??;
    let left = timeout
        .saturating_sub(started.elapsed())
        .max(crate::platform::child::REAP_LIMIT);
    let out = rx.recv_timeout(left).ok()??;
    Some(out)
}

/// Write `<state>/follow/<sid>.json` for every live csm-supervised peer
/// whose sidecar still names `from`. Best effort: a peer that misses its
/// follow file follows on its own 429 instead.
fn write_peer_follows(own_sid: &str, from: &str, to: &str, generation: u64, now: i64) {
    let state = crate::paths::smart_dir_no_create();
    let Ok(entries) = std::fs::read_dir(&state) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("pid") {
            continue;
        }
        let Some(sid) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if sid == own_sid || !crate::session::alias::looks_like_uuid(sid) {
            continue;
        }
        let Ok(Some((pid, _))) = crate::platform::pid::read_pid_file(&path) else {
            continue;
        };
        if !crate::hook::stop::check_is_live_claude_or_node(pid) {
            continue;
        }
        let on_from = crate::sidecar::read_sidecar(&crate::paths::sidecar(sid))
            .ok()
            .and_then(|s| s.account_id)
            .is_some_and(|a| a == from);
        if !on_from {
            continue;
        }
        let _ = write_follow(
            &crate::paths::follow(sid),
            &FollowFile {
                v: SENTINEL_V,
                generation,
                to_account: to.to_owned(),
                at: now,
            },
        );
    }
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::accounts::AccountEntry;
    use crate::usage::model::{ProfileUsage, UsageSection};

    fn section(pct: i64) -> Option<UsageSection> {
        Some(UsageSection {
            pct,
            resets: None,
            resets_at: None,
        })
    }

    fn data(rows: &[(&str, i64, i64)]) -> UsageData {
        let mut d = UsageData::default();
        for (id, session, week) in rows {
            d.profiles.insert(
                (*id).to_owned(),
                ProfileUsage {
                    session: section(*session),
                    week_all: section(*week),
                    ..Default::default()
                },
            );
        }
        d
    }

    fn accounts(ids: &[&str]) -> AccountSet {
        AccountSet {
            accounts: ids
                .iter()
                .map(|id| AccountEntry {
                    id: (*id).to_owned(),
                    email: None,
                    organization_name: None,
                    managed_auth_path: None,
                })
                .collect(),
            ..AccountSet::default()
        }
    }

    #[test]
    fn hop_role_follows_a_newer_generation() {
        assert_eq!(
            hop_role(2, 3, Some("a"), Some("b"), false),
            HopRole::Follow("b".into())
        );
        // Same account after a newer switch, still viable: follow.
        assert_eq!(
            hop_role(2, 3, Some("a"), Some("a"), true),
            HopRole::Follow("a".into())
        );
        // Same capped account after a newer switch: lead, never loop on it.
        assert_eq!(hop_role(2, 3, Some("a"), Some("a"), false), HopRole::Lead);
    }

    #[test]
    fn hop_role_follows_a_gui_switch_only_onto_a_viable_account() {
        assert_eq!(
            hop_role(3, 3, Some("a"), Some("b"), true),
            HopRole::Follow("b".into())
        );
        assert_eq!(hop_role(3, 3, Some("a"), Some("b"), false), HopRole::Lead);
        assert_eq!(hop_role(3, 3, Some("a"), Some("a"), true), HopRole::Lead);
        assert_eq!(hop_role(3, 3, Some("a"), None, true), HopRole::Lead);
        assert_eq!(hop_role(3, 3, Some("a"), Some(""), true), HopRole::Lead);
    }

    #[test]
    fn viable_in_uses_the_single_predicate() {
        let d = data(&[("a", 10, 20), ("b", 100, 20), ("c", 10, 99)]);
        assert_eq!(viable_in(&d, "a"), Some(true));
        assert_eq!(viable_in(&d, "b"), Some(false));
        assert_eq!(viable_in(&d, "c"), Some(false));
        assert_eq!(viable_in(&d, "zz"), None);
    }

    #[test]
    fn choose_target_keeps_a_viable_hook_pick() {
        let set = accounts(&["a", "b", "c"]);
        let d = data(&[("a", 100, 20), ("b", 10, 20), ("c", 10, 10)]);
        assert_eq!(
            choose_target(&set, Some(&d), "b", Some("a")),
            Ok("b".into())
        );
        // No data at all: a known pick still stands.
        assert_eq!(choose_target(&set, None, "b", Some("a")), Ok("b".into()));
    }

    #[test]
    fn choose_target_repicks_when_the_hook_pick_is_capped_or_unknown() {
        let set = accounts(&["a", "b", "c"]);
        let d = data(&[("a", 100, 20), ("b", 100, 20), ("c", 10, 10)]);
        assert_eq!(
            choose_target(&set, Some(&d), "b", Some("a")),
            Ok("c".into())
        );
        assert_eq!(
            choose_target(&set, Some(&d), "gone", Some("a")),
            Ok("c".into())
        );
        // The hook's pick is the capped account itself.
        assert_eq!(
            choose_target(&set, Some(&d), "a", Some("a")),
            Ok("c".into())
        );
    }

    #[test]
    fn choose_target_refuses_when_nothing_has_headroom() {
        let set = accounts(&["a", "b"]);
        let d = data(&[("a", 100, 20), ("b", 100, 20)]);
        assert!(choose_target(&set, Some(&d), "b", Some("a")).is_err());
        assert!(choose_target(&set, None, "gone", Some("a")).is_err());
        // A pick that is not one of Orca's accounts never comes back.
        let only_a = accounts(&["a"]);
        let d = data(&[("a", 100, 20), ("x", 10, 10)]);
        assert!(choose_target(&only_a, Some(&d), "x", Some("a")).is_err());
    }

    #[test]
    fn unsupervised_excludes_csm_pids() {
        let sup: HashSet<u32> = [10, 11].into_iter().collect();
        assert_eq!(unsupervised(&[10, 11, 12, 0], &sup), vec![12]);
        assert!(unsupervised(&[10], &sup).is_empty());
    }

    #[test]
    fn version_gate_needs_a_known_version_at_the_floor() {
        assert!(version_gate(Some("2.1.283 (Claude Code)"), "2.1.283").is_ok());
        assert!(version_gate(Some("2.2.0 (Claude Code)"), "2.1.283").is_ok());
        assert!(version_gate(Some("2.1.270 (Claude Code)"), "2.1.283").is_err());
        assert!(version_gate(Some("garbage"), "2.1.283").is_err());
        assert!(version_gate(None, "2.1.283").is_err());
    }

    /// Under `cfg(test)` the context resolves only inside the test home, so a
    /// hop run there can never reach the real state dir or Orca's store: it
    /// stays, with nothing to switch to.
    #[test]
    fn run_hop_in_an_empty_test_home_stays() {
        let home = tempfile::TempDir::new().unwrap();
        let sentinel = RelaunchSentinel {
            v: SENTINEL_V,
            session_id: "11111111-2222-3333-4444-555555555555".into(),
            target_account: "b".into(),
            from_account: Some("a".into()),
            from_gen: 0,
            reason: "limit:week_all".into(),
            at: 1,
            cwd: ".".into(),
            handoff: String::new(),
            hop: 1,
            born: 1,
            model_override: None,
        };
        let out = crate::testenv::with_test_home(home.path(), || {
            run_hop(
                &sentinel,
                "11111111-2222-3333-4444-555555555555",
                &ConfigDirPin::Leave,
                &mut |_| {},
            )
        });
        assert!(matches!(out, HopOutcome::Stay { .. }), "{out:?}");
    }

    /// A peer whose first wait for `switch.lock` runs out keeps waiting (it
    /// says so once) instead of giving up and relaunching into a `D` another
    /// csm may be switching; it takes the lock once the holder lets go.
    #[test]
    fn a_hop_keeps_waiting_for_a_held_switch_lock() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().to_path_buf();
        let held = SwitchLock::acquire(&state, Duration::from_secs(1)).unwrap();
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let releaser = std::thread::spawn(move || {
            // Let go once the waiter has said it is waiting.
            rx.recv_timeout(Duration::from_secs(10)).unwrap();
            std::thread::sleep(Duration::from_millis(50));
            drop(held);
        });
        let mut notices = 0;
        let got = acquire_hop_lock(
            &state,
            Duration::from_millis(30),
            Duration::from_secs(10),
            &mut || {
                notices += 1;
                let _ = tx.send(());
            },
        );
        releaser.join().unwrap();
        assert!(got.is_ok(), "{:?}", got.err());
        assert_eq!(notices, 1);
    }

    /// Held for the whole budget: `WouldBlock`, which `run_hop` turns into
    /// [`HopOutcome::LockBusy`] (no relaunch), never a `Stay`.
    #[test]
    fn a_hop_lock_held_past_the_budget_is_would_block() {
        let dir = tempfile::tempdir().unwrap();
        let held = SwitchLock::acquire(dir.path(), Duration::from_secs(1)).unwrap();
        let mut notices = 0;
        let got = acquire_hop_lock(
            dir.path(),
            Duration::from_millis(20),
            Duration::from_millis(60),
            &mut || notices += 1,
        );
        assert_eq!(
            got.err().map(|e| e.kind()),
            Some(std::io::ErrorKind::WouldBlock)
        );
        assert_eq!(notices, 1);
        drop(held);
    }
}
