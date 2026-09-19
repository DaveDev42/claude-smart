//! The `~/.claude` compatibility shim.
//!
//! csm gives every profile its own Claude Code config home (`~/.claude.<name>`)
//! and points `CLAUDE_CONFIG_DIR` at it. Claude Code follows that variable, and
//! so do csm's own readers. Much of the surrounding tooling does not: GUI
//! session browsers and transcript indexers resolve `~/.claude/projects` by
//! hand and scan nothing else, and other tools write their own hooks into
//! `~/.claude/settings.json`. csm's redirection is what emptied
//! `~/.claude/projects`, so those tools end up looking at a directory nothing
//! writes to.
//!
//! The answer is to keep the hardcoded path working. `~/.claude` stays a real
//! directory whose `projects` entry is a symlink to `~/.claude.shared/projects`
//! — the same transcript SSOT every profile's own `projects` links to (see
//! [`crate::provision`]). Every profile's sessions are then visible through the
//! default home too, and a tool that keeps its settings or hooks there is left
//! alone.
//!
//! What this module will not do: it never creates, renames, or deletes any
//! entry of `~/.claude` other than `projects`; it never reads, moves, or
//! rewrites the credential files a login there would leave behind; it never
//! deletes a transcript. Repairing a real `projects` directory MERGES it into
//! the shared one entry by entry, because a backup copy would hide that history
//! from a plain `claude --resume`.
//!
//! That merge is all-or-nothing. It is planned in full first, and if any name
//! is already taken in the shared store the plan is abandoned: nothing moves,
//! `~/.claude/projects` is left exactly as found, and the colliding names are
//! reported. Moving what can move and leaving the rest would take sessions out
//! of `~/.claude/projects` without putting a link in its place, so a plain
//! `claude --resume` — and every tool that hardcodes that path — would see
//! fewer sessions after the repair than before it.
//!
//! One window the merge cannot close: a rename into a name the plan found free
//! replaces whatever appeared there in the meantime, because POSIX `rename`
//! replaces silently. Only a live `claude` writing a transcript under the same
//! project name at that moment can do it, so run `--fix-home` when no session
//! is running.
//!
//! Layering (pure core + thin I/O shell):
//! - [`diagnose_home_claude_dir_at`] — one `lstat` on an injected path; never
//!   follows symlinks.
//! - [`classify`] — pure policy over that state, the `projects` link state, and
//!   the registry.
//! - [`inspect_at`] / [`ensure_home_shim_at`] — the seams, on injected paths.
//! - [`inspect`] / [`ensure_home_shim`] / [`ensure_home_shim_soft`] — the
//!   production shell binding those to the real home and the registry.
//!
//! Platform: the `projects` entry is a symlink, so the acting half is unix-only.
//! On Windows directory linking is delegated to OS-native tooling exactly as in
//! [`crate::provision`], and the hot-path entry point compiles to a no-op.
#![cfg_attr(not(unix), allow(dead_code))]

use std::io;
use std::path::{Path, PathBuf};

use crate::provision;

#[cfg(unix)]
use crate::account::ProfileMap;
#[cfg(unix)]
use crate::paths;

/// Env var that turns off the launch-time create-only step. Any non-empty value
/// disables it. `csm profiles doctor` ignores it: an explicit diagnosis always
/// reports, and `--fix-home` always acts.
pub const NO_SHIM_ENV: &str = "CSM_NO_HOME_SHIM";

/// Files whose presence directly inside a real `~/.claude` means someone logged
/// in under the default home. Advisory only — this module never reads, moves,
/// or rewrites them.
const IDENTITY_FILES: [&str; 2] = [".credentials.json", ".claude.json"];

// ─── diagnosis (read-only) ────────────────────────────────────────────────────

/// What sits at `~/.claude`, as seen by `lstat` (a symlink is reported as a
/// symlink, never resolved).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HomeClaudeState {
    /// Nothing there.
    Absent,
    /// A symlink (target as written; empty when unreadable).
    Symlink(PathBuf),
    /// A real directory — the shape the shim wants.
    RealDir,
    /// A regular file or anything else that is neither dir nor symlink.
    Other,
}

/// Probe `path` with a single `lstat`. Never follows the link: a dangling
/// symlink reads as [`HomeClaudeState::Symlink`], not as `Absent`.
pub fn diagnose_home_claude_dir_at(path: &Path) -> HomeClaudeState {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return HomeClaudeState::Absent,
        Err(_) => return HomeClaudeState::Other,
    };
    if is_symlink_like(&meta) {
        return HomeClaudeState::Symlink(std::fs::read_link(path).unwrap_or_default());
    }
    if meta.is_dir() {
        HomeClaudeState::RealDir
    } else {
        HomeClaudeState::Other
    }
}

/// A symlink on every platform; on Windows also a junction/reparse point, which
/// `file_type().is_symlink()` does not report.
fn is_symlink_like(meta: &std::fs::Metadata) -> bool {
    if meta.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return true;
        }
    }
    false
}

/// Which of [`IDENTITY_FILES`] sit directly inside `dir`, in listing order.
/// `lstat` only, so a symlinked credential file counts; the content is never
/// opened.
fn identity_files_in(dir: &Path) -> Vec<String> {
    IDENTITY_FILES
        .iter()
        .filter(|name| matches!(std::fs::symlink_metadata(dir.join(name)), Ok(m) if !m.is_dir()))
        .map(|n| (*n).to_owned())
        .collect()
}

// ─── policy (pure) ────────────────────────────────────────────────────────────

/// The verdict on `~/.claude` as a compatibility shim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HomeShimVerdict {
    /// Real dir, `projects` links to the shared transcript SSOT — the shim works.
    Ok,
    /// `projects` links to the shared transcript SSOT, but that directory is
    /// gone, so the link dangles: reading `~/.claude/projects` fails outright
    /// and Claude Code cannot create a transcript there either.
    SharedMissing,
    /// Nothing at `~/.claude`: a tool that hardcodes the path finds no sessions.
    Absent,
    /// Real dir, no `projects` entry — one symlink away from working.
    ProjectsMissing,
    /// `projects` is a real directory, so transcripts written through the
    /// default home sit outside the shared SSOT.
    ProjectsRealDir,
    /// `projects` is a symlink, but to this target instead of the shared SSOT.
    ProjectsWrongLink(PathBuf),
    /// `projects` is a regular file (or other non-dir).
    ProjectsNotADir,
    /// `~/.claude` is itself a symlink to this target. Someone aliased the
    /// default home deliberately; csm leaves it as found.
    LinkedElsewhere(PathBuf),
    /// `~/.claude` IS a registered profile's config dir, so ordinary profile
    /// provisioning already owns its links.
    Registered { name: String },
    /// `~/.claude` is neither a directory nor a symlink, or the platform does
    /// not manage the link at all.
    Unsupported,
}

/// One diagnosis of `~/.claude`: what is there, what its `projects` entry is,
/// which identity files sit beside it, and what to make of all that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomeShimFinding {
    pub path: PathBuf,
    pub state: HomeClaudeState,
    /// State of the `projects` entry, when `~/.claude` is a real directory on a
    /// platform where csm manages the link.
    pub projects: Option<provision::LinkState>,
    /// Which of [`IDENTITY_FILES`] are present. Advisory: csm reports them and
    /// never acts on them.
    pub identity_files: Vec<String>,
    pub verdict: HomeShimVerdict,
}

