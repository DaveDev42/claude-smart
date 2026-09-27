//! Stage C (retire) and settle: a legacy dir nothing uses any more has its
//! `file-history` and `plans` drained into `~/.claude`, its grants
//! quarantined and is renamed `<dir>.retired` ([`retire_gate`],
//! [`retire_verdict`], [`retire_dir`], [`retire_stage`]); after the last
//! dir the registry goes and `~/.claude.shared` is renamed. [`settle`]
//! files a quarantined `Retired` grant that is fresher than its stash once
//! Orca is stopped.

use std::io;
use std::path::{Path, PathBuf};

use anyhow::bail;
use serde_json::Value;

use crate::orca::context::Context;
use crate::orca::http::{OauthHttp, ProfileAnswer, parse_profile};
use crate::orca::keychain;
use crate::orca::live::{DirUsers, dir_users};
use crate::orca::quarantine::{self, Quarantine, Reason};
use crate::orca::readback::{self, access_token};
use crate::orca::runtime::{OauthIdentity, read_json_object, runtime_paths};
use crate::orca::stash::Stash;
use crate::orca::{OrcaView, fsx};

use super::adopt::failed;
use super::carry::*;
use super::cutover::*;
use super::legacy::*;

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

/// How a dir [`retire_verdict`] allows is retired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RetireAs {
    /// Its grants are filed under Orca account `id` ([`retire_dir`]).
    Account(String),
    /// It holds no login and names no account Orca has: renamed with
    /// nothing to file ([`retire_dir_no_login`], which checks again that
    /// no grant appeared).
    NoLogin,
}

/// Is `p` safe to retire? Pure over what was checked. `known` is the Orca
/// account the dir's `.claude.json` names, whatever grants the dir holds:
/// a dir with no grant left whose account Orca has is renamed with nothing
/// to quarantine. That is also how a retire that filed and deleted a dir's
/// grants but died before the rename completes on a rerun. A dir that
/// holds no login at all (never logged in, or logged out) retires too
/// (design section 2, C: "its login is InOrca or it holds none"): there is
/// nothing to lose, and the rename keeps what it holds.
pub(crate) fn retire_verdict(
    status: &Status,
    known: Option<&str>,
    check: StashCheck,
    grant: DirGrant,
    dir: &Path,
    home: &Path,
    exists: bool,
) -> Result<RetireAs, String> {
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
        // The dir's newer grant is filed as `Retired` under its account
        // (retire_dir attributes it first) and settle stores it once Orca
        // is stopped: it no longer holds the dir back.
        Status::InOrca { id, .. } if grant == DirGrant::Newer => Ok(RetireAs::Account(id.clone())),
        Status::InOrca { id, .. } => match check {
            // A rejected access token is an expired one: the stash still
            // holds the same or a newer refresh token, and every grant of
            // the dir goes to the quarantine, so nothing is lost.
            StashCheck::Verified | StashCheck::Rejected => Ok(RetireAs::Account(id.clone())),
            StashCheck::Unverified => Err(format!(
                "stash {id} did not verify (no answer from the profile endpoint, or it names another account)"
            )),
        },
        Status::ToImport => Err("not in Orca yet; adopt imports it first".into()),
        Status::NoCredentials => Ok(match known {
            Some(id) => RetireAs::Account(id.to_owned()),
            None => RetireAs::NoLogin,
        }),
    }
}

