//! `csm migrate` — move a machine off the legacy per-profile layout.
//!
//! The legacy layout: `~/.config/claude-as/profiles.json` maps profile names
//! to config dirs, a machine-wide `CLAUDE_CONFIG_DIR` floor points Orca and
//! every shell at one of them, and `~/.claude.shared/` holds the transcripts
//! every profile links to. The target: Orca owns the accounts, `D` is
//! `~/.claude`, csm keeps no registry (design §9).
//!
//! - `plan` (step 1) reads only, and reads no secret: each profile's status
//!   against Orca's accounts (`in Orca` / `to import` / `no credentials`),
//!   where its grant sits (Keychain items are probed for presence without
//!   `-w`, credential files by existence), Orca's current `D` and the
//!   target, and what steps 5-6 would do. Whether a dir's grant is fresher
//!   than the stash needs the secret, so `import` decides that.
//! - `import` (steps 4-7) imports or reads back each profile's login (it
//!   reads the grants: fingerprints and `expiresAt` pick what is fresher),
//!   seeds `~/.claude.json` from the floor profile's `.claude.json` minus
//!   `oauthAccount` when it does not exist yet and merges the floor
//!   profile's trust fields and MCP servers into it, turns the
//!   `~/.claude/{projects,sessions,plugins}` links into real dirs, moves
//!   csm's session sidecars and indexes from `~/.claude.shared/smart` to the
//!   new state dir, and switches to the floor profile's account.
//! - `retire` (step 8) moves a verified profile's grants into the
//!   quarantine, renames its dir to `<dir>.retired`, and once no profile
//!   holds a login any more removes the legacy registry files and the floor
//!   variable (`launchctl unsetenv` / `HKCU\Environment`).
//!
//! Every write step refuses while Orca runs, while a claude session is live
//! in an affected dir, while another csm process (a supervisor that would
//! restart claude) or a claude an earlier csm started runs, or while
//! `CLAUDE_CONFIG_DIR` is set, in this process or in the login session (the
//! launchd variable, `HKCU\Environment`) that Orca inherits. `import` takes
//! that gate again before steps 5 and 6. `--dry-run` prints what would be
//! done. Output never carries a token: grants appear as quarantine
//! fingerprints.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use serde_json::{Map, Value};

use crate::orca::add::{self, SystemClaude};
use crate::orca::context::{Context, live_claude_in};
use crate::orca::http::{OauthHttp, ProfileAnswer, SystemHttp, parse_profile};
use crate::orca::keychain;
use crate::orca::live::{ProcFacts, SystemProcs};
use crate::orca::quarantine::{self, Quarantine, Reason};
use crate::orca::readback::{self, ReadBack, access_token, read_freshness};
use crate::orca::record::{AccountRecord, AuthRuntime, IdentityKey, find_by_identity};
use crate::orca::runtime::{OauthIdentity, read_json_object, runtime_paths};
use crate::orca::stash::Stash;
use crate::orca::switch::{self, Outcome};
use crate::orca::{HostOs, OrcaView, SnapshotOptions, fsx};

// ─── arguments (pure) ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MigrateCmd {
    Plan,
    Import { dry_run: bool },
    Retire { dry_run: bool, only: Vec<String> },
    Help,
}

pub(crate) fn parse(args: &[OsString]) -> anyhow::Result<MigrateCmd> {
    let words: Vec<String> = args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let Some((verb, rest)) = words.split_first() else {
        return Ok(MigrateCmd::Plan);
    };
    let mut dry_run = false;
    let mut names = Vec::new();
    for w in rest {
        match w.as_str() {
            "--dry-run" | "-n" => dry_run = true,
            f if f.starts_with('-') => bail!("csm migrate {verb}: unknown flag {f:?}"),
            name => names.push(name.to_owned()),
        }
    }
    Ok(match verb.as_str() {
        "plan" if rest.is_empty() => MigrateCmd::Plan,
        "import" if names.is_empty() => MigrateCmd::Import { dry_run },
        "retire" => MigrateCmd::Retire {
            dry_run,
            only: names,
        },
        "-h" | "--help" | "help" => MigrateCmd::Help,
        other => bail!(
            "csm migrate: unexpected {other:?} (plan | import [--dry-run] | retire [--dry-run] [profile...])"
        ),
    })
}

/// `csm migrate …`
pub(crate) fn cmd_migrate(args: &[OsString]) -> anyhow::Result<()> {
    match parse(args)? {
        MigrateCmd::Plan => plan_cmd(),
        MigrateCmd::Import { dry_run } => import_cmd(dry_run),
        MigrateCmd::Retire { dry_run, only } => retire_cmd(dry_run, &only),
        MigrateCmd::Help => {
            println!(
                "csm migrate plan                          what the migration would do (read-only)"
            );
            println!(
                "csm migrate import [--dry-run]            import logins, merge config, move shared dirs, switch"
            );
            println!(
                "csm migrate retire [--dry-run] [name...]  retire verified legacy profile dirs"
            );
            Ok(())
        }
    }
}

// ─── the legacy registry ──────────────────────────────────────────────────────

/// One `profiles.json` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LegacyProfile {
    pub name: String,
    pub dir: PathBuf,
}

/// The legacy registry, read-only.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Legacy {
    /// Sorted by name.
    pub profiles: Vec<LegacyProfile>,
    /// The floor profile's name.
    pub floor: Option<String>,
}

/// `~/.config/claude-as`.
pub(crate) fn legacy_dir(home: &Path) -> PathBuf {
    home.join(".config").join("claude-as")
}

const LEGACY_FILES: [&str; 3] = ["profiles.json", "default", "floor-dir"];

/// Parse `profiles.json` plus the `default` / `floor-dir` texts. Pure.
pub(crate) fn parse_legacy(
    profiles_json: Option<&str>,
    default: Option<&str>,
    floor_dir: Option<&str>,
) -> Result<Legacy, String> {
    let Some(text) = profiles_json else {
        return Ok(Legacy::default());
    };
    let map: BTreeMap<String, String> =
        serde_json::from_str(text).map_err(|e| format!("profiles.json does not parse: {e}"))?;
    let profiles: Vec<LegacyProfile> = map
        .into_iter()
        .map(|(name, dir)| LegacyProfile {
            name,
            dir: PathBuf::from(dir),
        })
        .collect();
    let by_name = default
        .map(str::trim)
        .filter(|n| profiles.iter().any(|p| p.name == *n))
        .map(str::to_owned);
    let by_dir = || {
        let d = floor_dir.map(str::trim).filter(|d| !d.is_empty())?;
        let d = d.trim_end_matches(['/', '\\']);
        profiles
            .iter()
            .find(|p| p.dir.to_string_lossy().trim_end_matches(['/', '\\']) == d)
            .map(|p| p.name.clone())
    };
    let floor = by_name.or_else(by_dir);
    Ok(Legacy { profiles, floor })
}

fn read_text(p: &Path) -> io::Result<Option<String>> {
    match std::fs::read_to_string(p) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

pub(crate) fn load_legacy(home: &Path) -> anyhow::Result<Legacy> {
    let dir = legacy_dir(home);
    let get = |f: &str| {
        let p = dir.join(f);
        read_text(&p).with_context(|| format!("cannot read {}", p.display()))
    };
    let (pj, def, fd) = (get("profiles.json")?, get("default")?, get("floor-dir")?);
    parse_legacy(pj.as_deref(), def.as_deref(), fd.as_deref()).map_err(anyhow::Error::msg)
}

// ─── per-profile facts ────────────────────────────────────────────────────────

/// A grant, secret-free.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct GrantFacts {
    pub fingerprint: String,
    pub expires_at: Option<f64>,
    /// What the blob holds beside the grant (MCP servers' OAuth logins),
    /// as [`quarantine::side_state`] digests; for a dir, over all its
    /// grants.
    pub side: BTreeMap<String, String>,
}

impl GrantFacts {
    fn of(json: &str) -> GrantFacts {
        GrantFacts {
            fingerprint: quarantine::fingerprint(json),
            expires_at: read_freshness(json),
            side: quarantine::side_state(json),
        }
    }
}

/// The MCP logins a dir's grants hold that its account's stash lacks, for
/// a row whose stash step 4 keeps: the fingerprint compares only the Claude
/// grant, so these would otherwise go unmentioned until retire files them.
/// Pure.
pub(crate) fn extra_logins(
    status: &Status,
    dir: Option<&GrantFacts>,
    stash: Option<&GrantFacts>,
) -> Vec<String> {
    match (status, dir) {
        (
            Status::InOrca {
                fresher: Some(false),
                ..
            },
            Some(g),
        ) => quarantine::uncovered(&g.side, &stash.map(|s| s.side.clone()).unwrap_or_default()),
        _ => Vec::new(),
    }
}

/// How far a step may look at a legacy dir's grants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Probe {
    /// Presence only: Keychain items without `-w`, files by existence
    /// (`plan`, binding decision: no secret reads).
    Presence,
    /// Read the grants (`import`).
    Read,
}

/// What one legacy dir holds.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ProfileFacts {
    pub name: String,
    pub dir: PathBuf,
    pub exists: bool,
    /// `oauthAccount` from the dir's `.claude.json`.
    pub email: Option<String>,
    pub organization_uuid: Option<String>,
    /// Where the dir's grants sit (`scoped-keychain`, `file`).
    pub grant_sources: Vec<&'static str>,
    /// The dir's freshest grant; [`Probe::Read`] only.
    pub grant: Option<GrantFacts>,
}

/// Where a profile stands against Orca's accounts.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Status {
    /// Orca has the account. `fresher`: the dir's grant would win Orca's
    /// acceptance rule over the stash's (a different refresh token that is
    /// not older, or no stashed grant); `None` when the grant was only
    /// probed for presence.
    InOrca {
        id: String,
        fresher: Option<bool>,
    },
    ToImport,
    NoCredentials,
}

/// The identity (Orca's e9i key) a profile's `.claude.json` names. Pure.
pub(crate) fn identity_key(p: &ProfileFacts) -> Option<IdentityKey> {
    IdentityKey::new(
        p.email.as_deref(),
        p.organization_uuid.as_deref(),
        AuthRuntime::Host,
        None,
    )
}

/// The Orca account a profile's identity names, whatever the dir holds.
/// Pure.
pub(crate) fn identity_match<'a>(
    p: &ProfileFacts,
    records: &'a [AccountRecord],
) -> Option<&'a AccountRecord> {
    identity_key(p).and_then(|k| find_by_identity(records, &k))
}

/// Classify a profile. Pure.
pub(crate) fn classify(
    p: &ProfileFacts,
    records: &[AccountRecord],
    stash_grant: &dyn Fn(&str) -> Option<GrantFacts>,
) -> Status {
    if p.grant.is_none() && p.grant_sources.is_empty() {
        return Status::NoCredentials;
    }
    let Some(rec) = identity_match(p, records) else {
        return Status::ToImport;
    };
    let fresher = p.grant.as_ref().map(|grant| match stash_grant(&rec.id) {
        None => true,
        Some(s) if s.fingerprint == grant.fingerprint => false,
        Some(s) => grant.expires_at.unwrap_or(0.0) >= s.expires_at.unwrap_or(0.0),
    });
    Status::InOrca {
        id: rec.id.clone(),
        fresher,
    }
}

/// The Keychain services of a legacy dir's spellings, minus any that
/// resolves to `~/.claude`: that names D's own item, which is never a legacy
/// dir's to report or retire.
fn dir_services(ctx: &Context, dir: &Path) -> Vec<String> {
    let d = ctx.env.home.join(".claude");
    let d_services: Vec<String> = keychain::dir_spellings(&d.to_string_lossy())
        .iter()
        .map(|s| keychain::runtime_service(Some(s)))
        .collect();
    let mut out: Vec<String> = Vec::new();
    for spelling in keychain::dir_spellings(&dir.to_string_lossy()) {
        let svc = keychain::runtime_service(Some(&spelling));
        if !d_services.contains(&svc) && !out.contains(&svc) {
            out.push(svc);
        }
    }
    out
}

/// The grant source a failed Keychain probe or read reports: the dir may
/// hold a login csm could not see (a locked Keychain, a `security`
/// timeout), so it counts as holding one (fail closed).
pub(crate) const KEYCHAIN_UNREADABLE: &str = "keychain-unreadable";

/// Where a legacy dir's grants sit, without reading one: Keychain items by
/// a presence probe (no `-w`), `.credentials.json` by existence. A probe
/// that fails reports [`KEYCHAIN_UNREADABLE`], never "no item".
fn dir_grant_sources(ctx: &Context, dir: &Path) -> Vec<&'static str> {
    let mut out = Vec::new();
    if ctx.os() == HostOs::MacOs {
        let probes: Vec<_> = dir_services(ctx, dir)
            .iter()
            .map(|svc| keychain::has_password(svc, &ctx.keychain_user.acct))
            .collect();
        if probes.iter().any(|r| matches!(r, Ok(true))) {
            out.push("scoped-keychain");
        }
        if probes.iter().any(Result::is_err) {
            out.push(KEYCHAIN_UNREADABLE);
        }
    }
    if dir.join(".credentials.json").is_file() {
        out.push("file");
    }
    out
}

/// Every grant a legacy dir holds, as `(source, location, grant)`.
type DirGrantItem = (&'static str, String, crate::orca::SecretString);

/// Every grant a legacy dir holds: its `.credentials.json` and, on macOS,
/// each Keychain spelling of the dir. Reads the secrets. The flag is set
/// when a Keychain read failed, so the list may be incomplete.
fn dir_grants(ctx: &Context, dir: &Path) -> (Vec<DirGrantItem>, bool) {
    let mut out = Vec::new();
    let mut unreadable = false;
    if ctx.os() == HostOs::MacOs {
        for svc in dir_services(ctx, dir) {
            match keychain::find_password(&svc, &ctx.keychain_user.acct) {
                Ok(Some(v)) => out.push(("scoped-keychain", svc, v)),
                Ok(None) => {}
                Err(_) => unreadable = true,
            }
        }
    }
    let file = dir.join(".credentials.json");
    if let Ok(Some(t)) = crate::orca::read_capped(&file, 1024 * 1024) {
        out.push((
            "file",
            file.to_string_lossy().into_owned(),
            crate::orca::SecretString::new(t),
        ));
    }
    (out, unreadable)
}

fn profile_facts(ctx: &Context, p: &LegacyProfile, probe: Probe) -> ProfileFacts {
    let cfg = read_json_object(&p.dir.join(".claude.json"));
    let ident = cfg
        .as_ref()
        .and_then(|m| m.get("oauthAccount"))
        .map(OauthIdentity::from_value);
    let (grant_sources, grant) = match probe {
        Probe::Presence => (dir_grant_sources(ctx, &p.dir), None),
        Probe::Read => {
            let (grants, unreadable) = dir_grants(ctx, &p.dir);
            let mut sources: Vec<&'static str> = grants.iter().map(|(s, _, _)| *s).collect();
            sources.dedup();
            if unreadable {
                sources.push(KEYCHAIN_UNREADABLE);
            }
            let facts: Vec<GrantFacts> = grants
                .iter()
                .map(|(_, _, g)| GrantFacts::of(g.expose()))
                .collect();
            let mut side = BTreeMap::new();
            for f in &facts {
                for (k, v) in &f.side {
                    side.entry(k.clone()).or_insert_with(|| v.clone());
                }
            }
            let best = facts
                .into_iter()
                .max_by(|a, b| {
                    a.expires_at
                        .unwrap_or(0.0)
                        .total_cmp(&b.expires_at.unwrap_or(0.0))
                })
                .map(|b| GrantFacts { side, ..b });
            (sources, best)
        }
    };
    ProfileFacts {
        name: p.name.clone(),
        dir: p.dir.clone(),
        exists: p.dir.is_dir(),
        email: ident.as_ref().and_then(|i| i.email.clone()),
        organization_uuid: ident.and_then(|i| i.organization_uuid),
        grant_sources,
        grant,
    }
}

fn stash_path(view: &OrcaView, id: &str) -> Option<String> {
    view.store
        .as_ref()
        .and_then(|s| s.account(id))
        .and_then(|r| r.managed_auth_path.clone())
}

fn stash_grant(ctx: &Context, view: &OrcaView, id: &str) -> Option<GrantFacts> {
    let s = Stash::open(&ctx.user_data.dir, id, stash_path(view, id).as_deref()).ok()?;
    let c = s.credentials(ctx.os()).ok()??;
    Some(GrantFacts::of(c.expose()))
}

// ─── step 5: ~/.claude.json carry-over (pure) ─────────────────────────────────

/// The per-project fields claude's trust prompt, project onboarding and
/// project MCP settings read, copied whole when the target lacks them or
/// holds only Claude Code's default for them.
/// Local-scope MCP servers (`projects[<path>].mcpServers`, where a plain
/// `claude mcp add` puts them) merge per server name instead
/// ([`merge_config`]).
pub(crate) const TRUST_FIELDS: [&str; 9] = [
    "hasTrustDialogAccepted",
    "hasCompletedProjectOnboarding",
    "projectOnboardingSeenCount",
    "hasClaudeMdExternalIncludesApproved",
    "hasClaudeMdExternalIncludesWarningShown",
    "enabledMcpjsonServers",
    "disabledMcpjsonServers",
    "allowedTools",
    "mcpContextUris",
];

/// Add each of `from`'s servers that `to` lacks, keeping `to`'s own; the
/// names added are reported as `<prefix>.<name>`.
fn merge_servers(
    to: &mut Map<String, Value>,
    from: &Map<String, Value>,
    prefix: &str,
    added: &mut Vec<String>,
) {
    let tm = to
        .entry("mcpServers")
        .or_insert_with(|| Value::Object(Map::new()));
    if let Value::Object(tm) = tm {
        for (name, v) in from {
            if !tm.contains_key(name) {
                tm.insert(name.clone(), v.clone());
                added.push(format!("{prefix}.{name}"));
            }
        }
    }
}

/// Is `v` the value Claude Code writes for a project field nobody set?
/// Claude Code saves a project entry whole from its default (`nne` in
/// 2.1.283: `hasTrustDialogAccepted: false`, `allowedTools: []`, the other
/// approvals `false`, the lists empty), so a `false`, `0`, `[]`, `{}` or
/// `null` in `~/.claude.json` records no decision: a `claude -p` run or a
/// declined dialog under the default dir leaves one behind. Pure.
pub(crate) fn is_cc_project_default(v: &Value) -> bool {
    match v {
        Value::Null | Value::Bool(false) => true,
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        _ => false,
    }
}

/// Merge `from`'s trust fields, its local-scope MCP servers (per project),
/// its user-scope MCP servers and its onboarding keys ([`ONBOARDING_KEYS`])
/// into `target`, keeping every key and
/// server `target` already has, except that a trust field `target` holds
/// at Claude Code's default ([`is_cc_project_default`]) takes `from`'s
/// value when that one is not a default too (so the floor's accepted trust
/// wins over a `false` claude wrote for an untrusted run). Returns what
/// was added or upgraded. Pure.
pub(crate) fn merge_config(
    target: &mut Map<String, Value>,
    from: &Map<String, Value>,
) -> Vec<String> {
    merge_config_with(target, from, true)
}

/// [`merge_config`], with `upgrade_defaults` false treating every key
/// `target` holds as merged, whatever its value. `retire`'s precondition
/// asks that way: a value the user changed after import (even back to a
/// default) must not hold retire back for good. Pure.
fn merge_config_with(
    target: &mut Map<String, Value>,
    from: &Map<String, Value>,
    upgrade_defaults: bool,
) -> Vec<String> {
    let mut added = Vec::new();
    if let Some(Value::Object(projects)) = from.get("projects") {
        for (path, entry) in projects {
            let Value::Object(entry) = entry else {
                continue;
            };
            let fields: Vec<(&str, &Value)> = TRUST_FIELDS
                .iter()
                .filter_map(|f| entry.get(*f).map(|v| (*f, v)))
                .collect();
            let servers = entry
                .get("mcpServers")
                .and_then(Value::as_object)
                .filter(|m| !m.is_empty());
            if fields.is_empty() && servers.is_none() {
                continue;
            }
            let tp = target
                .entry("projects")
                .or_insert_with(|| Value::Object(Map::new()));
            let Value::Object(tp) = tp else {
                continue;
            };
            let te = tp
                .entry(path.clone())
                .or_insert_with(|| Value::Object(Map::new()));
            let Value::Object(te) = te else {
                continue;
            };
            for (f, v) in fields {
                let take = match te.get(f) {
                    None => true,
                    // Claude Code's own default is not a choice: the floor's
                    // real value replaces it.
                    Some(t) => {
                        upgrade_defaults && is_cc_project_default(t) && !is_cc_project_default(v)
                    }
                };
                if take {
                    te.insert(f.to_owned(), v.clone());
                    added.push(format!("projects[{path}].{f}"));
                }
            }
            if let Some(servers) = servers {
                merge_servers(
                    te,
                    servers,
                    &format!("projects[{path}].mcpServers"),
                    &mut added,
                );
            }
        }
    }
    if let Some(Value::Object(servers)) = from.get("mcpServers") {
        merge_servers(target, servers, "mcpServers", &mut added);
    }
    // An existing small ~/.claude.json (a stray claude run, Orca's
    // `{"oauthAccount":…}` stub) lacks claude's onboarding state, and
    // claude then runs its first-launch flow inside Orca panes. Only a key
    // the target lacks is added.
    for k in ONBOARDING_KEYS {
        if let Some(v) = from.get(k)
            && !target.contains_key(k)
        {
            target.insert(k.to_owned(), v.clone());
            added.push(k.to_owned());
        }
    }
    added
}

/// Top-level `.claude.json` keys step 5 carries into an existing
/// `~/.claude.json` that lacks them: claude gates its first-launch
/// onboarding on `hasCompletedOnboarding`.
const ONBOARDING_KEYS: [&str; 3] = ["hasCompletedOnboarding", "lastOnboardingVersion", "theme"];

/// Step 5's result.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Carry {
    /// The new `.claude.json` object.
    pub map: Map<String, Value>,
    /// `Some(n)`: there was no file, and `n` top-level keys of the floor
    /// profile's object (all but `oauthAccount`) seeded it.
    pub seeded: Option<usize>,
    /// Trust fields and MCP servers merged in ([`merge_config`]).
    pub added: Vec<String>,
}

impl Carry {
    /// Does the file need writing?
    pub(crate) fn changed(&self) -> bool {
        self.seeded.is_some() || !self.added.is_empty()
    }
}

/// Step 5 over the target's object, `None` when the file does not exist
/// yet. Then the floor profile's whole object minus `oauthAccount` seeds it
/// (binding decision): claude keeps its onboarding state and settings
/// instead of running its first-launch flow inside Orca panes, and the
/// identity comes from the switch, never from the retired profile. Then the
/// trust fields and MCP servers merge as for an existing file. Pure.
pub(crate) fn carry_config(target: Option<Map<String, Value>>, from: &Map<String, Value>) -> Carry {
    let (mut map, seeded) = match target {
        Some(m) => (m, None),
        None => {
            let mut m = from.clone();
            m.remove("oauthAccount");
            let n = m.len();
            (m, Some(n))
        }
    };
    let added = merge_config(&mut map, from);
    Carry { map, seeded, added }
}

// ─── step 6: shared dirs (pure decision) ──────────────────────────────────────

/// The entries step 6 carries from `~/.claude.shared` into `~/.claude`:
/// the dirs csm's provisioning linked (projects, sessions, plugins), the
/// dirs the profile layout linked beside them (todos, session-env,
/// shell-snapshots) and claude's prompt history file.
pub(crate) const SHARED_NAMES: [&str; 7] = [
    "projects",
    "sessions",
    "plugins",
    "todos",
    "session-env",
    "shell-snapshots",
    "history.jsonl",
];

/// Per-profile content claude reads from its config dir that step 6 does
/// not carry (it is not shared between profiles): it stays in the profile
/// dir, and so in `<dir>.retired`, for the operator to move by hand.
/// `settings.json` is the one that matters most: its `statusLine` (csm's
/// weekly-cap switch path), hooks, plugins and permissions are not in
/// `~/.claude/settings.json` after the move, which the fleet owns (design
/// section 9), so csm reports and never writes it.
pub(crate) const PER_PROFILE_NAMES: [&str; 12] = [
    "settings.json",
    "settings.local.json",
    "CLAUDE.md",
    "hooks",
    "statusline-command.sh",
    "keybindings.json",
    "output-styles",
    "agents",
    "commands",
    "skills",
    "plans",
    "file-history",
];

/// What `~/.claude/<name>` is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LocalKind {
    Absent,
    /// A symlink into `~/.claude.shared/<name>`.
    LinkToShared,
    /// A symlink elsewhere.
    OtherLink,
    RealDir,
    RealFile,
    /// Anything else.
    Other,
}

/// What `~/.claude.shared/<name>` is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SharedKind {
    Absent,
    Dir,
    File,
    Other,
}

