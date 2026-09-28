//! Adding, importing and removing Orca Claude accounts (design sections 2
//! and 6), ported from Orca 1.4.209: n9i.add/addFromConfigDir/persist
//! (M:247913-248000), r9i.remove (M:248039-248060), o9i/s9i/c9i/l9i/u9i
//! (M:248137-248205) and the login E9i/D9i (M:248390-248465).
//!
//! Routes:
//! - `accounts add` with Orca running: the cmd layer execs Orca's own CLI
//!   ([`ORCA_ADD_ARGV`]), found by [`orca_cli_candidates`]; stopped: [`login_add`], a port of Orca's login
//!   (a `mkdtemp` dir `orca-claude-login-XXXXXX`, `claude auth login
//!   --claudeai` then `claude auth status --json` with `CLAUDE_CONFIG_DIR`
//!   and `CLAUDE_SECURESTORAGE_CONFIG_DIR` set to it, capture, then on macOS
//!   delete the dir's scoped item and put the unscoped item back as it was);
//! - import ([`import`]): `accounts.addClaudeFromConfigDir` over RPC with
//!   Orca running, else the port of addFromConfigDir, including
//!   `previousLegacyCredentialsSha256` (c9i: on macOS the unscoped item is
//!   taken only when its sha256 differs from that digest);
//! - rm ([`remove`]): `accounts.removeClaude` over RPC, or offline for a
//!   host account that is not active: the store first, the stash only after
//!   L2 passed.
//!
//! Offline, the stash is created (random UUID, marker, credentials,
//! `oauth-account.json`) before the store patch that names it, and removed
//! again when the patch does not land. Identity dedupe is e9i
//! (lower-trimmed email, trimmed org uuid, runtime), checked before the
//! stash is created and again inside the patch. Every store write goes
//! through the store-write protocol under `switch.lock`.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::fsx::{self, SwitchLock};
use super::keychain::{self, KeychainUser};
use super::live::Liveness;
use super::quarantine::{Quarantine, Reason};
use super::record::{self, AuthRuntime, IdentityKey, NewRecord, find_by_identity, t6i};
use super::rpc;
use super::stash::{self, Stash};
use super::store::{self, Patch, RedoOp, RedoOpts, RedoOutcome, StoreView, StoreWrite};
use super::userdata::{DataFileChoice, HostOs};
use super::{OrcaError, SecretString};

/// Orca's own CLI for an interactive add while Orca runs.
#[cfg_attr(
    test,
    allow(dead_code, reason = "the exec that uses it is not built in tests")
)]
pub const ORCA_ADD_ARGV: [&str; 4] = ["account", "add", "--agent", "claude"];
/// Where packaged Linux Orca installs live (Orca's
/// PACKAGED_LINUX_LAUNCHER_DIRECTORIES, cli-command-inspection.ts).
pub const LINUX_INSTALL_DIRS: [&str; 3] = ["/opt/Orca", "/opt/orca-ide", "/opt/orca"];

/// Orca's CLI name per OS (bundled-cli-launcher-path.ts): `orca` on macOS,
/// `orca.exe` on Windows, `orca-ide` on Linux, where `/usr/bin/orca` is
/// GNOME's screen reader.
pub fn orca_cli_name(os: HostOs) -> &'static str {
    match os {
        HostOs::MacOs => "orca",
        HostOs::Windows => "orca.exe",
        HostOs::Linux => "orca-ide",
    }
}

/// Where to look for Orca's CLI for `orca account add`, in order (binding
/// decision): the bundled launcher of the running Orca (`<resources>/bin/<cli>`
/// beside its main executable), then of each install candidate, then each
/// `PATH` dir. `installs` are macOS `.app` bundles, Windows `Orca.exe`
/// paths, or Linux install dirs; `resources` is `Contents/Resources` in a
/// bundle and `resources` beside the executable elsewhere. On Linux the
/// `PATH` name is `orca-ide`, never `orca` (the screen reader). Pure.
pub fn orca_cli_candidates(
    os: HostOs,
    main_exe: Option<&Path>,
    installs: &[PathBuf],
    path_dirs: &[PathBuf],
) -> Vec<PathBuf> {
    let name = orca_cli_name(os);
    let mut out: Vec<PathBuf> = Vec::new();
    let mut push = |p: PathBuf| {
        if !out.contains(&p) {
            out.push(p);
        }
    };
    let from_exe = |exe: &Path| -> Option<PathBuf> {
        match os {
            HostOs::MacOs => super::live::bundle_of(exe)
                .map(|b| b.join("Contents").join("Resources").join("bin").join(name)),
            HostOs::Windows | HostOs::Linux => exe
                .parent()
                .map(|d| d.join("resources").join("bin").join(name)),
        }
    };
    if let Some(p) = main_exe.and_then(from_exe) {
        push(p);
    }
    for i in installs {
        let p = match os {
            HostOs::MacOs => i.join("Contents").join("Resources").join("bin").join(name),
            HostOs::Windows => match i.parent() {
                Some(d) => d.join("resources").join("bin").join(name),
                None => continue,
            },
            HostOs::Linux => i.join("resources").join("bin").join(name),
        };
        push(p);
    }
    for d in path_dirs.iter().filter(|d| d.is_absolute()) {
        push(d.join(name));
    }
    out
}

/// The login dir prefix (D9i).
pub const LOGIN_DIR_PREFIX: &str = "orca-claude-login-";
/// `claude auth login` timeout (w9i).
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(180);
/// `claude auth status` timeout (T9i / a9i).
pub const STATUS_TIMEOUT: Duration = Duration::from_secs(20);
/// The longest [`login_add`] holds `switch.lock`: the login, the status,
/// and, when Orca came up meanwhile, the redo's wait for its socket plus
/// `addClaudeFromConfigDir` (the Keychain cleanup after that is quick).
pub const LOGIN_LOCK_HOLD: Duration = Duration::from_secs(
    LOGIN_TIMEOUT.as_secs()
        + STATUS_TIMEOUT.as_secs()
        + store::REDO_WAIT.as_secs()
        + super::rpc::ADD_TIMEOUT.as_secs(),
);

// ─── the claude seam ──────────────────────────────────────────────────────────

/// What a `claude` run produced.
#[derive(Debug, Clone)]
pub struct CliOutput {
    pub success: bool,
    /// stdout (empty for an interactive run).
    pub stdout: String,
}

/// Running `claude` against a config dir.
pub trait ClaudeCli {
    /// Run `claude <args>` with `CLAUDE_CONFIG_DIR` and
    /// `CLAUDE_SECURESTORAGE_CONFIG_DIR` set to `config_dir`. `interactive`
    /// inherits the terminal; otherwise stdout is captured.
    fn run(
        &self,
        args: &[&str],
        config_dir: &Path,
        timeout: Duration,
        interactive: bool,
    ) -> io::Result<CliOutput>;
}

/// The real `claude`: `program` plus the tokens a configured launch command
/// puts before claude's own arguments (`npx happy …`).
#[derive(Debug, Clone)]
pub struct SystemClaude {
    pub program: PathBuf,
    pub prefix: Vec<std::ffi::OsString>,
}

impl SystemClaude {
    /// From a launch command's tokens (`config::launch_command_for_spawn`):
    /// the first is the program, the rest go before claude's arguments.
    /// `None` for an empty list. Pure.
    pub fn from_launch_command(tokens: Vec<std::ffi::OsString>) -> Option<SystemClaude> {
        let mut it = tokens.into_iter();
        let program = PathBuf::from(it.next()?);
        Some(SystemClaude {
            program,
            prefix: it.collect(),
        })
    }

    /// The claude csm's own `auth` calls run (binding decision): the
    /// configured launch command when one is set, else `claude` on `PATH`,
    /// skipping csm itself (`config::launch_command_for_spawn`).
    pub fn configured() -> io::Result<SystemClaude> {
        SystemClaude::from_launch_command(crate::config::launch_command_for_spawn()?)
            .ok_or_else(|| io::Error::other("empty launch command"))
    }
}

impl ClaudeCli for SystemClaude {
    #[cfg(test)]
    fn run(&self, _: &[&str], _: &Path, _: Duration, _: bool) -> io::Result<CliOutput> {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "tests never run the real claude",
        ))
    }

    #[cfg(not(test))]
    fn run(
        &self,
        args: &[&str],
        config_dir: &Path,
        timeout: Duration,
        interactive: bool,
    ) -> io::Result<CliOutput> {
        use std::io::Read;
        use std::process::{Command, Stdio};
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.prefix)
            .args(args)
            .env("CLAUDE_CONFIG_DIR", config_dir)
            .env("CLAUDE_SECURESTORAGE_CONFIG_DIR", config_dir);
        if interactive {
            cmd.stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit());
        } else {
            cmd.stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
        }
        // A non-interactive run gets its own process group so a timeout
        // kill reaches what it started; the interactive login keeps csm's
        // group, since it reads the terminal.
        let grouped = !interactive;
        if grouped {
            crate::platform::child::own_group(&mut cmd);
        }
        let mut child = cmd.spawn()?;
        let reader = child.stdout.take().map(|mut out| {
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let mut s = String::new();
                let _ = out.read_to_string(&mut s);
                let _ = tx.send(s);
            });
            rx
        });
        let status = crate::platform::child::wait_deadline(
            &mut child,
            timeout,
            Duration::from_millis(50),
            grouped,
        )?;
        // Bounded: a background process left holding stdout cannot hang us.
        let stdout = reader
            .and_then(|rx| rx.recv_timeout(crate::platform::child::REAP_LIMIT).ok())
            .unwrap_or_default();
        match status {
            Some(st) => Ok(CliOutput {
                success: st.success(),
                stdout,
            }),
            None => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "claude did not finish in time",
            )),
        }
    }
}

// ─── pure core ────────────────────────────────────────────────────────────────

/// The identity Orca records for a captured login (u9i).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CapturedIdentity {
    pub email: Option<String>,
    pub organization_uuid: Option<String>,
    pub organization_name: Option<String>,
}

fn as_obj(v: Option<Value>) -> Option<serde_json::Map<String, Value>> {
    match v? {
        Value::Object(m) => Some(m),
        _ => None,
    }
}

/// V3 chains: the first key whose value is a string (even blank), then p9i
/// (trim, blank as null).
fn first_str(sources: &[(Option<&serde_json::Map<String, Value>>, &str)]) -> Option<String> {
    let s = sources
        .iter()
        .find_map(|(o, k)| o.and_then(|o| o.get(*k)).and_then(Value::as_str))?;
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_owned())
}

/// u9i over `claude auth status --json`'s stdout, the config's
/// `oauthAccount` and the captured grant. Pure.
pub fn identity_from(status: &str, oauth: Option<&Value>, creds: &str) -> CapturedIdentity {
    let r = as_obj(serde_json::from_str(status).ok());
    let i = oauth.and_then(Value::as_object);
    let a = as_obj(
        serde_json::from_str::<Value>(creds)
            .ok()
            .and_then(|v| v.get("claudeAiOauth").cloned()),
    );
    CapturedIdentity {
        email: first_str(&[
            (r.as_ref(), "email"),
            (i, "emailAddress"),
            (i, "email"),
            (a.as_ref(), "email"),
        ]),
        organization_uuid: first_str(&[
            (r.as_ref(), "organizationUuid"),
            (r.as_ref(), "organizationId"),
            (i, "organizationUuid"),
            (i, "organizationId"),
        ]),
        organization_name: first_str(&[(r.as_ref(), "organizationName"), (i, "organizationName")]),
    }
}

/// JS truthiness of a JSON value.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// l9i over the two files' bytes (`.claude.json`, then `.config.json`).
/// Pure.
pub fn config_oauth_account(files: &[Option<&[u8]>]) -> Option<Value> {
    for b in files.iter().flatten() {
        let Ok(v) = serde_json::from_slice::<Value>(b) else {
            continue;
        };
        if let Some(o) = v.get("oauthAccount").filter(|o| truthy(o)) {
            return Some(o.clone());
        }
    }
    None
}

/// Lowercase hex sha256 (the legacy credential digest).
pub fn sha256_hex(bytes: &[u8]) -> String {
    keychain::hex_lower(&Sha256::digest(bytes))
}