/// Where settle can never carry a dir's newer grant into its stash (on
/// Windows csm writes no stash: [`settle_gate`]), the dir keeps it: filed
/// in the quarantine it would be the only live login of `id` and no tool
/// would use it. Re-authenticating the account in Orca makes the stash the
/// fresher copy, and the dir then retires. Pure.
pub(crate) fn newer_grant_gate(
    os: crate::orca::HostOs,
    grant: DirGrant,
    id: &str,
) -> Result<(), String> {
    if grant == DirGrant::Newer && os == crate::orca::HostOs::Windows {
        return Err(format!(
            "its login is fresher than Orca's copy of account {id}, and csm cannot carry it into \
             Orca on Windows; log in to that account again in Orca, then csm retires the dir"
        ));
    }
    Ok(())
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
pub(crate) fn json_mentions(v: &Value, at: &str, needles: &[String], out: &mut Vec<String>) {
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
pub(crate) fn references_to(home: &Path, dir: &Path) -> Vec<String> {
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
    /// The access token got a 401: usually just expired, since a dir
    /// retires hours or days after its claude last refreshed it. Filed as
    /// [`Reason::Retired`] under the account the dir's `.claude.json`
    /// names, with the 401 recorded, so [`settle`] refreshes it once
    /// nothing else holds its refresh token and stores the result only when
    /// the refreshed token profiles as that account. Filing it
    /// unattributed would strand the account's only live refresh token.
    Expired,
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
        Ok(readback::Veto::Unauthorized) => Ok(RetireFiling::Expired),
        Err(e) => Err(format!(
            "a grant in the dir could not be attributed ({e}); nothing moved, rerun when the profile endpoint answers"
        )),
    }
}

/// Does the profile endpoint put the stash's grant on the stash's account?
pub(crate) fn stash_check(
    ctx: &Context,
    view: &OrcaView,
    http: &dyn OauthHttp,
    id: &str,
) -> StashCheck {
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

/// What [`retire_dir`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RetiredDir {
    /// The report line.
    pub line: String,
    /// A grant it filed under the account is one [`settle`] would act on
    /// ([`settle_wanted`] against the stash): the phase stays open for the
    /// next run's settle. A copy equal to the stash, or an older one, is
    /// not.
    pub settle_due: bool,
}

/// A dir's live users, checked once more right before a retire deletes
/// anything: `Err` names who uses it now.
pub(crate) type StillFree<'a> = &'a dyn Fn() -> Result<(), String>;

/// Delete one original [`retire_dir`] filed, only while it still holds the
/// filed bytes ([`delete_verdict`]); one that changed since stops the
/// retire, and the filed copy stays in the quarantine.
fn delete_filed(ctx: &Context, source: &str, loc: &str, filed: &str) -> anyhow::Result<()> {
    let now = reread_grant(ctx, source, loc)?;
    match delete_verdict(filed, now.as_ref().map(|n| n.expose())) {
        Some(false) => Ok(()),
        Some(true) if source == "file" => {
            let p = Path::new(loc);
            fsx::guard(p)?;
            std::fs::remove_file(p)?;
            Ok(())
        }
        Some(true) => {
            keychain::delete_password(loc, &ctx.keychain_user.acct)?;
            Ok(())
        }
        None => bail!(
            "a grant of the dir changed while it was retired (a claude uses the dir); it stays \
             there, the copy filed before stays in the quarantine, and the next run retires it"
        ),
    }
}

/// Move a dir's grants into the quarantine, then rename it `<dir>.retired`.
/// Every grant is attributed first ([`retire_filing`]): one that is not
/// the account's is filed unattributed, never as a retired copy of `id`.
/// After the attribution (network calls) the dir's users are checked again
/// (`still_free`), and each original is deleted only while it still holds
/// the bytes filed ([`delete_filed`]): a claude that started in the dir
/// meanwhile keeps a grant it rotated.
pub(crate) fn retire_dir(
    ctx: &Context,
    view: &OrcaView,
    http: &dyn OauthHttp,
    dir: &Path,
    id: &str,
    still_free: StillFree<'_>,
) -> anyhow::Result<RetiredDir> {
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
    if let Err(who) = still_free() {
        bail!("the dir is in use now ({who}); nothing moved");
    }
    let mut moved = 0usize;
    let mut foreign = 0usize;
    let mut settle_due = false;
    let mut logins: Vec<String> = Vec::new();
    for ((source, loc, grant), filing) in grants.into_iter().zip(filings) {
        // Filed (and on macOS read back) before the original goes.
        match filing {
            RetireFiling::Own | RetireFiling::Expired => {
                settle_due |=
                    settle_wanted(grant.expose(), stash_creds.as_ref().map(|c| c.expose()));
                // The fingerprint compares only the Claude grant: MCP logins
                // the stash lacks make the copy more than a retired one, so
                // `doctor --fix` must not purge it as superseded.
                let extra =
                    quarantine::uncovered(&quarantine::side_state(grant.expose()), &stash_side);
                let reason = retired_reason(&extra);
                let answer = (filing == RetireFiling::Expired).then_some((401, None));
                q.file(grant.expose(), reason, source, Some(id), answer, now)?;
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
        delete_filed(ctx, source, &loc, grant.expose())?;
        moved += 1;
    }
    crate::e2e::point("migrate-retire-quarantined");
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
    Ok(RetiredDir {
        line: format!(
            "{} grant(s) quarantined{note}; dir renamed to {}",
            moved,
            retired.display()
        ),
        settle_due,
    })
}

/// Remove the legacy registry files. Returns the ones removed.
pub(crate) fn remove_legacy_files(home: &Path) -> io::Result<Vec<PathBuf>> {
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

// ─── the old shell guard ──────────────────────────────────────────────────────

/// Shell startup files, relative to the home dir, where the legacy
/// installer put its guard: `_d=$(csm cas --print-default-dir)`, falling
/// back to `~/.claude.<registry default>` when that prints nothing, then
/// `export CLAUDE_CONFIG_DIR`. Once the floor dir is renamed, that guard
/// exports a dir that no longer exists in every new shell, and plain
/// `claude` starts there logged out and empty.
pub(crate) const SHELL_STARTUP: &[&str] = &[
    ".zshenv",
    ".zprofile",
    ".zshrc",
    ".zlogin",
    ".bashrc",
    ".bash_profile",
    ".bash_login",
    ".profile",
    ".config/fish/config.fish",
    "Documents/PowerShell/Microsoft.PowerShell_profile.ps1",
    "Documents/PowerShell/profile.ps1",
    "Documents/WindowsPowerShell/Microsoft.PowerShell_profile.ps1",
    "Documents/WindowsPowerShell/profile.ps1",
];

/// Does a shell startup file's text run `csm cas --print-default-dir` on a
/// line that is not a comment? Pure.
pub(crate) fn runs_print_default_dir(text: &str) -> bool {
    text.lines().any(|l| {
        let t = l.trim_start();
        !t.starts_with('#')
            && t.contains("--print-default-dir")
            && t.split(|c: char| !(c.is_alphanumeric() || matches!(c, '-' | '_')))
                .any(|w| w == "cas")
    })
}

/// The shell startup files under `home` that still run the old guard. csm
/// only reads them (design section 7: it never edits shell startup files).
pub(crate) fn cas_shims(home: &Path) -> Vec<PathBuf> {
    const CAP: u64 = 256 * 1024;
    SHELL_STARTUP
        .iter()
        .map(|rel| home.join(rel))
        .filter(|p| {
            std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.len() <= CAP)
                && std::fs::read(p)
                    .ok()
                    .is_some_and(|b| runs_print_default_dir(&String::from_utf8_lossy(&b)))
        })
        .collect()
}

/// The pending line while `shims` is not empty. Pure.
pub(crate) fn shim_line(shims: &[PathBuf]) -> Option<String> {
    if shims.is_empty() {
        return None;
    }
    let names: Vec<String> = shims.iter().map(|p| p.display().to_string()).collect();
    Some(format!(
        "{} still runs `csm cas --print-default-dir`, whose fallback exports the floor dir in \
         every new shell; remove that guard first (the floor dir and the registry stay until then)",
        names.join(", ")
    ))
}

// ─── the retire gate (pure) ───────────────────────────────────────────────────

/// Everything [`retire_gate`] decides on for one legacy dir, beside the
/// verdict over its login ([`retire_verdict`]).
#[derive(Debug, Clone, Copy)]
pub(crate) struct RetireFacts<'a> {
    /// Who uses the dir ([`crate::orca::live::dir_users`]).
    pub users: &'a DirUsers,
    /// The floor profile's dir (I2 applies).
    pub is_floor: bool,
    /// Orca's live `D` is this dir: `Some(true)`; another, or Orca stopped:
    /// `Some(false)`; Orca runs and its `D` cannot be read: `None`.
    pub orca_d_is_dir: Option<bool>,
    /// I2's floor half ([`floor_gate`]); read for the floor dir only.
    pub floor: &'a FloorGate,
    /// A host with no Orca store: only the floor dir retires.
    pub store_less: bool,
    pub grant: DirGrant,
    /// Shell startup files that still run `csm cas --print-default-dir`
    /// ([`cas_shims`]); read for the floor dir only.
    pub shims: &'a [PathBuf],
    /// A legacy csm supervisor still runs ([`supervision`]): it may
    /// relaunch claude into any legacy dir, so every dir waits.
    pub supervisor: Option<&'a str>,
    /// Linux or Windows with an Orca store: `~/.claude` holds no login yet
    /// and the dir holds one, so csm launches with Orca stopped still run
    /// in the floor dir ([`crate::launch_context::Recorded::floor_fallback`]).
    /// Read for the floor dir only.
    pub home_empty: bool,
}

/// What [`retire_gate`] allows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RetireGate {
    /// Retire, once the verdict over its login passes.
    Go,
    /// Not now; the pending reason.
    Wait(String),
    /// Not on this host (a store-less host keeps the dir); not pending.
    Stay(String),
}

/// Stage C's gate for one dir (design section 2): no legacy csm
/// supervisor, no live user (a registered session, a claude or csm whose
/// environment names the dir; unreadable counts as live), never Orca's live `D`, for the floor dir
/// I2's floor half, and every grant readable. A dir grant newer than the
/// stash does not hold it back: [`retire_dir`] files it as `Retired` under
/// its account and [`settle`] stores it once Orca is stopped. Pure.
pub(crate) fn retire_gate(f: &RetireFacts<'_>) -> RetireGate {
    if f.store_less && !f.is_floor {
        return RetireGate::Stay(
            "no Orca store on this host: the dir stays until one exists".to_owned(),
        );
    }
    if let Some(w) = f.supervisor {
        return RetireGate::Wait(w.to_owned());
    }
    match f.users {
        DirUsers::Free => {}
        DirUsers::Live(w) => return RetireGate::Wait(format!("in use: {w}")),
        DirUsers::Unknown(w) => {
            return RetireGate::Wait(format!("{w}; counted as in use"));
        }
    }
    match f.orca_d_is_dir {
        Some(false) => {}
        Some(true) => {
            return RetireGate::Wait(
                "Orca runs in this dir; it moves to ~/.claude at Orca's next start".to_owned(),
            );
        }
        None => {
            return RetireGate::Wait(
                "Orca runs, but its D cannot be read, so it may run in this dir".to_owned(),
            );
        }
    }
    if f.is_floor
        && let Some(l) = floor_gate_line(f.floor)
    {
        return RetireGate::Wait(l);
    }
    if f.is_floor
        && let Some(l) = shim_line(f.shims)
    {
        return RetireGate::Wait(l);
    }
    if f.is_floor && f.home_empty {
        return RetireGate::Wait(
            "~/.claude holds no login yet (Orca puts its active account there at its next start); \
             csm launches run in this dir until then"
                .to_owned(),
        );
    }
    if f.grant == DirGrant::Unreadable && !f.store_less {
        return RetireGate::Wait(
            "a grant in the dir could not be read (is the Keychain locked?); nothing moved"
                .to_owned(),
        );
    }
    RetireGate::Go
}

// ─── settle ───────────────────────────────────────────────────────────────────

/// Does a quarantined `Retired` grant still want its account's stash: the
/// stash holds no grant, or Orca's read-back acceptance rule (with no
/// last-written grant, csm's case) takes the entry over it. Pure.
pub(crate) fn settle_wanted(entry: &str, stash: Option<&str>) -> bool {
    match stash {
        None => true,
        Some(s) if s == entry => false,
        Some(s) => readback::accepts(entry, s, true),
    }
}

/// Why settle cannot write a stash now, if it cannot: the same gate as A3's
/// read-back. Pure.
pub(crate) fn settle_gate(
    orca_running: bool,
    os: crate::orca::HostOs,
    version_ok: bool,
    store_access_allowed: bool,
) -> Result<(), &'static str> {
    if orca_running {
        Err("settle files it once Orca is stopped (quit Orca and run `csm migrate`)")
    } else if os == crate::orca::HostOs::Windows {
        Err("csm does not write a stash on Windows")
    } else if !version_ok || !store_access_allowed {
        Err("the stash is not csm's to write")
    } else {
        Ok(())
    }
}

