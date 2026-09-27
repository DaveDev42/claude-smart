//! Switching `D` to another Orca account (design section 3).
//!
//! Pure core:
//! - [`plan_switch`]: preconditions and the route: [`Plan::Rpc`] when Orca
//!   runs (and its `D` is csm's), [`Plan::Offline`] with the numbered steps
//!   when it does not, [`Plan::Noop`] when `D` already holds the target and
//!   the store names it, [`Plan::Refuse`] otherwise;
//! - [`classify_rpc_outcome`] (S5): an RPC switch succeeded only when the
//!   re-read state names the target as active and `D`'s identity is the
//!   target's; anything else is a failure, whatever the call returned.
//!
//! Shell ([`switch`]), under `<state>/switch.lock`:
//! - Orca running: `accounts.selectClaude`, retried with backoff while Orca
//!   answers "switch already in progress" (up to 15 s), then the S5
//!   re-read through `accounts.list{refreshUsage:false}` (the store when the
//!   socket does not answer) and `D`'s `oauthAccount`. csm writes nothing of
//!   Orca's.
//! - Orca stopped: the offline port, each step journaled in
//!   `<state>/switch.json` (`{gen, account, from, to, step, owner{pid,
//!   born}}`): read-back (only when an account was last synced), load the
//!   target, the system-default snapshot (active id null), the refresh (no
//!   live claude), materialize (neutral window, or Orca's order with a live
//!   claude), the store patch through the store-write protocol, verify,
//!   commit `gen + 1`. A failure after materialize restores `D`'s
//!   pre-images; Orca appearing at L0/L1 restores them too and redoes the
//!   select over RPC; Orca appearing at L2 leaves `D` as written and
//!   redoes over RPC.
//!
//! Recovery ([`recover`]): a journal left pending by a dead owner (csm holds
//! the lock, so the owner is gone) is repaired by re-running the switch to
//! the account the store names (the `a == i` repair). If the repair fails,
//! `D` is made neutral (no `oauthAccount`). With Orca running:
//! - a switch away from the system default that may have written `D` is
//!   not repaired: csm never writes `D` behind a running Orca (Invariant 6),
//!   so it files the system-default snapshot's grants in the quarantine
//!   (Orca's next select would capture the half-written `D` over the
//!   snapshot, the only copy of the user's own login), refuses a switch,
//!   and keeps the journal pending for the offline repair once Orca stops
//!   ([`Recovery::Deferred`]);
//! - otherwise the intent is cleared only when Orca's `D` is csm's, since
//!   only then does Orca's own sync own it. With another or an unknown `D`
//!   the journal stays pending for the offline repair once Orca stops.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::fsx::{self, SwitchLock, WriteOpts};
use super::http::OauthHttp;
use super::keychain::{self, KeychainUser};
use super::live::{LiveMark, Liveness, ProcFacts};
use super::quarantine::Quarantine;
use super::readback::{self, ReadBack, ReadBackReport};
use super::record::{AccountRecord, RuntimeTarget, p3};
use super::refresh::{self, StashRefresh};
use super::rpc;
use super::runtime::{
    self, Order, RuntimeIdentity, RuntimePaths, RuntimeTargetDir, read_runtime_identity,
};
use super::stash::{self, Stash};
use super::store::{self, Patch, RedoOp, RedoOpts, RedoOutcome, StoreView, StoreWrite};
use super::sysdefault::{self, Captured};
use super::userdata::{DataFileChoice, HostOs};
use super::{OrcaError, SecretString};

// ─── plan ─────────────────────────────────────────────────────────────────────

/// The target account as the preconditions see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetState {
    Ok,
    Missing,
    /// A WSL account (read-only in csm).
    NotHost,
    /// Q2i failed or the stash holds no valid grant.
    Unusable(String),
}

/// Everything [`plan_switch`] decides on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchState {
    pub target_id: String,
    pub target: TargetState,
    pub orca_running: bool,
    /// Orca main's `D` equals csm's (`None`: unknown). Only read when Orca
    /// runs.
    pub orca_dir_agrees: Option<bool>,
    /// The offline gates (version, store access, round-trip, schema):
    /// `Err(why)` refuses an offline switch.
    pub offline_allowed: Result<(), String>,
    /// The host active id the store names (a fresh Orca's last-synced one).
    pub active: Option<String>,
    /// `D`'s credentials and identity already are the target's.
    pub d_holds_target: bool,
    /// `switch.json` names an unfinished switch.
    pub journal_pending: bool,
    /// A live claude is registered in `D` (fail safe: unknown counts).
    pub live_claude: bool,
}

/// One offline step (design section 3's numbering in brackets).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// A stale journal: its switch is subsumed by this one once this one
    /// commits; a failure puts it back pending.
    Recover,
    /// [3]
    ReadBack,
    /// [4]
    LoadTarget,
    /// [5]
    CaptureSnapshot,
    /// [6]
    Refresh,
    /// [7]
    Materialize(Order),
    /// [8]
    Store,
    /// [9]
    Verify,
}

/// The route of a switch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    Rpc,
    Offline(Vec<Step>),
    Noop,
    Refuse(String),
}

/// Decide the route. Pure.
pub fn plan_switch(s: &SwitchState) -> Plan {
    match &s.target {
        TargetState::Ok => {}
        TargetState::Missing => {
            return Plan::Refuse(format!("no Claude account {}", s.target_id));
        }
        TargetState::NotHost => {
            return Plan::Refuse("WSL accounts are read-only in csm".into());
        }
        TargetState::Unusable(why) => {
            return Plan::Refuse(format!("the account's stash is unusable: {why}"));
        }
    }
    if s.orca_running {
        return match s.orca_dir_agrees {
            Some(true) => Plan::Rpc,
            Some(false) => Plan::Refuse(
                "Orca runs with a different CLAUDE_CONFIG_DIR than csm; switch in Orca".into(),
            ),
            None => Plan::Refuse(
                "cannot read Orca's CLAUDE_CONFIG_DIR, so csm cannot tell that Orca and csm share D"
                    .into(),
            ),
        };
    }
    if let Err(why) = &s.offline_allowed {
        return Plan::Refuse(format!("csm cannot write Orca's state offline: {why}"));
    }
    let repair = s.active.as_deref() == Some(s.target_id.as_str());
    if repair && s.d_holds_target && !s.journal_pending {
        return Plan::Noop;
    }
    let mut steps = Vec::new();
    if s.journal_pending {
        steps.push(Step::Recover);
    }
    if s.active.is_some() {
        steps.push(Step::ReadBack);
    }
    steps.push(Step::LoadTarget);
    if !repair && s.active.is_none() {
        steps.push(Step::CaptureSnapshot);
    }
    // Orca's sync refreshes the grant it is about to materialize whenever no
    // live Claude PTY exists, whether or not the account changed
    // (runtime-auth-sync.ts), so the repair refreshes too.
    if !s.live_claude {
        steps.push(Step::Refresh);
    }
    steps.push(Step::Materialize(if s.live_claude {
        Order::OrcaOrder
    } else {
        Order::NeutralWindow
    }));
    steps.push(Step::Store);
    steps.push(Step::Verify);
    Plan::Offline(steps)
}

// ─── RPC outcome ──────────────────────────────────────────────────────────────

/// S5's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RpcVerdict {
    Success,
    Failed(String),
}

/// Classify an RPC switch from the re-read state. `call_error` is the RPC's
/// error text, if it failed; the state decides either way. Pure.
pub fn classify_rpc_outcome(
    call_error: Option<&str>,
    observed_active: Option<&str>,
    d_uuid: Option<&str>,
    target: &str,
    target_uuid: Option<&str>,
) -> RpcVerdict {
    let why = |s: String| match call_error {
        Some(e) => RpcVerdict::Failed(format!("{s} (Orca said: {e})")),
        None => RpcVerdict::Failed(s),
    };
    if observed_active != Some(target) {
        return why(match observed_active {
            Some(a) => format!("Orca's active account is {a}, not {target}"),
            None => format!("Orca has no active account, not {target}"),
        });
    }
    if d_uuid != target_uuid {
        return why("D's identity is not the target account's".into());
    }
    RpcVerdict::Success
}

// ─── journal ──────────────────────────────────────────────────────────────────

/// The journal file.
pub const JOURNAL_FILE: &str = "switch.json";

/// The csm process that wrote the journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Owner {
    pub pid: u32,
    /// Process start time, epoch seconds (the pid-reuse guard).
    pub born: Option<u64>,
}

impl Owner {
    /// This process.
    pub fn current(facts: &dyn ProcFacts) -> Owner {
        let pid = std::process::id();
        Owner {
            pid,
            born: facts.start_time(pid),
        }
    }
}

/// The last step a switch finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JournalStep {
    Started,
    ReadBack,
    LoadTarget,
    CaptureSnapshot,
    Refresh,
    Materialize,
    Store,
    /// Done; `account` is `to`.
    Committed,
    /// Failed with `D` restored to its pre-images: nothing to repair.
    RolledBack,
    /// Orca came up and owns the outcome (the redo went over RPC).
    HandedToOrca,
}

/// `<state>/switch.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Journal {
    /// Bumped on every committed switch.
    #[serde(rename = "gen")]
    pub generation: u64,
    /// The account csm last confirmed active.
    pub account: Option<String>,
    pub from: Option<String>,
    pub to: String,
    pub step: JournalStep,
    /// A switch handed to a running Orca left `D` unsettled: its
    /// pre-images could not be put back, or an earlier crash's state came
    /// back, and Orca did not confirm a select that rewrote `D`. Orca then
    /// believes `D` is in sync, so while it runs the journal waits for the
    /// offline repair instead of being cleared for Orca's sync.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub orca_unconfirmed: bool,
    pub owner: Owner,
}

impl Journal {
    /// An unfinished switch: `D` may be half written.
    pub fn pending(&self) -> bool {
        !matches!(
            self.step,
            JournalStep::Committed | JournalStep::RolledBack | JournalStep::HandedToOrca
        )
    }
}

pub fn journal_path(state: &Path) -> PathBuf {
    state.join(JOURNAL_FILE)
}

/// The journal, `None` when absent or unreadable.
pub fn read_journal(state: &Path) -> Option<Journal> {
    let b = super::read_capped_bytes(&journal_path(state), 64 * 1024).ok()??;
    serde_json::from_slice(&b).ok()
}

fn write_journal(state: &Path, j: &Journal) -> Result<(), OrcaError> {
    fsx::create_dir_all(state, 0o700).map_err(|e| OrcaError::io("cannot create", state, e))?;
    let p = journal_path(state);
    let text = serde_json::to_vec_pretty(j).map_err(|e| OrcaError::Invalid(e.to_string()))?;
    fsx::write_atomic(&p, &text, WriteOpts::PRIVATE_DURABLE)
        .map_err(|e| OrcaError::io("cannot write", &p, e))
}

// ─── environment ──────────────────────────────────────────────────────────────

/// Timing knobs (tests shrink them).
#[derive(Debug, Clone, Copy)]
pub struct SwitchTiming {
    pub lock_wait: Duration,
    pub select_timeout: Duration,
    /// How long to retry "switch already in progress".
    pub busy_wait: Duration,
    pub busy_poll: Duration,
    pub redo: RedoOpts,
}

impl Default for SwitchTiming {
    fn default() -> Self {
        SwitchTiming {
            lock_wait: Duration::from_secs(30),
            select_timeout: rpc::SELECT_TIMEOUT,
            busy_wait: Duration::from_secs(15),
            busy_poll: Duration::from_millis(250),
            redo: RedoOpts::default(),
        }
    }
}

/// Everything a switch touches, injected.
pub struct SwitchEnv<'a> {
    pub os: HostOs,
    pub user_data: &'a Path,
    pub data_file: &'a DataFileChoice,
    /// csm's state dir.
    pub state: &'a Path,
    pub paths: &'a RuntimePaths,
    pub keychain_user: &'a KeychainUser,
    pub live: &'a dyn Liveness,
    pub http: &'a dyn OauthHttp,
    /// Is a live claude registered in `D`? (fail safe: unknown is true)
    pub live_claude: &'a dyn Fn() -> bool,
    /// Does Orca main's `D` equal csm's? Called only when Orca runs.
    pub orca_dir_agrees: &'a dyn Fn() -> Option<bool>,
    /// The Orca version is in csm's tested range.
    pub version_ok: bool,
    /// userData may be written (not a WSL view of Windows Orca).
    pub store_access_allowed: bool,
    pub owner: Owner,
    pub timing: SwitchTiming,
}

impl SwitchEnv<'_> {
    fn quarantine(&self) -> Quarantine {
        Quarantine::new(self.os, self.state)
    }

    fn target_dir(&self) -> RuntimeTargetDir<'_> {
        RuntimeTargetDir {
            os: self.os,
            paths: self.paths,
            user: self.keychain_user,
        }
    }
}

/// How a switch ended.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// `D` and Orca's state name the target.
    Switched,
    /// Already the target; nothing was written.
    AlreadyActive,
    /// Orca appeared mid-write and the redo over RPC gave no certain answer;
    /// `accounts doctor` reconciles.
    Uncertain(String),
}

/// Which route ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Rpc,
    Offline,
    /// Offline, then Orca appeared and the redo went over RPC.
    OfflineThenRpc,
    Noop,
}

/// What [`switch`] did.
#[derive(Debug)]
pub struct SwitchReport {
    pub route: Route,
    pub outcome: Outcome,
    pub to: String,
    pub generation: u64,
    pub steps: Vec<Step>,
    pub readback: Option<ReadBackReport>,
    pub snapshot: Option<Captured>,
    /// The refresh result: `None` when not attempted.
    pub refresh: Option<String>,
    pub redo: Option<RedoOutcome>,
}

// ─── shell helpers ────────────────────────────────────────────────────────────

/// The target's stash, its grant and its `oauth-account.json`.
struct Loaded {
    stash: Stash,
    creds: SecretString,
    oauth: Option<Value>,
}

fn load_account(env: &SwitchEnv<'_>, rec: &AccountRecord) -> Result<Loaded, String> {
    if !rec.is_host() {
        return Err("a WSL account".into());
    }
    let stash = Stash::open(env.user_data, &rec.id, rec.managed_auth_path.as_deref())
        .map_err(|e| e.to_string())?;
    let creds = stash
        .credentials(env.os)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "no stashed credentials".to_owned())?;
    if !stash::credentials_are_valid(creds.expose()) {
        return Err("the stashed credentials have no access token".into());
    }
    // A `null` identity is none: materialize then deletes the runtime key,
    // as Orca's writeRuntimeOauthAccount(null) does.
    let oauth = stash
        .oauth_account()
        .ok()
        .flatten()
        .filter(|v| !v.is_null());
    Ok(Loaded {
        stash,
        creds,
        oauth,
    })
}

/// The last-synced account's stashed grant as stored, valid or not (Orca's
/// read-back exclusion `e`). `None` when there is none or it cannot be read.
fn raw_stash_credentials(env: &SwitchEnv<'_>, rec: &AccountRecord) -> Option<SecretString> {
    if !rec.is_host() {
        return None;
    }
    Stash::open(env.user_data, &rec.id, rec.managed_auth_path.as_deref())
        .ok()?
        .credentials(env.os)
        .ok()
        .flatten()
}

fn target_state(env: &SwitchEnv<'_>, rec: Option<&AccountRecord>) -> TargetState {
    match rec {
        None => TargetState::Missing,
        Some(r) if !r.is_host() => TargetState::NotHost,
        Some(r) => match load_account(env, r) {
            Ok(_) => TargetState::Ok,
            Err(e) => TargetState::Unusable(e),
        },
    }
}