fn shared_kind(p: &Path) -> SharedKind {
    match std::fs::metadata(p) {
        Err(_) => SharedKind::Absent,
        Ok(m) if m.is_dir() => SharedKind::Dir,
        Ok(m) if m.is_file() => SharedKind::File,
        Ok(_) => SharedKind::Other,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SharedAction {
    Nothing,
    /// Remove the link, then move the shared dir into place.
    Unlink,
    /// Move the shared dir into place.
    Move,
    /// Move the shared dir's entries into the real dir; collisions stay.
    Drain,
    /// Both are files (the prompt history): the shared file's lines go in
    /// front of the local file's, then the shared file goes.
    Append,
    Skip(String),
}

/// Decide step 6 for one name. Pure.
pub(crate) fn shared_action(local: &LocalKind, shared: SharedKind) -> SharedAction {
    use SharedKind as S;
    match (local, shared) {
        (LocalKind::LinkToShared, S::Dir | S::File) => SharedAction::Unlink,
        (LocalKind::LinkToShared, _) => {
            SharedAction::Skip("links to a missing shared entry; remove the link by hand".into())
        }
        (_, S::Absent) => SharedAction::Nothing,
        (LocalKind::OtherLink, _) => {
            SharedAction::Skip("is a link outside ~/.claude.shared".into())
        }
        (_, S::Other) => SharedAction::Skip("the shared entry is neither a file nor a dir".into()),
        (LocalKind::Absent, _) => SharedAction::Move,
        (LocalKind::RealDir, S::Dir) => SharedAction::Drain,
        (LocalKind::RealFile, S::File) => SharedAction::Append,
        (LocalKind::RealDir, S::File) => {
            SharedAction::Skip("is a dir, the shared one a file".into())
        }
        (LocalKind::RealFile, S::Dir) => {
            SharedAction::Skip("is a file, the shared one a dir".into())
        }
        (LocalKind::Other, _) => SharedAction::Skip("is neither a file nor a dir".into()),
    }
}

/// The history file [`SharedAction::Append`] writes: `shared`'s lines,
/// then `local`'s. `None` when `local` already starts with `shared` (a
/// rerun after a crash between the write and the removal), so a rerun never
/// doubles the history. Pure.
pub(crate) fn appended(shared: &[u8], local: &[u8]) -> Option<Vec<u8>> {
    if local.starts_with(shared) {
        return None;
    }
    let mut out = shared.to_vec();
    if !out.is_empty() && !out.ends_with(b"\n") {
        out.push(b'\n');
    }
    out.extend_from_slice(local);
    Some(out)
}

fn shared_root(home: &Path) -> PathBuf {
    home.join(".claude.shared")
}

fn local_kind(local: &Path, shared: &Path) -> LocalKind {
    let Ok(md) = std::fs::symlink_metadata(local) else {
        return LocalKind::Absent;
    };
    if md.file_type().is_symlink() {
        let target = std::fs::read_link(local).unwrap_or_default();
        let target = if target.is_absolute() {
            target
        } else {
            local.parent().map(|p| p.join(&target)).unwrap_or(target)
        };
        let same = std::fs::canonicalize(&target).ok() == std::fs::canonicalize(shared).ok()
            && shared.exists();
        return if same || target == shared {
            LocalKind::LinkToShared
        } else {
            LocalKind::OtherLink
        };
    }
    if md.is_dir() {
        LocalKind::RealDir
    } else if md.is_file() {
        LocalKind::RealFile
    } else {
        LocalKind::Other
    }
}

fn shared_plan(home: &Path) -> Vec<(&'static str, SharedAction)> {
    let d = home.join(".claude");
    let sh = shared_root(home);
    SHARED_NAMES
        .iter()
        .map(|n| {
            let (l, s) = (d.join(n), sh.join(n));
            (*n, shared_action(&local_kind(&l, &s), shared_kind(&s)))
        })
        .collect()
}

/// What a profile dir holds of its own that step 6 does not carry: the
/// [`PER_PROFILE_NAMES`] that are not a link into `~/.claude.shared` (or,
/// after step 6 repointed it, into `~/.claude`), and any [`SHARED_NAMES`]
/// entry that is a real file or dir instead of the usual link, whose
/// content (transcripts, history) step 6 never merges.
fn left_behind(home: &Path, dir: &Path) -> Vec<String> {
    let sh = shared_root(home);
    let d = home.join(".claude");
    let linked_to = |p: &Path, n: &str| {
        local_kind(p, &sh.join(n)) == LocalKind::LinkToShared
            || local_kind(p, &d.join(n)) == LocalKind::LinkToShared
    };
    let own = PER_PROFILE_NAMES
        .iter()
        .filter(|n| {
            let p = dir.join(n);
            std::fs::symlink_metadata(&p).is_ok() && !linked_to(&p, n)
        })
        .map(|n| (*n).to_owned());
    let unshared = SHARED_NAMES
        .iter()
        .filter(|n| {
            std::fs::symlink_metadata(dir.join(n)).is_ok_and(|m| !m.file_type().is_symlink())
        })
        .map(|n| format!("{n} (its own, not the shared one: not merged)"));
    own.chain(unshared).collect()
}

// ─── step 6: plugin paths ─────────────────────────────────────────────────────

/// The plugin registries that record absolute install paths.
pub(crate) const PLUGIN_FILES: [&str; 2] = ["installed_plugins.json", "known_marketplaces.json"];

/// The keys whose values are absolute paths under a plugins dir.
const PLUGIN_PATH_KEYS: [&str; 2] = ["installPath", "installLocation"];

/// The old spellings of the plugins dir: every legacy profile dir's
/// `plugins` (the links claude recorded paths through) and
/// `~/.claude.shared/plugins`, each with a trailing separator. On Windows,
/// where a recorded path may use either separator, each base also comes
/// with every backslash spelled `/`.
fn old_plugin_prefixes(home: &Path, dirs: &[PathBuf]) -> Vec<String> {
    let mut out = Vec::new();
    for d in dirs
        .iter()
        .cloned()
        .chain(std::iter::once(shared_root(home)))
    {
        let native = d.join("plugins").to_string_lossy().into_owned();
        let mut bases = vec![native.clone()];
        if cfg!(windows) {
            bases.push(native.replace('\\', "/"));
        }
        for base in bases {
            for sep in ['/', '\\'] {
                let p = format!("{}{sep}", base.trim_end_matches(['/', '\\']));
                if !out.contains(&p) {
                    out.push(p);
                }
            }
        }
    }
    out
}

/// What [`rewrite_plugin_paths`] found.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct PluginRewrite {
    /// (old, new) paths rewritten.
    pub rewritten: Vec<(String, String)>,
    /// Old paths whose rewritten form does not exist: left as they are.
    pub dangling: Vec<String>,
}

/// Rewrite every `installPath`/`installLocation` under an old prefix to the
/// same path under `new_root` (with `sep`), when `exists` says the new path
/// is there. Pure apart from `exists`.
pub(crate) fn rewrite_plugin_paths(
    v: &mut Value,
    prefixes: &[String],
    new_root: &str,
    exists: &dyn Fn(&str) -> bool,
) -> PluginRewrite {
    let mut out = PluginRewrite::default();
    walk_plugin_paths(v, prefixes, new_root, exists, &mut out);
    out
}

fn walk_plugin_paths(
    v: &mut Value,
    prefixes: &[String],
    new_root: &str,
    exists: &dyn Fn(&str) -> bool,
    out: &mut PluginRewrite,
) {
    match v {
        Value::Object(m) => {
            for (k, val) in m.iter_mut() {
                if PLUGIN_PATH_KEYS.contains(&k.as_str())
                    && let Value::String(s) = val
                {
                    let Some((p, rest)) = prefixes
                        .iter()
                        .find_map(|p| s.strip_prefix(p.as_str()).map(|r| (p, r)))
                    else {
                        continue;
                    };
                    let sep = p.chars().last().unwrap_or('/');
                    let new = format!("{}{sep}{rest}", new_root.trim_end_matches(['/', '\\']));
                    if exists(&new) {
                        out.rewritten.push((s.clone(), new.clone()));
                        *s = new;
                    } else {
                        out.dangling.push(s.clone());
                    }
                } else {
                    walk_plugin_paths(val, prefixes, new_root, exists, out);
                }
            }
        }
        Value::Array(a) => a
            .iter_mut()
            .for_each(|x| walk_plugin_paths(x, prefixes, new_root, exists, out)),
        _ => {}
    }
}

/// The plugins dir step 6 reads the registries from: `~/.claude/plugins`
/// when it is a real dir already, else the shared one (plan time).
fn plugins_dir_now(home: &Path) -> PathBuf {
    let d = home.join(".claude").join("plugins");
    match std::fs::symlink_metadata(&d) {
        Ok(m) if m.is_dir() => d,
        _ => shared_root(home).join("plugins"),
    }
}

/// Plan time: how many recorded plugin paths go through an old prefix.
fn plugin_paths_preview(home: &Path, dirs: &[PathBuf]) -> usize {
    let prefixes = old_plugin_prefixes(home, dirs);
    let root = plugins_dir_now(home);
    PLUGIN_FILES
        .iter()
        .filter_map(|f| read_json_object(&root.join(f)))
        .map(|m| {
            let mut v = Value::Object(m);
            rewrite_plugin_paths(&mut v, &prefixes, "/", &|_| true)
                .rewritten
                .len()
        })
        .sum()
}

/// After the plugins dir moved into `~/.claude/plugins`: point the
/// registries' absolute paths at it, keeping a pre-image of each file
/// changed in `<state>/migrate/`. A path whose new form does not exist is
/// left as it is and reported. Returns one line per file touched.
fn apply_plugin_paths(home: &Path, dirs: &[PathBuf], state: &Path) -> io::Result<Vec<String>> {
    let root = home.join(".claude").join("plugins");
    match std::fs::symlink_metadata(&root) {
        Ok(m) if m.is_dir() && !m.file_type().is_symlink() => {}
        _ => return Ok(Vec::new()),
    }
    let prefixes = old_plugin_prefixes(home, dirs);
    let new_root = root.to_string_lossy().into_owned();
    let mut lines = Vec::new();
    for f in PLUGIN_FILES {
        let path = root.join(f);
        let Some(before) = crate::orca::read_capped_bytes(&path, 64 * 1024 * 1024)? else {
            continue;
        };
        let Ok(mut v @ Value::Object(_)) = serde_json::from_slice::<Value>(&before) else {
            lines.push(format!("{}: not a JSON object; left as is", path.display()));
            continue;
        };
        let r = rewrite_plugin_paths(&mut v, &prefixes, &new_root, &|p| Path::new(p).exists());
        if !r.rewritten.is_empty() {
            let pre_dir = state.join("migrate");
            fsx::create_dir_all(&pre_dir, 0o700)?;
            let pre = pre_dir.join(format!(
                "{f}.{}.pre",
                chrono::Utc::now().format("%Y%m%dT%H%M%SZ")
            ));
            fsx::write_atomic(&pre, &before, fsx::WriteOpts::PRIVATE)?;
            let mut text = serde_json::to_vec_pretty(&v).map_err(io::Error::other)?;
            if before.ends_with(b"\n") {
                text.push(b'\n');
            }
            fsx::guard(&path)?;
            fsx::write_atomic(&path, &text, fsx::WriteOpts::PRIVATE)?;
        }
        if !r.rewritten.is_empty() || !r.dangling.is_empty() {
            let mut line = format!(
                "{}: {} path(s) now under {}",
                path.display(),
                r.rewritten.len(),
                root.display()
            );
            if !r.dangling.is_empty() {
                line.push_str(&format!(
                    "; {} left pointing at an old dir whose new path does not exist (reinstall those with /plugin): {}",
                    r.dangling.len(),
                    r.dangling.join(", ")
                ));
            }
            lines.push(line);
        }
    }
    Ok(lines)
}

// ─── step 6: csm's old state dir ──────────────────────────────────────────────

/// `~/.claude.shared/smart`, csm's state dir under the profile layout.
fn legacy_smart_dir(home: &Path) -> PathBuf {
    shared_root(home).join("smart")
}

/// Does a top-level entry of the old state dir move to the new one? Only
/// what csm still reads in the same form: session sidecars
/// (`<session-uuid>.json`), the title index `titles.tsv` and the scan
/// indexes `scan-meta-v2.*.tsv`. Caches, cooldown and pid markers and the
/// per-profile usage records (keyed by profile names csm no longer has)
/// stay behind unread. Pure.
pub(crate) fn smart_carries(name: &str) -> bool {
    name == "titles.tsv"
        || (name.starts_with("scan-meta-v2.") && name.ends_with(".tsv"))
        || name
            .strip_suffix(".json")
            .is_some_and(crate::session::alias::looks_like_uuid)
}

/// Step 6's count over the old state dir: (entries that move, entries that
/// stay). `None` when there is no old dir.
fn smart_preview(old: &Path) -> Option<(usize, usize)> {
    let rd = std::fs::read_dir(old).ok()?;
    let (mut carry, mut stay) = (0, 0);
    for e in rd.filter_map(Result::ok) {
        let is_file = e.file_type().is_ok_and(|t| t.is_file());
        match e.file_name().to_str() {
            Some(n) if is_file && smart_carries(n) => carry += 1,
            _ => stay += 1,
        }
    }
    Some((carry, stay))
}

/// Move the carried entries of `old` into `new`. An entry `new` already has
/// stays in `old` unless the bytes are equal. Returns one line, `None` when
/// there is no old dir.
fn apply_smart(old: &Path, new: &Path) -> io::Result<Option<String>> {
    if !old.is_dir() {
        return Ok(None);
    }
    let same = matches!(
        (std::fs::canonicalize(old), std::fs::canonicalize(new)),
        (Ok(a), Ok(b)) if a == b
    );
    if same {
        return Ok(None);
    }
    fsx::create_dir_all(new, 0o700)?;
    let (mut moved, mut collided, mut stayed) = (0usize, 0usize, 0usize);
    for e in std::fs::read_dir(old)? {
        let e = e?;
        let is_file = e.file_type().is_ok_and(|t| t.is_file());
        let Some(name) = e.file_name().to_str().map(str::to_owned) else {
            stayed += 1;
            continue;
        };
        if !is_file || !smart_carries(&name) {
            stayed += 1;
            continue;
        }
        let (src, dst) = (e.path(), new.join(&name));
        match std::fs::symlink_metadata(&dst) {
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                move_path(&src, &dst)?;
                moved += 1;
            }
            Err(err) => return Err(err),
            Ok(md) if md.is_file() && std::fs::read(&src)? == std::fs::read(&dst)? => {
                remove_tree(&src)?;
                moved += 1;
            }
            Ok(_) => collided += 1,
        }
    }
    let mut line = format!(
        "{}: moved {moved} session file(s) into {}",
        old.display(),
        new.display()
    );
    if collided > 0 {
        line.push_str(&format!("; {collided} already there and kept in both"));
    }
    if stayed > 0 {
        line.push_str(&format!(
            "; {stayed} other entr{} (caches, markers, per-profile records) stay unread; remove the dir by hand",
            if stayed == 1 { "y" } else { "ies" }
        ));
    }
    Ok(Some(line))
}

// ─── filesystem moves ─────────────────────────────────────────────────────────

/// Rename `src` to `dst`; across filesystems, copy, verify, then remove.
fn move_path(src: &Path, dst: &Path) -> io::Result<()> {
    fsx::guard(src)?;
    fsx::guard(dst)?;
    match std::fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
            copy_tree(src, dst)?;
            verify_tree(src, dst)?;
            remove_tree(src)
        }
        Err(e) => Err(e),
    }
}

fn copy_tree(src: &Path, dst: &Path) -> io::Result<()> {
    let md = std::fs::symlink_metadata(src)?;
    if md.file_type().is_symlink() {
        #[cfg(unix)]
        return std::os::unix::fs::symlink(std::fs::read_link(src)?, dst);
        #[cfg(not(unix))]
        return std::fs::copy(src, dst).map(|_| ());
    }
    if md.is_dir() {
        std::fs::create_dir(dst)?;
        for e in std::fs::read_dir(src)? {
            let e = e?;
            copy_tree(&e.path(), &dst.join(e.file_name()))?;
        }
        return Ok(());
    }
    std::fs::copy(src, dst).map(|_| ())
}

fn verify_tree(src: &Path, dst: &Path) -> io::Result<()> {
    let md = std::fs::symlink_metadata(src)?;
    let bad = || {
        io::Error::other(format!(
            "copy of {} does not match; the source is kept",
            src.display()
        ))
    };
    if md.file_type().is_symlink() {
        return (std::fs::read_link(src)? == std::fs::read_link(dst)?)
            .then_some(())
            .ok_or_else(bad);
    }
    if md.is_dir() {
        for e in std::fs::read_dir(src)? {
            let e = e?;
            verify_tree(&e.path(), &dst.join(e.file_name()))?;
        }
        return Ok(());
    }
    (std::fs::read(src)? == std::fs::read(dst)?)
        .then_some(())
        .ok_or_else(bad)
}

fn remove_tree(p: &Path) -> io::Result<()> {
    fsx::guard(p)?;
    let md = std::fs::symlink_metadata(p)?;
    if md.is_dir() && !md.file_type().is_symlink() {
        std::fs::remove_dir_all(p)
    } else {
        std::fs::remove_file(p)
    }
}

/// What [`drain`] does with one entry of `src`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DrainFate {
    /// `dst` has nothing there: move it.
    Move,
    /// Both are real dirs: recurse.
    Recurse,
    /// Both are files with the same bytes: drop the source copy.
    RemoveSame,
    /// Anything else: it stays in `src`.
    Collide,
}

/// Decide [`DrainFate`] for `s` (the source entry) and `d` (its place in
/// the destination). Reads only.
fn drain_fate(s: &Path, d: &Path) -> io::Result<DrainFate> {
    let smd = std::fs::symlink_metadata(s)?;
    let dmd = match std::fs::symlink_metadata(d) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(DrainFate::Move),
        Err(err) => return Err(err),
        Ok(m) => m,
    };
    let both_dirs = smd.is_dir()
        && dmd.is_dir()
        && !smd.file_type().is_symlink()
        && !dmd.file_type().is_symlink();
    Ok(if both_dirs {
        DrainFate::Recurse
    } else if smd.is_file() && dmd.is_file() && std::fs::read(s)? == std::fs::read(d)? {
        DrainFate::RemoveSame
    } else {
        DrainFate::Collide
    })
}

/// Move `src`'s entries into `dst`, recursing into dirs both hold. An
/// entry `dst` already has stays in `src` unless the files are identical.
/// Returns the paths left behind.
fn drain(src: &Path, dst: &Path) -> io::Result<Vec<PathBuf>> {
    let mut left = Vec::new();
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let (s, d) = (e.path(), dst.join(e.file_name()));
        match drain_fate(&s, &d)? {
            DrainFate::Move => move_path(&s, &d)?,
            DrainFate::Recurse => left.extend(drain(&s, &d)?),
            DrainFate::RemoveSame => remove_tree(&s)?,
            DrainFate::Collide => left.push(s),
        }
    }
    if left.is_empty() {
        fsx::guard(src)?;
        let _ = std::fs::remove_dir(src);
    }
    Ok(left)
}

/// What [`drain`] would do now (read-only).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct DrainScan {
    /// It would still move or remove something.
    pub moves: bool,
    /// The entries it would leave in `src`: a rerun never resolves them.
    pub collided: Vec<PathBuf>,
}

/// [`drain`] without the writes: the same walk, the same decisions.
fn drain_scan(src: &Path, dst: &Path) -> io::Result<DrainScan> {
    let mut scan = DrainScan::default();
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let (s, d) = (e.path(), dst.join(e.file_name()));
        match drain_fate(&s, &d)? {
            DrainFate::Move | DrainFate::RemoveSame => scan.moves = true,
            DrainFate::Recurse => {
                let sub = drain_scan(&s, &d)?;
                scan.moves |= sub.moves;
                scan.collided.extend(sub.collided);
            }
            DrainFate::Collide => scan.collided.push(s),
        }
    }
    // With nothing collided, drain removes `src` itself.
    if scan.collided.is_empty() {
        scan.moves = true;
    }
    Ok(scan)
}

/// The paths a drain left, for the operator to merge by hand. Pure.
fn collided_list(left: &[PathBuf]) -> String {
    const SHOWN: usize = 10;
    let mut v: Vec<String> = left
        .iter()
        .take(SHOWN)
        .map(|p| p.display().to_string())
        .collect();
    if left.len() > SHOWN {
        v.push(format!("and {} more", left.len() - SHOWN));
    }
    v.join(", ")
}

/// Apply step 6. Returns one line per name.
fn apply_shared(home: &Path) -> io::Result<Vec<String>> {
    let d = home.join(".claude");
    let sh = shared_root(home);
    let mut out = Vec::new();
    for (name, action) in shared_plan(home) {
        let (l, s) = (d.join(name), sh.join(name));
        match action {
            SharedAction::Nothing => {}
            SharedAction::Skip(why) => out.push(format!("{}: {why}; left as is", l.display())),
            SharedAction::Unlink => {
                fsx::guard(&l)?;
                std::fs::remove_file(&l)?;
                move_path(&s, &l)?;
                out.push(format!("{}: now a real dir", l.display()));
            }
            SharedAction::Move => {
                fsx::create_dir_all(&d, 0o700)?;
                move_path(&s, &l)?;
                out.push(format!("{}: moved in from {}", l.display(), s.display()));
            }
            SharedAction::Append => {
                let (sb, lb) = (std::fs::read(&s)?, std::fs::read(&l)?);
                if let Some(text) = appended(&sb, &lb) {
                    fsx::guard(&l)?;
                    fsx::write_atomic(&l, &text, fsx::WriteOpts::PRIVATE)?;
                }
                remove_tree(&s)?;
                out.push(format!(
                    "{}: {} put in front of it",
                    l.display(),
                    s.display()
                ));
            }
            SharedAction::Drain => {
                let left = drain(&s, &l)?;
                if left.is_empty() {
                    out.push(format!("{}: drained {}", l.display(), s.display()));
                } else {
                    out.push(format!(
                        "{}: drained {}; {} entr{} collided and stayed (a rerun leaves them too; \
                         merge or delete them by hand): {}",
                        l.display(),
                        s.display(),
                        left.len(),
                        if left.len() == 1 { "y" } else { "ies" },
                        collided_list(&left)
                    ));
                }
            }
        }
    }
    Ok(out)
}

// ─── step 6: the profile dirs' own links ─────────────────────────────────────

/// `p` with `.` dropped and each `..` taking off the component before it,
/// without touching the filesystem (the link target it normalizes may be
/// gone). Pure.
#[cfg(any(unix, test))]
fn lexical(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Does the link text `target`, read from a link in `dir`, name `shared`?
/// Compared by text (made absolute and normalized), not by resolving it,
/// because step 6 moved the target away. Pure.
#[cfg(any(unix, test))]
pub(crate) fn names_shared(dir: &Path, target: &Path, shared: &Path) -> bool {
    let abs = if target.is_absolute() {
        target.to_path_buf()
    } else {
        dir.join(target)
    };
    lexical(&abs) == lexical(shared)
}

/// After step 6, repoint every legacy profile dir's link into
/// `~/.claude.shared/<name>` at `~/.claude/<name>`, where step 6 put the
/// real entry. Left alone, those links dangle between import and retire:
/// a claude started with a stale `CLAUDE_CONFIG_DIR` (a multiplexer server
/// keeps one) then loses its transcript, writes its history into a new
/// `~/.claude.shared/history.jsonl`, and cannot register in `sessions/`, so
/// retire's live-session gate cannot see it. Repointed, it writes into the
/// one real dir and registers where the gate looks. Each link is replaced
/// by a temp link and a rename, so it is never missing; a rerun repoints
/// links an earlier run left dangling and changes nothing else. Unix only:
/// a Windows symlink needs a privilege csm does not assume, and csm never
/// made the legacy links there.
#[cfg(unix)]
fn repoint_profile_links(home: &Path, dirs: &[PathBuf]) -> io::Result<Vec<String>> {
    let d = home.join(".claude");
    let sh = shared_root(home);
    let mut out = Vec::new();
    for dir in dirs {
        for name in SHARED_NAMES {
            let link = dir.join(name);
            let is_link = std::fs::symlink_metadata(&link)
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false);
            if !is_link {
                continue;
            }
            let Ok(text) = std::fs::read_link(&link) else {
                continue;
            };
            if !names_shared(dir, &text, &sh.join(name)) {
                continue;
            }
            let new_target = d.join(name);
            // Only where step 6 left a real entry to point at.
            match std::fs::symlink_metadata(&new_target) {
                Ok(m) if !m.file_type().is_symlink() => {}
                _ => continue,
            }
            let tmp = dir.join(format!(".{name}.csm-relink.{}", std::process::id()));
            fsx::guard(&link)?;
            fsx::guard(&tmp)?;
            let _ = std::fs::remove_file(&tmp);
            std::os::unix::fs::symlink(&new_target, &tmp)?;
            if let Err(e) = std::fs::rename(&tmp, &link) {
                let _ = std::fs::remove_file(&tmp);
                return Err(e);
            }
            out.push(format!(
                "{}: now links to {}",
                link.display(),
                new_target.display()
            ));
        }
    }
    Ok(out)
}

#[cfg(not(unix))]
fn repoint_profile_links(_home: &Path, _dirs: &[PathBuf]) -> io::Result<Vec<String>> {
    Ok(Vec::new())
}

// ─── the write gate (pure) ────────────────────────────────────────────────────

/// A `CLAUDE_CONFIG_DIR` value that is set at all, trimmed and without a
/// trailing separator. Pure.
fn set_config_dir(value: Option<&str>) -> Option<&Path> {
    let d = value.map(str::trim).filter(|d| !d.is_empty())?;
    Some(Path::new(d.trim_end_matches(['/', '\\'])))
}

/// Why a set `CLAUDE_CONFIG_DIR` naming `~/.claude` still refuses. Claude
/// Code reads `$CLAUDE_CONFIG_DIR/.claude.json` whenever the variable is
/// set, and Orca's resolveConfigPath (runtime-paths.ts) does the same, and
/// keeps doing it once that file exists. Step 5 merges into `~/.claude.json`
/// (the file an unset variable means), while step 7's switch would create
/// `~/.claude/.claude.json` holding only `oauthAccount`: the two files would
/// then stay split for good (identity in one, onboarding, trust and MCP
/// servers in the other).
const SET_TO_DEFAULT_WHY: &str = "with it set, claude and Orca read ~/.claude/.claude.json instead of \
     the ~/.claude.json this migration merges into";

/// Refuse a write step. `claude_config_dir` is this process's value;
/// `session_floor` is the login session's (the launchd variable on macOS,
/// `HKCU\Environment` on Windows), which Orca and every GUI- or
/// launchd-started process inherit even when this shell does not (an ssh
/// session, a manual `unset`). Either being set refuses, even when it
/// names `~/.claude` (see [`SET_TO_DEFAULT_WHY`]). Pure.
pub(crate) fn write_gate(
    orca_running: bool,
    live_in: &[PathBuf],
    claude_config_dir: Option<&str>,
    session_floor: Option<&str>,
    home: &Path,
    version_ok: bool,
) -> Result<(), String> {
    if orca_running {
        return Err("Orca is running; end every claude pane and quit Orca first".into());
    }
    if let Some(d) = live_in.first() {
        return Err(format!(
            "a claude session is live in {}; end it first (`csm reap` finds orphans)",
            d.display()
        ));
    }
    let default_dir = home.join(".claude");
    if let Some(d) = set_config_dir(claude_config_dir) {
        return Err(if d == default_dir {
            format!(
                "CLAUDE_CONFIG_DIR is set (to {}); {SET_TO_DEFAULT_WHY}. Remove the shell export \
                 (a leftover `csm cas` shim in ~/.zshenv or profile.ps1 sets it) and open a fresh shell",
                d.display()
            )
        } else {
            format!(
                "CLAUDE_CONFIG_DIR is still {}; drop the floor and open a fresh shell",
                d.display()
            )
        });
    }
    if let Some(d) = set_config_dir(session_floor) {
        return Err(if d == default_dir {
            format!(
                "the login session's CLAUDE_CONFIG_DIR is set (to {}), and Orca would start with \
                 it; {SET_TO_DEFAULT_WHY}. Clear it (`{FLOOR_UNSET_HINT}`) first",
                d.display()
            )
        } else {
            format!(
                "the login session's CLAUDE_CONFIG_DIR is still {}, and Orca would start with it \
                 as its D; drop the floor (`{FLOOR_UNSET_HINT}`) first, and remove any login item \
                 or LaunchAgent that sets it again at login",
                d.display()
            )
        });
    }
    if !version_ok {
        return Err("the installed Orca version is not one csm was tested with".into());
    }
    Ok(())
}

/// The write gate over the real machine. It does not look at the store's
/// backend: a SQLite-backed Orca profile (1.4.214+) only defers the steps
/// that patch Orca's store ([`row_action`], [`floor_action`]), which then
/// run over RPC once Orca is started. Refusing the whole import there made
/// it impossible (this gate needs Orca stopped, the SQLite one needed it
/// running), although steps 5 and 6 and the read-backs never touch the
/// store.
fn gate(ctx: &Context, procs: &dyn ProcFacts, legacy: &Legacy) -> anyhow::Result<()> {
    let home = &ctx.env.home;
    let mut dirs: Vec<PathBuf> = legacy.profiles.iter().map(|p| p.dir.clone()).collect();
    dirs.push(home.join(".claude"));
    let live: Vec<PathBuf> = dirs
        .into_iter()
        .filter(|d| d.is_dir() && live_claude_in(ctx.os(), d, procs))
        .collect();
    let floor = session_floor().map_err(|e| {
        anyhow::anyhow!("csm migrate: cannot read the session's CLAUDE_CONFIG_DIR: {e}")
    })?;
    write_gate(
        ctx.orca_running(procs),
        &live,
        ctx.env.claude_config_dir.as_deref(),
        floor.as_deref(),
        home,
        ctx.version_ok,
    )
    .map_err(|e| anyhow::anyhow!("csm migrate: {e}"))?;
    if cfg!(target_os = "macos") {
        agent_gate(&floor_agents(home)).map_err(|e| anyhow::anyhow!("csm migrate: {e}"))?;
    }
    if let Some(why) = supervision(home, procs) {
        bail!("csm migrate: {why}");
    }
    Ok(())
}

// ─── the write gate: csm supervisors ──────────────────────────────────────────

/// How long after the recorded `born` a supervised claude may have
/// started: the old launcher stamped `born` right before the spawn.
const BORN_SLACK_SECS: i64 = 5;

/// A claude an earlier csm started and still supervises: the first pid of
/// the old state dir's `<sid>.pid` files (`(pid, born)`) that still runs
/// and started at or within [`BORN_SLACK_SECS`] after `born`, so a reused
/// pid does not match. `csm reap` reads the new state dir only and does not
/// see these. Pure over `start_time`.
pub(crate) fn legacy_supervised_child(
    pidfiles: &[(u32, i64)],
    start_time: impl Fn(u32) -> Option<u64>,
) -> Option<u32> {
    pidfiles.iter().find_map(|&(pid, born)| {
        let started = i64::try_from(start_time(pid)?).ok()?;
        (pid != 0 && started >= born - 1 && started <= born + BORN_SLACK_SECS).then_some(pid)
    })
}

/// Another csm process: one whose executable is `csm` (the `claude` alias
/// runs the same binary), other than this process and its ancestors. A
/// `csm run` supervisor between two relaunch hops has no claude registered
/// anywhere, and would start one into a legacy dir or `~/.claude` while
/// the migration moves them. Pure.
pub(crate) fn other_csm(table: &[crate::platform::proc::ProcInfo], this: u32) -> Option<u32> {
    let parent = |pid: u32| table.iter().find(|p| p.pid == pid).and_then(|p| p.ppid);
    let mut mine = vec![this];
    let mut at = this;
    while let Some(pp) = parent(at) {
        if pp == 0 || mine.contains(&pp) || mine.len() > 64 {
            break;
        }
        mine.push(pp);
        at = pp;
    }
    let is_csm = |p: &crate::platform::proc::ProcInfo| {
        let stem = p
            .exe
            .as_deref()
            .and_then(Path::file_stem)
            .and_then(|s| s.to_str())
            .map(str::to_owned)
            .unwrap_or_else(|| crate::platform::proc_check::bare_basename(&p.name).to_owned());
        stem.eq_ignore_ascii_case("csm")
    };
    table
        .iter()
        .find(|p| !mine.contains(&p.pid) && is_csm(p))
        .map(|p| p.pid)
}