/// How settle runs.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SettleOpts<'a> {
    pub dry_run: bool,
    pub network_due: bool,
    /// Unix seconds.
    pub now: i64,
    /// The recorded legacy dirs: one not yet retired may still hold a
    /// grant's refresh token.
    pub legacy_dirs: &'a [PathBuf],
    /// May settle refresh a grant whose access token got a 401? Not on a
    /// run bound to a launch ([`RetireOpts::launch_bound`]): the process
    /// exits [`super::EXIT_GRACE`] after claude does, and a refresh cut
    /// between the token endpoint's rotation and the filing of its answer
    /// would lose the only live refresh token. `csm migrate` and the
    /// terminal FULL words refresh.
    pub refresh: bool,
}

/// The quarantine reasons settle files into a stash: a retired dir's grant
/// ([`Reason::Retired`]), the grant settle's own refresh returned
/// ([`Reason::Rotated`]; also a read-back's that could not be stored), and
/// what the cutover attributed in `~/.claude` ([`Reason::Cutover`]). Each
/// only when filed under an account. Pure.
pub(crate) fn settle_reason(reason: Reason) -> bool {
    matches!(
        reason,
        Reason::Retired | Reason::Rotated | Reason::Cutover | Reason::ExtraLogins
    )
}

/// May settle refresh a quarantined grant whose access token got a 401?
/// A refresh rotates the refresh token, so only when the grant has one and
/// nothing else still holds it (`held`: every stash's, `~/.claude`'s and
/// csm's `D`'s, and every legacy dir not yet retired). Pure.
pub(crate) fn settle_refresh_allowed(entry: &str, held: &[String]) -> Result<(), &'static str> {
    let Some(rt) = crate::orca::refresh::refresh_token_of(entry) else {
        return Err("it holds no refresh token");
    };
    if held.contains(&rt) {
        return Err(
            "its refresh token is still held elsewhere (a stash, ~/.claude or a profile dir), \
             which a refresh would log out",
        );
    }
    Ok(())
}

/// File each quarantined `Retired` grant into its account's stash when it
/// is fresher ([`settle_wanted`]) and the profile endpoint puts it on the
/// stash's account; one that profiles elsewhere is refiled under the
/// veto's reason for `accounts doctor`. Only with Orca stopped, under
/// `switch.lock`, and only onto a stash that did not change since it was
/// read: csm never writes a stash behind Orca. Returns how many grants
/// still wait.
pub(crate) fn settle(
    ctx: &Context,
    view: &OrcaView,
    http: &dyn OauthHttp,
    procs: &dyn crate::orca::live::ProcFacts,
    opts: SettleOpts<'_>,
    st: &mut super::state::MigrationState,
    report: &mut super::Report,
) -> usize {
    let q = Quarantine::new(ctx.os(), &ctx.state);
    let mut waiting = 0usize;
    for m in q.list() {
        if !settle_reason(m.reason) {
            continue;
        }
        let Some(id) = m.matched_account.as_deref() else {
            continue;
        };
        if !view.host_accounts().any(|a| a.id == id) {
            continue;
        }
        let fp = m.fingerprint.as_str();
        let entry = match q.get(fp) {
            Ok(Some(e)) => e,
            Ok(None) => continue,
            Err(e) => {
                waiting += 1;
                report
                    .pending
                    .push(format!("settle: cannot read quarantine entry {fp} ({e})"));
                continue;
            }
        };
        let stash = Stash::open(&ctx.user_data.dir, id, stash_path(view, id).as_deref()).ok();
        let managed = stash
            .as_ref()
            .and_then(|s| s.credentials(ctx.os()).ok().flatten());
        if !settle_wanted(entry.expose(), managed.as_ref().map(|s| s.expose())) {
            continue;
        }
        let what = format!("a retired dir's grant ({fp}) is fresher than stash {id}");
        if let Err(why) = settle_gate(
            ctx.orca_running(procs),
            ctx.os(),
            ctx.version_ok,
            ctx.user_data.store_access_allowed(),
        ) {
            waiting += 1;
            report.pending.push(format!("{what}; {why}"));
            continue;
        }
        if opts.dry_run {
            report
                .pending
                .push(format!("{what}; would file it into the stash"));
            continue;
        }
        if !opts.network_due {
            waiting += 1;
            report
                .pending
                .push(format!("{what}; settle waits after a network failure"));
            continue;
        }
        let stash_uuid = stash
            .as_ref()
            .and_then(|s| s.oauth_account().ok().flatten())
            .and_then(|v| OauthIdentity::from_value(&v).account_uuid);
        match readback::profile_veto(http, entry.expose(), stash_uuid.as_deref()) {
            Ok(readback::Veto::Owner) => {
                match store_settled(ctx, view, procs, id, managed.as_ref(), &entry) {
                    Ok(true) => report.changed.push(format!(
                        "stash {id} now holds a retired dir's fresher grant ({fp})"
                    )),
                    Ok(false) => {
                        waiting += 1;
                        report.pending.push(format!(
                            "{what}; Orca started or the stash changed, so it waits for the next run"
                        ));
                    }
                    Err(e) => {
                        waiting += 1;
                        report.pending.push(format!("{what}; {e}"));
                    }
                }
            }
            Ok(readback::Veto::Quarantine(reason, answer)) => {
                unattribute(&q, fp, reason, answer.as_ref(), id, report)
            }
            // Usually an access token that simply expired while the dir
            // waited to retire: refresh it (as read-back does), but only
            // once nothing else holds its refresh token.
            Ok(readback::Veto::Unauthorized) if !opts.refresh => {
                waiting += 1;
                report.pending.push(format!(
                    "{what}; its access token got a 401, and csm refreshes it only in `csm \
                     migrate` or a csm command in a terminal, never beside a launch"
                ));
            }
            Ok(readback::Veto::Unauthorized) => match settle_refreshed(
                ctx,
                view,
                procs,
                http,
                &q,
                id,
                fp,
                managed.as_ref(),
                &entry,
                stash_uuid.as_deref(),
                opts.legacy_dirs,
            ) {
                Ok(Refreshed::Stored(new_fp)) => report.changed.push(format!(
                    "stash {id} now holds a retired dir's grant ({fp}), refreshed ({new_fp})"
                )),
                Ok(Refreshed::Refiled(reason)) => refile(&q, fp, reason, id, report),
                Ok(Refreshed::Foreign(reason, answer)) => {
                    unattribute(&q, fp, reason, answer.as_ref(), id, report)
                }
                Ok(Refreshed::Waits(why)) => {
                    waiting += 1;
                    report
                        .pending
                        .push(format!("{what}; its access token got a 401 and {why}"));
                }
                Err(e) => {
                    if matches!(
                        e.downcast_ref::<crate::orca::OrcaError>(),
                        Some(crate::orca::OrcaError::Network(_))
                    ) {
                        st.next_attempt_at = Some(opts.now + super::state::NETWORK_BACKOFF_SECS);
                    }
                    waiting += 1;
                    report.pending.push(format!("{what}; {e}"));
                }
            },
            Err(e) => {
                st.next_attempt_at = Some(opts.now + super::state::NETWORK_BACKOFF_SECS);
                waiting += 1;
                report.pending.push(format!("{what}; {e}"));
            }
        }
    }
    waiting
}

