//! Orca glue for csm's everyday commands, outside `csm orca …` itself.
//!
//! - [`offline_view`]: the slot plus Orca's last persisted selection and the
//!   bindings computed from it. Display and the hook's target filter only;
//!   it never opens Orca's socket (so the hook, the statusline and `csm
//!   usage` stay fast and silent).
//! - [`sync_default_change`]: csm → Orca after an explicit default change
//!   (`csm profiles use`, `csm cas use`, eval `cas -g`, the editor's
//!   set-default). Warnings only; the command's own exit status never
//!   depends on Orca.
//! - [`follow_switch`]: the relaunch supervisor's opt-in `orca.followSwitch`
//!   select after an ordinary limit switch.
//!
//! Each I/O shell is a thin wrapper over a pure planner ([`plan_default_sync`],
//! [`plan_follow_switch`], [`slot_follow_label`], [`slot_users`]) that the
//! tests pin down. Every select goes through [`super::select_account`], so
//! the runtime-dir check always runs first.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::bind::{self, Bindings};
use super::pending::{self, PendingSelect};
use super::slot::{self, Slot};
use super::{OrcaState, SelectError, SelectOutcome, Selection};
use crate::account::ProfileMap;
use crate::cas::platform::dirs_equal;
use crate::config::Config;

/// Budget for an explicit default change's Orca round trip (read + select).
pub const EXPLICIT_SELECT_BUDGET: Duration = Duration::from_millis(1500);

/// Budget for the relaunch supervisor's followSwitch round trip.
#[cfg_attr(windows, allow(dead_code))] // the relaunch supervisor is unix-only
pub const FOLLOW_SWITCH_BUDGET: Duration = Duration::from_millis(1500);

// ─── offline view (display + hook) ────────────────────────────────────────────

/// Orca mode's slot plus what Orca last persisted, read without the socket.
#[derive(Debug, Clone)]
pub struct OfflineView {
    pub slot: Slot,
    /// `orca-data.json`'s selection (`None` when absent or unreadable).
    pub selection: Option<Selection>,
    /// Bindings computed against [`Self::selection`].
    pub bindings: Option<Bindings>,
    /// csm's default profile (the tie-break preference for the active account).
    pub default_name: String,
}

impl OfflineView {
    /// The slot row's follow label for `csm usage`.
    pub fn slot_follow(&self) -> String {
        slot_follow_label(
            self.selection.as_ref(),
            self.bindings.as_ref(),
            Some(&self.default_name),
        )
    }

    /// The email of the Orca account bound to `profile`, if any.
    pub fn email_for(&self, profile: &str) -> Option<&str> {
        let id = self.bindings.as_ref()?.account_for_profile(profile)?;
        self.selection
            .as_ref()?
            .account(id)
            .map(|a| a.email.as_str())
            .filter(|e| !e.is_empty())
    }

    /// Every profile sharing the identity of Orca's (persisted) active
    /// account. The hook skips these when the slot hits a limit: switching to
    /// one of them lands on the very account that is limited.
    pub fn active_identity_profiles(&self) -> Vec<String> {
        match (
            self.selection
                .as_ref()
                .and_then(Selection::effective_active_id),
            &self.bindings,
        ) {
            (Some(id), Some(b)) => b.profiles_sharing_identity(id),
            _ => Vec::new(),
        }
    }
}

/// Load the offline view for `profiles`, or `None` when Orca mode is OFF (or
/// `config.json` is unreadable). Silent; never touches the socket.
pub fn offline_view(profiles: &ProfileMap) -> Option<OfflineView> {
    let (config, slot) = slot::config_and_slot(profiles)?;
    let ud = super::user_data_dir_for(config.orca());
    let selection = ud
        .as_deref()
        .and_then(|ud| super::offline_selection_in(ud).ok().flatten());
    let bindings = match (&ud, &selection) {
        (Some(ud), Some(sel)) => Some(bind::compute(
            ud,
            sel,
            profiles,
            Some(&slot),
            &config.orca().bindings,
        )),
        _ => None,
    };
    Some(OfflineView {
        slot,
        selection,
        bindings,
        default_name: profiles.default_name(),
    })
}

