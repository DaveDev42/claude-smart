//! Orca interop: csm cooperating with the Orca desktop app's Claude account
//! switching.
//!
//! # Model
//!
//! Orca keeps each Claude account as a credential stash
//! (`<userData>/claude-accounts/<uuid>/auth/` + a Keychain item) and switches
//! by *materializing* the active account into ONE runtime dir: Orca's own
//! `process.env.CLAUDE_CONFIG_DIR?.trim()`, or `~/.claude` when unset. csm's
//! model is one config dir per account. The two meet in the **slot**
//! ([`slot`]): a dedicated registered csm profile (default `orca`, dir
//! `~/.claude.orca`) whose dir IS Orca's runtime dir. The launchd/HKCU floor
//! points at the slot so Orca inherits it; shells keep starting in a
//! per-account dir (`csm cas --print-default-dir` is unchanged). The slot is
//! never an account of its own and csm never launches into it by accident.
//!
//! Orca accounts are bound to csm profiles by identity ([`bind`]); csm
//! follows Orca's live active account on launch ([`follow`]) and can ask Orca
//! to select an account ([`select_account`]).
//!
//! # Orca facts relied on (Orca 1.4.209)
//!
//! - userData: `$ORCA_USER_DATA_PATH`, else macOS `~/Library/Application
//!   Support/orca`, Linux `$XDG_CONFIG_HOME/orca` or `~/.config/orca`,
//!   Windows `%APPDATA%\orca` ([`user_data_dir`]).
//! - `<userData>/orca-runtime.json` = `{runtimeId, pid, transports:[{kind,
//!   endpoint}], authToken, startedAt}`, rewritten at each start; running ⇔
//!   `pid` alive ([`RuntimeMetadata`]).
//! - RPC framing and the two methods csm uses: see [`rpc`].
//! - `accounts.list` → `claude = {accounts:[{id, email, managedAuthRuntime,
//!   …}], activeAccountId, activeAccountIdsByRuntime:{host, wsl}}`; the
//!   effective active id is `activeAccountIdsByRuntime.host ??
//!   activeAccountId`, `null` meaning Orca's "System default"
//!   ([`Selection::effective_active_id`]).
//! - Offline: `<userData>/profiles/local-default/orca-data.json`
//!   ([`data_file`]) — display only, never drives a launch.
//!
//! # What csm never does
//!
//! Never sends `accountId: null`; never calls `accounts.removeClaude` or
//! `accounts.addClaudeFromConfigDir` (they are not even encodable, see
//! [`rpc::Method`]); never writes `orca-data.json` or `claude-accounts/*`;
//! never reads Orca's stash secret; never prints, logs, or persists the RPC
//! `authToken`. The ONLY write surface is `accounts.selectClaude` with a
//! non-empty id, and only after the runtime-dir check in [`select_account`].

pub mod bind;
pub mod data_file;
pub mod follow;
pub mod identity;
pub mod integrate;
pub mod pending;
pub mod rpc;
pub mod slot;

pub use identity::Identity;

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;

use self::rpc::{AuthToken, Method, RpcError};

// ─── types ────────────────────────────────────────────────────────────────────

/// One Orca-managed Claude account (the record shape `accounts.list` and
/// `orca-data.json` share). Secrets are never part of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Account {
    pub id: String,
    pub email: String,
    #[serde(rename = "organizationUuid")]
    pub organization_uuid: Option<String>,
    #[serde(rename = "organizationName")]
    pub organization_name: Option<String>,
    /// `managedAuthRuntime`: `"host"` (default) or `"wsl"`. Only host
    /// accounts bind to csm profiles.
    pub runtime: String,
}

impl Account {
    pub fn is_host(&self) -> bool {
        self.runtime == "host"
    }
}

/// One `rateLimits.claude` window (display only).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RateWindow {
    #[serde(rename = "usedPercent")]
    pub used_percent: Option<f64>,
    #[serde(rename = "resetsAtMs")]
    pub resets_at_ms: Option<i64>,
}

/// `rateLimits.claude` for Orca's active account (display only — csm's own
/// usage collection stays authoritative for scoring).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RateLimits {
    pub session: Option<RateWindow>,
    pub weekly: Option<RateWindow>,
    #[serde(rename = "fableWeekly")]
    pub fable_weekly: Option<RateWindow>,
}

/// Orca's account list plus its active-account pointers.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Selection {
    pub accounts: Vec<Account>,
    /// `activeAccountId`.
    #[serde(rename = "activeAccountId")]
    pub active_id: Option<String>,
    /// `activeAccountIdsByRuntime.host`.
    #[serde(rename = "hostActiveAccountId")]
    pub host_active_id: Option<String>,
    #[serde(rename = "rateLimits", skip_serializing_if = "Option::is_none")]
    pub rate_limits: Option<RateLimits>,
}

impl Selection {
    /// `activeAccountIdsByRuntime.host ?? activeAccountId`; `None` is Orca's
    /// "System default" (no managed account active).
    pub fn effective_active_id(&self) -> Option<&str> {
        self.host_active_id
            .as_deref()
            .or(self.active_id.as_deref())
            .filter(|s| !s.is_empty())
    }

    pub fn account(&self, id: &str) -> Option<&Account> {
        self.accounts.iter().find(|a| a.id == id)
    }

    pub fn active_account(&self) -> Option<&Account> {
        self.effective_active_id().and_then(|id| self.account(id))
    }
}

