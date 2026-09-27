//! The machine's Orca context, resolved once for a command: the host
//! environment, userData, the store file, csm's state dir, `D`, the Keychain
//! account names and the liveness sources. The cmd layer builds a
//! [`SwitchEnv`] or an [`AccountsEnv`] from it instead of repeating the
//! wiring.
//!
//! Resolution reads files only. Nothing here writes, touches the Keychain,
//! or calls the network; the envs it builds do that when the caller runs a
//! switch or an account change.

use std::path::PathBuf;
use std::time::Duration;

use super::add::AccountsEnv;
use super::http::OauthHttp;
use super::keychain::KeychainUser;
use super::live::{ProcFacts, SystemLiveness};
use super::runtime::{self, RuntimePaths};
use super::store::RedoOpts;
use super::switch::{Owner, SwitchEnv, SwitchTiming};
use super::userdata::{self, DataFileChoice, UserData};
use super::{HostEnv, HostOs, OrcaError, fsx, live, procenv, rpc, version};

/// How long an account change waits for the `switch.lock`.
pub const LOCK_WAIT: Duration = Duration::from_secs(30);

/// One machine's Orca context.
#[derive(Debug, Clone)]
pub struct Context {
    pub env: HostEnv,
    pub user_data: UserData,
    pub data_file: DataFileChoice,
    /// csm's state dir.
    pub state: PathBuf,
    /// csm's `D`.
    pub paths: RuntimePaths,
    pub keychain_user: KeychainUser,
    /// The Orca version is in csm's tested range.
    pub version_ok: bool,
}

impl Context {
    /// The real machine (under `cfg(test)`, the thread's test home only).
    pub fn current(facts: &dyn ProcFacts) -> Result<Context, OrcaError> {
        Ok(Context::from_env(HostEnv::current()?, facts))
    }

    /// [`Context::current`] with the launch's `CLAUDE_CONFIG_DIR` pin
    /// applied, so `D` is the one the supervised child runs in, not the
    /// value this process inherited.
    pub fn current_pinned(
        facts: &dyn ProcFacts,
        pin: &crate::launch_context::ConfigDirPin,
    ) -> Result<Context, OrcaError> {
        let mut env = HostEnv::current()?;
        pin.apply_to(&mut env);
        Ok(Context::from_env(env, facts))
    }

    /// Resolve over an explicit environment and process table.
    pub fn from_env(env: HostEnv, facts: &dyn ProcFacts) -> Context {
        let user_data = userdata::resolve(&env, |dir| {
            rpc::read_runtime_metadata(dir)
                .ok()
                .flatten()
                .is_some_and(|m| m.pid != 0 && facts.alive(m.pid))
        });
        let data_file = userdata::data_file(&user_data.dir);
        let report = live::check(env.os, &user_data.dir, facts);
        let version_ok = version::detect(&env, report.main_exe.as_deref()).in_tested_range();
        let paths =
            runtime::runtime_paths(env.claude_config_dir.as_deref(), &env.home, |p| p.exists());
        Context {
            state: fsx::state_dir(&env),
            keychain_user: KeychainUser::from_env(&env),
            user_data,
            data_file,
            paths,
            version_ok,
            env,
        }
    }

    pub fn os(&self) -> HostOs {
        self.env.os
    }