/// The slot row's follow label: `→ <bound profile>` when Orca's active
/// account is bound, `→ (orca active: <email>)` when it is not, `→ (orca:
/// unknown)` when there is no active account to report. Pure.
pub fn slot_follow_label(
    selection: Option<&Selection>,
    bindings: Option<&Bindings>,
    default_name: Option<&str>,
) -> String {
    let Some(account) = selection.and_then(Selection::active_account) else {
        return "→ (orca: unknown)".to_owned();
    };
    match bindings.and_then(|b| b.profile_for_active(&account.id, default_name)) {
        Some(p) => format!("→ {p}"),
        None => format!("→ (orca active: {})", account.email),
    }
}

// ─── csm → Orca: explicit default change ──────────────────────────────────────

/// What an explicit default change should ask of Orca. Pure output of
/// [`plan_default_sync`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DefaultSyncPlan {
    /// The target is the slot: a plain default-state change, no RPC.
    SlotTarget,
    /// No Orca account list to bind against (Orca unreadable and no saved
    /// state); nothing to select.
    NoAccounts,
    /// The target has no Orca account.
    Unbound,
    /// Orca already has the target's account active.
    AlreadyActive,
    /// Orca runs: select `account_id`.
    Select { account_id: String },
    /// Orca is not running: queue `account_id`, expecting `prior` to still
    /// be active when it is applied.
    Queue {
        account_id: String,
        prior: Option<String>,
    },
}

/// Plan the Orca side of switching csm's default to `target`. `selection`
/// is Orca's live selection when `running`, else its persisted one; the
/// bindings were computed against it. Pure.
pub fn plan_default_sync(
    target: &str,
    slot: &Slot,
    running: bool,
    selection: Option<&Selection>,
    bindings: Option<&Bindings>,
) -> DefaultSyncPlan {
    if slot.is_profile(target) {
        return DefaultSyncPlan::SlotTarget;
    }
    let (Some(sel), Some(b)) = (selection, bindings) else {
        return DefaultSyncPlan::NoAccounts;
    };
    let Some(id) = b.account_for_profile(target) else {
        return DefaultSyncPlan::Unbound;
    };
    let active = sel.effective_active_id();
    if active == Some(id) {
        return DefaultSyncPlan::AlreadyActive;
    }
    if running {
        DefaultSyncPlan::Select {
            account_id: id.to_owned(),
        }
    } else {
        DefaultSyncPlan::Queue {
            account_id: id.to_owned(),
            prior: active.map(str::to_owned),
        }
    }
}

/// The stderr lines for one outcome of an explicit default change. Pure.
fn unbound_warning(target: &str) -> String {
    format!(
        "csm: warning: {target} has no Orca account; csm run follows Orca's active account \
         while Orca runs"
    )
}