/// c9i: which captured grant to keep. `scoped` is the dir's scoped item,
/// `unscoped` the unscoped item now, `prev_unscoped` the unscoped item
/// before the login (Orca's `t`), `prev_sha` the caller's legacy digest
/// (`None` = not given), `file` the dir's `.credentials.json`. Pure.
pub fn choose_captured(
    os: HostOs,
    scoped: Option<String>,
    unscoped: Option<String>,
    prev_unscoped: Option<&str>,
    prev_sha: Option<&str>,
    file: Option<String>,
) -> Option<String> {
    if os == HostOs::MacOs {
        if scoped.is_some() {
            return scoped;
        }
        let changed = match prev_sha {
            None => unscoped.as_deref() != prev_unscoped,
            Some(sha) => unscoped
                .as_deref()
                .is_some_and(|u| sha256_hex(u.as_bytes()) != sha),
        };
        if unscoped.is_some() && changed {
            return unscoped;
        }
    }
    file
}

// ─── environment ──────────────────────────────────────────────────────────────

/// Everything an account mutation touches.
pub struct AccountsEnv<'a> {
    pub os: HostOs,
    pub user_data: &'a Path,
    pub data_file: &'a DataFileChoice,
    pub state: &'a Path,
    pub keychain_user: &'a KeychainUser,
    pub live: &'a dyn Liveness,
    pub version_ok: bool,
    pub store_access_allowed: bool,
    pub lock_wait: Duration,
    pub redo: RedoOpts,
    /// The timeout of `accounts.addClaudeFromConfigDir` and
    /// `accounts.removeClaude`: Orca queues them behind any running account
    /// mutation, and the add runs `claude auth status` (up to 20 s) inside
    /// it, so they get [`rpc::ADD_TIMEOUT`], the wait Orca's own CLI uses,
    /// never the short list timeout.
    pub mutation_timeout: Duration,
}

/// Which route ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Rpc,
    Offline,
    /// Offline, then Orca appeared; redone over RPC.
    OfflineThenRpc,
}

/// What an add, import or rm did.
#[derive(Debug)]
pub struct AccountChange {
    pub route: Route,
    /// The account id (new for an add; the removed one for rm). `None` when
    /// Orca did the add over RPC.
    pub id: Option<String>,
    pub email: Option<String>,
    pub redo: Option<RedoOutcome>,
    /// A cleanup that failed and was left for `accounts doctor` (no
    /// secrets).
    pub leftover: Option<String>,
}

fn offline_gate(env: &AccountsEnv<'_>) -> Result<(), OrcaError> {
    if !env.version_ok {
        return Err(OrcaError::Refused(
            "the installed Orca version is not one csm was tested with; start Orca".into(),
        ));
    }
    if !env.store_access_allowed {
        return Err(OrcaError::Refused(
            "this userData is not csm's to write".into(),
        ));
    }
    store::sqlite_gate(env.data_file)
}

fn lock(env: &AccountsEnv<'_>) -> Result<SwitchLock, OrcaError> {
    SwitchLock::acquire(env.state, env.lock_wait)
        .map_err(|e| OrcaError::io("cannot take", &env.state.join(fsx::SWITCH_LOCK), e))
}

// ─── capture ──────────────────────────────────────────────────────────────────

/// A captured login: the exact grant, `oauthAccount`, identity.
pub struct Capture {
    pub creds: SecretString,
    pub oauth: Option<Value>,
    pub identity: CapturedIdentity,
}

impl std::fmt::Debug for Capture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Capture")
            .field("creds", &"<redacted>")
            .field("identity", &self.identity)
            .finish()
    }
}

fn read_small(p: &Path) -> Result<Option<Vec<u8>>, OrcaError> {
    super::read_capped_bytes(p, 64 * 1024 * 1024).map_err(|e| OrcaError::io("cannot read", p, e))
}

/// s9i over `dir` (c9i + l9i + u9i).
fn capture_dir(
    env: &AccountsEnv<'_>,
    dir: &Path,
    status: &str,
    prev_unscoped: Option<&str>,
    prev_sha: Option<&str>,
) -> Result<Capture, OrcaError> {
    let (scoped, unscoped) = if env.os == HostOs::MacOs {
        let d = dir.to_string_lossy().into_owned();
        let own = |r: Option<SecretString>| r.map(|s| s.expose().to_owned());
        (
            own(keychain::read_runtime_scoped(Some(&d), env.keychain_user)?),
            own(keychain::read_runtime_scoped(None, env.keychain_user)?),
        )
    } else {
        (None, None)
    };
    let file = read_small(&dir.join(stash::CREDENTIALS_FILE))?
        .map(|b| {
            String::from_utf8(b).map_err(|_| OrcaError::Invalid("credentials are not UTF-8".into()))
        })
        .transpose()?;
    let creds = choose_captured(env.os, scoped, unscoped, prev_unscoped, prev_sha, file)
        .ok_or_else(|| {
            OrcaError::Refused(
                "Claude login completed, but no OAuth credentials were captured".into(),
            )
        })?;
    let cfg = read_small(&dir.join(".claude.json"))?;
    let cfg2 = read_small(&dir.join(".config.json"))?;
    let oauth = config_oauth_account(&[cfg.as_deref(), cfg2.as_deref()]);
    let identity = identity_from(status, oauth.as_ref(), &creds);
    Ok(Capture {
        creds: SecretString::new(creds),
        oauth,
        identity,
    })
}

// ─── persist ──────────────────────────────────────────────────────────────────

fn identity_key(id: &CapturedIdentity) -> Result<IdentityKey, OrcaError> {
    IdentityKey::new(
        id.email.as_deref(),
        id.organization_uuid.as_deref(),
        AuthRuntime::Host,
        None,
    )
    .ok_or_else(|| {
        OrcaError::Refused(
            "Claude login completed, but csm could not resolve the account email".into(),
        )
    })
}

/// persist(): stash first, then the store patch, under the lock. `redo`
/// is what to reissue over RPC when Orca appears.
fn persist(
    env: &AccountsEnv<'_>,
    cap: &Capture,
    redo: Option<RedoOp>,
) -> Result<AccountChange, OrcaError> {
    let key = identity_key(&cap.identity)?;
    let email = cap.identity.email.clone();
    let dup = || OrcaError::Refused("This Claude account is already added.".into());
    if let Some(v) = store::load_choice(env.data_file)?
        .map(|f| StoreView::from_bytes(&f.bytes))
        .transpose()
        .map_err(|e| OrcaError::Refused(e.to_string()))?
        && find_by_identity(&v.accounts, &key).is_some()
    {
        return Err(dup());
    }
    let id = uuid::Uuid::new_v4().to_string();
    let s = stash::create(env.user_data, &id).map_err(|e| OrcaError::Refused(e.to_string()))?;
    let oauth = cap.oauth.clone().unwrap_or(Value::Null);
    let undo = |s: Stash| s.remove(env.user_data, env.os).map(|_| ()).err();
    if let Err(e) = s.write_auth(env.user_data, env.os, cap.creds.expose(), &oauth) {
        let _ = undo(s);
        return Err(OrcaError::Refused(format!("cannot write the stash: {e}")));
    }
    let path = stash::record_path(&s.auth_dir.to_string_lossy());
    let now = super::now_ms();
    let rec = record::new_record(
        &NewRecord {
            id: &id,
            email: email.as_deref().unwrap_or_default(),
            managed_auth_path: &path,
            organization_uuid: cap.identity.organization_uuid.as_deref(),
            organization_name: cap.identity.organization_name.as_deref(),
        },
        now,
    );
    let mut build = |v: &StoreView| -> Result<Patch, OrcaError> {
        if find_by_identity(&v.accounts, &key).is_some() {
            return Err(dup());
        }
        let mut accounts = v.accounts_raw.clone();
        accounts.push(rec.clone());
        Ok(Patch {
            accounts: Some(accounts),
            active_id: Some(v.active.host.clone()),
            active_by_runtime: Some(v.active.clone()),
        })
    };
    let w = store::write_protocol(env.data_file, true, env.live, None, env.state, &mut build);
    let mut change = AccountChange {
        route: Route::Offline,
        id: Some(id.clone()),
        email,
        redo: None,
        leftover: None,
    };
    match w {
        Ok(StoreWrite::Written) => Ok(change),
        Ok(StoreWrite::Unchanged) => {
            // An add always changes the store; a no-op write means the
            // record did not land, so the stash must not outlive it.
            let _ = undo(s);
            Err(OrcaError::Refused(
                "the Orca store did not take the new account".into(),
            ))
        }
        Ok(StoreWrite::OrcaAtL0 | StoreWrite::OrcaAtL1) => {
            // The store is untouched: the stash is ours alone.
            change.leftover = undo(s).map(|e| format!("stash {id}: {e}"));
            change.id = None;
            change.route = Route::OfflineThenRpc;
            change.redo = redo.map(|op| store::redo_over_rpc(env.user_data, &op, env.redo));
            Ok(change)
        }
        Ok(StoreWrite::OrcaAtL2) => {
            change.route = Route::OfflineThenRpc;
            change.redo = redo.map(|op| store::redo_over_rpc(env.user_data, &op, env.redo));
            Ok(change)
        }
        Err(e) => {
            let _ = undo(s);
            Err(e)
        }
    }
}

// ─── import ───────────────────────────────────────────────────────────────────

/// Import the account logged in under `config_dir` (addFromConfigDir).
/// Orca's first step over an import path (claude-auth-capture.ts): trim it,
/// refuse a blank one, then resolve. JavaScript's `trim` also strips a BOM.
/// A path that is not UTF-8 is used as given. Pure.
pub fn trim_config_dir(config_dir: &Path) -> Result<PathBuf, OrcaError> {
    let trimmed = match config_dir.to_str() {
        Some(s) => PathBuf::from(s.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')),
        None => config_dir.to_path_buf(),
    };
    if trimmed.as_os_str().is_empty() {
        return Err(OrcaError::Refused(
            "A Claude config directory path is required.".into(),
        ));
    }
    Ok(trimmed)
}

pub fn import(
    env: &AccountsEnv<'_>,
    cli: &dyn ClaudeCli,
    config_dir: &Path,
    previous_legacy_sha256: Option<&str>,
) -> Result<AccountChange, OrcaError> {
    import_with(
        env,
        cli,
        config_dir,
        LegacyDigest::Given(previous_legacy_sha256),
    )
}

/// [`import`] with `previousLegacyCredentialsSha256` = the sha256 of the
/// unscoped runtime item as it is once `switch.lock` is held (migration
/// step 4). Read under the lock, a concurrent csm switch cannot change the
/// item between the digest and the capture and make the capture take the
/// switched-in grant for the imported dir's.
pub fn import_current_legacy(
    env: &AccountsEnv<'_>,
    cli: &dyn ClaudeCli,
    config_dir: &Path,
) -> Result<AccountChange, OrcaError> {
    import_with(env, cli, config_dir, LegacyDigest::Current)
}

/// Where an import's legacy digest comes from.
enum LegacyDigest<'a> {
    /// The caller's (`None` = not given).
    Given(Option<&'a str>),
    /// The unscoped item's, read under `switch.lock` (macOS; none elsewhere).
    Current,
}

fn import_with(
    env: &AccountsEnv<'_>,
    cli: &dyn ClaudeCli,
    config_dir: &Path,
    digest: LegacyDigest<'_>,
) -> Result<AccountChange, OrcaError> {
    if let LegacyDigest::Given(Some(sha)) = digest
        && !(sha.len() == 64
            && sha
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()))
    {
        return Err(OrcaError::Refused(
            "Invalid legacy credential digest".into(),
        ));
    }
    let config_dir = trim_config_dir(config_dir)?;
    let dir = std::path::absolute(&config_dir)
        .map_err(|e| OrcaError::io("cannot resolve", &config_dir, e))?;
    let dir_s = dir.to_string_lossy().into_owned();
    let _lock = lock(env)?;
    let current;
    let previous_legacy_sha256 = match digest {
        LegacyDigest::Given(d) => d,
        LegacyDigest::Current => {
            current = if env.os == HostOs::MacOs {
                keychain::read_runtime_scoped(None, env.keychain_user)?
                    .map(|s| sha256_hex(s.expose().as_bytes()))
            } else {
                None
            };
            current.as_deref()
        }
    };
    if env.live.mark().running {
        rpc::add_claude_from_config_dir(
            env.user_data,
            &dir_s,
            previous_legacy_sha256,
            env.mutation_timeout,
        )?;
        return Ok(AccountChange {
            route: Route::Rpc,
            id: None,
            email: None,
            redo: None,
            leftover: None,
        });
    }
    offline_gate(env)?;
    if env.os != HostOs::MacOs && !dir.join(stash::CREDENTIALS_FILE).exists() {
        return Err(OrcaError::Refused(format!(
            "No Claude credentials found in {}. Run `claude login` into this directory first.",
            dir.display()
        )));
    }
    let status = cli
        .run(&["auth", "status", "--json"], &dir, STATUS_TIMEOUT, false)
        .map(|o| o.stdout)
        .unwrap_or_default();
    // o9i reads De.o() once and hands it to c9i as the "before" value.
    let before = if env.os == HostOs::MacOs {
        keychain::read_runtime_scoped(None, env.keychain_user)?.map(|s| s.expose().to_owned())
    } else {
        None
    };
    let cap = capture_dir(
        env,
        &dir,
        &status,
        before.as_deref(),
        previous_legacy_sha256,
    )?;
    let key = identity_key(&cap.identity)?;
    persist(
        env,
        &cap,
        Some(RedoOp::Add {
            config_dir: dir_s,
            previous_legacy_sha256: previous_legacy_sha256.map(str::to_owned),
            identity: key,
        }),
    )
}

