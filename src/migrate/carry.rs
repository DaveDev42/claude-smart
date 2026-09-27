//! Stage B (carry), live-safe: what claude reads from its config dir moves
//! into `~/.claude` while the legacy dirs keep working.
//!
//! - B1 ([`shared_step`], [`b1_one`]): each [`SHARED_NAMES`] entry of
//!   `~/.claude.shared` becomes the real `~/.claude/<name>`, and a compat
//!   link takes its place in `~/.claude.shared`, so a profile dir's links
//!   (`~/.claude.<p>/projects` → `~/.claude.shared/projects`) resolve in
//!   two hops. Every crash point is one of four states the next run
//!   finishes from; both sides real are drained, skipping live sessions and
//!   anything changed in the last five minutes; a move across filesystems
//!   waits until no claude runs in a legacy dir or `~/.claude`.
//! - B2 ([`carry_config_from`], [`config_step`]): `~/.claude.json` is
//!   seeded from the floor profile's `.claude.json` minus `oauthAccount`,
//!   or gains the keys it lacks, the floor's trust fields and MCP servers,
//!   then the other profiles' add-only. Under Claude Code's own config lock
//!   ([`fsx::ClaudeConfigLock`]) and `switch.lock`, with a pre-image in
//!   `<state>/migrate/`; it runs again on every run until the floor dir is
//!   retired (live floor sessions keep writing trust). It also enforces I3:
//!   a stray `~/.claude/.claude.json` is merged and moved to
//!   `<state>/migrate/`.
//! - B3 ([`copy_missing`]): the floor profile's settings, `CLAUDE.md`,
//!   hooks, agents, commands, skills, output styles, keybindings and
//!   statusline script are copied where `~/.claude` lacks them.
//! - B4 ([`apply_smart`]) and B5 ([`apply_plugin_paths`]): csm's old state
//!   dir and the plugin registries' recorded paths.
//!
//! Nothing here touches the Keychain, the network or Orca's RPC, so an
//! Orca pane may run B1 and B2 before its spawn ([`CarryOpts::pane`]).

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::bail;
use serde_json::{Map, Value};

use crate::launch_context::OrcaMain;
use crate::orca::fsx;
use crate::orca::keychain;
use crate::orca::live::ProcFacts;
use crate::orca::runtime::read_json_object;
use crate::orca::{HostEnv, HostOs};

use super::legacy::*;
use super::state::{MigrationState, StepStatus};
use super::{Report, ReportRow};

// ─── B2: ~/.claude.json carry-over (pure) ─────────────────────────────────────

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
pub(crate) fn merge_servers(
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
pub(crate) fn merge_config_with(
    target: &mut Map<String, Value>,
    from: &Map<String, Value>,
    upgrade_defaults: bool,
) -> Vec<String> {
    let mut added = merge_trust_mcp(target, from, upgrade_defaults);
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

/// The trust fields and MCP servers of [`merge_config_with`], without the
/// onboarding keys: what B2 takes from a profile other than the floor.
/// Pure.
pub(crate) fn merge_trust_mcp(
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
    added
}

/// The top-level keys B2 never copies: the identity (the switch owns it)
/// and the two maps it merges entry by entry.
pub(crate) const NOT_COPIED: [&str; 3] = ["oauthAccount", "projects", "mcpServers"];

/// Add each top-level key of `from` that `target` lacks, except
/// [`NOT_COPIED`]. Pure.
pub(crate) fn add_missing_keys(
    target: &mut Map<String, Value>,
    from: &Map<String, Value>,
    added: &mut Vec<String>,
) {
    for (k, v) in from {
        if !NOT_COPIED.contains(&k.as_str()) && !target.contains_key(k) {
            target.insert(k.clone(), v.clone());
            added.push(k.clone());
        }
    }
}

/// Top-level `.claude.json` keys B2 carries into an existing
/// `~/.claude.json` that lacks them: claude gates its first-launch
/// onboarding on `hasCompletedOnboarding`.
pub(crate) const ONBOARDING_KEYS: [&str; 3] =
    ["hasCompletedOnboarding", "lastOnboardingVersion", "theme"];

/// B2's result.
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

/// B2 over the floor profile's config alone ([`carry_config_from`]). Pure.
pub(crate) fn carry_config(target: Option<Map<String, Value>>, from: &Map<String, Value>) -> Carry {
    carry_config_from(target, Some(from), None, &[])
}

/// B2 over the target's object (`None`: `~/.claude.json` does not exist
/// yet). A missing file is seeded from the floor profile's whole object
/// minus `oauthAccount` (binding decision), else from a stray
/// `~/.claude/.claude.json` the same way: claude keeps its onboarding state
/// and settings instead of running its first-launch flow inside Orca panes,
/// and the identity comes from the switch, never from a legacy profile.
/// Then the floor's top-level keys the file lacks (never `oauthAccount`),
/// its trust fields and MCP servers ([`merge_config`]); the stray file's
/// the same, add-only; then each other profile's trust fields and MCP
/// servers, add-only. A key the target holds keeps its value, except a
/// trust field at Claude Code's default, which the floor's real value
/// replaces. Pure.
pub(crate) fn carry_config_from(
    target: Option<Map<String, Value>>,
    floor: Option<&Map<String, Value>>,
    stray: Option<&Map<String, Value>>,
    others: &[&Map<String, Value>],
) -> Carry {
    let (mut map, seeded) = match (target, floor.or(stray)) {
        (Some(m), _) => (m, None),
        (None, Some(seed)) => {
            let mut m = seed.clone();
            m.remove("oauthAccount");
            let n = m.len();
            (m, Some(n))
        }
        (None, None) => (Map::new(), None),
    };
    let mut added = Vec::new();
    if let Some(f) = floor {
        add_missing_keys(&mut map, f, &mut added);
        added.extend(merge_config(&mut map, f));
    }
    if let Some(s) = stray {
        add_missing_keys(&mut map, s, &mut added);
        added.extend(merge_trust_mcp(&mut map, s, false));
    }
    for o in others {
        added.extend(merge_trust_mcp(&mut map, o, false));
    }
    Carry { map, seeded, added }
}

// ─── B1: shared dirs (pure decision) ──────────────────────────────────────────

/// The entries B1 carries from `~/.claude.shared` into `~/.claude`: the
/// dirs csm's provisioning linked (projects, sessions, plugins), the dirs
/// the profile layout linked beside them (todos, session-env,
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

/// Per-profile content claude reads from its config dir that B1 does not
/// carry (it is not shared between profiles). B3 copies the floor
/// profile's where `~/.claude` lacks them ([`b3_copies`]) and never
/// overwrites one there (the fleet may own `~/.claude/settings.json`); the
/// rest stays in the profile dir, and so in `<dir>.retired`, and is
/// reported. The last four are claude's own per-dir caches and runtime
/// state (pasted text, background jobs, its daemon, the browser
/// extension's state): never carried, listed so the report accounts for
/// them.
pub(crate) const PER_PROFILE_NAMES: [&str; 16] = [
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
    "paste-cache",
    "jobs",
    "daemon",
    "chrome",
];

/// What `~/.claude.shared/<name>` is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SharedSide {
    Absent,
    Dir,
    File,
    /// B1's compat link: a link naming `~/.claude/<name>`.
    Compat,
    /// The same file as `~/.claude/<name>` (a hard link: Windows' compat
    /// link for the history file).
    SameFile,
    /// A link elsewhere.
    OtherLink,
    /// Anything else.
    Other,
}

/// The facts B1 decides one name on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SharedFacts {
    /// `~/.claude.shared` exists (a compat link needs it).
    pub root: bool,
    pub local: LocalKind,
    pub shared: SharedSide,
    /// The shared entry and `~/.claude` are on one filesystem.
    pub same_fs: bool,
    /// A claude may run in a legacy dir or `~/.claude` (or the move may
    /// not copy at all: an Orca pane before its spawn).
    pub live: bool,
    /// Either history file changed in the last [`RECENT`].
    pub recent: bool,
}