/// Write `entry` into stash `id` under `switch.lock`, if Orca is still
/// stopped and the stash still holds `seen`. `Ok(false)`: it was not
/// written.
fn store_settled(
    ctx: &Context,
    view: &OrcaView,
    procs: &dyn crate::orca::live::ProcFacts,
    id: &str,
    seen: Option<&crate::orca::SecretString>,
    entry: &crate::orca::SecretString,
) -> anyhow::Result<bool> {
    let _lock = fsx::SwitchLock::acquire(&ctx.state, crate::orca::context::LOCK_WAIT)?;
    if ctx.orca_running(procs) {
        return Ok(false);
    }
    store_settled_locked(ctx, view, id, seen, entry.expose())
}

/// What a settle refresh did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Refreshed {
    /// The refreshed grant went into the stash (its fingerprint).
    Stored(String),
    /// Not now; why.
    Waits(String),
    /// The grant is not the account's to store: refile it so.
    Refiled(Reason),
    /// The refreshed grant profiled as another account (or none provably):
    /// the refreshed copy is already refiled unattributed; the entry goes
    /// the same way.
    Foreign(Reason, Option<(u16, Option<String>)>),
}

/// Every refresh token a grant settle might refresh could share: every
/// stash's but `own`'s (whose grant the refresh replaces), `~/.claude`'s
/// and csm's `D`'s (the file and, on macOS, the runtime items), and every
/// legacy dir that still exists. `Err` when one could not be read (fail
/// closed: no refresh).
fn held_refresh_tokens(
    ctx: &Context,
    view: &OrcaView,
    own: &str,
    legacy_dirs: &[PathBuf],
) -> Result<Vec<String>, String> {
    use crate::orca::refresh::refresh_token_of;
    let mut held = Vec::new();
    let mut push = |json: &str| {
        if let Some(rt) = refresh_token_of(json) {
            held.push(rt);
        }
    };
    for a in view.host_accounts().filter(|a| a.id != own) {
        let Ok(s) = Stash::open(
            &ctx.user_data.dir,
            &a.id,
            stash_path(view, &a.id).as_deref(),
        ) else {
            continue;
        };
        match s.credentials(ctx.os()) {
            Ok(Some(c)) => push(c.expose()),
            Ok(None) => {}
            Err(_) => return Err(format!("stash {} cannot be read", a.id)),
        }
    }
    let implicit = runtime_paths(None, &ctx.env.home, |p| p.exists());
    for paths in [&implicit, &ctx.paths] {
        match crate::orca::read_capped(&paths.credentials_path, 1024 * 1024) {
            Ok(Some(t)) => push(&t),
            Ok(None) => {}
            Err(_) => {
                return Err(format!(
                    "{} cannot be read",
                    paths.credentials_path.display()
                ));
            }
        }
        if ctx.os() == crate::orca::HostOs::MacOs {
            let dir = paths.config_dir.to_string_lossy().into_owned();
            for d in [Some(dir.as_str()), None] {
                match keychain::read_runtime_scoped(d, &ctx.keychain_user) {
                    Ok(Some(v)) => push(v.expose()),
                    Ok(None) => {}
                    Err(_) => return Err("a runtime Keychain item cannot be read".to_owned()),
                }
            }
        }
    }
    for dir in legacy_dirs.iter().filter(|d| d.is_dir()) {
        let (grants, unreadable) = dir_grants(ctx, dir);
        if unreadable {
            return Err(format!(
                "a Keychain item of {} cannot be read",
                dir.display()
            ));
        }
        for (_, _, g) in &grants {
            push(g.expose());
        }
    }
    Ok(held)
}

/// Settle for a grant whose access token got a 401: under `switch.lock`
/// with Orca stopped and nothing else holding its refresh token, refresh
/// it, file the result in the quarantine first (it is now the only live
/// copy), then store it into stash `id` when it profiles as that account
/// and the stash still holds `seen`. Stored, both entries go: the old
/// refresh token is spent and the new grant sits in the stash. A refresh
/// the endpoint refuses (`invalid_grant`) refiles the entry as
/// unauthorized.
#[allow(clippy::too_many_arguments, reason = "settle's facts, passed through")]
fn settle_refreshed(
    ctx: &Context,
    view: &OrcaView,
    procs: &dyn crate::orca::live::ProcFacts,
    http: &dyn OauthHttp,
    q: &Quarantine,
    id: &str,
    fp: &str,
    seen: Option<&crate::orca::SecretString>,
    entry: &crate::orca::SecretString,
    stash_uuid: Option<&str>,
    legacy_dirs: &[PathBuf],
) -> anyhow::Result<Refreshed> {
    use crate::orca::refresh::{RefreshFail, refresh_grant};
    let _lock = fsx::SwitchLock::acquire(&ctx.state, crate::orca::context::LOCK_WAIT)?;
    if ctx.orca_running(procs) {
        return Ok(Refreshed::Waits("Orca started".to_owned()));
    }
    let rt = crate::orca::refresh::refresh_token_of;
    if let (Some(a), Some(b)) = (rt(entry.expose()), seen.and_then(|s| rt(s.expose())))
        && a == b
    {
        // The stash holds the same refresh token: nothing to carry over.
        return Ok(Refreshed::Refiled(Reason::Superseded));
    }
    let held = match held_refresh_tokens(ctx, view, id, legacy_dirs) {
        Ok(h) => h,
        Err(why) => return Ok(Refreshed::Waits(format!("{why}, so it is not refreshed"))),
    };
    if let Err(why) = settle_refresh_allowed(entry.expose(), &held) {
        return Ok(Refreshed::Waits(why.to_owned()));
    }
    let now_ms = chrono::Utc::now().timestamp_millis();
    // From the POST until the rotated grant is filed it lives only here:
    // the process exiting in between would lose it.
    let critical = super::Critical::enter();
    let rotated = match refresh_grant(entry.expose(), http, now_ms) {
        Ok(r) => r,
        Err(RefreshFail::Status(s)) if (400..500).contains(&s) => {
            return Ok(Refreshed::Refiled(Reason::Unauthorized));
        }
        Err(RefreshFail::NoRefreshToken) => return Ok(Refreshed::Refiled(Reason::Unauthorized)),
        Err(RefreshFail::NoAnswer) => {
            return Err(anyhow::Error::from(crate::orca::OrcaError::Network(
                "the token endpoint did not answer".to_owned(),
            )));
        }
        Err(e) => {
            return Ok(Refreshed::Waits(format!(
                "its refresh gave no usable answer ({e:?})"
            )));
        }
    };
    let filed = q.file(
        rotated.expose(),
        Reason::Rotated,
        "refresh",
        Some(id),
        None,
        now_ms,
    )?;
    drop(critical);
    match readback::profile_veto(http, rotated.expose(), stash_uuid)? {
        readback::Veto::Owner => {
            // The liveness check above came before two network calls: Orca
            // may have started since. csm never writes a stash behind a
            // running Orca; the rotated grant waits in the quarantine.
            if ctx.orca_running(procs) {
                return Ok(Refreshed::Waits(
                    "Orca started during the refresh; the refreshed grant waits in the quarantine"
                        .to_owned(),
                ));
            }
            if !store_settled_locked(ctx, view, id, seen, rotated.expose())? {
                return Ok(Refreshed::Waits(
                    "the stash changed meanwhile; the refreshed grant waits in the quarantine"
                        .to_owned(),
                ));
            }
            q.remove(filed.fingerprint())?;
            q.remove(fp)?;
            Ok(Refreshed::Stored(filed.fingerprint().to_owned()))
        }
        readback::Veto::Quarantine(reason, answer) => {
            q.unattribute(
                filed.fingerprint(),
                reason,
                answer.as_ref().map(|(s, u)| (*s, u.as_deref())),
            )?;
            Ok(Refreshed::Foreign(reason, answer))
        }
        readback::Veto::Unauthorized => Ok(Refreshed::Waits(
            "the refreshed grant was refused too; it waits in the quarantine".to_owned(),
        )),
    }
}

