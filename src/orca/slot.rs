//! The Orca slot: the registered csm profile whose dir is Orca's runtime dir.
//!
//! Orca mode is ON ⇔ `config.json` `orca.slotProfile` is set AND names a
//! registered profile ([`resolve`]); set-but-unregistered is OFF (with a
//! warning from the interactive commands). [`Slot::is_profile`] /
//! [`Slot::is_dir`] (and the free [`is_slot_dir`] for a caller with no
//! loaded registry) are the ONE exclusion helper every caller routes
//! through: the slot is never an account of its own (no usage row, no
//! scoring, never a launch fallback).
//!
//! A `config.json` that exists but does not parse makes Orca mode unknown.
//! The silent loaders ([`current`], [`for_registry`], [`config_and_slot`])
//! read that as OFF so the hook and statusline keep working; the callers
//! that could hurt the slot fail closed instead: registry edits refuse
//! ([`slot_for_guard`]), the floor writers refuse, the opt-in OAuth refresh
//! is skipped and `csm run` warns ([`config_unreadable`]).
//!
//! Also here: the `csm orca status` diagnosis — a pure [`diagnose`] over a
//! [`DiagnosisInput`] that the thin [`gather`] shell fills from disk, the
//! process table, the launchd floor, and (read-only) Orca's RPC.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::Serialize;

use super::bind::Bindings;
use super::pending::{PendingSelect, Verdict};
use super::{HostOs, Identity, OrcaState};
use crate::account::ProfileMap;
use crate::cas::platform::dirs_equal;
use crate::config::Config;

// ─── slot resolution ──────────────────────────────────────────────────────────

/// The slot profile and its registered dir.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Slot {
    pub name: String,
    pub dir: String,
}

impl Slot {
    /// Is `name` the slot profile?
    pub fn is_profile(&self, name: &str) -> bool {
        self.name == name
    }

    /// Is `dir` the slot's dir (normalized; case-insensitive on macOS)?
    pub fn is_dir(&self, dir: &str) -> bool {
        dirs_equal(&self.dir, dir)
    }

    /// [`Self::is_dir`] for a path.
    pub fn is_path(&self, dir: &Path) -> bool {
        dir.to_str().is_some_and(|d| self.is_dir(d))
    }
}

/// Where Orca mode stands for a given config + registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotResolution {
    /// `orca.slotProfile` unset.
    Off,
    /// Orca mode ON.
    On(Slot),
    /// `orca.slotProfile` names a profile that is not registered (mode OFF).
    Unregistered(String),
}

/// Resolve the slot from `config` + `profiles`. Pure.
pub fn resolve(config: &Config, profiles: &ProfileMap) -> SlotResolution {
    let Some(name) = config
        .orca()
        .slot_profile
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return SlotResolution::Off;
    };
    match profiles.get(name) {
        Some(dir) => SlotResolution::On(Slot {
            name: name.to_owned(),
            dir: dir.to_owned(),
        }),
        None => SlotResolution::Unregistered(name.to_owned()),
    }
}

/// The slot while Orca mode is ON, else `None`. The one "Orca mode ON"
/// predicate ([`Config::orca_mode_on`] delegates here).
pub fn active_slot(config: &Config, profiles: &ProfileMap) -> Option<Slot> {
    match resolve(config, profiles) {
        SlotResolution::On(s) => Some(s),
        _ => None,
    }
}

/// Print, once per process, the "slotProfile set but unregistered" warning.
/// Interactive commands only — the hook and statusline must stay silent.
pub fn warn_if_unregistered(config: &Config, profiles: &ProfileMap) {
    static WARNED: AtomicBool = AtomicBool::new(false);
    if let SlotResolution::Unregistered(name) = resolve(config, profiles)
        && !WARNED.swap(true, Ordering::Relaxed)
    {
        eprintln!(
            "csm: warning: orca.slotProfile is \"{name}\" but no such profile is registered; \
             Orca mode is off (run `csm orca init` or `csm orca disable`)"
        );
    }
}

/// The slot, loading the registry and config from disk. Silent: an
/// unreadable registry or config reads as Orca mode OFF. Callers that check
/// many profiles call this once and use [`Slot::is_profile`].
pub fn current() -> Option<Slot> {
    if !disk_state_readable() {
        return None;
    }
    let profiles = ProfileMap::load().ok()?;
    let config = Config::load().ok()?;
    active_slot(&config, &profiles)
}

/// The slot for an already-loaded registry, loading only `config.json`.
/// Silent: an unreadable config reads as Orca mode OFF, so the launch path,
/// the hook and the statusline keep their pre-Orca behaviour on a broken
/// config.
pub fn for_registry(profiles: &ProfileMap) -> Option<Slot> {
    if !disk_state_readable() {
        return None;
    }
    let config = Config::load().ok()?;
    active_slot(&config, profiles)
}

