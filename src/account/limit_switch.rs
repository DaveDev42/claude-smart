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
use crate::orca::HostEnv;
use crate::orca::context::{Context, LOCK_WAIT};
use crate::orca::fsx::SwitchLock;
use crate::orca::http::{OauthHttp, SystemHttp};
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

/// The leader's second look, right before it switches: `decided` is the
/// active account [`hop_role`] saw, `now` the one Orca names after the
/// limit-pick fetch (which can take 30 s). When it moved (the user picked
/// an account in Orca's GUI meanwhile), [`hop_role`] decides again over the
/// new one, so a switch someone else made is followed, never overridden
/// (design §4). Pure.
pub fn recheck_role(
    decided: Option<&str>,
    now: Option<&str>,
    from_gen: u64,
    cur_gen: u64,
    from_account: Option<&str>,
    now_viable: bool,
) -> HopRole {
    if now == decided {
        return HopRole::Lead;
    }
    hop_role(from_gen, cur_gen, from_account, now, now_viable)
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
    // A hop only follows a definitive limit on `from`, so staying on it is
    // the worst answer. When usage names no other account with headroom
    // (nothing read, or only `from`'s own reading), leave for the next one in
    // Orca's order that no reading rules out; Stay only when there is none.
    let picked = data.map(|d| scoring::pick_best_gated(d, from.unwrap_or(""), false, false));
    if let Some(Ok(Some(id))) = &picked
        && known(id)
    {
        return Ok(id.clone());
    }
    let ids: Vec<&str> = accounts.accounts.iter().map(|a| a.id.as_str()).collect();
    let capped = |id: &str| data.and_then(|d| viable_in(d, id)) == Some(false);
    scoring::next_after(&ids, from, capped).ok_or_else(|| match picked {
        Some(Err(e)) => e.to_string(),
        Some(_) => "no other account with headroom".into(),
        None => "no usage data to pick another account".into(),
    })
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

/// The longest a limit leader holds `switch.lock`: its limit-pick fetch
/// ([`crate::usage::LIMIT_PICK_TIMEOUT`]), then the switch: over RPC
/// `selectClaude` ([`crate::orca::rpc::SELECT_TIMEOUT`]) after up to 15 s
/// of "switch already in progress" retries, or offline a few 10 s profile
/// and token calls, which [`LEADER_MARGIN`] covers.
pub const LEADER_LOCK_HOLD: Duration = Duration::from_secs(
    crate::usage::LIMIT_PICK_TIMEOUT.as_secs()
        + crate::orca::rpc::SELECT_TIMEOUT.as_secs()
        + 15
        + LEADER_MARGIN.as_secs(),
);

/// Slack for a leader's offline network calls (profile veto, refresh) and
/// its per-account usage fetches beyond the Orca list.
const LEADER_MARGIN: Duration = Duration::from_secs(60);

/// How long a hop waits for `switch.lock` in all. Its claude is already
/// stopped, so it has nothing better to do than wait. The bound covers the
/// longest holder csm has, an interactive `accounts add` login
/// ([`crate::orca::add::LOGIN_LOCK_HOLD`], which includes the redo when
/// Orca comes up mid-login), queued behind or ahead of one limit leader
/// ([`LEADER_LOCK_HOLD`]), plus a margin. Derived from its parts so a
/// longer timeout in either cannot leave a queued hop timing out as
/// [`HopOutcome::LockBusy`] (no relaunch).
pub const HOP_LOCK_WAIT: Duration = Duration::from_secs(
    crate::orca::add::LOGIN_LOCK_HOLD.as_secs() + LEADER_LOCK_HOLD.as_secs() + 30,
);

const _: () = assert!(
    HOP_LOCK_WAIT.as_secs()
        > crate::orca::add::LOGIN_LOCK_HOLD.as_secs() + LEADER_LOCK_HOLD.as_secs()
);

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
    let http = SystemHttp::from_env();
    let mut limit_pick = |lock: &SwitchLock, env: &HostEnv| {
        let (fresh, warnings) =
            crate::usage::capture_warnings(|| crate::usage::fetch_for_limit_pick(lock, env));
        (fresh.ok(), warnings)
    };
    let outcome = run_hop_in(
        &ctx,
        &procs,
        &http,
        &mut limit_pick,
        sentinel,
        own_sid,
        notice,
    );
    // The terminal line is gone with the terminal (an Orca pane, a closed
    // window): the log keeps why the hop switched or stayed.
    let _ = crate::hook::notify::append_log(own_sid, &hop_log_message(own_sid, &outcome));
    record_outcome(own_sid, &sentinel.reason, &outcome);
    outcome
}