/// Pure policy over an already-probed `~/.claude`.
///
/// A registered profile dir at `path` wins over everything the `projects` entry
/// says: normal profile provisioning already maintains that dir's links, and
/// two owners for one path is how a repair loop starts.
///
/// There is deliberately no special case for an empty registry. The shim rules
/// are safe with zero profiles — the launch-time path only ever creates a
/// missing link, and a real `projects` directory is merged only on an explicit
/// `--fix-home`.
///
/// `shared_present` says whether the shared transcript dir is really there. A
/// link is only healthy if its target is: with the SSOT deleted or its volume
/// unmounted, the link state still reads as `Ok` (both sides compare equal
/// lexically once neither canonicalizes), and calling that a working shim would
/// leave `~/.claude/projects` dangling with nothing reporting it.
pub fn classify(
    path: &Path,
    state: &HomeClaudeState,
    projects: Option<&provision::LinkState>,
    shared_present: bool,
    registered: &[(String, PathBuf)],
) -> HomeShimVerdict {
    match state {
        HomeClaudeState::Absent => HomeShimVerdict::Absent,
        HomeClaudeState::Symlink(target) => HomeShimVerdict::LinkedElsewhere(target.clone()),
        HomeClaudeState::Other => HomeShimVerdict::Unsupported,
        HomeClaudeState::RealDir => {
            if let Some((name, _)) = registered.iter().find(|(_, d)| same_location(d, path)) {
                return HomeShimVerdict::Registered { name: name.clone() };
            }
            match projects {
                Some(provision::LinkState::Ok) if shared_present => HomeShimVerdict::Ok,
                Some(provision::LinkState::Ok) => HomeShimVerdict::SharedMissing,
                Some(provision::LinkState::Missing) => HomeShimVerdict::ProjectsMissing,
                Some(provision::LinkState::RealDir) => HomeShimVerdict::ProjectsRealDir,
                Some(provision::LinkState::WrongLink(t)) => {
                    HomeShimVerdict::ProjectsWrongLink(t.clone())
                }
                Some(provision::LinkState::NotADir) => HomeShimVerdict::ProjectsNotADir,
                // `classify_link` never reports this (only profile diagnosis
                // probes the target), but it means exactly this verdict.
                Some(provision::LinkState::Dangling) => HomeShimVerdict::SharedMissing,
                // No link state to judge: this platform does not manage it.
                None => HomeShimVerdict::Unsupported,
            }
        }
    }
}

/// Canonicalize-else-lexical path equality (mirrors `provision::links_match`): a
/// registry entry written with a trailing slash or through a symlinked home
/// still matches; a path that cannot be canonicalized compares lexically.
fn same_location(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
}

// ─── seams (read-only) ────────────────────────────────────────────────────────

/// Probe `home_claude` against `shared_projects` and the registered profile
/// dirs. Read-only: three `lstat`s at most, no enumeration.
#[cfg(unix)]
pub fn inspect_at(
    home_claude: &Path,
    shared_projects: &Path,
    registered: &[(String, PathBuf)],
) -> HomeShimFinding {
    let state = diagnose_home_claude_dir_at(home_claude);
    let (projects, identity_files) = match state {
        HomeClaudeState::RealDir => (
            Some(provision::classify_link(
                &home_claude.join("projects"),
                shared_projects,
            )),
            identity_files_in(home_claude),
        ),
        _ => (None, Vec::new()),
    };
    // Follows the link on purpose: the question is whether the SSOT is really
    // there, not whether its path is spelled as one.
    let shared_present = shared_projects.is_dir();
    let verdict = classify(
        home_claude,
        &state,
        projects.as_ref(),
        shared_present,
        registered,
    );
    HomeShimFinding {
        path: home_claude.to_path_buf(),
        state,
        projects,
        identity_files,
        verdict,
    }
}

/// Production diagnosis of the real `~/.claude` against `profiles`. Read-only.
#[cfg(unix)]
pub fn inspect(profiles: &ProfileMap) -> HomeShimFinding {
    inspect_at(
        &paths::home_claude_dir(),
        &paths::session_base_dir(),
        &registered_dirs(profiles),
    )
}

#[cfg(unix)]
fn registered_dirs(profiles: &ProfileMap) -> Vec<(String, PathBuf)> {
    profiles
        .iter()
        .map(|(n, d)| (n.to_owned(), PathBuf::from(d)))
        .collect()
}

// ─── the fix ──────────────────────────────────────────────────────────────────

/// How much [`ensure_home_shim_at`] is allowed to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShimMode {
    /// The launch hot path: create what is missing, touch nothing that exists.
    CreateOnly,
    /// `csm profiles doctor --fix-home`: also repoint a wrong link, back up a
    /// file, and merge a real `projects` directory into the shared one.
    Repair,
}

/// What a merge moved, and what blocked it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MergeReport {
    pub moved_dirs: usize,
    pub moved_files: usize,
    /// Names that already exist in the shared dir. A non-empty list means the
    /// merge was refused whole, so these are not leftovers of a half-done move:
    /// every entry of `projects` is still where it was.
    pub left_behind: Vec<PathBuf>,
}

/// What [`ensure_home_shim_at`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShimOutcome {
    /// `projects` now links to the shared SSOT.
    Linked(provision::LinkOutcome),
    /// A real `projects` dir was drained into the shared one and replaced by the
    /// link.
    Merged(MergeReport),
    /// The merge was refused: names listed in [`MergeReport::left_behind`]
    /// already exist in the shared store, so nothing moved, `projects` is still
    /// the real dir it was, and no link was made.
    PartiallyMerged(MergeReport),
    /// Left as found, for this reason.
    Skipped(HomeShimVerdict),
}