/// What B1 does next for one name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SharedStep {
    /// The real entry is in `~/.claude` and the compat link in place, or
    /// there is nothing on either side.
    Done,
    /// Crash state "link and real shared" (the start): remove the link
    /// `~/.claude/<name>`, rename the shared entry into its place, link.
    Start,
    /// The link is gone, the shared entry not moved yet: rename, link.
    Move,
    /// Crash state "both real" (dirs): drain the smaller into the larger,
    /// then rename.
    Drain,
    /// Both real history files: the shared lines go in front of the local
    /// ones, then the shared file goes.
    Append,
    /// Crash state "real local, no shared": put the compat link.
    Link,
    /// `~/.claude/<name>` links to a shared entry that is gone: remove the
    /// dangling link (it names nothing).
    DropLink,
    /// Not now; the reason is the pending line.
    Wait(&'static str),
    /// Never on its own: the reason, for the operator.
    Skip(String),
}

/// The pending line of a move across filesystems.
pub(crate) const EXDEV_WAIT: &str = "~/.claude.shared is on another filesystem than ~/.claude; the move copies, so it waits until no claude runs in a legacy dir or ~/.claude";

/// The pending line of a history file that is still being written.
pub(crate) const RECENT_WAIT: &str =
    "the prompt history changed in the last 5 minutes; it is merged once it rests";

/// Decide B1 for one name. Crash states: link and real shared (start),
/// both real (drain), real local and no shared (make the link), real local
/// and compat link (done). Pure.
pub(crate) fn shared_step(f: &SharedFacts) -> SharedStep {
    use LocalKind as L;
    use SharedSide as S;
    let exdev = |step: SharedStep| {
        if !f.same_fs && f.live {
            SharedStep::Wait(EXDEV_WAIT)
        } else {
            step
        }
    };
    match (&f.local, f.shared) {
        (L::LinkToShared, S::Dir | S::File) => exdev(SharedStep::Start),
        (L::LinkToShared, S::Absent) => SharedStep::DropLink,
        (L::LinkToShared, _) => {
            SharedStep::Skip("links to a shared entry that is neither a file nor a dir".into())
        }
        (L::OtherLink, _) => SharedStep::Skip("is a link outside ~/.claude.shared".into()),
        (L::Other, _) => SharedStep::Skip("is neither a file nor a dir".into()),
        (_, S::Other) => SharedStep::Skip("the shared entry is neither a file nor a dir".into()),
        (_, S::OtherLink) => SharedStep::Skip("the shared entry is a link elsewhere".into()),
        (L::Absent, S::Dir | S::File) => exdev(SharedStep::Move),
        (L::Absent, _) => SharedStep::Done,
        (L::RealDir, S::Compat) | (L::RealFile, S::Compat | S::SameFile) => SharedStep::Done,
        (L::RealDir | L::RealFile, S::Absent) if f.root => SharedStep::Link,
        (L::RealDir | L::RealFile, S::Absent) => SharedStep::Done,
        (L::RealDir, S::Dir) => exdev(SharedStep::Drain),
        (L::RealFile, S::File) if f.recent => SharedStep::Wait(RECENT_WAIT),
        (L::RealFile, S::File) => SharedStep::Append,
        (L::RealDir, S::File | S::SameFile) | (L::RealFile, S::Dir) => SharedStep::Skip(
            "is a dir where the shared entry is a file, or the other way round".into(),
        ),
    }
}

/// The dry-run and plan wording of a step. Pure.
pub(crate) fn step_line(step: &SharedStep) -> String {
    match step {
        SharedStep::Done => "nothing to do".into(),
        SharedStep::Start => {
            "replace the link with the shared entry, leave a compat link in ~/.claude.shared".into()
        }
        SharedStep::Move => "move the shared entry in, leave a compat link".into(),
        SharedStep::Drain => "drain the smaller side into the larger, then move it in".into(),
        SharedStep::Append => "put the shared file's lines in front of it, then link".into(),
        SharedStep::Link => "leave a compat link in ~/.claude.shared".into(),
        SharedStep::DropLink => "remove the link to the missing shared entry".into(),
        SharedStep::Wait(why) => format!("waits: {why}"),
        SharedStep::Skip(why) => format!("skipped: {why}"),
    }
}

// ─── B1: facts ────────────────────────────────────────────────────────────────

/// How long a file rests before B1 drains or merges it.
pub(crate) const RECENT: Duration = Duration::from_secs(5 * 60);

/// What `~/.claude/<name>` is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LocalKind {
    Absent,
    /// A link into `~/.claude.shared/<name>`.
    LinkToShared,
    /// A link elsewhere.
    OtherLink,
    RealDir,
    RealFile,
    /// Anything else.
    Other,
}

pub(crate) fn shared_root(home: &Path) -> PathBuf {
    home.join(".claude.shared")
}