/// [`store_settled`] for a caller that holds `switch.lock` and saw Orca
/// stopped under it.
fn store_settled_locked(
    ctx: &Context,
    view: &OrcaView,
    id: &str,
    seen: Option<&crate::orca::SecretString>,
    entry: &str,
) -> anyhow::Result<bool> {
    let s = Stash::open_for_write(&ctx.user_data.dir, id, stash_path(view, id).as_deref())?;
    let now = s.credentials(ctx.os())?;
    if now.as_ref().map(|n| n.expose()) != seen.map(|n| n.expose()) {
        return Ok(false);
    }
    // The entry replaces the whole blob, as Orca's own read-back does. MCP
    // logins only the stash holds would go with it: file the stash's blob
    // first.
    if let Some(old) = now.as_ref()
        && stash_keeps_more(old.expose(), entry)
    {
        Quarantine::new(ctx.os(), &ctx.state).file(
            old.expose(),
            Reason::ExtraLogins,
            "stash",
            Some(id),
            None,
            chrono::Utc::now().timestamp_millis(),
        )?;
    }
    s.write_credentials(&ctx.user_data.dir, ctx.os(), entry)?;
    Ok(true)
}

/// Does the stash's blob hold side state (MCP logins) `entry` lacks or
/// holds differently? Then writing `entry` over it would drop them. Pure.
pub(crate) fn stash_keeps_more(stash: &str, entry: &str) -> bool {
    !quarantine::uncovered(
        &quarantine::side_state(stash),
        &quarantine::side_state(entry),
    )
    .is_empty()
}

fn refile(q: &Quarantine, fp: &str, reason: Reason, id: &str, report: &mut super::Report) {
    match q.set_reason(fp, reason) {
        Ok(_) => report.changed.push(match reason {
            Reason::Superseded => format!(
                "grant {fp} from a retired dir shares stash {id}'s refresh token; left in the \
                 quarantine for `csm accounts doctor`"
            ),
            Reason::Unauthorized => format!(
                "grant {fp} from a retired dir was refused, and so was its refresh; left in the \
                 quarantine for `csm accounts doctor`"
            ),
            _ => format!(
                "grant {fp} from a retired dir does not profile as {id}'s account; left in the \
                 quarantine for `csm accounts doctor`"
            ),
        }),
        Err(e) => report
            .pending
            .push(format!("settle: cannot update quarantine entry {fp} ({e})")),
    }
}

/// Settle's profile veto: grant `fp`, filed under `id`, profiles as
/// another account (or none provably). Refile it unattributed with the
/// answer, so nothing later reads it as `id`'s.
fn unattribute(
    q: &Quarantine,
    fp: &str,
    reason: Reason,
    answer: Option<&(u16, Option<String>)>,
    id: &str,
    report: &mut super::Report,
) {
    match q.unattribute(fp, reason, answer.map(|(s, u)| (*s, u.as_deref()))) {
        Ok(_) => report.changed.push(format!(
            "grant {fp} from a retired dir does not profile as {id}'s account; left in the \
             quarantine unattributed for `csm accounts doctor`"
        )),
        Err(e) => report
            .pending
            .push(format!("settle: cannot update quarantine entry {fp} ({e})")),
    }
}

// ─── the retire stage (I/O shell) ─────────────────────────────────────────────

/// How stage C runs.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RetireOpts<'a> {
    pub dry_run: bool,
    /// Unix seconds.
    pub now: i64,
    /// An explicit `csm migrate`: the network backoff does not apply.
    pub explicit: bool,
    /// The launch's claude (the run after a spawn): a user of its `D` the
    /// session registry does not show yet.
    pub child: &'a super::LaunchChild,
    /// A run bound to a launch (before or after its spawn): cut
    /// [`super::EXIT_GRACE`] after claude exits, so settle does not
    /// refresh ([`SettleOpts::refresh`]).
    pub launch_bound: bool,
}

/// Stage C's end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RetireEnd {
    /// Every legacy dir, the registry and `~/.claude.shared` are retired
    /// and no retired grant waits for settle: the phase may be done.
    pub done: bool,
}

fn row(report: &mut super::Report, p: &LegacyProfile, line: impl Into<String>) {
    report.rows.push(super::ReportRow {
        name: p.name.clone(),
        dir: p.dir.clone(),
        stage: "retire",
        line: line.into(),
    });
}

fn same_dir(a: &Path, b: &Path) -> bool {
    lexical(a) == lexical(b)
        || std::fs::canonicalize(a)
            .ok()
            .is_some_and(|x| std::fs::canonicalize(b).ok() == Some(x))
}

/// The dirs a retiring profile dir may hold of its own that [`drain_own`]
/// moves into `~/.claude`: its `file-history` and `plans`, and the
/// transcript dirs of [`super::carry::SHARED_NAMES`] (`projects`, `todos`)
/// when the dir holds real ones instead of the usual links into
/// `~/.claude.shared`, so `claude --resume` and the picker still find
/// those sessions. `sessions`, `session-env` and `shell-snapshots` are a
/// dead claude's runtime state and `plugins` holds install paths B5
/// rewrites: they stay in `<dir>.retired`.
pub(crate) const DRAINED_DIRS: [&str; 4] = ["file-history", "plans", "projects", "todos"];

/// Move the dir's own [`DRAINED_DIRS`] into `~/.claude` (what `~/.claude`
/// already holds stays; a collision is left in the dir and so in
/// `<dir>.retired`), and put the lines of its own `history.jsonl` (a real
/// file, not the link into `~/.claude.shared`) in front of `~/.claude`'s.
/// Returns the lines.
pub(crate) fn drain_own(dir: &Path, home: &Path) -> io::Result<Vec<String>> {
    let mut lines = Vec::new();
    for n in DRAINED_DIRS {
        let src = dir.join(n);
        match std::fs::symlink_metadata(&src) {
            Ok(m) if m.is_dir() && !m.file_type().is_symlink() => {}
            _ => continue,
        }
        let dst = home.join(".claude").join(n);
        match std::fs::symlink_metadata(&dst) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                fsx::create_dir_all(&dst, 0o700)?;
            }
            Err(e) => return Err(e),
            Ok(m) if m.file_type().is_symlink() || !m.is_dir() => {
                lines.push(format!("{n} stays in the dir: ~/.claude/{n} is not a dir"));
                continue;
            }
            Ok(_) => {}
        }
        let left = drain(&src, &dst)?;
        if left.is_empty() {
            lines.push(format!("moved {n} into ~/.claude"));
        } else {
            lines.push(format!(
                "moved {n} into ~/.claude; left in the dir: {}",
                collided_list(&left)
            ));
        }
    }
    let src = dir.join("history.jsonl");
    if std::fs::symlink_metadata(&src).is_ok_and(|m| m.is_file()) {
        let dst = home.join(".claude").join("history.jsonl");
        match std::fs::symlink_metadata(&dst) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                fsx::create_dir_all(&home.join(".claude"), 0o700)?;
                move_path(&src, &dst)?;
                lines.push("moved history.jsonl into ~/.claude".to_owned());
            }
            Err(e) => return Err(e),
            Ok(m) if m.is_file() => {
                merge_history(&src, &dst, &mut || {})?;
                lines.push("put history.jsonl's lines in front of ~/.claude's".to_owned());
            }
            Ok(_) => lines.push(
                "history.jsonl stays in the dir: ~/.claude/history.jsonl is not a file".to_owned(),
            ),
        }
    }
    Ok(lines)
}