fn uuid_of(oauth: Option<&Value>) -> Option<String> {
    oauth
        .map(runtime::OauthIdentity::from_value)
        .and_then(|i| i.account_uuid)
}

fn d_uuid(paths: &RuntimePaths) -> Option<String> {
    match read_runtime_identity(paths) {
        RuntimeIdentity::Present(i) => i.account_uuid,
        _ => None,
    }
}

/// Step 9: `D`'s identity and grant are the target's. `Err(why)`.
fn verify(env: &SwitchEnv<'_>, t: &Loaded) -> Result<(), String> {
    if d_uuid(env.paths) != uuid_of(t.oauth.as_ref()) {
        return Err("D's oauthAccount is not the target's".into());
    }
    let file = super::read_capped_bytes(&env.paths.credentials_path, 1024 * 1024)
        .ok()
        .flatten();
    let same = file.as_deref() == Some(t.creds.expose().as_bytes());
    if let Some(mut f) = file {
        super::zero(&mut f);
    }
    if !same {
        return Err("D/.credentials.json is not the target's grant".into());
    }
    if env.os == HostOs::MacOs {
        let dir = env.paths.config_dir.to_string_lossy().into_owned();
        let item = keychain::read_runtime_scoped(Some(&dir), env.keychain_user)
            .map_err(|e| e.to_string())?;
        if item.as_ref().map(|s| s.expose()) != Some(t.creds.expose().trim()) {
            return Err("the scoped Keychain item is not the target's grant".into());
        }
    }
    Ok(())
}

fn d_holds(env: &SwitchEnv<'_>, t: &Loaded) -> bool {
    verify(env, t).is_ok()
}

/// The host's active account id when the store names one that has a record;
/// a dangling id counts as none, the way Orca's own lookup
/// (`getActiveAccount`) treats it. Pure.
fn named_active(view: &StoreView) -> Option<String> {
    view.active_host_id()
        .filter(|id| view.account(id).is_some())
        .map(str::to_owned)
}

fn load_view(choice: &DataFileChoice) -> Result<Option<StoreView>, OrcaError> {
    match store::load_choice(choice)? {
        None => Ok(None),
        Some(f) => StoreView::from_bytes(&f.bytes)
            .map(Some)
            .map_err(|e| OrcaError::Refused(e.to_string())),
    }
}

// ─── switch ───────────────────────────────────────────────────────────────────

/// Switch `D` to account `target`.
pub fn switch(env: &SwitchEnv<'_>, target: &str) -> Result<SwitchReport, OrcaError> {
    let _lock = SwitchLock::acquire(env.state, env.timing.lock_wait)
        .map_err(|e| OrcaError::io("cannot take", &env.state.join(fsx::SWITCH_LOCK), e))?;
    switch_locked(env, target)
}

/// [`switch`] for a caller that already holds `switch.lock` (the limit-switch
/// leader decides under the lock, then switches without releasing it). The
/// borrow proves the lock is held; `SwitchLock` is per open file, so taking
/// it a second time in the same process would block.
pub fn switch_held(
    env: &SwitchEnv<'_>,
    _held: &fsx::SwitchLock,
    target: &str,
) -> Result<SwitchReport, OrcaError> {
    switch_locked(env, target)
}

fn switch_locked(env: &SwitchEnv<'_>, target: &str) -> Result<SwitchReport, OrcaError> {
    let journal = read_journal(env.state);
    let l0 = env.live.mark();
    if l0.running {
        return switch_running(env, target, journal);
    }
    let view = load_view(env.data_file)?;
    let offline_allowed = if !env.version_ok {
        Err("the installed Orca version is not one csm was tested with".to_owned())
    } else if !env.store_access_allowed {
        Err("this userData is not csm's to write".to_owned())
    } else if env.data_file.has_state_db() {
        Err(store::SQLITE_REFUSAL.to_owned())
    } else {
        match &view {
            None => Err("no Orca store".to_owned()),
            Some(v) => v.writable().map_err(|e| e.to_string()),
        }
    };
    let rec = view.as_ref().and_then(|v| v.account(target)).cloned();
    let active = view.as_ref().and_then(named_active);
    // A pending journal with no active account in the store: settle it
    // first (a switch from the system default puts the snapshot back), so
    // this switch's snapshot captures the system default, not the half
    // written `D` a crash left.
    let journal = match journal {
        Some(mut j) if j.pending() && active.is_none() && offline_allowed.is_ok() => {
            settle_without_active(env, &mut j)?;
            Some(j)
        }
        other => other,
    };
    let loaded = rec.as_ref().and_then(|r| load_account(env, r).ok());
    let live_claude = (env.live_claude)();
    let state = SwitchState {
        target_id: target.to_owned(),
        target: target_state(env, rec.as_ref()),
        orca_running: false,
        orca_dir_agrees: None,
        offline_allowed,
        active: active.clone(),
        d_holds_target: loaded.as_ref().is_some_and(|t| d_holds(env, t)),
        journal_pending: journal.as_ref().is_some_and(Journal::pending),
        live_claude,
    };
    match plan_switch(&state) {
        Plan::Refuse(why) => Err(OrcaError::Refused(why)),
        Plan::Rpc => unreachable!("Orca is stopped"),
        Plan::Noop => Ok(SwitchReport {
            route: Route::Noop,
            outcome: Outcome::AlreadyActive,
            to: target.to_owned(),
            generation: journal.map(|j| j.generation).unwrap_or(0),
            steps: Vec::new(),
            readback: None,
            snapshot: None,
            refresh: None,
            redo: None,
        }),
        Plan::Offline(steps) => {
            let view = view.expect("offline_allowed implies a store");
            let rec = rec.expect("target state Ok implies a record");
            run_offline(env, &l0, &view, &rec, active, journal, steps)
        }
    }
}

fn switch_running(
    env: &SwitchEnv<'_>,
    target: &str,
    journal: Option<Journal>,
) -> Result<SwitchReport, OrcaError> {
    let null_crash = journal
        .as_ref()
        .filter(|j| j.pending() && j.from.is_none() && null_switch_touched_d(j.step));
    let snap = match rpc::accounts_list(env.user_data, false, rpc::LIST_TIMEOUT) {
        Ok(s) => s,
        Err(e) => {
            // Keep the snapshot even without Orca's answer (see below).
            if let Some(j) = null_crash {
                keep_null_snapshot(env, j)?;
            }
            return Err(e.into());
        }
    };
    // A crashed switch away from the system default may have left the
    // target's grant in `D`. The select below would make Orca capture that
    // `D` as the system default, over the snapshot that holds the user's
    // own login, and csm never writes `D` behind a running Orca (Invariant
    // 6; a GUI select could interleave with such a write). So csm files the
    // snapshot's grants in the quarantine, refuses, and leaves the journal
    // pending for the offline repair once Orca stops. Once Orca names an
    // account, csm cannot tell what it captured, so it refuses the same way.
    if let Some(j) = null_crash {
        return Err(OrcaError::Refused(
            match snap.claude.active_by_runtime.host.as_deref() {
                Some(a) => orca_selected_since_crash(j, a),
                None => defer_null_crash(env, j)?,
            },
        ));
    }
    let rec = snap
        .claude
        .accounts
        .iter()
        .find(|a| a.id == target)
        .cloned();
    let from = snap.claude.active_by_runtime.host.clone();
    let loaded = rec.as_ref().and_then(|r| load_account(env, r).ok());
    let state = SwitchState {
        target_id: target.to_owned(),
        target: target_state(env, rec.as_ref()),
        orca_running: true,
        orca_dir_agrees: (env.orca_dir_agrees)(),
        offline_allowed: Err("Orca is running".into()),
        active: from.clone(),
        d_holds_target: false,
        journal_pending: journal.as_ref().is_some_and(Journal::pending),
        live_claude: (env.live_claude)(),
    };
    match plan_switch(&state) {
        Plan::Rpc => {}
        Plan::Refuse(why) => return Err(OrcaError::Refused(why)),
        other => unreachable!("Orca runs: {other:?}"),
    }
    let target_uuid = loaded.as_ref().and_then(|t| uuid_of(t.oauth.as_ref()));
    let call_error = select_with_backoff(env, target)
        .err()
        .map(|e| e.to_string());
    let observed = match rpc::accounts_list(env.user_data, false, rpc::LIST_TIMEOUT) {
        Ok(s) => s.claude.active_by_runtime.host,
        Err(_) => load_view(env.data_file)
            .ok()
            .flatten()
            .and_then(|v| v.active_host_id().map(str::to_owned)),
    };
    match classify_rpc_outcome(
        call_error.as_deref(),
        observed.as_deref(),
        d_uuid(env.paths).as_deref(),
        target,
        target_uuid.as_deref(),
    ) {
        RpcVerdict::Failed(why) => Err(OrcaError::Refused(format!(
            "the switch did not take effect: {why}"
        ))),
        RpcVerdict::Success => {
            let generation = journal.as_ref().map(|j| j.generation).unwrap_or(0) + 1;
            write_journal(
                env.state,
                &Journal {
                    generation,
                    account: Some(target.to_owned()),
                    from,
                    to: target.to_owned(),
                    step: JournalStep::Committed,
                    orca_unconfirmed: false,
                    owner: env.owner.clone(),
                },
            )?;
            Ok(SwitchReport {
                route: Route::Rpc,
                outcome: Outcome::Switched,
                to: target.to_owned(),
                generation,
                steps: Vec::new(),
                readback: None,
                snapshot: None,
                refresh: None,
                redo: None,
            })
        }
    }
}

/// `accounts.selectClaude`, retried while Orca's own switch lock is held.
fn select_with_backoff(env: &SwitchEnv<'_>, target: &str) -> Result<(), rpc::RpcError> {
    let start = Instant::now();
    let mut wait = env.timing.busy_poll;
    loop {
        match rpc::select_claude(env.user_data, target, env.timing.select_timeout) {
            Err(e) if e.is_switch_in_progress() && start.elapsed() < env.timing.busy_wait => {
                std::thread::sleep(wait);
                wait = (wait * 2).min(Duration::from_secs(2));
            }
            Err(e) => return Err(e),
            Ok(_) => return Ok(()),
        }
    }
}

fn step_name(s: Step) -> JournalStep {
    match s {
        Step::Recover => JournalStep::Started,
        Step::ReadBack => JournalStep::ReadBack,
        Step::LoadTarget => JournalStep::LoadTarget,
        Step::CaptureSnapshot => JournalStep::CaptureSnapshot,
        Step::Refresh => JournalStep::Refresh,
        Step::Materialize(_) => JournalStep::Materialize,
        Step::Store => JournalStep::Store,
        Step::Verify => JournalStep::Store,
    }
}