// ─── login ────────────────────────────────────────────────────────────────────

/// The `previousLegacyCredentialsSha256` csm hands Orca when Orca imports
/// a login dir csm ran the login in. Without one, Orca's capture compares
/// the unscoped item with its value at the time of the import, not before
/// the login, so a login that wrote only that item (Claude Code before
/// 2.1.220) would be refused although csm's own capture took it. With no
/// item before the login, the digest of the empty string stands for
/// "absent": Orca skips an empty item, so any present one counts as
/// changed, as in [`choose_captured`] with `prev_unscoped = None`. Off
/// macOS Orca reads no Keychain item and no digest is sent. Pure.
pub fn login_legacy_digest(os: HostOs, before: Option<&str>) -> Option<String> {
    (os == HostOs::MacOs).then(|| sha256_hex(before.unwrap_or_default().as_bytes()))
}

/// mkdtemp(`<tmp>/orca-claude-login-`) plus realpath (D9i).
fn make_login_dir() -> Result<PathBuf, OrcaError> {
    let base = std::env::temp_dir();
    for _ in 0..16 {
        let tag: String = uuid::Uuid::new_v4().simple().to_string()[..6].to_owned();
        let p = base.join(format!("{LOGIN_DIR_PREFIX}{tag}"));
        fsx::guard(&p).map_err(|e| OrcaError::io("refusing", &p, e))?;
        match std::fs::DirBuilder::new().create(&p) {
            Ok(()) => {
                let _ = fsx::set_mode(&p, 0o700);
                return Ok(std::fs::canonicalize(&p).unwrap_or(p));
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(OrcaError::io("cannot create", &p, e)),
        }
    }
    Err(OrcaError::Refused("cannot create a login dir".into()))
}

/// What the login's finally block does with the unscoped runtime item
/// (binding decision 3). Orca's E9i puts the pre-login bytes back
/// unconditionally; csm first re-reads the item and restores only what the
/// login itself changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnscopedRestore {
    /// The item is as it was before the login: nothing to do.
    Leave,
    /// The login overwrote it: write the pre-login bytes back.
    Write,
    /// The login created it (there was none before): delete it.
    Delete,
    /// Someone else (a live claude in `D`) changed it during the login:
    /// leave it and file the pre-login bytes in the quarantine.
    KeepAndQuarantine,
}

/// Decide [`UnscopedRestore`]. `now` is the item re-read under the lock
/// (`Err` = unreadable); `login_values` are the grants the login produced
/// (the login dir's scoped item and the captured grant). Pure.
pub fn unscoped_restore(
    before: Option<&str>,
    now: Result<Option<&str>, ()>,
    login_values: &[&str],
) -> UnscopedRestore {
    let Ok(now) = now else {
        // Unknown: never overwrite; keep the old bytes in the quarantine.
        return match before {
            Some(_) => UnscopedRestore::KeepAndQuarantine,
            None => UnscopedRestore::Leave,
        };
    };
    if now == before {
        return UnscopedRestore::Leave;
    }
    if now.is_some_and(|n| login_values.contains(&n)) {
        return match before {
            Some(_) => UnscopedRestore::Write,
            None => UnscopedRestore::Delete,
        };
    }
    match before {
        Some(_) => UnscopedRestore::KeepAndQuarantine,
        None => UnscopedRestore::Leave,
    }
}

/// The quarantine entry holding the unscoped item's pre-login value.
struct Prefiled {
    fingerprint: String,
    /// The entry existed before this login (filed for another reason):
    /// it is neither relabelled nor removed.
    preexisting: bool,
}

/// File the unscoped item's pre-login value `v` in the quarantine.
fn prefile_before(env: &AccountsEnv<'_>, v: &str) -> Result<Prefiled, OrcaError> {
    let q = Quarantine::new(env.os, env.state);
    let fingerprint = super::quarantine::fingerprint(v);
    let preexisting = q.list().iter().any(|m| m.fingerprint == fingerprint);
    q.file(
        v,
        Reason::PreLogin,
        "legacy-keychain",
        None,
        None,
        super::now_ms(),
    )?;
    Ok(Prefiled {
        fingerprint,
        preexisting,
    })
}

/// Put the unscoped `Claude Code-credentials` item back to its pre-login
/// value `before`, touching only what the login changed
/// ([`unscoped_restore`]), and drop the pre-login quarantine copy once the
/// item holds that value again. Returns what was left for the operator.
/// macOS only; the caller holds `switch.lock`.
fn restore_unscoped(
    env: &AccountsEnv<'_>,
    before: Option<&SecretString>,
    prefiled: Option<&Prefiled>,
    login_values: &[&str],
) -> Vec<String> {
    let mut leftovers = Vec::new();
    let now = keychain::read_runtime_scoped(None, env.keychain_user);
    let decision = unscoped_restore(
        before.map(|s| s.expose()),
        now.as_ref()
            .map(|v| v.as_ref().map(|s| s.expose()))
            .map_err(|_| ()),
        login_values,
    );
    let item = || format!("the {} Keychain item", keychain::RUNTIME_SERVICE);
    // The pre-login copy is dropped once the item holds that value
    // again (or never changed); otherwise it stays filed.
    let mut item_restored = false;
    match (decision, before) {
        (UnscopedRestore::Leave, _) => item_restored = now.is_ok(),
        (UnscopedRestore::Write, Some(v)) => {
            if keychain::write_runtime_scoped(v.expose(), None, env.keychain_user).is_err() {
                leftovers.push(item());
            } else {
                item_restored = true;
            }
        }
        (UnscopedRestore::Delete, _) => {
            if keychain::delete_runtime_scoped(None, env.keychain_user).is_err() {
                leftovers.push(item());
            }
        }
        (UnscopedRestore::KeepAndQuarantine, Some(v)) => {
            let q = Quarantine::new(env.os, env.state);
            if let Some(p) = prefiled.filter(|p| !p.preexisting) {
                let _ = q.set_reason(&p.fingerprint, Reason::ChangedDuringLogin);
            }
            match q.file(
                v.expose(),
                Reason::ChangedDuringLogin,
                "legacy-keychain",
                None,
                None,
                super::now_ms(),
            ) {
                Ok(f) => leftovers.push(format!(
                    "{} changed during the login and was left as is; its pre-login grant is quarantined as {}",
                    item(),
                    f.fingerprint()
                )),
                Err(_) => leftovers.push(format!(
                    "{} changed during the login and was left as is; its pre-login grant could not be quarantined",
                    item()
                )),
            }
        }
        (UnscopedRestore::Write | UnscopedRestore::KeepAndQuarantine, None) => {
            unreachable!("unscoped_restore writes or files only a pre-login value")
        }
    }
    if item_restored
        && let Some(p) = prefiled.filter(|p| !p.preexisting)
        && Quarantine::new(env.os, env.state)
            .remove(&p.fingerprint)
            .is_err()
    {
        leftovers.push(format!(
            "the pre-login copy of {} in the quarantine ({})",
            item(),
            p.fingerprint
        ));
    }
    leftovers
}