/// After csm's default state was switched to `target` by an explicit
/// command, carry the change to Orca (Orca mode only; a no-op otherwise).
///
/// Writes only to stderr, never fails the caller: an RPC error or timeout is
/// a warning. Orca not running queues a pending select and says so. The
/// explicit choice supersedes any earlier queued select, which is dropped
/// first. The floor is not touched here; `apply_global` keeps it on the slot.
pub fn sync_default_change(target: &str, profiles: &ProfileMap) {
    let Some((config, slot)) = slot::config_and_slot(profiles) else {
        return;
    };
    // An explicit choice replaces whatever was queued before it, on every
    // path below (a slot target and an unresolvable userData dir included).
    let _ = pending::clear();
    let Some(ud) = super::user_data_dir_for(config.orca()) else {
        eprintln!("csm: warning: Orca's userData dir could not be resolved; Orca not switched");
        return;
    };
    if slot.is_profile(target) {
        return;
    }
    let deadline = Instant::now() + EXPLICIT_SELECT_BUDGET;
    let running = is_running(&ud);
    let selection = if running {
        match super::live_selection_fresh_in(&ud, EXPLICIT_SELECT_BUDGET / 2) {
            OrcaState::Live(s) => Some(s),
            // Orca runs but did not answer in time: bind against its saved
            // state and let the select (which re-reads) decide.
            _ => super::offline_selection_in(&ud).ok().flatten(),
        }
    } else {
        super::offline_selection_in(&ud).ok().flatten()
    };
    let bindings = selection
        .as_ref()
        .map(|s| bind::compute(&ud, s, profiles, Some(&slot), &config.orca().bindings));
    let email = |sel: Option<&Selection>, id: &str| {
        sel.and_then(|s| s.account(id))
            .map(|a| a.email.clone())
            .filter(|e| !e.is_empty())
            .unwrap_or_else(|| id.to_owned())
    };
    match plan_default_sync(
        target,
        &slot,
        running,
        selection.as_ref(),
        bindings.as_ref(),
    ) {
        DefaultSyncPlan::SlotTarget | DefaultSyncPlan::AlreadyActive => {}
        DefaultSyncPlan::NoAccounts => {
            eprintln!("csm: warning: Orca's accounts could not be read; Orca not switched");
        }
        DefaultSyncPlan::Unbound => eprintln!("{}", unbound_warning(target)),
        DefaultSyncPlan::Queue { account_id, prior } => {
            queue(
                &account_id,
                target,
                prior.as_deref(),
                &email(selection.as_ref(), &account_id),
                "Orca is not running",
            );
        }
        DefaultSyncPlan::Select { account_id } => {
            let who = email(selection.as_ref(), &account_id);
            let left = deadline.saturating_duration_since(Instant::now());
            match super::select_account(&account_id, &slot.dir, left) {
                Ok(SelectOutcome::Selected) => eprintln!("csm: Orca active → {who}"),
                Ok(SelectOutcome::Unknown) => eprintln!(
                    "csm: warning: Orca did not confirm the switch to {who} within {:.1}s; \
                     it may still apply (check with `csm orca status`)",
                    EXPLICIT_SELECT_BUDGET.as_secs_f32()
                ),
                Err(e @ (SelectError::NotRunning | SelectError::Unreachable(_))) => {
                    let prior = selection
                        .as_ref()
                        .and_then(Selection::effective_active_id)
                        .map(str::to_owned);
                    queue(&account_id, target, prior.as_deref(), &who, &e.to_string());
                }
                Err(e) => eprintln!("csm: warning: Orca not switched: {e}"),
            }
        }
    }
}

/// Queue a select for the next `csm run` / `csm orca sync`. `why` says why
/// it could not be applied now. On a platform with no select transport
/// (non-unix) nothing is queued: it could never apply, and a queued file
/// would make every `csm run` spawn a sync for 24 h.
fn queue(account_id: &str, profile: &str, prior: Option<&str>, who: &str, why: &str) {
    if !cfg!(unix) {
        eprintln!(
            "csm: note: Orca account selection is not supported on this platform; only csm's \
             default changed (select {who} in Orca yourself)"
        );
        return;
    }
    let now = crate::epoch::now_secs() as i64;
    match pending::write(&PendingSelect::new(account_id, profile, now, prior)) {
        Ok(()) => eprintln!(
            "csm: note: {why}; Orca's switch to {who} is queued for the next `csm run` or \
             `csm orca sync`"
        ),
        Err(e) => eprintln!("csm: warning: could not queue the Orca switch: {e}"),
    }
}

fn is_running(ud: &Path) -> bool {
    matches!(super::runtime_metadata_in(ud), Ok(Some(m)) if m.is_alive())
}

// ─── followSwitch (relaunch supervisor) ───────────────────────────────────────

