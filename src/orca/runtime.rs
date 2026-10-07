//! The runtime dir `D`: where it is, what identity it holds, which Claude
//! sessions are alive in it, and the materialize that writes an account
//! into it (see the materialize section).
//!
//! Ported from Orca 1.4.209 (F7i, M:247018-247034):
//! - getRuntimePaths: `D` = `CLAUDE_CONFIG_DIR` trimmed, else `~/.claude`;
//!   credentials at `D/.credentials.json`; Orca exports `CLAUDE_CONFIG_DIR`
//!   to its panes only when it had one ([`RuntimePaths::env_set`]).
//! - resolveConfigPath: `D/.claude.json` when `CLAUDE_CONFIG_DIR` was set or
//!   that file exists, else `~/.claude.json`.
//! - readJsonObject (M:247086-247093): a missing file reads as `{}`; an
//!   unreadable, unparseable or non-object file reads as "null" (and Orca
//!   then writes nothing) ([`parse_json_object`]).
//! - readIdentityFromOauthAccount: `accountUuid ?? accountId`,
//!   `emailAddress ?? email`, `organizationUuid ?? organizationId`, each
//!   trimmed, blank as null ([`OauthIdentity`]).
//!
//! `D`'s identity is `oauthAccount.accountUuid` in the runtime `.claude.json`;
//! it names a stash when exactly one host stash's `oauth-account.json`
//! carries the same uuid ([`match_account_uuid`]).
//!
//! Session registry (Claude Code 2.1.283): `D/sessions/<pid>.json` with
//! `pid, sessionId, cwd, startedAt, procStart, kind, entrypoint, pidDomain,
//! status…`. `procStart` is `LC_ALL=C TZ=UTC ps -o lstart=` trimmed on
//! POSIX; Windows writes `procStartFt` (a FILETIME) instead. `pidDomain` is
//! `darwin` on macOS (verified on a live registry); the Linux form
//! `linux:<machine-id>:<pid-ns link>` and the Windows form
//! `win32:<hostname lowercased>` are INFERRED from the bundle. A record is
//! live when its domain is this host's, its pid is alive, and the process
//! start time equals `procStart` (the pid-reuse guard). On Linux Claude Code
//! writes `procStart` as the process start time in clock ticks since boot
//! (all digits, `/proc/<pid>/stat` field 22; seen on a live Linux
//! registry), so an all-digit `procStart` is compared with that field
//! ([`ProcFacts::start_ticks`]) and needs no `ps` string. A record csm cannot
//! verify is [`SessionLiveness::Unverifiable`], which callers treat as
//! possibly live (fail safe: Orca defers its refresh when a session lives).

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::OrcaError;
use super::live::ProcFacts;
use super::record::AccountRecord;
use super::stash::Stash;
use super::userdata::HostOs;

/// Cap on `.claude.json` (it carries per-project history).
const CONFIG_CAP: u64 = 64 * 1024 * 1024;
/// Cap on one session record.
const SESSION_CAP: u64 = 256 * 1024;

// ─── paths ────────────────────────────────────────────────────────────────────

/// Orca's getRuntimePaths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimePaths {
    /// `D`.
    pub config_dir: PathBuf,
    /// `D/.credentials.json`.
    pub credentials_path: PathBuf,
    /// The `.claude.json` Claude Code reads for `D`.
    pub config_path: PathBuf,
    /// `CLAUDE_CONFIG_DIR` was set (Orca exports it to panes only then).
    pub env_set: bool,
}

/// Orca's resolveConfigPath. Pure given `exists`.
pub fn resolve_config_path(
    config_dir: &Path,
    env_set: bool,
    home: &Path,
    exists: impl Fn(&Path) -> bool,
) -> PathBuf {
    let in_dir = config_dir.join(".claude.json");
    if env_set || exists(&in_dir) {
        in_dir
    } else {
        home.join(".claude.json")
    }
}

/// Orca's getRuntimePaths over `CLAUDE_CONFIG_DIR` and the home dir. Pure
/// given `exists`.
pub fn runtime_paths(
    claude_config_dir: Option<&str>,
    home: &Path,
    exists: impl Fn(&Path) -> bool,
) -> RuntimePaths {
    let set = claude_config_dir.map(str::trim).filter(|s| !s.is_empty());
    let config_dir = match set {
        Some(d) => PathBuf::from(d),
        None => home.join(".claude"),
    };
    RuntimePaths {
        credentials_path: config_dir.join(".credentials.json"),
        config_path: resolve_config_path(&config_dir, set.is_some(), home, exists),
        env_set: set.is_some(),
        config_dir,
    }
}

// ─── readJsonObject ───────────────────────────────────────────────────────────

/// readJsonObject over the file's bytes (`None` = missing file). `None`
/// out means Orca's null: unparseable or not an object. Pure.
pub fn parse_json_object(bytes: Option<&[u8]>) -> Option<Map<String, Value>> {
    let Some(bytes) = bytes else {
        return Some(Map::new());
    };
    match serde_json::from_slice::<Value>(bytes) {
        Ok(Value::Object(m)) => Some(m),
        _ => None,
    }
}

/// readJsonObject on disk. An I/O failure other than "missing" is Orca's
/// null too (its readFileSync throw is caught).
pub fn read_json_object(path: &Path) -> Option<Map<String, Value>> {
    match super::read_capped_bytes(path, CONFIG_CAP) {
        Ok(b) => parse_json_object(b.as_deref()),
        Err(_) => None,
    }
}

// ─── identity ─────────────────────────────────────────────────────────────────

/// Orca's readIdentityFromOauthAccount.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OauthIdentity {
    pub account_uuid: Option<String>,
    pub email: Option<String>,
    pub organization_uuid: Option<String>,
}

fn field(v: &Value, primary: &str, fallback: &str) -> Option<String> {
    let s = v
        .get(primary)
        .and_then(Value::as_str)
        .or_else(|| v.get(fallback).and_then(Value::as_str))?;
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_owned())
}

impl OauthIdentity {
    /// Pure. A non-object reads as all-null, as in Orca.
    pub fn from_value(v: &Value) -> OauthIdentity {
        if !v.is_object() {
            return OauthIdentity::default();
        }
        OauthIdentity {
            account_uuid: field(v, "accountUuid", "accountId"),
            email: field(v, "emailAddress", "email"),
            organization_uuid: field(v, "organizationUuid", "organizationId"),
        }
    }
}

/// What the runtime `.claude.json` says about `D`'s account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeIdentity {
    /// The file reads as Orca's null (unparseable or not an object).
    Unreadable,
    /// No `oauthAccount` (the neutral window, or never logged in).
    None,
    Present(OauthIdentity),
}