/// What csm knows about Orca right now.
#[derive(Debug, Clone, PartialEq)]
pub enum OrcaState {
    /// Orca is running and answered `accounts.list`.
    Live(Selection),
    /// Orca is not running; this is its last persisted state (display only).
    Offline(Selection),
    /// No Orca here (no userData / no runtime file / no data file).
    Absent,
    /// Orca looks present but its state could not be read (timeout, still
    /// starting, unsupported transport, …).
    Unknown(String),
}

// ─── tolerant JSON walking (shared by the RPC result and orca-data.json) ────

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// One account record (`i9i` shape). Records without an `id` are skipped.
pub(crate) fn parse_account(v: &Value) -> Option<Account> {
    Some(Account {
        id: str_field(v, "id")?,
        email: str_field(v, "email").unwrap_or_default(),
        organization_uuid: str_field(v, "organizationUuid"),
        organization_name: str_field(v, "organizationName"),
        runtime: str_field(v, "managedAuthRuntime").unwrap_or_else(|| "host".to_owned()),
    })
}

pub(crate) fn parse_accounts(v: Option<&Value>) -> Vec<Account> {
    v.and_then(Value::as_array)
        .map(|a| a.iter().filter_map(parse_account).collect())
        .unwrap_or_default()
}

/// A `{accounts, activeAccountId, activeAccountIdsByRuntime}` snapshot — the
/// `claude` member of `accounts.list`, and the whole `accounts.selectClaude`
/// result.
pub fn parse_claude_snapshot(v: &Value) -> Option<Selection> {
    v.as_object()?;
    Some(Selection {
        accounts: parse_accounts(v.get("accounts")),
        active_id: str_field(v, "activeAccountId"),
        host_active_id: v
            .get("activeAccountIdsByRuntime")
            .and_then(|r| str_field(r, "host")),
        rate_limits: None,
    })
}

fn parse_window(v: Option<&Value>) -> Option<RateWindow> {
    let v = v?.as_object()?;
    Some(RateWindow {
        used_percent: v.get("usedPercent").and_then(Value::as_f64),
        resets_at_ms: v.get("resetsAt").and_then(Value::as_i64),
    })
}

/// The `accounts.list` result: `{claude, codex, rateLimits}`.
pub fn parse_list_result(v: &Value) -> Option<Selection> {
    let mut sel = parse_claude_snapshot(v.get("claude")?)?;
    if let Some(rl) = v.get("rateLimits").and_then(|r| r.get("claude")) {
        sel.rate_limits = Some(RateLimits {
            session: parse_window(rl.get("session")),
            weekly: parse_window(rl.get("weekly")),
            fable_weekly: parse_window(rl.get("fableWeekly")),
        });
    }
    Some(sel)
}

// ─── small I/O helpers ────────────────────────────────────────────────────────

/// Read `path` as UTF-8, refusing anything that is not a regular file (a FIFO
/// would block forever) or that exceeds `cap` bytes. `Ok(None)` when absent.
/// Error messages name the path and the reason, never the content.
pub(crate) fn read_capped(path: &Path, cap: u64) -> io::Result<Option<String>> {
    use std::io::Read;

    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is not a regular file", path.display()),
        ));
    }
    if meta.len() > cap {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} exceeds {cap} bytes", path.display()),
        ));
    }
    let mut buf = Vec::new();
    file.take(cap + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > cap {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} exceeds {cap} bytes", path.display()),
        ));
    }
    String::from_utf8(buf).map(Some).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is not UTF-8", path.display()),
        )
    })
}

/// Write `bytes` to `path` via a sibling tmp file + rename (same dir, so the
/// rename is atomic). Creates the parent dir.
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(format!(".tmp.{}", std::process::id()));
    let tmp = path.with_file_name(tmp_name);
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

// ─── userData dir ─────────────────────────────────────────────────────────────

/// The OS family whose userData convention applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostOs {
    MacOs,
    Linux,
    Windows,
}

impl HostOs {
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            HostOs::MacOs
        } else if cfg!(windows) {
            HostOs::Windows
        } else {
            HostOs::Linux
        }
    }
}

fn non_empty(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|s| !s.is_empty())
}

/// Expand a leading `~/` against `home`.
fn expand_tilde(p: &str, home: Option<&Path>) -> PathBuf {
    match (p.strip_prefix("~/"), home) {
        (Some(rest), Some(h)) => h.join(rest),
        _ => PathBuf::from(p),
    }
}

/// Pure userData resolution: config override, then `$ORCA_USER_DATA_PATH`,
/// then the platform default (Electron's `app.getPath("userData")` for an
/// app named `orca`).
pub(crate) fn resolve_user_data_dir(
    config_override: Option<&str>,
    env_override: Option<&str>,
    home: Option<&Path>,
    xdg_config_home: Option<&str>,
    appdata: Option<&str>,
    os: HostOs,
) -> Option<PathBuf> {
    if let Some(p) = non_empty(config_override) {
        return Some(expand_tilde(p, home));
    }
    if let Some(p) = non_empty(env_override) {
        return Some(PathBuf::from(p));
    }
    match os {
        HostOs::MacOs => Some(home?.join("Library/Application Support/orca")),
        HostOs::Linux => match non_empty(xdg_config_home) {
            Some(x) => Some(PathBuf::from(x).join("orca")),
            None => Some(home?.join(".config").join("orca")),
        },
        HostOs::Windows => non_empty(appdata).map(|a| PathBuf::from(a).join("orca")),
    }
}