/// The `<sid>.pid` records under the old state dir.
fn legacy_pidfiles(home: &Path) -> Vec<(u32, i64)> {
    let Ok(rd) = std::fs::read_dir(legacy_smart_dir(home)) else {
        return Vec::new();
    };
    rd.filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("pid"))
        .filter_map(|p| crate::platform::pid::read_pid_file(&p).ok().flatten())
        .collect()
}

/// Why a write step must wait for a csm supervisor, if it must. An
/// unreadable process table adds nothing here: the Orca check above
/// already fails closed on it.
fn supervision(home: &Path, procs: &dyn ProcFacts) -> Option<String> {
    if let Some(pid) = legacy_supervised_child(&legacy_pidfiles(home), |p| procs.start_time(p)) {
        return Some(format!(
            "claude pid {pid}, started by an earlier csm (~/.claude.shared/smart), still runs; end it first"
        ));
    }
    let table = procs.table()?;
    other_csm(&table, std::process::id()).map(|pid| {
        format!(
            "another csm process (pid {pid}) runs; a `csm run` supervisor restarts claude between hops, so end it first"
        )
    })
}

// ─── the write gate: a floor set again at every login ─────────────────────────

/// Does this LaunchAgent set `CLAUDE_CONFIG_DIR` for the login session?
/// A heuristic over text, never an execution: the plist (XML or binary;
/// both keep ASCII strings as bytes), or a script one of its absolute
/// `<string>` paths names (`read` returns at most a small file's bytes),
/// holds both `CLAUDE_CONFIG_DIR` and `setenv`. A job's own
/// `EnvironmentVariables` alone does not count: it reaches only that job.
/// Pure over `read`.
pub(crate) fn agent_sets_floor(plist: &[u8], read: &dyn Fn(&Path) -> Option<Vec<u8>>) -> bool {
    fn has(b: &[u8], needle: &str) -> bool {
        b.windows(needle.len()).any(|w| w == needle.as_bytes())
    }
    let sets = |b: &[u8]| has(b, "CLAUDE_CONFIG_DIR") && has(b, "setenv");
    if sets(plist) {
        return true;
    }
    let text = String::from_utf8_lossy(plist);
    text.split("<string>")
        .skip(1)
        .filter_map(|rest| rest.split_once("</string>").map(|(v, _)| v.trim()))
        .filter(|v| v.starts_with('/'))
        .any(|v| read(Path::new(v)).is_some_and(|b| sets(&b)))
}

/// `~/Library/LaunchAgents/*.plist` that set `CLAUDE_CONFIG_DIR` again at
/// every login ([`agent_sets_floor`]). Unset once, such an agent puts the
/// floor back at the next login, pointing an Orca started from the Dock
/// at a profile dir retire renamed.
fn floor_agents(home: &Path) -> Vec<PathBuf> {
    const CAP: u64 = 256 * 1024;
    let read = |p: &Path| -> Option<Vec<u8>> {
        let m = std::fs::metadata(p).ok()?;
        (m.is_file() && m.len() <= CAP).then(|| std::fs::read(p).ok())?
    };
    let Ok(rd) = std::fs::read_dir(home.join("Library").join("LaunchAgents")) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = rd
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "plist"))
        .filter(|p| read(p).is_some_and(|b| agent_sets_floor(&b, &read)))
        .collect();
    out.sort();
    out
}

/// Refuse while a LaunchAgent would set the floor again (macOS only). Pure.
pub(crate) fn agent_gate(agents: &[PathBuf]) -> Result<(), String> {
    if agents.is_empty() {
        return Ok(());
    }
    let names: Vec<String> = agents.iter().map(|p| p.display().to_string()).collect();
    Err(format!(
        "a LaunchAgent sets CLAUDE_CONFIG_DIR again at every login ({}); unload and remove it \
         first (`launchctl bootout gui/$(id -u) <plist>`, then delete the file), or the next \
         login points Orca at a profile dir retire renames",
        names.join(", ")
    ))
}

/// The login session's `CLAUDE_CONFIG_DIR` (the value [`unset_floor_env`]
/// clears). `None` under `cfg(test)`; the e2e build reads
/// `CSM_E2E_SESSION_FLOOR` instead of the real session.
fn session_floor() -> io::Result<Option<String>> {
    if cfg!(test) {
        return Ok(None);
    }
    if crate::e2e::ENABLED {
        return Ok(std::env::var("CSM_E2E_SESSION_FLOOR")
            .ok()
            .filter(|s| !s.trim().is_empty()));
    }
    session_floor_impl()
}

#[cfg(target_os = "macos")]
fn session_floor_impl() -> io::Result<Option<String>> {
    let out = std::process::Command::new("/bin/launchctl")
        .args(["getenv", "CLAUDE_CONFIG_DIR"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()?;
    if !out.status.success() {
        return Err(io::Error::other(format!(
            "launchctl getenv exited with {}",
            out.status
        )));
    }
    let v = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    Ok((!v.is_empty()).then_some(v))
}

#[cfg(windows)]
fn session_floor_impl() -> io::Result<Option<String>> {
    use windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND;
    use windows_sys::Win32::System::Registry::{
        HKEY_CURRENT_USER, RRF_NOEXPAND, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ, RegGetValueW,
    };
    let key: Vec<u16> = "Environment\0".encode_utf16().collect();
    let name: Vec<u16> = "CLAUDE_CONFIG_DIR\0".encode_utf16().collect();
    let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND;
    let mut len: u32 = 0;
    // SAFETY: NUL-terminated wide strings; a null buffer asks for the size.
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            flags,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut len,
        )
    };
    if rc == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    if rc != 0 {
        return Err(io::Error::other(format!(
            "RegGetValueW failed (0x{rc:08X})"
        )));
    }
    let mut buf = vec![0u16; (len as usize).div_ceil(2) + 1];
    let mut len = (buf.len() * 2) as u32;
    // SAFETY: `buf` holds `len` bytes.
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            flags,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut len,
        )
    };
    if rc == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    if rc != 0 {
        return Err(io::Error::other(format!(
            "RegGetValueW failed (0x{rc:08X})"
        )));
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let v = String::from_utf16_lossy(&buf[..end]).trim().to_owned();
    Ok((!v.is_empty()).then_some(v))
}

/// Linux has no session-wide floor csm set (the legacy fleet exported it
/// from shell rc files, which the process-env check covers).
#[cfg(all(unix, not(target_os = "macos")))]
fn session_floor_impl() -> io::Result<Option<String>> {
    Ok(None)
}

// ─── store-writing steps on a SQLite profile (pure) ───────────────────────────

/// What step 4 does with one plan row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RowAction {
    /// `accounts import` offline (patches the store).
    Import,
    /// Offline read-back into the account's stash (no store write).
    ReadBack(String),
    /// The import patches a SQLite-backed store, which csm changes only
    /// through a running Orca: left for `csm accounts import <dir>` then.
    Defer,
    Skip,
}

/// Step 4's action for a row; `sqlite`: the profile keeps its state in
/// SQLite. Pure.
pub(crate) fn row_action(status: &Status, sqlite: bool) -> RowAction {
    match status {
        Status::ToImport if sqlite => RowAction::Defer,
        Status::ToImport => RowAction::Import,
        Status::InOrca {
            id,
            fresher: Some(true),
        } => RowAction::ReadBack(id.clone()),
        _ => RowAction::Skip,
    }
}

/// Why step 4 must not act on a row: a Keychain item of its dir could not
/// be read (a locked Keychain, a `security` timeout), so its freshest
/// grant is unknown. Importing, reading back or keeping the stash on that
/// partial read could leave the dir's newer grant behind; the row counts
/// as failed instead, which also keeps step 7's floor switch (whose
/// refresh rotates the stash's refresh token) from running. Pure.
pub(crate) fn unreadable_row(facts: &ProfileFacts) -> Option<&'static str> {
    facts
        .grant_sources
        .contains(&KEYCHAIN_UNREADABLE)
        .then_some("a Keychain item of the dir could not be read (is the Keychain locked?); nothing done, rerun when it reads")
}

/// Pure: the plan rows step 4 must classify again right before acting,
/// against Orca's list as it is then: a `ToImport` row whose identity an
/// earlier `ToImport` row shares. The plan was built before any import, so
/// once the earlier row adds the account, importing this one would be
/// refused as a duplicate; classified again it is `InOrca`, and read back
/// only when its grant is fresher.
pub(crate) fn repeat_imports(rows: &[Row]) -> Vec<bool> {
    let mut seen: Vec<IdentityKey> = Vec::new();
    rows.iter()
        .map(|r| {
            if r.status != Status::ToImport {
                return false;
            }
            let Some(key) = identity_key(&r.facts) else {
                return false;
            };
            if seen.contains(&key) {
                return true;
            }
            seen.push(key);
            false
        })
        .collect()
}

/// The line for a deferred import. Pure.
pub(crate) fn deferred_import_line(dir: &Path) -> String {
    format!(
        "deferred: Orca keeps its state in SQLite, so csm adds accounts only through it; \
         start Orca and run `csm accounts import {}` (before `csm migrate retire`)",
        dir.display()
    )
}

/// What step 7 (the switch to the floor profile's account) does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FloorAction {
    /// Switch offline now.
    Switch(String),
    /// A SQLite profile: the switch must go through Orca; this is the line
    /// telling the operator what to run once Orca is started.
    Instruct(String),
    /// The floor profile has no Orca account (yet).
    NoAccount,
}

/// Step 7's action from the floor profile's status. Pure.
pub(crate) fn floor_action(floor: &str, status: &Status, sqlite: bool) -> FloorAction {
    match status {
        Status::InOrca { id, .. } if sqlite => FloorAction::Instruct(format!(
            "Orca keeps its state in SQLite: once Orca is started, run `csm accounts use {id}` \
             to switch to {floor}'s account"
        )),
        Status::InOrca { id, .. } => FloorAction::Switch(id.clone()),
        _ => FloorAction::NoAccount,
    }
}

/// What `migrate import --dry-run` says step 7 will do, from the floor
/// profile's name and plan status (`None`: no floor profile). Pure.
pub(crate) fn step7_dry_run(
    floor: Option<&str>,
    status: Option<&Status>,
    sqlite: bool,
) -> Vec<String> {
    const ATTRIBUTE: &str = "attribute ~/.claude's grants (profile check) and remove its \
         oauthAccount, so Orca's first start cannot file them under another account \
         (nothing to do when Orca's store names no active account)";
    let action = match (floor, status) {
        (Some(f), Some(st)) => Some((f, floor_action(f, st, sqlite))),
        (Some(f), None) => Some((f, FloorAction::NoAccount)),
        (None, _) => None,
    };
    match action {
        None => vec![ATTRIBUTE.to_owned()],
        Some((f, FloorAction::Switch(id))) => {
            vec![format!("switch to the floor profile {f}'s account ({id})")]
        }
        Some((f, FloorAction::Instruct(_))) => vec![
            format!(
                "not switch to the floor profile {f}'s account: Orca keeps its state in \
                 SQLite; run `csm accounts use` once Orca is started"
            ),
            ATTRIBUTE.to_owned(),
        ],
        Some((f, FloorAction::NoAccount)) => vec![
            format!("not switch: the floor profile {f} has no Orca account"),
            ATTRIBUTE.to_owned(),
        ],
    }
}

/// Step 7's line for [`switch::attribute_offline`], run when the floor
/// switch cannot run offline. `Err` fails the import: Orca must not start
/// before ~/.claude's grants are attributed (a profile endpoint that did
/// not answer included). Pure.
pub(crate) fn attribution_line(
    r: Result<switch::Attribution, String>,
) -> Result<Option<String>, String> {
    match r {
        Err(e) => Err(format!(
            "cannot attribute ~/.claude's grants before Orca starts ({e}); run \
             `csm migrate import` again before starting Orca"
        )),
        Ok(switch::Attribution::NoActiveAccount) => Ok(None),
        Ok(switch::Attribution::Done {
            readback,
            identity_cleared,
        }) => {
            let mut s = match &readback.persisted {
                Some(id) => format!("~/.claude's newest grant went to stash {id}"),
                None => "~/.claude's grants: no stash changed".to_owned(),
            };
            if !readback.quarantined.is_empty() {
                s.push_str(&format!(
                    "; {} grant(s) quarantined",
                    readback.quarantined.len()
                ));
            }
            if identity_cleared {
                s.push_str(
                    "; its oauthAccount was removed so Orca's first start matches its grants \
                     by refresh token only",
                );
            }
            Ok(Some(s))
        }
    }
}

// ─── plan ─────────────────────────────────────────────────────────────────────

/// One plan row.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Row {
    pub facts: ProfileFacts,
    pub status: Status,
    pub stash: Option<GrantFacts>,
    pub floor: bool,
}

/// The whole plan.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Plan {
    pub rows: Vec<Row>,
    pub orca_running: bool,
    pub orca_d: Option<PathBuf>,
    pub target_d: PathBuf,
    pub target_config: PathBuf,
    /// Step 5 additions from the floor profile.
    pub merge: Vec<String>,
    /// Step 5 seeds a new file with this many of the floor profile's keys.
    pub seeded: Option<usize>,
    /// Why step 5 cannot run: the floor profile's `.claude.json` cannot
    /// be read or is not a JSON object.
    pub merge_blocked: Option<String>,
    /// Keys other profiles hold that the merged file would still lack.
    pub differs: Vec<(String, Vec<String>)>,
    pub shared: Vec<(&'static str, SharedAction)>,
    /// Recorded plugin paths that go through an old profile's or the
    /// shared plugins dir (step 6 points them at `~/.claude/plugins`).
    pub plugin_paths: usize,
    /// Per profile, what [`left_behind`] found.
    pub left_behind: Vec<(String, Vec<String>)>,
    /// csm's new state dir, and step 6's (moving, staying) count over the
    /// old one.
    pub state_dir: PathBuf,
    pub smart: Option<(usize, usize)>,
}

fn build_plan(ctx: &Context, view: &OrcaView, legacy: &Legacy, probe: Probe) -> Plan {
    let host: Vec<AccountRecord> = view.host_accounts().cloned().collect();
    // A stash's grant is a secret too: read only when the grants are.
    let sg = |id: &str| match probe {
        Probe::Read => stash_grant(ctx, view, id),
        Probe::Presence => None,
    };
    let rows: Vec<Row> = legacy
        .profiles
        .iter()
        .map(|p| {
            let facts = profile_facts(ctx, p, probe);
            let status = classify(&facts, &host, &sg);
            let stash = match &status {
                Status::InOrca { id, .. } => sg(id),
                _ => None,
            };
            Row {
                floor: legacy.floor.as_deref() == Some(p.name.as_str()),
                facts,
                status,
                stash,
            }
        })
        .collect();
    let home = &ctx.env.home;
    let target = runtime_paths(None, home, |p| p.exists());
    let existing = target
        .config_path
        .exists()
        .then(|| read_json_object(&target.config_path).unwrap_or_default());
    let mut merged = existing.clone().unwrap_or_default();
    let mut merge = Vec::new();
    let mut seeded = None;
    let mut merge_blocked = None;
    if let Some(floor) = legacy
        .floor
        .as_ref()
        .and_then(|n| legacy.profiles.iter().find(|p| &p.name == n))
    {
        match read_floor_config(&floor.dir) {
            Ok(Some((from, _))) => {
                let c = carry_config(existing, &from);
                merged = c.map;
                merge = c.added;
                seeded = c.seeded;
            }
            Ok(None) => {}
            Err(why) => merge_blocked = Some(why),
        }
    }
    let differs = legacy
        .profiles
        .iter()
        .filter(|p| legacy.floor.as_deref() != Some(p.name.as_str()))
        .filter_map(|p| {
            let from = read_json_object(&p.dir.join(".claude.json"))?;
            let extra = merge_config(&mut merged.clone(), &from);
            (!extra.is_empty()).then(|| (p.name.clone(), extra))
        })
        .collect();
    Plan {
        rows,
        orca_running: view.running,
        orca_d: view.orca_runtime_dir.clone(),
        target_d: target.config_dir.clone(),
        target_config: target.config_path.clone(),
        merge,
        seeded,
        merge_blocked,
        differs,
        shared: shared_plan(home),
        plugin_paths: plugin_paths_preview(home, &legacy_dirs(legacy)),
        left_behind: legacy
            .profiles
            .iter()
            .map(|p| (p.name.clone(), left_behind(home, &p.dir)))
            .filter(|(_, v)| !v.is_empty())
            .collect(),
        state_dir: ctx.state.clone(),
        smart: smart_preview(&legacy_smart_dir(home)),
    }
}

fn legacy_dirs(legacy: &Legacy) -> Vec<PathBuf> {
    legacy.profiles.iter().map(|p| p.dir.clone()).collect()
}

fn fmt_exp(e: Option<f64>) -> String {
    match e.and_then(|ms| chrono::DateTime::from_timestamp_millis(ms as i64)) {
        Some(t) => t.format("%Y-%m-%d %H:%M UTC").to_string(),
        None => "?".into(),
    }
}

/// What a non-floor profile's `.claude.json` holds that `~/.claude.json`
/// still lacks (step 5 carries only the floor profile's), as lines naming
/// `from`, the file to merge them from by hand. Pure.
pub(crate) fn differs_lines(name: &str, from: &Path, keys: &[String]) -> Vec<String> {
    let mut out = vec![format!(
        "{name}: not carried into ~/.claude.json; merge by hand from {}:",
        from.display()
    )];
    out.extend(keys.iter().map(|k| format!("  {k}")));
    out
}

/// The line naming a kept stash's missing MCP logins ([`extra_logins`]).
/// Pure.
pub(crate) fn extra_logins_line(name: &str, extra: &[String]) -> Option<String> {
    (!extra.is_empty()).then(|| {
        format!(
            "{name}: the dir's credentials also hold {} that the stash lacks; retire files them in \
             the quarantine (extra-logins), log in to those MCP servers again",
            extra.join(", ")
        )
    })
}

/// Where the operator merges profile `name`'s config from: its dir, or
/// once retired, `<dir>.retired`.
fn profile_config(legacy: &Legacy, name: &str, retired: bool) -> PathBuf {
    let dir = legacy
        .profiles
        .iter()
        .find(|p| p.name == name)
        .map(|p| p.dir.clone())
        .unwrap_or_default();
    let dir = if retired {
        PathBuf::from(format!("{}.retired", dir.display()))
    } else {
        dir
    };
    dir.join(".claude.json")
}

/// Render the plan. Pure.
pub(crate) fn render_plan(p: &Plan) -> String {
    let mut o = String::new();
    o.push_str("profiles (~/.config/claude-as/profiles.json):\n");
    if p.rows.is_empty() {
        o.push_str("  (none: nothing to migrate)\n");
    }
    for r in &p.rows {
        let who = r.facts.email.as_deref().unwrap_or("-");
        let floor = if r.floor { " [floor]" } else { "" };
        let status = match &r.status {
            Status::InOrca { id, fresher } => format!(
                "in Orca ({id}){}",
                match fresher {
                    Some(true) => "; the dir's grant is fresher: import reads it back",
                    Some(false) => "",
                    None => "; import compares the dir's grant with the stash",
                }
            ),
            Status::ToImport => "to import".into(),
            Status::NoCredentials => "no credentials".into(),
        };
        o.push_str(&format!(
            "  {}{floor}  {}  {who}\n    {status}\n",
            r.facts.name,
            r.facts.dir.display()
        ));
        if let Some(g) = &r.facts.grant {
            o.push_str(&format!(
                "    dir grant   {}  expires {}\n",
                g.fingerprint,
                fmt_exp(g.expires_at)
            ));
        } else if !r.facts.grant_sources.is_empty() {
            o.push_str(&format!(
                "    dir grant   present ({}; not read)\n",
                r.facts.grant_sources.join(", ")
            ));
        }
        if let Some(g) = &r.stash {
            o.push_str(&format!(
                "    stash grant {}  expires {}\n",
                g.fingerprint,
                fmt_exp(g.expires_at)
            ));
        }
    }
    o.push_str(&format!(
        "\nOrca: {}; its D: {}\n",
        if p.orca_running { "running" } else { "stopped" },
        p.orca_d
            .as_ref()
            .map(|d| d.display().to_string())
            .unwrap_or_else(|| "unknown".into())
    ));
    o.push_str(&format!("target D: {}\n", p.target_d.display()));
    o.push_str(&format!(
        "\n{} gains from the floor profile:\n",
        p.target_config.display()
    ));
    if let Some(why) = &p.merge_blocked {
        o.push_str(&format!(
            "  cannot merge: {why}; import fails this step and retire refuses until it reads\n"
        ));
    } else if let Some(n) = p.seeded {
        o.push_str(&format!(
            "  (no file yet: seeded with the floor profile's {n} top-level key(s), oauthAccount left out)\n"
        ));
    } else if p.merge.is_empty() {
        o.push_str("  (nothing)\n");
    }
    for k in &p.merge {
        o.push_str(&format!("  + {k}\n"));
    }
    for (name, keys) in &p.differs {
        o.push_str(&format!("  profile {name} also has (merge by hand):\n"));
        for k in keys {
            o.push_str(&format!("    {k}\n"));
        }
    }
    o.push_str("\n~/.claude shared dirs:\n");
    for (name, a) in &p.shared {
        let what = match a {
            SharedAction::Nothing => "nothing to do".to_owned(),
            SharedAction::Unlink => "replace the link with the real dir".to_owned(),
            SharedAction::Move => "move the shared dir in".to_owned(),
            SharedAction::Drain => "drain the shared dir into it".to_owned(),
            SharedAction::Append => "put the shared file's lines in front of it".to_owned(),
            SharedAction::Skip(why) => format!("skipped: {why}"),
        };
        o.push_str(&format!("  {name}: {what}\n"));
    }
    if p.plugin_paths > 0 {
        o.push_str(&format!(
            "  plugin registries: {} recorded path(s) through an old plugins dir are pointed at ~/.claude/plugins\n",
            p.plugin_paths
        ));
    }
    if !p.left_behind.is_empty() {
        o.push_str(
            "\nper-profile content csm does not carry (it stays in <dir>.retired; move what ~/.claude should keep by hand):\n",
        );
        for (name, names) in &p.left_behind {
            o.push_str(&format!("  {name}: {}\n", names.join(", ")));
        }
        if p.left_behind
            .iter()
            .any(|(_, v)| v.iter().any(|n| n == "settings.json"))
        {
            o.push_str(
                "  note: a profile's settings.json (statusLine, hooks, enabledPlugins, permissions) is not \
                 carried; put what you need in ~/.claude/settings.json, including `csm statusline`, \
                 which the weekly-cap switch runs from\n",
            );
        }
    }
    match p.smart {
        None => o.push_str("  ~/.claude.shared/smart: none\n"),
        Some((carry, stay)) => o.push_str(&format!(
            "  ~/.claude.shared/smart: {carry} session file(s) move to {}; {stay} other entr{} (caches, markers, per-profile records) stay unread\n",
            p.state_dir.display(),
            if stay == 1 { "y" } else { "ies" }
        )),
    }
    o
}

fn setup() -> anyhow::Result<(Context, OrcaView, Legacy)> {
    let procs = SystemProcs;
    let ctx = Context::current(&procs).context("csm migrate")?;
    let view = crate::orca::snapshot(&SnapshotOptions::default()).context("csm migrate")?;
    let legacy = load_legacy(&ctx.env.home)?;
    Ok((ctx, view, legacy))
}

fn plan_cmd() -> anyhow::Result<()> {
    let (ctx, view, legacy) = setup()?;
    print!(
        "{}",
        render_plan(&build_plan(&ctx, &view, &legacy, Probe::Presence))
    );
    Ok(())
}

// ─── import (steps 4-7) ───────────────────────────────────────────────────────

