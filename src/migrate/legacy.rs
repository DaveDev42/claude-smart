//! The legacy per-profile layout, read-only: the `claude-as` registry
//! (`profiles.json`, `default`, `floor-dir`) and what each registered dir
//! holds (its `.claude.json` identity and where its grants sit).
//!
//! Moved as is from the former `csm migrate plan|import|retire` verbs;
//! [`classify`] decides where a profile stands against Orca's accounts.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::Context as _;

use crate::orca::context::Context;
use crate::orca::keychain;
use crate::orca::quarantine::{self};
use crate::orca::readback::read_freshness;
use crate::orca::record::{AccountRecord, AuthRuntime, IdentityKey, find_by_identity};
use crate::orca::runtime::{OauthIdentity, read_json_object};
use crate::orca::stash::Stash;
use crate::orca::{HostOs, OrcaView};

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

pub(crate) const LEGACY_FILES: [&str; 3] = ["profiles.json", "default", "floor-dir"];

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

pub(crate) fn read_text(p: &Path) -> io::Result<Option<String>> {
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
    pub(crate) fn of(json: &str) -> GrantFacts {
        GrantFacts {
            fingerprint: quarantine::fingerprint(json),
            expires_at: read_freshness(json),
            side: quarantine::side_state(json),
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
pub(crate) fn dir_services(ctx: &Context, dir: &Path) -> Vec<String> {
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
pub(crate) fn dir_grant_sources(ctx: &Context, dir: &Path) -> Vec<&'static str> {
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
pub(crate) type DirGrantItem = (&'static str, String, crate::orca::SecretString);

/// Every grant a legacy dir holds: its `.credentials.json` and, on macOS,
/// each Keychain spelling of the dir. Reads the secrets. The flag is set
/// when a Keychain read failed, so the list may be incomplete.
pub(crate) fn dir_grants(ctx: &Context, dir: &Path) -> (Vec<DirGrantItem>, bool) {
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

/// Read one of [`dir_grants`]'s items again, by its source and location:
/// what a retire deletes must still be what it filed.
pub(crate) fn reread_grant(
    ctx: &Context,
    source: &str,
    loc: &str,
) -> io::Result<Option<crate::orca::SecretString>> {
    if source == "file" {
        crate::orca::read_capped(Path::new(loc), 1024 * 1024)
            .map(|o| o.map(crate::orca::SecretString::new))
    } else {
        keychain::find_password(loc, &ctx.keychain_user.acct)
            .map_err(|e| io::Error::other(format!("a Keychain item of the dir ({e})")))
    }
}

/// May a retire delete the original it just filed: it still holds the
/// bytes filed (`Some(true)`), it is gone already (`Some(false)`, nothing
/// to delete), or it changed since it was read (`None`: a claude in the
/// dir rotated it, so the retire stops). Pure.
pub(crate) fn delete_verdict(filed: &str, now: Option<&str>) -> Option<bool> {
    match now {
        None => Some(false),
        Some(n) if n == filed => Some(true),
        Some(_) => None,
    }
}

pub(crate) fn profile_facts(ctx: &Context, p: &LegacyProfile, probe: Probe) -> ProfileFacts {
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

pub(crate) fn stash_path(view: &OrcaView, id: &str) -> Option<String> {
    view.store
        .as_ref()
        .and_then(|s| s.account(id))
        .and_then(|r| r.managed_auth_path.clone())
}

pub(crate) fn stash_grant(ctx: &Context, view: &OrcaView, id: &str) -> Option<GrantFacts> {
    let s = Stash::open(&ctx.user_data.dir, id, stash_path(view, id).as_deref()).ok()?;
    let c = s.credentials(ctx.os()).ok()??;
    Some(GrantFacts::of(c.expose()))
}
