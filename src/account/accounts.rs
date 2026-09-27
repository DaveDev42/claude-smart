//! Orca's host accounts as csm's usage, scoring and report code sees them.
//!
//! csm keeps no account registry of its own: the account list, the active id
//! and the one runtime dir `D` are Orca's (see [`crate::orca`]). This module
//! is the read-only adapter the rest of the crate uses in place of the old
//! profile registry, keyed by Orca's account id:
//!
//! - [`AccountSet::load`] reads Orca's store (no RPC, no Keychain, no
//!   network) and `D`'s `oauthAccount`, which it maps to an account id
//!   through the stashes' `oauth-account.json` files. It is cheap enough for
//!   the statusline path, and the only load the hook and the statusline use
//!   (design decision 8). [`AccountSet::load_pinned`] is the same read with a
//!   launch's `CLAUDE_CONFIG_DIR` pin applied.
//! - [`AccountSet::load_live`] takes Orca's own list over RPC
//!   (`accounts.list`) while Orca runs, since its store lags its memory, and
//!   falls back to the store otherwise. For callers outside the hook and the
//!   statusline only.
//! - [`find`] resolves what a user typed (`accounts use <id|prefix|email>`)
//!   to one account. Pure.
//! - [`is_valid_key`] gates an id before it names a file in csm's state dir.
//!
//! Only host accounts are listed: WSL-runtime accounts are Orca's to manage,
//! and csm never switches to one.

use std::path::{Path, PathBuf};

use crate::orca::keychain::KeychainUser;
use crate::orca::record::n6i;
use crate::orca::runtime::{self, RuntimeIdentity, RuntimePaths, UuidMatch};
use crate::orca::store::{self, StoreView};
use crate::orca::userdata;
use crate::orca::{HostEnv, HostOs, rpc};

// ─── types ────────────────────────────────────────────────────────────────────

/// One Orca host account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountEntry {
    /// Orca's account id (a random UUID).
    pub id: String,
    pub email: Option<String>,
    pub organization_name: Option<String>,
    /// The record's `managedAuthPath` (the stash dir), when the store has it.
    pub managed_auth_path: Option<String>,
}

impl AccountEntry {
    /// The short human label: the email's local part, else the id's first
    /// eight characters.
    pub fn label(&self) -> String {
        label_for(&self.id, self.email.as_deref())
    }
}

/// Orca's host accounts plus `D`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccountSet {
    /// In Orca's order.
    pub accounts: Vec<AccountEntry>,
    /// Orca's active host id (as the store, or Orca's live list, names it).
    pub active: Option<String>,
    /// The list and `active` are Orca's live `accounts.list` (Orca runs and
    /// answered), not the store.
    pub from_orca: bool,
    /// The account `D`'s `oauthAccount` maps to, when exactly one does.
    pub current: Option<String>,
    /// csm's runtime dir `D`.
    pub runtime_dir: PathBuf,
    /// `D`'s `oauthAccount.accountUuid`, when present.
    pub current_uuid: Option<String>,
    /// Where the credentials live; `None` for a set built by hand (tests)
    /// or when the host could not be resolved.
    pub host: Option<HostCtx>,
}

/// The host facts the usage collector needs to read a grant: the runtime
/// one in `D`, or an inactive account's stash. Holds no secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostCtx {
    pub os: HostOs,
    pub home: PathBuf,
    pub user_data: PathBuf,
    pub paths: RuntimePaths,
    pub keychain: KeychainUser,
}

impl AccountSet {
    /// Load from the real machine. Never fails: an unreadable store is an
    /// empty set (usage and scoring then have nothing to work with, which is
    /// what an empty registry used to mean).
    pub fn load() -> AccountSet {
        match HostEnv::current() {
            Ok(env) => AccountSet::load_with(&env),
            Err(_) => AccountSet::default(),
        }
    }

    /// [`AccountSet::load`] with a launch's `CLAUDE_CONFIG_DIR` pin applied
    /// (see [`crate::launch_context::ConfigDirPin::apply_to`]).
    pub fn load_pinned(pin: &crate::launch_context::ConfigDirPin) -> AccountSet {
        match HostEnv::current() {
            Ok(mut env) => {
                pin.apply_to(&mut env);
                AccountSet::load_with(&env)
            }
            Err(_) => AccountSet::default(),
        }
    }