fn import_cmd(dry_run: bool) -> anyhow::Result<()> {
    let procs = SystemProcs;
    let (ctx, view, legacy) = setup()?;
    let plan = build_plan(&ctx, &view, &legacy, Probe::Read);
    let sqlite = ctx.data_file.has_state_db();
    let repeats = repeat_imports(&plan.rows);
    if dry_run {
        println!("csm migrate import --dry-run would:");
        for (r, repeat) in plan.rows.iter().zip(&repeats) {
            if let Some(why) = unreadable_row(&r.facts) {
                println!("  fail {}: {why}", r.facts.name);
                continue;
            }
            if *repeat && !sqlite {
                println!(
                    "  decide {} after the import of an earlier profile of the same account \
                     (read back if its grant is fresher)",
                    r.facts.name
                );
                continue;
            }
            match &r.status {
                Status::ToImport if sqlite => println!(
                    "  not import {}: {}",
                    r.facts.name,
                    deferred_import_line(&r.facts.dir)
                ),
                Status::ToImport => {
                    println!("  import {} from {}", r.facts.name, r.facts.dir.display())
                }
                Status::InOrca {
                    id,
                    fresher: Some(true),
                } => {
                    println!("  read back {}'s grant into stash {id}", r.facts.name)
                }
                Status::InOrca { .. } => {
                    println!("  keep stash for {} (dir grant not fresher)", r.facts.name);
                    let extra = extra_logins(&r.status, r.facts.grant.as_ref(), r.stash.as_ref());
                    if let Some(line) = extra_logins_line(&r.facts.name, &extra) {
                        println!("    {line}");
                    }
                }
                Status::NoCredentials => println!("  skip {} (no credentials)", r.facts.name),
            }
        }
        if let Some(why) = &plan.merge_blocked {
            println!("  fail step 5: {why}; {FLOOR_CONFIG_FIX}");
        }
        if let Some(n) = plan.seeded {
            println!(
                "  create {} from the floor profile's {n} key(s), minus oauthAccount",
                plan.target_config.display()
            );
        }
        for k in &plan.merge {
            println!("  merge {k} into {}", plan.target_config.display());
        }
        for (name, keys) in &plan.differs {
            for line in differs_lines(name, &profile_config(&legacy, name, false), keys) {
                println!("  {line}");
            }
        }
        if let Some((carry, _)) = plan.smart {
            println!(
                "  move {carry} session file(s) from ~/.claude.shared/smart to {}",
                plan.state_dir.display()
            );
        }
        for (n, a) in &plan.shared {
            if !matches!(a, SharedAction::Nothing) {
                println!("  ~/.claude/{n}: {a:?}");
            }
        }
        let floor_row = legacy
            .floor
            .as_ref()
            .and_then(|f| plan.rows.iter().find(|r| &r.facts.name == f));
        for line in step7_dry_run(
            legacy.floor.as_deref(),
            floor_row.map(|r| &r.status),
            sqlite,
        ) {
            println!("  {line}");
        }
        if let Err(e) = gate(&ctx, &procs, &legacy) {
            println!("and would refuse now: {e}");
        }
        return Ok(());
    }
    gate(&ctx, &procs, &legacy)?;
    let mut failed = 0usize;
    // Retire waits for this run's step 7.
    clear_step7(&ctx.state).context("cannot reset csm's migrate state")?;

    // Step 4.
    let http = SystemHttp::from_env();
    for (r, repeat) in plan.rows.iter().zip(&repeats) {
        if let Some(why) = unreadable_row(&r.facts) {
            failed += 1;
            eprintln!("{}: {why}", r.facts.name);
            continue;
        }
        // A second profile of an account an earlier row just imported:
        // decide it against Orca's list as it is now.
        let now;
        let (status, view_now) = if *repeat {
            match crate::orca::snapshot(&SnapshotOptions::default()) {
                Ok(v) => {
                    now = v;
                    let host: Vec<AccountRecord> = now.host_accounts().cloned().collect();
                    let sg = |id: &str| stash_grant(&ctx, &now, id);
                    (classify(&r.facts, &host, &sg), &now)
                }
                Err(e) => {
                    failed += 1;
                    eprintln!("{}: {e:#}", r.facts.name);
                    continue;
                }
            }
        } else {
            (r.status.clone(), &view)
        };
        let res = match row_action(&status, sqlite) {
            RowAction::Import => import_one(&ctx, &procs, &r.facts.dir),
            RowAction::ReadBack(id) => {
                read_back_one(&ctx, &procs, view_now, &http, &r.facts.dir, &id)
            }
            RowAction::Defer => Ok(deferred_import_line(&r.facts.dir)),
            RowAction::Skip => {
                let extra = extra_logins(&status, r.facts.grant.as_ref(), r.stash.as_ref());
                if let Some(line) = extra_logins_line(&r.facts.name, &extra) {
                    println!("{line}");
                }
                continue;
            }
        };
        match res {
            Ok(line) => println!("{}: {line}", r.facts.name),
            Err(e) => {
                failed += 1;
                eprintln!("{}: {e:#}", r.facts.name);
            }
        }
    }

    // Step 4 can take minutes (profile calls, `claude auth status` per
    // row): Orca or a claude may have started since the gate above, so
    // steps 5 and 6 each take it again before they write.
    let regate = || gate(&ctx, &procs, &legacy);

    // Step 5.
    match gated(regate, || merge_step(&ctx, &legacy)) {
        Ok(Ok(Some(line))) => println!("{line}"),
        Ok(Ok(None)) => {}
        Ok(Err(e)) => {
            failed += 1;
            eprintln!("config merge: {e:#}");
        }
        Err(e) => {
            failed += 1;
            eprintln!("config merge: not run: {e:#}");
        }
    }
    // What the other profiles hold stays behind: say so here, not only in
    // `plan`, since retire moves those files out of view.
    for (name, keys) in &plan.differs {
        for line in differs_lines(name, &profile_config(&legacy, name, false), keys) {
            println!("{line}");
        }
    }

    // Step 6.
    let step6 = gated(regate, || {
        let mut failed = 0usize;
        match apply_shared(&ctx.env.home) {
            Ok(lines) => lines.iter().for_each(|l| println!("{l}")),
            Err(e) => {
                failed += 1;
                eprintln!("shared dirs: {e}");
            }
        }
        match repoint_profile_links(&ctx.env.home, &legacy_dirs(&legacy)) {
            Ok(lines) => lines.iter().for_each(|l| println!("{l}")),
            Err(e) => {
                failed += 1;
                eprintln!("profile links: {e}");
            }
        }
        match apply_plugin_paths(&ctx.env.home, &legacy_dirs(&legacy), &ctx.state) {
            Ok(lines) => lines.iter().for_each(|l| println!("{l}")),
            Err(e) => {
                failed += 1;
                eprintln!("plugin paths: {e}");
            }
        }
        for (name, names) in &plan.left_behind {
            println!(
                "{name}: its own {} stay in the profile dir (and <dir>.retired); move what ~/.claude should keep by hand",
                names.join(", ")
            );
        }
        match apply_smart(&legacy_smart_dir(&ctx.env.home), &ctx.state) {
            Ok(Some(line)) => println!("{line}"),
            Ok(None) => {}
            Err(e) => {
                failed += 1;
                eprintln!("csm state: {e}");
            }
        }
        failed
    });
    match step6 {
        Ok(n) => failed += n,
        Err(e) => {
            failed += 1;
            eprintln!("shared dirs: not run: {e:#}");
        }
    }

    // Step 7 needs steps 4-6. The attribution does not: it writes only
    // stashes, csm's quarantine and `D`'s `oauthAccount`, and Orca's first
    // start must not find `~/.claude`'s grants unattributed, whatever
    // failed above.
    let attribute = || {
        let r = ctx.with_switch_env(&procs, &http, switch::attribute_offline);
        attribution_line(r.map_err(|e| e.to_string()))
    };
    if failed > 0 {
        match gated(regate, attribute) {
            Ok(Ok(Some(line))) => println!("{line}"),
            Ok(Ok(None)) => {}
            Ok(Err(e)) => eprintln!("{e}"),
            Err(e) => eprintln!("attribution: not run: {e:#}"),
        }
        bail!(
            "csm migrate import: {failed} step(s) failed; the floor switch was not run. Do not \
             start Orca until `csm migrate import` completes (retire refuses until then)"
        );
    }

    // Step 7.
    let action = match &legacy.floor {
        Some(floor) => {
            let view = crate::orca::snapshot(&SnapshotOptions::default())?;
            let host: Vec<AccountRecord> = view.host_accounts().cloned().collect();
            let p = legacy
                .profiles
                .iter()
                .find(|p| &p.name == floor)
                .expect("floor is a profile");
            let facts = profile_facts(&ctx, p, Probe::Read);
            Some((
                floor,
                floor_action(floor, &classify(&facts, &host, &|_| None), sqlite),
            ))
        }
        None => None,
    };
    // No offline switch (none at all without a floor profile): attribute
    // ~/.claude's grants now, before Orca's first start reads them back
    // with no profile check (design §9 step 7).
    if !matches!(action, Some((_, FloorAction::Switch(_)))) {
        match attribute() {
            Ok(Some(line)) => println!("{line}"),
            Ok(None) => {}
            Err(e) => bail!("{e}"),
        }
    }
    let how = match action {
        None => "attributed",
        Some((_, FloorAction::Instruct(line))) => {
            println!("{line}");
            "attributed"
        }
        Some((floor, FloorAction::Switch(id))) => {
            let report = match ctx.with_switch_env(&procs, &http, |env| switch::switch(env, &id)) {
                Ok(r) => r,
                Err(e) => {
                    // The switch may have stopped before its own read-back.
                    match attribute() {
                        Ok(Some(line)) => println!("{line}"),
                        Ok(None) => {}
                        Err(a) => eprintln!("{a}"),
                    }
                    bail!(
                        "the switch to {floor}'s account ({id}) failed: {e:#}; do not start Orca \
                         until `csm migrate import` completes"
                    );
                }
            };
            match report.outcome {
                Outcome::Switched | Outcome::AlreadyActive => {
                    println!("switched ~/.claude to {floor}'s account ({id})");
                    "switched"
                }
                Outcome::Uncertain(why) => {
                    bail!("the switch to {id} did not verify ({why}); run `csm accounts doctor`")
                }
            }
        }
        Some((floor, FloorAction::NoAccount)) => {
            eprintln!(
                "the floor profile {floor} has no Orca account; run `csm accounts use` by hand"
            );
            "attributed"
        }
    };
    record_step7(&ctx.state, how).context("cannot record step 7 in csm's migrate state")?;
    println!("next: `csm migrate retire`, then start Orca and run `csm orca setup`");
    Ok(())
}

/// Run `step` only when `check` (the write gate, taken again) passes;
/// `Err` is the gate's refusal, and `step` did not run then. Pure over its
/// two fns.
fn gated<T>(
    check: impl FnOnce() -> anyhow::Result<()>,
    step: impl FnOnce() -> T,
) -> anyhow::Result<T> {
    check()?;
    Ok(step())
}

/// Step 4's import: `previousLegacyCredentialsSha256` is the unscoped
/// item's digest, read under `switch.lock` inside the import so a
/// concurrent switch cannot slip in between.
fn import_one(ctx: &Context, procs: &dyn ProcFacts, dir: &Path) -> anyhow::Result<String> {
    let cli = SystemClaude::configured()?;
    let c = ctx.with_accounts_env(procs, |env| add::import_current_legacy(env, &cli, dir))?;
    import_line(&c).map_err(anyhow::Error::msg)
}

/// Step 4's row for one import: `Err` when Orca refused the redo (nothing
/// was imported) or did not confirm it, so the row counts as failed and
/// the floor switch does not run on top of it. Pure.
pub(crate) fn import_line(c: &add::AccountChange) -> Result<String, String> {
    use crate::orca::store::RedoOutcome;
    let who = c
        .email
        .clone()
        .or_else(|| c.id.clone())
        .unwrap_or_else(|| "the account".into());
    let leftover = c
        .leftover
        .as_ref()
        .map(|l| format!("; left for `csm accounts doctor`: {l}"))
        .unwrap_or_default();
    match &c.redo {
        Some(RedoOutcome::Failed(why)) => Err(format!(
            "{who} was not imported: Orca refused it ({why}){leftover}"
        )),
        Some(RedoOutcome::Uncertain(why)) => Err(format!(
            "{who} may not be imported: it was handed to Orca, which did not confirm it ({why}); \
             check `csm accounts doctor`{leftover}"
        )),
        _ => Ok(format!("imported {who}{leftover}")),
    }
}

fn read_back_one(
    ctx: &Context,
    procs: &dyn ProcFacts,
    view: &OrcaView,
    http: &dyn OauthHttp,
    dir: &Path,
    id: &str,
) -> anyhow::Result<String> {
    let _lock = fsx::SwitchLock::acquire(&ctx.state, crate::orca::context::LOCK_WAIT)?;
    if ctx.orca_running(procs) {
        bail!("Orca started; read-back skipped");
    }
    let paths = runtime_paths(Some(&dir.to_string_lossy()), &ctx.env.home, |p| p.exists());
    let records: Vec<AccountRecord> = view.host_accounts().cloned().collect();
    let q = Quarantine::new(ctx.os(), &ctx.state);
    let rep = readback::read_back(&ReadBack {
        os: ctx.os(),
        user_data: &ctx.user_data.dir,
        paths: &paths,
        keychain_user: &ctx.keychain_user,
        records: &records,
        exclude: None,
        live_claude: false,
        http,
        quarantine: &q,
        now_ms: chrono::Utc::now().timestamp_millis(),
        migration: true,
    })?;
    let mut s = match &rep.persisted {
        Some(p) if p == id => format!("stash {id} now holds the dir's grant"),
        Some(p) => format!("the dir's grant went to stash {p}"),
        None => format!("stash {id} kept"),
    };
    if !rep.quarantined.is_empty() {
        s.push_str(&format!("; {} grant(s) quarantined", rep.quarantined.len()));
    }
    Ok(s)
}

/// The floor profile's `.claude.json`: the object and the bytes it was
/// parsed from (step 5 records their digest).
type FloorConfig = (Map<String, Value>, Vec<u8>);

/// The floor profile's `.claude.json` as step 5 sees it, from a capped
/// read: `Ok(None)` when there is no file (nothing to merge), `Err` when
/// the file cannot be read or is not a JSON object. That is never "nothing
/// to merge": retire would then rename the dir with its trust and MCP
/// settings neither carried over nor reported. Pure.
pub(crate) fn floor_config_from(
    read: io::Result<Option<Vec<u8>>>,
) -> Result<Option<FloorConfig>, String> {
    match read {
        Ok(None) => Ok(None),
        Ok(Some(b)) => match crate::orca::runtime::parse_json_object(Some(&b)) {
            Some(m) => Ok(Some((m, b))),
            None => Err("the floor profile's .claude.json is not a JSON object".into()),
        },
        Err(e) => Err(format!(
            "the floor profile's .claude.json cannot be read ({})",
            e.kind()
        )),
    }
}

/// [`floor_config_from`] over the floor profile's file.
fn read_floor_config(dir: &Path) -> Result<Option<FloorConfig>, String> {
    floor_config_from(crate::orca::read_capped_bytes(
        &dir.join(".claude.json"),
        64 * 1024 * 1024,
    ))
}

/// What to tell the operator when [`read_floor_config`] fails.
const FLOOR_CONFIG_FIX: &str =
    "fix it (or restore its .claude.json.backup) and rerun `csm migrate import`";

fn merge_step(ctx: &Context, legacy: &Legacy) -> anyhow::Result<Option<String>> {
    let Some(floor) = legacy
        .floor
        .as_ref()
        .and_then(|n| legacy.profiles.iter().find(|p| &p.name == n))
    else {
        return Ok(None);
    };
    let Some((from, floor_bytes)) =
        read_floor_config(&floor.dir).map_err(|e| anyhow::anyhow!("{e}; {FLOOR_CONFIG_FIX}"))?
    else {
        return Ok(None);
    };
    // The read-modify-write runs under switch.lock: the file is the one a
    // switch writes `oauthAccount` into, and a switch landing between the
    // read and the write would get its identity overwritten with the old one.
    let _lock = fsx::SwitchLock::acquire(&ctx.state, crate::orca::context::LOCK_WAIT)?;
    let target = runtime_paths(None, &ctx.env.home, |p| p.exists()).config_path;
    let before = crate::orca::read_capped_bytes(&target, 64 * 1024 * 1024)?;
    let existing = match &before {
        Some(b) => match serde_json::from_slice::<Value>(b) {
            Ok(Value::Object(m)) => Some(m),
            _ => bail!("{} is not a JSON object; left as is", target.display()),
        },
        None => None,
    };
    let carry = carry_config(existing, &from);
    if !carry.changed() {
        record_merge(&ctx.state, &floor_bytes)?;
        return Ok(None);
    }
    if let Some(b) = &before {
        let pre_dir = ctx.state.join("migrate");
        fsx::create_dir_all(&pre_dir, 0o700)?;
        let pre = pre_dir.join(format!(
            "claude.json.{}.pre",
            chrono::Utc::now().format("%Y%m%dT%H%M%SZ")
        ));
        fsx::write_atomic(&pre, b, fsx::WriteOpts::PRIVATE)?;
    }
    let mut text = serde_json::to_vec_pretty(&Value::Object(carry.map))?;
    text.push(b'\n');
    fsx::guard(&target)?;
    fsx::write_atomic(&target, &text, fsx::WriteOpts::PRIVATE)?;
    // Retire checks step 5 against this record, not against what the user
    // may have removed from ~/.claude.json since.
    record_merge(&ctx.state, &floor_bytes)?;
    Ok(Some(match carry.seeded {
        Some(n) => format!(
            "{}: created from {}'s {n} key(s) (oauthAccount left out), {} trust/MCP key(s) merged",
            target.display(),
            floor.name,
            carry.added.len()
        ),
        None => format!(
            "{}: merged {} key(s) from {}",
            target.display(),
            carry.added.len(),
            floor.name
        ),
    }))
}

// ─── retire (step 8) ──────────────────────────────────────────────────────────

/// What the profile endpoint said about a stash's own access token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StashCheck {
    /// It names the stash's account.
    Verified,
    /// 401: the stashed access token is dead (usually just expired).
    Rejected,
    /// No stash, no token, another account, or no usable answer.
    Unverified,
}

/// How a legacy dir's grants compare with the stash retire would leave as
/// the account's only copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DirGrant {
    /// The stash holds the same grant or a newer one, or the dir's newer
    /// grant already sits in the quarantine (a read-back vetoed it).
    Superseded,
    /// The dir's grant would win Orca's acceptance rule over the stash's:
    /// retiring now would leave an older refresh token live (design
    /// section 9 step 8: only after step 4 verified the stash).
    Newer,
    /// Not every grant could be read (a locked Keychain, a `security`
    /// timeout): nothing is moved.
    Unreadable,
}

/// Compare a retire row's dir grants with its stash. Pure.
pub(crate) fn dir_grant_state(
    facts: &ProfileFacts,
    status: &Status,
    quarantined: &dyn Fn(&str) -> bool,
) -> DirGrant {
    if facts.grant_sources.contains(&KEYCHAIN_UNREADABLE) {
        return DirGrant::Unreadable;
    }
    match status {
        Status::InOrca {
            fresher: Some(false),
            ..
        } => DirGrant::Superseded,
        Status::InOrca {
            fresher: Some(true),
            ..
        } => match &facts.grant {
            Some(g) if quarantined(&g.fingerprint) => DirGrant::Superseded,
            _ => DirGrant::Newer,
        },
        // Sources present but no grant read back: fail closed.
        Status::InOrca { fresher: None, .. } => DirGrant::Unreadable,
        Status::ToImport | Status::NoCredentials => DirGrant::Superseded,
    }
}

/// Is `p` safe to retire? Pure over what was checked. `known` is the Orca
/// account the dir's `.claude.json` names, whatever grants the dir holds:
/// a dir with no grant left whose account Orca has is renamed with nothing
/// to quarantine. That is also how a retire that filed and deleted a dir's
/// grants but died before the rename completes on a rerun.
pub(crate) fn retire_verdict(
    status: &Status,
    known: Option<&str>,
    check: StashCheck,
    grant: DirGrant,
    dir: &Path,
    home: &Path,
    exists: bool,
) -> Result<String, String> {
    let d = home.join(".claude");
    let same = |a: &Path, b: &Path| {
        a == b
            || std::fs::canonicalize(a)
                .ok()
                .is_some_and(|x| std::fs::canonicalize(b).ok() == Some(x))
    };
    if same(dir, &d) || same(dir, home) {
        return Err("is (or resolves to) the default dir; never retired".into());
    }
    if !exists {
        return Err("dir is gone (already retired?)".into());
    }
    match status {
        Status::InOrca { .. } if grant == DirGrant::Unreadable => Err(
            "not every grant in the dir could be read (is the Keychain locked?); nothing moved, rerun when it reads"
                .into(),
        ),
        Status::InOrca { id, .. } if grant == DirGrant::Newer => Err(format!(
            "the dir holds a grant newer than stash {id}; run `csm migrate import` first so it is read back"
        )),
        Status::InOrca { id, .. } => match check {
            StashCheck::Verified => Ok(id.clone()),
            StashCheck::Rejected => Err(format!(
                "stash {id}'s access token was rejected (expired?); let Orca refresh it: start Orca, \
                 run `csm usage --refresh` (for the active account, open one Claude pane instead), \
                 quit Orca, then rerun retire"
            )),
            StashCheck::Unverified => Err(format!(
                "stash {id} did not verify (no answer from the profile endpoint, or it names another account)"
            )),
        },
        Status::ToImport => Err("not in Orca yet; run `csm migrate import` first".into()),
        Status::NoCredentials => match known {
            Some(id) => Ok(id.to_owned()),
            None => Err("holds no login; remove the dir by hand if unused".into()),
        },
    }
}

// ─── retire: what still names a profile dir ───────────────────────────────────

/// Does `text` name the path `needle`: followed by the end, a separator or
/// a character no path component continues with (`.claude.work` does not
/// name `.claude.work2` or `.claude.work.retired`)? Pure.
pub(crate) fn names_path(text: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    text.match_indices(needle).any(|(i, _)| {
        text[i + needle.len()..]
            .chars()
            .next()
            .is_none_or(|c| !(c.is_alphanumeric() || matches!(c, '.' | '_' | '-')))
    })
}

/// The spellings of `dir` a settings file or an MCP server may use: as
/// registered (without a trailing separator), its realpath, and under the
/// home dir as `~/…`, `$HOME/…` and `${HOME}/…`. Pure over `real`.
pub(crate) fn dir_needles(dir: &Path, real: Option<&Path>, home: &Path) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |s: String| {
        let s = s.trim_end_matches(['/', '\\']).to_owned();
        if !s.is_empty() && !out.contains(&s) {
            out.push(s);
        }
    };
    for d in std::iter::once(dir).chain(real) {
        push(d.to_string_lossy().into_owned());
        if let Ok(rel) = d.strip_prefix(home) {
            let rel = rel.to_string_lossy();
            if !rel.is_empty() {
                for h in ["~", "$HOME", "${HOME}"] {
                    push(format!("{h}/{rel}"));
                }
            }
        }
    }
    out
}

/// Every string under `v` that names one of `needles`, as a key path
/// (`hooks.PreToolUse[0].hooks[0].command`). Pure.
fn json_mentions(v: &Value, at: &str, needles: &[String], out: &mut Vec<String>) {
    match v {
        Value::String(t) => {
            if needles.iter().any(|n| names_path(t, n)) {
                out.push(at.to_owned());
            }
        }
        Value::Array(a) => {
            for (i, x) in a.iter().enumerate() {
                json_mentions(x, &format!("{at}[{i}]"), needles, out);
            }
        }
        Value::Object(m) => {
            for (k, x) in m {
                let at = if at.is_empty() {
                    k.clone()
                } else {
                    format!("{at}.{k}")
                };
                json_mentions(x, &at, needles, out);
            }
        }
        _ => {}
    }
}

/// Where `~/.claude`'s settings (`(label, text)`, the whole file) and
/// `~/.claude.json`'s MCP servers (top level and per project; project keys
/// are paths claude ran in and never count) still name a dir retire would
/// rename. Each would break at the rename: a statusLine goes blank (and
/// with it the weekly-cap switch), a hook command that no longer exists
/// fails without blocking, an MCP server does not start. A settings file
/// that is not JSON is searched as text. Pure.
pub(crate) fn dir_references(
    settings: &[(&str, &str)],
    claude_json: Option<&str>,
    needles: &[String],
) -> Vec<String> {
    let mut out = Vec::new();
    for (label, text) in settings {
        match serde_json::from_str::<Value>(text) {
            Ok(v) => {
                let mut at = Vec::new();
                json_mentions(&v, "", needles, &mut at);
                out.extend(at.into_iter().map(|a| format!("{label} ({a})")));
            }
            Err(_) => {
                if needles.iter().any(|n| names_path(text, n)) {
                    out.push((*label).to_owned());
                }
            }
        }
    }
    let Some(Ok(Value::Object(cfg))) = claude_json.map(serde_json::from_str::<Value>) else {
        return out;
    };
    let mut at = Vec::new();
    if let Some(v) = cfg.get("mcpServers") {
        json_mentions(v, "mcpServers", needles, &mut at);
    }
    if let Some(Value::Object(projects)) = cfg.get("projects") {
        for (path, p) in projects {
            if let Some(v) = p.get("mcpServers") {
                json_mentions(v, &format!("projects[{path}].mcpServers"), needles, &mut at);
            }
        }
    }
    out.extend(at.into_iter().map(|a| format!("~/.claude.json ({a})")));
    out
}

/// [`dir_references`] over the machine: `~/.claude/settings.json`,
/// `~/.claude/settings.local.json` and `~/.claude.json`.
fn references_to(home: &Path, dir: &Path) -> Vec<String> {
    let read = |p: &Path| {
        crate::orca::read_capped_bytes(p, 64 * 1024 * 1024)
            .ok()
            .flatten()
            .map(|b| String::from_utf8_lossy(&b).into_owned())
    };
    let d = home.join(".claude");
    let files = [
        ("~/.claude/settings.json", read(&d.join("settings.json"))),
        (
            "~/.claude/settings.local.json",
            read(&d.join("settings.local.json")),
        ),
    ];
    let settings: Vec<(&str, &str)> = files
        .iter()
        .filter_map(|(l, t)| t.as_deref().map(|t| (*l, t)))
        .collect();
    let claude_json = read(&runtime_paths(None, home, |p| p.exists()).config_path);
    let real = std::fs::canonicalize(dir).ok();
    let needles = dir_needles(dir, real.as_deref(), home);
    dir_references(&settings, claude_json.as_deref(), &needles)
}

/// Retire's refusal while `refs` ([`dir_references`]) is not empty. Pure.
pub(crate) fn references_verdict(refs: &[String]) -> Result<(), String> {
    if refs.is_empty() {
        return Ok(());
    }
    let mut shown: Vec<&str> = refs.iter().map(String::as_str).take(3).collect();
    if refs.len() > 3 {
        shown.push("…");
    }
    Err(format!(
        "still named by {}; retiring would rename the dir under them. Move what they need into \
         ~/.claude and point them there first",
        shown.join(", ")
    ))
}

/// How retire files one of a dir's grants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RetireFiling {
    /// The account's own grant (the stash's, or one that profiles as the
    /// stash's account): filed as [`Reason::Retired`] under the account.
    Own,
    /// Not provably the account's: filed under this reason with the profile
    /// answer, attributed to no account, so `accounts doctor` shows it.
    Unattributed(Reason, Option<(u16, Option<String>)>),
}

/// Pure: file a dir grant. A grant equal to the stash's is the account's
/// without asking; any other goes through the profile veto (`veto`), since
/// the dir's `.claude.json` names the account only for the grant claude
/// last wrote, not for every Keychain spelling or file the dir holds. `Err`
/// when the endpoint gave no usable answer: retire then moves nothing.
pub(crate) fn retire_filing(
    same_as_stash: bool,
    veto: impl FnOnce() -> Result<readback::Veto, crate::orca::OrcaError>,
) -> Result<RetireFiling, String> {
    if same_as_stash {
        return Ok(RetireFiling::Own);
    }
    match veto() {
        Ok(readback::Veto::Owner) => Ok(RetireFiling::Own),
        Ok(readback::Veto::Quarantine(reason, profile)) => {
            Ok(RetireFiling::Unattributed(reason, profile))
        }
        Ok(readback::Veto::Unauthorized) => Ok(RetireFiling::Unattributed(
            Reason::Unauthorized,
            Some((401, None)),
        )),
        Err(e) => Err(format!(
            "a grant in the dir could not be attributed ({e}); nothing moved, rerun when the profile endpoint answers"
        )),
    }
}

/// Does the profile endpoint put the stash's grant on the stash's account?
fn stash_check(ctx: &Context, view: &OrcaView, http: &dyn OauthHttp, id: &str) -> StashCheck {
    let Ok(s) = Stash::open(&ctx.user_data.dir, id, stash_path(view, id).as_deref()) else {
        return StashCheck::Unverified;
    };
    let Some(uuid) = s
        .oauth_account()
        .ok()
        .flatten()
        .and_then(|v| OauthIdentity::from_value(&v).account_uuid)
    else {
        return StashCheck::Unverified;
    };
    let Some(token) = s
        .credentials(ctx.os())
        .ok()
        .flatten()
        .and_then(|c| access_token(c.expose()))
    else {
        return StashCheck::Unverified;
    };
    match http.get_profile(&token).map(|r| parse_profile(&r)) {
        Ok(ProfileAnswer::Ok {
            account_uuid: Some(u),
            ..
        }) if u == uuid => StashCheck::Verified,
        Ok(ProfileAnswer::Unauthorized) => StashCheck::Rejected,
        _ => StashCheck::Unverified,
    }
}

/// The reason retire files the account's own grant under: a copy holding
/// side state (MCP logins, `extra`) the stash lacks is not merely retired.
/// Pure.
pub(crate) fn retired_reason(extra: &[String]) -> Reason {
    if extra.is_empty() {
        Reason::Retired
    } else {
        Reason::ExtraLogins
    }
}

/// Move a dir's grants into the quarantine, then rename it `<dir>.retired`.
/// Every grant is attributed first ([`retire_filing`]): one that is not
/// the account's is filed unattributed, never as a retired copy of `id`.
fn retire_dir(
    ctx: &Context,
    view: &OrcaView,
    http: &dyn OauthHttp,
    dir: &Path,
    id: &str,
) -> anyhow::Result<String> {
    let retired = PathBuf::from(format!("{}.retired", dir.display()));
    if std::fs::symlink_metadata(&retired).is_ok() {
        bail!("{} already exists", retired.display());
    }
    let q = Quarantine::new(ctx.os(), &ctx.state);
    let now = chrono::Utc::now().timestamp_millis();
    let (grants, unreadable) = dir_grants(ctx, dir);
    if unreadable {
        bail!("a Keychain item of the dir could not be read; nothing moved");
    }
    // Attribute every grant before anything moves: a missing answer then
    // leaves the dir as it was.
    let stash = Stash::open(&ctx.user_data.dir, id, stash_path(view, id).as_deref()).ok();
    let stash_creds = stash
        .as_ref()
        .and_then(|s| s.credentials(ctx.os()).ok().flatten());
    let stash_fp = stash_creds
        .as_ref()
        .map(|c| quarantine::fingerprint(c.expose()));
    let stash_side = stash_creds
        .as_ref()
        .map(|c| quarantine::side_state(c.expose()))
        .unwrap_or_default();
    let stash_uuid = stash
        .as_ref()
        .and_then(|s| s.oauth_account().ok().flatten())
        .and_then(|v| OauthIdentity::from_value(&v).account_uuid);
    let mut filings = Vec::with_capacity(grants.len());
    for (_, _, grant) in &grants {
        let same = stash_fp.as_deref() == Some(quarantine::fingerprint(grant.expose()).as_str());
        let filing = retire_filing(same, || {
            readback::profile_veto(http, grant.expose(), stash_uuid.as_deref())
        })
        .map_err(anyhow::Error::msg)?;
        filings.push(filing);
    }
    let mut moved = 0usize;
    let mut foreign = 0usize;
    let mut logins: Vec<String> = Vec::new();
    for ((source, loc, grant), filing) in grants.into_iter().zip(filings) {
        // Filed (and on macOS read back) before the original goes.
        match filing {
            RetireFiling::Own => {
                // The fingerprint compares only the Claude grant: MCP logins
                // the stash lacks make the copy more than a retired one, so
                // `doctor --fix` must not purge it as superseded.
                let extra =
                    quarantine::uncovered(&quarantine::side_state(grant.expose()), &stash_side);
                let reason = retired_reason(&extra);
                q.file(grant.expose(), reason, source, Some(id), None, now)?;
                for k in extra {
                    if !logins.contains(&k) {
                        logins.push(k);
                    }
                }
            }
            RetireFiling::Unattributed(reason, profile) => {
                let answer = profile.as_ref().map(|(s, u)| (*s, u.as_deref()));
                q.file(grant.expose(), reason, source, None, answer, now)?;
                foreign += 1;
            }
        }
        if source == "file" {
            let p = Path::new(&loc);
            fsx::guard(p)?;
            std::fs::remove_file(p)?;
        } else {
            keychain::delete_password(&loc, &ctx.keychain_user.acct)?;
        }
        moved += 1;
    }
    move_path(dir, &retired)?;
    let mut note = if foreign > 0 {
        format!(
            " ({foreign} not provably this account's, filed unattributed: see `csm accounts doctor`)"
        )
    } else {
        String::new()
    };
    if !logins.is_empty() {
        note.push_str(&format!(
            " (also holding {} that stash {id} lacks: filed as extra-logins, which `csm accounts \
             doctor --fix` keeps; log in to those MCP servers again)",
            logins.join(", ")
        ));
    }
    Ok(format!(
        "{} grant(s) quarantined{note}; dir renamed to {}",
        moved,
        retired.display()
    ))
}