/// Orca's userData dir for `cfg` (the `orca` config block).
pub fn user_data_dir_for(cfg: &crate::config::OrcaConfig) -> Option<PathBuf> {
    let env = |k: &str| std::env::var(k).ok();
    resolve_user_data_dir(
        cfg.user_data_dir.as_deref(),
        env("ORCA_USER_DATA_PATH").as_deref(),
        crate::paths::home_dir().as_deref(),
        env("XDG_CONFIG_HOME").as_deref(),
        env("APPDATA").as_deref(),
        HostOs::current(),
    )
}

/// Orca's userData dir, honouring `orca.userDataDir` from csm's config (an
/// unreadable config falls back to the env/platform default).
pub fn user_data_dir() -> Option<PathBuf> {
    let cfg = crate::config::Config::load().unwrap_or_default();
    user_data_dir_for(cfg.orca())
}

// ─── runtime metadata ─────────────────────────────────────────────────────────

/// Cap on `orca-runtime.json` (a real one is a few hundred bytes).
const RUNTIME_FILE_CAP: u64 = 1024 * 1024;

/// One transport Orca advertises.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transport {
    /// `"unix"`, `"named-pipe"`, or `"websocket"`.
    pub kind: String,
    pub endpoint: String,
}

/// `<userData>/orca-runtime.json`. Holds the RPC `authToken`, so `Debug`
/// redacts it (via [`AuthToken`]) and nothing here is `Serialize`.
#[derive(Clone)]
pub struct RuntimeMetadata {
    pub runtime_id: String,
    pub pid: u32,
    pub transports: Vec<Transport>,
    pub auth_token: AuthToken,
    pub started_at: Option<i64>,
}

impl std::fmt::Debug for RuntimeMetadata {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeMetadata")
            .field("runtime_id", &self.runtime_id)
            .field("pid", &self.pid)
            .field("transports", &self.transports)
            .field("auth_token", &self.auth_token)
            .field("started_at", &self.started_at)
            .finish()
    }
}

impl RuntimeMetadata {
    /// Is Orca's pid alive? (A stale file from a crashed Orca → `false`.)
    pub fn is_alive(&self) -> bool {
        self.pid != 0 && crate::platform::proc::is_running(self.pid)
    }

    /// The unix-socket endpoint, if Orca advertises one AND this is a unix
    /// build. The named pipe and the websocket are never used.
    pub fn unix_endpoint(&self) -> Option<&str> {
        if !cfg!(unix) {
            return None;
        }
        self.transports
            .iter()
            .find(|t| t.kind == "unix" && !t.endpoint.is_empty())
            .map(|t| t.endpoint.as_str())
    }
}