/// The loaded `config.json` plus the slot for `profiles`, or `None` when
/// Orca mode is OFF or the config is unreadable. Silent.
pub fn config_and_slot(profiles: &ProfileMap) -> Option<(Config, Slot)> {
    if !disk_state_readable() {
        return None;
    }
    let config = Config::load().ok()?;
    let slot = active_slot(&config, profiles)?;
    Some((config, slot))
}

/// Is `dir` the Orca slot's dir (Orca mode ON)? Loads config.
pub fn is_slot_dir(dir: &str) -> bool {
    current().is_some_and(|s| s.is_dir(dir))
}

/// Whether the silent loaders may read `config.json` / `profiles.json`.
/// Always true in a real build. Under `cfg(test)` a test that has not pinned
/// a temp HOME reads as Orca mode OFF instead of falling through to the
/// developer's real config (the same rule as `cas::platform::write_floor_file`).
fn disk_state_readable() -> bool {
    #[cfg(test)]
    {
        crate::testenv::test_home().is_some()
    }
    #[cfg(not(test))]
    {
        true
    }
}

// ─── registry-edit guard ──────────────────────────────────────────────────────

/// A registry mutation, as the guard sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistryEdit<'a> {
    /// Register a new profile at `dir`.
    Add { name: &'a str, dir: &'a str },
    /// Point an existing (or new) profile `name` at `dir` (`set`, editor edit-dir).
    Repoint { name: &'a str, dir: &'a str },
    /// Rename profile `from`.
    Rename { from: &'a str },
    /// Unregister profile `name`.
    Remove { name: &'a str },
}

/// Why `edit` is refused while Orca mode is ON with `slot`, or `None` when
/// it is allowed. Pure. The slot can only be changed through `csm orca
/// init`/`disable`, and no other profile may share its dir (Orca would
/// overwrite that profile's credentials on every switch).
pub fn registry_edit_refusal(edit: RegistryEdit<'_>, slot: &Slot) -> Option<String> {
    let slot_msg = |verb: &str| {
        format!(
            "{verb}: '{}' is the Orca slot; run `csm orca disable` first",
            slot.name
        )
    };
    let dir_msg = |name: &str, dir: &str| {
        format!(
            "'{name}' → {dir}: that is the Orca slot's dir ('{}'); a second profile there \
             would have its credentials overwritten by Orca",
            slot.name
        )
    };
    match edit {
        RegistryEdit::Remove { name } if slot.is_profile(name) => Some(slot_msg("remove")),
        RegistryEdit::Rename { from } if slot.is_profile(from) => Some(slot_msg("rename")),
        RegistryEdit::Repoint { name, .. } if slot.is_profile(name) => Some(slot_msg("set")),
        RegistryEdit::Add { name, dir } | RegistryEdit::Repoint { name, dir }
            if slot.is_dir(dir) =>
        {
            Some(dir_msg(name, dir))
        }
        _ => None,
    }
}

/// The slot to guard registry edits with (`Ok(None)` = Orca mode OFF).
///
/// Fails CLOSED: a `config.json` that exists but cannot be read is an `Err`
/// (the message says to fix the file), and the registry-mutating callers
/// refuse. Reading it as OFF would let `add`/`set` point a profile at the
/// slot's dir while Orca owns it. The same refusal as
/// `cas::platform::apply_global`, `csm cas --print-floor-dir` and `csm config`.
pub fn slot_for_guard(profiles: &ProfileMap) -> Result<Option<Slot>, String> {
    if !disk_state_readable() {
        return Ok(None);
    }
    match Config::load() {
        Ok(cfg) => {
            warn_if_unregistered(&cfg, profiles);
            Ok(active_slot(&cfg, profiles))
        }
        Err(e) => Err(format!(
            "config.json unreadable ({e}); Orca mode cannot be determined, so registry edits are \
             refused until it is fixed (or removed)"
        )),
    }
}

/// `true` when `config.json` exists but cannot be read, so Orca mode is
/// unknown. The silent loaders above read that as OFF; a caller about to do
/// something the slot must never see (rotating an OAuth token, launching
/// with no slot exclusion) checks this to fail closed or warn. Silent.
pub fn config_unreadable() -> bool {
    disk_state_readable() && Config::load().is_err()
}

// ─── diagnosis (csm orca status) ──────────────────────────────────────────────

/// Severity of one finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Ok,
    Info,
    Warn,
    Error,
}

impl Level {
    fn tag(self) -> &'static str {
        match self {
            Level::Ok => "ok",
            Level::Info => "info",
            Level::Warn => "WARN",
            Level::Error => "ERROR",
        }
    }
}