/// Clear the machine-wide `CLAUDE_CONFIG_DIR` floor. Inert under
/// `cfg(test)` and in the e2e build: the value outlives the process and
/// belongs to the real login session.
fn unset_floor_env() -> io::Result<()> {
    if cfg!(test) || crate::e2e::ENABLED {
        return Ok(());
    }
    unset_floor_env_impl()
}

#[cfg(target_os = "macos")]
fn unset_floor_env_impl() -> io::Result<()> {
    let s = std::process::Command::new("/bin/launchctl")
        .args(["unsetenv", "CLAUDE_CONFIG_DIR"])
        .status()?;
    if s.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "launchctl unsetenv exited with {s}"
        )))
    }
}

#[cfg(windows)]
fn unset_floor_env_impl() -> io::Result<()> {
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, HWND};
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, RegCloseKey, RegDeleteValueW, RegOpenKeyExW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE,
    };
    let key: Vec<u16> = "Environment\0".encode_utf16().collect();
    let name: Vec<u16> = "CLAUDE_CONFIG_DIR\0".encode_utf16().collect();
    let mut hkey: HKEY = std::ptr::null_mut();
    // SAFETY: valid NUL-terminated wide strings and an out-pointer.
    let rc = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, key.as_ptr(), 0, KEY_SET_VALUE, &mut hkey) };
    if rc != 0 {
        return Err(io::Error::other(format!(
            "RegOpenKeyExW failed (0x{rc:08X})"
        )));
    }
    // SAFETY: hkey was opened above.
    let rc = unsafe { RegDeleteValueW(hkey, name.as_ptr()) };
    // SAFETY: hkey was opened above.
    unsafe { RegCloseKey(hkey) };
    if rc != 0 && rc != ERROR_FILE_NOT_FOUND {
        return Err(io::Error::other(format!(
            "RegDeleteValueW failed (0x{rc:08X})"
        )));
    }
    let mut result: usize = 0;
    // SAFETY: a broadcast with a static wide string and a timeout.
    unsafe {
        SendMessageTimeoutW(
            HWND_BROADCAST as HWND,
            WM_SETTINGCHANGE,
            0,
            key.as_ptr() as isize,
            SMTO_ABORTIFHUNG,
            5000,
            &mut result,
        );
    }
    Ok(())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn unset_floor_env_impl() -> io::Result<()> {
    Ok(())
}

/// The command that clears the floor by hand, for the failure line.
#[cfg(windows)]
const FLOOR_UNSET_HINT: &str = "reg delete HKCU\\Environment /v CLAUDE_CONFIG_DIR /f";
#[cfg(not(windows))]
const FLOOR_UNSET_HINT: &str = "launchctl unsetenv CLAUDE_CONFIG_DIR";

/// Retire's last part: clear the `CLAUDE_CONFIG_DIR` floor first, then
/// remove the registry. The order matters: retire acts on the floor only
/// while the registry still lists profiles, so a failed unset keeps the
/// registry and a rerun retries it. `Ok` = the lines to print; `Err` = the
/// one failure line. Pure over its two effects.
fn retire_registry(
    unset: impl FnOnce() -> io::Result<()>,
    remove: impl FnOnce() -> io::Result<Vec<PathBuf>>,
) -> Result<Vec<String>, String> {
    if let Err(e) = unset() {
        return Err(format!(
            "cannot clear the CLAUDE_CONFIG_DIR floor ({e}); the legacy registry was kept so a rerun retries it, or run `{FLOOR_UNSET_HINT}`"
        ));
    }
    let mut lines = vec!["cleared the CLAUDE_CONFIG_DIR floor".to_owned()];
    match remove() {
        Ok(removed) => {
            lines.extend(removed.iter().map(|p| format!("removed {}", p.display())));
            Ok(lines)
        }
        Err(e) => Err(format!(
            "cleared the CLAUDE_CONFIG_DIR floor, but cannot remove the legacy registry: {e}"
        )),
    }
}

/// Is the floor profile's dir gone (retired) while the registry still
/// names it? Then the `CLAUDE_CONFIG_DIR` floor points at nothing and must
/// be cleared. Pure over `is_dir`.
pub(crate) fn floor_dir_retired(legacy: &Legacy, is_dir: impl Fn(&Path) -> bool) -> bool {
    legacy
        .floor
        .as_deref()
        .and_then(|f| legacy.profiles.iter().find(|p| p.name == f))
        .is_some_and(|p| !is_dir(&p.dir))
}

/// The legacy profiles whose dir still holds a login once `gone` (the
/// dirs this retire run renamed, or in a dry run would rename) are
/// retired. Pure over `holds_login`: a dry run renamed nothing, so without
/// `gone` it would report the registry kept where the real run removes it.
pub(crate) fn still_holding_login<'a>(
    legacy: &'a Legacy,
    gone: &[PathBuf],
    holds_login: impl Fn(&Path) -> bool,
) -> Vec<&'a str> {
    legacy
        .profiles
        .iter()
        .filter(|p| !gone.contains(&p.dir) && holds_login(&p.dir))
        .map(|p| p.name.as_str())
        .collect()
}