/// Parse `orca-runtime.json`. Pure. Errors are generic on purpose: the text
/// holds a secret, and a serde message can quote the offending value.
pub fn parse_runtime_metadata(text: &str) -> Result<RuntimeMetadata, &'static str> {
    let v: Value = serde_json::from_str(text).map_err(|_| "orca-runtime.json is not JSON")?;
    let runtime_id = str_field(&v, "runtimeId").ok_or("orca-runtime.json has no runtimeId")?;
    let pid = v
        .get("pid")
        .and_then(Value::as_u64)
        .and_then(|p| u32::try_from(p).ok())
        .ok_or("orca-runtime.json has no valid pid")?;
    let token = v
        .get("authToken")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or("orca-runtime.json has no authToken")?;
    let transports = v
        .get("transports")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|t| {
                    Some(Transport {
                        kind: str_field(t, "kind")?,
                        endpoint: str_field(t, "endpoint")?,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(RuntimeMetadata {
        runtime_id,
        pid,
        transports,
        auth_token: AuthToken::new(token.to_owned()),
        started_at: v.get("startedAt").and_then(Value::as_i64),
    })
}

/// `orca-runtime.json` under `user_data`: `Ok(None)` when absent.
pub fn runtime_metadata_in(user_data: &Path) -> io::Result<Option<RuntimeMetadata>> {
    let path = user_data.join("orca-runtime.json");
    let Some(text) = read_capped(&path, RUNTIME_FILE_CAP)? else {
        return Ok(None);
    };
    parse_runtime_metadata(&text)
        .map(Some)
        .map_err(|m| io::Error::new(io::ErrorKind::InvalidData, m))
}

/// `orca-runtime.json` in the resolved userData dir.
#[allow(dead_code)] // convenience wrapper; current callers pass an explicit userData dir
pub fn runtime_metadata() -> io::Result<Option<RuntimeMetadata>> {
    match user_data_dir() {
        Some(ud) => runtime_metadata_in(&ud),
        None => Ok(None),
    }
}

// ─── Orca's actual runtime dir (process environment) ─────────────────────────

/// Orca's runtime-dir rule applied to a process environment block: a
/// non-blank `CLAUDE_CONFIG_DIR` (trimmed, as Orca trims it), else
/// `$HOME/.claude` (`HOME` from the block, else `fallback_home`). Pure.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn config_dir_from_environ<S: AsRef<std::ffi::OsStr>>(
    env: &[S],
    fallback_home: Option<&Path>,
) -> Option<PathBuf> {
    let lookup = |key: &str| {
        env.iter().find_map(|kv| {
            let kv = kv.as_ref().to_str()?;
            let (k, v) = kv.split_once('=')?;
            (k == key).then(|| v.to_owned())
        })
    };
    if let Some(dir) = lookup("CLAUDE_CONFIG_DIR") {
        let dir = dir.trim();
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    let home = lookup("HOME")
        .filter(|h| !h.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| fallback_home.map(Path::to_path_buf))?;
    Some(home.join(".claude"))
}

/// The dir the running Orca (`pid`) materializes accounts into, read from
/// its process environment. `None` when the environment cannot be read (a
/// failed probe, a permission error) and always on non-unix builds — callers
/// treat `None` as "refuse to select".
pub fn runtime_config_dir(pid: u32) -> Option<PathBuf> {
    #[cfg(unix)]
    {
        let env = crate::platform::proc::environ(pid)?;
        config_dir_from_environ(&env, crate::paths::home_dir().as_deref())
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        None
    }
}

// ─── reading Orca's state ─────────────────────────────────────────────────────

/// One `accounts.list {"refreshUsage":false}` against `meta`.
fn list_via(meta: &RuntimeMetadata, deadline: Instant) -> Result<Selection, RpcError> {
    let v = rpc::call(meta, &Method::AccountsList, deadline)?;
    parse_list_result(&v).ok_or_else(|| RpcError::Malformed("accounts.list result".to_owned()))
}

fn live_selection_impl(user_data: &Path, budget: Duration, launch_path: bool) -> OrcaState {
    let deadline = Instant::now() + budget;
    let meta = match runtime_metadata_in(user_data) {
        Ok(Some(m)) => m,
        Ok(None) => return OrcaState::Absent,
        Err(e) => return OrcaState::Unknown(e.to_string()),
    };
    if !meta.is_alive() {
        return OrcaState::Absent;
    }
    if meta.unix_endpoint().is_none() {
        return OrcaState::Unknown(RpcError::Unsupported.to_string());
    }
    if launch_path && rpc::negative_cache_until(&meta.runtime_id).is_some() {
        return OrcaState::Unknown("Orca RPC recently timed out (negative cache)".to_owned());
    }
    match list_via(&meta, deadline) {
        Ok(sel) => {
            rpc::clear_negative_cache();
            OrcaState::Live(sel)
        }
        Err(e) => {
            if launch_path && e == RpcError::Timeout {
                rpc::write_negative_cache(&meta.runtime_id);
            }
            OrcaState::Unknown(e.to_string())
        }
    }
}

/// Orca's LIVE selection for the launch path: `Live`, `Absent` (not running),
/// or `Unknown`. Honours the negative cache and records a timeout in it.
#[allow(dead_code)] // convenience wrapper; current callers pass an explicit userData dir
pub fn live_selection(budget: Duration) -> OrcaState {
    match user_data_dir() {
        Some(ud) => live_selection_in(&ud, budget),
        None => OrcaState::Absent,
    }
}

/// [`live_selection`] against an explicit userData dir.
pub fn live_selection_in(user_data: &Path, budget: Duration) -> OrcaState {
    live_selection_impl(user_data, budget, true)
}

/// Like [`live_selection`] but ignores the negative cache (explicit
/// commands: `csm orca status`/`accounts`/`sync`). Clears it on success.
pub fn live_selection_fresh_in(user_data: &Path, budget: Duration) -> OrcaState {
    live_selection_impl(user_data, budget, false)
}

/// Orca's last persisted selection from `orca-data.json` (display only).
#[allow(dead_code)] // convenience wrapper; current callers pass an explicit userData dir
pub fn offline_selection() -> io::Result<Option<Selection>> {
    match user_data_dir() {
        Some(ud) => offline_selection_in(&ud),
        None => Ok(None),
    }
}

/// [`offline_selection`] against an explicit userData dir.
pub fn offline_selection_in(user_data: &Path) -> io::Result<Option<Selection>> {
    Ok(data_file::read_in(user_data)?.map(|d| d.selection))
}

/// Best available state for display: live when Orca runs, its persisted state
/// when it does not, `Unknown` when it runs but cannot be read.
pub fn selection_in(user_data: &Path, budget: Duration) -> OrcaState {
    match live_selection_fresh_in(user_data, budget) {
        OrcaState::Absent => match offline_selection_in(user_data) {
            Ok(Some(sel)) => OrcaState::Offline(sel),
            Ok(None) => OrcaState::Absent,
            Err(e) => OrcaState::Unknown(e.to_string()),
        },
        other => other,
    }
}

// ─── select ───────────────────────────────────────────────────────────────────

/// A select that did not fail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectOutcome {
    /// Orca's effective active account is now the target.
    Selected,
    /// Orca did not confirm in time (a slow select, or Orca still starting);
    /// it may still land.
    Unknown,
}

/// Why a select was refused or failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SelectError {
    #[error("Orca is not running")]
    NotRunning,
    #[error("Orca's runtime dir ({}) is not the csm slot dir; refusing to select", actual.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "unreadable".to_owned()))]
    RuntimeDirMismatch { actual: Option<PathBuf> },
    #[error("Orca rejected the selection: {0}")]
    Rejected(String),
    #[error("Orca account selection is not supported on this platform (no unix socket)")]
    Unsupported,
    /// The select never reached Orca (its runtime metadata was unreadable,
    /// the socket refused the connect, or the budget ran out first). Nothing
    /// was asked of Orca, so a queued choice stays queued.
    #[error("Orca could not be reached: {0}")]
    Unreachable(String),
}

