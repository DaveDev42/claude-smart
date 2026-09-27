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
//! in an affected dir, or while `CLAUDE_CONFIG_DIR` still names a dir other
//! than `~/.claude`. `--dry-run` prints what would be done. Output never
//! carries a token: grants appear as quarantine fingerprints.

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
}

impl GrantFacts {
    fn of(json: &str) -> GrantFacts {
        GrantFacts {
            fingerprint: quarantine::fingerprint(json),
            expires_at: read_freshness(json),
        }
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

/// Classify a profile. Pure.
pub(crate) fn classify(
    p: &ProfileFacts,
    records: &[AccountRecord],
    stash_grant: &dyn Fn(&str) -> Option<GrantFacts>,
) -> Status {
    if p.grant.is_none() && p.grant_sources.is_empty() {
        return Status::NoCredentials;
    }
    let key = IdentityKey::new(
        p.email.as_deref(),
        p.organization_uuid.as_deref(),
        AuthRuntime::Host,
        None,
    );
    let Some(rec) = key.as_ref().and_then(|k| find_by_identity(records, k)) else {
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

/// Where a legacy dir's grants sit, without reading one: Keychain items by
/// a presence probe (no `-w`), `.credentials.json` by existence.
fn dir_grant_sources(ctx: &Context, dir: &Path) -> Vec<&'static str> {
    let mut out = Vec::new();
    if ctx.os() == HostOs::MacOs
        && dir_services(ctx, dir).iter().any(|svc| {
            matches!(
                keychain::has_password(svc, &ctx.keychain_user.acct),
                Ok(true)
            )
        })
    {
        out.push("scoped-keychain");
    }
    if dir.join(".credentials.json").is_file() {
        out.push("file");
    }
    out
}

/// Every grant a legacy dir holds: its `.credentials.json` and, on macOS,
/// each Keychain spelling of the dir. Reads the secrets.
fn dir_grants(ctx: &Context, dir: &Path) -> Vec<(&'static str, String, crate::orca::SecretString)> {
    let mut out = Vec::new();
    if ctx.os() == HostOs::MacOs {
        for svc in dir_services(ctx, dir) {
            if let Ok(Some(v)) = keychain::find_password(&svc, &ctx.keychain_user.acct) {
                out.push(("scoped-keychain", svc, v));
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
    out
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
            let grants = dir_grants(ctx, &p.dir);
            let mut sources: Vec<&'static str> = grants.iter().map(|(s, _, _)| *s).collect();
            sources.dedup();
            let best = grants
                .iter()
                .map(|(_, _, g)| GrantFacts::of(g.expose()))
                .max_by(|a, b| {
                    a.expires_at
                        .unwrap_or(0.0)
                        .total_cmp(&b.expires_at.unwrap_or(0.0))
                });
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

/// The per-project fields claude's trust prompt and project onboarding read.
pub(crate) const TRUST_FIELDS: [&str; 8] = [
    "hasTrustDialogAccepted",
    "hasCompletedProjectOnboarding",
    "projectOnboardingSeenCount",
    "hasClaudeMdExternalIncludesApproved",
    "hasClaudeMdExternalIncludesWarningShown",
    "enabledMcpjsonServers",
    "disabledMcpjsonServers",
    "allowedTools",
];

/// Merge `from`'s trust fields and user-scope MCP servers into `target`,
/// keeping every key `target` already has. Returns what was added. Pure.
pub(crate) fn merge_config(
    target: &mut Map<String, Value>,
    from: &Map<String, Value>,
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
            if fields.is_empty() {
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
                if !te.contains_key(f) {
                    te.insert(f.to_owned(), v.clone());
                    added.push(format!("projects[{path}].{f}"));
                }
            }
        }
    }
    if let Some(Value::Object(servers)) = from.get("mcpServers") {
        let tm = target
            .entry("mcpServers")
            .or_insert_with(|| Value::Object(Map::new()));
        if let Value::Object(tm) = tm {
            for (name, v) in servers {
                if !tm.contains_key(name) {
                    tm.insert(name.clone(), v.clone());
                    added.push(format!("mcpServers.{name}"));
                }
            }
        }
    }
    added
}

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

pub(crate) const SHARED_NAMES: [&str; 3] = ["projects", "sessions", "plugins"];

/// What `~/.claude/<name>` is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LocalKind {
    Absent,
    /// A symlink into `~/.claude.shared/<name>`.
    LinkToShared,
    /// A symlink elsewhere.
    OtherLink,
    RealDir,
    /// A file or anything else.
    Other,
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
    Skip(String),
}

/// Decide step 6 for one name. Pure.
pub(crate) fn shared_action(local: &LocalKind, shared_is_dir: bool) -> SharedAction {
    match (local, shared_is_dir) {
        (LocalKind::LinkToShared, true) => SharedAction::Unlink,
        (LocalKind::LinkToShared, false) => {
            SharedAction::Skip("links to a missing shared dir; remove the link by hand".into())
        }
        (LocalKind::Absent, true) => SharedAction::Move,
        (LocalKind::RealDir, true) => SharedAction::Drain,
        (LocalKind::OtherLink, _) => {
            SharedAction::Skip("is a link outside ~/.claude.shared".into())
        }
        (LocalKind::Other, _) => SharedAction::Skip("is not a dir".into()),
        (_, false) => SharedAction::Nothing,
    }
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
            (*n, shared_action(&local_kind(&l, &s), s.is_dir()))
        })
        .collect()
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

/// Move `src`'s entries into `dst`, recursing into dirs both hold. An
/// entry `dst` already has stays in `src` unless the files are identical.
/// Returns the paths left behind.
fn drain(src: &Path, dst: &Path) -> io::Result<Vec<PathBuf>> {
    let mut left = Vec::new();
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let (s, d) = (e.path(), dst.join(e.file_name()));
        let smd = std::fs::symlink_metadata(&s)?;
        match std::fs::symlink_metadata(&d) {
            Err(err) if err.kind() == io::ErrorKind::NotFound => move_path(&s, &d)?,
            Err(err) => return Err(err),
            Ok(dmd) => {
                let both_dirs = smd.is_dir()
                    && dmd.is_dir()
                    && !smd.file_type().is_symlink()
                    && !dmd.file_type().is_symlink();
                if both_dirs {
                    left.extend(drain(&s, &d)?);
                } else if smd.is_file() && dmd.is_file() && std::fs::read(&s)? == std::fs::read(&d)?
                {
                    remove_tree(&s)?;
                } else {
                    left.push(s);
                }
            }
        }
    }
    if left.is_empty() {
        fsx::guard(src)?;
        let _ = std::fs::remove_dir(src);
    }
    Ok(left)
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
            SharedAction::Drain => {
                let left = drain(&s, &l)?;
                if left.is_empty() {
                    out.push(format!("{}: drained {}", l.display(), s.display()));
                } else {
                    out.push(format!(
                        "{}: drained {}; {} entr{} collided and stayed",
                        l.display(),
                        s.display(),
                        left.len(),
                        if left.len() == 1 { "y" } else { "ies" }
                    ));
                }
            }
        }
    }
    Ok(out)
}