/// Read `D`'s identity from `paths.config_path`.
pub fn read_runtime_identity(paths: &RuntimePaths) -> RuntimeIdentity {
    match read_json_object(&paths.config_path) {
        None => RuntimeIdentity::Unreadable,
        Some(m) => match m.get("oauthAccount") {
            None | Some(Value::Null) => RuntimeIdentity::None,
            Some(v) => RuntimeIdentity::Present(OauthIdentity::from_value(v)),
        },
    }
}

/// Which account an `accountUuid` names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UuidMatch {
    Unique(String),
    None,
    Ambiguous(Vec<String>),
}

/// Map `uuid` to the one account whose stash identity carries it. Pure over
/// `(account id, stash identity)` pairs.
pub fn match_account_uuid(uuid: &str, stashes: &[(String, Option<OauthIdentity>)]) -> UuidMatch {
    let hits: Vec<String> = stashes
        .iter()
        .filter(|(_, ident)| {
            ident
                .as_ref()
                .and_then(|i| i.account_uuid.as_deref())
                .is_some_and(|u| u == uuid)
        })
        .map(|(id, _)| id.clone())
        .collect();
    match hits.len() {
        0 => UuidMatch::None,
        1 => UuidMatch::Unique(hits.into_iter().next().unwrap_or_default()),
        _ => UuidMatch::Ambiguous(hits),
    }
}

/// The stash identity (`oauth-account.json`) of every host record whose
/// stash passes Q2i. Records that fail are listed with `None`.
pub fn stash_identities(
    user_data: &Path,
    records: &[AccountRecord],
) -> Vec<(String, Option<OauthIdentity>)> {
    records
        .iter()
        .filter(|r| r.is_host())
        .map(|r| {
            let ident = Stash::open(user_data, &r.id, r.managed_auth_path.as_deref())
                .ok()
                .and_then(|s| s.oauth_account().ok().flatten())
                .map(|v| OauthIdentity::from_value(&v));
            (r.id.clone(), ident)
        })
        .collect()
}

/// The stash identity of every stash under `<userData>/claude-accounts/`
/// that no record in `records` names and that passes Q2i (its marker names
/// its dir). On a SQLite-backed profile `orca-data.json` is an export Orca
/// rewrites only when it quits, so an account added while Orca runs has a
/// stash but no record there yet. Read-only; unreadable dirs are skipped.
pub fn unlisted_stash_identities(
    user_data: &Path,
    records: &[AccountRecord],
) -> Vec<(String, Option<OauthIdentity>)> {
    let root = super::userdata::claude_accounts_root(user_data);
    let Ok(rd) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut out: Vec<(String, Option<OauthIdentity>)> = rd
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(str::to_owned))
        .filter(|id| !records.iter().any(|r| &r.id == id))
        .filter_map(|id| {
            let s = Stash::open(user_data, &id, None).ok()?;
            let ident = s
                .oauth_account()
                .ok()
                .flatten()
                .map(|v| OauthIdentity::from_value(&v));
            Some((id, ident))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// `D`'s account: its identity mapped to a stash id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeAccount {
    pub identity: RuntimeIdentity,
    /// `None` when `D` carries no `accountUuid`.
    pub account: Option<UuidMatch>,
}

/// Resolve `D`'s account among `records`.
pub fn runtime_account(
    paths: &RuntimePaths,
    user_data: &Path,
    records: &[AccountRecord],
) -> RuntimeAccount {
    let identity = read_runtime_identity(paths);
    let account = match &identity {
        RuntimeIdentity::Present(i) => i
            .account_uuid
            .as_deref()
            .map(|u| match_account_uuid(u, &stash_identities(user_data, records))),
        _ => None,
    };
    RuntimeAccount { identity, account }
}

// ─── session registry ─────────────────────────────────────────────────────────

/// One `D/sessions/<pid>.json` record (no secrets; `cwd` is kept out).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecord {
    pub pid: u32,
    pub session_id: Option<String>,
    pub proc_start: Option<String>,
    pub proc_start_ft: Option<String>,
    pub pid_domain: Option<String>,
    pub kind: Option<String>,
    pub status: Option<String>,
}

/// `<pid>.json` with a canonical decimal pid (Claude Code's QLo). Pure.
pub fn session_file_pid(name: &str) -> Option<u32> {
    let stem = name.strip_suffix(".json")?;
    let pid: u32 = stem.parse().ok()?;
    (pid.to_string() == stem).then_some(pid)
}

/// Parse one record (ZLo's tolerant reading). Pure.
pub fn parse_session_record(pid: u32, bytes: &[u8]) -> Option<SessionRecord> {
    let v: Value = serde_json::from_slice(bytes).ok()?;
    let obj = v.as_object()?;
    let s = |k: &str| obj.get(k).and_then(Value::as_str).map(str::to_owned);
    Some(SessionRecord {
        pid,
        session_id: s("sessionId"),
        proc_start: s("procStart"),
        proc_start_ft: s("procStartFt"),
        pid_domain: s("pidDomain"),
        kind: s("kind"),
        status: s("status"),
    })
}

/// `ps -o lstart=` in the C locale and UTC (`Sun Sep  6 01:02:03 2026`) as
/// epoch seconds. Pure.
pub fn lstart_epoch(s: &str) -> Option<i64> {
    let norm = s.split_whitespace().collect::<Vec<_>>().join(" ");
    chrono::NaiveDateTime::parse_from_str(&norm, "%a %b %e %H:%M:%S %Y")
        .ok()
        .map(|t| t.and_utc().timestamp())
}

/// `procStart` in Linux's form: all ASCII digits (clock ticks since boot).
/// Pure.
fn ticks_text(s: &str) -> Option<u64> {
    let t = s.trim();
    if t.is_empty() || !t.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    t.parse().ok()
}

/// A Windows FILETIME (100 ns ticks since 1601) as epoch seconds. Pure.
pub fn filetime_epoch(s: &str) -> Option<i64> {
    let ticks: u64 = s.trim().parse().ok()?;
    let secs = (ticks / 10_000_000) as i64;
    Some(secs - 11_644_473_600)
}

/// Claude Code's pidDomain for this host. Pure over its inputs.
pub fn pid_domain_for(
    os: HostOs,
    machine_id: Option<&str>,
    pid_ns: Option<&str>,
    hostname: Option<&str>,
) -> String {
    match os {
        HostOs::MacOs => "darwin".to_owned(),
        HostOs::Linux => format!(
            "linux:{}:{}",
            machine_id.map(str::trim).unwrap_or(""),
            pid_ns.unwrap_or("")
        ),
        HostOs::Windows => format!("win32:{}", hostname.unwrap_or("").to_lowercase()),
    }
}