/// Append the hop log's `switch` line for a hop that switched; every other
/// outcome leaves the log alone.
fn record_outcome(own_sid: &str, reason: &str, outcome: &HopOutcome) {
    if let HopOutcome::Switched { from, to, .. } = outcome {
        crate::hook::hops::record_switch(own_sid, reason, from.as_deref(), to);
    }
}

/// The `limit-switch.log` message for a hop's decision, in the hook's
/// `kind sid=… detail` shape. Pure.
pub fn hop_log_message(own_sid: &str, outcome: &HopOutcome) -> String {
    let sid = crate::hook::sid_short(own_sid);
    match outcome {
        HopOutcome::Switched { from, to, .. } => format!(
            "hop sid={sid} outcome=switched from={} to={to}",
            from.as_deref().unwrap_or("-")
        ),
        HopOutcome::Followed { to } => format!("hop sid={sid} outcome=followed to={to}"),
        HopOutcome::Stay { reason } => format!("hop sid={sid} outcome=stay reason={reason}"),
        HopOutcome::LockBusy => format!("hop sid={sid} outcome=lock-busy reason=switch.lock held"),
    }
}

/// The leader's fresh usage read for its pick: the data (`None` when the
/// fetch failed) and the collector's warnings. [`run_hop`] passes
/// [`crate::usage::fetch_for_limit_pick`]; tests pass a canned answer.
pub type LimitPickFetch<'a> =
    dyn FnMut(&SwitchLock, &HostEnv) -> (Option<UsageData>, Vec<String>) + 'a;