fn run_offline(
    env: &SwitchEnv<'_>,
    l0: &LiveMark,
    view: &StoreView,
    rec: &AccountRecord,
    active: Option<String>,
    prev: Option<Journal>,
    steps: Vec<Step>,
) -> Result<SwitchReport, OrcaError> {
    let target = rec.id.clone();
    // An earlier switch that never finished: `D` may be half written by it.
    // This switch subsumes its repair only by committing; until then that
    // journal must stay pending (see `fail`).
    let prev_pending = prev.as_ref().filter(|j| j.pending()).cloned();
    let prev_gen = prev.as_ref().map(|j| j.generation).unwrap_or(0);
    let prev_account = prev.as_ref().and_then(|j| j.account.clone());
    let mut journal = Journal {
        generation: prev_gen,
        account: prev_account,
        from: active.clone(),
        to: target.clone(),
        step: JournalStep::Started,
        orca_unconfirmed: false,
        owner: env.owner.clone(),
    };
    write_journal(env.state, &journal)?;
    let mut report = SwitchReport {
        route: Route::Offline,
        outcome: Outcome::Switched,
        to: target.clone(),
        generation: prev_gen,
        steps: Vec::new(),
        readback: None,
        snapshot: None,
        refresh: None,
        redo: None,
    };
    let mut loaded: Option<Loaded> = None;
    let mut pre: Option<runtime::PreImages> = None;
    // A materialize that failed and could not restore D itself: D may be
    // half written although `pre` is unset.
    let mut d_unrestored = false;

    // A failure: restore D when it was touched, then record the end state.
    // Anything short of a restored D leaves the journal pending, so the
    // recovery repairs it. A restored D is only as good as it was before
    // this switch: when an earlier switch was pending, D is (again) the
    // state that crash left, so its journal goes back as it was instead of
    // being closed. Closing it would drop the only record that D may hold
    // one account's grant beside another's identity, which Orca's next
    // start would read back into the wrong stash by email match.
    let fail = |journal: &mut Journal,
                pre: &Option<runtime::PreImages>,
                d_unrestored: bool,
                e: OrcaError| {
        let restored = !d_unrestored
            && match pre {
                None => true,
                Some(p) => runtime::restore_preimages(&env.target_dir(), p).is_ok(),
            };
        let reopened = restored && prev_pending.is_some();
        if let Some(p) = prev_pending.as_ref().filter(|_| restored) {
            *journal = p.clone();
        } else if restored {
            journal.step = JournalStep::RolledBack;
        } else if d_unrestored {
            journal.step = JournalStep::Materialize;
        }
        let _ = write_journal(env.state, journal);
        if reopened {
            OrcaError::Refused(format!(
                "{e}; an earlier switch did not finish and D still needs its repair, \
                 run `csm accounts doctor --fix`"
            ))
        } else if restored {
            e
        } else {
            OrcaError::Refused(format!(
                "{e}; D could not be restored, run `csm accounts doctor --fix`"
            ))
        }
    };

    for step in steps {
        let r: Result<(), OrcaError> = match step {
            Step::Recover => Ok(()),
            Step::ReadBack => (|| {
                let Some(a) = active.as_deref().and_then(|id| view.account(id)) else {
                    return Ok(());
                };
                // Orca runs the read-back whenever the previous stash is
                // present, valid or not. When it cannot be loaded at all,
                // csm still runs it with no exclusion: every runtime grant
                // is then attributed or quarantined before the materialize
                // overwrites it (B1: never discard what csm cannot
                // attribute).
                let exclude = raw_stash_credentials(env, a);
                let rb = readback::read_back(&ReadBack {
                    os: env.os,
                    user_data: env.user_data,
                    paths: env.paths,
                    keychain_user: env.keychain_user,
                    records: &view.accounts,
                    exclude: exclude.as_ref().map(|s| s.expose()),
                    live_claude: (env.live_claude)(),
                    http: env.http,
                    quarantine: &env.quarantine(),
                    now_ms: super::now_ms(),
                    migration: false,
                })?;
                report.readback = Some(rb);
                Ok(())
            })(),
            Step::LoadTarget => match load_account(env, rec) {
                Ok(t) => {
                    loaded = Some(t);
                    Ok(())
                }
                Err(why) => Err(OrcaError::Refused(format!(
                    "the target account's stash is unusable: {why}"
                ))),
            },
            Step::CaptureSnapshot => {
                let t = loaded.as_ref().expect("LoadTarget ran");
                sysdefault::capture_for_managed_entry(
                    env.user_data,
                    env.os,
                    env.paths,
                    env.keychain_user,
                    t.creds.expose(),
                    super::now_ms(),
                )
                .map(|c| report.snapshot = Some(c))
            }
            Step::Refresh => {
                // No unretired-legacy-dir gate here, unlike the usage
                // collector's inactive refresh (design section 10 item 5):
                // this grant is about to become `D`'s, and claude refreshes
                // it there on first use anyway, so skipping would only move
                // the same rotation later. Orca's own switch refreshes the
                // same way, and migration step 7 (the floor switch) runs
                // before retire by design.
                let t = loaded.as_mut().expect("LoadTarget ran");
                refresh::refresh_stash_if_needed(
                    env.user_data,
                    env.os,
                    &t.stash,
                    t.creds.expose(),
                    env.http,
                    &env.quarantine(),
                    super::now_ms(),
                )
                .map(|r| {
                    report.refresh = Some(match r {
                        StashRefresh::NotDue => "not due".to_owned(),
                        StashRefresh::Failed(f) => format!("failed: {f:?}"),
                        StashRefresh::Quarantined(fp) => {
                            format!("refreshed but not stored; quarantined as {fp}")
                        }
                        StashRefresh::Refreshed(fresh) => {
                            t.creds = fresh;
                            "refreshed".to_owned()
                        }
                    });
                })
            }
            Step::Materialize(order) => {
                let t = loaded.as_ref().expect("LoadTarget ran");
                match runtime::materialize_checked(
                    &env.target_dir(),
                    t.creds.expose(),
                    t.oauth.as_ref(),
                    order,
                ) {
                    Ok(m) => {
                        pre = Some(m.pre);
                        Ok(())
                    }
                    Err(f) => {
                        d_unrestored = !f.d_restored;
                        Err(f.error)
                    }
                }
            }
            Step::Store => {
                let tid = target.clone();
                let mut build = |v: &StoreView| -> Result<Patch, OrcaError> {
                    if v.account(&tid).is_none() {
                        return Err(OrcaError::Refused(
                            "the target account vanished from Orca's store".into(),
                        ));
                    }
                    Ok(Patch {
                        accounts: None,
                        active_id: Some(Some(tid.clone())),
                        active_by_runtime: Some(p3(&v.active, Some(&tid), &RuntimeTarget::Host)),
                    })
                };
                match store::write_protocol(
                    env.data_file,
                    false,
                    env.live,
                    Some(l0),
                    env.state,
                    &mut build,
                ) {
                    Ok(StoreWrite::Written | StoreWrite::Unchanged) => Ok(()),
                    Ok(
                        w @ (StoreWrite::OrcaAtL0 | StoreWrite::OrcaAtL1 | StoreWrite::OrcaAtL2),
                    ) => {
                        // L0/L1: the store is untouched; put D back so it
                        // matches the store, then let Orca select.
                        let unrestored = if w != StoreWrite::OrcaAtL2
                            && let Some(p) = &pre
                            && let Err(f) = runtime::restore_preimages(&env.target_dir(), p)
                        {
                            Some(format!("D could not be restored ({})", f.join(", ")))
                        } else {
                            None
                        };
                        let redo = store::redo_over_rpc(
                            env.user_data,
                            &RedoOp::Select { id: target.clone() },
                            env.timing.redo,
                        );
                        let hand = handover(&HandoverFacts {
                            at_l2: w == StoreWrite::OrcaAtL2,
                            dir_agrees: (env.orca_dir_agrees)() == Some(true),
                            redo_switched: redo_switched(&redo),
                            reissued: matches!(redo, RedoOutcome::Reissued(_)),
                            d_unrestored: unrestored.is_some(),
                            prev_pending: prev_pending.is_some(),
                        });
                        // Orca confirmed the target in csm's D: that is a
                        // switch like any other, so bump the generation
                        // peers follow on (a limit-switch peer checks
                        // `gen > from_gen`).
                        if hand.bump {
                            journal.generation += 1;
                            journal.account = Some(target.clone());
                            report.generation = journal.generation;
                        }
                        match hand.journal {
                            HandoverJournal::Close => journal.step = JournalStep::HandedToOrca,
                            HandoverJournal::KeepPending => journal.orca_unconfirmed = true,
                            // D is back in the state an earlier crash left:
                            // its journal goes back as it was (see `fail`),
                            // marked so a recovery while Orca runs does not
                            // clear it for Orca's sync, which ran before
                            // csm put that state back.
                            HandoverJournal::Reopen => {
                                if let Some(p) = &prev_pending {
                                    journal = p.clone();
                                    journal.orca_unconfirmed = true;
                                    report.generation = journal.generation;
                                }
                            }
                        }
                        let still_pending = hand.journal != HandoverJournal::Close;
                        write_journal(env.state, &journal)?;
                        let unrestored = unrestored.map(|u| {
                            if still_pending {
                                format!(
                                    "{u}; `csm accounts doctor --fix` repairs it once Orca stops"
                                )
                            } else {
                                u
                            }
                        });
                        report.route = Route::OfflineThenRpc;
                        report.outcome = handed_outcome(&redo, unrestored.as_deref())?;
                        if let Some(why) = hand.not_switched {
                            report.outcome = Outcome::Uncertain(match unrestored {
                                Some(u) => format!("{why}; {u}"),
                                None => why.to_owned(),
                            });
                        }
                        report.redo = Some(redo);
                        report.steps.push(step);
                        return Ok(report);
                    }
                    Err(e) => Err(e),
                }
            }
            Step::Verify => {
                let t = loaded.as_ref().expect("LoadTarget ran");
                match verify(env, t) {
                    Ok(()) => Ok(()),
                    Err(why) => {
                        // The store names the target already: leave the
                        // journal pending for the repair.
                        journal.step = JournalStep::Store;
                        let _ = write_journal(env.state, &journal);
                        return Err(OrcaError::Refused(format!(
                            "verification failed: {why}; run `csm accounts doctor --fix`"
                        )));
                    }
                }
            }
        };
        if let Err(e) = r {
            return Err(fail(&mut journal, &pre, d_unrestored, e));
        }
        report.steps.push(step);
        journal.step = step_name(step);
        write_journal(env.state, &journal)?;
    }
    journal.generation += 1;
    journal.account = Some(target.clone());
    journal.step = JournalStep::Committed;
    write_journal(env.state, &journal)?;
    report.generation = journal.generation;
    Ok(report)
}

/// Whether Orca confirmed the switch during a redo. Pure.
fn redo_switched(redo: &RedoOutcome) -> bool {
    matches!(redo, RedoOutcome::AlreadyDone(_) | RedoOutcome::Reissued(_))
}