/// What a store-less retire does with `~/.claude`'s login before the floor
/// dir's grant goes to the quarantine. The cutover copied the floor's login
/// into `~/.claude` when that had none, and both copies stayed in use until
/// the reboot the floor dir waits for: either side may have rotated the
/// shared refresh token since.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HomeLogin {
    /// `~/.claude` holds none: the floor's moves there (design section 2).
    Move,
    /// `~/.claude` keeps its own: the same bytes, a fresher copy, another
    /// account's login, or one csm cannot tell apart.
    Keep,
    /// The same account's login (one refresh-token line, or both
    /// `.claude.json` files name one account) and the floor's copy wins
    /// Orca's read-back rule: it replaces `~/.claude`'s, which goes to the
    /// quarantine.
    Replace,
}

/// [`HomeLogin`] over the two credential blobs. `same_account`: whether
/// the floor dir's and `~/.claude`'s `.claude.json` name one account
/// (`None`: either names none). Pure.
pub(crate) fn store_less_home(
    floor: &str,
    home: Option<&str>,
    same_account: Option<bool>,
) -> HomeLogin {
    let Some(home) = home else {
        return HomeLogin::Move;
    };
    // Replacing drops what only `~/.claude`'s blob holds (MCP logins), as
    // settle never does to a stash ([`stash_keeps_more`]).
    if home == floor || same_account == Some(false) || stash_keeps_more(home, floor) {
        return HomeLogin::Keep;
    }
    let one_line = quarantine::fingerprint(floor) == quarantine::fingerprint(home);
    if (one_line || same_account == Some(true)) && readback::accepts(floor, home, true) {
        HomeLogin::Replace
    } else {
        HomeLogin::Keep
    }
}

/// The account uuid a `.claude.json` names.
fn named_uuid(config: &Path) -> Option<String> {
    read_json_object(config)
        .and_then(|m| m.get("oauthAccount").cloned())
        .filter(|v| !v.is_null())
        .and_then(|v| OauthIdentity::from_value(&v).account_uuid)
}

/// Bring the floor dir's file grant to `~/.claude` when it is the login
/// that should live on there ([`store_less_home`]), under `switch.lock`.
/// Returns the line, if it changed anything.
fn store_less_carry_login(
    ctx: &Context,
    q: &Quarantine,
    dir: &Path,
    floor: &str,
    now: i64,
) -> anyhow::Result<Option<String>> {
    let implicit = runtime_paths(None, &ctx.env.home, |p| p.exists());
    let home_now = crate::orca::read_capped(&implicit.credentials_path, 1024 * 1024)?;
    let floor_uuid = named_uuid(&dir.join(".claude.json"));
    let home_uuid = named_uuid(&implicit.config_path);
    let same = match (&floor_uuid, &home_uuid) {
        (Some(a), Some(b)) => Some(a == b),
        _ => None,
    };
    let what = store_less_home(floor, home_now.as_deref(), same);
    if what == HomeLogin::Keep {
        return Ok(None);
    }
    if let Some(old) = home_now.as_deref() {
        q.file(old, Reason::Superseded, "file", None, None, now)?;
    }
    fsx::create_dir_all(&implicit.config_dir, 0o700)?;
    fsx::write_atomic(
        &implicit.credentials_path,
        floor.as_bytes(),
        fsx::WriteOpts::PRIVATE,
    )?;
    if what == HomeLogin::Move
        && home_uuid.is_none()
        && let Some(v) = read_json_object(&dir.join(".claude.json"))
            .and_then(|m| m.get("oauthAccount").cloned())
            .filter(|v| !v.is_null())
    {
        let held = super::hold_config_lock(&implicit.config_path, B2_WAIT)?;
        held.locks.touch();
        crate::orca::runtime::restore_identity(&implicit, Some(&v))?;
    }
    Ok(Some(match what {
        HomeLogin::Move => "~/.claude had no login: moved the floor profile's there".to_owned(),
        _ => "the floor profile's login was fresher than ~/.claude's copy: it replaced it (the \
              older copy is in the quarantine)"
            .to_owned(),
    }))
}

/// A store-less host's floor dir: its login is carried to `~/.claude`
/// when that is where it should live on ([`store_less_home`]), then every
/// grant goes to the quarantine as `Retired` with no account (there is no
/// Orca account to name) and the dir is renamed `<dir>.retired`. Under
/// `switch.lock`.
pub(crate) fn retire_dir_store_less(
    ctx: &Context,
    dir: &Path,
    still_free: StillFree<'_>,
) -> anyhow::Result<String> {
    let retired = PathBuf::from(format!("{}.retired", dir.display()));
    if std::fs::symlink_metadata(&retired).is_ok() {
        bail!("{} already exists", retired.display());
    }
    let _lock = fsx::SwitchLock::acquire(&ctx.state, crate::orca::context::LOCK_WAIT)?;
    let (grants, unreadable) = dir_grants(ctx, dir);
    if unreadable {
        bail!("a Keychain item of the dir could not be read; nothing moved");
    }
    if let Err(who) = still_free() {
        bail!("the dir is in use now ({who}); nothing moved");
    }
    let q = Quarantine::new(ctx.os(), &ctx.state);
    let now = chrono::Utc::now().timestamp_millis();
    let mut lines = Vec::new();
    if let Some((_, _, g)) = grants.iter().find(|(s, _, _)| *s == "file")
        && let Some(l) = store_less_carry_login(ctx, &q, dir, g.expose(), now)?
    {
        lines.push(l);
    }
    let mut moved = 0usize;
    for (source, loc, grant) in grants {
        q.file(grant.expose(), Reason::Retired, source, None, None, now)?;
        delete_filed(ctx, source, &loc, grant.expose())?;
        moved += 1;
    }
    crate::e2e::point("migrate-retire-quarantined");
    move_path(dir, &retired)?;
    lines.push(format!(
        "{moved} grant(s) quarantined; dir renamed to {}",
        retired.display()
    ));
    Ok(lines.join("; "))
}

/// A dir that holds no login and names no account Orca has: renamed
/// `<dir>.retired` with nothing to file. The dir's grants are read again
/// first; one that appeared since the verdict (a login in the meantime)
/// stops the retire, so nothing is filed without an account.
pub(crate) fn retire_dir_no_login(ctx: &Context, dir: &Path) -> anyhow::Result<String> {
    let retired = PathBuf::from(format!("{}.retired", dir.display()));
    if std::fs::symlink_metadata(&retired).is_ok() {
        bail!("{} already exists", retired.display());
    }
    let (grants, unreadable) = dir_grants(ctx, dir);
    if unreadable {
        bail!("a Keychain item of the dir could not be read; nothing moved");
    }
    if !grants.is_empty() {
        bail!("the dir holds a login now; nothing moved, the next run adopts it first");
    }
    move_path(dir, &retired)?;
    Ok(format!(
        "held no login; dir renamed to {}",
        retired.display()
    ))
}

