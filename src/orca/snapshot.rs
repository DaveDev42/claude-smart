//! One read-only view over Orca's account state, shared by the commands
//! that read it (`csm accounts`, `csm orca status`, `csm migrate`).
//!
//! [`snapshot`] resolves userData, checks whether Orca runs (fail closed),
//! loads the store, and, when asked and Orca runs, reads `accounts.list
//! {refreshUsage:false}` over RPC, which reflects Orca's memory (the store
//! lags it by up to a few seconds). It also resolves csm's `D`, Orca main's
//! `D` (from its environment) and `D`'s identity. Nothing here writes, and
//! nothing here holds a secret: the RPC token stays inside [`super::rpc`].

use std::path::PathBuf;
use std::time::Duration;

use super::live::{self, ProcFacts, SystemProcs};
use super::record::{AccountRecord, n6i};
use super::rpc;
use super::runtime::{self, RuntimeAccount, RuntimePaths};
use super::store::{self, StoreView};
use super::userdata::{self, UserData};
use super::version::{self, OrcaVersion};
use super::{HostEnv, OrcaError};

/// What the snapshot may do beyond reading files.
#[derive(Debug, Clone)]
pub struct SnapshotOptions {
    /// Ask a running Orca for `accounts.list {refreshUsage:false}`.
    pub rpc: bool,
    pub rpc_timeout: Duration,
    /// Read Orca main's environment for its `D`.
    pub orca_env: bool,
    /// Map `D`'s identity to an account (reads each stash's metadata).
    pub identity: bool,
}

impl Default for SnapshotOptions {
    fn default() -> Self {
        SnapshotOptions {
            rpc: true,
            rpc_timeout: Duration::from_secs(5),
            orca_env: true,
            identity: true,
        }
    }
}

/// Where the account list came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountSource {
    /// `accounts.list` over RPC (Orca's memory).
    Rpc,
    /// `orca-data.json`.
    Store,
    /// Neither was available.
    None,
}

/// Orca's account state as csm sees it.
#[derive(Debug, Clone)]
pub struct OrcaView {
    pub user_data: UserData,
    /// The store's account view, when a store exists and parses.
    pub store: Option<StoreView>,
    pub store_error: Option<String>,
    pub running: bool,
    /// csm's `D` (getRuntimePaths over csm's environment).
    pub runtime: RuntimePaths,
    /// Orca main's `D`, when Orca runs and its environment was read.
    pub orca_runtime_dir: Option<PathBuf>,
    /// How that `D` was found, when not from Orca main's own environment.
    pub orca_dir_note: Option<String>,
    /// `Some(true)` when both `D`s are known and equal.
    pub runtime_dir_agrees: Option<bool>,
    /// The effective host active id (n6i-normalized, as `accounts.list`
    /// reports it).
    pub active_id: Option<String>,
    pub accounts: Vec<AccountRecord>,
    pub source: AccountSource,
    /// The RPC read failed (Orca runs but did not answer).
    pub rpc_error: Option<String>,
    /// `D`'s identity and the account it maps to.
    pub runtime_account: Option<RuntimeAccount>,
    pub version: OrcaVersion,
    /// The Orca version is in csm's tested range.
    pub version_ok: bool,
    pub schema_version: Option<String>,
    /// The profile keeps its state in SQLite (Orca 1.4.214+): the store
    /// read above is Orca's export and may lag the database; csm never
    /// writes it offline.
    pub sqlite_state: bool,
}

impl OrcaView {
    /// Host accounts only (the ones csm switches between).
    pub fn host_accounts(&self) -> impl Iterator<Item = &AccountRecord> {
        self.accounts.iter().filter(|a| a.is_host())
    }

    /// May csm write Orca's store offline? Orca stopped, store access
    /// allowed, version tested, store gated. The write stage re-checks all
    /// of this under its lock; this is a read-only preview.
    pub fn offline_write_allowed(&self) -> bool {
        !self.running
            && self.user_data.store_access_allowed()
            && self.version_ok
            && !self.sqlite_state
            && self.store.as_ref().is_some_and(|s| s.writable().is_ok())
            && self.store_error.is_none()
    }
}

/// [`snapshot_with`] against the real machine.
pub fn snapshot(opts: &SnapshotOptions) -> Result<OrcaView, OrcaError> {
    let env = HostEnv::current()?;
    Ok(snapshot_with(&env, opts, &SystemProcs))
}