/// `p` without Windows' verbatim prefix (`\\?\`, `\\?\UNC\`), which a
/// junction's target carries. Pure.
fn plain(p: &Path) -> PathBuf {
    match p.to_str() {
        Some(s) if s.starts_with(r"\\?\UNC\") => PathBuf::from(format!(r"\\{}", &s[8..])),
        Some(s) if s.starts_with(r"\\?\") => PathBuf::from(&s[4..]),
        _ => p.to_path_buf(),
    }
}

/// `p` with `.` dropped and each `..` taking off the component before it,
/// without touching the filesystem (the link target it normalizes may be
/// gone). Pure.
pub(crate) fn lexical(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in plain(p).components() {
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

/// Does the link text `target`, read from a link in `dir`, name `want`?
/// Compared by text (made absolute and normalized), not by resolving it,
/// because the entry it names may have moved (or be the link itself).
/// Windows compares without case. Pure.
pub(crate) fn names_shared(dir: &Path, target: &Path, want: &Path) -> bool {
    let target = plain(target);
    let abs = if target.is_absolute() {
        target
    } else {
        dir.join(target)
    };
    let (a, b) = (lexical(&abs), lexical(want));
    if cfg!(windows) {
        a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
    } else {
        a == b
    }
}

pub(crate) fn local_kind(local: &Path, shared: &Path) -> LocalKind {
    let Ok(md) = std::fs::symlink_metadata(local) else {
        return LocalKind::Absent;
    };
    if md.file_type().is_symlink() {
        let dir = local.parent().unwrap_or(Path::new(""));
        let by_text = std::fs::read_link(local).is_ok_and(|t| names_shared(dir, &t, shared));
        let by_target = matches!(
            (std::fs::canonicalize(local), std::fs::canonicalize(shared)),
            (Ok(a), Ok(b)) if a == b
        );
        return if by_text || by_target {
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

/// What `shared` is, `local` being the `~/.claude` entry a compat link
/// names. A file is [`SharedSide::File`] here; [`shared_facts`] tells a
/// hard link apart.
pub(crate) fn shared_side(shared: &Path, local: &Path) -> SharedSide {
    let md = match std::fs::symlink_metadata(shared) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return SharedSide::Absent,
        Err(_) => return SharedSide::Other,
        Ok(m) => m,
    };
    if md.file_type().is_symlink() {
        let dir = shared.parent().unwrap_or(Path::new(""));
        return match std::fs::read_link(shared) {
            Ok(t) if names_shared(dir, &t, local) => SharedSide::Compat,
            _ => SharedSide::OtherLink,
        };
    }
    if md.is_dir() {
        SharedSide::Dir
    } else if md.is_file() {
        SharedSide::File
    } else {
        SharedSide::Other
    }
}

/// Is `p` newer than `since`? An mtime that cannot be read counts as new.
fn changed_since(p: &Path, since: SystemTime) -> bool {
    match std::fs::symlink_metadata(p) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => false,
        Err(_) => true,
        Ok(m) => m.modified().map_or(true, |t| t > since),
    }
}

/// Gather [`SharedFacts`] for one name.
pub(crate) fn shared_facts(home: &Path, name: &str, hot: &Hot, pane: bool) -> SharedFacts {
    let d = home.join(".claude");
    let sh = shared_root(home);
    let (l, s) = (d.join(name), sh.join(name));
    let local = local_kind(&l, &s);
    let mut shared = shared_side(&s, &l);
    if shared == SharedSide::File && local == LocalKind::RealFile && fsx::same_file(&s, &l) {
        shared = SharedSide::SameFile;
    }
    let exists = |p: &Path| std::fs::symlink_metadata(p).is_ok();
    let anchor = if exists(&l) {
        l.clone()
    } else if exists(&d) {
        d.clone()
    } else {
        home.to_path_buf()
    };
    SharedFacts {
        root: sh.is_dir(),
        same_fs: shared == SharedSide::Absent || fsx::same_fs(&s, &anchor),
        live: hot.live || pane,
        recent: local == LocalKind::RealFile
            && shared == SharedSide::File
            && (changed_since(&l, hot.since) || changed_since(&s, hot.since)),
        local,
        shared,
    }
}

// ─── B1: live sessions ────────────────────────────────────────────────────────

/// What a drain leaves alone: the entries of live sessions and anything
/// changed recently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Hot {
    /// Live (or unverifiable) session ids: an entry whose name starts with
    /// one is theirs (a transcript, its tool-results dir, its todos,
    /// session-env and file-history entries).
    pub sids: Vec<String>,
    /// Their pids: `sessions/<pid>.json`.
    pub pids: Vec<u32>,
    /// A file changed after this is hot.
    pub since: SystemTime,
    /// A session registry could not be listed: every drain waits.
    pub blind: bool,
    /// A claude may run in a legacy dir or `~/.claude`: a move across
    /// filesystems (a copy) waits.
    pub live: bool,
}

impl Hot {
    /// Nothing live; files changed after `now - RECENT` are hot.
    pub(crate) fn at(now: SystemTime) -> Hot {
        Hot {
            sids: Vec::new(),
            pids: Vec::new(),
            since: now.checked_sub(RECENT).unwrap_or(SystemTime::UNIX_EPOCH),
            blind: false,
            live: false,
        }
    }

    /// Is an entry with this name a live session's? Pure.
    pub(crate) fn names(&self, name: &str) -> bool {
        self.sids
            .iter()
            .any(|s| !s.is_empty() && name.starts_with(s.as_str()))
            || self.pids.iter().any(|p| {
                name.strip_prefix(p.to_string().as_str())
                    .is_some_and(|r| r.starts_with('.'))
            })
    }
}

/// [`Hot`] from the session registries of `~/.claude`, `~/.claude.shared`
/// and each legacy dir (each real dir once). A record that does not parse
/// makes the machine count as live; a registry that cannot be listed
/// also makes every drain wait.
pub(crate) fn live_hot(
    os: HostOs,
    home: &Path,
    dirs: &[PathBuf],
    procs: &dyn ProcFacts,
    now: SystemTime,
) -> Hot {
    let domain = crate::orca::runtime::this_pid_domain(os);
    let mut hot = Hot::at(now);
    let mut seen: Vec<PathBuf> = Vec::new();
    let roots = [home.join(".claude"), shared_root(home)]
        .into_iter()
        .chain(dirs.iter().cloned());
    for root in roots {
        let reg = root.join("sessions");
        let key = std::fs::canonicalize(&reg).unwrap_or_else(|_| reg.clone());
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        match crate::orca::runtime::scan_sessions(&reg, &domain, procs) {
            Ok(scan) => {
                hot.live |= scan.may_have_live();
                for rec in scan.live.iter().chain(&scan.unverifiable) {
                    hot.pids.push(rec.pid);
                    if let Some(id) = &rec.session_id {
                        hot.sids.push(id.clone());
                    }
                }
            }
            Err(_) => {
                hot.live = true;
                hot.blind = true;
            }
        }
    }
    hot
}

/// Count the launch's own claude child (the run after a spawn) as live:
/// it has not registered yet, so [`live_hot`] cannot see it. Pure.
pub(crate) fn child_is_hot(hot: &mut Hot, child_live: bool, child_sid: Option<&str>) {
    if !child_live {
        return;
    }
    hot.live = true;
    if let Some(sid) = child_sid
        && !hot.sids.iter().any(|s| s == sid)
    {
        hot.sids.push(sid.to_owned());
    }
}

/// B1's plan for every name, read-only, with nothing live and nothing
/// recent (the dry run and retire's precondition read it).
pub(crate) fn shared_plan(home: &Path) -> Vec<(&'static str, SharedStep)> {
    let far = SystemTime::now() + Duration::from_secs(10 * 365 * 24 * 3600);
    let quiet = Hot::at(far);
    SHARED_NAMES
        .iter()
        .map(|n| (*n, shared_step(&shared_facts(home, n, &quiet, false))))
        .collect()
}

// ─── B1: moves ────────────────────────────────────────────────────────────────

/// The history file [`SharedStep::Append`] writes: `shared`'s lines, then
/// `local`'s. `None` when `local` already starts with `shared` (a rerun
/// after a crash between the write and the removal), so a rerun never
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

/// How often [`merge_history`] re-reads a local history that grew under it.
const HISTORY_TRIES: usize = 5;

/// [`SharedStep::Append`]: put the shared history's lines in front of the
/// local one and retire the shared file, never dropping a line a live
/// session appends meanwhile (both files may still be written: a legacy
/// session appends through its link to `shared`, an implicit one to
/// `local`).
///
/// - `local` is replaced only while its length is still the one read
///   (else it is read again, at most [`HISTORY_TRIES`] times);
/// - `shared` is renamed aside in its own dir (atomic), so an append after
///   that creates a new shared file the next run merges; what reached the
///   old one after the read is appended to `local`, then the aside file
///   goes. When it no longer starts with what was merged (rewritten, not
///   appended to), it stays where it is and the step fails, naming it.
///
/// `after_read` runs between the reads and the writes (a test's writer).
pub(crate) fn merge_history(
    shared: &Path,
    local: &Path,
    after_read: &mut dyn FnMut(),
) -> io::Result<()> {
    use std::io::Write as _;
    fsx::guard(shared)?;
    fsx::guard(local)?;
    for _ in 0..HISTORY_TRIES {
        let (sb, lb) = (std::fs::read(shared)?, std::fs::read(local)?);
        after_read();
        if let Some(text) = appended(&sb, &lb) {
            if std::fs::metadata(local)?.len() != lb.len() as u64 {
                continue;
            }
            fsx::write_atomic(local, &text, fsx::WriteOpts::PRIVATE)?;
        }
        let name = shared
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let aside = shared.with_file_name(format!("{name}.csm-merged.{}", std::process::id()));
        std::fs::rename(shared, &aside)?;
        let now = std::fs::read(&aside)?;
        let Some(extra) = now.strip_prefix(sb.as_slice()) else {
            return Err(io::Error::other(format!(
                "{} changed under the merge (not by an append); it is kept at {}",
                shared.display(),
                aside.display()
            )));
        };
        if !extra.is_empty() {
            let mut f = std::fs::OpenOptions::new().append(true).open(local)?;
            let ends_nl = std::fs::read(local)?.last().is_none_or(|b| *b == b'\n');
            if !ends_nl {
                f.write_all(b"\n")?;
            }
            f.write_all(extra)?;
            f.sync_all()?;
        }
        return std::fs::remove_file(&aside);
    }
    Err(io::Error::new(
        io::ErrorKind::WouldBlock,
        format!("{} kept changing during the merge", local.display()),
    ))
}

/// Move `src` to `dst`, which must not exist: a file by a hard link and an
/// unlink (never replacing a file that appeared at `dst`), a dir by a
/// rename (which fails on a non-empty `dst`). Across filesystems it
/// copies, verifies and removes, and only when `allow_copy`: otherwise
/// `WouldBlock`.
pub(crate) fn rename_in(src: &Path, dst: &Path, allow_copy: bool) -> io::Result<()> {
    fsx::guard(src)?;
    fsx::guard(dst)?;
    let md = std::fs::symlink_metadata(src)?;
    let res = if md.is_file() {
        match std::fs::hard_link(src, dst) {
            Ok(()) => std::fs::remove_file(src),
            Err(e)
                if !matches!(
                    e.kind(),
                    io::ErrorKind::AlreadyExists | io::ErrorKind::CrossesDevices
                ) && std::fs::symlink_metadata(dst)
                    .is_err_and(|m| m.kind() == io::ErrorKind::NotFound) =>
            {
                // A filesystem without hard links.
                std::fs::rename(src, dst)
            }
            Err(e) => Err(e),
        }
    } else {
        std::fs::rename(src, dst)
    };
    match res {
        Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
            if !allow_copy {
                return Err(io::Error::new(io::ErrorKind::WouldBlock, EXDEV_WAIT));
            }
            if std::fs::symlink_metadata(dst).is_ok() {
                return Err(io::Error::from(io::ErrorKind::AlreadyExists));
            }
            let _critical = super::Critical::enter();
            copy_tree(src, dst)?;
            verify_tree(src, dst)?;
            remove_tree(src)
        }
        r => r,
    }
}

/// Did a move lose a race (the destination appeared)? Pure.
fn raced(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::AlreadyExists | io::ErrorKind::DirectoryNotEmpty
    )
}

/// Entries under `p`, counted to `cap`.
fn count_entries(p: &Path, cap: usize) -> io::Result<usize> {
    let mut n = 0;
    let mut stack = vec![p.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir)? {
            let e = e?;
            n += 1;
            if n >= cap {
                return Ok(n);
            }
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                stack.push(e.path());
            }
        }
    }
    Ok(n)
}