/// What the supervisor's followSwitch should do. Pure output of
/// [`plan_follow_switch`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(windows, allow(dead_code))] // the relaunch supervisor is unix-only
pub enum FollowPlan {
    /// Do nothing; the reason goes to the session log.
    Skip(String),
    /// Select `account_id` in Orca.
    Select { account_id: String },
}

/// Plan followSwitch for a limit switch onto `target`. `selection` is Orca's
/// LIVE selection (`None` when it could not be read); `slot_users` lists the
/// pids of other claude processes running in the slot. Pure.
#[cfg_attr(windows, allow(dead_code))] // the relaunch supervisor is unix-only
pub fn plan_follow_switch(
    target: &str,
    slot: &Slot,
    selection: Option<&Selection>,
    bindings: Option<&Bindings>,
    slot_users: &[u32],
) -> FollowPlan {
    if slot.is_profile(target) {
        return FollowPlan::Skip("target is the Orca slot".to_owned());
    }
    let (Some(sel), Some(b)) = (selection, bindings) else {
        return FollowPlan::Skip("Orca's live selection could not be read".to_owned());
    };
    let Some(id) = b.account_for_profile(target) else {
        return FollowPlan::Skip(format!("{target} has no Orca account"));
    };
    if sel.effective_active_id() == Some(id) {
        return FollowPlan::Skip("already active in Orca".to_owned());
    }
    if !slot_users.is_empty() {
        let pids: Vec<String> = slot_users.iter().map(u32::to_string).collect();
        return FollowPlan::Skip(format!(
            "claude runs in the Orca slot (pid {}); a switch would swap its credentials",
            pids.join(", ")
        ));
    }
    FollowPlan::Select {
        account_id: id.to_owned(),
    }
}

/// One process as [`slot_users`] sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(windows, allow(dead_code))] // the relaunch supervisor is unix-only
pub struct SlotUserCandidate {
    pub pid: u32,
    /// Its name/exe/argv0 identify it as claude (or node running claude).
    pub claude_like: bool,
    /// Its raw `CLAUDE_CONFIG_DIR`, `None` when unset.
    pub config_dir: Option<String>,
}

/// Pids of claude processes whose `CLAUDE_CONFIG_DIR` is the slot dir,
/// excluding `exclude` (Orca itself, this supervisor). Orca's own helper
/// processes inherit the slot dir too but are not claude, so only claude-like
/// processes count. Pure.
#[cfg_attr(windows, allow(dead_code))] // the relaunch supervisor is unix-only
pub fn slot_users(procs: &[SlotUserCandidate], slot_dir: &str, exclude: &[u32]) -> Vec<u32> {
    let mut out: Vec<u32> = procs
        .iter()
        .filter(|p| p.claude_like && !exclude.contains(&p.pid))
        .filter(|p| {
            p.config_dir
                .as_deref()
                .map(str::trim)
                .is_some_and(|d| !d.is_empty() && dirs_equal(d, slot_dir))
        })
        .map(|p| p.pid)
        .collect();
    out.sort_unstable();
    out
}

/// The live process table as [`SlotUserCandidate`]s (full sweep; off every
/// hot path).
#[cfg_attr(windows, allow(dead_code))] // the relaunch supervisor is unix-only
fn sweep_slot_candidates() -> Vec<SlotUserCandidate> {
    let launch = crate::config::resolve_launch_command();
    crate::platform::proc::sweep_config_dirs()
        .into_iter()
        .map(|p| {
            let exe = p.exe.as_deref().and_then(Path::to_str);
            let argv0 = p.argv0.as_deref().and_then(|a| a.to_str());
            SlotUserCandidate {
                pid: p.pid,
                claude_like: crate::platform::proc_check::identity_matches(
                    &[Some(p.name.as_str()), exe, argv0],
                    &launch,
                ),
                config_dir: p.config_dir,
            }
        })
        .collect()
}