/// [`run_hop`] over an explicit context, process table, HTTP client and
/// limit-pick fetch.
fn run_hop_in(
    ctx: &Context,
    procs: &dyn ProcFacts,
    http: &dyn OauthHttp,
    limit_pick: &mut LimitPickFetch<'_>,
    sentinel: &RelaunchSentinel,
    own_sid: &str,
    notice: &mut dyn FnMut(&str),
) -> HopOutcome {
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
    let running = ctx.orca_running(procs);
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
    let (fresh, warnings) = limit_pick(&lock, &ctx.env);
    warnings.iter().for_each(|w| notice(w));
    let fresh = fresh.or(cached);
    let target = match choose_target(
        &accounts,
        fresh.as_ref(),
        &sentinel.target_account,
        from.as_deref(),
    ) {
        Ok(t) => t,
        Err(why) => return stay(why),
    };
    if let Err(why) = unsupervised_gate(ctx, procs) {
        return stay(why);
    }
    // switch.lock does not hold Orca's GUI back: look again at what Orca
    // names now, after the slow fetch, and follow a switch made meanwhile.
    if running {
        let now = AccountSet::load_live_with(&ctx.env);
        if now.from_orca {
            let now_active = now.active.clone().or_else(|| now.current.clone());
            let now_viable = now_active
                .as_deref()
                .and_then(|a| fresh.as_ref().and_then(|d| viable_in(d, a)))
                .unwrap_or(true);
            if let HopRole::Follow(to) = recheck_role(
                active.as_deref(),
                now_active.as_deref(),
                sentinel.from_gen,
                cur_gen,
                from.as_deref(),
                now_viable,
            ) {
                return HopOutcome::Followed { to };
            }
        }
    }
    let report = ctx.with_switch_env(procs, http, |env| switch::switch_held(env, &lock, &target));
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
    let _ =
        crate::usage::local::note_identity(&after.runtime_dir, after.current_uuid.as_deref(), now);
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

    /// A GUI switch made while the leader fetched usage is followed; an
    /// unchanged active account keeps the leader leading.
    #[test]
    fn the_leader_follows_a_gui_switch_made_during_its_fetch() {
        // Decided Lead on a (capped, from a); the user picked c meanwhile.
        assert_eq!(
            recheck_role(Some("a"), Some("c"), 2, 2, Some("a"), true),
            HopRole::Follow("c".into())
        );
        // Nothing moved: lead on.
        assert_eq!(
            recheck_role(Some("a"), Some("a"), 2, 2, Some("a"), false),
            HopRole::Lead
        );
        // The user picked another capped account: lead to the target.
        assert_eq!(
            recheck_role(Some("a"), Some("c"), 2, 2, Some("a"), false),
            HopRole::Lead
        );
        // Orca now names none: lead.
        assert_eq!(
            recheck_role(Some("a"), None, 2, 2, Some("a"), true),
            HopRole::Lead
        );
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
        // A pick that is not one of Orca's accounts never comes back.
        let only_a = accounts(&["a"]);
        let d = data(&[("a", 100, 20), ("x", 10, 10)]);
        assert!(choose_target(&only_a, Some(&d), "x", Some("a")).is_err());
    }

    /// No usage at all after a definitive limit: leave the capped account for
    /// the next one in Orca's order instead of staying on it.
    #[test]
    fn choose_target_leaves_a_capped_account_when_no_usage_is_known() {
        let set = accounts(&["a", "b", "c"]);
        assert_eq!(choose_target(&set, None, "gone", Some("b")), Ok("c".into()));
        assert_eq!(choose_target(&set, None, "gone", Some("c")), Ok("a".into()));
        // Only `from` has a reading (and it is capped): the others are unknown.
        let d = data(&[("a", 100, 20)]);
        assert_eq!(
            choose_target(&set, Some(&d), "gone", Some("a")),
            Ok("b".into())
        );
        // An account a reading rules out is skipped.
        let d = data(&[("a", 100, 20), ("b", 100, 20)]);
        assert_eq!(
            choose_target(&set, Some(&d), "gone", Some("a")),
            Ok("c".into())
        );
        // The capped account alone: still nowhere to go.
        let only_a = accounts(&["a"]);
        assert!(choose_target(&only_a, None, "gone", Some("a")).is_err());
    }

    #[test]
    fn hop_log_message_carries_the_decision_and_its_reason() {
        let sid = "0017654e-0152-421f-9c32-291d889c4603";
        let short = crate::hook::sid_short(sid);
        let stay = HopOutcome::Stay {
            reason: "no other account with headroom".into(),
        };
        assert_eq!(
            hop_log_message(sid, &stay),
            format!("hop sid={short} outcome=stay reason=no other account with headroom")
        );
        let sw = HopOutcome::Switched {
            from: Some("a".into()),
            to: "b".into(),
            generation: 2,
        };
        assert_eq!(
            hop_log_message(sid, &sw),
            format!("hop sid={short} outcome=switched from=a to=b")
        );
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

    /// The leader's second look: Orca names `a` when the hop starts, and the
    /// user picks `b` in Orca's GUI while the limit-pick fetch runs. The hop
    /// follows `b` and never asks Orca to switch.
    #[cfg(unix)]
    #[test]
    fn a_leader_follows_a_gui_switch_made_during_its_fetch() {
        use crate::orca::HostOs;
        use crate::orca::http::FakeHttp;
        use crate::orca::testsupport::{
            FakeOrca, FakeProcs, OrcaModel, model_handler, proc_info, record_json,
        };
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};

        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let model = Arc::new(Mutex::new(OrcaModel::default()));
        let lists = Arc::new(AtomicUsize::new(0));
        let fake = {
            let inner = model_handler(model.clone());
            let (model, lists) = (model.clone(), lists.clone());
            FakeOrca::start(move |req: &serde_json::Value| {
                // Every list after the first sees the GUI's pick.
                if req["method"] == "accounts.list" && lists.fetch_add(1, Ordering::SeqCst) >= 1 {
                    model.lock().unwrap().active = Some("id-b".into());
                }
                inner(req)
            })
        };
        {
            let ud = fake.user_data();
            let mut m = model.lock().unwrap();
            m.accounts = vec![
                record_json(ud, "id-a", "alice@example.com", None),
                record_json(ud, "id-b", "bob@example.com", None),
            ];
            m.active = Some("id-a".into());
        }
        let mut env = crate::orca::HostEnv::for_test(&home, HostOs::MacOs);
        env.orca_user_data_path = Some(fake.user_data().to_string_lossy().into_owned());
        env.claude_config_dir = Some(home.join("claude-d").to_string_lossy().into_owned());
        // Orca main is this process, as far as the liveness probe knows.
        let procs = FakeProcs::default().with(proc_info(
            std::process::id(),
            "Orca",
            Some("/Users/example/Applications/Orca.app/Contents/MacOS/Orca"),
            &[],
        ));
        let ctx = Context::from_env(env, &procs);
        assert!(ctx.orca_running(&procs));
        let sentinel = RelaunchSentinel {
            v: SENTINEL_V,
            session_id: "11111111-2222-3333-4444-555555555555".into(),
            target_account: "id-b".into(),
            from_account: Some("id-a".into()),
            from_gen: 0,
            reason: "limit:week_all".into(),
            at: 1,
            cwd: ".".into(),
            handoff: String::new(),
            hop: 1,
            born: 1,
            model_override: None,
        };
        let mut fetches = 0;
        let out = crate::testenv::with_test_home(&home, || {
            run_hop_in(
                &ctx,
                &procs,
                &FakeHttp::default(),
                &mut |_: &SwitchLock, _: &HostEnv| {
                    fetches += 1;
                    (None, Vec::new())
                },
                &sentinel,
                "11111111-2222-3333-4444-555555555555",
                &mut |_| {},
            )
        });
        assert_eq!(out, HopOutcome::Followed { to: "id-b".into() });
        assert_eq!(fetches, 1, "the leader fetched before its second look");
        let methods: Vec<String> = fake
            .requests()
            .iter()
            .map(|r| r["method"].as_str().unwrap_or("").to_owned())
            .collect();
        assert_eq!(methods, ["accounts.list", "accounts.list"]);
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

    #[test]
    fn only_a_switched_hop_appends_a_hop_log_line() {
        let home = tempfile::tempdir().unwrap();
        let log = || {
            let path = crate::paths::smart_dir_no_create().join("hops.jsonl");
            std::fs::read_to_string(path).unwrap_or_default()
        };
        crate::testenv::with_test_home(home.path(), || {
            let reason = "limit:week_all";
            record_outcome(
                "sid-1",
                reason,
                &HopOutcome::Followed {
                    to: "acct-b".into(),
                },
            );
            record_outcome(
                "sid-1",
                reason,
                &HopOutcome::Stay {
                    reason: "no viable account".into(),
                },
            );
            record_outcome("sid-1", reason, &HopOutcome::LockBusy);
            assert_eq!(log(), "");

            record_outcome(
                "sid-1",
                reason,
                &HopOutcome::Switched {
                    from: Some("acct-a".into()),
                    to: "acct-b".into(),
                    generation: 3,
                },
            );
            let text = log();
            let lines: Vec<&str> = text.lines().collect();
            assert_eq!(lines.len(), 1);
            let v: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
            assert_eq!(v["kind"], "switch");
            assert_eq!(v["sid"], "sid-1");
            assert_eq!(v["reason"], "week_all");
            assert_eq!(v["from_account"], "acct-a");
            assert_eq!(v["to_account"], "acct-b");
        });
    }
}