/// Poll interval while waiting for a slow select to become visible.
const SELECT_POLL: Duration = Duration::from_millis(200);

/// Ask Orca to make `target_id` its active Claude account, within `budget`.
///
/// Safety check first (mandatory, no bypass): Orca's ACTUAL runtime dir, read
/// from its process environment, must equal `slot_dir`; an unreadable
/// environment refuses too (so this is off on Windows). Then a read-only
/// `accounts.list` pre-check (already active → `Selected` without a write;
/// unknown or non-host account → `Rejected`), then `accounts.selectClaude`.
/// A slow select is `Unknown`, not failure: csm polls `accounts.list` every
/// 200 ms until the target is active or the budget is spent. Only an
/// `ok:false` frame or a confirmed different active id is `Rejected`; a
/// select that was never written (unreadable runtime metadata, a refused
/// connect, a spent budget) is [`SelectError::Unreachable`], never `Unknown`.
pub fn select_account(
    target_id: &str,
    slot_dir: &str,
    budget: Duration,
) -> Result<SelectOutcome, SelectError> {
    let Some(ud) = user_data_dir() else {
        return Err(SelectError::NotRunning);
    };
    select_account_in(&ud, target_id, slot_dir, budget, runtime_config_dir)
}

/// [`select_account`] with the userData dir and the runtime-dir probe
/// injected (the fake-socket tests' seam).
pub(crate) fn select_account_in(
    user_data: &Path,
    target_id: &str,
    slot_dir: &str,
    budget: Duration,
    probe_runtime_dir: impl Fn(u32) -> Option<PathBuf>,
) -> Result<SelectOutcome, SelectError> {
    let start = Instant::now();
    let deadline = start + budget;
    let Some(method) = Method::select_claude(target_id) else {
        return Err(SelectError::Rejected("empty account id".to_owned()));
    };
    let meta = match runtime_metadata_in(user_data) {
        Ok(Some(m)) if m.is_alive() => m,
        Ok(_) => return Err(SelectError::NotRunning),
        // Possibly Orca mid-rewrite of the file: a transient read, not an
        // answer from Orca, so it must not drop a queued choice.
        Err(e) => {
            return Err(SelectError::Unreachable(format!(
                "its runtime metadata could not be read ({e})"
            )));
        }
    };
    if meta.unix_endpoint().is_none() {
        return Err(SelectError::Unsupported);
    }
    let actual = probe_runtime_dir(meta.pid);
    let matches = actual
        .as_deref()
        .and_then(Path::to_str)
        .is_some_and(|a| crate::cas::platform::dirs_equal(a, slot_dir));
    if !matches {
        return Err(SelectError::RuntimeDirMismatch { actual });
    }

    // Read-only pre-check, bounded to a third of the budget so a hung read
    // still leaves the select (and the confirmation poll) time of its own.
    match list_via(&meta, start + budget / 3) {
        Ok(sel) => {
            if sel.effective_active_id() == Some(target_id) {
                return Ok(SelectOutcome::Selected);
            }
            match sel.account(target_id) {
                None => {
                    return Err(SelectError::Rejected(format!(
                        "Orca has no Claude account with id {target_id}"
                    )));
                }
                Some(a) if !a.is_host() => {
                    return Err(SelectError::Rejected(format!(
                        "account {target_id} is a {} account, not a host account",
                        a.runtime
                    )));
                }
                Some(_) => {}
            }
        }
        Err(e) if e.is_services_starting() => return Ok(SelectOutcome::Unknown),
        Err(RpcError::RuntimeIdMismatch) => {
            return Err(SelectError::Rejected(
                RpcError::RuntimeIdMismatch.to_string(),
            ));
        }
        // The socket refused the connect: the select would too.
        Err(RpcError::NotSent(m)) => return Err(SelectError::Unreachable(m)),
        // Other transport trouble on the read: let the select itself decide.
        Err(_) => {}
    }

    // The select gets half of what is left so a slow one still leaves time
    // to watch the (early-committed) active id change.
    let remaining = deadline.saturating_duration_since(Instant::now());
    let select_deadline = Instant::now() + remaining / 2;
    match rpc::call(&meta, &method, select_deadline) {
        Ok(v) => {
            let snap = parse_claude_snapshot(&v);
            return match snap.as_ref().and_then(|s| s.effective_active_id()) {
                Some(id) if id == target_id => Ok(SelectOutcome::Selected),
                Some(other) => Err(SelectError::Rejected(format!(
                    "Orca kept account {other} active"
                ))),
                None if snap.is_some() => Err(SelectError::Rejected(
                    "Orca reports no active account after the select".to_owned(),
                )),
                None => Ok(SelectOutcome::Unknown),
            };
        }
        Err(e) if e.is_services_starting() => return Ok(SelectOutcome::Unknown),
        Err(e @ RpcError::Remote { .. }) => return Err(SelectError::Rejected(e.to_string())),
        Err(e @ RpcError::RuntimeIdMismatch) => {
            return Err(SelectError::Rejected(e.to_string()));
        }
        Err(RpcError::Unsupported) => return Err(SelectError::Unsupported),
        // Never written: Orca cannot act on it, so do not poll into Unknown.
        Err(RpcError::NotSent(m)) => return Err(SelectError::Unreachable(m)),
        // Timeout / Io / Closed / TooLarge / Malformed: the select may still
        // land — watch for it.
        Err(_) => {}
    }
    poll_until_active(&meta, target_id, deadline)
}