/// One line of `csm orca status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    pub level: Level,
    /// Stable machine code (`--json` consumers match on it).
    pub code: &'static str,
    pub message: String,
}

/// Everything [`diagnose`] needs, gathered by [`gather`] (or built by a test).
#[derive(Debug, Clone)]
pub struct DiagnosisInput {
    pub os: HostOs,
    /// `config.json` failed to parse (Orca mode then reads as OFF).
    pub config_error: Option<String>,
    pub slot: SlotResolution,
    pub user_data_dir: Option<PathBuf>,
    /// `orca-runtime.json`: `Ok(Some(pid))` present, `Ok(None)` absent.
    pub runtime_file: Result<Option<u32>, String>,
    pub orca_running: bool,
    /// Orca's actual runtime dir, from its process environment.
    pub runtime_dir: Option<String>,
    /// Live when running, offline otherwise.
    pub state: OrcaState,
    /// The registry, sorted by name.
    pub profiles: Vec<(String, String)>,
    pub slot_identity: Option<Identity>,
    pub active_identity: Option<Identity>,
    pub bindings: Option<Bindings>,
    /// Non-slot profiles whose live access token equals the slot's.
    pub token_sharers: Vec<String>,
    pub pending: Option<Result<PendingSelect, String>>,
    pub pending_verdict: Option<Verdict>,
    pub negative_cache_until: Option<i64>,
    pub agent_cmd_override: Option<String>,
    pub agent_default_args: Option<String>,
    pub slot_settings_exists: Option<bool>,
    /// `launchctl getenv CLAUDE_CONFIG_DIR` (macOS).
    pub launchd_floor: Option<String>,
    /// `~/.config/claude-as/floor-dir`.
    pub floor_file: Option<String>,
    pub other_data_files: Vec<PathBuf>,
    pub now: i64,
}

/// The `csm orca status` result.
#[derive(Debug, Clone, Serialize)]
pub struct Diagnosis {
    pub slot: Option<Slot>,
    #[serde(rename = "orcaRunning")]
    pub orca_running: bool,
    pub pid: Option<u32>,
    #[serde(rename = "runtimeDir")]
    pub runtime_dir: Option<String>,
    /// `live` / `offline` / `absent` / `unknown`.
    pub state: &'static str,
    #[serde(rename = "activeAccountId")]
    pub active_account_id: Option<String>,
    #[serde(rename = "activeEmail")]
    pub active_email: Option<String>,
    #[serde(rename = "activeProfile")]
    pub active_profile: Option<String>,
    pub findings: Vec<Finding>,
}

impl Diagnosis {
    /// Any ERROR finding (`csm orca status --strict` exits 1).
    pub fn has_error(&self) -> bool {
        self.findings.iter().any(|f| f.level == Level::Error)
    }

    /// Human rendering, one finding per line.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for f in &self.findings {
            out.push_str(&format!("{:<5}  {}\n", f.level.tag(), f.message));
        }
        out
    }
}