// ─── the write gate (pure) ────────────────────────────────────────────────────

/// Refuse a write step. Pure.
pub(crate) fn write_gate(
    orca_running: bool,
    live_in: &[PathBuf],
    claude_config_dir: Option<&str>,
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
    if let Some(d) = claude_config_dir.map(str::trim).filter(|d| !d.is_empty()) {
        let d = Path::new(d.trim_end_matches(['/', '\\']));
        if d != home.join(".claude") {
            return Err(format!(
                "CLAUDE_CONFIG_DIR is still {}; drop the floor and open a fresh shell",
                d.display()
            ));
        }
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
    write_gate(
        ctx.orca_running(procs),
        &live,
        ctx.env.claude_config_dir.as_deref(),
        home,
        ctx.version_ok,
    )
    .map_err(|e| anyhow::anyhow!("csm migrate: {e}"))?;
    Ok(())
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
    /// Keys other profiles hold that the merged file would still lack.
    pub differs: Vec<(String, Vec<String>)>,
    pub shared: Vec<(&'static str, SharedAction)>,
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
    if let Some(floor) = legacy
        .floor
        .as_ref()
        .and_then(|n| legacy.profiles.iter().find(|p| &p.name == n))
        && let Some(from) = read_json_object(&floor.dir.join(".claude.json"))
    {
        let c = carry_config(existing, &from);
        merged = c.map;
        merge = c.added;
        seeded = c.seeded;
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
        differs,
        shared: shared_plan(home),
        state_dir: ctx.state.clone(),
        smart: smart_preview(&legacy_smart_dir(home)),
    }
}

fn fmt_exp(e: Option<f64>) -> String {
    match e.and_then(|ms| chrono::DateTime::from_timestamp_millis(ms as i64)) {
        Some(t) => t.format("%Y-%m-%d %H:%M UTC").to_string(),
        None => "?".into(),
    }
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
    if let Some(n) = p.seeded {
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
            SharedAction::Skip(why) => format!("skipped: {why}"),
        };
        o.push_str(&format!("  {name}: {what}\n"));
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
    if dry_run {
        println!("csm migrate import --dry-run would:");
        for r in &plan.rows {
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
                    println!("  keep stash for {} (dir grant not fresher)", r.facts.name)
                }
                Status::NoCredentials => println!("  skip {} (no credentials)", r.facts.name),
            }
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
        if let Some(f) = &legacy.floor {
            if sqlite {
                println!(
                    "  not switch to the floor profile {f}'s account: Orca keeps its state in \
                     SQLite; run `csm accounts use` once Orca is started"
                );
            } else {
                println!("  switch to the floor profile {f}'s account");
            }
        }
        if let Err(e) = gate(&ctx, &procs, &legacy) {
            println!("and would refuse now: {e}");
        }
        return Ok(());
    }
    gate(&ctx, &procs, &legacy)?;
    let mut failed = 0usize;

    // Step 4.
    let http = SystemHttp::from_env();
    for r in &plan.rows {
        let res = match row_action(&r.status, sqlite) {
            RowAction::Import => import_one(&ctx, &procs, &r.facts.dir),
            RowAction::ReadBack(id) => read_back_one(&ctx, &procs, &view, &http, &r.facts.dir, &id),
            RowAction::Defer => Ok(deferred_import_line(&r.facts.dir)),
            RowAction::Skip => continue,
        };
        match res {
            Ok(line) => println!("{}: {line}", r.facts.name),
            Err(e) => {
                failed += 1;
                eprintln!("{}: {e:#}", r.facts.name);
            }
        }
    }

    // Step 5.
    match merge_step(&ctx, &legacy) {
        Ok(Some(line)) => println!("{line}"),
        Ok(None) => {}
        Err(e) => {
            failed += 1;
            eprintln!("config merge: {e:#}");
        }
    }

    // Step 6.
    match apply_shared(&ctx.env.home) {
        Ok(lines) => lines.iter().for_each(|l| println!("{l}")),
        Err(e) => {
            failed += 1;
            eprintln!("shared dirs: {e}");
        }
    }
    match apply_smart(&legacy_smart_dir(&ctx.env.home), &ctx.state) {
        Ok(Some(line)) => println!("{line}"),
        Ok(None) => {}
        Err(e) => {
            failed += 1;
            eprintln!("csm state: {e}");
        }
    }

    if failed > 0 {
        bail!("csm migrate import: {failed} step(s) failed; the floor switch was not run");
    }

    // Step 7.
    if let Some(floor) = &legacy.floor {
        let view = crate::orca::snapshot(&SnapshotOptions::default())?;
        let host: Vec<AccountRecord> = view.host_accounts().cloned().collect();
        let p = legacy
            .profiles
            .iter()
            .find(|p| &p.name == floor)
            .expect("floor is a profile");
        let facts = profile_facts(&ctx, p, Probe::Read);
        match floor_action(floor, &classify(&facts, &host, &|_| None), sqlite) {
            FloorAction::Instruct(line) => println!("{line}"),
            FloorAction::Switch(id) => {
                let report = ctx.with_switch_env(&procs, &http, |env| switch::switch(env, &id))?;
                match report.outcome {
                    Outcome::Switched | Outcome::AlreadyActive => {
                        println!("switched ~/.claude to {floor}'s account ({id})")
                    }
                    Outcome::Uncertain(why) => {
                        bail!(
                            "the switch to {id} did not verify ({why}); run `csm accounts doctor`"
                        )
                    }
                }
            }
            FloorAction::NoAccount => eprintln!(
                "the floor profile {floor} has no Orca account; run `csm accounts use` by hand"
            ),
        }
    }
    println!("next: `csm migrate retire`, then start Orca and run `csm orca setup`");
    Ok(())
}

/// Step 4's import: `previousLegacyCredentialsSha256` is the unscoped
/// item's digest, read under `switch.lock` inside the import so a
/// concurrent switch cannot slip in between.
fn import_one(ctx: &Context, procs: &dyn ProcFacts, dir: &Path) -> anyhow::Result<String> {
    let cli = SystemClaude::configured()?;
    let c = ctx.with_accounts_env(procs, |env| add::import_current_legacy(env, &cli, dir))?;
    Ok(format!(
        "imported {}",
        c.email.or(c.id).unwrap_or_else(|| "the account".into())
    ))
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

fn merge_step(ctx: &Context, legacy: &Legacy) -> anyhow::Result<Option<String>> {
    let Some(floor) = legacy
        .floor
        .as_ref()
        .and_then(|n| legacy.profiles.iter().find(|p| &p.name == n))
    else {
        return Ok(None);
    };
    let Some(from) = read_json_object(&floor.dir.join(".claude.json")) else {
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

/// Is `p` safe to retire? Pure over what was checked.
pub(crate) fn retire_verdict(
    status: &Status,
    stash_verified: bool,
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
        Status::InOrca { id, .. } if stash_verified => Ok(id.clone()),
        Status::InOrca { id, .. } => Err(format!(
            "stash {id} did not verify; run `csm migrate import` first"
        )),
        Status::ToImport => Err("not in Orca yet; run `csm migrate import` first".into()),
        Status::NoCredentials => Err("holds no login; remove the dir by hand if unused".into()),
    }
}

/// Does the profile endpoint put the stash's grant on the stash's account?
fn stash_verified(ctx: &Context, view: &OrcaView, http: &dyn OauthHttp, id: &str) -> bool {
    let Ok(s) = Stash::open(&ctx.user_data.dir, id, stash_path(view, id).as_deref()) else {
        return false;
    };
    let Some(uuid) = s
        .oauth_account()
        .ok()
        .flatten()
        .and_then(|v| OauthIdentity::from_value(&v).account_uuid)
    else {
        return false;
    };
    let Some(token) = s
        .credentials(ctx.os())
        .ok()
        .flatten()
        .and_then(|c| access_token(c.expose()))
    else {
        return false;
    };
    matches!(
        http.get_profile(&token).map(|r| parse_profile(&r)),
        Ok(ProfileAnswer::Ok { account_uuid: Some(u), .. }) if u == uuid
    )
}

/// Move a dir's grants into the quarantine, then rename it `<dir>.retired`.
fn retire_dir(ctx: &Context, dir: &Path, id: &str) -> anyhow::Result<String> {
    let retired = PathBuf::from(format!("{}.retired", dir.display()));
    if std::fs::symlink_metadata(&retired).is_ok() {
        bail!("{} already exists", retired.display());
    }
    let q = Quarantine::new(ctx.os(), &ctx.state);
    let now = chrono::Utc::now().timestamp_millis();
    let mut moved = 0usize;
    for (source, loc, grant) in dir_grants(ctx, dir) {
        // Filed (and on macOS read back) before the original goes.
        q.file(grant.expose(), Reason::Retired, source, Some(id), None, now)?;
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
    Ok(format!(
        "{} grant(s) quarantined; dir renamed to {}",
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

fn retire_cmd(dry_run: bool, only: &[String]) -> anyhow::Result<()> {
    let procs = SystemProcs;
    let (ctx, view, legacy) = setup()?;
    for n in only {
        if !legacy.profiles.iter().any(|p| &p.name == n) {
            bail!("csm migrate retire: no legacy profile named {n:?}");
        }
    }
    if !dry_run {
        gate(&ctx, &procs, &legacy)?;
    }
    let http = SystemHttp::from_env();
    let home = ctx.env.home.clone();
    let plan = build_plan(&ctx, &view, &legacy, Probe::Presence);
    let mut failed = 0usize;
    for r in plan
        .rows
        .iter()
        .filter(|r| only.is_empty() || only.contains(&r.facts.name))
    {
        let verified = match &r.status {
            Status::InOrca { id, .. } => stash_verified(&ctx, &view, &http, id),
            _ => false,
        };
        let name = &r.facts.name;
        match retire_verdict(&r.status, verified, &r.facts.dir, &home, r.facts.exists) {
            Err(why) => println!("{name}: skipped: {why}"),
            Ok(id) if dry_run => println!(
                "{name}: would quarantine its grants and rename {} (stash {id} verified)",
                r.facts.dir.display()
            ),
            Ok(id) => match retire_dir(&ctx, &r.facts.dir, &id) {
                Ok(line) => println!("{name}: {line}"),
                Err(e) => {
                    failed += 1;
                    eprintln!("{name}: {e:#}");
                }
            },
        }
    }
    // The registry goes once no legacy dir holds a login any more.
    let remaining: Vec<&str> = legacy
        .profiles
        .iter()
        .filter(|p| p.dir.is_dir() && !dir_grant_sources(&ctx, &p.dir).is_empty())
        .map(|p| p.name.as_str())
        .collect();
    if !remaining.is_empty() {
        println!(
            "legacy registry kept: {} still hold a login",
            remaining.join(", ")
        );
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
            }),
        }
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
        // An existing file is never seeded, only merged.
        let existing = json!({"numStartups": 1}).as_object().unwrap().clone();
        let c = carry_config(Some(existing), &from);
        assert_eq!(c.seeded, None);
        assert_eq!(c.map["numStartups"], 1);
        assert!(c.map.get("theme").is_none());
        assert_eq!(
            c.added,
            vec!["projects[/Users/example/src/app].hasTrustDialogAccepted"]
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
                "/Users/example/src/none": {"history": []}
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
                "projects[/Users/example/src/app].allowedTools",
                "projects[/Users/example/src/lib].hasTrustDialogAccepted",
                "mcpServers.new"
            ]
        );
        assert_eq!(
            target["projects"]["/Users/example/src/app"]["hasTrustDialogAccepted"],
            false
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

    #[test]
    fn shared_action_table() {
        assert_eq!(
            shared_action(&LocalKind::LinkToShared, true),
            SharedAction::Unlink
        );
        assert_eq!(shared_action(&LocalKind::Absent, true), SharedAction::Move);
        assert_eq!(
            shared_action(&LocalKind::RealDir, true),
            SharedAction::Drain
        );
        assert_eq!(
            shared_action(&LocalKind::RealDir, false),
            SharedAction::Nothing
        );
        assert_eq!(
            shared_action(&LocalKind::Absent, false),
            SharedAction::Nothing
        );
        assert!(matches!(
            shared_action(&LocalKind::OtherLink, true),
            SharedAction::Skip(_)
        ));
        assert!(matches!(
            shared_action(&LocalKind::LinkToShared, false),
            SharedAction::Skip(_)
        ));
        assert!(matches!(
            shared_action(&LocalKind::Other, true),
            SharedAction::Skip(_)
        ));
    }

    #[test]
    fn write_gate_refusals() {
        let home = Path::new("/Users/example");
        assert!(write_gate(true, &[], None, home, true).is_err());
        assert!(write_gate(false, &[home.join(".claude.work")], None, home, true).is_err());
        assert!(write_gate(false, &[], Some("/Users/example/.claude.work"), home, true).is_err());
        assert!(write_gate(false, &[], None, home, false).is_err());
        assert!(write_gate(false, &[], Some("/Users/example/.claude/"), home, true).is_ok());
        assert!(write_gate(false, &[], Some("  "), home, true).is_ok());
        assert!(write_gate(false, &[], None, home, true).is_ok());
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

    #[test]
    fn retire_verdicts() {
        let home = Path::new("/Users/example");
        let dir = home.join(".claude.work");
        let in_orca = Status::InOrca {
            id: "acct-a".into(),
            fresher: Some(false),
        };
        assert_eq!(
            retire_verdict(&in_orca, true, &dir, home, true).unwrap(),
            "acct-a"
        );
        assert!(retire_verdict(&in_orca, false, &dir, home, true).is_err());
        assert!(retire_verdict(&in_orca, true, &dir, home, false).is_err());
        assert!(retire_verdict(&in_orca, true, &home.join(".claude"), home, true).is_err());
        assert!(retire_verdict(&Status::ToImport, true, &dir, home, true).is_err());
        assert!(retire_verdict(&Status::NoCredentials, true, &dir, home, true).is_err());
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
        assert!(retire_verdict(&in_orca, true, &link, home, true).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn shared_links_become_real_dirs_and_real_dirs_drain() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let d = home.join(".claude");
        let sh = shared_root(home);
        for n in SHARED_NAMES {
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
                ("plugins", SharedAction::Move)
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

    /// Retiring a Linux profile files its grant in the quarantine before the
    /// file goes, then renames the dir.
    #[test]
    fn retire_dir_quarantines_then_renames() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let env = HostEnv::for_test(home, HostOs::Linux);
        let ctx = Context::from_env(env, &FakeProcs::default());
        let dir = home.join(".claude.work");
        std::fs::create_dir_all(&dir).unwrap();
        let grant = creds_json("at-w", "rt-w", 7);
        std::fs::write(dir.join(".credentials.json"), &grant).unwrap();
        let line = retire_dir(&ctx, &dir, "acct-a").unwrap();
        assert!(line.contains("1 grant(s)"), "{line}");
        assert!(!dir.exists());
        let retired = home.join(".claude.work.retired");
        assert!(retired.is_dir() && !retired.join(".credentials.json").exists());
        let q = Quarantine::new(HostOs::Linux, &ctx.state);
        let list = q.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].reason, Reason::Retired);
        assert_eq!(list[0].fingerprint, quarantine::fingerprint(&grant));
        assert_eq!(
            q.get(&list[0].fingerprint).unwrap().unwrap().expose(),
            grant
        );
        // A second retire refuses: the .retired name is taken.
        std::fs::create_dir_all(&dir).unwrap();
        assert!(retire_dir(&ctx, &dir, "acct-a").is_err());
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
}