fn poll_until_active(
    meta: &RuntimeMetadata,
    target_id: &str,
    deadline: Instant,
) -> Result<SelectOutcome, SelectError> {
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Ok(SelectOutcome::Unknown);
        }
        std::thread::sleep(SELECT_POLL.min(deadline - now));
        if let Ok(sel) = list_via(meta, deadline)
            && sel.effective_active_id() == Some(target_id)
        {
            return Ok(SelectOutcome::Selected);
        }
    }
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn list_fixture() -> Value {
        json!({
            "claude": {
                "accounts": [
                    {"id": "acct-1", "email": "alice@example.com", "managedAuthRuntime": "host",
                     "organizationUuid": "org-1", "organizationName": "Acme"},
                    {"id": "acct-2", "email": "bob@example.com"},
                    {"id": "acct-3", "email": "carol@example.com", "managedAuthRuntime": "wsl"},
                    {"email": "no-id@example.com"}
                ],
                "activeAccountId": "acct-2",
                "activeAccountIdsByRuntime": {"host": "acct-1", "wsl": {}}
            },
            "codex": {},
            "rateLimits": {"claude": {"session": {"usedPercent": 42.5, "windowMinutes": 300,
                "resetsAt": 1_700_000_000_000_i64}}}
        })
    }

    #[test]
    fn list_result_parses_accounts_and_active_ids() {
        let sel = parse_list_result(&list_fixture()).unwrap();
        assert_eq!(sel.accounts.len(), 3, "the id-less record is skipped");
        assert_eq!(sel.accounts[1].runtime, "host", "runtime defaults to host");
        assert!(!sel.accounts[2].is_host());
        assert_eq!(sel.effective_active_id(), Some("acct-1"), "host wins");
        assert_eq!(sel.active_account().unwrap().email, "alice@example.com");
        let rl = sel.rate_limits.unwrap();
        assert_eq!(rl.session.unwrap().used_percent, Some(42.5));
        assert!(rl.weekly.is_none());
    }

    #[test]
    fn effective_active_falls_back_to_active_account_id() {
        let sel = parse_claude_snapshot(&json!({
            "accounts": [], "activeAccountId": "acct-2",
            "activeAccountIdsByRuntime": {"host": null}
        }))
        .unwrap();
        assert_eq!(sel.effective_active_id(), Some("acct-2"));
        let none =
            parse_claude_snapshot(&json!({"accounts": [], "activeAccountId": null})).unwrap();
        assert_eq!(none.effective_active_id(), None, "System default");
    }

    #[test]
    fn user_data_dir_resolution_order() {
        let home = Path::new("/Users/example");
        let r = |cfg, env, xdg, appdata, os| {
            resolve_user_data_dir(cfg, env, Some(home), xdg, appdata, os)
        };
        assert_eq!(
            r(Some("~/od"), Some("/env/od"), None, None, HostOs::MacOs),
            Some(PathBuf::from("/Users/example/od")),
            "config override wins (with ~ expansion)"
        );
        assert_eq!(
            r(None, Some("/env/od"), None, None, HostOs::MacOs),
            Some(PathBuf::from("/env/od"))
        );
        assert_eq!(
            r(None, Some("  "), None, None, HostOs::MacOs),
            Some(PathBuf::from(
                "/Users/example/Library/Application Support/orca"
            ))
        );
        assert_eq!(
            r(None, None, Some("/xdg"), None, HostOs::Linux),
            Some(PathBuf::from("/xdg/orca"))
        );
        assert_eq!(
            r(None, None, None, None, HostOs::Linux),
            Some(PathBuf::from("/Users/example/.config/orca"))
        );
        assert_eq!(
            r(None, None, None, Some("C:\\AppData"), HostOs::Windows),
            Some(PathBuf::from("C:\\AppData").join("orca"))
        );
        assert_eq!(r(None, None, None, None, HostOs::Windows), None);
    }

    const RUNTIME: &str = r#"{"runtimeId":"rt-1","pid":4242,
        "transports":[{"kind":"websocket","endpoint":"ws://127.0.0.1:1"},
                      {"kind":"unix","endpoint":"/tmp/o.sock"}],
        "authToken":"secret-token-value","startedAt":1700000000000}"#;

    #[test]
    fn runtime_metadata_parses_and_redacts() {
        let m = parse_runtime_metadata(RUNTIME).unwrap();
        assert_eq!(m.runtime_id, "rt-1");
        assert_eq!(m.pid, 4242);
        assert_eq!(m.transports.len(), 2);
        if cfg!(unix) {
            assert_eq!(m.unix_endpoint(), Some("/tmp/o.sock"));
        } else {
            assert_eq!(m.unix_endpoint(), None);
        }
        let dbg = format!("{m:?}");
        assert!(!dbg.contains("secret-token-value"), "{dbg}");
    }

    #[test]
    fn runtime_metadata_errors_never_echo_content() {
        let bad = r#"{"runtimeId":"rt-1","pid":"secret-token-value","authToken":"x"}"#;
        let e = parse_runtime_metadata(bad).unwrap_err();
        assert!(!e.contains("secret-token-value"));
        assert!(parse_runtime_metadata("{").is_err());
        assert!(parse_runtime_metadata(r#"{"runtimeId":"a","pid":1}"#).is_err());
    }

    #[test]
    fn runtime_metadata_absent_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(runtime_metadata_in(dir.path()).unwrap().is_none());
        assert_eq!(
            live_selection_in(dir.path(), Duration::from_millis(50)),
            OrcaState::Absent
        );
    }

    #[test]
    fn dead_pid_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        // pid 0 is never "alive" for our purposes.
        std::fs::write(
            dir.path().join("orca-runtime.json"),
            r#"{"runtimeId":"rt","pid":0,"authToken":"t","transports":[]}"#,
        )
        .unwrap();
        assert_eq!(
            live_selection_in(dir.path(), Duration::from_millis(50)),
            OrcaState::Absent
        );
        assert_eq!(
            select_account_in(
                dir.path(),
                "acct-1",
                "/x",
                Duration::from_millis(50),
                |_| None
            ),
            Err(SelectError::NotRunning)
        );
    }

    #[test]
    fn read_capped_refuses_oversize_and_non_files() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        std::fs::write(&p, "12345").unwrap();
        assert_eq!(read_capped(&p, 5).unwrap().as_deref(), Some("12345"));
        assert!(read_capped(&p, 4).is_err());
        assert!(
            read_capped(dir.path(), 100).is_err(),
            "a dir is not a regular file"
        );
        assert!(
            read_capped(&dir.path().join("missing"), 5)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn config_dir_from_environ_follows_orcas_rule() {
        let home = Path::new("/Users/example");
        assert_eq!(
            config_dir_from_environ(
                &[
                    "PATH=/bin",
                    "CLAUDE_CONFIG_DIR= /Users/example/.claude.orca "
                ],
                Some(home)
            ),
            Some(PathBuf::from("/Users/example/.claude.orca")),
            "trimmed like Orca's ?.trim()"
        );
        assert_eq!(
            config_dir_from_environ(&["CLAUDE_CONFIG_DIR=  ", "HOME=/Users/other"], Some(home)),
            Some(PathBuf::from("/Users/other/.claude")),
            "blank → $HOME/.claude from the process env"
        );
        assert_eq!(
            config_dir_from_environ::<&str>(&[], Some(home)),
            Some(PathBuf::from("/Users/example/.claude"))
        );
        assert_eq!(config_dir_from_environ::<&str>(&[], None), None);
    }

    #[test]
    fn select_refuses_an_empty_id_before_any_io() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            select_account_in(dir.path(), " ", "/x", Duration::from_millis(50), |_| None),
            Err(SelectError::Rejected(_))
        ));
    }

    // ─── select against a fake Orca (never the live one) ───────────────────

    /// A fake Orca serving `n` connections on a temp socket. `handler` gets
    /// each request and returns the `result` (Ok) or `(code, message)` (Err),
    /// or `None` to stay silent (a slow select).
    #[cfg(unix)]
    fn fake_orca(
        dir: &Path,
        n: usize,
        handler: impl Fn(&Value) -> Option<Result<Value, (&'static str, &'static str)>> + Send + 'static,
    ) -> std::thread::JoinHandle<Vec<Value>> {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;

        let sock = dir.join("o.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let meta = json!({
            "runtimeId": "rt-test",
            "pid": std::process::id(),
            "transports": [{"kind": "unix", "endpoint": sock.to_str().unwrap()}],
            "authToken": "test-token",
        });
        std::fs::write(dir.join("orca-runtime.json"), meta.to_string()).unwrap();
        std::thread::spawn(move || {
            let mut seen = Vec::new();
            let mut held = Vec::new();
            for _ in 0..n {
                let Ok((stream, _)) = listener.accept() else {
                    break;
                };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let req: Value = serde_json::from_str(&line).unwrap();
                let id = req["id"].clone();
                let reply = handler(&req);
                seen.push(req);
                let mut w = stream;
                match reply {
                    Some(Ok(result)) => {
                        let f = json!({"id": id, "ok": true, "result": result,
                                       "_meta": {"runtimeId": "rt-test"}});
                        let _ = writeln!(w, "{f}");
                    }
                    Some(Err((code, message))) => {
                        let f = json!({"id": id, "ok": false,
                                       "error": {"code": code, "message": message}});
                        let _ = writeln!(w, "{f}");
                    }
                    None => held.push(w),
                }
            }
            seen
        })
    }

    #[cfg(unix)]
    fn snapshot(active: &str) -> Value {
        json!({
            "accounts": [
                {"id": "acct-1", "email": "alice@example.com", "managedAuthRuntime": "host"},
                {"id": "acct-2", "email": "bob@example.com", "managedAuthRuntime": "host"},
                {"id": "acct-w", "email": "carol@example.com", "managedAuthRuntime": "wsl"}
            ],
            "activeAccountId": active,
            "activeAccountIdsByRuntime": {"host": active}
        })
    }

    #[cfg(unix)]
    const SLOT: &str = "/Users/example/.claude.orca";

    #[cfg(unix)]
    fn probe_slot(_pid: u32) -> Option<PathBuf> {
        Some(PathBuf::from(SLOT))
    }

    #[cfg(unix)]
    #[test]
    fn select_that_never_reaches_orca_is_unreachable_not_unknown() {
        // Unreadable runtime metadata (e.g. mid-rewrite): not a refusal.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("orca-runtime.json"), "{").unwrap();
        let got = select_account_in(
            dir.path(),
            "acct-2",
            SLOT,
            Duration::from_secs(1),
            probe_slot,
        );
        assert!(matches!(got, Err(SelectError::Unreachable(_))), "{got:?}");

        // Alive pid, dead socket (a stale runtime file): fails fast.
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("gone.sock");
        let meta = json!({
            "runtimeId": "rt-test",
            "pid": std::process::id(),
            "transports": [{"kind": "unix", "endpoint": sock.to_str().unwrap()}],
            "authToken": "test-token",
        });
        std::fs::write(dir.path().join("orca-runtime.json"), meta.to_string()).unwrap();
        let start = Instant::now();
        let got = select_account_in(
            dir.path(),
            "acct-2",
            SLOT,
            Duration::from_secs(5),
            probe_slot,
        );
        assert!(matches!(got, Err(SelectError::Unreachable(_))), "{got:?}");
        assert!(start.elapsed() < Duration::from_secs(2), "no polling");
    }

    #[cfg(unix)]
    #[test]
    fn select_refuses_when_the_runtime_dir_is_not_the_slot() {
        let dir = tempfile::tempdir().unwrap();
        let h = fake_orca(dir.path(), 0, |_| None);
        let got = select_account_in(dir.path(), "acct-2", SLOT, Duration::from_secs(2), |_| {
            Some(PathBuf::from("/Users/example/.claude.work"))
        });
        assert!(matches!(
            got,
            Err(SelectError::RuntimeDirMismatch { actual: Some(_) })
        ));
        let got = select_account_in(dir.path(), "acct-2", SLOT, Duration::from_secs(2), |_| None);
        assert_eq!(got, Err(SelectError::RuntimeDirMismatch { actual: None }));
        drop(h);
    }

    #[cfg(unix)]
    #[test]
    fn select_happy_path_sends_one_select() {
        let dir = tempfile::tempdir().unwrap();
        let h = fake_orca(dir.path(), 2, |req| match req["method"].as_str() {
            Some("accounts.list") => Some(Ok(json!({"claude": snapshot("acct-1")}))),
            Some("accounts.selectClaude") => Some(Ok(snapshot("acct-2"))),
            _ => Some(Err(("bad", "unexpected method"))),
        });
        let got = select_account_in(
            dir.path(),
            "acct-2",
            SLOT,
            Duration::from_secs(5),
            probe_slot,
        );
        assert_eq!(got, Ok(SelectOutcome::Selected));
        let seen = h.join().unwrap();
        assert_eq!(seen[0]["params"], json!({"refreshUsage": false}));
        assert_eq!(seen[1]["method"], "accounts.selectClaude");
        assert_eq!(seen[1]["params"], json!({"accountId": "acct-2"}));
    }

    #[cfg(unix)]
    #[test]
    fn select_is_skipped_when_already_active_and_refuses_non_host() {
        let dir = tempfile::tempdir().unwrap();
        let h = fake_orca(dir.path(), 1, |_| {
            Some(Ok(json!({"claude": snapshot("acct-2")})))
        });
        let got = select_account_in(
            dir.path(),
            "acct-2",
            SLOT,
            Duration::from_secs(5),
            probe_slot,
        );
        assert_eq!(got, Ok(SelectOutcome::Selected));
        assert_eq!(h.join().unwrap().len(), 1, "no select was sent");

        let dir = tempfile::tempdir().unwrap();
        let h = fake_orca(dir.path(), 1, |_| {
            Some(Ok(json!({"claude": snapshot("acct-1")})))
        });
        let got = select_account_in(
            dir.path(),
            "acct-w",
            SLOT,
            Duration::from_secs(5),
            probe_slot,
        );
        assert!(matches!(got, Err(SelectError::Rejected(_))), "{got:?}");
        h.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn select_while_orca_is_starting_is_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let h = fake_orca(dir.path(), 1, |_| {
            Some(Err((
                "runtime_error",
                "Account services are not configured on this runtime",
            )))
        });
        let got = select_account_in(
            dir.path(),
            "acct-2",
            SLOT,
            Duration::from_secs(5),
            probe_slot,
        );
        assert_eq!(got, Ok(SelectOutcome::Unknown));
        h.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn select_error_frame_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let h = fake_orca(dir.path(), 2, |req| match req["method"].as_str() {
            Some("accounts.list") => Some(Ok(json!({"claude": snapshot("acct-1")}))),
            _ => Some(Err(("select_failed", "materialize failed"))),
        });
        let got = select_account_in(
            dir.path(),
            "acct-2",
            SLOT,
            Duration::from_secs(5),
            probe_slot,
        );
        assert!(
            matches!(got, Err(SelectError::Rejected(ref m)) if m.contains("materialize failed"))
        );
        h.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn slow_select_is_confirmed_by_polling() {
        let dir = tempfile::tempdir().unwrap();
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c = calls.clone();
        let h = fake_orca(dir.path(), 3, move |req| {
            let n = c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            match (n, req["method"].as_str()) {
                (0, _) => Some(Ok(json!({"claude": snapshot("acct-1")}))),
                (_, Some("accounts.selectClaude")) => None, // never answers
                _ => Some(Ok(json!({"claude": snapshot("acct-2")}))),
            }
        });
        let got = select_account_in(
            dir.path(),
            "acct-2",
            SLOT,
            Duration::from_secs(2),
            probe_slot,
        );
        assert_eq!(got, Ok(SelectOutcome::Selected));
        drop(h);
    }
}