/// Remove the legacy registry files. Returns the ones removed.
fn remove_legacy_files(home: &Path) -> io::Result<Vec<PathBuf>> {
    let dir = legacy_dir(home);
    let mut out = Vec::new();
    for f in LEGACY_FILES {
        let p = dir.join(f);
        fsx::guard(&p)?;
        match std::fs::remove_file(&p) {
            Ok(()) => out.push(p),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}

// ─── retire's precondition: import's steps 5 and 6 ran ────────────────────────

/// What `import` would still do in steps 5 and 6. Retire destroys their
/// inputs: it renames the floor profile's dir (step 5 reads its
/// `.claude.json`) and, once no dir holds a login, deletes `profiles.json`,
/// the only list of the old plugin-path prefixes step 6 rewrites. Gathered
/// by [`import_left`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct ImportLeft {
    /// Step 6: entries still to carry out of `~/.claude.shared`.
    pub shared: Vec<&'static str>,
    /// Step 5: why the floor profile's config is not merged yet, if not.
    pub merge: Option<String>,
    /// Step 6: recorded plugin paths still under an old prefix whose new
    /// path exists.
    pub plugin_paths: usize,
    /// Step 6: entries of `~/.claude.shared` a drain left because
    /// `~/.claude` holds a different one. A rerun never resolves them, so
    /// they do not hold retire back; retire names them.
    pub collided: Vec<PathBuf>,
    /// Step 7: why the last import did not finish attributing `~/.claude`'s
    /// grants (or switching to the floor profile's account), if it did not.
    /// Orca's first start after retire would read them back unchecked.
    pub step7: Option<String>,
}

/// Does step 6 still have `a` to do? A skip is reported by `import` and
/// never resolves on a rerun, so it does not hold retire back. A drain
/// counts here; [`import_left`] then keeps it only while it would still
/// move something ([`drain_scan`]). Pure.
fn shared_pending(a: &SharedAction) -> bool {
    matches!(
        a,
        SharedAction::Unlink | SharedAction::Move | SharedAction::Drain | SharedAction::Append
    )
}

/// Refuse retire while `import` has steps 5, 6 or 7 left. Pure.
pub(crate) fn retire_precondition(left: &ImportLeft) -> Result<(), String> {
    let mut why = Vec::new();
    if !left.shared.is_empty() {
        why.push(format!(
            "~/.claude.shared still holds {} for ~/.claude",
            left.shared.join(", ")
        ));
    }
    if let Some(m) = &left.merge {
        why.push(m.clone());
    }
    if left.plugin_paths > 0 {
        why.push(format!(
            "{} plugin path(s) still point into an old profile dir",
            left.plugin_paths
        ));
    }
    if let Some(m) = &left.step7 {
        why.push(format!("{m}; do not start Orca before it has"));
    }
    if why.is_empty() {
        return Ok(());
    }
    Err(format!(
        "{}; run `csm migrate import` first (retire renames the dirs and removes the profile list those steps read)",
        why.join("; ")
    ))
}

/// Step 5's state, [`ImportLeft::merge`]: `None` when there is nothing to
/// merge (no floor, no floor config) or it is merged. Pure over the two
/// objects; `target` is `Err` when the file is not a JSON object.
fn merge_left(
    from: Option<&Map<String, Value>>,
    target: Result<Option<Map<String, Value>>, ()>,
) -> Option<String> {
    let from = from?;
    match target {
        Err(()) => {
            Some("~/.claude.json is not a JSON object, so step 5 cannot merge into it".into())
        }
        Ok(None) => Some("~/.claude.json has not been created from the floor profile's".into()),
        Ok(Some(mut t)) => {
            // Presence only: see `merge_config_with`.
            let added = merge_config_with(&mut t, from, false);
            if !added.is_empty() {
                let mut keys: Vec<&str> = added.iter().map(String::as_str).take(3).collect();
                if added.len() > 3 {
                    keys.push("…");
                }
                Some(format!(
                    "the floor profile's trust/MCP settings are not all in ~/.claude.json ({})",
                    keys.join(", ")
                ))
            } else {
                None
            }
        }
    }
}

/// Gather [`ImportLeft`] from the machine (read-only).
fn import_left(home: &Path, state: &Path, legacy: &Legacy) -> ImportLeft {
    let (d, sh) = (home.join(".claude"), shared_root(home));
    let mut shared = Vec::new();
    let mut collided = Vec::new();
    for (n, a) in shared_plan(home) {
        if !shared_pending(&a) {
            continue;
        }
        if a == SharedAction::Drain {
            // A drain that can only collide again is done as far as a
            // rerun goes. An unreadable tree counts as pending.
            if let Ok(scan) = drain_scan(&sh.join(n), &d.join(n)) {
                collided.extend(scan.collided);
                if !scan.moves {
                    continue;
                }
            }
        }
        shared.push(n);
    }
    let floor = legacy
        .floor
        .as_ref()
        .and_then(|n| legacy.profiles.iter().find(|p| &p.name == n))
        .map(|p| read_floor_config(&p.dir));
    // A floor config that cannot be read holds retire back: renaming its
    // dir would leave its trust and MCP settings behind unreported.
    let unreadable = match &floor {
        Some(Err(why)) => Some(format!("{why}; {FLOOR_CONFIG_FIX}")),
        _ => None,
    };
    let (from, floor_bytes) = match floor {
        Some(Ok(Some((m, b)))) => (Some(m), Some(b)),
        _ => (None, None),
    };
    let recorded = merge_recorded(
        std::fs::read_to_string(merge_marker(state)).ok().as_deref(),
        floor_bytes.as_deref(),
    );
    let target_path = runtime_paths(None, home, |p| p.exists()).config_path;
    let target = match crate::orca::read_capped_bytes(&target_path, 64 * 1024 * 1024) {
        Ok(None) => Ok(None),
        Ok(Some(b)) => match serde_json::from_slice::<Value>(&b) {
            Ok(Value::Object(m)) => Ok(Some(m)),
            _ => Err(()),
        },
        Err(_) => Err(()),
    };
    // Step 5 recorded as done for this very floor config: a key the user
    // removed from ~/.claude.json since does not hold retire back.
    let merge = if unreadable.is_some() {
        unreadable
    } else if recorded {
        None
    } else {
        merge_left(from.as_ref(), target)
    };
    let plugin_paths = plugin_paths_left(home, &legacy_dirs(legacy));
    let step7 = (!step7_marker(state).is_file()).then(|| {
        "`csm migrate import` has not completed step 7 (attributing ~/.claude's grants, or \
         the switch to the floor profile's account)"
            .to_owned()
    });
    ImportLeft {
        shared,
        merge,
        plugin_paths,
        collided,
        step7,
    }
}

/// `<state>/migrate/attributed`: the last `migrate import` completed step 7
/// (`~/.claude`'s grants attributed, or `D` switched to the floor
/// profile's account). Import removes it before step 4, so a later import
/// that stops early leaves retire refusing again.
fn step7_marker(state: &Path) -> PathBuf {
    state.join("migrate").join("attributed")
}

/// Record step 7 as done; `how` is a short word for the operator.
fn record_step7(state: &Path, how: &str) -> io::Result<()> {
    let p = step7_marker(state);
    if let Some(dir) = p.parent() {
        fsx::create_dir_all(dir, 0o700)?;
    }
    fsx::write_atomic(&p, format!("{how}\n").as_bytes(), fsx::WriteOpts::PRIVATE)
}

/// Forget a recorded step 7 (import is about to run it again).
fn clear_step7(state: &Path) -> io::Result<()> {
    match std::fs::remove_file(step7_marker(state)) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// `<state>/migrate/config-merged`: the digest of the floor profile's
/// `.claude.json` that step 5 last merged into `~/.claude.json`.
fn merge_marker(state: &Path) -> PathBuf {
    state.join("migrate").join("config-merged")
}

/// The digest [`merge_marker`] holds. Pure.
fn config_digest(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    keychain::hex_lower(&Sha256::digest(bytes))
}

/// Whether the marker records a merge of exactly `floor` (the floor
/// profile's `.claude.json` as it is now). Pure.
fn merge_recorded(marker: Option<&str>, floor: Option<&[u8]>) -> bool {
    match (marker, floor) {
        (Some(m), Some(f)) => m.trim() == config_digest(f),
        _ => false,
    }
}

/// Record that step 5 merged `floor` (the floor config's bytes).
fn record_merge(state: &Path, floor: &[u8]) -> io::Result<()> {
    let p = merge_marker(state);
    if let Some(dir) = p.parent() {
        fsx::create_dir_all(dir, 0o700)?;
    }
    fsx::write_atomic(&p, config_digest(floor).as_bytes(), fsx::WriteOpts::PRIVATE)
}

/// How many recorded plugin paths [`apply_plugin_paths`] would still
/// rewrite (read-only): 0 while `~/.claude/plugins` is not a real dir yet,
/// which [`shared_plan`] reports instead.
fn plugin_paths_left(home: &Path, dirs: &[PathBuf]) -> usize {
    let root = home.join(".claude").join("plugins");
    match std::fs::symlink_metadata(&root) {
        Ok(m) if m.is_dir() && !m.file_type().is_symlink() => {}
        _ => return 0,
    }
    let prefixes = old_plugin_prefixes(home, dirs);
    let new_root = root.to_string_lossy().into_owned();
    PLUGIN_FILES
        .iter()
        .filter_map(|f| read_json_object(&root.join(f)))
        .map(|m| {
            let mut v = Value::Object(m);
            rewrite_plugin_paths(&mut v, &prefixes, &new_root, &|p| Path::new(p).exists())
                .rewritten
                .len()
        })
        .sum()
}

/// Name what profile `name`'s `.claude.json` holds that `~/.claude.json`
/// lacks, pointing at the retired copy: retire moves it out of view.
fn print_left_config(plan: &Plan, legacy: &Legacy, name: &str) {
    if let Some((_, keys)) = plan.differs.iter().find(|(n, _)| n == name) {
        for line in differs_lines(name, &profile_config(legacy, name, true), keys) {
            println!("{line}");
        }
    }
}

fn retire_cmd(dry_run: bool, only: &[String]) -> anyhow::Result<()> {
    let procs = SystemProcs;
    let (ctx, view, legacy) = setup()?;
    for n in only {
        if !legacy.profiles.iter().any(|p| &p.name == n) {
            bail!("csm migrate retire: no legacy profile named {n:?}");
        }
    }
    if dry_run {
        if let Err(e) = gate(&ctx, &procs, &legacy) {
            println!("csm migrate retire would refuse now: {e}");
        }
    } else {
        gate(&ctx, &procs, &legacy)?;
    }
    // Steps 5 and 6 read what retire renames or deletes: refuse until they
    // ran, or a later import silently skips them (the plugin paths would
    // dangle into `<dir>.retired`).
    let left = import_left(&ctx.env.home, &ctx.state, &legacy);
    if let Err(why) = retire_precondition(&left) {
        if dry_run {
            println!("csm migrate retire would refuse now: {why}");
            return Ok(());
        }
        bail!("csm migrate retire: {why}");
    }
    if !left.collided.is_empty() {
        println!(
            "note: ~/.claude.shared keeps entries that collided with ~/.claude's own; \
             merge or delete them by hand: {}",
            collided_list(&left.collided)
        );
    }
    let http = SystemHttp::from_env();
    let home = ctx.env.home.clone();
    // Read the grants: retire compares each dir's with its stash (design
    // section 9 step 8: only a stash step 4 left current).
    let plan = build_plan(&ctx, &view, &legacy, Probe::Read);
    let filed: Vec<String> = Quarantine::new(ctx.os(), &ctx.state)
        .list()
        .into_iter()
        .map(|m| m.fingerprint)
        .collect();
    let quarantined = |fp: &str| filed.iter().any(|f| f == fp);
    let host: Vec<AccountRecord> = view.host_accounts().cloned().collect();
    let mut failed = 0usize;
    // Dirs this run retires (or, dry, would retire).
    let mut gone: Vec<PathBuf> = Vec::new();
    for r in plan
        .rows
        .iter()
        .filter(|r| only.is_empty() || only.contains(&r.facts.name))
    {
        let grant = dir_grant_state(&r.facts, &r.status, &quarantined);
        let check = match &r.status {
            Status::InOrca { id, .. } if grant == DirGrant::Superseded => {
                stash_check(&ctx, &view, &http, id)
            }
            _ => StashCheck::Unverified,
        };
        let name = &r.facts.name;
        let known = identity_match(&r.facts, &host).map(|rec| rec.id.as_str());
        let verdict = retire_verdict(
            &r.status,
            known,
            check,
            grant,
            &r.facts.dir,
            &home,
            r.facts.exists,
        )
        .and_then(|id| references_verdict(&references_to(&home, &r.facts.dir)).map(|()| id));
        match verdict {
            Err(why) => println!("{name}: skipped: {why}"),
            Ok(id) if dry_run => {
                gone.push(r.facts.dir.clone());
                let why = match r.status {
                    Status::NoCredentials => format!("no grant left; its account is {id}"),
                    _ => format!("stash {id} verified"),
                };
                println!(
                    "{name}: would quarantine its grants and rename {} ({why})",
                    r.facts.dir.display()
                );
                print_left_config(&plan, &legacy, name);
            }
            Ok(id) => match retire_dir(&ctx, &view, &http, &r.facts.dir, &id) {
                Ok(line) => {
                    gone.push(r.facts.dir.clone());
                    println!("{name}: {line}");
                    print_left_config(&plan, &legacy, name);
                }
                Err(e) => {
                    failed += 1;
                    eprintln!("{name}: {e:#}");
                }
            },
        }
    }
    // The registry goes once no legacy dir holds a login any more. A dry
    // run renamed nothing, so it leaves out the dirs it would retire.
    let remaining = still_holding_login(&legacy, &gone, |d| {
        d.is_dir() && !dir_grant_sources(&ctx, d).is_empty()
    });
    if !remaining.is_empty() {
        println!(
            "legacy registry kept: {} still hold a login",
            remaining.join(", ")
        );
        // The floor names the floor profile's dir: once that dir is retired
        // the floor goes too, registry or not, so Orca and GUI claude never
        // start in a fresh empty dir under the old name.
        if floor_dir_retired(&legacy, |d| d.is_dir() && !gone.iter().any(|g| g == d)) {
            if dry_run {
                println!("would clear the CLAUDE_CONFIG_DIR floor (its dir is retired)");
            } else {
                match unset_floor_env() {
                    Ok(()) => println!("cleared the CLAUDE_CONFIG_DIR floor (its dir is retired)"),
                    Err(e) => {
                        failed += 1;
                        eprintln!(
                            "cannot clear the CLAUDE_CONFIG_DIR floor ({e}); a rerun retries it, or run `{FLOOR_UNSET_HINT}`"
                        );
                    }
                }
            }
        }
    } else if dry_run {
        println!(
            "would remove ~/.config/claude-as/{{profiles.json,default,floor-dir}} and the CLAUDE_CONFIG_DIR floor"
        );
    } else if !legacy.profiles.is_empty() {
        match retire_registry(unset_floor_env, || remove_legacy_files(&home)) {
            Ok(lines) => lines.iter().for_each(|l| println!("{l}")),
            Err(e) => {
                failed += 1;
                eprintln!("{e}");
            }
        }
    }
    if failed > 0 {
        bail!("csm migrate retire: {failed} step(s) failed");
    }
    Ok(())
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orca::HostEnv;
    use crate::orca::http::FakeHttp;
    use crate::orca::testsupport::{
        FakeProcs, creds_json, make_stash, oauth_json, record_json, write_store,
    };
    use serde_json::json;

    fn os(ss: &[&str]) -> Vec<OsString> {
        ss.iter().map(OsString::from).collect()
    }

    /// The floor is cleared before the registry goes: a failed unset keeps
    /// the registry (so a rerun, which acts only while it lists profiles,
    /// retries), and names the manual command.
    #[test]
    fn retire_clears_the_floor_before_removing_the_registry() {
        let removed = std::cell::Cell::new(false);
        let err = retire_registry(
            || Err(io::Error::other("launchctl exited 1")),
            || {
                removed.set(true);
                Ok(vec![])
            },
        )
        .unwrap_err();
        assert!(!removed.get(), "registry must stay when the unset fails");
        assert!(err.contains(FLOOR_UNSET_HINT), "{err}");
        assert!(err.contains("rerun"), "{err}");

        let lines = retire_registry(
            || Ok(()),
            || {
                Ok(vec![PathBuf::from(
                    "/Users/example/.config/claude-as/default",
                )])
            },
        )
        .unwrap();
        assert_eq!(lines[0], "cleared the CLAUDE_CONFIG_DIR floor");
        assert!(lines[1].starts_with("removed "));
    }

    #[test]
    fn parse_verbs_and_flags() {
        assert_eq!(parse(&[]).unwrap(), MigrateCmd::Plan);
        assert_eq!(parse(&os(&["plan"])).unwrap(), MigrateCmd::Plan);
        assert_eq!(
            parse(&os(&["import", "--dry-run"])).unwrap(),
            MigrateCmd::Import { dry_run: true }
        );
        assert_eq!(
            parse(&os(&["retire", "work"])).unwrap(),
            MigrateCmd::Retire {
                dry_run: false,
                only: vec!["work".into()]
            }
        );
        assert!(parse(&os(&["plan", "--dry-run"])).is_err());
        assert!(parse(&os(&["import", "x"])).is_err());
        assert!(parse(&os(&["import", "--force"])).is_err());
        assert!(parse(&os(&["go"])).is_err());
    }

    #[test]
    fn legacy_floor_comes_from_default_then_floor_dir() {
        let pj = r#"{"work":"/Users/example/.claude.work","home":"/Users/example/.claude.home"}"#;
        let l = parse_legacy(Some(pj), Some("home\n"), None).unwrap();
        assert_eq!(l.floor.as_deref(), Some("home"));
        assert_eq!(l.profiles[0].name, "home");
        let l = parse_legacy(
            Some(pj),
            Some("gone"),
            Some("/Users/example/.claude.work/\n"),
        )
        .unwrap();
        assert_eq!(l.floor.as_deref(), Some("work"));
        assert_eq!(parse_legacy(None, None, None).unwrap(), Legacy::default());
        assert!(parse_legacy(Some("[1]"), None, None).is_err());
    }

    fn rec(id: &str, email: &str) -> AccountRecord {
        AccountRecord::from_value(&json!({
            "id": id, "email": email, "managedAuthPath": "/x", "authMethod": "subscription-oauth"
        }))
        .unwrap()
    }

    fn facts(email: Option<&str>, grant: Option<(&str, f64)>) -> ProfileFacts {
        ProfileFacts {
            name: "work".into(),
            dir: "/Users/example/.claude.work".into(),
            exists: true,
            email: email.map(str::to_owned),
            organization_uuid: None,
            grant_sources: if grant.is_some() {
                vec!["file"]
            } else {
                vec![]
            },
            grant: grant.map(|(fp, e)| GrantFacts {
                fingerprint: fp.into(),
                expires_at: Some(e),
                side: BTreeMap::new(),
            }),
        }
    }

    /// Two profiles logged into the same account: only the second is
    /// decided again after the first one's import; other identities, rows
    /// already in Orca and rows with no identity are not.
    #[test]
    fn a_second_profile_of_an_account_is_decided_after_the_first_import() {
        let row = |email: Option<&str>, status: Status| Row {
            facts: facts(email, Some(("fp", 1.0))),
            status,
            stash: None,
            floor: false,
        };
        let rows = [
            row(Some("alice@example.com"), Status::ToImport),
            row(Some("bob@example.com"), Status::ToImport),
            row(Some(" Alice@Example.com "), Status::ToImport),
            row(None, Status::ToImport),
            row(None, Status::ToImport),
            row(
                Some("bob@example.com"),
                Status::InOrca {
                    id: "b".into(),
                    fresher: Some(false),
                },
            ),
            row(Some("alice@example.com"), Status::ToImport),
        ];
        assert_eq!(
            repeat_imports(&rows),
            [false, false, true, false, false, false, true]
        );
        // Classified again once the first import added the account, the
        // repeat is in Orca: read back only if its grant is fresher.
        let rec = AccountRecord::from_value(&record_json(
            Path::new("/Users/example/orca"),
            "acct-a",
            "alice@example.com",
            None,
        ))
        .unwrap();
        let older = |_: &str| {
            Some(GrantFacts {
                fingerprint: "fp-stash".into(),
                expires_at: Some(5.0),
                side: BTreeMap::new(),
            })
        };
        let st = classify(&rows[2].facts, std::slice::from_ref(&rec), &older);
        assert_eq!(
            st,
            Status::InOrca {
                id: "acct-a".into(),
                fresher: Some(false)
            }
        );
        assert_eq!(row_action(&st, false), RowAction::Skip);
    }

    #[test]
    fn classify_each_status() {
        let records = vec![rec("acct-a", "alice@example.com")];
        let none = |_: &str| None;
        assert_eq!(
            classify(&facts(Some("alice@example.com"), None), &records, &none),
            Status::NoCredentials
        );
        assert_eq!(
            classify(
                &facts(Some("bob@example.com"), Some(("f1", 1.0))),
                &records,
                &none
            ),
            Status::ToImport
        );
        assert_eq!(
            classify(&facts(None, Some(("f1", 1.0))), &records, &none),
            Status::ToImport
        );
        assert_eq!(
            classify(
                &facts(Some("Alice@Example.com"), Some(("f1", 1.0))),
                &records,
                &none
            ),
            Status::InOrca {
                id: "acct-a".into(),
                fresher: Some(true)
            }
        );
        let same = |_: &str| {
            Some(GrantFacts {
                fingerprint: "f1".into(),
                expires_at: Some(9.0),
                side: BTreeMap::new(),
            })
        };
        assert_eq!(
            classify(
                &facts(Some("alice@example.com"), Some(("f1", 1.0))),
                &records,
                &same
            ),
            Status::InOrca {
                id: "acct-a".into(),
                fresher: Some(false)
            }
        );
        let older = |_: &str| {
            Some(GrantFacts {
                fingerprint: "f0".into(),
                expires_at: Some(5.0),
                side: BTreeMap::new(),
            })
        };
        assert!(matches!(
            classify(
                &facts(Some("alice@example.com"), Some(("f1", 4.0))),
                &records,
                &older
            ),
            Status::InOrca {
                fresher: Some(false),
                ..
            }
        ));
        assert!(matches!(
            classify(
                &facts(Some("alice@example.com"), Some(("f1", 6.0))),
                &records,
                &older
            ),
            Status::InOrca {
                fresher: Some(true),
                ..
            }
        ));
    }

    /// A presence-only probe knows a grant exists but not whether it is
    /// fresher: `fresher` stays unknown and the stash is never consulted.
    #[test]
    fn classify_a_presence_only_grant() {
        let records = vec![rec("acct-a", "alice@example.com")];
        let mut f = facts(Some("alice@example.com"), None);
        f.grant_sources = vec!["scoped-keychain"];
        let never = |_: &str| -> Option<GrantFacts> { panic!("stash read in presence mode") };
        assert_eq!(
            classify(&f, &records, &never),
            Status::InOrca {
                id: "acct-a".into(),
                fresher: None
            }
        );
        f.email = Some("bob@example.com".into());
        assert_eq!(classify(&f, &records, &never), Status::ToImport);
    }

    #[test]
    fn carry_seeds_a_missing_file_without_the_identity() {
        let from = json!({
            "oauthAccount": {"emailAddress": "alice@example.com"},
            "hasCompletedOnboarding": true,
            "numStartups": 7,
            "theme": "dark",
            "projects": {"/Users/example/src/app": {"hasTrustDialogAccepted": true}}
        })
        .as_object()
        .unwrap()
        .clone();
        let c = carry_config(None, &from);
        assert_eq!(c.seeded, Some(4));
        assert!(c.changed());
        assert!(c.map.get("oauthAccount").is_none());
        assert_eq!(c.map["hasCompletedOnboarding"], true);
        assert_eq!(c.map["numStartups"], 7);
        assert_eq!(
            c.map["projects"]["/Users/example/src/app"]["hasTrustDialogAccepted"],
            true
        );
        // A floor file with nothing but settings still seeds.
        let bare = json!({"theme": "light", "oauthAccount": {}})
            .as_object()
            .unwrap()
            .clone();
        let c = carry_config(None, &bare);
        assert_eq!(c.seeded, Some(1));
        assert!(c.added.is_empty() && c.changed());
        // An existing file is never seeded, only merged: the trust fields
        // and the onboarding keys it lacks, nothing else.
        let existing = json!({"numStartups": 1}).as_object().unwrap().clone();
        let c = carry_config(Some(existing), &from);
        assert_eq!(c.seeded, None);
        assert_eq!(c.map["numStartups"], 1);
        assert_eq!(c.map["theme"], "dark");
        assert!(c.map.get("oauthAccount").is_none());
        assert_eq!(
            c.added,
            vec![
                "projects[/Users/example/src/app].hasTrustDialogAccepted",
                "hasCompletedOnboarding",
                "theme"
            ]
        );
    }

    /// Step 5 on a machine that only ever ran with the floor: no
    /// `~/.claude.json`, so the floor profile's file seeds it.
    #[test]
    fn merge_step_creates_the_missing_config_from_the_floor() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let ctx = Context::from_env(
            HostEnv::for_test(home, HostOs::Linux),
            &FakeProcs::default(),
        );
        let work = home.join(".claude.work");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(
            work.join(".claude.json"),
            json!({"oauthAccount": {"emailAddress": "alice@example.com"},
                   "hasCompletedOnboarding": true, "userID": "u-1"})
            .to_string(),
        )
        .unwrap();
        let legacy = Legacy {
            profiles: vec![LegacyProfile {
                name: "work".into(),
                dir: work,
            }],
            floor: Some("work".into()),
        };
        let line = merge_step(&ctx, &legacy)
            .unwrap()
            .expect("a file was written");
        assert!(line.contains("created"), "{line}");
        let got: Value =
            serde_json::from_slice(&std::fs::read(home.join(".claude.json")).unwrap()).unwrap();
        assert_eq!(got["hasCompletedOnboarding"], true);
        assert_eq!(got["userID"], "u-1");
        assert!(got.get("oauthAccount").is_none());
        // Rerun: the file exists now and nothing is left to merge.
        assert!(merge_step(&ctx, &legacy).unwrap().is_none());
    }

    /// Step 5 records what it merged: an MCP server the user removes from
    /// ~/.claude.json afterwards does not hold retire back (a rerun of
    /// import would put it back), while a floor config that changed since
    /// is checked again.
    #[test]
    fn a_recorded_merge_survives_a_later_removal() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let ctx = Context::from_env(
            HostEnv::for_test(home, HostOs::Linux),
            &FakeProcs::default(),
        );
        let work = home.join(".claude.work");
        std::fs::create_dir_all(&work).unwrap();
        let floor = json!({"mcpServers": {"docs": {"command": "x"}}});
        std::fs::write(work.join(".claude.json"), floor.to_string()).unwrap();
        let legacy = Legacy {
            profiles: vec![LegacyProfile {
                name: "work".into(),
                dir: work.clone(),
            }],
            floor: Some("work".into()),
        };
        assert!(import_left(home, &ctx.state, &legacy).merge.is_some());
        assert!(merge_step(&ctx, &legacy).unwrap().is_some());
        assert!(import_left(home, &ctx.state, &legacy).merge.is_none());
        // The user drops the server again.
        std::fs::write(home.join(".claude.json"), "{}").unwrap();
        assert!(import_left(home, &ctx.state, &legacy).merge.is_none());
        // The floor config changed since the merge: checked again.
        let changed = json!({"mcpServers": {"docs": {"command": "x"}, "web": {"command": "y"}}});
        std::fs::write(work.join(".claude.json"), changed.to_string()).unwrap();
        let m = import_left(home, &ctx.state, &legacy).merge.unwrap();
        assert!(m.contains("mcpServers"), "{m}");
        // A rerun with nothing to add records the merge too.
        assert!(merge_step(&ctx, &legacy).unwrap().is_some());
        std::fs::remove_file(merge_marker(&ctx.state)).unwrap();
        assert!(merge_step(&ctx, &legacy).unwrap().is_none());
        assert!(merge_recorded(
            std::fs::read_to_string(merge_marker(&ctx.state))
                .ok()
                .as_deref(),
            Some(changed.to_string().as_bytes())
        ));
        assert!(!merge_recorded(None, Some(b"{}")));
        assert!(!merge_recorded(Some("abc"), None));
    }

    /// An import Orca refused or did not confirm is a failed row, never
    /// `imported`: the run then exits non-zero and skips the floor switch.
    #[test]
    fn import_line_fails_a_refused_or_unconfirmed_redo() {
        use crate::orca::store::RedoOutcome;
        let change = |redo| add::AccountChange {
            route: add::Route::OfflineThenRpc,
            id: None,
            email: Some("carol@example.com".into()),
            redo,
            leftover: None,
        };
        assert_eq!(
            import_line(&change(None)).unwrap(),
            "imported carol@example.com"
        );
        assert!(import_line(&change(Some(RedoOutcome::Reissued(Value::Null)))).is_ok());
        let e = import_line(&change(Some(RedoOutcome::Failed("no".into())))).unwrap_err();
        assert!(e.contains("not imported") && e.contains("refused"), "{e}");
        let e = import_line(&change(Some(RedoOutcome::Uncertain("timeout".into())))).unwrap_err();
        assert!(e.contains("did not confirm") && e.contains("doctor"), "{e}");
    }

    #[test]
    fn retire_precondition_names_what_import_has_left() {
        assert!(retire_precondition(&ImportLeft::default()).is_ok());
        let e = retire_precondition(&ImportLeft {
            shared: vec!["projects", "plugins"],
            merge: Some("step 5 left".into()),
            plugin_paths: 2,
            collided: vec![PathBuf::from(
                "/Users/example/.claude.shared/sessions/1.json",
            )],
            step7: None,
        })
        .unwrap_err();
        assert!(e.contains("projects, plugins"), "{e}");
        assert!(e.contains("step 5 left"), "{e}");
        assert!(e.contains("2 plugin path(s)"), "{e}");
        assert!(e.contains("csm migrate import"), "{e}");
        // A skip never resolves on a rerun: it does not hold retire back.
        assert!(!shared_pending(&SharedAction::Skip("x".into())));
        assert!(!shared_pending(&SharedAction::Nothing));
        for a in [
            SharedAction::Unlink,
            SharedAction::Move,
            SharedAction::Drain,
            SharedAction::Append,
        ] {
            assert!(shared_pending(&a));
        }
    }

    #[test]
    fn merge_left_follows_carry_config() {
        let from = json!({"hasCompletedOnboarding": true,
            "projects": {"/w": {"hasTrustDialogAccepted": true}},
            "mcpServers": {"docs": {"command": "x"}}});
        let from = from.as_object().unwrap();
        assert!(merge_left(None, Ok(None)).is_none());
        assert!(
            merge_left(Some(from), Ok(None))
                .unwrap()
                .contains("created")
        );
        assert!(
            merge_left(Some(from), Err(()))
                .unwrap()
                .contains("not a JSON object")
        );
        let partial = json!({"projects": {"/w": {"hasTrustDialogAccepted": true}}});
        let m = merge_left(Some(from), Ok(partial.as_object().cloned())).unwrap();
        assert!(m.contains("mcpServers.docs"), "{m}");
        // Merged (a value the user changed since still counts as merged,
        // even a default that import itself would upgrade: retire must not
        // be held back for good by the user's own later choice).
        let done = json!({"projects": {"/w": {"hasTrustDialogAccepted": false}},
            "mcpServers": {"docs": {"command": "y"}}, "hasCompletedOnboarding": false});
        assert!(merge_left(Some(from), Ok(done.as_object().cloned())).is_none());
        let mut t = done.as_object().cloned().unwrap();
        assert_eq!(
            merge_config(&mut t, from),
            vec!["projects[/w].hasTrustDialogAccepted"]
        );
    }

    /// Retire before import would rename the floor dir and delete the
    /// profile list, and a later import would then skip steps 5 and 6
    /// without a word (plugin paths dangling into `<dir>.retired`). The
    /// precondition holds retire back until import has run them.
    #[cfg(unix)]
    #[test]
    fn retire_waits_until_import_ran_steps_5_and_6() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let ctx = Context::from_env(
            HostEnv::for_test(&home, HostOs::Linux),
            &FakeProcs::default(),
        );
        let work = home.join(".claude.work");
        let shared = home.join(".claude.shared");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(shared.join("projects")).unwrap();
        std::fs::create_dir_all(shared.join("plugins").join("cache").join("p1")).unwrap();
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::os::unix::fs::symlink(
            shared.join("projects"),
            home.join(".claude").join("projects"),
        )
        .unwrap();
        std::os::unix::fs::symlink(shared.join("plugins"), home.join(".claude").join("plugins"))
            .unwrap();
        std::os::unix::fs::symlink(shared.join("plugins"), work.join("plugins")).unwrap();
        let old = work.join("plugins").join("cache").join("p1");
        std::fs::write(
            shared.join("plugins").join("installed_plugins.json"),
            json!({"plugins": {"p1": [{"installPath": old.to_string_lossy()}]}}).to_string(),
        )
        .unwrap();
        std::fs::write(
            work.join(".claude.json"),
            json!({"projects": {"/w": {"hasTrustDialogAccepted": true}}}).to_string(),
        )
        .unwrap();
        let legacy = Legacy {
            profiles: vec![LegacyProfile {
                name: "work".into(),
                dir: work.clone(),
            }],
            floor: Some("work".into()),
        };
        let left = import_left(&home, &ctx.state, &legacy);
        assert!(left.shared.contains(&"projects"), "{left:?}");
        assert!(left.shared.contains(&"plugins"), "{left:?}");
        assert!(left.merge.is_some(), "{left:?}");
        assert!(retire_precondition(&left).is_err());

        // Import's steps 5 and 6.
        assert!(merge_step(&ctx, &legacy).unwrap().is_some());
        apply_shared(&home).unwrap();
        let left = import_left(&home, &ctx.state, &legacy);
        assert!(left.shared.is_empty() && left.merge.is_none(), "{left:?}");
        assert_eq!(left.plugin_paths, 1, "the registry still names the old dir");
        assert!(retire_precondition(&left).is_err());
        apply_plugin_paths(&home, &legacy_dirs(&legacy), &state).unwrap();
        let left = import_left(&home, &ctx.state, &legacy);
        assert!(left.step7.is_some(), "{left:?}");
        assert!(retire_precondition(&left).is_err());
        // Import's step 7.
        record_step7(&ctx.state, "switched").unwrap();
        let left = import_left(&home, &ctx.state, &legacy);
        assert_eq!(left, ImportLeft::default());
        assert!(retire_precondition(&left).is_ok());
    }

    #[test]
    fn smart_carries_only_what_csm_still_reads() {
        for n in [
            "01234567-89ab-cdef-0123-456789abcdef.json",
            "titles.tsv",
            "scan-meta-v2.-Users-example-src-app.tsv",
        ] {
            assert!(smart_carries(n), "{n}");
        }
        for n in [
            ".usage-cache.json",
            ".usage-fetch-failed",
            ".last-switch",
            "01234567-89ab-cdef-0123-456789abcdef.pid",
            "01234567-89ab-cdef-0123-456789abcdef.switched",
            "work.json",
            "scan-meta.x.tsv",
            "usage",
        ] {
            assert!(!smart_carries(n), "{n}");
        }
    }

    #[test]
    fn apply_smart_moves_sidecars_and_keeps_collisions() {
        let tmp = tempfile::tempdir().unwrap();
        let old = legacy_smart_dir(tmp.path());
        let new = tmp.path().join(".local").join("state").join("csm");
        std::fs::create_dir_all(old.join("usage")).unwrap();
        std::fs::create_dir_all(&new).unwrap();
        let sid = "01234567-89ab-cdef-0123-456789abcdef";
        let other = "11234567-89ab-cdef-0123-456789abcdef";
        std::fs::write(old.join(format!("{sid}.json")), b"{\"cwd\":\"/x\"}").unwrap();
        std::fs::write(old.join("titles.tsv"), b"t\tsid\t1\n").unwrap();
        std::fs::write(old.join(format!("{other}.json")), b"old").unwrap();
        std::fs::write(new.join(format!("{other}.json")), b"new").unwrap();
        std::fs::write(old.join(".usage-cache.json"), b"{}").unwrap();
        std::fs::write(old.join("usage").join("work.json"), b"{}").unwrap();
        assert_eq!(smart_preview(&old), Some((3, 2)));

        let line = apply_smart(&old, &new).unwrap().unwrap();
        assert!(line.contains("moved 2"), "{line}");
        assert!(line.contains("1 already there"), "{line}");
        assert_eq!(
            std::fs::read(new.join(format!("{sid}.json"))).unwrap(),
            b"{\"cwd\":\"/x\"}"
        );
        assert!(new.join("titles.tsv").is_file());
        assert_eq!(
            std::fs::read(new.join(format!("{other}.json"))).unwrap(),
            b"new"
        );
        assert!(
            old.join(format!("{other}.json")).is_file(),
            "collision stays"
        );
        assert!(old.join(".usage-cache.json").is_file());
        assert!(!new.join(".usage-cache.json").exists());
        assert!(!new.join("usage").exists());
        assert!(
            apply_smart(&tmp.path().join("none"), &new)
                .unwrap()
                .is_none()
        );
    }

    /// `migrate plan` on macOS reads no secret: every Keychain call is a
    /// presence probe (no `-w`), and neither a legacy item nor a stash is
    /// read.
    #[cfg(unix)]
    #[test]
    fn plan_on_macos_probes_the_keychain_for_presence_only() {
        let fake = crate::orca::testsupport::FakeSecurity::install();
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let env = HostEnv::for_test(home, HostOs::MacOs);
        let procs = FakeProcs::default();
        let ctx = Context::from_env(env.clone(), &procs);
        let ud = ctx.user_data.dir.clone();
        let alice = serde_json::to_vec(&oauth_json("u-a", "alice@example.com", None)).unwrap();
        make_stash(&ud, "acct-a", Some(&alice), None);
        fake.put(
            keychain::STASH_SERVICE,
            "acct-a",
            creds_json("at-stash", "rt-stash", 1).as_bytes(),
        );
        write_store(
            &ud,
            &[record_json(&ud, "acct-a", "alice@example.com", None)],
            Some("acct-a"),
        );
        let work = home.join(".claude.work");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(
            work.join(".claude.json"),
            json!({"oauthAccount": {"emailAddress": "alice@example.com"}}).to_string(),
        )
        .unwrap();
        fake.put(
            &keychain::runtime_service(Some(&work.to_string_lossy())),
            &ctx.keychain_user.acct,
            creds_json("at-dir", "rt-dir", 2).as_bytes(),
        );
        let cfg = legacy_dir(home);
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(cfg.join("profiles.json"), json!({"work": work}).to_string()).unwrap();
        let legacy = load_legacy(home).unwrap();
        let view = crate::orca::snapshot_with(&env, &SnapshotOptions::default(), &procs);
        let before = fake.argv().len();

        let plan = build_plan(&ctx, &view, &legacy, Probe::Presence);
        let calls: Vec<String> = fake.argv().split_off(before);
        assert!(!calls.is_empty(), "the dir's items were probed");
        for c in &calls {
            assert!(c.starts_with("find-generic-password"), "{c}");
            assert!(!c.split(' ').any(|t| t == "-w"), "a secret read: {c}");
        }
        assert_eq!(
            plan.rows[0].status,
            Status::InOrca {
                id: "acct-a".into(),
                fresher: None
            }
        );
        assert_eq!(plan.rows[0].facts.grant_sources, vec!["scoped-keychain"]);
        assert!(plan.rows[0].stash.is_none());
        let text = render_plan(&plan);
        assert!(
            text.contains("present (scoped-keychain; not read)"),
            "{text}"
        );
        for secret in ["at-dir", "rt-dir", "at-stash", "rt-stash"] {
            assert!(!text.contains(secret), "{text}");
        }
    }

    /// A Keychain probe or read that fails (a locked Keychain) never reads
    /// as "no login": plan reports it, the registry's `remaining` check
    /// counts the dir, and retire moves nothing.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_keychain_item_counts_as_a_login() {
        let fake = crate::orca::testsupport::FakeSecurity::install();
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let env = HostEnv::for_test(home, HostOs::MacOs);
        let procs = FakeProcs::default();
        let ctx = Context::from_env(env.clone(), &procs);
        let work = home.join(".claude.work");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(
            work.join(".claude.json"),
            json!({"oauthAccount": {"emailAddress": "alice@example.com"}}).to_string(),
        )
        .unwrap();
        let svc = keychain::runtime_service(Some(&work.to_string_lossy()));
        fake.put(
            &svc,
            &ctx.keychain_user.acct,
            creds_json("at-dir", "rt-dir", 2).as_bytes(),
        );
        fake.fail_find(&svc, true);

        assert_eq!(dir_grant_sources(&ctx, &work), vec![KEYCHAIN_UNREADABLE]);
        let lp = LegacyProfile {
            name: "work".into(),
            dir: work.clone(),
        };
        let facts = profile_facts(&ctx, &lp, Probe::Read);
        assert!(
            facts.grant_sources.contains(&KEYCHAIN_UNREADABLE),
            "{facts:?}"
        );
        assert_ne!(classify(&facts, &[], &|_| None), Status::NoCredentials);
        let view = crate::orca::snapshot_with(&env, &SnapshotOptions::default(), &procs);
        let err = retire_dir(&ctx, &view, &FakeHttp::default(), &work, "acct-a").unwrap_err();
        assert!(format!("{err:#}").contains("nothing moved"), "{err:#}");
        assert!(work.is_dir());
        assert!(!home.join(".claude.work.retired").exists());
        assert!(Quarantine::new(HostOs::MacOs, &ctx.state).list().is_empty());
    }

    #[test]
    fn merge_keeps_existing_keys_and_reports_additions() {
        let mut target = json!({
            "numStartups": 3,
            "projects": {"/Users/example/src/app": {"hasTrustDialogAccepted": false}},
            "mcpServers": {"kept": {"command": "a"}}
        })
        .as_object()
        .unwrap()
        .clone();
        let from = json!({
            "oauthAccount": {"emailAddress": "alice@example.com"},
            "projects": {
                "/Users/example/src/app": {"hasTrustDialogAccepted": true, "allowedTools": ["x"], "history": [1]},
                "/Users/example/src/lib": {"hasTrustDialogAccepted": true},
                "/Users/example/src/none": {"history": []},
                "/Users/example/src/mcp": {"mcpServers": {"local": {"command": "l"}}}
            },
            "mcpServers": {"kept": {"command": "b"}, "new": {"command": "c"}}
        })
        .as_object()
        .unwrap()
        .clone();
        let added = merge_config(&mut target, &from);
        assert_eq!(
            added,
            vec![
                "projects[/Users/example/src/app].hasTrustDialogAccepted",
                "projects[/Users/example/src/app].allowedTools",
                "projects[/Users/example/src/lib].hasTrustDialogAccepted",
                "projects[/Users/example/src/mcp].mcpServers.local",
                "mcpServers.new"
            ]
        );
        assert_eq!(
            target["projects"]["/Users/example/src/mcp"]["mcpServers"]["local"]["command"],
            "l"
        );
        // A `false` is Claude Code's default, not a decision: the floor's
        // accepted trust replaces it.
        assert_eq!(
            target["projects"]["/Users/example/src/app"]["hasTrustDialogAccepted"],
            true
        );
        assert!(
            target["projects"]["/Users/example/src/app"]
                .get("history")
                .is_none()
        );
        assert!(target["projects"].get("/Users/example/src/none").is_none());
        assert_eq!(target["mcpServers"]["kept"]["command"], "a");
        assert!(target.get("oauthAccount").is_none());
        assert_eq!(target["numStartups"], 3);
        assert!(merge_config(&mut target, &from).is_empty(), "idempotent");
    }

    /// Claude Code 2.1.283 saves a project entry whole from its default
    /// (`nne`), so an untrusted run under the default dir leaves explicit
    /// `false` and `[]` values. Step 5 upgrades those to the floor's real
    /// values, keeps a target value that is a real choice, and never lets a
    /// floor default overwrite anything.
    #[test]
    fn merge_replaces_claude_code_defaults_with_the_floor_values() {
        let nne = json!({
            "allowedTools": [],
            "mcpContextUris": [],
            "mcpServers": {},
            "enabledMcpjsonServers": [],
            "disabledMcpjsonServers": [],
            "hasTrustDialogAccepted": false,
            "hasClaudeMdExternalIncludesApproved": false,
            "hasClaudeMdExternalIncludesWarningShown": false
        });
        let mut target = json!({
            "projects": {
                "/Users/example/src/app": nne.clone(),
                "/Users/example/src/own": {"allowedTools": ["mine"], "hasTrustDialogAccepted": true},
                "/Users/example/src/same": nne
            }
        })
        .as_object()
        .unwrap()
        .clone();
        let from = json!({
            "projects": {
                "/Users/example/src/app": {
                    "hasTrustDialogAccepted": true,
                    "hasClaudeMdExternalIncludesApproved": true,
                    "allowedTools": ["Bash(ls)"],
                    "enabledMcpjsonServers": ["db"],
                    "disabledMcpjsonServers": [],
                    "projectOnboardingSeenCount": 2
                },
                "/Users/example/src/own": {"allowedTools": ["floor"], "hasTrustDialogAccepted": false},
                "/Users/example/src/same": {"hasTrustDialogAccepted": false, "allowedTools": []}
            }
        })
        .as_object()
        .unwrap()
        .clone();
        let added = merge_config(&mut target, &from);
        assert_eq!(
            added,
            vec![
                "projects[/Users/example/src/app].hasTrustDialogAccepted",
                "projects[/Users/example/src/app].projectOnboardingSeenCount",
                "projects[/Users/example/src/app].hasClaudeMdExternalIncludesApproved",
                "projects[/Users/example/src/app].enabledMcpjsonServers",
                "projects[/Users/example/src/app].allowedTools",
            ]
        );
        let app = &target["projects"]["/Users/example/src/app"];
        assert_eq!(app["hasTrustDialogAccepted"], true);
        assert_eq!(app["hasClaudeMdExternalIncludesApproved"], true);
        assert_eq!(app["allowedTools"], json!(["Bash(ls)"]));
        assert_eq!(app["enabledMcpjsonServers"], json!(["db"]));
        assert_eq!(app["disabledMcpjsonServers"], json!([]));
        assert_eq!(app["hasClaudeMdExternalIncludesWarningShown"], false);
        // A real target value is the user's choice and stays.
        let own = &target["projects"]["/Users/example/src/own"];
        assert_eq!(own["allowedTools"], json!(["mine"]));
        assert_eq!(own["hasTrustDialogAccepted"], true);
        // Default over default changes nothing.
        assert_eq!(
            target["projects"]["/Users/example/src/same"]["hasTrustDialogAccepted"],
            false
        );
        assert!(merge_config(&mut target, &from).is_empty(), "idempotent");
        for v in [json!(null), json!(false), json!(0), json!([]), json!({})] {
            assert!(is_cc_project_default(&v), "{v}");
        }
        for v in [
            json!(true),
            json!(1),
            json!(["x"]),
            json!({"a": 1}),
            json!(""),
        ] {
            assert!(!is_cc_project_default(&v), "{v}");
        }
    }

    #[test]
    fn shared_action_table() {
        use SharedKind as S;
        for kind in [S::Dir, S::File] {
            assert_eq!(
                shared_action(&LocalKind::LinkToShared, kind),
                SharedAction::Unlink
            );
            assert_eq!(shared_action(&LocalKind::Absent, kind), SharedAction::Move);
            assert!(matches!(
                shared_action(&LocalKind::OtherLink, kind),
                SharedAction::Skip(_)
            ));
            assert!(matches!(
                shared_action(&LocalKind::Other, kind),
                SharedAction::Skip(_)
            ));
        }
        assert_eq!(
            shared_action(&LocalKind::RealDir, S::Dir),
            SharedAction::Drain
        );
        assert_eq!(
            shared_action(&LocalKind::RealFile, S::File),
            SharedAction::Append
        );
        assert!(matches!(
            shared_action(&LocalKind::RealDir, S::File),
            SharedAction::Skip(_)
        ));
        assert!(matches!(
            shared_action(&LocalKind::RealFile, S::Dir),
            SharedAction::Skip(_)
        ));
        for local in [
            LocalKind::RealDir,
            LocalKind::RealFile,
            LocalKind::Absent,
            LocalKind::OtherLink,
        ] {
            assert_eq!(shared_action(&local, S::Absent), SharedAction::Nothing);
        }
        assert!(matches!(
            shared_action(&LocalKind::LinkToShared, S::Absent),
            SharedAction::Skip(_)
        ));
        assert!(matches!(
            shared_action(&LocalKind::Absent, S::Other),
            SharedAction::Skip(_)
        ));
    }

    #[test]
    fn appended_history_puts_the_shared_lines_first_once() {
        assert_eq!(
            appended(b"{\"a\":1}\n", b"{\"b\":2}\n").unwrap(),
            b"{\"a\":1}\n{\"b\":2}\n"
        );
        // A missing final newline does not glue two lines together.
        assert_eq!(appended(b"x", b"y\n").unwrap(), b"x\ny\n");
        // An empty shared history adds nothing: the local file stays.
        assert_eq!(appended(b"", b"y\n"), None);
        // Already appended (a rerun): nothing to write.
        assert_eq!(appended(b"x\n", b"x\ny\n"), None);
    }

    /// The plugin registries record absolute paths through the profile
    /// dir's `plugins` link. After step 6 moved the dir, they point at
    /// `~/.claude/plugins`, but only where that path exists.
    #[test]
    fn plugin_paths_move_to_the_new_plugins_dir() {
        let home = Path::new("/Users/example");
        let dirs = vec![home.join(".claude.work"), home.join(".claude.home")];
        let prefixes = old_plugin_prefixes(home, &dirs);
        let mut v = json!({
            "version": 2,
            "plugins": {
                "slack@official": [{
                    "scope": "user",
                    "installPath": "/Users/example/.claude.work/plugins/cache/official/slack/1.0",
                    "version": "1.0"
                }],
                "lint@other": [{
                    "installPath": "/Users/example/.claude.home/plugins/cache/other/lint/2.0"
                }],
                "gone@other": [{
                    "installPath": "/Users/example/.claude.shared/plugins/cache/other/gone/1.0"
                }],
                "elsewhere@x": [{"installPath": "/opt/plugins/x"}]
            }
        });
        let r = rewrite_plugin_paths(&mut v, &prefixes, "/Users/example/.claude/plugins", &|p| {
            !p.contains("/gone/")
        });
        assert_eq!(r.rewritten.len(), 2, "{r:?}");
        assert_eq!(
            r.dangling,
            vec!["/Users/example/.claude.shared/plugins/cache/other/gone/1.0"]
        );
        assert_eq!(
            v["plugins"]["slack@official"][0]["installPath"],
            "/Users/example/.claude/plugins/cache/official/slack/1.0"
        );
        assert_eq!(
            v["plugins"]["lint@other"][0]["installPath"],
            "/Users/example/.claude/plugins/cache/other/lint/2.0"
        );
        assert_eq!(
            v["plugins"]["elsewhere@x"][0]["installPath"],
            "/opt/plugins/x"
        );
        // Key order and other fields are kept.
        assert_eq!(
            serde_json::to_string(&v["plugins"]["slack@official"][0]).unwrap(),
            r#"{"scope":"user","installPath":"/Users/example/.claude/plugins/cache/official/slack/1.0","version":"1.0"}"#
        );
        let mut m = json!({"official": {"installLocation": "/Users/example/.claude.work/plugins/marketplaces/official"}});
        let r = rewrite_plugin_paths(&mut m, &prefixes, "/Users/example/.claude/plugins", &|_| {
            true
        });
        assert_eq!(r.rewritten.len(), 1);
        assert_eq!(
            m["official"]["installLocation"],
            "/Users/example/.claude/plugins/marketplaces/official"
        );
    }

    #[cfg(unix)]
    #[test]
    fn step_6_carries_history_and_linked_dirs_and_rewrites_plugin_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let d = home.join(".claude");
        let sh = shared_root(home);
        let work = home.join(".claude.work");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        for n in [
            "todos",
            "shell-snapshots",
            "plugins/cache/official/slack/1.0",
        ] {
            std::fs::create_dir_all(sh.join(n)).unwrap();
        }
        std::fs::write(sh.join("todos").join("t.json"), b"[]").unwrap();
        std::fs::write(sh.join("history.jsonl"), b"{\"old\":1}\n").unwrap();
        std::fs::write(d.join("history.jsonl"), b"{\"new\":2}\n").unwrap();
        std::os::unix::fs::symlink(sh.join("todos"), d.join("todos")).unwrap();
        let old = work.join("plugins/cache/official/slack/1.0");
        std::fs::write(
            sh.join("plugins").join("installed_plugins.json"),
            serde_json::to_vec_pretty(&json!({"version": 2, "plugins": {"slack@official": [
                {"installPath": old.to_string_lossy()}
            ]}}))
            .unwrap(),
        )
        .unwrap();
        // The profile's own content stays behind and is listed.
        std::fs::create_dir_all(work.join("skills")).unwrap();
        std::fs::create_dir_all(work.join("file-history")).unwrap();
        std::os::unix::fs::symlink(sh.join("todos"), work.join("todos")).unwrap();
        std::fs::write(work.join("settings.json"), b"{}").unwrap();
        std::fs::create_dir_all(work.join("hooks")).unwrap();
        // A real projects dir: its transcripts are not merged, so it is
        // reported too.
        std::fs::create_dir_all(work.join("projects")).unwrap();
        assert_eq!(
            left_behind(home, &work),
            vec![
                "settings.json",
                "hooks",
                "skills",
                "file-history",
                "projects (its own, not the shared one: not merged)"
            ]
        );

        let plan = shared_plan(home);
        let get = |n: &str| plan.iter().find(|(k, _)| *k == n).unwrap().1.clone();
        assert_eq!(get("todos"), SharedAction::Unlink);
        assert_eq!(get("shell-snapshots"), SharedAction::Move);
        assert_eq!(get("history.jsonl"), SharedAction::Append);
        assert_eq!(get("session-env"), SharedAction::Nothing);
        assert_eq!(plugin_paths_preview(home, std::slice::from_ref(&work)), 1);

        apply_shared(home).unwrap();
        assert_eq!(
            std::fs::read(d.join("history.jsonl")).unwrap(),
            b"{\"old\":1}\n{\"new\":2}\n"
        );
        assert!(!sh.join("history.jsonl").exists());
        assert!(d.join("shell-snapshots").is_dir());
        let tm = std::fs::symlink_metadata(d.join("todos")).unwrap();
        assert!(tm.is_dir() && !tm.file_type().is_symlink());

        let state = home.join("state");
        let lines = apply_plugin_paths(home, std::slice::from_ref(&work), &state).unwrap();
        assert_eq!(lines.len(), 1, "{lines:?}");
        let v: Value = serde_json::from_slice(
            &std::fs::read(d.join("plugins/installed_plugins.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            v["plugins"]["slack@official"][0]["installPath"],
            json!(d.join("plugins/cache/official/slack/1.0").to_string_lossy())
        );
        assert_eq!(std::fs::read_dir(state.join("migrate")).unwrap().count(), 1);
        // A rerun changes nothing.
        assert!(
            apply_plugin_paths(home, std::slice::from_ref(&work), &state)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_profile_link_names_the_shared_entry_by_text() {
        let home = Path::new("/Users/example");
        let work = home.join(".claude.work");
        let shared = home.join(".claude.shared").join("projects");
        assert!(names_shared(&work, &shared, &shared));
        assert!(names_shared(
            &work,
            Path::new("../.claude.shared/projects"),
            &shared
        ));
        assert!(names_shared(
            &work,
            Path::new("/Users/example/./.claude.shared/projects"),
            &shared
        ));
        assert!(!names_shared(
            &work,
            Path::new("/Users/example/.claude/projects"),
            &shared
        ));
        assert!(!names_shared(&work, Path::new("projects"), &shared));
    }

    /// Step 6 moves the shared entries into `~/.claude`; the legacy profile
    /// dirs' own links then point there, not at the moved-away shared path,
    /// so a claude started with a stale `CLAUDE_CONFIG_DIR` writes into the
    /// real dirs and registers where retire's gate looks.
    #[cfg(unix)]
    #[test]
    fn step_6_repoints_the_profile_dirs_links() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let sh = home.join(".claude.shared");
        let d = home.join(".claude");
        std::fs::create_dir_all(sh.join("projects")).unwrap();
        std::fs::create_dir_all(sh.join("sessions")).unwrap();
        std::fs::write(sh.join("history.jsonl"), "{\"a\":1}\n").unwrap();
        let work = home.join(".claude.work");
        let other = home.join(".claude.home");
        for p in [&work, &other] {
            std::fs::create_dir_all(p).unwrap();
            symlink(sh.join("sessions"), p.join("sessions")).unwrap();
            symlink(sh.join("history.jsonl"), p.join("history.jsonl")).unwrap();
        }
        symlink(sh.join("projects"), work.join("projects")).unwrap();
        // A relative spelling, and links step 6 has nothing to put behind.
        symlink("../.claude.shared/projects", other.join("projects")).unwrap();
        symlink(sh.join("todos"), work.join("todos")).unwrap();
        symlink("/opt/elsewhere/plugins", work.join("plugins")).unwrap();

        apply_shared(home).unwrap();
        let dirs = vec![work.clone(), other.clone()];
        let lines = repoint_profile_links(home, &dirs).unwrap();
        assert_eq!(lines.len(), 6, "{lines:?}");
        for p in &dirs {
            for n in ["projects", "sessions", "history.jsonl"] {
                assert_eq!(
                    std::fs::read_link(p.join(n)).unwrap(),
                    d.join(n),
                    "{p:?} {n}"
                );
                assert!(p.join(n).exists(), "{p:?} {n} resolves");
            }
        }
        // Left as they were: no real entry to point at, or not a shared link.
        assert_eq!(
            std::fs::read_link(work.join("todos")).unwrap(),
            sh.join("todos")
        );
        assert_eq!(
            std::fs::read_link(work.join("plugins")).unwrap(),
            Path::new("/opt/elsewhere/plugins")
        );
        // A stale-dir claude's writes land in ~/.claude.
        std::fs::create_dir(work.join("projects").join("-slug")).unwrap();
        assert!(d.join("projects").join("-slug").is_dir());
        std::fs::write(other.join("sessions").join("123.json"), "{}").unwrap();
        assert!(d.join("sessions").join("123.json").is_file());
        assert!(!sh.join("history.jsonl").exists());
        // No temp link left behind, and a rerun changes nothing.
        for p in &dirs {
            assert!(std::fs::read_dir(p).unwrap().all(|e| {
                !e.unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains("csm-relink")
            }));
        }
        assert!(repoint_profile_links(home, &dirs).unwrap().is_empty());
    }

    #[test]
    fn write_gate_refusals() {
        let home = Path::new("/Users/example");
        assert!(write_gate(true, &[], None, None, home, true).is_err());
        assert!(write_gate(false, &[home.join(".claude.work")], None, None, home, true).is_err());
        assert!(
            write_gate(
                false,
                &[],
                Some("/Users/example/.claude.work"),
                None,
                home,
                true
            )
            .is_err()
        );
        assert!(write_gate(false, &[], None, None, home, false).is_err());
        assert!(write_gate(false, &[], Some("  "), None, home, true).is_ok());
        assert!(write_gate(false, &[], None, None, home, true).is_ok());
    }

    /// The login session's floor refuses even when this shell has no
    /// `CLAUDE_CONFIG_DIR` (an ssh session, a manual `unset`): an Orca
    /// started from the Dock would inherit it as `D` while step 6 moves the
    /// shared dirs its links point at.
    #[test]
    fn write_gate_refuses_a_session_floor_this_shell_does_not_carry() {
        let home = Path::new("/Users/example");
        let e = write_gate(
            false,
            &[],
            None,
            Some("/Users/example/.claude.work\n"),
            home,
            true,
        )
        .unwrap_err();
        assert!(e.contains("login session"), "{e}");
        assert!(e.contains(".claude.work"), "{e}");
        assert!(write_gate(false, &[], None, Some(" "), home, true).is_ok());
        // The process value is reported first.
        let e = write_gate(
            false,
            &[],
            Some("/Users/example/.claude.home"),
            Some("/Users/example/.claude.work"),
            home,
            true,
        )
        .unwrap_err();
        assert!(e.contains(".claude.home"), "{e}");
    }

    /// `CLAUDE_CONFIG_DIR=~/.claude` (what a leftover `csm cas` shim
    /// exports once the floor is gone) still refuses: with the variable set,
    /// step 7's switch and Orca use `~/.claude/.claude.json` while step 5
    /// merges into `~/.claude.json`, and the files would stay split. The
    /// same holds for the login session's value, which Orca inherits.
    #[test]
    fn write_gate_refuses_config_dir_set_to_the_default_dir() {
        let home = Path::new("/Users/example");
        for v in ["/Users/example/.claude", "/Users/example/.claude/"] {
            let e = write_gate(false, &[], Some(v), None, home, true).unwrap_err();
            assert!(e.contains("~/.claude/.claude.json"), "{e}");
            assert!(e.contains("shim"), "{e}");
            let e = write_gate(false, &[], None, Some(v), home, true).unwrap_err();
            assert!(e.contains("login session"), "{e}");
            assert!(e.contains("~/.claude/.claude.json"), "{e}");
        }
    }

    #[test]
    fn session_floor_is_inert_under_test() {
        assert_eq!(session_floor().unwrap(), None);
    }

    /// A SQLite-backed Orca profile used to deadlock `migrate import`: the
    /// write gate needs Orca stopped and the SQLite gate needed it running.
    /// Now only the store-patching steps (the import, the floor switch) are
    /// deferred to a running Orca; read-backs still run offline.
    #[test]
    fn a_sqlite_profile_defers_only_the_store_writes() {
        let fresher = Status::InOrca {
            id: "acct-a".into(),
            fresher: Some(true),
        };
        let current = Status::InOrca {
            id: "acct-a".into(),
            fresher: Some(false),
        };
        assert_eq!(row_action(&Status::ToImport, false), RowAction::Import);
        assert_eq!(row_action(&Status::ToImport, true), RowAction::Defer);
        for sqlite in [false, true] {
            assert_eq!(
                row_action(&fresher, sqlite),
                RowAction::ReadBack("acct-a".into())
            );
            assert_eq!(row_action(&current, sqlite), RowAction::Skip);
            assert_eq!(row_action(&Status::NoCredentials, sqlite), RowAction::Skip);
        }
        let line = deferred_import_line(Path::new("/Users/example/.claude.home"));
        assert!(
            line.contains("csm accounts import /Users/example/.claude.home"),
            "{line}"
        );

        assert_eq!(
            floor_action("work", &current, false),
            FloorAction::Switch("acct-a".into())
        );
        match floor_action("work", &current, true) {
            FloorAction::Instruct(l) => assert!(l.contains("csm accounts use acct-a"), "{l}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            floor_action("work", &Status::ToImport, true),
            FloorAction::NoAccount
        );
    }

    /// Round 8: an agent that runs `launchctl setenv CLAUDE_CONFIG_DIR` at
    /// every login holds the migration back, whether the plist or the
    /// script it runs says it; an agent that only sets the variable for
    /// its own job does not.
    #[test]
    fn a_launch_agent_that_sets_the_floor_again_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let agents = home.join("Library").join("LaunchAgents");
        std::fs::create_dir_all(&agents).unwrap();
        assert_eq!(floor_agents(home), Vec::<PathBuf>::new());
        let script = home.join("bin-setenv");
        std::fs::write(
            &script,
            "#!/bin/sh\nlaunchctl setenv CLAUDE_CONFIG_DIR \"$HOME/.claude.work\"\n",
        )
        .unwrap();
        let plist = |args: &[&str]| {
            let a: String = args
                .iter()
                .map(|x| format!("<string>{x}</string>"))
                .collect();
            format!(
                "<?xml version=\"1.0\"?><plist><dict><key>Label</key><string>com.example.a</string>\
                 <key>ProgramArguments</key><array>{a}</array><key>RunAtLoad</key><true/></dict></plist>"
            )
        };
        std::fs::write(
            agents.join("a.plist"),
            plist(&["/bin/sh", script.to_str().unwrap()]),
        )
        .unwrap();
        std::fs::write(
            agents.join("b.plist"),
            plist(&[
                "/bin/launchctl",
                "setenv",
                "CLAUDE_CONFIG_DIR",
                "/Users/example/.claude.work",
            ]),
        )
        .unwrap();
        std::fs::write(
            agents.join("c.plist"),
            "<plist><dict><key>EnvironmentVariables</key><dict><key>CLAUDE_CONFIG_DIR</key>\
             <string>/Users/example/.claude</string></dict><key>ProgramArguments</key><array>\
             <string>/usr/bin/true</string></array></dict></plist>",
        )
        .unwrap();
        std::fs::write(
            agents.join("d.txt"),
            plist(&["/bin/launchctl", "setenv", "CLAUDE_CONFIG_DIR"]),
        )
        .unwrap();
        let found = floor_agents(home);
        assert_eq!(found, vec![agents.join("a.plist"), agents.join("b.plist")]);
        let e = agent_gate(&found).unwrap_err();
        assert!(e.contains("a.plist") && e.contains("LaunchAgent"), "{e}");
        assert!(agent_gate(&[]).is_ok());
    }

    /// Round 8: an existing small ~/.claude.json gains the floor's
    /// onboarding keys it lacks (never `oauthAccount`, never a key it has),
    /// so claude does not rerun its first launch in Orca panes.
    #[test]
    fn step5_carries_onboarding_state_into_an_existing_file() {
        let from = json!({
            "oauthAccount": {"emailAddress": "alice@example.com"},
            "hasCompletedOnboarding": true,
            "lastOnboardingVersion": "2.1.0",
            "theme": "dark",
            "numStartups": 40,
        });
        let target = json!({"oauthAccount": {"emailAddress": "bob@example.com"}, "theme": "light"});
        let c = carry_config(
            Some(target.as_object().unwrap().clone()),
            from.as_object().unwrap(),
        );
        assert_eq!(c.seeded, None);
        assert_eq!(
            c.added,
            vec!["hasCompletedOnboarding", "lastOnboardingVersion"]
        );
        assert_eq!(c.map["hasCompletedOnboarding"], json!(true));
        assert_eq!(c.map["theme"], json!("light"));
        assert_eq!(
            c.map["oauthAccount"]["emailAddress"],
            json!("bob@example.com")
        );
        assert!(!c.map.contains_key("numStartups"));
        // Retire's presence check sees them too.
        assert!(
            merge_left(
                from.as_object(),
                Ok(Some(target.as_object().unwrap().clone()))
            )
            .is_some()
        );
        assert!(merge_left(from.as_object(), Ok(Some(c.map.clone()))).is_none());
    }

    /// Round 8: retire refuses until the last import completed step 7, so
    /// a failed step-4 row (import stops before step 7) cannot lead to an
    /// Orca start that reads ~/.claude's grants back unattributed. A new
    /// import forgets the old record before it runs.
    #[test]
    fn retire_waits_for_the_last_imports_step7() {
        let e = retire_precondition(&ImportLeft {
            step7: Some("step 7 left".into()),
            ..ImportLeft::default()
        })
        .unwrap_err();
        assert!(
            e.contains("step 7 left") && e.contains("do not start Orca"),
            "{e}"
        );
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let state = home.join("state");
        let legacy = Legacy {
            profiles: Vec::new(),
            floor: None,
        };
        let left = import_left(home, &state, &legacy);
        assert!(
            left.step7.as_deref().is_some_and(|m| m.contains("step 7")),
            "{left:?}"
        );
        assert!(retire_precondition(&left).is_err());
        record_step7(&state, "attributed").unwrap();
        assert!(import_left(home, &state, &legacy).step7.is_none());
        assert!(retire_precondition(&import_left(home, &state, &legacy)).is_ok());
        clear_step7(&state).unwrap();
        clear_step7(&state).unwrap();
        assert!(import_left(home, &state, &legacy).step7.is_some());
    }

    /// Round 8: the dry run says what step 7 will really do: the switch
    /// only for a floor account Orca has on a JSON store, the attribution
    /// otherwise (also with no floor profile).
    #[test]
    fn step7_dry_run_matches_the_real_step() {
        let in_orca = Status::InOrca {
            id: "acct-a".into(),
            fresher: None,
        };
        let l = step7_dry_run(Some("work"), Some(&in_orca), false);
        assert_eq!(
            l,
            vec!["switch to the floor profile work's account (acct-a)"]
        );
        let l = step7_dry_run(Some("work"), Some(&in_orca), true);
        assert_eq!(l.len(), 2);
        assert!(
            l[0].contains("SQLite") && l[1].contains("attribute"),
            "{l:?}"
        );
        for st in [Some(&Status::NoCredentials), Some(&Status::ToImport), None] {
            let l = step7_dry_run(Some("work"), st, false);
            assert!(
                l[0].contains("has no Orca account") && l[1].contains("attribute"),
                "{l:?}"
            );
            assert!(!l.iter().any(|x| x.starts_with("switch")), "{l:?}");
        }
        let l = step7_dry_run(None, None, false);
        assert_eq!(l.len(), 1);
        assert!(
            l[0].contains("attribute") && l[0].contains("no active account"),
            "{l:?}"
        );
    }

    /// Step 7 without an offline switch: the attribution's failure fails
    /// the import (Orca must not start first), its result is one line.
    #[test]
    fn attribution_line_fails_the_import_on_error() {
        use crate::orca::readback::ReadBackReport;
        let e = attribution_line(Err("no network".into())).unwrap_err();
        assert!(
            e.contains("before starting Orca") && e.contains("no network"),
            "{e}"
        );
        assert_eq!(
            attribution_line(Ok(switch::Attribution::NoActiveAccount)).unwrap(),
            None
        );
        let l = attribution_line(Ok(switch::Attribution::Done {
            readback: ReadBackReport {
                candidates: 2,
                persisted: Some("acct-a".into()),
                quarantined: vec![("fp".into(), Reason::ProfileMismatch)],
            },
            identity_cleared: true,
        }))
        .unwrap()
        .unwrap();
        assert!(l.contains("stash acct-a"), "{l}");
        assert!(l.contains("1 grant(s) quarantined"), "{l}");
        assert!(l.contains("oauthAccount was removed"), "{l}");
    }

    #[test]
    fn retire_verdicts() {
        use DirGrant::{Newer, Superseded, Unreadable};
        use StashCheck::{Rejected, Unverified, Verified};
        let home = Path::new("/Users/example");
        let dir = home.join(".claude.work");
        let in_orca = Status::InOrca {
            id: "acct-a".into(),
            fresher: Some(false),
        };
        let v = |st: &Status, c, g, d: &Path, e| retire_verdict(st, None, c, g, d, home, e);
        assert_eq!(
            v(&in_orca, Verified, Superseded, &dir, true).unwrap(),
            "acct-a"
        );
        assert!(v(&in_orca, Unverified, Superseded, &dir, true).is_err());
        assert!(v(&in_orca, Verified, Superseded, &dir, false).is_err());
        assert!(v(&in_orca, Verified, Superseded, &home.join(".claude"), true).is_err());
        assert!(v(&Status::ToImport, Verified, Superseded, &dir, true).is_err());
        assert!(v(&Status::NoCredentials, Verified, Superseded, &dir, true).is_err());
        // No grant left but the account is Orca's (a retire that died
        // between the deletes and the rename): rename only. Still never the
        // default dir, and never a dir that is gone.
        let known = |d: &Path, e| {
            retire_verdict(
                &Status::NoCredentials,
                Some("acct-a"),
                Unverified,
                Superseded,
                d,
                home,
                e,
            )
        };
        assert_eq!(known(&dir, true).unwrap(), "acct-a");
        assert!(known(&dir, false).is_err());
        assert!(known(&home.join(".claude"), true).is_err());
        // Known identity does not lift the ToImport refusal.
        assert!(
            retire_verdict(
                &Status::ToImport,
                Some("acct-a"),
                Verified,
                Superseded,
                &dir,
                home,
                true
            )
            .is_err()
        );
        // A dir grant newer than the stash, or one that could not be read,
        // blocks retire even with a verified stash.
        let e = v(&in_orca, Verified, Newer, &dir, true).unwrap_err();
        assert!(e.contains("csm migrate import"), "{e}");
        let e = v(&in_orca, Verified, Unreadable, &dir, true).unwrap_err();
        assert!(e.contains("nothing moved"), "{e}");
        // A rejected (expired) stash token names a step that refreshes it,
        // never the import that has nothing to do for this row.
        let e = v(&in_orca, Rejected, Superseded, &dir, true).unwrap_err();
        assert!(e.contains("csm usage --refresh"), "{e}");
        assert!(!e.contains("migrate import"), "{e}");
        let e = v(&in_orca, Unverified, Superseded, &dir, true).unwrap_err();
        assert!(!e.contains("migrate import"), "{e}");
    }

    /// A dir grant counts as superseded when the stash holds it or a newer
    /// one, or when a read-back already filed it in the quarantine; a newer
    /// unfiled grant or an unreadable Keychain item blocks.
    #[test]
    fn dir_grant_state_compares_with_the_stash() {
        let never = |_: &str| false;
        let with = facts(Some("alice@example.com"), Some(("fp-dir", 9.0)));
        let st = |fresher| Status::InOrca {
            id: "acct-a".into(),
            fresher,
        };
        assert_eq!(
            dir_grant_state(&with, &st(Some(false)), &never),
            DirGrant::Superseded
        );
        assert_eq!(
            dir_grant_state(&with, &st(Some(true)), &never),
            DirGrant::Newer
        );
        let filed = |fp: &str| fp == "fp-dir";
        assert_eq!(
            dir_grant_state(&with, &st(Some(true)), &filed),
            DirGrant::Superseded
        );
        assert_eq!(
            dir_grant_state(&with, &st(None), &never),
            DirGrant::Unreadable
        );
        let mut locked = with.clone();
        locked.grant_sources.push(KEYCHAIN_UNREADABLE);
        assert_eq!(
            dir_grant_state(&locked, &st(Some(false)), &never),
            DirGrant::Unreadable
        );
    }

    /// A dry run renames nothing, so the dirs it would retire still hold
    /// their logins on disk: they must not count, or the dry run reports
    /// the registry kept where the real run removes it.
    #[test]
    fn still_holding_login_leaves_out_the_retired_dirs() {
        let legacy = parse_legacy(
            Some(r#"{"work":"/Users/example/.claude.work","home":"/Users/example/.claude.home"}"#),
            Some("work\n"),
            None,
        )
        .unwrap();
        let work = PathBuf::from("/Users/example/.claude.work");
        let home = PathBuf::from("/Users/example/.claude.home");
        let mut names = still_holding_login(&legacy, &[], |_| true);
        names.sort_unstable();
        assert_eq!(names, vec!["home", "work"]);
        assert_eq!(
            still_holding_login(&legacy, std::slice::from_ref(&work), |_| true),
            vec!["home"]
        );
        assert!(still_holding_login(&legacy, &[work, home.clone()], |_| true).is_empty());
        // A dir without a login never counts.
        assert!(still_holding_login(&legacy, &[], |_| false).is_empty());
        assert_eq!(
            still_holding_login(&legacy, &[], |d| d == home),
            vec!["home"]
        );
    }

    /// The floor is cleared once the floor profile's dir is gone, even while
    /// the registry stays for other profiles.
    #[test]
    fn floor_dir_retired_follows_the_floor_profile() {
        let legacy = parse_legacy(
            Some(r#"{"work":"/Users/example/.claude.work","home":"/Users/example/.claude.home"}"#),
            Some("work\n"),
            None,
        )
        .unwrap();
        let work = Path::new("/Users/example/.claude.work");
        assert!(!floor_dir_retired(&legacy, |_| true));
        assert!(floor_dir_retired(&legacy, |d| d != work));
        // Another profile's dir going does not touch the floor.
        assert!(!floor_dir_retired(&legacy, |d| d == work));
        let no_floor = parse_legacy(
            Some(r#"{"work":"/Users/example/.claude.work"}"#),
            None,
            None,
        )
        .unwrap();
        assert!(!floor_dir_retired(&no_floor, |_| false));
    }

    #[cfg(unix)]
    #[test]
    fn retire_refuses_a_link_to_the_default_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        let link = home.join(".claude.work");
        std::os::unix::fs::symlink(home.join(".claude"), &link).unwrap();
        let in_orca = Status::InOrca {
            id: "acct-a".into(),
            fresher: Some(false),
        };
        assert!(
            retire_verdict(
                &in_orca,
                None,
                StashCheck::Verified,
                DirGrant::Superseded,
                &link,
                home,
                true
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn shared_links_become_real_dirs_and_real_dirs_drain() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let d = home.join(".claude");
        let sh = shared_root(home);
        for n in ["projects", "sessions", "plugins"] {
            std::fs::create_dir_all(sh.join(n)).unwrap();
        }
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(sh.join("projects").join("t.jsonl"), b"transcript").unwrap();
        std::os::unix::fs::symlink(sh.join("projects"), d.join("projects")).unwrap();
        // sessions: a real dir with one colliding and one identical entry.
        std::fs::create_dir_all(d.join("sessions")).unwrap();
        std::fs::write(d.join("sessions").join("1.json"), b"mine").unwrap();
        std::fs::write(sh.join("sessions").join("1.json"), b"theirs").unwrap();
        std::fs::write(d.join("sessions").join("2.json"), b"same").unwrap();
        std::fs::write(sh.join("sessions").join("2.json"), b"same").unwrap();
        std::fs::write(sh.join("sessions").join("3.json"), b"new").unwrap();
        // plugins: absent locally.
        std::fs::write(sh.join("plugins").join("p"), b"plug").unwrap();

        let plan = shared_plan(home);
        assert_eq!(
            plan,
            vec![
                ("projects", SharedAction::Unlink),
                ("sessions", SharedAction::Drain),
                ("plugins", SharedAction::Move),
                ("todos", SharedAction::Nothing),
                ("session-env", SharedAction::Nothing),
                ("shell-snapshots", SharedAction::Nothing),
                ("history.jsonl", SharedAction::Nothing)
            ]
        );
        let lines = apply_shared(home).unwrap();
        assert_eq!(lines.len(), 3, "{lines:?}");
        let pm = std::fs::symlink_metadata(d.join("projects")).unwrap();
        assert!(pm.is_dir() && !pm.file_type().is_symlink());
        assert_eq!(
            std::fs::read(d.join("projects").join("t.jsonl")).unwrap(),
            b"transcript"
        );
        assert_eq!(
            std::fs::read(d.join("sessions").join("1.json")).unwrap(),
            b"mine"
        );
        assert_eq!(
            std::fs::read(d.join("sessions").join("3.json")).unwrap(),
            b"new"
        );
        assert!(
            sh.join("sessions").join("1.json").exists(),
            "collision stays"
        );
        assert!(
            !sh.join("sessions").join("2.json").exists(),
            "identical dropped"
        );
        assert_eq!(std::fs::read(d.join("plugins").join("p")).unwrap(), b"plug");
        // Rerun is a no-op.
        assert!(
            shared_plan(home)
                .iter()
                .all(|(n, a)| *n == "sessions" || *a == SharedAction::Nothing)
        );
        // The import line names what collided.
        let collided = sh.join("sessions").join("1.json");
        assert!(
            lines
                .iter()
                .any(|l| l.contains(&collided.display().to_string())),
            "{lines:?}"
        );
        // A drain that can only collide again does not hold retire back;
        // retire names what is left instead.
        assert_eq!(
            drain_scan(&sh.join("sessions"), &d.join("sessions")).unwrap(),
            DrainScan {
                moves: false,
                collided: vec![collided.clone()],
            }
        );
        let legacy = Legacy {
            profiles: Vec::new(),
            floor: None,
        };
        record_step7(&home.join("state"), "attributed").unwrap();
        let left = import_left(home, &home.join("state"), &legacy);
        assert!(left.shared.is_empty(), "{left:?}");
        assert_eq!(left.collided, vec![collided.clone()]);
        assert!(retire_precondition(&left).is_ok());
        // A new entry to carry makes it pending again.
        std::fs::write(sh.join("sessions").join("4.json"), b"late").unwrap();
        let left = import_left(home, &home.join("state"), &legacy);
        assert_eq!(left.shared, vec!["sessions"]);
        assert!(retire_precondition(&left).is_err());
        let lines = apply_shared(home).unwrap();
        assert!(lines.iter().any(|l| l.contains("collided")), "{lines:?}");
        assert!(
            import_left(home, &home.join("state"), &legacy)
                .shared
                .is_empty()
        );
    }

    /// A drain with no collision left removes the source dir, so an empty
    /// shared dir is still work to do.
    #[test]
    fn drain_scan_mirrors_drain() {
        let tmp = tempfile::tempdir().unwrap();
        let (src, dst) = (tmp.path().join("s"), tmp.path().join("d"));
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::create_dir_all(dst.join("sub")).unwrap();
        std::fs::write(src.join("sub").join("x"), b"1").unwrap();
        std::fs::write(dst.join("sub").join("x"), b"2").unwrap();
        let scan = drain_scan(&src, &dst).unwrap();
        assert!(!scan.moves);
        assert_eq!(scan.collided, vec![src.join("sub").join("x")]);
        assert_eq!(drain(&src, &dst).unwrap(), scan.collided);
        std::fs::remove_file(src.join("sub").join("x")).unwrap();
        assert!(drain_scan(&src, &dst).unwrap().moves, "empty dirs go");
        assert!(drain(&src, &dst).unwrap().is_empty());
        assert!(!src.exists());
        assert_eq!(collided_list(&[]), "");
        let many: Vec<PathBuf> = (0..12).map(|i| PathBuf::from(format!("p{i}"))).collect();
        assert!(collided_list(&many).ends_with("and 2 more"));
    }

    /// The plan over a Linux temp home: one profile in Orca with a fresher
    /// dir grant, one to import, one without credentials. No secret in the
    /// rendered text.
    #[test]
    fn plan_over_a_linux_home_without_secrets() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let env = HostEnv::for_test(home, HostOs::Linux);
        let procs = FakeProcs::default();
        let ctx = Context::from_env(env.clone(), &procs);
        let ud = ctx.user_data.dir.clone();
        let alice = serde_json::to_vec(&oauth_json("u-a", "alice@example.com", None)).unwrap();
        make_stash(
            &ud,
            "acct-a",
            Some(&alice),
            Some(creds_json("at-old", "rt-old", 1000).as_bytes()),
        );
        write_store(
            &ud,
            &[record_json(&ud, "acct-a", "alice@example.com", None)],
            Some("acct-a"),
        );

        let mk = |name: &str, email: Option<&str>, creds: Option<String>| {
            let dir = home.join(format!(".claude.{name}"));
            std::fs::create_dir_all(&dir).unwrap();
            if let Some(e) = email {
                std::fs::write(
                    dir.join(".claude.json"),
                    json!({"oauthAccount": {"emailAddress": e, "accountUuid": "u"},
                           "projects": {"/Users/example/src/app": {"hasTrustDialogAccepted": true}}})
                    .to_string(),
                )
                .unwrap();
            }
            if let Some(c) = creds {
                std::fs::write(dir.join(".credentials.json"), c).unwrap();
            }
            dir
        };
        let work = mk(
            "work",
            Some("alice@example.com"),
            Some(creds_json("at-new", "rt-new", 2000)),
        );
        let home_dir = mk(
            "home",
            Some("bob@example.com"),
            Some(creds_json("at-b", "rt-b", 5)),
        );
        let spare = mk("spare", None, None);
        let cfg = legacy_dir(home);
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(
            cfg.join("profiles.json"),
            json!({"work": work, "home": home_dir, "spare": spare}).to_string(),
        )
        .unwrap();
        std::fs::write(cfg.join("default"), "work\n").unwrap();

        let legacy = load_legacy(home).unwrap();
        let view = crate::orca::snapshot_with(&env, &SnapshotOptions::default(), &procs);
        let plan = build_plan(&ctx, &view, &legacy, Probe::Read);
        let by = |n: &str| {
            plan.rows
                .iter()
                .find(|r| r.facts.name == n)
                .unwrap()
                .status
                .clone()
        };
        assert_eq!(
            by("work"),
            Status::InOrca {
                id: "acct-a".into(),
                fresher: Some(true)
            }
        );
        assert_eq!(by("home"), Status::ToImport);
        assert_eq!(by("spare"), Status::NoCredentials);
        // No ~/.claude.json yet: the floor profile's file (minus
        // oauthAccount) seeds it, trust fields included, so nothing is left
        // to merge on top.
        assert_eq!(plan.seeded, Some(1));
        assert!(plan.merge.is_empty(), "{:?}", plan.merge);
        assert_eq!(plan.target_d, home.join(".claude"));
        let text = render_plan(&plan);
        assert!(text.contains("work [floor]"), "{text}");
        for secret in ["at-new", "rt-new", "at-old", "rt-old", "at-b", "rt-b"] {
            assert!(!text.contains(secret), "leaked {secret}: {text}");
        }
        // The gate lets a stopped, quiet machine through.
        assert!(gate(&ctx, &procs, &legacy).is_ok() || !ctx.version_ok);
    }

    /// A Linux world for retire: account `acct-a` (uuid `u-a`) with a
    /// stashed grant, and a legacy dir `~/.claude.work` holding `dir_grant`.
    fn retire_world(home: &Path, dir_grant: &str) -> (Context, OrcaView, PathBuf, String) {
        let env = HostEnv::for_test(home, HostOs::Linux);
        let procs = FakeProcs::default();
        let ctx = Context::from_env(env.clone(), &procs);
        let ud = ctx.user_data.dir.clone();
        let stash_grant = creds_json("at-a", "rt-a", 9);
        make_stash(
            &ud,
            "acct-a",
            Some(
                oauth_json("u-a", "alice@example.com", None)
                    .to_string()
                    .as_bytes(),
            ),
            Some(stash_grant.as_bytes()),
        );
        write_store(
            &ud,
            &[record_json(&ud, "acct-a", "alice@example.com", None)],
            Some("acct-a"),
        );
        let view = crate::orca::snapshot_with(&env, &SnapshotOptions::default(), &procs);
        let dir = home.join(".claude.work");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".credentials.json"), dir_grant).unwrap();
        (ctx, view, dir, stash_grant)
    }

    /// Retiring a Linux profile files its grant in the quarantine before the
    /// file goes, then renames the dir. The stash's own grant needs no
    /// profile call.
    #[test]
    fn retire_dir_quarantines_then_renames() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let grant = creds_json("at-a", "rt-a", 9);
        let (ctx, view, dir, _) = retire_world(home, &grant);
        let http = FakeHttp::default();
        let line = retire_dir(&ctx, &view, &http, &dir, "acct-a").unwrap();
        assert!(line.contains("1 grant(s)"), "{line}");
        assert!(http.profile_calls.lock().unwrap().is_empty());
        assert!(!dir.exists());
        let retired = home.join(".claude.work.retired");
        assert!(retired.is_dir() && !retired.join(".credentials.json").exists());
        let q = Quarantine::new(HostOs::Linux, &ctx.state);
        let list = q.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].reason, Reason::Retired);
        assert_eq!(list[0].matched_account.as_deref(), Some("acct-a"));
        assert_eq!(list[0].fingerprint, quarantine::fingerprint(&grant));
        assert_eq!(
            q.get(&list[0].fingerprint).unwrap().unwrap().expose(),
            grant
        );
        // A second retire refuses: the .retired name is taken.
        std::fs::create_dir_all(&dir).unwrap();
        assert!(retire_dir(&ctx, &view, &http, &dir, "acct-a").is_err());
    }

    /// Round 8: a dir copy of the stash's grant (same refresh token) that
    /// also holds MCP logins the stash lacks is filed as `extra-logins`,
    /// never as a plain retired copy `doctor --fix` would purge as
    /// superseded, and the output names the logins but no token.
    #[test]
    fn retire_dir_keeps_mcp_logins_the_stash_lacks() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let mut v: Value = serde_json::from_str(&creds_json("at-a", "rt-a", 9)).unwrap();
        v["mcpOAuth"] = json!({"srv-a": {"accessToken": "mcp-tok"}});
        let grant = v.to_string();
        let (ctx, view, dir, _) = retire_world(home, &grant);
        let http = FakeHttp::default();
        let line = retire_dir(&ctx, &view, &http, &dir, "acct-a").unwrap();
        assert!(http.profile_calls.lock().unwrap().is_empty());
        assert!(line.contains("mcpOAuth/srv-a"), "{line}");
        assert!(!line.contains("mcp-tok"), "{line}");
        let q = Quarantine::new(HostOs::Linux, &ctx.state);
        let list = q.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].reason, Reason::ExtraLogins);
        assert_eq!(list[0].matched_account.as_deref(), Some("acct-a"));
        assert_eq!(
            q.get(&list[0].fingerprint).unwrap().unwrap().expose(),
            grant
        );
    }

    #[test]
    fn extra_logins_only_for_a_kept_stash() {
        let side = |ks: &[&str]| -> BTreeMap<String, String> {
            ks.iter()
                .map(|k| ((*k).to_owned(), "d".to_owned()))
                .collect()
        };
        let g = |ks: &[&str]| GrantFacts {
            fingerprint: "fp".into(),
            expires_at: Some(1.0),
            side: side(ks),
        };
        let kept = Status::InOrca {
            id: "a".into(),
            fresher: Some(false),
        };
        let dir = g(&["mcpOAuth/x", "mcpOAuth/y"]);
        assert_eq!(
            extra_logins(&kept, Some(&dir), Some(&g(&["mcpOAuth/x"]))),
            vec!["mcpOAuth/y"]
        );
        assert_eq!(extra_logins(&kept, Some(&dir), None).len(), 2);
        assert!(extra_logins(&kept, Some(&dir), Some(&dir)).is_empty());
        // Read back (or not in Orca): nothing to report.
        let read = Status::InOrca {
            id: "a".into(),
            fresher: Some(true),
        };
        assert!(extra_logins(&read, Some(&dir), None).is_empty());
        assert!(extra_logins(&Status::ToImport, Some(&dir), None).is_empty());
        assert!(extra_logins_line("work", &[]).is_none());
        let line = extra_logins_line("work", &["mcpOAuth/y".into()]).unwrap();
        assert!(
            line.contains("mcpOAuth/y") && line.contains("extra-logins"),
            "{line}"
        );
        assert_eq!(retired_reason(&[]), Reason::Retired);
        assert_eq!(retired_reason(&["mcpOAuth/y".into()]), Reason::ExtraLogins);
    }

    /// Round 8: import and retire name what the other profiles hold that
    /// step 5 does not carry; retire points at the renamed copy.
    #[test]
    fn differs_lines_name_the_file_to_merge_from() {
        let legacy = Legacy {
            profiles: vec![LegacyProfile {
                name: "home".into(),
                dir: PathBuf::from("/Users/example/.claude.home"),
            }],
            floor: None,
        };
        let keys = vec!["projects[\"/Users/example/p\"].hasTrustDialogAccepted".to_owned()];
        let lines = differs_lines("home", &profile_config(&legacy, "home", true), &keys);
        assert_eq!(lines.len(), 2);
        let want = PathBuf::from("/Users/example/.claude.home.retired").join(".claude.json");
        assert!(lines[0].contains(&want.display().to_string()), "{lines:?}");
        assert!(lines[1].contains("hasTrustDialogAccepted"));
        let lines = differs_lines("home", &profile_config(&legacy, "home", false), &keys);
        let want = PathBuf::from("/Users/example/.claude.home").join(".claude.json");
        assert!(lines[0].contains(&want.display().to_string()), "{lines:?}");
    }

    /// A dir grant that is not the stash's goes through the profile veto:
    /// one of the stash's account is retired under it, another account's is
    /// filed unattributed (never as `acct-a`'s), and no answer moves
    /// nothing.
    #[test]
    fn retire_dir_attributes_every_grant() {
        // Another account's grant.
        let tmp = tempfile::tempdir().unwrap();
        let foreign = creds_json("at-b", "rt-b", 50);
        let (ctx, view, dir, _) = retire_world(tmp.path(), &foreign);
        let http = FakeHttp::default().profile_uuid("at-b", "u-b");
        let line = retire_dir(&ctx, &view, &http, &dir, "acct-a").unwrap();
        assert!(line.contains("1 not provably"), "{line}");
        let list = Quarantine::new(HostOs::Linux, &ctx.state).list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].reason, Reason::ProfileMismatch);
        assert_eq!(list[0].matched_account, None);
        assert!(!dir.exists());

        // An older grant of the same account: retired under it.
        let tmp = tempfile::tempdir().unwrap();
        let older = creds_json("at-a0", "rt-a0", 1);
        let (ctx, view, dir, _) = retire_world(tmp.path(), &older);
        let http = FakeHttp::default().profile_uuid("at-a0", "u-a");
        retire_dir(&ctx, &view, &http, &dir, "acct-a").unwrap();
        let list = Quarantine::new(HostOs::Linux, &ctx.state).list();
        assert_eq!(list[0].reason, Reason::Retired);
        assert_eq!(list[0].matched_account.as_deref(), Some("acct-a"));

        // No answer: nothing filed, nothing deleted, no rename.
        let tmp = tempfile::tempdir().unwrap();
        let (ctx, view, dir, _) = retire_world(tmp.path(), &foreign);
        let err = retire_dir(&ctx, &view, &FakeHttp::default(), &dir, "acct-a").unwrap_err();
        assert!(err.to_string().contains("nothing moved"), "{err}");
        assert!(dir.join(".credentials.json").is_file());
        assert!(Quarantine::new(HostOs::Linux, &ctx.state).list().is_empty());
    }

    #[test]
    fn retire_filing_maps_the_veto() {
        use crate::orca::readback::Veto;
        let never = || -> Result<Veto, crate::orca::OrcaError> { panic!("no profile call") };
        assert_eq!(retire_filing(true, never), Ok(RetireFiling::Own));
        assert_eq!(
            retire_filing(false, || Ok(Veto::Owner)),
            Ok(RetireFiling::Own)
        );
        assert_eq!(
            retire_filing(false, || Ok(Veto::Unauthorized)),
            Ok(RetireFiling::Unattributed(
                Reason::Unauthorized,
                Some((401, None))
            ))
        );
        assert_eq!(
            retire_filing(false, || Ok(Veto::Quarantine(
                Reason::ProfileMismatch,
                Some((200, Some("u-b".into())))
            ))),
            Ok(RetireFiling::Unattributed(
                Reason::ProfileMismatch,
                Some((200, Some("u-b".into())))
            ))
        );
        assert!(
            retire_filing(false, || Err(crate::orca::OrcaError::Network(
                "down".into()
            )))
            .is_err()
        );
    }

    /// A retire that filed and deleted the dir's grants but died before the
    /// rename: the rerun sees no grant, finds the account in Orca through
    /// the dir's identity, and renames.
    #[test]
    fn a_retire_that_died_before_the_rename_completes_on_a_rerun() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let (ctx, view, dir, _) = retire_world(home, "");
        std::fs::remove_file(dir.join(".credentials.json")).unwrap();
        std::fs::write(
            dir.join(".claude.json"),
            json!({"oauthAccount": oauth_json("u-a", "alice@example.com", None)}).to_string(),
        )
        .unwrap();
        let p = LegacyProfile {
            name: "work".into(),
            dir: dir.clone(),
        };
        let facts = profile_facts(&ctx, &p, Probe::Read);
        let host: Vec<AccountRecord> = view.host_accounts().cloned().collect();
        let status = classify(&facts, &host, &|_| None);
        assert_eq!(status, Status::NoCredentials);
        let known = identity_match(&facts, &host).map(|r| r.id.clone());
        let id = retire_verdict(
            &status,
            known.as_deref(),
            StashCheck::Unverified,
            dir_grant_state(&facts, &status, &|_| false),
            &dir,
            home,
            facts.exists,
        )
        .unwrap();
        assert_eq!(id, "acct-a");
        let line = retire_dir(&ctx, &view, &FakeHttp::default(), &dir, &id).unwrap();
        assert!(line.contains("0 grant(s)"), "{line}");
        assert!(home.join(".claude.work.retired").is_dir() && !dir.exists());
    }

    #[test]
    fn unset_floor_env_is_inert_under_test() {
        assert!(unset_floor_env().is_ok());
    }

    #[test]
    fn remove_legacy_files_only_touches_the_three_files() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = legacy_dir(tmp.path());
        std::fs::create_dir_all(&cfg).unwrap();
        for f in ["profiles.json", "default", "other"] {
            std::fs::write(cfg.join(f), b"x").unwrap();
        }
        let removed = remove_legacy_files(tmp.path()).unwrap();
        assert_eq!(removed.len(), 2);
        assert!(cfg.join("other").exists());
    }

    // ─── round 7: fail-closed steps and retire's references ─────────────────

    /// A floor `.claude.json` that cannot be read or is not an object is
    /// an error, never "nothing to merge"; only a missing file is.
    #[test]
    fn an_unreadable_floor_config_is_not_nothing_to_merge() {
        assert_eq!(floor_config_from(Ok(None)), Ok(None));
        let (m, b) = floor_config_from(Ok(Some(b"{\"a\":1}".to_vec())))
            .unwrap()
            .unwrap();
        assert_eq!(m["a"], 1);
        assert_eq!(b, b"{\"a\":1}");
        let bad = floor_config_from(Ok(Some(b"{bad".to_vec()))).unwrap_err();
        assert!(bad.contains("not a JSON object"), "{bad}");
        let err =
            floor_config_from(Err(io::Error::from(io::ErrorKind::PermissionDenied))).unwrap_err();
        assert!(err.contains("cannot be read"), "{err}");
    }

    /// Step 5 fails on an unparseable floor config, the plan says so, and
    /// retire's precondition refuses (even with a merge recorded for an
    /// earlier version of the file), so the dir is not renamed with its
    /// trust and MCP settings left behind unreported.
    #[test]
    fn an_unparseable_floor_config_fails_step_5_and_holds_retire_back() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let ctx = Context::from_env(
            HostEnv::for_test(home, HostOs::Linux),
            &FakeProcs::default(),
        );
        let work = home.join(".claude.work");
        std::fs::create_dir_all(&work).unwrap();
        let good = json!({"mcpServers": {"docs": {"command": "x"}}}).to_string();
        std::fs::write(work.join(".claude.json"), &good).unwrap();
        let legacy = Legacy {
            profiles: vec![LegacyProfile {
                name: "work".into(),
                dir: work.clone(),
            }],
            floor: Some("work".into()),
        };
        assert!(merge_step(&ctx, &legacy).unwrap().is_some());
        assert!(import_left(home, &ctx.state, &legacy).merge.is_none());
        // The floor config is truncated since.
        std::fs::write(work.join(".claude.json"), &good[..good.len() / 2]).unwrap();
        let e = merge_step(&ctx, &legacy).unwrap_err().to_string();
        assert!(e.contains("not a JSON object"), "{e}");
        let left = import_left(home, &ctx.state, &legacy);
        let why = left.merge.clone().expect("retire must refuse");
        assert!(why.contains(".claude.json"), "{why}");
        assert!(retire_precondition(&left).is_err());
        let plan = Plan {
            rows: Vec::new(),
            orca_running: false,
            orca_d: None,
            target_d: home.join(".claude"),
            target_config: home.join(".claude.json"),
            merge: Vec::new(),
            seeded: None,
            merge_blocked: read_floor_config(&work).err(),
            differs: Vec::new(),
            shared: Vec::new(),
            plugin_paths: 0,
            left_behind: Vec::new(),
            state_dir: ctx.state.clone(),
            smart: None,
        };
        let out = render_plan(&plan);
        assert!(out.contains("cannot merge"), "{out}");
        assert!(!out.contains("(nothing)"), "{out}");
        // No floor config at all: nothing to merge, nothing held back.
        std::fs::remove_file(work.join(".claude.json")).unwrap();
        assert!(merge_step(&ctx, &legacy).unwrap().is_none());
        assert!(import_left(home, &ctx.state, &legacy).merge.is_none());
    }

    /// A row whose Keychain could not be read fails in import instead of
    /// being skipped as "not fresher": the partial read decides nothing.
    #[test]
    fn an_unreadable_keychain_fails_the_import_row() {
        let mut f = facts(Some("alice@example.com"), Some(("fp-file", 10.0)));
        assert_eq!(unreadable_row(&f), None);
        f.grant_sources.push(KEYCHAIN_UNREADABLE);
        let why = unreadable_row(&f).unwrap();
        assert!(why.contains("Keychain"), "{why}");
        // The same row classified alone would have been a silent skip.
        let rec = [rec("id-a", "alice@example.com")];
        let st = classify(&f, &rec, &|_| {
            Some(GrantFacts {
                fingerprint: "fp-stash".into(),
                expires_at: Some(20.0),
                side: BTreeMap::new(),
            })
        });
        assert_eq!(row_action(&st, false), RowAction::Skip);
    }

    /// Steps 5 and 6 run only when the gate, taken again, passes.
    #[test]
    fn a_step_after_a_refused_regate_does_not_run() {
        let ran = std::cell::Cell::new(false);
        let r = gated(|| bail!("Orca is running"), || ran.set(true));
        assert!(r.is_err());
        assert!(!ran.get());
        assert_eq!(gated(|| Ok(()), || 7).unwrap(), 7);
    }

    /// A claude an earlier csm started counts while it runs with the
    /// recorded start; a reused pid or a dead one does not.
    #[test]
    fn a_live_child_of_an_earlier_csm_is_found_by_its_pidfile() {
        let files = [(10, 1000), (20, 1000), (30, 1000)];
        let st = |pid: u32| match pid {
            10 => None,       // gone
            20 => Some(5000), // pid reused later
            30 => Some(1002), // the supervised claude
            _ => None,
        };
        assert_eq!(legacy_supervised_child(&files, st), Some(30));
        assert_eq!(legacy_supervised_child(&files[..2], st), None);
        assert_eq!(legacy_supervised_child(&[], st), None);
    }

    /// Another csm process is found, but never this one or an ancestor.
    #[test]
    fn another_csm_process_is_found_but_not_this_one() {
        use crate::orca::testsupport::proc_info;
        let mut me = proc_info(100, "csm", Some("/usr/local/bin/csm"), &["migrate"]);
        me.ppid = Some(90);
        let mut parent = proc_info(90, "csm", Some("/usr/local/bin/csm"), &["claude"]);
        parent.ppid = Some(1);
        let shell = proc_info(80, "zsh", Some("/bin/zsh"), &[]);
        let table = vec![me.clone(), parent.clone(), shell.clone()];
        assert_eq!(other_csm(&table, 100), None);
        let sup = proc_info(200, "csm", Some("/opt/homebrew/bin/csm"), &["run"]);
        let mut table = table;
        table.push(sup);
        assert_eq!(other_csm(&table, 100), Some(200));
        let win = proc_info(300, "csm.exe", None, &["run"]);
        assert_eq!(other_csm(&[me, win], 100), Some(300));
        let claude = proc_info(400, "claude", Some("/Users/example/.local/bin/claude"), &[]);
        assert_eq!(other_csm(&[claude, shell], 100), None);
    }

    #[test]
    fn names_path_needs_a_path_boundary() {
        let n = "/Users/example/.claude.work";
        assert!(names_path(
            "bash /Users/example/.claude.work/statusline.sh",
            n
        ));
        assert!(names_path(
            "csm hook --owner '/Users/example/.claude.work'",
            n
        ));
        assert!(names_path("/Users/example/.claude.work", n));
        assert!(!names_path("/Users/example/.claude.work2/x", n));
        assert!(!names_path("/Users/example/.claude.work.retired/x", n));
        assert!(!names_path("/Users/example/.claude/x", n));
        assert!(!names_path("anything", ""));
    }

    #[test]
    fn dir_needles_cover_the_home_shorthands() {
        let home = Path::new("/Users/example");
        let n = dir_needles(
            Path::new("/Users/example/.claude.work/"),
            Some(Path::new("/Volumes/Data/example/.claude.work")),
            home,
        );
        assert_eq!(
            n,
            vec![
                "/Users/example/.claude.work",
                "~/.claude.work",
                "$HOME/.claude.work",
                "${HOME}/.claude.work",
                "/Volumes/Data/example/.claude.work",
            ]
        );
    }

    /// Retire refuses while ~/.claude's settings or an MCP server in
    /// ~/.claude.json name the dir, and a project key alone never counts.
    #[test]
    fn retire_refuses_while_settings_or_servers_name_the_dir() {
        let needles = dir_needles(
            Path::new("/Users/example/.claude.work"),
            None,
            Path::new("/Users/example"),
        );
        let settings = json!({
            "statusLine": {"type": "command", "command": "bash ~/.claude.work/statusline-command.sh"},
            "hooks": {"PreToolUse": [{"hooks": [{"command": "/Users/example/.claude.work/hooks/guard.sh"}]}]},
            "env": {"X": "/Users/example/.claude.work2"}
        })
        .to_string();
        let cfg = json!({
            "mcpServers": {"docs": {"command": "$HOME/.claude.work/mcp/docs"}},
            "projects": {
                "/Users/example/.claude.work": {"hasTrustDialogAccepted": true},
                "/Users/example/src": {"mcpServers": {"web": {"args": ["/Users/example/.claude.work/web.js"]}}}
            }
        })
        .to_string();
        let refs = dir_references(
            &[
                ("~/.claude/settings.json", settings.as_str()),
                (
                    "~/.claude/settings.local.json",
                    "not json: ~/.claude.work/x",
                ),
            ],
            Some(&cfg),
            &needles,
        );
        assert_eq!(
            refs,
            vec![
                "~/.claude/settings.json (statusLine.command)",
                "~/.claude/settings.json (hooks.PreToolUse[0].hooks[0].command)",
                "~/.claude/settings.local.json",
                "~/.claude.json (mcpServers.docs.command)",
                "~/.claude.json (projects[/Users/example/src].mcpServers.web.args[0])",
            ]
        );
        let why = references_verdict(&refs).unwrap_err();
        assert!(why.contains("statusLine.command"), "{why}");
        assert!(why.contains('…'), "{why}");
        assert_eq!(references_verdict(&[]), Ok(()));
        // Nothing names it: nothing to refuse.
        let clean = dir_references(
            &[(
                "~/.claude/settings.json",
                "{\"statusLine\":{\"command\":\"csm statusline\"}}",
            )],
            Some("{\"projects\":{\"/Users/example/.claude.work\":{}}}"),
            &needles,
        );
        assert!(clean.is_empty(), "{clean:?}");
    }

    /// The machine shell reads ~/.claude's files under the test home.
    #[test]
    fn references_to_reads_the_files_under_home() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let work = home.join(".claude.work");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        assert!(references_to(home, &work).is_empty());
        std::fs::write(
            home.join(".claude").join("settings.json"),
            json!({"statusLine": {"command": format!("bash {}/s.sh", work.display())}}).to_string(),
        )
        .unwrap();
        assert_eq!(
            references_to(home, &work),
            vec!["~/.claude/settings.json (statusLine.command)"]
        );
    }
}