/// Where something still links into `~/.claude.shared`: an entry of
/// `~/.claude` or of an unregistered `~/.claude.*` dir whose link target
/// lies under it. The compat links inside `~/.claude.shared` point out of
/// it and do not count.
pub(crate) fn links_into_shared(home: &Path, unregistered: &[PathBuf]) -> Vec<PathBuf> {
    let sh = lexical(&shared_root(home));
    let sh_real = std::fs::canonicalize(&sh).ok();
    let under = |t: &Path| {
        lexical(t).starts_with(&sh) || sh_real.as_ref().is_some_and(|r| lexical(t).starts_with(r))
    };
    let mut out = Vec::new();
    for dir in std::iter::once(home.join(".claude")).chain(unregistered.iter().cloned()) {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(t) = std::fs::read_link(&p) else {
                continue;
            };
            let t = if t.is_absolute() { t } else { dir.join(t) };
            if under(&t) {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// Stage C over the legacy dirs (the floor dir last), with settle first.
pub(crate) fn retire_stage(
    env: &crate::orca::HostEnv,
    state_dir: &Path,
    legacy: &Legacy,
    unregistered: &[PathBuf],
    opts: RetireOpts<'_>,
    st: &mut super::state::MigrationState,
    report: &mut super::Report,
) -> RetireEnd {
    use super::state::StepStatus;
    crate::usage::reach::note("migrate-retire");
    let not_done = RetireEnd { done: false };
    let home = &env.home;
    let procs = crate::orca::live::SystemProcs;
    let ctx = Context::from_env(env.clone(), &procs);
    let view = match crate::orca::snapshot(&crate::orca::SnapshotOptions::default()) {
        Ok(v) => v,
        Err(e) => {
            report.errors.push(format!("retire: {e}"));
            return not_done;
        }
    };
    if view.running
        && let Some(why) = &view.rpc_error
    {
        report.pending.push(format!(
            "retire: Orca runs but did not list its accounts ({why})"
        ));
        return not_done;
    }
    let http = crate::orca::http::SystemHttp::from_env();
    let network_due = opts.explicit || st.network_due(opts.now);
    let store_less = store_kind(&ctx, &view) == StoreKind::StoreLess;
    let orca_main = crate::launch_context::orca_main_dir(env);
    let orca = OrcaAt::of(&orca_main, view.running);
    let floor_now = session_floor();
    let floor_str = floor_now.as_ref().ok().and_then(|o| o.as_deref());
    let fg = floor_gate(
        st.cutover.as_ref(),
        fsx::boot_id().as_deref(),
        floor_now.as_ref().map(|o| o.as_deref()).map_err(|_| ()),
    );
    let legacy_dirs: Vec<PathBuf> = legacy.profiles.iter().map(|p| p.dir.clone()).collect();
    let settle_opts = || SettleOpts {
        dry_run: opts.dry_run,
        network_due,
        now: opts.now,
        legacy_dirs: &legacy_dirs,
        refresh: !opts.launch_bound,
    };
    // Not in a dry run: settle reads stashes, and a dry run probes the
    // Keychain for presence only (decision 6).
    let waiting = if store_less || opts.dry_run {
        0
    } else {
        settle(&ctx, &view, &http, &procs, settle_opts(), st, report)
    };
    let host: Vec<crate::orca::record::AccountRecord> = view.host_accounts().cloned().collect();
    let quarantined: Vec<String> = Quarantine::new(ctx.os(), &ctx.state)
        .list()
        .into_iter()
        .map(|m| m.fingerprint)
        .collect();
    let floor_name = legacy.floor.as_deref();
    let mut order: Vec<&LegacyProfile> = legacy
        .profiles
        .iter()
        .filter(|p| Some(p.name.as_str()) != floor_name)
        .collect();
    order.extend(
        legacy
            .profiles
            .iter()
            .filter(|p| Some(p.name.as_str()) == floor_name),
    );
    let shims = cas_shims(home);
    // No Keychain mirror on Linux and Windows: until Orca starts in
    // `~/.claude`, the floor dir holds the only login csm launches reach.
    let home_empty = ctx.os() != crate::orca::HostOs::MacOs
        && !store_less
        && !runtime_paths(None, home, |p| p.exists())
            .credentials_path
            .is_file();
    let supervisor = supervision(home, &procs, st.legacy.as_ref().and_then(|l| l.seen_at));
    let mut all_gone = true;
    // A grant this run files under an account comes after this run's
    // settle: when settle would act on it ([`RetiredDir::settle_due`]),
    // the phase stays open so the next run's settle sees it.
    let mut filed_now = false;
    for p in order {
        let key = format!("C:{}", p.name);
        if std::fs::symlink_metadata(&p.dir).is_err() {
            let retired = PathBuf::from(format!("{}.retired", p.dir.display()));
            row(
                report,
                p,
                if retired.exists() {
                    format!("retired ({})", retired.display())
                } else {
                    "the dir is gone".to_owned()
                },
            );
            st.set_step(&key, StepStatus::Done, None);
            continue;
        }
        all_gone = false;
        let is_floor = Some(p.name.as_str()) == floor_name;
        let users = if opts.child.uses(&p.dir) {
            DirUsers::Live("this launch's claude runs in it".to_owned())
        } else {
            dir_users(ctx.os(), &p.dir, home, &procs)
        };
        let orca_d_is_dir = match &orca {
            OrcaAt::Stopped => Some(false),
            OrcaAt::Unreadable => None,
            OrcaAt::Dir(d) => Some(same_dir(d, &p.dir)),
        };
        let probe = if opts.dry_run {
            Probe::Presence
        } else {
            Probe::Read
        };
        let facts = profile_facts(&ctx, p, probe);
        let status = classify(&facts, &host, &|id| match probe {
            Probe::Read => stash_grant(&ctx, &view, id),
            Probe::Presence => None,
        });
        let grant = if opts.dry_run {
            DirGrant::Superseded
        } else {
            dir_grant_state(&facts, &status, &|fp| quarantined.iter().any(|q| q == fp))
        };
        let gate = retire_gate(&RetireFacts {
            users: &users,
            is_floor,
            orca_d_is_dir,
            floor: &fg,
            store_less,
            grant,
            shims: &shims,
            supervisor: supervisor.as_deref(),
            home_empty: is_floor && home_empty && p.dir.join(".credentials.json").is_file(),
        });
        match gate {
            RetireGate::Go => {}
            RetireGate::Stay(why) => {
                row(report, p, why);
                continue;
            }
            RetireGate::Wait(why) => {
                row(report, p, format!("waits: {why}"));
                report.pending.push(format!("{}: {why}", p.name));
                st.set_step(&key, StepStatus::Pending, Some("deferred"));
                continue;
            }
        }
        let id = if store_less {
            None
        } else {
            let needs_check = !opts.dry_run
                && matches!(status, Status::InOrca { .. })
                && grant == DirGrant::Superseded;
            if needs_check && !network_due {
                row(report, p, "waits for the network");
                report
                    .pending
                    .push(format!("{}: retire waits after a network failure", p.name));
                continue;
            }
            let check = match (&status, needs_check) {
                (Status::InOrca { id, .. }, true) => stash_check(&ctx, &view, &http, id),
                _ => StashCheck::Unverified,
            };
            let known = identity_match(&facts, &host).map(|r| r.id.clone());
            let verdict = if opts.dry_run {
                match &status {
                    Status::InOrca { id, .. } => Ok(RetireAs::Account(id.clone())),
                    other => {
                        retire_verdict(other, known.as_deref(), check, grant, &p.dir, home, true)
                    }
                }
            } else {
                retire_verdict(
                    &status,
                    known.as_deref(),
                    check,
                    grant,
                    &p.dir,
                    home,
                    p.dir.is_dir(),
                )
            };
            let verdict = verdict.and_then(|v| match (&v, &status) {
                (RetireAs::Account(id), Status::InOrca { .. }) if !opts.dry_run => {
                    newer_grant_gate(ctx.os(), grant, id).map(|()| v)
                }
                _ => Ok(v),
            });
            match verdict {
                Ok(id) => Some(id),
                Err(why) => {
                    row(report, p, format!("waits: {why}"));
                    report.pending.push(format!("{}: {why}", p.name));
                    st.set_step(&key, StepStatus::Pending, Some("deferred"));
                    continue;
                }
            }
        };
        if let Err(why) = references_verdict(&references_to(home, &p.dir)) {
            row(report, p, format!("waits: {why}"));
            report.pending.push(format!("{}: {why}", p.name));
            st.set_step(&key, StepStatus::Pending, Some("deferred"));
            continue;
        }
        // The plugin registries: a legacy session may have recorded paths
        // through the dir since B5 last ran. Point them at ~/.claude/plugins,
        // and hold the dir while one would still break with its rename.
        if !opts.dry_run {
            match apply_plugin_paths(home, &legacy_dirs, state_dir, false) {
                Ok(lines) => report.changed.extend(lines),
                Err(e) => {
                    let e = anyhow::Error::from(e);
                    failed(
                        report,
                        st,
                        &key,
                        format!("{}: the plugin registries", p.name),
                        &e,
                        opts.now,
                    );
                    continue;
                }
            }
        }
        let plugin_refs = plugin_refs_into(home, &p.dir, opts.dry_run);
        if !plugin_refs.is_empty() {
            let why = plugin_refs_line(&p.dir, &plugin_refs);
            row(report, p, format!("waits: {why}"));
            report.pending.push(format!("{}: {why}", p.name));
            st.set_step(&key, StepStatus::Pending, Some("deferred"));
            continue;
        }
        if opts.dry_run {
            row(
                report,
                p,
                "would move its own file-history, plans, projects, todos and history into \
                 ~/.claude, quarantine the dir's grants and rename it <dir>.retired",
            );
            continue;
        }
        match drain_own(&p.dir, home) {
            Ok(lines) => {
                for l in lines {
                    row(report, p, l);
                }
            }
            Err(e) => {
                let e = anyhow::Error::from(e);
                failed(
                    report,
                    st,
                    &key,
                    format!("{}: retire", p.name),
                    &e,
                    opts.now,
                );
                continue;
            }
        }
        if is_floor {
            // The last B2: live floor sessions wrote trust there until now.
            let i3 = i3_applies(&orca_main, floor_str, home);
            match config_step(
                home,
                state_dir,
                legacy,
                ConfigOpts {
                    dry_run: false,
                    wait: B2_WAIT,
                    switch_wait: B2_WAIT,
                    i3,
                },
            ) {
                Ok(Some(l)) => {
                    row(report, p, l.clone());
                    report.changed.push(l);
                }
                Ok(None) => {}
                Err(e) => {
                    failed(
                        report,
                        st,
                        &key,
                        format!("{}: retire", p.name),
                        &e,
                        opts.now,
                    );
                    continue;
                }
            }
        }
        // The gate's user check ran before the network calls above and
        // below: a claude may have started in the dir since.
        let still_free = || -> Result<(), String> {
            if opts.child.uses(&p.dir) {
                return Err("this launch's claude runs in it".to_owned());
            }
            match dir_users(ctx.os(), &p.dir, home, &procs) {
                DirUsers::Free => Ok(()),
                DirUsers::Live(w) => Err(w),
                DirUsers::Unknown(w) => Err(format!("{w}; counted as in use")),
            }
        };
        let r = match &id {
            Some(RetireAs::Account(id)) => retire_dir(&ctx, &view, &http, &p.dir, id, &still_free)
                .map(|r| {
                    filed_now |= r.settle_due;
                    r.line
                }),
            Some(RetireAs::NoLogin) => retire_dir_no_login(&ctx, &p.dir),
            None => retire_dir_store_less(&ctx, &p.dir, &still_free),
        };
        match r {
            Ok(line) => {
                row(report, p, line.clone());
                report.changed.push(format!("{}: {line}", p.name));
                st.set_step(&key, StepStatus::Done, None);
            }
            Err(e) => failed(
                report,
                st,
                &key,
                format!("{}: retire", p.name),
                &e,
                opts.now,
            ),
        }
    }
    let all_gone = all_gone
        || legacy
            .profiles
            .iter()
            .all(|p| std::fs::symlink_metadata(&p.dir).is_err());
    if !all_gone || store_less || opts.dry_run {
        return RetireEnd { done: false };
    }
    // The guard falls back to the registry's default: it stays while the
    // guard does.
    if let Some(l) = shim_line(&shims) {
        report.pending.push(l);
        return not_done;
    }
    // After the last dir: the registry, then ~/.claude.shared.
    match remove_legacy_files(home) {
        Ok(removed) => {
            for f in removed {
                report.changed.push(format!("removed {}", f.display()));
            }
        }
        Err(e) => {
            report
                .errors
                .push(format!("cannot remove the legacy registry: {e}"));
            return not_done;
        }
    }
    let sh = shared_root(home);
    if std::fs::symlink_metadata(&sh).is_ok() {
        let links = links_into_shared(home, unregistered);
        if !links.is_empty() {
            let shown: Vec<String> = links
                .iter()
                .take(3)
                .map(|p| p.display().to_string())
                .collect();
            report.pending.push(format!(
                "~/.claude.shared stays: still linked from {}",
                shown.join(", ")
            ));
            return not_done;
        }
        let retired = PathBuf::from(format!("{}.retired", sh.display()));
        if std::fs::symlink_metadata(&retired).is_ok() {
            report.pending.push(format!(
                "~/.claude.shared stays: {} already exists",
                retired.display()
            ));
            return not_done;
        }
        if let Err(e) = move_path(&sh, &retired) {
            report
                .errors
                .push(format!("cannot rename ~/.claude.shared: {e}"));
            return not_done;
        }
        report
            .changed
            .push(format!("renamed ~/.claude.shared to {}", retired.display()));
    }
    // A grant this run's retires filed came after this run's settle: settle
    // once more so the phase can end in this run.
    let (waiting, filed_now) = if resettle_due(waiting, filed_now) {
        let w = settle(&ctx, &view, &http, &procs, settle_opts(), st, report);
        (w, false)
    } else {
        (waiting, filed_now)
    };
    RetireEnd {
        done: retire_done(waiting, filed_now),
    }
}

/// Does the stage run settle a second time: only when nothing waited for
/// the first one (else the phase stays open anyway, and a second pass
/// would repeat its pending lines) and a retire in this run filed a grant
/// settle would act on. Pure.
pub(crate) fn resettle_due(waiting: usize, filed_now: bool) -> bool {
    waiting == 0 && filed_now
}

/// May the phase end: no quarantined grant waits for settle, and none
/// settle would act on was filed after the last settle of this run
/// (see [`resettle_due`]). A filed copy equal to the stash, or older than
/// it, does not hold the phase open. Pure.
pub(crate) fn retire_done(waiting: usize, filed_now: bool) -> bool {
    waiting == 0 && !filed_now
}