/// Does the tree at `p` hold a live session's entry or a file changed
/// since `hot.since`? Unreadable counts as hot.
fn tree_hot(p: &Path, hot: &Hot) -> bool {
    let Ok(rd) = std::fs::read_dir(p) else {
        return true;
    };
    for e in rd {
        let Ok(e) = e else {
            return true;
        };
        let name = e.file_name();
        if hot.names(&name.to_string_lossy()) {
            return true;
        }
        match e.file_type() {
            Ok(t) if t.is_dir() => {
                if tree_hot(&e.path(), hot) {
                    return true;
                }
            }
            Ok(_) => {
                if changed_since(&e.path(), hot.since) {
                    return true;
                }
            }
            Err(_) => return true,
        }
    }
    false
}

/// What a [`drain_hot`] left.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Drained {
    /// Entries moved or removed.
    pub moved: usize,
    /// Entries left in the source: a live session's, or changed recently.
    pub hot: usize,
    /// Entries both sides held with different content: the source's copy
    /// moved under `aside`.
    pub aside: usize,
}

/// Drain `src` into `dst`: an entry `dst` lacks moves, dirs both hold are
/// drained in turn, identical files lose the source copy, and a source
/// entry that collides moves under `aside` (kept, never deleted). A live
/// session's entries and anything changed since `hot.since` stay (a dir
/// holding such an entry is drained around them). `src` goes when empty.
pub(crate) fn drain_hot(
    src: &Path,
    dst: &Path,
    hot: &Hot,
    aside: &Path,
    allow_copy: bool,
    out: &mut Drained,
) -> io::Result<()> {
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let name = e.file_name();
        let (s, d) = (e.path(), dst.join(&name));
        if hot.names(&name.to_string_lossy()) {
            out.hot += 1;
            continue;
        }
        let smd = std::fs::symlink_metadata(&s)?;
        let s_dir = smd.is_dir() && !smd.file_type().is_symlink();
        let busy = if s_dir {
            tree_hot(&s, hot)
        } else {
            changed_since(&s, hot.since)
        };
        match drain_fate(&s, &d)? {
            DrainFate::Move if s_dir && busy => {
                match std::fs::create_dir(&d) {
                    Ok(()) => {}
                    Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(err) => return Err(err),
                }
                drain_hot(&s, &d, hot, &aside.join(&name), allow_copy, out)?;
            }
            DrainFate::Move if busy => out.hot += 1,
            DrainFate::Move => match rename_in(&s, &d, allow_copy) {
                Ok(()) => out.moved += 1,
                Err(err) if raced(&err) => out.hot += 1,
                Err(err) => return Err(err),
            },
            DrainFate::Recurse => drain_hot(&s, &d, hot, &aside.join(&name), allow_copy, out)?,
            DrainFate::RemoveSame if busy => out.hot += 1,
            DrainFate::RemoveSame => {
                remove_tree(&s)?;
                out.moved += 1;
            }
            DrainFate::Collide if busy => out.hot += 1,
            DrainFate::Collide => {
                fsx::create_dir_all(aside, 0o700)?;
                move_path(&s, &aside.join(&name))?;
                out.aside += 1;
            }
        }
    }
    if std::fs::read_dir(src)?.next().is_none() {
        fsx::guard(src)?;
        let _ = std::fs::remove_dir(src);
    }
    Ok(())
}

// ─── B1: one name ─────────────────────────────────────────────────────────────

/// How [`b1_one`] ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum B1End {
    /// Nothing left; what this run did, if anything.
    Done(Vec<String>),
    /// Not now: the pending line (and what this run did before it).
    Wait(String, Vec<String>),
    /// Never on its own: the reason.
    Skip(String),
}

/// Before an Orca pane's spawn a drain walks at most this many entries a
/// side; a larger tree drains in the run after the spawn.
#[cfg(not(test))]
pub(crate) const PANE_DRAIN_CAP: usize = 20_000;
/// Small under test, so a fixture can exceed it.
#[cfg(test)]
pub(crate) const PANE_DRAIN_CAP: usize = 8;

/// The pending line of a drain too large to run before a pane's spawn.
pub(crate) const PANE_DRAIN_WAIT: &str =
    "too large to drain before an Orca pane starts; the run after the spawn drains it";

/// How long the B1 names of a pane's pre-spawn run may take in all; the
/// names left then wait for the run after the spawn.
pub(crate) const PANE_B1_BUDGET: Duration = Duration::from_secs(1);

/// The pending line of a B1 name a pane's pre-spawn run left.
pub(crate) const PANE_B1_WAIT: &str =
    "an Orca pane started first; the run after the spawn moves it";

/// The pending line of a drain whose session registries cannot be read.
pub(crate) const BLIND_WAIT: &str =
    "a session registry cannot be listed, so live sessions are unknown; the drain waits";