/// Build the diagnosis. Pure.
pub fn diagnose(input: &DiagnosisInput) -> Diagnosis {
    let mut f: Vec<Finding> = Vec::new();
    let mut push = |level, code, message: String| {
        f.push(Finding {
            level,
            code,
            message,
        })
    };

    if let Some(e) = &input.config_error {
        push(
            Level::Error,
            "config-unreadable",
            format!("config.json is unreadable ({e}); Orca mode reads as off"),
        );
    }
    let slot = match &input.slot {
        SlotResolution::On(s) => {
            push(
                Level::Ok,
                "slot",
                format!("Orca mode on: slot profile \"{}\" → {}", s.name, s.dir),
            );
            Some(s.clone())
        }
        SlotResolution::Unregistered(n) => {
            push(
                Level::Warn,
                "slot-unregistered",
                format!(
                    "orca.slotProfile is \"{n}\" but that profile is not registered; Orca mode is off"
                ),
            );
            None
        }
        SlotResolution::Off => {
            push(
                Level::Info,
                "slot-off",
                "Orca mode is off (set it up with `csm orca init`)".to_owned(),
            );
            None
        }
    };

    match &input.user_data_dir {
        Some(ud) => push(
            Level::Info,
            "user-data",
            format!("Orca userData: {}", ud.display()),
        ),
        None => push(
            Level::Info,
            "user-data-unknown",
            "Orca's userData dir could not be resolved".to_owned(),
        ),
    }

    let pid = match &input.runtime_file {
        Ok(Some(pid)) => Some(*pid),
        _ => None,
    };
    match (&input.runtime_file, input.orca_running) {
        (Err(e), _) => push(
            Level::Warn,
            "runtime-file-unreadable",
            format!("orca-runtime.json is unreadable: {e}"),
        ),
        (Ok(None), _) => push(
            Level::Info,
            "orca-not-running",
            "Orca is not running (no orca-runtime.json)".to_owned(),
        ),
        (Ok(Some(p)), false) => push(
            Level::Info,
            "orca-not-running",
            format!("Orca is not running (stale orca-runtime.json, pid {p})"),
        ),
        (Ok(Some(p)), true) => push(
            Level::Ok,
            "orca-running",
            format!("Orca is running (pid {p})"),
        ),
    }

    if input.orca_running {
        match (&input.runtime_dir, &slot) {
            (None, _) => push(
                Level::Warn,
                "runtime-dir-unknown",
                "could not read Orca's runtime dir from its process environment; \
                 csm will not select accounts in Orca"
                    .to_owned(),
            ),
            (Some(rd), Some(s)) if !s.is_dir(rd) => push(
                Level::Error,
                "runtime-dir-mismatch",
                format!(
                    "Orca materializes accounts into {rd}, not the slot dir {}; restart Orca \
                     from an environment where CLAUDE_CONFIG_DIR is the slot",
                    s.dir
                ),
            ),
            (Some(rd), Some(_)) => push(
                Level::Ok,
                "runtime-dir",
                format!("Orca's runtime dir is the slot ({rd})"),
            ),
            (Some(rd), None) => push(
                Level::Info,
                "runtime-dir",
                format!("Orca's runtime dir: {rd}"),
            ),
        }
    }

    for (name, dir) in &input.profiles {
        if slot.as_ref().is_some_and(|s| s.is_profile(name)) {
            continue;
        }
        if let Some(s) = &slot
            && s.is_dir(dir)
        {
            push(
                Level::Error,
                "profile-is-slot-dir",
                format!(
                    "profile \"{name}\" uses the slot dir {dir}; Orca overwrites its credentials"
                ),
            );
        } else if let Some(rd) = &input.runtime_dir
            && dirs_equal(rd, dir)
        {
            push(
                Level::Error,
                "profile-is-runtime-dir",
                format!(
                    "profile \"{name}\" ({dir}) is Orca's runtime dir; selecting an account in \
                     Orca overwrites that profile's credentials"
                ),
            );
        }
    }

    let (state_label, sel) = match &input.state {
        OrcaState::Live(s) => ("live", Some(s)),
        OrcaState::Offline(s) => ("offline", Some(s)),
        OrcaState::Absent => ("absent", None),
        OrcaState::Unknown(r) => {
            push(
                if input.orca_running {
                    Level::Warn
                } else {
                    Level::Info
                },
                "state-unknown",
                format!("Orca's account state could not be read: {r}"),
            );
            ("unknown", None)
        }
    };
    let active_id = sel.and_then(|s| s.effective_active_id()).map(str::to_owned);
    let active_email = sel
        .and_then(|s| s.active_account())
        .map(|a| a.email.clone())
        .filter(|e| !e.is_empty());
    let active_profile = active_id.as_deref().and_then(|id| {
        input
            .bindings
            .as_ref()
            .and_then(|b| b.profile_for(id))
            .map(str::to_owned)
    });
    if let Some(sel) = sel {
        let who = match (&active_id, &active_email) {
            (Some(id), Some(e)) => format!("{e} ({id})"),
            (Some(id), None) => id.clone(),
            _ => "System default (no managed account)".to_owned(),
        };
        let bound = match (&active_id, &active_profile) {
            (Some(_), Some(p)) => format!(" → profile \"{p}\""),
            (Some(_), None) => " → no bound profile".to_owned(),
            _ => String::new(),
        };
        push(
            Level::Info,
            "active",
            format!(
                "Orca active account ({state_label}, {} accounts): {who}{bound}",
                sel.accounts.len()
            ),
        );
    }

    if slot.is_some()
        && let (Some(si), Some(ai)) = (&input.slot_identity, &input.active_identity)
        && super::bind::match_kind(ai, si).is_none()
    {
        push(
            Level::Warn,
            "slot-identity-mismatch",
            format!(
                "the slot is logged in as {} but Orca's active account is {}",
                si.email.as_deref().unwrap_or("?"),
                ai.email.as_deref().unwrap_or("?")
            ),
        );
    }

    if !input.token_sharers.is_empty() {
        let list = input.token_sharers.join(", ");
        push(
            Level::Warn,
            "shared-token",
            format!(
                "profile(s) {list} hold the same live access token as the slot; Orca's token \
                 rotation will log them out — re-login each with \
                 `csm --profile <p> claude auth login` while Orca is quit"
            ),
        );
    }

    if let Some(b) = &input.bindings {
        for t in &b.ties {
            push(
                Level::Info,
                "tie",
                format!(
                    "account {} matches profiles {}; using \"{}\" (override with \
                     orca.bindings in config.json)",
                    t.account_id,
                    t.profiles.join(", "),
                    t.chosen
                ),
            );
        }
        for (id, bnd) in &b.by_account {
            if bnd.kind.is_weak() {
                push(
                    Level::Info,
                    "weak-binding",
                    format!(
                        "account {id} → \"{}\" is a weak match ({:?})",
                        bnd.profile, bnd.kind
                    ),
                );
            }
        }
        if !b.unbound_accounts.is_empty() {
            push(
                Level::Info,
                "unbound-accounts",
                format!(
                    "Orca accounts with no csm profile: {}",
                    b.unbound_accounts.join(", ")
                ),
            );
        }
        if !b.unbound_profiles.is_empty() {
            push(
                Level::Info,
                "unbound-profiles",
                format!(
                    "csm profiles with no Orca account: {}",
                    b.unbound_profiles.join(", ")
                ),
            );
        }
        if !b.non_host_accounts.is_empty() {
            push(
                Level::Info,
                "non-host-accounts",
                format!(
                    "non-host (WSL) Orca accounts, never bound: {}",
                    b.non_host_accounts.join(", ")
                ),
            );
        }
        for (id, p) in &b.invalid_overrides {
            push(
                Level::Warn,
                "invalid-override",
                format!(
                    "orca.bindings maps {id} to \"{p}\", which is not a registered non-slot profile"
                ),
            );
        }
    }

    match &input.pending {
        Some(Ok(p)) => {
            let v = input
                .pending_verdict
                .map(|v| format!("{v:?}"))
                .unwrap_or_else(|| "waiting for Orca".to_owned());
            push(
                Level::Info,
                "pending-select",
                format!(
                    "pending select: \"{}\" (account {}), queued {}s ago — {v}",
                    p.profile,
                    p.account_id,
                    input.now - p.queued_at
                ),
            );
        }
        Some(Err(e)) => push(
            Level::Warn,
            "pending-unreadable",
            format!("pending select file is unreadable: {e}"),
        ),
        None => {}
    }
    if let Some(until) = input.negative_cache_until {
        push(
            Level::Info,
            "rpc-negative-cache",
            format!(
                "Orca RPC recently timed out; launches skip it for {}s more",
                (until - input.now).max(0)
            ),
        );
    }
    if let Some(c) = &input.agent_cmd_override {
        push(
            Level::Info,
            "agent-cmd-override",
            format!("Orca agentCmdOverrides.claude = {c}"),
        );
    }
    if let Some(a) = &input.agent_default_args {
        push(
            Level::Info,
            "agent-default-args",
            format!("Orca agentDefaultArgs.claude = {a}"),
        );
    }

    if let Some(s) = &slot {
        match input.slot_settings_exists {
            Some(true) => push(
                Level::Ok,
                "slot-settings",
                "slot settings.json present".to_owned(),
            ),
            Some(false) => push(
                Level::Warn,
                "slot-settings-missing",
                format!(
                    "{}/settings.json is missing; template it like the other profiles \
                     (hooks, statusline)",
                    s.dir
                ),
            ),
            None => {}
        }
        match input.os {
            HostOs::MacOs => match &input.launchd_floor {
                Some(fl) if s.is_dir(fl) => push(
                    Level::Ok,
                    "floor",
                    format!("launchd CLAUDE_CONFIG_DIR is the slot ({fl})"),
                ),
                Some(fl) => push(
                    Level::Warn,
                    "floor-mismatch",
                    format!(
                        "launchd CLAUDE_CONFIG_DIR is {fl}, not the slot {}; run `csm orca init` \
                         (or `csm cas -g`) to repair",
                        s.dir
                    ),
                ),
                None => push(
                    Level::Warn,
                    "floor-unset",
                    "launchd CLAUDE_CONFIG_DIR is not set; GUI apps (Orca) will not inherit the slot"
                        .to_owned(),
                ),
            },
            HostOs::Linux => push(
                Level::Info,
                "linux-floor",
                format!(
                    "Linux has no machine-wide floor: start Orca with CLAUDE_CONFIG_DIR={}",
                    s.dir
                ),
            ),
            HostOs::Windows => push(
                Level::Info,
                "windows-unsupported",
                "Windows: account selection sync is not supported (no named-pipe transport)"
                    .to_owned(),
            ),
        }
        match &input.floor_file {
            Some(ff) if s.is_dir(ff) => {}
            Some(ff) => push(
                Level::Warn,
                "floor-file-mismatch",
                format!(
                    "~/.config/claude-as/floor-dir says {ff}, not the slot {}",
                    s.dir
                ),
            ),
            None => push(
                Level::Warn,
                "floor-file-missing",
                "~/.config/claude-as/floor-dir is missing".to_owned(),
            ),
        }
    }

    for p in &input.other_data_files {
        push(
            Level::Info,
            "other-data-file",
            format!(
                "additional Orca data file {} (csm reads only local-default)",
                p.display()
            ),
        );
    }

    Diagnosis {
        slot,
        orca_running: input.orca_running,
        pid,
        runtime_dir: input.runtime_dir.clone(),
        state: state_label,
        active_account_id: active_id,
        active_email,
        active_profile,
        findings: f,
    }
}