/// Ensure `home_claude/projects` links to `shared_projects`, to the depth `mode`
/// allows.
///
/// `CreateOnly` acts only when there is nothing to lose: `~/.claude` absent (the
/// dir and the link are created) or present without a `projects` entry (the link
/// is created). Everything else is reported as [`ShimOutcome::Skipped`].
///
/// Both modes also recreate the shared transcript dir when the `projects` link
/// points at one that is gone. Nothing there can be lost — the directory is
/// missing — and every other arm creates it too, so leaving a dangling link in
/// place would be the odd case out.
///
/// `Repair` adds the three destructive-looking cases, none of which loses data:
/// a wrong link is repointed, a file named `projects` is backed up as `.bak`,
/// and a real `projects` directory is MERGED into the shared one (see
/// [`merge_into_shared`]) and only then replaced by the link. A merge blocked by
/// colliding names moves nothing and makes no link.
///
/// Nothing in `~/.claude` other than `projects` is created, renamed, or removed
/// in either mode.
#[cfg(unix)]
pub fn ensure_home_shim_at(
    home_claude: &Path,
    shared_projects: &Path,
    registered: &[(String, PathBuf)],
    mode: ShimMode,
) -> io::Result<ShimOutcome> {
    let finding = inspect_at(home_claude, shared_projects, registered);
    let link = home_claude.join("projects");
    match &finding.verdict {
        // `link_dir_to_shared` creates the link's parent, so the absent case
        // needs no separate mkdir of `~/.claude` itself.
        HomeShimVerdict::Absent | HomeShimVerdict::ProjectsMissing => {
            link_projects(&link, shared_projects, home_claude, registered)
        }
        // The link is right and only its target is gone: put the target back.
        HomeShimVerdict::SharedMissing => {
            std::fs::create_dir_all(shared_projects)?;
            Ok(ShimOutcome::Linked(provision::link_dir_to_shared(
                &link,
                shared_projects,
            )?))
        }
        HomeShimVerdict::ProjectsWrongLink(_) | HomeShimVerdict::ProjectsNotADir
            if mode == ShimMode::Repair =>
        {
            Ok(ShimOutcome::Linked(provision::link_dir_to_shared(
                &link,
                shared_projects,
            )?))
        }
        HomeShimVerdict::ProjectsRealDir if mode == ShimMode::Repair => {
            match merge_into_shared(&link, shared_projects)? {
                MergeAttempt::Blocked(report) => Ok(ShimOutcome::PartiallyMerged(report)),
                MergeAttempt::Drained(report) => {
                    if std::fs::read_dir(&link)?.next().is_none() {
                        std::fs::remove_dir(&link)?;
                        provision::link_dir_to_shared(&link, shared_projects)?;
                        Ok(ShimOutcome::Merged(report))
                    } else {
                        // Someone wrote into the directory while we drained it.
                        Ok(ShimOutcome::PartiallyMerged(report))
                    }
                }
            }
        }
        other => Ok(ShimOutcome::Skipped(other.clone())),
    }
}

/// Create the `projects` link, tolerating a concurrent csm that won the race
/// between our `lstat` and our `symlink`: re-inspect, and if the link the other
/// process made is the one we wanted, report it as already linked.
#[cfg(unix)]
fn link_projects(
    link: &Path,
    shared_projects: &Path,
    home_claude: &Path,
    registered: &[(String, PathBuf)],
) -> io::Result<ShimOutcome> {
    match provision::link_dir_to_shared(link, shared_projects) {
        Ok(outcome) => Ok(ShimOutcome::Linked(outcome)),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            let again = inspect_at(home_claude, shared_projects, registered);
            if again.verdict == HomeShimVerdict::Ok {
                Ok(ShimOutcome::Linked(provision::LinkOutcome::AlreadyLinked))
            } else {
                Err(e)
            }
        }
        Err(e) => Err(e),
    }
}

/// The outcome of an attempted merge.
#[cfg(unix)]
enum MergeAttempt {
    /// Every entry of `src` moved into `shared`.
    Drained(MergeReport),
    /// Names collide, so nothing was moved at all.
    Blocked(MergeReport),
}

/// One rename the merge intends to perform.
#[cfg(unix)]
struct PlannedMove {
    from: PathBuf,
    to: PathBuf,
    is_dir: bool,
}

/// Everything a merge would do, worked out before it does any of it.
#[cfg(unix)]
#[derive(Default)]
struct MergePlan {
    moves: Vec<PlannedMove>,
    /// Subdirectories of `src` whose contents are moved out one by one; each is
    /// removed once it is empty so its parent can drain.
    drain_dirs: Vec<PathBuf>,
    /// Names already taken in `shared`. Any of these abandons the whole plan.
    conflicts: Vec<PathBuf>,
}

/// Move every entry of `src` into `shared`, or move nothing.
///
/// The plan comes first ([`plan_merge`]): an entry whose name is free in
/// `shared` is renamed across whole, and two plain directories of the same name
/// are merged one level deeper, again by rename. Any other collision blocks the
/// merge, and a blocked merge performs no rename at all —
/// [`MergeAttempt::Blocked`] means `src` is byte-for-byte as it was found.
///
/// That refusal is the point. A partial move would take the non-colliding
/// sessions out of `~/.claude/projects` while the directory stays a real,
/// unlinked directory, so the default home would end up showing fewer sessions
/// than before the repair.
///
/// Nothing is copied and nothing is deleted, so a cross-device `rename` fails
/// loudly rather than silently half-copying. An error mid-execution names the
/// entry it failed on and reports how much had already moved, since by then the
/// move is genuinely half done.
#[cfg(unix)]
fn merge_into_shared(src: &Path, shared: &Path) -> io::Result<MergeAttempt> {
    let plan = plan_merge(src, shared)?;
    if !plan.conflicts.is_empty() {
        return Ok(MergeAttempt::Blocked(MergeReport {
            left_behind: plan.conflicts,
            ..MergeReport::default()
        }));
    }
    let mut report = MergeReport::default();
    match execute_merge(&plan, shared, &mut report) {
        Ok(()) => Ok(MergeAttempt::Drained(report)),
        Err(e) => Err(with_progress(e, &report)),
    }
}

/// Walk `src` against `shared` and record what would move and what collides.
/// Read-only: `lstat` and `read_dir` and nothing else.
#[cfg(unix)]
fn plan_merge(src: &Path, shared: &Path) -> io::Result<MergePlan> {
    let mut plan = MergePlan::default();
    for name in entry_names(src)? {
        let from = src.join(&name);
        let to = shared.join(&name);
        match std::fs::symlink_metadata(&to) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let meta = std::fs::symlink_metadata(&from)?;
                plan.moves.push(PlannedMove {
                    from,
                    to,
                    is_dir: is_plain_dir(&meta),
                });
            }
            Ok(to_meta) => {
                let from_meta = std::fs::symlink_metadata(&from)?;
                if is_plain_dir(&from_meta) && is_plain_dir(&to_meta) {
                    plan_dir_entries(&from, &to, &mut plan)?;
                    plan.drain_dirs.push(from);
                } else {
                    plan.conflicts.push(from);
                }
            }
            Err(e) => return Err(e),
        }
    }
    Ok(plan)
}