/// What a switch handed to Orca at L0/L1/L2 knows.
#[derive(Debug, Clone, Copy, Default)]
struct HandoverFacts {
    /// Orca appeared after the rename: `D` holds the target as written.
    /// Otherwise (L0/L1) `D` was put back to its pre-images.
    at_l2: bool,
    /// Orca main's `CLAUDE_CONFIG_DIR` is csm's `D` (`Some(true)` only).
    dir_agrees: bool,
    /// Orca shows the target (it did already, or it took the reissue).
    redo_switched: bool,
    /// csm reissued the select and Orca took it: Orca's select ran after
    /// csm put `D` back, so it rewrote Orca's runtime dir.
    reissued: bool,
    /// `D`'s pre-images could not be put back.
    d_unrestored: bool,
    /// An earlier switch was pending when this one started.
    prev_pending: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HandoverJournal {
    /// Orca owns `D` now, or `D` is back to a clean state.
    Close,
    /// Keep the last step: the offline repair runs once Orca stops.
    KeepPending,
    /// Write the earlier pending journal back.
    Reopen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Handover {
    journal: HandoverJournal,
    bump: bool,
    /// Why csm's `D` did not switch although Orca may have.
    not_switched: Option<&'static str>,
}

/// Decide the journal of a switch handed to Orca. Pure.
///
/// Orca's select rewrites only Orca's own runtime dir, so it repairs
/// csm's `D` only when the two agree. At L0/L1 csm put `D` back to its
/// pre-images; when an earlier switch was pending, those pre-images are
/// the state that crash left, which only Orca's own select (reissued after
/// the restore, in csm's `D`) overwrites. Otherwise the earlier journal
/// goes back as it was, the way a failed switch leaves it, so its repair
/// is not lost.
fn handover(f: &HandoverFacts) -> Handover {
    let orca_rewrote_d = f.dir_agrees && f.reissued;
    if !f.at_l2 && !f.d_unrestored && f.prev_pending && !orca_rewrote_d {
        return Handover {
            journal: HandoverJournal::Reopen,
            bump: false,
            not_switched: Some(
                "Orca started during the switch; an earlier switch did not finish and D \
                 still needs its repair, run `csm accounts doctor --fix` once Orca stops",
            ),
        };
    }
    if !f.at_l2 && !f.dir_agrees {
        return Handover {
            journal: HandoverJournal::KeepPending,
            bump: false,
            not_switched: Some(
                "Orca started during the switch and does not run with csm's D (or csm cannot \
                 tell), so its select does not reach csm's D; csm repairs D once Orca stops",
            ),
        };
    }
    Handover {
        journal: if handed_leaves_pending(f.redo_switched, f.d_unrestored) {
            HandoverJournal::KeepPending
        } else {
            HandoverJournal::Close
        },
        bump: f.redo_switched && (f.at_l2 || f.dir_agrees),
        not_switched: None,
    }
}

/// Whether a switch handed to Orca at L0/L1/L2 leaves the journal pending:
/// `D`'s pre-images could not be put back and Orca did not confirm its own
/// select (which rewrites `D`). The journal then keeps its last step, so
/// `accounts doctor --fix` or a launch repairs `D` once Orca stops. Pure.
fn handed_leaves_pending(redo_switched: bool, d_unrestored: bool) -> bool {
    d_unrestored && !redo_switched
}

/// The outcome of a switch handed to Orca at L0/L1/L2, from the RPC redo
/// and a failed restore of `D`'s pre-images. A restore failure is never
/// dropped: it turns a success into [`Outcome::Uncertain`] and is added to a
/// refusal or an uncertain answer. Pure.
fn handed_outcome(redo: &RedoOutcome, unrestored: Option<&str>) -> Result<Outcome, OrcaError> {
    let with = |why: &str| match unrestored {
        Some(u) => format!("{why}; {u}"),
        None => why.to_owned(),
    };
    Ok(match redo {
        RedoOutcome::AlreadyDone(_) | RedoOutcome::Reissued(_) => match unrestored {
            Some(u) => Outcome::Uncertain(format!("Orca switched, but {u}")),
            None => Outcome::Switched,
        },
        RedoOutcome::Failed(why) => {
            return Err(OrcaError::Refused(with(&format!(
                "Orca started during the switch and refused it: {why}"
            ))));
        }
        RedoOutcome::Uncertain(why) => Outcome::Uncertain(with(why)),
    })
}

// ─── offline attribution ──────────────────────────────────────────────────────

/// What [`attribute_offline`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attribution {
    /// The store names no active account: Orca's start does not read `D`
    /// back, so nothing was done.
    NoActiveAccount,
    /// The read-back ran. `identity_cleared`: `D`'s `oauthAccount` was
    /// removed afterwards.
    Done {
        readback: ReadBackReport,
        identity_cleared: bool,
    },
}

/// Attribute `D`'s runtime grants with Orca stopped, without switching:
/// step 3 of the offline switch (the read-back with the profile veto) on
/// its own, then `D` made neutral.
///
/// For the migration's step 7 when the switch cannot run offline (a
/// SQLite-backed store, or the floor profile has no account). Orca's first
/// start reads `D` back while nothing is written yet
/// (runtime-auth-readback.ts, `lastWrittenCredentialsJson === null`): a
/// candidate matched by the email in `D`'s `oauthAccount` and fresher than
/// that account's stash is written into it with no profile check. So csm
/// first files each grant with its owner (or in the quarantine), then
/// removes `D`'s `oauthAccount`: without it Orca's matcher accepts a grant
/// only by an equal refresh token (runtime-auth-credential-matching.ts),
/// which is the owner csm just stored it with. The identity goes only when
/// the store names an active account, whose materialize on Orca's start
/// writes it again; with none, `D` is the system default Orca captures and
/// is left alone. Writes stashes, csm's quarantine and `D`'s
/// `.claude.json` only, never the store.
pub fn attribute_offline(env: &SwitchEnv<'_>) -> Result<Attribution, OrcaError> {
    let _lock = SwitchLock::acquire(env.state, env.timing.lock_wait)
        .map_err(|e| OrcaError::io("cannot take", &env.state.join(fsx::SWITCH_LOCK), e))?;
    if env.live.mark().running {
        return Err(OrcaError::Refused(
            "Orca is running; its own sync owns D".into(),
        ));
    }
    if !env.version_ok {
        return Err(OrcaError::Refused(
            "the installed Orca version is not one csm was tested with".into(),
        ));
    }
    if !env.store_access_allowed {
        return Err(OrcaError::Refused(
            "this userData is not csm's to write".into(),
        ));
    }
    let Some(view) = load_view(env.data_file)? else {
        return Ok(Attribution::NoActiveAccount);
    };
    let Some(active) = named_active(&view).and_then(|id| view.account(&id).cloned()) else {
        return Ok(Attribution::NoActiveAccount);
    };
    let exclude = raw_stash_credentials(env, &active);
    let readback = readback::read_back(&ReadBack {
        os: env.os,
        user_data: env.user_data,
        paths: env.paths,
        keychain_user: env.keychain_user,
        records: &view.accounts,
        exclude: exclude.as_ref().map(|s| s.expose()),
        live_claude: (env.live_claude)(),
        http: env.http,
        quarantine: &env.quarantine(),
        now_ms: super::now_ms(),
        migration: false,
    })?;
    // Orca came up during the read-back: never write `D` behind it.
    if env.live.mark().running {
        return Ok(Attribution::Done {
            readback,
            identity_cleared: false,
        });
    }
    let identity_cleared = matches!(
        read_runtime_identity(env.paths),
        RuntimeIdentity::Present(_)
    );
    if identity_cleared {
        runtime::clear_identity(env.paths)?;
    }
    Ok(Attribution::Done {
        readback,
        identity_cleared,
    })
}

// ─── recovery ─────────────────────────────────────────────────────────────────

/// What [`recover`] did.
#[derive(Debug)]
pub enum Recovery {
    /// No unfinished switch.
    Nothing,
    /// Orca runs: its own sync owns `D`; the intent was cleared.
    ClearedForOrca,
    /// The store names no active account: nothing to materialize; `D` was
    /// made neutral.
    Neutralized,
    /// The store names no active account and `D` holds an identity that is
    /// neither side of the crashed switch: the system default Orca put back
    /// when it cleared the selection. `D` was left alone.
    KeptSystemDefault,
    /// A switch away from the system default (no active account) died
    /// after the snapshot: `D` was put back to the snapshot.
    RestoredSystemDefault(sysdefault::RestoreReport),
    /// A switch away from the system default died before it touched `D`:
    /// nothing to repair.
    Untouched,
    /// `D` now holds the account the store names.
    Repaired(Box<SwitchReport>),
    /// Orca started during the repair, which was handed to it, and the
    /// hand-over did not verify (no answer, or `D` could not be put back).
    /// csm wrote nothing behind the running Orca. The text says why.
    Uncertain(String),
    /// The repair failed; `D` was made neutral. The error text says why.
    Failed(String),
    /// Another csm held `switch.lock` for the whole wait (a switch in
    /// progress, or a login): nothing was attempted and `D` was not touched.
    /// Not a failed repair; the holder may well finish correctly.
    Busy,
    /// Orca runs and csm cannot repair or hand over the switch now (Orca's
    /// `D` is another, or Orca selected an account since a switch away from
    /// the system default died): nothing was written and the journal stays
    /// pending for the repair once Orca stops. The text says why.
    Deferred(String),
}

/// Repair an unfinished switch left by a dead csm.
pub fn recover(env: &SwitchEnv<'_>) -> Result<Recovery, OrcaError> {
    let _lock = match SwitchLock::acquire(env.state, env.timing.lock_wait) {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(Recovery::Busy),
        Err(e) => {
            return Err(OrcaError::io(
                "cannot take",
                &env.state.join(fsx::SWITCH_LOCK),
                e,
            ));
        }
    };
    let Some(mut j) = read_journal(env.state).filter(Journal::pending) else {
        return Ok(Recovery::Nothing);
    };
    if env.live.mark().running {
        return recover_with_orca(env, &mut j);
    }
    // The same reading `switch_locked` uses: a dangling active id is no
    // account, so it settles here instead of failing a repair to a record
    // that does not exist (and neutralizing the system default it restored).
    let active = load_view(env.data_file)?.as_ref().and_then(named_active);
    let neutral = |j: &mut Journal| -> Result<(), OrcaError> {
        runtime::clear_identity(env.paths)?;
        j.step = JournalStep::RolledBack;
        write_journal(env.state, j)
    };
    let Some(id) = active else {
        return settle_without_active(env, &mut j);
    };
    match switch_locked(env, &id) {
        Ok(r) => Ok(repaired(r)),
        Err(e) => {
            let mut j = read_journal(env.state).unwrap_or(j);
            // Orca came up between the check above and the switch's own L0
            // (its first RPC then failed while the socket was not yet
            // listening), or during the offline steps: csm never writes `D`
            // behind a running Orca.
            if env.live.mark().running {
                return Ok(orca_came_up(j.pending(), &e.to_string()));
            }
            neutral(&mut j)?;
            Ok(Recovery::Failed(e.to_string()))
        }
    }
}

/// A repair's switch report as a [`Recovery`]: an uncertain hand-over to an
/// Orca that started meanwhile is not a repair. Pure.
fn repaired(r: SwitchReport) -> Recovery {
    match &r.outcome {
        Outcome::Uncertain(why) => Recovery::Uncertain(why.clone()),
        Outcome::Switched | Outcome::AlreadyActive => Recovery::Repaired(Box::new(r)),
    }
}

/// A repair that failed after Orca came up: nothing more is written.
/// `pending`: the journal still says the switch is unfinished. Pure.
fn orca_came_up(pending: bool, why: &str) -> Recovery {
    if pending {
        Recovery::Deferred(format!(
            "Orca started during the repair ({why}); csm repairs D once Orca stops"
        ))
    } else {
        Recovery::Uncertain(format!("Orca started during the repair: {why}"))
    }
}

/// [`recover`] while Orca runs.
fn recover_with_orca(env: &SwitchEnv<'_>, j: &mut Journal) -> Result<Recovery, OrcaError> {
    if j.from.is_none() && null_switch_touched_d(j.step) {
        // Only Orca's live answer counts: its store lags its memory (1.4.214
        // writes it at quit).
        return match rpc::accounts_list(env.user_data, false, rpc::LIST_TIMEOUT) {
            Ok(snap) => match snap.claude.active_by_runtime.host.as_deref() {
                None => Ok(Recovery::Deferred(defer_null_crash(env, j)?)),
                Some(a) => Ok(Recovery::Deferred(orca_selected_since_crash(j, a))),
            },
            // No answer (Orca still starting, a busy socket): keep the
            // snapshot anyway. Preserving reads only the snapshot and writes
            // only csm's quarantine, and a GUI select right after would
            // force-capture the half-written `D` over it.
            Err(e) => {
                let kept = keep_null_snapshot(env, j)?;
                Ok(Recovery::Deferred(format!(
                    "a switch away from the system default did not finish and Orca does not \
                     say whether it selected an account since ({e}); csm repairs it once Orca \
                     stops{kept}"
                )))
            }
        };
    }
    if j.orca_unconfirmed {
        // Handed to Orca without Orca confirming a select that rewrote `D`
        // (see `handover`): Orca believes `D` is in sync, so its own sync
        // does not repair it. Clearing the journal here would drop the
        // repair the hand-over promised for once Orca stops.
        return Ok(Recovery::Deferred(
            "a switch handed to Orca left D unsettled and Orca did not confirm rewriting it; \
             csm repairs it once Orca stops (`csm accounts doctor --fix`)"
                .into(),
        ));
    }
    match (env.orca_dir_agrees)() {
        Some(true) => {
            j.step = JournalStep::HandedToOrca;
            write_journal(env.state, j)?;
            Ok(Recovery::ClearedForOrca)
        }
        Some(false) => Ok(Recovery::Deferred(
            "Orca runs with another CLAUDE_CONFIG_DIR than csm, so its sync does not repair \
             csm's D; csm repairs it once Orca stops"
                .into(),
        )),
        None => Ok(Recovery::Deferred(
            "cannot read Orca's CLAUDE_CONFIG_DIR, so csm cannot tell that Orca's sync owns \
             csm's D; csm repairs it once Orca stops"
                .into(),
        )),
    }
}

/// A crashed switch away from the system default while Orca runs and names
/// no account: file the snapshot's grants in the quarantine (csm's own
/// state; `D` and Orca's files are not touched) and say why the repair
/// waits. The journal stays pending.
fn defer_null_crash(env: &SwitchEnv<'_>, j: &Journal) -> Result<String, OrcaError> {
    let kept = keep_null_snapshot(env, j)?;
    Ok(format!(
        "a switch from the system default to {} did not finish, and csm does not write D \
         while Orca runs{kept}. Quit Orca and run `csm accounts doctor --fix`",
        j.to
    ))
}

/// File the system-default snapshot's grants in the quarantine for a
/// crashed switch away from the system default (idempotent; `D` and Orca's
/// files are not touched). Returns the note suffix naming what was kept.
fn keep_null_snapshot(env: &SwitchEnv<'_>, j: &Journal) -> Result<String, OrcaError> {
    let managed = read_stash_grant(env, &j.to);
    let filed = sysdefault::preserve_snapshot(
        env.user_data,
        managed.as_ref().map(|s| s.expose()),
        &env.quarantine(),
        super::now_ms(),
    )?;
    Ok(if filed.is_empty() {
        String::new()
    } else {
        format!(
            "; the system default's grants are kept in csm's quarantine ({})",
            filed.join(", ")
        )
    })
}

/// The note for a crashed switch away from the system default that Orca
/// selected an account over. Pure.
fn orca_selected_since_crash(j: &Journal, active: &str) -> String {
    format!(
        "a switch from the system default to {} did not finish, and Orca has selected {active} \
         since; its system-default snapshot may now hold that switch's grant. Quit Orca and run \
         `csm accounts doctor --fix`",
        j.to
    )
}

/// Whether a switch from no account that last finished `step` may have
/// written `D`: the snapshot (step 5) is the last step before the
/// materialize, and nothing before it writes `D`. Pure.
pub fn null_switch_touched_d(step: JournalStep) -> bool {
    !matches!(
        step,
        JournalStep::Started | JournalStep::ReadBack | JournalStep::LoadTarget
    )
}

/// Close a pending journal while the store names no active account.
///
/// - A switch away from the system default (`from` null) that got past the
///   snapshot: `D` goes back to the snapshot it captured (design section 3:
///   "materialize `from`"). Only clearing the identity would leave the
///   target's grant in `D` as the "system default", and the next switch from
///   null would then force-capture it over the snapshot, losing the user's
///   own login.
/// - One that died before the snapshot never wrote `D`: nothing to repair,
///   and `D`'s identity (the system default's own) is kept.
/// - A switch from an account (`from` set) while the store now names none:
///   only Orca can have cleared the active id since (a GUI deselect, or
///   removing the active account), and Orca then put the system default
///   back into `D` (`restoreSystemDefaultSnapshot`). So `D`'s identity is
///   cleared only when it is one of the crashed switch's own accounts (what
///   a dead csm leaves), or when `D` has none; any other identity is the
///   system default Orca restored and is kept, or the next switch's forced
///   capture would record a null identity over the snapshot.
fn settle_without_active(env: &SwitchEnv<'_>, j: &mut Journal) -> Result<Recovery, OrcaError> {
    let recovery = if let Some(from) = j.from.as_deref() {
        let parties = [from, j.to.as_str()]
            .into_iter()
            .filter_map(|id| uuid_of(read_stash_oauth(env, id).as_ref()))
            .collect::<Vec<_>>();
        if identity_left_by_switch(d_uuid(env.paths).as_deref(), &parties) {
            runtime::clear_identity(env.paths)?;
            Recovery::Neutralized
        } else {
            Recovery::KeptSystemDefault
        }
    } else if !null_switch_touched_d(j.step) {
        Recovery::Untouched
    } else {
        let managed = read_stash_grant(env, &j.to);
        let managed_oauth = read_stash_oauth(env, &j.to);
        let r = sysdefault::restore_after_crash(
            env.user_data,
            env.os,
            env.paths,
            env.keychain_user,
            managed.as_ref().map(|s| s.expose()),
            managed_oauth.as_ref(),
            &env.quarantine(),
            super::now_ms(),
        )?;
        Recovery::RestoredSystemDefault(r)
    };
    j.step = JournalStep::RolledBack;
    write_journal(env.state, j)?;
    Ok(recovery)
}

/// Whether `D`'s identity (`d`) is one a crashed switch between accounts
/// left, given the `accountUuid`s of its `from` and `to` stashes: no
/// identity at all (clearing is a no-op), or one of theirs. Pure.
fn identity_left_by_switch(d: Option<&str>, parties: &[String]) -> bool {
    d.is_none_or(|u| parties.iter().any(|p| p == u))
}

/// Account `id`'s stashed grant as stored (valid or not), read through the
/// store's record; `None` when there is none.
fn read_stash_grant(env: &SwitchEnv<'_>, id: &str) -> Option<SecretString> {
    let view = load_view(env.data_file).ok()??;
    raw_stash_credentials(env, view.account(id)?)
}

/// Account `id`'s stashed `oauth-account.json`; `None` when there is none.
fn read_stash_oauth(env: &SwitchEnv<'_>, id: &str) -> Option<Value> {
    let view = load_view(env.data_file).ok()??;
    let rec = view.account(id)?;
    if !rec.is_host() {
        return None;
    }
    Stash::open(env.user_data, &rec.id, rec.managed_auth_path.as_deref())
        .ok()?
        .oauth_account()
        .ok()
        .flatten()
        .filter(|v| !v.is_null())
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orca::http::FakeHttp;
    use crate::orca::runtime::runtime_paths;
    use crate::orca::testsupport::{
        ScriptedLiveness, creds_json, oauth_json, record_json, write_store,
    };
    use serde_json::json;

    fn state(target: TargetState) -> SwitchState {
        SwitchState {
            target_id: "id-b".into(),
            target,
            orca_running: false,
            orca_dir_agrees: None,
            offline_allowed: Ok(()),
            active: Some("id-a".into()),
            d_holds_target: false,
            journal_pending: false,
            live_claude: false,
        }
    }

    #[test]
    fn plan_routes_and_steps() {
        use Step::*;
        assert!(matches!(
            plan_switch(&state(TargetState::Missing)),
            Plan::Refuse(_)
        ));
        assert!(matches!(
            plan_switch(&state(TargetState::NotHost)),
            Plan::Refuse(_)
        ));
        assert!(matches!(
            plan_switch(&state(TargetState::Unusable("x".into()))),
            Plan::Refuse(_)
        ));
        let mut s = state(TargetState::Ok);
        assert_eq!(
            plan_switch(&s),
            Plan::Offline(vec![
                ReadBack,
                LoadTarget,
                Refresh,
                Materialize(Order::NeutralWindow),
                Store,
                Verify
            ])
        );
        s.live_claude = true;
        assert_eq!(
            plan_switch(&s),
            Plan::Offline(vec![
                ReadBack,
                LoadTarget,
                Materialize(Order::OrcaOrder),
                Store,
                Verify
            ])
        );
        s.live_claude = false;
        s.active = None;
        s.journal_pending = true;
        assert_eq!(
            plan_switch(&s),
            Plan::Offline(vec![
                Recover,
                LoadTarget,
                CaptureSnapshot,
                Refresh,
                Materialize(Order::NeutralWindow),
                Store,
                Verify
            ])
        );
        // a == i: the repair (steps 3, 6 and 7-8), or nothing when D holds
        // it. Like Orca's sync, the repair refreshes a due grant unless a
        // live claude owns it.
        let mut s = state(TargetState::Ok);
        s.active = Some("id-b".into());
        assert_eq!(
            plan_switch(&s),
            Plan::Offline(vec![
                ReadBack,
                LoadTarget,
                Refresh,
                Materialize(Order::NeutralWindow),
                Store,
                Verify
            ])
        );
        s.live_claude = true;
        assert_eq!(
            plan_switch(&s),
            Plan::Offline(vec![
                ReadBack,
                LoadTarget,
                Materialize(Order::OrcaOrder),
                Store,
                Verify
            ])
        );
        s.live_claude = false;
        s.d_holds_target = true;
        assert_eq!(plan_switch(&s), Plan::Noop);
        s.journal_pending = true;
        assert!(matches!(plan_switch(&s), Plan::Offline(_)));
        // Offline gates.
        let mut s = state(TargetState::Ok);
        s.offline_allowed = Err("version".into());
        assert!(matches!(plan_switch(&s), Plan::Refuse(r) if r.contains("version")));
        // Running: RPC only when both D agree.
        let mut s = state(TargetState::Ok);
        s.orca_running = true;
        assert!(matches!(plan_switch(&s), Plan::Refuse(_)));
        s.orca_dir_agrees = Some(false);
        assert!(matches!(plan_switch(&s), Plan::Refuse(_)));
        s.orca_dir_agrees = Some(true);
        assert_eq!(plan_switch(&s), Plan::Rpc);
    }

    #[test]
    fn rpc_outcome_needs_the_state_not_the_answer() {
        use RpcVerdict::*;
        assert_eq!(
            classify_rpc_outcome(None, Some("b"), Some("u-b"), "b", Some("u-b")),
            Success
        );
        // A timeout whose effect landed is a success.
        assert_eq!(
            classify_rpc_outcome(Some("timed out"), Some("b"), Some("u-b"), "b", Some("u-b")),
            Success
        );
        assert!(matches!(
            classify_rpc_outcome(None, Some("a"), Some("u-b"), "b", Some("u-b")),
            Failed(_)
        ));
        assert!(matches!(
            classify_rpc_outcome(None, None, None, "b", None),
            Failed(_)
        ));
        assert!(matches!(
            classify_rpc_outcome(None, Some("b"), Some("u-a"), "b", Some("u-b")),
            Failed(_)
        ));
        assert!(matches!(
            classify_rpc_outcome(None, Some("b"), None, "b", Some("u-b")),
            Failed(_)
        ));
        assert_eq!(
            classify_rpc_outcome(None, Some("b"), None, "b", None),
            Success
        );
        match classify_rpc_outcome(Some("rolled back"), Some("a"), None, "b", None) {
            Failed(w) => assert!(w.contains("rolled back")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn journal_round_trips_and_knows_pending() {
        let tmp = tempfile::tempdir().unwrap();
        let j = Journal {
            generation: 3,
            account: Some("id-a".into()),
            from: Some("id-a".into()),
            to: "id-b".into(),
            step: JournalStep::Materialize,
            orca_unconfirmed: false,
            owner: Owner {
                pid: 1,
                born: Some(2),
            },
        };
        write_journal(tmp.path(), &j).unwrap();
        assert_eq!(read_journal(tmp.path()), Some(j.clone()));
        assert!(j.pending());
        let text = std::fs::read_to_string(journal_path(tmp.path())).unwrap();
        assert!(text.contains("\"step\": \"materialize\""));
        assert!(text.contains("\"gen\": 3"));
        for s in [
            JournalStep::Committed,
            JournalStep::RolledBack,
            JournalStep::HandedToOrca,
        ] {
            assert!(
                !Journal {
                    step: s,
                    orca_unconfirmed: false,
                    ..j.clone()
                }
                .pending()
            );
        }
    }

    // ─── offline shell ────────────────────────────────────────────────────────

    const OS: HostOs = HostOs::Linux;

    struct World {
        _tmp: Option<tempfile::TempDir>,
        ud: PathBuf,
        state: PathBuf,
        paths: RuntimePaths,
        choice: DataFileChoice,
        user: KeychainUser,
    }

    fn a_creds() -> String {
        creds_json("at-a", "rt-a", 4_000_000_000_000)
    }
    fn b_creds() -> String {
        creds_json("at-b", "rt-b", 4_000_000_000_000)
    }

    impl World {
        /// Accounts a (active, materialized in D) and b, under `ud` (a temp
        /// dir, or a fake Orca's userData).
        fn new(ud: Option<&Path>) -> World {
            let tmp = tempfile::tempdir().unwrap();
            let ud = ud
                .map(Path::to_path_buf)
                .unwrap_or_else(|| tmp.path().join("ud"));
            let d = tmp.path().join("D");
            std::fs::create_dir_all(&d).unwrap();
            let paths = runtime_paths(Some(d.to_str().unwrap()), tmp.path(), |p| p.exists());
            let mut recs = Vec::new();
            for (id, email, uuid, creds) in [
                ("id-a", "alice@example.com", "u-a", a_creds()),
                ("id-b", "bob@example.com", "u-b", b_creds()),
            ] {
                let s = stash::create(&ud, id).unwrap();
                s.write_auth(&ud, OS, &creds, &oauth_json(uuid, email, None))
                    .unwrap();
                recs.push(record_json(&ud, id, email, None));
            }
            let choice = write_store(&ud, &recs, Some("id-a"));
            std::fs::write(&paths.credentials_path, a_creds()).unwrap();
            std::fs::write(
                &paths.config_path,
                json!({"numStartups": 1, "oauthAccount": oauth_json("u-a", "alice@example.com", None)}).to_string(),
            )
            .unwrap();
            World {
                state: tmp.path().join("state"),
                _tmp: Some(tmp),
                ud,
                paths,
                choice,
                user: KeychainUser {
                    acct: "tester".into(),
                    delete_accts: vec!["tester".into()],
                },
            }
        }

        fn env<'a>(&'a self, live: &'a dyn Liveness, http: &'a dyn OauthHttp) -> SwitchEnv<'a> {
            SwitchEnv {
                os: OS,
                user_data: &self.ud,
                data_file: &self.choice,
                state: &self.state,
                paths: &self.paths,
                keychain_user: &self.user,
                live,
                http,
                live_claude: &|| false,
                orca_dir_agrees: &|| Some(true),
                version_ok: true,
                store_access_allowed: true,
                owner: Owner {
                    pid: std::process::id(),
                    born: None,
                },
                timing: SwitchTiming {
                    lock_wait: Duration::from_secs(2),
                    select_timeout: Duration::from_secs(2),
                    busy_wait: Duration::from_millis(300),
                    busy_poll: Duration::from_millis(20),
                    redo: RedoOpts {
                        wait: Duration::from_secs(2),
                        poll: Duration::from_millis(20),
                    },
                },
            }
        }

        fn store_active(&self) -> Option<String> {
            load_view(&self.choice)
                .unwrap()
                .unwrap()
                .active_host_id()
                .map(str::to_owned)
        }

        fn d_creds(&self) -> String {
            std::fs::read_to_string(&self.paths.credentials_path).unwrap()
        }
    }

    #[test]
    fn offline_switch_materializes_patches_and_commits() {
        let w = World::new(None);
        let live = ScriptedLiveness::stopped();
        let http = FakeHttp::default();
        let r = switch(&w.env(&live, &http), "id-b").unwrap();
        assert_eq!(r.route, Route::Offline);
        assert_eq!(r.outcome, Outcome::Switched);
        assert_eq!(r.generation, 1);
        assert_eq!(w.store_active().as_deref(), Some("id-b"));
        assert_eq!(w.d_creds(), b_creds());
        assert_eq!(d_uuid(&w.paths).as_deref(), Some("u-b"));
        let cfg: Value =
            serde_json::from_slice(&std::fs::read(&w.paths.config_path).unwrap()).unwrap();
        assert_eq!(cfg["numStartups"], json!(1));
        let j = read_journal(&w.state).unwrap();
        assert_eq!(
            (j.generation, j.step, j.account.as_deref()),
            (1, JournalStep::Committed, Some("id-b"))
        );
        // The runtime grant equalled a's stash: no read-back candidate, no
        // profile call; b's grant is far from expiry: no refresh call.
        assert!(http.profile_calls.lock().unwrap().is_empty());
        assert!(http.token_bodies.lock().unwrap().is_empty());
        assert_eq!(r.refresh.as_deref(), Some("not due"));
        // Switching to it again is a no-op.
        let r = switch(&w.env(&live, &http), "id-b").unwrap();
        assert_eq!(r.outcome, Outcome::AlreadyActive);
        // The store's other bytes are intact and a pre-image was kept.
        assert!(
            std::fs::read_to_string(&w.choice.path)
                .unwrap()
                .contains("\"zoom\":1.25")
        );
        assert_eq!(
            std::fs::read_dir(store::preimage_dir(&w.state))
                .unwrap()
                .count(),
            1
        );
    }

    #[test]
    fn a_refreshed_grant_from_claude_code_is_filed_before_the_switch() {
        let w = World::new(None);
        let fresher = creds_json("at-a2", "rt-a2", 4_100_000_000_000);
        std::fs::write(&w.paths.credentials_path, &fresher).unwrap();
        let live = ScriptedLiveness::stopped();
        let http = FakeHttp::default().profile_uuid("at-a2", "u-a");
        let r = switch(&w.env(&live, &http), "id-b").unwrap();
        assert_eq!(r.readback.unwrap().persisted.as_deref(), Some("id-a"));
        let a = Stash::open(&w.ud, "id-a", None).unwrap();
        assert_eq!(a.credentials(OS).unwrap().unwrap().expose(), fresher);
        assert_eq!(w.d_creds(), b_creds());
    }

    #[test]
    fn no_network_for_the_profile_veto_aborts_with_nothing_changed() {
        let w = World::new(None);
        let fresher = creds_json("at-a2", "rt-a2", 4_100_000_000_000);
        std::fs::write(&w.paths.credentials_path, &fresher).unwrap();
        let live = ScriptedLiveness::stopped();
        let err = switch(&w.env(&live, &FakeHttp::default()), "id-b").unwrap_err();
        assert!(matches!(err, OrcaError::Network(_)), "{err}");
        assert_eq!(w.store_active().as_deref(), Some("id-a"));
        assert_eq!(w.d_creds(), fresher);
        assert!(!read_journal(&w.state).unwrap().pending());
    }

    /// An offline a -> b switch with a live claude (Orca's order) died after
    /// writing b's grant and before the identity: b's grant sits beside a's
    /// `oauthAccount`, the store still names a, the journal is pending. The
    /// next switch fails before it writes `D` (no network for the profile
    /// veto): the earlier journal must stay pending, since `D` still needs
    /// its repair. Closing it would let Orca's next start file b's grant
    /// into a's stash by email match.
    #[test]
    fn a_failed_switch_over_a_pending_journal_keeps_it_pending() {
        let w = World::new(None);
        std::fs::write(&w.paths.credentials_path, b_creds()).unwrap();
        let crashed = Journal {
            generation: 3,
            account: Some("id-a".into()),
            from: Some("id-a".into()),
            to: "id-b".into(),
            step: JournalStep::LoadTarget,
            orca_unconfirmed: false,
            owner: Owner {
                pid: 1,
                born: Some(1),
            },
        };
        write_journal(&w.state, &crashed).unwrap();
        let live = ScriptedLiveness::stopped();
        let http = FakeHttp::default();
        let err = switch(&w.env(&live, &http), "id-b").unwrap_err();
        assert!(err.to_string().contains("doctor --fix"), "{err}");
        let j = read_journal(&w.state).unwrap();
        assert!(j.pending(), "{j:?}");
        assert_eq!(j, crashed);
        assert_eq!(w.d_creds(), b_creds());
        assert_eq!(w.store_active().as_deref(), Some("id-a"));
        // The repair still runs; with the profile endpoint still silent it
        // fails, and D is left neutral (no identity to match b's grant by).
        match recover(&w.env(&live, &http)).unwrap() {
            Recovery::Failed(_) => {}
            other => panic!("{other:?}"),
        }
        assert_eq!(d_uuid(&w.paths), None);
        // Without an earlier pending journal the same failure closes it.
        let w = World::new(None);
        std::fs::write(&w.paths.credentials_path, b_creds()).unwrap();
        let err = switch(&w.env(&live, &http), "id-b").unwrap_err();
        assert!(matches!(err, OrcaError::Network(_)), "{err}");
        assert!(!read_journal(&w.state).unwrap().pending());
    }

    #[test]
    fn a_first_switch_from_no_account_captures_the_system_default() {
        let w = World::new(None);
        let recs: Vec<Value> = ["id-a", "id-b"]
            .iter()
            .zip(["alice@example.com", "bob@example.com"])
            .map(|(id, e)| record_json(&w.ud, id, e, None))
            .collect();
        write_store(&w.ud, &recs, None);
        std::fs::write(&w.paths.credentials_path, "SYSTEM-DEFAULT").unwrap();
        let live = ScriptedLiveness::stopped();
        let r = switch(&w.env(&live, &FakeHttp::default()), "id-b").unwrap();
        assert_eq!(r.snapshot, Some(Captured::Written));
        let snap = sysdefault::read_snapshot(&w.ud).unwrap();
        assert_eq!(snap["credentialsJson"], json!("SYSTEM-DEFAULT"));
        assert!(r.readback.is_none());
    }

    #[test]
    fn a_due_refresh_goes_into_the_stash_before_materializing() {
        let w = World::new(None);
        let due = creds_json("at-b", "rt-b", 1000);
        Stash::open(&w.ud, "id-b", None)
            .unwrap()
            .write_credentials(&w.ud, OS, &due)
            .unwrap();
        let live = ScriptedLiveness::stopped();
        let http = FakeHttp::default().token_reply(FakeHttp::reply(
            200,
            r#"{"access_token":"at-b2","refresh_token":"rt-b2","expires_in":3600}"#,
        ));
        let r = switch(&w.env(&live, &http), "id-b").unwrap();
        assert_eq!(r.refresh.as_deref(), Some("refreshed"));
        let stored = Stash::open(&w.ud, "id-b", None)
            .unwrap()
            .credentials(OS)
            .unwrap()
            .unwrap();
        assert!(stored.expose().contains("at-b2"));
        assert_eq!(w.d_creds(), stored.expose());
    }

    #[test]
    fn a_refused_offline_gate_writes_nothing() {
        let w = World::new(None);
        let live = ScriptedLiveness::stopped();
        let http = FakeHttp::default();
        let mut env = w.env(&live, &http);
        env.version_ok = false;
        assert!(matches!(switch(&env, "id-b"), Err(OrcaError::Refused(_))));
        assert!(matches!(
            switch(&w.env(&live, &http), "id-zzz"),
            Err(OrcaError::Refused(_))
        ));
        assert_eq!(w.d_creds(), a_creds());
        assert!(read_journal(&w.state).is_none());
    }

    #[test]
    fn a_sqlite_backed_store_refuses_the_offline_switch_before_touching_d() {
        let w = World::new(None);
        let store_before = std::fs::read(&w.choice.path).unwrap();
        std::fs::write(
            w.choice
                .path
                .with_file_name(crate::orca::userdata::STATE_DB),
            b"",
        )
        .unwrap();
        let live = ScriptedLiveness::stopped();
        let http = FakeHttp::default();
        let e = switch(&w.env(&live, &http), "id-b").unwrap_err();
        assert!(e.to_string().contains("SQLite"), "{e}");
        assert_eq!(std::fs::read(&w.choice.path).unwrap(), store_before);
        assert_eq!(w.d_creds(), a_creds());
        assert!(read_journal(&w.state).is_none());
    }

    /// Migration step 7 with a SQLite-backed store: no offline switch, but
    /// `D`'s grants are attributed with the profile veto and `D`'s identity
    /// is cleared, so Orca's first start (nothing written yet) cannot file
    /// a's grant into b's stash by the email in `D`'s `oauthAccount`.
    #[test]
    fn offline_attribution_files_d_grants_and_neutralizes_d_without_the_store() {
        let w = World::new(None);
        std::fs::write(
            w.choice
                .path
                .with_file_name(crate::orca::userdata::STATE_DB),
            b"",
        )
        .unwrap();
        let store_before = std::fs::read(&w.choice.path).unwrap();
        // a's rotated grant beside b's identity: what Orca's cold read-back
        // would match to b by email.
        let fresher = creds_json("at-a2", "rt-a2", 4_100_000_000_000);
        std::fs::write(&w.paths.credentials_path, &fresher).unwrap();
        std::fs::write(
            &w.paths.config_path,
            json!({"numStartups": 1, "oauthAccount": oauth_json("u-b", "bob@example.com", None)})
                .to_string(),
        )
        .unwrap();
        let live = ScriptedLiveness::stopped();
        let http = FakeHttp::default().profile_uuid("at-a2", "u-a");
        let Attribution::Done {
            readback,
            identity_cleared,
        } = attribute_offline(&w.env(&live, &http)).unwrap()
        else {
            panic!("the store names an active account");
        };
        assert!(identity_cleared);
        assert_eq!(readback.candidates, 1);
        // b's stash is untouched; the grant sits with a or in the quarantine.
        let b = Stash::open(&w.ud, "id-b", None).unwrap();
        assert_eq!(b.credentials(OS).unwrap().unwrap().expose(), b_creds());
        let a = Stash::open(&w.ud, "id-a", None).unwrap();
        let in_a = a.credentials(OS).unwrap().unwrap().expose() == fresher;
        let q = Quarantine::new(OS, &w.state);
        let in_q = q
            .list()
            .iter()
            .any(|m| q.get(&m.fingerprint).unwrap().unwrap().expose() == fresher);
        assert!(in_a || in_q, "{readback:?}");
        // D: grant kept, identity gone, other keys intact; store untouched.
        assert_eq!(w.d_creds(), fresher);
        assert!(matches!(
            read_runtime_identity(&w.paths),
            RuntimeIdentity::None
        ));
        let cfg: Value =
            serde_json::from_slice(&std::fs::read(&w.paths.config_path).unwrap()).unwrap();
        assert_eq!(cfg["numStartups"], json!(1));
        assert_eq!(std::fs::read(&w.choice.path).unwrap(), store_before);
        assert!(read_journal(&w.state).is_none());
    }

    #[test]
    fn offline_attribution_refuses_a_running_orca_and_skips_no_active_account() {
        let w = World::new(None);
        let cfg_before = std::fs::read(&w.paths.config_path).unwrap();
        let running = ScriptedLiveness::new(vec![crate::orca::testsupport::running_mark()]);
        let http = FakeHttp::default();
        assert!(matches!(
            attribute_offline(&w.env(&running, &http)),
            Err(OrcaError::Refused(_))
        ));
        assert_eq!(std::fs::read(&w.paths.config_path).unwrap(), cfg_before);
        // No network for the veto: an error, and D keeps its identity.
        std::fs::write(
            &w.paths.credentials_path,
            creds_json("at-a2", "rt-a2", 4_100_000_000_000),
        )
        .unwrap();
        let stopped = ScriptedLiveness::stopped();
        assert!(matches!(
            attribute_offline(&w.env(&stopped, &http)),
            Err(OrcaError::Network(_))
        ));
        assert_eq!(std::fs::read(&w.paths.config_path).unwrap(), cfg_before);
        // No active account: D is the system default, left alone.
        let recs: Vec<Value> = ["id-a", "id-b"]
            .iter()
            .zip(["alice@example.com", "bob@example.com"])
            .map(|(id, e)| record_json(&w.ud, id, e, None))
            .collect();
        write_store(&w.ud, &recs, None);
        assert_eq!(
            attribute_offline(&w.env(&stopped, &http)).unwrap(),
            Attribution::NoActiveAccount
        );
        assert_eq!(std::fs::read(&w.paths.config_path).unwrap(), cfg_before);
    }

    /// Round 8: attribute_offline's safety branches. Orca coming up during
    /// the read-back leaves `D`'s identity in place (never written behind a
    /// running Orca), and an untested Orca version or a userData that is
    /// not csm's to write refuses with `D` and the stashes unchanged.
    #[test]
    fn offline_attribution_never_writes_d_behind_an_orca_that_came_up() {
        let w = World::new(None);
        let fresher = creds_json("at-a2", "rt-a2", 4_100_000_000_000);
        std::fs::write(&w.paths.credentials_path, &fresher).unwrap();
        std::fs::write(
            &w.paths.config_path,
            json!({"oauthAccount": oauth_json("u-b", "bob@example.com", None)}).to_string(),
        )
        .unwrap();
        let cfg_before = std::fs::read(&w.paths.config_path).unwrap();
        let stash_bytes = |id: &str| {
            Stash::open(&w.ud, id, None)
                .unwrap()
                .credentials(OS)
                .unwrap()
                .map(|s| s.expose().to_owned())
        };
        let (a_before, b_before) = (stash_bytes("id-a"), stash_bytes("id-b"));
        let http = FakeHttp::default().profile_uuid("at-a2", "u-a");
        for (version_ok, store_access_allowed) in [(false, true), (true, false)] {
            let live = ScriptedLiveness::stopped();
            let mut env = w.env(&live, &http);
            env.version_ok = version_ok;
            env.store_access_allowed = store_access_allowed;
            assert!(matches!(
                attribute_offline(&env),
                Err(OrcaError::Refused(_))
            ));
            assert_eq!(std::fs::read(&w.paths.config_path).unwrap(), cfg_before);
            assert_eq!(w.d_creds(), fresher);
            assert_eq!(stash_bytes("id-a"), a_before);
            assert_eq!(stash_bytes("id-b"), b_before);
            assert!(Quarantine::new(OS, &w.state).list().is_empty());
        }
        // Stopped at the first check, running after the read-back.
        let live = ScriptedLiveness::appears_at(1);
        let Attribution::Done {
            identity_cleared, ..
        } = attribute_offline(&w.env(&live, &http)).unwrap()
        else {
            panic!("the store names an active account");
        };
        assert!(!identity_cleared);
        assert_eq!(live.checks(), 2);
        assert_eq!(std::fs::read(&w.paths.config_path).unwrap(), cfg_before);
        assert!(matches!(
            read_runtime_identity(&w.paths),
            RuntimeIdentity::Present(_)
        ));
    }

    #[test]
    fn an_unusable_last_synced_stash_still_gets_a_read_back() {
        let w = World::new(None);
        // a's stash lost its grant; D holds a's rotated one, now its only
        // copy.
        let a_stash = Stash::open(&w.ud, "id-a", None).unwrap();
        std::fs::remove_file(a_stash.auth_dir.join(stash::CREDENTIALS_FILE)).unwrap();
        let rotated = creds_json("at-a2", "rt-a2", 4_100_000_000_000);
        std::fs::write(&w.paths.credentials_path, &rotated).unwrap();
        let live = ScriptedLiveness::stopped();
        let http = FakeHttp::default();
        let r = switch(&w.env(&live, &http), "id-b").unwrap();
        assert_eq!(r.outcome, Outcome::Switched);
        assert_eq!(w.d_creds(), b_creds());
        // The overwritten grant was filed, not dropped.
        let rb = r.readback.expect("the read-back ran");
        assert_eq!(rb.candidates, 1);
        let q = Quarantine::new(OS, &w.state);
        let list = q.list();
        assert_eq!(list.len(), 1, "{list:?}");
        assert_eq!(
            q.get(&list[0].fingerprint).unwrap().unwrap().expose(),
            rotated
        );
    }

    /// A materialize whose write and restore both fail leaves the journal
    /// pending, so a recovery repairs `D` (it used to read `rolled-back`).
    #[cfg(unix)]
    #[test]
    fn a_materialize_that_cannot_restore_d_leaves_the_journal_pending() {
        use crate::orca::testsupport::FakeSecurity;
        let fake = FakeSecurity::install();
        let w = World::new(None);
        for (id, c) in [("id-a", a_creds()), ("id-b", b_creds())] {
            fake.put(keychain::STASH_SERVICE, id, c.as_bytes());
        }
        let dir = w.paths.config_dir.to_string_lossy().into_owned();
        fake.put(
            &keychain::runtime_service(Some(&dir)),
            &w.user.acct,
            a_creds().as_bytes(),
        );
        fake.put(
            keychain::RUNTIME_SERVICE,
            &w.user.acct,
            a_creds().as_bytes(),
        );
        fake.fail_add(keychain::RUNTIME_SERVICE, true);
        let live = ScriptedLiveness::stopped();
        let http = FakeHttp::default();
        let mut env = w.env(&live, &http);
        env.os = HostOs::MacOs;
        let e = switch(&env, "id-b").unwrap_err();
        assert!(e.to_string().contains("doctor --fix"), "{e}");
        let j = read_journal(&w.state).unwrap();
        assert!(j.pending(), "{j:?}");
        assert_eq!(j.step, JournalStep::Materialize);
        assert_eq!(w.store_active().as_deref(), Some("id-a"));
    }

    #[test]
    fn recovery_repairs_to_the_account_the_store_names() {
        let w = World::new(None);
        // A crash mid-materialize: b's grant beside no identity.
        std::fs::write(&w.paths.credentials_path, b_creds()).unwrap();
        std::fs::write(&w.paths.config_path, "{}").unwrap();
        write_journal(
            &w.state,
            &Journal {
                generation: 4,
                account: Some("id-a".into()),
                from: Some("id-a".into()),
                to: "id-b".into(),
                step: JournalStep::Materialize,
                orca_unconfirmed: false,
                owner: Owner {
                    pid: 1,
                    born: Some(1),
                },
            },
        )
        .unwrap();
        let live = ScriptedLiveness::stopped();
        let http = FakeHttp::default();
        match recover(&w.env(&live, &http)).unwrap() {
            Recovery::Repaired(r) => {
                assert_eq!(r.to, "id-a");
                assert_eq!(r.generation, 5);
                assert_eq!(r.steps[0], Step::Recover);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(w.d_creds(), a_creds());
        assert_eq!(d_uuid(&w.paths).as_deref(), Some("u-a"));
        // b's grant equals b's stash, so read-back had nothing to file.
        assert!(Quarantine::new(OS, &w.state).list().is_empty());
        assert!(matches!(
            recover(&w.env(&live, &http)).unwrap(),
            Recovery::Nothing
        ));
    }

    /// A held `switch.lock` is a busy repair, not a failed one: nothing is
    /// attempted, `D` and the journal stay as they are.
    #[test]
    fn recovery_behind_a_held_lock_is_busy_and_touches_nothing() {
        let w = World::new(None);
        std::fs::write(&w.paths.credentials_path, b_creds()).unwrap();
        std::fs::write(&w.paths.config_path, "{}").unwrap();
        let j = Journal {
            generation: 4,
            account: Some("id-a".into()),
            from: Some("id-a".into()),
            to: "id-b".into(),
            step: JournalStep::Materialize,
            orca_unconfirmed: false,
            owner: Owner {
                pid: 1,
                born: Some(1),
            },
        };
        write_journal(&w.state, &j).unwrap();
        let held = SwitchLock::acquire(&w.state, Duration::from_secs(1)).unwrap();
        let live = ScriptedLiveness::stopped();
        let http = FakeHttp::default();
        let mut env = w.env(&live, &http);
        env.timing.lock_wait = Duration::from_millis(50);
        assert!(matches!(recover(&env).unwrap(), Recovery::Busy));
        assert_eq!(w.d_creds(), b_creds());
        assert_eq!(
            read_journal(&w.state).unwrap().step,
            JournalStep::Materialize
        );
        drop(held);
    }

    /// Design §3 safety net: a repair that cannot run leaves `D` neutral (no
    /// `oauthAccount`, other keys kept), closes the journal as rolled back
    /// and never touches the store.
    #[test]
    fn a_failed_recovery_neutralizes_d_and_closes_the_journal() {
        let w = World::new(None);
        // A crash mid-materialize: b's grant and identity in D.
        std::fs::write(&w.paths.credentials_path, b_creds()).unwrap();
        std::fs::write(
            &w.paths.config_path,
            json!({"numStartups": 1, "oauthAccount": oauth_json("u-b", "bob@example.com", None)})
                .to_string(),
        )
        .unwrap();
        // The account the store names has no usable stash: the repair fails.
        let a_stash = Stash::open(&w.ud, "id-a", None).unwrap();
        std::fs::remove_file(a_stash.auth_dir.join(stash::CREDENTIALS_FILE)).unwrap();
        write_journal(
            &w.state,
            &Journal {
                generation: 4,
                account: Some("id-a".into()),
                from: Some("id-a".into()),
                to: "id-b".into(),
                step: JournalStep::Materialize,
                orca_unconfirmed: false,
                owner: Owner {
                    pid: 1,
                    born: Some(1),
                },
            },
        )
        .unwrap();
        let live = ScriptedLiveness::stopped();
        let http = FakeHttp::default();
        match recover(&w.env(&live, &http)).unwrap() {
            Recovery::Failed(why) => assert!(!why.is_empty()),
            other => panic!("{other:?}"),
        }
        let cfg: Value =
            serde_json::from_slice(&std::fs::read(&w.paths.config_path).unwrap()).unwrap();
        assert!(cfg.get("oauthAccount").is_none(), "{cfg}");
        assert_eq!(cfg.get("numStartups"), Some(&json!(1)));
        assert_eq!(d_uuid(&w.paths), None);
        let j = read_journal(&w.state).unwrap();
        assert_eq!(j.step, JournalStep::RolledBack);
        assert!(!j.pending(), "{j:?}");
        assert_eq!(w.store_active().as_deref(), Some("id-a"));
        assert!(matches!(
            recover(&w.env(&live, &http)).unwrap(),
            Recovery::Nothing
        ));
    }

    /// A store with no active account and `D` holding the user's own login
    /// S (the system default): S's grant and identity.
    fn null_world() -> (World, String, Value) {
        null_world_in(None)
    }

    fn null_world_in(ud: Option<&Path>) -> (World, String, Value) {
        let w = World::new(ud);
        let recs: Vec<Value> = ["id-a", "id-b"]
            .iter()
            .zip(["alice@example.com", "bob@example.com"])
            .map(|(id, e)| record_json(&w.ud, id, e, None))
            .collect();
        write_store(&w.ud, &recs, None);
        let s = creds_json("at-s", "rt-s", 4_000_000_000_000);
        let so = oauth_json("u-s", "sam@example.com", None);
        std::fs::write(&w.paths.credentials_path, &s).unwrap();
        std::fs::write(
            &w.paths.config_path,
            json!({"numStartups": 1, "oauthAccount": so}).to_string(),
        )
        .unwrap();
        (w, s, so)
    }

    /// A switch from the system default to b that died right after the
    /// materialize wrote `D/.credentials.json` (neutral window: identity
    /// cleared, b's grant written, store untouched, journal pending).
    fn crash_after_write_file(w: &World, step: JournalStep) {
        sysdefault::capture_for_managed_entry(&w.ud, OS, &w.paths, &w.user, &b_creds(), 1).unwrap();
        runtime::clear_identity(&w.paths).unwrap();
        std::fs::write(&w.paths.credentials_path, b_creds()).unwrap();
        write_journal(
            &w.state,
            &Journal {
                generation: 0,
                account: None,
                from: None,
                to: "id-b".into(),
                step,
                orca_unconfirmed: false,
                owner: Owner {
                    pid: 1,
                    born: Some(1),
                },
            },
        )
        .unwrap();
    }

    fn snapshot_holds(w: &World, s: &str, so: &Value) {
        let snap = sysdefault::read_snapshot(&w.ud).unwrap();
        assert_eq!(snap["credentialsJson"], json!(s));
        assert_eq!(&snap["configOauthAccount"], so);
    }

    #[test]
    fn a_crashed_switch_from_the_system_default_is_put_back_and_survives_the_next_switch() {
        let (w, s, so) = null_world();
        crash_after_write_file(&w, JournalStep::Refresh);
        let live = ScriptedLiveness::stopped();
        let http = FakeHttp::default();
        match recover(&w.env(&live, &http)).unwrap() {
            Recovery::RestoredSystemDefault(r) => {
                assert!(r.had_snapshot);
                assert_eq!(r.restored, vec!["file".to_owned()]);
                // b's grant is b's stash's: nothing to quarantine.
                assert!(r.quarantined.is_empty(), "{r:?}");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(w.d_creds(), s);
        assert_eq!(d_uuid(&w.paths).as_deref(), Some("u-s"));
        let j = read_journal(&w.state).unwrap();
        assert_eq!(j.step, JournalStep::RolledBack);
        assert_eq!(w.store_active(), None);
        // The next switch captures S again, not b's grant.
        let r = switch(&w.env(&live, &http), "id-a").unwrap();
        assert_eq!(r.outcome, Outcome::Switched);
        snapshot_holds(&w, &s, &so);
        assert_eq!(w.d_creds(), a_creds());
    }

    /// A dangling active id (a record Orca no longer has) is no account for
    /// recovery either: the crashed switch from the system default is put
    /// back, not repaired to a missing record and then neutralized.
    #[test]
    fn a_dangling_active_id_settles_like_no_active_account() {
        let (w, s, _so) = null_world();
        let recs: Vec<Value> = ["id-a", "id-b"]
            .iter()
            .zip(["alice@example.com", "bob@example.com"])
            .map(|(id, e)| record_json(&w.ud, id, e, None))
            .collect();
        write_store(&w.ud, &recs, Some("id-gone"));
        crash_after_write_file(&w, JournalStep::Refresh);
        let live = ScriptedLiveness::stopped();
        let http = FakeHttp::default();
        match recover(&w.env(&live, &http)).unwrap() {
            Recovery::RestoredSystemDefault(r) => assert!(r.had_snapshot, "{r:?}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(w.d_creds(), s);
        assert_eq!(d_uuid(&w.paths).as_deref(), Some("u-s"));
        assert_eq!(
            read_journal(&w.state).unwrap().step,
            JournalStep::RolledBack
        );
    }

    fn pending_a_to_b(w: &World) {
        write_journal(
            &w.state,
            &Journal {
                generation: 2,
                account: Some("id-a".into()),
                from: Some("id-a".into()),
                to: "id-b".into(),
                step: JournalStep::Materialize,
                orca_unconfirmed: false,
                owner: Owner {
                    pid: 1,
                    born: Some(1),
                },
            },
        )
        .unwrap();
    }

    /// A switch a -> b died; Orca then deselected a and put the system
    /// default S back into `D`. Settling keeps S's identity, so the next
    /// switch's forced capture records S's `oauthAccount`, not a null one.
    #[test]
    fn a_crashed_switch_between_accounts_keeps_the_system_default_orca_restored() {
        let (w, s, so) = null_world();
        pending_a_to_b(&w);
        let live = ScriptedLiveness::stopped();
        let http = FakeHttp::default();
        assert!(matches!(
            recover(&w.env(&live, &http)).unwrap(),
            Recovery::KeptSystemDefault
        ));
        assert_eq!(d_uuid(&w.paths).as_deref(), Some("u-s"));
        assert_eq!(w.d_creds(), s);
        assert!(!read_journal(&w.state).unwrap().pending());
        let r = switch(&w.env(&live, &http), "id-a").unwrap();
        assert_eq!(r.outcome, Outcome::Switched);
        snapshot_holds(&w, &s, &so);
    }

    /// The same crash with `D` still holding one side's identity (no Orca
    /// in between to restore anything) is still made neutral.
    #[test]
    fn a_crashed_switch_between_accounts_still_neutralizes_its_own_identity() {
        let (w, _s, _so) = null_world();
        std::fs::write(&w.paths.credentials_path, b_creds()).unwrap();
        std::fs::write(
            &w.paths.config_path,
            json!({"numStartups": 1, "oauthAccount": oauth_json("u-b", "bob@example.com", None)})
                .to_string(),
        )
        .unwrap();
        pending_a_to_b(&w);
        let live = ScriptedLiveness::stopped();
        let http = FakeHttp::default();
        assert!(matches!(
            recover(&w.env(&live, &http)).unwrap(),
            Recovery::Neutralized
        ));
        assert_eq!(d_uuid(&w.paths), None);
        assert!(!read_journal(&w.state).unwrap().pending());
    }

    #[test]
    fn identity_left_by_switch_is_none_or_a_party() {
        let parties = vec!["u-a".to_owned(), "u-b".to_owned()];
        assert!(identity_left_by_switch(None, &parties));
        assert!(identity_left_by_switch(Some("u-a"), &parties));
        assert!(identity_left_by_switch(Some("u-b"), &parties));
        assert!(!identity_left_by_switch(Some("u-s"), &parties));
        assert!(!identity_left_by_switch(Some("u-s"), &[]));
    }

    #[test]
    fn the_next_switch_settles_a_crashed_switch_from_the_system_default_first() {
        let (w, s, so) = null_world();
        crash_after_write_file(&w, JournalStep::CaptureSnapshot);
        let live = ScriptedLiveness::stopped();
        let http = FakeHttp::default();
        let r = switch(&w.env(&live, &http), "id-a").unwrap();
        assert_eq!(r.outcome, Outcome::Switched);
        assert!(!r.steps.contains(&Step::Recover), "{:?}", r.steps);
        snapshot_holds(&w, &s, &so);
        assert_eq!(w.store_active().as_deref(), Some("id-a"));
    }

    #[test]
    fn a_foreign_grant_left_after_the_crash_is_quarantined_before_the_restore() {
        let (w, s, so) = null_world();
        crash_after_write_file(&w, JournalStep::Materialize);
        // A claude run after the crash rotated b's grant in D: no stash
        // holds the result.
        let rotated = creds_json("at-b9", "rt-b9", 4_100_000_000_000);
        std::fs::write(&w.paths.credentials_path, &rotated).unwrap();
        let live = ScriptedLiveness::stopped();
        let http = FakeHttp::default();
        match recover(&w.env(&live, &http)).unwrap() {
            Recovery::RestoredSystemDefault(r) => assert_eq!(r.quarantined.len(), 1),
            other => panic!("{other:?}"),
        }
        assert_eq!(w.d_creds(), s);
        let q = Quarantine::new(OS, &w.state);
        let list = q.list();
        assert_eq!(list.len(), 1, "{list:?}");
        assert_eq!(
            list[0].reason,
            crate::orca::quarantine::Reason::CrashRecovery
        );
        assert_eq!(
            q.get(&list[0].fingerprint).unwrap().unwrap().expose(),
            rotated
        );
        snapshot_holds(&w, &s, &so);
    }

    #[test]
    fn a_newer_system_default_grant_is_kept_when_the_crashed_switch_never_wrote_d() {
        let (w, s, so) = null_world();
        // The switch died in its refresh: the snapshot holds S, D is
        // untouched. A plain claude run since refreshed S to S2, which
        // rotated S's refresh token.
        crash_after_write_file(&w, JournalStep::CaptureSnapshot);
        std::fs::write(
            &w.paths.config_path,
            json!({"numStartups": 1, "oauthAccount": so}).to_string(),
        )
        .unwrap();
        let s2 = creds_json("at-s2", "rt-s2", 4_200_000_000_000);
        std::fs::write(&w.paths.credentials_path, &s2).unwrap();
        let live = ScriptedLiveness::stopped();
        let http = FakeHttp::default();
        match recover(&w.env(&live, &http)).unwrap() {
            Recovery::RestoredSystemDefault(r) => {
                assert!(r.restored.is_empty(), "{r:?}");
                assert_eq!(r.kept, vec!["file".to_owned()]);
                assert_eq!(r.quarantined.len(), 1, "{r:?}");
            }
            other => panic!("{other:?}"),
        }
        // D keeps the live grant and its own identity.
        assert_eq!(w.d_creds(), s2);
        assert_eq!(d_uuid(&w.paths).as_deref(), Some("u-s"));
        // The stale snapshot copy is filed, so the next capture cannot lose
        // it.
        let q = Quarantine::new(OS, &w.state);
        let list = q.list();
        assert_eq!(list.len(), 1, "{list:?}");
        assert_eq!(q.get(&list[0].fingerprint).unwrap().unwrap().expose(), s);
        assert!(!read_journal(&w.state).unwrap().pending());
    }

    #[test]
    fn recovery_with_orca_on_another_d_leaves_the_journal_pending() {
        let w = World::new(None);
        let j = Journal {
            generation: 3,
            account: Some("id-a".into()),
            from: Some("id-a".into()),
            to: "id-b".into(),
            step: JournalStep::Materialize,
            orca_unconfirmed: false,
            owner: Owner {
                pid: 1,
                born: Some(1),
            },
        };
        write_journal(&w.state, &j).unwrap();
        std::fs::write(&w.paths.credentials_path, b_creds()).unwrap();
        let live = ScriptedLiveness::new(vec![crate::orca::testsupport::running_mark()]);
        let http = FakeHttp::default();
        for agrees in [Some(false), None] {
            let f = move || agrees;
            let mut env = w.env(&live, &http);
            env.orca_dir_agrees = &f;
            match recover(&env).unwrap() {
                Recovery::Deferred(why) => assert!(why.contains("CLAUDE_CONFIG_DIR"), "{why}"),
                other => panic!("{other:?}"),
            }
            assert_eq!(read_journal(&w.state).unwrap(), j);
            assert_eq!(w.d_creds(), b_creds());
        }
        // Orca's D is csm's: its own sync owns D, the intent is cleared.
        assert!(matches!(
            recover(&w.env(&live, &http)).unwrap(),
            Recovery::ClearedForOrca
        ));
        assert_eq!(
            read_journal(&w.state).unwrap().step,
            JournalStep::HandedToOrca
        );
    }

    /// A hand-over that kept its journal pending (D could not be put back,
    /// Orca did not confirm a select that rewrote it) is not cleared for
    /// Orca's sync while Orca runs, even with Orca on csm's D: Orca believes
    /// D is in sync, so only csm's offline repair fixes it.
    #[test]
    fn an_unconfirmed_hand_over_waits_for_the_offline_repair() {
        let w = World::new(None);
        let j = Journal {
            generation: 3,
            account: Some("id-a".into()),
            from: Some("id-a".into()),
            to: "id-b".into(),
            step: JournalStep::Store,
            orca_unconfirmed: true,
            owner: Owner {
                pid: 1,
                born: Some(1),
            },
        };
        write_journal(&w.state, &j).unwrap();
        std::fs::write(&w.paths.credentials_path, b_creds()).unwrap();
        std::fs::write(&w.paths.config_path, "{}").unwrap();
        let live = ScriptedLiveness::new(vec![crate::orca::testsupport::running_mark()]);
        let http = FakeHttp::default();
        match recover(&w.env(&live, &http)).unwrap() {
            Recovery::Deferred(why) => assert!(why.contains("once Orca stops"), "{why}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(read_journal(&w.state).unwrap(), j);
        assert!(read_journal(&w.state).unwrap().pending());
        assert_eq!(w.d_creds(), b_creds());
        // Once Orca stops the offline repair runs and a fresh journal
        // (without the mark) replaces it.
        let stopped = ScriptedLiveness::stopped();
        assert!(matches!(
            recover(&w.env(&stopped, &http)).unwrap(),
            Recovery::Repaired(_)
        ));
        let after = read_journal(&w.state).unwrap();
        assert!(!after.pending() && !after.orca_unconfirmed, "{after:?}");
        assert_eq!(w.d_creds(), a_creds());
        // A journal written before the field existed reads as unmarked.
        let old = br#"{"gen":1,"account":null,"from":null,"to":"id-b","step":"store","owner":{"pid":1,"born":1}}"#;
        let j: Journal = serde_json::from_slice(old).unwrap();
        assert!(!j.orca_unconfirmed && j.pending());
        let text = serde_json::to_string(&after).unwrap();
        assert!(!text.contains("orca_unconfirmed"), "{text}");
    }

    #[test]
    fn a_switch_from_the_system_default_that_died_before_the_snapshot_leaves_d_alone() {
        let (w, s, _so) = null_world();
        write_journal(
            &w.state,
            &Journal {
                generation: 0,
                account: None,
                from: None,
                to: "id-b".into(),
                step: JournalStep::LoadTarget,
                orca_unconfirmed: false,
                owner: Owner {
                    pid: 1,
                    born: Some(1),
                },
            },
        )
        .unwrap();
        let live = ScriptedLiveness::stopped();
        let http = FakeHttp::default();
        assert!(matches!(
            recover(&w.env(&live, &http)).unwrap(),
            Recovery::Untouched
        ));
        // The system default's identity is kept (clearing it would lose it).
        assert_eq!(w.d_creds(), s);
        assert_eq!(d_uuid(&w.paths).as_deref(), Some("u-s"));
        assert!(!read_journal(&w.state).unwrap().pending());
        assert!(null_switch_touched_d(JournalStep::CaptureSnapshot));
        assert!(!null_switch_touched_d(JournalStep::Started));
    }

    /// Orca comes up between recover's own liveness check and the switch's
    /// L0; the switch's first RPC then fails (no socket yet). The repair
    /// must not clear `D`'s identity behind the running Orca, and the
    /// journal stays pending for a repair once Orca stops.
    #[test]
    fn recovery_never_neutralizes_d_behind_an_orca_that_came_up() {
        let w = World::new(None);
        std::fs::write(&w.paths.credentials_path, b_creds()).unwrap();
        std::fs::write(
            &w.paths.config_path,
            json!({"oauthAccount": oauth_json("u-b", "bob@example.com", None)}).to_string(),
        )
        .unwrap();
        let j = Journal {
            generation: 4,
            account: Some("id-a".into()),
            from: Some("id-a".into()),
            to: "id-b".into(),
            step: JournalStep::Materialize,
            orca_unconfirmed: false,
            owner: Owner {
                pid: 1,
                born: Some(1),
            },
        };
        write_journal(&w.state, &j).unwrap();
        // Check 0: recover's own (stopped); check 1: the switch's L0.
        let live = ScriptedLiveness::appears_at(1);
        let http = FakeHttp::default();
        match recover(&w.env(&live, &http)).unwrap() {
            Recovery::Deferred(why) => assert!(why.contains("Orca started"), "{why}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(read_journal(&w.state).unwrap(), j);
        assert_eq!(w.d_creds(), b_creds());
        assert_eq!(d_uuid(&w.paths).as_deref(), Some("u-b"));
    }

    #[test]
    fn an_uncertain_hand_over_is_not_a_repair() {
        let report = |outcome| SwitchReport {
            route: Route::OfflineThenRpc,
            outcome,
            to: "id-a".into(),
            generation: 1,
            steps: Vec::new(),
            readback: None,
            snapshot: None,
            refresh: None,
            redo: None,
        };
        match repaired(report(Outcome::Uncertain("no answer".into()))) {
            Recovery::Uncertain(why) => assert_eq!(why, "no answer"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            repaired(report(Outcome::Switched)),
            Recovery::Repaired(_)
        ));
        assert!(matches!(
            repaired(report(Outcome::AlreadyActive)),
            Recovery::Repaired(_)
        ));
        assert!(matches!(orca_came_up(true, "x"), Recovery::Deferred(_)));
        assert!(matches!(orca_came_up(false, "x"), Recovery::Uncertain(_)));
    }

    /// `D` left half written at L0/L1 keeps the journal pending unless
    /// Orca's own select (which rewrites `D`) was confirmed.
    #[test]
    fn an_unrestored_d_keeps_the_journal_pending_unless_orca_switched() {
        let ok = RedoOutcome::Reissued(json!({}));
        let done = RedoOutcome::AlreadyDone(rpc::ClaudeSnapshot {
            accounts: Vec::new(),
            active_account_id: Some("id-b".into()),
            active_by_runtime: Default::default(),
        });
        let failed = RedoOutcome::Failed("busy".into());
        let silent = RedoOutcome::Uncertain("no answer".into());
        let p = |r: &RedoOutcome, u: bool| handed_leaves_pending(redo_switched(r), u);
        assert!(!p(&ok, true));
        assert!(!p(&done, true));
        assert!(p(&failed, true));
        assert!(p(&silent, true));
        for r in [&ok, &done, &failed, &silent] {
            assert!(!p(r, false));
        }
    }

    /// The hand-over closes the journal only when Orca's select reaches
    /// csm's `D` or `D` is back to a clean state. A `D` put back to the
    /// state an earlier crash left reopens that crash's journal unless
    /// Orca's reissued select rewrote it; an Orca on another runtime dir
    /// never closes an L0/L1 hand-over, and neither reports a switch.
    #[test]
    fn a_handover_closes_the_journal_only_when_d_is_settled() {
        let base = HandoverFacts {
            dir_agrees: true,
            redo_switched: true,
            reissued: true,
            ..Default::default()
        };
        // The plain case: Orca took the select in csm's D.
        let h = handover(&base);
        assert_eq!(h.journal, HandoverJournal::Close);
        assert!(h.bump && h.not_switched.is_none());
        // Orca already showed the target, D back to a crashed state.
        let crashed = HandoverFacts {
            reissued: false,
            prev_pending: true,
            ..base
        };
        let h = handover(&crashed);
        assert_eq!(h.journal, HandoverJournal::Reopen);
        assert!(!h.bump && h.not_switched.is_some());
        // The same with a refused or unanswered redo.
        let h = handover(&HandoverFacts {
            redo_switched: false,
            ..crashed
        });
        assert_eq!(h.journal, HandoverJournal::Reopen);
        // Orca's reissued select after the restore rewrote D.
        assert_eq!(
            handover(&HandoverFacts {
                prev_pending: true,
                ..base
            })
            .journal,
            HandoverJournal::Close
        );
        // Orca on another (or an unreadable) runtime dir.
        for prev_pending in [false, true] {
            let h = handover(&HandoverFacts {
                dir_agrees: false,
                prev_pending,
                ..base
            });
            assert_ne!(h.journal, HandoverJournal::Close, "{prev_pending}");
            assert!(!h.bump && h.not_switched.is_some());
        }
        // At L2 D holds the target as written: no reopen.
        let h = handover(&HandoverFacts {
            at_l2: true,
            dir_agrees: false,
            prev_pending: true,
            reissued: false,
            ..base
        });
        assert_eq!(h.journal, HandoverJournal::Close);
        assert!(h.not_switched.is_none());
        // A D that could not be put back and no confirmed select: pending.
        let h = handover(&HandoverFacts {
            redo_switched: false,
            reissued: false,
            d_unrestored: true,
            ..base
        });
        assert_eq!(h.journal, HandoverJournal::KeepPending);
    }

    #[test]
    fn a_failed_restore_of_d_is_never_dropped_when_orca_takes_over() {
        let ok = RedoOutcome::Reissued(json!({}));
        assert_eq!(handed_outcome(&ok, None).unwrap(), Outcome::Switched);
        let u = "D could not be restored (.credentials.json)";
        match handed_outcome(&ok, Some(u)).unwrap() {
            Outcome::Uncertain(why) => assert!(why.contains(u), "{why}"),
            o => panic!("{o:?}"),
        }
        match handed_outcome(&RedoOutcome::Uncertain("no answer".into()), Some(u)).unwrap() {
            Outcome::Uncertain(why) => {
                assert!(why.contains("no answer") && why.contains(u), "{why}")
            }
            o => panic!("{o:?}"),
        }
        assert!(redo_switched(&ok));
        assert!(!redo_switched(&RedoOutcome::Failed("busy".into())));
        assert!(!redo_switched(&RedoOutcome::Uncertain("no answer".into())));
        let e = handed_outcome(&RedoOutcome::Failed("busy".into()), Some(u))
            .unwrap_err()
            .to_string();
        assert!(e.contains("busy") && e.contains(u), "{e}");
    }

    #[cfg(unix)]
    mod with_orca {
        use super::*;
        use crate::orca::testsupport::{FakeOrca, OrcaModel, model_handler, running_mark};
        use std::sync::{Arc, Mutex};

        #[test]
        fn orca_appearing_at_l1_restores_d_and_redoes_over_rpc() {
            let model = Arc::new(Mutex::new(OrcaModel::default()));
            let fake = FakeOrca::start(model_handler(model.clone()));
            let w = World::new(Some(fake.user_data()));
            {
                let mut m = model.lock().unwrap();
                m.accounts = vec![record_json(&w.ud, "id-a", "alice@example.com", None)];
                m.active = Some("id-a".into());
            }
            // Checks: 0 = switch L0, 1 = protocol L0, 2 = L1 (running).
            let live = ScriptedLiveness::appears_at(2);
            let r = switch(&w.env(&live, &FakeHttp::default()), "id-b").unwrap();
            assert_eq!(r.route, Route::OfflineThenRpc);
            assert!(matches!(r.redo, Some(RedoOutcome::Reissued(_))));
            assert_eq!(model.lock().unwrap().active.as_deref(), Some("id-b"));
            // Store untouched, D back to a.
            assert_eq!(w.store_active().as_deref(), Some("id-a"));
            assert_eq!(w.d_creds(), a_creds());
            assert_eq!(d_uuid(&w.paths).as_deref(), Some("u-a"));
            // Orca confirmed the target: the generation moved, so peers of a
            // limit switch follow instead of leading a second switch.
            assert_eq!(r.generation, 1);
            let j = read_journal(&w.state).unwrap();
            assert_eq!(
                (j.step, j.generation, j.account.as_deref()),
                (JournalStep::HandedToOrca, 1, Some("id-b"))
            );
        }

        #[test]
        fn orca_appearing_at_l2_leaves_d_and_redoes() {
            let model = Arc::new(Mutex::new(OrcaModel::default()));
            let fake = FakeOrca::start(model_handler(model.clone()));
            let w = World::new(Some(fake.user_data()));
            model.lock().unwrap().active = Some("id-a".into());
            let live = ScriptedLiveness::appears_at(3);
            let r = switch(&w.env(&live, &FakeHttp::default()), "id-b").unwrap();
            assert_eq!(r.route, Route::OfflineThenRpc);
            assert_eq!(w.store_active().as_deref(), Some("id-b"));
            assert_eq!(w.d_creds(), b_creds());
            assert_eq!(model.lock().unwrap().active.as_deref(), Some("id-b"));
            assert_eq!(r.generation, 1);
            assert_eq!(read_journal(&w.state).unwrap().generation, 1);
        }

        /// A running fake Orca that names no account, over a store with no
        /// active account and a switch from the system default to b that
        /// died after writing b's grant into `D`.
        fn running_null_world() -> (FakeOrca, World, String, Value, Arc<Mutex<OrcaModel>>) {
            let model = Arc::new(Mutex::new(OrcaModel::default()));
            let fake = FakeOrca::start(model_handler(model.clone()));
            let (w, s, so) = null_world_in(Some(fake.user_data()));
            model.lock().unwrap().accounts = ["id-a", "id-b"]
                .iter()
                .zip(["alice@example.com", "bob@example.com"])
                .map(|(id, e)| record_json(&w.ud, id, e, None))
                .collect();
            crash_after_write_file(&w, JournalStep::Materialize);
            (fake, w, s, so, model)
        }

        /// Invariant 6: with Orca running csm writes nothing to `D`, even to
        /// put the system default back. It files the snapshot's grant in
        /// the quarantine (so Orca's next select, capturing the half-written
        /// `D`, cannot lose the user's login) and defers the repair; once
        /// Orca stops, the offline recovery puts `D` back.
        #[test]
        fn a_running_orca_with_no_account_defers_and_keeps_the_system_default() {
            let (fake, w, s, so, _model) = running_null_world();
            let live = ScriptedLiveness::new(vec![running_mark()]);
            let http = FakeHttp::default();
            match recover(&w.env(&live, &http)).unwrap() {
                Recovery::Deferred(why) => assert!(why.contains("quarantine"), "{why}"),
                other => panic!("{other:?}"),
            }
            assert_eq!(w.d_creds(), b_creds(), "D is not written");
            assert_eq!(d_uuid(&w.paths), None);
            snapshot_holds(&w, &s, &so);
            assert!(read_journal(&w.state).unwrap().pending());
            let q = Quarantine::new(OS, &w.state);
            let fp = crate::orca::quarantine::fingerprint(&s);
            assert_eq!(q.get(&fp).unwrap().unwrap().expose(), s);
            assert!(
                fake.requests()
                    .iter()
                    .all(|r| r["method"] != "accounts.selectClaude")
            );
            // Orca stops: the offline recovery restores the system default.
            let live = ScriptedLiveness::stopped();
            match recover(&w.env(&live, &http)).unwrap() {
                Recovery::RestoredSystemDefault(r) => {
                    assert_eq!(r.restored, vec!["file".to_owned()])
                }
                other => panic!("{other:?}"),
            }
            assert_eq!(w.d_creds(), s);
            assert_eq!(d_uuid(&w.paths).as_deref(), Some("u-s"));
            assert!(!read_journal(&w.state).unwrap().pending());
        }

        /// Orca runs but its `accounts.list` fails (still starting, a busy
        /// socket): csm cannot tell whether it selected an account, and a
        /// GUI select right after would capture the half-written `D` over
        /// the snapshot. The snapshot's grant is kept either way, by the
        /// recovery and by an RPC switch that stops at the failed list.
        #[test]
        fn a_running_orca_that_does_not_list_still_keeps_the_system_default() {
            let fake = FakeOrca::start(|_: &Value| vec![FakeOrca::err("busy", "not ready")]);
            let (w, s, so) = null_world_in(Some(fake.user_data()));
            crash_after_write_file(&w, JournalStep::Materialize);
            let live = ScriptedLiveness::new(vec![running_mark()]);
            let http = FakeHttp::default();
            let fp = crate::orca::quarantine::fingerprint(&s);
            match recover(&w.env(&live, &http)).unwrap() {
                Recovery::Deferred(why) => {
                    assert!(why.contains("does not say") && why.contains(&fp), "{why}")
                }
                other => panic!("{other:?}"),
            }
            assert_eq!(w.d_creds(), b_creds(), "D is not written");
            snapshot_holds(&w, &s, &so);
            assert!(read_journal(&w.state).unwrap().pending());
            let q = Quarantine::new(OS, &w.state);
            assert_eq!(q.get(&fp).unwrap().unwrap().expose(), s);
            // The same through an RPC switch: nothing selected, still kept.
            q.remove(&fp).unwrap();
            assert!(switch(&w.env(&live, &http), "id-a").is_err());
            assert_eq!(q.get(&fp).unwrap().unwrap().expose(), s);
            assert!(
                fake.requests()
                    .iter()
                    .all(|r| r["method"] != "accounts.selectClaude")
            );
            assert!(read_journal(&w.state).unwrap().pending());
        }

        #[test]
        fn a_running_orca_that_selected_an_account_since_keeps_the_journal() {
            let (fake, w, _s, _so, model) = running_null_world();
            model.lock().unwrap().active = Some("id-a".into());
            let live = ScriptedLiveness::new(vec![running_mark()]);
            let http = FakeHttp::default();
            match recover(&w.env(&live, &http)).unwrap() {
                Recovery::Deferred(why) => assert!(why.contains("id-a"), "{why}"),
                other => panic!("{other:?}"),
            }
            assert!(read_journal(&w.state).unwrap().pending());
            assert_eq!(w.d_creds(), b_creds());
            // A switch over RPC refuses rather than make Orca capture D.
            let err = switch(&w.env(&live, &http), "id-b").unwrap_err();
            assert!(err.to_string().contains("system default"), "{err}");
            assert!(
                fake.requests()
                    .iter()
                    .all(|r| r["method"] != "accounts.selectClaude")
            );
        }

        #[test]
        fn an_rpc_switch_over_a_crashed_switch_from_the_system_default_refuses() {
            let (fake, w, s, so, _model) = running_null_world();
            let live = ScriptedLiveness::new(vec![running_mark()]);
            let http = FakeHttp::default();
            // Selecting would make Orca capture the half-written D over the
            // snapshot, and csm may not put D back behind a running Orca:
            // the switch is refused, the snapshot's grant is quarantined.
            let err = switch(&w.env(&live, &http), "id-a").unwrap_err();
            assert!(err.to_string().contains("doctor --fix"), "{err}");
            assert!(
                fake.requests()
                    .iter()
                    .all(|r| r["method"] != "accounts.selectClaude")
            );
            assert_eq!(w.d_creds(), b_creds());
            snapshot_holds(&w, &s, &so);
            assert!(read_journal(&w.state).is_some_and(|j| j.pending()));
            let fp = crate::orca::quarantine::fingerprint(&s);
            assert!(
                Quarantine::new(OS, &w.state)
                    .list()
                    .iter()
                    .any(|m| m.fingerprint == fp)
            );
        }

        fn running_world(materialize_on_select: bool) -> (FakeOrca, World, Arc<Mutex<OrcaModel>>) {
            let model = Arc::new(Mutex::new(OrcaModel::default()));
            let d_cfg: Arc<Mutex<Option<PathBuf>>> = Arc::new(Mutex::new(None));
            let inner = model_handler(model.clone());
            let cfg = d_cfg.clone();
            let fake = FakeOrca::start(move |req: &Value| {
                if materialize_on_select
                    && req["method"] == "accounts.selectClaude"
                    && let Some(p) = cfg.lock().unwrap().as_ref()
                {
                    let id = req["params"]["accountId"].as_str().unwrap_or("");
                    let uuid = if id == "id-b" { "u-b" } else { "u-a" };
                    std::fs::write(
                        p,
                        json!({"oauthAccount": {"accountUuid": uuid}}).to_string(),
                    )
                    .unwrap();
                }
                inner(req)
            });
            let w = World::new(Some(fake.user_data()));
            *d_cfg.lock().unwrap() = Some(w.paths.config_path.clone());
            {
                let mut m = model.lock().unwrap();
                m.accounts = ["id-a", "id-b"]
                    .iter()
                    .zip(["alice@example.com", "bob@example.com"])
                    .map(|(id, e)| record_json(&w.ud, id, e, None))
                    .collect();
                m.active = Some("id-a".into());
            }
            (fake, w, model)
        }

        #[test]
        fn a_running_orca_switches_over_rpc_and_is_verified() {
            let (_fake, w, model) = running_world(true);
            let live = ScriptedLiveness::new(vec![running_mark()]);
            let r = switch(&w.env(&live, &FakeHttp::default()), "id-b").unwrap();
            assert_eq!((r.route, r.generation), (Route::Rpc, 1));
            assert_eq!(model.lock().unwrap().active.as_deref(), Some("id-b"));
            // csm wrote nothing of Orca's: the store and D's grant as before.
            assert_eq!(w.store_active().as_deref(), Some("id-a"));
            assert_eq!(w.d_creds(), a_creds());
        }

        #[test]
        fn an_rpc_switch_whose_d_did_not_follow_is_a_failure() {
            let (_fake, w, _model) = running_world(false);
            let live = ScriptedLiveness::new(vec![running_mark()]);
            let err = switch(&w.env(&live, &FakeHttp::default()), "id-b").unwrap_err();
            assert!(err.to_string().contains("did not take effect"), "{err}");
            assert!(read_journal(&w.state).is_none());
        }

        #[test]
        fn a_running_orca_with_another_d_is_refused() {
            let (fake, w, _model) = running_world(true);
            let live = ScriptedLiveness::new(vec![running_mark()]);
            let http = FakeHttp::default();
            let mut env = w.env(&live, &http);
            env.orca_dir_agrees = &|| None;
            assert!(matches!(switch(&env, "id-b"), Err(OrcaError::Refused(_))));
            assert!(
                fake.requests()
                    .iter()
                    .all(|r| r["method"] != "accounts.selectClaude")
            );
        }

        #[test]
        fn switch_in_progress_is_retried() {
            let model = Arc::new(Mutex::new(OrcaModel::default()));
            let busy = Arc::new(Mutex::new(2u32));
            let inner = model_handler(model.clone());
            let b = busy.clone();
            let d_cfg: Arc<Mutex<Option<PathBuf>>> = Arc::new(Mutex::new(None));
            let cfg = d_cfg.clone();
            let fake = FakeOrca::start(move |req: &Value| {
                if req["method"] == "accounts.selectClaude" {
                    let mut n = b.lock().unwrap();
                    if *n > 0 {
                        *n -= 1;
                        return vec![FakeOrca::err(
                            "internal",
                            "A Claude account switch is already in progress.",
                        )];
                    }
                    if let Some(p) = cfg.lock().unwrap().as_ref() {
                        std::fs::write(
                            p,
                            json!({"oauthAccount": {"accountUuid": "u-b"}}).to_string(),
                        )
                        .unwrap();
                    }
                }
                inner(req)
            });
            let w = World::new(Some(fake.user_data()));
            *d_cfg.lock().unwrap() = Some(w.paths.config_path.clone());
            model.lock().unwrap().accounts =
                vec![record_json(&w.ud, "id-b", "bob@example.com", None)];
            let live = ScriptedLiveness::new(vec![running_mark()]);
            switch(&w.env(&live, &FakeHttp::default()), "id-b").unwrap();
            assert_eq!(*busy.lock().unwrap(), 0);
        }
    }
}