    /// The liveness source for this userData.
    pub fn liveness<'a>(&self, facts: &'a dyn ProcFacts) -> SystemLiveness<'a> {
        SystemLiveness {
            os: self.env.os,
            user_data: self.user_data.dir.clone(),
            facts,
        }
    }

    /// Is Orca running right now (fail closed)?
    pub fn orca_running(&self, facts: &dyn ProcFacts) -> bool {
        live::check(self.env.os, &self.user_data.dir, facts).running
    }

    /// Is a live claude registered in `D`? Unknown counts as live.
    pub fn live_claude(&self, facts: &dyn ProcFacts) -> bool {
        live_claude_in(self.env.os, &self.paths.config_dir, facts)
    }

    /// Does Orca main's `D` equal csm's? `None` when it cannot be read.
    pub fn orca_dir_agrees(&self, facts: &dyn ProcFacts) -> Option<bool> {
        let report = live::check(self.env.os, &self.user_data.dir, facts);
        let pid = report.main_pid.or(report.runtime.as_ref().map(|m| m.pid))?;
        let dir = procenv::orca_runtime_dir(pid, Some(&self.env.home))?;
        Some(procenv::same_runtime_dir(&self.paths, &dir))
    }

    /// Run `f` with a [`SwitchEnv`] over this context.
    pub fn with_switch_env<R>(
        &self,
        facts: &dyn ProcFacts,
        http: &dyn OauthHttp,
        f: impl FnOnce(&SwitchEnv<'_>) -> R,
    ) -> R {
        self.with_switch_env_child(facts, http, false, f)
    }

    /// [`Context::with_switch_env`] for a caller whose own claude child is
    /// running in `D` (`child_live`): the supervisor's recovery right after
    /// the spawn, before the child has registered in `D/sessions`. A live
    /// claude then counts as present whatever the scan says, so the repair
    /// uses Orca's materialize order and skips the refresh.
    pub fn with_switch_env_child<R>(
        &self,
        facts: &dyn ProcFacts,
        http: &dyn OauthHttp,
        child_live: bool,
        f: impl FnOnce(&SwitchEnv<'_>) -> R,
    ) -> R {
        let live = self.liveness(facts);
        let live_claude = || child_live || self.live_claude(facts);
        let agrees = || self.orca_dir_agrees(facts);
        let env = SwitchEnv {
            os: self.env.os,
            user_data: &self.user_data.dir,
            data_file: &self.data_file,
            state: &self.state,
            paths: &self.paths,
            keychain_user: &self.keychain_user,
            live: &live,
            http,
            live_claude: &live_claude,
            orca_dir_agrees: &agrees,
            version_ok: self.version_ok,
            store_access_allowed: self.user_data.store_access_allowed(),
            owner: Owner::current(facts),
            timing: SwitchTiming::default(),
        };
        f(&env)
    }

    /// Run `f` with an [`AccountsEnv`] over this context.
    pub fn with_accounts_env<R>(
        &self,
        facts: &dyn ProcFacts,
        f: impl FnOnce(&AccountsEnv<'_>) -> R,
    ) -> R {
        let live = self.liveness(facts);
        let env = AccountsEnv {
            os: self.env.os,
            user_data: &self.user_data.dir,
            data_file: &self.data_file,
            state: &self.state,
            keychain_user: &self.keychain_user,
            live: &live,
            version_ok: self.version_ok,
            store_access_allowed: self.user_data.store_access_allowed(),
            lock_wait: LOCK_WAIT,
            redo: RedoOpts::default(),
            mutation_timeout: rpc::ADD_TIMEOUT,
        };
        f(&env)
    }
}

/// Is a live claude registered in `dir/sessions`? A scan error counts as
/// live (fail safe).
pub fn live_claude_in(os: HostOs, dir: &std::path::Path, facts: &dyn ProcFacts) -> bool {
    let domain = runtime::this_pid_domain(os);
    runtime::scan_sessions(&dir.join("sessions"), &domain, facts)
        .map(|s| s.may_have_live())
        .unwrap_or(true)
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orca::testsupport::FakeProcs;

    #[test]
    fn context_resolves_under_the_test_home_only() {
        let tmp = tempfile::tempdir().unwrap();
        let env = HostEnv::for_test(tmp.path(), HostOs::Linux);
        let ctx = Context::from_env(env, &FakeProcs::default());
        assert!(ctx.user_data.dir.starts_with(tmp.path()));
        assert!(ctx.state.starts_with(tmp.path()));
        assert_eq!(ctx.paths.config_dir, tmp.path().join(".claude"));
        assert!(!ctx.orca_running(&FakeProcs::default()));
    }

    #[test]
    fn current_refuses_without_a_test_home() {
        assert!(Context::current(&FakeProcs::default()).is_err());
    }

    #[test]
    fn a_supervisors_own_child_counts_as_a_live_claude() {
        let tmp = tempfile::tempdir().unwrap();
        let env = HostEnv::for_test(tmp.path(), HostOs::Linux);
        let procs = FakeProcs::default();
        let ctx = Context::from_env(env, &procs);
        let http = crate::orca::http::FakeHttp::default();
        // No D/sessions registry: the scan finds nobody.
        assert!(!ctx.with_switch_env(&procs, &http, |e| (e.live_claude)()));
        assert!(ctx.with_switch_env_child(&procs, &http, true, |e| (e.live_claude)()));
        assert!(!ctx.with_switch_env_child(&procs, &http, false, |e| (e.live_claude)()));
    }

    #[test]
    fn no_sessions_dir_means_no_live_claude() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!live_claude_in(
            HostOs::Linux,
            tmp.path(),
            &FakeProcs::default()
        ));
    }
}