/// One level deeper: each entry of `from` whose name is free in `to` is planned
/// as a move, and the rest are collisions.
#[cfg(unix)]
fn plan_dir_entries(from: &Path, to: &Path, plan: &mut MergePlan) -> io::Result<()> {
    for name in entry_names(from)? {
        let src = from.join(&name);
        let dst = to.join(&name);
        match std::fs::symlink_metadata(&dst) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let meta = std::fs::symlink_metadata(&src)?;
                plan.moves.push(PlannedMove {
                    from: src,
                    to: dst,
                    is_dir: is_plain_dir(&meta),
                });
            }
            Ok(_) => plan.conflicts.push(src),
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Run a conflict-free plan, counting as it goes so a failure can say how far it
/// got.
#[cfg(unix)]
fn execute_merge(plan: &MergePlan, shared: &Path, report: &mut MergeReport) -> io::Result<()> {
    std::fs::create_dir_all(shared)?;
    for mv in &plan.moves {
        std::fs::rename(&mv.from, &mv.to).map_err(|e| naming(e, &mv.from, &mv.to))?;
        if mv.is_dir {
            report.moved_dirs += 1;
        } else {
            report.moved_files += 1;
        }
    }
    for dir in &plan.drain_dirs {
        if std::fs::read_dir(dir)?.next().is_none() {
            std::fs::remove_dir(dir)?;
        }
    }
    Ok(())
}

/// Restate an io error with the two paths involved. `std::fs::rename`'s own
/// error carries neither, so a bare "Permission denied" would leave the user
/// with nothing to look at.
#[cfg(unix)]
fn naming(e: io::Error, from: &Path, to: &Path) -> io::Error {
    io::Error::new(
        e.kind(),
        format!(
            "home shim merge: {} -> {}: {e}",
            from.display(),
            to.display()
        ),
    )
}

/// Append what a failed merge had already moved, so the failure message says
/// which transcripts now live in the shared dir.
#[cfg(unix)]
fn with_progress(e: io::Error, report: &MergeReport) -> io::Error {
    io::Error::new(
        e.kind(),
        format!(
            "{e} (after moving {} dir(s) and {} file(s) into the shared transcript dir)",
            report.moved_dirs, report.moved_files
        ),
    )
}

/// Entry names of `dir`, sorted, so a merge behaves the same on every run.
#[cfg(unix)]
fn entry_names(dir: &Path) -> io::Result<Vec<std::ffi::OsString>> {
    let mut names: Vec<std::ffi::OsString> = std::fs::read_dir(dir)?
        .flatten()
        .map(|e| e.file_name())
        .collect();
    names.sort();
    Ok(names)
}

/// A real directory, not a symlink to one.
#[cfg(unix)]
fn is_plain_dir(meta: &std::fs::Metadata) -> bool {
    meta.is_dir() && !meta.file_type().is_symlink()
}

/// [`ensure_home_shim_at`] bound to the real home and the registry.
#[cfg(unix)]
pub fn ensure_home_shim(profiles: &ProfileMap, mode: ShimMode) -> io::Result<ShimOutcome> {
    ensure_home_shim_at(
        &paths::home_claude_dir(),
        &paths::session_base_dir(),
        &registered_dirs(profiles),
        mode,
    )
}

// ─── hot path ─────────────────────────────────────────────────────────────────

/// Maintain the shim on every launch, switch, and register — once per process,
/// create-only, and never fatal.
///
/// Costs one `lstat` when the shim is already healthy. A state the create-only
/// mode will not touch gets one stderr line naming the repair command, and
/// anything else (unreadable registry, io error) is swallowed: a launch must not
/// fail over a compatibility link.
///
/// stderr only: callers include `csm cas --eval`, whose stdout the shell shim
/// `eval`s.
#[cfg(unix)]
pub fn ensure_home_shim_soft() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if std::env::var_os(NO_SHIM_ENV).is_some_and(|v| !v.is_empty()) {
            return;
        }
        let Ok(profiles) = ProfileMap::load() else {
            return;
        };
        let reason = match ensure_home_shim(&profiles, ShimMode::CreateOnly) {
            Ok(ShimOutcome::Skipped(v)) => needs_repair_reason(&v),
            Ok(_) => None,
            Err(e) => Some(format!("could not be linked: {e}")),
        };
        if let Some(reason) = reason {
            eprintln!("csm: ~/.claude/projects {reason}; run csm profiles doctor --fix-home");
        }
    });
}

/// Non-unix: the `projects` entry would be an OS-side junction, provisioned the
/// same way profile dirs are on that platform, so csm does nothing here.
#[cfg(not(unix))]
pub fn ensure_home_shim_soft() {}