/// This host's pidDomain.
pub fn this_pid_domain(os: HostOs) -> String {
    let (machine_id, pid_ns) = if os == HostOs::Linux {
        (
            std::fs::read_to_string("/etc/machine-id").ok(),
            std::fs::read_link("/proc/self/ns/pid")
                .ok()
                .map(|p| p.to_string_lossy().into_owned()),
        )
    } else {
        (None, None)
    };
    let host = super::live::hostname();
    pid_domain_for(
        os,
        machine_id.as_deref(),
        pid_ns.as_deref(),
        host.as_deref(),
    )
}

/// A session record's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionLiveness {
    Live,
    Dead,
    /// Another pid domain, or a start time that cannot be read: treat as
    /// possibly live.
    Unverifiable,
}

/// Classify one record. Pure over `facts`.
pub fn classify_session(
    rec: &SessionRecord,
    this_domain: &str,
    facts: &dyn ProcFacts,
) -> SessionLiveness {
    if rec.pid_domain.as_deref().is_some_and(|d| d != this_domain) {
        return SessionLiveness::Unverifiable;
    }
    if rec.pid <= 1 || !facts.alive(rec.pid) {
        return SessionLiveness::Dead;
    }
    // Linux: ticks since boot, compared exactly with `/proc/<pid>/stat`.
    if let Some(ticks) = rec.proc_start.as_deref().and_then(ticks_text) {
        return match facts.start_ticks(rec.pid) {
            None => SessionLiveness::Unverifiable,
            Some(got) if got == ticks => SessionLiveness::Live,
            Some(_) => SessionLiveness::Dead,
        };
    }
    let want = match (&rec.proc_start, &rec.proc_start_ft) {
        (Some(s), _) => lstart_epoch(s),
        (None, Some(ft)) => filetime_epoch(ft),
        // No start recorded: Claude Code's own check accepts the pid.
        (None, None) => return SessionLiveness::Live,
    };
    let Some(want) = want else {
        return SessionLiveness::Unverifiable;
    };
    match facts.start_time(rec.pid) {
        None => SessionLiveness::Unverifiable,
        Some(got) if (got as i64 - want).abs() <= 1 => SessionLiveness::Live,
        Some(_) => SessionLiveness::Dead,
    }
}

/// The live picture of `D/sessions`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionScan {
    pub live: Vec<SessionRecord>,
    pub unverifiable: Vec<SessionRecord>,
    pub dead: usize,
    /// Files that did not parse.
    pub unreadable: usize,
}

impl SessionScan {
    /// Anything that may be a live Claude in `D` (fail safe).
    pub fn may_have_live(&self) -> bool {
        !self.live.is_empty() || !self.unverifiable.is_empty() || self.unreadable > 0
    }
}

/// Scan `dir` (normally `D/sessions`). A missing dir is empty.
pub fn scan_sessions(
    dir: &Path,
    this_domain: &str,
    facts: &dyn ProcFacts,
) -> Result<SessionScan, OrcaError> {
    let mut scan = SessionScan::default();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(scan),
        Err(e) => return Err(OrcaError::io("cannot list", dir, e)),
    };
    for entry in entries {
        let entry = entry.map_err(|e| OrcaError::io("cannot list", dir, e))?;
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(session_file_pid) else {
            continue;
        };
        let rec = super::read_capped_bytes(&entry.path(), SESSION_CAP)
            .ok()
            .flatten()
            .and_then(|b| parse_session_record(pid, &b));
        let Some(rec) = rec else {
            scan.unreadable += 1;
            continue;
        };
        match classify_session(&rec, this_domain, facts) {
            SessionLiveness::Live => scan.live.push(rec),
            SessionLiveness::Unverifiable => scan.unverifiable.push(rec),
            SessionLiveness::Dead => scan.dead += 1,
        }
    }
    scan.live.sort_by_key(|r| r.pid);
    scan.unverifiable.sort_by_key(|r| r.pid);
    Ok(scan)
}

// ─── materialize ──────────────────────────────────────────────────────────────
//
// Put an account's stashed grant and identity into `D` (design section 3
// step 7; Orca's writeRuntimeCredentials, De.l and writeRuntimeOauthAccount,
// M:247050-247105, M:247420-247424):
// - `D/.credentials.json`: exactly the stashed string, mode 0600, skipped
//   (chmod 0600 only) when equal; Orca's fs-utils tmp + rename;
// - macOS: the scoped item for `D`, then the unscoped item when the names
//   differ;
// - the runtime `.claude.json`: `oauthAccount` set to the stash's
//   `oauth-account.json`, or deleted when the stash has none, rewritten as
//   `JSON.stringify(obj, null, 2) + "\n"`, skipped when equal.
//
// Two orders ([`Order`]). With no live claude in `D`, the neutral window:
// clear `oauthAccount` first, write the grant surfaces, then set it, so a
// crash never leaves the new grant beside the old identity. With a live
// claude, Orca's order (grant surfaces, then one `.claude.json` write).
//
// Before the first write csm records a pre-image of every surface
// ([`PreImages`]); a failed step restores them all. `.claude.json` is
// restored at the key level (re-read, then only `oauthAccount` put back),
// so a concurrent Claude Code update to other keys survives. An unreadable
// `.claude.json` refuses before any write (Orca would write the grant and
// skip the identity).

/// Which order [`materialize_checked`] writes in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    /// No live claude in `D`: identity cleared first, set last.
    NeutralWindow,
    /// A live claude in `D`: Orca's order.
    OrcaOrder,
}

/// One write of a materialize.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatStep {
    ClearIdentity,
    WriteFile,
    WriteScoped,
    WriteLegacy,
    SetIdentity,
}

/// The steps of a materialize, in order. `scoped_is_legacy` is true when
/// the scoped service name equals the unscoped one. Pure.
pub fn mat_steps(order: Order, os: HostOs, scoped_is_legacy: bool) -> Vec<MatStep> {
    let mut v = Vec::new();
    if order == Order::NeutralWindow {
        v.push(MatStep::ClearIdentity);
    }
    v.push(MatStep::WriteFile);
    if os == HostOs::MacOs {
        v.push(MatStep::WriteScoped);
        if !scoped_is_legacy {
            v.push(MatStep::WriteLegacy);
        }
    }
    v.push(MatStep::SetIdentity);
    v
}

/// `.claude.json`'s `oauthAccount` before csm touched it.
#[derive(Debug, Clone, PartialEq)]
pub enum ConfigPre {
    /// The file did not exist.
    Missing,
    /// The file existed; the key's value (`None` = no key).
    Key(Option<Value>),
}

/// Every surface of `D` as it was before a materialize.
pub struct PreImages {
    /// `D/.credentials.json`: bytes and mode, `None` when absent.
    pub file: Option<(super::SecretBytes, Option<u32>)>,
    /// The scoped item for `D` (macOS).
    pub scoped: Option<super::SecretString>,
    /// The unscoped item (macOS), when it is a separate item.
    pub legacy: Option<super::SecretString>,
    pub config: ConfigPre,
    os: HostOs,
    scoped_is_legacy: bool,
}