// ─── gather (I/O shell) ───────────────────────────────────────────────────────

fn token_digest(dir: &Path, now: chrono::DateTime<chrono::Utc>) -> Option<[u8; 32]> {
    use sha2::{Digest, Sha256};
    let tok = crate::usage::local::creds::lookup(dir, now).ok()?;
    Some(Sha256::digest(tok.access_token.as_bytes()).into())
}

/// Collect a [`DiagnosisInput`] from disk, the process table, the launchd
/// floor and (read-only, bounded by `budget`) Orca's `accounts.list`. Tokens
/// are compared as SHA-256 digests in memory and never leave this function.
pub fn gather(
    profiles: &ProfileMap,
    config: Result<&Config, String>,
    budget: Duration,
) -> DiagnosisInput {
    let default_cfg = Config::default();
    let (cfg, config_error) = match config {
        Ok(c) => (c, None),
        Err(e) => (&default_cfg, Some(e)),
    };
    let slot_res = resolve(cfg, profiles);
    let slot = match &slot_res {
        SlotResolution::On(s) => Some(s.clone()),
        _ => None,
    };
    let ud = super::user_data_dir_for(cfg.orca());
    let meta = ud
        .as_deref()
        .map(super::runtime_metadata_in)
        .unwrap_or(Ok(None));
    let running = matches!(&meta, Ok(Some(m)) if m.is_alive());
    let runtime_file = match &meta {
        Ok(m) => Ok(m.as_ref().map(|m| m.pid)),
        Err(e) => Err(e.to_string()),
    };
    let runtime_dir = match &meta {
        Ok(Some(m)) if running => {
            super::runtime_config_dir(m.pid).map(|p| p.to_string_lossy().into_owned())
        }
        _ => None,
    };
    let negative_cache_until = match &meta {
        Ok(Some(m)) => super::rpc::negative_cache_until(&m.runtime_id),
        _ => None,
    };
    let state = match &ud {
        Some(ud) => super::selection_in(ud, budget),
        None => OrcaState::Absent,
    };
    let offline = ud
        .as_deref()
        .and_then(|u| super::data_file::read_in(u).ok().flatten());
    let sel = match &state {
        OrcaState::Live(s) | OrcaState::Offline(s) => Some(s.clone()),
        _ => offline.as_ref().map(|d| d.selection.clone()),
    };
    let bindings = match (&ud, &sel) {
        (Some(ud), Some(sel)) => Some(super::bind::compute(
            ud,
            sel,
            profiles,
            slot.as_ref(),
            &cfg.orca().bindings,
        )),
        _ => None,
    };
    let active_identity = match (&ud, &sel) {
        (Some(ud), Some(sel)) => sel
            .active_account()
            .map(|a| super::identity::account_identity(ud, a)),
        _ => None,
    };
    let slot_identity = slot.as_ref().and_then(|s| {
        super::identity::read_profile_identity(Path::new(&s.dir))
            .ok()
            .flatten()
    });

    let now_utc = chrono::Utc::now();
    let token_sharers = match &slot {
        Some(s) => match token_digest(Path::new(&s.dir), now_utc) {
            Some(slot_digest) => profiles
                .names_sorted()
                .into_iter()
                .filter(|n| !s.is_profile(n))
                .filter(|n| {
                    profiles
                        .get(n)
                        .and_then(|d| token_digest(Path::new(d), now_utc))
                        .is_some_and(|d| d == slot_digest)
                })
                .map(str::to_owned)
                .collect(),
            None => Vec::new(),
        },
        None => Vec::new(),
    };

    let now = crate::epoch::now_secs() as i64;
    let pending = match super::pending::read() {
        Ok(Some(p)) => Some(Ok(p)),
        Ok(None) => None,
        Err(e) => Some(Err(e.to_string())),
    };
    let pending_verdict = match (&pending, &state) {
        (Some(Ok(p)), OrcaState::Live(s)) => {
            Some(super::pending::verdict(p, s.effective_active_id(), now))
        }
        _ => None,
    };
    let floor_file = std::fs::read_to_string(crate::paths::floor_dir_file())
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());

    DiagnosisInput {
        os: HostOs::current(),
        config_error,
        slot: slot_res,
        user_data_dir: ud.clone(),
        runtime_file,
        orca_running: running,
        runtime_dir,
        state,
        profiles: profiles
            .names_sorted()
            .into_iter()
            .map(|n| (n.to_owned(), profiles.get(n).unwrap_or_default().to_owned()))
            .collect(),
        slot_identity,
        active_identity,
        bindings,
        token_sharers,
        pending,
        pending_verdict,
        negative_cache_until,
        agent_cmd_override: offline.as_ref().and_then(|d| d.agent_cmd_override.clone()),
        agent_default_args: offline.as_ref().and_then(|d| d.agent_default_args.clone()),
        slot_settings_exists: slot
            .as_ref()
            .map(|s| Path::new(&s.dir).join("settings.json").is_file()),
        launchd_floor: crate::cas::platform::launchctl_getenv_config_dir(),
        floor_file,
        other_data_files: ud
            .as_deref()
            .map(super::data_file::other_data_files)
            .unwrap_or_default(),
        now,
    }
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orca::{Account, Selection};
    use std::collections::HashMap;

    const SLOT_DIR: &str = "/Users/example/.claude.orca";
    const WORK_DIR: &str = "/Users/example/.claude.work";

    fn registry() -> ProfileMap {
        let mut m = HashMap::new();
        m.insert("orca".to_owned(), SLOT_DIR.to_owned());
        m.insert("work".to_owned(), WORK_DIR.to_owned());
        ProfileMap(m)
    }

    fn cfg(slot: Option<&str>) -> Config {
        let mut c = Config::default();
        c.orca.slot_profile = slot.map(Into::into);
        c
    }

    #[test]
    fn resolution_states() {
        let p = registry();
        assert_eq!(resolve(&cfg(None), &p), SlotResolution::Off);
        assert_eq!(resolve(&cfg(Some("  ")), &p), SlotResolution::Off);
        assert_eq!(
            resolve(&cfg(Some("gone")), &p),
            SlotResolution::Unregistered("gone".into())
        );
        let s = active_slot(&cfg(Some("orca")), &p).unwrap();
        assert_eq!(s.dir, SLOT_DIR);
        assert!(s.is_profile("orca") && !s.is_profile("work"));
        assert!(s.is_dir("/Users/example/.claude.orca/"));
        assert!(!s.is_dir(WORK_DIR));
    }

    #[test]
    fn registry_edit_guard() {
        let s = active_slot(&cfg(Some("orca")), &registry()).unwrap();
        let refused = |e| registry_edit_refusal(e, &s).is_some();
        assert!(refused(RegistryEdit::Remove { name: "orca" }));
        assert!(refused(RegistryEdit::Rename { from: "orca" }));
        assert!(refused(RegistryEdit::Repoint {
            name: "orca",
            dir: SLOT_DIR
        }));
        assert!(refused(RegistryEdit::Repoint {
            name: "orca",
            dir: WORK_DIR
        }));
        assert!(refused(RegistryEdit::Add {
            name: "alias",
            dir: "/Users/example/.claude.orca/"
        }));
        assert!(refused(RegistryEdit::Repoint {
            name: "work",
            dir: SLOT_DIR
        }));
        assert!(!refused(RegistryEdit::Remove { name: "work" }));
        assert!(!refused(RegistryEdit::Rename { from: "work" }));
        assert!(!refused(RegistryEdit::Repoint {
            name: "work",
            dir: "/Users/example/.claude.w2"
        }));
        assert!(!refused(RegistryEdit::Add {
            name: "home",
            dir: "/Users/example/.claude.home"
        }));
    }

    fn base_input() -> DiagnosisInput {
        let slot = active_slot(&cfg(Some("orca")), &registry()).unwrap();
        DiagnosisInput {
            os: HostOs::MacOs,
            config_error: None,
            slot: SlotResolution::On(slot),
            user_data_dir: Some(PathBuf::from("/Users/example/orca-ud")),
            runtime_file: Ok(Some(4242)),
            orca_running: true,
            runtime_dir: Some(SLOT_DIR.to_owned()),
            state: OrcaState::Live(Selection {
                accounts: vec![Account {
                    id: "acct-1".into(),
                    email: "alice@example.com".into(),
                    organization_uuid: None,
                    organization_name: None,
                    runtime: "host".into(),
                }],
                active_id: Some("acct-1".into()),
                host_active_id: None,
                rate_limits: None,
            }),
            profiles: vec![
                ("orca".into(), SLOT_DIR.into()),
                ("work".into(), WORK_DIR.into()),
            ],
            slot_identity: None,
            active_identity: None,
            bindings: None,
            token_sharers: vec![],
            pending: None,
            pending_verdict: None,
            negative_cache_until: None,
            agent_cmd_override: None,
            agent_default_args: None,
            slot_settings_exists: Some(true),
            launchd_floor: Some(SLOT_DIR.to_owned()),
            floor_file: Some(SLOT_DIR.to_owned()),
            other_data_files: vec![],
            now: 1_000,
        }
    }

    fn codes(d: &Diagnosis) -> Vec<&'static str> {
        d.findings.iter().map(|f| f.code).collect()
    }

    #[test]
    fn healthy_setup_has_no_warnings() {
        let d = diagnose(&base_input());
        assert!(!d.has_error());
        assert!(
            d.findings.iter().all(|f| f.level <= Level::Info),
            "{:#?}",
            d.findings
        );
        assert_eq!(d.state, "live");
        assert_eq!(d.active_account_id.as_deref(), Some("acct-1"));
        assert_eq!(d.active_email.as_deref(), Some("alice@example.com"));
    }

    #[test]
    fn runtime_dir_mismatch_and_profile_collisions_are_errors() {
        let mut i = base_input();
        i.runtime_dir = Some(WORK_DIR.to_owned());
        let d = diagnose(&i);
        assert!(d.has_error());
        let c = codes(&d);
        assert!(c.contains(&"runtime-dir-mismatch"), "{c:?}");
        assert!(c.contains(&"profile-is-runtime-dir"), "{c:?}");

        let mut i = base_input();
        i.profiles.push(("home".into(), format!("{SLOT_DIR}/")));
        assert!(codes(&diagnose(&i)).contains(&"profile-is-slot-dir"));
    }

    #[test]
    fn warnings_for_identity_tokens_floor_and_settings() {
        let mut i = base_input();
        i.slot_identity = Some(Identity {
            email: Some("bob@example.com".into()),
            ..Default::default()
        });
        i.active_identity = Some(Identity {
            email: Some("alice@example.com".into()),
            ..Default::default()
        });
        i.token_sharers = vec!["work".into()];
        i.launchd_floor = Some(WORK_DIR.into());
        i.floor_file = None;
        i.slot_settings_exists = Some(false);
        let d = diagnose(&i);
        let c = codes(&d);
        for want in [
            "slot-identity-mismatch",
            "shared-token",
            "floor-mismatch",
            "floor-file-missing",
            "slot-settings-missing",
        ] {
            assert!(c.contains(&want), "{want} missing from {c:?}");
        }
        assert!(!d.has_error(), "these are warnings, not errors");
    }

    #[test]
    fn off_and_unregistered_modes() {
        let mut i = base_input();
        i.slot = SlotResolution::Unregistered("gone".into());
        let d = diagnose(&i);
        assert!(codes(&d).contains(&"slot-unregistered"));
        assert!(d.slot.is_none());
        assert!(!codes(&d).contains(&"floor"), "no floor checks while off");

        i.slot = SlotResolution::Off;
        i.config_error = Some("bad json".into());
        let d = diagnose(&i);
        assert!(d.has_error());
        assert!(codes(&d).contains(&"slot-off"));
    }

    #[test]
    fn stopped_orca_reports_offline_state() {
        let mut i = base_input();
        i.orca_running = false;
        i.runtime_dir = None;
        i.state = OrcaState::Offline(Selection::default());
        let d = diagnose(&i);
        assert_eq!(d.state, "offline");
        assert!(codes(&d).contains(&"orca-not-running"));
        assert!(!codes(&d).contains(&"runtime-dir-unknown"));
    }

    #[test]
    fn linux_prints_the_manual_floor_note() {
        let mut i = base_input();
        i.os = HostOs::Linux;
        i.launchd_floor = None;
        let c = codes(&diagnose(&i)).clone();
        assert!(c.contains(&"linux-floor"));
        assert!(!c.contains(&"floor-unset"));
    }

    #[test]
    fn guard_fails_closed_on_an_unreadable_config() {
        let home = tempfile::tempdir().unwrap();
        crate::testenv::with_test_home(home.path(), || {
            let mut m = std::collections::HashMap::new();
            m.insert("work".to_owned(), "/Users/example/.claude.work".to_owned());
            let profiles = ProfileMap(m);
            // Absent config → Orca mode OFF, readable.
            assert_eq!(slot_for_guard(&profiles), Ok(None));
            assert!(!config_unreadable());
            let c = crate::paths::config_json();
            std::fs::create_dir_all(c.parent().unwrap()).unwrap();
            std::fs::write(&c, "{ not json").unwrap();
            assert!(slot_for_guard(&profiles).is_err());
            assert!(config_unreadable());
            // The silent loader still reads it as OFF.
            assert_eq!(for_registry(&profiles), None);
        });
    }
}