/// Log in a new account with Orca stopped (E9i + add()).
///
/// `switch.lock` is held from before the login until the finally block has
/// run (binding decision 3), so no csm switch can materialize into the
/// unscoped item while the login waits for the browser. A live claude in
/// `D` can still refresh it; the finally block re-reads the item and never
/// overwrites a change the login did not make ([`unscoped_restore`]).
pub fn login_add(env: &AccountsEnv<'_>, cli: &dyn ClaudeCli) -> Result<AccountChange, OrcaError> {
    offline_gate(env)?;
    let _lock = lock(env)?;
    let dir = make_login_dir()?;
    let d = dir.to_string_lossy().into_owned();
    let before = if env.os == HostOs::MacOs {
        match keychain::read_runtime_aggregate(None, env.keychain_user) {
            Ok(v) => v,
            Err(e) => {
                let _ = fsx::remove_dir_all(&dir);
                return Err(e.into());
            }
        }
    } else {
        None
    };
    // `before` lives only in this process until the finally block puts it
    // back, and the interactive login shares csm's process group: a Ctrl-C
    // or a kill in between would lose it (with `D` = ~/.claude it may be
    // the only copy of the active account's newest grant). So a durable
    // copy goes into the quarantine first; the finally block drops it once
    // the item is back. A copy that cannot be kept refuses the login.
    let prefiled = match &before {
        None => None,
        Some(v) => match prefile_before(env, v.expose()) {
            Ok(p) => Some(p),
            Err(e) => {
                let _ = fsx::remove_dir_all(&dir);
                return Err(OrcaError::Refused(format!(
                    "cannot keep a copy of the {} Keychain item before the login ({e})",
                    keychain::RUNTIME_SERVICE
                )));
            }
        },
    };
    let mut captured: Option<SecretString> = None;
    // The unscoped item holds the login's grant (Claude Code before 2.1
    // writes it) beside `D`'s unchanged `oauthAccount`, which names the
    // active account, from the login until it is put back. Orca's read-back
    // would file that grant into the active account's stash by email, so
    // the item goes back right after the capture, before the store patch
    // or the redo over RPC (Orca's order: claude-login-session.ts restores
    // it in its finally, before the caller persists). A Ctrl-C or a closed
    // terminal during the login reaches claude, which exits, and csm still
    // runs this restore: csm itself ignores SIGINT/SIGHUP until then.
    let mut restored = false;
    let mut early_leftovers: Vec<String> = Vec::new();
    let mut signals = crate::platform::child::DeferInterrupts::install();
    let result = (|| -> Result<AccountChange, OrcaError> {
        let login = cli
            .run(&["auth", "login", "--claudeai"], &dir, LOGIN_TIMEOUT, true)
            .map_err(|e| OrcaError::Refused(format!("claude auth login failed: {e}")))?;
        if !login.success {
            return Err(OrcaError::Refused(
                "claude auth login did not succeed".into(),
            ));
        }
        let status = cli
            .run(&["auth", "status", "--json"], &dir, STATUS_TIMEOUT, false)
            .map(|o| o.stdout)
            .unwrap_or_default();
        let cap = capture_dir(
            env,
            &dir,
            &status,
            before.as_ref().map(|s| s.expose()),
            None,
        )?;
        captured = Some(SecretString::new(cap.creds.expose().to_owned()));
        if env.os == HostOs::MacOs {
            let dir_item = keychain::read_runtime_scoped(Some(&d), env.keychain_user)
                .ok()
                .flatten();
            // The login wrote only the unscoped item: keep its grant in the
            // login dir's scoped item too, so a redo's
            // `addClaudeFromConfigDir` still finds it once the unscoped item
            // is back (it skips a legacy item equal to the pre-login one).
            if dir_item.is_none() {
                let _ =
                    keychain::write_runtime_scoped(cap.creds.expose(), Some(&d), env.keychain_user);
            }
            let values: Vec<&str> = [
                dir_item.as_ref().map(|s| s.expose()),
                Some(cap.creds.expose()),
            ]
            .into_iter()
            .flatten()
            .collect();
            early_leftovers = restore_unscoped(env, before.as_ref(), prefiled.as_ref(), &values);
            restored = true;
        }
        signals.take();
        let key = identity_key(&cap.identity)?;
        let op = RedoOp::Add {
            config_dir: d.clone(),
            previous_legacy_sha256: login_legacy_digest(
                env.os,
                before.as_ref().map(|s| s.expose()),
            ),
            identity: key,
        };
        if env.live.mark().running {
            // Orca came up during the login: let it import the dir, the way
            // the store-write protocol redoes an add (look for the identity
            // first, then `addClaudeFromConfigDir` with Orca's own 60 s
            // wait). A timeout there may still be delivered, so the outcome
            // decides below whether the grant is filed before the cleanup.
            let redo = store::redo_over_rpc(env.user_data, &op, env.redo);
            // csm wrote nothing, so an identity Orca already shows was there
            // before this login: Orca's own add refuses it as a duplicate
            // (persist's findDuplicateClaudeAccount), and so does csm's
            // offline path. The fresh grant is placed nowhere; the error
            // path below files it.
            if matches!(redo, RedoOutcome::AlreadyDone(_)) {
                return Err(OrcaError::Refused(
                    "This Claude account is already added.".into(),
                ));
            }
            return Ok(AccountChange {
                route: Route::OfflineThenRpc,
                id: None,
                email: cap.identity.email.clone(),
                redo: Some(redo),
                leftover: None,
            });
        }
        persist(env, &cap, Some(op))
    })();
    // An add Orca did not confirm (no answer, a timeout that may still be
    // running, a refusal), or one that failed after the capture (the store
    // refused the record, no email): the login dir and its Keychain item
    // are about to go, and they may hold the only copy of the fresh grant.
    // File it first.
    let keep = match &result {
        Ok(c) => matches!(
            c.redo,
            Some(RedoOutcome::Uncertain(_) | RedoOutcome::Failed(_))
        )
        .then_some(Reason::AddUnconfirmed),
        Err(_) => Some(Reason::LoginNotAdded),
    };
    let unconfirmed = matches!(
        &result,
        Ok(AccountChange {
            redo: Some(RedoOutcome::Uncertain(_)),
            ..
        })
    );
    let mut kept_note = None;
    // The grant needed filing and the quarantine refused it (a full or
    // read-only state dir, a Keychain error): the login dir and its
    // Keychain item are then its only copies, and the cleanup keeps them.
    let mut grant_unfiled = false;
    if let Some(reason) = keep
        && let Some(c) = &captured
    {
        let q = Quarantine::new(env.os, env.state);
        kept_note = Some(
            match q.file(c.expose(), reason, "login", None, None, super::now_ms()) {
                // Only an unconfirmed add may still show up in Orca; a
                // refused or failed one never will.
                Ok(f) if unconfirmed => format!(
                    "the login's grant is quarantined as {} until Orca shows the account",
                    f.fingerprint()
                ),
                Ok(f) => format!("the login's grant is quarantined as {}", f.fingerprint()),
                Err(_) => {
                    grant_unfiled = true;
                    format!(
                        "the login's grant could not be quarantined, so the login dir {} and its Keychain item keep it",
                        dir.display()
                    )
                }
            },
        );
    }
    // The finally block, still under the lock: the dir's Keychain item, the
    // unscoped item (only what the login changed, unless the restore above
    // already ran), the dir.
    let mut leftovers = early_leftovers;
    if env.os == HostOs::MacOs {
        let dir_item = keychain::read_runtime_scoped(Some(&d), env.keychain_user)
            .ok()
            .flatten();
        if grant_unfiled {
            // Keep the dir's item. When the login wrote only the unscoped
            // item, which the restore below puts back to its pre-login
            // value, copy the grant into the dir's item first.
            if dir_item.is_none()
                && let Some(c) = &captured
                && keychain::write_runtime_scoped(c.expose(), Some(&d), env.keychain_user).is_err()
            {
                leftovers.push("the login's grant could not be kept in a Keychain item".to_owned());
            }
        } else if keychain::delete_runtime_scoped(Some(&d), env.keychain_user).is_err() {
            leftovers.push("the login dir's Keychain item".to_owned());
        }
        let login_values: Vec<&str> = [dir_item.as_ref(), captured.as_ref()]
            .into_iter()
            .flatten()
            .map(|s| s.expose())
            .collect();
        if !restored {
            leftovers.extend(restore_unscoped(
                env,
                before.as_ref(),
                prefiled.as_ref(),
                &login_values,
            ));
        }
    }
    drop(signals);
    if !grant_unfiled && fsx::remove_dir_all(&dir).is_err() {
        leftovers.push(dir.display().to_string());
    }
    leftovers.extend(kept_note);
    let mut change = match result {
        Ok(c) => c,
        Err(e) if leftovers.is_empty() => return Err(e),
        Err(e) => {
            return Err(OrcaError::Refused(format!(
                "{e}; not cleaned up: {}",
                leftovers.join(", ")
            )));
        }
    };
    if !leftovers.is_empty() {
        let extra = format!("not cleaned up: {}", leftovers.join(", "));
        change.leftover = Some(match change.leftover.take() {
            Some(l) => format!("{l}; {extra}"),
            None => extra,
        });
    }
    Ok(change)
}

// ─── remove ───────────────────────────────────────────────────────────────────

/// Remove account `id`: over RPC with Orca running; offline only for a host
/// account that is not active (store first, stash after L2).
pub fn remove(env: &AccountsEnv<'_>, id: &str) -> Result<AccountChange, OrcaError> {
    let _lock = lock(env)?;
    if env.live.mark().running {
        rpc::remove_claude(env.user_data, id, env.mutation_timeout)?;
        return Ok(AccountChange {
            route: Route::Rpc,
            id: Some(id.to_owned()),
            email: None,
            redo: None,
            leftover: None,
        });
    }
    offline_gate(env)?;
    let view = store::load_choice(env.data_file)?
        .map(|f| StoreView::from_bytes(&f.bytes))
        .transpose()
        .map_err(|e| OrcaError::Refused(e.to_string()))?
        .ok_or_else(|| OrcaError::Refused("no Orca store".into()))?;
    let rec = view
        .account(id)
        .cloned()
        .ok_or_else(|| OrcaError::Refused(format!("no Claude account {id}")))?;
    if !rec.is_host() {
        return Err(OrcaError::Refused(
            "WSL accounts are read-only in csm; remove it in Orca".into(),
        ));
    }
    if view.active_host_id() == Some(id) {
        return Err(OrcaError::Refused(
            "this is the active account; switch to another first, or remove it in Orca".into(),
        ));
    }
    let rid = id.to_owned();
    let mut build = |v: &StoreView| -> Result<Patch, OrcaError> {
        if v.account(&rid).is_none() {
            return Err(OrcaError::Refused(format!("no Claude account {rid}")));
        }
        if v.active_host_id() == Some(rid.as_str()) {
            return Err(OrcaError::Refused("the account became active".into()));
        }
        let accounts: Vec<Value> = v
            .accounts_raw
            .iter()
            .filter(|a| a.get("id").and_then(Value::as_str) != Some(rid.as_str()))
            .cloned()
            .collect();
        let i = t6i(&v.active, &rid);
        let active_id = if v.active_id_raw.as_deref() == Some(rid.as_str()) {
            None
        } else {
            i.host.clone()
        };
        Ok(Patch {
            accounts: Some(accounts),
            active_id: Some(active_id),
            active_by_runtime: Some(i),
        })
    };
    let mut change = AccountChange {
        route: Route::Offline,
        id: Some(id.to_owned()),
        email: rec.email.clone(),
        redo: None,
        leftover: None,
    };
    match store::write_protocol(env.data_file, false, env.live, None, env.state, &mut build)? {
        StoreWrite::Written | StoreWrite::Unchanged => {
            // Orca's storage.remove: rm -rf after Q2i (a refusal is only a
            // warning), then the Keychain item regardless.
            match Stash::open(env.user_data, id, rec.managed_auth_path.as_deref()) {
                Ok(s) => match s.remove(env.user_data, env.os) {
                    Ok(None) => {}
                    Ok(Some(k)) => change.leftover = Some(format!("the stash Keychain item: {k}")),
                    Err(e) => change.leftover = Some(format!("the stash dir: {e}")),
                },
                Err(e) => {
                    change.leftover = Some(format!("the stash dir was not removed: {e}"));
                    if env.os == HostOs::MacOs
                        && let Err(k) = keychain::delete_stash(id)
                    {
                        change.leftover = Some(format!("the stash Keychain item: {k}"));
                    }
                }
            }
            Ok(change)
        }
        StoreWrite::OrcaAtL0 | StoreWrite::OrcaAtL1 | StoreWrite::OrcaAtL2 => {
            // The stash stays until Orca's own removeClaude deletes it.
            change.route = Route::OfflineThenRpc;
            change.redo = Some(store::redo_over_rpc(
                env.user_data,
                &RedoOp::Remove { id: id.to_owned() },
                env.redo,
            ));
            Ok(change)
        }
    }
}

// ─── doctor repairs ───────────────────────────────────────────────────────────

/// Load the store's account view, `None` when there is no store.
fn load_view(env: &AccountsEnv<'_>) -> Result<Option<StoreView>, OrcaError> {
    store::load_choice(env.data_file)?
        .map(|f| StoreView::from_bytes(&f.bytes))
        .transpose()
        .map_err(|e| OrcaError::Refused(e.to_string()))
}

/// Remove an orphan stash for `accounts doctor --fix`: its grant goes to
/// the quarantine first, then the stash dir (and on macOS its Keychain item)
/// is deleted. The doctor's orphan list is a snapshot, so everything is
/// re-checked under `switch.lock`, which every offline add and import holds
/// from stash creation to store patch: Orca must still be stopped, the
/// offline gates must pass, and the store, read again now, must still name
/// no record `id`. Returns a Keychain cleanup that failed (no secrets).
pub fn remove_orphan(env: &AccountsEnv<'_>, id: &str) -> Result<Option<String>, OrcaError> {
    let _lock = lock(env)?;
    if env.live.mark().running {
        return Err(OrcaError::Refused(format!(
            "Orca started; orphan stash {id} left in place"
        )));
    }
    offline_gate(env)?;
    let view = load_view(env)?.ok_or_else(|| {
        OrcaError::Refused(format!(
            "no Orca store to confirm stash {id} is an orphan; left in place"
        ))
    })?;
    if view.account(id).is_some() {
        return Err(OrcaError::Refused(format!(
            "an Orca account names stash {id} now; left in place"
        )));
    }
    let stash = Stash::open(env.user_data, id, None)?;
    // Never lose a grant: it goes to the quarantine first.
    if let Some(creds) = stash.credentials(env.os)? {
        Quarantine::new(env.os, env.state).file(
            creds.expose(),
            Reason::Orphaned,
            &format!("stash {id}"),
            None,
            None,
            chrono::Utc::now().timestamp_millis(),
        )?;
    }
    Ok(stash.remove(env.user_data, env.os)?.map(|k| k.to_string()))
}