impl std::fmt::Debug for PreImages {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreImages")
            .field("file", &self.file.as_ref().map(|_| "<redacted>"))
            .field("scoped", &self.scoped.as_ref().map(|_| "<redacted>"))
            .field("legacy", &self.legacy.as_ref().map(|_| "<redacted>"))
            .field("config", &matches!(self.config, ConfigPre::Key(Some(_))))
            .finish()
    }
}

/// `D` and the Keychain user a materialize writes for.
#[derive(Debug, Clone, Copy)]
pub struct RuntimeTargetDir<'a> {
    pub os: HostOs,
    pub paths: &'a RuntimePaths,
    pub user: &'a super::keychain::KeychainUser,
}

impl RuntimeTargetDir<'_> {
    fn dir(&self) -> String {
        self.paths.config_dir.to_string_lossy().into_owned()
    }

    fn scoped_service(&self) -> String {
        super::keychain::runtime_service(Some(&self.dir()))
    }

    fn scoped_is_legacy(&self) -> bool {
        self.scoped_service() == super::keychain::RUNTIME_SERVICE
    }
}

fn read_config_pre(path: &Path) -> Result<ConfigPre, OrcaError> {
    let bytes = super::read_capped_bytes(path, CONFIG_CAP)
        .map_err(|e| OrcaError::io("cannot read", path, e))?;
    let Some(bytes) = bytes else {
        return Ok(ConfigPre::Missing);
    };
    match parse_json_object(Some(&bytes)) {
        Some(m) => Ok(ConfigPre::Key(m.get("oauthAccount").cloned())),
        None => Err(OrcaError::Refused(format!(
            "{} is not a JSON object; fix it before switching",
            path.display()
        ))),
    }
}

/// Record the pre-image of every surface. Reads only.
pub fn capture_preimages(t: &RuntimeTargetDir<'_>) -> Result<PreImages, OrcaError> {
    let config = read_config_pre(&t.paths.config_path)?;
    let p = &t.paths.credentials_path;
    let file = super::read_capped_bytes(p, CONFIG_CAP)
        .map_err(|e| OrcaError::io("cannot read", p, e))?
        .map(|b| (super::SecretBytes::new(b), super::fsx::mode_of(p)));
    let (scoped, legacy) = if t.os == HostOs::MacOs {
        let scoped = super::keychain::find_password(&t.scoped_service(), &t.user.acct)?;
        let legacy = if t.scoped_is_legacy() {
            None
        } else {
            super::keychain::find_password(super::keychain::RUNTIME_SERVICE, &t.user.acct)?
        };
        (scoped, legacy)
    } else {
        (None, None)
    };
    Ok(PreImages {
        file,
        scoped,
        legacy,
        config,
        os: t.os,
        scoped_is_legacy: t.scoped_is_legacy(),
    })
}

/// Orca's writeRuntimeCredentials: skip (chmod 0600) when equal.
fn write_creds_file(path: &Path, bytes: &[u8], mode: u32) -> Result<(), OrcaError> {
    let current = super::read_capped_bytes(path, CONFIG_CAP)
        .map_err(|e| OrcaError::io("cannot read", path, e))?;
    if current.as_deref() == Some(bytes) {
        let mut c = current.unwrap_or_default();
        super::zero(&mut c);
        #[cfg(unix)]
        super::fsx::set_mode(path, mode).map_err(|e| OrcaError::io("cannot chmod", path, e))?;
        return Ok(());
    }
    if let Some(mut c) = current {
        super::zero(&mut c);
    }
    if let Some(dir) = path.parent() {
        super::fsx::guard(dir).map_err(|e| OrcaError::io("refusing", dir, e))?;
        std::fs::create_dir_all(dir).map_err(|e| OrcaError::io("cannot create", dir, e))?;
    }
    let opts = super::fsx::WriteOpts {
        mode,
        ..super::fsx::WriteOpts::PRIVATE
    };
    super::fsx::write_atomic(path, bytes, opts).map_err(|e| OrcaError::io("cannot write", path, e))
}

/// Set (`Some`) or delete (`None`) `oauthAccount` in `.claude.json`
/// (writeRuntimeOauthAccount; a missing file reads as `{}`, so Orca creates
/// it, even as `{}` for a delete). `create` false leaves a missing file
/// missing (the neutral-window clear).
fn write_identity(path: &Path, value: Option<&Value>, create: bool) -> Result<(), OrcaError> {
    let bytes = super::read_capped_bytes(path, CONFIG_CAP)
        .map_err(|e| OrcaError::io("cannot read", path, e))?;
    if bytes.is_none() && !create {
        return Ok(());
    }
    let Some(mut m) = parse_json_object(bytes.as_deref()) else {
        return Err(OrcaError::Refused(format!(
            "{} is not a JSON object",
            path.display()
        )));
    };
    match value {
        Some(v) => {
            m.insert("oauthAccount".into(), v.clone());
        }
        None => {
            m.shift_remove("oauthAccount");
        }
    }
    let text = super::jsjson::write_json_text(&Value::Object(m));
    if bytes.as_deref() == Some(text.as_bytes()) {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        super::fsx::guard(dir).map_err(|e| OrcaError::io("refusing", dir, e))?;
        std::fs::create_dir_all(dir).map_err(|e| OrcaError::io("cannot create", dir, e))?;
    }
    super::fsx::write_atomic(path, text.as_bytes(), super::fsx::WriteOpts::PRIVATE)
        .map_err(|e| OrcaError::io("cannot write", path, e))
}

/// Make `D` neutral: delete `oauthAccount` from the runtime `.claude.json`
/// (a missing file stays missing). The recovery fallback of design
/// section 3.
pub fn clear_identity(paths: &RuntimePaths) -> Result<(), OrcaError> {
    write_identity(&paths.config_path, None, false)
}

/// Set (`Some`) or delete (`None`) `oauthAccount` in the runtime
/// `.claude.json`, the way the recovery of a crashed switch from no account
/// puts the system default's identity back. A file that is not a JSON
/// object is left alone (Orca's restoreRuntimeOauthAccountIfOwned returns
/// on a parse error); a missing file is created only to set a value.
pub fn restore_identity(paths: &RuntimePaths, value: Option<&Value>) -> Result<(), OrcaError> {
    let p = &paths.config_path;
    let bytes =
        super::read_capped_bytes(p, CONFIG_CAP).map_err(|e| OrcaError::io("cannot read", p, e))?;
    if bytes.is_some() && parse_json_object(bytes.as_deref()).is_none() {
        return Ok(());
    }
    write_identity(p, value, value.is_some())
}