/// Run B1 for one name until it is done or waits: each pass re-reads the
/// facts ([`shared_facts`]) and takes one [`shared_step`], so a crash at
/// any point leaves one of the four states the next run finishes from, and
/// a race (an entry appearing where one was moved) is a new state, not an
/// error. `aside` is where colliding entries of a drain go. `pane`: the
/// Orca-pane pre-spawn run, which never copies across filesystems.
pub(crate) fn b1_one(
    home: &Path,
    name: &str,
    hot: &Hot,
    pane: bool,
    aside: &Path,
) -> io::Result<B1End> {
    let d = home.join(".claude");
    let sh = shared_root(home);
    let (l, s) = (d.join(name), sh.join(name));
    let is_dir = name != "history.jsonl";
    let allow_copy = !pane && !hot.live;
    let mut did: Vec<String> = Vec::new();
    for _ in 0..6 {
        let step = shared_step(&shared_facts(home, name, hot, pane));
        let moved = match step {
            SharedStep::Done => return Ok(B1End::Done(did)),
            SharedStep::Wait(why) => return Ok(B1End::Wait(why.to_owned(), did)),
            SharedStep::Skip(why) => return Ok(B1End::Skip(why)),
            SharedStep::DropLink => {
                fsx::remove_link(&l)?;
                did.push("removed its link to the missing shared entry".into());
                continue;
            }
            SharedStep::Link => false,
            SharedStep::Start => {
                fsx::remove_link(&l)?;
                crate::e2e::point("migrate-b1-unlinked");
                true
            }
            SharedStep::Move => {
                fsx::create_dir_all(&d, 0o700)?;
                true
            }
            SharedStep::Append => {
                merge_history(&s, &l, &mut || {})?;
                did.push(format!("{}'s lines put in front of it", s.display()));
                continue;
            }
            SharedStep::Drain => {
                if hot.blind {
                    return Ok(B1End::Wait(BLIND_WAIT.to_owned(), did));
                }
                const CAP: usize = 1_000_000;
                let cap = if pane { PANE_DRAIN_CAP } else { CAP };
                let (nl, ns) = (count_entries(&l, cap)?, count_entries(&s, cap)?);
                if pane && (nl >= cap || ns >= cap) {
                    return Ok(B1End::Wait(PANE_DRAIN_WAIT.to_owned(), did));
                }
                let (src, dst) = if ns <= nl { (&s, &l) } else { (&l, &s) };
                let mut out = Drained::default();
                drain_hot(src, dst, hot, aside, allow_copy, &mut out)?;
                if out.moved > 0 {
                    did.push(format!(
                        "drained {} entr{} of {} into {}",
                        out.moved,
                        if out.moved == 1 { "y" } else { "ies" },
                        src.display(),
                        dst.display()
                    ));
                }
                if out.aside > 0 {
                    did.push(format!(
                        "{} entr{} held on both sides with different content: {}'s copy kept under {}",
                        out.aside,
                        if out.aside == 1 { "y" } else { "ies" },
                        src.display(),
                        aside.display()
                    ));
                }
                if out.hot > 0 {
                    return Ok(B1End::Wait(
                        format!(
                            "{} entr{} of {} belong to a live session or changed in the last 5 minutes; \
                             they move once they rest",
                            out.hot,
                            if out.hot == 1 { "y" } else { "ies" },
                            src.display()
                        ),
                        did,
                    ));
                }
                continue;
            }
        };
        if moved {
            match rename_in(&s, &l, allow_copy) {
                Ok(()) => did.push(format!("moved in from {}", s.display())),
                Err(e) if raced(&e) => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    return Ok(B1End::Wait(EXDEV_WAIT.to_owned(), did));
                }
                Err(e) => return Err(e),
            }
            crate::e2e::point("migrate-b1-moved");
        }
        match fsx::link_compat(&s, &l, is_dir) {
            Ok(()) => did.push(format!("{} now links to it", s.display())),
            // Something recreated the shared entry in the gap: the next
            // pass drains it.
            Err(e) if raced(&e) => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(B1End::Wait(
        "it kept changing while it moved; the next run finishes it".to_owned(),
        did,
    ))
}

// ─── left behind ──────────────────────────────────────────────────────────────

/// What a profile dir holds of its own that B1 does not carry: the
/// [`PER_PROFILE_NAMES`] that are not a link into `~/.claude.shared` (or
/// into `~/.claude`), and any [`SHARED_NAMES`] entry that is a real file or
/// dir instead of the usual link, which B1 never merges (retire's
/// `drain_own` moves the transcripts and history among them).
pub(crate) fn left_behind(home: &Path, dir: &Path) -> Vec<String> {
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
        .map(|n| {
            let merged = n == &"history.jsonl" || crate::migrate::retire::DRAINED_DIRS.contains(n);
            if merged {
                format!("{n} (its own, not the shared one: retire moves it into ~/.claude)")
            } else {
                format!("{n} (its own, not the shared one: not merged)")
            }
        });
    own.chain(unshared).collect()
}

// ─── B5: plugin paths ─────────────────────────────────────────────────────────

/// The plugin registries that record absolute install paths.
pub(crate) const PLUGIN_FILES: [&str; 2] = ["installed_plugins.json", "known_marketplaces.json"];

/// The keys whose values are absolute paths under a plugins dir.
pub(crate) const PLUGIN_PATH_KEYS: [&str; 2] = ["installPath", "installLocation"];

/// The old spellings of the plugins dir: every legacy profile dir's
/// `plugins` (the links claude recorded paths through) and
/// `~/.claude.shared/plugins`, each with a trailing separator. On Windows,
/// where a recorded path may use either separator, each base also comes
/// with every backslash spelled `/`.
pub(crate) fn old_plugin_prefixes(home: &Path, dirs: &[PathBuf]) -> Vec<String> {
    let all: Vec<PathBuf> = dirs
        .iter()
        .cloned()
        .chain(std::iter::once(shared_root(home)))
        .collect();
    plugin_prefixes_of(&all)
}

/// [`old_plugin_prefixes`] over exactly `dirs` (no shared root).
pub(crate) fn plugin_prefixes_of(dirs: &[PathBuf]) -> Vec<String> {
    let mut out = Vec::new();
    for d in dirs {
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

pub(crate) fn walk_plugin_paths(
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

/// Every `installPath`/`installLocation` value under one of `prefixes`.
/// Pure.
pub(crate) fn plugin_paths_under(v: &Value, prefixes: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    fn walk(v: &Value, prefixes: &[String], out: &mut Vec<String>) {
        match v {
            Value::Object(m) => {
                for (k, val) in m {
                    match val {
                        Value::String(s) if PLUGIN_PATH_KEYS.contains(&k.as_str()) => {
                            if prefixes.iter().any(|p| s.starts_with(p.as_str())) {
                                out.push(s.clone());
                            }
                        }
                        _ => walk(val, prefixes, out),
                    }
                }
            }
            Value::Array(a) => a.iter().for_each(|x| walk(x, prefixes, out)),
            _ => {}
        }
    }
    walk(v, prefixes, &mut out);
    out
}

/// Stage C's plugin check: the recorded plugin paths that go through
/// `dir`'s `plugins` and resolve today, so renaming `dir` would break
/// them (one that does not resolve now is broken already). Reads the
/// registries of [`plugins_dir_now`]. `after_b5`: leave out the paths
/// [`apply_plugin_paths`] would rewrite (a dry run, which does not run
/// it first).
pub(crate) fn plugin_refs_into(home: &Path, dir: &Path, after_b5: bool) -> Vec<String> {
    let prefixes = plugin_prefixes_of(&[dir.to_path_buf()]);
    let root = plugins_dir_now(home);
    let real = home.join(".claude").join("plugins");
    let real_now =
        std::fs::symlink_metadata(&real).is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink());
    let new_root = real.to_string_lossy().into_owned();
    PLUGIN_FILES
        .iter()
        .filter_map(|f| read_json_object(&root.join(f)))
        .flat_map(|m| plugin_paths_under(&Value::Object(m), &prefixes))
        .filter(|p| Path::new(p).exists())
        .filter(|p| {
            if !(after_b5 && real_now) {
                return true;
            }
            let mut v = serde_json::json!({ "installPath": p });
            rewrite_plugin_paths(&mut v, &prefixes, &new_root, &|q| Path::new(q).exists())
                .rewritten
                .is_empty()
        })
        .collect()
}

/// Stage C's line for a dir [`plugin_refs_into`] holds back. Pure.
pub(crate) fn plugin_refs_line(dir: &Path, refs: &[String]) -> String {
    let shown: Vec<&str> = refs.iter().take(2).map(String::as_str).collect();
    format!(
        "{} recorded plugin path(s) still go through {} ({}); csm points them at \
         ~/.claude/plugins once the files are there, else reinstall those plugins with /plugin",
        refs.len(),
        dir.join("plugins").display(),
        shown.join(", ")
    )
}

/// The plugins dir B5 reads the registries from: `~/.claude/plugins`
/// when it is a real dir already, else the shared one (plan time).
pub(crate) fn plugins_dir_now(home: &Path) -> PathBuf {
    let d = home.join(".claude").join("plugins");
    match std::fs::symlink_metadata(&d) {
        Ok(m) if m.is_dir() => d,
        _ => shared_root(home).join("plugins"),
    }
}

/// Plan time: how many recorded plugin paths go through an old prefix.
pub(crate) fn plugin_paths_preview(home: &Path, dirs: &[PathBuf]) -> usize {
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
/// left as it is and, with `report_dangling`, reported. Returns one line
/// per file touched (or, with `report_dangling`, holding such a path).
///
/// Runs on every carry, not once: a legacy session still running with
/// `CLAUDE_CONFIG_DIR=~/.claude.<p>` records new paths through its own
/// dir until the dir retires. It writes a registry only when a path
/// changes.
pub(crate) fn apply_plugin_paths(
    home: &Path,
    dirs: &[PathBuf],
    state: &Path,
    report_dangling: bool,
) -> io::Result<Vec<String>> {
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
        if !r.rewritten.is_empty() || (report_dangling && !r.dangling.is_empty()) {
            let mut line = format!(
                "{}: {} path(s) now under {}",
                path.display(),
                r.rewritten.len(),
                root.display()
            );
            if report_dangling && !r.dangling.is_empty() {
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

// ─── B4: csm's old state dir ──────────────────────────────────────────────────

/// `~/.claude.shared/smart`, csm's state dir under the profile layout.
pub(crate) fn legacy_smart_dir(home: &Path) -> PathBuf {
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

/// B4's count over the old state dir: (entries that move, entries that
/// stay). `None` when there is no old dir.
pub(crate) fn smart_preview(old: &Path) -> Option<(usize, usize)> {
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
/// there is no old dir or nothing moved.
pub(crate) fn apply_smart(old: &Path, new: &Path) -> io::Result<Option<String>> {
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
    // Nothing moved: nothing to say (a rerun finds the same collisions).
    Ok((moved > 0).then_some(line))
}

// ─── filesystem moves ─────────────────────────────────────────────────────────

/// Rename `src` to `dst`; across filesystems, copy, verify, then remove.
pub(crate) fn move_path(src: &Path, dst: &Path) -> io::Result<()> {
    fsx::guard(src)?;
    fsx::guard(dst)?;
    match std::fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
            let _critical = super::Critical::enter();
            copy_tree(src, dst)?;
            verify_tree(src, dst)?;
            remove_tree(src)
        }
        Err(e) => Err(e),
    }
}

pub(crate) fn copy_tree(src: &Path, dst: &Path) -> io::Result<()> {
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

pub(crate) fn verify_tree(src: &Path, dst: &Path) -> io::Result<()> {
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

pub(crate) fn remove_tree(p: &Path) -> io::Result<()> {
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
pub(crate) enum DrainFate {
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
pub(crate) fn drain_fate(s: &Path, d: &Path) -> io::Result<DrainFate> {
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
pub(crate) fn drain(src: &Path, dst: &Path) -> io::Result<Vec<PathBuf>> {
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

/// The paths a drain left, for the operator to merge by hand. Pure.
pub(crate) fn collided_list(left: &[PathBuf]) -> String {
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

// ─── the floor config ─────────────────────────────────────────────────────────

/// The floor profile's `.claude.json`: the object and the bytes it was
/// parsed from (B2 records their digest).
pub(crate) type FloorConfig = (Map<String, Value>, Vec<u8>);

/// The floor profile's `.claude.json` as B2 sees it, from a capped
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
pub(crate) fn read_floor_config(dir: &Path) -> Result<Option<FloorConfig>, String> {
    floor_config_from(crate::orca::read_capped_bytes(
        &dir.join(".claude.json"),
        64 * 1024 * 1024,
    ))
}

/// What to tell the operator when [`read_floor_config`] fails.
pub(crate) const FLOOR_CONFIG_FIX: &str =
    "fix it (or restore its .claude.json.backup); csm merges it on its next run";

// ─── B2: ~/.claude.json (the shell) ───────────────────────────────────────────

/// A stray `~/.claude/.claude.json`: Orca reads the config there once `D`
/// is an implicit `~/.claude`, while Claude Code reads `~/.claude.json`.
pub(crate) fn stray_config(home: &Path) -> PathBuf {
    home.join(".claude").join(".claude.json")
}

/// Does I3 apply (a stray `~/.claude/.claude.json` is merged and moved)?
/// Not while `~/.claude/.claude.json` is the live config: Orca runs with an
/// explicit `CLAUDE_CONFIG_DIR` naming `~/.claude` (its panes then read the
/// file there), or, with Orca stopped, the login session's floor names
/// `~/.claude`. Orca's environment unreadable: not now. Pure.
pub(crate) fn i3_applies(orca: &OrcaMain, floor: Option<&str>, home: &Path) -> bool {
    let d = lexical(&home.join(".claude"));
    let is_d = |p: &Path| lexical(p) == d;
    match orca {
        OrcaMain::Dir(o) => !(o.explicit && is_d(&o.dir)),
        OrcaMain::Unreadable => false,
        OrcaMain::Stopped => !floor
            .map(str::trim)
            .filter(|f| !f.is_empty())
            .is_some_and(|f| is_d(Path::new(f))),
    }
}

/// How [`config_step`] runs.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ConfigOpts {
    pub dry_run: bool,
    /// How long to wait for Claude Code's config lock.
    pub wait: Duration,
    /// How long to wait for `switch.lock` (zero before a pane's spawn: a
    /// busy one is a limit switch, and the pane does not wait for it).
    pub switch_wait: Duration,
    /// [`i3_applies`].
    pub i3: bool,
}

/// A `.claude.json` read as an object: `Ok(None)` when absent, `Err` when
/// unreadable or not an object (the message names the path, never the
/// content).
fn read_object(p: &Path) -> anyhow::Result<Option<FloorConfig>> {
    match crate::orca::read_capped_bytes(p, 64 * 1024 * 1024) {
        Ok(None) => Ok(None),
        Ok(Some(b)) => match serde_json::from_slice::<Value>(&b) {
            Ok(Value::Object(m)) => Ok(Some((m, b))),
            _ => bail!("{} is not a JSON object; left as is", p.display()),
        },
        Err(e) => bail!("{} cannot be read ({})", p.display(), e.kind()),
    }
}

/// B2: carry the legacy configs into `~/.claude.json` ([`carry_config_from`])
/// and, under I3, merge and move a stray `~/.claude/.claude.json`. Under
/// `switch.lock` (a switch writes `oauthAccount` into the same file) and
/// Claude Code's own `<config>.lock`; the write happens only when the file
/// did not change since it was read (else it reads again, three times),
/// with a pre-image in `<state>/migrate/`. Runs on every run while the
/// floor dir exists: live floor sessions keep writing trust there. Returns
/// the changed line.
pub(crate) fn config_step(
    home: &Path,
    state: &Path,
    legacy: &Legacy,
    o: ConfigOpts,
) -> anyhow::Result<Option<String>> {
    let floor_p = legacy
        .floor
        .as_ref()
        .and_then(|n| legacy.profiles.iter().find(|p| &p.name == n));
    let floor = match floor_p {
        Some(p) if p.dir.is_dir() => read_floor_config(&p.dir)
            .map_err(|e| anyhow::anyhow!("{e}; {FLOOR_CONFIG_FIX}"))?
            .map(|c| (p.name.as_str(), c)),
        _ => None,
    };
    let others: Vec<Map<String, Value>> = legacy
        .profiles
        .iter()
        .filter(|p| Some(&p.name) != legacy.floor.as_ref())
        .filter_map(|p| read_json_object(&p.dir.join(".claude.json")))
        .collect();
    let stray_p = stray_config(home);
    let stray_is_file = std::fs::symlink_metadata(&stray_p).is_ok_and(|m| m.is_file());
    let stray = if o.i3 && stray_is_file {
        read_object(&stray_p)?.map(|(m, _)| m)
    } else {
        None
    };
    if floor.is_none() && others.is_empty() && stray.is_none() {
        return Ok(None);
    }
    // Claude Code locks the path as it builds it; a linked ~/.claude.json
    // is written through its link, never replaced.
    let lock_at = home.join(".claude.json");
    let target = std::fs::canonicalize(&lock_at).unwrap_or_else(|_| lock_at.clone());
    let other_refs: Vec<&Map<String, Value>> = others.iter().collect();
    let merge = |existing: Option<Map<String, Value>>| {
        carry_config_from(
            existing,
            floor.as_ref().map(|(_, (m, _))| m),
            stray.as_ref(),
            &other_refs,
        )
    };
    let describe = |c: &Carry| -> String {
        let from = floor.as_ref().map_or("~/.claude/.claude.json", |(n, _)| *n);
        match c.seeded {
            Some(n) => format!(
                "{}: created from {from}'s {n} key(s) (oauthAccount left out), {} more merged",
                target.display(),
                c.added.len()
            ),
            None => format!(
                "{}: merged {} key(s) from the old profiles' configs",
                target.display(),
                c.added.len()
            ),
        }
    };
    if o.dry_run {
        let c = merge(read_object(&target)?.map(|(m, _)| m));
        let mut parts = Vec::new();
        if c.changed() {
            parts.push(describe(&c));
        }
        if stray.is_some() {
            parts.push(format!(
                "{} would be merged and moved to {}",
                stray_p.display(),
                state.join("migrate").display()
            ));
        }
        return Ok((!parts.is_empty()).then(|| parts.join("; ")));
    }
    // Nothing to merge: no lock is taken. A read that fails (a writer
    // mid-save) is read again under the locks.
    if stray.is_none()
        && let Ok(pre) = read_object(&target)
        && !merge(pre.map(|(m, _)| m)).changed()
    {
        if let Some((_, (_, bytes))) = &floor {
            record_merge_if_new(state, bytes)?;
        }
        return Ok(None);
    }
    let _switch = fsx::SwitchLock::acquire(state, o.switch_wait)?;
    // Not cut by the process exiting while the config lock is held: a
    // lock dir left behind holds every Claude Code's config save back
    // until it goes stale.
    let held = super::hold_config_lock(&lock_at, o.wait)?;
    let cc_lock = &held.locks;
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let mut line = None;
    let mut settled = false;
    for _ in 0..3 {
        let before = read_object(&target)?;
        let before_bytes = before.as_ref().map(|(_, b)| b.clone());
        let c = merge(before.map(|(m, _)| m));
        if !c.changed() {
            settled = true;
            break;
        }
        let mut text = serde_json::to_vec_pretty(&Value::Object(c.map.clone()))?;
        text.push(b'\n');
        cc_lock.touch();
        let now = crate::orca::read_capped_bytes(&target, 64 * 1024 * 1024)?;
        if now != before_bytes {
            continue;
        }
        let pre_dir = state.join("migrate");
        fsx::create_dir_all(&pre_dir, 0o700)?;
        if let Some(b) = &before_bytes {
            let pre = pre_dir.join(format!("claude.json.{stamp}.pre"));
            fsx::write_atomic(&pre, b, fsx::WriteOpts::PRIVATE)?;
        }
        crate::e2e::point("migrate-b2-write");
        fsx::guard(&target)?;
        fsx::write_atomic(&target, &text, fsx::WriteOpts::PRIVATE)?;
        line = Some(describe(&c));
        settled = true;
        break;
    }
    if !settled {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            format!("{} kept changing while csm merged it", target.display()),
        )
        .into());
    }
    if let Some((_, (_, bytes))) = &floor {
        // Retire checks B2 against this record, not against what the user
        // may have removed from ~/.claude.json since.
        record_merge(state, bytes)?;
    }
    if stray.is_some() {
        let pre_dir = state.join("migrate");
        fsx::create_dir_all(&pre_dir, 0o700)?;
        let to = pre_dir.join(format!("claude-dir.claude.json.{stamp}.pre"));
        move_path(&stray_p, &to)?;
        let moved = format!(
            "{}: merged into {} and moved to {}",
            stray_p.display(),
            target.display(),
            to.display()
        );
        line = Some(match line {
            Some(l) => format!("{l}; {moved}"),
            None => moved,
        });
    }
    Ok(line)
}

// ─── B3: the floor profile's own files ────────────────────────────────────────

/// Does B3 copy a top-level entry of the floor dir with this name? Pure.
pub(crate) fn b3_copies(name: &str) -> bool {
    (name.starts_with("settings") && name.ends_with(".json"))
        || matches!(
            name,
            "CLAUDE.md"
                | "hooks"
                | "agents"
                | "commands"
                | "skills"
                | "output-styles"
                | "keybindings.json"
                | "statusline-command.sh"
        )
}

/// Point every string in a copied settings file that names a path under
/// `from` at the same path under `to` (a hook or statusLine command in the
/// floor dir, which retire renames). Returns how many strings changed.
/// Pure.
pub(crate) fn rebase_paths(v: &mut Value, from: &str, to: &str) -> usize {
    match v {
        Value::String(s) => {
            let with_sep = |p: &str| format!("{p}{}", std::path::MAIN_SEPARATOR);
            let new = if s == from {
                to.to_owned()
            } else if s.contains(&with_sep(from)) {
                s.replace(&with_sep(from), &with_sep(to))
            } else {
                return 0;
            };
            *s = new;
            1
        }
        Value::Array(a) => a.iter_mut().map(|x| rebase_paths(x, from, to)).sum(),
        Value::Object(m) => m.values_mut().map(|x| rebase_paths(x, from, to)).sum(),
        _ => 0,
    }
}

/// The source B3 copies for `src`: a link into the floor dir,
/// `~/.claude.shared` or given relative is resolved (its target is renamed
/// later), any other link is copied as a link.
fn b3_source(src: &Path, floor: &Path, shared: &Path) -> io::Result<PathBuf> {
    let md = std::fs::symlink_metadata(src)?;
    if !md.file_type().is_symlink() {
        return Ok(src.to_path_buf());
    }
    let text = std::fs::read_link(src)?;
    let abs = lexical(&src.parent().unwrap_or(Path::new("")).join(&text));
    if !text.is_absolute() || abs.starts_with(floor) || abs.starts_with(shared) || cfg!(windows) {
        std::fs::canonicalize(src)
    } else {
        Ok(src.to_path_buf())
    }
}

/// B3: copy the floor dir's [`b3_copies`] entries that `~/.claude` lacks,
/// each through a temp name (copied, verified, then moved into place
/// without replacing anything). A settings file's paths into the floor
/// dir are rebased onto `~/.claude` ([`rebase_paths`]). Returns one line
/// per entry copied (or, on a dry run, to copy).
pub(crate) fn copy_missing(floor: &Path, home: &Path, dry_run: bool) -> io::Result<Vec<String>> {
    let d = home.join(".claude");
    let rd = match std::fs::read_dir(floor) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut names: Vec<String> = rd
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().to_str().map(str::to_owned))
        .filter(|n| b3_copies(n))
        .collect();
    names.sort();
    let pid = std::process::id();
    let mut out = Vec::new();
    for name in names {
        let (src, dst) = (floor.join(&name), d.join(&name));
        if std::fs::symlink_metadata(&dst).is_ok() {
            continue;
        }
        if dry_run {
            out.push(format!(
                "{}: would be copied from {}",
                dst.display(),
                src.display()
            ));
            continue;
        }
        fsx::create_dir_all(&d, 0o700)?;
        // Temps a crashed run left.
        let prefix = format!(".{name}.csm-copy.");
        for e in std::fs::read_dir(&d)?.filter_map(Result::ok) {
            if e.file_name()
                .to_str()
                .is_some_and(|n| n.starts_with(&prefix))
            {
                remove_tree(&e.path())?;
            }
        }
        let from = b3_source(&src, floor, &shared_root(home))?;
        let tmp = d.join(format!("{prefix}{pid}"));
        fsx::guard(&tmp)?;
        copy_tree(&from, &tmp)?;
        verify_tree(&from, &tmp)?;
        let tmd = std::fs::symlink_metadata(&tmp)?;
        let mut rebased = 0;
        if name.ends_with(".json") && tmd.is_file() {
            let bytes = std::fs::read(&tmp)?;
            if let Ok(mut v) = serde_json::from_slice::<Value>(&bytes) {
                rebased = rebase_paths(&mut v, &floor.to_string_lossy(), &d.to_string_lossy());
                if rebased > 0 {
                    let mut text = serde_json::to_vec_pretty(&v).map_err(io::Error::other)?;
                    if bytes.ends_with(b"\n") {
                        text.push(b'\n');
                    }
                    fsx::write_atomic(&tmp, &text, fsx::WriteOpts::PRIVATE)?;
                }
            }
        }
        let placed = if tmd.is_file() {
            std::fs::hard_link(&tmp, &dst)
        } else {
            std::fs::rename(&tmp, &dst)
        };
        match placed {
            Ok(()) => {}
            // It appeared meanwhile: the one there wins.
            Err(e) if raced(&e) => {
                remove_tree(&tmp)?;
                continue;
            }
            Err(e) => {
                let _ = remove_tree(&tmp);
                return Err(e);
            }
        }
        if std::fs::symlink_metadata(&tmp).is_ok() {
            remove_tree(&tmp)?;
        }
        let mut line = format!("{}: copied from {}", dst.display(), src.display());
        if rebased > 0 {
            line.push_str(&format!("; {rebased} path(s) into it now name ~/.claude"));
        }
        out.push(line);
    }
    Ok(out)
}

// ─── the stage ────────────────────────────────────────────────────────────────

/// How [`carry`] runs.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CarryOpts<'a> {
    pub dry_run: bool,
    /// The Orca-pane pre-spawn run: B1 (never copying across filesystems)
    /// and B2 (a 1 s lock wait) only.
    pub pane: bool,
    /// [`i3_applies`].
    pub i3: bool,
    pub now: SystemTime,
    /// The launch's claude runs (the run after a spawn): it has not
    /// registered in any session registry yet, so the scan cannot see it.
    /// A move across filesystems (copy, verify, remove) then waits: the
    /// child writes its transcript into the tree being copied.
    pub child_live: bool,
    /// That child's session id: its entries are hot for a drain.
    pub child_sid: Option<&'a str>,
}

/// How [`carry`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CarryEnd {
    /// Every step is done: the cutover may follow (I1).
    pub settled: bool,
}

/// How long B2 waits for its locks.
pub(crate) const B2_WAIT: Duration = Duration::from_secs(3);
/// B2's wait before an Orca pane's spawn.
pub(crate) const B2_PANE_WAIT: Duration = Duration::from_secs(1);

/// The row name stage B reports under.
const ROW: &str = "~/.claude";

fn b_row(report: &mut Report, home: &Path, line: String) {
    report.rows.push(ReportRow {
        name: ROW.to_owned(),
        dir: home.join(".claude"),
        stage: "carry",
        line,
    });
}

/// Record a failed step: a busy lock waits, anything else is an error.
fn b_failed(
    report: &mut Report,
    st: &mut MigrationState,
    key: &str,
    what: &str,
    e: &anyhow::Error,
) {
    let (class, waits) = super::adopt::error_class(e);
    let line = format!("{what}: {e}");
    if waits {
        st.set_step(key, StepStatus::Pending, Some(class));
        report.pending.push(line);
    } else {
        st.set_step(key, StepStatus::Error, Some(class));
        report.errors.push(line);
    }
}

/// Stage B. No Keychain, network or RPC. Every step is idempotent: each
/// run recomputes from disk and does what is left.
pub(crate) fn carry(
    env: &HostEnv,
    state: &Path,
    legacy: &Legacy,
    procs: &dyn ProcFacts,
    o: CarryOpts<'_>,
    st: &mut MigrationState,
    report: &mut Report,
) -> CarryEnd {
    crate::usage::reach::note("migrate-carry");
    let home = &env.home;
    let dirs: Vec<PathBuf> = legacy.profiles.iter().map(|p| p.dir.clone()).collect();
    let mut hot = live_hot(env.os, home, &dirs, procs, o.now);
    child_is_hot(&mut hot, o.child_live, o.child_sid);
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let mut settled = true;

    // B1.
    let pane_deadline = o.pane.then(|| std::time::Instant::now() + PANE_B1_BUDGET);
    for name in SHARED_NAMES {
        let key = format!("B1:{name}");
        let what = format!("{}", home.join(".claude").join(name).display());
        if !o.dry_run && pane_deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            settled = false;
            st.set_step(&key, StepStatus::Pending, Some("waits"));
            report.pending.push(format!("{what}: {PANE_B1_WAIT}"));
            continue;
        }
        if o.dry_run {
            let step = shared_step(&shared_facts(home, name, &hot, o.pane));
            if step != SharedStep::Done {
                settled = false;
                b_row(report, home, format!("{name}: {}", step_line(&step)));
            }
            continue;
        }
        let aside = state
            .join("migrate")
            .join("collided")
            .join(&stamp)
            .join(name);
        match b1_one(home, name, &hot, o.pane, &aside) {
            Ok(B1End::Done(did)) => {
                st.set_step(&key, StepStatus::Done, None);
                if !did.is_empty() {
                    report.changed.push(format!("{what}: {}", did.join("; ")));
                }
            }
            Ok(B1End::Wait(why, did)) => {
                settled = false;
                st.set_step(&key, StepStatus::Pending, Some("waits"));
                if !did.is_empty() {
                    report.changed.push(format!("{what}: {}", did.join("; ")));
                }
                report.pending.push(format!("{what}: {why}"));
            }
            Ok(B1End::Skip(why)) => {
                settled = false;
                st.set_step(&key, StepStatus::Error, Some("skipped"));
                report.errors.push(format!(
                    "{what} {why}; csm leaves it alone, move it by hand"
                ));
            }
            Err(e) => {
                settled = false;
                b_failed(report, st, &key, &what, &anyhow::Error::from(e));
            }
        }
    }

    // B2.
    let copts = ConfigOpts {
        dry_run: o.dry_run,
        wait: if o.pane { B2_PANE_WAIT } else { B2_WAIT },
        switch_wait: if o.pane { Duration::ZERO } else { B2_WAIT },
        i3: o.i3,
    };
    match config_step(home, state, legacy, copts) {
        Ok(Some(line)) if o.dry_run => {
            settled = false;
            b_row(report, home, line);
        }
        Ok(Some(line)) => {
            st.set_step("B2", StepStatus::Done, None);
            report.changed.push(line);
        }
        Ok(None) => {
            if !o.dry_run {
                st.set_step("B2", StepStatus::Done, None);
            }
        }
        Err(e) => {
            settled = false;
            b_failed(report, st, "B2", "~/.claude.json", &e);
        }
    }
    if o.pane {
        return CarryEnd { settled: false };
    }

    // B3.
    let floor_dir = legacy
        .floor
        .as_ref()
        .and_then(|n| legacy.profiles.iter().find(|p| &p.name == n))
        .map(|p| p.dir.clone());
    if let Some(floor) = &floor_dir {
        match copy_missing(floor, home, o.dry_run) {
            Ok(lines) if o.dry_run => {
                for l in lines {
                    b_row(report, home, l);
                }
            }
            Ok(lines) => {
                st.set_step("B3", StepStatus::Done, None);
                report.changed.extend(lines);
            }
            Err(e) => {
                settled = false;
                b_failed(
                    report,
                    st,
                    "B3",
                    "copying the floor profile's files",
                    &e.into(),
                );
            }
        }
    }

    // B4.
    let old = legacy_smart_dir(home);
    if o.dry_run {
        if let Some((carry, _)) = smart_preview(&old)
            && carry > 0
        {
            b_row(
                report,
                home,
                format!("{carry} session file(s) from {}", old.display()),
            );
        }
    } else {
        match apply_smart(&old, state) {
            Ok(line) => {
                st.set_step("B4", StepStatus::Done, None);
                report.changed.extend(line);
            }
            Err(e) => {
                settled = false;
                b_failed(report, st, "B4", "csm's old state dir", &e.into());
            }
        }
    }

    // B5, once ~/.claude/plugins is real, on every run until the dirs
    // retire (see [`apply_plugin_paths`]); the dangling paths are reported
    // on the first pass only.
    let plugins_real = std::fs::symlink_metadata(home.join(".claude").join("plugins"))
        .is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink());
    let b5_done = st
        .steps
        .get("B5")
        .is_some_and(|s| s.status == StepStatus::Done);
    if o.dry_run {
        let n = plugin_paths_preview(home, &dirs);
        if n > 0 {
            b_row(
                report,
                home,
                format!("{n} plugin path(s) to point at ~/.claude/plugins"),
            );
        }
    } else if plugins_real {
        match apply_plugin_paths(home, &dirs, state, !b5_done) {
            Ok(lines) => {
                st.set_step("B5", StepStatus::Done, None);
                report.changed.extend(lines);
            }
            Err(e) => {
                settled = false;
                b_failed(report, st, "B5", "the plugin registries", &e.into());
            }
        }
    }
    CarryEnd { settled }
}

// ─── the merge record ─────────────────────────────────────────────────────────

/// `<state>/migrate/config-merged`: the digest of the floor profile's
/// `.claude.json` that B2 last merged into `~/.claude.json`.
pub(crate) fn merge_marker(state: &Path) -> PathBuf {
    state.join("migrate").join("config-merged")
}

/// The digest [`merge_marker`] holds. Pure.
pub(crate) fn config_digest(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    keychain::hex_lower(&Sha256::digest(bytes))
}

/// Record that B2 merged `floor` (the floor config's bytes).
/// [`record_merge`], skipped when the marker already holds this digest.
fn record_merge_if_new(state: &Path, floor: &[u8]) -> io::Result<()> {
    let have = crate::orca::read_capped(&merge_marker(state), 1024)
        .ok()
        .flatten();
    if have.as_deref().map(str::trim) == Some(config_digest(floor).as_str()) {
        return Ok(());
    }
    record_merge(state, floor)
}

pub(crate) fn record_merge(state: &Path, floor: &[u8]) -> io::Result<()> {
    let p = merge_marker(state);
    if let Some(dir) = p.parent() {
        fsx::create_dir_all(dir, 0o700)?;
    }
    fsx::write_atomic(&p, config_digest(floor).as_bytes(), fsx::WriteOpts::PRIVATE)
}