/// Drop quarantine entry `fp` for `accounts doctor --fix`, re-checking under
/// `switch.lock` that stash `holder` still holds exactly that grant and, when
/// the store is readable, that a record still names `holder`. The entry is
/// csm's own and no Orca file is written, so the Orca liveness and version
/// gates do not apply.
pub fn purge_quarantine(env: &AccountsEnv<'_>, fp: &str, holder: &str) -> Result<(), OrcaError> {
    let _lock = lock(env)?;
    let path = match load_view(env).ok().flatten() {
        Some(v) => match v.account(holder) {
            Some(rec) => rec.managed_auth_path.clone(),
            None => {
                return Err(OrcaError::Refused(format!(
                    "no Orca account {holder} holds quarantine entry {fp} now; kept"
                )));
            }
        },
        None => None,
    };
    let stash = Stash::open(env.user_data, holder, path.as_deref())?;
    let Some(creds) = stash
        .credentials(env.os)?
        .filter(|c| super::quarantine::fingerprint(c.expose()) == fp)
    else {
        return Err(OrcaError::Refused(format!(
            "stash {holder} no longer holds quarantine entry {fp}; kept"
        )));
    };
    // The fingerprint names only the Claude grant: an entry that also holds
    // MCP logins the stash lacks is not superseded.
    let q = Quarantine::new(env.os, env.state);
    if let Some(entry) = q.get(fp)? {
        let extra = super::quarantine::uncovered(
            &super::quarantine::side_state(entry.expose()),
            &super::quarantine::side_state(creds.expose()),
        );
        if !extra.is_empty() {
            return Err(OrcaError::Refused(format!(
                "quarantine entry {fp} also holds {} that stash {holder} lacks; kept (log in to \
                 those MCP servers again, then rerun)",
                extra.join(", ")
            )));
        }
    }
    q.remove(fp)
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orca::testsupport::{
        ScriptedLiveness, creds_json, oauth_json, record_json, write_store,
    };
    use serde_json::json;
    use std::sync::Mutex;

    #[test]
    fn u9i_identity_order() {
        let creds = r#"{"claudeAiOauth":{"accessToken":"a","email":"c@example.com"}}"#;
        let oauth = json!({"emailAddress":"  ","email":"x@example.com","organizationUuid":" org-1 ","organizationName":"Acme"});
        // The status wins; a blank emailAddress still blocks the fallbacks.
        let id = identity_from(
            r#"{"email":" alice@example.com ","organizationName":"Acme"}"#,
            Some(&oauth),
            creds,
        );
        assert_eq!(id.email.as_deref(), Some("alice@example.com"));
        assert_eq!(id.organization_uuid.as_deref(), Some("org-1"));
        assert_eq!(id.organization_name.as_deref(), Some("Acme"));
        let id = identity_from("not json", Some(&oauth), creds);
        assert_eq!(id.email, None);
        let id = identity_from("", None, creds);
        assert_eq!(id.email.as_deref(), Some("c@example.com"));
    }

    #[test]
    fn l9i_takes_the_first_truthy_oauth_account() {
        let a = br#"{"oauthAccount":null}"#;
        let b = br#"{"oauthAccount":{"accountUuid":"u"}}"#;
        assert_eq!(
            config_oauth_account(&[Some(a), Some(b)]),
            Some(json!({"accountUuid":"u"}))
        );
        assert_eq!(config_oauth_account(&[Some(b"{"), None]), None);
        assert_eq!(
            config_oauth_account(&[Some(br#"{"oauthAccount":""}"#)]),
            None
        );
    }

    #[test]
    fn c9i_prefers_scoped_then_a_changed_unscoped_then_the_file() {
        let s = |x: &str| Some(x.to_owned());
        use HostOs::*;
        assert_eq!(
            choose_captured(MacOs, s("S"), s("U"), None, None, s("F")),
            s("S")
        );
        // Without a digest: the unscoped item only when it changed.
        assert_eq!(
            choose_captured(MacOs, None, s("U"), Some("U"), None, s("F")),
            s("F")
        );
        assert_eq!(
            choose_captured(MacOs, None, s("U2"), Some("U"), None, s("F")),
            s("U2")
        );
        // With a digest: only when its sha differs.
        let sha_u = sha256_hex(b"U");
        assert_eq!(
            choose_captured(MacOs, None, s("U"), None, Some(&sha_u), s("F")),
            s("F")
        );
        assert_eq!(
            choose_captured(MacOs, None, s("V"), None, Some(&sha_u), None),
            s("V")
        );
        assert_eq!(
            choose_captured(Linux, s("S"), s("U"), None, None, s("F")),
            s("F")
        );
        assert_eq!(choose_captured(Linux, None, None, None, None, None), None);
    }

    // ─── shells ───────────────────────────────────────────────────────────────

    /// A fake `claude`: records calls; `auth login` writes `creds` into the
    /// dir (and a `.claude.json`), `auth status` prints `status`.
    struct FakeClaude {
        creds: String,
        status: String,
        calls: Mutex<Vec<(String, PathBuf)>>,
    }

    impl ClaudeCli for FakeClaude {
        fn run(
            &self,
            args: &[&str],
            dir: &Path,
            _: Duration,
            interactive: bool,
        ) -> io::Result<CliOutput> {
            self.calls
                .lock()
                .unwrap()
                .push((args.join(" "), dir.to_path_buf()));
            if args[..2] == ["auth", "login"] {
                assert!(interactive);
                std::fs::write(dir.join(".credentials.json"), &self.creds)?;
                std::fs::write(
                    dir.join(".claude.json"),
                    json!({"oauthAccount": oauth_json("u-c", "carol@example.com", None)})
                        .to_string(),
                )?;
            }
            Ok(CliOutput {
                success: true,
                stdout: self.status.clone(),
            })
        }
    }

    struct World {
        tmp: tempfile::TempDir,
        ud: PathBuf,
        choice: DataFileChoice,
        user: KeychainUser,
    }

    fn world() -> World {
        let tmp = tempfile::tempdir().unwrap();
        let ud = tmp.path().join("ud");
        let mut recs = Vec::new();
        for (id, email) in [("id-a", "alice@example.com"), ("id-b", "bob@example.com")] {
            let s = stash::create(&ud, id).unwrap();
            s.write_auth(
                &ud,
                HostOs::Linux,
                &creds_json(id, id, 1),
                &oauth_json(id, email, None),
            )
            .unwrap();
            recs.push(record_json(&ud, id, email, None));
        }
        let choice = write_store(&ud, &recs, Some("id-a"));
        World {
            ud,
            choice,
            tmp,
            user: KeychainUser {
                acct: "t".into(),
                delete_accts: vec!["t".into()],
            },
        }
    }

    impl World {
        fn env<'a>(&'a self, live: &'a dyn Liveness) -> AccountsEnv<'a> {
            AccountsEnv {
                os: HostOs::Linux,
                user_data: &self.ud,
                data_file: &self.choice,
                state: self.tmp.path(),
                keychain_user: &self.user,
                live,
                version_ok: true,
                store_access_allowed: true,
                lock_wait: Duration::from_secs(2),
                redo: RedoOpts {
                    wait: Duration::from_millis(300),
                    poll: Duration::from_millis(20),
                },
                mutation_timeout: Duration::from_secs(2),
            }
        }

        fn view(&self) -> StoreView {
            StoreView::from_bytes(&std::fs::read(&self.choice.path).unwrap()).unwrap()
        }
    }

    #[test]
    fn import_creates_the_stash_then_the_record() {
        let w = world();
        let src = w.tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let grant = creds_json("at-c", "rt-c", 5);
        std::fs::write(src.join(".credentials.json"), &grant).unwrap();
        std::fs::write(
            src.join(".claude.json"),
            json!({"oauthAccount": oauth_json("u-c", "Carol@Example.com", None)}).to_string(),
        )
        .unwrap();
        let cli = FakeClaude {
            creds: String::new(),
            status: "{}".into(),
            calls: Mutex::new(Vec::new()),
        };
        let live = ScriptedLiveness::stopped();
        let c = import(&w.env(&live), &cli, &src, None).unwrap();
        let id = c.id.unwrap();
        assert_eq!(c.email.as_deref(), Some("Carol@Example.com"));
        let v = w.view();
        assert_eq!(v.accounts.len(), 3);
        assert_eq!(v.accounts[2].id, id);
        assert_eq!(v.active_host_id(), Some("id-a"));
        let s = Stash::open(&w.ud, &id, v.accounts[2].managed_auth_path.as_deref()).unwrap();
        // The record stores the path Orca's own create() would (Node's
        // realpath form), never the Windows verbatim `\\?\` form, which
        // Orca's ownership check rejects.
        let p = v.accounts[2].managed_auth_path.clone().unwrap();
        assert!(!p.starts_with(r"\\?\"), "{p}");
        assert_eq!(
            std::fs::canonicalize(&p).unwrap(),
            std::fs::canonicalize(stash::default_auth_dir(&w.ud, &id)).unwrap()
        );
        assert_eq!(
            s.credentials(HostOs::Linux).unwrap().unwrap().expose(),
            grant
        );
        assert_eq!(
            s.oauth_account().unwrap().unwrap()["accountUuid"],
            json!("u-c")
        );
        assert_eq!(cli.calls.lock().unwrap()[0].0, "auth status --json");
        // The same identity again (email case-folded) is refused, with no
        // stash left behind.
        let err = import(&w.env(&live), &cli, &src, None).unwrap_err();
        assert!(err.to_string().contains("already added"));
        assert_eq!(
            std::fs::read_dir(crate::orca::userdata::claude_accounts_root(&w.ud))
                .unwrap()
                .count(),
            3
        );
        // A bad digest is refused up front.
        assert!(import(&w.env(&live), &cli, &src, Some("ABC")).is_err());
    }

    #[test]
    fn import_needs_credentials_off_macos_and_an_email() {
        let w = world();
        let src = w.tmp.path().join("empty");
        std::fs::create_dir_all(&src).unwrap();
        let cli = FakeClaude {
            creds: String::new(),
            status: String::new(),
            calls: Mutex::new(Vec::new()),
        };
        let live = ScriptedLiveness::stopped();
        assert!(
            import(&w.env(&live), &cli, &src, None)
                .unwrap_err()
                .to_string()
                .contains("No Claude credentials")
        );
        std::fs::write(src.join(".credentials.json"), creds_json("a", "r", 1)).unwrap();
        assert!(
            import(&w.env(&live), &cli, &src, None)
                .unwrap_err()
                .to_string()
                .contains("email")
        );
        assert_eq!(w.view().accounts.len(), 2);
    }

    #[test]
    fn orca_appearing_at_l1_removes_the_new_stash() {
        let w = world();
        let src = w.tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join(".credentials.json"), creds_json("a", "r", 1)).unwrap();
        let cli = FakeClaude {
            creds: String::new(),
            status: r#"{"email":"carol@example.com"}"#.into(),
            calls: Mutex::new(Vec::new()),
        };
        // 0 = import's L0, 1 = protocol L0, 2 = L1.
        let live = ScriptedLiveness::appears_at(2);
        let c = import(&w.env(&live), &cli, &src, None).unwrap();
        assert_eq!(c.route, Route::OfflineThenRpc);
        assert!(matches!(c.redo, Some(RedoOutcome::Uncertain(_))));
        assert_eq!(w.view().accounts.len(), 2);
        assert_eq!(
            std::fs::read_dir(crate::orca::userdata::claude_accounts_root(&w.ud))
                .unwrap()
                .count(),
            2
        );
    }

    #[test]
    fn offline_adds_refuse_a_sqlite_backed_profile() {
        let w = world();
        let store_before = std::fs::read(&w.choice.path).unwrap();
        std::fs::write(
            w.choice
                .path
                .with_file_name(crate::orca::userdata::STATE_DB),
            b"",
        )
        .unwrap();
        let src = w.tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join(".credentials.json"), creds_json("a", "r", 1)).unwrap();
        let cli = FakeClaude {
            creds: creds_json("at-c", "rt-c", 5),
            status: r#"{"email":"carol@example.com"}"#.into(),
            calls: Mutex::new(Vec::new()),
        };
        let live = ScriptedLiveness::stopped();
        for e in [
            import(&w.env(&live), &cli, &src, None).unwrap_err(),
            login_add(&w.env(&live), &cli).unwrap_err(),
            remove(&w.env(&live), "id-b").unwrap_err(),
        ] {
            assert!(e.to_string().contains("SQLite"), "{e}");
        }
        // Nothing ran, nothing was created.
        assert!(cli.calls.lock().unwrap().is_empty());
        assert_eq!(std::fs::read(&w.choice.path).unwrap(), store_before);
        assert_eq!(
            std::fs::read_dir(crate::orca::userdata::claude_accounts_root(&w.ud))
                .unwrap()
                .count(),
            2
        );
    }

    /// Orca comes up during the login and does not confirm the import (no
    /// answer here; a real Orca may still be inside its 20 s `auth status`
    /// when a short timeout fires). The login dir is cleaned up, so the
    /// fresh grant is filed in the quarantine first, never lost.
    #[test]
    fn an_unconfirmed_add_by_an_orca_that_came_up_quarantines_the_grant() {
        let w = world();
        let grant = creds_json("at-c", "rt-c", 5);
        let cli = FakeClaude {
            creds: grant.clone(),
            status: r#"{"email":"carol@example.com","organizationUuid":null}"#.into(),
            calls: Mutex::new(Vec::new()),
        };
        let live = ScriptedLiveness::appears_at(0);
        let c = login_add(&w.env(&live), &cli).unwrap();
        assert_eq!(c.route, Route::OfflineThenRpc);
        assert!(matches!(c.redo, Some(RedoOutcome::Uncertain(_))), "{c:?}");
        let fp = crate::orca::quarantine::fingerprint(&grant);
        assert!(
            c.leftover.as_deref().is_some_and(|l| l.contains(&fp)),
            "{c:?}"
        );
        let q = Quarantine::new(HostOs::Linux, w.tmp.path());
        let list = q.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].reason, Reason::AddUnconfirmed);
        assert_eq!(q.get(&fp).unwrap().unwrap().expose(), grant);
        // Orca owns the add: csm wrote no record and no stash.
        assert_eq!(w.view().accounts.len(), 2);
        let dir = &cli.calls.lock().unwrap()[0].1;
        assert!(!dir.exists());
    }

    /// An add that fails after the capture (here the store already holds
    /// the identity) files the fresh grant before the login dir goes, and
    /// the error names it.
    #[test]
    fn a_login_whose_add_fails_quarantines_the_grant() {
        let w = world();
        let grant = creds_json("at-a2", "rt-a2", 5);
        let cli = FakeClaude {
            creds: grant.clone(),
            status: r#"{"email":"alice@example.com","organizationUuid":null}"#.into(),
            calls: Mutex::new(Vec::new()),
        };
        let live = ScriptedLiveness::stopped();
        let e = login_add(&w.env(&live), &cli).unwrap_err().to_string();
        let fp = crate::orca::quarantine::fingerprint(&grant);
        assert!(e.contains("already added") && e.contains(&fp), "{e}");
        let q = Quarantine::new(HostOs::Linux, w.tmp.path());
        let list = q.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].reason, Reason::LoginNotAdded);
        assert_eq!(q.get(&fp).unwrap().unwrap().expose(), grant);
        assert_eq!(w.view().accounts.len(), 2);
        assert!(!cli.calls.lock().unwrap()[0].1.exists());
    }

    /// The grant needed filing and the quarantine refused it: the login dir
    /// is then its only copy, so the cleanup keeps it and the error says
    /// where it is.
    #[test]
    fn a_login_grant_the_quarantine_refuses_keeps_the_login_dir() {
        let w = world();
        // A file where the quarantine dir should be: every filing fails.
        std::fs::write(w.tmp.path().join("quarantine"), "x").unwrap();
        let grant = creds_json("at-a2", "rt-a2", 5);
        let cli = FakeClaude {
            creds: grant.clone(),
            status: r#"{"email":"alice@example.com","organizationUuid":null}"#.into(),
            calls: Mutex::new(Vec::new()),
        };
        let live = ScriptedLiveness::stopped();
        let e = login_add(&w.env(&live), &cli).unwrap_err().to_string();
        let dir = cli.calls.lock().unwrap()[0].1.clone();
        let kept = std::fs::read_to_string(dir.join(".credentials.json"));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            e.contains("could not be quarantined") && e.contains(&dir.display().to_string()),
            "{e}"
        );
        assert_eq!(kept.unwrap(), grant, "the login dir keeps the grant");
    }

    /// The digest handed to Orca for a login dir makes Orca's capture take
    /// the same grant csm's own capture took from the pre-login value.
    #[test]
    fn the_login_digest_matches_the_pre_login_baseline() {
        use HostOs::*;
        assert_eq!(login_legacy_digest(Linux, Some("B")), None);
        assert_eq!(login_legacy_digest(Windows, None), None);
        let s = |v: &str| Some(v.to_owned());
        // A login that wrote only the unscoped item.
        for before in [None, Some("B")] {
            let digest = login_legacy_digest(MacOs, before).unwrap();
            assert_eq!(
                choose_captured(MacOs, None, s("NEW"), before, None, None),
                s("NEW")
            );
            assert_eq!(
                choose_captured(MacOs, None, s("NEW"), None, Some(&digest), None),
                s("NEW"),
                "{before:?}"
            );
        }
        // A login that left the unscoped item alone: neither takes it.
        let digest = login_legacy_digest(MacOs, Some("B")).unwrap();
        assert_eq!(digest, sha256_hex(b"B"));
        assert_eq!(
            choose_captured(MacOs, None, s("B"), Some("B"), None, s("F")),
            s("F")
        );
        assert_eq!(
            choose_captured(MacOs, None, s("B"), None, Some(&digest), s("F")),
            s("F")
        );
    }

    #[test]
    fn login_add_runs_claude_in_a_temp_dir_and_cleans_it() {
        let w = world();
        let cli = FakeClaude {
            creds: creds_json("at-c", "rt-c", 5),
            status: r#"{"email":"carol@example.com","organizationUuid":null}"#.into(),
            calls: Mutex::new(Vec::new()),
        };
        let live = ScriptedLiveness::stopped();
        let c = login_add(&w.env(&live), &cli).unwrap();
        assert_eq!(c.email.as_deref(), Some("carol@example.com"));
        assert!(c.leftover.is_none());
        let calls = cli.calls.lock().unwrap();
        assert_eq!(calls[0].0, "auth login --claudeai");
        assert_eq!(calls[1].0, "auth status --json");
        let dir = &calls[0].1;
        assert!(
            dir.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with(LOGIN_DIR_PREFIX)
        );
        assert!(!dir.exists());
        assert_eq!(w.view().accounts.len(), 3);
    }

    #[test]
    fn remove_refuses_the_active_account_and_removes_an_inactive_one() {
        let w = world();
        let live = ScriptedLiveness::stopped();
        assert!(
            remove(&w.env(&live), "id-a")
                .unwrap_err()
                .to_string()
                .contains("active")
        );
        assert!(remove(&w.env(&live), "id-zz").is_err());
        let c = remove(&w.env(&live), "id-b").unwrap();
        assert_eq!(c.route, Route::Offline);
        assert!(c.leftover.is_none());
        let v = w.view();
        assert_eq!(
            v.accounts.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
            vec!["id-a"]
        );
        assert_eq!(v.active_host_id(), Some("id-a"));
        assert!(
            !crate::orca::userdata::claude_accounts_root(&w.ud)
                .join("id-b")
                .exists()
        );
        // The other record's bytes are carried over as stored.
        assert!(
            std::fs::read_to_string(&w.choice.path)
                .unwrap()
                .contains("\"lastAuthenticatedAt\":1700000000000")
        );
    }

    #[test]
    fn remove_keeps_the_stash_when_orca_appears() {
        let w = world();
        let live = ScriptedLiveness::appears_at(3);
        let c = remove(&w.env(&live), "id-b").unwrap();
        assert_eq!(c.route, Route::OfflineThenRpc);
        assert!(
            crate::orca::userdata::claude_accounts_root(&w.ud)
                .join("id-b")
                .exists()
        );
    }

    #[test]
    fn orca_cli_candidates_put_the_running_bundle_first() {
        let main = Path::new("/Applications/Orca.app/Contents/MacOS/Orca");
        let got = orca_cli_candidates(
            HostOs::MacOs,
            Some(main),
            &[
                PathBuf::from("/Applications/Orca.app"),
                PathBuf::from("/Users/example/Applications/Orca.app"),
            ],
            // A PATH dir counts only when it is absolute on this host.
            &[crate::testenv::abs("/usr/local/bin"), PathBuf::from("rel")],
        );
        assert_eq!(
            got,
            vec![
                PathBuf::from("/Applications/Orca.app/Contents/Resources/bin/orca"),
                PathBuf::from("/Users/example/Applications/Orca.app/Contents/Resources/bin/orca"),
                crate::testenv::abs("/usr/local/bin/orca"),
            ]
        );
    }

    #[test]
    fn orca_cli_on_linux_is_orca_ide_never_the_screen_reader() {
        let got = orca_cli_candidates(
            HostOs::Linux,
            Some(Path::new("/opt/Orca/orca-ide")),
            &LINUX_INSTALL_DIRS.map(PathBuf::from),
            &[crate::testenv::abs("/usr/bin")],
        );
        assert_eq!(got[0], PathBuf::from("/opt/Orca/resources/bin/orca-ide"));
        assert_eq!(
            got.last().unwrap(),
            &crate::testenv::abs("/usr/bin/orca-ide")
        );
        assert!(got.iter().all(|p| p.ends_with("orca-ide")), "{got:?}");
        assert_eq!(got.len(), 4, "deduplicated: {got:?}");
    }

    #[test]
    fn orca_cli_on_windows_sits_in_resources_beside_orca_exe() {
        let exe = PathBuf::from("/c/Users/example/AppData/Local/Programs/Orca/Orca.exe");
        let got = orca_cli_candidates(HostOs::Windows, Some(&exe), std::slice::from_ref(&exe), &[]);
        assert_eq!(
            got,
            vec![PathBuf::from(
                "/c/Users/example/AppData/Local/Programs/Orca/resources/bin/orca.exe"
            )]
        );
    }

    #[test]
    fn system_claude_from_a_launch_command() {
        let c = SystemClaude::from_launch_command(vec!["/opt/example/npx".into(), "happy".into()])
            .unwrap();
        assert_eq!(c.program, PathBuf::from("/opt/example/npx"));
        assert_eq!(c.prefix, vec![std::ffi::OsString::from("happy")]);
        let c = SystemClaude::from_launch_command(vec!["claude".into()]).unwrap();
        assert_eq!(c.program, PathBuf::from("claude"));
        assert!(c.prefix.is_empty());
        assert!(SystemClaude::from_launch_command(Vec::new()).is_none());
    }

    #[test]
    fn the_real_claude_is_refused_under_test() {
        let c = SystemClaude {
            program: PathBuf::from("claude"),
            prefix: Vec::new(),
        };
        let e = c
            .run(
                &["--version"],
                Path::new("/nonexistent"),
                Duration::from_secs(1),
                false,
            )
            .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
    }

    /// A fake macOS `claude`: the login writes the dir's scoped item and
    /// clobbers the unscoped one with the same grant, as a Keychain-backed
    /// login can (the finally block restores only a change the login made;
    /// a different value there is someone else's and is kept, see
    /// `a_change_to_the_unscoped_item_during_login_is_kept_and_the_old_grant_quarantined`).
    #[cfg(unix)]
    struct MacClaude {
        user: KeychainUser,
        grant: String,
    }

    #[cfg(unix)]
    impl ClaudeCli for MacClaude {
        fn run(&self, args: &[&str], dir: &Path, _: Duration, _: bool) -> io::Result<CliOutput> {
            if args[..2] == ["auth", "login"] {
                let d = dir.to_string_lossy();
                keychain::write_runtime_scoped(&self.grant, Some(&d), &self.user).unwrap();
                keychain::write_runtime_scoped(&self.grant, None, &self.user).unwrap();
            }
            Ok(CliOutput {
                success: true,
                stdout: r#"{"email":"carol@example.com"}"#.into(),
            })
        }
    }

    /// Migration's import digests the unscoped item under `switch.lock`: a
    /// legacy dir with no scoped item then keeps its own file grant, and
    /// the unchanged unscoped item (another account's) is not captured.
    #[cfg(unix)]
    #[test]
    fn import_current_legacy_digests_the_unscoped_item_under_the_lock() {
        let _fake = crate::orca::testsupport::FakeSecurity::install();
        let w = world();
        let user = w.user.clone();
        let other = creds_json("at-b", "rt-b", 5);
        keychain::write_runtime_scoped(&other, None, &user).unwrap();
        let src = w.tmp.path().join("legacy");
        std::fs::create_dir_all(&src).unwrap();
        let grant = creds_json("at-c", "rt-c", 5);
        std::fs::write(src.join(".credentials.json"), &grant).unwrap();
        std::fs::write(
            src.join(".claude.json"),
            json!({"oauthAccount": oauth_json("u-c", "carol@example.com", None)}).to_string(),
        )
        .unwrap();
        let cli = FakeClaude {
            creds: String::new(),
            status: "{}".into(),
            calls: Mutex::new(Vec::new()),
        };
        let live = ScriptedLiveness::stopped();
        let mut env = w.env(&live);
        env.os = HostOs::MacOs;
        let c = import_current_legacy(&env, &cli, &src).unwrap();
        let id = c.id.unwrap();
        let v = w.view();
        let rec = v.accounts.iter().find(|a| a.id == id).unwrap();
        let s = Stash::open(&w.ud, &id, rec.managed_auth_path.as_deref()).unwrap();
        assert_eq!(
            s.credentials(HostOs::MacOs).unwrap().unwrap().expose(),
            grant
        );
    }

    #[cfg(unix)]
    #[test]
    fn macos_login_restores_the_unscoped_item_and_drops_the_scoped_one() {
        let fake = crate::orca::testsupport::FakeSecurity::install();
        for pre in [Some("PRE"), None] {
            let w = world();
            let user = w.user.clone();
            match pre {
                Some(p) => keychain::write_runtime_scoped(p, None, &user).unwrap(),
                None => {
                    keychain::delete_runtime_scoped(None, &user).unwrap();
                }
            }
            let cli = MacClaude {
                user: user.clone(),
                grant: creds_json("at-c", "rt-c", 5),
            };
            let live = ScriptedLiveness::stopped();
            let mut env = w.env(&live);
            env.os = HostOs::MacOs;
            let c = login_add(&env, &cli).unwrap();
            assert!(c.leftover.is_none());
            let id = c.id.unwrap();
            // The login's grant was kept in the new stash.
            assert_eq!(
                fake.get(keychain::STASH_SERVICE, &id).as_deref(),
                Some(creds_json("at-c", "rt-c", 5).as_bytes())
            );
            assert_eq!(
                fake.get(keychain::RUNTIME_SERVICE, &user.acct).as_deref(),
                pre.map(str::as_bytes)
            );
            // Only the unscoped runtime item (if any) and the stashes remain.
            assert!(
                fake.items()
                    .iter()
                    .all(|(svc, _)| svc == keychain::RUNTIME_SERVICE
                        || svc == keychain::STASH_SERVICE),
                "{:?}",
                fake.items()
            );
        }
    }

    /// A fake login of Claude Code before 2.1: it writes only the unscoped
    /// item.
    #[cfg(unix)]
    struct LegacyMacClaude {
        user: KeychainUser,
        grant: String,
    }

    #[cfg(unix)]
    impl ClaudeCli for LegacyMacClaude {
        fn run(&self, args: &[&str], _: &Path, _: Duration, _: bool) -> io::Result<CliOutput> {
            if args[..2] == ["auth", "login"] {
                keychain::write_runtime_scoped(&self.grant, None, &self.user).unwrap();
            }
            Ok(CliOutput {
                success: true,
                stdout: r#"{"email":"carol@example.com"}"#.into(),
            })
        }
    }

    /// Each liveness check after the login records the fake Keychain:
    /// (unscoped item, every scoped `Claude Code-credentials-*` value).
    #[cfg(unix)]
    type KeychainAtMarks = std::sync::Arc<Mutex<Vec<(Option<Vec<u8>>, Vec<Vec<u8>>)>>>;

    #[cfg(unix)]
    fn record_keychain_at_marks(
        live: ScriptedLiveness,
        fake: &crate::orca::testsupport::FakeSecurity,
        acct: &str,
        marks: usize,
    ) -> (ScriptedLiveness, KeychainAtMarks) {
        let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
        let items = fake.root().join("items");
        let unscoped = items.join(format!(
            "{}.{}",
            hex(keychain::RUNTIME_SERVICE.as_bytes()),
            hex(acct.as_bytes())
        ));
        let scoped_prefix = hex(format!("{}-", keychain::RUNTIME_SERVICE).as_bytes());
        let seen: KeychainAtMarks = Default::default();
        let mut live = live;
        for n in 0..marks {
            let (items, unscoped, scoped_prefix, seen) = (
                items.clone(),
                unscoped.clone(),
                scoped_prefix.clone(),
                seen.clone(),
            );
            live = live.on_check(n, move || {
                let scoped = std::fs::read_dir(&items)
                    .unwrap()
                    .filter_map(|e| {
                        let e = e.ok()?;
                        let name = e.file_name().into_string().ok()?;
                        name.starts_with(&scoped_prefix)
                            .then(|| std::fs::read(e.path()).unwrap())
                    })
                    .collect();
                seen.lock()
                    .unwrap()
                    .push((std::fs::read(&unscoped).ok(), scoped));
            });
        }
        (live, seen)
    }

    /// Round 8: the unscoped item holds the login's grant beside `D`'s
    /// `oauthAccount` (the active account) only until the capture. It is
    /// back to its pre-login value before the store patch's liveness
    /// checks, so an Orca coming up there never reads the new grant back
    /// into the active account's stash (Orca's own order).
    #[cfg(unix)]
    #[test]
    fn a_legacy_login_restores_the_unscoped_item_before_the_store_patch() {
        let fake = crate::orca::testsupport::FakeSecurity::install();
        let w = world();
        let user = w.user.clone();
        let pre = creds_json("at-a", "rt-a", 1);
        keychain::write_runtime_scoped(&pre, None, &user).unwrap();
        let grant = creds_json("at-c", "rt-c", 5);
        let cli = LegacyMacClaude {
            user: user.clone(),
            grant: grant.clone(),
        };
        // 0 = the post-login check, 1-3 = the protocol's L0/L1/L2.
        let (live, seen) =
            record_keychain_at_marks(ScriptedLiveness::stopped(), &fake, &user.acct, 4);
        let mut env = w.env(&live);
        env.os = HostOs::MacOs;
        let c = login_add(&env, &cli).unwrap();
        assert!(c.leftover.is_none(), "{:?}", c.leftover);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 4, "{} checks", live.checks());
        for (unscoped, _) in seen.iter() {
            assert_eq!(unscoped.as_deref(), Some(pre.as_bytes()));
        }
        let id = c.id.unwrap();
        assert_eq!(
            fake.get(keychain::STASH_SERVICE, &id).as_deref(),
            Some(grant.as_bytes())
        );
        assert_eq!(
            fake.get(keychain::RUNTIME_SERVICE, &user.acct).as_deref(),
            Some(pre.as_bytes())
        );
        assert!(
            Quarantine::new(HostOs::MacOs, w.tmp.path())
                .list()
                .is_empty()
        );
    }

    /// Round 8: when Orca comes up during a legacy login, the unscoped item
    /// is already back when csm asks Orca to import the dir, and the grant
    /// waits in the login dir's scoped item, where Orca's
    /// `addClaudeFromConfigDir` looks first.
    #[cfg(unix)]
    #[test]
    fn a_legacy_login_redone_over_rpc_restores_first_and_seeds_the_dir_item() {
        let fake = crate::orca::testsupport::FakeSecurity::install();
        let w = world();
        let user = w.user.clone();
        let pre = creds_json("at-a", "rt-a", 1);
        keychain::write_runtime_scoped(&pre, None, &user).unwrap();
        let grant = creds_json("at-c", "rt-c", 5);
        let cli = LegacyMacClaude {
            user: user.clone(),
            grant: grant.clone(),
        };
        let (live, seen) =
            record_keychain_at_marks(ScriptedLiveness::appears_at(0), &fake, &user.acct, 1);
        let mut env = w.env(&live);
        env.os = HostOs::MacOs;
        let c = login_add(&env, &cli).unwrap();
        assert_eq!(c.route, Route::OfflineThenRpc);
        let seen = seen.lock().unwrap();
        let (unscoped, scoped) = &seen[0];
        assert_eq!(unscoped.as_deref(), Some(pre.as_bytes()));
        assert!(scoped.iter().any(|v| v == grant.as_bytes()), "{scoped:?}");
        // Unconfirmed: the grant is filed, and the item keeps its value.
        assert_eq!(
            fake.get(keychain::RUNTIME_SERVICE, &user.acct).as_deref(),
            Some(pre.as_bytes())
        );
        let q = Quarantine::new(HostOs::MacOs, w.tmp.path());
        assert!(
            q.list().iter().any(|m| m.reason == Reason::AddUnconfirmed),
            "{:?}",
            q.list()
        );
    }

    /// A fake login that records what the quarantine holds while it runs.
    #[cfg(unix)]
    struct QuarantineProbe {
        inner: MacClaude,
        state: PathBuf,
        seen: Mutex<Vec<(Reason, String)>>,
    }

    #[cfg(unix)]
    impl ClaudeCli for QuarantineProbe {
        fn run(&self, args: &[&str], dir: &Path, t: Duration, i: bool) -> io::Result<CliOutput> {
            if args[..2] == ["auth", "login"] {
                let q = Quarantine::new(HostOs::MacOs, &self.state);
                *self.seen.lock().unwrap() = q
                    .list()
                    .into_iter()
                    .map(|m| {
                        let v = q.get(&m.fingerprint).unwrap().unwrap();
                        (m.reason, v.expose().to_owned())
                    })
                    .collect();
            }
            self.inner.run(args, dir, t, i)
        }
    }

    /// The unscoped item's pre-login value is on disk before the login
    /// starts (a csm killed mid-login loses nothing), and the copy goes
    /// once the finally block has put the item back.
    #[cfg(unix)]
    #[test]
    fn the_pre_login_unscoped_value_is_filed_before_the_login_and_dropped_after() {
        let fake = crate::orca::testsupport::FakeSecurity::install();
        let w = world();
        let user = w.user.clone();
        let pre = creds_json("at-a", "rt-a", 1);
        keychain::write_runtime_scoped(&pre, None, &user).unwrap();
        let cli = QuarantineProbe {
            inner: MacClaude {
                user: user.clone(),
                grant: creds_json("at-c", "rt-c", 5),
            },
            state: w.tmp.path().to_path_buf(),
            seen: Mutex::new(Vec::new()),
        };
        let live = ScriptedLiveness::stopped();
        let mut env = w.env(&live);
        env.os = HostOs::MacOs;
        let c = login_add(&env, &cli).unwrap();
        assert!(c.leftover.is_none(), "{:?}", c.leftover);
        assert_eq!(
            *cli.seen.lock().unwrap(),
            vec![(Reason::PreLogin, pre.clone())],
            "filed before the login ran"
        );
        assert_eq!(
            fake.get(keychain::RUNTIME_SERVICE, &user.acct).as_deref(),
            Some(pre.as_bytes())
        );
        assert!(
            Quarantine::new(HostOs::MacOs, w.tmp.path())
                .list()
                .is_empty()
        );
    }

    #[test]
    fn unscoped_restore_touches_only_what_the_login_changed() {
        use UnscopedRestore::*;
        let login = ["LOGIN", "SCOPED"];
        // Unchanged.
        assert_eq!(unscoped_restore(Some("A"), Ok(Some("A")), &login), Leave);
        assert_eq!(unscoped_restore(None, Ok(None), &login), Leave);
        // The login clobbered it (either of its values).
        assert_eq!(
            unscoped_restore(Some("A"), Ok(Some("LOGIN")), &login),
            Write
        );
        assert_eq!(
            unscoped_restore(Some("A"), Ok(Some("SCOPED")), &login),
            Write
        );
        assert_eq!(unscoped_restore(None, Ok(Some("LOGIN")), &login), Delete);
        // Someone else changed or deleted it: never overwritten.
        assert_eq!(
            unscoped_restore(Some("A"), Ok(Some("A2")), &login),
            KeepAndQuarantine
        );
        assert_eq!(
            unscoped_restore(Some("A"), Ok(None), &login),
            KeepAndQuarantine
        );
        assert_eq!(unscoped_restore(None, Ok(Some("OTHER")), &login), Leave);
        // Unreadable: never overwritten.
        assert_eq!(
            unscoped_restore(Some("A"), Err(()), &login),
            KeepAndQuarantine
        );
        assert_eq!(unscoped_restore(None, Err(()), &login), Leave);
    }

    /// A fake `claude` whose login also records whether `switch.lock` was
    /// free while it ran.
    struct LockProbeClaude {
        inner: FakeClaude,
        state: PathBuf,
        lock_free_during_login: Mutex<Option<bool>>,
    }

    impl ClaudeCli for LockProbeClaude {
        fn run(
            &self,
            args: &[&str],
            dir: &Path,
            t: Duration,
            interactive: bool,
        ) -> io::Result<CliOutput> {
            if args[..2] == ["auth", "login"] {
                let free = SwitchLock::acquire(&self.state, Duration::from_millis(50)).is_ok();
                *self.lock_free_during_login.lock().unwrap() = Some(free);
            }
            self.inner.run(args, dir, t, interactive)
        }
    }

    #[test]
    fn login_add_holds_switch_lock_for_the_whole_login() {
        let w = world();
        let cli = LockProbeClaude {
            inner: FakeClaude {
                creds: creds_json("at-c", "rt-c", 5),
                status: r#"{"email":"carol@example.com"}"#.into(),
                calls: Mutex::new(Vec::new()),
            },
            state: w.tmp.path().to_path_buf(),
            lock_free_during_login: Mutex::new(None),
        };
        let live = ScriptedLiveness::stopped();
        login_add(&w.env(&live), &cli).unwrap();
        assert_eq!(*cli.lock_free_during_login.lock().unwrap(), Some(false));
        // Released afterwards.
        assert!(SwitchLock::acquire(w.tmp.path(), Duration::from_millis(50)).is_ok());
    }

    /// A fake macOS `claude` during whose login a live claude in `D`
    /// refreshes the unscoped item (the login itself writes only its dir's
    /// scoped item).
    #[cfg(unix)]
    struct RefreshDuringLogin {
        user: KeychainUser,
        grant: String,
        refreshed: String,
    }

    #[cfg(unix)]
    impl ClaudeCli for RefreshDuringLogin {
        fn run(&self, args: &[&str], dir: &Path, _: Duration, _: bool) -> io::Result<CliOutput> {
            if args[..2] == ["auth", "login"] {
                let d = dir.to_string_lossy();
                keychain::write_runtime_scoped(&self.grant, Some(&d), &self.user).unwrap();
                keychain::write_runtime_scoped(&self.refreshed, None, &self.user).unwrap();
            }
            Ok(CliOutput {
                success: true,
                stdout: r#"{"email":"carol@example.com"}"#.into(),
            })
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_change_to_the_unscoped_item_during_login_is_kept_and_the_old_grant_quarantined() {
        let fake = crate::orca::testsupport::FakeSecurity::install();
        let w = world();
        let user = w.user.clone();
        let pre = creds_json("at-a", "rt-a", 1);
        let refreshed = creds_json("at-a2", "rt-a2", 2);
        keychain::write_runtime_scoped(&pre, None, &user).unwrap();
        let cli = RefreshDuringLogin {
            user: user.clone(),
            grant: creds_json("at-c", "rt-c", 5),
            refreshed: refreshed.clone(),
        };
        let live = ScriptedLiveness::stopped();
        let mut env = w.env(&live);
        env.os = HostOs::MacOs;
        let c = login_add(&env, &cli).unwrap();
        assert!(c.id.is_some());
        assert!(
            c.leftover
                .as_deref()
                .is_some_and(|l| l.contains("changed during the login")),
            "{:?}",
            c.leftover
        );
        // The live session's rotated grant stays where it wrote it.
        assert_eq!(
            fake.get(keychain::RUNTIME_SERVICE, &user.acct).as_deref(),
            Some(refreshed.as_bytes())
        );
        // The pre-login grant is filed, not dropped.
        let q = Quarantine::new(HostOs::MacOs, w.tmp.path());
        let list = q.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].reason, Reason::ChangedDuringLogin);
        assert_eq!(q.get(&list[0].fingerprint).unwrap().unwrap().expose(), pre);
    }

    /// Orca comes up during the login and already lists the account: csm
    /// wrote nothing, so this is a duplicate, as it is offline. The fresh
    /// grant is filed, and the add is reported as refused, not "added".
    #[cfg(unix)]
    #[test]
    fn a_login_orca_already_lists_is_a_duplicate_and_quarantines_the_grant() {
        use crate::orca::testsupport::{FakeOrca, OrcaModel, model_handler};
        use std::sync::Arc;
        let model = Arc::new(Mutex::new(OrcaModel::default()));
        let fake = FakeOrca::start(model_handler(model.clone()));
        model.lock().unwrap().accounts = vec![record_json(
            fake.user_data(),
            "id-a",
            "alice@example.com",
            None,
        )];
        let tmp = tempfile::tempdir().unwrap();
        let choice = crate::orca::userdata::data_file(fake.user_data());
        let user = KeychainUser {
            acct: "t".into(),
            delete_accts: vec![],
        };
        let live = ScriptedLiveness::appears_at(0);
        let env = AccountsEnv {
            os: HostOs::Linux,
            user_data: fake.user_data(),
            data_file: &choice,
            state: tmp.path(),
            keychain_user: &user,
            live: &live,
            version_ok: true,
            store_access_allowed: true,
            lock_wait: Duration::from_secs(2),
            redo: RedoOpts {
                wait: Duration::from_millis(300),
                poll: Duration::from_millis(20),
            },
            mutation_timeout: Duration::from_secs(2),
        };
        let grant = creds_json("at-a2", "rt-a2", 5);
        let cli = FakeClaude {
            creds: grant.clone(),
            status: r#"{"email":"alice@example.com","organizationUuid":null}"#.into(),
            calls: Mutex::new(Vec::new()),
        };
        let e = login_add(&env, &cli).unwrap_err().to_string();
        let fp = crate::orca::quarantine::fingerprint(&grant);
        assert!(e.contains("already added") && e.contains(&fp), "{e}");
        let q = Quarantine::new(HostOs::Linux, tmp.path());
        let list = q.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].reason, Reason::LoginNotAdded);
        assert!(
            fake.requests()
                .iter()
                .all(|r| r["method"] != "accounts.addClaudeFromConfigDir")
        );
        assert_eq!(model.lock().unwrap().accounts.len(), 1);
        assert!(!cli.calls.lock().unwrap()[0].1.exists());
    }

    #[cfg(unix)]
    #[test]
    fn remove_over_rpc_when_orca_runs() {
        use crate::orca::testsupport::{FakeOrca, OrcaModel, model_handler, running_mark};
        use std::sync::Arc;
        let model = Arc::new(Mutex::new(OrcaModel::default()));
        let fake = FakeOrca::start(model_handler(model.clone()));
        let tmp = tempfile::tempdir().unwrap();
        model.lock().unwrap().accounts = vec![record_json(
            fake.user_data(),
            "id-b",
            "bob@example.com",
            None,
        )];
        let choice = crate::orca::userdata::data_file(fake.user_data());
        let user = KeychainUser {
            acct: "t".into(),
            delete_accts: vec![],
        };
        let live = ScriptedLiveness::new(vec![running_mark()]);
        let env = AccountsEnv {
            os: HostOs::Linux,
            user_data: fake.user_data(),
            data_file: &choice,
            state: tmp.path(),
            keychain_user: &user,
            live: &live,
            version_ok: true,
            store_access_allowed: true,
            lock_wait: Duration::from_secs(2),
            redo: RedoOpts::default(),
            mutation_timeout: Duration::from_secs(2),
        };
        let c = remove(&env, "id-b").unwrap();
        assert_eq!(c.route, Route::Rpc);
        assert!(model.lock().unwrap().accounts.is_empty());
    }

    #[test]
    fn import_paths_are_trimmed_like_orca() {
        assert_eq!(
            trim_config_dir(Path::new("  /Users/example/.claude.work \n")).unwrap(),
            PathBuf::from("/Users/example/.claude.work")
        );
        assert_eq!(
            trim_config_dir(Path::new("\u{feff}/x")).unwrap(),
            PathBuf::from("/x")
        );
        for blank in ["", "   ", "\t\n"] {
            assert!(
                trim_config_dir(Path::new(blank))
                    .unwrap_err()
                    .to_string()
                    .contains("path is required")
            );
        }
        // A blank path is refused before claude runs or the lock is taken.
        struct NoClaude;
        impl ClaudeCli for NoClaude {
            fn run(&self, _: &[&str], _: &Path, _: Duration, _: bool) -> io::Result<CliOutput> {
                panic!("claude must not run for a blank path")
            }
        }
        let w = world();
        let live = ScriptedLiveness::stopped();
        let e = import(&w.env(&live), &NoClaude, Path::new("  "), None)
            .unwrap_err()
            .to_string();
        assert!(e.contains("path is required"), "{e}");
    }

    // ─── doctor repairs ───────────────────────────────────────────────────────

    #[test]
    fn remove_orphan_rechecks_the_store_under_the_lock() {
        let w = world();
        let root = crate::orca::userdata::claude_accounts_root(&w.ud);
        let live = ScriptedLiveness::stopped();
        for id in ["id-c", "id-z"] {
            let s = stash::create(&w.ud, id).unwrap();
            s.write_auth(
                &w.ud,
                HostOs::Linux,
                &creds_json(id, id, 1),
                &oauth_json(id, "carol@example.com", None),
            )
            .unwrap();
        }
        // A concurrent add landed its record for id-c after the doctor's
        // snapshot: the stash is no longer an orphan and stays.
        let recs: Vec<Value> = ["id-a", "id-b", "id-c"]
            .iter()
            .map(|id| record_json(&w.ud, id, &format!("{id}@example.com"), None))
            .collect();
        write_store(&w.ud, &recs, Some("id-a"));
        let e = remove_orphan(&w.env(&live), "id-c")
            .unwrap_err()
            .to_string();
        assert!(e.contains("names stash id-c"), "{e}");
        assert!(root.join("id-c").exists());

        // Held switch.lock (an add in flight): the repair waits, then gives up.
        {
            let _held = SwitchLock::acquire(w.tmp.path(), Duration::from_secs(1)).unwrap();
            let mut env = w.env(&live);
            env.lock_wait = Duration::from_millis(50);
            assert!(remove_orphan(&env, "id-z").is_err());
            assert!(root.join("id-z").exists());
        }

        // Orca running or an untested version: nothing happens.
        let running = ScriptedLiveness::new(vec![crate::orca::testsupport::running_mark()]);
        assert!(remove_orphan(&w.env(&running), "id-z").is_err());
        let mut env = w.env(&live);
        env.version_ok = false;
        assert!(remove_orphan(&env, "id-z").is_err());
        assert!(root.join("id-z").exists());

        // A true orphan: the grant goes to the quarantine, then the dir goes.
        assert_eq!(remove_orphan(&w.env(&live), "id-z").unwrap(), None);
        assert!(!root.join("id-z").exists());
        let q = Quarantine::new(HostOs::Linux, w.tmp.path()).list();
        assert_eq!(q.len(), 1);
        assert_eq!(
            q[0].fingerprint,
            crate::orca::quarantine::fingerprint(&creds_json("id-z", "id-z", 1))
        );
        assert_eq!(q[0].reason, Reason::Orphaned);
    }

    #[test]
    fn purge_quarantine_rechecks_the_holder_under_the_lock() {
        let w = world();
        let live = ScriptedLiveness::stopped();
        let q = Quarantine::new(HostOs::Linux, w.tmp.path());
        let grant = creds_json("id-b", "id-b", 1);
        let fp = crate::orca::quarantine::fingerprint(&grant);
        q.file(&grant, Reason::Orphaned, "test", None, None, 1)
            .unwrap();
        // id-a holds another grant: the entry stays.
        assert!(purge_quarantine(&w.env(&live), &fp, "id-a").is_err());
        // An id no record names: the entry stays.
        assert!(purge_quarantine(&w.env(&live), &fp, "id-zz").is_err());
        assert_eq!(q.list().len(), 1);
        // id-b holds exactly this grant.
        purge_quarantine(&w.env(&live), &fp, "id-b").unwrap();
        assert!(q.list().is_empty());
    }

    /// Round 8: the fingerprint names only the Claude grant. An entry that
    /// also holds MCP logins the stash lacks (a retired dir's copy) is not
    /// superseded by the stash, so `--fix` keeps it.
    #[test]
    fn purge_quarantine_keeps_an_entry_with_logins_the_stash_lacks() {
        let w = world();
        let live = ScriptedLiveness::stopped();
        let q = Quarantine::new(HostOs::Linux, w.tmp.path());
        let mut v: serde_json::Value =
            serde_json::from_str(&creds_json("id-b", "id-b", 1)).unwrap();
        v["mcpOAuth"] = json!({"srv-a": {"accessToken": "mcp-tok"}});
        let entry = v.to_string();
        let fp = q
            .file(&entry, Reason::ExtraLogins, "file", Some("id-b"), None, 1)
            .unwrap()
            .fingerprint()
            .to_owned();
        let err = purge_quarantine(&w.env(&live), &fp, "id-b").unwrap_err();
        assert!(err.to_string().contains("mcpOAuth/srv-a"), "{err}");
        assert!(!err.to_string().contains("mcp-tok"), "{err}");
        assert_eq!(q.get(&fp).unwrap().unwrap().expose(), entry);
        assert_eq!(q.list().len(), 1);
    }
}