/// Orca's restoreRuntimeCredentials: write `creds` to `D/.credentials.json`
/// (0600, skipped when equal), or delete the file for `None`.
pub fn restore_credentials_file(
    paths: &RuntimePaths,
    creds: Option<&str>,
) -> Result<(), OrcaError> {
    let c = &paths.credentials_path;
    match creds {
        Some(v) => write_creds_file(c, v.as_bytes(), 0o600),
        None => super::fsx::remove_file(c)
            .map(|_| ())
            .map_err(|e| OrcaError::io("cannot remove", c, e)),
    }
}

fn restore_item(svc: &str, acct: &str, pre: Option<&super::SecretString>) -> Result<(), OrcaError> {
    match pre {
        Some(v) => super::keychain::add_password(svc, acct, v.expose())?,
        None => {
            super::keychain::delete_password(svc, acct)?;
        }
    }
    Ok(())
}

/// Put every surface back as `pre` recorded it. Every surface is tried;
/// the failures are returned (names only).
pub fn restore_preimages(t: &RuntimeTargetDir<'_>, pre: &PreImages) -> Result<(), Vec<String>> {
    let mut failed = Vec::new();
    let p = &t.paths.config_path;
    let r = match &pre.config {
        ConfigPre::Missing => super::fsx::remove_file(p)
            .map(|_| ())
            .map_err(|e| OrcaError::io("cannot remove", p, e)),
        ConfigPre::Key(v) => write_identity(p, v.as_ref(), true),
    };
    if r.is_err() {
        failed.push(p.display().to_string());
    }
    if pre.os == HostOs::MacOs {
        let acct = &t.user.acct;
        if restore_item(&t.scoped_service(), acct, pre.scoped.as_ref()).is_err() {
            failed.push(t.scoped_service());
        }
        if !pre.scoped_is_legacy
            && restore_item(super::keychain::RUNTIME_SERVICE, acct, pre.legacy.as_ref()).is_err()
        {
            failed.push(super::keychain::RUNTIME_SERVICE.to_owned());
        }
    }
    let c = &t.paths.credentials_path;
    let r = match &pre.file {
        None => super::fsx::remove_file(c)
            .map(|_| ())
            .map_err(|e| OrcaError::io("cannot remove", c, e)),
        Some((b, mode)) => write_creds_file(c, b.expose(), mode.unwrap_or(0o600)),
    };
    if r.is_err() {
        failed.push(c.display().to_string());
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(failed)
    }
}

/// What [`materialize_checked`] did.
#[derive(Debug)]
pub struct Materialized {
    /// Restore these to undo the materialize (step 8's rollback).
    pub pre: PreImages,
    /// The steps written, in order (the tests check which surfaces a
    /// materialize touched).
    #[cfg_attr(not(test), allow(dead_code, reason = "read by the tests only"))]
    pub steps: Vec<MatStep>,
}

/// A failed [`materialize_checked`].
#[derive(Debug)]
pub struct MaterializeFailure {
    pub error: OrcaError,
    /// Every surface is as it was before (nothing was written, or the
    /// restore succeeded). `false`: `D` may be half written.
    pub d_restored: bool,
}

/// Materialize `creds` (the exact stashed string) and `oauth` (the stash's
/// `oauth-account.json`, `None` to delete the key) into `D`. On a failed
/// step every surface is restored and the error names what failed.
#[cfg(test)]
pub fn materialize(
    t: &RuntimeTargetDir<'_>,
    creds: &str,
    oauth: Option<&Value>,
    order: Order,
) -> Result<Materialized, OrcaError> {
    materialize_checked(t, creds, oauth, order).map_err(|f| f.error)
}