/// The supervisor's opt-in followSwitch: after an ordinary limit switch onto
/// `target`, best-effort ask Orca to select the target's account. Returns
/// the line to append to the session log, or `None` when followSwitch is
/// off (Orca mode OFF, `orca.followSwitch` false, unreadable config). Never
/// prints.
#[cfg_attr(windows, allow(dead_code))] // the relaunch supervisor is unix-only
pub fn follow_switch(target: &str, profiles: &ProfileMap) -> Option<String> {
    let (config, slot) = slot::config_and_slot(profiles)?;
    if !config.orca().follow_switch {
        return None;
    }
    Some(follow_switch_with(target, profiles, &config, &slot))
}

#[cfg_attr(windows, allow(dead_code))] // the relaunch supervisor is unix-only
fn follow_switch_with(target: &str, profiles: &ProfileMap, config: &Config, slot: &Slot) -> String {
    let log = |m: &str| format!("orca followSwitch → {target}: {m}");
    let deadline = Instant::now() + FOLLOW_SWITCH_BUDGET;
    let Some(ud): Option<PathBuf> = super::user_data_dir_for(config.orca()) else {
        return log("skipped (Orca's userData dir could not be resolved)");
    };
    let meta = match super::runtime_metadata_in(&ud) {
        Ok(Some(m)) if m.is_alive() => m,
        _ => return log("skipped (Orca is not running)"),
    };
    if slot.is_profile(target) {
        return log("skipped (target is the Orca slot)");
    }
    let selection = match super::live_selection_in(&ud, FOLLOW_SWITCH_BUDGET / 2) {
        OrcaState::Live(s) => Some(s),
        _ => None,
    };
    let bindings = selection
        .as_ref()
        .map(|s| bind::compute(&ud, s, profiles, Some(slot), &config.orca().bindings));
    // The sweep is only worth paying for when a select is otherwise due.
    let pre = plan_follow_switch(target, slot, selection.as_ref(), bindings.as_ref(), &[]);
    let users = match pre {
        FollowPlan::Select { .. } => slot_users(
            &sweep_slot_candidates(),
            &slot.dir,
            &[meta.pid, std::process::id()],
        ),
        FollowPlan::Skip(_) => Vec::new(),
    };
    match plan_follow_switch(target, slot, selection.as_ref(), bindings.as_ref(), &users) {
        FollowPlan::Skip(why) => log(&format!("skipped ({why})")),
        FollowPlan::Select { account_id } => {
            let left = deadline.saturating_duration_since(Instant::now());
            match super::select_account(&account_id, &slot.dir, left) {
                Ok(SelectOutcome::Selected) => log(&format!("selected {account_id}")),
                Ok(SelectOutcome::Unknown) => {
                    log(&format!("select of {account_id} not confirmed in time"))
                }
                Err(e) => log(&format!("select of {account_id} failed: {e}")),
            }
        }
    }
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orca::Account;
    use crate::orca::bind::{Binding, MatchKind};

    fn slot() -> Slot {
        Slot {
            name: "orca".to_owned(),
            dir: "/Users/example/.claude.orca".to_owned(),
        }
    }

    fn account(id: &str, email: &str) -> Account {
        Account {
            id: id.to_owned(),
            email: email.to_owned(),
            organization_uuid: None,
            organization_name: None,
            runtime: "host".to_owned(),
        }
    }

    fn sel(active: Option<&str>) -> Selection {
        Selection {
            accounts: vec![
                account("a1", "alice@example.com"),
                account("b2", "bob@example.com"),
            ],
            active_id: active.map(str::to_owned),
            host_active_id: None,
            rate_limits: None,
        }
    }

    /// a1 ↔ work (tied with work2), b2 unbound.
    fn bindings() -> Bindings {
        let mut b = Bindings::default();
        b.by_account.insert(
            "a1".to_owned(),
            Binding {
                profile: "work".to_owned(),
                kind: MatchKind::Uuid,
                via_override: false,
                candidates: vec!["work".to_owned(), "work2".to_owned()],
            },
        );
        b
    }

    // ── slot_follow_label ────────────────────────────────────────────────────

    #[test]
    fn follow_label_bound_active_account() {
        let s = sel(Some("a1"));
        assert_eq!(
            slot_follow_label(Some(&s), Some(&bindings()), None),
            "→ work"
        );
        // The default breaks a tie between two dirs of the same account.
        assert_eq!(
            slot_follow_label(Some(&s), Some(&bindings()), Some("work2")),
            "→ work2"
        );
    }

    #[test]
    fn follow_label_unbound_active_account_shows_email() {
        let s = sel(Some("b2"));
        assert_eq!(
            slot_follow_label(Some(&s), Some(&bindings()), None),
            "→ (orca active: bob@example.com)"
        );
    }

    #[test]
    fn follow_label_unknown_without_an_active_account() {
        assert_eq!(slot_follow_label(None, None, None), "→ (orca: unknown)");
        let s = sel(None);
        assert_eq!(
            slot_follow_label(Some(&s), Some(&bindings()), None),
            "→ (orca: unknown)"
        );
    }

    // ── plan_default_sync ────────────────────────────────────────────────────

    #[test]
    fn default_sync_slot_target_never_selects() {
        let s = sel(Some("b2"));
        assert_eq!(
            plan_default_sync("orca", &slot(), true, Some(&s), Some(&bindings())),
            DefaultSyncPlan::SlotTarget
        );
    }

    #[test]
    fn default_sync_selects_a_bound_profile_while_orca_runs() {
        let s = sel(Some("b2"));
        assert_eq!(
            plan_default_sync("work", &slot(), true, Some(&s), Some(&bindings())),
            DefaultSyncPlan::Select {
                account_id: "a1".to_owned()
            }
        );
        // A tie candidate resolves to the same account.
        assert_eq!(
            plan_default_sync("work2", &slot(), true, Some(&s), Some(&bindings())),
            DefaultSyncPlan::Select {
                account_id: "a1".to_owned()
            }
        );
    }

    #[test]
    fn default_sync_skips_when_already_active() {
        let s = sel(Some("a1"));
        assert_eq!(
            plan_default_sync("work", &slot(), true, Some(&s), Some(&bindings())),
            DefaultSyncPlan::AlreadyActive
        );
    }

    #[test]
    fn default_sync_unbound_profile_warns() {
        let s = sel(Some("a1"));
        assert_eq!(
            plan_default_sync("home", &slot(), true, Some(&s), Some(&bindings())),
            DefaultSyncPlan::Unbound
        );
        assert!(unbound_warning("home").contains("home has no Orca account"));
    }

    #[test]
    fn default_sync_queues_when_orca_is_not_running() {
        let s = sel(Some("b2"));
        assert_eq!(
            plan_default_sync("work", &slot(), false, Some(&s), Some(&bindings())),
            DefaultSyncPlan::Queue {
                account_id: "a1".to_owned(),
                prior: Some("b2".to_owned()),
            }
        );
    }

    #[test]
    fn default_sync_without_accounts_does_nothing() {
        assert_eq!(
            plan_default_sync("work", &slot(), true, None, None),
            DefaultSyncPlan::NoAccounts
        );
    }

    // ── plan_follow_switch ───────────────────────────────────────────────────

    #[test]
    fn follow_switch_selects_a_bound_target() {
        let s = sel(Some("b2"));
        assert_eq!(
            plan_follow_switch("work", &slot(), Some(&s), Some(&bindings()), &[]),
            FollowPlan::Select {
                account_id: "a1".to_owned()
            }
        );
    }

    #[test]
    fn follow_switch_skips() {
        let s = sel(Some("a1"));
        let b = bindings();
        let skip = |p: FollowPlan| matches!(p, FollowPlan::Skip(_));
        // already active
        assert!(skip(plan_follow_switch(
            "work",
            &slot(),
            Some(&s),
            Some(&b),
            &[]
        )));
        // unbound
        let s2 = sel(Some("b2"));
        assert!(skip(plan_follow_switch(
            "home",
            &slot(),
            Some(&s2),
            Some(&b),
            &[]
        )));
        // slot as target
        assert!(skip(plan_follow_switch(
            "orca",
            &slot(),
            Some(&s2),
            Some(&b),
            &[]
        )));
        // Orca unreadable
        assert!(skip(plan_follow_switch("work", &slot(), None, None, &[])));
        // another claude runs in the slot
        match plan_follow_switch("work", &slot(), Some(&s2), Some(&b), &[4242]) {
            FollowPlan::Skip(why) => assert!(why.contains("4242")),
            other => panic!("expected a skip, got {other:?}"),
        }
    }

    // ── slot_users ───────────────────────────────────────────────────────────

    #[test]
    fn slot_users_counts_only_other_claude_processes_in_the_slot() {
        let c = |pid, claude_like, dir: Option<&str>| SlotUserCandidate {
            pid,
            claude_like,
            config_dir: dir.map(str::to_owned),
        };
        let procs = vec![
            c(10, true, Some("/Users/example/.claude.orca/")),
            c(11, false, Some("/Users/example/.claude.orca")), // Orca helper
            c(12, true, Some("/Users/example/.claude.work")),
            c(13, true, None),
            c(14, true, Some("/Users/example/.claude.orca")), // excluded pid
            c(15, true, Some("  ")),
        ];
        assert_eq!(
            slot_users(&procs, "/Users/example/.claude.orca", &[14]),
            vec![10]
        );
    }

    // ── loaders stay off without a test HOME ─────────────────────────────────

    #[test]
    fn loaders_read_as_orca_off_without_a_test_home() {
        let pm = ProfileMap::default();
        assert!(offline_view(&pm).is_none());
        assert!(follow_switch("work", &pm).is_none());
        // Must not print or touch Orca either.
        sync_default_change("work", &pm);
    }

    #[test]
    fn offline_view_reads_saved_state_and_identities() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        crate::testenv::with_test_home(home, || {
            let pm = test_support::orca_home(home, &["home", "work"]);
            test_support::write_orca_data(
                home,
                &[("a1", "alice@example.com"), ("b2", "bob@example.com")],
                Some("b2"),
            );
            test_support::login(home, "work", "alice@example.com");
            test_support::login(home, "home", "bob@example.com");
            let v = offline_view(&pm).unwrap();
            assert_eq!(v.email_for("work"), Some("alice@example.com"));
            assert_eq!(v.email_for("orca"), None);
            assert_eq!(v.slot_follow(), "→ home");
            assert_eq!(v.active_identity_profiles(), vec!["home".to_string()]);
        });
    }

    /// Orca not running: an explicit default change queues the select
    /// (expecting Orca's saved active account to still be active), and a
    /// later change to an unbound profile drops that queue. Unix only: with
    /// no select transport nothing is ever queued (see the non-unix test).
    #[cfg(unix)]
    #[test]
    fn default_change_queues_while_orca_is_not_running() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        crate::testenv::with_test_home(home, || {
            let pm = test_support::orca_home(home, &["home", "work", "spare"]);
            test_support::write_orca_data(
                home,
                &[("a1", "alice@example.com"), ("b2", "bob@example.com")],
                Some("b2"),
            );
            test_support::login(home, "work", "alice@example.com");
            sync_default_change("work", &pm);
            let p = pending::read().unwrap().expect("a queued select");
            assert_eq!(p.account_id, "a1");
            assert_eq!(p.profile, "work");
            assert_eq!(p.expected_prior_active_id.as_deref(), Some("b2"));

            sync_default_change("spare", &pm);
            assert!(
                pending::read().unwrap().is_none(),
                "superseded by the explicit choice"
            );

            // The slot as target queues nothing, but it is still an explicit
            // choice: an older queued select must not outlive it.
            sync_default_change("work", &pm);
            assert!(pending::read().unwrap().is_some());
            sync_default_change("orca", &pm);
            assert!(
                pending::read().unwrap().is_none(),
                "the slot target also supersedes the queue"
            );
        });
    }

    /// No select transport (non-unix): a default change never queues, so
    /// `csm run` never spawns a sync that could not apply.
    #[cfg(not(unix))]
    #[test]
    fn default_change_never_queues_without_a_select_transport() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        crate::testenv::with_test_home(home, || {
            let pm = test_support::orca_home(home, &["home", "work"]);
            test_support::write_orca_data(home, &[("a1", "alice@example.com")], None);
            test_support::login(home, "work", "alice@example.com");
            sync_default_change("work", &pm);
            assert!(pending::read().unwrap().is_none());
        });
    }

    #[test]
    fn offline_view_in_an_isolated_home_without_orca_data() {
        let tmp = tempfile::tempdir().unwrap();
        crate::testenv::with_test_home(tmp.path(), || {
            let pm = test_support::orca_home(tmp.path(), &["work"]);
            let v = offline_view(&pm).expect("Orca mode is on");
            assert_eq!(v.slot.name, "orca");
            assert!(v.selection.is_none());
            assert_eq!(v.slot_follow(), "→ (orca: unknown)");
            assert_eq!(v.email_for("work"), None);
            assert!(v.active_identity_profiles().is_empty());
        });
    }
}