/// The snapshot over an explicit environment and process table.
pub fn snapshot_with(env: &HostEnv, opts: &SnapshotOptions, facts: &dyn ProcFacts) -> OrcaView {
    let user_data = userdata::resolve(env, |dir| {
        rpc::read_runtime_metadata(dir)
            .ok()
            .flatten()
            .is_some_and(|m| m.pid != 0 && facts.alive(m.pid))
    });
    let data_file = userdata::data_file(&user_data.dir);
    let live = live::check(env.os, &user_data.dir, facts);
    let running = live.running;

    let (store, store_error) = match store::load_choice(&data_file) {
        Ok(None) => (None, None),
        Ok(Some(f)) => match StoreView::from_bytes(&f.bytes) {
            Ok(v) => (
                Some(v),
                f.legacy
                    .then(|| "the store is Orca's legacy root file".to_owned()),
            ),
            Err(e) => (None, Some(e.to_string())),
        },
        Err(e) => (None, Some(e.to_string())),
    };

    let runtime =
        runtime::runtime_paths(env.claude_config_dir.as_deref(), &env.home, |p| p.exists());

    // Orca main's D, when Orca runs.
    let orca_pid = live.main_pid.or(live.runtime.as_ref().map(|m| m.pid));
    let orca_found = if running && opts.orca_env {
        orca_pid.and_then(|pid| super::procenv::orca_dir_sourced(pid, Some(&env.home)))
    } else {
        None
    };
    let orca_dir_note = orca_found.as_ref().and_then(|(_, src)| src.note());
    let orca_runtime_dir = orca_found.map(|(d, _)| d.dir);
    let runtime_dir_agrees = orca_runtime_dir
        .as_deref()
        .map(|d| super::procenv::same_runtime_dir(&runtime, d));

    // Accounts: RPC when Orca runs and answers, else the store.
    let mut rpc_error = None;
    let mut rpc_snapshot = None;
    if running && opts.rpc && user_data.store_access_allowed() {
        match rpc::accounts_list(&user_data.dir, false, opts.rpc_timeout) {
            Ok(s) => rpc_snapshot = Some(s),
            Err(e) => rpc_error = Some(e.to_string()),
        }
    }
    let (accounts, active_id, source) = match (&rpc_snapshot, &store) {
        (Some(s), _) => (
            s.claude.accounts.clone(),
            s.claude.active_by_runtime.host.clone(),
            AccountSource::Rpc,
        ),
        (None, Some(v)) => (
            v.accounts.clone(),
            n6i(&v.active, &v.accounts).host,
            AccountSource::Store,
        ),
        (None, None) => (Vec::new(), None, AccountSource::None),
    };

    // Stash paths come from the store records (RPC records carry none).
    let runtime_account = opts.identity.then(|| {
        let with_paths: Vec<AccountRecord> = accounts
            .iter()
            .map(|a| {
                let path = store
                    .as_ref()
                    .and_then(|s| s.account(&a.id))
                    .and_then(|r| r.managed_auth_path.clone());
                AccountRecord {
                    managed_auth_path: path.or_else(|| a.managed_auth_path.clone()),
                    ..a.clone()
                }
            })
            .collect();
        runtime::runtime_account(&runtime, &user_data.dir, &with_paths)
    });

    let version = version::detect(env, live.main_exe.as_deref());
    let version_ok = version.in_tested_range();
    OrcaView {
        schema_version: store.as_ref().and_then(|s| s.schema_version.clone()),
        sqlite_state: data_file.has_state_db(),
        user_data,
        store,
        store_error,
        running,
        runtime,
        orca_runtime_dir,
        orca_dir_note,
        runtime_dir_agrees,
        active_id,
        accounts,
        source,
        rpc_error,
        runtime_account,
        version,
        version_ok,
    }
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orca::HostOs;
    use crate::orca::runtime::UuidMatch;
    #[cfg(unix)]
    use crate::orca::testsupport::FakeOrca;
    use crate::orca::testsupport::{FakeProcs, make_stash};
    #[cfg(unix)]
    use serde_json::json;

    fn store_json(ud: &std::path::Path) -> String {
        // JSON-escaped, so a Windows path with backslashes stays valid.
        let auth = |id: &str| {
            serde_json::to_string(&crate::orca::stash::default_auth_dir(ud, id)).unwrap()
        };
        format!(
            concat!(
                r#"{{"schemaVersion":1,"settings":{{"claudeManagedAccounts":["#,
                r#"{{"id":"id-a","email":"alice@example.com","managedAuthPath":{a},"managedAuthRuntime":"host","wslDistro":null,"wslLinuxAuthPath":null,"authMethod":"subscription-oauth","organizationUuid":null,"organizationName":null,"createdAt":1,"updatedAt":1,"lastAuthenticatedAt":1}},"#,
                r#"{{"id":"id-b","email":"bob@example.com","managedAuthPath":{b},"managedAuthRuntime":"host","wslDistro":null,"wslLinuxAuthPath":null,"authMethod":"subscription-oauth","organizationUuid":"org-acme","organizationName":"Acme","createdAt":2,"updatedAt":2,"lastAuthenticatedAt":2}}"#,
                r#"],"activeClaudeManagedAccountId":"id-a","activeClaudeManagedAccountIdsByRuntime":{{"host":"id-a","wsl":{{}}}}}}}}"#
            ),
            a = auth("id-a"),
            b = auth("id-b")
        )
    }

    fn env_at(home: &std::path::Path) -> HostEnv {
        let mut e = HostEnv::for_test(home, HostOs::Linux);
        e.claude_config_dir = Some(home.join("claude-d").to_string_lossy().into_owned());
        e
    }

    #[test]
    fn empty_machine() {
        let dir = tempfile::tempdir().unwrap();
        let v = snapshot_with(
            &env_at(dir.path()),
            &SnapshotOptions::default(),
            &FakeProcs::default(),
        );
        assert!(!v.running);
        assert_eq!(v.source, AccountSource::None);
        assert!(v.accounts.is_empty() && v.active_id.is_none());
        assert!(!v.version_ok && !v.offline_write_allowed());
        assert_eq!(v.user_data.dir, dir.path().join(".config/orca"));
    }

    #[test]
    fn stopped_orca_reads_the_store_and_d_identity() {
        let dir = tempfile::tempdir().unwrap();
        let env = env_at(dir.path());
        let ud = dir.path().join(".config/orca");
        let file = ud.join("profiles/local-default/orca-data.json");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, store_json(&ud)).unwrap();
        make_stash(&ud, "id-a", Some(br#"{"accountUuid":"u-a"}"#), None);
        make_stash(&ud, "id-b", Some(br#"{"accountUuid":"u-b"}"#), None);
        let d = dir.path().join("claude-d");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join(".claude.json"),
            r#"{"oauthAccount":{"accountUuid":"u-b"}}"#,
        )
        .unwrap();

        let v = snapshot_with(&env, &SnapshotOptions::default(), &FakeProcs::default());
        assert!(!v.running);
        assert_eq!(v.source, AccountSource::Store);
        assert_eq!(v.accounts.len(), 2);
        assert_eq!(v.active_id.as_deref(), Some("id-a"));
        assert_eq!(v.schema_version.as_deref(), Some("1"));
        assert_eq!(v.runtime.config_dir, d);
        assert_eq!(
            v.runtime_account.as_ref().unwrap().account,
            Some(UuidMatch::Unique("id-b".into()))
        );
        assert!(v.store.as_ref().unwrap().writable().is_ok());
        // Linux has no version source yet: offline writes stay off.
        assert!(!v.version_ok && !v.offline_write_allowed());
    }

    #[cfg(unix)]
    #[test]
    fn running_orca_is_read_over_rpc() {
        let dir = tempfile::tempdir().unwrap();
        let orca = FakeOrca::start(|_| {
            vec![FakeOrca::ok(json!({
                "claude": {
                    "accounts": [{"id": "id-b", "email": "bob@example.com"}],
                    "activeAccountId": "id-b",
                    "activeAccountIdsByRuntime": {"host": "id-b", "wsl": {}}
                },
                "rateLimits": {"inactiveClaudeAccounts": []}
            }))]
        });
        // The hint names the fake's userData; its runtime pid is this test
        // process, which the fake table reports as Orca's main executable.
        let mut env = env_at(dir.path());
        env.orca_user_data_path = Some(orca.user_data().to_string_lossy().into_owned());
        let me = std::process::id();
        let facts = FakeProcs::default().with(crate::orca::testsupport::proc_info(
            me,
            "orca-ide",
            Some("/opt/Orca/orca-ide"),
            &[],
        ));
        let v = snapshot_with(
            &env,
            &SnapshotOptions {
                orca_env: false,
                ..SnapshotOptions::default()
            },
            &facts,
        );
        assert_eq!(v.user_data.dir, orca.user_data());
        assert!(v.running);
        assert_eq!(v.source, AccountSource::Rpc, "{:?}", v.rpc_error);
        assert_eq!(v.active_id.as_deref(), Some("id-b"));
        assert!(!v.offline_write_allowed());
    }

    #[test]
    fn snapshot_refuses_without_a_test_home() {
        crate::testenv::set_test_home(None);
        assert!(matches!(
            snapshot(&SnapshotOptions::default()),
            Err(OrcaError::Refused(_))
        ));
    }

    #[test]
    fn snapshot_under_a_test_home_stays_inside_it() {
        let dir = tempfile::tempdir().unwrap();
        let v = crate::testenv::with_test_home(dir.path(), || {
            snapshot(&SnapshotOptions::default()).unwrap()
        });
        assert!(v.user_data.dir.starts_with(dir.path()));
        assert!(v.runtime.config_dir.starts_with(dir.path()));
        assert!(!v.running, "the real machine's Orca is invisible to tests");
    }
}