/// Materialize `creds` and `oauth` into `D` (see the section notes above),
/// telling a failure that left `D` restored from one that could not
/// restore it (the switch journal must stay pending then).
pub fn materialize_checked(
    t: &RuntimeTargetDir<'_>,
    creds: &str,
    oauth: Option<&Value>,
    order: Order,
) -> Result<Materialized, MaterializeFailure> {
    let pre = capture_preimages(t).map_err(|error| MaterializeFailure {
        error,
        d_restored: true,
    })?;
    let steps = mat_steps(order, t.os, t.scoped_is_legacy());
    let dir = t.dir();
    let mut done = Vec::new();
    for step in &steps {
        let r = match step {
            MatStep::ClearIdentity => write_identity(&t.paths.config_path, None, false),
            MatStep::WriteFile => {
                write_creds_file(&t.paths.credentials_path, creds.as_bytes(), 0o600)
            }
            MatStep::WriteScoped => {
                super::keychain::write_runtime_scoped(creds, Some(&dir), t.user).map_err(Into::into)
            }
            MatStep::WriteLegacy => {
                super::keychain::add_password(super::keychain::RUNTIME_SERVICE, &t.user.acct, creds)
                    .map_err(Into::into)
            }
            MatStep::SetIdentity => write_identity(&t.paths.config_path, oauth, true),
        };
        if let Err(e) = r {
            return Err(match restore_preimages(t, &pre) {
                Ok(()) => MaterializeFailure {
                    error: OrcaError::Refused(format!(
                        "materialize failed at {step:?} ({e}); D was restored"
                    )),
                    d_restored: true,
                },
                Err(f) => MaterializeFailure {
                    error: OrcaError::Refused(format!(
                        "materialize failed at {step:?} ({e}); restoring also failed for: {}",
                        f.join(", ")
                    )),
                    d_restored: false,
                },
            });
        }
        done.push(*step);
    }
    Ok(Materialized { pre, steps: done })
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orca::testsupport::{FakeProcs, make_stash, proc_info};
    use serde_json::json;

    const HOME: &str = "/Users/example";

    #[test]
    fn resolve_config_path_four_cases() {
        let home = Path::new(HOME);
        let d = Path::new("/Users/example/.claude");
        let yes = |_: &Path| true;
        let no = |_: &Path| false;
        // env set, file exists / absent → D/.claude.json either way.
        assert_eq!(
            resolve_config_path(d, true, home, yes),
            d.join(".claude.json")
        );
        assert_eq!(
            resolve_config_path(d, true, home, no),
            d.join(".claude.json")
        );
        // env unset: D/.claude.json only if it exists, else ~/.claude.json.
        assert_eq!(
            resolve_config_path(d, false, home, yes),
            d.join(".claude.json")
        );
        assert_eq!(
            resolve_config_path(d, false, home, no),
            PathBuf::from("/Users/example/.claude.json")
        );
    }

    #[test]
    fn runtime_paths_trim_and_default() {
        let home = Path::new(HOME);
        let p = runtime_paths(Some("  /Users/example/.claude.work  "), home, |_| false);
        assert_eq!(p.config_dir, PathBuf::from("/Users/example/.claude.work"));
        assert_eq!(
            p.credentials_path,
            PathBuf::from("/Users/example/.claude.work/.credentials.json")
        );
        assert_eq!(
            p.config_path,
            PathBuf::from("/Users/example/.claude.work/.claude.json")
        );
        assert!(p.env_set);
        for blank in [None, Some(""), Some("   ")] {
            let p = runtime_paths(blank, home, |_| false);
            assert_eq!(p.config_dir, PathBuf::from("/Users/example/.claude"));
            assert_eq!(p.config_path, PathBuf::from("/Users/example/.claude.json"));
            assert!(!p.env_set);
        }
    }

    #[test]
    fn read_json_object_semantics() {
        assert_eq!(
            parse_json_object(None),
            Some(Map::new()),
            "missing reads as {{}}"
        );
        assert_eq!(parse_json_object(Some(b"{bad")), None);
        assert_eq!(parse_json_object(Some(b"[]")), None);
        assert_eq!(parse_json_object(Some(b"null")), None);
        assert_eq!(parse_json_object(Some(b"")), None);
        assert_eq!(parse_json_object(Some(b"{\"a\":1}")).unwrap()["a"], 1);

        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            read_json_object(&dir.path().join("absent")),
            Some(Map::new())
        );
        assert_eq!(read_json_object(dir.path()), None, "a dir is unreadable");
    }

    #[test]
    fn oauth_identity_fallbacks() {
        let i = OauthIdentity::from_value(&json!({
            "accountId": " u-1 ", "email": "alice@example.com", "organizationId": "  "
        }));
        assert_eq!(i.account_uuid.as_deref(), Some("u-1"));
        assert_eq!(i.email.as_deref(), Some("alice@example.com"));
        assert_eq!(i.organization_uuid, None);
        let i = OauthIdentity::from_value(&json!({
            "accountUuid": "u-2", "accountId": "ignored", "emailAddress": "bob@example.com"
        }));
        assert_eq!(i.account_uuid.as_deref(), Some("u-2"));
        assert_eq!(i.email.as_deref(), Some("bob@example.com"));
        assert_eq!(
            OauthIdentity::from_value(&json!("x")),
            OauthIdentity::default()
        );
    }

    #[test]
    fn uuid_matching() {
        let id = |u: &str| {
            Some(OauthIdentity {
                account_uuid: Some(u.into()),
                ..OauthIdentity::default()
            })
        };
        let stashes = vec![
            ("id-a".to_owned(), id("u-a")),
            ("id-b".to_owned(), id("u-b")),
            ("id-c".to_owned(), None),
            ("id-d".to_owned(), id("u-b")),
        ];
        assert_eq!(
            match_account_uuid("u-a", &stashes),
            UuidMatch::Unique("id-a".into())
        );
        assert_eq!(match_account_uuid("u-x", &stashes), UuidMatch::None);
        assert_eq!(
            match_account_uuid("u-b", &stashes),
            UuidMatch::Ambiguous(vec!["id-b".into(), "id-d".into()])
        );
    }

    fn record(id: &str) -> AccountRecord {
        AccountRecord::from_value(&json!({"id": id, "email": "alice@example.com"})).unwrap()
    }

    #[test]
    fn runtime_account_maps_d_to_a_stash() {
        let dir = tempfile::tempdir().unwrap();
        let ud = dir.path().join("orca");
        let d = dir.path().join("claude");
        std::fs::create_dir_all(&d).unwrap();
        make_stash(&ud, "id-a", Some(br#"{"accountUuid":"u-a"}"#), None);
        make_stash(&ud, "id-b", Some(br#"{"accountUuid":"u-b"}"#), None);
        let records = vec![record("id-a"), record("id-b"), record("id-gone")];
        let paths = runtime_paths(Some(d.to_str().unwrap()), dir.path(), |p| p.exists());

        // No .claude.json yet: {} → no oauthAccount.
        let r = runtime_account(&paths, &ud, &records);
        assert_eq!(r.identity, RuntimeIdentity::None);
        assert_eq!(r.account, None);

        std::fs::write(&paths.config_path, r#"{"oauthAccount":{"accountUuid":"u-b","emailAddress":"bob@example.com"},"projects":{}}"#).unwrap();
        let r = runtime_account(&paths, &ud, &records);
        assert_eq!(r.account, Some(UuidMatch::Unique("id-b".into())));

        std::fs::write(&paths.config_path, "{broken").unwrap();
        assert_eq!(
            runtime_account(&paths, &ud, &records).identity,
            RuntimeIdentity::Unreadable
        );
    }

    #[test]
    fn session_file_names() {
        assert_eq!(session_file_pid("123.json"), Some(123));
        assert_eq!(session_file_pid("0123.json"), None);
        assert_eq!(session_file_pid("123.abcd.key"), None);
        assert_eq!(session_file_pid("x.json"), None);
        assert_eq!(session_file_pid("123.json.tmp"), None);
    }

    #[test]
    fn lstart_and_filetime_parse() {
        assert_eq!(lstart_epoch("Thu Jan  1 00:00:10 1970"), Some(10));
        assert_eq!(
            lstart_epoch("Sat Sep 26 12:00:00 2026"),
            lstart_epoch(" Sat  Sep 26 12:00:00 2026 ")
        );
        assert!(lstart_epoch("Sat Sep 26 12:00:00 2026").is_some());
        assert!(lstart_epoch("Sun Sep  6 01:02:03 2026").is_some());
        assert_eq!(lstart_epoch("garbage"), None);
        assert_eq!(filetime_epoch("116444736100000000"), Some(10));
        assert_eq!(filetime_epoch("x"), None);
    }

    #[test]
    fn pid_domains() {
        assert_eq!(pid_domain_for(HostOs::MacOs, None, None, None), "darwin");
        assert_eq!(
            pid_domain_for(HostOs::Linux, Some("abc\n"), Some("pid:[4026531836]"), None),
            "linux:abc:pid:[4026531836]"
        );
        assert_eq!(
            pid_domain_for(HostOs::Windows, None, None, Some("HOST-A")),
            "win32:host-a"
        );
    }

    fn rec(pid: u32, start: Option<&str>, domain: Option<&str>) -> SessionRecord {
        SessionRecord {
            pid,
            session_id: Some("s".into()),
            proc_start: start.map(str::to_owned),
            proc_start_ft: None,
            pid_domain: domain.map(str::to_owned),
            kind: Some("interactive".into()),
            status: None,
        }
    }

    #[test]
    fn a_windows_record_of_this_host_with_a_dead_pid_is_dead() {
        // Claude Code writes `win32:<os.hostname() lowercased>`.
        let d = pid_domain_for(HostOs::Windows, None, None, Some("Acme-PC"));
        assert_eq!(d, "win32:acme-pc");
        let facts = FakeProcs::default();
        assert_eq!(
            classify_session(&rec(4321, None, Some("win32:acme-pc")), &d, &facts),
            SessionLiveness::Dead
        );
        // Another host's record stays unverifiable.
        assert_eq!(
            classify_session(&rec(4321, None, Some("win32:other")), &d, &facts),
            SessionLiveness::Unverifiable
        );
    }

    /// The real host name is read on Windows, so this host's domain is never
    /// the empty `win32:` that matched no record.
    #[cfg(windows)]
    #[test]
    fn this_windows_pid_domain_names_the_host() {
        let d = this_pid_domain(HostOs::Windows);
        let host = d.strip_prefix("win32:").unwrap();
        assert!(!host.is_empty());
        assert_eq!(host, host.to_lowercase());
        let facts = FakeProcs::default();
        assert_eq!(
            classify_session(&rec(4321, None, Some(&d)), &d, &facts),
            SessionLiveness::Dead
        );
    }

    #[test]
    fn session_liveness_with_the_pid_reuse_guard() {
        let start = "Thu Jan  1 00:16:40 1970"; // 1000
        let mut p = proc_info(500, "claude", Some("/usr/local/bin/claude"), &[]);
        p.start_time = 1000;
        let facts = FakeProcs::default().with(p);
        let d = "darwin";
        assert_eq!(
            classify_session(&rec(500, Some(start), Some(d)), d, &facts),
            SessionLiveness::Live
        );
        assert_eq!(
            classify_session(&rec(500, Some(start), None), d, &facts),
            SessionLiveness::Live
        );
        assert_eq!(
            classify_session(&rec(500, None, Some(d)), d, &facts),
            SessionLiveness::Live
        );
        // pid reused by a process that started later.
        assert_eq!(
            classify_session(
                &rec(500, Some("Thu Jan  1 01:00:00 1970"), Some(d)),
                d,
                &facts
            ),
            SessionLiveness::Dead
        );
        assert_eq!(
            classify_session(&rec(501, Some(start), Some(d)), d, &facts),
            SessionLiveness::Dead
        );
        assert_eq!(
            classify_session(&rec(1, None, Some(d)), d, &facts),
            SessionLiveness::Dead
        );
        assert_eq!(
            classify_session(&rec(500, Some(start), Some("linux:x:y")), d, &facts),
            SessionLiveness::Unverifiable
        );
        assert_eq!(
            classify_session(&rec(500, Some("garbage"), Some(d)), d, &facts),
            SessionLiveness::Unverifiable
        );
        let blind = FakeProcs::default().alive(500);
        assert_eq!(
            classify_session(&rec(500, Some(start), Some(d)), d, &blind),
            SessionLiveness::Unverifiable
        );
    }

    #[test]
    fn a_linux_record_in_clock_ticks_is_compared_with_the_proc_start_ticks() {
        let d = "linux:m:pid:[1]";
        let p = proc_info(500, "claude", None, &[]);
        let facts = FakeProcs::default().with(p).with_ticks(500, 1453);
        assert_eq!(
            classify_session(&rec(500, Some("1453"), Some(d)), d, &facts),
            SessionLiveness::Live
        );
        // The pid was reused after a reboot: a different start tick.
        assert_eq!(
            classify_session(&rec(500, Some("98765"), Some(d)), d, &facts),
            SessionLiveness::Dead
        );
        // A dead pid stays dead.
        assert_eq!(
            classify_session(&rec(501, Some("1453"), Some(d)), d, &facts),
            SessionLiveness::Dead
        );
        // No ticks source (not Linux, or /proc unreadable): fail safe.
        let blind = FakeProcs::default().with(proc_info(500, "claude", None, &[]));
        assert_eq!(
            classify_session(&rec(500, Some("1453"), Some(d)), d, &blind),
            SessionLiveness::Unverifiable
        );
        // A stale record behind a reused pid no longer blocks retire.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("500.json"),
            r#"{"pid":500,"procStart":"98765","pidDomain":"linux:m:pid:[1]"}"#,
        )
        .unwrap();
        assert!(
            !scan_sessions(dir.path(), d, &facts)
                .unwrap()
                .may_have_live()
        );
    }

    #[test]
    fn scan_sessions_reads_the_registry() {
        let dir = tempfile::tempdir().unwrap();
        let s = dir.path().join("sessions");
        assert_eq!(
            scan_sessions(&s, "darwin", &FakeProcs::default()).unwrap(),
            SessionScan::default()
        );
        std::fs::create_dir_all(&s).unwrap();
        let mut p = proc_info(500, "claude", None, &[]);
        p.start_time = 1000;
        let facts = FakeProcs::default().with(p);
        std::fs::write(s.join("500.json"), r#"{"pid":500,"sessionId":"s1","cwd":"/Users/example/src","procStart":"Thu Jan  1 00:16:40 1970","pidDomain":"darwin","kind":"interactive"}"#).unwrap();
        std::fs::write(
            s.join("501.json"),
            r#"{"pid":501,"procStart":"Thu Jan  1 00:16:40 1970","pidDomain":"darwin"}"#,
        )
        .unwrap();
        std::fs::write(s.join("502.json"), "{broken").unwrap();
        std::fs::write(s.join("500.0123.key"), "{}").unwrap();
        let scan = scan_sessions(&s, "darwin", &facts).unwrap();
        assert_eq!(scan.live.len(), 1);
        assert_eq!(scan.live[0].session_id.as_deref(), Some("s1"));
        assert_eq!((scan.dead, scan.unreadable), (1, 1));
        assert!(scan.may_have_live());
        std::fs::remove_file(s.join("500.json")).unwrap();
        std::fs::remove_file(s.join("502.json")).unwrap();
        assert!(!scan_sessions(&s, "darwin", &facts).unwrap().may_have_live());
    }

    // ─── materialize ──────────────────────────────────────────────────────────

    #[test]
    fn materialize_step_orders() {
        use MatStep::*;
        assert_eq!(
            mat_steps(Order::NeutralWindow, HostOs::MacOs, false),
            vec![
                ClearIdentity,
                WriteFile,
                WriteScoped,
                WriteLegacy,
                SetIdentity
            ]
        );
        assert_eq!(
            mat_steps(Order::OrcaOrder, HostOs::MacOs, true),
            vec![WriteFile, WriteScoped, SetIdentity]
        );
        assert_eq!(
            mat_steps(Order::OrcaOrder, HostOs::Linux, false),
            vec![WriteFile, SetIdentity]
        );
        assert_eq!(
            mat_steps(Order::NeutralWindow, HostOs::Windows, false),
            vec![ClearIdentity, WriteFile, SetIdentity]
        );
    }

    struct MatWorld {
        _tmp: tempfile::TempDir,
        paths: RuntimePaths,
        user: crate::orca::keychain::KeychainUser,
    }

    fn mat_world() -> MatWorld {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("D");
        std::fs::create_dir_all(&d).unwrap();
        let paths = runtime_paths(Some(d.to_str().unwrap()), tmp.path(), |p| p.exists());
        MatWorld {
            _tmp: tmp,
            paths,
            user: crate::orca::keychain::KeychainUser {
                acct: "tester".into(),
                delete_accts: vec!["tester".into()],
            },
        }
    }

    #[test]
    fn materialize_writes_exact_bytes_and_keeps_other_keys() {
        let w = mat_world();
        let t = RuntimeTargetDir {
            os: HostOs::Linux,
            paths: &w.paths,
            user: &w.user,
        };
        std::fs::write(&w.paths.credentials_path, "OLD").unwrap();
        std::fs::write(
            &w.paths.config_path,
            r#"{"numStartups":3,"oauthAccount":{"accountUuid":"u-old"},"projects":{"/Users/example/p":{"x":1.0}}}"#,
        )
        .unwrap();
        let creds = r#"{"claudeAiOauth":{"accessToken":"at","expiresAt":1.50}}"#;
        let oauth = json!({"accountUuid":"u-new","emailAddress":"alice@example.com"});
        let m = materialize(&t, creds, Some(&oauth), Order::NeutralWindow).unwrap();
        assert_eq!(m.steps.len(), 3);
        assert_eq!(
            std::fs::read(&w.paths.credentials_path).unwrap(),
            creds.as_bytes()
        );
        let cfg = std::fs::read_to_string(&w.paths.config_path).unwrap();
        assert_eq!(
            cfg,
            // The neutral window deletes the key first, so it comes back
            // last, as a JS delete + assign would place it.
            "{\n  \"numStartups\": 3,\n  \"projects\": {\n    \"/Users/example/p\": {\n      \"x\": 1\n    }\n  },\n  \"oauthAccount\": {\n    \"accountUuid\": \"u-new\",\n    \"emailAddress\": \"alice@example.com\"\n  }\n}\n"
        );
        #[cfg(unix)]
        {
            assert_eq!(
                crate::orca::fsx::mode_of(&w.paths.credentials_path),
                Some(0o600)
            );
            assert_eq!(crate::orca::fsx::mode_of(&w.paths.config_path), Some(0o600));
        }
        // Undo puts both back (the key only, the rest as now).
        restore_preimages(&t, &m.pre).unwrap();
        assert_eq!(std::fs::read(&w.paths.credentials_path).unwrap(), b"OLD");
        let back: Value =
            serde_json::from_slice(&std::fs::read(&w.paths.config_path).unwrap()).unwrap();
        assert_eq!(back["oauthAccount"], json!({"accountUuid":"u-old"}));
        // A stash without oauth-account.json deletes the key.
        materialize(&t, creds, None, Order::OrcaOrder).unwrap();
        let v: Value =
            serde_json::from_slice(&std::fs::read(&w.paths.config_path).unwrap()).unwrap();
        assert!(v.get("oauthAccount").is_none() && v.get("numStartups").is_some());
        // Equal bytes are not rewritten, only chmod-ed.
        #[cfg(unix)]
        {
            crate::orca::fsx::set_mode(&w.paths.credentials_path, 0o644).unwrap();
            materialize(&t, creds, None, Order::OrcaOrder).unwrap();
            assert_eq!(
                crate::orca::fsx::mode_of(&w.paths.credentials_path),
                Some(0o600)
            );
        }
    }

    #[test]
    fn materialize_creates_missing_files_and_refuses_an_unreadable_config() {
        let w = mat_world();
        let t = RuntimeTargetDir {
            os: HostOs::Linux,
            paths: &w.paths,
            user: &w.user,
        };
        let m = materialize(
            &t,
            "{\"claudeAiOauth\":{\"accessToken\":\"a\"}}",
            None,
            Order::NeutralWindow,
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&w.paths.config_path).unwrap(),
            "{}\n"
        );
        restore_preimages(&t, &m.pre).unwrap();
        assert!(!w.paths.config_path.exists() && !w.paths.credentials_path.exists());
        std::fs::write(&w.paths.config_path, "not json").unwrap();
        assert!(matches!(
            materialize(&t, "{}", None, Order::NeutralWindow),
            Err(OrcaError::Refused(_))
        ));
        assert!(!w.paths.credentials_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_keychain_write_restores_every_surface() {
        use crate::orca::keychain;
        let fake = crate::orca::testsupport::FakeSecurity::install();
        let w = mat_world();
        let t = RuntimeTargetDir {
            os: HostOs::MacOs,
            paths: &w.paths,
            user: &w.user,
        };
        let dir = w.paths.config_dir.to_str().unwrap().to_owned();
        keychain::write_runtime_scoped("OLD-S", Some(&dir), &w.user).unwrap();
        std::fs::write(&w.paths.credentials_path, "OLD").unwrap();
        std::fs::write(
            &w.paths.config_path,
            r#"{"oauthAccount":{"accountUuid":"u-old"}}"#,
        )
        .unwrap();
        let creds = "{\"claudeAiOauth\":{\"accessToken\":\"a\"}}";
        // Success first: both items hold the new grant.
        let m = materialize(
            &t,
            creds,
            Some(&json!({"accountUuid":"u-new"})),
            Order::NeutralWindow,
        )
        .unwrap();
        assert_eq!(m.steps.len(), 5);
        let scoped = keychain::runtime_service(Some(&dir));
        assert_eq!(
            fake.get(&scoped, "tester").as_deref(),
            Some(creds.as_bytes())
        );
        assert_eq!(
            fake.get(keychain::RUNTIME_SERVICE, "tester").as_deref(),
            Some(creds.as_bytes())
        );
        restore_preimages(&t, &m.pre).unwrap();
        assert_eq!(fake.get(&scoped, "tester").as_deref(), Some(&b"OLD-S"[..]));
        assert_eq!(fake.get(keychain::RUNTIME_SERVICE, "tester"), None);
        // The unscoped write fails: everything goes back.
        fake.fail_add(keychain::RUNTIME_SERVICE, true);
        let err = materialize(
            &t,
            creds,
            Some(&json!({"accountUuid":"u-new"})),
            Order::NeutralWindow,
        )
        .unwrap_err();
        assert!(err.to_string().contains("restored"), "{err}");
        assert!(!err.to_string().contains("accessToken"));
        assert_eq!(std::fs::read(&w.paths.credentials_path).unwrap(), b"OLD");
        assert_eq!(fake.get(&scoped, "tester").as_deref(), Some(&b"OLD-S"[..]));
        let back: Value =
            serde_json::from_slice(&std::fs::read(&w.paths.config_path).unwrap()).unwrap();
        assert_eq!(back["oauthAccount"], json!({"accountUuid":"u-old"}));
    }
}