/// The short reason for the hot path's one stderr line, or `None` when the
/// skipped verdict is nothing a user needs to act on.
#[cfg(unix)]
fn needs_repair_reason(verdict: &HomeShimVerdict) -> Option<String> {
    match verdict {
        HomeShimVerdict::ProjectsRealDir => {
            Some("is a real directory outside the shared transcript dir".to_owned())
        }
        HomeShimVerdict::ProjectsWrongLink(t) => Some(format!(
            "is linked to {} instead of the shared dir",
            t.display()
        )),
        HomeShimVerdict::ProjectsNotADir => Some("is a file, not a directory".to_owned()),
        _ => None,
    }
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use provision::{LinkOutcome, LinkState};
    use std::fs;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    /// `(home_claude, shared_projects)` under a fresh tempdir. Neither exists yet.
    fn layout(td: &tempfile::TempDir) -> (PathBuf, PathBuf) {
        let home = td.path().join("home");
        fs::create_dir_all(&home).unwrap();
        (
            home.join(".claude"),
            home.join(".claude.shared").join("projects"),
        )
    }

    fn registry(entries: &[(&str, &Path)]) -> Vec<(String, PathBuf)> {
        entries
            .iter()
            .map(|(n, d)| ((*n).to_owned(), d.to_path_buf()))
            .collect()
    }

    fn symlink(target: &Path, link: &Path) {
        std::os::unix::fs::symlink(target, link).unwrap();
    }

    fn write(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, body).unwrap();
    }

    /// A real `~/.claude` with the two identity files and a `settings.json`,
    /// i.e. the shape a login under the default home leaves behind.
    fn real_home_with_identity(home_claude: &Path) {
        fs::create_dir_all(home_claude).unwrap();
        write(&home_claude.join(".credentials.json"), r#"{"token":"x"}"#);
        write(&home_claude.join(".claude.json"), r#"{"userID":"u"}"#);
        write(&home_claude.join("settings.json"), r#"{"hooks":{}}"#);
    }

    /// A stable, recursive rendering of `dir` — path, kind, and payload — so a
    /// test can assert a tree is byte-for-byte what it was.
    fn snapshot(dir: &Path) -> Vec<String> {
        fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
            let Ok(rd) = fs::read_dir(dir) else { return };
            let mut entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
            entries.sort();
            for p in entries {
                let rel = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
                let meta = fs::symlink_metadata(&p).unwrap();
                if meta.file_type().is_symlink() {
                    out.push(format!("{rel} -> {}", fs::read_link(&p).unwrap().display()));
                } else if meta.is_dir() {
                    out.push(format!("{rel}/"));
                    walk(root, &p, out);
                } else {
                    out.push(format!("{rel} = {}", fs::read_to_string(&p).unwrap()));
                }
            }
        }
        let mut out = Vec::new();
        walk(dir, dir, &mut out);
        out
    }

    fn is_symlink_to(link: &Path, target: &Path) -> bool {
        matches!(fs::symlink_metadata(link), Ok(m) if m.file_type().is_symlink())
            && fs::read_link(link).unwrap() == target
    }

    // ── classify (pure) ───────────────────────────────────────────────────────

    #[test]
    fn classify_absent() {
        let td = tmp();
        let (home_claude, _) = layout(&td);
        let v = classify(
            &home_claude,
            &HomeClaudeState::Absent,
            None,
            false,
            &registry(&[("work", &td.path().join("w"))]),
        );
        assert_eq!(v, HomeShimVerdict::Absent);
    }

    #[test]
    fn classify_symlinked_home_is_left_alone() {
        let td = tmp();
        let (home_claude, _) = layout(&td);
        let target = td.path().join("home").join(".claude.work");
        let v = classify(
            &home_claude,
            &HomeClaudeState::Symlink(target.clone()),
            None,
            false,
            &registry(&[]),
        );
        assert_eq!(v, HomeShimVerdict::LinkedElsewhere(target));
    }

    #[test]
    fn classify_non_dir_is_unsupported() {
        let td = tmp();
        let (home_claude, _) = layout(&td);
        assert_eq!(
            classify(
                &home_claude,
                &HomeClaudeState::Other,
                None,
                false,
                &registry(&[])
            ),
            HomeShimVerdict::Unsupported
        );
    }

    #[test]
    fn classify_maps_every_projects_state() {
        let td = tmp();
        let (home_claude, _) = layout(&td);
        let elsewhere = td.path().join("elsewhere");
        let cases = [
            (LinkState::Ok, HomeShimVerdict::Ok),
            (LinkState::Missing, HomeShimVerdict::ProjectsMissing),
            (LinkState::RealDir, HomeShimVerdict::ProjectsRealDir),
            (
                LinkState::WrongLink(elsewhere.clone()),
                HomeShimVerdict::ProjectsWrongLink(elsewhere.clone()),
            ),
            (LinkState::NotADir, HomeShimVerdict::ProjectsNotADir),
            (LinkState::Dangling, HomeShimVerdict::SharedMissing),
        ];
        for (state, expected) in cases {
            assert_eq!(
                classify(
                    &home_claude,
                    &HomeClaudeState::RealDir,
                    Some(&state),
                    true,
                    &registry(&[("work", &td.path().join("w"))]),
                ),
                expected,
                "projects state {state:?}"
            );
        }
    }

    #[test]
    fn classify_ok_without_the_shared_dir_is_shared_missing() {
        let td = tmp();
        let (home_claude, _) = layout(&td);
        assert_eq!(
            classify(
                &home_claude,
                &HomeClaudeState::RealDir,
                Some(&LinkState::Ok),
                false,
                &registry(&[]),
            ),
            HomeShimVerdict::SharedMissing,
            "a link whose target is gone is not a working shim"
        );
    }

    #[test]
    fn classify_registered_wins_over_a_bad_projects_link() {
        let td = tmp();
        let (home_claude, _) = layout(&td);
        fs::create_dir_all(&home_claude).unwrap();
        let v = classify(
            &home_claude,
            &HomeClaudeState::RealDir,
            Some(&LinkState::RealDir),
            true,
            &registry(&[("home", &home_claude)]),
        );
        assert_eq!(
            v,
            HomeShimVerdict::Registered {
                name: "home".to_owned()
            },
            "profile provisioning owns a registered dir, shim rules must stand down"
        );
    }

    #[test]
    fn classify_empty_registry_is_not_special_cased() {
        let td = tmp();
        let (home_claude, _) = layout(&td);
        assert_eq!(
            classify(
                &home_claude,
                &HomeClaudeState::RealDir,
                Some(&LinkState::Missing),
                true,
                &registry(&[]),
            ),
            HomeShimVerdict::ProjectsMissing,
            "with no profiles the shim rules still apply — they never move a real dir at launch"
        );
    }

    #[test]
    fn classify_without_a_link_state_is_unsupported() {
        let td = tmp();
        let (home_claude, _) = layout(&td);
        assert_eq!(
            classify(
                &home_claude,
                &HomeClaudeState::RealDir,
                None,
                true,
                &registry(&[])
            ),
            HomeShimVerdict::Unsupported,
            "a platform that does not manage the link has nothing to judge"
        );
    }

    // ── inspect_at (on-disk layouts) ──────────────────────────────────────────

    #[test]
    fn inspect_at_reports_absent() {
        let td = tmp();
        let (home_claude, shared) = layout(&td);
        let f = inspect_at(&home_claude, &shared, &registry(&[]));
        assert_eq!(f.state, HomeClaudeState::Absent);
        assert_eq!(f.projects, None);
        assert!(f.identity_files.is_empty());
        assert_eq!(f.verdict, HomeShimVerdict::Absent);
        assert_eq!(f.path, home_claude);
    }

    #[test]
    fn inspect_at_reports_a_healthy_shim() {
        let td = tmp();
        let (home_claude, shared) = layout(&td);
        fs::create_dir_all(&shared).unwrap();
        fs::create_dir_all(&home_claude).unwrap();
        symlink(&shared, &home_claude.join("projects"));
        let f = inspect_at(&home_claude, &shared, &registry(&[]));
        assert_eq!(f.state, HomeClaudeState::RealDir);
        assert_eq!(f.projects, Some(LinkState::Ok));
        assert_eq!(f.verdict, HomeShimVerdict::Ok);
    }

    #[test]
    fn inspect_at_reports_identity_files() {
        let td = tmp();
        let (home_claude, shared) = layout(&td);
        real_home_with_identity(&home_claude);
        let f = inspect_at(&home_claude, &shared, &registry(&[]));
        assert_eq!(f.verdict, HomeShimVerdict::ProjectsMissing);
        assert_eq!(
            f.identity_files,
            vec![".credentials.json".to_owned(), ".claude.json".to_owned()],
            "settings.json is not an identity file"
        );
    }

    #[test]
    fn inspect_at_reports_a_real_projects_dir() {
        let td = tmp();
        let (home_claude, shared) = layout(&td);
        fs::create_dir_all(home_claude.join("projects")).unwrap();
        let f = inspect_at(&home_claude, &shared, &registry(&[]));
        assert_eq!(f.projects, Some(LinkState::RealDir));
        assert_eq!(f.verdict, HomeShimVerdict::ProjectsRealDir);
        assert!(f.identity_files.is_empty());
    }

    #[test]
    fn inspect_at_reports_a_wrong_link_and_a_file() {
        let td = tmp();
        let (home_claude, shared) = layout(&td);
        let elsewhere = td.path().join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        fs::create_dir_all(&home_claude).unwrap();
        symlink(&elsewhere, &home_claude.join("projects"));
        let f = inspect_at(&home_claude, &shared, &registry(&[]));
        assert_eq!(
            f.verdict,
            HomeShimVerdict::ProjectsWrongLink(elsewhere.clone())
        );

        fs::remove_file(home_claude.join("projects")).unwrap();
        write(&home_claude.join("projects"), "not a dir");
        let f = inspect_at(&home_claude, &shared, &registry(&[]));
        assert_eq!(f.verdict, HomeShimVerdict::ProjectsNotADir);
    }

    #[test]
    fn inspect_at_reports_a_symlinked_home_and_a_registered_one() {
        let td = tmp();
        let (home_claude, shared) = layout(&td);
        let target = td.path().join("home").join(".claude.work");
        fs::create_dir_all(&target).unwrap();
        symlink(&target, &home_claude);
        let f = inspect_at(&home_claude, &shared, &registry(&[]));
        assert_eq!(f.state, HomeClaudeState::Symlink(target.clone()));
        assert_eq!(f.verdict, HomeShimVerdict::LinkedElsewhere(target));

        let td2 = tmp();
        let (home_claude, shared) = layout(&td2);
        real_home_with_identity(&home_claude);
        let f = inspect_at(&home_claude, &shared, &registry(&[("home", &home_claude)]));
        assert_eq!(
            f.verdict,
            HomeShimVerdict::Registered {
                name: "home".to_owned()
            }
        );
    }

    #[test]
    fn inspect_at_reports_a_projects_link_whose_target_is_gone() {
        let td = tmp();
        let (home_claude, shared) = layout(&td);
        fs::create_dir_all(&shared).unwrap();
        fs::create_dir_all(&home_claude).unwrap();
        symlink(&shared, &home_claude.join("projects"));
        fs::remove_dir(&shared).unwrap();

        let f = inspect_at(&home_claude, &shared, &registry(&[]));
        assert_eq!(
            f.projects,
            Some(LinkState::Ok),
            "the link itself still names the right target"
        );
        assert_eq!(
            f.verdict,
            HomeShimVerdict::SharedMissing,
            "but the target is gone, so the shim does not work"
        );
    }

    #[test]
    fn inspect_at_never_follows_a_dangling_home_link() {
        let td = tmp();
        let (home_claude, shared) = layout(&td);
        let gone = td.path().join("home").join("gone");
        symlink(&gone, &home_claude);
        let f = inspect_at(&home_claude, &shared, &registry(&[]));
        assert_eq!(
            f.state,
            HomeClaudeState::Symlink(gone),
            "lstat, not stat: a dangling link must not read as Absent"
        );
    }

    // ── CreateOnly ────────────────────────────────────────────────────────────

    #[test]
    fn create_only_creates_home_and_link_when_absent() {
        let td = tmp();
        let (home_claude, shared) = layout(&td);
        let out = ensure_home_shim_at(&home_claude, &shared, &registry(&[]), ShimMode::CreateOnly)
            .unwrap();
        assert_eq!(out, ShimOutcome::Linked(LinkOutcome::Created));
        assert!(home_claude.is_dir(), "the default home is created");
        assert!(is_symlink_to(&home_claude.join("projects"), &shared));
        assert!(shared.is_dir(), "the shared SSOT exists (empty)");
    }

    #[test]
    fn create_only_links_a_missing_projects_entry() {
        let td = tmp();
        let (home_claude, shared) = layout(&td);
        real_home_with_identity(&home_claude);
        let before = snapshot(&home_claude);
        let out = ensure_home_shim_at(&home_claude, &shared, &registry(&[]), ShimMode::CreateOnly)
            .unwrap();
        assert_eq!(out, ShimOutcome::Linked(LinkOutcome::Created));
        assert!(is_symlink_to(&home_claude.join("projects"), &shared));
        // Everything that was already there is still exactly there.
        let after = snapshot(&home_claude);
        for line in before {
            assert!(after.contains(&line), "{line} must survive untouched");
        }
    }

    #[test]
    fn create_only_leaves_a_real_projects_dir_untouched() {
        let td = tmp();
        let (home_claude, shared) = layout(&td);
        real_home_with_identity(&home_claude);
        write(
            &home_claude.join("projects").join("-w-p").join("a.jsonl"),
            "a",
        );
        let before = snapshot(&home_claude);

        let out = ensure_home_shim_at(&home_claude, &shared, &registry(&[]), ShimMode::CreateOnly)
            .unwrap();
        assert_eq!(out, ShimOutcome::Skipped(HomeShimVerdict::ProjectsRealDir));
        assert_eq!(snapshot(&home_claude), before, "tree must be untouched");
    }

    #[test]
    fn create_only_leaves_a_wrong_link_and_a_file_untouched() {
        let td = tmp();
        let (home_claude, shared) = layout(&td);
        let elsewhere = td.path().join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        fs::create_dir_all(&home_claude).unwrap();
        symlink(&elsewhere, &home_claude.join("projects"));
        let before = snapshot(&home_claude);
        let out = ensure_home_shim_at(&home_claude, &shared, &registry(&[]), ShimMode::CreateOnly)
            .unwrap();
        assert_eq!(
            out,
            ShimOutcome::Skipped(HomeShimVerdict::ProjectsWrongLink(elsewhere))
        );
        assert_eq!(snapshot(&home_claude), before);

        let td2 = tmp();
        let (home_claude, shared) = layout(&td2);
        fs::create_dir_all(&home_claude).unwrap();
        write(&home_claude.join("projects"), "not a dir");
        let before = snapshot(&home_claude);
        let out = ensure_home_shim_at(&home_claude, &shared, &registry(&[]), ShimMode::CreateOnly)
            .unwrap();
        assert_eq!(out, ShimOutcome::Skipped(HomeShimVerdict::ProjectsNotADir));
        assert_eq!(snapshot(&home_claude), before);
        assert!(!shared.exists(), "a skip creates no shared dir");
    }

    #[test]
    fn create_only_leaves_a_symlinked_or_registered_home_untouched() {
        let td = tmp();
        let (home_claude, shared) = layout(&td);
        let target = td.path().join("home").join(".claude.work");
        fs::create_dir_all(&target).unwrap();
        symlink(&target, &home_claude);
        let out = ensure_home_shim_at(&home_claude, &shared, &registry(&[]), ShimMode::CreateOnly)
            .unwrap();
        assert_eq!(
            out,
            ShimOutcome::Skipped(HomeShimVerdict::LinkedElsewhere(target.clone()))
        );
        assert!(is_symlink_to(&home_claude, &target), "link untouched");
        assert!(snapshot(&target).is_empty(), "nothing written through it");

        let td2 = tmp();
        let (home_claude, shared) = layout(&td2);
        real_home_with_identity(&home_claude);
        let before = snapshot(&home_claude);
        let out = ensure_home_shim_at(
            &home_claude,
            &shared,
            &registry(&[("home", &home_claude)]),
            ShimMode::CreateOnly,
        )
        .unwrap();
        assert_eq!(
            out,
            ShimOutcome::Skipped(HomeShimVerdict::Registered {
                name: "home".to_owned()
            })
        );
        assert_eq!(snapshot(&home_claude), before);
    }

    /// The SSOT was deleted (or its volume went away) under a healthy shim. Both
    /// modes put it back: there is nothing there to lose, and every other arm
    /// creates it too.
    #[test]
    fn both_modes_recreate_a_missing_shared_dir() {
        for mode in [ShimMode::CreateOnly, ShimMode::Repair] {
            let td = tmp();
            let (home_claude, shared) = layout(&td);
            fs::create_dir_all(&shared).unwrap();
            fs::create_dir_all(&home_claude).unwrap();
            symlink(&shared, &home_claude.join("projects"));
            fs::remove_dir(&shared).unwrap();
            assert!(
                !home_claude.join("projects").is_dir(),
                "{mode:?}: dangling to start with"
            );

            let out = ensure_home_shim_at(&home_claude, &shared, &registry(&[]), mode).unwrap();
            assert_eq!(
                out,
                ShimOutcome::Linked(LinkOutcome::AlreadyLinked),
                "{mode:?}"
            );
            assert!(shared.is_dir(), "{mode:?}: the SSOT is back");
            assert!(
                home_claude.join("projects").is_dir(),
                "{mode:?}: and the shim resolves again"
            );
            assert_eq!(
                inspect_at(&home_claude, &shared, &registry(&[])).verdict,
                HomeShimVerdict::Ok,
                "{mode:?}"
            );
        }
    }

    #[test]
    fn create_only_is_idempotent() {
        let td = tmp();
        let (home_claude, shared) = layout(&td);
        ensure_home_shim_at(&home_claude, &shared, &registry(&[]), ShimMode::CreateOnly).unwrap();
        let again =
            ensure_home_shim_at(&home_claude, &shared, &registry(&[]), ShimMode::CreateOnly)
                .unwrap();
        assert_eq!(again, ShimOutcome::Skipped(HomeShimVerdict::Ok));
        assert!(is_symlink_to(&home_claude.join("projects"), &shared));
    }

    // ── Repair ────────────────────────────────────────────────────────────────

    #[test]
    fn repair_merges_a_real_projects_dir_and_links() {
        let td = tmp();
        let (home_claude, shared) = layout(&td);
        real_home_with_identity(&home_claude);
        let projects = home_claude.join("projects");
        write(&projects.join("-w-only-home").join("a.jsonl"), "a");
        write(&projects.join("-w-both").join("home.jsonl"), "h");
        write(&projects.join("loose.txt"), "loose");
        // The shared dir already has a same-named project dir with its own file.
        write(&shared.join("-w-both").join("shared.jsonl"), "s");

        let out =
            ensure_home_shim_at(&home_claude, &shared, &registry(&[]), ShimMode::Repair).unwrap();
        let ShimOutcome::Merged(report) = out else {
            panic!("expected Merged, got {out:?}");
        };
        assert_eq!(report.moved_dirs, 1, "-w-only-home moved whole");
        assert_eq!(report.moved_files, 2, "loose.txt + home.jsonl moved");
        assert!(report.left_behind.is_empty());

        assert!(is_symlink_to(&projects, &shared), "linked after draining");
        assert_eq!(
            fs::read_to_string(shared.join("loose.txt")).unwrap(),
            "loose"
        );
        assert_eq!(
            fs::read_to_string(shared.join("-w-only-home").join("a.jsonl")).unwrap(),
            "a"
        );
        // The colliding dir was merged file-wise: both files coexist.
        assert_eq!(
            fs::read_to_string(shared.join("-w-both").join("home.jsonl")).unwrap(),
            "h"
        );
        assert_eq!(
            fs::read_to_string(shared.join("-w-both").join("shared.jsonl")).unwrap(),
            "s"
        );
    }

    /// One collision refuses the whole merge. Moving the rest would empty
    /// `~/.claude/projects` of sessions while leaving it an unlinked real dir,
    /// so a plain `claude --resume` would see fewer sessions than before.
    #[test]
    fn repair_refuses_a_colliding_merge_and_moves_nothing() {
        let td = tmp();
        let (home_claude, shared) = layout(&td);
        let projects = home_claude.join("projects");
        write(&projects.join("-w-both").join("same.jsonl"), "home copy");
        write(&projects.join("-w-free").join("new.jsonl"), "new");
        write(&projects.join("loose.txt"), "loose");
        write(&shared.join("-w-both").join("same.jsonl"), "shared copy");
        let before_home = snapshot(&home_claude);
        let before_shared = snapshot(&shared);

        let out =
            ensure_home_shim_at(&home_claude, &shared, &registry(&[]), ShimMode::Repair).unwrap();
        let ShimOutcome::PartiallyMerged(report) = out else {
            panic!("expected PartiallyMerged, got {out:?}");
        };
        assert_eq!(report.moved_dirs, 0, "a blocked merge moves nothing");
        assert_eq!(report.moved_files, 0);
        assert_eq!(
            report.left_behind,
            vec![projects.join("-w-both").join("same.jsonl")],
            "the report names what blocked it"
        );

        assert_eq!(
            snapshot(&home_claude),
            before_home,
            "every session stays reachable through ~/.claude/projects"
        );
        assert_eq!(snapshot(&shared), before_shared, "and none is moved across");
        assert!(
            !fs::symlink_metadata(&projects)
                .unwrap()
                .file_type()
                .is_symlink(),
            "no link is made over a real dir that still holds transcripts"
        );
    }

    /// Resolving the collision by hand and re-running completes the merge — the
    /// refusal is a state the user can get out of.
    #[test]
    fn repair_completes_once_the_collision_is_resolved() {
        let td = tmp();
        let (home_claude, shared) = layout(&td);
        let projects = home_claude.join("projects");
        write(&projects.join("-w-both").join("same.jsonl"), "home copy");
        write(&projects.join("-w-free").join("new.jsonl"), "new");
        write(&shared.join("-w-both").join("same.jsonl"), "shared copy");
        ensure_home_shim_at(&home_claude, &shared, &registry(&[]), ShimMode::Repair).unwrap();

        fs::rename(
            projects.join("-w-both").join("same.jsonl"),
            projects.join("-w-both").join("same.home.jsonl"),
        )
        .unwrap();

        let out =
            ensure_home_shim_at(&home_claude, &shared, &registry(&[]), ShimMode::Repair).unwrap();
        let ShimOutcome::Merged(report) = out else {
            panic!("expected Merged, got {out:?}");
        };
        assert_eq!(report.moved_dirs, 1, "-w-free moved whole");
        assert_eq!(report.moved_files, 1, "the renamed file moved");
        assert!(report.left_behind.is_empty());
        assert!(is_symlink_to(&projects, &shared));
        assert_eq!(
            fs::read_to_string(shared.join("-w-both").join("same.jsonl")).unwrap(),
            "shared copy",
            "the shared copy is never overwritten"
        );
        assert_eq!(
            fs::read_to_string(shared.join("-w-both").join("same.home.jsonl")).unwrap(),
            "home copy"
        );
    }

    /// A blocked merge is decided before any rename runs, so the plan alone says
    /// what is in the way.
    #[test]
    fn plan_merge_finds_every_collision_without_moving_anything() {
        let td = tmp();
        let (home_claude, shared) = layout(&td);
        let projects = home_claude.join("projects");
        write(&projects.join("-w-both").join("same.jsonl"), "home copy");
        write(&projects.join("-w-free").join("new.jsonl"), "new");
        write(&shared.join("-w-both").join("same.jsonl"), "shared copy");
        // A file in the home dir whose name is a directory in the shared one.
        write(&projects.join("-w-odd"), "file here");
        fs::create_dir_all(shared.join("-w-odd")).unwrap();
        let before = snapshot(&home_claude);

        let plan = plan_merge(&projects, &shared).unwrap();
        assert_eq!(
            plan.conflicts,
            vec![
                projects.join("-w-both").join("same.jsonl"),
                projects.join("-w-odd")
            ],
            "both kinds of collision are found"
        );
        assert_eq!(
            plan.moves.len(),
            1,
            "and -w-free is still planned, just never run"
        );
        assert_eq!(snapshot(&home_claude), before, "planning touches nothing");
    }

    #[test]
    fn repair_never_backs_up_a_real_projects_dir() {
        let td = tmp();
        let (home_claude, shared) = layout(&td);
        let projects = home_claude.join("projects");
        write(&projects.join("-w-p").join("a.jsonl"), "a");
        ensure_home_shim_at(&home_claude, &shared, &registry(&[]), ShimMode::Repair).unwrap();
        let siblings = snapshot(&home_claude);
        assert!(
            !siblings.iter().any(|l| l.contains(".bak")),
            "a .bak would hide the history from `claude --resume`: {siblings:?}"
        );
    }

    #[test]
    fn repair_repoints_a_wrong_link() {
        let td = tmp();
        let (home_claude, shared) = layout(&td);
        let elsewhere = td.path().join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        fs::create_dir_all(&home_claude).unwrap();
        symlink(&elsewhere, &home_claude.join("projects"));

        let out =
            ensure_home_shim_at(&home_claude, &shared, &registry(&[]), ShimMode::Repair).unwrap();
        assert_eq!(out, ShimOutcome::Linked(LinkOutcome::Created));
        assert!(is_symlink_to(&home_claude.join("projects"), &shared));
        assert!(elsewhere.is_dir(), "the old target is not removed");
    }

    #[test]
    fn repair_backs_up_a_file_named_projects_and_links() {
        let td = tmp();
        let (home_claude, shared) = layout(&td);
        fs::create_dir_all(&home_claude).unwrap();
        write(&home_claude.join("projects"), "not a dir");

        let out =
            ensure_home_shim_at(&home_claude, &shared, &registry(&[]), ShimMode::Repair).unwrap();
        let ShimOutcome::Linked(LinkOutcome::BackedUp(backup)) = out else {
            panic!("expected Linked(BackedUp), got {out:?}");
        };
        assert_eq!(fs::read_to_string(&backup).unwrap(), "not a dir");
        assert!(is_symlink_to(&home_claude.join("projects"), &shared));
    }

    #[test]
    fn repair_leaves_identity_files_and_settings_untouched() {
        let td = tmp();
        let (home_claude, shared) = layout(&td);
        real_home_with_identity(&home_claude);
        write(
            &home_claude.join("projects").join("-w-p").join("a.jsonl"),
            "a",
        );
        write(&home_claude.join("hooks").join("own.sh"), "#!/bin/sh\n");

        ensure_home_shim_at(&home_claude, &shared, &registry(&[]), ShimMode::Repair).unwrap();

        assert_eq!(
            fs::read_to_string(home_claude.join(".credentials.json")).unwrap(),
            r#"{"token":"x"}"#
        );
        assert_eq!(
            fs::read_to_string(home_claude.join(".claude.json")).unwrap(),
            r#"{"userID":"u"}"#
        );
        assert_eq!(
            fs::read_to_string(home_claude.join("settings.json")).unwrap(),
            r#"{"hooks":{}}"#
        );
        assert_eq!(
            fs::read_to_string(home_claude.join("hooks").join("own.sh")).unwrap(),
            "#!/bin/sh\n",
            "a tool's own hooks dir is not the shim's business"
        );
    }

    #[test]
    fn repair_is_idempotent() {
        let td = tmp();
        let (home_claude, shared) = layout(&td);
        write(
            &home_claude.join("projects").join("-w-p").join("a.jsonl"),
            "a",
        );
        let first =
            ensure_home_shim_at(&home_claude, &shared, &registry(&[]), ShimMode::Repair).unwrap();
        assert!(matches!(first, ShimOutcome::Merged(_)));
        let second =
            ensure_home_shim_at(&home_claude, &shared, &registry(&[]), ShimMode::Repair).unwrap();
        assert_eq!(second, ShimOutcome::Skipped(HomeShimVerdict::Ok));
        assert!(is_symlink_to(&home_claude.join("projects"), &shared));
        assert_eq!(
            fs::read_to_string(shared.join("-w-p").join("a.jsonl")).unwrap(),
            "a"
        );
    }

    #[test]
    fn repair_still_stands_down_for_a_registered_or_symlinked_home() {
        let td = tmp();
        let (home_claude, shared) = layout(&td);
        real_home_with_identity(&home_claude);
        fs::create_dir_all(home_claude.join("projects")).unwrap();
        let before = snapshot(&home_claude);
        let out = ensure_home_shim_at(
            &home_claude,
            &shared,
            &registry(&[("home", &home_claude)]),
            ShimMode::Repair,
        )
        .unwrap();
        assert_eq!(
            out,
            ShimOutcome::Skipped(HomeShimVerdict::Registered {
                name: "home".to_owned()
            })
        );
        assert_eq!(snapshot(&home_claude), before);

        let td2 = tmp();
        let (home_claude, shared) = layout(&td2);
        let target = td2.path().join("home").join(".claude.work");
        fs::create_dir_all(&target).unwrap();
        symlink(&target, &home_claude);
        let out =
            ensure_home_shim_at(&home_claude, &shared, &registry(&[]), ShimMode::Repair).unwrap();
        assert_eq!(
            out,
            ShimOutcome::Skipped(HomeShimVerdict::LinkedElsewhere(target.clone()))
        );
        assert!(snapshot(&target).is_empty());
    }

    /// A merge that dies part-way says which entry it died on and how much had
    /// already moved: `std::fs::rename`'s own error names neither path, and the
    /// entries already in the shared dir are no longer where the user left them.
    #[test]
    fn a_failed_merge_names_the_entry_and_what_already_moved() {
        let from = PathBuf::from("/example/.claude/projects/-w-p");
        let to = PathBuf::from("/example/.claude.shared/projects/-w-p");
        let e = naming(
            io::Error::new(io::ErrorKind::PermissionDenied, "Permission denied"),
            &from,
            &to,
        );
        let e = with_progress(
            e,
            &MergeReport {
                moved_dirs: 2,
                moved_files: 3,
                left_behind: Vec::new(),
            },
        );
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied, "kind survives");
        let msg = e.to_string();
        for part in [
            "-w-p",
            ".claude.shared",
            "Permission denied",
            "2 dir(s) and 3 file(s)",
        ] {
            assert!(msg.contains(part), "{part:?} missing from {msg:?}");
        }
    }

    // ── hot-path reasons ──────────────────────────────────────────────────────

    #[test]
    fn only_repairable_states_warn_on_the_hot_path() {
        assert!(needs_repair_reason(&HomeShimVerdict::ProjectsRealDir).is_some());
        assert!(needs_repair_reason(&HomeShimVerdict::ProjectsNotADir).is_some());
        assert!(
            needs_repair_reason(&HomeShimVerdict::ProjectsWrongLink(PathBuf::from("/x"))).is_some()
        );
        for quiet in [
            HomeShimVerdict::Ok,
            // Healed in both modes, so it never reaches the warning.
            HomeShimVerdict::SharedMissing,
            HomeShimVerdict::LinkedElsewhere(PathBuf::from("/x")),
            HomeShimVerdict::Registered {
                name: "work".to_owned(),
            },
            HomeShimVerdict::Unsupported,
        ] {
            assert!(
                needs_repair_reason(&quiet).is_none(),
                "{quiet:?} is nothing to nag about"
            );
        }
    }

    // ── contract ──────────────────────────────────────────────────────────────

    #[test]
    fn module_never_writes_stdout() {
        // `csm cas --eval` evals this process's stdout in the shell; a stray
        // print here would be executed as shell. Assembled so this test's own
        // source doesn't trip itself.
        let stdout = ["print", "!("].concat();
        let stderr = ["eprint", "!("].concat();
        let stdout_ln = ["print", "ln!("].concat();
        let stderr_ln = ["eprint", "ln!("].concat();
        let src = include_str!("homeguard.rs");
        let hits: Vec<&str> = src
            .lines()
            .filter(|l| {
                let scrubbed = l.replace(&stderr_ln, "").replace(&stderr, "");
                scrubbed.contains(&stdout_ln) || scrubbed.contains(&stdout)
            })
            .collect();
        assert!(hits.is_empty(), "stdout writes in homeguard.rs: {hits:?}");
    }
}