/// Fixtures shared by other modules' tests: an isolated HOME in Orca mode.
#[cfg(test)]
pub(crate) mod test_support {
    use std::path::Path;

    use crate::account::ProfileMap;
    use crate::config::{Config, OrcaConfig};

    /// Inside `with_test_home(home, …)`: register the slot `orca` plus
    /// `others` (each at `<home>/.claude.<name>`), turn Orca mode on, and
    /// point Orca's userData at an empty `<home>/orca-ud` (no Orca running,
    /// no saved state). Returns the loaded registry.
    pub(crate) fn orca_home(home: &Path, others: &[&str]) -> ProfileMap {
        let mut pm = ProfileMap::default();
        pm.insert(
            "orca".to_owned(),
            home.join(".claude.orca").to_string_lossy().into_owned(),
        );
        for n in others {
            pm.insert(
                (*n).to_owned(),
                home.join(format!(".claude.{n}"))
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        pm.save().unwrap();
        Config {
            orca: OrcaConfig {
                slot_profile: Some("orca".to_owned()),
                user_data_dir: Some(home.join("orca-ud").to_string_lossy().into_owned()),
                ..Default::default()
            },
            ..Default::default()
        }
        .save()
        .unwrap();
        ProfileMap::load().unwrap()
    }

    /// Write Orca's saved state (`orca-data.json`) under `<home>/orca-ud`:
    /// host accounts `(id, email)` with `active` as the active one.
    pub(crate) fn write_orca_data(home: &Path, accounts: &[(&str, &str)], active: Option<&str>) {
        let path = crate::orca::data_file::data_file_path(&home.join("orca-ud"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let accts: Vec<serde_json::Value> = accounts
            .iter()
            .map(|(id, email)| serde_json::json!({"id": id, "email": email, "managedAuthRuntime": "host"}))
            .collect();
        let doc = serde_json::json!({"settings": {
            "claudeManagedAccounts": accts,
            "activeClaudeManagedAccountId": active,
        }});
        std::fs::write(path, doc.to_string()).unwrap();
    }

    /// Log `profile` (at `<home>/.claude.<profile>`) in as `email`.
    pub(crate) fn login(home: &Path, profile: &str, email: &str) {
        let dir = home.join(format!(".claude.{profile}"));
        std::fs::create_dir_all(&dir).unwrap();
        let doc = serde_json::json!({"oauthAccount": {"emailAddress": email}});
        std::fs::write(dir.join(".claude.json"), doc.to_string()).unwrap();
    }
}