    /// Load for an explicit environment. Reads files only.
    pub fn load_with(env: &HostEnv) -> AccountSet {
        AccountSet::load_from(env, None)
    }

    /// [`AccountSet::load`], but with Orca's own list when Orca runs: RPC
    /// `accounts.list{refreshUsage:false}` (Orca's memory), falling back to
    /// the store when Orca is stopped or does not answer within
    /// [`LIVE_LIST_TIMEOUT`]. From Orca 1.4.214 on the store is a SQLite
    /// export Orca rewrites only when it quits, so while Orca runs it lacks
    /// accounts added (and still lists accounts removed) since. For the
    /// callers outside the hook and the statusline, which stay on csm's own
    /// files (design decision 8): the limit-switch leader, `csm usage` and
    /// the usage collection.
    pub fn load_live() -> AccountSet {
        match HostEnv::current() {
            Ok(env) => AccountSet::load_live_with(&env),
            Err(_) => AccountSet::default(),
        }
    }

    /// [`AccountSet::load_live`] for an explicit environment.
    pub fn load_live_with(env: &HostEnv) -> AccountSet {
        AccountSet::load_from(env, Some(LIVE_LIST_TIMEOUT))
    }

    fn load_from(env: &HostEnv, rpc_timeout: Option<std::time::Duration>) -> AccountSet {
        let live_pid = |dir: &Path| {
            rpc::read_runtime_metadata(dir)
                .ok()
                .flatten()
                .is_some_and(|m| m.pid != 0 && crate::platform::proc::is_running(m.pid))
        };
        let ud = userdata::resolve(env, live_pid);
        let paths =
            runtime::runtime_paths(env.claude_config_dir.as_deref(), &env.home, |p| p.exists());
        let view = store::load_choice(&userdata::data_file(&ud.dir))
            .ok()
            .flatten()
            .and_then(|f| StoreView::from_bytes(&f.bytes).ok());
        let live = rpc_timeout
            .filter(|_| ud.store_access_allowed() && live_pid(&ud.dir))
            .and_then(|t| rpc::accounts_list(&ud.dir, false, t).ok());
        // (every record, the normalized host active id)
        let from_orca = live.is_some();
        let view = match live {
            Some(snap) => Some(live_list(snap, view.as_ref())),
            None => view.map(|v| {
                let active = n6i(&v.active, &v.accounts).host;
                (v.accounts, active)
            }),
        };
        let host = HostCtx {
            os: env.os,
            home: env.home.clone(),
            user_data: ud.dir.clone(),
            paths: paths.clone(),
            keychain: KeychainUser::from_env(env),
        };
        let Some((all, active)) = view else {
            return AccountSet {
                current_uuid: uuid_of(&runtime::read_runtime_identity(&paths)),
                runtime_dir: paths.config_dir,
                host: Some(host),
                ..AccountSet::default()
            };
        };
        let records: Vec<_> = all.into_iter().filter(|a| a.is_host()).collect();
        let ra = runtime::runtime_account(&paths, &ud.dir, &records);
        let unmatched = matches!(ra.account, Some(UuidMatch::None));
        let mut current = match ra.account {
            Some(UuidMatch::Unique(id)) => Some(id),
            _ => None,
        };
        let mut accounts: Vec<AccountEntry> = records
            .iter()
            .map(|a| AccountEntry {
                id: a.id.clone(),
                email: a.email.clone(),
                organization_name: a.organization_name.clone(),
                managed_auth_path: a.managed_auth_path.clone(),
            })
            .collect();
        // A SQLite-backed profile's store is an export frozen at Orca's last
        // quit: an account added since has a stash but no record. When `D`
        // names no listed account, look for it among those stashes (files
        // only, so the hook and the statusline may do it too).
        if !from_orca
            && unmatched
            && userdata::data_file(&ud.dir).has_state_db()
            && let Some(u) = uuid_of(&ra.identity)
        {
            let extra = runtime::unlisted_stash_identities(&ud.dir, &records);
            if let UuidMatch::Unique(id) = runtime::match_account_uuid(&u, &extra) {
                let email = extra
                    .iter()
                    .find(|(i, _)| *i == id)
                    .and_then(|(_, ident)| ident.as_ref()?.email.clone());
                accounts.push(AccountEntry {
                    id: id.clone(),
                    email,
                    organization_name: None,
                    managed_auth_path: None,
                });
                current = Some(id);
            }
        }
        AccountSet {
            accounts,
            active,
            from_orca,
            current,
            current_uuid: uuid_of(&ra.identity),
            runtime_dir: paths.config_dir,
            host: Some(host),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.accounts.is_empty()
    }

    /// Account ids, sorted (stable iteration for usage collection).
    pub fn ids_sorted(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self.accounts.iter().map(|a| a.id.as_str()).collect();
        v.sort_unstable();
        v
    }

    pub fn get(&self, id: &str) -> Option<&AccountEntry> {
        self.accounts.iter().find(|a| a.id == id)
    }

    pub fn contains(&self, id: &str) -> bool {
        self.get(id).is_some()
    }

    /// The label for `id` (see [`AccountEntry::label`]); an unknown id labels
    /// as its own prefix.
    pub fn label(&self, id: &str) -> String {
        match self.get(id) {
            Some(a) => a.label(),
            None => label_for(id, None),
        }
    }
}

/// How long [`AccountSet::load_live`] waits for Orca's `accounts.list`.
pub const LIVE_LIST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// Orca's live list as (records, host active id): RPC records carry no
/// `managedAuthPath`, so each takes the stash path the store names for the
/// same id. Pure.
fn live_list(
    snap: rpc::AccountsSnapshot,
    store: Option<&StoreView>,
) -> (Vec<crate::orca::record::AccountRecord>, Option<String>) {
    let accounts = snap
        .claude
        .accounts
        .into_iter()
        .map(|a| {
            let path = store
                .and_then(|s| s.account(&a.id))
                .and_then(|r| r.managed_auth_path.clone());
            crate::orca::record::AccountRecord {
                managed_auth_path: path.or(a.managed_auth_path),
                ..a
            }
        })
        .collect();
    (accounts, snap.claude.active_by_runtime.host)
}

/// `D`'s `accountUuid` from its identity.
fn uuid_of(identity: &RuntimeIdentity) -> Option<String> {
    match identity {
        RuntimeIdentity::Present(i) => i.account_uuid.clone(),
        _ => None,
    }
}

/// See [`AccountEntry::label`]. Pure.
pub fn label_for(id: &str, email: Option<&str>) -> String {
    if let Some(local) = email
        .map(str::trim)
        .and_then(|e| e.split('@').next())
        .filter(|l| !l.is_empty())
    {
        return local.to_owned();
    }
    id.chars().take(8).collect()
}

/// `true` when `id` may name a file under csm's state dir: 1-128 chars of
/// `[A-Za-z0-9_-]`. Orca's ids are random UUIDs, which pass. Pure.
pub fn is_valid_key(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

// ─── lookup ───────────────────────────────────────────────────────────────────

/// What [`find`] made of a query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup<'a> {
    Found(&'a AccountEntry),
    NotFound,
    /// More than one account fits; their ids.
    Ambiguous(Vec<&'a str>),
}

/// Resolve `query` to one account: an exact id, else an email (trimmed,
/// case-insensitive; two accounts can share an email across organizations),
/// else a unique id prefix. Pure.
pub fn find<'a>(accounts: &'a [AccountEntry], query: &str) -> Lookup<'a> {
    let q = query.trim();
    if q.is_empty() {
        return Lookup::NotFound;
    }
    if let Some(a) = accounts.iter().find(|a| a.id == q) {
        return Lookup::Found(a);
    }
    let by_email: Vec<&AccountEntry> = accounts
        .iter()
        .filter(|a| {
            a.email
                .as_deref()
                .is_some_and(|e| e.trim().eq_ignore_ascii_case(q))
        })
        .collect();
    match by_email.len() {
        1 => return Lookup::Found(by_email[0]),
        0 => {}
        _ => return Lookup::Ambiguous(by_email.iter().map(|a| a.id.as_str()).collect()),
    }
    let by_prefix: Vec<&AccountEntry> = accounts.iter().filter(|a| a.id.starts_with(q)).collect();
    match by_prefix.len() {
        0 => Lookup::NotFound,
        1 => Lookup::Found(by_prefix[0]),
        _ => Lookup::Ambiguous(by_prefix.iter().map(|a| a.id.as_str()).collect()),
    }
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orca::HostOs;

    fn entry(id: &str, email: Option<&str>) -> AccountEntry {
        AccountEntry {
            id: id.into(),
            email: email.map(str::to_owned),
            organization_name: None,
            managed_auth_path: None,
        }
    }

    fn sample() -> Vec<AccountEntry> {
        vec![
            entry("a1b2c3d4-0000", Some("alice@example.com")),
            entry("a1ffffff-0000", Some("bob@example.com")),
            entry("c0ffee00-0000", Some("Bob@Example.com")),
            entry("d00d0000-0000", None),
        ]
    }

    #[test]
    fn find_exact_id_first() {
        let a = sample();
        assert_eq!(find(&a, "d00d0000-0000"), Lookup::Found(&a[3]));
    }

    #[test]
    fn find_by_email_is_case_insensitive_and_trimmed() {
        let a = sample();
        assert_eq!(find(&a, " ALICE@example.com "), Lookup::Found(&a[0]));
    }

    #[test]
    fn a_shared_email_is_ambiguous() {
        let a = sample();
        assert_eq!(
            find(&a, "bob@example.com"),
            Lookup::Ambiguous(vec!["a1ffffff-0000", "c0ffee00-0000"])
        );
    }

    #[test]
    fn find_by_unique_prefix() {
        let a = sample();
        assert_eq!(find(&a, "c0f"), Lookup::Found(&a[2]));
        assert_eq!(
            find(&a, "a1"),
            Lookup::Ambiguous(vec!["a1b2c3d4-0000", "a1ffffff-0000"])
        );
        assert_eq!(find(&a, "zz"), Lookup::NotFound);
        assert_eq!(find(&a, "  "), Lookup::NotFound);
    }

    #[test]
    fn labels_use_the_email_local_part_else_the_id_prefix() {
        assert_eq!(
            label_for("a1b2c3d4-0000", Some("alice@example.com")),
            "alice"
        );
        assert_eq!(label_for("a1b2c3d4-0000", None), "a1b2c3d4");
        assert_eq!(label_for("a1b2c3d4-0000", Some("@example.com")), "a1b2c3d4");
    }

    #[test]
    fn valid_keys() {
        assert!(is_valid_key("0f8e1c2a-1111-4222-8333-944455556666"));
        assert!(!is_valid_key("../evil"));
        assert!(!is_valid_key(""));
        assert!(!is_valid_key("a/b"));
        assert!(!is_valid_key(&"x".repeat(129)));
    }

    #[test]
    fn label_names_known_and_unknown_ids() {
        let set = AccountSet {
            accounts: sample(),
            active: Some("a1b2c3d4-0000".into()),
            current: Some("a1b2c3d4-0000".into()),
            runtime_dir: PathBuf::from("/Users/example/.claude"),
            ..AccountSet::default()
        };
        assert_eq!(set.label("c0ffee00-0000"), "Bob");
        assert_eq!(set.label("unknown-id"), "unknown-");
    }

    #[test]
    fn load_reads_the_store_and_maps_d() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let mut env = HostEnv::for_test(home, HostOs::Linux);
        let d = home.join("claude-d");
        env.claude_config_dir = Some(d.to_string_lossy().into_owned());
        let ud = home.join(".config/orca");
        let file = ud.join("profiles/local-default/orca-data.json");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        // JSON-escaped, so a Windows path with backslashes stays valid.
        let auth =
            serde_json::to_string(&crate::orca::stash::default_auth_dir(&ud, "id-a")).unwrap();
        std::fs::write(
            &file,
            format!(
                concat!(
                    r#"{{"schemaVersion":1,"settings":{{"claudeManagedAccounts":["#,
                    r#"{{"id":"id-a","email":"alice@example.com","managedAuthPath":{auth},"managedAuthRuntime":"host"}},"#,
                    r#"{{"id":"id-w","email":"bob@example.com","managedAuthRuntime":"wsl","wslDistro":"Ubuntu"}}"#,
                    r#"],"activeClaudeManagedAccountId":"id-a","activeClaudeManagedAccountIdsByRuntime":{{"host":"id-a","wsl":{{}}}}}}}}"#
                ),
                auth = auth
            ),
        )
        .unwrap();
        crate::orca::testsupport::make_stash(&ud, "id-a", Some(br#"{"accountUuid":"u-a"}"#), None);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join(".claude.json"),
            r#"{"oauthAccount":{"accountUuid":"u-a"}}"#,
        )
        .unwrap();

        let set = AccountSet::load_with(&env);
        assert_eq!(
            set.ids_sorted(),
            vec!["id-a"],
            "WSL accounts are not listed"
        );
        assert_eq!(set.active.as_deref(), Some("id-a"));
        assert_eq!(set.current.as_deref(), Some("id-a"));
        assert_eq!(set.runtime_dir, d);
    }

    /// Orca 1.4.214 profile with `profile-state.db`: `orca-data.json` is the
    /// export from Orca's last quit, so an account added (and made active)
    /// since has a stash but no record. `D`'s identity still maps to it,
    /// from the stash's own `oauth-account.json`, with no RPC. Without the
    /// database a stash no record names is an orphan and never counts.
    #[test]
    fn a_sqlite_profile_maps_d_to_a_stash_the_export_lacks() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let mut env = HostEnv::for_test(home, HostOs::Linux);
        let d = home.join("claude-d");
        env.claude_config_dir = Some(d.to_string_lossy().into_owned());
        let ud = home.join(".config/orca");
        let file = ud.join("profiles/local-default/orca-data.json");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let auth =
            serde_json::to_string(&crate::orca::stash::default_auth_dir(&ud, "id-a")).unwrap();
        std::fs::write(
            &file,
            format!(
                concat!(
                    r#"{{"schemaVersion":1,"settings":{{"claudeManagedAccounts":["#,
                    r#"{{"id":"id-a","email":"alice@example.com","managedAuthPath":{auth},"managedAuthRuntime":"host"}}"#,
                    r#"],"activeClaudeManagedAccountId":"id-a","activeClaudeManagedAccountIdsByRuntime":{{"host":"id-a","wsl":{{}}}}}}}}"#
                ),
                auth = auth
            ),
        )
        .unwrap();
        crate::orca::testsupport::make_stash(&ud, "id-a", Some(br#"{"accountUuid":"u-a"}"#), None);
        crate::orca::testsupport::make_stash(
            &ud,
            "id-new",
            Some(br#"{"accountUuid":"u-new","emailAddress":"carol@example.com"}"#),
            None,
        );
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join(".claude.json"),
            r#"{"oauthAccount":{"accountUuid":"u-new"}}"#,
        )
        .unwrap();

        let set = AccountSet::load_with(&env);
        assert_eq!(
            set.current, None,
            "no database: an unlisted stash is an orphan"
        );
        assert_eq!(set.ids_sorted(), vec!["id-a"]);

        std::fs::write(file.with_file_name(crate::orca::userdata::STATE_DB), b"").unwrap();
        let set = AccountSet::load_with(&env);
        assert_eq!(set.current.as_deref(), Some("id-new"));
        assert_eq!(set.current_uuid.as_deref(), Some("u-new"));
        assert_eq!(set.label("id-new"), "carol");
        assert_eq!(set.active.as_deref(), Some("id-a"));
    }

    /// Orca's live list: every record it names, with the store's
    /// `managedAuthPath` where the store knows the id (RPC records carry
    /// none), `None` for an account added since the export (the stash then
    /// opens at its default path), and Orca's host active id.
    #[test]
    fn live_list_joins_the_store_paths_and_takes_orcas_active_id() {
        let store = StoreView::from_bytes(
            concat!(
                r#"{"schemaVersion":1,"settings":{"claudeManagedAccounts":["#,
                r#"{"id":"id-a","email":"alice@example.com","managedAuthPath":"/Users/example/custom/id-a/auth","managedAuthRuntime":"host"},"#,
                r#"{"id":"id-gone","email":"gone@example.com","managedAuthPath":"/Users/example/custom/id-gone/auth","managedAuthRuntime":"host"}"#,
                r#"],"activeClaudeManagedAccountId":"id-a","activeClaudeManagedAccountIdsByRuntime":{"host":"id-a","wsl":{}}}}"#
            )
            .as_bytes(),
        )
        .unwrap();
        let snap = rpc::parse_accounts_snapshot(&serde_json::json!({
            "claude": {
                "accounts": [
                    {"id": "id-a", "email": "alice@example.com", "managedAuthRuntime": "host"},
                    {"id": "id-new", "email": "new@example.com", "managedAuthRuntime": "host"},
                    {"id": "id-w", "email": "bob@example.com", "managedAuthRuntime": "wsl", "wslDistro": "Ubuntu"}
                ],
                "activeAccountId": "id-new",
                "activeAccountIdsByRuntime": {"host": "id-new", "wsl": {}}
            },
            "rateLimits": {"claude": null, "inactiveClaudeAccounts": []}
        }))
        .unwrap();

        let (recs, active) = live_list(snap.clone(), Some(&store));
        let ids: Vec<&str> = recs.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["id-a", "id-new", "id-w"],
            "the store's id-gone is not Orca's"
        );
        assert_eq!(
            recs[0].managed_auth_path.as_deref(),
            Some("/Users/example/custom/id-a/auth")
        );
        assert_eq!(recs[1].managed_auth_path, None);
        assert!(
            !recs[2].is_host(),
            "the WSL record is kept for the caller to filter"
        );
        assert_eq!(active.as_deref(), Some("id-new"));

        // No store at all: the RPC records as they came.
        let (recs, _) = live_list(snap, None);
        assert!(recs.iter().all(|r| r.managed_auth_path.is_none()));
    }

    /// `load_live_with` asks a running Orca, so an account that exists only
    /// in Orca's memory is listed; `load_with` (the hook's and statusline's
    /// read) stays on the store and never touches the socket.
    #[cfg(unix)]
    #[test]
    fn load_live_asks_orca_and_load_stays_on_the_store() {
        use crate::orca::testsupport::{
            FakeOrca, OrcaModel, model_handler, record_json, write_store,
        };
        use std::sync::{Arc, Mutex};

        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let model = Arc::new(Mutex::new(OrcaModel::default()));
        let orca = FakeOrca::start(model_handler(model.clone()));
        let ud = orca.user_data().to_path_buf();
        let a = record_json(&ud, "id-a", "alice@example.com", None);
        write_store(&ud, std::slice::from_ref(&a), Some("id-a"));
        {
            let mut m = model.lock().unwrap();
            let mut a_live = a;
            a_live.as_object_mut().unwrap().remove("managedAuthPath");
            m.accounts = vec![
                a_live,
                serde_json::json!({"id": "id-new", "email": "new@example.com", "managedAuthRuntime": "host"}),
            ];
            m.active = Some("id-new".into());
        }
        let mut env = HostEnv::for_test(home, HostOs::Linux);
        env.orca_user_data_path = Some(ud.to_string_lossy().into_owned());
        env.claude_config_dir = Some(home.join("claude-d").to_string_lossy().into_owned());

        let stored = AccountSet::load_with(&env);
        assert!(orca.requests().is_empty(), "load_with must not call Orca");
        assert!(!stored.from_orca);
        assert_eq!(stored.ids_sorted(), vec!["id-a"]);
        assert_eq!(stored.active.as_deref(), Some("id-a"));

        let live = AccountSet::load_live_with(&env);
        assert!(live.from_orca);
        assert_eq!(live.ids_sorted(), vec!["id-a", "id-new"]);
        assert_eq!(live.active.as_deref(), Some("id-new"));
        let store_path = default_auth_dir_of(&ud, "id-a");
        assert_eq!(
            live.get("id-a").unwrap().managed_auth_path.as_deref(),
            Some(store_path.as_str())
        );
        assert_eq!(live.get("id-new").unwrap().managed_auth_path, None);
        let methods: Vec<String> = orca
            .requests()
            .iter()
            .map(|r| r["method"].as_str().unwrap_or("").to_owned())
            .collect();
        assert_eq!(methods, vec!["accounts.list".to_owned()]);
    }

    #[cfg(unix)]
    fn default_auth_dir_of(ud: &Path, id: &str) -> String {
        crate::orca::testsupport::record_json(ud, id, "x@example.com", None)["managedAuthPath"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    #[test]
    fn load_without_a_test_home_is_empty() {
        crate::testenv::set_test_home(None);
        assert_eq!(AccountSet::load(), AccountSet::default());
    }
}
